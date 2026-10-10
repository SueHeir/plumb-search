//! Small, pure assembly rules shared by MCP and web adapters.

use plumb_core::place::{Place, OSM_COPYRIGHT_URL};
use plumb_core::{normalize_text, SafeSearch};
use plumb_index::pages::PlacedPage;
use plumb_index::places::{parse_place_query, Near, PlaceResults};
use plumb_index::{Hit, SearchOptions};
use serde::Serialize;

use crate::web::http_url;

pub(crate) const MAX_PLACES: usize = 8;

/// A guessed town must not turn a named non-place into local businesses.
pub(crate) fn not_a_name(found: Option<PlaceResults>, hits: &[Hit]) -> Option<PlaceResults> {
    found.filter(|found| !(found.guessed && hits.iter().any(|hit| hit.named)))
}

/// The existing place fields remain available, with explicit coverage and attribution.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Places {
    #[serde(flatten)]
    pub found: PlaceResults,
    pub status: &'static str,
    pub attribution: &'static str,
    pub attribution_url: &'static str,
    pub license: &'static str,
    pub osm_urls: Vec<String>,
    /// Place records currently carry no verified content language.
    pub language: Option<&'static str>,
}

pub(crate) fn places(
    query: &str,
    found: Option<PlaceResults>,
    hits: &[Hit],
    options: &SearchOptions,
    blocks_adult: impl Fn(&str) -> bool,
) -> Option<Places> {
    // Place records cannot satisfy page/site operators. Their language is unknown.
    if plumb_core::Operators::parse(query).any() {
        return None;
    }
    let asked = parse_place_query(query)?;
    let had_results = found.is_some();
    let found = not_a_name(found, hits);
    if had_results && found.is_none() {
        return None;
    }
    let mut found = found.or_else(|| {
        asked.said_where.then(|| PlaceResults {
            what: asked.what,
            center: None,
            near_me: asked.near == Near::Me,
            guessed: false,
            radius_km: 0.0,
            hits: Vec::new(),
        })
    })?;
    for hit in &mut found.hits {
        hit.place.website = hit.place.website.as_deref().and_then(http_url);
    }
    found.hits.retain(|hit| {
        (options.safe == SafeSearch::Off
            || !matches!(
                hit.place.kind.as_str(),
                "shop=erotic" | "amenity=stripclub" | "amenity=brothel"
            ) && !hit
                .place
                .website
                .as_deref()
                .and_then(plumb_core::registrable_domain)
                .is_some_and(|domain| blocks_adult(&domain)))
            && (!options.only_country
                || options.country.is_none()
                || options.country.as_deref() == hit.place.country.as_deref())
    });
    found.hits.truncate(MAX_PLACES);
    let status = if found.center.is_none() {
        if found.near_me {
            "missing_location"
        } else {
            "location_unavailable"
        }
    } else if found.hits.is_empty() {
        "no_indexed_matches"
    } else {
        "available"
    };
    let osm_urls = found.hits.iter().map(|hit| hit.place.osm_url()).collect();
    Some(Places {
        found,
        status,
        osm_urls,
        language: None,
        attribution: "OpenStreetMap contributors",
        attribution_url: OSM_COPYRIGHT_URL,
        license: "ODbL",
    })
}

/// One top-level result; supporting pages stay attached to their site.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Row<'a> {
    Site {
        site: &'a Hit,
        pages: Vec<&'a PlacedPage>,
    },
    Page {
        page: &'a PlacedPage,
    },
}

/// The bounded output shared by adapters, preserving typed source objects.
#[derive(Serialize)]
pub(crate) struct Assembled<'a> {
    pub rows: Vec<Row<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub places: Option<Places>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recent: Option<crate::news::Recent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<&'a plumb_answer::Answer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<&'a crate::web::answers::ProfileAnswer>,
}

pub(crate) fn ordered_rows<'a>(
    hits: &'a [Hit],
    pages: &'a [PlacedPage],
    limit: usize,
) -> Vec<Row<'a>> {
    let mut rows = Vec::new();
    for (at, site) in hits.iter().enumerate() {
        rows.extend(
            pages
                .iter()
                .filter(|p| p.under.is_none() && p.at == at)
                .map(|page| Row::Page { page }),
        );
        rows.push(Row::Site {
            site,
            pages: pages
                .iter()
                .filter(|p| p.under.as_deref() == Some(site.domain.as_str()))
                .collect(),
        });
    }
    rows.extend(
        pages
            .iter()
            .filter(|p| p.under.is_none() && p.at >= hits.len())
            .map(|page| Row::Page { page }),
    );
    rows.truncate(limit);
    rows
}

pub(crate) fn route_sources(
    query: &str,
    answer: Option<plumb_answer::Kind>,
    country: Option<&str>,
    results: &mut plumb_index::SearchResults,
    limit: usize,
    site: impl Fn(&str) -> Option<Hit>,
) {
    if plumb_core::Operators::parse(query).any() {
        return;
    }
    let Some(route) = crate::sources::route(query, answer, country) else {
        return;
    };
    let keep = results.hits.len().max(limit);
    crate::sources::lead_with(&mut results.hits, &mut results.pages, &route, site);
    results.hits.truncate(keep);
}

/// Guessed cards only lead explicit package, version or install requests.
pub(crate) fn promote_package(query: &str) -> bool {
    plumb_core::packages::install_command(query).is_some()
        || plumb_core::packages::package_query(query).is_some_and(|asked| {
            asked.surely
                || query.split_whitespace().any(|word| {
                    matches!(
                        word.to_ascii_lowercase().as_str(),
                        "version" | "versions" | "install"
                    )
                })
        })
}

/// The places' own websites as results, each site once, in the order the
/// places are listed: titled with the place's name, described by its kind
/// and address.
pub(crate) fn local_sites(found: &PlaceResults) -> Vec<Hit> {
    let mut sites: Vec<Hit> = Vec::new();
    for hit in &found.hits {
        let Some(url) = hit.place.website.as_deref().and_then(http_url) else {
            continue;
        };
        let Some(domain) = url::Url::parse(&url).ok().and_then(|u| {
            let host = u.host_str()?.to_string();
            plumb_core::registrable_domain(&host).or(Some(host))
        }) else {
            continue;
        };
        if sites.iter().any(|s| s.domain == domain) {
            continue;
        }
        let mut about = hit.place.label();
        if let Some(address) = &hit.place.address {
            about.push_str(" \u{b7} ");
            about.push_str(address);
        }
        sites.push(Hit {
            domain,
            url,
            title: Some(hit.place.name.clone()),
            description: Some(about),
            score: 0.0,
            text_score: 0.0,
            link_score: 0.0,
            placing_text_score: None,
            country: hit.place.country.clone(),
            named: false,
            official: false,
            key_pages: Vec::new(),
            demand: None,
            missing_words: false,
        });
    }
    sites
}

/// Link the same bounded local-site candidates on every surface; the adapter supplies lookups.
pub(crate) fn linked_sites(
    found: &PlaceResults,
    site: impl Fn(&str) -> Option<Hit>,
    search: impl Fn(&str) -> Vec<Hit>,
) -> Vec<Hit> {
    let mut sites: Vec<Hit> = local_sites(found)
        .into_iter()
        .map(|hit| site(&hit.domain).unwrap_or(hit))
        .collect();
    for place in places_without_sites(found) {
        if let Some(mut hit) = site_named_for(&place, search(&place.name)) {
            if !sites.iter().any(|s| s.domain == hit.domain) {
                hit.score = 0.0;
                hit.text_score = 0.0;
                hit.placing_text_score = None;
                hit.named = false;
                sites.push(hit);
            }
        }
    }
    sites
}

/// The places of `found` that give no website, at most
/// [`MAX_NAME_LOOKUPS`] of them, for their sites to be looked up by name
/// ([`site_named_for`]).
pub(crate) fn places_without_sites(found: &PlaceResults) -> Vec<Place> {
    found
        .hits
        .iter()
        .map(|hit| &hit.place)
        .filter(|place| place.website.as_deref().and_then(http_url).is_none())
        .take(MAX_NAME_LOOKUPS)
        .cloned()
        .collect()
}

/// How many places without a website are looked up by name.
const MAX_NAME_LOOKUPS: usize = 6;

/// Among `hits` found for a place's name, the place's own site: one whose
/// domain spells the name or its first two words or more (elliottbaybook.com for
/// "Elliott Bay Book Company"), and that is well known (a chain:
/// starbucks.com) or says the place's town. Small places share names
/// across towns; another town's Joe's Pizza is not this one.
pub(crate) fn site_named_for(place: &Place, hits: Vec<Hit>) -> Option<Hit> {
    let squash = |text: &str| normalize_text(text).replace(' ', "");
    let town = place.town.as_deref().map(normalize_text);
    hits.into_iter().find(|hit| {
        let label = squash(hit.domain.split('.').next().unwrap_or(""));
        if label.len() < 5 {
            return false;
        }
        // The whole name, or two words of it or more: elliott.com is not
        // the Elliott Bay Book Company.
        let name = normalize_text(&place.name);
        let words: Vec<&str> = name.split(' ').collect();
        let mut lead = String::new();
        let spelled = words.iter().enumerate().any(|(n, word)| {
            lead.push_str(word);
            lead == label && (n >= 1 || words.len() == 1)
        });
        let local = town.as_deref().is_some_and(|town| {
            !town.is_empty()
                && [hit.title.as_deref(), hit.description.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|text| {
                        format!(" {} ", normalize_text(text)).contains(&format!(" {town} "))
                    })
        });
        spelled && (hit.link_score >= plumb_index::WELL_KNOWN_LINK_SCORE || local)
    })
}

/// The sites for a query that lists places around a town ("brewery in
/// denver"): the places' own sites (`local`, from [`local_sites`])
/// first, then the sites that say what was looked for, then the rest;
/// sites named after the town (the city's own site, its football team)
/// are left out when places have sites to show. At most `limit`, or as
/// many as there were.
pub(crate) fn local_first(
    found: &PlaceResults,
    hits: &mut Vec<Hit>,
    local: Vec<Hit>,
    limit: usize,
) {
    if found.center.is_none() || found.near_me {
        return;
    }
    let roots: Vec<String> = normalize_text(&found.what)
        .split(' ')
        .filter_map(word_root)
        .collect();
    if roots.is_empty() {
        return;
    }
    let says_what = |hit: &Hit| {
        let text = normalize_text(&format!(
            "{} {}",
            hit.title.as_deref().unwrap_or(""),
            hit.description.as_deref().unwrap_or("")
        ));
        let words: Vec<&str> = text.split(' ').collect();
        roots
            .iter()
            .all(|root| words.iter().any(|w| w.starts_with(root.as_str())))
    };
    let keep = hits.len().max(limit.min(hits.len() + local.len()));
    let rest = std::mem::take(hits);
    let mut sorted: Vec<Hit> = local;
    let (what, town): (Vec<Hit>, Vec<Hit>) = rest
        .into_iter()
        .filter(|hit| !sorted.iter().any(|s| s.domain == hit.domain))
        .partition(says_what);
    let local = sorted.len();
    sorted.extend(what);
    // Sites named after the town (its government, university, football
    // team) are not what was asked for; with the places' own sites to
    // show, they are left out.
    let town_name = found
        .center
        .as_ref()
        .map(|center| normalize_text(&center.name).replace(' ', ""))
        .unwrap_or_default();
    let named_after_town = |hit: &Hit| {
        town_name.len() >= 3
            && (hit.domain.replace(['.', '-'], "").contains(&town_name)
                || hit
                    .title
                    .as_deref()
                    .is_some_and(|t| normalize_text(t).replace(' ', "").contains(&town_name)))
    };
    // Without any, they go last: Seattle's climbing gyms have no sites of
    // their own, and seattle.gov is still not one.
    let (named, other): (Vec<Hit>, Vec<Hit>) = town.into_iter().partition(named_after_town);
    sorted.extend(other);
    if local == 0 {
        sorted.extend(named);
    }
    sorted.truncate(keep);
    *hits = sorted;
}

/// What a word of a query starts with in its other forms: "brewer" for
/// "brewery" and "breweries", "hotel" for "hotels". `None` for words too
/// short to tell by.
pub(crate) fn word_root(word: &str) -> Option<String> {
    let root = if let Some(stem) = word.strip_suffix("ies") {
        stem
    } else if let Some(stem) = word.strip_suffix('y') {
        stem
    } else if let Some(stem) = word
        .strip_suffix("es")
        .filter(|w| w.ends_with(['s', 'x', 'h']))
    {
        stem
    } else {
        word.strip_suffix('s').unwrap_or(word)
    };
    (root.chars().count() >= 3).then(|| root.to_string())
}

/// Constrain auxiliary headlines with the same operators and safe-search policy.
pub(crate) fn recent(
    query: &str,
    recent: Option<crate::news::Recent>,
    options: &SearchOptions,
    limit: usize,
    blocks_adult: impl Fn(&str) -> bool,
) -> Option<crate::news::Recent> {
    if options.recent == plumb_core::RecentNews::Off {
        return None;
    }
    let mut recent = recent?;
    let operators = plumb_core::Operators::parse(query);
    recent.headlines.retain(|h| {
        plumb_core::host_of(&h.url).is_some_and(|host| operators.allows(&host, [&h.title as &str]))
            && (options.safe == SafeSearch::Off || !blocks_adult(&h.domain))
    });
    recent.headlines.truncate(limit.min(5));
    if recent.headlines.is_empty() && recent.status == "available" {
        recent.status = "filtered".into();
    }
    Some(recent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plumb_index::places::PlaceHit;

    #[test]
    fn place_limits_and_filters_do_not_broaden_location_or_remove_identity() {
        let mut found = PlaceResults {
            what: "shops".into(),
            center: Some(Place {
                name: "Denver".into(),
                kind: "place=city".into(),
                ..Default::default()
            }),
            near_me: false,
            guessed: false,
            radius_km: 12.0,
            hits: (0..20)
                .map(|n| PlaceHit {
                    km: n as f64,
                    place: Place {
                        name: format!("Shop {n}"),
                        osm: format!("n{n}"),
                        kind: "shop=books".into(),
                        country: Some("US".into()),
                        ..Default::default()
                    },
                })
                .collect(),
        };
        found.hits[0].place.kind = "shop=erotic".into();
        found.hits[1].place.country = Some("CA".into());
        found.hits[2].place.website = Some("https://adult.test/".into());
        found.hits[3].place.website = Some("javascript:alert(1)".into());
        let options = SearchOptions {
            only_country: true,
            country: Some("US".into()),
            ..Default::default()
        };
        let shown = places("shops in denver", Some(found), &[], &options, |domain| {
            domain == "adult.test"
        })
        .unwrap();
        assert_eq!(shown.found.hits.len(), MAX_PLACES);
        assert_eq!(shown.found.hits[0].place.osm, "n3");
        assert_eq!(shown.found.hits[0].place.website, None);
        assert_eq!(shown.osm_urls[0], "https://www.openstreetmap.org/node/3");
        assert_eq!(shown.found.center.unwrap().name, "Denver");
    }

    #[test]
    fn auxiliary_headlines_honor_operators_safe_search_and_limit() {
        let headline = |domain: &str| crate::news::RecentHeadline {
            domain: domain.into(),
            url: format!("https://{domain}/1"),
            title: "NVIDIA earnings".into(),
            at: 1,
        };
        let block = crate::news::Recent {
            site: None,
            status: "available".into(),
            sources: Vec::new(),
            headlines: vec![
                headline("adult.test"),
                headline("bbc.com"),
                headline("reuters.com"),
            ],
        };
        let result = recent(
            "NVIDIA earnings site:bbc.com",
            Some(block),
            &SearchOptions::default(),
            1,
            |domain| domain == "adult.test",
        )
        .unwrap();
        assert_eq!(result.headlines.len(), 1);
        assert_eq!(result.headlines[0].domain, "bbc.com");
    }
}
