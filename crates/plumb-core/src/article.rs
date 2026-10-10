//! Single pages shown as results next to sites, starting with Wikipedia
//! articles: "marie curie" finds en.wikipedia.org/wiki/Marie_Curie.
//!
//! An article is kept small, about 100 bytes: its title, the names that
//! redirect to it, Wikipedia's one-line description, its Wikidata item,
//! the official website of that item when it has one, and how often it
//! was read. No article text is kept: Plumb finds pages by name.
//!
//! Articles travel as a tab-separated file, most read first, so the top
//! `N` are its first `N` lines:
//!
//! ```text
//! views  title  description  item  site  aliases     (tab-separated)
//! 81234  Marie Curie  Polish-French physicist ...  Q7186  (no site)  Maria Curie|Madame Curie
//! ```
//!
//! Aliases are separated by `|`, which no Wikipedia title contains.
//!
//! An article whose item has official profiles ([`crate::profiles`]) is
//! followed by a line of them, which files made before profiles never
//! have and readers made before them skip as a line they cannot read:
//!
//! ```text
//! profiles  item  service=id|service=id     (tab-separated)
//! profiles  Q19897578  youtube-handle=MrBeast|x=MrBeast
//! ```
//!
//! Such lines are not pages: the top `N` articles are the first `N` lines
//! that are not.
//!
//! The same line carries the item's official website when that is not
//! the front page of a site of its own: `website=https://music.youtube.com/`
//! for YouTube Music, whose site is youtube.com. Readers made before it
//! leave it out as a service they do not know.
//!
//! So do the article's lead, the first sentences of its text
//! (`lead=The sternum or breastbone is a long flat bone ...`), and its
//! other names: the titles that lead to it that are not among its aliases,
//! such as those that lead to one of its sections (`name=Manubrium`). Both
//! come from Wikipedia's search dump (`plumb fetch-leads`).
//!
//! A docs page's line carries the headings of its sections the same way
//! (`section=List Comprehensions`), from `plumb fetch-pages --set docs`.
//! Rich docs optionally add `search={"version":1,...}` to that same line.
//! Its bounded JSON preserves exact API identifiers, fragment anchors and
//! source passages; pipes are JSON-escaped so legacy readers ignore the
//! extension without misreading its content as profiles or extra sections.

use std::io::{BufRead, Write};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::facts::{parse_facts, write_facts, Fact};
use crate::packages::{PackageInfo, PACKAGE_LINE};
use crate::profiles::{parse_profiles, write_profiles, Profile};

/// The articles file's first line.
pub const ARTICLES_HEADER: &str = "views\ttitle\tdescription\titem\tsite\taliases\n";

/// Most aliases (redirect names) kept per article, the most read first.
pub const MAX_ALIASES: usize = 5;

/// Longest description kept, in characters.
pub const MAX_ARTICLE_DESCRIPTION_CHARS: usize = 160;

/// Longest lead kept, in characters: whole sentences up to this many, or
/// the first sentence cut to it.
pub const MAX_LEAD_CHARS: usize = 300;

/// Most other names ([`Article::names`]) kept per article.
pub const MAX_OTHER_NAMES: usize = 10;

/// Most sections ([`Article::sections`]) kept per page.
pub const MAX_SECTIONS: usize = 64;

/// Most characters of a page's sections, all together.
pub const MAX_SECTIONS_CHARS: usize = 1_000;

/// Current optional docs search extension. Old readers ignore its key.
pub const SEARCH_VERSION: u8 = 1;
pub const SEARCH_KEY: &str = "search";
/// Encoded JSON bytes, excluding the `search=` key, per page.
pub const MAX_SEARCH_BYTES: usize = 8 * 1024;
pub const MAX_SEARCH_SYMBOLS: usize = 64;
pub const MAX_SEARCH_SYMBOL_BYTES: usize = 4 * 1024;
pub const MAX_SEARCH_IDENTIFIER_BYTES: usize = 192;
pub const MAX_SEARCH_ANCHOR_BYTES: usize = 256;
pub const MAX_SEARCH_PASSAGES: usize = 8;
pub const MAX_SEARCH_PASSAGE_CHARS: usize = 512;
pub const MAX_SEARCH_HEADING_CHARS: usize = 160;

/// Exact, case-preserving API name and the HTML fragment that owns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchSymbol {
    pub identifier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
}

/// Source text, not a generated answer. Anchors are HTML IDs (without `#`),
/// not token offsets or character positions in the original HTML.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPassage {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub heading: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
}

/// Optional rich content on the existing profiles line. Missing content
/// means a compact/legacy record; it is not evidence of a rich rebuild.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchContent {
    pub version: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<SearchSymbol>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passages: Vec<SearchPassage>,
}

impl Default for SearchContent {
    fn default() -> Self {
        Self {
            version: SEARCH_VERSION,
            symbols: Vec::new(),
            passages: Vec::new(),
        }
    }
}

impl SearchContent {
    /// Validate and bound producer data before caching/indexing/writing.
    /// Overlong identifiers/anchors are discarded rather than cut into a
    /// different identifier or a broken fragment. Producer priority order
    /// determines which content survives the byte budgets.
    pub fn bounded(&self) -> Option<Self> {
        if self.version != SEARCH_VERSION {
            return None;
        }
        let mut result = Self::default();
        let mut bytes = 0;
        for symbol in &self.symbols {
            if result.symbols.len() == MAX_SEARCH_SYMBOLS {
                break;
            }
            if !valid_identifier(&symbol.identifier)
                || result
                    .symbols
                    .iter()
                    .any(|s| s.identifier == symbol.identifier && s.anchor == symbol.anchor)
            {
                continue;
            }
            let symbol = SearchSymbol {
                identifier: symbol.identifier.clone(),
                anchor: symbol.anchor.as_deref().and_then(search_anchor),
            };
            let size = search_json(&symbol).len();
            if bytes + size > MAX_SEARCH_SYMBOL_BYTES {
                continue;
            }
            bytes += size;
            result.symbols.push(symbol);
        }
        for passage in &self.passages {
            if result.passages.len() == MAX_SEARCH_PASSAGES {
                break;
            }
            let text = crate::truncate_chars(
                &crate::collapse_whitespace(&passage.text),
                MAX_SEARCH_PASSAGE_CHARS,
            );
            if text.is_empty() {
                continue;
            }
            let passage = SearchPassage {
                heading: crate::truncate_chars(
                    &crate::collapse_whitespace(&passage.heading),
                    MAX_SEARCH_HEADING_CHARS,
                ),
                text,
                anchor: passage.anchor.as_deref().and_then(search_anchor),
            };
            if !result.passages.contains(&passage) {
                result.passages.push(passage);
            }
        }
        // Unicode and JSON escaping also count toward the wire budget.
        while search_json(&result).len() > MAX_SEARCH_BYTES {
            if result.passages.pop().is_none() {
                result.symbols.pop()?;
            }
        }
        (!result.symbols.is_empty() || !result.passages.is_empty()).then_some(result)
    }

    /// Lexical input for retrieval: exact identifiers remain alongside
    /// their punctuation-separated words. Index exact symbols separately
    /// when the index supports them; passages retain their own provenance.
    pub fn text(&self) -> String {
        let mut text = String::new();
        for symbol in &self.symbols {
            text.push_str(&symbol.identifier);
            text.push(' ');
            for word in symbol
                .identifier
                .split(|c: char| !c.is_alphanumeric())
                .filter(|s| !s.is_empty())
            {
                text.push_str(word);
                text.push(' ');
            }
        }
        for passage in &self.passages {
            text.push_str(&passage.heading);
            text.push(' ');
            text.push_str(&passage.text);
            text.push(' ');
        }
        text
    }
}

pub fn valid_identifier(identifier: &str) -> bool {
    !identifier.is_empty()
        && identifier.len() <= MAX_SEARCH_IDENTIFIER_BYTES
        && identifier.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && identifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':'))
}

/// A fragment ID, preserved exactly; never truncate a destination.
pub fn search_anchor(anchor: &str) -> Option<String> {
    (!anchor.is_empty()
        && anchor.len() <= MAX_SEARCH_ANCHOR_BYTES
        && !anchor.chars().any(|c| c.is_control() || c.is_whitespace()))
    .then(|| anchor.to_string())
}

fn search_json(value: &impl Serialize) -> String {
    // JSON already escapes tabs and newlines. Escape the remaining TSV
    // metadata separator as JSON, so embedded pipes survive round trips.
    serde_json::to_string(value)
        .expect("search content is serializable")
        .replace('|', "\\u007c")
}

fn parse_search(value: &str) -> Option<SearchContent> {
    if value.len() > MAX_SEARCH_BYTES {
        return None;
    }
    serde_json::from_str::<SearchContent>(value).ok()?.bounded()
}

/// The articles file of Wikipedia in `lang` (`en`): `wikipedia-en.tsv.gz`.
pub fn articles_file_name(lang: &str) -> String {
    format!("wikipedia-{lang}.tsv.gz")
}

/// One Wikipedia article.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Article {
    /// The title as shown, with spaces: `Python (programming language)`.
    pub title: String,
    /// Wikipedia's short description ("General-purpose programming
    /// language"), when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Wikidata item id (`Q28865`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    /// Registrable domain of the item's official website (`python.org`),
    /// so a result for that site can carry the article instead of both
    /// being listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    /// Page views over the days the file was made from.
    pub views: u64,
    /// Other titles that lead to this article, the most read first, at
    /// most [`MAX_ALIASES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// The item's official profiles (a YouTube channel, an X account).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<Profile>,
    /// The item's official website when it is a subdomain or an inner
    /// page of [`Article::site`] rather than its front page:
    /// `https://music.youtube.com/` for YouTube Music.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    /// A software package's card (see [`crate::packages`]), on the line
    /// after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<PackageInfo>,
    /// Facts about the item from Wikidata ([`crate::facts`]), on its line
    /// of profiles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<Fact>,
    /// The first sentences of the article ([`lead_of`]), on its line of
    /// profiles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead: Option<String>,
    /// Other titles that lead to the article than its aliases, at most
    /// [`MAX_OTHER_NAMES`]: less read ones, and ones that lead to one of
    /// its sections ("Manubrium" to Sternum), on its line of profiles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    /// The headings of a docs page's sections, at most [`MAX_SECTIONS`]
    /// and [`MAX_SECTIONS_CHARS`] characters in all
    /// ("List Comprehensions" in Python's "Data Structures"), on its line
    /// of profiles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Optional inner-page search metadata; independent of display text.
    pub search: Option<SearchContent>,
    /// Declared content language, as a primary code (`es`, `de`). Unknown
    /// stays absent; a host's country is not evidence of content language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// The key of an official website on a line of profiles.
pub const WEBSITE_KEY: &str = "website";
/// The key of an article's lead on a line of profiles.
pub const LEAD_KEY: &str = "lead";
/// The key of one of an article's other names on a line of profiles.
pub const NAME_KEY: &str = "name";
/// The key of one of a docs page's sections on a line of profiles.
pub const SECTION_KEY: &str = "section";
/// The declared page language on the existing optional extension line.
pub const LANGUAGE_KEY: &str = "language";

/// What starts a line of profiles in an articles file.
pub const PROFILES_LINE: &str = "profiles\t";

/// Whether `line` of an articles file is a line of profiles rather than
/// an article.
pub fn is_profiles_line(line: &[u8]) -> bool {
    line.starts_with(PROFILES_LINE.as_bytes())
}

/// What a line of profiles says about an article's item.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfilesLine<'a> {
    pub item: &'a str,
    pub profiles: Vec<Profile>,
    pub website: Option<String>,
    pub facts: Vec<Fact>,
    pub lead: Option<String>,
    pub names: Vec<String>,
    pub sections: Vec<String>,
    pub search: Option<SearchContent>,
    pub language: Option<String>,
}

/// The item, profiles, official website and facts of a line of profiles,
/// `None` for another line.
pub fn parse_profiles_line(line: &str) -> Option<ProfilesLine<'_>> {
    let rest = line
        .trim_end_matches(['\n', '\r'])
        .strip_prefix(PROFILES_LINE)?;
    let (item, profiles) = rest.split_once('\t')?;
    let website = profiles.split('|').find_map(|pair| {
        let (key, url) = pair.split_once('=')?;
        let url = url.trim();
        (key.trim() == WEBSITE_KEY && is_web_address(url)).then(|| url.to_string())
    });
    let values = |wanted: &'static str| {
        profiles.split('|').filter_map(move |pair| {
            let (key, value) = pair.split_once('=')?;
            let value = value.trim();
            (key.trim() == wanted && !value.is_empty()).then(|| value.to_string())
        })
    };
    Some(ProfilesLine {
        item: item.trim(),
        profiles: parse_profiles(profiles),
        website,
        facts: parse_facts(profiles),
        lead: values(LEAD_KEY).next(),
        names: values(NAME_KEY).take(MAX_OTHER_NAMES).collect(),
        sections: values(SECTION_KEY).take(MAX_SECTIONS).collect(),
        search: profiles.split('|').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key.trim() == SEARCH_KEY)
                .then(|| parse_search(value.trim()))
                .flatten()
        }),
        language: profiles.split('|').find_map(|pair| {
            let (key, tag) = pair.split_once('=')?;
            (key.trim() == LANGUAGE_KEY)
                .then(|| crate::language_code(tag))
                .flatten()
        }),
    })
}

/// Whether `url` is an `http` or `https` address that fits a field.
fn is_web_address(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && !url.contains(['\t', '\n', '\r', '|', ' '])
}

/// The address of the article `title` on Wikipedia in `lang`.
pub fn article_url(lang: &str, title: &str) -> String {
    let path: String = title
        .replace(' ', "_")
        .chars()
        .map(|c| match c {
            // Kept as they are in Wikipedia's own links.
            'A'..='Z'
            | 'a'..='z'
            | '0'..='9'
            | '_'
            | '-'
            | '.'
            | '('
            | ')'
            | ','
            | ':'
            | '!'
            | '*'
            | '\''
            | '~'
            | '/' => c.to_string(),
            c => {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf)
                    .bytes()
                    .map(|b| format!("%{b:02X}"))
                    .collect()
            }
        })
        .collect();
    format!("https://{lang}.wikipedia.org/wiki/{path}")
}

/// `text` with tabs, line breaks and `|` made spaces, so it fits a field.
fn field(text: &str) -> String {
    crate::collapse_whitespace(&text.replace(['\t', '\n', '\r', '|'], " "))
}

/// Writes `article` as one line of an articles file.
pub fn write_article(out: &mut impl Write, article: &Article) -> std::io::Result<()> {
    let aliases: Vec<String> = article.aliases.iter().map(|a| field(a)).collect();
    writeln!(
        out,
        "{}\t{}\t{}\t{}\t{}\t{}",
        article.views,
        field(&article.title),
        field(article.description.as_deref().unwrap_or("")),
        field(article.item.as_deref().unwrap_or("")),
        field(article.site.as_deref().unwrap_or("")),
        aliases.join("|"),
    )?;
    let mut profiles = write_profiles(&article.profiles);
    if let Some(website) = article.website.as_deref().filter(|url| is_web_address(url)) {
        if !profiles.is_empty() {
            profiles.push('|');
        }
        profiles.push_str(WEBSITE_KEY);
        profiles.push('=');
        profiles.push_str(website);
    }
    let facts = write_facts(&article.facts);
    if !facts.is_empty() {
        if !profiles.is_empty() {
            profiles.push('|');
        }
        profiles.push_str(&facts);
    }
    let lead = article.lead.as_deref().map(field);
    let names = article.names.iter().map(|name| field(name));
    let sections = article.sections.iter().map(|section| field(section));
    let language = article.language.as_deref().and_then(crate::language_code);
    for (key, value) in lead
        .into_iter()
        .map(|lead| (LEAD_KEY, lead))
        .chain(names.map(|name| (NAME_KEY, name)))
        .chain(sections.map(|section| (SECTION_KEY, section)))
        .chain(language.map(|language| (LANGUAGE_KEY, language)))
        .filter(|(_, value)| !value.is_empty())
    {
        if !profiles.is_empty() {
            profiles.push('|');
        }
        profiles.push_str(key);
        profiles.push('=');
        profiles.push_str(&value);
    }
    if let Some(search) = article.search.as_ref().and_then(SearchContent::bounded) {
        if !profiles.is_empty() {
            profiles.push('|');
        }
        profiles.push_str(SEARCH_KEY);
        profiles.push('=');
        profiles.push_str(&search_json(&search));
    }
    if !profiles.is_empty() {
        write!(out, "{PROFILES_LINE}")?;
        writeln!(
            out,
            "{}\t{profiles}",
            field(article.item.as_deref().unwrap_or(""))
        )?;
    }
    if let (Some(package), Some(item)) = (&article.package, article.item.as_deref()) {
        writeln!(out, "{PACKAGE_LINE}{}\t{}", field(item), package.write())?;
    }
    Ok(())
}

/// The articles of an articles file's lines, each with the profiles on
/// the line after it, and the line it was on (counting from 1). The
/// header and blank lines are skipped; a line that does not read is an
/// error, after which reading goes on.
pub fn articles_of<I>(lines: I) -> ArticleLines<I>
where
    I: Iterator<Item = String>,
{
    ArticleLines {
        lines,
        number: 0,
        pending: None,
        error: None,
    }
}

/// See [`articles_of`].
pub struct ArticleLines<I> {
    lines: I,
    number: usize,
    /// The last article read, waiting for a line of profiles after it.
    pending: Option<(usize, Article)>,
    /// A line that did not read, to hand back next.
    error: Option<(usize, anyhow::Error)>,
}

impl<I: Iterator<Item = String>> Iterator for ArticleLines<I> {
    type Item = (usize, Result<Article>);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some((n, err)) = self.error.take() {
            return Some((n, Err(err)));
        }
        loop {
            let Some(line) = self.lines.next() else {
                return self.pending.take().map(|(n, a)| (n, Ok(a)));
            };
            self.number += 1;
            if (self.number == 1 && line.starts_with("views\t")) || line.trim().is_empty() {
                continue;
            }
            if let Some(found) = parse_profiles_line(&line) {
                if let Some((_, article)) = &mut self.pending {
                    if article.item.as_deref() == Some(found.item) {
                        article.profiles = found.profiles;
                        article.website = found.website;
                        article.facts = found.facts;
                        article.lead = found.lead;
                        article.names = found.names;
                        article.sections = found.sections;
                        article.search = found.search;
                        article.language = found.language;
                    }
                }
                continue;
            }
            if line.starts_with(PACKAGE_LINE) {
                if let (Some((item, package)), Some((_, article))) =
                    (PackageInfo::parse_line(&line), &mut self.pending)
                {
                    if article.item.as_deref() == Some(item.as_str()) {
                        article.package = Some(package);
                    }
                }
                continue;
            }
            let number = self.number;
            match parse_article(&line) {
                Ok(article) => {
                    if let Some((n, done)) = self.pending.replace((number, article)) {
                        return Some((n, Ok(done)));
                    }
                }
                // A line that does not read ends the article before it,
                // which goes first, so the order stays.
                Err(err) => {
                    return Some(match self.pending.take() {
                        Some((n, done)) => {
                            self.error = Some((number, err));
                            (n, Ok(done))
                        }
                        None => (number, Err(err)),
                    })
                }
            }
        }
    }
}

/// Parses one line of an articles file (not the header).
pub fn parse_article(line: &str) -> Result<Article> {
    let line = line.trim_end_matches(['\n', '\r']);
    let cols: Vec<&str> = line.split('\t').collect();
    if cols.len() != 6 {
        bail!("expected 6 tab-separated fields, found {}", cols.len());
    }
    let views = cols[0]
        .parse()
        .with_context(|| format!("bad view count {:?}", cols[0]))?;
    let title = cols[1].trim();
    if title.is_empty() {
        bail!("no title");
    }
    let some = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
    Ok(Article {
        title: title.to_string(),
        description: some(cols[2]),
        item: some(cols[3]),
        site: some(cols[4]),
        views,
        aliases: cols[5]
            .split('|')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            .collect(),
        profiles: Vec::new(),
        website: None,
        package: None,
        facts: Vec::new(),
        lead: None,
        names: Vec::new(),
        sections: Vec::new(),
        search: None,
        language: None,
    })
}

/// The first sentences of `text` (an article's opening paragraph), as
/// many whole ones as fit in [`MAX_LEAD_CHARS`], or the first cut there
/// at a word: what [`Article::lead`] keeps.
pub fn lead_of(text: &str) -> Option<String> {
    let text = crate::collapse_whitespace(text);
    if text.is_empty() {
        return None;
    }
    if text.chars().count() <= MAX_LEAD_CHARS {
        return Some(text);
    }
    let mut end = 0;
    for stop in sentence_ends(&text) {
        if text[..stop].chars().count() > MAX_LEAD_CHARS {
            break;
        }
        end = stop;
    }
    if end > 0 {
        return Some(text[..end].to_string());
    }
    let cut = crate::truncate_chars(&text, MAX_LEAD_CHARS);
    let cut = match cut.rfind(' ') {
        Some(space) if space > 0 => cut[..space].trim_end_matches([',', ';', ':']),
        _ => &cut,
    };
    Some(format!("{cut}…"))
}

/// The first sentence of an article's lead (see [`lead_of`]).
pub fn first_sentence(lead: &str) -> &str {
    match sentence_ends(lead).next() {
        Some(end) => &lead[..end],
        None => lead,
    }
}

/// Short words that end in a full stop without ending a sentence.
const ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "st", "jr", "sr", "mt", "ft", "no", "vs", "inc", "ltd", "co", "corp",
    "ca", "c", "approx", "est", "gen", "col", "lt", "sgt", "capt", "rev", "prof", "sen", "rep",
    "gov", "pres", "fr", "op", "vol", "jan", "feb", "mar", "apr", "jun", "jul", "aug", "sep",
    "sept", "oct", "nov", "dec", "e.g", "i.e", "etc", "al", "bros",
];

/// The byte offsets just past each sentence's full stop (or `?`, `!`) in
/// `text`: a stop followed by a space and a capital letter, a digit or a
/// quote, after a word that is not an abbreviation or an initial.
fn sentence_ends(text: &str) -> impl Iterator<Item = usize> + '_ {
    text.char_indices().filter_map(move |(at, c)| {
        if !matches!(c, '.' | '?' | '!') {
            return None;
        }
        let end = at + c.len_utf8();
        let mut after = text[end..].chars();
        if after.next() != Some(' ') {
            return None;
        }
        let next = after.next()?;
        if !(next.is_uppercase() || next.is_ascii_digit() || matches!(next, '"' | '“' | '(')) {
            return None;
        }
        if c == '.' {
            let word = text[..at]
                .rsplit(|c: char| c.is_whitespace() || c == '(')
                .next()
                .unwrap_or("");
            let letters = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '.');
            // An initial ("John F. Kennedy", "U.S."), an abbreviation.
            if letters.chars().filter(|c| c.is_alphabetic()).count() <= 1
                && letters.chars().all(|c| c.is_alphabetic() || c == '.')
                || letters.contains('.')
                || ABBREVIATIONS.contains(&letters.to_lowercase().as_str())
            {
                return None;
            }
        }
        Some(end)
    })
}

/// Reads up to `limit` articles of an articles file (the most read ones,
/// since the file is sorted), skipping its header.
pub fn read_articles(reader: impl BufRead, limit: usize) -> Result<Vec<Article>> {
    let mut articles = Vec::new();
    let mut failed = None;
    let lines = reader.lines().map_while(|line| match line {
        Ok(line) => Some(line),
        Err(err) => {
            failed = Some(err);
            None
        }
    });
    for (n, article) in articles_of(lines) {
        if articles.len() >= limit {
            break;
        }
        articles.push(article.with_context(|| format!("articles line {n}"))?);
    }
    if let Some(err) = failed {
        return Err(anyhow::Error::new(err).context("reading articles"));
    }
    Ok(articles)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_match_wikipedia_links() {
        assert_eq!(
            article_url("en", "Python (programming language)"),
            "https://en.wikipedia.org/wiki/Python_(programming_language)"
        );
        assert_eq!(
            article_url("en", "Nestlé"),
            "https://en.wikipedia.org/wiki/Nestl%C3%A9"
        );
        assert_eq!(
            article_url("en", "AT&T"),
            "https://en.wikipedia.org/wiki/AT%26T"
        );
    }

    #[test]
    fn lines_round_trip() {
        let article = Article {
            title: "Marie Curie".into(),
            description: Some("Polish-French physicist\tand chemist".into()),
            item: Some("Q7186".into()),
            site: None,
            views: 81234,
            aliases: vec!["Maria Curie".into(), "Madame Curie".into()],
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            search: None,
            language: None,
        };
        let mut out = Vec::new();
        out.extend_from_slice(ARTICLES_HEADER.as_bytes());
        write_article(&mut out, &article).unwrap();
        let back = read_articles(&out[..], 10).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(
            back[0].description.as_deref(),
            Some("Polish-French physicist and chemist")
        );
        assert_eq!(back[0].aliases, article.aliases);
        assert_eq!(back[0].views, 81234);
        assert_eq!(back[0].site, None);
    }

    #[test]
    fn profiles_follow_their_article() {
        use crate::profiles::Profile;
        let beast = Article {
            title: "MrBeast".into(),
            item: Some("Q19897578".into()),
            views: 900,
            profiles: vec![
                Profile {
                    service: "youtube-handle".into(),
                    id: "MrBeast".into(),
                },
                Profile {
                    service: "x".into(),
                    id: "MrBeast".into(),
                },
            ],
            ..Article::default()
        };
        let plain = Article {
            title: "Plain".into(),
            item: Some("Q1".into()),
            views: 5,
            ..Article::default()
        };
        let mut out = Vec::new();
        out.extend_from_slice(ARTICLES_HEADER.as_bytes());
        write_article(&mut out, &beast).unwrap();
        write_article(&mut out, &plain).unwrap();
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.contains("\nprofiles\tQ19897578\tyoutube-handle=MrBeast|x=MrBeast\n"));
        assert_eq!(read_articles(&out[..], 10).unwrap(), [beast.clone(), plain]);
        assert_eq!(read_articles(&out[..], 1).unwrap(), [beast]);
        // A reader made before profiles finds no article in their line.
        assert!(parse_article("profiles\tQ1\tx=a").is_err());
    }

    #[test]
    fn websites_ride_on_the_line_of_profiles() {
        let music = Article {
            title: "YouTube Music".into(),
            item: Some("Q28404534".into()),
            site: Some("youtube.com".into()),
            views: 700,
            website: Some("https://music.youtube.com/".into()),
            ..Article::default()
        };
        let mut out = Vec::new();
        out.extend_from_slice(ARTICLES_HEADER.as_bytes());
        write_article(&mut out, &music).unwrap();
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.contains("\nprofiles\tQ28404534\twebsite=https://music.youtube.com/\n"));
        assert_eq!(read_articles(&out[..], 10).unwrap(), [music]);
        // Readers made before websites see no profile in it.
        assert!(parse_profiles("website=https://music.youtube.com/").is_empty());
    }

    #[test]
    fn facts_ride_on_the_line_of_profiles() {
        use crate::facts::FactKind;
        let australia = Article {
            title: "Australia".into(),
            item: Some("Q408".into()),
            views: 9_000,
            profiles: vec![Profile {
                service: "x".into(),
                id: "Australia".into(),
            }],
            facts: vec![
                Fact {
                    kind: FactKind::Capital,
                    value: "Canberra".into(),
                },
                Fact {
                    kind: FactKind::Population,
                    value: "27204809;2024".into(),
                },
            ],
            ..Article::default()
        };
        let mut out = Vec::new();
        out.extend_from_slice(ARTICLES_HEADER.as_bytes());
        write_article(&mut out, &australia).unwrap();
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.contains(
            "\nprofiles\tQ408\tx=Australia|f-capital=Canberra|f-population=27204809;2024\n"
        ));
        assert_eq!(read_articles(&out[..], 10).unwrap(), [australia]);
        // Readers made before facts see only the profile.
        assert_eq!(
            parse_profiles("x=Australia|f-capital=Canberra|f-population=27204809;2024").len(),
            1
        );
    }

    #[test]
    fn leads_and_other_names_ride_on_the_line_of_profiles() {
        let sternum = Article {
            title: "Sternum".into(),
            item: Some("Q4590598".into()),
            views: 9_000,
            lead: Some("The sternum or breastbone is a long flat bone | of the chest.".into()),
            names: vec!["Manubrium".into(), "Xiphisternum".into()],
            ..Article::default()
        };
        let mut out = Vec::new();
        out.extend_from_slice(ARTICLES_HEADER.as_bytes());
        write_article(&mut out, &sternum).unwrap();
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.contains(
            "\nprofiles\tQ4590598\tlead=The sternum or breastbone is a long flat bone of the chest.|name=Manubrium|name=Xiphisternum\n"
        ));
        let back = read_articles(&out[..], 10).unwrap();
        assert_eq!(
            back[0].lead.as_deref(),
            Some("The sternum or breastbone is a long flat bone of the chest.")
        );
        assert_eq!(back[0].names, sternum.names);
        // Readers made before leads see no profile or fact in it.
        assert!(parse_profiles("lead=A b.|name=C").is_empty());
        assert!(parse_facts("lead=A b.|name=C").is_empty());
    }

    #[test]
    fn a_docs_pages_sections_ride_on_the_line_of_profiles() {
        let page = Article {
            title: "Data Structures".into(),
            item: Some("https://docs.python.org/3/tutorial/datastructures.html".into()),
            views: 1_000,
            sections: vec!["More on Lists".into(), "List Comprehensions".into()],
            ..Article::default()
        };
        let mut out = Vec::new();
        out.extend_from_slice(ARTICLES_HEADER.as_bytes());
        write_article(&mut out, &page).unwrap();
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.contains("\tsection=More on Lists|section=List Comprehensions\n"));
        let back = read_articles(&out[..], 10).unwrap();
        assert_eq!(back[0].sections, page.sections);
        assert!(back[0].names.is_empty());
        assert!(parse_profiles("section=More on Lists").is_empty());
    }

    #[test]
    fn rich_docs_round_trip_without_changing_legacy_columns_or_headings() {
        let search = SearchContent {
            symbols: vec![SearchSymbol {
                identifier: "set_multiplayer_authority".into(),
                anchor: Some("class-node-method-set-multiplayer-authority".into()),
            }],
            passages: vec![SearchPassage {
                heading: "CrashLoopBackOff".into(),
                text: "Source says a | b = c; tabs\tand\nnewlines survive as whitespace.".into(),
                anchor: Some("backoff|原因".into()),
            }],
            ..SearchContent::default()
        }
        .bounded()
        .unwrap();
        let page = Article {
            title: "Node".into(),
            item: Some("https://docs.godotengine.org/en/stable/classes/class_node.html".into()),
            sections: vec!["Method Descriptions".into()],
            search: Some(search.clone()),
            ..Article::default()
        };
        let mut out = Vec::new();
        write_article(&mut out, &page).unwrap();
        let text = String::from_utf8(out.clone()).unwrap();
        let lines: Vec<_> = text.lines().collect();
        let compact = parse_article(lines[0]).unwrap();
        assert_eq!(compact.title, page.title);
        assert_eq!(compact.search, None);
        assert!(
            crate::profiles::parse_profiles(lines[1].splitn(3, '\t').nth(2).unwrap()).is_empty()
        );
        assert!(text.contains("\\u007c"));
        assert_eq!(lines[1].split('\t').count(), 3);
        let back = read_articles(&out[..], 1).unwrap();
        assert_eq!(back[0], page);
        assert!(back[0]
            .search
            .as_ref()
            .unwrap()
            .text()
            .contains("set multiplayer authority"));
        let legacy: Article = serde_json::from_str(r#"{"title":"Node","views":1}"#).unwrap();
        assert!(legacy.search.is_none());
    }

    #[test]
    fn malformed_or_future_search_extensions_leave_the_parent_and_sections_readable() {
        for value in [
            "{",
            r#"{"version":2,"symbols":[]}"#,
            r#"{"version":1,"symbols":"bad"}"#,
            &"x".repeat(MAX_SEARCH_BYTES + 1),
        ] {
            let line = format!("profiles\turl\tsection=Methods|search={value}|section=Errors");
            let parsed = parse_profiles_line(&line).unwrap();
            assert_eq!(parsed.sections, ["Methods", "Errors"]);
            assert!(parsed.search.is_none());
            let file = format!("1\tNode\t\turl\t\t\n{line}\n");
            assert_eq!(read_articles(file.as_bytes(), 1).unwrap()[0].title, "Node");
        }
    }

    #[test]
    fn rich_content_enforces_byte_character_and_identity_bounds() {
        let search = SearchContent {
            symbols: (0..200)
                .map(|i| SearchSymbol {
                    identifier: format!("long_identifier_{i}"),
                    anchor: Some("a".repeat(MAX_SEARCH_ANCHOR_BYTES + 1)),
                })
                .chain([SearchSymbol {
                    identifier: "x".repeat(MAX_SEARCH_IDENTIFIER_BYTES + 1),
                    anchor: None,
                }])
                .collect(),
            passages: (0..30)
                .map(|i| SearchPassage {
                    heading: format!("Section {i}"),
                    text: "原因|".repeat(600),
                    anchor: None,
                })
                .collect(),
            ..SearchContent::default()
        };
        let bounded = search.bounded().unwrap();
        assert!(bounded.symbols.len() <= MAX_SEARCH_SYMBOLS);
        assert!(bounded
            .symbols
            .iter()
            .all(|s| s.anchor.is_none() && s.identifier.len() <= MAX_SEARCH_IDENTIFIER_BYTES));
        assert!(bounded.passages.len() <= MAX_SEARCH_PASSAGES);
        assert!(bounded
            .passages
            .iter()
            .all(|p| p.text.chars().count() <= MAX_SEARCH_PASSAGE_CHARS));
        let encoded = search_json(&bounded);
        assert!(encoded.len() <= MAX_SEARCH_BYTES);
        assert_eq!(parse_search(&encoded), Some(bounded));
    }

    #[test]
    fn declared_language_uses_the_optional_extension_without_changing_six_columns() {
        let article = Article {
            title: "Introducción".into(),
            item: Some("https://docs.python.org/es/3/tutorial/".into()),
            language: Some("es-ES".into()),
            sections: vec!["Listas".into()],
            ..Article::default()
        };
        let mut out = Vec::new();
        write_article(&mut out, &article).unwrap();
        let text = String::from_utf8(out.clone()).unwrap();
        let base = text.lines().next().unwrap();
        assert_eq!(base.split('\t').count(), 6);
        assert_eq!(parse_article(base).unwrap().language, None);
        assert!(text.contains("|language=es\n"));
        let back = read_articles(&out[..], 10).unwrap();
        assert_eq!(back[0].language.as_deref(), Some("es"));
        assert_eq!(back[0].sections, ["Listas"]);
        assert!(parse_profiles("language=es").is_empty());
        assert!(parse_facts("language=es").is_empty());
        assert_eq!(
            parse_profiles_line("profiles\tQ1\tlanguage=und")
                .unwrap()
                .language,
            None
        );
        assert_eq!(
            parse_profiles_line("profiles\tQ1\tlanguage=en-@@")
                .unwrap()
                .language,
            None
        );
        let mismatched = format!("{base}\nprofiles\tQ1\tlanguage=de\n");
        assert_eq!(
            read_articles(mismatched.as_bytes(), 1).unwrap()[0].language,
            None
        );
    }

    #[test]
    fn leads_are_whole_sentences() {
        let text = "The sternum or breastbone is a long flat bone located in the central part of the chest. It connects to the ribs via cartilage and forms the front of the rib cage, thus helping to protect the heart, lungs, and major blood vessels from injury. Shaped roughly like a necktie, it is one of the largest and longest flat bones of the body.";
        let lead = lead_of(text).unwrap();
        assert!(lead.ends_with("from injury."), "{lead}");
        assert_eq!(
            first_sentence(&lead),
            "The sternum or breastbone is a long flat bone located in the central part of the chest."
        );
        assert_eq!(
            first_sentence("John F. Kennedy was the 35th president of the U.S. He was born in Brookline, Mass. in 1917."),
            "John F. Kennedy was the 35th president of the U.S. He was born in Brookline, Mass. in 1917."
        );
        assert_eq!(
            first_sentence("St. Louis is a city in Missouri. It lies on the Mississippi."),
            "St. Louis is a city in Missouri."
        );
        let long = "word ".repeat(100);
        let cut = lead_of(&long).unwrap();
        assert!(cut.ends_with("word…") && cut.chars().count() <= MAX_LEAD_CHARS + 1);
        assert_eq!(lead_of("  "), None);
    }

    #[test]
    fn a_bad_line_keeps_the_order() {
        let text = format!("{ARTICLES_HEADER}9\tA\t\t\t\t\nbad\n5\tB\t\t\t\t\n");
        let read: Vec<(usize, bool)> = articles_of(text.lines().map(str::to_string))
            .map(|(n, a)| (n, a.is_ok()))
            .collect();
        assert_eq!(read, [(2, true), (3, false), (4, true)]);
    }

    #[test]
    fn reads_only_the_top() {
        let text = format!("{ARTICLES_HEADER}9\tA\t\t\t\t\n5\tB\t\t\t\t\n1\tC\t\t\t\t\n");
        let top = read_articles(text.as_bytes(), 2).unwrap();
        assert_eq!(
            top.iter().map(|a| a.title.as_str()).collect::<Vec<_>>(),
            ["A", "B"]
        );
    }

    #[test]
    fn bad_lines_are_errors() {
        assert!(parse_article("x\tA\t\t\t\t").is_err());
        assert!(parse_article("1\tA").is_err());
        assert!(parse_article("1\t \t\t\t\t").is_err());
    }
}
