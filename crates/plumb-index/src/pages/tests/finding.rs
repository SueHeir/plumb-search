//! Finding articles: whole titles, plurals, languages, operators, questions,
//! namesakes and spelling.

use super::*;

#[test]
fn plurals_name_the_article_titled_in_the_singular() {
    let (_dir, s) = searcher(&[
        page("Tariff", 90_000, &[]),
        page("No Kings protests", 50_000, &[]),
        page("Kings", 1_000, &[]),
    ]);
    let found = |query: &str, sites: &[crate::Hit]| {
        let mut hits = s.search(query, 5).unwrap();
        s.add_other_number(query, sites, &mut hits, 5).unwrap();
        hits
    };
    let tariffs_net = known_site("tariffs.net", true, 0.015);
    let hits = found("tariffs", std::slice::from_ref(&tariffs_net));
    assert_eq!(titles(&hits)[0], "Tariff");
    assert!(hits[0].named);
    let hits = found("no kings protest", &[]);
    assert_eq!(titles(&hits)[0], "No Kings protests");
    assert!(hits[0].named);
    // Named as typed: no other number is tried.
    let hits = found("kings", &[]);
    assert_eq!(titles(&hits)[0], "Kings");
    // A site that answers the query well keeps it to the sites.
    let mut tariff_site = known_site("tariffs.gov", false, 0.6);
    tariff_site.text_score = 0.9;
    assert!(found("tariffs", &[tariff_site])
        .iter()
        .all(|hit| !hit.named));
    let mut official = tariffs_net;
    official.official = true;
    assert!(found("tariffs", &[official]).iter().all(|hit| !hit.named));
    assert_eq!(last_word_in_other_number("news"), None);
    assert_eq!(last_word_in_other_number("tariffs.net"), None);
}

#[test]
fn a_title_with_a_possessive_is_named_without_it() {
    let (_dir, s) = searcher(&[
        page("Ohm's law", 7_111, &[]),
        page("Ohm's acoustic law", 81, &[]),
        page("Halley's Comet", 13_861, &[]),
        page("Maslow's hierarchy of needs", 19_592, &[]),
        page("McDonald's", 50_000, &[]),
    ]);
    let named = |q: &str| -> Vec<String> {
        s.search(q, 5)
            .unwrap()
            .into_iter()
            .filter(|hit| hit.named)
            .map(|hit| hit.page.title)
            .collect()
    };
    assert_eq!(named("ohm law"), ["Ohm's law"]);
    assert_eq!(named("halley comet"), ["Halley's Comet"]);
    assert_eq!(
        named("maslow hierarchy of needs"),
        ["Maslow's hierarchy of needs"]
    );
    assert_eq!(named("ohm's law"), ["Ohm's law"]);
    // A name is not a title that ends in it: "mcdonald" is a surname too.
    assert!(named("mcdonald").is_empty());
    assert!(named("ohm").is_empty());
}

#[test]
fn whole_titles_win_then_popularity() {
    let (_dir, s) = searcher(&[
        page("Marie Curie", 80_000, &["Madame Curie"]),
        page("Pierre Curie", 30_000, &[]),
        page("Curie (unit)", 2_000, &[]),
        page("Python (programming language)", 900_000, &[]),
        page("Python (genus)", 20_000, &["Pythonidae"]),
        page("Monty Python", 200_000, &[]),
    ]);
    let hits = s.search("Marie Curie", 5).unwrap();
    assert_eq!(titles(&hits)[0], "Marie Curie");
    assert!(hits[0].named);
    let hits = s.search("python", 5).unwrap();
    assert_eq!(
        titles(&hits)[..3],
        [
            "Python (programming language)",
            "Python (genus)",
            "Monty Python"
        ]
    );
    assert!(!hits[2].named);
    let hits = s.search("madame curie", 5).unwrap();
    assert_eq!(titles(&hits)[0], "Marie Curie");
    assert!(hits[0].named);
    assert!(s.search("pythonidae", 1).unwrap()[0].named);
}

#[test]
fn language_is_filtered_before_popular_candidates_fill_the_cap() {
    let mut pages: Vec<Page> = (0..CANDIDATES + 25)
        .map(|i| {
            let mut page = page("Language control", 10_000 - i as u64, &[]);
            page.url = format!("https://en.wikipedia.org/wiki/Control_{i}");
            page
        })
        .collect();
    let mut spanish = page("Language control", 1, &[]);
    spanish.set = "wikipedia-es".into();
    spanish.url = "https://es.wikipedia.org/wiki/Control".into();
    pages.push(spanish.clone());
    let mut german = spanish.clone();
    german.set = "wikipedia-de".into();
    german.url = "https://de.wikipedia.org/wiki/Control".into();
    pages.push(german.clone());
    let (_dir, searcher) = searcher(&pages);
    for (lang, expected) in [("es-MX", spanish), ("de", german)] {
        let hits = searcher
            .in_language(Some(lang))
            .search("language control", 1)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].page.url, expected.url);
    }
    assert!(searcher
        .in_language(Some("fr"))
        .search("language control", 1)
        .unwrap()
        .is_empty());
    assert!(searcher
        .in_language(Some("bogus"))
        .search("language control", 1)
        .unwrap()
        .is_empty());
    let (known, unknown) = searcher.language_coverage();
    assert_eq!(known.get("es"), Some(&1));
    assert_eq!(known.get("de"), Some(&1));
    assert_eq!(unknown, 0);
}

#[test]
fn declared_reference_language_beats_host_country_and_unknown_is_reported() {
    let spanish = Page::from_reference(Article {
        title: "Receta fácil de galleta".into(),
        item: Some("https://recipes.example.de/galletas".into()),
        language: Some("es".into()),
        ..Article::default()
    })
    .unwrap();
    let german = Page::from_reference(Article {
        title: "Gesunde Ernährung und Bewegung".into(),
        item: Some("https://gesund.bund.de/ernaehrung".into()),
        language: Some("de-DE".into()),
        ..Article::default()
    })
    .unwrap();
    let unknown = Page::from_reference(Article {
        title: "Receta fácil de galleta".into(),
        item: Some("https://unknown.example.es/recipe".into()),
        ..Article::default()
    })
    .unwrap();
    assert_eq!(spanish.language(), Some("es"));
    assert_eq!(german.language(), Some("de"));
    assert_eq!(unknown.language(), None);
    let (_dir, searcher) = searcher(&[spanish.clone(), german.clone(), unknown]);
    let es = searcher
        .in_language(Some("es"))
        .search("recetas fáciles galletas", 5)
        .unwrap();
    assert!(es.iter().any(|hit| hit.page.url == spanish.url), "{es:?}");
    let de = searcher
        .in_language(Some("de"))
        .search("gesunde ernährung bewegung", 5)
        .unwrap();
    assert!(de.iter().any(|hit| hit.page.url == german.url), "{de:?}");
    assert_eq!(searcher.language_coverage().1, 1);
}

#[test]
fn spanish_dni_appointment_filter_never_returns_english_us_intelligence() {
    let spanish = Page::from_reference(Article {
        title: "Cita previa DNI".into(),
        item: Some("https://www.dnielectronico.es/PortalDNIe/ciudadanos".into()),
        language: Some("es".into()),
        ..Article::default()
    })
    .unwrap();
    let english = Page::from_reference(Article {
        title: "Cita previa DNI".into(),
        item: Some("https://www.dni.gov/".into()),
        views: 100_000,
        language: Some("en".into()),
        ..Article::default()
    })
    .unwrap();
    let (_dir, searcher) = searcher(&[english, spanish.clone()]);
    let found = searcher
        .in_language(Some("es"))
        .search("cita previa dni", 5)
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].page.url, spanish.url);
}

#[test]
fn pages_follow_the_language_and_strict_safe_search() {
    let article = page("Einstein", 1, &[]);
    let mut book = page("Sexy beasts", 1, &[]);
    book.set = BOOKS_SET.into();
    let en = crate::SearchOptions {
        language: Some("en".into()),
        ..crate::SearchOptions::default()
    };
    assert!(options_allow(&en, &article));
    assert!(
        options_allow(&en, &book),
        "old English-filter behavior keeps identifiable unknown-language records"
    );
    let de = crate::SearchOptions {
        language: Some("de".into()),
        ..crate::SearchOptions::default()
    };
    assert!(!options_allow(&de, &article));
    assert!(options_allow(&crate::SearchOptions::default(), &book));
    let strict = crate::SearchOptions {
        safe: SafeSearch::Strict,
        ..crate::SearchOptions::default()
    };
    assert!(!options_allow(&strict, &book));
    assert!(options_allow(&strict, &article));
}

#[test]
fn operators_pick_and_list_pages() {
    let mut repo = Page::from_repo(Article {
        title: "python/cpython".into(),
        views: 60_000,
        ..Article::default()
    });
    repo.description = Some("The Python programming language".into());
    let (_dir, s) = searcher(&[
        page("Python (programming language)", 900_000, &[]),
        page("Python (genus)", 20_000, &["Pythonidae"]),
        page("Monty Python", 200_000, &[]),
        repo,
    ]);
    let place = |query: &str| {
        let ops = Operators::parse(query);
        let found = s.search(&ops.words, OPERATOR_PAGES).unwrap();
        let placed = place_operator_pages(&ops, &[site("python.org", false)], found);
        placed
            .into_iter()
            .map(|p| (p.hit.page.title, p.at))
            .collect::<Vec<_>>()
    };
    let on_github = place("python site:github.com");
    assert_eq!(on_github, [("python/cpython".to_string(), 1)]);
    let on_wikipedia = place("python site:en.wikipedia.org");
    assert_eq!(on_wikipedia.len(), 3);
    assert!(on_wikipedia.iter().all(|(_, at)| *at == 1));
    let without = place("python -monty -site:github.com");
    assert!(without
        .iter()
        .all(|(t, _)| t != "Monty Python" && t != "python/cpython"));
}

#[test]
fn questions_without_the_word_asked_about_are_not_found() {
    let question = |title: &str, tags: &str, item: &str| {
        Page::from_question(Article {
            title: title.into(),
            description: Some(tags.into()),
            item: Some(item.into()),
            views: 100_000,
            ..Article::default()
        })
    };
    let pages = [
        question("How do I get rid of my bounty?", "bounty, meta", "1"),
        question(
            "How to get rid of large gaps in text",
            "ms-word, layout",
            "2",
        ),
        question("How do I get rid of aphids on roses?", "pests, roses", "3"),
    ];
    let (_dir, s) = searcher(&pages);
    let hits = s.search("how to get rid of aphids", 5).unwrap();
    assert_eq!(titles(&hits), ["How do I get rid of aphids on roses?"]);
}

#[test]
fn asking_words_count_half_toward_a_question() {
    let question = |title: &str, tags: &str, item: &str| {
        Page::from_question(Article {
            title: title.into(),
            description: Some(tags.into()),
            item: Some(item.into()),
            views: 100_000,
            ..Article::default()
        })
    };
    let pages = [
        question(
            "How many amps should my car pull while it is turned off?",
            "battery, relay, power",
            "1",
        ),
        question(
            "How long does cooked pasta last in the fridge?",
            "pasta, storage-lifetime, refrigerator",
            "2",
        ),
        question(
            "How do I get my toddler to sleep by himself?",
            "sleep, toddler",
            "3",
        ),
        question("Is it safe to drive through the night?", "road-trips", "4"),
        question(
            "Can I eat cheese which has been infected with blue cheese mold?",
            "cheese, food-safety",
            "5",
        ),
        question(
            "Is it healthy for a cat to regularly eat cheese?",
            "cats, diet",
            "6",
        ),
        question("Why does my cat knead?", "cats, behavior", "7"),
        question("How do I stop my cat scratching furniture?", "cats", "8"),
    ];
    // Names that make the query's other words known, not typos.
    let known = [
        "Climate change",
        "Social change",
        "Change management",
        "Social work",
        "Work ethic",
        "Work (physics)",
        "Twelfth Night",
        "Night owl",
        "Through the Night",
        "Through the Looking-Glass",
        "Through the Wall",
        "Get Out",
        "Get Smart",
        "My Fair Lady",
        "I, Robot",
        "I Love Lucy",
    ]
    .map(|title| page(title, 1_000, &[]));
    let (_dir, s) = searcher(&[&pages[..], &known[..]].concat());
    let found = |query: &str| -> Vec<String> {
        s.search(query, 5)
            .unwrap()
            .into_iter()
            .filter(|hit| hit.page.is_question())
            .map(|hit| hit.page.title)
            .collect()
    };
    // Only the asking words and one or two others.
    assert!(found("how to change a car battery").is_empty());
    assert!(found("how does a refrigerator work").is_empty());
    assert_eq!(
        found("can cats eat cheese"),
        ["Is it healthy for a cat to regularly eat cheese?"]
    );
    // Most of what the query asks about.
    assert_eq!(
        found("how do i get my toddler to sleep through the night"),
        ["How do I get my toddler to sleep by himself?"]
    );
}

#[test]
fn no_question_is_found_for_a_known_word_none_asks_about() {
    let question = |title: &str, tags: &str, item: &str| {
        Page::from_question(Article {
            title: title.into(),
            description: Some(tags.into()),
            item: Some(item.into()),
            views: 100_000,
            ..Article::default()
        })
    };
    let pages = [
        question("How do I get rid of my bounty?", "bounty, meta", "1"),
        question(
            "How to get rid of large gaps in text",
            "ms-word, layout",
            "2",
        ),
        question(
            "How do I undo the most recent local commits in Git?",
            "git, git-commit",
            "3",
        ),
        page("Aphid", 50_000, &[]),
        page("Green peach aphid", 2_000, &[]),
        page("Black bean aphid", 1_000, &[]),
    ];
    let (_dir, s) = searcher(&pages);
    let questions = |query: &str| -> Vec<String> {
        s.search(query, 5)
            .unwrap()
            .into_iter()
            .filter(|hit| hit.page.is_question())
            .map(|hit| hit.page.title)
            .collect()
    };
    // No question has "aphids", which pages name: none is about them.
    assert!(questions("how to get rid of aphids").is_empty());
    // "comit" no page knows, so it may be a typo: the question with
    // the other words is still found.
    assert_eq!(
        questions("how to undo git comit"),
        ["How do I undo the most recent local commits in Git?"]
    );
}

#[test]
fn questions_are_found_in_their_duplicates_words() {
    let undo = Page::from_question(Article {
        title: "How do I undo the most recent local commits in Git?".into(),
        description: Some("git, version-control, git-commit, undo".into()),
        item: Some("927358".into()),
        views: 14_000_000,
        aliases: vec!["Revert to a previous Git commit without losing history".into()],
        ..Article::default()
    });
    let (_dir, s) = searcher(&[undo]);
    let hits = s
        .search("revert previous commit losing history", 5)
        .unwrap();
    assert_eq!(
        titles(&hits),
        ["How do I undo the most recent local commits in Git?"]
    );
    assert!(hits[0].whole);
}

#[test]
fn a_title_and_a_name_name_the_living_holder_over_past_ones() {
    use plumb_core::facts::{Fact, FactKind};
    let holder = |title: &str, views: u64, description: &str, died: Option<&str>| {
        let mut holder = page(title, views, &[]);
        holder.description = Some(description.into());
        holder.facts = std::iter::once((FactKind::Born, "1600"))
            .chain(died.map(|year| (FactKind::Died, year)))
            .map(|(kind, value)| Fact {
                kind,
                value: value.into(),
            })
            .collect();
        holder
    };
    let (_dir, s) = searcher(&[
        holder(
            "Charles III",
            110_755,
            "King of the United Kingdom since 2022",
            None,
        ),
        holder(
            "Charles I of England",
            36_925,
            "King of England, Scotland and Ireland from 1625 to 1649",
            Some("1649"),
        ),
        holder(
            "Charles II of England",
            34_747,
            "King of England, Scotland, and Ireland from 1660 to 1685",
            Some("1685"),
        ),
        page("King Charles (musician)", 124, &[]),
        holder(
            "Elizabeth II",
            124_534,
            "Queen of the United Kingdom from 1952 to 2022",
            Some("2022"),
        ),
        holder(
            "Elizabeth I",
            66_242,
            "Queen of England and Ireland from 1558 to 1603",
            Some("1603"),
        ),
    ]);
    let named = |q: &str| -> Vec<String> {
        let hits = s.search(q, 5).unwrap();
        hits.into_iter()
            .filter(|hit| hit.named)
            .map(|hit| hit.page.title)
            .collect()
    };
    assert_eq!(named("king charles")[0], "Charles III");
    // Two queens of the name, neither living.
    assert!(!named("queen elizabeth")
        .iter()
        .any(|title| title.starts_with("Elizabeth")));
}

#[test]
fn whole_questions_lead() {
    let question = Page::from_question(Article {
        title: "How do I delete a Git branch locally and remotely?".into(),
        description: Some("git, version-control, git-branch".into()),
        item: Some("2003505".into()),
        views: 12_000_000,
        ..Article::default()
    });
    let (_dir, s) = searcher(&[question]);
    let sites = [known_site("git-scm.com", false, 0.9)];
    let hits = s
        .search("delete a git branch locally and remotely", 5)
        .unwrap();
    assert!(hits[0].whole);
    assert_eq!(
        place_pages("delete a git branch locally and remotely", &sites, hits)[0].at,
        0
    );
    // Most of the query's words, asked as a question naming no site:
    // the question still leads, ahead of a site that only shares a
    // word with it.
    let asked = "how to delete git branch remotely fast";
    let hits = s.search(asked, 5).unwrap();
    assert!(!hits[0].whole);
    assert_eq!(place_pages(asked, &sites, hits)[0].at, 0);
    // Not asked as a question: after the best site.
    let hits = s.search("delete git branch remotely fast", 5).unwrap();
    assert_eq!(
        place_pages("delete git branch remotely fast", &sites, hits)[0].at,
        1
    );
    // A site the question names stays first.
    let named = [known_site("git-scm.com", true, 0.9)];
    let hits = s.search(asked, 5).unwrap();
    assert_eq!(place_pages(asked, &named, hits)[0].at, 1);
}

#[test]
fn sites_called_like_organizations_stay_first() {
    let mut bank = page("U.S. Bancorp", 900_000, &["US Bank"]);
    bank.description = Some("American multinational banking institution".into());
    let repo = Page::from_repo(Article {
        title: "firasdib/Regex101".into(),
        views: 5_000,
        aliases: vec!["Regex101".into()],
        ..Article::default()
    });
    let (_dir, s) = searcher(&[bank, repo, page("Sonnet", 300_000, &[])]);
    for (query, site) in [("us bank", "usbank.com"), ("regex101", "regex101.com")] {
        let hits = s.search(query, 5).unwrap();
        assert!(hits[0].named, "{query}");
        let placed = place_pages(query, &[known_site(site, false, 0.0)], hits);
        assert_eq!(placed[0].at, 1, "{query}");
    }
    // A site called like a person or a work is not first for it.
    let mut curie = page("Marie Curie", 900_000, &[]);
    curie.description = Some("Polish-French physicist and chemist".into());
    let placed = place_pages(
        "marie curie",
        &[known_site("mariecurie.org", false, 0.0)],
        vec![PageHit {
            page: curie,
            score: 1.0,
            named: true,
            popularity: 0.9,
            whole: false,
            learned: None,
        }],
    );
    assert_eq!(placed[0].at, 0);
}

#[test]
fn namesakes_are_weighed_by_how_much_their_articles_are_read() {
    let mut planet = page("Mars", 2_000_000, &[]);
    planet.description = Some("Fourth planet from the Sun".into());
    let mut company = page("Mars Inc.", 20_000, &["Mars, Incorporated"]);
    company.description = Some("American manufacturer of confectionery".into());
    company.site = Some("mars.com".into());
    let mut repo = Page::from_repo(Article {
        title: "mars/mars".into(),
        views: 9_000_000,
        ..Article::default()
    });
    repo.site = Some("mars.com".into());
    let (_dir, s) = searcher(&[planet, company, repo]);
    // Only articles say what a site is read about.
    assert!((s.site_popularity("mars.com").unwrap().unwrap() - 0.68).abs() < 0.01);
    assert_eq!(s.site_popularity("planet.example").unwrap(), None);

    let mut sites = [known_site("mars.com", true, 0.65)];
    sites[0].official = true;
    let place = |sites: &[crate::Hit]| {
        let hits = s.search("mars", 5).unwrap();
        assert_eq!(hits[0].page.title, "Mars");
        place_pages("mars", sites, hits)
            .into_iter()
            .find(|p| p.hit.page.title == "Mars")
            .unwrap()
            .at
    };
    // An official site was never put after an article.
    assert_eq!(place(&sites), 1);
    // The planet is read a hundred times more than the company.
    s.note_demand(&mut sites).unwrap();
    assert!(sites[0].demand.is_some());
    assert_eq!(place(&sites), 0);
    // Not when the company is read about as much.
    sites[0].demand = Some(1.0);
    assert_eq!(place(&sites), 1);
    // The article Wikipedia calls "Mars" needs only to be read more.
    let planet = s.search("mars", 5).unwrap()[0].popularity;
    sites[0].demand = Some(planet - 0.02);
    assert_eq!(place(&sites), 0);
}

#[test]
fn articles_come_before_repositories_named_alike() {
    let repo = Page::from_repo(Article {
        title: "google-deepmind/sonnet".into(),
        views: 9_000,
        aliases: vec!["sonnet".into()],
        ..Article::default()
    });
    let (_dir, s) = searcher(&[repo, page("Sonnet", 30_000, &[])]);
    let hits = s.search("sonnet", 5).unwrap();
    assert_eq!(titles(&hits), ["Sonnet", "google-deepmind/sonnet"]);
}

#[test]
fn partial_names_score_lower() {
    let (_dir, s) = searcher(&[page("Marie Curie", 80_000, &[])]);
    let hit = &s.search("curie", 1).unwrap()[0];
    assert!(!hit.named);
    assert!(hit.score < 0.5, "{}", hit.score);
    assert!(s.search("marie antoinette", 1).unwrap().is_empty());
    assert!(s.search("", 1).unwrap().is_empty());
}
#[test]
fn a_name_inside_a_longer_search_finds_its_page() {
    let article = |title: &str, description: &str, views: u64, aliases: &[&str]| {
        let mut article = page(title, views, aliases);
        article.description = Some(description.into());
        article.item = Some("Q1".into());
        article
    };
    let asyncio = Page::from_docs(Article {
        title: "asyncio".into(),
        description: Some(
            "asyncio is a library to write concurrent code using the async/await syntax.".into(),
        ),
        item: Some("https://docs.python.org/3/library/asyncio.html".into()),
        views: 5_000,
        aliases: vec!["Python asyncio".into(), "asyncio Python".into()],
        ..Article::default()
    })
    .unwrap();
    let (_dir, s) = searcher(&[
        asyncio,
        article(
            "Upper Peninsula of Michigan",
            "northern of the two peninsulas of Michigan",
            60_000,
            &["Upper Peninsula"],
        ),
        article("Michigan", "state of the United States", 500_000, &[]),
        article("Ninja", "covert agent in feudal Japan", 300_000, &[]),
        article(
            "Terry Stafford",
            "American singer-songwriter (1941–1996)",
            1_001,
            &[],
        ),
        article("Stafford", "town in Staffordshire, England", 20_000, &[]),
        article(
            "Death",
            "permanent cessation of vital functions",
            200_000,
            &[],
        ),
    ]);
    let listed = |q: &str| -> Vec<String> {
        let hits = s.search(q, 10).unwrap();
        hits.into_iter()
            .filter(|hit| hit.named || hit.score >= MIN_PARTIAL_SCORE)
            .map(|hit| hit.page.title)
            .collect()
    };
    // A docs page by its product and title, an article by its title.
    assert_eq!(listed("python asyncio tutorial"), ["asyncio"]);
    assert_eq!(listed("terry stafford death"), ["Terry Stafford"]);
    assert_eq!(
        listed("upper peninsula michigan map")[0],
        "Upper Peninsula of Michigan"
    );
    // An article named by one word of two is no closer for the other.
    assert!(listed("ninja terms").len() <= 1);
}

#[test]
fn a_qualifier_the_search_says_counts_as_part_of_the_name() {
    let article = |title: &str, description: &str, views: u64, aliases: &[&str]| {
        let mut article = page(title, views, aliases);
        article.description = Some(description.into());
        article.item = Some(format!("Q{views}"));
        article
    };
    let (_dir, s) = searcher(&[
        article(
            "Mercury (element)",
            "Chemical element with atomic number 80",
            400_000,
            &["Hg", "quicksilver"],
        ),
        article(
            "Chemical element",
            "Chemical substance not composed of simpler ones",
            300_000,
            &["element"],
        ),
        article(
            "Mercury (planet)",
            "smallest and closest planet to the Sun",
            900_000,
            &["Mercury"],
        ),
        article("Earth", "planet", 5_000_000, &[]),
    ]);
    let first = |q: &str| s.search(q, 5).unwrap()[0].page.title.clone();
    // Its title and qualifier apart name the element more fully than the
    // two words in a row name Chemical element.
    assert_eq!(first("the chemical element mercury"), "Mercury (element)");
    assert_eq!(first("chemical element mercury"), "Mercury (element)");
    assert_eq!(first("mercury element"), "Mercury (element)");
}

#[test]
fn words_of_things_are_corrected_from_page_names() {
    let mut pages = Vec::new();
    for i in 0..25 {
        pages.push(page(&format!("Anubis statue {i}"), 10, &[]));
        pages.push(page(&format!("Budapest hotels {i}"), 10, &[]));
    }
    pages.push(page("Anubas (beetle)", 1, &[]));
    for i in 0..5 {
        pages.push(page(&format!("Perfi album {i}"), 10, &[]));
    }
    for i in 0..25 {
        pages.push(page(&format!("Erft river {i}"), 10, &[]));
    }
    let (_dir, searcher) = searcher(&pages);
    let nothing_known = |_: &str| false;
    let suggest = |query: &str| {
        searcher
            .suggest_spelling(query, None, &nothing_known)
            .unwrap()
            .map(|s| s.query)
    };
    // Even a word one page has is taken for a slip of a far commoner one.
    assert_eq!(suggest("anubas").as_deref(), Some("anubis"));
    assert_eq!(
        suggest("budafest hotels").as_deref(),
        Some("budapest hotels")
    );
    // Known words, short words and words with digits stay.
    assert_eq!(suggest("budapest"), None);
    assert_eq!(suggest("anub"), None);
    assert_eq!(suggest("budafest2"), None);
    // A web address is a name, not words.
    assert_eq!(suggest("budafest.com"), None);
    // A rare word is no slip of another rare one: few pages say "perfi".
    // Nor of a common one with another first letter: "erft".
    assert_eq!(suggest("perft"), None);
    // A word the sites know is spelled right.
    let sites_know = |word: &str| word == "budafest";
    assert_eq!(
        searcher
            .suggest_spelling("budafest", None, &sites_know)
            .unwrap(),
        None
    );
}

#[test]
fn spellings_keep_the_words_pages_know() {
    let mut pages = Vec::new();
    for i in 0..3 {
        pages.push(page(&format!("PKCE flow {i}"), 10, &[]));
        pages.push(page(&format!("Stain removal {i}"), 10, &[]));
    }
    let (_dir, searcher) = searcher(&pages);
    let spelling = |query: &str, site: Option<&str>| crate::Spelling {
        query: query.into(),
        site: site.map(str::to_string),
        applied: false,
    };
    // Every changed word is known: no suggestion.
    assert_eq!(
        searcher
            .check_spelling("oauth2 pkce flow", spelling("oauth2 pace flow", None))
            .unwrap(),
        None
    );
    // Only the real typo is still fixed.
    assert_eq!(
        searcher
            .check_spelling(
                "remove red wnie stain",
                spelling("remove red wine spain", None)
            )
            .unwrap(),
        Some(spelling("remove red wine stain", None))
    );
    // Split or joined words with a known one changed: dropped.
    assert_eq!(
        searcher
            .check_spelling("stain wood", spelling("stainwood", None))
            .unwrap(),
        None
    );
    // A site's name is corrected whatever pages say.
    assert_eq!(
        searcher
            .check_spelling("stain", spelling("spain", Some("spain.info")))
            .unwrap(),
        Some(spelling("spain", Some("spain.info")))
    );
}

/// Wikipedia keeps the misspellings people type as redirects, so the
/// pages know "mississipi" and "einstien" and name articles by them.
#[test]
fn misspelled_redirects_are_still_corrected() {
    let pages = [
        page("Mississippi", 9_000, &["Mississipi", "MS"]),
        page("Mississippi River", 5_000, &["Mississipi River"]),
        page(
            "University of Mississippi",
            2_000,
            &["University of Mississipi"],
        ),
        page("Albert Einstein", 9_000, &["Einstein", "Einstien"]),
        page("Hanukkah", 3_000, &["Hanukah", "Chanukah"]),
        // A misspelling that is a title too, but of a little read page.
        page("Cemetery", 9_000, &["Cemetary"]),
        page("Cemetary (band)", 10, &[]),
        page("Cemetary Gates", 20, &[]),
        page("Cemetary Hill", 20, &[]),
        page("Stain", 900, &[]),
        page("Staining", 800, &[]),
        page("Stain removal", 10, &[]),
        page("Wood stain", 10, &[]),
    ];
    let (_dir, searcher) = searcher(&pages);
    let spelling = |query: &str| crate::Spelling {
        query: query.into(),
        site: None,
        applied: false,
    };
    let checked = |typed: &str, fixed: &str| {
        searcher
            .check_spelling(typed, spelling(fixed))
            .unwrap()
            .map(|spelling| spelling.query)
    };
    assert_eq!(
        checked("mississipi", "mississippi").as_deref(),
        Some("mississippi")
    );
    assert_eq!(
        checked("mississipi river cruise", "mississippi river cruise").as_deref(),
        Some("mississippi river cruise")
    );
    assert_eq!(checked("cemetary", "cemetery").as_deref(), Some("cemetery"));
    // A word the best read page with it is titled with is put back.
    assert_eq!(checked("stain wood", "spain wood"), None);
    let spelled_right = |typed: &str, fixed: Option<&str>| {
        let found = searcher.search(typed, 10).unwrap();
        searcher.spelled_right(typed, &found, fixed.map(spelling).as_ref())
    };
    // Named by a redirect, but the suggestion names the article too.
    assert!(!spelled_right("einstien", Some("einstein")));
    assert!(!spelled_right("mississipi", Some("mississippi")));
    assert!(!spelled_right("hanukah", Some("hanukkah")));
    // Named by its title as typed: spelled right.
    assert!(spelled_right("cemetary", Some("cemetery")));
    assert!(spelled_right("stain", Some("spain")));
    // Named by a redirect, and nothing else suggested.
    assert!(spelled_right("einstien", None));
    assert!(spelled_right("einstein", Some("einstien")));
}

#[test]
fn untitled_sites_take_their_article_s_title() {
    let mut notion = page("Notion (productivity software)", 500, &[]);
    notion.site = Some("notion.so".into());
    // A show whose website is a page of cbc.ca is read more than CBC's
    // own article, but it is not what cbc.ca is.
    let mut show = page("Schitt's Creek", 9_000, &[]);
    show.site = Some("cbc.ca".into());
    show.website = Some("https://www.cbc.ca/schittscreek".into());
    let mut cbc = page("CBC Television", 3_000, &[]);
    cbc.site = Some("cbc.ca".into());
    // A domain of the initials: nba.com.
    let mut nba = page("National Basketball Association", 4_000, &[]);
    nba.site = Some("nba.com".into());
    let (_dir, searcher) = searcher(&[notion, show, cbc, nba]);
    let mut sites = vec![
        site("notion.so", true),
        site("other.com", false),
        site("cbc.ca", true),
        site("nba.com", true),
    ];
    sites[1].title = Some("Other".into());
    searcher.title_untitled(&mut sites).unwrap();
    assert_eq!(sites[0].title.as_deref(), Some("Notion"));
    assert_eq!(sites[1].title.as_deref(), Some("Other"));
    assert_eq!(sites[2].title.as_deref(), Some("CBC Television"));
    assert_eq!(
        sites[3].title.as_deref(),
        Some("National Basketball Association")
    );
}

#[test]
fn official_sites_with_another_page_s_title_take_their_article_s() {
    let mut post = page("New York Post", 5_000, &[]);
    post.site = Some("nypost.com".into());
    let mut valve = page("Valve Corporation", 9_000, &[]);
    valve.site = Some("steampowered.com".into());
    let mut steam = page("Steam (service)", 4_000, &[]);
    steam.site = Some("steampowered.com".into());
    let (_dir, searcher) = searcher(&[post, valve, steam]);
    let titled = |domain: &str, title: &str, official: bool| crate::Hit {
        title: Some(title.into()),
        official,
        ..site(domain, false)
    };
    let mut sites = vec![
        titled(
            "nypost.com",
            "California Post – Breaking California News",
            true,
        ),
        titled("steampowered.com", "Welcome to Steam", true),
        // Not official: nothing says which name is its own.
        titled("nypost.com", "California Post", false),
    ];
    searcher.title_untitled(&mut sites).unwrap();
    assert_eq!(sites[0].title.as_deref(), Some("New York Post"));
    assert_eq!(sites[1].title.as_deref(), Some("Welcome to Steam"));
    assert_eq!(sites[2].title.as_deref(), Some("California Post"));
}

#[test]
fn a_famous_surname_alone_names_the_person() {
    let mut hilton = page("Hilton Worldwide", 3_448, &[]);
    hilton.description = Some("American hospitality company".into());
    hilton.site = Some("hilton.com".into());
    let (_dir, s) = searcher(&[
        person("Margaret Thatcher", 43_807, &[]),
        person("Denis Thatcher", 3_863, &[]),
        page("Thatcher effect", 900, &[]),
        person("Franz Kafka", 32_449, &[]),
        page("Kafka (film)", 1_200, &[]),
        page("Kafka on the Shore", 4_270, &[]),
        // Read too little more than another of the name.
        person("Franklin D. Roosevelt", 69_007, &[]),
        person("Theodore Roosevelt", 52_362, &[]),
        // An article is called the word, or a company's site is.
        person("Bruno Mars", 90_557, &[]),
        page("Mars", 80_000, &[]),
        person("Paris Hilton", 34_645, &[]),
        hilton,
    ]);
    let named = |q: &str| named_titles(&s, q);
    assert_eq!(named("thatcher"), ["Margaret Thatcher"]);
    assert_eq!(named("kafka"), ["Franz Kafka", "Kafka (film)"]);
    assert!(named("roosevelt").is_empty());
    assert_eq!(named("mars"), ["Mars"]);
    assert!(named("hilton").is_empty());
    // A surname and more words still name no one.
    assert!(named("thatcher era").is_empty());
}

#[test]
fn a_title_and_a_name_name_the_person_who_held_it() {
    let described = |title: &str, views: u64, aliases: &[&str], description: &str| {
        let mut person = person(title, views, aliases);
        person.description = Some(description.into());
        person
    };
    let (_dir, s) = searcher(&[
        described(
            "John F. Kennedy",
            75_228,
            &["JFK"],
            "President of the United States from 1961 to 1963",
        ),
        described(
            "Kennedy (commentator)",
            17_415,
            &[],
            "American television personality",
        ),
        described(
            "Robert F. Kennedy",
            31_922,
            &[],
            "American politician and lawyer (1925–1968)",
        ),
        described(
            "Diana, Princess of Wales",
            94_857,
            &["Lady Diana Spencer"],
            "Member of the British royal family (1961–1997)",
        ),
        page("Princess Diana (song)", 1_500, &[]),
        described("Isaac Newton", 40_694, &[], "English polymath (1642–1727)"),
        page("Newton (unit)", 4_568, &[]),
        described(
            "George W. Bush",
            69_493,
            &[],
            "President of the United States from 2001 to 2009",
        ),
        described(
            "George H. W. Bush",
            41_000,
            &[],
            "President of the United States from 1989 to 1993",
        ),
        described(
            "Abraham Lincoln",
            80_000,
            &["President Lincoln"],
            "President of the United States",
        ),
        page("General Motors", 30_000, &[]),
    ]);
    let named = |q: &str| named_titles(&s, q);
    assert_eq!(named("president kennedy"), ["John F. Kennedy"]);
    assert_eq!(named("princess diana")[0], "Diana, Princess of Wales");
    assert_eq!(named("sir isaac newton"), ["Isaac Newton"]);
    // Two presidents of the name, a page called the whole query, and no
    // person of the name.
    assert!(named("president bush").is_empty());
    assert_eq!(named("president lincoln"), ["Abraham Lincoln"]);
    assert_eq!(named("general motors"), ["General Motors"]);
}

#[test]
fn a_name_with_capitals_inside_that_names_no_page_lists_questions_about_it() {
    let question = |title: &str, tags: &str, views| {
        Page::from_question(Article {
            title: title.into(),
            description: Some(tags.into()),
            item: Some(title.len().to_string()),
            views,
            ..Article::default()
        })
    };
    let pages = vec![
        question(
            "Kubernetes pod stuck in CrashLoopBackOff",
            "kubernetes",
            90_000,
        ),
        question(
            "What does CrashLoopBackOff mean for a pod?",
            "kubernetes, pod",
            40_000,
        ),
        question(
            "Pod restarts with exit code 137",
            "kubernetes, crashloopbackoff",
            200_000,
        ),
        question(
            "UnicodeDecodeError: 'utf-8' codec can't decode byte",
            "python, unicode",
            300_000,
        ),
        question(
            "Connect ECONNREFUSED 127.0.0.1:3000 in Node",
            "node.js",
            80_000,
        ),
        question("How do I embed a YouTube video?", "html, youtube", 500_000),
        page("YouTube", 9_000_000, &[]),
        page("Kubernetes", 2_000_000, &[]),
    ];
    let (_dir, searcher) = searcher(&pages);
    let sites = [site("crashlytics.com", false), site("other.com", false)];
    for (query, title) in [
        (
            "CrashLoopBackOff",
            "Kubernetes pod stuck in CrashLoopBackOff",
        ),
        (
            "UnicodeDecodeError",
            "UnicodeDecodeError: 'utf-8' codec can't decode byte",
        ),
        (
            "ECONNREFUSED",
            "Connect ECONNREFUSED 127.0.0.1:3000 in Node",
        ),
    ] {
        let placed = place_pages(query, &sites, searcher.search(query, 10).unwrap());
        let listed: Vec<&str> = placed.iter().map(|p| p.hit.page.title.as_str()).collect();
        assert_eq!(listed.first(), Some(&title), "{query}: {listed:?}");
        // A question with it only among its tags is not about it.
        assert!(
            !listed.contains(&"Pod restarts with exit code 137"),
            "{query}"
        );
    }
    // A name an article has, and a word in lower case, are as before.
    for query in ["YouTube", "crashloopbackoff"] {
        let placed = place_pages(query, &sites, searcher.search(query, 10).unwrap());
        assert!(
            placed.iter().all(|p| !p.hit.page.is_question()),
            "{query}: {placed:?}"
        );
    }
}

#[test]
fn a_question_of_only_asking_words_and_code_names_nothing() {
    let question = |title: &str, alias: &str, item: &str| {
        Page::from_question(Article {
            title: title.into(),
            description: Some("command-line".into()),
            item: Some(item.into()),
            views: 300_000,
            aliases: vec![alias.into()],
            ..Article::default()
        })
    };
    let pages = [
        question(
            "Why did the command \":(){ : : & };:\" make my system lag?",
            "What does :(){ : :& };: do?",
            "1",
        ),
        question(
            "Why do we use \"./\" (dot slash) to execute a file?",
            "What does ./ mean?",
            "2",
        ),
        question("What is a monad?", "Monads explained", "3"),
        question("What does ~$ stand for?", "Tilde before the prompt", "4"),
    ];
    // Pages that make the query's other words known, not typos.
    let known = [
        "Liver",
        "Liver disease",
        "Liver transplantation",
        "Kidney",
        "Kidney stone",
        "Kidney failure",
        "NASA",
        "NASA TV",
        "NASA Astronaut Corps",
    ]
    .map(|title| page(title, 1_000, &[]));
    let (_dir, s) = searcher(&[&pages[..], &known[..]].concat());
    let questions = |query: &str| -> Vec<String> {
        s.search(query, 5)
            .unwrap()
            .into_iter()
            .filter(|hit| hit.page.is_question())
            .map(|hit| hit.page.title)
            .collect()
    };
    assert!(questions("what does the liver do").is_empty());
    assert!(questions("what does a kidney do").is_empty());
    assert!(questions("what does yeet mean").is_empty());
    assert!(questions("what does nasa stand for").is_empty());
    // A question that asks about something is still found.
    assert_eq!(questions("what is a monad"), ["What is a monad?"]);
}

#[test]
fn the_and_a_name_names_what_the_name_alone_does_when_far_more_read() {
    let (_dir, s) = searcher(&[
        page("Titanic (1997 film)", 150_000, &[]),
        page("Titanic", 120_000, &["RMS Titanic"]),
        page("Stroke", 30_000, &["Strokes"]),
        page("Pope", 28_157, &["Supreme Pontiff"]),
        page("The Strokes", 19_059, &[]),
        page("The Police", 17_369, &[]),
        page("Police", 15_000, &["Police officer"]),
        page("Bible", 14_969, &["The Bible"]),
        page("The Titanic (song)", 114, &[]),
        page("The Pope (book)", 65, &["Du Pape", "On the Pope"]),
    ]);
    let named = |q: &str| -> Vec<String> {
        let hits = s.search(q, 5).unwrap();
        hits.into_iter()
            .filter(|hit| hit.named)
            .map(|hit| hit.page.title)
            .collect()
    };
    assert_eq!(named("the pope"), ["Pope", "The Pope (book)"]);
    assert_eq!(named("the titanic"), ["Titanic", "The Titanic (song)"]);
    // Read too little more than what "the" and the name call.
    assert_eq!(named("the police"), ["The Police"]);
    assert_eq!(named("the strokes"), ["The Strokes"]);
    assert_eq!(named("the bible"), ["Bible"]);
}
