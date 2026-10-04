//! The first sentences of the English Wikipedia articles about the
//! organizations behind the best-known official websites: what a site is,
//! in words people search with ("GitHub is a proprietary developer platform
//! that allows developers to create, store, manage, and share their code"),
//! for sites whose homepage says little and whose Wikidata description is
//! only "website".
//!
//! The [`INTRO_ITEMS`] items of the facts file ([`crate::facts`]) with the
//! most sitelinks are looked up: Wikidata's query service gives the title
//! of each one's English article, [`TITLES_BATCH`] items at a time, and
//! Wikipedia's API the start of each article, [`EXTRACTS_BATCH`] at a
//! time. They are saved as `wikipedia-intros.tsv` with the header
//! `item\tintro`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use plumb_core::{collapse_whitespace, truncate_chars};
use serde::Deserialize;
use tracing::{info, warn};

use crate::download::{part_path, WikidataPacing};
use crate::facts::{load_site_facts, sparql_json};
use crate::wikidata::bare_item_id;
use crate::{open_maybe_gz, Line, LineReader, OfficialSite};

/// File name of the intros in a seed directory.
pub const INTROS_FILE_NAME: &str = "wikipedia-intros.tsv";

/// The intros file's first line.
pub const INTROS_HEADER: &str = "item\tintro\n";

/// English Wikipedia's API.
pub const WIKIPEDIA_API_URL: &str = "https://en.wikipedia.org/w/api.php";

/// How many of the best-known items (most sitelinks) get an intro.
pub const INTRO_ITEMS: usize = 20_000;

/// Longest intro kept, in characters.
pub const MAX_INTRO_CHARS: usize = 300;

/// Items per query for their article titles.
const TITLES_BATCH: usize = 2_000;

/// Articles per Wikipedia API request; the most it gives intros for at once.
const EXTRACTS_BATCH: usize = 20;

/// Tries of one Wikipedia API request before the download fails.
const API_TRIES: u32 = 3;

/// A Wikipedia API request without an answer after this long is tried again.
const API_TIMEOUT: Duration = Duration::from_secs(60);

/// Intros by Wikidata item id (`Q739868`).
pub type IntrosByItem = HashMap<String, String>;

/// The SPARQL query for the titles of the English Wikipedia articles about
/// `items`, Wikidata item ids such as `Q739868`.
pub fn titles_query(items: &[String]) -> String {
    let values: String = items.iter().map(|item| format!(" wd:{item}")).collect();
    format!(
        "SELECT ?item ?title WHERE {{ VALUES ?item {{{values} }} \
         ?article schema:about ?item ; schema:isPartOf <https://en.wikipedia.org/> ; \
         schema:name ?title . }}"
    )
}

/// The URL asking Wikipedia's API at `api` for the plain-text start of the
/// articles `titles` (at most [`EXTRACTS_BATCH`]), following redirects.
pub fn extracts_url(api: &str, titles: &[&str]) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("action", "query")
        .append_pair("format", "json")
        .append_pair("formatversion", "2")
        .append_pair("prop", "extracts")
        .append_pair("exintro", "1")
        .append_pair("explaintext", "1")
        .append_pair("exsentences", "2")
        .append_pair("exlimit", &EXTRACTS_BATCH.to_string())
        .append_pair("redirects", "1")
        .append_pair("titles", &titles.join("|"))
        .finish();
    format!("{api}?{query}")
}

/// The items with the most sitelinks in the facts file `facts`, at most
/// `limit`, best known first (ties by item id).
pub fn best_known_items(facts: &Path, limit: usize) -> Result<Vec<String>> {
    let mut items: Vec<(u32, String)> = load_site_facts(facts)?
        .into_iter()
        .filter(|(_, facts)| facts.sitelinks > 0)
        .map(|(item, facts)| (facts.sitelinks, item))
        .collect();
    items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    items.truncate(limit);
    Ok(items.into_iter().map(|(_, item)| item).collect())
}

/// Looks up the English Wikipedia intros of the [`INTRO_ITEMS`] best-known
/// items of the facts file `facts`: their article titles from the SPARQL
/// `endpoint`, then the articles' first sentences from Wikipedia's `api`,
/// with a tenth of `pacing.pause` between API requests. Writes
/// `dir/`[`INTROS_FILE_NAME`]. Fails if a query or request still fails
/// after a few tries, leaving any earlier file in place.
pub async fn download_wikipedia_intros(
    client: &reqwest::Client,
    endpoint: &str,
    api: &str,
    dir: &Path,
    facts: &Path,
    pacing: WikidataPacing,
) -> Result<PathBuf> {
    let facts = facts.to_path_buf();
    let items = tokio::task::spawn_blocking(move || best_known_items(&facts, INTRO_ITEMS))
        .await
        .context("reading the Wikidata facts")??;
    let started = Instant::now();

    let mut titles: Vec<(String, String)> = Vec::new();
    for (i, batch) in items.chunks(TITLES_BATCH).enumerate() {
        if i > 0 {
            tokio::time::sleep(pacing.pause).await;
        }
        let json = sparql_json(client, endpoint, &titles_query(batch), pacing)
            .await
            .with_context(|| {
                format!("asking Wikidata for the articles of {} items", batch.len())
            })?;
        push_titles(&mut titles, &json)?;
    }
    info!(
        "asking Wikipedia for the intros of {} of {} items, {EXTRACTS_BATCH} at a time",
        titles.len(),
        items.len()
    );

    let pause = pacing.pause / 10;
    let mut tsv = String::from(INTROS_HEADER);
    let mut found = 0usize;
    for (i, batch) in titles.chunks(EXTRACTS_BATCH).enumerate() {
        if i > 0 {
            tokio::time::sleep(pause).await;
        }
        let names: Vec<&str> = batch.iter().map(|(_, title)| title.as_str()).collect();
        let json = api_json(client, &extracts_url(api, &names), pacing.retry_wait).await?;
        for (item, intro) in intros_of(batch, &json)? {
            tsv.push_str(&format!("{item}\t{intro}\n"));
            found += 1;
        }
    }

    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(INTROS_FILE_NAME);
    let part = part_path(&dest);
    tokio::fs::write(&part, tsv.as_bytes())
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    tokio::fs::rename(&part, &dest)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
    info!(
        "wrote {found} Wikipedia intros to {} in {:.0} s",
        dest.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(dest)
}

/// Gets `url` from Wikipedia's API, trying again after HTTP 429 or 5xx and
/// failed connections, waiting `wait`, doubling.
async fn api_json(client: &reqwest::Client, url: &str, mut wait: Duration) -> Result<Vec<u8>> {
    let mut tries = 0;
    loop {
        tries += 1;
        let answer = client.get(url).timeout(API_TIMEOUT).send().await;
        let err = match answer {
            Ok(response) if response.status().is_success() => match response.bytes().await {
                Ok(body) => return Ok(body.to_vec()),
                Err(err) => anyhow::Error::new(err).context("reading Wikipedia's answer"),
            },
            Ok(response) => {
                let status = response.status();
                let err = anyhow::anyhow!("Wikipedia's API answered HTTP {status}");
                if status != reqwest::StatusCode::TOO_MANY_REQUESTS && !status.is_server_error() {
                    return Err(err);
                }
                err
            }
            Err(err) => anyhow::Error::new(err).context("asking Wikipedia's API"),
        };
        if tries >= API_TRIES {
            return Err(err.context(format!("tried {tries} times")));
        }
        warn!("{err:#}; trying again in {:.1} s", wait.as_secs_f64());
        tokio::time::sleep(wait).await;
        wait = wait.saturating_mul(2);
    }
}

#[derive(Debug, Deserialize)]
struct TitlesResponse {
    results: TitlesResults,
}

#[derive(Debug, Deserialize)]
struct TitlesResults {
    bindings: Vec<HashMap<String, Term>>,
}

#[derive(Debug, Deserialize)]
struct Term {
    value: String,
}

/// Appends the (item, article title) pairs of the SPARQL JSON results of a
/// [`titles_query`] to `titles`.
fn push_titles(titles: &mut Vec<(String, String)>, json: &[u8]) -> Result<()> {
    let response: TitlesResponse =
        serde_json::from_slice(json).context("parsing Wikidata SPARQL results")?;
    for binding in response.results.bindings {
        let (Some(item), Some(title)) = (binding.get("item"), binding.get("title")) else {
            continue;
        };
        let item = bare_item_id(&item.value);
        let title = title.value.trim();
        if !item.is_empty() && !title.is_empty() && !title.contains('|') {
            titles.push((item.to_string(), title.to_string()));
        }
    }
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
struct ExtractsResponse {
    #[serde(default)]
    query: ExtractsQuery,
}

#[derive(Debug, Default, Deserialize)]
struct ExtractsQuery {
    #[serde(default)]
    normalized: Vec<Renamed>,
    #[serde(default)]
    redirects: Vec<Renamed>,
    #[serde(default)]
    pages: Vec<Page>,
}

#[derive(Debug, Deserialize)]
struct Renamed {
    from: String,
    to: String,
}

#[derive(Debug, Deserialize)]
struct Page {
    title: String,
    #[serde(default)]
    extract: Option<String>,
}

/// The intro of each of the (item, title) pairs `asked` in Wikipedia's
/// answer `json`, following its title normalizations and redirects.
fn intros_of(asked: &[(String, String)], json: &[u8]) -> Result<Vec<(String, String)>> {
    let response: ExtractsResponse =
        serde_json::from_slice(json).context("parsing Wikipedia's answer")?;
    let query = response.query;
    let renamed: HashMap<&str, &str> = query
        .normalized
        .iter()
        .chain(&query.redirects)
        .map(|r| (r.from.as_str(), r.to.as_str()))
        .collect();
    let extracts: HashMap<&str, &str> = query
        .pages
        .iter()
        .filter_map(|page| Some((page.title.as_str(), page.extract.as_deref()?)))
        .collect();
    let mut intros = Vec::new();
    for (item, title) in asked {
        let mut title = title.as_str();
        // A normalized title can itself redirect.
        for _ in 0..2 {
            if let Some(&to) = renamed.get(title) {
                title = to;
            }
        }
        if let Some(intro) = extracts.get(title).and_then(|text| clean_intro(text)) {
            intros.push((item.clone(), intro));
        }
    }
    Ok(intros)
}

/// The intro of an article's plain-text start: one line, without the
/// parenthesized pronunciations and native names that follow the subject
/// ("Spotify (; Swedish: [ˈspɔ̂tːɪfaɪ]) is"), cut to [`MAX_INTRO_CHARS`] at a
/// word. `None` when nothing is left.
pub fn clean_intro(text: &str) -> Option<String> {
    let text = collapse_whitespace(&text.replace(['\t', '\n', '\r'], " "));
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let out = collapse_whitespace(&out.replace(" ,", ",").replace(" .", "."));
    if out.is_empty() {
        return None;
    }
    if out.chars().count() <= MAX_INTRO_CHARS {
        return Some(out);
    }
    let cut = truncate_chars(&out, MAX_INTRO_CHARS);
    let cut = match cut.rfind(' ') {
        Some(space) => &cut[..space],
        None => cut.as_str(),
    };
    Some(cut.trim_end_matches([',', ';', ':']).to_string())
}

/// Reads an intros file ([`INTROS_FILE_NAME`]); it may be gzipped.
pub fn load_intros(path: &Path) -> Result<IntrosByItem> {
    let mut lines = LineReader::new(open_maybe_gz(path)?);
    let read_err = || format!("reading {}", path.display());
    let mut intros = IntrosByItem::new();
    let mut header = true;
    while let Some((_, line)) = lines.next_line().with_context(read_err)? {
        let Line::Text(line) = line else {
            continue;
        };
        if std::mem::take(&mut header) {
            continue;
        }
        let Some((item, intro)) = line.split_once('\t') else {
            continue;
        };
        let item = bare_item_id(item.trim());
        let intro = intro.trim();
        if !item.is_empty() && !intro.is_empty() {
            intros
                .entry(item.to_string())
                .or_insert_with(|| truncate_chars(intro, MAX_INTRO_CHARS));
        }
    }
    info!(
        "loaded Wikipedia intros for {} items from {}",
        intros.len(),
        path.display()
    );
    Ok(intros)
}

/// Copies each item's intro onto its official website claims.
pub fn attach_intros(sites: &mut [OfficialSite], intros: &IntrosByItem) {
    for site in sites {
        if let Some(intro) = intros.get(&site.item) {
            site.intro = Some(intro.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intros_follow_renames_and_lose_pronunciations() {
        let asked = vec![
            ("Q1".to_string(), "spotify".to_string()),
            ("Q2".to_string(), "GitHub".to_string()),
            ("Q3".to_string(), "Gone".to_string()),
        ];
        let answer = r#"{"query":{
            "normalized":[{"from":"spotify","to":"Spotify"}],
            "redirects":[{"from":"GitHub","to":"GitHub (service)"}],
            "pages":[
                {"title":"Spotify","extract":"Spotify (; Swedish: [ˈspɔ̂tːɪfaɪ]) is a Swedish audio streaming and media service provider.\nIt was founded in 2006."},
                {"title":"GitHub (service)","extract":"GitHub is a proprietary developer platform that allows developers to create, store, manage, and share their code."},
                {"title":"Gone","missing":true}
            ]}}"#;
        let intros = intros_of(&asked, answer.as_bytes()).unwrap();
        assert_eq!(
            intros,
            [
                (
                    "Q1".to_string(),
                    "Spotify is a Swedish audio streaming and media service provider. It was founded in 2006."
                        .to_string()
                ),
                (
                    "Q2".to_string(),
                    "GitHub is a proprietary developer platform that allows developers to create, store, manage, and share their code."
                        .to_string()
                ),
            ]
        );
    }

    #[test]
    fn long_intros_are_cut_at_a_word() {
        let long = "word ".repeat(100);
        let intro = clean_intro(&long).unwrap();
        assert!(intro.chars().count() <= MAX_INTRO_CHARS);
        assert!(intro.ends_with("word"));
        assert_eq!(clean_intro("  (only this) "), None);
    }

    #[test]
    fn titles_parse_and_intros_round_trip() {
        let answer = br#"{"results":{"bindings":[
            {"item":{"value":"http://www.wikidata.org/entity/Q1"},"title":{"value":"Spotify"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q2"},"title":{"value":"A|B"}}
        ]}}"#;
        let mut titles = Vec::new();
        push_titles(&mut titles, answer).unwrap();
        assert_eq!(titles, [("Q1".to_string(), "Spotify".to_string())]);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(INTROS_FILE_NAME);
        std::fs::write(&path, format!("{INTROS_HEADER}Q1\tSpotify is a service.\n")).unwrap();
        let intros = load_intros(&path).unwrap();
        assert_eq!(intros["Q1"], "Spotify is a service.");
        let mut sites =
            vec![OfficialSite::new("Q1", "Spotify", "https://www.spotify.com/").unwrap()];
        attach_intros(&mut sites, &intros);
        assert_eq!(sites[0].intro.as_deref(), Some("Spotify is a service."));
    }

    #[test]
    fn extracts_urls_ask_for_plain_intros() {
        let url = extracts_url(WIKIPEDIA_API_URL, &["Spotify", "AT&T"]);
        assert!(url.starts_with("https://en.wikipedia.org/w/api.php?action=query"));
        assert!(url.contains("exintro=1"));
        assert!(url.contains("titles=Spotify%7CAT%26T"));
    }
}
