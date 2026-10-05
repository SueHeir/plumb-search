//! Wikipedia articles ([`plumb_core::article`]) from Wikimedia's public
//! dumps, without crawling Wikipedia:
//!
//! - `{lang}wiki-latest-page.sql.gz`: every page's id, title and whether
//!   it is a redirect;
//! - `{lang}wiki-latest-page_props.sql.gz`: each article's Wikidata item,
//!   short description, and whether it is a disambiguation page;
//! - `{lang}wiki-latest-redirect.sql.gz`: which article each redirect
//!   leads to;
//! - a few days of `pageview_complete` files: how often each page was
//!   read, the articles' popularity, as Tranco's list is for sites.
//!
//! Every article read at least once in those days is kept (disambiguation
//! pages are not), most read first, with its [`MAX_ALIASES`] most read
//! redirects as aliases. An item's official website, from Wikidata's file
//! of official websites ([`crate::wikidata`]), is noted as the article's
//! site.
//!
//! Anyone can make the same file from the same dumps, so a node can check
//! another's.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use plumb_core::article::{
    write_article, Article, ARTICLES_HEADER, MAX_ALIASES, MAX_ARTICLE_DESCRIPTION_CHARS,
};
use plumb_core::truncate_chars;
use tracing::info;

use crate::download::{download_to_file, part_path};
use crate::open_maybe_gz;

/// Where Wikimedia's dumps are.
pub const DUMPS_URL: &str = "https://dumps.wikimedia.org";

/// A value in a row of an SQL dump.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue<'a> {
    Null,
    /// A number, as written.
    Number(&'a str),
    /// A string, unescaped.
    Text(Cow<'a, str>),
}

impl SqlValue<'_> {
    fn as_u64(&self) -> Option<u64> {
        match self {
            SqlValue::Number(n) => n.parse().ok(),
            SqlValue::Text(t) => t.parse().ok(),
            SqlValue::Null => None,
        }
    }

    fn as_str(&self) -> &str {
        match self {
            SqlValue::Number(n) => n,
            SqlValue::Text(t) => t,
            SqlValue::Null => "",
        }
    }
}

/// English Wikipedia's front page, which is in the article namespace.
const MAIN_PAGE: &str = "Main Page";

/// Reads the rows of `table` in a MySQL dump (as `mysqldump` writes
/// Wikimedia's), calling `row` with each row's values and the table's
/// column names, from its `CREATE TABLE`.
pub fn for_each_row(
    reader: impl BufRead,
    table: &str,
    mut row: impl FnMut(&[String], &[SqlValue<'_>]) -> Result<()>,
) -> Result<u64> {
    let create = format!("CREATE TABLE `{table}` (");
    // Rows follow on the same line (`VALUES (..),(..);`) or, in newer
    // dumps, one per line after a bare `VALUES` line, until one ends in `;`.
    let insert = format!("INSERT INTO `{table}` VALUES");
    let mut columns: Vec<String> = Vec::new();
    let mut in_create = false;
    let mut in_insert = false;
    let mut rows = 0u64;
    for line in split_lines(reader) {
        let line = line?;
        if in_create {
            let trimmed = line.trim_ascii_start();
            if let Some(rest) = trimmed.strip_prefix(b"`") {
                if let Some(end) = rest.iter().position(|&b| b == b'`') {
                    columns.push(String::from_utf8_lossy(&rest[..end]).into_owned());
                }
            } else if trimmed.starts_with(b")") {
                in_create = false;
            }
            continue;
        }
        if line.starts_with(create.as_bytes()) {
            in_create = true;
            columns.clear();
            continue;
        }
        let mut rest: &[u8] = if let Some(rest) = line.strip_prefix(insert.as_bytes()) {
            in_insert = true;
            rest
        } else if in_insert {
            &line
        } else {
            continue;
        };
        if columns.is_empty() {
            bail!("the dump has rows of `{table}` before its CREATE TABLE");
        }
        let mut values: Vec<SqlValue<'_>> = Vec::with_capacity(columns.len());
        loop {
            rest = rest.trim_ascii_start();
            match rest.first() {
                Some(b'(') => rest = &rest[1..],
                Some(b',') => {
                    rest = &rest[1..];
                    continue;
                }
                Some(b';') => {
                    in_insert = false;
                    rest = &rest[1..];
                    continue;
                }
                None => break,
                Some(_) => bail!("unexpected text in an INSERT of `{table}`"),
            }
            values.clear();
            loop {
                rest = rest.trim_ascii_start();
                let (value, after) = parse_value(rest)?;
                values.push(value);
                rest = after.trim_ascii_start();
                match rest.first() {
                    Some(b',') => rest = &rest[1..],
                    Some(b')') => {
                        rest = &rest[1..];
                        break;
                    }
                    _ => bail!("a row of `{table}` does not end"),
                }
            }
            rows += 1;
            row(&columns, &values)?;
        }
    }
    Ok(rows)
}

/// Lines of `reader` as bytes, without the line break. Dumps are UTF-8,
/// but one bad byte must not stop a whole dump.
fn split_lines(mut reader: impl BufRead) -> impl Iterator<Item = Result<Vec<u8>>> {
    std::iter::from_fn(move || {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => None,
            Ok(_) => {
                while matches!(line.last(), Some(b'\n' | b'\r')) {
                    line.pop();
                }
                Some(Ok(line))
            }
            Err(err) => Some(Err(anyhow::Error::from(err).context("reading a dump"))),
        }
    })
}

fn parse_value(text: &[u8]) -> Result<(SqlValue<'_>, &[u8])> {
    match text.first() {
        Some(b'\'') => {
            let body = &text[1..];
            let mut i = 0;
            let mut escaped = false;
            while i < body.len() {
                match body[i] {
                    b'\\' => {
                        escaped = true;
                        i += 2;
                    }
                    b'\'' => break,
                    _ => i += 1,
                }
            }
            if i >= body.len() {
                bail!("a string in the dump does not end");
            }
            let raw = &body[..i];
            let value = if escaped {
                Cow::Owned(unescape(raw))
            } else {
                String::from_utf8_lossy(raw)
            };
            Ok((SqlValue::Text(value), &body[i + 1..]))
        }
        Some(_) => {
            let end = text
                .iter()
                .position(|&b| b == b',' || b == b')')
                .unwrap_or(text.len());
            let word = std::str::from_utf8(&text[..end])
                .context("a value in the dump is not text")?
                .trim();
            let value = if word.eq_ignore_ascii_case("NULL") {
                SqlValue::Null
            } else {
                SqlValue::Number(word)
            };
            Ok((value, &text[end..]))
        }
        None => bail!("a row in the dump ends early"),
    }
}

fn unescape(raw: &[u8]) -> String {
    let mut out = Vec::with_capacity(raw.len());
    let mut bytes = raw.iter();
    while let Some(&b) = bytes.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match bytes.next() {
            Some(b'n') => out.push(b'\n'),
            Some(b't') => out.push(b'\t'),
            Some(b'r') => out.push(b'\r'),
            Some(b'0') => out.push(0),
            Some(b'Z') => out.push(0x1a),
            Some(&other) => out.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn column(columns: &[String], name: &str) -> Result<usize> {
    columns
        .iter()
        .position(|c| c == name)
        .with_context(|| format!("the dump has no column `{name}`"))
}

/// The dump files [`build_articles`] reads.
#[derive(Debug, Clone)]
pub struct ArticleDumps {
    pub page: PathBuf,
    pub page_props: PathBuf,
    pub redirect: PathBuf,
    /// `pageview_complete` files, one per day.
    pub pageviews: Vec<PathBuf>,
    /// Wikidata's official websites ([`crate::wikidata`]), when at hand.
    pub official_sites: Option<PathBuf>,
}

#[derive(Default)]
struct Page {
    title: Box<str>,
    description: Option<Box<str>>,
    item: Option<Box<str>>,
    disambiguation: bool,
    views: u64,
    aliases: Vec<(u64, Box<str>)>,
}

/// Reads `dumps` of Wikipedia in `lang` and returns its articles read at
/// least once, most read first.
pub fn build_articles(lang: &str, dumps: &ArticleDumps) -> Result<Vec<Article>> {
    // Articles and redirects in the main namespace, by page id.
    let mut pages: HashMap<u32, Page> = HashMap::new();
    let mut redirects: HashMap<u32, (Box<str>, u64)> = HashMap::new();
    let mut cols: Option<[usize; 4]> = None;
    let rows = for_each_row(open_maybe_gz(&dumps.page)?, "page", {
        |columns, values| {
            let [id, ns, title, is_redirect] = match cols {
                Some(c) => c,
                None => *cols.insert([
                    column(columns, "page_id")?,
                    column(columns, "page_namespace")?,
                    column(columns, "page_title")?,
                    column(columns, "page_is_redirect")?,
                ]),
            };
            if values.get(ns).and_then(SqlValue::as_u64) != Some(0) {
                return Ok(());
            }
            let Some(id) = values.get(id).and_then(SqlValue::as_u64) else {
                return Ok(());
            };
            let Ok(id) = u32::try_from(id) else {
                return Ok(());
            };
            let title: Box<str> = values
                .get(title)
                .map_or("", SqlValue::as_str)
                .replace('_', " ")
                .into();
            if values.get(is_redirect).and_then(SqlValue::as_u64) == Some(1) {
                redirects.insert(id, (title, 0));
            } else {
                pages.insert(
                    id,
                    Page {
                        title,
                        ..Page::default()
                    },
                );
            }
            Ok(())
        }
    })
    .with_context(|| format!("reading {}", dumps.page.display()))?;
    info!(
        "pages     {rows:>10} rows: {} articles, {} redirects",
        pages.len(),
        redirects.len()
    );

    let mut cols: Option<[usize; 3]> = None;
    let rows = for_each_row(open_maybe_gz(&dumps.page_props)?, "page_props", {
        |columns, values| {
            let [page, name, value] = match cols {
                Some(c) => c,
                None => *cols.insert([
                    column(columns, "pp_page")?,
                    column(columns, "pp_propname")?,
                    column(columns, "pp_value")?,
                ]),
            };
            let Some(id) = values.get(page).and_then(SqlValue::as_u64) else {
                return Ok(());
            };
            let Some(page) = u32::try_from(id).ok().and_then(|id| pages.get_mut(&id)) else {
                return Ok(());
            };
            let value = values.get(value).map_or("", SqlValue::as_str).trim();
            match values.get(name).map_or("", SqlValue::as_str) {
                "wikibase_item" if !value.is_empty() => page.item = Some(value.into()),
                "wikibase-shortdesc" if !value.is_empty() => {
                    page.description =
                        Some(truncate_chars(value, MAX_ARTICLE_DESCRIPTION_CHARS).into())
                }
                "disambiguation" => page.disambiguation = true,
                _ => {}
            }
            Ok(())
        }
    })
    .with_context(|| format!("reading {}", dumps.page_props.display()))?;
    info!("page props {rows:>9} rows");

    let project = format!("{lang}.wikipedia");
    for path in &dumps.pageviews {
        let (lines, counted) = add_pageviews(open_pageviews(path)?, &project, &mut |id, views| {
            if let Some(page) = pages.get_mut(&id) {
                page.views += views;
            } else if let Some((_, redirect_views)) = redirects.get_mut(&id) {
                *redirect_views += views;
            }
        })
        .with_context(|| format!("reading {}", path.display()))?;
        info!(
            "pageviews {lines:>10} lines, {counted} of {project} ({})",
            path.display()
        );
    }

    // Titles of articles that were read, for the redirects to find.
    let by_title: HashMap<Box<str>, u32> = pages
        .iter()
        .filter(|(_, page)| page.views > 0 && !page.disambiguation)
        .map(|(&id, page)| (page.title.clone(), id))
        .collect();
    let mut cols: Option<[usize; 5]> = None;
    let rows = for_each_row(open_maybe_gz(&dumps.redirect)?, "redirect", {
        |columns, values| {
            let [from, ns, title, interwiki, fragment] = match cols {
                Some(c) => c,
                None => *cols.insert([
                    column(columns, "rd_from")?,
                    column(columns, "rd_namespace")?,
                    column(columns, "rd_title")?,
                    column(columns, "rd_interwiki")?,
                    column(columns, "rd_fragment")?,
                ]),
            };
            if values.get(ns).and_then(SqlValue::as_u64) != Some(0)
                || !values.get(interwiki).map_or("", SqlValue::as_str).is_empty()
                // A redirect to a section names the section, not the article.
                || !values.get(fragment).map_or("", SqlValue::as_str).is_empty()
            {
                return Ok(());
            }
            let Some(from) = values
                .get(from)
                .and_then(SqlValue::as_u64)
                .and_then(|id| u32::try_from(id).ok())
            else {
                return Ok(());
            };
            let Some((name, views)) = redirects.remove(&from) else {
                return Ok(());
            };
            if views == 0 {
                return Ok(());
            }
            let target = values
                .get(title)
                .map_or("", SqlValue::as_str)
                .replace('_', " ");
            if let Some(page) = by_title
                .get(target.as_str())
                .and_then(|id| pages.get_mut(id))
            {
                page.aliases.push((views, name));
            }
            Ok(())
        }
    })
    .with_context(|| format!("reading {}", dumps.redirect.display()))?;
    info!("redirects {rows:>10} rows");
    drop(by_title);

    let sites = match &dumps.official_sites {
        Some(path) => official_site_by_item(path)?,
        None => HashMap::new(),
    };

    let mut articles: Vec<Article> = pages
        .into_values()
        // The main page is the most read page but no article.
        .filter(|page| page.views > 0 && !page.disambiguation && &*page.title != MAIN_PAGE)
        .map(|mut page| {
            page.aliases
                .sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            let title_key = plumb_core::normalize_text(&page.title);
            let mut aliases: Vec<String> = Vec::new();
            for (_, alias) in page.aliases {
                if aliases.len() == MAX_ALIASES {
                    break;
                }
                // "Marie curie" adds nothing to "Marie Curie".
                let key = plumb_core::normalize_text(&alias);
                if key == title_key || aliases.iter().any(|a| plumb_core::normalize_text(a) == key)
                {
                    continue;
                }
                aliases.push(alias.into());
            }
            let site = page
                .item
                .as_deref()
                .and_then(|item| sites.get(item))
                .cloned();
            Article {
                title: page.title.into(),
                description: page.description.map(Into::into),
                item: page.item.map(Into::into),
                site,
                views: page.views,
                aliases,
                profiles: Vec::new(),
            }
        })
        .collect();
    articles.sort_by(|a, b| b.views.cmp(&a.views).then_with(|| a.title.cmp(&b.title)));
    Ok(articles)
}

/// Adds up the views of `project`'s pages in a `pageview_complete` file,
/// whose lines are `project title page_id access views hourly`. Returns the
/// lines read and the ones of `project`.
pub fn add_pageviews(
    reader: impl BufRead,
    project: &str,
    add: &mut impl FnMut(u32, u64),
) -> Result<(u64, u64)> {
    let mut lines = 0;
    let mut counted = 0;
    for line in split_lines(reader) {
        let line = line?;
        lines += 1;
        let mut fields = line.split(|&b| b == b' ');
        if fields.next() != Some(project.as_bytes()) {
            continue;
        }
        let (_title, id, _access, views) =
            (fields.next(), fields.next(), fields.next(), fields.next());
        let parse = |field: Option<&[u8]>| {
            field
                .and_then(|f| std::str::from_utf8(f).ok())
                .and_then(|f| f.parse::<u64>().ok())
        };
        let (Some(id), Some(views)) = (parse(id), parse(views)) else {
            continue;
        };
        let Ok(id) = u32::try_from(id) else {
            continue;
        };
        counted += 1;
        add(id, views);
    }
    Ok((lines, counted))
}

/// Opens a pageviews file, bzip2 as Wikimedia publishes them, or gzip or
/// plain.
fn open_pageviews(path: &Path) -> Result<Box<dyn BufRead>> {
    if path.extension().is_some_and(|e| e == "bz2") {
        let file =
            std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        Ok(Box::new(BufReader::with_capacity(
            1 << 16,
            bzip2::read::MultiBzDecoder::new(BufReader::new(file)),
        )))
    } else {
        open_maybe_gz(path)
    }
}

/// Official websites' registrable domains by Wikidata item. An item with
/// several keeps the first.
fn official_site_by_item(path: &Path) -> Result<HashMap<String, String>> {
    let mut sites = HashMap::new();
    for site in crate::load_wikidata_official_sites(path)? {
        sites.entry(site.item).or_insert(site.domain);
    }
    Ok(sites)
}

/// Writes `articles` as an articles file at `dest`, gzipped, by way of a
/// part file so `dest` only ever holds a whole file.
pub fn write_articles_file(dest: &Path, articles: &[Article]) -> Result<()> {
    let part = part_path(dest);
    let file =
        std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?;
    let mut out = flate2::write::GzEncoder::new(
        std::io::BufWriter::new(file),
        flate2::Compression::default(),
    );
    out.write_all(ARTICLES_HEADER.as_bytes())?;
    for article in articles {
        write_article(&mut out, article)?;
    }
    out.finish()?
        .into_inner()
        .map_err(|e| e.into_error())?
        .sync_all()?;
    std::fs::rename(&part, dest)
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
    Ok(())
}

/// The address of the latest dump of `table` of Wikipedia in `lang`.
pub fn dump_url(lang: &str, table: &str) -> String {
    format!("{DUMPS_URL}/{lang}wiki/latest/{lang}wiki-latest-{table}.sql.gz")
}

/// The address of the `pageview_complete` file of `day` (`2026-09-30`).
pub fn pageviews_url(day: &str) -> Result<String> {
    let parts: Vec<&str> = day.split('-').collect();
    let [year, month, date] = parts[..] else {
        bail!("expected a day like 2026-09-30, got {day:?}");
    };
    Ok(format!(
        "{DUMPS_URL}/other/pageview_complete/{year}/{year}-{month}/pageviews-{year}{month}{date}-user.bz2"
    ))
}

/// The `days` days before `today` minus two (Wikimedia publishes a day's
/// views a day or so later), as `YYYY-MM-DD`, newest first.
pub fn pageview_days(today_unix: u64, days: u32) -> Vec<String> {
    let today = today_unix / 86_400;
    (0..u64::from(days))
        .map(|n| civil_date(today - 2 - n))
        .collect()
}

/// `days` since 1970-01-01 as `YYYY-MM-DD` (Howard Hinnant's algorithm).
fn civil_date(days: u64) -> String {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Downloads what [`build_articles`] reads for `lang` into `dir`, keeping
/// files already there: a dump younger than `keep_days`, and any
/// pageviews file (a past day's views never change).
pub async fn download_article_dumps(
    client: &reqwest::Client,
    dir: &Path,
    lang: &str,
    pageview_days: &[String],
    keep_days: u64,
) -> Result<ArticleDumps> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let get = async |url: String, keep: Option<u64>| -> Result<PathBuf> {
        let name = crate::download::file_name_from_url(&url)?;
        let dest = dir.join(name);
        let fresh = std::fs::metadata(&dest)
            .ok()
            .is_some_and(|meta| match keep {
                None => true,
                Some(days) => meta
                    .modified()
                    .ok()
                    .and_then(|m| m.elapsed().ok())
                    .is_some_and(|age| age.as_secs() < days * 86_400),
            });
        if fresh {
            info!("keeping {}", dest.display());
        } else {
            info!("downloading {url}");
            download_to_file(client, &url, &dest).await?;
        }
        Ok(dest)
    };
    let page = get(dump_url(lang, "page"), Some(keep_days)).await?;
    let page_props = get(dump_url(lang, "page_props"), Some(keep_days)).await?;
    let redirect = get(dump_url(lang, "redirect"), Some(keep_days)).await?;
    let mut pageviews = Vec::new();
    for day in pageview_days {
        pageviews.push(get(pageviews_url(day)?, None).await?);
    }
    Ok(ArticleDumps {
        page,
        page_props,
        redirect,
        pageviews,
        official_sites: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "\
-- MySQL dump
CREATE TABLE `page` (
  `page_id` int(8) unsigned NOT NULL AUTO_INCREMENT,
  `page_namespace` int(11) NOT NULL DEFAULT 0,
  `page_title` varbinary(255) NOT NULL DEFAULT '',
  `page_is_redirect` tinyint(1) unsigned NOT NULL DEFAULT 0,
  `page_len` int(8) unsigned NOT NULL DEFAULT 0,
  PRIMARY KEY (`page_id`)
) ENGINE=InnoDB;
INSERT INTO `page` VALUES (1,0,'Marie_Curie',0,100),(2,0,'Madame_Curie',1,10),(3,0,'Marie_curie',1,10),(4,1,'Marie_Curie',0,5),(5,0,'Curie',0,9),(6,0,'Python_(programming_language)',0,99);
INSERT INTO `page` VALUES (7,0,'Pierre_Curie',0,50),(8,0,'O\\'Brien',0,7),(9,0,'Unread',0,1),(10,0,'Python_language',1,3);
";

    const PROPS: &str = "\
CREATE TABLE `page_props` (
  `pp_page` int(10) unsigned NOT NULL,
  `pp_propname` varbinary(60) NOT NULL,
  `pp_value` blob NOT NULL,
  `pp_sortkey` float DEFAULT NULL
) ENGINE=InnoDB;
INSERT INTO `page_props` VALUES (1,'wikibase_item','Q7186',NULL),(1,'wikibase-shortdesc','Polish-French physicist and chemist (1867–1934)',NULL),(5,'disambiguation','',NULL),(6,'wikibase_item','Q28865',NULL),(6,'wikibase-shortdesc','General-purpose programming language',NULL);
";

    const REDIRECT: &str = "\
CREATE TABLE `redirect` (
  `rd_from` int(8) unsigned NOT NULL DEFAULT 0,
  `rd_namespace` int(11) NOT NULL DEFAULT 0,
  `rd_title` varbinary(255) NOT NULL DEFAULT '',
  `rd_interwiki` varbinary(32) DEFAULT NULL,
  `rd_fragment` varbinary(255) DEFAULT NULL
) ENGINE=InnoDB;
INSERT INTO `redirect` VALUES (2,0,'Marie_Curie','',''),(3,0,'Marie_Curie','',''),(10,0,'Python_(programming_language)','','Syntax');
";

    const VIEWS: &str = "\
en.wikipedia Marie_Curie 1 desktop 500 A1
en.wikipedia Marie_Curie 1 mobile-web 300 A1
en.wikipedia Madame_Curie 2 desktop 20 A1
en.wikipedia Marie_curie 3 desktop 5 A1
en.wikipedia Curie 5 desktop 99 A1
en.wikipedia Python_(programming_language) 6 desktop 900 A1
en.wikipedia Pierre_Curie 7 desktop 40 A1
en.wikipedia O'Brien 8 desktop 2 A1
en.wikipedia Python_language 10 desktop 50 A1
en.wikipedia Main_Page null desktop 9999 A1
de.wikipedia Marie_Curie 1 desktop 70000 A1
";

    fn dumps(dir: &Path) -> ArticleDumps {
        let write = |name: &str, text: &str| {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            path
        };
        let sites = write(
            "sites.tsv",
            "item\tlabel\twebsite\nQ28865\tPython\thttps://www.python.org/\n",
        );
        ArticleDumps {
            page: write("page.sql", PAGE),
            page_props: write("page_props.sql", PROPS),
            redirect: write("redirect.sql", REDIRECT),
            pageviews: vec![write("views.txt", VIEWS)],
            official_sites: Some(sites),
        }
    }

    #[test]
    fn parses_rows_with_escapes() {
        let mut titles = Vec::new();
        for_each_row(PAGE.as_bytes(), "page", |columns, values| {
            assert_eq!(columns.len(), 5);
            titles.push(values[2].as_str().to_string());
            Ok(())
        })
        .unwrap();
        assert_eq!(titles.len(), 10);
        assert!(titles.contains(&"O'Brien".to_string()));
    }

    #[test]
    fn parses_rows_on_lines_of_their_own() {
        // The layout of the 2026 dumps.
        let dump = "CREATE TABLE `page` (
  `page_id` int(8) unsigned NOT NULL,
  `page_title` varbinary(255) NOT NULL
) ENGINE=InnoDB;
INSERT INTO `page` VALUES
(10,'AccessibleComputing'),
(12,'Anarchism'),
(13,'It\\'s; here');
INSERT INTO `page` VALUES (14,'Next'),(15,'Last');
UNLOCK TABLES;
(99,'Not a row')
";
        let mut titles = Vec::new();
        let rows = for_each_row(dump.as_bytes(), "page", |_, values| {
            titles.push(values[1].as_str().to_string());
            Ok(())
        })
        .unwrap();
        assert_eq!(rows, 5);
        assert_eq!(
            titles,
            [
                "AccessibleComputing",
                "Anarchism",
                "It's; here",
                "Next",
                "Last"
            ]
        );
    }

    #[test]
    fn builds_articles_most_read_first() {
        let dir = tempfile::tempdir().unwrap();
        let articles = build_articles("en", &dumps(dir.path())).unwrap();
        let titles: Vec<&str> = articles.iter().map(|a| a.title.as_str()).collect();
        // Curie is a disambiguation page, Unread was never read, the talk
        // page is not an article.
        assert_eq!(
            titles,
            [
                "Python (programming language)",
                "Marie Curie",
                "Pierre Curie",
                "O'Brien"
            ]
        );
        let curie = &articles[1];
        assert_eq!(curie.views, 800);
        assert_eq!(curie.item.as_deref(), Some("Q7186"));
        assert_eq!(
            curie.description.as_deref(),
            Some("Polish-French physicist and chemist (1867–1934)")
        );
        // "Marie curie" only differs in case, so only Madame Curie is kept.
        assert_eq!(curie.aliases, ["Madame Curie"]);
        assert_eq!(curie.site, None);
        let python = &articles[0];
        assert_eq!(python.site.as_deref(), Some("python.org"));
        // A redirect to a section is not an alias.
        assert!(python.aliases.is_empty());
    }

    #[test]
    fn writes_a_readable_file() {
        let dir = tempfile::tempdir().unwrap();
        let articles = build_articles("en", &dumps(dir.path())).unwrap();
        let dest = dir.path().join("wikipedia-en.tsv.gz");
        write_articles_file(&dest, &articles).unwrap();
        let back = plumb_core::article::read_articles(open_maybe_gz(&dest).unwrap(), 2).unwrap();
        assert_eq!(back, articles[..2]);
    }

    #[test]
    fn pageview_urls_and_days() {
        assert_eq!(
            pageviews_url("2026-09-30").unwrap(),
            "https://dumps.wikimedia.org/other/pageview_complete/2026/2026-09/pageviews-20260930-user.bz2"
        );
        // 2026-10-04 00:00 UTC.
        let days = pageview_days(1_791_072_000, 3);
        assert_eq!(days, ["2026-10-02", "2026-10-01", "2026-09-30"]);
        assert_eq!(
            dump_url("en", "page_props"),
            "https://dumps.wikimedia.org/enwiki/latest/enwiki-latest-page_props.sql.gz"
        );
    }
}
