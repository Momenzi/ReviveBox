//! Error types returned by recipe parsing, validation, schema export, and database loading.

use std::fmt;
use std::path::PathBuf;

use thiserror::Error;

use crate::validation::ValidationReport;

/// Errors produced while loading, validating, or exporting game compatibility recipes.
#[derive(Debug, Error)]
pub enum RecipeError {
    /// Filesystem I/O failure while reading or writing a recipe artifact.
    #[error("I/O error accessing `{path}`: {source}")]
    Io {
        /// Path associated with the failing I/O operation.
        path: PathBuf,
        /// Underlying operating system error.
        #[source]
        source: std::io::Error,
    },

    /// JSON syntax or structural deserialization failure for a recipe file.
    #[error("failed to parse recipe JSON at `{path}`: {source}")]
    ParseJson {
        /// Path to the malformed JSON recipe file.
        path: PathBuf,
        /// Deserialization error emitted by `serde_json`.
        #[source]
        source: serde_json::Error,
    },

    /// JSON serialization failure (for example during schema generation).
    #[error("failed to serialize JSON output: {source}")]
    SerializeJson {
        /// Serialization error emitted by `serde_json`.
        #[source]
        source: serde_json::Error,
    },

    /// Semantic or security validation failure for a parsed recipe.
    #[error("{}", format_validation_failure(game_id, path.as_deref(), report))]
    ValidationFailed {
        /// Identifier of the recipe that failed validation.
        game_id: String,
        /// Source file path when loaded from disk.
        path: Option<PathBuf>,
        /// Full validation report containing errors and warnings.
        report: ValidationReport,
    },

    /// Attempted to insert two recipes sharing the same `game_id`.
    #[error(
        "duplicate game_id `{game_id}`: already loaded `{existing_title}`, cannot also load `{duplicate_title}`"
    )]
    DuplicateGameId {
        /// Colliding recipe identifier.
        game_id: String,
        /// Title of the first loaded recipe.
        existing_title: String,
        /// Title of the duplicate recipe.
        duplicate_title: String,
    },

    /// Attempted to register identical full cryptographic executable hashes across releases.
    #[error(
        "duplicate {algorithm} executable fingerprint `{hash}` claimed by both `{first_game_id}` and `{second_game_id}`"
    )]
    DuplicateFingerprintHash {
        /// Hash algorithm name (`SHA-256` or `MD5`).
        algorithm: &'static str,
        /// Duplicate lowercase hex digest.
        hash: String,
        /// First game/release claiming this hash.
        first_game_id: String,
        /// Second game/release claiming this hash.
        second_game_id: String,
    },

    /// Requested recipe directory does not exist or is not a directory.
    #[error("recipe directory does not exist or is not a directory: `{path}`")]
    DirectoryNotFound {
        /// Missing or invalid directory path.
        path: PathBuf,
    },
}

fn format_validation_failure(
    game_id: &str,
    path: Option<&std::path::Path>,
    report: &ValidationReport,
) -> String {
    let mut out = match path {
        Some(p) => format!("recipe `{game_id}` (`{}`) failed validation:\n", p.display()),
        None => format!("recipe `{game_id}` failed validation:\n"),
    };
    let rendered = report.to_string();
    out.push_str(rendered.trim_end());
    out
}

impl fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for err in &self.errors {
            writeln!(f, "  [ERROR] {}: {} ({:?})", err.field, err.message, err.code)?;
        }
        for warn in &self.warnings {
            writeln!(f, "  [WARN]  {}: {} ({:?})", warn.field, warn.message, warn.code)?;
        }
        Ok(())
    }
}
