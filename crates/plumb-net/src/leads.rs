//! Leads: pages AI agents found useful and chose to share with other
//! nodes, so an agent searching another node for the same thing can start
//! there (Liz, 2026-10-08).
//!
//! A lead is the public form of a finding (`plumb-node`'s `findings`),
//! made only when an agent asks to share one on a node that allows it. It
//! carries the page, a short note on why it helped, when it was reported
//! and until when it holds, signed with the reporting node's key. It
//! leaves out the search itself: each of its words is a [`word_key`], a
//! number shared by many words, so a node can match a lead to a search of
//! its own without the lead spelling out what was searched (common words
//! can still be guessed from their keys, as bucket numbers can). The
//! search's text, and the answer the agent worked out, are never in it
//! unless the agent shares the search as well.
//!
//! A lead is a pointer to read, not a fact: it says a node's agent found
//! the page useful, not what the page says now. Nodes keep leads apart
//! from crawls, which other crawlers check (see [`crate::agree`]).
//!
//! Leads travel over the gossip topic `plumb/leads/1`, and a node meeting
//! another asks it for the newest it holds on `/plumb/leads/1`. Every node
//! keeps them in `DIR/net/leads.jsonl`, at most [`MAX_LEADS`], at most
//! [`MAX_LEADS_PER_REPORTER`] from one node and [`MAX_LEADS_PER_DAY`] a day
//! from one node, each until it expires, at most [`MAX_LEAD_SECS`] after it
//! was reported. Which leads a search shows follows the node's search
//! scope (see [`crate::scope`]).

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use libp2p::identity::{Keypair, PublicKey};
use libp2p::PeerId;
use plumb_core::{collapse_whitespace, truncate_chars};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::hash::Hash;
use crate::scope::SearchScope;

/// The file in a node's network directory.
pub const LEADS_FILE: &str = "leads.jsonl";
/// Longest a lead holds: 30 days after it was reported.
pub const MAX_LEAD_SECS: u64 = 30 * DAY;
/// Most leads a node keeps.
pub const MAX_LEADS: usize = 20_000;
/// Most leads kept from one reporting node; its oldest go first.
pub const MAX_LEADS_PER_REPORTER: usize = 500;
/// Most leads taken from one reporting node a day, its own included.
pub const MAX_LEADS_PER_DAY: usize = 50;
/// Most leads one request on `/plumb/leads/1` returns, newest first.
pub const MAX_LISTED_LEADS: usize = 2_000;
/// Most word keys of a lead's search.
pub const MAX_KEYS: usize = 16;
/// Longest note or shared search, in characters.
pub const MAX_NOTE_CHARS: usize = 300;
/// Longest page address.
pub const MAX_URL_CHARS: usize = 2_000;
/// Least share of two searches' word keys they must have in common to
/// match, as findings match (see `plumb-node`'s `findings`).
const MATCH_SHARE: f32 = 0.75;
/// How far in the future a lead may say it was reported: clocks differ.
const CLOCK_SLACK: u64 = 3_600;
const DAY: u64 = 86_400;
const SIGNING_CONTEXT: &[u8] = b"plumb-lead-v1\0";
/// A file rewritten once it holds this many more lines than leads kept.
const COMPACT_SLACK: usize = 1_000;

/// One word of a search as a lead carries it: the first two bytes of a
/// hash of the word, so about 65,000 numbers for every word there is.
pub fn word_key(word: &str) -> u16 {
    let hash = Hash::of(&[b"plumb-lead-word-v1\0", word.as_bytes()]);
    u16::from_be_bytes([hash.0[0], hash.0[1]])
}

/// A search's words as [`word_key`]s: `topic` for the words saying what it
/// is about, `asks` for those saying what is wanted of it ("latest",
/// "install"). Two searches match only when their topics are the same.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeadKeys {
    pub topic: Vec<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asks: Vec<u16>,
}

impl LeadKeys {
    /// The keys of a search's `topic` and `asks` words, each set sorted
    /// and once.
    pub fn new<'a>(
        topic: impl IntoIterator<Item = &'a str>,
        asks: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let keys = |words: &mut dyn Iterator<Item = &'a str>| {
            let mut keys: Vec<u16> = words.map(word_key).collect();
            keys.sort_unstable();
            keys.dedup();
            keys
        };
        LeadKeys {
            topic: keys(&mut topic.into_iter()),
            asks: keys(&mut asks.into_iter()),
        }
    }

    fn all(&self) -> HashSet<u16> {
        self.topic.iter().chain(&self.asks).copied().collect()
    }

    /// How well a lead's keys `self` match a search's `query`: the share of
    /// the larger one's keys they have in common, 0 below [`MATCH_SHARE`] or
    /// when their topics differ.
    pub fn closeness(&self, query: &LeadKeys) -> f32 {
        if self.topic.is_empty() || self.topic != query.topic {
            return 0.0;
        }
        let (mine, theirs) = (self.all(), query.all());
        let shared = mine.intersection(&theirs).count();
        let share = shared as f32 / mine.len().max(theirs.len()).max(1) as f32;
        if share >= MATCH_SHARE {
            share
        } else {
            0.0
        }
    }

    fn canonical(&self) -> bool {
        let sorted = |keys: &[u16]| keys.windows(2).all(|pair| pair[0] < pair[1]);
        sorted(&self.topic) && sorted(&self.asks)
    }
}

/// What a node is asked to share; [`Lead::sign`] makes the lead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadDraft {
    pub keys: LeadKeys,
    /// The search as typed, only when the agent chose to share it too.
    pub query: Option<String>,
    /// The page.
    pub url: String,
    /// Why the page helped.
    pub note: String,
}

/// A page a node's agent found useful for a search, as it is shared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lead {
    /// The reporting node's public key, protobuf-encoded as libp2p does.
    #[serde(with = "crate::batch::bytes_hex")]
    pub reporter: Vec<u8>,
    #[serde(flatten)]
    pub keys: LeadKeys,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    pub url: String,
    pub note: String,
    /// When it was reported, in Unix seconds.
    pub at: u64,
    /// Until when it holds.
    pub expires: u64,
    #[serde(with = "crate::batch::bytes_hex")]
    pub signature: Vec<u8>,
}

impl Lead {
    /// Signs `draft` with `key` as reported at `now`, holding for
    /// [`MAX_LEAD_SECS`]: its text cut to size, or why it cannot be shared.
    pub fn sign(key: &Keypair, draft: LeadDraft, now: u64) -> Result<Lead> {
        let short = |text: &str| truncate_chars(&collapse_whitespace(text), MAX_NOTE_CHARS);
        let mut lead = Lead {
            reporter: key.public().encode_protobuf(),
            keys: draft.keys,
            query: draft.query.as_deref().map(short).filter(|q| !q.is_empty()),
            url: public_url(&draft.url)?,
            note: short(&draft.note),
            at: now,
            expires: now + MAX_LEAD_SECS,
            signature: Vec::new(),
        };
        lead.signature = key
            .sign(&lead.signing_bytes())
            .context("signing the lead")?;
        lead.check(now)?;
        Ok(lead)
    }

    /// The bytes signed: each field after a context string, lengths first,
    /// so a lead's signature is never mistaken for any other.
    fn signing_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(256 + self.url.len() + self.note.len());
        let mut bytes = |part: &[u8]| {
            out.extend_from_slice(&(part.len() as u32).to_be_bytes());
            out.extend_from_slice(part);
        };
        bytes(SIGNING_CONTEXT);
        bytes(&self.reporter);
        let keys = |keys: &[u16]| {
            keys.iter()
                .flat_map(|k| k.to_be_bytes())
                .collect::<Vec<_>>()
        };
        bytes(&keys(&self.keys.topic));
        bytes(&keys(&self.keys.asks));
        match &self.query {
            Some(query) => {
                bytes(&[1]);
                bytes(query.as_bytes());
            }
            None => bytes(&[0]),
        }
        bytes(self.url.as_bytes());
        bytes(self.note.as_bytes());
        bytes(&self.at.to_be_bytes());
        bytes(&self.expires.to_be_bytes());
        out
    }

    /// Identifies the lead: the hash of what was signed.
    pub fn id(&self) -> Hash {
        Hash::of(&[&self.signing_bytes()])
    }

    /// The reporting node, once the signature checks out.
    pub fn verify(&self) -> Result<PeerId> {
        let key = PublicKey::try_decode_protobuf(&self.reporter)
            .context("the reporter key does not decode")?;
        ensure!(
            key.verify(&self.signing_bytes(), &self.signature),
            "the lead's signature does not match"
        );
        Ok(key.to_peer_id())
    }

    /// Whether it no longer holds at `now`.
    pub fn expired(&self, now: u64) -> bool {
        self.expires <= now
    }

    /// Checks the signature, that it holds at `now` and not for longer than
    /// [`MAX_LEAD_SECS`], and that its parts are in their canonical form
    /// and within their limits. Returns the reporting node.
    pub fn check(&self, now: u64) -> Result<PeerId> {
        let reporter = self.verify()?;
        ensure!(
            self.at <= now + CLOCK_SLACK,
            "the lead was reported in the future"
        );
        ensure!(
            self.expires > self.at && self.expires - self.at <= MAX_LEAD_SECS,
            "a lead holds for at most {} days",
            MAX_LEAD_SECS / DAY
        );
        ensure!(!self.expired(now), "the lead expired");
        ensure!(
            !self.keys.topic.is_empty()
                && self.keys.topic.len() + self.keys.asks.len() <= MAX_KEYS
                && self.keys.canonical(),
            "a lead's search has 1 to {MAX_KEYS} words, each once and in order"
        );
        let fits = |text: &str| {
            !text.is_empty()
                && text.chars().count() <= MAX_NOTE_CHARS
                && collapse_whitespace(text) == text
        };
        ensure!(fits(&self.note), "the note is empty, too long or untidy");
        ensure!(
            self.query.as_deref().is_none_or(fits),
            "the search is empty, too long or untidy"
        );
        ensure!(
            public_url(&self.url)? == self.url,
            "the page address is not in its usual form"
        );
        Ok(reporter)
    }

    /// Whether `other` is the same report from the same node: the same page
    /// for the same search.
    fn same(&self, other: &Lead) -> bool {
        self.reporter == other.reporter && self.url == other.url && self.keys == other.keys
    }
}

/// `address` as a lead names it: an http or https address of a public
/// site, without a user name or password.
fn public_url(address: &str) -> Result<String> {
    let address = address.trim();
    ensure!(
        address.chars().count() <= MAX_URL_CHARS,
        "the page address is too long"
    );
    let url = url::Url::parse(address).context("the page must be a web address")?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "the page must be an http or https address"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "the page address must not hold a user name or password"
    );
    match url.host() {
        Some(url::Host::Domain(name)) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            let local = !name.contains('.')
                || [
                    "localhost",
                    "local",
                    "internal",
                    "lan",
                    "home.arpa",
                    "intranet",
                ]
                .iter()
                .any(|end| name == *end || name.ends_with(&format!(".{end}")));
            ensure!(!local, "{name} is a name on a private network");
        }
        Some(url::Host::Ipv4(ip)) => ensure!(public_ip(IpAddr::V4(ip)), "{ip} is not public"),
        Some(url::Host::Ipv6(ip)) => ensure!(public_ip(IpAddr::V6(ip)), "{ip} is not public"),
        None => bail!("the page address has no host"),
    }
    Ok(url.to_string())
}

/// Whether `ip` may be on the internet, roughly: not loopback, private,
/// link-local, shared (CGNAT), unique-local, unspecified or multicast.
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            !(ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_broadcast()
                || (a == 100 && (64..128).contains(&b)))
        }
        IpAddr::V6(ip) => {
            let first = ip.segments()[0];
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80)
        }
    }
}

/// How a node stands to the node that reported a lead, as its search scope
/// sees it (see [`crate::scope`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// A node this node trusts.
    Trusted,
    /// A node one of those trusts.
    FriendOfFriend,
    /// Any other node.
    Other,
}

impl Relation {
    /// Whether a search under `scope` shows leads from such a node.
    pub fn in_scope(self, scope: SearchScope) -> bool {
        match scope {
            SearchScope::Trusted => self == Relation::Trusted,
            SearchScope::FriendsOfFriends => self != Relation::Other,
            SearchScope::Anyone => true,
        }
    }
}

/// One node that reported a [`FoundLead`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeadReporter {
    pub peer_id: String,
    pub relation: Relation,
    /// Why the page helped, as that node's agent put it.
    pub note: String,
    /// The search, when that node's agent shared it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// When it was reported, in Unix seconds.
    pub at: u64,
    pub expires: u64,
}

/// A page other nodes reported for a search like the one made, with every
/// node in scope that reported it, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FoundLead {
    pub url: String,
    pub reporters: Vec<LeadReporter>,
    /// How closely the reported searches match, 0.75 to 1.
    pub closeness: f32,
}

impl FoundLead {
    /// When it was last reported.
    pub fn newest(&self) -> u64 {
        self.reporters.first().map_or(0, |r| r.at)
    }
}

#[derive(Debug, Clone)]
struct Held {
    lead: Lead,
    reporter: PeerId,
    id: Hash,
}

/// What [`LeadStore::insert`] did with a lead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inserted {
    /// Kept: new, or newer than the same report it replaced.
    Taken,
    /// Already held, or older than the same report held.
    Known,
    /// Its node reported [`MAX_LEADS_PER_DAY`] already in the last day.
    TooMany,
}

/// The leads a node keeps.
#[derive(Debug, Default)]
pub struct LeadStore {
    path: Option<PathBuf>,
    held: Vec<Held>,
    ids: HashSet<Hash>,
    /// Lines in the file, kept leads and replaced or dropped ones.
    lines: usize,
    budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
}

impl LeadStore {
    pub fn with_budget(
        mut self,
        budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
    ) -> Self {
        self.budget = budget;
        self
    }

    /// The leads kept in `path` that hold at `now`, none when there is no
    /// file yet. Lines that do not read are left out.
    pub fn open(path: &Path, now: u64) -> Result<LeadStore> {
        let mut store = LeadStore {
            path: Some(path.to_path_buf()),
            ..LeadStore::default()
        };
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(store),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            store.lines += 1;
            let Ok(lead) = serde_json::from_str::<Lead>(line) else {
                continue;
            };
            // Checked when taken in; only time has passed since.
            let Ok(reporter) = PublicKey::try_decode_protobuf(&lead.reporter) else {
                continue;
            };
            if !lead.expired(now) {
                store.keep(lead, reporter.to_peer_id(), now, false);
            }
        }
        if store.lines > store.held.len() {
            store.compact()?;
        }
        Ok(store)
    }

    /// Leads held.
    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Leads held from `reporter`.
    pub fn from(&self, reporter: &PeerId) -> usize {
        self.held.iter().filter(|h| h.reporter == *reporter).count()
    }

    /// Keeps `lead` from `reporter`, which passed [`Lead::check`], and
    /// writes it to the file.
    pub fn insert(&mut self, lead: Lead, reporter: PeerId, now: u64) -> Result<Inserted> {
        let inserted = self.keep(lead, reporter, now, true);
        if inserted == Inserted::Taken {
            self.append()?;
            if self.lines > 2 * self.held.len() + COMPACT_SLACK {
                self.compact()?;
            }
        }
        Ok(inserted)
    }

    /// [`LeadStore::insert`] in memory; `limit_rate` holds each node to
    /// [`MAX_LEADS_PER_DAY`].
    fn keep(&mut self, lead: Lead, reporter: PeerId, now: u64, limit_rate: bool) -> Inserted {
        let id = lead.id();
        if self.ids.contains(&id) {
            return Inserted::Known;
        }
        if let Some(same) = self.held.iter().position(|h| h.lead.same(&lead)) {
            if self.held[same].lead.at >= lead.at {
                return Inserted::Known;
            }
        }
        if limit_rate {
            let today = self
                .held
                .iter()
                .filter(|h| h.reporter == reporter && h.lead.at + DAY > now)
                .count();
            if today >= MAX_LEADS_PER_DAY {
                return Inserted::TooMany;
            }
        }
        if let Some(same) = self.held.iter().position(|h| h.lead.same(&lead)) {
            self.remove(same);
        }
        if self.from(&reporter) >= MAX_LEADS_PER_REPORTER {
            self.remove_oldest_of(reporter);
        }
        if self.held.len() >= MAX_LEADS {
            // The node with the most leads gives one up, so a node that
            // floods the network crowds out only itself.
            let mut counts: HashMap<PeerId, usize> = HashMap::new();
            for h in &self.held {
                *counts.entry(h.reporter).or_default() += 1;
            }
            if let Some((most, _)) = counts.into_iter().max_by_key(|(_, n)| *n) {
                self.remove_oldest_of(most);
            }
        }
        self.ids.insert(id);
        self.held.push(Held { lead, reporter, id });
        Inserted::Taken
    }

    fn remove(&mut self, index: usize) {
        let held = self.held.remove(index);
        self.ids.remove(&held.id);
    }

    fn remove_oldest_of(&mut self, reporter: PeerId) {
        let oldest = self
            .held
            .iter()
            .enumerate()
            .filter(|(_, h)| h.reporter == reporter)
            .min_by_key(|(_, h)| h.lead.at)
            .map(|(i, _)| i);
        if let Some(i) = oldest {
            self.remove(i);
        }
    }

    /// Drops the leads that expired by `now`.
    pub fn prune(&mut self, now: u64) -> Result<()> {
        let before = self.held.len();
        self.held.retain(|h| !h.lead.expired(now));
        if self.held.len() < before {
            self.ids = self.held.iter().map(|h| h.id).collect();
            self.compact()?;
        }
        Ok(())
    }

    /// Up to `max` leads held, newest first, for a node catching up.
    pub fn list(&self, max: usize) -> Vec<Lead> {
        let mut held: Vec<&Held> = self.held.iter().collect();
        held.sort_by_key(|h| std::cmp::Reverse(h.lead.at));
        held.into_iter().take(max).map(|h| h.lead.clone()).collect()
    }

    /// The pages reported for a search with `keys`, by nodes other than
    /// `me` that `relation` places in `scope`, closest, then reported by the
    /// closest nodes, by the most nodes and most recently first; at most
    /// `limit`.
    pub fn matching(
        &self,
        keys: &LeadKeys,
        me: &PeerId,
        scope: SearchScope,
        relation: impl Fn(&PeerId) -> Relation,
        now: u64,
        limit: usize,
    ) -> Vec<FoundLead> {
        let mut by_url: HashMap<&str, (FoundLead, Relation)> = HashMap::new();
        for held in &self.held {
            if held.reporter == *me || held.lead.expired(now) {
                continue;
            }
            let close = held.lead.keys.closeness(keys);
            let stands = relation(&held.reporter);
            if close == 0.0 || !stands.in_scope(scope) {
                continue;
            }
            let lead = &held.lead;
            let (found, best) = by_url.entry(&lead.url).or_insert_with(|| {
                (
                    FoundLead {
                        url: lead.url.clone(),
                        reporters: Vec::new(),
                        closeness: 0.0,
                    },
                    stands,
                )
            });
            found.closeness = found.closeness.max(close);
            *best = (*best).min(stands);
            found.reporters.push(LeadReporter {
                peer_id: held.reporter.to_string(),
                relation: stands,
                note: lead.note.clone(),
                query: lead.query.clone(),
                at: lead.at,
                expires: lead.expires,
            });
        }
        let mut found: Vec<(FoundLead, Relation)> = by_url.into_values().collect();
        for (lead, _) in &mut found {
            lead.reporters.sort_by_key(|r| std::cmp::Reverse(r.at));
        }
        found.sort_by(|(a, ra), (b, rb)| {
            b.closeness
                .total_cmp(&a.closeness)
                .then(ra.cmp(rb))
                .then(b.reporters.len().cmp(&a.reporters.len()))
                .then(b.newest().cmp(&a.newest()))
                .then(a.url.cmp(&b.url))
        });
        found
            .into_iter()
            .take(limit)
            .map(|(lead, _)| lead)
            .collect()
    }

    /// Adds the newest lead to the file.
    fn append(&mut self) -> Result<()> {
        let (Some(path), Some(held)) = (&self.path, self.held.last()) else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            plumb_core::storage::create_directory(parent, self.budget.as_ref())
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut line = serde_json::to_vec(&held.lead).context("encoding a lead")?;
        line.push(b'\n');
        if let Some(budget) = &self.budget {
            let mut file =
                plumb_core::storage::BudgetFile::open_write(path, false, Some(budget.clone()))?;
            file.seek(std::io::SeekFrom::End(0))?;
            file.write_all(&line)?;
        } else {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut file| file.write_all(&line))
                .with_context(|| format!("writing {}", path.display()))?;
        }
        self.lines += 1;
        Ok(())
    }

    /// Writes the file again with the leads kept and nothing else.
    fn compact(&mut self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut text = Vec::new();
        for held in &self.held {
            serde_json::to_writer(&mut text, &held.lead).context("encoding a lead")?;
            text.push(b'\n');
        }
        let part = path.with_extension("jsonl.part");
        if self.budget.is_some() {
            plumb_core::storage::write_atomic(path, &part, &text, self.budget.as_ref(), None)?;
        } else {
            fs::write(&part, text)
                .and_then(|()| fs::rename(&part, path))
                .with_context(|| format!("writing {}", path.display()))
                .inspect_err(|err| warn!("{err:#}"))?;
        }
        self.lines = self.held.len();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_790_000_000;

    fn draft(url: &str) -> LeadDraft {
        LeadDraft {
            keys: LeadKeys::new(["tokio"], ["latest", "version"]),
            query: None,
            url: url.to_string(),
            note: "the changelog lists  every release".to_string(),
        }
    }

    #[test]
    fn a_lead_is_signed_checked_and_matched_by_its_keys() {
        let key = Keypair::generate_ed25519();
        let lead = Lead::sign(&key, draft("https://crates.io/crates/tokio"), NOW).unwrap();
        assert_eq!(lead.check(NOW).unwrap(), key.public().to_peer_id());
        assert_eq!(lead.note, "the changelog lists every release");
        assert_eq!(lead.expires, NOW + MAX_LEAD_SECS);
        // The search's words are not in it.
        let json = serde_json::to_string(&lead).unwrap();
        assert!(!json.contains("\"query\""), "{json}");
        assert!(!json.contains("latest"), "{json}");
        let back: Lead = serde_json::from_str(&json).unwrap();
        assert_eq!(back, lead);

        let same = LeadKeys::new(["tokio"], ["version", "latest"]);
        assert!(lead.keys.closeness(&same) >= 0.99);
        let fewer = LeadKeys::new(["tokio"], ["latest"]);
        assert!(
            lead.keys.closeness(&fewer) == 0.0,
            "2 of 3 words is too few"
        );
        let other = LeadKeys::new(["tokio", "select"], []);
        assert_eq!(lead.keys.closeness(&other), 0.0);

        // Changed, forged or expired, it no longer checks out.
        let mut changed = lead.clone();
        changed.url = "https://evil.example/".into();
        assert!(changed.check(NOW).is_err());
        let mut longer = lead.clone();
        longer.expires += 1;
        assert!(longer.check(NOW).is_err());
        assert!(lead.check(NOW + MAX_LEAD_SECS).is_err());
        let mut theirs = lead.clone();
        theirs.reporter = Keypair::generate_ed25519().public().encode_protobuf();
        assert!(theirs.check(NOW).is_err());
    }

    #[test]
    fn leads_name_public_pages_only() {
        let key = Keypair::generate_ed25519();
        let sign = |url: &str| Lead::sign(&key, draft(url), NOW);
        assert!(sign("https://docs.rs/tokio/latest/tokio/").is_ok());
        assert!(sign("http://93.184.216.34/page").is_ok());
        for url in [
            "http://127.0.0.1:8080/",
            "http://192.168.1.10/wiki",
            "http://[::1]/",
            "http://localhost/",
            "http://nas.local/",
            "http://wiki/",
            "http://jira.corp.internal/",
            "https://user:secret@example.com/",
            "ftp://example.com/",
            "javascript:alert(1)",
        ] {
            assert!(sign(url).is_err(), "{url}");
        }
        let mut bad = draft("https://example.com/");
        bad.keys.topic.clear();
        assert!(Lead::sign(&key, bad, NOW).is_err());
    }

    #[test]
    fn the_store_keeps_leads_within_bounds_and_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEADS_FILE);
        let (me, friend, stranger) = (
            Keypair::generate_ed25519(),
            Keypair::generate_ed25519(),
            Keypair::generate_ed25519(),
        );
        let peer = |key: &Keypair| key.public().to_peer_id();
        let mut store = LeadStore::open(&path, NOW).unwrap();
        let take = |store: &mut LeadStore, key: &Keypair, url: &str, at: u64| {
            let lead = Lead::sign(key, draft(url), at).unwrap();
            store.insert(lead, peer(key), at).unwrap()
        };
        let url = "https://tokio.rs/blog";
        assert_eq!(take(&mut store, &friend, url, NOW), Inserted::Taken);
        // Sent again, it is held once; reported again later, it replaces.
        let again = store.list(1)[0].clone();
        assert_eq!(
            store.insert(again, peer(&friend), NOW).unwrap(),
            Inserted::Known
        );
        assert_eq!(take(&mut store, &friend, url, NOW + 5), Inserted::Taken);
        assert_eq!(store.len(), 1);
        take(&mut store, &stranger, url, NOW + 1);
        take(&mut store, &me, "https://docs.rs/tokio", NOW + 2);

        let relation = |p: &PeerId| {
            if *p == peer(&friend) {
                Relation::Trusted
            } else {
                Relation::Other
            }
        };
        let keys = LeadKeys::new(["tokio"], ["latest", "version"]);
        let found = |store: &LeadStore, scope| {
            store.matching(&keys, &peer(&me), scope, relation, NOW + 10, 5)
        };
        // Its own lead is left out; both others name one page.
        let all = found(&store, SearchScope::Anyone);
        assert_eq!(all.len(), 1, "{all:?}");
        assert_eq!(all[0].url, url);
        assert_eq!(all[0].reporters.len(), 2);
        assert_eq!(all[0].reporters[0].at, NOW + 5);
        assert_eq!(all[0].reporters[0].relation, Relation::Trusted);
        let trusted = found(&store, SearchScope::Trusted);
        assert_eq!(trusted[0].reporters.len(), 1);
        let none = store.matching(
            &LeadKeys::new(["axum"], []),
            &peer(&me),
            SearchScope::Anyone,
            relation,
            NOW,
            5,
        );
        assert!(none.is_empty());

        // Read back as it was, without the lead that was replaced.
        let again = LeadStore::open(&path, NOW + 10).unwrap();
        assert_eq!(again.len(), 3);
        assert_eq!(again.lines, 3);
        assert_eq!(found(&again, SearchScope::Anyone), all);
        // Expired, they go, from memory and from the file.
        let later = NOW + MAX_LEAD_SECS + 10;
        let mut store = again;
        store.prune(later).unwrap();
        assert!(store.is_empty());
        assert!(LeadStore::open(&path, NOW).unwrap().is_empty());

        // One node may report only so many a day.
        let mut store = LeadStore::default();
        for i in 0..MAX_LEADS_PER_DAY {
            let url = format!("https://example.com/{i}");
            assert_eq!(take(&mut store, &stranger, &url, NOW), Inserted::Taken);
        }
        assert_eq!(
            take(&mut store, &stranger, "https://example.com/more", NOW),
            Inserted::TooMany
        );
        assert_eq!(
            take(&mut store, &stranger, "https://example.com/more", NOW + DAY),
            Inserted::Taken
        );
    }

    #[test]
    fn a_node_with_too_many_leads_gives_up_its_oldest() {
        let mut store = LeadStore::default();
        let key = Keypair::generate_ed25519();
        let reporter = key.public().to_peer_id();
        for i in 0..=MAX_LEADS_PER_REPORTER {
            let day = (i / MAX_LEADS_PER_DAY) as u64;
            let at = NOW + day * DAY + i as u64;
            let lead = Lead::sign(&key, draft(&format!("https://example.com/{i}")), at).unwrap();
            assert_eq!(store.insert(lead, reporter, at).unwrap(), Inserted::Taken);
        }
        assert_eq!(store.len(), MAX_LEADS_PER_REPORTER);
        assert!(!store
            .list(MAX_LISTED_LEADS)
            .iter()
            .any(|l| l.url == "https://example.com/0"));
    }
}
