//! Evidence about the entity an identity tool was asked for. Search naming
//! and spelling signals alone do not establish that a site belongs to it.

use super::*;

pub(super) struct Intent {
    pub entity: String,
    pub words: Vec<String>,
    pub country: Option<&'static str>,
    country_name: Option<String>,
}

impl Intent {
    pub fn of(name: &str) -> Self {
        let mut entity = bare_name(name).unwrap_or_else(|| name.trim().to_lowercase());
        let parts: Vec<_> = entity.split_whitespace().collect();
        let jurisdiction = (1..parts.len()).find_map(|at| {
            // A country's name can be part of a brand: Bank of America.
            if matches!(parts[at - 1], "of" | "de" | "del" | "di") {
                return None;
            }
            let suffix = parts[at..].join(" ");
            let country = if suffix == "méxico" {
                Some("MX")
            } else {
                plumb_core::country_of_name(&suffix)
            }?;
            Some((parts[..at].join(" "), suffix, country))
        });
        let country_name = jurisdiction.as_ref().map(|(_, suffix, _)| suffix.clone());
        let country = jurisdiction.map(|(prefix, _, country)| {
            entity = prefix;
            country
        });
        // Category/task suffixes qualify the entity before them. They are
        // not independent evidence for a site called Converter or Vector.
        for task in ["currency converter", "vector database", "database", "email"] {
            if let Some(prefix) = entity.strip_suffix(&format!(" {task}")) {
                if !prefix.trim().is_empty() {
                    entity = prefix.trim().to_string();
                    break;
                }
            }
        }
        Self {
            words: name_words(&entity),
            entity,
            country,
            country_name,
        }
    }

    pub fn matches_site(&self, hit: &Hit) -> bool {
        self.matches(&hit.domain, hit.title.as_deref().unwrap_or(""))
            && self.matches_country(&hit.domain, hit.country.as_deref())
            && self.shows_jurisdiction(
                &hit.domain,
                hit.title.as_deref().unwrap_or(""),
                hit.country.as_deref(),
            )
    }

    pub fn matches_article(&self, page: &Page) -> bool {
        self.matches(page.website.as_deref().unwrap_or(""), &page.title)
            && self.matches_country(page.site.as_deref().unwrap_or(""), None)
            && self.shows_jurisdiction(page.site.as_deref().unwrap_or(""), &page.title, None)
    }

    fn shows_jurisdiction(&self, address: &str, title: &str, country: Option<&str>) -> bool {
        // An unknown/global country does not prove a local entity. Keep
        // matching brands whose own title carries the country (Air France).
        country
            .or_else(|| plumb_core::tld_country(address))
            .is_some()
            || self.country_name.as_ref().is_none_or(|name| {
                plumb_core::normalize_text(title).contains(&plumb_core::normalize_text(name))
            })
    }

    fn matches(&self, address: &str, title: &str) -> bool {
        let host = host_of(address).unwrap_or_default();
        let (unicode, _) = idna::domain_to_unicode(&host);
        let host_words = plumb_core::normalize_text(&unicode);
        let title_words = plumb_core::normalize_text(title);
        // A joined/hyphenated full name (oldnavy.com, life-wiki.com) is
        // useful evidence. A substring of a different brand (ChromaKey)
        // is not. Prefix domains such as trychroma.com need the entity's
        // actual name in the title or authoritative metadata.
        let entity_words = plumb_core::normalize_text(&self.entity);
        let domain_names_entity = plumb_core::normalize_text(&domain_label(&host)).replace(' ', "")
            == entity_words.replace(' ', "");
        if self.words.is_empty() {
            // Single-letter brands and names normally treated as filler
            // still need literal evidence (X, the R Project, app.com).
            return !entity_words.is_empty()
                && (domain_names_entity
                    || title_words == entity_words
                    || (entity_words.chars().count() == 1
                        && title_words
                            .split_whitespace()
                            .any(|word| word == entity_words)));
        }
        self.words.iter().all(|word| {
            title_words.split_whitespace().any(|shown| shown == word)
                || host_words.split_whitespace().any(|shown| shown == word)
                || domain_names_entity
        })
    }

    pub fn matches_country(&self, domain: &str, country: Option<&str>) -> bool {
        self.country.is_none_or(|asked| {
            country
                .or_else(|| plumb_core::tld_country(domain))
                .is_none_or(|shown| shown.eq_ignore_ascii_case(asked))
        })
    }
}

/// A reviewed owner reference for an exact host. There is deliberately no
/// same-label/different-TLD inference and no inheritance to child hosts.
/// Owner references establish affiliation; a redirect only records where
/// a verified alias currently leads. No network access is needed at runtime.
pub(super) fn affiliation(host: &str) -> Option<Value> {
    let (owner, destination, sources, redirect) = match host {
        "console.hetzner.cloud" => (
            "hetzner.com",
            "https://console.hetzner.com/",
            vec![
                "https://status.hetzner.com/incident/62839f8e-073a-4159-87a1-b05d093fe689",
                "https://www.hetzner.com/cloud/",
            ],
            Some("https://console.hetzner.cloud/"),
        ),
        "console.hetzner.com" => (
            "hetzner.com",
            "https://console.hetzner.com/",
            vec!["https://www.hetzner.com/cloud/"],
            None,
        ),
        "api.semanticscholar.org" => (
            "semanticscholar.org",
            "https://api.semanticscholar.org/",
            vec!["https://webflow.semanticscholar.org/product/api/tutorial"],
            None,
        ),
        _ => return None,
    };
    Some(json!({
        "host": host,
        "owner": owner,
        "official_url": destination,
        "sources": sources,
        "verified_at": "2026-10-09",
        "kind": if redirect.is_some() { "verified_migration" } else { "owner_reference" },
        "observed_redirect": redirect.map(|from| json!({ "from": from, "to": destination })),
    }))
}
