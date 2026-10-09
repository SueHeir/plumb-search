//! Searches for the news itself: "news", "world news", "breaking news".
//!
//! Such a search names no site. Its words are in the domains of sites that
//! only share them (news.cn, news.by), while the big papers and agencies
//! people read for the news (nytimes.com, reuters.com) are called by their
//! own names. So the well-known sites that say they are news, in their
//! title or description or Wikidata's words, are ranked as sites of the
//! kind the query names, and the news words name no site.

use std::collections::HashSet;

use anyhow::Result;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::IndexRecordOption;
use tantivy::{DocAddress, Order, Term};

use crate::{
    normalize_country, normalize_text, schema, unit_or, Columns, Hit, RankConfig, Ranked,
    SearchOptions, SearchResults, Searcher, WELL_KNOWN_LINK_SCORE,
};

/// Words that, alone or together, ask for the news rather than name
/// anything. One of [`NEWS_WORDS`] must be among them.
const NEWS_QUERY_WORDS: &[&str] = &[
    "news",
    "headlines",
    "latest",
    "breaking",
    "top",
    "today",
    "todays",
    "current",
    "world",
    "international",
    "global",
    "live",
    "daily",
    "stories",
    "events",
];

/// The words that make a search of [`NEWS_QUERY_WORDS`] one for the news.
const NEWS_WORDS: &[&str] = &["news", "headlines"];

/// The words a site says it is news by, in its title, description or
/// Wikidata's description ("American daily newspaper", "international
/// news agency").
const SAYS_NEWS: &[&str] = &["news", "newspaper"];

/// How many of the best-linked sites that say they are news are looked at.
const NEWS_CANDIDATES: usize = 100;

/// Whether `query` asks only for the news: "news", "world news", "latest
/// headlines", "news today". "fox news" and "news.cn" name a site.
pub fn asks_only_for_news(query: &str) -> bool {
    if query.contains('.') {
        return false;
    }
    let text = normalize_text(query);
    let words: Vec<&str> = text.split_whitespace().collect();
    !words.is_empty()
        && words.iter().all(|word| NEWS_QUERY_WORDS.contains(word))
        && words.iter().any(|word| NEWS_WORDS.contains(word))
}

impl Searcher {
    /// For a query asking only for the news ([`asks_only_for_news`]), adds
    /// the well-known sites that say they are news to `results`, each
    /// scored as a site of the kind the query names (a full text match and
    /// [`RankConfig::kind_bonus`]), so popularity and the home country
    /// decide among them; the sites keep the better of the two scores.
    /// The news words name no site: news.cn is not what "news" is after.
    /// Off unless [`RankConfig::news_sites`].
    pub(crate) fn add_news_sites(
        &self,
        query: &str,
        results: &mut SearchResults,
        limit: usize,
        cfg: &RankConfig,
        options: &SearchOptions,
    ) -> Result<()> {
        if !cfg.news_sites || limit == 0 || !asks_only_for_news(query) {
            return Ok(());
        }
        let searcher = self.reader.searcher();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for field in [
            self.fields.title,
            self.fields.description,
            self.fields.about,
        ] {
            for word in SAYS_NEWS {
                clauses.push((
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(field, word),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
        }
        let found: Vec<DocAddress> = searcher
            .search(
                &BooleanQuery::new(clauses),
                &TopDocs::with_limit(NEWS_CANDIDATES)
                    .order_by_fast_field::<f64>(schema::LINK_SCORE, Order::Desc),
            )?
            .into_iter()
            .map(|(_, addr)| addr)
            .collect();
        let default = RankConfig::default();
        let alpha = unit_or(cfg.alpha, default.alpha);
        let country_boost = unit_or(cfg.country_boost, default.country_boost);
        let home = options.country.as_deref().and_then(normalize_country);
        let language = options.language.as_deref().and_then(crate::language_code);
        let mut columns: Vec<Option<Columns>> = Vec::new();
        for segment in searcher.segment_readers() {
            let fast = segment.fast_fields();
            columns.push(Some(Columns {
                link_scores: fast.f64(schema::LINK_SCORE)?,
                domains: fast.str(schema::DOMAIN)?,
                countries: fast.str(schema::COUNTRY)?,
                languages: fast.str(schema::LANGUAGE)?,
                adult: fast.u64(schema::ADULT)?,
            }));
        }
        let mut seen: HashSet<String> = HashSet::new();
        for hit in &mut results.hits {
            hit.named = false;
            seen.insert(hit.domain.clone());
        }
        let mut added: Vec<Hit> = Vec::new();
        for addr in found {
            let Some(column) = columns[addr.segment_ord as usize].as_ref() else {
                continue;
            };
            let link_score = column.link_scores.first(addr.doc_id).unwrap_or(0.0) as f32;
            if link_score < WELL_KNOWN_LINK_SCORE || options.safe.hides(column.adult(addr.doc_id)) {
                continue;
            }
            if let (Some(wanted), Some(site)) = (&language, column.language(addr.doc_id)) {
                if !plumb_core::language_fits(wanted, &site) {
                    continue;
                }
            }
            let country = column.country(addr.doc_id);
            let country_bonus = match (&home, &country) {
                (Some(home), Some(country)) if home == country => country_boost,
                (Some(_), Some(_)) if options.only_country => continue,
                (Some(_), Some(_)) => -country_boost,
                _ => 0.0,
            };
            let ranked = Ranked {
                addr,
                score: alpha * link_score + (1.0 - alpha) + cfg.kind_bonus + country_bonus,
                text_score: 1.0,
                placing_text_score: None,
                link_score,
                country,
                named: false,
                closeness: None,
                label_names_query: false,
                tie_break: (addr.segment_ord, u64::from(addr.doc_id)),
            };
            let (hit, _) = self.hit(&searcher, ranked)?;
            match results.hits.iter_mut().find(|h| h.domain == hit.domain) {
                Some(listed) if listed.score < hit.score => {
                    listed.score = hit.score;
                    listed.text_score = hit.text_score;
                }
                Some(_) => {}
                None if seen.insert(hit.domain.clone()) => added.push(hit),
                None => {}
            }
        }
        results.hits.extend(added);
        results.hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| b.link_score.total_cmp(&a.link_score))
                .then_with(|| a.domain.cmp(&b.domain))
        });
        results.hits.truncate(limit);
        // A link into news.cn's own search is not what "world news" wants.
        results.site_search = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn news_alone_is_asked_for() {
        for query in [
            "news",
            "News",
            "world news",
            "breaking news",
            "latest news today",
            "headlines",
            "today's news",
            "top stories news",
        ] {
            assert!(asks_only_for_news(query), "{query}");
        }
        for query in [
            "fox news",
            "news.cn",
            "today",
            "world",
            "top stories",
            "election news",
            "",
        ] {
            assert!(!asks_only_for_news(query), "{query}");
        }
    }
}
