//! The OpenSearch description.

use super::*;

/// Whether `body` links to the OpenSearch description from its `<head>`.
fn links_to_opensearch(body: &str) -> bool {
    let link = "<link rel=\"search\" type=\"application/opensearchdescription+xml\" \
                title=\"Plumb Search\" href=\"/opensearch.xml\">";
    match (body.find(link), body.find("</head>")) {
        (Some(link), Some(head_end)) => link < head_end,
        _ => false,
    }
}

#[tokio::test]
async fn every_page_links_to_the_opensearch_description() {
    for uri in ["/", "/search?q=us+bank"] {
        let (_, headers, body) = get(backend(bank_hits()), uri).await;
        assert!(links_to_opensearch(&body), "{uri}: {body}");
        // The link loads nothing into the page, so the policy stays as strict.
        assert_eq!(
            headers[header::CONTENT_SECURITY_POLICY],
            CONTENT_SECURITY_POLICY
        );
    }
    let failing = Arc::new(FakeBackend {
        fail: true,
        ..FakeBackend::default()
    });
    let (_, _, body) = get(failing, "/search?q=x").await;
    assert!(links_to_opensearch(&body), "{body}");
    let setting_up = node(node_status(Phase::SettingUp, Step::Downloading));
    let (_, _, body) = send(node_router(backend(Vec::new()), setting_up), "/").await;
    assert!(body.contains("<title>Setting up - Plumb Search</title>"));
    assert!(links_to_opensearch(&body), "{body}");
}

async fn opensearch_for(app: Router, headers: &[(&str, &str)]) -> (StatusCode, HeaderMap, String) {
    send_with_headers(app, "/opensearch.xml", headers).await
}

#[tokio::test]
async fn the_opensearch_description_describes_plumb() {
    let (code, headers, body) =
        opensearch_for(router(backend(Vec::new())), &[("host", "127.0.0.1:7586")]).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/opensearchdescription+xml"
    );
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert!(
        body.starts_with(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <OpenSearchDescription xmlns=\"http://a9.com/-/spec/opensearch/1.1/\">\n"
        ),
        "{body}"
    );
    for expected in [
        "\n<ShortName>Plumb Search</ShortName>\n",
        "\n<Description>Plumb Search</Description>\n",
        "\n<InputEncoding>UTF-8</InputEncoding>\n",
        // A PNG starts with these bytes, in base64.
        "\n<Image width=\"32\" height=\"32\" type=\"image/png\">\
         data:image/png;base64,iVBORw0KGgo",
        "\n<Url type=\"text/html\" method=\"get\" \
         template=\"http://127.0.0.1:7586/search?q={searchTerms}\"/>\n",
    ] {
        assert!(body.contains(expected), "no {expected:?} in {body}");
    }
    assert!(body.ends_with("</OpenSearchDescription>\n"), "{body}");
    let base64 = body.split("base64,").nth(1).unwrap();
    let base64 = &base64[..base64.find('<').unwrap()];
    assert!(base64
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b)));
}

#[tokio::test]
async fn the_opensearch_description_searches_the_host_it_was_asked_from() {
    let cases: [(&[(&str, &str)], &str); 6] = [
        (&[("host", "127.0.0.1:8080")], "http://127.0.0.1:8080"),
        (&[("host", "Plumb.LAN:8080")], "http://plumb.lan:8080"),
        (&[("host", "192.168.1.20")], "http://192.168.1.20"),
        (&[("host", "[::1]:7586")], "http://[::1]:7586"),
        (&[("host", "localhost:80")], "http://localhost"),
        (
            &[
                ("host", "search.example.org"),
                ("x-forwarded-proto", "https, http"),
            ],
            "https://search.example.org",
        ),
    ];
    for (headers, origin) in cases {
        let (code, _, body) = opensearch_for(router(backend(Vec::new())), headers).await;
        assert_eq!(code, StatusCode::OK, "{headers:?}");
        let template = format!("template=\"{origin}/search?q={{searchTerms}}\"");
        assert!(
            body.contains(&template),
            "{headers:?}: no {template} in {body}"
        );
    }

    // A node serves it too, even before its index is ready.
    let setting_up = node(node_status(Phase::SettingUp, Step::Downloading));
    let app = node_router(backend(Vec::new()), setting_up);
    let (code, _, body) = opensearch_for(app, &[("host", "127.0.0.1:7586")]).await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.contains("template=\"http://127.0.0.1:7586/search?q={searchTerms}\""));
}

#[tokio::test]
async fn the_opensearch_description_needs_a_plain_host() {
    for host in [
        "",
        "user@example.com",
        "example.com/path",
        "example.com?q=1",
        "example.com#top",
        "exa mple.com",
        "example.com:99999",
        "\"><script>alert(1)</script>",
    ] {
        let (code, _, body) = opensearch_for(router(backend(Vec::new())), &[("host", host)]).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{host:?}: {body}");
        assert!(!body.contains("<script"), "{body}");
    }
    let (code, _, _) = opensearch_for(router(backend(Vec::new())), &[]).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
}

#[test]
fn the_opensearch_description_escapes_the_host() {
    let body = render_opensearch("http://a&b'c\"d<e>.example");
    assert!(
        body.contains(
            "template=\"http://a&amp;b&#39;c&quot;d&lt;e&gt;.example/search?q={searchTerms}\""
        ),
        "{body}"
    );
}
