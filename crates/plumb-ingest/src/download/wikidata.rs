//! The official websites from Wikidata: listed by a mirror in one query, or
//! by Wikidata's own query service one band of sitelink counts at a time,
//! paced and asked again while it is too busy.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tracing::{info, warn};

use super::{part_path, WIKIDATA_FILE_NAME, WIKIDATA_PREFIXES};
#[cfg(doc)]
use super::{QLEVER_WIKIDATA_URL, WIKIDATA_SPARQL_URL};

mod bands;
mod sparql;

use bands::{missing_bands, save_band, saved_bands, saved_rows};
use sparql::{try_query, Failed, OfficialSitesTsv, WikidataRow};

pub use bands::{wikidata_sitelink_bands, wikidata_sparql_query, SitelinkBand};
pub use sparql::wikidata_json_to_tsv;

/// How [`download_wikidata_official_sites_paced`] spaces out its queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WikidataPacing {
    /// The wait before each band's query but the first. Wikidata's query
    /// service limits the query time each client uses per minute, and
    /// answers HTTP 429 to clients over the limit.
    pub pause: Duration,
    /// The first wait before asking for a band again, after Wikidata was too
    /// busy to answer it whole (or answered HTTP 429 or 5xx without a
    /// `Retry-After`, or could not be reached). It doubles with each try of
    /// the same band, up to 10 times this. All the waits of one download
    /// together may come to 40 times this ([`WikidataPacing::budget`]).
    pub retry_wait: Duration,
}

impl Default for WikidataPacing {
    /// 5 seconds between two bands; 30, 60, 120, 240 and 300 seconds before
    /// asking for a band again; 20 minutes of such waits in all.
    fn default() -> Self {
        WikidataPacing {
            pause: Duration::from_secs(5),
            retry_wait: Duration::from_secs(30),
        }
    }
}

impl WikidataPacing {
    /// The wait before try `tries + 1` of a band: `retry_wait`, doubled for
    /// each try after the first, and at most 10 times `retry_wait`.
    pub fn wait_after(&self, tries: u32) -> Duration {
        let doubled = 1u32
            .checked_shl(tries.saturating_sub(1))
            .unwrap_or(u32::MAX)
            .min(MAX_WAIT_FACTOR);
        self.retry_wait.saturating_mul(doubled)
    }

    /// The shortest wait after HTTP 429 without a `Retry-After`: twice
    /// `retry_wait`, 60 seconds by default.
    pub fn rate_limit_wait(&self) -> Duration {
        self.retry_wait.saturating_mul(2)
    }

    /// How long one download may wait, in all, before asking for bands
    /// again: 40 times `retry_wait`. A download that would wait longer
    /// stops; what it has is saved for the next.
    pub fn budget(&self) -> Duration {
        self.retry_wait.saturating_mul(WAIT_BUDGET_FACTOR)
    }
}

/// The longest wait before asking for a band again, as a multiple of
/// [`WikidataPacing::retry_wait`].
const MAX_WAIT_FACTOR: u32 = 10;

/// All the waits of one download, as a multiple of
/// [`WikidataPacing::retry_wait`].
const WAIT_BUDGET_FACTOR: u32 = 40;

/// Tries of one band before it is split, or the download given up.
const WIKIDATA_TRIES: u32 = 6;

/// Answers of one band saying Wikidata is too busy before it is halved: a
/// narrower query is more likely to finish than more waiting (on
/// 2026-10-03 the 25 to 29 sitelinks band timed out five times in a row
/// while 25-26 and 27-29 had come in that morning).
const BUSY_TRIES: u32 = 3;

/// How many times a band of [`wikidata_sitelink_bands`] may be halved: a
/// band becomes at most 4 queries. Halving barely speeds a query up, so it
/// is only done when a band failed all its tries.
const MAX_SPLITS: u32 = 2;

/// The longest `Retry-After` a download waits for; an answer that asks for
/// more fails the download, to be tried again much later.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

/// The folder next to the TSV where each band is saved as it comes in,
/// until the TSV is written.
pub const WIKIDATA_BANDS_DIR_NAME: &str = "wikidata-official-sites.bands";

/// Asks the SPARQL endpoint at `endpoint` ([`WIKIDATA_SPARQL_URL`], or a
/// mirror) for items with an official website (P856) and at least
/// `min_sitelinks` sitelinks (a notability filter), and writes
/// `dir/wikidata-official-sites.tsv` with the header `item\tlabel\twebsite`.
///
/// Wikidata's public query service stops every query after 60 seconds, and
/// listing all these items takes longer than that. So they are asked for in
/// bands of sitelink counts ([`wikidata_sitelink_bands`]), one query after
/// another. Each band takes 20 to 50 seconds even when small, since most of
/// the work is the same scan of every item with an official website, so
/// whether it ends within the limit depends on how busy the service is.
///
/// The bands are asked for lowest first, waiting `pacing.pause` before each
/// but the first, and each answer is logged. Every band that comes in whole
/// is saved at once in `dir/wikidata-official-sites.bands/`; a download
/// finds the bands an earlier one saved there (in the last week, for the
/// same or a lower minimum) and asks only for the rest. Once every band is in, their rows go
/// into the TSV, through a `.part` file renamed when complete, and the saved
/// bands are removed.
///
/// An answer must be whole: when Wikidata stops a query at its time limit,
/// the answer still comes as HTTP 200, but its JSON breaks off and the query
/// service's Java exception follows. Such an answer, one that ends in that
/// exception (also as HTTP 500), HTTP 503 or 504, and no whole answer within
/// 3 minutes all mean that Wikidata was too busy: the band is asked for
/// again after a wait ([`WikidataPacing::wait_after`]). A band too busy 3
/// times is asked for in two halves
/// ([`SitelinkBand::halves`]), and a band of the defaults is halved at most
/// twice; then the download fails.
///
/// A band still too busy when the waits run out is halved too, each half
/// getting at least one try, so a long wait on one band does not end the
/// download while narrower queries might get through.
///
/// HTTP 429 and other 5xx, and failed connections, are tried again the same
/// way, but not halved; after HTTP 429 the wait is at least
/// [`WikidataPacing::rate_limit_wait`]. A `Retry-After` is waited for
/// instead (at most 10 minutes; an answer that asks for more fails the
/// download). The waits of
/// one download come to at most [`WikidataPacing::budget`]; past that, it
/// fails. Other failures, such as HTTP 4xx or an answer that is not SPARQL
/// JSON, fail the download at once. A download that fails keeps the bands it
/// saved for the next.
pub async fn download_wikidata_official_sites_paced(
    client: &reqwest::Client,
    endpoint: &str,
    dir: &Path,
    min_sitelinks: u32,
    pacing: WikidataPacing,
) -> Result<PathBuf> {
    let bands_dir = dir.join(WIKIDATA_BANDS_DIR_NAME);
    let mut done = saved_bands(&bands_dir, min_sitelinks)?;
    let missing = missing_bands(&wikidata_sitelink_bands(min_sitelinks), &done);
    if done.is_empty() {
        info!(
            "asking Wikidata for official websites of items with at least {min_sitelinks} \
             sitelinks, in {} queries by number of sitelinks",
            missing.len()
        );
    } else {
        info!(
            "asking Wikidata for official websites of items with at least {min_sitelinks} \
             sitelinks: {} bands of sitelink counts were saved by an earlier try, {} to go",
            done.len(),
            missing.len()
        );
    }
    let started = Instant::now();
    let mut budget = pacing.budget();
    // The bands still to ask for, the next one last, with how many times
    // each was halved.
    let mut todo: Vec<(SitelinkBand, u32)> = missing.into_iter().rev().map(|b| (b, 0)).collect();
    let mut queries = 0u32;
    while let Some((band, splits)) = todo.pop() {
        if queries > 0 {
            tokio::time::sleep(pacing.pause).await;
        }
        let asked = Instant::now();
        let answer = query_band(client, endpoint, band, pacing, &mut budget, &mut queries).await;
        match answer {
            Ok(rows) => {
                let got = rows.len();
                save_band(&bands_dir, band, rows).await?;
                done.push(band);
                info!(
                    "Wikidata: {got} official websites of items with {band} in {:.1} s; \
                     {} bands to go",
                    asked.elapsed().as_secs_f64(),
                    todo.len()
                );
            }
            Err(BandFailure::Busy(why)) => {
                let halves = band.halves().filter(|_| splits < MAX_SPLITS);
                let Some((lower, upper)) = halves else {
                    bail!(
                        "Wikidata was too busy to answer the query for items with {band} \
                         ({why}); {}{}",
                        match splits {
                            0 => "it cannot be split; ".to_string(),
                            n => format!("it is already split {n} times; "),
                        },
                        try_later(&done)
                    );
                };
                warn!(
                    "Wikidata was too busy to answer the query for items with {band} ({why}); \
                     asking for items with {lower} and with {upper} separately"
                );
                todo.push((upper, splits + 1));
                todo.push((lower, splits + 1));
            }
            Err(BandFailure::Fatal(err)) => {
                return Err(err.context(format!(
                    "asking Wikidata for items with {band} ({})",
                    try_later(&done)
                )));
            }
        }
    }

    done.sort_by_key(|band| band.min);
    let mut tsv = OfficialSitesTsv::new();
    for &band in &done {
        let path = bands_dir.join(band.file_name());
        let text = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        tsv.add(saved_rows(&text));
    }
    crate::storage::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(WIKIDATA_FILE_NAME);
    let part = part_path(&dest);
    crate::storage::write(&part, tsv.text.as_bytes())
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    crate::storage::rename(&part, &dest)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
    if let Err(err) = crate::storage::remove_dir_all(&bands_dir).await {
        if err.kind() != io::ErrorKind::NotFound {
            warn!("could not remove {}: {err}", bands_dir.display());
        }
    }
    info!(
        "wrote {} official websites to {} after {queries} queries in {:.0} s",
        tsv.rows(),
        dest.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(dest)
}

/// [`download_wikidata_official_sites_paced`], asking the `mirror` first
/// when there is one ([`download_wikidata_official_sites_bulk`]): one query
/// there lists them all in seconds, while Wikidata's own query service
/// needs a query per band of sitelink counts, each close to its 60-second
/// limit. When the mirror fails, the bands are asked of `endpoint`, from
/// [`ENDPOINT_MIN_SITELINKS`] up at least: below that, bands of a single
/// count run past its limit too (exactly 3 sitelinks did on 2026-10-09),
/// and a node is better off with the better-known sites than with none.
pub async fn download_wikidata_official_sites_with(
    client: &reqwest::Client,
    mirror: Option<&str>,
    endpoint: &str,
    dir: &Path,
    min_sitelinks: u32,
    pacing: WikidataPacing,
) -> Result<PathBuf> {
    if let Some(mirror) = mirror {
        match download_wikidata_official_sites_bulk(client, mirror, dir, min_sitelinks, pacing)
            .await
        {
            Ok(path) => return Ok(path),
            Err(err) => {
                let min = min_sitelinks.max(ENDPOINT_MIN_SITELINKS);
                warn!(
                    "could not list the official websites at {mirror} ({err:#}); \
                     asking {endpoint} band by band, from {min} sitelinks up"
                );
                return download_wikidata_official_sites_paced(client, endpoint, dir, min, pacing)
                    .await;
            }
        }
    }
    download_wikidata_official_sites_paced(client, endpoint, dir, min_sitelinks, pacing).await
}

/// The fewest sitelinks [`download_wikidata_official_sites_with`] asks
/// Wikidata's own endpoint for once the mirror has failed.
pub const ENDPOINT_MIN_SITELINKS: u32 = 25;

/// The fewest sitelinks of the items whose official websites are fetched
/// by default. On 2026-10-09, going from 25 down to 3 (131k to 627k sites)
/// put the right site in the top 10 for 64.7% of described searches
/// instead of 56.9% (tune half; 71.0% instead of 68.8% held out), with
/// brand and ai searches about level: Expedia, Indeed, Glassdoor and
/// Instacart are below 25.
pub const DEFAULT_MIN_SITELINKS: u32 = 3;

/// The query for every item with an official website and at least
/// `min_sitelinks` sitelinks, with its English (or multilingual) label, or
/// its id when it has neither, as Wikidata's label service gives. It needs
/// no label service, so endpoints other than Wikidata's own can answer it.
/// The sitelink count is read as a number first, since QLever compares its
/// counts with a plain number as never equal.
pub fn bulk_official_sites_query(min_sitelinks: u32) -> String {
    format!(
        "{WIKIDATA_PREFIXES}SELECT ?item ?itemLabel ?website WHERE {{ \
         ?item wdt:P856 ?website ; wikibase:sitelinks ?s . \
         FILTER(xsd:integer(?s) >= {min_sitelinks}) \
         OPTIONAL {{ ?item rdfs:label ?en . FILTER(LANG(?en) = \"en\") }} \
         OPTIONAL {{ ?item rdfs:label ?mul . FILTER(LANG(?mul) = \"mul\") }} \
         BIND(COALESCE(?en, ?mul, STRAFTER(STR(?item), \"/entity/\")) AS ?itemLabel) }}"
    )
}

/// Fewer rows than this from [`download_wikidata_official_sites_bulk`]
/// means the endpoint did not understand the query as meant: there were
/// 131,386 items from 25 sitelinks up in October 2026, and an empty answer
/// would leave a node without official websites.
const MIN_BULK_ROWS: usize = 1_000;

/// Asks `endpoint`, such as [`QLEVER_WIKIDATA_URL`], for every official
/// website of items with at least `min_sitelinks` sitelinks in one query
/// ([`bulk_official_sites_query`]), and writes the same TSV as
/// [`download_wikidata_official_sites_paced`]. An answer that breaks off,
/// HTTP 429 or 5xx and failed connections are tried again after the waits
/// of `pacing` (or the `Retry-After`), up to 6 tries; an answer with fewer
/// than 1,000 rows is an error, so a mirror that reads the query
/// differently is not taken for the truth.
pub async fn download_wikidata_official_sites_bulk(
    client: &reqwest::Client,
    endpoint: &str,
    dir: &Path,
    min_sitelinks: u32,
    pacing: WikidataPacing,
) -> Result<PathBuf> {
    info!(
        "asking {endpoint} for official websites of items with at least {min_sitelinks} \
         sitelinks, in one query"
    );
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("query", &bulk_official_sites_query(min_sitelinks))
        .finish();
    let started = Instant::now();
    let mut tries = 0;
    let rows = loop {
        tries += 1;
        let (why, after, rate_limited) = match try_query(client, endpoint, &form).await {
            Ok(rows) => break rows,
            Err(Failed::Fatal(err)) => return Err(err),
            Err(Failed::Again {
                why,
                after,
                rate_limited,
                ..
            }) => (why, after, rate_limited),
        };
        if tries >= WIKIDATA_TRIES {
            bail!("{why} (tried {tries} times)");
        }
        let wait = match after {
            Some(after) if after > MAX_RETRY_AFTER => bail!(
                "{why}; the answer asks to wait {} seconds before the next query",
                after.as_secs()
            ),
            Some(after) => after,
            None if rate_limited => pacing.wait_after(tries).max(pacing.rate_limit_wait()),
            None => pacing.wait_after(tries),
        };
        warn!(
            "{endpoint}: {why}; trying again in {:.1} s",
            wait.as_secs_f64()
        );
        tokio::time::sleep(wait).await;
    };
    if rows.len() < MIN_BULK_ROWS {
        bail!(
            "only {} official websites came back, fewer than the {MIN_BULK_ROWS} expected",
            rows.len()
        );
    }
    let mut tsv = OfficialSitesTsv::new();
    tsv.add(rows);
    crate::storage::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(WIKIDATA_FILE_NAME);
    let part = part_path(&dest);
    crate::storage::write(&part, tsv.text.as_bytes())
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    crate::storage::rename(&part, &dest)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
    info!(
        "wrote {} official websites to {} after {tries} queries in {:.0} s",
        tsv.rows(),
        dest.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(dest)
}

/// What a failed download says about trying again, given the bands saved.
fn try_later(done: &[SitelinkBand]) -> String {
    match done.len() {
        0 => "try again later".to_string(),
        1 => "try again later; the 1 band already in is saved".to_string(),
        n => format!("try again later; the {n} bands already in are saved"),
    }
}

/// Why the query for one band has no rows.
#[derive(Debug)]
enum BandFailure {
    /// Wikidata stayed too busy to answer it whole through every try: ask
    /// for less. Says what was seen last.
    Busy(String),
    /// Not worth asking again in this download.
    Fatal(anyhow::Error),
}

/// Asks for one band, again after a wait while Wikidata is too busy or
/// answers HTTP 429 or 5xx, or cannot be reached; see
/// [`download_wikidata_official_sites_paced`]. `budget` is what is left of
/// the download's waits, and `queries` counts every query sent.
async fn query_band(
    client: &reqwest::Client,
    endpoint: &str,
    band: SitelinkBand,
    pacing: WikidataPacing,
    budget: &mut Duration,
    queries: &mut u32,
) -> Result<Vec<WikidataRow>, BandFailure> {
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("query", &band.sparql_query())
        .finish();
    let mut tries = 0;
    let mut busy_tries = 0;
    loop {
        tries += 1;
        *queries += 1;
        let (why, after, busy, rate_limited) = match try_query(client, endpoint, &form).await {
            Ok(rows) => return Ok(rows),
            Err(Failed::Fatal(err)) => return Err(BandFailure::Fatal(err)),
            Err(Failed::Again {
                why,
                after,
                busy,
                rate_limited,
            }) => (why, after, busy, rate_limited),
        };
        busy_tries += u32::from(busy);
        if busy && busy_tries >= BUSY_TRIES {
            return Err(BandFailure::Busy(format!("{why} (tried {tries} times)")));
        }
        if tries >= WIKIDATA_TRIES {
            let why = format!("{why} (tried {tries} times)");
            return Err(if busy {
                BandFailure::Busy(why)
            } else {
                BandFailure::Fatal(anyhow!(why))
            });
        }
        let wait = match after {
            Some(after) if after > MAX_RETRY_AFTER => {
                return Err(BandFailure::Fatal(anyhow!(
                    "{why}; the answer asks to wait {} seconds before the next query",
                    after.as_secs()
                )))
            }
            Some(after) => after,
            None if rate_limited => pacing.wait_after(tries).max(pacing.rate_limit_wait()),
            None => pacing.wait_after(tries),
        };
        if wait > *budget && busy {
            // Out of waiting time: a smaller query is the better bet.
            return Err(BandFailure::Busy(format!(
                "{why} (tried {tries} times, with no waiting time left)"
            )));
        }
        if wait > *budget {
            return Err(BandFailure::Fatal(anyhow!(
                "{why} (tried {tries} times); waiting {wait:?} more would pass the {:?} this \
                 download may wait in all",
                pacing.budget()
            )));
        }
        *budget -= wait;
        warn!(
            "Wikidata query for items with {band}: {why}; trying again in {:.1} s",
            wait.as_secs_f64()
        );
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
mod tests;
