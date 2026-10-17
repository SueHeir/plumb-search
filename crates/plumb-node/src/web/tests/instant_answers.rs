//! Instant answers, facts, profiles and info boxes.

use super::*;

#[tokio::test]
async fn instant_answers_go_above_the_results() {
    let fake = backend(bank_hits());
    let (status, _, body) = get(fake.clone(), "/search?q=12*(3%2B4)").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("<p class=\"iaq\">12 × (3 + 4) =</p><p class=\"iaa\">84</p>"),
        "{body}"
    );
    let (_, _, body) = get(fake.clone(), "/search?q=10+km+in+miles").await;
    assert!(body.contains("6.21371 miles"), "{body}");
    let (_, _, body) = get(fake.clone(), "/search?q=us+bank").await;
    assert!(!body.contains("class=\"ia\""));
    let (_, _, body) = get(fake, "/api/search?q=2%2B2&full=1").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["answer"]["answer"], "4");
    assert!(json["hits"].is_array());
}

/// Suggests a spelling for one query, as the node's spellers do.
struct SpellingBackend;

impl SearchBackend for SpellingBackend {
    fn search(&self, _: &str, _: usize) -> Result<Vec<Hit>> {
        Ok(Vec::new())
    }

    fn num_docs(&self) -> u64 {
        1
    }

    fn search_full(&self, query: &str, _: usize, _: &SearchOptions) -> Result<SearchResults> {
        let fixed = match query {
            "10 kilomters in miles" => "10 kilometers in miles",
            "2+2" => "2+3",
            _ => return Ok(SearchResults::default()),
        };
        Ok(SearchResults {
            spelling: Some(Spelling {
                query: fixed.into(),
                site: None,
                applied: false,
            }),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn a_misspelled_ask_gets_the_answer_to_its_spelling() {
    let app = router_with(Arc::new(SpellingBackend), HomeCountry::Off);
    let (_, _, body) = send(app.clone(), "/search?q=10+kilomters+in+miles").await;
    assert!(body.contains("6.21371 miles"), "{body}");
    let (_, _, body) = send(app.clone(), "/api/search?q=10+kilomters+in+miles&full=1").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(json["answer"]["answer"]
        .as_str()
        .unwrap()
        .contains("6.21371"));
    // The answer to what was typed comes first.
    let (_, _, body) = send(app, "/api/search?q=2%2B2&full=1").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["answer"]["answer"], "4");
}

struct EntityAnswersBackend {
    displayed: Vec<plumb_index::pages::PlacedPage>,
    candidates: Vec<PageHit>,
    music: Vec<plumb_index::pages::PlacedPage>,
    lookups: Mutex<Vec<String>>,
}

impl EntityAnswersBackend {
    fn new(displayed: Vec<PageHit>, candidates: Vec<PageHit>) -> Self {
        Self {
            displayed: displayed
                .into_iter()
                .enumerate()
                .map(|(at, hit)| plumb_index::pages::PlacedPage {
                    hit,
                    under: None,
                    at,
                })
                .collect(),
            candidates,
            music: Vec::new(),
            lookups: Mutex::new(Vec::new()),
        }
    }
}

impl SearchBackend for EntityAnswersBackend {
    fn search(&self, _: &str, _: usize) -> Result<Vec<Hit>> {
        Ok(Vec::new())
    }

    fn num_docs(&self) -> u64 {
        self.displayed.len() as u64
    }

    fn search_full(&self, query: &str, _: usize, _: &SearchOptions) -> Result<SearchResults> {
        Ok(SearchResults {
            pages: if query.ends_with(" song") {
                self.music.clone()
            } else {
                self.displayed.clone()
            },
            ..Default::default()
        })
    }

    fn entities(&self, query: &str, limit: usize, _: &SearchOptions) -> Result<Vec<PageHit>> {
        self.lookups.lock().unwrap().push(query.to_string());
        Ok(self.candidates.iter().take(limit).cloned().collect())
    }
}

fn answer_entity(title: &str, item: &str, description: &str) -> PageHit {
    PageHit {
        page: plumb_index::pages::Page::from_article(
            "en",
            plumb_core::Article {
                title: title.into(),
                item: Some(item.into()),
                description: Some(description.into()),
                ..Default::default()
            },
        ),
        named: true,
        whole: true,
        score: 1.0,
        popularity: 0.9,
        learned: None,
        meant: false,
    }
}

#[tokio::test]
async fn facts_resolve_japan_when_who_takes_the_displayed_result() {
    let mut who = answer_entity("Japan", "Q7817", "WHO country page");
    who.page.set = "reference".into();
    who.page.url = "https://www.who.int/countries/jpn/".into();
    let mut japan = answer_entity("Japan", "Q17", "country in East Asia");
    japan.page.facts.push(plumb_core::facts::Fact {
        kind: plumb_core::facts::FactKind::Population,
        value: "123802000;2024".into(),
    });
    // Japan's article is a direct entity candidate but was discarded
    // from the display lane, whose one available row belongs to WHO.
    let backend = Arc::new(EntityAnswersBackend::new(vec![who], vec![japan]));
    let app = router_with(backend.clone(), HomeCountry::Off);
    let (status, _, body) =
        send(app.clone(), "/api/search?q=japan+population&full=1&limit=1").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["answer"]["question"], "Population of Japan");
    assert_eq!(json["answer"]["answer"], "123,802,000");
    assert_eq!(json["answer"]["note"], "Counted in 2024, from Wikidata");
    assert_eq!(json["assembled"]["answer"], json["answer"]);
    assert_eq!(
        json["pages"][0]["item"]["url"],
        "https://www.who.int/countries/jpn/"
    );
    let (status, _, body) = send(app, "/search?q=japan+population&limit=1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<p class=\"iaq\">Population of Japan</p>"));
    assert!(body.contains("<p class=\"iaa\">123,802,000</p>"));
    assert!(body.contains("Counted in 2024, from Wikidata"));
    assert!(body.contains("https://www.who.int/countries/jpn/"));
    assert_eq!(*backend.lookups.lock().unwrap(), ["japan", "japan"]);
}

#[tokio::test]
async fn sums_and_operators_other_than_quotes_get_no_info_box() {
    // The results for "2*3" lead with its digits read without the sign.
    let number = answer_entity("23 (number)", "Q713498", "natural number");
    let backend = Arc::new(EntityAnswersBackend::new(vec![number], Vec::new()));
    let app = router_with(backend, HomeCountry::Off);
    let (_, _, body) = send(app.clone(), "/api/search?q=2*3&full=1").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["answer"]["answer"], "6");
    assert!(json["info"].is_null(), "{body}");
    let (_, _, body) = send(app.clone(), "/search?q=2*3").await;
    assert!(body.contains("<p class=\"iaa\">6</p>"), "{body}");
    assert!(!body.contains("class=\"ib\""), "{body}");
    // Nor does a search with operators, in the API as on the page.
    let (_, _, body) = send(app.clone(), "/api/search?q=23+-film&full=1").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(json["info"].is_null(), "{body}");
    // The number asked for alone gets its box, in quotes too.
    for query in ["23", "%2223%22"] {
        let (_, _, body) = send(app.clone(), &format!("/api/search?q={query}&full=1")).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["info"]["title"], "23 (number)", "{query}");
        let (_, _, body) = send(app.clone(), &format!("/search?q={query}")).await;
        assert!(body.contains("class=\"ib\""), "{query}: {body}");
    }
}

#[tokio::test]
async fn profiles_resolve_identity_even_when_the_display_names_another_page() {
    let mut beast = answer_entity("MrBeast", "Q19897578", "American YouTuber");
    beast.page.profiles.push(plumb_core::profiles::Profile {
        service: "youtube-handle".into(),
        id: "MrBeast".into(),
    });
    let unrelated = answer_entity("MrBeast (song)", "Q999", "song");
    let backend = Arc::new(EntityAnswersBackend::new(vec![unrelated], vec![beast]));
    let app = router_with(backend.clone(), HomeCountry::Off);
    let (_, _, body) = send(app.clone(), "/search?q=mrbeast+youtube").await;
    assert!(body.contains("<section class=\"pf\""));
    assert!(body.contains("href=\"https://www.youtube.com/@MrBeast\""));
    let (_, _, body) = send(app, "/api/search?q=mrbeast+youtube&full=1").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["profile"]["of"], "MrBeast");
    assert_eq!(json["profile"]["url"], "https://www.youtube.com/@MrBeast");
    assert_eq!(json["assembled"]["profile"], json["profile"]);
    assert_eq!(*backend.lookups.lock().unwrap(), ["mrbeast", "mrbeast"]);
}

#[tokio::test]
async fn facts_and_profiles_do_not_resolve_by_enrichment_availability() {
    let main = answer_entity("Japan", "Q17", "country in East Asia");
    let mut namesake = answer_entity("Japan (band)", "Q127814", "English band");
    namesake.page.facts.push(plumb_core::facts::Fact {
        kind: plumb_core::facts::FactKind::Population,
        value: "5".into(),
    });
    namesake.page.profiles.push(plumb_core::profiles::Profile {
        service: "youtube-handle".into(),
        id: "namesake".into(),
    });
    let missing = EntityAnswersBackend::new(vec![namesake.clone()], vec![namesake.clone(), main]);
    let mut first = answer_entity("Springfield (Illinois)", "Q28515", "city");
    first.page.facts = namesake.page.facts;
    first.page.profiles = namesake.page.profiles;
    let second = answer_entity("Springfield (Massachusetts)", "Q49158", "city");
    let ambiguous = EntityAnswersBackend::new(vec![first.clone()], vec![first, second]);
    for (backend, subject) in [(missing, "japan"), (ambiguous, "springfield")] {
        let app = router_with(Arc::new(backend), HomeCountry::Off);
        for property in ["population", "youtube"] {
            let (_, _, body) = send(
                app.clone(),
                &format!("/api/search?q={subject}+{property}&full=1"),
            )
            .await;
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert!(json.get("answer").is_none(), "{body}");
            assert!(json.get("profile").is_none(), "{body}");
            let (_, _, body) = send(app.clone(), &format!("/search?q={subject}+{property}")).await;
            assert!(!body.contains("<section class=\"ia\""), "{body}");
            assert!(!body.contains("<section class=\"pf\""), "{body}");
        }
    }
}

#[tokio::test]
async fn entity_profiles_keep_the_musicbrainz_lyrics_fallback() {
    let entity = answer_entity("Hey Jude", "Q210179", "song by The Beatles");
    let mut song = entity.clone();
    song.page = plumb_index::pages::Page::from_music(plumb_core::Article {
        title: "Hey Jude".into(),
        description: Some("Song by The Beatles, 1968".into()),
        aliases: vec!["Hey Jude The Beatles".into()],
        item: Some("recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69".into()),
        ..Default::default()
    })
    .unwrap();
    let mut backend = EntityAnswersBackend::new(vec![entity.clone()], vec![entity]);
    backend.music.push(plumb_index::pages::PlacedPage {
        hit: song,
        at: 0,
        under: None,
    });
    let app = router_with(Arc::new(backend), HomeCountry::Off);
    let (_, _, body) = send(app.clone(), "/api/search?q=hey+jude+lyrics&full=1").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["profile"]["of"], "Hey Jude");
    assert_eq!(json["profile"]["source"], "MusicBrainz");
    assert_eq!(json["profile"]["search"], true);
    assert_eq!(
        json["profile"]["url"],
        "https://genius.com/search?q=Hey+Jude+The+Beatles"
    );
    let (_, _, body) = send(app, "/search?q=hey+jude+lyrics").await;
    assert!(body.contains("<section class=\"pf\""));
    assert!(body.contains("https://genius.com/search?q=Hey+Jude+The+Beatles"));
}

/// Finds the article on MrBeast, named by "mrbeast", with his profiles.
struct BeastBackend;

impl SearchBackend for BeastBackend {
    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Hit>> {
        Ok(Vec::new())
    }

    fn search_full(
        &self,
        query: &str,
        _limit: usize,
        _options: &SearchOptions,
    ) -> Result<SearchResults> {
        use plumb_core::profiles::Profile;
        use plumb_index::pages::{Page, PageHit, PlacedPage};
        let mut results = SearchResults::default();
        if query.contains("mrbeast") {
            results.pages.push(PlacedPage {
                hit: PageHit {
                    page: Page {
                        paper: None,
                        set: "wikipedia-en".into(),
                        url: "https://en.wikipedia.org/wiki/MrBeast".into(),
                        title: "MrBeast".into(),
                        description: Some("American YouTuber".into()),
                        site: None,
                        views: 900_000,
                        aliases: Vec::new(),
                        item: Some("Q19897578".into()),
                        profiles: vec![Profile {
                            service: "youtube-handle".into(),
                            id: "MrBeast".into(),
                        }],
                        website: None,
                        package: None,
                        facts: Vec::new(),
                        lead: None,
                        names: Vec::new(),
                        sections: Vec::new(),
                        content_language: None,
                        search: None,
                    },
                    score: 1.0,
                    named: query == "mrbeast",
                    popularity: 0.9,
                    whole: false,
                    learned: None,
                    meant: false,
                },
                under: None,
                at: 0,
            });
        }
        Ok(results)
    }

    fn num_docs(&self) -> u64 {
        1
    }

    fn entities(&self, query: &str, limit: usize, options: &SearchOptions) -> Result<Vec<PageHit>> {
        Ok(self
            .search_full(query, limit, options)?
            .pages
            .into_iter()
            .map(|placed| placed.hit)
            .collect())
    }
}

#[tokio::test]
async fn a_profile_asked_for_comes_first() {
    let app = router_with(Arc::new(BeastBackend), HomeCountry::Off);
    let (status, _, body) = send(app.clone(), "/search?q=mrbeast+youtube").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<section class=\"pf\""), "{body}");
    assert!(body.contains("href=\"https://www.youtube.com/@MrBeast\""));
    assert!(
        body.contains("<h2>MrBeast</h2>"),
        "the info box is about him"
    );
    let (_, _, body) = send(app.clone(), "/search?q=mrbeast+twitch").await;
    assert!(!body.contains("class=\"pf\""), "no Twitch channel known");
    let (_, _, body) = send(app, "/api/search?q=mrbeast+youtube&full=1").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["profile"]["url"], "https://www.youtube.com/@MrBeast");
    assert_eq!(json["info"]["profiles"][0]["service"], "YouTube");
}

#[test]
fn an_article_the_query_names_gets_an_info_box() {
    use plumb_index::pages::{Page, PageHit, PlacedPage};
    let article = PageHit {
        page: Page {
            paper: None,
            set: "wikipedia-en".into(),
            url: "https://en.wikipedia.org/wiki/Marie_Curie".into(),
            title: "Marie Curie".into(),
            description: Some("Polish-French physicist and chemist (1867–1934)".into()),
            site: None,
            views: 100_000,
            aliases: Vec::new(),
            item: Some("Q7186".into()),
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            content_language: None,
            search: None,
        },
        score: 1.0,
        named: true,
        popularity: 0.9,
        whole: false,
        learned: None,
        meant: false,
    };
    let results = SearchResults {
        pages: vec![PlacedPage {
            hit: article,
            under: None,
            at: 0,
        }],
        hits: vec![scored("mariecurie.org.uk", 0.5)],
        site_search: None,
        spelling: None,
        meant: None,
    };
    let page = render_results(
        "marie curie",
        &results,
        None,
        &NetOutcome::NotAsked,
        &no_settings(),
        None,
        10,
        false,
        &Icons::default(),
    );
    assert!(page.contains("<div class=\"wrap wide\">"), "{page}");
    assert!(
        page.contains("<aside class=\"ib\" aria-label=\"About Marie Curie\"><h2>Marie Curie</h2>")
    );
    assert!(page.contains("href=\"https://www.wikidata.org/wiki/Q7186\""));
    // The article is still listed with the results.
    assert!(page.contains("<li class=\"pg\">"));

    let mut plain = results.clone();
    plain.pages.clear();
    let page = render_results(
        "marie curie",
        &plain,
        None,
        &NetOutcome::NotAsked,
        &no_settings(),
        None,
        10,
        false,
        &Icons::default(),
    );
    assert!(!page.contains("class=\"ib\""));
    // Without the box the results keep the same left edge.
    assert!(page.contains("<div class=\"wrap wide\">"), "{page}");
}
