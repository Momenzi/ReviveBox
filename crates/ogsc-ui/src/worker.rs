//! Asynchronous background threads reporting progress and results back to the `egui` UI loop.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use ogsc_patcher::{
    ApplyReport, HttpDownloader, LaunchedProcess, PatchContext, PatchEngine, PatchEvent,
    RegistryBackend, ResolvedLaunchCommand, APP_COMPAT_LAYERS_KEY,
};
use ogsc_recipes::{GameRecipe, RecipeDatabase, RegistryHive};
use ogsc_scanner::{BackupManager, GameScanner, RollbackSummary, ScanOptions, ScanReport};
use ogsc_wrappers::ToolkitStore;

use crate::covers::{load_or_fetch_cover, CoverLoadOutcome, CoverRequest, UreqCoverClient};
use crate::debug_console::{
    describe_exit_code, diagnose_crash_hints, format_exit_code_hex, inspect_game_directory_mods,
    inspect_pe_summary, query_recent_windows_crash_events, DebugLogLevel, DebugLogLine,
    DetectedGameMods, PeSummary,
};
use crate::recipe_sync::{sync_recipes_from_github, SyncOutcome, UreqSyncClient};
use crate::settings::AppSettings;

/// Events delivered from background worker threads to the main `OgscApp` UI thread.
pub enum WorkerMessage {
    /// Completed scanning one or more user-selected folders.
    ScanFinished {
        /// Existing folders included in this scan pass.
        scanned_folders: Vec<PathBuf>,
        /// Configured scan folders skipped because they do not currently exist on disk.
        skipped_missing_folders: Vec<PathBuf>,
        /// Scan report or formatted error string.
        result: Result<ScanReport, String>,
    },

    /// Completed remote GitHub recipe synchronization + local recipe database reload.
    SyncFinished {
        /// Summary of remote sync (or offline fallback).
        outcome: SyncOutcome,
        /// Reloaded local [`RecipeDatabase`] reflecting any updated `.json` manifests.
        reloaded_db: Option<RecipeDatabase>,
    },

    /// Completed asynchronous cover load or SteamGridDB fetch.
    CoverLoaded(CoverLoadOutcome),

    /// Live patch progress event streamed while applying selected fixes.
    PatchEvent {
        /// Library entry ID being patched.
        entry_id: String,
        /// Event emitted by `ogsc-patcher`.
        event: PatchEvent,
    },

    /// Completed transactional fix application for `entry_id`.
    PatchFinished {
        /// Library entry ID.
        entry_id: String,
        /// Game-specific apply report or error message.
        result: Result<ApplyReport, String>,
    },

    /// Completed rollback + registry restoration for `entry_id`.
    RollbackFinished {
        /// Library entry ID.
        entry_id: String,
        /// Remaining applied fix IDs recorded in `.ogsc_backup` (empty after full rollback).
        remaining_applied_fix_ids: Vec<String>,
        /// Rollback summary or error message.
        result: Result<RollbackSummary, String>,
    },

    /// Single diagnostic/stdout/stderr log line streamed to the Debug Console.
    DebugLog(DebugLogLine),

    /// Notification emitted when a monitored game process exits (or finishes startup crash watch).
    GameExited {
        /// Game title associated with the launch.
        title: String,
        /// Process identifier, when reported.
        pid: Option<u32>,
        /// Process exit code, when observable.
        exit_code: Option<i32>,
        /// Observed process runtime in milliseconds.
        elapsed_ms: u64,
        /// Whether the run exited abnormally or triggered an Event Viewer crash record.
        crashed: bool,
    },
}

/// Partitions configured scan folders into existing directories and missing/unmounted paths.
pub(crate) fn partition_existing_scan_folders(
    folders: Vec<PathBuf>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    folders.into_iter().partition(|dir| dir.is_dir())
}

/// Spawns a background scan of `folders` against `recipe_db` and wakes the UI thread when done.
pub fn spawn_scan_folders(
    tx: Sender<WorkerMessage>,
    ctx: egui::Context,
    folders: Vec<PathBuf>,
    recipe_db: RecipeDatabase,
) {
    thread::spawn(move || {
        let (existing, missing) = partition_existing_scan_folders(folders);
        let result = if existing.is_empty() {
            Err("No configured scan folders currently exist on disk.".to_owned())
        } else {
            let scanner = GameScanner::new(&recipe_db, ScanOptions::default());
            scanner.scan_roots(&existing).map_err(|err| err.to_string())
        };
        let _ = tx.send(WorkerMessage::ScanFinished {
            scanned_folders: existing,
            skipped_missing_folders: missing,
            result,
        });
        ctx.request_repaint();
    });
}

/// Spawns GitHub recipe synchronization into `settings.recipes_dir` with automatic offline fallback.
pub fn spawn_sync_recipes(tx: Sender<WorkerMessage>, ctx: egui::Context, settings: AppSettings) {
    thread::spawn(move || {
        let client = UreqSyncClient::new();
        let outcome = sync_recipes_from_github(
            &client,
            &settings.github_repo_url,
            &settings.github_branch,
            &settings.github_recipes_path,
            &settings.recipes_dir,
        );
        let reloaded_db = if settings.recipes_dir.is_dir() {
            RecipeDatabase::load_from_dir(&settings.recipes_dir, false).ok().map(|loaded| loaded.db)
        } else {
            None
        };
        let _ = tx.send(WorkerMessage::SyncFinished { outcome, reloaded_db });
        ctx.request_repaint();
    });
}

/// Spawns asynchronous cover lookup / SteamGridDB download for a single game card.
pub fn spawn_load_cover(tx: Sender<WorkerMessage>, ctx: egui::Context, req: CoverRequest) {
    thread::spawn(move || {
        let client = UreqCoverClient::new();
        let outcome = load_or_fetch_cover(&client, &req);
        let _ = tx.send(WorkerMessage::CoverLoaded(outcome));
        ctx.request_repaint();
    });
}

/// Parameters describing an asynchronous patch execution job triggered from Patch Advisor.
pub struct ApplyFixesJob {
    /// Library entry ID being patched.
    pub entry_id: String,
    /// Full declarative compatibility recipe.
    pub recipe: GameRecipe,
    /// Target release identifier.
    pub release_id: String,
    /// Local installation root directory.
    pub install_dir: PathBuf,
    /// Selected fix IDs approved by the user.
    pub selected_fix_ids: Vec<String>,
    /// Snapshot of current launcher settings.
    pub settings: AppSettings,
}

/// Spawns transactional fix application and streams every [`PatchEvent`] live to the Patch Advisor modal.
pub fn spawn_apply_fixes(tx: Sender<WorkerMessage>, ctx: egui::Context, job: ApplyFixesJob) {
    thread::spawn(move || {
        let downloader = HttpDownloader::new();
        let mut registry = host_registry_backend();
        let mut patch_ctx = build_preview_context(&job.recipe, &job.install_dir, &job.settings);
        patch_ctx.recipe_dir = Some(job.settings.recipes_dir.clone());

        let result = {
            let mut engine = PatchEngine::new(&downloader, registry.as_mut());
            let mut sink = |event: PatchEvent| {
                let _ =
                    tx.send(WorkerMessage::PatchEvent { entry_id: job.entry_id.clone(), event });
                ctx.request_repaint();
            };
            engine
                .apply_fixes(
                    &job.recipe,
                    &job.release_id,
                    &job.install_dir,
                    &job.selected_fix_ids,
                    &patch_ctx,
                    &mut sink,
                )
                .map_err(|err| err.to_string())
        };

        let _ = tx.send(WorkerMessage::PatchFinished { entry_id: job.entry_id, result });
        ctx.request_repaint();
    });
}

/// Spawns full transactional rollback (files + `HKCU` registry entries) while preserving `.ogsc_sandbox`.
pub fn spawn_rollback(
    tx: Sender<WorkerMessage>,
    ctx: egui::Context,
    entry_id: String,
    recipe: GameRecipe,
    install_dir: PathBuf,
) {
    thread::spawn(move || {
        let downloader = HttpDownloader::new();
        let mut registry = host_registry_backend();
        let result = {
            let mut engine = PatchEngine::new(&downloader, registry.as_mut());
            let mut sink = ogsc_patcher::NoopSink;
            engine
                .rollback_fixes(&recipe, &install_dir, None, &mut sink)
                .map_err(|err| err.to_string())
        };

        let remaining_applied_fix_ids =
            BackupManager::load_existing(&install_dir, &recipe.rollback.backup_directory)
                .map_or_else(|_| Vec::new(), |mgr| mgr.journal().fix_order.clone());

        let _ = tx.send(WorkerMessage::RollbackFinished {
            entry_id,
            remaining_applied_fix_ids,
            result,
        });
        ctx.request_repaint();
    });
}

/// Parameters for asynchronously monitoring a launched game process and capturing crash data.
pub struct MonitorGameJob {
    /// Human-readable game title.
    pub title: String,
    /// Resolved executable path, working directory, environment, and process constraints.
    pub cmd: ResolvedLaunchCommand,
    /// Spawned child process handle and elevation metadata.
    pub launched: LaunchedProcess,
}

/// Logs pre-launch PE/DRM/DLL/registry diagnostics, streams stdout/stderr, and queries Event Viewer on crash.
pub fn spawn_monitor_launched_game(
    tx: Sender<WorkerMessage>,
    ctx: egui::Context,
    mut job: MonitorGameJob,
) {
    thread::spawn(move || {
        let exe_name = job
            .cmd
            .program
            .file_name()
            .map_or_else(|| "game.exe".to_owned(), |n| n.to_string_lossy().into_owned());
        let (pe_summary, mods) = emit_prelaunch_diagnostics(&tx, &ctx, &job);

        if let Some(child) = job.launched.child.take() {
            monitor_child_process(
                &tx,
                &ctx,
                child,
                &job.title,
                &exe_name,
                job.launched.pid,
                pe_summary.as_ref(),
                &mods,
            );
        } else {
            monitor_elevated_process(
                &tx,
                &ctx,
                &job.title,
                &exe_name,
                job.launched.pid,
                pe_summary.as_ref(),
                &mods,
            );
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn monitor_child_process(
    tx: &Sender<WorkerMessage>,
    ctx: &egui::Context,
    mut child: std::process::Child,
    title: &str,
    exe_name: &str,
    pid: Option<u32>,
    pe_summary: Option<&PeSummary>,
    mods: &DetectedGameMods,
) {
    spawn_stdio_reader(child.stdout.take(), "STDOUT", DebugLogLevel::Stdout, tx, ctx);
    spawn_stdio_reader(child.stderr.take(), "STDERR", DebugLogLevel::Stderr, tx, ctx);

    let started = Instant::now();
    match child.wait() {
        Ok(status) => {
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let exit_code = status.code();
            let crashed = exit_code != Some(0);
            let exit_desc = exit_code.map_or_else(
                || "terminated by signal".to_owned(),
                |c| {
                    let hex = format_exit_code_hex(c);
                    match describe_exit_code(c) {
                        Some(meaning) => format!("{hex} ({c}) - {meaning}"),
                        None => format!("{hex} ({c})"),
                    }
                },
            );
            let level = if crashed { DebugLogLevel::Error } else { DebugLogLevel::Success };
            emit_log(
                tx,
                ctx,
                "EXIT",
                format!("Process exited after {} : {exit_desc}", format_duration_ms(elapsed_ms)),
                level,
            );

            if crashed {
                thread::sleep(Duration::from_millis(650));
                let lookback_ms = elapsed_ms.saturating_add(15_000).clamp(15_000, 120_000);
                let _ = emit_crash_analysis(
                    tx,
                    ctx,
                    exe_name,
                    pid,
                    exit_code,
                    pe_summary,
                    mods,
                    lookback_ms,
                );
            }

            let _ = tx.send(WorkerMessage::GameExited {
                title: title.to_owned(),
                pid,
                exit_code,
                elapsed_ms,
                crashed,
            });
            ctx.request_repaint();
        }
        Err(err) => {
            emit_log(
                tx,
                ctx,
                "EXIT",
                format!("Failed waiting for child process: {err}"),
                DebugLogLevel::Error,
            );
            let _ = tx.send(WorkerMessage::GameExited {
                title: title.to_owned(),
                pid,
                exit_code: None,
                elapsed_ms: 0,
                crashed: true,
            });
            ctx.request_repaint();
        }
    }
}

fn monitor_elevated_process(
    tx: &Sender<WorkerMessage>,
    ctx: &egui::Context,
    title: &str,
    exe_name: &str,
    pid: Option<u32>,
    pe_summary: Option<&PeSummary>,
    mods: &DetectedGameMods,
) {
    emit_log(
        tx,
        ctx,
        "ELEVATED",
        "Launched via UAC ShellExecuteExW (`runas`): stdout/stderr pipes and exit code wait are unavailable; watching Event Viewer for immediate startup crashes...",
        DebugLogLevel::Warning,
    );
    thread::sleep(Duration::from_millis(3_000));
    let found_crash_events =
        emit_crash_analysis(tx, ctx, exe_name, pid, None, pe_summary, mods, 15_000);
    let _ = tx.send(WorkerMessage::GameExited {
        title: title.to_owned(),
        pid,
        exit_code: None,
        elapsed_ms: 3_000,
        crashed: found_crash_events,
    });
    ctx.request_repaint();
}

fn emit_log(
    tx: &Sender<WorkerMessage>,
    ctx: &egui::Context,
    tag: &str,
    text: impl Into<String>,
    level: DebugLogLevel,
) {
    let _ = tx.send(WorkerMessage::DebugLog(DebugLogLine::now(tag, text, level)));
    ctx.request_repaint();
}

fn emit_prelaunch_diagnostics(
    tx: &Sender<WorkerMessage>,
    ctx: &egui::Context,
    job: &MonitorGameJob,
) -> (Option<PeSummary>, DetectedGameMods) {
    emit_log(tx, ctx, "LAUNCH", format!("=== Launching {} ===", job.title), DebugLogLevel::Info);
    emit_log(
        tx,
        ctx,
        "LAUNCH",
        format!("Executable: {}", job.cmd.program.display()),
        DebugLogLevel::Info,
    );
    emit_log(
        tx,
        ctx,
        "LAUNCH",
        format!("Working dir: {}", job.cmd.working_dir.display()),
        DebugLogLevel::Info,
    );
    let args_txt = if job.cmd.args.is_empty() { "none".to_owned() } else { job.cmd.args.join(" ") };
    emit_log(tx, ctx, "LAUNCH", format!("Arguments: {args_txt}"), DebugLogLevel::Info);

    if !job.cmd.env.is_empty() {
        let env_pairs: Vec<String> = job.cmd.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        emit_log(tx, ctx, "ENV", env_pairs.join("; "), DebugLogLevel::Info);
    }

    let aff_txt =
        job.cmd.cpu_affinity_mask.map_or_else(|| "default".to_owned(), |m| format!("0x{m:X}"));
    let prio_txt =
        job.cmd.process_priority.map_or_else(|| "Normal".to_owned(), |p| format!("{p:?}"));
    emit_log(
        tx,
        ctx,
        "PROC",
        format!(
            "PID={}, elevated={}, CPU affinity={aff_txt}, priority={prio_txt}",
            job.launched.pid.map_or_else(|| "unknown".to_owned(), |p| p.to_string()),
            job.launched.elevated
        ),
        DebugLogLevel::Info,
    );

    if let Some(note) = &job.launched.note {
        emit_log(tx, ctx, "PROC", note.clone(), DebugLogLevel::Warning);
    }

    emit_pe_and_mods_diagnostics(tx, ctx, &job.cmd.program)
}

fn emit_pe_and_mods_diagnostics(
    tx: &Sender<WorkerMessage>,
    ctx: &egui::Context,
    program: &std::path::Path,
) -> (Option<PeSummary>, DetectedGameMods) {
    let reg = host_registry_backend();
    let exe_str = program.to_string_lossy();
    match reg.read_string(RegistryHive::Hkcu, APP_COMPAT_LAYERS_KEY, &exe_str) {
        Ok(Some(val)) => {
            emit_log(
                tx,
                ctx,
                "REG",
                format!("AppCompatFlags\\Layers = `{val}`"),
                DebugLogLevel::Info,
            );
        }
        Ok(None) => {
            emit_log(tx, ctx, "REG", "AppCompatFlags\\Layers = (none)", DebugLogLevel::Info);
        }
        Err(err) => {
            emit_log(
                tx,
                ctx,
                "REG",
                format!("Registry check failed: {err}"),
                DebugLogLevel::Warning,
            );
        }
    }

    let pe_summary = match inspect_pe_summary(program) {
        Ok(pe) => {
            emit_log(
                tx,
                ctx,
                "PE",
                format!(
                    "{}, size={} bytes, entry=0x{:08X}, timestamp=0x{:08X}, sections=[{}]",
                    pe.architecture,
                    pe.file_size_bytes,
                    pe.entry_point_rva,
                    pe.time_date_stamp,
                    pe.sections.join(", ")
                ),
                DebugLogLevel::Info,
            );
            if !pe.drm_markers.is_empty() {
                emit_log(
                    tx,
                    ctx,
                    "DRM",
                    format!(
                        "Detected legacy DRM/packer section markers: [{}] (Windows 10/11 blocks secdrv.sys).",
                        pe.drm_markers.join(", ")
                    ),
                    DebugLogLevel::Warning,
                );
            }
            Some(pe)
        }
        Err(err) => {
            emit_log(
                tx,
                ctx,
                "PE",
                format!("PE header inspect skipped: {err}"),
                DebugLogLevel::Warning,
            );
            None
        }
    };

    let mods = inspect_game_directory_mods(program);
    let proxies_txt =
        if mods.proxy_dlls.is_empty() { "none".to_owned() } else { mods.proxy_dlls.join(", ") };
    let asi_txt =
        if mods.asi_plugins.is_empty() { "none".to_owned() } else { mods.asi_plugins.join(", ") };
    emit_log(
        tx,
        ctx,
        "MODS",
        format!("Proxy DLLs/configs: [{proxies_txt}]; scripts/*.asi: [{asi_txt}]"),
        DebugLogLevel::Info,
    );

    (pe_summary, mods)
}

fn spawn_stdio_reader<R: Read + Send + 'static>(
    pipe: Option<R>,
    tag: &'static str,
    level: DebugLogLevel,
    tx: &Sender<WorkerMessage>,
    ctx: &egui::Context,
) {
    let Some(stream) = pipe else {
        return;
    };
    let tx_clone = tx.clone();
    let ctx_clone = ctx.clone();
    thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines().map_while(Result::ok) {
            let trimmed = line.trim_end();
            if !trimmed.is_empty() {
                emit_log(&tx_clone, &ctx_clone, tag, trimmed.to_owned(), level);
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn emit_crash_analysis(
    tx: &Sender<WorkerMessage>,
    ctx: &egui::Context,
    exe_name: &str,
    pid: Option<u32>,
    exit_code: Option<i32>,
    pe_summary: Option<&PeSummary>,
    mods: &DetectedGameMods,
    lookback_ms: u64,
) -> bool {
    let events = match query_recent_windows_crash_events(exe_name, pid, lookback_ms) {
        Ok(evs) => {
            if evs.is_empty() {
                emit_log(
                    tx,
                    ctx,
                    "EVENTLOG",
                    format!("No Windows Event ID 1000/1005 crash entries matched `{exe_name}`."),
                    DebugLogLevel::Info,
                );
            } else {
                for ev in &evs {
                    let summary = if ev.event_id == 1000 {
                        format!(
                            "Event ID 1000: module={}, exception={}, offset={}, pid={}",
                            ev.faulting_module.as_deref().unwrap_or("unknown"),
                            ev.exception_code.as_deref().unwrap_or("unknown"),
                            ev.fault_offset.as_deref().unwrap_or("unknown"),
                            ev.process_id_hex.as_deref().unwrap_or("unknown"),
                        )
                    } else {
                        format!(
                            "Event ID 1005: program={}, error_value={}",
                            ev.faulting_app.as_deref().unwrap_or(exe_name),
                            ev.error_value.as_deref().unwrap_or("unknown"),
                        )
                    };
                    emit_log(tx, ctx, "EVENTLOG", summary, DebugLogLevel::Error);
                }
            }
            evs
        }
        Err(err) => {
            emit_log(
                tx,
                ctx,
                "EVENTLOG",
                format!("Event Viewer lookup failed: {err}"),
                DebugLogLevel::Warning,
            );
            Vec::new()
        }
    };

    for hint in diagnose_crash_hints(exit_code, pe_summary, &events, mods) {
        emit_log(tx, ctx, "HINT", hint, DebugLogLevel::Warning);
    }

    !events.is_empty()
}

fn format_duration_ms(elapsed_ms: u64) -> String {
    let secs = elapsed_ms / 1000;
    let tenths = (elapsed_ms % 1000) / 100;
    format!("{secs}.{tenths}s")
}

/// Creates a [`PatchContext`] configured for dry-run previews inside the Patch Advisor modal.
pub fn build_preview_context(
    recipe: &GameRecipe,
    install_dir: &std::path::Path,
    settings: &AppSettings,
) -> PatchContext {
    let mut ctx = PatchContext::for_game(
        install_dir,
        ToolkitStore::new(&settings.toolkit_dir),
        settings.cache_dir.clone(),
        recipe.sandbox.as_ref().map(|s| s.sandbox_root.as_str()),
    );
    ctx.recipe_dir = Some(settings.recipes_dir.clone());
    ctx
}

#[cfg(windows)]
pub(crate) fn host_registry_backend() -> Box<dyn RegistryBackend> {
    Box::new(ogsc_patcher::WindowsRegistryBackend)
}

#[cfg(not(windows))]
pub(crate) fn host_registry_backend() -> Box<dyn RegistryBackend> {
    Box::new(ogsc_patcher::MemoryRegistryBackend::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partitions_existing_and_missing_scan_folders() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let existing_dir = temp.path().join("present");
        std::fs::create_dir_all(&existing_dir).expect("create existing");
        let missing_dir = temp.path().join("removed_folder");

        let (existing, missing) =
            partition_existing_scan_folders(vec![existing_dir.clone(), missing_dir.clone()]);
        assert_eq!(existing, vec![existing_dir]);
        assert_eq!(missing, vec![missing_dir]);
    }
}
