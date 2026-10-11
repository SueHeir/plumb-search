//! Select a site's explicit task section without claiming to have read its body.

use std::borrow::Cow;

use plumb_core::{normalize_text, registrable_domain, KeyPage, Operators};
use plumb_index::pages::{PlacedPage, MIN_PARTIAL_SCORE};
use plumb_index::Hit;
use serde::Serialize;
use url::Url;

/// Only source navigation is known. The site's title and description remain
/// site metadata; the anchor label is not a fetched document title or excerpt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct NavigationDestination {
    pub source: &'static str,
    pub label: String,
    pub url: String,
    pub homepage_url: String,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Tasks {
    dining: bool,
    shopping: bool,
}

impl Tasks {
    fn add(&mut self, word: &str) -> bool {
        match word {
            "dine" | "dining" | "restaurant" | "restaurants" => self.dining = true,
            "shop" | "shops" | "shopping" | "store" | "stores" => self.shopping = true,
            _ => return false,
        }
        true
    }

    fn any(self) -> bool {
        self.dining || self.shopping
    }

    fn covers(self, asked: Self) -> bool {
        (!asked.dining || self.dining) && (!asked.shopping || self.shopping)
    }
}

/// A short section label, with no extra content words: "Dine & Shop" is
/// navigation, while "Dining room furniture" is a different task.
fn section_tasks(label: &str) -> Option<Tasks> {
    let text = normalize_text(label);
    let mut tasks = Tasks::default();
    for word in text.split_whitespace() {
        if !tasks.add(word) && word != "and" {
            return None;
        }
    }
    tasks.any().then_some(tasks)
}

/// Fail closed before URL parsing can normalize dot segments or backslashes.
fn clean_address(raw: &str) -> bool {
    if raw
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return false;
    }
    let bytes = raw.as_bytes();
    for (at, byte) in bytes.iter().enumerate() {
        if *byte == b'%' {
            let Some(pair) = bytes.get(at + 1..at + 3) else {
                return false;
            };
            let Some(high) = (pair[0] as char).to_digit(16) else {
                return false;
            };
            let Some(low) = (pair[1] as char).to_digit(16) else {
                return false;
            };
            let decoded = (high * 16 + low) as u8;
            if decoded.is_ascii_control()
                || decoded.is_ascii_whitespace()
                || matches!(decoded, b'.' | b'/' | b'\\')
            {
                return false;
            }
        }
    }
    !raw.split('/').any(|part| matches!(part, "." | ".."))
}

fn section_url(page: &KeyPage, domain: &str, asked: Tasks) -> Option<String> {
    if !page.is_valid_for(domain)
        || !section_tasks(&page.label)?.covers(asked)
        || !clean_address(&page.url)
    {
        return None;
    }
    let url = Url::parse(&page.url).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || registrable_domain(url.host_str()?).as_deref() != Some(domain)
    {
        return None;
    }
    // A clean section URL corroborates its label. Filtered views and unrelated
    // action paths do not become main destinations from a navigation label.
    let section = url.path().trim_end_matches('/').rsplit('/').next()?;
    if !section_tasks(section)?.covers(asked) {
        return None;
    }
    Some(url.to_string())
}

fn asked_tasks(query: &str, site: &Hit) -> Option<Tasks> {
    if Operators::parse(query).any() || registrable_domain(query).is_some() {
        return None;
    }
    let normalized = normalize_text(query);
    let words: Vec<_> = normalized.split_whitespace().collect();
    // Explicit site/homepage navigation retains the official homepage.
    if words
        .iter()
        .any(|word| matches!(*word, "homepage" | "website"))
        || words
            .windows(2)
            .any(|pair| matches!(pair, ["home", "page"] | ["official", "site"]))
    {
        return None;
    }
    let label = site.domain.split('.').next()?.replace('-', "");
    if label.len() < 6 {
        return None;
    }
    // Require a complete multiword site identity, not one shared word, before
    // substituting an internal destination for a site's established URL.
    let identity = (0..words.len()).find_map(|start| {
        (start + 2..=words.len())
            .find(|&end| words[start..end].concat() == label)
            .map(|end| start..end)
    })?;
    let title = normalize_text(site.title.as_deref()?);
    let identity_name = words[identity.clone()].join(" ");
    if !format!(" {title} ").contains(&format!(" {identity_name} "))
        || words
            .iter()
            .any(|word| matches!(*word, "not" | "without" | "except"))
    {
        return None;
    }
    let mut asked = Tasks::default();
    for (at, word) in words.iter().enumerate() {
        if !identity.contains(&at) {
            asked.add(word);
        }
    }
    asked.any().then_some(asked)
}

pub(crate) fn site_destination<'a>(
    query: &str,
    site: &'a Hit,
    pages: &[PlacedPage],
) -> (Cow<'a, Hit>, Option<NavigationDestination>) {
    let select = || {
        let asked = asked_tasks(query, site)?;
        let homepage = Url::parse(&site.url).ok()?;
        if !clean_address(&site.url)
            || !matches!(homepage.scheme(), "http" | "https")
            || !homepage.username().is_empty()
            || homepage.password().is_some()
            || homepage.port().is_some()
            || registrable_domain(homepage.host_str()?).as_deref() != Some(site.domain.as_str())
            || !plumb_core::is_homepage_path(homepage.path())
            || homepage.query().is_some()
            || homepage.fragment().is_some()
        {
            return None;
        }
        // This is a fallback for link-only knowledge. Preserve stronger
        // retrieved pages and their existing ordering, sources and anchors.
        if pages.iter().any(|page| {
            (page.hit.named || page.hit.whole || page.hit.score >= MIN_PARTIAL_SCORE)
                && registrable_domain(&page.hit.page.url).as_deref() == Some(site.domain.as_str())
                && Url::parse(&page.hit.page.url)
                    .is_ok_and(|url| !plumb_core::is_homepage_path(url.path()))
        }) {
            return None;
        }
        let mut selected: Option<NavigationDestination> = None;
        for page in &site.key_pages {
            let Some(url) = section_url(page, &site.domain, asked) else {
                continue;
            };
            match &selected {
                Some(found) if found.url != url => return None,
                Some(_) => {}
                None => {
                    selected = Some(NavigationDestination {
                        source: "site_navigation",
                        label: page.label.clone(),
                        url,
                        homepage_url: site.url.clone(),
                    })
                }
            }
        }
        selected
    };
    match select() {
        Some(navigation) => {
            let mut selected = site.clone();
            selected.url.clone_from(&navigation.url);
            (Cow::Owned(selected), Some(navigation))
        }
        None => (Cow::Borrowed(site), None),
    }
}

#[cfg(test)]
mod tests;
