//! Links into a site's own search, for queries like "github plumb search":
//! Plumb only knows sites by name, so for the rest of the words it hands
//! off to the site.

use crate::registrable_domain;

/// Where the words go in a search address, as in OpenSearch descriptions.
pub const SEARCH_TERMS: &str = "{searchTerms}";

/// Search addresses of a few big sites, for before a crawl has read their
/// homepage's search form. Keyed by registrable domain.
const KNOWN_SEARCH_TEMPLATES: &[(&str, &str)] = &[
    ("amazon.com", "https://www.amazon.com/s?k={searchTerms}"),
    ("apple.com", "https://www.apple.com/us/search/{searchTerms}"),
    (
        "archive.org",
        "https://archive.org/search?query={searchTerms}",
    ),
    ("bbc.co.uk", "https://www.bbc.co.uk/search?q={searchTerms}"),
    (
        "bestbuy.com",
        "https://www.bestbuy.com/site/searchpage.jsp?st={searchTerms}",
    ),
    ("cnn.com", "https://www.cnn.com/search?q={searchTerms}"),
    ("crates.io", "https://crates.io/search?q={searchTerms}"),
    (
        "docs.rs",
        "https://docs.rs/releases/search?query={searchTerms}",
    ),
    (
        "ebay.com",
        "https://www.ebay.com/sch/i.html?_nkw={searchTerms}",
    ),
    ("etsy.com", "https://www.etsy.com/search?q={searchTerms}"),
    (
        "facebook.com",
        "https://www.facebook.com/search/top/?q={searchTerms}",
    ),
    ("github.com", "https://github.com/search?q={searchTerms}"),
    (
        "gitlab.com",
        "https://gitlab.com/search?search={searchTerms}",
    ),
    ("homedepot.com", "https://www.homedepot.com/s/{searchTerms}"),
    ("imdb.com", "https://www.imdb.com/find/?q={searchTerms}"),
    (
        "linkedin.com",
        "https://www.linkedin.com/search/results/all/?keywords={searchTerms}",
    ),
    ("medium.com", "https://medium.com/search?q={searchTerms}"),
    (
        "microsoft.com",
        "https://www.microsoft.com/en-us/search/explore?q={searchTerms}",
    ),
    (
        "mozilla.org",
        "https://developer.mozilla.org/en-US/search?q={searchTerms}",
    ),
    (
        "netflix.com",
        "https://www.netflix.com/search?q={searchTerms}",
    ),
    ("npmjs.com", "https://www.npmjs.com/search?q={searchTerms}"),
    (
        "nytimes.com",
        "https://www.nytimes.com/search?query={searchTerms}",
    ),
    (
        "pinterest.com",
        "https://www.pinterest.com/search/pins/?q={searchTerms}",
    ),
    ("pypi.org", "https://pypi.org/search/?q={searchTerms}"),
    ("quora.com", "https://www.quora.com/search?q={searchTerms}"),
    (
        "reddit.com",
        "https://www.reddit.com/search/?q={searchTerms}",
    ),
    (
        "spotify.com",
        "https://open.spotify.com/search/{searchTerms}",
    ),
    (
        "stackoverflow.com",
        "https://stackoverflow.com/search?q={searchTerms}",
    ),
    (
        "steampowered.com",
        "https://store.steampowered.com/search/?term={searchTerms}",
    ),
    (
        "target.com",
        "https://www.target.com/s?searchTerm={searchTerms}",
    ),
    (
        "tiktok.com",
        "https://www.tiktok.com/search?q={searchTerms}",
    ),
    (
        "twitch.tv",
        "https://www.twitch.tv/search?term={searchTerms}",
    ),
    ("twitter.com", "https://x.com/search?q={searchTerms}"),
    (
        "walmart.com",
        "https://www.walmart.com/search?q={searchTerms}",
    ),
    (
        "wikipedia.org",
        "https://en.wikipedia.org/w/index.php?search={searchTerms}",
    ),
    ("x.com", "https://x.com/search?q={searchTerms}"),
    (
        "youtube.com",
        "https://www.youtube.com/results?search_query={searchTerms}",
    ),
];

/// A built-in search address for `domain`, a registrable domain, if Plumb
/// knows one.
pub fn search_template_for(domain: &str) -> Option<&'static str> {
    KNOWN_SEARCH_TEMPLATES
        .iter()
        .find(|(known, _)| *known == domain)
        .map(|(_, template)| *template)
}

/// The address that searches the site `domain` for `terms`, from its
/// search `template`. `None` unless the template is an http(s) address on
/// `domain` itself (or `twitter.com`'s move to `x.com`) holding
/// [`SEARCH_TERMS`] once, and `terms` has some text. The terms are
/// percent-encoded, spaces as `%20`, which works in paths and queries alike.
pub fn search_link(template: &str, domain: &str, terms: &str) -> Option<String> {
    let terms = terms.trim();
    if terms.is_empty() || template.matches(SEARCH_TERMS).count() != 1 {
        return None;
    }
    let lower = template.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return None;
    }
    let probe = template.replace(SEARCH_TERMS, "x");
    let target = registrable_domain(&probe)?;
    let moved = domain == "twitter.com" && target == "x.com";
    if target != domain && !moved {
        return None;
    }
    let encoded: String = url::form_urlencoded::byte_serialize(terms.as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    Some(template.replace(SEARCH_TERMS, &encoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_encode_the_terms() {
        assert_eq!(
            search_link(
                "https://github.com/search?q={searchTerms}",
                "github.com",
                "sueheir plumb-search"
            )
            .as_deref(),
            Some("https://github.com/search?q=sueheir%20plumb-search")
        );
        assert_eq!(
            search_link(
                "https://www.homedepot.com/s/{searchTerms}",
                "homedepot.com",
                "a&b/c+d"
            )
            .as_deref(),
            Some("https://www.homedepot.com/s/a%26b%2Fc%2Bd")
        );
    }

    #[test]
    fn links_stay_on_the_site() {
        let other = "https://evil.example/search?q={searchTerms}";
        assert_eq!(search_link(other, "github.com", "x"), None);
        assert_eq!(
            search_link("javascript:alert({searchTerms})", "github.com", "x"),
            None
        );
        assert_eq!(
            search_link("https://github.com/search", "github.com", "x"),
            None
        );
        assert_eq!(
            search_link(
                "https://github.com/search?q={searchTerms}",
                "github.com",
                "  "
            ),
            None
        );
        assert_eq!(
            search_link(
                "https://github.com/{searchTerms}?q={searchTerms}",
                "github.com",
                "x"
            ),
            None
        );
    }

    #[test]
    fn every_known_template_works() {
        for (domain, template) in KNOWN_SEARCH_TEMPLATES {
            assert!(
                search_link(template, domain, "test words").is_some(),
                "{domain}"
            );
            assert_eq!(search_template_for(domain), Some(*template));
        }
        assert_eq!(search_template_for("example.com"), None);
    }
}
