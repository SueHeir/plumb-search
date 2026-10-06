//! Ranking the sites of a few buckets, in the browser.
//!
//! The node ranks with a Tantivy index (`plumb-index`), which does not
//! build for WebAssembly. A private search only has a few hundred candidate
//! sites, so this ranks them directly, with the same formula:
//!
//! `score = alpha * link_score + trust * ((1 - alpha) * text_score + name_bonus) + country_bonus`
//!
//! The name bonus, trust, kinds and country work exactly as in the index.
//! The text match is simpler: each query word scores the boost of every
//! field it appears in (label, joined names, aliases, title, link text,
//! description, the same boosts as the index), without BM25's word
//! frequencies, and is normalized to `0..=1` over the candidates. A word
//! also matches its other number in free text ("videos", "video"). Text is
//! not ASCII-folded, so `nestle` does not find `Nestlé` here.

use std::collections::{HashMap, HashSet};

use plumb_core::{
    domain_label, joined, kind_key, language_code, normalize_country, normalize_text, other_number,
    record_adult_level, registrable_domain, site_country, truncate_chars, Operators, SafeSearch,
    SiteRecord, MAX_ALIASES, MAX_TEXT_CHARS,
};
use serde::Serialize;

/// Weight of the popularity prior, as `plumb_index::RankConfig::default()`.
pub const ALPHA: f32 = 0.35;
/// Bonus of a domain label equal to the query.
pub const EXACT_LABEL_BONUS: f32 = 0.25;
/// Bonus of an alias equal to the query.
pub const EXACT_ALIAS_BONUS: f32 = 0.1;
/// Link score a site needs for its text to count in full after a site's
/// name plus more words.
pub const TRUSTED_LINK_SCORE: f32 = 0.2;
/// Share of its text a site with no link score keeps then.
pub const UNTRUSTED_SHARE: f32 = 0.5;
/// Bonus of a site of the kind the query names ("banks").
pub const KIND_BONUS: f32 = 0.25;
/// Share of the top score a site needs to stay listed below a site the
/// query names, as `plumb_index::RankConfig::default().named_share`.
pub const NAMED_SHARE: f32 = 0.4;
/// Link score of a well-known site, as `plumb_index::WELL_KNOWN_LINK_SCORE`.
pub const WELL_KNOWN_LINK_SCORE: f32 = 0.5;
/// Bonus of a site of the home country, and malus of another country's.
pub const COUNTRY_BOOST: f32 = 0.06;

const LABEL_BOOST: f32 = 4.0;
const JOINED_BOOST: f32 = 4.0;
const ALIASES_BOOST: f32 = 2.5;
const TITLE_BOOST: f32 = 2.0;
const ANCHORS_BOOST: f32 = 1.5;
const DESCRIPTION_BOOST: f32 = 0.5;
const WHOLE_QUERY_BOOST: f32 = 6.0;
const DOMAIN_BOOST: f32 = 10.0;
/// Share of a word's boost its other number gets, as in the index (which
/// also never lets it count for more than the word as typed).
const OTHER_NUMBER_SHARE: f32 = 0.8;
const MAX_QUERY_WORDS: usize = 16;
/// Link texts and title parts that get a joined form, as in the index.
const JOINED_LINK_TEXTS: usize = 8;
const MAX_TITLE_PARTS: usize = 4;
const TITLE_SEPARATORS: [char; 10] = ['|', '·', '•', ':', '–', '—', '»', '«', '/', '\\'];

/// Where the searcher is, for the country bonus.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Home country, a two-letter code.
    pub country: Option<String>,
    /// Leave out other countries' sites.
    pub only_country: bool,
    /// What safe search leaves out, as on a node (without its blocklist).
    pub safe: SafeSearch,
    /// Leave out sites whose homepage is in another language (a language
    /// code); sites that do not say stay.
    pub language: Option<String>,
}

/// One result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Ranked {
    pub domain: String,
    /// The record's URL, or `https://<domain>/`.
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub score: f32,
    pub text_score: f32,
    pub link_score: f32,
}

/// The best `limit` of `sites` for `query`, best first. Search operators
/// in the query ([`Operators`]) narrow the sites as on a node.
pub fn rank(query: &str, sites: &[SiteRecord], options: &Options, limit: usize) -> Vec<Ranked> {
    let ops = Operators::parse(query);
    if !ops.any() {
        return rank_words(query, sites, options, limit);
    }
    let kept: Vec<SiteRecord> = sites
        .iter()
        .filter(|site| ops.allows_host(&site.domain))
        .cloned()
        .collect();
    rank_words(&ops.lookup_text(), &kept, options, kept.len())
        .into_iter()
        .filter(|ranked| {
            let texts = [ranked.title.as_deref(), ranked.description.as_deref()];
            ops.allows(&ranked.domain, texts.into_iter().flatten())
        })
        .take(limit)
        .collect()
}

/// [`rank`] for a query without operators.
fn rank_words(query: &str, sites: &[SiteRecord], options: &Options, limit: usize) -> Vec<Ranked> {
    let Some(query) = Query::new(query) else {
        return Vec::new();
    };
    if limit == 0 || sites.is_empty() {
        return Vec::new();
    }
    let docs: Vec<Doc> = sites.iter().map(Doc::new).collect();
    let names: Vec<NameMatch> = docs.iter().map(|doc| query.name_match(doc)).collect();
    let link_scores: Vec<f32> = sites.iter().map(SiteRecord::link_score).collect();
    let named_link_score = names
        .iter()
        .zip(&link_scores)
        .filter(|(name, _)| name.words() > 0 && name.words() < query.len)
        .map(|(_, &score)| score)
        .fold(0.0, f32::max);
    let trusted = TRUSTED_LINK_SCORE.min(named_link_score);
    let text: Vec<f32> = docs.iter().map(|doc| query.text_match(doc)).collect();
    let max_text = text.iter().copied().fold(0.0, f32::max);
    let home = options.country.as_deref().and_then(normalize_country);
    let words = query.len as f32;

    let mut ranked = Vec::new();
    let language = options.language.as_deref().and_then(language_code);
    for (i, site) in sites.iter().enumerate() {
        if options.safe.hides(record_adult_level(site)) {
            continue;
        }
        let site_language = site.language.as_deref().and_then(language_code);
        if let (Some(wanted), Some(site)) = (&language, &site_language) {
            if wanted != site {
                continue;
            }
        }
        let name = names[i];
        let is_kind = query
            .kind
            .as_ref()
            .is_some_and(|kind| docs[i].kinds.contains(kind));
        if text[i] <= 0.0 && name.words() == 0 && !is_kind {
            continue;
        }
        let country = site_country(site);
        let country_bonus = match (&home, &country) {
            (Some(home), Some(country)) if home == country => COUNTRY_BOOST,
            (Some(_), Some(_)) if options.only_country => continue,
            (Some(_), Some(_)) => -COUNTRY_BOOST,
            _ => 0.0,
        };
        let link_score = link_scores[i];
        let text_score = if is_kind || name.label >= query.len {
            1.0
        } else if max_text > 0.0 {
            (text[i] / max_text).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let mut name_bonus = (EXACT_LABEL_BONUS * name.label as f32 / words)
            .max(EXACT_ALIAS_BONUS * name.alias as f32 / words);
        if is_kind {
            name_bonus = name_bonus.max(KIND_BONUS);
        }
        let trust = if name.typed || trusted <= 0.0 {
            1.0
        } else {
            let evidence = (link_score / trusted).min(1.0);
            UNTRUSTED_SHARE + (1.0 - UNTRUSTED_SHARE) * evidence
        };
        let named = name.typed || name.words() >= query.len;
        ranked.push((
            named,
            Ranked {
                domain: site.domain.clone(),
                url: site
                    .url
                    .as_deref()
                    .map(str::trim)
                    .filter(|url| !url.is_empty())
                    .map_or_else(|| format!("https://{}/", site.domain), str::to_string),
                title: site.title.clone().filter(|t| !t.trim().is_empty()),
                description: site.description.clone().filter(|d| !d.trim().is_empty()),
                score: ALPHA * link_score
                    + trust * ((1.0 - ALPHA) * text_score + name_bonus)
                    + country_bonus,
                text_score,
                link_score,
            },
        ));
    }
    ranked.sort_by(|(_, a), (_, b)| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| b.link_score.total_cmp(&a.link_score))
            .then_with(|| a.domain.cmp(&b.domain))
    });
    // Far below a site the query names: filler, as on a node.
    if let Some(&(true, ref top)) = ranked
        .first()
        .filter(|(_, top)| top.link_score >= WELL_KNOWN_LINK_SCORE)
    {
        let least = top.score * NAMED_SHARE;
        ranked.retain(|(named, r)| *named || r.score >= least);
    }
    ranked.truncate(limit);
    ranked.into_iter().map(|(_, r)| r).collect()
}

/// A site's names, split the way the index splits them.
#[derive(Debug, Default)]
struct Doc {
    domain: String,
    /// Words of the domain label, and the label joined.
    label: HashSet<String>,
    /// Whole names joined: label, title and its parts, aliases, top link texts.
    joined: HashSet<String>,
    /// Joined names that name the site as its domain does: the label, and
    /// an official site's aliases.
    label_keys: HashSet<String>,
    /// Joined aliases.
    alias_keys: HashSet<String>,
    aliases: HashSet<String>,
    title: HashSet<String>,
    anchors: HashSet<String>,
    description: HashSet<String>,
    kinds: HashSet<String>,
}

impl Doc {
    fn new(record: &SiteRecord) -> Doc {
        let mut doc = Doc {
            domain: record.domain.clone(),
            ..Doc::default()
        };
        let label = label_text(&record.domain);
        doc.label.extend(words(&label));
        doc.label.insert(joined(&label));
        doc.label_keys.insert(joined(&label));
        doc.joined.insert(joined(&label));
        if let Some(title) = record.title.as_deref().filter(|t| !t.trim().is_empty()) {
            let title = truncate_chars(title, MAX_TEXT_CHARS);
            let parts: Vec<&str> = title
                .split(TITLE_SEPARATORS)
                .flat_map(|part| part.split(" - "))
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .take(MAX_TITLE_PARTS)
                .collect();
            if parts.len() > 1 {
                doc.joined.insert(joined(&title));
            }
            for part in parts {
                doc.joined.insert(joined(part));
            }
            doc.title.extend(words(&title));
        }
        if let Some(description) = &record.description {
            doc.description
                .extend(words(&truncate_chars(description, MAX_TEXT_CHARS)));
        }
        for alias in record
            .aliases
            .iter()
            .filter(|a| !a.trim().is_empty())
            .take(MAX_ALIASES)
        {
            let alias = truncate_chars(alias, MAX_TEXT_CHARS);
            doc.aliases.extend(words(&alias));
            doc.joined.insert(joined(&alias));
            let short = normalize_text(&alias)
                .strip_prefix("the ")
                .filter(|rest| !rest.is_empty())
                .map(joined);
            for key in std::iter::once(joined(&alias)).chain(short.clone()) {
                if record.signals.official_site {
                    doc.label_keys.insert(key.clone());
                }
                doc.alias_keys.insert(key);
            }
            doc.joined.extend(short);
        }
        let mut texts: Vec<_> = record.link_texts.iter().collect();
        texts.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.text.cmp(&b.text)));
        for (i, lt) in texts.into_iter().enumerate() {
            doc.anchors.extend(words(&lt.text));
            if i < JOINED_LINK_TEXTS {
                doc.joined.insert(joined(&lt.text));
            }
        }
        doc.kinds.extend(
            record
                .kinds
                .iter()
                .map(|kind| kind_key(kind))
                .filter(|k| !k.is_empty()),
        );
        doc.joined.remove("");
        doc
    }
}

/// A query, split the way the index splits it.
#[derive(Debug)]
struct Query {
    /// Distinct words, in query order.
    words: Vec<String>,
    /// Each word in its other number, if it has one ("videos" -> "video").
    others: Vec<Option<String>>,
    /// The whole query joined.
    joined: Option<String>,
    /// The first word, the first two joined and so on, with the words each
    /// covers; a leading "the" may be left out.
    leading: Vec<(String, usize)>,
    kind: Option<String>,
    /// Words, repeats included.
    len: usize,
    /// The registrable domain of a query that is a hostname or URL.
    domain: Option<String>,
}

impl Query {
    fn new(query: &str) -> Option<Query> {
        let query = truncate_chars(query, MAX_TEXT_CHARS);
        let tokens = words(&query);
        if tokens.is_empty() {
            return None;
        }
        let mut distinct: Vec<String> = Vec::new();
        for word in &tokens {
            if !distinct.contains(word) {
                distinct.push(word.clone());
            }
        }
        distinct.truncate(MAX_QUERY_WORDS);
        let prefixes = |skip: usize| {
            let mut key = String::new();
            tokens
                .iter()
                .skip(skip)
                .take(MAX_QUERY_WORDS)
                .enumerate()
                .map(|(i, word)| {
                    key.push_str(word);
                    (key.clone(), skip + i + 1)
                })
                .collect::<Vec<_>>()
        };
        let mut leading = prefixes(0);
        if tokens.len() > 1 && tokens[0] == "the" {
            leading.extend(prefixes(1));
        }
        let trimmed = query.trim();
        let domain = (trimmed.contains('.') && !trimmed.contains(char::is_whitespace))
            .then(|| registrable_domain(trimmed))
            .flatten();
        let others = distinct.iter().map(|word| other_number(word)).collect();
        Some(Query {
            words: distinct,
            others,
            joined: Some(joined(&query)).filter(|j| !j.is_empty()),
            leading,
            kind: Some(kind_key(&query)).filter(|k| !k.is_empty()),
            len: tokens.len(),
            domain,
        })
    }

    /// How well `doc`'s fields match, before normalizing: the boost of
    /// every field each clause matches, a clause added twice keeping its
    /// larger boost, as in the index.
    fn text_match(&self, doc: &Doc) -> f32 {
        let mut clauses: HashMap<(Field, &str), f32> = HashMap::new();
        fn add<'a>(
            clauses: &mut HashMap<(Field, &'a str), f32>,
            field: Field,
            term: &'a str,
            boost: f32,
        ) {
            let known = clauses.entry((field, term)).or_insert(0.0);
            *known = known.max(boost);
        }
        let name_share = 1.0 / self.words.len() as f32;
        for word in &self.words {
            add(&mut clauses, Field::Label, word, LABEL_BOOST * name_share);
            add(&mut clauses, Field::Joined, word, JOINED_BOOST * name_share);
            add(&mut clauses, Field::Aliases, word, ALIASES_BOOST);
            add(&mut clauses, Field::Title, word, TITLE_BOOST);
            add(&mut clauses, Field::Anchors, word, ANCHORS_BOOST);
            add(&mut clauses, Field::Description, word, DESCRIPTION_BOOST);
        }
        // The other number of each word, in the fields of free text only.
        for other in self.others.iter().flatten() {
            for (field, boost) in [
                (Field::Aliases, ALIASES_BOOST),
                (Field::Title, TITLE_BOOST),
                (Field::Anchors, ANCHORS_BOOST),
                (Field::Description, DESCRIPTION_BOOST),
            ] {
                add(&mut clauses, field, other, boost * OTHER_NUMBER_SHARE);
            }
        }
        if let Some(joined) = &self.joined {
            add(&mut clauses, Field::Joined, joined, WHOLE_QUERY_BOOST);
            add(&mut clauses, Field::Label, joined, WHOLE_QUERY_BOOST);
        }
        if let Some(domain) = &self.domain {
            add(&mut clauses, Field::Domain, domain, DOMAIN_BOOST);
        }
        clauses
            .into_iter()
            .filter(|((field, term), _)| field.holds(doc, term))
            .map(|(_, boost)| boost)
            .sum()
    }

    fn name_match(&self, doc: &Doc) -> NameMatch {
        let mut name = NameMatch::default();
        for (key, words) in &self.leading {
            if doc.label_keys.contains(key) {
                name.label = name.label.max(*words);
            }
            if doc.alias_keys.contains(key) {
                name.alias = name.alias.max(*words);
            }
        }
        if self.domain.as_deref() == Some(doc.domain.as_str()) {
            name.label = self.len;
            name.typed = true;
        }
        name
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    Label,
    Joined,
    Aliases,
    Title,
    Anchors,
    Description,
    Domain,
}

impl Field {
    fn holds(self, doc: &Doc, term: &str) -> bool {
        match self {
            Field::Label => doc.label.contains(term),
            Field::Joined => doc.joined.contains(term),
            Field::Aliases => doc.aliases.contains(term),
            Field::Title => doc.title.contains(term),
            Field::Anchors => doc.anchors.contains(term),
            Field::Description => doc.description.contains(term),
            Field::Domain => doc.domain == term,
        }
    }
}

/// How many of the query's first words a site's names cover.
#[derive(Debug, Clone, Copy, Default)]
struct NameMatch {
    label: usize,
    alias: usize,
    typed: bool,
}

impl NameMatch {
    fn words(&self) -> usize {
        self.label.max(self.alias)
    }
}

/// The words of `text`, as the index's word analyzer splits them.
fn words(text: &str) -> Vec<String> {
    normalize_text(text)
        .split(' ')
        .map(|word| word.chars().filter(|c| c.is_alphanumeric()).collect())
        .filter(|word: &String| !word.is_empty())
        .collect()
}

/// The domain label as people write it, punycode decoded.
fn label_text(domain: &str) -> String {
    let label = domain_label(domain);
    if label.contains("xn--") {
        if let (unicode, Ok(())) = idna::domain_to_unicode(&label) {
            return unicode;
        }
    }
    label
}

/// One copy of each site: a site can come in more than one bucket, or from
/// more than one node. The first copy's text is kept, and each popularity
/// signal keeps the less favorable value of all copies, as nodes do with
/// each other's answers, so one node alone cannot make a site look more
/// popular.
pub fn merge_copies(sites: Vec<SiteRecord>) -> Vec<SiteRecord> {
    let mut order: Vec<SiteRecord> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for site in sites {
        match at.get(&site.domain) {
            Some(&i) => {
                let kept = &mut order[i].signals;
                let other = &site.signals;
                kept.harmonic_rank = worse_rank(kept.harmonic_rank, other.harmonic_rank);
                kept.pagerank_rank = worse_rank(kept.pagerank_rank, other.pagerank_rank);
                kept.tranco_rank = worse_rank(kept.tranco_rank, other.tranco_rank);
                kept.linking_domains = kept.linking_domains.min(other.linking_domains);
                kept.official_site &= other.official_site;
                kept.sitelinks = kept.sitelinks.min(other.sitelinks);
            }
            None => {
                at.insert(site.domain.clone(), order.len());
                order.push(site);
            }
        }
    }
    order
}

/// The larger rank (worse), counting a missing one as the worst.
fn worse_rank<T: Ord>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        _ => None,
    }
}
