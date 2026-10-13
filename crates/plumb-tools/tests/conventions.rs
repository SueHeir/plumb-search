//! Counts the patterns CLAUDE.md asks node code to avoid, and fails when
//! a count goes up. Code that already has them is left as it is: each
//! limit is today's count, and the change that removes some lowers it.
//!
//! It reads the node's crates' source (not tests, comments or strings),
//! so it needs no build of them.

use std::fs;
use std::path::{Path, PathBuf};

/// The crates the `plumb` binary is built from.
const NODE_CRATES: &[&str] = &[
    "plumb-core",
    "plumb-format",
    "plumb-answer",
    "plumb-ingest",
    "plumb-crawl",
    "plumb-index",
    "plumb-embed",
    "plumb-net",
    "plumb-node",
    "plumb-plugin",
];

/// Longer functions than this are counted.
const LONG_FUNCTION_LINES: usize = 150;
/// Longer files than this, tests and comments included, are counted.
const LONG_FILE_LINES: usize = 3_000;

/// What is counted, and how many there may be.
struct Limits {
    /// `.lock().unwrap()` and the like: a std lock that panics if a holder
    /// panicked. `plumb_core::locks` has `locked()`.
    raw_locks: usize,
    /// `fs::write`, `fs::rename` and `File::create` outside
    /// `plumb_core::storage`: writes that are not atomic or not charged
    /// to the storage budget.
    direct_writes: usize,
    /// `tokio::spawn` in plumb-node: a background task, which `supervise`
    /// would restart.
    bare_spawns: usize,
    /// `.unwrap()` outside tests.
    unwraps: usize,
    /// Functions longer than [`LONG_FUNCTION_LINES`], outside tests.
    long_functions: usize,
    /// Files longer than [`LONG_FILE_LINES`].
    long_files: usize,
}

/// The counts on 2026-10-13.
const LIMITS: Limits = Limits {
    raw_locks: 2,
    direct_writes: 62,
    bare_spawns: 17,
    unwraps: 19,
    long_functions: 30,
    long_files: 0,
};

const RAW_LOCKS: &[&str] = &[
    ".lock().unwrap()",
    ".lock().expect(",
    ".read().unwrap()",
    ".read().expect(",
    ".write().unwrap()",
    ".write().expect(",
];
const DIRECT_WRITES: &[&str] = &["fs::write(", "fs::rename(", "File::create("];

#[test]
fn node_code_keeps_to_its_conventions() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut found = Limits {
        raw_locks: 0,
        direct_writes: 0,
        bare_spawns: 0,
        unwraps: 0,
        long_functions: 0,
        long_files: 0,
    };
    let mut long = Vec::new();
    for name in NODE_CRATES {
        for (path, test_file) in rust_files(&root.join("crates").join(name).join("src")) {
            let text = fs::read_to_string(&path).unwrap();
            let shown = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string();
            let lines = text.lines().count();
            if lines > LONG_FILE_LINES {
                found.long_files += 1;
                long.push(format!("{shown}: {lines} lines"));
            }
            if test_file {
                continue;
            }
            let code = without_tests(&blanked(&text));
            // Calls split over lines by rustfmt count too.
            let squashed: String = code.chars().filter(|c| !c.is_whitespace()).collect();
            let count = |calls: &[&str]| -> usize {
                calls
                    .iter()
                    .map(|call| squashed.matches(call).count())
                    .sum()
            };
            found.raw_locks += count(RAW_LOCKS);
            if !shown.ends_with("plumb-core/src/storage.rs") {
                found.direct_writes += count(DIRECT_WRITES);
            }
            if *name == "plumb-node" {
                found.bare_spawns += count(&["tokio::spawn("]);
            }
            found.unwraps += count(&[".unwrap()"]);
            for (function, lines) in long_functions(&code) {
                found.long_functions += 1;
                long.push(format!("{shown}: fn {function}, {lines} lines"));
            }
        }
    }
    let counts = [
        ("raw_locks", found.raw_locks, LIMITS.raw_locks),
        ("direct_writes", found.direct_writes, LIMITS.direct_writes),
        ("bare_spawns", found.bare_spawns, LIMITS.bare_spawns),
        ("unwraps", found.unwraps, LIMITS.unwraps),
        (
            "long_functions",
            found.long_functions,
            LIMITS.long_functions,
        ),
        ("long_files", found.long_files, LIMITS.long_files),
    ];
    let mut over = Vec::new();
    for (what, count, limit) in counts {
        println!("{what}: {count} (limit {limit})");
        if count > limit {
            over.push(format!("{what}: {count}, over its limit of {limit}"));
        } else if count < limit {
            println!("  {what} went down: lower its limit in LIMITS to {count}");
        }
    }
    long.sort();
    for line in &long {
        println!("long: {line}");
    }
    assert!(
        over.is_empty(),
        "node code has more of what CLAUDE.md asks it to avoid:\n{}\n\
         Use plumb_core::locks, plumb_core::storage and supervise, and keep \
         functions under {LONG_FUNCTION_LINES} lines and files under \
         {LONG_FILE_LINES}. Run with --nocapture to see the long ones.",
        over.join("\n")
    );
}

/// The `.rs` files under `dir`, each with whether it holds only tests.
fn rust_files(dir: &Path) -> Vec<(PathBuf, bool)> {
    let mut files = Vec::new();
    let mut dirs = vec![(dir.to_path_buf(), false)];
    while let Some((dir, tests)) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if path.is_dir() {
                dirs.push((path, tests || name == "tests"));
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push((path, tests || name == "tests.rs"));
            }
        }
    }
    files.sort();
    files
}

/// `text` with comments, strings and character literals turned to spaces,
/// keeping every line where it was.
fn blanked(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let blank = |out: &mut String, c: char| out.push(if c == '\n' { '\n' } else { ' ' });
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                out.push(' ');
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            let mut depth = 0;
            while i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    out.push_str("  ");
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    out.push_str("  ");
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    blank(&mut out, chars[i]);
                    i += 1;
                }
            }
        } else if let Some(hashes) = raw_string_start(&chars, i) {
            // r"..." or r#"..."#, possibly after b.
            let open = chars[i..].iter().position(|&c| c == '"').unwrap() + 1;
            for _ in 0..open {
                out.push(' ');
            }
            i += open;
            while i < chars.len() {
                if chars[i] == '"'
                    && chars[i + 1..]
                        .iter()
                        .take(hashes)
                        .filter(|&&c| c == '#')
                        .count()
                        == hashes
                {
                    for _ in 0..=hashes {
                        out.push(' ');
                    }
                    i += 1 + hashes;
                    break;
                }
                blank(&mut out, chars[i]);
                i += 1;
            }
        } else if c == '"' {
            out.push(' ');
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    blank(&mut out, chars[i]);
                    i += 1;
                }
                if i < chars.len() {
                    blank(&mut out, chars[i]);
                    i += 1;
                }
            }
            out.push(' ');
            i += 1;
        } else if c == '\'' && is_char_literal(&chars, i) {
            out.push(' ');
            i += 1;
            while i < chars.len() && chars[i] != '\'' {
                if chars[i] == '\\' {
                    out.push(' ');
                    i += 1;
                }
                out.push(' ');
                i += 1;
            }
            out.push(' ');
            i += 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// The number of `#`s of a raw string starting at `i`, if one does.
fn raw_string_start(chars: &[char], i: usize) -> Option<usize> {
    let before = i.checked_sub(1).map(|b| chars[b]);
    if before.is_some_and(|b| b.is_alphanumeric() || b == '_') {
        return None;
    }
    let mut j = i;
    if chars.get(j) == Some(&'b') {
        j += 1;
    }
    if chars.get(j) != Some(&'r') {
        return None;
    }
    j += 1;
    let hashes = chars[j..].iter().take_while(|&&c| c == '#').count();
    (chars.get(j + hashes) == Some(&'"')).then_some(hashes)
}

/// Whether the `'` at `i` starts a character literal rather than a
/// lifetime or label.
fn is_char_literal(chars: &[char], i: usize) -> bool {
    match chars.get(i + 1) {
        Some('\\') => true,
        Some(_) => chars.get(i + 2) == Some(&'\''),
        None => false,
    }
}

/// `code` with the items under `#[cfg(test)]` turned to blank lines.
fn without_tests(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut keep = vec![true; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() != "#[cfg(test)]" {
            i += 1;
            continue;
        }
        let start = i;
        let mut depth = 0i64;
        let mut opened = false;
        while i < lines.len() {
            let line = lines[i];
            depth += line.matches('{').count() as i64 - line.matches('}').count() as i64;
            opened |= line.contains('{');
            i += 1;
            let item = i - 1 > start;
            if item && ((opened && depth <= 0) || (!opened && line.trim_end().ends_with(';'))) {
                break;
            }
        }
        for skip in &mut keep[start..i] {
            *skip = false;
        }
    }
    lines
        .iter()
        .zip(keep)
        .map(|(line, keep)| if keep { *line } else { "" })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The functions in `code` longer than [`LONG_FUNCTION_LINES`], with their
/// length.
fn long_functions(code: &str) -> Vec<(String, usize)> {
    let lines: Vec<&str> = code.lines().collect();
    let mut long = Vec::new();
    for (start, line) in lines.iter().enumerate() {
        let Some(at) = function_name_at(line) else {
            continue;
        };
        let name: String = line[at..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        // The body starts at the first `{` before any `;` (a declaration
        // without a body has none).
        let mut depth = 0i64;
        let mut opened = false;
        let mut end = None;
        'lines: for (n, line) in lines.iter().enumerate().skip(start) {
            let from = if n == start { at } else { 0 };
            for c in line[from..].chars() {
                match c {
                    ';' if !opened => break 'lines,
                    '{' => {
                        opened = true;
                        depth += 1;
                    }
                    '}' => {
                        depth -= 1;
                        if opened && depth == 0 {
                            end = Some(n);
                            break 'lines;
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(end) = end {
            let length = end - start + 1;
            if length > LONG_FUNCTION_LINES {
                long.push((name, length));
            }
        }
    }
    long
}

/// Where the name of a function declared on `line` starts, if one is.
fn function_name_at(line: &str) -> Option<usize> {
    let at = line.find("fn ")?;
    let before = line[..at].trim();
    let words_ok = before.split_whitespace().all(|word| {
        matches!(
            word,
            "pub" | "async" | "const" | "unsafe" | "extern" | "default"
        ) || word.starts_with("pub(")
            || word.starts_with('"')
    });
    let starts_word = at == 0 || !line[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_');
    let name = line[at + 3..].chars().next()?;
    (words_ok && starts_word && (name.is_alphabetic() || name == '_')).then_some(at + 3)
}
