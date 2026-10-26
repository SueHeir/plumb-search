//! Typos: what is corrected, what is only suggested and what is left alone.

use super::*;

/// Sites whose titles teach the spelling model: "amtrak" in many,
/// "amtrack" in some (enough to be a known word), "canon" and "grand
/// canyon" each in some, and a few slips of other words.
fn spelling_corpus() -> Vec<Item> {
    let mut records = typo_corpus();
    records.push(site(
        "amtrak.com",
        Some("Amtrak"),
        None,
        &["Amtrak"],
        &[],
        popular(700, 15_000),
    ));
    let titles = [
        ("Amtrak station", 450),
        ("Amtrack fan page", 20),
        ("Grand Canyon tours", 25),
        ("Canon camera repair", 25),
        ("Grand hotel", 25),
        ("Hockey club", 30),
        ("Hocky club", 2),
        // Two more of each slip, so the error model learns them.
        ("Kodak camera", 25),
        ("Kodack camera", 2),
        ("Batik prints", 25),
        ("Batick prints", 2),
        ("Turkey travel", 25),
        ("Turky travel", 2),
        ("Jockey silks", 25),
        ("Jocky silks", 2),
        ("Perf1 benchmark", 80),
        ("Perft results", 1),
        ("Erft cycling", 25),
        ("Perf tools", 25),
        ("Ikala karaoke", 80),
        ("Inkala sudoku", 3),
        ("Kanye West albums", 40),
        ("Kayne Anderson capital", 22),
        ("Kimipet supplies", 10),
    ];
    let mut n = 0;
    for (title, count) in titles {
        for i in 0..count {
            n += 1;
            records.push(site(
                &format!("spelling{n}.com"),
                Some(&format!("{title} {i}")),
                None,
                &[],
                &[],
                obscure(5_000_000 + n, 5),
            ));
        }
    }
    // A popular site whose name is one letter from "perft".
    records.push(site(
        "perf1.com",
        Some("Perf1"),
        None,
        &["Perf1"],
        &[],
        popular(20_000, 2_000),
    ));
    records
}

fn spelled_with(searcher: &Searcher, query: &str, cfg: &RankConfig) -> Option<String> {
    searcher
        .search_full(query, 10, cfg, &SearchOptions::default())
        .unwrap()
        .spelling
        .map(|s| s.query)
}

#[test]
fn indexes_learn_spelling_from_their_words() {
    let (_dir, searcher) = build(&spelling_corpus());
    let model = searcher.spelling_model().expect("a spelling model");
    assert!(model.count("amtrak") >= 450);
    assert!(model.count("amtrack") >= 20);
    // "amtrack" is ten times rarer than "amtrak", one edit away, so a
    // `k` typed `ck` is a learned slip; so is a dropped `e`.
    let learned = model.ln_channel("amtrack", "amtrak");
    let unseen = model.ln_channel("amtrxk", "amtrak");
    assert!(learned > unseen, "{learned} > {unseen}");
    assert!(model.ln_channel("hocky", "hockey") > model.ln_channel("hockqy", "hockey"));
    assert!(model.ln_score(Some("grand"), "canyon") > model.ln_score(Some("grand"), "canon"));
}

#[test]
fn known_words_are_corrected_only_when_trusted_to() {
    let (_dir, searcher) = build(&spelling_corpus());
    // "amtrack" is found in 20 sites: a known word, left alone by
    // default.
    assert_eq!(
        spelled_with(&searcher, "amtrack", &RankConfig::default()),
        None
    );
    let trusting = RankConfig {
        real_word_weight: Some(2.0),
        ..RankConfig::default()
    };
    assert_eq!(
        spelled_with(&searcher, "amtrack", &trusting).as_deref(),
        Some("amtrak")
    );
    // Words commoner than anything near them stay.
    assert_eq!(spelled_with(&searcher, "pizza", &trusting), None);
    assert_eq!(spelled_with(&searcher, "canon", &trusting), None);
}

#[test]
fn rare_terms_that_are_meant_stay() {
    let (_dir, searcher) = build(&spelling_corpus());
    // A word without digits is no slip of one with them, and a word
    // one site says is no slip of a common word with another first
    // letter ("erft").
    assert_eq!(
        spelled_with(&searcher, "perft", &RankConfig::default()),
        None
    );
    // Three sites say "inkala": far fewer than say "ikala", but too many
    // for a slip as unlikely as a dropped letter.
    assert_eq!(
        spelled_with(&searcher, "inkala", &RankConfig::default()),
        None
    );
    assert_eq!(
        spelled_with(&searcher, "inkala sudoku", &RankConfig::default()),
        None
    );
    // Nor is it a slip of a popular site's name with a digit in it.
    assert_eq!(
        spelled_with(&searcher, "perft results", &RankConfig::default()),
        None
    );
    // A word no site says is no likelier a slip of one only a few
    // sites say: "kimipet" is no known word.
    assert_eq!(
        spelled_with(&searcher, "kiwipete perft position", &RankConfig::default()),
        None
    );
    // A word no site says counts as if one did: dropping a `t` from
    // "perf", which only a few dozen sites say, is the rarer reading.
    assert_eq!(
        spelled_with(&searcher, "perft tools", &RankConfig::default()),
        None
    );
}

#[test]
fn words_around_a_word_can_show_it_is_a_slip() {
    let (_dir, searcher) = build(&spelling_corpus());
    assert_eq!(
        spelled_with(&searcher, "grand canon", &RankConfig::default()),
        None
    );
    let trusting = RankConfig {
        context_weight: Some(10.0),
        ..RankConfig::default()
    };
    assert_eq!(
        spelled_with(&searcher, "grand canon", &trusting).as_deref(),
        Some("grand canyon")
    );
    // An artist's name with two letters swapped, where "kayne" is a
    // known word (a surname) on its own.
    assert_eq!(
        spelled_with(&searcher, "kayne west stronger", &RankConfig::default()),
        None
    );
    assert_eq!(
        spelled_with(&searcher, "kayne west stronger", &trusting).as_deref(),
        Some("kanye west stronger")
    );
    // Nothing has the words as typed, and the spelling finds sites
    // with all of them: its results are shown.
    let results = searcher
        .search_full(
            "kayne west albums",
            10,
            &trusting,
            &SearchOptions::default(),
        )
        .unwrap();
    let spelling = results.spelling.expect("a spelling");
    assert_eq!(spelling.query, "kanye west albums");
    assert!(spelling.applied);
    assert!(results.hits[0]
        .title
        .as_deref()
        .unwrap_or("")
        .starts_with("Kanye West albums"));
    // Alone, "canon" is a word like any other.
    assert_eq!(spelled_with(&searcher, "canon", &trusting), None);
    assert_eq!(spelled_with(&searcher, "canon camera", &trusting), None);
}

fn search_spelled(searcher: &Searcher, query: &str) -> SearchResults {
    searcher
        .search_full(query, 10, &RankConfig::default(), &SearchOptions::default())
        .unwrap()
}

/// The query suggested, whatever site comes with it.
fn suggested(query: &str) -> Option<String> {
    Some(query.to_string())
}

#[test]
fn misspelled_names_find_the_site() {
    let (_dir, searcher) = build(&typo_corpus());
    for (typed, fixed, domain) in [
        ("amazom", "amazon", "amazon.com"),
        ("Amazn", "amazon", "amazon.com"),
        ("gooogle", "google", "google.com"),
        ("bank of amercia", "bank of america", "bankofamerica.com"),
        ("bank of americ", "bank of america", "bankofamerica.com"),
    ] {
        let results = search_spelled(&searcher, typed);
        assert_eq!(suggestion(&results), suggested(fixed), "{typed}");
        // The suggestion finds the site.
        let fixed_hits = search_spelled(&searcher, fixed).hits;
        assert_eq!(fixed_hits[0].domain, domain, "{typed}");
    }
    // A word's typo is fixed to the nearest common word. With the
    // learned model, "forecast" would have to be in far more sites than
    // this small index has to outweigh an edit it never saw.
    let by_edits = RankConfig {
        spelling_channel: false,
        ..RankConfig::default()
    };
    assert_eq!(
        spelled_with(&searcher, "weather forcast", &by_edits).as_deref(),
        Some("weather forecast")
    );
    assert_eq!(
        search_spelled(&searcher, "weather forecast").hits[0].domain,
        "weather.com"
    );
    // A word typed with letters English does not use stays as typed
    // ("Edlén" is not "ellen"); the others are still corrected.
    assert_eq!(spelled_with(&searcher, "weather forcást", &by_edits), None);
    assert_eq!(
        spelled_with(&searcher, "wéather forcast", &by_edits).as_deref(),
        Some("weather forecast")
    );
    assert_eq!(suggestion(&search_spelled(&searcher, "amazóm")), None);
}

#[test]
fn typos_never_lead_to_look_alikes() {
    let (_dir, searcher) = build(&typo_corpus());
    // amazen.com is as near "amazn" as amazon.com, but has nothing to
    // show for itself.
    let results = search_spelled(&searcher, "amazn");
    assert_eq!(suggestion(&results), suggested("amazon"));
    // The site the suggestion names is listed, second at most, without
    // correcting the search.
    if let Some(site) = results.spelling.as_ref().and_then(|s| s.site.clone()) {
        let at = domains(&results.hits).iter().position(|d| *d == site);
        assert!(at.is_some_and(|at| at <= 1), "{:?}", domains(&results.hits));
    }
    let fixed_hits = search_spelled(&searcher, "amazon").hits;
    assert_eq!(domains(&fixed_hits)[0], "amazon.com");
}

#[test]
fn misspelled_words_are_not_taken_for_a_site_s_name() {
    let mut records = typo_corpus();
    records.push(site(
        "distance.to",
        Some("Distance"),
        None,
        &["Distance"],
        &[],
        ranked(40_000, 2_000),
    ));
    for i in 0..30 {
        records.push(site(
            &format!("existence{i}.com"),
            Some(&format!("Existence of things {i}")),
            None,
            &[],
            &[],
            obscure(5_000_000 + i, 10),
        ));
        records.push(site(
            &format!("movers{i}.com"),
            Some(&format!("Long distance movers {i}")),
            None,
            &[],
            &[],
            obscure(6_000_000 + i, 10),
        ));
    }
    let (_dir, searcher) = build(&records);
    // distance.to is two edits from "existance", "existence" one.
    let results = search_spelled(&searcher, "existance");
    assert_ne!(suggestion(&results), suggested("distance"));
    assert!(!domains(&results.hits).contains(&"distance.to"));
    // A slip of the name itself still finds the site.
    let results = search_spelled(&searcher, "distanse");
    assert_eq!(suggestion(&results), suggested("distance"));
    assert_eq!(domains(&results.hits)[0], "distance.to");
}

#[test]
fn well_known_names_one_slip_away_are_meant() {
    let mut records = typo_corpus();
    records.push(site(
        "shein.com",
        Some("SHEIN"),
        None,
        &["SHEIN"],
        &[],
        popular(300, 30_000),
    ));
    for i in 0..30 {
        records.push(site(
            &format!("shine{i}.com"),
            Some(&format!("Shine bright {i}")),
            None,
            &[],
            &[],
            obscure(5_000_000 + i, 10),
        ));
    }
    let (_dir, searcher) = build(&records);
    // "shine" is the commoner word, one swap from "shien" too, but
    // shein.com is well known.
    let results = search_spelled(&searcher, "shien");
    assert_eq!(suggestion(&results), suggested("shein"));
    assert!(domains(&results.hits).contains(&"shein.com"));
}

#[test]
fn a_likelier_slip_beats_a_well_known_name() {
    let mut records = typo_corpus();
    for (domain, name) in [("wired.com", "WIRED"), ("trier.de", "Trier")] {
        records.push(site(
            domain,
            Some(name),
            None,
            &[name],
            &[],
            popular(300, 30_000),
        ));
    }
    // The sites teach that "ei" is often typed "ie".
    for i in 0..30 {
        records.push(site(
            &format!("weird{i}.com"),
            Some(&format!(
                "Weird facts: their receive, deceive and ceiling {i}"
            )),
            None,
            &[],
            &[],
            obscure(5_000_000 + i, 10),
        ));
    }
    for (i, slip) in ["recieve", "decieve", "cieling"].into_iter().enumerate() {
        records.push(site(
            &format!("{slip}.net"),
            Some(&format!("How to {slip}")),
            None,
            &[],
            &[],
            obscure(6_000_000 + i as u64, 10),
        ));
    }
    let (_dir, searcher) = build(&records);
    // "wired" is one swap from "wierd" too, and wired.com well known, but
    // "ei" typed "ie" is the likelier slip.
    let results = search_spelled(&searcher, "wierd");
    assert_ne!(suggestion(&results), suggested("wired"));
    // Nor is "thier" Trier: a letter changed is a rarer slip.
    let results = search_spelled(&searcher, "thier");
    assert_ne!(suggestion(&results), suggested("trier"));
}

#[test]
fn exact_names_are_not_corrected() {
    let (_dir, searcher) = build(&typo_corpus());
    for query in [
        "amazon",
        "google",
        "bank of america",
        "piazza",
        "amazon.com",
    ] {
        let results = search_spelled(&searcher, query);
        assert_eq!(results.spelling, None, "{query}");
    }
    // A little-known site named exactly is searched as typed, with
    // the far better-known site a letter away offered instead.
    let results = search_spelled(&searcher, "gogle");
    assert_eq!(suggestion(&results), suggested("google"));
    assert_eq!(results.hits[0].domain, "gogle.com");
    // Searching exactly finds it with no suggestion.
    let options = SearchOptions {
        exact: true,
        ..SearchOptions::default()
    };
    let hits = searcher
        .search_full("gogle", 10, &RankConfig::default(), &options)
        .unwrap()
        .hits;
    assert_eq!(hits[0].domain, "gogle.com");
    // A site with popularity of its own keeps its name, though a more
    // popular one is a letter away.
    let results = search_spelled(&searcher, "hilten");
    assert_eq!(results.spelling, None);
    assert_eq!(results.hits[0].domain, "hilten.com");
}

#[test]
fn a_site_named_by_every_word_keeps_its_name() {
    let mut records = typo_corpus();
    records.extend([
        site(
            "linux.org",
            Some("Linux"),
            None,
            &["Linux"],
            &[],
            popular(500, 20_000),
        ),
        site(
            "linustechtips.com",
            Some("Forums - Linus Tech Tips"),
            None,
            &[],
            &[],
            obscure(900_000, 30),
        ),
    ]);
    for i in 0..25 {
        records.push(site(
            &format!("linux{i}.com"),
            Some(&format!("Linux tips {i}")),
            None,
            &[],
            &[],
            obscure(5_000_000 + i, 10),
        ));
    }
    let (_dir, searcher) = build(&records);
    // "linus" is a letter from the well-known "linux", but the whole
    // query names a site: only its first word would be corrected.
    let results = search_spelled(&searcher, "linus tech tips");
    assert_eq!(results.spelling, None);
    assert_eq!(results.hits[0].domain, "linustechtips.com");
}

#[test]
fn several_word_names_and_first_letters() {
    let (_dir, searcher) = build(&typo_corpus());
    for (typed, fixed, domain) in [
        // "capitol" and "taco" are words the index knows, and "bel" is
        // short, but together they are a typo of a well-known name.
        ("capitol one", "capital one", "capitalone.com"),
        ("taco bel", "taco bell", "tacobell.com"),
        // edx.org and fedex.com are both an edit away; typos rarely
        // change the first letter.
        ("fedx", "fedex", "fedex.com"),
    ] {
        let results = search_spelled(&searcher, typed);
        assert_eq!(suggestion(&results), suggested(fixed), "{typed}");
        // The suggestion finds the site.
        let fixed_hits = search_spelled(&searcher, fixed).hits;
        assert_eq!(fixed_hits[0].domain, domain, "{typed}");
    }
}

#[test]
fn spelled_right_words_get_no_suggestion() {
    let mut records = typo_corpus();
    records.extend([
        site(
            "ulta.com",
            Some("Ulta Beauty"),
            None,
            &[],
            &[],
            popular(800, 15_000),
        ),
        site(
            "lyft.com",
            Some("Lyft"),
            None,
            &[],
            &[],
            popular(700, 15_000),
        ),
        site(
            "syft.com",
            Some("Syft"),
            None,
            &[],
            &[],
            ranked(30_000, 2_000),
        ),
        site(
            "numbeo.com",
            Some("Numbeo"),
            None,
            &[],
            &[],
            popular(2_000, 9_000),
        ),
        site(
            "whattf.com",
            Some("What TF"),
            None,
            &[],
            &[],
            popular(2_500, 9_000),
        ),
    ]);
    for i in 0..25 {
        records.push(site(
            &format!("alta{i}.com"),
            Some(&format!("Alta ski lodge {i}")),
            None,
            &[],
            &[],
            obscure(7_000_000 + i, 10),
        ));
        records.push(site(
            &format!("words{i}.com"),
            Some(&format!("The number of things to do {i}")),
            None,
            &[],
            &[],
            obscure(8_000_000 + i, 10),
        ));
        records.push(site(
            &format!("outlets{i}.com"),
            Some(&format!("Outlet store {i}")),
            None,
            &[],
            &[],
            obscure(9_000_000 + i, 10),
        ));
    }
    let (_dir, searcher) = build(&records);
    for query in [
        // A popular site's name is what was meant, wherever it is.
        "where to buy ulta",
        "lyft app",
        // Known short words don't bend into a name: not "numbe o
        // senators", not "what tf bring".
        "number of senators",
        "what to bring",
        // Numbers and versions aren't typos.
        "240v outlet",
    ] {
        assert_eq!(search_spelled(&searcher, query).spelling, None, "{query}");
    }
    // A real typo still gets one.
    assert_eq!(
        suggestion(&search_spelled(&searcher, "amazom")),
        suggested("amazon")
    );
}

#[test]
fn a_site_s_name_typed_stays_unless_a_far_better_known_one_is_meant() {
    let mut records = typo_corpus();
    records.extend([
        site(
            "plagiarisma.net",
            Some("Plagiarisma"),
            Some("Free plagiarism checker online"),
            &[],
            &[],
            ranked(200_000, 300),
        ),
        site(
            "plagiarismcheck.org",
            Some("Plagiarism Checker"),
            None,
            &["Plagiarism Checker"],
            &[],
            ranked(60_000, 1_500),
        ),
    ]);
    let (_dir, searcher) = build(&records);
    let score = |domain: &str| searcher.site(domain).unwrap().unwrap().link_score;
    let (typed, meant) = (score("plagiarisma.net"), score("plagiarismcheck.org"));
    assert!(
        typed < meant && meant < typed + crate::search::TYPO_POPULARITY_MARGIN,
        "{typed} {meant}"
    );
    let results = search_spelled(&searcher, "plagiarisma checker");
    assert_eq!(results.spelling, None);
    assert_eq!(domains(&results.hits)[0], "plagiarisma.net");
    // A slip of the better-known name is still one.
    assert_eq!(
        suggestion(&search_spelled(&searcher, "plagiarism chekcer")),
        suggested("plagiarism checker")
    );
}

#[test]
fn a_known_word_that_names_a_site_is_still_a_slip_of_a_better_known_name() {
    let mut records = typo_corpus();
    records.push(site(
        "capitol.example",
        Some("Capitol"),
        None,
        &[],
        &[],
        ranked(2_000, 5_000),
    ));
    let (_dir, searcher) = build(&records);
    let score = |domain: &str| searcher.site(domain).unwrap().unwrap().link_score;
    let (typed, meant) = (score("capitol.example"), score("capitalone.com"));
    assert!(
        typed < meant && meant < typed + crate::search::TYPO_POPULARITY_MARGIN,
        "{typed} {meant}"
    );
    assert_eq!(
        suggestion(&search_spelled(&searcher, "capitol one")),
        suggested("capital one")
    );
}

#[test]
fn known_words_and_short_words_are_left_alone() {
    let (_dir, searcher) = build(&typo_corpus());
    // piazza.com is a letter away, but pizza is a word the index knows.
    let results = search_spelled(&searcher, "pizza");
    assert_eq!(results.spelling, None);
    assert!(results.hits[0].domain.starts_with("pizzeria"));
    // Too short to tell a typo from another name.
    assert_eq!(search_spelled(&searcher, "gle").spelling, None);
}

#[test]
fn exact_searches_skip_correction() {
    let (_dir, searcher) = build(&typo_corpus());
    let options = SearchOptions {
        exact: true,
        ..SearchOptions::default()
    };
    let results = searcher
        .search_full("amazom", 10, &RankConfig::default(), &options)
        .unwrap();
    assert_eq!(results.spelling, None);
    assert!(!domains(&results.hits).contains(&"amazon.com"));
}

#[test]
fn plurals_find_singulars_but_never_outweigh_the_words_typed() {
    let mut records = vec![
        site(
            "vimeo.com",
            Some("Vimeo"),
            Some("The all-in-one video platform"),
            &[],
            &[],
            ranked(150, 40_000),
        ),
        site(
            "clips.example",
            Some("Clips"),
            Some("Short videos"),
            &[],
            &[],
            ranked(150, 40_000),
        ),
    ];
    // Many sites have "videos", so "video" is the rarer word.
    for i in 0..30 {
        records.push(site(
            &format!("v{i}.example"),
            Some("Free videos"),
            Some("Free videos to watch"),
            &[],
            &[],
            obscure(5_000_000 + i, 2),
        ));
    }
    let (_dir, searcher) = build(&records);
    let hits = searcher.search("videos", 40).unwrap();
    let rank = |domain: &str| domains(&hits).iter().position(|d| *d == domain);
    let (Some(vimeo), Some(clips)) = (rank("vimeo.com"), rank("clips.example")) else {
        panic!("{hits:?}");
    };
    assert!(clips < vimeo, "{hits:?}");
}

#[test]
fn the_most_popular_matches_are_ranked_past_the_bm25_cutoff() {
    let mut youtube = site(
        "youtube.com",
        Some("YouTube"),
        Some("Enjoy the videos and music you love, and share it all with the world"),
        &[],
        &[],
        popular(2, 2_000_000),
    );
    site::set_wikidata_about(
        &mut youtube,
        Some("American online video sharing platform".into()),
    );
    let mut records = vec![youtube];
    for i in 0..30 {
        records.push(site(
            &format!("watch{i}.example"),
            Some("Watch videos online"),
            Some("Watch free videos online"),
            &[],
            &[],
            obscure(5_000_000 + i, 2),
        ));
    }
    let (_dir, searcher) = build(&records);
    let cfg = RankConfig {
        candidates: 10,
        ..RankConfig::default()
    };
    let hits = searcher
        .search_with("watch videos online", 10, &cfg)
        .unwrap();
    assert!(domains(&hits).contains(&"youtube.com"), "{hits:?}");
}

#[test]
fn mistyped_addresses_suggest_the_site_s_address() {
    let (_dir, searcher) = build(&typo_corpus());
    let spelled = |query: &str| {
        let results = search_spelled(&searcher, query);
        let spelling = results.spelling.map(|s| (s.query, s.site));
        let hits = domains(&results.hits)
            .iter()
            .map(|d| d.to_string())
            .collect();
        (spelling, hits)
    };
    let suggests = |domain: &str| Some((domain.to_string(), Some(domain.to_string())));
    // No site has the address typed: the site meant comes first.
    for typed in ["amzon.com", "amazn.com", "wwwamazon.com"] {
        let (spelling, hits): (_, Vec<String>) = spelled(typed);
        assert_eq!(spelling, suggests("amazon.com"), "{typed}");
        assert_eq!(hits[0], "amazon.com", "{typed}");
    }
    // A look-alike that has it stays first, the site meant right under it.
    let (spelling, hits) = spelled("gogle.com");
    assert_eq!(spelling, suggests("google.com"));
    assert_eq!(hits[..2], ["gogle.com", "google.com"]);
    // Addresses of sites known in their own right, or of nothing near
    // a well-known one, get none.
    for typed in ["amazon.com", "hilten.com", "zzqxv.com"] {
        assert_eq!(spelled(typed).0, None, "{typed}");
    }
}
