use super::*;
use crate::mcp::tools::MAX_FACT_SUBJECTS;

/// Finds Wikipedia's article on Australia, with its facts, for any query
/// naming it, and Mount Everest's for any naming that.
struct Australia;

impl SearchBackend for Australia {
    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Hit>> {
        Ok(Vec::new())
    }

    fn search_full(
        &self,
        query: &str,
        _limit: usize,
        _options: &SearchOptions,
    ) -> Result<SearchResults> {
        use plumb_core::facts::{Fact, FactKind};
        let mut pages = Vec::new();
        let article = match query.to_lowercase() {
            query if query.contains("australia") => Some(plumb_core::article::Article {
                title: "Australia".into(),
                description: Some("country in Oceania".into()),
                item: Some("Q408".into()),
                facts: vec![
                    Fact {
                        kind: FactKind::Capital,
                        value: "Canberra".into(),
                    },
                    Fact {
                        kind: FactKind::Population,
                        value: "27204809;2024".into(),
                    },
                ],
                ..Default::default()
            }),
            query if query.contains("everest") => Some(plumb_core::article::Article {
                title: "Mount Everest".into(),
                description: Some("highest mountain on Earth".into()),
                item: Some("Q513".into()),
                facts: vec![Fact {
                    kind: FactKind::Elevation,
                    value: "8848.86".into(),
                }],
                ..Default::default()
            }),
            _ => None,
        };
        if let Some(article) = article {
            let page = plumb_index::pages::Page::from_article("en", article);
            pages.push(plumb_index::pages::PlacedPage {
                hit: plumb_index::pages::PageHit {
                    page,
                    score: 0.9,
                    named: true,
                    popularity: 1.0,
                    whole: true,
                    learned: None,
                },
                under: None,
                at: 0,
            });
        }
        Ok(SearchResults {
            hits: Vec::new(),
            pages,
            site_search: None,
            spelling: None,
        })
    }

    fn entities(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
    ) -> Result<Vec<plumb_index::pages::PageHit>> {
        Ok(self
            .search_full(query, limit, options)?
            .pages
            .into_iter()
            .map(|placed| placed.hit)
            .collect())
    }

    fn num_docs(&self) -> u64 {
        0
    }
}

#[test]
fn facts_come_with_the_wikidata_item_and_property_they_are_from() {
    let mcp = Mcp::new(Arc::new(Australia), None);
    let answer = call(&mcp, "facts", json!({ "subject": "Australia" }));
    let result = &answer["result"];
    assert_eq!(result["isError"], false, "{answer}");
    let data = &result["structuredContent"];
    assert_eq!(data["item"], "Q408");
    assert_eq!(data["facts"][0]["question"], "Capital of Australia");
    assert_eq!(data["facts"][0]["value"], "Canberra");
    assert_eq!(
        data["facts"][0]["source"],
        "https://www.wikidata.org/wiki/Q408#P36"
    );
    let text = result["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        text,
        "Australia, country in Oceania https://en.wikipedia.org/wiki/Australia\n\
         Capital of Australia: Canberra [Wikidata Q408 P36]\n\
         Population of Australia: 27,204,809 (Counted in 2024) [Wikidata Q408 P1082]\n\
         Source: https://www.wikidata.org/wiki/Q408"
    );

    // One kind, by its key or as people ask it.
    for about in ["capital", "capital city"] {
        let answer = call(
            &mcp,
            "facts",
            json!({ "subject": "australia", "about": about }),
        );
        let facts = answer["result"]["structuredContent"]["facts"]
            .as_array()
            .unwrap();
        assert_eq!(facts.len(), 1, "{about}: {answer}");
        assert_eq!(facts[0]["kind"], "capital");
    }
    // A mountain's height is its elevation; a country has neither.
    let answer = call(
        &mcp,
        "facts",
        json!({ "subject": "Mount Everest", "about": "height" }),
    );
    let facts = answer["result"]["structuredContent"]["facts"]
        .as_array()
        .unwrap();
    assert_eq!(facts.len(), 1, "{answer}");
    assert_eq!(facts[0]["kind"], "elevation");
    assert_eq!(facts[0]["property"], "P2044");
    let answer = call(
        &mcp,
        "facts",
        json!({ "subject": "australia", "about": "height" }),
    );
    assert_eq!(answer["result"]["structuredContent"]["found"], false);
    // A kind it has no fact of, a kind it never keeps, a subject it lacks.
    let answer = call(
        &mcp,
        "facts",
        json!({ "subject": "australia", "about": "ceo" }),
    );
    assert_eq!(answer["result"]["structuredContent"]["found"], false);
    let answer = call(
        &mcp,
        "facts",
        json!({ "subject": "australia", "about": "favourite colour" }),
    );
    assert_eq!(answer["result"]["isError"], false);
    assert_eq!(
        answer["result"]["structuredContent"]["status"],
        "unsupported_property"
    );
    let text = answer["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("capital, population"), "{text}");
    let answer = call(&mcp, "facts", json!({ "subject": "Atlantis" }));
    assert_eq!(
        answer["result"]["content"][0]["text"],
        "Plumb could not resolve Atlantis to an indexed entity."
    );

    // Several subjects in one call, each answered as alone.
    let answer = call(
        &mcp,
        "facts",
        json!({ "subject": "Australia", "about": "capital", "more_subjects": ["Atlantis", "australia"] }),
    );
    let data = &answer["result"]["structuredContent"];
    assert_eq!(data["facts"][0]["value"], "Canberra", "{answer}");
    assert_eq!(data["more"][0]["found"], false);
    assert_eq!(data["more"][1]["facts"][0]["value"], "Canberra");
    assert_eq!(
        answer["result"]["content"][0]["text"],
        "Australia, country in Oceania https://en.wikipedia.org/wiki/Australia\n\
         Capital of Australia: Canberra [Wikidata Q408 P36]\n\
         Source: https://www.wikidata.org/wiki/Q408\n\
         Plumb could not resolve Atlantis to an indexed entity.\n\
         Australia, country in Oceania https://en.wikipedia.org/wiki/Australia\n\
         Capital of Australia: Canberra [Wikidata Q408 P36]\n\
         Source: https://www.wikidata.org/wiki/Q408"
    );
    // Without more_subjects the answer has no "more".
    let answer = call(&mcp, "facts", json!({ "subject": "Australia" }));
    assert!(answer["result"]["structuredContent"].get("more").is_none());
    let many: Vec<String> = (0..MAX_FACT_SUBJECTS)
        .map(|n| format!("place {n}"))
        .collect();
    for more in [json!(many), json!("Canada")] {
        let answer = call(
            &mcp,
            "facts",
            json!({ "subject": "Australia", "more_subjects": more }),
        );
        assert_eq!(answer["error"]["code"], INVALID_PARAMS, "{answer}");
    }
}

#[test]
fn site_info_says_what_it_folds_and_whose_official_site_it_is() {
    let mcp = scripted(|_| {
        let mut placed = article("National Park Service", Some("nps.gov"), None);
        placed.under = Some("nps.gov".into());
        results(
            vec![titled("nps.gov", "NPS.gov Homepage", 0.69, true)],
            vec![placed],
        )
    });
    let reply = call(
        &mcp,
        "site_info",
        json!({ "domain": "https://developer.nps.gov/api/" }),
    );
    let answer = &reply["result"]["structuredContent"];
    assert_eq!(answer["domain"], "nps.gov");
    assert_eq!(answer["host"], "developer.nps.gov");
    assert_eq!(answer["part_of"], "nps.gov");
    assert_eq!(answer["official"], true);
    assert_eq!(answer["official_for"], "National Park Service");
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("Plumb keeps no entry of its own for developer.nps.gov"),
        "{text}"
    );
    assert!(
        text.contains("official website of National Park Service"),
        "{text}"
    );

    // www. is the site itself.
    let answer =
        &call(&mcp, "site_info", json!({ "domain": "www.nps.gov" }))["result"]["structuredContent"];
    assert_eq!(answer.get("host"), None);
}

/// Displayed search deliberately loses the entity behind a WHO namesake.
struct EntityIndex(plumb_index::pages::PageSearcher);

impl SearchBackend for EntityIndex {
    fn search(&self, _: &str, _: usize) -> Result<Vec<Hit>> {
        Ok(Vec::new())
    }
    fn search_full(&self, _: &str, _: usize, _: &SearchOptions) -> Result<SearchResults> {
        let page = Page::from_set(
            plumb_index::pages::SUBPAGES_SET,
            plumb_core::article::Article {
                title: "Japan".into(),
                item: Some("https://www.who.int/countries/jpn".into()),
                ..Default::default()
            },
        )
        .unwrap();
        Ok(SearchResults {
            hits: Vec::new(),
            pages: vec![PlacedPage {
                hit: shelved(page),
                under: None,
                at: 0,
            }],
            spelling: None,
            site_search: None,
        })
    }
    fn entities(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
    ) -> Result<Vec<plumb_index::pages::PageHit>> {
        Ok(self
            .0
            .entities(query, 200)?
            .into_iter()
            .filter(|hit| plumb_index::pages::options_allow(options, &hit.page))
            .take(limit)
            .collect())
    }
    fn num_docs(&self) -> u64 {
        0
    }
}

#[test]
fn facts_resolve_identity_before_display_and_property_availability() {
    use plumb_core::article::Article;
    use plumb_core::facts::{Fact, FactKind};
    let article = |title: &str, item: &str, description: &str, facts: Vec<Fact>| {
        Page::from_article(
            "en",
            Article {
                title: title.into(),
                item: Some(item.into()),
                description: Some(description.into()),
                facts,
                ..Default::default()
            },
        )
    };
    let mut japan = article(
        "Japan",
        "Q17",
        "country in East Asia",
        vec![Fact {
            kind: FactKind::Population,
            value: "123802000;2024".into(),
        }],
    );
    japan.profiles.push(plumb_core::profiles::Profile {
        service: "youtube-handle".into(),
        id: "JapanGov".into(),
    });
    let pages = vec![
        japan,
        article("Apple", "Q89", "fruit", Vec::new()),
        article(
            "Apple Inc.",
            "Q312",
            "American technology company",
            Vec::new(),
        ),
        article(
            "Apple Records",
            "Q200",
            "record label",
            vec![Fact {
                kind: FactKind::Ceo,
                value: "Namesake CEO".into(),
            }],
        ),
        article(
            "Pear Records",
            "Q881",
            "record label",
            vec![Fact {
                kind: FactKind::Ceo,
                value: "Namesake CEO".into(),
            }],
        ),
        article("Mercury (planet)", "Q308", "planet", Vec::new()),
        article(
            "Mercury (element)",
            "Q925",
            "chemical element",
            vec![Fact {
                kind: FactKind::AtomicNumber,
                value: "80".into(),
            }],
        ),
    ];
    let dir = tempfile::tempdir().unwrap();
    let index = dir.path().join("pages");
    plumb_index::pages::build_page_index(&index, pages).unwrap();
    let mcp = Mcp::new(
        Arc::new(EntityIndex(
            plumb_index::pages::PageSearcher::open(&index).unwrap(),
        )),
        None,
    );
    let facts = mcp
        .facts("Japan", Some("population"), &SearchOptions::default())
        .unwrap();
    assert_eq!(facts["item"], "Q17");
    assert_eq!(facts["status"], "ok");
    assert_eq!(facts["facts"][0]["value"], "123,802,000");
    assert_eq!(facts["facts"][0]["observation_year"], 2024);
    assert_eq!(facts["facts"][0]["unit"], "people");
    assert_eq!(facts["facts"][0]["raw_values"][0], "123802000;2024");
    assert_eq!(facts["freshness"], "unknown");
    assert!(facts["facts"][0]["retrieved_at"].is_null());
    let search = mcp
        .search("Japan population", None, &SearchOptions::default())
        .unwrap();
    assert_eq!(search["answer"]["answer"], "123,802,000");
    let search = mcp
        .search("Japan youtube", None, &SearchOptions::default())
        .unwrap();
    assert_eq!(search["profile"]["of"], "Japan");
    let missing = mcp
        .facts("Apple", Some("ceo"), &SearchOptions::default())
        .unwrap();
    assert_eq!(missing["item"], "Q312", "{missing}");
    assert_eq!(missing["status"], "missing_enrichment");
    assert_eq!(missing["found"], false);
    for about in ["founder", "founded"] {
        let ambiguous = mcp
            .facts("Apple", Some(about), &SearchOptions::default())
            .unwrap();
        assert_eq!(ambiguous["status"], "ambiguous_entity", "{ambiguous}");
    }
    let partial = mcp
        .facts("Pear", Some("ceo"), &SearchOptions::default())
        .unwrap();
    assert_eq!(partial["status"], "unresolved_entity", "{partial}");
    let ambiguous = mcp
        .facts("Mercury", Some("atomic-number"), &SearchOptions::default())
        .unwrap();
    assert_eq!(ambiguous["status"], "ambiguous_entity");
    assert_eq!(ambiguous["candidates"].as_array().unwrap().len(), 2);
    let specified = mcp
        .facts(
            "Mercury (element)",
            Some("atomic-number"),
            &SearchOptions::default(),
        )
        .unwrap();
    assert_eq!(specified["item"], "Q925");
}

#[test]
fn founding_facts_clarify_shared_names_and_keep_qualified_entity_coverage() {
    use plumb_core::facts::{
        Fact,
        FactKind::{Founded, Founder},
    };
    let page = |title: &str, item: &str, description: &str, facts| {
        Page::from_article(
            "en",
            plumb_core::Article {
                title: title.into(),
                item: Some(item.into()),
                description: Some(description.into()),
                aliases: vec!["Nacrelin".into()],
                facts,
                ..Default::default()
            },
        )
    };
    let pages = vec![
        page("Nacrelin", "Q910101", "mineral", Vec::new()),
        page(
            "Nacrelin Inc.",
            "Q910102",
            "company",
            vec![Fact {
                kind: Founded,
                value: "1901".into(),
            }],
        ),
        page(
            "Nacrelin (city)",
            "Q910103",
            "city",
            vec![
                Fact {
                    kind: Founder,
                    value: "Example founder".into(),
                },
                Fact {
                    kind: Founded,
                    value: "1801".into(),
                },
            ],
        ),
    ];
    let dir = tempfile::tempdir().unwrap();
    let index = dir.path().join("pages");
    plumb_index::pages::build_page_index(&index, pages).unwrap();
    let mcp = Mcp::new(
        Arc::new(EntityIndex(
            plumb_index::pages::PageSearcher::open(&index).unwrap(),
        )),
        None,
    );
    for about in ["founder", "founded"] {
        let answer = mcp
            .facts("Nacrelin", Some(about), &SearchOptions::default())
            .unwrap();
        assert_eq!(answer["status"], "ambiguous_entity", "{answer}");
        assert_eq!(answer["candidates"].as_array().unwrap().len(), 3);
    }
    let city = mcp
        .facts(
            "Nacrelin (city)",
            Some("founder"),
            &SearchOptions::default(),
        )
        .unwrap();
    assert_eq!(city["status"], "ok", "{city}");
    assert_eq!(city["item"], "Q910103");
    assert_eq!(city["facts"][0]["value"], "Example founder");
    let company = mcp
        .facts("Nacrelin Inc.", Some("founded"), &SearchOptions::default())
        .unwrap();
    assert_eq!(company["status"], "ok", "{company}");
    assert_eq!(company["item"], "Q910102");
    assert_eq!(company["facts"][0]["value"], "1901");
    let missing = mcp
        .facts("Nacrelin Inc.", Some("founder"), &SearchOptions::default())
        .unwrap();
    assert_eq!(missing["status"], "missing_enrichment", "{missing}");
    assert_eq!(missing["item"], "Q910102");
    assert!(!missing["found"].as_bool().unwrap());
    let search = mcp
        .search("who founded Nacrelin", None, &SearchOptions::default())
        .unwrap();
    assert!(search["answer"].is_null(), "{search}");
}

struct LyricsWithEntity;

impl SearchBackend for LyricsWithEntity {
    fn search(&self, _: &str, _: usize) -> Result<Vec<Hit>> {
        Ok(Vec::new())
    }
    fn num_docs(&self) -> u64 {
        0
    }
    fn entities(
        &self,
        _: &str,
        _: usize,
        _: &SearchOptions,
    ) -> Result<Vec<plumb_index::pages::PageHit>> {
        let mut hit = shelved(Page::from_article(
            "en",
            plumb_core::article::Article {
                title: "Hey Jude".into(),
                description: Some("song by The Beatles".into()),
                item: Some("Q210179".into()),
                ..Default::default()
            },
        ));
        hit.named = true;
        Ok(vec![hit])
    }
    fn search_full(&self, query: &str, _: usize, _: &SearchOptions) -> Result<SearchResults> {
        let pages = if query.ends_with(" song") {
            let page = Page::from_music(plumb_core::article::Article {
                title: "Hey Jude".into(),
                description: Some("Song by The Beatles, 1968".into()),
                aliases: vec!["Hey Jude The Beatles".into()],
                item: Some("recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69".into()),
                ..Default::default()
            })
            .unwrap();
            vec![PlacedPage {
                hit: plumb_index::pages::PageHit {
                    named: true,
                    ..shelved(page)
                },
                under: None,
                at: 0,
            }]
        } else {
            Vec::new()
        };
        Ok(SearchResults {
            hits: Vec::new(),
            pages,
            spelling: None,
            site_search: None,
        })
    }
}

#[test]
fn facts_entity_profiles_keep_the_existing_music_fallback() {
    let mcp = Mcp::new(Arc::new(LyricsWithEntity), None);
    let answer = mcp
        .search("Hey Jude lyrics", None, &SearchOptions::default())
        .unwrap();
    assert_eq!(answer["profile"]["of"], "Hey Jude");
    assert_eq!(answer["profile"]["source"], "MusicBrainz");
    assert_eq!(answer["profile"]["search"], true);
    assert!(answer["profile"]["url"]
        .as_str()
        .unwrap()
        .starts_with("https://genius.com/search?"));
}

/// Finds `articles` for every name, and each of `songs` for its search.
struct Profiles {
    articles: Vec<plumb_index::pages::PageHit>,
    songs: Vec<(&'static str, PlacedPage)>,
}

impl SearchBackend for Profiles {
    fn search(&self, _: &str, _: usize) -> Result<Vec<Hit>> {
        Ok(Vec::new())
    }
    fn num_docs(&self) -> u64 {
        0
    }
    fn entities(
        &self,
        query: &str,
        _: usize,
        _: &SearchOptions,
    ) -> Result<Vec<plumb_index::pages::PageHit>> {
        // As in the page index, a film's title alone finds its article,
        // not the films set's page.
        Ok(self
            .articles
            .iter()
            .filter(|hit| {
                hit.page.set != plumb_index::pages::FILMS_SET
                    || !hit.page.title.eq_ignore_ascii_case(query)
            })
            .cloned()
            .collect())
    }
    fn search_full(&self, query: &str, _: usize, _: &SearchOptions) -> Result<SearchResults> {
        Ok(SearchResults {
            pages: self
                .songs
                .iter()
                .filter(|(asked, _)| *asked == query)
                .map(|(_, song)| song.clone())
                .collect(),
            ..Default::default()
        })
    }
}

#[test]
fn lyrics_come_from_the_song_asked_for() {
    let article = |title: &str, item: &str, description: &str| plumb_index::pages::PageHit {
        named: true,
        ..shelved(Page::from_article(
            "en",
            plumb_core::article::Article {
                title: title.into(),
                description: Some(description.into()),
                item: Some(item.into()),
                ..Default::default()
            },
        ))
    };
    let song = |title: &str, by: &str, genius: &str| PlacedPage {
        hit: plumb_index::pages::PageHit {
            named: true,
            ..shelved(
                Page::from_music(plumb_core::article::Article {
                    title: title.into(),
                    description: Some(format!("Song by {by}, 1992")),
                    aliases: vec![format!("{title} {by}")],
                    item: Some("recording/70595637-9310-45f2-a266-58f8de4874a7".into()),
                    profiles: vec![plumb_core::profiles::Profile {
                        service: "genius-song".into(),
                        id: genius.into(),
                    }],
                    ..Default::default()
                })
                .unwrap(),
            )
        },
        under: None,
        at: 0,
    };
    let lyrics = |backend: Profiles, query: &str| {
        Mcp::new(Arc::new(backend), None)
            .search(query, None, &SearchOptions::default())
            .unwrap()
    };
    // Two articles are called "Creep"; MusicBrainz lists Radiohead's
    // first, with its page on Genius.
    let creep = Profiles {
        articles: vec![
            article(
                "Creep (Radiohead song)",
                "Q1140405",
                "1992 single by Radiohead",
            ),
            article("Creep (TLC song)", "Q2304339", "1994 single by TLC"),
        ],
        songs: vec![(
            "creep song",
            song("Creep", "Radiohead", "Radiohead-creep-lyrics"),
        )],
    };
    let found = lyrics(creep, "creep lyrics");
    let genius = "https://genius.com/Radiohead-creep-lyrics";
    assert_eq!(found["profile"]["url"], genius);
    // It is the first result, as on the results page, and a listing of
    // the song rather than an official profile.
    assert_eq!(
        found["ordered_results"][0],
        json!({"kind": "profile", "url": genius})
    );
    let text = super::text::render("search", &found);
    assert!(
        text.contains(&format!("Listing: Creep on Genius: {genius}")),
        "{text}"
    );
    // "Pyramid Song" is Radiohead's, never Wolfmother's "Pyramid", which a
    // search for "pyramid song" finds as a song.
    let pyramid = Profiles {
        articles: vec![article(
            "Pyramid Song",
            "Q1755624",
            "2001 single by Radiohead",
        )],
        songs: vec![(
            "pyramid song",
            song("Pyramid", "Wolfmother", "Wolfmother-pyramid-lyrics"),
        )],
    };
    assert!(lyrics(pyramid, "pyramid song lyrics")["profile"].is_null());
}

#[test]
fn listings_are_of_the_kind_of_work_they_list() {
    let article = |title: &str, item: &str, imdb: Option<&str>| plumb_index::pages::PageHit {
        named: true,
        ..shelved(Page::from_article(
            "en",
            plumb_core::article::Article {
                title: title.into(),
                item: Some(item.into()),
                profiles: imdb
                    .map(|id| plumb_core::profiles::Profile {
                        service: "imdb".into(),
                        id: id.into(),
                    })
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        ))
    };
    let profile = |articles, query: &str| {
        let backend = Profiles {
            articles,
            songs: Vec::new(),
        };
        Mcp::new(Arc::new(backend), None)
            .search(query, None, &SearchOptions::default())
            .unwrap()["profile"]
            .clone()
    };
    let barbie = || {
        vec![
            article("Barbie", "Q167447", None),
            article("Barbie (film)", "Q55606767", Some("tt1517268")),
        ]
    };
    assert_eq!(
        profile(barbie(), "barbie cast")["url"],
        "https://www.imdb.com/title/tt1517268/"
    );
    // Other profiles are still the doll's, which has none; two shows of
    // the name stay ambiguous.
    assert!(profile(barbie(), "barbie youtube").is_null());
    let offices = vec![
        article(
            "The Office (American TV series)",
            "Q23831",
            Some("tt0386676"),
        ),
        article(
            "The Office (British TV series)",
            "Q23834",
            Some("tt0290978"),
        ),
    ];
    assert!(profile(offices, "the office cast").is_null());
    // A store of games lists the game, not the god.
    let mut hades = vec![
        article("Hades", "Q42272", None),
        article("Hades (video game)", "Q63000000", None),
    ];
    hades[1].page.profiles = vec![plumb_core::profiles::Profile {
        service: "steam".into(),
        id: "1145360".into(),
    }];
    assert_eq!(
        profile(hades, "hades steam")["url"],
        "https://store.steampowered.com/app/1145360/"
    );
    // "top gun movie" names the 1955 film of the films set, but asks for
    // the film called Top Gun, found by its name alone.
    let imdb = |id: &str| {
        vec![plumb_core::profiles::Profile {
            service: "imdb".into(),
            id: id.into(),
        }]
    };
    let mut old = shelved(
        Page::from_film(plumb_core::article::Article {
            title: "Top Gun".into(),
            description: Some("Film by Henry Levin, 1955".into()),
            item: Some("Q7824830/Top_Gun_(1955_film)".into()),
            ..Default::default()
        })
        .unwrap(),
    );
    old.named = true;
    old.page.profiles = imdb("tt0048734");
    let mut top_gun = article("Top Gun", "Q49003", Some("tt0092099"));
    top_gun.page.description = Some("1986 action drama film by Tony Scott".into());
    assert_eq!(
        profile(vec![old, top_gun], "top gun movie cast")["url"],
        "https://www.imdb.com/title/tt0092099/"
    );
}
