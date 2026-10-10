//! Identity embedded at compile time, never inferred from the runtime checkout.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildInfo {
    pub version: String,
    /// Full Git SHA, or `unknown` for a source archive without an override.
    pub revision: String,
    /// None means cleanliness could not be established.
    pub dirty: Option<bool>,
    pub source: String,
    /// Hash of tracked and non-ignored source files at build time, when Git is available.
    pub source_sha256: Option<String>,
}

pub fn current() -> BuildInfo {
    BuildInfo {
        version: env!("CARGO_PKG_VERSION").into(),
        revision: option_env!("PLUMB_BUILD_REVISION")
            .unwrap_or("unknown")
            .into(),
        dirty: match option_env!("PLUMB_BUILD_DIRTY").unwrap_or("unknown") {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        source_sha256: option_env!("PLUMB_BUILD_SOURCE_SHA256")
            .filter(|s| *s != "unknown")
            .map(str::to_owned),
        source: option_env!("PLUMB_BUILD_SOURCE")
            .unwrap_or("unknown")
            .into(),
    }
}
