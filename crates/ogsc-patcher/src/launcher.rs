//! Game and emulator launch resolution, sandbox environment injection, and Windows affinity/priority launch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use ogsc_recipes::{AffinityApplyMethod, EmulatorKind, GameRecipe, PatchAction, ProcessPriority};
use ogsc_wrappers::ToolkitStore;

use crate::archive::{join_abs_rel, normalize_safe_relative};
use crate::error::PatcherError;
use crate::win32::apply_process_affinity_and_priority;

/// Metadata and process handle returned after successfully spawning a game or emulator.
#[derive(Debug)]
pub struct LaunchedProcess {
    /// OS process identifier, when reported by Windows/the host OS.
    pub pid: Option<u32>,
    /// Whether the target required UAC elevation (`ShellExecuteExW` `"runas"` fallback).
    pub elevated: bool,
    /// Optional informational note (e.g., when elevation prevented standard-user affinity/env injection).
    pub note: Option<String>,
    /// Owned standard child process handle (`None` when spawned elevated via the Windows shell).
    pub child: Option<Child>,
}

impl LaunchedProcess {
    /// Returns the OS process ID of the launched process, if available.
    pub fn id(&self) -> Option<u32> {
        self.pid
    }
}

/// Formats command-line arguments according to Windows `CommandLineToArgvW` quoting rules.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn format_windows_args(args: &[String]) -> String {
    let mut out = String::new();
    for (idx, arg) in args.iter().enumerate() {
        if idx > 0 {
            out.push(' ');
        }
        if !arg.is_empty() && !arg.chars().any(|c| matches!(c, ' ' | '\t' | '"')) {
            out.push_str(arg);
            continue;
        }
        out.push('"');
        let mut backslashes = 0_usize;
        for ch in arg.chars() {
            match ch {
                '\\' => backslashes += 1,
                '"' => {
                    for _ in 0..=(backslashes * 2) {
                        out.push('\\');
                    }
                    backslashes = 0;
                    out.push('"');
                }
                other => {
                    for _ in 0..backslashes {
                        out.push('\\');
                    }
                    backslashes = 0;
                    out.push(other);
                }
            }
        }
        for _ in 0..(backslashes * 2) {
            out.push('\\');
        }
        out.push('"');
    }
    out
}

/// Parameters describing how to launch a scanned or patched game installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOptions {
    /// Root directory of the local game installation.
    pub game_root: PathBuf,
    /// Matched recipe `release_id`.
    pub release_id: String,
    /// Fix IDs currently applied to this installation (used to derive `LimitCpuAffinity` policies).
    pub applied_fix_ids: Vec<String>,
    /// Extra command-line arguments appended after the recipe's configured launch arguments.
    pub extra_args: Vec<String>,
}

/// Fully resolved spawn command, working directory, environment, and Windows process constraints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLaunchCommand {
    /// Target executable or emulator binary path.
    pub program: PathBuf,
    /// Command-line arguments passed to `program`.
    pub args: Vec<String>,
    /// Working directory for the spawned child process.
    pub working_dir: PathBuf,
    /// Environment variables merged from sandbox redirection and recipe `launch.environment`.
    pub env: BTreeMap<String, String>,
    /// Optional Windows `SetProcessAffinityMask` bitmask.
    pub cpu_affinity_mask: Option<u64>,
    /// Optional Windows process priority class.
    pub process_priority: Option<ProcessPriority>,
}

/// Resolves the exact executable/emulator invocation, sandbox environment, and CPU affinity mask for launching.
pub fn resolve_launch_command(
    recipe: &GameRecipe,
    toolkit_store: &ToolkitStore,
    opts: &LaunchOptions,
) -> Result<ResolvedLaunchCommand, PatcherError> {
    let working_dir = match &recipe.launch.working_directory {
        Some(wd) => {
            let norm = normalize_safe_relative(wd, true)?;
            join_abs_rel(&opts.game_root, &norm)
        }
        None => opts.game_root.clone(),
    };

    let (program, mut args, final_working_dir) = if let Some(hook) = &recipe.launch.emulator_hook {
        let emu_exe = match hook.emulator {
            EmulatorKind::DosBoxX => toolkit_store.dosbox_x_executable(),
            EmulatorKind::Box86 => toolkit_store.box86_executable(),
        };
        let cfg_rel = normalize_safe_relative(&hook.config_file, false)?;
        let cfg_abs = join_abs_rel(&opts.game_root, &cfg_rel);
        let flag = match hook.emulator {
            EmulatorKind::DosBoxX => "-conf",
            EmulatorKind::Box86 => "--config",
        };
        let mut emu_args = vec![flag.to_owned(), cfg_abs.to_string_lossy().into_owned()];
        emu_args.extend(recipe.launch.arguments.iter().cloned());
        emu_args.extend(opts.extra_args.iter().cloned());
        (emu_exe, emu_args, working_dir)
    } else {
        let exe_rel = normalize_safe_relative(
            recipe.executable_for_launch(&opts.release_id, &opts.applied_fix_ids),
            false,
        )?;
        let exe_abs = join_abs_rel(&opts.game_root, &exe_rel);
        let wd = if recipe.launch.working_directory.is_some() {
            working_dir
        } else {
            exe_abs.parent().map_or_else(|| opts.game_root.clone(), Path::to_path_buf)
        };
        let mut game_args = recipe.launch.arguments.clone();
        game_args.extend(opts.extra_args.iter().cloned());
        (exe_abs, game_args, wd)
    };

    let mut env = BTreeMap::new();
    if let Some(sb) = &recipe.sandbox {
        if sb.enabled_by_default {
            let sb_rel = normalize_safe_relative(&sb.sandbox_root, false)?;
            let sb_abs = join_abs_rel(&opts.game_root, &sb_rel).to_string_lossy().into_owned();
            if sb.redirect_documents {
                env.insert("USERPROFILE".to_owned(), sb_abs.clone());
                env.insert("HOME".to_owned(), sb_abs.clone());
            }
            if sb.redirect_appdata_local {
                env.insert("LOCALAPPDATA".to_owned(), sb_abs.clone());
            }
            if sb.redirect_appdata_roaming {
                env.insert("APPDATA".to_owned(), sb_abs);
            }
        }
    }
    for (k, v) in &recipe.launch.environment {
        env.insert(k.clone(), v.clone());
    }

    let available_cores =
        std::thread::available_parallelism().map_or(8, |n| u32::try_from(n.get()).unwrap_or(8));
    let mut cpu_affinity_mask = None;
    let mut process_priority = None;

    for step in recipe.steps_for_selected_fixes(&opts.release_id, &opts.applied_fix_ids) {
        if let PatchAction::LimitCpuAffinity {
            mode,
            apply_method: AffinityApplyMethod::LauncherProcessMask,
            process_priority: prio,
        } = &step.action
        {
            cpu_affinity_mask = Some(mode.to_affinity_mask(available_cores));
            if let Some(p) = prio {
                process_priority = Some(*p);
            }
        }
    }

    if args.is_empty() && !opts.extra_args.is_empty() {
        args.extend(opts.extra_args.iter().cloned());
    }

    Ok(ResolvedLaunchCommand {
        program,
        args,
        working_dir: final_working_dir,
        env,
        cpu_affinity_mask,
        process_priority,
    })
}

/// Spawns `cmd.program` with its configured working directory, environment, CPU affinity, and priority.
pub fn launch_game(cmd: &ResolvedLaunchCommand) -> Result<LaunchedProcess, PatcherError> {
    launch_game_internal(cmd, false)
}

/// Spawns `cmd.program` with stdout and stderr piped so callers (such as a debug console) can stream output.
pub fn launch_game_captured(cmd: &ResolvedLaunchCommand) -> Result<LaunchedProcess, PatcherError> {
    launch_game_internal(cmd, true)
}

fn launch_game_internal(
    cmd: &ResolvedLaunchCommand,
    capture_stdio: bool,
) -> Result<LaunchedProcess, PatcherError> {
    if !cmd.program.is_file() {
        return Err(PatcherError::LaunchFailed {
            program: cmd.program.clone(),
            reason: "target executable or emulator binary does not exist on disk".to_owned(),
        });
    }

    let mut command = Command::new(&cmd.program);
    command.args(&cmd.args).current_dir(&cmd.working_dir).envs(&cmd.env);
    if capture_stdio {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    }

    match command.spawn() {
        Ok(child) => {
            apply_process_affinity_and_priority(
                &child,
                cmd.cpu_affinity_mask,
                cmd.process_priority,
            )?;
            let pid = Some(child.id());
            Ok(LaunchedProcess { pid, elevated: false, note: None, child: Some(child) })
        }
        Err(err) => {
            #[cfg(windows)]
            if err.raw_os_error() == Some(740) {
                return crate::win32::launch_elevated_via_shell(cmd);
            }
            Err(PatcherError::LaunchFailed {
                program: cmd.program.clone(),
                reason: err.to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use ogsc_recipes::{
        BinaryArchitecture, CpuAffinityMode, Distribution, EmulatorLaunchHook,
        ExecutableFingerprint, FixOption, GameRelease, HashVerificationStatus, HealthStatus,
        LaunchConfig, PatchStep, RecipeMetadata, RollbackPlan, RollbackStrategy,
        RuntimeRequirements, SandboxConfig, CURRENT_SCHEMA_VERSION,
    };

    use super::*;

    fn sample_recipe(emulator: Option<EmulatorLaunchHook>) -> GameRecipe {
        GameRecipe {
            schema_version: CURRENT_SCHEMA_VERSION,
            game_id: "sample".to_owned(),
            title: "Sample".to_owned(),
            release_year: 2005,
            developer: None,
            publisher: None,
            metadata: RecipeMetadata {
                recipe_version: "1.0.0".to_owned(),
                authors: vec!["Tester".to_owned()],
                license: "MIT".to_owned(),
                updated_at: "2026-11-22".to_owned(),
                pcgamingwiki_url: None,
                steam_app_id: None,
                steamgriddb_id: None,
                cover_art_url: None,
                required_game_version: None,
                incompatible_releases: Vec::new(),
                notes: None,
            },
            baseline_health: HealthStatus::NeedsWrapper,
            post_fix_health: HealthStatus::ReadyToPlay,
            releases: vec![GameRelease {
                release_id: "retail".to_owned(),
                label: "Retail".to_owned(),
                distribution: Distribution::RetailCd,
                version: "1.0".to_owned(),
                region: None,
                executable_path: "bin/game.exe".to_owned(),
                fingerprint: ExecutableFingerprint {
                    file_size_bytes: Some(1234),
                    partial_sha256: None,
                    sha256: Some(std::iter::repeat_n('1', 64).collect()),
                    md5: None,
                    verification_status: HashVerificationStatus::Verified,
                },
                sentinel_files: Vec::new(),
            }],
            known_issues: Vec::new(),
            runtime_requirements: RuntimeRequirements {
                architecture: BinaryArchitecture::Win32,
                requires_16bit_emulator: emulator.is_some(),
                native_graphics_api: vec!["Direct3D 9".to_owned()],
                target_graphics_api: Some("Direct3D 11".to_owned()),
                requires_admin: false,
            },
            sandbox: Some(SandboxConfig {
                enabled_by_default: true,
                sandbox_root: ".ogsc_sandbox".to_owned(),
                redirect_documents: true,
                redirect_appdata_local: true,
                redirect_appdata_roaming: false,
                known_save_subpaths: vec!["saves".to_owned()],
            }),
            fixes: vec![FixOption {
                id: "affinity_fix".to_owned(),
                title: "Affinity".to_owned(),
                description: "Single core".to_owned(),
                recommended: true,
                applies_to_releases: Vec::new(),
                steps: vec![PatchStep {
                    id: "limit-core".to_owned(),
                    name: "Limit core".to_owned(),
                    description: "Pin to core 0".to_owned(),
                    applies_to_releases: Vec::new(),
                    os_filter: None,
                    action: PatchAction::LimitCpuAffinity {
                        mode: CpuAffinityMode::SingleCore,
                        apply_method: AffinityApplyMethod::LauncherProcessMask,
                        process_priority: Some(ProcessPriority::AboveNormal),
                    },
                }],
            }],
            launch: LaunchConfig {
                default_executable: "bin/game.exe".to_owned(),
                release_executable_overrides: BTreeMap::new(),
                fix_executable_overrides: BTreeMap::new(),
                working_directory: None,
                arguments: vec!["-windowed".to_owned()],
                environment: BTreeMap::from([("OGSC_ENV".to_owned(), "1".to_owned())]),
                emulator_hook: emulator,
            },
            rollback: RollbackPlan {
                strategy: RollbackStrategy::TransactionalAutomatic,
                backup_directory: ".ogsc_backup".to_owned(),
                preserve_user_saves: true,
                restore_registry_values: true,
                extra_cleanup_paths: Vec::new(),
                custom_actions: Vec::new(),
            },
        }
    }

    #[test]
    fn resolves_native_affinity_sandbox_and_emulator_hooks() {
        let store = ToolkitStore::new(r"C:\Wrappers");
        let game_root = PathBuf::from(r"C:\Games\Sample");

        let native_recipe = sample_recipe(None);
        let cmd = resolve_launch_command(
            &native_recipe,
            &store,
            &LaunchOptions {
                game_root: game_root.clone(),
                release_id: "retail".to_owned(),
                applied_fix_ids: vec!["affinity_fix".to_owned()],
                extra_args: vec!["-nomovies".to_owned()],
            },
        )
        .expect("resolve native");

        assert_eq!(cmd.program, game_root.join("bin").join("game.exe"));
        assert_eq!(cmd.working_dir, game_root.join("bin"));
        assert_eq!(cmd.args, vec!["-windowed", "-nomovies"]);
        assert_eq!(cmd.cpu_affinity_mask, Some(1));
        assert_eq!(cmd.process_priority, Some(ProcessPriority::AboveNormal));
        assert_eq!(
            cmd.env.get("USERPROFILE").map(String::as_str),
            Some(game_root.join(".ogsc_sandbox").to_string_lossy().as_ref())
        );
        assert_eq!(cmd.env.get("OGSC_ENV").map(String::as_str), Some("1"));

        let emu_recipe = sample_recipe(Some(EmulatorLaunchHook {
            emulator: EmulatorKind::DosBoxX,
            config_file: "dosbox-x.conf".to_owned(),
            disk_image_hint: None,
        }));
        let emu_cmd = resolve_launch_command(
            &emu_recipe,
            &store,
            &LaunchOptions {
                game_root: game_root.clone(),
                release_id: "retail".to_owned(),
                applied_fix_ids: Vec::new(),
                extra_args: Vec::new(),
            },
        )
        .expect("resolve dosbox-x");

        assert_eq!(emu_cmd.program, store.dosbox_x_executable());
        assert_eq!(emu_cmd.args[0], "-conf");
        assert_eq!(emu_cmd.args[1], game_root.join("dosbox-x.conf").to_string_lossy());
        assert_eq!(emu_cmd.cpu_affinity_mask, None);
    }

    #[test]
    fn resolves_fix_executable_override_when_fix_applied() {
        let store = ToolkitStore::new(r"C:\Wrappers");
        let game_root = PathBuf::from(r"C:\Games\Sample");
        let mut recipe = sample_recipe(None);
        recipe
            .launch
            .fix_executable_overrides
            .insert("affinity_fix".to_owned(), "bin/RecapLauncher.exe".to_owned());

        let with_fix = resolve_launch_command(
            &recipe,
            &store,
            &LaunchOptions {
                game_root: game_root.clone(),
                release_id: "retail".to_owned(),
                applied_fix_ids: vec!["affinity_fix".to_owned()],
                extra_args: Vec::new(),
            },
        )
        .expect("resolve override");
        assert_eq!(with_fix.program, game_root.join("bin").join("RecapLauncher.exe"));

        let without_fix = resolve_launch_command(
            &recipe,
            &store,
            &LaunchOptions {
                game_root: game_root.clone(),
                release_id: "retail".to_owned(),
                applied_fix_ids: Vec::new(),
                extra_args: Vec::new(),
            },
        )
        .expect("resolve fallback");
        assert_eq!(without_fix.program, game_root.join("bin").join("game.exe"));
    }

    #[test]
    fn formats_windows_command_line_arguments_safely() {
        let args = vec![
            "-title".to_owned(),
            "Barbie Diaries".to_owned(),
            r"C:\Program Files\Game\".to_owned(),
            r#"say "hi"\"#.to_owned(),
            String::new(),
        ];
        assert_eq!(
            format_windows_args(&args),
            r#"-title "Barbie Diaries" "C:\Program Files\Game\\" "say \"hi\"\\" """#
        );
    }
}
