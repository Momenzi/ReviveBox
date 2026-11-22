//! Core `GameRecipe` schema types representing a legacy PC game compatibility manifest.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::steps::{FixOption, PatchStep, RollbackPlan};

/// Current declarative recipe format version supported by this build of `ogsc-recipes`.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Complete declarative compatibility manifest for a legacy PC game (1995–2012).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GameRecipe {
    /// Integer schema version (must match [`CURRENT_SCHEMA_VERSION`]).
    pub schema_version: u32,

    /// Stable lowercase slug identifying the game (for example `nfs-most-wanted-2005`).
    pub game_id: String,

    /// Full display title of the game.
    pub title: String,

    /// Original PC release year (validated within `1980..=2030`).
    pub release_year: u16,

    /// Original game developer studio, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developer: Option<String>,

    /// Original PC publisher, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,

    /// Provenance, authorship, licensing, and external artwork lookup metadata.
    pub metadata: RecipeMetadata,

    /// Unpatched health status on modern Windows 10/11 systems.
    pub baseline_health: HealthStatus,

    /// Expected health status once all recommended fixes (and user prerequisites) are applied.
    pub post_fix_health: HealthStatus,

    /// Recognized game releases and executable fingerprints used for BYOG identification.
    pub releases: Vec<GameRelease>,

    /// Cataloged Windows 10/11 compatibility issues shown in the Patch Advisor popup.
    pub known_issues: Vec<KnownIssue>,

    /// Target binary architecture, graphics APIs, and emulator requirements.
    pub runtime_requirements: RuntimeRequirements,

    /// Optional per-game portable sandbox redirection settings to keep Documents/AppData clean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxConfig>,

    /// Ordered list of selectable compatibility fix groups presented in the Patch Advisor popup.
    pub fixes: Vec<FixOption>,

    /// Process launch arguments, working directory, environment overrides, or emulator hooks.
    pub launch: LaunchConfig,

    /// Non-destructive backup and uninstall instructions to cleanly revert all applied fixes.
    pub rollback: RollbackPlan,
}

impl GameRecipe {
    /// Returns the ordered slice of selectable [`FixOption`] entries applicable to `release_id`.
    ///
    /// Fixes with an empty `applies_to_releases` list apply to all releases.
    pub fn fixes_for_release(&self, release_id: &str) -> Vec<&FixOption> {
        self.fixes
            .iter()
            .filter(|fix| {
                fix.applies_to_releases.is_empty()
                    || fix.applies_to_releases.iter().any(|id| id == release_id)
            })
            .collect()
    }

    /// Looks up a selectable [`FixOption`] by `fix_id`.
    pub fn fix_by_id(&self, fix_id: &str) -> Option<&FixOption> {
        self.fixes.iter().find(|fix| fix.id == fix_id)
    }

    /// Collects ordered [`PatchStep`]s across all `selected_fix_ids` applicable to `release_id`.
    pub fn steps_for_selected_fixes(
        &self,
        release_id: &str,
        selected_fix_ids: &[String],
    ) -> Vec<&PatchStep> {
        self.fixes
            .iter()
            .filter(|fix| {
                selected_fix_ids.iter().any(|id| id == &fix.id)
                    && (fix.applies_to_releases.is_empty()
                        || fix.applies_to_releases.iter().any(|id| id == release_id))
            })
            .flat_map(|fix| fix.steps.iter())
            .filter(|step| {
                step.applies_to_releases.is_empty()
                    || step.applies_to_releases.iter().any(|id| id == release_id)
            })
            .collect()
    }

    /// Looks up a [`GameRelease`] entry by `release_id`.
    pub fn release_by_id(&self, release_id: &str) -> Option<&GameRelease> {
        self.releases.iter().find(|release| release.release_id == release_id)
    }

    /// Returns `true` when `release_id` is listed in `metadata.incompatible_releases`.
    pub fn is_release_incompatible(&self, release_id: &str) -> bool {
        self.metadata.incompatible_releases.iter().any(|id| id == release_id)
    }

    /// Resolves the game-relative launch executable path for `release_id`, falling back to `launch.default_executable`.
    pub fn executable_for_release(&self, release_id: &str) -> &str {
        self.launch
            .release_executable_overrides
            .get(release_id)
            .map_or(self.launch.default_executable.as_str(), String::as_str)
    }

    /// Resolves the launch executable path taking applied `fix_executable_overrides` into account,
    /// then `release_executable_overrides`, then `launch.default_executable`.
    pub fn executable_for_launch(&self, release_id: &str, applied_fix_ids: &[String]) -> &str {
        for fix_id in applied_fix_ids.iter().rev() {
            if let Some(exe) = self.launch.fix_executable_overrides.get(fix_id) {
                return exe.as_str();
            }
        }
        self.executable_for_release(release_id)
    }
}

/// Authorship, licensing, and external database identifiers for a recipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecipeMetadata {
    /// SemVer string tracking revisions to this recipe (for example `1.0.0`).
    pub recipe_version: String,

    /// Maintainers or contributors who authored and verified the recipe.
    pub authors: Vec<String>,

    /// License governing the recipe file itself (for example `MIT` or `MIT OR Apache-2.0`).
    pub license: String,

    /// Last modification date in ISO-8601 `YYYY-MM-DD` format.
    pub updated_at: String,

    /// Optional HTTPS link to the PCGamingWiki article documenting these fixes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pcgamingwiki_url: Option<String>,

    /// Optional Steam App ID for artwork or distribution cross-referencing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steam_app_id: Option<u32>,

    /// Optional SteamGridDB game ID for automatic cover/grid art fetching in the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steamgriddb_id: Option<u64>,

    /// Optional direct HTTPS cover artwork image URL (PNG, JPEG, or WebP) cached locally by the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_art_url: Option<String>,

    /// Human-readable required game version, patch level, or runtime prerequisites shown prominently in the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_game_version: Option<String>,

    /// Recognized `release_id`s that are known-incompatible with this recipe's core fix (triggers a prominent UI warning).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incompatible_releases: Vec<String>,

    /// Optional maintainer notes (for example hash verification status or regional caveats).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Compatibility badge shown in the launcher UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// Crashes, refuses to start, or is blocked by legacy kernel DRM/16-bit incompatibility on Win11.
    BrokenOnWin11,
    /// Starts with severe rendering, audio, or input failures unless a wrapper/shim is deployed.
    NeedsWrapper,
    /// Boots and runs, but suffers from aspect-ratio, FPS physics, or stability issues.
    PartiallyPlayable,
    /// Runs reliably on Windows 10/11 with proper scaling, controls, and saves.
    ReadyToPlay,
}

/// Specific edition, patch level, or storefront release matched by the scanner engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GameRelease {
    /// Release slug unique within this recipe (for example `retail-cd-v1.3`).
    pub release_id: String,

    /// Display label describing edition and patch version.
    pub label: String,

    /// Distribution channel or physical release format.
    pub distribution: Distribution,

    /// Game version string (for example `1.3` or `1.01`).
    pub version: String,

    /// Optional release region code (for example `US`, `EU`, `Global`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,

    /// Game-relative path to the primary executable to fingerprint.
    pub executable_path: String,

    /// Cryptographic and size fingerprint used to identify this exact release.
    pub fingerprint: ExecutableFingerprint,

    /// Additional game-relative data files whose presence boosts identification confidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sentinel_files: Vec<String>,
}

/// Distribution source of a user-owned legacy game installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Distribution {
    /// Physical Compact Disc retail release.
    RetailCd,
    /// Physical DVD-ROM retail release.
    RetailDvd,
    /// Steam digital storefront release.
    Steam,
    /// GOG.com DRM-free release.
    Gog,
    /// EA App / Origin release.
    EaApp,
    /// Ubisoft Connect / Uplay release.
    UbisoftConnect,
    /// Epic Games Store release.
    EpicGames,
    /// Legally distributed freeware or open-source release.
    CommunityFreeware,
    /// Other legitimate storefront or bundle release.
    Other,
}

/// Cryptographic and byte-size fingerprint for matching a local executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecutableFingerprint {
    /// Exact executable size in bytes for fast pre-filtering during directory scans.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_size_bytes: Option<u64>,

    /// Fast partial SHA-256 slice (for example first 64 KiB) to rapidly narrow candidates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_sha256: Option<PartialHash>,

    /// Full lowercase hexadecimal SHA-256 digest of the executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,

    /// Full lowercase hexadecimal MD5 digest (commonly published on legacy compatibility wikis).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,

    /// Provenance confidence level of the recorded hash values.
    #[serde(default)]
    pub verification_status: HashVerificationStatus,
}

/// Partial byte-range SHA-256 digest used for ultra-fast initial executable fingerprinting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PartialHash {
    /// Byte offset from the start of the file (typically `0`).
    #[serde(default)]
    pub offset: u64,

    /// Number of bytes hashed starting at `offset` (must be `> 0`, typically `65536`).
    pub length: u64,

    /// Expected lowercase 64-character hex SHA-256 of the byte slice.
    pub sha256: String,
}

/// Provenance status indicating whether a cryptographic digest has been independently verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HashVerificationStatus {
    /// Verified directly against a known-good binary or official release asset.
    #[default]
    Verified,
    /// Sourced from trusted community databases (for example PCGamingWiki) awaiting local confirmation.
    CommunityReported,
    /// Placeholder template hash requiring real binary verification before strict release approval.
    UnverifiedTemplate,
}

/// Documented legacy compatibility defect and its remediation mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KnownIssue {
    /// Issue identifier unique within this recipe.
    pub id: String,

    /// High-level root-cause category.
    pub category: IssueCategory,

    /// Impact severity on Windows 10/11.
    pub severity: IssueSeverity,

    /// Short one-line summary shown in the UI issue list.
    pub summary: String,

    /// Technical explanation of the root cause and fix behavior.
    pub details: String,

    /// Selectable fix IDs from `fixes` that address this issue (empty if manual user action is required).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixed_by_fixes: Vec<String>,

    /// Clear BYOG guidance when a prerequisite (such as installing an official publisher patch) needs user action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_action_required: Option<String>,
}

/// Taxonomy of legacy Windows game compatibility failure modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssueCategory {
    /// Legacy disc DRM kernel drivers blocked on Vista+ (SafeDisc `secdrv.sys`, SecuROM, StarForce).
    LegacyDrm,
    /// Deprecated DirectDraw, Direct3D 1–9, or 3dfx Glide rendering bugs.
    GraphicsApi,
    /// Physics, AI, or simulation timers tied to frame rate above 60 FPS.
    HighFpsPhysics,
    /// Missing 16:9/21:9 resolutions, stretched HUD, or clipped FOV.
    WidescreenAndHud,
    /// DirectSound3D / EAX hardware audio loss or stuttering.
    Audio,
    /// Legacy DirectInput / WinMM controller axis/trigger incompatibilities.
    ControllerInput,
    /// Multi-core race conditions, RDTSC drift, or modern CPU thread count crashes.
    CpuAffinityAndTiming,
    /// 16-bit NE/Win16 or MS-DOS binaries unsupported natively on 64-bit Windows (NTVDM absent).
    SixteenBitSubsystem,
    /// Program Files write failures, UAC VirtualStore redirection, or Documents folder clutter.
    PermissionsAndVirtualStore,
    /// Heap corruption, DEP/ASLR issues, or intro codec crashes.
    CrashOrStability,
}

/// Impact level of a [`KnownIssue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    /// Prevents the game from launching or progressing past startup.
    Blocker,
    /// Severe graphical corruption, frequent crashes, or broken core controls.
    Major,
    /// Playable but with visual glitches, wrong aspect ratio, or minor audio bugs.
    Minor,
    /// Quality-of-life improvement (higher shadow resolution, portable saves, modern controller prompts).
    Enhancement,
}

/// Target runtime environment and graphics API translation metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuntimeRequirements {
    /// Primary executable architecture.
    pub architecture: BinaryArchitecture,

    /// Whether the game must be launched via DOSBox-X or 86Box on 64-bit Windows.
    pub requires_16bit_emulator: bool,

    /// Graphics APIs originally used by the unpatched binary (for example `Direct3D 9`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub native_graphics_api: Vec<String>,

    /// Modern graphics API targeted after wrapper deployment (for example `Direct3D 11`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_graphics_api: Option<String>,

    /// Whether the executable inherently requires elevated privileges (avoid whenever possible).
    #[serde(default)]
    pub requires_admin: bool,
}

/// CPU/OS executable format of a legacy title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BinaryArchitecture {
    /// MS-DOS 16-bit / DOS4GW 32-bit extender.
    Dos,
    /// Windows 16-bit New Executable (NE).
    Win16,
    /// Windows 32-bit Portable Executable (PE32).
    Win32,
    /// Windows 64-bit Portable Executable (PE32+).
    Win64,
}

/// Per-game portable sandbox redirection configuration to avoid polluting user OS folders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Whether portable save/config isolation is active by default.
    #[serde(default = "default_true")]
    pub enabled_by_default: bool,

    /// Game-relative root folder hosting sandboxed saves and user profiles.
    #[serde(default = "default_sandbox_root")]
    pub sandbox_root: String,

    /// Redirect `USERPROFILE` / `My Documents` lookups into the sandbox when supported.
    #[serde(default)]
    pub redirect_documents: bool,

    /// Redirect `LOCALAPPDATA` into the sandbox at launch.
    #[serde(default)]
    pub redirect_appdata_local: bool,

    /// Redirect `APPDATA` (Roaming) into the sandbox at launch.
    #[serde(default)]
    pub redirect_appdata_roaming: bool,

    /// Subpaths relative to `sandbox_root` or user documents where save files are stored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_save_subpaths: Vec<String>,
}

/// Launch invocation settings used after compatibility fixes are applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchConfig {
    /// Default game-relative executable path to spawn.
    pub default_executable: String,

    /// Optional release-specific launch executable overrides keyed by `release_id`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub release_executable_overrides: BTreeMap<String, String>,

    /// Optional fix-specific launch executable overrides keyed by applied `fix_id`
    /// (for example launching a local revival server wrapper when that fix is active).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fix_executable_overrides: BTreeMap<String, String>,

    /// Optional game-relative working directory (defaults to the executable's parent directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,

    /// Command-line flags passed to the launched executable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<String>,

    /// Extra environment variables injected into the launched process.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,

    /// Optional emulator hook for DOS / Win16 titles requiring DOSBox-X or 86Box.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emulator_hook: Option<EmulatorLaunchHook>,
}

/// Configuration binding a game launch to DOSBox-X or 86Box.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmulatorLaunchHook {
    /// Which bundled/configured emulator executes the title.
    pub emulator: EmulatorKind,

    /// Game-relative emulator config file generated or referenced at launch.
    pub config_file: String,

    /// Optional hint describing user-provided optical/floppy disc image requirements.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_image_hint: Option<String>,
}

/// Emulator runtime identifiers for non-Win64-native titles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum EmulatorKind {
    /// DOSBox-X emulator (`dosbox_x`).
    #[serde(rename = "dosbox_x")]
    DosBoxX,
    /// 86Box PC hardware emulator (`86box`).
    #[serde(rename = "86box")]
    Box86,
}

fn default_true() -> bool {
    true
}

fn default_sandbox_root() -> String {
    ".ogsc_sandbox".to_owned()
}
