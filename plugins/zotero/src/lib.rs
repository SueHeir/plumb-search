//! A Plumb Search plugin for your Zotero library, through the local API
//! that Zotero 7 and later serve on your computer, a copy of Zotero's web
//! API (<https://www.zotero.org/support/dev/web_api/v3/basics>):
//!
//! - every search lists up to five items whose titles, authors or year
//!   hold all of its words;
//! - `zotero ...` or `my papers ...` searches every field, the notes and
//!   the text of PDFs too, for up to ten items.
//!
//! It needs Zotero open on the node's computer, with "Allow other
//! applications on this computer to communicate with Zotero" ticked in its
//! settings; see `README.md`. Build it with
//! `cargo build --release -p plumb-plugin-zotero --target wasm32-unknown-unknown`.

use std::collections::{BTreeMap, HashMap, HashSet};

use plumb_plugin::{encode, get, Error, Item, Query};
use serde::Deserialize;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

/// The local API's address for the library of whoever uses this Zotero.
const API: &str = "http://127.0.0.1:23119/api/users/0";
/// Items listed for a search, and for one with a keyword.
const SHOWN: usize = 5;
const SHOWN_ASKED: usize = 10;
/// Items asked of Zotero before they are matched and ranked here: fewer
/// when every field is searched, as notes and annotations then come too,
/// each in full, and a plugin may read 2 MB of an answer.
const FETCHED: usize = 50;
const FETCHED_EVERYWHERE: usize = 25;

/// Where an item came out, by the first of these fields it has.
const VENUES: [&str; 15] = [
    "publicationTitle",
    "proceedingsTitle",
    "bookTitle",
    "encyclopediaTitle",
    "websiteTitle",
    "blogTitle",
    "forumTitle",
    "programTitle",
    "university",
    "institution",
    "court",
    "repository",
    "publisher",
    "label",
    "company",
];

/// One item as the local API answers it.
#[derive(Debug, Deserialize)]
struct Found {
    #[serde(default)]
    links: Links,
    #[serde(default)]
    meta: Meta,
    data: Data,
}

#[derive(Debug, Default, Deserialize)]
struct Links {
    /// The item in the zotero.org web library, when Zotero syncs there.
    alternate: Option<Link>,
}

#[derive(Debug, Deserialize)]
struct Link {
    href: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    /// "Vaswani et al."
    creator_summary: Option<String>,
    /// "2017-06-12", or "2017".
    parsed_date: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Data {
    key: String,
    item_type: String,
    parent_item: Option<String>,
    #[serde(default)]
    creators: Vec<Creator>,
    /// The other fields, which differ from one item type to the next.
    #[serde(flatten)]
    fields: BTreeMap<String, serde_json::Value>,
}

impl Data {
    /// The field's text, when it has some.
    fn text(&self, field: &str) -> Option<&str> {
        let text = self.fields.get(field)?.as_str()?.trim();
        (!text.is_empty()).then_some(text)
    }

    /// Its title, in whichever field its type keeps it.
    fn title(&self) -> Option<&str> {
        ["title", "caseName", "nameOfAct", "subject"]
            .into_iter()
            .find_map(|field| self.text(field))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Creator {
    first_name: Option<String>,
    last_name: Option<String>,
    name: Option<String>,
}

fn search(query: &Query) -> Result<Vec<Item>, Error> {
    let words = words(&query.terms);
    if words.is_empty() {
        return Ok(Vec::new());
    }
    if query.keyword.is_some() {
        return everywhere(&query.terms, &words).map_err(explain);
    }
    // This runs for every search, and a closed Zotero is no failure.
    match by_title(&words) {
        Err(Error::Failed | Error::Status(403 | 404)) => Ok(Vec::new()),
        found => found,
    }
}

/// Items whose titles, authors or year hold every one of `words`.
fn by_title(words: &[String]) -> Result<Vec<Item>, Error> {
    let url = format!(
        "{API}/items/top?q={}&limit={FETCHED}",
        encode(&words.join(" "))
    );
    Ok(listed(fetch(&url)?, words, true, SHOWN))
}

/// Items with `terms` in any field, their notes or their PDFs' text.
fn everywhere(terms: &str, words: &[String]) -> Result<Vec<Item>, Error> {
    let url = format!(
        "{API}/items?q={}&qmode=everything&limit={FETCHED_EVERYWHERE}",
        encode(terms)
    );
    Ok(listed(
        with_parents(fetch(&url)?)?,
        words,
        false,
        SHOWN_ASKED,
    ))
}

fn fetch(url: &str) -> Result<Vec<Found>, Error> {
    get(url)?.json()
}

/// What `everywhere` found with each note, PDF or annotation in its
/// item's place: Zotero finds those as items of their own, and an
/// annotation's parent is a PDF, whose parent is the item. Both are asked
/// for by type, as `/items?itemKey=` alone sends each item's notes and
/// annotations along with it, and the API can leave out only one type.
fn with_parents(found: Vec<Found>) -> Result<Vec<Found>, Error> {
    let order: Vec<String> = found.iter().map(|f| f.data.key.clone()).collect();
    let mut by_key: HashMap<String, Found> =
        found.into_iter().map(|f| (f.data.key.clone(), f)).collect();
    for (of, url) in [
        (Some("annotation"), "items?itemType=attachment&itemKey="),
        (None, "items/top?itemKey="),
    ] {
        let missing: HashSet<String> = by_key
            .values()
            .filter(|f| of.is_none_or(|of| f.data.item_type == of))
            .filter_map(|f| f.data.parent_item.clone())
            .filter(|key| {
                !by_key.contains_key(key) && key.chars().all(|c| c.is_ascii_alphanumeric())
            })
            .collect();
        if missing.is_empty() {
            continue;
        }
        let keys: Vec<String> = missing.into_iter().collect();
        for parent in fetch(&format!("{API}/{url}{}", keys.join(",")))? {
            by_key.entry(parent.data.key.clone()).or_insert(parent);
        }
    }
    let mut listed = Vec::new();
    for mut key in order {
        for _ in 0..3 {
            match by_key.get(&key).and_then(|f| f.data.parent_item.clone()) {
                Some(parent) if by_key.contains_key(&parent) => key = parent,
                _ => break,
            }
        }
        if let Some(found) = by_key.remove(&key) {
            listed.push(found);
        }
    }
    Ok(listed)
}

/// The first `count` items as results, those with more of `words` in
/// their titles first. With `whole`, only items whose titles, authors or
/// year hold every word, each from its start: "rust" is not in "Trust".
fn listed(found: Vec<Found>, words: &[String], whole: bool, count: usize) -> Vec<Item> {
    let mut ranked: Vec<(usize, Found)> = found
        .into_iter()
        .filter(|found| listable(&found.data))
        .filter(|found| {
            let about = about(found);
            !whole || words.iter().all(|word| has(&about, word))
        })
        .map(|found| {
            let title = self::words(found.data.title().unwrap_or(""));
            let score = words.iter().filter(|word| has(&title, word)).count();
            (score, found)
        })
        .collect();
    // Stable, so Zotero's order (last changed first) breaks ties.
    ranked.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    ranked
        .into_iter()
        .filter_map(|(_, found)| item(&found))
        .take(count)
        .collect()
}

/// Whether an item is one to list: not a note or an annotation, nor a file
/// or link attached to another item.
fn listable(data: &Data) -> bool {
    match data.item_type.as_str() {
        "note" | "annotation" => false,
        "attachment" => data.parent_item.is_none(),
        _ => true,
    }
}

/// The words of an item's title, authors and year.
fn about(found: &Found) -> Vec<String> {
    let mut text = found.data.title().unwrap_or("").to_string();
    for creator in &found.data.creators {
        for part in [&creator.first_name, &creator.last_name, &creator.name] {
            text.push(' ');
            text.push_str(part.as_deref().unwrap_or(""));
        }
    }
    text.push(' ');
    text.push_str(year(found).unwrap_or(""));
    words(&text)
}

/// Whether `word` starts one of `words`, or is anywhere in one in a script
/// written without spaces (Chinese, Japanese, Thai).
fn has(words: &[String], word: &str) -> bool {
    let unspaced = word
        .chars()
        .any(|c| matches!(c, '\u{0E00}'..='\u{0EFF}' | '\u{3040}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}'));
    words.iter().any(|w| {
        if unspaced {
            w.contains(word)
        } else {
            w.starts_with(word)
        }
    })
}

fn item(found: &Found) -> Option<Item> {
    let data = &found.data;
    let mut item = Item::new(data.title()?, link(found)?);
    let mut snippet: Vec<&str> = [found.meta.creator_summary.as_deref(), year(found)]
        .into_iter()
        .flatten()
        .collect();
    snippet.extend(VENUES.iter().find_map(|field| data.text(field)));
    let mut snippet = snippet.join(", ");
    if let Some(abstract_note) = data.text("abstractNote") {
        if !snippet.is_empty() {
            snippet.push_str(". ");
        }
        snippet.push_str(abstract_note);
    }
    if !snippet.is_empty() {
        item = item.snippet(snippet);
    }
    if let Some(badge) = badge(&data.item_type) {
        item = item.badge(badge);
    }
    Some(item)
}

/// Where a result goes: the item's own address, its DOI, its book at Open
/// Library, or the item in the web library. An item with none of them is
/// left out, as a result needs an address.
fn link(found: &Found) -> Option<String> {
    let data = &found.data;
    if let Some(url) = data.text("url") {
        let lower = url.to_ascii_lowercase();
        if lower.starts_with("https://") || lower.starts_with("http://") {
            return Some(url.to_string());
        }
    }
    if let Some(doi) = data
        .text("DOI")
        .and_then(|doi| doi.find("10.").map(|at| &doi[at..]))
    {
        return Some(format!(
            "https://doi.org/{}",
            encode(doi).replace("%2F", "/")
        ));
    }
    if let Some(isbn) = data.text("ISBN").and_then(isbn) {
        return Some(format!("https://openlibrary.org/isbn/{isbn}"));
    }
    let web = found.links.alternate.as_ref()?.href.as_str();
    web.starts_with("https://www.zotero.org/")
        .then(|| web.to_string())
}

/// The first ISBN in the field, without its hyphens: a field can hold
/// several, and words such as "ISBN" or "(pbk.)".
fn isbn(field: &str) -> Option<String> {
    field
        .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
        .find_map(|token| {
            let isbn: String = token
                .trim_start_matches(|c: char| !c.is_ascii_digit())
                .chars()
                .filter(|c| *c != '-')
                .collect();
            let digits = isbn.trim_end_matches(['X', 'x']);
            let ok = matches!(isbn.len(), 10 | 13)
                && isbn.len() - digits.len() <= 1
                && digits.chars().all(|c| c.is_ascii_digit());
            ok.then_some(isbn)
        })
}

fn year(found: &Found) -> Option<&str> {
    let date = found.meta.parsed_date.as_deref()?;
    let year = date.get(..4)?;
    // Zotero gives "0000" for a date it cannot read, such as "n.d.".
    (year != "0000" && year.chars().all(|c| c.is_ascii_digit())).then_some(year)
}

fn badge(item_type: &str) -> Option<&'static str> {
    Some(match item_type {
        "journalArticle" | "magazineArticle" => "Article",
        "newspaperArticle" => "News",
        "book" => "Book",
        "bookSection" => "Chapter",
        "conferencePaper" => "Paper",
        "preprint" => "Preprint",
        "thesis" => "Thesis",
        "report" => "Report",
        "webpage" => "Web page",
        "blogPost" => "Blog post",
        "case" => "Case",
        "statute" => "Statute",
        "patent" => "Patent",
        "dataset" => "Dataset",
        "computerProgram" => "Software",
        "videoRecording" => "Video",
        "podcast" => "Podcast",
        _ => return None,
    })
}

/// What a search with a keyword says when Zotero does not answer.
fn explain(error: Error) -> Error {
    match error {
        Error::Failed => Error::Other(
            "Zotero did not answer: it is not open on this computer, or it serves its API on \
             another port than 23119"
                .into(),
        ),
        Error::Status(403) => Error::Other(
            "Zotero's local API is off: tick \"Allow other applications on this computer to \
             communicate with Zotero\" in Zotero's Settings, under Advanced"
                .into(),
        ),
        Error::Status(404) => {
            Error::Other("this Zotero has no local API: it needs Zotero 7 or later".into())
        }
        error => error,
    }
}

/// Lowercase words of letters and digits without their accents, as
/// Zotero's own search compares them: "Gödel's Proof" is `godel`, `s`,
/// `proof`.
fn words(text: &str) -> Vec<String> {
    let plain: String = text.nfkd().filter(|c| !is_combining_mark(*c)).collect();
    plain
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

plumb_plugin::plugin!(search);

#[cfg(test)]
mod tests {
    use super::*;

    /// A sample answer, saved from Zotero 10's local API.
    fn sample(name: &str) -> Vec<Found> {
        let path = format!("{}/samples/{name}.json", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn titles(items: &[Item]) -> Vec<&str> {
        items.iter().map(|item| item.title.as_str()).collect()
    }

    #[test]
    fn a_search_lists_items_with_every_word_in_their_titles_authors_or_year() {
        let found = listed(sample("library"), &words("attention need"), true, SHOWN);
        assert_eq!(titles(&found), ["Attention Is All You Need"]);
        let paper = &found[0];
        assert_eq!(paper.url, "https://arxiv.org/abs/1706.03762");
        assert_eq!(paper.badge.as_deref(), Some("Article"));
        assert_eq!(
            paper.snippet.as_deref(),
            Some(
                "Vaswani et al., 2017, Advances in Neural Information Processing Systems. The \
                 dominant sequence transduction models are based on complex recurrent or \
                 convolutional neural networks in an encoder-decoder configuration."
            )
        );
        let found = listed(sample("library"), &words("vaswani 2017"), true, SHOWN);
        assert_eq!(titles(&found), ["Attention Is All You Need"]);
        // A word must start one of the item's: "rust" is not in "Trust".
        let found = listed(sample("library"), &words("rust"), true, SHOWN);
        assert_eq!(titles(&found), ["The Rust Programming Language"]);
        assert!(listed(sample("library"), &words("attention cats"), true, SHOWN).is_empty());
    }

    #[test]
    fn results_link_to_the_item_its_doi_or_its_book() {
        let all = listed(sample("library"), &[], false, 20);
        let links: Vec<(&str, &str)> = all
            .iter()
            .map(|item| (item.title.as_str(), item.url.as_str()))
            .collect();
        // Notes are left out, and so is a thesis with no address at all.
        assert_eq!(
            links,
            [
                (
                    "Brown v. Board of Education",
                    "https://en.wikipedia.org/wiki/Brown_v._Board_of_Education"
                ),
                (
                    "Attention Is All You Need",
                    "https://arxiv.org/abs/1706.03762"
                ),
                ("Dune", "https://openlibrary.org/isbn/9780441013593"),
                (
                    "The Rust Programming Language",
                    "https://doc.rust-lang.org/book/"
                ),
                (
                    "Deep Residual Learning for Image Recognition",
                    "https://doi.org/10.1109/CVPR.2016.90"
                ),
            ]
        );
        assert_eq!(all[0].badge.as_deref(), Some("Case"));
        assert_eq!(all[0].snippet.as_deref(), Some("1954"));
        assert_eq!(
            all[2].snippet.as_deref(),
            Some("Herbert, 1965, Chilton Books")
        );
        assert_eq!(all[3].snippet.as_deref(), Some("Rust documentation"));
    }

    #[test]
    fn a_match_in_a_note_lists_the_note_s_item() {
        // As if Zotero had answered the request for the note's item.
        let mut found = sample("child-match");
        found.extend(sample("parents"));
        let items = listed(
            with_parents(found).unwrap(),
            &words("heads"),
            false,
            SHOWN_ASKED,
        );
        assert_eq!(titles(&items), ["Attention Is All You Need"]);
    }

    /// An item as the local API would answer it, with only these fields.
    fn found(meta: serde_json::Value, data: serde_json::Value) -> Found {
        serde_json::from_value(serde_json::json!({ "meta": meta, "data": data })).unwrap()
    }

    #[test]
    fn words_match_without_accents_and_inside_unspaced_scripts() {
        let library = || {
            vec![
                found(
                    serde_json::json!({}),
                    serde_json::json!({"key": "A", "itemType": "book", "title": "Gödel, Escher, Bach", "url": "https://example.org/geb"}),
                ),
                found(
                    serde_json::json!({}),
                    serde_json::json!({"key": "B", "itemType": "journalArticle", "title": "基于注意力机制的神经网络", "url": "https://example.org/cn"}),
                ),
            ]
        };
        assert_eq!(
            titles(&listed(library(), &words("godel bach"), true, SHOWN)),
            ["Gödel, Escher, Bach"]
        );
        assert_eq!(
            titles(&listed(library(), &words("GÖDEL"), true, SHOWN)),
            ["Gödel, Escher, Bach"]
        );
        assert_eq!(
            titles(&listed(library(), &words("注意力机制"), true, SHOWN)),
            ["基于注意力机制的神经网络"]
        );
        // Latin words still match from their start only.
        assert!(listed(library(), &words("scher"), true, SHOWN).is_empty());
    }

    #[test]
    fn an_unreadable_date_has_no_year() {
        let undated = found(
            serde_json::json!({"creatorSummary": "Doe", "parsedDate": "0000"}),
            serde_json::json!({"key": "A", "itemType": "report", "title": "Notes", "url": "https://example.org/notes"}),
        );
        assert_eq!(item(&undated).unwrap().snippet.as_deref(), Some("Doe"));
    }

    #[test]
    fn files_and_links_show_on_their_own_only() {
        let library = vec![
            found(
                serde_json::json!({}),
                serde_json::json!({"key": "A", "itemType": "attachment", "title": "A saved page", "url": "https://example.org/page"}),
            ),
            found(
                serde_json::json!({}),
                serde_json::json!({"key": "B", "itemType": "attachment", "parentItem": "C", "title": "Full Text PDF", "url": "https://example.org/pdf"}),
            ),
        ];
        assert_eq!(
            titles(&listed(library, &[], false, SHOWN)),
            ["A saved page"]
        );
    }

    #[test]
    fn titles_with_the_words_come_first() {
        let found = listed(
            sample("library"),
            &words("dune herbert"),
            false,
            SHOWN_ASKED,
        );
        assert_eq!(found[0].title, "Dune");
    }

    #[test]
    fn isbns_and_dois_are_cleaned() {
        assert_eq!(isbn("978-0-441-01359-3").as_deref(), Some("9780441013593"));
        assert_eq!(
            isbn("0441013597 9780441013593").as_deref(),
            Some("0441013597")
        );
        assert_eq!(isbn("044101359X").as_deref(), Some("044101359X"));
        assert_eq!(isbn("not an isbn"), None);
        assert_eq!(
            isbn("978-0-441-01359-3, 0441013597").as_deref(),
            Some("9780441013593")
        );
        assert_eq!(
            isbn("ISBN 9780441013593 (pbk.)").as_deref(),
            Some("9780441013593")
        );
        assert_eq!(isbn("ISBN:0441013597").as_deref(), Some("0441013597"));
        let found: Found = serde_json::from_value(serde_json::json!({
            "data": {"key": "K", "itemType": "journalArticle", "title": "T", "DOI": "https://doi.org/10.1000/a b#c"}
        }))
        .unwrap();
        assert_eq!(
            link(&found).as_deref(),
            Some("https://doi.org/10.1000/a%20b%23c")
        );
    }

    #[test]
    fn a_synced_item_with_no_address_links_to_the_web_library() {
        let found: Found = serde_json::from_value(serde_json::json!({
            "links": {"alternate": {"href": "https://www.zotero.org/liz/items/ABCD1234", "type": "text/html"}},
            "data": {"key": "ABCD1234", "itemType": "thesis", "title": "A thesis"}
        }))
        .unwrap();
        assert_eq!(
            link(&found).as_deref(),
            Some("https://www.zotero.org/liz/items/ABCD1234")
        );
    }

    #[test]
    fn says_why_zotero_did_not_answer() {
        assert!(explain(Error::Failed).to_string().contains("not open"));
        assert!(explain(Error::Status(403))
            .to_string()
            .contains("Allow other applications"));
        assert_eq!(explain(Error::Status(500)), Error::Status(500));
    }

    #[test]
    fn a_search_of_no_words_asks_nothing() {
        let query = Query {
            text: "!!".into(),
            terms: "!!".into(),
            ..Query::default()
        };
        assert!(search(&query).unwrap().is_empty());
    }
}
