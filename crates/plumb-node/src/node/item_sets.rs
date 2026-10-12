//! The item sets a node searches beside its page sets: the sets its
//! plugins build ([`crate::plugins::publish`]) and any set it holds
//! besides the denylist, the site rules and the project's page sets
//! ([`super::signed_sets`]).
//!
//! The page index ([`crate::pages`]) takes them in with today's page sets.
//! Each set is checked as `plumb set check` does, then read as items. The
//! layers among them are held in memory and merged onto the things they
//! name ([`plumb_index::merge`]) as the things are read, one at a time.
//! What the denylist denies is left out before anything is indexed, the
//! layers included. Each thing becomes a page as its set says
//! ([`plumb_index::items::item_page`]): its title, the line under it and
//! where it is listed. The layers are laid on the pages of today's page
//! sets too ([`PageLayers`]), which count as the project's.
//!
//! Whose a set is decides how its layers rank: the owner's own sets,
//! then the project's, then any other publisher the owner trusts for sets
//! ([`super::NodeConfig::set_publishers`]), in the order given.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result};
use plumb_format::deny::{Denylist, DENY};
use plumb_format::manifest::is_set_name;
use plumb_format::sites::SITE_RULE;
use plumb_format::{Item, Manifest, PublicKey};
use plumb_index::items::{item_page, layered_page, page_item, page_stub, Place, SetView};
use plumb_index::merge::{merge, Layers, Source, Sourced, Trust, Weight};
use plumb_index::pages::Page;
use tracing::warn;

use super::signed_sets::{project_key, DIR, TAKEN_SETS};
use crate::plugins::publish::{OWN_DIR, OWN_SCOPE};
use crate::sets::{check_set, items_in, now, MANIFEST};

/// An item set the page index takes in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemSetFile {
    pub dir: PathBuf,
    /// `<scope>/<name>`.
    pub set: String,
    /// The key it must be published by: the owner's, for their own sets.
    pub publisher: PublicKey,
    pub trust: Trust,
    /// Changes whenever what is indexed of the set would: the hashes of
    /// its parts, its title, its lines and its place. Not its generation,
    /// so a run that changed nothing builds nothing again.
    pub stamp: String,
}

/// The item sets in `data` the node searches: its own, those of `own`'s
/// set names (`<scope>/<name>`) published with its key (the node's
/// publisher key, `None` before its first plugin run), and those it holds
/// that one of `publishers` published. The denylist, the site rules, sets
/// this version can't read and sets placed `answer-only` are left out.
pub fn find(
    data: &Path,
    own: Option<(&PublicKey, &[String])>,
    publishers: &[PublicKey],
) -> Vec<ItemSetFile> {
    let mut found = Vec::new();
    if let Some((key, sets)) = own {
        for (set, dir) in set_dirs(&data.join(OWN_DIR)) {
            if set.split('/').next() == Some(OWN_SCOPE) && sets.contains(&set) {
                found.extend(item_set(set, dir, &[*key], |_| Trust::Owner));
            }
        }
    }
    let project = project_key();
    for (set, dir) in set_dirs(&data.join(DIR)) {
        // The project's page sets are read as the page sets they are
        // ([`crate::pages::HeldPageSet`]).
        let page_set = set
            .strip_prefix("plumb/")
            .is_some_and(|id| crate::pages::SetInfo::find(id).is_some());
        if TAKEN_SETS.contains(&set.as_str()) || page_set {
            continue;
        }
        found.extend(item_set(set, dir, publishers, |publisher| {
            if publisher == project {
                Trust::Project
            } else {
                Trust::Outside {
                    weight: Weight::Low,
                    added: publishers
                        .iter()
                        .position(|key| *key == publisher)
                        .unwrap_or(publishers.len()),
                }
            }
        }));
    }
    found
}

/// The sets under `root`, laid out `<scope>/<name>/`, by name.
fn set_dirs(root: &Path) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    let Ok(scopes) = fs::read_dir(root) else {
        return found;
    };
    for scope in scopes.flatten() {
        let Ok(names) = fs::read_dir(scope.path()) else {
            continue;
        };
        for dir in names.flatten() {
            let set = format!(
                "{}/{}",
                scope.file_name().to_string_lossy(),
                dir.file_name().to_string_lossy()
            );
            // Not the `.new` and `.old` folders of a set being replaced,
            // nor a run's notes beside its set.
            if is_set_name(&set) && dir.path().join(MANIFEST).is_file() {
                found.push((set, dir.path()));
            }
        }
    }
    found.sort();
    found
}

/// The set in `dir` as the page index takes it in, when one of
/// `publishers` published it and it is one to search.
fn item_set(
    set: String,
    dir: PathBuf,
    publishers: &[PublicKey],
    trust: impl Fn(PublicKey) -> Trust,
) -> Option<ItemSetFile> {
    let read = || -> Result<Option<ItemSetFile>> {
        let text = fs::read_to_string(dir.join(MANIFEST))?;
        let (manifest, signer) = Manifest::from_signed_json(&text, &now())?;
        anyhow::ensure!(
            manifest.set == set,
            "its manifest names the set {}",
            manifest.set
        );
        let Some(publisher) = publishers
            .iter()
            .find(|key| manifest.trusted(&signer, key, &[]).is_ok())
        else {
            return Ok(None);
        };
        if manifest
            .kinds
            .iter()
            .any(|kind| kind == DENY || kind == SITE_RULE)
            || !manifest.not_understood().is_empty()
            || SetView::of(&manifest).place == Place::AnswerOnly
        {
            return Ok(None);
        }
        let mut stamp: Vec<String> = manifest
            .files
            .iter()
            .map(|part| part.sha256.clone())
            .collect();
        stamp.push(serde_json::to_string(&(
            &manifest.title,
            &manifest.show,
            &manifest.place,
        ))?);
        Ok(Some(ItemSetFile {
            dir: dir.clone(),
            set: set.clone(),
            publisher: *publisher,
            trust: trust(*publisher),
            stamp: stamp.join(","),
        }))
    };
    match read() {
        Ok(found) => found,
        Err(err) => {
            // The node looks every few seconds: say it once.
            static SAID: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
            let said = format!("item set {set}: {err:#}");
            if SAID
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(said.clone())
            {
                warn!("{said}");
            }
            None
        }
    }
}

/// What reading item sets for the page index came to.
#[derive(Debug, Default)]
pub struct ItemPages {
    /// Each set's pages, most read first, set after set.
    pub pages: Vec<Page>,
    /// The extensions held, merged onto what they name.
    pub layers: Layers,
    /// The sets read, which [`ItemPages::layers`] point into.
    pub sources: Vec<Source>,
    /// Things and extensions the denylist left out.
    pub denied: usize,
    /// Sets that failed their check or could not be read, with why.
    pub failed: Vec<(String, String)>,
}

/// The pages of `sets` (see the module notes), leaving out what
/// `denylist` denies.
pub fn pages(sets: &[ItemSetFile], denylist: Option<&Denylist>) -> ItemPages {
    let mut out = ItemPages::default();
    let mut sources: Vec<Source> = Vec::new();
    let mut readable: Vec<(&ItemSetFile, Manifest)> = Vec::new();
    for file in sets {
        let read = check_set(&file.dir, Some(&file.publisher), &[])
            .and_then(|_| read_manifest(&file.dir))
            .and_then(|manifest| {
                let source =
                    Source::of(&manifest, file.trust).context("its generation has no time")?;
                Ok((manifest, source))
            });
        match read {
            Ok((manifest, source)) => {
                sources.push(source);
                readable.push((file, manifest));
            }
            Err(err) => out.failed.push((file.set.clone(), format!("{err:#}"))),
        }
    }
    let mut failed = vec![None; readable.len()];
    let mut extensions: Vec<(Item, usize)> = Vec::new();
    for (n, (file, manifest)) in readable.iter().enumerate() {
        // A set that can't be read to its end lays nothing on the others.
        let mut held = Vec::new();
        let mut set_denied = 0;
        for item in items_in(&file.dir, manifest) {
            match item {
                Ok(item) if item.is_extension() => {
                    if denied(denylist, &item, manifest) {
                        set_denied += 1;
                    } else {
                        held.push((item, n));
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    failed[n] = Some(format!("{err:#}"));
                    break;
                }
            }
        }
        if failed[n].is_none() {
            extensions.extend(held);
            out.denied += set_denied;
        }
    }
    let layers = Layers::new(extensions);
    for (n, (file, manifest)) in readable.iter().enumerate() {
        if failed[n].is_some() {
            continue;
        }
        let view = SetView::of(manifest);
        let mut pages = Vec::new();
        for item in items_in(&file.dir, manifest) {
            let item = match item {
                Ok(item) => item,
                Err(err) => {
                    failed[n] = Some(format!("{err:#}"));
                    break;
                }
            };
            if item.is_extension() || item.is_deleted() {
                continue;
            }
            let thing = Sourced {
                item: &item,
                source: n,
            };
            let Some(merged) = merge(&sources, thing, &layers.on(&sources, thing)) else {
                continue;
            };
            if denied(denylist, &merged.item, manifest) {
                out.denied += 1;
                continue;
            }
            pages.extend(item_page(&merged, &view));
        }
        if failed[n].is_some() {
            continue;
        }
        // The page index weighs a page against the first of its set.
        pages.sort_by_key(|page| std::cmp::Reverse(page.views));
        out.pages.extend(pages);
    }
    for ((file, _), why) in readable.iter().zip(failed) {
        if let Some(why) = why {
            out.failed.push((file.set.clone(), why));
        }
    }
    out.layers = layers;
    out.sources = sources;
    out
}

/// The layers of item sets, laid on the pages of today's page sets as they
/// are indexed. A page set counts as the project's set `plumb/<id>`, as
/// `plumb set from-pages` names it, so a layer can name its pages by
/// `item:` as well as by URL or Wikidata id. A page no layer names is
/// indexed as it is.
#[derive(Debug)]
pub struct PageLayers {
    layers: Layers,
    sources: Vec<Source>,
    /// How many pages layers changed.
    laid: Cell<usize>,
}

impl PageLayers {
    /// The layers `read` holds, `None` when it holds none.
    pub fn of(read: &mut ItemPages) -> Option<PageLayers> {
        (!read.layers.is_empty()).then(|| PageLayers {
            layers: std::mem::take(&mut read.layers),
            sources: std::mem::take(&mut read.sources),
            laid: Cell::new(0),
        })
    }

    /// Adds the page set `id`, made at `modified` (Unix seconds), and
    /// gives what its pages are laid on with.
    pub fn page_set(&mut self, id: &str, modified: i64) -> usize {
        self.sources.push(Source {
            publisher: project_key().to_string(),
            set: format!("plumb/{id}"),
            generation: modified,
            trust: Trust::Project,
            same_as: Default::default(),
            labels: Vec::new(),
        });
        self.sources.len() - 1
    }

    /// `page`, of the page set `source` ([`PageLayers::page_set`]), with
    /// the layers that name it merged on.
    pub fn lay(&self, page: Page, source: usize) -> Page {
        let named = page_stub(&page).is_some_and(|stub| {
            let stub = Sourced {
                item: &stub,
                source,
            };
            !self.layers.on(&self.sources, stub).is_empty()
        });
        let Some(item) = named.then(|| page_item(&page)).flatten() else {
            return page;
        };
        let thing = Sourced {
            item: &item,
            source,
        };
        let on = self.layers.on(&self.sources, thing);
        let Some(merged) = merge(&self.sources, thing, &on) else {
            return page;
        };
        self.laid.set(self.laid.get() + 1);
        layered_page(page, &merged)
    }

    /// How many pages layers changed so far.
    pub fn laid(&self) -> usize {
        self.laid.get()
    }
}

fn read_manifest(dir: &Path) -> Result<Manifest> {
    let text = fs::read_to_string(dir.join(MANIFEST))
        .with_context(|| format!("reading {}", dir.join(MANIFEST).display()))?;
    Ok(Manifest::from_signed_json(&text, &now())?.0)
}

/// True when `denylist` takes off `item` of the set `manifest` describes.
/// A `csam` entry's hit is logged by the entry's id only.
fn denied(denylist: Option<&Denylist>, item: &Item, manifest: &Manifest) -> bool {
    let Some(denial) = denylist.and_then(|list| list.denies(item, Some(manifest))) else {
        return false;
    };
    if denial.is_locked() {
        warn!(
            "denylist entry {} ({}) left an item of {} out of the page index",
            denial.id, denial.category, manifest.set
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ed25519_dalek::SigningKey;
    use plumb_format::deny::{Entry, Switch};
    use plumb_format::FieldSpec;
    use serde_json::{json, Value};

    use super::*;
    use crate::sets::{deny_entry, sign_set, write_set, DenyEntryArgs};

    /// Writes the set `set` in `dir` and signs it with `key`.
    fn signed(dir: &Path, set: &str, key: &SigningKey, extra: Value, items: &[Value]) {
        let mut manifest = json!({
            "format": plumb_format::FORMAT, "set": set, "generation": "2026-10-12T04:00:00Z",
        });
        let Value::Object(map) = &mut manifest else {
            unreachable!()
        };
        let Value::Object(extra) = extra else {
            unreachable!()
        };
        let fields: BTreeMap<String, FieldSpec> = extra
            .get("fields")
            .map(|fields| serde_json::from_value(fields.clone()).unwrap())
            .unwrap_or_default();
        map.extend(extra);
        let items = items
            .iter()
            .map(|item| Some(serde_json::from_value::<Item>(item.clone()).unwrap()));
        write_set(items, fields, map.clone(), dir, &[], 3).unwrap();
        sign_set(dir, key, None, None).unwrap();
    }

    #[test]
    fn item_sets_are_found_merged_and_denied_before_they_are_indexed() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        let (own, publisher, stranger) = (
            SigningKey::from_bytes(&[1; 32]),
            SigningKey::from_bytes(&[2; 32]),
            SigningKey::from_bytes(&[3; 32]),
        );
        let stories = data.join(OWN_DIR).join("local/stories");
        signed(
            &stories,
            "local/stories",
            &own,
            json!({
                "title": "Stories",
                "fields": {"points": {"type": "number", "roles": ["show"]}},
                "show": {"about": "{points} points"},
                "place": {"how": "asked", "ask": ["news"]},
            }),
            &[
                json!({"id": "a", "kind": "story", "title": "Story A",
                       "url": "https://news.example/a", "weight": 5, "fields": {"points": 5}}),
                json!({"id": "b", "kind": "story", "title": "Story B",
                       "url": "https://bad.example/b", "weight": 9, "fields": {"points": 9}}),
            ],
        );
        let held = data.join(DIR);
        signed(
            &held.join("pub/notes"),
            "pub/notes",
            &publisher,
            json!({"license": "CC0-1.0", "extends": ["url"]}),
            &[
                json!({"id": "n1", "extends": "url:https://news.example/a", "names": ["Alpha story"]}),
                json!({"id": "n2", "kind": "note", "title": "A note", "url": "https://notes.example/x"}),
            ],
        );
        signed(
            &held.join("pub/answers"),
            "pub/answers",
            &publisher,
            json!({"license": "CC0-1.0", "place": {"how": "answer-only"}}),
            &[json!({"id": "x", "kind": "fact", "title": "X", "url": "https://x.example/"})],
        );
        signed(
            &held.join("pub/strangers"),
            "pub/strangers",
            &stranger,
            json!({"license": "CC0-1.0"}),
            &[json!({"id": "s", "kind": "note", "title": "S", "url": "https://s.example/"})],
        );
        // A set being replaced, beside the one in use.
        std::fs::create_dir_all(held.join("pub/notes.new")).unwrap();

        let (own_key, publisher_key) = (PublicKey::of(&own), PublicKey::of(&publisher));
        let mine = ["local/stories".to_string()];
        let found = find(data, Some((&own_key, &mine)), &[publisher_key]);
        let names: Vec<(&str, Trust)> = found
            .iter()
            .map(|file| (file.set.as_str(), file.trust))
            .collect();
        assert_eq!(
            names,
            [
                ("local/stories", Trust::Owner),
                (
                    "pub/notes",
                    Trust::Outside {
                        weight: Weight::Low,
                        added: 0
                    }
                ),
            ],
            "not answer-only sets, nor a stranger's"
        );
        assert!(
            find(data, None, &[publisher_key])
                .iter()
                .all(|file| file.set != "local/stories"),
            "no own sets before the node has a key"
        );
        assert!(
            find(data, Some((&own_key, &[])), &[publisher_key])
                .iter()
                .all(|file| file.set != "local/stories"),
            "nor the set of a plugin since taken out"
        );

        let entry = deny_entry(&DenyEntryArgs {
            category: "spam".into(),
            target: Some("site:bad.example".into()),
            hash_of: None,
            any_query: false,
            places: Vec::new(),
            id: None,
        })
        .unwrap();
        let mut denylist = Denylist::new(Switch::On);
        denylist.add(Entry::from_item(&entry).unwrap().unwrap());
        let read = pages(&found, Some(&denylist));
        assert!(read.failed.is_empty(), "{:?}", read.failed);
        assert_eq!((read.layers.len(), read.denied), (1, 1));
        let listed: Vec<(&str, &str)> = read
            .pages
            .iter()
            .map(|page| (page.set.as_str(), page.title.as_str()))
            .collect();
        assert_eq!(
            listed,
            [("local/stories", "Story A"), ("pub/notes", "A note")]
        );
        let story = &read.pages[0];
        assert_eq!(story.description.as_deref(), Some("5 points"));
        assert_eq!(story.names, ["Alpha story"], "an outside layer's name");
        assert!(story.is_asked());
        assert!(!read.pages[1].is_asked());

        // The layers are laid on today's page sets too.
        let mut read = read;
        let mut layers = PageLayers::of(&mut read).unwrap();
        let wiki = layers.page_set("wikipedia-en", 0);
        let page = |url: &str| -> Page {
            serde_json::from_value(json!({
                "set": "wikipedia-en", "url": url, "title": "A page", "views": 10,
            }))
            .unwrap()
        };
        let named = layers.lay(page("https://news.example/a"), wiki);
        assert_eq!(named.names, ["Alpha story"]);
        let other = page("https://news.example/other");
        assert_eq!(layers.lay(other.clone(), wiki), other);
        assert_eq!(layers.laid(), 1);

        // A set that fails its check is left out; the others are read.
        let part = held.join("pub/notes/items-001.jsonl.zst");
        let mut bytes = std::fs::read(&part).unwrap();
        bytes.push(0);
        std::fs::write(&part, bytes).unwrap();
        let read = pages(&found, None);
        assert_eq!(read.failed.len(), 1);
        assert_eq!(read.failed[0].0, "pub/notes");
        assert_eq!(read.pages.len(), 2, "both stories, with no denylist");
        assert!(read.pages[0].names.is_empty());
    }

    #[test]
    fn a_held_layer_names_a_page_of_the_page_sets() {
        use plumb_core::article::{article_url, write_article, Article, ARTICLES_HEADER};

        use crate::pages::{open_or_build, open_or_build_with, PageSets, SetInfo, Wanted};

        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        let file = SetInfo::find("wikipedia-en").unwrap().file(data);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let mut text = ARTICLES_HEADER.as_bytes().to_vec();
        for (title, views) in [("Marie Curie", 900), ("Pierre Curie", 300)] {
            let article = Article {
                title: title.to_string(),
                views,
                ..Article::default()
            };
            write_article(&mut text, &article).unwrap();
        }
        std::fs::write(file, text).unwrap();
        let publisher = SigningKey::from_bytes(&[2; 32]);
        signed(
            &data.join(DIR).join("pub/nicknames"),
            "pub/nicknames",
            &publisher,
            json!({"license": "CC0-1.0", "extends": ["url"]}),
            &[json!({"id": "n", "names": ["Radium pioneer"],
                     "extends": format!("url:{}", article_url("en", "Marie Curie"))})],
        );
        let found = |searcher: &plumb_index::pages::PageSearcher| -> Vec<String> {
            let hits = searcher.search("radium pioneer", 5).unwrap();
            hits.into_iter().map(|hit| hit.page.title).collect()
        };

        let wanted = Wanted::new(data, &PageSets::default(), 0);
        let (key, searcher) = open_or_build(data, &wanted).unwrap().unwrap();
        assert!(found(&searcher).is_empty());
        let wanted = wanted.with_items(find(data, None, &[PublicKey::of(&publisher)]));
        assert_eq!(wanted.items.len(), 1);
        let (layered, searcher) = open_or_build_with(data, &wanted, None, None)
            .unwrap()
            .unwrap();
        assert_ne!(layered, key);
        assert_eq!(searcher.num_pages(), 2, "a layer adds no page");
        assert_eq!(found(&searcher), ["Marie Curie"]);
    }
}
