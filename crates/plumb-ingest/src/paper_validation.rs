//! Publication gates and a bounded, resumable arXiv consistency queue.
//! The fetch/publish owner must validate the candidate before swapping it.

use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::{article::Article, normalize_text};
use serde::{Deserialize, Serialize};

use crate::paper_names::{self, ArxivPaper, Named};

/// Optional canary expectations, supplied explicitly by the caller. No
/// specific papers are required by enrichment or whole-corpus validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperCanary {
    pub id: String,
    pub title: String,
    pub first_author: String,
    pub submitted: String,
}

pub fn canary_ids(expected: &[PaperCanary]) -> Vec<String> {
    expected.iter().map(|p| p.id.clone()).collect()
}

pub fn repair_canary(
    papers: &mut Vec<Article>,
    found: &[ArxivPaper],
    expected: &[PaperCanary],
) -> Result<Named> {
    let sources = paper_names::unique_sources(found);
    for canary in expected {
        let Some(source) = sources.get(canary.id.as_str()) else {
            bail!("canary arXiv verification unresolved for {}", canary.id);
        };
        if normalize_text(&source.title) != normalize_text(&canary.title)
            || !source
                .authors
                .first()
                .is_some_and(|a| paper_names::same_author(a, &canary.first_author))
            || source.published.as_deref() != Some(canary.submitted.as_str())
        {
            bail!("canary arXiv verification conflicts for {}", canary.id);
        }
    }
    let selected: Vec<_> = found
        .iter()
        .filter(|s| expected.iter().any(|e| e.id == s.id))
        .cloned()
        .collect();
    let done = paper_names::add_arxiv_papers(papers, &selected, &Default::default());
    if !done.unresolved.is_empty() {
        bail!("canary identities remain ambiguous: {:?}", done.unresolved);
    }
    validate_canary(papers, expected)?;
    Ok(done)
}

pub fn validate_canary(papers: &[Article], expected: &[PaperCanary]) -> Result<()> {
    validate_consistency(papers)?;
    for canary in expected {
        let rows: Vec<_> = papers
            .iter()
            .filter(|p| paper_names::paper_arxiv_id(p).as_deref() == Some(canary.id.as_str()))
            .collect();
        if rows.is_empty() {
            bail!("canary {} is absent", canary.id);
        }
        for row in rows {
            let Some(proof) = row.paper.as_ref().and_then(|m| m.verified_arxiv.as_ref()) else {
                bail!("canary {} has no source verification", canary.id);
            };
            if normalize_text(&row.title) != normalize_text(&canary.title)
                || !proof
                    .authors
                    .first()
                    .is_some_and(|a| paper_names::same_author(a, &canary.first_author))
                || proof.submitted != canary.submitted
            {
                bail!("canary {} has unresolved metadata", canary.id);
            }
        }
    }
    Ok(())
}

/// Select existing identities from the corpus, not a curated ID list.
/// Corrections without source snapshots are verified first. One ordinary
/// enrichment run selects at most 100; larger work uses ConsistencyQueue.
pub fn consistency_ids(papers: &[Article], budget: usize) -> Vec<String> {
    let mut ids = Vec::new();
    for priority in [true, false] {
        for row in papers {
            let correction = row
                .paper
                .as_ref()
                .is_some_and(|m| !m.corrections.is_empty() && m.verified_arxiv.is_none());
            if correction != priority {
                continue;
            }
            if let Some(id) = paper_names::paper_arxiv_id(row) {
                if !ids.contains(&id) && ids.len() < budget.min(100) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

/// Validate every row's internal/source-backed consistency without
/// requiring particular titles or external coverage for legacy records.
/// Unverified legacy data stays unverified; a correction must carry proof.
pub fn validate_consistency(papers: &[Article]) -> Result<()> {
    let mut identities = std::collections::HashMap::<String, (&str, Option<&str>)>::new();
    for (index, row) in papers.iter().enumerate() {
        let item = row
            .item
            .as_deref()
            .filter(|id| !id.contains(char::is_whitespace));
        if !item.is_some_and(|id| {
            id.starts_with("10.") && id.contains('/')
                || id
                    .strip_prefix('W')
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        }) {
            bail!("paper row {index} has no valid primary DOI/OpenAlex identity");
        }
        if row.title.trim().is_empty() {
            bail!("paper row {index} has an empty title");
        }
        if let Some(item) = row.item.as_deref().filter(|id| !id.is_empty()) {
            let author = row
                .paper
                .as_ref()
                .and_then(|m| m.authors.first())
                .map(String::as_str);
            if let Some((title, first)) = identities.get(&item.to_ascii_lowercase()) {
                if normalize_text(title) != normalize_text(&row.title)
                    || first
                        .zip(author)
                        .is_some_and(|(a, b)| !paper_names::same_author(a, b))
                {
                    bail!("paper row {index} has conflicting records for source ID {item}");
                }
            } else {
                identities.insert(item.to_ascii_lowercase(), (&row.title, author));
            }
        }
        let Some(m) = &row.paper else {
            continue;
        };
        if m.write().is_none() {
            bail!("paper row {index} has invalid or oversized metadata");
        }
        if m.publication_year.is_some_and(|y| !(1..=9999).contains(&y))
            || m.publication_date
                .as_deref()
                .zip(m.publication_year)
                .is_some_and(|(d, y)| d[..4].parse::<i32>().ok() != Some(y))
        {
            bail!("paper row {index} has conflicting publication date/year");
        }
        if m.doi
            .as_deref()
            .zip(row.item.as_deref().filter(|id| id.starts_with("10.")))
            .is_some_and(|(a, b)| !a.eq_ignore_ascii_case(b))
            || m.openalex_id
                .as_deref()
                .zip(row.item.as_deref().filter(|id| id.starts_with('W')))
                .is_some_and(|(a, b)| a != b)
        {
            bail!("paper row {index} has conflicting primary source IDs");
        }
        let primary = row.item.as_deref().and_then(|s| {
            s.to_ascii_lowercase()
                .strip_prefix(crate::openalex::ARXIV_DOI)
                .map(str::to_string)
        });
        if primary
            .as_ref()
            .zip(m.arxiv_id.as_ref())
            .is_some_and(|(a, b)| a != b)
            || row
                .website
                .as_deref()
                .and_then(paper_names::arxiv_id_of)
                .as_ref()
                .zip(m.arxiv_id.as_ref())
                .is_some_and(|(a, b)| a != b)
        {
            bail!("paper row {index} has conflicting arXiv identities");
        }
        if primary.is_some()
            && (m
                .publication_date
                .as_ref()
                .zip(m.preprint_date.as_ref())
                .is_some_and(|(a, b)| a != b)
                || m.preprint_date
                    .as_deref()
                    .zip(m.publication_year)
                    .is_some_and(|(d, y)| d[..4].parse::<i32>().ok() != Some(y)))
        {
            bail!("paper row {index} has conflicting primary preprint dates");
        }
        let Some(proof) = &m.verified_arxiv else {
            if !m.corrections.is_empty() {
                bail!("paper row {index} has a correction without source verification");
            }
            continue;
        };
        if paper_names::arxiv_id_of(&format!("https://arxiv.org/abs/{}", proof.id)).as_deref()
            != Some(proof.id.as_str())
            || m.arxiv_id.as_deref() != Some(proof.id.as_str())
            || normalize_text(&row.title) != normalize_text(&proof.title)
            || !m
                .authors
                .first()
                .zip(proof.authors.first())
                .is_some_and(|(a, b)| paper_names::same_author(a, b))
            || m.preprint_date.as_deref() != Some(proof.submitted.as_str())
            || m.preprint_version_date != proof.updated
        {
            bail!("paper row {index} conflicts with its arXiv source snapshot");
        }
        if primary.is_some()
            && (m.authors != proof.authors
                || m.publication_date.as_deref() != Some(proof.submitted.as_str())
                || m.publication_year != proof.submitted[..4].parse().ok()
                || m.version_date != proof.updated)
        {
            bail!("paper row {index} has conflicting verified preprint metadata");
        }
    }
    Ok(())
}

/// A queue belongs to one staged generation. The caller supplies its stable
/// generation key; changed keys/IDs restart the queue. No live-query use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsistencyQueue {
    version: u32,
    generation: String,
    ids: Vec<String>,
    pub next: usize,
    pub verified: Vec<String>,
    pub unresolved: Vec<String>,
}

impl ConsistencyQueue {
    pub fn new(generation: &str, ids: &[String]) -> Result<Self> {
        let mut ids = ids.to_vec();
        ids.sort();
        ids.dedup();
        if ids.len() > 10_000 {
            bail!("arXiv consistency queue exceeds 10000 identities");
        }
        if ids.iter().any(|id| {
            paper_names::arxiv_id_of(&format!("https://arxiv.org/abs/{id}")).as_deref() != Some(id)
        }) {
            bail!("invalid arXiv consistency identity");
        }
        Ok(Self {
            version: 2,
            generation: generation.into(),
            ids,
            next: 0,
            verified: vec![],
            unresolved: vec![],
        })
    }

    pub fn resume(path: &Path, generation: &str, ids: &[String]) -> Result<Self> {
        let fresh = Self::new(generation, ids)?;
        let ids = &fresh.ids;
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(queue) = serde_json::from_slice::<Self>(&bytes) {
                if queue.version == 2
                    && queue.generation == generation
                    && &queue.ids == ids
                    && queue.next <= ids.len()
                {
                    return Ok(queue);
                }
            }
        }
        Ok(fresh)
    }

    pub fn pending(&self, budget: usize) -> &[String] {
        &self.ids[self.next..(self.next + budget.min(100)).min(self.ids.len())]
    }

    /// Apply a single authoritative batch and retain a completion report.
    /// Missing responses stay explicit and can seed a later retry queue.
    pub fn apply(&mut self, papers: &mut [Article], budget: usize, found: &[ArxivPaper]) -> Named {
        let pending = self.pending(budget).to_vec();
        let unique = paper_names::unique_sources(found);
        let accepted: Vec<ArxivPaper> = pending
            .iter()
            .filter_map(|id| unique.get(id.as_str()).map(|s| (*s).clone()))
            .collect();
        let done = paper_names::verify_existing(papers, &accepted);
        for id in &pending {
            let consistent = accepted.iter().find(|p| &p.id == id).is_some_and(|source| {
                let rows: Vec<&Article> = papers
                    .iter()
                    .filter(|p| paper_names::paper_arxiv_id(p).as_ref() == Some(id))
                    .collect();
                !rows.is_empty()
                    && !done.unresolved.contains(id)
                    && rows.iter().all(|row| {
                        validate_consistency(std::slice::from_ref(*row)).is_ok()
                            && normalize_text(&row.title) == normalize_text(&source.title)
                            && row
                                .paper
                                .as_ref()
                                .and_then(|m| m.verified_arxiv.as_ref())
                                .is_some_and(|p| {
                                    p.id == source.id
                                        && p.submitted
                                            == source.published.as_deref().unwrap_or_default()
                                })
                    })
            });
            if consistent {
                self.verified.push(id.clone());
            } else {
                self.unresolved.push(id.clone());
            }
        }
        self.next += pending.len();
        done
    }

    /// Call only after persisting the corrected staged rows. Saving queue
    /// progress before rows would lose a correction after interruption.
    pub fn save(&self, path: &Path) -> Result<()> {
        let part = path.with_extension("json.part");
        std::fs::write(&part, serde_json::to_vec(self)?)
            .with_context(|| format!("writing {}", part.display()))?;
        std::fs::rename(&part, path).with_context(|| format!("saving {}", path.display()))
    }
}

/// One request batch, at most 100 IDs, for a caller-owned staged generation.
pub async fn verify_batch(
    client: &reqwest::Client,
    papers: &mut [Article],
    queue: &mut ConsistencyQueue,
    budget: usize,
) -> Result<Named> {
    let found = paper_names::fetch_arxiv(client, queue.pending(budget)).await?;
    Ok(queue.apply(papers, budget, &found))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_validation_has_no_curated_paper_requirement() {
        let row = Article {
            title: "Unrelated legitimate journal paper".into(),
            item: Some("10.1234/other".into()),
            ..Default::default()
        };
        assert!(validate_consistency(std::slice::from_ref(&row)).is_ok());
        assert!(validate_consistency(&[]).is_ok());
        assert!(consistency_ids(std::slice::from_ref(&row), 100).is_empty());
        let mut conflict = row.clone();
        conflict.title = "Different paper with same DOI".into();
        assert!(validate_consistency(&[row, conflict]).is_err());
    }

    #[test]
    fn corpus_selected_verification_is_bounded_and_duplicates_cannot_verify() {
        let source = ArxivPaper {
            id: "2401.01234".into(),
            title: "Synthetic paper".into(),
            year: Some(2024),
            authors: vec!["Jane Example".into()],
            published: Some("2024-01-02".into()),
            updated: None,
        };
        let mut rows = vec![Article {
            title: "Wrong title".into(),
            item: Some(source.doi()),
            ..Default::default()
        }];
        let ids = consistency_ids(&rows, 1000);
        assert_eq!(ids.as_slice(), std::slice::from_ref(&source.id));
        let mut queue = ConsistencyQueue::new("test-generation", &ids).unwrap();
        let before = rows.clone();
        queue.apply(&mut rows, 1000, &[source.clone(), source.clone()]);
        assert_eq!(queue.unresolved, ids);
        assert!(queue.verified.is_empty());
        assert_eq!(rows, before);
        let mut retry = ConsistencyQueue::new("test-generation-retry", &queue.unresolved).unwrap();
        retry.apply(&mut rows, 1000, &[source]);
        assert!(retry.unresolved.is_empty());
        assert_eq!(retry.verified, ids);
        validate_consistency(&rows).unwrap();
        let many: Vec<_> = (0..150)
            .map(|n| Article {
                title: format!("Paper {n}"),
                item: Some(format!("10.48550/arxiv.2401.{n:05}")),
                ..Default::default()
            })
            .collect();
        assert_eq!(consistency_ids(&many, 1000).len(), 100);
    }

    #[test]
    fn queues_resume_and_generation_changes_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("verify.json");
        let ids = vec!["2401.01234".to_string(), "2402.05678".to_string()];
        let mut queue = ConsistencyQueue::resume(&path, "candidate-a", &ids).unwrap();
        assert_eq!(queue.pending(1).len(), 1);
        queue.apply(&mut [], 1, &[]);
        assert_eq!(queue.unresolved.len(), 1);
        queue.save(&path).unwrap();
        assert_eq!(
            ConsistencyQueue::resume(&path, "candidate-a", &ids).unwrap(),
            queue
        );
        assert_eq!(
            ConsistencyQueue::resume(&path, "candidate-b", &ids)
                .unwrap()
                .next,
            0
        );
        assert_eq!(
            ConsistencyQueue::resume(&path, "candidate-a", &ids[..1])
                .unwrap()
                .next,
            0
        );
    }

    #[test]
    fn missing_authoritative_metadata_is_a_publication_failure() {
        let mut papers = vec![];
        let expected = vec![PaperCanary {
            id: "2401.01234".into(),
            title: "Example".into(),
            first_author: "Jane Example".into(),
            submitted: "2024-01-02".into(),
        }];
        assert!(repair_canary(&mut papers, &[], &expected).is_err());
        assert!(papers.is_empty());
        assert!(validate_canary(&papers, &expected).is_err());
        assert!(validate_consistency(&papers).is_ok());
    }
}
