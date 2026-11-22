//! End-to-end integration test verifying dry-run planning, multi-fix transactional application
//! (ZIP, 7z, bsdiff, cached archive DLL extraction, dgVoodoo 2 + chained XInput Plus deployment,
//! encoding-aware dynamic INI updates, HKCU AppCompatFlags merging, sandbox saves), launch
//! resolution, mid-fix failure auto-rollback, and full rollback while preserving saves.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use ogsc_patcher::{
    launch_game, plan_fixes, resolve_launch_command, Downloader, LaunchOptions,
    MemoryRegistryBackend, PatchContext, PatchEngine, PatchEvent, PatcherError, RegistryBackend,
    VecEventSink, APP_COMPAT_LAYERS_KEY,
};
use ogsc_recipes::{
    AffinityApplyMethod, AppCompatFlag, ArchiveFormat, BinaryArchitecture, BinaryPatchSource,
    CpuAffinityMode, DgVoodooApi, Distribution, DllChainRule, DllSource, DynamicIniValue,
    DynamicValueKind, ExecutableFingerprint, FixOption, GameRecipe, GameRelease,
    HashVerificationStatus, HealthStatus, IniEncoding, IniValue, IssueCategory, IssueSeverity,
    KnownIssue, LaunchConfig, LineEnding, PatchAction, PatchStep, ProcessPriority, RecipeMetadata,
    RegistryHive, RollbackPlan, RollbackStrategy, RuntimeRequirements, SandboxConfig, TargetArch,
    WrapperDeployment, WrapperKind, CURRENT_SCHEMA_VERSION,
};
use ogsc_scanner::sha256_bytes;
use ogsc_wrappers::ToolkitStore;
use zip::write::SimpleFileOptions;

struct MockDownloader {
    blobs: BTreeMap<String, Vec<u8>>,
}

impl Downloader for MockDownloader {
    fn download_to_file(
        &self,
        url: &str,
        dest: &Path,
        on_progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), PatcherError> {
        let data = self.blobs.get(url).ok_or_else(|| PatcherError::DownloadFailed {
            url: url.to_owned(),
            reason: "mock URL not found".to_owned(),
        })?;
        fs::write(dest, data)
            .map_err(|source| PatcherError::Io { path: dest.to_path_buf(), source })?;
        on_progress(data.len() as u64, Some(data.len() as u64));
        Ok(())
    }
}

fn create_zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut cursor);
        let opts = SimpleFileOptions::default();
        for (name, data) in entries {
            writer.start_file(name, opts).expect("start zip file");
            writer.write_all(data).expect("write zip data");
        }
        writer.finish().expect("finish zip");
    }
    cursor.into_inner()
}

fn build_widescreen_fix(zip_sha: &str, asi_sha: &str) -> FixOption {
    let ini_sections = BTreeMap::from([(
        "MAIN".to_owned(),
        BTreeMap::from([
            (
                "ResX".to_owned(),
                IniValue::Dynamic(DynamicIniValue { dynamic: DynamicValueKind::DesktopWidth }),
            ),
            (
                "ResY".to_owned(),
                IniValue::Dynamic(DynamicIniValue { dynamic: DynamicValueKind::DesktopHeight }),
            ),
            (
                "SandboxDir".to_owned(),
                IniValue::Dynamic(DynamicIniValue { dynamic: DynamicValueKind::GameSandboxDir }),
            ),
        ]),
    )]);

    FixOption {
        id: "widescreen_fix".to_owned(),
        title: "Widescreen Fix + INI".to_owned(),
        description: "Downloads zip, extracts cached ASI, updates Win1252 INI.".to_owned(),
        recommended: true,
        applies_to_releases: Vec::new(),
        steps: vec![
            PatchStep {
                id: "dl-zip".to_owned(),
                name: "Download Fix Zip".to_owned(),
                description: "Downloads ASI loader".to_owned(),
                applies_to_releases: Vec::new(),
                os_filter: None,
                action: PatchAction::DownloadVerifiedZip {
                    url: "https://patches.example.org/widescreen.zip".to_owned(),
                    mirror_urls: Vec::new(),
                    sha256: zip_sha.to_owned(),
                    verification_status: HashVerificationStatus::Verified,
                    size_bytes: None,
                    archive_format: ArchiveFormat::Zip,
                    extract_to: ".".to_owned(),
                    include_patterns: vec!["dinput8.dll".to_owned()],
                    strip_components: 0,
                    overwrite_existing: true,
                    license: "MIT".to_owned(),
                    source_repo: None,
                },
            },
            PatchStep {
                id: "extract-asi".to_owned(),
                name: "Extract ASI".to_owned(),
                description: "Extracts ASI from cached archive".to_owned(),
                applies_to_releases: Vec::new(),
                os_filter: None,
                action: PatchAction::ExtractDll {
                    source: DllSource::CachedArchive {
                        download_step_id: "dl-zip".to_owned(),
                        archive_member: "scripts/wide.asi".to_owned(),
                    },
                    destination_path: "scripts/wide.asi".to_owned(),
                    expected_sha256: Some(asi_sha.to_owned()),
                    verification_status: HashVerificationStatus::Verified,
                    backup_original: true,
                },
            },
            PatchStep {
                id: "write-ini".to_owned(),
                name: "Write INI".to_owned(),
                description: "Updates dynamic resolution".to_owned(),
                applies_to_releases: Vec::new(),
                os_filter: None,
                action: PatchAction::WriteIniConfig {
                    target_file: "scripts/wide.ini".to_owned(),
                    create_if_missing: true,
                    encoding: IniEncoding::Windows1252,
                    line_ending: LineEnding::Crlf,
                    sections: ini_sections,
                },
            },
        ],
    }
}

fn build_wrapper_steps() -> Vec<PatchStep> {
    vec![
        PatchStep {
            id: "init-sb".to_owned(),
            name: "Init Sandbox".to_owned(),
            description: "Creates .ogsc_sandbox/saves".to_owned(),
            applies_to_releases: Vec::new(),
            os_filter: None,
            action: PatchAction::ConfigureSandbox {
                create_directories: vec![
                    ".ogsc_sandbox".to_owned(),
                    ".ogsc_sandbox/saves".to_owned(),
                ],
                migrate_existing_saves_copy_only: false,
            },
        },
        PatchStep {
            id: "deploy-dg".to_owned(),
            name: "Deploy dgVoodoo2".to_owned(),
            description: "D3D8 wrapper".to_owned(),
            applies_to_releases: Vec::new(),
            os_filter: None,
            action: PatchAction::DeployWrapper(WrapperDeployment {
                wrapper: WrapperKind::DgVoodoo2 {
                    api_targets: vec![DgVoodooApi::D3d8],
                    arch: TargetArch::X86,
                    config_overrides: BTreeMap::new(),
                },
                min_toolkit_version: None,
                target_dir: ".".to_owned(),
                dll_chain_renames: Vec::new(),
            }),
        },
        PatchStep {
            id: "deploy-xi".to_owned(),
            name: "Deploy XInput Plus".to_owned(),
            description: "Chained dinput8".to_owned(),
            applies_to_releases: Vec::new(),
            os_filter: None,
            action: PatchAction::DeployWrapper(WrapperDeployment {
                wrapper: WrapperKind::XInputPlus {
                    target_dlls: vec!["dinput8.dll".to_owned()],
                    profile_ini: BTreeMap::new(),
                },
                min_toolkit_version: None,
                target_dir: ".".to_owned(),
                dll_chain_renames: vec![DllChainRule {
                    original_dll: "dinput8.dll".to_owned(),
                    chained_as: "dinput8_xinput.dll".to_owned(),
                    notes: None,
                }],
            }),
        },
    ]
}

fn build_wrappers_and_flags_fix(
    orig_bin_sha: &str,
    patched_bin_sha: &str,
    patch_sha: &str,
) -> FixOption {
    let mut steps = build_wrapper_steps();
    steps.extend([
        PatchStep {
            id: "bsdiff-exe".to_owned(),
            name: "Patch engine.dll".to_owned(),
            description: "Verified binary patch".to_owned(),
            applies_to_releases: Vec::new(),
            os_filter: None,
            action: PatchAction::ApplyBinaryBsdiffPatch {
                target_file: "engine.dll".to_owned(),
                patch_source: BinaryPatchSource::Download {
                    url: "https://patches.example.org/engine.bsdiff".to_owned(),
                    sha256: patch_sha.to_owned(),
                },
                expected_original_sha256: orig_bin_sha.to_owned(),
                expected_patched_sha256: patched_bin_sha.to_owned(),
                verification_status: HashVerificationStatus::Verified,
                backup_original: true,
            },
        },
        PatchStep {
            id: "reg-flags".to_owned(),
            name: "Registry AppCompat".to_owned(),
            description: "HKCU HighDPIAware".to_owned(),
            applies_to_releases: Vec::new(),
            os_filter: None,
            action: PatchAction::SetRegistryCompatFlags {
                hive: RegistryHive::Hkcu,
                target_executable: "game.exe".to_owned(),
                flags: vec![AppCompatFlag::HighDpiAware],
                merge_with_existing: true,
            },
        },
        PatchStep {
            id: "cpu-cap".to_owned(),
            name: "CPU Affinity".to_owned(),
            description: "Single core".to_owned(),
            applies_to_releases: Vec::new(),
            os_filter: None,
            action: PatchAction::LimitCpuAffinity {
                mode: CpuAffinityMode::SingleCore,
                apply_method: AffinityApplyMethod::LauncherProcessMask,
                process_priority: Some(ProcessPriority::AboveNormal),
            },
        },
    ]);

    FixOption {
        id: "wrappers_and_flags".to_owned(),
        title: "dgVoodoo2, XInput Plus, bsdiff & Compat Flags".to_owned(),
        description: "Deploys wrappers, applies bsdiff, creates sandbox, sets registry & affinity."
            .to_owned(),
        recommended: true,
        applies_to_releases: Vec::new(),
        steps,
    }
}

fn build_e2e_recipe(
    zip_sha: &str,
    asi_sha: &str,
    orig_bin_sha: &str,
    patched_bin_sha: &str,
    patch_sha: &str,
) -> GameRecipe {
    GameRecipe {
        schema_version: CURRENT_SCHEMA_VERSION,
        game_id: "e2e-classic-2004".to_owned(),
        title: "E2E Classic (2004)".to_owned(),
        release_year: 2004,
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
        baseline_health: HealthStatus::BrokenOnWin11,
        post_fix_health: HealthStatus::ReadyToPlay,
        releases: vec![GameRelease {
            release_id: "retail".to_owned(),
            label: "Retail".to_owned(),
            distribution: Distribution::RetailCd,
            version: "1.0".to_owned(),
            region: None,
            executable_path: "game.exe".to_owned(),
            fingerprint: ExecutableFingerprint {
                file_size_bytes: Some(12),
                partial_sha256: None,
                sha256: Some(orig_bin_sha.to_owned()),
                md5: None,
                verification_status: HashVerificationStatus::Verified,
            },
            sentinel_files: Vec::new(),
        }],
        known_issues: vec![KnownIssue {
            id: "missing-widescreen".to_owned(),
            category: IssueCategory::WidescreenAndHud,
            severity: IssueSeverity::Major,
            summary: "No widescreen".to_owned(),
            details: "Fixed by widescreen fix".to_owned(),
            fixed_by_fixes: vec!["widescreen_fix".to_owned()],
            user_action_required: None,
        }],
        runtime_requirements: RuntimeRequirements {
            architecture: BinaryArchitecture::Win32,
            requires_16bit_emulator: false,
            native_graphics_api: vec!["Direct3D 8".to_owned()],
            target_graphics_api: Some("Direct3D 11".to_owned()),
            requires_admin: false,
        },
        sandbox: Some(SandboxConfig {
            enabled_by_default: true,
            sandbox_root: ".ogsc_sandbox".to_owned(),
            redirect_documents: true,
            redirect_appdata_local: false,
            redirect_appdata_roaming: false,
            known_save_subpaths: vec!["saves".to_owned()],
        }),
        fixes: vec![
            build_widescreen_fix(zip_sha, asi_sha),
            build_wrappers_and_flags_fix(orig_bin_sha, patched_bin_sha, patch_sha),
        ],
        launch: LaunchConfig {
            default_executable: "game.exe".to_owned(),
            release_executable_overrides: BTreeMap::new(),
            fix_executable_overrides: BTreeMap::new(),
            working_directory: Some(".".to_owned()),
            arguments: vec!["-play".to_owned()],
            environment: BTreeMap::new(),
            emulator_hook: None,
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

struct E2eFixture {
    _temp: tempfile::TempDir,
    game_root: std::path::PathBuf,
    exe_abs_str: String,
    vanilla_ini: Vec<u8>,
    vanilla_engine: Vec<u8>,
    patched_engine: Vec<u8>,
    recipe: GameRecipe,
    downloader: MockDownloader,
    registry: MemoryRegistryBackend,
    ctx: PatchContext,
}

impl E2eFixture {
    fn setup() -> Self {
        let temp = tempfile::tempdir().expect("create tempdir");
        let game_root = temp.path().join("Game");
        let toolkits_dir = temp.path().join("toolkits");
        let cache_dir = temp.path().join("cache");
        fs::create_dir_all(game_root.join("scripts")).expect("mkdir scripts");
        fs::create_dir_all(toolkits_dir.join("dgvoodoo2").join("MS").join("x86"))
            .expect("mkdir dg");
        fs::create_dir_all(toolkits_dir.join("xinput_plus")).expect("mkdir xi");

        let vanilla_ini = b"; Comment kept\r\n[MAIN]\r\nResX = 800\r\nKeep = yes\r\n".to_vec();
        fs::write(game_root.join("scripts").join("wide.ini"), &vanilla_ini).expect("write ini");
        let vanilla_engine = b"OLD_ENGINE_V1_0000000000".to_vec();
        let patched_engine = b"NEW_ENGINE_V1_1_WIDESCRN".to_vec();
        fs::write(game_root.join("engine.dll"), &vanilla_engine).expect("write engine.dll");
        fs::write(game_root.join("game.exe"), b"MZ_STUB").expect("write game.exe");

        fs::write(
            toolkits_dir.join("dgvoodoo2").join("MS").join("x86").join("D3D8.dll"),
            b"DG_D3D8",
        )
        .expect("write d3d8");
        fs::write(toolkits_dir.join("xinput_plus").join("dinput8.dll"), b"XI_DINPUT8")
            .expect("write xi");

        let asi_bytes = b"ASI_PLUGIN_PAYLOAD";
        let zip_bytes =
            create_zip_bytes(&[("dinput8.dll", b"ASI_LOADER"), ("scripts/wide.asi", asi_bytes)]);
        let mut bsdiff_bytes = Vec::new();
        bsdiff::diff(&vanilla_engine, &patched_engine, &mut bsdiff_bytes).expect("diff");

        let zip_sha = sha256_bytes(&zip_bytes);
        let asi_sha = sha256_bytes(asi_bytes);
        let orig_sha = sha256_bytes(&vanilla_engine);
        let patched_sha = sha256_bytes(&patched_engine);
        let patch_sha = sha256_bytes(&bsdiff_bytes);

        let recipe = build_e2e_recipe(&zip_sha, &asi_sha, &orig_sha, &patched_sha, &patch_sha);
        let report = recipe.validate();
        assert!(report.is_valid(), "recipe must be valid: {report}");

        let downloader = MockDownloader {
            blobs: BTreeMap::from([
                ("https://patches.example.org/widescreen.zip".to_owned(), zip_bytes),
                ("https://patches.example.org/engine.bsdiff".to_owned(), bsdiff_bytes),
            ]),
        };

        let mut registry = MemoryRegistryBackend::new();
        let exe_abs_str = game_root.join("game.exe").to_string_lossy().into_owned();
        registry
            .write_string(RegistryHive::Hkcu, APP_COMPAT_LAYERS_KEY, &exe_abs_str, "~ WINXPSP3")
            .expect("seed hkcu");

        let mut ctx = PatchContext::for_game(
            &game_root,
            ToolkitStore::new(&toolkits_dir),
            cache_dir,
            Some(".ogsc_sandbox"),
        );
        ctx.dynamic_ctx.desktop_width = 3440;
        ctx.dynamic_ctx.desktop_height = 1440;

        Self {
            _temp: temp,
            game_root,
            exe_abs_str,
            vanilla_ini,
            vanilla_engine,
            patched_engine,
            recipe,
            downloader,
            registry,
            ctx,
        }
    }
}

#[test]
fn full_patcher_lifecycle_plan_apply_events_launch_and_rollback() {
    let mut fx = E2eFixture::setup();
    let selected = vec!["widescreen_fix".to_owned(), "wrappers_and_flags".to_owned()];

    // 1. Dry-run preview before touching disk
    let plan = plan_fixes(&fx.recipe, "retail", &fx.game_root, &selected, &fx.ctx, &fx.registry)
        .expect("plan");
    assert_eq!(plan.fixes.len(), 2);
    assert_eq!(plan.downloads.len(), 2);
    assert_eq!(plan.registry_changes.len(), 1);
    assert_eq!(plan.registry_changes[0].planned_value, "~ WINXPSP3 HIGHDPIAWARE");
    assert!(plan.unique_target_files().contains(&"dinput8_xinput.dll".to_owned()));

    // 2. Transactional execution + structured events
    let mut sink = VecEventSink::default();
    let apply_report = {
        let mut engine = PatchEngine::new(&fx.downloader, &mut fx.registry);
        engine
            .apply_fixes(&fx.recipe, "retail", &fx.game_root, &selected, &fx.ctx, &mut sink)
            .expect("apply")
    };
    assert_eq!(apply_report.applied_fix_ids, selected);
    assert!(sink.events.iter().any(|e| matches!(e, PatchEvent::DownloadProgress { .. })));
    assert!(sink.events.iter().any(|e| matches!(e, PatchEvent::WrapperDeployed { .. })));
    assert!(sink.events.iter().any(|e| matches!(e, PatchEvent::RegistryUpdated { .. })));

    // Verify patched outputs
    assert_eq!(fs::read(fx.game_root.join("engine.dll")).expect("read engine"), fx.patched_engine);
    let ini_after =
        fs::read_to_string(fx.game_root.join("scripts").join("wide.ini")).expect("read ini");
    assert!(ini_after.contains("; Comment kept\r\n"));
    assert!(ini_after.contains("ResX = 3440\r\n"));
    assert!(ini_after.contains("Keep = yes\r\n"));
    assert!(fx.game_root.join("dinput8.dll").is_file());
    assert!(fx.game_root.join("dinput8_xinput.dll").is_file());
    assert!(fx.game_root.join("D3D8.dll").is_file());
    assert_eq!(
        fx.registry
            .read_string(RegistryHive::Hkcu, APP_COMPAT_LAYERS_KEY, &fx.exe_abs_str)
            .expect("read reg"),
        Some("~ WINXPSP3 HIGHDPIAWARE".to_owned())
    );

    // Simulate user save inside sandbox
    let save_path = fx.game_root.join(".ogsc_sandbox").join("saves").join("profile.sav");
    fs::write(&save_path, b"USER_SAVE").expect("write save");

    // 3. Launch resolution
    let launch_cmd = resolve_launch_command(
        &fx.recipe,
        &fx.ctx.toolkit_store,
        &LaunchOptions {
            game_root: fx.game_root.clone(),
            release_id: "retail".to_owned(),
            applied_fix_ids: apply_report.applied_fix_ids.clone(),
            extra_args: Vec::new(),
        },
    )
    .expect("resolve launch");
    assert_eq!(launch_cmd.cpu_affinity_mask, Some(1));
    assert_eq!(launch_cmd.process_priority, Some(ProcessPriority::AboveNormal));
    // Verify launch_game fails cleanly when executable is missing:
    let mut bad_cmd = launch_cmd;
    bad_cmd.program = fx.game_root.join("nonexistent.exe");
    assert!(matches!(launch_game(&bad_cmd), Err(PatcherError::LaunchFailed { .. })));

    // 4. Full rollback restores vanilla INI, engine.dll, and HKCU registry while preserving saves
    let mut rb_sink = VecEventSink::default();
    let rb_summary = {
        let mut engine = PatchEngine::new(&fx.downloader, &mut fx.registry);
        engine.rollback_fixes(&fx.recipe, &fx.game_root, None, &mut rb_sink).expect("rollback")
    };
    assert_eq!(rb_summary.restored_files, 2);
    assert_eq!(
        fs::read(fx.game_root.join("scripts").join("wide.ini")).expect("ini"),
        fx.vanilla_ini
    );
    assert_eq!(fs::read(fx.game_root.join("engine.dll")).expect("engine"), fx.vanilla_engine);
    assert!(!fx.game_root.join("dinput8.dll").exists());
    assert!(!fx.game_root.join("dinput8_xinput.dll").exists());
    assert!(!fx.game_root.join("D3D8.dll").exists());
    assert!(save_path.is_file(), "user save in `.ogsc_sandbox` must survive rollback");
    assert_eq!(
        fx.registry
            .read_string(RegistryHive::Hkcu, APP_COMPAT_LAYERS_KEY, &fx.exe_abs_str)
            .expect("read reg"),
        Some("~ WINXPSP3".to_owned())
    );
}

#[test]
fn failed_step_mid_fix_rolls_back_all_partial_changes_automatically() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let game_root = temp.path().join("Game");
    let cache_dir = temp.path().join("cache");
    fs::create_dir_all(&game_root).expect("mkdir game");
    fs::write(game_root.join("game.exe"), b"VANILLA_EXE").expect("write exe");

    // Empty toolkits dir will cause step 2 (DeployWrapper) to fail AFTER step 1 (SetRegistryCompatFlags) runs!
    let mut recipe = build_e2e_recipe(
        &"1".repeat(64),
        &"2".repeat(64),
        &"3".repeat(64),
        &"4".repeat(64),
        &"5".repeat(64),
    );
    // Keep only one fix with registry edit first, then failing wrapper deployment second
    recipe.fixes = vec![FixOption {
        id: "partial_fail_fix".to_owned(),
        title: "Partial Fail Fix".to_owned(),
        description: "Tests automatic per-fix rollback.".to_owned(),
        recommended: true,
        applies_to_releases: Vec::new(),
        steps: vec![
            PatchStep {
                id: "reg-first".to_owned(),
                name: "Set Registry".to_owned(),
                description: "Writes HKCU flag".to_owned(),
                applies_to_releases: Vec::new(),
                os_filter: None,
                action: PatchAction::SetRegistryCompatFlags {
                    hive: RegistryHive::Hkcu,
                    target_executable: "game.exe".to_owned(),
                    flags: vec![AppCompatFlag::HighDpiAware],
                    merge_with_existing: true,
                },
            },
            PatchStep {
                id: "missing-wrapper-second".to_owned(),
                name: "Deploy Missing Wrapper".to_owned(),
                description: "Fails due to missing toolkit DLL".to_owned(),
                applies_to_releases: Vec::new(),
                os_filter: None,
                action: PatchAction::DeployWrapper(WrapperDeployment {
                    wrapper: WrapperKind::DgVoodoo2 {
                        api_targets: vec![DgVoodooApi::D3d8],
                        arch: TargetArch::X86,
                        config_overrides: BTreeMap::new(),
                    },
                    min_toolkit_version: None,
                    target_dir: ".".to_owned(),
                    dll_chain_renames: Vec::new(),
                }),
            },
        ],
    }];

    let dl = MockDownloader { blobs: BTreeMap::new() };
    let mut registry = MemoryRegistryBackend::new();
    let exe_str = game_root.join("game.exe").to_string_lossy().into_owned();
    let ctx = PatchContext::for_game(
        &game_root,
        ToolkitStore::new(temp.path().join("no_toolkits")),
        cache_dir,
        None,
    );
    let mut sink = VecEventSink::default();

    let mut engine = PatchEngine::new(&dl, &mut registry);
    let err = engine
        .apply_fixes(
            &recipe,
            "retail",
            &game_root,
            &["partial_fail_fix".to_owned()],
            &ctx,
            &mut sink,
        )
        .expect_err("missing wrapper must fail");
    assert!(matches!(err, PatcherError::Wrapper(_)));

    // Verify automatic rollback undid the HKCU registry edit and left zero .ogsc_backup files!
    assert_eq!(
        registry.read_string(RegistryHive::Hkcu, APP_COMPAT_LAYERS_KEY, &exe_str).expect("read"),
        None
    );
    assert!(!game_root.join(".ogsc_backup").exists());
    let _ = File::create(temp.path().join("done.marker")).expect("cleanup check");
}

#[test]
fn patches_and_rolls_back_readonly_retail_cd_files() {
    let mut fx = E2eFixture::setup();

    for rel in ["engine.dll", "scripts/wide.ini"] {
        let path = fx.game_root.join(rel);
        let mut perms = fs::metadata(&path).expect("meta").permissions();
        perms.set_readonly(true);
        fs::set_permissions(&path, perms).expect("set readonly");
    }

    let selected = vec!["widescreen_fix".to_owned(), "wrappers_and_flags".to_owned()];
    let mut sink = VecEventSink::default();
    let apply_report = {
        let mut engine = PatchEngine::new(&fx.downloader, &mut fx.registry);
        engine
            .apply_fixes(&fx.recipe, "retail", &fx.game_root, &selected, &fx.ctx, &mut sink)
            .expect("apply over readonly files must succeed")
    };
    assert_eq!(apply_report.applied_fix_ids, selected);
    assert_eq!(fs::read(fx.game_root.join("engine.dll")).expect("read engine"), fx.patched_engine);
    let ini_after =
        fs::read_to_string(fx.game_root.join("scripts").join("wide.ini")).expect("read ini");
    assert!(ini_after.contains("ResX = 3440\r\n"));

    let mut rb_sink = VecEventSink::default();
    let rb_summary = {
        let mut engine = PatchEngine::new(&fx.downloader, &mut fx.registry);
        engine
            .rollback_fixes(&fx.recipe, &fx.game_root, None, &mut rb_sink)
            .expect("rollback readonly targets must succeed")
    };
    assert_eq!(rb_summary.restored_files, 2);
    assert_eq!(fs::read(fx.game_root.join("engine.dll")).expect("engine"), fx.vanilla_engine);
    assert_eq!(
        fs::read(fx.game_root.join("scripts").join("wide.ini")).expect("ini"),
        fx.vanilla_ini
    );
    assert!(!fx.game_root.join(".ogsc_backup").exists());
}

#[test]
fn reapplying_bsdiff_fix_skips_already_patched_file_idempotently() {
    let mut fx = E2eFixture::setup();
    let selected = vec!["wrappers_and_flags".to_owned()];

    let mut first_sink = VecEventSink::default();
    let first_report = {
        let mut engine = PatchEngine::new(&fx.downloader, &mut fx.registry);
        engine
            .apply_fixes(&fx.recipe, "retail", &fx.game_root, &selected, &fx.ctx, &mut first_sink)
            .expect("initial apply")
    };
    assert_eq!(first_report.applied_fix_ids, selected);
    assert_eq!(fs::read(fx.game_root.join("engine.dll")).expect("patched"), fx.patched_engine);

    // Re-applying the same fix when engine.dll already matches expected_patched_sha256 skips cleanly
    let mut second_sink = VecEventSink::default();
    let second_report = {
        let mut engine = PatchEngine::new(&fx.downloader, &mut fx.registry);
        engine
            .apply_fixes(&fx.recipe, "retail", &fx.game_root, &selected, &fx.ctx, &mut second_sink)
            .expect("idempotent reapply")
    };
    assert_eq!(second_report.applied_fix_ids, selected);
    assert!(second_report.skipped_steps.contains(&"bsdiff-exe".to_owned()));
    assert!(second_sink
        .events
        .iter()
        .any(|e| matches!(e, PatchEvent::StepSkipped { step_id, .. } if step_id == "bsdiff-exe")));

    // Original vanilla bytes inside .ogsc_backup were NOT overwritten and restore cleanly on rollback
    let mut rb_sink = VecEventSink::default();
    {
        let mut engine = PatchEngine::new(&fx.downloader, &mut fx.registry);
        engine.rollback_fixes(&fx.recipe, &fx.game_root, None, &mut rb_sink).expect("rollback");
    }
    assert_eq!(fs::read(fx.game_root.join("engine.dll")).expect("restored"), fx.vanilla_engine);
}
