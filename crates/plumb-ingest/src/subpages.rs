//! The subpages page set: inner pages of the sites in
//! [`plumb_core::subpages::SUBPAGE_SITES`] (universities and labs, big
//! companies, government, entertainment, museums), written like reference
//! pages ([`crate::reference`]): named by their own title without the
//! site's name, described by their own description or the start of their
//! text, their views the site's weight over how deep the page is.
//!
//! Some sites end their titles with their name in brackets ("Yosemite
//! National Park (U.S. National Park Service)"); a bracketed ending three
//! or more pages share is dropped too.

use std::collections::HashMap;

use plumb_core::article::Article;
use plumb_core::subpages::SubpageSite;

use crate::docs::FetchedDoc;
use crate::reference::reference_articles;

/// Pages of a site whose titles share a bracketed ending for it to be the
/// site's name.
const SHARED_BY: usize = 3;

/// The articles of `site`'s pages `docs`, in the order given (see
/// [`reference_articles`]).
pub fn subpage_articles(site: &SubpageSite, docs: &[FetchedDoc]) -> Vec<Article> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for title in docs.iter().filter_map(|doc| doc.title.as_deref()) {
        if let Some((_, ending)) = bracketed_ending(title) {
            *counts.entry(ending).or_default() += 1;
        }
    }
    let docs: Vec<FetchedDoc> = docs
        .iter()
        .map(|doc| {
            let title = doc.title.as_deref().map(|title| {
                match bracketed_ending(title) {
                    // The reference rules drop an ending after " | ".
                    Some((head, ending)) if counts.get(ending).is_some_and(|&n| n >= SHARED_BY) => {
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
        }
    }

    #[test]
    fn a_bracketed_ending_many_pages_share_is_the_sites() {
        let site = plumb_core::subpages::site("chessprogramming.org").unwrap();
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
    fn reads_bracketed_endings() {
        assert_eq!(
            bracketed_ending("Yosemite National Park (U.S. National Park Service)"),
            Some(("Yosemite National Park", "U.S. National Park Service"))
        );
        assert_eq!(bracketed_ending("(Untitled)"), None);
        assert_eq!(bracketed_ending("Perft Results"), None);
    }
}
