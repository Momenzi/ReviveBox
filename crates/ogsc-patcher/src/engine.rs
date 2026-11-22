//! Transactional patch execution engine with per-fix automatic rollback on failure.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use ogsc_recipes::{
    ArchiveFormat, BinaryPatchSource, DllSource, FixOption, GameRecipe, PatchAction, PatchStep,
    WrapperDeployment,
};
use ogsc_scanner::{BackupManager, RollbackSummary};
use ogsc_wrappers::{deploy_wrapper, plan_wrapper_deployment, ToolkitDownloadSpec, ToolkitKind};

use crate::archive::{
    ensure_parent_dirs, extract_archive, extract_single_archive_member,
    extract_zip_into_toolkit_dir, join_abs_rel, normalize_safe_relative,
    write_file_clearing_readonly, ExtractArchiveOptions,
};
use crate::bsdiff_patch::apply_verified_bsdiff;
use crate::download::{fetch_verified_file, Downloader};
use crate::error::PatcherError;
use crate::events::{EventSink, PatchEvent};
use crate::ini::apply_ini_updates;
use crate::plan::{resolve_selected_fixes, step_skip_reason, PatchContext};
use crate::registry::{apply_compat_flags, revert_registry_changes, RegistryBackend};

/// Report returned after [`PatchEngine::apply_fixes`] completes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// Fix IDs successfully applied and recorded in the backup journal.
    pub applied_fix_ids: Vec<String>,
    /// Step IDs skipped due to `applies_to_releases` or `os_filter`.
    pub skipped_steps: Vec<String>,
    /// Non-fatal warnings emitted during execution.
    pub warnings: Vec<String>,
}

/// Executes declarative recipe [`FixOption`]s transactionally against a local game directory.
pub struct PatchEngine<'a> {
    downloader: &'a dyn Downloader,
    registry: &'a mut dyn RegistryBackend,
}

impl<'a> PatchEngine<'a> {
    /// Constructs a new [`PatchEngine`] backed by `downloader` and `registry`.
    pub fn new(downloader: &'a dyn Downloader, registry: &'a mut dyn RegistryBackend) -> Self {
        Self { downloader, registry }
    }

    /// Applies `selected_fix_ids` in recipe order through [`BackupManager`].
    ///
    /// If any step inside a fix fails, all filesystem and registry edits performed by that failing
    /// fix are automatically rolled back before returning the error.
    pub fn apply_fixes(
        &mut self,
        recipe: &GameRecipe,
        release_id: &str,
        game_root: &Path,
        selected_fix_ids: &[String],
        ctx: &PatchContext,
        sink: &mut dyn EventSink,
    ) -> Result<ApplyReport, PatcherError> {
        let selected_fixes = resolve_selected_fixes(recipe, release_id, selected_fix_ids)?;
        let preserve_paths = preserve_paths_for_recipe(recipe);

        let mut backup = BackupManager::open(
            game_root,
            &recipe.rollback.backup_directory,
            &recipe.game_id,
            release_id,
        )?;

        let mut report = ApplyReport::default();

        for fix in selected_fixes {
            let active_count = fix
                .steps
                .iter()
                .filter(|s| step_skip_reason(s, release_id, ctx.windows_build).is_none())
                .count();
            sink.emit(PatchEvent::FixStarted {
                fix_id: fix.id.clone(),
                title: fix.title.clone(),
                total_steps: active_count,
            });

            if let Err(err) = self.apply_single_fix(
                fix,
                release_id,
                game_root,
                ctx,
                &mut backup,
                sink,
                &mut report,
            ) {
                // Transactional safety: undo partial mutations made by the failing fix.
                if backup.journal().applied_fixes.contains_key(&fix.id) {
                    match backup.rollback_fix(&fix.id, &preserve_paths) {
                        Ok(rb) => {
                            if let Err(reg_err) = revert_registry_changes(
                                self.registry,
                                &rb.registry_actions_to_revert,
                            ) {
                                sink.emit(PatchEvent::Warning {
                                    message: format!(
                                        "failed to revert registry during failure cleanup for `{}`: {reg_err}",
                                        fix.id
                                    ),
                                });
                            }
                        }
                        Err(rb_err) => {
                            sink.emit(PatchEvent::Warning {
                                message: format!(
                                    "failed partial rollback for `{}`: {rb_err}",
                                    fix.id
                                ),
                            });
                        }
                    }
                }
                return Err(err);
            }

            sink.emit(PatchEvent::FixCompleted { fix_id: fix.id.clone() });
            report.applied_fix_ids.push(fix.id.clone());
        }

        Ok(report)
    }

    /// Reverts a single `fix_id` (or all active fixes when `fix_id == None`) and restores registry state.
    pub fn rollback_fixes(
        &mut self,
        recipe: &GameRecipe,
        game_root: &Path,
        fix_id: Option<&str>,
        sink: &mut dyn EventSink,
    ) -> Result<RollbackSummary, PatcherError> {
        let preserve_paths = preserve_paths_for_recipe(recipe);
        let mut backup =
            BackupManager::load_existing(game_root, &recipe.rollback.backup_directory)?;

        let summary = match fix_id {
            Some(id) => backup.rollback_fix(id, &preserve_paths)?,
            None => backup.rollback_all(&preserve_paths)?,
        };

        if recipe.rollback.restore_registry_values && !summary.registry_actions_to_revert.is_empty()
        {
            let reverted =
                revert_registry_changes(self.registry, &summary.registry_actions_to_revert)?;
            sink.emit(PatchEvent::Warning {
                message: format!("Reverted {reverted} Windows Registry AppCompat entry(ies)"),
            });
        }

        Ok(summary)
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_single_fix(
        &mut self,
        fix: &FixOption,
        release_id: &str,
        game_root: &Path,
        ctx: &PatchContext,
        backup: &mut BackupManager,
        sink: &mut dyn EventSink,
        report: &mut ApplyReport,
    ) -> Result<(), PatcherError> {
        let mut cached_archives: HashMap<String, (PathBuf, ArchiveFormat)> = HashMap::new();

        for step in &fix.steps {
            if let Some(reason) = step_skip_reason(step, release_id, ctx.windows_build) {
                sink.emit(PatchEvent::StepSkipped {
                    fix_id: fix.id.clone(),
                    step_id: step.id.clone(),
                    reason,
                });
                report.skipped_steps.push(step.id.clone());
                continue;
            }

            sink.emit(PatchEvent::StepStarted {
                fix_id: fix.id.clone(),
                step_id: step.id.clone(),
                step_name: step.name.clone(),
            });

            self.execute_step(
                &fix.id,
                step,
                game_root,
                ctx,
                backup,
                &mut cached_archives,
                sink,
                report,
            )?;

            sink.emit(PatchEvent::StepCompleted {
                fix_id: fix.id.clone(),
                step_id: step.id.clone(),
            });
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_step(
        &mut self,
        fix_id: &str,
        step: &PatchStep,
        game_root: &Path,
        ctx: &PatchContext,
        backup: &mut BackupManager,
        cached_archives: &mut HashMap<String, (PathBuf, ArchiveFormat)>,
        sink: &mut dyn EventSink,
        report: &mut ApplyReport,
    ) -> Result<(), PatcherError> {
        match &step.action {
            PatchAction::DownloadVerifiedZip { .. } => self.execute_download_step(
                fix_id,
                step,
                game_root,
                ctx,
                backup,
                cached_archives,
                sink,
            ),
            PatchAction::ExtractDll { .. } => execute_extract_dll(
                self.downloader,
                fix_id,
                &step.action,
                game_root,
                ctx,
                backup,
                cached_archives,
                sink,
            ),
            PatchAction::DeployWrapper(deployment) => execute_wrapper_step(
                self.downloader,
                fix_id,
                deployment,
                game_root,
                ctx,
                backup,
                sink,
            ),
            PatchAction::ApplyBinaryBsdiffPatch { .. } => {
                self.execute_bsdiff_patch(fix_id, step, game_root, ctx, backup, sink, report)
            }
            PatchAction::SetRegistryCompatFlags { .. } => {
                execute_registry_step(self.registry, fix_id, &step.action, game_root, backup, sink)
            }
            PatchAction::WriteIniConfig { .. } => {
                execute_write_ini_step(fix_id, &step.action, game_root, ctx, backup, sink)
            }
            PatchAction::LimitCpuAffinity { mode, apply_method, process_priority } => {
                let summary =
                    format!("{mode:?} via {apply_method:?} (priority: {process_priority:?})");
                sink.emit(PatchEvent::CpuAffinityConfigured { fix_id: fix_id.to_owned(), summary });
                Ok(())
            }
            PatchAction::ConfigureSandbox { .. } => {
                execute_sandbox_step(fix_id, &step.action, game_root, backup, sink, report)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_download_step(
        &self,
        fix_id: &str,
        step: &PatchStep,
        game_root: &Path,
        ctx: &PatchContext,
        backup: &mut BackupManager,
        cached_archives: &mut HashMap<String, (PathBuf, ArchiveFormat)>,
        sink: &mut dyn EventSink,
    ) -> Result<(), PatcherError> {
        let PatchAction::DownloadVerifiedZip {
            url,
            mirror_urls,
            sha256,
            archive_format,
            extract_to,
            include_patterns,
            strip_components,
            overwrite_existing,
            ..
        } = &step.action
        else {
            return Ok(());
        };
        let archive_path = fetch_verified_file(
            self.downloader,
            &ctx.cache_dir,
            url,
            mirror_urls,
            sha256,
            &step.id,
            sink,
        )?;
        let _ = extract_archive(
            &archive_path,
            game_root,
            &ExtractArchiveOptions {
                fix_id,
                format: *archive_format,
                extract_to,
                include_patterns,
                strip_components: *strip_components,
                overwrite_existing: *overwrite_existing,
            },
            backup,
            sink,
        )?;
        cached_archives.insert(step.id.clone(), (archive_path, *archive_format));
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_bsdiff_patch(
        &self,
        fix_id: &str,
        step: &PatchStep,
        game_root: &Path,
        ctx: &PatchContext,
        backup: &mut BackupManager,
        sink: &mut dyn EventSink,
        report: &mut ApplyReport,
    ) -> Result<(), PatcherError> {
        let PatchAction::ApplyBinaryBsdiffPatch {
            target_file,
            patch_source,
            expected_original_sha256,
            expected_patched_sha256,
            backup_original,
            ..
        } = &step.action
        else {
            return Ok(());
        };
        let norm_target = normalize_safe_relative(target_file, false)?;
        let abs_target = join_abs_rel(game_root, &norm_target);
        let original_bytes = fs::read(&abs_target)
            .map_err(|source| PatcherError::Io { path: abs_target.clone(), source })?;

        if ogsc_scanner::sha256_bytes(&original_bytes)
            == expected_patched_sha256.to_ascii_lowercase()
        {
            sink.emit(PatchEvent::StepSkipped {
                fix_id: fix_id.to_owned(),
                step_id: step.id.clone(),
                reason: format!("`{norm_target}` already matches expected patched SHA-256"),
            });
            report.skipped_steps.push(step.id.clone());
            return Ok(());
        }

        let patch_path = match patch_source {
            BinaryPatchSource::Download { url, sha256 } => fetch_verified_file(
                self.downloader,
                &ctx.cache_dir,
                url,
                &[],
                sha256,
                &step.id,
                sink,
            )?,
            BinaryPatchSource::LocalManifestRelative { path, sha256 } => {
                let norm_patch = normalize_safe_relative(path, false)?;
                let base = ctx.recipe_dir.as_deref().unwrap_or(game_root);
                let full_patch = join_abs_rel(base, &norm_patch);
                let actual = ogsc_scanner::compute_sha256(&full_patch)?;
                if actual != sha256.to_ascii_lowercase() {
                    return Err(PatcherError::HashMismatch {
                        subject: norm_patch,
                        expected: sha256.to_ascii_lowercase(),
                        actual,
                    });
                }
                full_patch
            }
        };

        let patch_bytes = fs::read(&patch_path)
            .map_err(|source| PatcherError::Io { path: patch_path, source })?;
        let patched_bytes = apply_verified_bsdiff(
            &original_bytes,
            &patch_bytes,
            &norm_target,
            expected_original_sha256,
            expected_patched_sha256,
        )?;

        if *backup_original {
            backup.backup_file_before_write(fix_id, &norm_target)?;
            sink.emit(PatchEvent::FileBackedUp {
                fix_id: fix_id.to_owned(),
                relative_path: norm_target.clone(),
            });
        }
        write_file_clearing_readonly(&abs_target, &patched_bytes)?;
        sink.emit(PatchEvent::FileWritten {
            fix_id: fix_id.to_owned(),
            relative_path: norm_target,
        });
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_extract_dll(
    downloader: &dyn Downloader,
    fix_id: &str,
    action: &PatchAction,
    game_root: &Path,
    ctx: &PatchContext,
    backup: &mut BackupManager,
    cached_archives: &HashMap<String, (PathBuf, ArchiveFormat)>,
    sink: &mut dyn EventSink,
) -> Result<(), PatcherError> {
    let PatchAction::ExtractDll {
        source, destination_path, expected_sha256, backup_original, ..
    } = action
    else {
        return Ok(());
    };
    let expected_sha256 = expected_sha256.as_deref();
    let backup_original = *backup_original;
    match source {
        DllSource::BundledToolkit { toolkit, relative_path } => {
            if ctx.toolkit_store.resolve_bundled_file(toolkit, relative_path).is_err() {
                if let Some(spec) =
                    ToolkitKind::from_slug(toolkit).and_then(ToolkitKind::official_download)
                {
                    if let Err(err) = provision_toolkit_from_spec(downloader, ctx, spec, sink) {
                        sink.emit(PatchEvent::Warning {
                            message: format!(
                                "Could not auto-download `{}` v{}: {err}",
                                spec.kind.slug(),
                                spec.version
                            ),
                        });
                    }
                }
            }
            let src = ctx.toolkit_store.resolve_bundled_file(toolkit, relative_path)?;
            let dest_rel = normalize_safe_relative(destination_path, false)?;
            let bytes = fs::read(&src).map_err(|source| PatcherError::Io { path: src, source })?;
            if let Some(expected) = expected_sha256 {
                let actual = ogsc_scanner::sha256_bytes(&bytes);
                if actual != expected.to_ascii_lowercase() {
                    return Err(PatcherError::HashMismatch {
                        subject: destination_path.to_owned(),
                        expected: expected.to_ascii_lowercase(),
                        actual,
                    });
                }
            }
            if backup_original {
                backup.backup_file_before_write(fix_id, &dest_rel)?;
                sink.emit(PatchEvent::FileBackedUp {
                    fix_id: fix_id.to_owned(),
                    relative_path: dest_rel.clone(),
                });
            }
            ensure_parent_dirs(game_root, &dest_rel, fix_id, backup)?;
            let dest_abs = join_abs_rel(game_root, &dest_rel);
            write_file_clearing_readonly(&dest_abs, &bytes)?;
            sink.emit(PatchEvent::FileWritten {
                fix_id: fix_id.to_owned(),
                relative_path: dest_rel,
            });
            Ok(())
        }
        DllSource::CachedArchive { download_step_id, archive_member } => {
            let (archive_path, format) =
                cached_archives.get(download_step_id).ok_or_else(|| {
                    PatcherError::MissingCachedArchive {
                        download_step_id: download_step_id.clone(),
                    }
                })?;
            extract_single_archive_member(
                archive_path,
                *format,
                archive_member,
                game_root,
                fix_id,
                destination_path,
                expected_sha256,
                backup_original,
                backup,
                sink,
            )
        }
    }
}

fn execute_write_ini_step(
    fix_id: &str,
    action: &PatchAction,
    game_root: &Path,
    ctx: &PatchContext,
    backup: &mut BackupManager,
    sink: &mut dyn EventSink,
) -> Result<(), PatcherError> {
    let PatchAction::WriteIniConfig {
        target_file,
        create_if_missing,
        encoding,
        line_ending,
        sections,
    } = action
    else {
        return Ok(());
    };
    let norm_ini = normalize_safe_relative(target_file, false)?;
    let abs_ini = join_abs_rel(game_root, &norm_ini);
    let existing = if abs_ini.is_file() {
        Some(
            fs::read(&abs_ini)
                .map_err(|source| PatcherError::Io { path: abs_ini.clone(), source })?,
        )
    } else if *create_if_missing {
        None
    } else {
        return Err(PatcherError::IniTargetMissing { path: abs_ini });
    };

    let updated = apply_ini_updates(
        existing.as_deref(),
        &norm_ini,
        *encoding,
        *line_ending,
        sections,
        &ctx.dynamic_ctx,
    )?;
    backup.backup_file_before_write(fix_id, &norm_ini)?;
    sink.emit(PatchEvent::FileBackedUp {
        fix_id: fix_id.to_owned(),
        relative_path: norm_ini.clone(),
    });
    ensure_parent_dirs(game_root, &norm_ini, fix_id, backup)?;
    write_file_clearing_readonly(&abs_ini, &updated)?;
    sink.emit(PatchEvent::FileWritten { fix_id: fix_id.to_owned(), relative_path: norm_ini });
    Ok(())
}

fn ensure_dir_recorded(
    game_root: &Path,
    norm_dir: &str,
    fix_id: &str,
    backup: &mut BackupManager,
    sink: &mut dyn EventSink,
) -> Result<(), PatcherError> {
    let mut cur_rel = String::new();
    let mut cur_abs = game_root.to_path_buf();
    for seg in norm_dir.split('/').filter(|s| !s.is_empty()) {
        if !cur_rel.is_empty() {
            cur_rel.push('/');
        }
        cur_rel.push_str(seg);
        cur_abs.push(seg);
        if !cur_abs.exists() {
            fs::create_dir(&cur_abs)
                .map_err(|source| PatcherError::Io { path: cur_abs.clone(), source })?;
            backup.record_created_directory(fix_id, &cur_rel)?;
            sink.emit(PatchEvent::FileWritten {
                fix_id: fix_id.to_owned(),
                relative_path: cur_rel.clone(),
            });
        }
    }
    Ok(())
}

pub(crate) fn provision_toolkit_from_spec(
    downloader: &dyn Downloader,
    ctx: &PatchContext,
    spec: ToolkitDownloadSpec,
    sink: &mut dyn EventSink,
) -> Result<usize, PatcherError> {
    let step_id = format!("toolkit-{}", spec.kind.slug());
    let archive_path = fetch_verified_file(
        downloader,
        &ctx.cache_dir,
        spec.url,
        &[],
        spec.sha256,
        &step_id,
        sink,
    )?;
    let dest_dir = ctx.toolkit_store.toolkit_dir(spec.kind);
    extract_zip_into_toolkit_dir(&archive_path, &dest_dir)
}

fn execute_wrapper_step(
    downloader: &dyn Downloader,
    fix_id: &str,
    deployment: &WrapperDeployment,
    game_root: &Path,
    ctx: &PatchContext,
    backup: &mut BackupManager,
    sink: &mut dyn EventSink,
) -> Result<(), PatcherError> {
    let pre_plan = plan_wrapper_deployment(&ctx.toolkit_store, deployment)?;
    if pre_plan.dll_copies.iter().any(|c| !c.source_exists) {
        if let Some(spec) = ToolkitKind::from_wrapper_kind(&deployment.wrapper).official_download()
        {
            if let Err(err) = provision_toolkit_from_spec(downloader, ctx, spec, sink) {
                sink.emit(PatchEvent::Warning {
                    message: format!(
                        "Could not auto-download `{}` v{}: {err}",
                        spec.kind.slug(),
                        spec.version
                    ),
                });
            }
        }
    }

    let deployed = deploy_wrapper(&ctx.toolkit_store, game_root, fix_id, deployment, backup)?;
    for file in &deployed.deployed_files {
        sink.emit(PatchEvent::FileWritten {
            fix_id: fix_id.to_owned(),
            relative_path: file.clone(),
        });
    }
    sink.emit(PatchEvent::WrapperDeployed {
        fix_id: fix_id.to_owned(),
        wrapper: deployed.wrapper_name.to_owned(),
        files: deployed.deployed_files,
    });
    Ok(())
}

fn execute_registry_step(
    registry: &mut dyn RegistryBackend,
    fix_id: &str,
    action: &PatchAction,
    game_root: &Path,
    backup: &mut BackupManager,
    sink: &mut dyn EventSink,
) -> Result<(), PatcherError> {
    let PatchAction::SetRegistryCompatFlags { hive, target_executable, flags, merge_with_existing } =
        action
    else {
        return Ok(());
    };
    let norm_exe = normalize_safe_relative(target_executable, false)?;
    let abs_exe = join_abs_rel(game_root, &norm_exe);
    let (record, new_value) =
        apply_compat_flags(registry, *hive, &abs_exe, flags, *merge_with_existing)?;
    backup.record_registry_change(fix_id, record.clone())?;
    sink.emit(PatchEvent::RegistryUpdated {
        fix_id: fix_id.to_owned(),
        key_path: record.key_path,
        value_name: record.value_name,
        new_value,
    });
    Ok(())
}

fn execute_sandbox_step(
    fix_id: &str,
    action: &PatchAction,
    game_root: &Path,
    backup: &mut BackupManager,
    sink: &mut dyn EventSink,
    report: &mut ApplyReport,
) -> Result<(), PatcherError> {
    let PatchAction::ConfigureSandbox { create_directories, migrate_existing_saves_copy_only } =
        action
    else {
        return Ok(());
    };
    for dir in create_directories {
        let norm_dir = normalize_safe_relative(dir, false)?;
        ensure_dir_recorded(game_root, &norm_dir, fix_id, backup, sink)?;
    }
    if *migrate_existing_saves_copy_only {
        let note = "Copy-only save migration runs when legacy user Documents save folders exist"
            .to_owned();
        report.warnings.push(note.clone());
        sink.emit(PatchEvent::Warning { message: note });
    }
    Ok(())
}

fn preserve_paths_for_recipe(recipe: &GameRecipe) -> Vec<String> {
    if recipe.rollback.preserve_user_saves {
        recipe.sandbox.as_ref().map(|sb| vec![sb.sandbox_root.clone()]).unwrap_or_default()
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Write;

    use ogsc_recipes::{DgVoodooApi, TargetArch, WrapperKind};
    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::events::VecEventSink;

    struct MockToolkitDownloader {
        url: String,
        bytes: Vec<u8>,
    }

    impl Downloader for MockToolkitDownloader {
        fn download_to_file(
            &self,
            url: &str,
            dest: &Path,
            on_progress: &mut dyn FnMut(u64, Option<u64>),
        ) -> Result<(), PatcherError> {
            assert_eq!(url, self.url);
            fs::write(dest, &self.bytes)
                .map_err(|source| PatcherError::Io { path: dest.to_path_buf(), source })?;
            on_progress(self.bytes.len() as u64, Some(self.bytes.len() as u64));
            Ok(())
        }
    }

    #[test]
    fn auto_provisions_official_dgvoodoo2_toolkit_and_deploys_d3d9() {
        let dir = tempfile::tempdir().expect("tempdir");
        let game_root = dir.path().join("Game");
        let toolkits_dir = dir.path().join("runtime_wrappers");
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(game_root.join("bin")).expect("mkdir bin");

        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut cursor);
            zip.start_file("MS/x86/D3D9.dll", SimpleFileOptions::default()).expect("start d3d9");
            zip.write_all(b"DGVOODOO_D3D9_STUB").expect("write d3d9");
            zip.finish().expect("finish zip");
        }
        let zip_bytes = cursor.into_inner();
        let actual_sha = ogsc_scanner::sha256_bytes(&zip_bytes);

        let spec = ToolkitDownloadSpec {
            kind: ToolkitKind::DgVoodoo2,
            version: "2.87.6",
            url: "https://github.com/dege-diosg/dgVoodoo2/releases/download/2.87.6/dgVoodoo2_87_6.zip",
            sha256: Box::leak(actual_sha.into_boxed_str()),
        };
        let dl = MockToolkitDownloader { url: spec.url.to_owned(), bytes: zip_bytes };
        let ctx = PatchContext::for_game(
            &game_root,
            ogsc_wrappers::ToolkitStore::new(&toolkits_dir),
            cache_dir,
            None,
        );
        let mut sink = VecEventSink::default();

        let extracted = provision_toolkit_from_spec(&dl, &ctx, spec, &mut sink).expect("provision");
        assert_eq!(extracted, 1);
        assert!(toolkits_dir.join("dgvoodoo2").join("MS").join("x86").join("D3D9.dll").is_file());

        let mut backup =
            BackupManager::open(&game_root, ".ogsc_backup", "barbie", "retail").expect("backup");
        let deployment = WrapperDeployment {
            wrapper: WrapperKind::DgVoodoo2 {
                api_targets: vec![DgVoodooApi::D3d9],
                arch: TargetArch::X86,
                config_overrides: BTreeMap::new(),
            },
            min_toolkit_version: None,
            target_dir: "bin".to_owned(),
            dll_chain_renames: Vec::new(),
        };
        let report =
            deploy_wrapper(&ctx.toolkit_store, &game_root, "dgv2", &deployment, &mut backup)
                .expect("deploy after provision");
        assert!(report.deployed_files.contains(&"bin/D3D9.dll".to_owned()));
        assert_eq!(
            fs::read(game_root.join("bin").join("D3D9.dll")).expect("read deployed"),
            b"DGVOODOO_D3D9_STUB"
        );
    }
}
