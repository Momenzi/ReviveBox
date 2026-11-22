//! Multi-stage parallel directory scanner and executable fingerprint matcher.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use ogsc_recipes::{GameRecipe, GameRelease, RecipeDatabase};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::error::ScannerError;
use crate::hasher::{compute_hashes, compute_partial_sha256, ComputedHashes};

/// Configurable directory traversal and matching options for [`GameScanner`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOptions {
    /// Maximum directory recursion depth under each scan root.
    pub max_depth: usize,
    /// Whether to follow filesystem symlinks / directory junctions during scanning.
    pub follow_symlinks: bool,
    /// Case-insensitive directory names skipped during traversal (such as `.ogsc_backup` or `Windows`).
    pub skip_dir_names: Vec<String>,
    /// Allow filename + sentinel matching when exact binary hashes differ or are template placeholders.
    pub include_heuristic_matches: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_depth: 8,
            follow_symlinks: false,
            skip_dir_names: vec![
                ".ogsc_backup".to_owned(),
                ".ogsc_sandbox".to_owned(),
                ".git".to_owned(),
                "node_modules".to_owned(),
                "$recycle.bin".to_owned(),
                "system volume information".to_owned(),
                "windows".to_owned(),
            ],
            include_heuristic_matches: true,
        }
    }
}

/// Strength of identification between a local executable and a recipe release.
///
/// Ordered from weakest to strongest so derived [`Ord`] ranks higher confidence greater.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchConfidence {
    /// Matched by relative executable path and all declared sentinel files (hash absent or differed).
    FilenameAndSentinelsOnly,
    /// Matched by exact file size plus partial SHA-256 slice (when no full hash is specified).
    PartialSha256AndSize,
    /// Matched by exact full-file MD5 digest (and file size when specified).
    ExactMd5,
    /// Matched by exact full-file SHA-256 digest (and file size when specified).
    ExactSha256,
}

/// A single recognized legacy game installation found on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanMatch {
    /// Matched recipe `game_id`.
    pub game_id: String,
    /// Display title from the matched recipe.
    pub title: String,
    /// Matched release slug within the recipe.
    pub release_id: String,
    /// Human-readable release label.
    pub release_label: String,
    /// Resolved root directory of the game installation.
    pub game_root: PathBuf,
    /// Full path to the matched executable file.
    pub executable_path: PathBuf,
    /// Confidence tier of the match.
    pub confidence: MatchConfidence,
    /// Observed size, SHA-256, and MD5 of the local executable (computed on match).
    pub observed_hashes: Option<ComputedHashes>,
    /// Sentinel files declared by the release that were found under `game_root`.
    pub matched_sentinels: Vec<String>,
    /// Sentinel files declared by the release that were missing under `game_root`.
    pub missing_sentinels: Vec<String>,
}

/// Aggregated statistics, matches, and non-fatal traversal warnings from a scan run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanReport {
    /// Normalized root directories scanned.
    pub scanned_roots: Vec<PathBuf>,
    /// Total regular files encountered during traversal.
    pub files_inspected: usize,
    /// Files whose basename matched at least one known recipe executable.
    pub candidate_executables: usize,
    /// Wall-clock duration of the scan in milliseconds.
    pub elapsed_ms: u64,
    /// Deduplicated recognized game installations sorted deterministically by title and path.
    pub matches: Vec<ScanMatch>,
    /// Non-fatal traversal or unreadable-file diagnostics encountered during scanning.
    pub warnings: Vec<String>,
}

/// Multi-stage parallel directory scanner matching local legacy game binaries against a [`RecipeDatabase`].
pub struct GameScanner<'a> {
    db: &'a RecipeDatabase,
    options: ScanOptions,
}

impl<'a> GameScanner<'a> {
    /// Creates a new [`GameScanner`] backed by `db` and configured with `options`.
    pub fn new(db: &'a RecipeDatabase, options: ScanOptions) -> Self {
        Self { db, options }
    }

    /// Recursively scans a single directory tree for known legacy games.
    pub fn scan_dir(&self, root: &Path) -> Result<ScanReport, ScannerError> {
        self.scan_roots(&[root.to_path_buf()])
    }

    /// Recursively scans multiple directory roots and deduplicates recognized game installations.
    pub fn scan_roots(&self, roots: &[PathBuf]) -> Result<ScanReport, ScannerError> {
        let start = Instant::now();
        let mut normalized_roots = Vec::with_capacity(roots.len());
        for root in roots {
            if !root.is_dir() {
                return Err(ScannerError::ScanRootNotFound { path: root.clone() });
            }
            normalized_roots.push(clean_absolute_path(root)?);
        }

        let mut files_inspected = 0_usize;
        let mut candidates = Vec::new();
        let mut warnings = Vec::new();

        for root in &normalized_roots {
            let walker = WalkDir::new(root)
                .max_depth(self.options.max_depth)
                .follow_links(self.options.follow_symlinks)
                .into_iter()
                .filter_entry(|entry| {
                    !entry.file_type().is_dir()
                        || entry.depth() == 0
                        || !self.should_skip_dir(entry.file_name().to_string_lossy().as_ref())
                });

            for entry_res in walker {
                match entry_res {
                    Ok(entry) if entry.file_type().is_file() => {
                        files_inspected += 1;
                        let name = entry.file_name().to_string_lossy();
                        if !self.db.find_by_executable_name(&name).is_empty() {
                            candidates.push(entry.into_path());
                        }
                    }
                    Ok(_) => {}
                    Err(err) => warnings.push(format!("walkdir warning: {err}")),
                }
            }
        }

        let candidate_executables = candidates.len();
        let eval_results: Vec<(Option<ScanMatch>, Vec<String>)> = candidates
            .par_iter()
            .map(|exe_path| match self.evaluate_candidate(exe_path) {
                Ok(found) => (found, Vec::new()),
                Err(err) => (None, vec![format!("skipping `{}`: {err}", exe_path.display())]),
            })
            .collect();

        let mut raw_matches = Vec::new();
        for (maybe_match, mut eval_warnings) in eval_results {
            warnings.append(&mut eval_warnings);
            if let Some(m) = maybe_match {
                raw_matches.push(m);
            }
        }

        let matches = deduplicate_and_sort_matches(raw_matches);
        let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

        Ok(ScanReport {
            scanned_roots: normalized_roots,
            files_inspected,
            candidate_executables,
            elapsed_ms,
            matches,
            warnings,
        })
    }

    /// Inspects a single executable path and returns the strongest matching recipe release, if any.
    pub fn identify_executable(&self, exe_path: &Path) -> Result<Option<ScanMatch>, ScannerError> {
        let meta = fs::metadata(exe_path)
            .map_err(|source| ScannerError::Io { path: exe_path.to_path_buf(), source })?;
        if !meta.is_file() {
            return Ok(None);
        }
        let clean = clean_absolute_path(exe_path)?;
        self.evaluate_candidate(&clean)
    }

    fn should_skip_dir(&self, dir_name: &str) -> bool {
        self.options.skip_dir_names.iter().any(|skip| skip.eq_ignore_ascii_case(dir_name))
    }

    fn evaluate_candidate(&self, exe_path: &Path) -> Result<Option<ScanMatch>, ScannerError> {
        let Some(file_name) = exe_path.file_name().and_then(|n| n.to_str()) else {
            return Ok(None);
        };
        let refs = self.db.find_by_executable_name(file_name);
        if refs.is_empty() {
            return Ok(None);
        }

        let file_size = fs::metadata(exe_path)
            .map_err(|source| ScannerError::Io { path: exe_path.to_path_buf(), source })?
            .len();

        let mut cached_hashes: Option<ComputedHashes> = None;
        let mut best: Option<ScanMatch> = None;

        for release_ref in refs {
            let Some(recipe) = self.db.get_by_id(&release_ref.game_id) else {
                continue;
            };
            let Some(release) =
                recipe.releases.iter().find(|rel| rel.release_id == release_ref.release_id)
            else {
                continue;
            };
            let Some(game_root) = derive_game_root(exe_path, &release.executable_path) else {
                continue;
            };

            let (matched_sentinels, missing_sentinels) =
                evaluate_sentinels(&game_root, &release.sentinel_files);

            let Some(confidence) = self.classify_match(
                exe_path,
                file_size,
                release,
                &missing_sentinels,
                &mut cached_hashes,
            )?
            else {
                continue;
            };

            if cached_hashes.is_none() {
                cached_hashes = Some(compute_hashes(exe_path)?);
            }

            let candidate = build_scan_match(
                recipe,
                release,
                game_root,
                exe_path.to_path_buf(),
                confidence,
                cached_hashes.clone(),
                matched_sentinels,
                missing_sentinels,
            );

            if best.as_ref().is_none_or(|prev| is_better_match(&candidate, prev)) {
                best = Some(candidate);
            }
        }

        Ok(best)
    }

    fn classify_match(
        &self,
        exe_path: &Path,
        file_size: u64,
        release: &GameRelease,
        missing_sentinels: &[String],
        cached_hashes: &mut Option<ComputedHashes>,
    ) -> Result<Option<MatchConfidence>, ScannerError> {
        let fp = &release.fingerprint;
        let size_matches = fp.file_size_bytes.is_none_or(|expected| expected == file_size);

        if size_matches {
            let mut partial_ok = true;
            if let Some(partial) = &fp.partial_sha256 {
                let actual_partial =
                    compute_partial_sha256(exe_path, partial.offset, partial.length)?;
                partial_ok = actual_partial == partial.sha256;
            }

            if partial_ok {
                if fp.sha256.is_some() || fp.md5.is_some() {
                    if cached_hashes.is_none() {
                        *cached_hashes = Some(compute_hashes(exe_path)?);
                    }
                    let actual = cached_hashes.as_ref().expect("just computed");
                    if fp.sha256.as_deref() == Some(actual.sha256.as_str()) {
                        return Ok(Some(MatchConfidence::ExactSha256));
                    }
                    if fp.md5.as_deref() == Some(actual.md5.as_str()) {
                        return Ok(Some(MatchConfidence::ExactMd5));
                    }
                } else if fp.partial_sha256.is_some() {
                    return Ok(Some(MatchConfidence::PartialSha256AndSize));
                }
            }
        }

        if self.options.include_heuristic_matches && missing_sentinels.is_empty() {
            return Ok(Some(MatchConfidence::FilenameAndSentinelsOnly));
        }

        Ok(None)
    }
}

#[allow(clippy::too_many_arguments)]
fn build_scan_match(
    recipe: &GameRecipe,
    release: &GameRelease,
    game_root: PathBuf,
    executable_path: PathBuf,
    confidence: MatchConfidence,
    observed_hashes: Option<ComputedHashes>,
    matched_sentinels: Vec<String>,
    missing_sentinels: Vec<String>,
) -> ScanMatch {
    ScanMatch {
        game_id: recipe.game_id.clone(),
        title: recipe.title.clone(),
        release_id: release.release_id.clone(),
        release_label: release.label.clone(),
        game_root,
        executable_path,
        confidence,
        observed_hashes,
        matched_sentinels,
        missing_sentinels,
    }
}

fn derive_game_root(exe_path: &Path, recipe_exe_rel: &str) -> Option<PathBuf> {
    let rel_parts: Vec<&str> =
        recipe_exe_rel.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    if rel_parts.is_empty() {
        return None;
    }

    let mut current = exe_path;
    for expected in rel_parts.iter().rev() {
        let actual = current.file_name()?.to_str()?;
        if !actual.eq_ignore_ascii_case(expected) {
            return None;
        }
        current = current.parent()?;
    }

    Some(current.to_path_buf())
}

fn evaluate_sentinels(game_root: &Path, sentinels: &[String]) -> (Vec<String>, Vec<String>) {
    let mut matched = Vec::new();
    let mut missing = Vec::new();
    for sentinel in sentinels {
        let mut full = game_root.to_path_buf();
        for seg in sentinel.split(['/', '\\']).filter(|s| !s.is_empty()) {
            full.push(seg);
        }
        if full.is_file() {
            matched.push(sentinel.clone());
        } else {
            missing.push(sentinel.clone());
        }
    }
    (matched, missing)
}

fn deduplicate_and_sort_matches(matches: Vec<ScanMatch>) -> Vec<ScanMatch> {
    let mut deduped: HashMap<(String, String), ScanMatch> = HashMap::new();
    for m in matches {
        let key = (normalized_path_key(&m.game_root), m.game_id.clone());
        match deduped.get(&key) {
            Some(existing) if !is_better_match(&m, existing) => {}
            _ => {
                deduped.insert(key, m);
            }
        }
    }

    let mut out: Vec<ScanMatch> = deduped.into_values().collect();
    out.sort_by(|a, b| {
        a.title
            .cmp(&b.title)
            .then_with(|| a.game_id.cmp(&b.game_id))
            .then_with(|| normalized_path_key(&a.game_root).cmp(&normalized_path_key(&b.game_root)))
    });
    out
}

fn is_better_match(candidate: &ScanMatch, current: &ScanMatch) -> bool {
    match candidate.confidence.cmp(&current.confidence) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => {
            match candidate.matched_sentinels.len().cmp(&current.matched_sentinels.len()) {
                Ordering::Greater => true,
                Ordering::Less => false,
                Ordering::Equal => candidate.release_id < current.release_id,
            }
        }
    }
}

pub(crate) fn clean_absolute_path(path: &Path) -> Result<PathBuf, ScannerError> {
    let abs = std::path::absolute(path)
        .map_err(|source| ScannerError::Io { path: path.to_path_buf(), source })?;
    let mut cleaned = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !cleaned.pop() {
                    cleaned.push(comp);
                }
            }
            _ => cleaned.push(comp),
        }
    }
    Ok(cleaned)
}

pub(crate) fn normalized_path_key(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let trimmed = s.trim_end_matches('/');
    if cfg!(windows) {
        trimmed.to_ascii_lowercase()
    } else {
        trimmed.to_owned()
    }
}

#[allow(dead_code)]
fn _assert_btreemap_unused_check(_: BTreeMap<String, String>) {}

#[cfg(test)]
mod tests {
    use ogsc_recipes::{
        BinaryArchitecture, Distribution, ExecutableFingerprint, FixOption, GameRecipe,
        GameRelease, HashVerificationStatus, HealthStatus, LaunchConfig, PartialHash, PatchAction,
        PatchStep, RecipeMetadata, RollbackPlan, RollbackStrategy, RuntimeRequirements,
        CURRENT_SCHEMA_VERSION,
    };

    use super::*;
    use crate::hasher::compute_fingerprint;

    fn test_recipe(
        game_id: &str,
        title: &str,
        exe_rel: &str,
        fingerprint: ExecutableFingerprint,
        sentinels: Vec<&str>,
    ) -> GameRecipe {
        GameRecipe {
            schema_version: CURRENT_SCHEMA_VERSION,
            game_id: game_id.to_owned(),
            title: title.to_owned(),
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
                release_id: "default-release".to_owned(),
                label: "Default".to_owned(),
                distribution: Distribution::RetailCd,
                version: "1.0".to_owned(),
                region: None,
                executable_path: exe_rel.to_owned(),
                fingerprint,
                sentinel_files: sentinels.into_iter().map(str::to_owned).collect(),
            }],
            known_issues: Vec::new(),
            runtime_requirements: RuntimeRequirements {
                architecture: BinaryArchitecture::Win32,
                requires_16bit_emulator: false,
                native_graphics_api: vec!["Direct3D 9".to_owned()],
                target_graphics_api: Some("Direct3D 11".to_owned()),
                requires_admin: false,
            },
            sandbox: None,
            fixes: vec![FixOption {
                id: "dummy_fix".to_owned(),
                title: "Dummy Fix".to_owned(),
                description: "No-op sandbox step for tests.".to_owned(),
                recommended: true,
                applies_to_releases: Vec::new(),
                steps: vec![PatchStep {
                    id: "sandbox-dir".to_owned(),
                    name: "Sandbox".to_owned(),
                    description: "Create sandbox".to_owned(),
                    applies_to_releases: Vec::new(),
                    os_filter: None,
                    action: PatchAction::ConfigureSandbox {
                        create_directories: vec![".ogsc_sandbox".to_owned()],
                        migrate_existing_saves_copy_only: false,
                    },
                }],
            }],
            launch: LaunchConfig {
                default_executable: exe_rel.to_owned(),
                release_executable_overrides: BTreeMap::new(),
                fix_executable_overrides: BTreeMap::new(),
                working_directory: None,
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
    fn matches_sha256_md5_partial_and_nested_game_roots() {
        let dir = tempfile::tempdir().expect("create tempdir");

        // 1. Exact SHA-256 game in nested `offline/system/`
        let sc_root = dir.path().join("SplinterCellPT");
        let sc_sys = sc_root.join("offline").join("system");
        fs::create_dir_all(&sc_sys).expect("mkdir sc");
        let sc_exe = sc_sys.join("SplinterCell2.exe");
        fs::write(&sc_exe, b"SC2_EXACT_SHA_BINARY_PAYLOAD").expect("write sc exe");
        fs::write(sc_sys.join("Engine.dll"), b"engine").expect("write sentinel");
        let sc_fp = compute_fingerprint(&sc_exe, 8).expect("fp sc");

        // 2. Exact MD5 game
        let md5_root = dir.path().join("Md5Game");
        fs::create_dir_all(&md5_root).expect("mkdir md5");
        let md5_exe = md5_root.join("LegacyMd5.exe");
        fs::write(&md5_exe, b"LEGACY_MD5_PAYLOAD").expect("write md5 exe");
        let md5_fp = compute_fingerprint(&md5_exe, 8).expect("fp md5");

        // 3. Partial SHA-256 + size game
        let part_root = dir.path().join("PartialGame");
        fs::create_dir_all(&part_root).expect("mkdir partial");
        let part_exe = part_root.join("Partial.exe");
        fs::write(&part_exe, b"HEADER_12345678_REST_OF_EXE").expect("write partial exe");
        let part_fp = compute_fingerprint(&part_exe, 8).expect("fp partial");

        // Also place a copy inside `.ogsc_backup` that MUST be ignored by the scanner!
        let backup_copy = sc_root.join(".ogsc_backup").join("offline").join("system");
        fs::create_dir_all(&backup_copy).expect("mkdir backup");
        fs::write(backup_copy.join("SplinterCell2.exe"), b"SC2_EXACT_SHA_BINARY_PAYLOAD")
            .expect("write backup copy");

        let mut db = RecipeDatabase::new();
        db.insert(
            test_recipe(
                "sc-pt",
                "Splinter Cell PT",
                "offline/system/SplinterCell2.exe",
                ExecutableFingerprint {
                    file_size_bytes: Some(sc_fp.file_size_bytes),
                    partial_sha256: Some(PartialHash {
                        offset: 0,
                        length: sc_fp.partial_length,
                        sha256: sc_fp.partial_sha256,
                    }),
                    sha256: Some(sc_fp.sha256),
                    md5: None,
                    verification_status: HashVerificationStatus::Verified,
                },
                vec!["offline/system/Engine.dll"],
            ),
            None,
            true,
        )
        .expect("insert sc");

        db.insert(
            test_recipe(
                "md5-title",
                "MD5 Title",
                "LegacyMd5.exe",
                ExecutableFingerprint {
                    file_size_bytes: Some(md5_fp.file_size_bytes),
                    partial_sha256: None,
                    sha256: None,
                    md5: Some(md5_fp.md5),
                    verification_status: HashVerificationStatus::Verified,
                },
                Vec::new(),
            ),
            None,
            true,
        )
        .expect("insert md5");

        db.insert(
            test_recipe(
                "partial-title",
                "Partial Title",
                "Partial.exe",
                ExecutableFingerprint {
                    file_size_bytes: Some(part_fp.file_size_bytes),
                    partial_sha256: Some(PartialHash {
                        offset: 0,
                        length: part_fp.partial_length,
                        sha256: part_fp.partial_sha256,
                    }),
                    sha256: None,
                    md5: None,
                    verification_status: HashVerificationStatus::Verified,
                },
                Vec::new(),
            ),
            None,
            true,
        )
        .expect("insert partial");

        let scanner = GameScanner::new(&db, ScanOptions::default());
        let report = scanner.scan_dir(dir.path()).expect("scan tempdir");

        assert_eq!(report.matches.len(), 3);
        let sc_match = report.matches.iter().find(|m| m.game_id == "sc-pt").expect("sc match");
        assert_eq!(sc_match.confidence, MatchConfidence::ExactSha256);
        assert_eq!(sc_match.game_root, clean_absolute_path(&sc_root).expect("clean"));
        assert_eq!(sc_match.matched_sentinels, vec!["offline/system/Engine.dll"]);

        let md5_match =
            report.matches.iter().find(|m| m.game_id == "md5-title").expect("md5 match");
        assert_eq!(md5_match.confidence, MatchConfidence::ExactMd5);

        let part_match =
            report.matches.iter().find(|m| m.game_id == "partial-title").expect("partial match");
        assert_eq!(part_match.confidence, MatchConfidence::PartialSha256AndSize);
    }

    #[test]
    fn heuristic_matching_respects_sentinels_and_opt_out_flag() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let game_root = dir.path().join("NFSMW");
        fs::create_dir_all(game_root.join("GLOBAL")).expect("mkdir GLOBAL");
        fs::write(game_root.join("speed.exe"), b"DIFFERENT_LOCAL_BUILD").expect("write speed.exe");
        fs::write(game_root.join("GLOBAL").join("GlobalB.lzc"), b"data").expect("write sentinel");

        let mut db = RecipeDatabase::new();
        db.insert(
            test_recipe(
                "nfs-mw",
                "Need for Speed: Most Wanted",
                "speed.exe",
                ExecutableFingerprint {
                    file_size_bytes: Some(6_029_312),
                    partial_sha256: None,
                    sha256: Some(std::iter::repeat_n('f', 64).collect()),
                    md5: None,
                    verification_status: HashVerificationStatus::Verified,
                },
                vec!["GLOBAL/GlobalB.lzc"],
            ),
            None,
            true,
        )
        .expect("insert nfs");

        let with_heuristics = GameScanner::new(&db, ScanOptions::default())
            .scan_dir(dir.path())
            .expect("scan with heuristics");
        assert_eq!(with_heuristics.matches.len(), 1);
        assert_eq!(
            with_heuristics.matches[0].confidence,
            MatchConfidence::FilenameAndSentinelsOnly
        );
        assert!(with_heuristics.matches[0].observed_hashes.is_some());

        let no_heuristics_opts =
            ScanOptions { include_heuristic_matches: false, ..ScanOptions::default() };
        let without_heuristics = GameScanner::new(&db, no_heuristics_opts)
            .scan_dir(dir.path())
            .expect("scan without heuristics");
        assert_eq!(without_heuristics.matches.len(), 0);
    }
}
