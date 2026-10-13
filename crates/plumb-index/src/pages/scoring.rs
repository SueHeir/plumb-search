//! One search of the page index: the candidates a query finds and how well
//! it names each (the score the module documentation describes).

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{TantivyDocument, Term};

use super::docs::docs_asked;
use super::sites::base_title;
use super::{
    Page, PageHit, PageSearcher, ALIAS_MATCH, ASKING_WORDS, BOOKS_SET, CANDIDATES,
    DESCRIBED_MISSING_FROM, DESCRIBED_QUERY_WORDS, DESCRIBED_WORD, DOCS_SET, FILMS_SET, GITHUB_SET,
    LEAD_CANDIDATES, LEAD_MATCH, MUSIC_SET, OTHER_NAME_MATCH, PACKAGES_SET, PAPERS_SET,
    PAPER_TITLE_WORDS, PARTIAL_MATCH, PODCASTS_SET, POPULARITY_SCALE, POPULARITY_SHARE,
    QUESTION_QUERY_WORDS, QUESTION_SHARE, QUESTION_TITLE_SHARE, REFERENCE2_SET,
    REFERENCE_QUERY_WORDS, REPO_WEIGHT, SHELF_WEIGHT, SUBPAGES_SET, TITLE_INSIDE_CANDIDATES,
    WELL_KNOWN_PACKAGE,
};
use crate::analysis;

impl PageSearcher {
    pub(crate) fn search_once(
        &self,
        query: &str,
        limit: usize,
        scope: Option<&dyn Query>,
    ) -> Result<Vec<PageHit>> {
        let words = analysis::tokens(&self.words, query);
        let Some(joined) = analysis::tokens(&self.joined, query).pop() else {
            return Ok(Vec::new());
        };
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let searcher = self.reader.searcher();
        // Pages named by the whole query are searched apart, so pages that
        // merely have its words never crowd them out: the many Stack
        // Overflow questions on "google maps" are more read than the
        // article Google Maps.
        let named_by_query = TermQuery::new(
            Term::from_field_text(self.fields.keys, &joined),
            IndexRecordOption::Basic,
        );
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        // Pages with every word of the query.
        let every_word: Vec<(Occur, Box<dyn Query>)> = words
            .iter()
            .map(|word| {
                (
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.words, word),
                        IndexRecordOption::WithFreqs,
                    )) as Box<dyn Query>,
                )
            })
            .collect();
        clauses.push((Occur::Should, Box::new(BooleanQuery::new(every_word))));
        // Books, podcasts, songs and albums named by their title and what
        // they are: "dune book", "hardcore history podcast", "hey jude song".
        if let Some((title, last)) = query.trim().rsplit_once(char::is_whitespace) {
            if matches!(
                last.to_lowercase().as_str(),
                "book" | "novel" | "podcast" | "song" | "album"
            ) {
                let title_words: Vec<(Occur, Box<dyn Query>)> =
                    analysis::tokens(&self.words, title)
                        .iter()
                        .map(|word| {
                            (
                                Occur::Must,
                                Box::new(TermQuery::new(
                                    Term::from_field_text(self.fields.words, word),
                                    IndexRecordOption::Basic,
                                )) as Box<dyn Query>,
                            )
                        })
                        .collect();
                if !title_words.is_empty() {
                    clauses.push((Occur::Should, Box::new(BooleanQuery::new(title_words))));
                }
                if let Some(key) = analysis::tokens(&self.joined, title).pop() {
                    clauses.push((
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.keys, &key),
                            IndexRecordOption::Basic,
                        )),
                    ));
                }
            }
        }
        let query_words: HashSet<&str> = words.iter().map(String::as_str).collect();
        let by_popularity = || {
            TopDocs::with_limit(CANDIDATES)
                .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc)
        };
        let mut addresses: Vec<_> = searcher
            .search(
                self.language_query(scoped_query(&named_by_query, scope).as_ref())
                    .as_ref(),
                &by_popularity(),
            )?
            .into_iter()
            .map(|(_, address)| address)
            .collect();
        for (_, address) in searcher.search(
            self.language_query(scoped_query(&BooleanQuery::new(clauses), scope).as_ref())
                .as_ref(),
            &by_popularity(),
        )? {
            if !addresses.contains(&address) {
                addresses.push(address);
            }
        }
        // Books and papers named by their title and then their author:
        // "random forests breiman". Searched apart, the titles of two words
        // or more that start the query; a page found only so is kept only
        // when it is asked for so.
        let mut by_title_first = HashSet::new();
        // Films and shows by a title of one word too: "inception 2010".
        // Other pages found so must be films asked for so.
        let mut film_title_first = HashSet::new();
        for end in 1..words.len() {
            let Some(key) = analysis::tokens(&self.joined, &words[..end].join(" ")).pop() else {
                continue;
            };
            let named = TermQuery::new(
                Term::from_field_text(self.fields.keys, &key),
                IndexRecordOption::Basic,
            );
            for (_, address) in searcher.search(
                self.language_query(scoped_query(&named, scope).as_ref())
                    .as_ref(),
                &by_popularity(),
            )? {
                if !addresses.contains(&address) {
                    addresses.push(address);
                    by_title_first.insert(address);
                    if end == 1 {
                        film_title_first.insert(address);
                    }
                }
            }
        }
        // Packages, only ever found when the query asks for one: "serde
        // crate", "latest version of requests python".
        let package_query = plumb_core::packages::package_query(query);
        let package_key = package_query
            .as_ref()
            .and_then(|asked| analysis::tokens(&self.joined, &asked.name).pop());
        if let Some(key) = &package_key {
            let named = TermQuery::new(
                Term::from_field_text(self.fields.keys, key),
                IndexRecordOption::Basic,
            );
            for (_, address) in searcher.search(
                self.language_query(scoped_query(&named, scope).as_ref())
                    .as_ref(),
                &by_popularity(),
            )? {
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
        // Questions with most of the query's words, searched apart so they
        // never crowd out pages the query names.
        let stems = self.question_words(query);
        if stems.len() >= QUESTION_QUERY_WORDS.min(REFERENCE_QUERY_WORDS) {
            let needed = (stems.len() as f32 * QUESTION_SHARE).ceil() as usize;
            let most_words = BooleanQuery::with_minimum_required_clauses(
                stems
                    .iter()
                    .map(|stem| {
                        (
                            Occur::Should,
                            Box::new(TermQuery::new(
                                Term::from_field_text(self.fields.topic, stem),
                                IndexRecordOption::Basic,
                            )) as Box<dyn Query>,
                        )
                    })
                    .collect(),
                needed,
            );
            for (_, address) in searcher.search(
                self.language_query(scoped_query(&most_words, scope).as_ref())
                    .as_ref(),
                &by_popularity(),
            )? {
                // Found by most of the query's words, not only by a title
                // that starts it: "react usestate hook" is the docs page
                // "React useState" and more of its words.
                by_title_first.remove(&address);
                film_title_first.remove(&address);
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
        // Articles that say what the query asks for, in their names or
        // description: "queen album" finds Queen (album), "mckinley
        // president" William McKinley.
        let described = self.described_words(query);
        if described.len() >= DESCRIBED_QUERY_WORDS {
            let mut needed = vec![described.len()];
            if described.len() >= DESCRIBED_MISSING_FROM {
                needed.push(described.len() - 1);
            }
            for needed in needed {
                let most_words = BooleanQuery::with_minimum_required_clauses(
                    described
                        .iter()
                        .map(|stem| {
                            (
                                Occur::Should,
                                Box::new(TermQuery::new(
                                    Term::from_field_text(self.fields.about, stem),
                                    IndexRecordOption::Basic,
                                )) as Box<dyn Query>,
                            )
                        })
                        .collect(),
                    needed,
                );
                for (_, address) in searcher.search(
                    self.language_query(scoped_query(&most_words, scope).as_ref())
                        .as_ref(),
                    &by_popularity(),
                )? {
                    if !addresses.contains(&address) {
                        addresses.push(address);
                    }
                }
            }
        }
        // Articles whose lead, names and description have every such word:
        // "triassic jurassic cretaceous" finds Mesozoic, "manubrium
        // sternum" Sternum. Ranked by how well the leads match, not by
        // how often the articles are read.
        let mut lead_scores: HashMap<tantivy::DocAddress, f32> = HashMap::new();
        if described.len() >= DESCRIBED_QUERY_WORDS {
            let every_word = BooleanQuery::new(
                described
                    .iter()
                    .map(|stem| {
                        let either: Vec<(Occur, Box<dyn Query>)> = [
                            (self.fields.lead, IndexRecordOption::WithFreqs),
                            (self.fields.about, IndexRecordOption::Basic),
                        ]
                        .into_iter()
                        .map(|(field, option)| {
                            (
                                Occur::Should,
                                Box::new(TermQuery::new(Term::from_field_text(field, stem), option))
                                    as Box<dyn Query>,
                            )
                        })
                        .collect();
                        (
                            Occur::Must,
                            Box::new(BooleanQuery::new(either)) as Box<dyn Query>,
                        )
                    })
                    .collect(),
            );
            for (score, address) in searcher.search(
                self.language_query(scoped_query(&every_word, scope).as_ref())
                    .as_ref(),
                &TopDocs::with_limit(LEAD_CANDIDATES).order_by_score(),
            )? {
                lead_scores.insert(address, score);
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
        let best_lead = lead_scores.values().copied().fold(0.0f32, f32::max);
        // The query's rarest word that some question has: what it is
        // about. A question without it has only the asking words ("how to
        // get rid of aphids" found "How do I get rid of my bounty?"). A
        // word the pages know that no question has is rarer still: no
        // question asks about it, so none is found. A word nothing knows
        // is likely a typo, and is passed over.
        let mut topic_word: Option<(u64, &String)> = None;
        if stems.len() >= QUESTION_QUERY_WORDS.min(REFERENCE_QUERY_WORDS) {
            for stem in &stems {
                let found = searcher.doc_freq(&Term::from_field_text(self.fields.topic, stem))?;
                if (found > 0 || self.knows_stem(query, stem)?)
                    && topic_word.is_none_or(|(least, _)| found < least)
                {
                    topic_word = Some((found, stem));
                }
            }
        }
        let topic_word = topic_word.map(|(_, stem)| stem.as_str());
        // Articles whose title is some of the query and the rest says more
        // about it: "titanic sinking", "radium discovery marie curie",
        // "catholic homily". They lack the query's other words, so nothing
        // above finds them; [`PageSearcher::name_match`] scores them by the
        // share of the query their title is.
        let mut by_title_inside = HashSet::new();
        // Other pages found only so are left out. This comes after every
        // other way pages are found, so "only so" means it.
        let mut only_inside = HashSet::new();
        let mut inside_keys = HashSet::new();
        for start in 0..words.len() {
            for end in start + 1..=words.len() {
                let span = &words[start..end];
                if span.len() == words.len()
                    || span.iter().all(|word| {
                        crate::is_function_word(word) || ASKING_WORDS.contains(&word.as_str())
                    })
                {
                    continue;
                }
                let Some(key) = analysis::tokens(&self.joined, &span.join(" ")).pop() else {
                    continue;
                };
                let named = TermQuery::new(
                    Term::from_field_text(self.fields.keys, &key),
                    IndexRecordOption::Basic,
                );
                inside_keys.insert(key);
                let most_read = TopDocs::with_limit(TITLE_INSIDE_CANDIDATES)
                    .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc);
                for (_, address) in searcher.search(
                    self.language_query(scoped_query(&named, scope).as_ref())
                        .as_ref(),
                    &most_read,
                )? {
                    if !addresses.contains(&address) {
                        addresses.push(address);
                        only_inside.insert(address);
                    }
                    by_title_inside.insert(address);
                }
            }
        }
        let mut hits = Vec::new();
        for address in addresses {
            let document: TantivyDocument = searcher.doc(address)?;
            let Some(stored) = document
                .get_first(self.fields.page)
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let page: Page = serde_json::from_str(stored)?;
            let popularity = document
                .get_first(self.fields.popularity)
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as f32
                / POPULARITY_SCALE;
            if page.set == PACKAGES_SET {
                let asked = match (&package_query, &package_key) {
                    (Some(asked), Some(key)) => self.package_match(&page, asked, key, popularity),
                    _ => false,
                };
                if asked {
                    hits.push(PageHit {
                        score: ALIAS_MATCH
                            * (1.0 - POPULARITY_SHARE + POPULARITY_SHARE * popularity),
                        page,
                        named: true,
                        popularity,
                        whole: false,
                        learned: None,
                    });
                }
                continue;
            }
            let asked_as_film = self.film_match(&page, &words);
            let asked_by_title = asked_as_film || self.book_match(&page, &words);
            // Songs and albums share their titles with too much ("Dead
            // Sea", "Notion", "Lord of the Flies"): one is only listed
            // when asked for by its artist or as a song or album.
            if page.set == MUSIC_SET && !asked_by_title {
                continue;
            }
            // Its title in full, or another name: "queen tour" is not
            // about Queen (album).
            let article_inside = page.is_article()
                && by_title_inside.contains(&address)
                && std::iter::once(&page.title)
                    .chain(&page.aliases)
                    .any(|name| {
                        analysis::tokens(&self.joined, name)
                            .pop()
                            .is_some_and(|key| inside_keys.contains(&key))
                    });
            if !article_inside
                && (by_title_first.contains(&address) && !asked_by_title
                    || film_title_first.contains(&address) && !asked_as_film)
            {
                continue;
            }
            if only_inside.contains(&address) && !article_inside && !asked_by_title {
                continue;
            }
            let (mut name, mut named) = self.name_match(&page, query, &joined, &query_words);
            let mut whole = false;
            if asked_by_title {
                (name, named, whole) = (name.max(ALIAS_MATCH), true, true);
            } else if !named && (page.set != DOCS_SET || docs_asked(&page, query)) {
                let (question, asked) = self.question_match(&page, &stems, topic_word);
                name = name.max(question);
                whole = asked;
            }
            if !named && !whole {
                name = name.max(self.described_match(&page, &described));
                if let Some(&lead) = lead_scores.get(&address).filter(|_| page.is_article()) {
                    name = name
                        .max(LEAD_MATCH * (0.5 + 0.5 * lead / best_lead.max(f32::MIN_POSITIVE)));
                }
            }
            if name <= 0.0 {
                continue;
            }
            let mut score = name * (1.0 - POPULARITY_SHARE + POPULARITY_SHARE * popularity);
            if !page.may_lead() {
                score *= SHELF_WEIGHT;
            } else if page.set == GITHUB_SET {
                score *= REPO_WEIGHT;
            }
            hits.push(PageHit {
                page,
                score,
                named,
                popularity,
                whole,
                learned: None,
            });
        }
        match words.as_slice() {
            [word] => self.name_by_surname(word, &joined, &mut hits),
            [title, name @ ..] => self.name_by_title(title, name, &joined, &mut hits),
            [] => {}
        }
        let mut hits = fold_films(hits);
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        truncate_keeping_inner_pages(&mut hits, limit);
        Ok(hits)
    }

    /// Whether the query `words` are a film's or show's title followed by
    /// words of its description (its year, director or cast) or by what it
    /// is: "inception 2010", "inception christopher nolan", "dune movie",
    /// "breaking bad tv show".
    fn film_match(&self, page: &Page, words: &[String]) -> bool {
        if page.set != FILMS_SET {
            return false;
        }
        let kinds = if page.is_show() {
            plumb_core::films::SHOW_WORDS
        } else {
            plumb_core::films::FILM_WORDS
        };
        let title = analysis::tokens(&self.words, &page.title);
        let rest = match words.strip_prefix(title.as_slice()) {
            Some(rest) if !title.is_empty() && !rest.is_empty() => rest,
            _ => return false,
        };
        let told: HashSet<String> =
            analysis::tokens(&self.words, page.description.as_deref().unwrap_or(""))
                .into_iter()
                .collect();
        rest.iter()
            .all(|word| told.contains(word) || kinds.contains(&word.as_str()))
    }

    /// Whether the package `page` is the one `asked` for, whose name's key
    /// is `key`: it is called so (or so for short: "gin" for
    /// github.com/gin-gonic/gin), on a registry the query names, and well
    /// known unless the query names a registry rather than a language.
    fn package_match(
        &self,
        page: &Page,
        asked: &plumb_core::packages::PackageQuery,
        key: &str,
        popularity: f32,
    ) -> bool {
        let Some(registry) = page.registry() else {
            return false;
        };
        let called = std::iter::once(&page.title)
            .chain(&page.aliases)
            .any(|name| analysis::tokens(&self.joined, name).pop().as_deref() == Some(key));
        called && asked.wants(registry.key) && (asked.surely || popularity >= WELL_KNOWN_PACKAGE)
    }

    /// The different stemmed words of `query`, as questions are searched.
    pub(crate) fn question_words(&self, query: &str) -> Vec<String> {
        let mut stems = analysis::tokens(&self.stemmed, query);
        let mut seen = HashSet::new();
        stems.retain(|stem| seen.insert(stem.clone()));
        stems
    }

    /// Whether a word of `query` whose stem is `stem` is one the pages
    /// know ([`PageSearcher::knows_word`]), as typed or as its stem:
    /// "aphids" is known by the names that say "aphid".
    fn knows_stem(&self, query: &str, stem: &str) -> Result<bool> {
        for word in analysis::tokens(&self.words, query) {
            let stems = analysis::tokens(&self.stemmed, &word);
            if stems.first().map(String::as_str) == Some(stem)
                && (self.knows_word(&word)? || self.knows_word(stem)?)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The different stemmed words of `query` without the words that only
    /// ask ([`ASKING_WORDS`]), as articles are matched on what they say of
    /// themselves: "resin" of "what does resin mean".
    pub(crate) fn described_words(&self, query: &str) -> Vec<String> {
        let asked: Vec<String> = analysis::tokens(&self.words, query)
            .into_iter()
            .filter(|word| !ASKING_WORDS.contains(&word.as_str()))
            .collect();
        self.question_words(&asked.join(" "))
    }

    /// How well a Wikipedia article covers the query's words `stems`
    /// ([`PageSearcher::described_words`]) with one of its names and what it
    /// says of itself ([`Page::about`], and its lead): 0 unless a name has some of them
    /// and the article all of them, or all but one when the name is whole
    /// in the query and the query has [`DESCRIBED_MISSING_FROM`] words.
    /// Otherwise [`PARTIAL_MATCH`] times the share of the query in the
    /// name, a word the article only says counting [`DESCRIBED_WORD`], and
    /// halfway down by the share of the name the query lacks: "queen
    /// album" covers Queen (album) by 0.45.
    pub(crate) fn described_match(&self, page: &Page, stems: &[String]) -> f32 {
        if stems.len() < DESCRIBED_QUERY_WORDS {
            return 0.0;
        }
        let Some(about) = page.about() else {
            return 0.0;
        };
        let mut about: HashSet<String> = analysis::tokens(&self.stemmed, &about)
            .into_iter()
            .collect();
        if let Some(lead) = &page.lead {
            about.extend(analysis::tokens(&self.stemmed, lead));
        }
        let missing = stems.iter().filter(|stem| !about.contains(*stem)).count();
        let mut best = 0.0f32;
        for name in
            std::iter::once(base_title(&page.title)).chain(page.aliases.iter().map(String::as_str))
        {
            let name: HashSet<String> = analysis::tokens(&self.stemmed, name).into_iter().collect();
            let in_name = stems.iter().filter(|stem| name.contains(*stem)).count();
            if in_name == 0 {
                continue;
            }
            let whole_name = name.iter().all(|word| stems.contains(word));
            if missing > 1 || missing == 1 && !(whole_name && stems.len() >= DESCRIBED_MISSING_FROM)
            {
                continue;
            }
            let said = stems.len() - in_name - missing;
            let query_share = (in_name as f32 + DESCRIBED_WORD * said as f32) / stems.len() as f32;
            let name_share =
                name.iter().filter(|word| stems.contains(*word)).count() as f32 / name.len() as f32;
            best = best.max(PARTIAL_MATCH * query_share * (0.5 + 0.5 * name_share));
        }
        best
    }

    /// How well a question's words cover the query's stemmed words
    /// `stems`: [`PARTIAL_MATCH`] times the share they have, when that is
    /// at least [`QUESTION_SHARE`] of at least [`QUESTION_QUERY_WORDS`].
    /// And whether the query asks the question as a whole: it has all of
    /// the query's words, and the query at least [`QUESTION_TITLE_SHARE`]
    /// of its title's. A question without `topic_word`, the query's rarest
    /// word, matches not at all. The question's words are its title and
    /// tags, or the title of a duplicate of it and its tags, whichever
    /// covers the query best ([`Page::asked_as`]).
    pub(crate) fn question_match(
        &self,
        page: &Page,
        stems: &[String],
        topic_word: Option<&str>,
    ) -> (f32, bool) {
        let fewest = if page.is_site_page() {
            REFERENCE_QUERY_WORDS
        } else {
            QUESTION_QUERY_WORDS
        };
        if stems.len() < fewest {
            return (0.0, false);
        }
        let mut best = (0.0f32, false);
        for (title, topic) in page.asked_as() {
            let words: HashSet<String> = analysis::tokens(&self.stemmed, &topic)
                .into_iter()
                .collect();
            if topic_word.is_some_and(|word| !words.contains(word)) {
                continue;
            }
            let share = stems.iter().filter(|stem| words.contains(*stem)).count() as f32
                / stems.len() as f32;
            if share < QUESTION_SHARE {
                continue;
            }
            let title: HashSet<String> =
                analysis::tokens(&self.stemmed, title).into_iter().collect();
            let asked = share >= 1.0
                && !title.is_empty()
                && title.iter().filter(|word| stems.contains(word)).count() as f32
                    >= QUESTION_TITLE_SHARE * title.len() as f32;
            let found = (PARTIAL_MATCH * share, asked);
            if found.0 > best.0 || found.0 == best.0 && found.1 && !best.1 {
                best = found;
            }
        }
        best
    }

    /// Whether the query `words` are a book's, podcast's or paper's title
    /// followed by words of its author's name or by what it is ("book",
    /// "novel", "podcast", "paper"): "dune frank herbert", "the great gatsby
    /// book", "hardcore history podcast", "random forests breiman". A
    /// paper's year and venue may follow too, and a paper's whole title of
    /// [`PAPER_TITLE_WORDS`] words or more asks for it alone.
    pub(crate) fn book_match(&self, page: &Page, words: &[String]) -> bool {
        let (byline, kinds): (&str, &[&str]) = match page.set.as_str() {
            BOOKS_SET => ("Book by ", &["book", "novel"]),
            PODCASTS_SET => ("Podcast by ", &["podcast"]),
            PAPERS_SET => ("Paper by ", &["paper"]),
            MUSIC_SET if page.is_song() => ("Song by ", &["song"]),
            MUSIC_SET => ("Album by ", &["album"]),
            _ => return false,
        };
        let by = page
            .description
            .as_deref()
            .and_then(|d| d.strip_prefix(byline))
            .unwrap_or("");
        // "Book by AUTHOR, YEAR", "Podcast by AUTHOR · CATEGORY", "Paper by
        // AUTHOR et al., YEAR, VENUE".
        let author = if page.set == PAPERS_SET {
            by.split(", ")
                .collect::<Vec<_>>()
                .join(" ")
                .replace(" et al.", "")
        } else {
            let by = by.split(" · ").next().unwrap_or(by);
            by.rsplit_once(", ")
                .map_or(by, |(name, _)| name)
                .to_string()
        };
        // Asked for artist first, the main artist alone: "kanye west drive
        // slow" for "Kanye West feat. Paul Wall & GLC".
        let main = [" feat. ", " ft. "]
            .iter()
            .find_map(|joiner| author.split_once(joiner))
            .map_or(author.as_str(), |(main, _)| main);
        let artist = analysis::tokens(&self.words, main);
        let author: HashSet<String> = analysis::tokens(&self.words, &author).into_iter().collect();
        // The title, or the title without its subtitle: "Frankenstein" for
        // "Frankenstein; or, The Modern Prometheus".
        let short = page.title.split([':', ';']).next().unwrap_or("");
        [page.title.as_str(), short]
            .into_iter()
            .enumerate()
            .any(|(i, title)| {
                let title = analysis::tokens(&self.words, title);
                // Songs and albums are as often asked for artist first:
                // "vampire weekend step", "nirvana nevermind album",
                // "beatles hey jude".
                let unthe = match artist.as_slice() {
                    [the, rest @ ..] if the == "the" && !rest.is_empty() => rest,
                    artist => artist,
                };
                if page.set == MUSIC_SET && !artist.is_empty() && !title.is_empty() {
                    if let Some(rest) = words
                        .strip_prefix(artist.as_slice())
                        .or_else(|| words.strip_prefix(unthe))
                        .and_then(|rest| rest.strip_prefix(title.as_slice()))
                    {
                        if rest.is_empty()
                            || matches!(rest, [word] if kinds.contains(&word.as_str()))
                        {
                            return true;
                        }
                    }
                }
                let rest = match words.strip_prefix(title.as_slice()) {
                    Some([]) => {
                        return page.set == PAPERS_SET && i == 0 && title.len() >= PAPER_TITLE_WORDS
                    }
                    Some(rest) if !title.is_empty() => rest,
                    _ => return false,
                };
                // "hey jude by the beatles".
                let rest = match rest {
                    [by, rest @ ..] if by == "by" && !rest.is_empty() => rest,
                    rest => rest,
                };
                rest.iter().all(|word| author.contains(word))
                    || matches!(rest, [word] if kinds.contains(&word.as_str()))
            })
            || (page.set == PODCASTS_SET && self.podcast_named(page, words))
    }

    /// Whether `words` are the end of a podcast's title, two words or more,
    /// and "podcast": "hardcore history podcast" for "Dan Carlin's Hardcore
    /// History".
    fn podcast_named(&self, page: &Page, words: &[String]) -> bool {
        let Some((last, named)) = words.split_last() else {
            return false;
        };
        let title = analysis::tokens(&self.words, &page.title);
        last == "podcast" && named.len() >= 2 && title.ends_with(named)
    }

    pub(crate) fn name_match(
        &self,
        page: &Page,
        raw_query: &str,
        joined: &str,
        query: &HashSet<&str>,
    ) -> (f32, bool) {
        let key = |text: &str| {
            analysis::tokens(&self.joined, text)
                .pop()
                .unwrap_or_default()
        };
        let spelled = |text: &str| plumb_core::collapse_whitespace(text).to_lowercase();
        // A docs page is named only by one of its names in full: its title
        // alone ("Glossary") or part of it says too little.
        if page.set == DOCS_SET {
            let named = page.aliases.iter().any(|alias| key(alias) == joined);
            return if named {
                (ALIAS_MATCH, true)
            } else {
                (0.0, false)
            };
        }
        if spelled(&page.title) == spelled(raw_query) {
            return (1.0, true);
        }
        // A reference page is named only by its whole title: part of it
        // ("Stool") says too little, and it is found by its words
        // otherwise ([`Self::question_match`]).
        if page.is_site_page() {
            return if key(&page.title) == joined {
                (ALIAS_MATCH, true)
            } else {
                (0.0, false)
            };
        }
        // "Mozart (film)" and "Mozart!" are no better a match for "mozart"
        // than the redirect "Mozart" to "Wolfgang Amadeus Mozart":
        // popularity decides between them.
        if key(&page.title) == joined
            || key(base_title(&page.title)) == joined
            || page.aliases.iter().any(|alias| key(alias) == joined)
        {
            return (ALIAS_MATCH, true);
        }
        let mut best = if page.names.iter().any(|name| key(name) == joined) {
            OTHER_NAME_MATCH
        } else {
            0.0f32
        };
        for name in
            std::iter::once(base_title(&page.title)).chain(page.aliases.iter().map(String::as_str))
        {
            let words: HashSet<String> = analysis::tokens(&self.words, name).into_iter().collect();
            if words.is_empty() {
                continue;
            }
            let shared = words
                .iter()
                .filter(|w| query.contains(w.as_ref() as &str))
                .count();
            // The query names the page and says more ("marie curie
            // radium"), or names part of it ("curie" for "Marie Curie").
            if shared == words.len() || shared == query.len() {
                let share = shared as f32 / words.len().max(query.len()) as f32;
                best = best.max(PARTIAL_MATCH * share);
            }
        }
        (best, false)
    }

    /// Names the person a query of one `word` (whose key is `joined`) is
    /// the surname of when Wikipedia has no redirect from it: "thatcher"
    /// is Margaret Thatcher and "kafka" Franz Kafka, not the film Kafka.
    /// Only when they are read [`super::SURNAME_MARGIN`] times as much as
    /// every other article with the word in its title ("darwin" is a city
    /// too, "ford" two presidents), and no article is called the word
    /// ("swift", "clinton") nor a company's site ("tesla", "hilton").
    fn name_by_surname(&self, word: &str, joined: &str, hits: &mut [PageHit]) {
        let called =
            |name: &str| analysis::tokens(&self.joined, name).pop().as_deref() == Some(joined);
        let articles = || {
            hits.iter()
                .enumerate()
                .filter(|(_, hit)| hit.page.is_article())
        };
        let taken = articles().any(|(_, hit)| {
            std::iter::once(&hit.page.title)
                .chain(&hit.page.aliases)
                .any(|name| called(name))
                || super::sites::describes_an_organization(hit.page.description.as_deref())
                    && hit
                        .page
                        .site
                        .as_deref()
                        .and_then(|site| site.split('.').next())
                        .is_some_and(called)
        });
        let titled = |hit: &PageHit| analysis::tokens(&self.words, &hit.page.title);
        let Some((best, views)) = articles()
            .filter(|(_, hit)| {
                !hit.named
                    && hit.page.is_person()
                    && titled(hit).last().map(String::as_str) == Some(word)
            })
            .max_by_key(|(_, hit)| hit.page.views)
            .map(|(at, hit)| (at, hit.page.views))
        else {
            return;
        };
        if taken
            || articles().any(|(at, hit)| {
                at != best
                    && hit.page.views * super::SURNAME_MARGIN > views
                    && titled(hit).iter().any(|title_word| title_word == word)
            })
        {
            return;
        }
        name_by_alias(&mut hits[best]);
    }

    /// Names the person a `title` and words of their `name` call when no
    /// article is called the whole query (whose key is `joined`): "sir
    /// isaac newton" is Isaac Newton, "president obama" Barack Obama,
    /// "princess diana" Diana, Princess of Wales, not the song. Every word
    /// of the name must be in the person's title or another name, and an
    /// office ([`super::OFFICES`]) in their title or description. Only
    /// when they are read [`super::SURNAME_MARGIN`] times as much as every
    /// other article that fits: "president bush" is two presidents.
    fn name_by_title(&self, title: &str, name: &[String], joined: &str, hits: &mut [PageHit]) {
        let office = super::OFFICES.contains(&title);
        if !office && !super::HONORIFICS.contains(&title) {
            return;
        }
        let words = |text: &str| analysis::tokens(&self.words, text);
        let fits = |page: &Page| {
            let names: HashSet<String> = std::iter::once(&page.title)
                .chain(&page.aliases)
                .flat_map(|text| words(text))
                .collect();
            let said = || {
                words(&page.title)
                    .into_iter()
                    .chain(words(page.description.as_deref().unwrap_or("")))
                    .any(|word| word == title)
            };
            page.is_article() && name.iter().all(|word| names.contains(word)) && (!office || said())
        };
        let called =
            |text: &str| analysis::tokens(&self.joined, text).pop().as_deref() == Some(joined);
        if hits.iter().any(|hit| {
            hit.page.is_article()
                && std::iter::once(&hit.page.title)
                    .chain(&hit.page.aliases)
                    .any(|text| called(text))
        }) {
            return;
        }
        let Some((best, views)) = hits
            .iter()
            .enumerate()
            .filter(|(_, hit)| !hit.named && hit.page.is_person() && fits(&hit.page))
            .max_by_key(|(_, hit)| hit.page.views)
            .map(|(at, hit)| (at, hit.page.views))
        else {
            return;
        };
        if hits.iter().enumerate().any(|(at, hit)| {
            at != best && hit.page.views * super::SURNAME_MARGIN > views && fits(&hit.page)
        }) {
            return;
        }
        name_by_alias(&mut hits[best]);
    }
}

/// Marks `hit` named by the query and scores it as if by an alias.
fn name_by_alias(hit: &mut PageHit) {
    hit.named = true;
    hit.score = ALIAS_MATCH * (1.0 - POPULARITY_SHARE + POPULARITY_SHARE * hit.popularity);
}

/// Folds the films and shows in `hits` that English Wikipedia has an
/// article on into that article: such a film is listed only when the query
/// asks for it ([`PageHit::whole`]: "inception 2010"), and then as its
/// article when that was found too, which the query then asks for. The
/// article alone covers its title.
fn fold_films(hits: Vec<PageHit>) -> Vec<PageHit> {
    let (films, mut kept): (Vec<PageHit>, Vec<PageHit>) = hits
        .into_iter()
        .partition(|hit| hit.page.is_film_with_article());
    for film in films.into_iter().filter(|film| film.whole) {
        match kept
            .iter_mut()
            .find(|hit| hit.page.is_article() && hit.page.item == film.page.item)
        {
            Some(article) => {
                article.named = true;
                article.whole = true;
                article.score = article.score.max(film.score);
            }
            None => kept.push(film),
        }
    }
    kept
}

/// Cuts `hits`, best first, to `limit`, keeping the best docs page and the
/// best subpage/reference page among them: the many Stack Overflow questions with the
/// words of "javascript array sort" are more read than MDN's page on it,
/// and would crowd it out.
pub(crate) fn truncate_keeping_inner_pages(hits: &mut Vec<PageHit>, limit: usize) {
    let kept = [DOCS_SET, SUBPAGES_SET, REFERENCE2_SET];
    for set in kept {
        if hits.len() <= limit || limit == 0 || hits[..limit].iter().any(|h| h.page.set == set) {
            continue;
        }
        let Some(best) = hits.iter().position(|h| h.page.set == set) else {
            continue;
        };
        // In place of the last hit that is not one kept already.
        let Some(last) = (0..limit)
            .rev()
            .find(|&i| !kept.contains(&hits[i].page.set.as_str()))
        else {
            continue;
        };
        let hit = hits.remove(best);
        hits.insert(last, hit);
    }
    hits.truncate(limit);
}

pub(crate) fn scoped_query(query: &dyn Query, scope: Option<&dyn Query>) -> Box<dyn Query> {
    match scope {
        Some(scope) => Box::new(BooleanQuery::new(vec![
            (Occur::Must, query.box_clone()),
            (Occur::Must, scope.box_clone()),
        ])),
        None => query.box_clone(),
    }
}
