//! Draws the base map under the places' pins as SVG: land, water, parks,
//! roads and a few names, from the map tiles the node keeps.

use std::collections::BTreeMap;
use std::f64::consts::PI;
use std::fmt::Write as _;

use anyhow::Result;

use super::mvt::{self, Class, Value};
use super::pmtiles::{Reader, Source};

/// Most tiles drawn for one map.
const MAX_TILES: u64 = 16;
/// Names written on one map, at most.
const MAX_LABELS: usize = 6;
/// Map coordinates are written in halves of an SVG unit, as whole numbers.
const PRECISION: f64 = 2.0;

/// What part of the world a map shows, and how.
pub struct Frame<'a> {
    /// Its corners: south, west, north, east, in degrees.
    pub bounds: (f64, f64, f64, f64),
    /// Its size in SVG units.
    pub width: f64,
    pub height: f64,
    /// Metres an SVG unit stands for.
    pub metres_per_unit: f64,
    /// Where a point (latitude, longitude) goes on it.
    pub to_svg: &'a dyn Fn(f64, f64) -> (f64, f64),
    /// Points names must keep clear of (the pins).
    pub keep_clear: &'a [(f64, f64)],
    /// A name not to write, as the map already says it.
    pub skip: &'a str,
}

/// The tile column and row of a point at zoom `z`, as fractions.
fn tile_xy(lat: f64, lon: f64, z: u8) -> (f64, f64) {
    let n = f64::from(1u32 << z);
    let lat = lat.clamp(-85.05, 85.05).to_radians();
    let x = (lon + 180.0) / 360.0 * n;
    let y = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / PI) / 2.0 * n;
    (x, y)
}

/// The zoom whose tiles have about the detail this map can show: a tile is
/// drawn 512 pixels across in the styles made for these tiles, and an SVG
/// unit shows as about a pixel and a half.
fn zoom_for(frame: &Frame, lat: f64) -> u8 {
    let metres_per_pixel = frame.metres_per_unit / 1.4;
    let z = (78_271.517 * lat.to_radians().cos() / metres_per_pixel).log2();
    z.ceil().clamp(0.0, 16.0) as u8
}

/// A path of one class, as SVG path data in whole half units.
#[derive(Default)]
struct PathData {
    d: String,
}

impl PathData {
    fn add(&mut self, points: &[(f64, f64)], close: bool) {
        let mut last: Option<(i64, i64)> = None;
        let mut moves = 0;
        for &(x, y) in points {
            let at = (
                (x * PRECISION).round() as i64,
                (y * PRECISION).round() as i64,
            );
            match last {
                None => {
                    let _ = write!(self.d, "M{} {}", at.0, at.1);
                }
                Some(prev) if prev == at => continue,
                Some(prev) => {
                    let (dx, dy) = (at.0 - prev.0, at.1 - prev.1);
                    if moves == 0 {
                        self.d.push('l');
                    } else if dx >= 0 {
                        self.d.push(' ');
                    }
                    let _ = write!(self.d, "{dx}");
                    if dy >= 0 {
                        self.d.push(' ');
                    }
                    let _ = write!(self.d, "{dy}");
                    moves += 1;
                }
            }
            last = Some(at);
        }
        if close && moves > 0 {
            self.d.push('z');
        }
    }
}

/// A name that might be written on the map.
struct Label {
    class: Class,
    name: String,
    rank: f64,
    x: f64,
    y: f64,
}

/// The base map of `frame`, as SVG elements to go under the pins, and
/// whether it has land (so the background is sea); `None` when the map
/// file has no tile there.
pub fn draw<S: Source>(reader: &Reader<S>, frame: &Frame) -> Result<Option<String>> {
    let (south, west, north, east) = frame.bounds;
    let mid_lat = (south + north) / 2.0;
    let mid_lon = (west + east) / 2.0;
    let mut z = zoom_for(frame, mid_lat).min(reader.header.max_zoom);
    // The deepest zoom the file has here (it keeps detail near some towns
    // only), with not too many tiles.
    loop {
        let (x0, y0) = tile_xy(north, west, z);
        let (x1, y1) = tile_xy(south, east, z);
        let count = (x1.floor() - x0.floor() + 1.0) * (y1.floor() - y0.floor() + 1.0);
        let (cx, cy) = tile_xy(mid_lat, mid_lon, z);
        let has = reader
            .find(super::pmtiles::tile_id(z, cx as u32, cy as u32))?
            .is_some();
        if (has && count <= MAX_TILES as f64) || z == 0 {
            if !has {
                return Ok(None);
            }
            break;
        }
        z -= 1;
    }
    let n = 1u32 << z;
    let (x0, y0) = tile_xy(north, west, z);
    let (x1, y1) = tile_xy(south, east, z);
    let mut paths: BTreeMap<Class, PathData> = BTreeMap::new();
    let mut labels = Vec::new();
    let pad = 8.0;
    let inside = |pts: &[(f64, f64)]| {
        let (mut minx, mut miny, mut maxx, mut maxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for (x, y) in pts {
            minx = minx.min(*x);
            miny = miny.min(*y);
            maxx = maxx.max(*x);
            maxy = maxy.max(*y);
        }
        maxx >= -pad && minx <= frame.width + pad && maxy >= -pad && miny <= frame.height + pad
    };
    for ty in (y0.floor() as u32)..=(y1.floor() as u32).min(n - 1) {
        for tx in (x0.floor().max(0.0) as u32)..=(x1.floor() as u32).min(n - 1) {
            let Some(tile) = reader.tile(z, tx, ty)? else {
                continue;
            };
            let layers = mvt::decode(&tile)?;
            for layer in &layers {
                let extent = f64::from(layer.extent);
                let to_svg = |(px, py): (i32, i32)| {
                    let x = (f64::from(tx) + f64::from(px) / extent) / f64::from(n);
                    let y = (f64::from(ty) + f64::from(py) / extent) / f64::from(n);
                    let lon = x * 360.0 - 180.0;
                    let lat = (PI * (1.0 - 2.0 * y)).sinh().atan().to_degrees();
                    (frame.to_svg)(lat, lon)
                };
                for feature in &layer.features {
                    let Some(class) = layer.class(feature) else {
                        continue;
                    };
                    let parts = mvt::parts(&feature.geometry);
                    if class.is_label() {
                        let Some(name) = layer.prop(feature, "name").and_then(Value::as_str) else {
                            continue;
                        };
                        let rank = layer
                            .prop(feature, "rank")
                            .or_else(|| layer.prop(feature, "population_rank"))
                            .and_then(|v| v.as_num().or_else(|| v.as_str()?.parse().ok()))
                            .unwrap_or(0.0);
                        if let Some(&point) = parts.first().and_then(|p| p.first()) {
                            let (x, y) = to_svg(point);
                            labels.push(Label {
                                class,
                                name: name.to_string(),
                                rank,
                                x,
                                y,
                            });
                        }
                        continue;
                    }
                    let close = feature.geom == 3;
                    for part in parts {
                        let points: Vec<(f64, f64)> = part.into_iter().map(to_svg).collect();
                        if points.len() > 1 && inside(&points) {
                            paths.entry(class).or_default().add(&points, close);
                        }
                    }
                }
            }
        }
    }
    let land = paths.contains_key(&Class::Earth);
    let mut svg = format!(
        "<rect class=\"{}\" width=\"{}\" height=\"{}\" rx=\"8\"/>\
         <g transform=\"scale({})\">",
        if land { "sea" } else { "land" },
        frame.width,
        frame.height,
        1.0 / PRECISION
    );
    for (class, path) in &paths {
        if !path.d.is_empty() {
            let _ = write!(svg, "<path class=\"{}\" d=\"{}\"/>", class.name(), path.d);
        }
    }
    svg.push_str("</g>");
    svg.push_str(&place_labels(labels, frame));
    Ok(Some(svg))
}

/// The names written on the map: towns before neighbourhoods, bigger
/// first, none over another or over a pin.
fn place_labels(mut labels: Vec<Label>, frame: &Frame) -> String {
    labels.sort_by(|a, b| {
        a.class
            .cmp(&b.class)
            .then(b.rank.total_cmp(&a.rank))
            .then(a.name.cmp(&b.name))
    });
    let mut taken: Vec<(f64, f64, f64, f64)> = frame
        .keep_clear
        .iter()
        .map(|(x, y)| (x - 13.0, y - 13.0, x + 13.0, y + 13.0))
        .collect();
    let mut out = String::new();
    let mut written = 0;
    let mut seen = std::collections::HashSet::new();
    for label in labels {
        if written == MAX_LABELS {
            break;
        }
        if label.name == frame.skip || !seen.insert(label.name.clone()) {
            continue;
        }
        let half = label.name.chars().count() as f64 * 3.1 + 2.0;
        let rect = (label.x - half, label.y - 9.0, label.x + half, label.y + 3.0);
        let fits = rect.0 >= 4.0
            && rect.2 <= frame.width - 4.0
            && rect.1 >= 4.0
            && rect.3 <= frame.height - 24.0;
        let clear = taken
            .iter()
            .all(|t| rect.2 < t.0 || rect.0 > t.2 || rect.3 < t.1 || rect.1 > t.3);
        if !fits || !clear {
            continue;
        }
        taken.push(rect);
        written += 1;
        let _ = write!(
            out,
            "<text class=\"nm{}\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\">{}</text>",
            if label.class == Class::Town {
                " tn"
            } else {
                ""
            },
            label.x,
            label.y,
            crate::web::escape_html(&label.name)
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_relative_and_skip_repeats() {
        let mut path = PathData::default();
        path.add(&[(1.0, 1.0), (1.1, 1.1), (3.0, 1.0), (3.0, 4.0)], true);
        assert_eq!(path.d, "M2 2l4 0 0 6z");
    }

    #[test]
    fn zoom_follows_the_map_scale() {
        let to_svg = |_: f64, _: f64| (0.0, 0.0);
        let frame = |metres| Frame {
            bounds: (0.0, 0.0, 0.0, 0.0),
            width: 480.0,
            height: 260.0,
            metres_per_unit: metres,
            to_svg: &to_svg,
            keep_clear: &[],
            skip: "",
        };
        // About 25 km across a city at 40 degrees north.
        let city = zoom_for(&frame(25_000.0 / 480.0), 40.0);
        assert_eq!(city, 11);
        let street = zoom_for(&frame(1_500.0 / 480.0), 40.0);
        assert!(street >= 14);
    }
}
