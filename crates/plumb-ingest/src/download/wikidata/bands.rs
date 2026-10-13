//! Bands of sitelink counts, asked for one query each, and the bands an
//! earlier download saved so that a later one can go on from them.

use std::fmt;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

#[cfg(doc)]
use super::download_wikidata_official_sites_paced;
use super::sparql::{OfficialSitesTsv, WikidataRow};
use crate::download::part_path;

/// A range of sitelink counts that one Wikidata query asks for: `min` to
/// `max`, both included, or `min` and up when `max` is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SitelinkBand {
    pub min: u32,
    pub max: Option<u32>,
}

/// Where the bands of [`wikidata_sitelink_bands`] start from 25 sitelinks up.
const WIKIDATA_BAND_STARTS: [u32; 7] = [25, 30, 36, 45, 60, 85, 130];

/// An open band, `min` sitelinks or more, is not split once `min` is this
/// high: no Wikidata item has nearly that many sitelinks.
const OPEN_BAND_SPLIT_LIMIT: u32 = 1_000;

/// The bands of sitelink counts that [`download_wikidata_official_sites_paced`]
/// asks for, lowest first. Together they cover every count from
/// `min_sitelinks` up, each once: below 25 each count is a band of its own,
/// then come 25-29, 30-35, 36-44, 45-59, 60-84, 85-129 and 130 or more.
///
/// From 25 up there were 131,386 rows in October 2026, too many for one
/// query to list within Wikidata's 60-second limit. These bands came in
/// whole then, holding 27,397, 28,773, 38,990, 24,556, 6,942, 3,314 and
/// 1,414 rows, in 21 to 47 seconds each.
pub fn wikidata_sitelink_bands(min_sitelinks: u32) -> Vec<SitelinkBand> {
    let starts: Vec<u32> = std::iter::once(min_sitelinks)
        .chain(min_sitelinks.saturating_add(1)..WIKIDATA_BAND_STARTS[0])
        .chain(
            WIKIDATA_BAND_STARTS
                .into_iter()
                .filter(|&start| start > min_sitelinks),
        )
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(i, &min)| SitelinkBand {
            min,
            max: starts.get(i + 1).map(|next| next - 1),
        })
        .collect()
}

impl SitelinkBand {
    /// The SPARQL query for the items in the band, as
    /// [`wikidata_sparql_query`] is for those from a minimum up.
    pub fn sparql_query(&self) -> String {
        if self.max == Some(self.min) {
            // One count: naming it lets Wikidata start from its index of
            // sitelink counts instead of every item with a website (on
            // 2026-10-09 the filter for exactly 20 ran past the limit,
            // while this answered in 50 seconds).
            return format!(
                "SELECT ?item ?itemLabel ?website WHERE {{ ?item wikibase:sitelinks {} . ?item wdt:P856 ?website . SERVICE wikibase:label {{ bd:serviceParam wikibase:language \"en,mul\". }} }}",
                self.min
            );
        }
        let filter = match self.max {
            Some(max) => format!("?s >= {} && ?s < {}", self.min, u64::from(max) + 1),
            None => format!("?s >= {}", self.min),
        };
        format!(
            "SELECT ?item ?itemLabel ?website WHERE {{ ?item wdt:P856 ?website ; wikibase:sitelinks ?s . FILTER({filter}) SERVICE wikibase:label {{ bd:serviceParam wikibase:language \"en,mul\". }} }}"
        )
    }

    /// The band in two, lower half first, to ask for once Wikidata was too
    /// busy for the band's query; `None` when it cannot be split. A band
    /// with an upper end is cut in the middle, down to single counts. One
    /// without, `min` and up, is cut into `min` to `2 * min - 1` and
    /// `2 * min` and up, since items with more sitelinks are much rarer,
    /// unless `min` is 1,000 or more.
    pub fn halves(&self) -> Option<(SitelinkBand, SitelinkBand)> {
        let band = |min, max| SitelinkBand { min, max };
        match self.max {
            Some(max) if max > self.min => {
                let mid = self.min + (max - self.min) / 2;
                Some((band(self.min, Some(mid)), band(mid + 1, Some(max))))
            }
            Some(_) => None,
            None if self.min >= OPEN_BAND_SPLIT_LIMIT => None,
            None => {
                let upper = self.min.saturating_mul(2).max(self.min + 1);
                Some((band(self.min, Some(upper - 1)), band(upper, None)))
            }
        }
    }

    /// The name of the file the band is saved in: `25-29.tsv`, or
    /// `130-up.tsv` for 130 or more sitelinks.
    pub(super) fn file_name(&self) -> String {
        match self.max {
            Some(max) => format!("{}-{max}.tsv", self.min),
            None => format!("{}-up.tsv", self.min),
        }
    }

    /// The band saved in a file named `name` by [`SitelinkBand::file_name`].
    fn from_file_name(name: &str) -> Option<SitelinkBand> {
        let (min, max) = name.strip_suffix(".tsv")?.split_once('-')?;
        let min = min.parse().ok()?;
        let max = match max {
            "up" => None,
            max => Some(max.parse().ok()?),
        };
        max.is_none_or(|max| min <= max)
            .then_some(SitelinkBand { min, max })
    }
}

impl fmt::Display for SitelinkBand {
    /// `exactly 25 sitelinks`, `25 to 29 sitelinks` or `130 or more sitelinks`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.max {
            Some(1) if self.min == 1 => f.write_str("exactly 1 sitelink"),
            Some(max) if max == self.min => write!(f, "exactly {max} sitelinks"),
            Some(max) => write!(f, "{} to {max} sitelinks", self.min),
            None => write!(f, "{} or more sitelinks", self.min),
        }
    }
}

/// The SPARQL query for items with an official website and at least
/// `min_sitelinks` sitelinks, with English (or multilingual) labels. The
/// download asks for narrower bands of it ([`SitelinkBand::sparql_query`]).
pub fn wikidata_sparql_query(min_sitelinks: u32) -> String {
    SitelinkBand {
        min: min_sitelinks,
        max: None,
    }
    .sparql_query()
}

/// Bands saved by an earlier download longer ago than this are asked for again.
const SAVED_BAND_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The bands that an earlier download saved in `bands_dir` and that this
/// one can use, lowest first: saved in the last week, with at least
/// `min_sitelinks` sitelinks, and not overlapping one another. Others found
/// there are removed. None when `bands_dir` does not exist.
pub(super) fn saved_bands(bands_dir: &Path, min_sitelinks: u32) -> Result<Vec<SitelinkBand>> {
    let entries = match std::fs::read_dir(bands_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", bands_dir.display())),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", bands_dir.display()))?;
        let band = entry
            .file_name()
            .to_str()
            .and_then(SitelinkBand::from_file_name);
        let recent = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| {
                SystemTime::now()
                    .duration_since(modified)
                    .map_or(true, |age| age < SAVED_BAND_MAX_AGE)
            });
        match band {
            Some(band) if recent && band.min >= min_sitelinks => found.push(band),
            _ => {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    found.sort_by_key(|band| band.min);
    let mut usable: Vec<SitelinkBand> = Vec::new();
    for band in found {
        let overlaps = usable
            .last()
            .is_some_and(|last| last.max.is_none_or(|max| band.min <= max));
        if overlaps {
            let _ = std::fs::remove_file(bands_dir.join(band.file_name()));
        } else {
            usable.push(band);
        }
    }
    Ok(usable)
}

/// The parts of `wanted` (lowest first, not overlapping) that no band of
/// `saved` (the same) covers, lowest first. A band of `wanted` that is
/// partly covered leaves its uncovered stretches.
pub(super) fn missing_bands(wanted: &[SitelinkBand], saved: &[SitelinkBand]) -> Vec<SitelinkBand> {
    let mut missing = Vec::new();
    for &band in wanted {
        // The lowest count of the band not yet seen to be covered; `None`
        // once all of it is.
        let mut from = Some(band.min);
        for s in saved {
            let Some(start) = from else { break };
            if s.max.is_some_and(|max| max < start) {
                continue;
            }
            if band.max.is_some_and(|max| s.min > max) {
                break;
            }
            if s.min > start {
                missing.push(SitelinkBand {
                    min: start,
                    max: Some(s.min - 1),
                });
            }
            from = s
                .max
                .and_then(|max| max.checked_add(1))
                .filter(|&next| band.max.is_none_or(|max| next <= max));
        }
        if let Some(start) = from {
            missing.push(SitelinkBand {
                min: start,
                max: band.max,
            });
        }
    }
    missing
}

/// Saves the rows of `band` in `bands_dir`, through a `.part` file.
pub(super) async fn save_band(
    bands_dir: &Path,
    band: SitelinkBand,
    rows: Vec<WikidataRow>,
) -> Result<()> {
    crate::storage::create_dir_all(bands_dir)
        .await
        .with_context(|| format!("creating {}", bands_dir.display()))?;
    let mut tsv = OfficialSitesTsv::new();
    tsv.add(rows);
    let dest = bands_dir.join(band.file_name());
    let part = part_path(&dest);
    crate::storage::write(&part, tsv.text.as_bytes())
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    crate::storage::rename(&part, &dest)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))
}

/// The rows of a band that [`save_band`] saved.
pub(super) fn saved_rows(text: &str) -> Vec<WikidataRow> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let (item, label, website) = (fields.next()?, fields.next()?, fields.next()?);
            Some(WikidataRow {
                item: item.to_string(),
                label: label.to_string(),
                website: website.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
