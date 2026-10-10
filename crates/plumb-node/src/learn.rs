//! Learning from clicks, for each browser that searches a node with its
//! history on (see [`crate::history`]), kept in that browser's history
//! file and nowhere else.
//!
//! Two things are learned:
//!
//! - Which boxes of the results page the searcher uses, and for which
//!   searches: the places list and its map, and the "Recent" headlines.
//!   Each time a box is shown the node notes it, and each time one of its
//!   links is opened (through `/go`) it notes that too. A box the searcher
//!   keeps passing by for searches like this one is folded to one line
//!   (still there, opened with a click), and "Recent" headlines they keep
//!   reading come unfolded.
//! - Which results they pass over. A site listed above the one they pick,
//!   search after search, and never picked itself, moves down a little.
//!   Sites they pick move up through [`crate::history::History::bonus`].
//!
//! In edit mode (`/search?edit=1`) the searcher can also say it outright:
//! each result gets buttons to put it higher or lower for this search, or
//! hide it from this search ([`Verdict`]), and each box buttons to fold it
//! or keep it open, for this search or for every search ([`BoxVerdict`]).
//! What they say wins over what their clicks say.
//!
//! "Searches like this one" are, from most to least alike: the same
//!   search, searches sharing its words, and every search the box was
//!   shown for in the same way (for places: whether the query said where,
//!   as in "pizza in denver", or the town was guessed, as in "us bank").
//!   The most alike that has been seen [`MIN_SHOWN`] times decides.

use plumb_core::normalize_text;
use serde::{Deserialize, Serialize};

/// Times a box must have been shown for searches alike before its use
/// for them decides anything.
pub const MIN_SHOWN: u32 = 3;
/// A box opened less often than this, of the times shown, is folded.
pub const FOLD_BELOW: f32 = 0.2;
/// "Recent" headlines read at least this often are unfolded.
pub const OPEN_FROM: f32 = 0.5;
/// Most box counts kept; the ones seen longest ago go first.
pub(crate) const MAX_BLOCK_COUNTS: usize = 400;
/// Most site counts kept.
pub(crate) const MAX_SITE_COUNTS: usize = 300;
/// Results pages remembered, to tell which box or site was opened.
const MAX_SHOWN: usize = 30;
/// Results remembered per page, best first.
const SITES_PER_PAGE: usize = 10;
/// Times a site must be passed over, never picked, before it moves down.
const PASSED_BEFORE_DOWN: u32 = 3;
/// How far down a site passed over moves, per time past
/// [`PASSED_BEFORE_DOWN`], and at most.
const PASSED_STEP: f32 = 0.05;
const PASSED_MOST: f32 = 0.1;
/// Words of a query counted, at most.
const MAX_WORDS: usize = 8;
/// Score a result put higher for a search gets, and one put lower.
pub const VERDICT_UP: f32 = 0.4;
pub const VERDICT_DOWN: f32 = -0.3;
/// Most verdicts kept, of each kind.
pub(crate) const MAX_VERDICTS: usize = 500;

/// A box of the results page that is learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Block {
    /// Places and their map ("pizza in denver").
    Places,
    /// The "Recent" headlines.
    News,
}

impl Block {
    pub fn as_str(self) -> &'static str {
        match self {
            Block::Places => "places",
            Block::News => "news",
        }
    }

    pub fn parse(name: &str) -> Option<Block> {
        match name {
            "places" => Some(Block::Places),
            "news" => Some(Block::News),
            _ => None,
        }
    }

    /// What the box is called on the history page.
    pub fn label(self) -> &'static str {
        match self {
            Block::Places => "Places and map",
            Block::News => "Recent headlines",
        }
    }
}

/// How a box was shown, which counts apart: places for a query that said
/// where are not places for a guessed town.
pub fn places_context(guessed: bool) -> &'static str {
    if guessed {
        "guessed"
    } else {
        "said"
    }
}

/// How the "Recent" box was shown: a named site's latest posts, or
/// headlines about the query's words.
pub fn news_context(site: bool) -> &'static str {
    if site {
        "site"
    } else {
        "words"
    }
}

/// Times a box was shown and used for searches alike.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockCount {
    pub block: Block,
    /// `q <query>`, `w <context> <word>` or `c <context>`.
    pub key: String,
    pub shown: u32,
    pub used: u32,
    /// Unix time it last changed.
    pub at: u64,
}

/// Times a site was picked, and passed over for a site below it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteCount {
    pub domain: String,
    pub picked: u32,
    pub passed: u32,
    pub at: u64,
}

/// A results page shown, remembered until a few more are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShownPage {
    /// The query, as [`query_key`] makes it.
    pub query: String,
    /// The boxes shown, how, and whether they were counted as shown
    /// (folded ones are not).
    pub blocks: Vec<(Block, String, bool)>,
    /// Boxes already counted as used on this page.
    pub used: Vec<Block>,
    /// This node's results, best first.
    pub sites: Vec<String>,
    /// Sites already counted as picked on this page.
    pub picked: Vec<String>,
    pub at: u64,
}

/// What a browser's clicks taught the node.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Learned {
    pub blocks: Vec<BlockCount>,
    pub sites: Vec<SiteCount>,
    /// Newest first.
    pub pages: Vec<ShownPage>,
    /// What the searcher said about results, in edit mode: newest first.
    pub verdicts: Vec<SiteVerdict>,
    /// What the searcher said about boxes, in edit mode: newest first.
    pub boxes: Vec<BoxVerdict>,
    /// What they like and dislike in results in general, from the tuning
    /// page and edit mode: one count for each trait.
    pub tastes: Vec<Taste>,
    /// The same count over every result shown in edit mode, which each
    /// trait is compared with. `None` in history files from before it was
    /// kept, whose tastes counted only rated results and are dropped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judged: Option<Judged>,
}

/// Every result shown in edit mode and what was said of them; see
/// [`Learned::tastes`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Judged {
    pub liked: f32,
    pub disliked: f32,
    pub seen: f32,
    /// The searches ([`query_key`]) whose results are counted as seen,
    /// newest first, so a page reloaded after each button counts once.
    pub pages: Vec<String>,
}

/// A kind of result people tend to like or not, whatever they search for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trait {
    /// The official site of what Wikidata describes.
    Official,
    /// Encyclopedias and dictionaries.
    Reference,
    /// Forums and question-and-answer sites.
    Forums,
    /// Code hosting, package registries and software docs.
    Code,
    Video,
    Social,
    Shopping,
    News,
    /// Government sites, universities and schools.
    Public,
    /// Among the best-known sites.
    Popular,
    /// Little-known sites.
    Small,
    /// Sites of the searcher's own country.
    Local,
}

/// Every trait, in the order they are listed.
pub const TRAITS: [Trait; 12] = [
    Trait::Official,
    Trait::Reference,
    Trait::Forums,
    Trait::Code,
    Trait::Video,
    Trait::Social,
    Trait::Shopping,
    Trait::News,
    Trait::Public,
    Trait::Popular,
    Trait::Small,
    Trait::Local,
];

impl Trait {
    pub fn as_str(self) -> &'static str {
        match self {
            Trait::Official => "official",
            Trait::Reference => "reference",
            Trait::Forums => "forums",
            Trait::Code => "code",
            Trait::Video => "video",
            Trait::Social => "social",
            Trait::Shopping => "shopping",
            Trait::News => "news",
            Trait::Public => "public",
            Trait::Popular => "popular",
            Trait::Small => "small",
            Trait::Local => "local",
        }
    }

    pub fn parse(name: &str) -> Option<Trait> {
        TRAITS.into_iter().find(|t| t.as_str() == name)
    }

    /// What the pages call it.
    pub fn label(self) -> &'static str {
        match self {
            Trait::Official => "official sites",
            Trait::Reference => "encyclopedias and dictionaries",
            Trait::Forums => "forums and Q&A",
            Trait::Code => "code and software docs",
            Trait::Video => "video",
            Trait::Social => "social media",
            Trait::Shopping => "shops",
            Trait::News => "news sites",
            Trait::Public => "government and schools",
            Trait::Popular => "well-known sites",
            Trait::Small => "small, lesser-known sites",
            Trait::Local => "sites from your country",
        }
    }
}

/// Sites of each kind that are known by name.
const KNOWN: &[(Trait, &[&str])] = &[
    (
        Trait::Reference,
        &[
            "wikipedia.org",
            "wikidata.org",
            "wiktionary.org",
            "wikimedia.org",
            "britannica.com",
            "merriam-webster.com",
            "dictionary.com",
            "cambridge.org",
            "oxfordlearnersdictionaries.com",
        ],
    ),
    (
        Trait::Forums,
        &[
            "reddit.com",
            "stackoverflow.com",
            "stackexchange.com",
            "superuser.com",
            "serverfault.com",
            "askubuntu.com",
            "quora.com",
            "news.ycombinator.com",
            "ycombinator.com",
            "lemmy.world",
            "discourse.org",
        ],
    ),
    (
        Trait::Code,
        &[
            "github.com",
            "gitlab.com",
            "codeberg.org",
            "sourceforge.net",
            "crates.io",
            "docs.rs",
            "npmjs.com",
            "pypi.org",
            "readthedocs.io",
            "rust-lang.org",
            "python.org",
            "mozilla.org",
            "developer.mozilla.org",
            "pkg.go.dev",
        ],
    ),
    (
        Trait::Video,
        &[
            "youtube.com",
            "vimeo.com",
            "twitch.tv",
            "dailymotion.com",
            "tiktok.com",
            "netflix.com",
        ],
    ),
    (
        Trait::Social,
        &[
            "x.com",
            "twitter.com",
            "facebook.com",
            "instagram.com",
            "linkedin.com",
            "pinterest.com",
            "threads.net",
            "bsky.app",
            "tumblr.com",
            "tiktok.com",
            "mastodon.social",
        ],
    ),
    (
        Trait::Shopping,
        &[
            "amazon.com",
            "amazon.co.uk",
            "amazon.de",
            "ebay.com",
            "etsy.com",
            "walmart.com",
            "target.com",
            "bestbuy.com",
            "aliexpress.com",
            "temu.com",
            "shein.com",
        ],
    ),
    (
        Trait::News,
        &[
            "nytimes.com",
            "bbc.com",
            "bbc.co.uk",
            "cnn.com",
            "theguardian.com",
            "reuters.com",
            "apnews.com",
            "washingtonpost.com",
            "foxnews.com",
            "npr.org",
            "wsj.com",
            "bloomberg.com",
            "nbcnews.com",
            "cbsnews.com",
            "aljazeera.com",
            "spiegel.de",
            "lemonde.fr",
        ],
    ),
];
/// [`plumb_core::link_score`] from which a site counts as well known, and
/// below which as little known.
const POPULAR_FROM: f32 = 0.6;
const SMALL_BELOW: f32 = 0.35;
/// Most a liked or disliked trait moves a result, and most all of them
/// together do.
pub const TASTE_MOST: f32 = 0.15;
/// Results with a trait, and without it, seen in edit mode before what was
/// said of them counts.
const TASTE_MIN_SEEN: f32 = 6.0;
/// Results with a trait moved up or down before it counts.
const TASTE_MIN_RATED: f32 = 2.0;
/// Standard errors the difference must clear before a trait counts.
const TASTE_SURE: f32 = 2.0;
/// Results of a page counted as seen in edit mode: those looked at.
pub const JUDGED_PER_PAGE: usize = 10;
/// Searches remembered in [`Judged::pages`].
const MAX_JUDGED_PAGES: usize = 200;
const TASTES_MOST: f32 = 0.2;
/// A trait whose taste moves results at least this much is said to be
/// liked (or disliked) on the pages.
pub const TASTE_SHOWN_FROM: f32 = 0.04;

/// The traits of a site, for a searcher whose country is `home`.
pub fn traits(
    domain: &str,
    official: bool,
    link_score: f32,
    country: Option<&str>,
    home: Option<&str>,
) -> Vec<Trait> {
    let mut found = Vec::new();
    if official {
        found.push(Trait::Official);
    }
    let is = |known: &str| domain == known || domain.ends_with(&format!(".{known}"));
    for (kind, domains) in KNOWN {
        if domains.iter().any(|d| is(d)) && !found.contains(kind) {
            found.push(*kind);
        }
    }
    let public = [
        ".gov", ".mil", ".edu", ".gov.uk", ".ac.uk", ".gc.ca", ".gov.au", ".edu.au",
    ];
    if public.iter().any(|end| domain.ends_with(end)) {
        found.push(Trait::Public);
    }
    if link_score >= POPULAR_FROM {
        found.push(Trait::Popular);
    } else if link_score < SMALL_BELOW {
        found.push(Trait::Small);
    }
    if let (Some(country), Some(home)) = (country, home) {
        if country.eq_ignore_ascii_case(home) {
            found.push(Trait::Local);
        }
    }
    found
}

/// How a searcher feels about results with a trait, from what they did
/// with them in edit mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Taste {
    #[serde(rename = "trait")]
    pub kind: Trait,
    /// Moved up.
    pub liked: f32,
    /// Moved down or hidden.
    pub disliked: f32,
    /// Shown in edit mode, whatever was done with them.
    pub seen: f32,
}

impl Taste {
    /// The score it adds to a result with the trait, up to [`TASTE_MOST`]
    /// either way: how much more (or less) often results with it were moved
    /// up rather than down than results without it, of those `all` seen.
    ///
    /// Most results moved down or hidden are simply not what was searched
    /// for, and every result has some traits, so counting each rating
    /// against the result's traits alone would make every common trait
    /// look disliked. Only a difference from the other results says
    /// anything, and only once it is larger than chance would make it:
    /// [`TASTE_SURE`] standard errors, with enough results either way.
    pub fn score(&self, all: &Judged) -> f32 {
        let seen = self.seen;
        let others = all.seen - seen;
        if seen < TASTE_MIN_SEEN
            || others < TASTE_MIN_SEEN
            || self.liked + self.disliked < TASTE_MIN_RATED
        {
            return 0.0;
        }
        // Up counts 1, down -1, nothing 0: the mean and its variance.
        let mean_and_variance = |liked: f32, disliked: f32, seen: f32| {
            let (liked, disliked) = (liked.clamp(0.0, seen), disliked.clamp(0.0, seen));
            let mean = (liked - disliked) / seen;
            let variance = ((liked + disliked) / seen - mean * mean).max(0.0);
            // Never quite sure from a handful of results.
            (mean, variance.max(0.1) / seen)
        };
        let (with, with_var) = mean_and_variance(self.liked, self.disliked, seen);
        let (without, without_var) =
            mean_and_variance(all.liked - self.liked, all.disliked - self.disliked, others);
        let difference = with - without;
        let sure = TASTE_SURE * (with_var + without_var).sqrt();
        if difference.abs() <= sure {
            return 0.0;
        }
        let beyond = difference.abs() - sure;
        (difference.signum() * beyond * TASTE_MOST * 2.0).clamp(-TASTE_MOST, TASTE_MOST)
    }
}

/// How a result was rated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rating {
    Like,
    Neither,
    Dislike,
}

/// What the searcher said about a result for a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Higher for this search.
    Up,
    /// Lower for this search.
    Down,
    /// Not for this search: left out.
    Hide,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Up => "up",
            Verdict::Down => "down",
            Verdict::Hide => "hide",
        }
    }

    pub fn parse(name: &str) -> Option<Verdict> {
        match name {
            "up" => Some(Verdict::Up),
            "down" => Some(Verdict::Down),
            "hide" => Some(Verdict::Hide),
            _ => None,
        }
    }

    /// The score it adds to the result, for a [`Verdict::Up`] or
    /// [`Verdict::Down`]: enough to move it past most results that match
    /// about as well.
    pub fn score(self) -> f32 {
        match self {
            Verdict::Up => VERDICT_UP,
            Verdict::Down => VERDICT_DOWN,
            Verdict::Hide => 0.0,
        }
    }

    /// What the history page calls it.
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Up => "higher",
            Verdict::Down => "lower",
            Verdict::Hide => "hidden",
        }
    }
}

/// A result's verdict for a search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteVerdict {
    /// The query, as [`query_key`] makes it.
    pub query: String,
    pub domain: String,
    pub verdict: Verdict,
    pub at: u64,
}

/// A box folded or shown by the searcher's own choice: for one search
/// (`q <query>`) or every search showing it the same way (`c <context>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxVerdict {
    pub block: Block,
    pub key: String,
    /// Folded, or else always shown open.
    pub fold: bool,
    pub at: u64,
}

/// What to do with a box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// As always.
    Usual,
    /// Fold it to one line: it is seldom used for searches like this.
    Fold,
    /// Unfold it: it is often used for searches like this.
    Open,
}

/// What makes two searches the same: case, accents and punctuation do not
/// count.
pub fn query_key(query: &str) -> String {
    normalize_text(query)
}

fn words(query: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for word in query_key(query).split(' ').filter(|w| !w.is_empty()) {
        if !words.iter().any(|w| w == word) {
            words.push(word.to_owned());
        }
        if words.len() == MAX_WORDS {
            break;
        }
    }
    words
}

/// The share of shows a box was used, leaning to a half while there is
/// little to go on.
fn rate(used: u32, shown: u32) -> f32 {
    (used as f32 + 0.5) / (shown as f32 + 1.0)
}

impl Learned {
    /// The keys a box shown for `query` in `context` counts under, most
    /// alike first: the query, its words, the context.
    fn keys(query: &str, context: &str) -> (String, Vec<String>, String) {
        (
            format!("q {}", query_key(query)),
            words(query)
                .into_iter()
                .map(|word| format!("w {context} {word}"))
                .collect(),
            format!("c {context}"),
        )
    }

    fn count(&self, block: Block, key: &str) -> Option<&BlockCount> {
        self.blocks
            .iter()
            .find(|c| c.block == block && c.key == key)
    }

    fn bump(&mut self, block: Block, key: String, used: bool, at: u64) {
        match self
            .blocks
            .iter_mut()
            .find(|c| c.block == block && c.key == key)
        {
            Some(count) => {
                if used {
                    count.used = count.used.saturating_add(1);
                } else {
                    count.shown = count.shown.saturating_add(1);
                }
                count.at = at;
            }
            None => self.blocks.push(BlockCount {
                block,
                key,
                shown: u32::from(!used),
                used: u32::from(used),
                at,
            }),
        }
    }

    /// How often `block` was used for searches like `query`, shown in
    /// `context`: the share used, and of how many shows. `None` until
    /// searches alike were seen [`MIN_SHOWN`] times.
    pub fn estimate(&self, block: Block, query: &str, context: &str) -> Option<(f32, u32)> {
        let (exact, words, all) = Self::keys(query, context);
        if let Some(count) = self.count(block, &exact).filter(|c| c.shown >= MIN_SHOWN) {
            return Some((rate(count.used, count.shown), count.shown));
        }
        let alike: Vec<&BlockCount> = words
            .iter()
            .filter_map(|key| self.count(block, key))
            .filter(|c| c.shown >= MIN_SHOWN)
            .collect();
        if !alike.is_empty() {
            let mean =
                alike.iter().map(|c| rate(c.used, c.shown)).sum::<f32>() / alike.len() as f32;
            let shown = alike.iter().map(|c| c.shown).max().unwrap_or_default();
            return Some((mean, shown));
        }
        self.count(block, &all)
            .filter(|c| c.shown >= MIN_SHOWN)
            .map(|c| (rate(c.used, c.shown), c.shown))
    }

    /// What to do with `block` for `query`, shown in `context`: what the
    /// searcher said, else what their clicks say.
    pub fn choice(&self, block: Block, query: &str, context: &str) -> Choice {
        if let Some(fold) = self.box_verdict(block, query, context) {
            return if fold { Choice::Fold } else { Choice::Open };
        }
        match (block, self.estimate(block, query, context)) {
            (Block::Places, Some((rate, _))) if rate < FOLD_BELOW => Choice::Fold,
            (Block::News, Some((rate, _))) if rate >= OPEN_FROM => Choice::Open,
            _ => Choice::Usual,
        }
    }

    /// Notes a results page for `query`: the boxes shown open on it, those
    /// shown folded (not counted as shown), and this node's results, best
    /// first.
    pub fn note_shown(
        &mut self,
        query: &str,
        blocks: &[(Block, &str)],
        folded: &[(Block, &str)],
        sites: &[String],
        at: u64,
    ) {
        let key = query_key(query);
        if key.is_empty() {
            return;
        }
        for &(block, context) in blocks {
            let (exact, words, all) = Self::keys(query, context);
            self.bump(block, exact, false, at);
            for word in words {
                self.bump(block, word, false, at);
            }
            self.bump(block, all, false, at);
        }
        self.pages.insert(
            0,
            ShownPage {
                query: key,
                blocks: blocks
                    .iter()
                    .map(|&(block, context)| (block, context.to_owned(), true))
                    .chain(
                        folded
                            .iter()
                            .map(|&(block, context)| (block, context.to_owned(), false)),
                    )
                    .collect(),
                used: Vec::new(),
                sites: sites.iter().take(SITES_PER_PAGE).cloned().collect(),
                picked: Vec::new(),
                at,
            },
        );
        self.trim();
    }

    /// Notes that a link of `block` was opened from the latest page for
    /// `query`; once per page.
    pub fn note_used(&mut self, query: &str, block: Block, at: u64) {
        let key = query_key(query);
        let Some(page) = self.pages.iter_mut().find(|p| p.query == key) else {
            return;
        };
        if page.used.contains(&block) {
            return;
        }
        page.used.push(block);
        let Some((_, context, counted)) = page.blocks.iter().find(|(b, _, _)| *b == block).cloned()
        else {
            return;
        };
        if !counted {
            // A folded box was not counted as shown: count it now, so it is
            // never used more often than shown.
            let (exact, words, all) = Self::keys(query, &context);
            self.bump(block, exact, false, at);
            for word in words {
                self.bump(block, word, false, at);
            }
            self.bump(block, all, false, at);
        }
        let (exact, words, all) = Self::keys(query, &context);
        self.bump(block, exact, true, at);
        for word in words {
            self.bump(block, word, true, at);
        }
        self.bump(block, all, true, at);
        self.trim();
    }

    /// Notes that `domain` was picked from the latest page for `query`:
    /// the sites above it that are not picked from that page were passed
    /// over, once per page.
    pub fn note_picked(&mut self, query: &str, domain: &str, at: u64) {
        let key = query_key(query);
        let Some(page) = self.pages.iter_mut().find(|p| p.query == key) else {
            return;
        };
        if page.picked.iter().any(|d| d == domain) {
            return;
        }
        let first_pick = page.picked.is_empty();
        page.picked.push(domain.to_owned());
        let passed: Vec<String> = match page.sites.iter().position(|d| d == domain) {
            Some(at) if first_pick => page.sites[..at].to_vec(),
            _ => Vec::new(),
        };
        self.site(domain, at).picked += 1;
        for domain in passed {
            let count = self.site(&domain, at);
            count.passed = count.passed.saturating_add(1);
        }
        self.trim();
    }

    /// Where `domain` was on the latest page for `query` (0 for the first
    /// result), and whether it was already picked from that page.
    pub fn place(&self, query: &str, domain: &str) -> Option<(usize, bool)> {
        let key = query_key(query);
        let page = self.pages.iter().find(|p| p.query == key)?;
        let at = page.sites.iter().position(|d| d == domain)?;
        Some((at, page.picked.iter().any(|d| d == domain)))
    }

    fn site(&mut self, domain: &str, at: u64) -> &mut SiteCount {
        let i = match self.sites.iter().position(|s| s.domain == domain) {
            Some(i) => i,
            None => {
                self.sites.push(SiteCount {
                    domain: domain.to_owned(),
                    picked: 0,
                    passed: 0,
                    at,
                });
                self.sites.len() - 1
            }
        };
        let count = &mut self.sites[i];
        count.at = at;
        count
    }

    /// The score `domain` loses for being passed over, never picked: 0 or
    /// less.
    pub fn passed_over(&self, domain: &str) -> f32 {
        let Some(count) = self.sites.iter().find(|s| s.domain == domain) else {
            return 0.0;
        };
        if count.picked > 0 || count.passed < PASSED_BEFORE_DOWN {
            return 0.0;
        }
        let steps = (count.passed - PASSED_BEFORE_DOWN + 1) as f32;
        -(steps * PASSED_STEP).min(PASSED_MOST)
    }

    /// Sites moved down for being passed over, most passed first.
    pub fn moved_down(&self) -> Vec<&SiteCount> {
        let mut sites: Vec<&SiteCount> = self
            .sites
            .iter()
            .filter(|s| self.passed_over(&s.domain) < 0.0)
            .collect();
        sites.sort_by_key(|s| std::cmp::Reverse(s.passed));
        sites
    }

    /// Searches (and words, and kinds of search) a box is folded or
    /// unfolded for, most seen first.
    pub fn decided(&self) -> Vec<(&BlockCount, Choice)> {
        let mut decided: Vec<(&BlockCount, Choice)> = self
            .blocks
            .iter()
            .filter(|c| c.shown >= MIN_SHOWN)
            .filter_map(|c| {
                let rate = rate(c.used, c.shown);
                match c.block {
                    Block::Places if rate < FOLD_BELOW => Some((c, Choice::Fold)),
                    Block::News if rate >= OPEN_FROM => Some((c, Choice::Open)),
                    _ => None,
                }
            })
            .collect();
        decided.sort_by_key(|d| std::cmp::Reverse(d.0.shown));
        decided
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
            && self.sites.is_empty()
            && self.verdicts.is_empty()
            && self.boxes.is_empty()
            && self.tastes.is_empty()
    }

    /// The count every trait is compared with, starting it (and dropping
    /// tastes counted the old way) when there is none.
    fn judged(&mut self) -> &mut Judged {
        if self.judged.is_none() {
            self.tastes.clear();
        }
        self.judged.get_or_insert_with(Judged::default)
    }

    fn taste(&mut self, kind: Trait) -> &mut Taste {
        let i = match self.tastes.iter().position(|t| t.kind == kind) {
            Some(i) => i,
            None => {
                self.tastes.push(Taste {
                    kind,
                    liked: 0.0,
                    disliked: 0.0,
                    seen: 0.0,
                });
                self.tastes.len() - 1
            }
        };
        &mut self.tastes[i]
    }

    /// Notes the results of a page shown in edit mode for `query`, each by
    /// its traits, once for each search.
    pub fn note_judged(&mut self, query: &str, results: &[Vec<Trait>]) {
        let key = query_key(query);
        let judged = self.judged();
        if judged.pages.contains(&key) {
            return;
        }
        judged.pages.insert(0, key);
        judged.pages.truncate(MAX_JUDGED_PAGES);
        judged.seen += results.len() as f32;
        for traits in results {
            for &kind in traits {
                self.taste(kind).seen += 1.0;
            }
        }
    }

    /// Notes a rating of a result with `traits`, worth `weight` ratings (a
    /// negative weight takes one back).
    pub fn rate(&mut self, traits: &[Trait], rating: Rating, weight: f32) {
        let add = |liked: &mut f32, disliked: &mut f32| match rating {
            Rating::Like => *liked = (*liked + weight).max(0.0),
            Rating::Dislike => *disliked = (*disliked + weight).max(0.0),
            Rating::Neither => {}
        };
        let judged = self.judged();
        add(&mut judged.liked, &mut judged.disliked);
        for &kind in traits {
            let taste = self.taste(kind);
            add(&mut taste.liked, &mut taste.disliked);
        }
    }

    /// Each trait's score, see [`Taste::score`].
    fn taste_scores(&self) -> impl Iterator<Item = (f32, Trait)> + '_ {
        self.judged.iter().flat_map(move |all| {
            self.tastes
                .iter()
                .map(move |taste| (taste.score(all), taste.kind))
        })
    }

    /// The score a result with `traits` gets for the searcher's tastes,
    /// and the liked trait that counts most, if one counts.
    pub fn taste_score(&self, traits: &[Trait]) -> (f32, Option<Trait>) {
        let mut total = 0.0f32;
        let mut best: Option<(f32, Trait)> = None;
        for (score, kind) in self.taste_scores().filter(|(_, k)| traits.contains(k)) {
            total += score;
            if score >= TASTE_SHOWN_FROM && best.is_none_or(|(b, _)| score > b) {
                best = Some((score, kind));
            }
        }
        (
            total.clamp(-TASTES_MOST, TASTES_MOST),
            best.map(|(_, kind)| kind),
        )
    }

    /// The traits liked and disliked enough to say so, strongest first.
    pub fn leanings(&self) -> (Vec<Trait>, Vec<Trait>) {
        let mut tastes: Vec<(f32, Trait)> = self.taste_scores().collect();
        tastes.sort_by(|a, b| b.0.total_cmp(&a.0));
        let liked = tastes
            .iter()
            .filter(|(s, _)| *s >= TASTE_SHOWN_FROM)
            .map(|(_, k)| *k)
            .collect();
        let disliked = tastes
            .iter()
            .rev()
            .filter(|(s, _)| *s <= -TASTE_SHOWN_FROM)
            .map(|(_, k)| *k)
            .collect();
        (liked, disliked)
    }

    /// Notes that `block`, shown in `context`, was useful or not on the
    /// tuning page: counts as that many shows for every search showing it
    /// that way.
    pub fn rate_block(&mut self, block: Block, context: &str, useful: bool, at: u64) {
        let key = format!("c {context}");
        self.bump(block, key.clone(), false, at);
        if useful {
            self.bump(block, key, true, at);
        }
    }

    /// What the searcher said about `domain` for `query`.
    pub fn verdict(&self, query: &str, domain: &str) -> Option<Verdict> {
        let key = query_key(query);
        self.verdicts
            .iter()
            .find(|v| v.domain == domain && v.query == key)
            .map(|v| v.verdict)
    }

    /// Every verdict for `query`, by domain.
    pub fn verdicts_for(&self, query: &str) -> Vec<(String, Verdict)> {
        let key = query_key(query);
        self.verdicts
            .iter()
            .filter(|v| v.query == key)
            .map(|v| (v.domain.clone(), v.verdict))
            .collect()
    }

    /// Says `verdict` about `domain` for `query`; `None` takes it back.
    pub fn set_verdict(&mut self, query: &str, domain: &str, verdict: Option<Verdict>, at: u64) {
        let key = query_key(query);
        if key.is_empty() || domain.is_empty() {
            return;
        }
        self.verdicts
            .retain(|v| !(v.domain == domain && v.query == key));
        if let Some(verdict) = verdict {
            self.verdicts.insert(
                0,
                SiteVerdict {
                    query: key,
                    domain: domain.to_owned(),
                    verdict,
                    at,
                },
            );
            self.verdicts.truncate(MAX_VERDICTS);
        }
    }

    /// The searcher's own choice for `block` on `query`, shown in
    /// `context`: folded (`true`) or open; for this search first.
    pub fn box_verdict(&self, block: Block, query: &str, context: &str) -> Option<bool> {
        let exact = format!("q {}", query_key(query));
        let all = format!("c {context}");
        [exact, all].iter().find_map(|key| {
            self.boxes
                .iter()
                .find(|b| b.block == block && &b.key == key)
                .map(|b| b.fold)
        })
    }

    /// Folds `block` (or, with `fold` false, always shows it open) for
    /// `query`, or with `every` for every search showing it in `context`;
    /// `None` takes back what was said for this search, and for every
    /// search too when `every`.
    pub fn set_box(
        &mut self,
        block: Block,
        query: &str,
        context: &str,
        every: bool,
        fold: Option<bool>,
        at: u64,
    ) {
        let key = if every {
            format!("c {context}")
        } else {
            format!("q {}", query_key(query))
        };
        if key == "q " {
            return;
        }
        let exact = format!("q {}", query_key(query));
        self.boxes.retain(|b| {
            !(b.block == block && (b.key == key || (fold.is_none() && b.key == exact)))
        });
        if let Some(fold) = fold {
            self.boxes.insert(
                0,
                BoxVerdict {
                    block,
                    key,
                    fold,
                    at,
                },
            );
            self.boxes.truncate(MAX_VERDICTS);
        }
    }

    fn trim(&mut self) {
        self.pages.truncate(MAX_SHOWN);
        if self.blocks.len() > MAX_BLOCK_COUNTS {
            self.blocks.sort_by_key(|c| std::cmp::Reverse(c.at));
            self.blocks.truncate(MAX_BLOCK_COUNTS);
        }
        if self.sites.len() > MAX_SITE_COUNTS {
            self.sites.sort_by_key(|c| std::cmp::Reverse(c.at));
            self.sites.truncate(MAX_SITE_COUNTS);
        }
    }
}

/// A key of a [`BlockCount`] in words, for the history page.
pub fn describe_key(key: &str) -> String {
    let context = |c: &str| match c {
        "guessed" => "searches without \"in\" or \"near\"",
        "said" => "searches that say where",
        "site" => "a site's latest posts",
        "words" => "headlines about a search's words",
        _ => "other searches",
    };
    if let Some(query) = key.strip_prefix("q ") {
        return format!("\u{201c}{query}\u{201d}");
    }
    if let Some(rest) = key.strip_prefix("w ") {
        if let Some((c, word)) = rest.split_once(' ') {
            return format!("{} with \u{201c}{word}\u{201d}", context(c));
        }
    }
    if let Some(c) = key.strip_prefix("c ") {
        return format!("all {}", context(c));
    }
    key.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show_places(learned: &mut Learned, query: &str, guessed: bool, at: u64) {
        let sites = vec!["usbank.com".to_owned(), "bank.example".to_owned()];
        let shown = [(Block::Places, places_context(guessed))];
        match learned.choice(Block::Places, query, places_context(guessed)) {
            Choice::Fold => learned.note_shown(query, &[], &shown, &sites, at),
            _ => learned.note_shown(query, &shown, &[], &sites, at),
        }
    }

    #[test]
    fn a_box_never_used_is_folded_and_one_use_brings_it_back() {
        let mut learned = Learned::default();
        for at in 0..2 {
            show_places(&mut learned, "us bank", true, at);
        }
        assert_eq!(
            learned.choice(Block::Places, "us bank", "guessed"),
            Choice::Usual
        );
        show_places(&mut learned, "US  Bank", true, 3);
        assert_eq!(
            learned.choice(Block::Places, "us bank", "guessed"),
            Choice::Fold
        );
        // Shown folded: not counted, so still folded.
        show_places(&mut learned, "us bank", true, 4);
        assert_eq!(
            learned.choice(Block::Places, "us bank", "guessed"),
            Choice::Fold
        );
        // Opened from the folded box: shown, and used.
        learned.note_used("us bank", Block::Places, 5);
        assert_eq!(
            learned.choice(Block::Places, "us bank", "guessed"),
            Choice::Usual
        );
    }

    #[test]
    fn alike_searches_count_and_said_where_counts_apart() {
        let mut learned = Learned::default();
        for at in 0..3 {
            show_places(&mut learned, "us bank", true, at);
        }
        // Shares "bank", shown the same way.
        assert_eq!(
            learned.choice(Block::Places, "denver bank", "guessed"),
            Choice::Fold
        );
        // Said where: never seen.
        assert_eq!(
            learned.choice(Block::Places, "bank in denver", "said"),
            Choice::Usual
        );
        // Used for pizza in denver every time.
        for at in 0..3 {
            show_places(&mut learned, "pizza in denver", false, at);
            learned.note_used("pizza in denver", Block::Places, at);
        }
        assert_eq!(
            learned.choice(Block::Places, "pizza in denver", "said"),
            Choice::Usual
        );
        assert_eq!(
            learned.choice(Block::Places, "tacos in boulder", "said"),
            Choice::Usual
        );
    }

    #[test]
    fn a_box_counts_one_use_per_page() {
        let mut learned = Learned::default();
        for at in 0..3 {
            learned.note_shown("rust", &[(Block::News, "words")], &[], &[], at);
            learned.note_used("rust", Block::News, at);
            learned.note_used("rust", Block::News, at);
        }
        let count = learned.count(Block::News, "q rust").unwrap();
        assert_eq!((count.shown, count.used), (3, 3));
        assert_eq!(learned.choice(Block::News, "rust", "words"), Choice::Open);
        assert_eq!(learned.decided().len(), 3);
    }

    #[test]
    fn sites_passed_over_move_down_until_picked() {
        let mut learned = Learned::default();
        let sites: Vec<String> = ["spam.example", "good.example", "other.example"]
            .map(String::from)
            .to_vec();
        for at in 0..2 {
            learned.note_shown("thing", &[], &[], &sites, at);
            learned.note_picked("thing", "good.example", at);
            // A second pick from the same page passes nothing more.
            learned.note_picked("thing", "other.example", at);
        }
        assert_eq!(learned.passed_over("spam.example"), 0.0);
        learned.note_shown("thing", &[], &[], &sites, 3);
        learned.note_picked("thing", "good.example", 3);
        assert!(learned.passed_over("spam.example") < 0.0);
        assert_eq!(learned.passed_over("good.example"), 0.0);
        assert_eq!(learned.passed_over("other.example"), 0.0);
        assert_eq!(learned.moved_down().len(), 1);
        for at in 4..20 {
            learned.note_shown("thing", &[], &[], &sites, at);
            learned.note_picked("thing", "good.example", at);
        }
        assert!(learned.passed_over("spam.example") >= -PASSED_MOST);
        learned.note_shown("thing", &[], &[], &sites, 30);
        learned.note_picked("thing", "spam.example", 30);
        assert_eq!(learned.passed_over("spam.example"), 0.0);
    }

    #[test]
    fn clicks_without_a_page_shown_are_ignored() {
        let mut learned = Learned::default();
        learned.note_used("us bank", Block::Places, 1);
        learned.note_picked("us bank", "usbank.com", 1);
        assert!(learned.is_empty());
    }

    #[test]
    fn verdicts_are_per_search_and_can_be_taken_back() {
        let mut learned = Learned::default();
        learned.set_verdict("US Bank", "spam.example", Some(Verdict::Hide), 1);
        learned.set_verdict("us bank", "usbank.com", Some(Verdict::Up), 1);
        assert_eq!(
            learned.verdict("us  bank", "spam.example"),
            Some(Verdict::Hide)
        );
        assert_eq!(learned.verdict("bank", "spam.example"), None);
        assert_eq!(learned.verdicts_for("us bank").len(), 2);
        learned.set_verdict("us bank", "spam.example", Some(Verdict::Down), 2);
        assert_eq!(
            learned.verdict("us bank", "spam.example"),
            Some(Verdict::Down)
        );
        learned.set_verdict("us bank", "spam.example", None, 3);
        assert_eq!(learned.verdict("us bank", "spam.example"), None);
        assert_eq!(learned.verdicts.len(), 1);
    }

    #[test]
    fn box_verdicts_win_over_clicks() {
        let mut learned = Learned::default();
        // Used every time, but the searcher says fold it here.
        for at in 0..3 {
            show_places(&mut learned, "pizza in denver", false, at);
            learned.note_used("pizza in denver", Block::Places, at);
        }
        learned.set_box(
            Block::Places,
            "pizza in denver",
            "said",
            false,
            Some(true),
            4,
        );
        assert_eq!(
            learned.choice(Block::Places, "pizza in denver", "said"),
            Choice::Fold
        );
        assert_eq!(
            learned.choice(Block::Places, "tacos in denver", "said"),
            Choice::Usual
        );
        // Folded for every guessed town.
        learned.set_box(Block::Places, "denver bank", "guessed", true, Some(true), 5);
        assert_eq!(
            learned.choice(Block::Places, "boulder bank", "guessed"),
            Choice::Fold
        );
        // Taking back "every" also takes back this search's.
        learned.set_box(Block::Places, "pizza in denver", "said", true, None, 6);
        assert_eq!(
            learned.choice(Block::Places, "pizza in denver", "said"),
            Choice::Usual
        );
        learned.set_box(Block::News, "rust", "words", false, Some(false), 7);
        assert_eq!(learned.choice(Block::News, "rust", "words"), Choice::Open);
    }

    #[test]
    fn traits_say_what_kind_of_site_it_is() {
        let t = traits("en.wikipedia.org", false, 0.9, None, Some("US"));
        assert_eq!(t, [Trait::Reference, Trait::Popular]);
        let t = traits("irs.gov", true, 0.5, Some("US"), Some("US"));
        assert_eq!(t, [Trait::Official, Trait::Public, Trait::Local]);
        let t = traits("myblog.example", false, 0.1, Some("DE"), Some("US"));
        assert_eq!(t, [Trait::Small]);
        assert!(!traits("notgithub.com", false, 0.5, None, None).contains(&Trait::Code));
    }

    #[test]
    fn tastes_move_results_with_their_traits() {
        let mut learned = Learned::default();
        assert_eq!(learned.taste_score(&[Trait::Code]), (0.0, None));
        // Six searches of ten results: two code sites, one social site,
        // seven others, popular and small alike.
        let page = |n: usize| match n {
            0 | 1 => vec![Trait::Code, Trait::Popular],
            2 => vec![Trait::Social, Trait::Popular],
            n if n % 2 == 0 => vec![Trait::Popular],
            _ => vec![Trait::Small],
        };
        for search in 0..6 {
            let results: Vec<Vec<Trait>> = (0..10).map(page).collect();
            learned.note_judged(&format!("search {search}"), &results);
            // Reloaded after each button: counted once.
            learned.note_judged(&format!("search {search}"), &results);
            learned.rate(&page(0), Rating::Like, 1.0);
            learned.rate(&page(2), Rating::Dislike, 1.0);
            // Something not searched for, of every kind.
            learned.rate(&page(3 + search), Rating::Dislike, 1.0);
        }
        assert_eq!(learned.judged.as_ref().unwrap().seen, 60.0);
        let (code, why) = learned.taste_score(&[Trait::Code]);
        assert!(code > 0.0 && code <= TASTE_MOST, "{code}");
        assert_eq!(why, Some(Trait::Code));
        let (social, why) = learned.taste_score(&[Trait::Social]);
        assert!(social < 0.0, "{social}");
        assert_eq!(why, None);
        // Moved down about as often as the rest: nothing to say.
        assert_eq!(learned.taste_score(&[Trait::Popular]).0, 0.0);
        assert_eq!(learned.taste_score(&[Trait::Small]).0, 0.0);
        assert_eq!(learned.taste_score(&[Trait::Video]).0, 0.0);
        let (liked, disliked) = learned.leanings();
        assert_eq!(liked, [Trait::Code]);
        assert_eq!(disliked, [Trait::Social]);
        assert_eq!(Trait::parse("code"), Some(Trait::Code));
    }

    #[test]
    fn a_few_results_moved_down_say_nothing_about_their_kind() {
        // What Liz saw: small, well-known and official sites all
        // "disliked" after hiding a few results that were not what she
        // searched for.
        let mut learned = Learned::default();
        let results = [
            vec![Trait::Official, Trait::Popular, Trait::Local],
            vec![Trait::Reference, Trait::Popular],
            vec![Trait::Small, Trait::Local],
            vec![Trait::Small],
            vec![Trait::Official, Trait::Small, Trait::Local],
            vec![Trait::Popular, Trait::Local],
        ];
        for search in 0..4 {
            learned.note_judged(&format!("search {search}"), &results);
        }
        learned.rate(&results[0], Rating::Dislike, 1.0);
        learned.rate(&results[3], Rating::Dislike, 1.0);
        learned.rate(&results[4], Rating::Dislike, 1.0);
        learned.rate(&results[5], Rating::Like, 1.0);
        assert_eq!(learned.leanings(), (vec![], vec![]));
        // Taken back: counts as never said.
        learned.rate(&results[5], Rating::Like, -1.0);
        assert_eq!(learned.judged.as_ref().unwrap().liked, 0.0);
    }

    #[test]
    fn tastes_counted_the_old_way_are_dropped() {
        let mut learned: Learned = serde_json::from_str(
            r#"{"tastes":[{"trait":"small","liked":0,"disliked":3,"seen":3}]}"#,
        )
        .unwrap();
        assert_eq!(learned.taste_score(&[Trait::Small]).0, 0.0);
        learned.note_judged("q", &[vec![Trait::Code]]);
        assert_eq!(learned.tastes.len(), 1);
        assert_eq!(learned.tastes[0].kind, Trait::Code);
    }

    #[test]
    fn a_box_rated_useless_folds_everywhere_it_shows_that_way() {
        let mut learned = Learned::default();
        for at in 0..3 {
            learned.rate_block(Block::Places, "guessed", false, at);
        }
        assert_eq!(
            learned.choice(Block::Places, "denver bank", "guessed"),
            Choice::Fold
        );
        assert_eq!(
            learned.choice(Block::Places, "pizza in denver", "said"),
            Choice::Usual
        );
    }

    #[test]
    fn counts_are_capped() {
        let mut learned = Learned::default();
        for n in 0..300u64 {
            let query = format!("query{n} word{n}");
            learned.note_shown(
                &query,
                &[(Block::Places, "said")],
                &[],
                &[format!("s{n}.example")],
                n,
            );
            learned.note_picked(&query, &format!("s{n}.example"), n);
        }
        assert!(learned.blocks.len() <= MAX_BLOCK_COUNTS);
        assert!(learned.pages.len() <= MAX_SHOWN);
        assert!(learned.sites.len() <= MAX_SITE_COUNTS);
    }
}
