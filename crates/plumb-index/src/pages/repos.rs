//! Repositories asked for by their name and what they are: "frp reverse
//! proxy" for fatedier/frp, "A fast reverse proxy to help you expose a
//! local server behind a NAT or firewall to the internet".
//!
//! A repository's only key is its title, `owner/name`, so the searches by
//! keys in [`PageSearcher::search_once`] find one by its name only when the
//! query has its owner too. Here repositories are found by the words of
//! their names instead.

use std::collections::HashSet;

use anyhow::Result;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::IndexRecordOption;
use tantivy::{DocAddress, Searcher, Term};

use super::scoring::{name_by_alias, scoped_query};
use super::{
    Page, PageHit, PageSearcher, ASKING_WORDS, GITHUB_SET, REPO_WEIGHT, TITLE_INSIDE_CANDIDATES,
};
use crate::analysis;

impl PageSearcher {
    /// The most starred repositories that each part of the query `words`
    /// may name, by the part's words or by the part written as one word
    /// ("adguard home" for AdGuardHome), each with whether the whole query
    /// found it. Of the whole query only the one word is searched: its
    /// words are searched already.
    pub(crate) fn repos_called(
        &self,
        searcher: &Searcher,
        words: &[String],
        scope: Option<&dyn Query>,
    ) -> Result<Vec<(DocAddress, bool)>> {
        let term = |word: &str| -> Box<dyn Query> {
            Box::new(TermQuery::new(
                Term::from_field_text(self.fields.words, word),
                IndexRecordOption::Basic,
            ))
        };
        let mut found: Vec<(DocAddress, bool)> = Vec::new();
        for len in (1..=words.len()).rev() {
            let whole = len == words.len();
            for span in words.windows(len) {
                if whole && len == 1
                    || span.iter().all(|word| {
                        crate::is_function_word(word) || ASKING_WORDS.contains(&word.as_str())
                    })
                {
                    continue;
                }
                let Some(key) = analysis::tokens(&self.joined, &span.join(" ")).pop() else {
                    continue;
                };
                let mut called = vec![(Occur::Should, term(&key))];
                if !whole {
                    let every_word = span.iter().map(|word| (Occur::Must, term(word))).collect();
                    called.push((Occur::Should, Box::new(BooleanQuery::new(every_word))));
                }
                let repos = BooleanQuery::new(vec![
                    (Occur::Must, self.scope_term(&format!("set:{GITHUB_SET}"))),
                    (Occur::Must, Box::new(BooleanQuery::new(called))),
                ]);
                let most_starred = TopDocs::with_limit(TITLE_INSIDE_CANDIDATES)
                    .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc);
                for (_, address) in searcher.search(
                    self.language_query(scoped_query(&repos, scope).as_ref())
                        .as_ref(),
                    &most_starred,
                )? {
                    if !found.iter().any(|(seen, _)| *seen == address) {
                        found.push((address, whole));
                    }
                }
            }
        }
        Ok(found)
    }

    /// Whether the query `words` call the repository `page` by part of
    /// them, its title, its name or another name, and every other main word
    /// is a word of its description: "frp reverse proxy" and "delta git
    /// pager" (dandavison/delta, "A syntax-highlighting pager for git,
    /// diff, grep, and blame output"), not "delta airlines flights".
    pub(crate) fn repo_described(&self, page: &Page, words: &[String]) -> bool {
        if page.set != GITHUB_SET {
            return false;
        }
        let key = |text: &str| analysis::tokens(&self.joined, text).pop();
        let names: HashSet<String> = std::iter::once(page.title.as_str())
            .chain(page.title.split_once('/').map(|(_, name)| name))
            .chain(page.aliases.iter().map(String::as_str))
            .filter_map(key)
            .collect();
        let told: HashSet<String> =
            analysis::tokens(&self.stemmed, page.description.as_deref().unwrap_or(""))
                .into_iter()
                .collect();
        (1..words.len()).any(|len| {
            words.windows(len).enumerate().any(|(start, span)| {
                if !key(&span.join(" ")).is_some_and(|span| names.contains(&span)) {
                    return false;
                }
                let rest: Vec<&str> = words[..start]
                    .iter()
                    .chain(&words[start + len..])
                    .map(String::as_str)
                    .filter(|word| !crate::is_function_word(word) && !ASKING_WORDS.contains(word))
                    .collect();
                let stems = analysis::tokens(&self.stemmed, &rest.join(" "));
                !stems.is_empty() && stems.iter().all(|stem| told.contains(stem))
            })
        })
    }
}

/// Names the repositories at `described` in `hits`, each with whether it
/// would have been left out otherwise ([`PageSearcher::repo_described`]),
/// when no page is called the whole query: each is asked for by its
/// name and what it is, as a film is by its title and year. When one is,
/// the query is its name, not a description: "react native" is React
/// Native, not facebook/react, "The library for web and native user
/// interfaces". The repositories are then left as they were.
pub(crate) fn name_described_repos(hits: &mut Vec<PageHit>, described: &[(usize, bool)]) {
    let called = hits.iter().any(|hit| hit.named);
    for &(at, left_out) in described.iter().rev() {
        if !called {
            let hit = &mut hits[at];
            name_by_alias(hit);
            hit.score *= REPO_WEIGHT;
            hit.whole = true;
        } else if left_out {
            hits.remove(at);
        }
    }
}
