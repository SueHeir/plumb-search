//! An example Plumb Search plugin: Hacker News stories for searches that
//! start or end with "hn" or "hacker news", from the public search API
//! that Algolia runs for Hacker News (<https://hn.algolia.com/api>).
//!
//! It also builds a set ahead of time, once a day: the stories with at
//! least 100 points since a week before its first run, which the node
//! keeps for itself as `local/hn-stories`. Each run asks again for the last
//! two days, since stories keep gaining points for a while, and saves where
//! it looked from.
//!
//! Build it with
//! `cargo build --release -p plumb-plugin-hacker-news --target wasm32-unknown-unknown`,
//! then see `docs/plugins.md` to install it.

use plumb_plugin::{emit, encode, get, Error, Item, PublishInput, Published, Query, SetItem};
use serde::Deserialize;
use serde_json::json;

/// Stories asked for.
const STORIES: usize = 6;
/// Points a story needs to go in the set.
const MIN_POINTS: u64 = 100;
/// How far back each run looks again: stories keep gaining points.
const LOOK_AGAIN: u64 = 2 * 86_400;
/// How far back the first run looks.
const FIRST_LOOK: u64 = 7 * 86_400;
/// Pages of 100 stories one run asks for, at most.
const PAGES: u64 = 10;

#[derive(Debug, Deserialize)]
struct Found {
    hits: Vec<Story>,
    #[serde(rename = "nbPages", default)]
    pages: u64,
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

/// Builds the set: the stories since the last run looked, less two days,
/// with at least [`MIN_POINTS`] points.
fn publish(input: &PublishInput) -> Result<Published, Error> {
    let since = input.state["since"]
        .as_u64()
        .unwrap_or(input.now.saturating_sub(FIRST_LOOK))
        .min(input.now.saturating_sub(LOOK_AGAIN));
    let filters = encode(&format!("points>={MIN_POINTS},created_at_i>{since}"));
    for page in 0..PAGES {
        let url = format!(
            "https://hn.algolia.com/api/v1/search_by_date?tags=story&numericFilters={filters}\
             &hitsPerPage=100&page={page}"
        );
        let found: Found = get(&url)?.json()?;
        let last = page + 1 >= found.pages;
        emit(&set_items(found))?;
        if last {
            break;
        }
    }
    Ok(Published::changes(
        json!({ "since": input.now.saturating_sub(LOOK_AGAIN) }),
    ))
}

/// The stories as items of the set.
fn set_items(found: Found) -> Vec<SetItem> {
    found
        .hits
        .into_iter()
        .filter_map(|story| {
            let title = story.title.filter(|t| !t.trim().is_empty())?;
            let discussion = format!("https://news.ycombinator.com/item?id={}", story.id);
            let url = story
                .url
                .filter(|u| u.starts_with("http"))
                .unwrap_or_else(|| discussion.clone());
            let mut item =
                SetItem::new(story.id, "story", title, url).field("discussion", &discussion);
            if let Some(points) = story.points {
                item = item.field("points", points).weight(points as f64);
            }
            if let Some(comments) = story.num_comments {
                item = item.field("comments", comments);
            }
            if let Some(author) = story.author {
                item = item.about(format!("by {author}"));
            }
            if let Some(at) = story.created_at_i {
                item = item.published(at);
            }
            Some(item)
        })
        .collect()
}

plumb_plugin::plugin!(search);
plumb_plugin::publish!(publish);

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

    #[test]
    fn stories_become_items_of_the_set() {
        let found: Found = serde_json::from_str(
            r#"{"nbPages":1,"hits":[
                {"objectID":"1","title":"Rust 2.0","url":"https://blog.rust-lang.org/x",
                 "author":"steve","points":120,"num_comments":45,"created_at_i":1700000000},
                {"objectID":"2","title":"Ask HN: Rust jobs?","url":null,"points":101}
            ]}"#,
        )
        .unwrap();
        let items = set_items(found);
        assert_eq!(
            serde_json::to_value(&items[0]).unwrap(),
            json!({
                "id": "1", "kind": "story", "title": "Rust 2.0",
                "url": "https://blog.rust-lang.org/x", "about": "by steve",
                "weight": 120.0, "published": "2023-11-14T22:13:20Z",
                "fields": {"discussion": "https://news.ycombinator.com/item?id=1",
                           "points": 120, "comments": 45},
            })
        );
        assert_eq!(
            items[1].url.as_deref(),
            Some("https://news.ycombinator.com/item?id=2")
        );
    }
}
