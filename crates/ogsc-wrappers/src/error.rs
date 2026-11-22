//! Error types returned by compatibility wrapper asset resolution and deployment.

use std::path::PathBuf;

use thiserror::Error;

/// Errors produced when planning or deploying compatibility wrapper toolkits.
#[derive(Debug, Error)]
pub enum WrapperError {
    /// Filesystem I/O error while reading toolkit assets or writing wrapper files.
    #[error("I/O error accessing `{path}`: {source}")]
    Io {
        /// Path associated with the failing I/O operation.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// Required toolkit root directory is not installed under the user's `runtime_wrappers` folder.
    #[error("compatibility toolkit `{toolkit}` is not installed at `{expected_dir}`")]
    ToolkitNotInstalled {
        /// Toolkit slug (for example `dgvoodoo2` or `dosbox_x`).
        toolkit: String,
        /// Expected local directory path.
        expected_dir: PathBuf,
    },

    /// A required wrapper DLL or emulator binary is missing from the local toolkit directory.
    #[error("missing `{toolkit}` toolkit asset at `{expected_path}`")]
    MissingToolkitAsset {
        /// Toolkit slug.
        toolkit: String,
        /// Missing local asset path.
        expected_path: PathBuf,
    },

    /// Rejected unsafe relative path or filename inside a wrapper deployment or toolkit lookup.
    #[error("unsafe relative path `{path}`: {reason}")]
    UnsafePath {
        /// Rejected path string.
        path: String,
        /// Explanation of why the path is unsafe.
        reason: &'static str,
    },

    /// Conflicting or invalid `dll_chain_renames` entry.
    #[error("invalid DLL chain rename `{original_dll}` -> `{chained_as}`")]
    InvalidChainRename {
        /// Original proxy DLL name.
        original_dll: String,
        /// Requested target filename.
        chained_as: String,
    },

    /// Transactional backup failure while snapshotting pre-deployment game files.
    #[error(transparent)]
    Backup(#[from] ogsc_scanner::ScannerError),
}
