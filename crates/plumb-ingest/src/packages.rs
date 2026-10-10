//! The packages page set: the most used packages of npm, PyPI, crates.io
//! and the other registries ([`plumb_core::packages::REGISTRIES`]), each
//! with a card a coding agent can use without opening a page: the latest
//! version, its license, and where its docs and code are.
//!
//! The packages come from ecosyste.ms's open API, which lists every
//! registry's packages the same way, most downloaded first (most depended
//! on for registries that count no downloads). Its data is CC BY-SA 4.0.
//! ecosyste.ms allows 5,000 requests an hour; a registry's top 20,000 take
//! 80.
//!
//! A registry's downloads are counted over different times and in very
//! different numbers (npm's top package has billions a month, a Go module
//! none), so each package's views are its downloads as a share of its
//! registry's most, times [`VIEWS_SCALE`]: the top package of every
//! registry is equally popular, and "serde crate" and "react npm" weigh
//! their packages alike.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use plumb_core::article::{Article, MAX_ARTICLE_DESCRIPTION_CHARS};
use plumb_core::packages::{package_item, PackageInfo, Registry, REGISTRIES};
use serde::Deserialize;
use tracing::{info, warn};

/// The packages of a registry, `{}` for its name on ecosyste.ms.
const LIST_URL: &str = "https://packages.ecosyste.ms/api/v1/registries/{}/packages";
/// Packages asked for a page.
pub const PER_PAGE: usize = 250;
/// Packages kept of each registry, unless asked otherwise.
pub const DEFAULT_MAX_PER_REGISTRY: usize = 20_000;
/// Views of each registry's most used package.
pub const VIEWS_SCALE: f64 = 1e9;
/// Times a page is asked for before the registry is left as it is.
const RETRIES: u32 = 6;

/// One package as ecosyste.ms lists it, the fields used.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Listed {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub repository_url: Option<String>,
    #[serde(default)]
    pub documentation_url: Option<String>,
    #[serde(default)]
    pub normalized_licenses: Vec<String>,
    #[serde(default)]
    pub licenses: Option<String>,
    #[serde(default)]
    pub latest_release_number: Option<String>,
    #[serde(default)]
    pub latest_release_published_at: Option<String>,
    #[serde(default)]
    pub downloads: Option<u64>,
    #[serde(default)]
    pub dependent_repos_count: Option<u64>,
    #[serde(default)]
    pub status: Option<String>,
}

/// What a registry's packages are listed by: `downloads`, or for those that
/// count none, `dependent_repos_count`.
pub fn sort_key(registry: &Registry) -> &'static str {
    match registry.key {
        "go" | "maven" => "dependent_repos_count",
        _ => "downloads",
    }
}

impl Listed {
    /// How used the package is, by `sort` (see [`sort_key`]).
    fn used(&self, sort: &str) -> u64 {
        if sort == "downloads" {
            self.downloads.unwrap_or(0)
        } else {
            self.dependent_repos_count.unwrap_or(0)
        }
    }

    /// The package as an article (see [`plumb_core::packages`]), with
    /// views `views`; `None` for a package removed from its registry or
    /// named strangely.
    pub fn to_article(&self, registry: &Registry, views: u64) -> Option<Article> {
        let name = self.name.trim();
        if !registry.accepts(name) || self.status.as_deref() == Some("removed") {
            return None;
        }
        let description = self
            .description
            .as_deref()
            .map(|d| {
                plumb_core::truncate_chars(
                    &plumb_core::collapse_whitespace(d),
                    MAX_ARTICLE_DESCRIPTION_CHARS,
                )
            })
            .filter(|d| !d.is_empty());
        let link = |url: &Option<String>| {
            url.as_deref()
                .map(str::trim)
                .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
                .map(str::to_string)
        };
        let repo = link(&self.repository_url);
        let docs = link(&self.documentation_url);
        // The homepage only when it says more than the docs or the code.
        let homepage = link(&self.homepage).filter(|home| {
            let same = |other: &Option<String>| {
                other
                    .as_deref()
                    .is_some_and(|o| o.trim_end_matches('/') == home.trim_end_matches('/'))
            };
            !same(&repo) && !same(&docs)
        });
        let license = if self.normalized_licenses.is_empty() {
            self.licenses
                .as_deref()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
        } else {
            Some(self.normalized_licenses.join(" OR "))
        };
        let released = self
            .latest_release_published_at
            .as_deref()
            .and_then(|at| at.get(..10))
            .map(str::to_string);
        let mut aliases = Vec::new();
        if let Some(short) = registry.short_name(name) {
            aliases.push(short.to_string());
        }
        Some(Article {
            title: name.to_string(),
            description,
            item: Some(package_item(registry.key, name)),
            site: None,
            views,
            aliases,
            profiles: Vec::new(),
            website: None,
            package: Some(PackageInfo {
                registry: registry.key.to_string(),
                name: name.to_string(),
                version: self
                    .latest_release_number
                    .as_deref()
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string),
                released,
                license,
                docs,
                repo,
                homepage,
            }),
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            search: None,
            language: None,
            paper: None,
        })
    }
}

/// The articles of one registry's `listed` packages, most used first: the
/// views of each are its use as a share of the most used's, times
/// [`VIEWS_SCALE`] (at least 1).
pub fn registry_articles(registry: &Registry, listed: &[Listed]) -> Vec<Article> {
    let sort = sort_key(registry);
    let most = listed
        .iter()
        .map(|l| l.used(sort))
        .max()
        .unwrap_or(0)
        .max(1) as f64;
    let mut seen = std::collections::HashSet::new();
    listed
        .iter()
        .filter(|l| seen.insert(l.name.trim().to_lowercase()))
        .filter_map(|l| {
            let views = ((l.used(sort) as f64 / most) * VIEWS_SCALE)
                .round()
                .max(1.0) as u64;
            l.to_article(registry, views)
        })
        .collect()
}

/// Fetches up to `max` of `registry`'s most used packages.
pub async fn fetch_registry(
    client: &reqwest::Client,
    registry: &Registry,
    max: usize,
) -> Result<Vec<Listed>> {
    let base = LIST_URL.replace("{}", registry.ecosystems);
    let sort = sort_key(registry);
    let mut listed: Vec<Listed> = Vec::new();
    let mut page = 1usize;
    let mut failures = 0u32;
    while listed.len() < max {
        let url = reqwest::Url::parse_with_params(
            &base,
            &[
                ("sort", sort),
                ("order", "desc"),
                ("per_page", &PER_PAGE.to_string()),
                ("page", &page.to_string()),
            ],
        )?;
        let answer = match client.get(url).send().await {
            Ok(response) => {
                let status = response.status();
                if status.as_u16() == 429 || status.is_server_error() {
                    Err(anyhow::anyhow!("ecosyste.ms answered {status}"))
                } else if !status.is_success() {
                    bail!(
                        "ecosyste.ms answered {status} for {} page {page}",
                        registry.name
                    );
                } else {
                    response
                        .bytes()
                        .await
                        .context("reading ecosyste.ms's answer")
                        .and_then(|bytes| {
                            serde_json::from_slice::<Vec<Listed>>(&bytes)
                                .context("reading ecosyste.ms's answer")
                        })
                }
            }
            Err(err) => Err(err.into()),
        };
        let packages = match answer {
            Ok(packages) => packages,
            Err(err) => {
                failures += 1;
                if failures > RETRIES {
                    warn!(
                        "{}: giving up at page {page} with {} packages: {err:#}",
                        registry.name,
                        listed.len()
                    );
                    break;
                }
                let wait = 15 * u64::from(failures);
                warn!(
                    "{} page {page}: {err:#}; trying again in {wait}s",
                    registry.name
                );
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
        };
        failures = 0;
        let count = packages.len();
        listed.extend(packages);
        if page.is_multiple_of(20) {
            info!("{}: {} packages so far", registry.name, listed.len());
        }
        if count < PER_PAGE {
            break;
        }
        page += 1;
        // Well under ecosyste.ms's 5,000 an hour.
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    listed.truncate(max);
    Ok(listed)
}

/// Fetches the packages of the registries `keys` (all when empty), up to
/// `max_per_registry` of each, as articles, most viewed first.
pub async fn fetch_packages(
    client: &reqwest::Client,
    keys: &[String],
    max_per_registry: usize,
) -> Result<Vec<Article>> {
    let registries: Vec<&Registry> = if keys.is_empty() {
        REGISTRIES.iter().collect()
    } else {
        keys.iter()
            .map(|key| {
                plumb_core::packages::registry(key.trim()).with_context(|| {
                    format!(
                        "unknown registry {key:?}; there are: {}",
                        REGISTRIES
                            .iter()
                            .map(|r| r.key)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
            })
            .collect::<Result<_>>()?
    };
    let mut articles = Vec::new();
    for registry in registries {
        info!(
            "listing {}'s {} most used packages",
            registry.name, max_per_registry
        );
        let listed = fetch_registry(client, registry, max_per_registry).await?;
        let mut kept = registry_articles(registry, &listed);
        info!("{}: {} packages", registry.name, kept.len());
        articles.append(&mut kept);
    }
    articles.sort_by(|a, b| b.views.cmp(&a.views).then_with(|| a.item.cmp(&b.item)));
    Ok(articles)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(name: &str, downloads: u64) -> Listed {
        Listed {
            name: name.into(),
            description: Some("Fast,\n unopinionated web framework".into()),
            homepage: Some("https://expressjs.com/".into()),
            repository_url: Some("https://github.com/expressjs/express".into()),
            documentation_url: None,
            normalized_licenses: vec!["MIT".into()],
            licenses: Some("MIT".into()),
            latest_release_number: Some("5.1.0".into()),
            latest_release_published_at: Some("2026-03-31T14:02:11.000Z".into()),
            downloads: Some(downloads),
            dependent_repos_count: None,
            status: None,
        }
    }

    #[test]
    fn packages_become_articles() {
        let npm = plumb_core::packages::registry("npm").unwrap();
        let articles = registry_articles(
            npm,
            &[
                listed("react", 400_000_000),
                listed("express", 200_000_000),
                listed("Express", 1),
                listed("../x", 5),
            ],
        );
        assert_eq!(articles.len(), 2);
        assert_eq!(articles[0].views, 1_000_000_000);
        let express = &articles[1];
        assert_eq!(express.title, "express");
        assert_eq!(express.views, 500_000_000);
        assert_eq!(express.item.as_deref(), Some("npm:express"));
        assert_eq!(
            express.description.as_deref(),
            Some("Fast, unopinionated web framework")
        );
        let package = express.package.as_ref().unwrap();
        assert_eq!(package.version.as_deref(), Some("5.1.0"));
        assert_eq!(package.released.as_deref(), Some("2026-03-31"));
        assert_eq!(package.license.as_deref(), Some("MIT"));
        assert_eq!(package.homepage.as_deref(), Some("https://expressjs.com/"));
        // A homepage that is only the code is left out.
        let mut same = listed("x", 1);
        same.homepage = Some("https://github.com/expressjs/express/".into());
        let article = same.to_article(npm, 1).unwrap();
        assert_eq!(article.package.unwrap().homepage, None);
    }

    #[test]
    fn written_packages_read_back() {
        let go = plumb_core::packages::registry("go").unwrap();
        let mut gin = listed("github.com/gin-gonic/gin", 0);
        gin.dependent_repos_count = Some(90_000);
        let articles = registry_articles(go, &[gin]);
        assert_eq!(articles[0].aliases, ["gin"]);
        let mut file = Vec::new();
        plumb_core::article::write_article(&mut file, &articles[0]).unwrap();
        let text = String::from_utf8(file).unwrap();
        let read: Vec<_> = plumb_core::article::articles_of(text.lines().map(str::to_string))
            .map(|(_, a)| a.unwrap())
            .collect();
        assert_eq!(read, articles);
    }
}
