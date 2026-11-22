//! Developer and diagnostic CLI for **ReviveBox** (*Old Games Second Chance*).

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ogsc_patcher::{
    launch_game, plan_fixes, registry::revert_registry_changes, resolve_launch_command,
    HttpDownloader, LaunchOptions, PatchContext, PatchEngine, PatchEvent, PatchPlan,
    RegistryBackend,
};
use ogsc_recipes::{
    GameRecipe, PatchAction, RecipeDatabase, RecipeWarningRecord, ValidationIssue, WrapperKind,
};
use ogsc_scanner::{
    compute_fingerprint, BackupManager, GameScanner, LibraryStore, ScanOptions,
    DEFAULT_PARTIAL_HASH_BYTES,
};
use ogsc_wrappers::ToolkitStore;

#[derive(Debug, Parser)]
#[command(
    name = "ogsc",
    version,
    about = "ReviveBox (Old Games Second Chance) — recipe validator, game scanner, patcher, launcher, and backup tool"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate a single `.json` recipe file or a directory of recipes.
    Validate {
        /// File or directory path containing `.json` recipe manifests.
        #[arg(default_value = "recipes/games")]
        path: PathBuf,
        /// Treat warnings (such as `unverified_template` hashes or HKLM writes) as failures.
        #[arg(long)]
        strict: bool,
    },

    /// Display a human-readable summary of a single `.json` recipe manifest.
    Inspect {
        /// Path to the recipe `.json` file.
        path: PathBuf,
    },

    /// Export the Draft 2020-12 JSON Schema for `GameRecipe`.
    ExportSchema {
        /// Optional destination path; writes to stdout when omitted.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Compute size, partial SHA-256, full SHA-256, and MD5 fingerprint for an executable file.
    Hash {
        /// Path to the local `.exe` file to fingerprint.
        path: PathBuf,
        /// Byte count from offset 0 used for `partial_sha256` (defaults to 65536).
        #[arg(long, default_value_t = DEFAULT_PARTIAL_HASH_BYTES)]
        partial_bytes: u64,
    },

    /// Scan one or more game folders against the local recipe database.
    Scan {
        /// Directory roots to scan recursively.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Directory containing `.json` recipe manifests.
        #[arg(long, default_value = "recipes/games")]
        recipes: PathBuf,
        /// Optional `library.json` path to persist matched games and register scan roots.
        #[arg(long)]
        library: Option<PathBuf>,
        /// Disable heuristic filename + sentinel matches (require cryptographic/size hash match).
        #[arg(long)]
        no_heuristics: bool,
    },

    /// Inspect or update persisted `library.json` game status and fix selections.
    Library {
        /// Path to `library.json` (defaults to per-user LocalAppData location).
        #[arg(long, global = true)]
        library: Option<PathBuf>,
        #[command(subcommand)]
        action: LibraryCommand,
    },

    /// Inspect or roll back transactional backups stored in `<game_root>/.ogsc_backup`.
    Backup {
        #[command(subcommand)]
        action: BackupCommand,
    },

    /// Preview (`--dry-run`) or transactionally apply selected recipe fix groups to a game directory.
    Apply {
        /// Path to the recipe `.json` file.
        recipe: PathBuf,
        /// Game installation root directory.
        #[arg(long)]
        game_root: PathBuf,
        /// Target release ID (defaults to the first declared release in the recipe).
        #[arg(long)]
        release: Option<String>,
        /// Fix IDs to apply (repeatable; defaults to all `recommended` fixes for the release).
        #[arg(long = "fix")]
        fixes: Vec<String>,
        /// Custom `runtime_wrappers` root directory.
        #[arg(long)]
        toolkit_dir: Option<PathBuf>,
        /// Custom verified download cache directory.
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        /// Optional `library.json` path to update once fixes succeed.
        #[arg(long)]
        library: Option<PathBuf>,
        /// Optional `LibraryEntry::entry_id` to mark `Patched`.
        #[arg(long)]
        entry: Option<String>,
        /// Preview every file, wrapper, download, and HKCU registry change without modifying disk.
        #[arg(long)]
        dry_run: bool,
    },

    /// Resolve or launch a game/emulator with sandbox environment and Windows CPU affinity applied.
    Launch {
        /// Path to the recipe `.json` file.
        recipe: PathBuf,
        /// Game installation root directory.
        #[arg(long)]
        game_root: PathBuf,
        /// Target release ID (defaults to the first declared release in the recipe).
        #[arg(long)]
        release: Option<String>,
        /// Applied fix IDs used to resolve launch-time CPU affinity/priority rules.
        #[arg(long = "applied-fix")]
        applied_fixes: Vec<String>,
        /// Custom `runtime_wrappers` root directory.
        #[arg(long)]
        toolkit_dir: Option<PathBuf>,
        /// Extra command-line arguments appended at launch.
        #[arg(long = "arg")]
        extra_args: Vec<String>,
        /// Print resolved command, environment, and CPU affinity mask without spawning.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
enum LibraryCommand {
    /// List all imported games and saved scan folders in `library.json`.
    List,
    /// Record applied/declined fix IDs or mark an entry as DeclinedVanilla.
    SetDecision {
        /// Deterministic `LibraryEntry::entry_id`.
        entry_id: String,
        /// Fix IDs applied to this installation.
        #[arg(long = "applied")]
        applied_fix_ids: Vec<String>,
        /// Fix IDs explicitly declined by the user.
        #[arg(long = "declined")]
        declined_fix_ids: Vec<String>,
        /// Mark the game as `DeclinedVanilla` (clears applied fixes).
        #[arg(long)]
        vanilla: bool,
    },
}

#[derive(Debug, Subcommand)]
enum BackupCommand {
    /// Display backed-up vanilla files and active fix transactions in `journal.json`.
    Status {
        /// Game installation root directory.
        #[arg(long)]
        game_root: PathBuf,
        /// Relative backup directory name under `game_root`.
        #[arg(long, default_value = ".ogsc_backup")]
        backup_dir: String,
    },
    /// Revert a specific fix (or all active fixes) using verified content-addressed blobs.
    Rollback {
        /// Game installation root directory.
        #[arg(long)]
        game_root: PathBuf,
        /// Relative backup directory name under `game_root`.
        #[arg(long, default_value = ".ogsc_backup")]
        backup_dir: String,
        /// Optional single `fix_id` to revert (reverts all fixes in reverse order when omitted).
        #[arg(long)]
        fix: Option<String>,
        /// Relative paths (such as `.ogsc_sandbox`) to preserve untouched during rollback.
        #[arg(long)]
        preserve: Vec<String>,
        /// Do not automatically add `.ogsc_sandbox` to the preserved paths list.
        #[arg(long)]
        no_default_preserve: bool,
        /// Also revert recorded Windows Registry `AppCompatFlags\Layers` changes.
        #[arg(long)]
        revert_registry: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Validate { path, strict } => cmd_validate(&path, strict),
        Command::Inspect { path } => cmd_inspect(&path),
        Command::ExportSchema { output } => cmd_export_schema(output.as_deref()),
        Command::Hash { path, partial_bytes } => cmd_hash(&path, partial_bytes),
        Command::Scan { paths, recipes, library, no_heuristics } => {
            cmd_scan(&paths, &recipes, library.as_deref(), no_heuristics)
        }
        Command::Library { library, action } => cmd_library(library.as_deref(), action),
        Command::Backup { action } => cmd_backup(action),
        Command::Apply {
            recipe,
            game_root,
            release,
            fixes,
            toolkit_dir,
            cache_dir,
            library,
            entry,
            dry_run,
        } => cmd_apply(
            &recipe,
            &game_root,
            release.as_deref(),
            fixes,
            toolkit_dir,
            cache_dir,
            library.as_deref(),
            entry.as_deref(),
            dry_run,
        ),
        Command::Launch {
            recipe,
            game_root,
            release,
            applied_fixes,
            toolkit_dir,
            extra_args,
            dry_run,
        } => cmd_launch(
            &recipe,
            &game_root,
            release.as_deref(),
            applied_fixes,
            toolkit_dir,
            extra_args,
            dry_run,
        ),
    }
}

fn cmd_validate(path: &Path, strict: bool) -> Result<()> {
    if path.is_file() {
        let (recipe, warnings) = RecipeDatabase::load_recipe_file(path, strict)?;
        if !warnings.is_empty() {
            print_issue_list(&recipe.game_id, Some(path), &warnings);
        }
        println!(
            "OK: `{}` ({}) validated ({} warning(s))",
            recipe.game_id,
            recipe.title,
            warnings.len()
        );
        return Ok(());
    }

    let loaded = RecipeDatabase::load_from_dir(path, strict)?;
    let warning_count: usize = loaded.warnings.iter().map(|r| r.warnings.len()).sum();

    for record in &loaded.warnings {
        print_warning_record(record);
    }

    println!(
        "OK: validated {} recipe(s) in `{}` ({} warning(s))",
        loaded.db.len(),
        path.display(),
        warning_count
    );
    Ok(())
}

fn print_warning_record(record: &RecipeWarningRecord) {
    print_issue_list(&record.game_id, record.source_path.as_deref(), &record.warnings);
}

fn print_issue_list(game_id: &str, path: Option<&Path>, issues: &[ValidationIssue]) {
    match path {
        Some(p) => println!("Warnings for `{game_id}` ({}):", p.display()),
        None => println!("Warnings for `{game_id}`:"),
    }
    for issue in issues {
        println!("  [WARN] {}: {} ({:?})", issue.field, issue.message, issue.code);
    }
}

fn cmd_inspect(path: &Path) -> Result<()> {
    let (recipe, warnings) = RecipeDatabase::load_recipe_file(path, false)?;
    print_recipe_summary(&recipe);
    if !warnings.is_empty() {
        println!();
        print_issue_list(&recipe.game_id, Some(path), &warnings);
    }
    Ok(())
}

fn print_recipe_summary(recipe: &GameRecipe) {
    println!("Title:          {} ({})", recipe.title, recipe.release_year);
    println!("Game ID:        {}", recipe.game_id);
    println!(
        "Schema / Ver:   v{} / recipe {}",
        recipe.schema_version, recipe.metadata.recipe_version
    );
    println!(
        "Studio:         {} / {}",
        recipe.developer.as_deref().unwrap_or("Unknown"),
        recipe.publisher.as_deref().unwrap_or("Unknown")
    );
    println!("Health:         {:?} -> {:?}", recipe.baseline_health, recipe.post_fix_health);
    println!(
        "Runtime:        {:?}, emulator_required={}, native={:?}, target={}",
        recipe.runtime_requirements.architecture,
        recipe.runtime_requirements.requires_16bit_emulator,
        recipe.runtime_requirements.native_graphics_api,
        recipe.runtime_requirements.target_graphics_api.as_deref().unwrap_or("Native")
    );

    if let Some(sb) = &recipe.sandbox {
        println!(
            "Sandbox:        enabled_by_default={}, root=`{}`, saves={:?}",
            sb.enabled_by_default, sb.sandbox_root, sb.known_save_subpaths
        );
    }

    println!("\nReleases ({}):", recipe.releases.len());
    for rel in &recipe.releases {
        let sha_preview =
            rel.fingerprint.sha256.as_deref().map_or("<none>", |h| &h[..h.len().min(12)]);
        println!(
            "  - {} [{} / {:?}] exe=`{}` size={:?} sha256={}... ({:?})",
            rel.release_id,
            rel.label,
            rel.distribution,
            rel.executable_path,
            rel.fingerprint.file_size_bytes,
            sha_preview,
            rel.fingerprint.verification_status
        );
    }

    println!("\nKnown Issues ({}):", recipe.known_issues.len());
    for issue in &recipe.known_issues {
        println!(
            "  - [ {:?} | {:?} ] {}: {}",
            issue.severity, issue.category, issue.id, issue.summary
        );
        if !issue.fixed_by_fixes.is_empty() {
            println!("      fixed_by_fixes: {}", issue.fixed_by_fixes.join(", "));
        }
        if let Some(action) = &issue.user_action_required {
            println!("      user_action_required: {action}");
        }
    }

    println!("\nSelectable Fix Groups ({}):", recipe.fixes.len());
    for (f_idx, fix) in recipe.fixes.iter().enumerate() {
        let checked = if fix.recommended { "x" } else { " " };
        let badge = if fix.recommended { "recommended" } else { "optional" };
        println!("  {}. [{checked}] {} ({}) [{badge}]", f_idx + 1, fix.title, fix.id);
        println!("      {}", fix.description);
        for (s_idx, step) in fix.steps.iter().enumerate() {
            println!(
                "        {}.{}. {} ({}) -> {}",
                f_idx + 1,
                s_idx + 1,
                step.name,
                step.id,
                describe_action(&step.action)
            );
        }
    }

    println!(
        "\nLaunch:         `{}` (args: {:?})",
        recipe.launch.default_executable, recipe.launch.arguments
    );
    println!(
        "Rollback:       {:?} (backup_dir=`{}`, preserve_saves={}, restore_registry={})",
        recipe.rollback.strategy,
        recipe.rollback.backup_directory,
        recipe.rollback.preserve_user_saves,
        recipe.rollback.restore_registry_values
    );
}

fn describe_action(action: &PatchAction) -> String {
    match action {
        PatchAction::DownloadVerifiedZip { url, extract_to, .. } => {
            format!("download_verified_zip `{url}` -> `{extract_to}`")
        }
        PatchAction::ExtractDll { destination_path, .. } => {
            format!("extract_dll -> `{destination_path}`")
        }
        PatchAction::DeployWrapper(dep) => {
            let kind = match &dep.wrapper {
                WrapperKind::DgVoodoo2 { .. } => "dgvoodoo2",
                WrapperKind::DxWrapper { .. } => "dxwrapper",
                WrapperKind::WineD3d { .. } => "wined3d",
                WrapperKind::XInputPlus { .. } => "xinput_plus",
                WrapperKind::DosBoxX { .. } => "dosbox_x",
                WrapperKind::Box86 { .. } => "86box",
            };
            format!("deploy_wrapper `{kind}` -> `{}`", dep.target_dir)
        }
        PatchAction::ApplyBinaryBsdiffPatch { target_file, .. } => {
            format!("apply_binary_bsdiff_patch `{target_file}`")
        }
        PatchAction::SetRegistryCompatFlags { hive, target_executable, flags, .. } => {
            let tokens: Vec<&str> =
                flags.iter().map(ogsc_recipes::AppCompatFlag::to_windows_token).collect();
            format!(
                "set_registry_compat_flags {:?} `{target_executable}` [{}]",
                hive,
                tokens.join(" ")
            )
        }
        PatchAction::WriteIniConfig { target_file, sections, .. } => {
            format!("write_ini_config `{target_file}` ({} section(s))", sections.len())
        }
        PatchAction::LimitCpuAffinity { mode, apply_method, .. } => {
            format!("limit_cpu_affinity {mode:?} via {apply_method:?}")
        }
        PatchAction::ConfigureSandbox { create_directories, .. } => {
            format!("configure_sandbox ({} dir(s))", create_directories.len())
        }
    }
}

fn cmd_export_schema(output: Option<&Path>) -> Result<()> {
    let json = ogsc_recipes::generate_json_schema_pretty()?;
    if let Some(dest) = output {
        if let Some(parent) = dest.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create schema output directory `{}`", parent.display())
                })?;
            }
        }
        fs::write(dest, json)
            .with_context(|| format!("failed to write JSON schema to `{}`", dest.display()))?;
        println!("Wrote Draft 2020-12 JSON Schema to `{}`", dest.display());
    } else {
        print!("{json}");
    }
    Ok(())
}

fn cmd_hash(path: &Path, partial_bytes: u64) -> Result<()> {
    let fp = compute_fingerprint(path, partial_bytes)?;
    println!("File:           {}", path.display());
    println!("Size (bytes):   {}", fp.file_size_bytes);
    println!(
        "Partial SHA256: {} (offset={}, len={})",
        fp.partial_sha256, fp.partial_offset, fp.partial_length
    );
    println!("Full SHA-256:   {}", fp.sha256);
    println!("Full MD5:       {}", fp.md5);
    println!("\nRecipe ExecutableFingerprint JSON snippet:");
    println!(
        "{{\n  \"file_size_bytes\": {},\n  \"partial_sha256\": {{\n    \"offset\": {},\n    \"length\": {},\n    \"sha256\": \"{}\"\n  }},\n  \"sha256\": \"{}\",\n  \"md5\": \"{}\",\n  \"verification_status\": \"verified\"\n}}",
        fp.file_size_bytes,
        fp.partial_offset,
        fp.partial_length,
        fp.partial_sha256,
        fp.sha256,
        fp.md5
    );
    Ok(())
}

fn cmd_scan(
    paths: &[PathBuf],
    recipes_dir: &Path,
    library_path: Option<&Path>,
    no_heuristics: bool,
) -> Result<()> {
    let loaded = RecipeDatabase::load_from_dir(recipes_dir, false)?;
    let options =
        ScanOptions { include_heuristic_matches: !no_heuristics, ..ScanOptions::default() };
    let scanner = GameScanner::new(&loaded.db, options);
    let report = scanner.scan_roots(paths)?;

    println!(
        "Scanned {} file(s) ({} candidate executable(s)) in {} ms -> {} match(es)",
        report.files_inspected,
        report.candidate_executables,
        report.elapsed_ms,
        report.matches.len()
    );

    for m in &report.matches {
        println!(
            "\n  - {} ({}) [release: `{}` | confidence: {:?}]",
            m.title, m.game_id, m.release_id, m.confidence
        );
        println!("    Root: {}", m.game_root.display());
        println!("    Exe:  {}", m.executable_path.display());
        if !m.matched_sentinels.is_empty() {
            println!("    Sentinels matched: {}", m.matched_sentinels.join(", "));
        }
        if !m.missing_sentinels.is_empty() {
            println!("    Sentinels missing: {}", m.missing_sentinels.join(", "));
        }
    }

    if !report.warnings.is_empty() {
        println!("\nTraversal Warnings ({}):", report.warnings.len());
        for warn in &report.warnings {
            println!("  [WARN] {warn}");
        }
    }

    if let Some(lib_path) = library_path {
        let mut store = LibraryStore::open(lib_path)?;
        for root in &report.scanned_roots {
            store.add_scan_folder(root)?;
        }
        for m in &report.matches {
            store.upsert_scan_match(m);
        }
        store.save()?;
        println!(
            "\nSaved {} library entries to `{}`",
            store.library().entries.len(),
            lib_path.display()
        );
    }

    Ok(())
}

fn cmd_library(library_path: Option<&Path>, action: LibraryCommand) -> Result<()> {
    let path = library_path.map_or_else(LibraryStore::default_path, Path::to_path_buf);
    let mut store = LibraryStore::open(&path)?;

    match action {
        LibraryCommand::List => {
            println!(
                "Library: {} ({} entries)",
                store.path().display(),
                store.library().entries.len()
            );
            if !store.library().scan_folders.is_empty() {
                println!("Scan folders:");
                for dir in &store.library().scan_folders {
                    println!("  - {}", dir.display());
                }
            }
            for entry in store.entries() {
                println!("\n  - {} | {} ({:?})", entry.entry_id, entry.title, entry.status);
                println!("    Install: {}", entry.install_dir.display());
                println!("    Release: {} ({:?})", entry.release_id, entry.match_confidence);
                println!("    Applied: {:?}", entry.applied_fix_ids);
                println!("    Declined: {:?}", entry.declined_fix_ids);
            }
            Ok(())
        }
        LibraryCommand::SetDecision { entry_id, applied_fix_ids, declined_fix_ids, vanilla } => {
            let status = if vanilla {
                store.mark_declined_vanilla(&entry_id, declined_fix_ids)?.status
            } else {
                store.record_applied_fixes(&entry_id, applied_fix_ids, declined_fix_ids)?.status
            };
            store.save()?;
            println!("Updated `{entry_id}` -> {status:?} (saved to `{}`)", store.path().display());
            Ok(())
        }
    }
}

fn cmd_backup(action: BackupCommand) -> Result<()> {
    match action {
        BackupCommand::Status { game_root, backup_dir } => {
            let mgr = BackupManager::load_existing(&game_root, &backup_dir)?;
            let journal = mgr.journal();
            println!(
                "Backup Journal: game=`{}` release=`{}` (updated {})",
                journal.game_id, journal.release_id, journal.updated_at
            );
            println!("Original snapshots tracked: {}", journal.original_files.len());
            println!("Active fixes ({}):", journal.fix_order.len());
            for fix_id in &journal.fix_order {
                if let Some(tx) = journal.applied_fixes.get(fix_id) {
                    println!(
                        "  - {} (applied {}, {} files, {} dirs, {} registry edits)",
                        tx.fix_id,
                        tx.applied_at,
                        tx.touched_files.len(),
                        tx.created_directories.len(),
                        tx.registry_records.len()
                    );
                }
            }
            Ok(())
        }
        BackupCommand::Rollback {
            game_root,
            backup_dir,
            fix,
            mut preserve,
            no_default_preserve,
            revert_registry,
        } => {
            if !no_default_preserve
                && !preserve.iter().any(|p| p.eq_ignore_ascii_case(".ogsc_sandbox"))
            {
                preserve.push(".ogsc_sandbox".to_owned());
            }
            let mut mgr = BackupManager::load_existing(&game_root, &backup_dir)?;
            let summary = match fix.as_deref() {
                Some(fix_id) => mgr.rollback_fix(fix_id, &preserve)?,
                None => mgr.rollback_all(&preserve)?,
            };

            let mut reverted_reg = 0_usize;
            if revert_registry && !summary.registry_actions_to_revert.is_empty() {
                let mut reg = host_registry_backend();
                reverted_reg =
                    revert_registry_changes(reg.as_mut(), &summary.registry_actions_to_revert)?;
            }

            println!(
                "Rolled back {:?}: restored {} file(s), deleted {} file(s), removed {} empty dir(s), skipped {} preserved path(s), reverted {} registry entry(ies)",
                summary.rolled_back_fixes,
                summary.restored_files,
                summary.deleted_files,
                summary.removed_directories,
                summary.skipped_preserved_paths.len(),
                reverted_reg
            );
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_apply(
    recipe_path: &Path,
    game_root: &Path,
    release_opt: Option<&str>,
    fixes: Vec<String>,
    toolkit_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    library_path: Option<&Path>,
    entry_id: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    let (recipe, _) = RecipeDatabase::load_recipe_file(recipe_path, false)?;
    let release_id = release_opt.unwrap_or_else(|| recipe.releases[0].release_id.as_str());
    let selected = if fixes.is_empty() {
        recipe
            .fixes_for_release(release_id)
            .into_iter()
            .filter(|f| f.recommended)
            .map(|f| f.id.clone())
            .collect()
    } else {
        fixes
    };

    let store = ToolkitStore::new(toolkit_dir.unwrap_or_else(ToolkitStore::default_dir));
    let cache = cache_dir.unwrap_or_else(HttpDownloader::default_cache_dir);
    let mut ctx = PatchContext::for_game(
        game_root,
        store,
        cache,
        recipe.sandbox.as_ref().map(|s| s.sandbox_root.as_str()),
    );
    ctx.recipe_dir = recipe_path.parent().map(Path::to_path_buf);

    let mut reg = host_registry_backend();

    if dry_run {
        let plan = plan_fixes(&recipe, release_id, game_root, &selected, &ctx, reg.as_ref())?;
        print_patch_plan(&plan);
        return Ok(());
    }

    let downloader = HttpDownloader::new();
    let mut engine = PatchEngine::new(&downloader, reg.as_mut());
    let mut sink = |event: PatchEvent| print_patch_event(&event);
    let report = engine.apply_fixes(&recipe, release_id, game_root, &selected, &ctx, &mut sink)?;

    if let (Some(lib_path), Some(id)) = (library_path, entry_id) {
        let declined: Vec<String> = recipe
            .fixes_for_release(release_id)
            .into_iter()
            .filter(|f| !report.applied_fix_ids.contains(&f.id))
            .map(|f| f.id.clone())
            .collect();
        let mut lib = LibraryStore::open(lib_path)?;
        lib.record_applied_fixes(id, report.applied_fix_ids.clone(), declined)?;
        lib.save()?;
    }

    println!(
        "OK: applied {} fix group(s) ({:?}), skipped {} step(s)",
        report.applied_fix_ids.len(),
        report.applied_fix_ids,
        report.skipped_steps.len()
    );
    Ok(())
}

fn cmd_launch(
    recipe_path: &Path,
    game_root: &Path,
    release_opt: Option<&str>,
    applied_fixes: Vec<String>,
    toolkit_dir: Option<PathBuf>,
    extra_args: Vec<String>,
    dry_run: bool,
) -> Result<()> {
    let (recipe, _) = RecipeDatabase::load_recipe_file(recipe_path, false)?;
    let release_id = release_opt.unwrap_or_else(|| recipe.releases[0].release_id.as_str());
    let store = ToolkitStore::new(toolkit_dir.unwrap_or_else(ToolkitStore::default_dir));
    let cmd = resolve_launch_command(
        &recipe,
        &store,
        &LaunchOptions {
            game_root: game_root.to_path_buf(),
            release_id: release_id.to_owned(),
            applied_fix_ids: applied_fixes,
            extra_args,
        },
    )?;

    if dry_run {
        println!("Program:      {}", cmd.program.display());
        println!("Working dir:  {}", cmd.working_dir.display());
        println!("Args:         {:?}", cmd.args);
        println!("Env:          {:?}", cmd.env);
        println!("CPU affinity: {:?}", cmd.cpu_affinity_mask.map(|m| format!("0x{m:X}")));
        println!("Priority:     {:?}", cmd.process_priority);
        return Ok(());
    }

    let launched = launch_game(&cmd)?;
    let pid_text = launched.id().map_or_else(|| "unknown".to_owned(), |id| id.to_string());
    if launched.elevated {
        println!("Launched `{}` (PID {pid_text}, elevated via UAC)", cmd.program.display());
    } else {
        println!("Launched `{}` (PID {pid_text})", cmd.program.display());
    }
    if let Some(note) = &launched.note {
        println!("Note: {note}");
    }
    Ok(())
}

fn print_patch_plan(plan: &PatchPlan) {
    println!("Dry-Run Patch Plan for `{}` (release `{}`):", plan.game_id, plan.release_id);
    println!("\nSelected Fix Groups ({}):", plan.fixes.len());
    for fix in &plan.fixes {
        println!(
            "  - {} ({}) -> {} active step(s), {} skipped",
            fix.title, fix.fix_id, fix.active_step_count, fix.skipped_step_count
        );
    }

    println!("\nDownloads ({}):", plan.downloads.len());
    for dl in &plan.downloads {
        println!("  - [{}] {} (cached={})", dl.step_id, dl.url, dl.cached_locally);
    }

    println!("\nFile & Directory Changes ({}):", plan.file_changes.len());
    for ch in &plan.file_changes {
        println!(
            "  - [{:?}] `{}` (exists={}) -- {}",
            ch.kind, ch.relative_path, ch.already_exists, ch.detail
        );
    }

    println!("\nRegistry Changes ({}):", plan.registry_changes.len());
    for reg in &plan.registry_changes {
        println!(
            "  - {:?}\\{}\\{} -> `{}` (was {:?})",
            reg.hive, reg.key_path, reg.value_name, reg.planned_value, reg.current_value
        );
    }

    if !plan.launch_notes.is_empty() {
        println!("\nLaunch Configuration:");
        for note in &plan.launch_notes {
            println!("  - {note}");
        }
    }

    if !plan.warnings.is_empty() {
        println!("\nWarnings ({}):", plan.warnings.len());
        for warn in &plan.warnings {
            println!("  [WARN] {warn}");
        }
    }
}

fn print_patch_event(event: &PatchEvent) {
    match event {
        PatchEvent::FixStarted { fix_id, title, total_steps } => {
            println!("==> Fix `{fix_id}`: {title} ({total_steps} step(s))");
        }
        PatchEvent::StepStarted { step_id, step_name, .. } => {
            println!("  -> [{step_id}] {step_name}");
        }
        PatchEvent::DownloadStarted { step_id, url, from_cache } => {
            println!("     [{step_id}] downloading `{url}` (from_cache={from_cache})");
        }
        PatchEvent::DownloadVerified { step_id, sha256, bytes } => {
            println!("     [{step_id}] verified {bytes} bytes (sha256={sha256})");
        }
        PatchEvent::FileBackedUp { relative_path, .. } => {
            println!("     backed up `{relative_path}`");
        }
        PatchEvent::FileWritten { relative_path, .. } => {
            println!("     wrote `{relative_path}`");
        }
        PatchEvent::WrapperDeployed { wrapper, files, .. } => {
            println!("     deployed wrapper `{wrapper}` ({} file(s))", files.len());
        }
        PatchEvent::RegistryUpdated { key_path, value_name, new_value, .. } => {
            println!("     registry `{key_path}\\{value_name}` = `{new_value}`");
        }
        PatchEvent::CpuAffinityConfigured { summary, .. } => {
            println!("     affinity: {summary}");
        }
        PatchEvent::StepSkipped { step_id, reason, .. } => {
            println!("  -- [{step_id}] skipped: {reason}");
        }
        PatchEvent::DownloadProgress { .. }
        | PatchEvent::StepCompleted { .. }
        | PatchEvent::FixCompleted { .. } => {}
        PatchEvent::Warning { message } => {
            println!("  [WARN] {message}");
        }
    }
}

#[cfg(windows)]
fn host_registry_backend() -> Box<dyn RegistryBackend> {
    Box::new(ogsc_patcher::WindowsRegistryBackend)
}

#[cfg(not(windows))]
fn host_registry_backend() -> Box<dyn RegistryBackend> {
    Box::new(ogsc_patcher::MemoryRegistryBackend::new())
}
