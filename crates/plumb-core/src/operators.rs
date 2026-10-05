//! Search operators in a query: `site:example.com`, `"exact words"` and
//! `-word`, as on other search engines. Every place that searches (a
//! node's index, the network, private search in the browser) reads them
//! with [`Operators::parse`] and keeps only what [`Operators`] allows.

use crate::{host_of, normalize_text, other_number, registrable_domain};

/// The operators of a query, and its other words.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Operators {
    /// What to search for: the plain words and the words of each phrase,
    /// in the order typed, without operators.
    pub words: String,
    /// The query as typed without its `site:` and `-site:` words, for a
    /// site's own search.
    pub site_terms: String,
    /// `site:` values: results must be on one of these hosts (or under
    /// one of these top-level domains, for a value without a dot).
    pub sites: Vec<String>,
    /// `-site:` values: results must not be on any of these.
    pub not_sites: Vec<String>,
    /// `"quoted"` phrases, normalized: each must be in a result's text.
    pub phrases: Vec<String>,
    /// `-word` and `-"quoted phrase"`, normalized: none may be in a
    /// result's text.
    pub excluded: Vec<String>,
}

impl Operators {
    /// Reads the operators of `query`. Words that only look like operators
    /// stay words: a lone `-`, `spider-man`, `site:` with nothing after it.
    /// An unclosed quote runs to the end of the query.
    pub fn parse(query: &str) -> Self {
        let mut ops = Operators::default();
        let mut words: Vec<String> = Vec::new();
        let mut terms: Vec<&str> = Vec::new();
        let mut rest = query.trim_start();
        while !rest.is_empty() {
            let start = rest;
            let negated =
                rest.starts_with('-') && rest[1..].starts_with(|c: char| !c.is_whitespace());
            let body = if negated { &rest[1..] } else { rest };
            if let Some(quoted) = body.strip_prefix(['"', '\u{201C}', '\u{201D}']) {
                let close = quoted
                    .char_indices()
                    .find(|&(_, c)| matches!(c, '"' | '\u{201C}' | '\u{201D}'));
                let (phrase, after) = match close {
                    Some((end, c)) => (&quoted[..end], &quoted[end + c.len_utf8()..]),
                    None => (quoted, ""),
                };
                let normal = normalize_text(phrase);
                if !normal.is_empty() {
                    if negated {
                        ops.excluded.push(normal);
                    } else {
                        words.push(phrase.trim().to_string());
                        ops.phrases.push(normal);
                    }
                }
                terms.push(start[..start.len() - after.len()].trim());
                rest = after.trim_start();
                continue;
            }
            let end = body.find(char::is_whitespace).unwrap_or(body.len());
            let word = &body[..end];
            rest = body[end..].trim_start();
            let typed = start[..start.len() - body.len() + end].trim();
            if let Some(site) = site_value(word) {
                if negated {
                    ops.not_sites.push(site);
                } else {
                    ops.sites.push(site);
                }
                continue;
            }
            terms.push(typed);
            if negated {
                let normal = normalize_text(word);
                if !normal.is_empty() {
                    ops.excluded.push(normal);
                    continue;
                }
            }
            if negated {
                words.push(format!("-{word}"));
            } else {
                words.push(word.to_string());
            }
        }
        ops.words = words.join(" ");
        ops.site_terms = terms.join(" ");
        ops
    }

    /// Whether the query used any operator.
    pub fn any(&self) -> bool {
        !(self.sites.is_empty()
            && self.not_sites.is_empty()
            && self.phrases.is_empty()
            && self.excluded.is_empty())
    }

    /// The words to look sites up by: [`Operators::words`], or else the
    /// site the first `site:` names (`site:github.com` looks up github.com).
    pub fn lookup_text(&self) -> String {
        if !self.words.is_empty() {
            return self.words.clone();
        }
        self.sites
            .iter()
            .find_map(|site| registrable_domain(site))
            .unwrap_or_default()
    }

    /// Whether a result on `host` (a site's domain, or the host of a
    /// page's address) may be listed. A site's registrable domain counts
    /// as on a `site:` host below it: `site:docs.python.org` keeps
    /// python.org.
    pub fn allows_host(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        let on = |site: &String| on_site(&host, site);
        (self.sites.is_empty() || self.sites.iter().any(on))
            && !self.not_sites.iter().any(|site| covers(&host, site))
    }

    /// Whether a result whose text is `texts` (its domain, title,
    /// description and the like) has every phrase and no excluded word.
    pub fn allows_text<'a>(&self, texts: impl IntoIterator<Item = &'a str>) -> bool {
        if self.phrases.is_empty() && self.excluded.is_empty() {
            return true;
        }
        let mut text = String::from(" ");
        for part in texts {
            text.push_str(&normalize_text(part));
            text.push(' ');
        }
        let has = |phrase: &str| text.contains(&format!(" {phrase} "));
        self.phrases.iter().all(|phrase| has(phrase))
            && !self.excluded.iter().any(|word| {
                // "-car" leaves out "cars" too.
                has(word)
                    || has(&format!("{word}s"))
                    || other_number(word).is_some_and(|other| has(&other))
            })
    }

    /// [`Operators::allows_host`] and [`Operators::allows_text`] for a
    /// result at `url` on `host`, whose other text is `texts`. The host
    /// counts as text too, so `-amazon` leaves out amazon.com.
    pub fn allows<'a>(&self, host: &'a str, texts: impl IntoIterator<Item = &'a str>) -> bool {
        self.allows_host(host) && self.allows_text(std::iter::once(host).chain(texts))
    }
}

/// The host or top-level domain a `site:` word names, lowercase, if it is
/// one: `site:GitHub.com` -> `github.com`, `site:https://docs.rs/x` ->
/// `docs.rs`, `site:.de` and `site:de` -> `de`.
fn site_value(word: &str) -> Option<String> {
    let (key, value) = word.split_once(':')?;
    if !key.eq_ignore_ascii_case("site") {
        return None;
    }
    let value = value.trim_start_matches("*.").trim_start_matches('.');
    if !value.is_empty() && !value.contains(['.', '/']) {
        let tld = value.to_ascii_lowercase();
        return tld
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            .then_some(tld);
    }
    host_of(value).map(|host| {
        host.strip_prefix("www.")
            .map(str::to_string)
            .unwrap_or(host)
    })
}

/// Whether `host` is `site` or below it (`en.wikipedia.org` is below
/// `wikipedia.org`; every `.de` host is below `de`).
fn covers(host: &str, site: &str) -> bool {
    host == site
        || host
            .strip_suffix(site)
            .is_some_and(|before| before.ends_with('.'))
}

/// [`covers`], or `host` is the registrable domain of `site`.
fn on_site(host: &str, site: &str) -> bool {
    covers(host, site) || registrable_domain(site).as_deref() == Some(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_queries_have_no_operators() {
        for query in [
            "github",
            "spider-man",
            "a - b",
            "site:",
            "10:30 - 11",
            "re:invent",
        ] {
            let ops = Operators::parse(query);
            assert!(!ops.any(), "{query}: {ops:?}");
            assert_eq!(
                ops.words,
                query.split_whitespace().collect::<Vec<_>>().join(" ")
            );
        }
    }

    #[test]
    fn reads_each_operator() {
        let ops = Operators::parse(
            r#"site:GitHub.com rust "web server" -actix -"hello world" -site:gist.github.com"#,
        );
        assert_eq!(ops.words, "rust web server");
        assert_eq!(ops.sites, ["github.com"]);
        assert_eq!(ops.not_sites, ["gist.github.com"]);
        assert_eq!(ops.phrases, ["web server"]);
        assert_eq!(ops.excluded, ["actix", "hello world"]);
        assert_eq!(ops.site_terms, r#"rust "web server" -actix -"hello world""#);
        assert!(ops.any());
    }

    #[test]
    fn site_values_can_be_addresses_or_top_level_domains() {
        assert_eq!(
            Operators::parse("site:https://www.docs.rs/x").sites,
            ["docs.rs"]
        );
        assert_eq!(Operators::parse("site:.de bahn").sites, ["de"]);
        assert_eq!(Operators::parse("SITE:gov").sites, ["gov"]);
        assert_eq!(Operators::parse("site:*.gov.uk").sites, ["gov.uk"]);
    }

    #[test]
    fn an_unclosed_quote_runs_to_the_end() {
        let ops = Operators::parse("\u{201C}to be or not");
        assert_eq!(ops.phrases, ["to be or not"]);
        assert_eq!(ops.words, "to be or not");
    }

    #[test]
    fn a_curly_closing_quote_ends_the_phrase() {
        let ops = Operators::parse("\u{201C}new york\u{201D} pizza");
        assert_eq!(ops.phrases, ["new york"]);
        assert_eq!(ops.words, "new york pizza");
        let ops = Operators::parse("\"foo\u{201D} bar");
        assert_eq!(ops.phrases, ["foo"]);
        assert_eq!(ops.words, "foo bar");
        let ops = Operators::parse("-\u{201C}foo\u{201D} bar");
        assert_eq!(ops.excluded, ["foo"]);
        assert_eq!(ops.words, "bar");
    }

    #[test]
    fn sites_narrow_by_host() {
        let ops = Operators::parse("site:wikipedia.org einstein");
        assert!(ops.allows_host("wikipedia.org"));
        assert!(ops.allows_host("en.wikipedia.org"));
        assert!(!ops.allows_host("notwikipedia.org"));
        assert!(!ops.allows_host("einstein.org"));

        let below = Operators::parse("site:docs.python.org");
        assert!(below.allows_host("python.org"));
        assert!(below.allows_host("docs.python.org"));
        assert!(!below.allows_host("wiki.python.org"));

        let tld = Operators::parse("site:gov tax");
        assert!(tld.allows_host("irs.gov"));
        assert!(!tld.allows_host("gov.uk"));

        let not = Operators::parse("python -site:python.org");
        assert!(!not.allows_host("python.org"));
        assert!(!not.allows_host("docs.python.org"));
        assert!(not.allows_host("realpython.com"));
    }

    #[test]
    fn phrases_and_excluded_words_check_whole_words() {
        let ops = Operators::parse(r#""new york" -car"#);
        assert!(ops.allows("nytimes.com", ["The New York Times"]));
        assert!(!ops.allows("york.ac.uk", ["University of York, New campus"]));
        assert!(!ops.allows("example.com", ["New York cars for sale"]));
        assert!(ops.allows("example.com", ["New York carpets"]));
        let amazon = Operators::parse("shopping -amazon");
        assert!(!amazon.allows("amazon.com", ["Online shopping"]));
        assert!(amazon.allows("ebay.com", ["Online shopping"]));
    }
}
