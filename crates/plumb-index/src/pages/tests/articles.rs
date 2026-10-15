//! Articles found by their names, leads and what they say of themselves.

use super::*;

#[test]
fn a_judged_name_finds_what_it_judges() {
    let (_dir, s) = searcher(&[
        page("Blood pressure", 90_000, &[]),
        page("Normal distribution", 80_000, &[]),
        page("Distribution (economics)", 20_000, &[]),
        page("BMI (company)", 5_000, &["BMI"]),
    ]);
    let named = |query: &str| -> Vec<String> {
        let hits = s.search(query, 5).unwrap();
        hits.into_iter()
            .filter(|hit| hit.named)
            .map(|hit| hit.page.title)
            .collect()
    };
    assert_eq!(named("what is a normal blood pressure"), ["Blood pressure"]);
    // A page called the whole name keeps its namesakes away.
    assert_eq!(
        named("what is a normal distribution"),
        ["Normal distribution"]
    );
    assert!(named("what is a healthy bmi").is_empty());
}

#[test]
fn a_field_after_the_name_keeps_that_sense() {
    let mut film = page("Cell (film)", 90_000, &[]);
    film.description = Some("2016 film by Tod Williams".into());
    let mut biology = page("Cell (biology)", 80_000, &[]);
    biology.description = Some("Basic unit of all life".into());
    let (_dir, s) = searcher(&[film, biology, page("Cell biology", 40_000, &[])]);
    let named = |query: &str| -> Vec<String> {
        let hits = s.search(query, 5).unwrap();
        hits.into_iter()
            .filter(|hit| hit.named)
            .map(|hit| hit.page.title)
            .collect()
    };
    assert_eq!(named("what is a cell in biology"), ["Cell (biology)"]);
    // With no page on the field's sense, the name's pages are all kept.
    assert_eq!(named("what is a cell in poetry").len(), 2);
}

#[test]
fn words_that_say_what_is_wanted_keep_the_name() {
    let (_dir, s) = searcher(&[
        page("Lululemon", 50_000, &[]),
        page("Galileo Galilei", 90_000, &["Galileo"]),
        page("Batch normalization", 20_000, &[]),
        page("Ariana Grande", 300_000, &[]),
        page("Hiccup", 60_000, &["Hiccups"]),
        page("Refrigerator", 70_000, &["Fridge"]),
        page("Eiffel Tower", 80_000, &[]),
        page("Eiffel Tower (Paris, Tennessee)", 2_000, &[]),
        Page::from_paper(Article {
            title: "Batch Normalization: Accelerating Deep Network Training by Reducing Internal Covariate Shift".into(),
            description: Some("Paper by Sergey Ioffe et al., 2015".into()),
            item: Some("10.1/1".into()),
            views: 40_000,
            ..Article::default()
        }),
    ]);
    let first = |q: &str| {
        let hits = s.search(q, 5).unwrap();
        hits.first().map(|h| (h.page.title.clone(), h.named))
    };
    assert_eq!(
        first("lululemon wikipedia"),
        Some(("Lululemon".into(), true))
    );
    assert_eq!(
        first("who is galileo"),
        Some(("Galileo Galilei".into(), true))
    );
    assert_eq!(
        first("ariana grande age"),
        Some(("Ariana Grande".into(), true))
    );
    assert_eq!(first("what causes hiccups"), Some(("Hiccup".into(), true)));
    assert_eq!(
        first("how does a refrigerator work"),
        Some(("Refrigerator".into(), true))
    );
    assert_eq!(
        first("how tall is the eiffel tower"),
        Some(("Eiffel Tower".into(), true))
    );
    let (title, named) = first("batch normalization paper").unwrap();
    assert!(
        title.starts_with("Batch Normalization: Accelerating"),
        "{title}"
    );
    assert!(named);
    // Without such words nothing changes.
    assert_eq!(
        first("batch normalization").unwrap().0,
        "Batch normalization"
    );
    assert_eq!(hinted_name("wikipedia"), None);
    assert_eq!(hinted_name("who is"), None);
    assert_eq!(
        hinted_name("Who is Dalai Lama?"),
        Some(("dalai lama".into(), Hint::Any))
    );
}

#[test]
fn a_person_wikidata_gives_no_dates_for_is_a_person() {
    let described = |title: &str, views: u64, description: &str, lead: &str| {
        let mut page = page(title, views, &[]);
        page.description = Some(description.into());
        page.lead = Some(lead.into());
        page
    };
    let stafford = described(
        "Terry Stafford",
        1_001,
        "American singer-songwriter (1941–1996)",
        "Terry LaVerne Stafford (November 22, 1941 – March 17, 1996) was an American singer.",
    );
    let roe = described(
        "Tommy Roe",
        1_086,
        "American rock and pop singer-songwriter",
        "Thomas David Roe (/roʊ/; born May 9, 1942) is an American rock and pop singer.",
    );
    let mut fuller = page("Bobby Fuller", 1_953, &[]);
    fuller.facts = vec![plumb_core::facts::Fact {
        kind: plumb_core::facts::FactKind::Died,
        value: "1966-07-18".into(),
    }];
    let band = described(
        "The Bobby Fuller Four",
        625,
        "American rock & roll band (1962–1966)",
        "The Bobby Fuller Four (sometimes stylized as Bobby Fuller 4) was a rock band.",
    );
    let war = described(
        "World War II",
        900_000,
        "1939–1945 global conflict",
        "World War II (1 September 1939 – 2 September 1945) was a global conflict.",
    );
    assert!(stafford.is_person() && roe.is_person() && fuller.is_person());
    assert!(!band.is_person() && !war.is_person());
    let (_dir, s) = searcher(&[stafford, roe, fuller, band, page("Stafford", 50_000, &[])]);
    let named = |q: &str| -> Vec<String> {
        let hits = s.search(q, 5).unwrap();
        hits.into_iter()
            .filter(|hit| hit.named)
            .map(|hit| hit.page.title)
            .collect()
    };
    assert_eq!(named("terry stafford death"), ["Terry Stafford"]);
    assert_eq!(named("tommy roe songs"), ["Tommy Roe"]);
    assert_eq!(named("bobby fuller death"), ["Bobby Fuller"]);
}

#[test]
fn a_search_for_one_part_of_a_thing_finds_the_thing() {
    let (_dir, s) = searcher(&[
        page("Ibuprofen", 200_000, &[]),
        page("Shingles", 150_000, &["Herpes zoster"]),
        page("Volcano", 120_000, &[]),
        page("Iron", 110_000, &[]),
        page("Sun", 180_000, &[]),
        page("Glucose", 90_000, &[]),
        page("Sphere", 80_000, &[]),
        page("Side Effects (2013 film)", 300_000, &["Side Effects"]),
        page("Plaster", 90_000, &[]),
        page("Plaster cast", 20_000, &[]),
        dated_paper("A loss function for metric learning", None, Some(2022), 10),
        dated_paper("Loss aversion in markets", None, Some(2022), 10),
    ]);
    let first = |q: &str| {
        let hits = s.search(q, 5).unwrap();
        hits.first().map(|h| (h.page.title.clone(), h.named))
    };
    for (query, title) in [
        ("ibuprofen side effects", "Ibuprofen"),
        ("side effects of ibuprofen", "Ibuprofen"),
        ("shingles symptoms", "Shingles"),
        ("volcano facts", "Volcano"),
        ("melting point of iron", "Iron"),
        ("iron melting point", "Iron"),
        ("what is the melting point of iron", "Iron"),
        ("mass of the sun", "Sun"),
        ("glucose chemical formula", "Glucose"),
        ("volume of a sphere", "Sphere"),
    ] {
        assert_eq!(first(query), Some((title.into(), true)), "{query}");
    }
    // A page called the whole query is what it asks for: "plaster cast"
    // is not about plaster.
    assert_eq!(first("plaster cast"), Some(("Plaster cast".into(), true)));
    let hits = s.search("plaster cast", 5).unwrap();
    assert!(
        hits.iter().all(|h| h.page.title != "Plaster" || !h.named),
        "{hits:?}"
    );
    // An everyday thing's temperature is a setting, not its article's.
    assert_eq!(hinted_name("oven temperature"), None);
    // Papers keep those words.
    assert_eq!(
        hinted_name("loss function"),
        Some(("loss".into(), Hint::Aspect))
    );
    let papers = s
        .search_papers(&PaperQuery::parse("loss function after:2020").unwrap(), 5)
        .unwrap();
    assert_eq!(titles(&papers), ["A loss function for metric learning"]);
}

#[test]
fn repository_words_find_the_repository() {
    let mut tokio = Page::from_repo(Article {
        title: "tokio-rs/tokio".into(),
        aliases: vec!["tokio".into()],
        views: 30_000,
        ..Article::default()
    });
    tokio.site = Some("tokio.rs".into());
    let reverse = Page::from_repo(Article {
        title: "someone/reverse-proxy".into(),
        views: 30_000,
        ..Article::default()
    });
    let (_dir, s) = searcher(&[
        tokio,
        reverse,
        page("Tokio Hotel", 200_000, &[]),
        page("Repurchase agreement", 90_000, &["Reverse repo"]),
    ]);
    let sites = [site("github.com", false), site("tokio.rs", false)];
    for query in [
        "tokio github",
        "tokio repo",
        "tokio github repo",
        "Tokio repository",
    ] {
        let hits = s.search(query, 5).unwrap();
        assert_eq!(hits[0].page.title, "tokio-rs/tokio", "{query}");
        assert!(hits[0].named, "{query}");
        let placed = place_pages(query, &sites, hits.clone());
        assert!(
            placed.iter().any(|p| p.hit.page.title == "tokio-rs/tokio"),
            "{query}"
        );
        // Without its site, it comes right after github.com.
        let placed = place_pages(query, &[site("github.com", true)], hits);
        assert_eq!(placed[0].at, 1, "{query}");
    }
    assert_eq!(
        hinted_name("tokio github"),
        Some(("tokio".into(), Hint::Repo))
    );
    assert_eq!(hinted_name("github"), None);
    // A repository named only in part is not taken for the one asked for.
    let hits = s.search("reverse repo", 5).unwrap();
    assert_eq!(titles(&hits), ["Repurchase agreement"]);
}

#[test]
fn a_search_for_a_list_finds_the_list_article() {
    let (_dir, s) = searcher(&[
        page("List of popes", 90_000, &[]),
        page("Pope John numbering", 5_000, &[]),
        page("Order of St. Sylvester", 2_000, &[]),
        page(
            "List of presidents of the United States",
            300_000,
            &["List of US presidents"],
        ),
        page("Linked list", 80_000, &[]),
    ]);
    for (query, list) in [
        ("popes in order", "List of popes"),
        ("list of popes in order", "List of popes"),
        (
            "us presidents list",
            "List of presidents of the United States",
        ),
        (
            "US presidents list in order",
            "List of presidents of the United States",
        ),
    ] {
        let hits = s.search(query, 5).unwrap();
        assert_eq!(hits[0].page.title, list, "{query}");
        assert!(hits[0].named, "{query}");
    }
    // Nothing is called "list of linked".
    assert_eq!(
        titles(&s.search("linked list", 5).unwrap()),
        ["Linked list"]
    );
}

#[test]
fn fact_questions_find_the_article_they_ask_about() {
    let described = |title: &str, views: u64, description: &str| {
        let mut page = page(title, views, &[]);
        page.description = Some(description.into());
        page
    };
    let (_dir, s) = searcher(&[
        page("Isaac Newton", 400_000, &[]),
        described(
            "Hannah Ayscough",
            3_000,
            "Mother of Sir Isaac Newton (1616–1679)",
        ),
        described(
            "Nate Newton (American football)",
            5_000,
            "American football player (born 1961)",
        ),
        page("Mont Blanc", 60_000, &[]),
        described("SS Mont-Blanc", 8_000, "French cargo ship"),
        page("Age of Empires", 80_000, &[]),
        page("Empires (film)", 2_000, &[]),
        described("Madrid", 90_000, "Capital and largest city of Spain"),
        described("Real Madrid CF", 300_000, "Spanish football club"),
        described("Atlético Madrid", 120_000, "Spanish football club"),
        described("Rome", 90_000, "Capital city of Italy"),
        described("S.S. Lazio", 300_000, "Italian football club in Rome"),
    ]);
    let first = |q: &str| {
        let hits = s.search(q, 5).unwrap();
        hits.first().map(|h| (h.page.title.clone(), h.named))
    };
    assert_eq!(
        first("when was isaac newton born"),
        Some(("Isaac Newton".into(), true))
    );
    assert_eq!(
        first("how high is mont blanc"),
        Some(("Mont Blanc".into(), true))
    );
    // Only what the subject names is added: "madrid spanish" names
    // nothing, and the clubs it describes are no answer.
    assert_eq!(
        first("madrid spanish capital city"),
        Some(("Madrid".into(), false))
    );
    assert_eq!(
        first("rome italian capital city"),
        Some(("Rome".into(), false))
    );
    // A query that names something is not a question about its last words.
    let hits = s.search("age of empires", 5).unwrap();
    assert_eq!(hits[0].page.title, "Age of Empires");
    assert!(!hits
        .iter()
        .any(|h| h.page.title == "Empires (film)" && h.named));
}

#[test]
fn a_question_about_a_works_maker_finds_the_work_without_its_the() {
    use plumb_core::facts::{Fact, FactKind};
    let work = |title: &str, views: u64, maker: Option<(FactKind, &str)>| {
        let mut page = page(title, views, &[]);
        page.facts = maker
            .map(|(kind, value)| Fact {
                kind,
                value: value.into(),
            })
            .into_iter()
            .collect();
        page
    };
    let (_dir, s) = searcher(&[
        work(
            "Mona Lisa",
            34_216,
            Some((FactKind::Creator, "Leonardo da Vinci")),
        ),
        work("The Mona Lisa (song)", 24, None),
        work(
            "The Hobbit",
            30_000,
            Some((FactKind::Author, "J. R. R. Tolkien")),
        ),
        work("Hobbit", 10_000, None),
    ]);
    let named = |q: &str| -> Vec<String> {
        s.search(q, 5)
            .unwrap()
            .into_iter()
            .filter(|hit| hit.named)
            .map(|hit| hit.page.title)
            .collect()
    };
    assert!(named("who painted the mona lisa").contains(&"Mona Lisa".to_string()));
    // Named without "the" only when it holds what is asked: the hobbits
    // have no author.
    assert_eq!(named("who wrote the hobbit"), ["The Hobbit"]);
}

#[test]
fn an_event_is_named_whichever_end_its_year_is_on_and_asked_who_won() {
    let (_dir, s) = searcher(&[
        page("Tour de France", 200_000, &[]),
        page("2025 Tour de France", 40_000, &[]),
        page("2026 Tour de France", 30_000, &[]),
        page("Eurovision Song Contest 2025", 60_000, &[]),
        page("2025 World Series", 50_000, &[]),
        page("World Series", 80_000, &[]),
    ]);
    let first = |q: &str| {
        let hits = s.search(q, 5).unwrap();
        hits.first().map(|h| (h.page.title.clone(), h.named))
    };
    for query in ["tour de france 2025", "2025 tour de france"] {
        assert_eq!(
            first(query),
            Some(("2025 Tour de France".into(), true)),
            "{query}"
        );
    }
    assert_eq!(
        first("2025 eurovision song contest"),
        Some(("Eurovision Song Contest 2025".into(), true))
    );
    for query in ["who won the 2025 world series", "world series 2025 winner"] {
        assert_eq!(
            first(query),
            Some(("2025 World Series".into(), true)),
            "{query}"
        );
    }
    assert_eq!(
        year_moved("tour de france 2025").as_deref(),
        Some("2025 tour de france")
    );
    assert_eq!(year_moved("1984"), None);
    assert_eq!(year_moved("route 66"), None);
}

#[test]
fn an_article_is_found_by_its_alias_past_popular_site_pages_with_the_word() {
    // Site pages are as popular as their sites, more than all but the most
    // read articles: hundreds with "NFL" in their titles filled the
    // candidates, and the article named by "nfl" was never seen.
    let mut pages = vec![
        page("United States", 5_000_000, &[]),
        page("National Football League", 51_000, &["Nfl", "The NFL"]),
        page("NFL+", 363, &[]),
    ];
    pages.extend((0..CANDIDATES + 50).map(|i| {
        Page::from_set(
            SUBPAGES_SET,
            Article {
                title: format!("NFL Scores Week {i}"),
                item: Some(format!("https://sports{i}.example/nfl/scores")),
                views: 5_000,
                ..Article::default()
            },
        )
        .unwrap()
    }));
    let (_dir, s) = searcher(&pages);
    let hits = s.search("nfl", 5).unwrap();
    assert_eq!(
        hits.first().map(|h| (h.page.title.as_str(), h.named)),
        Some(("National Football League", true)),
        "{:?}",
        titles(&hits)
    );
}

#[test]
fn articles_whose_title_is_part_of_the_query_are_found() {
    let (_dir, s) = searcher(&[
        page("Titanic", 400_000, &["RMS Titanic"]),
        page("Marie Curie", 300_000, &[]),
        page("Homily", 20_000, &[]),
        page("Sinking", 5_000, &[]),
        Page::from_question(Article {
            title: "Homily".into(),
            description: Some("sermon, outline".into()),
            item: Some("1".into()),
            views: 900_000,
            ..Article::default()
        }),
    ]);
    let first = |query: &str| {
        s.search(query, 5)
            .unwrap()
            .first()
            .map(|hit| (hit.page.title.clone(), hit.named))
    };
    assert_eq!(first("titanic sinking"), Some(("Titanic".into(), false)));
    assert_eq!(
        first("radium discovery marie curie"),
        Some(("Marie Curie".into(), false))
    );
    assert_eq!(first("rms titanic wreck"), Some(("Titanic".into(), false)));
    // Only articles are found so: a question titled "Homily" is not.
    let hits = s.search("catholic homily", 5).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].page.is_article());
    // Words that only ask name no article.
    assert_eq!(first("what does it mean"), None);
    // A question found by most of its words is kept, though its title
    // is part of the query too.
    let hits = s.search("homily sermon outline example", 5).unwrap();
    assert!(hits.iter().any(|hit| hit.page.is_question()), "{hits:?}");
}

#[test]
fn an_article_the_query_names_and_its_lead_describes_comes_first() {
    let with_lead = |title: &str, views: u64, lead: &str| {
        let mut page = page(title, views, &[]);
        page.lead = Some(lead.into());
        page
    };
    let (_dir, s) = searcher(&[
        with_lead(
            "Marie Curie",
            300_000,
            "Polish and naturalised-French physicist and chemist who conducted \
             pioneering research on radioactivity, including the discovery of \
             polonium and radium.",
        ),
        with_lead(
            "Radium",
            250_000,
            "Radium is a chemical element. Its discovery by Marie and Pierre \
             Curie in 1898 made Marie Curie famous; radium was isolated in 1910.",
        ),
        with_lead(
            "Radium bromide",
            3_000,
            "Radium bromide is a compound of radium. Marie Curie used it in \
             the discovery of radium.",
        ),
    ]);
    let hits = s.search("radium discovery marie curie", 5).unwrap();
    assert_eq!(hits[0].page.title, "Marie Curie", "{hits:?}");
    assert!(hits[0].score >= MIN_PARTIAL_SCORE, "{hits:?}");
    // An article that only says it all is not named: "radium discovery"
    // still finds Radium by its lead.
    let hits = s.search("radium discovery", 5).unwrap();
    assert_eq!(hits[0].page.title, "Radium", "{hits:?}");
}

#[test]
fn articles_are_found_by_what_they_say_of_themselves() {
    let described = |title: &str, description: &str, views: u64, aliases: &[&str]| {
        let mut page = page(title, views, aliases);
        page.description = Some(description.into());
        page
    };
    let (_dir, s) = searcher(&[
        described("Queen (band)", "British rock band", 900_000, &[]),
        described("Queen (album)", "1973 studio album by Queen", 40_000, &[]),
        described(
            "William McKinley",
            "President of the United States from 1897 to 1901",
            300_000,
            &[],
        ),
        described("Folic acid", "Synthetic form of vitamin B9", 200_000, &[]),
        described("Coworking", "Shared office arrangement", 50_000, &[]),
        described("Resin", "Solid or highly viscous substance", 60_000, &[]),
        described("Album", "Collection of audio recordings", 500_000, &[]),
        described("Acid", "Chemical compound", 500_000, &[]),
    ]);
    let found = |query: &str| -> Vec<(String, f32)> {
        s.search(query, 5)
            .unwrap()
            .into_iter()
            .map(|hit| (hit.page.title, hit.score))
            .collect()
    };
    let first = |query: &str| found(query).first().map(|(title, _)| title.clone());
    assert_eq!(first("queen album").as_deref(), Some("Queen (album)"));
    assert_eq!(
        first("mckinley president").as_deref(),
        Some("William McKinley")
    );
    // A word the article does not say is left over when it names the
    // article in full.
    assert_eq!(first("source of folic acid").as_deref(), Some("Folic acid"));
    // Words that only ask are not looked for.
    assert_eq!(first("what is coworking").as_deref(), Some("Coworking"));
    assert_eq!(first("what does resin mean").as_deref(), Some("Resin"));
    // Such a page is not named, and is listed only when it covers the
    // query well enough.
    let hits = s.search("mckinley president", 5).unwrap();
    assert!(!hits[0].named);
    // A word neither its names nor its description have keeps the
    // article out when the query names it only in part.
    assert!(found("queen tour")
        .iter()
        .all(|(title, _)| title != "Queen (album)"));
    // One word is a name, not a description.
    assert!(found("album")
        .iter()
        .all(|(title, _)| title != "Queen (album)"));
    assert_eq!(
        hinted_name("what does resin mean"),
        Some(("resin".into(), Hint::Any))
    );
    assert_eq!(
        hinted_name("What is a manubrium?"),
        Some(("manubrium".into(), Hint::Any))
    );
    for (query, name) in [
        ("what does asap stand for", "asap"),
        ("What does NASA stand for?", "nasa"),
        ("what’s a roth ira", "roth ira"),
        ("what's kombucha", "kombucha"),
        ("what is the meaning of serendipity", "serendipity"),
        ("what causes hiccups", "hiccups"),
        ("what causes the northern lights", "northern lights"),
        ("where is machu picchu", "machu picchu"),
        ("how many people live in tokyo", "tokyo"),
    ] {
        assert_eq!(
            hinted_name(query),
            Some((name.into(), Hint::Any)),
            "{query:?}"
        );
    }
    // A question of how a thing works, what it does or how to be rid of
    // it asks about the thing itself, not its namesakes.
    for (query, name) in [
        ("how does a refrigerator work", "refrigerator"),
        ("how do vaccines work", "vaccines"),
        ("how tall is the eiffel tower", "eiffel tower"),
        ("how big is texas", "texas"),
        ("how tall is the eiffel tower in feet", "eiffel tower"),
        ("how big is texas in square miles", "texas"),
        ("how old is obama now", "obama"),
        ("how metabolism works", "metabolism"),
        ("how the stock market works", "stock market"),
        ("how vaccines work", "vaccines"),
        ("where does photosynthesis take place", "photosynthesis"),
        ("where does meiosis occur", "meiosis"),
        ("where do babies come from", "babies"),
        ("where is taylor swift from", "taylor swift"),
        ("where are the beatles from", "beatles"),
        ("is coffee bad for you", "coffee"),
        ("is intermittent fasting healthy", "intermittent fasting"),
        ("are eggs good for you", "eggs"),
        ("does creatine work", "creatine"),
        ("do charcoal masks really work", "charcoal masks"),
        ("what does the liver do", "liver"),
        ("what does a pharmacist do", "pharmacist"),
        ("what do bees do", "bees"),
        ("who invented the telephone", "telephone"),
        ("who discovered penicillin", "penicillin"),
        ("when was the printing press invented", "printing press"),
        ("what do giraffes eat", "giraffes"),
        ("what does a koala eat", "koala"),
        ("where do penguins live", "penguins"),
        ("how long do dogs live", "dogs"),
        ("how long does a turtle live", "turtle"),
        ("how fast can a cheetah run", "cheetah"),
        ("how big do great danes get", "great danes"),
        ("how many legs does a spider have", "spider"),
        ("how many moons does jupiter have", "jupiter"),
        ("how many kids does elon musk have", "elon musk"),
        ("how to get rid of hiccups", "hiccups"),
        ("how do i get rid of fruit flies", "fruit flies"),
        ("how to treat a sunburn", "sunburn"),
        ("how to stop snoring", "snoring"),
    ] {
        assert_eq!(
            hinted_name(query),
            Some((name.into(), Hint::Own)),
            "{query:?}"
        );
    }
    assert_eq!(hinted_name("whats app"), None);
    assert_eq!(hinted_name("how does it work"), None);
    assert_eq!(hinted_name("how long is it"), None);
    assert_eq!(hinted_name("how long does it take to boil an egg"), None);
    assert_eq!(hinted_name("how do i make pancakes"), None);
    // A pronoun or a clause between the words is not a name.
    for query in [
        "how it works",
        "does it work",
        "is it safe",
        "is this website safe",
        "do you work",
        "how do i make it work",
        "how to make money work",
        "how many hours do nurses work",
        "is the new bridge over the river safe",
        "who invented it",
        "when was it invented",
        "what do you do",
        "what do i do",
        "how to get rid of it",
        "how to stop you from doing it",
        "what do you eat",
        "where do i live",
        "how many do you have",
        "how many hours do nurses work",
    ] {
        assert_eq!(hinted_name(query), None, "{query:?}");
    }
}

#[test]
fn a_problem_asked_how_to_be_rid_of_names_its_own_article() {
    let (_dir, s) = searcher(&[
        page("Heartburn", 30_000, &[]),
        page("Heartburn (film)", 20_000, &[]),
        page("Hiccup", 40_000, &[]),
        page("Mole (animal)", 50_000, &[]),
        page("Mole (skin)", 30_000, &[]),
    ]);
    let named = |query: &str| -> Vec<String> {
        let hits = s.search(query, 5).unwrap();
        let mut titles: Vec<String> = hits
            .into_iter()
            .filter(|hit| hit.named)
            .map(|hit| hit.page.title)
            .collect();
        titles.sort();
        titles
    };
    assert_eq!(named("how to get rid of heartburn"), ["Heartburn"]);
    assert_eq!(named("how to stop hiccups"), ["Hiccup"]);
    // With no article called the name alone, its namesakes all stay.
    assert_eq!(
        named("how to get rid of a mole"),
        ["Mole (animal)", "Mole (skin)"]
    );
}

#[test]
fn a_question_of_what_a_thing_does_names_its_own_article() {
    let (_dir, s) = searcher(&[
        page("Spider", 90_000, &[]),
        page("Spider (2002 film)", 20_000, &[]),
        page("Giraffe", 60_000, &[]),
    ]);
    let named = |query: &str| -> Vec<String> {
        let hits = s.search(query, 5).unwrap();
        hits.into_iter()
            .filter(|hit| hit.named)
            .map(|hit| hit.page.title)
            .collect()
    };
    assert_eq!(named("how many legs does a spider have"), ["Spider"]);
    assert_eq!(named("what do giraffes eat"), ["Giraffe"]);
}

#[test]
fn who_played_a_role_names_the_character() {
    let (_dir, s) = searcher(&[
        page("Joker (character)", 500_000, &[]),
        page("The People's Joker", 40_000, &[]),
        page("Joker (playing card)", 50_000, &[]),
        page("Woody (Toy Story)", 100_000, &[]),
        page("Woodie King Jr.", 5_000, &[]),
    ]);
    let first = |q: &str| {
        let hits = s.search(q, 5).unwrap();
        hits.into_iter()
            .find(|hit| hit.named)
            .map(|hit| hit.page.title)
    };
    assert_eq!(
        first("who played the joker").as_deref(),
        Some("Joker (character)")
    );
    assert_eq!(
        first("who voiced woody").as_deref(),
        Some("Woody (Toy Story)")
    );
    assert_eq!(
        hinted_name("who plays spiderman"),
        Some(("spiderman".into(), Hint::Any))
    );
}

#[test]
fn a_bands_members_name_the_band() {
    let (_dir, s) = searcher(&[
        page("BTS", 900_000, &[]),
        page("BTS: Bon Voyage", 40_000, &[]),
        page("ABBA", 500_000, &[]),
        page("Mansour Abbas", 60_000, &[]),
        page("Member of parliament", 80_000, &[]),
    ]);
    let named = |q: &str| -> Vec<String> {
        let hits = s.search(q, 5).unwrap();
        hits.iter()
            .filter(|hit| hit.named)
            .map(|hit| hit.page.title.clone())
            .collect()
    };
    for q in [
        "bts members",
        "who are the members of bts",
        "BTS band members",
    ] {
        assert_eq!(named(q), ["BTS"], "{q}");
    }
    assert_eq!(named("abba members"), ["ABBA"]);
    assert_eq!(
        hinted_name("members of the rolling stones"),
        Some(("rolling stones".into(), Hint::Any))
    );
}

#[test]
fn articles_are_found_by_their_other_names() {
    let mut sternum = page("Sternum", 300_000, &["Breastbone"]);
    sternum.names = vec!["Manubrium".into(), "Manubrium sterni".into()];
    let (_dir, s) = searcher(&[
        sternum,
        page("Manubrium (band)", 100, &[]),
        page("Clavicle", 400_000, &[]),
    ]);
    let hits = s.search("manubrium", 5).unwrap();
    // The page the query names comes first; the article it names a
    // part of is listed, but not as named.
    assert_eq!(titles(&hits), ["Manubrium (band)", "Sternum"]);
    assert!(!hits[1].named);
    assert!(hits[1].score >= MIN_PARTIAL_SCORE);
    let (_dir, s) = searcher(&[{
        let mut sternum = page("Sternum", 300_000, &[]);
        sternum.names = vec!["Manubrium".into()];
        sternum
    }]);
    assert_eq!(titles(&s.search("manubrium", 5).unwrap()), ["Sternum"]);
    assert_eq!(
        titles(&s.search("what is the manubrium", 5).unwrap()),
        ["Sternum"]
    );
}

#[test]
fn articles_are_found_by_their_leads() {
    let led = |title: &str, views: u64, lead: &str| {
        let mut page = page(title, views, &[]);
        page.lead = Some(lead.into());
        page
    };
    let (_dir, s) = searcher(&[
        led(
            "Mesozoic",
            200_000,
            "The Mesozoic Era is the era of Earth's geological history, comprising the Triassic, Jurassic and Cretaceous Periods.",
        ),
        led(
            "Dinosaur",
            900_000,
            "Dinosaurs are a diverse group of reptiles that emerged during the Triassic period. They became dominant in the Jurassic, and most died out at the end of the Cretaceous, with birds the only survivors of a long history spanning many periods and kinds of animals on every continent.",
        ),
        led(
            "Jurassic Park",
            800_000,
            "Jurassic Park is a 1993 American science fiction film.",
        ),
        led("Okinawa Prefecture", 300_000, "Okinawa Prefecture is the southernmost prefecture of Japan, with a culture of its own."),
    ]);
    let hits = s.search("triassic jurassic cretaceous", 5).unwrap();
    assert_eq!(hits[0].page.title, "Mesozoic", "{:?}", titles(&hits));
    assert!(!hits[0].named && hits[0].score >= MIN_PARTIAL_SCORE);
    assert!(titles(&hits).contains(&"Dinosaur"));
    assert!(!titles(&hits).contains(&"Jurassic Park"));
    assert_eq!(
        titles(&s.search("okinawa culture", 5).unwrap()).first(),
        Some(&"Okinawa Prefecture")
    );
    // One word is a name, not a description.
    assert!(!titles(&s.search("triassic", 5).unwrap()).contains(&"Mesozoic"));
}

#[test]
fn a_word_said_only_in_another_form_with_its_stem_is_another_word() {
    let led = |title: &str, views: u64, description: &str, lead: &str| {
        let mut page = page(title, views, &[]);
        page.description = Some(description.into());
        page.lead = Some(lead.into());
        page
    };
    let (_dir, s) = searcher(&[
        led(
            "Thomas Cook Airlines",
            760,
            "Charter and scheduled airline of the United Kingdom (2003-2019)",
            "Thomas Cook Airlines Ltd. was a British charter and scheduled airline \
             headquartered in Manchester, England.",
        ),
        led(
            "Robin Hood's Stone",
            300,
            "Scheduled monument in Liverpool",
            "Robin Hood's Stone is a standing stone in Liverpool, England.",
        ),
        led(
            "2027 Rugby World Cup",
            9_000,
            "Eleventh edition of the Rugby World Cup",
            "The 2027 Rugby World Cup is scheduled to be the eleventh Rugby World Cup.",
        ),
        led(
            "Fixture list",
            2_000,
            "List of the matches a league plays",
            "A fixture list gives the schedules of the Premier League's clubs.",
        ),
    ]);
    // "scheduled" has the stem of "schedule" but is another word, in a
    // lead, a description, or beside a name with the query's words.
    for query in [
        "manchester united schedule",
        "liverpool schedule",
        "world cup schedule",
    ] {
        assert_eq!(
            titles(&s.search(query, 5).unwrap()),
            Vec::<&str>::new(),
            "{query}"
        );
    }
    // The word itself is said in the singular or the plural.
    assert_eq!(
        titles(&s.search("premier league schedule", 5).unwrap()),
        ["Fixture list"]
    );
    assert_eq!(
        titles(&s.search("rugby world cup scheduled", 5).unwrap()),
        ["2027 Rugby World Cup"]
    );
}

#[test]
fn words_are_only_looked_up() {
    let word = |title: &str, description: &str, views: u64| {
        Page::from_word(Article {
            title: title.into(),
            description: Some(description.into()),
            views,
            ..Article::default()
        })
    };
    let (_dir, s) = searcher(&[
        page("Free (album)", 1_000, &[]),
        word("free", "(adjective) Unconstrained.", 900),
        word("Free", "(noun) A surname.", 5),
        word("AWOL", "(adjective) Absent without leave.", 50),
        word(
            "aka",
            "(noun) The New Zealand vine Metrosideros fulgens.",
            1,
        ),
        word("AKA", "(preposition) Initialism of also known as.", 12),
    ]);
    // Never a result.
    assert_eq!(titles(&s.search("free", 5).unwrap()), ["Free (album)"]);
    let free = s.definition("free").unwrap().unwrap();
    assert_eq!(
        free.description.as_deref(),
        Some("(adjective) Unconstrained.")
    );
    assert_eq!(free.url, "https://en.wiktionary.org/wiki/free");
    assert_eq!(s.definition("awol").unwrap().unwrap().title, "AWOL");
    // Lower case says nothing of capitals: the best-known form.
    assert_eq!(s.definition("aka").unwrap().unwrap().title, "AKA");
    assert_eq!(s.definition("Free").unwrap().unwrap().title, "Free");
    assert_eq!(s.definition("freedom").unwrap(), None);
}

#[test]
fn what_happened_to_a_person_names_its_article() {
    let (_dir, s) = searcher(&[
        page("Abraham Lincoln", 900_000, &["Lincoln"]),
        page("Assassination of Abraham Lincoln", 300_000, &[]),
        page("Lincoln, Nebraska", 100_000, &[]),
        page("Death of Diana, Princess of Wales", 200_000, &[]),
        page("Murder of George Floyd", 200_000, &[]),
        page("Battle of Lincoln", 5_000, &[]),
    ]);
    let named = |q: &str| -> Vec<String> {
        let hits = s.search(q, 5).unwrap();
        hits.iter()
            .filter(|hit| hit.named)
            .map(|hit| hit.page.title.clone())
            .collect()
    };
    assert_eq!(
        named("lincoln assassination"),
        ["Assassination of Abraham Lincoln"]
    );
    assert_eq!(
        named("princess diana death"),
        ["Death of Diana, Princess of Wales"]
    );
    assert_eq!(named("george floyd murder"), ["Murder of George Floyd"]);
    // Only what happens to a person, and only with the name.
    assert!(named("lincoln battle").is_empty());
    assert_eq!(named("lincoln"), ["Abraham Lincoln"]);
}

#[test]
fn what_is_asked_about_a_person_finds_their_article() {
    let (_dir, s) = searcher(&[
        person("Barack Obama", 900_000, &["Obama"]),
        person("Pablo Picasso", 400_000, &["Picasso"]),
        page("Picasso (crater)", 9_000, &[]),
        person("Steve Jobs", 700_000, &[]),
        person("Black (singer)", 20_000, &[]),
        page("Black Death", 300_000, &[]),
        page("Christmas", 500_000, &[]),
    ]);
    let named = |q: &str| named_titles(&s, q);
    assert_eq!(named("obama daughters"), ["Barack Obama"]);
    assert_eq!(named("picasso paintings"), ["Pablo Picasso"]);
    assert_eq!(named("steve jobs cause of death"), ["Steve Jobs"]);
    // With a possessive, or as a question.
    assert_eq!(named("obama's daughters"), ["Barack Obama"]);
    assert_eq!(named("who are obama's parents"), ["Barack Obama"]);
    assert_eq!(named("picasso's paintings"), ["Pablo Picasso"]);
    assert_eq!(named("how did steve jobs die"), ["Steve Jobs"]);
    assert_eq!(named("what did steve jobs die of"), ["Steve Jobs"]);
    assert_eq!(named("how many kids does obama have"), ["Barack Obama"]);
    // As "who is picasso" would, the crater too, below him.
    assert_eq!(
        named("what is picasso known for"),
        ["Pablo Picasso", "Picasso (crater)"]
    );
    assert_eq!(named("why was steve jobs famous"), ["Steve Jobs"]);
    assert_eq!(named("what is picasso's real name"), ["Pablo Picasso"]);
    assert_eq!(named("where is steve jobs buried"), ["Steve Jobs"]);
    assert_eq!(
        hinted_name("einstein's biography"),
        Some(("einstein".into(), Hint::Any))
    );
    assert_eq!(
        hinted_name("what is mcdonald's"),
        Some(("mcdonald's".into(), Hint::Any))
    );
    assert_eq!(
        hinted_name("mcdonald's wikipedia"),
        Some(("mcdonald's".into(), Hint::Article))
    );
    // Only a person, and not when a page is called the whole query.
    assert_eq!(named("black death"), ["Black Death"]);
    assert!(named("christmas songs").is_empty());
    assert_eq!(
        hinted_name("Lebron James kids"),
        Some(("lebron james".into(), Hint::Person))
    );
}

#[test]
fn a_slip_in_a_well_read_name_is_corrected() {
    let (_dir, s) = searcher(&[
        page("Jeff Bezos", 400_000, &[]),
        page("Leonardo da Vinci", 90_000, &[]),
        page("Stephen Hawking", 45_000, &[]),
        page("Stephen Hawkins", 48, &[]),
        page("IPhone 16", 60_000, &[]),
        page("Bill Gates", 300_000, &[]),
        page("Serena Williams", 70_000, &[]),
        page("Jean Meeus", 80, &[]),
        page("Bezos family", 200, &[]),
    ]);
    let suggest = |query: &str| {
        s.suggest_spelling(query, None, &|_| false)
            .unwrap()
            .map(|s| s.query)
    };
    assert_eq!(suggest("jeff bezoz").as_deref(), Some("jeff bezos"));
    assert_eq!(
        suggest("serena wiliams").as_deref(),
        Some("serena williams")
    );
    // The first words, with the rest after them.
    assert_eq!(
        suggest("leonardo da vinchi net worth").as_deref(),
        Some("leonardo da vinci net worth")
    );
    // A name as typed is no slip, however much more another is read.
    assert_eq!(suggest("stephen hawkins"), None);
    assert_eq!(suggest("jeff bezos"), None);
    // Nor are other numbers, other numbers of a word, or a little-read
    // article's name.
    assert_eq!(suggest("iphone 15"), None);
    assert_eq!(suggest("bill gate"), None);
    assert_eq!(suggest("jean meeuz"), None);
}

#[test]
fn comparisons_find_the_articles_on_both_sides() {
    for (query, a, b) in [
        ("mitosis vs meiosis", "mitosis", "meiosis"),
        ("Virus vs. bacteria?", "virus", "bacteria"),
        (
            "physical therapy versus occupational therapy",
            "physical therapy",
            "occupational therapy",
        ),
        (
            "what is the difference between a crocodile and an alligator",
            "crocodile",
            "alligator",
        ),
        (
            "difference between weather and climate",
            "weather",
            "climate",
        ),
        ("is a tomato a fruit", "tomato", "fruit"),
        ("is the sun a star", "sun", "star"),
        ("is pluto a planet", "pluto", "planet"),
    ] {
        assert_eq!(compared(query), Some([a.into(), b.into()]), "{query:?}");
    }
    for query in [
        "mitosis",
        "vs code",
        "batman v superman",
        "how to choose a roth ira vs a traditional ira",
        "difference between",
        "is it a good idea",
        "is there a god",
        "is a virus alive",
        "is bitcoin a good investment",
        "is it safe to eat a raw egg",
    ] {
        assert_eq!(compared(query), None, "{query:?}");
    }
    let (_dir, s) = searcher(&[
        page("Mitosis", 300_000, &[]),
        page("Meiosis", 250_000, &[]),
        page("Mitosis (blockchain)", 1_000, &[]),
        page("Cell cycle", 200_000, &[]),
    ]);
    let hits = s.search("mitosis vs meiosis", 5).unwrap();
    let named: Vec<_> = hits
        .iter()
        .filter(|hit| hit.named)
        .map(|hit| hit.page.title.as_str())
        .collect();
    assert_eq!(named[..2], ["Mitosis", "Meiosis"], "{hits:?}");
    // A page that compares them already is what is asked for.
    let (_dir, s) = searcher(&[
        page("Crocodile", 300_000, &[]),
        page("Alligator", 250_000, &[]),
        page("Crocodiles vs. Alligators", 50_000, &[]),
    ]);
    let hits = s.search("crocodile vs alligator", 5).unwrap();
    assert_eq!(hits[0].page.title, "Crocodiles vs. Alligators", "{hits:?}");
    assert!(hits.iter().all(|hit| !hit.named), "{hits:?}");
    let (_dir, s) = searcher(&[page("Tomato", 300_000, &[]), page("Fruit", 250_000, &[])]);
    let hits = s.search("is a tomato a fruit", 5).unwrap();
    let mut named: Vec<_> = hits
        .iter()
        .filter(|hit| hit.named)
        .map(|hit| hit.page.title.as_str())
        .collect();
    named.sort();
    assert_eq!(named, ["Fruit", "Tomato"], "{hits:?}");
}

#[test]
fn a_page_on_one_use_of_both_sides_does_not_compare_them() {
    let question = |title: &str, tags: &str, item: &str| {
        Page::from_question(Article {
            title: title.into(),
            description: Some(tags.into()),
            item: Some(item.into()),
            views: 100_000,
            ..Article::default()
        })
    };
    let (_dir, s) = searcher(&[
        page("Solid-state drive", 300_000, &["SSD"]),
        page("Hard disk drive", 250_000, &["HDD"]),
        question("Power consumption: SSD vs HDD", "ssd, power", "1"),
        question("SSD vs HDD for databases", "database, ssd", "2"),
    ]);
    let hits = s.search("ssd vs hdd", 5).unwrap();
    let named: Vec<_> = hits
        .iter()
        .filter(|hit| hit.named)
        .map(|hit| hit.page.title.as_str())
        .collect();
    assert_eq!(named, ["Solid-state drive", "Hard disk drive"], "{hits:?}");
    // A question that asks only the comparison is one.
    let (_dir, s) = searcher(&[
        page("Mitosis", 300_000, &[]),
        page("Meiosis", 250_000, &[]),
        question(
            "What is the difference between mitosis and meiosis?",
            "cell-biology, mitosis",
            "3",
        ),
    ]);
    let hits = s
        .search("difference between mitosis and meiosis", 5)
        .unwrap();
    assert!(hits[0].page.is_question(), "{hits:?}");
    assert!(hits.iter().all(|hit| !hit.named), "{hits:?}");
}

#[test]
fn one_side_of_a_thing_finds_its_article_unless_one_is_called_so() {
    for (query, name) in [
        ("benefits of meditation", "meditation"),
        ("uses of graphene", "graphene"),
        ("Causes of the French Revolution?", "french revolution"),
        (
            "advantages and disadvantages of nuclear energy",
            "nuclear energy",
        ),
    ] {
        assert_eq!(aspect_asked(query).as_deref(), Some(name), "{query:?}");
    }
    assert_eq!(aspect_asked("meditation"), None);
    assert_eq!(aspect_asked("benefits of it"), None);
    let (_dir, s) = searcher(&[
        page("Meditation", 300_000, &[]),
        page("Meditation (album)", 2_000, &[]),
        page("French Revolution", 900_000, &[]),
        page("Causes of the French Revolution", 30_000, &[]),
    ]);
    let hits = s.search("benefits of meditation", 5).unwrap();
    assert_eq!(hits[0].page.title, "Meditation", "{hits:?}");
    assert!(hits[0].named, "{hits:?}");
    // The article on that side is called the whole query, and the more
    // read article on the whole thing is not looked up to come before it.
    let hits = s.search("causes of the french revolution", 5).unwrap();
    assert_eq!(
        hits[0].page.title, "Causes of the French Revolution",
        "{hits:?}"
    );
    assert!(
        hits.iter()
            .all(|hit| !hit.named || hit.page.title != "French Revolution"),
        "{hits:?}"
    );
}

#[test]
fn a_plural_that_names_only_namesakes_asks_about_its_singular() {
    let (_dir, s) = searcher(&[
        page("Cloud", 300_000, &[]),
        page("Clouds (2020 film)", 5_000, &[]),
        page("Clouds (Joni Mitchell album)", 8_000, &[]),
        page("Solar panel", 100_000, &[]),
        page("Python (programming language)", 400_000, &[]),
        page("Python (genus)", 20_000, &[]),
        page("Pythonidae", 30_000, &["Pythons"]),
    ]);
    let named = |query: &str| -> Vec<String> {
        s.search(query, 5)
            .unwrap()
            .into_iter()
            .filter(|hit| hit.named)
            .map(|hit| hit.page.title)
            .collect()
    };
    assert_eq!(named("what are clouds")[0], "Cloud");
    assert_eq!(named("what are solar panels"), ["Solar panel"]);
    // A singular asks about what it names, qualified or not.
    let python = named("what is python");
    assert!(!python.contains(&"Pythonidae".to_string()), "{python:?}");
    let pythons = named("what is a ball python");
    assert!(!pythons.contains(&"Pythonidae".to_string()), "{pythons:?}");
}

#[test]
fn a_monarch_typed_with_a_number_is_named() {
    let mut book = page("Henry the Eighth", 3, &[]);
    book.set = BOOKS_SET.into();
    let (_dir, s) = searcher(&[
        person("Henry VIII", 85_702, &["Henry 8"]),
        page("Henry VIII (play)", 1_900, &[]),
        person("Henry Percy, 8th Earl of Northumberland", 300, &[]),
        book,
        person("Louis XIV", 39_618, &[]),
        person("Charles III", 120_000, &["King Charles III"]),
        person("Pope Benedict XVI", 30_000, &[]),
        page("Queen Elizabeth 2", 6_000, &["QE2"]),
        person("Elizabeth II", 200_000, &["Queen Elizabeth II"]),
        page("Epistle of James", 2_302, &["James 1"]),
        page("Windows 11", 50_000, &[]),
    ]);
    let named = |q: &str| named_titles(&s, q);
    for q in ["henry 8th", "henry the 8th", "henry the eighth", "henry 8"] {
        assert_eq!(named(q)[0], "Henry VIII", "{q}");
    }
    assert_eq!(named("louis the 14th"), ["Louis XIV"]);
    assert_eq!(named("charles 3"), ["Charles III"]);
    assert_eq!(named("king charles 3"), ["Charles III"]);
    assert_eq!(named("pope benedict 16"), ["Pope Benedict XVI"]);
    assert_eq!(named("elizabeth the 2nd"), ["Elizabeth II"]);
    // Not when an article is called the query as typed.
    assert_eq!(named("queen elizabeth 2"), ["Queen Elizabeth 2"]);
    assert_eq!(named("james 1"), ["Epistle of James"]);
    assert_eq!(
        regnal_name("Louis the Fourteenth"),
        Some("louis xiv".into())
    );
    assert_eq!(regnal_name("pope pius 12"), Some("pope pius xii".into()));
    assert_eq!(regnal_name("windows 11 pro"), None);
    assert_eq!(regnal_name("the 8th"), None);
    assert_eq!(regnal_name("covid-19"), None);
}

#[test]
fn what_is_asked_about_an_event_finds_its_article() {
    let described = |title: &str, aliases: &[&str], description: &str| {
        let mut page = page(title, 50_000, aliases);
        page.description = Some(description.into());
        page
    };
    let (_dir, s) = searcher(&[
        described(
            "UEFA Champions League",
            &["Champions League"],
            "European association football tournament",
        ),
        described(
            "Los Angeles Lakers",
            &["Lakers"],
            "National Basketball Association team",
        ),
        described(
            "Coachella Valley Music and Arts Festival",
            &["Coachella"],
            "Annual music and arts festival in Indio, California",
        ),
        described(
            "2026 FIFA World Cup",
            &["2026 World Cup"],
            "Football tournament in North America",
        ),
        described("Glastonbury", &[], "Town in Somerset, England"),
        described("Bus", &[], "Large road vehicle for carrying passengers"),
        described("Truth", &[], "Being in accord with fact or reality"),
        described("Truth table", &[], "Mathematical table used in logic"),
    ]);
    let named = |q: &str| -> Vec<String> {
        let hits = s.search(q, 5).unwrap();
        hits.iter()
            .filter(|hit| hit.named)
            .map(|hit| hit.page.title.clone())
            .collect()
    };
    assert_eq!(
        named("champions league schedule"),
        ["UEFA Champions League"]
    );
    assert_eq!(named("lakers live scores"), ["Los Angeles Lakers"]);
    assert_eq!(
        named("coachella lineup"),
        ["Coachella Valley Music and Arts Festival"]
    );
    assert_eq!(named("world cup 2026 tickets"), ["2026 FIFA World Cup"]);
    // Only an event, team or venue, and not when a page is called the
    // whole query.
    assert!(named("glastonbury lineup").is_empty());
    assert!(named("bus schedule").is_empty());
    assert_eq!(named("truth table"), ["Truth table"]);
}

#[test]
fn what_is_asked_about_a_drug_finds_its_article() {
    let drug = |title: &str, aliases: &[&str], description: &str, lead: Option<&str>| {
        let mut page = page(title, 50_000, aliases);
        page.description = Some(description.into());
        page.lead = lead.map(str::to_string);
        page
    };
    let (_dir, s) = searcher(&[
        drug(
            "Memantine",
            &["Namenda"],
            "Medication used to treat Alzheimer's disease",
            None,
        ),
        // Only the lead says it is a medication.
        drug(
            "Gabapentin",
            &["Neurontin"],
            "Gamma-aminobutyric acid analog",
            Some("Gabapentin, sold under the brand name Neurontin among others, is an anticonvulsant medication used to treat neuropathic pain. It is taken by mouth."),
        ),
        drug("Alprazolam", &["Xanax"], "Benzodiazepine medication", None),
        drug("Report", &[], "Document that presents information", None),
        drug("Opioid", &[], "Class of analgesic drug", None),
        drug(
            "Alcohol (chemistry)",
            &[],
            "Organic compound with a hydroxyl group",
            None,
        ),
        drug("Child", &[], "Human between birth and puberty", None),
        drug(
            "Child abuse",
            &[],
            "Maltreatment of a child",
            None,
        ),
        drug(
            "Opioid withdrawal",
            &[],
            "Symptoms after stopping opioids",
            None,
        ),
    ]);
    let named = |q: &str| named_titles(&s, q);
    assert_eq!(named("memantine abuse"), ["Memantine"]);
    assert_eq!(named("namenda misuse"), ["Memantine"]);
    assert_eq!(named("gabapentin withdrawal symptoms"), ["Gabapentin"]);
    assert_eq!(named("xanax addiction"), ["Alprazolam"]);
    assert_eq!(named("what is memantine abuse"), ["Memantine"]);
    assert_eq!(named("memantine abuse treatment"), ["Memantine"]);
    assert_eq!(
        drug_asked("Gabapentin withdrawal symptoms?"),
        Some("gabapentin".into())
    );
    // Only a drug, and not when a page is named otherwise or the name is a
    // kind of drug.
    assert!(named("report abuse").is_empty());
    assert!(named("alcohol addiction").is_empty());
    assert!(named("opioid addiction").is_empty());
    assert_eq!(named("child abuse"), ["Child abuse"]);
    assert_eq!(named("what is child abuse"), ["Child abuse"]);
    assert_eq!(named("opioid withdrawal symptoms"), ["Opioid withdrawal"]);
}
