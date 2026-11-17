//! `plumb-tools judge`: whether a change made search better, from two
//! runs of `plumb-tools eval --recall --outcomes-out` over the same
//! searches, one of the build without the change (base) and one with it
//! (head).
//!
//! An outcomes file holds, for each search of a queries file, whether an
//! answer is held at all and where it was listed. It holds no search and
//! no answer, so the outcomes of a sealed file can be kept and compared.
//!
//! For each queries file `judge` prints three counts (answers held, listed
//! in the first results, listed first) with the searches gained and lost,
//! and a verdict. The verdict reads one score, the reciprocal rank: 1 for
//! an answer listed first, 1/2 for second, 0 for one not listed. It is
//! "better" or "worse" when a change that does nothing would move that
//! score as far less than one time in twenty ([`chance`]), and "no
//! different" otherwise. So a change may lose some searches and still be
//! better, and one that moves two or three searches is no different,
//! however good those two or three look.

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// A verdict of better or worse is given when a change that does nothing
/// would move the score as far less often than this.
const LUCK: f64 = 0.05;
/// The most moved searches the chance is counted exactly for.
const EXACT_MOVES: usize = 20;

#[derive(Debug, clap::Args)]
pub struct JudgeArgs {
    /// Outcomes of the build without the change, written by `plumb-tools
    /// eval --recall --outcomes-out`.
    #[arg(long, value_name = "PATH")]
    pub base: PathBuf,
    /// Outcomes of the build with the change, of the same queries files.
    #[arg(long, value_name = "PATH")]
    pub head: PathBuf,
}

/// Whether one search's answer is held, and the rank it was listed at.
pub type Outcome = (bool, Option<usize>);
/// Whether an outcome counts in one of the three counts.
type Counts = fn(&Outcome) -> bool;

/// One queries file's outcomes in a run.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcomes {
    /// The file's name, without its folder and extension.
    pub suite: String,
    /// How many results each search fetched.
    pub limit: usize,
    /// What the searches and their answers hash to, so that only runs of
    /// the same searches are compared.
    pub digest: String,
    pub searches: Vec<Outcome>,
}

/// As an outcomes file holds it: `== suite<TAB>limit<TAB>digest`, then
/// one `held<TAB>rank` line per search, 0 for a rank not listed.
impl fmt::Display for Outcomes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "== {}\t{}\t{}", self.suite, self.limit, self.digest)?;
        for (held, rank) in &self.searches {
            writeln!(f, "{}\t{}", u8::from(*held), rank.unwrap_or(0))?;
        }
        Ok(())
    }
}

/// The outcomes an outcomes file holds, one for each queries file.
pub fn parse(text: &str) -> Result<Vec<Outcomes>> {
    let mut all: Vec<Outcomes> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let number = |field: &str| {
            field
                .parse::<usize>()
                .with_context(|| format!("line {}: {field:?} is not a number", i + 1))
        };
        if let Some(file) = line.strip_prefix("== ") {
            let [suite, limit, digest] = file.split('\t').collect::<Vec<_>>()[..] else {
                bail!("line {}: not the start of a queries file", i + 1);
            };
            all.push(Outcomes {
                suite: suite.to_string(),
                limit: number(limit)?,
                digest: digest.to_string(),
                searches: Vec::new(),
            });
            continue;
        }
        let (Some((held, rank)), Some(outcomes)) = (line.split_once('\t'), all.last_mut()) else {
            bail!("line {}: not a search of a queries file", i + 1);
        };
        let rank = number(rank)?;
        outcomes
            .searches
            .push((number(held)? == 1, (rank > 0).then_some(rank)));
    }
    Ok(all)
}

pub fn run(args: &JudgeArgs) -> Result<()> {
    let read = |path: &Path| {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        parse(&text).with_context(|| format!("parsing {}", path.display()))
    };
    let (base, head) = (read(&args.base)?, read(&args.head)?);
    if base.len() != head.len() {
        bail!(
            "base has {} queries files and head {}",
            base.len(),
            head.len()
        );
    }
    for (base, head) in base.iter().zip(&head) {
        print!("{}", compare(base, head)?);
    }
    Ok(())
}

/// The counts and the verdict of one queries file.
fn compare(base: &Outcomes, head: &Outcomes) -> Result<String> {
    let same = |run: &'_ Outcomes| (run.limit, run.searches.len());
    if (&base.suite, &base.digest, same(base)) != (&head.suite, &head.digest, same(head)) {
        bail!(
            "{} and {} are not runs of the same searches with the same --limit",
            base.suite,
            head.suite
        );
    }
    let limit = base.limit;
    let pairs = || base.searches.iter().zip(&head.searches);
    let mut out = String::new();
    let _ = writeln!(out, "== {} ({} searches)", base.suite, base.searches.len());
    let _ = writeln!(
        out,
        "  {:<18}{:>6}{:>7}{:>8}{:>6}{:>6}",
        "", "base", "head", "gained", "lost", "net"
    );
    let listed = format!("in the first {limit}");
    let counts: [(&str, Counts); 3] = [
        ("held", |s| s.0),
        (listed.as_str(), |s| s.1.is_some()),
        ("first", |s| s.1 == Some(1)),
    ];
    for (name, hit) in counts {
        let count = |run: &Outcomes| run.searches.iter().filter(|s| hit(s)).count();
        let gained = pairs().filter(|(b, h)| !hit(b) && hit(h)).count();
        let lost = pairs().filter(|(b, h)| hit(b) && !hit(h)).count();
        let _ = writeln!(
            out,
            "  {name:<18}{:>6}{:>7}{gained:>8}{lost:>6}{:>+6}",
            count(base),
            count(head),
            gained as i64 - lost as i64
        );
    }
    let score = |s: &Outcome| s.1.map_or(0.0, |rank| 1.0 / rank as f64);
    let mean = |run: &Outcomes| {
        run.searches.iter().map(score).sum::<f64>() / run.searches.len().max(1) as f64
    };
    let moves: Vec<f64> = pairs()
        .map(|(b, h)| score(h) - score(b))
        .filter(|moved| *moved != 0.0)
        .collect();
    let _ = writeln!(
        out,
        "  score (MRR@{limit}) {:.3} -> {:.3}: {} searches moved, {} up and {} down",
        mean(base),
        mean(head),
        moves.len(),
        moves.iter().filter(|moved| **moved > 0.0).count(),
        moves.iter().filter(|moved| **moved < 0.0).count()
    );
    let (total, luck) = (moves.iter().sum::<f64>(), chance(&moves));
    let verdict = if base.searches == head.searches {
        "identical".to_string()
    } else if total == 0.0 || luck >= LUCK {
        "no different".to_string()
    } else {
        format!(
            "{} (a change that does nothing moves the score this far {:.1}% of the time)",
            if total > 0.0 { "better" } else { "worse" },
            luck * 100.0
        )
    };
    let _ = writeln!(out, "  verdict: {verdict}");
    Ok(out)
}

/// How often a change that does nothing would move the score at least as
/// far as `moves` (each moved search's change in reciprocal rank) add up
/// to, in the direction they went: a sign-flip test. Such a change is as
/// likely to move a search up as down, so every pattern of signs is as
/// likely as the one seen. Up to [`EXACT_MOVES`] moves every pattern is
/// counted; beyond that the sum is taken for normal.
fn chance(moves: &[f64]) -> f64 {
    let total = moves.iter().sum::<f64>().abs();
    if moves.len() <= EXACT_MOVES {
        let patterns = 1u32 << moves.len();
        let as_far = (0..patterns)
            .filter(|signs| {
                let sum: f64 = moves
                    .iter()
                    .enumerate()
                    .map(|(i, moved)| moved.abs() * if signs >> i & 1 == 1 { 1.0 } else { -1.0 })
                    .sum();
                sum >= total - 1e-9
            })
            .count();
        return as_far as f64 / f64::from(patterns);
    }
    let spread = moves.iter().map(|moved| moved * moved).sum::<f64>().sqrt();
    upper_tail(total / spread)
}

/// The share of a normal distribution more than `z` (0 or more) standard
/// deviations above its mean (Abramowitz and Stegun 7.1.26, right to
/// seven places).
fn upper_tail(z: f64) -> f64 {
    let x = z / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let series = t
        * (0.254_829_592
            + t * (-0.284_496_736
                + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    0.5 * series * (-x * x).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcomes(searches: &[Outcome]) -> Outcomes {
        Outcomes {
            suite: "fresh".into(),
            limit: 10,
            digest: "ab12".into(),
            searches: searches.to_vec(),
        }
    }

    #[test]
    fn an_outcomes_file_reads_back_as_written() {
        let one = outcomes(&[(true, Some(3)), (false, None), (true, None)]);
        let two = Outcomes {
            suite: "other".into(),
            ..outcomes(&[(true, Some(1))])
        };
        let text = format!("{one}{two}");
        assert_eq!(text.lines().nth(1), Some("1\t3"));
        assert_eq!(parse(&text).unwrap(), vec![one, two]);
        assert!(parse("1\t3\n").is_err(), "a search before any file");
        assert!(parse("== fresh\t10\tab12\n1\tthird\n").is_err());
    }

    #[test]
    fn a_change_that_gains_more_than_it_loses_is_better() {
        // Eight searches gain a listing and one loses first place for second.
        let mut base = vec![(true, None); 8];
        base.extend([(true, Some(1)), (false, None), (true, Some(2))]);
        let mut head = vec![(true, Some(2)); 8];
        head.extend([(true, Some(2)), (true, None), (true, Some(2))]);
        let report = compare(&outcomes(&base), &outcomes(&head)).unwrap();
        let line = |start: &str| {
            let line = report.lines().find(|l| l.trim_start().starts_with(start));
            line.unwrap_or_else(|| panic!("no {start} line in\n{report}"))
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(line("held"), "held 10 11 1 0 +1");
        assert_eq!(line("in the first 10"), "in the first 10 2 10 8 0 +8");
        assert_eq!(line("first"), "first 1 0 0 1 -1");
        assert!(line("score").ends_with("9 searches moved, 8 up and 1 down"));
        assert!(line("verdict").starts_with("verdict: better"), "{report}");
        // The same change the other way round is worse.
        let report = compare(&outcomes(&head), &outcomes(&base)).unwrap();
        assert!(report.contains("verdict: worse"), "{report}");
    }

    #[test]
    fn a_change_that_moves_few_searches_is_no_different() {
        let base = outcomes(&[(true, None), (true, None), (true, Some(4)), (false, None)]);
        // Three searches move up, which chance does one time in eight.
        let head = outcomes(&[
            (true, Some(1)),
            (true, Some(1)),
            (true, Some(1)),
            (false, None),
        ]);
        let report = compare(&base, &head).unwrap();
        assert!(report.contains("verdict: no different"), "{report}");
        // More is held and nothing is listed differently.
        let held = outcomes(&[(true, None), (true, None), (true, Some(4)), (true, None)]);
        let report = compare(&base, &held).unwrap();
        assert!(report.contains("verdict: no different"), "{report}");
        let report = compare(&base, &base).unwrap();
        assert!(report.contains("verdict: identical"), "{report}");
    }

    #[test]
    fn only_runs_of_the_same_searches_are_compared() {
        let base = outcomes(&[(true, None)]);
        let other = Outcomes {
            digest: "cd34".into(),
            ..base.clone()
        };
        assert!(compare(&base, &other).is_err());
        assert!(compare(&base, &outcomes(&[(true, None), (true, None)])).is_err());
    }

    #[test]
    fn chance_counts_the_sign_patterns_as_far_out() {
        assert_eq!(chance(&[]), 1.0);
        assert_eq!(chance(&[1.0]), 0.5);
        assert_eq!(chance(&[0.5, 0.5, 0.5, 0.5, 0.5]), 1.0 / 32.0);
        // Losses count as far as gains do.
        assert_eq!(chance(&[-0.5, -0.5, -0.5, -0.5, -0.5]), 1.0 / 32.0);
        // One large loss against three small gains: the large one decides,
        // so half the patterns are as far out.
        assert_eq!(chance(&[0.1, 0.1, 0.1, -1.0]), 0.5);
        // Past the exact count: 13 up and 12 down is nothing, 25 up is not.
        let mut mixed = vec![1.0; 13];
        mixed.extend([-1.0; 12]);
        assert!((chance(&mixed) - 0.42).abs() < 0.01);
        assert!(chance(&[1.0; 25]) < 1e-6);
        assert!((upper_tail(1.96) - 0.025).abs() < 1e-4);
    }
}
