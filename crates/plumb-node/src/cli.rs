//! Command-line arguments of the `plumb` binary.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

use crate::country::HomeCountry;
use crate::websearch::{parse_web_search, WebSearch};

/// Plumb Search: a self-hostable search engine.
///
/// The easy way: `plumb run --data DIR` sets everything up and keeps the
/// index fresh. Step by step: fetch-data, ingest, crawl (optional), index,
/// then search, serve or eval.
#[derive(Debug, Parser)]
#[command(name = "plumb", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a node: set up an index from public seed data on first start,
    /// serve the search page, and keep crawling homepages to refresh the index.
    Run(RunArgs),
    /// Download the seed datasets: Tranco, Common Crawl domain ranks and
    /// Wikidata official websites.
    FetchData(FetchDataArgs),
    /// Make a page set file (Wikipedia articles) from Wikimedia's dumps,
    /// for a node to list single pages with its sites.
    FetchPages(FetchPagesArgs),
    /// Fold seed data and earlier records into one records file.
    Ingest(IngestArgs),
    /// Fetch the homepages of the best-scored records and merge what they say.
    Crawl(CrawlArgs),
    /// Build the search index from a records file.
    Index(IndexArgs),
    /// Search the index from the command line.
    Search(SearchArgs),
    /// Serve the search page and a JSON API over HTTP.
    Serve(ServeArgs),
    /// Check how often the official site ranks first for a list of queries.
    Eval(EvalArgs),
    /// Make a vector of each site's text with a small embedding model
    /// (downloaded on first use), so searches can find sites by meaning.
    Embed(EmbedArgs),
    /// Let the Plumb Search app on another computer change this node's
    /// settings: `on` makes a new token (shown once), `off` stops it.
    RemoteControl(RemoteControlArgs),
    /// Measure node storage and private-search bucket sizes without changing data.
    Storage(StorageArgs),
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
pub struct RemoteControlArgs {
    /// The node's data directory, as given to `plumb run --data`.
    #[arg(long, value_name = "DIR", global = true, default_value = ".")]
    pub data: PathBuf,
    #[command(subcommand)]
    pub action: RemoteControlAction,
}

#[derive(Debug, Clone, Subcommand)]
pub enum RemoteControlAction {
    /// Turn remote control on with a new token, which replaces any earlier
    /// one, and print it.
    On {
        /// Also take requests from public addresses and through reverse
        /// proxies. Without it, only this computer and local networks
        /// (and Tailscale) can use the token. Put the node behind HTTPS
        /// first, or the token crosses the internet in the clear.
        #[arg(long)]
        allow_public: bool,
    },
    /// Turn remote control off: no token works any more.
    Off,
    /// Say whether remote control is on.
    Status,
}

#[derive(Debug, Args)]
pub struct EmbedArgs {
    /// Records file whose sites to embed.
    #[arg(long, value_name = "FILE")]
    pub records: PathBuf,
    /// Directory of the model's files, downloaded when missing.
    #[arg(long, value_name = "DIR")]
    pub model: PathBuf,
    /// Vectors file, created or brought up to date: only sites whose text
    /// changed are embedded again.
    #[arg(long, value_name = "FILE")]
    pub vectors: PathBuf,
    /// Texts embedded at once [default: one per CPU].
    #[arg(long, value_name = "N", value_parser = parse_positive)]
    pub threads: Option<usize>,
}

/// Search by meaning too, for queries that name no site.
#[derive(Debug, Clone, Default, Args)]
pub struct MeaningArgs {
    /// Vectors file made by `plumb embed`.
    #[arg(long, value_name = "FILE", requires = "model")]
    pub vectors: Option<PathBuf>,
    /// Directory of the model that made the vectors.
    #[arg(long, value_name = "DIR", requires = "vectors")]
    pub model: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Data directory for the downloads, the records file and the search
    /// indexes (created if missing). One node per directory.
    #[arg(long, value_name = "DIR")]
    pub data: PathBuf,
    /// Address to listen on; 0.0.0.0:8080 serves other machines too.
    #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:8080")]
    pub bind: SocketAddr,
    /// Also serve HTTPS here, such as 0.0.0.0:8443, with a certificate the
    /// node makes for itself, so the desktop app on another computer can
    /// use remote control over a local network. The app trusts the
    /// certificate by the fingerprint `plumb remote-control on` prints.
    #[arg(long, value_name = "ADDR")]
    pub https_bind: Option<SocketAddr>,
    /// Defaults to start from. server: 1,000,000 sites, 10,000 homepages
    /// crawled at first and 5,000 more every hour. desktop: 250,000
    /// sites, 2,000 homepages at first and 1,000 more every 12 hours.
    #[arg(long, value_enum, default_value_t = Profile::Server)]
    pub profile: Profile,
    /// How many of the best-ranked sites to keep from the seed data, on
    /// first start [default: from --profile].
    #[arg(long, value_name = "N", value_parser = parse_positive)]
    pub sites: Option<usize>,
    /// Homepages to crawl once the first index is built, 0 for none
    /// [default: from --profile].
    #[arg(long, value_name = "N")]
    pub initial_crawl: Option<usize>,
    /// Hours between refreshes, which crawl more homepages and rebuild the
    /// index, e.g. 24 or 0.5 [default: from --profile].
    #[arg(long, value_name = "H", value_parser = parse_hours)]
    pub refresh_hours: Option<Duration>,
    /// Homepages crawled per refresh [default: from --profile].
    #[arg(long, value_name = "N", value_parser = parse_positive)]
    pub crawl_per_refresh: Option<usize>,
    /// Homepages fetched at once while crawling [default: 16]. The panel's
    /// workload presets (light, balanced, full) set their own.
    #[arg(long, value_name = "N", value_parser = parse_positive)]
    pub crawl_concurrency: Option<usize>,
    /// Never refresh: keep the index as the initial crawl leaves it.
    #[arg(long, conflicts_with_all = ["refresh_hours", "crawl_per_refresh"])]
    pub no_refresh: bool,
    /// Fold the seed files already in DIR/seed into the records again
    /// before starting (downloading only those more than a week old), for
    /// records made before a change to how seed data is read. Use it once.
    #[arg(long)]
    pub reseed: bool,
    /// Common Crawl web graph release to add domain ranks from on first
    /// start, such as cc-main-2025-26-nov-dec-jan (release names are listed
    /// on https://commoncrawl.org/web-graphs). Only the top rows are
    /// downloaded. Without it, Common Crawl is skipped.
    #[arg(long, value_name = "NAME", value_parser = parse_release)]
    pub cc_release: Option<String>,
    /// Weight of the popularity prior in the ranking, from 0 to 1
    /// [default: the index's default].
    #[arg(long, value_name = "A", value_parser = parse_alpha)]
    pub alpha: Option<f32>,
    /// Home country, whose sites rank a little higher and other countries'
    /// a little lower: a two-letter code such as US or DE, `any` for none,
    /// or `auto` to take it from each browser's language setting
    /// (`en-US` -> US), falling back to this computer's region settings.
    /// A search can pick another with `country=` in its address.
    #[arg(long, value_name = "CODE", default_value = "auto", value_parser = HomeCountry::parse)]
    pub country: HomeCountry,
    /// Show "Search the web with ..." above the results, a link that hands
    /// the query to this engine: duckduckgo, google, bing, brave or
    /// startpage, or `off` for none. Plumb never fetches its results.
    /// Bangs such as `!g` work either way.
    #[arg(long, value_name = "ENGINE", default_value = "off", value_parser = parse_web_search)]
    pub web_search: WebSearch,
    /// Also find sites by meaning for searches that name no site ("electric
    /// car maker"). Downloads a small embedding model (about 130 MB) into
    /// DIR/model and embeds each site's text in the background after every
    /// index build, best-ranked sites first, into DIR/vectors.bin.
    #[arg(long)]
    pub search_by_meaning: bool,
    /// Threads that embed sites for search by meaning [default: half the
    /// CPUs this node may use]. A server with CPUs to spare catches up
    /// faster with more.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u16).range(1..))]
    pub embed_threads: Option<u16>,
    /// Crawl homepages through the proxy in HTTP_PROXY, HTTPS_PROXY or
    /// ALL_PROXY (except hosts in NO_PROXY), for machines that reach the
    /// internet only through one. Without it, homepages are fetched directly
    /// (seed downloads always use those variables).
    #[arg(long)]
    pub use_system_proxy: bool,
    /// Join the Plumb network: share crawl work with other nodes, search
    /// them, and answer their searches. No port forwarding is needed.
    #[arg(long)]
    pub network: bool,
    /// Port for node-to-node connections, over TCP and QUIC (UDP).
    #[arg(
        long,
        value_name = "PORT",
        default_value_t = 4001,
        requires = "network"
    )]
    pub p2p_port: u16,
    /// A node to connect to first, as a multiaddr ending in /p2p/<id>, e.g.
    /// /ip4/192.168.1.20/tcp/4001/p2p/12D3Koo...; may be repeated. The
    /// network's own first nodes, on plumbsearch.org, are tried as well.
    #[arg(long, value_name = "MULTIADDR", requires = "network")]
    pub bootstrap: Vec<plumb_net::Multiaddr>,
    /// Do not start from the network's own first nodes on plumbsearch.org;
    /// only from --bootstrap nodes and nodes on the local network. For test
    /// networks that must stay apart from the real one.
    #[arg(long, requires = "network")]
    pub no_default_bootstrap: bool,
    /// An address other nodes can reach this one at, for a server with a
    /// public address, e.g. /ip4/203.0.113.7/tcp/4001; may be repeated.
    #[arg(long, value_name = "MULTIADDR", requires = "network")]
    pub public_addr: Vec<plumb_net::Multiaddr>,
    /// Relay connections for nodes behind NAT. Only for a node others can
    /// reach (see --public-addr).
    #[arg(long, requires = "public_addr")]
    pub relay: bool,
    /// Do not ask the home router to forward the port (UPnP).
    #[arg(long, requires = "network")]
    pub no_upnp: bool,
    /// Do not look for other Plumb nodes on the local network (mDNS).
    #[arg(long, requires = "network")]
    pub no_local_discovery: bool,
    /// A node (by its id, 12D3Koo...) whose crawls this node takes in as
    /// soon as it signs them, without waiting for a second crawler to
    /// agree. Other nodes' crawls still need agreement. May be repeated.
    #[arg(long = "trust-peer", value_name = "PEER_ID", requires = "network")]
    pub trust_peer: Vec<plumb_net::PeerId>,
    /// Do not trust the plumbsearch.org node by default; only nodes given
    /// with --trust-peer.
    #[arg(long, requires = "network")]
    pub no_default_trust: bool,
    /// Offer private search at /private even off the network. Nodes that
    /// answer other nodes' searches offer it anyway, since they already
    /// have the groups of sites (buckets) browsers fetch and rank
    /// themselves. Query text stays in the browser, but requested buckets
    /// can reveal likely searches. Buckets take about as much disk again as
    /// the records file.
    #[arg(long)]
    pub private_search: bool,
    /// Keep a search history for each browser that searches this node, in
    /// DIR/history: its past searches and the sites it opened, which the
    /// search page can show and rank higher. Each browser sees only its
    /// own. For a node only you and people you live with search; leave it
    /// off on a public server.
    #[arg(long)]
    pub search_history: bool,
    /// A topic this node focuses on, such as "games"; may be repeated. The
    /// node crawls the sites about it first and twice as often, and keeps
    /// more of them when it fills a storage limit, so the more nodes focus
    /// on a topic, the better the network knows it. Other nodes can tell
    /// from what this node crawls. Topics set on the panel are added.
    #[arg(long, value_name = "TOPIC")]
    pub focus: Vec<String>,
    /// Share which result is opened for a search, anonymously: this node
    /// notes the pick (on its own disk, for the current week) and sends a
    /// few threshold-encrypted reports a day, which no node can read until
    /// many nodes report the same pick.
    #[arg(long, requires = "network")]
    pub share_popularity: bool,
    /// Also share the homepages crawled into this records file, such as one
    /// `plumb crawl` is filling (its journal included), and add them to this
    /// node's own records: every half hour, those crawled since the last
    /// time and within the last 6 days.
    #[arg(long, value_name = "PATH", requires = "network")]
    pub publish_records: Option<PathBuf>,
    /// Crawl any site, not only the ones the network assigns this node each
    /// day. For a person's own nodes: only nodes that trust this one take
    /// its crawls of sites it was not assigned. With --crawl-with, the
    /// sites are split by hash so the nodes don't overlap.
    #[arg(long, requires = "network")]
    pub crawl_any_site: bool,
    /// Another node (12D3Koo...) crawling with --crawl-any-site to split the
    /// sites with; may be repeated. Each site goes to one of this node and
    /// the ones given that crawled in the last day.
    #[arg(long, value_name = "PEER_ID", requires = "crawl_any_site")]
    pub crawl_with: Vec<plumb_net::PeerId>,
    /// Minutes between scheduled background rounds of bucket requests.
    /// Searches queue missing buckets for these rounds; 0 disables them and
    /// fetches immediately when searching [default: 10].
    #[arg(long, value_name = "MINUTES", requires = "network")]
    pub round_minutes: Option<u64>,
    /// Days of the network's crawl batches to keep on disk [default: 35].
    #[arg(long, value_name = "DAYS", requires = "network", value_parser = clap::value_parser!(u64).range(1..))]
    pub keep_batches_days: Option<u64>,
    /// Don't ask trusted nodes for their crawled sites to fill free space
    /// (up to 90% of the storage limit set on the panel, or all of their
    /// sites with no limit).
    #[arg(long, requires = "network")]
    pub no_fill: bool,
    /// On first start, download the seed data (Tranco, Common Crawl,
    /// Wikidata, Wikipedia) even in the network. Without it, a new node
    /// in the network sets up from the sites of a node it trusts, and
    /// downloads the seed data only when none answers. Setting up from
    /// the network needs filling (no --no-fill) and a trusted node.
    #[arg(long)]
    pub seed_from_outside: bool,
}

/// Starting points for `plumb run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Profile {
    /// A server or homelab: more sites and more crawling.
    Server,
    /// A desktop or laptop: a smaller index and lighter crawling.
    Desktop,
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
    /// (a notability filter that keeps the query small enough to finish).
    #[arg(long, value_name = "N", default_value_t = 25)]
    pub wikidata_min_sitelinks: u32,
    /// Keep each file an earlier run saved in --dir within this many days
    /// instead of fetching it again, so a rerun only fetches what is missing,
    /// stale or failed. A file copied in from another run's folder counts
    /// as just saved. 0 fetches everything.
    #[arg(long, value_name = "DAYS", default_value_t = 0)]
    pub keep_days: u64,
}

#[derive(Debug, Args)]
pub struct FetchPagesArgs {
    /// The page set to make: wikipedia-en (English Wikipedia's articles)
    /// or github (GitHub repositories, from GitHub's search API; set
    /// GITHUB_TOKEN to search three times as fast).
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
    #[arg(long, value_name = "STARS", default_value_t = plumb_ingest::github::DEFAULT_MIN_STARS)]
    pub min_stars: u64,
    /// GitHub: most repositories kept.
    #[arg(long, value_name = "N", default_value_t = 1_000_000)]
    pub max_repos: usize,
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("sources")
        .required(true)
        .multiple(true)
        .args(["tranco", "cc_ranks", "wat", "wikidata", "records"])
))]
pub struct IngestArgs {
    /// Tranco list: the .zip as downloaded, a .csv or a .csv.gz.
    #[arg(long, value_name = "PATH")]
    pub tranco: Option<PathBuf>,
    /// Common Crawl domain ranks file (.txt or .txt.gz).
    #[arg(long, value_name = "PATH")]
    pub cc_ranks: Option<PathBuf>,
    /// Common Crawl WAT files, gzipped or plain (list several, or repeat the flag).
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub wat: Vec<PathBuf>,
    /// Wikidata official websites (the TSV that fetch-data writes).
    #[arg(long, value_name = "PATH")]
    pub wikidata: Option<PathBuf>,
    /// Countries and kinds of the organizations behind Wikidata's official
    /// websites (the wikidata-site-facts.tsv that fetch-data writes); needs
    /// --wikidata.
    #[arg(long, value_name = "PATH", requires = "wikidata")]
    pub wikidata_facts: Option<PathBuf>,
    /// The first sentences of the Wikipedia articles about the best-known
    /// of those organizations (the wikipedia-intros.tsv that fetch-data
    /// writes); needs --wikidata.
    #[arg(long, value_name = "PATH", requires = "wikidata")]
    pub wikipedia_intros: Option<PathBuf>,
    /// Wikidata official websites of banks, credit unions, airlines and other
    /// kinds of organizations, whatever their sitelinks (the
    /// wikidata-kind-sites.tsv that fetch-data writes); needs --wikidata.
    #[arg(long, value_name = "PATH", requires = "wikidata")]
    pub wikidata_kinds: Option<PathBuf>,
    /// Records files from an earlier ingest or crawl to merge in (list several,
    /// or repeat the flag), each with the journal an interrupted crawl may
    /// have left next to it (PATH.journal). Seed files given with them
    /// replace their seed signals.
    ///
    /// --tranco replaces their Tranco ranks, --cc-ranks their Common Crawl
    /// ranks and --wikidata their official-site marks, so a domain that has
    /// lost its rank or its listing (one that expired and was registered
    /// again, say) does not keep them. Aliases do not say where they came
    /// from, so --wikidata also drops every alias of a site that was marked
    /// official; the new Wikidata file adds back the labels of sites it still
    /// lists, and crawls add back og:site_name on the next fetch. Other sites
    /// keep their aliases, and every site keeps its page fields, link text,
    /// linking-domain count and crawl times.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub records: Vec<PathBuf>,
    /// Read at most N entries from the Tranco list and the Common Crawl ranks
    /// (both sorted best first), and keep the best N records of each records
    /// file [default: the Common Crawl ranks stop at twice --top, or at
    /// 1000000 without --top].
    ///
    /// The full Common Crawl ranks file has over 100M rows, and each million
    /// read takes about 0.9 GB of memory. Without this flag the Tranco list is
    /// read whole; WAT, Wikidata and records files always are.
    #[arg(long, value_name = "N")]
    pub limit_per_source: Option<usize>,
    /// Keep only the N records with the best link score. This also bounds how
    /// much of the Common Crawl ranks is read (see --limit-per-source).
    #[arg(long, value_name = "N")]
    pub top: Option<usize>,
    /// Where to write the records (JSON lines). A file already there is
    /// replaced, and a crawl journal next to it (PATH.journal) deleted; to
    /// keep what they hold, list the file under --records too.
    #[arg(long, value_name = "PATH")]
    pub out: PathBuf,
}

#[derive(Debug, Args)]
pub struct CrawlArgs {
    /// Records file to pick homepages from (JSON lines).
    #[arg(long, value_name = "PATH")]
    pub records: PathBuf,
    /// How many homepages to fetch: half from sites never tried and half from
    /// sites due again, best link score first in each (a half with too few
    /// leaves the rest to the other).
    #[arg(long, value_name = "N", default_value_t = 1000)]
    pub top: usize,
    /// Days before a homepage that was fetched, or answered with an error,
    /// is due again. Sites that could not be reached at all are retried
    /// after 1 day, then 2, 4, 8... days, up to this.
    #[arg(long, value_name = "D", default_value_t = 30)]
    pub skip_crawled_within_days: u64,
    /// Homepage fetches in flight at once.
    #[arg(long, value_name = "N", default_value_t = 16, value_parser = parse_positive)]
    pub concurrency: usize,
    /// Host name lookups in flight at once. Home routers drop lookups when
    /// hundreds arrive together; lower this if many sites come back as
    /// "could not be reached" at a high --concurrency.
    #[arg(long, value_name = "N", default_value_t = 32, value_parser = parse_positive)]
    pub dns_lookups: usize,
    /// Where to write the updated records [default: overwrite --records].
    /// Each batch of homepages is saved at once to a journal next to it
    /// (PATH.journal), which is folded in at the end, so an interrupted crawl
    /// keeps what it fetched; the next crawl or index of PATH picks it up.
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
    /// Fetch homepages through the proxy in HTTP_PROXY, HTTPS_PROXY or
    /// ALL_PROXY (except hosts in NO_PROXY), for machines that reach the
    /// internet only through one. Without it, homepages are fetched directly.
    #[arg(long)]
    pub use_system_proxy: bool,
}

#[derive(Debug, Args)]
pub struct IndexArgs {
    /// Records file to index (JSON lines).
    #[arg(long, value_name = "PATH")]
    pub records: PathBuf,
    /// Index directory; an index already there is replaced.
    #[arg(long, value_name = "DIR")]
    pub index: PathBuf,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    /// Index directory.
    #[arg(long, value_name = "DIR")]
    pub index: PathBuf,
    /// Number of results.
    #[arg(long, value_name = "N", default_value_t = 10, value_parser = parse_positive)]
    pub limit: usize,
    /// Weight of the popularity prior in the ranking, from 0 to 1
    /// [default: the index's default].
    #[arg(long, value_name = "A", value_parser = parse_alpha)]
    pub alpha: Option<f32>,
    /// Print the hits as JSON.
    #[arg(long)]
    pub json: bool,
    /// Home country, a two-letter code such as US or DE: its sites rank a
    /// little higher, other countries' a little lower [default: none].
    #[arg(long, value_name = "CODE", value_parser = parse_country)]
    pub country: Option<String>,
    /// Leave out other countries' sites (needs --country).
    #[arg(long, requires = "country")]
    pub only_country: bool,
    /// Search for the query exactly as typed, without correcting typos.
    #[arg(long)]
    pub exact: bool,
    #[command(flatten)]
    pub meaning: MeaningArgs,
    /// What to search for, e.g. `us bank`.
    #[arg(required = true, value_name = "QUERY")]
    pub query: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Index directory.
    #[arg(long, value_name = "DIR")]
    pub index: PathBuf,
    /// Address to listen on.
    #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:8080")]
    pub bind: SocketAddr,
    /// Weight of the popularity prior in the ranking, from 0 to 1
    /// [default: the index's default].
    #[arg(long, value_name = "A", value_parser = parse_alpha)]
    pub alpha: Option<f32>,
    /// Home country, whose sites rank a little higher and other countries'
    /// a little lower: a two-letter code such as US or DE, `any` for none,
    /// or `auto` to take it from each browser's language setting
    /// (`en-US` -> US), falling back to this computer's region settings.
    /// A search can pick another with `country=` in its address.
    #[arg(long, value_name = "CODE", default_value = "auto", value_parser = HomeCountry::parse)]
    pub country: HomeCountry,
    /// Show "Search the web with ..." above the results, a link that hands
    /// the query to this engine: duckduckgo, google, bing, brave or
    /// startpage, or `off` for none. Plumb never fetches its results.
    /// Bangs such as `!g` work either way.
    #[arg(long, value_name = "ENGINE", default_value = "off", value_parser = parse_web_search)]
    pub web_search: WebSearch,
    #[command(flatten)]
    pub meaning: MeaningArgs,
}

#[derive(Debug, Args)]
pub struct EvalArgs {
    /// Index directory.
    #[arg(long, value_name = "DIR")]
    pub index: PathBuf,
    /// Queries file: `query<TAB>expected_domain[,another_ok_domain]` per line;
    /// blank lines and lines starting with `#` are skipped.
    #[arg(long, value_name = "TSV")]
    pub queries: PathBuf,
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
    /// Search for each query exactly as written, without correcting typos.
    #[arg(long)]
    pub exact: bool,
    /// Ranking knobs to change, as JSON, e.g. '{"exact_label_bonus": 0.1}'.
    /// The other knobs keep their defaults; --alpha wins over an alpha here.
    #[arg(long, value_name = "JSON", value_parser = parse_rank_config)]
    pub rank: Option<plumb_index::RankConfig>,
    /// For each miss, also show how the first site and the expected one
    /// scored: final score, text match, link score and closeness in meaning.
    #[arg(long)]
    pub explain: bool,
    /// Page set files (wikipedia-en.tsv.gz, github.tsv.gz from fetch-pages)
    /// whose pages are listed among the sites, as a node lists them. Can be
    /// given more than once.
    #[arg(long, value_name = "PATH")]
    pub pages: Vec<PathBuf>,
    /// How many of each page set's most read pages to keep.
    #[arg(long, value_name = "N", default_value_t = usize::MAX, hide_default_value = true)]
    pub pages_top: usize,
    #[command(flatten)]
    pub meaning: MeaningArgs,
}

fn parse_rank_config(s: &str) -> Result<plumb_index::RankConfig, String> {
    serde_json::from_str(s).map_err(|err| format!("expected ranking knobs as JSON: {err}"))
}

fn parse_positive(s: &str) -> Result<usize, String> {
    match s.trim().parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("expected a whole number above 0, got `{s}`")),
    }
}

fn parse_country(text: &str) -> Result<String, String> {
    plumb_core::normalize_country(text)
        .ok_or_else(|| format!("expected a two-letter country code such as US or DE, got {text:?}"))
}

fn parse_alpha(s: &str) -> Result<f32, String> {
    match s.trim().parse::<f32>() {
        Ok(a) if (0.0..=1.0).contains(&a) => Ok(a),
        _ => Err(format!("expected a number from 0 to 1, got `{s}`")),
    }
}

/// Hours, possibly fractional, as a duration of at least one second.
fn parse_hours(s: &str) -> Result<Duration, String> {
    let hours = s
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|h| h.is_finite() && *h > 0.0);
    hours
        .and_then(|h| Duration::try_from_secs_f64(h * 3600.0).ok())
        .filter(|d| d.as_secs() >= 1)
        .ok_or_else(|| format!("expected a number of hours above 0, such as 24 or 0.5, got `{s}`"))
}

fn parse_release(s: &str) -> Result<String, String> {
    crate::node::check_release_name(s)
        .map(|()| s.trim().to_string())
        .map_err(|err| err.to_string())
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
        Cli::try_parse_from(std::iter::once("plumb").chain(args.iter().copied()))
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
    fn definitions_are_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn ingest_needs_a_source() {
        let err = parse(&["ingest", "--out", "r.jsonl"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        let cli = parse(&[
            "ingest",
            "--wat",
            "a.wat",
            "b.wat.gz",
            "--wat",
            "c.wat",
            "--records",
            "old.jsonl",
            "--out",
            "r.jsonl",
        ])
        .unwrap();
        let Command::Ingest(args) = cli.command else {
            panic!("not ingest");
        };
        assert_eq!(args.wat.len(), 3);
        assert_eq!(args.records, vec![PathBuf::from("old.jsonl")]);
        assert_eq!(args.tranco, None);
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
        assert_eq!(args.wikidata_min_sitelinks, 25);
        assert!(!args.skip_tranco && !args.skip_wikidata);
    }

    #[test]
    fn eval_and_serve_defaults() {
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

        let cli = parse(&["serve", "--index", "idx"]).unwrap();
        let Command::Serve(args) = cli.command else {
            panic!("not serve");
        };
        assert_eq!(args.bind, "127.0.0.1:8080".parse().unwrap());
    }

    #[test]
    fn run_defaults_and_overrides() {
        let cli = parse(&["run", "--data", "/data"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!("not run");
        };
        assert_eq!(args.data, PathBuf::from("/data"));
        assert_eq!(args.bind, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(args.profile, Profile::Server);
        assert_eq!(
            (args.sites, args.initial_crawl, args.refresh_hours),
            (None, None, None)
        );
        assert!(!args.no_refresh);
        assert!(!args.use_system_proxy);
        assert!(parse(&["run"]).is_err());

        let cli = parse(&[
            "run",
            "--data",
            "d",
            "--bind",
            "0.0.0.0:8080",
            "--profile",
            "desktop",
            "--sites",
            "1000",
            "--initial-crawl",
            "0",
            "--refresh-hours",
            "0.5",
            "--crawl-per-refresh",
            "10",
            "--cc-release",
            "cc-main-2025-26-nov-dec-jan",
            "--alpha",
            "0.5",
        ])
        .unwrap();
        let Command::Run(args) = cli.command else {
            panic!("not run");
        };
        assert_eq!(args.bind, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(args.profile, Profile::Desktop);
        assert_eq!(args.sites, Some(1000));
        assert_eq!(args.initial_crawl, Some(0));
        assert_eq!(args.refresh_hours, Some(Duration::from_secs(1800)));
        assert_eq!(args.crawl_per_refresh, Some(10));
        assert_eq!(
            args.cc_release.as_deref(),
            Some("cc-main-2025-26-nov-dec-jan")
        );
        assert_eq!(args.alpha, Some(0.5));
    }

    #[test]
    fn run_rejects_bad_values() {
        let run = |extra: &[&str]| {
            let mut args = vec!["run", "--data", "d"];
            args.extend_from_slice(extra);
            parse(&args)
        };
        for bad in [
            &["--sites", "0"][..],
            &["--refresh-hours", "0"],
            &["--refresh-hours", "-1"],
            &["--refresh-hours", "soon"],
            &["--refresh-hours", "inf"],
            &["--crawl-per-refresh", "0"],
            &["--cc-release", "https://data.commoncrawl.org/x"],
            &["--cc-release", "../etc"],
            &["--alpha", "2"],
            &["--profile", "laptop"],
            &["--bind", "localhost"],
        ] {
            assert!(run(bad).is_err(), "{bad:?}");
        }
        for conflict in [
            &["--no-refresh", "--refresh-hours", "2"][..],
            &["--no-refresh", "--crawl-per-refresh", "2"],
        ] {
            let err = run(conflict).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{conflict:?}"
            );
        }
        let Command::Run(args) = run(&["--no-refresh"]).unwrap().command else {
            panic!("not run");
        };
        assert!(args.no_refresh);
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
