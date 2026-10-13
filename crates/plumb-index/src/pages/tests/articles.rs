//! Articles found by their names, leads and what they say of themselves.

use super::*;

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
        ("how does a refrigerator work", "refrigerator"),
        ("how do vaccines work", "vaccines"),
        ("how tall is the eiffel tower", "eiffel tower"),
        ("how big is texas", "texas"),
        ("where is machu picchu", "machu picchu"),
        ("how many people live in tokyo", "tokyo"),
    ] {
        assert_eq!(
            hinted_name(query),
            Some((name.into(), Hint::Any)),
            "{query:?}"
        );
    }
    assert_eq!(hinted_name("whats app"), None);
    assert_eq!(hinted_name("how does it work"), None);
    assert_eq!(hinted_name("how long is it"), None);
    assert_eq!(hinted_name("how long does it take to boil an egg"), None);
    assert_eq!(hinted_name("how do i make pancakes"), None);
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
    assert_eq!(s.definition("freedom").unwrap(), None);
}

#[test]
fn what_is_asked_about_a_person_finds_their_article() {
    let person = |title: &str, views: u64, aliases: &[&str]| {
        let mut page = page(title, views, aliases);
        page.facts = vec![plumb_core::facts::Fact {
            kind: plumb_core::facts::FactKind::Born,
            value: "1961-08-04".into(),
        }];
        page
    };
    let (_dir, s) = searcher(&[
        person("Barack Obama", 900_000, &["Obama"]),
        person("Pablo Picasso", 400_000, &["Picasso"]),
        page("Picasso (crater)", 9_000, &[]),
        person("Steve Jobs", 700_000, &[]),
        person("Black (singer)", 20_000, &[]),
        page("Black Death", 300_000, &[]),
        page("Christmas", 500_000, &[]),
    ]);
    let named = |q: &str| -> Vec<String> {
        let hits = s.search(q, 5).unwrap();
        hits.iter()
            .filter(|hit| hit.named)
            .map(|hit| hit.page.title.clone())
            .collect()
    };
    assert_eq!(named("obama daughters"), ["Barack Obama"]);
    assert_eq!(named("picasso paintings"), ["Pablo Picasso"]);
    assert_eq!(named("steve jobs cause of death"), ["Steve Jobs"]);
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
