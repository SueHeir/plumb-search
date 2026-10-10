//! Bounded scholarly metadata carried by the optional articles extension.
//! Publication and preprint dates describe different versions; a fetch date
//! never stands in for either. Counts retain their provider meaning.

use serde::{Deserialize, Serialize};

pub const MAX_PAPER_METADATA_BYTES: usize = 16_384;
pub const MAX_PAPER_AUTHORS: usize = 64;
pub const MAX_PAPER_CORRECTIONS: usize = 4;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaperCountKind {
    #[default]
    Unknown,
    Citations,
    MethodUses,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaperMetadata {
    pub doi: Option<String>,
    pub arxiv_id: Option<String>,
    pub openalex_id: Option<String>,
    pub authors: Vec<String>,
    /// Date of this record's publication, as supplied by its provider.
    pub publication_date: Option<String>,
    /// Provider value retained even when it fails strict day validation.
    pub raw_publication_date: Option<String>,
    /// Retained even when a provider supplies only a year.
    pub publication_year: Option<i32>,
    /// First arXiv submission; does not replace a later journal date.
    pub preprint_date: Option<String>,
    pub version_date: Option<String>,
    /// Latest arXiv revision; independent of a journal record's version.
    pub preprint_version_date: Option<String>,
    pub venue: Option<String>,
    pub source: String,
    pub count_kind: PaperCountKind,
    pub count: u64,
    pub alternate_urls: Vec<String>,
    pub corrections: Vec<PaperCorrection>,
}

/// Conflicting provider values are audit evidence, never search aliases.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaperCorrection {
    pub reason: String,
    pub source_url: String,
    pub previous_item: Option<String>,
    pub previous_title: String,
    pub previous_description: Option<String>,
    pub previous_authors: Vec<String>,
    pub previous_publication_date: Option<String>,
    pub previous_raw_publication_date: Option<String>,
    pub previous_publication_year: Option<i32>,
}

/// Strict ISO day validation, including leap years. Unknown/partial dates
/// cannot satisfy a hard day constraint.
pub fn valid_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    if bytes
        .iter()
        .enumerate()
        .any(|(i, b)| i != 4 && i != 7 && !b.is_ascii_digit())
    {
        return false;
    }
    let year: u32 = date[..4].parse().unwrap_or(0);
    let month: u32 = date[5..7].parse().unwrap_or(0);
    let day: u32 = date[8..].parse().unwrap_or(0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(400) || year.is_multiple_of(4) && !year.is_multiple_of(100) => 29,
        2 => 28,
        _ => 0,
    };
    year > 0 && day > 0 && day <= days
}

impl PaperMetadata {
    pub fn count_label(&self) -> &'static str {
        match self.count_kind {
            PaperCountKind::Citations => "citations",
            PaperCountKind::MethodUses => "method uses",
            PaperCountKind::Unknown => "popularity (count type unknown)",
        }
    }

    pub fn date_labels(&self) -> Vec<(&'static str, String)> {
        let mut dates = Vec::new();
        let published = self
            .publication_date
            .as_deref()
            .filter(|d| valid_date(d))
            .map(str::to_string)
            .or_else(|| {
                self.publication_year
                    .filter(|y| (1..=9999).contains(y))
                    .map(|y| y.to_string())
            });
        dates.push((
            "Published",
            published.unwrap_or_else(|| "unknown".to_string()),
        ));
        for (label, date) in [
            ("Preprint", &self.preprint_date),
            ("Version", &self.version_date),
            ("Preprint revised", &self.preprint_version_date),
        ] {
            if let Some(date) = date.as_deref().filter(|d| valid_date(d)) {
                dates.push((label, date.to_string()));
            }
        }
        dates
    }

    /// Percent encoding keeps JSON delimiters out of the profiles line.
    /// Old readers skip the unknown `paper` key and keep the six columns.
    pub fn write(&self) -> Option<String> {
        if !self.bounded() {
            return None;
        }
        let json = serde_json::to_vec(self).ok()?;
        if json.len() > MAX_PAPER_METADATA_BYTES {
            return None;
        }
        let text = String::from_utf8(json).ok()?;
        Some(
            text.chars()
                .map(|c| match c {
                    '%' | '|' | '\t' | '\n' | '\r' => format!("%{:02X}", c as u32),
                    _ => c.to_string(),
                })
                .collect(),
        )
    }

    pub fn parse(encoded: &str) -> Option<Self> {
        if encoded.len() > MAX_PAPER_METADATA_BYTES * 3 {
            return None;
        }
        let mut bytes = Vec::with_capacity(encoded.len().min(MAX_PAPER_METADATA_BYTES));
        let mut at = 0;
        while at < encoded.len() {
            if encoded.as_bytes()[at] == b'%' {
                let hex = encoded.get(at + 1..at + 3)?;
                bytes.push(u8::from_str_radix(hex, 16).ok()?);
                at += 3;
            } else {
                bytes.push(encoded.as_bytes()[at]);
                at += 1;
            }
            if bytes.len() > MAX_PAPER_METADATA_BYTES {
                return None;
            }
        }
        let metadata: Self = serde_json::from_slice(&bytes).ok()?;
        metadata.bounded().then_some(metadata)
    }

    fn bounded(&self) -> bool {
        self.authors.len() <= MAX_PAPER_AUTHORS
            && self.alternate_urls.len() <= 8
            && self.corrections.len() <= MAX_PAPER_CORRECTIONS
            && self
                .corrections
                .iter()
                .all(|c| c.previous_authors.len() <= MAX_PAPER_AUTHORS)
            && [
                &self.publication_date,
                &self.preprint_date,
                &self.version_date,
                &self.preprint_version_date,
            ]
            .into_iter()
            .flatten()
            .all(|date| valid_date(date))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_extensions_are_bounded() {
        assert!(valid_date("2024-02-29"));
        assert!(!valid_date("2025-02-29"));
        assert!(!valid_date("2025-13-01"));
        assert!(!valid_date("2025"));
        assert!(!valid_date("é025-01-01"));
        let metadata = PaperMetadata {
            authors: vec!["A | B\tC".into()],
            publication_date: Some("2017-06-12".into()),
            ..PaperMetadata::default()
        };
        let encoded = metadata.write().unwrap();
        assert!(!encoded.contains(['|', '\t', '\n']));
        assert_eq!(PaperMetadata::parse(&encoded), Some(metadata));
        assert_eq!(PaperMetadata::parse("%XX"), None);
        assert_eq!(
            PaperMetadata::parse(&"%00".repeat(MAX_PAPER_METADATA_BYTES + 1)),
            None
        );
        let oversized = PaperMetadata {
            authors: vec!["A".into(); 65],
            ..PaperMetadata::default()
        };
        assert_eq!(oversized.write(), None);
    }
}
