//! The `read_page` tool: a page as plain text, fetched by this node.

use super::*;

/// Characters of a page's opening `site_info` returns when it reads one.
const MAX_OPENING_CHARS: usize = 300;

/// Characters of a page `read_page` returns when the caller does not say...
const DEFAULT_READ_CHARS: usize = 6_000;

/// ...and the most it returns at once.
pub(super) const MAX_READ_CHARS: usize = 30_000;

/// Links `read_page` lists when asked for them.
const MAX_LINKS_RETURNED: usize = 60;

/// Most headings `read_page` lists in an outline...
const MAX_OUTLINE_HEADINGS: usize = 60;

/// ...and the characters of each section's opening it shows.
const OUTLINE_OPENING_CHARS: usize = 100;

/// What `read_page` fetches pages with: a reader, and the runtime its
/// requests run on (the tools themselves run on a blocking thread).
#[derive(Clone)]
pub struct Reader {
    pages: PageReader,
    runtime: tokio::runtime::Handle,
}

impl Reader {
    pub fn new(pages: PageReader, runtime: tokio::runtime::Handle) -> Self {
        Reader { pages, runtime }
    }

    /// A reader with the usual settings, running on `runtime`.
    pub fn standard(runtime: tokio::runtime::Handle) -> Result<Self> {
        Reader::with_config(ReadConfig::default(), runtime)
    }

    /// A reader with `config`, running on `runtime`.
    pub fn with_config(config: ReadConfig, runtime: tokio::runtime::Handle) -> Result<Self> {
        let pages = PageReader::new(config).context("making the page reader")?;
        Ok(Reader::new(pages, runtime))
    }

    /// What a page says of itself, for `site_info`: where it ends up, its
    /// title and its opening words.
    pub(super) fn front(&self, address: &str) -> Result<Value> {
        let page = self
            .runtime
            .block_on(self.pages.read(address))
            .map_err(anyhow::Error::from)?;
        let opening: String = page
            .text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(MAX_OPENING_CHARS)
            .collect();
        let host = host_of(&page.url).unwrap_or_default();
        if plumb_core::is_bot_check_page(&host, page.title.as_deref(), None, &[], Some(&opening)) {
            bail!("it showed a bot check instead of the page");
        }
        Ok(json!({ "url": page.url, "title": page.title, "opening": opening }))
    }
}

impl Mcp {
    /// `read_page`: the page's text, and whether its address (after
    /// redirects) is a look-alike.
    pub fn read_page(&self, args: &ReadArgs, options: &SearchOptions) -> Result<Value> {
        let Some(reader) = &self.reader else {
            bail!("this node does not read pages");
        };
        let mut answer = reader.read(args)?;
        if let Some(url) = answer["url"].as_str() {
            if let Ok(check) = self.check_lookalike(url, options) {
                add_site_check(&mut answer, &check);
            }
        }
        Ok(answer)
    }
}

/// `read_page`'s arguments.
#[derive(Debug, Clone)]
pub struct ReadArgs {
    pub(super) url: String,
    pub(super) start: usize,
    pub(super) max_chars: usize,
    links: bool,
    /// Words to jump to: the part returned starts at their first
    /// appearance from `start`.
    find: Option<String>,
    /// The page's headings, where each starts and its opening words,
    /// instead of its text.
    outline: bool,
}

impl ReadArgs {
    pub(super) fn of(args: &Map<String, Value>) -> Result<Self, (i64, String)> {
        Ok(ReadArgs {
            url: url_arg(args, "url")?,
            start: whole_number(args, "start")?.unwrap_or(0),
            max_chars: whole_number(args, "max_chars")?
                .unwrap_or(DEFAULT_READ_CHARS)
                .clamp(200, MAX_READ_CHARS),
            links: args.get("links").and_then(Value::as_bool).unwrap_or(false),
            find: args
                .get("find")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|find| !find.is_empty())
                .map(|find| truncate_chars(find, MAX_QUERY_CHARS)),
            outline: args
                .get("outline")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

/// Adds `check_lookalike`'s verdict to a `read_page` answer.
pub(super) fn add_site_check(answer: &mut Value, check: &Value) {
    if let Some(fields) = answer.as_object_mut() {
        fields.insert(
            "site".into(),
            json!({ "verdict": check["verdict"], "imitates": check["imitates"]["domain"] }),
        );
    }
}

impl Reader {
    /// Fetches the page and cuts out the part asked for.
    pub(super) fn read(&self, args: &ReadArgs) -> Result<Value> {
        let page = self
            .runtime
            .block_on(self.pages.read(&args.url))
            .map_err(anyhow::Error::from)?;
        let domain = registrable_domain(&page.url).or_else(|| {
            url::Url::parse(&page.url)
                .ok()?
                .host_str()
                .map(str::to_string)
        });
        if let Some(domain) = domain {
            let opening: String = page.text.chars().take(2_000).collect();
            if plumb_core::is_bot_check_page(
                &domain,
                page.title.as_deref(),
                None,
                &[],
                Some(&opening),
            ) {
                bail!(
                    "{domain} showed a bot check (a CAPTCHA or \"checking your browser\" page) \
                     instead of the page; try another source"
                );
            }
        }
        let (max_chars, links) = (args.max_chars, args.links);
        let chars: Vec<char> = page.text.chars().collect();
        let total = chars.len();
        if args.outline {
            let sections = outline(&chars);
            // A page without headings has no outline to show: its text
            // follows instead, so the call is not wasted.
            if !sections.is_empty() {
                return Ok(json!({
                    "url": page.url,
                    "title": page.title,
                    "outline": sections,
                    "length": total,
                    "truncated": page.cut,
                }));
            }
        }
        let mut start = args.start.min(total);
        let found = args
            .find
            .as_deref()
            .map(|find| match find_from(&chars, find, start) {
                Some(at) => {
                    // From the start of its line, when that is near enough
                    // for the match to stay well inside the text returned.
                    let line = chars[..at]
                        .iter()
                        .rposition(|&c| c == '\n')
                        .map_or(0, |n| n + 1);
                    let near = (max_chars / 3).min(300);
                    start = if at - line <= near { line } else { at };
                    true
                }
                None => false,
            });
        let mut end = (start + max_chars).min(total);
        let mut text: String = chars[start..end].iter().collect();
        if end < total {
            // End at a line break when one is in the second half.
            if let Some(cut) = text.rfind('\n').filter(|&cut| cut > text.len() / 2) {
                text.truncate(cut);
                end = start + text.chars().count();
            }
        }
        let mut answer = json!({
            "url": page.url,
            "title": page.title,
            "text": text.trim_end(),
            "start": start,
            "end": end,
            "length": total,
            "more": end < total,
            "truncated": page.cut,
        });
        let fields = answer.as_object_mut().expect("an object");
        if end < total {
            fields.insert("next_start".into(), json!(end));
        }
        if let Some(found) = found {
            fields.insert("found".into(), json!(found));
        }
        if links {
            let links: Vec<Value> = page
                .links
                .iter()
                .take(MAX_LINKS_RETURNED)
                .map(|(text, url)| json!({ "text": truncate_chars(text, 100), "url": url }))
                .collect();
            fields.insert("links".into(), json!(links));
        }
        Ok(answer)
    }
}

/// A page's sections: each Markdown heading in its text with its level,
/// where it starts and the opening words under it, after the words before
/// the first heading (level 0) when there are any. Without headings, none.
/// A long outline keeps the higher levels, then the first headings.
fn outline(chars: &[char]) -> Vec<Value> {
    // (level, heading, where the line starts, where the text under it starts)
    let mut headings: Vec<(usize, String, usize, usize)> = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let end = chars[at..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(chars.len(), |n| at + n);
        let line: String = chars[at..end].iter().collect();
        let level = line.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&level) && line[level..].starts_with(' ') {
            let heading = line[level..].trim().to_string();
            if !heading.is_empty() {
                headings.push((level, heading, at, end));
            }
        }
        at = end + 1;
    }
    if headings.is_empty() {
        return Vec::new();
    }
    let mut deepest = 6;
    while headings.len() > MAX_OUTLINE_HEADINGS && deepest > 1 {
        headings.retain(|(level, ..)| *level < deepest);
        deepest -= 1;
    }
    headings.truncate(MAX_OUTLINE_HEADINGS);
    // The words under a heading, up to the next line that is a heading.
    let opening = |from: usize| -> String {
        let words: String = chars[from.min(chars.len())..]
            .iter()
            .take(OUTLINE_OPENING_CHARS * 4)
            .collect();
        let words: String = words
            .lines()
            .map(str::trim)
            .take_while(|line| !line.starts_with('#'))
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        truncate_chars(&words, OUTLINE_OPENING_CHARS)
    };
    let mut sections = Vec::new();
    let before = opening(0);
    if headings[0].2 > 0 && !before.is_empty() {
        sections.push(json!({ "level": 0, "heading": "", "start": 0, "opening": before }));
    }
    for (level, heading, start, under) in headings {
        sections.push(json!({
            "level": level,
            "heading": truncate_chars(&heading, OUTLINE_OPENING_CHARS),
            "start": start,
            "opening": opening(under),
        }));
    }
    sections
}

/// Where `find` first appears in `chars` at or after `from`, ignoring case.
pub(super) fn find_from(chars: &[char], find: &str, from: usize) -> Option<usize> {
    let fold = |c: char| c.to_lowercase().next().unwrap_or(c);
    let find: String = find.trim().chars().map(fold).collect();
    if find.is_empty() || from >= chars.len() {
        return None;
    }
    // One char folds to one char, so positions carry over; `str::find`
    // stays linear where comparing at every position could take seconds
    // on a big page.
    let rest: String = chars[from..].iter().map(|&c| fold(c)).collect();
    let byte = rest.find(&find)?;
    Some(from + rest[..byte].chars().count())
}
