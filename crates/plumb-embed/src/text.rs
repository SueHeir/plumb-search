//! The text of a site that is embedded.

use std::collections::HashSet;

use plumb_core::collapse_whitespace;
use plumb_core::site::{self, Item};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// Most words of a site's text that are embedded.
pub const MAX_TEXT_WORDS: usize = 100;
/// Most link texts (other sites' words for the site, most used first) in a
/// site's text.
pub const MAX_LINK_TEXTS: usize = 5;

/// SHA-256 of a site's text ([`text_hash`]).
pub type TextHash = [u8; 32];

/// The text of `site` (its stored item, [`plumb_core::site`]) to embed:
/// its names, homepage title and description, what Wikidata and Wikipedia
/// say the organization is (or, for a site with none of that, a model's
/// one-sentence summary), the [`MAX_LINK_TEXTS`] words other sites link to
/// it with most, its homepage headings and the start of its homepage text,
/// in that order, joined by ". " and cut at [`MAX_TEXT_WORDS`] words. Each
/// part is put in Unicode NFC and its whitespace collapsed, and a part
/// equal to an earlier one (ignoring case) is left out. The same site
/// always gives the same text; a site with none of these gives an empty
/// one.
pub fn site_text(site: &Item) -> String {
    site_text_words(site, MAX_TEXT_WORDS)
}

/// [`site_text`] cut at `words` words instead of [`MAX_TEXT_WORDS`], for
/// models that read longer texts.
pub fn site_text_words(site: &Item, words: usize) -> String {
    let link_texts = site::link_texts(site);
    let link_texts = link_texts
        .iter()
        .take(MAX_LINK_TEXTS)
        .map(|link| link.text.as_str());
    let wikidata_about = site::wikidata_about(site);
    let intro = site::intro(site);
    let summary = site::summary(site)
        .filter(|_| site.about.is_none() && wikidata_about.is_none() && intro.is_none());
    let headings = site::headings(site);
    let parts = site
        .names
        .iter()
        .map(String::as_str)
        .chain(site.title.as_deref())
        .chain(site.about.as_deref())
        .chain(wikidata_about)
        .chain(intro)
        .chain(summary)
        .chain(link_texts)
        .chain(headings.iter().map(String::as_str))
        .chain(site.text.as_deref());
    joined(parts, words)
}

/// The text of a page to embed: its title, the line under it and its
/// snippet (for a Wikipedia article, its description and lead), joined as
/// [`site_text`] joins a site's parts and cut at `words` words.
pub fn page_text(
    title: Option<&str>,
    about: Option<&str>,
    snippet: Option<&str>,
    words: usize,
) -> String {
    joined([title, about, snippet].into_iter().flatten(), words)
}

/// `parts` in Unicode NFC with their whitespace collapsed, a part equal to
/// an earlier one (ignoring case) left out, joined by ". " and cut at
/// `words` words.
fn joined<'a>(parts: impl Iterator<Item = &'a str>, words: usize) -> String {
    let mut seen = HashSet::new();
    let mut kept = Vec::new();
    for part in parts {
        let part = collapse_whitespace(&part.nfc().collect::<String>());
        if !part.is_empty() && seen.insert(part.to_lowercase()) {
            kept.push(part);
        }
    }
    let text = kept.join(". ");
    let kept: Vec<&str> = text.split_whitespace().take(words).collect();
    kept.join(" ")
}

/// SHA-256 of `text`, so nodes can tell whether they embedded the same text.
pub fn text_hash(text: &str) -> TextHash {
    Sha256::digest(text.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::path::Path;

    use plumb_core::{stored, KeyPage, LinkText, Signals, SiteRecord};

    use super::*;

    /// A site as a node reads it from its records file to embed it: as
    /// the file holds it, not cleaned.
    fn as_read(record: SiteRecord) -> Item {
        stored::read_item(&stored::line(&record).unwrap()).unwrap()
    }

    /// [`site_text_words`] as it was on records, before it read sites'
    /// items: what the item version is checked against.
    fn record_text_words(record: &SiteRecord, words: usize) -> String {
        let link_texts = record
            .link_texts
            .iter()
            .take(MAX_LINK_TEXTS)
            .map(|link| &link.text);
        let summary = record.summary.as_ref().filter(|_| {
            record.description.is_none() && record.about.is_none() && record.intro.is_none()
        });
        let parts = record
            .aliases
            .iter()
            .chain(&record.title)
            .chain(&record.description)
            .chain(&record.about)
            .chain(&record.intro)
            .chain(summary)
            .chain(link_texts)
            .chain(&record.headings)
            .chain(&record.body_text);
        let mut seen = HashSet::new();
        let mut kept = Vec::new();
        for part in parts {
            let part = collapse_whitespace(&part.nfc().collect::<String>());
            if !part.is_empty() && seen.insert(part.to_lowercase()) {
                kept.push(part);
            }
        }
        let text = kept.join(". ");
        let kept: Vec<&str> = text.split_whitespace().take(words).collect();
        kept.join(" ")
    }

    #[test]
    fn a_sites_text_is_its_records() {
        for record in golden_sites() {
            for words in [MAX_TEXT_WORDS, 0, 1, 3, 7, 400] {
                let want = record_text_words(&record, words);
                let site = as_read(record.clone());
                assert_eq!(site_text_words(&site, words), want, "{}", record.domain);
                // Its record's stored item as it is, and cleaned.
                let as_is = record.clone().stored_item_as_is();
                assert_eq!(site_text_words(&as_is, words), want, "{}", record.domain);
                let mut clean = record.clone();
                clean.clean();
                let want = record_text_words(&clean, words);
                let site = clean.to_stored_item();
                assert_eq!(site_text_words(&site, words), want, "{}", record.domain);
            }
        }
    }

    /// Sites that differ in what their text is made of: every part and
    /// none, parts the same but for case, Unicode form or whitespace,
    /// control characters, a summary beside each text that hides it, more
    /// link texts than are kept, text longer than is embedded and text with
    /// no spaces, and parts that say nothing.
    fn golden_sites() -> Vec<SiteRecord> {
        let mut sites = vec![SiteRecord::new("empty.com")];

        let mut tesla = SiteRecord::new("tesla.com");
        tesla.aliases = vec!["Tesla".into(), "Tesla, Inc.".into()];
        tesla.title = Some("Electric  Cars,\tSolar & Clean Energy | Tesla".into());
        tesla.description = Some("tesla".into());
        tesla.about = Some("American electric vehicle and clean energy company".into());
        tesla.headings = vec!["Model 3".into(), "Model Y".into(), "MODEL 3".into()];
        sites.push(tesla);

        let mut cafe = SiteRecord::new("cafe.fr");
        cafe.aliases = vec!["Caf\u{e9} de Flore".into()];
        cafe.title = Some("Cafe\u{301} de Flore".into());
        cafe.description = Some("Le caf\u{e9}\u{7} historique de Saint-Germain".into());
        cafe.body_text = Some("Ouvert\u{1B}$B tous les jours\u{0}.".into());
        sites.push(cafe);

        let summary = |domain: &str| {
            let mut record = SiteRecord::new(domain);
            record.title = Some(format!("The {domain} homepage"));
            record.summary = Some("A model's one-sentence summary of the site.".into());
            record
        };
        sites.push(summary("summary-only.org"));
        let mut described = summary("summary-description.org");
        described.description = Some("The homepage's own description.".into());
        sites.push(described);
        let mut about = summary("summary-about.org");
        about.about = Some("what Wikidata says it is".into());
        sites.push(about);
        let mut intro = summary("summary-intro.org");
        intro.intro = Some("Summary Intro is an organization.".into());
        sites.push(intro);

        let mut linked = SiteRecord::new("chicagotribune.com");
        linked.title = Some("Chicago Tribune".into());
        linked.link_texts = [
            "chicago tribune",
            "chicago news",
            "",
            "Trib",
            "  ",
            "the tribune",
            "news from chicago",
            "seventh",
        ]
        .iter()
        .enumerate()
        .map(|(i, text)| LinkText::from_linkers(*text, (1 << (8 - i)) - 1))
        .collect();
        sites.push(linked);

        let mut long = SiteRecord::new("long.org");
        long.title = Some("A long page".into());
        long.headings = (1..=12).map(|i| format!("Heading {i}")).collect();
        long.body_text = Some(
            (1..=150)
                .map(|i| format!("word{i}"))
                .collect::<Vec<_>>()
                .join(" "),
        );
        sites.push(long);

        let mut chinese = SiteRecord::new("gov.cn");
        chinese.title = Some("中华人民共和国中央人民政府门户网站".into());
        chinese.body_text = Some("国务院".repeat(700));
        sites.push(chinese);

        let mut blank = SiteRecord::new("blank.net");
        blank.aliases = vec!["".into(), "  ".into(), "\u{a0}".into(), "Blank".into()];
        blank.title = Some("".into());
        blank.description = Some("\u{3000}\t\n".into());
        blank.about = Some("".into());
        blank.summary = Some("A blank site.".into());
        blank.headings = vec!["".into(), " blank ".into()];
        blank.body_text = Some(" ".into());
        sites.push(blank);

        let mut untidy = SiteRecord::new("untidy.de");
        untidy.aliases = vec![" Untidy GmbH ".into(), "Untidy\u{a0}GmbH".into()];
        untidy.title = Some("\u{1B}$B%3%_%C%H\u{1B}(B Untidy\u{7}".into());
        untidy.headings = vec!["First\nline".into(), "Caf\u{e9}\r\n".into()];
        untidy.intro = Some("Untidy GmbH is a\u{85}company.".into());
        sites.push(untidy);

        let mut hindi = SiteRecord::new("aajtak.in");
        hindi.aliases = vec!["\u{91c}\u{93c}\u{93e}\u{928}".into()];
        hindi.title = Some("\u{95b}\u{93e}\u{928}".into());
        hindi.description = Some("ज़माना".into());
        sites.push(hindi);

        let mut vietnamese = SiteRecord::new("nchmf.gov.vn");
        vietnamese.title = Some("Dư\u{323} ba\u{301}o thời tiết".into());
        vietnamese.about = Some("Dự báo thời tiết".into());
        sites.push(vietnamese);

        let mut full = SiteRecord::new("python.org");
        full.url = Some("https://www.python.org/".into());
        full.title = Some("Welcome to Python.org".into());
        full.description = Some("The official home of the Python Programming Language".into());
        full.aliases = vec!["Python".into(), "Python Software Foundation".into()];
        full.about = Some("general-purpose programming language".into());
        full.intro = Some("Python is a high-level programming language.".into());
        full.summary = Some("Python's home.".into());
        full.headings = vec!["Get Started".into(), "Download".into()];
        full.terms = vec!["python".into(), "download".into()];
        full.kinds = vec!["programming language".into()];
        full.country = Some("US".into());
        full.language = Some("en".into());
        full.search_url = Some("https://www.python.org/search/?q={searchTerms}".into());
        full.body_text =
            Some("Python is a programming language that lets you work quickly.".into());
        full.add_link_text("python", "example.org");
        full.add_link_text("Python", "other.org");
        full.add_link_text("the python language", "example.org");
        full.key_pages = vec![KeyPage {
            label: "Docs".into(),
            url: "https://docs.python.org/".into(),
        }];
        full.links_to = vec!["pypi.org".into()];
        full.signals = Signals {
            tranco_rank: Some(400),
            harmonic_rank: Some(300),
            pagerank_rank: Some(250),
            linking_domains: 12,
            official_site: true,
            sitelinks: 150,
            fact_checks: 3,
            fact_agrees: 2,
        };
        full.crawled_at = Some(1_791_605_520);
        full.crawl_version = 7;
        full.crawl_attempted_at = Some(1_791_605_600);
        full.icon = Some("https://www.python.org/favicon.ico".into());
        sites.push(full);
        sites
    }

    /// The text embedded of each site of [`golden_sites`], as a node reads
    /// it ([`as_read`]), at [`MAX_TEXT_WORDS`] and at fewer and more words,
    /// with its hash, against `text_golden.txt` beside this file. Written
    /// when the text was made of site records, before it was made of
    /// sites' items (step 7c of the data format plan): nodes compare the
    /// hashes of each other's texts, so the text may not change. Never
    /// written again to make a test pass: `PLUMB_WRITE_GOLDEN=1 cargo test
    /// -p plumb-embed golden` writes it.
    #[test]
    fn site_texts_match_the_golden_file() {
        let mut out = String::from(
            "# The text plumb-embed embeds of each site: its domain, the most words\n\
             # embedded, the text's SHA-256 and the text as JSON. Written by\n\
             # `PLUMB_WRITE_GOLDEN=1 cargo test -p plumb-embed golden`.\n",
        );
        for record in golden_sites() {
            let domain = record.domain.clone();
            let site = as_read(record);
            for words in [MAX_TEXT_WORDS, 3, 400] {
                let text = site_text_words(&site, words);
                let hash: String = text_hash(&text)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                let json = serde_json::to_string(&text).unwrap();
                writeln!(out, "{domain}\t{words}\t{hash}\t{json}").unwrap();
            }
            assert_eq!(site_text(&site), site_text_words(&site, MAX_TEXT_WORDS));
        }
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/text_golden.txt");
        if std::env::var_os("PLUMB_WRITE_GOLDEN").is_some() {
            std::fs::write(&path, &out).unwrap();
            return;
        }
        let golden = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
        for (n, (want, got)) in golden.lines().zip(out.lines()).enumerate() {
            assert_eq!(want, got, "{}:{} differs", path.display(), n + 1);
        }
        assert_eq!(golden.lines().count(), out.lines().count());
    }

    #[test]
    fn site_text_is_fixed_and_short() {
        let mut tesla = site::new("tesla.com");
        tesla.names = vec!["Tesla".into(), "Tesla, Inc.".into()];
        tesla.title = Some("Electric  Cars,\tSolar & Clean Energy | Tesla".into());
        tesla.about = Some("tesla".into());
        let about = "American electric vehicle and clean energy company";
        site::set_wikidata_about(&mut tesla, Some(about.into()));
        site::set_headings(&mut tesla, vec!["Model 3".into(), "Model Y".into()]);
        assert_eq!(
            site_text(&tesla),
            "Tesla. Tesla, Inc.. Electric Cars, Solar & Clean Energy | Tesla. \
             American electric vehicle and clean energy company. Model 3. Model Y"
        );
        assert_eq!(
            text_hash(&site_text(&tesla)),
            text_hash(&site_text(&tesla.clone()))
        );

        // Composed and decomposed accents give the same text.
        let mut composed = site::new("cafe.fr");
        composed.title = Some("Caf\u{e9}".into());
        let mut decomposed = composed.clone();
        decomposed.title = Some("Cafe\u{301}".into());
        assert_eq!(site_text(&composed), site_text(&decomposed));

        site::set_headings(&mut tesla, vec!["word ".repeat(500)]);
        assert_eq!(site_text(&tesla).split_whitespace().count(), MAX_TEXT_WORDS);
        assert_eq!(site_text(&site::new("empty.com")), "");

        // Other sites' words for a site give it text before any crawl.
        let mut linked = site::new("chicagotribune.com");
        let texts: Vec<LinkText> = ["Chicago Tribune", "chicago news", "a", "b", "c", "d"]
            .iter()
            .map(|text| LinkText::from_linkers(*text, 1))
            .collect();
        site::set_link_texts(&mut linked, &texts);
        assert_eq!(site_text(&linked), "Chicago Tribune. chicago news. a. b. c");
    }

    #[test]
    fn page_text_joins_title_description_and_lead() {
        assert_eq!(
            page_text(
                Some("Napoleon"),
                Some("French emperor (1769–1821)"),
                Some("Napoleon  Bonaparte was a French\nmilitary officer"),
                MAX_TEXT_WORDS
            ),
            "Napoleon. French emperor (1769–1821). Napoleon Bonaparte was a French military officer"
        );
        assert_eq!(
            page_text(Some("Iron lung"), None, Some("iron lung"), 100),
            "Iron lung"
        );
        assert_eq!(page_text(Some("a b c"), None, None, 2), "a b");
        assert_eq!(page_text(None, None, None, 100), "");
    }
}
