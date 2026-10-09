//! The index schema and how a [`SiteRecord`] becomes a document.
//!
//! | field         | holds                                                | used for                    |
//! |---------------|------------------------------------------------------|-----------------------------|
//! | `domain`      | registrable domain, untokenized                      | stored, fast, typed domains |
//! | `url`         | homepage URL                                         | stored                      |
//! | `title`       | homepage title                                       | stored, BM25                |
//! | `description` | meta description                                     | stored, BM25                |
//! | `about`       | Wikidata's description of the organization           | stored, BM25                |
//! | `headings`    | homepage `<h1>` and `<h2>` texts                     | BM25                        |
//! | `terms`       | search terms picked from the homepage (experiment)   | BM25                        |
//! | `label`       | domain label, its hyphen-split words and joined form | BM25                        |
//! | `aliases`     | other names                                          | BM25                        |
//! | `anchors`     | inbound link texts, frequent ones repeated           | BM25                        |
//! | `joined`      | label, title and its parts, aliases, top link texts  | BM25, one token per name    |
//! | `label_key`   | joined label, labels of sites redirecting here       | exact label match           |
//! | `alias_key`   | joined aliases                                       | exact alias match           |
//! | `link_score`  | [`plumb_core::link_score`]                           | stored, fast                |
//! | `country`     | [`plumb_core::site_country`], untokenized            | stored, fast                |
//! | `kind_key`    | [`plumb_core::kind_key`] of each kind                | kind queries ("banks")      |
//! | `search_url`  | the site's search address                            | stored, site search links   |
//! | `language`    | the homepage's language code, untokenized            | stored, fast, language filter |
//! | `adult`       | [`plumb_core::AdultLevel`] as 0, 1 or 2              | fast, safe search           |
//! | `key_pages`   | the site's key pages, as JSON                        | stored, sitelinks           |
//! | `fingerprint` | [`plumb_core::simhash`] of the homepage's text       | stored, near-copies         |
//! | `link_name_key` | name-like link texts, once per linking site        | names other sites give it   |
//!
//! An official website's aliases (its Wikidata names) also go into
//! `label_key`, so they name the site as strongly as its domain does, and
//! every alias starting with "The" is also keyed without it.
//!
//! `link_name_key` holds the link texts that read like a name, that
//! [`MIN_LINK_NAME_SITES`] or more sites use and that are at least
//! [`MIN_LINK_NAME_SHARE`] of the site's links, each as many times as sites
//! use it (up to [`MAX_LINK_NAME_REPEATS`]), and without a leading "The"
//! too. Its term frequencies say how
//! many sites call this site by that text, so a search can weigh how often
//! a text names this site against every other site it names, as CrossWikis
//! does with Wikipedia's links ([`Searcher`](crate::Searcher)).

use anyhow::{Context, Result};
use plumb_core::key_pages::{key_pages_or_known, valid_key_pages};
use plumb_core::{
    domain_label, joined, kind_key, normalize_text, record_adult_level, site_country,
    site_language, truncate_chars, AdultLevel, LinkText, SiteRecord, MAX_ALIASES, MAX_HEADINGS,
    MAX_KINDS, MAX_LINK_TEXTS, MAX_TERMS, MAX_TEXT_CHARS,
};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, STORED, STRING,
};
use tantivy::TantivyDocument;

use crate::analysis::{JOINED_ANALYZER, WORDS_ANALYZER};

pub(crate) const DOMAIN: &str = "domain";
pub(crate) const URL: &str = "url";
pub(crate) const TITLE: &str = "title";
pub(crate) const DESCRIPTION: &str = "description";
pub(crate) const HEADINGS: &str = "headings";
pub(crate) const TERMS: &str = "terms";
pub(crate) const ABOUT: &str = "about";
pub(crate) const LABEL: &str = "label";
pub(crate) const ALIASES: &str = "aliases";
pub(crate) const ANCHORS: &str = "anchors";
pub(crate) const JOINED: &str = "joined";
pub(crate) const LABEL_KEY: &str = "label_key";
pub(crate) const ALIAS_KEY: &str = "alias_key";
pub(crate) const LINK_SCORE: &str = "link_score";
pub(crate) const COUNTRY: &str = "country";
pub(crate) const KIND_KEY: &str = "kind_key";
pub(crate) const SEARCH_URL: &str = "search_url";
pub(crate) const LANGUAGE: &str = "language";
pub(crate) const ADULT: &str = "adult";
pub(crate) const KEY_PAGES: &str = "key_pages";
pub(crate) const FINGERPRINT: &str = "fingerprint";
pub(crate) const LINK_NAME_KEY: &str = "link_name_key";

/// Fewest distinct sites linking with a text for it to name the site:
/// independent sources have to agree, as on any other name.
pub(crate) const MIN_LINK_NAME_SITES: u32 = 3;
/// Most times one link text goes into `link_name_key`. Enough to tell a
/// site most links name apart from one a few do.
pub(crate) const MAX_LINK_NAME_REPEATS: u32 = 64;
/// Least share of the links to a site that use a text for it to be one of
/// the site's names: what a few of a bank's many links say of it ("online
/// banking") describes it rather than names it.
const MIN_LINK_NAME_SHARE: f64 = 0.05;
/// Longest link text taken as a name, in characters, and most words.
const MAX_LINK_NAME_CHARS: usize = 60;
const MAX_LINK_NAME_WORDS: usize = 6;

/// How many link texts (most frequent first) also get a joined form.
const JOINED_LINK_TEXTS: usize = 8;
/// A link text used `count` times goes into `anchors` `min(1 + ln(count), 5)`
/// times (rounded), so frequent texts weigh more without drowning the rest.
const MAX_ANCHOR_REPEATS: f64 = 5.0;
/// Most title parts indexed in joined form.
const MAX_TITLE_PARTS: usize = 4;
/// Characters that separate the parts of a homepage title. ` - ` (a hyphen
/// between spaces) separates too; a bare hyphen does not (`Coca-Cola`).
const TITLE_SEPARATORS: [char; 10] = ['|', '·', '•', ':', '–', '—', '»', '«', '/', '\\'];

/// Whole homepage titles, after [`normalize_text`], that say nothing about
/// the site: "Home", "Index", a site builder's default. Such a site goes by
/// its other names, as if it had no title, and is not found by "home".
const BLANK_TITLES: &[&str] = &[
    "home",
    "homepage",
    "home page",
    "index",
    "welcome",
    "untitled",
    "untitled document",
    "document",
    "main page",
    "default",
    "new tab",
    "react app",
    "vite react",
    "vite react ts",
    "my blog",
    "my wordpress blog",
    "my wordpress site",
];

/// Whether `title` says nothing about the site ([`BLANK_TITLES`]), or is an
/// unfilled template ("%siteName", "<!-- figma:title -->", "{{ title }}").
fn is_blank_title(title: &str) -> bool {
    let raw = title.trim();
    raw.starts_with("<!--")
        || raw.contains("{{")
        || raw.starts_with('%')
        || BLANK_TITLES.contains(&normalize_text(raw).as_str())
}

/// Handles on the schema's fields.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fields {
    pub(crate) domain: Field,
    pub(crate) url: Field,
    pub(crate) title: Field,
    pub(crate) description: Field,
    pub(crate) headings: Field,
    pub(crate) terms: Field,
    pub(crate) about: Field,
    pub(crate) label: Field,
    pub(crate) aliases: Field,
    pub(crate) anchors: Field,
    pub(crate) joined: Field,
    pub(crate) label_key: Field,
    pub(crate) alias_key: Field,
    pub(crate) link_score: Field,
    pub(crate) country: Field,
    pub(crate) kind_key: Field,
    pub(crate) search_url: Field,
    pub(crate) language: Field,
    pub(crate) adult: Field,
    pub(crate) key_pages: Field,
    /// Missing from indexes built before it was added.
    pub(crate) fingerprint: Option<Field>,
    /// Missing from indexes built before it was added.
    pub(crate) link_name_key: Option<Field>,
}

impl Fields {
    /// Looks the fields up by name; fails when `schema` lacks one.
    pub(crate) fn new(schema: &Schema) -> Result<Fields> {
        let field = |name: &str| {
            schema
                .get_field(name)
                .with_context(|| format!("the index has no {name:?} field"))
        };
        Ok(Fields {
            domain: field(DOMAIN)?,
            url: field(URL)?,
            title: field(TITLE)?,
            description: field(DESCRIPTION)?,
            headings: field(HEADINGS)?,
            terms: field(TERMS)?,
            about: field(ABOUT)?,
            label: field(LABEL)?,
            aliases: field(ALIASES)?,
            anchors: field(ANCHORS)?,
            joined: field(JOINED)?,
            label_key: field(LABEL_KEY)?,
            alias_key: field(ALIAS_KEY)?,
            link_score: field(LINK_SCORE)?,
            country: field(COUNTRY)?,
            kind_key: field(KIND_KEY)?,
            search_url: field(SEARCH_URL)?,
            language: field(LANGUAGE)?,
            adult: field(ADULT)?,
            key_pages: field(KEY_PAGES)?,
            fingerprint: schema.get_field(FINGERPRINT).ok(),
            link_name_key: schema.get_field(LINK_NAME_KEY).ok(),
        })
    }
}

/// The schema of a Plumb index.
pub(crate) fn schema() -> Schema {
    schema_with(true, true)
}

/// Whether an index with `schema` can be searched: it has this version's
/// schema, the one before [`LINK_NAME_KEY`] (until its next build, link
/// texts name nothing), or the one before [`FINGERPRINT`] too (near-copies
/// are not told apart either).
pub(crate) fn readable(schema: &Schema) -> bool {
    *schema == schema_with(true, true)
        || *schema == schema_with(true, false)
        || *schema == schema_with(false, false)
}

/// The schema, with or without the [`FINGERPRINT`] and [`LINK_NAME_KEY`]
/// fields.
fn schema_with(fingerprint: bool, link_names: bool) -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field(DOMAIN, STRING | STORED | FAST);
    builder.add_text_field(URL, STORED);
    builder.add_text_field(TITLE, words().set_stored());
    builder.add_text_field(DESCRIPTION, words().set_stored());
    builder.add_text_field(HEADINGS, words());
    builder.add_text_field(TERMS, words());
    builder.add_text_field(ABOUT, words().set_stored());
    builder.add_text_field(LABEL, words());
    builder.add_text_field(ALIASES, words());
    builder.add_text_field(ANCHORS, words());
    // No length normalization: a site with many names should not lose to one
    // with few, and the term frequency counts how many sources agree on a name.
    builder.add_text_field(JOINED, keys(IndexRecordOption::WithFreqs));
    builder.add_text_field(LABEL_KEY, keys(IndexRecordOption::Basic));
    builder.add_text_field(ALIAS_KEY, keys(IndexRecordOption::Basic));
    builder.add_f64_field(LINK_SCORE, FAST | STORED);
    builder.add_text_field(COUNTRY, STRING | STORED | FAST);
    builder.add_text_field(KIND_KEY, keys(IndexRecordOption::Basic));
    builder.add_text_field(SEARCH_URL, STORED);
    builder.add_text_field(LANGUAGE, STRING | STORED | FAST);
    builder.add_u64_field(ADULT, FAST);
    builder.add_text_field(KEY_PAGES, STORED);
    if fingerprint {
        builder.add_u64_field(FINGERPRINT, STORED);
    }
    if link_names {
        builder.add_text_field(LINK_NAME_KEY, keys(IndexRecordOption::WithFreqs));
    }
    builder.build()
}

/// The [`AdultLevel`] an `adult` field value stands for.
pub(crate) fn adult_from(value: u64) -> AdultLevel {
    match value {
        0 => AdultLevel::None,
        1 => AdultLevel::Suggestive,
        _ => AdultLevel::Explicit,
    }
}

/// Word-tokenized text scored with BM25 (term frequencies, no positions).
fn words() -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(WORDS_ANALYZER)
            .set_index_option(IndexRecordOption::WithFreqs),
    )
}

/// Whole values in joined form, without length normalization.
fn keys(record: IndexRecordOption) -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(JOINED_ANALYZER)
            .set_index_option(record)
            .set_fieldnorms(false),
    )
}

/// The document for `record`, whose domain is canonical
/// ([`plumb_core::canonical_domain`]). `redirect_names` are the domain
/// labels of sites that redirect to it, which name it as its own label does.
pub(crate) fn document(
    f: &Fields,
    record: &SiteRecord,
    redirect_names: &[String],
) -> TantivyDocument {
    let domain = record.domain.as_str();
    let mut doc = TantivyDocument::default();
    doc.add_text(f.domain, domain);
    // A homepage read on another site's host says what that site is.
    let borrowed = non_empty(&record.url).is_some_and(|url| crate::reads_another_site(url, domain));
    if let Some(url) = non_empty(&record.url).filter(|_| !borrowed) {
        doc.add_text(f.url, url.trim());
    }

    let label = label_text(domain);
    doc.add_text(f.label, &label);
    let label_joined = joined(&label);
    if label_joined != normalize_text(&label) {
        // `us-bank` is indexed as `us`, `bank` and `usbank`.
        doc.add_text(f.label, &label_joined);
    }
    doc.add_text(f.label_key, &label);
    doc.add_text(f.joined, &label);
    for name in redirect_names {
        doc.add_text(f.label_key, name);
        doc.add_text(f.joined, name);
        doc.add_text(f.aliases, name);
    }

    // A site whose homepage gave no title (it blocks crawlers, redirects
    // elsewhere or was not crawled yet), or one that says nothing ("Home"),
    // goes by its first name instead:
    // Wikidata's for an official site ("Gmail"), else its own
    // `og:site_name`. It is shown as the result's title, and matched as one.
    let title = non_empty(&record.title)
        .filter(|title| !borrowed && !is_blank_title(title) && !title.contains('\u{FFFD}'))
        .or_else(|| {
            record
                .aliases
                .iter()
                .map(String::as_str)
                .find(|alias| !alias.trim().is_empty())
        });
    if let Some(title) = title {
        let title = truncate_chars(title, MAX_TEXT_CHARS);
        let parts = title_parts(&title);
        if parts.len() > 1 {
            doc.add_text(f.joined, &title);
        }
        for part in parts {
            doc.add_text(f.joined, part);
        }
        doc.add_text(f.title, &title);
    }
    if let Some(description) = non_empty(&record.description).filter(|_| !borrowed) {
        doc.add_text(f.description, truncate_chars(description, MAX_TEXT_CHARS));
    }
    // Wikipedia's words for the site count like its own description, and
    // are shown when it has none.
    if let Some(intro) = non_empty(&record.intro) {
        doc.add_text(f.description, truncate_chars(intro, MAX_TEXT_CHARS));
    }
    // A model's sentence for a site with no words of its own stands in
    // for its description.
    if let Some(summary) = non_empty(&record.summary).filter(|_| {
        non_empty(&record.intro).is_none() && (borrowed || non_empty(&record.description).is_none())
    }) {
        doc.add_text(f.description, truncate_chars(summary, MAX_TEXT_CHARS));
    }
    for heading in record.headings.iter().take(MAX_HEADINGS) {
        doc.add_text(f.headings, truncate_chars(heading, MAX_TEXT_CHARS));
    }
    if !record.terms.is_empty() {
        let terms: Vec<&str> = record
            .terms
            .iter()
            .take(MAX_TERMS)
            .map(String::as_str)
            .collect();
        doc.add_text(f.terms, terms.join(" "));
    }
    if let Some(about) = non_empty(&record.about) {
        doc.add_text(f.about, truncate_chars(about, MAX_TEXT_CHARS));
    }

    let official = record.signals.official_site;
    for alias in record
        .aliases
        .iter()
        .filter(|a| !a.trim().is_empty())
        .take(MAX_ALIASES)
    {
        let alias = truncate_chars(alias, MAX_TEXT_CHARS);
        doc.add_text(f.aliases, &alias);
        doc.add_text(f.joined, &alias);
        let short = without_leading_article(&alias);
        for key in std::iter::once(alias.as_str()).chain(short.as_deref()) {
            doc.add_text(f.alias_key, key);
            if official {
                doc.add_text(f.label_key, key);
            }
        }
        if let Some(short) = &short {
            doc.add_text(f.joined, short);
        }
    }

    let link_texts = top_link_texts(&record.link_texts);
    let links: u64 = link_texts.iter().map(|lt| u64::from(lt.count)).sum();
    for (i, link_text) in link_texts.into_iter().enumerate() {
        let text = truncate_chars(&link_text.text, MAX_TEXT_CHARS);
        for _ in 0..anchor_repeats(link_text.count) {
            doc.add_text(f.anchors, &text);
        }
        if i < JOINED_LINK_TEXTS {
            doc.add_text(f.joined, &text);
        }
        if let Some(field) = f.link_name_key {
            let share = f64::from(link_text.count) / links as f64;
            if link_text.count >= MIN_LINK_NAME_SITES
                && share >= MIN_LINK_NAME_SHARE
                && reads_like_a_name(&text)
            {
                let short = without_leading_article(&text);
                for key in std::iter::once(text.as_str()).chain(short.as_deref()) {
                    for _ in 0..link_text.count.min(MAX_LINK_NAME_REPEATS) {
                        doc.add_text(field, key);
                    }
                }
            }
        }
    }

    doc.add_f64(f.link_score, f64::from(record.link_score()));
    if let Some(country) = site_country(record) {
        doc.add_text(f.country, country);
    }
    for kind in record.kinds.iter().take(MAX_KINDS) {
        let key = kind_key(kind);
        if !key.is_empty() {
            doc.add_text(f.kind_key, key);
        }
    }
    if let Some(search_url) = non_empty(&record.search_url) {
        doc.add_text(f.search_url, search_url.trim());
    }
    // What the homepage says, or the writing system its title and
    // description are in when that is plainly not the one it says.
    let text = format!(
        "{} {}",
        record.title.as_deref().unwrap_or_default(),
        record.description.as_deref().unwrap_or_default()
    );
    if let Some(language) = site_language(record.language.as_deref(), &text) {
        doc.add_text(f.language, language);
    }
    doc.add_u64(f.adult, record_adult_level(record) as u64);
    let fingerprint = f.fingerprint.zip(
        non_empty(&record.body_text)
            .filter(|_| !borrowed)
            .and_then(plumb_core::simhash::fingerprint),
    );
    if let Some((field, print)) = fingerprint {
        doc.add_u64(field, print);
    }
    let key_pages = valid_key_pages(
        key_pages_or_known(&record.key_pages, &record.domain),
        &record.domain,
    );
    if !key_pages.is_empty() {
        if let Ok(json) = serde_json::to_string(&key_pages) {
            doc.add_text(f.key_pages, json);
        }
    }
    doc
}

/// The texts of `record` the spelling model counts words in
/// ([`crate::spell_model`]): what others call the site and what it calls
/// itself, not its description, which it may stuff with search words.
pub(crate) fn spelling_texts(record: &SiteRecord, redirect_names: &[String]) -> Vec<String> {
    let domain = record.domain.as_str();
    let borrowed = non_empty(&record.url).is_some_and(|url| crate::reads_another_site(url, domain));
    let mut texts = vec![label_text(domain)];
    texts.extend(redirect_names.iter().cloned());
    texts.extend(
        record
            .aliases
            .iter()
            .filter(|a| !a.trim().is_empty())
            .take(MAX_ALIASES)
            .map(|alias| truncate_chars(alias, MAX_TEXT_CHARS)),
    );
    if let Some(title) = non_empty(&record.title)
        .filter(|title| !borrowed && !is_blank_title(title) && !title.contains('\u{FFFD}'))
    {
        texts.extend(
            title_parts(&truncate_chars(title, MAX_TEXT_CHARS))
                .into_iter()
                .map(str::to_string),
        );
    }
    texts.extend(
        top_link_texts(&record.link_texts)
            .into_iter()
            .map(|link_text| truncate_chars(&link_text.text, MAX_TEXT_CHARS)),
    );
    if let Some(about) = non_empty(&record.about) {
        texts.push(truncate_chars(about, MAX_TEXT_CHARS));
    }
    texts
}

/// A name without a leading "The": `The Wall Street Journal` -> `Wall
/// Street Journal`, so people who leave the article out still name it.
/// `None` when the name does not start with it or is nothing more.
pub(crate) fn without_leading_article(name: &str) -> Option<String> {
    let normalized = normalize_text(name);
    let rest = normalized.strip_prefix("the ")?;
    (!rest.is_empty()).then(|| rest.to_string())
}

/// The domain label as people write it: `usbank.com` -> `usbank`, and
/// punycode decoded, `xn--bcher-kva.de` -> `bücher`.
pub(crate) fn label_text(domain: &str) -> String {
    let label = domain_label(domain);
    if label.contains("xn--") {
        if let (unicode, Ok(())) = idna::domain_to_unicode(&label) {
            return unicode;
        }
    }
    label
}

/// The parts of a homepage title between separators, at most
/// [`MAX_TITLE_PARTS`]: `U.S. Bank | Personal Banking` -> `U.S. Bank`,
/// `Personal Banking`. A title without separators is its only part.
fn title_parts(title: &str) -> Vec<&str> {
    title
        .split(TITLE_SEPARATORS)
        .flat_map(|part| part.split(" - "))
        .map(str::trim)
        .filter(|part| !normalize_text(part).is_empty())
        .take(MAX_TITLE_PARTS)
        .collect()
}

/// The link texts worth indexing, most frequent first, at most [`MAX_LINK_TEXTS`].
fn top_link_texts(link_texts: &[LinkText]) -> Vec<&LinkText> {
    let mut top: Vec<&LinkText> = link_texts
        .iter()
        .filter(|lt| lt.count > 0 && !lt.text.trim().is_empty())
        .collect();
    top.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.text.cmp(&b.text)));
    top.truncate(MAX_LINK_TEXTS);
    top
}

/// Whether a link text reads like a name: a few words with a letter, not
/// an address or a sentence.
fn reads_like_a_name(text: &str) -> bool {
    let words = normalize_text(text);
    let count = words.split_whitespace().count();
    (1..=MAX_LINK_NAME_WORDS).contains(&count)
        && words.chars().count() <= MAX_LINK_NAME_CHARS
        && words.chars().any(char::is_alphabetic)
        && !text.contains("://")
        && !text.contains(['|', '<', '>', '{', '}', '@'])
}

/// How many times a link text used `count` times goes into `anchors`.
fn anchor_repeats(count: u32) -> usize {
    (1.0 + f64::from(count.max(1)).ln())
        .min(MAX_ANCHOR_REPEATS)
        .round() as usize
}

fn non_empty(text: &Option<String>) -> Option<&str> {
    text.as_deref().filter(|t| !t.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_readable() {
        assert_eq!(label_text("usbank.com"), "usbank");
        assert_eq!(label_text("us-bank-login.com"), "us-bank-login");
        assert_eq!(label_text("bbc.co.uk"), "bbc");
        assert_eq!(label_text("xn--bcher-kva.de"), "bücher");
    }

    #[test]
    fn blank_titles_say_nothing() {
        for title in [
            "Home",
            "index",
            "Welcome!",
            "%siteName",
            "<!-- figma:title -->",
            "React App",
        ] {
            assert!(is_blank_title(title), "{title}");
        }
        for title in ["Home Depot", "Welcome to Chase", "Notion", "100% Pure"] {
            assert!(!is_blank_title(title), "{title}");
        }
    }

    #[test]
    fn titles_split_at_separators() {
        assert_eq!(
            title_parts("U.S. Bank | Personal Banking"),
            ["U.S. Bank", "Personal Banking"]
        );
        assert_eq!(
            title_parts("Bank of America - Banking, Credit Cards"),
            ["Bank of America", "Banking, Credit Cards"]
        );
        assert_eq!(title_parts("Coca-Cola"), ["Coca-Cola"]);
        assert_eq!(
            title_parts("Amazon.com: Online Shopping"),
            ["Amazon.com", "Online Shopping"]
        );
        assert_eq!(title_parts(" | "), Vec::<&str>::new());
        assert_eq!(title_parts("a | b | c | d | e | f").len(), MAX_TITLE_PARTS);
    }

    #[test]
    fn frequent_link_texts_repeat_more() {
        assert_eq!(anchor_repeats(0), 1);
        assert_eq!(anchor_repeats(1), 1);
        assert_eq!(anchor_repeats(2), 2);
        assert_eq!(anchor_repeats(10), 3);
        assert_eq!(anchor_repeats(20), 4);
        assert_eq!(anchor_repeats(1_000_000), 5);
    }

    #[test]
    fn link_texts_are_sorted_and_capped() {
        let link_texts: Vec<LinkText> = (0..40u32)
            .map(|i| LinkText::with_count(format!("text {i}"), i))
            .collect();
        let top = top_link_texts(&link_texts);
        assert_eq!(top.len(), MAX_LINK_TEXTS);
        assert_eq!(top[0].count, 39);
        assert!(top.iter().all(|lt| lt.count > 0));
    }

    #[test]
    fn leading_articles_are_dropped() {
        assert_eq!(
            without_leading_article("The Wall Street Journal").as_deref(),
            Some("wall street journal")
        );
        assert_eq!(without_leading_article("The"), None);
        assert_eq!(without_leading_article("Theory"), None);
        assert_eq!(without_leading_article("U.S. Bank"), None);
    }

    #[test]
    fn schema_has_every_field() {
        let fields = Fields::new(&schema()).unwrap();
        let schema = schema();
        assert_eq!(schema.get_field_name(fields.joined), JOINED);
        assert!(Fields::new(&Schema::builder().build()).is_err());
        assert!(fields.fingerprint.is_some());
        assert!(fields.link_name_key.is_some());
        // Indexes built before link names, and before fingerprints, can
        // still be searched.
        let before_link_names = schema_with(true, false);
        let older = schema_with(false, false);
        assert!(readable(&older) && readable(&before_link_names) && readable(&schema));
        assert!(!readable(&schema_with(false, true)));
        assert!(!readable(&Schema::builder().build()));
        assert!(Fields::new(&older).unwrap().fingerprint.is_none());
        assert!(Fields::new(&before_link_names)
            .unwrap()
            .link_name_key
            .is_none());
    }

    #[test]
    fn names_are_short_and_plain() {
        assert!(reads_like_a_name("The Guardian"));
        assert!(reads_like_a_name("BBC"));
        assert!(!reads_like_a_name("https://www.theguardian.com/"));
        assert!(!reads_like_a_name(
            "read the full story on the guardian website today"
        ));
        assert!(!reads_like_a_name("..."));
    }
}
