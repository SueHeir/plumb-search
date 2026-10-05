//! How a node shares out the bucket requests it answers for free, so heavy
//! traffic from a few cannot crowd out everyone else.
//!
//! A request is answered for free when all of these hold:
//!
//! - fewer than `NetConfig::max_answering` requests are being answered;
//! - the address it came from, or the relay that passed it on sealed, sent
//!   fewer than [`FREE_PER_MINUTE`] a minute lately (with a burst of
//!   [`FREE_BURST`]). Requests come under throwaway identities, so the
//!   address is all there is to go by; a relay this node trusts is not
//!   held to a rate;
//! - fewer than the owner's daily limit (`NetConfig::answer_per_day`, none
//!   unless set) were answered for free today, by UTC days.
//!
//! Past those, a node says it is busy, and only requests that spend a token
//! of its own get in (see [`crate::credits`]). Nodes that crawl or answer
//! for it, the nodes it trusts included, earn those tokens. Searches from
//! the node's own front end are never limited.

use std::collections::HashMap;

use std::net::IpAddr;

use libp2p::PeerId;

/// Free requests one node may send straight to us a minute, on average.
pub const FREE_PER_MINUTE: u32 = 120;
/// Free requests one node may send in a burst before
/// [`FREE_PER_MINUTE`] holds it back.
pub const FREE_BURST: u32 = 240;
/// Nodes whose rate is remembered; idle ones are forgotten past this.
const MAX_TRACKED: usize = 10_000;

const DAY_SECS: u64 = 86_400;

/// Where free requests are counted from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// The address a request came straight from.
    Ip(IpAddr),
    /// The relay that passed on a sealed request.
    Peer(PeerId),
}

/// One node's free requests: a token bucket, refilled over time.
#[derive(Debug, Clone, Copy)]
struct Rate {
    /// Requests it may still send now.
    left: f64,
    /// When `left` was last brought up to date, in milliseconds.
    at_ms: u64,
}

/// What this node answered for free, and for whom.
#[derive(Debug)]
pub struct Allowance {
    per_day: Option<u64>,
    rates: HashMap<Source, Rate>,
    /// The UTC day `today` counts.
    day: u64,
    /// Free answers today.
    today: u64,
    /// Requests turned away as busy since start.
    turned_away: u64,
}

impl Allowance {
    /// At most `per_day` free answers a day; `None` for no daily limit.
    pub fn new(per_day: Option<u64>) -> Allowance {
        Allowance {
            per_day,
            rates: HashMap::new(),
            day: 0,
            today: 0,
            turned_away: 0,
        }
    }

    /// Takes one free answer for a request from `from`, or from no source
    /// held to a rate (`None`), at `now_ms` (Unix milliseconds). False when
    /// the source used up its rate or the day's limit is reached: nothing
    /// is taken then.
    pub fn take(&mut self, from: Option<&Source>, now_ms: u64) -> bool {
        let day = now_ms / 1000 / DAY_SECS;
        if day != self.day {
            self.day = day;
            self.today = 0;
        }
        if self.per_day.is_some_and(|cap| self.today >= cap) {
            return false;
        }
        if let Some(peer) = from {
            if self.rates.len() >= MAX_TRACKED && !self.rates.contains_key(peer) {
                self.forget_idle(now_ms);
            }
            let rate = self.rates.entry(*peer).or_insert(Rate {
                left: f64::from(FREE_BURST),
                at_ms: now_ms,
            });
            let minutes = now_ms.saturating_sub(rate.at_ms) as f64 / 60_000.0;
            rate.left =
                (rate.left + minutes * f64::from(FREE_PER_MINUTE)).min(f64::from(FREE_BURST));
            rate.at_ms = now_ms;
            if rate.left < 1.0 {
                return false;
            }
            rate.left -= 1.0;
        }
        self.today += 1;
        true
    }

    /// Notes a request turned away as busy.
    pub fn turn_away(&mut self) {
        self.turned_away += 1;
    }

    /// Free answers today, as of the last request.
    pub fn today(&self) -> u64 {
        self.today
    }

    pub fn per_day(&self) -> Option<u64> {
        self.per_day
    }

    pub fn turned_away(&self) -> u64 {
        self.turned_away
    }

    /// Drops the nodes whose bucket has filled up again: they would start
    /// over with a full one anyway. If none has, drops them all.
    fn forget_idle(&mut self, now_ms: u64) {
        let full_after_ms = u64::from(FREE_BURST) * 60_000 / u64::from(FREE_PER_MINUTE);
        self.rates
            .retain(|_, rate| now_ms.saturating_sub(rate.at_ms) < full_after_ms);
        if self.rates.len() >= MAX_TRACKED {
            self.rates.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_790_000_000_000;

    #[test]
    fn one_node_is_held_to_its_rate_and_others_are_not() {
        let (heavy, other) = (
            Source::Peer(PeerId::random()),
            Source::Ip([10, 0, 0, 1].into()),
        );
        let mut allowance = Allowance::new(None);
        for _ in 0..FREE_BURST {
            assert!(allowance.take(Some(&heavy), NOW));
        }
        assert!(!allowance.take(Some(&heavy), NOW), "burst used up");
        assert!(allowance.take(Some(&other), NOW), "others still get in");
        assert!(allowance.take(None, NOW), "and sealed requests");
        // Half a second later one more has come back.
        assert!(allowance.take(Some(&heavy), NOW + 500));
        assert!(!allowance.take(Some(&heavy), NOW + 500));
        // A minute's wait refills a minute's worth.
        for _ in 0..FREE_PER_MINUTE {
            assert!(allowance.take(Some(&heavy), NOW + 60_500));
        }
        assert!(!allowance.take(Some(&heavy), NOW + 60_500));
    }

    #[test]
    fn the_daily_limit_holds_for_everyone_until_the_next_day() {
        let mut allowance = Allowance::new(Some(3));
        assert!(allowance.take(Some(&Source::Peer(PeerId::random())), NOW));
        assert!(allowance.take(None, NOW));
        assert!(allowance.take(Some(&Source::Peer(PeerId::random())), NOW));
        assert!(!allowance.take(Some(&Source::Peer(PeerId::random())), NOW));
        assert!(!allowance.take(None, NOW));
        assert_eq!(allowance.today(), 3);
        let tomorrow = (NOW / 1000 / DAY_SECS + 1) * DAY_SECS * 1000;
        assert!(allowance.take(None, tomorrow));
        assert_eq!(allowance.today(), 1);
    }

    #[test]
    fn many_nodes_do_not_grow_the_table_without_end() {
        let mut allowance = Allowance::new(None);
        for i in 0..(MAX_TRACKED as u64 + 10) {
            allowance.take(Some(&Source::Peer(PeerId::random())), NOW + i);
        }
        assert!(allowance.rates.len() <= MAX_TRACKED);
    }
}
