//! Transactional patch execution engine, dry-run planner, verified HTTPS downloader,
//! Zip/7z extractor, bsdiff patcher, INI editor, Windows `AppCompatFlags` registry writer,
//! and CPU-affinity game launcher for **Old Games – Second Chance**.

pub mod archive;
pub mod bsdiff_patch;
pub mod download;
pub mod engine;
pub mod error;
pub mod events;
pub mod ini;
pub mod launcher;
pub mod plan;
pub mod registry;
pub mod win32;

pub use archive::{extract_archive, extract_single_archive_member, ExtractArchiveOptions};
pub use bsdiff_patch::apply_verified_bsdiff;
pub use download::{fetch_verified_file, Downloader, HttpDownloader};
pub use engine::{ApplyReport, PatchEngine};
pub use error::PatcherError;
pub use events::{EventSink, NoopSink, PatchEvent, VecEventSink};
pub use ini::{apply_ini_updates, format_ini_value, DynamicContext};
pub use launcher::{
    launch_game, launch_game_captured, resolve_launch_command, LaunchOptions, LaunchedProcess,
    ResolvedLaunchCommand,
};
pub use plan::{
    plan_fixes, PatchContext, PatchPlan, PlannedDownload, PlannedFileChange, PlannedFileOpKind,
    PlannedFixPreview, PlannedRegistryChange, PlannedWrapperSummary,
};
#[cfg(windows)]
pub use registry::WindowsRegistryBackend;
pub use registry::{
    apply_compat_flags, merge_compat_flags, revert_registry_changes, MemoryRegistryBackend,
    RegistryBackend, APP_COMPAT_LAYERS_KEY,
};
pub use win32::{
    apply_process_affinity_and_priority, current_windows_build, query_primary_display,
};
