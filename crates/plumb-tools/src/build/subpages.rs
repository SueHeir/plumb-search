//! The subpages page set: inner pages of the sites in
//! [`crate::subpage_sites::SUBPAGE_SITES`] (universities and labs, big
//! companies, government, entertainment, museums), written like reference
//! pages ([`crate::build::reference`]): named by their own title without the
//! site's name, described by their own description or the start of their
//! text, their views the site's weight over how deep the page is.
//!
//! Some sites end their titles with their name in brackets ("Yosemite
//! National Park (U.S. National Park Service)"); a bracketed ending three
//! or more pages share is dropped too.

use plumb_core::article::Article;

use crate::build::docs::FetchedDoc;
use crate::build::reference::{reference_articles, shared_by, unlocalized};
use crate::subpage_sites::SubpageSite;

/// The articles of `site`'s pages `docs`, in the order given (see
/// [`reference_articles`]).
pub fn subpage_articles(site: &SubpageSite, docs: &[FetchedDoc]) -> Vec<Article> {
    let pages: Vec<String> = docs.iter().map(|doc| unlocalized(&doc.url)).collect();
    let endings = shared_by(
        docs.iter()
            .zip(&pages)
            .filter_map(|(doc, page)| Some((bracketed_ending(doc.title.as_deref()?)?.1, page))),
    );
    let docs: Vec<FetchedDoc> = docs
        .iter()
        .map(|doc| {
            let title = doc.title.as_deref().map(|title| {
                match bracketed_ending(title) {
                    // The reference rules drop an ending after " | ".
                    Some((head, ending)) if endings.contains(ending) => {
                        format!("{head} | {ending}")
                    }
                    _ => title.to_string(),
                }
            });
            FetchedDoc {
                title,
                ..doc.clone()
            }
        })
        .collect();
    reference_articles(&site.site, &docs)
}

/// `title` before a bracketed ending, and what is in the brackets:
/// "Yosemite National Park (U.S. National Park Service)" is "Yosemite
/// National Park" and "U.S. National Park Service".
fn bracketed_ending(title: &str) -> Option<(&str, &str)> {
    let title = title.trim();
    let inner = title.strip_suffix(')')?;
    let open = inner.rfind(" (")?;
    let head = inner[..open].trim();
    let ending = inner[open + 2..].trim();
    (!head.is_empty() && !ending.is_empty() && !ending.contains('(')).then_some((head, ending))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(url: &str, title: &str) -> FetchedDoc {
        FetchedDoc {
            url: url.to_string(),
            title: Some(title.to_string()),
            description: Some(format!("About {title}")),
            text: None,
            sections: Vec::new(),
            search: None,
            language: None,
        }
    }

    #[test]
    fn a_bracketed_ending_many_pages_share_is_the_sites() {
        let site = crate::subpage_sites::site("chessprogramming.org").unwrap();
        let docs = vec![
            doc(
                "https://www.chessprogramming.org/Perft_Results",
                "Perft Results (Chess Programming Wiki)",
            ),
            doc(
                "https://www.chessprogramming.org/Perft",
                "Perft (Chess Programming Wiki)",
            ),
            doc(
                "https://www.chessprogramming.org/Bitboards",
                "Bitboards (Chess Programming Wiki)",
            ),
            doc(
                "https://www.chessprogramming.org/Stockfish",
                "Stockfish (chess engine)",
            ),
            // Copies of one page in other languages are one page.
            doc(
                "https://www.chessprogramming.org/de/Stockfish",
                "Stockfish (chess engine)",
            ),
            doc(
                "https://www.chessprogramming.org/fr/Stockfish",
                "Stockfish (chess engine)",
            ),
        ];
        let titles: Vec<String> = subpage_articles(site, &docs)
            .into_iter()
            .map(|article| article.title)
            .collect();
        assert_eq!(
            titles,
            [
                "Perft Results",
                "Perft",
                "Bitboards",
                // One page's own brackets stay.
                "Stockfish (chess engine)"
            ]
        );
    }

    #[test]
    fn a_brands_page_in_many_countries_is_kept_from_its_own() {
        // Outside the US, Apple's titles end "- Apple (UK)": without the
        // site's name every English copy of a product's page was the same
        // title, so all were dropped and only the Arabic copy was left.
        let site = crate::subpage_sites::site("www.apple.com").unwrap();
        let mut docs = Vec::new();
        for (folder, country, language) in [
            ("uk/", " (UK)", "en-GB"),
            ("au/", " (AU)", "en-AU"),
            ("ae/", " (AE)", "en-AE"),
            ("ae-ar/", " (AE)", "ar-AE"),
            ("", "", "en-US"),
        ] {
            for (path, name) in [
                ("airpods-pro/", "AirPods Pro 3 - Apple"),
                ("iphone/", "iPhone - Apple"),
                ("watch/", "Apple Watch - Apple"),
            ] {
                docs.push(FetchedDoc {
                    language: Some(language.to_string()),
                    ..doc(
                        &format!("https://www.apple.com/{folder}{path}"),
                        &format!("{name}{country}"),
                    )
                });
            }
        }
        docs.push(doc("https://www.apple.com/", "Apple"));
        let pages: Vec<(String, Option<String>, Option<String>)> = subpage_articles(site, &docs)
            .into_iter()
            .map(|article| (article.title, article.item, article.language))
            .collect();
        let page = |title: &str, at: &str, language: &str| {
            (
                title.to_string(),
                Some(format!("https://www.apple.com/{at}")),
                Some(language.to_string()),
            )
        };
        // The US pages, though listed last, are the shallowest; the front
        // page is the site, not one of its pages.
        let english: Vec<_> = pages
            .iter()
            .filter(|(_, _, language)| language.as_deref() == Some("en"))
            .cloned()
            .collect();
        assert_eq!(
            english,
            [
                page("AirPods Pro 3", "airpods-pro/", "en"),
                page("iPhone", "iphone/", "en"),
                page("Apple Watch", "watch/", "en"),
            ]
        );
        assert!(pages.contains(&page("AirPods Pro 3", "ae-ar/airpods-pro/", "ar")));
    }

    #[test]
    fn copies_of_a_page_others_share_a_title_with_are_one_page() {
        // "iPhone - Apple", "iPhone - Compare Models - Apple" and
        // "iPhone - Tech Specs - Apple" are all "iPhone" without their
        // endings: each keeps the ending before "Apple" back, and so do
        // its copies in other countries. A gone page's title on pages in
        // three countries is the site's.
        let site = crate::subpage_sites::site("www.apple.com").unwrap();
        let mut docs = Vec::new();
        for (folder, country, gone) in [("uk/", " (UK)", "a"), ("au/", " (AU)", "b"), ("", "", "c")]
        {
            for name in ["iPhone", "Mac", "iPad"] {
                let path = name.to_lowercase();
                for (tab, ending) in [
                    ("", ""),
                    ("compare/", " - Compare Models"),
                    ("specs/", " - Tech Specs"),
                ] {
                    docs.push(FetchedDoc {
                        language: Some("en".into()),
                        ..doc(
                            &format!("https://www.apple.com/{folder}{path}/{tab}"),
                            &format!("{name}{ending} - Apple{country}"),
                        )
                    });
                }
            }
            docs.push(FetchedDoc {
                language: Some("en".into()),
                ..doc(
                    &format!("https://www.apple.com/{folder}gone-{gone}/"),
                    &format!("Sorry, this page isn't available - Apple{country}"),
                )
            });
        }
        let pages: Vec<(String, String)> = subpage_articles(site, &docs)
            .into_iter()
            .map(|article| (article.title, article.item.unwrap()))
            .collect();
        let expected: Vec<(String, String)> = ["iPhone", "Mac", "iPad"]
            .into_iter()
            .flat_map(|name| {
                let path = name.to_lowercase();
                [
                    ("", " - Apple"),
                    ("compare/", " - Compare Models"),
                    ("specs/", " - Tech Specs"),
                ]
                .map(|(tab, ending)| {
                    (
                        format!("{name}{ending}"),
                        format!("https://www.apple.com/{path}/{tab}"),
                    )
                })
            })
            .collect();
        assert_eq!(pages, expected);
    }

    #[test]
    fn reads_bracketed_endings() {
        assert_eq!(
            bracketed_ending("Yosemite National Park (U.S. National Park Service)"),
            Some(("Yosemite National Park", "U.S. National Park Service"))
        );
        assert_eq!(bracketed_ending("(Untitled)"), None);
        assert_eq!(bracketed_ending("Perft Results"), None);
    }
}
