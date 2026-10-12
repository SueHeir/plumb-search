//! An example Plumb Search plugin: Hacker News stories for searches that
//! start or end with "hn" or "hacker news", from the public search API
//! that Algolia runs for Hacker News (<https://hn.algolia.com/api>).
//!
//! Build it with
//! `cargo build --release -p plumb-plugin-hacker-news --target wasm32-unknown-unknown`,
//! then see `docs/plugins.md` to install it.

use plumb_plugin::{encode, get, Error, Item, Query};
use serde::Deserialize;

/// Stories asked for.
const STORIES: usize = 6;

#[derive(Debug, Deserialize)]
struct Found {
    hits: Vec<Story>,
}

#[derive(Debug, Deserialize)]
struct Story {
    #[serde(rename = "objectID")]
    id: String,
    title: Option<String>,
    url: Option<String>,
    author: Option<String>,
    points: Option<u64>,
    num_comments: Option<u64>,
    created_at_i: Option<u64>,
}

fn search(query: &Query) -> Result<Vec<Item>, Error> {
    if query.terms.trim().is_empty() {
        return Ok(Vec::new());
    }
    let url = format!(
        "https://hn.algolia.com/api/v1/search?query={}&tags=story&hitsPerPage={STORIES}",
        encode(&query.terms)
    );
    let found: Found = get(&url)?.json()?;
    Ok(items(found))
}

fn items(found: Found) -> Vec<Item> {
    found
        .hits
        .into_iter()
        .filter_map(|story| {
            let title = story.title.filter(|t| !t.trim().is_empty())?;
            let discussion = format!("https://news.ycombinator.com/item?id={}", story.id);
            // Ask HN and other text posts have no link of their own.
            let url = story
                .url
                .filter(|u| u.starts_with("http"))
                .unwrap_or_else(|| discussion.clone());
            let mut about = Vec::new();
            if let Some(points) = story.points {
                about.push(format!("{points} points"));
            }
            if let Some(comments) = story.num_comments {
                about.push(format!("{comments} comments"));
            }
            if let Some(author) = story.author {
                about.push(format!("by {author}"));
            }
            let mut item = Item::new(title, url)
                .snippet(format!("{} on Hacker News: {discussion}", about.join(", ")));
            if let Some(at) = story.created_at_i {
                item = item.published(at);
            }
            Some(item)
        })
        .collect()
}

plumb_plugin::plugin!(search);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_stories_and_links_text_posts_to_their_discussion() {
        let found: Found = serde_json::from_str(
            r#"{"hits":[
                {"objectID":"1","title":"Rust 2.0","url":"https://blog.rust-lang.org/x",
                 "author":"steve","points":120,"num_comments":45,"created_at_i":1700000000},
                {"objectID":"2","title":"Ask HN: Rust jobs?","url":null,"author":"amy"},
                {"objectID":"3","title":null,"url":"https://untitled.example/"}
            ]}"#,
        )
        .unwrap();
        let items = items(found);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].url, "https://blog.rust-lang.org/x");
        assert_eq!(
            items[0].snippet.as_deref(),
            Some("120 points, 45 comments, by steve on Hacker News: https://news.ycombinator.com/item?id=1")
        );
        assert_eq!(items[0].published, Some(1_700_000_000));
        assert_eq!(items[1].url, "https://news.ycombinator.com/item?id=2");
    }
}
