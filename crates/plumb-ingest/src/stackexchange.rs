//! The Stack Overflow page set: the most viewed questions, listed next to
//! the sites when a search names one ("undo last git commit" can find
//! "How do I undo the most recent local commits in Git?").
//!
//! Questions come from Stack Exchange's public data dump (CC BY-SA 4.0),
//! the `Posts.xml` of a site inside its `.7z` archive, and are written as
//! an articles file ([`plumb_core::article`]): the title is the question's
//! title, the views its view count, the description its tags
//! ("git, git-commit"), and the item its question id, from which the
//! address is made (`https://stackoverflow.com/questions/ID`). No question
//! or answer text is kept. The titles of up to [`MAX_DUPLICATE_TITLES`]
//! questions closed as its duplicates (from the dump's `PostLinks.xml`)
//! are kept as its aliases: the same question in other words ("git undo
//! commit" is asked as "How to revert the last commit in Git?" too).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::article::{Article, MAX_ARTICLE_DESCRIPTION_CHARS};
use tracing::{info, warn};

/// Where Stack Overflow's posts are in Stack Exchange's dump on the
/// Internet Archive.
pub const STACKOVERFLOW_POSTS_URL: &str =
    "https://archive.org/download/stackexchange/stackoverflow.com-Posts.7z";

/// Where Stack Overflow's links between posts (duplicates among them) are
/// in the same dump.
pub const STACKOVERFLOW_POST_LINKS_URL: &str =
    "https://archive.org/download/stackexchange/stackoverflow.com-PostLinks.7z";

/// Most titles of a question's duplicates kept as its aliases, the most
/// viewed.
pub const MAX_DUPLICATE_TITLES: usize = 3;

/// `PostLinks.xml`'s link type of a question closed as a duplicate of
/// another.
const DUPLICATE_LINK: &str = "3";

/// Questions closed as duplicates: the duplicate's id and the id of the
/// question it repeats.
pub type Duplicates = HashMap<u64, u64>;

/// One question of a dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub id: u64,
    pub title: String,
    pub views: u64,
    pub score: i64,
    pub tags: Vec<String>,
    /// Titles of questions closed as its duplicates, most viewed first.
    pub aliases: Vec<String>,
}

impl Question {
    /// The question as an articles file line (see the module docs).
    pub fn into_article(self) -> Article {
        let tags = self.tags.join(", ");
        Article {
            title: self.title,
            description: (!tags.is_empty())
                .then(|| plumb_core::truncate_chars(&tags, MAX_ARTICLE_DESCRIPTION_CHARS)),
            item: Some(self.id.to_string()),
            site: None,
            views: self.views,
            aliases: self.aliases,
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: Vec::new(),
        }
    }
}

impl Question {
    /// The question of Stack Exchange site `site` as a line of the
    /// `stackexchange` set's file: like [`Question::into_article`], with
    /// the site in its item (`diy.stackexchange.com/12345`).
    pub fn into_exchange_article(self, site: &plumb_core::stack_exchange::ExchangeSite) -> Article {
        let item = plumb_core::stack_exchange::question_item(site, self.id);
        Article {
            item: Some(item),
            ..self.into_article()
        }
    }
}

/// The value of attribute `name` in the XML element `row`, entities
/// decoded.
fn attribute(row: &str, name: &str) -> Option<String> {
    let key = format!(" {name}=\"");
    let start = row.find(&key)? + key.len();
    let end = start + row[start..].find('"')?;
    Some(decode_entities(&row[start..end]))
}

/// `text` with XML's entities (`&amp;`, `&#39;`, `&#x2F;`) decoded.
pub(crate) fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|&end| end <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The tags of a post: `|git|git-commit|` (dumps since 2024) or
/// `<git><git-commit>` (older ones).
fn parse_tags(tags: &str) -> Vec<String> {
    tags.split(['|', '<', '>'])
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Reads one line of `Posts.xml`: a question (`PostTypeId="1"`) with a
/// title, else `None`.
pub fn parse_question(line: &str) -> Option<Question> {
    let row = line.trim_start();
    if !row.starts_with("<row ") || !row.contains(" PostTypeId=\"1\"") {
        return None;
    }
    let title = plumb_core::collapse_whitespace(&attribute(row, "Title")?);
    if title.is_empty() {
        return None;
    }
    Some(Question {
        id: attribute(row, "Id")?.parse().ok()?,
        title,
        views: attribute(row, "ViewCount")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        score: attribute(row, "Score")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        tags: attribute(row, "Tags")
            .map(|t| parse_tags(&t))
            .unwrap_or_default(),
        aliases: Vec::new(),
    })
}

/// Reads one line of `PostLinks.xml`: a question closed as a duplicate
/// (`LinkTypeId="3"`) and the one it repeats, else `None`.
pub fn parse_duplicate(line: &str) -> Option<(u64, u64)> {
    let row = line.trim_start();
    if !row.starts_with("<row ") {
        return None;
    }
    if attribute(row, "LinkTypeId")? != DUPLICATE_LINK {
        return None;
    }
    let duplicate = attribute(row, "PostId")?.parse().ok()?;
    let original = attribute(row, "RelatedPostId")?.parse().ok()?;
    (duplicate != original).then_some((duplicate, original))
}

/// The questions closed as duplicates in `PostLinks.xml` (`reader`).
pub fn read_duplicates(reader: impl BufRead) -> Result<Duplicates> {
    let mut duplicates = Duplicates::new();
    let mut line = Vec::new();
    let mut reader = reader;
    loop {
        line.clear();
        if reader
            .read_until(b'\n', &mut line)
            .context("reading the post links")?
            == 0
        {
            break;
        }
        if let Some((duplicate, original)) = parse_duplicate(&String::from_utf8_lossy(&line)) {
            duplicates.insert(duplicate, original);
        }
    }
    info!("{} questions closed as duplicates", duplicates.len());
    Ok(duplicates)
}

/// The `keep` most viewed questions with at least `min_score` in
/// `Posts.xml` (`reader`), most viewed first, each with the titles of its
/// `duplicates` as aliases.
pub fn read_questions(
    reader: impl BufRead,
    min_score: i64,
    keep: usize,
    duplicates: &Duplicates,
) -> Result<Vec<Question>> {
    // The least viewed of those kept so far is on top.
    let mut kept: BinaryHeap<Reverse<(u64, u64)>> = BinaryHeap::new();
    let mut questions: HashMap<u64, Question> = Default::default();
    // The views and titles of each question's duplicates.
    let mut repeats: HashMap<u64, Vec<(u64, String)>> = Default::default();
    let mut seen = 0u64;
    let mut line = Vec::new();
    let mut reader = reader;
    loop {
        line.clear();
        if reader
            .read_until(b'\n', &mut line)
            .context("reading the posts")?
            == 0
        {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        let Some(question) = parse_question(&text) else {
            continue;
        };
        seen += 1;
        if seen.is_multiple_of(1_000_000) {
            info!("{seen} questions read");
        }
        if let Some(&original) = duplicates.get(&question.id) {
            repeats
                .entry(original)
                .or_default()
                .push((question.views, question.title.clone()));
        }
        if question.score < min_score {
            continue;
        }
        if kept.len() == keep {
            match kept.peek() {
                Some(Reverse((views, _))) if *views < question.views => {
                    let Reverse((_, id)) = kept.pop().expect("peeked");
                    questions.remove(&id);
                }
                _ => continue,
            }
        }
        kept.push(Reverse((question.views, question.id)));
        questions.insert(question.id, question);
    }
    let mut out: Vec<Question> = questions.into_values().collect();
    out.sort_by(|a, b| b.views.cmp(&a.views).then_with(|| a.id.cmp(&b.id)));
    let mut named = 0usize;
    for question in &mut out {
        if let Some(titles) = repeats.remove(&question.id) {
            question.aliases = duplicate_titles(&question.title, titles);
            named += usize::from(!question.aliases.is_empty());
        }
    }
    info!(
        "{seen} questions in all, {} kept, {named} with their duplicates' titles",
        out.len()
    );
    Ok(out)
}

/// Of the titles of a question's duplicates (with their views), the
/// [`MAX_DUPLICATE_TITLES`] most viewed that say it differently from its
/// own `title` and from each other.
fn duplicate_titles(title: &str, mut titles: Vec<(u64, String)>) -> Vec<String> {
    titles.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut said = vec![plumb_core::normalize_text(title)];
    let mut out = Vec::new();
    for (_, alias) in titles {
        let key = plumb_core::normalize_text(&alias);
        if key.is_empty() || said.contains(&key) {
            continue;
        }
        said.push(key);
        out.push(alias);
        if out.len() == MAX_DUPLICATE_TITLES {
            break;
        }
    }
    out
}

/// Calls `read` with the first entry named `name` (`Posts.xml`) of the 7z
/// archive `path`; `None` when it has none.
fn read_entry<T>(
    path: &Path,
    name: &str,
    mut read: impl FnMut(&mut dyn BufRead) -> Result<T>,
) -> Result<Option<T>> {
    let mut archive = sevenz_rust2::ArchiveReader::open(path, sevenz_rust2::Password::empty())
        .with_context(|| format!("opening {}", path.display()))?;
    let mut found: Option<Result<T>> = None;
    archive
        .for_each_entries(|entry, reader: &mut dyn Read| {
            let entry_name = entry.name().rsplit(['/', '\\']).next().unwrap_or("");
            if found.is_some() || !entry_name.eq_ignore_ascii_case(name) {
                // Entries of a solid archive must still be read through.
                std::io::copy(reader, &mut std::io::sink())?;
                return Ok(true);
            }
            found = Some(read(&mut BufReader::with_capacity(1 << 20, reader)));
            Ok(true)
        })
        .with_context(|| format!("unpacking {}", path.display()))?;
    found.transpose()
}

/// The questions closed as duplicates in the `PostLinks.xml` of the 7z
/// archive `path` (`stackoverflow.com-PostLinks.7z`, or a whole site's
/// `superuser.com.7z`); none when it has no such file.
pub fn read_duplicates_7z(path: &Path) -> Result<Duplicates> {
    Ok(read_entry(path, "PostLinks.xml", |reader| read_duplicates(reader))?.unwrap_or_default())
}

/// [`read_questions`] of the `Posts.xml` in the 7z archive `path`
/// (`stackoverflow.com-Posts.7z`, or a whole site's `superuser.com.7z`),
/// with the duplicates in `links` (another archive's `PostLinks.xml`), or
/// without `links` in the same archive's. A site's archive lists
/// `PostLinks.xml` before `Posts.xml`, so it is read once.
pub fn read_questions_7z(
    path: &Path,
    min_score: i64,
    keep: usize,
    links: Option<&Path>,
) -> Result<Vec<Question>> {
    let no_duplicates = |err: anyhow::Error| {
        warn!("reading the duplicates: {err:#}; the questions keep no other titles");
        Duplicates::new()
    };
    let mut duplicates = links.map(|links| read_duplicates_7z(links).unwrap_or_else(no_duplicates));
    let mut archive = sevenz_rust2::ArchiveReader::open(path, sevenz_rust2::Password::empty())
        .with_context(|| format!("opening {}", path.display()))?;
    let mut found: Option<Result<Vec<Question>>> = None;
    let mut read_without_links = false;
    archive
        .for_each_entries(|entry, reader: &mut dyn Read| {
            let name = entry.name().rsplit(['/', '\\']).next().unwrap_or("");
            let reader = &mut BufReader::with_capacity(1 << 20, reader);
            if duplicates.is_none() && name.eq_ignore_ascii_case("PostLinks.xml") {
                duplicates = Some(read_duplicates(reader).unwrap_or_else(no_duplicates));
            } else if found.is_none() && name.eq_ignore_ascii_case("Posts.xml") {
                read_without_links = duplicates.is_none();
                let none = Duplicates::new();
                found = Some(read_questions(
                    reader,
                    min_score,
                    keep,
                    duplicates.as_ref().unwrap_or(&none),
                ));
            } else {
                // Entries of a solid archive must still be read through.
                std::io::copy(reader, &mut std::io::sink())?;
            }
            Ok(true)
        })
        .with_context(|| format!("unpacking {}", path.display()))?;
    match found {
        // The links came after the posts: read the posts again with them.
        Some(Ok(_)) if read_without_links && duplicates.as_ref().is_some_and(|d| !d.is_empty()) => {
            let duplicates = duplicates.unwrap_or_default();
            read_entry(path, "Posts.xml", |reader| {
                read_questions(reader, min_score, keep, &duplicates)
            })?
            .context("Posts.xml went missing")
        }
        Some(questions) => questions,
        None => bail!("{} has no Posts.xml", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POSTS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<posts>
  <row Id="927358" PostTypeId="1" AcceptedAnswerId="927386" Score="27000" ViewCount="14000000" Title="How do I undo the most recent local commits in Git?" Tags="|git|version-control|git-commit|undo|" AnswerCount="100" />
  <row Id="927386" PostTypeId="2" ParentId="927358" Score="30000" Body="&lt;p&gt;Use reset&lt;/p&gt;" />
  <row Id="11227809" PostTypeId="1" Score="27000" ViewCount="1900000" Title="Why is processing a sorted array faster than processing an unsorted array?" Tags="&lt;java&gt;&lt;c++&gt;&lt;performance&gt;" />
  <row Id="5" PostTypeId="1" Score="-3" ViewCount="99" Title="Why doesn&#39;t this &amp; that work?" Tags="|c|" />
  <row Id="6" PostTypeId="1" Score="2" ViewCount="50" Title="Low" Tags="|c|" />
</posts>
"#;

    #[test]
    fn reads_questions_most_viewed_first() {
        let all = read_questions(POSTS.as_bytes(), i64::MIN, 10, &Duplicates::new()).unwrap();
        let ids: Vec<u64> = all.iter().map(|q| q.id).collect();
        assert_eq!(ids, [927358, 11227809, 5, 6]);
        assert_eq!(all[2].title, "Why doesn't this & that work?");
        assert_eq!(all[1].tags, ["java", "c++", "performance"]);
        // Only the best viewed, and only questions with a score.
        let top = read_questions(POSTS.as_bytes(), 0, 2, &Duplicates::new()).unwrap();
        assert_eq!(
            top.iter().map(|q| q.id).collect::<Vec<_>>(),
            [927358, 11227809]
        );
        let scored = read_questions(POSTS.as_bytes(), 0, 10, &Duplicates::new()).unwrap();
        assert_eq!(scored.len(), 3);
    }

    #[test]
    fn questions_become_articles() {
        let question = read_questions(POSTS.as_bytes(), 0, 1, &Duplicates::new())
            .unwrap()
            .remove(0);
        let article = question.into_article();
        assert_eq!(article.item.as_deref(), Some("927358"));
        assert_eq!(article.views, 14_000_000);
        assert_eq!(
            article.description.as_deref(),
            Some("git, version-control, git-commit, undo")
        );
    }

    #[test]
    fn other_sites_questions_name_their_site() {
        let questions = read_questions(POSTS.as_bytes(), 1, 10, &Duplicates::new()).unwrap();
        let diy = plumb_core::stack_exchange::site_of("diy.stackexchange.com").unwrap();
        let article = questions[0].clone().into_exchange_article(diy);
        assert_eq!(
            article.item.as_deref(),
            Some("diy.stackexchange.com/927358")
        );
        assert_eq!(
            article.title,
            "How do I undo the most recent local commits in Git?"
        );
    }

    const LINKS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<postlinks>
  <row Id="1" CreationDate="2010-01-01T00:00:00.000" PostId="5" RelatedPostId="927358" LinkTypeId="3" />
  <row Id="2" CreationDate="2010-01-01T00:00:00.000" PostId="6" RelatedPostId="927358" LinkTypeId="3" />
  <row Id="3" CreationDate="2010-01-01T00:00:00.000" PostId="7" RelatedPostId="927358" LinkTypeId="3" />
  <row Id="4" CreationDate="2010-01-01T00:00:00.000" PostId="11227809" RelatedPostId="927358" LinkTypeId="1" />
</postlinks>
"#;

    #[test]
    fn duplicates_titles_become_aliases() {
        let duplicates = read_duplicates(LINKS.as_bytes()).unwrap();
        // Only links of the duplicate kind.
        assert_eq!(duplicates.len(), 3);
        assert_eq!(duplicates.get(&5), Some(&927358));
        let posts = POSTS.replace(
            "</posts>",
            r#"  <row Id="7" PostTypeId="1" Score="1" ViewCount="10" Title="how do I undo the most recent local commits in git" Tags="|git|" />
</posts>"#,
        );
        let questions = read_questions(posts.as_bytes(), 1, 2, &duplicates).unwrap();
        let undo = &questions[0];
        assert_eq!(undo.id, 927358);
        // Duplicates are named even when not kept themselves, the most
        // viewed first; one saying the same as the title is not.
        assert_eq!(undo.aliases, ["Why doesn't this & that work?", "Low"]);
        assert_eq!(
            undo.clone().into_article().aliases,
            ["Why doesn't this & that work?", "Low"]
        );
        assert!(questions[1].aliases.is_empty());
    }

    #[test]
    fn entities_decode() {
        assert_eq!(
            decode_entities("a &amp;&#x2F;&#47; b &bogus c"),
            "a &// b &bogus c"
        );
    }
}
