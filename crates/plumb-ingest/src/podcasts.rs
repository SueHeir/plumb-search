//! The podcasts page set: the podcasts Podcast Index rates most popular,
//! listed next to the sites ("hardcore history podcast" finds Dan
//! Carlin's show).
//!
//! Podcast Index (<https://podcastindex.org>) keeps an open index of every
//! podcast feed, "available for free, for any use", and publishes it whole
//! as a SQLite database. Its `podcasts` table gives each feed a title, the
//! iTunes author, the podcast's website, its Apple Podcasts id and a
//! popularity score from 0 to 9, which many shows share (160,000 have 9),
//! so ties are broken by having an Apple Podcasts listing, then the years
//! the show has run, then its episodes (capped, so a feed posting
//! thousands of generated episodes doesn't lead). A show listed twice
//! under one title and site (or author) is kept once. They are written as an articles file
//! ([`plumb_core::article`]): the title is the podcast's title, the
//! description "Podcast by AUTHOR" and its first category, the item its
//! Podcast Index id (from which the address is made), the site its
//! website's domain when the website is a site of its own, the views a
//! popularity made of the score and the episodes, the one alias
//! "AUTHOR podcast", and the profile its Apple Podcasts id.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use plumb_core::article::{Article, MAX_ARTICLE_DESCRIPTION_CHARS};
use plumb_core::profiles::Profile;
use rusqlite::types::ValueRef;
use tracing::info;

/// Podcast Index's database, a gzipped tar of one SQLite file. It answers
/// only requests that name themselves (`download::USER_AGENT`).
pub const FEEDS_URL: &str = "https://public.podcastindex.org/podcastindex_feeds.db.tgz";

/// The database's file name inside [`FEEDS_URL`]'s archive.
pub const FEEDS_DB: &str = "podcastindex_feeds.db";

/// Fewest popularity points of a podcast kept by default (scores go from 0
/// to 9; about 300,000 podcasts have 4 or more).
pub const DEFAULT_MIN_SCORE: u32 = 4;

/// Highest real popularity score: higher ones are broken rows.
const MAX_SCORE: u32 = 9;

/// Most episodes counted towards popularity.
const MAX_EPISODES: u64 = 999;

/// Most years of running counted towards popularity.
const MAX_YEARS: u64 = 20;

/// Seconds in a year, for the years a show has run.
const YEAR_SECS: i64 = 365 * 24 * 60 * 60;

/// Hosts whose front page is the host's, not a podcast's: a podcast whose
/// website is one of them has no site of its own.
const HOSTS: &[&str] = &[
    "acast.com",
    "anchor.fm",
    "apple.com",
    "art19.com",
    "audioboom.com",
    "blubrry.com",
    "buzzsprout.com",
    "captivate.fm",
    "castos.com",
    "iheart.com",
    "libsyn.com",
    "megaphone.fm",
    "omny.fm",
    "patreon.com",
    "podbean.com",
    "podomatic.com",
    "redcircle.com",
    "simplecast.com",
    "soundcloud.com",
    "spotify.com",
    "spreaker.com",
    "squarespace.com",
    "substack.com",
    "transistor.fm",
    "wordpress.com",
    "youtube.com",
];

/// One podcast kept from the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Podcast {
    pub id: u64,
    pub title: String,
    pub author: String,
    pub link: String,
    pub itunes_id: Option<u64>,
    pub score: u32,
    pub episodes: u64,
    /// Whole years between its oldest and newest episodes.
    pub years: u64,
    pub category: String,
}

impl Podcast {
    /// Its popularity as views: the score, then an Apple Podcasts listing,
    /// then the years it has run, then the episodes.
    fn views(&self) -> u64 {
        let episodes = self.episodes.min(MAX_EPISODES);
        let years = self.years.min(MAX_YEARS) * (MAX_EPISODES + 1);
        let listed = u64::from(self.itunes_id.is_some()) * (MAX_YEARS + 1) * (MAX_EPISODES + 1);
        let score = u64::from(self.score) * 2 * (MAX_YEARS + 1) * (MAX_EPISODES + 1);
        score + listed + years + episodes
    }

    /// What makes two rows the same show: the title and the site, or the
    /// author when it has no site of its own.
    fn show(&self) -> (String, String) {
        let by = self
            .own_site()
            .unwrap_or_else(|| plumb_core::normalize_text(&self.author));
        (plumb_core::normalize_text(&self.title), by)
    }

    /// The domain of its website when that is the front page of a site of
    /// its own: `dancarlin.com` for `https://www.dancarlin.com/`, nothing
    /// for `https://example.libsyn.com/` or `https://anchor.fm/show`.
    fn own_site(&self) -> Option<String> {
        let url = url::Url::parse(self.link.trim()).ok()?;
        if !matches!(url.scheme(), "http" | "https") || !matches!(url.path(), "" | "/") {
            return None;
        }
        let domain = plumb_core::registrable_domain(url.as_str())?;
        (!HOSTS.contains(&domain.as_str())).then_some(domain)
    }

    /// The podcast as an articles file line (see the module docs).
    pub fn into_article(self) -> Article {
        let mut description = if self.author.is_empty() {
            "Podcast".to_string()
        } else {
            format!("Podcast by {}", self.author)
        };
        if !self.category.is_empty() {
            description.push_str(&format!(" · {}", self.category));
        }
        let aliases = if self.author.is_empty()
            || plumb_core::normalize_text(&self.author) == plumb_core::normalize_text(&self.title)
        {
            Vec::new()
        } else {
            vec![format!("{} podcast", self.author)]
        };
        Article {
            description: Some(plumb_core::truncate_chars(
                &description,
                MAX_ARTICLE_DESCRIPTION_CHARS,
            )),
            item: Some(self.id.to_string()),
            site: self.own_site(),
            views: self.views(),
            aliases,
            profiles: self
                .itunes_id
                .map(|id| Profile {
                    service: "apple-podcasts".to_string(),
                    id: id.to_string(),
                })
                .into_iter()
                .collect(),
            title: self.title,
            ..Article::default()
        }
    }
}

/// The podcasts of the database at `db` with a popularity score of at
/// least `min_score` that answered when last fetched, most popular first,
/// at most `keep`.
pub fn read_podcasts(db: &Path, min_score: u32, keep: usize) -> Result<Vec<Podcast>> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", db.display()))?;
    let mut statement = conn.prepare(
        "SELECT id, title, itunesAuthor, link, itunesId, popularityScore, episodeCount, \
         category1, oldestItemPubdate, newestItemPubdate FROM podcasts WHERE popularityScore >= ?1 AND popularityScore <= ?2 \
         AND lastHttpStatus = 200 AND title != '' AND COALESCE(NULLIF(duplicateOf, ''), 0) = 0",
    )?;
    let rows = statement.query_map([min_score, MAX_SCORE], |row| {
        // The dump stores a missing number as '' rather than NULL, so take
        // integers only and anything else as missing; likewise for text.
        let text = |i: usize| -> rusqlite::Result<String> {
            Ok(match row.get_ref(i)? {
                ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                _ => String::new(),
            })
        };
        let number = |i: usize| -> rusqlite::Result<Option<i64>> {
            Ok(match row.get_ref(i)? {
                ValueRef::Integer(n) => Some(n),
                _ => None,
            })
        };
        Ok(Podcast {
            id: number(0)?
                .and_then(|id| u64::try_from(id).ok())
                .unwrap_or(0),
            title: plumb_core::collapse_whitespace(&text(1)?),
            author: plumb_core::collapse_whitespace(&text(2)?),
            link: text(3)?,
            itunes_id: number(4)?
                .and_then(|id| u64::try_from(id).ok())
                .filter(|&id| id > 0),
            score: number(5)?.and_then(|n| u32::try_from(n).ok()).unwrap_or(0),
            episodes: number(6)?.and_then(|n| u64::try_from(n).ok()).unwrap_or(0),
            years: match (number(8)?, number(9)?) {
                (Some(oldest), Some(newest)) if oldest > 0 && newest > oldest => {
                    u64::try_from((newest - oldest) / YEAR_SECS).unwrap_or(0)
                }
                _ => 0,
            },
            category: plumb_core::collapse_whitespace(&text(7)?),
        })
    })?;
    let mut podcasts = Vec::new();
    for row in rows {
        let podcast = row?;
        if podcast.id > 0 && !podcast.title.is_empty() {
            podcasts.push(podcast);
        }
    }
    // Keep one row per show: the most popular, which prefers the one with
    // an Apple Podcasts listing.
    podcasts.sort_by(|a, b| b.views().cmp(&a.views()).then(a.id.cmp(&b.id)));
    let read = podcasts.len();
    let mut seen = std::collections::HashSet::new();
    podcasts.retain(|podcast| seen.insert(podcast.show()));
    info!(
        "{} podcasts with a score of at least {min_score} ({} repeats left out)",
        podcasts.len(),
        read - podcasts.len()
    );
    podcasts.truncate(keep);
    Ok(podcasts)
}

/// Unpacks the database from the archive `tgz` (as downloaded from
/// [`FEEDS_URL`]) into `dir`, unless it is there already and newer, and
/// returns its path.
pub fn unpack_feeds(tgz: &Path, dir: &Path) -> Result<PathBuf> {
    let db = dir.join(FEEDS_DB);
    let modified = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    if let (Some(db_time), Some(tgz_time)) = (modified(&db), modified(tgz)) {
        if db_time >= tgz_time {
            return Ok(db);
        }
    }
    let file = std::fs::File::open(tgz).with_context(|| format!("opening {}", tgz.display()))?;
    let mut archive =
        tar::Archive::new(flate2::read::GzDecoder::new(std::io::BufReader::new(file)));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let is_db = entry
            .path()?
            .file_name()
            .is_some_and(|name| name == FEEDS_DB);
        if is_db {
            let part = dir.join(format!("{FEEDS_DB}.part"));
            let mut out = std::fs::File::create(&part)
                .with_context(|| format!("creating {}", part.display()))?;
            std::io::copy(&mut entry, &mut out)
                .with_context(|| format!("unpacking {}", tgz.display()))?;
            std::fs::rename(&part, &db)?;
            return Ok(db);
        }
    }
    anyhow::bail!("{} has no {FEEDS_DB}", tgz.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feeds(dir: &Path) -> PathBuf {
        let db = dir.join(FEEDS_DB);
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE podcasts (id INTEGER PRIMARY KEY, url TEXT, title TEXT, link TEXT, \
             lastHttpStatus INTEGER, itunesId INTEGER, itunesAuthor TEXT, episodeCount INTEGER, \
             popularityScore INTEGER, category1 TEXT, duplicateOf INTEGER, \
             oldestItemPubdate INTEGER, newestItemPubdate INTEGER);
             INSERT INTO podcasts VALUES
              (1, 'a', 'Dan Carlin''s Hardcore History', 'https://www.dancarlin.com/', 200, 173001861,
               'Dan Carlin', 70, 9, 'History', NULL, 1136073600, 1735689600),
              (2, 'b', 'Small Show', 'https://small.libsyn.com/', 200, '', 'Someone', 500, 4, '', '',
               '', ''),
              (3, 'c', 'Gone', 'https://gone.example/', 404, 1, 'X', 9, 9, '', NULL, 0, 0),
              (4, 'd', '', 'https://empty.example/', 200, 1, 'X', 9, 9, '', NULL, 0, 0),
              (5, 'e', 'Copy', 'https://copy.example/', 200, 1, 'X', 9, 9, '', 1, 0, 0),
              (6, 'f', 'Broken', '', 200, 1, '', 0, 29, '', NULL, 0, 0),
              (7, 'g', 'Unpopular', '', 200, 1, '', 0, 1, '', NULL, 0, 0),
              (8, 'h', 'Dan Carlin''s Hardcore History', 'https://www.dancarlin.com/', 200, '',
               'Dan Carlin', 75, 9, 'History', '', 1136073600, 1735689600),
              (9, 'i', 'Endless Generated Show', 'https://generated.example/', 200, 5, 'Bot',
               9000, 9, '', '', 1640995200, 1735689600);",
        )
        .unwrap();
        db
    }

    #[test]
    fn podcasts_are_read_from_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let db = feeds(dir.path());
        let podcasts = read_podcasts(&db, 4, 10).unwrap();
        let titles: Vec<&str> = podcasts.iter().map(|p| p.title.as_str()).collect();
        // The show listed twice is kept once, with its Apple Podcasts
        // listing; years running outrank thousands of episodes.
        assert_eq!(
            titles,
            [
                "Dan Carlin's Hardcore History",
                "Endless Generated Show",
                "Small Show"
            ]
        );
        assert_eq!(podcasts[0].years, 19);
        let history = podcasts[0].clone().into_article();
        assert_eq!(history.item.as_deref(), Some("1"));
        assert_eq!(history.site.as_deref(), Some("dancarlin.com"));
        assert_eq!(
            history.description.as_deref(),
            Some("Podcast by Dan Carlin · History")
        );
        assert_eq!(history.aliases, ["Dan Carlin podcast"]);
        assert_eq!(history.profiles[0].service, "apple-podcasts");
        assert_eq!(history.profiles[0].id, "173001861");
        assert!(history.views > podcasts[1].views());
        let small = podcasts[2].clone().into_article();
        assert_eq!(small.site, None);
        assert!(small.profiles.is_empty());
        assert_eq!(read_podcasts(&db, 4, 1).unwrap().len(), 1);
        // The Apple Podcasts id comes back from the file.
        let file = dir.path().join("podcasts.tsv.gz");
        crate::articles::write_articles_file(&file, &[history]).unwrap();
        let reader = crate::open_maybe_gz(&file).unwrap();
        let lines = std::io::BufRead::lines(reader).map(Result::unwrap);
        let read: Vec<Article> = plumb_core::article::articles_of(lines)
            .map(|(_, a)| a.unwrap())
            .collect();
        assert_eq!(read[0].profiles[0].id, "173001861");
    }

    #[test]
    fn the_database_is_unpacked() {
        let dir = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let db = feeds(source.path());
        let tgz = dir.path().join("feeds.tgz");
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            std::fs::File::create(&tgz).unwrap(),
            flate2::Compression::fast(),
        ));
        builder
            .append_path_with_name(&db, format!("./{FEEDS_DB}"))
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        let out = tempfile::tempdir().unwrap();
        let unpacked = unpack_feeds(&tgz, out.path()).unwrap();
        assert_eq!(read_podcasts(&unpacked, 4, 10).unwrap().len(), 3);
    }
}
