//! The places part of a results page: "pizza in denver" lists pizza places
//! in Denver above the sites, with a small map drawn here as SVG (no map
//! tiles: the page loads nothing from anywhere else), and links to
//! OpenStreetMap, whose data it is.

use std::fmt::Write as _;

use plumb_core::place::{Place, OSM_COPYRIGHT_URL};
use plumb_index::places::{PlaceHit, PlaceResults};

use super::{escape_html, http_url, Icons};

/// Countries that measure roads in miles.
const MILES: &[&str] = &["US", "GB", "LR", "MM"];
/// The map's size in SVG units.
const MAP_WIDTH: f64 = 640.0;
const MAP_HEIGHT: f64 = 240.0;
/// Room around the pins.
const MAP_PAD: f64 = 22.0;

/// Styles of the places part, added to the page's.
pub(super) const STYLE: &str = "\
.pl{margin:1rem 0 .5rem;padding:.9rem 1rem;border:1px solid var(--line);border-radius:.75rem}\
.pl h2{margin:0 0 .6rem;font-size:1.05rem}\
.pl .map{display:block;width:100%;height:auto;margin:0 0 .4rem;border-radius:.5rem}\
.map .bg{fill:var(--net)}.map .grid{stroke:var(--line);stroke-width:1}\
.map .pin{fill:var(--accent)}.map .pn{fill:var(--bg);font:600 12px system-ui,sans-serif}\
.map .ctr{fill:none;stroke:var(--fg);stroke-width:2}\
.map .lbl{fill:var(--fg);font:12px system-ui,sans-serif}\
.map .bar{stroke:var(--fg);stroke-width:2}\
.pl ol{margin:0}.pl li{display:flex;gap:.6rem;padding:.45rem 0;margin:0}\
.pl .no{flex:none;display:grid;place-items:center;width:1.5rem;height:1.5rem;border-radius:50%;\
background:var(--accent);color:var(--bg);font-size:.8rem;font-weight:600}\
.pl .pt{min-width:0;line-height:1.35;overflow-wrap:anywhere}\
.pl .pt a{color:var(--link)}.pl .pt>a{font-weight:600}\
.pl .pa{font-size:.85rem;color:var(--muted)}\
.pl .ic{display:inline-grid;width:1.1rem;height:1.1rem;vertical-align:-.2rem;margin-right:.25rem}\
.pl .ic img{width:12px;height:12px}";

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

/// The places part of a results page. `about` is whether this node has an
/// About page where the searcher can give their town; `country` the
/// searcher's country, for miles or km.
pub(super) fn render_places(
    found: &PlaceResults,
    about: bool,
    country: Option<&str>,
    icons: &Icons,
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
        out.push_str(&render_map(center, &found.hits, miles));
        out.push_str("<ol>\n");
        for (n, hit) in found.hits.iter().enumerate() {
            render_place(&mut out, n + 1, hit, miles, icons);
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

/// One place of the list.
fn render_place(out: &mut String, n: usize, hit: &PlaceHit, miles: bool, icons: &Icons) {
    let place = &hit.place;
    let osm = escape_html(&place.osm_url());
    let website = place.website.as_deref().and_then(http_url);
    let name = escape_html(&place.name);
    let link = website.as_deref().map_or(osm.clone(), escape_html);
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
                escape_html(website.as_deref().unwrap_or_default()),
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
/// bar. Drawn from coordinates alone: no streets.
fn render_map(center: &Place, hits: &[PlaceHit], miles: bool) -> String {
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
         aria-label=\"Map of the places listed, numbered as in the list\">\
         <rect class=\"bg\" width=\"{MAP_WIDTH}\" height=\"{MAP_HEIGHT}\" rx=\"8\"/>"
    );
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
    for (n, point) in points.iter().enumerate().rev() {
        let (x, y) = to_svg(*point);
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

    fn found() -> PlaceResults {
        PlaceResults {
            what: "pizza".into(),
            center: Some(place("Denver", "place=city", 39.7392, -104.9903)),
            near_me: false,
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
    fn places_are_listed_with_a_map_and_credit() {
        let html = render_places(&found(), true, Some("US"), &Icons::default());
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
        let html = render_places(&found(), true, Some("DE"), &Icons::default());
        assert!(html.contains("1.4 km"));
    }

    #[test]
    fn near_me_without_a_town_says_how_to_give_one() {
        let mut found = found();
        found.center = None;
        found.near_me = true;
        found.hits.clear();
        let html = render_places(&found, true, None, &Icons::default());
        assert!(html.contains("href=\"/about\""));
        let html = render_places(&found, false, None, &Icons::default());
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
        assert!(places < body.find("No sites match").unwrap());
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
    }

    #[test]
    fn one_place_still_makes_a_map() {
        let mut found = found();
        found.hits.truncate(1);
        let html = render_places(&found, false, None, &Icons::default());
        assert!(html.contains("class=\"pin\""));
        assert!(!html.contains("NaN") && !html.contains("inf"));
    }
}
