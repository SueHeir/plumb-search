//! Command-line arguments of the `plumb-tools` binary.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use plumb_node::cli::{parse_alpha, parse_country, parse_language, parse_positive, MeaningArgs};

/// Plumb Search tools: make the project's page sets and map, measure and
/// train ranking, and look into records files. A node never runs these;
/// `plumb` runs the node.
#[derive(Debug, Parser)]
#[command(name = "plumb-tools", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Download the seed datasets: Tranco, Common Crawl domain ranks and
    /// Wikidata official websites.
    FetchData(FetchDataArgs),
    /// Make a page set file (Wikipedia articles) from Wikimedia's dumps,
    /// for a node to list single pages with its sites.
    FetchPages(FetchPagesArgs),
    /// Make the map file the places' maps are drawn from: streets, water,
    /// parks and town names (OpenStreetMap, via the Protomaps basemap) for
    /// the whole world at low zooms and in detail around chosen points.
    FetchMap(crate::map_fetch::FetchMapArgs),
    /// Add official profiles (YouTube, Twitch, X, app stores, ...) from
    /// Wikidata to a Wikipedia articles file made by fetch-pages, and write
    /// the items with profiles but no article as the wikidata set beside it.
    FetchProfiles(FetchProfilesArgs),
    /// Add facts from Wikidata (a country's capital, a person's birth date,
    /// a company's CEO) to a Wikipedia articles file made by fetch-pages,
    /// for searches that ask one ("capital of australia").
    FetchFacts(FetchFactsArgs),
    /// Add each article's lead (its first sentences) and other names (the
    /// titles that lead to it, to one of its sections too: "Manubrium" to
    /// Sternum) to a Wikipedia articles file made by fetch-pages, from
    /// Wikimedia's weekly dump of its search index.
    FetchLeads(FetchLeadsArgs),
    /// Check how often the official site ranks first for a list of queries.
    Eval(EvalArgs),
    /// Check that the pages a queries file expects are in the page sets.
    CheckLabels(crate::eval_labels::CheckLabelsArgs),
    /// Train the learned ranking on the test searches `eval
    /// --features-out` wrote, and measure it on the half it did not see.
    TrainRank(crate::train_rank::TrainRankArgs),
    /// Fetch homepages and keep their visible text, for `plumb-tools terms`
    /// (an experiment).
    FetchText(crate::terms::FetchTextArgs),
    /// Pick each site's search terms from its homepage text, made by
    /// `plumb-tools fetch-text`, into the records (an experiment).
    Terms(crate::terms::TermsArgs),
    /// One sentence about each well-known site with no text, written by a
    /// language model: `pick` the sites, then `apply` the sentences.
    Summaries(crate::summaries::SummariesArgs),
    /// Measure node storage and private-search bucket sizes without changing data.
    Storage(StorageArgs),
    /// Rank the sites in a records file by the links their homepages make
    /// to each other (a PageRank of our own crawls), without changing
    /// anything. A look at the link graph; nothing uses the ranks yet.
    LinkRank(LinkRankArgs),
    /// How often each site's pages state Wikidata's facts right, from
    /// Common Crawl's page text (Knowledge-Based Trust), and optionally a
    /// copy of a records file with each site's counts, which its link
    /// score counts in.
    FactTrust(crate::fact_trust::FactTrustArgs),
    /// Fetch the homepages of a records file's best-scored sites and merge
    /// what they say, as a node's crawl rounds do.
    Crawl(crate::crawl::CrawlArgs),
    /// Build a node's search index from a records file.
    Index(crate::search::IndexArgs),
    /// Search an index from the command line.
    Search(crate::search::SearchArgs),
    /// Show what an index's spelling model learned from its words: the
    /// commonest slips, how likely given typos are, and completions.
    Spelling(crate::search::SpellingArgs),
    /// Make a vector of each site's text with a small embedding model
    /// (downloaded on first use), so searches can find sites by meaning.
    Embed(crate::embed::EmbedArgs),
    /// Make a vector of each page of a page set from its title,
    /// description and snippet (an article's lead), keyed by its URL (an
    /// experiment: nodes find pages by their words only).
    EmbedPages(crate::embed_pages::EmbedPagesArgs),
    /// Which pages searches find by meaning alone, with the vectors
    /// `embed-pages` made: where each search's wanted article comes among
    /// the nearest pages.
    PageMeaning(crate::embed_pages::PageMeaningArgs),
    /// Count the sites in a node's records that look dead, the ones
    /// `plumb run --drop-dead-sites` takes out of its index, without
    /// changing anything. Reads the whole records file into memory.
    DeadSites(crate::dead_sites::DeadSitesArgs),
    /// Learn each kind of Wikidata fact (capital, founder, CEO...) as a
    /// map between the vectors of Wikipedia articles, and measure how well
    /// the maps find facts they were not shown (an experiment).
    Relations(crate::relations::RelationsArgs),
    /// Follow a relation from a thing to what it is related to, through
    /// several steps if asked ("headquarters > capital"), or check a claim,
    /// with the maps `relations` wrote (an experiment).
    Relate(crate::relations::RelateArgs),
    /// How much of a records file the best sites take (by link score), by
    /// kind and at 100k, 250k, 500k... sites, and optionally a copy cut
    /// down to the best of them. The records file is left as it is.
    TopSites(TopSitesArgs),
    /// Sets in the data format: make one from a page set, a denylist or the
    /// site rules, sign it with a set-signing key, or check one as a node
    /// would.
    Set(crate::set::SetArgs),
}

#[derive(Debug, Args)]
pub struct StorageArgs {
    /// Existing node data directory. Symlinks are excluded from the scan.
    #[arg(long, value_name = "DIR")]
    pub data: PathBuf,
    /// Print aggregate measurements as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct TopSitesArgs {
    /// Records file (JSON lines). A journal next to it is read too, from
    /// a copy: the file is left as it is.
    #[arg(long, value_name = "PATH")]
    pub records: PathBuf,
    /// Write the best sites here, as a records file to index and evaluate.
    /// Sites a dead-site cut left only ranks are never written.
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
    /// With --out, how many sites to write [default: all].
    #[arg(long, value_name = "N", requires = "out")]
    pub top: Option<usize>,
    /// With --out, write only sites with a name: a homepage title, or a
    /// name from Wikidata or an About page.
    #[arg(long, requires = "out")]
    pub named_only: bool,
}

#[derive(Debug, Args)]
pub struct LinkRankArgs {
    /// Records file (JSON lines). A journal next to it is read too, from
    /// a copy: the file is left as it is.
    #[arg(long, value_name = "PATH")]
    pub records: PathBuf,
    /// Also write every site's rank here, best first, as tab-separated
    /// lines.
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
    /// Also write a copy of the records here in which each site's
    /// `pagerank_rank` is its place by links when that is better, to index
    /// and evaluate a node that ranks by links. The records file is left
    /// as it is.
    #[arg(long, value_name = "PATH")]
    pub apply: Option<PathBuf>,
    /// With --apply, only the best this many sites by links get their
    /// place; the rest keep their ranks as they were. 0 changes no rank.
    #[arg(long, value_name = "N", default_value_t = 50_000, requires = "apply")]
    pub apply_top: u32,
    /// With --apply, also take the links a site got from unranked sites
    /// (link farms: sites no trusted site links to) off its count of
    /// linking sites.
    #[arg(long, requires = "apply")]
    pub demote: bool,
    /// Name the sites most often linked from the same trusted sites as
    /// this one (may be given more than once).
    #[arg(long, value_name = "DOMAIN")]
    pub similar: Vec<String>,
    /// How many of the best sites to name.
    #[arg(long, value_name = "N", default_value_t = 30)]
    pub show: usize,
    /// Most rounds to run before stopping.
    #[arg(long, value_name = "N", default_value_t = 50, value_parser = parse_positive)]
    pub rounds: usize,
}

#[derive(Debug, Args)]
pub struct FetchDataArgs {
    /// Directory to download into (created if missing).
    #[arg(long, value_name = "DIR")]
    pub dir: PathBuf,
    /// Common Crawl web graph release to take domain ranks from, such as
    /// cc-main-2025-26-nov-dec-jan (release names are listed on
    /// https://commoncrawl.org/web-graphs). Without this or --cc-ranks-url,
    /// Common Crawl is skipped.
    #[arg(long, value_name = "NAME", conflicts_with = "cc_ranks_url")]
    pub cc_release: Option<String>,
    /// Download Common Crawl domain ranks from this URL instead of a release name.
    #[arg(long, value_name = "URL")]
    pub cc_ranks_url: Option<String>,
    /// Do not download the Tranco list.
    #[arg(long)]
    pub skip_tranco: bool,
    /// Do not query Wikidata.
    #[arg(long)]
    pub skip_wikidata: bool,
    /// Only fetch Wikidata items with at least this many Wikipedia sitelinks
    /// (a notability filter). Wikidata's own endpoint can only list them
    /// from 25 up, so that is where it starts when --wikidata-mirror fails.
    #[arg(long, value_name = "N", default_value_t = plumb_ingest::download::DEFAULT_MIN_SITELINKS)]
    pub wikidata_min_sitelinks: u32,
    /// A copy of Wikidata to ask first for the official websites and their
    /// facts: QLever's lists them all in seconds, while Wikidata's own
    /// endpoint needs many queries, each close to its 60-second limit. When
    /// it fails, Wikidata's own endpoint is asked.
    #[arg(long, value_name = "URL", default_value = plumb_ingest::download::QLEVER_WIKIDATA_URL)]
    pub wikidata_mirror: String,
    /// Ask only Wikidata's own endpoint, not --wikidata-mirror.
    #[arg(long)]
    pub no_wikidata_mirror: bool,
    /// Keep each file an earlier run saved in --dir within this many days
    /// instead of fetching it again, so a rerun only fetches what is missing,
    /// stale or failed. A file copied in from another run's folder counts
    /// as just saved. 0 fetches everything.
    #[arg(long, value_name = "DAYS", default_value_t = 0)]
    pub keep_days: u64,
}

#[derive(Debug, Args)]
pub struct FetchProfilesArgs {
    /// A node's data directory whose English Wikipedia set gets the
    /// profiles; the node picks the new file up within seconds.
    #[arg(long, value_name = "DIR", required_unless_present = "articles")]
    pub data: Option<PathBuf>,
    /// The articles file to add them to instead.
    #[arg(long, value_name = "PATH")]
    pub articles: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct FetchLeadsArgs {
    /// A node's data directory whose English Wikipedia set gets the leads;
    /// the node picks the new file up within seconds.
    #[arg(long, value_name = "DIR", required_unless_present = "articles")]
    pub data: Option<PathBuf>,
    /// The articles file to add them to instead.
    #[arg(long, value_name = "PATH")]
    pub articles: Option<PathBuf>,
    /// Directory for the dump's files (about 66 of 600 MB for English, a
    /// few at a time, each deleted once read) and what was read of them,
    /// so a stopped run carries on.
    #[arg(long, value_name = "DIR")]
    pub work: PathBuf,
    /// How many of the most read articles get a lead.
    #[arg(long, value_name = "N", default_value_t = 2_000_000)]
    pub top: usize,
    /// Keep the dump's files once read.
    #[arg(long)]
    pub keep_dumps: bool,
    /// Read these files of the dump instead of downloading it.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub dumps: Vec<PathBuf>,
}

#[derive(Debug, Args)]
pub struct FetchFactsArgs {
    /// A node's data directory whose English Wikipedia set gets the
    /// facts; the node picks the new file up within seconds.
    #[arg(long, value_name = "DIR", required_unless_present = "articles")]
    pub data: Option<PathBuf>,
    /// The articles file to add them to instead.
    #[arg(long, value_name = "PATH")]
    pub articles: Option<PathBuf>,
    /// Repair just these Wikidata IDs, without scanning worldwide facts.
    #[arg(long, value_name = "QID", num_args = 1.., conflicts_with = "retry_facts")]
    pub items: Vec<String>,
    /// Property keys for --items (default: every supported property).
    #[arg(long, value_name = "KEY", num_args = 1.., requires = "items")]
    pub properties: Vec<String>,
    /// Retry the failed item/property pairs of a previous completion JSON.
    #[arg(long, value_name = "PATH", conflicts_with = "items")]
    pub retry_facts: Option<PathBuf>,
    /// Save completion/provenance and retry pairs here (default: next to
    /// the articles file, with .facts.json appended).
    #[arg(long, value_name = "PATH")]
    pub report: Option<PathBuf>,
    /// Where to read on when Wikidata's query service stops answering a
    /// kind's deep pages (it times out on them): by default QLever's copy
    /// of Wikidata.
    #[arg(long, value_name = "URL", default_value = crate::build::item_facts::DEEP_SPARQL_URL)]
    pub deep_endpoint: String,
    /// Ask only Wikidata's query service, never --deep-endpoint.
    #[arg(long)]
    pub wikidata_only: bool,
}

#[derive(Debug, Args)]
pub struct FetchPagesArgs {
    /// The page set to make: wikipedia-en (English Wikipedia's articles),
    /// github (GitHub repositories, from GitHub's search API; set
    /// GITHUB_TOKEN to search three times as fast), stackoverflow (Stack
    /// Overflow's most viewed questions, from Stack Exchange's data dump),
    /// stackexchange (the most viewed questions of Super User, Ask Ubuntu,
    /// Home Improvement and 30 more Stack Exchange sites, from the same
    /// dump),
    /// books (Open Library's most shelved works, from its dumps), podcasts
    /// (Podcast Index's most popular podcasts, from its database), music
    /// (the songs and albums most listened to, from MusicBrainz's dump and
    /// ListenBrainz's listener counts), films (films and TV shows, with
    /// their year, director, cast and listings, from Wikidata), papers
    /// (the most cited works, from OpenAlex's API; set OPENALEX_API_KEY if
    /// it asks for one), packages (the most used packages of eight
    /// registries, from ecosyste.ms), docs (pages of MDN, Python's docs and
    /// 36 more software docs sites, from their sitemaps; --work keeps each
    /// site's pages so a stopped run carries on), reference (pages of
    /// about 150 well-known reference sites: health, dictionaries, recipes,
    /// how-tos and government, from their sitemaps; --work as for docs),
    /// subpages (pages of about 250 universities and labs, big companies,
    /// government agencies, entertainment sites and museums, from their
    /// sitemaps and the pages their homepages link to; --work as for docs),
    /// places (named shops, restaurants, parks and towns from
    /// OpenStreetMap) or wiktionary (English words and what they mean,
    /// from kaikki.org's reading of Wiktionary, about 3.3 GB, for "define"
    /// searches).
    #[arg(long, value_name = "SET", default_value = "wikipedia-en")]
    pub set: String,
    /// Directory to download Wikipedia's dumps into (created if missing).
    /// About 3 GB for English, plus about 400 MB per day of page views.
    #[arg(long, value_name = "DIR")]
    pub work: Option<PathBuf>,
    /// A node's data directory to put the set's file in, where the node
    /// picks it up within seconds.
    #[arg(long, value_name = "DIR", required_unless_present = "out")]
    pub data: Option<PathBuf>,
    /// Write the set's file here instead.
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
    /// Days of page views to rank by, ending two days ago.
    #[arg(long, value_name = "DAYS", default_value_t = 7)]
    pub pageview_days: u32,
    /// Keep downloaded dumps younger than this many days instead of fetching
    /// them again (page views of past days never change, so they are always
    /// kept).
    #[arg(long, value_name = "DAYS", default_value_t = 20)]
    pub keep_days: u64,
    /// Docs/reference/subpages: reuse compatible completed caches younger
    /// than this many days (0 always refreshes).
    #[arg(long, value_name = "DAYS", default_value_t = 7)]
    pub cache_max_age_days: u64,
    /// Docs/reference/subpages: fetch again even when a cache is fresh.
    #[arg(long)]
    pub force_refresh: bool,
    /// Docs/reference/subpages: discard existing hosts intentionally.
    /// By default successful selected hosts are merged into the old set.
    #[arg(long)]
    pub replace_set: bool,
    /// Docs/reference/subpages: fewest useful pages needed to replace a host.
    #[arg(long, value_name = "N", default_value_t = 3)]
    pub min_useful_pages: usize,
    /// Docs/reference/subpages/papers: write an immutable generation for evaluation
    /// without publishing the set file.
    #[arg(long, conflicts_with = "promote_generation")]
    pub stage_only: bool,
    /// Promote a previously staged generation after its canaries pass.
    #[arg(long, value_name = "DIR")]
    pub promote_generation: Option<PathBuf>,
    /// Docs/reference/subpages/papers: explicitly permit growth beyond 125% of the
    /// current compressed set (normally left staged for review).
    #[arg(long)]
    pub allow_set_growth: bool,
    /// Docs/reference/subpages/papers: maximum compressed bytes in a candidate set;
    /// 0 leaves the limit unset. Includes retained hosts in a targeted refresh.
    #[arg(long, value_name = "BYTES", default_value_t = 0)]
    pub max_set_bytes: u64,
    /// Wikidata's official websites (wikidata-official-sites.tsv from
    /// fetch-data), so an article about a site's organization is shown
    /// under that site.
    #[arg(long, value_name = "PATH")]
    pub official_sites: Option<PathBuf>,
    /// Read these files instead of downloading: the page, page_props and
    /// redirect dumps, then the page view files.
    #[arg(long, value_name = "PATH", num_args = 4.., conflicts_with = "pageview_days")]
    pub dumps: Vec<PathBuf>,
    /// GitHub: fewest stars of a repository kept.
    #[arg(long, value_name = "STARS", default_value_t = crate::build::github::DEFAULT_MIN_STARS)]
    pub min_stars: u64,
    /// GitHub: most repositories kept.
    #[arg(long, value_name = "N", default_value_t = 1_000_000)]
    pub max_repos: usize,
    /// Stack Overflow: read this Posts .7z instead of downloading it.
    #[arg(long, value_name = "PATH")]
    pub posts: Option<PathBuf>,
    /// Stack Overflow and other Stack Exchange sites: lowest score of a
    /// question kept.
    #[arg(
        long,
        value_name = "SCORE",
        default_value_t = 1,
        allow_negative_numbers = true
    )]
    pub min_score: i64,
    /// Stack Overflow: most questions kept, the most viewed.
    #[arg(long, value_name = "N", default_value_t = 2_000_000)]
    pub max_questions: usize,
    /// Other Stack Exchange sites: most questions kept of each site, the
    /// most viewed.
    #[arg(long, value_name = "N", default_value_t = 100_000)]
    pub max_per_site: usize,
    /// Other Stack Exchange sites: delete each site's dump once it is read,
    /// rather than keeping it for --keep-days.
    #[arg(long)]
    pub drop_dumps: bool,
    /// Books: fewest reading log entries and ratings of a book kept.
    #[arg(long, value_name = "N", default_value_t = 3)]
    pub min_shelvings: u32,
    /// Books: most books kept, the most shelved.
    #[arg(long, value_name = "N", default_value_t = 1_000_000)]
    pub max_books: usize,
    /// Papers: fewest citations of a paper kept.
    #[arg(long, value_name = "N", default_value_t = crate::build::openalex::DEFAULT_MIN_CITATIONS)]
    pub min_citations: u64,
    /// Papers: most papers kept, the most cited.
    #[arg(long, value_name = "N", default_value_t = 2_000_000)]
    pub max_papers: usize,
    /// Papers: opt in to bounded recent publication ingestion ending on this
    /// inclusive ISO date (YYYY-MM-DD). Requires --work for resumable progress.
    /// The recent lane stays off unless this option is supplied.
    #[arg(long, value_name = "DATE", requires = "work")]
    pub recent_papers_end: Option<String>,
    /// Papers: inclusive recent publication window, 1..366 days.
    #[arg(long, value_name = "DAYS", default_value_t = crate::build::recent_papers::DEFAULT_WINDOW_DAYS, requires = "recent_papers_end")]
    pub recent_papers_days: usize,
    /// Papers: recent records reserved across date/domain partitions, 1..50000.
    #[arg(long, value_name = "N", default_value_t = crate::build::recent_papers::DEFAULT_RECORD_BUDGET, requires = "recent_papers_end")]
    pub recent_papers_records: usize,
    /// Papers: most recent-provider requests this invocation, 1..1000. A
    /// refused or unfinished run retains progress and keeps the previous set.
    #[arg(
        long,
        value_name = "N",
        default_value_t = 1000,
        requires = "recent_papers_end"
    )]
    pub recent_papers_requests: usize,
    /// Papers: most requests to CORE for free copies (fifty papers each),
    /// when CORE_API_KEY is set.
    #[arg(long, value_name = "N", default_value_t = crate::build::core_ac::DEFAULT_MAX_REQUESTS)]
    pub max_core_requests: usize,
    /// Podcasts: fewest Podcast Index popularity points (0 to 9) of a
    /// podcast kept.
    #[arg(long, value_name = "N", default_value_t = crate::build::podcasts::DEFAULT_MIN_SCORE)]
    pub min_podcast_score: u32,
    /// Podcasts: most podcasts kept, the most popular.
    #[arg(long, value_name = "N", default_value_t = 300_000)]
    pub max_podcasts: usize,
    /// Podcasts: read this Podcast Index database (podcastindex_feeds.db)
    /// instead of downloading it into --work.
    #[arg(long, value_name = "PATH")]
    pub podcast_db: Option<PathBuf>,
    /// Music: read this MusicBrainz core dump (mbdump.tar.bz2, or a
    /// directory of its tables) instead of downloading it into --work.
    #[arg(long, value_name = "PATH")]
    pub musicbrainz_dump: Option<PathBuf>,
    /// Music: ListenBrainz's canonical data dump (`.tar.zst`, or its
    /// canonical_recording_redirect.csv) instead of downloading it into
    /// --work.
    #[arg(long, value_name = "PATH")]
    pub listenbrainz_canonical: Option<PathBuf>,
    /// Music: most songs kept, the most listened to.
    #[arg(long, value_name = "N", default_value_t = crate::build::musicbrainz::DEFAULT_MAX_SONGS)]
    pub max_songs: usize,
    /// Music: most albums kept, the most listened to.
    #[arg(long, value_name = "N", default_value_t = crate::build::musicbrainz::DEFAULT_MAX_ALBUMS)]
    pub max_albums: usize,
    /// Music: fewest release groups (albums, singles, compilations) a song
    /// is on for ListenBrainz to be asked about it, unless it is on an
    /// album kept.
    #[arg(long, value_name = "N", default_value_t = crate::build::musicbrainz::DEFAULT_MIN_SONG_RELEASES)]
    pub min_song_releases: u32,
    /// Music: fewest ListenBrainz listeners of a song or album kept.
    #[arg(long, value_name = "N", default_value_t = crate::build::musicbrainz::DEFAULT_MIN_LISTENERS)]
    pub min_listeners: u64,
    /// Films: most films and shows kept, the most linked from Wikipedias
    /// and other wikis.
    #[arg(long, value_name = "N", default_value_t = crate::build::films::DEFAULT_MAX_FILMS)]
    pub max_films: usize,
    /// Films: fewest sitelinks (Wikipedias and other wikis with a page on
    /// it) of a film or show kept.
    #[arg(long, value_name = "N", default_value_t = crate::build::films::DEFAULT_MIN_SITELINKS)]
    pub min_film_sitelinks: u64,
    /// Packages: the registries to list (npm, pypi, crates, go, gem,
    /// composer, nuget, maven), comma-separated; all when left out.
    #[arg(long, value_name = "KEYS", value_delimiter = ',')]
    pub registries: Vec<String>,
    /// Packages: most packages kept of each registry, the most used.
    #[arg(long, value_name = "N", default_value_t = crate::build::packages::DEFAULT_MAX_PER_REGISTRY)]
    pub max_per_registry: usize,
    /// Places: read these OpenStreetMap extracts (.osm.pbf, which may
    /// overlap) instead of downloading the whole planet (about 90 GB) into
    /// --work.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub osm: Vec<PathBuf>,
    /// Places: the English Wikipedia articles file (the wikipedia-en set),
    /// whose titles and redirects name the places their Wikidata items
    /// are: "Big Ben", "Philly".
    #[arg(long, value_name = "PATH")]
    pub articles: Option<PathBuf>,
    /// Docs: the docs sites to fetch (mdn, python, rust and others; see
    /// plumb_core::docs), comma-separated; all when left out.
    #[arg(long, value_name = "KEYS", value_delimiter = ',')]
    pub docs_sites: Vec<String>,
    /// Docs: most pages fetched of each site, the shallowest first.
    #[arg(long, value_name = "N", default_value_t = crate::build::docs::DEFAULT_MAX_PER_SITE)]
    pub max_docs_per_site: usize,
    /// Reference: the reference sites to fetch, by host without `www.`
    /// (healthline.com, merriam-webster.com and others; see
    /// plumb_tools::reference_sites), comma-separated; all when left out.
    #[arg(long, value_name = "HOSTS", value_delimiter = ',')]
    pub reference_sites: Vec<String>,
    /// Reference: limit pages fetched per host, including hosts with their
    /// own larger cap. When omitted, use the source cap or 5,000 pages.
    #[arg(long, value_name = "N")]
    pub max_reference_per_site: Option<usize>,
    /// Subpages: limit pages fetched per host, including source-specific caps.
    #[arg(long, value_name = "N")]
    pub max_subpages_per_site: Option<usize>,
    /// Subpages: the subpage sites to fetch, by host without `www.`
    /// (nist.gov, chessprogramming.org and others; see
    /// plumb_tools::subpage_sites), comma-separated; all when left out.
    #[arg(long, value_name = "HOSTS", value_delimiter = ',')]
    pub subpage_sites: Vec<String>,
    /// Subpages: only the sites of these kinds (university, company,
    /// government, entertainment, museum), comma-separated; all when left
    /// out.
    #[arg(long, value_name = "KINDS", value_delimiter = ',')]
    pub subpage_kinds: Vec<String>,
}

#[derive(Debug, Args)]
pub struct EvalArgs {
    /// Index directory.
    #[arg(long, value_name = "DIR")]
    pub index: PathBuf,
    /// Queries file: `query<TAB>expected_domain[,another_ok_domain]` per line;
    /// blank lines and lines starting with `#` are skipped. Can be given
    /// more than once; each file is measured on its own.
    #[arg(long, value_name = "TSV", required_unless_present = "acceptance")]
    pub queries: Vec<PathBuf>,
    /// Family-based JSONL contracts, separate from training features.
    #[arg(long, value_name = "JSONL", requires = "report")]
    pub acceptance: Vec<PathBuf>,
    /// Machine-readable manifest, complete responses, stages and summary.
    /// Core offline mode: findings, personalization, plugins and peers are off.
    #[arg(long, value_name = "JSONL", requires = "eval_time", conflicts_with_all = ["sweep", "features_out", "facts", "profiles", "follow_suggestions", "rerank_model"])]
    pub report: Option<PathBuf>,
    /// Fixed evaluation clock, Unix seconds. Required for reproducible reports.
    #[arg(long, value_name = "UNIX", value_parser = clap::value_parser!(u64).range(0..=i64::MAX as u64))]
    pub eval_time: Option<u64>,
    /// Try several rankings in one run: a file of `name<TAB>{"knob": value}`
    /// lines, each changing knobs of the ranking --rank gives. Prints one
    /// table of every queries file under every ranking, and which queries
    /// each one moved, against the ranking unchanged ("base").
    #[arg(long, value_name = "TSV")]
    pub sweep: Option<PathBuf>,
    /// With --sweep, also write every query's rank under every ranking to
    /// this TSV file (`variant suite line query rank`, 0 when not found).
    #[arg(long, value_name = "PATH", requires = "sweep")]
    pub ranks_out: Option<PathBuf>,
    /// Write each query's listed results, with the scores and signals that
    /// ranked them and which one was expected, to this JSON-lines file:
    /// what a learned ranking is trained on. Not with --sweep or --facts.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["sweep", "facts"])]
    pub features_out: Option<PathBuf>,
    /// With --features-out, also score the first 20 results of each query
    /// with this cross-encoder model (a folder with its config.json,
    /// tokenizer.json and model.safetensors), and time it. Can be given
    /// more than once.
    #[arg(long, value_name = "DIR", requires = "features_out")]
    pub rerank_model: Vec<PathBuf>,
    /// Write each query's deep site candidates to this JSON-lines file,
    /// with the raw signals a first stage could score them by (which of the
    /// query's words each field has, how long the fields are, how rare each
    /// word is) and which sites are expected: for designing the first stage
    /// offline. Only queries with an expected site.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["sweep", "facts", "features_out", "recall"])]
    pub candidates_out: Option<PathBuf>,
    /// With --candidates-out, how far down each list of candidates goes:
    /// today's first stage, the sites with every main word, and the sites
    /// nearest in meaning.
    #[arg(long, value_name = "N", default_value_t = 500, value_parser = parse_positive)]
    pub candidates_depth: usize,
    /// Results fetched per query; an expected site further down counts as not found.
    #[arg(long, value_name = "N", default_value_t = 10, value_parser = parse_positive)]
    pub limit: usize,
    /// Weight of the popularity prior in the ranking, from 0 to 1
    /// [default: the index's default].
    #[arg(long, value_name = "A", value_parser = parse_alpha)]
    pub alpha: Option<f32>,
    /// Exit with an error when the share of queries answered at rank 1 is
    /// below this fraction, e.g. 0.9.
    #[arg(long, value_name = "F", value_parser = parse_fraction)]
    pub min_top1: Option<f64>,
    /// Home country of the searches, a two-letter code such as US
    /// [default: none].
    #[arg(long, value_name = "CODE", value_parser = parse_country)]
    pub country: Option<String>,
    /// Only sites in this language, a code such as en, as the search
    /// page's language setting does [default: any].
    #[arg(long, value_name = "CODE", value_parser = parse_language)]
    pub lang: Option<String>,
    /// Search for each query without suggesting a spelling.
    #[arg(long, conflicts_with = "follow_suggestions")]
    pub exact: bool,
    /// When a query gets a "Did you mean" suggestion, measure the
    /// suggestion's results instead: what one click finds
    /// (eval/typo_queries.tsv).
    #[arg(long)]
    pub follow_suggestions: bool,
    /// Print every "Did you mean" suggestion a query gets, to see how many
    /// right spellings get one (eval/brand_queries.tsv) and what typos are
    /// taken for.
    #[arg(long, conflicts_with = "exact")]
    pub show_suggestions: bool,
    /// Ranking knobs to change, as JSON, e.g. '{"exact_label_bonus": 0.1}'.
    /// The other knobs keep their defaults; --alpha wins over an alpha here.
    #[arg(long, value_name = "JSON", value_parser = parse_rank_config)]
    pub rank: Option<plumb_index::RankConfig>,
    /// For each miss, also show how the first site and the expected one
    /// scored: final score, text match, link score and closeness in meaning.
    #[arg(long)]
    pub explain: bool,
    /// Measure where each query's answer is lost before ranking instead:
    /// whether it is in the index or a page set at all, how far down the
    /// sites matching the query's words (first 50, 100, 1,000, 10,000) or
    /// nearest in meaning it is, or the pages found, and whether the
    /// ranking looked at it. Prints one line per query not answered in the
    /// first --limit and a table per file.
    #[arg(long, conflicts_with_all = ["sweep", "facts", "features_out"])]
    pub recall: bool,
    /// Print the first N results of every query, hit or miss: each site's
    /// domain (with the pages shown under it) or page's address.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub show: usize,
    /// Check the instant answers to fact searches instead of the ranks
    /// (eval/fact_queries.tsv, with --pages and a Wikipedia set made with
    /// fetch-facts): a query counts as found when its answer has one of
    /// the expected texts.
    #[arg(long)]
    pub facts: bool,
    /// Count the profile or listing a node shows above the results for a
    /// query ending in a service ("bohemian rhapsody lyrics", "mrbeast
    /// youtube") as the first result (eval/lyrics_queries.tsv, with
    /// --pages).
    #[arg(long)]
    pub profiles: bool,
    /// Page set files (wikipedia-en.tsv.gz, github.tsv.gz from fetch-pages)
    /// whose pages are listed among the sites, as a node lists them. Can be
    /// given more than once.
    #[arg(long, value_name = "PATH")]
    pub pages: Vec<PathBuf>,
    /// How many of each page set's most read pages to keep.
    #[arg(long, value_name = "N", default_value_t = usize::MAX, hide_default_value = true)]
    pub pages_top: usize,
    /// Keep the index built from --pages in this folder and reuse it on
    /// later runs with the same page set files and the same `plumb`
    /// binary, instead of rebuilding it every run. Runs at once share one
    /// build; only the few most recently used indexes are kept.
    #[arg(long, value_name = "DIR", env = "PLUMB_EVAL_PAGES_CACHE")]
    pub pages_cache: Option<PathBuf>,
    /// Measure only one half of the queries: `tune` to try ranking changes
    /// on, `held-out` to check them on afterwards. Which half a query is in
    /// depends on its words alone (see eval/README.md) [default: both].
    #[arg(long, value_name = "HALF")]
    pub half: Option<Half>,
    #[command(flatten)]
    pub meaning: MeaningArgs,
}

/// A half of a queries file, for `plumb-tools eval --half`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Half {
    /// The queries ranking changes are tried and tuned on.
    Tune,
    /// The queries kept back to check a tuned ranking on.
    HeldOut,
}

fn parse_rank_config(s: &str) -> Result<plumb_index::RankConfig, String> {
    serde_json::from_str(s).map_err(|err| format!("expected ranking knobs as JSON: {err}"))
}

fn parse_fraction(s: &str) -> Result<f64, String> {
    match s.trim().parse::<f64>() {
        Ok(f) if (0.0..=1.0).contains(&f) => Ok(f),
        _ => Err(format!(
            "expected a fraction from 0 to 1 (0.9 means 90%), got `{s}`"
        )),
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("plumb-tools").chain(args.iter().copied()))
    }

    #[test]
    fn definitions_are_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn inner_page_refreshes_have_explicit_cache_stage_and_budget_controls() {
        let cli = parse(&[
            "fetch-pages",
            "--set",
            "reference",
            "--out",
            "scratch/reference.tsv.gz",
            "--reference-sites",
            "irs.gov,ssa.gov",
            "--max-reference-per-site",
            "12",
            "--cache-max-age-days",
            "1",
            "--force-refresh",
            "--stage-only",
            "--max-set-bytes",
            "1000000",
        ])
        .unwrap();
        let Command::FetchPages(args) = cli.command else {
            panic!("not fetch-pages")
        };
        assert_eq!(args.cache_max_age_days, 1);
        assert!(args.force_refresh && args.stage_only);
        assert_eq!(args.max_reference_per_site, Some(12));
        assert_eq!(args.max_set_bytes, 1000000);
        assert!(!args.replace_set);
        assert!(parse(&[
            "fetch-pages",
            "--set",
            "docs",
            "--out",
            "docs.tsv.gz",
            "--stage-only",
            "--promote-generation",
            "generation"
        ])
        .is_err());
    }

    #[test]
    fn recent_paper_ingestion_is_explicit_and_requires_resumable_work() {
        let Command::FetchPages(defaults) =
            parse(&["fetch-pages", "--set", "papers", "--out", "papers.tsv.gz"])
                .unwrap()
                .command
        else {
            panic!("not fetch-pages")
        };
        assert!(defaults.recent_papers_end.is_none());
        assert!(parse(&[
            "fetch-pages",
            "--set",
            "papers",
            "--out",
            "papers.tsv.gz",
            "--recent-papers-end",
            "2026-10-09",
        ])
        .is_err());
        assert!(parse(&[
            "fetch-pages",
            "--set",
            "papers",
            "--out",
            "papers.tsv.gz",
            "--recent-papers-requests",
            "5",
        ])
        .is_err());
        let Command::FetchPages(args) = parse(&[
            "fetch-pages",
            "--set",
            "papers",
            "--out",
            "papers.tsv.gz",
            "--work",
            "scratch",
            "--recent-papers-end",
            "2026-10-09",
            "--recent-papers-days",
            "30",
            "--recent-papers-records",
            "100",
            "--recent-papers-requests",
            "4",
            "--stage-only",
        ])
        .unwrap()
        .command
        else {
            panic!("not fetch-pages")
        };
        assert_eq!(args.recent_papers_end.as_deref(), Some("2026-10-09"));
        assert_eq!(args.recent_papers_days, 30);
        assert_eq!(args.recent_papers_records, 100);
        assert_eq!(args.recent_papers_requests, 4);
        assert!(args.stage_only);
    }

    #[test]
    fn storage_requires_data_and_accepts_json() {
        assert!(parse(&["storage"]).is_err());
        let Command::Storage(args) = parse(&["storage", "--data", "/data", "--json"])
            .unwrap()
            .command
        else {
            panic!("not storage")
        };
        assert_eq!(args.data, PathBuf::from("/data"));
        assert!(args.json);
    }

    #[test]
    fn facts_targeted_flags_require_an_explicit_item_list() {
        let cli = parse(&[
            "fetch-facts",
            "--articles",
            "staged.tsv.gz",
            "--items",
            "Q17",
            "Q408",
            "--properties",
            "population",
            "capital",
        ])
        .unwrap();
        let Command::FetchFacts(args) = cli.command else {
            panic!("expected fetch-facts");
        };
        assert_eq!(args.items, ["Q17", "Q408"]);
        assert_eq!(args.properties, ["population", "capital"]);
        assert!(parse(&[
            "fetch-facts",
            "--articles",
            "staged.tsv.gz",
            "--properties",
            "population"
        ])
        .is_err());
        assert!(parse(&[
            "fetch-facts",
            "--articles",
            "staged.tsv.gz",
            "--items",
            "Q17",
            "--retry-facts",
            "report.json"
        ])
        .is_err());
        let cli = parse(&[
            "fetch-facts",
            "--articles",
            "staged.tsv.gz",
            "--retry-facts",
            "report.json",
        ])
        .unwrap();
        let Command::FetchFacts(args) = cli.command else {
            panic!("expected fetch-facts");
        };
        assert_eq!(args.retry_facts, Some(PathBuf::from("report.json")));
    }

    #[test]
    fn fetch_data_release_and_url_conflict() {
        let err = parse(&[
            "fetch-data",
            "--dir",
            "data",
            "--cc-release",
            "cc-main-2025-26-nov-dec-jan",
            "--cc-ranks-url",
            "https://example.org/ranks.txt.gz",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        let cli = parse(&["fetch-data", "--dir", "data"]).unwrap();
        let Command::FetchData(args) = cli.command else {
            panic!("not fetch-data");
        };
        assert_eq!(args.wikidata_min_sitelinks, 3);
        assert!(!args.skip_tranco && !args.skip_wikidata);
    }

    #[test]
    fn eval_defaults() {
        let cli = parse(&[
            "eval",
            "--index",
            "idx",
            "--queries",
            "q.tsv",
            "--min-top1",
            "0.9",
        ])
        .unwrap();
        let Command::Eval(args) = cli.command else {
            panic!("not eval");
        };
        assert_eq!(args.min_top1, Some(0.9));
        assert_eq!(args.limit, 10);
        assert!(parse(&["eval", "--index", "i", "--queries", "q", "--min-top1", "90"]).is_err());
        assert!(args.rank.is_none() && !args.explain);
        let cli = parse(&[
            "eval",
            "--index",
            "i",
            "--queries",
            "q",
            "--rank",
            r#"{"exact_label_bonus": 0.1}"#,
        ])
        .unwrap();
        let Command::Eval(args) = cli.command else {
            panic!("not eval");
        };
        let rank = args.rank.unwrap();
        assert_eq!(rank.exact_label_bonus, 0.1);
        assert_eq!(rank.alpha, plumb_index::RankConfig::default().alpha);
        assert!(parse(&["eval", "--index", "i", "--queries", "q", "--rank", "0.1"]).is_err());
    }

    #[test]
    fn search_takes_query_words() {
        let cli = parse(&["search", "--index", "idx", "--json", "us", "bank"]).unwrap();
        let Command::Search(args) = cli.command else {
            panic!("not search");
        };
        assert_eq!(args.query, ["us", "bank"]);
        assert_eq!(args.limit, 10);
        assert!(args.json);
        assert!(parse(&["search", "--index", "idx"]).is_err());
        assert!(parse(&["search", "--index", "idx", "--limit", "0", "x"]).is_err());
        assert!(parse(&["search", "--index", "idx", "--alpha", "1.5", "x"]).is_err());
    }

    #[test]
    fn paper_search_cli_takes_typed_dates_and_order() {
        let cli = parse(&[
            "search",
            "--index",
            "pages",
            "--paper",
            "--after",
            "2025",
            "--before",
            "2026-09-30",
            "--order",
            "newest",
            "transformer",
        ])
        .unwrap();
        let Command::Search(args) = cli.command else {
            panic!("not search");
        };
        assert!(args.paper);
        let query = plumb_core::paper_query::PaperQuery::with_options(
            &args.query.join(" "),
            args.after.as_deref(),
            args.before.as_deref(),
            args.order.as_deref(),
        )
        .unwrap();
        assert!(query.newest);
        assert_eq!(query.before.unwrap().day, Some(20260930));
        assert!(parse(&[
            "search",
            "--index",
            "pages",
            "--after",
            "2025",
            "transformer"
        ])
        .is_err());
        assert!(parse(&[
            "search",
            "--index",
            "pages",
            "--paper",
            "--order",
            "bad",
            "transformer"
        ])
        .is_err());
    }

    #[test]
    fn crawl_defaults() {
        let cli = parse(&["crawl", "--records", "r.jsonl"]).unwrap();
        let Command::Crawl(args) = cli.command else {
            panic!("not crawl");
        };
        assert_eq!(args.top, 1000);
        assert_eq!(args.skip_crawled_within_days, 30);
        assert_eq!(args.concurrency, 16);
        assert_eq!(args.out, None);
        assert!(!args.use_system_proxy);
        assert!(parse(&["crawl", "--records", "r", "--concurrency", "0"]).is_err());
        let cli = parse(&["crawl", "--records", "r", "--use-system-proxy"]).unwrap();
        let Command::Crawl(args) = cli.command else {
            panic!("not crawl");
        };
        assert!(args.use_system_proxy);
    }
}
