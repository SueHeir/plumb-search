//! Subpages of a site and packages: listed only when asked for.

use super::*;

#[test]
fn subpages_are_found_like_reference_pages() {
    let page = |url: &str, title: &str, description: &str| {
        Page::from_set(
            SUBPAGES_SET,
            Article {
                title: title.into(),
                description: Some(description.into()),
                item: Some(url.into()),
                views: 2_000,
                ..Article::default()
            },
        )
        .unwrap()
    };
    let perft = page(
        "https://chessprogramming.org/Perft_Results",
        "Perft Results",
        "Perft results of the initial position and Kiwipete.",
    );
    let sp811 = page(
        "https://www.nist.gov/pml/special-publication-811",
        "Special Publication 811",
        "NIST Guide to the SI, with conversion factors.",
    );
    assert!(Page::has_reader(SUBPAGES_SET));
    assert!(perft.is_site_page());
    assert_eq!(perft.set_domain(), "chessprogramming.org");
    assert_eq!(sp811.set_name(), "nist.gov");
    assert!(Page::from_set(SUBPAGES_SET, Article::default()).is_none());
    let (_dir, searcher) = searcher(&[perft.clone(), sp811.clone()]);
    let found = |query: &str| -> Vec<(String, bool)> {
        searcher
            .search(query, 10)
            .unwrap()
            .into_iter()
            .map(|hit| (hit.page.title, hit.named))
            .collect()
    };
    // Named by its whole title, or found by most words.
    assert_eq!(found("perft results"), [(perft.title.clone(), true)]);
    assert_eq!(
        found("nist special publication 811"),
        [(sp811.title.clone(), false)]
    );
    // Part of a title names nothing.
    assert!(found("perft").is_empty());
}

#[test]
fn subpages_named_like_an_article_come_after_it() {
    let subpage = |url: &str, title: &str, views: u64| {
        Page::from_set(
            SUBPAGES_SET,
            Article {
                title: title.into(),
                item: Some(url.into()),
                views,
                ..Article::default()
            },
        )
        .unwrap()
    };
    let article = |title: &str, views: u64, site: Option<&str>| {
        let mut article = page(title, views, &[]);
        article.item = Some(format!("Q{views}"));
        article.site = site.map(str::to_string);
        article
    };
    let (_dir, searcher) = searcher(&[
        article("United States", 10_000_000, None),
        article("Napoleon", 30_000, None),
        article("Yosemite National Park", 20_000, Some("nps.gov")),
        subpage(
            "https://www.nps.gov/yose/",
            "Yosemite National Park",
            10_000,
        ),
        subpage("https://chessprogramming.org/Napoleon", "Napoleon", 4_000),
    ]);
    let first = |query: &str| {
        let hits = searcher.search(query, 10).unwrap();
        assert_eq!(hits.len(), 2, "{query}: {hits:?}");
        hits[0].page.url.clone()
    };
    // Read by the site's weight, the chess engine would come first.
    assert_eq!(first("napoleon"), "https://en.wikipedia.org/wiki/Napoleon");
    // Under a site the query names, the one page listed is the article.
    let hits = searcher.search("napoleon", 10).unwrap();
    let placed = place_pages("napoleon", &[site("napoleon.org", true)], hits);
    let listed: Vec<&str> = placed.iter().map(|p| p.hit.page.set.as_str()).collect();
    assert_eq!(listed, ["wikipedia-en"]);
    // The park's own page stays first.
    assert_eq!(first("yosemite national park"), "https://www.nps.gov/yose/");
}

#[test]
fn declared_subpage_language_survives_retrieval_and_placement() {
    for (language, title, query) in [
        ("es", "Receta fácil de galleta", "recetas fáciles galletas"),
        (
            "de",
            "Gesunde Ernährung und Bewegung",
            "gesunden ernährung bewegungen",
        ),
    ] {
        // Exercise the URL heuristic and the title-word gate separately.
        for path in [language, "guide"] {
            let page = Page::from_set(
                SUBPAGES_SET,
                Article {
                    title: title.into(),
                    item: Some(format!("https://reference.example/{path}/article")),
                    language: Some(language.into()),
                    views: 2_000,
                    ..Article::default()
                },
            )
            .unwrap();
            let mut unknown = page.clone();
            unknown.content_language = None;
            unknown.url.push_str("-unknown");
            let options = crate::SearchOptions {
                language: Some(language.into()),
                ..crate::SearchOptions::default()
            };
            assert_eq!(unknown.language(), None);
            assert!(!options_allow(&options, &unknown));
            let (_dir, searcher) = searcher(&[page.clone(), unknown]);
            let hits = searcher
                .in_language(options.language.as_deref())
                .search(query, 5)
                .unwrap();
            assert_eq!(hits.len(), 1, "{language} /{path}: {hits:?}");
            assert!(
                !hits[0].named,
                "must exercise the title-word placement gate"
            );
            assert!(options_allow(&options, &hits[0].page));
            let placed = place_pages(query, &[], hits);
            assert_eq!(placed.len(), 1, "{language} /{path}");
            assert_eq!(placed[0].hit.page.url, page.url);
        }
    }
}

#[test]
fn tag_archives_are_listed_only_on_their_site() {
    let hit = |url: &str, title: &str| PageHit {
        page: Page::from_set(
            SUBPAGES_SET,
            Article {
                title: title.into(),
                description: Some("Explore Johnson & Johnson's lupus resources.".into()),
                item: Some(url.into()),
                views: 2_000,
                ..Article::default()
            },
        )
        .unwrap(),
        score: 0.9,
        named: true,
        popularity: 0.9,
        whole: false,
        learned: None,
    };
    let lupus = hit("https://www.jnj.com/tag/lupus", "Lupus");
    let sites =
        |domains: &[&str]| -> Vec<crate::Hit> { domains.iter().map(|d| site(d, false)).collect() };
    // Named by its title, it says nothing of lupus.
    assert!(!subpage_asked("lupus", &sites(&["lupus.org"]), &lupus));
    assert!(!subpage_asked("lupus", &[], &lupus));
    // Unless the search is on its site.
    assert!(subpage_asked(
        "johnson and johnson lupus",
        &sites(&["jnj.com"]),
        &lupus
    ));
    // A page about the subject, or a tag in a page's name, is no archive.
    for url in [
        "https://www.who.int/health-topics/hypertension",
        "https://www.jnj.com/lupus/tag",
        "https://example.com/tag",
        "https://example.com/products/name-tag",
    ] {
        assert!(subpage_asked("lupus", &[], &hit(url, "Lupus")), "{url}");
    }
}

#[test]
fn subpages_are_listed_only_when_asked_for() {
    let hit = |url: &str, title: &str, named: bool| PageHit {
        page: Page::from_set(
            SUBPAGES_SET,
            Article {
                title: title.into(),
                item: Some(url.into()),
                views: 2_000,
                ..Article::default()
            },
        )
        .unwrap(),
        score: 0.9,
        named,
        popularity: 0.5,
        whole: false,
        learned: None,
    };
    let listed = |query: &str, sites: &[&str], hit: PageHit| {
        let sites: Vec<crate::Hit> = sites.iter().map(|d| site(d, false)).collect();
        !place_pages(query, &sites, vec![hit]).is_empty()
    };
    let summit = || {
        hit(
            "https://www.jnj.com/latest-news/womens-leadership-summit",
            "Women's Leadership Summit brings seven teams together",
            false,
        )
    };
    // Found by its words on a site the query does not name.
    assert!(!listed(
        "seven summits",
        &["7summits.com", "wikipedia.org"],
        summit()
    ));
    // On one of the query's best sites.
    assert!(listed("jnj seven summit", &["jnj.com"], summit()));
    let everest = || {
        hit(
            "https://www.ign.com/articles/everest-review",
            "Everest Review",
            false,
        )
    };
    assert!(!listed("mount everest", &["everest.com"], everest()));
    // Two of three words is not enough, and the "s" of "John's" is
    // not a word.
    let gatsby = || {
        hit(
            "https://www.broadway.org/shows/gatsby",
            "The Great Gatsby - On Broadway",
            false,
        )
    };
    assert!(!listed(
        "the great gatsby f scott fitzgerald",
        &["gatsbyjs.com"],
        gatsby()
    ));
    let health = || {
        hit(
            "https://www.oracle.com/customers/st-johns-health/",
            "St. John's Health",
            false,
        )
    };
    assert!(!listed("time in st john's", &["stjohns.edu"], health()));
    // Not in another language, even on a best site.
    let codes = |path: &str| {
        hit(
            &format!("https://www.iso.org/{path}iso-4217-currency-codes.html"),
            "ISO 4217 Currency codes",
            false,
        )
    };
    assert!(!listed("iso 4217", &["iso.org"], codes("ru/")));
    assert!(!listed("iso 4217", &["iso.org"], codes("fr-fr/")));
    assert!(listed("iso 4217", &["iso.org"], codes("")));
    assert!(listed("iso 4217", &["iso.org"], codes("en/")));
    // Most of its title's words.
    let principles = || {
        hit(
            "https://www.aboutamazon.com/about-us/leadership-principles",
            "Leadership Principles",
            false,
        )
    };
    assert!(listed(
        "amazon leadership principles",
        &["amazon.com"],
        principles()
    ));
    let fact_sheet = || {
        hit(
            "https://nssdc.gsfc.nasa.gov/planetary/factsheet/",
            "Planetary Fact Sheet - Metric",
            false,
        )
    };
    assert!(listed(
        "planetary fact sheet",
        &["planetary.org"],
        fact_sheet()
    ));
    // Or of its title before a subtitle, not before a dash.
    let culture = || {
        hit(
            "https://www.aboutamazon.com/about-us/leadership-principles",
            "Amazon Leadership Principles: Values and company culture",
            false,
        )
    };
    assert!(listed(
        "amazon leadership principles",
        &["amazon.jobs"],
        culture()
    ));
    let transcript = || {
        hit(
            "https://www.archives.gov/founding-docs/declaration-transcript",
            "Declaration of Independence: A Transcription",
            false,
        )
    };
    assert!(listed(
        "declaration of independence transcript",
        &["wikipedia.org"],
        transcript()
    ));
    // The site's name after " -- " is left out.
    let rule30 = || {
        hit(
            "https://mathworld.wolfram.com/Rule30.html",
            "Rule 30 -- from Wolfram MathWorld",
            false,
        )
    };
    assert!(listed(
        "rule 30 cellular automaton",
        &["automattic.com"],
        rule30()
    ));
    // A query the title holds whole needs only half of the title.
    let martinis = || {
        hit(
            "https://computerhistory.org/profile/john-martinis/",
            "John Martinis - CHM",
            false,
        )
    };
    assert!(listed("john martinis", &["martinis.com"], martinis()));
    let climate = || {
        hit(
            "https://www.who.int/news/item/climate-change-and-health-statement",
            "WHO statement on climate change and health at the summit",
            false,
        )
    };
    assert!(!listed("climate change", &["climate.gov"], climate()));
    // Not for a year its title does not have.
    let nobel = || {
        hit(
            "https://home.cern/nobel-prize-particle-physics/",
            "A Nobel prize for particle physics",
            false,
        )
    };
    assert!(!listed(
        "nobel prize in physics 2025",
        &["nobelprize.org"],
        nobel()
    ));
    assert!(listed(
        "nobel prize particle physics",
        &["nobelprize.org"],
        nobel()
    ));
    // Named by its whole title, or on a subdomain of a best site.
    assert!(listed(
        "perft results",
        &[],
        hit(
            "https://chessprogramming.org/Perft_Results",
            "Perft Results",
            true
        )
    ));
    assert!(listed(
        "codata constants",
        &["nist.gov"],
        hit(
            "https://physics.nist.gov/cuu/Constants/",
            "Fundamental Physical Constants",
            false
        )
    ));
}

#[test]
fn packages_are_found_only_when_asked_for() {
    let (_dir, searcher) = searcher(&[
        page("Serde", 5_000, &[]),
        page("Requests", 900, &[]),
        package("crates", "serde", 1_000_000_000),
        package("crates", "book", 20),
        package("pypi", "requests", 1_000_000_000),
        package("npm", "requests", 2_000),
        package("go", "github.com/gin-gonic/gin", 1_000_000_000),
    ]);
    let urls = |query: &str| -> Vec<String> {
        searcher
            .search(query, 10)
            .unwrap()
            .into_iter()
            .filter(|hit| hit.page.package.is_some())
            .map(|hit| hit.page.url)
            .collect()
    };
    assert!(urls("serde").is_empty());
    assert!(urls("requests").is_empty());
    assert_eq!(urls("serde crate"), ["https://crates.io/crates/serde"]);
    assert_eq!(
        urls("latest version of requests python"),
        ["https://pypi.org/project/requests/"]
    );
    assert_eq!(
        urls("requests npm"),
        ["https://www.npmjs.com/package/requests"]
    );
    // Either registry's, the most used first.
    assert_eq!(
        urls("requests package"),
        [
            "https://pypi.org/project/requests/",
            "https://www.npmjs.com/package/requests"
        ]
    );
    // A language alone asks only for well-known packages.
    assert!(urls("rust book").is_empty());
    assert_eq!(urls("book crate"), ["https://crates.io/crates/book"]);
    assert_eq!(
        urls("gin golang"),
        ["https://pkg.go.dev/github.com/gin-gonic/gin"]
    );
    let hit = searcher.search("serde crate", 10).unwrap().remove(0);
    assert!(hit.named && hit.page.set_name() == "crates.io");
}
