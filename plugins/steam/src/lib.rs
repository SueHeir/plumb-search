//! A Plumb Search plugin for your Steam account, through the Steam Web API
//! (<https://steamcommunity.com/dev>) with your own key:
//!
//! - `steam portal` or `my games portal` finds games in your library, with
//!   the hours you have played them;
//! - results about a game (its store page, its Wikipedia article) get an
//!   "Owned, 42 h" or "On your wishlist" badge.
//!
//! Its `config.json` holds your key and your SteamID64; see
//! `config.example.json`. Build it with
//! `cargo build --release -p plumb-plugin-steam --target wasm32-unknown-unknown`,
//! then see `docs/plugins.md` to install it.

use std::collections::HashMap;

use plumb_plugin::{encode, get, Error, Item, Note, Query, Shown};
use serde::Deserialize;

/// Games listed for a search of the library.
const GAMES: usize = 8;

#[derive(Debug, Deserialize)]
struct Owned {
    response: OwnedGames,
}

#[derive(Debug, Default, Deserialize)]
struct OwnedGames {
    #[serde(default)]
    games: Vec<Game>,
}

#[derive(Debug, Deserialize)]
struct Game {
    appid: u64,
    #[serde(default)]
    name: Option<String>,
    /// Minutes played.
    #[serde(default)]
    playtime_forever: u64,
    #[serde(default)]
    rtime_last_played: u64,
}

#[derive(Debug, Deserialize)]
struct Wishlist {
    response: WishlistItems,
}

#[derive(Debug, Default, Deserialize)]
struct WishlistItems {
    #[serde(default)]
    items: Vec<Wished>,
}

#[derive(Debug, Deserialize)]
struct Wished {
    appid: u64,
}

/// The key and account from `config.json`.
struct Account {
    key: String,
    steamid: String,
}

fn account(config: &serde_json::Value) -> Result<Account, Error> {
    let key = config["key"].as_str().unwrap_or("").trim();
    let steamid = config["steamid"].as_str().unwrap_or("").trim();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(Error::Other(
            "config.json needs your Steam Web API key as \"key\"".into(),
        ));
    }
    if steamid.len() != 17 || !steamid.chars().all(|c| c.is_ascii_digit()) {
        return Err(Error::Other(
            "config.json needs your 17-digit SteamID64 as \"steamid\"".into(),
        ));
    }
    Ok(Account {
        key: key.into(),
        steamid: steamid.into(),
    })
}

fn owned_games(account: &Account, names: bool) -> Result<Vec<Game>, Error> {
    let url = format!(
        "https://api.steampowered.com/IPlayerService/GetOwnedGames/v1/?key={}&steamid={}&include_appinfo={}&include_played_free_games=1&format=json",
        encode(&account.key),
        encode(&account.steamid),
        u8::from(names)
    );
    let owned: Owned = get(&url)?.json()?;
    Ok(owned.response.games)
}

fn wishlist(account: &Account) -> Result<Vec<u64>, Error> {
    let url = format!(
        "https://api.steampowered.com/IWishlistService/GetWishlist/v1/?key={}&steamid={}&format=json",
        encode(&account.key),
        encode(&account.steamid)
    );
    let wishlist: Wishlist = get(&url)?.json()?;
    Ok(wishlist
        .response
        .items
        .into_iter()
        .map(|wished| wished.appid)
        .collect())
}

fn search(query: &Query) -> Result<Vec<Item>, Error> {
    // A search about a game runs this too (for `ids`); its badge comes from
    // `annotate`, so only a keyword search lists the library.
    if query.keyword.is_none() || query.terms.trim().is_empty() {
        return Ok(Vec::new());
    }
    let account = account(&query.config)?;
    Ok(matching(owned_games(&account, true)?, &query.terms))
}

/// The games whose names hold every word of `terms`, most played first.
fn matching(games: Vec<Game>, terms: &str) -> Vec<Item> {
    let words: Vec<String> = words(terms);
    let mut found: Vec<Game> = games
        .into_iter()
        .filter(|game| {
            let name = words_joined(game.name.as_deref().unwrap_or(""));
            !words.is_empty() && words.iter().all(|word| name.contains(&format!(" {word}")))
        })
        .collect();
    found.sort_by_key(|game| std::cmp::Reverse(game.playtime_forever));
    found
        .into_iter()
        .take(GAMES)
        .filter_map(|game| {
            let name = game.name.filter(|n| !n.trim().is_empty())?;
            let mut item = Item::new(
                name,
                format!("https://store.steampowered.com/app/{}/", game.appid),
            )
            .badge("Owned")
            .image(format!(
                "https://cdn.cloudflare.steamstatic.com/steam/apps/{}/capsule_184x69.jpg",
                game.appid
            ))
            .snippet(format!(
                "In your Steam library, {}.",
                played(game.playtime_forever)
            ));
            if game.rtime_last_played > 0 {
                item = item.published(game.rtime_last_played);
            }
            Some(item)
        })
        .collect()
}

fn annotate(shown: &Shown) -> Result<Vec<Note>, Error> {
    let wanted: Vec<(u32, u64)> = shown
        .results
        .iter()
        .filter_map(|result| {
            let id = result.about.as_ref()?.id("steam")?.parse().ok()?;
            Some((result.id, id))
        })
        .collect();
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let account = account(&shown.config)?;
    let owned = owned_games(&account, false)?;
    // A private or empty wishlist is no reason to lose the owned badges.
    let wished = wishlist(&account).unwrap_or_default();
    Ok(notes(&wanted, &owned, &wished))
}

fn notes(wanted: &[(u32, u64)], owned: &[Game], wished: &[u64]) -> Vec<Note> {
    let minutes: HashMap<u64, u64> = owned
        .iter()
        .map(|game| (game.appid, game.playtime_forever))
        .collect();
    wanted
        .iter()
        .filter_map(|&(result, app)| {
            if let Some(&played_for) = minutes.get(&app) {
                let badge = if played_for >= 60 {
                    format!("Owned, {} h", played_for / 60)
                } else {
                    "Owned".to_string()
                };
                Some(Note::new(result).badge(badge))
            } else if wished.contains(&app) {
                Some(Note::new(result).badge("On your wishlist"))
            } else {
                None
            }
        })
        .collect()
}

fn played(minutes: u64) -> String {
    match minutes {
        0 => "never played".into(),
        1..=59 => format!("{minutes} min played"),
        60..=119 => "1 hour played".into(),
        _ => format!("{} hours played", minutes / 60),
    }
}

/// Lowercase words of letters and digits: "Half-Life 2" is `half`,
/// `life`, `2`.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The words of `text` between single spaces, with a space at each end so
/// that a word matches only a whole word's start.
fn words_joined(text: &str) -> String {
    format!(" {} ", words(text).join(" "))
}

plumb_plugin::plugin!(search);
plumb_plugin::annotate!(annotate);

#[cfg(test)]
mod tests {
    use super::*;
    use plumb_plugin::Query;

    fn library() -> Vec<Game> {
        let owned: Owned = serde_json::from_str(
            r#"{"response":{"game_count":3,"games":[
                {"appid":620,"name":"Portal 2","playtime_forever":1500,"rtime_last_played":1700000000},
                {"appid":400,"name":"Portal","playtime_forever":30},
                {"appid":220,"name":"Half-Life 2","playtime_forever":0}
            ]}}"#,
        )
        .unwrap();
        owned.response.games
    }

    #[test]
    fn finds_library_games_by_name_most_played_first() {
        let items = matching(library(), "portal");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Portal 2");
        assert_eq!(items[0].url, "https://store.steampowered.com/app/620/");
        assert_eq!(
            items[0].snippet.as_deref(),
            Some("In your Steam library, 25 hours played.")
        );
        assert_eq!(items[0].published, Some(1_700_000_000));
        assert_eq!(items[1].title, "Portal");
        assert_eq!(items[1].published, None);

        let items = matching(library(), "half life");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].snippet.as_deref(),
            Some("In your Steam library, never played.")
        );
        // Words match from their start only.
        assert!(matching(library(), "ortal").is_empty());
    }

    #[test]
    fn badges_owned_and_wishlisted_games_only() {
        let wished: Wishlist =
            serde_json::from_str(r#"{"response":{"items":[{"appid":70,"priority":1}]}}"#).unwrap();
        let wished: Vec<u64> = wished.response.items.iter().map(|w| w.appid).collect();
        let notes = notes(&[(1, 620), (2, 400), (3, 70), (4, 10)], &library(), &wished);
        let badges: Vec<(u32, &str)> = notes
            .iter()
            .map(|note| (note.id, note.badge.as_deref().unwrap()))
            .collect();
        assert_eq!(
            badges,
            [(1, "Owned, 25 h"), (2, "Owned"), (3, "On your wishlist")]
        );
    }

    #[test]
    fn a_private_library_reads_as_empty() {
        let owned: Owned = serde_json::from_str(r#"{"response":{}}"#).unwrap();
        assert!(owned.response.games.is_empty());
    }

    #[test]
    fn asks_for_a_key_and_steamid() {
        assert!(account(&serde_json::Value::Null).is_err());
        let config = serde_json::json!({"key": "ABC123", "steamid": "7656"});
        assert!(account(&config).is_err());
        let config = serde_json::json!({"key": "ABC123", "steamid": "76561197960287930"});
        assert!(account(&config).is_ok());
    }

    #[test]
    fn searches_about_a_game_without_a_keyword_list_nothing() {
        let query = Query {
            text: "portal 2".into(),
            terms: "portal 2".into(),
            ..Query::default()
        };
        assert!(search(&query).unwrap().is_empty());
    }
}
