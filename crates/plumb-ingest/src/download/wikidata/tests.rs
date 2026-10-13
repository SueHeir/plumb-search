//! Tests of the official websites downloads, against a stand-in for
//! Wikidata's SPARQL endpoint on a loopback port, and the helpers that make
//! its answers, which the tests of the bands and of reading answers share.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tokio::io::AsyncWriteExt;

use super::*;
use crate::download::tests::{gzip, loopback_client};

pub(super) fn band(min: u32, max: Option<u32>) -> SitelinkBand {
    SitelinkBand { min, max }
}

/// No waits, so that tests of retries run fast.
fn quick() -> WikidataPacing {
    WikidataPacing {
        pause: Duration::ZERO,
        retry_wait: Duration::from_millis(1),
    }
}

#[test]
fn the_bulk_query_needs_no_label_service() {
    let query = bulk_official_sites_query(3);
    assert!(query.starts_with(WIKIDATA_PREFIXES), "{query}");
    assert!(query.contains("FILTER(xsd:integer(?s) >= 3)"), "{query}");
    assert!(query.contains("COALESCE(?en, ?mul, STRAFTER(STR(?item), \"/entity/\"))"));
    assert!(!query.contains("SERVICE"));
}

/// `n` made-up official websites.
fn many_rows(n: usize) -> Vec<(String, String, String)> {
    (0..n)
        .map(|i| {
            (
                format!("Q{i}"),
                format!("Item {i}"),
                format!("https://item{i}.org/"),
            )
        })
        .collect()
}

fn sparql_ok(rows: &[(String, String, String)]) -> Vec<u8> {
    let answer = sparql_answer(
        rows.iter()
            .map(|(item, label, website)| (item.as_str(), label.as_str(), website.as_str())),
    );
    http_response("200 OK", &[], answer.as_bytes())
}

#[tokio::test]
async fn the_mirror_lists_every_site_in_one_query() {
    let rows = many_rows(MIN_BULK_ROWS + 5);
    let (url, asked) = sparql_endpoint(move |_, n| match n {
        0 => http_response("429 Too Many Requests", &[], b"slow down"),
        _ => sparql_ok(&rows),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let path = download_wikidata_official_sites_with(
        &loopback_client(),
        Some(&url),
        "http://127.0.0.1:9/never",
        dir.path(),
        3,
        quick(),
    )
    .await
    .unwrap();
    let text = std::fs::read_to_string(path).unwrap();
    assert_eq!(text.lines().count(), MIN_BULK_ROWS + 6);
    assert!(text.contains("Q7\tItem 7\thttps://item7.org/\n"));
    assert_eq!(asked.bands(), [band(3, None), band(3, None)]);
}

#[tokio::test]
async fn without_the_mirror_wikidata_is_asked_from_25_up() {
    let (mirror, _) = sparql_endpoint(|_, _| http_response("502 Bad Gateway", &[], b"")).await;
    let (main, asked) = sparql_endpoint(|_, _| sparql_ok(&many_rows(2))).await;
    let dir = tempfile::tempdir().unwrap();
    download_wikidata_official_sites_with(
        &loopback_client(),
        Some(&mirror),
        &main,
        dir.path(),
        3,
        quick(),
    )
    .await
    .unwrap();
    assert_eq!(
        asked.bands(),
        wikidata_sitelink_bands(ENDPOINT_MIN_SITELINKS)
    );
}

#[tokio::test]
async fn a_mirror_with_too_few_rows_falls_back_to_the_bands() {
    let (mirror, _) = sparql_endpoint(|_, _| sparql_ok(&many_rows(3))).await;
    let (main, asked) = sparql_endpoint(|_, _| sparql_ok(&many_rows(2))).await;
    let dir = tempfile::tempdir().unwrap();
    // From 130 up there is one band, so one query.
    let path = download_wikidata_official_sites_with(
        &loopback_client(),
        Some(&mirror),
        &main,
        dir.path(),
        130,
        quick(),
    )
    .await
    .unwrap();
    let text = std::fs::read_to_string(path).unwrap();
    assert_eq!(text.lines().count(), 3);
    assert_eq!(asked.bands(), [band(130, None)]);
}

/// What Wikidata's SPARQL endpoint answers for `rows` (item id, label,
/// website), pretty-printed as it does.
pub(super) fn sparql_answer<'a>(
    rows: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
) -> String {
    let bindings: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(item, label, website)| {
            serde_json::json!({
                "item": {"type": "uri", "value": format!("http://www.wikidata.org/entity/{item}")},
                "website": {"type": "uri", "value": website},
                "itemLabel": {"xml:lang": "en", "type": "literal", "value": label},
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "head": {"vars": ["item", "itemLabel", "website"]},
        "results": {"bindings": bindings},
    }))
    .unwrap()
}

/// `answer` as Wikidata sends it when it stops `query` at its time
/// limit: broken off inside an item's URL, then the query and the Java
/// exception of the query service. The quotes in the query close the
/// broken string, so that a JSON parser trips over what follows, as it
/// did on the real thing.
pub(super) fn stopped_at_time_limit(answer: &str, query: &str) -> String {
    let from = answer.len() * 3 / 5;
    let cut = answer[from..]
        .find("entity/Q")
        .map_or(from, |at| from + at + "entity/Q".len());
    format!(
        "{}SPARQL-QUERY: queryStr={query}\n\
         java.util.concurrent.TimeoutException\n\
         \tat java.util.concurrent.FutureTask.get(FutureTask.java:205)\n\
         \tat com.bigdata.rdf.sail.webapp.BigdataServlet.submitApiTask(BigdataServlet.java:292)\n\
         \tat com.bigdata.rdf.sail.webapp.QueryServlet.doSparqlQuery(QueryServlet.java:678)\n",
        &answer[..cut]
    )
}

fn http_response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut response = head.into_bytes();
    response.extend_from_slice(body);
    response
}

/// The band a query from [`SitelinkBand::sparql_query`] asks for.
fn band_of_query(query: &str) -> Option<SitelinkBand> {
    if let Some(rest) = query.split_once("?item wikibase:sitelinks ") {
        let n = rest.1.split_once(' ')?.0.parse().ok()?;
        return Some(band(n, Some(n)));
    }
    if let Some(rest) = query.split_once("FILTER(xsd:integer(?s) >= ") {
        return Some(band(rest.1.split_once(')')?.0.parse().ok()?, None));
    }
    let filter = query.split_once("FILTER(")?.1.split_once(')')?.0;
    let (mut min, mut max) = (None, None);
    for part in filter.split("&&").map(str::trim) {
        if let Some(n) = part.strip_prefix("?s >= ") {
            min = Some(n.parse().ok()?);
        } else {
            let n: u64 = part.strip_prefix("?s < ")?.parse().ok()?;
            max = Some(u32::try_from(n - 1).ok()?);
        }
    }
    Some(band(min?, max))
}

/// The bands a stand-in endpoint was asked for, in order, with the head
/// of each request, lowercased.
#[derive(Debug, Default, Clone)]
struct Asked(Arc<Mutex<Vec<(SitelinkBand, String)>>>);

impl Asked {
    fn bands(&self) -> Vec<SitelinkBand> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(band, _)| *band)
            .collect()
    }

    fn heads(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, head)| head.clone())
            .collect()
    }
}

/// A stand-in for Wikidata's SPARQL endpoint on a loopback port; no
/// outside network. It reads the band out of each form-encoded query and
/// answers `respond(band, n)`, where `n` counts the earlier queries for
/// that same band.
async fn sparql_endpoint<F>(respond: F) -> (String, Asked)
where
    F: Fn(SitelinkBand, usize) -> Vec<u8> + Send + 'static,
{
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/sparql", listener.local_addr().unwrap());
    let asked = Asked::default();
    let log = asked.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            let body_start = loop {
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(end + 4);
                }
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => break None,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            };
            let Some(body_start) = body_start else {
                continue;
            };
            let head = String::from_utf8_lossy(&request[..body_start]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|n| n.trim().parse().ok())
                .unwrap_or(0);
            while request.len() < body_start + length {
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let query = url::form_urlencoded::parse(&request[body_start..])
                .find(|(name, _)| name == "query")
                .map(|(_, query)| query.into_owned())
                .unwrap_or_default();
            let band = band_of_query(&query).expect("a query for a band of sitelinks");
            let n = {
                let mut log = log.0.lock().unwrap();
                let n = log.iter().filter(|(b, _)| *b == band).count();
                log.push((band, head));
                n
            };
            let _ = socket.write_all(&respond(band, n)).await;
            let _ = socket.shutdown().await;
        }
    });
    (url, asked)
}

/// Rows of a made-up Wikidata as (sitelinks, item, label, website):
/// 2000 / n items with n sitelinks for n from 20 to 340, one website
/// each; an item with two websites; and one listed with 26 and with 30
/// sitelinks, as when an item gains sitelinks between two queries.
fn made_up_wikidata() -> Vec<(u32, String, String, String)> {
    let mut rows = Vec::new();
    for n in 20..=340u32 {
        for i in 0..2000 / n {
            let id = n * 1000 + i;
            rows.push((
                n,
                format!("Q{id}"),
                format!("Item {id}"),
                format!("https://item{id}.org/"),
            ));
        }
    }
    for website in ["https://two.org/", "https://two.net/"] {
        rows.push((40, "Q1".into(), "Two websites".into(), website.into()));
    }
    for n in [26, 30] {
        rows.push((n, "Q2".into(), "Moved".into(), "https://moved.org/".into()));
    }
    rows
}

fn rows_in(rows: &[(u32, String, String, String)], band: SitelinkBand) -> Vec<(&str, &str, &str)> {
    rows.iter()
        .filter(|(n, ..)| band.min <= *n && band.max.is_none_or(|max| *n <= max))
        .map(|(_, item, label, website)| (item.as_str(), label.as_str(), website.as_str()))
        .collect()
}

/// The answer of the made-up Wikidata for `band`, gzipped as Wikidata
/// sends it, whole or, when `stopped`, stopped at the time limit.
fn made_up_answer(
    data: &[(u32, String, String, String)],
    band: SitelinkBand,
    stopped: bool,
) -> Vec<u8> {
    let answer = sparql_answer(rows_in(data, band));
    let body = match stopped {
        true => stopped_at_time_limit(&answer, &band.sparql_query()),
        false => answer,
    };
    http_response(
        "200 OK",
        &[("Content-Encoding", "gzip")],
        &gzip(body.as_bytes()),
    )
}

/// Checks that `path` holds every made-up row from `min` sitelinks up, once.
fn assert_has_every_row(path: &Path, data: &[(u32, String, String, String)], min: u32) {
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("item\tlabel\twebsite"));
    let got: Vec<&str> = lines.collect();
    let expected: HashSet<String> = rows_in(data, band(min, None))
        .into_iter()
        .map(|(item, label, website)| format!("{item}\t{label}\t{website}"))
        .collect();
    assert_eq!(got.len(), expected.len(), "a row came twice");
    let got: HashSet<String> = got.into_iter().map(str::to_string).collect();
    assert_eq!(got, expected);
    let sites = crate::load_wikidata_official_sites(path).unwrap();
    assert_eq!(sites.len(), expected.len());
    assert!(sites
        .iter()
        .any(|site| site.item == "Q2" && site.domain == "moved.org"));
}

/// The names in `dir`, sorted.
pub(super) fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn asks_again_for_bands_wikidata_stops_and_halves_them_last() {
    // Wikidata stops the first query of every band at its time limit,
    // and every query for 36 to 44 sitelinks.
    let data = Arc::new(made_up_wikidata());
    let served = Arc::clone(&data);
    let (url, asked) = sparql_endpoint(move |asked_for, n| {
        let stopped = n == 0 || asked_for == band(36, Some(44));
        made_up_answer(&served, asked_for, stopped)
    })
    .await;

    let dir = tempfile::tempdir().unwrap();
    let seed = dir.path().join("seed");
    let path = download_wikidata_official_sites_paced(&loopback_client(), &url, &seed, 25, quick())
        .await
        .unwrap();
    assert_eq!(path, seed.join(WIKIDATA_FILE_NAME));
    assert_has_every_row(&path, &data, 25);
    // The saved bands are gone with the `.part` file.
    assert_eq!(names_in(&seed), [WIKIDATA_FILE_NAME]);

    // Each band was asked for again after it was stopped; the one that
    // was stopped every time was split once its tries ran out.
    let mut expected = Vec::new();
    for b in wikidata_sitelink_bands(25) {
        if b == band(36, Some(44)) {
            expected.extend([b; BUSY_TRIES as usize]);
            expected.extend([band(36, Some(40)); 2]);
            expected.extend([band(41, Some(44)); 2]);
        } else {
            expected.extend([b; 2]);
        }
    }
    assert_eq!(asked.bands(), expected);
    for head in asked.heads() {
        assert!(head.starts_with("post /sparql "), "{head}");
        assert!(
            head.contains("\r\naccept: application/sparql-results+json\r\n"),
            "{head}"
        );
        assert!(head.contains("\r\naccept-encoding: gzip\r\n"), "{head}");
        assert!(
            head.contains("\r\ncontent-type: application/x-www-form-urlencoded\r\n"),
            "{head}"
        );
        assert!(head.contains("\r\nuser-agent: plumbsearch/"), "{head}");
    }
}

#[tokio::test]
async fn resumes_with_the_bands_an_earlier_try_saved() {
    // The first try: 30 to 35 sitelinks is always stopped, and once it
    // is split, the upper half is refused.
    let data = Arc::new(made_up_wikidata());
    let served = Arc::clone(&data);
    let (url, asked) = sparql_endpoint(move |asked_for, _| {
        if asked_for == band(33, Some(35)) {
            return http_response("403 Forbidden", &[], b"blocked");
        }
        made_up_answer(&served, asked_for, asked_for == band(30, Some(35)))
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let err =
        download_wikidata_official_sites_paced(&loopback_client(), &url, dir.path(), 25, quick())
            .await
            .unwrap_err();
    let err = format!("{err:#}");
    assert!(
        err.starts_with(
            "asking Wikidata for items with 33 to 35 sitelinks (try again later; the 2 bands \
             already in are saved): Wikidata query failed: HTTP 403 Forbidden: blocked"
        ),
        "{err}"
    );
    let mut expected = vec![band(25, Some(29))];
    expected.extend([band(30, Some(35)); BUSY_TRIES as usize]);
    expected.extend([band(30, Some(32)), band(33, Some(35))]);
    assert_eq!(asked.bands(), expected);
    let bands_dir = dir.path().join(WIKIDATA_BANDS_DIR_NAME);
    assert_eq!(names_in(dir.path()), [WIKIDATA_BANDS_DIR_NAME]);
    assert_eq!(names_in(&bands_dir), ["25-29.tsv", "30-32.tsv"]);

    // The next try asks only for what is missing.
    let served = Arc::clone(&data);
    let (url, asked) = sparql_endpoint(move |band, _| made_up_answer(&served, band, false)).await;
    let path =
        download_wikidata_official_sites_paced(&loopback_client(), &url, dir.path(), 25, quick())
            .await
            .unwrap();
    assert_eq!(
        asked.bands(),
        [
            band(33, Some(35)),
            band(36, Some(44)),
            band(45, Some(59)),
            band(60, Some(84)),
            band(85, Some(129)),
            band(130, None),
        ]
    );
    assert_has_every_row(&path, &data, 25);
    assert_eq!(names_in(dir.path()), [WIKIDATA_FILE_NAME]);

    // A try with nothing saved asks for every band.
    let (url, asked) = sparql_endpoint(move |band, _| made_up_answer(&data, band, false)).await;
    download_wikidata_official_sites_paced(&loopback_client(), &url, dir.path(), 25, quick())
        .await
        .unwrap();
    assert_eq!(asked.bands(), wikidata_sitelink_bands(25));
}

#[tokio::test]
async fn halving_stops_at_the_cap() {
    // Wikidata never answers whole. No waits, so the waits allowed in all
    // never run out.
    let no_waits = WikidataPacing {
        pause: Duration::ZERO,
        retry_wait: Duration::ZERO,
    };
    let (url, asked) = sparql_endpoint(|band, _| {
        let body = stopped_at_time_limit(&answer_for(band), &band.sparql_query());
        http_response("200 OK", &[], body.as_bytes())
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let err =
        download_wikidata_official_sites_paced(&loopback_client(), &url, dir.path(), 130, no_waits)
            .await
            .unwrap_err();
    let err = format!("{err:#}");
    assert!(
        err.starts_with(
            "Wikidata was too busy to answer the query for items with 130 to 194 sitelinks \
             (the answer breaks off after"
        ),
        "{err}"
    );
    assert!(
        err.ends_with("(tried 3 times)); it is already split 2 times; try again later"),
        "{err}"
    );
    let tries = BUSY_TRIES as usize;
    let mut expected = vec![band(130, None); tries];
    expected.extend(vec![band(130, Some(259)); tries]);
    expected.extend(vec![band(130, Some(194)); tries]);
    assert_eq!(asked.bands(), expected);
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());

    // A single count cannot be split at all.
    let (url, asked) =
        sparql_endpoint(|_, _| http_response("504 Gateway Timeout", &[], b"upstream timed out"))
            .await;
    let err =
        download_wikidata_official_sites_paced(&loopback_client(), &url, dir.path(), 24, no_waits)
            .await
            .unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "Wikidata was too busy to answer the query for items with exactly 24 sitelinks \
         (HTTP 504 Gateway Timeout: upstream timed out (tried 3 times)); it cannot be \
         split; try again later"
    );
    assert_eq!(asked.bands(), vec![band(24, Some(24)); tries]);
}

#[tokio::test]
async fn waits_stop_at_the_budget() {
    let (url, asked) =
        sparql_endpoint(|_, _| http_response("503 Service Unavailable", &[], b"busy")).await;
    let dir = tempfile::tempdir().unwrap();
    let pacing = quick();
    assert_eq!(pacing.budget(), Duration::from_millis(40));
    let err =
        download_wikidata_official_sites_paced(&loopback_client(), &url, dir.path(), 130, pacing)
            .await
            .unwrap_err();
    // 1 + 2 ms for the open band, 1 + 2 for its lower half and 1 + 2
    // for that one's lower half, then the halving stops.
    assert_eq!(
        format!("{err:#}"),
        "Wikidata was too busy to answer the query for items with 130 to 194 sitelinks \
         (HTTP 503 Service Unavailable: busy (tried 3 times)); it is already split 2 times; \
         try again later"
    );
    let mut expected = vec![band(130, None); 3];
    expected.extend([band(130, Some(259)); 3]);
    expected.extend([band(130, Some(194)); 3]);
    assert_eq!(asked.bands(), expected);

    // Out of waiting time, a busy band is handed back to be halved
    // rather than failing the download.
    let (url, asked) =
        sparql_endpoint(|_, _| http_response("503 Service Unavailable", &[], b"busy")).await;
    let mut budget = Duration::ZERO;
    let mut queries = 0;
    let failure = query_band(
        &loopback_client(),
        &url,
        band(130, None),
        quick(),
        &mut budget,
        &mut queries,
    )
    .await
    .unwrap_err();
    match failure {
        BandFailure::Busy(why) => assert!(why.contains("no waiting time left"), "{why}"),
        BandFailure::Fatal(err) => panic!("not busy: {err:#}"),
    }
    assert_eq!(asked.bands(), [band(130, None)]);
}

#[test]
fn waits_double_up_to_ten_times_the_first() {
    let pacing = WikidataPacing::default();
    let waits: Vec<u64> = (1..=7).map(|t| pacing.wait_after(t).as_secs()).collect();
    assert_eq!(waits, [30, 60, 120, 240, 300, 300, 300]);
    assert_eq!(pacing.budget(), Duration::from_secs(20 * 60));
    assert_eq!(pacing.rate_limit_wait(), Duration::from_secs(60));
    assert_eq!(pacing.wait_after(u32::MAX), Duration::from_secs(300));
}

/// The made-up rows of [`retries_rate_limits_and_server_errors`].
fn answer_for(band: SitelinkBand) -> String {
    let rows = [
        (150, "Q150", "One fifty", "https://one-fifty.org/"),
        (250, "Q250", "Two fifty", "https://two-fifty.org/"),
    ];
    sparql_answer(
        rows.into_iter()
            .filter(|(n, ..)| band.min <= *n && band.max.is_none_or(|max| *n <= max))
            .map(|(_, item, label, website)| (item, label, website)),
    )
}

#[tokio::test]
async fn retries_rate_limits_and_server_errors() {
    let (url, asked) = sparql_endpoint(|band, n| match n {
        // Without Retry-After: at least twice the first wait.
        0 => http_response("429 Too Many Requests", &[], b"slow down"),
        1 => http_response("504 Gateway Timeout", &[], b"upstream timed out"),
        // Stopped at the time limit before the answer started.
        2 => http_response(
            "500 Internal Server Error",
            &[],
            stopped_at_time_limit("", &band.sparql_query()).as_bytes(),
        ),
        3 => http_response("502 Bad Gateway", &[], b"bad gateway"),
        4 => http_response(
            "429 Too Many Requests",
            &[("Retry-After", "1")],
            b"slow down",
        ),
        _ => http_response("200 OK", &[], answer_for(band).as_bytes()),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let pacing = WikidataPacing {
        pause: Duration::ZERO,
        retry_wait: Duration::from_millis(50),
    };
    let path =
        download_wikidata_official_sites_paced(&loopback_client(), &url, dir.path(), 130, pacing)
            .await
            .unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(1800),
        "waited 100 (not 50), 100, 200 and 400 ms, then as long as Retry-After asked"
    );
    assert_eq!(asked.bands(), [band(130, None); 6]);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "item\tlabel\twebsite\n\
         Q150\tOne fifty\thttps://one-fifty.org/\n\
         Q250\tTwo fifty\thttps://two-fifty.org/\n"
    );
}

#[tokio::test]
async fn an_answer_the_connection_cuts_short_is_asked_for_again() {
    let (url, asked) = sparql_endpoint(|band, n| {
        let answer = answer_for(band);
        if n > 0 {
            return http_response("200 OK", &[], answer.as_bytes());
        }
        // Promises more than it sends, then hangs up.
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            answer.len() + 1000
        )
        .into_bytes();
        response.extend_from_slice(answer.as_bytes());
        response
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let path =
        download_wikidata_official_sites_paced(&loopback_client(), &url, dir.path(), 130, quick())
            .await
            .unwrap();
    assert_eq!(asked.bands(), [band(130, None); 2]);
    assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 3);
}

#[tokio::test]
async fn gives_up_on_client_errors_bad_answers_and_long_waits() {
    let cases = [
        (
            http_response("400 Bad Request", &[], b"bad query"),
            1,
            "Wikidata query failed: HTTP 400 Bad Request: bad query",
        ),
        (
            http_response("502 Bad Gateway", &[], b"down"),
            WIKIDATA_TRIES as usize,
            "HTTP 502 Bad Gateway: down (tried 6 times)",
        ),
        (
            http_response("429 Too Many Requests", &[("Retry-After", "3600")], b""),
            1,
            "the answer asks to wait 3600 seconds before the next query",
        ),
        (
            http_response("200 OK", &[], b"<html>Sign in to the Wi-Fi</html>"),
            1,
            "not SPARQL JSON results",
        ),
    ];
    for (response, requests, expected) in cases {
        let (url, asked) = sparql_endpoint(move |_, _| response.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let err = download_wikidata_official_sites_paced(
            &loopback_client(),
            &url,
            dir.path(),
            130,
            quick(),
        )
        .await
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.starts_with(
                "asking Wikidata for items with 130 or more sitelinks (try again later): "
            ),
            "{err}"
        );
        assert!(err.contains(expected), "{err}");
        assert_eq!(asked.bands().len(), requests, "{err}");
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }
}
