//! Error types returned by game scanning, hashing, transactional backups, and library persistence.

use std::path::PathBuf;

use thiserror::Error;

/// Errors emitted by [`crate::GameScanner`], [`crate::BackupManager`], and [`crate::LibraryStore`].
#[derive(Debug, Error)]
pub enum ScannerError {
    /// Filesystem I/O failure during scanning, hashing, backup, or library writes.
    #[error("I/O error accessing `{path}`: {source}")]
    Io {
        /// Path associated with the failing I/O operation.
        path: PathBuf,
        /// Underlying OS I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Requested scan root does not exist or is not a directory.
    #[error("scan root does not exist or is not a directory: `{path}`")]
    ScanRootNotFound {
        /// Missing or non-directory scan root.
        path: PathBuf,
    },

    /// A resolved filesystem path escapes its parent game installation root.
    #[error("path `{path}` resolves outside the allowed game directory")]
    PathEscape {
        /// Offending path string.
        path: String,
    },

    /// A relative path failed safety validation (absolute, traversal, or reserved device name).
    #[error("invalid relative path `{path}`: {reason}")]
    InvalidRelativePath {
        /// Rejected path string.
        path: String,
        /// Explanation of why the path was rejected.
        reason: &'static str,
    },

    /// Backup `journal.json` could not be parsed.
    #[error("corrupted backup journal at `{path}`: {source}")]
    BackupCorrupted {
        /// Path to the malformed `journal.json`.
        path: PathBuf,
        /// Underlying JSON parsing error.
        #[source]
        source: serde_json::Error,
    },

    /// Backup `journal.json` serialization error.
    #[error("failed to serialize backup journal: {source}")]
    BackupSerialize {
        /// Underlying JSON serialization error.
        #[source]
        source: serde_json::Error,
    },

    /// Backup journal uses an unsupported schema version.
    #[error("unsupported backup journal version {version}")]
    BackupUnsupportedVersion {
        /// Version integer found on disk.
        version: u32,
    },

    /// Restoring a backed-up file failed cryptographic integrity verification.
    #[error(
        "backup integrity mismatch for `{relative_path}`: expected SHA-256 `{expected}`, got `{actual}`"
    )]
    BackupHashMismatch {
        /// Relative path inside the game folder being restored.
        relative_path: String,
        /// SHA-256 recorded in `journal.json`.
        expected: String,
        /// Actual SHA-256 read from the backup blob.
        actual: String,
    },

    /// Content-addressed backup blob referenced by `journal.json` is missing from disk.
    #[error("missing backup blob `{blob_path}`")]
    BackupFileMissing {
        /// Expected path under `<backup_dir>/blobs/`.
        blob_path: PathBuf,
    },

    /// Attempted to roll back a fix ID that is not recorded in `journal.json`.
    #[error("fix `{fix_id}` is not present in the backup journal")]
    FixNotRecorded {
        /// Missing fix identifier.
        fix_id: String,
    },

    /// `library.json` could not be parsed.
    #[error("failed to parse game library at `{path}`: {source}")]
    LibraryParse {
        /// Path to `library.json`.
        path: PathBuf,
        /// Underlying JSON parsing error.
        #[source]
        source: serde_json::Error,
    },

    /// `library.json` serialization failed.
    #[error("failed to serialize game library: {source}")]
    LibrarySerialize {
        /// Underlying JSON serialization error.
        #[source]
        source: serde_json::Error,
    },

    /// `library.json` uses an unsupported schema version.
    #[error("unsupported game library version {version}")]
    LibraryUnsupportedVersion {
        /// Version integer found on disk.
        version: u32,
    },

    /// Requested library entry ID was not found in `library.json`.
    #[error("game entry `{entry_id}` not found in library")]
    GameNotInLibrary {
        /// Missing `LibraryEntry::entry_id`.
        entry_id: String,
    },
}
