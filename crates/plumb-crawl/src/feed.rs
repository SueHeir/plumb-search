//! Reading recent headlines out of a site's RSS or Atom feed.
//!
//! Only each post's title, link and date are read; feed bodies (summaries,
//! full text) are skipped. Posts without a date the reader understands are
//! left out, since nothing says they are recent.

use std::borrow::Cow;

use plumb_core::Headline;
use url::Url;

/// Most XML nodes a feed may have; a feed with more is not read.
const MAX_FEED_NODES: u32 = 200_000;

/// The recent posts in a feed of `domain`'s, fetched from `feed_url` at
/// `now`, newest first, each checked with [`Headline::checked`] (on the
/// site, within the last week). Relative links are resolved against
/// `feed_url`. Reads RSS 2.0 (and 0.9x), RSS 1.0 (RDF) and Atom. `None`
/// when the body is not one of those.
pub fn read_feed(domain: &str, feed_url: &Url, body: &str, now: u64) -> Option<Vec<Headline>> {
    let body = body.trim_start_matches('\u{feff}');
    // A DTD can define entities that expand without bound (a "billion
    // laughs" feed), so the DOCTYPE is cut out and DTDs are refused. A feed
    // that used an entity of its own then fails to parse, and is not read.
    let body = without_doctype(body);
    let options = roxmltree::ParsingOptions {
        allow_dtd: false,
        nodes_limit: MAX_FEED_NODES,
        ..roxmltree::ParsingOptions::default()
    };
    let doc = roxmltree::Document::parse_with_options(&body, options).ok()?;
    let root = doc.root_element();
    let items: Vec<roxmltree::Node> = match root.tag_name().name() {
        "rss" => root
            .children()
            .filter(|n| n.has_tag_name("channel"))
            .flat_map(|channel| channel.children().filter(|n| n.has_tag_name("item")))
            .collect(),
        // RSS 1.0 lists its items next to the channel.
        "RDF" => root.children().filter(|n| n.has_tag_name("item")).collect(),
        "feed" => root
            .children()
            .filter(|n| n.has_tag_name("entry"))
            .collect(),
        _ => return None,
    };
    let mut headlines: Vec<Headline> = items
        .into_iter()
        .filter_map(|item| {
            let (title, link, at) = read_item(item)?;
            let link = feed_url.join(link.trim()).ok()?;
            Headline::checked(domain, &title, link.as_str(), at, now)
        })
        .collect();
    headlines.sort_by_key(|h| std::cmp::Reverse(h.at));
    headlines.dedup_by(|a, b| a.url == b.url);
    Some(headlines)
}

/// `body` with the DOCTYPE declaration in its prolog (internal subset and
/// all) cut out. A body without one, or whose declaration does not end,
/// comes back unchanged (and a DOCTYPE left in it is refused by the parser).
fn without_doctype(body: &str) -> Cow<'_, str> {
    let Some(start) = body.find("<!DOCTYPE") else {
        return Cow::Borrowed(body);
    };
    // Only a DOCTYPE in the prolog, before the root element, is a DOCTYPE.
    if !prolog_is_markup_only(&body[..start]) {
        return Cow::Borrowed(body);
    }
    let mut depth = 0usize;
    let mut quote = None;
    for (i, c) in body[start..].char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '[') => depth += 1,
            (None, ']') => depth = depth.saturating_sub(1),
            (None, '>') if depth == 0 => {
                let end = start + i + 1;
                return Cow::Owned(format!("{}{}", &body[..start], &body[end..]));
            }
            _ => {}
        }
    }
    Cow::Borrowed(body)
}

/// Whether `prolog` holds only the XML declaration, comments and
/// processing instructions (no element has started before it ends).
fn prolog_is_markup_only(prolog: &str) -> bool {
    let mut rest = prolog.trim_start();
    while !rest.is_empty() {
        let end = if rest.starts_with("<?") {
            rest.find("?>").map(|i| i + 2)
        } else if rest.starts_with("<!--") {
            rest.find("-->").map(|i| i + 3)
        } else {
            None
        };
        match end {
            Some(end) => rest = rest[end..].trim_start(),
            None => return false,
        }
    }
    true
}

/// An item's (or entry's) title, link and date.
fn read_item(item: roxmltree::Node) -> Option<(String, String, u64)> {
    let mut title = None;
    let mut link = None;
    let mut permalink = None;
    let mut published = None;
    let mut updated = None;
    for child in item.children().filter(roxmltree::Node::is_element) {
        match child.tag_name().name() {
            "title" if title.is_none() => title = Some(text_of(child)),
            "link" if link.is_none() => {
                // Atom: <link rel="alternate" href="...">; RSS: <link>url</link>.
                match child.attribute("href") {
                    Some(href) => {
                        if child.attribute("rel").is_none_or(|rel| rel == "alternate") {
                            link = Some(href.to_string());
                        }
                    }
                    None => link = Some(text_of(child)),
                }
            }
            "guid" if child.attribute("isPermaLink") != Some("false") => {
                permalink = Some(text_of(child));
            }
            "pubDate" | "published" | "issued" => published = parse_date(&text_of(child)),
            "date" | "updated" | "modified" => updated = parse_date(&text_of(child)),
            _ => {}
        }
    }
    let link = link
        .filter(|l| !l.trim().is_empty())
        .or(permalink.filter(|l| l.starts_with("http")))?;
    Some((title?, link, published.or(updated)?))
}

/// The text inside an element, its child elements' included (an Atom
/// title of type "xhtml" is markup).
fn text_of(node: roxmltree::Node) -> String {
    node.descendants()
        .filter(roxmltree::Node::is_text)
        .filter_map(|n| n.text())
        .collect()
}

/// Unix seconds of a feed date: RFC 3339 / ISO 8601 (Atom, `dc:date`) or
/// RFC 822 / 2822 (RSS `pubDate`).
pub fn parse_date(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.as_bytes().first()?.is_ascii_digit() && text.as_bytes().get(4) == Some(&b'-') {
        parse_rfc3339(text)
    } else {
        parse_rfc2822(text)
    }
}

/// `2026-10-05T03:46:25Z`, `2026-10-05T03:46:25.123+02:00`, or a bare date.
fn parse_rfc3339(text: &str) -> Option<u64> {
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: u32 = text.get(5..7)?.parse().ok()?;
    let day: u32 = text.get(8..10)?.parse().ok()?;
    let rest = text.get(10..).unwrap_or_default();
    let Some(time) = rest.strip_prefix(['T', 't', ' ']) else {
        return to_unix(year, month, day, 0, 0, 0, 0);
    };
    let hour: u32 = time.get(0..2)?.parse().ok()?;
    let minute: u32 = time.get(3..5)?.parse().ok()?;
    let (second, zone) = match time.get(5..6) {
        Some(":") => (time.get(6..8)?.parse().ok()?, time.get(8..)?),
        _ => (0, time.get(5..)?),
    };
    let zone = zone.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    to_unix(year, month, day, hour, minute, second, zone_offset(zone)?)
}

/// `Sun, 05 Oct 2026 03:46:25 +0000`, with or without the weekday and
/// seconds, a two-digit year, or a named zone (`GMT`, `EST`).
fn parse_rfc2822(text: &str) -> Option<u64> {
    let text = match text.find(',') {
        Some(comma) => &text[comma + 1..],
        None => text,
    };
    let mut words = text.split_whitespace();
    let day: u32 = words.next()?.parse().ok()?;
    let month = month_number(words.next()?)?;
    let year: i64 = match words.next()?.parse().ok()? {
        y @ 0..=49 => 2000 + y,
        y @ 50..=99 => 1900 + y,
        y => y,
    };
    let time = words.next()?;
    let mut parts = time.split(':');
    let hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = parts.next()?.parse().ok()?;
    let second: u32 = match parts.next() {
        Some(s) => s.parse().ok()?,
        None => 0,
    };
    let offset = match words.next() {
        Some(zone) => zone_offset(zone)?,
        None => 0,
    };
    to_unix(year, month, day, hour, minute, second, offset)
}

fn month_number(name: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let short = name.get(..3)?.to_ascii_lowercase();
    MONTHS
        .iter()
        .position(|m| *m == short)
        .map(|i| i as u32 + 1)
}

/// Seconds east of UTC: `Z`, `+02:00`, `-0500`, or a US or UTC zone name.
fn zone_offset(zone: &str) -> Option<i64> {
    let zone = zone.trim();
    let named = match zone.to_ascii_uppercase().as_str() {
        "" | "Z" | "GMT" | "UT" | "UTC" => Some(0),
        "EDT" => Some(-4),
        "EST" | "CDT" => Some(-5),
        "CST" | "MDT" => Some(-6),
        "MST" | "PDT" => Some(-7),
        "PST" => Some(-8),
        _ => None,
    };
    if let Some(hours) = named {
        return Some(hours * 3600);
    }
    let sign = match zone.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits: String = zone[1..].chars().filter(char::is_ascii_digit).collect();
    if digits.len() != 4 {
        return None;
    }
    let h: i64 = digits[..2].parse().ok()?;
    let m: i64 = digits[2..].parse().ok()?;
    Some(sign * (h * 3600 + m * 60))
}

/// Unix seconds of a civil time `offset` seconds east of UTC; `None` for
/// impossible fields or a time before 1970.
fn to_unix(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    offset: i64,
) -> Option<u64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    // A leap second counts as the next second.
    if second > 60 {
        return None;
    }
    // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second)
        - offset;
    u64::try_from(secs).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_791_158_400; // 2026-10-05T00:00:00Z

    #[test]
    fn reads_both_date_forms() {
        assert_eq!(parse_date("2026-10-05T00:00:00Z"), Some(NOW));
        assert_eq!(parse_date("2026-10-05T02:00:00.250+02:00"), Some(NOW));
        assert_eq!(parse_date("2026-10-05"), Some(NOW));
        assert_eq!(parse_date("Mon, 05 Oct 2026 00:00:00 GMT"), Some(NOW));
        assert_eq!(parse_date("Sun, 04 Oct 2026 19:00:00 -0500"), Some(NOW));
        assert_eq!(parse_date("4 Oct 2026 20:00 EDT"), Some(NOW));
        assert_eq!(parse_date("Sun, 04 Oct 2026 17:00:00 PDT"), Some(NOW));
        assert_eq!(parse_date("05 Oct 26 00:00:00 +0000"), Some(NOW));
        assert_eq!(parse_date("yesterday"), None);
        assert_eq!(parse_date("2026-13-05T00:00:00Z"), None);
    }

    #[test]
    fn reads_rss_items() {
        let feed = Url::parse("https://www.example.com/feed.xml").unwrap();
        let body = r#"<?xml version="1.0"?>
            <rss version="2.0" xmlns:dc="http://purl.org/dc/elements/1.1/"><channel>
              <title>Example News</title>
              <item><title>Older story</title><link>https://www.example.com/older</link>
                <pubDate>Sun, 04 Oct 2026 12:00:00 GMT</pubDate></item>
              <item><title><![CDATA[Big <b>news</b> today]]></title><link>/big</link>
                <dc:date>2026-10-04T23:00:00Z</dc:date><description>Long text</description></item>
              <item><title>Elsewhere</title><link>https://other.org/x</link>
                <pubDate>Sun, 04 Oct 2026 22:00:00 GMT</pubDate></item>
              <item><title>No date</title><link>https://www.example.com/undated</link></item>
              <item><title>Last month</title><link>https://www.example.com/old</link>
                <pubDate>Fri, 04 Sep 2026 12:00:00 GMT</pubDate></item>
              <item><title>Guid only</title><guid>https://www.example.com/guid</guid>
                <pubDate>Sun, 04 Oct 2026 11:00:00 GMT</pubDate></item>
            </channel></rss>"#;
        let items = read_feed("example.com", &feed, body, NOW).unwrap();
        let titles: Vec<&str> = items.iter().map(|h| h.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Big <b>news</b> today", "Older story", "Guid only"]
        );
        assert_eq!(items[0].url, "https://www.example.com/big");
        assert_eq!(items[0].at, NOW - 3600);
    }

    #[test]
    fn reads_atom_and_rdf_entries() {
        let feed = Url::parse("https://blog.example.com/atom.xml").unwrap();
        let atom = r#"<feed xmlns="http://www.w3.org/2005/Atom"><title>Blog</title>
              <entry><title type="html">Release 2.0</title>
                <link rel="replies" href="https://blog.example.com/2.0#comments"/>
                <link href="https://blog.example.com/2.0"/>
                <updated>2026-10-04T10:00:00Z</updated></entry>
            </feed>"#;
        let items = read_feed("example.com", &feed, atom, NOW).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].url, "https://blog.example.com/2.0");
        let rdf = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
              xmlns="http://purl.org/rss/1.0/" xmlns:dc="http://purl.org/dc/elements/1.1/">
              <channel><title>R</title></channel>
              <item><title>An RDF post</title><link>https://example.com/p</link>
                <dc:date>2026-10-04T09:00:00+00:00</dc:date></item>
            </rdf:RDF>"#;
        let items = read_feed("example.com", &feed, rdf, NOW).unwrap();
        assert_eq!(items[0].title, "An RDF post");
        assert!(read_feed("example.com", &feed, "<html><body>hi</body></html>", NOW).is_none());
        assert!(read_feed("example.com", &feed, "not xml", NOW).is_none());
    }

    #[test]
    fn drops_doctypes_and_refuses_entity_expansion() {
        let feed = Url::parse("https://www.example.com/rss").unwrap();
        // RSS 0.91 feeds carry a DOCTYPE; they still read.
        let rss091 = r#"<?xml version="1.0"?>
            <!-- generated -->
            <!DOCTYPE rss PUBLIC "-//Netscape Communications//DTD RSS 0.91//EN"
              "http://my.netscape.com/publish/formats/rss-0.91.dtd">
            <rss version="0.91"><channel><title>Old</title>
              <item><title>Still here</title><link>https://www.example.com/a</link>
                <pubDate>Sun, 04 Oct 2026 12:00:00 GMT</pubDate></item>
            </channel></rss>"#;
        let items = read_feed("example.com", &feed, rss091, NOW).unwrap();
        assert_eq!(items[0].title, "Still here");
        // Entities defined in an internal subset are never expanded: the
        // feed does not parse rather than growing without bound.
        let laughs = r#"<?xml version="1.0"?>
            <!DOCTYPE rss [
              <!ENTITY a "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa">
              <!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;">
              <!ENTITY c "&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;">
            ]>
            <rss version="2.0"><channel><title>&c;&c;&c;</title>
              <item><title>&c;</title><link>https://www.example.com/a</link>
                <pubDate>Sun, 04 Oct 2026 12:00:00 GMT</pubDate></item>
            </channel></rss>"#;
        assert!(read_feed("example.com", &feed, laughs, NOW).is_none());
        // A subset with `]` and `>` inside quoted values is cut whole.
        let quoted = r#"<!DOCTYPE feed [ <!ENTITY x "]>"> ]>
            <feed xmlns="http://www.w3.org/2005/Atom">
              <entry><title>Q</title><link href="https://www.example.com/q"/>
                <updated>2026-10-04T10:00:00Z</updated></entry></feed>"#;
        assert_eq!(
            read_feed("example.com", &feed, quoted, NOW).unwrap().len(),
            1
        );
        // A DOCTYPE string inside the document is left alone.
        let inside = "<a><![CDATA[<!DOCTYPE x>]]></a>";
        assert_eq!(without_doctype(inside), inside);
    }
}
