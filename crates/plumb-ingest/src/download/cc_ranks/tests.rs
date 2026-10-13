//! Tests of the ranks downloads and of keeping the top of a ranks file.

use std::time::Duration;

use tokio::io::AsyncWriteExt;

use super::*;
use crate::download::tests::{gzip, loopback_client};

#[test]
fn file_names_from_urls() {
    assert_eq!(
        file_name_from_url(&cc_domain_ranks_url("cc-main-2025-26-nov-dec-jan")).unwrap(),
        "cc-main-2025-26-nov-dec-jan-domain-ranks.txt.gz"
    );
    assert_eq!(
        file_name_from_url("https://example.org/a/b.txt?x=1#frag").unwrap(),
        "b.txt"
    );
    assert!(file_name_from_url("https://example.org/dir/").is_err());
    assert!(file_name_from_url("https://example.org").is_err());
    assert!(file_name_from_url("not a url").is_err());
}

#[test]
fn top_file_names() {
    assert_eq!(
        cc_domain_ranks_top_file_name(
            &cc_domain_ranks_url("cc-main-2025-26-nov-dec-jan"),
            1_000_000
        )
        .unwrap(),
        "cc-main-2025-26-nov-dec-jan-domain-ranks-top1000000.txt"
    );
    assert_eq!(
        cc_domain_ranks_top_file_name("https://example.org/r.txt", 5).unwrap(),
        "r-top5.txt"
    );
    assert_eq!(
        cc_domain_ranks_top_file_name("https://example.org/ranks", 5).unwrap(),
        "ranks-top5.txt"
    );
    assert!(cc_domain_ranks_top_file_name("https://example.org/dir/", 5).is_err());
}

const RANKS_HEADER: &str =
    "#harmonicc_pos\t#harmonicc_val\t#pr_pos\t#pr_val\t#host_rev\t#n_hosts\n";

/// A ranks file with `rows` rows, some ending in CRLF, with blank lines
/// before the header and among the rows.
fn ranks_text(rows: usize) -> String {
    let mut text = format!("\n{RANKS_HEADER}");
    for i in 1..=rows {
        let end = if i % 3 == 0 { "\r\n" } else { "\n" };
        text.push_str(&format!("{i}\t1.0E7\t{i}\t0.001\tcom.site{i}\t{i}{end}"));
        if i % 50 == 0 {
            text.push_str(" \t \n");
        }
    }
    text
}

/// What [`TopLines`] should keep of `text`: everything up to the end of
/// the line holding the `wanted`th row after the header.
fn expected_top(text: &str, wanted: usize) -> String {
    let mut kept = String::new();
    let mut header_seen = false;
    let mut rows = 0;
    for line in text.split_inclusive('\n') {
        kept.push_str(line);
        if line.trim().is_empty() {
            continue;
        }
        if header_seen {
            rows += 1;
        }
        header_seen = true;
        if rows >= wanted {
            break;
        }
    }
    kept
}

/// `data` gzipped as `members` members, split at arbitrary byte offsets
/// (mid-line, too), the way Common Crawl writes some of its files.
fn gzip_members(data: &[u8], members: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..members {
        let (from, to) = (data.len() * i / members, data.len() * (i + 1) / members);
        out.extend(gzip(&data[from..to]));
    }
    out
}

/// Feeds `body` to a [`TopLines`] in pieces of `chunk` bytes, as a
/// download would: stops once it is done, finishes at the end of the body.
fn run_top_lines(body: &[u8], chunk: usize, wanted: usize) -> io::Result<(String, TopLines)> {
    let mut top = TopLines::new(wanted);
    let mut kept = Vec::new();
    let mut pieces = body.chunks(chunk);
    while !top.is_done() {
        match pieces.next() {
            Some(piece) => top.write(piece)?,
            None => {
                top.finish()?;
                break;
            }
        }
        kept.extend(top.take_output());
    }
    kept.extend(top.take_output());
    Ok((String::from_utf8(kept).unwrap(), top))
}

#[test]
fn top_lines_keeps_the_header_and_the_first_rows() {
    let text = ranks_text(300);
    let bodies = [
        ("gzip", gzip(text.as_bytes())),
        ("3 gzip members", gzip_members(text.as_bytes(), 3)),
        ("plain", text.clone().into_bytes()),
    ];
    for (kind, body) in &bodies {
        for chunk in [1, 2, 3, 7, 64, 997, usize::MAX] {
            for wanted in [0, 1, 7, 50, 299, 300, 301, 5_000] {
                let what = format!("{kind}, {chunk}-byte chunks, {wanted} rows");
                let (kept, top) = run_top_lines(body, chunk, wanted).expect(&what);
                assert_eq!(kept, expected_top(&text, wanted), "{what}");
                assert_eq!(top.rows(), wanted.min(300), "{what}");
                assert_eq!(top.is_done(), wanted <= 300, "{what}");
                assert!(top.has_header(), "{what}");
            }
        }
    }
    // Nothing past the last row wanted is kept, not even the next line break.
    let (kept, _) = run_top_lines(&gzip(text.as_bytes()), 64, 2).unwrap();
    assert_eq!(
        kept,
        format!(
            "\n{RANKS_HEADER}1\t1.0E7\t1\t0.001\tcom.site1\t1\n2\t1.0E7\t2\t0.001\tcom.site2\t2\n"
        )
    );
}

#[test]
fn top_lines_reads_what_load_cc_domain_ranks_reads() {
    let text = ranks_text(120);
    let dir = tempfile::tempdir().unwrap();
    let whole = dir.path().join("whole.txt");
    std::fs::write(&whole, &text).unwrap();
    let (kept, _) = run_top_lines(&gzip_members(text.as_bytes(), 4), 100, 75).unwrap();
    let top = dir.path().join("top.txt");
    std::fs::write(&top, kept).unwrap();
    let expected = crate::load_cc_domain_ranks(&whole, Some(75)).unwrap();
    assert_eq!(expected.len(), 75);
    assert_eq!(crate::load_cc_domain_ranks(&top, None).unwrap(), expected);
}

#[test]
fn top_lines_rejects_a_gzip_body_cut_short() {
    let text = ranks_text(300);
    let body = gzip_members(text.as_bytes(), 2);
    let cut = &body[..body.len() - 20];
    for chunk in [1, 64, usize::MAX] {
        assert!(run_top_lines(cut, chunk, 1_000).is_err(), "{chunk}");
        // A cut that leaves enough rows is fine: the rest is never read.
        let (kept, top) = run_top_lines(cut, chunk, 100).unwrap();
        assert!(top.is_done());
        assert_eq!(kept, expected_top(&text, 100));
    }
    assert!(run_top_lines(b"\x1f\x8b not really gzip", 4, 10).is_err());
}

#[test]
fn top_lines_short_bodies() {
    let (kept, top) = run_top_lines(b"", 1, 10).unwrap();
    assert_eq!(
        (kept.as_str(), top.has_header(), top.is_done()),
        ("", false, false)
    );
    let (kept, top) = run_top_lines(b"\n \n", 1, 10).unwrap();
    assert_eq!((kept.as_str(), top.has_header()), ("\n \n", false));
    let (kept, top) = run_top_lines(b"h", 1, 0).unwrap();
    assert_eq!(
        (kept.as_str(), top.has_header(), top.is_done()),
        ("h", true, true)
    );
    // A last row without a line break counts.
    let (kept, top) = run_top_lines(b"h\nr1\nr2", 1, 2).unwrap();
    assert_eq!(
        (kept.as_str(), top.rows(), top.is_done()),
        ("h\nr1\nr2", 2, true)
    );
}

/// Serves one response on a loopback port, at a URL whose file name is
/// `name`: `head`, then the pieces of `body` until they run out or the
/// client hangs up. The task returns how many pieces were sent.
async fn serve_pieces<I>(
    name: &str,
    head: String,
    body: I,
) -> (String, tokio::task::JoinHandle<usize>)
where
    I: IntoIterator<Item = Vec<u8>> + Send + 'static,
    I::IntoIter: Send,
{
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return 0;
        };
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => return 0,
                Ok(n) => request.extend_from_slice(&buf[..n]),
            }
        }
        if socket.write_all(head.as_bytes()).await.is_err() {
            return 0;
        }
        let mut sent = 0;
        for piece in body {
            if socket.write_all(&piece).await.is_err() {
                return sent;
            }
            sent += 1;
        }
        let _ = socket.shutdown().await;
        sent
    });
    (format!("http://{addr}/graph/{name}"), task)
}

#[tokio::test]
async fn downloads_only_the_top_of_a_ranks_file() {
    // An endless ranks file: the header, then the same 1,000 rows over
    // and over, one gzip member at a time, with no length given.
    let rows: String = ranks_text(1_000)
        .lines()
        .skip(2)
        .map(|l| format!("{l}\n"))
        .collect();
    let member = gzip(rows.as_bytes());
    const MEMBERS: usize = 100_000;
    let body =
        std::iter::once(gzip(RANKS_HEADER.as_bytes())).chain(std::iter::repeat_n(member, MEMBERS));
    let head = "HTTP/1.1 200 OK\r\nContent-Type: application/gzip\r\nConnection: close\r\n\r\n";
    let (url, server) = serve_pieces("x-domain-ranks.txt.gz", head.to_string(), body).await;

    let dir = tempfile::tempdir().unwrap();
    let path = tokio::time::timeout(
        Duration::from_secs(60),
        download_cc_domain_ranks_top(&loopback_client(), &url, dir.path(), 2_500),
    )
    .await
    .expect("the download stops by itself")
    .unwrap();
    assert_eq!(path, dir.path().join("x-domain-ranks-top2500.txt"));
    assert!(!part_path(&path).exists());

    let saved = std::fs::read_to_string(&path).unwrap();
    let mut expected = RANKS_HEADER.to_string();
    expected.push_str(&rows);
    expected.push_str(&rows);
    expected.push_str(&expected_top(&format!("h\n{rows}"), 500)[2..]);
    assert_eq!(saved, expected);
    let ranks = crate::load_cc_domain_ranks(&path, None).unwrap();
    assert_eq!(ranks.len(), 2_500);
    assert_eq!(ranks[0].domain, "site1.com");

    // The client hung up long before the server ran out of members.
    let sent = tokio::time::timeout(Duration::from_secs(30), server)
        .await
        .expect("the server notices the client left")
        .unwrap();
    assert!(sent < MEMBERS, "sent all {sent} members");
}

#[tokio::test]
async fn saves_a_short_ranks_file_whole() {
    let text = ranks_text(40);
    let body = gzip_members(text.as_bytes(), 2);
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let (url, _) = serve_pieces("r.txt.gz", head, [body]).await;
    let dir = tempfile::tempdir().unwrap();
    let path =
        download_cc_domain_ranks_top(&loopback_client(), &url, &dir.path().join("seed"), 1_000)
            .await
            .unwrap();
    assert_eq!(path, dir.path().join("seed/r-top1000.txt"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
}

#[tokio::test]
async fn cut_or_failed_ranks_downloads_leave_no_files() {
    let text = ranks_text(40);
    let body = gzip(text.as_bytes());
    // Complete as HTTP goes, but the gzip stream stops short.
    let cut = body[..body.len() / 2].to_vec();
    let responses = [
        (
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                cut.len()
            ),
            cut,
        ),
        // The connection drops before the promised length.
        (
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len() + 100
            ),
            body.clone(),
        ),
        (
            "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\n".to_string(),
            b"not found".to_vec(),
        ),
        (
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
            Vec::new(),
        ),
    ];
    for (head, body) in responses {
        let (url, _) = serve_pieces("r.txt.gz", head.clone(), [body]).await;
        let dir = tempfile::tempdir().unwrap();
        let err = download_cc_domain_ranks_top(&loopback_client(), &url, dir.path(), 1_000)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains(&url), "{err:#}");
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(left.is_empty(), "{head}: {left:?}");
    }
}
