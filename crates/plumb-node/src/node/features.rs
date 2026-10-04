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
        }
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

    pub fn load(dir: &Path) -> Result<Option<Self>> {
        match std::fs::read(dir.join("features.json")) {
            Ok(bytes) => {
                let settings: Self =
                    serde_json::from_slice(&bytes).context("reading features.json")?;
                settings.check()?;
                Ok(Some(settings))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).context("reading features.json"),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        self.check()?;
        store::write_atomically(
            &dir.join("features.json"),
            &serde_json::to_vec_pretty(self)?,
        )
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
                "--private-search",
                config.private_search,
                self.private_search,
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
        config.private_search = self.private_search;
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
        } else {
            config.network = None;
        }
        Ok(())
    }
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
}
