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
//! or answer text is kept.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::article::{Article, MAX_ARTICLE_DESCRIPTION_CHARS};
use tracing::info;

/// Where Stack Overflow's posts are in Stack Exchange's dump on the
/// Internet Archive.
pub const STACKOVERFLOW_POSTS_URL: &str =
    "https://archive.org/download/stackexchange/stackoverflow.com-Posts.7z";

/// One question of a dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub id: u64,
    pub title: String,
    pub views: u64,
    pub score: i64,
    pub tags: Vec<String>,
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
            aliases: Vec::new(),
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
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
fn decode_entities(text: &str) -> String {
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
    })
}

/// The `keep` most viewed questions with at least `min_score` in
/// `Posts.xml` (`reader`), most viewed first.
pub fn read_questions(reader: impl BufRead, min_score: i64, keep: usize) -> Result<Vec<Question>> {
    // The least viewed of those kept so far is on top.
    let mut kept: BinaryHeap<Reverse<(u64, u64)>> = BinaryHeap::new();
    let mut questions: std::collections::HashMap<u64, Question> = Default::default();
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
    info!("{seen} questions in all, {} kept", out.len());
    Ok(out)
}

/// [`read_questions`] of the `Posts.xml` in the 7z archive `path`
/// (`stackoverflow.com-Posts.7z`, or a whole site's `superuser.com.7z`).
pub fn read_questions_7z(path: &Path, min_score: i64, keep: usize) -> Result<Vec<Question>> {
    let mut archive = sevenz_rust2::ArchiveReader::open(path, sevenz_rust2::Password::empty())
        .with_context(|| format!("opening {}", path.display()))?;
    let mut found: Option<Result<Vec<Question>>> = None;
    archive
        .for_each_entries(|entry, reader: &mut dyn Read| {
            let name = entry.name().rsplit(['/', '\\']).next().unwrap_or("");
            if found.is_some() || !name.eq_ignore_ascii_case("Posts.xml") {
                // Entries of a solid archive must still be read through.
                std::io::copy(reader, &mut std::io::sink())?;
                return Ok(true);
            }
            found = Some(read_questions(
                BufReader::with_capacity(1 << 20, reader),
                min_score,
                keep,
            ));
            Ok(true)
        })
        .with_context(|| format!("unpacking {}", path.display()))?;
    match found {
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
        let all = read_questions(POSTS.as_bytes(), i64::MIN, 10).unwrap();
        let ids: Vec<u64> = all.iter().map(|q| q.id).collect();
        assert_eq!(ids, [927358, 11227809, 5, 6]);
        assert_eq!(all[2].title, "Why doesn't this & that work?");
        assert_eq!(all[1].tags, ["java", "c++", "performance"]);
        // Only the best viewed, and only questions with a score.
        let top = read_questions(POSTS.as_bytes(), 0, 2).unwrap();
        assert_eq!(
            top.iter().map(|q| q.id).collect::<Vec<_>>(),
            [927358, 11227809]
        );
        let scored = read_questions(POSTS.as_bytes(), 0, 10).unwrap();
        assert_eq!(scored.len(), 3);
    }

    #[test]
    fn questions_become_articles() {
        let question = read_questions(POSTS.as_bytes(), 0, 1).unwrap().remove(0);
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
        let questions = read_questions(POSTS.as_bytes(), 1, 10).unwrap();
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

    #[test]
    fn entities_decode() {
        assert_eq!(
            decode_entities("a &amp;&#x2F;&#47; b &bogus c"),
            "a &// b &bogus c"
        );
    }
}
