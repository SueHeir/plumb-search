//! Pages and the sites they are about: an untitled site takes its
//! article's title, and organizations and governments keep their sites first.

use anyhow::Result;
use tantivy::collector::TopDocs;
use tantivy::query::TermQuery;
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{TantivyDocument, Term};

use super::{Page, PageSearcher, TITLE_CANDIDATES};

/// `title` without a trailing qualifier in brackets: `Python (programming
/// language)` -> `Python`.
pub fn base_title(title: &str) -> &str {
    match title.rfind(" (") {
        Some(i) if title.ends_with(')') && i > 0 => &title[..i],
        _ => title,
    }
}

/// The title of a work in a series without the series' name: "Tears of
/// the Kingdom" of "The Legend of Zelda: Tears of the Kingdom", "The
/// Phantom Menace" of "Star Wars: Episode I – The Phantom Menace". `None`
/// when there is no such part, or it has fewer than two words that are
/// not stop words ("Mission: Impossible", "Pokémon: The Movie"), too
/// little to name the work alone.
pub(crate) fn subtitle(title: &str) -> Option<&str> {
    let (_, rest) = base_title(title).rsplit_once(": ")?;
    let rest = rest.rsplit_once(" – ").map_or(rest, |(_, last)| last);
    (crate::analysis::tokens(&crate::analysis::stemmed_analyzer(), rest).len() >= 2).then_some(rest)
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
    /// whose title the domain spells ([`site_is_titled`]) or abbreviates
    /// ([`site_is_initials`], nba.com) counts:
    /// "Schitt's Creek", whose website is a page on cbc.ca, is not what
    /// cbc.ca is. An official site whose title may be borrowed
    /// ([`may_be_borrowed`]) and names none of its articles takes the most
    /// read one's title too: "New York Post" for nypost.com, not
    /// "California Post".
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
            let query = TermQuery::new(
                Term::from_field_text(self.fields.site, &site.domain),
                IndexRecordOption::Basic,
            );
            let best = searcher.search(
                &query,
                &TopDocs::with_limit(TITLE_CANDIDATES)
                    .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc),
            )?;
            let mut articles = Vec::new();
            for (_, address) in best {
                let document: TantivyDocument = searcher.doc(address)?;
                let Some(stored) = document
                    .get_first(self.fields.page)
                    .and_then(|v| v.as_str())
                else {
                    continue;
                };
                let page: Page = serde_json::from_str(stored)?;
                if page.is_article() && page.website.is_none() {
                    articles.push(page.title);
                }
            }
            let names = articles.iter().map(|title| base_title(title).trim());
            let title = if untitled {
                articles
                    .iter()
                    .find(|title| {
                        site_is_titled(&site.domain, title) || site_is_initials(&site.domain, title)
                    })
                    .map(|title| base_title(title).trim())
            } else {
                // A title that names what any article about the site is
                // stays ("Welcome to Steam" for steampowered.com, whose
                // articles are Valve's and Steam's); else the most read
                // one names it.
                let shown = squash(site.title.as_deref().unwrap_or(""));
                let named = names.clone().any(|name| {
                    let name = squash(name);
                    name.len() < 3 || shown.contains(&name)
                });
                names.clone().next().filter(|_| !named)
            };
            if let Some(title) = title.filter(|title| !title.is_empty()) {
                site.title = Some(title.to_string());
            }
        }
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

/// Whether Wikipedia's short description of a page says it is about an
/// organization, product or service ("American auto insurance company",
/// "Open-source software framework"), see [`ORGANIZATION_WORDS`].
pub(crate) fn describes_an_organization(description: Option<&str>) -> bool {
    description.is_some_and(|text| {
        text.split(|c: char| !c.is_alphanumeric())
            .map(str::to_lowercase)
            .any(|word| {
                let one = word.strip_suffix('s').unwrap_or(&word);
                ORGANIZATION_WORDS.contains(&word.as_str()) || ORGANIZATION_WORDS.contains(&one)
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

/// Whether `domain`'s first label is the initials of the page's title,
/// three or more of them, small words aside: nba.com for "National
/// Basketball Association".
fn site_is_initials(domain: &str, title: &str) -> bool {
    let label = squash(domain.split('.').next().unwrap_or(""));
    let initials: String = base_title(title)
        .split_whitespace()
        .map(squash)
        .filter(|word| !word.is_empty() && !["of", "and", "the", "for"].contains(&word.as_str()))
        .filter_map(|word| word.chars().next())
        .collect();
    label.len() >= 3 && initials == label
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
