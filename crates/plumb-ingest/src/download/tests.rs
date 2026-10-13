//! Tests of the HTTP client, and helpers that the tests of the download
//! modules share.

use std::io::Write;

use super::*;

#[test]
fn client_builds() {
    http_client().unwrap();
}

pub(super) fn loopback_client() -> reqwest::Client {
    client_builder().no_proxy().build().unwrap()
}

pub(super) fn gzip(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}
