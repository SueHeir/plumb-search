//! Explicit publication constraints shared by paper search entry points.
//! Bounds are inclusive. A year permits year-only records; a day requires
//! an actual publication day. Preprint revisions never supply publication dates.

use crate::papers::{valid_date, PaperMetadata};
use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaperBound {
    pub year: u64,
    pub day: Option<u64>,
}

impl PaperBound {
    pub fn parse(value: &str) -> Result<Self> {
        if valid_date(value) {
            return Ok(Self {
                year: value[..4].parse()?,
                day: day_number(value),
            });
        }
        if value.len() == 4 && value.bytes().all(|b| b.is_ascii_digit()) {
            let year = value.parse()?;
            if year > 0 {
                return Ok(Self { year, day: None });
            }
        }
        bail!("paper date must be YYYY or a valid YYYY-MM-DD; got {value:?}")
    }

    fn first_day(self) -> u64 {
        self.day.unwrap_or(self.year * 10000 + 101)
    }
    fn last_day(self) -> u64 {
        self.day.unwrap_or(self.year * 10000 + 1231)
    }
}

pub fn day_number(value: &str) -> Option<u64> {
    valid_date(value)
        .then(|| value.replace('-', "").parse().ok())
        .flatten()
}

pub fn publication_year(paper: &PaperMetadata) -> Option<u64> {
    paper
        .publication_date
        .as_deref()
        .and_then(day_number)
        .map(|day| day / 10000)
        .or_else(|| {
            paper
                .publication_year
                .filter(|y| (1..=9999).contains(y))
                .map(|y| y as u64)
        })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaperQuery {
    /// Original lexical text, with only explicit unquoted date/order terms removed.
    pub query: String,
    pub after: Option<PaperBound>,
    pub before: Option<PaperBound>,
    pub newest: bool,
    pub constrained: bool,
}

impl PaperQuery {
    pub fn parse(query: &str) -> Result<Self> {
        let mut parsed = Self::default();
        let mut terms = Vec::new();
        // Keep quoted titles opaque, including curly quotes and exclusions.
        let mut quote = false;
        let mut start = 0;
        let mut tokens = Vec::new();
        for (at, c) in query.char_indices() {
            if matches!(c, '"' | '“' | '”') {
                quote = !quote;
            }
            if c.is_whitespace() && !quote {
                if start < at {
                    tokens.push(&query[start..at]);
                }
                start = at + c.len_utf8();
            }
        }
        if start < query.len() {
            tokens.push(&query[start..]);
        }
        for token in tokens {
            if !token.contains(['"', '“', '”']) {
                if let Some((key, value)) = token.split_once(':') {
                    match key.to_ascii_lowercase().as_str() {
                        "after" => {
                            let bound = PaperBound::parse(value)?;
                            if parsed.after.is_some_and(|old| old != bound) {
                                bail!("conflicting paper after bounds; supply one bound");
                            }
                            parsed.after = Some(bound);
                            parsed.constrained = true;
                            continue;
                        }
                        "before" => {
                            let bound = PaperBound::parse(value)?;
                            if parsed.before.is_some_and(|old| old != bound) {
                                bail!("conflicting paper before bounds; supply one bound");
                            }
                            parsed.before = Some(bound);
                            parsed.constrained = true;
                            continue;
                        }
                        "sort" => {
                            match value {
                                "newest" => parsed.newest = true,
                                "relevance" => parsed.newest = false,
                                _ => bail!("paper sort must be newest or relevance"),
                            }
                            parsed.constrained = true;
                            continue;
                        }
                        _ => {}
                    }
                }
            }
            terms.push(token);
        }
        // Only a literal, unquoted publication phrase makes a bare year hard.
        // Ordinary title years (BERT 2018) and software versions stay lexical.
        if terms.len() >= 4 {
            for i in 0..=terms.len() - 4 {
                if matches!(
                    terms[i].to_ascii_lowercase().as_str(),
                    "papers" | "research"
                ) && terms[i + 1].eq_ignore_ascii_case("published")
                    && terms[i + 2].eq_ignore_ascii_case("in")
                    && terms[i + 3].len() == 4
                {
                    if let Ok(bound) = PaperBound::parse(terms[i + 3]) {
                        if parsed.after.is_some() || parsed.before.is_some() {
                            bail!("use either published in YYYY or explicit paper date bounds");
                        }
                        parsed.after = Some(bound);
                        parsed.before = Some(bound);
                        parsed.constrained = true;
                        terms.drain(i..i + 4);
                        break;
                    }
                }
            }
        }
        if parsed
            .after
            .zip(parsed.before)
            .is_some_and(|(a, b)| a.first_day() > b.last_day())
        {
            bail!("paper after bound is later than before bound");
        }
        parsed.query = terms.join(" ");
        Ok(parsed)
    }

    /// Add typed CLI/API/MCP fields using the same validated query grammar.
    pub fn with_options(
        query: &str,
        after: Option<&str>,
        before: Option<&str>,
        order: Option<&str>,
    ) -> Result<Self> {
        let mut text = query.to_string();
        for (key, value) in [("after", after), ("before", before)] {
            if let Some(value) = value {
                PaperBound::parse(value)?;
                text.push_str(&format!(" {key}:{value}"));
            }
        }
        if let Some(order) = order {
            if !matches!(order, "newest" | "relevance") {
                bail!("paper order must be newest or relevance");
            }
            text.push_str(&format!(" sort:{order}"));
        }
        Self::parse(&text)
    }

    pub fn allows(&self, paper: Option<&PaperMetadata>) -> bool {
        if self.after.is_none() && self.before.is_none() {
            return true;
        }
        let Some(paper) = paper else {
            return false;
        };
        let day = paper.publication_date.as_deref().and_then(day_number);
        let year = publication_year(paper);
        self.after.is_none_or(|b| match b.day {
            Some(d) => day.is_some_and(|v| v >= d),
            None => year.is_some_and(|y| y >= b.year),
        }) && self.before.is_none_or(|b| match b.day {
            Some(d) => day.is_some_and(|v| v <= d),
            None => year.is_some_and(|y| y <= b.year),
        })
    }

    pub fn text(&self) -> String {
        let mut text = self.query.clone();
        for (key, bound) in [("after", self.after), ("before", self.before)] {
            if let Some(bound) = bound {
                let value = bound.day.map_or_else(
                    || format!("{:04}", bound.year),
                    |d| format!("{:04}-{:02}-{:02}", d / 10000, d / 100 % 100, d % 100),
                );
                text.push_str(&format!(" {key}:{value}"));
            }
        }
        if self.newest {
            text.push_str(" sort:newest");
        }
        text.trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paper_dates_are_explicit_and_validated() {
        let query =
            PaperQuery::parse("transformers after:2025 before:2026-02-28 sort:newest").unwrap();
        assert_eq!(query.query, "transformers");
        assert!(query.newest);
        assert!(PaperQuery::parse("x after:2026-02-29").is_err());
        assert!(PaperQuery::parse("x after:2026 before:2025").is_err());
        assert!(PaperQuery::parse("x sort:latest").is_err());
        assert!(PaperQuery::parse("after:2025-01-01 after:2025").is_err());
        assert!(PaperQuery::with_options("x", Some("2025 x"), None, None).is_err());
        assert_eq!(PaperQuery::parse(&query.text()).unwrap(), query);
    }
    #[test]
    fn paper_years_in_titles_are_not_constraints() {
        for text in [
            "BERT 2018",
            "Python 3.12",
            "\"papers published in 2025\"",
            "\"after:2025\"",
        ] {
            let query = PaperQuery::parse(text).unwrap();
            assert!(!query.constrained);
            assert_eq!(query.query, text);
        }
        let query = PaperQuery::parse("transformer papers published in 2025").unwrap();
        assert_eq!(query.query, "transformer");
        assert_eq!(query.after, query.before);
    }
    #[test]
    fn paper_unknown_and_partial_dates_obey_precision() {
        let paper = PaperMetadata {
            publication_year: Some(2025),
            preprint_version_date: Some("2026-06-01".into()),
            ..Default::default()
        };
        assert!(PaperQuery::parse("after:2025 before:2025")
            .unwrap()
            .allows(Some(&paper)));
        assert!(!PaperQuery::parse("after:2025-01-01")
            .unwrap()
            .allows(Some(&paper)));
        assert!(!PaperQuery::parse("after:2025").unwrap().allows(None));
        assert!(!PaperQuery::parse("after:2026")
            .unwrap()
            .allows(Some(&paper)));
    }
}
