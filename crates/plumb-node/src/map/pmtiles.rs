//! The PMTiles format (version 3): map tiles in one file, found by a
//! directory of tile ids, so a reader takes only the bytes it needs, from a
//! file or over HTTP with range requests. See
//! <https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md>.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Mutex;

use anyhow::{bail, ensure, Context, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression as GzLevel;

/// Bytes of the header.
pub const HEADER_LEN: usize = 127;
/// The header and root directory fit in this many bytes, so one read of
/// it gets both.
const ROOT_SPAN: usize = 16_384;
/// How deep leaf directories may nest.
const MAX_DEPTH: usize = 4;
/// `tile_type` of Mapbox vector tiles.
pub const TILE_MVT: u8 = 1;
/// `*_compression`: none and gzip, the two this reads.
pub const COMPRESSION_NONE: u8 = 1;
pub const COMPRESSION_GZIP: u8 = 2;

/// The id of tile `z/x/y`: all the tiles of lower zooms, then the tiles of
/// zoom `z` along a Hilbert curve, so tiles near each other have ids near
/// each other.
pub fn tile_id(z: u8, x: u32, y: u32) -> u64 {
    let mut id = ((1u64 << (2 * u32::from(z))) - 1) / 3;
    let n = 1u64 << z;
    let (mut x, mut y) = (u64::from(x), u64::from(y));
    let mut s = n / 2;
    while s > 0 {
        let rx = u64::from(x & s > 0);
        let ry = u64::from(y & s > 0);
        id += s * s * ((3 * rx) ^ ry);
        if ry == 0 {
            if rx == 1 {
                x = (n - 1).wrapping_sub(x);
                y = (n - 1).wrapping_sub(y);
            }
            std::mem::swap(&mut x, &mut y);
        }
        s /= 2;
    }
    id
}

/// The first tile id of zoom `z`.
pub fn first_id(z: u8) -> u64 {
    ((1u64 << (2 * u32::from(z))) - 1) / 3
}

/// The zoom of tile id `id`.
pub fn zoom_of(id: u64) -> u8 {
    let mut z = 0;
    while first_id(z + 1) <= id {
        z += 1;
    }
    z
}

/// The fixed part at the start of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    pub root_offset: u64,
    pub root_length: u64,
    pub metadata_offset: u64,
    pub metadata_length: u64,
    pub leaf_offset: u64,
    pub leaf_length: u64,
    pub data_offset: u64,
    pub data_length: u64,
    pub addressed_tiles: u64,
    pub tile_entries: u64,
    pub tile_contents: u64,
    pub clustered: bool,
    pub internal_compression: u8,
    pub tile_compression: u8,
    pub tile_type: u8,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub min_lon_e7: i32,
    pub min_lat_e7: i32,
    pub max_lon_e7: i32,
    pub max_lat_e7: i32,
    pub center_zoom: u8,
    pub center_lon_e7: i32,
    pub center_lat_e7: i32,
}

impl Header {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() >= HEADER_LEN, "too short for a PMTiles header");
        ensure!(&bytes[..7] == b"PMTiles", "not a PMTiles file");
        ensure!(
            bytes[7] == 3,
            "PMTiles version {} (only 3 is read)",
            bytes[7]
        );
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let i32_at = |at: usize| i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        Ok(Header {
            root_offset: u64_at(8),
            root_length: u64_at(16),
            metadata_offset: u64_at(24),
            metadata_length: u64_at(32),
            leaf_offset: u64_at(40),
            leaf_length: u64_at(48),
            data_offset: u64_at(56),
            data_length: u64_at(64),
            addressed_tiles: u64_at(72),
            tile_entries: u64_at(80),
            tile_contents: u64_at(88),
            clustered: bytes[96] == 1,
            internal_compression: bytes[97],
            tile_compression: bytes[98],
            tile_type: bytes[99],
            min_zoom: bytes[100],
            max_zoom: bytes[101],
            min_lon_e7: i32_at(102),
            min_lat_e7: i32_at(106),
            max_lon_e7: i32_at(110),
            max_lat_e7: i32_at(114),
            center_zoom: bytes[118],
            center_lon_e7: i32_at(119),
            center_lat_e7: i32_at(123),
        })
    }

    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[..7].copy_from_slice(b"PMTiles");
        out[7] = 3;
        let fields = [
            self.root_offset,
            self.root_length,
            self.metadata_offset,
            self.metadata_length,
            self.leaf_offset,
            self.leaf_length,
            self.data_offset,
            self.data_length,
            self.addressed_tiles,
            self.tile_entries,
            self.tile_contents,
        ];
        for (n, field) in fields.iter().enumerate() {
            out[8 + 8 * n..16 + 8 * n].copy_from_slice(&field.to_le_bytes());
        }
        out[96] = u8::from(self.clustered);
        out[97] = self.internal_compression;
        out[98] = self.tile_compression;
        out[99] = self.tile_type;
        out[100] = self.min_zoom;
        out[101] = self.max_zoom;
        out[102..106].copy_from_slice(&self.min_lon_e7.to_le_bytes());
        out[106..110].copy_from_slice(&self.min_lat_e7.to_le_bytes());
        out[110..114].copy_from_slice(&self.max_lon_e7.to_le_bytes());
        out[114..118].copy_from_slice(&self.max_lat_e7.to_le_bytes());
        out[118] = self.center_zoom;
        out[119..123].copy_from_slice(&self.center_lon_e7.to_le_bytes());
        out[123..127].copy_from_slice(&self.center_lat_e7.to_le_bytes());
        out
    }
}

/// One entry of a directory: a run of `run_length` tiles from `tile_id`
/// with the same bytes, or, when `run_length` is 0, a leaf directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub tile_id: u64,
    pub offset: u64,
    pub length: u32,
    pub run_length: u32,
}

fn read_varint(bytes: &[u8], at: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at).context("directory ends early")?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(value);
        }
    }
    bail!("varint too long")
}

pub(super) fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Reads a directory (already decompressed).
pub fn parse_directory(bytes: &[u8]) -> Result<Vec<Entry>> {
    let mut at = 0;
    let count = read_varint(bytes, &mut at)? as usize;
    ensure!(count <= bytes.len(), "directory too long for its bytes");
    let mut entries = vec![
        Entry {
            tile_id: 0,
            offset: 0,
            length: 0,
            run_length: 0,
        };
        count
    ];
    let mut last = 0u64;
    for entry in &mut entries {
        last += read_varint(bytes, &mut at)?;
        entry.tile_id = last;
    }
    for entry in &mut entries {
        entry.run_length = read_varint(bytes, &mut at)? as u32;
    }
    for entry in &mut entries {
        entry.length = read_varint(bytes, &mut at)? as u32;
    }
    for n in 0..count {
        let value = read_varint(bytes, &mut at)?;
        entries[n].offset = if value == 0 && n > 0 {
            entries[n - 1].offset + u64::from(entries[n - 1].length)
        } else {
            value.saturating_sub(1)
        };
    }
    Ok(entries)
}

/// Writes a directory (not yet compressed).
pub fn directory_bytes(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * 6 + 8);
    write_varint(&mut out, entries.len() as u64);
    let mut last = 0;
    for entry in entries {
        write_varint(&mut out, entry.tile_id - last);
        last = entry.tile_id;
    }
    for entry in entries {
        write_varint(&mut out, u64::from(entry.run_length));
    }
    for entry in entries {
        write_varint(&mut out, u64::from(entry.length));
    }
    for (n, entry) in entries.iter().enumerate() {
        let follows =
            n > 0 && entry.offset == entries[n - 1].offset + u64::from(entries[n - 1].length);
        write_varint(&mut out, if follows { 0 } else { entry.offset + 1 });
    }
    out
}

/// `bytes` decompressed as `compression` says.
pub fn decompress(bytes: &[u8], compression: u8) -> Result<Vec<u8>> {
    match compression {
        COMPRESSION_NONE | 0 => Ok(bytes.to_vec()),
        COMPRESSION_GZIP => {
            let mut out = Vec::with_capacity(bytes.len() * 3);
            GzDecoder::new(bytes)
                .read_to_end(&mut out)
                .context("ungzipping")?;
            Ok(out)
        }
        other => bail!("compression {other} is not read (only none and gzip)"),
    }
}

pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::with_capacity(bytes.len() / 3), GzLevel::default());
    encoder.write_all(bytes).expect("writing to memory");
    encoder.finish().expect("writing to memory")
}

/// Where a PMTiles file's bytes come from.
pub trait Source: Send + Sync {
    /// `length` bytes at `offset` (fewer only at the end of the file).
    fn read(&self, offset: u64, length: usize) -> Result<Vec<u8>>;
}

/// A file on disk.
pub struct FileSource(Mutex<std::fs::File>);

impl FileSource {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let file =
            std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        Ok(FileSource(Mutex::new(file)))
    }
}

impl Source for FileSource {
    fn read(&self, offset: u64, length: usize) -> Result<Vec<u8>> {
        use std::io::{Seek, SeekFrom};
        let mut file = self.0.lock().unwrap_or_else(|e| e.into_inner());
        file.seek(SeekFrom::Start(offset))?;
        let mut out = Vec::with_capacity(length);
        (&mut *file).take(length as u64).read_to_end(&mut out)?;
        Ok(out)
    }
}

/// Bytes in memory, for tests.
impl Source for Vec<u8> {
    fn read(&self, offset: u64, length: usize) -> Result<Vec<u8>> {
        let start = (offset as usize).min(self.len());
        let end = start.saturating_add(length).min(self.len());
        Ok(self[start..end].to_vec())
    }
}

/// Leaf directories kept in memory by a reader, at most.
const LEAVES_KEPT: usize = 64;

/// Reads tiles from a PMTiles source.
pub struct Reader<S> {
    source: S,
    pub header: Header,
    root: Vec<Entry>,
    leaves: Mutex<HashMap<u64, std::sync::Arc<Vec<Entry>>>>,
}

impl<S: Source> Reader<S> {
    pub fn new(source: S) -> Result<Self> {
        let start = source.read(0, ROOT_SPAN)?;
        let header = Header::parse(&start)?;
        let root_end = (header.root_offset + header.root_length) as usize;
        let root_bytes = if root_end <= start.len() {
            start[header.root_offset as usize..root_end].to_vec()
        } else {
            source.read(header.root_offset, header.root_length as usize)?
        };
        let root = parse_directory(&decompress(&root_bytes, header.internal_compression)?)
            .context("reading the root directory")?;
        Ok(Reader {
            source,
            header,
            root,
            leaves: Mutex::new(HashMap::new()),
        })
    }

    pub fn source(&self) -> &S {
        &self.source
    }

    /// The metadata, as JSON.
    pub fn metadata(&self) -> Result<serde_json::Value> {
        if self.header.metadata_length == 0 {
            return Ok(serde_json::Value::Null);
        }
        let bytes = self.source.read(
            self.header.metadata_offset,
            self.header.metadata_length as usize,
        )?;
        let bytes = decompress(&bytes, self.header.internal_compression)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn leaf(&self, entry: &Entry) -> Result<std::sync::Arc<Vec<Entry>>> {
        let offset = self.header.leaf_offset + entry.offset;
        if let Some(leaf) = self
            .leaves
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&offset)
        {
            return Ok(leaf.clone());
        }
        let bytes = self.source.read(offset, entry.length as usize)?;
        let leaf = std::sync::Arc::new(parse_directory(&decompress(
            &bytes,
            self.header.internal_compression,
        )?)?);
        let mut leaves = self.leaves.lock().unwrap_or_else(|e| e.into_inner());
        if leaves.len() >= LEAVES_KEPT {
            leaves.clear();
        }
        leaves.insert(offset, leaf.clone());
        Ok(leaf)
    }

    /// Where tile `id`'s bytes are: offset from the start of the file and
    /// length.
    pub fn find(&self, id: u64) -> Result<Option<(u64, u32)>> {
        let mut dir = std::sync::Arc::new(self.root.clone());
        for _ in 0..MAX_DEPTH {
            let at = dir.partition_point(|e| e.tile_id <= id);
            let Some(entry) = at.checked_sub(1).map(|n| dir[n]) else {
                return Ok(None);
            };
            if entry.run_length == 0 {
                dir = self.leaf(&entry)?;
                continue;
            }
            if id < entry.tile_id + u64::from(entry.run_length) {
                return Ok(Some((self.header.data_offset + entry.offset, entry.length)));
            }
            return Ok(None);
        }
        bail!("leaf directories nest too deep")
    }

    /// Tile `z/x/y`, decompressed, if the file has it.
    pub fn tile(&self, z: u8, x: u32, y: u32) -> Result<Option<Vec<u8>>> {
        let Some((offset, length)) = self.find(tile_id(z, x, y))? else {
            return Ok(None);
        };
        let bytes = self.source.read(offset, length as usize)?;
        Ok(Some(decompress(&bytes, self.header.tile_compression)?))
    }

    /// The tile entries (never leaves) whose tiles fall in `ranges` (sorted
    /// half-open ranges of tile ids), each cut to the ranges, with offsets
    /// from the start of the file.
    pub fn entries_in(&self, ranges: &[(u64, u64)]) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        self.collect(&self.root.clone(), u64::MAX, ranges, &mut out, 0)?;
        Ok(out)
    }

    fn collect(
        &self,
        dir: &[Entry],
        end: u64,
        ranges: &[(u64, u64)],
        out: &mut Vec<Entry>,
        depth: usize,
    ) -> Result<()> {
        ensure!(depth < MAX_DEPTH, "leaf directories nest too deep");
        for (n, entry) in dir.iter().enumerate() {
            let next = dir.get(n + 1).map_or(end, |e| e.tile_id);
            let span_end = if entry.run_length == 0 {
                next
            } else {
                entry.tile_id + u64::from(entry.run_length)
            };
            // The ranges this entry overlaps.
            let first = ranges.partition_point(|r| r.1 <= entry.tile_id);
            let overlaps = ranges[first..]
                .iter()
                .take_while(|r| r.0 < span_end)
                .copied()
                .collect::<Vec<_>>();
            if overlaps.is_empty() {
                continue;
            }
            if entry.run_length == 0 {
                let leaf = self.leaf(entry)?;
                self.collect(&leaf, next, ranges, out, depth + 1)?;
                continue;
            }
            for (start, stop) in overlaps {
                let from = start.max(entry.tile_id);
                let to = stop.min(span_end);
                if from < to {
                    out.push(Entry {
                        tile_id: from,
                        offset: self.header.data_offset + entry.offset,
                        length: entry.length,
                        run_length: (to - from) as u32,
                    });
                }
            }
        }
        Ok(())
    }
}

/// Writes a PMTiles file: tiles are added in tile id order, the same bytes
/// stored once, then [`Writer::finish`] writes the directories in front.
pub struct Writer {
    data: std::io::BufWriter<std::fs::File>,
    data_path: std::path::PathBuf,
    data_len: u64,
    entries: Vec<Entry>,
    /// Where each tile's bytes went, by their hash.
    stored: HashMap<[u8; 32], (u64, u32)>,
    addressed: u64,
    /// The header and root directory fit in this many bytes.
    root_span: usize,
}

impl Writer {
    /// A writer that keeps tile bytes in `scratch` until it finishes.
    pub fn new(scratch: &std::path::Path) -> Result<Self> {
        let file = std::fs::File::create(scratch)
            .with_context(|| format!("creating {}", scratch.display()))?;
        Ok(Writer {
            data: std::io::BufWriter::new(file),
            data_path: scratch.to_path_buf(),
            data_len: 0,
            entries: Vec::new(),
            stored: HashMap::new(),
            addressed: 0,
            root_span: ROOT_SPAN,
        })
    }

    /// Adds `run` tiles from `id` with these (compressed) bytes; ids must
    /// grow.
    pub fn add(&mut self, id: u64, run: u32, bytes: &[u8]) -> Result<()> {
        use sha2::Digest;
        if let Some(last) = self.entries.last() {
            ensure!(
                id >= last.tile_id + u64::from(last.run_length),
                "tiles out of order"
            );
        }
        let hash: [u8; 32] = sha2::Sha256::digest(bytes).into();
        let (offset, length) = match self.stored.get(&hash) {
            Some(at) => *at,
            None => {
                self.data.write_all(bytes)?;
                let at = (self.data_len, bytes.len() as u32);
                self.data_len += bytes.len() as u64;
                self.stored.insert(hash, at);
                at
            }
        };
        self.addressed += u64::from(run);
        if let Some(last) = self.entries.last_mut() {
            if last.offset == offset
                && last.tile_id + u64::from(last.run_length) == id
                && last.run_length.checked_add(run).is_some()
            {
                last.run_length += run;
                return Ok(());
            }
        }
        self.entries.push(Entry {
            tile_id: id,
            offset,
            length,
            run_length: run,
        });
        Ok(())
    }

    /// Tiles added so far.
    pub fn tiles(&self) -> u64 {
        self.addressed
    }

    /// Writes the file to `path`. `header` gives the tile type, compression,
    /// zooms and bounds; the rest is filled in here.
    pub fn finish(
        mut self,
        path: &std::path::Path,
        mut header: Header,
        metadata: &serde_json::Value,
    ) -> Result<()> {
        self.data.flush()?;
        drop(self.data);
        let (root, leaves) = directories(&self.entries, self.root_span);
        let metadata = gzip(&serde_json::to_vec(metadata)?);
        header.internal_compression = COMPRESSION_GZIP;
        header.clustered = true;
        header.root_offset = HEADER_LEN as u64;
        header.root_length = root.len() as u64;
        header.metadata_offset = header.root_offset + header.root_length;
        header.metadata_length = metadata.len() as u64;
        header.leaf_offset = header.metadata_offset + header.metadata_length;
        header.leaf_length = leaves.len() as u64;
        header.data_offset = header.leaf_offset + header.leaf_length;
        header.data_length = self.data_len;
        header.addressed_tiles = self.addressed;
        header.tile_entries = self.entries.len() as u64;
        header.tile_contents = self.stored.len() as u64;
        let mut out = std::io::BufWriter::new(
            std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?,
        );
        out.write_all(&header.to_bytes())?;
        out.write_all(&root)?;
        out.write_all(&metadata)?;
        out.write_all(&leaves)?;
        let mut data = std::fs::File::open(&self.data_path)?;
        std::io::copy(&mut data, &mut out)?;
        out.flush()?;
        drop(data);
        let _ = std::fs::remove_file(&self.data_path);
        Ok(())
    }
}

/// The root directory and the leaf directories, compressed, so the root
/// fits in the first `span` bytes with the header.
fn directories(entries: &[Entry], span: usize) -> (Vec<u8>, Vec<u8>) {
    let root = gzip(&directory_bytes(entries));
    if root.len() + HEADER_LEN <= span {
        return (root, Vec::new());
    }
    let mut per_leaf = if span < ROOT_SPAN { 256 } else { 4096 };
    loop {
        let mut leaves = Vec::new();
        let mut pointers = Vec::new();
        for chunk in entries.chunks(per_leaf) {
            let leaf = gzip(&directory_bytes(chunk));
            pointers.push(Entry {
                tile_id: chunk[0].tile_id,
                offset: leaves.len() as u64,
                length: leaf.len() as u32,
                run_length: 0,
            });
            leaves.extend_from_slice(&leaf);
        }
        let root = gzip(&directory_bytes(&pointers));
        if root.len() + HEADER_LEN <= span {
            return (root, leaves);
        }
        per_leaf *= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_ids_follow_the_hilbert_curve() {
        assert_eq!(tile_id(0, 0, 0), 0);
        assert_eq!(tile_id(1, 0, 0), 1);
        assert_eq!(tile_id(1, 0, 1), 2);
        assert_eq!(tile_id(1, 1, 1), 3);
        assert_eq!(tile_id(1, 1, 0), 4);
        assert_eq!(tile_id(2, 0, 0), 5);
        assert_eq!(first_id(3), 21);
        assert_eq!(zoom_of(20), 2);
        assert_eq!(zoom_of(21), 3);
        // Every tile of a zoom has its own id within the zoom's ids.
        let mut ids: Vec<u64> = (0..8)
            .flat_map(|x| (0..8).map(move |y| tile_id(3, x, y)))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 64);
        assert_eq!(ids[0], first_id(3));
        assert_eq!(ids[63], first_id(4) - 1);
    }

    #[test]
    fn directories_read_back() {
        let entries = vec![
            Entry {
                tile_id: 0,
                offset: 0,
                length: 10,
                run_length: 1,
            },
            Entry {
                tile_id: 1,
                offset: 10,
                length: 5,
                run_length: 3,
            },
            Entry {
                tile_id: 9,
                offset: 0,
                length: 10,
                run_length: 1,
            },
        ];
        assert_eq!(
            parse_directory(&directory_bytes(&entries)).unwrap(),
            entries
        );
    }

    #[test]
    fn a_written_file_reads_back_with_leaves() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::new(&dir.path().join("data")).unwrap();
        // A small root, so it needs leaf directories.
        writer.root_span = 300;
        let mut expect = Vec::new();
        for id in (0..60_000u64).step_by(3) {
            let bytes = gzip(format!("tile {}", (id * 7_919) % 5_003).as_bytes());
            writer.add(id, 1, &bytes).unwrap();
            expect.push(id);
        }
        writer.add(700_000, 50, &gzip(b"sea")).unwrap();
        let path = dir.path().join("t.pmtiles");
        writer
            .finish(
                &path,
                Header {
                    tile_type: TILE_MVT,
                    tile_compression: COMPRESSION_GZIP,
                    max_zoom: 9,
                    ..Header::default()
                },
                &serde_json::json!({"name": "test"}),
            )
            .unwrap();
        let reader = Reader::new(FileSource::open(&path).unwrap()).unwrap();
        assert!(reader.header.leaf_length > 0, "{:?}", reader.header);
        assert_eq!(reader.metadata().unwrap()["name"], "test");
        for id in [0u64, 2_997, 59_997] {
            let (offset, length) = reader.find(id).unwrap().unwrap();
            let bytes = reader.source().read(offset, length as usize).unwrap();
            assert_eq!(
                decompress(&bytes, COMPRESSION_GZIP).unwrap(),
                format!("tile {}", (id * 7_919) % 5_003).as_bytes()
            );
        }
        assert_eq!(reader.find(1).unwrap(), None);
        assert!(reader.find(700_049).unwrap().is_some());
        assert_eq!(reader.find(700_050).unwrap(), None);
        // Only the asked ranges, runs cut to them.
        let found = reader.entries_in(&[(5, 13), (700_010, 700_012)]).unwrap();
        let ids: Vec<(u64, u32)> = found.iter().map(|e| (e.tile_id, e.run_length)).collect();
        assert_eq!(ids, vec![(6, 1), (9, 1), (12, 1), (700_010, 2)]);
    }
}
