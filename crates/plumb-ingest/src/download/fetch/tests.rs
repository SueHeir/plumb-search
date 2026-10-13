//! Tests of downloading one file, against servers on a loopback port.

use tokio::io::AsyncWriteExt;

use super::*;
use crate::download::tests::loopback_client;

#[test]
fn part_files_sit_next_to_the_destination() {
    assert_eq!(
        part_path(Path::new("data/ranks.txt.gz")),
        PathBuf::from("data/ranks.txt.gz.part")
    );
}

#[test]
fn progress_every_five_percent_or_64_mb() {
    let mut known = Progress::new(Some(1000));
    let reports: Vec<u64> = (1..=100)
        .map(|i| i * 10)
        .filter(|&w| known.should_report(w))
        .collect();
    assert_eq!(reports, (1..=20).map(|i| i * 50).collect::<Vec<u64>>());

    // A big chunk that jumps several steps logs once.
    let mut jump = Progress::new(Some(1000));
    assert!(jump.should_report(730));
    assert!(!jump.should_report(749));
    assert!(jump.should_report(750));

    let mb = 1 << 20;
    let mut unknown = Progress::new(None);
    assert!(!unknown.should_report(63 * mb));
    assert!(unknown.should_report(64 * mb));
    assert!(!unknown.should_report(100 * mb));
    assert!(unknown.should_report(128 * mb));

    let mut tiny = Progress::new(Some(3));
    assert!(tiny.should_report(1));
    assert!(tiny.should_report(3));
}

/// Serves one canned HTTP response on a loopback port; no outside network.
async fn serve_once(response: Vec<u8>) -> String {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => request.extend_from_slice(&buf[..n]),
            }
        }
        let _ = socket.write_all(&response).await;
        let _ = socket.shutdown().await;
    });
    format!("http://{addr}/files/data.bin")
}

#[tokio::test]
async fn downloads_via_part_file() {
    let body = vec![b'x'; 100_000];
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);
    let url = serve_once(response).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("nested/data.bin");
    let written = download_to_file(&loopback_client(), &url, &dest)
        .await
        .unwrap();
    assert_eq!(written, body.len() as u64);
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert!(!part_path(&dest).exists());
}

#[tokio::test]
async fn downloads_without_content_length() {
    let url =
        serve_once(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nstreamed until close".to_vec())
            .await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("data.bin");
    let written = download_to_file(&loopback_client(), &url, &dest)
        .await
        .unwrap();
    assert_eq!(written, 20);
    assert_eq!(std::fs::read(&dest).unwrap(), b"streamed until close");
}

#[tokio::test]
async fn non_success_status_is_an_error() {
    let url = serve_once(
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found"
            .to_vec(),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("data.bin");
    let err = download_to_file(&loopback_client(), &url, &dest)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("404"), "{err:#}");
    assert!(!dest.exists());
    assert!(!part_path(&dest).exists());
}

#[tokio::test]
async fn truncated_body_is_an_error_and_leaves_no_files() {
    let url = serve_once(
        b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\nshort".to_vec(),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("data.bin");
    assert!(download_to_file(&loopback_client(), &url, &dest)
        .await
        .is_err());
    assert!(!dest.exists());
    assert!(!part_path(&dest).exists());
}

/// Serves `responses` on a loopback port, one per connection, and
/// keeps each request's head (lowercased) in the returned list.
async fn serve_each(
    responses: Vec<Vec<u8>>,
) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = std::sync::Arc::clone(&requests);
    tokio::spawn(async move {
        for response in responses {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let head = String::from_utf8_lossy(&request).to_lowercase();
            seen.lock().unwrap().push(head);
            let _ = socket.write_all(&response).await;
            let _ = socket.shutdown().await;
        }
    });
    (format!("http://{addr}/files/data.bin"), requests)
}

/// A response with `head` lines, then `body`.
fn response(head: &str, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!("{head}\r\nConnection: close\r\n\r\n").into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn numbered(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn a_download_that_breaks_off_carries_on() {
    let body = numbered(1000);
    let (url, requests) = serve_each(vec![
        // Says 1000 bytes, sends 400 and hangs up.
        response(
            "HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nAccept-Ranges: bytes\r\nETag: \"v1\"",
            &body[..400],
        ),
        response(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 600\r\n\
             Content-Range: bytes 400-999/1000",
            &body[400..],
        ),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("data.bin");
    let written = download_to_file(&loopback_client(), &url, &dest)
        .await
        .unwrap();
    assert_eq!(written, 1000);
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert!(!part_path(&dest).exists());
    assert!(!part_info_path(&dest).exists());
    let requests = requests.lock().unwrap();
    assert!(!requests[0].contains("range:"), "{}", requests[0]);
    assert!(
        requests[1].contains("\r\nrange: bytes=400-\r\n"),
        "{}",
        requests[1]
    );
    assert!(
        requests[1].contains("\r\nif-range: \"v1\"\r\n"),
        "{}",
        requests[1]
    );
}

#[tokio::test]
async fn a_part_file_is_carried_on_by_a_later_download() {
    let body = numbered(1000);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("data.bin");
    let resume = |url: &str| {
        std::fs::write(part_path(&dest), &body[..300]).unwrap();
        let info = PartInfo {
            url: url.to_string(),
            version: "Mon, 05 Oct 2026 00:00:00 GMT".into(),
            total: Some(1000),
        };
        std::fs::write(part_info_path(&dest), serde_json::to_vec(&info).unwrap()).unwrap();
    };
    let (url, requests) = serve_each(vec![response(
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 300-999/1000\r\n\
         Content-Length: 700",
        &body[300..],
    )])
    .await;
    resume(&url);
    download_to_file(&loopback_client(), &url, &dest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    let head = requests.lock().unwrap()[0].clone();
    assert!(head.contains("\r\nrange: bytes=300-\r\n"), "{head}");
    assert!(head.contains("if-range: mon, 05 oct 2026"), "{head}");

    // The file changed since: the server sends it whole, which replaces
    // the bytes kept.
    let changed = vec![b'n'; 500];
    let (url, _) = serve_each(vec![response(
        "HTTP/1.1 200 OK\r\nContent-Length: 500",
        &changed,
    )])
    .await;
    resume(&url);
    download_to_file(&loopback_client(), &url, &dest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), changed);
    assert!(!part_info_path(&dest).exists());

    // A part of some other address is not carried on.
    let (url, requests) = serve_each(vec![response(
        "HTTP/1.1 200 OK\r\nContent-Length: 500",
        &changed,
    )])
    .await;
    resume("http://example.com/other.bin");
    download_to_file(&loopback_client(), &url, &dest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), changed);
    assert!(!requests.lock().unwrap()[0].contains("range:"));
}
