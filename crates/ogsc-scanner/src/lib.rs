//! Parallel game directory scanning, cryptographic executable fingerprinting, transactional
//! pre-patch backups, and local `library.json` persistence for **Old Games – Second Chance**.

#![forbid(unsafe_code)]

pub mod backup;
pub mod error;
pub mod hasher;
pub mod library;
pub mod scanner;

pub use backup::{
    BackupJournal, BackupManager, FixTransaction, OriginalFileSnapshot, RegistryChangeRecord,
    RollbackSummary, JOURNAL_SCHEMA_VERSION,
};
pub use error::ScannerError;
pub use hasher::{
    compute_fingerprint, compute_hashes, compute_partial_sha256, compute_sha256, sha256_bytes,
    ComputedHashes, GeneratedFingerprint, DEFAULT_PARTIAL_HASH_BYTES,
};
pub use library::{
    compute_entry_id, GameLibrary, GameStatus, LibraryEntry, LibraryStore, LIBRARY_SCHEMA_VERSION,
};
pub use scanner::{GameScanner, MatchConfidence, ScanMatch, ScanOptions, ScanReport};
