//! Shared desktop/server feature choices. Saved changes apply on restart.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use super::{store, NodeConfig};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FeatureSettings {
    pub network: bool,
    pub search_by_meaning: bool,
    pub private_search: bool,
    pub share_popularity: bool,
    /// Keep a search history for each browser; `None` (features saved
    /// before it existed) keeps the node's default, on for the desktop.
    pub search_history: Option<bool>,
    pub bootstrap: Vec<String>,
    /// Turns off trusting [`plumb_net::node::DEFAULT_TRUSTED_PEERS`].
    pub no_default_trust: bool,
    /// Node ids whose crawls are taken in at once, besides the default ones.
    pub trusted: Vec<String>,
    /// Which nodes network searches ask; `None` keeps the node's own
    /// (`--search-from`, friends of friends unless given).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_from: Option<plumb_net::SearchScope>,
    /// How many network searches the node answers for free a day for other
    /// nodes; `None` keeps the node's own (`--answer-per-day`, no limit
    /// unless given).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer_limit: Option<AnswerLimit>,
    /// Spend credits on tokens to be answered by busy nodes; `None` keeps
    /// the node's own (on unless `--no-spend-credits`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spend_credits: Option<bool>,
}

/// A daily limit on the network searches answered for free for other nodes
/// (see [`plumb_net::allowance`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerLimit {
    /// `None` for no limit.
    pub per_day: Option<u64>,
}

impl FeatureSettings {
    pub fn from_config(config: &NodeConfig) -> Self {
        Self {
            network: config.network.is_some(),
            search_by_meaning: config.search_by_meaning,
            private_search: config.private_search,
            share_popularity: config.share_popularity,
            search_history: Some(config.search_history),
            bootstrap: config
                .network
                .as_ref()
                .map(|n| n.bootstrap.iter().map(ToString::to_string).collect())
                .unwrap_or_default(),
            no_default_trust: config.network.as_ref().is_some_and(|n| {
                default_trusted()
                    .iter()
                    .any(|id| !n.trusted_peers.contains(id))
            }),
            trusted: config
                .network
                .as_ref()
                .map(|n| {
                    let defaults = default_trusted();
                    n.trusted_peers
                        .iter()
                        .filter(|id| !defaults.contains(id))
                        .map(ToString::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            search_from: Some(
                config
                    .network
                    .as_ref()
                    .map(|n| n.search_scope)
                    .unwrap_or_default(),
            ),
            // The defaults read as no choice made.
            answer_limit: config
                .network
                .as_ref()
                .and_then(|n| n.answer_per_day)
                .map(|n| AnswerLimit { per_day: Some(n) }),
            spend_credits: config
                .network
                .as_ref()
                .is_some_and(|n| !n.collect_tokens)
                .then_some(false),
        }
    }

    /// Which nodes network searches ask under these settings.
    pub fn search_scope(&self) -> plumb_net::SearchScope {
        self.search_from.unwrap_or_default()
    }

    /// Adds `peer` to the trusted nodes, unless it is trusted already.
    /// Returns whether it was added.
    pub fn trust(&mut self, peer: &str) -> bool {
        let Ok(id) = peer.parse::<plumb_net::PeerId>() else {
            return false;
        };
        let by_default = !self.no_default_trust && default_trusted().contains(&id);
        if by_default || self.trusted.iter().any(|t| t == peer) {
            return false;
        }
        self.trusted.push(peer.to_owned());
        true
    }

    /// Whether the node finds others through the Plumb network's own
    /// bootstrap nodes ([`plumb_net::DEFAULT_BOOTSTRAP`]).
    pub fn uses_default_bootstrap(&self) -> bool {
        self.bootstrap
            .iter()
            .any(|addr| plumb_net::DEFAULT_BOOTSTRAP.contains(&addr.as_str()))
    }

    /// The bootstrap nodes added by hand, besides the default ones.
    pub fn extra_bootstrap(&self) -> impl Iterator<Item = &str> {
        self.bootstrap
            .iter()
            .map(String::as_str)
            .filter(|addr| !plumb_net::DEFAULT_BOOTSTRAP.contains(addr))
    }

    /// Sets the bootstrap nodes: the default ones if `default`, then
    /// `extra`, each once.
    pub fn set_bootstrap<'a>(&mut self, default: bool, extra: impl IntoIterator<Item = &'a str>) {
        let mut bootstrap: Vec<String> = Vec::new();
        let defaults: &[&str] = if default {
            &plumb_net::DEFAULT_BOOTSTRAP
        } else {
            &[]
        };
        for addr in defaults.iter().copied().chain(extra) {
            if !bootstrap.iter().any(|a| a == addr) {
                bootstrap.push(addr.to_owned());
            }
        }
        self.bootstrap = bootstrap;
    }

    /// The nodes whose crawls are taken in at once under these settings.
    fn trusted_peers(&self) -> Result<Vec<plumb_net::PeerId>> {
        let mut peers = if self.no_default_trust {
            Vec::new()
        } else {
            default_trusted()
        };
        for id in &self.trusted {
            let id = id
                .parse()
                .with_context(|| format!("Invalid trusted node id: {id}"))?;
            if !peers.contains(&id) {
                peers.push(id);
            }
        }
        Ok(peers)
    }

    pub fn check(&self) -> Result<()> {
        if self.share_popularity && !self.network {
            bail!("Turn on the Plumb network to share popularity. Nothing was changed.");
        }
        let mut net = plumb_net::NetConfig::new(Default::default());
        for address in &self.bootstrap {
            net.bootstrap.push(
                address
                    .parse()
                    .with_context(|| format!("Invalid bootstrap address: {address}"))?,
            );
        }
        self.trusted_peers()?;
        Ok(())
    }

    /// The saved settings. In a data directory copied from another node's
    /// (see [`Saved::Elsewhere`]) the network is off.
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        Ok(Self::read(dir)?.map(|(mut settings, saved)| {
            if matches!(saved, Saved::Elsewhere(_)) {
                settings.network = false;
                settings.share_popularity = false;
            }
            settings
        }))
    }

    /// The settings as saved, and where they were saved.
    fn read(dir: &Path) -> Result<Option<(Self, Saved)>> {
        let bytes = match std::fs::read(dir.join("features.json")) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).context("reading features.json"),
        };
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).context("reading features.json")?;
        let saved = match value.get(SAVED_IN).and_then(|v| v.as_str()) {
            None => Saved::Unknown,
            Some(saved_in) if saved_in == stamp(dir) => Saved::Here,
            Some(saved_in) => Saved::Elsewhere(saved_in.to_owned()),
        };
        let settings: Self = serde_json::from_value(value).context("reading features.json")?;
        settings.check()?;
        Ok(Some((settings, saved)))
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        self.check()?;
        let mut value = serde_json::to_value(self)?;
        value[SAVED_IN] = stamp(dir).into();
        store::write_atomically(
            &dir.join("features.json"),
            &serde_json::to_vec_pretty(&value)?,
        )
    }

    /// Applies the settings saved in `config`'s data directory, if any, as
    /// a node starts. A copy of another node's data directory (a test node
    /// made from a live one) carries its network identity and its saved
    /// choice to join the network, so there the network stays off unless
    /// `--network` is given; the settings are then saved again as this
    /// directory's own.
    pub fn apply_saved(config: &mut NodeConfig) -> Result<()> {
        let dir = config.data_dir.clone();
        let Some((mut settings, saved)) = Self::read(&dir)? else {
            return Ok(());
        };
        let on_command_line = config.network.is_some();
        if let Saved::Elsewhere(other) = &saved {
            if settings.network && !on_command_line {
                tracing::warn!(
                    "The Plumb network stays off: {} was saved in {other}, so this data                      directory is a copy of another node's. Start with --network, or turn                      the network on in the panel's Optional features, to join it.",
                    dir.join("features.json").display()
                );
                settings.network = false;
                settings.share_popularity = false;
            }
        }
        if settings.network && !on_command_line {
            tracing::info!(
                "Joining the Plumb network: it was turned on in the panel's Optional                  features (saved in features.json), not by --network"
            );
        }
        settings.apply(config)?;
        if !matches!(saved, Saved::Here) {
            settings.save(&dir)?;
        }
        Ok(())
    }

    pub fn apply(&self, config: &mut NodeConfig) -> Result<()> {
        self.check()?;
        // Choices saved in the panel are the latest word, but someone who
        // started the node with a flag should hear why it does nothing.
        for (name, on_command_line, saved) in [
            ("--network", config.network.is_some(), self.network),
            (
                "--search-by-meaning",
                config.search_by_meaning,
                self.search_by_meaning,
            ),
            (
                "--share-popularity",
                config.share_popularity,
                self.share_popularity,
            ),
            (
                "--search-history",
                config.search_history,
                self.search_history.unwrap_or(true),
            ),
        ] {
            if on_command_line && !saved {
                tracing::warn!(
                    "{name} is ignored: it was turned off in the panel's Optional features, \
                     saved in features.json; turn it on there"
                );
            }
        }
        config.search_by_meaning = self.search_by_meaning;
        // No longer a panel choice: private search runs wherever there are
        // buckets. An older save that turned it on still builds them.
        config.private_search |= self.private_search;
        config.share_popularity = self.share_popularity;
        if let Some(history) = self.search_history {
            config.search_history = history;
        }
        if self.network {
            let net = config
                .network
                .get_or_insert_with(|| plumb_net::NetConfig::new(config.data_dir.join("net")));
            net.bootstrap = self
                .bootstrap
                .iter()
                .map(|s| s.parse())
                .collect::<Result<_, _>>()?;
            net.trusted_peers = self.trusted_peers()?;
            if let Some(scope) = self.search_from {
                net.search_scope = scope;
            }
            if let Some(limit) = self.answer_limit {
                net.answer_per_day = limit.per_day;
            }
            if let Some(spend) = self.spend_credits {
                net.collect_tokens = spend;
            }
        } else {
            config.network = None;
        }
        Ok(())
    }
}

/// The key of features.json that names the data directory it was saved in.
const SAVED_IN: &str = "saved_in";

/// Where features.json was saved.
#[derive(Debug, PartialEq, Eq)]
enum Saved {
    /// In the directory it is read from.
    Here,
    /// In another directory (the one given): the directory was copied, or
    /// moved.
    Elsewhere(String),
    /// Before the directory was written down.
    Unknown,
}

/// How features.json names the data directory `dir`.
fn stamp(dir: &Path) -> String {
    std::fs::canonicalize(dir)
        .unwrap_or_else(|_| dir.to_path_buf())
        .display()
        .to_string()
}

fn default_trusted() -> Vec<plumb_net::PeerId> {
    plumb_net::NetConfig::new(Default::default()).trusted_peers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_survive_restart_and_preserve_server_transport() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = NodeConfig::desktop(dir.path().into());
        let preferences = FeatureSettings {
            network: true,
            private_search: true,
            search_by_meaning: true,
            share_popularity: true,
            search_history: Some(false),
            bootstrap: vec!["/ip4/127.0.0.1/tcp/4002".into()],
            no_default_trust: true,
            trusted: vec!["12D3KooWEwYB7PYxRNgvSWiwkLXvwYajSmYn4yoPqmkN7NbNqJjg".into()],
            search_from: Some(plumb_net::SearchScope::Trusted),
            answer_limit: Some(AnswerLimit {
                per_day: Some(5_000),
            }),
            spend_credits: Some(false),
        };
        preferences.save(dir.path()).unwrap();
        FeatureSettings::load(dir.path())
            .unwrap()
            .unwrap()
            .apply(&mut config)
            .unwrap();
        assert_eq!(FeatureSettings::from_config(&config), preferences);
        config.network.as_mut().unwrap().upnp = false;
        preferences.apply(&mut config).unwrap();
        assert!(!config.network.as_ref().unwrap().upnp);
        let invalid = FeatureSettings {
            network: false,
            ..preferences.clone()
        };
        assert!(invalid.save(dir.path()).is_err());
        assert_eq!(
            FeatureSettings::load(dir.path()).unwrap(),
            Some(preferences)
        );
    }

    #[test]
    fn a_copied_data_directory_stays_off_the_network_unless_asked() {
        let live = tempfile::tempdir().unwrap();
        let on = FeatureSettings {
            network: true,
            share_popularity: true,
            ..Default::default()
        };
        on.save(live.path()).unwrap();
        let copy = |dir: &Path| {
            std::fs::copy(live.path().join("features.json"), dir.join("features.json")).unwrap();
        };

        // The live node itself joins.
        let mut config = NodeConfig::desktop(live.path().into());
        FeatureSettings::apply_saved(&mut config).unwrap();
        assert!(config.network.is_some());

        // A copy does not, and is saved as off.
        let test = tempfile::tempdir().unwrap();
        copy(test.path());
        assert!(!FeatureSettings::load(test.path()).unwrap().unwrap().network);
        let mut config = NodeConfig::desktop(test.path().into());
        FeatureSettings::apply_saved(&mut config).unwrap();
        assert!(config.network.is_none());
        assert!(!config.share_popularity);
        let saved = FeatureSettings::read(test.path()).unwrap().unwrap();
        assert_eq!(saved.1, Saved::Here);
        assert!(!saved.0.network);

        // Unless started with --network.
        let asked = tempfile::tempdir().unwrap();
        copy(asked.path());
        let mut config = NodeConfig::desktop(asked.path().into());
        config.network = Some(plumb_net::NetConfig::new(asked.path().join("net")));
        FeatureSettings::apply_saved(&mut config).unwrap();
        assert!(config.network.is_some());
        assert!(
            FeatureSettings::load(asked.path())
                .unwrap()
                .unwrap()
                .network
        );

        // Settings saved before the directory was written down are kept,
        // and from then on belong to it.
        let old = tempfile::tempdir().unwrap();
        std::fs::write(old.path().join("features.json"), br#"{"network": true}"#).unwrap();
        let mut config = NodeConfig::desktop(old.path().into());
        FeatureSettings::apply_saved(&mut config).unwrap();
        assert!(config.network.is_some());
        assert_eq!(
            FeatureSettings::read(old.path()).unwrap().unwrap().1,
            Saved::Here
        );
    }

    #[test]
    fn the_default_bootstrap_nodes_are_one_switch() {
        let mut features = FeatureSettings::default();
        features.set_bootstrap(true, ["/ip4/10.0.0.2/tcp/4001"]);
        assert!(features.uses_default_bootstrap());
        assert_eq!(features.bootstrap.len(), 3);
        assert_eq!(
            features.extra_bootstrap().collect::<Vec<_>>(),
            ["/ip4/10.0.0.2/tcp/4001"]
        );
        // Typing a default address in the box adds it once.
        features.set_bootstrap(true, [plumb_net::DEFAULT_BOOTSTRAP[0]]);
        assert_eq!(features.bootstrap.len(), 2);
        features.set_bootstrap(false, ["/ip4/10.0.0.2/tcp/4001"]);
        assert!(!features.uses_default_bootstrap());
        assert_eq!(features.bootstrap, ["/ip4/10.0.0.2/tcp/4001"]);
    }

    #[test]
    fn nodes_trust_plumbsearch_org_unless_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = NodeConfig::desktop(dir.path().into());
        let mut features = FeatureSettings {
            network: true,
            search_history: Some(false),
            search_from: Some(plumb_net::SearchScope::FriendsOfFriends),
            ..Default::default()
        };
        features.apply(&mut config).unwrap();
        let trusted = &config.network.as_ref().unwrap().trusted_peers;
        assert_eq!(trusted, &default_trusted());
        assert!(!trusted.is_empty());
        assert_eq!(FeatureSettings::from_config(&config), features);

        let extra = "12D3KooWEwYB7PYxRNgvSWiwkLXvwYajSmYn4yoPqmkN7NbNqJjg";
        features.trusted = vec![extra.into()];
        features.apply(&mut config).unwrap();
        assert_eq!(config.network.as_ref().unwrap().trusted_peers.len(), 2);
        features.no_default_trust = true;
        features.apply(&mut config).unwrap();
        let trusted = &config.network.as_ref().unwrap().trusted_peers;
        assert_eq!(trusted.len(), 1);
        assert_eq!(trusted[0].to_string(), extra);
        assert_eq!(FeatureSettings::from_config(&config), features);

        features.trusted = vec!["not-a-node".into()];
        assert!(features.check().is_err());
    }

    #[test]
    fn search_scope_is_friends_of_friends_unless_chosen() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = NodeConfig::desktop(dir.path().into());
        let mut features = FeatureSettings {
            network: true,
            search_history: Some(false),
            ..Default::default()
        };
        features.apply(&mut config).unwrap();
        let scope = |config: &NodeConfig| config.network.as_ref().unwrap().search_scope;
        assert_eq!(scope(&config), plumb_net::SearchScope::FriendsOfFriends);
        features.search_from = Some(plumb_net::SearchScope::Anyone);
        features.apply(&mut config).unwrap();
        assert_eq!(scope(&config), plumb_net::SearchScope::Anyone);
        assert_eq!(FeatureSettings::from_config(&config), features);
        // Saves from before the choice keep what the node was started with.
        let old: FeatureSettings =
            serde_json::from_str(r#"{"network":true,"search_history":false}"#).unwrap();
        old.apply(&mut config).unwrap();
        assert_eq!(scope(&config), plumb_net::SearchScope::Anyone);
    }

    #[test]
    fn trusting_a_node_adds_it_once() {
        let mut features = FeatureSettings::default();
        let id = "12D3KooWEwYB7PYxRNgvSWiwkLXvwYajSmYn4yoPqmkN7NbNqJjg";
        assert!(features.trust(id));
        assert!(!features.trust(id));
        assert!(!features.trust("not-a-node"));
        // plumbsearch.org is trusted already, unless that was turned off.
        let default = plumb_net::node::DEFAULT_TRUSTED_PEERS[0];
        assert!(!features.trust(default));
        features.no_default_trust = true;
        assert!(features.trust(default));
        assert_eq!(features.trusted, [id, default]);
    }
}
