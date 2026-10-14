//! Documentation and reference pages.

use super::*;

fn docs_page(url: &str, title: &str, aliases: &[&str], description: &str) -> Page {
    Page::from_docs(Article {
        title: title.into(),
        description: Some(description.into()),
        item: Some(url.into()),
        views: 5_000,
        aliases: aliases.iter().map(|a| a.to_string()).collect(),
        ..Article::default()
    })
    .unwrap()
}

// Verbatim rows (including aliases and section extensions) read from
// the HPC docs file on 2026-10-10 UTC. No crawl or enrichment is needed
// to recover these pages.
fn short_docs_fixture() -> Vec<Page> {
    plumb_core::article::articles_of(
        include_str!("../../../tests/fixtures/short-docs.tsv")
            .lines()
            .map(str::to_string),
    )
    .map(|(_, article)| Page::from_docs(article.unwrap()).unwrap())
    .collect()
}

#[test]
fn short_typed_docs_recover_existing_records() {
    let (_dir, searcher) = searcher(&short_docs_fixture());
    for (query, expected) in [
        ("tomllib", "https://docs.python.org/3/library/tomllib.html"),
        ("python tomllib", "https://docs.python.org/3/library/tomllib.html"),
        ("tomllib site:docs.python.org", "https://docs.python.org/3/library/tomllib.html"),
        ("padStart", "https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/String/padStart"),
        ("javascript padStart", "https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/String/padStart"),
        ("react hooks", "https://react.dev/reference/react/hooks"),
    ] {
        let ops = Operators::parse(query);
        let hits = searcher.search_naming_docs(&ops.words, &ops, true, 10).unwrap();
        assert!(hits.iter().any(|hit| hit.page.url == expected), "{query}: {hits:?}");
    }
    let hits = searcher
        .search_naming_docs("python", &Operators::default(), true, 10)
        .unwrap();
    assert!(
        !hits.is_empty(),
        "product-only docs must find existing overview pages"
    );
    assert!(
        hits.iter().all(|hit| hit.page.url.ends_with("/index.html")),
        "{hits:?}"
    );
}

#[test]
fn docs_kind_host_and_exact_symbols_survive_candidate_caps() {
    let mut pages = short_docs_fixture();
    for i in 0..CANDIDATES + 20 {
        let mut article = page(
            &format!("Python tomllib {i}"),
            1_000_000,
            &["Python tomllib"],
        );
        // Other redirect names are indexed as exact keys; ordinary
        // Wikipedia aliases only contribute to the words field.
        article.names.push("Python tomllib".into());
        pages.push(article);
        pages.push(Page::from_question(Article {
            title: format!("Python tomllib question {i}"),
            description: Some("python, tomllib".into()),
            item: Some(i.to_string()),
            views: 1_000_000,
            ..Article::default()
        }));
        let mut other_host = docs_page(
            &format!("https://developer.mozilla.org/en-US/docs/tomllib/{i}"),
            "tomllib — Other ecosystem",
            &["MDN tomllib"],
            "JavaScript tomllib",
        );
        other_host.views = 1_000_000;
        pages.push(other_host);
        let mut broad = docs_page(
            &format!("https://developer.mozilla.org/en-US/docs/padding/{i}"),
            &format!("padStart examples {i}"),
            &[],
            "JavaScript padding examples",
        );
        broad.views = 1_000_000;
        pages.push(broad);
    }
    // Production files are ordered by views; put the deliberately
    // popular distractors before the low-view docs fixture as well.
    pages.sort_by_key(|page| std::cmp::Reverse(page.views));
    let (_dir, searcher) = searcher(&pages);
    // Legacy mixed-set retrieval loses the named docs to popular
    // article aliases. Docs are eligible within their own cap now.
    assert!(!searcher
        .search_general("python tomllib", 10, None)
        .unwrap()
        .iter()
        .any(|hit| hit.page.url.ends_with("/library/tomllib.html")));
    for (query, docs) in [
        ("python tomllib", true),
        ("tomllib site:docs.python.org", true),
        ("tomllib site:docs.python.org", false),
    ] {
        let ops = Operators::parse(query);
        let hits = searcher
            .search_naming_docs(&ops.words, &ops, docs, 10)
            .unwrap();
        assert!(
            hits.iter()
                .any(|hit| hit.page.url.ends_with("/library/tomllib.html")),
            "{query}: {hits:?}"
        );
        assert!(hits
            .iter()
            .all(|hit| hit.page.set == DOCS_SET && operators_allow(&ops, &hit.page)));
    }
    let ops = Operators::parse("padStart site:developer.mozilla.org");
    let hits = searcher
        .search_naming_docs(&ops.words, &ops, true, 1)
        .unwrap();
    assert_eq!(hits[0].page.url, "https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/String/padStart");
}

#[test]
fn short_docs_preserve_operators_ecosystems_and_navigation() {
    let mut pages = short_docs_fixture();
    let mut structures = docs_page(
        "https://docs.python.org/3/tutorial/datastructures.html",
        "Data Structures",
        &["Python Data Structures"],
        "Lists and sequences",
    );
    structures.sections = vec!["List Comprehensions".into()];
    pages.push(structures);
    pages.push(docs_page(
        "https://docs.python.org/3/glossary.html",
        "Glossary",
        &["Python Glossary"],
        "Python words",
    ));
    let (_dir, searcher) = searcher(&pages);
    for query in ["tomllib", "padStart"] {
        let hits = searcher.search(query, 10).unwrap();
        assert!(hits.iter().any(|hit| hit.page.set == DOCS_SET), "{query}");
        assert!(hits
            .iter()
            .filter(|hit| hit.page.set == DOCS_SET)
            .all(|hit| !hit.named && !hit.whole));
    }
    for query in ["python", "glossary", "list comprehensions"] {
        assert!(
            searcher
                .search(query, 10)
                .unwrap()
                .iter()
                .all(|hit| hit.page.set != DOCS_SET),
            "{query}"
        );
    }
    for query in [
        "python list comprehension",
        "list comprehension site:docs.python.org",
        "python documentation",
        "what is tomllib documentation",
    ] {
        let ops = Operators::parse(query);
        assert!(
            !searcher
                .search_naming_docs(&ops.words, &ops, true, 10)
                .unwrap()
                .is_empty(),
            "{query}"
        );
    }
    let kotlin = searcher
        .search_naming_docs("kotlin padStart", &Operators::default(), true, 10)
        .unwrap();
    assert_eq!(kotlin.len(), 1);
    assert_eq!(kotlin[0].page.set_name(), "kotlinlang.org");
    let js = searcher
        .search_naming_docs("javascript padStart", &Operators::default(), true, 10)
        .unwrap();
    assert_eq!(js.len(), 1);
    assert_eq!(js[0].page.set_name(), "developer.mozilla.org");
    assert!(searcher
        .search_naming_docs(
            "Array.prototype.padStart()",
            &Operators::default(),
            true,
            10
        )
        .unwrap()
        .is_empty());
    for query in [
        "tomllib -site:python.org",
        "tomllib site:kotlinlang.org",
        "tomllib -TOML",
        "padStart -\"String.prototype.padStart\" site:developer.mozilla.org",
        "\"react hooks\" -reference",
        "\"react built-in hooks\"",
    ] {
        let ops = Operators::parse(query);
        assert!(
            searcher
                .search_naming_docs(&ops.words, &ops, true, 10)
                .unwrap()
                .is_empty(),
            "{query}"
        );
    }
    let ops = Operators::parse("\"react hooks\"");
    assert_eq!(
        searcher
            .search_naming_docs(&ops.words, &ops, true, 10)
            .unwrap()[0]
            .page
            .title,
        "Built-in React Hooks"
    );
    assert!(searcher
        .search_naming_docs("tomllib", &Operators::default(), true, 0)
        .unwrap()
        .is_empty());
    let question = Page::from_question(Article {
        title: "How do I parse configuration files?".into(),
        description: Some("python, tomllib".into()),
        item: Some("1".into()),
        ..Article::default()
    });
    assert_eq!(
        searcher.question_match(
            &question,
            &searcher.question_words("python tomllib"),
            &searcher.question_words("python tomllib"),
            None
        ),
        (0.0, false)
    );
    assert!(
        searcher
            .question_match(
                &question,
                &searcher.question_words("parse configuration python"),
                &searcher.question_words("parse configuration python"),
                None
            )
            .0
            > 0.0
    );
}

#[test]
fn reference_pages_are_listed_only_when_asked_for() {
    let allowed = |set: &str, query: &str, title: &str| {
        let hit = PageHit {
            page: Page::from_set(
                set,
                Article {
                    title: title.into(),
                    item: Some("https://reference.example/article".into()),
                    views: 2_000,
                    ..Article::default()
                },
            )
            .unwrap(),
            score: 0.9,
            named: false,
            popularity: 0.5,
            whole: false,
            learned: None,
        };
        subpage_asked(query, &[], &hit)
    };
    for set in [REFERENCE_SET, REFERENCE2_SET] {
        for (query, title) in [
            ("celiac disease", "Celiac disease"),
            (
                "how to renew a green card",
                "How to renew or replace your green card",
            ),
            (
                "symptoms of type 2 diabetes",
                "Type 2 diabetes: Symptoms, diagnosis and treatment",
            ),
            ("define prioritize", "Prioritize Definition & Meaning"),
            ("stool smell", "Foul-Smelling Stool: Causes and Treatment"),
            ("how to fix a leaking faucet", "Fixing a leaking faucet"),
            // Long titles with all of the query's words.
            (
                "pain with ovarian cyst",
                "Will an ovarian cyst cause back pain? What to know",
            ),
            (
                "toilet leaks at base",
                "How to Fix a Toilet That Leaks Around the Base",
            ),
            (
                "epley's maneuver",
                "Epley Maneuver (Canalith Repositioning Procedure)",
            ),
            ("ribeye steak", "Charcoal-Grilled Ribeye Steak Recipe"),
            // A short title with all but the query's last word.
            ("lemon drop cocktail", "Lemon drop martini recipe"),
            ("time zone converter", "Time Zone Calculator"),
        ] {
            assert!(allowed(set, query, title), "{set} {query}: {title}");
        }
        for (query, title) in [
            ("seven summits", "The Seven Second Summits of the World"),
            (
                "time in st john's",
                "The Best Time to Visit the US Virgin Islands",
            ),
            (
                "nobel prize in physics 2025",
                "John F. Clauser: Nobel Prize-winning physicist",
            ),
            (
                "the great gatsby f scott fitzgerald",
                "The Great Gatsby Questions for Study and Discussion",
            ),
            ("leaking faucet repair", "Faucet installation cost"),
            (
                "fifa world cup schedule",
                "FIFA World Cup 2026 travel guide",
            ),
            // From the 1,000-search review on hpc, where each was first.
            (
                "work australia",
                "There's a 2nd El Niño — and scientists just figured out how it works",
            ),
            (
                "crater lakes",
                "NASA's Perseverance rover confirms presence of ancient lake on Mars",
            ),
            (
                "google flights",
                "Some Very Smart Money Can't Agree On What to Do With Alphabet's Soaring Stock",
            ),
            (
                "lack of friends",
                "Indoor Grilling: Leeks with Asian Vinaigrette Recipe",
            ),
            // News on what a two-word query names, listed under its site.
            ("hulk hogan", "Hulk Hogan's Daughter Brooke Welcomes Twins"),
            (
                "taco bell",
                "Taco Bell Has a New Version of the Menu Item That Sold Out in Days",
            ),
        ] {
            assert!(!allowed(set, query, title), "{set} {query}: {title}");
        }
    }
    // A subpage needs most of its title in the query, however much of the
    // query its title has.
    let cyst = "Will an ovarian cyst cause back pain? What to know";
    assert!(!allowed(SUBPAGES_SET, "pain with ovarian cyst", cyst));
    let page = Page::from_set(
        REFERENCE2_SET,
        Article {
            title: "Celiac disease".into(),
            item: Some("https://www.healthline.com/health/celiac".into()),
            ..Article::default()
        },
    )
    .unwrap();
    assert!(Page::has_reader(REFERENCE2_SET));
    assert!(Page::has_reader(REFERENCE_SET));
    assert!(page.is_site_page());
    assert_eq!(page.set_name(), "healthline.com");
    assert_eq!(page.set_domain(), "healthline.com");
}

fn reference_page(url: &str, title: &str, description: &str) -> Page {
    Page::from_reference(Article {
        title: title.into(),
        description: Some(description.into()),
        item: Some(url.into()),
        views: 5_000,
        ..Article::default()
    })
    .unwrap()
}

#[test]
fn reference_pages_are_found_by_most_words_and_lead_weak_sites() {
    let stool = reference_page(
        "https://www.healthline.com/health/foul-smelling-stool",
        "Foul-Smelling Stool: Causes and Treatment",
        "Foul-smelling stools have an unusually strong, putrid smell.",
    );
    assert_eq!(stool.set_name(), "healthline.com");
    assert_eq!(stool.set_domain(), "healthline.com");
    let prioritize = reference_page(
        "https://www.merriam-webster.com/dictionary/prioritize",
        "Prioritize Definition & Meaning",
        "The meaning of PRIORITIZE is to list or rate in order of priority.",
    );
    let (_dir, searcher) = searcher(&[stool.clone(), prioritize.clone()]);
    let found = |query: &str| -> Vec<(String, bool)> {
        searcher
            .search(query, 10)
            .unwrap()
            .into_iter()
            .map(|hit| (hit.page.title, hit.named))
            .collect()
    };
    assert_eq!(found("foul smelling stool"), [(stool.title.clone(), false)]);
    // Two words are enough.
    assert_eq!(
        found("prioritize meaning"),
        [(prioritize.title.clone(), false)]
    );
    // Part of its title names nothing; its whole title does.
    assert!(found("stool").is_empty());
    assert_eq!(
        found("Prioritize Definition & Meaning"),
        [(prioritize.title.clone(), true)]
    );
    assert!(found("smelling salts").is_empty());
    let read = Page::from_set(
        REFERENCE_SET,
        Article {
            title: stool.title.clone(),
            description: stool.description.clone(),
            item: Some(stool.url.clone()),
            views: stool.views,
            ..Article::default()
        },
    );
    assert_eq!(read, Some(stool.clone()));
    assert!(Page::has_reader(REFERENCE_SET));

    // Asked in full, it leads any site the query does not name; found
    // by most of its words, only one that shares a word with the query.
    let placed_at = |query: &str, best: crate::Hit| {
        let hit = searcher.search(query, 10).unwrap().remove(0);
        let sites = [best, site("other.com", false)];
        place_pages(query, &sites, vec![hit])[0].at
    };
    let mut weak = site("stool.com", false);
    weak.text_score = 0.2;
    assert_eq!(
        placed_at("foul smelling stool", site("stool.com", false)),
        0
    );
    assert_eq!(placed_at("foul smelling stool", site("stool.com", true)), 1);
    assert_eq!(placed_at("stool smell", weak), 0);
    assert_eq!(placed_at("stool smell", site("stool.com", false)), 1);
    assert_eq!(placed_at("stool smell", site("stool.com", true)), 1);
}

#[test]
fn docs_pages_are_found_by_product_and_title_or_most_words() {
    let sorting = docs_page(
        "https://docs.python.org/3/howto/sorting.html",
        "Sorting Techniques",
        &["Python Sorting Techniques", "Sorting Techniques Python"],
        "Python lists have a built-in list.sort() method that modifies the list in-place.",
    );
    assert_eq!(sorting.set_name(), "docs.python.org");
    assert_eq!(sorting.set_domain(), "docs.python.org");
    let glossary = docs_page(
        "https://docs.python.org/3/glossary.html",
        "Glossary",
        &["Python Glossary", "Glossary Python"],
        "The default Python prompt of the interactive shell.",
    );
    let (_dir, searcher) = searcher(&[
        page("Glossary", 2_000, &[]),
        sorting.clone(),
        glossary.clone(),
    ]);
    let found = |query: &str| -> Vec<(String, bool)> {
        searcher
            .search(query, 10)
            .unwrap()
            .into_iter()
            .filter(|hit| hit.page.set == DOCS_SET)
            .map(|hit| (hit.page.title, hit.named))
            .collect()
    };
    assert_eq!(
        found("python sorting techniques"),
        [("Sorting Techniques".to_string(), true)]
    );
    assert_eq!(found("python glossary"), [("Glossary".to_string(), true)]);
    // Its title alone names nothing.
    assert!(found("glossary").is_empty());
    assert!(found("sorting techniques").is_empty());
    // Most of the query's words, with the product named.
    assert_eq!(
        found("sort a list in python"),
        [("Sorting Techniques".to_string(), false)]
    );
    assert!(found("sort a list in place").is_empty());
    assert!(Page::from_docs(Article {
        item: Some("http://docs.python.org/3/".into()),
        ..Article::default()
    })
    .is_none());
    let read = Page::from_set(
        DOCS_SET,
        Article {
            title: sorting.title.clone(),
            description: sorting.description.clone(),
            item: Some(sorting.url.clone()),
            views: sorting.views,
            aliases: sorting.aliases.clone(),
            ..Article::default()
        },
    );
    assert_eq!(read, Some(sorting.clone()));
    assert!(Page::has_reader(DOCS_SET));

    // Found by its words, it comes after the best site unless that is
    // the docs' own.
    let placed_at = |best: &str| {
        let sites = [site(best, false), site("other.com", false)];
        let mut placed = vec![PlacedPage {
            at: 0,
            under: None,
            hit: PageHit {
                page: sorting.clone(),
                score: 0.9,
                named: false,
                popularity: 1.0,
                whole: true,
                learned: None,
            },
        }];
        keep_page_rules("sort a list in python", &sites, &mut placed);
        placed[0].at
    };
    assert_eq!(placed_at("pypi.org"), 1);
    assert_eq!(placed_at("python.org"), 0);
    assert_eq!(placed_at("docs.python.org"), 0);
    assert_eq!(placed_at("cpython.org"), 1);
}

#[test]
fn rich_docs_symbols_passages_and_language_survive_indexing_together() {
    use plumb_core::article::{SearchPassage, SearchSymbol};
    let page = Page::from_docs(Article {
        title: "Node — Godot documentation".into(),
        item: Some("https://docs.godotengine.org/en/stable/classes/class_node.html".into()),
        aliases: vec!["Godot Node".into()],
        language: Some("en".into()),
        search: Some(SearchContent {
            symbols: vec![SearchSymbol {
                identifier: "set_multiplayer_authority".into(),
                anchor: Some("class-node-method-set-multiplayer-authority".into()),
            }],
            passages: vec![SearchPassage {
                heading: "Multiplayer authority".into(),
                text: "Sets the node's multiplayer authority to the given peer identifier.".into(),
                anchor: Some("class-node-method-set-multiplayer-authority".into()),
            }],
            ..SearchContent::default()
        }),
        ..Article::default()
    })
    .unwrap();
    assert_eq!(page.language(), Some("en"));
    assert!(page.topic().unwrap().contains("set_multiplayer_authority"));
    let (_dir, index) = searcher(std::slice::from_ref(&page));
    let found = index.search("godot set multiplayer authority", 5).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].page.search, page.search);
    assert_eq!(found[0].page.content_language, page.content_language);
    for query in [
        "set_multiplayer_authority",
        "godot set_multiplayer_authority()",
    ] {
        let ops = Operators::parse(query);
        let found = index.search_naming_docs(&ops.words, &ops, true, 5).unwrap();
        assert_eq!(found.len(), 1, "{query}: {found:?}");
        assert!(found[0].named);
    }
    let plain = index.search("set_multiplayer_authority", 5).unwrap();
    assert_eq!(plain.len(), 1);
    assert!(!plain[0].named);
    let mut legacy = serde_json::to_value(&page).unwrap();
    legacy.as_object_mut().unwrap().remove("search");
    assert!(serde_json::from_value::<Page>(legacy)
        .unwrap()
        .search
        .is_none());
}

#[test]
fn rich_docs_qualified_symbols_survive_topic_candidate_caps() {
    use plumb_core::article::SearchSymbol;
    let mut method = docs_page(
        "https://docs.godotengine.org/en/stable/classes/class_node.html",
        "Node — Godot documentation",
        &["Godot Node"],
        "A scene tree node.",
    );
    method.views = 1;
    method.search = Some(SearchContent {
        symbols: vec![SearchSymbol {
            identifier: "Node.set_multiplayer_authority".into(),
            anchor: Some("class-node-method-set-multiplayer-authority".into()),
        }],
        ..SearchContent::default()
    });
    let mut pages = (0..220)
        .map(|i| {
            docs_page(
                &format!("https://docs.godotengine.org/en/stable/tutorials/authority-{i}.html"),
                &format!("Multiplayer authority examples {i}"),
                &[],
                "Godot node set multiplayer authority examples.",
            )
        })
        .collect::<Vec<_>>();
    pages.push(method.clone());
    let (_dir, index) = searcher(&pages);
    for query in [
        "godot Node.set_multiplayer_authority()",
        "set_multiplayer_authority site:docs.godotengine.org",
    ] {
        let ops = Operators::parse(query);
        let found = index.search_naming_docs(&ops.words, &ops, true, 1).unwrap();
        assert_eq!(found[0].page.url, method.url, "{query}: {found:?}");
    }
    for query in [
        "godot Other.set_multiplayer_authority",
        "set_multiplayer_authority -site:godotengine.org",
        "set_multiplayer_authority site:docs.python.org",
    ] {
        let ops = Operators::parse(query);
        assert!(
            index
                .search_naming_docs(&ops.words, &ops, true, 5)
                .unwrap()
                .is_empty(),
            "{query}"
        );
    }
    assert!(docs_symbol_matches(
        "set_multiplayer_authority()",
        "Node.set_multiplayer_authority"
    ));
    assert!(!docs_symbol_matches(
        "Other.set_multiplayer_authority",
        "Node.set_multiplayer_authority"
    ));
}

#[test]
fn rich_docs_short_topics_require_source_headings_or_error_literals() {
    use plumb_core::article::SearchPassage;
    let mut page = docs_page(
        "https://kubernetes.io/docs/concepts/workloads/pods/pod-lifecycle/",
        "Pod Lifecycle",
        &["Kubernetes Pod Lifecycle"],
        "The lifecycle of a Pod.",
    );
    page.search = Some(SearchContent {
        passages: vec![SearchPassage {
            heading: "Container failures".into(),
            text: "A container can enter CrashLoopBackOff when it repeatedly fails. Generic troubleshooting advice follows.".into(),
            anchor: Some("container-restarts".into()),
        }],
        ..SearchContent::default()
    });
    let (_dir, index) = searcher(&[page]);
    for query in [
        "kubernetes CrashLoopBackOff",
        "crashloopbackoff",
        "failures",
    ] {
        let found = index
            .search_naming_docs(query, &Operators::default(), true, 5)
            .unwrap();
        assert_eq!(found.len(), 1, "{query}: {found:?}");
    }
    assert!(index
        .search_naming_docs("troubleshooting", &Operators::default(), true, 5)
        .unwrap()
        .is_empty());
}

#[test]
fn docs_pages_are_found_by_their_sections() {
    let mut structures = docs_page(
        "https://docs.python.org/3/tutorial/datastructures.html",
        "Data Structures",
        &["Python Data Structures", "Data Structures Python"],
        "This chapter describes some things you've learned about already in more detail.",
    );
    structures.sections = vec![
        "More on Lists".into(),
        "List Comprehensions".into(),
        "Nested List Comprehensions".into(),
    ];
    let sorting = docs_page(
        "https://docs.python.org/3/howto/sorting.html",
        "Sorting Techniques",
        &["Python Sorting Techniques", "Sorting Techniques Python"],
        "Python lists have a built-in list.sort() method that modifies the list in-place.",
    );
    let (_dir, searcher) = searcher(&[structures.clone(), sorting]);
    let found = |query: &str| -> Vec<(String, bool)> {
        searcher
            .search(query, 10)
            .unwrap()
            .into_iter()
            .filter(|hit| hit.page.set == DOCS_SET)
            .map(|hit| (hit.page.title, hit.named))
            .collect()
    };
    assert_eq!(
        found("python list comprehension"),
        [("Data Structures".to_string(), false)]
    );
    // A section's heading names nothing on its own.
    assert!(found("list comprehensions").is_empty());
    let read = Page::from_set(
        DOCS_SET,
        Article {
            title: structures.title.clone(),
            description: structures.description.clone(),
            item: Some(structures.url.clone()),
            views: structures.views,
            aliases: structures.aliases.clone(),
            sections: structures.sections.clone(),
            language: structures.content_language.clone(),
            ..Article::default()
        },
    );
    assert_eq!(read, Some(structures));
}

#[test]
fn docs_pages_are_found_by_a_word_on_their_site_or_when_docs_are_wanted() {
    let asyncio = docs_page(
        "https://docs.python.org/3/library/asyncio.html",
        "asyncio",
        &["Python asyncio", "asyncio Python"],
        "asyncio is a library to write concurrent code using the async/await syntax.",
    );
    let question = |title: &str, item: &str, tags: &str| {
        Page::from_question(Article {
            title: title.into(),
            description: Some(tags.into()),
            item: Some(item.into()),
            views: 1_000_000,
            ..Article::default()
        })
    };
    let (_dir, searcher) = searcher(&[
        asyncio.clone(),
        question(
            "How do I use asyncio with threads?",
            "1",
            "python, python-asyncio",
        ),
        question("asyncio: event loop is closed", "2", "python, python-3.x"),
    ]);
    let found = |query: &str, docs: bool| -> Vec<String> {
        let ops = Operators::parse(query);
        searcher
            .search_naming_docs(&ops.words, &ops, docs, 10)
            .unwrap()
            .into_iter()
            .filter(|hit| operators_allow(&ops, &hit.page))
            .map(|hit| hit.page.title)
            .collect()
    };
    // The word alone names no docs: the questions alone, as before.
    let titles = |hits: Vec<PageHit>| -> Vec<String> {
        hits.into_iter().map(|hit| hit.page.title).collect()
    };
    assert_eq!(
        found("asyncio", false),
        titles(searcher.search("asyncio", 10).unwrap())
    );
    assert!(!found("asyncio", false).contains(&"asyncio".to_string()));
    // Its site names them, and so does a search for docs pages alone,
    // by the questions it finds, which are about Python.
    assert_eq!(found("asyncio site:docs.python.org", false), ["asyncio"]);
    assert_eq!(found("asyncio site:python.org", false), ["asyncio"]);
    assert!(found("asyncio", true).contains(&"asyncio".to_string()));
    // Another docs site, or a top-level domain, names other docs.
    assert!(found("asyncio site:developer.mozilla.org", false).is_empty());
    assert!(!found("asyncio site:org", false).contains(&"asyncio".to_string()));
    // Explicit docs intent allows a short symbol while the TLD still
    // filters its host. The TLD alone supplies no product context.
    assert!(found("asyncio site:org", true).contains(&"asyncio".to_string()));

    let python = plumb_core::docs::site("python").unwrap();
    assert_eq!(naming_docs(python, "asyncio"), "Python asyncio");
    // .NET's pages are named "dotnet …" too.
    let dotnet = plumb_core::docs::site("dotnet").unwrap();
    assert_eq!(naming_docs(dotnet, "linq"), "dotnet linq");
}

#[test]
fn docs_pages_named_then_more_words_and_crowded_by_questions_are_listed() {
    let use_state = docs_page(
        "https://react.dev/reference/react/useState",
        "useState",
        &["React useState", "useState React"],
        "useState is a React Hook that lets you add a state variable to your component.",
    );
    let question = |title: &str, item: &str, views| {
        Page::from_question(Article {
            title: title.into(),
            description: Some("reactjs, react-hooks".into()),
            item: Some(item.into()),
            views,
            ..Article::default()
        })
    };
    let mut pages = vec![use_state.clone()];
    for i in 0..12 {
        pages.push(question(
            &format!("React useState hook question {i}"),
            &format!("{i}"),
            1_000_000 + i,
        ));
    }
    let (_dir, searcher) = searcher(&pages);
    // Named by its product and title, with a word of its description
    // after them, not as a book is by its title and author.
    let hits = searcher.search("react usestate hook", 10).unwrap();
    assert_eq!(hits.len(), 10);
    // The questions outnumber it, and are more read, but it stays.
    let docs: Vec<&PageHit> = hits.iter().filter(|h| h.page.set == DOCS_SET).collect();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].page.title, "useState");
    assert!(docs[0].named && !docs[0].whole);
    // Listed after the best site, besides the questions that took
    // the places for pages.
    let sites = [site("react.dev", true), site("other.com", false)];
    let placed = place_pages("react usestate hook", &sites, hits);
    let alone: Vec<&str> = placed
        .iter()
        .filter(|p| p.under.is_none())
        .map(|p| p.hit.page.title.as_str())
        .collect();
    assert!(alone.contains(&"useState"), "{alone:?}");
    assert_eq!(
        placed
            .iter()
            .find(|p| p.hit.page.set == DOCS_SET)
            .map(|p| p.at),
        Some(1)
    );
}

#[test]
fn docs_pages_of_an_api_named_as_code_lead() {
    let mdn = "https://developer.mozilla.org/en-US/docs";
    let pages = [
        docs_page(
            "https://react.dev/reference/react/useState",
            "useState",
            &[],
            "useState is a React Hook that lets you add a state variable to your component.",
        ),
        docs_page(
            &format!("{mdn}/Web/JavaScript/Reference/Global_Objects/Object/keys"),
            "Object.keys() — JavaScript",
            &[],
            "The Object.keys() static method returns an array of a given object's own property names.",
        ),
        docs_page(
            "https://en.cppreference.com/cpp/container/vector",
            "std::vector",
            &[],
            "A sequence container that encapsulates dynamic size arrays.",
        ),
        docs_page(
            "https://en.cppreference.com/cpp/memory/c/malloc",
            "std::malloc",
            &[],
            "Allocates size bytes of uninitialized storage.",
        ),
        docs_page(
            &format!("{mdn}/Glossary/jQuery"),
            "jQuery — Glossary",
            &[],
            "jQuery is a JavaScript Library that focuses on simplifying DOM manipulation.",
        ),
        docs_page(
            "https://doc.rust-lang.org/std/os/macos/index.html",
            "std::os::macos",
            &[],
            "Platform-specific definitions for macOS.",
        ),
    ];
    let (_dir, searcher) = searcher(&pages);
    // The pages listed on their own, in order, with a question placed
    // first and a site sharing a word with the query as the best.
    let placed = |query: &str, best: &str| {
        let sites = [site(best, true), site("other.com", false)];
        let mut placed = vec![PlacedPage {
            at: 0,
            under: None,
            hit: found("A question", None, false, 0.5),
        }];
        placed.extend(place_pages(
            query,
            &sites,
            searcher.search(query, 10).unwrap(),
        ));
        keep_page_rules(query, &sites, &mut placed);
        placed
            .iter()
            .map(|p| (p.hit.page.title.clone(), p.at))
            .collect::<Vec<_>>()
    };
    let docs_after_best = |query: &str, best: &str| {
        placed(query, best)
            .iter()
            .all(|(title, at)| title == "A question" || *at > 0)
    };
    for (query, best, title) in [
        ("useState", "senate.gov", "useState"),
        ("Object.keys", "keys.so", "Object.keys() — JavaScript"),
        ("Object.keys()", "keys.so", "Object.keys() — JavaScript"),
        ("std::vector", "vector.dev", "std::vector"),
    ] {
        assert_eq!(placed(query, best)[0], (title.to_string(), 0), "{query}");
    }
    // A word written plainly may mean the word, and a site called what
    // the query names is what it is for.
    assert!(docs_after_best("malloc", "mallocprivacy.com"));
    assert!(docs_after_best("jQuery", "jquery.com"));
    // Only the whole name counts: "macOS" is not Rust's std::os::macos.
    assert!(docs_after_best("macOS", "apple.com"));
}

#[test]
fn docs_pages_are_named_by_what_their_title_and_address_share() {
    let mdn = "https://developer.mozilla.org/en-US/docs/Web";
    let page = |path: &str, title: &str| docs_page(&format!("{mdn}/{path}"), title, &[], "");
    let pages = [
        page(
            "API/Document/querySelectorAll",
            "Document: querySelectorAll() method — Web APIs",
        ),
        page(
            "CSS/Reference/Properties/border-radius",
            "border-radius CSS property — CSS",
        ),
        page(
            "CSS/Reference/Properties/border-bottom-left-radius",
            "border-bottom-left-radius CSS property — CSS",
        ),
        page(
            "CSS/Guides/Backgrounds_and_borders/Border-radius_generator",
            "Border-radius generator — CSS",
        ),
        page(
            "HTTP/Reference/Headers/Content-Type",
            "Content-Type header — HTTP",
        ),
        docs_page(
            "https://docs.python.org/3/library/functions.html",
            "Built-in Functions — Python 3.14.0 documentation",
            &[],
            "",
        ),
        page("API/Window/open", "Window: open() method — Web APIs"),
    ];
    assert!(crate::pages::docs::docs_symbols(&pages[0]).contains(&"queryselectorall".to_string()));
    assert!(crate::pages::docs::docs_symbols(&pages[1]).contains(&"border-radius".to_string()));
    // A guide that starts with the name, and a title in words, name nothing.
    assert!(crate::pages::docs::docs_symbols(&pages[3]).is_empty());
    assert!(crate::pages::docs::docs_symbols(&pages[5]).is_empty());
    assert!(crate::pages::docs::docs_symbols(&pages[6]).is_empty());
    let (_dir, searcher) = searcher(&pages);
    for (query, best, title) in [
        ("querySelectorAll", "ixquick.com", &pages[0].title),
        ("border-radius", "homeaffairs.gov.au", &pages[1].title),
        ("css border-radius", "css-tricks.com", &pages[1].title),
        ("Content-Type", "pubmed.ncbi.nlm.nih.gov", &pages[4].title),
    ] {
        let sites = [site(best, false), site("other.com", false)];
        let placed = place_pages(query, &sites, searcher.search(query, 10).unwrap());
        let first = placed.iter().find(|p| p.at == 0 && p.under.is_none());
        assert_eq!(
            first.map(|p| &p.hit.page.title),
            Some(title),
            "{query}: {placed:?}"
        );
    }
    // A word is still not a symbol.
    assert!(searcher.search("functions", 10).unwrap().is_empty());
    assert!(searcher.search("open", 10).unwrap().is_empty());
}

#[test]
fn manual_pages_are_named_without_their_section() {
    let man = |name: &str, section: &str| {
        docs_page(
            &format!(
                "https://man7.org/linux/man-pages/man{}/{name}.{section}.html",
                &section[..1]
            ),
            &format!("{name}({section})"),
            &[],
            "man7.org > Linux > man-pages",
        )
    };
    let pages = [
        man("ls", "1"),
        man("flatpak-remote-ls", "1"),
        man("ls", "1p"),
        man("tar", "1"),
        man("git-get-tar-commit-id", "1"),
    ];
    let (_dir, searcher) = searcher(&pages);
    for (query, title) in [
        ("man ls", "ls(1)"),
        ("man tar", "tar(1)"),
        ("linux tar", "tar(1)"),
    ] {
        let hits = searcher.search(query, 10).unwrap();
        assert_eq!(hits[0].page.title, title, "{query}: {hits:?}");
        assert!(hits[0].named, "{query}");
        // It comes before a site called "man" something.
        let sites = [site("manchester.ac.uk", false), site("other.com", false)];
        let placed = place_pages(query, &sites, hits);
        let first = placed.iter().find(|p| p.at == 0 && p.under.is_none());
        assert_eq!(
            first.map(|p| p.hit.page.title.as_str()),
            Some(title),
            "{query}"
        );
    }
    // A bare name may be anything.
    assert!(searcher.search("ls", 10).unwrap().is_empty());
}

#[test]
fn docs_pages_named_by_part_of_a_search_keep_the_rest_as_detail() {
    let with_sections = |mut page: Page, sections: &[&str]| {
        page.sections = sections.iter().map(|s| s.to_string()).collect();
        page
    };
    // Titles, other names and sections as the docs pages have them.
    let logging = with_sections(
        docs_page(
            "https://docs.python.org/3/library/logging.html",
            "logging — Logging facility for Python",
            &["Python logging", "logging Python"],
            "Source code: Lib/logging/__init__.py This module defines functions and classes which implement a flexible event logging system.",
        ),
        &["Logger Objects", "Logging Levels", "Handler Objects", "Module-Level Functions"],
    );
    let pathlib = with_sections(
        docs_page(
            "https://docs.python.org/3/library/pathlib.html",
            "pathlib — Object-oriented filesystem paths",
            &["Python pathlib", "pathlib Python"],
            "Source code: Lib/pathlib/ This module offers classes representing filesystem paths.",
        ),
        &[
            "Basic use",
            "Pure paths",
            "Concrete paths",
            "Pattern language",
            "Comparison to the glob module",
        ],
    );
    let random = with_sections(
        docs_page(
            "https://docs.python.org/3/library/random.html",
            "random — Generate pseudo-random numbers",
            &["Python random", "random Python"],
            "Source code: Lib/random.py This module implements pseudo-random number generators for various distributions.",
        ),
        &["Bookkeeping functions", "Functions for integers", "Functions for sequences", "Examples"],
    );
    let rebase = with_sections(
        docs_page(
            "https://git-scm.com/docs/git-rebase",
            "git-rebase",
            &["git rebase", "Git git-rebase", "git-rebase Git"],
            "",
        ),
        &[
            "NAME",
            "SYNOPSIS",
            "DESCRIPTION",
            "OPTIONS",
            "MERGE STRATEGIES",
            "INTERACTIVE MODE",
            "SPLITTING COMMITS",
        ],
    );
    let effect = with_sections(
        docs_page(
            "https://react.dev/reference/react/useEffect",
            "useEffect",
            &["React useEffect", "useEffect React"],
            "useEffect is a React Hook that lets you synchronize a component with an external system.",
        ),
        &["Reference", "Usage", "Troubleshooting"],
    );
    let array = with_sections(
        docs_page(
            "https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Array",
            "Array — JavaScript",
            &["MDN Array", "Array MDN", "JavaScript Array", "Array JavaScript"],
            "The Array object enables storing a collection of multiple items under a single variable name, and flat() makes it flat.",
        ),
        &["Description", "Constructor", "Static methods", "Instance methods", "Examples"],
    );
    let flat = docs_page(
        "https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Array/flat",
        "Array.prototype.flat() — JavaScript",
        &["MDN Array.prototype.flat()", "Array.prototype.flat() MDN", "JavaScript Array.prototype.flat()", "Array.prototype.flat() JavaScript"],
        "The flat() method of Array instances creates a new array with all sub-array elements concatenated into it.",
    );
    let (_dir, searcher) = searcher(&[logging, pathlib, random, rebase, effect, array, flat]);
    let first_docs = |query: &str| {
        searcher
            .search(query, 10)
            .unwrap()
            .into_iter()
            .find(|hit| hit.page.set == DOCS_SET)
            .map(|hit| (hit.page.title, hit.named))
    };
    for (query, title) in [
        // The rest is written as code: a function of the module.
        (
            "python logging basicConfig",
            "logging — Logging facility for Python",
        ),
        (
            "logging basicConfig python",
            "logging — Logging facility for Python",
        ),
        ("react useEffect cleanup", "useEffect"),
        // The rest is on the page.
        (
            "python pathlib glob",
            "pathlib — Object-oriented filesystem paths",
        ),
        ("git rebase interactive", "git-rebase"),
        ("git interactive rebase", "git-rebase"),
    ] {
        assert_eq!(
            first_docs(query),
            Some((title.to_string(), true)),
            "{query}"
        );
        let hits = searcher.search(query, 10).unwrap();
        let placed = place_pages(query, &[site("other.com", false)], hits);
        assert!(
            placed
                .iter()
                .any(|p| p.hit.page.title == title && p.at <= 1),
            "{query}: {placed:?}"
        );
    }
    // A page the whole search names is still named so.
    assert_eq!(
        first_docs("python logging"),
        Some(("logging — Logging facility for Python".to_string(), true))
    );
    // Nothing says random's page is about forests, nor is a version code.
    for query in ["python random forest", "python 3.12 random forest"] {
        assert!(
            searcher
                .search(query, 10)
                .unwrap()
                .iter()
                .all(|hit| !hit.page.title.starts_with("random")),
            "{query}"
        );
    }
    // A page whose title has every word comes before the one a part names.
    let hits = searcher.search("javascript array flat", 10).unwrap();
    assert_eq!(hits[0].page.title, "Array.prototype.flat() — JavaScript");
    assert!(hits
        .iter()
        .all(|hit| !hit.named || !hit.page.title.starts_with("Array —")));
}

#[test]
fn docs_pages_asked_for_as_a_tutorial_or_examples_are_named() {
    let asyncio = docs_page(
        "https://docs.python.org/3/library/asyncio.html",
        "asyncio — Asynchronous I/O",
        &["Python asyncio", "asyncio Python"],
        "asyncio is a library to write concurrent code using the async/await syntax.",
    );
    let tutorial = docs_page(
        "https://docs.python.org/3/tutorial/index.html",
        "The Python Tutorial",
        &["Python The Python Tutorial"],
        "Python is an easy to learn, powerful programming language.",
    );
    let (_dir, searcher) = searcher(&[asyncio, tutorial]);
    for query in [
        "python asyncio tutorial",
        "asyncio python examples",
        "python asyncio guide",
    ] {
        let hits = searcher.search(query, 10).unwrap();
        assert_eq!(hits[0].page.title, "asyncio — Asynchronous I/O", "{query}");
        assert!(hits[0].named, "{query}: {hits:?}");
    }
    // The word alone is still the topic: the tutorial, not asyncio.
    let hits = searcher.search("python tutorial", 10).unwrap();
    assert!(
        hits.iter()
            .all(|hit| hit.page.title != "asyncio — Asynchronous I/O"),
        "{hits:?}"
    );
    // A page titled so keeps its place first: the argparse tutorial.
    let argparse = docs_page(
        "https://docs.python.org/3/library/argparse.html",
        "argparse — Parser for command-line options, arguments and subcommands",
        &["Python argparse", "argparse Python"],
        "The argparse module makes it easy to write user-friendly command-line interfaces.",
    );
    let howto = docs_page(
        "https://docs.python.org/3/howto/argparse.html",
        "Argparse Tutorial",
        &["Python Argparse Tutorial", "Argparse Tutorial Python"],
        "This tutorial is intended to be a gentle introduction to argparse.",
    );
    let (_dir2, searcher) = super::searcher(&[argparse, howto]);
    let hits = searcher.search("python argparse tutorial", 10).unwrap();
    assert_eq!(hits[0].page.title, "Argparse Tutorial", "{hits:?}");
    let sites = [site("python.org", true), site("other.com", false)];
    let placed = place_pages("python argparse tutorial", &sites, hits);
    let first = placed.iter().find(|p| p.under.is_none());
    assert_eq!(
        first.map(|p| p.hit.page.title.as_str()),
        Some("Argparse Tutorial")
    );
}
