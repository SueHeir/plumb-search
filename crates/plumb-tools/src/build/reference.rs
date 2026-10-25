//! The reference page set: inner pages of the sites in
//! [`crate::reference_sites::REFERENCE_SITES`] (health, dictionaries,
//! recipes, how-tos, government), each written as an article whose item
//! is the page's address.
//!
//! The pages are fetched by `plumb-tools fetch-pages --set reference` (with
//! `crate::fetch::site_pages`); this turns what was fetched into the
//! set's articles. A page is named by its own title without the site's
//! name ("Foul-Smelling Stool: Causes and Treatment | MedicineNet" is
//! "Foul-Smelling Stool: Causes and Treatment"), and described by its own
//! description, or else the start of its text. Its views are the site's
//! weight over how deep the page is, so a site's main pages come before
//! its deep ones and the most used sites before others.
//!
//! A reference page keeps no section headings or passages of its text. It
//! is listed only for words of its title (`subpage_asked` in plumb-index),
//! so words only its headings or text have let it take one of a search's
//! places for pages and be left out: with its text, BabyCenter's "Baby ear
//! infections" and eight more of its pages took every place for "what good
//! for diarrhea", and the search lost the article Diarrhea. Subpages, which
//! are listed on the best sites without words of their title, keep both.

use std::collections::{HashMap, HashSet};

use plumb_core::article::{Article, MAX_ARTICLE_DESCRIPTION_CHARS};

use crate::build::docs::{FetchedDoc, NOT_PAGES};
use crate::reference_sites::ReferenceSite;

/// Views of a page right at the root of a site of weight 1.
pub const VIEWS_PER_WEIGHT: u64 = 1_000;
/// Most pages fetched from each site, unless the site or the run asks
/// otherwise.
pub const DEFAULT_MAX_PER_SITE: usize = 5_000;
/// Pages of a site whose titles share an ending ("| MedicineNet"), a title
/// or a description for it to be the site's rather than theirs.
const SHARED_BY: usize = 3;
/// What ends a title before the site's name.
const TITLE_SEPARATORS: &[&str] = &[" | ", " - ", " – ", " — ", " :: ", " · ", " » "];

/// The reference set's articles of `site`'s pages `docs`: its
/// [`site_articles`] without their headings and passages.
pub fn reference_articles(site: &ReferenceSite, docs: &[FetchedDoc]) -> Vec<Article> {
    site_articles(site, docs)
        .into_iter()
        .map(|article| Article {
            sections: Vec::new(),
            search: None,
            ..article
        })
        .collect()
}

/// The articles of `site`'s pages `docs`, in the order given, named by
/// [`page_titles`]. Of the pages with the same title in one language, only
/// the shallowest is kept (the first of equally deep ones): the copies of
/// a page in other countries ("/uk/airpods-pro/") are deeper than the page
/// itself. The site's front page is left out, and with it any page that
/// has its title (one that is gone, sent back to it). A description
/// [`SHARED_BY`] or more pages share is the site's: the page's text is used
/// instead, or none.
pub fn site_articles(site: &ReferenceSite, docs: &[FetchedDoc]) -> Vec<Article> {
    let pages: Vec<String> = docs.iter().map(|doc| unlocalized(&doc.url)).collect();
    let languages: Vec<Option<String>> = docs
        .iter()
        .map(|doc| doc.language.as_deref().and_then(plumb_core::language_code))
        .collect();
    let titles = page_titles(docs, &pages, &languages);
    let descriptions = shared(docs.iter().map(|doc| doc.description.as_deref()), &pages);
    let texts = shared(docs.iter().map(|doc| doc.text.as_deref()), &pages);
    let mut seen_urls = HashSet::new();
    let mut kept: HashMap<(String, &Option<String>), (usize, usize)> = HashMap::new();
    for (i, (doc, title)) in docs.iter().zip(&titles).enumerate() {
        let Some(title) = title else { continue };
        let Ok(url) = url::Url::parse(&doc.url) else {
            continue;
        };
        let lower = title.to_lowercase();
        if !useful_page(doc) || NOT_PAGES.contains(&lower.as_str()) || !seen_urls.insert(&doc.url) {
            continue;
        }
        let depth = url.path().split('/').filter(|s| !s.is_empty()).count();
        let best = kept.entry((lower, &languages[i])).or_insert((depth, i));
        *best = (*best).min((depth, i));
    }
    let mut kept: Vec<(usize, usize)> =
        kept.into_values().filter(|&(depth, _)| depth > 0).collect();
    kept.sort_unstable_by_key(|&(_, i)| i);
    kept.into_iter()
        .filter_map(|(depth, i)| {
            let doc = &docs[i];
            let description = doc
                .description
                .as_deref()
                .and_then(short)
                .filter(|text| !descriptions.contains(text))
                .or_else(|| {
                    doc.text
                        .as_deref()
                        .and_then(short)
                        .filter(|text| !texts.contains(text))
                });
            Some(Article {
                title: titles[i].clone()?,
                description,
                language: languages[i].clone(),
                item: Some(doc.url.clone()),
                views: site.weight * VIEWS_PER_WEIGHT / depth as u64,
                sections: useful_sections(&doc.sections),
                search: doc
                    .search
                    .as_ref()
                    .and_then(plumb_core::article::SearchContent::bounded),
                ..Article::default()
            })
        })
        .collect()
}

/// The titles of `docs` without the site's name: its endings that
/// [`SHARED_BY`] or more pages share ("Foul-Smelling Stool: Causes and
/// Treatment | MedicineNet" is "Foul-Smelling Stool: Causes and
/// Treatment"). A title that many pages in one language still share is
/// the site's ("Access Denied", the homepage's title on pages that are
/// gone): `None`, unless they differ once the last ending dropped is put
/// back. Mayo Clinic's "Asthma - Symptoms and causes - Mayo Clinic" is
/// "Asthma - Symptoms and causes", as its other tabs on asthma are
/// "Asthma" too without their tab. `pages` are the docs' addresses read by
/// [`unlocalized`], so a page's copies in other countries count once.
fn page_titles(
    docs: &[FetchedDoc],
    pages: &[String],
    languages: &[Option<String>],
) -> Vec<Option<String>> {
    let endings = shared_endings(docs.iter().map(|doc| doc.title.as_deref()), pages);
    let forms: Vec<Vec<String>> = docs
        .iter()
        .map(|doc| title_forms(doc.title.as_deref().unwrap_or(""), &endings))
        .collect();
    let shared_as =
        |pick: fn(&[String]) -> Option<&String>| {
            shared_by(forms.iter().zip(languages).zip(pages).filter_map(
                |((forms, language), page)| Some(((pick(forms)?.to_lowercase(), language), page)),
            ))
        };
    let last: fn(&[String]) -> Option<&String> = |forms| forms.last();
    let before_last: fn(&[String]) -> Option<&String> =
        |forms| forms.get(forms.len().saturating_sub(2));
    let (shared, shared_before) = (shared_as(last), shared_as(before_last));
    forms
        .iter()
        .zip(languages)
        .map(|(forms, language)| {
            let unshared = |title: &&String, shared: &HashSet<(String, &Option<String>)>| {
                !shared.contains(&(title.to_lowercase(), language))
            };
            last(forms)
                .filter(|title| unshared(title, &shared))
                .or_else(|| before_last(forms).filter(|title| unshared(title, &shared_before)))
                .cloned()
        })
        .collect()
}

/// Exclude soft errors, site indexes, search/redirect pages and empty JS
/// shells from useful coverage, even when HTTP returned 200. Shared by the
/// publication gate for docs and reference/subpages.
pub fn useful_page(doc: &FetchedDoc) -> bool {
    let Some(title) = doc.title.as_deref() else {
        return false;
    };
    let lower = plumb_core::collapse_whitespace(title).to_lowercase();
    let head = TITLE_SEPARATORS
        .iter()
        .filter_map(|s| lower.split_once(s).map(|p| p.0))
        .next()
        .unwrap_or(&lower);
    if NOT_PAGES.contains(&head)
        || [
            "site index",
            "sitemap",
            "access denied",
            "403 forbidden",
            "internal server error",
            "service unavailable",
            "just a moment",
            "enable javascript",
        ]
        .contains(&head)
        || head.starts_with("redirecting")
        || head.starts_with("404 ")
    {
        return false;
    }
    let text = doc.text.as_deref().unwrap_or("").trim();
    if text
        .to_lowercase()
        .contains("you need to enable javascript to run this app")
    {
        return false;
    }
    !title.trim().is_empty()
        && (doc
            .description
            .as_deref()
            .is_some_and(|d| !d.trim().is_empty())
            || !text.is_empty())
}

fn useful_sections(sections: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut chars = 0;
    sections
        .iter()
        .filter_map(|s| {
            let s = plumb_core::collapse_whitespace(s);
            let lower = s.to_lowercase();
            if s.is_empty()
                || ["navigation", "menu", "search", "on this page"].contains(&lower.as_str())
                || !seen.insert(lower)
            {
                return None;
            }
            let n = s.chars().count();
            if chars + n > plumb_core::article::MAX_SECTIONS_CHARS {
                return None;
            }
            chars += n;
            Some(s)
        })
        .take(plumb_core::article::MAX_SECTIONS)
        .collect()
}

/// Sorts `articles` most viewed first, keeping each address once.
pub fn sort_reference(articles: &mut Vec<Article>) {
    let mut seen = HashSet::new();
    articles.retain(|article| seen.insert(article.item.clone()));
    articles.sort_by_key(|article| std::cmp::Reverse(article.views));
}

/// `url`'s host and path without the locale folders it starts with
/// ("/uk/", "/en-au/", "/ca/fr/"), so that the copies of a page in other
/// countries read the same.
pub(crate) fn unlocalized(url: &str) -> String {
    let Ok(url) = url::Url::parse(url) else {
        return url.to_string();
    };
    let path = url.path().to_ascii_lowercase();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let locales = segments
        .iter()
        .take(2)
        .take_while(|s| plumb_core::is_locale_segment(s))
        .count();
    format!(
        "{}/{}",
        url.host_str().unwrap_or_default(),
        segments[locales..].join("/")
    )
}

/// The title endings ([`TITLE_SEPARATORS`] and what follows the last)
/// that [`SHARED_BY`] or more of `titles`' pages share: the site's name.
fn shared_endings<'a>(
    titles: impl Iterator<Item = Option<&'a str>>,
    pages: &[String],
) -> HashSet<String> {
    let mut endings = Vec::new();
    for (title, page) in titles.zip(pages) {
        let Some(title) = title else { continue };
        let mut rest = plumb_core::collapse_whitespace(title);
        // "Pancakes Recipe - Allrecipes.com | Allrecipes": both endings.
        for _ in 0..2 {
            let Some((head, ending)) = split_ending(&rest) else {
                break;
            };
            endings.push((ending.to_lowercase(), page));
            rest = head.to_string();
        }
    }
    shared_by(endings.into_iter())
}

/// `title` before its last separator, and what follows it.
fn split_ending(title: &str) -> Option<(&str, &str)> {
    TITLE_SEPARATORS
        .iter()
        .filter_map(|sep| title.rfind(sep).map(|at| (at, sep.len())))
        .max_by_key(|(at, _)| *at)
        .map(|(at, len)| (title[..at].trim(), title[at + len..].trim()))
        .filter(|(head, ending)| !head.is_empty() && !ending.is_empty())
}

/// `title`, then what is left of it as each of its endings in `endings`
/// (the site's name) is dropped: "Asthma - Symptoms and causes - Mayo
/// Clinic", "Asthma - Symptoms and causes", "Asthma". Nothing when
/// `title` is empty.
fn title_forms(title: &str, endings: &HashSet<String>) -> Vec<String> {
    let title = plumb_core::collapse_whitespace(title);
    if title.is_empty() {
        return Vec::new();
    }
    let mut forms = vec![title];
    while let Some((head, ending)) = forms.last().and_then(|title| split_ending(title)) {
        if !endings.contains(&ending.to_lowercase()) {
            break;
        }
        let head = head.to_string();
        forms.push(head);
    }
    forms
}

/// The short forms of `texts` that [`SHARED_BY`] or more of their pages
/// share.
fn shared<'a>(texts: impl Iterator<Item = Option<&'a str>>, pages: &[String]) -> HashSet<String> {
    shared_by(
        texts
            .zip(pages)
            .filter_map(|(text, page)| Some((short(text?)?, page))),
    )
}

/// What [`SHARED_BY`] or more different pages have, the copies of a page
/// in other countries ([`unlocalized`]) being one page.
pub(crate) fn shared_by<'a, K: Eq + std::hash::Hash>(
    keys: impl Iterator<Item = (K, &'a String)>,
) -> HashSet<K> {
    let mut pages: HashMap<K, HashSet<&String>> = HashMap::new();
    for (key, page) in keys {
        pages.entry(key).or_default().insert(page);
    }
    pages
        .into_iter()
        .filter(|(_, pages)| pages.len() >= SHARED_BY)
        .map(|(key, _)| key)
        .collect()
}

/// `text` as a description: its whitespace collapsed, cut to
/// [`MAX_ARTICLE_DESCRIPTION_CHARS`]. `None` when empty.
fn short(text: &str) -> Option<String> {
    let text = plumb_core::collapse_whitespace(text);
    (!text.is_empty()).then(|| plumb_core::truncate_chars(&text, MAX_ARTICLE_DESCRIPTION_CHARS))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(url: &str, title: &str, description: &str) -> FetchedDoc {
        FetchedDoc {
            url: url.to_string(),
            title: Some(title.to_string()),
            description: Some(description.to_string()),
            text: Some(format!("Text of {title}")),
            sections: Vec::new(),
            search: None,
            language: None,
        }
    }

    #[test]
    fn useful_coverage_rejects_indexes_errors_empty_and_js_shells() {
        for title in [
            "Site Index | Agency",
            "Search Results",
            "Access Denied",
            "Redirecting…",
            "404 Not Found",
            "Internal Server Error",
        ] {
            assert!(
                !useful_page(&doc("https://example.org/task", title, "Boilerplate")),
                "{title}"
            );
        }
        let mut empty = doc("https://example.org/empty", "Agency", "");
        empty.text = None;
        assert!(!useful_page(&empty));
        empty.text = Some("You need to enable JavaScript to run this app.".into());
        assert!(!useful_page(&empty));
        assert!(useful_page(&doc(
            "https://example.org/form-1040",
            "About Form 1040",
            "How to file your tax return"
        )));
    }

    #[test]
    fn articles_drop_the_sites_name_and_shared_text() {
        let site = crate::reference_sites::site("medicinenet.com").unwrap();
        let docs = vec![
            doc(
                "https://www.medicinenet.com/health/foul-smelling-stool",
                "Foul-Smelling Stool: Causes and Treatment | MedicineNet",
                "What makes stool smell bad.",
            ),
            doc(
                "https://www.medicinenet.com/health/high-tsh",
                "High TSH Levels - What They Mean | MedicineNet",
                "Medical information you can trust",
            ),
            doc(
                "https://www.medicinenet.com/health/gone",
                "Page Not Found | MedicineNet",
                "Medical information you can trust",
            ),
            doc(
                "https://www.medicinenet.com/a/b/c",
                "Medical information you can trust",
                "Medical information you can trust",
            ),
            doc(
                "https://www.medicinenet.com/d/e/f/g",
                "Medical information you can trust",
                "x",
            ),
        ];
        let articles = reference_articles(site, &docs);
        let titles: Vec<&str> = articles.iter().map(|a| a.title.as_str()).collect();
        // The site's name goes; "Page Not Found" is no page; a title
        // twice is kept once, from the shallower page.
        assert_eq!(
            titles,
            [
                "Foul-Smelling Stool: Causes and Treatment",
                "High TSH Levels - What They Mean",
                "Medical information you can trust"
            ]
        );
        assert_eq!(
            articles[0].description.as_deref(),
            Some("What makes stool smell bad.")
        );
        // A description most pages share is the site's: the page's text
        // instead.
        assert_eq!(
            articles[1].description.as_deref(),
            Some("Text of High TSH Levels - What They Mean | MedicineNet")
        );
        assert_eq!(
            articles[0].item.as_deref(),
            Some("https://www.medicinenet.com/health/foul-smelling-stool")
        );
        // Weight 8 over two segments deep; three deep weighs less.
        assert_eq!(articles[0].views, 4_000);
        assert_eq!(articles[2].views, 2_666);
    }

    #[test]
    fn reference_articles_keep_no_headings_or_passages() {
        let site = crate::reference_sites::site("babycenter.com").unwrap();
        let mut page = doc(
            "https://www.babycenter.com/health/illness-and-infection/ear-infections",
            "Baby ear infections: Signs, symptoms, and treatment | BabyCenter",
            "How to tell if your baby has an ear infection.",
        );
        page.sections = vec!["Symptoms".into(), "Diarrhea and vomiting".into()];
        page.search = Some(plumb_core::article::SearchContent {
            passages: vec![plumb_core::article::SearchPassage {
                heading: "Symptoms".into(),
                text: "Some babies have diarrhea.".into(),
                anchor: None,
            }],
            ..Default::default()
        });
        let pages = [page];
        let site_pages = site_articles(site, &pages);
        assert_eq!(site_pages[0].sections.len(), 2);
        assert!(site_pages[0].search.is_some());
        let articles = reference_articles(site, &pages);
        assert_eq!(articles.len(), 1);
        assert!(articles[0].sections.is_empty());
        assert_eq!(articles[0].search, None);
    }

    #[test]
    fn language_is_declared_per_page_even_on_a_multilingual_host() {
        let site = crate::reference_sites::site("gesund.bund.de").unwrap();
        let docs: Vec<FetchedDoc> = [
            ("de", Some("de-DE")),
            ("en", Some("en")),
            ("es", Some("es")),
            ("unknown", None),
        ]
        .into_iter()
        .map(|(path, language)| FetchedDoc {
            url: format!("https://gesund.bund.de/{path}/information"),
            title: Some("Information".into()),
            text: Some(format!("Useful health information for the {path} page.")),
            language: language.map(str::to_string),
            ..FetchedDoc::default()
        })
        .collect();
        let articles = reference_articles(site, &docs);
        assert_eq!(articles.len(), 4);
        assert_eq!(
            articles
                .iter()
                .map(|article| article.language.as_deref())
                .collect::<Vec<_>>(),
            [Some("de"), Some("en"), Some("es"), None]
        );
    }

    #[test]
    fn a_title_most_pages_share_is_none() {
        // Pages that are gone, showing the homepage's title.
        let site = crate::reference_sites::site("medicinenet.com").unwrap();
        let docs: Vec<FetchedDoc> = (0..3)
            .map(|i| {
                doc(
                    &format!("https://www.medicinenet.com/{i}"),
                    "MedicineNet: Medical information and health advice you can trust.",
                    "",
                )
            })
            .collect();
        assert!(reference_articles(site, &docs).is_empty());
    }

    /// The titles `site` makes of `titles`, each a page under `prefix`.
    fn titles_of(
        site: &str,
        prefix: &str,
        titles: &[(impl AsRef<str>, impl AsRef<str>)],
    ) -> Vec<String> {
        let site = crate::reference_sites::site(site).unwrap();
        let docs: Vec<FetchedDoc> = titles
            .iter()
            .map(|(path, title)| {
                let path = path.as_ref();
                doc(&format!("{prefix}{path}"), title.as_ref(), path)
            })
            .collect();
        reference_articles(site, &docs)
            .into_iter()
            .map(|a| a.title)
            .collect()
    }

    #[test]
    fn titles_alike_without_both_endings_keep_the_one_before_the_site() {
        // Mayo Clinic's three tabs on a disease are all "Asthma" without
        // "- Symptoms and causes - Mayo Clinic" and the like, and NHS
        // pages "Symptoms" without "- Asthma - NHS".
        let mut mayo = Vec::new();
        let mut nhs = Vec::new();
        for disease in ["Asthma", "Gout", "Shingles"] {
            for tab in ["Symptoms and causes", "Diagnosis and treatment", "Doctors"] {
                mayo.push((
                    format!("{disease}/{tab}"),
                    format!("{disease} - {tab} - Mayo Clinic"),
                ));
            }
            for part in ["Symptoms", "Causes", "Treatment"] {
                nhs.push((
                    format!("{disease}/{part}"),
                    format!("{part} - {disease} - NHS"),
                ));
            }
        }
        let mayo = titles_of(
            "mayoclinic.org",
            "https://www.mayoclinic.org/diseases-conditions/",
            &mayo,
        );
        assert_eq!(mayo.len(), 9);
        assert_eq!(
            mayo[..3],
            [
                "Asthma - Symptoms and causes",
                "Asthma - Diagnosis and treatment",
                "Asthma - Doctors"
            ]
        );
        let nhs = titles_of("nhs.uk", "https://www.nhs.uk/conditions/", &nhs);
        assert_eq!(nhs.len(), 9);
        assert_eq!(
            nhs[..3],
            ["Symptoms - Asthma", "Causes - Asthma", "Treatment - Asthma"]
        );
    }

    #[test]
    fn a_page_in_other_countries_is_one_page() {
        // Its title, ending and description are the page's own, not the
        // site's, though three copies share them.
        let site = crate::reference_sites::site("medicinenet.com").unwrap();
        let mut docs: Vec<FetchedDoc> = ["en-us", "en-gb", "en-au"]
            .into_iter()
            .map(|locale| {
                doc(
                    &format!("https://www.medicinenet.com/{locale}/health/high-tsh"),
                    "Overview - High TSH Levels | MedicineNet",
                    "What a high TSH level means.",
                )
            })
            .collect();
        docs.push(doc(
            "https://www.medicinenet.com/health/gout",
            "Gout | MedicineNet",
            "About gout.",
        ));
        docs.push(doc(
            "https://www.medicinenet.com/health/asthma",
            "Asthma | MedicineNet",
            "About asthma.",
        ));
        let pages: Vec<(String, String, Option<String>)> = reference_articles(site, &docs)
            .into_iter()
            .map(|a| (a.title, a.item.unwrap(), a.description))
            .collect();
        let page = |title: &str, path: &str, description: &str| {
            (
                title.to_string(),
                format!("https://www.medicinenet.com/{path}"),
                Some(description.to_string()),
            )
        };
        assert_eq!(
            pages,
            [
                page(
                    "Overview - High TSH Levels",
                    "en-us/health/high-tsh",
                    "What a high TSH level means."
                ),
                page("Gout", "health/gout", "About gout."),
                page("Asthma", "health/asthma", "About asthma."),
            ]
        );
    }

    #[test]
    fn the_front_page_and_pages_with_its_title_are_left_out() {
        let titles = titles_of(
            "medicinenet.com",
            "https://www.medicinenet.com/",
            &[
                ("", "MedicineNet: Medical information you can trust"),
                (
                    "health/old",
                    "MedicineNet: Medical information you can trust",
                ),
                // Not a locale folder, though it reads like one.
                ("ai", "AI in Health | MedicineNet"),
                ("health/gout", "Gout | MedicineNet"),
                ("health/asthma", "Asthma | MedicineNet"),
            ],
        );
        assert_eq!(titles, ["AI in Health", "Gout", "Asthma"]);
    }

    #[test]
    fn reads_addresses_without_their_locale_folders() {
        assert_eq!(
            unlocalized("https://www.apple.com/ca/fr/airpods-pro/"),
            "www.apple.com/airpods-pro"
        );
        assert_eq!(
            unlocalized("https://www.apple.com/ae-ar/"),
            "www.apple.com/"
        );
        // Only the folders it starts with.
        assert_eq!(
            unlocalized("https://example.org/Docs/en/intro"),
            "example.org/docs/en/intro"
        );
    }

    #[test]
    fn sorts_most_viewed_first_once() {
        let article = |item: &str, views| Article {
            title: item.into(),
            item: Some(item.into()),
            views,
            ..Article::default()
        };
        let mut articles = vec![article("a", 1), article("b", 5), article("a", 9)];
        sort_reference(&mut articles);
        let items: Vec<_> = articles.iter().map(|a| a.views).collect();
        assert_eq!(items, [5, 1]);
    }
}
