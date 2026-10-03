//! Popularity sharing: nodes learn which site people pick for a search
//! without anyone learning who searched for what.
//!
//! * A node that shares popularity notes, on its own disk only, which
//!   result its user opened for a query ("searched `us bank`, picked
//!   usbank.com"): a [`PickLog`] that keeps the current week and nothing
//!   older.
//! * A few times a day it turns one pick into a [`Report`] and hands it to
//!   another node under a throwaway identity (see [`crate::node`]), which
//!   passes it to every node on the gossip topic. Each pair is reported at
//!   most once a week, and at most [`REPORTS_PER_DAY`] reports a day.
//! * A report is threshold-encrypted with STAR (Brave's `sta-rs`, the
//!   STARLite variant: its randomness is derived from the pick itself). It
//!   carries a tag, the encrypted pick and one secret share of the key.
//!   Reports of the same pick in the same week have the same tag, and the
//!   key comes back only from [`REPORT_THRESHOLD`] shares of it, so a pick
//!   is readable only once that many reports of it were sent. Below that,
//!   nobody learns anything from it but its tag.
//! * Every node holds the reports of the last two weeks and counts them
//!   itself ([`tally`]), so any node can count or recount and they all get
//!   the same [`PopularityTable`]. Search ranking adds a small bonus from it
//!   ([`PopularityTable::bonus`]).
//!
//! What is shared is cut down before it leaves the node ([`pick_query`]):
//! a query is normalized, and one longer than [`MAX_PICK_WORDS`] words or
//! [`MAX_PICK_CHARS`] characters, or holding an e-mail address or a run of
//! 4 or more digits, is never reported.
//!
//! Known gaps, see `docs/network.md`: with STARLite anyone can guess a pick
//! and check whether its tag was reported (they learn that somebody sent
//! it, not who); the node a report is handed to sees the sender's IP
//! address until reports go through the IP-hiding relay; and nothing yet
//! stops one machine from sending many reports of one pick under many
//! throwaway identities, which anonymous crawl tokens are meant to fix.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use plumb_core::{canonical_domain, normalize_text};
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use sta_rs::{
    derive_ske_key, load_bytes, share_recover, Ciphertext, Message, MessageGenerator,
    SingleMeasurement,
};

use crate::hash::Hash;

/// Reports of one pick it takes before anyone can read the pick.
pub const REPORT_THRESHOLD: u32 = 10;
/// A report epoch is one week: picks are counted per week.
pub const REPORT_EPOCH_SECS: u64 = 7 * 24 * 60 * 60;
/// Most reports one node sends a day.
pub const REPORTS_PER_DAY: u32 = 8;
/// Longest report accepted, as bytes of the STAR message.
pub const MAX_REPORT_BYTES: usize = 1_024;
/// Most reports a node holds for one epoch; later ones are refused.
pub const MAX_REPORTS_PER_EPOCH: usize = 200_000;
/// Longest query reported, in characters after normalizing.
pub const MAX_PICK_CHARS: usize = 64;
/// Most words in a query reported.
pub const MAX_PICK_WORDS: usize = 6;
/// The largest ranking bonus a pick earns, for the site picked most for a
/// query. About a fifth of what naming a site exactly earns, so popularity
/// reorders close calls without overruling the name match.
pub const MAX_POPULARITY_BONUS: f32 = 0.15;

/// The STAR label the measurement is encrypted under (`sta-rs` uses it
/// for every message).
const STAR_ENCRYPT_LABEL: &str = "star_encrypt";
/// Tries at recovering a group's key from other subsets of its shares,
/// when a bad share spoils the first.
const RECOVERY_TRIES: usize = 8;

/// The report epoch of a time.
pub fn report_epoch(unix_secs: u64) -> u64 {
    unix_secs / REPORT_EPOCH_SECS
}

/// The query as it is reported, or `None` when it is not reported at all:
/// [`normalize_text`]ed, at most [`MAX_PICK_WORDS`] words and
/// [`MAX_PICK_CHARS`] characters, with no e-mail address and no run of 4
/// or more digits (phone, account and street numbers, dates).
pub fn pick_query(query: &str) -> Option<String> {
    if query.contains('@') {
        return None;
    }
    let normalized = normalize_text(query);
    if normalized.is_empty()
        || normalized.chars().count() > MAX_PICK_CHARS
        || normalized.split(' ').count() > MAX_PICK_WORDS
    {
        return None;
    }
    let mut digits = 0;
    for c in normalized.chars() {
        digits = if c.is_ascii_digit() { digits + 1 } else { 0 };
        if digits >= 4 {
            return None;
        }
    }
    Some(normalized)
}

/// The bytes a report encrypts.
fn measurement(query: &str, domain: &str) -> Vec<u8> {
    format!("plumb-pick-v1\n{query}\n{domain}").into_bytes()
}

fn parse_measurement(bytes: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.split('\n');
    if lines.next()? != "plumb-pick-v1" {
        return None;
    }
    let query = lines.next()?;
    let domain = lines.next()?;
    if lines.next().is_some() || pick_query(query).as_deref() != Some(query) {
        return None;
    }
    if canonical_domain(domain).as_deref() != Some(domain) {
        return None;
    }
    Some((query.to_string(), domain.to_string()))
}

/// The STAR epoch of a report epoch: picks of different weeks never share
/// a tag or a key.
fn star_epoch(epoch: u64) -> Vec<u8> {
    format!("plumb-popularity-v1/{epoch}").into_bytes()
}

fn generator(epoch: u64, query: &str, domain: &str, threshold: u32) -> MessageGenerator {
    MessageGenerator::new(
        SingleMeasurement::new(&measurement(query, domain)),
        threshold,
        &star_epoch(epoch),
    )
}

/// One threshold-encrypted pick, as nodes pass it around.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The [`report_epoch`] it counts in.
    pub epoch: u64,
    /// The STAR message: encrypted pick, key share and tag.
    #[serde(with = "crate::batch::bytes_hex")]
    pub message: Vec<u8>,
}

/// A report's parts, as [`Report::parts`] reads them.
struct Parts {
    tag: Vec<u8>,
    ciphertext: Vec<u8>,
    share: sta_rs::Share,
}

impl Report {
    /// A report of picking `domain` for `query` in `epoch`. `query` must be
    /// what [`pick_query`] returns and `domain` a registrable domain.
    pub fn new(epoch: u64, query: &str, domain: &str) -> Result<Report> {
        Self::with_threshold(epoch, query, domain, REPORT_THRESHOLD)
    }

    fn with_threshold(epoch: u64, query: &str, domain: &str, threshold: u32) -> Result<Report> {
        if pick_query(query).as_deref() != Some(query) {
            bail!("{query:?} is not a query that is reported");
        }
        if canonical_domain(domain).as_deref() != Some(domain) {
            bail!("{domain:?} is not a registrable domain");
        }
        let generator = generator(epoch, query, domain, threshold);
        let mut randomness = [0u8; 32];
        generator.sample_local_randomness(&mut randomness);
        let message = Message::generate(&generator, &randomness, None)
            .map_err(|err| anyhow::anyhow!("encrypting a report: {err}"))?;
        let report = Report {
            epoch,
            message: message.to_bytes(),
        };
        if report.message.len() > MAX_REPORT_BYTES {
            bail!("the report is too long");
        }
        Ok(report)
    }

    /// The report's id, which nodes tell reports apart by.
    pub fn id(&self) -> Hash {
        Hash::of(&[b"plumb-report-v1", &self.epoch.to_be_bytes(), &self.message])
    }

    /// Checks that the report is of the current or the previous week and
    /// well formed, with a share for [`REPORT_THRESHOLD`] reports.
    pub fn check(&self, now: u64) -> Result<()> {
        let current = report_epoch(now);
        if self.epoch > current || self.epoch + 1 < current {
            bail!("a report of week {} in week {current}", self.epoch);
        }
        if self.message.len() > MAX_REPORT_BYTES {
            bail!("a report of {} bytes", self.message.len());
        }
        if self.parts(REPORT_THRESHOLD).is_none() {
            bail!("a report that is not a STAR message");
        }
        Ok(())
    }

    /// The tag, the encrypted pick and the share, when the message is well
    /// formed and its share is for `threshold` reports. `sta-rs` can panic
    /// on short input, which is caught here: reports come from anyone.
    fn parts(&self, threshold: u32) -> Option<Parts> {
        let message = catch_unwind(|| Message::from_bytes(&self.message)).ok()??;
        // A share starts with the threshold it was made for.
        let share = message.share.to_bytes();
        if share.get(..4) != Some(&threshold.to_le_bytes()[..]) || message.tag.len() != 32 {
            return None;
        }
        Some(Parts {
            tag: message.tag.clone(),
            ciphertext: message.ciphertext.to_bytes(),
            share: message.share.clone(),
        })
    }
}

/// A pick enough reports were sent of.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Popular {
    pub query: String,
    pub domain: String,
    /// Reports of it, from at least [`REPORT_THRESHOLD`] identities.
    pub count: u32,
}

/// A report's tag and ciphertext, which reports of one pick share.
type GroupKey = (Vec<u8>, Vec<u8>);

/// The picks of `epoch` that at least [`REPORT_THRESHOLD`] of `reports`
/// were sent of, with how many. Other epochs' reports are ignored.
pub fn tally(reports: &[Report], epoch: u64) -> Vec<Popular> {
    tally_with(reports, epoch, REPORT_THRESHOLD)
}

fn tally_with(reports: &[Report], epoch: u64, threshold: u32) -> Vec<Popular> {
    // Honest reports of one pick share their tag and, since the key and
    // the encryption come from the pick alone, their ciphertext too.
    let mut groups: HashMap<GroupKey, BTreeMap<Vec<u8>, sta_rs::Share>> = HashMap::new();
    for report in reports.iter().filter(|r| r.epoch == epoch) {
        let Some(parts) = report.parts(threshold) else {
            continue;
        };
        groups
            .entry((parts.tag, parts.ciphertext))
            .or_default()
            .insert(parts.share.to_bytes(), parts.share);
    }
    let mut out = Vec::new();
    for ((tag, ciphertext), shares) in groups {
        if shares.len() < threshold as usize {
            continue;
        }
        let shares: Vec<sta_rs::Share> = shares.into_values().collect();
        if let Some((query, domain)) = recover(&shares, &tag, &ciphertext, epoch, threshold) {
            out.push(Popular {
                query,
                domain,
                count: u32::try_from(shares.len()).unwrap_or(u32::MAX),
            });
        }
    }
    out.sort();
    out
}

/// Reads the pick of a group of shares: recovers the key from `threshold`
/// of them, decrypts, and checks that the pick really has this tag and
/// ciphertext. Tries other subsets when one fails, so one bad share in a
/// group cannot hide a pick.
fn recover(
    shares: &[sta_rs::Share],
    tag: &[u8],
    ciphertext: &[u8],
    epoch: u64,
    threshold: u32,
) -> Option<(String, String)> {
    let k = threshold as usize;
    let mut order: Vec<usize> = (0..shares.len()).collect();
    let mut rng = rand_core::OsRng;
    for attempt in 0..RECOVERY_TRIES {
        if attempt > 0 {
            if shares.len() == k {
                return None;
            }
            for i in (1..order.len()).rev() {
                let j = (rng.next_u64() % (i as u64 + 1)) as usize;
                order.swap(i, j);
            }
        }
        let subset: Vec<sta_rs::Share> = order[..k].iter().map(|&i| shares[i].clone()).collect();
        let opened = catch_unwind(AssertUnwindSafe(|| {
            let commune = share_recover(&subset).ok()?;
            let mut key = [0u8; 16];
            derive_ske_key(&commune.get_message(), &star_epoch(epoch), &mut key);
            let data = Ciphertext::from_bytes(ciphertext).decrypt(&key, STAR_ENCRYPT_LABEL);
            load_bytes(&data).map(<[u8]>::to_vec)
        }));
        let Ok(Some(bytes)) = opened else {
            continue;
        };
        let Some((query, domain)) = parse_measurement(&bytes) else {
            continue;
        };
        // The same pick, reported afresh, must give the same tag and
        // ciphertext; anything else was forged.
        let again = Report::with_threshold(epoch, &query, &domain, threshold).ok()?;
        let again = again.parts(threshold)?;
        if again.tag == tag && again.ciphertext == ciphertext {
            return Some((query, domain));
        }
    }
    None
}

/// What the network's reports say people pick, which every node works out
/// for itself from the reports it holds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PopularityTable {
    /// The report epochs counted.
    pub epochs: Vec<u64>,
    /// Every pick counted, with its reports summed over those epochs.
    pub picks: Vec<Popular>,
    #[serde(skip)]
    by_query: HashMap<String, Vec<(String, u32)>>,
}

impl PopularityTable {
    pub fn new(epochs: Vec<u64>, tallies: impl IntoIterator<Item = Popular>) -> PopularityTable {
        let mut summed: BTreeMap<(String, String), u32> = BTreeMap::new();
        for p in tallies {
            *summed.entry((p.query, p.domain)).or_default() += p.count;
        }
        let picks = summed
            .into_iter()
            .map(|((query, domain), count)| Popular {
                query,
                domain,
                count,
            })
            .collect();
        PopularityTable::from_picks(epochs, picks)
    }

    fn from_picks(epochs: Vec<u64>, picks: Vec<Popular>) -> PopularityTable {
        let mut by_query: HashMap<String, Vec<(String, u32)>> = HashMap::new();
        for p in &picks {
            by_query
                .entry(p.query.clone())
                .or_default()
                .push((p.domain.clone(), p.count));
        }
        PopularityTable {
            epochs,
            picks,
            by_query,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.picks.is_empty()
    }

    /// The ranking bonus of `domain` for `query`: [`MAX_POPULARITY_BONUS`]
    /// for the site picked most for the query, less in proportion for
    /// sites picked less, 0 for sites nobody's picks reached the threshold.
    pub fn bonus(&self, query: &str, domain: &str) -> f32 {
        let Some(key) = pick_query(query) else {
            return 0.0;
        };
        let Some(sites) = self.by_query.get(&key) else {
            return 0.0;
        };
        let most = sites.iter().map(|(_, c)| *c).max().unwrap_or(0);
        sites
            .iter()
            .find(|(d, _)| d == domain)
            .map_or(0.0, |(_, count)| {
                MAX_POPULARITY_BONUS * *count as f32 / most.max(1) as f32
            })
    }

    /// Reads a table saved with [`PopularityTable::save`].
    pub fn load(path: &Path) -> Result<PopularityTable> {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let table: PopularityTable = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not a popularity table", path.display()))?;
        Ok(PopularityTable::from_picks(table.epochs, table.picks))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }
}

/// The picks a node's user made this week, kept on this node only
/// (`DIR/net/picks.json`), and which of them were reported.
#[derive(Debug)]
pub struct PickLog {
    path: PathBuf,
    state: PickState,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PickState {
    /// The report epoch the picks are from.
    epoch: u64,
    /// Times each pick was made, keyed `query\tdomain`.
    picks: BTreeMap<String, u32>,
    /// Picks reported this epoch.
    reported: BTreeSet<String>,
    /// The day `sent_today` counts.
    day: u64,
    sent_today: u32,
}

impl PickLog {
    /// Opens the log at `path`; a missing or unreadable file starts empty.
    pub fn open(path: &Path) -> PickLog {
        let state = fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        PickLog {
            path: path.to_path_buf(),
            state,
        }
    }

    /// Notes that `domain` was picked for `query`. Returns whether the
    /// pick is one that can be reported ([`pick_query`]); others are not
    /// kept at all.
    pub fn record(&mut self, query: &str, domain: &str, now: u64) -> Result<bool> {
        let (Some(query), Some(domain)) = (pick_query(query), canonical_domain(domain)) else {
            return Ok(false);
        };
        self.roll(now);
        *self
            .state
            .picks
            .entry(format!("{query}\t{domain}"))
            .or_default() += 1;
        self.save()?;
        Ok(true)
    }

    /// The next pick to report, if any is due: the most-made pick not yet
    /// reported this week, while fewer than [`REPORTS_PER_DAY`] went out
    /// today. Call [`PickLog::sent`] once its report is out.
    pub fn next_due(&mut self, now: u64) -> Option<(String, String)> {
        self.roll(now);
        if self.state.day == now / 86_400 && self.state.sent_today >= REPORTS_PER_DAY {
            return None;
        }
        let key = self
            .state
            .picks
            .iter()
            .filter(|(key, _)| !self.state.reported.contains(*key))
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
            .map(|(key, _)| key.clone())?;
        let (query, domain) = key.split_once('\t').expect("keys hold a tab");
        Some((query.to_string(), domain.to_string()))
    }

    /// Notes that the report of a pick went out: it is not reported again
    /// this week, and counts toward today's [`REPORTS_PER_DAY`].
    pub fn sent(&mut self, query: &str, domain: &str, now: u64) -> Result<()> {
        self.roll(now);
        let today = now / 86_400;
        if self.state.day != today {
            self.state.day = today;
            self.state.sent_today = 0;
        }
        self.state.reported.insert(format!("{query}\t{domain}"));
        self.state.sent_today += 1;
        self.save()
    }

    /// Picks made this week.
    pub fn len(&self) -> usize {
        self.state.picks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.state.picks.is_empty()
    }

    /// Forgets last week's picks once a new week starts.
    fn roll(&mut self, now: u64) {
        let epoch = report_epoch(now);
        if self.state.epoch != epoch {
            self.state = PickState {
                epoch,
                ..PickState::default()
            };
        }
    }

    fn save(&self) -> Result<()> {
        write_atomic(&self.path, &serde_json::to_vec(&self.state)?)
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("saving {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_790_000_000;

    fn reports(epoch: u64, query: &str, domain: &str, n: usize, threshold: u32) -> Vec<Report> {
        (0..n)
            .map(|_| Report::with_threshold(epoch, query, domain, threshold).unwrap())
            .collect()
    }

    #[test]
    fn only_short_plain_queries_are_reported() {
        assert_eq!(pick_query("U.S. Bank").as_deref(), Some("us bank"));
        assert_eq!(
            pick_query("  Wall Street   Journal ").as_deref(),
            Some("wall street journal")
        );
        assert_eq!(pick_query("route 66").as_deref(), Some("route 66"));
        for private in [
            "",
            "   ",
            "jane.doe@example.com",
            "call 555 1234",
            "5551234",
            "account 12345678",
            "one two three four five six seven",
            &"x".repeat(65),
        ] {
            assert_eq!(pick_query(private), None, "{private:?}");
        }
    }

    #[test]
    fn a_pick_is_readable_only_once_enough_reports_are_sent() {
        let epoch = report_epoch(NOW);
        let mut sent = reports(epoch, "us bank", "usbank.com", 2, 3);
        assert!(tally_with(&sent, epoch, 3).is_empty());
        // Other picks and other weeks do not help.
        sent.extend(reports(epoch, "us bank", "usbank-login.com", 2, 3));
        sent.extend(reports(epoch - 1, "us bank", "usbank.com", 2, 3));
        assert!(tally_with(&sent, epoch, 3).is_empty());

        sent.extend(reports(epoch, "us bank", "usbank.com", 2, 3));
        assert_eq!(
            tally_with(&sent, epoch, 3),
            vec![Popular {
                query: "us bank".into(),
                domain: "usbank.com".into(),
                count: 4
            }]
        );
    }

    #[test]
    fn reports_of_one_pick_look_alike_but_are_not_copies() {
        let epoch = report_epoch(NOW);
        let [a, b] = [0, 1].map(|_| Report::new(epoch, "us bank", "usbank.com").unwrap());
        assert_ne!(a.id(), b.id());
        let (pa, pb) = (
            a.parts(REPORT_THRESHOLD).unwrap(),
            b.parts(REPORT_THRESHOLD).unwrap(),
        );
        assert_eq!(pa.tag, pb.tag);
        let other = Report::new(epoch, "us bank", "usbank-login.com").unwrap();
        assert_ne!(pa.tag, other.parts(REPORT_THRESHOLD).unwrap().tag);
        assert!(a.message.len() <= MAX_REPORT_BYTES, "{}", a.message.len());
        // The pick is not in the clear.
        let text = String::from_utf8_lossy(&a.message);
        assert!(!text.contains("usbank"));
    }

    #[test]
    fn copies_of_one_report_count_once() {
        let epoch = report_epoch(NOW);
        let one = Report::with_threshold(epoch, "us bank", "usbank.com", 3).unwrap();
        let sent = vec![one.clone(), one.clone(), one];
        assert!(tally_with(&sent, epoch, 3).is_empty());
    }

    #[test]
    fn junk_and_bad_shares_do_not_break_counting() {
        let epoch = report_epoch(NOW);
        let mut sent = reports(epoch, "us bank", "usbank.com", 4, 3);
        // A forged share in the group: the tag and ciphertext of a real
        // report, with another pick's share.
        let real = sent[0].parts(3).unwrap();
        let other = Report::with_threshold(epoch, "chase", "chase.com", 3).unwrap();
        let mut forged = Vec::new();
        sta_rs::store_bytes(&real.ciphertext, &mut forged);
        sta_rs::store_bytes(&other.parts(3).unwrap().share.to_bytes(), &mut forged);
        sta_rs::store_bytes(&real.tag, &mut forged);
        sent.insert(
            0,
            Report {
                epoch,
                message: forged,
            },
        );
        for junk in [vec![], vec![1, 2, 3], vec![0xff; 600]] {
            let report = Report {
                epoch,
                message: junk,
            };
            assert!(report.check(NOW).is_err());
            sent.push(report);
        }
        let counted = tally_with(&sent, epoch, 3);
        assert_eq!(counted.len(), 1);
        assert_eq!(counted[0].domain, "usbank.com");
    }

    #[test]
    fn shares_made_for_a_lower_threshold_are_refused() {
        let epoch = report_epoch(NOW);
        let low = Report::with_threshold(epoch, "us bank", "usbank.com", 1).unwrap();
        assert!(low.check(NOW).is_err());
        assert!(tally(&[low], epoch).is_empty());
        assert!(Report::new(epoch, "us bank", "usbank.com")
            .unwrap()
            .check(NOW)
            .is_ok());
    }

    #[test]
    fn only_this_and_last_weeks_reports_are_taken() {
        let epoch = report_epoch(NOW);
        let report = |e| Report::new(e, "us bank", "usbank.com").unwrap();
        assert!(report(epoch).check(NOW).is_ok());
        assert!(report(epoch - 1).check(NOW).is_ok());
        assert!(report(epoch - 2).check(NOW).is_err());
        assert!(report(epoch + 1).check(NOW).is_err());
        assert!(Report::new(epoch, "a@b.com", "usbank.com").is_err());
        assert!(Report::new(epoch, "us bank", "not a domain").is_err());
    }

    #[test]
    fn the_table_gives_the_most_picked_site_the_full_bonus() {
        let table = PopularityTable::new(
            vec![1, 2],
            [
                Popular {
                    query: "us bank".into(),
                    domain: "usbank.com".into(),
                    count: 30,
                },
                Popular {
                    query: "us bank".into(),
                    domain: "usbank.com".into(),
                    count: 10,
                },
                Popular {
                    query: "us bank".into(),
                    domain: "usbankhome.com".into(),
                    count: 20,
                },
            ],
        );
        assert_eq!(table.picks.len(), 2);
        assert_eq!(table.bonus("US Bank", "usbank.com"), MAX_POPULARITY_BONUS);
        assert_eq!(
            table.bonus("us bank", "usbankhome.com"),
            MAX_POPULARITY_BONUS / 2.0
        );
        assert_eq!(table.bonus("us bank", "chase.com"), 0.0);
        assert_eq!(table.bonus("chase", "chase.com"), 0.0);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("popularity.json");
        table.save(&path).unwrap();
        assert_eq!(PopularityTable::load(&path).unwrap(), table);
    }

    #[test]
    fn the_pick_log_reports_each_pick_once_a_week_and_a_few_a_day() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("picks.json");
        let mut log = PickLog::open(&path);
        assert!(!log.record("jane@example.com", "gmail.com", NOW).unwrap());
        assert!(log.record("US Bank", "usbank.com", NOW).unwrap());
        assert!(log.record("us bank", "usbank.com", NOW).unwrap());
        assert!(log.record("chase", "chase.com", NOW).unwrap());
        for i in 0..20 {
            log.record(&format!("site {i}"), "example.com", NOW)
                .unwrap();
        }
        // The most-made pick goes first, and it survives a restart.
        let mut log = PickLog::open(&path);
        let first = log.next_due(NOW).unwrap();
        assert_eq!(first, ("us bank".into(), "usbank.com".into()));
        // Until it is sent, it stays due.
        assert_eq!(log.next_due(NOW).unwrap(), first);
        let mut sent = 0;
        while let Some((query, domain)) = log.next_due(NOW) {
            log.sent(&query, &domain, NOW).unwrap();
            sent += 1;
        }
        assert_eq!(sent, REPORTS_PER_DAY);
        // The next day more go out, but never the same pick twice.
        let tomorrow = NOW + 86_400;
        let mut log = PickLog::open(&path);
        let mut seen = BTreeSet::new();
        while let Some((query, domain)) = log.next_due(tomorrow) {
            assert!(seen.insert(query.clone()), "{query:?}");
            assert_ne!(domain, "usbank.com");
            log.sent(&query, &domain, tomorrow).unwrap();
        }
        assert_eq!(seen.len(), REPORTS_PER_DAY as usize);
        // A new week forgets the old picks.
        let next_week = (report_epoch(NOW) + 1) * REPORT_EPOCH_SECS;
        assert_eq!(log.next_due(next_week), None);
        assert!(log.is_empty());
    }
}
