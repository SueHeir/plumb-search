//! Fetched pages as text, in the charset they are written in.

use std::borrow::Cow;

use encoding_rs::{Encoding, UTF_8};

/// How much of a page is searched for the charset it declares: browsers
/// look in the first 1024 bytes.
const PRESCAN_BYTES: usize = 1024;

/// `body` as text. A page that is UTF-8 is read as UTF-8, whatever it
/// says, so a server's wrong default (`charset=iso-8859-1`) does not
/// garble it. Any other page is read in the charset its byte order mark,
/// its `Content-Type` header (`content_type`) or the page itself declares
/// (`<meta charset>`, `<meta http-equiv>`, `<?xml encoding?>`), so a
/// Shift_JIS, EUC-KR, GBK or windows-1252 page reads as written, not as
/// `�`. Bytes that are not of the charset become `�`.
pub(crate) fn decode<'a>(body: &'a [u8], content_type: Option<&str>) -> Cow<'a, str> {
    if let Some((encoding, bom)) = Encoding::for_bom(body) {
        return encoding.decode_without_bom_handling(&body[bom..]).0;
    }
    let utf8 = match std::str::from_utf8(body) {
        Ok(_) => true,
        // A page cut at the size limit can end inside a character.
        Err(err) => err.error_len().is_none(),
    };
    let encoding = if utf8 {
        UTF_8
    } else {
        content_type
            .and_then(|kind| charset_after(kind.as_bytes(), b"charset"))
            .or_else(|| declared(&body[..body.len().min(PRESCAN_BYTES)]))
            .unwrap_or(UTF_8)
    };
    encoding.decode_without_bom_handling(body).0
}

/// The charset the start of a page declares in a `<meta>` tag or in its
/// XML declaration.
fn declared(head: &[u8]) -> Option<&'static Encoding> {
    let lower = head.to_ascii_lowercase();
    if lower.starts_with(b"<?xml") {
        let end = find(&lower, b"?>").unwrap_or(lower.len());
        if let Some(found) = charset_after(&lower[..end], b"encoding") {
            return Some(found);
        }
    }
    let mut rest = lower.as_slice();
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
            decode(page, None),
            "<meta charset=\"iso-8859-1\"><title>Café 東京</title>"
        );
        assert_eq!(
            decode(page, Some("text/html; charset=ISO-8859-1")),
            "<meta charset=\"iso-8859-1\"><title>Café 東京</title>"
        );
        // Cut inside the last character.
        let cut = "<title>東京".as_bytes();
        assert_eq!(decode(&cut[..cut.len() - 1], None), "<title>東\u{FFFD}");
        assert_eq!(
            decode(b"\xEF\xBB\xBF<title>Caf\xC3\xA9</title>", None),
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
        assert!(decode(&page, None).ends_with("<title>価格.com - 価格比較</title>"));

        let (title, _, _) = encoding_rs::EUC_KR.encode("<title>네이버 날씨</title>");
        let page = [
            b"<META HTTP-EQUIV='Content-Type' CONTENT='text/html; charset=euc-kr'>".as_slice(),
            &title,
        ]
        .concat();
        assert!(decode(&page, None).ends_with("<title>네이버 날씨</title>"));

        // The header comes first, and windows-1252 stands in for Latin-1.
        let page = b"<meta charset=\"gbk\"><title>Caf\xE9 M\xFCnchen</title>";
        assert_eq!(
            decode(page, Some("text/html; charset=\"ISO-8859-1\"")),
            "<meta charset=\"gbk\"><title>Café München</title>"
        );

        let (feed, _, _) = encoding_rs::GBK.encode("<rss><title>天气预报</title></rss>");
        let page = [
            b"<?xml version=\"1.0\" encoding=\"GB2312\"?>".as_slice(),
            &feed,
        ]
        .concat();
        assert!(decode(&page, None).ends_with("<rss><title>天气预报</title></rss>"));
    }

    #[test]
    fn undeclared_or_unusable_charsets_stay_lossy_utf8() {
        assert_eq!(
            decode(b"<title>Caf\xE9</title>", None),
            "<title>Caf\u{FFFD}</title>"
        );
        assert_eq!(
            decode(b"<meta charset=utf-16><title>Caf\xE9</title>", None),
            "<meta charset=utf-16><title>Caf\u{FFFD}</title>"
        );
        assert_eq!(
            decode(b"<meta charset=iso-2022-kr><title>Caf\xE9</title>", None),
            "<meta charset=iso-2022-kr><title>Caf\u{FFFD}</title>"
        );
        assert_eq!(
            decode(b"<meta charset=klingon><title>Caf\xE9</title>", None),
            "<meta charset=klingon><title>Caf\u{FFFD}</title>"
        );
        // Only the start of the page counts, as in browsers.
        let late = [
            vec![b' '; PRESCAN_BYTES],
            b"<meta charset=\"windows-1252\"><title>Caf\xE9</title>".to_vec(),
        ]
        .concat();
        assert!(decode(&late, None).ends_with("Caf\u{FFFD}</title>"));
    }
}
