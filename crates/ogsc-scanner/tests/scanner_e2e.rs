//! End-to-end integration test exercising scanning, hashing, library persistence, backup creation,
//! partial/full rollback, and sandbox save preservation across `ogsc-recipes` and `ogsc-scanner`.

use std::collections::BTreeMap;
use std::fs;

use ogsc_recipes::{
    BinaryArchitecture, Distribution, ExecutableFingerprint, FixOption, GameRecipe, GameRelease,
    HashVerificationStatus, HealthStatus, IniEncoding, IniValue, IssueCategory, IssueSeverity,
    KnownIssue, LaunchConfig, LineEnding, PartialHash, PatchAction, PatchStep, RecipeDatabase,
    RecipeMetadata, RollbackPlan, RollbackStrategy, RuntimeRequirements, SandboxConfig,
    CURRENT_SCHEMA_VERSION,
};
use ogsc_scanner::{
    compute_fingerprint, BackupManager, GameScanner, GameStatus, GeneratedFingerprint,
    LibraryStore, MatchConfidence, ScanOptions, DEFAULT_PARTIAL_HASH_BYTES,
};

fn build_test_fixes() -> Vec<FixOption> {
    let mut sections = BTreeMap::new();
    let mut main = BTreeMap::new();
    main.insert("ResX".to_owned(), IniValue::Integer(2560));
    main.insert("ResY".to_owned(), IniValue::Integer(1440));
    sections.insert("MAIN".to_owned(), main);

    vec![
        FixOption {
            id: "widescreen_fix".to_owned(),
            title: "Widescreen Fix".to_owned(),
            description: "Configures native resolution.".to_owned(),
            recommended: true,
            applies_to_releases: Vec::new(),
            steps: vec![PatchStep {
                id: "write-ini".to_owned(),
                name: "Write Widescreen INI".to_owned(),
                description: "Writes 2560x1440".to_owned(),
                applies_to_releases: Vec::new(),
                os_filter: None,
                action: PatchAction::WriteIniConfig {
                    target_file: "scripts/NFSMostWanted.WidescreenFix.ini".to_owned(),
                    create_if_missing: true,
                    encoding: IniEncoding::Utf8,
                    line_ending: LineEnding::Crlf,
                    sections,
                },
            }],
        },
        FixOption {
            id: "portable_sandbox".to_owned(),
            title: "Portable Save Sandbox".to_owned(),
            description: "Redirects saves into `.ogsc_sandbox/saves`.".to_owned(),
            recommended: true,
            applies_to_releases: Vec::new(),
            steps: vec![PatchStep {
                id: "create-sandbox".to_owned(),
                name: "Create Sandbox".to_owned(),
                description: "Creates `.ogsc_sandbox/saves`".to_owned(),
                applies_to_releases: Vec::new(),
                os_filter: None,
                action: PatchAction::ConfigureSandbox {
                    create_directories: vec![
                        ".ogsc_sandbox".to_owned(),
                        ".ogsc_sandbox/saves".to_owned(),
                    ],
                    migrate_existing_saves_copy_only: false,
                },
            }],
        },
    ]
}

fn build_test_nfs_recipe(fp: &GeneratedFingerprint) -> GameRecipe {
    GameRecipe {
        schema_version: CURRENT_SCHEMA_VERSION,
        game_id: "nfs-most-wanted-2005".to_owned(),
        title: "Need for Speed: Most Wanted (2005)".to_owned(),
        release_year: 2005,
        developer: Some("EA Canada".to_owned()),
        publisher: Some("Electronic Arts".to_owned()),
        metadata: RecipeMetadata {
            recipe_version: "1.0.0".to_owned(),
            authors: vec!["Integration Test".to_owned()],
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
            release_id: "retail-cd-v1.3".to_owned(),
            label: "Retail CD v1.3".to_owned(),
            distribution: Distribution::RetailCd,
            version: "1.3".to_owned(),
            region: Some("Global".to_owned()),
            executable_path: "speed.exe".to_owned(),
            fingerprint: ExecutableFingerprint {
                file_size_bytes: Some(fp.file_size_bytes),
                partial_sha256: Some(PartialHash {
                    offset: fp.partial_offset,
                    length: fp.partial_length,
                    sha256: fp.partial_sha256.clone(),
                }),
                sha256: Some(fp.sha256.clone()),
                md5: Some(fp.md5.clone()),
                verification_status: HashVerificationStatus::Verified,
            },
            sentinel_files: vec!["GLOBAL/GlobalB.lzc".to_owned()],
        }],
        known_issues: vec![KnownIssue {
            id: "widescreen-fov".to_owned(),
            category: IssueCategory::WidescreenAndHud,
            severity: IssueSeverity::Major,
            summary: "Missing widescreen resolutions".to_owned(),
            details: "Fixed by Widescreen Fix.".to_owned(),
            fixed_by_fixes: vec!["widescreen_fix".to_owned()],
            user_action_required: None,
        }],
        runtime_requirements: RuntimeRequirements {
            architecture: BinaryArchitecture::Win32,
            requires_16bit_emulator: false,
            native_graphics_api: vec!["Direct3D 9".to_owned()],
            target_graphics_api: Some("Direct3D 11".to_owned()),
            requires_admin: false,
        },
        sandbox: Some(SandboxConfig {
            enabled_by_default: true,
            sandbox_root: ".ogsc_sandbox".to_owned(),
            redirect_documents: false,
            redirect_appdata_local: false,
            redirect_appdata_roaming: false,
            known_save_subpaths: vec!["saves".to_owned()],
        }),
        fixes: build_test_fixes(),
        launch: LaunchConfig {
            default_executable: "speed.exe".to_owned(),
            release_executable_overrides: BTreeMap::new(),
            fix_executable_overrides: BTreeMap::new(),
            working_directory: Some(".".to_owned()),
            arguments: Vec::new(),
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

#[test]
fn end_to_end_scan_library_backup_and_rollback_preserves_vanilla_and_saves() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let games_root = temp.path().join("Games");
    let nfs_dir = games_root.join("NFS_Most_Wanted");
    let scripts_dir = nfs_dir.join("scripts");
    let global_dir = nfs_dir.join("GLOBAL");
    let recipes_dir = temp.path().join("recipes");

    fs::create_dir_all(&scripts_dir).expect("mkdir scripts");
    fs::create_dir_all(&global_dir).expect("mkdir GLOBAL");
    fs::create_dir_all(&recipes_dir).expect("mkdir recipes");

    let vanilla_exe_bytes = b"MZ_SYNTHETIC_SPEED_EXE_V1.3_RETAIL_BINARY";
    let exe_path = nfs_dir.join("speed.exe");
    fs::write(&exe_path, vanilla_exe_bytes).expect("write speed.exe");
    fs::write(global_dir.join("GlobalB.lzc"), b"SENTINEL_GLOBAL_DATA").expect("write sentinel");

    let vanilla_ini_bytes = b"[MAIN]\r\nResX = 800\r\nResY = 600\r\n";
    let ini_path = scripts_dir.join("NFSMostWanted.WidescreenFix.ini");
    fs::write(&ini_path, vanilla_ini_bytes).expect("write vanilla ini");

    let fp = compute_fingerprint(&exe_path, DEFAULT_PARTIAL_HASH_BYTES).expect("fingerprint exe");
    let recipe = build_test_nfs_recipe(&fp);

    let recipe_json = serde_json::to_string_pretty(&recipe).expect("serialize recipe");
    fs::write(recipes_dir.join("nfs.json"), recipe_json).expect("write recipe file");

    // 1. Load recipe DB under strict mode and scan the synthetic games folder.
    let loaded = RecipeDatabase::load_from_dir(&recipes_dir, true)
        .expect("strict load should pass with verified hash");
    let scanner = GameScanner::new(&loaded.db, ScanOptions::default());
    let scan_report = scanner.scan_dir(&games_root).expect("scan games_root");

    assert_eq!(scan_report.matches.len(), 1);
    let m = &scan_report.matches[0];
    assert_eq!(m.confidence, MatchConfidence::ExactSha256);
    assert_eq!(m.game_id, "nfs-most-wanted-2005");
    assert_eq!(m.release_id, "retail-cd-v1.3");

    // 2. Import scan result into library.json (starts as NeedsFixes).
    let lib_path = temp.path().join("state").join("library.json");
    let mut library = LibraryStore::open(&lib_path).expect("open library");
    library.add_scan_folder(&games_root).expect("register scan folder");
    let entry_id = library.upsert_scan_match(m).entry_id.clone();
    assert_eq!(library.get(&entry_id).expect("entry").status, GameStatus::NeedsFixes);

    // 3. Simulate Patch Advisor selection -> back up before mutating files.
    let mut backup = BackupManager::open(&m.game_root, ".ogsc_backup", &m.game_id, &m.release_id)
        .expect("open backup");
    backup
        .backup_file_before_write("widescreen_fix", "scripts/NFSMostWanted.WidescreenFix.ini")
        .expect("backup ini");
    backup
        .backup_file_before_write("widescreen_fix", "dinput8.dll")
        .expect("record new dinput8.dll");
    backup
        .record_created_directory("portable_sandbox", ".ogsc_sandbox")
        .expect("record sandbox dir");
    backup
        .record_created_directory("portable_sandbox", ".ogsc_sandbox/saves")
        .expect("record saves dir");

    fs::write(&ini_path, b"[MAIN]\r\nResX = 2560\r\nResY = 1440\r\n").expect("write patched ini");
    fs::write(nfs_dir.join("dinput8.dll"), b"ASI_LOADER_DLL").expect("write dinput8.dll");
    fs::create_dir_all(nfs_dir.join(".ogsc_sandbox").join("saves")).expect("mkdir saves");
    let user_save_path = nfs_dir.join(".ogsc_sandbox").join("saves").join("MyCareer.sav");
    fs::write(&user_save_path, b"CAREER_SAVE_DATA").expect("write user save");

    library
        .record_applied_fixes(
            &entry_id,
            vec!["widescreen_fix".to_owned(), "portable_sandbox".to_owned()],
            vec!["dgvoodoo2_wrapper".to_owned()],
        )
        .expect("record applied fixes");
    library.save().expect("save library");

    // 4. Reload library from disk and confirm Patched state + fix lists survived.
    let mut reloaded_lib = LibraryStore::open(&lib_path).expect("reload library");
    let persisted = reloaded_lib.get(&entry_id).expect("persisted entry");
    assert_eq!(persisted.status, GameStatus::Patched);
    assert_eq!(persisted.applied_fix_ids, vec!["widescreen_fix", "portable_sandbox"]);
    assert_eq!(persisted.declined_fix_ids, vec!["dgvoodoo2_wrapper"]);

    // 5. Roll back all fixes via BackupManager::load_existing while protecting `.ogsc_sandbox`.
    let mut loaded_backup =
        BackupManager::load_existing(&nfs_dir, ".ogsc_backup").expect("load existing backup");
    let preserves = vec![".ogsc_sandbox".to_owned()];
    let summary = loaded_backup.rollback_all(&preserves).expect("rollback all");

    assert_eq!(summary.restored_files, 1);
    assert_eq!(summary.deleted_files, 1);
    assert_eq!(fs::read(&ini_path).expect("read restored ini"), vanilla_ini_bytes);
    assert_eq!(fs::read(&exe_path).expect("read exe"), vanilla_exe_bytes);
    assert!(!nfs_dir.join("dinput8.dll").exists());
    assert!(user_save_path.is_file(), "user saves inside `.ogsc_sandbox` must survive rollback");
    assert!(!nfs_dir.join(".ogsc_backup").exists());

    reloaded_lib.mark_rolled_back(&entry_id, Vec::new()).expect("mark rolled back");
    reloaded_lib.save().expect("save rolled back library");
    assert_eq!(reloaded_lib.get(&entry_id).expect("entry").status, GameStatus::NeedsFixes);
}
