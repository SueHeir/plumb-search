//! `plumb set`: sets in the data format ([`plumb_format`]) made from
//! today's page sets, signed and checked.
//!
//! A set is a folder: `manifest.json` and its parts, `items-001.jsonl.zst`
//! and on, one item per line, most weighted first. `from-pages` writes one
//! from an articles file, `sign` signs its manifest with a set-signing key
//! from `new-key`, and `check` checks a set as a node would before taking
//! it. Signing is a release step someone takes on purpose: no build signs
//! by itself.
//!
//! Nothing here changes what a node indexes or sends.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use clap::{Args, Subcommand, ValueEnum};
use ed25519_dalek::SigningKey;
use plumb_format::manifest::is_set_name;
use plumb_format::notes::{is_revoked, sign_note, NOTE_FORMAT};
use plumb_format::sign::sign;
use plumb_format::{Item, Manifest, Note, PublicKey, FORMAT};
use plumb_index::items::{page_fields, page_item};
use plumb_index::pages::Page;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// The manifest's file name in a set's folder.
pub const MANIFEST: &str = "manifest.json";

/// How many items the parts hold, counted from the most weighted: the
/// first 100,000, up to the first 1,000,000, then the rest.
const PART_ENDS: &[u64] = &[100_000, 1_000_000];

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
            let written = from_pages(&args)?;
            println!(
                "{}: {} items in {} parts, {} pages left out; sign it with `plumb set sign`",
                args.out.display(),
                written.items,
                written.parts,
                written.left_out
            );
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

pub fn from_pages(args: &FromPagesArgs) -> Result<Written> {
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
    let written = write_set(pages, manifest, &args.out, PART_ENDS, args.level)?;
    if let Some(err) = read_error {
        // A cut-off file must not become a set that looks whole.
        fs::remove_file(args.out.join(MANIFEST))?;
        bail!(
            "reading {}: {err}; {} has no manifest",
            args.pages.display(),
            args.out.display()
        );
    }
    Ok(written)
}

/// Writes `pages` as the parts of a set in `out`, with `manifest` (not
/// yet signed) completed by the parts, kinds and fields.
fn write_set(
    pages: impl Iterator<Item = Page>,
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
    let declared = page_fields();
    let (mut ids, mut kinds, mut used) = (HashSet::new(), BTreeSet::new(), BTreeSet::new());
    let mut files = Vec::new();
    let mut part: Option<(String, zstd::Encoder<'static, BufWriter<File>>, u64)> = None;
    let (mut items, mut left_out) = (0u64, 0u64);
    for page in pages {
        let Some(item) = page_item(&page).filter(|item| ids.insert(item.id.clone())) else {
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
    ensure!(items > 0, "no page read as an item");
    let fields: BTreeMap<_, _> = declared
        .into_iter()
        .filter(|(name, _)| used.contains(name))
        .collect();
    manifest.insert("kinds".into(), json!(kinds));
    manifest.insert("order".into(), json!("weight"));
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
    let checked = check_parts(dir, &manifest)?;
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
    let missing = manifest.not_understood();
    ensure!(
        missing.is_empty(),
        "{} requires {missing:?}, which this version does not read",
        manifest.set
    );
    check_parts(dir, &manifest)
}

fn check_parts(dir: &Path, manifest: &Manifest) -> Result<Checked> {
    let mut ids = HashSet::new();
    let mut items = 0;
    for part in &manifest.files {
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
                item.may_share(),
                "{}: a personal item in a shared set",
                at()
            );
            ensure!(ids.insert(item.id), "{}: an id seen before", at());
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
        parts: manifest.files.len(),
        items,
    })
}

/// A file's size and hex SHA-256.
fn measure(path: &Path) -> Result<(u64, String)> {
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
fn now() -> String {
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
        write_set(pages.into_iter(), manifest, dir, &[2, 4], 3).unwrap()
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
