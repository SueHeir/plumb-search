//! Ranking the sites of a few buckets, in the browser.
//!
//! The node ranks with a Tantivy index (`plumb-index`), which does not
//! build for WebAssembly. A private search only has a few hundred candidate
//! sites, so this ranks them directly, with the same formula:
//!
//! `score = alpha * link_score + trust * ((1 - alpha) * text_score + name_bonus) + country_bonus`
//!
//! Whole-query evidence precedes popularity, as in the index: exact names,
//! typed domains, kinds and established navigation subjects are protected;
//! other sites need substantive coverage before receiving their full prior.
//! The text match is simpler: each query word scores the boost of every
//! field it appears in (label, joined names, aliases, title, link text,
//! description, headings, terms and Wikidata description), without BM25's word
//! frequencies, and is normalized to `0..=1` over the candidates. A word
//! also matches its other number in free text ("videos", "video"). Text is
//! not ASCII-folded, so `nestle` does not find `Nestlé` here.

use std::collections::{HashMap, HashSet};

use plumb_core::{
    domain_label, is_function_word, joined, kind_key, language_code, normalize_country,
    normalize_text, other_number, record_adult_level, registrable_domain, site_country,
    truncate_chars, Operators, SafeSearch, SiteRecord, MAX_ALIASES, MAX_HEADINGS, MAX_TERMS,
    MAX_TEXT_CHARS,
};
use serde::Serialize;

/// Weight of the popularity prior, as `plumb_index::RankConfig::default()`.
pub const ALPHA: f32 = 0.35;
/// Popularity weight when the query does not establish a navigation target.
pub const DESCRIBED_ALPHA: f32 = 0.5;
/// Minimum normalized text evidence for the full descriptive prior.
pub const DESCRIBED_RELEVANCE: f32 = 0.04;
pub const NAVIGATIONAL_RELEVANCE: f32 = 0.05;
pub const QUESTION_RELEVANCE: f32 = 0.3;
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
const ABOUT_BOOST: f32 = 2.0;
const HEADINGS_BOOST: f32 = 0.5;
const TERMS_BOOST: f32 = 1.0;
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

// The native query vocabulary. Contract tests compare the intent handling
// with plumb-index; that crate cannot be a browser runtime dependency.
const FILLER_WORDS: &[&str] = &[
    "how", "why", "what", "whats", "when", "where", "which", "who", "whom", "whose", "can",
    "could", "should", "do", "does", "did", "is", "are", "was", "were", "will", "would", "my",
    "me", "your", "best", "top", "open", "now", "today", "hours", "near", "nearby", "website",
    "site", "homepage", "app", "login", "signin", "sign", "log", "account", "contact", "support",
    "help", "official",
];
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
const QUESTION_WORDS: &[&str] = &[
    "how", "why", "what", "whats", "what's", "when", "where", "which", "who", "can", "could",
    "should", "do", "does", "did", "is", "are", "will", "would",
];

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
fn rank_words(
    query_text: &str,
    sites: &[SiteRecord],
    options: &Options,
    limit: usize,
) -> Vec<Ranked> {
    let Some(query) = Query::new(query_text) else {
        return Vec::new();
    };
    if limit == 0 || sites.is_empty() {
        return Vec::new();
    }
    let docs: Vec<Doc> = sites.iter().map(Doc::new).collect();
    let names: Vec<NameMatch> = docs.iter().map(|doc| query.name_match(doc)).collect();
    let link_scores: Vec<f32> = sites.iter().map(SiteRecord::link_score).collect();
    let navigation_subject = navigation_subject(query_text).and_then(|s| Query::new(&s));
    let navigation_names: Vec<bool> = docs
        .iter()
        .zip(&link_scores)
        .map(|(doc, &score)| {
            score >= WELL_KNOWN_LINK_SCORE
                && navigation_subject
                    .as_ref()
                    .is_some_and(|subject| subject.name_match(doc).words() >= subject.len)
        })
        .collect();
    let named_in_full = names.iter().any(|name| name.words() >= query.len);
    let navigational = named_in_full || query.domain.is_some() || navigation_names.contains(&true);
    let alpha = if navigational { ALPHA } else { DESCRIBED_ALPHA };
    let mut relevance_floor = if navigational {
        NAVIGATIONAL_RELEVANCE
    } else {
        DESCRIBED_RELEVANCE
    };
    if query.len >= 2 && !named_in_full && asked_as_question(query_text) {
        relevance_floor = relevance_floor.max(QUESTION_RELEVANCE);
    }
    let named_link_score = names
        .iter()
        .zip(&link_scores)
        .filter(|(name, _)| name.words() > 0 && name.words() < query.len)
        .map(|(_, &score)| score)
        .fold(0.0, f32::max);
    let trusted = TRUSTED_LINK_SCORE.min(named_link_score);
    let text: Vec<f32> = docs.iter().map(|doc| query.text_match(doc)).collect();
    let max_text = text.iter().copied().fold(0.0, f32::max);
    let weights = query.evidence_weights(&docs);
    // Buckets contain no query vectors. Whole-query lexical support can
    // resolve competing aliases; otherwise a single established identity
    // retains its scope. A domain spelling repeated in its own text cannot.
    let full_identity = names
        .iter()
        .any(|name| name.typed || name.alias >= query.len);
    let subject_names: Vec<_> = names
        .iter()
        .zip(&docs)
        .filter(|_| !full_identity)
        .map(|(name, doc)| (doc, name.alias))
        .filter(|(_, words)| {
            *words > 0
                && *words < query.len
                && !query
                    .tokens
                    .get(*words)
                    .is_some_and(|word| is_function_word(word))
        })
        .map(|(doc, words)| {
            let evidence = query.evidence(doc, &weights, words);
            (
                doc,
                words,
                evidence.substantive >= 0.75 && evidence.task_supported,
            )
        })
        .collect();
    let mut identities: HashMap<usize, HashSet<String>> = HashMap::new();
    for &(doc, words, _) in &subject_names {
        identities
            .entry(words)
            .or_default()
            .insert(doc.domain.clone());
    }
    let named_subject_words = subject_names
        .into_iter()
        .filter(|(_, words, compatible)| {
            *compatible || identities.get(words).is_some_and(|sites| sites.len() == 1)
        })
        .map(|(_, words, compatible)| (compatible, words))
        .max()
        .map(|(_, words)| words)
        .unwrap_or(0);
    let home = options.country.as_deref().and_then(normalize_country);
    let words = query.len as f32;

    let mut ranked = Vec::new();
    let language = options.language.as_deref().and_then(language_code);
    for (i, site) in sites.iter().enumerate() {
        if options.safe.hides(record_adult_level(site)) {
            continue;
        }
        let name = names[i];
        let site_language = site.language.as_deref().and_then(language_code);
        if let (Some(wanted), Some(site)) = (&language, &site_language) {
            if wanted != site && !name.typed && name.label < query.len {
                continue;
            }
        }
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
        let full_hostname = self::words(&site.domain) == query.tokens;
        let named = name.typed || full_hostname || name.words() >= query.len;
        let protected = named && named_subject_words == 0
            || name.typed
            || full_hostname
            || name.alias >= query.len
            || is_kind
            || navigation_names[i];
        let evidence = query.evidence(&docs[i], &weights, name.words());
        let subject_evidence = query.evidence(&docs[i], &weights, named_subject_words);
        let subject = subject_evidence.subject;
        let subject_share = if protected {
            1.0
        } else {
            subject.unwrap_or(1.0)
        };
        // There are no query vectors in private buckets. Missing semantic
        // evidence cannot promote a domain collision to a substantive match.
        let convincing = protected
            || subject_share >= 1.0 - f32::EPSILON
                && (subject.is_none() || subject_evidence.task_supported)
                && evidence.substantive >= 0.75;
        let whole_share = if convincing {
            1.0
        } else {
            evidence.coverage * subject_share
        };
        let name_share = if convincing {
            1.0
        } else {
            evidence.remaining * subject_share
        };
        let text_score = if is_kind || name.label >= query.len {
            1.0
        } else if max_text > 0.0 {
            (text[i] / max_text).clamp(0.0, 1.0)
        } else {
            0.0
        } * whole_share;
        let mut name_bonus = (EXACT_LABEL_BONUS * name.label as f32 / words)
            .max(EXACT_ALIAS_BONUS * name.alias as f32 / words);
        if is_kind {
            name_bonus = name_bonus.max(KIND_BONUS);
        }
        name_bonus *= name_share;
        let trust = if name.typed || trusted <= 0.0 {
            1.0
        } else {
            let evidence = (link_score / trusted).min(1.0);
            UNTRUSTED_SHARE + (1.0 - UNTRUSTED_SHARE) * evidence
        };
        let prior = if protected {
            link_score
        } else {
            link_score * (text_score / relevance_floor).min(1.0)
        } * whole_share;
        ranked.push((
            convincing,
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
                score: alpha * prior
                    + trust * ((1.0 - alpha) * text_score + name_bonus)
                    + country_bonus,
                text_score,
                link_score,
            },
        ));
    }
    ranked.sort_by(|(a_tier, _, a), (b_tier, _, b)| {
        b_tier
            .cmp(a_tier)
            .then_with(|| b.score.total_cmp(&a.score))
            .then_with(|| b.link_score.total_cmp(&a.link_score))
            .then_with(|| a.domain.cmp(&b.domain))
    });
    // Keep callers that sort scores from undoing the relevance tiers.
    if let Some(first_weak) = ranked
        .iter()
        .position(|(tier, _, _)| !tier)
        .filter(|&i| i > 0)
    {
        let boundary = ranked[first_weak - 1].2.score;
        let ceiling = boundary - f32::EPSILON * boundary.abs().max(1.0);
        for (_, _, row) in &mut ranked[first_weak..] {
            row.score = row.score.min(ceiling);
        }
    }
    // Far below a site the query names: filler, as on a node.
    if let Some(&(_, true, ref top)) = ranked
        .first()
        .filter(|(_, _, top)| top.link_score >= WELL_KNOWN_LINK_SCORE)
    {
        let least = top.score * NAMED_SHARE;
        ranked.retain(|(_, named, r)| *named || r.score >= least);
    }
    ranked.truncate(limit);
    ranked.into_iter().map(|(_, _, r)| r).collect()
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
    about: HashSet<String>,
    headings: HashSet<String>,
    terms: HashSet<String>,
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
        fn non_empty(text: Option<&str>) -> Option<&str> {
            text.filter(|text| !text.trim().is_empty())
        }
        let borrowed = non_empty(record.url.as_deref())
            .and_then(registrable_domain)
            .is_some_and(|domain| domain != record.domain);
        let title = non_empty(record.title.as_deref())
            .filter(|_| !borrowed)
            .or_else(|| record.aliases.iter().find_map(|a| non_empty(Some(a))));
        if let Some(title) = title {
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
        let description = non_empty(record.description.as_deref());
        if let Some(description) = description.filter(|_| !borrowed) {
            doc.description
                .extend(words(&truncate_chars(description, MAX_TEXT_CHARS)));
        }
        let intro = non_empty(record.intro.as_deref());
        if let Some(intro) = intro {
            doc.description
                .extend(words(&truncate_chars(intro, MAX_TEXT_CHARS)));
        }
        if let Some(summary) = non_empty(record.summary.as_deref())
            .filter(|_| intro.is_none() && (borrowed || description.is_none()))
        {
            doc.description
                .extend(words(&truncate_chars(summary, MAX_TEXT_CHARS)));
        }
        if let Some(about) = &record.about {
            doc.about
                .extend(words(&truncate_chars(about, MAX_TEXT_CHARS)));
        }
        for heading in record.headings.iter().take(MAX_HEADINGS) {
            doc.headings
                .extend(words(&truncate_chars(heading, MAX_TEXT_CHARS)));
        }
        for term in record.terms.iter().take(MAX_TERMS) {
            doc.terms.extend(words(term));
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
    tokens: Vec<String>,
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
    asked: bool,
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
        let mut leading: Vec<_> = prefixes(0)
            .into_iter()
            .filter(|(key, count)| tokens.len() == 1 || *count > 1 || !is_function_word(key))
            .collect();
        if tokens.len() > 1 && tokens[0] == "the" {
            leading.extend(prefixes(1));
        }
        let trimmed = query.trim();
        let domain = (trimmed.contains('.') && !trimmed.contains(char::is_whitespace))
            .then(|| registrable_domain(trimmed))
            .flatten();
        let others = distinct.iter().map(|word| other_number(word)).collect();
        let asked = tokens.len() >= 3 && asked_as_question(&query);
        Some(Query {
            words: distinct,
            others,
            joined: Some(joined(&query)).filter(|j| !j.is_empty()),
            leading,
            kind: Some(kind_key(&query)).filter(|k| !k.is_empty()),
            len: tokens.len(),
            tokens,
            domain,
            asked,
        })
    }

    fn per_word(&self) -> [(Field, f32); 9] {
        let name_share = 1.0 / self.words.len() as f32;
        [
            (Field::Label, LABEL_BOOST * name_share),
            (Field::Joined, JOINED_BOOST * name_share),
            (Field::Aliases, ALIASES_BOOST),
            (Field::Title, TITLE_BOOST),
            (Field::Anchors, ANCHORS_BOOST),
            (Field::Description, DESCRIPTION_BOOST),
            (Field::About, ABOUT_BOOST),
            (Field::Headings, HEADINGS_BOOST),
            (Field::Terms, TERMS_BOOST),
        ]
    }

    fn allows_word(&self, i: usize, field: Field, other: bool) -> bool {
        let word = &self.words[i];
        let name_field = matches!(field, Field::Label | Field::Joined | Field::Aliases);
        let filler = self.len > 1 && FILLER_WORDS.contains(&word.as_str());
        if filler && (name_field || matches!(field, Field::Title | Field::Anchors)) {
            return false;
        }
        if other {
            return !matches!(field, Field::Label | Field::Joined);
        }
        !name_field || !self.asked && (self.len == 1 || i == 0 || !is_function_word(word))
    }

    fn evidence_weights(&self, docs: &[Doc]) -> Vec<f32> {
        let subject = without_intent_words(&self.words.join(" "));
        let subject: Option<HashSet<_>> = subject.as_ref().map(|s| s.split_whitespace().collect());
        self.words
            .iter()
            .map(|word| {
                if is_function_word(word) || FILLER_WORDS.contains(&word.as_str()) {
                    0.0
                } else if ["online", "watch", "read", "find", "learn"].contains(&word.as_str()) {
                    0.1
                } else if subject.as_ref().is_some_and(|s| !s.contains(word.as_str())) {
                    0.25
                } else {
                    let frequency = docs
                        .iter()
                        .flat_map(|doc| SUBSTANTIVE_FIELDS.map(|field| field.holds(doc, word)))
                        .filter(|&matched| matched)
                        .count();
                    (1.0 + (docs.len() as f32 / (1.0 + frequency as f32)).ln_1p()).clamp(1.0, 3.0)
                }
            })
            .collect()
    }

    fn evidence(&self, doc: &Doc, weights: &[f32], prefix_words: usize) -> LexicalEvidence {
        let prefix = self
            .tokens
            .iter()
            .take(prefix_words)
            .collect::<HashSet<_>>()
            .len();
        let total: f32 = weights.iter().sum();
        let subject_total: f32 = weights.iter().take(prefix).sum();
        let remaining_total: f32 = weights.iter().skip(prefix).sum();
        let mut evidence = LexicalEvidence {
            task_supported: !weights.iter().skip(prefix).any(|&weight| weight >= 1.0),
            ..Default::default()
        };
        let mut matched_words = 0;
        let mut matched_subject = 0.0;
        for (i, &weight) in weights.iter().enumerate() {
            let word = &self.words[i];
            let other = self.others[i].as_deref();
            let any = self.per_word().into_iter().any(|(field, _)| {
                self.allows_word(i, field, false) && field.holds(doc, word)
                    || self.allows_word(i, field, true)
                        && other.is_some_and(|other| field.holds(doc, other))
            });
            if any {
                matched_words += 1;
                evidence.coverage += weight;
            }
            if SUBSTANTIVE_FIELDS.into_iter().any(|field| {
                field.holds(doc, word) || other.is_some_and(|other| field.holds(doc, other))
            }) {
                evidence.substantive += weight;
                if i < prefix {
                    matched_subject += weight;
                }
                if i >= prefix {
                    evidence.remaining += weight;
                    evidence.task_supported |= weight >= 1.0;
                }
            }
        }
        if total > 0.0 {
            evidence.coverage = (evidence.coverage / total).min(1.0);
            evidence.substantive = (evidence.substantive / total).min(1.0);
        } else {
            evidence.coverage = matched_words as f32 / self.words.len() as f32;
            evidence.substantive = evidence.coverage;
        }
        evidence.remaining = if remaining_total > 0.0 {
            evidence.remaining / remaining_total
        } else {
            1.0
        };
        evidence.subject = (subject_total > 0.0).then_some(matched_subject / subject_total);
        evidence
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
        for (i, word) in self.words.iter().enumerate() {
            for (field, boost) in self.per_word() {
                if self.allows_word(i, field, false) {
                    add(&mut clauses, field, word, boost);
                }
            }
        }
        // The other number of each word, in the fields of free text only.
        for (i, other) in self.others.iter().enumerate() {
            let Some(other) = other else { continue };
            for (field, boost) in self.per_word() {
                if self.allows_word(i, field, true) {
                    add(&mut clauses, field, other, boost * OTHER_NUMBER_SHARE);
                }
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
            if self.asked || *words == 1 && self.len > 1 && FILLER_WORDS.contains(&key.as_str()) {
                continue;
            }
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
    About,
    Headings,
    Terms,
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
            Field::About => doc.about.contains(term),
            Field::Headings => doc.headings.contains(term),
            Field::Terms => doc.terms.contains(term),
            Field::Domain => doc.domain == term,
        }
    }
}

const SUBSTANTIVE_FIELDS: [Field; 6] = [
    Field::Title,
    Field::Description,
    Field::About,
    Field::Headings,
    Field::Terms,
    Field::Aliases,
];

#[derive(Default)]
struct LexicalEvidence {
    coverage: f32,
    substantive: f32,
    remaining: f32,
    subject: Option<f32>,
    task_supported: bool,
}

fn asked_as_question(query: &str) -> bool {
    query
        .split_whitespace()
        .next()
        .is_some_and(|word| QUESTION_WORDS.contains(&word.to_lowercase().as_str()))
}

fn without_intent_words(query: &str) -> Option<String> {
    let normalized = normalize_text(query);
    let mut words: Vec<_> = normalized.split_whitespace().collect();
    let all = words.len();
    loop {
        let cut = INTENT_WORDS
            .iter()
            .filter_map(|intent| {
                let n = intent.split(' ').count();
                let tail = words.get(words.len().checked_sub(n)?..)?;
                (n < words.len() && tail.iter().copied().eq(intent.split(' '))).then_some(n)
            })
            .max();
        match cut {
            Some(n) => words.truncate(words.len() - n),
            None => break,
        }
    }
    (words.len() < all).then(|| words.join(" "))
}

fn navigation_subject(query: &str) -> Option<String> {
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

#[cfg(test)]
mod evidence_tests {
    use super::*;

    #[test]
    fn navigation_vocabulary_agrees_with_the_native_contract() {
        for intent in INTENT_WORDS {
            for query in [
                format!("aster {intent}"),
                format!("aster qz {intent}"),
                format!("aster government {intent}"),
                format!("aster {intent} login"),
                intent.to_string(),
            ] {
                assert_eq!(
                    without_intent_words(&query),
                    plumb_index::without_intent_words(&query)
                );
            }
        }
        for qualifier in [
            "refund",
            "api",
            "manual",
            "policy",
            "qz",
            "7",
            "website address",
        ] {
            let query = format!("aster {qualifier}");
            assert_eq!(without_intent_words(&query), None);
            assert_eq!(plumb_index::without_intent_words(&query), None);
        }
    }

    #[test]
    fn domain_and_anchor_words_are_not_substantive_qualifier_evidence() {
        let mut site = SiteRecord::new("aster.example");
        site.title = Some("Aster Observatory".into());
        site.link_texts = vec![plumb_core::LinkText::with_count(
            "Aster Observatory refund",
            4,
        )];
        let docs = [Doc::new(&site)];
        let query = Query::new("aster refund").unwrap();
        let weights = query.evidence_weights(&docs);
        let evidence = query.evidence(&docs[0], &weights, 1);
        assert_eq!(evidence.coverage, 1.0);
        assert!(evidence.substantive < 0.75);
        assert_eq!(evidence.remaining, 0.0);
    }

    #[test]
    fn remaining_evidence_counts_distinct_prefix_words() {
        let mut site = SiteRecord::new("aster.example");
        site.title = Some("Aster Observatory".into());
        site.description = Some("Refunds".into());
        let docs = [Doc::new(&site)];
        for query in ["aster refund", "aster aster refund", "the aster refund"] {
            let query = Query::new(query).unwrap();
            let weights = query.evidence_weights(&docs);
            let prefix = query.len - 1;
            let evidence = query.evidence(&docs[0], &weights, prefix);
            assert_eq!(evidence.substantive, 1.0);
            assert_eq!(evidence.remaining, 1.0);
        }
    }
}
