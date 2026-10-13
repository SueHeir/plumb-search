//! The page index's tests: what a search finds and how pages are listed
//! with sites.

mod articles;
mod docs;
mod finding;
mod media;
mod named_site;
mod papers_and_entities;
mod placing;
mod repos;
mod subpages_and_packages;

use anyhow::bail;
use plumb_core::article::Article;
use plumb_core::paper_query::PaperQuery;
use plumb_core::{Operators, SafeSearch};

use super::docs::naming_docs;
use super::placing::subpage_asked;
use super::search::{hinted_name, last_word_in_other_number, year_moved, Hint};
use super::sites::{base_title, describes_an_organization, site_is_titled};
use super::*;

fn page(title: &str, views: u64, aliases: &[&str]) -> Page {
    Page::from_article(
        "en",
        Article {
            title: title.into(),
            views,
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
            ..Article::default()
        },
    )
}

fn searcher(pages: &[Page]) -> (tempfile::TempDir, PageSearcher) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pages");
    build_page_index(&path, pages.to_vec()).unwrap();
    let searcher = PageSearcher::open(&path).unwrap();
    (dir, searcher)
}

fn dated_paper(title: &str, date: Option<&str>, year: Option<i32>, count: u64) -> Page {
    Page::from_paper(Article {
        title: title.into(),
        item: Some(format!("10.1234/{}", title.replace(' ', "-"))),
        views: count,
        paper: Some(PaperMetadata {
            publication_date: date.map(str::to_string),
            publication_year: year,
            authors: vec!["Example Author".to_string()],
            count_kind: plumb_core::papers::PaperCountKind::Citations,
            count,
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn titles(hits: &[PageHit]) -> Vec<&str> {
    hits.iter().map(|h| h.page.title.as_str()).collect()
}

fn package(registry: &str, name: &str, views: u64) -> Page {
    let registry = plumb_core::packages::registry(registry).unwrap();
    Page::from_package(Article {
        title: name.into(),
        item: Some(plumb_core::packages::package_item(registry.key, name)),
        views,
        aliases: registry
            .short_name(name)
            .map(str::to_string)
            .into_iter()
            .collect(),
        package: Some(PackageInfo {
            registry: registry.key.into(),
            name: name.into(),
            version: Some("1.0.0".into()),
            ..PackageInfo::default()
        }),
        ..Article::default()
    })
    .unwrap()
}

fn site(domain: &str, named: bool) -> crate::Hit {
    known_site(domain, named, 1.0)
}

fn known_site(domain: &str, named: bool, link_score: f32) -> crate::Hit {
    crate::Hit {
        domain: domain.into(),
        url: format!("https://{domain}/"),
        title: None,
        description: None,
        score: 1.0,
        text_score: 1.0,
        link_score,
        country: None,
        named,
        official: false,
        key_pages: Vec::new(),
        demand: None,
        missing_words: false,
        query_evidence: None,
        placing_text_score: None,
    }
}

fn found(title: &str, site: Option<&str>, named: bool, score: f32) -> PageHit {
    let mut page = page(title, 1, &[]);
    page.site = site.map(str::to_string);
    PageHit {
        page,
        score,
        named,
        popularity: score,
        whole: false,
        learned: None,
    }
}

fn film(title: &str, description: &str, item: &str, sitelinks: u64) -> Page {
    Page::from_film(Article {
        title: title.into(),
        description: Some(description.into()),
        item: Some(item.into()),
        views: sitelinks,
        ..Article::default()
    })
    .unwrap()
}
