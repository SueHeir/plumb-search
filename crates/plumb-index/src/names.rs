//! Which site a query names: by its label, its official and linked names,
//! a typed hostname, or the kind of thing it says it is.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use plumb_core::{domain_label, is_function_word, kind_key, normalize_text, registrable_domain};
use tantivy::postings::Postings;
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{DocAddress, DocSet, TantivyDocument, Term, TERMINATED};

use crate::query::{matching_docs, ParsedQuery, COMPARISON_WORDS};
use crate::rank::{domain_of, link_scores};
use crate::{
    analysis, spell, RankConfig, Searcher, FILLER_WORDS, LINK_NAME_SHARE, MAX_QUERY_WORDS,
};

/// Words that say what someone wants from a site rather than which site:
/// "paypal login", "postgres docs", "usps tracking". Look-alikes put them
/// in their domains (paypal-login.us), and big hosts match them in their
/// link text (github.com for "docs"), so an exact indexed name before them
/// can retain its navigation score. Substantive tasks and qualifiers stay
/// in the subject and must contribute their own query evidence.
const INTENT_WORDS: &[&str] = &[
    "login",
    "log in",
    "logon",
    "log on",
    "signin",
    "sign in",
    "sign on",
    "account",
    "my account",
    "support",
    "help",
    "help center",
    "customer service",
    "contact",
    "docs",
    "web docs",
    "documentation",
    "official site",
    "official website",
    "website",
    "homepage",
    "home page",
    "download",
    "portal",
    "tracking",
    "check in",
    "careers",
    "investor relations",
];

/// A link text more sites than this are linked with names none of them,
/// and is not read further.
const MAX_LINK_NAME_DOCS: u32 = 1_000;

impl Searcher {
    /// Whether anything of the site's own says what it is: a title, a
    /// description or Wikidata's words.
    pub(crate) fn says_what_it_is(
        &self,
        searcher: &tantivy::Searcher,
        addr: DocAddress,
    ) -> Result<bool> {
        let segment = searcher.segment_reader(addr.segment_ord);
        for field in [
            self.fields.title,
            self.fields.description,
            self.fields.about,
        ] {
            if let Some(norms) = segment.fieldnorms_readers().get_field(field)? {
                if norms.fieldnorm(addr.doc_id) > 0 {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Whether the site is the official site of a Wikidata item.
    pub(crate) fn is_official(
        &self,
        searcher: &tantivy::Searcher,
        addr: DocAddress,
    ) -> Result<bool> {
        let segment = searcher.segment_reader(addr.segment_ord);
        Ok(
            match segment.fieldnorms_readers().get_field(self.fields.about)? {
                Some(norms) => norms.fieldnorm(addr.doc_id) > 0,
                None => false,
            },
        )
    }

    /// Whether people know the site by its name: it is well linked, the
    /// official site of a Wikidata item, or has other names. A domain that
    /// only spells some words (vacationrentals.com for "vacation rentals")
    /// is not.
    pub(crate) fn known_by_name(
        &self,
        searcher: &tantivy::Searcher,
        addr: DocAddress,
    ) -> Result<bool> {
        Ok(
            crate::rank::best_link_score(searcher, &HashSet::from([addr]))
                >= crate::WELL_KNOWN_LINK_SCORE
                || self.is_official(searcher, addr)?
                || self.has_aliases(searcher, addr)?,
        )
    }

    /// Whether the site has other names (from Wikidata or a redirect),
    /// which stand in for a missing title.
    pub(crate) fn has_aliases(
        &self,
        searcher: &tantivy::Searcher,
        addr: DocAddress,
    ) -> Result<bool> {
        let segment = searcher.segment_reader(addr.segment_ord);
        Ok(
            match segment
                .fieldnorms_readers()
                .get_field(self.fields.aliases)?
            {
                Some(norms) => norms.fieldnorm(addr.doc_id) > 0,
                None => false,
            },
        )
    }

    /// Leaves out of `names` the sites the query's first words name only by
    /// a name they don't call themselves by ([`Searcher::calls_itself`]),
    /// when an official site better known than they are has it as its
    /// domain label. Such a site borrowed the name: holden.com.au's homepage
    /// gives "Facebook" as its og:site_name, and sites built with Laravel
    /// keep its default name "Laravel", so "facebook" named holden.com.au in
    /// full next to facebook.com, and "spotify premium price" named okta.com
    /// by its first word. A site that calls itself by the name keeps it:
    /// deltafaucet.com, "Delta Faucet", for "delta", and x.com, "formerly
    /// Twitter", for "twitter".
    pub(crate) fn drop_borrowed_names(
        &self,
        searcher: &tantivy::Searcher,
        query: &ParsedQuery,
        names: &mut HashMap<DocAddress, NameMatch>,
    ) -> Result<()> {
        for (key, words) in &query.leading {
            let named: HashSet<DocAddress> = names
                .iter()
                .filter(|(_, name)| name.words() == *words && !name.typed)
                .map(|(&addr, _)| addr)
                .collect();
            let scores = link_scores(searcher, &named);
            let own_label = |addr| {
                domain_of(searcher, addr).is_some_and(|domain| domain_label(&domain) == *key)
            };
            let mut owner = 0.0;
            for &(score, addr) in &scores {
                if score > owner && own_label(addr) && self.is_official(searcher, addr)? {
                    owner = score;
                }
            }
            for (score, addr) in scores {
                if score < owner && !own_label(addr) && !self.calls_itself(searcher, addr, key)? {
                    names.remove(&addr);
                }
            }
        }
        Ok(())
    }

    /// Whether words that, joined, are `key` are in the site's title or,
    /// for an official site, in its description, where Wikipedia's words
    /// for it go: x.com is "formerly known as Twitter", meta.com "formerly
    /// Facebook, Inc.". Another site's description can name the brand in
    /// passing ("Follow us on Instagram").
    fn calls_itself(
        &self,
        searcher: &tantivy::Searcher,
        addr: DocAddress,
        key: &str,
    ) -> Result<bool> {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let has = |field| {
            doc.get_all(field)
                .filter_map(|value| value.as_str())
                .any(|text| {
                    let normalized = normalize_text(text);
                    let words: Vec<&str> = normalized.split_whitespace().collect();
                    (0..words.len()).any(|start| {
                        let mut run = String::new();
                        for word in &words[start..] {
                            run.push_str(word);
                            if run.len() >= key.len() {
                                return run == key;
                            }
                        }
                        false
                    })
                })
        };
        if has(self.fields.title) {
            return Ok(true);
        }
        let described = has(self.fields.description);
        Ok(described && self.is_official(searcher, addr)?)
    }

    /// Whether the first `words` words of `query` are one word that many
    /// sites' titles have ([`spell::KNOWN_WORD_DOCS`] or more): "get",
    /// "yoga", "capital".
    pub(crate) fn names_by_common_word(
        &self,
        searcher: &tantivy::Searcher,
        query: &str,
        words: usize,
    ) -> Result<bool> {
        if words != 1 {
            return Ok(false);
        }
        let Some(word) = analysis::tokens(&self.words, query).into_iter().next() else {
            return Ok(false);
        };
        let term = Term::from_field_text(self.fields.title, &word);
        Ok(searcher.doc_freq(&term)? >= spell::KNOWN_WORD_DOCS)
    }

    /// The sites whose domain label or an alias equals the query's first
    /// words, with how many words each covers, plus the site of a typed
    /// hostname (covering the whole query). An official site's Wikidata
    /// names count as labels (see [`schema`](crate::schema)), and a text most links to a
    /// site use counts as an alias ([`RankConfig::link_names`]).
    pub(crate) fn name_matches(
        &self,
        searcher: &tantivy::Searcher,
        query: &ParsedQuery,
        cfg: &RankConfig,
    ) -> Result<HashMap<DocAddress, NameMatch>> {
        let mut names: HashMap<DocAddress, NameMatch> = HashMap::new();
        for (key, words) in &query.leading {
            let words = *words;
            if query.asked || words == 1 && query.len > 1 && query.is_filler(key) {
                continue;
            }
            let label = Term::from_field_text(self.fields.label_key, key);
            for addr in matching_docs(searcher, vec![label])? {
                let name = names.entry(addr).or_default();
                name.label = name.label.max(words);
            }
            let alias = Term::from_field_text(self.fields.alias_key, key);
            for addr in matching_docs(searcher, vec![alias])? {
                let name = names.entry(addr).or_default();
                name.alias = name.alias.max(words);
            }
            if let Some(field) = self.fields.link_name_key.filter(|_| cfg.link_names) {
                let term = Term::from_field_text(field, key);
                if let Some(addr) = named_by_links(searcher, &term)? {
                    let name = names.entry(addr).or_default();
                    name.linked = name.linked.max(words);
                }
            }
        }
        if let Some(domain) = &query.domain {
            let domain = Term::from_field_text(self.fields.domain, domain);
            for addr in matching_docs(searcher, vec![domain])? {
                let name = names.entry(addr).or_default();
                name.label = query.len;
                name.typed = true;
            }
        }
        Ok(names)
    }

    /// The sites whose domain label or an alias is a run of the query's
    /// words anywhere in it ("hilton" in "back bay hilton", "wix" in
    /// "ecommerce wix"), unless the run goes on into more words as one
    /// phrase ("cricket" in "the cricket and the ant", "guess" in "guess
    /// the states game"). A run of one function, filler or comparison word
    /// names nothing. The second set has only the sites named by an alias.
    ///
    /// A question names a site only by a text most links to it use
    /// ([`RankConfig::link_names`]): netflix.com in "how to cancel netflix"
    /// or reddit.com in "how to fix a leaky faucet reddit", but not rid.it
    /// in "how to get rid of aphids", which only its domain calls "rid".
    pub(crate) fn names_inside(
        &self,
        searcher: &tantivy::Searcher,
        query: &ParsedQuery,
        query_text: &str,
        cfg: &RankConfig,
    ) -> Result<(HashSet<DocAddress>, HashSet<DocAddress>)> {
        let link_names = self.fields.link_name_key.filter(|_| cfg.link_names);
        if query.asked && link_names.is_none() {
            return Ok((HashSet::new(), HashSet::new()));
        }
        let tokens = analysis::tokens(&self.words, query_text);
        let tokens = &tokens[..tokens.len().min(MAX_QUERY_WORDS)];
        let mut labels = Vec::new();
        let mut aliases = Vec::new();
        let mut linked = HashSet::new();
        for start in 0..tokens.len() {
            let mut key = String::new();
            for end in start..tokens.len() {
                key.push_str(&tokens[end]);
                let one = end == start;
                if one
                    && (is_function_word(&key)
                        || FILLER_WORDS.contains(&key.as_str())
                        || COMPARISON_WORDS.contains(&key.as_str()))
                {
                    continue;
                }
                let goes_on = tokens
                    .get(end + 1)
                    .is_some_and(|next| is_function_word(next));
                if goes_on {
                    continue;
                }
                if let Some(field) = link_names.filter(|_| query.asked) {
                    let term = Term::from_field_text(field, &key);
                    linked.extend(named_by_links(searcher, &term)?);
                    continue;
                }
                labels.push(Term::from_field_text(self.fields.label_key, &key));
                aliases.push(Term::from_field_text(self.fields.alias_key, &key));
            }
        }
        let mut by_alias = matching_docs(searcher, aliases)?;
        by_alias.extend(linked);
        let mut named = matching_docs(searcher, labels)?;
        named.extend(by_alias.iter().copied());
        Ok((named, by_alias))
    }

    /// Whether the document's title has the first `words` words of
    /// `query`: wimbledon.com, "The Championships, Wimbledon", calls itself
    /// what "wimbledon final" starts with, and a site that has the word
    /// only for its domain doesn't.
    pub(crate) fn titled_by_first_words(
        &self,
        searcher: &tantivy::Searcher,
        addr: DocAddress,
        query: &str,
        words: usize,
    ) -> Result<bool> {
        let first: Vec<String> = analysis::tokens(&self.words, query)
            .into_iter()
            .take(words)
            .collect();
        let doc: TantivyDocument = searcher.doc(addr)?;
        let titled = doc
            .get_all(self.fields.title)
            .filter_map(|value| value.as_str())
            .any(|title| {
                let title = analysis::tokens(&self.words, title);
                first.iter().all(|word| title.contains(word))
            });
        Ok(titled)
    }

    /// Whether the document's title, description or Wikidata description
    /// holds `words` consecutive words whose [`kind_key`] is `key`.
    pub(crate) fn describes_itself_as(
        &self,
        searcher: &tantivy::Searcher,
        addr: DocAddress,
        key: &str,
        words: usize,
    ) -> Result<bool> {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let fields = [
            self.fields.title,
            self.fields.description,
            self.fields.about,
        ];
        Ok(fields.into_iter().any(|field| {
            doc.get_all(field)
                .filter_map(|value| value.as_str())
                .any(|text| names_kind(text, key, words))
        }))
    }
}

/// Whether `domain` is under a top-level name reserved for tests (RFC
/// 2606, RFC 6761), which no real site has. `.example` stays: Plumb's own
/// tests and fixtures use it.
pub(crate) fn is_reserved_name(domain: &str) -> bool {
    let tld = domain.rsplit('.').next().unwrap_or("");
    ["test", "invalid", "localhost"].contains(&tld)
}

/// How a query names sites, for typo correction.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Named {
    /// The most of the query's first words a site's name covers exactly.
    pub(crate) words: usize,
    /// The best link score among the sites the whole query names, if any.
    pub(crate) full_link_score: Option<f32>,
    /// The query names a kind of site that the index has.
    pub(crate) kind: bool,
    /// The query is a hostname or URL.
    pub(crate) typed: bool,
}

/// The words of `query` before the [`INTENT_WORDS`] it ends with, if it
/// ends with any and has other words: "paypal login" -> "paypal".
pub fn without_intent_words(query: &str) -> Option<String> {
    let mut words: Vec<String> = normalize_text(query)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let all = words.len();
    loop {
        // The longest that fits: "web docs" rather than "docs".
        let cut = INTENT_WORDS
            .iter()
            .filter_map(|intent| {
                let n = intent.split(' ').count();
                let tail = words.get(words.len().checked_sub(n)?..)?;
                (n < words.len() && tail.iter().map(String::as_str).eq(intent.split(' ')))
                    .then_some(n)
            })
            .max();
        match cut {
            Some(n) => words.truncate(words.len() - n),
            None => break,
        }
    }
    (words.len() < all).then(|| words.join(" "))
}

/// An established site-navigation intent can wrap the exact subject in
/// question words. Institutional and product qualifiers remain part of
/// the indexed name being resolved; their wording never licenses a
/// partial name to discard them.
pub(crate) fn navigation_subject(query: &str) -> Option<String> {
    let subject = without_intent_words(query)?;
    let subject = [
        "where is ",
        "where can i find ",
        "what is ",
        "how do i find ",
    ]
    .iter()
    .find_map(|prefix| subject.strip_prefix(prefix))
    .unwrap_or(&subject);
    (!subject.is_empty()).then(|| subject.to_string())
}

/// How many of the query's words, from the first on, a site's names cover.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NameMatch {
    /// Words covered by the domain label: 2 for usbank.com in "us bank login".
    pub(crate) label: usize,
    /// Words covered by an alias.
    pub(crate) alias: usize,
    /// Words covered by a text most links to the site use
    /// ([`RankConfig::link_names`]).
    pub(crate) linked: usize,
    /// The query is this site's hostname or URL.
    pub(crate) typed: bool,
}

impl NameMatch {
    /// Words covered by the site's best name.
    pub(crate) fn words(&self) -> usize {
        self.label.max(self.alias).max(self.linked)
    }
}

/// The document most of the sites linking with the text of `term` link
/// to, in a `link_name_key` field: one that holds more than
/// [`LINK_NAME_SHARE`] of the term's occurrences, each standing for a
/// linking site ([`schema`](crate::schema)).
pub(crate) fn named_by_links(
    searcher: &tantivy::Searcher,
    term: &Term,
) -> Result<Option<DocAddress>> {
    if searcher.doc_freq(term)? > u64::from(MAX_LINK_NAME_DOCS) {
        return Ok(None);
    }
    let mut total = 0u64;
    let mut best: Option<(u32, DocAddress)> = None;
    for (segment_ord, reader) in searcher.segment_readers().iter().enumerate() {
        let inverted = reader.inverted_index(term.field())?;
        let Some(mut postings) = inverted.read_postings(term, IndexRecordOption::WithFreqs)? else {
            continue;
        };
        let alive = reader.alive_bitset();
        while postings.doc() != TERMINATED {
            let doc = postings.doc();
            if alive.is_none_or(|alive| alive.is_alive(doc)) {
                let sites = postings.term_freq();
                total += u64::from(sites);
                if best.is_none_or(|(most, _)| sites > most) {
                    best = Some((sites, DocAddress::new(segment_ord as u32, doc)));
                }
            }
            postings.advance();
        }
    }
    Ok(best
        .filter(|&(sites, _)| f64::from(sites) > f64::from(LINK_NAME_SHARE) * total as f64)
        .map(|(_, addr)| addr))
}

/// The registrable domain of a query that looks like a hostname or URL.
pub(crate) fn typed_domain(query: &str) -> Option<String> {
    let query = query.trim();
    if !query.contains('.') || query.contains(char::is_whitespace) {
        return None;
    }
    registrable_domain(query)
}

/// Whether `text` holds `words` consecutive words whose [`kind_key`] is
/// `key` at the end of a phrase: "a free online encyclopedia, made by"
/// names `onlineencyclopedia`, but "search engine optimization tools" does
/// not name `searchengine`. A phrase ends at punctuation, the end of the
/// text, or a word such as "that", "for" or "in".
pub(crate) fn names_kind(text: &str, key: &str, words: usize) -> bool {
    const ENDS: &[&str] = &[
        "that", "which", "who", "where", "for", "with", "by", "from", "in", "of", "on", "and",
        "or", "to", "since", "founded", "based", "run", "made", "created",
    ];
    if words == 0 {
        return false;
    }
    let breaks = |c: char| {
        matches!(
            c,
            ',' | '.' | ';' | ':' | '!' | '?' | '(' | ')' | '|' | '\u{2013}' | '\u{2014}'
        )
    };
    text.split(breaks).any(|clause| {
        let normalized = plumb_core::normalize_text(clause);
        let tokens: Vec<&str> = normalized.split(' ').filter(|w| !w.is_empty()).collect();
        tokens.windows(words).enumerate().any(|(i, window)| {
            let next = tokens.get(i + words);
            next.is_none_or(|next| ENDS.contains(next)) && kind_key(&window.join(" ")) == key
        })
    })
}
