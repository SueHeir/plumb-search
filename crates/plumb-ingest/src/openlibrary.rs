//! The books page set: Open Library's works that readers shelve or rate
//! most, listed next to the sites ("dune frank herbert" finds the book).
//!
//! Books come from Open Library's monthly data dumps (CC0): how often each
//! work is on a reading log or rated gives its popularity, the works dump
//! its title and authors, and the authors dump the authors' names. They are
//! written as an articles file ([`plumb_core::article`]): the title is the
//! work's title, the description "Book by AUTHOR, YEAR", the item its work
//! id (`OL45804W`, from which the address is made), the views its reading
//! log entries and ratings, and the one alias "TITLE AUTHOR".

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};
use plumb_core::article::{Article, MAX_ARTICLE_DESCRIPTION_CHARS};
use serde::Deserialize;
use tracing::info;

/// Open Library's latest dumps, each a gzipped TSV.
pub const WORKS_URL: &str = "https://openlibrary.org/data/ol_dump_works_latest.txt.gz";
pub const AUTHORS_URL: &str = "https://openlibrary.org/data/ol_dump_authors_latest.txt.gz";
pub const READING_LOG_URL: &str = "https://openlibrary.org/data/ol_dump_reading-log_latest.txt.gz";
pub const RATINGS_URL: &str = "https://openlibrary.org/data/ol_dump_ratings_latest.txt.gz";

/// The number of a work key: `/works/OL45804W` -> 45804.
fn work_number(key: &str) -> Option<u32> {
    key.trim()
        .strip_prefix("/works/OL")?
        .strip_suffix('W')?
        .parse()
        .ok()
}

/// The number of an author key: `/authors/OL34184A` -> 34184.
fn author_number(key: &str) -> Option<u32> {
    key.trim()
        .strip_prefix("/authors/OL")?
        .strip_suffix('A')?
        .parse()
        .ok()
}

fn open_gz(path: &Path) -> Result<impl BufRead> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    Ok(BufReader::with_capacity(
        1 << 20,
        flate2::read::MultiGzDecoder::new(BufReader::new(file)),
    ))
}

/// Calls `each` with every line of `reader`, lossily decoded.
fn for_each_line(mut reader: impl BufRead, mut each: impl FnMut(&str)) -> Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader
            .read_until(b'\n', &mut line)
            .context("reading a dump")?
            == 0
        {
            return Ok(());
        }
        each(String::from_utf8_lossy(&line).trim_end());
    }
}

/// Adds one to `counts` for each line of a reading log or ratings dump
/// (`/works/OL45804W \t /books/OL... \t ...`).
pub fn count_shelvings(reader: impl BufRead, counts: &mut HashMap<u32, u32>) -> Result<()> {
    for_each_line(reader, |line| {
        if let Some(work) = line.split('\t').next().and_then(work_number) {
            *counts.entry(work).or_default() += 1;
        }
    })
}

#[derive(Debug, Deserialize)]
struct WorkJson {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    authors: Vec<AuthorRole>,
    #[serde(default)]
    first_publish_date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthorRole {
    #[serde(default)]
    author: Option<KeyRef>,
}

#[derive(Debug, Deserialize)]
struct KeyRef {
    key: String,
}

#[derive(Debug, Deserialize)]
struct AuthorJson {
    #[serde(default)]
    name: Option<String>,
}

/// One work kept from the works dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Book {
    pub work: u32,
    pub title: String,
    pub author: Option<u32>,
    pub year: Option<u16>,
    pub shelvings: u32,
}

/// The first four-digit year in `date` ("June 1965" -> 1965).
fn year_of(date: &str) -> Option<u16> {
    date.as_bytes()
        .windows(4)
        .enumerate()
        .find(|(i, w)| {
            w.iter().all(u8::is_ascii_digit)
                && !date.as_bytes()[i + 4..]
                    .first()
                    .is_some_and(u8::is_ascii_digit)
                && (*i == 0 || !date.as_bytes()[i - 1].is_ascii_digit())
        })
        .and_then(|(_, w)| std::str::from_utf8(w).ok()?.parse().ok())
}

/// The works of the works dump (`type \t key \t revision \t modified \t
/// json`) that `counts` has, with their counts.
pub fn read_works(reader: impl BufRead, counts: &HashMap<u32, u32>) -> Result<Vec<Book>> {
    let mut books = Vec::new();
    for_each_line(reader, |line| {
        let mut columns = line.splitn(5, '\t');
        let (Some(kind), Some(key), _, _, Some(json)) = (
            columns.next(),
            columns.next(),
            columns.next(),
            columns.next(),
            columns.next(),
        ) else {
            return;
        };
        if kind != "/type/work" {
            return;
        }
        let Some(work) = work_number(key) else {
            return;
        };
        let Some(&shelvings) = counts.get(&work) else {
            return;
        };
        let Ok(parsed) = serde_json::from_str::<WorkJson>(json) else {
            return;
        };
        let title = plumb_core::collapse_whitespace(parsed.title.as_deref().unwrap_or(""));
        if title.is_empty() {
            return;
        }
        books.push(Book {
            work,
            title,
            author: parsed
                .authors
                .iter()
                .find_map(|role| author_number(&role.author.as_ref()?.key)),
            year: parsed.first_publish_date.as_deref().and_then(year_of),
            shelvings,
        });
    })?;
    Ok(books)
}

/// Names of the authors in `wanted`, from the authors dump.
pub fn read_author_names(
    reader: impl BufRead,
    wanted: &HashSet<u32>,
) -> Result<HashMap<u32, String>> {
    let mut names = HashMap::new();
    for_each_line(reader, |line| {
        let mut columns = line.splitn(5, '\t');
        let (_, Some(key), _, _, Some(json)) = (
            columns.next(),
            columns.next(),
            columns.next(),
            columns.next(),
            columns.next(),
        ) else {
            return;
        };
        let Some(author) = author_number(key).filter(|a| wanted.contains(a)) else {
            return;
        };
        if let Ok(AuthorJson { name: Some(name) }) = serde_json::from_str(json) {
            let name = plumb_core::collapse_whitespace(&name);
            if !name.is_empty() {
                names.insert(author, name);
            }
        }
    })?;
    Ok(names)
}

impl Book {
    /// The book as an articles file line (see the module docs).
    pub fn into_article(self, author: Option<&str>) -> Article {
        let description = match (author, self.year) {
            (Some(author), Some(year)) => format!("Book by {author}, {year}"),
            (Some(author), None) => format!("Book by {author}"),
            (None, Some(year)) => format!("Book, {year}"),
            (None, None) => "Book".to_string(),
        };
        let aliases = author
            .map(|author| vec![format!("{} {author}", self.title)])
            .unwrap_or_default();
        Article {
            description: Some(plumb_core::truncate_chars(
                &description,
                MAX_ARTICLE_DESCRIPTION_CHARS,
            )),
            item: Some(format!("OL{}W", self.work)),
            site: None,
            views: self.shelvings.into(),
            aliases,
            title: self.title,
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            search: None,
        }
    }
}

/// The dumps a books file is made from.
pub struct BookDumps<'a> {
    pub works: &'a Path,
    pub authors: &'a Path,
    /// Reading log and ratings dumps.
    pub shelvings: &'a [&'a Path],
}

/// The `keep` most shelved books with at least `min_shelvings`, most
/// shelved first.
pub fn build_books(dumps: &BookDumps, min_shelvings: u32, keep: usize) -> Result<Vec<Article>> {
    let mut counts = HashMap::new();
    for path in dumps.shelvings {
        info!("counting shelvings in {}", path.display());
        count_shelvings(open_gz(path)?, &mut counts)?;
    }
    counts.retain(|_, n| *n >= min_shelvings);
    info!(
        "{} works shelved at least {min_shelvings} times",
        counts.len()
    );
    info!("reading works from {}", dumps.works.display());
    let mut books = read_works(open_gz(dumps.works)?, &counts)?;
    drop(counts);
    books.sort_by(|a, b| b.shelvings.cmp(&a.shelvings).then(a.work.cmp(&b.work)));
    books.truncate(keep);
    let wanted: HashSet<u32> = books.iter().filter_map(|b| b.author).collect();
    info!(
        "reading {} authors' names from {}",
        wanted.len(),
        dumps.authors.display()
    );
    let names = read_author_names(open_gz(dumps.authors)?, &wanted)?;
    Ok(books
        .into_iter()
        .map(|book| {
            let author = book.author.and_then(|a| names.get(&a)).cloned();
            book.into_article(author.as_deref())
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKS: &str = "/type/work\t/works/OL893415W\t9\t2024-01-01T00:00:00\t{\"title\": \"Dune\", \"authors\": [{\"type\": {\"key\": \"/type/author_role\"}, \"author\": {\"key\": \"/authors/OL79034A\"}}], \"first_publish_date\": \"1965\", \"key\": \"/works/OL893415W\"}
/type/work\t/works/OL1W\t1\t2024-01-01T00:00:00\t{\"title\": \"Unread\"}
/type/redirect\t/works/OL2W\t1\t2024-01-01T00:00:00\t{\"location\": \"/works/OL893415W\"}
/type/work\t/works/OL3W\t1\t2024-01-01T00:00:00\t{\"title\": \"  Nobody's   Book \", \"first_publish_date\": \"June 12, 2001\"}
";
    const AUTHORS: &str =
        "/type/author\t/authors/OL79034A\t3\t2024-01-01T00:00:00\t{\"name\": \"Frank Herbert\"}
/type/author\t/authors/OL5A\t3\t2024-01-01T00:00:00\t{\"name\": \"Someone Else\"}
";
    const LOG: &str = "/works/OL893415W\t/books/OL1M\tAlready Read\t2023-01-01
/works/OL893415W\t\tWant to Read\t2023-01-02
/works/OL3W\t/books/OL2M\tWant to Read\t2023-01-02
/works/OL2W\t/books/OL2M\tWant to Read\t2023-01-02
";

    #[test]
    fn books_are_read_from_the_dumps() {
        let mut counts = HashMap::new();
        count_shelvings(LOG.as_bytes(), &mut counts).unwrap();
        assert_eq!(counts[&893415], 2);
        let books = read_works(WORKS.as_bytes(), &counts).unwrap();
        assert_eq!(books.len(), 2);
        assert_eq!(books[0].title, "Dune");
        assert_eq!(books[0].author, Some(79034));
        assert_eq!(books[0].year, Some(1965));
        assert_eq!(books[1].title, "Nobody's Book");
        assert_eq!(books[1].year, Some(2001));
        let names = read_author_names(AUTHORS.as_bytes(), &HashSet::from([79034])).unwrap();
        assert_eq!(names.len(), 1);
        let article = books[0].clone().into_article(Some(&names[&79034]));
        assert_eq!(article.item.as_deref(), Some("OL893415W"));
        assert_eq!(
            article.description.as_deref(),
            Some("Book by Frank Herbert, 1965")
        );
        assert_eq!(article.aliases, ["Dune Frank Herbert"]);
        assert_eq!(article.views, 2);
        let anonymous = books[1].clone().into_article(None);
        assert_eq!(anonymous.description.as_deref(), Some("Book, 2001"));
        assert!(anonymous.aliases.is_empty());
    }

    #[test]
    fn years_are_found() {
        assert_eq!(year_of("1965"), Some(1965));
        assert_eq!(year_of("June 12, 2001"), Some(2001));
        assert_eq!(year_of("12345"), None);
        assert_eq!(year_of("unknown"), None);
    }
}
