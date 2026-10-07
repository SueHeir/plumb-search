//! The docs page set: pages of the software docs sites in
//! [`plumb_core::docs::DOCS_SITES`], each written as an article whose item
//! is the page's address.
//!
//! The pages are fetched by `plumb fetch-pages --set docs` (with
//! `plumb_crawl::fetch_site_pages`); this turns what was fetched into the
//! set's articles. A page is named by its own title (see
//! [`plumb_core::docs::page_title`]), and called by that title after or
//! before the product's name ("python sorting techniques", "javascript
//! array.prototype.sort()"). Its description is the page's own, or else the
//! start of its text. Its views are the site's weight over how deep the
//! page is under the site's docs, so a site's main pages come before its
//! deep ones and the most used docs before others.

use std::collections::HashSet;

use plumb_core::article::{Article, MAX_ALIASES, MAX_ARTICLE_DESCRIPTION_CHARS};
use plumb_core::docs::{page_title, DocsSite};
use serde::{Deserialize, Serialize};

/// Views of a page right at the root of a site of weight 1.
pub const VIEWS_PER_WEIGHT: u64 = 1_000;
/// Most pages fetched from each site, unless asked otherwise.
pub const DEFAULT_MAX_PER_SITE: usize = 20_000;
/// Most words of a title's second part that name its section ("JavaScript"
/// in "Array.prototype.sort() — JavaScript").
const SECTION_WORDS: usize = 3;

/// What a wiki's pages that list or are about pages start with.
const WIKI_LISTS: &[&str] = &[
    "Category:",
    "Special:",
    "Talk:",
    "File:",
    "Template:",
    "User:",
    "Help:",
    "ArchWiki:",
];

/// A docs page as fetched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchedDoc {
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// The start of the page's text.
    pub text: Option<String>,
}

/// The articles of `site`'s pages `docs`, in the order given, leaving out
/// pages with no title of their own and a title the site has used before
/// (the first, shallowest, page keeps it).
pub fn docs_articles(site: &DocsSite, docs: &[FetchedDoc]) -> Vec<Article> {
    let mut seen = HashSet::new();
    docs.iter()
        .filter_map(|doc| doc_article(site, doc))
        .filter(|article| seen.insert(article.title.to_lowercase()))
        .collect()
}

/// The article of `doc`, a page of `site`. `None` for a page with no title
/// but the site's name, or not under the site's roots.
pub fn doc_article(site: &DocsSite, doc: &FetchedDoc) -> Option<Article> {
    let depth = depth_under(site, &doc.url)?;
    let title = page_title(site, doc.title.as_deref()?)?;
    // A wiki's lists of pages, not pages: "Category:Electronic Frontier
    // Foundation".
    if WIKI_LISTS.iter().any(|list| title.starts_with(list)) {
        return None;
    }
    let description = doc
        .description
        .as_deref()
        .or(doc.text.as_deref())
        .map(plumb_core::collapse_whitespace)
        .filter(|text| !text.is_empty())
        .map(|text| plumb_core::truncate_chars(&text, MAX_ARTICLE_DESCRIPTION_CHARS));
    Some(Article {
        aliases: aliases(site, &title),
        title,
        description,
        item: Some(doc.url.clone()),
        views: site.weight * VIEWS_PER_WEIGHT / depth.max(1) as u64,
        ..Article::default()
    })
}

/// What the page `title` of `site` is called by: its first part after and
/// before the product's name, and after its section when its second part
/// names one ("JavaScript Array.prototype.sort()").
fn aliases(site: &DocsSite, title: &str) -> Vec<String> {
    let mut parts = title.split(" — ");
    let first = parts.next().unwrap_or(title).trim();
    let mut aliases = vec![
        format!("{} {first}", site.product),
        format!("{first} {}", site.product),
    ];
    if let Some(section) = parts
        .next()
        .map(str::trim)
        .filter(|s| s.split_whitespace().count() <= SECTION_WORDS)
    {
        // "CSS: Cascading Style Sheets" is "CSS".
        let section = section.split(':').next().unwrap_or(section).trim();
        aliases.push(format!("{section} {first}"));
        aliases.push(format!("{first} {section}"));
    }
    let lower = first.to_lowercase();
    aliases.retain(|alias| alias.to_lowercase() != lower);
    aliases.dedup();
    aliases.truncate(MAX_ALIASES);
    aliases
}

/// How deep `url` is under the deepest of `site`'s roots it is under: the
/// path segments after the root's, at least 1. `None` when it is under
/// none of them.
fn depth_under(site: &DocsSite, url: &str) -> Option<usize> {
    let segments = |path: &str| path.split('/').filter(|s| !s.is_empty()).count();
    site.roots
        .iter()
        .filter(|root| url.starts_with(*root) || url.starts_with(root.trim_end_matches('/')))
        .map(|root| {
            let rest = &url[root.trim_end_matches('/').len()..];
            segments(rest.split(['?', '#']).next().unwrap_or(""))
        })
        .min()
        .map(|depth| depth.max(1))
}

/// Sorts `articles` most viewed first, keeping each address once.
pub fn sort_docs(articles: &mut Vec<Article>) {
    let mut seen = HashSet::new();
    articles.retain(|article| seen.insert(article.item.clone()));
    articles.sort_by_key(|article| std::cmp::Reverse(article.views));
}

#[cfg(test)]
mod tests {
    use super::*;
    use plumb_core::docs::site;

    fn doc(url: &str, title: &str, description: Option<&str>) -> FetchedDoc {
        FetchedDoc {
            url: url.into(),
            title: Some(title.into()),
            description: description.map(Into::into),
            text: Some("Python lists have a built-in list.sort() method.".into()),
        }
    }

    #[test]
    fn writes_docs_pages_as_articles() {
        let python = site("python").unwrap();
        let sorting = doc_article(
            python,
            &doc(
                "https://docs.python.org/3/howto/sorting.html",
                "Sorting Techniques — Python 3.14.0 documentation",
                None,
            ),
        )
        .unwrap();
        assert_eq!(sorting.title, "Sorting Techniques");
        assert_eq!(
            sorting.aliases,
            ["Python Sorting Techniques", "Sorting Techniques Python"]
        );
        assert_eq!(
            sorting.description.as_deref(),
            Some("Python lists have a built-in list.sort() method.")
        );
        assert_eq!(
            sorting.item.as_deref(),
            Some("https://docs.python.org/3/howto/sorting.html")
        );
        assert_eq!(sorting.views, 10 * VIEWS_PER_WEIGHT / 2);

        let mdn = site("mdn").unwrap();
        let sort = doc_article(
            mdn,
            &doc(
                "https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Array/sort",
                "Array.prototype.sort() - JavaScript | MDN",
                Some("The sort() method of Array instances sorts the elements of an array in place."),
            ),
        )
        .unwrap();
        assert_eq!(sort.title, "Array.prototype.sort() — JavaScript");
        assert_eq!(
            sort.aliases,
            [
                "MDN Array.prototype.sort()",
                "Array.prototype.sort() MDN",
                "JavaScript Array.prototype.sort()",
                "Array.prototype.sort() JavaScript"
            ]
        );
        assert!(sort.description.unwrap().starts_with("The sort() method"));
        assert_eq!(sort.views, 10 * VIEWS_PER_WEIGHT / 6);

        // The site's front page, and a page of another site.
        assert!(doc_article(
            python,
            &doc("https://docs.python.org/3/", "3.14.0 Documentation", None)
        )
        .is_none());
        assert!(doc_article(python, &doc("https://python.org/x", "X", None)).is_none());
        let arch = site("archwiki").unwrap();
        assert!(doc_article(
            arch,
            &doc(
                "https://wiki.archlinux.org/title/Category:Electronic_Frontier_Foundation",
                "Category:Electronic Frontier Foundation - ArchWiki",
                None
            )
        )
        .is_none());
    }

    #[test]
    fn keeps_a_title_once_and_sorts_by_views() {
        let python = site("python").unwrap();
        let mut articles = docs_articles(
            python,
            &[
                doc(
                    "https://docs.python.org/3/library/functions.html",
                    "Built-in Functions — Python 3.14.0 documentation",
                    None,
                ),
                doc(
                    "https://docs.python.org/3/library/x/functions.html",
                    "Built-in Functions — Python 3.14.0 documentation",
                    None,
                ),
                doc(
                    "https://docs.python.org/3/glossary.html",
                    "Glossary — Python 3.14.0 documentation",
                    None,
                ),
            ],
        );
        assert_eq!(articles.len(), 2);
        sort_docs(&mut articles);
        assert_eq!(articles[0].title, "Glossary");
        assert_eq!(articles[1].title, "Built-in Functions");
    }
}
