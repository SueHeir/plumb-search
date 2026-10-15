//! What a results page says: spelling, songs, a site's pages, the source
//! line, icons and headlines.

use super::*;

#[test]
fn a_misspelled_name_offers_its_site_with_the_suggestion() {
    let results = SearchResults {
        pages: Vec::new(),
        hits: vec![scored("youtbue.com", 0.9)],
        site_search: None,
        spelling: Some(Spelling {
            query: "youtube".into(),
            site: Some("youtube.com".into()),
            applied: false,
        }),
    };
    let page = render_results(
        "youtbue",
        &results,
        None,
        &NetOutcome::NotAsked,
        &no_settings(),
        None,
        10,
        true,
        &Icons::default(),
    );
    assert!(
        page.contains(
            "<strong>youtube</strong></a>? <a class=\"sps\" \
             href=\"https://youtube.com/\" rel=\"noreferrer\">Go to youtube.com</a></p>"
        ),
        "{page}"
    );
    // The results are still for what was typed.
    assert!(page.contains("/go?q=youtbue&amp;d=youtbue.com"), "{page}");
}

#[test]
fn results_for_a_spelling_say_so_and_link_the_query_as_typed() {
    let results = SearchResults {
        pages: Vec::new(),
        hits: vec![scored("kanyewest.com", 0.9)],
        site_search: None,
        spelling: Some(Spelling {
            query: "kanye west".into(),
            site: None,
            applied: true,
        }),
    };
    let page = render_results(
        "kayne west",
        &results,
        None,
        &NetOutcome::NotAsked,
        &no_settings(),
        None,
        10,
        true,
        &Icons::default(),
    );
    assert!(
        page.contains("Showing results for <strong>kanye west</strong>. Search instead for"),
        "{page}"
    );
    assert!(page.contains("q=kayne+west"), "{page}");
    assert!(page.contains("exact=1"), "{page}");
    assert!(!page.contains("Did you mean"), "{page}");
}

#[test]
fn typos_are_searched_as_typed_with_a_suggestion() {
    let mut results = SearchResults {
        pages: Vec::new(),
        hits: vec![scored("amazom.com", 0.9)],
        site_search: None,
        spelling: Some(Spelling {
            query: "amazon".into(),
            site: None,
            applied: false,
        }),
    };
    let mut settings = no_settings();
    settings.options.country = Some("DE".into());
    settings.country_mode = "DE".into();
    let page = render_results(
        "amazom",
        &results,
        None,
        &NetOutcome::NotAsked,
        &settings,
        None,
        10,
        true,
        &Icons::default(),
    );
    assert!(
        page.contains(
            "<p class=\"sp\">Did you mean <a href=\"/search?q=amazon&amp;country=DE&amp;lang=any\">\
         <strong>amazon</strong></a>?</p>"
        ),
        "{page}"
    );
    assert!(!page.contains("Showing results for"), "{page}");
    // Picks are noted for the query as typed.
    assert!(page.contains("/go?q=amazom&amp;d=amazom.com"), "{page}");

    // Searching as typed carries on through the picks.
    let mut settings = no_settings();
    settings.options.exact = true;
    results.spelling = None;
    let page = render_results(
        "gogle",
        &results,
        None,
        &NetOutcome::NotAsked,
        &settings,
        None,
        10,
        true,
        &Icons::default(),
    );
    assert!(page.contains("&amp;exact=1"), "{page}");
    assert!(!page.contains("class=\"sp\""), "{page}");
}

#[test]
fn a_search_naming_a_song_near_the_top_is_about_it() {
    let song =
        |title: &str, named: bool, under: Option<&str>, at: usize| plumb_index::pages::PlacedPage {
            hit: PageHit {
                page: plumb_index::pages::Page::from_music(plumb_core::Article {
                    title: title.into(),
                    description: Some("Song by Radiohead, 1992".into()),
                    item: Some("recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69".into()),
                    ..Default::default()
                })
                .unwrap(),
                score: 0.5,
                named,
                popularity: 0.1,
                whole: false,
                learned: None,
                meant: false,
            },
            under: under.map(str::to_string),
            at,
        };
    let about = music_about(&[song("Creep", true, None, 1)]).map(crate::plugins::about_page);
    let about = about.expect("about the song");
    assert_eq!(about.title, "Creep");
    assert_eq!(about.by.as_deref(), Some("Radiohead"));
    assert!(about.id("musicbrainz-recording").is_some());
    // Not one the query does not name, one far down, or one under a site.
    assert!(music_about(&[song("Creep", false, None, 0)]).is_none());
    assert!(music_about(&[song("Creep", true, None, MUSIC_ABOUT_AT + 1)]).is_none());
    assert!(music_about(&[song("Creep", true, Some("radiohead.com"), 0)]).is_none());
    // The higher of two.
    let pages = [
        song("Creep (Live)", true, None, 2),
        song("Creep", true, None, 0),
    ];
    assert_eq!(music_about(&pages).unwrap().title, "Creep");
}

#[test]
fn site_queries_list_every_page_on_the_site() {
    let pages = ["Albert Einstein", "Einstein family", "Einstein (crater)"]
        .into_iter()
        .map(|title| plumb_index::pages::PlacedPage {
            hit: PageHit {
                page: plumb_index::pages::Page::from_article(
                    "en",
                    plumb_core::Article {
                        title: title.into(),
                        ..Default::default()
                    },
                ),
                score: 0.5,
                named: false,
                popularity: 0.1,
                whole: false,
                learned: None,
                meant: false,
            },
            under: None,
            at: 0,
        })
        .collect();
    let results = SearchResults {
        pages,
        hits: Vec::new(),
        site_search: None,
        spelling: None,
    };
    let page = render_results(
        "einstein site:wikipedia.org",
        &results,
        None,
        &NetOutcome::NotAsked,
        &no_settings(),
        None,
        10,
        false,
        &Icons::default(),
    );
    assert!(!page.contains("No sites match"));
    for title in ["Albert Einstein", "Einstein family", "Einstein (crater)"] {
        assert!(page.contains(title), "{title}");
    }
}

#[test]
fn a_site_shows_every_page_under_it() {
    let under = |page: plumb_index::pages::Page| plumb_index::pages::PlacedPage {
        hit: PageHit {
            page,
            score: 0.5,
            named: false,
            popularity: 0.1,
            whole: false,
            learned: None,
            meant: false,
        },
        under: Some("census.gov".into()),
        at: 0,
    };
    let clock = plumb_index::pages::Page::from_set(
        plumb_index::pages::SUBPAGES_SET,
        plumb_core::Article {
            title: "Population Clock".into(),
            item: Some("https://www.census.gov/popclock/".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let bureau = plumb_index::pages::Page::from_article(
        "en",
        plumb_core::Article {
            title: "United States Census Bureau".into(),
            ..Default::default()
        },
    );
    let results = SearchResults {
        pages: vec![under(clock), under(bureau)],
        hits: vec![scored("census.gov", 0.9)],
        site_search: None,
        spelling: None,
    };
    let page = render_results(
        "census population clock",
        &results,
        None,
        &NetOutcome::NotAsked,
        &no_settings(),
        None,
        10,
        false,
        &Icons::default(),
    );
    let clock = page
        .find("https://www.census.gov/popclock/")
        .expect("the clock");
    let bureau = page
        .find("United States Census Bureau")
        .expect("the article");
    assert!(clock < bureau, "{page}");
}

#[test]
fn the_source_line_says_where_results_came_from() {
    let results = SearchResults {
        pages: Vec::new(),
        hits: vec![scored("a.com", 0.9)],
        site_search: None,
        spelling: None,
    };
    let mut settings = no_settings();
    settings.network = NetSetting::Off;
    settings.options.country = Some("DE".into());
    settings.country_mode = "DE".into();
    let page = render_results(
        "q",
        &results,
        None,
        &NetOutcome::NotAsked,
        &settings,
        None,
        10,
        false,
        &Icons::default(),
    );
    assert!(page.contains(
        "From this site's own index. <a href=\"/search?q=q&amp;country=DE&amp;net=1&amp;lang=any\">"
    ));

    settings.network = NetSetting::On;
    let page = render_results(
        "q",
        &results,
        None,
        &NetOutcome::Failed,
        &settings,
        None,
        10,
        false,
        &Icons::default(),
    );
    assert!(page.contains("the Plumb network did not answer this time"));
    assert!(page.contains("a.com"));

    let page = render_results(
        "q",
        &results,
        None,
        &answered(Vec::new()),
        &settings,
        None,
        10,
        false,
        &Icons::default(),
    );
    assert!(!page.contains("Tinted"));
    let none = NetOutcome::Answered(NetworkResults::default());
    let page = render_results(
        "q",
        &results,
        None,
        &none,
        &settings,
        None,
        10,
        false,
        &Icons::default(),
    );
    assert!(page.contains("none of the Plumb nodes it searches are connected right"));
}

#[test]
fn scheduled_downloads_and_stale_results_have_clear_status() {
    let pending = NetworkResults {
        pending: 2,
        ..Default::default()
    };
    let page = render_network("us bank", &pending, &Icons::default(), &no_settings());
    assert!(
        page.contains("waiting for the next background download"),
        "{page}"
    );
    assert!(!page.contains("No other nodes are connected"), "{page}");
    assert!(!page.contains("No node had a site matching"), "{page}");

    let stale = NetworkResults {
        cached: 1,
        stale: 1,
        ..Default::default()
    };
    let mut line = String::new();
    let mut settings = no_settings();
    settings.network = NetSetting::On;
    render_source(
        &mut line,
        "us bank",
        &settings,
        &NetOutcome::Answered(stale),
        0,
    );
    assert!(line.contains("saved Plumb results, read locally"), "{line}");
    assert!(line.contains("may be out of date"), "{line}");
    assert!(!line.contains("no other Plumb nodes"), "{line}");
}

#[test]
fn results_show_site_icons_inline_and_letters_otherwise() {
    let results = SearchResults {
        pages: Vec::new(),
        spelling: None,
        hits: vec![
            hit(
                "usbank.com",
                "https://www.usbank.com/",
                Some("U.S. Bank"),
                None,
            ),
            hit(
                "chase.com",
                "https://www.chase.com/personal",
                Some("Chase"),
                None,
            ),
        ],
        site_search: None,
    };
    let mut icons = Icons::default();
    icons
        .0
        .insert("usbank.com".into(), "data:image/png;base64,iVBORw0K".into());
    let page = render_results(
        "bank",
        &results,
        None,
        &NetOutcome::NotAsked,
        &no_settings(),
        None,
        10,
        false,
        &icons,
    );
    assert!(page.contains(
        "<span class=\"ic\"><img src=\"data:image/png;base64,iVBORw0K\" alt=\"\" \
         width=\"18\" height=\"18\"></span>"
    ));
    let (letter, color) = site_initial("chase.com");
    assert_eq!(letter, 'C');
    assert!(page.contains(&format!(
        "<span class=\"ic l{color}\" aria-hidden=\"true\">C</span>"
    )));
    assert!(page.contains("<span class=\"u\">www.chase.com/personal</span>"));
    // An address that is just the domain is not repeated.
    let results = SearchResults {
        pages: Vec::new(),
        spelling: None,
        hits: vec![hit("jsr.io", "https://jsr.io/", Some("JSR"), None)],
        site_search: None,
    };
    let page = render_results(
        "jsr",
        &results,
        None,
        &NetOutcome::NotAsked,
        &no_settings(),
        None,
        10,
        false,
        &Icons::default(),
    );
    assert!(
        page.contains("<span class=\"dn\">jsr.io</span></span>"),
        "{page}"
    );
    // The page loads no image from anywhere: icons ride inside it.
    assert!(!page.contains("src=\"http"));
    assert!(CONTENT_SECURITY_POLICY.contains("img-src data:;"));
}

#[test]
fn recent_headlines_follow_the_first_result_escaped() {
    let results = SearchResults {
        pages: Vec::new(),
        spelling: None,
        hits: vec![
            hit("news.com", "https://news.com/", Some("News"), None),
            hit("other.com", "https://other.com/", Some("Other"), None),
        ],
        site_search: None,
    };
    let now = now_unix();
    let headline = |title: &str, url: &str| crate::news::RecentHeadline {
        domain: "news.com".into(),
        title: title.into(),
        url: url.into(),
        at: now - 2 * 3600,
    };
    let recent = Recent {
        status: "available".into(),
        sources: Vec::new(),
        site: Some("news.com".into()),
        headlines: vec![
            headline(
                "<script>alert(1)</script> wins",
                "https://news.com/a?x=1&y=2",
            ),
            headline("Sneaky", "javascript:alert(1)"),
        ],
    };
    let page = render_results_with(
        "news",
        &results,
        None,
        &NetOutcome::NotAsked,
        &no_settings(),
        None,
        10,
        false,
        &Icons::default(),
        Some(&recent),
    );
    let block = page.find("<li class=\"news\">").expect("a Recent block");
    assert!(
        page.find("news.com/").unwrap() < block,
        "after the first result"
    );
    assert!(block < page.find("other.com").unwrap(), "before the second");
    // Folded by default, behind a one-line summary.
    assert!(page.contains(
        "<details><summary><span class=\"nh\">Latest from news.com</span> \
         <span class=\"m\">1 headline, newest 2 hours ago</span></summary>"
    ));
    assert!(page.contains(
        "<a href=\"https://news.com/a?x=1&amp;y=2\" rel=\"noreferrer\">\
         &lt;script&gt;alert(1)&lt;/script&gt; wins</a>"
    ));
    assert!(page.contains("news.com &middot; 2 hours ago"));
    assert!(!page.contains("Sneaky"));
    assert!(!page.contains("<script>"));

    // The gear's "Recent news" choice opens it, or leaves it out.
    let render = |view: RecentNews| {
        let mut settings = no_settings();
        settings.options.recent = view;
        render_results_with(
            "news",
            &results,
            None,
            &NetOutcome::NotAsked,
            &settings,
            None,
            10,
            false,
            &Icons::default(),
            Some(&recent),
        )
    };
    let open = render(RecentNews::Expanded);
    assert!(open.contains("<li class=\"news\"><details open>"));
    assert!(open.contains("<option value=\"expanded\" selected>Open</option>"));
    assert!(!render(RecentNews::Off).contains("class=\"news\""));
    // The choice rides along in the page's links, unless it is the default.
    let mut options = SearchOptions::default();
    assert!(!search_link("/search", "x", &options, false).contains("news="));
    options.recent = RecentNews::Off;
    assert!(search_link("/search", "x", &options, false).ends_with("&news=off"));
}

struct FeedNews(crate::news::NewsStore);

impl StatusSource for FeedNews {
    fn status(&self) -> Status {
        node_status(Phase::Ready, Step::Idle)
    }
    fn recent(
        &self,
        query: &str,
        top: Option<(&str, bool)>,
        language: Option<&str>,
    ) -> Option<Recent> {
        self.0.recent(query, top, now_unix(), language)
    }
}

#[tokio::test]
async fn publisher_headlines_share_dates_sources_and_unavailable_status() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::news::NewsStore::open(dir.path());
    let now = now_unix();
    store.watch(vec![crate::news::Watched {
        domain: "bbc.co.uk".into(),
        homepage: "https://www.bbc.co.uk/".into(),
        ..Default::default()
    }]);
    store.apply(
        vec![plumb_crawl::FeedCheck {
            domain: "bbc.co.uk".into(),
            outcome: plumb_crawl::FeedOutcome::Read {
                feed: "https://feeds.bbci.co.uk/news/rss.xml".into(),
                etag: None,
                last_modified: None,
                headlines: vec![plumb_core::Headline {
                    title: "NVIDIA earnings rise".into(),
                    url: "https://www.bbc.com/news/business/1".into(),
                    at: now - 60,
                }],
            },
        }],
        now,
    );
    let node = Arc::new(FeedNews(store));
    let backend = backend(bank_hits());
    let mcp = crate::mcp::Mcp::new(backend.clone(), None).with_node(Some(node.clone()));
    let answer = mcp
        .search("BBC latest news", Some(2), &SearchOptions::default())
        .unwrap();
    let app = node_router(backend, node);
    let (_, _, body) = send(app.clone(), "/api/search?q=BBC+latest+news&full=1&limit=2").await;
    let api: serde_json::Value = serde_json::from_str(&body).unwrap();
    let (_, _, body) = send(app.clone(), "/api/recent?q=BBC+latest+news&limit=2").await;
    let recent: serde_json::Value = serde_json::from_str(&body).unwrap();
    let (_, _, body) = send(
        app.clone(),
        "/search?q=BBC+latest+news&format=json&categories=news&limit=2",
    )
    .await;
    let searx: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(api["recent"], recent);
    assert_eq!(searx["recent"], recent);
    assert_eq!(
        answer["recent"][0]["published_at"],
        recent["headlines"][0]["at"]
    );
    assert_eq!(
        answer["recent"][0]["site"],
        recent["headlines"][0]["domain"]
    );
    assert_eq!(answer["news_status"]["sources"], recent["sources"]);
    assert_eq!(recent["sources"][1]["checked_at"], now);
    assert!(answer["recent"][0]["published_date"]
        .as_str()
        .unwrap()
        .ends_with('Z'));
    assert_eq!(searx["results"][0]["url"], answer["recent"][0]["url"]);
    let (_, _, body) = send(app, "/api/recent?q=Reuters+latest+news").await;
    let unavailable: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(unavailable["status"], "source_unavailable");
    assert!(unavailable["headlines"].as_array().unwrap().is_empty());
}

#[test]
fn caddy_exposes_recent_without_management_routes() {
    let config = include_str!("../../../../../site/Caddyfile");
    let paths = config
        .lines()
        .find(|line| line.trim_start().starts_with("@node path "))
        .unwrap();
    let allowed: Vec<_> = paths.split_whitespace().skip(2).collect();
    assert!(allowed.contains(&"/api/recent"));
    for private in ["/app", "/api/control", "/api/recent/*", "/api/*"] {
        assert!(!allowed.contains(&private), "{private}");
    }
}

#[tokio::test]
async fn a_local_site_put_first_is_opened_from_its_link() {
    let node = Arc::new(SharingNode {
        status: node_status(Phase::Ready, Step::Idle),
        picks: Mutex::new(Vec::new()),
    });
    let app = || node_router(Arc::new(BreweryPlaces), node.clone());
    let (_, _, page) = send(app(), "/search?q=brewery+in+denver").await;
    assert!(
        page.contains("/go?q=brewery+in+denver&amp;d=unknown-ales.example"),
        "{page}"
    );
    let (_, headers, _) = send(app(), "/go?q=brewery+in+denver&d=unknown-ales.example").await;
    let location = headers.get("location").unwrap().to_str().unwrap();
    assert!(location.contains("unknown-ales.example"), "{location}");
    // Not a site of the places, nor of the results: back to them.
    let (_, headers, _) = send(app(), "/go?q=brewery+in+denver&d=evil.example").await;
    let location = headers.get("location").unwrap().to_str().unwrap();
    assert!(location.starts_with("/search?"), "{location}");
    // A brewery's page on a site the results also list is listed under
    // the brewery's name and opened, not the site's homepage.
    let query = "brewery+in+denver+westword";
    let (_, _, page) = send(app(), &format!("/search?q={query}")).await;
    let row = &page[page
        .find("<span class=\"u\">www.westword.com/bars/great-divide")
        .unwrap_or_else(|| panic!("{page}"))..];
    assert!(
        row[..row.find("</a>").unwrap()].contains("Great Divide"),
        "{row}"
    );
    let (_, headers, _) = send(app(), &format!("/go?q={query}&d=westword.com")).await;
    assert_eq!(
        headers["location"],
        "https://www.westword.com/bars/great-divide"
    );
}

/// Finds more the further it looks, and ranks some of it above what it
/// found before.
struct Deeper;

impl SearchBackend for Deeper {
    fn search(&self, _: &str, limit: usize) -> Result<Vec<Hit>> {
        let found: &[&str] = if limit <= DEFAULT_LIMIT {
            &["a.com", "b.com"]
        } else {
            &["a.com", "x.com", "b.com", "c.com"]
        };
        Ok(found.iter().map(|domain| scored(domain, 0.5)).collect())
    }

    fn num_docs(&self) -> u64 {
        4
    }
}

#[tokio::test]
async fn more_results_keeps_the_page_before_on_top() {
    let app = router_with(Arc::new(Deeper), HomeCountry::Off);
    let (_, _, first) = send(app.clone(), "/search?q=abc").await;
    // It lands on the first site the page does not have.
    assert!(
        first.contains("&amp;limit=20#r3\">More results</a>"),
        "{first}"
    );
    assert!(!first.contains("id=\"r"), "{first}");
    let (_, _, more) = send(app, "/search?q=abc&limit=20").await;
    let at = |domain: &str| more.find(&format!("<span class=\"dn\">{domain}</span>"));
    let order: Vec<_> = ["a.com", "b.com", "x.com", "c.com"].map(at).into();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{more}");
    assert!(more.contains("<li id=\"r3\">"), "{more}");
    // Four sites of twenty asked for: there are no more.
    assert!(
        more.contains("<p class=\"s\" id=\"r5\">No more results.</p>"),
        "{more}"
    );
    assert!(!more.contains("More results</a>"), "{more}");
}

#[test]
fn more_results_goes_on_while_pages_fill_up_to_the_most_listed() {
    let sites = |n: usize| SearchResults {
        hits: (0..n).map(|i| scored(&format!("s{i}.com"), 0.5)).collect(),
        ..SearchResults::default()
    };
    let render = |results: &SearchResults, limit: usize| {
        render_results(
            "q",
            results,
            None,
            &NetOutcome::NotAsked,
            &no_settings(),
            None,
            limit,
            false,
            &Icons::default(),
        )
    };
    let page = render(&sites(15), 20);
    assert!(
        page.contains("&amp;limit=30#r16\">More results</a>"),
        "{page}"
    );
    assert!(page.contains("<li id=\"r15\">"), "{page}");
    let page = render(&sites(MAX_LIMIT), MAX_LIMIT);
    assert!(
        !page.contains("More results") && !page.contains("No more"),
        "{page}"
    );
    assert!(!render(&sites(0), DEFAULT_LIMIT).contains("More results"));
}
