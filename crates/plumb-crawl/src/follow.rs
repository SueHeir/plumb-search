//! Following a site's links toward the pages a node's searches want.
//!
//! A search that names a site and more ("who dengue", "irs 401k limits")
//! wants one of the site's inner pages, and a node holds homepages. So
//! [`follow_links`] looks for the page the way a person would: from the
//! site's homepage, or from links found on an earlier visit, it fetches
//! the links whose text or address has the most of the search's words,
//! best first, and keeps the pages whose title, description, headings or
//! address have them. Each page fetched adds its links to the choice, so a
//! search leads a few hops into the site. When the homepage links to
//! nothing with the words, the menu links that lead to a list of the
//! site's pages ("Health topics", "A-Z") are tried. The links with the
//! words left unfetched come back as leads for next time, and so do the
//! links a page found makes to other sites.
//!
//! Every page is fetched as a homepage is ([`Visit`]): robots.txt asked,
//! the per-host delay kept, redirects followed only on the same site, no
//! private addresses. A caller's check (the node's denylist) can leave out
//! addresses besides: they are not fetched, and a page that redirects to
//! one is neither kept nor followed.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use futures::stream::{self, StreamExt};
use plumb_core::{collapse_whitespace, is_function_word, truncate_chars};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{build_client, http_url, CrawlConfig, CrawlOutcome, PageMeta, Visit};

/// Most links followed from the homepage to a page.
pub const MAX_DEPTH: u8 = 4;
/// Most leads a visit gives back.
pub const MAX_LEADS: usize = 50;
/// Most links to other sites a visit gives back.
const MAX_ELSEWHERE: usize = 5;
/// Longest link text kept of a lead, in characters.
const MAX_LABEL_CHARS: usize = 100;
/// Longest address followed, in bytes.
const MAX_URL_BYTES: usize = 1_000;
/// Longest title kept of a page found, in characters.
const MAX_TITLE_CHARS: usize = 200;
/// Longest description kept, in characters.
const MAX_DESCRIPTION_CHARS: usize = 300;
/// The homepage's place among the links to follow: after any link with a
/// third or more of a search's words.
const HOMEPAGE_SHARE: f32 = 0.3;
/// A menu link to a list of the site's pages, followed when the homepage
/// links to nothing with the words.
const HUB_SHARE: f32 = 0.1;
/// Menu links to lists of pages tried at most.
const MAX_HUBS: usize = 3;
/// Words of a menu link to a list of the site's pages, as [`match_words`]
/// gives them.
const HUB_WORDS: &[&str] = &[
    "topic",
    "index",
    "browse",
    "directory",
    "explore",
    "library",
    "resource",
    "guide",
    "categor",
    "subject",
];
/// What a hop costs a link's share, so of two links alike the nearer is
/// taken.
const DEPTH_COST: f32 = 0.01;
/// Longest a visit to one site goes on.
const VISIT_TIME: Duration = Duration::from_secs(5 * 60);

/// A site to follow links on, and what for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FollowTarget {
    /// Registrable domain of the site.
    pub domain: String,
    /// Its homepage, where a visit starts.
    pub homepage: String,
    /// Links found before ([`Followed::leads`]), taken first when they have
    /// the words.
    pub leads: Vec<Lead>,
    /// The words of each search that wants a page of the site, as
    /// [`match_words`] gives them.
    pub wants: Vec<Vec<String>>,
    /// Most pages fetched.
    pub max_fetches: usize,
}

/// A link to follow.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lead {
    pub url: String,
    /// Its text where it was found.
    pub label: String,
    /// Links followed from a homepage to reach it.
    pub depth: u8,
}

/// A page with the words a search wanted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WantedPage {
    /// Where it was, after redirects.
    pub url: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Links followed from the homepage to reach it.
    pub depth: u8,
}

/// What a visit found on one site.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Followed {
    pub domain: String,
    /// Each page with the words, and which of the target's wants it has.
    pub pages: Vec<(WantedPage, usize)>,
    /// Links on the site with words of a want that were not fetched, best
    /// first, at most [`MAX_LEADS`].
    pub leads: Vec<Lead>,
    /// Links from the pages found to other sites with enough of a want's
    /// words, and which want, at most [`MAX_ELSEWHERE`].
    pub elsewhere: Vec<(Lead, usize)>,
    /// Pages asked for.
    pub fetched: usize,
}

/// The words of `text` as links and pages are matched by them: lowercase,
/// two characters or more, without function words, and without a plural's
/// "s" ("Fact sheets" has "fact" and "sheet").
pub fn match_words(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|word| word.chars().count() > 1 && !is_function_word(word))
        .map(stem)
        .collect()
}

/// `word` without a plural's "s": "sheets" and "policies" are "sheet"
/// and "policy", but "class", "status" and "analysis" stay.
fn stem(word: String) -> String {
    if word.len() <= 4 || !word.is_ascii() || ["ss", "us", "is"].iter().any(|e| word.ends_with(e)) {
        return word;
    }
    if let Some(stem) = word.strip_suffix("ies") {
        return format!("{stem}y");
    }
    match word.strip_suffix('s') {
        Some(stem) => stem.to_string(),
        None => word,
    }
}

/// The words of `url`'s path: "dengue" and "fact" of
/// `/news-room/fact-sheets/detail/dengue`.
fn path_words(url: &str) -> HashSet<String> {
    Url::parse(url)
        .map(|url| match_words(url.path()))
        .unwrap_or_default()
}

/// The largest share of a want's words that `words` has.
fn best_share(wants: &[HashSet<String>], words: &HashSet<String>) -> f32 {
    wants
        .iter()
        .map(|want| want.intersection(words).count() as f32 / want.len().max(1) as f32)
        .fold(0.0, f32::max)
}

/// Which of `wants` `words` has enough of: all of one or two words, three
/// in five of more. The first with the largest share.
fn has_enough(wants: &[HashSet<String>], words: &HashSet<String>) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (i, want) in wants.iter().enumerate() {
        let have = want.intersection(words).count();
        let needed = if want.len() <= 2 {
            want.len()
        } else {
            (want.len() * 3).div_ceil(5)
        };
        if have >= needed.max(1) && best.is_none_or(|(_, most)| have * 1000 / want.len() > most) {
            best = Some((i, have * 1000 / want.len()));
        }
    }
    best.map(|(i, _)| i)
}

/// A link's share of a want's words, by its text and path.
fn link_share(wants: &[HashSet<String>], label: &str, url: &str) -> f32 {
    let mut words = match_words(label);
    words.extend(path_words(url));
    best_share(wants, &words)
}

/// `meta`, the page at `url`, as a page found when it has enough of a
/// want's words in its title, description, headings or address.
fn wanted_page(
    wants: &[HashSet<String>],
    url: &str,
    meta: &PageMeta,
    depth: u8,
) -> Option<(WantedPage, usize)> {
    let title = collapse_whitespace(meta.title.as_deref()?);
    if title.is_empty() {
        return None;
    }
    let mut words = match_words(&title);
    words.extend(match_words(meta.description.as_deref().unwrap_or("")));
    for heading in &meta.headings {
        words.extend(match_words(heading));
    }
    words.extend(path_words(url));
    let want = has_enough(wants, &words)?;
    let page = WantedPage {
        url: url.to_string(),
        title: truncate_chars(&title, MAX_TITLE_CHARS).to_string(),
        description: meta
            .description
            .as_deref()
            .map(collapse_whitespace)
            .filter(|text| !text.is_empty())
            .map(|text| truncate_chars(&text, MAX_DESCRIPTION_CHARS).to_string()),
        language: meta.language.clone(),
        depth,
    };
    Some((page, want))
}

/// Visits each of `targets` (see the module's notes), a few sites at once
/// (`cfg.concurrency`), each page fetched within `cfg.target_deadline`,
/// and none whose address `allowed` refuses. Must run inside a Tokio
/// runtime.
pub async fn follow_links(
    targets: Vec<FollowTarget>,
    cfg: &CrawlConfig,
    allowed: &(dyn Fn(&str) -> bool + Sync),
) -> Vec<Followed> {
    let cfg = CrawlConfig {
        fetch_icons: false,
        ..cfg.clone()
    };
    let Ok(client) = build_client(&cfg) else {
        return Vec::new();
    };
    stream::iter(targets)
        .map(|target| follow(&client, &cfg, target, allowed))
        .buffer_unordered(cfg.concurrency.max(1))
        .collect()
        .await
}

/// One site's visit.
async fn follow(
    client: &Client,
    cfg: &CrawlConfig,
    target: FollowTarget,
    allowed: &(dyn Fn(&str) -> bool + Sync),
) -> Followed {
    let started = Instant::now();
    let wants: Vec<HashSet<String>> = target
        .wants
        .iter()
        .map(|want| want.iter().cloned().collect::<HashSet<_>>())
        .filter(|want| !want.is_empty())
        .collect();
    let mut followed = Followed {
        domain: target.domain.clone(),
        ..Followed::default()
    };
    if wants.is_empty() {
        return followed;
    }
    let mut queue: Vec<(f32, Lead)> = target
        .leads
        .into_iter()
        .map(|lead| (link_share(&wants, &lead.label, &lead.url), lead))
        .filter(|(share, _)| *share > 0.0)
        .collect();
    queue.push((
        HOMEPAGE_SHARE,
        Lead {
            url: target.homepage.clone(),
            ..Lead::default()
        },
    ));
    let mut seen = HashSet::new();
    let mut visit = Visit::new(client, cfg);
    while followed.fetched < target.max_fetches && started.elapsed() < VISIT_TIME {
        // The best link, the first of equals in page order.
        let Some(best) =
            (0..queue.len()).reduce(|a, b| if queue[b].0 > queue[a].0 { b } else { a })
        else {
            break;
        };
        let (_, lead) = queue.remove(best);
        let Ok(url) = http_url(&lead.url) else {
            continue;
        };
        if !seen.insert(without_fragment(&url)) || !allowed(url.as_str()) {
            continue;
        }
        followed.fetched += 1;
        let fetch = visit.fetch_homepage(&target.domain, &url);
        let Ok(CrawlOutcome::Fetched(page)) =
            tokio::time::timeout(cfg.target_deadline, fetch).await
        else {
            continue;
        };
        if let Ok(at) = Url::parse(&page.final_url) {
            seen.insert(without_fragment(&at));
        }
        if !allowed(&page.final_url) {
            continue;
        }
        let found = (lead.depth > 0)
            .then(|| wanted_page(&wants, &page.final_url, &page.meta, lead.depth))
            .flatten();
        if let Some(&(_, want)) = found.as_ref() {
            // Links to other sites with enough of the same words.
            for link in &page.meta.links {
                let mut words = match_words(&link.text);
                words.extend(path_words(&link.url));
                if followed.elsewhere.len() < MAX_ELSEWHERE
                    && has_enough(&wants[want..=want], &words).is_some()
                {
                    let lead = new_lead(&link.url, &link.text, 1);
                    followed.elsewhere.extend(lead.map(|lead| (lead, want)));
                }
            }
        }
        followed.pages.extend(found);
        if lead.depth < MAX_DEPTH {
            queue.extend(links_to_follow(&wants, &page.meta, lead.depth + 1));
        }
    }
    followed.leads = leads_left(queue, &seen);
    followed
}

/// The links on a page to follow, `depth` hops from the homepage, with
/// their share of a want's words; when the homepage (`depth` 1) has none
/// with the words, its menu links to lists of the site's pages.
fn links_to_follow(wants: &[HashSet<String>], meta: &PageMeta, depth: u8) -> Vec<(f32, Lead)> {
    let cost = DEPTH_COST * f32::from(depth);
    let matching: Vec<(f32, Lead)> = meta
        .own_links
        .iter()
        .map(|link| (link_share(wants, &link.label, &link.url), link))
        .filter(|(share, _)| *share > 0.0)
        .filter_map(|(share, link)| Some((share - cost, new_lead(&link.url, &link.label, depth)?)))
        .collect();
    if !matching.is_empty() || depth > 1 {
        return matching;
    }
    meta.own_links
        .iter()
        .filter(|link| link.in_nav && is_hub(&link.label))
        .filter_map(|link| Some((HUB_SHARE, new_lead(&link.url, &link.label, depth)?)))
        .take(MAX_HUBS)
        .collect()
}

/// A link to follow, its text cut short; none when its address is too
/// long to be a page's.
fn new_lead(url: &str, label: &str, depth: u8) -> Option<Lead> {
    (url.len() <= MAX_URL_BYTES).then(|| Lead {
        url: url.to_string(),
        label: truncate_chars(label, MAX_LABEL_CHARS).to_string(),
        depth,
    })
}

/// Whether a menu link's text says it leads to a list of the site's pages.
fn is_hub(label: &str) -> bool {
    let lower = label.to_lowercase();
    lower.contains("a-z")
        || lower.contains("a to z")
        || match_words(&lower)
            .iter()
            .any(|word| HUB_WORDS.iter().any(|hub| word.starts_with(hub)))
}

/// The links left with words of a want, best first, each once, at most
/// [`MAX_LEADS`].
fn leads_left(mut queue: Vec<(f32, Lead)>, seen: &HashSet<String>) -> Vec<Lead> {
    queue.retain(|(share, _)| *share > HUB_SHARE);
    queue.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut kept = HashSet::new();
    queue
        .into_iter()
        .map(|(_, lead)| lead)
        .filter(|lead| {
            let key = Url::parse(&lead.url)
                .map(|url| without_fragment(&url))
                .unwrap_or_default();
            !seen.contains(&key) && kept.insert(key)
        })
        .take(MAX_LEADS)
        .collect()
}

fn without_fragment(url: &Url) -> String {
    let mut url = url.clone();
    url.set_fragment(None);
    url.into()
}

#[cfg(test)]
mod tests;
