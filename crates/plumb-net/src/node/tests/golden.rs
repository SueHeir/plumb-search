//! What a node sends other nodes of its sites, and what it takes in of
//! theirs, checked against `golden.txt` beside this file: crawl batches as
//! signed and carried (`/plumb/batch/2`), what a node keeps of one from a
//! crawler it does not trust, from one it trusts and from itself, what two
//! crawlers agreeing confirm, and the proofs of single crawls; a bucket
//! answer (`/plumb/bucket/2`), the same sealed (`/plumb/oblivious/2`), and
//! the sites a search takes from it; and fill answers (`/plumb/fill/1`)
//! with what a node keeps of them. A message is written as the JSON of
//! what goes on the wire and the SHA-256 of its CBOR, and a site a node
//! takes in as the stored line the inbox gets of it.
//!
//! Written on records, before the network moved onto sites' items (step 9
//! of the data format plan), so that the network on items is checked
//! against it byte for byte. Never written again to make a test pass:
//! `PLUMB_WRITE_GOLDEN=1 cargo test -p plumb-net --lib golden` writes it.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Mutex;

use libp2p::identity::Keypair;
use libp2p::PeerId;
use plumb_core::site::{self, Item};
use plumb_core::sites::{SiteLine, SiteList};
use plumb_core::{linker_bit, Headline, KeyPage, LinkText, Redirect};
use sha2::{Digest, Sha256};

use super::answering::lookup;
use super::batches::{batch_chunks, by_crawl_epoch, delivery_bytes};
use super::keep_trusted_crawls;
use crate::agree::Agreement;
use crate::assign::{epoch_of, is_assigned, EPOCH_SECS, MAX_SHARE_PPM};
use crate::batch::{
    accept_batch, accept_news, accept_own_batch, accept_trusted_batch, mostly_kept, Batch,
};
use crate::bucket::BucketSource;
use crate::oblivious::{open_response, pad, seal_request, seal_response, Gateway};
use crate::proto::{BatchResponse, BucketRequest, BucketResponse, FillResponse};
use crate::search::{check_answer, FoundSite, NetSearch};
use crate::store::BatchStore;

const NOW: u64 = 1_792_000_000;

fn key(seed: u8) -> Keypair {
    Keypair::ed25519_from_bytes([seed; 32]).unwrap()
}

/// The stored line a node writes of a site it takes in, as the inbox
/// holds it.
fn item_line(site: &Item) -> String {
    String::from_utf8(plumb_core::stored::item_as_line(site).unwrap()).unwrap()
}

/// The stored lines of the sites a list holds, as the inbox gets them.
fn lines(sites: &SiteList) -> Vec<String> {
    let line = |line: &[u8]| String::from_utf8(line.to_vec()).unwrap();
    sites.lines().map(line).collect()
}

/// `sites` as the node holds them to publish.
fn list(sites: &[Item]) -> SiteList {
    sites.iter().cloned().collect()
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A message as CBOR, as the request-response protocols carry it.
fn cbor(message: &impl serde::Serialize) -> Vec<u8> {
    cbor4ii::serde::to_vec(Vec::new(), message).unwrap()
}

/// The first domains `golden{i}.com` that `assigned` takes.
fn domains(n: usize, assigned: impl Fn(&str) -> bool) -> Vec<String> {
    (0..)
        .map(|i| format!("golden{i}.com"))
        .filter(|domain| assigned(domain))
        .take(n)
        .collect()
}

fn crawl(domain: &str, title: &str) -> Item {
    let mut record = site::new(domain);
    record.url = Some(format!("https://www.{domain}/"));
    record.title = Some(title.into());
    record.about = Some(format!("All about {domain}."));
    site::set_crawled(&mut record, Some(NOW - 600));
    record
}

/// The sites of the golden crawlers, `a` and `b`.
struct Sites {
    /// Assigned to both crawlers.
    both: Vec<String>,
    /// Assigned to `a` only.
    only_a: Vec<String>,
    /// Assigned to neither.
    neither: String,
}

impl Sites {
    fn of(a: &PeerId, b: &PeerId) -> Sites {
        let epoch = epoch_of(NOW);
        let assigned =
            |peer: &PeerId, domain: &str| is_assigned(epoch, peer, domain, MAX_SHARE_PPM);
        Sites {
            both: domains(2, |d| assigned(a, d) && assigned(b, d)),
            only_a: domains(3, |d| assigned(a, d) && !assigned(b, d)),
            neither: domains(1, |d| !assigned(a, d) && !assigned(b, d)).remove(0),
        }
    }

    /// What crawler `a` crawled: a rich homepage, as a crawl leaves it
    /// (untidy text, page text longer than an item carries, a key page at
    /// a URL that is not canonical and one on another site, links to a
    /// host no item can name and to itself, link texts some of whose
    /// linkers the batch did not crawl, ranks and crawl marks, an icon and
    /// headlines); a plain one with an icon; one whose homepage URL is on
    /// another site; one that went elsewhere; one crawled long ago; one it
    /// was not assigned; a site it found links to; a feed check; and a
    /// domain that is not canonical.
    fn crawled_by_a(&self) -> Vec<Item> {
        let [a1, a2] = [&self.both[0], &self.both[1]];
        let mut rich = crawl(a1, " A1  Savings\u{7} Bank ");
        rich.about = Some("Du\u{323} ba\u{301}o for savers".into());
        rich.names = vec!["A1 Bank".into(), "A One".into(), "A1 Bank".into()];
        rich.lang = Some("en-US".into());
        site::set_headings(&mut rich, vec!["Open an account".into(), "Rates ".into()]);
        rich.text = Some("Save more with A1. ".repeat(80));
        site::set_terms(&mut rich, vec!["savings".into(), "bank".into()]);
        site::set_key_pages(
            &mut rich,
            vec![
                KeyPage {
                    label: "Rates".into(),
                    url: format!("https://www.{a1}/rates"),
                },
                KeyPage {
                    label: " Help ".into(),
                    url: format!("HTTPS://WWW.{}/help/#top", a1.to_uppercase()),
                },
                KeyPage {
                    label: "Elsewhere".into(),
                    url: "https://other.org/a1".into(),
                },
            ],
        );
        site::set_links_to(
            &mut rich,
            vec![
                a2.clone(),
                "foo_bar.com".into(),
                "linked.org".into(),
                a1.clone(),
            ],
        );
        site::set_link_texts(
            &mut rich,
            &[
                LinkText::from_linkers("a1 bank", linker_bit(a2)),
                LinkText::from_linkers("savings", linker_bit("elsewhere.org")),
                LinkText::with_count("bank", 3),
            ],
        );
        let mut signals = site::signals(&rich);
        signals.linking_domains = 5;
        signals.tranco_rank = Some(10);
        signals.official_site = true;
        site::set_signals(&mut rich, &signals);
        site::set_search_url(
            &mut rich,
            Some(format!("https://www.{a1}/search?q={{searchTerms}}")),
        );
        site::set_icon(&mut rich, Some("iVBORw0KGgo=".into()));
        site::set_news(
            &mut rich,
            vec![Headline {
                title: "Rates go up".into(),
                url: format!("https://www.{a1}/news/rates"),
                at: NOW - 1_200,
            }],
        );
        let mut marks = site::crawl_marks(&rich);
        marks.tried = Some(NOW - 600);
        marks.failures = 2;
        site::set_crawl_marks(&mut rich, marks);
        site::set_crawl_version(&mut rich, 7);

        let mut plain = crawl(a2, "A2 Credit Union");
        site::set_link_texts(
            &mut plain,
            &[LinkText::from_linkers("credit union", linker_bit(a1))],
        );
        let mut signals = site::signals(&plain);
        signals.linking_domains = 1;
        site::set_signals(&mut plain, &signals);
        site::set_icon(&mut plain, Some("R0lGODlh".into()));

        let mut elsewhere = crawl(&self.only_a[0], "X1 Shop");
        elsewhere.url = Some("https://elsewhere.org/x1".into());
        let mut redirected = crawl(&self.only_a[1], "R1");
        site::set_redirect(
            &mut redirected,
            Some(Redirect {
                to: "elsewhere.org".into(),
                at: NOW - 600,
                inner: false,
            }),
        );
        let mut stale = crawl(&self.only_a[2], "Stale");
        site::set_crawled(&mut stale, Some(NOW - 10 * EPOCH_SECS));
        let unassigned = crawl(&self.neither, "N1 Not Assigned");

        let mut linked = site::new("linked.org");
        site::set_link_texts(
            &mut linked,
            &[
                LinkText::from_linkers("linked", linker_bit(a1) | linker_bit(a2)),
                LinkText::from_linkers("forged", linker_bit("elsewhere.org")),
            ],
        );
        let mut signals = site::signals(&linked);
        signals.linking_domains = 9;
        site::set_signals(&mut linked, &signals);

        let mut feed = site::new("news.example");
        let headline = |url: &str, at: u64| Headline {
            title: "Big news".into(),
            url: url.into(),
            at,
        };
        site::set_news(
            &mut feed,
            vec![
                headline("https://www.news.example/big", NOW - 600),
                headline("https://elsewhere.org/x", NOW - 600),
                headline("https://www.news.example/old", NOW - 30 * EPOCH_SECS),
            ],
        );
        let uncanonical = crawl("WWW.Upper.COM", "Upper");

        vec![
            rich,
            plain,
            elsewhere,
            redirected,
            stale,
            unassigned,
            linked,
            feed,
            uncanonical,
        ]
    }

    /// What crawler `b` crawled of the same sites: the same homepages, by
    /// its own words, and the same links.
    fn crawled_by_b(&self) -> Vec<Item> {
        let [a1, a2] = [&self.both[0], &self.both[1]];
        let mut rich = crawl(a1, "A1 Savings Bank");
        rich.about = Some("D\u{1ef1} b\u{e1}o for savers".into());
        rich.names = vec!["A1 Bank".into(), "Savings A1".into()];
        site::set_link_texts(
            &mut rich,
            &[LinkText::from_linkers("A1 Bank", linker_bit(a2))],
        );
        let mut signals = site::signals(&rich);
        signals.linking_domains = 4;
        site::set_signals(&mut rich, &signals);
        site::set_crawled(&mut rich, Some(NOW - 300));
        let mut plain = crawl(a2, "A2 Credit Union");
        site::set_link_texts(
            &mut plain,
            &[LinkText::from_linkers("credit union", linker_bit(a1))],
        );
        let mut signals = site::signals(&plain);
        signals.linking_domains = 1;
        site::set_signals(&mut plain, &signals);
        let mut linked = site::new("linked.org");
        site::set_link_texts(
            &mut linked,
            &[LinkText::from_linkers(
                "linked",
                linker_bit(a1) | linker_bit(a2),
            )],
        );
        let mut signals = site::signals(&linked);
        signals.linking_domains = 2;
        site::set_signals(&mut linked, &signals);
        vec![rich, plain, linked]
    }

    /// Sites as the node answering holds them in its bucket table: clean,
    /// with what it knows of them besides a crawl, and some it never
    /// crawled or no longer can.
    fn held(&self, crawled: &[Item]) -> Vec<Item> {
        let mut held: Vec<Item> = crawled[..3].to_vec();
        let mut signals = site::signals(&held[0]);
        signals.harmonic_rank = Some(40);
        site::set_signals(&mut held[0], &signals);
        site::set_kinds(&mut held[0], vec!["bank".into()]);
        site::set_wikidata_about(&mut held[0], Some("a savings bank".into()));
        site::set_intro(&mut held[0], Some("A1 is a savings bank.".into()));
        site::set_redirect(
            &mut held[0],
            Some(Redirect {
                to: "foo_bar.com".into(),
                at: NOW - 6_000,
                inner: true,
            }),
        );
        let mut ranked = site::new("ranked.org");
        let mut signals = site::signals(&ranked);
        signals.tranco_rank = Some(900);
        site::set_signals(&mut ranked, &signals);
        ranked.names = vec!["Ranked".into()];
        let mut gone = site::new("gone.org");
        let mut signals = site::signals(&gone);
        signals.tranco_rank = Some(100);
        site::set_signals(&mut gone, &signals);
        site::set_crawled(&mut gone, Some(NOW - 400 * EPOCH_SECS));
        let mut marks = site::crawl_marks(&gone);
        marks.failures = 6;
        marks.gone = Some(NOW - 30 * EPOCH_SECS);
        site::set_crawl_marks(&mut gone, marks);
        let mut bot = crawl("bot.org", "Just a moment...");
        bot.url = Some("https://www.bot.org/".into());
        bot.about = None;
        held.extend([ranked, gone, bot]);
        held.iter_mut().for_each(site::clean);
        held
    }
}

fn sign(key: &Keypair, records: &[Item]) -> Batch {
    Batch::sign(key, &list(records), epoch_of(NOW), MAX_SHARE_PPM, NOW)
        .unwrap()
        .unwrap()
}

/// A node's bucket table of `lines`, with icons for some of its sites.
struct Table(Vec<String>);

impl BucketSource for Table {
    fn bucket(&self, _bucket: u32) -> Option<Vec<String>> {
        Some(self.0.clone())
    }

    fn ranked(&self, from: usize, count: usize) -> Option<(Vec<String>, usize)> {
        let end = from.saturating_add(count).min(self.0.len());
        let lines = self.0.get(from..end).unwrap_or_default();
        Some((lines.to_vec(), self.0.len()))
    }

    fn icon(&self, domain: &str) -> Option<String> {
        (domain.starts_with("golden") || domain == "gone.org").then(|| "cG5n".into())
    }
}

fn found_sites(text: &mut String, label: &str, found: &[FoundSite]) {
    let line = |site: &SiteLine| String::from_utf8(site.line().to_vec()).unwrap();
    for site in found {
        let domain = site.site.domain();
        writeln!(
            text,
            "{label} {domain} verified {} crawler {} answers {} crawlers {:?} confirmed {} trusted {} proof {}",
            site.verified,
            site.crawler.as_deref().unwrap_or("-"),
            site.answers,
            site.crawlers,
            site.confirmed,
            site.trusted,
            site.proof.is_some(),
        )
        .unwrap();
        writeln!(text, "{label} {domain} site {}", line(&site.site)).unwrap();
        match &site.shared {
            Some(shared) => writeln!(text, "{label} {domain} shared {}", line(shared)),
            None => writeln!(text, "{label} {domain} shared -"),
        }
        .unwrap();
        let keeps = site.keeps().map_or("-".into(), line);
        writeln!(text, "{label} {domain} keeps {keeps}").unwrap();
    }
}

#[test]
fn the_network_sends_and_takes_in_the_golden_bytes() {
    let (a, b, me) = (key(1), key(2), key(3));
    let (a_id, b_id, me_id) = (
        a.public().to_peer_id(),
        b.public().to_peer_id(),
        me.public().to_peer_id(),
    );
    let sites = Sites::of(&a_id, &b_id);
    let mut text = String::from(
        "# What the network sends of a node's sites and takes in of others':\n\
         # see golden.rs before writing it again. Written on records.\n",
    );
    writeln!(text, "crawlers a {a_id} b {b_id} me {me_id}").unwrap();

    // Crawl batches, as signed and carried.
    let by_a = sites.crawled_by_a();
    let by_b = sites.crawled_by_b();
    let batch_a = sign(&a, &by_a);
    let batch_b = sign(&b, &by_b);
    for (name, batch) in [("a", &batch_a), ("b", &batch_b)] {
        writeln!(text, "batch {name} id {}", batch.id()).unwrap();
        writeln!(
            text,
            "batch {name} {}",
            serde_json::to_string(batch).unwrap()
        )
        .unwrap();
        let carried = BatchResponse::Batch(Some(batch.clone()));
        writeln!(text, "batch {name} cbor {}", sha(&cbor(&carried))).unwrap();
        writeln!(
            text,
            "batch {name} header {}",
            serde_json::to_string(&batch.header).unwrap()
        )
        .unwrap();
    }

    // What a node keeps of a batch, by whose it is.
    let kept = [
        ("other", accept_batch(&batch_a, &a_id, NOW)),
        ("trusted", accept_trusted_batch(&batch_a, &a_id, NOW)),
        ("own", accept_own_batch(&batch_a, &a_id, NOW)),
        ("news", accept_news(&batch_a, NOW)),
        ("other b", accept_batch(&batch_b, &b_id, NOW)),
    ];
    for (name, sites) in &kept {
        for line in lines(sites) {
            writeln!(text, "accept {name} {line}").unwrap();
        }
    }
    writeln!(
        text,
        "mostly kept {} {}",
        mostly_kept(&batch_a, &kept[0].1),
        mostly_kept(&batch_b, &kept[4].1)
    )
    .unwrap();

    // What agreement confirms: two crawlers this node counts, then a
    // crawler this node trusts.
    let mut agreement = Agreement::new(me_id, []);
    for (name, crawler, sites) in [
        ("a", a_id, kept[0].1.clone()),
        ("b", b_id, kept[4].1.clone()),
    ] {
        let confirmed = agreement.observe(crawler, sites, NOW);
        for line in lines(&confirmed) {
            writeln!(text, "confirmed after {name} {line}").unwrap();
        }
        writeln!(
            text,
            "confirmed after {name} delivery bytes {}",
            delivery_bytes(&confirmed).unwrap()
        )
        .unwrap();
    }
    let mut trusting = Agreement::new(me_id, [a_id]);
    let confirmed = trusting.observe(a_id, kept[1].1.clone(), NOW);
    for line in lines(&confirmed) {
        writeln!(text, "confirmed trusted {line}").unwrap();
    }
    let mut delivered = confirmed;
    delivered.extend(kept[3].1.clone().into_lines());
    writeln!(
        text,
        "confirmed trusted delivery bytes {}",
        delivery_bytes(&delivered).unwrap()
    )
    .unwrap();

    // Single crawls, by their proofs.
    for i in 0..batch_a.records.len() {
        let proof = batch_a.proof(i);
        let verified = proof.verify(NOW).map(|(site, _)| item_line(&site));
        let trusted = proof.verify_trusted(NOW).map(|(site, _)| item_line(&site));
        for (name, got) in [("verify", verified), ("verify_trusted", trusted)] {
            let got = got.unwrap_or_else(|_| "refused".into());
            writeln!(text, "proof {i} {name} {got}").unwrap();
        }
    }

    // Publishing: by the epoch each homepage was crawled in, and in runs
    // that fit a batch.
    let mut published = by_a.clone();
    let mut yesterday = crawl(&sites.only_a[0], "Yesterday");
    site::set_crawled(&mut yesterday, Some(NOW - EPOCH_SECS));
    let mut future = crawl(&sites.only_a[1], "Future");
    site::set_crawled(&mut future, Some(NOW + 2 * EPOCH_SECS));
    published.extend([yesterday, future]);
    let assigned = |epoch, domain: &str| is_assigned(epoch, &a_id, domain, MAX_SHARE_PPM);
    for (epoch, sites) in by_crawl_epoch(list(&published), NOW, assigned) {
        let domains: Vec<String> = sites.domains().map(str::to_string).collect();
        let chunks: Vec<usize> = batch_chunks(sites).iter().map(SiteList::len).collect();
        writeln!(text, "publish epoch {epoch} {domains:?} chunks {chunks:?}").unwrap();
    }

    // A bucket answer, with the proofs of the crawls held.
    let held = sites.held(&by_a);
    let table = Table(held.iter().map(item_line).collect());
    let dir = tempfile::tempdir().unwrap();
    let store = {
        let mut store = BatchStore::open(dir.path()).unwrap();
        store.insert(&batch_a).unwrap();
        store.insert(&batch_b).unwrap();
        Mutex::new(store)
    };
    let answer = lookup(&table, &store, 0, NOW);
    writeln!(text, "bucket {}", serde_json::to_string(&answer).unwrap()).unwrap();
    writeln!(text, "bucket cbor {}", sha(&cbor(&answer))).unwrap();

    // The same answer sealed: what is sealed, and that it opens to it.
    let plain = pad(&cbor(&answer), crate::oblivious::MIN_RESPONSE_SIZE);
    writeln!(text, "sealed {} bytes {}", plain.len(), sha(&plain)).unwrap();
    let gateway = Gateway::new(&me, NOW).unwrap();
    let request = BucketRequest::new(0);
    let (sealed, opener) = seal_request(&gateway.keys(), &me_id, NOW, &request).unwrap();
    let (_, sealer) = gateway.open(&sealed).unwrap();
    let answered = seal_response(sealer, &answer).unwrap();
    assert_eq!(
        open_response::<BucketResponse>(opener, &answered).unwrap(),
        answer
    );

    // What a search takes from the answer, and from it with this node
    // trusting crawler `a`.
    let found = check_answer(answer.records.clone().unwrap(), NOW).unwrap();
    found_sites(&mut text, "found", &found);
    let mut search = NetSearch {
        found,
        ..NetSearch::default()
    };
    keep_trusted_crawls(&mut search, &[a_id], NOW);
    found_sites(&mut text, "found trusting a", &search.found);

    // Fill answers, of crawled sites and of every site, and what a node
    // keeps of them.
    for all in [false, true] {
        let answer = crate::fill::answer(&table, 0, 100, all);
        writeln!(
            text,
            "fill all {all} {}",
            serde_json::to_string(&answer).unwrap()
        )
        .unwrap();
        writeln!(text, "fill all {all} cbor {}", sha(&cbor(&answer))).unwrap();
        let accept = if all {
            crate::fill::accept_seed
        } else {
            crate::fill::accept_filled
        };
        for got in &answer.records {
            let got = accept(got, NOW).map_or("refused".into(), |site| item_line(&site));
            writeln!(text, "fill all {all} kept {got}").unwrap();
        }
    }
    let busy = FillResponse {
        records: Vec::new(),
        next: 7,
        total: 0,
        busy: true,
    };
    writeln!(text, "fill busy cbor {}", sha(&cbor(&busy))).unwrap();

    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/node/tests/golden.txt");
    check_golden(&golden, &text);
}

/// Compares `text` with the golden file at `path` line by line, or writes
/// it there when `PLUMB_WRITE_GOLDEN` is set.
fn check_golden(path: &Path, text: &str) {
    if std::env::var_os("PLUMB_WRITE_GOLDEN").is_some() {
        std::fs::write(path, text).unwrap();
        return;
    }
    let golden = std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
    let (mut want, mut got) = (golden.lines(), text.lines());
    for n in 1.. {
        match (want.next(), got.next()) {
            (None, None) => break,
            (want, got) if want == got => continue,
            (want, got) => panic!(
                "{}:{n} differs\n  golden: {}\n  now:    {}",
                path.display(),
                want.unwrap_or("(the end)"),
                got.unwrap_or("(the end)")
            ),
        }
    }
}
