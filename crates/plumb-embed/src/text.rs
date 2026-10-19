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

    use plumb_core::site::CrawlMarks;
    use plumb_core::{stored, KeyPage, LinkText, Signals};

    use super::*;

    /// A site as a node reads it from its records file to embed it: as
    /// the file holds it, not cleaned.
    fn as_read(site: &Item) -> Item {
        stored::read_item(&stored::item_as_line(site).unwrap()).unwrap()
    }

    /// Fewer words are the first words of the text.
    #[test]
    fn a_site_s_text_is_cut_at_a_number_of_words() {
        for site in golden_sites() {
            let site = as_read(&site);
            let whole = site_text_words(&site, 400);
            for words in [MAX_TEXT_WORDS, 0, 1, 3, 7] {
                let first: Vec<&str> = whole.split_whitespace().take(words).collect();
                assert_eq!(
                    site_text_words(&site, words),
                    first.join(" "),
                    "{}",
                    site.id
                );
            }
        }
    }

    /// Sites that differ in what their text is made of: every part and
    /// none, parts the same but for case, Unicode form or whitespace,
    /// control characters, a summary beside each text that hides it, more
    /// link texts than are kept, text longer than is embedded and text with
    /// no spaces, and parts that say nothing.
    fn golden_sites() -> Vec<Item> {
        let mut sites = vec![site::new("empty.com")];

        let mut tesla = site::new("tesla.com");
        tesla.names = vec!["Tesla".into(), "Tesla, Inc.".into()];
        tesla.title = Some("Electric  Cars,\tSolar & Clean Energy | Tesla".into());
        tesla.about = Some("tesla".into());
        let about = "American electric vehicle and clean energy company";
        site::set_wikidata_about(&mut tesla, Some(about.into()));
        let headings = vec!["Model 3".into(), "Model Y".into(), "MODEL 3".into()];
        site::set_headings(&mut tesla, headings);
        sites.push(tesla);

        let mut cafe = site::new("cafe.fr");
        cafe.names = vec!["Caf\u{e9} de Flore".into()];
        cafe.title = Some("Cafe\u{301} de Flore".into());
        cafe.about = Some("Le caf\u{e9}\u{7} historique de Saint-Germain".into());
        cafe.text = Some("Ouvert\u{1B}$B tous les jours\u{0}.".into());
        sites.push(cafe);

        let summary = |domain: &str| {
            let mut site = site::new(domain);
            site.title = Some(format!("The {domain} homepage"));
            let summary = "A model's one-sentence summary of the site.";
            site::set_summary(&mut site, Some(summary.into()));
            site
        };
        sites.push(summary("summary-only.org"));
        let mut described = summary("summary-description.org");
        described.about = Some("The homepage's own description.".into());
        sites.push(described);
        let mut about = summary("summary-about.org");
        site::set_wikidata_about(&mut about, Some("what Wikidata says it is".into()));
        sites.push(about);
        let mut intro = summary("summary-intro.org");
        site::set_intro(&mut intro, Some("Summary Intro is an organization.".into()));
        sites.push(intro);

        let mut linked = site::new("chicagotribune.com");
        linked.title = Some("Chicago Tribune".into());
        let link_texts: Vec<LinkText> = [
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
        site::set_link_texts(&mut linked, &link_texts);
        sites.push(linked);

        let mut long = site::new("long.org");
        long.title = Some("A long page".into());
        site::set_headings(
            &mut long,
            (1..=12).map(|i| format!("Heading {i}")).collect(),
        );
        long.text = Some(
            (1..=150)
                .map(|i| format!("word{i}"))
                .collect::<Vec<_>>()
                .join(" "),
        );
        sites.push(long);

        let mut chinese = site::new("gov.cn");
        chinese.title = Some("中华人民共和国中央人民政府门户网站".into());
        chinese.text = Some("国务院".repeat(700));
        sites.push(chinese);

        let mut blank = site::new("blank.net");
        blank.names = vec!["".into(), "  ".into(), "\u{a0}".into(), "Blank".into()];
        blank.title = Some("".into());
        blank.about = Some("\u{3000}\t\n".into());
        site::set_wikidata_about(&mut blank, Some("".into()));
        site::set_summary(&mut blank, Some("A blank site.".into()));
        site::set_headings(&mut blank, vec!["".into(), " blank ".into()]);
        blank.text = Some(" ".into());
        sites.push(blank);

        let mut untidy = site::new("untidy.de");
        untidy.names = vec![" Untidy GmbH ".into(), "Untidy\u{a0}GmbH".into()];
        untidy.title = Some("\u{1B}$B%3%_%C%H\u{1B}(B Untidy\u{7}".into());
        let headings = vec!["First\nline".into(), "Caf\u{e9}\r\n".into()];
        site::set_headings(&mut untidy, headings);
        site::set_intro(&mut untidy, Some("Untidy GmbH is a\u{85}company.".into()));
        sites.push(untidy);

        let mut hindi = site::new("aajtak.in");
        hindi.names = vec!["\u{91c}\u{93c}\u{93e}\u{928}".into()];
        hindi.title = Some("\u{95b}\u{93e}\u{928}".into());
        hindi.about = Some("ज़माना".into());
        sites.push(hindi);

        let mut vietnamese = site::new("nchmf.gov.vn");
        vietnamese.title = Some("Dư\u{323} ba\u{301}o thời tiết".into());
        site::set_wikidata_about(&mut vietnamese, Some("Dự báo thời tiết".into()));
        sites.push(vietnamese);

        let mut full = site::new("python.org");
        full.url = Some("https://www.python.org/".into());
        full.title = Some("Welcome to Python.org".into());
        full.about = Some("The official home of the Python Programming Language".into());
        full.names = vec!["Python".into(), "Python Software Foundation".into()];
        let about = "general-purpose programming language";
        site::set_wikidata_about(&mut full, Some(about.into()));
        let intro = "Python is a high-level programming language.";
        site::set_intro(&mut full, Some(intro.into()));
        site::set_summary(&mut full, Some("Python's home.".into()));
        site::set_headings(&mut full, vec!["Get Started".into(), "Download".into()]);
        site::set_terms(&mut full, vec!["python".into(), "download".into()]);
        site::set_kinds(&mut full, vec!["programming language".into()]);
        site::set_country(&mut full, Some("US".into()));
        full.lang = Some("en".into());
        let search = "https://www.python.org/search/?q={searchTerms}";
        site::set_search_url(&mut full, Some(search.into()));
        full.text = Some("Python is a programming language that lets you work quickly.".into());
        site::add_link_text(&mut full, "python", "example.org");
        site::add_link_text(&mut full, "Python", "other.org");
        site::add_link_text(&mut full, "the python language", "example.org");
        let docs = KeyPage {
            label: "Docs".into(),
            url: "https://docs.python.org/".into(),
        };
        site::set_key_pages(&mut full, vec![docs]);
        site::set_links_to(&mut full, vec!["pypi.org".into()]);
        let signals = Signals {
            tranco_rank: Some(400),
            harmonic_rank: Some(300),
            pagerank_rank: Some(250),
            linking_domains: 12,
            official_site: true,
            sitelinks: 150,
            fact_checks: 3,
            fact_agrees: 2,
        };
        site::set_signals(&mut full, &signals);
        site::set_crawled(&mut full, Some(1_791_605_520));
        site::set_crawl_version(&mut full, 7);
        let marks = CrawlMarks {
            tried: Some(1_791_605_600),
            ..CrawlMarks::default()
        };
        site::set_crawl_marks(&mut full, marks);
        site::set_icon(&mut full, Some("https://www.python.org/favicon.ico".into()));
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
        for site in golden_sites() {
            let domain = site.id.clone();
            let site = as_read(&site);
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
