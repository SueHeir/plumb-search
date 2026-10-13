//! Building the index, papers by date and relevance, and entities.

use super::*;

#[test]
fn a_build_told_to_stop_leaves_nothing() {
    let pages = (0..=GO_ON_EVERY).map(|i| page(&format!("Page {i}"), i, &[]));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pages");
    let mut asked = 0;
    let stopped = build_page_index_with_budget(&path, pages, None, &mut || {
        asked += 1;
        bail!("stopping")
    });
    assert!(stopped.is_err());
    assert_eq!(asked, 1);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn paper_date_filter_precedes_candidate_limits_and_excludes_unknown_days() {
    let mut pages: Vec<_> = (0..CANDIDATES + 25)
        .map(|i| {
            dated_paper(
                &format!("Transformer old {i}"),
                Some("2017-06-12"),
                Some(2017),
                10_000,
            )
        })
        .collect();
    pages.push(dated_paper(
        "Transformer recent",
        Some("2026-09-30"),
        Some(2026),
        0,
    ));
    pages.push(dated_paper("Transformer year only", None, Some(2026), 1));
    pages.push(dated_paper("Transformer unknown", None, None, 5));
    pages.push(page("Transformer wikipedia", 1_000_000, &[]));
    let (_dir, index) = searcher(&pages);
    let query = PaperQuery::parse("transformer after:2026-09-01 before:2026-09-30").unwrap();
    let hits = index.search_papers(&query, 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].page.title, "Transformer recent");
    assert_eq!(hits[0].page.paper.as_ref().unwrap().count, 0);
    assert_eq!(index.search("transformer after:2026", 10).unwrap().len(), 2);
    assert_eq!(index.paper_coverage().total, (CANDIDATES + 28) as u64);
    assert_eq!(
        index.paper_coverage().publication_day,
        (CANDIDATES + 26) as u64
    );
}

#[test]
fn paper_newest_collects_recent_candidates_and_preserves_exact_relevance() {
    let mut pages: Vec<_> = (0..CANDIDATES + 20)
        .map(|i| {
            dated_paper(
                &format!("Transformer old {i}"),
                Some("2017-06-12"),
                Some(2017),
                10_000,
            )
        })
        .collect();
    pages.push(dated_paper(
        "Transformer recent",
        Some("2026-09-30"),
        Some(2026),
        0,
    ));
    pages.push(dated_paper(
        "Transformer",
        Some("2010-01-01"),
        Some(2010),
        10_000,
    ));
    let (_dir, index) = searcher(&pages);
    let hits = index
        .search_papers(&PaperQuery::parse("transformer sort:newest").unwrap(), 2)
        .unwrap();
    assert_eq!(hits[0].page.title, "Transformer");
    assert!(hits[0].named);
    assert_eq!(hits[1].page.title, "Transformer recent");
}

#[test]
fn paper_metadata_round_trips_with_parent_language_and_search_fields() {
    let mut source = Article {
        title: "Research".into(),
        item: Some("10.1234/research".into()),
        language: Some("fr".into()),
        paper: Some(PaperMetadata {
            publication_date: Some("2026-01-02".into()),
            preprint_date: Some("2025-01-01".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    source.search = Some(SearchContent::default());
    let page = Page::from_paper(source);
    let decoded: Page = serde_json::from_slice(&serde_json::to_vec(&page).unwrap()).unwrap();
    assert_eq!(page, decoded);
    assert_eq!(page.content_language.as_deref(), Some("fr"));
    assert!(page.search.is_some());
    let (_dir, index) = searcher(&[page]);
    assert!(index
        .in_language(Some("en"))
        .search_papers(&PaperQuery::parse("after:2026").unwrap(), 10)
        .unwrap()
        .is_empty());
    assert_eq!(
        index
            .in_language(Some("fr"))
            .search_papers(&PaperQuery::parse("after:2026").unwrap(), 10)
            .unwrap()
            .len(),
        1
    );
    let old: Page = serde_json::from_value(serde_json::json!({ "set": "papers", "url": "https://doi.org/10.1/x", "title": "Old", "views": 1 })).unwrap();
    assert!(old.paper.is_none());
}

#[test]
fn entity_lookup_survives_competing_pages_and_keeps_item_identity() {
    let japan = Page::from_article(
        "en",
        Article {
            title: "Japan".into(),
            item: Some("Q17".into()),
            description: Some("country in East Asia".into()),
            views: 1,
            aliases: vec!["Nippon".into()],
            facts: vec![plumb_core::facts::Fact {
                kind: plumb_core::facts::FactKind::Population,
                value: "123802000;2024".into(),
            }],
            ..Default::default()
        },
    );
    let mut pages: Vec<_> = (0..250)
        .map(|n| {
            Page::from_set(
                SUBPAGES_SET,
                Article {
                    title: "Japan".into(),
                    item: Some(format!("https://www.who.int/japan/{n}")),
                    views: 100_000,
                    ..Default::default()
                },
            )
            .unwrap()
        })
        .collect();
    pages.push(japan);
    let (_dir, index) = searcher(&pages);
    for name in ["Japan", "Nippon"] {
        let found = index.entities(name, 5).unwrap();
        assert_eq!(found.len(), 1, "{name}: {found:?}");
        assert_eq!(found[0].page.item.as_deref(), Some("Q17"));
        assert_eq!(found[0].page.facts[0].observation_year(), Some(2024));
        assert!(found[0].named);
        assert!(found[0].learned.is_none());
    }
    assert!(index.entities("unrelated country", 5).unwrap().is_empty());
    assert!(index.entities("Japan", 0).unwrap().is_empty());
}

#[test]
fn entity_language_is_filtered_before_name_and_description_caps() {
    for by_description in [false, true] {
        let entity = |language: &str, number: usize, views: u64| {
            let mut page = Page::from_article(
                language,
                Article {
                    title: if by_description {
                        format!("Language entity {number}")
                    } else {
                        "Language control".into()
                    },
                    description: by_description.then(|| "Language control".into()),
                    item: Some(format!("Q{}", 100_000 + number)),
                    views,
                    ..Article::default()
                },
            );
            page.url = format!("https://{language}.wikipedia.org/wiki/Entity_{number}");
            page
        };
        let mut pages: Vec<_> = (0..CANDIDATES + 25)
            .map(|number| entity("en", number, 100_000))
            .collect();
        let spanish = entity("es", CANDIDATES + 25, 1);
        let german = entity("de", CANDIDATES + 26, 1);
        pages.extend([spanish.clone(), german.clone()]);
        let (_dir, index) = searcher(&pages);
        // A post-cap filter would lose both requested-language entities.
        let unscoped = index.entities("language control", CANDIDATES).unwrap();
        assert_eq!(unscoped.len(), CANDIDATES);
        assert!(unscoped.iter().all(|hit| hit.page.language() == Some("en")));
        for (language, expected) in [("es", spanish), ("de", german)] {
            let found = index
                .in_language(Some(language))
                .entities("language control", CANDIDATES)
                .unwrap();
            assert_eq!(found.len(), 1, "{language}, description={by_description}");
            assert_eq!(found[0].page.url, expected.url);
            assert_eq!(found[0].named, !by_description);
        }
    }
}

#[test]
fn entities_keep_exact_stop_word_names() {
    let who = Page::from_article(
        "en",
        Article {
            title: "The Who".into(),
            item: Some("Q93332".into()),
            ..Default::default()
        },
    );
    let (_dir, index) = searcher(&[who]);
    let hits = index.entities("The Who", 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].page.item.as_deref(), Some("Q93332"));
}

#[test]
fn papers_are_asked_for_by_their_title_author_year_or_venue() {
    let paper = |title: &str, description: &str, views: u64| {
        Page::from_paper(Article {
            title: title.into(),
            description: Some(description.into()),
            item: Some(format!("10.1/{views}")),
            views,
            ..Article::default()
        })
    };
    let (_dir, s) = searcher(&[
        paper(
            "Random Forests",
            "Paper by Leo Breiman, 2001, Machine Learning",
            134_047,
        ),
        paper(
            "Basic local alignment search tool",
            "Paper by Stephen F. Altschul et al., 1990, Journal of Molecular Biology",
            90_000,
        ),
        paper(
            "Deep learning",
            "Paper by Yann LeCun et al., 2015, Nature",
            85_369,
        ),
        page("Random forest", 300_000, &[]),
        page("Tool", 400_000, &[]),
    ]);
    for query in [
        "random forests breiman",
        "random forests leo breiman 2001",
        "basic local alignment search tool",
        "deep learning lecun nature",
        "deep learning paper",
    ] {
        let hits = s.search(query, 5).unwrap();
        assert!(
            hits[0].whole && hits[0].page.set == PAPERS_SET,
            "{query}: {hits:?}"
        );
    }
    // A title of fewer words alone, or with words that are not its
    // byline's, asks for no paper.
    for query in ["deep learning", "random forests in python"] {
        assert!(
            !s.search(query, 5).unwrap().iter().any(|hit| hit.whole),
            "{query}"
        );
    }
}

#[test]
fn books_and_papers_follow_articles_and_sites() {
    let book = |title: &str, author: &str| {
        Page::from_book(Article {
            title: title.into(),
            description: Some(format!("Book by {author}")),
            item: Some("OL893415W".into()),
            views: 5_000,
            aliases: vec![format!("{title} {author}")],
            ..Article::default()
        })
    };
    let paper = Page::from_paper(Article {
        title: "Attention Is All You Need".into(),
        item: Some("10.48550/arxiv.1706.03762".into()),
        views: 90_000,
        ..Article::default()
    });
    assert_eq!(paper.url, "https://doi.org/10.48550/arxiv.1706.03762");
    assert_eq!(paper.set_name(), "OpenAlex");
    let (_dir, s) = searcher(&[
        book("Dune", "Frank Herbert"),
        book("Frankenstein; or, The Modern Prometheus", "Mary Shelley"),
        page("Dune (novel)", 1_000, &[]),
        paper,
    ]);
    // The article on the novel before the book of the same name.
    let hits = s.search("dune", 5).unwrap();
    assert_eq!(titles(&hits), ["Dune (novel)", "Dune"]);
    assert_eq!(hits[1].page.url, "https://openlibrary.org/works/OL893415W");
    // Alone the title may be anything: the book never comes before
    // every site.
    let placed = place_pages("dune", &[known_site("dunebook.com", false, 0.0)], hits);
    assert_eq!(placed[1].at, 1);
    // With its author or "book" it is the book that is searched for,
    // before any site the query does not name.
    for query in [
        "dune frank herbert",
        "dune herbert",
        "Dune book",
        "frankenstein mary shelley",
    ] {
        let hits = s.search(query, 5).unwrap();
        assert!(hits[0].whole && hits[0].page.set == BOOKS_SET, "{query}");
        let placed = place_pages(query, &[known_site("dunebook.com", false, 0.9)], hits);
        assert_eq!(placed[0].at, 0, "{query}");
    }
    let hits = s.search("dune frank herbert", 5).unwrap();
    let placed = place_pages(
        "dune frank herbert",
        &[known_site("dunefrankherbert.com", true, 0.0)],
        hits,
    );
    assert_eq!(placed[0].at, 1);
    assert!(!s
        .search("dune movie", 5)
        .unwrap()
        .iter()
        .any(|hit| hit.whole));
    // Only one edition leads.
    let best = s.search("dune frank herbert", 5).unwrap().remove(0);
    let placed = place_pages(
        "dune frank herbert",
        &[known_site("dunebook.com", false, 0.9)],
        vec![best.clone(), best],
    );
    assert_eq!(placed.iter().map(|p| p.at).collect::<Vec<_>>(), [0, 1]);
    // Papers are found by most of their words too.
    let hits = s.search("attention all you need paper", 5).unwrap();
    assert_eq!(titles(&hits), ["Attention Is All You Need"]);
}

#[test]
fn urls_and_sets() {
    let p = page("AT&T", 1, &[]);
    assert_eq!(p.url, "https://en.wikipedia.org/wiki/AT%26T");
    assert_eq!(p.set_name(), "Wikipedia");
    assert_eq!(base_title("Python (genus)"), "Python");
    assert_eq!(base_title("(Untitled)"), "(Untitled)");
    assert_eq!(page_key("Marie Curie"), "mariecurie");
}
