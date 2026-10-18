//! The page index's golden file, `golden.txt` next to this one: what the
//! index makes of a fixed set of pages of every page set, and what each of
//! its searches finds in it. It holds every document as the index keeps
//! it ([`PageSearcher::dump`]): the page as a search reads it back, its
//! popularity and each term of each field. Then for each query, what the
//! searches a node makes of the page index give: the pages found (by
//! [`PageSearcher::search`], [`PageSearcher::entities`] and
//! [`PageSearcher::search_naming_docs`]), with their scores to 4 decimal
//! places, the spelling suggested, the word defined, the song known, the
//! papers found, the page a part of the query means, the page a site's
//! search names, a typed address's page, the titles lent to untitled
//! sites and how read the sites' articles are.
//!
//! The pages come as a node reads them from each set's articles file
//! (`Page::from_set`): an article's profiles, facts, lead and other names
//! from its line of profiles, a person's site left out when it is a page
//! of a bigger one, papers' metadata, packages' cards, docs pages'
//! sections and source text. Some come as a node reads them from the
//! project's item sets (`items::page_of_item`): a person whose item was
//! written before a person's site on a bigger site was left out, a title
//! not in NFC, a docs page and a paper.
//!
//! The file was written by the page index that stored each page's JSON,
//! before the index moved onto items (step 11 of the data format plan),
//! so it says the two find the same pages the same way. It is never
//! written again to make a test pass: a difference is a change in search
//! results. When they are meant to change, `PLUMB_WRITE_GOLDEN=1 cargo
//! test -p plumb-index pages::tests::golden` writes it, and the change goes
//! through the Batch evals.

use std::fmt::Write as _;
use std::path::Path;

use plumb_core::article::{
    articles_of, write_article, SearchContent, SearchPassage, SearchSymbol, ARTICLES_HEADER,
    SEARCH_VERSION,
};
use plumb_core::facts::{Fact, FactKind};
use plumb_core::papers::{PaperCountKind, PaperMetadata};
use plumb_core::profiles::Profile;

use super::*;

fn article(title: &str, views: u64) -> Article {
    Article {
        title: title.into(),
        views,
        ..Article::default()
    }
}

fn fact(kind: FactKind, value: &str) -> Fact {
    Fact {
        kind,
        value: value.into(),
    }
}

fn profile(service: &str, id: &str) -> Profile {
    Profile {
        service: service.into(),
        id: id.into(),
    }
}

fn strings(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|text| text.to_string()).collect()
}

/// English Wikipedia's articles: people (one whose site is a page of a
/// bigger one), companies with sites, profiles and facts, articles with
/// leads and other names, works, places and things.
fn english_articles() -> Vec<Article> {
    let described = |title: &str, views: u64, description: &str| Article {
        description: Some(description.into()),
        ..article(title, views)
    };
    vec![
        Article {
            item: Some("Q7186".into()),
            aliases: strings(&["Madame Curie", "Maria Skłodowska"]),
            facts: vec![
                fact(FactKind::Born, "1867-11-07"),
                fact(FactKind::Died, "1934-07-04"),
                fact(FactKind::Birthplace, "Warsaw"),
            ],
            lead: Some(
                "Maria Salomea Skłodowska-Curie (7 November 1867 – 4 July 1934), known simply \
                 as Marie Curie, was a Polish and naturalised-French physicist and chemist who \
                 conducted pioneering research on radioactivity."
                    .into(),
            ),
            names: strings(&["Marie Skłodowska Curie", "Curie, Marie"]),
            ..described(
                "Marie Curie",
                80_000,
                "Polish and naturalised-French physicist and chemist (1867–1934)",
            )
        },
        Article {
            item: Some("Q28865".into()),
            site: Some("python.org".into()),
            aliases: strings(&["Python language", "Python programming language"]),
            profiles: vec![profile("github", "python"), profile("x", "ThePSF")],
            facts: vec![fact(FactKind::Founded, "1991-02-20")],
            lead: Some(
                "Python is a high-level, general-purpose programming language. Its design \
                 philosophy emphasizes code readability with the use of significant indentation."
                    .into(),
            ),
            ..described(
                "Python (programming language)",
                120_000,
                "General-purpose programming language",
            )
        },
        Article {
            item: Some("Q271376".into()),
            aliases: strings(&["Python snake"]),
            names: strings(&["Pythonidae"]),
            ..described("Python (genus)", 20_000, "Genus of reptiles")
        },
        Article {
            item: Some("Q359442".into()),
            site: Some("senate.gov".into()),
            website: Some("https://www.sanders.senate.gov/".into()),
            aliases: strings(&["Bernard Sanders"]),
            profiles: vec![profile("x", "BernieSanders")],
            facts: vec![fact(FactKind::Born, "1941-09-08")],
            lead: Some(
                "Bernard Sanders (born September 8, 1941) is an American politician and \
                 activist who is the senior United States senator from Vermont."
                    .into(),
            ),
            ..described(
                "Bernie Sanders",
                150_000,
                "American politician and activist (born 1941)",
            )
        },
        Article {
            item: Some("Q265852".into()),
            site: Some("apple.com".into()),
            website: Some("https://www.apple.com/leadership/tim-cook/".into()),
            facts: vec![fact(FactKind::Born, "1960-11-01")],
            ..described(
                "Tim Cook",
                90_000,
                "American business executive (born 1960)",
            )
        },
        Article {
            item: Some("Q28404534".into()),
            site: Some("youtube.com".into()),
            website: Some("https://music.youtube.com/".into()),
            profiles: vec![profile("x", "youtubemusic")],
            ..described("YouTube Music", 60_000, "Music streaming service")
        },
        Article {
            item: Some("Q312".into()),
            site: Some("apple.com".into()),
            aliases: strings(&["Apple Computer", "Apple Inc"]),
            profiles: vec![
                profile("x", "Apple"),
                profile("youtube", "UCE_M8A5yxnLfW0KghEeajjw"),
            ],
            facts: vec![
                fact(FactKind::Founded, "1976-04-01"),
                fact(FactKind::Founder, "Steve Jobs"),
                fact(FactKind::Ceo, "Tim Cook"),
                fact(FactKind::Headquarters, "Cupertino"),
            ],
            lead: Some(
                "Apple Inc. is an American multinational technology company headquartered in \
                 Cupertino, California."
                    .into(),
            ),
            ..described("Apple Inc.", 400_000, "American technology company")
        },
        Article {
            item: Some("Q89".into()),
            ..described("Apple", 200_000, "Fruit of the apple tree")
        },
        Article {
            item: Some("Q524629".into()),
            site: Some("chase.com".into()),
            facts: vec![
                fact(FactKind::Founded, "1799"),
                fact(FactKind::Headquarters, "New York City"),
            ],
            ..described("Chase Bank", 9_000, "American bank")
        },
        Article {
            item: Some("Q2015620".into()),
            site: Some("usbank.com".into()),
            aliases: strings(&["U.S. Bancorp", "US Bank"]),
            ..described("U.S. Bank", 12_000, "American bank holding company")
        },
        Article {
            item: Some("Q1074091".into()),
            site: Some("nypost.com".into()),
            ..described("New York Post", 70_000, "American daily newspaper")
        },
        Article {
            item: Some("Q10520".into()),
            names: strings(&["Manubrium", "Breastbone"]),
            lead: Some(
                "The sternum or breastbone is a long flat bone located in the central part of \
                 the chest."
                    .into(),
            ),
            ..described("Sternum", 30_000, "Long flat bone in the chest")
        },
        Article {
            item: Some("Q23445".into()),
            lead: Some(
                "The Mesozoic Era is the second-to-last era of Earth's geological history, \
                 lasting from about 252 to 66 million years ago and comprising the Triassic, \
                 Jurassic and Cretaceous Periods."
                    .into(),
            ),
            ..described("Mesozoic", 40_000, "Era of geological history")
        },
        described("Triassic", 30_000, "First period of the Mesozoic Era"),
        described("Free (album)", 1_000, "1969 studio album by Free"),
        Article {
            item: Some("Q65".into()),
            aliases: strings(&["No Way Home", "Spider-Man 3"]),
            ..described("Spider-Man: No Way Home", 200_000, "2021 film by Jon Watts")
        },
        described(
            "The Dark Side of the Moon",
            90_000,
            "1973 studio album by Pink Floyd",
        ),
        Article {
            item: Some("Q15862".into()),
            site: Some("queenonline.com".into()),
            ..described("Queen (band)", 150_000, "British rock band")
        },
        described("Queen (album)", 8_000, "1973 studio album by Queen"),
        Article {
            item: Some("Q517".into()),
            aliases: strings(&["Napoleon Bonaparte", "Napoleon I", "Bonaparte"]),
            facts: vec![
                fact(FactKind::Born, "1769-08-15"),
                fact(FactKind::Died, "1821-05-05"),
            ],
            lead: Some(
                "Napoleon Bonaparte (born Napoleone di Buonaparte; 15 August 1769 – 5 May \
                 1821) was a French military officer and statesman who rose to prominence \
                 during the French Revolution and became Emperor of the French."
                    .into(),
            ),
            ..described(
                "Napoleon",
                250_000,
                "French emperor and military leader (1769–1821)",
            )
        },
        Article {
            item: Some("Q215880".into()),
            lead: Some(
                "A hamstring is any one of the three posterior thigh muscles between the hip \
                 and the knee."
                    .into(),
            ),
            ..described("Hamstring", 25_000, "Muscles at the back of the thigh")
        },
        described("Tariff", 50_000, "Tax on imports or exports"),
        Article {
            item: Some("Q111".into()),
            facts: vec![
                fact(FactKind::OrbitalPeriod, "687 days"),
                fact(FactKind::Radius, "3389.5 km"),
            ],
            ..described("Mars", 300_000, "Fourth planet from the Sun")
        },
        Article {
            item: Some("Q1048316".into()),
            site: Some("mars.com".into()),
            aliases: strings(&["Mars Inc"]),
            ..described(
                "Mars Inc.",
                40_000,
                "American multinational manufacturer of confectionery",
            )
        },
        Article {
            item: Some("Q142".into()),
            facts: vec![
                fact(FactKind::Capital, "Paris"),
                fact(FactKind::Population, "68042591"),
                fact(FactKind::Currency, "Euro"),
            ],
            ..described("France", 400_000, "Country in Western Europe")
        },
        Article {
            item: Some("Q90".into()),
            facts: vec![fact(FactKind::Population, "2102650")],
            ..described("Paris", 350_000, "Capital and largest city of France")
        },
        Article {
            item: Some("Q7416".into()),
            facts: vec![
                fact(FactKind::Born, "1925-10-13"),
                fact(FactKind::Died, "2013-04-08"),
            ],
            ..described(
                "Margaret Thatcher",
                200_000,
                "Prime Minister of the United Kingdom from 1979 to 1990",
            )
        },
        described("Thatching", 9_000, "Building a roof with dry vegetation"),
        Article {
            item: Some("Q9696".into()),
            aliases: strings(&["JFK", "John Kennedy"]),
            facts: vec![
                fact(FactKind::Born, "1917-05-29"),
                fact(FactKind::Died, "1963-11-22"),
            ],
            ..described(
                "John F. Kennedy",
                500_000,
                "President of the United States from 1961 to 1963",
            )
        },
        Article {
            item: Some("Q6390208".into()),
            facts: vec![fact(FactKind::Born, "1972-08-08")],
            ..described(
                "Kennedy (commentator)",
                10_000,
                "American political commentator",
            )
        },
        Article {
            item: Some("Q25173".into()),
            aliases: strings(&["Titanic"]),
            ..described("RMS Titanic", 300_000, "British ocean liner")
        },
        described("Titanic (1997 film)", 250_000, "1997 film by James Cameron"),
        described("Mitosis", 60_000, "Process of cell division"),
        described("Meiosis", 50_000, "Cell division producing gametes"),
        described("Tomato", 300_000, "Edible berry of the tomato plant"),
        described(
            "Fruit",
            250_000,
            "Seed-bearing structure of flowering plants",
        ),
        described("Linked list", 40_000, "Data structure"),
        Article {
            aliases: strings(&["C#", "C Sharp"]),
            ..described(
                "C Sharp (programming language)",
                90_000,
                "Programming language by Microsoft",
            )
        },
        described("Lien", 20_000, "Form of security interest"),
        described("Abuse", 15_000, "Improper usage or treatment"),
        described(
            "Memantine",
            9_000,
            "Medication used for Alzheimer's disease",
        ),
    ]
}

/// The pages of every other set, as their articles files write them, most
/// read first in each set.
fn other_sets() -> Vec<(&'static str, Vec<Article>)> {
    let described = |title: &str, views: u64, description: &str, item: &str| Article {
        description: Some(description.into()),
        item: Some(item.into()),
        ..article(title, views)
    };
    let package = |registry: &str, name: &str, views: u64, description: &str| {
        let key = plumb_core::packages::registry(registry).unwrap();
        Article {
            aliases: key
                .short_name(name)
                .map(str::to_string)
                .into_iter()
                .collect(),
            package: Some(PackageInfo {
                registry: registry.into(),
                name: name.into(),
                version: Some("1.0.215".into()),
                released: Some("2026-01-02".into()),
                license: Some("MIT OR Apache-2.0".into()),
                docs: None,
                repo: Some(format!("https://github.com/example/{name}")),
                homepage: None,
            }),
            ..described(
                name,
                views,
                description,
                &plumb_core::packages::package_item(registry, name),
            )
        }
    };
    let docs = plumb_core::article::articles_of(
        include_str!("../../../tests/fixtures/short-docs.tsv")
            .lines()
            .map(str::to_string),
    )
    .map(|(_, article)| article.unwrap())
    .chain([Article {
        sections: strings(&["Running an asyncio Program", "Tasks", "Sleeping"]),
        search: Some(SearchContent {
            version: SEARCH_VERSION,
            symbols: vec![SearchSymbol {
                identifier: "asyncio.run".into(),
                anchor: Some("asyncio.run".into()),
            }],
            passages: vec![SearchPassage {
                heading: "Running an asyncio Program".into(),
                text: "Execute the coroutine and return the result, managing the event loop."
                    .into(),
                anchor: Some("running-an-asyncio-program".into()),
            }],
        }),
        aliases: strings(&["Python asyncio", "asyncio Python"]),
        ..described(
            "asyncio — Asynchronous I/O",
            3_000,
            "asyncio is a library to write concurrent code using the async/await syntax.",
            "https://docs.python.org/3/library/asyncio.html",
        )
    }])
    .collect();
    vec![
        (
            "wikipedia-es",
            vec![described(
                "Galletas",
                5_000,
                "Recetas fáciles de galletas caseras",
                "Q13266",
            )],
        ),
        (
            "wikipedia-de",
            vec![described(
                "Gesunde Ernährung",
                4_000,
                "Ernährung und Bewegung",
                "Q2095",
            )],
        ),
        (
            GITHUB_SET,
            vec![
                Article {
                    site: Some("kernel.org".into()),
                    ..described("torvalds/linux", 180_000, "Linux kernel source tree", "")
                },
                Article {
                    site: Some("rust-lang.org".into()),
                    aliases: strings(&["rust"]),
                    ..described(
                        "rust-lang/rust",
                        95_000,
                        "Empowering everyone to build reliable and efficient software.",
                        "",
                    )
                },
                described(
                    "google-deepmind/sonnet",
                    9_800,
                    "TensorFlow-based neural network library",
                    "",
                ),
            ],
        ),
        (
            STACKOVERFLOW_SET,
            vec![
                Article {
                    aliases: strings(&[
                        "How to revert a commit in Git?",
                        "Undo the last git commit",
                    ]),
                    ..described(
                        "How do I undo the most recent local commits in Git?",
                        13_000_000,
                        "git, version-control, git-commit, undo",
                        "927358",
                    )
                },
                described(
                    "How do I delete a Git branch locally and remotely?",
                    11_000_000,
                    "git, version-control, git-branch, git-push, git-remote",
                    "2003505",
                ),
                described(
                    "How to sort a list of strings ignoring whitespace in Python?",
                    40_000,
                    "python, list, sorting",
                    "36139",
                ),
            ],
        ),
        (
            STACKEXCHANGE_SET,
            vec![
                described(
                    "How to get rid of aphids on indoor plants?",
                    90_000,
                    "pest-control, aphids, houseplants",
                    "gardening.stackexchange.com/1234",
                ),
                described(
                    "How do I find which process is using a port?",
                    250_000,
                    "linux, networking, ports",
                    "unix.stackexchange.com/106561",
                ),
            ],
        ),
        (
            BOOKS_SET,
            vec![
                Article {
                    aliases: strings(&["Dune Frank Herbert"]),
                    ..described("Dune", 50_000, "Book by Frank Herbert", "OL893415W")
                },
                described("C# in Depth", 5_000, "Book by Jon Skeet", "OL17055567W"),
            ],
        ),
        (
            PODCASTS_SET,
            vec![
                Article {
                    site: Some("nytimes.com".into()),
                    aliases: strings(&["The Daily podcast"]),
                    profiles: vec![profile("spotify-show", "3IM0lmZxpFAY7CwMuv9H4g")],
                    ..described(
                        "The Daily",
                        120_000,
                        "Podcast by The New York Times · News",
                        "743229",
                    )
                },
                Article {
                    site: Some("dancarlin.com".into()),
                    aliases: strings(&["Dan Carlin podcast"]),
                    ..described(
                        "Dan Carlin's Hardcore History",
                        90_070,
                        "Podcast by Dan Carlin · History",
                        "1",
                    )
                },
            ],
        ),
        (
            MUSIC_SET,
            vec![
                Article {
                    aliases: strings(&["Smells Like Teen Spirit Nirvana"]),
                    profiles: vec![
                        profile("youtube-video", "hTWKbfoikeg"),
                        profile("spotify-track", "1f3yAtsJtY87CTmM8RLnxf"),
                    ],
                    ..described(
                        "Smells Like Teen Spirit",
                        321_094,
                        "Song by Nirvana, 1991",
                        "recording/5fb524f1-8cc8-4c04-a921-e34c0a911ea7",
                    )
                },
                Article {
                    aliases: strings(&["Nevermind Nirvana"]),
                    ..described(
                        "Nevermind",
                        250_000,
                        "Album by Nirvana, 1991",
                        "release-group/1b022e01-4da6-387b-8658-8678046e4cef",
                    )
                },
                Article {
                    aliases: strings(&["Free Bird Lynyrd Skynyrd"]),
                    ..described(
                        "Free Bird",
                        120_000,
                        "Song by Lynyrd Skynyrd, 1973",
                        "recording/8e4a59a7-5fd1-4bce-bd53-bb72d5b7d1f0",
                    )
                },
            ],
        ),
        (
            FILMS_SET,
            vec![
                Article {
                    aliases: strings(&["Inception film"]),
                    ..described(
                        "Inception",
                        310,
                        "2010 film by Christopher Nolan",
                        "Q25188/Inception",
                    )
                },
                described(
                    "Breaking Bad",
                    290,
                    "American television series (2008–2013)",
                    "Q1079",
                ),
            ],
        ),
        (DOCS_SET, docs),
        (
            REFERENCE_SET,
            vec![
                described(
                    "Dengue Fever: Symptoms, Causes, and Treatment",
                    800,
                    "Dengue fever is a mosquito-borne illness. Learn about its symptoms, causes \
                     and how it is treated.",
                    "https://www.healthline.com/health/dengue-fever",
                ),
                described(
                    "Prioritize Definition & Meaning",
                    700,
                    "The meaning of PRIORITIZE is to list or rate in order of priority.",
                    "https://www.merriam-webster.com/dictionary/prioritize",
                ),
            ],
        ),
        (
            REFERENCE2_SET,
            vec![described(
                "How to Change a Car Battery",
                600,
                "Step by step: disconnect the negative terminal first, then the positive.",
                "https://www.wikihow.com/Change-a-Car-Battery",
            )],
        ),
        (
            SUBPAGES_SET,
            vec![
                described(
                    "Dengue and severe dengue",
                    900,
                    "WHO fact sheet on dengue: key facts, transmission, symptoms and treatment.",
                    "https://www.who.int/news-room/fact-sheets/detail/dengue-and-severe-dengue",
                ),
                described(
                    "Admissions",
                    500,
                    "Apply to Harvard College: first-year and transfer admissions.",
                    "https://college.harvard.edu/admissions",
                ),
            ],
        ),
        (
            WANTED_SET,
            vec![described(
                "Mortgage rates today",
                500,
                "Compare today's mortgage rates from U.S. Bank.",
                "https://www.usbank.com/home-loans/mortgage/mortgage-rates.html",
            )],
        ),
        (
            PAPERS_SET,
            vec![
                Article {
                    website: Some("https://arxiv.org/abs/1706.03762".into()),
                    paper: Some(PaperMetadata {
                        doi: Some("10.48550/arxiv.1706.03762".into()),
                        arxiv_id: Some("1706.03762".into()),
                        openalex_id: Some("W2963403868".into()),
                        authors: strings(&["Ashish Vaswani", "Noam Shazeer", "Niki Parmar"]),
                        publication_date: Some("2017-06-12".into()),
                        publication_year: Some(2017),
                        venue: Some("Advances in Neural Information Processing Systems".into()),
                        source: "openalex".into(),
                        count_kind: PaperCountKind::Citations,
                        count: 120_000,
                        ..PaperMetadata::default()
                    }),
                    ..described(
                        "Attention Is All You Need",
                        120_000,
                        "The dominant sequence transduction models are based on complex \
                         recurrent or convolutional neural networks.",
                        "10.48550/arxiv.1706.03762",
                    )
                },
                Article {
                    paper: Some(PaperMetadata {
                        doi: Some("10.1109/cvpr.2016.90".into()),
                        authors: strings(&["Kaiming He", "Xiangyu Zhang"]),
                        publication_date: Some("2016-06-01".into()),
                        publication_year: Some(2016),
                        venue: Some("Computer Vision and Pattern Recognition".into()),
                        source: "openalex".into(),
                        count_kind: PaperCountKind::Citations,
                        count: 150_000,
                        ..PaperMetadata::default()
                    }),
                    ..described(
                        "Deep Residual Learning for Image Recognition",
                        110_000,
                        "Deeper neural networks are more difficult to train.",
                        "10.1109/cvpr.2016.90",
                    )
                },
                Article {
                    paper: Some(PaperMetadata {
                        authors: strings(&["Stephen F. Altschul"]),
                        publication_year: Some(1990),
                        source: "openalex".into(),
                        count_kind: PaperCountKind::Citations,
                        count: 90_000,
                        ..PaperMetadata::default()
                    }),
                    ..described(
                        "Basic local alignment search tool",
                        90_000,
                        "A new approach to rapid sequence comparison.",
                        "W2055043387",
                    )
                },
            ],
        ),
        (
            PACKAGES_SET,
            vec![
                package(
                    "npm",
                    "react",
                    900_000,
                    "React is a JavaScript library for building user interfaces.",
                ),
                package("pypi", "requests", 800_000, "Python HTTP for Humans."),
                package(
                    "crates",
                    "serde",
                    700_000,
                    "A generic serialization/deserialization framework",
                ),
                package(
                    "go",
                    "github.com/gin-gonic/gin",
                    300_000,
                    "Gin is a HTTP web framework written in Go",
                ),
            ],
        ),
        (
            WIKIDATA_SET,
            vec![Article {
                site: Some("linustechtips.com".into()),
                profiles: vec![profile("youtube", "UCXuqSBlHAE6Xw-yeJA0Tunw")],
                facts: vec![fact(FactKind::Founded, "2008-11-24")],
                aliases: strings(&["LTT"]),
                ..described(
                    "Linus Tech Tips",
                    40,
                    "Canadian technology YouTube channel",
                    "Q30325435",
                )
            }],
        ),
        (
            WIKTIONARY_SET,
            vec![
                described("free", 900, "(adjective) Unconstrained.", ""),
                described("AWOL", 300, "(adjective) Absent without leave.", ""),
                described(
                    "prioritize",
                    200,
                    "(verb) To arrange in order of importance.",
                    "",
                ),
                described("Free", 5, "(noun) A surname.", ""),
            ],
        ),
    ]
}

/// Pages of the project's item sets, as a node reads them
/// ([`crate::items::page_of_item`]), each set's after its file's: a person
/// whose item was written before a person's site on a bigger site was left
/// out (2026-10-13), so it still has it, an article whose title is not in
/// NFC (its `page` field holds it escaped) and that declares another
/// language than its set's, a docs page with source text, and a paper with
/// its authors, venue, dates and a verified arXiv copy.
fn item_set_pages() -> Vec<Page> {
    let items = [
        (
            "wikipedia-en",
            serde_json::json!({
                "id": "https://en.wikipedia.org/wiki/Elizabeth_Warren", "kind": "article",
                "url": "https://en.wikipedia.org/wiki/Elizabeth_Warren",
                "title": "Elizabeth Warren", "site": "wikipedia.org",
                "about": "American politician and academic (born 1949)",
                "text": "Elizabeth Ann Warren (born June 22, 1949) is an American politician and former law professor.",
                "lang": "en", "weight": 120000.0, "ids": {"wd": "Q434706"},
                "names": ["Senator Warren"],
                "edges": [{"rel": "about", "to": "site:senate.gov"}],
                "fields": {
                    "website": "https://www.warren.senate.gov/",
                    "facts": [{"kind": "born", "value": "1949-06-22"}],
                    "profiles": [{"service": "x", "id": "SenWarren"}],
                    "other_names": ["Warren, Elizabeth"],
                },
            }),
        ),
        (
            "wikipedia-en",
            serde_json::json!({
                "id": "https://en.wikipedia.org/wiki/Cafe_Society", "kind": "article",
                "url": "https://en.wikipedia.org/wiki/Cafe_Society",
                "title": "Café Society", "site": "wikipedia.org",
                "about": "Term for a set of fashionable people", "lang": "fr",
                "weight": 3000.0,
                "fields": {"page": {"title": "Cafe\\u{301} Society", "escaped": ["title"]}},
            }),
        ),
        (
            DOCS_SET,
            serde_json::json!({
                "id": "https://docs.rs/serde/latest/serde/trait.Serialize.html",
                "kind": "docs-page",
                "url": "https://docs.rs/serde/latest/serde/trait.Serialize.html",
                "title": "Serialize in serde", "site": "docs.rs",
                "about": "A data structure that can be serialized into any data format supported by Serde.",
                "weight": 2000.0, "names": ["serde Serialize", "Serialize serde"],
                "fields": {
                    "sections": ["Required Methods", "Implementors"],
                    "symbols": [{"identifier": "serde::Serialize", "anchor": "tymethod.serialize"}],
                    "passages": [{"heading": "Required Methods", "text": "Serialize this value into the given Serde serializer."}],
                },
            }),
        ),
        (
            PAPERS_SET,
            serde_json::json!({
                "id": "10.1038/nature14539", "kind": "paper",
                "url": "https://doi.org/10.1038/nature14539",
                "title": "Deep learning", "site": "doi.org",
                "about": "Deep learning allows computational models that are composed of multiple processing layers to learn representations of data.",
                "weight": 80000.0, "published": "2015-05-27",
                "ids": {"doi": "10.1038/nature14539", "openalex": "W1919803322"},
                "fields": {
                    "authors": ["Yann LeCun", "Yoshua Bengio", "Geoffrey Hinton"],
                    "venue": "Nature",
                    "paper": {
                        "doi": "10.1038/nature14539", "openalex_id": "W1919803322",
                        "publication_date": "2015-05-27", "publication_year": 2015,
                        "source": "openalex", "count_kind": "citations", "count": 80000,
                    },
                    "verified_arxiv": {
                        "id": "1506.00000", "title": "Deep learning",
                        "authors": ["Yann LeCun"], "submitted": "2015-05-27",
                    },
                    "website": "https://www.nature.com/articles/nature14539",
                },
            }),
        ),
    ];
    items
        .into_iter()
        .map(|(set, item)| {
            let item: plumb_format::Item = serde_json::from_value(item).unwrap();
            crate::items::page_of_item(&item, set).unwrap()
        })
        .collect()
}

/// The golden pages, as a node reads each set's articles file: written by
/// `plumb-tools fetch-pages`, most read first, read back line by line;
/// and then the pages of item sets ([`item_set_pages`]).
fn golden_pages() -> Vec<Page> {
    let mut pages = file_pages();
    pages.extend(item_set_pages());
    pages
}

fn file_pages() -> Vec<Page> {
    let mut sets = vec![("wikipedia-en", english_articles())];
    sets.extend(other_sets());
    let mut pages = Vec::new();
    for (set, mut articles) in sets {
        articles.sort_by_key(|article| std::cmp::Reverse(article.views));
        let mut file = ARTICLES_HEADER.as_bytes().to_vec();
        for article in &mut articles {
            if article.item.as_deref() == Some("") {
                article.item = None;
            }
            write_article(&mut file, article).unwrap();
        }
        let file = String::from_utf8(file).unwrap();
        let read: Vec<Page> = articles_of(file.lines().map(str::to_string))
            .map(|(_, article)| Page::from_set(set, article.unwrap()).unwrap())
            .collect();
        assert_eq!(read.len(), articles.len(), "{set}");
        pages.extend(read);
    }
    pages
}

const QUERIES: &[&str] = &[
    "marie curie",
    "Marie Curie",
    "madame curie",
    "curie",
    "radium discovery marie curie",
    "python",
    "python programming language",
    "pythonidae",
    "python snake",
    "bernie sanders",
    "senator bernie sanders",
    "sanders",
    "tim cook",
    "apple ceo",
    "apple",
    "youtube music",
    "chase",
    "chase bank",
    "us bank",
    "u.s. bancorp",
    "new york post",
    "manubrium",
    "what is the manubrium",
    "breastbone",
    "triassic jurassic cretaceous",
    "triassic",
    "free",
    "free bird",
    "no way home",
    "spider-man no way home",
    "dark side of the moon",
    "queen",
    "queen album",
    "members of queen",
    "napoleon",
    "how did napoleon become emperor",
    "hamstring insertion point",
    "tariffs",
    "mars",
    "mars bar company",
    "france",
    "capital of france",
    "france population",
    "paris",
    "thatcher",
    "president kennedy",
    "kennedy",
    "titanic",
    "titanic movie",
    "titanic sinking",
    "mitosis vs meiosis",
    "is a tomato a fruit",
    "linked list",
    "c#",
    "how to remove a lien",
    "memantine abuse",
    "galletas",
    "recetas fáciles galletas",
    "gesunde ernährung bewegung",
    "linux kernel",
    "torvalds linux",
    "rust github",
    "sonnet",
    "undo last git commit",
    "how to undo the most recent commit in git",
    "delete a git branch locally and remotely",
    "sort list of strings ignoring whitespace python",
    "how to get rid of aphids",
    "which process is using a port",
    "dune",
    "dune frank herbert",
    "dune book",
    "c# in depth",
    "the daily",
    "the daily podcast",
    "hardcore history",
    "smells like teen spirit",
    "smells like teen spirit nirvana",
    "nevermind nirvana album",
    "teen spirit lyrics",
    "inception",
    "inception film",
    "breaking bad",
    "python tutorial",
    "tomllib",
    "python tomllib",
    "padstart",
    "javascript padstart",
    "react hooks",
    "asyncio",
    "asyncio.run",
    "python asyncio run",
    "tomllib site:docs.python.org",
    "dengue fever symptoms",
    "who dengue fact sheet",
    "define prioritize",
    "how to change a car battery",
    "harvard admissions",
    "us bank mortgage rates",
    "attention is all you need",
    "attention is all you need paper",
    "deep residual learning",
    "basic local alignment search tool",
    "vaswani attention 2017",
    "react",
    "react npm",
    "requests python",
    "serde crate",
    "gin golang",
    "linus tech tips",
    "ltt",
    "awol",
    "what does awol mean",
    "mesozoic era",
    "einstien",
    "marie cruie",
    "pythn",
    "napolean",
    "https://www.healthline.com/health/dengue-fever",
    "python.org",
    "elizabeth warren",
    "senator warren",
    "warren senate",
    "cafe society",
    "café society",
    "serde serialize",
    "serde::serialize",
    "deep learning",
    "deep learning lecun nature",
    "",
];

/// Domains whose articles' popularity the golden file holds.
const DOMAINS: &[&str] = &[
    "python.org",
    "senate.gov",
    "apple.com",
    "youtube.com",
    "chase.com",
    "usbank.com",
    "nypost.com",
    "queenonline.com",
    "mars.com",
    "nytimes.com",
    "kernel.org",
    "example.com",
];

/// A hit as the golden file writes it: its score to 4 decimal places and
/// what else it says, and its page when it is not the page stored.
fn hit_line(hit: &PageHit, stored: &[Page]) -> String {
    let mut line = format!(
        "{:.4} {} {} pop={:.4}",
        hit.score, hit.page.set, hit.page.url, hit.popularity
    );
    for (flag, on) in [
        ("named", hit.named),
        ("whole", hit.whole),
        ("meant", hit.meant),
    ] {
        if on {
            write!(line, " {flag}").unwrap();
        }
    }
    if let Some(learned) = &hit.learned {
        write!(line, " learned={}", json(learned)).unwrap();
    }
    if !stored.contains(&hit.page) {
        write!(line, " page={}", json(&hit.page)).unwrap();
    }
    line
}

fn json<T: serde::Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value).unwrap()
}

fn hits_text(out: &mut String, what: &str, hits: &[PageHit], stored: &[Page]) {
    if hits.is_empty() {
        return;
    }
    writeln!(out, "  {what}").unwrap();
    for hit in hits {
        writeln!(out, "    {}", hit_line(hit, stored)).unwrap();
    }
}

/// Sites whose results the searches below place pages among: untitled,
/// as a node's are before it reads their homepages, and one a query names.
fn sites_for(query: &str) -> Vec<crate::Hit> {
    let mut sites: Vec<crate::Hit> = ["usbank.com", "nypost.com", "python.org", "who.int"]
        .iter()
        .map(|domain| known_site(domain, false, 0.5))
        .collect();
    if let Some(named) = sites
        .iter_mut()
        .find(|site| query.starts_with(site.domain.split('.').next().unwrap()))
    {
        named.named = true;
    }
    sites
}

fn golden_text(dir: &Path, searcher: &PageSearcher) -> String {
    let mut out = String::new();
    writeln!(out, "# the index").unwrap();
    writeln!(
        out,
        "pages {} papers {} languages {}",
        searcher.num_pages(),
        json(searcher.paper_coverage()),
        json(&searcher.language_coverage())
    )
    .unwrap();
    writeln!(
        out,
        "stats {}",
        std::fs::read_to_string(dir.join("pages.json")).unwrap()
    )
    .unwrap();
    out.push_str(&searcher.dump().unwrap());
    let stored: Vec<Page> = golden_pages();
    writeln!(out, "# sites").unwrap();
    for domain in DOMAINS {
        writeln!(
            out,
            "{domain} popularity {:?}",
            searcher
                .site_popularity(domain)
                .unwrap()
                .map(|p| format!("{p:.4}"))
        )
        .unwrap();
    }
    let mut untitled = sites_for("");
    searcher.title_untitled(&mut untitled).unwrap();
    searcher.note_demand(&mut untitled).unwrap();
    for site in &untitled {
        writeln!(
            out,
            "{} title {:?} demand {:?}",
            site.domain,
            site.title,
            site.demand.map(|d| format!("{d:.4}"))
        )
        .unwrap();
    }
    for word in ["python", "curie", "git", "dengue", "zzz"] {
        writeln!(out, "knows {word} {}", searcher.knows_word(word).unwrap()).unwrap();
    }
    for query in QUERIES {
        writeln!(out, "# query {query:?}").unwrap();
        let mut found = searcher.search(query, 10).unwrap();
        hits_text(&mut out, "search", &found, &stored);
        hits_text(
            &mut out,
            "entities",
            &searcher.entities(query, 10).unwrap(),
            &stored,
        );
        let ops = Operators::parse(query);
        for docs in [false, true] {
            let hits = searcher
                .search_naming_docs(&ops.words, &ops, docs, 10)
                .unwrap();
            if hits != found {
                hits_text(&mut out, &format!("naming docs={docs}"), &hits, &stored);
            }
        }
        if let Ok(paper) = PaperQuery::parse(query) {
            hits_text(
                &mut out,
                "papers",
                &searcher.search_papers(&paper, 10).unwrap(),
                &stored,
            );
        }
        let sites = sites_for(query);
        let before = found.clone();
        searcher
            .add_other_number(query, &sites, &mut found, 10)
            .unwrap();
        if let Some(meant) = found
            .iter()
            .find(|hit| !hit.named)
            .map(|hit| hit.page.url.clone())
        {
            searcher
                .add_meant(&[meant_page(&meant, 1.0)], &mut found)
                .unwrap();
        }
        searcher
            .add_meant(
                &[meant_page("https://en.wikipedia.org/wiki/Hamstring", 1.0)],
                &mut found,
            )
            .unwrap();
        searcher
            .add_named_site_page(query, &sites, &mut found)
            .unwrap();
        if found != before {
            hits_text(&mut out, "added", &found, &stored);
        }
        writeln!(
            out,
            "  spelled right {}",
            searcher.spelled_right(query, &found, None)
        )
        .unwrap();
        if let Some(spelling) = searcher.suggest_spelling(query, None, &|_| false).unwrap() {
            writeln!(out, "  spelling {}", json(&spelling)).unwrap();
            let checked = searcher.check_spelling(query, spelling.clone()).unwrap();
            writeln!(out, "  checked {}", json(&checked)).unwrap();
            let mut placed = Vec::new();
            searcher
                .list_suggested_page(&spelling, &crate::SearchOptions::default(), &mut placed)
                .unwrap();
            for page in &placed {
                writeln!(out, "  suggested page {}", hit_line(&page.hit, &stored)).unwrap();
            }
        }
        let mut placed = Vec::new();
        searcher
            .list_typed_page(query, &crate::SearchOptions::default(), &mut placed)
            .unwrap();
        for page in &placed {
            writeln!(out, "  typed page {}", hit_line(&page.hit, &stored)).unwrap();
        }
        if let Some(word) = searcher.definition(query).unwrap() {
            writeln!(out, "  definition {}", json(&word)).unwrap();
        }
        if let Some(song) = searcher.known_song(query).unwrap() {
            writeln!(out, "  known song {}", json(&song)).unwrap();
        }
    }
    out
}

/// Checks `text` against the golden file at `path`, or writes it there
/// when `PLUMB_WRITE_GOLDEN` is set.
fn check_golden(path: &Path, text: &str) {
    if std::env::var_os("PLUMB_WRITE_GOLDEN").is_some() {
        std::fs::write(path, text).unwrap();
        return;
    }
    let golden = std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
    let (mut want, mut got) = (golden.lines(), text.lines());
    for n in 1.. {
        match (want.next(), got.next()) {
            (None, None) => break,
            (want, got) if want == got => continue,
            (want, got) => panic!(
                "{}:{n} differs\n  golden: {}\n  now:    {}",
                path.display(),
                want.unwrap_or("(the end)"),
                got.unwrap_or("(the end)")
            ),
        }
    }
}

#[test]
fn the_page_index_and_its_searches_give_what_the_golden_file_holds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pages");
    build_page_index(&path, golden_pages()).unwrap();
    let searcher = PageSearcher::open(&path).unwrap();
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pages/tests/golden.txt");
    check_golden(&golden, &golden_text(&path, &searcher));
}
