//! Songs, albums, films, shows and podcasts.

use super::*;

fn song(title: &str, by: &str, item: &str, listeners: u64) -> Page {
    Page::from_music(Article {
        title: title.into(),
        description: Some(format!("{by}, 1968")),
        item: Some(item.into()),
        views: listeners,
        aliases: vec![format!("{title} {}", by.split_once(" by ").unwrap().1)],
        ..Article::default()
    })
    .unwrap()
}

#[test]
fn songs_and_albums_are_found_by_their_name_and_artist() {
    let jude = song(
        "Hey Jude",
        "Song by The Beatles",
        "recording/a1b2c3d4-d987-4042-ae91-78d6a3267d69",
        120_000,
    );
    assert_eq!(
        jude.url,
        "https://musicbrainz.org/recording/a1b2c3d4-d987-4042-ae91-78d6a3267d69"
    );
    assert!(jude.is_song() && !jude.may_lead());
    assert_eq!(jude.set_name(), "MusicBrainz");
    // With guests, the main artist first finds it too.
    let slow = song(
        "Drive Slow",
        "Song by Kanye West feat. Paul Wall & GLC",
        "recording/a1b2c3d4-0000-4042-ae91-78d6a3267d69",
        30_000,
    );
    let (_dir, s) = searcher(std::slice::from_ref(&slow));
    for query in [
        "kanye west drive slow",
        "kanye west drive slow song",
        "drive slow kanye west",
    ] {
        let hits = s.search(query, 5).unwrap();
        assert!(
            !hits.is_empty() && hits[0].page.url == slow.url,
            "{query}: {hits:?}"
        );
    }
    let white = song(
        "The Beatles",
        "Album by The Beatles",
        "release-group/a1b2c3d4-a1db-32aa-b14f-bc9cc507b843",
        40_000,
    );
    assert!(!white.is_song());
    let (_dir, s) = searcher(&[
        jude.clone(),
        white,
        page("Hey Jude", 300_000, &[]),
        page("The Beatles", 900_000, &[]),
    ]);
    for query in [
        "hey jude the beatles",
        "hey jude beatles",
        "hey jude by the beatles",
        "hey jude song",
        "the beatles hey jude",
        "beatles hey jude",
        "beatles hey jude song",
    ] {
        let hits = s.search(query, 5).unwrap();
        assert!(
            hits[0].whole && hits[0].named && hits[0].page.url == jude.url,
            "{query}: {hits:?}"
        );
    }
    // Alone, the title asks for the article only, and part of it for
    // no song.
    let hits = s.search("hey jude", 5).unwrap();
    assert_eq!(titles(&hits), ["Hey Jude"]);
    assert!(hits[0].page.is_article());
    assert!(s
        .search("jude", 5)
        .unwrap()
        .iter()
        .all(|hit| hit.page.set != MUSIC_SET));
    // The artist alone, or with words that are not the title, asks
    // for no song.
    for query in ["the beatles", "beatles hey"] {
        assert!(
            s.search(query, 5)
                .unwrap()
                .iter()
                .all(|hit| hit.page.url != jude.url),
            "{query}"
        );
    }
    let hits = s.search("the beatles album", 5).unwrap();
    assert!(hits[0].whole && hits[0].page.set == MUSIC_SET && !hits[0].page.is_song());
    // An item of neither kind is no page.
    assert_eq!(
        Page::from_music(Article {
            title: "x".into(),
            item: Some("artist/a1b2c3d4-d987-4042-ae91-78d6a3267d69".into()),
            ..Article::default()
        }),
        None
    );
}

#[test]
fn a_makers_albums_songs_films_or_books_find_the_list_of_them() {
    let (_dir, s) = searcher(&[
        page("Drake (musician)", 90_000, &["Drake"]),
        page("Drake albums discography", 9_000, &[]),
        page("Drake discography", 8_000, &[]),
        page("Pink Floyd discography", 7_000, &[]),
        page("The Beatles", 100_000, &[]),
        page("List of songs recorded by the Beatles", 7_000, &[]),
        page("Tom Hanks filmography", 6_000, &[]),
        page("Stephen King bibliography", 6_000, &[]),
    ]);
    for (query, title) in [
        ("drake albums", "Drake albums discography"),
        ("drake discography", "Drake discography"),
        ("pink floyd albums", "Pink Floyd discography"),
        ("pink floyd songs", "Pink Floyd discography"),
        ("beatles songs", "List of songs recorded by the Beatles"),
        ("The Beatles songs", "List of songs recorded by the Beatles"),
        ("tom hanks movies", "Tom Hanks filmography"),
        ("stephen king books", "Stephen King bibliography"),
    ] {
        let hits = s.search(query, 5).unwrap();
        assert!(
            hits.iter().any(|hit| hit.page.title == title && hit.named),
            "{query}: {hits:?}"
        );
    }
    // Only someone's works have such a list.
    for query in ["best albums", "horror movies"] {
        assert!(s.search(query, 5).unwrap().iter().all(|hit| !hit.named));
    }
}

#[test]
fn a_title_alone_is_for_a_song_far_better_known_than_its_namesakes() {
    let creep = song(
        "Creep",
        "Song by Radiohead",
        "recording/a1b2c3d4-0001-4042-ae91-78d6a3267d69",
        288_000,
    );
    let tlc = song(
        "Creep",
        "Song by TLC",
        "recording/a1b2c3d4-0002-4042-ae91-78d6a3267d69",
        34_000,
    );
    let oasis = song(
        "Hello",
        "Song by Oasis",
        "recording/a1b2c3d4-0003-4042-ae91-78d6a3267d69",
        93_000,
    );
    let evanescence = song(
        "Hello",
        "Song by Evanescence",
        "recording/a1b2c3d4-0004-4042-ae91-78d6a3267d69",
        83_000,
    );
    let quiet = song(
        "Dead Sea",
        "Song by The Lumineers",
        "recording/a1b2c3d4-0005-4042-ae91-78d6a3267d69",
        40_000,
    );
    let (_dir, s) = searcher(&[
        creep.clone(),
        tlc,
        oasis,
        evanescence,
        quiet,
        page("Creep", 50_000, &[]),
    ]);
    assert_eq!(s.known_song("creep").unwrap(), Some(creep.clone()));
    assert_eq!(s.known_song("Creep").unwrap(), Some(creep));
    // Titles shared about equally, too few listeners, or no title.
    assert_eq!(s.known_song("hello").unwrap(), None);
    assert_eq!(s.known_song("dead sea").unwrap(), None);
    assert_eq!(s.known_song("radiohead").unwrap(), None);
    // The title still lists no song.
    assert!(s
        .search("creep", 5)
        .unwrap()
        .iter()
        .all(|hit| hit.page.set != MUSIC_SET));
}

#[test]
fn films_and_shows_are_found_by_their_title_and_year_or_maker() {
    let dune = film(
        "Dune",
        "Film by Denis Villeneuve, 2021 · with Timothée Chalamet, Zendaya",
        "Q63985561/Dune_(2021_film)",
        80,
    );
    assert_eq!(dune.url, "https://en.wikipedia.org/wiki/Dune_(2021_film)");
    assert_eq!(dune.item.as_deref(), Some("Q63985561"));
    assert!(dune.is_film_with_article() && !dune.is_show() && !dune.may_lead());
    assert_eq!(dune.set_name(), "Wikipedia");
    let lynch = film(
        "Dune",
        "Film by David Lynch, 1984 · with Kyle MacLachlan",
        "Q114819/Dune_(1984_film)",
        60,
    );
    // No English article: listed as itself, on Wikidata.
    let foreign = film(
        "Les Dents de la nuit",
        "Film by Stephen Cafiero, 2008",
        "Q3230000",
        4,
    );
    assert_eq!(foreign.url, "https://www.wikidata.org/wiki/Q3230000");
    assert_eq!(foreign.set_name(), "Wikidata");
    assert_eq!(foreign.set_domain(), "wikidata.org");
    let bad = film(
        "Breaking Bad",
        "TV series by Vince Gilligan, 2008–2013 · with Bryan Cranston",
        "Q1079/Breaking_Bad",
        90,
    );
    assert!(bad.is_show());
    let mut article_2021 = page("Dune (2021 film)", 400_000, &[]);
    article_2021.item = Some("Q63985561".into());
    let mut novel = page("Dune (novel)", 900_000, &["Dune"]);
    novel.item = Some("Q190192".into());
    let mut show_article = page("Breaking Bad", 800_000, &[]);
    show_article.item = Some("Q1079".into());
    let (_dir, s) = searcher(&[
        dune.clone(),
        lynch.clone(),
        foreign.clone(),
        bad,
        article_2021,
        novel,
        show_article,
    ]);
    // The year, the director or "movie" asks for the film, found as its
    // article.
    for (query, url) in [
        (
            "dune 2021",
            "https://en.wikipedia.org/wiki/Dune_(2021_film)",
        ),
        (
            "dune villeneuve",
            "https://en.wikipedia.org/wiki/Dune_(2021_film)",
        ),
        (
            "dune 1984",
            "https://en.wikipedia.org/wiki/Dune_(1984_film)",
        ),
        (
            "dune david lynch",
            "https://en.wikipedia.org/wiki/Dune_(1984_film)",
        ),
        (
            "dune 2021 movie",
            "https://en.wikipedia.org/wiki/Dune_(2021_film)",
        ),
        (
            "dune zendaya",
            "https://en.wikipedia.org/wiki/Dune_(2021_film)",
        ),
        (
            "breaking bad tv show",
            "https://en.wikipedia.org/wiki/Breaking_Bad",
        ),
        (
            "les dents de la nuit 2008",
            "https://www.wikidata.org/wiki/Q3230000",
        ),
    ] {
        let hits = s.search(query, 5).unwrap();
        assert!(
            hits[0].whole && hits[0].named && hits[0].page.url == url,
            "{query}: {hits:?}"
        );
    }
    // Found with it, the article is what is listed, once.
    let hits = s.search("dune 2021", 5).unwrap();
    assert!(hits[0].page.is_article(), "{hits:?}");
    assert_eq!(
        hits.iter()
            .filter(|h| h.page.item.as_deref() == Some("Q63985561"))
            .count(),
        1
    );
    // Not found with it, the film is listed with its article's address.
    let hits = s.search("dune 1984", 5).unwrap();
    assert_eq!(hits[0].page.set, FILMS_SET);
    // Its title alone asks for the articles, not the films.
    let hits = s.search("dune", 5).unwrap();
    assert!(hits.iter().all(|h| h.page.set != FILMS_SET), "{hits:?}");
    // A show is not asked for as a movie, nor a film as a show.
    let hits = s.search("breaking bad movie", 5).unwrap();
    assert!(hits.iter().all(|h| !h.whole), "{hits:?}");
    let hits = s.search("dune tv series", 5).unwrap();
    assert!(hits.iter().all(|h| !h.whole), "{hits:?}");
    // A film with no article is listed by its title, below the sites.
    let hits = s.search("les dents de la nuit", 5).unwrap();
    assert!(hits[0].named && !hits[0].whole && hits[0].page.url == foreign.url);
    // An item that does not read is no page.
    for item in ["X1", "Q12/bad path", ""] {
        assert_eq!(
            Page::from_film(Article {
                title: "x".into(),
                item: Some(item.into()),
                ..Article::default()
            }),
            None,
            "{item}"
        );
    }
}

#[test]
fn podcasts_are_found_by_their_name_and_podcast() {
    let podcast = Page::from_podcast(Article {
        title: "Dan Carlin's Hardcore History".into(),
        description: Some("Podcast by Dan Carlin · History".into()),
        item: Some("1".into()),
        site: Some("dancarlin.com".into()),
        views: 90_070,
        aliases: vec!["Dan Carlin podcast".into()],
        ..Article::default()
    });
    assert_eq!(podcast.url, "https://podcastindex.org/podcast/1");
    assert_eq!(podcast.set_name(), "Podcast Index");
    let (_dir, s) = searcher(&[podcast, page("History", 500_000, &[])]);
    for query in [
        "hardcore history podcast",
        "dan carlin's hardcore history podcast",
        "dan carlin's hardcore history dan carlin",
    ] {
        let hits = s.search(query, 5).unwrap();
        assert!(
            hits[0].whole && hits[0].page.set == PODCASTS_SET,
            "{query}: {hits:?}"
        );
    }
    let hits = s.search("dan carlin podcast", 5).unwrap();
    assert!(hits[0].named && hits[0].page.set == PODCASTS_SET);
    // Alone, a title of the end of another is no podcast's.
    assert!(!s
        .search("history podcast", 5)
        .unwrap()
        .iter()
        .any(|hit| hit.whole));
}
