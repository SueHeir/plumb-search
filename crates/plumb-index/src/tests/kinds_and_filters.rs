//! Kinds of thing, countries, site searches, safe search, languages and
//! search operators.

use super::*;
use crate::hits::{is_garbled, is_unfilled};

fn bank_corpus() -> Vec<SiteRecord> {
    let bank = |domain: &str, alias: &str, country: &str, rank: u32| {
        with_facts(
            site(domain, None, None, &[alias], &[], popular(rank, 5_000)),
            Some(country),
            &["bank", "public company"],
        )
    };
    vec![
        bank("chase.com", "Chase Bank", "US", 150),
        bank("wellsfargo.com", "Wells Fargo", "US", 250),
        bank("usbank.com", "U.S. Bank", "US", 1_500),
        bank("db.com", "Deutsche Bank", "DE", 2_000),
        bank("commerzbank.de", "Commerzbank", "DE", 3_000),
        bank("sparkasse.de", "Sparkasse", "DE", 1_800),
        with_facts(
            site(
                "airbus.com",
                None,
                None,
                &["Airbus"],
                &[],
                popular(2_500, 4_000),
            ),
            None,
            &["aircraft manufacturer"],
        ),
        site(
            "bankrate.com",
            Some("Bankrate: banks, mortgage rates and savings accounts"),
            None,
            &[],
            &[("banks", 50)],
            ranked(2_000, 6_000),
        ),
    ]
}

fn search_in(searcher: &Searcher, query: &str, options: &SearchOptions) -> Vec<String> {
    searcher
        .search_full(query, 10, &RankConfig::default(), options)
        .unwrap()
        .hits
        .into_iter()
        .map(|hit| hit.domain)
        .collect()
}

#[test]
fn kinds_list_the_banks_of_your_country_first() {
    let (_dir, searcher) = build(&bank_corpus());
    let us = search_in(&searcher, "banks", &options("US", false));
    assert_eq!(
        us[..3],
        ["chase.com", "wellsfargo.com", "usbank.com"],
        "{us:?}"
    );
    let de = search_in(&searcher, "banks", &options("de", false));
    assert_eq!(
        de[..3],
        ["sparkasse.de", "db.com", "commerzbank.de"],
        "{de:?}"
    );
    // Singular works too, and every bank is listed even without text matches.
    let bank = search_in(&searcher, "bank", &options("US", false));
    for domain in ["chase.com", "db.com", "commerzbank.de", "sparkasse.de"] {
        assert!(bank.contains(&domain.to_string()), "{bank:?}");
    }
    assert!(!bank.contains(&"airbus.com".to_string()));
    // Generic kinds are not kept, so they find nothing by kind.
    assert!(search_in(&searcher, "public companies", &options("US", false)).is_empty());
}

#[test]
fn news_alone_lists_the_well_known_news_sites() {
    let records = vec![
        with_facts(
            site(
                "news.cn",
                Some("新华网_让新闻离你更近"),
                None,
                &[],
                &[],
                popular(900, 30_000),
            ),
            Some("CN"),
            &["news website"],
        ),
        with_facts(
            site(
                "nytimes.com",
                Some("The New York Times"),
                Some("American daily newspaper"),
                &["The New York Times"],
                &[],
                popular(80, 200_000),
            ),
            Some("US"),
            &["newspaper"],
        ),
        with_facts(
            site(
                "cnn.com",
                Some("Breaking News, Latest News and Videos | CNN"),
                Some("View the latest news and breaking news today"),
                &["CNN"],
                &[],
                popular(90, 180_000),
            ),
            Some("US"),
            &["television channel"],
        ),
        site(
            "newsblog.example",
            Some("News news news"),
            None,
            &[],
            &[],
            obscure(800_000, 3),
        ),
        site(
            "foxnews.com",
            Some("Fox News"),
            Some("Breaking news"),
            &["Fox News"],
            &[],
            popular(300, 90_000),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let us = search_in(&searcher, "news", &options("US", false));
    assert_eq!(us[..2], ["nytimes.com", "cnn.com"], "{us:?}");
    let pos = |domain: &str| us.iter().position(|d| d == domain).unwrap();
    assert!(pos("news.cn") > pos("foxnews.com"), "{us:?}");
    assert!(!us.contains(&"newsblog.example".to_string()) || pos("newsblog.example") > 3);
    let world = search_in(&searcher, "world news", &options("US", false));
    assert_eq!(world[0], "nytimes.com", "{world:?}");
    // The news words name no site.
    let hits = searcher.search("news", 10).unwrap();
    assert!(hits.iter().all(|hit| !hit.named), "{hits:?}");
    // A site the query names keeps it.
    assert_eq!(top(&searcher, "fox news"), "foxnews.com");
    // Off, news.cn leads as before.
    let cfg = RankConfig {
        news_sites: false,
        ..RankConfig::default()
    };
    let off = searcher
        .search_full("news", 10, &cfg, &options("US", false))
        .unwrap();
    assert_eq!(off.hits[0].domain, "news.cn", "{:?}", domains(&off.hits));
}

#[test]
fn news_topic_language_filter_uses_the_same_script_fallback() {
    let title = "News — научные исследования технологии и материалы";
    let mut declared = site(
        "declared.example",
        Some(title),
        None,
        &[],
        &[],
        popular(500, 30000),
    );
    declared.language = Some("en".into());
    let records = [
        site(
            "inferred.example",
            Some(title),
            None,
            &[],
            &[],
            popular(500, 30000),
        ),
        declared,
        site(
            "mixed.example",
            Some("News 中文社区 English language daily reporting"),
            None,
            &[],
            &[],
            popular(500, 30000),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let found = |language: Option<&str>| {
        searcher
            .search_full(
                "world news",
                20,
                &RankConfig::default(),
                &SearchOptions {
                    language: language.map(str::to_string),
                    ..SearchOptions::default()
                },
            )
            .unwrap()
            .hits
            .into_iter()
            .map(|hit| hit.domain)
            .collect::<Vec<_>>()
    };
    assert_eq!(found(None).len(), 3);
    let english = found(Some("en"));
    assert!(!english.contains(&"inferred.example".to_string()));
    assert!(english.contains(&"declared.example".to_string()));
    assert!(english.contains(&"mixed.example".to_string()));
    let russian = found(Some("ru"));
    assert!(russian.contains(&"inferred.example".to_string()));
    assert!(!russian.contains(&"declared.example".to_string()));
}

#[test]
fn a_site_that_says_it_is_of_a_kind_joins_the_kind() {
    let wikipedia = with_facts(
        site(
            "wikipedia.org",
            Some("Wikipedia"),
            Some("Wikipedia is a free online encyclopedia, created and edited by volunteers"),
            &[],
            &[],
            ranked(30, 50_000),
        ),
        None,
        &["Wikimedia content project"],
    );
    let grok = with_facts(
        site(
            "grokipedia.com",
            Some("Grokipedia"),
            None,
            &[],
            &[],
            ranked(40_000, 300),
        ),
        None,
        &["online encyclopedia"],
    );
    // A popular site that mentions a single-word kind is not one.
    let kayak = site(
        "kayak.com",
        Some("KAYAK"),
        Some("Cheap flights and airline tickets"),
        &[],
        &[],
        ranked(200, 20_000),
    );
    let delta = with_facts(
        site(
            "delta.com",
            Some("Delta Air Lines"),
            None,
            &[],
            &[],
            ranked(2_000, 5_000),
        ),
        Some("US"),
        &["airline"],
    );
    let (_dir, searcher) = build(&[wikipedia, grok, kayak, delta]);
    let hits = search_in(&searcher, "online encyclopedia", &options("US", false));
    assert_eq!(hits[..2], ["wikipedia.org", "grokipedia.com"], "{hits:?}");
    let hits = search_in(&searcher, "airlines", &options("US", false));
    assert_eq!(hits[0], "delta.com", "{hits:?}");
    assert!(names_kind(
        "Free Online Encyclopedias, by all",
        "onlineencyclopedia",
        2
    ));
    assert!(names_kind(
        "an online encyclopedia that anyone edits",
        "onlineencyclopedia",
        2
    ));
    assert!(!names_kind(
        "online and encyclopedia",
        "onlineencyclopedia",
        2
    ));
    assert!(!names_kind(
        "Search engine optimization tools",
        "searchengine",
        2
    ));
    assert!(names_kind("A private search engine.", "searchengine", 2));
}

#[test]
fn strict_country_filter_keeps_global_sites() {
    let (_dir, searcher) = build(&bank_corpus());
    let only_us = search_in(&searcher, "banks", &options("US", true));
    assert!(
        only_us.iter().all(|d| !d.ends_with(".de") && d != "db.com"),
        "{only_us:?}"
    );
    assert!(only_us.contains(&"bankrate.com".to_string()));
    let hits = searcher
        .search_full(
            "deutsche bank",
            10,
            &RankConfig::default(),
            &options("US", true),
        )
        .unwrap()
        .hits;
    assert!(hits.iter().all(|hit| hit.country.as_deref() != Some("DE")));
    // Without a home country there is nothing to filter by.
    let hits = searcher
        .search_full(
            "deutsche bank",
            10,
            &RankConfig::default(),
            &SearchOptions {
                country: None,
                only_country: true,
                exact: false,
                ..SearchOptions::default()
            },
        )
        .unwrap()
        .hits;
    assert_eq!(hits[0].domain, "db.com");
    assert_eq!(hits[0].country.as_deref(), Some("DE"));
}

#[test]
fn home_country_nudges_names_without_overruling_them() {
    let mut records = short_names_corpus();
    records.push(site(
        "bbc.co.uk",
        None,
        None,
        &["BBC"],
        &[],
        popular(100, 50_000),
    ));
    records.push(site(
        "bbc.com",
        None,
        None,
        &["BBC"],
        &[],
        popular(90, 50_000),
    ));
    let (_dir, searcher) = build(&records);
    let us = options("US", false);
    assert_eq!(search_in(&searcher, "american airlines", &us)[0], "aa.com");
    assert_eq!(
        search_in(&searcher, "bbc", &options("GB", false))[0],
        "bbc.co.uk"
    );
    assert_eq!(search_in(&searcher, "bbc", &us)[0], "bbc.com");
    // A French user asking for the French site by its name still gets it.
    let fr = search_in(&searcher, "americanairlines fr", &options("FR", false));
    assert_eq!(fr[0], "americanairlines.fr");
}

#[test]
fn site_search_links_hand_off_the_rest_of_the_query() {
    let mut records = corpus();
    records.push(site(
        "github.com",
        Some("GitHub"),
        None,
        &["GitHub"],
        &[],
        popular(30, 100_000),
    ));
    let mut shop = site(
        "acme-shop.com",
        Some("Acme Shop"),
        None,
        &[],
        &[],
        ranked(5_000, 100),
    );
    shop.search_url = Some("https://www.acme-shop.com/find?q={searchTerms}".into());
    records.push(shop);
    let mut liar = site("liar.com", Some("Liar"), None, &[], &[], ranked(5_000, 100));
    liar.search_url = Some("https://evil.example/?q={searchTerms}".into());
    records.push(liar);
    let (_dir, searcher) = build(&records);
    let full = |q: &str| {
        searcher
            .search_full(q, 10, &RankConfig::default(), &SearchOptions::default())
            .unwrap()
    };

    let github = full("github SueHeir/plumb-search");
    assert_eq!(github.hits[0].domain, "github.com");
    assert_eq!(
        github.site_search,
        Some(SiteSearch {
            domain: "github.com".into(),
            terms: "SueHeir/plumb-search".into(),
            url: "https://github.com/search?q=SueHeir%2Fplumb-search".into(),
        })
    );
    let shop = full("acme shop  red  boots").site_search.unwrap();
    assert_eq!(shop.terms, "red boots");
    assert_eq!(shop.url, "https://www.acme-shop.com/find?q=red%20boots");
    // Just the name, a site without a search address, and a search
    // address on another site offer nothing.
    assert_eq!(full("github").site_search, None);
    assert_eq!(full("us bank login").site_search, None);
    assert_eq!(full("liar stuff").site_search, None);
    // Words that go on from the name as a phrase, and questions, have
    // no site to search.
    assert_eq!(full("github of the world").site_search, None);
    assert_eq!(full("github vs gitlab").site_search, None);
    assert_eq!(full("can github host pages").site_search, None);
}

#[test]
fn borrowed_and_garbled_titles_are_not_shown() {
    let mut records = corpus();
    let mut nih = site(
        "nih.gov",
        Some("Home | NIH Office of Disease Prevention Website"),
        Some("Prevention research"),
        &["National Institutes of Health"],
        &[],
        popular(400, 20_000),
    );
    nih.url = Some("https://prevention.nih.gov/".into());
    records.push(nih);
    records.push(site(
        "netease.com",
        Some("\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}"),
        None,
        &[],
        &[],
        popular(900, 10_000),
    ));
    let (_dir, searcher) = build(&records);
    let first = |q: &str| {
        searcher
            .search_full(q, 10, &RankConfig::default(), &SearchOptions::default())
            .unwrap()
            .hits
            .into_iter()
            .next()
            .unwrap()
    };
    let nih = first("nih");
    assert_eq!(nih.domain, "nih.gov");
    assert_eq!(nih.url, "https://nih.gov/");
    assert_eq!(nih.title.as_deref(), Some("National Institutes of Health"));
    assert_ne!(nih.description.as_deref(), Some("Prevention research"));
    let netease = first("netease");
    assert_eq!(netease.domain, "netease.com");
    assert_eq!(netease.title, None);
}

#[test]
fn unfilled_template_descriptions_and_check_addresses_are_not_shown() {
    let mut frengly = site(
        "frengly.com",
        Some("Free Online Translator - online translate www"),
        Some("{{description}}"),
        &[],
        &[],
        ranked(20_000, 40),
    );
    frengly.url = Some("https://frengly.com/".into());
    let mut surbl = site(
        "surbl.org",
        Some("SURBL"),
        Some("surbl desc"),
        &[],
        &[],
        ranked(30_000, 30),
    );
    surbl.about = Some("anti-spam blocklist".into());
    // Crawled into a check, whose address leads nowhere now.
    let mut skyscanner = site(
        "skyscanner.nl",
        Some("Skyscanner"),
        Some("Vergelijk goedkope vluchtprijzen"),
        &[],
        &[],
        ranked(9_000, 200),
    );
    skyscanner.url =
        Some("https://www.skyscanner.nl/sttc/px/captcha-v2/index.html?url=Lz8=&uuid=5103".into());
    let mut records = corpus();
    records.extend([frengly, surbl, skyscanner]);
    let (_dir, searcher) = build(&records);
    let first = |q: &str| searcher.search(q, 10).unwrap().into_iter().next().unwrap();
    let frengly = first("frengly");
    assert_eq!(frengly.domain, "frengly.com");
    assert_eq!(frengly.description, None);
    // What Wikidata says it is, in place of the placeholder.
    let surbl = first("surbl");
    assert_eq!(surbl.domain, "surbl.org");
    assert_eq!(surbl.description.as_deref(), Some("anti-spam blocklist"));
    assert_eq!(first("skyscanner").url, "https://skyscanner.nl/");
    assert_eq!(first("frengly").url, "https://frengly.com/");
}

#[test]
fn unfilled_descriptions_are_told_from_short_ones() {
    for text in [
        "{{description}}",
        "{{ page.excerpt }}",
        "${siteDescription}",
        "null",
        "YOUR DESCRIPTION HERE",
        "Site description",
        "surbl desc",
    ] {
        assert!(is_unfilled(text), "{text:?}");
    }
    for text in [
        "Logic-less {{mustache}} templates.",
        "Todo sobre el iPhone de Apple.",
        "Sort results asc or desc",
        "Description of the Dutch East India Company",
        "Null hypothesis explained",
        "Descriptive statistics",
    ] {
        assert!(!is_unfilled(text), "{text:?}");
    }
}

#[test]
fn garbled_descriptions_are_told_from_lost_accents() {
    for text in [
        "���¾�γ�����й������紴��Ĳƾ���ý�塣",
        "�X�v�ۂb�`���f�B�J�����^�c����A",
        "��ҳ",
        "JTNDZGl2JTIwY2xhc3MlM0QlMjJkZXNrdG9wX3Zp",
        "5LiA5Y+q54ix5oqY6IW+55qE57uv6Z6g",
    ] {
        assert!(is_garbled(text), "{text:?}");
    }
    for text in [
        "D�couvrez et explorez plus de 2000 images satellites",
        "MotorGiga.com: El coche desde todos los �ngulos",
        "Python asyncio reference",
        "ABCDEFGHIJKLMNOPQRSTUVWXYZ",
        "",
    ] {
        assert!(!is_garbled(text), "{text:?}");
    }
}

#[test]
fn site_search_needs_the_site_first_and_more_than_a_common_word() {
    let mut records = corpus();
    let mut get = site("get.gov", Some("Get"), None, &[], &[], ranked(50_000, 10));
    get.search_url = Some("https://get.gov/search?q={searchTerms}".into());
    records.push(get);
    for i in 0..25 {
        records.push(site(
            &format!("getaway{i}.com"),
            Some(&format!("Get away {i}")),
            None,
            &[],
            &[],
            ranked(100_000 + i, 10),
        ));
    }
    let (_dir, searcher) = build(&records);
    let found = searcher
        .search_full(
            "get rid of flies",
            10,
            &RankConfig::default(),
            &SearchOptions::default(),
        )
        .unwrap();
    assert_eq!(found.site_search, None);
}

#[test]
fn safe_search_and_language_leave_sites_out() {
    let mut records = corpus();
    records.push(site(
        "bankporn.example",
        Some("Bank vault videos"),
        None,
        &[],
        &[],
        ranked(800, 5_000),
    ));
    let mut studio = site(
        "studio.example",
        Some("Bank Studio"),
        None,
        &[],
        &[],
        ranked(700, 5_000),
    );
    studio.kinds = vec!["pornographic film studio".into()];
    records.push(studio);
    records.push(site(
        "banklingerie.example",
        Some("Bank lingerie, sexy and simple"),
        None,
        &[],
        &[],
        ranked(750, 5_000),
    ));
    let mut german = site(
        "bankde.example",
        Some("Bank Deutschland"),
        None,
        &[],
        &[],
        ranked(760, 5_000),
    );
    german.language = Some("de".into());
    records.push(german);
    // Says no language, but its title is Russian.
    records.push(site(
        "bankru.example",
        Some("Bank Россия — официальный сайт банка"),
        None,
        &[],
        &[],
        ranked(755, 5_000),
    ));
    // Explicit English metadata remains authoritative.
    let mut template = site(
        "banktemplate.example",
        Some("Bank Москва — кредиты и вклады онлайн"),
        None,
        &[],
        &[],
        ranked(745, 5_000),
    );
    template.language = Some("en".into());
    records.push(template);
    let (_dir, searcher) = build(&records);
    let with = |options: SearchOptions| {
        let hits = searcher
            .search_full("bank", 50, &RankConfig::default(), &options)
            .unwrap()
            .hits;
        domains(&hits)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let has = |found: &[String], domain: &str| found.iter().any(|d| d == domain);

    let off = with(SearchOptions {
        safe: SafeSearch::Off,
        ..SearchOptions::default()
    });
    for domain in ["bankporn.example", "studio.example", "banklingerie.example"] {
        assert!(has(&off, domain), "{domain}");
    }
    let moderate = with(SearchOptions::default());
    assert!(!has(&moderate, "bankporn.example"));
    assert!(!has(&moderate, "studio.example"));
    assert!(has(&moderate, "banklingerie.example"));
    let strict = with(SearchOptions {
        safe: SafeSearch::Strict,
        ..SearchOptions::default()
    });
    assert!(!has(&strict, "banklingerie.example"));
    assert!(has(&strict, "usbank.com"));

    // Sites that say another language go; sites that say nothing stay.
    let english = with(SearchOptions {
        language: Some("en".into()),
        ..SearchOptions::default()
    });
    assert!(!has(&english, "bankde.example"));
    assert!(has(&english, "usbank.com"));
    let german = with(SearchOptions {
        language: Some("de".into()),
        ..SearchOptions::default()
    });
    assert!(has(&german, "bankde.example"));
    // Missing-language Cyrillic text is excluded by incompatible filters.
    assert!(has(&off, "bankru.example"));
    assert!(!has(&english, "bankru.example"));
    assert!(!has(&german, "bankru.example"));
    // Explicit language is authoritative, even for a multilingual excerpt.
    assert!(has(&english, "banktemplate.example"));
    for language in ["ru", "uk"] {
        let found = with(SearchOptions {
            language: Some(language.into()),
            ..SearchOptions::default()
        });
        assert!(has(&found, "bankru.example"), "{language}");
        assert!(!has(&found, "banktemplate.example"), "{language}");
    }
    // ...unless the query names the site.
    let english = SearchOptions {
        language: Some("en".into()),
        ..SearchOptions::default()
    };
    let named = searcher
        .search_full("bankde", 50, &RankConfig::default(), &english)
        .unwrap()
        .hits;
    assert!(domains(&named).contains(&"bankde.example"));
}

#[test]
fn a_query_in_another_script_than_the_language_keeps_its_sites() {
    let mut rakuten = site(
        "rakuten.example",
        Some("楽天市場 | ショッピング"),
        None,
        &[],
        &[],
        ranked(50, 50_000),
    );
    rakuten.language = Some("ja".into());
    let records = [
        rakuten,
        // Says no language, but its title is Arabic.
        site(
            "aljazeera.example",
            Some("الجزيرة نت: آخر أخبار اليوم"),
            None,
            &[],
            &[],
            ranked(60, 50_000),
        ),
        site(
            "bankru.example",
            Some("Bank Россия — официальный сайт банка"),
            None,
            &[],
            &[],
            ranked(755, 5_000),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let english = SearchOptions {
        language: Some("en".into()),
        ..SearchOptions::default()
    };
    let found = |query: &str| {
        let hits = searcher
            .search_full(query, 10, &RankConfig::default(), &english)
            .unwrap()
            .hits;
        domains(&hits)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    // English chosen, but the query is written in the sites' script.
    assert_eq!(found("楽天市場"), ["rakuten.example"]);
    assert_eq!(found("الجزيرة"), ["aljazeera.example"]);
    assert_eq!(found("россия"), ["bankru.example"]);
    // A query in Latin letters keeps the filter.
    assert!(found("bank").is_empty());
    assert_eq!(english.language_for("bank"), Some("en"));
    assert_eq!(english.language_for("楽天"), None);
    assert_eq!(searcher.language_for(&english, "楽天"), None);
}

#[test]
fn a_query_in_another_language_keeps_its_sites() {
    let declared = |domain: String, title: String, language: &str| {
        let mut record = site(&domain, Some(&title), None, &[], &[], ranked(900, 5_000));
        record.language = Some(language.into());
        record
    };
    let mut records = Vec::new();
    for town in [
        "Hamburg", "Bremen", "Köln", "Bonn", "Essen", "Kiel", "Ulm", "Trier", "Jena", "Fulda",
        "Hagen", "Mainz",
    ] {
        let name = town.to_lowercase();
        records.push(declared(
            format!("wetter-{name}.example"),
            format!("Wetter {town}"),
            "de",
        ));
        records.push(declared(
            format!("weather-{name}.example"),
            format!("Weather {town}"),
            "en",
        ));
        records.push(declared(
            format!("hotels-{name}.example"),
            format!("Hotels {town}"),
            "en",
        ));
    }
    records.push(declared("dwd.example".into(), "Wetter Berlin".into(), "de"));
    records.push(declared(
        "gasthof.example".into(),
        "Hotels Berlin".into(),
        "de",
    ));
    let (_dir, searcher) = build(&records);
    let english = SearchOptions {
        language: Some("en".into()),
        ..SearchOptions::default()
    };
    let found = |query: &str| {
        let hits = searcher
            .search_full(query, 10, &RankConfig::default(), &english)
            .unwrap()
            .hits;
        domains(&hits)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    // "wetter" is on German sites only, so the search is German.
    assert_eq!(searcher.language_for(&english, "wetter berlin"), None);
    assert_eq!(found("wetter berlin")[0], "dwd.example");
    // "hotels" is on English sites, so the filter stays.
    assert_eq!(
        searcher.language_for(&english, "hotels berlin").as_deref(),
        Some("en")
    );
    assert!(!found("hotels berlin").contains(&"gasthof.example".to_string()));
    // Words too few sites have, and numbers, decide nothing.
    for query in ["berlin", "weather 2026", "zzz"] {
        assert_eq!(
            searcher.language_for(&english, query).as_deref(),
            Some("en"),
            "{query}"
        );
    }
    // The same the other way round.
    let german = SearchOptions {
        language: Some("de".into()),
        ..SearchOptions::default()
    };
    assert_eq!(searcher.language_for(&german, "weather berlin"), None);
    assert_eq!(
        searcher.language_for(&german, "wetter berlin").as_deref(),
        Some("de")
    );
}

#[test]
fn search_operators_narrow_the_results() {
    let mut records = corpus();
    records.push(site(
        "github.com",
        Some("GitHub: where the world builds software"),
        None,
        &["GitHub"],
        &[],
        popular(30, 100_000),
    ));
    records.push(site(
        "bankrate.com",
        Some("Bankrate: mortgage rates and bank reviews"),
        None,
        &[],
        &[],
        ranked(900, 5_000),
    ));
    let (_dir, searcher) = build(&records);
    let full = |q: &str| {
        searcher
            .search_full(q, 10, &RankConfig::default(), &SearchOptions::default())
            .unwrap()
    };
    let plain = domains(&full("bank").hits).len();
    assert!(plain > 2);

    // Only sites under the named host, and the site itself.
    let on_site = full("bank site:bankrate.com");
    assert_eq!(domains(&on_site.hits), ["bankrate.com"]);
    // A site whose homepage lacks the words is still listed, with a
    // link into its own search.
    let github = full("site:github.com plumb \"search engine\"");
    assert_eq!(domains(&github.hits), ["github.com"]);
    assert_eq!(
        github.site_search.unwrap().url,
        "https://github.com/search?q=plumb%20%22search%20engine%22"
    );
    assert_eq!(domains(&full("site:github.com").hits), ["github.com"]);
    assert!(full("site:github.com").site_search.is_none());

    // Excluded words and hosts.
    let without = full("bank -bankrate");
    assert!(!domains(&without.hits).contains(&"bankrate.com"));
    assert!(!domains(&without.hits).is_empty());
    let not_site = full("bank -site:usbank.com");
    assert!(!domains(&not_site.hits).contains(&"usbank.com"));

    // A phrase keeps only sites with those words in that order.
    let phrase = full("\"bank reviews\"");
    assert!(domains(&phrase.hits).contains(&"bankrate.com"));
    assert!(!domains(&phrase.hits).contains(&"usbank.com"));
    assert!(full("-bank").hits.is_empty());
}

#[test]
fn brand_names_find_the_official_site() {
    let (_dir, searcher) = build(&corpus());
    for query in [
        "us bank",
        "US Bank",
        "usbank",
        "u.s. bank",
        "U.S. Bank",
        "US BANK",
    ] {
        assert_eq!(top(&searcher, query), "usbank.com", "query {query:?}");
    }
    for query in ["bank of america", "bankofamerica", "Bank of America"] {
        assert_eq!(
            top(&searcher, query),
            "bankofamerica.com",
            "query {query:?}"
        );
    }
    assert_eq!(top(&searcher, "chase"), "chase.com");
    assert_eq!(top(&searcher, "wikipedia"), "wikipedia.org");
}

#[test]
fn a_well_known_site_the_query_starts_by_naming_keeps_its_language() {
    let foreign = |domain: &str, title: &str, alias: &[&str], language: &str, signals| {
        let mut record = site(domain, Some(title), None, alias, &[], signals);
        record.language = Some(language.into());
        record
    };
    let mut records = corpus();
    records.extend([
        foreign(
            "realmadrid.example",
            "Real Madrid CF | Web oficial",
            &["Real Madrid"],
            "es",
            popular(500, 50_000),
        ),
        // Well known, but by one common word.
        foreign(
            "free.example",
            "Free: Freebox, forfaits et jeux",
            &["Free"],
            "fr",
            popular(300, 50_000),
        ),
        // Named by two words, but known to few.
        foreign(
            "realestate.example",
            "Real Estate Immobilien",
            &[],
            "de",
            obscure(5_000_000, 20),
        ),
    ]);
    let (_dir, searcher) = build(&records);
    let english = SearchOptions {
        language: Some("en".into()),
        ..SearchOptions::default()
    };
    for (query, domain, kept) in [
        ("real madrid tickets", "realmadrid.example", true),
        ("free games", "free.example", false),
        ("real estate agents", "realestate.example", false),
    ] {
        let found =
            |options: &SearchOptions| search_in(&searcher, query, options).contains(&domain.into());
        assert!(found(&SearchOptions::default()), "{query}");
        assert_eq!(found(&english), kept, "{query}");
    }
}
