//! Official profiles ([`plumb_core::profiles`]) from Wikidata, added to a
//! Wikipedia articles file: each article whose item has a YouTube channel,
//! an X account, an app in the App Store and so on gets a line of them.
//!
//! Each service's property is asked for whole, a page of
//! [`PROFILES_PAGE`] statements at a time: a query for one property and
//! nothing else is a scan of one index, which the query service answers in
//! seconds. Before that, each property's formatter URL (P1630) is checked
//! to point at the service's site, so a mistaken property number in
//! [`plumb_core::profiles::SERVICES`] leaves that service out rather than
//! linking somewhere wrong.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::article::{articles_of, write_article, ARTICLES_HEADER};
use plumb_core::profiles::{Profile, Service, SERVICES};
use serde::Deserialize;
use tracing::{info, warn};

use crate::download::{part_path, WikidataPacing};
use crate::facts::sparql_json;
use crate::open_maybe_gz;
use crate::wikidata::{ItemSite, OfficialSite};

/// Statements asked for in one query.
pub const PROFILES_PAGE: usize = 200_000;

/// Profiles by Wikidata item (`Q19897578`).
pub type ProfilesByItem = HashMap<String, Vec<Profile>>;

/// Official websites by Wikidata item.
pub type WebsitesByItem = HashMap<String, ItemSite>;

#[derive(Debug, Deserialize)]
struct Response {
    results: Results,
}

#[derive(Debug, Deserialize)]
struct Results {
    bindings: Vec<HashMap<String, Term>>,
}

#[derive(Debug, Deserialize)]
struct Term {
    value: String,
}

fn bindings(json: &[u8]) -> Result<Vec<HashMap<String, Term>>> {
    let response: Response = serde_json::from_slice(json).context("reading the Wikidata answer")?;
    Ok(response.results.bindings)
}

/// `Q42` of `http://www.wikidata.org/entity/Q42`.
fn entity_id(uri: &str) -> &str {
    uri.rsplit('/').next().unwrap_or(uri)
}

/// The query for the formatter URLs of every service's property.
fn formatters_query() -> String {
    let values: Vec<String> = SERVICES
        .iter()
        .map(|s| format!("wd:{}", s.property))
        .collect();
    format!(
        "SELECT ?p ?f WHERE {{ VALUES ?p {{ {} }} ?p wdt:P1630 ?f . }}",
        values.join(" ")
    )
}

/// Hosts a service's formatter URL may name besides [`Service::host`].
fn other_hosts(service: &Service) -> &'static [&'static str] {
    match service.key {
        "x" => &["twitter.com"],
        _ => &[],
    }
}

/// The services whose property's formatter URL, in the answer to
/// [`formatters_query`], points at the service's site. Mastodon's
/// addresses name the user's own server, so its property is taken as it is.
fn checked_services(json: &[u8]) -> Result<Vec<&'static Service>> {
    let mut formatters: HashMap<String, Vec<String>> = HashMap::new();
    for row in bindings(json)? {
        if let (Some(p), Some(f)) = (row.get("p"), row.get("f")) {
            formatters
                .entry(entity_id(&p.value).to_string())
                .or_default()
                .push(f.value.to_lowercase());
        }
    }
    let mut checked = Vec::new();
    for service in SERVICES {
        let found = formatters
            .get(service.property)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let fits = service.host.is_empty()
            || found.iter().any(|f| {
                std::iter::once(service.host)
                    .chain(other_hosts(service).iter().copied())
                    .any(|host| f.contains(host))
            });
        if fits {
            checked.push(service);
        } else {
            warn!(
                "left out {} profiles: Wikidata's {} formats as {found:?}, not {}",
                service.name, service.property, service.host
            );
        }
    }
    Ok(checked)
}

fn page_query(service: &Service, offset: usize) -> String {
    format!(
        "SELECT ?item ?id WHERE {{ ?item wdt:{} ?id . }} LIMIT {PROFILES_PAGE} OFFSET {offset}",
        service.property
    )
}

/// Adds the rows of an answer to [`page_query`] to `profiles`; returns how
/// many rows there were. An item keeps one identifier per service, the
/// first that looks right.
fn add_page(profiles: &mut ProfilesByItem, service: &Service, json: &[u8]) -> Result<usize> {
    let rows = bindings(json)?;
    for row in &rows {
        let (Some(item), Some(id)) = (row.get("item"), row.get("id")) else {
            continue;
        };
        let item = entity_id(&item.value);
        let id = id.value.trim().trim_start_matches('@');
        if !item.starts_with('Q') || !service.accepts(id) {
            continue;
        }
        let kept = profiles.entry(item.to_string()).or_default();
        if !kept.iter().any(|p| p.service == service.key) {
            kept.push(Profile {
                service: service.key.to_string(),
                id: id.to_string(),
            });
        }
    }
    Ok(rows.len())
}

/// Asks Wikidata's query service at `endpoint` for every service's
/// profiles.
pub async fn fetch_profiles(
    client: &reqwest::Client,
    endpoint: &str,
    pacing: WikidataPacing,
) -> Result<ProfilesByItem> {
    let json = sparql_json(client, endpoint, &formatters_query(), pacing).await?;
    let services = checked_services(&json)?;
    if services.is_empty() {
        bail!("no service's Wikidata property checked out");
    }
    let mut profiles = ProfilesByItem::new();
    for service in services {
        let mut offset = 0;
        loop {
            tokio::time::sleep(pacing.pause).await;
            let json = sparql_json(client, endpoint, &page_query(service, offset), pacing)
                .await
                .with_context(|| format!("asking Wikidata for {} profiles", service.name))?;
            let rows = add_page(&mut profiles, service, &json)?;
            info!(
                "{} ({}): {} statements from {offset}",
                service.name, service.property, rows
            );
            if rows < PROFILES_PAGE {
                break;
            }
            offset += PROFILES_PAGE;
        }
    }
    // The order an info box lists them in, whatever order they came in.
    for kept in profiles.values_mut() {
        kept.sort_by_key(|p| SERVICES.iter().position(|s| s.key == p.service));
    }
    Ok(profiles)
}

/// The query for a page of official website (P856) statements.
fn websites_page_query(offset: usize) -> String {
    format!(
        "SELECT ?item ?url WHERE {{ ?item wdt:P856 ?url . }} LIMIT {PROFILES_PAGE} OFFSET {offset}"
    )
}

/// Adds the rows of an answer to [`websites_page_query`] to `websites`;
/// returns how many rows there were.
fn add_websites_page(websites: &mut WebsitesByItem, json: &[u8]) -> Result<usize> {
    let rows = bindings(json)?;
    for row in &rows {
        let (Some(item), Some(url)) = (row.get("item"), row.get("url")) else {
            continue;
        };
        let item = entity_id(&item.value);
        if !item.starts_with('Q') {
            continue;
        }
        if let Some(claim) = OfficialSite::new(item, "", &url.value) {
            ItemSite::add(websites, &claim);
        }
    }
    Ok(rows.len())
}

/// Asks Wikidata's query service at `endpoint` for every item's official
/// websites, to link the ones that are part of another site: YouTube
/// Music's `https://music.youtube.com/`.
pub async fn fetch_websites(
    client: &reqwest::Client,
    endpoint: &str,
    pacing: WikidataPacing,
) -> Result<WebsitesByItem> {
    let mut websites = WebsitesByItem::new();
    let mut offset = 0;
    loop {
        tokio::time::sleep(pacing.pause).await;
        let json = sparql_json(client, endpoint, &websites_page_query(offset), pacing)
            .await
            .context("asking Wikidata for official websites")?;
        let rows = add_websites_page(&mut websites, &json)?;
        info!("official websites (P856): {rows} statements from {offset}");
        if rows < PROFILES_PAGE {
            break;
        }
        offset += PROFILES_PAGE;
    }
    Ok(websites)
}

/// What [`add_profiles_to_file`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AddedProfiles {
    pub articles: u64,
    pub with_profiles: u64,
    pub profiles: u64,
    /// Articles linking their item's official website on another site.
    pub websites: u64,
}

/// Rewrites the articles file `path` with `profiles` and official
/// `websites` for its articles' items, replacing any it had, by way of a
/// part file. A website counts only when it is on the article's site.
pub fn add_profiles_to_file(
    path: &Path,
    profiles: &ProfilesByItem,
    websites: &WebsitesByItem,
) -> Result<AddedProfiles> {
    let reader = open_maybe_gz(path)?;
    let mut failed = None;
    let lines = std::io::BufRead::lines(reader).map_while(|line| match line {
        Ok(line) => Some(line),
        Err(err) => {
            failed = Some(err);
            None
        }
    });
    let part = part_path(path);
    let file =
        std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?;
    let mut out = flate2::write::GzEncoder::new(
        std::io::BufWriter::new(file),
        flate2::Compression::default(),
    );
    out.write_all(ARTICLES_HEADER.as_bytes())?;
    let mut added = AddedProfiles::default();
    for (n, article) in articles_of(lines) {
        let mut article = article.with_context(|| format!("{} line {n}", path.display()))?;
        article.profiles = article
            .item
            .as_deref()
            .and_then(|item| profiles.get(item))
            .cloned()
            .unwrap_or_default();
        article.website = article
            .item
            .as_deref()
            .and_then(|item| websites.get(item))
            .filter(|site| article.site.as_deref() == Some(site.domain.as_str()))
            .and_then(ItemSite::website)
            .map(str::to_string);
        added.articles += 1;
        added.websites += u64::from(article.website.is_some());
        if !article.profiles.is_empty() {
            added.with_profiles += 1;
            added.profiles += article.profiles.len() as u64;
        }
        write_article(&mut out, &article)?;
    }
    if let Some(err) = failed {
        return Err(anyhow::Error::new(err).context(format!("reading {}", path.display())));
    }
    out.finish()?
        .into_inner()
        .map_err(|e| e.into_error())?
        .sync_all()?;
    std::fs::rename(&part, path)
        .with_context(|| format!("renaming {} to {}", part.display(), path.display()))?;
    Ok(added)
}

#[cfg(test)]
mod tests {
    use plumb_core::article::{read_articles, Article};
    use plumb_core::profiles::service_by_key;

    use super::*;

    fn answer(rows: &[(&str, &str, &str, &str)]) -> Vec<u8> {
        let bindings: Vec<serde_json::Value> = rows
            .iter()
            .map(|(k1, v1, k2, v2)| serde_json::json!({ *k1: { "value": v1 }, *k2: { "value": v2 } }))
            .collect();
        serde_json::to_vec(&serde_json::json!({ "results": { "bindings": bindings } })).unwrap()
    }

    #[test]
    fn checks_each_property_formats_for_its_service() {
        let json = answer(&[
            (
                "p",
                "http://www.wikidata.org/entity/P2397",
                "f",
                "https://www.youtube.com/channel/$1",
            ),
            (
                "p",
                "http://www.wikidata.org/entity/P2002",
                "f",
                "https://twitter.com/$1",
            ),
            (
                "p",
                "http://www.wikidata.org/entity/P5797",
                "f",
                "https://example.org/$1",
            ),
        ]);
        let keys: Vec<&str> = checked_services(&json)
            .unwrap()
            .iter()
            .map(|s| s.key)
            .collect();
        assert!(keys.contains(&"youtube"));
        assert!(
            keys.contains(&"x"),
            "X's property still formats as twitter.com"
        );
        assert!(keys.contains(&"mastodon"));
        assert!(!keys.contains(&"twitch"), "a formatter for another site");
        assert!(!keys.contains(&"github"), "no formatter at all");
    }

    #[test]
    fn keeps_one_good_identifier_per_service() {
        let x = service_by_key("x").unwrap();
        let mut profiles = ProfilesByItem::new();
        let json = answer(&[
            ("item", "http://www.wikidata.org/entity/Q1", "id", "@first"),
            ("item", "http://www.wikidata.org/entity/Q1", "id", "second"),
            ("item", "http://www.wikidata.org/entity/Q2", "id", "not ok!"),
        ]);
        assert_eq!(add_page(&mut profiles, x, &json).unwrap(), 3);
        assert_eq!(
            profiles["Q1"],
            [Profile {
                service: "x".into(),
                id: "first".into()
            }]
        );
        assert!(!profiles.contains_key("Q2"));
    }

    #[test]
    fn adds_profiles_to_an_articles_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wikipedia-en.tsv.gz");
        let articles = [
            Article {
                title: "MrBeast".into(),
                item: Some("Q19897578".into()),
                views: 9,
                ..Article::default()
            },
            Article {
                title: "Plain".into(),
                item: Some("Q5".into()),
                views: 1,
                ..Article::default()
            },
            Article {
                title: "YouTube Music".into(),
                item: Some("Q28404534".into()),
                site: Some("youtube.com".into()),
                views: 1,
                ..Article::default()
            },
        ];
        crate::articles::write_articles_file(&path, &articles).unwrap();
        let mut profiles = ProfilesByItem::new();
        profiles.insert(
            "Q19897578".into(),
            vec![Profile {
                service: "youtube-handle".into(),
                id: "MrBeast".into(),
            }],
        );
        let mut websites = WebsitesByItem::new();
        let json = answer(&[
            (
                "item",
                "http://www.wikidata.org/entity/Q28404534",
                "url",
                "https://music.youtube.com/",
            ),
            // MrBeast's own front page: no website to link.
            (
                "item",
                "http://www.wikidata.org/entity/Q19897578",
                "url",
                "https://mrbeast.store/collections",
            ),
            (
                "item",
                "http://www.wikidata.org/entity/Q19897578",
                "url",
                "https://www.mrbeast.com/",
            ),
        ]);
        assert_eq!(add_websites_page(&mut websites, &json).unwrap(), 3);
        let added = add_profiles_to_file(&path, &profiles, &websites).unwrap();
        assert_eq!(
            added,
            AddedProfiles {
                articles: 3,
                with_profiles: 1,
                profiles: 1,
                websites: 1,
            }
        );
        let back = read_articles(open_maybe_gz(&path).unwrap(), 10).unwrap();
        assert_eq!(back[0].profiles, profiles["Q19897578"]);
        assert!(back[1].profiles.is_empty());
        assert_eq!(back[0].website, None);
        assert_eq!(
            back[2].website.as_deref(),
            Some("https://music.youtube.com/")
        );
        // Again, with none: the old ones go.
        add_profiles_to_file(&path, &ProfilesByItem::new(), &WebsitesByItem::new()).unwrap();
        let back = read_articles(open_maybe_gz(&path).unwrap(), 10).unwrap();
        assert!(back[0].profiles.is_empty());
        assert_eq!(back[2].website, None);
    }
}
