//! The music page set: the songs and albums people listen to most, from
//! MusicBrainz's data dump, listed next to the sites ("bohemian rhapsody
//! queen" finds the song, and "bohemian rhapsody lyrics" links its lyrics
//! on Genius).
//!
//! MusicBrainz publishes its whole database twice a week. Only the core
//! tables of `mbdump.tar.bz2` are read, which are CC0; nothing of the
//! derived dumps (tags, ratings, annotations) is. No lyrics are kept, only
//! where to read them: the Genius page MusicBrainz links a song's work to.
//!
//! A song is all the recordings of one title by one artist credit, its
//! remasters and live versions too. How popular it is, is how many people
//! listened to its recordings, as ListenBrainz (MusicBrainz's sister
//! project, whose data is CC0 too) counts them; an album's, how many
//! listened to it. ListenBrainz is asked about the songs on at least
//! [`MusicOptions::min_song_releases`] release groups (an album, a single,
//! a compilation) and every song of the albums most listened to, so an
//! album's best-known songs are asked about even when no single or
//! compilation put them out.
//!
//! They are written as an articles file ([`plumb_core::article`]): the
//! title is the song's or album's title, the description "Song by ARTIST,
//! YEAR" or "Album by ARTIST, YEAR", the item `recording/MBID` or
//! `release-group/MBID` (from which the address on musicbrainz.org is
//! made), the views its listeners, the one alias "TITLE ARTIST", and the
//! profiles where it can be read or heard: its lyrics on Genius, the song
//! or album on Spotify or Apple Music, its music video.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use plumb_core::article::{Article, MAX_ARTICLE_DESCRIPTION_CHARS};
use plumb_core::profiles::Profile;
use serde::Deserialize;
use tracing::{info, warn};

/// Where MusicBrainz's full exports are listed; `LATEST` names the newest.
pub const FULLEXPORT_URL: &str = "https://data.metabrainz.org/pub/musicbrainz/data/fullexport/";
/// The core tables' archive in an export, about 6 GB.
pub const CORE_DUMP: &str = "mbdump.tar.bz2";

/// The tables read, each a file `mbdump/TABLE` in the archive.
pub const TABLES: &[&str] = &[
    "artist_credit",
    "l_recording_url",
    "l_recording_work",
    "l_release_url",
    "l_url_work",
    "medium",
    "recording",
    "release",
    "release_country",
    "release_group",
    "release_group_primary_type",
    "release_group_secondary_type",
    "release_group_secondary_type_join",
    "release_status",
    "release_unknown_country",
    "track",
    "url",
];

/// What is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MusicOptions {
    /// Most songs kept, the most listened to.
    pub max_songs: usize,
    /// Fewest release groups a song is on to be asked about, unless it is
    /// on an album kept.
    pub min_song_releases: u32,
    /// Most albums kept, the most listened to.
    pub max_albums: usize,
    /// Fewest official editions of an album asked about.
    pub min_album_releases: u32,
    /// Fewest listeners of a song or album kept.
    pub min_listeners: u64,
}

/// Defaults of [`MusicOptions`].
pub const DEFAULT_MAX_SONGS: usize = 150_000;
pub const DEFAULT_MIN_SONG_RELEASES: u32 = 2;
pub const DEFAULT_MAX_ALBUMS: usize = 30_000;
pub const DEFAULT_MIN_ALBUM_RELEASES: u32 = 1;
pub const DEFAULT_MIN_LISTENERS: u64 = 20;

impl Default for MusicOptions {
    fn default() -> Self {
        MusicOptions {
            max_songs: DEFAULT_MAX_SONGS,
            min_song_releases: DEFAULT_MIN_SONG_RELEASES,
            max_albums: DEFAULT_MAX_ALBUMS,
            min_album_releases: DEFAULT_MIN_ALBUM_RELEASES,
            min_listeners: DEFAULT_MIN_LISTENERS,
        }
    }
}

/// The directory of the newest full export, from `LATEST`'s text
/// (`20261004-001001`).
pub fn export_url(latest: &str) -> Result<String> {
    let latest = latest.trim();
    if latest.is_empty()
        || !latest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("MusicBrainz's LATEST does not name an export: {latest:?}");
    }
    Ok(format!("{FULLEXPORT_URL}{latest}/"))
}

/// Writes the [`TABLES`] of the core dump `archive` (`mbdump.tar.bz2`) to
/// `dir`, each whole or not at all. Tables already there are kept, so an
/// interrupted run carries on.
pub fn extract_tables(archive: &Path, dir: &Path) -> Result<()> {
    // Which archive the tables there came from: its size and time.
    let meta = std::fs::metadata(archive)
        .with_context(|| format!("reading {}", archive.display()))?;
    let from = format!(
        "{} {}",
        meta.len(),
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs())
    );
    let marker = dir.join(".from");
    if std::fs::read_to_string(&marker).is_ok_and(|text| text == from) {
        if TABLES.iter().all(|table| dir.join(table).is_file()) {
            info!("keeping the tables in {}", dir.display());
            return Ok(());
        }
    } else {
        // Tables of another dump are not kept.
        for table in TABLES {
            let _ = std::fs::remove_file(dir.join(table));
        }
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(&marker, &from)?;
    let file =
        std::fs::File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let decoder = bzip2::read::MultiBzDecoder::new(BufReader::with_capacity(1 << 20, file));
    let mut tar = tar::Archive::new(decoder);
    let mut found = HashSet::new();
    for entry in tar.entries().context("reading the MusicBrainz dump")? {
        let mut entry = entry.context("reading the MusicBrainz dump")?;
        let path = entry.path()?.into_owned();
        let Some(table) = path
            .strip_prefix("mbdump")
            .ok()
            .and_then(|p| p.to_str())
            .and_then(|name| TABLES.iter().find(|t| **t == name))
        else {
            continue;
        };
        let dest = dir.join(table);
        found.insert(*table);
        if dest.is_file() {
            continue;
        }
        info!("extracting {table}");
        let part = dir.join(format!("{table}.part"));
        let mut out = std::io::BufWriter::new(
            std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?,
        );
        std::io::copy(&mut entry, &mut out).with_context(|| format!("extracting {table}"))?;
        std::io::Write::flush(&mut out)?;
        drop(out);
        std::fs::rename(&part, &dest)?;
    }
    let missing: Vec<&str> = TABLES
        .iter()
        .copied()
        .filter(|t| !found.contains(t) && !dir.join(t).is_file())
        .collect();
    if !missing.is_empty() {
        bail!(
            "{} has no table {}; is it MusicBrainz's core dump?",
            archive.display(),
            missing.join(", ")
        );
    }
    Ok(())
}

/// The directory holding the tables, for `path` given as the dump: a
/// directory of tables (or one with `mbdump/` in it), or the archive,
/// whose tables are then extracted beside it into `mbdump/`.
pub fn tables_dir(path: &Path) -> Result<PathBuf> {
    if path.is_dir() {
        let inner = path.join("mbdump");
        return Ok(if inner.is_dir() {
            inner
        } else {
            path.to_path_buf()
        });
    }
    let dir = path.parent().unwrap_or(Path::new(".")).join("mbdump");
    extract_tables(path, &dir)?;
    Ok(dir)
}

/// A field of PostgreSQL's text COPY format, which MusicBrainz's dumps
/// are in: `None` for `\N`, otherwise with its backslash escapes undone.
fn text(raw: &str) -> Option<String> {
    if raw == "\\N" {
        return None;
    }
    if !raw.contains('\\') {
        return Some(raw.to_string());
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('b') | Some('f') | Some('v') => out.push(' '),
            Some(other) => out.push(other),
            None => {}
        }
    }
    Some(out)
}

/// A whole-number field; `None` for `\N` or anything else.
fn num(raw: &str) -> Option<u32> {
    raw.parse().ok()
}

/// Calls `each` with the fields of every row of the table `table` in
/// `dir`, and says how many rows there were.
fn for_each_row(dir: &Path, table: &str, mut each: impl FnMut(&[&str])) -> Result<u64> {
    let path = dir.join(table);
    let file = std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut line = Vec::new();
    let mut rows = 0u64;
    loop {
        line.clear();
        if reader
            .read_until(b'\n', &mut line)
            .with_context(|| format!("reading {}", path.display()))?
            == 0
        {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        // A tab within a field is written `\t`, so a tab always parts two.
        let fields: Vec<&str> = text.trim_end_matches(['\n', '\r']).split('\t').collect();
        each(&fields);
        rows += 1;
    }
    Ok(rows)
}

/// The ids of the rows of a table of names (`release_status`) called so.
fn named_ids(dir: &Path, table: &str) -> Result<HashMap<String, u32>> {
    let mut ids = HashMap::new();
    for_each_row(dir, table, |row| {
        if let (Some(id), Some(name)) = (row.first().and_then(|f| num(f)), row.get(1)) {
            if let Some(name) = text(name) {
                ids.insert(name, id);
            }
        }
    })?;
    Ok(ids)
}

/// Sets `v[i]` to `x`, growing `v` as needed.
fn put<T: Clone + Default>(v: &mut Vec<T>, i: u32, x: T) {
    let i = i as usize;
    if i >= v.len() {
        v.resize(i + 1, T::default());
    }
    v[i] = x;
}

fn at<T: Clone + Default>(v: &[T], i: u32) -> T {
    v.get(i as usize).cloned().unwrap_or_default()
}

/// `year`, or the earlier of it and `other` when both are known (0 is
/// not known).
fn earliest(year: u16, other: u16) -> u16 {
    match (year, other) {
        (0, y) | (y, 0) => y,
        (a, b) => a.min(b),
    }
}

/// Whether `gid` reads as a MusicBrainz id.
fn is_mbid(gid: &str) -> bool {
    gid.len() == 36
        && gid.chars().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// What a song is grouped by: its artist credit and its title's words.
fn song_key(credit: u32, title: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    credit.hash(&mut hasher);
    plumb_core::normalize_text(title).hash(&mut hasher);
    // 0 marks a recording left out.
    hasher.finish() | 1
}

/// Whether a title is a placeholder rather than a name: `[untitled]`,
/// `[silence]`.
fn is_placeholder(title: &str) -> bool {
    title.trim().is_empty() || title.trim_start().starts_with('[')
}

/// Artist credits that name no artist: "Various Artists", "[unknown]".
fn is_no_artist(name: &str) -> bool {
    name == "Various Artists" || is_placeholder(name)
}

/// A MusicBrainz id (`b1a9c0e9-d987-4042-ae91-78d6a3267d69`) as a number.
pub type Mbid = u128;

/// The MusicBrainz id `gid` reads as.
pub fn parse_mbid(gid: &str) -> Option<Mbid> {
    is_mbid(gid)
        .then(|| u128::from_str_radix(&gid.replace('-', ""), 16).ok())
        .flatten()
}

/// The MusicBrainz id `id` as written: `b1a9c0e9-d987-4042-ae91-78d6a3267d69`.
pub fn mbid_text(id: Mbid) -> String {
    let hex = format!("{id:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// An album that may be kept.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Album {
    id: u32,
    gid: Mbid,
    title: String,
    credit: u32,
    year: u16,
}

/// A song that may be kept: its recordings and the year it first came out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Song {
    recordings: Vec<u32>,
    year: u16,
}

/// What the tables of MusicBrainz's dump hold about songs and albums,
/// read before ListenBrainz is asked how listened to they are.
pub struct MusicDump {
    dir: PathBuf,
    options: MusicOptions,
    /// Each recording's song (see [`song_key`]) and the recording, sorted,
    /// so a song's recordings are together.
    song_recordings: Vec<(u64, u32)>,
    /// Release groups each recording is on, by recording.
    recording_groups: Vec<u32>,
    /// The year each recording first came out, by recording.
    recording_year: Vec<u16>,
    /// Each recording and release group it is on, as `recording << 32 |
    /// group`, sorted.
    on_groups: Vec<u64>,
    /// Each official release's release group, by release.
    release_group: Vec<u32>,
    albums: Vec<Album>,
    /// The songs asked about, after [`MusicDump::songs_to_ask`].
    songs: Vec<Song>,
    /// Their recordings' ids.
    recording_mbids: HashMap<u32, Mbid>,
}

impl MusicDump {
    /// Reads the tables in `dir`.
    pub fn read(dir: &Path, options: &MusicOptions) -> Result<MusicDump> {
        let statuses = named_ids(dir, "release_status")?;
        let official = *statuses
            .get("Official")
            .context("release_status has no Official")?;
        let album_type = *named_ids(dir, "release_group_primary_type")?
            .get("Album")
            .context("release_group_primary_type has no Album")?;
        // An album that is also a compilation, a live album, a DJ mix or a
        // demo is left out; a soundtrack is kept.
        let left_out_types: HashSet<u32> = named_ids(dir, "release_group_secondary_type")?
            .into_iter()
            .filter(|(name, _)| name != "Soundtrack")
            .map(|(_, id)| id)
            .collect();

        let mut no_artist = HashSet::new();
        for_each_row(dir, "artist_credit", |row| {
            if let (Some(id), Some(name)) = (row.first().and_then(|f| num(f)), row.get(1)) {
                if text(name).is_none_or(|name| is_no_artist(&name)) {
                    no_artist.insert(id);
                }
            }
        })?;

        // The year each release came out, the earliest of its countries'.
        let mut release_year: Vec<u16> = Vec::new();
        let mut note_year = |release: Option<u32>, year: Option<&&str>| {
            let (Some(release), Some(year)) = (release, year.and_then(|y| y.parse::<u16>().ok()))
            else {
                return;
            };
            let year = earliest(at(&release_year, release), year);
            put(&mut release_year, release, year);
        };
        for_each_row(dir, "release_country", |row| {
            note_year(row.first().and_then(|f| num(f)), row.get(2));
        })?;
        for_each_row(dir, "release_unknown_country", |row| {
            note_year(row.first().and_then(|f| num(f)), row.get(1));
        })?;

        // Official releases' groups, and how many official editions each
        // group has.
        let mut release_group: Vec<u32> = Vec::new();
        let mut editions: Vec<u32> = Vec::new();
        let mut group_year: Vec<u16> = Vec::new();
        let releases = for_each_row(dir, "release", |row| {
            let (Some(id), Some(group)) = (
                row.first().and_then(|f| num(f)),
                row.get(4).and_then(|f| num(f)),
            ) else {
                return;
            };
            if row.get(5).and_then(|f| num(f)) != Some(official) {
                return;
            }
            put(&mut release_group, id, group);
            let count = at(&editions, group) + 1;
            put(&mut editions, group, count);
            let year = earliest(at(&group_year, group), at(&release_year, id));
            put(&mut group_year, group, year);
        })?;
        info!("read {releases} releases");

        let mut not_albums = HashSet::new();
        for_each_row(dir, "release_group_secondary_type_join", |row| {
            if let (Some(group), Some(kind)) = (
                row.first().and_then(|f| num(f)),
                row.get(1).and_then(|f| num(f)),
            ) {
                if left_out_types.contains(&kind) {
                    not_albums.insert(group);
                }
            }
        })?;
        let mut albums: Vec<Album> = Vec::new();
        for_each_row(dir, "release_group", |row| {
            let (Some(id), Some(gid), Some(title), Some(credit)) = (
                row.first().and_then(|f| num(f)),
                row.get(1).and_then(|f| parse_mbid(f)),
                row.get(2).and_then(|f| text(f)),
                row.get(3).and_then(|f| num(f)),
            ) else {
                return;
            };
            if row.get(4).and_then(|f| num(f)) != Some(album_type)
                || at(&editions, id) < options.min_album_releases.max(1)
                || not_albums.contains(&id)
                || no_artist.contains(&credit)
                || is_placeholder(&title)
            {
                return;
            }
            albums.push(Album {
                id,
                gid,
                title,
                credit,
                year: at(&group_year, id),
            });
        })?;
        info!("{} albums to ask about", albums.len());

        let mut medium_release: Vec<u32> = Vec::new();
        for_each_row(dir, "medium", |row| {
            if let (Some(id), Some(release)) = (
                row.first().and_then(|f| num(f)),
                row.get(1).and_then(|f| num(f)),
            ) {
                put(&mut medium_release, id, release);
            }
        })?;

        // Each recording's song, 0 for one left out: a video, a placeholder
        // or one by no artist.
        let mut recording_song: Vec<u64> = Vec::new();
        let recordings = for_each_row(dir, "recording", |row| {
            let (Some(id), Some(title), Some(credit)) = (
                row.first().and_then(|f| num(f)),
                row.get(2).and_then(|f| text(f)),
                row.get(3).and_then(|f| num(f)),
            ) else {
                return;
            };
            if row.get(8) == Some(&"t") || is_placeholder(&title) || no_artist.contains(&credit) {
                return;
            }
            put(&mut recording_song, id, song_key(credit, &title));
        })?;
        info!("read {recordings} recordings");

        let mut on_groups: Vec<u64> = Vec::new();
        let mut recording_year: Vec<u16> = Vec::new();
        let tracks = for_each_row(dir, "track", |row| {
            let (Some(recording), Some(medium)) = (
                row.get(2).and_then(|f| num(f)),
                row.get(3).and_then(|f| num(f)),
            ) else {
                return;
            };
            if row.get(11) == Some(&"t") || at(&recording_song, recording) == 0 {
                return;
            }
            let release = at(&medium_release, medium);
            let group = at(&release_group, release);
            if group == 0 {
                return;
            }
            on_groups.push(u64::from(recording) << 32 | u64::from(group));
            let year = earliest(at(&recording_year, recording), at(&release_year, release));
            put(&mut recording_year, recording, year);
        })?;
        info!("read {tracks} tracks");
        if on_groups.is_empty() {
            bail!(
                "no track of {} is on an official release; are these MusicBrainz's core tables, \
                 with their columns where this build expects them?",
                dir.display()
            );
        }
        on_groups.sort_unstable();
        on_groups.dedup();
        let mut recording_groups: Vec<u32> = Vec::new();
        for run in on_groups.chunk_by(|a, b| a >> 32 == b >> 32) {
            put(
                &mut recording_groups,
                (run[0] >> 32) as u32,
                run.len() as u32,
            );
        }
        let mut song_recordings: Vec<(u64, u32)> = recording_song
            .iter()
            .enumerate()
            .filter(|&(id, &song)| song != 0 && at(&recording_groups, id as u32) > 0)
            .map(|(id, &song)| (song, id as u32))
            .collect();
        song_recordings.sort_unstable();
        Ok(MusicDump {
            dir: dir.to_path_buf(),
            options: *options,
            song_recordings,
            recording_groups,
            recording_year,
            on_groups,
            release_group,
            albums,
            songs: Vec::new(),
            recording_mbids: HashMap::new(),
        })
    }

    /// The albums to ask ListenBrainz about.
    pub fn album_mbids(&self) -> Vec<Mbid> {
        self.albums.iter().map(|a| a.gid).collect()
    }

    /// The albums kept, given each album's `listeners`: the
    /// [`MusicOptions::max_albums`] most listened to, with at least
    /// [`MusicOptions::min_listeners`].
    fn kept_albums(&self, listeners: &HashMap<Mbid, u64>) -> Vec<(&Album, u64)> {
        let mut kept: Vec<(&Album, u64)> = self
            .albums
            .iter()
            .filter_map(|album| {
                let n = listeners.get(&album.gid).copied().unwrap_or(0);
                (n >= self.options.min_listeners).then_some((album, n))
            })
            .collect();
        kept.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.id.cmp(&b.0.id)));
        kept.truncate(self.options.max_albums);
        kept
    }

    /// The recordings to ask ListenBrainz about: every recording of each
    /// song on at least [`MusicOptions::min_song_releases`] release groups
    /// or on an album kept, given each album's `listeners`.
    pub fn songs_to_ask(&mut self, album_listeners: &HashMap<Mbid, u64>) -> Result<Vec<Mbid>> {
        let kept_albums: HashSet<u32> = self
            .kept_albums(album_listeners)
            .into_iter()
            .map(|(album, _)| album.id)
            .collect();
        let mut on_kept_album: Vec<bool> = Vec::new();
        for pair in &self.on_groups {
            if kept_albums.contains(&(*pair as u32)) {
                put(&mut on_kept_album, (pair >> 32) as u32, true);
            }
        }
        let mut songs = Vec::new();
        for run in self.song_recordings.chunk_by(|a, b| a.0 == b.0) {
            let recordings: Vec<u32> = run.iter().map(|&(_, id)| id).collect();
            let groups: u32 = recordings
                .iter()
                .map(|&id| at(&self.recording_groups, id))
                .sum();
            if groups >= self.options.min_song_releases
                || recordings.iter().any(|&id| at(&on_kept_album, id))
            {
                let year = recordings
                    .iter()
                    .fold(0, |year, &id| earliest(year, at(&self.recording_year, id)));
                songs.push(Song { recordings, year });
            }
        }
        let wanted: HashSet<u32> = songs
            .iter()
            .flat_map(|s| s.recordings.iter().copied())
            .collect();
        let mut mbids = HashMap::new();
        for_each_row(&self.dir, "recording", |row| {
            if let (Some(id), Some(gid)) = (
                row.first().and_then(|f| num(f)),
                row.get(1).and_then(|f| parse_mbid(f)),
            ) {
                if wanted.contains(&id) {
                    mbids.insert(id, gid);
                }
            }
        })?;
        info!(
            "{} songs with {} recordings to ask about",
            songs.len(),
            mbids.len()
        );
        self.songs = songs;
        self.recording_mbids = mbids;
        let mut asked: Vec<Mbid> = self.recording_mbids.values().copied().collect();
        asked.sort_unstable();
        Ok(asked)
    }

    /// The songs and albums kept, most listened to first, given each
    /// album's and recording's listeners. A song is listed as its most
    /// listened to recording.
    pub fn into_articles(
        self,
        album_listeners: &HashMap<Mbid, u64>,
        recording_listeners: &HashMap<Mbid, u64>,
    ) -> Result<Vec<Article>> {
        let listened = |id: u32| {
            self.recording_mbids
                .get(&id)
                .and_then(|gid| recording_listeners.get(gid))
                .copied()
                .unwrap_or(0)
        };
        let mut songs: Vec<(u32, u64, u16)> = self
            .songs
            .iter()
            .filter_map(|song| {
                let listeners: u64 = song.recordings.iter().map(|&id| listened(id)).sum();
                // The most listened to recording; of those listened to
                // alike, the one on most release groups.
                let best = *song.recordings.iter().max_by(|&&a, &&b| {
                    listened(a)
                        .cmp(&listened(b))
                        .then(at(&self.recording_groups, a).cmp(&at(&self.recording_groups, b)))
                        .then(b.cmp(&a))
                })?;
                (listeners >= self.options.min_listeners).then_some((best, listeners, song.year))
            })
            .collect();
        songs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        songs.truncate(self.options.max_songs);
        let wanted: HashMap<u32, (u64, u16)> = songs
            .iter()
            .map(|&(id, listeners, year)| (id, (listeners, year)))
            .collect();
        let mut kept: Vec<Kept> = Vec::new();
        for_each_row(&self.dir, "recording", |row| {
            let (Some(id), Some(title), Some(credit)) = (
                row.first().and_then(|f| num(f)),
                row.get(2).and_then(|f| text(f)),
                row.get(3).and_then(|f| num(f)),
            ) else {
                return;
            };
            let (Some(&(listeners, year)), Some(&gid)) =
                (wanted.get(&id), self.recording_mbids.get(&id))
            else {
                return;
            };
            kept.push(Kept {
                kind: RECORDING,
                id,
                listeners,
                year,
                gid,
                title,
                credit,
                profiles: Vec::new(),
            });
        })?;
        let songs_kept = kept.len();
        kept.extend(
            self.kept_albums(album_listeners)
                .into_iter()
                .map(|(album, listeners)| Kept {
                    kind: RELEASE_GROUP,
                    id: album.id,
                    listeners,
                    year: album.year,
                    gid: album.gid,
                    title: album.title.clone(),
                    credit: album.credit,
                    profiles: Vec::new(),
                }),
        );
        info!(
            "keeping {songs_kept} songs and {} albums",
            kept.len() - songs_kept
        );
        add_links(&self.dir, &mut kept, &self.release_group)?;

        let credits: HashSet<u32> = kept.iter().map(|k| k.credit).collect();
        let mut names: HashMap<u32, String> = HashMap::new();
        for_each_row(&self.dir, "artist_credit", |row| {
            if let (Some(id), Some(name)) = (
                row.first().and_then(|f| num(f)),
                row.get(1).and_then(|f| text(f)),
            ) {
                if credits.contains(&id) {
                    names.insert(id, name);
                }
            }
        })?;
        kept.sort_by(|a, b| {
            b.listeners
                .cmp(&a.listeners)
                .then(a.kind.cmp(b.kind))
                .then(a.id.cmp(&b.id))
        });
        Ok(kept
            .into_iter()
            .map(|k| {
                let artist = names.get(&k.credit).map(String::as_str);
                into_article(k, artist)
            })
            .collect())
    }
}

/// What a song's item and address start with.
pub const RECORDING: &str = "recording";
/// What an album's item and address start with.
pub const RELEASE_GROUP: &str = "release-group";

/// A song or album kept, before its artist's name is read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kept {
    /// [`RECORDING`] or [`RELEASE_GROUP`].
    kind: &'static str,
    /// Its recording's or release group's row id.
    id: u32,
    listeners: u64,
    year: u16,
    gid: Mbid,
    title: String,
    credit: u32,
    profiles: Vec<Profile>,
}

/// The profile a link of MusicBrainz's is, among those kept for `kind`
/// ([`RECORDING`] or [`RELEASE_GROUP`]): a song's Genius lyrics, Spotify
/// track and music video, an album's Spotify and Apple Music albums.
pub fn profile_of(kind: &str, address: &str) -> Option<Profile> {
    let url = url::Url::parse(address).ok()?;
    let host = url.host_str()?.trim_start_matches("www.");
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let (service, id) = match (kind, host, segments.as_slice()) {
        (RECORDING, "genius.com", [page]) if page.ends_with("-lyrics") => {
            ("genius-song", page.to_string())
        }
        (RECORDING, "open.spotify.com", ["track", id]) => ("spotify-track", id.to_string()),
        (RECORDING, "youtube.com" | "music.youtube.com", ["watch"]) => (
            "youtube-video",
            url.query_pairs()
                .find(|(k, _)| k == "v")
                .map(|(_, v)| v.into_owned())?,
        ),
        (RELEASE_GROUP, "open.spotify.com", ["album", id]) => ("spotify-album", id.to_string()),
        (RELEASE_GROUP, "music.apple.com" | "itunes.apple.com", [.., last])
            if segments.contains(&"album") =>
        {
            let digits = last.trim_start_matches("id");
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            ("apple-music-album", digits.to_string())
        }
        _ => return None,
    };
    let profile = Profile {
        service: service.to_string(),
        id,
    };
    profile.link().map(|_| profile)
}

/// Adds to the songs and albums `kept` the links MusicBrainz has for
/// them: a song's recording's own and its work's (lyrics), an album's
/// releases'. `release_group` gives each official release's group.
fn add_links(dir: &Path, kept: &mut [Kept], release_group: &[u32]) -> Result<()> {
    let index = |kind: &str| -> HashMap<u32, usize> {
        kept.iter()
            .enumerate()
            .filter(|(_, k)| k.kind == kind)
            .map(|(i, k)| (k.id, i))
            .collect()
    };
    let songs = index(RECORDING);
    let albums = index(RELEASE_GROUP);
    // A song's works: the songs performed.
    let mut work_songs: HashMap<u32, Vec<usize>> = HashMap::new();
    for_each_row(dir, "l_recording_work", |row| {
        if let (Some(recording), Some(work)) = (
            row.get(2).and_then(|f| num(f)),
            row.get(3).and_then(|f| num(f)),
        ) {
            if let Some(&i) = songs.get(&recording) {
                work_songs.entry(work).or_default().push(i);
            }
        }
    })?;
    let mut url_of: HashMap<u32, Vec<usize>> = HashMap::new();
    for_each_row(dir, "l_recording_url", |row| {
        if let (Some(recording), Some(url)) = (
            row.get(2).and_then(|f| num(f)),
            row.get(3).and_then(|f| num(f)),
        ) {
            if let Some(&i) = songs.get(&recording) {
                url_of.entry(url).or_default().push(i);
            }
        }
    })?;
    for_each_row(dir, "l_url_work", |row| {
        if let (Some(url), Some(work)) = (
            row.get(2).and_then(|f| num(f)),
            row.get(3).and_then(|f| num(f)),
        ) {
            for &i in work_songs.get(&work).into_iter().flatten() {
                url_of.entry(url).or_default().push(i);
            }
        }
    })?;
    for_each_row(dir, "l_release_url", |row| {
        if let (Some(release), Some(url)) = (
            row.get(2).and_then(|f| num(f)),
            row.get(3).and_then(|f| num(f)),
        ) {
            if let Some(&i) = albums.get(&at(release_group, release)) {
                url_of.entry(url).or_default().push(i);
            }
        }
    })?;
    for_each_row(dir, "url", |row| {
        let (Some(id), Some(address)) = (row.first().and_then(|f| num(f)), row.get(2)) else {
            return;
        };
        for &i in url_of.get(&id).into_iter().flatten() {
            let Some(profile) = profile_of(kept[i].kind, address) else {
                continue;
            };
            // One of each service, the first linked.
            if !kept[i]
                .profiles
                .iter()
                .any(|p| p.service == profile.service)
            {
                kept[i].profiles.push(profile);
            }
        }
    })?;
    for k in kept.iter_mut() {
        k.profiles.sort_by_key(|p| {
            plumb_core::profiles::SERVICES
                .iter()
                .position(|s| s.key == p.service)
        });
    }
    Ok(())
}

/// The song or album `kept` by `artist` as an articles file line (see the
/// module docs).
fn into_article(kept: Kept, artist: Option<&str>) -> Article {
    let what = if kept.kind == RECORDING {
        "Song"
    } else {
        "Album"
    };
    let description = match (artist, kept.year) {
        (Some(artist), 0) => format!("{what} by {artist}"),
        (Some(artist), year) => format!("{what} by {artist}, {year}"),
        (None, 0) => what.to_string(),
        (None, year) => format!("{what}, {year}"),
    };
    let title = plumb_core::collapse_whitespace(&kept.title);
    Article {
        description: Some(plumb_core::truncate_chars(
            &description,
            MAX_ARTICLE_DESCRIPTION_CHARS,
        )),
        item: Some(format!("{}/{}", kept.kind, mbid_text(kept.gid))),
        site: None,
        views: kept.listeners,
        aliases: artist
            .map(|artist| vec![format!("{title} {artist}")])
            .unwrap_or_default(),
        title,
        profiles: kept.profiles,
        website: None,
        package: None,
        facts: Vec::new(),
    }
}

/// ListenBrainz's popularity API: how many people listened to recordings
/// or release groups, up to [`LISTENBRAINZ_BATCH`] a request.
pub const LISTENBRAINZ_URL: &str = "https://api.listenbrainz.org/1/popularity/";
/// Most ids ListenBrainz takes in one request.
pub const LISTENBRAINZ_BATCH: usize = 1_000;

/// What ListenBrainz is asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listened {
    Recordings,
    Albums,
}

impl Listened {
    /// The path, the field of the ids asked about, and of the id answered.
    fn names(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Listened::Recordings => ("recording", "recording_mbids", "recording_mbid"),
            Listened::Albums => ("release-group", "release_group_mbids", "release_group_mbid"),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Popularity {
    #[serde(default)]
    recording_mbid: Option<String>,
    #[serde(default)]
    release_group_mbid: Option<String>,
    #[serde(default)]
    total_user_count: Option<u64>,
}

/// The listeners in a ListenBrainz answer, by id; ids it knows nothing of
/// have none.
pub fn parse_popularity(json: &[u8]) -> Result<Vec<(Mbid, u64)>> {
    let found: Vec<Popularity> =
        serde_json::from_slice(json).context("reading ListenBrainz's answer")?;
    Ok(found
        .into_iter()
        .filter_map(|p| {
            let id = parse_mbid(p.recording_mbid.or(p.release_group_mbid)?.as_str())?;
            Some((id, p.total_user_count.unwrap_or(0)))
        })
        .collect())
}

/// The listeners of each of `ids`, from ListenBrainz. Answers are kept in
/// `cache` (`MBID \t listeners` lines) as they come, so a run that stops
/// carries on where it left off, and the rate ListenBrainz sets is kept to.
pub async fn fetch_listeners(
    client: &reqwest::Client,
    what: Listened,
    ids: &[Mbid],
    cache: &Path,
) -> Result<HashMap<Mbid, u64>> {
    use std::io::Write;
    let mut known: HashMap<Mbid, u64> = HashMap::new();
    if cache.is_file() {
        let reader = BufReader::new(std::fs::File::open(cache)?);
        for line in reader.lines() {
            let line = line?;
            if let Some((id, n)) = line.split_once('\t') {
                if let (Some(id), Ok(n)) = (parse_mbid(id), n.trim().parse()) {
                    known.insert(id, n);
                }
            }
        }
    }
    let wanted: Vec<Mbid> = ids
        .iter()
        .copied()
        .filter(|id| !known.contains_key(id))
        .collect();
    info!(
        "asking ListenBrainz about {} of {} ({} known)",
        wanted.len(),
        ids.len(),
        ids.len() - wanted.len()
    );
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cache)
        .with_context(|| format!("opening {}", cache.display()))?;
    let (path, field, _) = what.names();
    let url = format!("{LISTENBRAINZ_URL}{path}");
    let batches = wanted.chunks(LISTENBRAINZ_BATCH);
    let count = batches.len();
    for (n, batch) in batches.enumerate() {
        let ids: Vec<String> = batch.iter().map(|id| mbid_text(*id)).collect();
        let body = serde_json::to_vec(&serde_json::json!({ field: ids }))?;
        let mut tries = 0u32;
        let answer = loop {
            tries += 1;
            let sent = client
                .post(&url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone())
                .send()
                .await;
            let response = match sent {
                Ok(response) => response,
                Err(err) if tries < 8 => {
                    warn!("asking ListenBrainz: {err}; trying again");
                    tokio::time::sleep(Duration::from_secs(5 * u64::from(tries))).await;
                    continue;
                }
                Err(err) => return Err(err).context("asking ListenBrainz"),
            };
            let header = |name: &str| {
                response
                    .headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
            };
            let remaining = header("x-ratelimit-remaining");
            let reset_in = header("x-ratelimit-reset-in").unwrap_or(10).min(120);
            let status = response.status();
            if status.as_u16() == 429 || status.is_server_error() {
                if tries >= 8 {
                    bail!("ListenBrainz keeps answering {status}");
                }
                warn!("ListenBrainz answered {status}; waiting {reset_in} s");
                tokio::time::sleep(Duration::from_secs(reset_in + u64::from(tries))).await;
                continue;
            }
            if !status.is_success() {
                bail!("ListenBrainz answered {status}");
            }
            let bytes = response.bytes().await.context("reading ListenBrainz")?;
            // Keep to the rate: wait out the window once it is used up.
            if remaining.is_some_and(|r| r <= 1) {
                tokio::time::sleep(Duration::from_secs(reset_in + 1)).await;
            }
            break bytes;
        };
        let found = parse_popularity(&answer)?;
        let mut lines = String::new();
        let answered: HashMap<Mbid, u64> = found.into_iter().collect();
        // Ids ListenBrainz did not answer about are noted with none, so
        // they are not asked about again.
        for id in batch {
            let n = answered.get(id).copied().unwrap_or(0);
            known.insert(*id, n);
            lines.push_str(&format!("{}\t{n}\n", mbid_text(*id)));
        }
        out.write_all(lines.as_bytes())?;
        if (n + 1) % 200 == 0 || n + 1 == count {
            info!("asked ListenBrainz {} of {count} times", n + 1);
        }
    }
    Ok(ids
        .iter()
        .filter_map(|id| Some((*id, *known.get(id)?)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOHEMIAN: &str = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";
    const BOHEMIAN_LIVE: &str = "aaaaaaaa-d987-4042-ae91-78d6a3267d69";
    const NOBODY: &str = "bbbbbbbb-d987-4042-ae91-78d6a3267d69";
    const VIDEO: &str = "cccccccc-d987-4042-ae91-78d6a3267d69";
    const NIGHT: &str = "1dc4c347-a1db-32aa-b14f-bc9cc507b843";
    const HITS: &str = "dddddddd-a1db-32aa-b14f-bc9cc507b843";

    /// Writes the tables of a small dump: Queen's "Bohemian Rhapsody" on
    /// A Night at the Opera (two official editions) and on Greatest Hits
    /// (a compilation), with a live recording too; a song by no artist;
    /// a music video; and a bootleg.
    fn write_dump(dir: &Path) {
        let tables: &[(&str, &str)] = &[
            ("artist_credit", "1\tQueen\t1\t10\t2000-01-01\t0\tx\n2\tVarious Artists\t1\t10\t2000-01-01\t0\tx\n3\tEarth, Wind \\\\ Fire\t1\t1\t2000-01-01\t0\tx\n"),
            ("release_status", "1\tOfficial\t\\N\t1\t\\N\tx\n3\tBootleg\t\\N\t3\t\\N\tx\n"),
            ("release_group_primary_type", "1\tAlbum\t\\N\t1\t\\N\tx\n2\tSingle\t\\N\t2\t\\N\tx\n"),
            ("release_group_secondary_type", "1\tCompilation\t\\N\t1\t\\N\tx\n2\tSoundtrack\t\\N\t2\t\\N\tx\n"),
            ("release_group_secondary_type_join", "11\t1\t2000-01-01\n"),
            (
                "release_group",
                &format!("10\t{NIGHT}\tA Night at the Opera\t1\t1\t\t0\tx\n11\t{HITS}\tGreatest Hits\t1\t1\t\t0\tx\n12\t{NOBODY}\tA Single\t1\t2\t\t0\tx\n13\t{NOBODY}\tAll Sorts\t2\t1\t\t0\tx\n"),
            ),
            (
                "release",
                "100\tx\tA Night at the Opera\t1\t10\t1\t\\N\t\\N\t\\N\t\\N\t\t0\t-1\tx\n101\tx\tA Night at the Opera\t1\t10\t1\t\\N\t\\N\t\\N\t\\N\t\t0\t-1\tx\n102\tx\tGreatest Hits\t1\t11\t1\t\\N\t\\N\t\\N\t\\N\t\t0\t-1\tx\n103\tx\tA Single\t1\t12\t1\t\\N\t\\N\t\\N\t\\N\t\t0\t-1\tx\n104\tx\tLive Bootleg\t1\t12\t3\t\\N\t\\N\t\\N\t\\N\t\t0\t-1\tx\n105\tx\tAll Sorts\t2\t13\t1\t\\N\t\\N\t\\N\t\\N\t\t0\t-1\tx\n",
            ),
            ("release_country", "100\t1\t1975\t11\t21\n101\t2\t1976\t\\N\t\\N\n102\t1\t1981\t\\N\t\\N\n"),
            ("release_unknown_country", "103\t1992\t\\N\t\\N\n"),
            ("medium", "1000\t100\t1\t1\t\t0\tx\t12\n1001\t101\t1\t1\t\t0\tx\t12\n1002\t102\t1\t1\t\t0\tx\t17\n1003\t103\t1\t1\t\t0\tx\t2\n1004\t104\t1\t1\t\t0\tx\t2\n1005\t105\t1\t1\t\t0\tx\t2\n"),
            (
                "recording",
                &format!("500\t{BOHEMIAN}\tBohemian Rhapsody\t1\t354000\t\t0\tx\tf\n501\t{BOHEMIAN_LIVE}\tBohemian  Rhapsody\t1\t360000\tlive\t0\tx\tf\n502\t{NOBODY}\tSomething\t2\t1\t\t0\tx\tf\n503\t{VIDEO}\tBohemian Rhapsody (video)\t1\t1\t\t0\tx\tt\n504\t{NOBODY}\t[untitled]\t1\t1\t\t0\tx\tf\n"),
            ),
            (
                "track",
                "1\tx\t500\t1000\t1\t1\tBohemian Rhapsody\t1\t1\t0\tx\tf\n2\tx\t500\t1001\t1\t1\tBohemian Rhapsody\t1\t1\t0\tx\tf\n3\tx\t500\t1002\t1\t1\tBohemian Rhapsody\t1\t1\t0\tx\tf\n4\tx\t501\t1003\t1\t1\tBohemian Rhapsody\t1\t1\t0\tx\tf\n5\tx\t501\t1004\t1\t1\tBohemian Rhapsody\t1\t1\t0\tx\tf\n6\tx\t502\t1005\t1\t1\tSomething\t2\t1\t0\tx\tf\n7\tx\t503\t1000\t2\t2\tVideo\t1\t1\t0\tx\tf\n8\tx\t504\t1000\t3\t3\tx\t1\t1\t0\tx\tf\n",
            ),
            ("l_recording_work", "1\t1\t500\t9000\t0\tx\t0\t\t\n"),
            ("l_url_work", "1\t2\t7000\t9000\t0\tx\t0\t\t\n"),
            ("l_recording_url", "1\t3\t500\t7001\t0\tx\t0\t\t\n2\t3\t500\t7002\t0\tx\t0\t\t\n"),
            ("l_release_url", "1\t4\t101\t7003\t0\tx\t0\t\t\n"),
            (
                "url",
                "7000\tx\thttps://genius.com/Queen-bohemian-rhapsody-lyrics\t0\tx\n7001\tx\thttps://open.spotify.com/track/7tFiyTwD0nx5a1eklYtX2J\t0\tx\n7002\tx\thttps://example.com/evil\t0\tx\n7003\tx\thttps://music.apple.com/gb/album/1440650428\t0\tx\n7004\tx\thttps://genius.com/unrelated-lyrics\t0\tx\n",
            ),
        ];
        for (table, rows) in tables {
            std::fs::write(dir.join(table), rows).unwrap();
        }
    }

    fn mbid(text: &str) -> Mbid {
        parse_mbid(text).unwrap()
    }

    #[test]
    fn songs_and_albums_are_read_from_the_dump() {
        let dir = tempfile::tempdir().unwrap();
        write_dump(dir.path());
        let options = MusicOptions {
            min_song_releases: 3,
            min_listeners: 1,
            ..MusicOptions::default()
        };
        let mut dump = MusicDump::read(dir.path(), &options).unwrap();
        // The single is no album, the compilation is left out, and so is
        // the album by no artist.
        assert_eq!(dump.album_mbids(), [mbid(NIGHT)]);
        let albums = HashMap::from([(mbid(NIGHT), 19_247)]);
        // Bohemian Rhapsody is on three release groups (the bootleg is not
        // counted); the song by no artist, the video and the placeholder
        // are left out.
        let mut asked = dump.songs_to_ask(&albums).unwrap();
        asked.sort();
        let mut both = vec![mbid(BOHEMIAN), mbid(BOHEMIAN_LIVE)];
        both.sort();
        assert_eq!(asked, both);
        let recordings = HashMap::from([(mbid(BOHEMIAN), 211_087), (mbid(BOHEMIAN_LIVE), 13)]);
        let music = dump.into_articles(&albums, &recordings).unwrap();
        let titles: Vec<&str> = music.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, ["Bohemian Rhapsody", "A Night at the Opera"]);
        let song = &music[0];
        assert_eq!(song.views, 211_100);
        assert_eq!(
            song.item.as_deref(),
            Some(&*format!("recording/{BOHEMIAN}"))
        );
        assert_eq!(song.description.as_deref(), Some("Song by Queen, 1975"));
        assert_eq!(song.aliases, ["Bohemian Rhapsody Queen"]);
        let links: Vec<String> = song.profiles.iter().map(|p| p.link().unwrap().1).collect();
        assert_eq!(
            links,
            [
                "https://open.spotify.com/track/7tFiyTwD0nx5a1eklYtX2J",
                "https://genius.com/Queen-bohemian-rhapsody-lyrics",
            ]
        );
        let album = &music[1];
        assert_eq!(album.views, 19_247);
        assert_eq!(
            album.item.as_deref(),
            Some(&*format!("release-group/{NIGHT}"))
        );
        assert_eq!(album.description.as_deref(), Some("Album by Queen, 1975"));
        assert_eq!(
            album.profiles[0].link().unwrap().1,
            "https://music.apple.com/album/1440650428"
        );
    }

    #[test]
    fn songs_of_albums_kept_are_asked_about() {
        let dir = tempfile::tempdir().unwrap();
        write_dump(dir.path());
        let options = MusicOptions {
            min_song_releases: 10,
            min_listeners: 100,
            ..MusicOptions::default()
        };
        // On too few release groups, and its album too little listened to.
        let mut dump = MusicDump::read(dir.path(), &options).unwrap();
        let quiet = HashMap::from([(mbid(NIGHT), 99)]);
        assert!(dump.songs_to_ask(&quiet).unwrap().is_empty());
        // Its album is kept, so the song is asked about; but it is too
        // little listened to itself.
        let mut dump = MusicDump::read(dir.path(), &options).unwrap();
        let albums = HashMap::from([(mbid(NIGHT), 100)]);
        assert_eq!(dump.songs_to_ask(&albums).unwrap().len(), 2);
        let recordings = HashMap::from([(mbid(BOHEMIAN), 60), (mbid(BOHEMIAN_LIVE), 39)]);
        let music = dump.into_articles(&albums, &recordings).unwrap();
        let titles: Vec<&str> = music.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, ["A Night at the Opera"]);
        // The songs kept are cut to the most listened to.
        let few = MusicOptions {
            max_songs: 0,
            max_albums: 0,
            ..options
        };
        let mut dump = MusicDump::read(dir.path(), &few).unwrap();
        dump.songs_to_ask(&albums).unwrap();
        assert!(dump.into_articles(&albums, &recordings).unwrap().is_empty());
    }

    #[test]
    fn listeners_are_read_from_listenbrainz() {
        let json = br#"[{"recording_mbid":"b1a9c0e9-d987-4042-ae91-78d6a3267d69","total_listen_count":2268014,"total_user_count":211087},{"recording_mbid":"ebf79ba5-085e-48d2-9eb8-2d992fbf0f6d","total_listen_count":null,"total_user_count":null}]"#;
        assert_eq!(
            parse_popularity(json).unwrap(),
            [
                (mbid(BOHEMIAN), 211_087),
                (mbid("ebf79ba5-085e-48d2-9eb8-2d992fbf0f6d"), 0)
            ]
        );
        let json = br#"[{"release_group_mbid":"1dc4c347-a1db-32aa-b14f-bc9cc507b843","total_listen_count":583674,"total_user_count":19247}]"#;
        assert_eq!(parse_popularity(json).unwrap(), [(mbid(NIGHT), 19_247)]);
        assert_eq!(mbid_text(mbid(BOHEMIAN)), BOHEMIAN);
        assert_eq!(parse_mbid("b1a9c0e9d9874042ae9178d6a3267d69"), None);
    }

    #[test]
    fn copy_fields_are_unescaped() {
        assert_eq!(text("\\N"), None);
        assert_eq!(text("AC\\\\DC").as_deref(), Some("AC\\DC"));
        assert_eq!(text("a\\tb").as_deref(), Some("a\tb"));
        assert_eq!(text("plain").as_deref(), Some("plain"));
    }

    #[test]
    fn only_known_links_are_kept() {
        let link = |kind: &str, url: &str| profile_of(kind, url).map(|p| p.link().unwrap().1);
        assert_eq!(
            link("recording", "https://www.youtube.com/watch?v=fJ9rUzIMcZQ").as_deref(),
            Some("https://www.youtube.com/watch?v=fJ9rUzIMcZQ")
        );
        assert_eq!(
            link(
                "recording",
                "https://genius.com/Queen-bohemian-rhapsody-lyrics"
            )
            .as_deref(),
            Some("https://genius.com/Queen-bohemian-rhapsody-lyrics")
        );
        assert_eq!(link("recording", "https://genius.com/artists/Queen"), None);
        assert_eq!(
            link("recording", "https://genius.com/albums/Queen/Jazz"),
            None
        );
        assert_eq!(
            link(
                "release-group",
                "https://genius.com/Queen-bohemian-rhapsody-lyrics"
            ),
            None
        );
        assert_eq!(
            link(
                "release-group",
                "https://itunes.apple.com/us/album/id1440650428"
            )
            .as_deref(),
            Some("https://music.apple.com/album/1440650428")
        );
        assert_eq!(
            link(
                "release-group",
                "https://open.spotify.com/album/1GbtB4zTqAsyfZEsm1RZfx"
            )
            .as_deref(),
            Some("https://open.spotify.com/album/1GbtB4zTqAsyfZEsm1RZfx")
        );
        assert_eq!(
            link("recording", "https://open.spotify.com/track/../evil"),
            None
        );
        assert_eq!(link("recording", "not a url"), None);
    }

    #[test]
    fn exports_are_named_by_latest() {
        assert_eq!(
            export_url("20261004-001001\n").unwrap(),
            "https://data.metabrainz.org/pub/musicbrainz/data/fullexport/20261004-001001/"
        );
        assert!(export_url("../etc").is_err());
        assert!(export_url("").is_err());
    }

    #[test]
    fn tables_are_extracted_from_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let tables = dir.path().join("src");
        std::fs::create_dir(&tables).unwrap();
        write_dump(&tables);
        let archive = dir.path().join("mbdump.tar.bz2");
        {
            let file = std::fs::File::create(&archive).unwrap();
            let encoder = bzip2::write::BzEncoder::new(file, bzip2::Compression::fast());
            let mut tar = tar::Builder::new(encoder);
            tar.append_dir_all("mbdump", &tables).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let out = tables_dir(&archive).unwrap();
        assert_eq!(out, dir.path().join("mbdump"));
        for table in TABLES {
            assert_eq!(
                std::fs::read(out.join(table)).unwrap(),
                std::fs::read(tables.join(table)).unwrap()
            );
        }
        // A directory of tables is read as it is.
        assert_eq!(tables_dir(dir.path()).unwrap(), out);
        assert_eq!(tables_dir(&tables).unwrap(), tables);
    }
}
