//! Bounded retrieval comparison from boilerplate_probe JSONL, without embeddings.
use anyhow::Result;
use plumb_core::SiteRecord;
use plumb_index::{build_index, RankConfig, SearchOptions, Searcher};
use std::io::{self, Read};
fn main() -> Result<()> {
    let path = std::env::args().nth(1).expect("query JSON path");
    let queries: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let records: Vec<SiteRecord> = input
        .lines()
        .map(|line| {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            let id = row["id"].as_str().unwrap();
            let url = row["url"].as_str().unwrap();
            let domain = queries.iter().find(|q| q["id"] == id).unwrap()["domain"]
                .as_str()
                .unwrap();
            let mut meta = row["meta"].clone();
            meta["domain"] = domain.into();
            meta["url"] = url.into();
            let mut record: SiteRecord = serde_json::from_value(meta).unwrap();
            if let Some(name) = row["meta"]["site_name"].as_str() {
                record.add_alias(name);
            }
            if let Some(names) = row["meta"]["structured_names"].as_array() {
                for name in names {
                    if let Some(name) = name.as_str() {
                        record.add_alias(name);
                    }
                }
            }
            record
        })
        .collect();
    let dir = tempfile::tempdir()?;
    build_index(dir.path(), &records)?;
    let searcher = Searcher::open(dir.path())?;
    for (mode, terms_boost) in [
        ("default", RankConfig::default().terms_boost),
        ("without-terms", 0.0),
    ] {
        let cfg = RankConfig {
            terms_boost,
            ..RankConfig::default()
        };
        for row in &queries {
            for query in row["queries"].as_array().unwrap() {
                let q = query.as_str().unwrap();
                let hits = searcher
                    .search_full(
                        q,
                        10,
                        &cfg,
                        &SearchOptions {
                            exact: true,
                            ..Default::default()
                        },
                    )?
                    .hits;
                let rank = hits
                    .iter()
                    .position(|h| h.domain == row["domain"].as_str().unwrap())
                    .map(|i| i + 1);
                println!(
                    "{}",
                    serde_json::json!({"mode":mode,"id":row["id"],"query":q,"expected":row["domain"],"rank":rank,"hits":hits.iter().map(|h| &h.domain).collect::<Vec<_>>()})
                );
            }
        }
    }
    Ok(())
}
