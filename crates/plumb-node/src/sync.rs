//! Profiles shared between a searcher's browsers, computers and nodes.
//!
//! A profile (search history, About you, and what clicks, edits and the
//! tuning page taught; see [`crate::history`]) belongs to one browser on one
//! node. To use it somewhere else, the searcher makes a **link code** on
//! that node's `/link` page and pastes it on the other:
//!
//! - in another browser on the same node, that browser takes the profile;
//! - on another node, that node asks the first one for the profile over
//!   `/plumb/profile/1`, keeps a copy of it, and from then on the two keep
//!   their copies alike (see [`run`]).
//!
//! A link code is the node's id and a secret that works once, for
//! [`CODE_MINUTES`] minutes. What the browser had before is merged in, so
//! two profiles become one.
//!
//! Only linked nodes are ever sent a profile, and only over the network's
//! end to end encrypted connections (relays pass on what they cannot
//! read). Public servers keep no profiles and answer nothing. Either node
//! can stop sharing at any time.
//!
//! Copies are merged three ways, against the last copy both nodes agreed
//! on (a numbered round): counts add what each side added since, an item
//! deleted on one side stays deleted unless the other side changed it, and
//! the searcher's own choices go to the newest. When the two nodes do not
//! agree on the last copy (an answer was lost), they merge without it:
//! nothing is lost, though something deleted may come back.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use plumb_core::now_unix;
use plumb_net::proto::{ByteBuf, ProfileRequest, ProfileResponse};
use plumb_net::{NetHandle, PeerId};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::about::{About, AboutStore, MAX_INTERESTS, MAX_SITES};
use crate::history::{valid_profile, History, HistoryStore, MAX_OPENED, MAX_SEARCHES};
use crate::learn::{Judged, Learned, MAX_BLOCK_COUNTS, MAX_SITE_COUNTS, MAX_VERDICTS};

/// How long a link code works.
pub const CODE_MINUTES: u64 = 10;
/// Link codes waiting to be used, on one node.
const MAX_CODES: usize = 32;
/// Characters of a link code's secret: 40 bits.
const TOKEN_CHARS: usize = 8;
/// Crockford's base 32: no I, L, O or U to misread.
const TOKEN_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Wrong link codes taken in [`FAILED_WINDOW`] seconds before every code
/// is refused until the window ends: guessing one would take far more.
const MAX_FAILED: u32 = 30;
const FAILED_WINDOW: u64 = 10 * 60;
/// Nodes one profile can be shared with.
pub const MAX_LINKS: usize = 16;
/// Seconds between looks at what needs syncing.
const ROUND_SECONDS: u64 = 60;
/// A linked node is asked at least this often, changed or not, and a node
/// that did not answer is asked again no sooner.
const SYNC_EVERY: u64 = 5 * 60;
/// Of two linked nodes, the one with the smaller id asks the other; the
/// other asks only when not asked for this long, so a node that cannot
/// reach the other still gets synced.
const WAIT_FOR_ASKER: u64 = 10 * 60;

/// One sync at a time on a node, so two merges never interleave.
static SYNCING: Mutex<()> = Mutex::new(());
/// Link files are written one at a time.
static LINKS_WRITING: Mutex<()> = Mutex::new(());
/// Link codes waiting to be used.
static CODES: Mutex<Vec<Code>> = Mutex::new(Vec::new());
/// Start of the current window of wrong codes, and how many came in it.
static FAILED: Mutex<(u64, u32)> = Mutex::new((0, 0));

/// What two nodes send each other of a profile.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub history: History,
    pub about: About,
}

impl Snapshot {
    /// The profile `profile` in `dir`, without the results pages lately
    /// shown, which only matter where they were shown.
    pub fn load(dir: &Path, profile: &str) -> Snapshot {
        let mut history = HistoryStore::new(dir).load(profile);
        history.learned.pages.clear();
        Snapshot {
            history,
            about: AboutStore::new(dir).load(profile),
        }
    }

    fn to_bytes(&self) -> Result<ByteBuf> {
        Ok(ByteBuf::from(serde_json::to_vec(self)?))
    }

    fn from_bytes(bytes: &[u8]) -> Result<Snapshot> {
        serde_json::from_slice(bytes).context("reading a profile sent by another node")
    }

    /// Merges `remote` into `dir`'s copy of `profile`, against `base`,
    /// and returns the merged copy (without results pages).
    fn merge_into(
        dir: &Path,
        profile: &str,
        base: Option<&Snapshot>,
        remote: &Snapshot,
    ) -> Result<Snapshot> {
        let mut merged = Snapshot::default();
        HistoryStore::new(dir).update(profile, |history| {
            *history = merge_history(base.map(|b| &b.history), history, &remote.history);
            merged.history = history.clone();
        })?;
        merged.history.learned.pages.clear();
        AboutStore::new(dir).update(profile, |about| {
            *about = merge_about(base.map(|b| &b.about), about, &remote.about);
            merged.about = about.clone();
        })?;
        Ok(merged)
    }
}

/// `local` and `remote` merged against `base`, the copy both had last.
pub fn merge_history(base: Option<&History>, local: &History, remote: &History) -> History {
    let mut searches = merge_items(
        base.map(|b| b.searches.as_slice()),
        &local.searches,
        &remote.searches,
        |s| crate::history::key(&s.query),
        |_, l, r| newest(l, r, |s| s.at).clone(),
    );
    searches.sort_by_key(|s| std::cmp::Reverse(s.at));
    searches.truncate(MAX_SEARCHES);
    let mut opened = merge_items(
        base.map(|b| b.opened.as_slice()),
        &local.opened,
        &remote.opened,
        |o| (crate::history::key(&o.query), o.domain.clone()),
        |b, l, r| {
            let mut o = newest(l, r, |o| o.at).clone();
            o.times = add(b.map(|b| b.times), l.times, r.times);
            o.weighted = add(b.map(|b| b.weighted), l.weighted, r.weighted);
            o
        },
    );
    opened.sort_by_key(|o| std::cmp::Reverse(o.at));
    opened.truncate(MAX_OPENED);
    History {
        searches,
        opened,
        learned: merge_learned(base.map(|b| &b.learned), &local.learned, &remote.learned),
    }
}

fn merge_learned(base: Option<&Learned>, local: &Learned, remote: &Learned) -> Learned {
    let mut blocks = merge_items(
        base.map(|b| b.blocks.as_slice()),
        &local.blocks,
        &remote.blocks,
        |c| (c.block, c.key.clone()),
        |b, l, r| {
            let mut c = l.clone();
            c.shown = add(b.map(|b| b.shown), l.shown, r.shown);
            c.used = add(b.map(|b| b.used), l.used, r.used);
            c.at = l.at.max(r.at);
            c
        },
    );
    blocks.sort_by_key(|c| std::cmp::Reverse(c.at));
    blocks.truncate(MAX_BLOCK_COUNTS);
    let mut sites = merge_items(
        base.map(|b| b.sites.as_slice()),
        &local.sites,
        &remote.sites,
        |c| c.domain.clone(),
        |b, l, r| {
            let mut c = l.clone();
            c.picked = add(b.map(|b| b.picked), l.picked, r.picked);
            c.passed = add(b.map(|b| b.passed), l.passed, r.passed);
            c.at = l.at.max(r.at);
            c
        },
    );
    sites.sort_by_key(|c| std::cmp::Reverse(c.at));
    sites.truncate(MAX_SITE_COUNTS);
    let mut verdicts = merge_items(
        base.map(|b| b.verdicts.as_slice()),
        &local.verdicts,
        &remote.verdicts,
        |v| (v.query.clone(), v.domain.clone()),
        |_, l, r| newest(l, r, |v| v.at).clone(),
    );
    verdicts.sort_by_key(|v| std::cmp::Reverse(v.at));
    verdicts.truncate(MAX_VERDICTS);
    let mut boxes = merge_items(
        base.map(|b| b.boxes.as_slice()),
        &local.boxes,
        &remote.boxes,
        |v| (v.block, v.key.clone()),
        |_, l, r| newest(l, r, |v| v.at).clone(),
    );
    boxes.sort_by_key(|v| std::cmp::Reverse(v.at));
    boxes.truncate(MAX_VERDICTS);
    let tastes = merge_items(
        base.map(|b| b.tastes.as_slice()),
        &local.tastes,
        &remote.tastes,
        |t| t.kind,
        |b, l, r| {
            let mut t = l.clone();
            t.liked = add_f32(b.map(|b| b.liked), l.liked, r.liked);
            t.disliked = add_f32(b.map(|b| b.disliked), l.disliked, r.disliked);
            t.seen = add_f32(b.map(|b| b.seen), l.seen, r.seen);
            t
        },
    );
    // Counted on both sides since the last agreed copy, as tastes are.
    // Tastes counted the old way, without it, are dropped.
    let judged = match (&local.judged, &remote.judged) {
        (Some(l), Some(r)) => {
            let b = base.and_then(|b| b.judged.as_ref());
            let mut pages = l.pages.clone();
            pages.extend(r.pages.iter().filter(|p| !l.pages.contains(p)).cloned());
            pages.truncate(l.pages.len().max(r.pages.len()));
            Some(Judged {
                liked: add_f32(b.map(|b| b.liked), l.liked, r.liked),
                disliked: add_f32(b.map(|b| b.disliked), l.disliked, r.disliked),
                seen: add_f32(b.map(|b| b.seen), l.seen, r.seen),
                pages,
            })
        }
        (one, other) => one.clone().or_else(|| other.clone()),
    };
    let tastes = match (&local.judged, &remote.judged) {
        (Some(_), None) => local.tastes.clone(),
        (None, Some(_)) => remote.tastes.clone(),
        _ => tastes,
    };
    Learned {
        blocks,
        sites,
        // Only where they were shown.
        pages: local.pages.clone(),
        verdicts,
        boxes,
        tastes,
        judged,
    }
}

/// `local` and `remote` merged against `base`.
pub fn merge_about(base: Option<&About>, local: &About, remote: &About) -> About {
    let words = |base: Option<&Vec<String>>, l: &[String], r: &[String], most: usize| {
        let mut merged = merge_items(
            base.map(Vec::as_slice),
            l,
            r,
            |w| w.to_lowercase(),
            |_, l, _| l.clone(),
        );
        merged.truncate(most);
        merged
    };
    let town = match base {
        Some(base) if local.town == base.town => remote.town.clone(),
        None if local.town.is_empty() => remote.town.clone(),
        _ => local.town.clone(),
    };
    // Each kind of result as the town: a side that changed it wins.
    let mut kinds = local.kinds.clone();
    for kind in local.kinds.keys().chain(remote.kinds.keys()) {
        let unchanged = match base {
            Some(base) => local.kinds.get(kind) == base.kinds.get(kind),
            None => !local.kinds.contains_key(kind),
        };
        if unchanged {
            match remote.kinds.get(kind) {
                Some(amount) => kinds.insert(kind.clone(), *amount),
                None => kinds.remove(kind),
            };
        }
    }
    About {
        interests: words(
            base.map(|b| &b.interests),
            &local.interests,
            &remote.interests,
            MAX_INTERESTS,
        ),
        pinned: words(
            base.map(|b| &b.pinned),
            &local.pinned,
            &remote.pinned,
            MAX_SITES,
        ),
        hidden: words(
            base.map(|b| &b.hidden),
            &local.hidden,
            &remote.hidden,
            MAX_SITES,
        ),
        town,
        kinds,
    }
}

/// The items of `local` and `remote`, told apart by `key`, merged against
/// `base`: an item on both sides is `both(base's, local's, remote's)`; one
/// on one side only was added there, or deleted on the other, which sticks
/// unless the side keeping it changed it since `base`. Local items first,
/// in their order.
fn merge_items<T, K>(
    base: Option<&[T]>,
    local: &[T],
    remote: &[T],
    key: impl Fn(&T) -> K,
    both: impl Fn(Option<&T>, &T, &T) -> T,
) -> Vec<T>
where
    T: Clone + PartialEq,
    K: Eq + Hash,
{
    let base: HashMap<K, &T> = base.unwrap_or(&[]).iter().map(|t| (key(t), t)).collect();
    let mut remote_items: HashMap<K, &T> = HashMap::new();
    for item in remote {
        remote_items.entry(key(item)).or_insert(item);
    }
    let changed = |k: &K, item: &T| base.get(k).is_none_or(|b| *b != item);
    let mut seen = HashSet::new();
    let mut merged = Vec::new();
    for item in local {
        let k = key(item);
        if seen.contains(&k) {
            continue;
        }
        match remote_items.get(&k) {
            Some(theirs) => merged.push(both(base.get(&k).copied(), item, theirs)),
            None if changed(&k, item) => merged.push(item.clone()),
            None => {}
        }
        seen.insert(k);
    }
    for item in remote {
        let k = key(item);
        if !seen.contains(&k) && changed(&k, item) {
            merged.push(item.clone());
        }
        seen.insert(k);
    }
    merged
}

/// A count both sides kept: what each added to `base`, or the larger
/// without one (adding then would count twice what both already had).
fn add(base: Option<u32>, local: u32, remote: u32) -> u32 {
    match base {
        Some(base) => {
            let sum = i64::from(local) + i64::from(remote) - i64::from(base);
            u32::try_from(sum.max(0)).unwrap_or(u32::MAX)
        }
        None => local.max(remote),
    }
}

fn add_f32(base: Option<f32>, local: f32, remote: f32) -> f32 {
    match base {
        Some(base) => (local + remote - base).max(0.0),
        None => local.max(remote),
    }
}

/// Whichever of `local` and `remote` is newer; `local` on a tie.
fn newest<'a, T>(local: &'a T, remote: &'a T, at: impl Fn(&T) -> u64) -> &'a T {
    if at(remote) > at(local) {
        remote
    } else {
        local
    }
}

/// The nodes a profile is shared with: `DIR/history/<id>.sync.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Links {
    pub nodes: Vec<Link>,
}

/// A node a profile is shared with.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Link {
    /// Its node id.
    pub peer: String,
    /// When it was linked.
    pub since: u64,
    /// The last round both nodes agreed on, and the copy they had then.
    pub round: Option<u64>,
    pub base: Option<Snapshot>,
    /// When this node last synced with it, and last tried.
    pub synced: u64,
    pub tried: u64,
    /// When it last asked this node to sync.
    pub heard: u64,
    /// Why the last try failed.
    pub problem: Option<String>,
}

impl Link {
    fn new(peer: &PeerId) -> Link {
        Link {
            peer: peer.to_string(),
            since: now_unix(),
            ..Link::default()
        }
    }

    /// When it was last in touch, either way.
    pub fn last_synced(&self) -> u64 {
        self.synced.max(self.heard)
    }
}

fn links_path(dir: &Path, profile: &str) -> Option<PathBuf> {
    valid_profile(profile).then(|| dir.join(format!("{profile}.sync.json")))
}

/// The nodes `profile` is shared with.
pub fn links(dir: &Path, profile: &str) -> Links {
    links_path(dir, profile)
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Changes the links of `profile`; with none left, the file goes.
fn update_links<R>(dir: &Path, profile: &str, change: impl FnOnce(&mut Links) -> R) -> Result<R> {
    let Some(path) = links_path(dir, profile) else {
        bail!("not a profile id: {profile:?}");
    };
    let _writing = LINKS_WRITING.lock().unwrap_or_else(PoisonError::into_inner);
    let mut links = links(dir, profile);
    let result = change(&mut links);
    if links.nodes.is_empty() {
        match fs::remove_file(&path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                return Err(err).with_context(|| format!("deleting {}", path.display()))
            }
            _ => return Ok(result),
        }
    }
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    crate::node::store::write_atomically(&path, &serde_json::to_vec(&links)?)?;
    Ok(result)
}

/// Changes the link of `profile` with `peer`, if there is one.
fn update_link(
    dir: &Path,
    profile: &str,
    peer: &PeerId,
    change: impl FnOnce(&mut Link),
) -> Result<()> {
    let peer = peer.to_string();
    update_links(dir, profile, |links| {
        if let Some(link) = links.nodes.iter_mut().find(|l| l.peer == peer) {
            change(link);
        }
    })
}

/// Adds a link of `profile` with `peer`, unless it has one.
fn add_link(dir: &Path, profile: &str, peer: &PeerId) -> Result<()> {
    let id = peer.to_string();
    update_links(dir, profile, |links| {
        if links.nodes.iter().any(|l| l.peer == id) {
            return Ok(());
        }
        if links.nodes.len() >= MAX_LINKS {
            bail!("this profile is shared with {MAX_LINKS} nodes already");
        }
        links.nodes.push(Link::new(peer));
        Ok(())
    })?
}

/// The profiles in `dir` shared with another node.
pub fn linked_profiles(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let profile = name.strip_suffix(".sync.json")?;
            valid_profile(profile).then(|| profile.to_owned())
        })
        .collect()
}

/// A link code waiting to be used.
struct Code {
    token: String,
    profile: String,
    until: u64,
}

/// A new link code's secret for `profile`, good once for [`CODE_MINUTES`].
pub fn make_code(profile: &str) -> Result<String> {
    if !valid_profile(profile) {
        bail!("not a profile id: {profile:?}");
    }
    let mut bytes = [0u8; TOKEN_CHARS];
    getrandom::fill(&mut bytes).map_err(|err| anyhow::anyhow!("no randomness: {err}"))?;
    let token: String = bytes
        .iter()
        .map(|b| char::from(TOKEN_ALPHABET[usize::from(b % 32)]))
        .collect();
    let now = now_unix();
    let mut codes = CODES.lock().unwrap_or_else(PoisonError::into_inner);
    codes.retain(|c| c.until > now);
    if codes.len() >= MAX_CODES {
        codes.remove(0);
    }
    codes.push(Code {
        token: token.clone(),
        profile: profile.to_owned(),
        until: now + CODE_MINUTES * 60,
    });
    Ok(token)
}

/// The profile of link code secret `token`, which is then used up; `None`
/// for a wrong or old one, or while too many wrong ones came lately.
fn take_code(token: &str) -> Option<String> {
    let now = now_unix();
    let mut failed = FAILED.lock().unwrap_or_else(PoisonError::into_inner);
    if now >= failed.0 + FAILED_WINDOW {
        *failed = (now, 0);
    }
    if failed.1 >= MAX_FAILED {
        return None;
    }
    let token = normal_token(token);
    let mut codes = CODES.lock().unwrap_or_else(PoisonError::into_inner);
    codes.retain(|c| c.until > now);
    match codes.iter().position(|c| c.token == token) {
        Some(i) => Some(codes.remove(i).profile),
        None => {
            failed.1 += 1;
            None
        }
    }
}

/// A token as typed, the way [`make_code`] spells it: capitals, no
/// dashes or spaces, and the letters misread for digits read as them.
fn normal_token(token: &str) -> String {
    token
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

/// A link code as shown: the secret in two halves, then the node's id.
pub fn format_code(token: &str, node: Option<&PeerId>) -> String {
    let (a, b) = token.split_at(token.len() / 2);
    match node {
        Some(node) => format!("{a}-{b}@{node}"),
        None => format!("{a}-{b}"),
    }
}

/// The secret and node of a link code as pasted.
pub fn parse_code(code: &str) -> Result<(String, Option<PeerId>)> {
    let code = code.trim();
    let (token, node) = match code.split_once('@') {
        Some((token, node)) => (
            token,
            Some(
                node.trim()
                    .parse::<PeerId>()
                    .ok()
                    .context("that link code's node id is not right; paste the whole code")?,
            ),
        ),
        None => (code, None),
    };
    let token = normal_token(token);
    if token.len() != TOKEN_CHARS || !token.bytes().all(|b| TOKEN_ALPHABET.contains(&b)) {
        bail!("that is not a link code; it looks like ABCD-1234@12D3KooW…");
    }
    Ok((token, node))
}

/// Answers `peer` about a profile on this node, whose profiles are in
/// `dir` (see [`ProfileRequest`]).
pub fn answer(dir: &Path, peer: PeerId, request: ProfileRequest) -> ProfileResponse {
    match try_answer(dir, peer, request) {
        Ok(response) => response,
        Err(err) => ProfileResponse::Refused(format!("{err:#}")),
    }
}

fn try_answer(dir: &Path, peer: PeerId, request: ProfileRequest) -> Result<ProfileResponse> {
    match request {
        ProfileRequest::Join { token } => {
            let Some(profile) = take_code(&token) else {
                bail!("that link code is wrong, used already or too old; make a new one");
            };
            add_link(dir, &profile, &peer)?;
            info!("shared a search profile with node {peer}");
            Ok(ProfileResponse::Joined {
                state: Snapshot::load(dir, &profile).to_bytes()?,
                profile,
            })
        }
        ProfileRequest::Sync {
            profile,
            round,
            state,
        } => {
            let id = peer.to_string();
            let link = links(dir, &profile)
                .nodes
                .into_iter()
                .find(|l| l.peer == id);
            let Some(link) = link else {
                bail!("this profile is not shared with your node");
            };
            let remote = Snapshot::from_bytes(&state)?;
            let _syncing = SYNCING.lock().unwrap_or_else(PoisonError::into_inner);
            let base = link
                .base
                .as_ref()
                .filter(|_| round.is_some() && round == link.round);
            let merged = Snapshot::merge_into(dir, &profile, base, &remote)?;
            let round = new_round()?;
            let now = now_unix();
            update_link(dir, &profile, &peer, |link| {
                link.round = Some(round);
                link.base = Some(merged.clone());
                link.heard = now;
                link.problem = None;
            })?;
            Ok(ProfileResponse::Synced {
                round,
                state: merged.to_bytes()?,
            })
        }
        ProfileRequest::Leave { profile } => {
            let id = peer.to_string();
            update_links(dir, &profile, |links| links.nodes.retain(|l| l.peer != id))?;
            info!("node {peer} stopped sharing a search profile");
            Ok(ProfileResponse::Left)
        }
    }
}

fn new_round() -> Result<u64> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|err| anyhow::anyhow!("no randomness: {err}"))?;
    Ok(u64::from_le_bytes(bytes))
}

/// Something that answers profile requests: another node over the
/// network, or (in tests) a folder.
pub trait Asker {
    fn ask(
        &self,
        peer: PeerId,
        request: ProfileRequest,
    ) -> impl std::future::Future<Output = Result<ProfileResponse>> + Send;
}

impl Asker for NetHandle {
    async fn ask(&self, peer: PeerId, request: ProfileRequest) -> Result<ProfileResponse> {
        self.ask_profile(peer, request).await
    }
}

/// Syncs `profile` with `peer` now.
pub async fn sync_with(dir: &Path, net: &impl Asker, profile: &str, peer: PeerId) -> Result<()> {
    let id = peer.to_string();
    let Some(link) = links(dir, profile).nodes.into_iter().find(|l| l.peer == id) else {
        bail!("this profile is not shared with {peer}");
    };
    let sent = Snapshot::load(dir, profile);
    let request = ProfileRequest::Sync {
        profile: profile.to_owned(),
        round: link.round,
        state: sent.to_bytes()?,
    };
    let answer = net.ask(peer, request).await;
    let now = now_unix();
    let failed = |problem: String| {
        update_link(dir, profile, &peer, |link| {
            link.tried = now;
            link.problem = Some(problem);
        })
    };
    match answer {
        Ok(ProfileResponse::Synced { round, state }) => {
            let remote = Snapshot::from_bytes(&state)?;
            let _syncing = SYNCING.lock().unwrap_or_else(PoisonError::into_inner);
            // What was sent is the base: the answer has all of it and what
            // the other node changed, this node only what changed since.
            Snapshot::merge_into(dir, profile, Some(&sent), &remote)?;
            update_link(dir, profile, &peer, |link| {
                link.round = Some(round);
                link.base = Some(remote);
                link.synced = now;
                link.tried = now;
                link.problem = None;
            })
        }
        Ok(ProfileResponse::Refused(why)) => {
            failed(why.clone())?;
            bail!("{peer} refused: {why}")
        }
        Ok(other) => {
            failed("it answered something else".into())?;
            bail!("{peer} answered {other:?}")
        }
        Err(err) => {
            failed("it could not be reached".into())?;
            Err(err)
        }
    }
}

/// What pasting a link code did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Joined {
    /// The browser now uses `profile`, a profile of this node.
    Here(String),
    /// The browser now uses `profile`, shared with node `node`.
    There { profile: String, node: PeerId },
}

impl Joined {
    pub fn profile(&self) -> &str {
        match self {
            Joined::Here(profile) | Joined::There { profile, .. } => profile,
        }
    }
}

/// Uses link code `code` for a browser with profile `current`, if it has
/// one, on the node whose profiles are in `dir` and whose id is `me`.
/// What the browser had is merged into the profile it joins.
pub async fn join(
    dir: &Path,
    net: Option<&impl Asker>,
    me: Option<PeerId>,
    code: &str,
    current: Option<&str>,
) -> Result<Joined> {
    let (token, node) = parse_code(code)?;
    let node = node.filter(|node| Some(*node) != me);
    let Some(node) = node else {
        let Some(profile) = take_code(&token) else {
            bail!("that link code is wrong, used already or too old; make a new one");
        };
        if let Some(current) = current.filter(|c| *c != profile) {
            absorb(dir, current, &profile)?;
        }
        return Ok(Joined::Here(profile));
    };
    let Some(net) = net else {
        bail!("this node is not on the Plumb network, so it cannot reach the node of that code");
    };
    let answer = net
        .ask(node, ProfileRequest::Join { token })
        .await
        .context("could not reach the node of that link code")?;
    let (profile, state) = match answer {
        ProfileResponse::Joined { profile, state } => (profile, state),
        ProfileResponse::Refused(why) => bail!("{why}"),
        other => bail!("the node answered {other:?}"),
    };
    if !valid_profile(&profile) {
        bail!("the node sent a profile id that is not one");
    }
    let remote = Snapshot::from_bytes(&state)?;
    {
        let _syncing = SYNCING.lock().unwrap_or_else(PoisonError::into_inner);
        Snapshot::merge_into(dir, &profile, None, &remote)?;
        if let Some(current) = current.filter(|c| *c != profile) {
            absorb(dir, current, &profile)?;
        }
        add_link(dir, &profile, &node)?;
    }
    // Sends the other node what the browser had; later rounds retry.
    if let Err(err) = sync_with(dir, net, &profile, node).await {
        debug!("first sync of a joined profile: {err:#}");
    }
    Ok(Joined::There { profile, node })
}

/// Merges profile `from` into `into`. `from` is deleted afterwards, unless
/// it is itself shared with other nodes, which still sync it.
fn absorb(dir: &Path, from: &str, into: &str) -> Result<()> {
    let old = Snapshot::load(dir, from);
    Snapshot::merge_into(dir, into, None, &old)?;
    if links(dir, from).nodes.is_empty() {
        HistoryStore::new(dir).clear(from)?;
        AboutStore::new(dir).save(from, &About::default())?;
    }
    Ok(())
}

/// Stops sharing `profile` with `peer`, telling it if it can be reached.
pub async fn stop(dir: &Path, net: Option<&impl Asker>, profile: &str, peer: PeerId) -> Result<()> {
    let id = peer.to_string();
    update_links(dir, profile, |links| links.nodes.retain(|l| l.peer != id))?;
    if let Some(net) = net {
        let leave = ProfileRequest::Leave {
            profile: profile.to_owned(),
        };
        if let Err(err) = net.ask(peer, leave).await {
            debug!("telling {peer} a profile is no longer shared: {err:#}");
        }
    }
    Ok(())
}

/// Keeps the profiles in `dir` alike on the nodes they are shared with,
/// for as long as the node runs.
pub async fn run(dir: PathBuf, net: Arc<NetHandle>) {
    let me = net.peer_id().to_string();
    let mut tick = tokio::time::interval(Duration::from_secs(ROUND_SECONDS));
    loop {
        tick.tick().await;
        let now = now_unix();
        for profile in linked_profiles(&dir) {
            let changed = changed_at(&dir, &profile);
            for link in links(&dir, &profile).nodes {
                if !due(&me, &link, changed, now) {
                    continue;
                }
                let Ok(peer) = link.peer.parse::<PeerId>() else {
                    continue;
                };
                if let Err(err) = sync_with(&dir, net.as_ref(), &profile, peer).await {
                    debug!("syncing a profile with {peer}: {err:#}");
                }
            }
        }
    }
}

/// Whether this node (`me`) should ask `link`'s node to sync now, its
/// profile last changed at `changed`.
fn due(me: &str, link: &Link, changed: u64, now: u64) -> bool {
    let asker = me < link.peer.as_str() || now.saturating_sub(link.heard) >= WAIT_FOR_ASKER;
    if !asker {
        return false;
    }
    let since_try = now.saturating_sub(link.tried);
    if link.problem.is_some() {
        return since_try >= SYNC_EVERY;
    }
    since_try >= SYNC_EVERY || changed > link.last_synced()
}

/// When the files of `profile` last changed, in Unix seconds.
fn changed_at(dir: &Path, profile: &str) -> u64 {
    [format!("{profile}.json"), format!("{profile}.about.json")]
        .iter()
        .filter_map(|name| fs::metadata(dir.join(name)).ok()?.modified().ok())
        .filter_map(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
