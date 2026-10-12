//! One canonical form for URLs (`url/1`), and the match key that decides
//! when two URLs are the same page.
//!
//! The **canonical URL** is what items store and what `url:` refs hold. It
//! is the URL as a browser parses it (WHATWG), http or https only, with no
//! user info, a lower-case IDNA host, no default port, no fragment, dot
//! segments resolved, no empty `?`, and percent-encoding made regular:
//! characters that never needed encoding are decoded and the rest use
//! upper-case hex. Two spellings of one address give one canonical URL.
//!
//! The **match key** is looser, for deciding that two canonical URLs are
//! one result, and for denylist entries: it leaves out the scheme (http and
//! https match), a leading `www.`, a trailing `/` on a path, and the
//! tracking parameters in [`TRACKING_PARAMS`], and it sorts the parameters
//! left. A denylist entry is the SHA-256 of a match key
//! ([`url_sha256`]), or of the match key without its query
//! ([`url_sha256_any_query`]) for an entry that holds whatever is added
//! after the `?`.
//!
//! Both are frozen as `url/1`. A different rule would be `url/2`, named by
//! the entries hashed with it; it never changes what `url/1` gives.

use sha2::{Digest, Sha256};
use url::Url;

/// The canonicalizer's name, carried by denylist entries hashed with it.
pub const URL_VERSION: &str = "url/1";

/// Query parameters the match key leaves out: they say who sent a visitor,
/// not which page it is. A name ending in `*` stands for every name that
/// starts with the rest. Fixed with `url/1`.
pub const TRACKING_PARAMS: &[&str] = &[
    "utm_*", "fbclid", "gclid", "dclid", "gbraid", "wbraid", "msclkid", "mc_cid", "mc_eid",
    "yclid", "igshid", "ref_src",
];

/// The canonical form of `input`, or `None` when it is not an http or https
/// URL with a host, or carries user info.
///
/// ```
/// use plumb_format::urls::canonical_url;
/// assert_eq!(
///     canonical_url("HTTPS://Docs.Python.org:443/3/./library/../%7euser?#top").as_deref(),
///     Some("https://docs.python.org/3/~user"),
/// );
/// ```
pub fn canonical_url(input: &str) -> Option<String> {
    canonical(input).map(String::from)
}

/// The canonical form of `input`, parsed.
fn canonical(input: &str) -> Option<Url> {
    let mut url = Url::parse(input.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    url.set_fragment(None);
    if url.path().contains('%') {
        let path = regular_percent_encoding(url.path());
        url.set_path(&path);
    }
    match url.query() {
        Some("") => url.set_query(None),
        Some(query) if query.contains('%') => {
            let query = regular_percent_encoding(query);
            url.set_query(Some(&query));
        }
        _ => {}
    }
    Some(url)
}

/// True when `url` is already in canonical form.
pub fn is_canonical(url: &str) -> bool {
    canonical_url(url).as_deref() == Some(url)
}

/// The match key of `input`, or `None` when it has no canonical form.
///
/// ```
/// use plumb_format::urls::match_key;
/// assert_eq!(
///     match_key("http://www.example.com/shop/?utm_source=x&b=2&a=1").as_deref(),
///     Some("example.com/shop?a=1&b=2"),
/// );
/// ```
pub fn match_key(input: &str) -> Option<String> {
    let url = canonical(input)?;
    let host = url.host_str()?;
    let mut key = host.strip_prefix("www.").unwrap_or(host).to_string();
    if let Some(port) = url.port() {
        key.push(':');
        key.push_str(&port.to_string());
    }
    let path = url.path();
    key.push_str(if path.len() > 1 {
        path.strip_suffix('/').unwrap_or(path)
    } else {
        path
    });
    if let Some(query) = url.query() {
        let mut params: Vec<&str> = query
            .split('&')
            .filter(|param| !param.is_empty() && !is_tracking(param))
            .collect();
        params.sort_unstable();
        if !params.is_empty() {
            key.push('?');
            key.push_str(&params.join("&"));
        }
    }
    Some(key)
}

/// The match key of `input` without its query.
pub fn match_key_any_query(input: &str) -> Option<String> {
    let key = match_key(input)?;
    Some(match key.split_once('?') {
        Some((without, _)) => without.to_string(),
        None => key,
    })
}

/// A denylist target for exactly this page: `url-sha256:<hex>` of its match
/// key.
pub fn url_sha256(input: &str) -> Option<String> {
    match_key(input).map(|key| hashed(&key))
}

/// A denylist target for this page with any query: `url-sha256:<hex>` of its
/// match key without the query. A node checks both hashes of every URL, so
/// adding `?x=1` does not get past an entry made this way.
pub fn url_sha256_any_query(input: &str) -> Option<String> {
    match_key_any_query(input).map(|key| hashed(&key))
}

fn hashed(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let mut out = String::with_capacity(11 + 64);
    out.push_str("url-sha256:");
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn is_tracking(param: &str) -> bool {
    let name = param.split('=').next().unwrap_or_default();
    TRACKING_PARAMS
        .iter()
        .any(|known| match known.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == *known,
        })
}

/// `%xx` escapes of unreserved characters (letters, digits, `-._~`)
/// decoded, and every other escape in upper-case hex. `text` is a parsed
/// URL's path or query, which is always ASCII.
fn regular_percent_encoding(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            let hex = &text[i + 1..i + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                    out.push(byte as char);
                } else {
                    out.push('%');
                    out.push_str(&hex.to_ascii_uppercase());
                }
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_urls() {
        let cases = [
            ("https://Example.COM", Some("https://example.com/")),
            ("http://example.com:80/a", Some("http://example.com/a")),
            (
                "https://example.com:8443/a",
                Some("https://example.com:8443/a"),
            ),
            (
                "https://example.com/a/b/../c/./d#part",
                Some("https://example.com/a/c/d"),
            ),
            ("https://example.com/a?", Some("https://example.com/a")),
            (
                "https://example.com/%7e%41%2f%2F",
                Some("https://example.com/~A%2F%2F"),
            ),
            (
                "https://example.com/?q=%c3%a9",
                Some("https://example.com/?q=%C3%A9"),
            ),
            ("https://bücher.de/", Some("https://xn--bcher-kva.de/")),
            ("  https://example.com/x  ", Some("https://example.com/x")),
            ("ftp://example.com/", None),
            ("magnet:?xt=urn:btih:abc", None),
            ("https://user:pass@example.com/", None),
            ("https://mailto:someone@gmail.com", None),
            ("not a url", None),
        ];
        for (input, want) in cases {
            assert_eq!(canonical_url(input).as_deref(), want, "{input}");
        }
    }

    #[test]
    fn canonical_is_stable() {
        for input in [
            "https://Example.com/a%20b/?x=%7e#f",
            "http://www.example.com",
            "https://example.com/%e2%82%ac",
        ] {
            let once = canonical_url(input).unwrap();
            assert!(is_canonical(&once), "{once}");
        }
    }

    #[test]
    fn match_keys() {
        let same = [
            "https://example.com/shop",
            "http://example.com/shop/",
            "https://www.example.com/shop?utm_source=mail&utm_medium=x",
            "https://EXAMPLE.com/shop#reviews",
            "https://example.com/shop?fbclid=abc",
        ];
        for input in same {
            assert_eq!(
                match_key(input).as_deref(),
                Some("example.com/shop"),
                "{input}"
            );
        }
        assert_eq!(
            match_key("https://example.com/").as_deref(),
            Some("example.com/")
        );
        assert_eq!(
            match_key("https://example.com").as_deref(),
            Some("example.com/")
        );
        assert_eq!(
            match_key("https://example.com/s?b=2&a=1&utm_x=3").as_deref(),
            Some("example.com/s?a=1&b=2")
        );
        assert_eq!(
            match_key("https://example.com:8080/s").as_deref(),
            Some("example.com:8080/s")
        );
        assert_ne!(
            match_key("https://example.com/s?id=1"),
            match_key("https://example.com/s?id=2")
        );
        assert_eq!(match_key("magnet:?xt=urn:btih:abc"), None);
    }

    #[test]
    fn any_query_hash_holds_whatever_is_added() {
        let entry = url_sha256_any_query("https://example.com/file").unwrap();
        for dodge in [
            "https://example.com/file?x=1",
            "http://www.example.com/file/?a=b&c=d",
            "https://example.com/file",
        ] {
            assert_eq!(
                url_sha256_any_query(dodge).as_deref(),
                Some(entry.as_str()),
                "{dodge}"
            );
        }
        assert_ne!(
            url_sha256("https://example.com/file?x=1"),
            url_sha256("https://example.com/file")
        );
        assert!(entry.starts_with("url-sha256:") && entry.len() == 11 + 64);
    }

    #[test]
    fn broken_escapes_pass_through() {
        assert_eq!(
            canonical_url("https://example.com/100%25/%zz/%").as_deref(),
            Some("https://example.com/100%25/%zz/%")
        );
    }
}
