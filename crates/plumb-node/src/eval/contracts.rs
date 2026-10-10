//! Reproducible offline contracts. Reports are observations of this evaluator's
//! pipeline, not attestations that an HTTP deployment used the same assembly.
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use plumb_core::{host_of, Operators};
use plumb_index::pages::{operators_allow, options_allow, place_operator_pages, Page};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Relevant {
    pub identity: String,
    #[serde(default = "default_grade")]
    pub grade: u8,
}
fn default_grade() -> u8 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Negative {
    pub identity: String,
    /// Fails even when a relevant URL also appears. Ranks are 1-based.
    pub max_rank: usize,
    #[serde(default = "default_reason")]
    pub reason: String,
}
fn default_reason() -> String {
    "irrelevant".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    pub kind: Option<String>,
    pub site: Option<String>,
    pub language: Option<String>,
    pub country: Option<String>,
    pub only_country: bool,
    pub exact: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Expect {
    pub kind: Option<String>,
    pub site: Option<String>,
    pub language: Option<String>,
    pub country: Option<String>,
    /// These checks inspect the answer and its note, not query/label text.
    pub answer_contains: Vec<String>,
    pub answer_excludes: Vec<String>,
    pub date_contains: Vec<String>,
    pub abstain: bool,
    /// Identity-tool expectations are consumed by eval/official_site/run.py.
    pub verdict: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub id: String,
    pub family: String,
    pub category: String,
    pub query: String,
    #[serde(default = "search_tool")]
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
    /// "manual" requires reviewer + evidence. Candidates and self-grades
    /// are reported separately and can never become release labels silently.
    pub label_status: String,
    #[serde(default)]
    pub reviewer: Option<String>,
    #[serde(default)]
    pub evidence: Value,
    #[serde(default)]
    pub self_grade: Value,
    #[serde(default)]
    pub root_cause: Value,
    #[serde(default)]
    pub split: Option<String>,
    #[serde(default)]
    pub options: Options,
    #[serde(default)]
    pub relevant: Vec<Relevant>,
    #[serde(default)]
    pub negatives: Vec<Negative>,
    #[serde(default)]
    pub expect: Expect,
}
fn search_tool() -> String {
    "search".into()
}

/// Same stable hash as legacy suites, applied to the family, never a paraphrase.
pub fn family_half(family: &str) -> Half {
    half_of(family)
}
fn family_key(family: &str) -> String {
    family
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

impl Case {
    pub fn half(&self) -> Half {
        match self.split.as_deref() {
            Some("tune") => Half::Tune,
            Some("held-out") => Half::HeldOut,
            _ => family_half(&self.family),
        }
    }
    fn validate(&self) -> Result<()> {
        if [&self.id, &self.family, &self.category, &self.query]
            .iter()
            .any(|s| s.trim().is_empty())
        {
            bail!("id, family, category and query must be nonempty");
        }
        if !["search", "official_site", "check_lookalike", "facts"].contains(&self.tool.as_str()) {
            bail!("unsupported tool {}", self.tool);
        }
        if !["manual", "candidate", "legacy"].contains(&self.label_status.as_str()) {
            bail!("label_status must be manual, candidate or legacy");
        }
        if self.label_status == "manual"
            && (self.reviewer.as_deref().is_none_or(|s| s.trim().is_empty())
                || self.evidence.is_null())
        {
            bail!("manual labels require reviewer and evidence");
        }
        if self
            .split
            .as_deref()
            .is_some_and(|s| !["tune", "held-out"].contains(&s))
        {
            bail!("split must be tune or held-out");
        }
        if self
            .relevant
            .iter()
            .any(|r| r.identity.is_empty() || !(1..=3).contains(&r.grade))
        {
            bail!("relevance identities must be nonempty, grades 1..3");
        }
        if self
            .negatives
            .iter()
            .any(|n| n.identity.is_empty() || n.max_rank == 0)
        {
            bail!("negatives need an identity and positive max_rank");
        }
        if self.relevant.is_empty()
            && !self.expect.abstain
            && self.expect.answer_contains.is_empty()
            && self.expect.verdict.is_none()
        {
            bail!("a contract needs relevance, an answer, a verdict or abstention");
        }
        for kind in [&self.options.kind, &self.expect.kind]
            .into_iter()
            .flatten()
        {
            if ![
                "site",
                "article",
                "question",
                "package",
                "repo",
                "book",
                "paper",
                "docs",
                "reference",
                "subpage",
            ]
            .contains(&kind.as_str())
            {
                bail!("unsupported kind {kind}");
            }
        }
        Ok(())
    }
}

pub fn parse_cases(text: &str) -> Result<Vec<Case>> {
    let mut cases = Vec::new();
    let mut ids = HashSet::new();
    let mut families = BTreeMap::new();
    for (i, line) in text.trim_start_matches('\u{feff}').lines().enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let case: Case = serde_json::from_str(line).with_context(|| format!("line {}", i + 1))?;
        case.validate()
            .with_context(|| format!("line {} ({})", i + 1, case.id))?;
        if !ids.insert(case.id.clone()) {
            bail!("duplicate id {}", case.id);
        }
        let key = family_key(&case.family);
        if families
            .insert(key, case.half())
            .is_some_and(|half| half != case.half())
        {
            bail!("family {} leaks across splits", case.family);
        }
        cases.push(case);
    }
    Ok(cases)
}

/// Match URL prefixes explicitly; domains use a hostname boundary, so a
/// forbidden brand.com also catches api.brand.com but never brand.com.evil.
pub fn matches(identity: &str, key: &str) -> bool {
    if identity.contains("://") {
        return is_expected(identity, key);
    }
    let wanted = identity.trim_end_matches('.').to_ascii_lowercase();
    let host = host_of(key).unwrap_or_else(|| key.to_ascii_lowercase());
    host == wanted || host.ends_with(&format!(".{wanted}"))
}

fn kind(page: &Page) -> &str {
    if page.is_article() {
        "article"
    } else if page.is_question() {
        "question"
    } else if page.package.is_some() {
        "package"
    } else {
        match page.set.as_str() {
            "github" => "repo",
            "books" => "book",
            "papers" => "paper",
            "docs" => "docs",
            "reference" | "reference2" => "reference",
            "subpages2" => "subpage",
            _ => &page.set,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Row {
    identities: Vec<String>,
    kind: String,
    language: Option<String>,
    country: Option<String>,
}

fn rows(hits: &[Hit], placed: &[PlacedPage]) -> Vec<Row> {
    listed_with_pages(hits, placed.to_vec())
        .into_iter()
        .map(|identities| {
            let first = &identities[0];
            if let Some(hit) = hits.iter().find(|h| &h.domain == first) {
                Row {
                    identities,
                    kind: "site".into(),
                    language: None,
                    country: hit.country.clone(),
                }
            } else if let Some(page) = placed.iter().map(|p| &p.hit.page).find(|p| &p.url == first)
            {
                Row {
                    identities,
                    kind: kind(page).into(),
                    language: page.language().map(str::to_owned),
                    country: None,
                }
            } else {
                unreachable!("listed row has a source")
            }
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Score {
    rank: Option<usize>,
    top1: bool,
    top3: bool,
    mrr: f64,
    ndcg10: Option<f64>,
    recall: BTreeMap<usize, Option<f64>>,
    wrong_domain: bool,
    wrong_brand_top3: bool,
    violations: Vec<String>,
    passed: bool,
}

fn score(case: &Case, rows: &[Row], answer: &Option<plumb_answer::Answer>, limit: usize) -> Score {
    let rank_of_identity = |identity: &str| {
        rows.iter()
            .position(|row| row.identities.iter().any(|k| matches(identity, k)))
            .map(|i| i + 1)
    };
    let rank = case
        .relevant
        .iter()
        .filter_map(|r| rank_of_identity(&r.identity))
        .min();
    let mut violations = Vec::new();
    let mut wrong_domain = false;
    let mut wrong_brand_top3 = false;
    for negative in &case.negatives {
        if let Some(rank) = rank_of_identity(&negative.identity).filter(|r| *r <= negative.max_rank)
        {
            wrong_domain = true;
            wrong_brand_top3 |= negative.reason == "wrong_brand" && rank <= 3;
            violations.push(format!(
                "negative {} at rank {rank} ({})",
                negative.identity, negative.reason
            ));
        }
    }
    if !case.relevant.is_empty() && rank.is_none() {
        violations.push("relevant identity not serialized".into());
    }
    if case.expect.abstain && (!rows.is_empty() || answer.is_some()) {
        violations.push("expected abstention".into());
    }
    // Metadata contracts apply to a relevant row; an empty/missing result
    // cannot satisfy them vacuously. Unknown language/country is a failure.
    let selected = rank.and_then(|r| rows.get(r - 1)).or_else(|| rows.first());
    for (name, want, got) in [
        (
            "kind",
            case.expect.kind.as_deref(),
            selected.map(|r| r.kind.as_str()),
        ),
        (
            "language",
            case.expect.language.as_deref(),
            selected.and_then(|r| r.language.as_deref()),
        ),
        (
            "country",
            case.expect.country.as_deref(),
            selected.and_then(|r| r.country.as_deref()),
        ),
    ] {
        if let Some(want) = want {
            if got != Some(want) {
                violations.push(format!("{name}: expected {want}, observed {got:?}"));
            }
        }
    }
    if let Some(site) = &case.expect.site {
        if selected.is_none_or(|r| !r.identities.iter().any(|k| matches(site, k))) {
            violations.push(format!("expected site {site}"));
        }
    }
    let text = answer
        .as_ref()
        .map(|a| format!("{} {}", a.answer, a.note.as_deref().unwrap_or("")))
        .unwrap_or_default()
        .to_lowercase();
    for expected in case
        .expect
        .answer_contains
        .iter()
        .chain(&case.expect.date_contains)
    {
        if !text.contains(&expected.to_lowercase()) {
            violations.push(format!("answer missing {expected:?}"));
        }
    }
    for excluded in &case.expect.answer_excludes {
        if text.contains(&excluded.to_lowercase()) {
            violations.push(format!("answer contains excluded {excluded:?}"));
        }
    }
    let gain = |grade: u8| (2f64).powi(i32::from(grade)) - 1.;
    let dcg: f64 = rows
        .iter()
        .take(10)
        .enumerate()
        .map(|(i, row)| {
            // A URL repeated under two rows cannot earn relevance twice.
            let grade = case
                .relevant
                .iter()
                .filter(|r| {
                    row.identities.iter().any(|k| matches(&r.identity, k))
                        && rank_of_identity(&r.identity) == Some(i + 1)
                })
                .map(|r| r.grade)
                .max()
                .unwrap_or(0);
            gain(grade) / ((i + 2) as f64).log2()
        })
        .sum();
    let mut grades: Vec<_> = case.relevant.iter().map(|r| r.grade).collect();
    grades.sort_unstable_by(|a, b| b.cmp(a));
    let ideal: f64 = grades
        .iter()
        .take(10)
        .enumerate()
        .map(|(i, &g)| gain(g) / ((i + 2) as f64).log2())
        .sum();
    Score {
        rank,
        top1: rank == Some(1),
        top3: rank.is_some_and(|r| r <= 3),
        mrr: rank.map_or(0., |r| 1. / r as f64),
        ndcg10: (ideal > 0.).then(|| dcg / ideal),
        recall: [10, 50, 100]
            .into_iter()
            .map(|depth| {
                let n = case
                    .relevant
                    .iter()
                    .filter(|r| rank_of_identity(&r.identity).is_some_and(|rank| rank <= depth))
                    .count();
                (
                    depth,
                    (!case.relevant.is_empty() && limit >= depth)
                        .then(|| ratio(n, case.relevant.len())),
                )
            })
            .collect(),
        wrong_domain,
        wrong_brand_top3,
        passed: violations.is_empty(),
        violations,
    }
}

fn identities(pages: &[plumb_index::pages::PageHit]) -> Vec<String> {
    pages.iter().map(|p| p.page.url.clone()).collect()
}
fn row_rank(rows: &[Row], case: &Case) -> Option<usize> {
    rows.iter()
        .position(|row| {
            row.identities
                .iter()
                .any(|k| case.relevant.iter().any(|r| matches(&r.identity, k)))
        })
        .map(|i| i + 1)
}

fn observe(
    args: &EvalArgs,
    setup: &Setup,
    cfg: &plumb_index::RankConfig,
    case: &Case,
) -> Result<Value> {
    let start = Instant::now();
    let options = SearchOptions {
        country: case
            .options
            .country
            .clone()
            .or_else(|| args.country.clone()),
        language: case.options.language.clone().or_else(|| args.lang.clone()),
        exact: case.options.exact || args.exact,
        only_country: case.options.only_country,
        ..SearchOptions::default()
    };
    let query = if let Some(site) = &case.options.site {
        format!("{} site:{site}", case.query)
    } else {
        case.query.clone()
    };
    let ops = Operators::parse(&query);
    let words = if ops.any() { &ops.words } else { &query };
    let meaning = setup.meaning.as_ref().and_then(|m| m.query(words));
    let semantic = meaning.as_ref().map(|m| m as &dyn Meaning);
    let mut found = setup
        .searcher
        .search_meaning(&query, args.limit, cfg, &options, semantic)?;
    let typed = case.options.kind.as_deref();
    let mut raw_pages = if let Some(pages) = &setup.pages {
        if typed == Some("site") {
            Vec::new()
        } else if typed.is_some() || ops.any() {
            pages.search_naming_docs(words, &ops, typed == Some("docs"), 200)?
        } else {
            pages.search(words, 10)?
        }
    } else {
        Vec::new()
    };
    if typed.is_none() && !ops.any() {
        if let Some(pages) = &setup.pages {
            pages.add_other_number(words, &found.hits, &mut raw_pages, 10)?;
        }
    }
    let filtered: Vec<_> = raw_pages
        .iter()
        .filter(|p| {
            options_allow(&options, &p.page)
                && operators_allow(&ops, &p.page)
                && typed.is_none_or(|wanted| kind(&p.page) == wanted)
        })
        .cloned()
        .collect();
    let mut placed;
    if typed.is_some_and(|k| k != "site") {
        found.hits.clear();
        placed = filtered
            .iter()
            .take(args.limit)
            .enumerate()
            .map(|(at, hit)| PlacedPage {
                hit: hit.clone(),
                under: None,
                at,
            })
            .collect();
    } else if ops.any() {
        placed = place_operator_pages(&ops, &found.hits, filtered.clone());
    } else {
        if cfg.add_named_site {
            add_named_site(&mut found.hits, &filtered, |d| {
                setup.searcher.site(d).ok().flatten()
            });
        }
        if cfg.drop_namesakes {
            drop_namesakes_of_words(&mut found.hits, &filtered);
        }
        lift_named_sites(&mut found.hits, &filtered);
        if let Some(pages) = &setup.pages {
            pages.note_demand(&mut found.hits)?;
        }
        placed = place_pages(words, &found.hits, filtered.clone());
    }
    let blended = rows(&found.hits, &placed);
    let selected = placed
        .iter()
        .map(|p| p.hit.page.url.clone())
        .collect::<Vec<_>>();
    if cfg.learned && typed.is_none() && !ops.any() {
        plumb_index::learned::reorder(
            plumb_index::learned::Model::builtin(),
            words,
            &mut found.hits,
            &mut placed,
        );
    }
    let learned = rows(&found.hits, &placed);
    let now = args.eval_time.expect("report requires fixed time");
    let mut fact_lookup = None;
    let mut answer = plumb_answer::answer(&case.query, now as i64, None);
    if answer.is_none() {
        if let (Some(asked), Some(pages)) = (
            plumb_core::facts::fact_asked(&case.query),
            setup.pages.as_ref(),
        ) {
            let lookup_sites =
                setup
                    .searcher
                    .search_meaning(&asked.subject, 5, cfg, &options, None)?;
            let lookup_pages = pages.search(&asked.subject, 10)?;
            let lookup = place_pages(&asked.subject, &lookup_sites.hits, lookup_pages);
            answer = crate::web::answers::fact_answer(&asked, &lookup, now);
            fact_lookup = Some(
                json!({"subject":asked.subject,"sites":lookup_sites.hits,"pages":lookup,"mode":"legacy_eval_subject_lookup"}),
            );
        }
    }
    let route = crate::sources::route(
        words,
        answer.as_ref().map(|a| a.kind),
        options.country.as_deref(),
    );
    if typed.is_none() {
        if let Some(route) = route {
            crate::sources::lead_with(&mut found.hits, &mut placed, &route, |d| {
                setup.searcher.site(d).ok().flatten()
            });
        }
    }
    let mut serialized = rows(&found.hits, &placed);
    serialized.truncate(args.limit);
    let latency_ms = start.elapsed().as_secs_f64() * 1000.;
    let response = json!({"hits": found.hits, "pages": placed, "answer": answer, "spelling": found.spelling, "rows": serialized});
    let response_bytes = serde_json::to_vec(&response)?.len();
    // Diagnostics are outside latency measurement: expensive collection must
    // not inflate the reported user-path latency.
    let expected: Vec<_> = case.relevant.iter().map(|r| r.identity.clone()).collect();
    let diagnostic_pages = setup
        .pages
        .as_ref()
        .map(|pages| pages.search_naming_docs(words, &ops, typed == Some("docs"), 100))
        .transpose()?
        .unwrap_or_default();
    let pool = setup
        .searcher
        .candidate_pool(&query, cfg, &options, semantic)?;
    let lexical = setup
        .searcher
        .words_order(&query, 100, cfg, &expected)?
        .map(|w| w.order);
    let present: Vec<_> = expected
        .iter()
        .map(|e| {
            let present = if e.contains("://") {
                match e.strip_suffix('*') {
                    Some(prefix) => setup.page_urls.iter().any(|u| u.starts_with(prefix)),
                    None => setup.page_urls.contains(e),
                }
            } else {
                setup.searcher.has_domain(e)
            };
            json!({"identity":e,"present":present})
        })
        .collect();
    let semantic_keys = semantic.map(|m| m.nearest()).unwrap_or_default();
    let candidate_identities: Vec<_> = lexical
        .iter()
        .flatten()
        .chain(&semantic_keys)
        .cloned()
        .chain(identities(&diagnostic_pages))
        .collect();
    let candidate_recall: BTreeMap<_, _> = [10, 50, 100]
        .into_iter()
        .map(|depth| {
            let lexical_keys = lexical.as_deref().unwrap_or_default();
            let semantic_keys = semantic.map(|m| m.nearest()).unwrap_or_default();
            let pages_keys = identities(&diagnostic_pages);
            let count = expected
                .iter()
                .filter(|e| {
                    lexical_keys
                        .iter()
                        .take(depth)
                        .chain(semantic_keys.iter().take(depth))
                        .chain(pages_keys.iter().take(depth))
                        .any(|k| matches(e, k))
                })
                .count();
            (
                depth,
                (!expected.is_empty()).then(|| ratio(count, expected.len())),
            )
        })
        .collect();
    let scored = score(case, &serialized, &answer, args.limit);
    Ok(json!({
        "type":"query", "case":case, "half":if case.half() == Half::Tune {"tune"} else {"held-out"},
        "query":query,"options":options,"score":scored,"candidate_recall":candidate_recall,"latency_ms":latency_ms,"response_bytes":response_bytes,"response":response,
        "stages":{
            "record_present":present,
            "exact_identity_lookup":{"mode":"label_address_membership", "production_entity_lookup_observed":false},
            "fact_subject_lookup":fact_lookup,
            "lexical_candidate":{"sites":lexical,"pages":identities(&raw_pages)},
            "semantic_candidate":semantic.map(|m| m.nearest()),
            "diagnostic_candidates":{"pages":identities(&diagnostic_pages),"depth":100,"used_by_search_pipeline":false},
            "source_filtered_candidate":{"site_pool":{"words":pool.words,"popular":pool.popular,"named":pool.named,"kind":pool.kind,"meaning":pool.meaning},"pages":identities(&filtered)},
            "page_selection":selected,"blended_row":{"rows":blended,"rank":row_rank(&blended,case)},
            "learned_order":{"enabled":cfg.learned,"rows":learned,"rank":row_rank(&learned,case)},
            "serialized_output":{"rows":serialized,"rank":scored.rank},
            "loss": if scored.rank.is_some() {"serialized"} else if row_rank(&learned,case).is_some() {"serialization"}
                else if row_rank(&blended,case).is_some() {"learned_order"}
                else if selected.iter().any(|k| expected.iter().any(|e| matches(e,k))) {"blending"}
                else if identities(&filtered).iter().any(|k| expected.iter().any(|e| matches(e,k))) {"page_selection"}
                else if identities(&raw_pages).iter().any(|k| expected.iter().any(|e| matches(e,k))) {"source_filter"}
                else if candidate_identities.iter().any(|k| expected.iter().any(|e| matches(e,k))) {"candidate_not_selected"}
                else if present.iter().any(|p| p["present"] == true) {"retrieval"} else {"record_missing"}
        }
    }))
}

fn fingerprint(path: &Path) -> Result<Value> {
    let mut files = Vec::new();
    fn visit(path: &Path, root: &Path, files: &mut Vec<(String, u64, String)>) -> Result<()> {
        if path.is_dir() {
            let mut entries = fs::read_dir(path)?
                .map(|e| e.map(|e| e.path()))
                .collect::<std::io::Result<Vec<_>>>()?;
            entries.sort();
            for path in entries {
                visit(&path, root, files)?;
            }
        } else if path.is_file() {
            // Tantivy lockfiles can change on read-only open; never include
            // them as corpus bytes. Do not include scratch/cache directories.
            if path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(".lock"))
            {
                return Ok(());
            }
            let mut reader = BufReader::new(File::open(path)?);
            let mut hash = Sha256::new();
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hash.update(&buf[..n]);
            }
            let name = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();
            files.push((
                name,
                fs::metadata(path)?.len(),
                format!("{:x}", hash.finalize()),
            ));
        } else {
            bail!("corpus path {} is unavailable", path.display());
        }
        Ok(())
    }
    let root = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(Path::new("."))
    };
    visit(path, root, &mut files)?;
    let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&files)?));
    let bytes: u64 = files.iter().map(|(_, bytes, _)| bytes).sum();
    Ok(json!({"path":path,"sha256":digest,"bytes":bytes,"files":files}))
}

fn corpus(args: &EvalArgs) -> Result<Vec<Value>> {
    std::iter::once(&args.index)
        .chain(&args.pages)
        .map(|p| fingerprint(p))
        .collect()
}

fn legacy(path: &Path) -> Result<Vec<Case>> {
    let text = fs::read_to_string(path)?;
    Ok(parse_queries(&text)?
        .into_iter()
        .map(|q| Case {
            id: format!("{}:{}", path.display(), q.line),
            family: q.query.clone(),
            category: path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            query: q.query,
            tool: search_tool(),
            arguments: Value::Null,
            label_status: "legacy".into(),
            reviewer: None,
            evidence: json!({"suite":path,"line":q.line}),
            self_grade: Value::Null,
            root_cause: Value::Null,
            split: None,
            options: Options::default(),
            expect: Expect::default(),
            relevant: q
                .expected
                .into_iter()
                .map(|identity| Relevant { identity, grade: 1 })
                .collect(),
            negatives: Vec::new(),
        })
        .collect())
}

pub(super) fn run(args: &EvalArgs) -> Result<()> {
    if let Some(model) = &args.meaning.model {
        if model.join(plumb_embed::SERVER_FILE).exists() {
            bail!("offline reports require local model weights, not an embedding server");
        }
    }
    let mut cases = Vec::new();
    for file in &args.queries {
        cases.extend(legacy(file)?);
    }
    for file in &args.acceptance {
        cases.extend(
            parse_cases(&fs::read_to_string(file)?)
                .with_context(|| format!("reading {}", file.display()))?,
        );
    }
    let mut families = BTreeMap::new();
    let mut ids = HashSet::new();
    for case in &cases {
        if !ids.insert(&case.id) {
            bail!("duplicate case id {}", case.id);
        }
        let family = family_key(&case.family);
        if families
            .insert(family, case.half())
            .is_some_and(|h| h != case.half())
        {
            bail!("family {} leaks across suite splits", case.family);
        }
    }
    let tool_cases = cases.iter().filter(|c| c.tool != "search").count();
    cases.retain(|c| c.tool == "search" && args.half.is_none_or(|h| c.half() == h));
    if cases.is_empty() {
        bail!("no search contracts selected; identity tools use eval/official_site/run.py");
    }
    let mut cfg = args.rank.unwrap_or_default();
    if let Some(alpha) = args.alpha {
        cfg.alpha = alpha;
    }
    let report = args.report.as_ref().expect("report selected");
    let parent = fs::canonicalize(
        report
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    let output = parent.join(report.file_name().context("report needs a filename")?);
    if output.starts_with(fs::canonicalize(&args.index)?)
        || args
            .pages
            .iter()
            .any(|p| fs::canonicalize(p).ok().is_some_and(|p| p == output))
    {
        bail!("report must be outside the corpus; refusing to overwrite corpus data");
    }
    let before = corpus(args)?;
    let setup = open_setup(args)?;
    let path = args.report.as_ref().expect("report selected");
    let mut out =
        BufWriter::new(File::create(path).with_context(|| format!("writing {}", path.display()))?);
    let manifest = json!({
        "type":"manifest","schema":1,"build":crate::build_info::current(),"target":"in-process-offline","transport":"core",
        "eval_time":args.eval_time,"rank":cfg,"learned_model_sha256":format!("{:x}",Sha256::digest(include_bytes!("../../../plumb-index/src/learned_model.json"))),
        "model":setup.meaning.as_ref().map(|m| format!("{:?}",m.embedder().id())),
        "vectors":args.meaning.vectors.as_ref().map(|p| fingerprint(p)).transpose()?,
        "query_instruction":format!("{:?}",args.meaning.query_instruction),
        "corpus":before,"indexed_counts":{"sites":setup.searcher.num_docs(),"pages":setup.pages.as_ref().map(|p| p.num_pages())},
        "sources":args.pages,"pages_top":args.pages_top,"limit":args.limit,
        "features":{"findings":false,"personalization":false,"plugins":false,"external_results":false,"peers":false},
        "skipped_tool_cases":tool_cases,"cases":cases.len(),"half":format!("{:?}",args.half),
        "suite_fingerprints":args.queries.iter().chain(&args.acceptance).map(|p| fingerprint(p)).collect::<Result<Vec<_>>>()?
    });
    writeln!(out, "{manifest}")?;
    let mut observations = Vec::new();
    for case in &cases {
        let row = observe(args, &setup, &cfg, case)?;
        writeln!(out, "{row}")?;
        out.flush()?;
        observations.push(row);
    }
    let stable = before == corpus(args)?;
    let mut groups: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for row in &observations {
        let case = &row["case"];
        groups
            .entry(format!(
                "{}:{}",
                case["label_status"].as_str().unwrap(),
                case["category"].as_str().unwrap()
            ))
            .or_default()
            .push(row);
    }
    let summaries: BTreeMap<_, _> = groups.into_iter().map(|(key, rows)| {
        let mean = |field: &str| {
            let values: Vec<_> = rows.iter()
                .filter(|r| !r["case"]["relevant"].as_array().unwrap().is_empty())
                .filter_map(|r| r["score"][field].as_f64()).collect();
            if values.is_empty() { None } else { Some(values.iter().sum::<f64>() / values.len() as f64) }
        };
        let count = |field: &str| rows.iter().filter(|r| r["score"][field] == true).count();
        let families: HashSet<_> = rows.iter().map(|r| r["case"]["family"].as_str().unwrap()).collect();
        let family_success = families.iter().filter(|f| rows.iter().filter(|r| r["case"]["family"] == **f).all(|r| r["score"]["passed"] == true)).count();
        let recall_mean = |location: &str, depth: usize| {
            let values: Vec<_> = rows.iter().filter_map(|r| r[location][depth.to_string()].as_f64()).collect();
            if values.is_empty() { None } else { Some(values.iter().sum::<f64>() / values.len() as f64) }
        };
        let candidate_recall: BTreeMap<_,_> = [10,50,100].into_iter().map(|depth| (depth,recall_mean("candidate_recall",depth))).collect();
        let serialized_recall: BTreeMap<_,_> = [10,50,100].into_iter().map(|depth| {
            let values: Vec<_> = rows.iter().filter_map(|r| r["score"]["recall"][depth.to_string()].as_f64()).collect();
            (depth,if values.is_empty() { None } else { Some(values.iter().sum::<f64>() / values.len() as f64) })
        }).collect();
        (key,json!({"queries":rows.len(),"retrieval_queries":rows.iter().filter(|r| !r["case"]["relevant"].as_array().unwrap().is_empty()).count(),
            "candidate_recall":candidate_recall,"serialized_recall":serialized_recall,"families":families.len(),"families_all_passed":family_success,
            "passed":count("passed"),"top1":count("top1"),"top3":count("top3"),"mrr":mean("mrr"),"ndcg10":mean("ndcg10"),
            "wrong_domain":count("wrong_domain"),"wrong_brand_top3":count("wrong_brand_top3")}))
    }).collect();
    let mut latencies: Vec<_> = observations
        .iter()
        .filter_map(|r| r["latency_ms"].as_f64())
        .collect();
    latencies.sort_by(f64::total_cmp);
    let percentile =
        |p: f64| latencies[((latencies.len() as f64 * p).ceil() as usize).saturating_sub(1)];
    let summary = json!({"type":"summary","corpus_unchanged":stable,"groups":summaries,
        "latency_ms":{"median":percentile(0.5),"p95":percentile(0.95),"excludes_stage_diagnostics":true},
        "rss_bytes":null,"rss_measurement":"use /usr/bin/time peak RSS; steady-state server RSS measured separately",
        "response_bytes":observations.iter().filter_map(|r|r["response_bytes"].as_u64()).sum::<u64>(),
        "release_gate":"only manual label groups; candidate and legacy observations do not establish independent accuracy"});
    writeln!(out, "{summary}")?;
    out.flush()?;
    println!("{summary}");
    if !stable {
        bail!("corpus changed during evaluation; comparison invalid");
    }
    if observations
        .iter()
        .any(|r| r["case"]["label_status"] == "manual" && r["score"]["passed"] == false)
    {
        bail!("manually reviewed contract failed; see report");
    }
    if let Some(min) = args.min_top1 {
        for (name, group) in &summaries {
            let count = group["retrieval_queries"].as_u64().unwrap_or(0);
            let top1 = group["top1"].as_u64().unwrap_or(0);
            if count > 0 && ratio(top1 as usize, count as usize) < min {
                bail!("{name} is below --min-top1 {min}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn case() -> Case {
        serde_json::from_value(json!({"id":"x","family":"one family","category":"navigation","query":"alpha","label_status":"candidate","relevant":[{"identity":"alpha.org","grade":3}],"negatives":[{"identity":"wrong.org","max_rank":3,"reason":"wrong_brand"}]})).unwrap()
    }
    fn row(key: &str) -> Row {
        Row {
            identities: vec![key.into()],
            kind: "site".into(),
            language: None,
            country: None,
        }
    }
    #[test]
    fn relevant_answer_does_not_excuse_wrong_brand() {
        let scored = score(&case(), &[row("wrong.org"), row("alpha.org")], &None, 100);
        assert_eq!(scored.rank, Some(2));
        assert!(scored.wrong_brand_top3);
        assert!(!scored.passed);
        assert!((scored.ndcg10.unwrap() - 1. / 3f64.log2()).abs() < 1e-9);
    }
    #[test]
    fn missing_and_abstaining_are_distinct() {
        assert!(!score(&case(), &[], &None, 100).passed);
        let mut c = case();
        c.relevant.clear();
        c.expect.abstain = true;
        assert!(score(&c, &[], &None, 100).passed);
        assert!(!score(&c, &[row("alpha.org")], &None, 100).passed);
    }
    #[test]
    fn unknown_metadata_and_query_dates_cannot_satisfy_contracts() {
        let mut c = case();
        c.query = "2024 english".into();
        c.expect.language = Some("en".into());
        c.expect.date_contains = vec!["2024".into()];
        assert_eq!(
            score(&c, &[row("alpha.org")], &None, 100).violations.len(),
            2
        );
    }
    #[test]
    fn boundary_matches_and_family_leakage_are_checked() {
        assert!(matches("alpha.org", "https://api.alpha.org/a"));
        assert!(!matches("alpha.org", "https://alpha.org.attacker.test/a"));
        let mut a = case();
        a.split = Some("tune".into());
        let mut b = a.clone();
        b.id = "y".into();
        b.query = "paraphrase".into();
        b.split = Some("held-out".into());
        assert!(parse_cases(&format!(
            "{}\n{}",
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        ))
        .is_err());
        assert_eq!(family_half(" One   Family "), family_half("one family"));
    }
    #[test]
    fn fixed_snapshot_is_deterministic_and_attributes_filter_loss() {
        use clap::Parser;
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("sites");
        let mut alpha = plumb_core::SiteRecord::new("alpha.org");
        alpha.title = Some("Alpha".into());
        alpha.aliases.push("Alpha".into());
        plumb_index::build_index(&index, &[alpha]).unwrap();
        let pages = dir.path().join("wikipedia-fr.tsv");
        let mut file = File::create(&pages).unwrap();
        plumb_core::article::write_article(
            &mut file,
            &plumb_core::Article {
                title: "Alpha".into(),
                views: 100,
                ..Default::default()
            },
        )
        .unwrap();
        let suite = dir.path().join("contracts.jsonl");
        let mut filtered = case();
        filtered.id = "filter".into();
        filtered.relevant = vec![Relevant {
            identity: "https://fr.wikipedia.org/wiki/Alpha".into(),
            grade: 3,
        }];
        filtered.options.language = Some("en".into());
        let mut missing = case();
        missing.id = "missing".into();
        missing.relevant = vec![Relevant {
            identity: "missing.invalid".into(),
            grade: 3,
        }];
        fs::write(
            &suite,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&filtered).unwrap(),
                serde_json::to_string(&missing).unwrap()
            ),
        )
        .unwrap();
        let report = dir.path().join("report.jsonl");
        let cli = crate::cli::Cli::try_parse_from([
            "plumb",
            "eval",
            "--index",
            index.to_str().unwrap(),
            "--pages",
            pages.to_str().unwrap(),
            "--acceptance",
            suite.to_str().unwrap(),
            "--report",
            report.to_str().unwrap(),
            "--eval-time",
            "1791586800",
            "--limit",
            "100",
        ])
        .unwrap();
        let crate::cli::Command::Eval(args) = cli.command else {
            panic!("not eval")
        };
        run(&args).unwrap();
        let read = || {
            fs::read_to_string(&report)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str::<Value>(l).unwrap())
                .filter(|r| r["type"] == "query")
                .map(|mut r| {
                    r.as_object_mut().unwrap().remove("latency_ms");
                    r
                })
                .collect::<Vec<_>>()
        };
        let before = read();
        run(&args).unwrap();
        assert_eq!(before, read());
        assert_eq!(before[0]["stages"]["loss"], "source_filter");
        assert_eq!(before[1]["stages"]["loss"], "record_missing");
    }
    #[test]
    fn repository_contracts_have_separate_families() {
        let audit = parse_cases(include_str!("../../../../eval/contracts/audit.jsonl")).unwrap();
        let heldout = parse_cases(include_str!(
            "../../../../eval/contracts/family_heldout.jsonl"
        ))
        .unwrap();
        assert_eq!(heldout.len(), 200);
        assert_eq!(
            heldout
                .iter()
                .map(|c| &c.family)
                .collect::<HashSet<_>>()
                .len(),
            40
        );
        assert!(heldout
            .iter()
            .all(|c| c.half() == Half::HeldOut && !audit.iter().any(|a| a.family == c.family)));
    }
    #[test]
    fn self_grades_never_become_manual_labels() {
        let mut c = case();
        c.label_status = "manual".into();
        c.self_grade = json!("yes");
        assert!(c.validate().is_err());
        c.reviewer = Some("reviewer".into());
        c.evidence = json!({"source":"full-response"});
        assert!(c.validate().is_ok());
    }
}
