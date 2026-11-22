//! Error types returned by dry-run planning, patch execution, downloading, and game launching.

use std::path::PathBuf;

use thiserror::Error;

/// Errors emitted by [`crate::PatchEngine`], [`crate::plan_fixes`], and [`crate::launch_game`].
#[derive(Debug, Error)]
pub enum PatcherError {
    /// Filesystem I/O failure.
    #[error("I/O error accessing `{path}`: {source}")]
    Io {
        /// Path associated with the failing I/O operation.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// Recipe validation or loading error.
    #[error(transparent)]
    Recipe(#[from] ogsc_recipes::RecipeError),

    /// Scanner or backup/rollback error.
    #[error(transparent)]
    Scanner(#[from] ogsc_scanner::ScannerError),

    /// Wrapper toolkit resolution or deployment error.
    #[error(transparent)]
    Wrapper(#[from] ogsc_wrappers::WrapperError),

    /// Caller requested a `fix_id` not defined by the recipe or inapplicable to the matched release.
    #[error("unknown or inapplicable fix `{fix_id}` for release `{release_id}` in `{game_id}`")]
    UnknownFixId {
        /// Recipe game identifier.
        game_id: String,
        /// Matched release identifier.
        release_id: String,
        /// Unrecognized or inapplicable fix ID.
        fix_id: String,
    },

    /// Download failure over HTTPS (or insecure URL rejection).
    #[error("failed to download `{url}`: {reason}")]
    DownloadFailed {
        /// Target URL that failed.
        url: String,
        /// Error explanation.
        reason: String,
    },

    /// SHA-256 verification failure on a downloaded file, extracted DLL, or binary patch target.
    #[error("SHA-256 mismatch for `{subject}`: expected `{expected}`, got `{actual}`")]
    HashMismatch {
        /// Description or path of the verified file.
        subject: String,
        /// Expected lowercase hex SHA-256 digest.
        expected: String,
        /// Observed lowercase hex SHA-256 digest.
        actual: String,
    },

    /// Archive parsing or extraction failure.
    #[error("archive error for `{path}`: {reason}")]
    ArchiveError {
        /// Archive path on disk.
        path: PathBuf,
        /// Underlying decompression/format failure.
        reason: String,
    },

    /// `ExtractDll::CachedArchive` step had no cached archive from an earlier download step.
    #[error("missing cached archive from download step `{download_step_id}`")]
    MissingCachedArchive {
        /// Referenced preceding `DownloadVerifiedZip` step ID.
        download_step_id: String,
    },

    /// Requested archive member was not found inside the downloaded archive.
    #[error("archive `{archive}` does not contain member `{member}`")]
    ArchiveMemberNotFound {
        /// Archive path on disk.
        archive: PathBuf,
        /// Missing member relative path.
        member: String,
    },

    /// Binary `bsdiff` patch application failure.
    #[error("failed to apply bsdiff patch to `{target_file}`: {reason}")]
    BsdiffError {
        /// Target file being patched.
        target_file: String,
        /// Underlying error reason.
        reason: String,
    },

    /// Character encoding/decoding failure while processing an INI configuration file.
    #[error("INI encoding error for `{target_file}`: {reason}")]
    IniEncodingError {
        /// INI file path.
        target_file: String,
        /// Encoding failure details.
        reason: String,
    },

    /// `WriteIniConfig` target file is missing and `create_if_missing == false`.
    #[error("target INI file `{path}` does not exist and create_if_missing is false")]
    IniTargetMissing {
        /// Missing INI file path.
        path: PathBuf,
    },

    /// Windows Registry read/write failure.
    #[error("registry error on `{key_path}\\{value_name}`: {reason}")]
    RegistryError {
        /// Registry key path.
        key_path: String,
        /// Registry value name.
        value_name: String,
        /// Underlying error explanation.
        reason: String,
    },

    /// Unsafe relative path or Zip-Slip traversal detected.
    #[error("unsafe relative path `{path}`: {reason}")]
    UnsafePath {
        /// Rejected path string.
        path: String,
        /// Reason the path was rejected.
        reason: &'static str,
    },

    /// Game or emulator launch failure.
    #[error("failed to launch `{program}`: {reason}")]
    LaunchFailed {
        /// Executable or emulator path that failed to spawn or configure.
        program: PathBuf,
        /// Failure explanation.
        reason: String,
    },
}
