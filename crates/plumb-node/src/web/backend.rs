//! What the web server searches and reports on: a search backend (an
//! index, or a node) and a node's status.

use std::sync::Arc;

use anyhow::Result;
use plumb_core::SiteRecord;
use plumb_index::pages::PageHit;
use plumb_index::{Hit, RankConfig, SearchOptions, SearchResults, Searcher};
use plumb_net::NetHandle;
use tracing::error;

use crate::meaning::{MeaningIndex, SharedMeaning};
use crate::node::{NodeSettings, Status};

/// Answers queries for the web handlers. [`IndexBackend`] is the real one;
/// tests can plug in their own.
pub trait SearchBackend: Send + Sync {
    /// Best `limit` hits for `query`, best first.
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>>;
    /// [`SearchBackend::search`] with the searcher's choices, plus a site
    /// search link. By default the choices are ignored and there is no link.
    fn search_full(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
    ) -> Result<SearchResults> {
        let _ = options;
        Ok(SearchResults {
            hits: self.search(query, limit)?,
            pages: Vec::new(),
            site_search: None,
            spelling: None,
        })
    }
    /// [`SearchBackend::search_full`] ranked with `rank` instead of the
    /// backend's own knobs, for experiments ([`crate::experiments`]). By
    /// default `rank` is ignored.
    fn search_ranked(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
        rank: &RankConfig,
    ) -> Result<SearchResults> {
        let _ = rank;
        self.search_full(query, limit, options)
    }
    /// Number of sites that can be found.
    fn num_docs(&self) -> u64;
    /// The places `query` asks for, when it asks for places somewhere
    /// ("pizza in denver"), around `home`, the searcher's own town, for
    /// "near me"; see [`plumb_index::places`]. By default there are none.
    fn places(
        &self,
        query: &str,
        home: Option<&str>,
        country: Option<&str>,
    ) -> Option<plumb_index::places::PlaceResults> {
        let _ = (query, home, country);
        None
    }
    /// The site `domain` (a canonical domain) as a hit, when the index
    /// has it. By default there are none.
    fn site(&self, domain: &str) -> Option<Hit> {
        let _ = domain;
        None
    }

    /// The town (or else other place) `text` names, for a weather answer;
    /// see [`plumb_index::places::PlaceSearcher::locate`]. By default none.
    fn locate(&self, text: &str, country: Option<&str>) -> Option<plumb_core::place::Place> {
        let _ = (text, country);
        None
    }

    /// The map the places' map is drawn on, if the node keeps one.
    fn base_map(&self) -> Option<Arc<crate::map::BaseMap>> {
        None
    }

    /// The song or album a search of `query` alone is surely for, when
    /// the node keeps the music set; see
    /// [`plumb_index::pages::PageSearcher::known_song`]. By default none.
    fn known_song(&self, query: &str, options: &SearchOptions) -> Option<plumb_index::pages::Page> {
        let _ = (query, options);
        None
    }

    /// The Wiktionary word `name` is, when the node keeps the set; see
    /// [`plumb_index::pages::PageSearcher::definition`]. By default none.
    fn definition(&self, name: &str) -> Option<plumb_index::pages::Page> {
        let _ = name;
        None
    }

    /// Bounded named-entity candidates with stable Wikidata IDs, before
    /// display blending. Backends without a page index return none.
    fn entities(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
    ) -> Result<Vec<plumb_index::pages::PageHit>> {
        let _ = (query, limit, options);
        Ok(Vec::new())
    }

    /// The best `limit` pages for `query` that `keep` keeps, best first,
    /// when the node keeps page sets: for a search that asks for one kind
    /// of page (`kind` of MCP's `search`), docs pages when `docs`. Search
    /// operators and `options` narrow them as they narrow the pages of a
    /// search. By default none.
    fn pages_of(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
        docs: bool,
        keep: &dyn Fn(&plumb_index::pages::Page) -> bool,
    ) -> Vec<plumb_index::pages::PageHit> {
        let _ = (query, limit, options, docs, keep);
        Vec::new()
    }

    /// Typed paper retrieval applies publication constraints before caps.
    fn papers(
        &self,
        query: &plumb_core::paper_query::PaperQuery,
        limit: usize,
        options: &SearchOptions,
    ) -> Result<Vec<PageHit>> {
        Ok(self.pages_of(&query.text(), limit, options, false, &|p| {
            p.set == plumb_index::pages::PAPERS_SET && query.allows(p.paper.as_ref())
        }))
    }

    fn paper_coverage(&self) -> Option<plumb_index::pages::PaperCoverage> {
        None
    }
}

/// A [`Searcher`] with fixed ranking settings.
pub struct IndexBackend {
    searcher: Searcher,
    rank: RankConfig,
    meaning: SharedMeaning,
    places: Option<plumb_index::places::PlaceSearcher>,
    map: Option<crate::map::MapFile>,
}

impl IndexBackend {
    pub fn new(searcher: Searcher, rank: RankConfig) -> Self {
        IndexBackend {
            searcher,
            rank,
            meaning: SharedMeaning::default(),
            places: None,
            map: None,
        }
    }

    /// Draws the places' map on the map in `file` (see [`crate::map`]).
    pub fn with_map(mut self, file: std::path::PathBuf) -> Self {
        self.map = Some(crate::map::MapFile::new(file));
        self
    }

    /// Also lists the places of `places` for queries that ask for them.
    pub fn with_places(mut self, places: plumb_index::places::PlaceSearcher) -> Self {
        self.places = Some(places);
        self
    }

    /// Also ranks by meaning, for queries that name no site, once
    /// `meaning` holds a model and vectors.
    pub fn with_meaning(mut self, meaning: SharedMeaning) -> Self {
        self.meaning = meaning;
        self
    }

    /// Whether the index holds `domain`.
    /// The site `domain` as a result no query found
    /// ([`Searcher::site`]).
    pub fn site(&self, domain: &str) -> Option<Hit> {
        self.searcher.site(domain).ok().flatten()
    }

    pub fn has_domain(&self, domain: &str) -> bool {
        self.searcher.has_domain(domain)
    }

    /// The index's searcher, for what it knows of words.
    pub fn searcher(&self) -> &Searcher {
        &self.searcher
    }

    /// [`SearchBackend::search_full`], ranking by `meaning` too when given,
    /// and with `rank` instead of the backend's own knobs when given.
    pub fn search_full_with(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
        meaning: Option<&MeaningIndex>,
        rank: Option<&RankConfig>,
    ) -> Result<SearchResults> {
        let query_meaning = meaning.and_then(|meaning| meaning.query(query));
        self.searcher.search_meaning(
            query,
            limit,
            rank.unwrap_or(&self.rank),
            options,
            query_meaning
                .as_ref()
                .map(|m| m as &dyn plumb_index::Meaning),
        )
    }
}

impl SearchBackend for IndexBackend {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        self.searcher.search_with(query, limit, &self.rank)
    }

    fn search_full(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
    ) -> Result<SearchResults> {
        let meaning = self.meaning.get();
        self.search_full_with(query, limit, options, meaning.as_deref(), None)
    }

    fn search_ranked(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
        rank: &RankConfig,
    ) -> Result<SearchResults> {
        let meaning = self.meaning.get();
        self.search_full_with(query, limit, options, meaning.as_deref(), Some(rank))
    }

    fn num_docs(&self) -> u64 {
        self.searcher.num_docs()
    }

    fn locate(&self, text: &str, country: Option<&str>) -> Option<plumb_core::place::Place> {
        let places = self.places.as_ref()?;
        places.locate(text, country).unwrap_or_else(|err| {
            error!("locating {text:?}: {err:#}");
            None
        })
    }

    fn places(
        &self,
        query: &str,
        home: Option<&str>,
        country: Option<&str>,
    ) -> Option<plumb_index::places::PlaceResults> {
        let places = self.places.as_ref()?;
        places
            .search(query, home, country, 8)
            .unwrap_or_else(|err| {
                error!("searching places: {err:#}");
                None
            })
    }

    fn base_map(&self) -> Option<Arc<crate::map::BaseMap>> {
        self.map.as_ref()?.get()
    }

    fn site(&self, domain: &str) -> Option<Hit> {
        if !self.searcher.has_domain(domain) {
            return None;
        }
        let options = SearchOptions {
            exact: true,
            ..SearchOptions::default()
        };
        self.searcher
            .search_full(domain, 1, &self.rank, &options)
            .ok()?
            .hits
            .into_iter()
            .find(|hit| hit.domain == domain)
    }
}

/// What a long-running node tells its web pages about itself, and what its
/// panel (`/app`) can change.
pub trait StatusSource: Send + Sync {
    /// The node's status, as `GET /api/status` returns it.
    fn status(&self) -> Status;
    /// The network side, for a node that joined the Plumb network.
    fn network(&self) -> Option<Arc<NetHandle>> {
        None
    }
    /// How the node ranks, which it also uses for what other nodes send.
    fn rank(&self) -> RankConfig {
        RankConfig::default()
    }
    /// True when the node's denylist takes off the page at `url`.
    fn denies(&self, url: &str) -> bool {
        let _ = url;
        false
    }
    /// Told what a person searched for here and what was found, so the
    /// node can go looking for the pages it wanted
    /// ([`crate::wanted_pages`]).
    fn searched(&self, query: &str, results: &SearchResults) {
        let _ = (query, results);
    }
    /// Names the bucket table private search fetches from, when the node
    /// serves private search and its index has buckets.
    fn bucket_table(&self) -> Option<String> {
        None
    }
    /// Bucket `bucket` of table `table`, its records as JSON; `None` when
    /// that table is not the one served (any more).
    fn bucket(&self, _table: &str, _bucket: u32) -> Option<Result<Vec<String>>> {
        None
    }
    /// Whether the node notes which result is opened for a search and
    /// reports it anonymously; its result links then go through `/go`.
    fn shares_popularity(&self) -> bool {
        false
    }
    /// Whether AI apps on this computer may share their findings with other
    /// nodes (see [`crate::findings`]).
    fn shares_findings(&self) -> bool {
        false
    }
    /// Notes that `domain` was opened from the results for `query`.
    fn record_pick(&self, query: &str, domain: &str) {
        let _ = (query, domain);
    }
    /// Whether safe search leaves out `domain` because it is on the
    /// node's adult blocklist.
    fn blocks_adult(&self, _domain: &str) -> bool {
        false
    }

    fn keep_from_network(&self, records: Vec<SiteRecord>) {
        let _ = records;
    }

    /// The icon of `domain` as a small PNG, when a crawl found one.
    fn icon(&self, _domain: &str) -> Option<Vec<u8>> {
        None
    }

    /// The "Recent" block for `query`, whose best result is `top` (its
    /// domain, and whether the query names it), searched in `language`;
    /// see [`crate::news::NewsStore::recent`].
    fn recent(
        &self,
        _query: &str,
        _top: Option<(&str, bool)>,
        _language: Option<&str>,
    ) -> Option<crate::news::Recent> {
        None
    }

    /// Active and next-start feature choices, shared by desktop and Docker.
    fn features(&self) -> crate::node::features::FeatureSettings {
        Default::default()
    }
    fn saved_features(&self) -> Result<crate::node::features::FeatureSettings> {
        Ok(self.features())
    }
    fn change_features(&self, _features: crate::node::features::FeatureSettings) -> Result<()> {
        anyhow::bail!("this node has no feature settings")
    }

    /// The node's settings; `None` when it has none.
    fn settings(&self) -> Option<NodeSettings> {
        None
    }

    /// Saves new settings and puts them in force.
    fn change_settings(&self, _settings: NodeSettings) -> Result<()> {
        anyhow::bail!("this node has no settings")
    }

    /// Starts a refresh now.
    fn refresh_now(&self) {}

    /// Restarts the node, to apply saved feature changes; only a node whose
    /// [`Status::can_restart`] says so can.
    fn restart(&self) -> Result<()> {
        anyhow::bail!("This node cannot restart itself. Restart it where it runs.")
    }

    /// What the node did lately, newest first.
    fn activity_log(&self) -> Vec<crate::node::LogEntry> {
        Vec::new()
    }

    /// Tries failed work again now.
    fn retry(&self, _what: crate::node::Retry) -> Result<()> {
        anyhow::bail!("This node cannot retry work from the panel.")
    }

    /// Saves a backup in the data folder's `backups/`.
    fn make_backup(&self) -> Result<crate::node::backup::BackupInfo> {
        anyhow::bail!("This node has no data folder to back up.")
    }

    /// Restores `backup` into the data folder, then restarts if it can.
    fn restore_backup(&self, _backup: &crate::node::backup::Backup) -> Result<()> {
        anyhow::bail!("This node has no data folder to restore into.")
    }

    /// Tries the network's bootstrap nodes again now.
    fn reconnect_network(&self) -> Result<()> {
        match self.network() {
            Some(net) => net.reconnect(),
            None => anyhow::bail!("The Plumb network is off on this node."),
        }
    }

    /// Where the node keeps its data, to show on the panel. Its remote
    /// control file is there too: without a data folder, the node cannot be
    /// controlled remotely.
    fn data_dir(&self) -> Option<std::path::PathBuf> {
        None
    }

    /// The address the node was told to listen on.
    fn bind(&self) -> Option<std::net::SocketAddr> {
        None
    }

    /// Where the node serves HTTPS for remote control, if it does.
    fn https_bind(&self) -> Option<std::net::SocketAddr> {
        None
    }

    /// Whether the panel may list and control other nodes.
    fn manages_other_nodes(&self) -> bool {
        false
    }

    /// Where the node keeps each browser's search history; `None` when it
    /// keeps none.
    fn search_history(&self) -> Option<crate::history::HistoryStore> {
        None
    }
}
