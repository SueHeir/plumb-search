//! `plumb check-labels`: are the answers a queries file expects really in
//! the page sets? A test search whose expected page no set has (a wrong
//! question number, a renamed article, a repository whose owner is
//! written in another case) can never be found, and only makes a ranking
//! look worse than it is.
//!
//! For each expected page address it prints `ok` with the page's title,
//! so a person can see the label is the page the search means, or `case`
//! when a set has the address in another case, or `missing`. An address
//! ending in `*` is `ok` when any page starts with it. Sites (domains) and
//! fact answers are not pages and are skipped. `--find TITLE` lists the
//! pages titled TITLE, the most read first, to look up a label.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use plumb_index::pages::Page;
use tracing::info;

use crate::eval::{parse_queries, set_of_file};

#[derive(Debug, Args)]
pub struct CheckLabelsArgs {
    /// Queries files (eval/*_queries.tsv). Can be given more than once.
    #[arg(long, value_name = "TSV")]
    pub queries: Vec<PathBuf>,
    /// Page set files (wikipedia-en.tsv.gz, github.tsv.gz from
    /// fetch-pages), named as for `plumb eval --pages`. Can be given more
    /// than once.
    #[arg(long, value_name = "PATH", required = true)]
    pub pages: Vec<PathBuf>,
    /// Also list the pages titled this (any case), the most read first.
    /// Can be given more than once.
    #[arg(long, value_name = "TITLE")]
    pub find: Vec<String>,
}

/// What a set has for one expected address.
#[derive(Debug, PartialEq, Eq)]
pub enum Label<'a> {
    /// The page, by its title; or, for an address ending in `*`, the
    /// number of pages it takes.
    Found(String),
    /// A page whose address differs only in case.
    OtherCase(&'a str),
    Missing,
}

/// The pages of the sets, looked up by address.
pub struct Pages {
    by_url: HashMap<String, Page>,
    /// Lowercased address to the address.
    folded: HashMap<String, String>,
}

impl Pages {
    pub fn new(pages: Vec<Page>) -> Pages {
        let folded = pages
            .iter()
            .map(|p| (p.url.to_lowercase(), p.url.clone()))
            .collect();
        let by_url = pages.into_iter().map(|p| (p.url.clone(), p)).collect();
        Pages { by_url, folded }
    }

    pub fn label(&self, expected: &str) -> Label<'_> {
        if let Some(start) = expected.strip_suffix('*') {
            let n = self.by_url.keys().filter(|u| u.starts_with(start)).count();
            return if n > 0 {
                Label::Found(format!("{n} pages"))
            } else {
                Label::Missing
            };
        }
        if let Some(page) = self.by_url.get(expected) {
            return Label::Found(page.title.clone());
        }
        match self.folded.get(&expected.to_lowercase()) {
            Some(url) => Label::OtherCase(url),
            None => Label::Missing,
        }
    }

    /// The pages titled `title` (any case), the most read first.
    pub fn titled(&self, title: &str) -> Vec<&Page> {
        let title = title.to_lowercase();
        let mut found: Vec<&Page> = self
            .by_url
            .values()
            .filter(|p| p.title.to_lowercase() == title)
            .collect();
        found.sort_by(|a, b| b.views.cmp(&a.views).then(a.url.cmp(&b.url)));
        found
    }
}

/// Whether `answer` is a page's address rather than a site or a fact.
fn is_page(answer: &str) -> bool {
    answer.starts_with("https://") || answer.starts_with("http://")
}

pub fn run(args: CheckLabelsArgs) -> Result<()> {
    let mut all = Vec::new();
    for file in &args.pages {
        let reader = plumb_ingest::open_maybe_gz(file)?;
        let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let articles = plumb_core::article::read_articles(reader, usize::MAX)
            .with_context(|| format!("reading {}", file.display()))?;
        info!("{} pages in {}", articles.len(), file.display());
        let set = set_of_file(name);
        all.extend(articles.into_iter().filter_map(|a| Page::from_set(&set, a)));
    }
    let pages = Pages::new(all);
    for title in &args.find {
        println!("find {title:?}");
        for page in pages.titled(title).into_iter().take(10) {
            println!("  {}  {}  {} views", page.url, page.title, page.views);
        }
    }
    for path in &args.queries {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let queries =
            parse_queries(&text).with_context(|| format!("parsing {}", path.display()))?;
        let (mut ok, mut wrong) = (0, 0);
        println!("== {}", path.display());
        for q in &queries {
            for expected in q.expected.iter().filter(|e| is_page(e)) {
                let line = q.line;
                match pages.label(expected) {
                    Label::Found(title) => {
                        ok += 1;
                        println!("ok       {:?} (line {line}) {expected}: {title}", q.query);
                    }
                    Label::OtherCase(url) => {
                        wrong += 1;
                        println!(
                            "case     {:?} (line {line}) {expected}: the set has {url}",
                            q.query
                        );
                    }
                    Label::Missing => {
                        wrong += 1;
                        println!("missing  {:?} (line {line}) {expected}", q.query);
                    }
                }
            }
        }
        println!("{ok} found, {wrong} not\n");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use plumb_core::article::Article;

    fn page(set: &str, title: &str, item: &str, views: u64) -> Page {
        Page::from_set(
            set,
            Article {
                title: title.into(),
                item: Some(item.into()),
                views,
                ..Article::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn labels_are_found_in_another_case_or_missing() {
        let pages = Pages::new(vec![
            page("github", "BurntSushi/ripgrep", "", 50_000),
            page(
                "stackoverflow",
                "How do I undo the most recent local commits in Git?",
                "927358",
                9,
            ),
            page("wikipedia-en", "Marie Curie", "Q7186", 100),
        ]);
        assert_eq!(
            pages.label("https://stackoverflow.com/questions/927358"),
            Label::Found("How do I undo the most recent local commits in Git?".into())
        );
        assert_eq!(
            pages.label("https://github.com/burntsushi/ripgrep"),
            Label::OtherCase("https://github.com/BurntSushi/ripgrep")
        );
        assert_eq!(
            pages.label("https://stackoverflow.com/questions/1"),
            Label::Missing
        );
        assert_eq!(
            pages.label("https://stackoverflow.com/questions/*"),
            Label::Found("1 pages".into())
        );
        assert_eq!(
            pages.label("https://diy.stackexchange.com/questions/*"),
            Label::Missing
        );
        assert_eq!(pages.titled("marie CURIE").len(), 1);
        assert!(is_page("https://en.wikipedia.org/wiki/Marie_Curie"));
        assert!(!is_page("chase.com") && !is_page("canberra"));
    }
}
