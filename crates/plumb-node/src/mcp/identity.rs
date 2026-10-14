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
    /// The entity `name` names, with a country at its end taken as where
    /// it is wanted: "SAT México" is SAT in Mexico.
    pub fn of(name: &str) -> Self {
        let whole = Self::whole(name);
        let parts: Vec<_> = whole.entity.split_whitespace().collect();
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
        match jurisdiction {
            Some((entity, country_name, country)) => Self {
                country: Some(country),
                country_name: Some(country_name),
                ..Self::named(entity)
            },
            None => whole,
        }
    }

    /// The entity all of `name` names, a country at its end included: New
    /// Mexico, not "new" in Mexico.
    pub fn whole(name: &str) -> Self {
        Self::named(bare_name(name).unwrap_or_else(|| name.trim().to_lowercase()))
    }

    fn named(entity: String) -> Self {
        Self {
            // Categories and resources distinguish the requested entity.
            // Keep every substantive word after documented docs/login intent
            // parsing; no category suffix can silently manufacture a match.
            words: plumb_core::normalize_text(&entity)
                .split_whitespace()
                .filter(|word| !plumb_core::is_function_word(word))
                .map(str::to_string)
                .collect(),
            entity,
            country: None,
            country_name: None,
        }
    }

    /// Whether Wikidata knows something by exactly this name: an article
    /// named by it, or an official site titled it.
    pub fn is_known_name(&self, results: &SearchResults) -> bool {
        let entity = plumb_core::normalize_text(&self.entity);
        results.pages.iter().any(|placed| {
            placed.hit.named
                && super::official_site::is_article(&placed.hit.page)
                && plumb_core::normalize_text(&placed.hit.page.title) == entity
        }) || results.hits.iter().any(|hit| {
            hit.official
                && hit
                    .title
                    .as_deref()
                    .is_some_and(|title| plumb_core::normalize_text(title) == entity)
        })
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
        let title = plumb_core::normalize_text(&page.title);
        // An article's website address alone cannot name the article's
        // entity. Its title must show the identity, or expand its acronym
        // (USNO -> United States Naval Observatory).
        let title_names = self
            .words
            .iter()
            .any(|word| title.split_whitespace().any(|shown| shown == word))
            || self.expands_acronym(&title);
        // A person's page on a bigger site keeps no site (plumb-index's
        // drop_borrowed_site), so their country shows in their website.
        let site = super::official_site::website_domain(page).unwrap_or_default();
        title_names
            && self.matches(page.website.as_deref().unwrap_or(""), &page.title)
            && self.matches_country(&site, None)
            && self.shows_jurisdiction(&site, &page.title, None)
    }

    /// A name match is retrieval evidence, not ownership. A bare domain
    /// namesake or a copied/generated title must remain a suggestion.
    pub fn supports_site(&self, hit: &Hit) -> bool {
        if !self.matches_site(hit)
            || registrable_domain(&hit.url) != registrable_domain(&hit.domain)
        {
            return false;
        }
        let entity = plumb_core::normalize_text(&self.entity);
        let host = host_of(&hit.domain).unwrap_or_default();
        // An explicitly requested address identifies the destination
        // itself; it does not require inferring an owner from a brand.
        if self.entity.contains('.')
            && host_of(&self.entity).is_some_and(|asked| {
                asked == host || host_of(&hit.url).as_deref() == Some(asked.as_str())
            })
        {
            return true;
        }
        let title = plumb_core::normalize_text(hit.title.as_deref().unwrap_or(""));
        let title_shows = self.text_shows_entity(&title) || self.expands_acronym(&title);
        let domain_names = plumb_core::normalize_text(&domain_label(&host)).replace(' ', "")
            == entity.replace(' ', "");
        // Wikidata's site-level flag is useful only after the whole entity
        // is bound to the site; it is not a license for partial names.
        if hit.official && title_shows {
            return true;
        }
        // Preserve a jurisdiction-qualified entity when its full domain
        // name and its own descriptive text corroborate that identity.
        // This stays low confidence without an owner reference.
        if self.country.is_some()
            && domain_names
            && self.text_shows_entity(&plumb_core::normalize_text(
                hit.description.as_deref().unwrap_or(""),
            ))
        {
            return true;
        }
        // A site's own title and complete domain name can corroborate one
        // another. Every substantive identity word must appear in the
        // domain, so Yosemite is not Yosemite National Park. A complete
        // single name also covers project labels like trychroma, but never
        // turns the label alone into ownership evidence.
        let label = plumb_core::normalize_text(&domain_label(&host)).replace(' ', "");
        let complete_label = domain_names
            || (!self.words.is_empty() && self.words.iter().all(|word| label.contains(word)));
        title_shows && complete_label
    }

    fn text_shows_entity(&self, text: &str) -> bool {
        if self.words.is_empty() {
            let entity = plumb_core::normalize_text(&self.entity);
            return !entity.is_empty()
                && (text == entity
                    || (entity.chars().count() == 1
                        && text.split_whitespace().any(|word| word == entity)));
        }
        self.words
            .iter()
            .all(|word| text.split_whitespace().any(|shown| shown == word))
            || (self.words.len() > 1
                && text
                    .split_whitespace()
                    .any(|word| word == self.words.concat()))
    }

    fn expands_acronym(&self, text: &str) -> bool {
        let initials: String = text
            .split_whitespace()
            .filter(|word| !plumb_core::is_function_word(word))
            .filter_map(|word| word.chars().next())
            .collect();
        self.words.len() == 1 && (3..=8).contains(&self.words[0].len()) && initials == self.words[0]
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
