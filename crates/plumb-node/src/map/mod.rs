//! The base map under the places' pins: streets, water, parks and town
//! names, drawn by the node as SVG from map tiles it keeps itself, so a
//! search page still loads nothing from any other server and no tile
//! server learns where anyone looks.
//!
//! The tiles are a slim cut of the Protomaps basemap (OpenStreetMap data,
//! ODbL) in one PMTiles file, `DIR/pages/sets/map.pmtiles`, made by
//! `plumb fetch-map`: the whole world at low zooms, and street detail
//! around chosen points. Where the file has no detail the map is drawn
//! from the deepest tiles it has there; with no file the pins are drawn
//! alone, as before. See docs/places.md.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{ensure, Result};
use tracing::{info, warn};

pub mod draw;
pub mod fetch;
pub mod mvt;
pub mod pmtiles;

use pmtiles::{FileSource, Reader};

/// The map file's name in a data directory's set files.
pub const MAP_FILE: &str = "map.pmtiles";

/// The map file of the node with data directory `data_dir`.
pub fn file(data_dir: &Path) -> PathBuf {
    crate::pages::sets_dir(data_dir).join(MAP_FILE)
}

/// An open map file.
pub struct BaseMap {
    reader: Reader<FileSource>,
}

impl BaseMap {
    pub fn open(path: &Path) -> Result<Self> {
        let reader = Reader::new(FileSource::open(path)?)?;
        ensure!(
            reader.header.tile_type == pmtiles::TILE_MVT,
            "{} does not hold vector tiles",
            path.display()
        );
        Ok(BaseMap { reader })
    }

    /// The map of `frame` as SVG elements, or `None` where the file has
    /// nothing.
    pub fn draw(&self, frame: &draw::Frame) -> Option<String> {
        draw::draw(&self.reader, frame).unwrap_or_else(|err| {
            warn!("drawing the map: {err:#}");
            None
        })
    }
}

/// When a map file was last changed, its length, and the map it held.
type Opened = (SystemTime, u64, Option<Arc<BaseMap>>);

/// A map file that is opened when first needed and again when it
/// changes, so a new one from `plumb fetch-map` is used within a search.
pub struct MapFile {
    path: PathBuf,
    open: Mutex<Option<Opened>>,
}

impl MapFile {
    pub fn new(path: PathBuf) -> Self {
        MapFile {
            path,
            open: Mutex::new(None),
        }
    }

    /// The map, if the file is there and reads.
    pub fn get(&self) -> Option<Arc<BaseMap>> {
        let meta = std::fs::metadata(&self.path).ok()?;
        let stamp = (
            meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            meta.len(),
        );
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((modified, len, map)) = open.as_ref() {
            if (*modified, *len) == stamp {
                return map.clone();
            }
        }
        let map = match BaseMap::open(&self.path) {
            Ok(map) => {
                info!("drawing maps from {}", self.path.display());
                Some(Arc::new(map))
            }
            Err(err) => {
                warn!("not drawing maps: {err:#}");
                None
            }
        };
        *open = Some((stamp.0, stamp.1, map.clone()));
        map
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A map file with the sample tile as the whole world and as the tile
    /// of Denver's centre at zoom 12.
    pub(crate) fn sample_map(dir: &Path) -> PathBuf {
        let tile = pmtiles::gzip(&mvt::slim(&mvt::tests::sample_tile()).unwrap());
        let mut writer = pmtiles::Writer::new(&dir.join("data")).unwrap();
        writer.add(0, 1, &tile).unwrap();
        // Denver, 39.74 N 104.99 W, at zoom 12.
        writer
            .add(pmtiles::tile_id(12, 853, 1554), 1, &tile)
            .unwrap();
        let path = dir.join(MAP_FILE);
        writer
            .finish(
                &path,
                pmtiles::Header {
                    tile_type: pmtiles::TILE_MVT,
                    tile_compression: pmtiles::COMPRESSION_GZIP,
                    max_zoom: 12,
                    ..Default::default()
                },
                &serde_json::json!({}),
            )
            .unwrap();
        path
    }

    #[test]
    fn draws_streets_where_the_file_has_them() {
        let dir = tempfile::tempdir().unwrap();
        let file = MapFile::new(sample_map(dir.path()));
        let map = file.get().unwrap();
        // 10 km across the north-west corner of Denver's centre tile,
        // where the sample's street is, in a 480 by 260 frame.
        let lon = 853.0 / 4096.0 * 360.0 - 180.0 + 0.01;
        let lat = (std::f64::consts::PI * (1.0 - 2.0 * 1554.0 / 4096.0))
            .sinh()
            .atan()
            .to_degrees()
            - 0.005;
        let scale = 480.0 / 10.0;
        let to_svg = |la: f64, lo: f64| {
            (
                240.0 + (lo - lon) * 111.32 * lat.to_radians().cos() * scale,
                130.0 - (la - lat) * 110.57 * scale,
            )
        };
        let frame = draw::Frame {
            bounds: (lat - 0.012, lon - 0.06, lat + 0.012, lon + 0.06),
            width: 480.0,
            height: 260.0,
            metres_per_unit: 1000.0 / scale,
            to_svg: &to_svg,
            keep_clear: &[],
            skip: "",
        };
        let svg = map.draw(&frame).unwrap();
        assert!(svg.starts_with("<rect class=\"sea\""), "{svg}");
        assert!(svg.contains("<path class=\"earth\""), "{svg}");
        assert!(svg.contains("<path class=\"minor\""), "{svg}");
        // Somewhere with no tile of its own is drawn from the world tile.
        let far = draw::Frame {
            bounds: (10.0, 10.0, 10.1, 10.1),
            ..frame
        };
        assert!(map.draw(&far).is_some());
        // No file, no map.
        assert!(MapFile::new(dir.path().join("none")).get().is_none());
    }
}
