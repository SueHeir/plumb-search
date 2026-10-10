//! Publication gates and a bounded, resumable arXiv consistency queue.
//! The fetch/publish owner must validate the candidate before swapping it.

use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::{article::Article, normalize_text};
use serde::{Deserialize, Serialize};

use crate::paper_names::{self, ArxivPaper, Named};

pub struct Landmark {
    pub id: &'static str,
    pub title: &'static str,
    pub first_author: &'static str,
    pub submitted: &'static str,
}

/// Acceptance anchors from the original arXiv records. They do not depend
/// on the supplementary Papers with Code archive being available.
pub const LANDMARKS: &[Landmark] = &[
    Landmark {
        id: "1706.03762",
        title: "Attention Is All You Need",
        first_author: "Ashish Vaswani",
        submitted: "2017-06-12",
    },
    Landmark {
        id: "2005.11401",
        title: "Retrieval-Augmented Generation for Knowledge-Intensive NLP Tasks",
        first_author: "Patrick Lewis",
        submitted: "2020-05-22",
    },
];

pub fn required_arxiv_ids() -> Vec<String> {
    LANDMARKS
        .iter()
        .map(|landmark| landmark.id.into())
        .collect()
}

/// Validate authoritative responses *before* mutating any rows. A missing,
/// wrong-ID or incomplete response cannot be mistaken for verification.
pub fn repair_landmarks(papers: &mut Vec<Article>, found: &[ArxivPaper]) -> Result<Named> {
    for landmark in LANDMARKS {
        let matching: Vec<&ArxivPaper> = found.iter().filter(|p| p.id == landmark.id).collect();
        if matching.len() != 1 {
            bail!("required arXiv verification unresolved for {}", landmark.id);
        }
        let paper = matching[0];
        if normalize_text(&paper.title) != normalize_text(landmark.title)
            || paper.authors.first().map(String::as_str) != Some(landmark.first_author)
            || paper.published.as_deref() != Some(landmark.submitted)
        {
            bail!("required arXiv verification conflicts for {}", landmark.id);
        }
    }
    let required: Vec<ArxivPaper> = found
        .iter()
        .filter(|paper| LANDMARKS.iter().any(|l| l.id == paper.id))
        .cloned()
        .collect();
    let done = paper_names::add_arxiv_papers(papers, &required, &Default::default());
    validate_landmarks(papers)?;
    Ok(done)
}

/// Mandatory generation gate for the ingestion publisher. Failure means
/// keep the previous good generation, never write an unenriched candidate.
pub fn validate_landmarks(papers: &[Article]) -> Result<()> {
    if papers
        .iter()
        .any(|paper| paper.item.as_deref() == Some("10.65215/2q58a426"))
    {
        bail!("audited Transformer DOI remains unresolved; retain the previous generation");
    }
    for landmark in LANDMARKS {
        let matching: Vec<&Article> = papers
            .iter()
            .filter(|paper| paper_names::paper_arxiv_id(paper).as_deref() == Some(landmark.id))
            .collect();
        if matching.is_empty() {
            bail!("required landmark {} is absent", landmark.id);
        }
        for paper in matching {
            let Some(metadata) = paper.paper.as_ref() else {
                bail!(
                    "required landmark {} has no verification metadata",
                    landmark.id
                );
            };
            if normalize_text(&paper.title) != normalize_text(landmark.title)
                || !metadata
                    .authors
                    .first()
                    .is_some_and(|a| paper_names::same_author(a, landmark.first_author))
                || metadata.preprint_date.as_deref() != Some(landmark.submitted)
                || metadata.write().is_none()
            {
                bail!("required landmark {} has unresolved metadata", landmark.id);
            }
            if paper.item.as_deref().is_some_and(|id| {
                id.eq_ignore_ascii_case(&format!("{}{}", crate::openalex::ARXIV_DOI, landmark.id))
            }) && (metadata.publication_date.as_deref() != Some(landmark.submitted)
                || metadata.publication_year != landmark.submitted[..4].parse().ok())
            {
                bail!(
                    "required landmark {} has conflicting preprint dates",
                    landmark.id
                );
            }
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
    pub fn resume(path: &Path, generation: &str, ids: &[String]) -> Result<Self> {
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
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(queue) = serde_json::from_slice::<Self>(&bytes) {
                if queue.version == 1
                    && queue.generation == generation
                    && queue.ids == ids
                    && queue.next <= ids.len()
                {
                    return Ok(queue);
                }
            }
        }
        Ok(Self {
            version: 1,
            generation: generation.into(),
            ids,
            next: 0,
            verified: vec![],
            unresolved: vec![],
        })
    }

    pub fn pending(&self, budget: usize) -> &[String] {
        &self.ids[self.next..(self.next + budget.min(100)).min(self.ids.len())]
    }

    /// Apply a single authoritative batch and retain a completion report.
    /// Missing responses stay explicit and can seed a later retry queue.
    pub fn apply(&mut self, papers: &mut [Article], budget: usize, found: &[ArxivPaper]) -> Named {
        let pending = self.pending(budget).to_vec();
        let accepted: Vec<ArxivPaper> = found
            .iter()
            .filter(|p| {
                pending.contains(&p.id)
                    && !p.title.is_empty()
                    && !p.authors.is_empty()
                    && p.published.is_some()
            })
            .cloned()
            .collect();
        let done = paper_names::verify_existing(papers, &accepted);
        for id in &pending {
            let consistent = accepted.iter().find(|p| &p.id == id).is_some_and(|source| {
                let rows: Vec<&Article> = papers
                    .iter()
                    .filter(|p| paper_names::paper_arxiv_id(p).as_ref() == Some(id))
                    .collect();
                !rows.is_empty()
                    && rows.iter().all(|row| {
                        normalize_text(&row.title) == normalize_text(&source.title)
                            && row.paper.as_ref().is_some_and(|m| {
                                m.preprint_date == source.published
                                    && m.authors
                                        .first()
                                        .zip(source.authors.first())
                                        .is_some_and(|(a, b)| paper_names::same_author(a, b))
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
    fn queues_resume_and_generation_changes_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("verify.json");
        let ids = required_arxiv_ids();
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
        assert!(repair_landmarks(&mut papers, &[]).is_err());
        assert!(papers.is_empty());
        assert!(validate_landmarks(&papers).is_err());
    }
}
