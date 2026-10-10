//! The reference page set: inner pages of the sites in
//! [`plumb_core::reference::REFERENCE_SITES`] (health, dictionaries,
//! recipes, how-tos, government), each written as an article whose item
//! is the page's address.
//!
//! The pages are fetched by `plumb fetch-pages --set reference` (with
//! `plumb_crawl::fetch_site_pages`); this turns what was fetched into the
//! set's articles. A page is named by its own title without the site's
//! name ("Foul-Smelling Stool: Causes and Treatment | Healthline" is
//! "Foul-Smelling Stool: Causes and Treatment"), and described by its own
//! description, or else the start of its text. Its views are the site's
//! weight over how deep the page is, so a site's main pages come before
//! its deep ones and the most used sites before others.

use std::collections::{HashMap, HashSet};

use plumb_core::article::{Article, MAX_ARTICLE_DESCRIPTION_CHARS};
use plumb_core::reference::ReferenceSite;

use crate::docs::{FetchedDoc, NOT_PAGES};

/// Views of a page right at the root of a site of weight 1.
pub const VIEWS_PER_WEIGHT: u64 = 1_000;
/// Most pages fetched from each site, unless the site or the run asks
/// otherwise.
pub const DEFAULT_MAX_PER_SITE: usize = 5_000;
/// Pages of a site whose titles share an ending ("| Healthline"), a title
/// or a description for it to be the site's rather than theirs.
const SHARED_BY: usize = 3;
/// What ends a title before the site's name.
const TITLE_SEPARATORS: &[&str] = &[" | ", " - ", " – ", " — ", " :: ", " · ", " » "];

/// The articles of `site`'s pages `docs`, in the order given. The site's
/// name is dropped from the end of their titles (an ending
/// [`SHARED_BY`] or more of them share). Pages with no title of their own,
/// one [`SHARED_BY`] or more share ("Access Denied", the homepage's title
/// on a page that is gone), or a title an earlier page has, are left out.
/// A description many pages share is the site's: the page's text is used
/// instead, or none.
pub fn reference_articles(site: &ReferenceSite, docs: &[FetchedDoc]) -> Vec<Article> {
    let endings = shared_endings(docs.iter().filter_map(|doc| doc.title.as_deref()));
    let titles: Vec<Option<String>> = docs
        .iter()
        .map(|doc| doc.title.as_deref().and_then(|t| page_title(t, &endings)))
        .collect();
    let mut title_counts = HashMap::new();
    for (doc, title) in docs.iter().zip(&titles) {
        if let Some(title) = title {
            *title_counts
                .entry((
                    title.to_lowercase(),
                    doc.language.as_deref().and_then(plumb_core::language_code),
                ))
                .or_default() += 1;
        }
    }
    let descriptions = shared(docs.iter().map(|doc| doc.description.as_deref()));
    let texts = shared(docs.iter().map(|doc| doc.text.as_deref()));
    let mut seen_titles = HashSet::new();
    let mut seen_urls = HashSet::new();
    let mut articles = Vec::new();
    for (doc, title) in docs.iter().zip(titles) {
        let Some(title) = title else { continue };
        let Some(depth) = depth_of(&doc.url) else {
            continue;
        };
        let lower = title.to_lowercase();
        let language = doc.language.as_deref().and_then(plumb_core::language_code);
        if title_counts
            .get(&(lower.clone(), language.clone()))
            .copied()
            .unwrap_or(0)
            >= SHARED_BY
            || NOT_PAGES.contains(&lower.as_str())
            || !seen_urls.insert(doc.url.clone())
            || !seen_titles.insert((lower, language.clone()))
        {
            continue;
        }
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
        articles.push(Article {
            title,
            description,
            language,
            item: Some(doc.url.clone()),
            views: site.weight * VIEWS_PER_WEIGHT / depth.max(1) as u64,
            ..Article::default()
        });
    }
    articles
}

/// Sorts `articles` most viewed first, keeping each address once.
pub fn sort_reference(articles: &mut Vec<Article>) {
    let mut seen = HashSet::new();
    articles.retain(|article| seen.insert(article.item.clone()));
    articles.sort_by_key(|article| std::cmp::Reverse(article.views));
}

/// How many path segments deep `url` is, `None` for an address that does
/// not read.
fn depth_of(url: &str) -> Option<usize> {
    let url = url::Url::parse(url).ok()?;
    Some(url.path().split('/').filter(|s| !s.is_empty()).count())
}

/// The title endings ([`TITLE_SEPARATORS`] and what follows the last)
/// that [`SHARED_BY`] or more of `titles` share: the site's name.
fn shared_endings<'a>(titles: impl Iterator<Item = &'a str>) -> HashSet<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for title in titles {
        let mut rest = plumb_core::collapse_whitespace(title);
        // "Pancakes Recipe - Allrecipes.com | Allrecipes": both endings.
        for _ in 0..2 {
            let Some((head, ending)) = split_ending(&rest) else {
                break;
            };
            *counts.entry(ending.to_lowercase()).or_default() += 1;
            rest = head.to_string();
        }
    }
    counts
        .into_iter()
        .filter(|(_, count)| *count >= SHARED_BY)
        .map(|(ending, _)| ending)
        .collect()
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

/// `title` without the endings in `endings` (the site's name), `None`
/// when nothing is left.
fn page_title(title: &str, endings: &HashSet<String>) -> Option<String> {
    let mut title = plumb_core::collapse_whitespace(title);
    while let Some((head, ending)) = split_ending(&title) {
        if !endings.contains(&ending.to_lowercase()) {
            break;
        }
        title = head.to_string();
    }
    let title = title.trim().to_string();
    (!title.is_empty()).then_some(title)
}

/// The short forms of `texts` that [`SHARED_BY`] or more of them share.
fn shared<'a>(texts: impl Iterator<Item = Option<&'a str>>) -> HashSet<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for text in texts.flatten().filter_map(short) {
        *counts.entry(text).or_default() += 1;
    }
    counts
        .into_iter()
        .filter(|(_, count)| *count >= SHARED_BY)
        .map(|(text, _)| text)
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
    fn articles_drop_the_sites_name_and_shared_text() {
        let site = plumb_core::reference::site("healthline.com").unwrap();
        let docs = vec![
            doc(
                "https://www.healthline.com/health/foul-smelling-stool",
                "Foul-Smelling Stool: Causes and Treatment | Healthline",
                "What makes stool smell bad.",
            ),
            doc(
                "https://www.healthline.com/health/high-tsh",
                "High TSH Levels - What They Mean | Healthline",
                "Medical information you can trust",
            ),
            doc(
                "https://www.healthline.com/health/gone",
                "Page Not Found | Healthline",
                "Medical information you can trust",
            ),
            doc(
                "https://www.healthline.com/a/b/c",
                "Medical information you can trust",
                "Medical information you can trust",
            ),
            doc(
                "https://www.healthline.com/d",
                "Medical information you can trust",
                "x",
            ),
        ];
        let articles = reference_articles(site, &docs);
        let titles: Vec<&str> = articles.iter().map(|a| a.title.as_str()).collect();
        // The site's name goes; "Page Not Found" is no page; a title
        // twice is kept once.
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
            Some("Text of High TSH Levels - What They Mean | Healthline")
        );
        assert_eq!(
            articles[0].item.as_deref(),
            Some("https://www.healthline.com/health/foul-smelling-stool")
        );
        // Weight 10 over two segments deep; three deep weighs less.
        assert_eq!(articles[0].views, 5_000);
        assert_eq!(articles[2].views, 3_333);
    }

    #[test]
    fn language_is_declared_per_page_even_on_a_multilingual_host() {
        let site = plumb_core::reference::site("gesund.bund.de").unwrap();
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
        let site = plumb_core::reference::site("healthline.com").unwrap();
        let docs: Vec<FetchedDoc> = (0..3)
            .map(|i| {
                doc(
                    &format!("https://www.healthline.com/{i}"),
                    "Access Denied",
                    "",
                )
            })
            .collect();
        assert!(reference_articles(site, &docs).is_empty());
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
