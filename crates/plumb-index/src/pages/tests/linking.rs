//! Entity linking: the article a part of the query means.

use super::*;

fn article(title: &str, description: &str, views: u64, aliases: &[&str]) -> Page {
    let mut page = page(title, views, aliases);
    page.description = Some(description.into());
    page
}

#[test]
fn a_part_of_the_query_links_to_the_article_it_usually_means() {
    let mut singer = article("Jordan Smith (musician)", "American singer", 40_000, &[]);
    singer.lead = Some("Jordan Smith is an American singer who won The Voice.".into());
    let (_dir, s) = searcher(&[
        article("Napoleon", "French emperor (1769–1821)", 900_000, &[]),
        article(
            "Napoleon (2023 film)",
            "2023 film by Ridley Scott",
            100_000,
            &[],
        ),
        article("Hamstring", "Muscles at the back of the thigh", 60_000, &[]),
        article(
            "Trepanning",
            "Drilling a hole in the skull",
            20_000,
            &["Trephining"],
        ),
        article("Wood", "Tissue found in trees", 50_000, &[]),
        article("Tylenol", "Painkiller brand and its formula", 300_000, &[]),
        article(
            "Lien",
            "Right to keep property until a debt is paid",
            30_000,
            &[],
        ),
        article("Alien (film)", "1979 film by Ridley Scott", 200_000, &[]),
        singer,
        article(
            "Jordan Smith (footballer)",
            "English footballer",
            5_000,
            &[],
        ),
    ]);
    let first = |query: &str| {
        s.search(query, 5)
            .unwrap()
            .first()
            .map(|hit| hit.page.title.clone())
    };
    // The most read sense, though the rest of the query says more than
    // the article: what it says ("French emperor"), or what the query is
    // most about.
    assert_eq!(
        first("how did napoleon become emperor").as_deref(),
        Some("Napoleon")
    );
    assert_eq!(
        first("hamstring insertion point").as_deref(),
        Some("Hamstring")
    );
    // Meant, not named, so listed after up to three sites whatever the
    // learned ranking says.
    assert!(s.search("hamstring insertion point", 5).unwrap()[0].meant);
    assert!(!s.search("hamstring", 5).unwrap()[0].meant);
    // Listed without the link, so it keeps its place.
    let tylenol = s.search("tylenol formula", 5).unwrap();
    assert!(tylenol[0].page.title == "Tylenol" && !tylenol[0].meant);
    // The last word, after a word like "for".
    assert_eq!(first("uses for wood").as_deref(), Some("Wood"));
    // Not "a lien", which joins to Alien, so would hold "lien".
    let lien = s.search("how to remove a lien", 5).unwrap();
    assert!(lien[0].page.title == "Lien" && lien[0].score >= MIN_PARTIAL_SCORE);
    // An alias, or a title with a qualifier, when the article says more
    // of the query.
    assert_eq!(first("trephining skull").as_deref(), Some("Trepanning"));
    assert_eq!(
        first("jordan smith voice").as_deref(),
        Some("Jordan Smith (musician)")
    );
}

#[test]
fn a_part_of_the_query_is_not_linked_by_a_word_of_a_longer_name_or_a_last_word() {
    let (_dir, s) = searcher(&[
        article("Lambda", "Greek letter", 40_000, &[]),
        article("AWS Lambda", "Serverless computing service", 30_000, &[]),
        article("Login", "Identifying oneself to a computer", 30_000, &[]),
        article(
            "Squeaky Fromme",
            "American attempted assassin",
            50_000,
            &["Squeaky"],
        ),
        article("Floor", "Bottom surface of a room", 30_000, &[]),
        article(
            "Maven (wrestler)",
            "American professional wrestler",
            50_000,
            &[],
        ),
    ]);
    // Listed among the sites: a page found so scores at least this.
    let listed = |query: &str, title: &str| {
        s.search(query, 5)
            .unwrap()
            .iter()
            .any(|hit| hit.page.title == title && hit.score >= MIN_PARTIAL_SCORE)
    };
    // "aws lambda" names AWS Lambda, so the query is not about Lambda.
    assert!(listed("aws lambda pricing", "AWS Lambda"));
    assert!(!listed("aws lambda pricing", "Lambda"));
    // The last word names what is wanted of the rest.
    assert!(!listed("chase login", "Login"));
    // An alias or a title with a qualifier needs the article to say more
    // of the query.
    assert!(!listed("how to fix a squeaky floor", "Squeaky Fromme"));
    assert!(!listed("maven central", "Maven (wrestler)"));
}

#[test]
fn the_article_nearest_in_meaning_comes_after_three_sites() {
    let mut sleep = article("Microsleep", "Brief episode of sleep", 900_000, &[]);
    sleep.site = Some("c.com".into());
    let url = sleep.url.clone();
    let meant = [meant_page(&url, 0.8)];
    let requests = package("pypi", "requests", 100_000);
    let rare = package("pypi", "tinylib", 10);
    let trocar = article("Trocar", "Surgical instrument", 10, &[]);
    let spooner = article("Ryan Spooner", "Canadian ice hockey player", 3_000, &[]);
    let (_dir, s) = searcher(&[
        sleep.clone(),
        article("Napoleon", "French emperor (1769–1821)", 500_000, &[]),
        requests.clone(),
        rare.clone(),
        trocar.clone(),
        spooner.clone(),
    ]);
    let mut sites: Vec<crate::Hit> = ["a.com", "b.com", "c.com", "d.com", "e.com"]
        .iter()
        .map(|domain| site(domain, false))
        .collect();
    // No word of "micro naps" names Microsleep; meaning lists it after
    // the first sites, wherever the learned order would put it, and not
    // under its site, which it would lift.
    let mut found = s.search("micro naps", 5).unwrap();
    assert!(found.is_empty());
    s.add_meant("micro naps", &meant, &mut found).unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0].meant && !found[0].named && !found[0].whole);
    assert!(found[0].score >= MIN_PARTIAL_SCORE);
    let mut placed = place_pages("micro naps", &sites, found);
    crate::learned::reorder(
        crate::learned::Model::builtin(),
        "micro naps",
        &mut sites,
        &mut placed,
    );
    assert_eq!(placed.len(), 1);
    assert!(placed[0].at >= PARTIAL_AFTER && placed[0].under.is_none());
    // Found by words too: kept as they scored it when that lists it, else
    // scored and marked as meant.
    let found_by_words = |score: f32| PageHit {
        page: sleep.clone(),
        score,
        named: false,
        popularity: 1.0,
        whole: false,
        learned: None,
        meant: false,
    };
    let mut found = vec![found_by_words(0.5)];
    assert_eq!(s.add_meant("", &meant, &mut found).unwrap(), None);
    assert!(found.len() == 1 && found[0].score == 0.5 && !found[0].meant);
    let mut found = vec![found_by_words(0.1)];
    s.add_meant("", &meant, &mut found).unwrap();
    assert!(found.len() == 1 && found[0].score >= MIN_PARTIAL_SCORE && found[0].meant);
    // Words find a page the query names better.
    let mut found = s.search("napoleon", 5).unwrap();
    let named = found[0].clone();
    assert!(named.named);
    let before = found.clone();
    assert_eq!(s.add_meant("napoleon", &meant, &mut found).unwrap(), None);
    assert_eq!(titles(&found), titles(&before));
    // An article goes before the pages words found in part, though they
    // score more, so placing lists it: "eye test chart" listed Eye
    // examination and Lea test, not Eye chart. One they list moves there
    // too, unmarked; one that answers the query whole stays first.
    let in_part = |page: &Page, score, popularity| PageHit {
        page: page.clone(),
        popularity,
        ..found_by_words(score)
    };
    let mut found = vec![in_part(&named.page, 0.45, 1.0), in_part(&spooner, 0.4, 1.0)];
    s.add_meant("", &meant, &mut found).unwrap();
    assert_eq!(titles(&found), ["Microsleep", "Napoleon", "Ryan Spooner"]);
    let placed = place_pages("micro naps", &sites, found);
    assert!(placed.iter().any(|p| p.hit.page.title == "Microsleep"));
    let mut found = vec![
        in_part(&named.page, 0.45, 1.0),
        in_part(&spooner, 0.4, 1.0),
        found_by_words(0.35),
    ];
    assert_eq!(s.add_meant("", &meant, &mut found).unwrap(), None);
    assert_eq!(titles(&found), ["Microsleep", "Napoleon", "Ryan Spooner"]);
    assert!(found[0].score == 0.35 && !found[0].meant);
    let mut found = vec![
        PageHit {
            whole: true,
            ..in_part(&named.page, 0.45, 1.0)
        },
        in_part(&spooner, 0.4, 1.0),
    ];
    s.add_meant("", &meant, &mut found).unwrap();
    assert_eq!(titles(&found), ["Napoleon", "Microsleep", "Ryan Spooner"]);
    // A medical search lists health sources first, so there the article
    // goes by score and takes no place of a page words list.
    let medical = "urinary tract infection symptoms men";
    let in_part_pages = || vec![in_part(&named.page, 0.6, 1.0), in_part(&spooner, 0.5, 1.0)];
    let mut found = in_part_pages();
    s.add_meant(medical, &meant, &mut found).unwrap();
    assert_eq!(titles(&found), ["Napoleon", "Ryan Spooner", "Microsleep"]);
    let mut found = [in_part_pages(), vec![found_by_words(0.35)]].concat();
    assert_eq!(s.add_meant(medical, &meant, &mut found).unwrap(), None);
    assert_eq!(titles(&found), ["Napoleon", "Ryan Spooner", "Microsleep"]);
    // One words list but that does not count stays as they found it.
    let mut found = vec![in_part(&trocar, 0.5, 0.1)];
    let trocar_meant = [meant_page(&trocar.url, 0.9), meant_page(&url, 0.8)];
    assert_eq!(s.add_meant("", &trocar_meant, &mut found).unwrap(), None);
    assert!(titles(&found) == ["Trocar"] && !found[0].meant);
    // An address the pages lack adds nothing; the next nearest stands in
    // for it. A package only when words found code too, and as a page that
    // is not an article, only when it is near.
    let nowhere = meant_page("https://en.wikipedia.org/wiki/Nowhere", 0.95);
    let mut found = Vec::new();
    s.add_meant("", std::slice::from_ref(&nowhere), &mut found)
        .unwrap();
    assert!(found.is_empty());
    let nearest = [
        nowhere,
        meant_page(&requests.url, 0.9),
        meant_page(&url, 0.8),
    ];
    s.add_meant("", &nearest, &mut found).unwrap();
    assert_eq!(titles(&found), ["Microsleep"]);
    // An article read too little to count gives way too: "traductor" was
    // not Trocar.
    let mut found = Vec::new();
    s.add_meant(
        "",
        &[meant_page(&trocar.url, 0.9), meant_page(&url, 0.8)],
        &mut found,
    )
    .unwrap();
    assert_eq!(titles(&found), ["Microsleep"]);
    // So does one read enough but neither near nor standing out much:
    // "spooner wisc" was not Ryan Spooner either.
    let weak = crate::MeantPage {
        gap: 0.05,
        ..meant_page(&spooner.url, 0.5)
    };
    let mut found = Vec::new();
    s.add_meant("", &[weak, meant_page(&url, 0.8)], &mut found)
        .unwrap();
    assert_eq!(titles(&found), ["Microsleep"]);
    let mut found = Vec::new();
    s.add_meant("", &[meant_page(&spooner.url, 0.7)], &mut found)
        .unwrap();
    assert_eq!(titles(&found), ["Ryan Spooner"]);
    // Far from every site listed, it needs more: "www.supershuttle.com"
    // lists supershuttle.com, and was not super.com.
    let near_sites = |sites| crate::MeantPage {
        sites,
        ..meant_page(&spooner.url, 0.5)
    };
    let mut found = Vec::new();
    s.add_meant(
        "",
        &[near_sites(Some(0.4)), meant_page(&url, 0.8)],
        &mut found,
    )
    .unwrap();
    assert_eq!(titles(&found), ["Microsleep"]);
    let mut found = Vec::new();
    s.add_meant("", &[near_sites(Some(0.8))], &mut found)
        .unwrap();
    assert_eq!(titles(&found), ["Ryan Spooner"]);
    let flask = || PageHit {
        page: package("pypi", "flask", 1_000),
        score: 0.1,
        named: false,
        popularity: 0.1,
        whole: false,
        learned: None,
        meant: false,
    };
    let mut found = vec![flask()];
    s.add_meant("", &nearest, &mut found).unwrap();
    assert_eq!(titles(&found), ["requests", "flask"]);
    assert!(found[0].meant);
    // It comes after one words list, though it scores more: the two pages
    // placing lists are not one of them short.
    let flask_after = PageHit {
        score: MIN_PARTIAL_SCORE,
        ..flask()
    };
    assert!(found[0].score > flask_after.score);
    let mut found = vec![flask_after];
    s.add_meant("", &nearest, &mut found).unwrap();
    assert_eq!(titles(&found), ["flask", "requests"]);
    let farther = [meant_page(&requests.url, 0.8), nearest[2].clone()];
    let mut found = vec![flask()];
    s.add_meant("", &farther, &mut found).unwrap();
    assert_eq!(titles(&found), ["Microsleep", "flask"]);
    // Nor does one that would score too little to be placed, however
    // near: placing would drop it and leave the search with none.
    let mut found = vec![flask()];
    s.add_meant(
        "",
        &[meant_page(&rare.url, 0.95), nearest[2].clone()],
        &mut found,
    )
    .unwrap();
    assert_eq!(titles(&found), ["Microsleep", "flask"]);
    // Nor does one words list well, near or not: the article follows.
    let flask_listed = PageHit {
        score: 0.5,
        ..flask()
    };
    for cosine in [0.8, 0.9] {
        let mut found = vec![flask_listed.clone()];
        let listed = [
            meant_page(&flask_listed.page.url, cosine),
            nearest[2].clone(),
        ];
        s.add_meant("", &listed, &mut found).unwrap();
        assert_eq!(found.len(), 2);
        assert!(found
            .iter()
            .any(|hit| hit.page.title == "Microsleep" && hit.meant));
        assert!(found
            .iter()
            .any(|hit| hit.page.title == "flask" && hit.score == 0.5 && !hit.meant));
    }
}
