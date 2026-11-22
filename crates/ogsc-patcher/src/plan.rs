//! Non-destructive dry-run planner (`plan_fixes`) previewing all file, wrapper, and registry changes.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ogsc_recipes::{
    BinaryPatchSource, DllSource, FixOption, GameRecipe, OsFilter, PatchAction, PatchStep,
    RegistryHive,
};
use ogsc_wrappers::{plan_wrapper_deployment, ToolkitKind, ToolkitStore};

use crate::archive::{join_abs_rel, normalize_safe_relative};
use crate::error::PatcherError;
use crate::ini::DynamicContext;
use crate::registry::{merge_compat_flags, RegistryBackend, APP_COMPAT_LAYERS_KEY};
use crate::win32::{current_windows_build, query_primary_display};

/// Host context supplied to [`plan_fixes`] and [`PatchEngine`](crate::PatchEngine).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchContext {
    /// Local `runtime_wrappers` store used to resolve dgVoodoo 2, DxWrapper, WineD3D, XInput Plus, and emulators.
    pub toolkit_store: ToolkitStore,
    /// Local download cache directory storing SHA-256-verified community archives.
    pub cache_dir: PathBuf,
    /// Dynamic display and sandbox parameters substituted into INI configs.
    pub dynamic_ctx: DynamicContext,
    /// Current Windows OS build number (used for `os_filter` step filtering).
    pub windows_build: Option<u32>,
    /// Optional base directory for resolving `BinaryPatchSource::LocalManifestRelative`.
    pub recipe_dir: Option<PathBuf>,
}

impl PatchContext {
    /// Constructs a [`PatchContext`] for `game_root` using live host display metrics and Windows build detection.
    pub fn for_game(
        game_root: &Path,
        toolkit_store: ToolkitStore,
        cache_dir: PathBuf,
        sandbox_root_rel: Option<&str>,
    ) -> Self {
        let (desktop_width, desktop_height, refresh_rate_hz) = query_primary_display();
        let sb_rel = sandbox_root_rel.unwrap_or(".ogsc_sandbox");
        let game_sandbox_dir = game_root.join(sb_rel);

        Self {
            toolkit_store,
            cache_dir,
            dynamic_ctx: DynamicContext {
                desktop_width,
                desktop_height,
                refresh_rate_hz,
                game_sandbox_dir,
            },
            windows_build: current_windows_build(),
            recipe_dir: None,
        }
    }
}

/// Kind of filesystem mutation anticipated by [`plan_fixes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlannedFileOpKind {
    /// Extracts archive files under a destination directory.
    ExtractArchiveToDir,
    /// Creates or replaces a DLL/ASI/FLT file.
    WriteDll,
    /// Copies a bundled compatibility wrapper DLL.
    DeployWrapperDll,
    /// Generates a wrapper or emulator configuration file.
    WriteWrapperConfig,
    /// Creates or merges settings into an INI file.
    MergeIni,
    /// Applies a `bsdiff` binary patch to an existing file.
    BinaryBsdiffPatch,
    /// Ensures a sandbox directory exists.
    CreateDirectory,
}

/// Single file or directory operation anticipated in a dry-run preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFileChange {
    /// Owning fix ID.
    pub fix_id: String,
    /// Owning step ID.
    pub step_id: String,
    /// Operation category.
    pub kind: PlannedFileOpKind,
    /// Game-relative file or directory path.
    pub relative_path: String,
    /// Whether the target path currently exists on disk (indicating backup + overwrite/merge).
    pub already_exists: bool,
    /// Concise human-readable description shown in the Patch Advisor popup.
    pub detail: String,
}

/// Registry modification anticipated by [`plan_fixes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRegistryChange {
    /// Owning fix ID.
    pub fix_id: String,
    /// Owning step ID.
    pub step_id: String,
    /// Target registry hive (`hkcu` or `hklm`).
    pub hive: RegistryHive,
    /// Registry subkey path.
    pub key_path: String,
    /// Value name (resolved absolute path to target executable).
    pub value_name: String,
    /// Current value on disk/backend if readable.
    pub current_value: Option<String>,
    /// Resulting merged value that will be written.
    pub planned_value: String,
}

/// HTTPS download anticipated by [`plan_fixes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedDownload {
    /// Owning fix ID.
    pub fix_id: String,
    /// Owning step ID.
    pub step_id: String,
    /// Primary HTTPS URL.
    pub url: String,
    /// Expected SHA-256 digest.
    pub sha256: String,
    /// Whether `<cache_dir>/<sha256>` is already cached locally.
    pub cached_locally: bool,
}

/// Wrapper toolkit deployment summary in a dry-run preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedWrapperSummary {
    /// Owning fix ID.
    pub fix_id: String,
    /// Wrapper slug (`dgvoodoo2`, `dxwrapper`, `wined3d`, `xinput_plus`, `dosbox_x`, `86box`).
    pub wrapper_name: String,
    /// Relative destination directory inside `game_root`.
    pub target_dir: String,
    /// Number of DLL files to copy.
    pub dll_count: usize,
    /// Number of generated config files to write.
    pub config_count: usize,
    /// Missing toolkit DLL source paths on disk (empty when ready to deploy).
    pub missing_toolkit_assets: Vec<PathBuf>,
    /// Whether this toolkit can be auto-downloaded from an official verified GitHub release.
    pub auto_download_available: bool,
}

/// Per-fix preview group rendered in the Patch Advisor confirmation dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFixPreview {
    /// Fix identifier (`FixOption::id`).
    pub fix_id: String,
    /// Human-readable fix title.
    pub title: String,
    /// Number of active steps in this fix (after release/OS filtering).
    pub active_step_count: usize,
    /// Number of steps skipped due to release or OS build filtering.
    pub skipped_step_count: usize,
}

/// Complete dry-run plan returned by [`plan_fixes`] before touching the game directory or registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchPlan {
    /// Target recipe `game_id`.
    pub game_id: String,
    /// Target `release_id`.
    pub release_id: String,
    /// Ordered fix summaries.
    pub fixes: Vec<PlannedFixPreview>,
    /// Anticipated HTTPS archive/patch downloads.
    pub downloads: Vec<PlannedDownload>,
    /// Anticipated file and directory changes.
    pub file_changes: Vec<PlannedFileChange>,
    /// Anticipated wrapper deployments.
    pub wrappers: Vec<PlannedWrapperSummary>,
    /// Anticipated Windows Registry changes.
    pub registry_changes: Vec<PlannedRegistryChange>,
    /// Launch-time CPU affinity/priority configurations.
    pub launch_notes: Vec<String>,
    /// Non-fatal warnings (for example missing local toolkit assets or skipped steps).
    pub warnings: Vec<String>,
}

impl PatchPlan {
    /// Returns the deduplicated sorted list of all game-relative file paths touched by this plan.
    pub fn unique_target_files(&self) -> Vec<String> {
        let set: BTreeSet<&str> = self
            .file_changes
            .iter()
            .filter(|c| c.kind != PlannedFileOpKind::CreateDirectory)
            .map(|c| c.relative_path.as_str())
            .collect();
        set.into_iter().map(str::to_owned).collect()
    }
}

/// Computes a pure read-only [`PatchPlan`] for `selected_fix_ids` without mutating disk or registry.
pub fn plan_fixes(
    recipe: &GameRecipe,
    release_id: &str,
    game_root: &Path,
    selected_fix_ids: &[String],
    ctx: &PatchContext,
    registry: &dyn RegistryBackend,
) -> Result<PatchPlan, PatcherError> {
    let selected_fixes = resolve_selected_fixes(recipe, release_id, selected_fix_ids)?;

    let mut plan = PatchPlan {
        game_id: recipe.game_id.clone(),
        release_id: release_id.to_owned(),
        fixes: Vec::new(),
        downloads: Vec::new(),
        file_changes: Vec::new(),
        wrappers: Vec::new(),
        registry_changes: Vec::new(),
        launch_notes: Vec::new(),
        warnings: Vec::new(),
    };

    for fix in selected_fixes {
        let mut active_step_count = 0_usize;
        let mut skipped_step_count = 0_usize;

        for step in &fix.steps {
            if let Some(reason) = step_skip_reason(step, release_id, ctx.windows_build) {
                skipped_step_count += 1;
                plan.warnings.push(format!("step `{}` skipped: {reason}", step.id));
                continue;
            }
            active_step_count += 1;
            inspect_step_for_plan(fix, step, game_root, ctx, registry, &mut plan)?;
        }

        plan.fixes.push(PlannedFixPreview {
            fix_id: fix.id.clone(),
            title: fix.title.clone(),
            active_step_count,
            skipped_step_count,
        });
    }

    Ok(plan)
}

pub(crate) fn resolve_selected_fixes<'a>(
    recipe: &'a GameRecipe,
    release_id: &str,
    selected_fix_ids: &[String],
) -> Result<Vec<&'a FixOption>, PatcherError> {
    let mut seen = BTreeSet::new();
    for fix_id in selected_fix_ids {
        let Some(fix) = recipe.fix_by_id(fix_id) else {
            return Err(PatcherError::UnknownFixId {
                game_id: recipe.game_id.clone(),
                release_id: release_id.to_owned(),
                fix_id: fix_id.clone(),
            });
        };
        let applies = fix.applies_to_releases.is_empty()
            || fix.applies_to_releases.iter().any(|r| r == release_id);
        if !applies {
            return Err(PatcherError::UnknownFixId {
                game_id: recipe.game_id.clone(),
                release_id: release_id.to_owned(),
                fix_id: fix_id.clone(),
            });
        }
        seen.insert(fix_id.as_str());
    }

    // Always execute in recipe-declared order for deterministic layering.
    Ok(recipe.fixes.iter().filter(|fix| seen.contains(fix.id.as_str())).collect())
}

pub(crate) fn step_skip_reason(
    step: &PatchStep,
    release_id: &str,
    windows_build: Option<u32>,
) -> Option<String> {
    if !step.applies_to_releases.is_empty()
        && !step.applies_to_releases.iter().any(|r| r == release_id)
    {
        return Some(format!("applies only to releases {:?}", step.applies_to_releases));
    }
    if let (Some(filter), Some(build)) = (step.os_filter, windows_build) {
        if !matches_os_filter(filter, build) {
            return Some(format!("Windows build {build} is outside os_filter"));
        }
    }
    None
}

fn matches_os_filter(filter: OsFilter, build: u32) -> bool {
    filter.min_windows_build.is_none_or(|min_b| build >= min_b)
        && filter.max_windows_build.is_none_or(|max_b| build <= max_b)
}

fn inspect_step_for_plan(
    fix: &FixOption,
    step: &PatchStep,
    game_root: &Path,
    ctx: &PatchContext,
    registry: &dyn RegistryBackend,
    plan: &mut PatchPlan,
) -> Result<(), PatcherError> {
    match &step.action {
        PatchAction::DownloadVerifiedZip { url, sha256, extract_to, include_patterns, .. } => {
            inspect_download_for_plan(
                fix,
                step,
                url,
                sha256,
                extract_to,
                include_patterns,
                ctx,
                plan,
            )?;
        }
        PatchAction::ExtractDll { source, destination_path, .. } => {
            inspect_extract_dll_for_plan(
                fix,
                step,
                source,
                destination_path,
                game_root,
                ctx,
                plan,
            )?;
        }
        PatchAction::DeployWrapper(deployment) => {
            inspect_wrapper_for_plan(fix, step, deployment, game_root, ctx, plan)?;
        }
        PatchAction::ApplyBinaryBsdiffPatch { target_file, patch_source, .. } => {
            inspect_bsdiff_for_plan(fix, step, target_file, patch_source, game_root, ctx, plan)?;
        }
        PatchAction::SetRegistryCompatFlags {
            hive,
            target_executable,
            flags,
            merge_with_existing,
        } => {
            let norm_exe = normalize_safe_relative(target_executable, false)?;
            let abs_exe = join_abs_rel(game_root, &norm_exe);
            let value_name = abs_exe.to_string_lossy().into_owned();
            let current_value = registry.read_string(*hive, APP_COMPAT_LAYERS_KEY, &value_name)?;
            let planned_value =
                merge_compat_flags(current_value.as_deref(), flags, *merge_with_existing);
            plan.registry_changes.push(PlannedRegistryChange {
                fix_id: fix.id.clone(),
                step_id: step.id.clone(),
                hive: *hive,
                key_path: APP_COMPAT_LAYERS_KEY.to_owned(),
                value_name,
                current_value,
                planned_value,
            });
        }
        PatchAction::WriteIniConfig { target_file, sections, .. } => {
            let norm_ini = normalize_safe_relative(target_file, false)?;
            let ini_abs = join_abs_rel(game_root, &norm_ini);
            plan.file_changes.push(PlannedFileChange {
                fix_id: fix.id.clone(),
                step_id: step.id.clone(),
                kind: PlannedFileOpKind::MergeIni,
                relative_path: norm_ini,
                already_exists: ini_abs.exists(),
                detail: format!("Merge {} INI section(s)", sections.len()),
            });
        }
        PatchAction::LimitCpuAffinity { mode, apply_method, process_priority } => {
            plan.launch_notes.push(format!(
                "CPU affinity policy `{mode:?}` via `{apply_method:?}` (priority: {process_priority:?})"
            ));
        }
        PatchAction::ConfigureSandbox { create_directories, .. } => {
            for dir in create_directories {
                let norm_dir = normalize_safe_relative(dir, false)?;
                let dir_abs = join_abs_rel(game_root, &norm_dir);
                plan.file_changes.push(PlannedFileChange {
                    fix_id: fix.id.clone(),
                    step_id: step.id.clone(),
                    kind: PlannedFileOpKind::CreateDirectory,
                    relative_path: norm_dir,
                    already_exists: dir_abs.exists(),
                    detail: "Ensure portable sandbox directory".to_owned(),
                });
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn inspect_extract_dll_for_plan(
    fix: &FixOption,
    step: &PatchStep,
    source: &DllSource,
    destination_path: &str,
    game_root: &Path,
    ctx: &PatchContext,
    plan: &mut PatchPlan,
) -> Result<(), PatcherError> {
    let norm_dest = normalize_safe_relative(destination_path, false)?;
    let dest_abs = join_abs_rel(game_root, &norm_dest);
    let detail = match source {
        DllSource::BundledToolkit { toolkit, relative_path } => {
            if let Err(err) = ctx.toolkit_store.resolve_bundled_file(toolkit, relative_path) {
                plan.warnings.push(format!("step `{}`: {err}", step.id));
            }
            format!("Copy `{toolkit}/{relative_path}`")
        }
        DllSource::CachedArchive { download_step_id, archive_member } => {
            format!("Extract `{archive_member}` from `{download_step_id}`")
        }
    };
    plan.file_changes.push(PlannedFileChange {
        fix_id: fix.id.clone(),
        step_id: step.id.clone(),
        kind: PlannedFileOpKind::WriteDll,
        relative_path: norm_dest,
        already_exists: dest_abs.exists(),
        detail,
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn inspect_bsdiff_for_plan(
    fix: &FixOption,
    step: &PatchStep,
    target_file: &str,
    patch_source: &BinaryPatchSource,
    game_root: &Path,
    ctx: &PatchContext,
    plan: &mut PatchPlan,
) -> Result<(), PatcherError> {
    let norm_target = normalize_safe_relative(target_file, false)?;
    let target_abs = join_abs_rel(game_root, &norm_target);
    if let BinaryPatchSource::Download { url, sha256 } = patch_source {
        plan.downloads.push(PlannedDownload {
            fix_id: fix.id.clone(),
            step_id: step.id.clone(),
            url: url.clone(),
            sha256: sha256.clone(),
            cached_locally: ctx.cache_dir.join(sha256.to_ascii_lowercase()).is_file(),
        });
    }
    plan.file_changes.push(PlannedFileChange {
        fix_id: fix.id.clone(),
        step_id: step.id.clone(),
        kind: PlannedFileOpKind::BinaryBsdiffPatch,
        relative_path: norm_target,
        already_exists: target_abs.exists(),
        detail: "Apply SHA-256-verified bsdiff binary patch".to_owned(),
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn inspect_download_for_plan(
    fix: &FixOption,
    step: &PatchStep,
    url: &str,
    sha256: &str,
    extract_to: &str,
    include_patterns: &[String],
    ctx: &PatchContext,
    plan: &mut PatchPlan,
) -> Result<(), PatcherError> {
    let norm_dir = normalize_safe_relative(extract_to, true)?;
    let cached_locally = ctx.cache_dir.join(sha256.to_ascii_lowercase()).is_file();
    plan.downloads.push(PlannedDownload {
        fix_id: fix.id.clone(),
        step_id: step.id.clone(),
        url: url.to_owned(),
        sha256: sha256.to_owned(),
        cached_locally,
    });
    let detail = if include_patterns.is_empty() {
        "Extract verified archive contents".to_owned()
    } else {
        format!("Extract {} archive pattern(s)", include_patterns.len())
    };
    plan.file_changes.push(PlannedFileChange {
        fix_id: fix.id.clone(),
        step_id: step.id.clone(),
        kind: PlannedFileOpKind::ExtractArchiveToDir,
        relative_path: norm_dir,
        already_exists: false,
        detail,
    });
    Ok(())
}

fn inspect_wrapper_for_plan(
    fix: &FixOption,
    step: &PatchStep,
    deployment: &ogsc_recipes::WrapperDeployment,
    game_root: &Path,
    ctx: &PatchContext,
    plan: &mut PatchPlan,
) -> Result<(), PatcherError> {
    let wplan = plan_wrapper_deployment(&ctx.toolkit_store, deployment)?;
    let mut missing_assets = Vec::new();
    for copy in &wplan.dll_copies {
        if !copy.source_exists {
            missing_assets.push(copy.source_path.clone());
        }
        let dest_abs = join_abs_rel(game_root, &copy.destination_relative_path);
        let detail = match &copy.chained_from {
            Some(orig) => format!("Deploy `{}` DLL (chained from `{orig}`)", wplan.wrapper_name),
            None => format!("Deploy `{}` DLL", wplan.wrapper_name),
        };
        plan.file_changes.push(PlannedFileChange {
            fix_id: fix.id.clone(),
            step_id: step.id.clone(),
            kind: PlannedFileOpKind::DeployWrapperDll,
            relative_path: copy.destination_relative_path.clone(),
            already_exists: dest_abs.exists(),
            detail,
        });
    }
    for cfg in &wplan.config_writes {
        let dest_abs = join_abs_rel(game_root, &cfg.destination_relative_path);
        plan.file_changes.push(PlannedFileChange {
            fix_id: fix.id.clone(),
            step_id: step.id.clone(),
            kind: PlannedFileOpKind::WriteWrapperConfig,
            relative_path: cfg.destination_relative_path.clone(),
            already_exists: dest_abs.exists(),
            detail: format!("Generate `{}` configuration", wplan.wrapper_name),
        });
    }
    let official = ToolkitKind::from_wrapper_kind(&deployment.wrapper).official_download();
    if !missing_assets.is_empty() {
        if let Some(spec) = official {
            plan.downloads.push(PlannedDownload {
                fix_id: fix.id.clone(),
                step_id: format!("{}-toolkit", step.id),
                url: spec.url.to_owned(),
                sha256: spec.sha256.to_owned(),
                cached_locally: ctx.cache_dir.join(spec.sha256).is_file(),
            });
        } else {
            plan.warnings.push(format!(
                "wrapper `{}` is missing {} local asset(s) in `{}`",
                wplan.wrapper_name,
                missing_assets.len(),
                ctx.toolkit_store.root_dir().display()
            ));
        }
    }
    plan.wrappers.push(PlannedWrapperSummary {
        fix_id: fix.id.clone(),
        wrapper_name: wplan.wrapper_name.to_owned(),
        target_dir: wplan.target_dir,
        dll_count: wplan.dll_copies.len(),
        config_count: wplan.config_writes.len(),
        missing_toolkit_assets: missing_assets,
        auto_download_available: official.is_some(),
    });
    Ok(())
}
