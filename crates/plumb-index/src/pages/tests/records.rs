//! Records a search names by their identifier, listed first.

use super::*;

/// The address of each record `query` names.
fn record_urls(query: &str) -> Vec<String> {
    super::super::records::records_named(query)
        .into_iter()
        .map(|hit| hit.page.url)
        .collect()
}

#[test]
fn identifiers_build_their_records_addresses() {
    for (query, url, title) in [
        (
            "RFC 3339",
            "https://www.rfc-editor.org/rfc/rfc3339.html",
            "RFC 3339",
        ),
        (
            "rfc3339 timestamp format",
            "https://www.rfc-editor.org/rfc/rfc3339.html",
            "RFC 3339",
        ),
        ("pep 8", "https://peps.python.org/pep-0008/", "PEP 8"),
        (
            "Python 3.12 type parameter syntax PEP 695 official docs",
            "https://peps.python.org/pep-0695/",
            "PEP 695",
        ),
        ("OEIS A001316", "https://oeis.org/A001316", "OEIS A001316"),
        (
            "oeis a108 catalan numbers",
            "https://oeis.org/A000108",
            "OEIS A000108",
        ),
        (
            "Wikidata property ISO 4217 code P498",
            "https://www.wikidata.org/wiki/Property:P498",
            "Wikidata P498",
        ),
        (
            "wikidata q42",
            "https://www.wikidata.org/wiki/Q42",
            "Wikidata Q42",
        ),
        (
            "arXiv:1706.03762v5",
            "https://arxiv.org/abs/1706.03762v5",
            "arXiv 1706.03762v5",
        ),
        (
            "PMID: 12345",
            "https://pubmed.ncbi.nlm.nih.gov/12345/",
            "PMID 12345",
        ),
        (
            "cve-2021-44228 log4j",
            "https://www.cve.org/CVERecord?id=CVE-2021-44228",
            "CVE-2021-44228",
        ),
        (
            "9789240034228 WHO air quality guidelines",
            "https://openlibrary.org/isbn/9789240034228",
            "ISBN 9789240034228",
        ),
        (
            "isbn 0-14-143951-3",
            "https://openlibrary.org/isbn/0141439513",
            "ISBN 0141439513",
        ),
        (
            "doi:10.1038/nature14539",
            "https://doi.org/10.1038/nature14539",
            "DOI 10.1038/nature14539",
        ),
    ] {
        let found = super::super::records::records_named(query);
        assert_eq!(found.len(), 1, "{query}");
        assert_eq!(found[0].page.url, url, "{query}");
        assert_eq!(found[0].page.title, title, "{query}");
        assert_eq!(found[0].page.set, RECORDS_SET);
        assert_eq!(
            found[0].page.set_name(),
            url.split('/').nth(2).unwrap().trim_start_matches("www.")
        );
    }
    // In the order the query names them.
    assert_eq!(
        record_urls("rfc 7231 pep 3333"),
        [
            "https://www.rfc-editor.org/rfc/rfc7231.html",
            "https://peps.python.org/pep-3333/"
        ]
    );
}

#[test]
fn what_only_looks_like_an_identifier_makes_no_record() {
    for query in [
        // Other words starting with the scheme's, and its site.
        "pepsi 2",
        "pep boys",
        "rfc editor",
        "wikidata.org",
        // A quantity.
        "pep 28 days",
        // Shapes too common to read without the scheme's word.
        "a001316",
        "q42 earnings",
        "1706.03762",
        // A wrong check digit, and an ISBN-10 the query does not call one.
        "9789240034229",
        "0141439513",
        // Too long, or not an identifier at all.
        "rfc 123456",
        "oeis a1234567",
        "wikidata p0",
        "cve-21-44228",
        "10.1/x",
        "arxiv attention is all you need",
    ] {
        assert_eq!(record_urls(query), Vec::<String>::new(), "{query}");
    }
}

#[test]
fn a_record_leads_the_results_whatever_else_is_found() {
    let (_dir, searcher) = searcher(&[page("Request for Comments", 90_000, &["RFC"])]);
    let sites = [site("rfc-editor.org", true), site("ietf.org", false)];
    let found = searcher.search("rfc 3339", 10).unwrap();
    assert_eq!(titles(&found)[0], "RFC 3339");
    let mut placed = place_pages("rfc 3339", &sites, found);
    let first = |placed: &[PlacedPage]| {
        let page = &placed[0];
        (page.at, page.under.clone(), page.hit.page.title.clone())
    };
    assert_eq!(first(&placed), (0, None, "RFC 3339".to_string()));
    // The learned order leaves it first, and so does placing again, as
    // the results page and MCP do.
    let mut hits = sites.to_vec();
    crate::learned::reorder(
        crate::learned::Model::builtin(),
        "rfc 3339",
        &mut hits,
        &mut placed,
    );
    assert_eq!(first(&placed), (0, None, "RFC 3339".to_string()));
    let again = place_pages(
        "rfc 3339",
        &hits,
        placed.into_iter().map(|p| p.hit).collect(),
    );
    assert_eq!(first(&again), (0, None, "RFC 3339".to_string()));
}
