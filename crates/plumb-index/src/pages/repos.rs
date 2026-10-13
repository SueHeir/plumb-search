//! Repositories asked for by their name and what they are: "frp reverse
//! proxy" for fatedier/frp, "A fast reverse proxy to help you expose a
//! local server behind a NAT or firewall to the internet".
//!
//! A repository's only key is its title, `owner/name`, so the searches by
//! keys in [`PageSearcher::search_once`] find one by its name only when the
//! query has its owner too. Here repositories are found by the words of
//! their names instead.

use std::collections::{HashMap, HashSet};

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

/// Most words of a part of the query taken for a repository's name. Names
/// are rarely longer, and a long query has many more parts than that,
/// each a search.
const NAME_WORDS: usize = 3;

/// The repositories a search found that the query may ask for by their
/// name and what they are ([`PageSearcher::repo_described`]), kept apart
/// until every page found is known.
#[derive(Default)]
pub(crate) struct DescribedRepos {
    /// The most popularity of a repository found called by each name, by
    /// the name's key.
    most_starred: HashMap<String, f32>,
    /// Where each described repository is among the hits, the key of the
    /// part of the query that names it, and whether it would have been
    /// left out otherwise.
    described: Vec<(usize, String, bool)>,
}

impl PageSearcher {
    /// The most starred repositories that each part of the query `words` of
    /// at most [`NAME_WORDS`] words may name, by the part's words or by the
    /// part written as one word ("adguard home" for AdGuardHome), each with
    /// whether the whole query found it. Of the whole query only the one
    /// word is searched: its words are searched already.
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
        let mut search = |called: Vec<(Occur, Box<dyn Query>)>, whole: bool| -> Result<()> {
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
            Ok(())
        };
        let joined = analysis::tokens(&self.joined, &words.join(" ")).pop();
        if let Some(key) = joined.filter(|_| words.len() > 1) {
            search(vec![(Occur::Should, term(&key))], true)?;
        }
        for len in 1..words.len().min(NAME_WORDS + 1) {
            for span in words.windows(len) {
                if span.iter().all(|word| {
                    crate::is_function_word(word) || ASKING_WORDS.contains(&word.as_str())
                }) {
                    continue;
                }
                let Some(key) = analysis::tokens(&self.joined, &span.join(" ")).pop() else {
                    continue;
                };
                let every_word = span.iter().map(|word| (Occur::Must, term(word))).collect();
                search(
                    vec![
                        (Occur::Should, term(&key)),
                        (Occur::Should, Box::new(BooleanQuery::new(every_word))),
                    ],
                    false,
                )?;
            }
        }
        Ok(found)
    }

    /// Notes the repository `page`, whose popularity is `popularity`, in
    /// `repos`, and gives the key of the part of the query `words` that
    /// calls it by its title, its name or another name when every other main
    /// word is a word of its description: "frp" of "frp reverse proxy" and
    /// "delta" of "delta git pager" (dandavison/delta, "A syntax-highlighting
    /// pager for git, diff, grep, and blame output"), not of "delta airlines
    /// flights". `None` for pages of other sets.
    pub(crate) fn repo_described(
        &self,
        page: &Page,
        popularity: f32,
        words: &[String],
        repos: &mut DescribedRepos,
    ) -> Option<String> {
        if page.set != GITHUB_SET {
            return None;
        }
        let key = |text: &str| analysis::tokens(&self.joined, text).pop();
        let names: HashSet<String> = std::iter::once(page.title.as_str())
            .chain(page.title.split_once('/').map(|(_, name)| name))
            .chain(page.aliases.iter().map(String::as_str))
            .filter_map(key)
            .collect();
        for name in &names {
            let most = repos.most_starred.entry(name.clone()).or_insert(popularity);
            *most = most.max(popularity);
        }
        let told: HashSet<String> =
            analysis::tokens(&self.stemmed, page.description.as_deref().unwrap_or(""))
                .into_iter()
                .collect();
        (1..words.len().min(NAME_WORDS + 1)).find_map(|len| {
            words.windows(len).enumerate().find_map(|(start, span)| {
                let span = key(&span.join(" ")).filter(|span| names.contains(span))?;
                let rest: Vec<&str> = words[..start]
                    .iter()
                    .chain(&words[start + len..])
                    .map(String::as_str)
                    .filter(|word| !crate::is_function_word(word) && !ASKING_WORDS.contains(word))
                    .collect();
                let stems = analysis::tokens(&self.stemmed, &rest.join(" "));
                (!stems.is_empty() && stems.iter().all(|stem| told.contains(stem))).then_some(span)
            })
        })
    }
}

impl DescribedRepos {
    /// Keeps in mind that the hit at `at` is a repository the query names
    /// by the part whose key is `key` and describes with the rest, and
    /// whether it would have been left out otherwise.
    pub(crate) fn keep(&mut self, at: usize, key: String, left_out: bool) {
        self.described.push((at, key, left_out));
    }

    /// Names the described repositories in `hits` when no page is called
    /// the whole query, each the most starred repository called by its
    /// name: each is asked for by its name and what it is, as a film is by
    /// its title and year. When a page is called the whole query, the query
    /// is its name, not a description: "react native" is React Native, not
    /// facebook/react, "The library for web and native user interfaces".
    /// And a name others share with a better starred repository names
    /// that one: "python examples" is not geekcomputers/Python, "My Python
    /// Examples", when people mean TheAlgorithms/Python by "python". The
    /// repositories not named are left as they were.
    pub(crate) fn name(self, hits: &mut Vec<PageHit>) {
        let called = hits.iter().any(|hit| hit.named);
        for (at, key, left_out) in self.described.into_iter().rev() {
            let best = self
                .most_starred
                .get(&key)
                .is_none_or(|most| hits[at].popularity >= *most);
            if !called && best {
                let hit = &mut hits[at];
                name_by_alias(hit);
                hit.score *= REPO_WEIGHT;
                hit.whole = true;
            } else if left_out {
                hits.remove(at);
            }
        }
    }
}
