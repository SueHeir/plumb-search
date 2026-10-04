//! Single pages shown as results next to sites, starting with Wikipedia
//! articles: "marie curie" finds en.wikipedia.org/wiki/Marie_Curie.
//!
//! An article is kept small, about 100 bytes: its title, the names that
//! redirect to it, Wikipedia's one-line description, its Wikidata item,
//! the official website of that item when it has one, and how often it
//! was read. No article text is kept: Plumb finds pages by name.
//!
//! Articles travel as a tab-separated file, most read first, so the top
//! `N` are its first `N` lines:
//!
//! ```text
//! views  title  description  item  site  aliases     (tab-separated)
//! 81234  Marie Curie  Polish-French physicist ...  Q7186  (no site)  Maria Curie|Madame Curie
//! ```
//!
//! Aliases are separated by `|`, which no Wikipedia title contains.

use std::io::{BufRead, Write};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The articles file's first line.
pub const ARTICLES_HEADER: &str = "views\ttitle\tdescription\titem\tsite\taliases\n";

/// Most aliases (redirect names) kept per article, the most read first.
pub const MAX_ALIASES: usize = 5;

/// Longest description kept, in characters.
pub const MAX_ARTICLE_DESCRIPTION_CHARS: usize = 160;

/// The articles file of Wikipedia in `lang` (`en`): `wikipedia-en.tsv.gz`.
pub fn articles_file_name(lang: &str) -> String {
    format!("wikipedia-{lang}.tsv.gz")
}

/// One Wikipedia article.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Article {
    /// The title as shown, with spaces: `Python (programming language)`.
    pub title: String,
    /// Wikipedia's short description ("General-purpose programming
    /// language"), when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Wikidata item id (`Q28865`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    /// Registrable domain of the item's official website (`python.org`),
    /// so a result for that site can carry the article instead of both
    /// being listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    /// Page views over the days the file was made from.
    pub views: u64,
    /// Other titles that lead to this article, the most read first, at
    /// most [`MAX_ALIASES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

/// The address of the article `title` on Wikipedia in `lang`.
pub fn article_url(lang: &str, title: &str) -> String {
    let path: String = title
        .replace(' ', "_")
        .chars()
        .map(|c| match c {
            // Kept as they are in Wikipedia's own links.
            'A'..='Z'
            | 'a'..='z'
            | '0'..='9'
            | '_'
            | '-'
            | '.'
            | '('
            | ')'
            | ','
            | ':'
            | '!'
            | '*'
            | '\''
            | '~'
            | '/' => c.to_string(),
            c => {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf)
                    .bytes()
                    .map(|b| format!("%{b:02X}"))
                    .collect()
            }
        })
        .collect();
    format!("https://{lang}.wikipedia.org/wiki/{path}")
}

/// `text` with tabs, line breaks and `|` made spaces, so it fits a field.
fn field(text: &str) -> String {
    crate::collapse_whitespace(&text.replace(['\t', '\n', '\r', '|'], " "))
}

/// Writes `article` as one line of an articles file.
pub fn write_article(out: &mut impl Write, article: &Article) -> std::io::Result<()> {
    let aliases: Vec<String> = article.aliases.iter().map(|a| field(a)).collect();
    writeln!(
        out,
        "{}\t{}\t{}\t{}\t{}\t{}",
        article.views,
        field(&article.title),
        field(article.description.as_deref().unwrap_or("")),
        field(article.item.as_deref().unwrap_or("")),
        field(article.site.as_deref().unwrap_or("")),
        aliases.join("|"),
    )
}

/// Parses one line of an articles file (not the header).
pub fn parse_article(line: &str) -> Result<Article> {
    let line = line.trim_end_matches(['\n', '\r']);
    let cols: Vec<&str> = line.split('\t').collect();
    if cols.len() != 6 {
        bail!("expected 6 tab-separated fields, found {}", cols.len());
    }
    let views = cols[0]
        .parse()
        .with_context(|| format!("bad view count {:?}", cols[0]))?;
    let title = cols[1].trim();
    if title.is_empty() {
        bail!("no title");
    }
    let some = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
    Ok(Article {
        title: title.to_string(),
        description: some(cols[2]),
        item: some(cols[3]),
        site: some(cols[4]),
        views,
        aliases: cols[5]
            .split('|')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            .collect(),
    })
}

/// Reads up to `limit` articles of an articles file (the most read ones,
/// since the file is sorted), skipping its header.
pub fn read_articles(reader: impl BufRead, limit: usize) -> Result<Vec<Article>> {
    let mut articles = Vec::new();
    for (n, line) in reader.lines().enumerate() {
        if articles.len() >= limit {
            break;
        }
        let line = line.context("reading articles")?;
        if n == 0 && line.starts_with("views\t") {
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        articles.push(parse_article(&line).with_context(|| format!("articles line {}", n + 1))?);
    }
    Ok(articles)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_match_wikipedia_links() {
        assert_eq!(
            article_url("en", "Python (programming language)"),
            "https://en.wikipedia.org/wiki/Python_(programming_language)"
        );
        assert_eq!(
            article_url("en", "Nestlé"),
            "https://en.wikipedia.org/wiki/Nestl%C3%A9"
        );
        assert_eq!(
            article_url("en", "AT&T"),
            "https://en.wikipedia.org/wiki/AT%26T"
        );
    }

    #[test]
    fn lines_round_trip() {
        let article = Article {
            title: "Marie Curie".into(),
            description: Some("Polish-French physicist\tand chemist".into()),
            item: Some("Q7186".into()),
            site: None,
            views: 81234,
            aliases: vec!["Maria Curie".into(), "Madame Curie".into()],
        };
        let mut out = Vec::new();
        out.extend_from_slice(ARTICLES_HEADER.as_bytes());
        write_article(&mut out, &article).unwrap();
        let back = read_articles(&out[..], 10).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(
            back[0].description.as_deref(),
            Some("Polish-French physicist and chemist")
        );
        assert_eq!(back[0].aliases, article.aliases);
        assert_eq!(back[0].views, 81234);
        assert_eq!(back[0].site, None);
    }

    #[test]
    fn reads_only_the_top() {
        let text = format!("{ARTICLES_HEADER}9\tA\t\t\t\t\n5\tB\t\t\t\t\n1\tC\t\t\t\t\n");
        let top = read_articles(text.as_bytes(), 2).unwrap();
        assert_eq!(
            top.iter().map(|a| a.title.as_str()).collect::<Vec<_>>(),
            ["A", "B"]
        );
    }

    #[test]
    fn bad_lines_are_errors() {
        assert!(parse_article("x\tA\t\t\t\t").is_err());
        assert!(parse_article("1\tA").is_err());
        assert!(parse_article("1\t \t\t\t\t").is_err());
    }
}
