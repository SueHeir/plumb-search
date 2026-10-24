//! The project's site grades (`plumb/site-grades`): a layer that labels
//! sites `machine-written` with a `grade` field, `spam` or `low_value`,
//! written by a language model on the project's own computer and signed
//! there (`plumb-tools set site-grades`). Every node takes the same set
//! ([`super::signed_sets`]), so all of them grade a site alike without
//! running a model. Searches push graded sites down by the share the
//! node's settings give ([`NodeSettings::site_grades_percent`],
//! [`plumb_index::grades::demote`]).
//!
//! Only graded sites are in the set; a site with none is fine. The node
//! holds them as [`SiteGrades`], 16 bytes a site: a few MB for the few
//! hundred thousand sites a grading of every site finds. Reading the set
//! goes an item at a time.
//!
//! The node's owner can grade sites too, through an AI app on the node's
//! computer (the MCP tool `grade_site`). Those grades are kept in
//! `DIR/site-grades.json`, stay on the node, and take the place of the
//! set's for the sites they name: `ok` lifts the set's grade. Searches use
//! both alike.
//!
//! [`NodeSettings::site_grades_percent`]: super::NodeSettings::site_grades_percent

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Result};
use plumb_core::locks::{Locked, RwLocked};
use plumb_format::Ref;
use plumb_index::grades::{Grade, SiteGrades};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::Inner;

/// Where the owner's own grades are kept.
const OWN_GRADES_FILE: &str = "site-grades.json";
/// Most sites the owner's apps grade.
const MAX_OWN_GRADES: usize = 10_000;
/// Longest reason kept with a grade, in characters, as MCP tools take
/// text.
const MAX_WHY_CHARS: usize = 200;

/// A grade the node's owner gave a site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnGrade {
    /// `spam`, `low_value`, or `ok` for a site that is fine whatever the
    /// project's set says.
    pub grade: String,
    pub why: String,
    /// When, in Unix seconds.
    pub at: u64,
}

/// What `grade_site` did.
#[derive(Debug, Clone, Serialize)]
pub struct Graded {
    pub site: String,
    pub grade: String,
    /// The project's grade of the site, which the owner's takes the place
    /// of.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set_grade: Option<&'static str>,
    /// How much of its penalty a graded site loses on this node; 0 while
    /// the node's Site grades setting turns grades off.
    pub percent: u8,
    /// Sites the owner's apps graded in all.
    pub own_grades: usize,
}

/// The project's site grades as a signed set.
pub const SITE_GRADES: &str = "plumb/site-grades";

/// The field that holds a site's grade.
pub const GRADE_FIELD: &str = "grade";

/// The grades the set in `dir` gives, without checking who signed it.
/// Items that are not a grade of a site, or name a grade this version
/// does not know, are skipped.
pub fn read(dir: &Path) -> Result<SiteGrades> {
    let mut graded = Vec::new();
    crate::sets::for_each_item(dir, |item| {
        let grade = item
            .fields
            .get(GRADE_FIELD)
            .and_then(|value| value.as_str())
            .and_then(Grade::parse);
        if let (Some(Ref::Site(domain)), Some(grade), false) =
            (item.extends, grade, item.deleted.is_some())
        {
            graded.push((domain, grade));
        }
        Ok(())
    })?;
    Ok(SiteGrades::new(graded))
}

impl Inner {
    /// Makes the site grades set held, when one of the node's publishers
    /// signed it, the grades searches use.
    pub(super) fn grades_from_set(&self) {
        let held = super::signed_sets::held_trusted(
            &self.paths.data,
            SITE_GRADES,
            &self.config.set_publishers,
        );
        let Some((dir, _)) = held else {
            return;
        };
        match read(&dir) {
            Ok(grades) => {
                info!("{} sites graded, from {SITE_GRADES}", grades.len());
                *self.set_grades.write_locked() = Some(Arc::new(grades));
                self.combine_grades(&self.own_grades.read_locked());
            }
            Err(err) => warn!("{SITE_GRADES}: {err:#}"),
        }
    }

    /// Reads the owner's own grades, at start. A file that can't be read
    /// is moved aside, so the next grade does not write over it.
    pub(super) fn load_own_grades(&self) {
        let path = self.paths.data.join(OWN_GRADES_FILE);
        let own = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                let aside = path.with_extension("json.bad");
                warn!("{}: {err}; moved to {}", path.display(), aside.display());
                if let Err(err) =
                    plumb_core::storage::replace_file(&path, &aside, self.storage.as_deref())
                {
                    warn!("{}: {err}", aside.display());
                }
                BTreeMap::new()
            }),
            Err(_) => BTreeMap::new(),
        };
        let mut kept = self.own_grades.write_locked();
        *kept = own;
        self.combine_grades(&kept);
    }

    /// Makes the set's grades, with the owner's `own` in their place, the
    /// ones searches use. Callers hold the lock on the owner's grades, so
    /// two changes never land out of order.
    fn combine_grades(&self, own: &BTreeMap<String, OwnGrade>) {
        let own: Vec<(String, Option<Grade>)> = own
            .iter()
            .map(|(site, own)| (site.clone(), Grade::parse(&own.grade)))
            .collect();
        let set = self.set_grades.read_locked().clone();
        let grades = match (set, own.is_empty()) {
            (set, true) => set,
            (set, false) => Some(Arc::new(set.unwrap_or_default().overridden(&own))),
        };
        *self.grades.write_locked() = grades;
    }

    /// `grade_site`: the owner's grade of the site `site` (`spam`,
    /// `low_value` or `ok`), kept and used at once.
    pub(super) fn grade_site(&self, site: &str, grade: &str, why: &str) -> Result<Graded> {
        if grade != "ok" && Grade::parse(grade).is_none() {
            bail!("a grade is spam, low_value or ok, not {grade:?}");
        }
        let own_grades = {
            let mut own = self.own_grades.write_locked();
            if own.len() >= MAX_OWN_GRADES && !own.contains_key(site) {
                bail!("this node keeps at most {MAX_OWN_GRADES} grades of its own");
            }
            let graded = OwnGrade {
                grade: grade.to_string(),
                why: plumb_core::truncate_chars(why.trim(), MAX_WHY_CHARS),
                at: plumb_core::now_unix(),
            };
            // Saved under the lock, so an older copy never lands last; only
            // `grade_site` and `node_tuning` take it, off async threads. A
            // grade that can't be saved isn't used either.
            let mut next = own.clone();
            next.insert(site.to_string(), graded);
            let path = self.paths.data.join(OWN_GRADES_FILE);
            let bytes = serde_json::to_vec_pretty(&next)?;
            plumb_core::storage::write_atomic(
                &path,
                &path.with_extension("json.tmp"),
                &bytes,
                self.storage.as_ref(),
                None,
            )?;
            *own = next;
            self.combine_grades(&own);
            own.len()
        };
        self.journal.info(format!(
            "An AI app graded {site} {grade}: {}",
            plumb_core::truncate_chars(why.trim(), 120)
        ));
        let set_grade = self
            .set_grades
            .read_locked()
            .as_ref()
            .and_then(|set| set.get(site));
        Ok(Graded {
            site: site.to_string(),
            grade: grade.to_string(),
            set_grade: set_grade.map(Grade::name),
            percent: self.settings.locked().site_grades_percent,
            own_grades,
        })
    }

    /// The owner's own grades, the most recent first.
    pub(super) fn own_grades(&self) -> Vec<(String, OwnGrade)> {
        let mut own: Vec<(String, OwnGrade)> = self
            .own_grades
            .read_locked()
            .iter()
            .map(|(site, own)| (site.clone(), own.clone()))
            .collect();
        own.sort_by_key(|(_, own)| std::cmp::Reverse(own.at));
        own
    }

    /// Sites the project's set grades.
    pub(super) fn set_grades_len(&self) -> usize {
        self.set_grades
            .read_locked()
            .as_ref()
            .map_or(0, |set| set.len())
    }

    /// The site grades searches use (the set's and the owner's) and the
    /// share of their penalty they take, while the node holds grades and
    /// its settings take some.
    pub(super) fn grades_in_force(&self) -> Option<(Arc<SiteGrades>, u8)> {
        let percent = self.settings.locked().site_grades_percent;
        let grades = self.grades.read_locked().clone()?;
        (percent > 0 && !grades.is_empty()).then_some((grades, percent))
    }
}
