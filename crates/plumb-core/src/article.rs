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
//!
//! An article whose item has official profiles ([`crate::profiles`]) is
//! followed by a line of them, which files made before profiles never
//! have and readers made before them skip as a line they cannot read:
//!
//! ```text
//! profiles  item  service=id|service=id     (tab-separated)
//! profiles  Q19897578  youtube-handle=MrBeast|x=MrBeast
//! ```
//!
//! Such lines are not pages: the top `N` articles are the first `N` lines
//! that are not.
//!
//! The same line carries the item's official website when that is not
//! the front page of a site of its own: `website=https://music.youtube.com/`
//! for YouTube Music, whose site is youtube.com. Readers made before it
//! leave it out as a service they do not know.

use std::io::{BufRead, Write};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::profiles::{parse_profiles, write_profiles, Profile};

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
    /// The item's official profiles (a YouTube channel, an X account).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<Profile>,
    /// The item's official website when it is a subdomain or an inner
    /// page of [`Article::site`] rather than its front page:
    /// `https://music.youtube.com/` for YouTube Music.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
}

/// The key of an official website on a line of profiles.
pub const WEBSITE_KEY: &str = "website";

/// What starts a line of profiles in an articles file.
pub const PROFILES_LINE: &str = "profiles\t";

/// Whether `line` of an articles file is a line of profiles rather than
/// an article.
pub fn is_profiles_line(line: &[u8]) -> bool {
    line.starts_with(PROFILES_LINE.as_bytes())
}

/// The item, profiles and official website of a line of profiles, `None`
/// for another line.
pub fn parse_profiles_line(line: &str) -> Option<(&str, Vec<Profile>, Option<String>)> {
    let rest = line
        .trim_end_matches(['\n', '\r'])
        .strip_prefix(PROFILES_LINE)?;
    let (item, profiles) = rest.split_once('\t')?;
    let website = profiles.split('|').find_map(|pair| {
        let (key, url) = pair.split_once('=')?;
        let url = url.trim();
        (key.trim() == WEBSITE_KEY && is_web_address(url)).then(|| url.to_string())
    });
    Some((item.trim(), parse_profiles(profiles), website))
}

/// Whether `url` is an `http` or `https` address that fits a field.
fn is_web_address(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && !url.contains(['\t', '\n', '\r', '|', ' '])
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
    )?;
    let mut profiles = write_profiles(&article.profiles);
    if let Some(website) = article.website.as_deref().filter(|url| is_web_address(url)) {
        if !profiles.is_empty() {
            profiles.push('|');
        }
        profiles.push_str(WEBSITE_KEY);
        profiles.push('=');
        profiles.push_str(website);
    }
    if !profiles.is_empty() {
        write!(out, "{PROFILES_LINE}")?;
        writeln!(
            out,
            "{}\t{profiles}",
            field(article.item.as_deref().unwrap_or(""))
        )?;
    }
    Ok(())
}

/// The articles of an articles file's lines, each with the profiles on
/// the line after it, and the line it was on (counting from 1). The
/// header and blank lines are skipped; a line that does not read is an
/// error, after which reading goes on.
pub fn articles_of<I>(lines: I) -> ArticleLines<I>
where
    I: Iterator<Item = String>,
{
    ArticleLines {
        lines,
        number: 0,
        pending: None,
        error: None,
    }
}

/// See [`articles_of`].
pub struct ArticleLines<I> {
    lines: I,
    number: usize,
    /// The last article read, waiting for a line of profiles after it.
    pending: Option<(usize, Article)>,
    /// A line that did not read, to hand back next.
    error: Option<(usize, anyhow::Error)>,
}

impl<I: Iterator<Item = String>> Iterator for ArticleLines<I> {
    type Item = (usize, Result<Article>);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some((n, err)) = self.error.take() {
            return Some((n, Err(err)));
        }
        loop {
            let Some(line) = self.lines.next() else {
                return self.pending.take().map(|(n, a)| (n, Ok(a)));
            };
            self.number += 1;
            if (self.number == 1 && line.starts_with("views\t")) || line.trim().is_empty() {
                continue;
            }
            if let Some((item, profiles, website)) = parse_profiles_line(&line) {
                if let Some((_, article)) = &mut self.pending {
                    if article.item.as_deref() == Some(item) {
                        article.profiles = profiles;
                        article.website = website;
                    }
                }
                continue;
            }
            let number = self.number;
            match parse_article(&line) {
                Ok(article) => {
                    if let Some((n, done)) = self.pending.replace((number, article)) {
                        return Some((n, Ok(done)));
                    }
                }
                // A line that does not read ends the article before it,
                // which goes first, so the order stays.
                Err(err) => {
                    return Some(match self.pending.take() {
                        Some((n, done)) => {
                            self.error = Some((number, err));
                            (n, Ok(done))
                        }
                        None => (number, Err(err)),
                    })
                }
            }
        }
    }
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
        profiles: Vec::new(),
        website: None,
    })
}

/// Reads up to `limit` articles of an articles file (the most read ones,
/// since the file is sorted), skipping its header.
pub fn read_articles(reader: impl BufRead, limit: usize) -> Result<Vec<Article>> {
    let mut articles = Vec::new();
    let mut failed = None;
    let lines = reader.lines().map_while(|line| match line {
        Ok(line) => Some(line),
        Err(err) => {
            failed = Some(err);
            None
        }
    });
    for (n, article) in articles_of(lines) {
        if articles.len() >= limit {
            break;
        }
        articles.push(article.with_context(|| format!("articles line {n}"))?);
    }
    if let Some(err) = failed {
        return Err(anyhow::Error::new(err).context("reading articles"));
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
            profiles: Vec::new(),
            website: None,
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
    fn profiles_follow_their_article() {
        use crate::profiles::Profile;
        let beast = Article {
            title: "MrBeast".into(),
            item: Some("Q19897578".into()),
            views: 900,
            profiles: vec![
                Profile {
                    service: "youtube-handle".into(),
                    id: "MrBeast".into(),
                },
                Profile {
                    service: "x".into(),
                    id: "MrBeast".into(),
                },
            ],
            ..Article::default()
        };
        let plain = Article {
            title: "Plain".into(),
            item: Some("Q1".into()),
            views: 5,
            ..Article::default()
        };
        let mut out = Vec::new();
        out.extend_from_slice(ARTICLES_HEADER.as_bytes());
        write_article(&mut out, &beast).unwrap();
        write_article(&mut out, &plain).unwrap();
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.contains("\nprofiles\tQ19897578\tyoutube-handle=MrBeast|x=MrBeast\n"));
        assert_eq!(read_articles(&out[..], 10).unwrap(), [beast.clone(), plain]);
        assert_eq!(read_articles(&out[..], 1).unwrap(), [beast]);
        // A reader made before profiles finds no article in their line.
        assert!(parse_article("profiles\tQ1\tx=a").is_err());
    }

    #[test]
    fn websites_ride_on_the_line_of_profiles() {
        let music = Article {
            title: "YouTube Music".into(),
            item: Some("Q28404534".into()),
            site: Some("youtube.com".into()),
            views: 700,
            website: Some("https://music.youtube.com/".into()),
            ..Article::default()
        };
        let mut out = Vec::new();
        out.extend_from_slice(ARTICLES_HEADER.as_bytes());
        write_article(&mut out, &music).unwrap();
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.contains("\nprofiles\tQ28404534\twebsite=https://music.youtube.com/\n"));
        assert_eq!(read_articles(&out[..], 10).unwrap(), [music]);
        // Readers made before websites see no profile in it.
        assert!(parse_profiles("website=https://music.youtube.com/").is_empty());
    }

    #[test]
    fn a_bad_line_keeps_the_order() {
        let text = format!("{ARTICLES_HEADER}9\tA\t\t\t\t\nbad\n5\tB\t\t\t\t\n");
        let read: Vec<(usize, bool)> = articles_of(text.lines().map(str::to_string))
            .map(|(n, a)| (n, a.is_ok()))
            .collect();
        assert_eq!(read, [(2, true), (3, false), (4, true)]);
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
