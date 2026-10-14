//! Building the page index from page sets.

use std::path::Path;

use anyhow::{Context, Result};
use plumb_core::host_of;
use plumb_core::paper_query::{day_number, publication_year};
use tantivy::directory::Directory;
use tantivy::schema::{
    IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, INDEXED, STORED, STRING,
};
use tantivy::TantivyDocument;

use super::docs::{docs_overview, docs_symbols};
use super::sites::{base_title, short_titles};
use super::{Fields, Page, PageIndexStats, DOCS_SET, PAPERS_SET, POPULARITY_SCALE, WIKTIONARY_SET};
use crate::analysis::{self, JOINED_ANALYZER, STEMMED_ANALYZER, WORDS_ANALYZER};
use crate::replace::Staging;

/// The page index schema.
pub(crate) fn schema() -> (Schema, Fields) {
    let mut builder = Schema::builder();
    let words = builder.add_text_field(
        "words",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(WORDS_ANALYZER)
                .set_index_option(IndexRecordOption::WithFreqs),
        ),
    );
    let keys = builder.add_text_field(
        "keys",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(JOINED_ANALYZER)
                .set_index_option(IndexRecordOption::Basic),
        ),
    );
    let scope = builder.add_text_field("scope", STRING);
    let topic = builder.add_text_field(
        "topic",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(STEMMED_ANALYZER)
                .set_index_option(IndexRecordOption::Basic),
        ),
    );
    let about = builder.add_text_field(
        "about",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(STEMMED_ANALYZER)
                .set_index_option(IndexRecordOption::Basic),
        ),
    );
    let lead = builder.add_text_field(
        "lead",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(STEMMED_ANALYZER)
                .set_index_option(IndexRecordOption::WithFreqs),
        ),
    );
    let word = builder.add_text_field(
        "word",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(JOINED_ANALYZER)
                .set_index_option(IndexRecordOption::Basic),
        ),
    );
    let popularity = builder.add_u64_field("popularity", FAST | STORED);
    let site = builder.add_text_field("site", STRING);
    let language = builder.add_text_field("language", STRING);
    let paper_year = builder.add_u64_field("paper_year", INDEXED | FAST);
    let paper_day = builder.add_u64_field("paper_day", INDEXED | FAST);
    let page = builder.add_text_field("page", STORED);
    (
        builder.build(),
        Fields {
            words,
            keys,
            scope,
            topic,
            about,
            lead,
            word,
            popularity,
            site,
            language,
            paper_year,
            paper_day,
            page,
        },
    )
}

/// Builds the page index of `pages` in `dir`, replacing any there.
pub fn build_page_index(
    dir: &Path,
    pages: impl IntoIterator<Item = Page>,
) -> Result<PageIndexStats> {
    build_page_index_with_budget(dir, pages, None, &mut || Ok(()))
}

/// How many pages [`build_page_index_with_budget`] adds between asking
/// whether to go on.
pub const GO_ON_EVERY: u64 = 10_000;

/// [`build_page_index`], counting against `budget`. `go_on` is asked every
/// [`GO_ON_EVERY`] pages, and its error ends the build with `dir` as it
/// was, so a node that stops doesn't wait for the rest.
pub fn build_page_index_with_budget(
    dir: &Path,
    pages: impl IntoIterator<Item = Page>,
    budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
    go_on: &mut dyn FnMut() -> Result<()>,
) -> Result<PageIndexStats> {
    let staging = Staging::new_with_budget(dir, budget.clone())?;
    let (schema, fields) = schema();
    let index = crate::storage::create_index(staging.path(), schema, budget, staging.lifecycle())
        .with_context(|| format!("creating the page index in {}", dir.display()))?;
    analysis::register(index.tokenizers());
    let mut writer = index
        .writer_with_num_threads(1, 64 << 20)
        .context("opening the page index for writing")?;
    let mut stats = PageIndexStats::default();
    // Sets come most read first, so the first page of a set is its most
    // read.
    let mut most_by_set: std::collections::HashMap<String, u64> = Default::default();
    for page in pages {
        if stats.pages > 0 && stats.pages % GO_ON_EVERY == 0 {
            go_on()?;
        }
        let most = most_by_set
            .entry(page.set.clone())
            .and_modify(|most| *most = (*most).max(page.views))
            .or_insert(page.views);
        let popularity = if *most == 0 {
            0.0
        } else {
            ((page.views as f32).ln_1p() / (*most as f32).ln_1p()).min(1.0)
        };
        let mut document = TantivyDocument::default();
        if let Some(language) = page.language() {
            document.add_text(fields.language, language);
            *stats.languages.entry(language.to_string()).or_default() += 1;
        } else {
            document.add_text(fields.language, "unknown");
        }
        document.add_text(fields.scope, format!("set:{}", page.set));
        if page.set == PAPERS_SET {
            stats.papers.total += 1;
            if let Some(paper) = &page.paper {
                if let Some(year) = publication_year(paper) {
                    document.add_u64(fields.paper_year, year);
                    stats.papers.publication_year += 1;
                }
                if let Some(date) = paper
                    .publication_date
                    .as_deref()
                    .filter(|d| day_number(d).is_some())
                {
                    document.add_u64(fields.paper_day, day_number(date).unwrap());
                    stats.papers.publication_day += 1;
                    if stats
                        .papers
                        .earliest_day
                        .as_deref()
                        .is_none_or(|old| date < old)
                    {
                        stats.papers.earliest_day = Some(date.to_string());
                    }
                    if stats
                        .papers
                        .latest_day
                        .as_deref()
                        .is_none_or(|old| date > old)
                    {
                        stats.papers.latest_day = Some(date.to_string());
                    }
                }
            }
        }
        if let Some(host) = host_of(&page.url) {
            document.add_text(fields.scope, format!("host:{host}"));
            let mut suffix = host.as_str();
            loop {
                document.add_text(fields.scope, format!("under:{suffix}"));
                let Some((_, rest)) = suffix.split_once('.') else {
                    break;
                };
                suffix = rest;
            }
        }
        if page.set == DOCS_SET {
            for symbol in docs_symbols(&page) {
                document.add_text(fields.scope, format!("symbol:{symbol}"));
            }
            if let Some(site) = plumb_core::docs::site_of_url(&page.url) {
                document.add_text(fields.scope, format!("docs:{}", site.key));
                if docs_overview(&page, site) {
                    document.add_text(fields.scope, format!("overview:{}", site.key));
                }
            }
        }
        document.add_u64(
            fields.popularity,
            (popularity * POPULARITY_SCALE).round() as u64,
        );
        if page.set == WIKTIONARY_SET {
            document.add_text(fields.word, &page.title);
            document.add_text(fields.page, serde_json::to_string(&page)?);
            writer.add_document(document)?;
            stats.pages += 1;
            continue;
        }
        document.add_text(fields.words, &page.title);
        for alias in &page.aliases {
            document.add_text(fields.words, alias);
        }
        for name in &page.names {
            document.add_text(fields.words, name);
            document.add_text(fields.keys, name);
        }
        if let Some(topic) = page.topic() {
            document.add_pre_tokenized_text(
                fields.topic,
                analysis::content_tokens(page.language(), &topic),
            );
        }
        if let Some(about) = page.about() {
            document.add_pre_tokenized_text(
                fields.about,
                analysis::content_tokens(page.language(), &about),
            );
        }
        if let Some(lead) = page.lead.as_deref().filter(|_| page.is_article()) {
            document.add_pre_tokenized_text(
                fields.lead,
                analysis::content_tokens(page.language(), lead),
            );
        }
        // A docs page's title alone ("Introduction") names nothing: it is
        // named by its product's name and title ("python sorting
        // techniques").
        if page.set != DOCS_SET {
            document.add_text(fields.keys, &page.title);
            let base = base_title(&page.title);
            if base != page.title {
                document.add_text(fields.keys, base);
            }
            // "no way home" asks for "Spider-Man: No Way Home", "dark side
            // of the moon" for "The Dark Side of the Moon".
            if page.is_article() {
                for short in short_titles(&page.title) {
                    document.add_text(fields.keys, short);
                }
            }
        }
        // A package is asked for by its short name too: "gin golang".
        if page.package.is_some() || page.set == DOCS_SET {
            for alias in &page.aliases {
                document.add_text(fields.keys, alias);
            }
        }
        if let Some(site) = page.site.as_deref().filter(|_| page.is_article()) {
            document.add_text(fields.site, site);
        }
        document.add_text(fields.page, serde_json::to_string(&page)?);
        writer.add_document(document)?;
        stats.pages += 1;
        stats.most_views = stats.most_views.max(page.views);
    }
    writer.commit().context("writing the page index")?;
    // A merge the commit started must end before the index is put in
    // place: one cut short leaves its segment files behind for good.
    writer
        .wait_merging_threads()
        .context("finishing the page index merges")?;
    if staging.budget().is_some() {
        index
            .directory()
            .atomic_write(Path::new("pages.json"), &serde_json::to_vec(&stats)?)?;
    } else {
        std::fs::write(
            staging.path().join("pages.json"),
            serde_json::to_vec(&stats)?,
        )?;
    }
    staging.install()?;
    Ok(stats)
}
