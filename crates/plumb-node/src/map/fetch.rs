//! `plumb fetch-map`: cuts the map file from a Protomaps basemap, read
//! from a file or over HTTP with range requests (only the tiles kept are
//! downloaded), keeping only what the places' maps draw.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Args;
use tracing::{info, warn};

use super::mvt;
use super::pmtiles::{self, Entry, Header, Reader, Source, Writer};

/// Where Protomaps publishes its daily basemap builds.
const BUILDS: &str = "https://build.protomaps.com";
/// One request reads at most this many bytes.
const MAX_READ: u64 = 32 << 20;
/// Tiles closer than this in the source are read with one request.
const MAX_GAP: u64 = 1 << 20;
/// Slimmed tiles kept by their source offset, for tiles many ids share
/// (open sea, empty land), when they are this small.
const SHARED_TILE: u32 = 4096;

#[derive(Debug, Args)]
pub struct FetchMapArgs {
    /// The Protomaps basemap to cut from: a .pmtiles file or its address.
    /// Without it, yesterday's daily build at build.protomaps.com (see
    /// maps.protomaps.com/builds).
    #[arg(long, value_name = "FILE_OR_URL")]
    pub from: Option<String>,
    /// A node's data directory to put the map in (pages/sets/map.pmtiles),
    /// where the node picks it up at its next search.
    #[arg(long, value_name = "DIR", required_unless_present = "out")]
    pub data: Option<PathBuf>,
    /// Write the map here instead.
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
    /// Keep the whole world up to this zoom (8 shows towns and main
    /// roads; each zoom more is about twice the size).
    #[arg(long, value_name = "ZOOM", default_value_t = 8)]
    pub world_zoom: u8,
    /// Keep tiles up to this zoom around each --near point (14 shows
    /// every street).
    #[arg(long, value_name = "ZOOM", default_value_t = 14)]
    pub zoom: u8,
    /// A point to keep street detail around, as LAT,LON ("39.74,-104.99"
    /// for Denver). Can be given more than once.
    #[arg(long, value_name = "LAT,LON")]
    pub near: Vec<String>,
    /// How far around each --near point, in kilometres.
    #[arg(long, value_name = "KM", default_value_t = 50.0)]
    pub km: f64,
    /// Only list how many tiles and bytes of the source would be read, by
    /// zoom, and stop.
    #[arg(long)]
    pub dry_run: bool,
}

/// A basemap over HTTP, read with range requests.
struct HttpSource {
    url: String,
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
}

impl HttpSource {
    fn new(url: &str) -> Result<Self> {
        Ok(HttpSource {
            url: url.to_string(),
            client: plumb_ingest::download::http_client()?,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
        })
    }

    fn read_once(&self, offset: u64, length: usize) -> Result<Vec<u8>> {
        let end = offset + length as u64 - 1;
        self.runtime.block_on(async {
            let response = self
                .client
                .get(&self.url)
                .header(reqwest::header::RANGE, format!("bytes={offset}-{end}"))
                .send()
                .await?;
            let status = response.status();
            if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
                return Ok(Vec::new());
            }
            if status != reqwest::StatusCode::PARTIAL_CONTENT {
                bail!("{} answered {status} to a range request", self.url);
            }
            Ok(response.bytes().await?.to_vec())
        })
    }
}

impl Source for HttpSource {
    fn read(&self, offset: u64, length: usize) -> Result<Vec<u8>> {
        let mut wait = Duration::from_secs(2);
        let mut tries = 0;
        loop {
            match self.read_once(offset, length) {
                Ok(bytes) => return Ok(bytes),
                Err(err) if tries < 4 && !err.to_string().contains(" 404 ") => {
                    warn!("reading {}: {err:#}; trying again", self.url);
                    std::thread::sleep(wait);
                    wait *= 2;
                    tries += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

/// A file or an address, read the same way.
enum AnySource {
    File(pmtiles::FileSource),
    Http(HttpSource),
}

impl Source for AnySource {
    fn read(&self, offset: u64, length: usize) -> Result<Vec<u8>> {
        match self {
            AnySource::File(f) => f.read(offset, length),
            AnySource::Http(h) => h.read(offset, length),
        }
    }
}

/// Opens `from`, or the latest daily build when it is `None`.
fn open_source(from: Option<&str>) -> Result<(String, Reader<AnySource>)> {
    let candidates: Vec<String> = match from {
        Some(from) => vec![from.to_string()],
        None => (1..=3)
            .map(|days| {
                let day = chrono::Utc::now() - chrono::Duration::days(days);
                format!("{BUILDS}/{}.pmtiles", day.format("%Y%m%d"))
            })
            .collect(),
    };
    let mut last = None;
    for candidate in candidates {
        let source = if candidate.starts_with("http://") || candidate.starts_with("https://") {
            AnySource::Http(HttpSource::new(&candidate)?)
        } else {
            AnySource::File(pmtiles::FileSource::open(Path::new(&candidate))?)
        };
        match Reader::new(source) {
            Ok(reader) => return Ok((candidate, reader)),
            Err(err) => {
                warn!("{candidate}: {err:#}");
                last = Some(err.context(candidate));
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no basemap to read")))
}

/// `LAT,LON`.
fn parse_point(text: &str) -> Result<(f64, f64)> {
    let (lat, lon) = text
        .split_once(',')
        .with_context(|| format!("{text:?} is not LAT,LON"))?;
    let lat: f64 = lat
        .trim()
        .parse()
        .with_context(|| format!("latitude in {text:?}"))?;
    let lon: f64 = lon
        .trim()
        .parse()
        .with_context(|| format!("longitude in {text:?}"))?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        bail!("{text:?} is not on Earth");
    }
    Ok((lat, lon))
}

/// The tile ids to keep, as sorted half-open ranges: every tile up to
/// `world_zoom`, and those within `km` of each point up to `zoom`.
pub fn wanted_ranges(world_zoom: u8, zoom: u8, near: &[(f64, f64)], km: f64) -> Vec<(u64, u64)> {
    let mut ids = Vec::new();
    for z in world_zoom.saturating_add(1)..=zoom {
        let n = 1u32 << z;
        let tile = |lat: f64, lon: f64| {
            let lat = lat.clamp(-85.05, 85.05).to_radians();
            let x = ((lon + 180.0) / 360.0 * f64::from(n)).floor();
            let y = ((1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0
                * f64::from(n))
            .floor();
            (
                x.clamp(0.0, f64::from(n - 1)) as u32,
                y.clamp(0.0, f64::from(n - 1)) as u32,
            )
        };
        for &(lat, lon) in near {
            let dlat = km / 110.57;
            let dlon = km / (111.32 * lat.to_radians().cos().max(0.01));
            let (x0, y0) = tile(lat + dlat, lon - dlon);
            let (x1, y1) = tile(lat - dlat, lon + dlon);
            for x in x0..=x1 {
                for y in y0..=y1 {
                    ids.push(pmtiles::tile_id(z, x, y));
                }
            }
        }
    }
    ids.sort_unstable();
    ids.dedup();
    let mut ranges = vec![(0, pmtiles::first_id(world_zoom.saturating_add(1)))];
    for id in ids {
        match ranges.last_mut() {
            Some(last) if last.1 >= id => last.1 = last.1.max(id + 1),
            _ => ranges.push((id, id + 1)),
        }
    }
    ranges
}

/// Groups of entries (in tile id order) read with one request each.
fn reads(entries: &[Entry]) -> Vec<std::ops::Range<usize>> {
    let mut groups = Vec::new();
    let mut start = 0;
    while start < entries.len() {
        let first = entries[start].offset;
        let mut end_byte = first + u64::from(entries[start].length);
        let mut end = start + 1;
        while let Some(next) = entries.get(end) {
            let next_end = next.offset + u64::from(next.length);
            if next.offset < end_byte
                || next.offset - end_byte > MAX_GAP
                || next_end - first > MAX_READ
            {
                break;
            }
            end_byte = next_end;
            end += 1;
        }
        groups.push(start..end);
        start = end;
    }
    groups
}

/// A source tile, slimmed and compressed again.
fn slim_tile(bytes: &[u8], compression: u8) -> Result<Vec<u8>> {
    let tile = pmtiles::decompress(bytes, compression)?;
    Ok(pmtiles::gzip(&mvt::slim(&tile)?))
}

pub fn run(args: FetchMapArgs) -> Result<()> {
    let near = args
        .near
        .iter()
        .map(|p| parse_point(p))
        .collect::<Result<Vec<_>>>()?;
    let zoom = if near.is_empty() {
        args.world_zoom
    } else {
        args.zoom.max(args.world_zoom)
    };
    let (from, reader) = open_source(args.from.as_deref())?;
    let header = reader.header;
    if header.tile_type != pmtiles::TILE_MVT {
        bail!("{from} does not hold vector tiles");
    }
    if zoom > header.max_zoom {
        warn!(
            "{from} goes only to zoom {}; keeping up to that",
            header.max_zoom
        );
    }
    let ranges = wanted_ranges(args.world_zoom, zoom.min(header.max_zoom), &near, args.km);
    info!("reading the directories of {from}");
    let entries = reader.entries_in(&ranges)?;
    // Bytes to read by zoom, each stored tile counted once.
    let mut by_zoom: std::collections::BTreeMap<u8, (u64, u64)> = Default::default();
    let mut counted = std::collections::HashSet::new();
    for entry in &entries {
        let z = pmtiles::zoom_of(entry.tile_id);
        let row = by_zoom.entry(z).or_default();
        row.0 += u64::from(entry.run_length);
        if counted.insert(entry.offset) {
            row.1 += u64::from(entry.length);
        }
    }
    drop(counted);
    let total: u64 = by_zoom.values().map(|r| r.1).sum();
    for (z, (tiles, bytes)) in &by_zoom {
        info!(
            "zoom {z}: {tiles} tiles, {:.1} MB to read",
            *bytes as f64 / 1e6
        );
    }
    info!("{:.1} MB to read in all", total as f64 / 1e6);
    if args.dry_run {
        return Ok(());
    }
    let dest = match (&args.out, &args.data) {
        (Some(out), _) => out.clone(),
        (None, Some(data)) => super::file(data),
        (None, None) => bail!("pass --data DIR or --out PATH"),
    };
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let part = dest.with_extension("pmtiles.part");
    let scratch = dest.with_extension("pmtiles.data.part");
    let mut writer = Writer::new(&scratch)?;
    let mut shared: HashMap<u64, Vec<u8>> = HashMap::new();
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut done_bytes = 0u64;
    let mut last_note = std::time::Instant::now();
    for group in reads(&entries) {
        let group = &entries[group];
        let first = group[0].offset;
        let last = group.last().expect("groups are not empty");
        let length = (last.offset + u64::from(last.length) - first) as usize;
        let cached = group.len() == 1 && shared.contains_key(&first);
        let bytes = if cached {
            Vec::new()
        } else {
            reader.source().read(first, length)?
        };
        // Slim the group's tiles on every core, then add them in order.
        let mut slimmed: Vec<Option<Result<Vec<u8>>>> = (0..group.len()).map(|_| None).collect();
        let chunk = group.len().div_ceil(workers).max(1);
        std::thread::scope(|scope| {
            for (entries, out) in group.chunks(chunk).zip(slimmed.chunks_mut(chunk)) {
                let bytes = &bytes;
                let shared = &shared;
                scope.spawn(move || {
                    for (entry, out) in entries.iter().zip(out.iter_mut()) {
                        if let Some(done) = shared.get(&entry.offset) {
                            *out = Some(Ok(done.clone()));
                            continue;
                        }
                        let at = (entry.offset - first) as usize;
                        let Some(tile) = bytes.get(at..at + entry.length as usize) else {
                            *out = Some(Err(anyhow::anyhow!("the source ended early")));
                            continue;
                        };
                        *out = Some(slim_tile(tile, header.tile_compression));
                    }
                });
            }
        });
        for (entry, slim) in group.iter().zip(slimmed) {
            let slim = slim
                .expect("every tile is slimmed")
                .with_context(|| format!("tile {}", entry.tile_id))?;
            writer.add(entry.tile_id, entry.run_length, &slim)?;
            if entry.length <= SHARED_TILE {
                if shared.len() > 100_000 {
                    shared.clear();
                }
                shared.entry(entry.offset).or_insert(slim);
            }
        }
        done_bytes += length as u64;
        if last_note.elapsed() > Duration::from_secs(30) {
            info!(
                "read {:.0} of {:.0} MB, {} tiles",
                done_bytes as f64 / 1e6,
                total as f64 / 1e6,
                writer.tiles()
            );
            last_note = std::time::Instant::now();
        }
    }
    let attribution = "© OpenStreetMap contributors (ODbL), via Protomaps";
    writer.finish(
        &part,
        Header {
            tile_type: pmtiles::TILE_MVT,
            tile_compression: pmtiles::COMPRESSION_GZIP,
            min_zoom: 0,
            max_zoom: zoom.min(header.max_zoom),
            min_lon_e7: -1_800_000_000,
            min_lat_e7: -850_511_287,
            max_lon_e7: 1_800_000_000,
            max_lat_e7: 850_511_287,
            ..Header::default()
        },
        &serde_json::json!({
            "name": "Plumb base map",
            "attribution": attribution,
            "source": from,
            "world_zoom": args.world_zoom,
            "zoom": zoom,
            "near": args.near,
            "km": args.km,
            "vector_layers": mvt::Class::ALL
                .iter()
                .map(|c| serde_json::json!({"id": c.name(), "fields": {}}))
                .collect::<Vec<_>>(),
        }),
    )?;
    std::fs::rename(&part, &dest)
        .with_context(|| format!("moving the map to {}", dest.display()))?;
    let size = std::fs::metadata(&dest).map_or(0, |m| m.len());
    info!(
        "wrote the map to {} ({:.1} MB)",
        dest.display(),
        size as f64 / 1e6
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_world_then_tiles_near_the_points() {
        let ranges = wanted_ranges(2, 4, &[(39.74, -104.99)], 50.0);
        assert_eq!(ranges[0], (0, pmtiles::first_id(3)));
        // Denver is one tile at zooms 3 and 4.
        assert_eq!(ranges.len(), 3);
        assert_eq!(
            wanted_ranges(5, 5, &[], 50.0),
            vec![(0, pmtiles::first_id(6))]
        );
    }

    #[test]
    fn points_parse() {
        assert_eq!(parse_point("39.74, -104.99").unwrap(), (39.74, -104.99));
        assert!(parse_point("91,0").is_err());
        assert!(parse_point("denver").is_err());
    }

    #[test]
    fn reads_group_nearby_tiles() {
        let entry = |offset, length| Entry {
            tile_id: 0,
            offset,
            length,
            run_length: 1,
        };
        let entries = [
            entry(0, 10),
            entry(10, 10),
            entry(5_000_000, 10),
            entry(0, 10),
        ];
        assert_eq!(reads(&entries), vec![0..2, 2..3, 3..4]);
    }
}
