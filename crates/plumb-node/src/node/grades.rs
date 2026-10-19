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
//! [`NodeSettings::site_grades_percent`]: super::NodeSettings::site_grades_percent

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use plumb_core::locks::{Locked, RwLocked};
use plumb_format::Ref;
use plumb_index::grades::{Grade, SiteGrades};
use tracing::{info, warn};

use super::Inner;

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
                *self.grades.write_locked() = Some(Arc::new(grades));
            }
            Err(err) => warn!("{SITE_GRADES}: {err:#}"),
        }
    }

    /// The site grades searches use and the share of their penalty they
    /// take, while the node holds grades and its settings take some.
    pub(super) fn grades_in_force(&self) -> Option<(Arc<SiteGrades>, u8)> {
        let percent = self.settings.locked().site_grades_percent;
        let grades = self.grades.read_locked().clone()?;
        (percent > 0 && !grades.is_empty()).then_some((grades, percent))
    }
}
