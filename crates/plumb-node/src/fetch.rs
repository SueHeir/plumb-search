//! `plumb fetch-data`: downloads the seed datasets.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use plumb_ingest::{articles, download, facts, intros, kind_sites};
use tracing::{error, info, warn};

use crate::block_on;
use crate::cli::{FetchDataArgs, FetchFactsArgs, FetchPagesArgs, FetchProfilesArgs};

/// Where release names for `--cc-release` are listed. We know of no
/// machine-readable index of releases, so we point people here instead.
const CC_WEB_GRAPHS_PAGE: &str = "https://commoncrawl.org/web-graphs";

/// `--top` of the suggested `plumb ingest`, as in the README. Besides the
/// records kept, it bounds the Common Crawl rows read, and so the memory used.
const SUGGESTED_TOP: usize = 1_000_000;

/// What happened to one dataset.
#[derive(Debug)]
enum Outcome {
    Saved(PathBuf),
    /// Saved by an earlier run within `--keep-days`, so not fetched again.
    Kept(PathBuf),
    Skipped(String),
    Failed(anyhow::Error),
}

/// Makes the GitHub repositories set file `dest`.
fn run_github(args: &FetchPagesArgs, dest: &std::path::Path) -> Result<()> {
    let token = std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty());
    info!(
        "searching GitHub for repositories with at least {} stars ({})",
        args.min_stars,
        if token.is_some() {
            "with a token"
        } else {
            "without a token: 10 searches a minute"
        }
    );
    let client = download::http_client()?;
    let repos = block_on(plumb_ingest::github::fetch_repos(
        &client,
        args.min_stars,
        args.max_repos,
        token.as_deref(),
    ))??;
    if repos.is_empty() {
        bail!("GitHub gave no repositories; nothing was written");
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    articles::write_articles_file(dest, &repos)?;
    let size = std::fs::metadata(dest).map_or(0, |m| m.len());
    info!(
        "wrote {} repositories to {} ({:.1} MB): {} with a description, {} with a homepage",
        repos.len(),
        dest.display(),
        size as f64 / 1e6,
        repos.iter().filter(|r| r.description.is_some()).count(),
        repos.iter().filter(|r| r.site.is_some()).count(),
    );
    Ok(())
}

/// Makes the Stack Overflow questions set file `dest` from Stack Exchange's
/// dump of Stack Overflow's posts (about 20 GB), downloaded into --work
/// unless --posts names it.
fn run_stackoverflow(args: &FetchPagesArgs, dest: &std::path::Path) -> Result<()> {
    use plumb_ingest::stackexchange;
    let posts = match &args.posts {
        Some(posts) => posts.clone(),
        None => fetch_dump(
            args,
            stackexchange::STACKOVERFLOW_POSTS_URL,
            "Stack Overflow's posts",
        )?,
    };
    info!("reading questions from {}", posts.display());
    let questions = stackexchange::read_questions_7z(&posts, args.min_score, args.max_questions)?;
    let questions: Vec<_> = questions
        .into_iter()
        .map(stackexchange::Question::into_article)
        .collect();
    write_set(dest, &questions, "questions")
}

/// Makes the `stackexchange` set file `dest` from the dumps of the Stack
/// Exchange sites in [`plumb_core::stack_exchange::SITES`] (a few GB in
/// all), downloaded into --work one at a time: each site's
/// --max-per-site most viewed questions, all of them together most viewed
/// first. A site whose dump can't be had is left out, with a warning.
fn run_stackexchange(args: &FetchPagesArgs, dest: &std::path::Path) -> Result<()> {
    use plumb_core::stack_exchange::SITES;
    use plumb_ingest::stackexchange;
    let mut questions = Vec::new();
    let mut missing = Vec::new();
    for site in SITES {
        let read = fetch_dump(args, &site.dump_url(), site.name).and_then(|dump| {
            info!("reading {}'s questions from {}", site.name, dump.display());
            let read = stackexchange::read_questions_7z(&dump, args.min_score, args.max_per_site);
            if args.drop_dumps {
                if let Err(err) = std::fs::remove_file(&dump) {
                    warn!("removing {}: {err}", dump.display());
                }
            }
            read
        });
        match read {
            Ok(read) => questions.extend(
                read.into_iter()
                    .map(|question| (question.views, question.into_exchange_article(site))),
            ),
            Err(err) => {
                warn!("leaving out {}: {err:#}", site.name);
                missing.push(site.name);
            }
        }
    }
    if !missing.is_empty() {
        warn!("{} sites left out: {}", missing.len(), missing.join(", "));
    }
    questions.sort_by_key(|(views, _)| std::cmp::Reverse(*views));
    let questions: Vec<_> = questions.into_iter().map(|(_, article)| article).collect();
    if questions.is_empty() {
        bail!("no site's dump gave questions; nothing was written");
    }
    write_set(dest, &questions, "questions")
}

/// Downloads `url` into --work, unless a copy there is younger than
/// --keep-days, and gives its path.
fn fetch_dump(args: &FetchPagesArgs, url: &str, what: &str) -> Result<std::path::PathBuf> {
    let work = args
        .work
        .as_deref()
        .with_context(|| format!("pass --work DIR to download {what} into"))?;
    let path = work.join(download::file_name_from_url(url)?);
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age.as_secs() < args.keep_days * 86_400);
    if fresh {
        info!("keeping {}", path.display());
        return Ok(path);
    }
    info!("downloading {what} from {url}");
    let client = download::http_client()?;
    // Downloaded to a part file, renamed when whole.
    block_on(download::download_to_file(&client, url, &path))??;
    Ok(path)
}

/// Writes `pages` (articles) to `dest` and says how many there were.
fn write_set(
    dest: &std::path::Path,
    pages: &[plumb_core::article::Article],
    what: &str,
) -> Result<()> {
    if pages.is_empty() {
        bail!("no {what} were found; nothing was written");
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    articles::write_articles_file(dest, pages)?;
    let size = std::fs::metadata(dest).map_or(0, |m| m.len());
    info!(
        "wrote {} {what} to {} ({:.1} MB)",
        pages.len(),
        dest.display(),
        size as f64 / 1e6,
    );
    Ok(())
}

/// Makes the books set file `dest` from Open Library's dumps (about 4 GB).
fn run_books(args: &FetchPagesArgs, dest: &std::path::Path) -> Result<()> {
    use plumb_ingest::openlibrary;
    let works = fetch_dump(args, openlibrary::WORKS_URL, "Open Library's works")?;
    let authors = fetch_dump(args, openlibrary::AUTHORS_URL, "Open Library's authors")?;
    let log = fetch_dump(
        args,
        openlibrary::READING_LOG_URL,
        "Open Library's reading log",
    )?;
    let ratings = fetch_dump(args, openlibrary::RATINGS_URL, "Open Library's ratings")?;
    let books = openlibrary::build_books(
        &openlibrary::BookDumps {
            works: &works,
            authors: &authors,
            shelvings: &[&log, &ratings],
        },
        args.min_shelvings,
        args.max_books,
    )?;
    write_set(dest, &books, "books")
}

/// Makes the podcasts set file `dest` from Podcast Index's database
/// (about 1.8 GB, 5 GB unpacked).
fn run_podcasts(args: &FetchPagesArgs, dest: &std::path::Path) -> Result<()> {
    use plumb_ingest::podcasts;
    let db = match &args.podcast_db {
        Some(db) => db.clone(),
        None => {
            let tgz = fetch_dump(args, podcasts::FEEDS_URL, "Podcast Index's database")?;
            let dir = tgz.parent().unwrap_or(std::path::Path::new("."));
            info!("unpacking {}", tgz.display());
            podcasts::unpack_feeds(&tgz, dir)?
        }
    };
    let found = podcasts::read_podcasts(&db, args.min_podcast_score, args.max_podcasts)?;
    let pages: Vec<_> = found
        .into_iter()
        .map(podcasts::Podcast::into_article)
        .collect();
    write_set(dest, &pages, "podcasts")
}

/// Makes the music set file `dest` from MusicBrainz's core dump (about
/// 7 GB, downloaded into --work unless --musicbrainz-dump names it) and
/// ListenBrainz's listener counts, which are kept in --work as they come.
fn run_music(args: &FetchPagesArgs, dest: &std::path::Path) -> Result<()> {
    use plumb_ingest::musicbrainz::{self, Listened, MusicDump, MusicOptions};
    let work = args
        .work
        .as_deref()
        .context("pass --work DIR for MusicBrainz's dump and ListenBrainz's answers")?;
    std::fs::create_dir_all(work).with_context(|| format!("creating {}", work.display()))?;
    let client = download::http_client()?;
    let dump = match &args.musicbrainz_dump {
        Some(dump) => dump.clone(),
        None => {
            let latest_url = format!("{}LATEST", musicbrainz::FULLEXPORT_URL);
            let latest = block_on(async {
                client
                    .get(&latest_url)
                    .send()
                    .await?
                    .error_for_status()?
                    .text()
                    .await
            })?
            .with_context(|| format!("reading {latest_url}"))?;
            let url = format!(
                "{}{}",
                musicbrainz::export_url(&latest)?,
                musicbrainz::CORE_DUMP
            );
            fetch_dump(args, &url, "MusicBrainz's core dump")?
        }
    };
    let tables = musicbrainz::tables_dir(&dump)?;
    let options = MusicOptions {
        max_songs: args.max_songs,
        min_song_releases: args.min_song_releases,
        max_albums: args.max_albums,
        min_listeners: args.min_listeners,
        ..MusicOptions::default()
    };
    let mut music = MusicDump::read(&tables, &options)?;
    let albums = block_on(musicbrainz::fetch_listeners(
        &client,
        Listened::Albums,
        &music.album_mbids(),
        &work.join("listenbrainz-albums.tsv"),
    ))??;
    let canonical = match &args.listenbrainz_canonical {
        Some(path) => path.clone(),
        None => {
            let listing = block_on(async {
                client
                    .get(musicbrainz::CANONICAL_URL)
                    .send()
                    .await?
                    .error_for_status()?
                    .text()
                    .await
            })?
            .with_context(|| format!("reading {}", musicbrainz::CANONICAL_URL))?;
            let url = musicbrainz::canonical_dump_url(&listing)?;
            fetch_dump(args, &url, "ListenBrainz's canonical data dump")?
        }
    };
    let recordings = music.songs_to_ask(&albums, Some(&canonical))?;
    let recordings = block_on(musicbrainz::fetch_listeners(
        &client,
        Listened::Recordings,
        &recordings,
        &work.join("listenbrainz-recordings.tsv"),
    ))??;
    let pages = music.into_articles(&albums, &recordings)?;
    write_set(dest, &pages, "songs and albums")?;
    info!(
        "{} songs, {} with lyrics on Genius",
        pages
            .iter()
            .filter(|p| p
                .description
                .as_deref()
                .is_some_and(|d| d.starts_with("Song")))
            .count(),
        pages
            .iter()
            .filter(|p| p.profiles.iter().any(|p| p.service == "genius-song"))
            .count(),
    );
    Ok(())
}

/// Makes the films set file `dest` from Wikidata's query service.
fn run_films(args: &FetchPagesArgs, dest: &std::path::Path) -> Result<()> {
    let client = download::http_client()?;
    let options = plumb_ingest::films::FilmOptions {
        max_films: args.max_films,
        min_sitelinks: args.min_film_sitelinks,
    };
    let films = block_on(plumb_ingest::films::fetch_films(
        &client,
        download::WIKIDATA_SPARQL_URL,
        download::WikidataPacing::default(),
        &options,
    ))??;
    write_set(dest, &films, "films and shows")
}

/// Docs sites fetched at once; each site's pages are fetched one at a time.
const DOCS_SITES_AT_ONCE: usize = 16;

/// Makes the docs set file `dest` from the docs sites' pages (see
/// [`plumb_core::docs`]). With --work, each site's pages are kept there as
/// `docs-KEY.json` when fetched, and a site already kept is not fetched
/// again.
fn run_docs(args: &FetchPagesArgs, dest: &std::path::Path) -> Result<()> {
    use plumb_core::docs::{DocsSite, DOCS_SITES};
    use plumb_ingest::docs::{docs_articles, sort_docs, FetchedDoc};

    let sites: Vec<&'static DocsSite> = if args.docs_sites.is_empty() {
        DOCS_SITES.iter().collect()
    } else {
        args.docs_sites
            .iter()
            .map(|key| {
                plumb_core::docs::site(key).with_context(|| {
                    let keys: Vec<&str> = DOCS_SITES.iter().map(|site| site.key).collect();
                    format!("unknown docs site {key:?}; there are: {}", keys.join(", "))
                })
            })
            .collect::<Result<_>>()?
    };
    if let Some(work) = &args.work {
        std::fs::create_dir_all(work).with_context(|| format!("creating {}", work.display()))?;
    }
    let max_pages = args.max_docs_per_site;
    let work = args.work.clone();
    let fetched = block_on(async move {
        let cfg = plumb_crawl::CrawlConfig::default();
        let mut running = tokio::task::JoinSet::new();
        let mut done = Vec::new();
        let mut queue = sites.into_iter();
        loop {
            while running.len() < DOCS_SITES_AT_ONCE {
                let Some(site) = queue.next() else { break };
                let cfg = cfg.clone();
                let kept = work
                    .as_ref()
                    .map(|work| work.join(format!("docs-{}.json", site.key)));
                running.spawn(async move {
                    if let Some(docs) = kept.as_deref().and_then(read_kept_docs) {
                        info!(
                            "{}: {} pages kept from an earlier run",
                            site.key,
                            docs.len()
                        );
                        return (site, docs);
                    }
                    let target = plumb_crawl::SitePagesTarget {
                        domain: site.domain.to_string(),
                        roots: site.roots.iter().map(|r| r.to_string()).collect(),
                        sitemaps: site.sitemaps.iter().map(|r| r.to_string()).collect(),
                        index_pages: site.index_pages.iter().map(|r| r.to_string()).collect(),
                        max_pages,
                    };
                    let result = plumb_crawl::fetch_site_pages(&target, &cfg).await;
                    let docs: Vec<FetchedDoc> = result
                        .pages
                        .into_iter()
                        .map(|page| FetchedDoc {
                            url: page.url,
                            title: page.meta.title,
                            description: page.meta.description,
                            text: page.meta.body_text,
                        })
                        .collect();
                    if let Some(kept) = &kept {
                        if !docs.is_empty() {
                            if let Err(err) = write_kept_docs(kept, &docs) {
                                warn!("{}: keeping its pages: {err:#}", site.key);
                            }
                        }
                    }
                    (site, docs)
                });
            }
            match running.join_next().await {
                Some(Ok(site_docs)) => done.push(site_docs),
                Some(Err(err)) => warn!("a docs site's fetch failed: {err}"),
                None => break,
            }
        }
        done
    })?;
    let mut pages = Vec::new();
    for (site, docs) in &fetched {
        let articles = docs_articles(site, docs);
        info!(
            "{}: {} pages of {} fetched",
            site.key,
            articles.len(),
            docs.len()
        );
        pages.extend(articles);
    }
    sort_docs(&mut pages);
    write_set(dest, &pages, "docs pages")
}

/// The pages of a docs site kept at `path` by an earlier run.
fn read_kept_docs(path: &std::path::Path) -> Option<Vec<plumb_ingest::docs::FetchedDoc>> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Keeps the pages `docs` of a docs site at `path`.
fn write_kept_docs(path: &std::path::Path, docs: &[plumb_ingest::docs::FetchedDoc]) -> Result<()> {
    let part = path.with_extension("json.part");
    std::fs::write(&part, serde_json::to_vec(docs)?)?;
    std::fs::rename(&part, path)?;
    Ok(())
}

/// Makes the papers set file `dest` from OpenAlex's API, with free copies
/// from OpenAlex (Unpaywall's data and arXiv) and CORE.
fn run_papers(args: &FetchPagesArgs, dest: &std::path::Path) -> Result<()> {
    let key = std::env::var("OPENALEX_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty());
    info!(
        "asking OpenAlex for works cited at least {} times",
        args.min_citations
    );
    let client = download::http_client()?;
    // With --work, the papers so far are kept there, so a run OpenAlex
    // stops can be carried on.
    let progress = args.work.as_deref().map(|w| w.join("openalex"));
    let fetched = block_on(plumb_ingest::openalex::fetch_papers(
        &client,
        args.min_citations,
        args.max_papers,
        key.as_deref(),
        progress.as_deref(),
    ))??;
    if !fetched.complete {
        warn!(
            "OpenAlex stopped answering: writing the {} most cited papers; run again{} to carry on",
            fetched.papers.len(),
            if progress.is_some() {
                " with the same --work"
            } else {
                " with --work DIR"
            }
        );
    }
    let mut papers = fetched.papers;
    // CORE's repositories give free copies of papers OpenAlex knows none
    // of; with --work, what CORE answered is kept there for later runs.
    let core_key = std::env::var("CORE_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty());
    let core_cache = args.work.as_deref().map(|w| w.join("core"));
    if core_key.is_some() || core_cache.is_some() {
        // CORE only adds to the papers, so a CORE that fails loses none.
        match block_on(plumb_ingest::core_ac::fill_free_copies(
            &client,
            core_key.as_deref(),
            &mut papers,
            args.max_core_requests,
            core_cache.as_deref(),
        ))? {
            Ok(filled) => info!(
                "CORE gave {} papers a free copy ({} requests); {} not asked about yet{}",
                filled.found,
                filled.requests,
                filled.left,
                if core_key.is_some() {
                    ""
                } else {
                    " (set CORE_API_KEY to ask)"
                }
            ),
            Err(err) => warn!("asking CORE for free copies: {err:#}; writing the papers without"),
        }
    }
    let free = papers.iter().filter(|p| p.website.is_some()).count();
    info!("{free} of {} papers have a free copy", papers.len());
    write_set(dest, &papers, "papers")
}

/// Makes the packages set file `dest` from ecosyste.ms's lists of the
/// registries' packages.
fn run_packages(args: &FetchPagesArgs, dest: &std::path::Path) -> Result<()> {
    let client = download::http_client()?;
    let packages = block_on(plumb_ingest::packages::fetch_packages(
        &client,
        &args.registries,
        args.max_per_registry,
    ))??;
    write_set(dest, &packages, "packages")
}

/// `plumb fetch-profiles`: adds Wikidata's official profiles, and official
/// websites that are part of another site, to the English Wikipedia
/// articles file.
pub fn run_profiles(args: FetchProfilesArgs) -> Result<()> {
    let path = match (&args.articles, &args.data) {
        (Some(path), _) => path.clone(),
        (None, Some(data)) => crate::pages::SetInfo::find("wikipedia-en")
            .context("no English Wikipedia set")?
            .file(data),
        (None, None) => bail!("pass --data DIR or --articles PATH"),
    };
    if !path.is_file() {
        bail!(
            "{} is not there; make it with plumb fetch-pages first",
            path.display()
        );
    }
    let client = download::http_client()?;
    let profiles = block_on(plumb_ingest::profiles::fetch_profiles(
        &client,
        download::WIKIDATA_SPARQL_URL,
        download::WikidataPacing::default(),
    ))??;
    info!("Wikidata has profiles for {} items", profiles.len());
    let websites = block_on(plumb_ingest::profiles::fetch_websites(
        &client,
        download::WIKIDATA_SPARQL_URL,
        download::WikidataPacing::default(),
    ))??;
    info!(
        "Wikidata has official websites for {} items",
        websites.len()
    );
    // The items without an article, as a set of their own beside it.
    let with_articles = plumb_ingest::profiles::items_in_file(&path)?;
    let items = block_on(plumb_ingest::profiles::fetch_profile_items(
        &client,
        download::WIKIDATA_SPARQL_URL,
        download::WikidataPacing::default(),
        &profiles,
        &websites,
        &with_articles,
    ))??;
    let items_path = path.with_file_name(format!("{}.tsv.gz", plumb_index::pages::WIKIDATA_SET));
    plumb_ingest::articles::write_articles_file(&items_path, &items)?;
    info!(
        "wrote {} items with profiles and no article to {}",
        items.len(),
        items_path.display()
    );
    let added = plumb_ingest::profiles::add_profiles_to_file(&path, &profiles, &websites)?;
    info!(
        "{}: {} of {} articles have profiles, {} in all; {} link a website on their site",
        path.display(),
        added.with_profiles,
        added.articles,
        added.profiles,
        added.websites
    );
    Ok(())
}

/// `plumb fetch-facts`: adds facts from Wikidata to an articles file.
pub fn run_facts(args: FetchFactsArgs) -> Result<()> {
    let path = match (&args.articles, &args.data) {
        (Some(path), _) => path.clone(),
        (None, Some(data)) => crate::pages::SetInfo::find("wikipedia-en")
            .context("no English Wikipedia set")?
            .file(data),
        (None, None) => bail!("pass --data DIR or --articles PATH"),
    };
    if !path.is_file() {
        bail!(
            "{} is not there; make it with plumb fetch-pages first",
            path.display()
        );
    }
    let wanted = plumb_ingest::profiles::items_in_order(&path)?;
    info!("{} articles have a Wikidata item", wanted.len());
    let client = download::http_client()?;
    let facts = block_on(plumb_ingest::item_facts::fetch_facts(
        &client,
        download::WIKIDATA_SPARQL_URL,
        download::WikidataPacing::default(),
        &wanted,
    ))??;
    let added = plumb_ingest::item_facts::add_facts_to_file(&path, &facts)?;
    info!(
        "{}: {} of {} articles have facts, {} in all",
        path.display(),
        added.with_facts,
        added.articles,
        added.facts
    );
    Ok(())
}

/// Makes the places set file `dest` from an OpenStreetMap extract, by
/// default the whole planet (about 90 GB), downloaded into --work.
fn run_places(args: &FetchPagesArgs, dest: &std::path::Path) -> Result<()> {
    use plumb_ingest::osm;
    let pbf = match &args.osm {
        Some(pbf) => pbf.clone(),
        None => fetch_dump(args, osm::PLANET_URL, "OpenStreetMap's planet file")?,
    };
    let places = osm::read_places(&pbf)?;
    if places.is_empty() {
        bail!(
            "no places were found in {}; nothing was written",
            pbf.display()
        );
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    osm::write_places_file(dest, &places)?;
    let size = std::fs::metadata(dest).map_or(0, |m| m.len());
    let towns = places.iter().filter(|p| p.is_town()).count();
    let with_site = places.iter().filter(|p| p.website.is_some()).count();
    let with_country = places.iter().filter(|p| p.country.is_some()).count();
    info!(
        "wrote {} places to {} ({:.1} MB): {towns} towns, {with_site} with a website, \
         {with_country} with a country",
        places.len(),
        dest.display(),
        size as f64 / 1e6,
    );
    Ok(())
}

/// `plumb fetch-pages`: makes a page set file.
pub fn run_pages(args: FetchPagesArgs) -> Result<()> {
    let Some(set) = crate::pages::SetInfo::find(&args.set) else {
        bail!(
            "unknown page set {:?}; there are: {}",
            args.set,
            crate::pages::SETS
                .iter()
                .map(|s| s.id)
                .collect::<Vec<_>>()
                .join(", ")
        );
    };
    let dest = match (&args.out, &args.data) {
        (Some(out), _) => out.clone(),
        (None, Some(data)) => set.file(data),
        (None, None) => bail!("pass --data DIR or --out PATH"),
    };
    if set.id == plumb_index::pages::GITHUB_SET {
        return run_github(&args, &dest);
    }
    if set.id == plumb_index::pages::STACKOVERFLOW_SET {
        return run_stackoverflow(&args, &dest);
    }
    if set.id == plumb_index::pages::STACKEXCHANGE_SET {
        return run_stackexchange(&args, &dest);
    }
    if set.id == plumb_index::pages::BOOKS_SET {
        return run_books(&args, &dest);
    }
    if set.id == plumb_index::pages::PAPERS_SET {
        return run_papers(&args, &dest);
    }
    if set.id == plumb_index::pages::PODCASTS_SET {
        return run_podcasts(&args, &dest);
    }
    if set.id == plumb_index::pages::MUSIC_SET {
        return run_music(&args, &dest);
    }
    if set.id == plumb_index::pages::FILMS_SET {
        return run_films(&args, &dest);
    }
    if set.id == plumb_index::pages::DOCS_SET {
        return run_docs(&args, &dest);
    }
    if set.id == plumb_index::pages::PACKAGES_SET {
        return run_packages(&args, &dest);
    }
    if set.id == plumb_index::places::PLACES_SET {
        return run_places(&args, &dest);
    }
    let Some(lang) = set.id.strip_prefix("wikipedia-") else {
        bail!("fetch-pages cannot make {} yet", set.id);
    };
    let mut dumps = if args.dumps.is_empty() {
        let work = args
            .work
            .as_deref()
            .context("pass --work DIR for Wikipedia's dumps, or --dumps FILES")?;
        let client = download::http_client()?;
        let days = articles::pageview_days(plumb_core::now_unix(), args.pageview_days);
        block_on(articles::download_article_dumps(
            &client,
            work,
            lang,
            &days,
            args.keep_days,
        ))??
    } else {
        let mut files = args.dumps.iter().cloned();
        articles::ArticleDumps {
            page: files.next().context("no page dump")?,
            page_props: files.next().context("no page_props dump")?,
            redirect: files.next().context("no redirect dump")?,
            pageviews: files.collect(),
            official_sites: None,
        }
    };
    dumps.official_sites = args.official_sites.clone();
    let articles = articles::build_articles(lang, &dumps)?;
    if articles.is_empty() {
        bail!("the dumps gave no articles that were read; nothing was written");
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    articles::write_articles_file(&dest, &articles)?;
    let size = std::fs::metadata(&dest).map_or(0, |m| m.len());
    let with_description = articles.iter().filter(|a| a.description.is_some()).count();
    let with_site = articles.iter().filter(|a| a.site.is_some()).count();
    let views: u64 = articles.iter().map(|a| a.views).sum();
    let share = |n: usize| {
        let top: u64 = articles.iter().take(n).map(|a| a.views).sum();
        100.0 * top as f64 / views.max(1) as f64
    };
    info!(
        "wrote {} articles to {} ({:.1} MB): {} with a description, {} with an official site; \
         the top 100,000 have {:.1}% of the views, the top 1,000,000 {:.1}%",
        articles.len(),
        dest.display(),
        size as f64 / 1e6,
        with_description,
        with_site,
        share(100_000),
        share(1_000_000)
    );
    info!(
        "add official profiles to it with: plumb fetch-profiles --articles {}",
        dest.display()
    );
    Ok(())
}

pub fn run(args: FetchDataArgs) -> Result<()> {
    let cc_url = cc_ranks_url(&args)?;
    std::fs::create_dir_all(&args.dir)
        .with_context(|| format!("creating {}", args.dir.display()))?;
    let client = download::http_client()?;
    let kept = |name: &str| recent_file(&args.dir.join(name), args.keep_days);

    let outcomes = block_on(async {
        let tranco = if args.skip_tranco {
            Outcome::Skipped("--skip-tranco".to_string())
        } else if let Some(path) = kept(download::TRANCO_FILE_NAME) {
            Outcome::Kept(path)
        } else {
            info!("downloading the Tranco list");
            outcome(download::download_tranco(&client, &args.dir).await)
        };
        let cc_ranks = match &cc_url {
            Some(url) => {
                info!("downloading Common Crawl domain ranks from {url}");
                outcome(download::download_cc_domain_ranks(&client, url, &args.dir).await)
            }
            None => Outcome::Skipped(format!(
                "pass --cc-release NAME (release names are listed on {CC_WEB_GRAPHS_PAGE}) \
                 or --cc-ranks-url URL"
            )),
        };
        let wikidata = if args.skip_wikidata {
            Outcome::Skipped("--skip-wikidata".to_string())
        } else if let Some(path) = kept(download::WIKIDATA_FILE_NAME) {
            Outcome::Kept(path)
        } else {
            info!(
                "asking Wikidata for official websites of items with at least {} sitelinks",
                args.wikidata_min_sitelinks
            );
            outcome(
                download::download_wikidata_official_sites(
                    &client,
                    &args.dir,
                    args.wikidata_min_sitelinks,
                )
                .await,
            )
        };
        let kind_sites = if args.skip_wikidata {
            Outcome::Skipped("--skip-wikidata".to_string())
        } else if let Some(path) = kept(kind_sites::KIND_SITES_FILE_NAME) {
            Outcome::Kept(path)
        } else {
            outcome(
                kind_sites::download_kind_sites(
                    &client,
                    download::WIKIDATA_SPARQL_URL,
                    &args.dir,
                    download::WikidataPacing::default(),
                )
                .await,
            )
        };
        let sites_files = facts_sources(&args.dir);
        let facts = if args.skip_wikidata {
            Outcome::Skipped("--skip-wikidata".to_string())
        } else if let Some(path) = kept(facts::FACTS_FILE_NAME) {
            Outcome::Kept(path)
        } else if !args.dir.join(download::WIKIDATA_FILE_NAME).is_file() {
            // Facts for the by-kind sites alone would leave out most
            // official sites, and the intros picked from them too.
            Outcome::Skipped("needs the official websites, which are missing".to_string())
        } else {
            outcome(
                facts::download_site_facts(
                    &client,
                    download::WIKIDATA_SPARQL_URL,
                    &args.dir,
                    &sites_files,
                    download::WikidataPacing::default(),
                )
                .await,
            )
        };
        let facts_file = args.dir.join(facts::FACTS_FILE_NAME);
        let intros = if args.skip_wikidata {
            Outcome::Skipped("--skip-wikidata".to_string())
        } else if let Some(path) = kept(intros::INTROS_FILE_NAME) {
            Outcome::Kept(path)
        } else if !facts_file.is_file() {
            Outcome::Skipped("needs the Wikidata facts, which are missing".to_string())
        } else {
            outcome(
                intros::download_wikipedia_intros(
                    &client,
                    download::WIKIDATA_SPARQL_URL,
                    intros::WIKIPEDIA_API_URL,
                    &args.dir,
                    &facts_file,
                    download::WikidataPacing::default(),
                )
                .await,
            )
        };
        [
            ("tranco", tranco),
            ("cc-ranks", cc_ranks),
            ("wikidata", wikidata),
            ("wikidata-kinds", kind_sites),
            ("wikidata-facts", facts),
            ("wikipedia-intros", intros),
        ]
    })?;

    let mut failed = Vec::new();
    for (name, outcome) in &outcomes {
        match outcome {
            Outcome::Saved(path) => println!("{name:<9} saved {}", path.display()),
            Outcome::Kept(path) => println!(
                "{name:<9} kept {} (saved within --keep-days {})",
                path.display(),
                args.keep_days
            ),
            Outcome::Skipped(why) => println!("{name:<9} skipped: {why}"),
            Outcome::Failed(err) => {
                println!("{name:<9} FAILED: {err:#}");
                failed.push(*name);
            }
        }
    }
    if let Some(command) = ingest_hint(&outcomes, &args.dir) {
        println!("next: {command}");
    }
    if !failed.is_empty() {
        bail!("could not download {}", failed.join(", "));
    }
    Ok(())
}

/// The official-site files on disk that facts are fetched for. A file whose
/// download failed this run keeps its earlier copy, which still counts: facts
/// for only the files saved this run would replace a complete facts file with
/// one that leaves the other file's sites out.
fn facts_sources(dir: &Path) -> Vec<PathBuf> {
    [
        download::WIKIDATA_FILE_NAME,
        kind_sites::KIND_SITES_FILE_NAME,
    ]
    .into_iter()
    .map(|name| dir.join(name))
    .filter(|path| path.is_file())
    .collect()
}

/// `path` if it is a file saved within the last `days` days (never for 0).
fn recent_file(path: &Path, days: u64) -> Option<PathBuf> {
    let modified = std::fs::metadata(path)
        .ok()
        .filter(|meta| meta.is_file())?
        .modified()
        .ok()?;
    let age = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    (days > 0 && age < std::time::Duration::from_secs(days * 24 * 60 * 60))
        .then(|| path.to_path_buf())
}

fn outcome(result: Result<PathBuf>) -> Outcome {
    match result {
        Ok(path) => Outcome::Saved(path),
        Err(err) => {
            error!("{err:#}");
            Outcome::Failed(err)
        }
    }
}

/// The Common Crawl ranks URL to fetch, if any. Release names are checked
/// loosely so that a pasted URL or path is caught before it is spliced into
/// another URL.
fn cc_ranks_url(args: &FetchDataArgs) -> Result<Option<String>> {
    if let Some(url) = &args.cc_ranks_url {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            bail!("--cc-ranks-url must be an http(s) URL, got {url:?}");
        }
        return Ok(Some(url.clone()));
    }
    let Some(release) = &args.cc_release else {
        return Ok(None);
    };
    let release = release.trim();
    let well_formed = !release.is_empty()
        && release
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !well_formed {
        bail!(
            "--cc-release takes a release name such as cc-main-2025-26-nov-dec-jan \
             (see {CC_WEB_GRAPHS_PAGE}), got {release:?}; use --cc-ranks-url for a URL"
        );
    }
    Ok(Some(download::cc_domain_ranks_url(release)))
}

/// The `plumb ingest` command for the files just saved or kept, keeping the best
/// [`SUGGESTED_TOP`] sites.
fn ingest_hint(outcomes: &[(&str, Outcome)], dir: &Path) -> Option<String> {
    let wikidata_saved = outcomes.iter().any(|(name, outcome)| {
        *name == "wikidata" && matches!(outcome, Outcome::Saved(_) | Outcome::Kept(_))
    });
    let flags: Vec<String> = outcomes
        .iter()
        .filter_map(|(name, outcome)| match outcome {
            Outcome::Saved(path) | Outcome::Kept(path) => {
                Some(format!("--{name} {}", path.display()))
            }
            _ => None,
        })
        // Facts, kind sites and intros only go next to the official websites.
        .filter(|flag| {
            !(flag.starts_with("--wikidata-facts ")
                || flag.starts_with("--wikidata-kinds ")
                || flag.starts_with("--wikipedia-intros "))
                || wikidata_saved
        })
        .collect();
    if flags.is_empty() {
        return None;
    }
    Some(format!(
        "plumb ingest {} --top {SUGGESTED_TOP} --out {}",
        flags.join(" "),
        dir.join("records.jsonl").display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(release: Option<&str>, url: Option<&str>) -> FetchDataArgs {
        FetchDataArgs {
            dir: PathBuf::from("data"),
            cc_release: release.map(str::to_string),
            cc_ranks_url: url.map(str::to_string),
            skip_tranco: false,
            skip_wikidata: false,
            wikidata_min_sitelinks: 25,
            keep_days: 0,
        }
    }

    #[test]
    fn facts_cover_official_sites_from_earlier_runs() {
        let dir = tempfile::tempdir().unwrap();
        assert!(facts_sources(dir.path()).is_empty());
        // Only the kinds download worked this run; the main file is from the
        // last run and its sites still need facts.
        let sites = dir.path().join(download::WIKIDATA_FILE_NAME);
        let kinds = dir.path().join(kind_sites::KIND_SITES_FILE_NAME);
        std::fs::write(&sites, "item\tsite\n").unwrap();
        std::fs::write(&kinds, "item\tsite\n").unwrap();
        assert_eq!(facts_sources(dir.path()), [sites.clone(), kinds]);
        std::fs::remove_file(dir.path().join(kind_sites::KIND_SITES_FILE_NAME)).unwrap();
        assert_eq!(facts_sources(dir.path()), [sites]);
    }

    #[test]
    fn common_crawl_is_optional() {
        assert_eq!(cc_ranks_url(&args(None, None)).unwrap(), None);
    }

    #[test]
    fn release_names_become_urls() {
        let url = cc_ranks_url(&args(Some("cc-main-2025-26-nov-dec-jan"), None))
            .unwrap()
            .unwrap();
        assert_eq!(
            url,
            download::cc_domain_ranks_url("cc-main-2025-26-nov-dec-jan")
        );
        let url = cc_ranks_url(&args(None, Some("https://example.org/r.txt.gz"))).unwrap();
        assert_eq!(url.as_deref(), Some("https://example.org/r.txt.gz"));
    }

    #[test]
    fn bad_release_names_and_urls_are_rejected() {
        for release in ["", "../etc", "https://data.commoncrawl.org/x", "a b"] {
            assert!(
                cc_ranks_url(&args(Some(release), None)).is_err(),
                "{release}"
            );
        }
        assert!(cc_ranks_url(&args(None, Some("ftp://example.org/r.gz"))).is_err());
    }

    #[test]
    fn only_files_saved_within_keep_days_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("facts.tsv");
        assert_eq!(recent_file(&path, 1), None);
        std::fs::write(&path, "item\n").unwrap();
        assert_eq!(recent_file(&path, 1), Some(path.clone()));
        assert_eq!(recent_file(&path, 0), None);
        assert_eq!(recent_file(dir.path(), 1), None);
    }

    #[test]
    fn hint_lists_saved_files_only() {
        let outcomes = [
            ("tranco", Outcome::Saved(PathBuf::from("data/tranco.zip"))),
            ("cc-ranks", Outcome::Skipped("no release".into())),
            ("wikidata", Outcome::Failed(anyhow::anyhow!("timeout"))),
        ];
        assert_eq!(
            ingest_hint(&outcomes, Path::new("data")).as_deref(),
            Some("plumb ingest --tranco data/tranco.zip --top 1000000 --out data/records.jsonl")
        );
        let kept = [
            ("wikidata", Outcome::Kept(PathBuf::from("data/sites.tsv"))),
            (
                "wikipedia-intros",
                Outcome::Saved(PathBuf::from("data/intros.tsv")),
            ),
        ];
        assert_eq!(
            ingest_hint(&kept, Path::new("data")).as_deref(),
            Some(
                "plumb ingest --wikidata data/sites.tsv --wikipedia-intros data/intros.tsv \
                 --top 1000000 --out data/records.jsonl"
            )
        );
        let nothing = [("tranco", Outcome::Skipped("--skip-tranco".into()))];
        assert_eq!(ingest_hint(&nothing, Path::new("data")), None);
    }
}
