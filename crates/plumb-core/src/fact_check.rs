//! Checking what a page says against Wikidata's facts, for a site's trust
//! by accuracy (Knowledge-Based Trust, Dong et al. 2015): a site whose
//! pages state facts that match Wikidata is likely right about other
//! things too, however few sites link to it, and a site that gets them
//! wrong is not made right by links.
//!
//! A [`FactBook`] holds the names of Wikipedia articles and the facts of
//! the ones with facts ([`crate::facts`]). [`FactBook::check`] reads a
//! page's text a sentence at a time, finds the articles it names by their
//! titles and, for each, a statement of one of its facts:
//!
//! - a date after a word that says what it is ("born", "died",
//!   "founded"), or a lifespan in brackets after the name ("Albert
//!   Einstein (14 March 1879 – 18 April 1955)");
//! - a number with its unit after "population", "elevation", "height",
//!   "tall" or "area";
//! - another article's name after "capital", "founded by", "directed
//!   by", "written by", "composed by" or "currency".
//!
//! Each statement found [`Check::agrees`] with Wikidata or not. What
//! Wikidata does not know is not checked: as in the paper, a value other
//! than the one Wikidata has is taken as wrong only for a fact it has.
//! Values close but not equal (a population a few years old, a height a
//! centimetre off) are neither, so only clear mistakes count against a
//! site.
//!
//! Names are matched only when they cannot mean much else: titles without
//! a bracketed qualifier (Wikipedia adds one when the bare name is taken),
//! written with a capital, of two or more words unless the article is
//! among the most read, and shared by no other article. Aliases are not
//! used: the redirects that lead to a person's article include the names
//! of people around them.

use std::collections::HashMap;

use crate::article::Article;
use crate::facts::{Date, Fact, FactKind, ValueType};
use crate::normalize_text;

/// Most words in a name looked for.
pub const MAX_NAME_WORDS: usize = 6;

/// Articles among the most read this many whose one-word titles count as
/// names: "France" and "Einstein" are, "Jordan" (the country) is too but
/// has no birth date, so "Jordan was born" checks nothing.
pub const ONE_WORD_TOP: usize = 50_000;

/// Most tokens between a name and the word saying what follows
/// ("Albert Einstein, the physicist, was born ...").
const CUE_REACH: usize = 12;

/// Most tokens after that word in which the value is looked for.
const VALUE_REACH: usize = 8;

/// Most tokens inside a lifespan's brackets.
const BRACKET_REACH: usize = 24;

/// Most tokens between a name and the cue of a value that is another
/// article's name.
const ITEM_CUE_REACH: usize = 3;

/// Words between such a cue and its value ("capital is the city of").
const ITEM_FILLERS: &[&str] = &["is", "was", "the", "city", "of", "being", "remains"];

/// Fewest years between the dates in a name's brackets for them to be a
/// lifespan.
const MIN_LIFESPAN: i32 = 20;

/// Years off past which a date is likely about something else (another
/// John Howard) rather than a mistake.
const OTHER_THING_YEARS: i32 = 10;

/// Times off past which a number is likely of something else.
const OTHER_THING_TIMES: f64 = 3.0;

/// What a page said about one of a [`FactBook`]'s articles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Check {
    /// The article, by its place in the book ([`FactBook::title`]).
    pub entity: u32,
    pub kind: FactKind,
    /// Whether the page's value is Wikidata's.
    pub agrees: bool,
}

/// What a name in the book stands for.
#[derive(Debug, Clone, Copy)]
enum Name {
    /// An article with facts that can be checked.
    Entity(u32),
    /// An article without such facts, or two articles of one name: still
    /// a name, so a page naming it is not taken to name something else,
    /// and it can be another article's value.
    Known,
}

#[derive(Debug, Clone)]
struct Entity {
    title: Box<str>,
    facts: Vec<Fact>,
}

/// The names of Wikipedia's articles and the facts of those that have
/// facts to check pages against.
#[derive(Debug, Default)]
pub struct FactBook {
    names: HashMap<Box<str>, Name>,
    entities: Vec<Entity>,
    articles: usize,
}

/// Whether facts of `kind` are checked.
pub fn checked_kind(kind: FactKind) -> bool {
    use FactKind::*;
    matches!(
        kind,
        Born | Died
            | Founded
            | Population
            | Elevation
            | Height
            | Area
            | Capital
            | Director
            | Author
            | Composer
    )
}

impl FactBook {
    pub fn new() -> FactBook {
        FactBook::default()
    }

    /// Adds the next article of an articles file, most read first.
    pub fn add(&mut self, article: &Article) {
        let rank = self.articles;
        self.articles += 1;
        let title = article.title.trim();
        if title.ends_with(')') && title.contains(" (") {
            return;
        }
        let name = normalize_text(title);
        let words = name.split(' ').count();
        if name.is_empty() || words > MAX_NAME_WORDS {
            return;
        }
        let mut facts: Vec<Fact> = article
            .facts
            .iter()
            // Founders are kept, unchecked, so that "founded by Steve Jobs
            // in 1976" reads past them to the year.
            .filter(|fact| checked_kind(fact.kind) || fact.kind == FactKind::Founder)
            .cloned()
            .collect();
        // A country's or city's founding is a matter of definition
        // (Wikidata's United States was founded in 1784).
        if facts.iter().any(|fact| {
            matches!(
                fact.kind,
                FactKind::Capital | FactKind::Population | FactKind::Area
            )
        }) {
            facts.retain(|fact| fact.kind != FactKind::Founded);
        }
        // "David was born": one word is too few to tell people apart.
        if words == 1 {
            facts.retain(|fact| !matches!(fact.kind, FactKind::Born | FactKind::Died));
        }
        let one_word_ok = words > 1 || (rank < ONE_WORD_TOP && name.len() >= 4);
        let named = match self.names.get(name.as_str()) {
            // A second article of the same name: neither is meant for sure.
            Some(_) => Name::Known,
            None if !facts.iter().any(|f| checked_kind(f.kind)) || !one_word_ok => Name::Known,
            None => {
                let id = u32::try_from(self.entities.len()).expect("fewer than 4 billion articles");
                self.entities.push(Entity {
                    title: title.into(),
                    facts,
                });
                Name::Entity(id)
            }
        };
        self.names.insert(name.into_boxed_str(), named);
    }

    /// Articles read, with or without facts.
    pub fn articles(&self) -> usize {
        self.articles
    }

    /// Articles whose facts are checked.
    pub fn entities(&self) -> usize {
        self.entities.len()
    }

    /// The title of the article `entity` of a [`Check`].
    pub fn title(&self, entity: u32) -> &str {
        &self.entities[entity as usize].title
    }

    /// Wikidata's values of `kind` for `entity`, as kept in facts.
    pub fn values(&self, entity: u32, kind: FactKind) -> impl Iterator<Item = &str> {
        self.entities[entity as usize]
            .facts
            .iter()
            .filter(move |fact| fact.kind == kind)
            .map(|fact| fact.value.as_str())
    }

    fn lookup(&self, key: &str) -> Option<Name> {
        self.names.get(key).copied()
    }

    /// The facts `text` states about the book's articles, each checked
    /// once a sentence. Lines are paragraphs, as in Common Crawl's text.
    pub fn check(&self, text: &str) -> Vec<Check> {
        let mut checks = Vec::new();
        for line in text.lines() {
            let tokens = tokenize(line);
            for sentence in sentences(&tokens) {
                self.check_sentence(line, sentence, &mut checks);
            }
        }
        checks
    }

    fn check_sentence(&self, line: &str, tokens: &[Token], out: &mut Vec<Check>) {
        let cued = (0..tokens.len()).any(|i| cue_at_token(tokens, i).is_some());
        if !cued && !tokens.iter().any(|t| t.is_punct('(')) {
            return;
        }
        let mentions = self.mentions(tokens);
        let first = out.len();
        for mention in &mentions {
            let Name::Entity(id) = mention.name else {
                continue;
            };
            let entity = &self.entities[id as usize];
            let mut found: Vec<(FactKind, bool)> = Vec::new();
            lifespan(tokens, mention.end, entity, &mut found);
            let alone = stands_alone(tokens, mention);
            for cue_at in mention.end..tokens.len().min(mention.end + CUE_REACH) {
                if !alone {
                    break;
                }
                let Some(cue) = cue_at_token(tokens, cue_at) else {
                    continue;
                };
                // Another name before the cue: the cue is likely about it.
                if self.subject_between(tokens, &mentions, mention.end, cue_at, entity) {
                    break;
                }
                // "Tokyo Skytree's viewing deck at a height of": the cue is
                // about something of the article's.
                if mention.possessive && cue_at != mention.end {
                    break;
                }
                // A name's cue is right after it: "Inception was directed
                // by", or a noun after a possessive, "Australia's capital"
                // ("Seoul is the capital of Korea" says what Seoul is).
                if cue.kinds.iter().any(|k| k.value_type() == ValueType::Item) {
                    let close = if cue.len == 2 {
                        cue_at <= mention.end + ITEM_CUE_REACH
                    } else {
                        mention.possessive
                    };
                    if !close {
                        continue;
                    }
                }
                // "is 330 m tall": the number comes first.
                if tokens[cue_at].is_word("tall") {
                    let from = cue_at.saturating_sub(5).max(mention.end);
                    if let Some(verdict) =
                        entity_value(entity, FactKind::Height).and_then(|truth| {
                            quantity_in(tokens, from, cue_at, FactKind::Height)
                                .and_then(|v| compare_quantities(FactKind::Height, v, truth))
                        })
                    {
                        found.push((FactKind::Height, verdict));
                    }
                    continue;
                }
                let start = cue_at + cue.len;
                self.value_after(
                    line, tokens, &mentions, start, cue.kinds, entity, &mut found,
                );
            }
            // "the capital of France is Paris", "the population of Paris
            // is", "Sydney is the capital of Australia", "Christopher
            // Nolan, director of Inception". Not "the capital of Spain's
            // Catalonia region" or "the capital of Jiangsu Province".
            let whole = !mention.possessive && !capital_after(tokens, mention.end);
            if let Some((cue_at, cue)) = cue_before(tokens, mention.start).filter(|_| whole) {
                let names_value = cue.kinds.iter().any(|k| k.value_type() == ValueType::Item);
                if !names_value {
                    self.value_after(
                        line,
                        tokens,
                        &mentions,
                        mention.end,
                        cue.kinds,
                        entity,
                        &mut found,
                    );
                }
                for &kind in cue
                    .kinds
                    .iter()
                    .filter(|k| k.value_type() == ValueType::Item)
                {
                    if let Some(before) = name_before(&mentions, tokens, cue_at) {
                        judge_item(line, tokens, kind, entity, before, &mut found);
                    } else if tokens.get(mention.end).is_some_and(|t| {
                        t.is_word("is") || (t.is_word("was") && kind != FactKind::Capital)
                    }) {
                        self.value_after(
                            line,
                            tokens,
                            &mentions,
                            mention.end,
                            &[kind],
                            entity,
                            &mut found,
                        );
                    }
                }
            }
            for (kind, agrees) in found {
                if !out[first..]
                    .iter()
                    .any(|c| c.entity == id && c.kind == kind)
                {
                    out.push(Check {
                        entity: id,
                        kind,
                        agrees,
                    });
                }
            }
        }
    }

    /// Whether another subject comes between tokens `from` and `to`: a
    /// name other than one of `entity`'s values, or a capitalized word
    /// that a verb follows ("YouTube channel Frodo was founded").
    fn subject_between(
        &self,
        tokens: &[Token],
        mentions: &[Mention],
        from: usize,
        to: usize,
        entity: &Entity,
    ) -> bool {
        let named = mentions
            .iter()
            .filter(|m| m.start >= from && m.end <= to)
            .any(|m| !is_value_of(entity, &m.key));
        // "COSS (the Finnish Centre for Open Source Solutions), founded",
        // "a company he founded".
        let bracket = tokens[from..to].iter().any(|t| {
            t.is_punct(')')
                || t.is_punct('(')
                || t.word().is_some_and(|w| {
                    matches!(
                        w,
                        "he" | "she" | "they" | "we" | "i" | "you" | "his" | "her" | "their"
                    )
                })
        });
        named
            || bracket
            || (from..to).any(|i| {
                tokens[i].capital
                    && tokens[i].word().is_some()
                    && tokens
                        .get(i + 1)
                        .and_then(Token::word)
                        .is_some_and(|w| matches!(w, "was" | "is" | "were" | "are" | "has" | "had"))
            })
    }

    /// Looks for a value of each of `kinds` that `entity` has, in the
    /// tokens from `start`.
    #[allow(clippy::too_many_arguments)]
    fn value_after(
        &self,
        line: &str,
        tokens: &[Token],
        mentions: &[Mention],
        start: usize,
        kinds: &[FactKind],
        entity: &Entity,
        found: &mut Vec<(FactKind, bool)>,
    ) {
        let end = tokens.len().min(start + VALUE_REACH);
        // A date or number is before the next name that is not one of
        // the article's values.
        let end = mentions
            .iter()
            .filter(|m| m.start >= start && m.start < end && !is_value_of(entity, &m.key))
            .map(|m| m.start)
            .min()
            .unwrap_or(end);
        for &kind in kinds {
            if found.iter().any(|(k, _)| *k == kind) {
                continue;
            }
            let Some(truth) = entity_value(entity, kind) else {
                continue;
            };
            let verdict = match kind.value_type() {
                ValueType::Time if start < end => date_in(tokens, start, end)
                    .and_then(|date| Date::parse(truth).and_then(|t| compare_dates(date, t))),
                ValueType::Quantity if start < end => quantity_in(tokens, start, end, kind)
                    .and_then(|value| compare_quantities(kind, value, truth)),
                ValueType::Item => {
                    // The name is right after the cue: "capital, Canberra",
                    // "capital is Canberra", "directed by Christopher Nolan".
                    let mut at = start;
                    while tokens.get(at).is_some_and(|t| {
                        t.is_punct(',')
                            || t.is_punct(':')
                            || t.word().is_some_and(|w| ITEM_FILLERS.contains(&w))
                    }) {
                        at += 1;
                    }
                    match mentions.iter().find(|m| m.start == at) {
                        Some(named) => {
                            judge_item(line, tokens, kind, entity, named, found);
                            continue;
                        }
                        None => None,
                    }
                }
                _ => None,
            };
            if let Some(agrees) = verdict {
                found.push((kind, agrees));
            }
        }
    }

    /// The names in `tokens`, longest first where they overlap, each
    /// starting with a capital letter.
    fn mentions(&self, tokens: &[Token]) -> Vec<Mention> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            if tokens[i].word().is_none() || !tokens[i].capital {
                i += 1;
                continue;
            }
            // The words from i, a hyphen allowed between two of them.
            let mut words: Vec<(usize, &Token)> = vec![(i, &tokens[i])];
            let mut j = i + 1;
            while words.len() < MAX_NAME_WORDS && j < tokens.len() {
                if tokens[j].word().is_some() {
                    words.push((j, &tokens[j]));
                    j += 1;
                } else if tokens[j].is_punct('-')
                    && tokens.get(j + 1).is_some_and(|t| t.word().is_some())
                {
                    j += 1;
                } else {
                    break;
                }
            }
            let mut matched = None;
            for n in (1..=words.len()).rev() {
                let mut key = String::new();
                for (k, (_, token)) in words[..n].iter().enumerate() {
                    if k > 0 {
                        key.push(' ');
                    }
                    key.push_str(token.word().unwrap_or_default());
                }
                let last = words[n - 1].1;
                let found = self
                    .lookup(&key)
                    .map(|name| (name, key.clone(), false))
                    .or_else(|| {
                        // "Einstein's" for "Einstein".
                        let base = last.possessive_base()?;
                        let cut = key.len() - last.word().unwrap_or_default().len();
                        let key = format!("{}{base}", &key[..cut]);
                        self.lookup(&key).map(|name| (name, key, true))
                    });
                // "In", "The": a short word that starts a sentence names
                // nothing.
                let short = n == 1 && key.chars().count() < 4;
                if let Some((name, key, possessive)) = found.filter(|_| !short) {
                    matched = Some(Mention {
                        start: i,
                        end: words[n - 1].0 + 1,
                        name,
                        key,
                        possessive,
                    });
                    break;
                }
            }
            match matched {
                Some(mention) => {
                    i = mention.end;
                    out.push(mention);
                }
                None => i += 1,
            }
        }
        out
    }
}

#[derive(Debug, Clone)]
struct Mention {
    start: usize,
    end: usize,
    name: Name,
    key: String,
    /// Written with "'s": "Australia's".
    possessive: bool,
}

fn entity_value(entity: &Entity, kind: FactKind) -> Option<&str> {
    entity
        .facts
        .iter()
        .find(|f| f.kind == kind)
        .map(|f| f.value.as_str())
}

fn is_value_of(entity: &Entity, key: &str) -> bool {
    entity
        .facts
        .iter()
        .any(|f| f.kind.value_type() == ValueType::Item && normalize_text(&f.value) == key)
}

/// Whether the name `named` is `entity`'s value of `kind`, or another
/// article's name: a part of the right name ("Washington" for
/// "Washington, D.C.") or a name the right one is part of is neither.
fn judge_item(
    line: &str,
    tokens: &[Token],
    kind: FactKind,
    entity: &Entity,
    named: &Mention,
    found: &mut Vec<(FactKind, bool)>,
) {
    // "Spain's capital is Europe's third-largest city": a name with "'s"
    // is not the value but whose it is.
    if found.iter().any(|(k, _)| *k == kind)
        || named.possessive
        || named.key == normalize_text(&entity.title)
    {
        return;
    }
    let truths: Vec<String> = entity
        .facts
        .iter()
        .filter(|f| f.kind == kind)
        .map(|f| normalize_text(&f.value))
        .filter(|t| !t.is_empty())
        .collect();
    if truths.is_empty() {
        return;
    }
    // The name and a few tokens after it, for values that read as more
    // than the name ("Washington, D.C.").
    let end = tokens.len().min(named.end + 4);
    let window = format!(
        " {} ",
        normalize_text(&line[tokens[named.start].start..tokens[end - 1].end])
    );
    let name = format!(" {} ", named.key);
    let verdict = if truths.iter().any(|t| window.starts_with(&format!(" {t} "))) {
        Some(true)
    } else if truths.iter().any(|t| {
        let t = format!(" {t} ");
        t.contains(&name) || name.contains(&t)
    }) {
        None
    } else {
        Some(false)
    };
    if let Some(agrees) = verdict {
        found.push((kind, agrees));
    }
}

/// The mention ending right before `cue_at`, with at most "is the", "was
/// the" or ", the" between: "Sydney is the capital of", "Christopher
/// Nolan, director of".
fn name_before<'a>(
    mentions: &'a [Mention],
    tokens: &[Token],
    cue_at: usize,
) -> Option<&'a Mention> {
    let mut at = cue_at;
    let mut skipped = 0;
    while at > 0 && skipped < 3 {
        let t = &tokens[at - 1];
        if t.is_punct(',')
            || t.word()
                .is_some_and(|w| matches!(w, "is" | "was" | "the" | "a"))
        {
            at -= 1;
            skipped += 1;
        } else {
            break;
        }
    }
    if skipped == 0 {
        return None;
    }
    mentions.iter().find(|m| m.end == at)
}

/// A one-word cue right before "of" and the name at `name_start`: "the
/// population of", "capital of the". Not "the industrial capital of": a
/// capital is only the one with "the", "its" or nothing before it.
fn cue_before(tokens: &[Token], name_start: usize) -> Option<(usize, Cue)> {
    let mut at = name_start.checked_sub(1)?;
    if tokens[at].is_word("the") {
        at = at.checked_sub(1)?;
    }
    if !tokens[at].is_word("of") {
        return None;
    }
    let cue_at = at.checked_sub(1)?;
    let cue = cue_at_token(tokens, cue_at).filter(|cue| cue.len == 1)?;
    if cue.kinds.contains(&FactKind::Capital) && cue_at > 0 {
        let before = &tokens[cue_at - 1];
        let plain = before.is_punct(',')
            || before
                .word()
                .is_some_and(|w| matches!(w, "the" | "a" | "its" | "their" | "is" | "as"));
        // "Tanis was the capital of Egypt": once, not now.
        let past = tokens[..cue_at]
            .iter()
            .rev()
            .take(3)
            .any(|t| t.is_word("was") || t.is_word("former") || t.is_word("ancient"));
        if !plain || past {
            return None;
        }
    }
    Some((cue_at, cue))
}

/// Whether the token at `at` is a word with a capital past a sentence's
/// start: more of a name.
fn capital_after(tokens: &[Token], at: usize) -> bool {
    tokens
        .get(at)
        .is_some_and(|t| t.capital && t.word().is_some())
}

/// Words a name after which is part of something else: "the population
/// of North Dakota", "a peak north of Addis Ababa", "the Coalition for the
/// International Criminal Court".
const PART_OF: &[&str] = &["of", "for", "in", "at", "from", "near", "to"];

/// Whether `mention` is the subject of what follows it rather than part
/// of something else: not after "of" or "in" (within a few words), and
/// not inside a longer name ("ALT Linux", "Alaska Boise").
fn stands_alone(tokens: &[Token], mention: &Mention) -> bool {
    let before = &tokens[mention.start.saturating_sub(3)..mention.start];
    if before
        .iter()
        .any(|t| t.word().is_some_and(|w| PART_OF.contains(&w)))
    {
        return false;
    }
    let capital_before = mention.start > 0 && capital_after(tokens, mention.start - 1);
    !capital_before && (mention.possessive || !capital_after(tokens, mention.end))
}

/// A cue: words saying which kind of fact follows.
#[derive(Debug, Clone, Copy)]
struct Cue {
    kinds: &'static [FactKind],
    /// Tokens it takes.
    len: usize,
}

fn cue_at_token(tokens: &[Token], at: usize) -> Option<Cue> {
    use FactKind::*;
    // "Capital Area Dental Society", "Currency Dilemma": a capital past a
    // sentence's start makes a name.
    if tokens[at].capital && at > 0 {
        return None;
    }
    let word = tokens[at].word()?;
    let by = tokens.get(at + 1).and_then(Token::word) == Some("by");
    let cue = |kinds: &'static [FactKind], len| Some(Cue { kinds, len });
    match word {
        "born" => cue(&[Born], 1),
        "died" => cue(&[Died], 1),
        "founded" | "cofounded" | "established" if by => cue(&[Founded], 2),
        "founded" | "established" | "incorporated" => cue(&[Founded], 1),
        "directed" if by => cue(&[Director], 2),
        "director" => cue(&[Director], 1),
        "written" if by => cue(&[Author], 2),
        "author" => cue(&[Author], 1),
        "composed" if by => cue(&[Composer], 2),
        "composer" => cue(&[Composer], 1),
        "capital" => cue(&[Capital], 1),
        "population" => cue(&[Population], 1),
        "elevation" | "altitude" => cue(&[Elevation], 1),
        "height" => cue(&[Height, Elevation], 1),
        "tall" | "stands" => cue(&[Height], 1),
        "area" => cue(&[Area], 1),
        _ => None,
    }
}

/// Born and died from a lifespan in brackets right after a name:
/// "(14 March 1879 – 18 April 1955)", "(1856–1939)", "(born 1976)". A
/// span shorter than [`MIN_LIFESPAN`] years is a term of office or a
/// marriage, one of a hundred years or more an anniversary.
fn lifespan(tokens: &[Token], after: usize, entity: &Entity, found: &mut Vec<(FactKind, bool)>) {
    if !tokens.get(after).is_some_and(|t| t.is_punct('(')) {
        return;
    }
    let start = after + 1;
    let Some(close) =
        (start..tokens.len().min(start + BRACKET_REACH)).find(|&i| tokens[i].is_punct(')'))
    else {
        return;
    };
    let inside = &tokens[start..close];
    let truth = |kind: FactKind| entity_value(entity, kind).and_then(Date::parse);
    // (born 1976), (b. 14 March 1879)
    let mut at = 0;
    if inside
        .first()
        .is_some_and(|t| t.is_word("born") || t.is_word("b"))
    {
        at = 1 + usize::from(inside.get(1).is_some_and(|t| t.is_punct('.')));
        if let (Some((date, used)), Some(t)) = (date_at(inside, at), truth(FactKind::Born)) {
            if used == inside.len() {
                if let Some(agrees) = compare_dates(date, t) {
                    found.push((FactKind::Born, agrees));
                }
            }
        }
        return;
    }
    // A date, a dash, a date, and nothing else.
    let Some((born, dash)) = date_at(inside, at) else {
        return;
    };
    if !inside.get(dash).is_some_and(Token::is_dash) {
        return;
    }
    let Some((died, used)) = date_at(inside, dash + 1) else {
        return;
    };
    if used != inside.len() || !(MIN_LIFESPAN..100).contains(&(died.year - born.year)) {
        return;
    }
    // One date far off: another person, or no lifespan ("Saddam Hussein
    // (1980 - 2002)" on a banknote).
    let far = [(FactKind::Born, born), (FactKind::Died, died)]
        .iter()
        .any(|&(kind, date)| {
            truth(kind).is_some_and(|t| (date.year - t.year).abs() > OTHER_THING_YEARS)
        });
    if far {
        return;
    }
    for (kind, date) in [(FactKind::Born, born), (FactKind::Died, died)] {
        if let Some(agrees) = truth(kind).and_then(|t| compare_dates(date, t)) {
            found.push((kind, agrees));
        }
    }
}

/// The first date in `tokens[start..end]`: what follows "born" or
/// "founded". A decade ("the 1990s") or a year with "BC" is no date.
fn date_in(tokens: &[Token], start: usize, end: usize) -> Option<Date> {
    for i in start..end {
        if tokens[i].word().is_some_and(|w| {
            matches!(
                w,
                "c" | "ca"
                    | "circa"
                    | "around"
                    | "about"
                    | "approximately"
                    | "before"
                    | "after"
                    | "between"
                    | "until"
                    | "since"
                    | "by"
                    | "late"
                    | "early"
                    | "mid"
            )
        }) {
            return None;
        }
        if let Some((date, _)) = date_at(tokens, i) {
            return Some(date);
        }
        // A number that is no date ("born 3 days later") ends the look.
        if tokens[i].number().is_some() {
            return None;
        }
    }
    None
}

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

fn month_of(word: &str) -> Option<u8> {
    if let Some(i) = MONTHS.iter().position(|m| *m == word) {
        return Some(i as u8 + 1);
    }
    let short = match word {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" | "sept" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    Some(short)
}

/// A year from 1000 to 2100 written as one: four digits, no commas.
fn year_of(token: &Token) -> Option<i32> {
    match token.kind {
        TokenKind::Number {
            value,
            plain: true,
            digits: 4,
        } if (1000.0..=2100.0).contains(&value) => Some(value as i32),
        _ => None,
    }
}

fn day_of(token: &Token) -> Option<u8> {
    match token.kind {
        TokenKind::Number {
            value,
            plain: true,
            digits: 1 | 2,
        } if (1.0..=31.0).contains(&value) => Some(value as u8),
        _ => None,
    }
}

/// A date starting at `tokens[i]`: "14 March 1879", "March 14, 1879",
/// "March 1879", "1879-03-14" or "1879", and the token after it.
fn date_at(tokens: &[Token], i: usize) -> Option<(Date, usize)> {
    let at = |k: usize| tokens.get(i + k);
    let not_decade = |k: usize| {
        !tokens
            .get(i + k)
            .and_then(Token::word)
            .is_some_and(|w| matches!(w, "s" | "bc" | "bce" | "ad"))
            && !tokens.get(i + k).is_some_and(|t| t.is_punct('%'))
    };
    let first = at(0)?;
    // 14 March 1879
    if let (Some(day), Some(month), Some(year)) = (
        day_of(first),
        at(1).and_then(Token::word).and_then(month_of),
        at(2).and_then(year_of),
    ) {
        return Some((date(year, Some(month), Some(day)), i + 3));
    }
    if let Some(month) = first.word().and_then(month_of) {
        // March 14, 1879
        if let Some(day) = at(1).and_then(day_of) {
            let comma = usize::from(at(2).is_some_and(|t| t.is_punct(',')));
            if let Some(year) = at(2 + comma).and_then(year_of) {
                return Some((date(year, Some(month), Some(day)), i + 3 + comma));
            }
        }
        // March 1879
        if let Some(year) = at(1).and_then(year_of) {
            return Some((date(year, Some(month), None), i + 2));
        }
        return None;
    }
    let year = year_of(first)?;
    // 1879-03-14
    if at(1).is_some_and(|t| t.is_punct('-')) && at(3).is_some_and(|t| t.is_punct('-')) {
        if let (
            Some(TokenKind::Number {
                value: m,
                digits: 2,
                ..
            }),
            Some(day),
        ) = (at(2).map(|t| t.kind), at(4).and_then(day_of))
        {
            if (1.0..=12.0).contains(&m) {
                return Some((date(year, Some(m as u8), Some(day)), i + 5));
            }
        }
    }
    not_decade(1).then(|| (date(year, None, None), i + 1))
}

fn date(year: i32, month: Option<u8>, day: Option<u8>) -> Date {
    Date { year, month, day }
}

/// Whether a stated date is Wikidata's: `None` when it is too close to
/// tell (a founding a year off, which depends on what counts) or Wikidata
/// knows it less precisely than the page.
fn compare_dates(stated: Date, truth: Date) -> Option<bool> {
    let years = (stated.year - truth.year).abs();
    if years > OTHER_THING_YEARS {
        return None;
    }
    // A year off is often a matter of records (Purcell, 1658 or 1659).
    if years == 1 {
        return None;
    }
    if years > 0 {
        return Some(false);
    }
    match (stated.month, truth.month) {
        (Some(a), Some(b)) if a != b => return Some(false),
        (Some(_), None) => return None,
        _ => {}
    }
    match (stated.day, truth.day) {
        // A day off is often a time zone or a calendar.
        (Some(a), Some(b)) if a.abs_diff(b) > 1 => Some(false),
        (Some(a), Some(b)) if a != b => None,
        (Some(_), None) => None,
        _ => Some(true),
    }
}

/// Words that make a population, height or area not the one asked
/// about: "the metropolitan area's population", "population density".
const OTHER_MEASURE: &[&str] = &[
    "metro",
    "metropolitan",
    "urban",
    "greater",
    "agglomeration",
    "region",
    "county",
    "district",
    "province",
    "municipality",
    "density",
    "growth",
    "households",
    "families",
    "land",
    "water",
    "floor",
    "roof",
    "tip",
    "prominence",
    "average",
    "mean",
];

/// The first number of `kind` in `tokens[start..end]`, in SI units
/// (metres, square metres, people). Lengths and areas need their unit.
fn quantity_in(tokens: &[Token], start: usize, end: usize, kind: FactKind) -> Option<f64> {
    if tokens[start.saturating_sub(4)..end]
        .iter()
        .any(|t| t.word().is_some_and(|w| OTHER_MEASURE.contains(&w)))
    {
        return None;
    }
    for i in start..end {
        let Some(value) = tokens[i].number() else {
            continue;
        };
        // "(2020 census)": a year, not the number.
        if year_of(&tokens[i]).is_some()
            && !tokens
                .get(i + 1)
                .and_then(Token::word)
                .is_some_and(multiplier_word)
        {
            continue;
        }
        let next = tokens.get(i + 1);
        if next.is_some_and(|t| t.is_punct('%')) {
            return None;
        }
        // "more than 2,000", "less than 500,000": a bound, not the number.
        let bound = tokens[start..i].iter().rev().take(2).any(|t| {
            t.word().is_some_and(|w| {
                matches!(
                    w,
                    "than"
                        | "over"
                        | "under"
                        | "nearly"
                        | "almost"
                        | "upto"
                        | "least"
                        | "most"
                        | "exceeds"
                )
            })
        });
        if bound {
            return None;
        }
        return match kind {
            FactKind::Population => {
                let mult = next
                    .and_then(Token::word)
                    .map_or(1.0, |w| multiplier(w).unwrap_or(1.0));
                let people = value * mult;
                // "per", "km": a density or a distance.
                let after = if mult > 1.0 { i + 2 } else { i + 1 };
                if tokens.get(after).and_then(Token::word).is_some_and(|w| {
                    matches!(w, "per" | "km" | "km2" | "km²" | "sq" | "square" | "mi")
                }) {
                    return None;
                }
                Some(people)
            }
            FactKind::Area => area_unit(tokens, i + 1).map(|m2| value * m2),
            FactKind::Height | FactKind::Elevation => length(tokens, i, value),
            _ => None,
        };
    }
    None
}

fn multiplier_word(word: &str) -> bool {
    multiplier(word).is_some()
}

fn multiplier(word: &str) -> Option<f64> {
    match word {
        "thousand" => Some(1e3),
        "million" | "mn" => Some(1e6),
        "billion" | "bn" => Some(1e9),
        _ => None,
    }
}

/// Square metres in the area unit at `tokens[at..]`.
fn area_unit(tokens: &[Token], at: usize) -> Option<f64> {
    let w = |k: usize| tokens.get(at + k).and_then(Token::word);
    // "sq. mi": the dot is a token.
    let skip_dot = usize::from(tokens.get(at + 1).is_some_and(|t| t.is_punct('.')));
    let unit = match (w(0)?, w(1 + skip_dot)) {
        ("km2" | "km²", _) => 1e6,
        ("mi2" | "mi²", _) => 2.589_988e6,
        ("sq" | "square", Some("km" | "kilometres" | "kilometers" | "kilometre" | "kilometer")) => {
            1e6
        }
        ("sq" | "square", Some("mi" | "miles" | "mile")) => 2.589_988e6,
        ("sq" | "square", Some("m" | "metres" | "meters")) => 1.0,
        ("hectares", _) => 1e4,
        ("acres", _) => 4_046.856,
        _ => return None,
    };
    Some(unit)
}

/// Metres in the length at `tokens[i..]`, whose number is `value`:
/// "8,849 m", "29,032 feet", "1.75 m", "175 cm", "6 ft 2 in".
fn length(tokens: &[Token], i: usize, value: f64) -> Option<f64> {
    let unit = tokens.get(i + 1).and_then(Token::word)?;
    let metres = match unit {
        "m" | "metres" | "meters" | "metre" | "meter" => value,
        "cm" | "centimetres" | "centimeters" => value / 100.0,
        "km" | "kilometres" | "kilometers" => value * 1000.0,
        "ft" | "feet" | "foot" => {
            let mut metres = value * 0.3048;
            let inches = tokens.get(i + 2).and_then(Token::number);
            let inch_unit = tokens
                .get(i + 3)
                .and_then(Token::word)
                .is_some_and(|w| matches!(w, "in" | "inch" | "inches"));
            if let (Some(inches), true) = (inches, inch_unit) {
                metres += inches * 0.0254;
            }
            metres
        }
        _ => return None,
    };
    Some(metres)
}

/// Whether a stated number is Wikidata's: close enough agrees, far off
/// does not, in between is neither (a census a few years old, a height
/// with or without its antenna).
fn compare_quantities(kind: FactKind, stated: f64, truth: &str) -> Option<bool> {
    let truth: f64 = truth.split(';').next()?.parse().ok()?;
    if truth <= 0.0 || stated <= 0.0 {
        return None;
    }
    let times = (stated / truth).max(truth / stated);
    if times > OTHER_THING_TIMES {
        return None;
    }
    let off = times - 1.0;
    let (same, wrong) = match kind {
        FactKind::Population => (0.10, 0.5),
        FactKind::Area => (0.05, 0.3),
        FactKind::Elevation => (0.03, 0.2),
        FactKind::Height => (0.02, 0.1),
        _ => return None,
    };
    if off <= same {
        Some(true)
    } else if off >= wrong {
        Some(false)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum TokenKind {
    Word,
    /// A number: `plain` without thousands commas or decimals, `digits`
    /// before any decimal point.
    Number {
        value: f64,
        plain: bool,
        digits: usize,
    },
    Punct(char),
}

/// A token of a line: a word (lowercase letters and digits, apostrophes
/// dropped), a number, or one other character.
#[derive(Debug, Clone, PartialEq)]
struct Token {
    kind: TokenKind,
    text: String,
    /// Starts with a capital letter.
    capital: bool,
    /// Byte offsets in the line.
    start: usize,
    end: usize,
    /// The word without a final "'s", when it had one.
    base: Option<String>,
}

impl Token {
    fn word(&self) -> Option<&str> {
        (self.kind == TokenKind::Word).then_some(self.text.as_str())
    }

    fn is_word(&self, word: &str) -> bool {
        self.word() == Some(word)
    }

    fn is_punct(&self, c: char) -> bool {
        self.kind == TokenKind::Punct(c)
    }

    fn is_dash(&self) -> bool {
        matches!(self.kind, TokenKind::Punct('-' | '–' | '—')) || self.is_word("to")
    }

    fn number(&self) -> Option<f64> {
        match self.kind {
            TokenKind::Number { value, .. } => Some(value),
            _ => None,
        }
    }

    fn possessive_base(&self) -> Option<&str> {
        self.base.as_deref()
    }
}

fn is_apostrophe(c: char) -> bool {
    matches!(c, '\'' | '\u{2019}' | '\u{02BC}')
}

/// The tokens of one line.
fn tokenize(line: &str) -> Vec<Token> {
    let mut tokens: Vec<Token> = Vec::new();
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let end_of = |k: usize| chars.get(k).map_or(line.len(), |&(at, _)| at);
    let mut k = 0;
    while k < chars.len() {
        let (start, c) = chars[k];
        if c.is_whitespace() {
            k += 1;
            continue;
        }
        if c.is_ascii_digit() {
            let mut j = k;
            let mut text = String::new();
            let mut plain = true;
            let mut digits = 0;
            let mut decimals = false;
            while j < chars.len() {
                let ch = chars[j].1;
                if ch.is_ascii_digit() {
                    text.push(ch);
                    if !decimals {
                        digits += 1;
                    }
                    j += 1;
                } else if ch == ',' && !decimals {
                    // Thousands: exactly three digits follow.
                    let group: Vec<char> = chars[j + 1..].iter().take(4).map(|&(_, c)| c).collect();
                    if group.len() >= 3
                        && group[..3].iter().all(char::is_ascii_digit)
                        && !group.get(3).is_some_and(char::is_ascii_digit)
                    {
                        plain = false;
                        j += 1;
                    } else {
                        break;
                    }
                } else if ch == '.'
                    && !decimals
                    && chars.get(j + 1).is_some_and(|&(_, c)| c.is_ascii_digit())
                {
                    text.push('.');
                    decimals = true;
                    plain = false;
                    j += 1;
                } else {
                    break;
                }
            }
            let value = text.parse::<f64>().unwrap_or(0.0);
            tokens.push(Token {
                kind: TokenKind::Number {
                    value,
                    plain,
                    digits,
                },
                text,
                capital: false,
                start,
                end: end_of(j),
                base: None,
            });
            k = j;
            continue;
        }
        if c.is_alphanumeric() {
            let mut j = k;
            let mut text = String::new();
            let mut base = None;
            while j < chars.len() {
                let ch = chars[j].1;
                if ch.is_alphanumeric() {
                    text.extend(ch.to_lowercase().filter(|c| c.is_alphanumeric()));
                    j += 1;
                } else if is_apostrophe(ch)
                    && chars.get(j + 1).is_some_and(|&(_, c)| c.is_alphabetic())
                {
                    let rest_is_s = chars.get(j + 1).is_some_and(|&(_, c)| c == 's' || c == 'S')
                        && !chars.get(j + 2).is_some_and(|&(_, c)| c.is_alphanumeric());
                    if rest_is_s {
                        base = Some(text.clone());
                    }
                    j += 1;
                } else {
                    break;
                }
            }
            tokens.push(Token {
                kind: TokenKind::Word,
                text,
                capital: c.is_uppercase(),
                start,
                end: end_of(j),
                base,
            });
            k = j;
            continue;
        }
        tokens.push(Token {
            kind: TokenKind::Punct(c),
            text: c.to_string(),
            capital: false,
            start,
            end: end_of(k + 1),
            base: None,
        });
        k += 1;
    }
    join_initials(tokens)
}

/// Joins runs of single letters, as [`normalize_text`] does: "J. K.
/// Rowling" is `jk rowling`, "U.S." is `us`.
fn join_initials(tokens: Vec<Token>) -> Vec<Token> {
    let single = |t: &Token| t.kind == TokenKind::Word && t.text.chars().count() == 1;
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        if single(&tokens[i]) {
            // Letters with at most a dot after each.
            let mut letters = vec![i];
            let mut j = i + 1;
            loop {
                let skip = usize::from(tokens.get(j).is_some_and(|t| t.is_punct('.')));
                match tokens.get(j + skip) {
                    Some(t) if single(t) => {
                        letters.push(j + skip);
                        j += skip + 1;
                    }
                    _ => break,
                }
            }
            if letters.len() >= 2 {
                let last = *letters.last().expect("two letters");
                // The dot after the last letter belongs to it.
                let end_at = if tokens.get(last + 1).is_some_and(|t| t.is_punct('.')) {
                    last + 1
                } else {
                    last
                };
                let text: String = letters.iter().map(|&l| tokens[l].text.as_str()).collect();
                out.push(Token {
                    kind: TokenKind::Word,
                    text,
                    capital: tokens[i].capital,
                    start: tokens[i].start,
                    end: tokens[end_at].end,
                    base: None,
                });
                i = end_at + 1;
                continue;
            }
        }
        out.push(tokens[i].clone());
        i += 1;
    }
    out
}

/// Short words a dot after which does not end a sentence: "Dr.", "St.",
/// "Jr.", "Inc.".
fn abbreviation(word: &str) -> bool {
    matches!(
        word,
        "mr" | "mrs"
            | "ms"
            | "dr"
            | "st"
            | "jr"
            | "sr"
            | "inc"
            | "ltd"
            | "co"
            | "corp"
            | "mt"
            | "no"
            | "vs"
            | "approx"
            | "est"
            | "gen"
            | "col"
            | "sen"
            | "rep"
            | "gov"
            | "lt"
            | "sgt"
            | "prof"
            | "rev"
            | "pres"
            | "jan"
            | "feb"
            | "mar"
            | "apr"
            | "jun"
            | "jul"
            | "aug"
            | "sep"
            | "sept"
            | "oct"
            | "nov"
            | "dec"
            | "b"
            | "d"
            | "c"
            | "ca"
            | "fl"
    )
}

/// The sentences of a line's tokens.
fn sentences(tokens: &[Token]) -> Vec<&[Token]> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, token) in tokens.iter().enumerate() {
        let ends = match token.kind {
            TokenKind::Punct('!' | '?' | ';' | '|' | '•') => true,
            TokenKind::Punct('.') => {
                let before = i.checked_sub(1).map(|b| &tokens[b]);
                let after_capital_or_end = tokens
                    .get(i + 1)
                    .is_none_or(|t| t.capital || t.number().is_some());
                after_capital_or_end && !before.and_then(Token::word).is_some_and(abbreviation)
            }
            _ => false,
        };
        if ends {
            if i > start {
                out.push(&tokens[start..i]);
            }
            start = i + 1;
        }
    }
    if start < tokens.len() {
        out.push(&tokens[start..]);
    }
    out
}

#[cfg(test)]
mod tests;
