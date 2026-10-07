//! The places part of a results page: "pizza in denver" lists pizza places
//! in Denver above the sites, with a small map drawn here as SVG, on the
//! streets of the node's own map file when it has one (see [`crate::map`]:
//! the page loads nothing from anywhere else), and links to OpenStreetMap,
//! whose data it is.

use std::fmt::Write as _;

use plumb_core::normalize_text;
use plumb_core::place::{Place, OSM_COPYRIGHT_URL};
use plumb_index::places::{PlaceHit, PlaceResults};
use plumb_index::Hit;

use super::{escape_html, http_url, Icons};

/// Countries that measure roads in miles.
const MILES: &[&str] = &["US", "GB", "LR", "MM"];
/// The map's size in SVG units.
const MAP_WIDTH: f64 = 480.0;
const MAP_HEIGHT: f64 = 260.0;
/// Room around the pins.
const MAP_PAD: f64 = 22.0;

/// Styles of the places part, added to the page's.
pub(super) const STYLE: &str = "\
.pl{margin:1rem 0 .5rem;padding:.9rem 1rem;border:1px solid var(--line);border-radius:.75rem}\
.pl h2{margin:0 0 .6rem;font-size:1.05rem}\
.pl .map{display:block;width:100%;height:auto;margin:0 0 .4rem;border-radius:.5rem}\
.pl .map{overflow:hidden;--m-land:#f4f2ee;--m-sea:#b3d4e6;--m-green:#d3e8c4;--m-minor:#dcd8cf;\
--m-major:#c9c3b8;--m-hw:#e7b766;--m-rail:#aaa49a;--m-name:#5f6368}\
@media (prefers-color-scheme:dark){.pl .map{--m-land:#2b2c2f;--m-sea:#1d3442;--m-green:#25362b;\
--m-minor:#3e4045;--m-major:#55575d;--m-hw:#8a6a33;--m-rail:#5a5d63;--m-name:#a8adb3}}\
.map .bg{fill:var(--net)}.map .grid{stroke:var(--line);stroke-width:1}\
.map .sea,.map .water{fill:var(--m-sea)}.map .land,.map .earth{fill:var(--m-land)}\
.map .green{fill:var(--m-green)}.map path.river{fill:none;stroke:var(--m-sea);stroke-width:3}\
.map path.rail{fill:none;stroke:var(--m-rail);stroke-width:2;stroke-dasharray:6 4}\
.map path.minor,.map path.major,.map path.highway{fill:none;stroke-linecap:round;stroke-linejoin:round}\
.map .minor{stroke:var(--m-minor);stroke-width:2}.map .major{stroke:var(--m-major);stroke-width:4}\
.map .highway{stroke:var(--m-hw);stroke-width:5}\
.map .nm{fill:var(--m-name);font:11px system-ui,sans-serif;paint-order:stroke;stroke:var(--m-land);\
stroke-width:3px;stroke-linejoin:round}.map .nm.tn{font-weight:600}\
.map .pin{fill:var(--accent);stroke:var(--bg);stroke-width:1.5}\
.map .pn{fill:var(--bg);font:600 12px system-ui,sans-serif}\
.map .ctr{fill:none;stroke:var(--fg);stroke-width:2}\
.map .lbl{fill:var(--fg);font:12px system-ui,sans-serif;paint-order:stroke;stroke:var(--bg);\
stroke-width:3px;stroke-linejoin:round}\
.map .bar{stroke:var(--fg);stroke-width:2}\
.pl ol{margin:0}.pl li{display:flex;gap:.6rem;padding:.45rem 0;margin:0}\
.pl .no{flex:none;display:grid;place-items:center;width:1.5rem;height:1.5rem;border-radius:50%;\
background:var(--accent);color:var(--bg);font-size:.8rem;font-weight:600}\
.pl .pt{min-width:0;line-height:1.35;overflow-wrap:anywhere}\
.pl .pt a{color:var(--link)}.pl .pt>a{font-weight:600}\
.pl .pa{font-size:.85rem;color:var(--muted)}\
.pl .ic{display:inline-grid;width:1.1rem;height:1.1rem;vertical-align:-.2rem;margin-right:.25rem}\
.pl .ic img{width:12px;height:12px}\
.plf{margin:1rem 0 .5rem}.plf>summary{cursor:pointer;color:var(--muted);font-size:.95rem}\
.plf>summary .m{font-size:.85rem}.plf[open] .pl{margin-top:.5rem}";

/// Whether distances are in miles for a searcher in `country`.
fn in_miles(country: Option<&str>) -> bool {
    country.is_some_and(|c| MILES.contains(&c))
}

/// `km` as the searcher measures: "0.4 mi", "1.2 km", "12 km".
fn distance_words(km: f64, miles: bool) -> String {
    let (n, unit) = if miles {
        (km / 1.609_344, "mi")
    } else {
        (km, "km")
    };
    if n < 10.0 {
        format!("{n:.1} {unit}")
    } else {
        format!("{n:.0} {unit}")
    }
}

/// "Denver, CO", "Paris, FR": a place's name with its region or country.
pub(super) fn place_name(place: &Place) -> String {
    let mut name = place.name.clone();
    if let Some(town) = place.town.as_deref().filter(|t| *t != place.name) {
        let _ = write!(name, ", {town}");
    } else if let Some(region) = place.region.as_deref().filter(|r| r.len() <= 3) {
        let _ = write!(name, ", {region}");
    } else if let Some(country) = &place.country {
        let _ = write!(name, ", {country}");
    }
    name
}

/// What `query` looked for, capitalized: "Pizza".
fn capitalized(what: &str) -> String {
    let mut chars = what.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The places part of a results page, its map drawn on `base_map` when
/// there is one. `about` is whether this node has an
/// About page where the searcher can give their town; `country` the
/// searcher's country, for miles or km. `link` gives the address a place's
/// link goes to, from the place's own (a `/go` link that notes the box was
/// used, or the address itself).
pub(super) fn render_places(
    found: &PlaceResults,
    base_map: Option<&crate::map::BaseMap>,
    about: bool,
    country: Option<&str>,
    icons: &Icons,
    link: &dyn Fn(&str) -> String,
) -> String {
    let what = escape_html(&found.what);
    let Some(center) = &found.center else {
        // "Near me", and no town to go by.
        let how = if about {
            "To list places near you, tell Plumb your town on \
             <a href=\"/about\">About you</a>. It never works out where you are by itself."
                .to_string()
        } else {
            format!(
                "Plumb does not know where you are. Search with your town, such as \
                 <strong>{what} in Denver</strong>."
            )
        };
        return format!(
            "<section class=\"pl\" aria-label=\"Places\"><p class=\"m\">{how}</p></section>\n"
        );
    };
    let miles = in_miles(country);
    let mut out = String::from("<section class=\"pl\" aria-label=\"Places\">\n");
    let _ = writeln!(
        out,
        "<h2>{} {} {}</h2>",
        escape_html(&capitalized(&found.what)),
        if center.is_town() { "in" } else { "near" },
        escape_html(&place_name(center)),
    );
    if found.hits.is_empty() {
        let _ = writeln!(
            out,
            "<p class=\"m\">No places found for <strong>{what}</strong> within {}.</p>",
            distance_words(found.radius_km, miles)
        );
    } else {
        out.push_str(&render_map(center, &found.hits, miles, base_map));
        out.push_str("<ol>\n");
        for (n, hit) in found.hits.iter().enumerate() {
            render_place(&mut out, n + 1, hit, miles, icons, link);
        }
        out.push_str("</ol>\n");
    }
    let _ = writeln!(
        out,
        "<p class=\"m\">Places &copy; <a href=\"{OSM_COPYRIGHT_URL}\" rel=\"noreferrer\">\
         OpenStreetMap contributors</a>, ODbL. <a href=\"{}\" rel=\"noreferrer\">Open this area \
         in OpenStreetMap</a></p>\n</section>",
        escape_html(&area_url(center, found.radius_km)),
    );
    out
}

/// The places part folded behind a one-line summary, for a searcher who
/// seldom opens places for searches like this one (see [`crate::learn`]).
pub(super) fn fold_places(found: &PlaceResults, html: &str, chosen: bool) -> String {
    let where_ = found
        .center
        .as_ref()
        .map(|center| format!(" near {}", escape_html(&place_name(center))))
        .unwrap_or_default();
    format!(
        "<details class=\"plf\"><summary>{}{where_} <span class=\"m\">{}</span></summary>\n\
         {html}</details>\n",
        escape_html(&capitalized(&found.what)),
        if chosen {
            "folded, as you asked"
        } else {
            "folded: you seldom open places for searches like this"
        },
    )
}

/// The addresses a place's links go to: its website, if any, and its
/// OpenStreetMap page.
pub(super) fn place_links(hit: &PlaceHit) -> Vec<String> {
    let place = &hit.place;
    let mut links = Vec::new();
    if let Some(website) = place.website.as_deref().and_then(http_url) {
        links.push(website);
    }
    links.push(place.osm_url());
    links
}

/// One place of the list.
fn render_place(
    out: &mut String,
    n: usize,
    hit: &PlaceHit,
    miles: bool,
    icons: &Icons,
    link: &dyn Fn(&str) -> String,
) {
    let place = &hit.place;
    let osm_url = place.osm_url();
    let osm = escape_html(&link(&osm_url));
    let website = place.website.as_deref().and_then(http_url);
    let name = escape_html(&place.name);
    let site_link = website.as_deref().map(|w| escape_html(&link(w)));
    let link = site_link.clone().unwrap_or_else(|| osm.clone());
    let _ = write!(
        out,
        "<li><span class=\"no\" aria-hidden=\"true\">{n}</span><div class=\"pt\">\
         <a href=\"{link}\" rel=\"noreferrer\">{name}</a> <span class=\"tag\">{} &middot; {}</span>\
         <div class=\"pa\">",
        escape_html(&place.label()),
        distance_words(hit.km, miles),
    );
    let mut parts = Vec::new();
    if let Some(address) = &place.address {
        parts.push(escape_html(address));
    }
    if let Some(site) = website.as_deref().and_then(|w| url::Url::parse(w).ok()) {
        if let Some(host) = site.host_str() {
            let host = host.strip_prefix("www.").unwrap_or(host);
            let domain = plumb_core::registrable_domain(host).unwrap_or_else(|| host.to_string());
            // An icon the node has means a site it knows.
            let icon = icons
                .get(&domain)
                .map(|src| {
                    format!(
                        "<span class=\"ic\"><img src=\"{}\" alt=\"\"></span>",
                        escape_html(src)
                    )
                })
                .unwrap_or_default();
            parts.push(format!(
                "{icon}<a href=\"{}\" rel=\"noreferrer\">{}</a>",
                site_link.as_deref().unwrap_or_default(),
                escape_html(host)
            ));
        }
    }
    parts.push(format!("<a href=\"{osm}\" rel=\"noreferrer\">Map</a>"));
    out.push_str(&parts.join(" &middot; "));
    out.push_str("</div></div></li>\n");
}

/// The domains of the places' websites, for their icons.
pub(super) fn website_domains(found: &PlaceResults) -> Vec<String> {
    found
        .hits
        .iter()
        .filter_map(|hit| hit.place.website.as_deref())
        .filter_map(|w| url::Url::parse(w).ok())
        .filter_map(|u| {
            let host = u.host_str()?.to_string();
            plumb_core::registrable_domain(&host).or(Some(host))
        })
        .collect()
}

/// The places' own websites as results, each site once, in the order the
/// places are listed: titled with the place's name, described by its kind
/// and address.
pub(super) fn local_sites(found: &PlaceResults) -> Vec<Hit> {
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
            country: None,
            named: false,
            official: false,
            key_pages: Vec::new(),
            demand: None,
            missing_words: false,
        });
    }
    sites
}

/// The sites for a query that lists places around a town ("brewery in
/// denver"): the places' own sites (`local`, from [`local_sites`])
/// first, then the sites that say what was looked for, then the rest;
/// sites named after the town (the city's own site, its football team)
/// are left out when places have sites to show. At most `limit`, or as
/// many as there were.
pub(super) fn local_first(
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
fn word_root(word: &str) -> Option<String> {
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

/// openstreetmap.org around `center`, zoomed to show `km` around it.
fn area_url(center: &Place, km: f64) -> String {
    let zoom = match km {
        k if k >= 10.0 => 12,
        k if k >= 5.0 => 13,
        k if k >= 2.0 => 14,
        _ => 15,
    };
    format!(
        "https://www.openstreetmap.org/?mlat={lat:.5}&mlon={lon:.5}#map={zoom}/{lat:.5}/{lon:.5}",
        lat = center.lat,
        lon = center.lon,
    )
}

/// A map of the places as numbered pins around the centre, with a scale
/// bar, on the streets of `base_map` when it has them there.
fn render_map(
    center: &Place,
    hits: &[PlaceHit],
    miles: bool,
    base_map: Option<&crate::map::BaseMap>,
) -> String {
    // Kilometres east and north of the centre.
    let km_per_lon = 111.32 * center.lat.to_radians().cos().max(0.01);
    let at = |lat: f64, lon: f64| {
        let mut dlon = lon - center.lon;
        if dlon > 180.0 {
            dlon -= 360.0;
        } else if dlon < -180.0 {
            dlon += 360.0;
        }
        (dlon * km_per_lon, (lat - center.lat) * 110.57)
    };
    let points: Vec<(f64, f64)> = hits.iter().map(|h| at(h.place.lat, h.place.lon)).collect();
    let (mut west, mut east, mut south, mut north) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for (x, y) in &points {
        west = west.min(*x);
        east = east.max(*x);
        south = south.min(*y);
        north = north.max(*y);
    }
    // At least half a kilometre across, so one pin is not a whole map.
    let span_x = (east - west).max(0.5);
    let span_y = (north - south).max(0.5);
    let scale = ((MAP_WIDTH - 2.0 * MAP_PAD) / span_x).min((MAP_HEIGHT - 2.0 * MAP_PAD) / span_y);
    let mid_x = (west + east) / 2.0;
    let mid_y = (south + north) / 2.0;
    let to_svg = |(x, y): (f64, f64)| {
        (
            MAP_WIDTH / 2.0 + (x - mid_x) * scale,
            MAP_HEIGHT / 2.0 - (y - mid_y) * scale,
        )
    };
    let mut svg = format!(
        "<svg class=\"map\" viewBox=\"0 0 {MAP_WIDTH} {MAP_HEIGHT}\" role=\"img\" \
         aria-label=\"Map of the places listed, numbered as in the list\">"
    );
    let pins: Vec<(f64, f64)> = points.iter().map(|p| to_svg(*p)).collect();
    let streets = base_map.and_then(|map| {
        // The map's corners, back from SVG units to degrees.
        let lat_at = |sy: f64| center.lat + (mid_y - (sy - MAP_HEIGHT / 2.0) / scale) / 110.57;
        let lon_at = |sx: f64| center.lon + (mid_x + (sx - MAP_WIDTH / 2.0) / scale) / km_per_lon;
        let place_to_svg = |lat: f64, lon: f64| to_svg(at(lat, lon));
        let mut keep_clear = pins.clone();
        keep_clear.push(to_svg((0.0, 0.0)));
        map.draw(&crate::map::draw::Frame {
            bounds: (
                lat_at(MAP_HEIGHT),
                lon_at(0.0),
                lat_at(0.0),
                lon_at(MAP_WIDTH),
            ),
            width: MAP_WIDTH,
            height: MAP_HEIGHT,
            metres_per_unit: 1000.0 / scale,
            to_svg: &place_to_svg,
            keep_clear: &keep_clear,
            skip: &center.name,
        })
    });
    match streets {
        Some(streets) => svg.push_str(&streets),
        None => {
            let _ = write!(
                svg,
                "<rect class=\"bg\" width=\"{MAP_WIDTH}\" height=\"{MAP_HEIGHT}\" rx=\"8\"/>"
            );
        }
    }
    // A scale bar of a round distance, about a fifth of the map across.
    let unit_km = if miles { 1.609_344 } else { 1.0 };
    let want = MAP_WIDTH / 5.0 / scale / unit_km;
    let step = [0.1, 0.2, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0]
        .into_iter()
        .rev()
        .find(|s| *s <= want)
        .unwrap_or(0.1);
    let bar = step * unit_km * scale;
    let _ = write!(
        svg,
        "<line class=\"bar\" x1=\"12\" y1=\"{y:.1}\" x2=\"{x2:.1}\" y2=\"{y:.1}\"/>\
         <text class=\"lbl\" x=\"12\" y=\"{ty:.1}\">{step} {}</text>",
        if miles { "mi" } else { "km" },
        y = MAP_HEIGHT - 10.0,
        x2 = 12.0 + bar,
        ty = MAP_HEIGHT - 16.0,
    );
    // Where the search looked from, when it is on the map.
    let (cx, cy) = to_svg((0.0, 0.0));
    if (0.0..=MAP_WIDTH).contains(&cx) && (0.0..=MAP_HEIGHT).contains(&cy) {
        let _ = write!(
            svg,
            "<circle class=\"ctr\" cx=\"{cx:.1}\" cy=\"{cy:.1}\" r=\"6\"/>\
             <text class=\"lbl\" x=\"{:.1}\" y=\"{:.1}\">{}</text>",
            cx + 9.0,
            cy - 8.0,
            escape_html(&center.name)
        );
    }
    // The farthest first, so the nearest are drawn on top.
    for (n, &(x, y)) in pins.iter().enumerate().rev() {
        let _ = write!(
            svg,
            "<circle class=\"pin\" cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"11\"/>\
             <text class=\"pn\" x=\"{x:.1}\" y=\"{:.1}\" text-anchor=\"middle\">{}</text>",
            y + 4.0,
            n + 1
        );
    }
    svg.push_str("</svg>\n");
    svg
}

#[cfg(test)]
mod tests {
    #[test]
    fn words_match_their_other_forms() {
        assert_eq!(word_root("brewery").as_deref(), Some("brewer"));
        assert_eq!(word_root("breweries").as_deref(), Some("brewer"));
        assert_eq!(word_root("hotels").as_deref(), Some("hotel"));
        assert_eq!(word_root("pizza").as_deref(), Some("pizza"));
        assert_eq!(word_root("bars").as_deref(), Some("bar"));
        assert_eq!(word_root("ny"), None);
    }

    use super::*;

    fn place(name: &str, kind: &str, lat: f64, lon: f64) -> Place {
        Place {
            name: name.into(),
            kind: kind.into(),
            lat,
            lon,
            osm: "n42".into(),
            region: Some("CO".into()),
            country: Some("US".into()),
            ..Place::default()
        }
    }

    fn same(href: &str) -> String {
        href.to_owned()
    }

    #[test]
    fn town_sites_go_last_when_the_places_have_no_sites() {
        let site = |domain: &str, title: &str| Hit {
            domain: domain.into(),
            url: format!("https://{domain}/"),
            title: Some(title.into()),
            description: None,
            score: 1.0,
            text_score: 1.0,
            link_score: 0.5,
            placing_text_score: None,
            country: None,
            named: false,
            official: false,
            key_pages: Vec::new(),
            demand: None,
            missing_words: false,
        };
        let mut hits = vec![
            site("denvergov.org", "City and County of Denver"),
            site("slicelife.com", "Order food online"),
        ];
        local_first(&found(), &mut hits, Vec::new(), 10);
        let domains: Vec<&str> = hits.iter().map(|h| h.domain.as_str()).collect();
        assert_eq!(domains, ["slicelife.com", "denvergov.org"]);
    }

    #[test]
    fn place_links_can_go_through_go_and_fold() {
        let found = found();
        let html = render_places(&found, None, true, Some("US"), &Icons::default(), &|href| {
            format!("/go?u={href}")
        });
        assert!(
            html.contains("href=\"/go?u=https://www.bluepan.com/menu\""),
            "{html}"
        );
        assert!(
            html.contains("href=\"/go?u=https://www.openstreetmap.org/"),
            "{html}"
        );
        assert_eq!(
            place_links(&found.hits[0])[0],
            "https://www.bluepan.com/menu"
        );
        let folded = fold_places(&found, &html, false);
        assert!(
            folded.starts_with("<details class=\"plf\"><summary>Pizza near Denver"),
            "{folded}"
        );
        assert!(!folded.contains("<details class=\"plf\" open"));
    }

    fn found() -> PlaceResults {
        PlaceResults {
            what: "pizza".into(),
            center: Some(place("Denver", "place=city", 39.7392, -104.9903)),
            near_me: false,
            guessed: false,
            radius_km: 12.0,
            hits: vec![
                PlaceHit {
                    place: Place {
                        tags: vec!["cuisine=pizza".into()],
                        address: Some("1 Main <St>".into()),
                        website: Some("https://www.bluepan.com/menu".into()),
                        ..place("Blue <Pan>", "amenity=restaurant", 39.75, -104.98)
                    },
                    km: 1.4,
                },
                PlaceHit {
                    place: place("Pizza Hut", "amenity=fast_food", 39.70, -104.95),
                    km: 5.3,
                },
            ],
        }
    }

    #[test]
    fn the_map_is_drawn_on_streets_when_the_node_has_them() {
        let dir = tempfile::tempdir().unwrap();
        let map = crate::map::BaseMap::open(&crate::map::tests::sample_map(dir.path())).unwrap();
        let html = render_places(
            &found(),
            Some(&map),
            true,
            Some("US"),
            &Icons::default(),
            &same,
        );
        let svg = &html[html.find("<svg").unwrap()..html.find("</svg>").unwrap()];
        assert!(svg.contains("<rect class=\"sea\""), "{svg}");
        assert!(!svg.contains("class=\"bg\""), "{svg}");
        // The pins are still on top.
        assert!(svg.rfind("class=\"pin\"").unwrap() > svg.find("<path").unwrap());
    }

    #[test]
    fn places_are_listed_with_a_map_and_credit() {
        let html = render_places(&found(), None, true, Some("US"), &Icons::default(), &same);
        assert!(html.contains("<h2>Pizza in Denver, CO</h2>"));
        assert!(html.contains("Blue &lt;Pan&gt;"));
        assert!(html.contains("1 Main &lt;St&gt;"));
        assert!(html.contains("Pizza restaurant &middot; 0.9 mi"));
        assert!(html.contains("href=\"https://www.bluepan.com/menu\""));
        assert!(html.contains(">bluepan.com</a>"));
        assert!(html.contains("https://www.openstreetmap.org/node/42"));
        assert!(html.contains(OSM_COPYRIGHT_URL));
        assert!(html.contains("<svg class=\"map\""));
        // Nothing on the map comes from anywhere else.
        let map = &html[html.find("<svg").unwrap()..html.find("</svg>").unwrap()];
        assert!(!map.contains("http"));
        assert_eq!(map.matches("class=\"pin\"").count(), 2);
        // Kilometres elsewhere.
        let html = render_places(&found(), None, true, Some("DE"), &Icons::default(), &same);
        assert!(html.contains("1.4 km"));
    }

    #[test]
    fn near_me_without_a_town_says_how_to_give_one() {
        let mut found = found();
        found.center = None;
        found.near_me = true;
        found.hits.clear();
        let html = render_places(&found, None, true, None, &Icons::default(), &same);
        assert!(html.contains("href=\"/about\""));
        let html = render_places(&found, None, false, None, &Icons::default(), &same);
        assert!(html.contains("pizza in Denver"));
    }

    /// Finds pizza in Denver and nothing else.
    struct PizzaBackend;

    impl crate::web::SearchBackend for PizzaBackend {
        fn search(&self, _query: &str, _limit: usize) -> anyhow::Result<Vec<plumb_index::Hit>> {
            Ok(Vec::new())
        }

        fn num_docs(&self) -> u64 {
            1
        }

        fn places(
            &self,
            query: &str,
            home: Option<&str>,
            _country: Option<&str>,
        ) -> Option<PlaceResults> {
            assert_eq!(
                home, None,
                "a visitor's town comes only from their About page"
            );
            (query == "pizza in denver").then(found)
        }
    }

    #[tokio::test]
    async fn the_results_page_lists_places_above_the_sites() {
        use tower::ServiceExt;
        let app = crate::web::router(std::sync::Arc::new(PizzaBackend));
        let request = axum::http::Request::builder()
            .uri("/search?q=pizza+in+denver")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        let places = body
            .find("<section class=\"pl\"")
            .expect("places are listed");
        // The place's own site is listed among the sites, below.
        assert!(!body.contains("No sites match"), "{body}");
        assert!(places < body.rfind("https://www.bluepan.com/menu").unwrap());
        assert!(body.contains("<h2>Pizza in Denver, CO</h2>"));
        let request = axum::http::Request::builder()
            .uri("/api/search?q=pizza+in+denver&full=1")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["places"]["hits"][0]["place"]["name"], "Blue <Pan>");
        assert_eq!(json["hits"][0]["domain"], "bluepan.com");
        assert_eq!(json["hits"][0]["title"], "Blue <Pan>");
    }

    #[test]
    fn one_place_still_makes_a_map() {
        let mut found = found();
        found.hits.truncate(1);
        let html = render_places(&found, None, false, None, &Icons::default(), &same);
        assert!(html.contains("class=\"pin\""));
        assert!(!html.contains("NaN") && !html.contains("inf"));
    }
}
