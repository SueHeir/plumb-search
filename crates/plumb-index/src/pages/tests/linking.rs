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
    // Meant, not named, so listed after three sites whatever the learned
    // ranking says.
    assert!(s.search("hamstring insertion point", 5).unwrap()[0].meant);
    assert!(!s.search("hamstring", 5).unwrap()[0].meant);
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
