//! Mapbox vector tiles: reading them, and "slimming" a Protomaps basemap
//! tile to what Plumb's small maps draw (land, water, parks, roads and a
//! few town and neighbourhood names), which is a fraction of its bytes.
//! See <https://github.com/mapbox/vector-tile-spec/tree/master/2.1>.

use anyhow::{bail, Context, Result};

use super::pmtiles::write_varint;

/// What a feature is drawn as. A slim tile names its layers after these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    Earth,
    Green,
    Water,
    River,
    Rail,
    Minor,
    Major,
    Highway,
    /// A town's or city's name.
    Town,
    /// A neighbourhood's name.
    Hood,
}

impl Class {
    pub const ALL: [Class; 10] = [
        Class::Earth,
        Class::Green,
        Class::Water,
        Class::River,
        Class::Rail,
        Class::Minor,
        Class::Major,
        Class::Highway,
        Class::Town,
        Class::Hood,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Class::Earth => "earth",
            Class::Green => "green",
            Class::Water => "water",
            Class::River => "river",
            Class::Rail => "rail",
            Class::Minor => "minor",
            Class::Major => "major",
            Class::Highway => "highway",
            Class::Town => "town",
            Class::Hood => "hood",
        }
    }

    /// The layer of a slim tile.
    fn of_slim(layer: &str) -> Option<Class> {
        Class::ALL.into_iter().find(|c| c.name() == layer)
    }

    pub fn is_label(self) -> bool {
        matches!(self, Class::Town | Class::Hood)
    }
}

/// Kinds of land drawn green.
const GREEN: &[&str] = &[
    "park",
    "forest",
    "wood",
    "grass",
    "garden",
    "nature_reserve",
    "national_park",
    "protected_area",
    "golf_course",
    "cemetery",
    "recreation_ground",
    "meadow",
    "scrub",
    "playground",
    "dog_park",
    "pitch",
    "zoo",
    "village_green",
    "allotments",
];

/// How a feature of `layer`, of geometry `geom` (1 point, 2 line, 3
/// polygon), is drawn, from its `kind` (Protomaps basemap v4, or
/// `pmap:kind` of earlier versions), `kind_detail` and whether it is in a
/// tunnel; `None` when it is not drawn. A slim tile's layers are already
/// classes.
pub fn classify(
    layer: &str,
    geom: u32,
    kind: Option<&str>,
    detail: Option<&str>,
    tunnel: bool,
) -> Option<Class> {
    if let Some(class) = Class::of_slim(layer) {
        return Some(class);
    }
    let kind = kind.unwrap_or("");
    match (layer, geom) {
        ("earth", 3) => Some(Class::Earth),
        ("water", 3) => Some(Class::Water),
        ("water" | "physical_line", 2) if !tunnel => Some(Class::River),
        ("landuse" | "landcover" | "natural" | "land", 3) if GREEN.contains(&kind) => {
            Some(Class::Green)
        }
        ("roads", 2) if !tunnel => match kind {
            _ if matches!(detail, Some("pedestrian" | "living_street")) => Some(Class::Minor),
            // Driveways and car park lanes would crowd a small map.
            "minor_road" if detail == Some("service") => None,
            "highway" => Some(Class::Highway),
            "major_road" | "medium_road" => Some(Class::Major),
            "minor_road" => Some(Class::Minor),
            "rail" if detail.is_none_or(|d| d == "rail") => Some(Class::Rail),
            _ => None,
        },
        ("places", 1) => match kind {
            "locality" | "city" => Some(Class::Town),
            "neighbourhood" | "macrohood" => Some(Class::Hood),
            _ => None,
        },
        _ => None,
    }
}

/// A property value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value<'a> {
    Str(&'a str),
    Num(f64),
    Bool(bool),
}

impl Value<'_> {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_num(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Feature {
    /// 1 point, 2 line, 3 polygon.
    pub geom: u32,
    pub tags: Vec<u32>,
    pub geometry: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct Layer<'a> {
    pub name: &'a str,
    pub extent: u32,
    pub keys: Vec<&'a str>,
    pub values: Vec<Value<'a>>,
    pub features: Vec<Feature>,
}

impl Layer<'_> {
    /// Feature `f`'s value for `key`.
    pub fn prop(&self, f: &Feature, key: &str) -> Option<&Value<'_>> {
        f.tags.as_chunks::<2>().0.iter().find_map(|kv| {
            (self.keys.get(kv[0] as usize) == Some(&key))
                .then(|| self.values.get(kv[1] as usize))
                .flatten()
        })
    }

    /// How feature `f` is drawn, if it is.
    pub fn class(&self, f: &Feature) -> Option<Class> {
        let detail = self
            .prop(f, "kind_detail")
            .or_else(|| self.prop(f, "highway"))
            .and_then(Value::as_str);
        let tunnel = matches!(self.prop(f, "is_tunnel"), Some(Value::Bool(true)))
            || self
                .prop(f, "tunnel")
                .is_some_and(|v| v.as_str().is_some_and(|s| s != "no") || v == &Value::Bool(true));
        classify(self.name, f.geom, self.kind(f), detail, tunnel)
    }

    /// Feature `f`'s kind.
    pub fn kind(&self, f: &Feature) -> Option<&str> {
        self.prop(f, "kind")
            .or_else(|| self.prop(f, "pmap:kind"))
            .and_then(Value::as_str)
    }
}

/// Protocol buffer reading, just enough for vector tiles.
struct Pb<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Pb<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Pb { bytes, at: 0 }
    }

    fn done(&self) -> bool {
        self.at >= self.bytes.len()
    }

    fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.bytes.get(self.at).context("tile ends early")?;
            self.at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Ok(value);
            }
        }
        bail!("varint too long")
    }

    /// The next field's number and wire type.
    fn key(&mut self) -> Result<(u64, u64)> {
        let key = self.varint()?;
        Ok((key >> 3, key & 7))
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.varint()? as usize;
        let end = self.at.checked_add(len).context("length overflows")?;
        let out = self.bytes.get(self.at..end).context("tile ends early")?;
        self.at = end;
        Ok(out)
    }

    fn fixed(&mut self, n: usize) -> Result<&'a [u8]> {
        let out = self
            .bytes
            .get(self.at..self.at + n)
            .context("tile ends early")?;
        self.at += n;
        Ok(out)
    }

    fn skip(&mut self, wire: u64) -> Result<()> {
        match wire {
            0 => {
                self.varint()?;
            }
            1 => {
                self.fixed(8)?;
            }
            2 => {
                self.bytes()?;
            }
            5 => {
                self.fixed(4)?;
            }
            other => bail!("wire type {other}"),
        }
        Ok(())
    }

    /// A repeated uint32 field, packed or not.
    fn uints(&mut self, wire: u64, out: &mut Vec<u32>) -> Result<()> {
        if wire == 2 {
            let mut inner = Pb::new(self.bytes()?);
            while !inner.done() {
                out.push(inner.varint()? as u32);
            }
        } else {
            out.push(self.varint()? as u32);
        }
        Ok(())
    }
}

/// The layers of a (decompressed) tile.
pub fn decode(tile: &[u8]) -> Result<Vec<Layer<'_>>> {
    let mut pb = Pb::new(tile);
    let mut layers = Vec::new();
    while !pb.done() {
        match pb.key()? {
            (3, 2) => layers.push(decode_layer(pb.bytes()?)?),
            (_, wire) => pb.skip(wire)?,
        }
    }
    Ok(layers)
}

fn decode_layer(bytes: &[u8]) -> Result<Layer<'_>> {
    let mut pb = Pb::new(bytes);
    let mut layer = Layer {
        name: "",
        extent: 4096,
        keys: Vec::new(),
        values: Vec::new(),
        features: Vec::new(),
    };
    while !pb.done() {
        match pb.key()? {
            (1, 2) => layer.name = std::str::from_utf8(pb.bytes()?).unwrap_or(""),
            (2, 2) => layer.features.push(decode_feature(pb.bytes()?)?),
            (3, 2) => layer
                .keys
                .push(std::str::from_utf8(pb.bytes()?).unwrap_or("")),
            (4, 2) => layer.values.push(decode_value(pb.bytes()?)?),
            (5, 0) => layer.extent = (pb.varint()? as u32).max(1),
            (_, wire) => pb.skip(wire)?,
        }
    }
    Ok(layer)
}

fn decode_feature(bytes: &[u8]) -> Result<Feature> {
    let mut pb = Pb::new(bytes);
    let mut feature = Feature::default();
    while !pb.done() {
        match pb.key()? {
            (2, wire) => pb.uints(wire, &mut feature.tags)?,
            (3, 0) => feature.geom = pb.varint()? as u32,
            (4, wire) => pb.uints(wire, &mut feature.geometry)?,
            (_, wire) => pb.skip(wire)?,
        }
    }
    Ok(feature)
}

fn decode_value(bytes: &[u8]) -> Result<Value<'_>> {
    let mut pb = Pb::new(bytes);
    let mut value = Value::Bool(false);
    while !pb.done() {
        value = match pb.key()? {
            (1, 2) => Value::Str(std::str::from_utf8(pb.bytes()?).unwrap_or("")),
            (2, 5) => Value::Num(f64::from(f32::from_le_bytes(
                pb.fixed(4)?.try_into().unwrap(),
            ))),
            (3, 1) => Value::Num(f64::from_le_bytes(pb.fixed(8)?.try_into().unwrap())),
            (4 | 5, 0) => Value::Num(pb.varint()? as i64 as f64),
            (6, 0) => {
                let n = pb.varint()?;
                Value::Num(((n >> 1) as i64 ^ -((n & 1) as i64)) as f64)
            }
            (7, 0) => Value::Bool(pb.varint()? != 0),
            (_, wire) => {
                pb.skip(wire)?;
                continue;
            }
        };
    }
    Ok(value)
}

/// A feature's geometry as parts (a line, or a polygon's ring) of points
/// in tile units.
pub fn parts(geometry: &[u32]) -> Vec<Vec<(i32, i32)>> {
    let mut parts: Vec<Vec<(i32, i32)>> = Vec::new();
    let (mut x, mut y) = (0i32, 0i32);
    let mut at = 0;
    let zigzag = |n: u32| ((n >> 1) as i32) ^ -((n & 1) as i32);
    while at < geometry.len() {
        let command = geometry[at] & 7;
        let count = (geometry[at] >> 3) as usize;
        at += 1;
        match command {
            1 | 2 => {
                for _ in 0..count {
                    let (Some(dx), Some(dy)) = (geometry.get(at), geometry.get(at + 1)) else {
                        return parts;
                    };
                    at += 2;
                    x = x.wrapping_add(zigzag(*dx));
                    y = y.wrapping_add(zigzag(*dy));
                    if command == 1 {
                        parts.push(vec![(x, y)]);
                    } else if let Some(part) = parts.last_mut() {
                        part.push((x, y));
                    }
                }
            }
            7 => {
                if let Some(part) = parts.last_mut() {
                    if let Some(first) = part.first().copied() {
                        part.push(first);
                    }
                }
            }
            _ => return parts,
        }
    }
    parts
}

/// One layer of a tile being written.
#[derive(Default)]
struct OutLayer {
    keys: Vec<String>,
    values: Vec<String>,
    features: Vec<u8>,
}

impl OutLayer {
    fn tag(list: &mut Vec<String>, text: &str) -> u32 {
        match list.iter().position(|t| t == text) {
            Some(n) => n as u32,
            None => {
                list.push(text.to_string());
                (list.len() - 1) as u32
            }
        }
    }

    fn add(&mut self, geom: u32, props: &[(&str, &str)], geometry: &[u32]) {
        let mut tags = Vec::new();
        for (key, value) in props {
            let k = Self::tag(&mut self.keys, key);
            let v = Self::tag(&mut self.values, value);
            write_varint(&mut tags, u64::from(k));
            write_varint(&mut tags, u64::from(v));
        }
        let mut packed = Vec::with_capacity(geometry.len() * 2);
        for n in geometry {
            write_varint(&mut packed, u64::from(*n));
        }
        let mut feature = Vec::new();
        if !tags.is_empty() {
            field(&mut feature, 2, &tags);
        }
        write_varint(&mut feature, 3 << 3);
        write_varint(&mut feature, u64::from(geom));
        field(&mut feature, 4, &packed);
        field(&mut self.features, 2, &feature);
    }
}

/// A length-delimited field.
fn field(out: &mut Vec<u8>, number: u64, bytes: &[u8]) {
    write_varint(out, (number << 3) | 2);
    write_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// `tile` (decompressed) with only what Plumb draws, each layer named after
/// its [`Class`] and labels keeping their name and population rank; empty
/// when nothing is drawn.
pub fn slim(tile: &[u8]) -> Result<Vec<u8>> {
    let layers = decode(tile)?;
    let mut out: Vec<(Class, u32, OutLayer)> = Vec::new();
    for layer in &layers {
        for feature in &layer.features {
            let Some(class) = layer.class(feature) else {
                continue;
            };
            let at = match out
                .iter()
                .position(|(c, e, _)| *c == class && *e == layer.extent)
            {
                Some(at) => at,
                None => {
                    out.push((class, layer.extent, OutLayer::default()));
                    out.len() - 1
                }
            };
            let target = &mut out[at].2;
            if class.is_label() {
                let Some(name) = layer.prop(feature, "name").and_then(Value::as_str) else {
                    continue;
                };
                let rank = layer
                    .prop(feature, "population_rank")
                    .and_then(Value::as_num)
                    .unwrap_or(0.0);
                let rank = format!("{}", rank as i64);
                target.add(1, &[("name", name), ("rank", &rank)], &feature.geometry);
            } else {
                target.add(feature.geom, &[], &feature.geometry);
            }
        }
    }
    out.sort_by_key(|(class, extent, _)| (*class, *extent));
    let mut tile = Vec::new();
    for (class, extent, layer) in out {
        let mut bytes = Vec::new();
        write_varint(&mut bytes, 15 << 3);
        write_varint(&mut bytes, 2);
        field(&mut bytes, 1, class.name().as_bytes());
        bytes.extend_from_slice(&layer.features);
        for key in &layer.keys {
            field(&mut bytes, 3, key.as_bytes());
        }
        for value in &layer.values {
            let mut v = Vec::new();
            field(&mut v, 1, value.as_bytes());
            field(&mut bytes, 4, &v);
        }
        write_varint(&mut bytes, 5 << 3);
        write_varint(&mut bytes, u64::from(extent));
        field(&mut tile, 3, &bytes);
    }
    Ok(tile)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// Geometry type, properties and geometry.
    type SampleFeature<'a> = (u32, Vec<(&'a str, &'a str)>, Vec<u32>);

    /// A tile with a park, a motorway in a tunnel, a residential street, a
    /// building and a town, in the Protomaps v4 shape.
    pub fn sample_tile() -> Vec<u8> {
        let mut tile = Vec::new();
        let layer = |name: &str, feats: Vec<SampleFeature>| {
            let mut l = OutLayer::default();
            for (geom, props, geometry) in feats {
                l.add(geom, &props, &geometry);
            }
            let mut bytes = Vec::new();
            field(&mut bytes, 1, name.as_bytes());
            bytes.extend_from_slice(&l.features);
            for key in &l.keys {
                field(&mut bytes, 3, key.as_bytes());
            }
            for value in &l.values {
                let mut v = Vec::new();
                field(&mut v, 1, value.as_bytes());
                field(&mut bytes, 4, &v);
            }
            bytes
        };
        // MoveTo(1) (10,10), LineTo(3) (+100,0) (0,+100) (-100,0), ClosePath.
        let square = vec![9, 20, 20, 26, 200, 0, 0, 200, 199, 0, 15];
        let line = vec![9, 0, 0, 10, 400, 400];
        let point = vec![9, 100, 100];
        for bytes in [
            layer(
                "earth",
                vec![(
                    3,
                    vec![("kind", "earth")],
                    vec![9, 0, 0, 26, 8192, 0, 0, 8192, 8191, 0, 15],
                )],
            ),
            layer(
                "landuse",
                vec![
                    (3, vec![("kind", "park")], square.clone()),
                    (3, vec![("kind", "industrial")], square),
                ],
            ),
            layer(
                "roads",
                vec![
                    (
                        2,
                        vec![("kind", "highway"), ("is_tunnel", "x")],
                        line.clone(),
                    ),
                    (2, vec![("kind", "minor_road"), ("name", "Elm St")], line),
                ],
            ),
            layer(
                "buildings",
                vec![(3, vec![("kind", "building")], vec![9, 2, 2, 10, 2, 2, 15])],
            ),
            layer(
                "places",
                vec![(1, vec![("kind", "locality"), ("name", "Denver")], point)],
            ),
        ] {
            field(&mut tile, 3, &bytes);
        }
        tile
    }

    #[test]
    fn slimming_keeps_what_is_drawn() {
        let slim = slim(&sample_tile()).unwrap();
        let layers = decode(&slim).unwrap();
        let names: Vec<&str> = layers.iter().map(|l| l.name).collect();
        // The tunnel's "is_tunnel" here is a string, so it is not a tunnel;
        // the building and the industrial area are gone.
        assert_eq!(names, ["earth", "green", "minor", "highway", "town"]);
        let green = &layers[1];
        assert_eq!(green.features.len(), 1);
        assert_eq!(
            parts(&green.features[0].geometry),
            vec![vec![(10, 10), (110, 10), (110, 110), (10, 110), (10, 10)]]
        );
        let town = &layers[4];
        assert_eq!(
            town.prop(&town.features[0], "name"),
            Some(&Value::Str("Denver"))
        );
        assert!(slim.len() < sample_tile().len());
    }

    #[test]
    fn kinds_are_classed() {
        assert_eq!(
            classify("roads", 2, Some("highway"), None, true),
            None,
            "tunnels are not drawn"
        );
        assert_eq!(
            classify("roads", 2, Some("rail"), Some("subway"), false),
            None
        );
        assert_eq!(
            classify("natural", 3, Some("wood"), None, false),
            Some(Class::Green)
        );
        assert_eq!(classify("water", 3, None, None, false), Some(Class::Water));
        assert_eq!(
            classify("roads", 2, Some("other"), Some("pedestrian"), false),
            Some(Class::Minor)
        );
        assert_eq!(
            classify("roads", 2, Some("minor_road"), Some("service"), false),
            None
        );
        assert_eq!(classify("hood", 1, None, None, false), Some(Class::Hood));
    }
}
