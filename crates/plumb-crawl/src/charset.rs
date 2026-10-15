//! Fetched pages as text, in the charset they are written in.

use std::borrow::Cow;

use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use encoding_rs::{Encoding, UTF_8};

/// How much of a page is searched for the charset it declares. Browsers
/// look in the first 1024 bytes, then take a `<meta charset>` they meet
/// later in the `<head>` too: smbc.co.jp declares Shift_JIS at byte 1027,
/// after a list of updates in comments.
const HEAD_BYTES: usize = 64 * 1024;

/// The start of a page, where a declaration counts even past the head.
const PRESCAN_BYTES: usize = 1024;

/// `body` as text. A page that is UTF-8 is read as UTF-8, whatever it
/// says, so a server's wrong default (`charset=iso-8859-1`) does not
/// garble it. Any other page is read in the charset its byte order mark,
/// its `Content-Type` header (`content_type`) or its head declares
/// (`<meta charset>`, `<meta http-equiv>`, `<?xml encoding?>`), so a
/// Shift_JIS, EUC-KR, GBK or windows-1252 page reads as written, not as
/// `�`; a page that declares none, or says UTF-8 and is not, is read in
/// the charset it looks to be in on its `host` ([`guessed`]). Bytes that
/// are not of the charset become `�`.
pub(crate) fn decode<'a>(
    body: &'a [u8],
    content_type: Option<&str>,
    host: Option<&str>,
) -> Cow<'a, str> {
    if let Some((encoding, bom)) = Encoding::for_bom(body) {
        return encoding.decode_without_bom_handling(&body[bom..]).0;
    }
    let utf8 = match std::str::from_utf8(body) {
        // ISO-2022-JP is ASCII with escape sequences, so it passes for
        // UTF-8 (comiket.co.jp).
        Ok(_) => !(body.is_ascii() && body.contains(&0x1B)),
        // A page cut at the size limit can end inside a character.
        Err(err) => err.error_len().is_none(),
    };
    // A page that says UTF-8 and is not says nothing: budget.gov.hk is
    // Big5 and etomato.com EUC-KR under `<meta charset="utf-8">`.
    let not_utf8 = |encoding: &&Encoding| *encoding != UTF_8;
    let encoding = if utf8 {
        UTF_8
    } else {
        content_type
            .and_then(|kind| charset_after(kind.as_bytes(), b"charset"))
            .filter(not_utf8)
            .or_else(|| declared(&body[..body.len().min(HEAD_BYTES)]).filter(not_utf8))
            .unwrap_or_else(|| guessed(body, host))
    };
    encoding.decode_without_bom_handling(body).0
}

/// The charset the start of a page declares in its XML declaration, or in
/// a `<meta>` tag in its first 1024 bytes or anywhere in its `<head>`.
fn declared(head: &[u8]) -> Option<&'static Encoding> {
    let lower = head.to_ascii_lowercase();
    if lower.starts_with(b"<?xml") {
        let end = find(&lower, b"?>").unwrap_or(lower.len());
        if let Some(found) = charset_after(&lower[..end], b"encoding") {
            return Some(found);
        }
    }
    // The head ends at `</head>`, or at `<body>` when that is left out.
    let end = [find(&lower, b"</head"), find(&lower, b"<body")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(lower.len())
        .max(lower.len().min(PRESCAN_BYTES));
    let mut rest = &lower[..end];
    while let Some(at) = find(rest, b"<meta") {
        let tag = &rest[at..];
        let tag = &tag[..find(tag, b">").unwrap_or(tag.len())];
        if let Some(found) = charset_after(tag, b"charset") {
            return Some(found);
        }
        rest = &rest[at + 5..];
    }
    None
}

/// The charset of a page on `host` that declares none: UTF-8 when more of
/// its non-ASCII text is UTF-8 than not (a UTF-8 page with a stray byte or
/// two), else the one Firefox would guess from its bytes and top-level
/// domain.
fn guessed(body: &[u8], host: Option<&str>) -> &'static Encoding {
    let (mut utf8, mut not) = (0, 0);
    for chunk in body.utf8_chunks() {
        utf8 += chunk.valid().chars().filter(|c| !c.is_ascii()).count();
        not += usize::from(!chunk.invalid().is_empty());
    }
    if utf8 > not {
        return UTF_8;
    }
    let mut detector = EncodingDetector::new(Iso2022JpDetection::Allow);
    detector.feed(body, true);
    // The detector panics on a domain that is not lower-case ASCII.
    let tld = host
        .and_then(|host| host.trim_end_matches('.').rsplit('.').next())
        .filter(|tld| {
            tld.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        });
    detector.guess(tld.map(str::as_bytes), Utf8Detection::Deny)
}

/// The encoding named after `name` and `=` in `text`, quoted or not:
/// `text/html; charset=Shift_JIS`, `<meta charset="euc-kr">`. A page
/// that says UTF-16 in its text is ASCII-compatible after all, and the
/// "replacement" encoding would turn the whole page into one `�`, so
/// neither counts.
fn charset_after(text: &[u8], name: &[u8]) -> Option<&'static Encoding> {
    let lower = text.to_ascii_lowercase();
    let mut rest = lower.as_slice();
    while let Some(at) = find(rest, name) {
        rest = &rest[at + name.len()..];
        let value = rest.trim_ascii_start();
        let Some(value) = value.strip_prefix(b"=") else {
            continue;
        };
        let value = value.trim_ascii_start();
        let value = value
            .strip_prefix(b"\"")
            .or_else(|| value.strip_prefix(b"'"))
            .unwrap_or(value);
        let end = value
            .iter()
            .position(|b| matches!(b, b'"' | b'\'' | b';' | b'>' | b'/') || b.is_ascii_whitespace())
            .unwrap_or(value.len());
        let encoding = Encoding::for_label(&value[..end])?;
        let unusable = encoding == encoding_rs::UTF_16LE
            || encoding == encoding_rs::UTF_16BE
            || encoding == encoding_rs::REPLACEMENT;
        return (!unusable).then_some(encoding);
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_pages_stay_utf8_whatever_they_say() {
        let page = "<meta charset=\"iso-8859-1\"><title>Café 東京</title>".as_bytes();
        assert_eq!(
            decode(page, None, None),
            "<meta charset=\"iso-8859-1\"><title>Café 東京</title>"
        );
        assert_eq!(
            decode(page, Some("text/html; charset=ISO-8859-1"), None),
            "<meta charset=\"iso-8859-1\"><title>Café 東京</title>"
        );
        // Cut inside the last character.
        let cut = "<title>東京".as_bytes();
        assert_eq!(
            decode(&cut[..cut.len() - 1], None, None),
            "<title>東\u{FFFD}"
        );
        assert_eq!(
            decode(b"\xEF\xBB\xBF<title>Caf\xC3\xA9</title>", None, None),
            "<title>Café</title>"
        );
    }

    #[test]
    fn legacy_pages_read_in_the_charset_they_declare() {
        let (title, _, _) = encoding_rs::SHIFT_JIS.encode("<title>価格.com - 価格比較</title>");
        let page = [
            b"<html><head><meta charset=\"Shift_JIS\">".as_slice(),
            &title,
        ]
        .concat();
        assert!(decode(&page, None, None).ends_with("<title>価格.com - 価格比較</title>"));

        let (title, _, _) = encoding_rs::EUC_KR.encode("<title>네이버 날씨</title>");
        let page = [
            b"<META HTTP-EQUIV='Content-Type' CONTENT='text/html; charset=euc-kr'>".as_slice(),
            &title,
        ]
        .concat();
        assert!(decode(&page, None, None).ends_with("<title>네이버 날씨</title>"));

        // The header comes first, and windows-1252 stands in for Latin-1.
        let page = b"<meta charset=\"gbk\"><title>Caf\xE9 M\xFCnchen</title>";
        assert_eq!(
            decode(page, Some("text/html; charset=\"ISO-8859-1\""), None),
            "<meta charset=\"gbk\"><title>Café München</title>"
        );

        let (feed, _, _) = encoding_rs::GBK.encode("<rss><title>天气预报</title></rss>");
        let page = [
            b"<?xml version=\"1.0\" encoding=\"GB2312\"?>".as_slice(),
            &feed,
        ]
        .concat();
        assert!(decode(&page, None, None).ends_with("<rss><title>天气预报</title></rss>"));

        // Past the first 1024 bytes, but still in the head.
        let (title, _, _) = encoding_rs::SHIFT_JIS.encode("<title>三井住友銀行</title>");
        let page = [
            b"<!DOCTYPE html>".as_slice(),
            &b"<!-- Updated -->".repeat(70),
            b"<html><head><meta charset=\"Shift_JIS\">",
            &title,
        ]
        .concat();
        assert!(decode(&page, None, None).ends_with("<title>三井住友銀行</title>"));

        let (title, _, _) = encoding_rs::ISO_2022_JP.encode("<title>コミックマーケット</title>");
        assert_eq!(
            decode(&title, Some("text/html; charset=iso-2022-jp"), None),
            "<title>コミックマーケット</title>"
        );
    }

    #[test]
    fn undeclared_charsets_are_guessed() {
        assert_eq!(
            decode(b"<title>Caf\xE9 M\xFCnchen</title>", None, None),
            "<title>Café München</title>"
        );
        let (page, _, _) =
            encoding_rs::SHIFT_JIS.encode("<title>サービス一覧 | 三井住友銀行</title>");
        assert_eq!(
            decode(&page, None, None),
            "<title>サービス一覧 | 三井住友銀行</title>"
        );
        let (page, _, _) = encoding_rs::ISO_2022_JP.encode("<title>コミックマーケット</title>");
        assert_eq!(
            decode(&page, None, None),
            "<title>コミックマーケット</title>"
        );
        // Too little text to tell from, so the site's country decides.
        assert_eq!(
            decode(
                b"<title>\x93]\x91\x97</title>",
                None,
                Some("www.broadcasting.co.jp")
            ),
            "<title>転送</title>"
        );
        // A charset this cannot read counts as none.
        for meta in ["utf-16", "iso-2022-kr", "klingon"] {
            let page = format!("<meta charset={meta}><title>Caf\u{E9}</title>");
            let (page, _, _) = encoding_rs::WINDOWS_1252.encode(&page);
            assert!(
                decode(&page, None, None).ends_with("<title>Café</title>"),
                "{meta}"
            );
        }
        // A meta tag past the head declares nothing.
        let (text, _, _) = encoding_rs::SHIFT_JIS.encode("<p>サービス一覧 | 三井住友銀行</p>");
        let late = [
            b"<head></head><body>".as_slice(),
            &[b' '; PRESCAN_BYTES],
            b"<meta charset=\"windows-1252\">",
            &text,
        ]
        .concat();
        assert!(decode(&late, None, None).ends_with("<p>サービス一覧 | 三井住友銀行</p>"));
        // A UTF-8 page with a stray byte stays UTF-8, whatever it says.
        let stray = ["<title>Café 東京</title>".as_bytes(), b"\xE9"].concat();
        for content_type in [None, Some("text/html; charset=utf-8")] {
            assert_eq!(
                decode(&stray, content_type, None),
                "<title>Café 東京</title>\u{FFFD}"
            );
        }
        // A page that says UTF-8 and is not is guessed, or read in the
        // charset its head declares when only its header says UTF-8.
        let (title, _, _) = encoding_rs::EUC_KR.encode("<title>이토마토 | 증권 정보</title>");
        let page = [b"<meta charset=\"utf-8\" />".as_slice(), &title].concat();
        assert!(decode(&page, None, Some("www.etomato.com"))
            .ends_with("<title>이토마토 | 증권 정보</title>"));
        let (title, _, _) = encoding_rs::BIG5.encode("<title>財政預算案</title>");
        let page = [b"<meta charset=\"utf-8\" />".as_slice(), &title].concat();
        assert!(decode(
            &page,
            Some("text/html; charset=UTF-8"),
            Some("budget.gov.hk")
        )
        .ends_with("<title>財政預算案</title>"));
        let page = b"<meta charset=\"iso-8859-1\"><title>Caf\xE9</title>";
        assert!(
            decode(page, Some("text/html; charset=utf-8"), None).ends_with("<title>Café</title>")
        );
    }
}
