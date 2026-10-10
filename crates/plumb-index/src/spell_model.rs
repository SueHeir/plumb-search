//! What the sites' own words teach about spelling, after Whitelaw et al,
//! "Using the Web for Language Independent Spellchecking and
//! Autocorrection" (2009), and Brants et al, "Large Language Models in
//! Machine Translation" (2007, Stupid Backoff).
//!
//! Built with the index ([`ModelBuilder`]) and kept next to it in
//! [`MODEL_FILE`], it holds two things:
//!
//! - **An error model**, `P(typed | meant)`. No list of misspellings is
//!   given: the index's own words are paired up, each word with a word one
//!   edit away found in at least [`MINED_RATIO`] times as many sites ("words
//!   are usually spelled as intended"), and what was changed is counted as
//!   substring rules of up to two letters (Brill and Moore): `k -> ck`,
//!   `ll -> l`, `ie -> ei`. A rule's probability is how often its letters
//!   were typed that way, among all the times they were meant. A typo then
//!   costs what its edits cost: a doubled letter dropped is cheap, a first
//!   letter changed is dear.
//! - **A word-pair model** over the sites' names, titles, link texts and
//!   descriptions: in how many sites each word and each pair of
//!   neighbouring words is found. Scored with Stupid Backoff, it says how
//!   likely a word is after another ("capital one" over "capitol one").
//!
//! Adult sites' words are left out of the word-pair model.
//!
//! A missing or unreadable file only means typos are corrected as before,
//! by edit distance and popularity, and nothing is completed.

use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use tantivy::schema::Field;
use tantivy::tokenizer::{TextAnalyzer, TokenStream};

/// The file in an index directory that holds the model.
pub const MODEL_FILE: &str = "spelling.bin";
const MAGIC: &[u8; 8] = b"PLUMBSP1";

/// A word is taken as a misspelling of a word one edit away found in this
/// many times as many sites. The paper's "conservative" ratio.
pub(crate) const MINED_RATIO: u64 = 10;
/// Least number of mined pairs a rule needs to be kept.
const MIN_RULE_PAIRS: u32 = 3;
/// Added to how often a rule's letters are meant, in sites, so a rule of
/// rare letters is not taken as likely from a few pairs.
const RULE_SMOOTHING: u64 = 10_000;
/// Only words this long or longer are paired up: shorter ones are one
/// edit from too many others.
const MIN_MINED_CHARS: usize = 4;
/// Longest word paired up or kept.
const MAX_WORD_CHARS: usize = 24;
/// An edit of a word's first letter is this much less likely than the
/// rules say: typos rarely start a word ("fedx" is fedex, not edx), and
/// the pairs mined include names that differ in their first letter.
const FIRST_LETTER_SHARE: f32 = 0.1;
/// Stupid Backoff's discount for a word seen without the word before it.
const BACKOFF: f64 = 0.4;
/// While building, the most word pairs and words held at once. Past
/// either, the rarest are dropped (lossy counting), so building a million
/// sites takes a bounded amount of memory.
const BUILD_PAIRS_CAP: usize = 4_000_000;
const BUILD_WORDS_CAP: usize = 1_500_000;
/// Words and word pairs found in fewer sites are left out of the file.
const MIN_SAVED_COUNT: u32 = 3;

/// Words in one string, so a million of them take little more memory than
/// their letters.
#[derive(Debug, Clone, Default, PartialEq)]
struct WordList {
    text: String,
    /// Where each word ends in `text`.
    ends: Vec<u32>,
}

impl WordList {
    fn len(&self) -> usize {
        self.ends.len()
    }

    fn get(&self, i: usize) -> &str {
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.text[start..self.ends[i] as usize]
    }

    fn push(&mut self, word: &str) {
        self.text.push_str(word);
        self.ends.push(self.text.len() as u32);
    }

    fn iter(&self) -> impl Iterator<Item = &str> {
        (0..self.len()).map(|i| self.get(i))
    }

    /// The index of `word`, for sorted words.
    fn find(&self, word: &str) -> Option<usize> {
        let at = self.first_not_below(word);
        (at < self.len() && self.get(at) == word).then_some(at)
    }

    /// The index of the first word not below `word`, for sorted words.
    fn first_not_below(&self, word: &str) -> usize {
        let (mut low, mut high) = (0, self.len());
        while low < high {
            let mid = (low + high) / 2;
            if self.get(mid) < word {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low
    }
}

/// Substring rules of the error model, (meant, typed), each with
/// `P(typed | meant)`.
type Rules = HashMap<(Box<str>, Box<str>), f32>;

/// The spelling model of an index; see the [module docs](self).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Model {
    /// Sites counted.
    sites: u64,
    /// Words, sorted, and in how many sites each is found.
    words: WordList,
    counts: Vec<u32>,
    /// Neighbouring word pairs, as indexes into `words`, sorted, and in how
    /// many sites each is found.
    pairs: Vec<(u32, u32)>,
    pair_counts: Vec<u32>,
    /// `P(typed | meant)` of each substring rule.
    rules: Rules,
    /// The probability of an edit no rule covers.
    unseen: f32,
}

impl Model {
    /// The model kept in `index_dir`, if one is there and readable.
    pub fn open(index_dir: &Path) -> Option<Model> {
        let path = index_dir.join(MODEL_FILE);
        let file = File::open(&path).ok()?;
        Model::read(&mut BufReader::new(file)).ok()
    }

    /// Sites counted.
    pub fn sites(&self) -> u64 {
        self.sites
    }

    /// How many words and word pairs it knows, and how many error rules.
    pub fn size(&self) -> (usize, usize, usize) {
        (self.words.len(), self.pairs.len(), self.rules.len())
    }

    fn id(&self, word: &str) -> Option<u32> {
        self.words.find(word).map(|i| i as u32)
    }

    /// In how many sites `word` is found (0 when fewer than were kept).
    pub fn count(&self, word: &str) -> u32 {
        self.id(word).map_or(0, |id| self.counts[id as usize])
    }

    fn pair_count(&self, first: u32, second: u32) -> u32 {
        self.pairs
            .binary_search(&(first, second))
            .map_or(0, |i| self.pair_counts[i])
    }

    /// The Stupid Backoff score of `word` after `before`, as a natural log:
    /// the share of sites with `before` that go on with `word`, or, when
    /// none do, [`BACKOFF`] times the share of sites with `word`.
    pub fn ln_score(&self, before: Option<&str>, word: &str) -> f64 {
        let id = self.id(word);
        if let (Some(before), Some(id)) = (before.and_then(|b| self.id(b)), id) {
            let pair = self.pair_count(before, id);
            if pair > 0 {
                return (f64::from(pair) / f64::from(self.counts[before as usize])).ln();
            }
        }
        let count = id.map_or(0.5, |id| f64::from(self.counts[id as usize]));
        let discount = if before.is_some() { BACKOFF } else { 1.0 };
        (discount * count / (self.sites.max(1) as f64)).ln()
    }

    /// The Stupid Backoff score of `words` in order, as a natural log.
    pub fn ln_score_all(&self, words: &[&str]) -> f64 {
        let mut before = None;
        let mut sum = 0.0;
        for word in words {
            sum += self.ln_score(before, word);
            before = Some(*word);
        }
        sum
    }

    /// `ln P(typed | meant)`: the cost of the edits that turn `meant` into
    /// `typed`, by the learned rules. 0 when they are the same.
    pub fn ln_channel(&self, typed: &str, meant: &str) -> f64 {
        edit_rules(meant, typed)
            .iter()
            .map(|forms| {
                // Typos rarely start a word.
                let first = forms
                    .iter()
                    .any(|(r, t)| r.starts_with('^') || t.starts_with('^'));
                let p = forms
                    .iter()
                    .filter_map(|rule| {
                        self.rules
                            .get(&(rule.0.as_str().into(), rule.1.as_str().into()))
                    })
                    .fold(self.unseen, |best, &p| best.max(p));
                let p = if first { p * FIRST_LETTER_SHARE } else { p };
                f64::from(p).ln()
            })
            .sum()
    }

    /// The learned rules, most likely first, as (meant, typed, probability).
    pub fn top_rules(&self, limit: usize) -> Vec<(String, String, f32)> {
        let mut rules: Vec<(String, String, f32)> = self
            .rules
            .iter()
            .map(|((r, t), p)| (r.to_string(), t.to_string(), *p))
            .collect();
        rules.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        rules.truncate(limit);
        rules
    }

    /// Writes the model to `path`.
    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let mut out = BufWriter::new(file);
        self.write(&mut out)?;
        out.flush()?;
        out.into_inner()
            .map_err(|err| err.into_error())?
            .sync_all()?;
        Ok(())
    }

    pub(crate) fn write(&self, out: &mut impl Write) -> Result<()> {
        out.write_all(MAGIC)?;
        out.write_all(&self.sites.to_le_bytes())?;
        out.write_all(&(self.words.len() as u32).to_le_bytes())?;
        for (word, count) in self.words.iter().zip(&self.counts) {
            write_str(out, word)?;
            out.write_all(&count.to_le_bytes())?;
        }
        out.write_all(&(self.pairs.len() as u32).to_le_bytes())?;
        for ((a, b), count) in self.pairs.iter().zip(&self.pair_counts) {
            out.write_all(&a.to_le_bytes())?;
            out.write_all(&b.to_le_bytes())?;
            out.write_all(&count.to_le_bytes())?;
        }
        let mut rules: Vec<_> = self.rules.iter().collect();
        rules.sort_by(|a, b| a.0.cmp(b.0));
        out.write_all(&(rules.len() as u32).to_le_bytes())?;
        for ((meant, typed), p) in rules {
            write_str(out, meant)?;
            write_str(out, typed)?;
            out.write_all(&p.to_le_bytes())?;
        }
        out.write_all(&self.unseen.to_le_bytes())?;
        Ok(())
    }

    fn read(input: &mut impl Read) -> Result<Model> {
        let mut magic = [0u8; 8];
        input.read_exact(&mut magic)?;
        if &magic != MAGIC {
            bail!("not a spelling model");
        }
        let sites = read_u64(input)?;
        let n = read_u32(input)? as usize;
        let mut words = WordList::default();
        let mut counts = Vec::with_capacity(n.min(1 << 24));
        for i in 0..n {
            let word = read_str(input)?;
            if i > 0 && words.get(i - 1) >= word.as_ref() {
                bail!("the words are not sorted");
            }
            if words.text.len() + word.len() > u32::MAX as usize {
                bail!("too many words");
            }
            words.push(&word);
            counts.push(read_u32(input)?);
        }
        let n = read_u32(input)? as usize;
        let mut pairs = Vec::with_capacity(n.min(1 << 24));
        let mut pair_counts = Vec::with_capacity(n.min(1 << 24));
        for _ in 0..n {
            let pair = (read_u32(input)?, read_u32(input)?);
            if pair.0 as usize >= words.len() || pair.1 as usize >= words.len() {
                bail!("a word pair names no word");
            }
            if pairs.last().is_some_and(|last| *last >= pair) {
                bail!("the word pairs are not sorted");
            }
            pairs.push(pair);
            pair_counts.push(read_u32(input)?);
        }
        let n = read_u32(input)? as usize;
        let mut rules = HashMap::with_capacity(n.min(1 << 20));
        for _ in 0..n {
            let meant = read_str(input)?;
            let typed = read_str(input)?;
            rules.insert((meant, typed), read_f32(input)?);
        }
        let unseen = read_f32(input)?;
        Ok(Model {
            sites,
            words,
            counts,
            pairs,
            pair_counts,
            rules,
            unseen,
        })
    }
}

fn write_str(out: &mut impl Write, text: &str) -> Result<()> {
    let len = u16::try_from(text.len()).context("a word too long to keep")?;
    out.write_all(&len.to_le_bytes())?;
    out.write_all(text.as_bytes())?;
    Ok(())
}

fn read_str(input: &mut impl Read) -> Result<Box<str>> {
    let mut len = [0u8; 2];
    input.read_exact(&mut len)?;
    let mut bytes = vec![0u8; usize::from(u16::from_le_bytes(len))];
    input.read_exact(&mut bytes)?;
    Ok(String::from_utf8(bytes)?.into_boxed_str())
}

fn read_u32(input: &mut impl Read) -> Result<u32> {
    let mut bytes = [0u8; 4];
    input.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(input: &mut impl Read) -> Result<u64> {
    let mut bytes = [0u8; 8];
    input.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_f32(input: &mut impl Read) -> Result<f32> {
    let mut bytes = [0u8; 4];
    input.read_exact(&mut bytes)?;
    Ok(f32::from_le_bytes(bytes))
}

/// Counts words and word pairs site by site while an index is built; see
/// [`ModelBuilder::finish`] for the rest.
pub(crate) struct ModelBuilder {
    analyzer: TextAnalyzer,
    sites: u64,
    ids: HashMap<Box<str>, u32>,
    words: Vec<Box<str>>,
    counts: Vec<u32>,
    pairs: HashMap<(u32, u32), u32>,
}

impl ModelBuilder {
    pub(crate) fn new(analyzer: TextAnalyzer) -> ModelBuilder {
        ModelBuilder {
            analyzer,
            sites: 0,
            ids: HashMap::new(),
            words: Vec::new(),
            counts: Vec::new(),
            pairs: HashMap::new(),
        }
    }

    /// Counts the words of one site's `texts` (its names, title, link
    /// texts...): each word and each pair of neighbouring words in the
    /// same text once per site.
    pub(crate) fn add<'a>(&mut self, texts: impl IntoIterator<Item = &'a str>) {
        self.sites += 1;
        let mut words: HashSet<u32> = HashSet::new();
        let mut pairs: HashSet<(u32, u32)> = HashSet::new();
        for text in texts {
            let mut before: Option<u32> = None;
            let mut stream = self.analyzer.token_stream(text);
            while let Some(token) = stream.next() {
                let word = token.text.as_str();
                if word.chars().count() > MAX_WORD_CHARS {
                    before = None;
                    continue;
                }
                let id = match self.ids.get(word) {
                    Some(&id) => id,
                    None => {
                        let id = self.words.len() as u32;
                        self.ids.insert(word.into(), id);
                        self.words.push(word.into());
                        self.counts.push(0);
                        id
                    }
                };
                words.insert(id);
                if let Some(before) = before {
                    pairs.insert((before, id));
                }
                before = Some(id);
            }
        }
        for id in words {
            self.counts[id as usize] += 1;
        }
        for pair in pairs {
            *self.pairs.entry(pair).or_insert(0) += 1;
        }
        if self.pairs.len() > BUILD_PAIRS_CAP || self.words.len() > BUILD_WORDS_CAP {
            // Drop what was seen once so far, and more only while that is
            // not enough to halve what is held: a pair seen in many sites
            // reaches two long before the next pruning.
            let mut least = 2;
            loop {
                self.prune(least);
                if self.pairs.len() <= BUILD_PAIRS_CAP / 2
                    && self.words.len() <= BUILD_WORDS_CAP / 2
                {
                    break;
                }
                least += 1;
            }
        }
    }

    /// Drops the words and pairs found in fewer than `least` sites,
    /// keeping every word a kept pair has.
    fn prune(&mut self, least: u32) {
        self.pairs.retain(|_, n| *n >= least);
        let mut keep = vec![false; self.words.len()];
        for (i, &n) in self.counts.iter().enumerate() {
            keep[i] = n >= least;
        }
        for &(a, b) in self.pairs.keys() {
            keep[a as usize] = true;
            keep[b as usize] = true;
        }
        let mut new_id = vec![u32::MAX; self.words.len()];
        let mut words = Vec::new();
        let mut counts = Vec::new();
        for (i, word) in std::mem::take(&mut self.words).into_iter().enumerate() {
            if keep[i] {
                new_id[i] = words.len() as u32;
                words.push(word);
                counts.push(self.counts[i]);
            }
        }
        self.ids = words
            .iter()
            .enumerate()
            .map(|(i, w)| (w.clone(), i as u32))
            .collect();
        self.pairs = std::mem::take(&mut self.pairs)
            .into_iter()
            .map(|((a, b), n)| ((new_id[a as usize], new_id[b as usize]), n))
            .collect();
        self.words = words;
        self.counts = counts;
    }

    /// The model: the words and pairs counted, plus the error model mined
    /// from `searcher`'s words in `fields` (see [`mine_rules`]).
    pub(crate) fn finish(
        mut self,
        searcher: &tantivy::Searcher,
        fields: &[Field],
    ) -> Result<Model> {
        self.prune(MIN_SAVED_COUNT);
        let (rules, unseen) = mine_rules(searcher, fields)?;
        let mut order: Vec<u32> = (0..self.words.len() as u32).collect();
        order.sort_by(|&a, &b| self.words[a as usize].cmp(&self.words[b as usize]));
        let mut new_id = vec![0u32; order.len()];
        for (i, &old) in order.iter().enumerate() {
            new_id[old as usize] = i as u32;
        }
        let mut words = WordList::default();
        for &old in &order {
            words.push(&self.words[old as usize]);
        }
        drop(std::mem::take(&mut self.words));
        drop(std::mem::take(&mut self.ids));
        let counts: Vec<u32> = order.iter().map(|&old| self.counts[old as usize]).collect();
        let mut pairs: Vec<((u32, u32), u32)> = self
            .pairs
            .into_iter()
            .map(|((a, b), n)| ((new_id[a as usize], new_id[b as usize]), n))
            .collect();
        pairs.sort_unstable();
        Ok(Model {
            sites: self.sites,
            words,
            counts,
            pair_counts: pairs.iter().map(|p| p.1).collect(),
            pairs: pairs.into_iter().map(|p| p.0).collect(),
            rules,
            unseen,
        })
    }
}

/// One edit turning a meant word into a typed one, by letter position in
/// the meant word.
#[derive(Debug, Clone, PartialEq)]
enum Edit {
    /// `meant[at]` typed as `typed`.
    Swap { at: usize, typed: char },
    /// `meant[at]` left out.
    Drop { at: usize },
    /// `typed` put in before `meant[at]`.
    Add { at: usize, typed: char },
    /// `meant[at]` and `meant[at + 1]` typed the other way round.
    Turn { at: usize },
}

/// The substring rules (meant, typed) of up to two letters `edit` can be
/// read as, in `marked`, the meant word with `^` and `$` around it:
/// `amtrak` -> `amtrack` is `a -> ac` or `k -> ck`.
fn rules(marked: &[char], edit: &Edit) -> Vec<(String, String)> {
    let m = marked;
    let s = |range: std::ops::Range<usize>| -> String { m[range].iter().collect() };
    match *edit {
        Edit::Swap { at, typed } => vec![
            (s(at..at + 1), typed.to_string()),
            (s(at - 1..at + 1), format!("{}{typed}", m[at - 1])),
            (s(at..at + 2), format!("{typed}{}", m[at + 1])),
        ],
        Edit::Drop { at } => vec![
            (s(at - 1..at + 1), s(at - 1..at)),
            (s(at..at + 2), s(at + 1..at + 2)),
        ],
        Edit::Add { at, typed } => vec![
            (s(at - 1..at), format!("{}{typed}", m[at - 1])),
            (s(at..at + 1), format!("{typed}{}", m[at])),
        ],
        Edit::Turn { at } => vec![(s(at..at + 2), format!("{}{}", m[at + 1], m[at]))],
    }
}

/// The rules of each edit of the cheapest way (optimal string alignment)
/// from `meant` to `typed`; see [`rules`].
fn edit_rules(meant: &str, typed: &str) -> Vec<Vec<(String, String)>> {
    let a: Vec<char> = meant.chars().collect();
    let b: Vec<char> = typed.chars().collect();
    let marked: Vec<char> = std::iter::once('^')
        .chain(a.iter().copied())
        .chain(std::iter::once('$'))
        .collect();
    osa_edits(&a, &b)
        .into_iter()
        .map(|edit| rules(&marked, &shift(edit)))
        .collect()
}

/// `edit` with its position moved past the leading `^`.
fn shift(edit: Edit) -> Edit {
    match edit {
        Edit::Swap { at, typed } => Edit::Swap { at: at + 1, typed },
        Edit::Drop { at } => Edit::Drop { at: at + 1 },
        Edit::Add { at, typed } => Edit::Add { at: at + 1, typed },
        Edit::Turn { at } => Edit::Turn { at: at + 1 },
    }
}

/// The edits of an optimal string alignment of `a` (meant) to `b`
/// (typed), by position in `a`.
fn osa_edits(a: &[char], b: &[char]) -> Vec<Edit> {
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a[i - 1] == b[j - 1] && d[i][j] == d[i - 1][j - 1] {
            i -= 1;
            j -= 1;
        } else if i > 1
            && j > 1
            && a[i - 1] == b[j - 2]
            && a[i - 2] == b[j - 1]
            && d[i][j] == d[i - 2][j - 2] + 1
        {
            out.push(Edit::Turn { at: i - 2 });
            i -= 2;
            j -= 2;
        } else if i > 0 && j > 0 && d[i][j] == d[i - 1][j - 1] + 1 {
            out.push(Edit::Swap {
                at: i - 1,
                typed: b[j - 1],
            });
            i -= 1;
            j -= 1;
        } else if j > 0 && d[i][j] == d[i][j - 1] + 1 {
            out.push(Edit::Add {
                at: i,
                typed: b[j - 1],
            });
            j -= 1;
        } else {
            out.push(Edit::Drop { at: i - 1 });
            i -= 1;
        }
    }
    out.reverse();
    out
}

/// The terms of `fields` in `searcher`, in order, each with the number of
/// sites it is found in summed over the fields (a site counts once per
/// field), passed to `each`.
fn for_each_term(
    searcher: &tantivy::Searcher,
    fields: &[Field],
    mut each: impl FnMut(&str, u64),
) -> Result<()> {
    let mut indexes = Vec::new();
    for segment in searcher.segment_readers() {
        for &field in fields {
            indexes.push(segment.inverted_index(field)?);
        }
    }
    let mut streams = Vec::with_capacity(indexes.len());
    for index in &indexes {
        streams.push(index.terms().stream()?);
    }
    // Smallest term first: (Reverse(term), stream).
    let mut heap: BinaryHeap<(std::cmp::Reverse<Vec<u8>>, usize)> = BinaryHeap::new();
    for (i, stream) in streams.iter_mut().enumerate() {
        if stream.advance() {
            heap.push((std::cmp::Reverse(stream.key().to_vec()), i));
        }
    }
    let mut current: Option<(Vec<u8>, u64)> = None;
    while let Some((std::cmp::Reverse(key), i)) = heap.pop() {
        let docs = u64::from(streams[i].value().doc_freq);
        match &mut current {
            Some((term, sum)) if *term == key => *sum += docs,
            _ => {
                if let Some((term, sum)) = current.take() {
                    if let Ok(term) = std::str::from_utf8(&term) {
                        each(term, sum);
                    }
                }
                current = Some((key, docs));
            }
        }
        if streams[i].advance() {
            heap.push((std::cmp::Reverse(streams[i].key().to_vec()), i));
        }
    }
    if let Some((term, sum)) = current {
        if let Ok(term) = std::str::from_utf8(&term) {
            each(term, sum);
        }
    }
    Ok(())
}

/// Whether `word` is worth pairing up: letters only, of a length typos are
/// looked for in.
fn minable(word: &str) -> bool {
    let chars = word.chars().count();
    (MIN_MINED_CHARS..=MAX_WORD_CHARS).contains(&chars) && word.chars().all(char::is_alphabetic)
}

/// A quick hash of a word, for finding words one deletion apart.
fn hash(chars: impl Iterator<Item = char>) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for c in chars {
        h ^= u64::from(c);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// The word with each letter left out in turn, and as it is, hashed.
fn deletion_hashes(chars: &[char]) -> Vec<u64> {
    let mut out = Vec::with_capacity(chars.len() + 1);
    out.push(hash(chars.iter().copied()));
    for skip in 0..chars.len() {
        out.push(hash(
            chars
                .iter()
                .enumerate()
                .filter(|&(i, _)| i != skip)
                .map(|(_, &c)| c),
        ));
    }
    out
}

/// Whether two words differ by a plural or possessive `s` at the end:
/// "bank" and "banks" are both meant.
pub(crate) fn plural_pair(a: &str, b: &str) -> bool {
    a.strip_suffix('s') == Some(b) || b.strip_suffix('s') == Some(a)
}

/// The error model: each word of `fields` paired with the word one edit
/// away found in the most sites, at least [`MINED_RATIO`] times as many,
/// and the edits counted as substring rules ([`rules`]), each
/// weighted by the sites the misspelling is found in. A rule's probability
/// is that weight over the weight of the sites its meant letters are found
/// in (across all words found in at least [`MINED_RATIO`] sites). Returns
/// the rules and the probability of an edit no rule covers.
pub(crate) fn mine_rules(searcher: &tantivy::Searcher, fields: &[Field]) -> Result<(Rules, f32)> {
    // The words that can be meant, and how often each substring of one or
    // two letters is meant.
    let mut meant: Vec<(Vec<char>, u64)> = Vec::new();
    let mut substrings: HashMap<String, u64> = HashMap::new();
    let mut letters: u64 = 0;
    for_each_term(searcher, fields, |term, docs| {
        if docs < MINED_RATIO || !minable(term) {
            return;
        }
        let marked: Vec<char> = std::iter::once('^')
            .chain(term.chars())
            .chain(std::iter::once('$'))
            .collect();
        for i in 0..marked.len() {
            *substrings.entry(marked[i].to_string()).or_insert(0) += docs;
            if i + 1 < marked.len() {
                let pair: String = marked[i..i + 2].iter().collect();
                *substrings.entry(pair).or_insert(0) += docs;
            }
        }
        letters += docs * term.chars().count() as u64;
        meant.push((term.chars().collect(), docs));
    })?;
    let mut by_deletion: Vec<(u64, u32)> = Vec::new();
    for (i, (chars, _)) in meant.iter().enumerate() {
        for h in deletion_hashes(chars) {
            by_deletion.push((h, i as u32));
        }
    }
    by_deletion.sort_unstable();
    by_deletion.dedup();

    // Per rule: sites with the misspellings, and how many misspellings.
    let mut weights: HashMap<(String, String), (u64, u32)> = HashMap::new();
    let mut typos: u64 = 0;
    for_each_term(searcher, fields, |term, docs| {
        if !minable(term) {
            return;
        }
        let chars: Vec<char> = term.chars().collect();
        let mut best: Option<u32> = None;
        for h in deletion_hashes(&chars) {
            let start = by_deletion.partition_point(|&(k, _)| k < h);
            for &(k, i) in &by_deletion[start..] {
                if k != h {
                    break;
                }
                let (candidate, candidate_docs) = &meant[i as usize];
                if *candidate == chars
                    || *candidate_docs < MINED_RATIO * docs
                    || best.is_some_and(|b| meant[b as usize].1 >= *candidate_docs)
                {
                    continue;
                }
                let found = osa_edits(candidate, &chars);
                if found.len() != 1 {
                    continue;
                }
                let candidate: String = candidate.iter().collect();
                if plural_pair(&candidate, term) {
                    continue;
                }
                best = Some(i);
            }
        }
        let Some(best) = best else {
            return;
        };
        let meant_word: String = meant[best as usize].0.iter().collect();
        typos += docs;
        for forms in edit_rules(&meant_word, term) {
            for rule in forms {
                let weight = weights.entry(rule).or_insert((0, 0));
                weight.0 += docs;
                weight.1 += 1;
            }
        }
    })?;
    let mut rules = HashMap::with_capacity(weights.len());
    for ((r, t), (weight, pairs)) in weights {
        // A rule only a pair or two show may be chance: names that differ
        // by a letter ("schwaab" and "schwab"), not a slip.
        if pairs < MIN_RULE_PAIRS {
            continue;
        }
        let Some(&of) = substrings.get(&r) else {
            continue;
        };
        // Rare letters are smoothed towards no rule at all.
        let p = (weight as f64 / (of + RULE_SMOOTHING) as f64).min(1.0) as f32;
        rules.insert((r.into_boxed_str(), t.into_boxed_str()), p);
    }
    // An unseen edit: well below the average error rate per letter.
    let unseen = if letters == 0 {
        1e-6
    } else {
        ((typos as f64 / letters as f64) / 100.0).clamp(1e-9, 1e-4) as f32
    };
    Ok((rules, unseen))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules_of(meant: &str, typed: &str) -> Vec<Vec<(String, String)>> {
        edit_rules(meant, typed)
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[test]
    fn edits_read_as_substring_rules() {
        assert_eq!(
            rules_of("amtrak", "amtrack"),
            [vec![pair("a", "ac"), pair("k", "ck")]]
        );
        assert_eq!(
            rules_of("allstate", "alstate"),
            [vec![pair("al", "a"), pair("ll", "l")]]
        );
        assert_eq!(rules_of("visa", "vias"), [vec![pair("sa", "as")]]);
        assert_eq!(
            rules_of("hertz", "herts"),
            [vec![pair("z", "s"), pair("tz", "ts"), pair("z$", "s$")]]
        );
        // The first letter is read with the start of the word.
        assert_eq!(
            rules_of("alta", "ulta"),
            [vec![pair("a", "u"), pair("^a", "^u"), pair("al", "ul")]]
        );
        assert!(rules_of("same", "same").is_empty());
        assert_eq!(rules_of("amazon", "amzon").len(), 1);
        assert_eq!(rules_of("amazon", "amzn").len(), 2);
    }

    fn model() -> Model {
        let mut rules = HashMap::new();
        rules.insert(("k".into(), "ck".into()), 0.01);
        rules.insert(("ll".into(), "l".into()), 0.02);
        let words = ["america", "bank", "of", "one", "capital", "capitol"];
        let mut sorted: Vec<&str> = words.to_vec();
        sorted.sort_unstable();
        let id = |w: &str| sorted.iter().position(|s| *s == w).unwrap() as u32;
        let counts = |w: &str| match w {
            "america" => 50,
            "bank" => 100,
            "of" => 400,
            "one" => 80,
            "capital" => 60,
            "capitol" => 30,
            _ => 0,
        };
        let mut pairs = vec![
            ((id("bank"), id("of")), 50),
            ((id("of"), id("america")), 30),
            ((id("capital"), id("one")), 40),
        ];
        pairs.sort_unstable();
        Model {
            sites: 1000,
            words: {
                let mut list = WordList::default();
                for word in &sorted {
                    list.push(word);
                }
                list
            },
            counts: sorted.iter().map(|w| counts(w)).collect(),
            pair_counts: pairs.iter().map(|p| p.1).collect(),
            pairs: pairs.into_iter().map(|p| p.0).collect(),
            rules,
            unseen: 1e-6,
        }
    }

    #[test]
    fn learned_edits_cost_less_than_unseen_ones() {
        let m = model();
        assert_eq!(m.ln_channel("amtrak", "amtrak"), 0.0);
        assert!((m.ln_channel("amtrack", "amtrak") - 0.01f64.ln()).abs() < 1e-6);
        assert!(m.ln_channel("amtrack", "amtrak") > m.ln_channel("amtrek", "amtrak"));
        // Two edits cost both.
        assert!(m.ln_channel("amtrac", "amtrak") > m.ln_channel("amtrc", "amtrak"));
    }

    #[test]
    fn word_pairs_score_by_stupid_backoff() {
        let m = model();
        assert!((m.ln_score(Some("capital"), "one") - (40.0f64 / 60.0).ln()).abs() < 1e-9);
        // Never seen after "capitol": backed off to how common "one" is.
        let backed_off = (0.4f64 * 80.0 / 1000.0).ln();
        assert!((m.ln_score(Some("capitol"), "one") - backed_off).abs() < 1e-9);
        assert!(
            m.ln_score_all(&["capital", "one"]) > m.ln_score_all(&["capitol", "one"]),
            "capital one is the likelier name"
        );
    }

    #[test]
    fn models_survive_a_round_trip() {
        let m = model();
        let mut bytes = Vec::new();
        m.write(&mut bytes).unwrap();
        assert_eq!(Model::read(&mut bytes.as_slice()).unwrap(), m);
        assert!(Model::read(&mut &bytes[..bytes.len() - 1]).is_err());
        assert!(Model::read(&mut &b"nonsense"[..]).is_err());
    }

    #[test]
    fn builders_count_sites_not_mentions() {
        let mut builder = ModelBuilder::new(crate::analysis::words_analyzer());
        builder.add(["Bank of America", "bank of america login"]);
        builder.add(["Bank of the West"]);
        builder.add(["Capital One"]);
        assert_eq!(builder.sites, 3);
        let id = |b: &ModelBuilder, w: &str| b.ids[w];
        assert_eq!(builder.counts[id(&builder, "bank") as usize], 2);
        let pair = (id(&builder, "bank"), id(&builder, "of"));
        assert_eq!(builder.pairs[&pair], 2);
        builder.prune(2);
        assert!(!builder.ids.contains_key("capital"));
        let pair = (id(&builder, "bank"), id(&builder, "of"));
        assert_eq!(builder.pairs[&pair], 2);
    }
}
