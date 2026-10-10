//! Preview the real web UI with synthetic search results and a disposable
//! data directory. No crawler or network node starts, and installed app
//! data is never opened. Run `cargo run -p plumb-node --example ui_preview`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use plumb_index::Hit;
use plumb_node::node::{features::FeatureSettings, NodeSettings, Phase, Status, Step};
use plumb_node::web::{node_router, SearchBackend, StatusSource};

struct PreviewSearch;

impl SearchBackend for PreviewSearch {
    fn search(&self, _query: &str, limit: usize) -> Result<Vec<Hit>> {
        let sites = [
            ("rust-lang.org", "Rust Programming Language", "A language empowering everyone to build reliable and efficient software."),
            ("doc.rust-lang.org", "Learn Rust", "The Rust book, standard library and language reference. Start with a guided introduction or look up an API."),
            ("github.com", "GitHub", "Explore open source projects and the people building them."),
            ("stackoverflow.com", "Stack Overflow", "Practical answers to programming questions, written by the community."),
        ];
        Ok(sites
            .into_iter()
            .take(limit)
            .enumerate()
            .map(|(i, (domain, title, description))| Hit {
                domain: domain.into(),
                url: format!("https://{domain}/"),
                title: Some(title.into()),
                description: Some(description.into()),
                score: 1.0 - i as f32 * 0.1,
                text_score: 1.0,
                link_score: 0.8,
                placing_text_score: None,
                country: None,
                named: i == 0,
                official: i == 0,
                key_pages: Vec::new(),
                demand: None,
                missing_words: false,
                query_evidence: None,
            })
            .collect())
    }

    fn num_docs(&self) -> u64 {
        4
    }
}

struct PreviewNode {
    dir: tempfile::TempDir,
    settings: Mutex<NodeSettings>,
    features: Mutex<FeatureSettings>,
}

impl PreviewNode {
    fn save_state(&self) -> Result<()> {
        std::fs::write(
            self.dir.path().join("settings.json"),
            serde_json::to_vec(&*self.settings.lock().unwrap())?,
        )?;
        std::fs::write(
            self.dir.path().join("features.json"),
            serde_json::to_vec(&*self.features.lock().unwrap())?,
        )?;
        Ok(())
    }
}

impl StatusSource for PreviewNode {
    fn status(&self) -> Status {
        let settings = self.settings.lock().unwrap();
        Status {
            phase: Phase::Ready,
            step: Step::Idle,
            detail: "UI preview — synthetic data, no background work".into(),
            progress: None,
            last_error: None,
            wikidata_missing: false,
            wikidata_error: None,
            sites: 4,
            index: Some("preview".into()),
            last_refresh: None,
            next_refresh: None,
            version: "UI preview".into(),
            network: None,
            fill: None,
            crawl_left: 0,
            page_coverage: None,
            background_updates: settings.background_updates,
            paused: None,
            paused_until: None,
            disk_used: 1_400_000_000,
            storage_limit: settings.storage_limit_mb * 1_000_000,
            downloaded_today: 18_000_000,
            downloaded_total: 1_400_000_000,
            homepages_visited: 0,
            meaning_sites: None,
            meaning_work: None,
            can_restart: false,
        }
    }

    fn settings(&self) -> Option<NodeSettings> {
        Some(self.settings.lock().unwrap().clone())
    }

    fn change_settings(&self, settings: NodeSettings) -> Result<()> {
        *self.settings.lock().unwrap() = settings;
        Ok(())
    }

    fn features(&self) -> FeatureSettings {
        self.features.lock().unwrap().clone()
    }

    fn change_features(&self, features: FeatureSettings) -> Result<()> {
        *self.features.lock().unwrap() = features;
        Ok(())
    }

    fn make_backup(&self) -> Result<plumb_node::node::backup::BackupInfo> {
        self.save_state()?;
        plumb_node::node::backup::save(self.dir.path(), None)
    }

    fn restore_backup(&self, backup: &plumb_node::node::backup::Backup) -> Result<()> {
        // Only disposable preview settings are present here; no real keys.
        self.save_state()?;
        plumb_node::node::backup::save(self.dir.path(), Some("before-restore"))?;
        backup.restore(self.dir.path())?;
        *self.settings.lock().unwrap() =
            serde_json::from_slice(&std::fs::read(self.dir.path().join("settings.json"))?)?;
        *self.features.lock().unwrap() =
            serde_json::from_slice(&std::fs::read(self.dir.path().join("features.json"))?)?;
        Ok(())
    }

    fn data_dir(&self) -> Option<std::path::PathBuf> {
        Some(self.dir.path().to_owned())
    }

    fn bind(&self) -> Option<SocketAddr> {
        Some("127.0.0.1:54101".parse().unwrap())
    }

    fn manages_other_nodes(&self) -> bool {
        true
    }

    fn search_history(&self) -> Option<plumb_node::history::HistoryStore> {
        Some(plumb_node::history::HistoryStore::new(
            self.dir.path().join("history"),
        ))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let node = PreviewNode {
        dir: tempfile::tempdir()?,
        settings: Mutex::new(NodeSettings {
            setup_chosen: true,
            ..NodeSettings::desktop()
        }),
        features: Mutex::new(FeatureSettings {
            search_history: Some(true),
            ..Default::default()
        }),
    };
    let router = node_router(Arc::new(PreviewSearch), Arc::new(node));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:54101").await?;
    println!("UI preview: http://127.0.0.1:54101/ (temporary, synthetic data)");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
