//! When and how hard a node crawls: workload presets, crawl hours and
//! "pause for an hour" (see [`super::NodeSettings`]).

use serde::{Deserialize, Serialize};

/// How hard the node works in the background, as one choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Workload {
    /// Few homepages at a time and small limits, for a laptop or a slow
    /// connection.
    Light,
    /// The desktop defaults.
    Balanced,
    /// Many homepages at a time and no limits, for a server.
    Full,
    /// The limits as typed, at the crawler's usual pace.
    #[default]
    Custom,
}

impl Workload {
    pub const PRESETS: [Workload; 3] = [Workload::Light, Workload::Balanced, Workload::Full];

    /// Homepages fetched at once; `custom` is what the custom workload
    /// uses, [`Workload::Custom`]'s own when `None`.
    pub fn concurrency_or(self, custom: Option<usize>) -> usize {
        match (self, custom) {
            (Workload::Custom, Some(n)) => n.max(1),
            _ => self.concurrency(),
        }
    }

    /// Homepages fetched at once.
    pub fn concurrency(self) -> usize {
        match self {
            Workload::Light => 4,
            Workload::Balanced | Workload::Custom => 16,
            Workload::Full => 32,
        }
    }

    /// The download limit (MB a day) and storage limit (MB) of a preset,
    /// 0 for none; `None` for [`Workload::Custom`].
    pub fn limits(self) -> Option<(u64, u64)> {
        match self {
            Workload::Light => Some((100, 1_000)),
            Workload::Balanced => Some((500, 2_000)),
            Workload::Full => Some((0, 0)),
            Workload::Custom => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Workload::Light => "light",
            Workload::Balanced => "balanced",
            Workload::Full => "full",
            Workload::Custom => "custom",
        }
    }

    pub fn from_name(name: &str) -> Option<Workload> {
        [
            Workload::Light,
            Workload::Balanced,
            Workload::Full,
            Workload::Custom,
        ]
        .into_iter()
        .find(|w| w.name() == name)
    }
}

/// Hours of the day, on the node's own clock, when crawling may run:
/// from `from:00` up to `to:00`, past midnight when `to` is earlier.
/// Equal hours mean all day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrawlHours {
    pub from: u8,
    pub to: u8,
}

impl CrawlHours {
    /// Whether crawling may run at `hour` (0 to 23).
    pub fn allows(self, hour: u8) -> bool {
        let (from, to) = (self.from % 24, self.to % 24);
        match from.cmp(&to) {
            std::cmp::Ordering::Equal => true,
            std::cmp::Ordering::Less => (from..to).contains(&hour),
            std::cmp::Ordering::Greater => hour >= from || hour < to,
        }
    }

    /// Seconds until crawling may run again, at `hour:minute:second`; 0
    /// when it may run now.
    pub fn wait(self, hour: u8, minute: u8, second: u8) -> u64 {
        if self.allows(hour) {
            return 0;
        }
        let hours = (u64::from(self.from % 24) + 24 - u64::from(hour)) % 24;
        (hours * 3600).saturating_sub(u64::from(minute) * 60 + u64::from(second))
    }

    /// `22:00–07:00`.
    pub fn words(self) -> String {
        format!("{:02}:00\u{2013}{:02}:00", self.from % 24, self.to % 24)
    }
}

/// The node's clock now: hour, minute, second.
pub fn local_time() -> (u8, u8, u8) {
    use chrono::Timelike;
    let now = chrono::Local::now();
    (now.hour() as u8, now.minute() as u8, now.second() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crawl_hours_wrap_past_midnight() {
        let night = CrawlHours { from: 22, to: 7 };
        assert!(night.allows(23) && night.allows(0) && night.allows(6));
        assert!(!night.allows(7) && !night.allows(12) && !night.allows(21));
        assert_eq!(night.wait(23, 0, 0), 0);
        // At 20:30 the window opens in 90 minutes.
        assert_eq!(night.wait(20, 30, 0), 90 * 60);
        // At 07:00 it opens again at 22:00.
        assert_eq!(night.wait(7, 0, 0), 15 * 3600);
        let day = CrawlHours { from: 9, to: 17 };
        assert!(day.allows(9) && !day.allows(17));
        assert_eq!(day.wait(17, 59, 59), 15 * 3600 + 1);
        assert!(CrawlHours { from: 3, to: 3 }.allows(12));
        assert_eq!(night.words(), "22:00\u{2013}07:00");
    }

    #[test]
    fn presets_set_the_limits_and_pace() {
        assert_eq!(Workload::Balanced.limits(), Some((500, 2_000)));
        assert_eq!(Workload::Full.limits(), Some((0, 0)));
        assert_eq!(Workload::Custom.limits(), None);
        assert!(Workload::Light.concurrency() < Workload::Full.concurrency());
        // A concurrency given for the node applies to the custom workload.
        assert_eq!(Workload::Custom.concurrency_or(Some(256)), 256);
        assert_eq!(Workload::Custom.concurrency_or(None), 16);
        assert_eq!(Workload::Light.concurrency_or(Some(256)), 4);
        for w in Workload::PRESETS {
            assert_eq!(Workload::from_name(w.name()), Some(w));
        }
    }
}
