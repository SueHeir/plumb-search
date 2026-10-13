//! Downloads the seed datasets. These hosts must be reachable from the
//! machine running `plumb-tools fetch-data` (or a `plumb run` node on first start).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

mod cc_ranks;
mod fetch;
mod wikidata;

pub use cc_ranks::{
    cc_domain_ranks_top_file_name, cc_domain_ranks_url, download_cc_domain_ranks,
    download_cc_domain_ranks_top, file_name_from_url,
};
pub use fetch::{download_to_file, part_path};
pub use wikidata::{
    bulk_official_sites_query, download_wikidata_official_sites_bulk,
    download_wikidata_official_sites_paced, download_wikidata_official_sites_with,
    wikidata_json_to_tsv, wikidata_sitelink_bands, wikidata_sparql_query, SitelinkBand,
    WikidataPacing, DEFAULT_MIN_SITELINKS, ENDPOINT_MIN_SITELINKS, WIKIDATA_BANDS_DIR_NAME,
};

/// Sent with every request, so site owners and dataset hosts can see who is fetching.
pub const USER_AGENT: &str = concat!(
    "PlumbSearch/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/SueHeir/plumb-search)"
);

/// The latest Tranco list, as a zip holding `top-1m.csv`.
pub const TRANCO_LATEST_URL: &str = "https://tranco-list.eu/top-1m.csv.zip";

/// Wikidata's public SPARQL endpoint.
pub const WIKIDATA_SPARQL_URL: &str = "https://query.wikidata.org/sparql";

/// QLever's copy of Wikidata (<https://qlever.dev>), a SPARQL endpoint
/// that lists every item with an official website in seconds and has no
/// 60-second limit. It is rebuilt from Wikidata's dumps, so it can be a
/// few days behind. The official websites and their facts are asked of it
/// first, and of [`WIKIDATA_SPARQL_URL`] when it fails.
pub const QLEVER_WIKIDATA_URL: &str = "https://qlever.dev/api/wikidata";

/// The prefixes Wikidata's query service declares by itself, declared for
/// endpoints such as [`QLEVER_WIKIDATA_URL`] that need them.
pub const WIKIDATA_PREFIXES: &str = "PREFIX wd: <http://www.wikidata.org/entity/> \
     PREFIX wdt: <http://www.wikidata.org/prop/direct/> \
     PREFIX wikibase: <http://wikiba.se/ontology#> \
     PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> \
     PREFIX skos: <http://www.w3.org/2004/02/skos/core#> \
     PREFIX schema: <http://schema.org/> \
     PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> ";

/// File name [`download_tranco`] saves to.
pub const TRANCO_FILE_NAME: &str = "tranco-top-1m.csv.zip";

/// File name [`download_wikidata_official_sites_paced`] saves to.
pub const WIKIDATA_FILE_NAME: &str = "wikidata-official-sites.tsv";

/// A client with [`USER_AGENT`], gzip off (we store files as served) and generous timeouts.
///
/// Connecting may take 30 seconds and each read 120 seconds; there is no
/// overall timeout, since dataset files can be gigabytes. Every response
/// decompression is turned off, even when another crate in the build enables
/// reqwest's `gzip` feature.
pub fn http_client() -> Result<reqwest::Client> {
    client_builder().build().context("building the HTTP client")
}

fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
}

/// Downloads the latest Tranco list to `dir/tranco-top-1m.csv.zip` and returns that path.
pub async fn download_tranco(client: &reqwest::Client, dir: &Path) -> Result<PathBuf> {
    let dest = dir.join(TRANCO_FILE_NAME);
    download_to_file(client, TRANCO_LATEST_URL, &dest).await?;
    Ok(dest)
}

#[cfg(test)]
mod tests;
