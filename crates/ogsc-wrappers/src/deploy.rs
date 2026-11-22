//! Dry-run planning and transactional deployment for [`WrapperDeployment`] specifications.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use ogsc_recipes::{DllChainRule, WrapperDeployment, WrapperKind};
use ogsc_scanner::BackupManager;

use crate::catalog::{normalize_safe_relative, ToolkitKind, ToolkitStore};
use crate::config::{
    render_86box_cfg, render_dgvoodoo_conf, render_dosbox_x_conf, render_dxwrapper_ini,
    render_xinput_plus_ini,
};
use crate::error::WrapperError;

/// A single planned wrapper DLL copy operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedDllCopy {
    /// Toolkit directory owning this DLL asset.
    pub toolkit: String,
    /// Absolute source path inside the local [`ToolkitStore`].
    pub source_path: PathBuf,
    /// Game-relative destination path where the DLL will be deployed.
    pub destination_relative_path: String,
    /// Original proxy DLL name if `dll_chain_renames` renamed this destination.
    pub chained_from: Option<String>,
    /// Whether `source_path` currently exists on disk inside the [`ToolkitStore`].
    pub source_exists: bool,
}

/// A single generated wrapper/emulator configuration file write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedConfigWrite {
    /// Game-relative destination path of the generated config file.
    pub destination_relative_path: String,
    /// Full CRLF-formatted file contents.
    pub content: String,
}

/// Non-destructive dry-run preview of a [`WrapperDeployment`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapperPlan {
    /// Human-readable wrapper slug (`dgvoodoo2`, `dxwrapper`, `wined3d`, `xinput_plus`, `dosbox_x`, `86box`).
    pub wrapper_name: &'static str,
    /// Normalized game-relative target directory (`"."` for game root).
    pub target_dir: String,
    /// DLL files to copy into the game folder.
    pub dll_copies: Vec<PlannedDllCopy>,
    /// Generated configuration files to write into the game folder.
    pub config_writes: Vec<PlannedConfigWrite>,
    /// Expected emulator binary path when deploying a `dosbox_x` or `86box` profile.
    pub emulator_executable: Option<PathBuf>,
}

/// Report returned after transactionally deploying a wrapper via [`deploy_wrapper`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapperDeployReport {
    /// Deployed wrapper slug.
    pub wrapper_name: &'static str,
    /// Ordered game-relative paths created or updated (backed up first via [`BackupManager`]).
    pub deployed_files: Vec<String>,
}

/// Computes a pure read-only [`WrapperPlan`] without modifying the filesystem.
pub fn plan_wrapper_deployment(
    store: &ToolkitStore,
    deployment: &WrapperDeployment,
) -> Result<WrapperPlan, WrapperError> {
    let target_dir = normalize_safe_relative(&deployment.target_dir, true)?;
    for rule in &deployment.dll_chain_renames {
        let _ = normalize_safe_relative(&rule.original_dll, false)?;
        let _ = normalize_safe_relative(&rule.chained_as, false)?;
        if rule.original_dll.eq_ignore_ascii_case(&rule.chained_as) {
            return Err(WrapperError::InvalidChainRename {
                original_dll: rule.original_dll.clone(),
                chained_as: rule.chained_as.clone(),
            });
        }
    }

    let mut dll_copies = Vec::new();
    let mut config_writes = Vec::new();
    let mut emulator_executable = None;

    let wrapper_name = match &deployment.wrapper {
        WrapperKind::DgVoodoo2 { api_targets, arch, config_overrides } => {
            for &api in api_targets {
                let (src, orig_name) = store.dgvoodoo_asset(api, *arch);
                push_planned_dll(
                    &mut dll_copies,
                    ToolkitKind::DgVoodoo2.slug(),
                    src,
                    orig_name,
                    &target_dir,
                    &deployment.dll_chain_renames,
                )?;
            }
            config_writes.push(PlannedConfigWrite {
                destination_relative_path: join_rel(&target_dir, "dgVoodoo.conf"),
                content: render_dgvoodoo_conf(config_overrides),
            });
            ToolkitKind::DgVoodoo2.slug()
        }
        WrapperKind::DxWrapper { stub_dlls, config_overrides } => plan_dxwrapper(
            store,
            &target_dir,
            stub_dlls,
            config_overrides,
            &deployment.dll_chain_renames,
            &mut dll_copies,
            &mut config_writes,
        )?,
        WrapperKind::WineD3d { dlls, .. } => {
            plan_named_dlls(
                &mut dll_copies,
                ToolkitKind::WineD3d.slug(),
                dlls,
                &target_dir,
                &deployment.dll_chain_renames,
                |dll| store.wined3d_asset(dll),
            )?;
            ToolkitKind::WineD3d.slug()
        }
        WrapperKind::XInputPlus { target_dlls, profile_ini } => {
            plan_named_dlls(
                &mut dll_copies,
                ToolkitKind::XInputPlus.slug(),
                target_dlls,
                &target_dir,
                &deployment.dll_chain_renames,
                |dll| store.xinput_plus_asset(dll),
            )?;
            config_writes.push(PlannedConfigWrite {
                destination_relative_path: join_rel(&target_dir, "XInputPlus.ini"),
                content: render_xinput_plus_ini(profile_ini),
            });
            ToolkitKind::XInputPlus.slug()
        }
        WrapperKind::DosBoxX { machine, os_profile, config_overrides } => {
            emulator_executable = Some(store.dosbox_x_executable());
            config_writes.push(PlannedConfigWrite {
                destination_relative_path: join_rel(&target_dir, "dosbox-x.conf"),
                content: render_dosbox_x_conf(machine, *os_profile, config_overrides),
            });
            ToolkitKind::DosBoxX.slug()
        }
        WrapperKind::Box86 { machine_profile, os_profile, config_overrides } => {
            emulator_executable = Some(store.box86_executable());
            config_writes.push(PlannedConfigWrite {
                destination_relative_path: join_rel(&target_dir, "86box.cfg"),
                content: render_86box_cfg(machine_profile, *os_profile, config_overrides),
            });
            ToolkitKind::Box86.slug()
        }
    };

    Ok(WrapperPlan { wrapper_name, target_dir, dll_copies, config_writes, emulator_executable })
}

fn plan_dxwrapper(
    store: &ToolkitStore,
    target_dir: &str,
    stub_dlls: &[String],
    config_overrides: &std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, String>,
    >,
    renames: &[DllChainRule],
    dll_copies: &mut Vec<PlannedDllCopy>,
    config_writes: &mut Vec<PlannedConfigWrite>,
) -> Result<&'static str, WrapperError> {
    let mut seen = BTreeSet::new();
    seen.insert("dxwrapper.dll".to_owned());
    push_planned_dll(
        dll_copies,
        ToolkitKind::DxWrapper.slug(),
        store.dxwrapper_core_dll(),
        "dxwrapper.dll",
        target_dir,
        renames,
    )?;
    for stub in stub_dlls {
        let safe_stub = normalize_safe_relative(stub, false)?;
        if seen.insert(safe_stub.to_ascii_lowercase()) {
            push_planned_dll(
                dll_copies,
                ToolkitKind::DxWrapper.slug(),
                store.dxwrapper_stub_asset(&safe_stub),
                &safe_stub,
                target_dir,
                renames,
            )?;
        }
    }
    config_writes.push(PlannedConfigWrite {
        destination_relative_path: join_rel(target_dir, "dxwrapper.ini"),
        content: render_dxwrapper_ini(stub_dlls, config_overrides),
    });
    Ok(ToolkitKind::DxWrapper.slug())
}

fn plan_named_dlls(
    dll_copies: &mut Vec<PlannedDllCopy>,
    toolkit: &'static str,
    dlls: &[String],
    target_dir: &str,
    renames: &[DllChainRule],
    resolve_src: impl Fn(&str) -> PathBuf,
) -> Result<(), WrapperError> {
    for dll in dlls {
        let safe_dll = normalize_safe_relative(dll, false)?;
        push_planned_dll(
            dll_copies,
            toolkit,
            resolve_src(&safe_dll),
            &safe_dll,
            target_dir,
            renames,
        )?;
    }
    Ok(())
}

/// Validates, backs up originals via [`BackupManager`], and deploys wrapper DLLs and config files into `game_root`.
pub fn deploy_wrapper(
    store: &ToolkitStore,
    game_root: &Path,
    fix_id: &str,
    deployment: &WrapperDeployment,
    backup: &mut BackupManager,
) -> Result<WrapperDeployReport, WrapperError> {
    let plan = plan_wrapper_deployment(store, deployment)?;

    // Pre-flight check: verify every required source DLL exists before modifying anything.
    for copy in &plan.dll_copies {
        if !copy.source_exists {
            return Err(WrapperError::MissingToolkitAsset {
                toolkit: copy.toolkit.clone(),
                expected_path: copy.source_path.clone(),
            });
        }
    }

    let mut deployed_files = Vec::new();

    if plan.target_dir != "." {
        ensure_relative_dir(game_root, &plan.target_dir, fix_id, backup)?;
    }

    for copy in &plan.dll_copies {
        let dest_abs = join_abs_rel(game_root, &copy.destination_relative_path);
        if let Some(parent_rel) = Path::new(&copy.destination_relative_path).parent() {
            let parent_str = parent_rel.to_string_lossy().replace('\\', "/");
            if !parent_str.is_empty() && parent_str != "." {
                ensure_relative_dir(game_root, &parent_str, fix_id, backup)?;
            }
        }
        backup.backup_file_before_write(fix_id, &copy.destination_relative_path)?;
        clear_readonly_if_exists(&dest_abs)?;
        fs::copy(&copy.source_path, &dest_abs)
            .map_err(|source| WrapperError::Io { path: dest_abs, source })?;
        deployed_files.push(copy.destination_relative_path.clone());
    }

    for cfg in &plan.config_writes {
        let dest_abs = join_abs_rel(game_root, &cfg.destination_relative_path);
        backup.backup_file_before_write(fix_id, &cfg.destination_relative_path)?;
        clear_readonly_if_exists(&dest_abs)?;
        fs::write(&dest_abs, cfg.content.as_bytes())
            .map_err(|source| WrapperError::Io { path: dest_abs, source })?;
        deployed_files.push(cfg.destination_relative_path.clone());
    }

    Ok(WrapperDeployReport { wrapper_name: plan.wrapper_name, deployed_files })
}

fn clear_readonly_if_exists(path: &Path) -> Result<(), WrapperError> {
    match fs::metadata(path) {
        Ok(meta) => {
            let mut perms = meta.permissions();
            if perms.readonly() {
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                fs::set_permissions(path, perms)
                    .map_err(|source| WrapperError::Io { path: path.to_path_buf(), source })?;
            }
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(WrapperError::Io { path: path.to_path_buf(), source }),
    }
}

fn push_planned_dll(
    out: &mut Vec<PlannedDllCopy>,
    toolkit: &str,
    source_path: PathBuf,
    original_filename: &str,
    target_dir: &str,
    renames: &[DllChainRule],
) -> Result<(), WrapperError> {
    let safe_orig = normalize_safe_relative(original_filename, false)?;
    let rename = renames.iter().find(|r| r.original_dll.eq_ignore_ascii_case(&safe_orig));
    let (final_name, chained_from) = match rename {
        Some(rule) => (normalize_safe_relative(&rule.chained_as, false)?, Some(safe_orig)),
        None => (safe_orig, None),
    };
    let source_exists = source_path.is_file();

    out.push(PlannedDllCopy {
        toolkit: toolkit.to_owned(),
        source_path,
        destination_relative_path: join_rel(target_dir, &final_name),
        chained_from,
        source_exists,
    });
    Ok(())
}

fn ensure_relative_dir(
    game_root: &Path,
    rel_dir: &str,
    fix_id: &str,
    backup: &mut BackupManager,
) -> Result<(), WrapperError> {
    let mut cur_rel = String::new();
    let mut cur_abs = game_root.to_path_buf();
    for seg in rel_dir.split('/').filter(|s| !s.is_empty()) {
        if !cur_rel.is_empty() {
            cur_rel.push('/');
        }
        cur_rel.push_str(seg);
        cur_abs.push(seg);
        if !cur_abs.exists() {
            fs::create_dir(&cur_abs)
                .map_err(|source| WrapperError::Io { path: cur_abs.clone(), source })?;
            backup.record_created_directory(fix_id, &cur_rel)?;
        }
    }
    Ok(())
}

fn join_rel(dir: &str, file: &str) -> String {
    if dir == "." {
        file.to_owned()
    } else {
        format!("{dir}/{file}")
    }
}

fn join_abs_rel(root: &Path, norm_rel: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    for seg in norm_rel.split('/') {
        out.push(seg);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ogsc_recipes::{DgVoodooApi, LegacyOsProfile, TargetArch};

    use super::*;

    #[test]
    fn deploys_and_rolls_back_dgvoodoo_and_chained_xinput_plus() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let toolkits_dir = temp.path().join("toolkits");
        let game_dir = temp.path().join("Game");
        fs::create_dir_all(toolkits_dir.join("dgvoodoo2").join("MS").join("x86"))
            .expect("mkdir dg");
        fs::create_dir_all(toolkits_dir.join("xinput_plus")).expect("mkdir xinput");
        fs::create_dir_all(game_dir.join("offline").join("system")).expect("mkdir game");

        fs::write(
            toolkits_dir.join("dgvoodoo2").join("MS").join("x86").join("D3D8.dll"),
            b"DGVOODOO_D3D8_BYTES",
        )
        .expect("write d3d8");
        fs::write(toolkits_dir.join("xinput_plus").join("dinput8.dll"), b"XINPUT_DINPUT8_BYTES")
            .expect("write dinput8");
        fs::write(toolkits_dir.join("xinput_plus").join("xinput1_3.dll"), b"XINPUT_1_3_BYTES")
            .expect("write xinput1_3");

        let store = ToolkitStore::new(&toolkits_dir);
        let mut backup =
            BackupManager::open(&game_dir, ".ogsc_backup", "sc-pt", "retail").expect("open backup");

        let dg_dep = WrapperDeployment {
            wrapper: WrapperKind::DgVoodoo2 {
                api_targets: vec![DgVoodooApi::D3d8],
                arch: TargetArch::X86,
                config_overrides: BTreeMap::from([(
                    "DirectX".to_owned(),
                    BTreeMap::from([("VRAM".to_owned(), "256".to_owned())]),
                )]),
            },
            min_toolkit_version: None,
            target_dir: "offline/system".to_owned(),
            dll_chain_renames: Vec::new(),
        };

        let dg_report =
            deploy_wrapper(&store, &game_dir, "dgvoodoo2_wrapper", &dg_dep, &mut backup)
                .expect("deploy dgVoodoo2");
        assert_eq!(
            dg_report.deployed_files,
            vec!["offline/system/D3D8.dll", "offline/system/dgVoodoo.conf"]
        );
        let conf =
            fs::read_to_string(game_dir.join("offline").join("system").join("dgVoodoo.conf"))
                .expect("read conf");
        assert!(conf.contains("VRAM = 256\r\n"));
        assert!(conf.contains("dgVoodooWatermark = false\r\n"));

        let xi_dep = WrapperDeployment {
            wrapper: WrapperKind::XInputPlus {
                target_dlls: vec!["dinput8.dll".to_owned(), "xinput1_3.dll".to_owned()],
                profile_ini: BTreeMap::new(),
            },
            min_toolkit_version: None,
            target_dir: ".".to_owned(),
            dll_chain_renames: vec![DllChainRule {
                original_dll: "dinput8.dll".to_owned(),
                chained_as: "dinput8_xinput.dll".to_owned(),
                notes: None,
            }],
        };

        let xi_report = deploy_wrapper(&store, &game_dir, "xinput_plus", &xi_dep, &mut backup)
            .expect("deploy xinput_plus");
        assert_eq!(
            xi_report.deployed_files,
            vec!["dinput8_xinput.dll", "xinput1_3.dll", "XInputPlus.ini"]
        );
        assert!(game_dir.join("dinput8_xinput.dll").is_file());
        assert!(!game_dir.join("dinput8.dll").exists());

        let summary = backup.rollback_all(&[]).expect("rollback wrappers");
        assert_eq!(summary.deleted_files, 5);
        assert!(!game_dir.join("offline").join("system").join("D3D8.dll").exists());
        assert!(!game_dir.join("dinput8_xinput.dll").exists());
    }

    #[test]
    fn missing_toolkit_asset_aborts_before_mutating_game_directory() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let store = ToolkitStore::new(temp.path().join("empty_toolkits"));
        let game_dir = temp.path().join("Game");
        fs::create_dir_all(&game_dir).expect("mkdir game");

        let mut backup =
            BackupManager::open(&game_dir, ".ogsc_backup", "g", "r").expect("open backup");
        let dep = WrapperDeployment {
            wrapper: WrapperKind::DxWrapper {
                stub_dlls: vec!["ddraw.dll".to_owned()],
                config_overrides: BTreeMap::new(),
            },
            min_toolkit_version: None,
            target_dir: ".".to_owned(),
            dll_chain_renames: Vec::new(),
        };

        let err = deploy_wrapper(&store, &game_dir, "dxwrapper", &dep, &mut backup)
            .expect_err("missing dxwrapper.dll should fail pre-flight");
        assert!(matches!(err, WrapperError::MissingToolkitAsset { .. }));
        assert!(!game_dir.join("dxwrapper.ini").exists(), "no config file should be written");
        assert!(!game_dir.join(".ogsc_backup").exists(), "no backup journal should remain");
    }

    #[test]
    fn plans_dosbox_x_emulator_profile_cleanly() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let store = ToolkitStore::new(temp.path());
        let dep = WrapperDeployment {
            wrapper: WrapperKind::DosBoxX {
                machine: "svga_s3".to_owned(),
                os_profile: LegacyOsProfile::Win95,
                config_overrides: BTreeMap::new(),
            },
            min_toolkit_version: None,
            target_dir: ".".to_owned(),
            dll_chain_renames: Vec::new(),
        };
        let plan = plan_wrapper_deployment(&store, &dep).expect("plan dosbox-x");
        assert_eq!(plan.wrapper_name, "dosbox_x");
        assert_eq!(plan.emulator_executable, Some(store.dosbox_x_executable()));
        assert_eq!(plan.config_writes[0].destination_relative_path, "dosbox-x.conf");
        assert!(plan.config_writes[0].content.contains("ver = 95\r\n"));
    }
}
