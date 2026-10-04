//! Offline demonstrations for docs/reviews/privacy-security.md.
//! Run with `cargo run --locked -p plumb-net --example privacy_review`.
//! Uses synthetic searches and reports; makes no network requests.

use std::collections::BTreeSet;

use anyhow::Result;
use plumb_core::keys::{bucket_of, pick_buckets, query_keys, BUCKETS_PER_SEARCH};
use plumb_net::popularity::{report_epoch, tally, Report, REPORT_THRESHOLD};
use sta_rs::Message;

fn real_buckets(query: &str) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    for key in query_keys(query) {
        out.insert(bucket_of(&key));
        if out.len() == BUCKETS_PER_SEARCH {
            break;
        }
    }
    out
}

fn main() -> Result<()> {
    // Padding is known only to the simulated client. The dictionary check
    // receives just the observed four bucket numbers, not the padding seed.
    let query = "us bank";
    let mut state = 0x12_34_56_78_u64;
    let (observed, _) = pick_buckets(query, || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        state
    });
    let observed: BTreeSet<_> = observed.into_iter().collect();
    let mut dictionary: Vec<&str> = include_str!("../../../eval/brand_queries.tsv")
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split('\t').next())
        .filter(|line| *line != "query")
        .collect();
    dictionary.push(query);
    dictionary.sort_unstable();
    dictionary.dedup();
    let candidates: Vec<_> = dictionary
        .iter()
        .copied()
        .filter(|candidate| {
            let real = real_buckets(candidate);
            !real.is_empty() && real.is_subset(&observed)
        })
        .collect();
    assert!(candidates.contains(&query));
    println!("Observed bucket numbers: {observed:?}");
    println!(
        "Dictionary candidates: {} -> {}",
        dictionary.len(),
        candidates.len()
    );
    println!("Remaining candidates: {candidates:?}");

    let epoch = report_epoch(plumb_core::now_unix());
    let observed = Report::new(epoch, query, "usbank.com")?;
    let guessed = Report::new(epoch, query, "usbank.com")?;
    let actual = Message::from_bytes(&observed.message).expect("valid synthetic report");
    let guess = Message::from_bytes(&guessed.message).expect("valid guessed report");
    assert_eq!(actual.tag, guess.tag);
    assert_eq!(actual.ciphertext.to_bytes(), guess.ciphertext.to_bytes());
    println!("One observed popularity report matches the guessed query/domain tag and ciphertext.");

    // An attacker needs no independent people or crawler keys to generate
    // distinct shares accepted by the public report parser and tally.
    let reports: Vec<_> = (0..REPORT_THRESHOLD)
        .map(|_| Report::new(epoch, query, "usbank.com"))
        .collect::<Result<_>>()?;
    for report in &reports {
        report.check(plumb_core::now_unix())?;
    }
    let counted = tally(&reports, epoch);
    assert_eq!(counted.len(), 1);
    assert_eq!(counted[0].count, REPORT_THRESHOLD);
    println!(
        "One process generated {} accepted reports: {counted:?}",
        REPORT_THRESHOLD
    );

    let mut poisoned = plumb_core::SiteRecord::new("usbank.com");
    poisoned.url = Some("https://attacker.example/".into());
    poisoned.crawled_at = Some(plumb_core::now_unix());
    let accepted =
        plumb_net::fill::accept_filled(&serde_json::to_string(&poisoned)?, plumb_core::now_unix())
            .expect("the trusted-fill parser accepts this record");
    assert!(accepted.url.is_none());
    println!(
        "Trusted fill stripped the off-domain URL for {}.",
        accepted.domain
    );
    Ok(())
}
