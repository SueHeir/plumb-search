//! Pages and the sites they are about: an untitled site takes its
//! article's title, and organizations and governments keep their sites first.

use anyhow::Result;
use tantivy::collector::TopDocs;
use tantivy::query::TermQuery;
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{TantivyDocument, Term};

use super::{Page, PageHit, PageSearcher, ALIAS_MATCH, POPULARITY_SCALE, TITLE_CANDIDATES};

/// The trailing qualifier in brackets that [`base_title`] leaves out:
/// `element` of `Mercury (element)`.
pub(crate) fn title_qualifier(title: &str) -> Option<&str> {
    let base = base_title(title);
    (base.len() < title.len()).then(|| &title[base.len() + 2..title.len() - 1])
}

/// `title` without a trailing qualifier in brackets: `Python (programming
/// language)` -> `Python`.
pub fn base_title(title: &str) -> &str {
    match title.rfind(" (") {
        Some(i) if title.ends_with(')') && i > 0 => &title[..i],
        _ => title,
    }
}

/// Shorter titles an article is searched for by: a work's title in a
/// series without the series' name ("Tears of the Kingdom" of "The Legend
/// of Zelda: Tears of the Kingdom", "The Phantom Menace" of "Star Wars:
/// Episode I – The Phantom Menace"), and its title or that one without a
/// leading "The" ("Dark Side of the Moon", "Phantom Menace"). Only those
/// of two words or more that are not stop words: "Mission: Impossible",
/// "Pokémon: The Movie" and "The Beatles" would say too little.
pub(crate) fn short_titles(title: &str) -> Vec<&str> {
    let base = base_title(title);
    let subtitle = base
        .rsplit_once(": ")
        .map(|(_, rest)| rest.rsplit_once(" – ").map_or(rest, |(_, last)| last));
    let words =
        |text: &str| crate::analysis::tokens(&crate::analysis::stemmed_analyzer(), text).len();
    [
        subtitle,
        base.strip_prefix("The "),
        subtitle.and_then(|subtitle| subtitle.strip_prefix("The ")),
    ]
    .into_iter()
    .flatten()
    .filter(|short| words(short) >= 2)
    .collect()
}

/// A title as people type it without the "'s" of a name before another
/// word: "Ohm law" for "Ohm's law", "Halley Comet" for "Halley's Comet".
/// `None` when it has no such word. A title that ends in one keeps it:
/// "mcdonald" is a surname as much as McDonald's.
pub(crate) fn without_possessive(title: &str) -> Option<String> {
    let bare = title.replace("'s ", " ").replace("’s ", " ");
    (bare != title).then_some(bare)
}

impl PageSearcher {
    /// Gives the sites among `sites` that have no title the title of the
    /// most read Wikipedia article about what each is the official website
    /// of, without its qualifier: "Notion" for notion.so, "Yelp" for
    /// yelp.com, whose homepages turn crawlers away. Only an article whose
    /// official website is the site's homepage ([`Page::website`]) and
    /// whose title the domain spells ([`site_spells`]) counts:
    /// "Schitt's Creek", whose website is a page on cbc.ca, is not what
    /// cbc.ca is, nor is "Snowflake ID", whose website is twitter.com, what
    /// twitter.com is. An official site whose title may be borrowed
    /// ([`may_be_borrowed`]) and names none of its articles takes such an
    /// article's title too: "New York Post" for nypost.com, not "California
    /// Post".
    pub fn title_untitled(&self, sites: &mut [crate::Hit]) -> Result<()> {
        let searcher = self.reader.searcher();
        for site in sites.iter_mut() {
            let untitled = site
                .title
                .as_deref()
                .is_none_or(|title| title.trim().is_empty());
            if !untitled && !may_be_borrowed(site) {
                continue;
            }
            let articles: Vec<String> = self
                .site_articles(&searcher, &site.domain)?
                .into_iter()
                .map(|(page, _)| page.title)
                .collect();
            let spelled = articles
                .iter()
                .find(|title| site_spells(&site.domain, title))
                .map(|title| base_title(title).trim());
            let title = if untitled {
                spelled
            } else {
                // A title that names what any article about the site is
                // stays ("Welcome to Steam" for steampowered.com, whose
                // articles are Valve's and Steam's).
                let shown = squash(site.title.as_deref().unwrap_or(""));
                let named = articles.iter().any(|title| {
                    let name = squash(base_title(title));
                    name.len() < 3 || shown.contains(&name)
                });
                spelled.filter(|_| !named)
            };
            if let Some(title) = title.filter(|title| !title.is_empty()) {
                site.title = Some(title.to_string());
            }
        }
        Ok(())
    }

    /// The most read Wikipedia articles, with their
    /// [`PageHit::popularity`], whose item's official website is
    /// `domain`'s front page ([`Page::website`]), most read first.
    fn site_articles(
        &self,
        searcher: &tantivy::Searcher,
        domain: &str,
    ) -> Result<Vec<(Page, f32)>> {
        let query = TermQuery::new(
            Term::from_field_text(self.fields.site, domain),
            IndexRecordOption::Basic,
        );
        let best = searcher.search(
            &query,
            &TopDocs::with_limit(TITLE_CANDIDATES)
                .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc),
        )?;
        let mut articles = Vec::new();
        for (popularity, address) in best {
            let document: TantivyDocument = searcher.doc(address)?;
            let Some(stored) = document
                .get_first(self.fields.page)
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let page: Page = serde_json::from_str(stored)?;
            if page.is_article() && page.website.is_none() {
                let popularity = popularity.unwrap_or(0) as f32 / POPULARITY_SCALE;
                articles.push((page, popularity));
            }
        }
        Ok(articles)
    }

    /// Puts first among `hits`, named, the article about the site that
    /// `query`, a web address, is the front page of: the most read one
    /// whose name the domain spells ([`site_spells`]). "x.com" is X, the
    /// social network, not "X.com (bank)", whose name was the address. An
    /// article Wikipedia calls by the address alone stays first
    /// ("nytimes.com", the newspaper's website), and an address with more
    /// than "www." before the domain is an inner site's ("mail.google.com"
    /// is not Google).
    pub(crate) fn name_typed_site(&self, query: &str, hits: &mut Vec<PageHit>) -> Result<()> {
        let Some(domain) = crate::names::typed_domain(query) else {
            return Ok(());
        };
        let typed = query.trim().to_lowercase();
        let called = |hit: &PageHit| hit.named && hit.page.title.to_lowercase() == domain;
        if typed.strip_prefix("www.").unwrap_or(&typed) != domain || hits.iter().any(called) {
            return Ok(());
        }
        let searcher = self.reader.searcher();
        let Some((page, popularity)) = self
            .site_articles(&searcher, &domain)?
            .into_iter()
            .find(|(page, _)| site_spells(&domain, &page.title))
        else {
            return Ok(());
        };
        let score = hits.iter().map(|hit| hit.score).fold(ALIAS_MATCH, f32::max);
        hits.retain(|hit| hit.page != page);
        hits.insert(
            0,
            PageHit {
                page,
                score,
                named: true,
                popularity,
                whole: false,
                learned: None,
                meant: false,
            },
        );
        Ok(())
    }
}

/// Words of a short description saying a page is about an organization,
/// a product or a service, which has a website of its own, rather than a
/// person, place, idea or work.
const ORGANIZATION_WORDS: &[&str] = &[
    "agency",
    "airline",
    "app",
    "application",
    "bank",
    "banking",
    "brand",
    "broker",
    "brokerage",
    "business",
    "card",
    "chain",
    "company",
    "conglomerate",
    "cooperative",
    "corporation",
    "exchange",
    "firm",
    "foundation",
    "framework",
    "institution",
    "insurer",
    "library",
    "manufacturer",
    "marketplace",
    "nonprofit",
    "organisation",
    "organization",
    "pharmacy",
    "platform",
    "program",
    "programme",
    "protocol",
    "provider",
    "retailer",
    "service",
    "software",
    "specification",
    "standard",
    "startup",
    "subsidiary",
    "union",
    "website",
];

/// Words of a short description saying a page is about an event, a
/// competition, a team or league that plays in one, or a venue: what has
/// a schedule, results or tickets.
const EVENT_WORDS: &[&str] = &[
    "arena",
    "award",
    "bowl",
    "carnival",
    "ceremony",
    "championship",
    "club",
    "competition",
    "conference",
    "contest",
    "convention",
    "cup",
    "election",
    "event",
    "expo",
    "fair",
    "festival",
    "franchise",
    "gala",
    "league",
    "marathon",
    "pageant",
    "parade",
    "playoff",
    "promotion",
    "race",
    "rally",
    "referendum",
    "regatta",
    "season",
    "stadium",
    "team",
    "tour",
    "tournament",
];

/// Whether Wikipedia's short description of a page says it is about an
/// organization, product or service ("American auto insurance company",
/// "Open-source software framework"), see [`ORGANIZATION_WORDS`].
pub(crate) fn describes_an_organization(description: Option<&str>) -> bool {
    describes_as(description, ORGANIZATION_WORDS)
}

/// Whether Wikipedia's short description of a page says it is about an
/// event, competition, team or venue ("European association football
/// tournament", "National Basketball Association team"), see
/// [`EVENT_WORDS`].
pub(crate) fn describes_an_event(description: Option<&str>) -> bool {
    describes_as(description, EVENT_WORDS)
}

/// Whether `description` has one of `words`, or its plural.
fn describes_as(description: Option<&str>, words: &[&str]) -> bool {
    description.is_some_and(|text| {
        text.split(|c: char| !c.is_alphanumeric())
            .map(str::to_lowercase)
            .any(|word| {
                let one = word.strip_suffix('s').unwrap_or(&word);
                words.contains(&word.as_str()) || words.contains(&one)
            })
    })
}

/// `text` lowercased with only its letters and digits.
pub(crate) fn squash(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether `domain` is a government's (`.gov`, `.gov.uk`, `.mil`).
pub(crate) fn is_government(domain: &str) -> bool {
    domain
        .split('.')
        .skip(1)
        .any(|label| label == "gov" || label == "mil")
}

/// Whether `domain`'s first label spells the page's title without its
/// qualifier, or that title's first words: cvs.com for "CVS Pharmacy",
/// capitalone.com for "Capital One", tauri.app for "Tauri (software
/// framework)", navyfederal.org for "Navy Federal Credit Union". Such a site is most likely what the page is about.
pub(crate) fn site_is_titled(domain: &str, title: &str) -> bool {
    let label = squash(domain.split('.').next().unwrap_or(""));
    if label.is_empty() {
        return false;
    }
    // The title's first words: "navyfederal" for "Navy Federal Credit
    // Union".
    let mut lead = String::new();
    for word in base_title(title).split_whitespace() {
        lead.push_str(&squash(word));
        if lead == label {
            return true;
        }
        if lead.len() >= label.len() {
            return false;
        }
    }
    false
}

/// Whether `domain` spells the page's title ([`site_is_titled`]) or
/// writes each of its words in full or by its first letter, small words
/// aside, in three letters or more: nba.com for "National Basketball
/// Association", nypost.com for "New York Post".
fn site_spells(domain: &str, title: &str) -> bool {
    let label = squash(domain.split('.').next().unwrap_or(""));
    let words: Vec<String> = base_title(title)
        .split_whitespace()
        .map(squash)
        .filter(|word| !word.is_empty())
        .collect();
    site_is_titled(domain, title) || label.len() >= 3 && abbreviates(&label, &words)
}

/// Whether `label` is `words`, each in full or by its first letter, the
/// small ones perhaps left out.
fn abbreviates(label: &str, words: &[String]) -> bool {
    let Some((word, rest)) = words.split_first() else {
        return label.is_empty();
    };
    let initial = word
        .chars()
        .next()
        .and_then(|first| label.strip_prefix(first));
    (["of", "and", "the", "for"].contains(&word.as_str()) && abbreviates(label, rest))
        || label
            .strip_prefix(word.as_str())
            .is_some_and(|left| abbreviates(left, rest))
        || initial.is_some_and(|left| abbreviates(left, rest))
}

/// Whether `site`'s title may be another page's: an official site whose
/// title has not its own name, as when its homepage sent the crawler on
/// to a section ("California Post" for nypost.com). Its article's title
/// says which ([`PageSearcher::title_untitled`]).
fn may_be_borrowed(site: &crate::Hit) -> bool {
    let label = squash(site.domain.split('.').next().unwrap_or(""));
    site.official
        && label.len() >= 3
        && site
            .title
            .as_deref()
            .is_some_and(|title| !squash(title).contains(&label))
}

/// What happens to a person that Wikipedia titles "Assassination of
/// Abraham Lincoln", "Wedding of Prince Harry and Meghan Markle".
const EVENTS: &[&str] = &[
    "abdication",
    "arrest",
    "assassination",
    "burial",
    "coronation",
    "death",
    "disappearance",
    "execution",
    "funeral",
    "impeachment",
    "inauguration",
    "kidnapping",
    "killing",
    "murder",
    "presidency",
    "resignation",
    "shooting",
    "trial",
    "wedding",
];

/// Whether `query` is a name and then what happened to them, which
/// `title` is the article on: "lincoln assassination" for
/// "Assassination of Abraham Lincoln", "george floyd murder" for "Murder
/// of George Floyd". Every word of the name has to be in the title.
pub(crate) fn event_of(title: &str, query: &str) -> bool {
    let words = |text: &str| -> Vec<String> {
        text.split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    let Some((event, whom)) = base_title(title).split_once(" of ") else {
        return false;
    };
    let query = words(query);
    let Some((asked, name)) = query.split_last() else {
        return false;
    };
    let whom = words(whom);
    !name.is_empty()
        && EVENTS.contains(&asked.as_str())
        && words(event) == [asked.clone()]
        && name.iter().all(|word| whom.contains(word))
}
