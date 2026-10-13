//! Repositories found by their names and what they say of themselves.

use super::*;

/// The repository `title` (`owner/name`), with its name as another title,
/// as the github set writes it.
fn repo(title: &str, stars: u64, description: &str) -> Page {
    Page::from_repo(Article {
        title: title.into(),
        views: stars,
        aliases: title
            .split_once('/')
            .map(|(_, name)| name.to_string())
            .into_iter()
            .collect(),
        description: Some(description.into()),
        ..Article::default()
    })
}

#[test]
fn a_repository_named_in_part_and_described_by_the_rest_is_found() {
    let (_dir, s) = searcher(&[
        repo(
            "fatedier/frp",
            90_000,
            "A fast reverse proxy to help you expose a local server behind a NAT or firewall to the internet",
        ),
        repo(
            "dandavison/delta",
            25_000,
            "A syntax-highlighting pager for git, diff, grep, and blame output",
        ),
        repo(
            "delta-io/delta",
            8_000,
            "An open-source storage framework that enables building a Lakehouse architecture",
        ),
        repo(
            "AdguardTeam/AdGuardHome",
            30_000,
            "Network-wide ads & trackers blocking DNS server",
        ),
        page("Git", 500_000, &[]),
        page("Reverse proxy", 60_000, &[]),
        page("Delta Air Lines", 300_000, &["Delta"]),
        page("Tax rate", 50_000, &[]),
    ]);
    let hits = |query: &str| s.search(query, 5).unwrap();
    for (query, title) in [
        ("frp reverse proxy", "fatedier/frp"),
        ("delta git pager", "dandavison/delta"),
        ("git pager delta", "dandavison/delta"),
        ("fatedier frp proxy", "fatedier/frp"),
    ] {
        let hits = hits(query);
        assert_eq!(hits[0].page.title, title, "{query}: {hits:?}");
        assert!(hits[0].named && hits[0].whole, "{query}: {hits:?}");
        // Asked for as a whole, it leads the sites the words found.
        let placed = place_pages(query, &[site("example.com", false)], hits);
        assert_eq!(placed[0].hit.page.title, title, "{query}");
        assert_eq!(placed[0].at, 0, "{query}");
    }
    // The other repository called so is not described so.
    assert!(!titles(&hits("delta git pager")).contains(&"delta-io/delta"));
    // A name written as one word is called by its words too.
    let found = hits("adguard home");
    assert_eq!(found[0].page.title, "AdguardTeam/AdGuardHome", "{found:?}");
    assert!(found[0].named);
    // The name alone still names it.
    assert!(hits("frp")[0].named);
    // Words its description does not have leave it out, as before.
    for (query, title) in [
        ("frp tax rates", "fatedier/frp"),
        ("delta airlines flights", "dandavison/delta"),
        ("delta storage pager", "dandavison/delta"),
    ] {
        let hits = hits(query);
        assert!(!titles(&hits).contains(&title), "{query}: {hits:?}");
    }
    // Words that only ask describe nothing: named by "frp", not asked for.
    let found = hits("what is frp");
    assert!(found[0].named && !found[0].whole, "{found:?}");
}

#[test]
fn what_the_whole_query_names_keeps_its_place_over_a_described_repository() {
    let (_dir, s) = searcher(&[
        repo(
            "facebook/react",
            240_000,
            "The library for web and native user interfaces.",
        ),
        repo(
            "facebook/react-native",
            120_000,
            "A framework for building native applications using React",
        ),
        repo("torvalds/linux", 200_000, "Linux kernel source tree"),
        page("React Native", 200_000, &[]),
        page("Linux kernel", 400_000, &[]),
        page("Titanic", 400_000, &["RMS Titanic"]),
        repo(
            "someone/titanic",
            2_000,
            "Survival on the Titanic, a Kaggle competition",
        ),
    ]);
    let hits = |query: &str| s.search(query, 5).unwrap();
    // "react native" is React Native, not facebook/react's "web and
    // native user interfaces".
    let found = hits("react native");
    assert_eq!(found[0].page.title, "React Native", "{found:?}");
    assert!(!titles(&found).contains(&"facebook/react"), "{found:?}");
    let found = hits("linux kernel");
    assert_eq!(found[0].page.title, "Linux kernel", "{found:?}");
    assert!(!titles(&found).contains(&"torvalds/linux"), "{found:?}");
    // Nothing is called "linux kernel source": the repository is asked for.
    let found = hits("linux kernel source");
    assert_eq!(found[0].page.title, "torvalds/linux", "{found:?}");
    // An article whose title is part of the query still leads a repository
    // called so that does not say the rest.
    let found = hits("titanic sinking");
    assert_eq!(found[0].page.title, "Titanic", "{found:?}");
    assert!(!titles(&found).contains(&"someone/titanic"), "{found:?}");
}
