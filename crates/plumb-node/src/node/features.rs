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
    pub bootstrap: Vec<String>,
}

impl FeatureSettings {
    pub fn from_config(config: &NodeConfig) -> Self {
        Self {
            network: config.network.is_some(),
            search_by_meaning: config.search_by_meaning,
            private_search: config.private_search,
            share_popularity: config.share_popularity,
            bootstrap: config
                .network
                .as_ref()
                .map(|n| n.bootstrap.iter().map(ToString::to_string).collect())
                .unwrap_or_default(),
        }
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
        config.search_by_meaning = self.search_by_meaning;
        config.private_search = self.private_search;
        config.share_popularity = self.share_popularity;
        if self.network {
            let net = config
                .network
                .get_or_insert_with(|| plumb_net::NetConfig::new(config.data_dir.join("net")));
            net.bootstrap = self
                .bootstrap
                .iter()
                .map(|s| s.parse())
                .collect::<Result<_, _>>()?;
        } else {
            config.network = None;
        }
        Ok(())
    }
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
            bootstrap: vec!["/ip4/127.0.0.1/tcp/4002".into()],
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
}
