//! Tests of the bands of sitelink counts, their queries and file names,
//! and the bands saved between downloads.

use super::*;
use crate::download::wikidata::tests::{band, names_in};

#[test]
fn sparql_query_text() {
    assert_eq!(
        wikidata_sparql_query(20),
        "SELECT ?item ?itemLabel ?website WHERE { ?item wdt:P856 ?website ; wikibase:sitelinks ?s . FILTER(?s >= 20) SERVICE wikibase:label { bd:serviceParam wikibase:language \"en,mul\". } }"
    );
}

#[test]
fn sitelink_bands_cover_every_count_once() {
    assert_eq!(
        wikidata_sitelink_bands(25),
        [
            band(25, Some(29)),
            band(30, Some(35)),
            band(36, Some(44)),
            band(45, Some(59)),
            band(60, Some(84)),
            band(85, Some(129)),
            band(130, None),
        ]
    );
    assert_eq!(
        wikidata_sitelink_bands(45),
        [
            band(45, Some(59)),
            band(60, Some(84)),
            band(85, Some(129)),
            band(130, None)
        ]
    );
    assert_eq!(
        wikidata_sitelink_bands(100),
        [band(100, Some(129)), band(130, None)]
    );
    assert_eq!(wikidata_sitelink_bands(250), [band(250, None)]);
    // Below 25, every count is a band of its own.
    assert_eq!(
        wikidata_sitelink_bands(22)[..4],
        [
            band(22, Some(22)),
            band(23, Some(23)),
            band(24, Some(24)),
            band(25, Some(29))
        ]
    );
    assert_eq!(wikidata_sitelink_bands(0).len(), 25 + 7);
    for min in [0, 1, 10, 24, 25, 26, 27, 33, 99, 100, 101, 999, u32::MAX] {
        let bands = wikidata_sitelink_bands(min);
        assert_eq!(bands[0].min, min, "{min}");
        assert_eq!(bands.last().unwrap().max, None, "{min}");
        for pair in bands.windows(2) {
            let end = pair[0].max.expect("only the last band is open");
            assert!(pair[0].min <= end, "{min}: {pair:?}");
            assert_eq!(pair[1].min, end + 1, "{min}: {pair:?}");
        }
    }
}

#[test]
fn bands_split_in_halves_down_to_single_counts() {
    assert_eq!(
        band(25, Some(26)).halves(),
        Some((band(25, Some(25)), band(26, Some(26))))
    );
    assert_eq!(
        band(27, Some(29)).halves(),
        Some((band(27, Some(28)), band(29, Some(29))))
    );
    assert_eq!(
        band(70, Some(99)).halves(),
        Some((band(70, Some(84)), band(85, Some(99))))
    );
    assert_eq!(band(25, Some(25)).halves(), None);
    // A band without an upper end is cut at twice its start.
    assert_eq!(
        band(100, None).halves(),
        Some((band(100, Some(199)), band(200, None)))
    );
    assert_eq!(
        band(0, None).halves(),
        Some((band(0, Some(0)), band(1, None)))
    );
    assert_eq!(band(OPEN_BAND_SPLIT_LIMIT, None).halves(), None);
    assert_eq!(band(u32::MAX, None).halves(), None);
    assert_eq!(
        band(u32::MAX - 1, Some(u32::MAX)).halves(),
        Some((
            band(u32::MAX - 1, Some(u32::MAX - 1)),
            band(u32::MAX, Some(u32::MAX))
        ))
    );

    // Halving again and again covers the same counts, in order, and
    // ends in single counts (or an open band too high to split).
    for start in [
        band(25, Some(26)),
        band(70, Some(99)),
        band(100, None),
        band(0, None),
    ] {
        let mut todo = vec![start];
        let mut leaves = Vec::new();
        while let Some(b) = todo.pop() {
            match b.halves() {
                Some((lower, upper)) => {
                    todo.push(upper);
                    todo.push(lower);
                }
                None => leaves.push(b),
            }
        }
        assert_eq!(leaves[0].min, start.min, "{start:?}");
        assert_eq!(leaves.last().unwrap().max, start.max, "{start:?}");
        for pair in leaves.windows(2) {
            assert_eq!(pair[0].max, Some(pair[0].min), "{start:?}: {pair:?}");
            assert_eq!(pair[1].min, pair[0].min + 1, "{start:?}: {pair:?}");
        }
    }
    // Even the widest band reaches a single count within 32 halvings.
    let mut lowest = band(0, Some(u32::MAX));
    let mut halvings = 0;
    while let Some((lower, _)) = lowest.halves() {
        lowest = lower;
        halvings += 1;
    }
    assert_eq!((lowest, halvings), (band(0, Some(0)), 32));
}

#[test]
fn one_count_bands_start_from_the_count() {
    assert_eq!(
        band(20, Some(20)).sparql_query(),
        "SELECT ?item ?itemLabel ?website WHERE { ?item wikibase:sitelinks 20 . ?item wdt:P856 ?website . SERVICE wikibase:label { bd:serviceParam wikibase:language \"en,mul\". } }"
    );
}

#[test]
fn band_queries_and_names() {
    assert_eq!(
        band(25, Some(29)).sparql_query(),
        "SELECT ?item ?itemLabel ?website WHERE { ?item wdt:P856 ?website ; wikibase:sitelinks ?s . FILTER(?s >= 25 && ?s < 30) SERVICE wikibase:label { bd:serviceParam wikibase:language \"en,mul\". } }"
    );
    assert_eq!(
        band(130, None).sparql_query(),
        "SELECT ?item ?itemLabel ?website WHERE { ?item wdt:P856 ?website ; wikibase:sitelinks ?s . FILTER(?s >= 130) SERVICE wikibase:label { bd:serviceParam wikibase:language \"en,mul\". } }"
    );
    assert!(band(0, Some(u32::MAX))
        .sparql_query()
        .contains("FILTER(?s >= 0 && ?s < 4294967296)"));
    assert_eq!(band(100, None).sparql_query(), wikidata_sparql_query(100));
    let names: Vec<String> = [
        band(25, Some(25)),
        band(25, Some(26)),
        band(100, None),
        band(1, Some(1)),
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    assert_eq!(
        names,
        [
            "exactly 25 sitelinks",
            "25 to 26 sitelinks",
            "100 or more sitelinks",
            "exactly 1 sitelink"
        ]
    );
}

#[test]
fn saved_bands_are_found_and_the_missing_ones_asked_for() {
    let dir = tempfile::tempdir().unwrap();
    assert!(saved_bands(&dir.path().join("none"), 25)
        .unwrap()
        .is_empty());
    for name in [
        "20-24.tsv",
        "25-29.tsv",
        "28-31.tsv",
        "36-40.tsv",
        "130-up.tsv",
        "30-32.tsv.part",
        "41-40.tsv",
        "notes.txt",
    ] {
        std::fs::write(dir.path().join(name), "item\tlabel\twebsite\n").unwrap();
    }
    // Below the minimum, overlapping a lower band, or not a band: removed.
    assert_eq!(
        saved_bands(dir.path(), 25).unwrap(),
        [band(25, Some(29)), band(36, Some(40)), band(130, None)]
    );
    assert_eq!(
        names_in(dir.path()),
        ["130-up.tsv", "25-29.tsv", "36-40.tsv"]
    );

    let wanted = wikidata_sitelink_bands(25);
    assert_eq!(missing_bands(&wanted, &[]), wanted);
    assert_eq!(
        missing_bands(
            &wanted,
            &[band(25, Some(29)), band(36, Some(40)), band(130, None)]
        ),
        [
            band(30, Some(35)),
            band(41, Some(44)),
            band(45, Some(59)),
            band(60, Some(84)),
            band(85, Some(129)),
        ]
    );
    // Saved bands that do not line up with the wanted ones.
    assert_eq!(
        missing_bands(
            &wanted,
            &[
                band(27, Some(32)),
                band(38, Some(39)),
                band(41, Some(44)),
                band(200, None)
            ]
        ),
        [
            band(25, Some(26)),
            band(33, Some(35)),
            band(36, Some(37)),
            band(40, Some(40)),
            band(45, Some(59)),
            band(60, Some(84)),
            band(85, Some(129)),
            band(130, Some(199)),
        ]
    );
    assert!(missing_bands(&wanted, &[band(25, None)]).is_empty());
    assert_eq!(
        missing_bands(&[band(0, Some(u32::MAX))], &[band(5, Some(u32::MAX))]),
        [band(0, Some(4))]
    );

    for b in [band(25, Some(29)), band(7, Some(7)), band(130, None)] {
        assert_eq!(SitelinkBand::from_file_name(&b.file_name()), Some(b));
    }
}
