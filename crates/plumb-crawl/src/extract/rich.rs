//! Bounded sidecar for the streaming extractor; never builds an HTML tree.

use html5ever::tokenizer::Tag;
use plumb_core::article::{
    search_anchor, valid_identifier, SearchContent, SearchPassage, SearchSymbol,
    MAX_SEARCH_IDENTIFIER_BYTES, MAX_SEARCH_PASSAGES, MAX_SEARCH_PASSAGE_CHARS, MAX_SEARCH_SYMBOLS,
};

use super::{attr, CHROME_ELEMENTS, HIDDEN_ELEMENTS, WORD_BREAK_ELEMENTS};

const MAX_DEPTH: usize = 256;
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];
const BLOCKS: &[&str] = &[
    "h1", "h2", "h3", "h4", "h5", "h6", "p", "pre", "dt", "dd", "li", "th", "td",
];

struct Frame {
    name: String,
    hidden: bool,
    code: bool,
    definition_name: bool,
    foreign: bool,
    anchor: Option<String>,
}

#[derive(Default)]
struct Section {
    heading: String,
    anchor: Option<String>,
    depth: usize,
}

struct Block {
    name: String,
    passage: SearchPassage,
    chars: usize,
    distinctive: bool,
    definition: bool,
}

/// At most 256 small frames, one 512-character block and eight candidate
/// passages. Excessive nesting fails closed for rich extraction. The
/// compact extractor continues to read under its existing time limit.
#[derive(Default)]
pub(super) struct RichText {
    frames: Vec<Frame>,
    overflow: usize,
    section: Section,
    block: Option<Block>,
    word: String,
    long_word: bool,
    symbols: Vec<(u64, SearchSymbol)>,
    passages: Vec<(u64, SearchPassage)>,
}

impl RichText {
    fn hidden(&self) -> bool {
        self.overflow > 0 || self.frames.last().is_some_and(|f| f.hidden)
    }

    fn anchor(&self) -> Option<String> {
        self.frames
            .iter()
            .enumerate()
            .rev()
            .find_map(|(depth, frame)| {
                (depth >= self.section.depth)
                    .then(|| frame.anchor.clone())
                    .flatten()
            })
            .or_else(|| self.section.anchor.clone())
    }

    pub(super) fn start(&mut self, tag: &Tag) {
        let name: &str = &tag.name;
        if self.overflow > 0 {
            if !VOID.contains(&name) {
                self.overflow = self.overflow.saturating_add(1);
            }
            return;
        }
        if WORD_BREAK_ELEMENTS.contains(&name)
            || matches!(name, "a" | "code" | "samp" | "kbd" | "strong")
            || attr(tag, "id").is_some()
        {
            self.close_word();
        }
        if BLOCKS.contains(&name) {
            self.close_block();
        }
        let hidden = self.hidden()
            || HIDDEN_ELEMENTS.contains(&name)
            || CHROME_ELEMENTS.contains(&name)
            || matches!(name, "head" | "title" | "svg" | "math")
            || attr(tag, "hidden").is_some()
            || attr(tag, "aria-hidden").is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
            || attr(tag, "role").is_some_and(|roles| {
                roles.split_whitespace().any(|role| {
                    [
                        "navigation",
                        "menu",
                        "menubar",
                        "banner",
                        "complementary",
                        "contentinfo",
                        "search",
                    ]
                    .iter()
                    .any(|wanted| role.eq_ignore_ascii_case(wanted))
                })
            })
            || attr(tag, "style").is_some_and(hidden_style);
        let anchor = (!hidden)
            .then(|| {
                attr(tag, "id")
                    .or_else(|| {
                        (name == "a")
                            .then(|| {
                                attr(tag, "name").or_else(|| {
                                    attr(tag, "href").and_then(|href| href.strip_prefix('#'))
                                })
                            })
                            .flatten()
                    })
                    .and_then(search_anchor)
            })
            .flatten();
        let foreign =
            matches!(name, "svg" | "math") || self.frames.last().is_some_and(|f| f.foreign);
        // HTML ignores a self-closing slash on ordinary elements, but it
        // closes SVG/MathML elements. An icon must not hide later prose.
        if !(VOID.contains(&name) || foreign && tag.self_closing) {
            if self.frames.len() == MAX_DEPTH {
                self.overflow = 1;
                return;
            }
            let code = matches!(name, "code" | "pre" | "samp" | "kbd")
                || self.frames.last().is_some_and(|f| f.code);
            let definition_name = self.block.as_ref().is_some_and(|block| block.definition)
                && (matches!(name, "strong" | "code")
                    || attr(tag, "class").is_some_and(|classes| {
                        classes
                            .split_whitespace()
                            .any(|class| matches!(class, "sig-name" | "descname"))
                    }));
            if definition_name {
                self.close_word();
            }
            self.frames.push(Frame {
                name: name.chars().take(64).collect(),
                hidden,
                code,
                definition_name,
                foreign,
                anchor: anchor.clone(),
            });
        }
        if hidden {
            return;
        }
        if let Some(anchor) = &anchor {
            // Some generators put the qualified name directly in the ID.
            if distinctive(anchor) {
                self.add_symbol(anchor, false, false);
            }
        }
        if BLOCKS.contains(&name) {
            let heading = name.starts_with('h') && name.len() == 2;
            self.block = Some(Block {
                name: name.to_string(),
                passage: SearchPassage {
                    heading: if heading {
                        String::new()
                    } else {
                        self.section.heading.clone()
                    },
                    text: String::new(),
                    anchor: anchor.or_else(|| self.anchor()),
                },
                chars: 0,
                distinctive: false,
                definition: name == "dt"
                    || attr(tag, "class").is_some_and(|classes| {
                        classes
                            .split_whitespace()
                            .any(|class| matches!(class, "classref-method" | "sig"))
                    }),
            });
        } else if WORD_BREAK_ELEMENTS.contains(&name) {
            self.space();
        }
    }

    pub(super) fn end(&mut self, tag: &Tag) {
        let name: &str = &tag.name;
        if self.overflow > 0 {
            if !VOID.contains(&name) {
                self.overflow -= 1;
            }
            return;
        }
        // Unmatched closing tags cannot reopen an excluded subtree.
        let Some(at) = self.frames.iter().rposition(|f| f.name == name) else {
            return;
        };
        if WORD_BREAK_ELEMENTS.contains(&name)
            || matches!(name, "a" | "code" | "samp" | "kbd" | "strong")
            || self.frames[at].definition_name
        {
            self.close_word();
        }
        if self.block.as_ref().is_some_and(|block| block.name == name) {
            self.close_block();
        }
        self.frames.truncate(at);
        if WORD_BREAK_ELEMENTS.contains(&name) && !self.hidden() {
            self.space();
        }
    }

    pub(super) fn text(&mut self, text: &str) {
        if self.hidden() {
            return;
        }
        for c in text.chars() {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':') {
                if self.word.len() < MAX_SEARCH_IDENTIFIER_BYTES {
                    self.word.push(c);
                } else {
                    self.long_word = true;
                }
            } else {
                self.close_word();
            }
            if let Some(block) = &mut self.block {
                if block.chars < MAX_SEARCH_PASSAGE_CHARS {
                    if c.is_whitespace() {
                        if !block.passage.text.ends_with(' ') && !block.passage.text.is_empty() {
                            block.passage.text.push(' ');
                            block.chars += 1;
                        }
                    } else {
                        block.passage.text.push(c);
                        block.chars += 1;
                    }
                }
            }
        }
    }

    fn space(&mut self) {
        if let Some(block) = &mut self.block {
            if block.chars < MAX_SEARCH_PASSAGE_CHARS
                && !block.passage.text.ends_with(' ')
                && !block.passage.text.is_empty()
            {
                block.passage.text.push(' ');
                block.chars += 1;
            }
        }
    }

    fn close_word(&mut self) {
        let word = std::mem::take(&mut self.word);
        let long = std::mem::take(&mut self.long_word);
        if long || self.hidden() {
            return;
        }
        let word = word.trim_end_matches(['.', ':']);
        let code = self.frames.last().is_some_and(|f| f.code);
        let definition = self.frames.iter().any(|f| f.definition_name);
        if distinctive(word) || code || definition {
            self.add_symbol(word, code, definition);
        }
    }

    fn add_symbol(&mut self, identifier: &str, code: bool, definition: bool) {
        if !valid_identifier(identifier) || identifier.len() < 3 {
            return;
        }
        let distinctive = distinctive(identifier);
        if let Some(block) = &mut self.block {
            block.distinctive |= distinctive;
        }
        let symbol = SearchSymbol {
            identifier: identifier.to_string(),
            anchor: self.anchor(),
        };
        // Explicit signature names outrank incidental references. Length
        // is not evidence of importance: long constants must not crowd out
        // shorter API names. Stable hashing samples peers across the page.
        let priority = if definition && distinctive {
            4
        } else if definition {
            3
        } else if distinctive {
            2
        } else if code {
            1
        } else {
            0
        };
        let rank = (priority << 48) | (stable_hash(identifier) & ((1 << 48) - 1));
        if let Some((prior_rank, prior)) = self
            .symbols
            .iter_mut()
            .find(|(_, s)| s.identifier == identifier)
        {
            // A later definition replaces an earlier mention's destination.
            // Other mentions share one slot, retaining the first known anchor.
            if rank > *prior_rank {
                *prior_rank = rank;
                *prior = symbol;
            } else if prior.anchor.is_none() {
                prior.anchor = symbol.anchor;
            }
            return;
        }
        keep_best(&mut self.symbols, (rank, symbol), MAX_SEARCH_SYMBOLS);
    }

    fn close_block(&mut self) {
        self.close_word();
        let Some(mut block) = self.block.take() else {
            return;
        };
        block.passage.text = block
            .passage
            .text
            .trim()
            .trim_end_matches('¶')
            .trim()
            .to_string();
        if block.passage.text.is_empty() {
            return;
        }
        let heading = block.name.starts_with('h') && block.name.len() == 2;
        if heading {
            self.section = Section {
                heading: plumb_core::truncate_chars(
                    &block.passage.text,
                    plumb_core::article::MAX_SEARCH_HEADING_CHARS,
                ),
                anchor: block.passage.anchor.clone(),
                depth: self.frames.len().saturating_sub(1),
            };
            block.passage.heading = self.section.heading.clone();
        } else if block.definition && block.passage.anchor.is_some() {
            // Sphinx/Godot put the owning ID on the signature; its following
            // description belongs to the same definition until the next
            // signature or heading. Keep that destination with the prose.
            self.section.anchor = block.passage.anchor.clone();
            self.section.depth = self.frames.len().saturating_sub(1);
        }
        let priority = u64::from(block.distinctive) * 4
            + u64::from(matches!(block.name.as_str(), "pre" | "dt" | "dd")) * 2
            + u64::from(!heading && block.passage.text.len() >= 40);
        let rank = (priority << 56)
            | (stable_hash(&format!("{}{}", block.passage.heading, block.passage.text))
                & ((1 << 56) - 1));
        // Prefer one informative window per anchored section. Plain
        // unanchored paragraphs remain independently eligible.
        if let Some(at) = self
            .passages
            .iter()
            .position(|(_, p)| p.anchor.is_some() && p.anchor == block.passage.anchor)
        {
            let prior = &mut self.passages[at];
            let combined = prior.1.text.chars().count() + 1 + block.passage.text.chars().count();
            if prior.1.heading == block.passage.heading
                && prior.1.text != block.passage.text
                && combined <= MAX_SEARCH_PASSAGE_CHARS
            {
                prior.1.text.push(' ');
                prior.1.text.push_str(&block.passage.text);
                prior.0 = prior.0.max(rank);
            } else if rank > prior.0 {
                self.passages[at] = (rank, block.passage);
            }
        } else {
            keep_best(
                &mut self.passages,
                (rank, block.passage),
                MAX_SEARCH_PASSAGES,
            );
        }
    }

    pub(super) fn finish(mut self) -> Option<SearchContent> {
        self.close_block();
        self.close_word();
        self.symbols
            .sort_by_key(|(rank, _)| std::cmp::Reverse(*rank));
        self.passages
            .sort_by_key(|(rank, _)| std::cmp::Reverse(*rank));
        SearchContent {
            symbols: self.symbols.into_iter().map(|(_, s)| s).collect(),
            passages: self.passages.into_iter().map(|(_, p)| p).collect(),
            ..SearchContent::default()
        }
        .bounded()
    }
}

fn keep_best<T>(values: &mut Vec<(u64, T)>, value: (u64, T), limit: usize) {
    if values.len() < limit {
        values.push(value);
    } else if let Some((at, (rank, _))) =
        values.iter().enumerate().min_by_key(|(_, (rank, _))| *rank)
    {
        if value.0 > *rank {
            values[at] = value;
        }
    }
}

fn distinctive(word: &str) -> bool {
    word.contains(['_', '.'])
        || word.contains("::")
        || word
            .bytes()
            .zip(word.bytes().skip(1))
            .any(|(a, b)| a.is_ascii_lowercase() && b.is_ascii_uppercase())
}

fn hidden_style(style: &str) -> bool {
    style
        .split(';')
        .filter_map(|part| part.split_once(':'))
        .any(|(key, value)| {
            let key = key.trim();
            let value = value.split('!').next().unwrap_or(value).trim();
            (key.eq_ignore_ascii_case("display") && value.eq_ignore_ascii_case("none"))
                || (key.eq_ignore_ascii_case("visibility") && value.eq_ignore_ascii_case("hidden"))
        })
}

fn stable_hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}
