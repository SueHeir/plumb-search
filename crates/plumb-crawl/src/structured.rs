//! The names a homepage gives its own site in schema.org JSON-LD
//! (`<script type="application/ld+json">`): the `name`, `alternateName`
//! and `legalName` of its `WebSite` and `Organization` items.
//!
//! Many homepages say little in their visible text (a menu, a banner, a
//! cookie notice) but describe the site in JSON-LD for search engines, and
//! that is where the site's own names are written out, the short form and
//! the legal one included ("BofA", "Bank of America Corporation"). They
//! become aliases of the site, which crawlers must agree on like any other.
//!
//! Only items at the top of a block (or in its `@graph`) count, and only
//! those about this site: an item whose `url` is on another domain (the
//! publisher of an article, a partner) is skipped.

use plumb_core::{collapse_whitespace, registrable_domain};
use serde_json::Value;

/// Most JSON-LD blocks read from a page.
pub(crate) const MAX_BLOCKS: usize = 4;

/// Longest JSON-LD block read, in bytes; a longer one is skipped whole.
pub(crate) const MAX_BLOCK_BYTES: usize = 64 * 1024;

/// Most names taken from a page.
pub(crate) const MAX_NAMES: usize = 4;

/// Longest name kept, in characters: longer ones are slogans or titles.
const MAX_NAME_CHARS: usize = 60;

/// Most items read from a block (top level and `@graph` together).
const MAX_ITEMS: usize = 64;

/// Types whose names are names of the site itself.
const SITE_TYPES: &[&str] = &[
    "WebSite",
    "Organization",
    "Corporation",
    "Brand",
    "OnlineStore",
    "OnlineBusiness",
    "LocalBusiness",
    "NewsMediaOrganization",
    "EducationalOrganization",
    "CollegeOrUniversity",
    "GovernmentOrganization",
    "NGO",
    "BankOrCreditUnion",
    "FinancialService",
    "Airline",
    "Store",
    "Project",
    "ResearchOrganization",
    "SportsOrganization",
    "MedicalOrganization",
];

/// The site's own names in the JSON-LD `blocks` of a page on `own_domain`,
/// each once (case aside), in page order, at most [`MAX_NAMES`].
pub(crate) fn site_names(blocks: &[String], own_domain: Option<&str>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for block in blocks.iter().take(MAX_BLOCKS) {
        if block.len() > MAX_BLOCK_BYTES {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(block.trim()) else {
            continue;
        };
        for item in items(&value).into_iter().take(MAX_ITEMS) {
            if !is_site_item(item, own_domain) {
                continue;
            }
            for key in ["name", "alternateName", "legalName"] {
                for name in strings(item.get(key)) {
                    let Some(name) = clean_name(name) else {
                        continue;
                    };
                    if names.len() < MAX_NAMES
                        && !names.iter().any(|n| n.eq_ignore_ascii_case(&name))
                    {
                        names.push(name);
                    }
                }
            }
        }
    }
    names
}

/// The items at the top of a block: the block itself, the members of a
/// top-level array, and the members of any `@graph`.
fn items(value: &Value) -> Vec<&Value> {
    let tops: Vec<&Value> = match value {
        Value::Array(list) => list.iter().collect(),
        other => vec![other],
    };
    let mut out = Vec::new();
    for top in tops {
        if let Some(Value::Array(graph)) = top.get("@graph") {
            out.extend(graph.iter());
        }
        out.push(top);
    }
    out.retain(|item| item.is_object());
    out
}

/// Whether an item is one of [`SITE_TYPES`] and, when it gives a `url`,
/// that address is on `own_domain`.
fn is_site_item(item: &Value, own_domain: Option<&str>) -> bool {
    let typed = strings(item.get("@type")).into_iter().any(|kind| {
        let kind = kind.rsplit(['/', ':']).next().unwrap_or(kind);
        SITE_TYPES.contains(&kind)
    });
    if !typed {
        return false;
    }
    match strings(item.get("url")).first() {
        None => true,
        Some(url) => match (registrable_domain(url), own_domain) {
            (Some(domain), Some(own)) => domain == own,
            // A relative address ("/") is on this site.
            (None, _) => !url.contains("://"),
            (Some(_), None) => false,
        },
    }
}

/// A string value, or the strings of an array of them.
fn strings(value: Option<&Value>) -> Vec<&str> {
    match value {
        Some(Value::String(text)) => vec![text.as_str()],
        Some(Value::Array(list)) => list.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// A name with its spaces collapsed, or `None` when it does not read like
/// a name: empty, too long, a title with separators ("Acme | Home"), or an
/// address.
fn clean_name(name: &str) -> Option<String> {
    let name = collapse_whitespace(name);
    let chars = name.chars().count();
    if !(2..=MAX_NAME_CHARS).contains(&chars) {
        return None;
    }
    if name.contains(['|', '<', '>', '{', '}']) || name.contains(" - ") || name.contains("://") {
        return None;
    }
    if !name.chars().any(char::is_alphanumeric) {
        return None;
    }
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(json: &str) -> Vec<String> {
        site_names(&[json.to_string()], Some("example.com"))
    }

    #[test]
    fn reads_organization_and_website_names() {
        let json = r#"{
            "@context": "https://schema.org",
            "@graph": [
                {"@type": "WebSite", "url": "https://www.example.com/", "name": "Example",
                 "alternateName": ["EX", "Example Bank"]},
                {"@type": "Organization", "name": "Example Bank", "legalName": "Example Bank Corporation",
                 "url": "https://example.com"}
            ]
        }"#;
        assert_eq!(
            names(json),
            ["Example", "EX", "Example Bank", "Example Bank Corporation"]
        );
    }

    #[test]
    fn skips_items_about_other_sites_and_other_types() {
        let json = r#"[
            {"@type": "Organization", "name": "Partner Inc", "url": "https://partner.org"},
            {"@type": "Article", "name": "How we started"},
            {"@type": ["Corporation", "Thing"], "name": "Example Corp", "url": "/"},
            {"@type": "http://schema.org/Brand", "name": "Examplo"}
        ]"#;
        assert_eq!(names(json), ["Example Corp", "Examplo"]);
    }

    #[test]
    fn skips_titles_slogans_and_broken_json() {
        let json = r#"{"@type": "WebSite", "name": "Example | Home",
            "alternateName": ["Example - the best bank in the whole wide world", "Ex", "x"]}"#;
        assert_eq!(names(json), ["Ex"]);
        assert!(names("{\"@type\": \"WebSite\", \"name\": ").is_empty());
        assert!(names("<!-- {} -->").is_empty());
    }

    #[test]
    fn keeps_each_name_once_and_at_most_a_few() {
        let json = r#"{"@type": "Organization", "name": "Example",
            "alternateName": ["EXAMPLE", "A1", "B2", "C3", "D4", "E5"]}"#;
        assert_eq!(names(json), ["Example", "A1", "B2", "C3"]);
    }
}
