//! Command-line arguments of the `plumb` binary.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

use crate::country::HomeCountry;
pub use crate::meaning::MeaningModel;
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
    /// Make the map file the places' maps are drawn from: streets, water,
    /// parks and town names (OpenStreetMap, via the Protomaps basemap) for
    /// the whole world at low zooms and in detail around chosen points.
    FetchMap(crate::map::fetch::FetchMapArgs),
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
    /// Fold seed data and earlier records into one records file.
    Ingest(IngestArgs),
    /// Fetch the homepages of the best-scored records and merge what they say.
    Crawl(CrawlArgs),
    /// Build the search index from a records file.
    Index(IndexArgs),
    /// Search the index from the command line.
    Search(SearchArgs),
    /// Show what an index's spelling model learned from its words: the
    /// commonest slips, how likely given typos are, and completions.
    Spelling(SpellingArgs),
    /// Serve the search page and a JSON API over HTTP.
    Serve(ServeArgs),
    /// Check how often the official site ranks first for a list of queries.
    Eval(EvalArgs),
    /// Check that the pages a queries file expects are in the page sets.
    CheckLabels(crate::eval_labels::CheckLabelsArgs),
    /// Train the learned ranking on the test searches `eval
    /// --features-out` wrote, and measure it on the half it did not see.
    TrainRank(crate::train_rank::TrainRankArgs),
    /// Make a vector of each site's text with a small embedding model
    /// (downloaded on first use), so searches can find sites by meaning.
    Embed(EmbedArgs),
    /// Fetch homepages and keep their visible text, for `plumb terms`
    /// (an experiment).
    FetchText(crate::terms::FetchTextArgs),
    /// Pick each site's search terms from its homepage text, made by
    /// `plumb fetch-text`, into the records (an experiment).
    Terms(crate::terms::TermsArgs),
    /// One sentence about each well-known site with no text, written by a
    /// language model: `pick` the sites, then `apply` the sentences.
    Summaries(crate::summaries::SummariesArgs),
    /// Let the Plumb Search app on another computer change this node's
    /// settings: `on` makes a new token (shown once), `off` stops it.
    RemoteControl(RemoteControlArgs),
    /// Measure node storage and private-search bucket sizes without changing data.
    Storage(StorageArgs),
    /// Count the sites in a node's records that look dead, the ones
    /// `plumb run --drop-dead-sites` takes out of its index, without
    /// changing anything. Reads the whole records file into memory.
    DeadSites(DeadSitesArgs),
    /// How a node's ranking experiments (its `experiments.json`) are
    /// doing: each against its layer's control, with 95% confidence
    /// intervals. Reads the results; changes nothing.
    Experiments(ExperimentsArgs),
    /// Write out the searches and the sites opened for them that browsers
    /// chose to have kept as training examples (the settings gear's "Use
    /// my searches to train Plumb's ranking"), with how much each site is
    /// wanted once corrected for its place on the page.
    ClickLabels(ClickLabelsArgs),
    /// Rank the sites in a records file by the links their homepages make
    /// to each other (a PageRank of our own crawls), without changing
    /// anything. A look at the link graph; nothing uses the ranks yet.
    LinkRank(LinkRankArgs),
    /// How often each site's pages state Wikidata's facts right, from
    /// Common Crawl's page text (Knowledge-Based Trust), and optionally a
    /// copy of a records file with each site's counts, which its link
    /// score counts in.
    FactTrust(crate::fact_trust::FactTrustArgs),
    /// Learn each kind of Wikidata fact (capital, founder, CEO...) as a
    /// map between the vectors of Wikipedia articles, and measure how well
    /// the maps find facts they were not shown (an experiment).
    Relations(crate::relations::RelationsArgs),
    /// How much of a records file the best sites take (by link score), by
    /// kind and at 100k, 250k, 500k... sites, and optionally a copy cut
    /// down to the best of them. The records file is left as it is.
    TopSites(TopSitesArgs),
    /// Let AI apps on this computer (Claude Desktop, Claude Code, ...) ask
    /// Plumb for official sites and look-alikes: an MCP server over stdin
    /// and stdout.
    Mcp(McpArgs),
    /// Run a plugin on one search and print its results as JSON, to try a
    /// plugin before installing it (see docs/plugins.md).
    TryPlugin(TryPluginArgs),
    /// Check that a running node answers: exits 0 when its /api/status
    /// answers with success, 1 otherwise. The Docker image's HEALTHCHECK.
    Healthcheck(HealthcheckArgs),
}

#[derive(Debug, Args)]
pub struct HealthcheckArgs {
    /// The node's web address, as served by `plumb run --bind`.
    #[arg(long, value_name = "URL", default_value = "http://127.0.0.1:8080")]
    pub url: String,
    /// How long to wait for the answer, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = 5)]
    pub timeout: u64,
}

#[derive(Debug, Args)]
pub struct TryPluginArgs {
    /// The plugin's folder: plugin.json, plugin.wasm and optionally
    /// config.json.
    #[arg(long, value_name = "DIR")]
    pub plugin: PathBuf,
    /// What to search for, keyword included or not, or the address of a
    /// page for a plugin with `pages`.
    #[arg(required_unless_present_any = ["act", "annotate"], num_args = 1..)]
    pub query: Vec<String>,
    /// Press one of its buttons instead: the button's data, as JSON (the
    /// `data` of an action in its results), and print what it did.
    #[arg(long, value_name = "JSON")]
    pub act: Option<String>,
    /// Have it mark up results instead: a JSON file of results as a
    /// node shows them to plugins (a list of `{"id", "url", "title",
    /// "site", "about"}`), and print its notes.
    #[arg(long, value_name = "FILE", conflicts_with = "act")]
    pub annotate: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct McpArgs {
    /// The Plumb node to ask: its web address. Its `/mcp` answers.
    #[arg(long, value_name = "URL", default_value = "https://plumbsearch.org")]
    pub node: String,
    /// Answer from this index directory instead of asking a node.
    #[arg(long, value_name = "DIR", conflicts_with = "node")]
    pub index: Option<PathBuf>,
    /// Home country for answers from --index, a two-letter code such as US
    /// [default: none].
    #[arg(long, value_name = "CODE", value_parser = parse_country, requires = "index")]
    pub country: Option<String>,
    /// Also offer the tool `relate`, from the relation maps `plumb
    /// relations` wrote to this directory (an experiment).
    #[arg(long, value_name = "DIR")]
    pub relations: Option<PathBuf>,
    /// The model that made the maps' vectors, so names that are not among
    /// the maps' articles can be embedded.
    #[arg(long, value_name = "DIR", requires = "relations")]
    pub relations_model: Option<PathBuf>,
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
pub struct DeadSitesArgs {
    /// The node's data directory, as given to `plumb run --data`.
    #[arg(long, value_name = "DIR")]
    pub data: PathBuf,
    /// How many of the best-known dead sites to name.
    #[arg(long, value_name = "N", default_value_t = 20)]
    pub show: usize,
}

#[derive(Debug, Args)]
pub struct ClickLabelsArgs {
    /// The node's data directory, as given to `plumb run --data`.
    #[arg(long, value_name = "DIR")]
    pub data: PathBuf,
    /// Write every search and site here, as JSON lines: `query`,
    /// `domain`, `shown`, `opened` and `wanted` (clicks per time shown,
    /// each counted for how far down the page it was, at most 1).
    #[arg(long, value_name = "JSONL")]
    pub out: PathBuf,
    /// Also write the searches whose clicks clearly pick one site as a
    /// queries file (`query<TAB>domain`), for `plumb eval --features-out`
    /// and then `plumb train-rank`.
    #[arg(long, value_name = "TSV")]
    pub queries: Option<PathBuf>,
    /// Times the site must have been opened for the search to go in the
    /// queries file.
    #[arg(long, value_name = "N", default_value_t = 2)]
    pub min_opened: u32,
}

#[derive(Debug, Args)]
pub struct ExperimentsArgs {
    /// The node's data directory, as given to `plumb run --data`.
    #[arg(long, value_name = "DIR")]
    pub data: PathBuf,
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
    /// Whether searches are embedded after the model's instruction for
    /// search queries ([`QueryInstruction`]); for trying it out.
    #[arg(long, value_enum, default_value_t = QueryInstruction::Off, hide = true)]
    pub query_instruction: QueryInstruction,
}

/// How a search is embedded: as it is, or after the instruction the model
/// was trained to read before a search ("Represent this sentence for
/// searching relevant passages: "), which sites' texts are not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum QueryInstruction {
    /// As it is, for the nearest sites and their closeness.
    #[default]
    Off,
    /// After the instruction, for both.
    On,
    /// Both ways, closeness being the mean of the two.
    Mix,
    /// Both ways, closeness being the lower of the two.
    Min,
    /// After the instruction for ranking sites; as it is for deciding
    /// whether a page goes before them.
    Split,
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
    /// Take sites that look dead out of the index: homepages no crawl has
    /// reached for 60 days, after 6 tries in a row over a month or more
    /// that got no answer at all (see `plumb dead-sites`, which counts them
    /// without changing anything). Never the best 10,000 sites, official
    /// websites, sites about this node's topics, or ones an About page puts
    /// first or a searcher opened. A dead site keeps a small record and
    /// comes back when a crawl reaches it again.
    #[arg(long)]
    pub drop_dead_sites: bool,
    /// Add sites the records do not hold yet: domains crawls find linked
    /// from the sites held, and new sites in other nodes' shared crawls.
    /// Off by default while the network holds its list of sites steady:
    /// crawls only refresh the sites held.
    #[arg(long)]
    pub take_new_sites: bool,
    /// Only crawl, for a small server that supports the network and that
    /// nobody searches: crawl rounds go on and publish their batches, but
    /// no search index is built, no page sets, places or feeds are kept,
    /// and other nodes' crawls are not folded in, so millions of sites fit
    /// in well under a gigabyte of memory. Search by meaning and private
    /// search are off. Without it again, the node builds its index.
    #[arg(long, conflicts_with_all = ["search_by_meaning", "private_search", "blackhole"])]
    pub crawl_only: bool,
    /// Homepages fetched at once while crawling [default: 16]. The panel's
    /// workload presets (light, balanced, full) set their own.
    #[arg(long, value_name = "N", value_parser = parse_positive)]
    pub crawl_concurrency: Option<usize>,
    /// Watch the RSS or Atom feeds of this many of the best-ranked sites
    /// for the results page's "Recent" block, 0 for none [default: 3000,
    /// 300 with --profile desktop]. Each feed is checked at most hourly.
    #[arg(long, value_name = "N")]
    pub news_feeds: Option<usize>,
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
    /// (`en-US` -> US), falling back to this computer's region settings,
    /// then the United States. A search can pick another with `country=` in its address.
    #[arg(long, value_name = "CODE", default_value = "auto", value_parser = HomeCountry::parse)]
    pub country: HomeCountry,
    /// Start the search pages with "Only this country" on, leaving out
    /// other countries' sites until the settings gear turns it off. The
    /// JSON API and `/mcp` still need `only=1`.
    #[arg(long)]
    pub only_country: bool,
    /// Language of the sites searches show when they do not pick one, a
    /// code such as en [default: the browser's first language when the
    /// settings gear offers it, else en].
    #[arg(long, value_name = "CODE", value_parser = parse_language)]
    pub lang: Option<String>,
    /// Show "Search the web with ..." above the results, a link that hands
    /// the query to this engine: duckduckgo, google, bing, brave or
    /// startpage, or `off` for none. Plumb never fetches its results.
    /// Bangs such as `!g` work either way.
    #[arg(long, value_name = "ENGINE", default_value = "off", value_parser = parse_web_search)]
    pub web_search: WebSearch,
    /// Let every client of `/mcp`, not only AI apps on this computer, use
    /// its `read_page` tool, which fetches a page from this node. For a
    /// node on a home network whose AI apps run on other computers; never
    /// on a node the whole internet can reach.
    #[arg(long)]
    pub mcp_read_pages: bool,
    /// Also find sites by meaning for searches that name no site ("electric
    /// car maker"). Downloads a small embedding model (about 130 MB) into
    /// DIR/model and embeds each site's text in the background after every
    /// index build, best-ranked sites first, into DIR/vectors.bin.
    #[arg(long)]
    pub search_by_meaning: bool,
    /// The model search by meaning runs: `small` (bge-small-en-v1.5, about
    /// 130 MB, English) or `gemma` (EmbeddingGemma 2, about 310 MB, many
    /// languages) into DIR/model-gemma. Switching makes the vectors again,
    /// or takes them from trusted nodes running the same model.
    #[arg(long, value_name = "MODEL", value_enum, default_value_t = MeaningModel::Small, requires = "search_by_meaning")]
    pub meaning_model: MeaningModel,
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
    /// Which nodes network searches ask: `trusted` (only the nodes this
    /// node trusts), `friends-of-friends` (those and the nodes they trust)
    /// or `anyone`. Friends of friends unless given.
    #[arg(long, value_name = "WHO", requires = "network")]
    pub search_from: Option<plumb_net::SearchScope>,
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
    /// Let AI apps on this computer share a finding with other Plumb nodes
    /// when they ask to (report_finding with share: true): the page, why it
    /// helped and the search's words as numbers, signed with this node's
    /// key. Never the search or the task, unless the app shares the search
    /// too.
    #[arg(long, requires = "network")]
    pub share_findings: bool,
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
    /// Collect all the data the network offers, for a server with disk and
    /// memory to spare: take the crawled sites of every trusted node, not
    /// just one, and go through their lists again every day; keep every
    /// page set (Wikipedia, GitHub, Stack Overflow, books, papers) in full;
    /// keep the network's crawl batches for good (unless
    /// --keep-batches-days is given) and ask each node met for all the
    /// batches still taken. The storage limit, the day's download limit and
    /// the memory an index build may take still hold, and only trusted
    /// nodes' sites are taken.
    #[arg(long, requires = "network", conflicts_with = "no_fill")]
    pub blackhole: bool,
    /// Minutes between scheduled background rounds of bucket requests.
    /// Searches queue missing buckets for these rounds; 0 disables them and
    /// fetches immediately when searching [default: 10].
    #[arg(long, value_name = "MINUTES", requires = "network")]
    pub round_minutes: Option<u64>,
    /// Days of the network's crawl batches to keep on disk [default: 35].
    #[arg(long, value_name = "DAYS", requires = "network", value_parser = clap::value_parser!(u64).range(1..))]
    pub keep_batches_days: Option<u64>,
    /// Most network searches (bucket requests) to answer for free each day
    /// for other nodes; past it, only requests that spend this node's
    /// credit tokens are answered. Searches on this node's own page are
    /// never limited [default: no limit].
    #[arg(long, value_name = "REQUESTS", requires = "network")]
    pub answer_per_day: Option<u64>,
    /// Do not spend credits: never collect tokens from the nodes this node
    /// searches, so busy nodes turn its searches away like anyone's.
    #[arg(long, requires = "network")]
    pub no_spend_credits: bool,
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
    /// Which page set files to replace by themselves when a node this one
    /// trusts has a newer one: `all` (the default; each may grow at most a
    /// quarter past this node's own at a time, so a much bigger set is not
    /// loaded unasked), `off`, or the sets to update with no limit on
    /// growth, separated by commas (`films,stackoverflow,map`; `map` is the
    /// map file). Sets a node has no file of are taken either way.
    #[arg(long, value_name = "all|off|SETS")]
    pub set_updates: Option<crate::pages::SetUpdates>,
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
    /// Where to read on when Wikidata's query service stops answering a
    /// kind's deep pages (it times out on them): by default QLever's copy
    /// of Wikidata.
    #[arg(long, value_name = "URL", default_value = plumb_ingest::item_facts::DEEP_SPARQL_URL)]
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
    #[arg(long, value_name = "N", default_value_t = plumb_ingest::openalex::DEFAULT_MIN_CITATIONS)]
    pub min_citations: u64,
    /// Papers: most papers kept, the most cited.
    #[arg(long, value_name = "N", default_value_t = 2_000_000)]
    pub max_papers: usize,
    /// Papers: most requests to CORE for free copies (fifty papers each),
    /// when CORE_API_KEY is set.
    #[arg(long, value_name = "N", default_value_t = plumb_ingest::core_ac::DEFAULT_MAX_REQUESTS)]
    pub max_core_requests: usize,
    /// Podcasts: fewest Podcast Index popularity points (0 to 9) of a
    /// podcast kept.
    #[arg(long, value_name = "N", default_value_t = plumb_ingest::podcasts::DEFAULT_MIN_SCORE)]
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
    #[arg(long, value_name = "N", default_value_t = plumb_ingest::musicbrainz::DEFAULT_MAX_SONGS)]
    pub max_songs: usize,
    /// Music: most albums kept, the most listened to.
    #[arg(long, value_name = "N", default_value_t = plumb_ingest::musicbrainz::DEFAULT_MAX_ALBUMS)]
    pub max_albums: usize,
    /// Music: fewest release groups (albums, singles, compilations) a song
    /// is on for ListenBrainz to be asked about it, unless it is on an
    /// album kept.
    #[arg(long, value_name = "N", default_value_t = plumb_ingest::musicbrainz::DEFAULT_MIN_SONG_RELEASES)]
    pub min_song_releases: u32,
    /// Music: fewest ListenBrainz listeners of a song or album kept.
    #[arg(long, value_name = "N", default_value_t = plumb_ingest::musicbrainz::DEFAULT_MIN_LISTENERS)]
    pub min_listeners: u64,
    /// Films: most films and shows kept, the most linked from Wikipedias
    /// and other wikis.
    #[arg(long, value_name = "N", default_value_t = plumb_ingest::films::DEFAULT_MAX_FILMS)]
    pub max_films: usize,
    /// Films: fewest sitelinks (Wikipedias and other wikis with a page on
    /// it) of a film or show kept.
    #[arg(long, value_name = "N", default_value_t = plumb_ingest::films::DEFAULT_MIN_SITELINKS)]
    pub min_film_sitelinks: u64,
    /// Packages: the registries to list (npm, pypi, crates, go, gem,
    /// composer, nuget, maven), comma-separated; all when left out.
    #[arg(long, value_name = "KEYS", value_delimiter = ',')]
    pub registries: Vec<String>,
    /// Packages: most packages kept of each registry, the most used.
    #[arg(long, value_name = "N", default_value_t = plumb_ingest::packages::DEFAULT_MAX_PER_REGISTRY)]
    pub max_per_registry: usize,
    /// Places: read this OpenStreetMap extract (.osm.pbf) instead of
    /// downloading the whole planet (about 90 GB) into --work.
    #[arg(long, value_name = "PATH")]
    pub osm: Option<PathBuf>,
    /// Docs: the docs sites to fetch (mdn, python, rust and others; see
    /// plumb_core::docs), comma-separated; all when left out.
    #[arg(long, value_name = "KEYS", value_delimiter = ',')]
    pub docs_sites: Vec<String>,
    /// Docs: most pages fetched of each site, the shallowest first.
    #[arg(long, value_name = "N", default_value_t = plumb_ingest::docs::DEFAULT_MAX_PER_SITE)]
    pub max_docs_per_site: usize,
    /// Reference: the reference sites to fetch, by host without `www.`
    /// (healthline.com, merriam-webster.com and others; see
    /// plumb_core::reference), comma-separated; all when left out.
    #[arg(long, value_name = "HOSTS", value_delimiter = ',')]
    pub reference_sites: Vec<String>,
    /// Reference: most pages fetched of each site, the shallowest first.
    /// Sites with a page for every word (dictionaries) take their own
    /// number, more.
    #[arg(long, value_name = "N", default_value_t = plumb_ingest::reference::DEFAULT_MAX_PER_SITE)]
    pub max_reference_per_site: usize,
    /// Subpages: the subpage sites to fetch, by host without `www.`
    /// (nist.gov, chessprogramming.org and others; see
    /// plumb_core::subpages), comma-separated; all when left out.
    #[arg(long, value_name = "HOSTS", value_delimiter = ',')]
    pub subpage_sites: Vec<String>,
    /// Subpages: only the sites of these kinds (university, company,
    /// government, entertainment, museum), comma-separated; all when left
    /// out.
    #[arg(long, value_name = "KINDS", value_delimiter = ',')]
    pub subpage_kinds: Vec<String>,
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
pub struct SpellingArgs {
    /// Index directory.
    #[arg(long, value_name = "DIR")]
    pub index: PathBuf,
    /// How many of the learned slips to list, likeliest first.
    #[arg(long, value_name = "N", default_value_t = 40)]
    pub rules: usize,
    /// A typo and the word meant, as `typed:meant` (`amtrack:amtrak`): how
    /// likely the slip is and how common each word is. May be repeated.
    #[arg(long, value_name = "TYPED:MEANT")]
    pub pair: Vec<String>,
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
    /// Search for the query without suggesting a spelling.
    #[arg(long)]
    pub exact: bool,
    #[command(flatten)]
    pub meaning: MeaningArgs,
    /// A places file (places.tsv.gz from `fetch-pages --set places`): also
    /// list the places the query asks for. Indexed next to it on first use.
    #[arg(long, value_name = "PATH")]
    pub places: Option<PathBuf>,
    /// The town "near me" means, with --places.
    #[arg(long, value_name = "TOWN", requires = "places")]
    pub town: Option<String>,
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
    /// (`en-US` -> US), falling back to this computer's region settings,
    /// then the United States. A search can pick another with `country=` in its address.
    #[arg(long, value_name = "CODE", default_value = "auto", value_parser = HomeCountry::parse)]
    pub country: HomeCountry,
    /// Start the search pages with "Only this country" on, leaving out
    /// other countries' sites until the settings gear turns it off. The
    /// JSON API and `/mcp` still need `only=1`.
    #[arg(long)]
    pub only_country: bool,
    /// Language of the sites searches show when they do not pick one, a
    /// code such as en [default: the browser's first language when the
    /// settings gear offers it, else en].
    #[arg(long, value_name = "CODE", value_parser = parse_language)]
    pub lang: Option<String>,
    /// Show "Search the web with ..." above the results, a link that hands
    /// the query to this engine: duckduckgo, google, bing, brave or
    /// startpage, or `off` for none. Plumb never fetches its results.
    /// Bangs such as `!g` work either way.
    #[arg(long, value_name = "ENGINE", default_value = "off", value_parser = parse_web_search)]
    pub web_search: WebSearch,
    /// Let every client of `/mcp`, not only AI apps on this computer, use
    /// its `read_page` tool, which fetches a page from this node. For a
    /// node on a home network whose AI apps run on other computers; never
    /// on a node the whole internet can reach.
    #[arg(long)]
    pub mcp_read_pages: bool,
    /// A places file (places.tsv.gz from `fetch-pages --set places`), so
    /// "pizza in denver" lists places. Indexed next to it on first use.
    #[arg(long, value_name = "PATH")]
    pub places: Option<PathBuf>,
    /// A map file (map.pmtiles from `fetch-map`), so the places' map shows
    /// streets, water and parks under the pins.
    #[arg(long, value_name = "PATH", requires = "places")]
    pub map: Option<PathBuf>,
    /// A folder of plugins, one folder each, whose results show with the
    /// node's own (see docs/plugins.md). A node started with `run` uses
    /// DIR/plugins.
    #[arg(long, value_name = "DIR")]
    pub plugins: Option<PathBuf>,
    #[command(flatten)]
    pub meaning: MeaningArgs,
}

#[derive(Debug, Args)]
pub struct EvalArgs {
    /// Index directory.
    #[arg(long, value_name = "DIR")]
    pub index: PathBuf,
    /// Queries file: `query<TAB>expected_domain[,another_ok_domain]` per line;
    /// blank lines and lines starting with `#` are skipped. Can be given
    /// more than once; each file is measured on its own.
    #[arg(long, value_name = "TSV", required = true)]
    pub queries: Vec<PathBuf>,
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

/// A half of a queries file, for `plumb eval --half`.
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

pub(crate) fn parse_positive(s: &str) -> Result<usize, String> {
    match s.trim().parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("expected a whole number above 0, got `{s}`")),
    }
}

fn parse_language(text: &str) -> Result<String, String> {
    plumb_core::language_code(text)
        .ok_or_else(|| format!("expected a language code such as en or de, got {text:?}"))
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
    fn healthcheck_defaults_to_the_images_port() {
        let Command::Healthcheck(args) = parse(&["healthcheck"]).unwrap().command else {
            panic!("not healthcheck")
        };
        assert_eq!(args.url, "http://127.0.0.1:8080");
        assert_eq!(args.timeout, 5);
        let Command::Healthcheck(args) = parse(&["healthcheck", "--url", "http://[::1]:7586"])
            .unwrap()
            .command
        else {
            panic!("not healthcheck")
        };
        assert_eq!(args.url, "http://[::1]:7586");
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
        assert_eq!(args.wikidata_min_sitelinks, 3);
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
