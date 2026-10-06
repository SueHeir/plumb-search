//! `plumb link-rank`: a PageRank of the sites in a records file, from the
//! links their homepages make to other sites ([`SiteRecord::links_to`]).
//!
//! A site ranks high when sites that rank high link to it. This is a first
//! look at the graph our own crawls make, run offline on a copy of a node's
//! records: it changes nothing and nothing reads what it writes yet.
//!
//! It is made to fit a small node. Each domain is held once, with a few
//! numbers beside it; the links themselves go to a temporary file next to
//! the records and are read back once per round (4 bytes a link). A
//! million synthetic sites with 5 million links peaked at 194 MB.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::rc::Rc;

use anyhow::{Context, Result};
use plumb_core::{canonical_domain, SiteRecord};

use crate::cli::LinkRankArgs;
use crate::web::group_thousands;

/// Share of a site's rank that follows its links; the rest is spread over
/// every site evenly. Google's paper used 0.85.
const DAMPING: f64 = 0.85;

/// Rounds stop once ranks move less than this in all (sum of changes).
const CONVERGED: f64 = 1e-7;

/// The graph of a records file, links on disk.
pub(crate) struct Graph {
    names: Vec<Rc<str>>,
    ids: HashMap<Rc<str>, u32>,
    /// Whether each site has a record (others are only linked to).
    in_records: Vec<bool>,
    /// Each site's Tranco rank, 0 for none.
    tranco: Vec<u32>,
    /// The site each one stands for: itself, or the one its homepage
    /// redirects to.
    resolve: Vec<u32>,
    /// Records with at least one link to another site.
    sources: usize,
    links: u64,
    file: tempfile::NamedTempFile,
    writer: Option<BufWriter<File>>,
}

impl Graph {
    /// An empty graph whose links go to a temporary file in `dir`.
    pub(crate) fn new(dir: &Path) -> Result<Graph> {
        let file = tempfile::Builder::new()
            .prefix(".plumb-links-")
            .tempfile_in(dir)
            .with_context(|| format!("making a file in {}", dir.display()))?;
        let writer = Some(BufWriter::new(file.reopen()?));
        Ok(Graph {
            names: Vec::new(),
            ids: HashMap::new(),
            in_records: Vec::new(),
            tranco: Vec::new(),
            resolve: Vec::new(),
            sources: 0,
            links: 0,
            file,
            writer,
        })
    }

    fn id(&mut self, domain: &str) -> u32 {
        if let Some(&id) = self.ids.get(domain) {
            return id;
        }
        let id = u32::try_from(self.names.len()).expect("fewer than 4 billion sites");
        let name: Rc<str> = domain.into();
        self.names.push(name.clone());
        self.ids.insert(name, id);
        self.in_records.push(false);
        self.tranco.push(0);
        self.resolve.push(id);
        id
    }

    /// Adds one record: the site, where it redirects, and its links.
    pub(crate) fn add(&mut self, record: &SiteRecord) -> Result<()> {
        let Some(domain) = canonical_domain(&record.domain) else {
            return Ok(());
        };
        let src = self.id(&domain);
        self.in_records[src as usize] = true;
        self.tranco[src as usize] = record.signals.tranco_rank.unwrap_or(0);
        if let Some(to) = record
            .redirect
            .as_ref()
            .and_then(|r| canonical_domain(&r.to))
        {
            if to != domain {
                self.resolve[src as usize] = self.id(&to);
            }
        }
        let targets: Vec<u32> = record
            .links_to
            .iter()
            .filter(|to| **to != domain)
            .map(|to| self.id(to))
            .collect();
        if targets.is_empty() {
            return Ok(());
        }
        self.sources += 1;
        self.links += targets.len() as u64;
        let writer = self.writer.as_mut().expect("links still being added");
        writer.write_all(&src.to_le_bytes())?;
        writer.write_all(&(targets.len() as u32).to_le_bytes())?;
        for target in targets {
            writer.write_all(&target.to_le_bytes())?;
        }
        Ok(())
    }

    /// Follows redirects to their end (at most a few hops; a loop stays put).
    fn finish_redirects(&mut self) {
        let first = self.resolve.clone();
        for (id, resolved) in self.resolve.iter_mut().enumerate() {
            let mut to = first[id];
            for _ in 0..4 {
                let next = first[to as usize];
                if next == to {
                    break;
                }
                to = next;
            }
            *resolved = if first[to as usize] == to {
                to
            } else {
                id as u32
            };
        }
    }

    /// Calls `each` with every linking site and the sites it links to,
    /// redirects followed, links to itself and repeats left out.
    fn for_each_source(&self, mut each: impl FnMut(u32, &[u32])) -> Result<()> {
        let mut reader = BufReader::with_capacity(1 << 20, self.file.reopen()?);
        let mut word = [0u8; 4];
        let mut targets = Vec::new();
        loop {
            match reader.read_exact(&mut word) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(err) => return Err(err.into()),
            }
            let src = self.resolve[u32::from_le_bytes(word) as usize];
            reader.read_exact(&mut word)?;
            let n = u32::from_le_bytes(word);
            targets.clear();
            for _ in 0..n {
                reader.read_exact(&mut word)?;
                let to = self.resolve[u32::from_le_bytes(word) as usize];
                if to != src && !targets.contains(&to) {
                    targets.push(to);
                }
            }
            each(src, &targets);
        }
    }

    /// Ranks every site: PageRank with [`DAMPING`], the rank of sites
    /// that link nowhere spread evenly, for at most `rounds` rounds.
    pub(crate) fn rank(mut self, rounds: usize) -> Result<Ranked> {
        if let Some(mut writer) = self.writer.take() {
            writer.flush()?;
        }
        self.finish_redirects();
        let n = self.names.len();
        let mut in_links = vec![0u32; n];
        self.for_each_source(|_, targets| {
            for &to in targets {
                in_links[to as usize] += 1;
            }
        })?;
        let mut rank = vec![if n == 0 { 0.0 } else { 1.0 / n as f64 }; n];
        let mut next = vec![0.0f64; n];
        let mut done = 0;
        let mut change = 0.0;
        for round in 0..rounds {
            next.fill(0.0);
            let mut followed = 0.0;
            self.for_each_source(|src, targets| {
                if targets.is_empty() {
                    return;
                }
                let share = rank[src as usize] / targets.len() as f64;
                for &to in targets {
                    next[to as usize] += share;
                }
                followed += rank[src as usize];
            })?;
            // What did not follow a link (sites linking nowhere, and the
            // part every site gives up) is spread evenly.
            let even = (1.0 - DAMPING * followed) / n as f64;
            change = 0.0;
            for (old, new) in rank.iter_mut().zip(next.iter()) {
                let value = DAMPING * new + even;
                change += (value - *old).abs();
                *old = value;
            }
            done = round + 1;
            if change < CONVERGED {
                break;
            }
        }
        Ok(Ranked {
            graph: self,
            rank,
            in_links,
            rounds: done,
            change,
        })
    }
}

/// A ranked [`Graph`].
pub(crate) struct Ranked {
    graph: Graph,
    /// Each site's share of all rank (they sum to 1).
    rank: Vec<f64>,
    /// Distinct sites linking to each one, redirects followed.
    in_links: Vec<u32>,
    rounds: usize,
    change: f64,
}

impl Ranked {
    /// The sites that are themselves (no redirect), best first, ties by
    /// domain.
    pub(crate) fn order(&self) -> Vec<u32> {
        let g = &self.graph;
        let mut order: Vec<u32> = (0..g.names.len() as u32)
            .filter(|&id| g.resolve[id as usize] == id)
            .collect();
        order.sort_by(|&a, &b| {
            self.rank[b as usize]
                .total_cmp(&self.rank[a as usize])
                .then_with(|| g.names[a as usize].cmp(&g.names[b as usize]))
        });
        order
    }

    /// A site's rank as a multiple of the average (1.0).
    pub(crate) fn score(&self, id: u32) -> f64 {
        self.rank[id as usize] * self.graph.names.len() as f64
    }

    pub(crate) fn name(&self, id: u32) -> &str {
        &self.graph.names[id as usize]
    }
}

/// `plumb link-rank`.
pub fn run(args: &LinkRankArgs) -> Result<()> {
    let dir = match args.records.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    // A journal next to the file is folded into a copy, leaving the file
    // (and any node using it) alone.
    let copy = tempfile::Builder::new()
        .prefix(".plumb-link-rank-")
        .suffix(".jsonl")
        .tempfile_in(dir)
        .with_context(|| format!("making a file in {}", dir.display()))?
        .into_temp_path();
    let read_from = match crate::outline::outline_copy(&args.records, &copy)
        .with_context(|| format!("reading records {}", args.records.display()))?
    {
        Some((path, _)) => path.to_path_buf(),
        None => args.records.clone(),
    };
    let mut graph = Graph::new(dir)?;
    let mut records = 0usize;
    let mut failed = None;
    crate::outline::for_each_record(&read_from, |record| {
        records += 1;
        if failed.is_none() {
            if let Err(err) = graph.add(&record) {
                failed = Some(err);
            }
        }
    })?;
    drop(copy);
    if let Some(err) = failed {
        return Err(err);
    }
    let sources = graph.sources;
    let links = graph.links;
    let sites = graph.names.len();
    let ranked = graph.rank(args.rounds)?;
    let order = ranked.order();

    println!(
        "{} records, {} with links to other sites ({:.1}%), {} links",
        group_thousands(records as u64),
        group_thousands(sources as u64),
        100.0 * sources as f64 / records.max(1) as f64,
        group_thousands(links)
    );
    let unknown = ranked
        .graph
        .in_records
        .iter()
        .filter(|&&known| !known)
        .count();
    println!(
        "{} sites in the graph, {} of them only linked to (no record)",
        group_thousands(sites as u64),
        group_thousands(unknown as u64)
    );
    let linked = ranked.in_links.iter().filter(|&&n| n > 0).count();
    println!(
        "{} sites have at least one link in; ranked in {} rounds (last change {:.2e})",
        group_thousands(linked as u64),
        ranked.rounds,
        ranked.change
    );

    // How the best by links compare with Tranco, among sites with records.
    let known: Vec<u32> = order
        .iter()
        .copied()
        .filter(|&id| ranked.graph.in_records[id as usize])
        .collect();
    for top in [100usize, 1_000, 10_000] {
        if known.len() < top {
            break;
        }
        let mut tranco: Vec<u32> = known[..top]
            .iter()
            .map(|&id| ranked.graph.tranco[id as usize])
            .filter(|&rank| rank > 0)
            .collect();
        tranco.sort_unstable();
        let in_10k = tranco.iter().filter(|&&rank| rank <= 10_000).count();
        let median = tranco.get(tranco.len() / 2).copied().unwrap_or(0);
        println!(
            "best {top} by links: {} in Tranco (median rank {}), {} in its top 10,000",
            tranco.len(),
            group_thousands(u64::from(median)),
            in_10k
        );
    }

    println!("\nbest by links (score: 1.0 is average; links in):");
    for &id in order.iter().take(args.show) {
        println!(
            "  {:>9.1}  {:>7}  {}{}",
            ranked.score(id),
            ranked.in_links[id as usize],
            ranked.name(id),
            if ranked.graph.in_records[id as usize] {
                ""
            } else {
                "  (no record)"
            }
        );
    }
    let missing: Vec<u32> = order
        .iter()
        .copied()
        .filter(|&id| !ranked.graph.in_records[id as usize])
        .take(args.show)
        .collect();
    if !missing.is_empty() {
        println!("\nbest linked sites with no record yet:");
        for id in missing {
            println!(
                "  {:>9.1}  {:>7}  {}",
                ranked.score(id),
                ranked.in_links[id as usize],
                ranked.name(id)
            );
        }
    }

    if let Some(out) = &args.out {
        let mut file = BufWriter::new(
            File::create(out).with_context(|| format!("writing {}", out.display()))?,
        );
        writeln!(file, "domain\tscore\tlinks_in\thas_record\ttranco_rank")?;
        for &id in &order {
            let i = id as usize;
            writeln!(
                file,
                "{}\t{:.4}\t{}\t{}\t{}",
                ranked.name(id),
                ranked.score(id),
                ranked.in_links[i],
                u8::from(ranked.graph.in_records[i]),
                ranked.graph.tranco[i]
            )?;
        }
        file.flush()?;
        println!(
            "\nwrote {} sites to {}",
            group_thousands(order.len() as u64),
            out.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use plumb_core::Redirect;

    fn site(domain: &str, links: &[&str]) -> SiteRecord {
        let mut record = SiteRecord::new(domain);
        record.links_to = links.iter().map(|s| s.to_string()).collect();
        record
    }

    fn ranked(records: &[SiteRecord]) -> Ranked {
        let dir = tempfile::tempdir().unwrap();
        let mut graph = Graph::new(dir.path()).unwrap();
        for record in records {
            graph.add(record).unwrap();
        }
        graph.rank(100).unwrap()
    }

    fn names(ranked: &Ranked) -> Vec<String> {
        ranked
            .order()
            .into_iter()
            .map(|id| ranked.name(id).to_owned())
            .collect()
    }

    #[test]
    fn the_most_linked_site_ranks_first_and_ranks_sum_to_one() {
        let ranked = ranked(&[
            site("a.com", &["hub.org"]),
            site("b.com", &["hub.org", "a.com"]),
            site("c.com", &["hub.org"]),
            site("hub.org", &["a.com", "d.net"]),
        ]);
        let order = names(&ranked);
        assert_eq!(order[0], "hub.org");
        // Linked from hub.org and b.com, ahead of b.com and c.com, which
        // nobody links to.
        assert_eq!(order[1], "a.com");
        let sum: f64 = ranked.rank.iter().sum();
        assert!((sum - 1.0).abs() < 1e-9, "{sum}");
        let hub = ranked.graph.ids["hub.org"];
        assert_eq!(ranked.in_links[hub as usize], 3);
        // d.net has no record, only a link in.
        let d = ranked.graph.ids["d.net"];
        assert!(!ranked.graph.in_records[d as usize]);
    }

    #[test]
    fn links_to_a_redirecting_site_count_for_where_it_goes() {
        let mut old = site("pncbank.com", &[]);
        old.redirect = Some(Redirect {
            to: "pnc.com".into(),
            at: 1,
        });
        let ranked = ranked(&[
            old,
            site("a.com", &["pncbank.com"]),
            site("b.com", &["pnc.com", "pncbank.com"]),
            site("c.com", &["other.com"]),
        ]);
        let order = names(&ranked);
        assert_eq!(order[0], "pnc.com");
        assert!(!order.contains(&"pncbank.com".to_owned()));
        let pnc = ranked.graph.ids["pnc.com"];
        // b.com's two links are one once followed.
        assert_eq!(ranked.in_links[pnc as usize], 2);
    }

    #[test]
    fn a_redirect_loop_stays_put() {
        let mut a = site("a.com", &[]);
        a.redirect = Some(Redirect {
            to: "b.com".into(),
            at: 1,
        });
        let mut b = site("b.com", &[]);
        b.redirect = Some(Redirect {
            to: "a.com".into(),
            at: 1,
        });
        let ranked = ranked(&[a, b, site("c.com", &["a.com"])]);
        assert_eq!(names(&ranked).len(), 3);
    }

    #[test]
    fn an_empty_graph_ranks_nothing() {
        let ranked = ranked(&[]);
        assert!(ranked.order().is_empty());
    }
}
