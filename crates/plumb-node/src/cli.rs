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
/// index fresh. Step by step: `plumb-tools fetch-data`, `plumb ingest`,
/// `plumb-tools crawl` (optional) and `plumb-tools index`, then `plumb
/// serve`.
#[derive(Debug, Parser)]
#[command(name = "plumb", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the embedded version, Git revision and cleanliness as JSON.
    BuildInfo,
    /// Run a node: set up an index from public seed data on first start,
    /// serve the search page, and keep crawling homepages to refresh the index.
    Run(Box<RunArgs>),
    /// Fold seed data and earlier records into one records file.
    Ingest(IngestArgs),
    /// Serve the search page and a JSON API over HTTP.
    Serve(ServeArgs),
    /// Let the Plumb Search app on another computer change this node's
    /// settings: `on` makes a new token (shown once), `off` stops it.
    RemoteControl(RemoteControlArgs),
    /// How a node's ranking experiments (its `experiments.json`) are
    /// doing: each against its layer's control, with 95% confidence
    /// intervals. Reads the results; changes nothing.
    Experiments(ExperimentsArgs),
    /// Write out the searches and the sites opened for them that browsers
    /// chose to have kept as training examples (the settings gear's "Use
    /// my searches to train Plumb's ranking"), with how much each site is
    /// wanted once corrected for its place on the page.
    ClickLabels(ClickLabelsArgs),
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
    /// Answer tool calls with the short text alone, leaving out the same
    /// answer as JSON (`structuredContent`), for AI apps that give the
    /// model the JSON.
    #[arg(long)]
    pub text_answers: bool,
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
    /// queries file (`query<TAB>domain`), for `plumb-tools eval --features-out`
    /// and then `plumb-tools train-rank`.
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

/// Search by meaning too, for queries that name no site.
#[derive(Debug, Clone, Default, Args)]
pub struct MeaningArgs {
    /// Vectors file made by `plumb-tools embed`.
    #[arg(long, value_name = "FILE", requires = "model")]
    pub vectors: Option<PathBuf>,
    /// Directory of the model that made the vectors.
    #[arg(long, value_name = "DIR", requires = "vectors")]
    pub model: Option<PathBuf>,
    /// Whether searches are embedded after the model's instruction for
    /// search queries ([`QueryInstruction`]); for trying the others out.
    #[arg(long, value_enum, default_value_t = QueryInstruction::Split, hide = true)]
    pub query_instruction: QueryInstruction,
}

/// How a search is embedded: as it is, or after the instruction the model
/// was trained to read before a search ("Represent this sentence for
/// searching relevant passages: "), which sites' texts are not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum QueryInstruction {
    /// As it is, for the nearest sites and their closeness.
    Off,
    /// After the instruction, for both.
    On,
    /// Both ways, closeness being the mean of the two.
    Mix,
    /// Both ways, closeness being the lower of the two.
    Min,
    /// After the instruction for ranking sites; as it is for deciding
    /// whether a page goes before them. What nodes do: searches that
    /// describe a site find it more often this way (described 25% to 31%
    /// first by the hand-made order; with the learned ranking retrained
    /// on it, 47% to 50% on the held-out half, all searches 76.1% to
    /// 76.6%).
    #[default]
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
    /// that got no answer at all (see `plumb-tools dead-sites`, which counts them
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
    /// Go looking for the inner pages this node's searches want: after a
    /// search whose best sites show no page for the rest of its words, the
    /// node follows those sites' links a few pages deep to find one, and
    /// keeps what it finds for later searches. The searches' words are
    /// kept 30 days in DIR/wanted, so it suits a node only you and people
    /// you live with search; leave it off on a public server. On for
    /// --profile desktop already.
    #[arg(long)]
    pub wanted_pages: bool,
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
    /// Let every client of `/mcp`, not only AI apps on this computer, keep
    /// findings with its `report_finding` tool and see them listed with
    /// search results. Findings hold what agents searched for, so only for
    /// a node whose address you give to people you trust. Sharing findings
    /// with other nodes stays with this computer's apps.
    #[arg(long)]
    pub mcp_findings: bool,
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
    /// Read each search's first results with the reader in DIR (its
    /// config.json, tokenizer.json and model.safetensors) and rank them
    /// with the learned model trained on its scores. Plumb's 6-layer reader
    /// adds about 0.3 s to each search on two cores.
    #[arg(long, value_name = "DIR")]
    pub reader: Option<PathBuf>,
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
    /// `plumb-tools crawl` is filling (its journal included), and add them to this
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
    /// Don't apply the project's denylist (`plumb/denylist`), which takes
    /// pages and sites off search results when the project has accepted a
    /// takedown. Its entries for child sexual abuse material apply anyway.
    #[arg(long)]
    pub no_project_denylist: bool,
    /// Which page set files to replace by themselves when a node this one
    /// trusts has a newer one: `all` (the default; each may grow at most a
    /// quarter past this node's own at a time, so a much bigger set is not
    /// loaded unasked), `off`, or the sets to update with no limit on
    /// growth, separated by commas (`films,stackoverflow,map`; `map` is the
    /// map file). A set the node has no file of is taken only if this
    /// allows it; the map file is taken either way.
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
    /// Let every client of `/mcp`, not only AI apps on this computer, keep
    /// findings with its `report_finding` tool and see them listed with
    /// search results. Findings hold what agents searched for, so only for
    /// a node whose address you give to people you trust. Sharing findings
    /// with other nodes stays with this computer's apps.
    #[arg(long)]
    pub mcp_findings: bool,
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

pub fn parse_positive(s: &str) -> Result<usize, String> {
    match s.trim().parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("expected a whole number above 0, got `{s}`")),
    }
}

pub fn parse_language(text: &str) -> Result<String, String> {
    plumb_core::language_code(text)
        .ok_or_else(|| format!("expected a language code such as en or de, got {text:?}"))
}

pub fn parse_country(text: &str) -> Result<String, String> {
    plumb_core::normalize_country(text)
        .ok_or_else(|| format!("expected a two-letter country code such as US or DE, got {text:?}"))
}

pub fn parse_alpha(s: &str) -> Result<f32, String> {
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

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("plumb").chain(args.iter().copied()))
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
    fn serve_defaults() {
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
}
