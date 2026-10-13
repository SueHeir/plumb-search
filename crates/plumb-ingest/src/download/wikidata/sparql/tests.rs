//! Tests of reading SPARQL answers into rows and TSV.

use super::*;
use crate::download::wikidata::tests::{sparql_answer, stopped_at_time_limit};
use crate::download::{wikidata_sparql_query, WIKIDATA_FILE_NAME};

#[test]
fn sparql_json_to_tsv() {
    let json = r#"{
      "head": {"vars": ["item", "itemLabel", "website"]},
      "results": {"bindings": [
        {"item": {"type": "uri", "value": "http://www.wikidata.org/entity/Q739868"},
         "website": {"type": "uri", "value": "https://www.usbank.com/"},
         "itemLabel": {"xml:lang": "en", "type": "literal", "value": "U.S. Bancorp"}},
        {"item": {"type": "uri", "value": "http://www.wikidata.org/entity/Q2"},
         "website": {"type": "uri", "value": "https://example.org/"},
         "itemLabel": {"type": "literal", "value": "Tabbed\tand\nsplit\r\nlabel"}},
        {"item": {"type": "uri", "value": "http://www.wikidata.org/entity/Q3"},
         "itemLabel": {"type": "literal", "value": "No website"}},
        {"item": {"type": "uri", "value": "http://www.wikidata.org/entity/Q4"},
         "website": {"type": "uri", "value": "https://nolabel.org/"}},
        {"website": {"type": "uri", "value": "https://noitem.org/"},
         "itemLabel": {"type": "literal", "value": "No item"}},
        {"item": {"type": "uri", "value": "http://www.wikidata.org/entity/Q6"},
         "website": {"type": "uri", "value": "https://blank.org/"},
         "itemLabel": {"type": "literal", "value": " \t "}},
        {"item": {"type": "uri", "value": "Q7"},
         "website": {"type": "uri", "value": "https://bare.org/"},
         "itemLabel": {"type": "literal", "value": "Bare id"}}
      ]}
    }"#;
    assert_eq!(
        wikidata_json_to_tsv(json.as_bytes()).unwrap(),
        "item\tlabel\twebsite\n\
         Q739868\tU.S. Bancorp\thttps://www.usbank.com/\n\
         Q2\tTabbed and split  label\thttps://example.org/\n\
         Q7\tBare id\thttps://bare.org/\n"
    );
    let empty = r#"{"head": {"vars": []}, "results": {"bindings": []}}"#;
    assert_eq!(
        wikidata_json_to_tsv(empty.as_bytes()).unwrap(),
        "item\tlabel\twebsite\n"
    );
    assert!(wikidata_json_to_tsv(b"<html>Query timeout</html>").is_err());
    assert!(wikidata_json_to_tsv(br#"{"head": {}}"#).is_err());
}

#[test]
fn tsv_round_trips_through_the_loader() {
    let json = r#"{"results": {"bindings": [
        {"item": {"value": "http://www.wikidata.org/entity/Q739868"},
         "itemLabel": {"value": "U.S. Bancorp"},
         "website": {"value": "https://www.usbank.com/"}}
    ]}}"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(WIKIDATA_FILE_NAME);
    std::fs::write(&path, wikidata_json_to_tsv(json.as_bytes()).unwrap()).unwrap();
    let sites = crate::load_wikidata_official_sites(&path).unwrap();
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].item, "Q739868");
    assert_eq!(sites[0].label, "U.S. Bancorp");
    assert_eq!(sites[0].domain, "usbank.com");
}

#[test]
fn answers_cut_short_are_told_apart_from_bad_ones() {
    let answer = sparql_answer([
        ("Q1", "One", "https://one.org/"),
        ("Q2", "Two", "https://two.org/"),
        ("Q3", "Three", "https://three.org/"),
    ]);
    assert_eq!(parse_sparql_rows(answer.as_bytes()).unwrap().len(), 3);
    let is_cut = |body: &str| matches!(parse_sparql_rows(body.as_bytes()), Err(BadAnswer::Cut(_)));
    let is_invalid = |body: &str| {
        matches!(
            parse_sparql_rows(body.as_bytes()),
            Err(BadAnswer::Invalid(_))
        )
    };

    // What Wikidata sends when it stops a query at its time limit.
    let stopped = stopped_at_time_limit(&answer, &wikidata_sparql_query(25));
    let err = serde_json::from_str::<SparqlResponse>(&stopped).unwrap_err();
    assert_eq!(err.classify(), Category::Syntax, "{err}");
    assert!(is_cut(&stopped));
    // JSON that breaks off anywhere, with nothing after it or something else.
    for end in [1, answer.len() / 3, answer.len() / 2, answer.len() - 1] {
        let cut = &answer[..end];
        assert!(is_cut(cut), "{cut}");
        assert!(
            is_cut(&format!("{cut}\n<html>502 Bad Gateway</html>")),
            "{cut}"
        );
    }
    assert!(is_cut(""));
    assert!(is_cut(" \n"));
    // Whole, but followed by the exception.
    assert!(is_cut(&format!(
        "{answer}\njava.util.concurrent.TimeoutException"
    )));
    assert!(is_cut(&format!(
        "{answer}\ncom.bigdata.bop.engine.QueryTimeoutException: Query deadline is expired."
    )));
    // A whole answer that only mentions the exception is fine.
    let mentions = sparql_answer([(
        "Q4",
        "java.util.concurrent.TimeoutException",
        "https://java.org/",
    )]);
    assert_eq!(parse_sparql_rows(mentions.as_bytes()).unwrap().len(), 1);
    // Not SPARQL JSON results at all.
    for body in [
        "<html>Query timeout</html>",
        "{\"head\": {}}",
        "[1, 2]",
        "null",
    ] {
        assert!(is_invalid(body), "{body}");
    }

    // Reported as Wikidata's time limit, not as a JSON error.
    let err = format!(
        "{:#}",
        wikidata_json_to_tsv(stopped.as_bytes()).unwrap_err()
    );
    assert!(
        err.starts_with(
            "Wikidata stopped the query at its time limit: the answer breaks off after"
        ),
        "{err}"
    );
    assert!(err.contains("timeout error"), "{err}");
    assert!(!err.contains("expected"), "{err}");
    let err = format!(
        "{:#}",
        wikidata_json_to_tsv(b"<html>Query timeout</html>").unwrap_err()
    );
    assert!(
        err.contains("not SPARQL JSON results: \"<html>Query timeout</html>\""),
        "{err}"
    );
}

fn row(item: &str, label: &str, website: &str) -> WikidataRow {
    WikidataRow {
        item: item.to_string(),
        label: label.to_string(),
        website: website.to_string(),
    }
}

#[test]
fn rows_of_every_band_are_merged_once_each() {
    let mut tsv = OfficialSitesTsv::new();
    let first = vec![
        row("Q1", "One", "https://one.org/"),
        row("Q2", "Two", "https://two.org/"),
        row("Q2", "Two", "https://two.com/"),
    ];
    assert_eq!(tsv.add(first), 3);
    // An item whose sitelinks changed between two queries comes again,
    // maybe under a new label: the first row is kept.
    let second = vec![
        row("Q2", "Two", "https://two.org/"),
        row("Q1", "Renamed", "https://one.org/"),
        row("Q3", "Three", "https://three.org/"),
    ];
    assert_eq!(tsv.add(second), 1);
    assert_eq!(tsv.add(Vec::new()), 0);
    assert_eq!(tsv.rows(), 4);
    assert_eq!(
        tsv.text,
        "item\tlabel\twebsite\n\
         Q1\tOne\thttps://one.org/\n\
         Q2\tTwo\thttps://two.org/\n\
         Q2\tTwo\thttps://two.com/\n\
         Q3\tThree\thttps://three.org/\n"
    );
}

#[test]
fn retry_after_headers() {
    let after = |value: &str| {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, value.parse().unwrap());
        retry_after(&headers)
    };
    assert_eq!(after("120"), Some(Duration::from_secs(120)));
    assert_eq!(after("0"), Some(Duration::ZERO));
    let soon = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(90));
    let wait = after(&soon).unwrap();
    assert!(
        (Duration::from_secs(85)..=Duration::from_secs(90)).contains(&wait),
        "{wait:?}"
    );
    let past = httpdate::fmt_http_date(SystemTime::now() - Duration::from_secs(90));
    assert_eq!(after(&past), Some(Duration::ZERO));
    assert_eq!(after("soon"), None);
    assert_eq!(after("-5"), None);
    assert_eq!(retry_after(&HeaderMap::new()), None);
}
