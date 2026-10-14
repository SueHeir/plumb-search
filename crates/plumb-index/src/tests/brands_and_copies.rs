//! Brands against decoys, pill shops and sign-in pages, and copies of a
//! site on other domains.

use super::*;

#[test]
fn decoys_rank_below_the_brand() {
    let (_dir, searcher) = build(&corpus());
    let hits = searcher.search_with("us bank", 10, &keep_all()).unwrap();
    let found = domains(&hits);
    assert_eq!(found[0], "usbank.com");
    // The decoys are still decent matches, just not first...
    assert!(found.contains(&"usbank-login-help.com"), "{found:?}");
    assert!(found.contains(&"usbankreviews.net"), "{found:?}");
    // ...though far enough below the bank to be left out by default.
    let hits = searcher.search("us bank", 10).unwrap();
    assert_eq!(domains(&hits), ["usbank.com"]);

    let hits = searcher
        .search_with("bank of america", 10, &keep_all())
        .unwrap();
    let found = domains(&hits);
    assert_eq!(found[0], "bankofamerica.com");
    assert!(
        found.contains(&"bankofamerica-fraud-alerts.com"),
        "{found:?}"
    );
    assert!(found.contains(&"americabank.com"), "{found:?}");

    // Extra words still lead to the brand, not the page that has them all.
    assert_eq!(top(&searcher, "us bank login"), "usbank.com");
    // Word order matters: the small bank named exactly this beats the big one.
    assert_eq!(top(&searcher, "america bank"), "americabank.com");
}

#[test]
fn stuffed_decoys_do_not_win() {
    let mut records = corpus();
    // Names itself "US Bank" wherever a site can: title, og:site_name, a few links.
    records.push(site(
        "usbank-online.com",
        Some("US Bank | US Bank Online | US Bank"),
        Some("US Bank US Bank US Bank online banking login."),
        &["US Bank"],
        &[("US Bank", 5), ("usbank", 2)],
        Signals::default(),
    ));
    // A fairly popular site whose whole name is one of the query words.
    records.push(site(
        "bank.com",
        Some("Bank.com"),
        None,
        &[],
        &[("bank", 40)],
        obscure(30_000, 500),
    ));
    let (_dir, searcher) = build(&records);
    for query in ["us bank", "usbank", "u.s. bank", "US Bank online banking"] {
        assert_eq!(top(&searcher, query), "usbank.com", "query {query:?}");
    }
    assert_eq!(top(&searcher, "bank"), "bank.com");
}

#[test]
fn bare_names_lose_to_a_site_with_its_own_subject_evidence() {
    let mut records = [
        site(
            "youtube.com",
            Some("YouTube"),
            Some("Enjoy the videos you love."),
            &["YouTube"],
            &[("youtube", 300)],
            popular(9, 6_000),
        ),
        // Never crawled, but well placed in the link graph.
        site(
            "you-tubemusic.com",
            None,
            None,
            &[],
            &[],
            obscure(300_000, 0),
        ),
        site(
            "ytmusicfans.net",
            Some("YouTube Music fans | YouTube Music playlists"),
            Some("The best YouTube Music playlists, picked by YouTube Music fans."),
            &[],
            &[("youtube music playlists", 4)],
            obscure(80_000, 3),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let hits = searcher.search("youtube music", 10).unwrap();
    let lookalike = hits
        .iter()
        .find(|h| h.domain == "you-tubemusic.com")
        .unwrap();
    assert!(lookalike.link_score > RankConfig::default().trusted_link_score);
    let partial = hits.iter().find(|hit| hit.domain == "youtube.com").unwrap();
    assert_eq!(partial.query_evidence.as_ref().unwrap().relevance_tier, 0);
    // With no other site named, the bare name is the answer.
    assert_eq!(top(&searcher, "you tubemusic"), "you-tubemusic.com");
    // The same site can win with actual indexed subject evidence;
    // the bare domain itself cannot supply the missing Music term.
    records[0].description = Some("Enjoy the videos and music you love.".into());
    let (_dir, supported) = build(&records);
    assert_eq!(top(&supported, "youtube music"), "youtube.com");
}

#[test]
fn sign_in_pages_pill_shops_and_test_names_are_left_out() {
    let records = [
        site(
            "outlook.com",
            Some("Outlook"),
            Some("Microsoft Outlook email and calendar."),
            &["Microsoft Outlook"],
            &[("outlook", 500)],
            popular(40, 4_000),
        ),
        site(
            "bpl.net",
            Some("Outlook Web App"),
            None,
            &[],
            &[],
            obscure(50_000, 5),
        ),
        site(
            "outlooktips.org",
            Some("Microsoft Outlook tips"),
            Some("Tips for Microsoft Outlook."),
            &[],
            &[],
            obscure(90_000, 2),
        ),
        site(
            "celebrex365o24.com",
            Some("Microsoft Outlook deals"),
            None,
            &[],
            &[],
            obscure(30_000, 2),
        ),
        // Microsoft's own, titled for its product.
        site(
            "live.com",
            Some("Outlook"),
            None,
            &[],
            &[],
            popular(60, 3_000),
        ),
        site(
            "outlook.test",
            Some("Microsoft Outlook"),
            None,
            &[],
            &[],
            obscure(9_000, 2),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let hits = searcher
        .search_with("microsoft outlook", 10, &keep_all())
        .unwrap();
    let order = domains(&hits);
    assert!(order.contains(&"outlooktips.org"), "{order:?}");
    assert!(order.contains(&"live.com"), "{order:?}");
    assert!(!order.contains(&"bpl.net"), "{order:?}");
    assert!(!order.contains(&"outlook.test"), "{order:?}");
    assert!(!order.contains(&"celebrex365o24.com"), "{order:?}");
    // Asked for by name, they are found.
    assert_eq!(top(&searcher, "bpl.net"), "bpl.net");
}

#[test]
fn copies_of_the_named_site_on_other_domains_are_left_out() {
    let records = [
        site(
            "gmail.com",
            None,
            None,
            &["Gmail"],
            &[("gmail", 900)],
            popular(20, 5_000),
        ),
        site(
            "gmail.ru",
            None,
            None,
            &[],
            &[("gmail", 3)],
            obscure(9_000, 30),
        ),
        site("gmail.hu", None, None, &[], &[], obscure(12_000, 20)),
        site(
            "gmail.jp",
            Some("Fans in Japan"),
            Some("Tips for Gmail users in Japan."),
            &[],
            &[],
            obscure(15_000, 10),
        ),
        site(
            "gmailtips.net",
            Some("Gmail tips and tricks"),
            Some("How to use Gmail: filters, labels and shortcuts."),
            &[],
            &[("gmail tips", 2)],
            obscure(200_000, 2),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let hits = searcher.search_with("gmail", 10, &keep_all()).unwrap();
    let order = domains(&hits);
    assert_eq!(order[0], "gmail.com", "{order:?}");
    // Copies with nothing to say are left out, and so is a crawled one
    // for another country, but for a searcher there or a search for it.
    assert!(!order.contains(&"gmail.ru"), "{order:?}");
    assert!(!order.contains(&"gmail.hu"), "{order:?}");
    assert!(!order.contains(&"gmail.jp"), "{order:?}");
    assert!(order.contains(&"gmailtips.net"), "{order:?}");
    let options = SearchOptions {
        country: Some("JP".into()),
        ..SearchOptions::default()
    };
    let in_japan = searcher
        .search_full("gmail", 10, &keep_all(), &options)
        .unwrap()
        .hits;
    let japan = searcher
        .search_with("gmail japan", 10, &keep_all())
        .unwrap();
    for hits in [in_japan, japan] {
        assert!(domains(&hits).contains(&"gmail.jp"), "{:?}", domains(&hits));
    }
    // Typed, the copy is what was asked for.
    let hits = searcher.search("gmail.ru", 10).unwrap();
    assert_eq!(hits[0].domain, "gmail.ru", "{:?}", domains(&hits));
}

#[test]
fn hyphenated_look_alikes_of_the_named_site_are_left_out() {
    let mut official = site(
        "you-tube.org",
        Some("You Tube Trust"),
        None,
        &[],
        &[],
        obscure(250_000, 4),
    );
    official.about = Some("charity".into());
    let records = [
        site(
            "youtube.com",
            Some("YouTube"),
            Some("Enjoy the videos and music you love."),
            &["YouTube"],
            &[("youtube", 900)],
            popular(2, 9_000),
        ),
        // Titled to pass for the real one.
        site(
            "yout-ube.com",
            Some("Watch YouTube Videos with No Ads on Auto-Repeat"),
            None,
            &[],
            &[("youtube", 2)],
            obscure(90_000, 30),
        ),
        site("you-tube.ru", None, None, &[], &[], obscure(300_000, 1)),
        site(
            "youtube-tools.net",
            Some("YouTube tools"),
            Some("Download thumbnails and tags of YouTube videos."),
            &[],
            &[],
            obscure(200_000, 2),
        ),
        official,
    ];
    let (_dir, searcher) = build(&records);
    let order = domains(&searcher.search_with("youtube", 10, &keep_all()).unwrap()).join(" ");
    assert!(order.starts_with("youtube.com"), "{order}");
    assert!(!order.contains("yout-ube.com"), "{order}");
    assert!(!order.contains("you-tube.ru"), "{order}");
    // The name and more is not the name split up.
    assert!(order.contains("youtube-tools.net"), "{order}");
    assert!(order.contains("you-tube.org"), "{order}");
    // Typed, the look-alike is what was asked for.
    assert_eq!(top(&searcher, "yout-ube.com"), "yout-ube.com");
}

#[test]
fn copies_go_when_the_named_site_is_not_first() {
    let records = [
        // Better known, and the query is in its description, but not its
        // name: it ranks above the site the query names.
        site(
            "google.com",
            Some("Google"),
            Some("Search, Gmail, Maps and more."),
            &["Google"],
            &[("google", 5_000)],
            popular(1, 90_000),
        ),
        site(
            "gmail.com",
            None,
            None,
            &[],
            &[("gmail", 900)],
            popular(40, 3_000),
        ),
        site("gmail.cm", None, None, &[], &[], obscure(300_000, 1)),
    ];
    let (_dir, searcher) = build(&records);
    let order = domains(&searcher.search_with("gmail", 10, &keep_all()).unwrap()).join(" ");
    assert!(order.contains("gmail.com"), "{order}");
    assert!(!order.contains("gmail.cm"), "{order}");
}

#[test]
fn the_query_spelled_as_a_domain_goes_next_to_the_site_it_names() {
    let records = [
        site(
            "facebook.com",
            Some("Facebook"),
            Some("Log in to Facebook to connect with friends."),
            &["Facebook"],
            &[("facebook", 900)],
            popular(3, 9_000),
        ),
        site(
            "facebook-login.com",
            None,
            None,
            &[],
            &[],
            obscure(150_000, 5),
        ),
        site(
            "facebooklogin.com",
            Some("Facebook Login"),
            None,
            &[],
            &[],
            obscure(160_000, 4),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let order = domains(
        &searcher
            .search_with("facebook login", 10, &keep_all())
            .unwrap(),
    )
    .join(" ");
    assert_eq!(order, "facebook.com");
    // Typed, it is found.
    assert_eq!(top(&searcher, "facebook-login.com"), "facebook-login.com");
}

#[test]
fn the_site_a_suggestion_names_is_listed_second() {
    let hit = |domain: &str, score: f32| Hit {
        domain: domain.into(),
        url: format!("https://{domain}/"),
        title: None,
        description: None,
        score,
        text_score: 1.0,
        link_score: 0.5,
        placing_text_score: None,
        country: None,
        named: false,
        official: false,
        key_pages: Vec::new(),
        demand: None,
        missing_words: false,
        query_evidence: None,
    };
    // "tco bell": bell.ca as typed, tacobell.com right under it.
    let mut hits = vec![
        hit("bell.ca", 0.7),
        hit("cityofbell.org", 0.6),
        hit("t.co", 0.5),
        hit("tacobell.com", 0.4),
    ];
    suggested_site_second(&mut hits, "tacobell.com");
    assert_eq!(
        domains(&hits),
        ["bell.ca", "tacobell.com", "cityofbell.org", "t.co"]
    );
    assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
    // First or second already: left where it is.
    suggested_site_second(&mut hits, "bell.ca");
    suggested_site_second(&mut hits, "tacobell.com");
    assert_eq!(domains(&hits)[..2], ["bell.ca", "tacobell.com"]);
}

#[test]
fn near_copies_of_a_better_known_site_are_left_out() {
    let text = "Welcome to our store. We sell handmade leather shoes, boots and \
        sandals, cut and stitched by hand in our workshop since 1952. Free shipping on \
        orders over fifty dollars, and free returns within thirty days of delivery. \
        Sign up for our newsletter to hear about new styles first.";
    let shop = |domain: &str, title: &str, body: &str, signals: Signals| {
        let mut record = site(domain, Some(title), None, &[], &[], signals);
        record.body_text = Some(body.to_string());
        record
    };
    let copy_text = text.replace("1952", "1953").replace("fifty", "sixty");
    let other_text = "Shoes for the whole family: running shoes, school shoes and \
        work boots from the brands you know, in every size, with stores in twelve \
        towns and a website that delivers the next day.";
    let records = [
        shop(
            "leathershoes.example",
            "Leather Shoes",
            text,
            obscure(5_000, 300),
        ),
        // A scrape under its own title, linked by nobody.
        shop(
            "cheap-shoes.example",
            "Cheap Shoes Online",
            &copy_text,
            Signals::default(),
        ),
        // Another shoe shop, as little known, but its own text.
        shop(
            "familyshoes.example",
            "Shoes Shop",
            other_text,
            Signals::default(),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let hits = searcher.search("shoes", 10).unwrap();
    assert_eq!(
        domains(&hits),
        ["leathershoes.example", "familyshoes.example"],
        "{hits:#?}"
    );
    // Asked for by its address, the copy stays.
    let hits = searcher.search("cheap-shoes.example", 10).unwrap();
    assert!(domains(&hits).contains(&"cheap-shoes.example"));
}

#[test]
fn a_spam_network_with_one_text_is_listed_once() {
    let text = "Beginner's guide to coding in Python. Learn variables, loops, \
        functions and lists step by step, with small exercises after every lesson \
        and answers at the end, so you can practise at your own pace and build \
        your first programs in a weekend.";
    let page = |domain: &str, title: &str, body: &str| {
        let mut record = site(domain, Some(title), None, &[], &[], obscure(50_000, 20));
        record.body_text = Some(body.to_string());
        record
    };
    let pizza = "Joe's Pizza: New York style pizza by the slice, baked in a coal oven \
        since 1975. Order online for pickup or delivery, see our menu and opening hours.";
    let records = [
        page(
            "modusygunaciro.example",
            "Beginner's Guide to Coding in Python | Crest Center",
            text,
        ),
        page(
            "nyhizyjybawozi.example",
            "Beginner's Guide to Coding in Python | Harbor Workshop",
            text,
        ),
        // Namesakes on one template: each title is its own name.
        page("joespizzanyc.example", "Joe's Pizza NYC", pizza),
        page(
            "joespizzala.example",
            "Joe's Pizza LA",
            &pizza.replace("New York", "LA"),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let hits = searcher.search("coding in python", 10).unwrap();
    assert_eq!(domains(&hits).len(), 1, "{hits:#?}");
    let hits = searcher.search("joe's pizza", 10).unwrap();
    assert_eq!(domains(&hits).len(), 2, "{hits:#?}");
}

#[test]
fn copies_and_copycats_are_left_out() {
    let hit = |domain: &str, title: Option<&str>, link_score: f32, official: bool| Hit {
        domain: domain.into(),
        url: format!("https://{domain}/"),
        title: title.map(str::to_string),
        description: None,
        score: 1.0,
        text_score: 1.0,
        link_score,
        placing_text_score: None,
        country: None,
        named: true,
        official,
        key_pages: Vec::new(),
        demand: None,
        missing_words: false,
        query_evidence: None,
    };
    let kept = |hits: Vec<Hit>, typed: Option<&str>| -> Vec<String> {
        without_copies(hits, typed, None, &[])
            .into_iter()
            .map(|h| h.domain)
            .collect()
    };
    // Airbnb in other countries and on other endings, titled with the
    // brand or not at all; its charity has a title of its own.
    let airbnb = vec![
        hit("airbnb.com", Some("Airbnb | Vacation rentals"), 0.9, true),
        hit(
            "airbnb.fr",
            Some("Airbnb : locations de vacances"),
            0.6,
            false,
        ),
        hit("airbnb.tech", None, 0.3, false),
        hit("airbnb.org", Some("Emergency housing by hosts"), 0.5, false),
        hit("vrbo.com", Some("Vrbo"), 0.8, true),
    ];
    assert_eq!(
        kept(airbnb.clone(), None),
        ["airbnb.com", "airbnb.org", "vrbo.com"]
    );
    // Typed, the copy stays.
    assert!(kept(airbnb, Some("airbnb.fr")).contains(&"airbnb.fr".to_string()));
    // An official site beats the copy ranked above it.
    let gene = vec![
        hit(
            "23andme.org",
            Some("DNA Genetic Testing - 23andMe"),
            0.0,
            false,
        ),
        hit(
            "23andme.com",
            Some("DNA Genetic Testing - 23andMe"),
            0.7,
            true,
        ),
    ];
    assert_eq!(kept(gene, None), ["23andme.com"]);
    // A better-linked site with no title stays next to an official
    // namesake: hm.com and hm.edu (a university) for "h&m".
    let hm = vec![
        hit("hm.edu", Some("Hochschule München"), 0.4, true),
        hit("hm.com", None, 0.7, false),
    ];
    assert_eq!(kept(hm, None), ["hm.edu", "hm.com"]);
    // Not copies (the brand suite's misses after #270): a generic
    // ending next to a country's, a site with no title next to a
    // namesake not titled with the brand, and .me as a generic ending.
    let cases = [
        (
            "honda.com.vn",
            Some("Honda"),
            0.49,
            true,
            "honda.com",
            None,
            0.4,
        ),
        (
            "astro.com.my",
            Some("Astro TV | Astro"),
            0.52,
            false,
            "astro.build",
            Some("Astro"),
            0.4,
        ),
        (
            "hm.edu",
            Some("Hochschule München"),
            0.54,
            true,
            "hm.com",
            None,
            0.4,
        ),
        (
            "proton.com",
            Some("PROTON - INSPIRING CONNECTIONS"),
            0.5,
            true,
            "proton.me",
            Some("Proton"),
            0.4,
        ),
        (
            "mercadolibre.com.ar",
            Some("Mercado Libre Argentina"),
            0.5,
            false,
            "mercadolibre.com",
            Some("Mercado Libre"),
            0.4,
        ),
    ];
    let cases = cases.into_iter().chain([(
        "weather.gov",
        Some("National Weather Service"),
        0.67,
        true,
        "weather.com",
        None,
        0.55,
    )]);
    for (a, a_title, a_link, a_official, b, b_title, b_link) in cases {
        let both = vec![
            hit(a, a_title, a_link, a_official),
            hit(b, b_title, b_link, false),
        ];
        assert_eq!(kept(both, None), [a, b]);
    }
    // A brand's site for another country goes under its better-known site
    // on a generic ending, official or not, in whatever language; but for
    // a searcher there or a search naming the country, and an official
    // one only under an official one.
    let mcdonalds = vec![
        hit("mcdonalds.com", Some("McDonald's"), 0.64, true),
        hit(
            "mcdonalds.co.jp",
            Some("マクドナルド公式サイト"),
            0.56,
            true,
        ),
        hit("mcdonalds.co.kr", Some("맥도날드"), 0.21, false),
        hit("mcdonalds.ca", Some("McDonald's Canada"), 0.43, true),
        hit("mcdonaldsfans.com", Some("McDonald's fans"), 0.1, false),
    ];
    let kept_for = |home: Option<&str>, asked: &[&str]| -> Vec<String> {
        without_copies(mcdonalds.clone(), None, home, asked)
            .into_iter()
            .map(|h| h.domain)
            .collect()
    };
    assert_eq!(kept_for(None, &[]), ["mcdonalds.com", "mcdonaldsfans.com"]);
    assert_eq!(
        kept_for(Some("US"), &[]),
        ["mcdonalds.com", "mcdonaldsfans.com"]
    );
    assert!(kept_for(Some("JP"), &[]).contains(&"mcdonalds.co.jp".to_string()));
    assert!(kept_for(None, &["KR"]).contains(&"mcdonalds.co.kr".to_string()));
    // Nor is a site titled with the brand a copy on the ending of a
    // country the search names, though a copycat there still is.
    let amazon = vec![
        hit("amazon.com", Some("Amazon.com"), 0.9, true),
        hit("amazon.co.uk", Some("Amazon.co.uk"), 0.55, false),
        hit("amazon.de", None, 0.4, false),
        hit("amaz0n.co.uk", Some("Amazon.com"), 0.0, false),
    ];
    let kept_for = |asked: &[&str]| -> Vec<String> {
        without_copies(amazon.clone(), None, None, asked)
            .into_iter()
            .map(|h| h.domain)
            .collect()
    };
    assert_eq!(kept_for(&[]), ["amazon.com"]);
    assert_eq!(kept_for(&["GB"]), ["amazon.com", "amazon.co.uk"]);
    assert_eq!(kept_for(&["DE"]), ["amazon.com", "amazon.de"]);
    let abc = vec![
        hit("abc.com", Some("ABC Network - ABC.com"), 0.67, true),
        hit("abc.net.au", None, 0.6, false),
    ];
    assert_eq!(kept(abc.clone(), None), ["abc.com"]);
    let in_australia = without_copies(abc, None, Some("AU"), &[]);
    assert_eq!(domains(&in_australia), ["abc.com", "abc.net.au"]);
    let paris = vec![
        hit("paris.com", Some("Paris travel guide"), 0.6, false),
        hit("paris.fr", Some("Paris.fr"), 0.5, true),
    ];
    assert_eq!(kept(paris, None), ["paris.com", "paris.fr"]);
    // Not named by the query, a country's site stays (stjohns.ca for "time
    // in st john's newfoundland", next to stjohns.edu), unless one of the
    // brand's official sites titled and described alike: Google Search's
    // country addresses are one site, whose best known stays.
    let mut city = hit("stjohns.ca", Some("City of St. John's"), 0.5, true);
    city.named = false;
    let stjohns = vec![
        hit("stjohns.edu", Some("St. John's University"), 0.6, true),
        city,
    ];
    assert_eq!(kept(stjohns, None), ["stjohns.edu", "stjohns.ca"]);
    let copy = |domain: &str, link_score: f32| Hit {
        named: false,
        description: Some("search engine by Google LLC".into()),
        ..hit(domain, Some("Google Search"), link_score, true)
    };
    let google = vec![
        Hit {
            named: false,
            ..hit("google.com", Some("Google"), 1.0, true)
        },
        copy("google.fr", 0.73),
        copy("google.co.uk", 0.75),
        copy("google.it", 0.67),
        hit("amazon.de", Some("Amazon.de"), 0.72, true),
    ];
    assert_eq!(
        kept(google, None),
        ["google.com", "google.co.uk", "amazon.de"]
    );
    // A site nobody links to with a listed site's title.
    let coin = vec![
        hit(
            "coinbase.com",
            Some("Coinbase - Buy and Sell Crypto"),
            0.9,
            true,
        ),
        hit(
            "elephant-blue.org",
            Some("Coinbase - Buy and Sell Crypto"),
            0.0,
            false,
        ),
        hit("kraken.com", Some("Kraken"), 0.8, true),
    ];
    assert_eq!(kept(coin, None), ["coinbase.com", "kraken.com"]);
    // Linked or not, and the real one with no Wikidata: the title
    // names Coinbase, and elephant-blue.org is not Coinbase.
    let coin = vec![
        hit(
            "coinbase.com",
            Some("Coinbase - Buy and Sell Crypto"),
            0.65,
            false,
        ),
        hit(
            "elephant-blue.org",
            Some("Coinbase - Buy and Sell Crypto"),
            0.23,
            false,
        ),
    ];
    assert_eq!(kept(coin, None), ["coinbase.com"]);
    // Namesakes sharing a title that is each one's own name stay.
    let pizza = vec![
        hit("joespizzanyc.com", Some("Joe's Pizza"), 0.3, false),
        hit("joes-pizza.com", Some("Joe's Pizza"), 0.3, false),
        hit("joespizza.ie", Some("Joe's Pizza"), 0.1, false),
    ];
    assert_eq!(
        kept(pizza, None),
        ["joespizzanyc.com", "joes-pizza.com", "joespizza.ie"]
    );
    // A spam network under one title that names none of its sites, as
    // well linked as each other: the first stays.
    let network = vec![
        hit("python.org", Some("Welcome to Python.org"), 0.9, true),
        hit("bountystory.site", Some("INFORMATION40"), 0.29, false),
        hit("bountystory.fun", Some("INFORMATION40"), 0.29, false),
        hit("wordlistfinder.com", Some("Word List Finder"), 0.3, false),
        hit("bountystory.online", Some("INFORMATION40"), 0.29, false),
    ];
    assert_eq!(
        kept(network, None),
        ["python.org", "bountystory.site", "wordlistfinder.com"]
    );
    // A whole title of a site with a short name, copied on a site it is
    // no name of: mos-scripts.com titled as CNN's homepage.
    let cnn = vec![
        hit(
            "cnn.com",
            Some("Breaking News, Latest News and Videos | CNN"),
            0.9,
            true,
        ),
        hit(
            "mos-scripts.com",
            Some("Breaking News, Latest News and Videos | CNN"),
            0.23,
            false,
        ),
    ];
    assert_eq!(kept(cnn, None), ["cnn.com"]);
    // Little-known sites sharing a title stay when each says something
    // else of itself, or the title has no words, or names the site.
    let described = |domain: &str, title: &str, description: &str| Hit {
        description: Some(description.into()),
        ..hit(domain, Some(title), 0.2, false)
    };
    let dentists = vec![
        described(
            "smithdental.com",
            "Family Dentistry",
            "Dr. Smith in Austin.",
        ),
        described(
            "austinsmiles.com",
            "Family Dentistry",
            "Gentle care in Round Rock.",
        ),
        hit("stars.example", Some("★★★"), 0.2, false),
        hit("moons.example", Some("☾☾"), 0.2, false),
        hit("xn--mnchen-3ya.de", Some("München"), 0.2, false),
        hit("muenchen.example", Some("München"), 0.3, false),
    ];
    assert_eq!(
        kept(dentists, None),
        [
            "smithdental.com",
            "austinsmiles.com",
            "stars.example",
            "moons.example",
            "xn--mnchen-3ya.de",
            "muenchen.example"
        ]
    );
    // The best known of unofficial namesakes stays, wherever it ranks.
    let cbc = vec![
        hit("cbc.bb", Some("CBC Barbados"), 0.3, false),
        hit("cbc.ca", Some("CBC"), 0.8, false),
    ];
    assert_eq!(kept(cbc, None), ["cbc.ca"]);
    // The first of unofficial namesakes stays.
    let quiz = vec![
        hit("quizlet.com", None, 0.8, false),
        hit("quizlet.fun", Some("quizlet.fun - Coming Soon"), 0.0, false),
    ];
    assert_eq!(kept(quiz, None), ["quizlet.com"]);
}

#[test]
fn country_copies_of_a_site_are_listed_once() {
    let engine = |domain: &str, rank: u32| {
        site(
            domain,
            Some("Google"),
            Some("Search the world's information. Google search engine."),
            &[],
            &[("search engine", 50)],
            popular(rank, 1_000),
        )
    };
    let official = |mut record: SiteRecord| {
        record.about = Some("Something Wikidata describes".into());
        record
    };
    let records = [
        engine("google.com", 1),
        engine("google.co.uk", 20),
        engine("google.de", 25),
        engine("google.fr", 30),
        site(
            "bing.com",
            Some("Bing"),
            Some("Microsoft's search engine."),
            &[],
            &[("search engine", 20)],
            popular(40, 500),
        ),
        official(site(
            "swift.org",
            Some("Swift search engines"),
            Some("Swift search engines."),
            &[],
            &[],
            popular(500, 50),
        )),
        official(site(
            "swift.com",
            Some("Swift search engines"),
            Some("Swift search engines."),
            &[],
            &[],
            popular(600, 50),
        )),
    ];
    let (_dir, searcher) = build(&records);
    let hits = searcher.search("search engines", 10).unwrap();
    let order = domains(&hits);
    assert_eq!(
        order.iter().filter(|d| d.starts_with("google.")).count(),
        1,
        "{order:?}"
    );
    assert!(order.contains(&"bing.com"), "{order:?}");
    // Two official sites of one label are two sites.
    assert!(
        order.contains(&"swift.org") && order.contains(&"swift.com"),
        "{order:?}"
    );
    // Typed, a copy is what was asked for.
    assert_eq!(top(&searcher, "google.de"), "google.de");
    // A country site that is the official site of something stays.
    let mut cbc_ca = site("cbc.ca", Some("CBC"), None, &[], &[], popular(300, 500));
    cbc_ca.about = Some("Canadian public broadcaster".into());
    let records = [
        site("cbc.com", Some("CBC"), None, &[], &[], popular(200, 600)),
        cbc_ca,
    ];
    let (_dir, searcher) = build(&records);
    let hits = searcher.search("cbc", 10).unwrap();
    assert!(domains(&hits).contains(&"cbc.ca"), "{:?}", domains(&hits));
    // .co alone is a generic ending, but .com.co is Colombia's: Pizza
    // Hut Colombia is the brand again next to pizzahut.com.
    let records = [
        site(
            "pizzahut.com",
            Some("Pizza Hut"),
            None,
            &[],
            &[],
            popular(200, 600),
        ),
        site(
            "pizzahut.com.co",
            Some("Pizza Hut Colombia"),
            None,
            &[],
            &[],
            popular(9_000, 20),
        ),
        site(
            "pizzahut.co",
            Some("Pizza Hut Deals"),
            None,
            &[],
            &[],
            popular(9_500, 20),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let hits = searcher.search("pizza hut", 10).unwrap();
    let order = domains(&hits);
    assert!(order.contains(&"pizzahut.com"), "{order:?}");
    assert!(!order.contains(&"pizzahut.com.co"), "{order:?}");
    assert_eq!(brand_label("google.co.uk"), Some(("google", "co.uk")));
    assert_eq!(brand_label("news.ycombinator.com"), None);
}

#[test]
fn sites_people_link_to_by_name_are_not_bare() {
    let records = [
        site(
            "home.com",
            Some("Home"),
            Some("Homes for sale."),
            &[],
            &[("home", 40)],
            obscure(5_000, 40),
        ),
        // Turns crawlers away, but people link to it by name.
        site(
            "homedepot.com",
            None,
            None,
            &[],
            &[("home depot", 200), ("the home depot", 80)],
            popular(60, 3_000),
        ),
        site(
            "homedepot.com.mx",
            Some("The Home Depot México"),
            Some("Home Depot: herramientas y materiales."),
            &[],
            &[],
            obscure(40_000, 2),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let hits = searcher.search("home depot", 10).unwrap();
    assert_eq!(hits[0].domain, "homedepot.com", "{:?}", domains(&hits));
}

#[test]
fn popularity_breaks_equal_name_matches() {
    let (_dir, searcher) = build(&corpus());
    let hits = searcher.search("acme", 10).unwrap();
    assert_eq!(domains(&hits)[..2], ["acme.com", "acme.net"]);
    assert_eq!(hits[0].text_score, hits[1].text_score);
    assert!(hits[0].link_score > hits[1].link_score);
}

#[test]
fn link_text_alone_finds_a_site() {
    let (_dir, searcher) = build(&corpus());
    let hits = searcher.search("bofa", 10).unwrap();
    assert_eq!(hits[0].domain, "bankofamerica.com");
    let title = hits[0].title.as_deref().unwrap().to_lowercase();
    assert!(!title.contains("bofa"));
}

#[test]
fn aliases_beat_bare_redirect_domains() {
    let (_dir, searcher) = build(&corpus());
    // allybank.com has the exact label, ally.com the alias, the page and
    // the links. With no title of its own, allybank.com then comes after
    // the titled sites.
    let hits = searcher.search("ally bank", 10).unwrap();
    assert_eq!(hits[0].domain, "ally.com");
    assert!(domains(&hits).contains(&"allybank.com"));
}

#[test]
fn accents_and_case_do_not_matter() {
    let (_dir, searcher) = build(&corpus());
    for query in ["nestle", "Nestlé", "NESTLÉ", "nestlé"] {
        assert_eq!(top(&searcher, query), "nestle.com", "query {query:?}");
    }
}

#[test]
fn punycode_labels_match_unicode_queries() {
    let (_dir, searcher) = build(&corpus());
    for query in ["bücher", "bucher", "Bücher"] {
        assert_eq!(top(&searcher, query), "xn--bcher-kva.de", "query {query:?}");
    }
}

#[test]
fn typed_domains_go_to_that_domain() {
    let (_dir, searcher) = build(&corpus());
    for query in [
        "usbank.com",
        "www.usbank.com",
        "https://www.usbank.com/personal",
    ] {
        assert_eq!(top(&searcher, query), "usbank.com", "query {query:?}");
    }
    assert_eq!(
        top(&searcher, "usbank-login-help.com"),
        "usbank-login-help.com"
    );
    assert_eq!(top(&searcher, "bücher.de"), "xn--bcher-kva.de");
}

#[test]
fn queries_without_letters_or_digits_find_nothing() {
    let (_dir, searcher) = build(&corpus());
    for query in ["", "   ", "!!!", "--- / ...", "'", "|", "\u{2019}"] {
        assert!(
            searcher.search(query, 10).unwrap().is_empty(),
            "query {query:?}"
        );
    }
    assert!(searcher.search("zzzzqqq", 10).unwrap().is_empty());
}

#[test]
fn scores_blend_text_and_popularity() {
    let (_dir, searcher) = build(&corpus());
    // The comparison arm retains the preceding blend; evidence-tier
    // behavior and score monotonicity have their own ranking fixtures.
    let cfg = RankConfig {
        whole_query_relevance: false,
        ..RankConfig::default()
    };
    let search = |query| searcher.search_with(query, 10, &cfg).unwrap();
    let check = |hits: &[Hit], expected: &dyn Fn(&Hit) -> f32| {
        assert!(hits.iter().any(|hit| hit.text_score == 1.0));
        for pair in hits.windows(2) {
            assert!(pair[0].score >= pair[1].score);
        }
        for hit in hits {
            assert!((0.0..=1.0).contains(&hit.text_score), "{hit:?}");
            assert!((0.0..=1.0).contains(&hit.link_score), "{hit:?}");
            assert!((hit.score - expected(hit)).abs() < 1e-5, "{hit:?}");
        }
    };

    // No site is named "online" or "online banking": a plain blend, at
    // the popularity weight of queries that describe what they look for,
    // with popularity counting in proportion below the relevance floor.
    let alpha = cfg.described_alpha.unwrap();
    let floor = cfg.described_relevance.unwrap();
    let hits = search("online banking");
    check(&hits, &|hit| {
        let prior = hit.link_score * (hit.text_score / floor).min(1.0);
        alpha * prior + (1.0 - alpha) * hit.text_score
    });

    // "us bank" is usbank.com's whole name, as its label and in most
    // links to it: the larger bonus of the two, full trust.
    let hits = search("us bank");
    let usbank = &hits[0];
    assert_eq!(usbank.domain, "usbank.com");
    let record_score = corpus()[0].link_score();
    assert!((usbank.link_score - record_score).abs() < 1e-6);
    let label_bonus = |hit: &Hit, share: f32| {
        if hit.domain == "usbank.com" {
            cfg.exact_label_bonus * share
        } else {
            0.0
        }
    };
    let whole_name_bonus = |hit: &Hit| {
        label_bonus(hit, 1.0).max(if hit.domain == "usbank.com" {
            cfg.link_name_bonus
        } else {
            0.0
        })
    };
    check(&hits, &|hit| {
        cfg.alpha * hit.link_score + (1.0 - cfg.alpha) * hit.text_score + whole_name_bonus(hit)
    });

    // "us bank online" goes on past the name: usbank.com gets 2/3 of the
    // bonus, and sites below the trusted link score lose some of theirs.
    let hits = search("us bank online");
    assert_eq!(hits[0].domain, "usbank.com");
    let trusted = cfg.trusted_link_score.min(usbank.link_score);
    assert!(hits.iter().any(|hit| hit.link_score < trusted));
    check(&hits, &|hit| {
        let evidence = (hit.link_score / trusted).min(1.0);
        let trust = cfg.untrusted_share + (1.0 - cfg.untrusted_share) * evidence;
        cfg.alpha * hit.link_score
            + trust * ((1.0 - cfg.alpha) * hit.text_score + label_bonus(hit, 2.0 / 3.0))
    });
}

#[test]
fn a_brand_site_for_the_country_searched_for_is_listed() {
    let shop = |domain: &str, title: &str, rank: u32| {
        site(
            domain,
            Some(title),
            Some("Free shipping on millions of items."),
            &[],
            &[],
            popular(rank, 1_000),
        )
    };
    let mut amazon = shop("amazon.com", "Amazon.com", 5);
    amazon.about = Some("American online retailer".into());
    let records = [
        amazon,
        shop("amazon.co.uk", "Amazon.co.uk", 200),
        shop("amazon.de", "Amazon.de", 220),
    ];
    let (_dir, searcher) = build(&records);
    assert_eq!(top(&searcher, "amazon uk"), "amazon.co.uk");
    let hits = searcher.search("amazon germany", 10).unwrap();
    assert_eq!(domains(&hits), ["amazon.com", "amazon.de"]);
    let hits = searcher.search("amazon", 10).unwrap();
    assert_eq!(domains(&hits), ["amazon.com"]);
}
