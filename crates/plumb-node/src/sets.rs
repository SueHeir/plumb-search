//! `plumb set`: sets in the data format ([`plumb_format`]) made from
//! today's page sets, signed and checked.
//!
//! A set is a folder: `manifest.json` and its parts, `items-001.jsonl.zst`
//! and on, one item per line, most weighted first. `from-pages` writes one
//! from an articles file, `page-sets` the project's set of each page set a
//! node holds, `denylist` one from a file of denylist entries
//! (`deny-entry` makes them), `site-rules` one of the rules that say which
//! site a host is on, `sign` signs its manifest with a set-signing
//! key from `new-key`, and `check` checks a set as a node would before
//! taking it. Signing is a release step someone takes on purpose: no build
//! signs by itself.
//!
//! Nothing here changes what a node indexes or sends.

use std::collections::{hash_map, BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use clap::{Args, Subcommand, ValueEnum};
use ed25519_dalek::SigningKey;
use plumb_format::deny::{Entry, DENY};
use plumb_format::manifest::is_set_name;
use plumb_format::notes::{is_revoked, sign_note, NOTE_FORMAT};
use plumb_format::sign::sign;
use plumb_format::sites::{read_suffix_list, Rule, SITE_RULE};
use plumb_format::urls::{url_sha256, url_sha256_any_query};
use plumb_format::{FieldSpec, Item, Manifest, Note, PublicKey, SiteRules, FORMAT};
use plumb_index::items::{differing_members, page_fields, page_item_or_why, page_of_item, visible};
use plumb_index::pages::Page;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::site_rules::{built_in_rules, hosts_to_check, moved_sites, Moved};

/// The manifest's file name in a set's folder.
pub const MANIFEST: &str = "manifest.json";

/// How many items the parts hold, counted from the most weighted: the
/// first 100,000, up to the first 1,000,000, then the rest.
pub(crate) const PART_ENDS: &[u64] = &[100_000, 1_000_000];

/// What a set-signing key file starts with.
const KEY_PREFIX: &str = "ed25519-secret:";

#[derive(Debug, Args)]
pub struct SetArgs {
    #[command(subcommand)]
    pub command: SetCommand,
}

#[derive(Debug, Subcommand)]
pub enum SetCommand {
    /// Make a key for signing sets, kept apart from any node key, and print
    /// its public key.
    NewKey {
        /// Where to save it. An existing file is never overwritten.
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
    },
    /// Write a page set in today's format (an articles file) as a set of
    /// items, with a manifest still to be signed.
    FromPages(FromPagesArgs),
    /// Write each page set a node holds whole as the project's item set of
    /// it, `plumb/<id>`, with the license and attribution the project
    /// picked for it, each with a manifest still to be signed.
    PageSets(PageSetsArgs),
    /// Write a denylist from a file of entries, one a line, with a manifest
    /// still to be signed.
    Denylist(DenylistArgs),
    /// Print a denylist entry as one line, to add to an entries file.
    DenyEntry(DenyEntryArgs),
    /// Write the rules that say which site a host is on, from a Public
    /// Suffix List file and the built-in subsite lists, with a manifest
    /// still to be signed, and list the hosts whose site they move.
    SiteRules(SiteRulesArgs),
    /// Sign a set's manifest after checking every part and item.
    Sign(SignArgs),
    /// Check a set as a node does before taking it: its signature, the size
    /// and hash of every part, and every item.
    Check(CheckArgs),
    /// Sign a key note: let another key sign some sets for a while
    /// (`delegate`), name the key that publishes from now on (`handover`),
    /// or withdraw a key (`revoke`).
    Note(NoteArgs),
}

#[derive(Debug, Args)]
pub struct FromPagesArgs {
    /// The articles file, `.tsv` or `.tsv.gz`, most read first.
    #[arg(long, value_name = "FILE")]
    pub pages: PathBuf,
    /// Its page set: `wikipedia-en`, `docs`, `music`...
    #[arg(long)]
    pub set: String,
    /// The set's name in the format, `<scope>/<name>`. `plumb/<set>` when
    /// not given.
    #[arg(long)]
    pub name: Option<String>,
    /// The folder to write. It must not exist yet, or be empty.
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,
    #[arg(long)]
    pub title: Option<String>,
    /// The set's license, an SPDX id (`CC-BY-SA-4.0`).
    #[arg(long)]
    pub license: Option<String>,
    /// How results from it credit their source: `From Wikipedia ({url})`.
    #[arg(long)]
    pub attribution: Option<String>,
    /// What the items' weight counts: `page views, last 7 days`.
    #[arg(long, value_name = "TEXT")]
    pub weight_counts: Option<String>,
    /// The zstd level of the parts.
    #[arg(long, default_value_t = 19)]
    pub level: i32,
}

#[derive(Debug, Args)]
pub struct PageSetsArgs {
    /// The node's data folder, whose `pages/sets/` holds the files. They
    /// are only read.
    #[arg(long, value_name = "DIR")]
    pub data: PathBuf,
    /// Where to write the sets, each in `<DIR>/plumb/<id>/`, which must
    /// not exist yet or be empty.
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,
    /// Only these sets, separated by commas.
    #[arg(long, value_delimiter = ',')]
    pub only: Vec<String>,
    /// The zstd level of the parts.
    #[arg(long, default_value_t = 19)]
    pub level: i32,
}

#[derive(Debug, Args)]
pub struct DenylistArgs {
    /// The entries, one JSON object a line, from `plumb set deny-entry`.
    #[arg(long, value_name = "FILE")]
    pub entries: PathBuf,
    /// The set's name, `<scope>/<name>`.
    #[arg(long, default_value = "plumb/denylist")]
    pub name: String,
    /// The folder to write. It must not exist yet, or be empty.
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,
    #[arg(long)]
    pub title: Option<String>,
    /// The list's license, an SPDX id. A set without one is never passed
    /// on.
    #[arg(long)]
    pub license: Option<String>,
    /// The zstd level of the parts.
    #[arg(long, default_value_t = 19)]
    pub level: i32,
}

#[derive(Debug, Args)]
pub struct SiteRulesArgs {
    /// The Public Suffix List, `public_suffix_list.dat`.
    #[arg(long, value_name = "FILE")]
    pub psl: PathBuf,
    /// The set's name, `<scope>/<name>`.
    #[arg(long, default_value = "plumb/site-rules")]
    pub name: String,
    /// The folder to write. It must not exist yet, or be empty.
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,
    #[arg(long)]
    pub title: Option<String>,
    /// The set's license, an SPDX id: the suffix list's own by default.
    #[arg(long, default_value = "MPL-2.0")]
    pub license: String,
    /// More hosts or URLs to check, one a line, besides those the rules
    /// name.
    #[arg(long, value_name = "FILE")]
    pub hosts: Option<PathBuf>,
    /// The zstd level of the parts.
    #[arg(long, default_value_t = 19)]
    pub level: i32,
}

#[derive(Debug, Args)]
pub struct DenyEntryArgs {
    /// Why: `csam`, `copyright`, `privacy`, `court-order`, `terror`,
    /// `malware`, `spam`.
    #[arg(long)]
    pub category: String,
    /// What it takes off: `url:<URL>`, `prefix:<URL>`, `site:<domain>`,
    /// `item:<publisher>/<set>/<id>` or `publisher:<key>`.
    #[arg(long, required_unless_present = "hash_of", conflicts_with = "hash_of")]
    pub target: Option<String>,
    /// Take off this page by the hash of its address, so the list never
    /// spells it out. The only target a `csam` entry may have.
    #[arg(long, value_name = "URL")]
    pub hash_of: Option<String>,
    /// With --hash-of: whatever query the page is given.
    #[arg(long, requires = "hash_of")]
    pub any_query: bool,
    /// A jurisdiction it is for (`US`, `DE`, `EU`). Repeat for more; `*`,
    /// everywhere, when not given.
    #[arg(long = "where", value_name = "PLACE")]
    pub places: Vec<String>,
    /// Its id in the list. The date and the start of the target's hash
    /// when not given.
    #[arg(long)]
    pub id: Option<String>,
}

#[derive(Debug, Args)]
pub struct SignArgs {
    /// The set's folder.
    pub dir: PathBuf,
    /// The set-signing key, from `plumb set new-key`.
    #[arg(long, value_name = "FILE")]
    pub key: PathBuf,
    /// The generation this one replaces, when one was published before.
    #[arg(long, value_name = "TIMESTAMP")]
    pub prev: Option<String>,
    /// Sign for another publisher, with its delegation to this key (from
    /// `plumb set note delegate`). Without it, the key's own key publishes.
    #[arg(long, value_name = "FILE")]
    pub delegation: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct CheckArgs {
    /// The set's folder.
    pub dir: PathBuf,
    /// The key the set must be published by (`ed25519:...`). Without it,
    /// any good signature passes and the publisher is printed.
    #[arg(long, value_name = "KEY")]
    pub publisher: Option<String>,
    /// A handover or revocation note to apply, as a node that holds it
    /// would. Repeat for more.
    #[arg(long = "note", value_name = "FILE")]
    pub notes: Vec<PathBuf>,
}

#[derive(Debug, Args)]
pub struct NoteArgs {
    pub kind: NoteKind,
    /// The key that signs the note: the publisher's for a delegation, the
    /// old key for a handover, and for a revocation the key itself or the
    /// publisher that delegated to it.
    #[arg(long, value_name = "FILE")]
    pub key: PathBuf,
    /// The key the note is about (`ed25519:...`). A revocation without it
    /// revokes the signing key itself.
    #[arg(long, value_name = "KEY")]
    pub to: Option<String>,
    /// For a delegation: a set name or `<scope>/<start>*` it covers. Repeat
    /// for more.
    #[arg(long = "sets", value_name = "PATTERN")]
    pub sets: Vec<String>,
    /// For a delegation: when it ends, a UTC timestamp. Renew it before.
    #[arg(long, value_name = "TIMESTAMP")]
    pub until: Option<String>,
    /// Where to save the note. An existing file is never overwritten.
    #[arg(long, value_name = "FILE")]
    pub out: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum NoteKind {
    Delegate,
    Handover,
    Revoke,
}

pub fn run(args: SetArgs) -> Result<()> {
    match args.command {
        SetCommand::NewKey { out } => {
            let key = new_key(&out)?;
            println!("{}", PublicKey::of(&key));
            Ok(())
        }
        SetCommand::FromPages(args) => {
            let (written, differ) = from_pages(&args)?;
            let name = args.out.display().to_string();
            println!(
                "{name}: {} items in {} parts; sign it with `plumb set sign`",
                written.items, written.parts,
            );
            differ.print(&name);
            Ok(())
        }
        SetCommand::PageSets(args) => page_sets(&args),
        SetCommand::Denylist(args) => {
            let written = denylist(&args)?;
            println!(
                "{}: {} entries in {} parts; sign it with `plumb set sign`",
                args.out.display(),
                written.items,
                written.parts
            );
            Ok(())
        }
        SetCommand::DenyEntry(args) => {
            println!("{}", serde_json::to_string(&deny_entry(&args)?)?);
            Ok(())
        }
        SetCommand::SiteRules(args) => {
            let (written, checked, moved) = site_rules(&args)?;
            println!(
                "{}: {} rules in {} parts; sign it with `plumb set sign`",
                args.out.display(),
                written.items,
                written.parts
            );
            println!(
                "{checked} hosts checked, {} on a site other than the built-in lists give",
                moved.len()
            );
            for moved in moved.iter().take(50) {
                println!(
                    "  {}: {} -> {}",
                    moved.host,
                    moved.built_in.as_deref().unwrap_or("(no site)"),
                    moved.by_rules.as_deref().unwrap_or("(no site)")
                );
            }
            Ok(())
        }
        SetCommand::Sign(args) => {
            let key = read_key(&args.key)?;
            let delegation = args.delegation.as_deref().map(read_json).transpose()?;
            let checked = sign_set(&args.dir, &key, args.prev.as_deref(), delegation)?;
            println!(
                "signed {} generation {} as {}, published by {}: {} items",
                checked.set,
                checked.generation,
                PublicKey::of(&key),
                checked.publisher,
                checked.items
            );
            Ok(())
        }
        SetCommand::Check(args) => {
            let publisher = args
                .publisher
                .as_deref()
                .map(str::parse::<PublicKey>)
                .transpose()?;
            let notes = args
                .notes
                .iter()
                .map(|path| {
                    Note::from_signed_value(&read_json(path)?)
                        .with_context(|| format!("{} is not a good key note", path.display()))
                })
                .collect::<Result<Vec<_>>>()?;
            let checked = check_set(&args.dir, publisher.as_ref(), &notes)?;
            println!(
                "{} generation {} by {}: {} items in {} parts, all good",
                checked.set, checked.generation, checked.publisher, checked.items, checked.parts
            );
            if checked.newer > 0 {
                println!(
                    "{} of its entries are newer than this version, which applies the rest",
                    checked.newer
                );
            }
            Ok(())
        }
        SetCommand::Note(args) => {
            let key = read_key(&args.key)?;
            let note = write_note(&args, &key)?;
            println!(
                "{}: {} note by {} about {}",
                args.out.display(),
                note.kind,
                note.by,
                note.key
            );
            Ok(())
        }
    }
}

/// Signs the note `args` asks for with `key` and saves it.
pub fn write_note(args: &NoteArgs, key: &SigningKey) -> Result<Note> {
    let by = PublicKey::of(key);
    let about = match (&args.to, args.kind) {
        (Some(to), _) => to.parse::<PublicKey>()?,
        (None, NoteKind::Revoke) => by,
        (None, _) => bail!("say which key the note is about with --to"),
    };
    let kind = match args.kind {
        NoteKind::Delegate => "delegate",
        NoteKind::Handover => "handover",
        NoteKind::Revoke => "revoke",
    };
    let mut value = json!({
        "format": NOTE_FORMAT,
        "kind": kind,
        "by": by.to_string(),
        "key": about.to_string(),
        "issued": now(),
    });
    if !args.sets.is_empty() {
        value["sets"] = json!(args.sets);
    }
    if let Some(until) = &args.until {
        value["until"] = json!(until);
    }
    sign_note(&mut value, key)?;
    let note = Note::from_signed_value(&value)?;
    let mut text = serde_json::to_string_pretty(&value)?;
    text.push('\n');
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args.out)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .with_context(|| format!("writing {} (it must not exist yet)", args.out.display()))?;
    Ok(note)
}

/// Makes a set-signing key and saves it at `path`, readable by its owner
/// only.
pub fn new_key(path: &Path) -> Result<SigningKey> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|err| anyhow::anyhow!("no randomness: {err}"))?;
    let key = SigningKey::from_bytes(&seed);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("making {} (it must not exist yet)", path.display()))?;
    writeln!(file, "{KEY_PREFIX}{}", URL_SAFE_NO_PAD.encode(seed))?;
    file.sync_all()?;
    Ok(key)
}

/// Reads a key saved by [`new_key`].
pub fn read_key(path: &Path) -> Result<SigningKey> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let seed: [u8; 32] = text
        .trim()
        .strip_prefix(KEY_PREFIX)
        .and_then(|encoded| URL_SAFE_NO_PAD.decode(encoded).ok())
        .and_then(|bytes| bytes.try_into().ok())
        .with_context(|| format!("{} is not a set-signing key", path.display()))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// What [`from_pages`] wrote.
#[derive(Debug, PartialEq)]
pub struct Written {
    pub items: u64,
    pub parts: usize,
    /// Pages that gave no item, or the same id as a page before them.
    pub left_out: u64,
}

/// What of a page set does not come over to its items.
#[derive(Debug, Default)]
pub struct Differ {
    /// Pages whose item gives them back otherwise ([`page_of_item`]), by
    /// the member that differs.
    pub members: BTreeMap<String, Differs>,
    /// Pages left out, by why: how many, and the first one.
    pub left_out: BTreeMap<String, (u64, String)>,
}

/// Pages one member comes back otherwise on: how many, and the first one,
/// with that member as the page has it and as its item gives it back.
#[derive(Debug)]
pub struct Differs {
    pub pages: u64,
    pub url: String,
    pub page: String,
    pub back: String,
}

impl Differ {
    /// Whether every page comes back the same from its item.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty() && self.left_out.is_empty()
    }

    /// Whether every page comes back the same from its item but for
    /// pages that repeat an earlier page's id and URL, which are left
    /// out: the same link listed twice.
    pub fn only_repeats(&self) -> bool {
        self.members.is_empty() && self.left_out.keys().all(|why| why == REPEAT)
    }

    /// Says what of the set `name` does not come over, or that all of it
    /// does.
    pub fn print(&self, name: &str) {
        if self.is_empty() {
            println!("{name}: every page comes back the same from its item");
        } else if self.only_repeats() {
            println!(
                "{name}: every page comes back the same from its item but {} that repeat an earlier page",
                self.left_out[REPEAT].0
            );
        }
        let mut left_out: Vec<_> = self.left_out.iter().collect();
        left_out.sort_by_key(|(_, (pages, _))| std::cmp::Reverse(*pages));
        for (why, (pages, first)) in left_out.iter().take(10) {
            println!("{name}: {pages} pages left out, first {first}: {why}");
        }
        if left_out.len() > 10 {
            println!(
                "{name}: and pages left out for {} other reasons",
                left_out.len() - 10
            );
        }
        for (member, differs) in &self.members {
            println!(
                "{name}: {member} comes back otherwise on {} pages, first {}: {} as {}",
                differs.pages, differs.url, differs.page, differs.back
            );
        }
    }

    fn leave_out(&mut self, why: &str, first: impl FnOnce() -> String) {
        self.left_out
            .entry(why.to_string())
            .or_insert_with(|| (0, first()))
            .0 += 1;
    }
}

/// Why a page with the id and URL of a page before it is left out.
const REPEAT: &str = "an earlier page has its id and URL";

/// `page`'s member `name` as JSON, with what NFC could change shown
/// ([`visible`]), cut to 300 characters.
fn member_of(page: Option<&Page>, name: &str) -> String {
    let value = page
        .and_then(|page| serde_json::to_value(page).ok())
        .and_then(|page| page.get(name).cloned())
        .unwrap_or(Value::Null);
    let text = visible(&value.to_string());
    match text.char_indices().nth(300) {
        Some((at, _)) => format!("{}...", &text[..at]),
        None => text,
    }
}

/// Writes the page set in `args.pages` as a set of items, and says which
/// pages do not come back the same from their items.
pub fn from_pages(args: &FromPagesArgs) -> Result<(Written, Differ)> {
    let name = args
        .name
        .clone()
        .unwrap_or_else(|| format!("plumb/{}", args.set));
    ensure!(is_set_name(&name), "{name:?} is not <scope>/<name>");
    ensure!(
        Page::has_reader(&args.set),
        "{} is not a page set this version reads",
        args.set
    );
    let reader = plumb_ingest::open_maybe_gz(&args.pages)?;
    let mut read_error = None;
    let lines = reader
        .lines()
        .map_while(|line| line.map_err(|err| read_error = Some(err)).ok());
    let mut bad = 0u64;
    let pages = plumb_core::article::articles_of(lines).filter_map(|(n, article)| match article {
        // Files made before fetch-pages left it out.
        Ok(article) if article.title == "Main Page" => None,
        Ok(article) => Page::from_set(&args.set, article),
        Err(err) => {
            bad += 1;
            if bad <= 3 {
                tracing::warn!("{} line {n}: {err:#}", args.pages.display());
            }
            None
        }
    });
    let mut manifest = Map::new();
    manifest.insert("format".into(), json!(FORMAT));
    manifest.insert("set".into(), json!(name));
    manifest.insert("generation".into(), json!(now()));
    for (key, value) in [
        ("title", &args.title),
        ("license", &args.license),
        ("attribution", &args.attribution),
    ] {
        if let Some(value) = value {
            manifest.insert(key.into(), json!(value));
        }
    }
    if let Some(counts) = &args.weight_counts {
        manifest.insert("weight".into(), json!({"counts": counts, "scale": "log"}));
    }
    manifest.insert("order".into(), json!("weight"));
    let mut differ = Differ::default();
    // Each id's URL, by hash, to tell a link listed twice from two pages
    // that share an id.
    let mut ids = HashMap::new();
    let items = pages.map(|page| {
        let item = match page_item_or_why(&page) {
            Ok(item) => item,
            Err(why) => {
                differ.leave_out(&why, || page.url.clone());
                return None;
            }
        };
        let url = id_hash(item.url.as_deref().unwrap_or_default());
        match ids.entry(id_hash(&item.id)) {
            hash_map::Entry::Vacant(vacant) => {
                vacant.insert(url);
            }
            hash_map::Entry::Occupied(earlier) if *earlier.get() == url => {
                differ.leave_out(REPEAT, || page.url.clone());
                return None;
            }
            hash_map::Entry::Occupied(_) => {
                differ.leave_out("an earlier page has its id but another URL", || {
                    format!("{} (id {})", page.url, item.id)
                });
                return None;
            }
        }
        let back = page_of_item(&item, &args.set);
        let members = back.as_ref().map_or_else(
            || vec!["page".to_string()],
            |back| differing_members(&page, back),
        );
        for member in members {
            let differs = differ
                .members
                .entry(member.clone())
                .or_insert_with(|| Differs {
                    pages: 0,
                    url: page.url.clone(),
                    page: member_of(Some(&page), &member),
                    back: member_of(back.as_ref(), &member),
                });
            differs.pages += 1;
        }
        Some(item)
    });
    let written = write_set(
        items,
        page_fields(),
        manifest,
        &args.out,
        PART_ENDS,
        args.level,
    )?;
    if let Some(err) = read_error {
        // A cut-off file must not become a set that looks whole.
        fs::remove_file(args.out.join(MANIFEST))?;
        bail!(
            "reading {}: {err}; {} has no manifest",
            args.pages.display(),
            args.out.display()
        );
    }
    Ok((written, differ))
}

/// The license, the attribution and what the weight counts of the
/// project's page set `id`, as the project picked them (`licenses.md` in
/// the design notes): a set takes the license of what it is made from, and
/// what is read off public pages is `LicenseRef-plumb-crawl`.
pub fn page_set_terms(id: &str) -> (&'static str, Option<&'static str>, Option<&'static str>) {
    use plumb_index::pages::{
        BOOKS_SET, FILMS_SET, GITHUB_SET, MUSIC_SET, PACKAGES_SET, PAPERS_SET, PODCASTS_SET,
        STACKEXCHANGE_SET, STACKOVERFLOW_SET, WIKIDATA_SET, WIKTIONARY_SET,
    };
    match id {
        set if set.starts_with("wikipedia-") => (
            "CC-BY-SA-4.0",
            Some("From Wikipedia ({url})"),
            Some("page views"),
        ),
        STACKOVERFLOW_SET | STACKEXCHANGE_SET => {
            ("CC-BY-SA-4.0", Some("From Stack Exchange ({url})"), None)
        }
        WIKTIONARY_SET => ("CC-BY-SA-4.0", Some("From Wiktionary ({url})"), None),
        PACKAGES_SET => (
            "CC-BY-SA-4.0",
            Some("From ecosyste.ms"),
            Some("downloads, as a share of its registry's most downloaded package"),
        ),
        WIKIDATA_SET | FILMS_SET => ("CC0-1.0", Some("From Wikidata"), Some("sitelinks")),
        BOOKS_SET => ("CC0-1.0", Some("From Open Library"), None),
        MUSIC_SET => ("CC0-1.0", Some("From MusicBrainz"), Some("listeners")),
        PAPERS_SET => ("CC0-1.0", Some("From OpenAlex"), Some("citations")),
        PODCASTS_SET => ("LicenseRef-podcast-index", Some("From Podcast Index"), None),
        GITHUB_SET => ("LicenseRef-plumb-crawl", None, Some("stars")),
        // Docs, reference and subpages: read off the pages themselves.
        _ => ("LicenseRef-plumb-crawl", None, None),
    }
}

/// Writes each page set whose whole file is in `args.data` as the
/// project's item set of it, and says how each went.
pub fn page_sets(args: &PageSetsArgs) -> Result<()> {
    let mut written = 0;
    for set in crate::pages::SETS {
        if !Page::has_reader(set.id)
            || !(args.only.is_empty() || args.only.iter().any(|id| id == set.id))
        {
            continue;
        }
        let file = set.file(&args.data);
        let file = plumb_net::pages::generation_file(&file).unwrap_or(file);
        if !file.is_file() {
            continue;
        }
        if set
            .file_notes(&args.data)
            .is_some_and(|notes| !notes.complete)
        {
            println!("{}: left out: the node holds only part of it", set.id);
            continue;
        }
        let (license, attribution, weight_counts) = page_set_terms(set.id);
        let name = format!("plumb/{}", set.id);
        let from = FromPagesArgs {
            pages: file,
            set: set.id.to_string(),
            name: Some(name.clone()),
            out: args.out.join(&name),
            title: Some(set.name.to_string()),
            license: Some(license.to_string()),
            attribution: attribution.map(str::to_string),
            weight_counts: weight_counts.map(str::to_string),
            level: args.level,
        };
        let (made, differ) = from_pages(&from).with_context(|| format!("writing {name}"))?;
        let bytes: u64 = fs::read_dir(&from.out)?
            .flatten()
            .filter_map(|entry| entry.metadata().ok())
            .map(|meta| meta.len())
            .sum();
        println!(
            "{name}: {} items in {} parts, {bytes} bytes, {} pages left out",
            made.items, made.parts, made.left_out
        );
        differ.print(&name);
        written += 1;
    }
    ensure!(
        written > 0,
        "no whole page set file in {}",
        args.data.display()
    );
    println!("sign each with `plumb set sign <DIR> --key <KEY>`, then check it");
    Ok(())
}

/// Writes the entries in `args.entries` as a denylist. Every line must be
/// an entry this version understands, with an id of its own: a list that
/// quietly lost an entry would fail open.
pub fn denylist(args: &DenylistArgs) -> Result<Written> {
    ensure!(
        is_set_name(&args.name),
        "{:?} is not <scope>/<name>",
        args.name
    );
    let text = fs::read_to_string(&args.entries)
        .with_context(|| format!("reading {}", args.entries.display()))?;
    let mut ids = HashSet::new();
    let mut entries = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let at = || format!("{} line {}", args.entries.display(), n + 1);
        let item: Item = serde_json::from_str(line).with_context(at)?;
        item.check().with_context(at)?;
        ensure!(
            Entry::from_item(&item).with_context(at)?.is_some(),
            "{}: this version does not understand the entry",
            at()
        );
        ensure!(
            ids.insert(item.id.clone()),
            "{}: id {:?} is used before",
            at(),
            item.id
        );
        entries.push(item);
    }
    let mut manifest = Map::new();
    manifest.insert("format".into(), json!(FORMAT));
    manifest.insert("set".into(), json!(args.name));
    manifest.insert("generation".into(), json!(now()));
    for (key, value) in [("title", &args.title), ("license", &args.license)] {
        if let Some(value) = value {
            manifest.insert(key.into(), json!(value));
        }
    }
    let items = entries.into_iter().map(Some);
    write_set(
        items,
        BTreeMap::new(),
        manifest,
        &args.out,
        PART_ENDS,
        args.level,
    )
}

/// Writes the rules of the Public Suffix List in `args.psl` and the
/// built-in subsite lists as a site rules set, then checks the hosts the
/// rules name, and those in `args.hosts`, against the built-in lists.
/// Returns what it wrote, how many hosts it checked and those whose site
/// the set moves.
pub fn site_rules(args: &SiteRulesArgs) -> Result<(Written, usize, Vec<Moved>)> {
    ensure!(
        is_set_name(&args.name),
        "{:?} is not <scope>/<name>",
        args.name
    );
    let text =
        fs::read_to_string(&args.psl).with_context(|| format!("reading {}", args.psl.display()))?;
    let mut rules = read_suffix_list(&text).with_context(|| args.psl.display().to_string())?;
    rules.extend(built_in_rules());
    let mut hosts = hosts_to_check(&rules);
    if let Some(path) = &args.hosts {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        hosts.extend(
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(String::from),
        );
    }
    let set: SiteRules = rules.iter().cloned().collect();
    let moved = moved_sites(&set, hosts.iter().map(String::as_str));
    let mut manifest = Map::new();
    manifest.insert("format".into(), json!(FORMAT));
    manifest.insert("set".into(), json!(args.name));
    manifest.insert("generation".into(), json!(now()));
    manifest.insert("license".into(), json!(args.license));
    if let Some(title) = &args.title {
        manifest.insert("title".into(), json!(title));
    }
    let items = rules.iter().map(|rule| {
        let item: Item = serde_json::from_value(json!({"id": rule.to_string(), "kind": SITE_RULE}))
            .expect("a rule is an item");
        Some(item)
    });
    let written = write_set(
        items,
        BTreeMap::new(),
        manifest,
        &args.out,
        PART_ENDS,
        args.level,
    )?;
    Ok((written, hosts.len(), moved))
}

/// The entry `args` describes, checked as a node reads it.
pub fn deny_entry(args: &DenyEntryArgs) -> Result<Item> {
    let target = match (&args.target, &args.hash_of) {
        (Some(target), None) => target.clone(),
        (None, Some(url)) => {
            let hashed = if args.any_query {
                url_sha256_any_query(url)
            } else {
                url_sha256(url)
            };
            hashed.with_context(|| format!("{url:?} is not an http or https URL"))?
        }
        _ => bail!("give one of --target and --hash-of"),
    };
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let id = args.id.clone().unwrap_or_else(|| {
        let hash = Sha256::digest(target.as_bytes());
        let start: String = hash[..6].iter().map(|b| format!("{b:02x}")).collect();
        format!("{today}-{start}")
    });
    let places = if args.places.is_empty() {
        vec!["*".to_string()]
    } else {
        args.places.clone()
    };
    let item: Item = serde_json::from_value(json!({
        "id": id, "kind": DENY, "target": target, "category": args.category,
        "where": places, "added": today,
    }))?;
    item.check()?;
    ensure!(
        Entry::from_item(&item)?.is_some(),
        "this version does not understand {target:?}"
    );
    Ok(item)
}

/// Writes `items` as the parts of a set in `out`, with `manifest` (not
/// yet signed) completed by the parts, kinds and the fields of `declared`
/// the items use. A `None`, or an id seen before, is left out.
pub(crate) fn write_set(
    items_in: impl Iterator<Item = Option<Item>>,
    declared: BTreeMap<String, FieldSpec>,
    mut manifest: Map<String, Value>,
    out: &Path,
    part_ends: &[u64],
    level: i32,
) -> Result<Written> {
    if out.exists() {
        ensure!(
            fs::read_dir(out)?.next().is_none(),
            "{} is not empty",
            out.display()
        );
    }
    fs::create_dir_all(out).with_context(|| format!("making {}", out.display()))?;
    let (mut ids, mut kinds, mut used) = (HashSet::new(), BTreeSet::new(), BTreeSet::new());
    let mut files = Vec::new();
    let mut part: Option<(String, zstd::Encoder<'static, BufWriter<File>>, u64)> = None;
    let (mut items, mut left_out) = (0u64, 0u64);
    for item in items_in {
        let Some(item) = item.filter(|item| ids.insert(id_hash(&item.id))) else {
            left_out += 1;
            continue;
        };
        if part.is_none() {
            let path = format!("items-{:03}.jsonl.zst", files.len() + 1);
            let file = File::create(out.join(&path))?;
            part = Some((path, zstd::Encoder::new(BufWriter::new(file), level)?, 0));
        }
        let (_, encoder, count) = part.as_mut().expect("a part is open");
        serde_json::to_writer(&mut *encoder, &item)?;
        encoder.write_all(b"\n")?;
        *count += 1;
        items += 1;
        kinds.insert(item.kind);
        used.extend(item.fields.into_keys());
        if part_ends.contains(&items) {
            files.push(finish_part(out, part.take().expect("a part is open"))?);
        }
    }
    if let Some(open) = part {
        files.push(finish_part(out, open)?);
    }
    ensure!(items > 0, "no item to write");
    let fields: BTreeMap<_, _> = declared
        .into_iter()
        .filter(|(name, _)| used.contains(name))
        .collect();
    manifest.insert("kinds".into(), json!(kinds));
    manifest.insert("files".into(), json!(files));
    if !fields.is_empty() {
        manifest.insert("fields".into(), json!(fields));
    }
    write_manifest(out, &Value::Object(manifest))?;
    Ok(Written {
        items,
        parts: files.len(),
        left_out,
    })
}

fn finish_part(
    out: &Path,
    (path, encoder, count): (String, zstd::Encoder<'static, BufWriter<File>>, u64),
) -> Result<Value> {
    let file = encoder
        .finish()?
        .into_inner()
        .map_err(|err| err.into_error())?;
    file.sync_all()?;
    let (bytes, sha256) = measure(&out.join(&path))?;
    Ok(json!({"path": path, "count": count, "bytes": bytes, "sha256": sha256}))
}

/// What [`sign_set`] and [`check_set`] found.
#[derive(Debug)]
pub struct Checked {
    pub set: String,
    pub generation: String,
    pub publisher: PublicKey,
    pub parts: usize,
    pub items: u64,
    /// A denylist's entries, or a site rules set's rules, newer than this
    /// version understands.
    pub newer: u64,
}

/// Signs the manifest in `dir` with `key`, after measuring each part again
/// and checking every item, so a signature never covers a set that a node
/// would refuse. `key` publishes the set, or with `delegation` (a note
/// from the publisher to `key`) signs it for the publisher.
pub fn sign_set(
    dir: &Path,
    key: &SigningKey,
    prev: Option<&str>,
    delegation: Option<Value>,
) -> Result<Checked> {
    let mut value = read_manifest(dir)?;
    let map = value
        .as_object_mut()
        .context("the manifest is not a JSON object")?;
    map.remove("signature");
    map.remove("signed_by");
    map.remove("delegation");
    let signer = PublicKey::of(key);
    match delegation {
        Some(delegation) => {
            let note = Note::from_signed_value(&delegation).context("the delegation")?;
            ensure!(
                note.kind == "delegate" && note.key == signer,
                "the delegation is not to {signer}"
            );
            map.insert("publisher".into(), json!(note.by.to_string()));
            map.insert("signed_by".into(), json!(signer.to_string()));
            map.insert("delegation".into(), delegation);
        }
        None => {
            map.insert("publisher".into(), json!(signer.to_string()));
        }
    }
    if let Some(prev) = prev {
        map.insert("prev".into(), json!(prev));
    }
    if let Some(files) = map.get_mut("files").and_then(Value::as_array_mut) {
        for part in files {
            let path = part
                .get("path")
                .and_then(Value::as_str)
                .context("a part has no path")?;
            let (bytes, sha256) = measure(&dir.join(path))?;
            part["bytes"] = json!(bytes);
            part["sha256"] = json!(sha256);
        }
    }
    let manifest: Manifest = serde_json::from_value(value.clone())?;
    manifest.check()?;
    let checked = check_parts(dir, &manifest, usize::MAX)?;
    let missing = manifest.not_understood();
    ensure!(
        missing.is_empty() && checked.newer == 0,
        "this version does not understand all of {} ({missing:?} required, {} entries); \
         sign it with a newer one",
        manifest.set,
        checked.newer
    );
    sign(&mut value, key)?;
    // What a node would read: a delegation that does not cover the set
    // now is refused here, before anything is written.
    Manifest::from_signed_json(&serde_json::to_string(&value)?, &now())?;
    write_manifest(dir, &value)?;
    Ok(checked)
}

/// Checks the set in `dir`: its manifest's signature and rules, then each
/// part's size, hash and count, and every item. With `trusted`, the set
/// must be published by that key or the key it handed over to in `notes`.
/// A signer revoked in `notes` is refused either way.
pub fn check_set(dir: &Path, trusted: Option<&PublicKey>, notes: &[Note]) -> Result<Checked> {
    check_first_parts(dir, trusted, notes, usize::MAX)
}

/// [`check_set`] for a node that holds only the first `parts` parts of a
/// set, as it may of a page set: the manifest is checked whole, and the
/// parts it holds.
pub fn check_first_parts(
    dir: &Path,
    trusted: Option<&PublicKey>,
    notes: &[Note],
    parts: usize,
) -> Result<Checked> {
    let text = fs::read_to_string(dir.join(MANIFEST))
        .with_context(|| format!("reading {}", dir.join(MANIFEST).display()))?;
    let (manifest, signer) = Manifest::from_signed_json(&text, &now())?;
    match trusted {
        Some(trusted) => manifest.trusted(&signer, trusted, notes)?,
        None => ensure!(
            !is_revoked(&signer, &manifest.publisher, notes),
            "{signer}, which signed {}, is revoked",
            manifest.set
        ),
    }
    // A denylist is read whatever it requires: skipping it would fail open.
    let missing = manifest.not_understood();
    ensure!(
        missing.is_empty() || manifest.is_denylist(),
        "{} requires {missing:?}, which this version does not read",
        manifest.set
    );
    check_parts(dir, &manifest, parts)
}

/// The manifest and the items of the set in `dir`, part by part, as they
/// are. Check the set first ([`check_set`]): this reads what is there.
pub fn read_items(dir: &Path) -> Result<(Manifest, Vec<Item>)> {
    let text = fs::read_to_string(dir.join(MANIFEST))
        .with_context(|| format!("reading {}", dir.join(MANIFEST).display()))?;
    let (manifest, _) = Manifest::from_signed_json(&text, &now())?;
    let mut items = Vec::new();
    for part in &manifest.files {
        let path = dir.join(&part.path);
        let reader = BufReader::new(zstd::Decoder::new(File::open(&path)?)?);
        for line in reader.lines() {
            let line = line.with_context(|| format!("reading {}", path.display()))?;
            items.push(serde_json::from_str::<Item>(&line)?);
        }
    }
    Ok((manifest, items))
}

/// 64 bits of an id's SHA-256, for telling ids apart.
fn id_hash(id: &str) -> u64 {
    let digest = Sha256::digest(id.as_bytes());
    u64::from_le_bytes(digest[..8].try_into().expect("8 bytes"))
}

fn check_parts(dir: &Path, manifest: &Manifest, parts: usize) -> Result<Checked> {
    // Ids by hash: a page set's millions of ids would take hundreds of
    // megabytes as text.
    let mut ids = HashSet::new();
    let (mut items, mut newer) = (0, 0);
    for part in manifest.files.iter().take(parts) {
        let path = dir.join(&part.path);
        let (bytes, sha256) = measure(&path)?;
        ensure!(
            bytes == part.bytes && sha256 == part.sha256,
            "{} is not the file the manifest names (size or hash differs)",
            part.path
        );
        let reader = BufReader::new(zstd::Decoder::new(File::open(&path)?)?);
        let mut count = 0;
        for (n, line) in reader.lines().enumerate() {
            let line = line.with_context(|| format!("reading {}", part.path))?;
            let at = || format!("{} line {}", part.path, n + 1);
            let item: Item = serde_json::from_str(&line).with_context(at)?;
            manifest.check_item(&item).with_context(at)?;
            ensure!(
                item.may_share() || !manifest.may_share(),
                "{}: a personal item in a set that may be shared",
                at()
            );
            if manifest.is_denylist() && !item.is_deleted() && Entry::from_item(&item)?.is_none() {
                newer += 1;
            }
            if manifest.is_site_rules() && !item.is_deleted() && Rule::from_item(&item)?.is_none() {
                newer += 1;
            }
            ensure!(ids.insert(id_hash(&item.id)), "{}: an id seen before", at());
            count += 1;
        }
        ensure!(
            count == part.count,
            "{} holds {count} items, not {}",
            part.path,
            part.count
        );
        items += count;
    }
    Ok(Checked {
        set: manifest.set.clone(),
        generation: manifest.generation.clone(),
        publisher: manifest.publisher,
        parts: manifest.files.len().min(parts),
        items,
        newer,
    })
}

/// A file's size and hex SHA-256.
pub(crate) fn measure(path: &Path) -> Result<(u64, String)> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    let mut bytes = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        bytes += n as u64;
    }
    let hex = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((bytes, hex))
}

/// The time now as a UTC timestamp, as the format writes them.
pub(crate) fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn read_json(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} is not JSON", path.display()))
}

fn read_manifest(dir: &Path) -> Result<Value> {
    let path = dir.join(MANIFEST);
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} is not JSON", path.display()))
}

/// Writes the manifest beside its parts, replacing the old one only once
/// the new one is whole.
fn write_manifest(dir: &Path, manifest: &Value) -> Result<()> {
    let path = dir.join(MANIFEST);
    let partial = dir.join(".manifest.json.partial");
    let mut text = serde_json::to_string_pretty(manifest)?;
    text.push('\n');
    fs::write(&partial, text)?;
    fs::rename(&partial, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use plumb_index::items::page_item;

    #[test]
    fn a_nodes_page_sets_are_written_as_the_projects_item_sets() {
        use plumb_core::article::{write_article, Article, ARTICLES_HEADER};

        let temp = tempfile::tempdir().unwrap();
        let (data, out) = (temp.path().join("data"), temp.path().join("out"));
        let sets = crate::pages::sets_dir(&data);
        fs::create_dir_all(&sets).unwrap();
        for (file, title, item) in [
            ("wikipedia-en.tsv.gz", "Marie Curie", Some("Q7186")),
            ("books.tsv.gz", "Fantastic Mr Fox", Some("OL45804W")),
        ] {
            let mut text = ARTICLES_HEADER.as_bytes().to_vec();
            let article = Article {
                title: title.into(),
                views: 9,
                item: item.map(str::to_string),
                ..Article::default()
            };
            write_article(&mut text, &article).unwrap();
            fs::write(sets.join(file), text).unwrap();
        }
        let args = PageSetsArgs {
            data,
            out: out.clone(),
            only: Vec::new(),
            level: 3,
        };
        page_sets(&args).unwrap();
        let read = |id: &str| -> Value {
            serde_json::from_str(&fs::read_to_string(out.join(id).join(MANIFEST)).unwrap()).unwrap()
        };
        let wikipedia = read("plumb/wikipedia-en");
        assert_eq!(wikipedia["license"], "CC-BY-SA-4.0");
        assert_eq!(wikipedia["title"], "English Wikipedia articles");
        assert_eq!(wikipedia["kinds"], json!(["article"]));
        let books = read("plumb/books");
        assert_eq!(books["license"], "CC0-1.0");
        assert_eq!(books["attribution"], "From Open Library");
        assert!(!out.join("plumb/docs").exists());
        let key = SigningKey::from_bytes(&[3; 32]);
        sign_set(&out.join("plumb/books"), &key, None, None).unwrap();
        let checked = check_set(&out.join("plumb/books"), Some(&PublicKey::of(&key)), &[]).unwrap();
        assert_eq!((checked.set.as_str(), checked.items), ("plumb/books", 1));
        // Written again only into empty folders.
        assert!(page_sets(&args).is_err());
    }

    #[test]
    fn what_does_not_come_over_to_the_items_is_said_with_why() {
        use plumb_core::article::{write_article, Article, ARTICLES_HEADER};

        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("books.tsv.gz");
        let mut text = ARTICLES_HEADER.as_bytes().to_vec();
        for (title, item) in [
            ("Cafe\u{301} society", "OL1W"),
            ("Ring \u{7}the bell", "OL2W"),
            ("Twice", "OL3W"),
            ("Twice again", "OL3W"),
        ] {
            let article = Article {
                title: title.into(),
                views: 9,
                item: Some(item.into()),
                ..Article::default()
            };
            write_article(&mut text, &article).unwrap();
        }
        fs::write(&file, text).unwrap();
        let args = FromPagesArgs {
            pages: file,
            set: "books".into(),
            name: None,
            out: temp.path().join("out"),
            title: None,
            license: Some("CC0-1.0".into()),
            attribution: None,
            weight_counts: None,
            level: 3,
        };
        let (written, differ) = from_pages(&args).unwrap();
        assert_eq!(written.items, 3);
        assert!(differ.members.is_empty(), "{:?}", differ.members);
        assert_eq!(
            differ.left_out,
            BTreeMap::from([(
                REPEAT.to_string(),
                (1, "https://openlibrary.org/works/OL3W".to_string())
            )])
        );
        assert_eq!(written.left_out, 1, "counted once");
        assert!(!differ.is_empty());
        assert!(
            differ.only_repeats(),
            "a link listed twice is all that differs"
        );
        let page: Page = serde_json::from_value(json!({
            "set": "books", "url": "https://openlibrary.org/works/OL1W",
            "title": "Cafe\u{301}", "views": 1,
        }))
        .unwrap();
        assert_eq!(member_of(Some(&page), "title"), "\"Cafe\\u{301}\"");
        assert_eq!(member_of(None, "title"), "null");
    }

    fn docs_pages(count: usize) -> Vec<Page> {
        (0..count)
            .map(|n| {
                serde_json::from_value(json!({
                    "set": "docs",
                    "url": format!("https://docs.python.org/3/library/m{n}.html"),
                    "title": format!("Module {n}"),
                    "views": 1000 - n,
                    "sections": ["Functions"],
                }))
                .unwrap()
            })
            .collect()
    }

    fn written(dir: &Path, pages: Vec<Page>) -> Written {
        let manifest = json!({"format": FORMAT, "set": "plumb/docs",
                              "generation": "2026-10-12T04:00:00Z", "license": "CC-BY-4.0"});
        let Value::Object(manifest) = manifest else {
            unreachable!()
        };
        let items = pages.iter().map(page_item);
        write_set(items, page_fields(), manifest, dir, &[2, 4], 3).unwrap()
    }

    #[test]
    fn written_signed_and_checked() {
        let temp = tempfile::tempdir().unwrap();
        let (dir, key_path) = (temp.path().join("set"), temp.path().join("set.key"));
        let mut pages = docs_pages(5);
        pages.push(pages[0].clone());
        let written = written(&dir, pages);
        assert_eq!(
            written,
            Written {
                items: 5,
                parts: 3,
                left_out: 1
            }
        );
        assert!(
            check_set(&dir, None, &[]).is_err(),
            "an unsigned set does not pass"
        );

        let key = new_key(&key_path).unwrap();
        assert!(new_key(&key_path).is_err(), "a key is never overwritten");
        assert_eq!(read_key(&key_path).unwrap(), key);
        let signed = sign_set(&dir, &key, None, None).unwrap();
        assert_eq!((signed.items, signed.parts), (5, 3));

        let checked = check_set(&dir, Some(&PublicKey::of(&key)), &[]).unwrap();
        assert_eq!(checked.set, "plumb/docs");
        assert_eq!(checked.items, 5);
        let manifest = read_manifest(&dir).unwrap();
        assert_eq!(manifest["kinds"], json!(["docs-page"]));
        assert_eq!(manifest["fields"]["sections"]["type"], json!("text"));
        assert!(manifest["fields"].get("authors").is_none());
        assert_eq!(manifest["files"][0]["count"], json!(2));

        let other = SigningKey::from_bytes(&[9; 32]);
        assert!(check_set(&dir, Some(&PublicKey::of(&other)), &[]).is_err());
    }

    #[test]
    fn tampering_is_caught() {
        let temp = tempfile::tempdir().unwrap();
        let key = SigningKey::from_bytes(&[4; 32]);
        let signed_set = |name: &str, pages: usize| {
            let dir = temp.path().join(name);
            written(&dir, docs_pages(pages));
            sign_set(&dir, &key, Some("2026-10-05T04:00:00Z"), None).unwrap();
            check_set(&dir, None, &[]).unwrap();
            dir
        };

        let dir = signed_set("edited", 3);
        let mut manifest = read_manifest(&dir).unwrap();
        manifest["title"] = json!("Something else");
        write_manifest(&dir, &manifest).unwrap();
        let err = check_set(&dir, None, &[]).unwrap_err();
        assert!(format!("{err:#}").contains("signature"), "{err:#}");

        let (dir, other) = (signed_set("swapped", 3), signed_set("other", 1));
        fs::copy(
            other.join("items-001.jsonl.zst"),
            dir.join("items-002.jsonl.zst"),
        )
        .unwrap();
        let err = check_set(&dir, None, &[]).unwrap_err();
        assert!(format!("{err:#}").contains("hash"), "{err:#}");
        let err = sign_set(&dir, &key, None, None).unwrap_err();
        assert!(format!("{err:#}").contains("seen before"), "{err:#}");
    }

    #[test]
    fn a_builder_signs_for_the_project_with_its_delegation() {
        let temp = tempfile::tempdir().unwrap();
        let (root, builder) = (
            SigningKey::from_bytes(&[1; 32]),
            SigningKey::from_bytes(&[2; 32]),
        );
        let (root_key, builder_key) = (PublicKey::of(&root), PublicKey::of(&builder));
        let note = |kind, key: &SigningKey, to: Option<PublicKey>, sets: &[&str], name: &str| {
            let args = NoteArgs {
                kind,
                key: PathBuf::new(),
                to: to.map(|key| key.to_string()),
                sets: sets.iter().map(|s| s.to_string()).collect(),
                until: (kind == NoteKind::Delegate).then(|| "2099-01-01T00:00:00Z".to_string()),
                out: temp.path().join(name),
            };
            write_note(&args, key).unwrap();
            read_json(&args.out).unwrap()
        };
        let delegation = note(
            NoteKind::Delegate,
            &root,
            Some(builder_key),
            &["plumb/doc*"],
            "delegation.json",
        );
        let dir = temp.path().join("set");
        written(&dir, docs_pages(2));

        let checked = sign_set(&dir, &builder, None, Some(delegation.clone())).unwrap();
        assert_eq!(checked.publisher, root_key);
        let manifest = read_manifest(&dir).unwrap();
        assert_eq!(manifest["signed_by"], json!(builder_key.to_string()));
        check_set(&dir, Some(&root_key), &[]).unwrap();
        assert!(check_set(&dir, Some(&builder_key), &[]).is_err());

        let revoked = note(NoteKind::Revoke, &builder, None, &[], "revoke.json");
        let revoked = Note::from_signed_value(&revoked).unwrap();
        assert!(check_set(&dir, Some(&root_key), std::slice::from_ref(&revoked)).is_err());
        assert!(check_set(&dir, None, &[revoked]).is_err());

        // A delegation to another key, or for other sets, signs nothing.
        let stranger = SigningKey::from_bytes(&[3; 32]);
        assert!(sign_set(&dir, &stranger, None, Some(delegation)).is_err());
        let elsewhere = note(
            NoteKind::Delegate,
            &root,
            Some(builder_key),
            &["plumb/wikipedia-*"],
            "elsewhere.json",
        );
        assert!(sign_set(&dir, &builder, None, Some(elsewhere)).is_err());
        check_set(&dir, Some(&root_key), &[]).unwrap();

        // After a handover, the old key's sets are no longer taken.
        let handover = note(
            NoteKind::Handover,
            &root,
            Some(builder_key),
            &[],
            "handover.json",
        );
        let handover = Note::from_signed_value(&handover).unwrap();
        assert!(check_set(&dir, Some(&root_key), std::slice::from_ref(&handover)).is_err());
        sign_set(&dir, &builder, None, None).unwrap();
        check_set(&dir, Some(&root_key), &[handover]).unwrap();
        assert!(write_note(
            &NoteArgs {
                kind: NoteKind::Handover,
                key: PathBuf::new(),
                to: None,
                sets: Vec::new(),
                until: None,
                out: temp.path().join("nowhere.json"),
            },
            &root
        )
        .is_err());
    }

    #[test]
    fn a_denylist_is_written_signed_and_checked() {
        let temp = tempfile::tempdir().unwrap();
        let entry = |category: &str, target: Option<&str>, hash_of: Option<&str>| {
            let args = DenyEntryArgs {
                category: category.into(),
                target: target.map(String::from),
                hash_of: hash_of.map(String::from),
                any_query: hash_of.is_some_and(|url| url.contains('?')),
                places: Vec::new(),
                id: None,
            };
            deny_entry(&args).map(|item| serde_json::to_string(&item).unwrap())
        };
        let csam = entry("csam", None, Some("http://www.example.org/x?id=1")).unwrap();
        assert!(!csam.contains("example.org"), "{csam}");
        assert!(
            entry("csam", Some("url:https://example.org/x"), None).is_err(),
            "a csam entry names its page by hash"
        );
        assert!(entry("spam", Some("site:Example.org"), None).is_err());
        let lines = [
            csam,
            entry("copyright", Some("site:example.net"), None).unwrap(),
            entry("malware", Some("prefix:https://example.com/files/"), None).unwrap(),
        ];
        let entries = temp.path().join("entries.jsonl");
        fs::write(&entries, lines.join("\n")).unwrap();
        let args = |out: &str, entries: &Path| DenylistArgs {
            entries: entries.to_path_buf(),
            name: "plumb/denylist".into(),
            out: temp.path().join(out),
            title: None,
            license: Some("CC0-1.0".into()),
            level: 3,
        };
        let written = denylist(&args("list", &entries)).unwrap();
        assert_eq!((written.items, written.parts), (3, 1));
        let key = SigningKey::from_bytes(&[5; 32]);
        sign_set(&temp.path().join("list"), &key, None, None).unwrap();
        let checked =
            check_set(&temp.path().join("list"), Some(&PublicKey::of(&key)), &[]).unwrap();
        assert_eq!((checked.items, checked.newer), (3, 0));
        assert_eq!(
            read_manifest(&temp.path().join("list")).unwrap()["kinds"],
            json!(["deny"])
        );

        // Two entries with one id, or one this version can't read, never make a list.
        let twice = temp.path().join("twice.jsonl");
        fs::write(&twice, format!("{}\n{}", lines[1], lines[1])).unwrap();
        assert!(denylist(&args("twice", &twice)).is_err());
        let newer_line = lines[1].replace("site:example.net", "blob:sha256-00");
        let newer = temp.path().join("newer.jsonl");
        fs::write(&newer, format!("{}\n{newer_line}", lines[0])).unwrap();
        assert!(denylist(&args("newer", &newer)).is_err());

        // A list from a newer version is checked, and applied, all the same,
        // but this version won't sign what it can't read.
        let dir = temp.path().join("from-newer");
        let items = [lines[0].as_str(), &newer_line]
            .map(|line| Some(serde_json::from_str::<Item>(line).unwrap()));
        let manifest = json!({"format": FORMAT, "set": "plumb/denylist",
                              "generation": "2026-10-12T04:00:00Z", "license": "CC0-1.0",
                              "requires": ["deny-blob/1"]});
        let Value::Object(manifest) = manifest else {
            unreachable!()
        };
        write_set(items.into_iter(), BTreeMap::new(), manifest, &dir, &[], 3).unwrap();
        assert!(sign_set(&dir, &key, None, None).is_err());
        let mut value = read_manifest(&dir).unwrap();
        value["publisher"] = json!(PublicKey::of(&key).to_string());
        sign(&mut value, &key).unwrap();
        write_manifest(&dir, &value).unwrap();
        let checked = check_set(&dir, None, &[]).unwrap();
        assert_eq!((checked.items, checked.newer), (2, 1));
    }

    #[test]
    fn a_bad_item_is_never_signed() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("set");
        written(&dir, docs_pages(1));
        let part = dir.join("items-001.jsonl.zst");
        let bad = r#"{"id":"x","kind":"docs-page","url":"https://Example.com/"}"#;
        fs::write(&part, zstd::encode_all(bad.as_bytes(), 3).unwrap()).unwrap();
        let err = sign_set(&dir, &SigningKey::from_bytes(&[4; 32]), None, None).unwrap_err();
        assert!(format!("{err:#}").contains("line 1"), "{err:#}");
        assert!(read_manifest(&dir).unwrap().get("signature").is_none());
    }
}
