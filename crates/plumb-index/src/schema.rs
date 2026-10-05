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
//! | `key_pages`   | the site's key pages, as JSON                        | stored, sitelinks           |
//!
//! An official website's aliases (its Wikidata names) also go into
//! `label_key`, so they name the site as strongly as its domain does, and
//! every alias starting with "The" is also keyed without it.

use anyhow::{Context, Result};
use plumb_core::key_pages::valid_key_pages;
use plumb_core::{
    domain_label, joined, kind_key, normalize_text, site_country, truncate_chars, LinkText,
    SiteRecord, MAX_ALIASES, MAX_HEADINGS, MAX_KINDS, MAX_LINK_TEXTS, MAX_TEXT_CHARS,
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
pub(crate) const KEY_PAGES: &str = "key_pages";

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

/// Handles on the schema's fields.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fields {
    pub(crate) domain: Field,
    pub(crate) url: Field,
    pub(crate) title: Field,
    pub(crate) description: Field,
    pub(crate) headings: Field,
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
    pub(crate) key_pages: Field,
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
            key_pages: field(KEY_PAGES)?,
        })
    }
}

/// The schema of a Plumb index.
pub(crate) fn schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field(DOMAIN, STRING | STORED | FAST);
    builder.add_text_field(URL, STORED);
    builder.add_text_field(TITLE, words().set_stored());
    builder.add_text_field(DESCRIPTION, words().set_stored());
    builder.add_text_field(HEADINGS, words());
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
    builder.add_text_field(KEY_PAGES, STORED);
    builder.build()
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
    if let Some(url) = non_empty(&record.url) {
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

    if let Some(title) = non_empty(&record.title) {
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
    if let Some(description) = non_empty(&record.description) {
        doc.add_text(f.description, truncate_chars(description, MAX_TEXT_CHARS));
    }
    // Wikipedia's words for the site count like its own description, and
    // are shown when it has none.
    if let Some(intro) = non_empty(&record.intro) {
        doc.add_text(f.description, truncate_chars(intro, MAX_TEXT_CHARS));
    }
    for heading in record.headings.iter().take(MAX_HEADINGS) {
        doc.add_text(f.headings, truncate_chars(heading, MAX_TEXT_CHARS));
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

    for (i, link_text) in top_link_texts(&record.link_texts).into_iter().enumerate() {
        let text = truncate_chars(&link_text.text, MAX_TEXT_CHARS);
        for _ in 0..anchor_repeats(link_text.count) {
            doc.add_text(f.anchors, &text);
        }
        if i < JOINED_LINK_TEXTS {
            doc.add_text(f.joined, &text);
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
    let key_pages = valid_key_pages(record.key_pages.clone(), &record.domain);
    if !key_pages.is_empty() {
        if let Ok(json) = serde_json::to_string(&key_pages) {
            doc.add_text(f.key_pages, json);
        }
    }
    doc
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
    }
}
