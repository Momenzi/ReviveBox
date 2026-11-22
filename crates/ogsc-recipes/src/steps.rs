//! Ordered patch step definitions, Windows AppCompat flags, INI mutations, CPU affinity, and rollback plans.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::schema::HashVerificationStatus;
use crate::wrapper::WrapperDeployment;

/// A selectable compatibility fix group presented to the user in the Patch Advisor popup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FixOption {
    /// Stable fix group identifier unique within the recipe (for example `widescreen_fix`).
    pub id: String,

    /// Human-readable title displayed next to the fix checkbox in the Patch Advisor popup.
    pub title: String,

    /// Clear explanation of what this fix resolves and which files or registry keys it touches.
    pub description: String,

    /// Whether this fix is pre-checked by default in the Patch Advisor popup.
    #[serde(default = "default_true")]
    pub recommended: bool,

    /// Release IDs this fix applies to. An empty list means it applies to all releases.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applies_to_releases: Vec<String>,

    /// Ordered self-contained steps executed if and only if the user selects this fix.
    pub steps: Vec<PatchStep>,
}

/// A single ordered action inside a [`FixOption`] pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchStep {
    /// Stable step identifier unique across the recipe (referenced by `cached_archive` within the same fix).
    pub id: String,

    /// Human-readable step name displayed in the Patch & Launch log console.
    pub name: String,

    /// Detailed explanation of what this step modifies and why it is safe.
    pub description: String,

    /// Release IDs this step applies to. An empty list means it applies to all releases.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applies_to_releases: Vec<String>,

    /// Optional Windows build range filter restricting when this step executes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_filter: Option<OsFilter>,

    /// Concrete action executed during patching.
    pub action: PatchAction,
}

/// Optional Windows OS build number filter (`min_windows_build..=max_windows_build`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OsFilter {
    /// Minimum inclusive Windows build number (for example `19041` for Win10 2004, `22000` for Win11).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_windows_build: Option<u32>,

    /// Maximum inclusive Windows build number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_windows_build: Option<u32>,
}

/// Declarative action performed by a [`PatchStep`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PatchAction {
    /// Download a verified open-source/community archive over HTTPS and extract selected files.
    DownloadVerifiedZip {
        /// Primary HTTPS download URL (must be `https://`).
        url: String,
        /// Optional fallback HTTPS mirror URLs.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mirror_urls: Vec<String>,
        /// Expected lowercase hex SHA-256 digest of the downloaded archive.
        sha256: String,
        /// Provenance/verification status of the archive digest.
        #[serde(default)]
        verification_status: HashVerificationStatus,
        /// Optional expected download size in bytes for progress bars and pre-flight checks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size_bytes: Option<u64>,
        /// Archive container format.
        archive_format: ArchiveFormat,
        /// Game-relative destination directory (`"."` extracts into the game root).
        extract_to: String,
        /// Optional glob patterns restricting which archive entries are extracted.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        include_patterns: Vec<String>,
        /// Number of leading archive path components to strip during extraction.
        #[serde(default)]
        strip_components: u8,
        /// Whether extracted files may overwrite existing files (Originals are backed up first).
        #[serde(default = "default_true")]
        overwrite_existing: bool,
        /// SPDX or descriptive license of the community patch (for example `MIT`, `Zlib`).
        license: String,
        /// Optional upstream repository or release page URL.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_repo: Option<String>,
    },

    /// Extract or copy a single DLL/ASI/FLT file from the bundled toolkit or an earlier downloaded archive.
    ExtractDll {
        /// Origin of the DLL file.
        source: DllSource,
        /// Game-relative destination file path (for example `dinput8.dll` or `scripts/mod.asi`).
        destination_path: String,
        /// Optional expected lowercase SHA-256 digest of the extracted DLL.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_sha256: Option<String>,
        /// Provenance/verification status of `expected_sha256`.
        #[serde(default)]
        verification_status: HashVerificationStatus,
        /// Whether any pre-existing file at `destination_path` must be backed up before replacing.
        #[serde(default = "default_true")]
        backup_original: bool,
    },

    /// Deploy and configure a bundled compatibility runtime wrapper.
    DeployWrapper(WrapperDeployment),

    /// Apply a verified `bsdiff` binary patch to an existing local game file (non-destructive backup first).
    ApplyBinaryBsdiffPatch {
        /// Game-relative target file to patch.
        target_file: String,
        /// Source of the `.bsdiff` patch payload.
        patch_source: BinaryPatchSource,
        /// Required lowercase SHA-256 of the unpatched local file before applying the patch.
        expected_original_sha256: String,
        /// Required lowercase SHA-256 of the resulting file after applying the patch.
        expected_patched_sha256: String,
        /// Provenance/verification status of the expected digests.
        #[serde(default)]
        verification_status: HashVerificationStatus,
        /// Whether to back up the original target file prior to patching.
        #[serde(default = "default_true")]
        backup_original: bool,
    },

    /// Apply Windows Application Compatibility Layers in the registry (`AppCompatFlags\Layers`).
    SetRegistryCompatFlags {
        /// Target registry hive (`hkcu` strongly preferred for non-destructive per-user isolation).
        #[serde(default)]
        hive: RegistryHive,
        /// Game-relative executable path whose resolved full path receives the compatibility flags.
        target_executable: String,
        /// Ordered list of Windows compatibility layer tokens to apply.
        flags: Vec<AppCompatFlag>,
        /// Whether to preserve unrelated existing user compatibility flags on that executable.
        #[serde(default = "default_true")]
        merge_with_existing: bool,
    },

    /// Create or update INI configuration sections without clobbering unrelated keys.
    WriteIniConfig {
        /// Game-relative INI file path.
        target_file: String,
        /// Create parent directories and file if missing.
        #[serde(default = "default_true")]
        create_if_missing: bool,
        /// Character encoding used when reading and writing the INI file.
        #[serde(default)]
        encoding: IniEncoding,
        /// Line ending style expected by the legacy parser or mod.
        #[serde(default)]
        line_ending: LineEnding,
        /// Nested `Section -> Key -> Value` map of settings to write/merge.
        sections: BTreeMap<String, BTreeMap<String, IniValue>>,
    },

    /// Constrain process CPU core affinity and/or priority to prevent legacy timing/thread crashes.
    LimitCpuAffinity {
        /// Policy describing how the Windows processor affinity mask is calculated.
        mode: CpuAffinityMode,
        /// Mechanism used to enforce affinity (process launch mask or `AppCompatFlags` token).
        apply_method: AffinityApplyMethod,
        /// Optional Windows process priority class applied at launch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        process_priority: Option<ProcessPriority>,
    },

    /// Initialize per-game portable sandbox directories and optional copy-only save migration.
    ConfigureSandbox {
        /// Relative subdirectories inside the game/sandbox tree to ensure exist before launch.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        create_directories: Vec<String>,
        /// Copy (never delete) pre-existing saves from standard OS folders into the sandbox.
        #[serde(default)]
        migrate_existing_saves_copy_only: bool,
    },
}

/// Supported archive container formats for `DownloadVerifiedZip`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFormat {
    /// Standard PKZIP `.zip` archive.
    Zip,
    /// 7-Zip `.7z` archive.
    #[serde(rename = "7z")]
    SevenZ,
}

/// Origin for an `ExtractDll` step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DllSource {
    /// DLL supplied directly by the launcher's bundled `runtime_wrappers` distribution.
    BundledToolkit {
        /// Toolkit name (for example `dgvoodoo2`, `dxwrapper`, `wined3d`, `xinput_plus`).
        toolkit: String,
        /// Path inside that bundled toolkit directory.
        relative_path: String,
    },
    /// DLL extracted from an earlier `download_verified_zip` step in the same recipe.
    CachedArchive {
        /// `PatchStep::id` of the preceding `download_verified_zip` step.
        download_step_id: String,
        /// Relative entry path inside that downloaded archive.
        archive_member: String,
    },
}

/// Origin for a `bsdiff` patch payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BinaryPatchSource {
    /// Download the `.bsdiff` file over HTTPS and verify its SHA-256 before patching.
    Download {
        /// HTTPS URL of the `.bsdiff` patch file.
        url: String,
        /// Expected lowercase SHA-256 digest of the `.bsdiff` file.
        sha256: String,
    },
    /// Read a `.bsdiff` file bundled relative to the recipe manifest directory.
    LocalManifestRelative {
        /// Relative path under the recipe/patches directory.
        path: String,
        /// Expected lowercase SHA-256 digest of the `.bsdiff` file.
        sha256: String,
    },
}

/// Windows Registry hive for `AppCompatFlags\Layers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RegistryHive {
    /// `HKEY_CURRENT_USER\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers`.
    #[default]
    Hkcu,
    /// `HKEY_LOCAL_MACHINE\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers` (requires elevation).
    Hklm,
}

/// Standard Windows `AppCompatFlags\Layers` compatibility tokens.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "flag", content = "value", rename_all = "snake_case")]
pub enum AppCompatFlag {
    /// Windows 95 compatibility shim (`WIN95`).
    Win95,
    /// Windows 98 / Me compatibility shim (`WIN98`).
    Win98,
    /// Windows XP Service Pack 2 shim (`WINXPSP2`).
    WinXpSp2,
    /// Windows XP Service Pack 3 shim (`WINXPSP3`).
    WinXpSp3,
    /// Windows Vista Service Pack 2 shim (`VISTASP2`).
    VistaSp2,
    /// Windows 7 shim (`WIN7RTM`).
    Win7Rtm,
    /// Windows 8 shim (`WIN8RTM`).
    Win8Rtm,
    /// Force 256-color (8-bit paletted) mode (`256COLOR`).
    Color8Bit,
    /// Force 16-bit (65536-color) mode (`16BITCOLOR`).
    Color16Bit,
    /// Force 640x480 resolution (`640X480`).
    Resolution640x480,
    /// Legacy DPI scaling override (`HIGHDPIAWARE`).
    HighDpiAware,
    /// Application DPI unaware override (`DPIUNAWARE`).
    DpiUnaware,
    /// Disable fullscreen optimizations (`DISABLEDXMAXIMIZEDWINDOWEDMODE`).
    DisableFullscreenOptimizations,
    /// Request administrator elevation (`RUNASADMIN`).
    RunAsAdmin,
    /// Force single-core affinity shim (`SingleProcAffinity`).
    SingleProcAffinity,
    /// Disable Windows DWM/themes compatibility (`DISABLETHEMES`).
    DisableThemes,
    /// Arbitrary verified Windows AppCompat shim token (must be ASCII without whitespace).
    Custom(String),
}

impl AppCompatFlag {
    /// Returns the exact Windows `AppCompatFlags\Layers` registry value token.
    pub fn to_windows_token(&self) -> &str {
        match self {
            Self::Win95 => "WIN95",
            Self::Win98 => "WIN98",
            Self::WinXpSp2 => "WINXPSP2",
            Self::WinXpSp3 => "WINXPSP3",
            Self::VistaSp2 => "VISTASP2",
            Self::Win7Rtm => "WIN7RTM",
            Self::Win8Rtm => "WIN8RTM",
            Self::Color8Bit => "256COLOR",
            Self::Color16Bit => "16BITCOLOR",
            Self::Resolution640x480 => "640X480",
            Self::HighDpiAware => "HIGHDPIAWARE",
            Self::DpiUnaware => "DPIUNAWARE",
            Self::DisableFullscreenOptimizations => "DISABLEDXMAXIMIZEDWINDOWEDMODE",
            Self::RunAsAdmin => "RUNASADMIN",
            Self::SingleProcAffinity => "SingleProcAffinity",
            Self::DisableThemes => "DISABLETHEMES",
            Self::Custom(token) => token.as_str(),
        }
    }
}

/// Character encoding used when updating legacy INI files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IniEncoding {
    /// UTF-8 without BOM (default for modern ASI/Widescreen INI configs).
    #[default]
    Utf8,
    /// Windows-1252 / ANSI codepage common in 1998–2008 retail configs.
    Windows1252,
    /// UTF-16LE with BOM (used by some Unreal Engine / InstallShield INI files).
    Utf16Le,
}

/// Line ending format when serializing INI changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LineEnding {
    /// Windows CRLF (`\r\n`) — safe for legacy Win32 `GetPrivateProfileStringA` parsers.
    #[default]
    Crlf,
    /// Unix LF (`\n`).
    Lf,
}

/// Value written to an INI key, supporting static literals or runtime-resolved system parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum IniValue {
    /// Placeholder resolved dynamically at patch/launch time on the user's machine.
    Dynamic(DynamicIniValue),
    /// Boolean setting (formatted per target INI convention, e.g., `1`/`0` or `true`/`false`).
    Boolean(bool),
    /// Signed 64-bit integer setting.
    Integer(i64),
    /// Floating-point setting.
    Float(f64),
    /// Literal string setting.
    String(String),
}

/// Wrapper object denoting a dynamic runtime-resolved INI value (`{"dynamic": "desktop_width"}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DynamicIniValue {
    /// Runtime value identifier resolved on the user's host before writing the INI file.
    pub dynamic: DynamicValueKind,
}

/// Runtime parameters resolved dynamically from the user's Windows environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DynamicValueKind {
    /// Primary monitor desktop width in pixels.
    DesktopWidth,
    /// Primary monitor desktop height in pixels.
    DesktopHeight,
    /// Primary monitor refresh rate in Hz.
    RefreshRateHz,
    /// Absolute path to this game's portable sandbox directory.
    GameSandboxDir,
}

/// Policy defining how logical CPU cores are restricted for legacy titles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "policy", rename_all = "snake_case")]
pub enum CpuAffinityMode {
    /// Pin the process strictly to logical core 0 (`0x1`) to fix RDTSC drift / race conditions.
    SingleCore,
    /// Cap the process to at most `count` logical cores (`(1 << min(count, avail, 64)) - 1`).
    MaxLogicalCores {
        /// Maximum number of logical CPU cores allowed (`>= 1`).
        count: u32,
    },
    /// Disable Hyper-Threading/SMT sibling threads (even-numbered logical cores only, capped by `max_cores`).
    PhysicalCoresOnly {
        /// Optional cap on how many even-numbered logical cores to expose.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_cores: Option<u32>,
    },
    /// Explicit 64-bit Windows `SetProcessAffinityMask` bitmask.
    ExplicitMask {
        /// Non-zero 64-bit processor bitmask.
        mask: u64,
    },
}

impl CpuAffinityMode {
    /// Computes the non-zero 64-bit Windows processor affinity mask for `available_logical_cores`.
    ///
    /// Clamps `available_logical_cores` to `1..=64` (standard Windows `DWORD_PTR` group width).
    pub fn to_affinity_mask(&self, available_logical_cores: u32) -> u64 {
        let avail = available_logical_cores.clamp(1, 64);
        match *self {
            Self::SingleCore => 1,
            Self::MaxLogicalCores { count } => {
                let effective = count.clamp(1, avail);
                full_low_bits_mask(effective)
            }
            Self::PhysicalCoresOnly { max_cores } => {
                let mut mask = 0_u64;
                let limit = max_cores.unwrap_or(avail).max(1) as usize;
                for idx in (0..avail).step_by(2).take(limit) {
                    mask |= 1_u64 << idx;
                }
                mask.max(1)
            }
            Self::ExplicitMask { mask } => {
                let avail_mask = full_low_bits_mask(avail);
                let masked = mask & avail_mask;
                if masked == 0 {
                    1
                } else {
                    masked
                }
            }
        }
    }
}

fn full_low_bits_mask(bit_count: u32) -> u64 {
    if bit_count >= 64 {
        u64::MAX
    } else {
        (1_u64 << bit_count) - 1
    }
}

/// How CPU affinity restriction is enforced on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AffinityApplyMethod {
    /// Set via `SetProcessAffinityMask` on the launched child process (non-persistent).
    LauncherProcessMask,
    /// Apply Windows `SingleProcAffinity` AppCompat shim in the registry.
    RegistryShim,
}

/// Windows process priority class applied when launching the game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProcessPriority {
    /// Normal scheduling priority.
    Normal,
    /// Above normal scheduling priority.
    AboveNormal,
    /// High scheduling priority (use cautiously to avoid input starvation).
    High,
}

/// Declarative rollback strategy and cleanup policy for reverting all recipe changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RollbackPlan {
    /// High-level strategy describing how rollback journal entries are replayed.
    pub strategy: RollbackStrategy,

    /// Game-relative directory storing pre-patch file backups and the rollback transaction journal.
    #[serde(default = "default_backup_dir")]
    pub backup_directory: String,

    /// Guarantee that user saves inside the sandbox are never deleted during rollback.
    #[serde(default = "default_true")]
    pub preserve_user_saves: bool,

    /// Whether registry `AppCompatFlags\Layers` edits recorded by the transaction log should be reverted.
    #[serde(default = "default_true")]
    pub restore_registry_values: bool,

    /// Additional relative file or empty-directory paths created by patches that may be cleaned up on rollback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_cleanup_paths: Vec<String>,

    /// Explicit ordered cleanup actions executed during rollback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_actions: Vec<RollbackAction>,
}

/// Strategy used by the rollback engine when reverting a recipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RollbackStrategy {
    /// Automatically reverse every file/registry transaction recorded during installation.
    TransactionalAutomatic,
    /// Reverse recorded transactions and also execute `extra_cleanup_paths` / `custom_actions`.
    AutomaticWithExtras,
    /// Execute only the explicitly enumerated `custom_actions`.
    ExplicitOnly,
}

/// Individual explicit cleanup step within a [`RollbackPlan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RollbackAction {
    /// Delete a specific file placed by a patch step.
    DeleteFile {
        /// Game-relative file path to remove.
        path: String,
    },
    /// Remove a directory only if it is empty after file cleanup.
    DeleteDirectoryIfEmpty {
        /// Game-relative directory path.
        path: String,
    },
    /// Restore a pre-patch backup of the given original file path.
    RestoreBackup {
        /// Game-relative path of the original file to restore from backup.
        original_path: String,
    },
    /// Remove or restore `AppCompatFlags\Layers` registry state for an executable.
    RemoveRegistryCompatFlags {
        /// Registry hive where the flags were written.
        #[serde(default)]
        hive: RegistryHive,
        /// Game-relative executable path.
        target_executable: String,
    },
}

fn default_true() -> bool {
    true
}

fn default_backup_dir() -> String {
    ".ogsc_backup".to_owned()
}
