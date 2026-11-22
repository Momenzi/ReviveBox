//! Multi-file recipe loading, cross-recipe hash indexing, and lookup utilities.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::RecipeError;
use crate::schema::GameRecipe;
use crate::validation::ValidationIssue;

/// Reference pointing to a specific [`GameRelease`](crate::schema::GameRelease) inside a loaded [`GameRecipe`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReleaseRef {
    /// Owning recipe `game_id`.
    pub game_id: String,
    /// Matched `release_id` within that recipe.
    pub release_id: String,
}

/// Non-fatal validation warnings captured for a single loaded recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeWarningRecord {
    /// Identifier of the recipe that emitted warnings.
    pub game_id: String,
    /// Path to the JSON file on disk when loaded from the filesystem.
    pub source_path: Option<PathBuf>,
    /// Warnings emitted by [`GameRecipe::validate`].
    pub warnings: Vec<ValidationIssue>,
}

/// Database returned by [`RecipeDatabase::load_from_dir`] alongside any non-fatal recipe warnings.
#[derive(Debug, Clone, Default)]
pub struct LoadedDatabase {
    /// Indexed collection of validated recipes.
    pub db: RecipeDatabase,
    /// Warnings collected from loaded recipes (empty when all recipes pass strict checks).
    pub warnings: Vec<RecipeWarningRecord>,
}

/// In-memory indexed repository of validated [`GameRecipe`] manifests.
#[derive(Debug, Clone, Default)]
pub struct RecipeDatabase {
    recipes: BTreeMap<String, GameRecipe>,
    by_exe_name: HashMap<String, Vec<ReleaseRef>>,
    by_sha256: HashMap<String, ReleaseRef>,
    by_md5: HashMap<String, ReleaseRef>,
}

impl RecipeDatabase {
    /// Creates an empty [`RecipeDatabase`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads, parses, and validates a single `.json` recipe file from disk.
    ///
    /// Returns the parsed [`GameRecipe`] together with any non-fatal warnings when `strict == false`.
    pub fn load_recipe_file(
        path: &Path,
        strict: bool,
    ) -> Result<(GameRecipe, Vec<ValidationIssue>), RecipeError> {
        let raw = fs::read_to_string(path)
            .map_err(|source| RecipeError::Io { path: path.to_path_buf(), source })?;
        let recipe: GameRecipe = serde_json::from_str(&raw)
            .map_err(|source| RecipeError::ParseJson { path: path.to_path_buf(), source })?;

        let report = recipe.validate();
        let warnings = report.warnings.clone();
        if let Err(failed_report) = report.into_result(strict) {
            return Err(RecipeError::ValidationFailed {
                game_id: recipe.game_id,
                path: Some(path.to_path_buf()),
                report: failed_report,
            });
        }

        Ok((recipe, warnings))
    }

    /// Recursively loads and validates every `.json` file inside `dir` in deterministic sorted order.
    pub fn load_from_dir(dir: &Path, strict: bool) -> Result<LoadedDatabase, RecipeError> {
        if !dir.is_dir() {
            return Err(RecipeError::DirectoryNotFound { path: dir.to_path_buf() });
        }

        let mut json_files = Vec::new();
        collect_json_files(dir, &mut json_files)?;
        json_files.sort();

        let mut db = Self::new();
        let mut warning_records = Vec::new();

        for path in json_files {
            let (recipe, warnings) = Self::load_recipe_file(&path, strict)?;
            db.insert_validated(recipe, warnings, Some(path), &mut warning_records)?;
        }

        Ok(LoadedDatabase { db, warnings: warning_records })
    }

    /// Validates and inserts an in-memory [`GameRecipe`] into this database.
    pub fn insert(
        &mut self,
        recipe: GameRecipe,
        source_path: Option<PathBuf>,
        strict: bool,
    ) -> Result<Vec<ValidationIssue>, RecipeError> {
        let report = recipe.validate();
        let warnings = report.warnings.clone();
        if let Err(failed_report) = report.into_result(strict) {
            return Err(RecipeError::ValidationFailed {
                game_id: recipe.game_id,
                path: source_path,
                report: failed_report,
            });
        }

        let mut records = Vec::new();
        self.insert_validated(recipe, warnings.clone(), source_path, &mut records)?;
        Ok(warnings)
    }

    fn insert_validated(
        &mut self,
        recipe: GameRecipe,
        warnings: Vec<ValidationIssue>,
        source_path: Option<PathBuf>,
        warning_records: &mut Vec<RecipeWarningRecord>,
    ) -> Result<(), RecipeError> {
        if let Some(existing) = self.recipes.get(&recipe.game_id) {
            return Err(RecipeError::DuplicateGameId {
                game_id: recipe.game_id.clone(),
                existing_title: existing.title.clone(),
                duplicate_title: recipe.title,
            });
        }

        // Check cross-recipe fingerprint collisions before mutating indices.
        for release in &recipe.releases {
            if let Some(sha) = &release.fingerprint.sha256 {
                if let Some(existing) = self.by_sha256.get(sha) {
                    return Err(RecipeError::DuplicateFingerprintHash {
                        algorithm: "SHA-256",
                        hash: sha.clone(),
                        first_game_id: existing.game_id.clone(),
                        second_game_id: recipe.game_id.clone(),
                    });
                }
            }
            if let Some(md5) = &release.fingerprint.md5 {
                if let Some(existing) = self.by_md5.get(md5) {
                    return Err(RecipeError::DuplicateFingerprintHash {
                        algorithm: "MD5",
                        hash: md5.clone(),
                        first_game_id: existing.game_id.clone(),
                        second_game_id: recipe.game_id.clone(),
                    });
                }
            }
        }

        for release in &recipe.releases {
            let release_ref = ReleaseRef {
                game_id: recipe.game_id.clone(),
                release_id: release.release_id.clone(),
            };
            let exe_basename = executable_basename(&release.executable_path);
            self.by_exe_name.entry(exe_basename).or_default().push(release_ref.clone());

            if let Some(sha) = &release.fingerprint.sha256 {
                self.by_sha256.insert(sha.clone(), release_ref.clone());
            }
            if let Some(md5) = &release.fingerprint.md5 {
                self.by_md5.insert(md5.clone(), release_ref);
            }
        }

        if !warnings.is_empty() {
            warning_records.push(RecipeWarningRecord {
                game_id: recipe.game_id.clone(),
                source_path,
                warnings,
            });
        }

        self.recipes.insert(recipe.game_id.clone(), recipe);
        Ok(())
    }

    /// Returns the recipe matching `game_id`, if loaded.
    pub fn get_by_id(&self, game_id: &str) -> Option<&GameRecipe> {
        self.recipes.get(game_id)
    }

    /// Iterates over all loaded recipes sorted deterministically by `game_id`.
    pub fn all_recipes(&self) -> impl Iterator<Item = &GameRecipe> {
        self.recipes.values()
    }

    /// Finds candidate releases matching a case-insensitive executable filename (for example `SPEED.EXE`).
    pub fn find_by_executable_name(&self, executable_name: &str) -> &[ReleaseRef] {
        let key = executable_basename(executable_name);
        self.by_exe_name.get(&key).map_or(&[], Vec::as_slice)
    }

    /// Finds the unique release matching a full lowercase or uppercase hex SHA-256 digest.
    pub fn find_by_sha256(&self, sha256: &str) -> Option<&ReleaseRef> {
        self.by_sha256.get(&sha256.to_ascii_lowercase())
    }

    /// Finds the unique release matching a full lowercase or uppercase hex MD5 digest.
    pub fn find_by_md5(&self, md5: &str) -> Option<&ReleaseRef> {
        self.by_md5.get(&md5.to_ascii_lowercase())
    }

    /// Returns the number of loaded game recipes.
    pub fn len(&self) -> usize {
        self.recipes.len()
    }

    /// Returns `true` when no recipes are loaded.
    pub fn is_empty(&self) -> bool {
        self.recipes.is_empty()
    }
}

fn executable_basename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_ascii_lowercase()
}

fn collect_json_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), RecipeError> {
    let entries =
        fs::read_dir(dir).map_err(|source| RecipeError::Io { path: dir.to_path_buf(), source })?;

    for entry_res in entries {
        let entry =
            entry_res.map_err(|source| RecipeError::Io { path: dir.to_path_buf(), source })?;
        let path = entry.path();
        if path.is_dir() {
            collect_json_files(&path, out)?;
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        {
            out.push(path);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        BinaryArchitecture, Distribution, ExecutableFingerprint, GameRelease,
        HashVerificationStatus, HealthStatus, LaunchConfig, RecipeMetadata, RuntimeRequirements,
        CURRENT_SCHEMA_VERSION,
    };
    use crate::steps::{RollbackPlan, RollbackStrategy};

    fn minimal_recipe(game_id: &str, sha_char: char) -> GameRecipe {
        let sha: String = std::iter::repeat_n(sha_char, 64).collect();
        GameRecipe {
            schema_version: CURRENT_SCHEMA_VERSION,
            game_id: game_id.to_owned(),
            title: format!("Title {game_id}"),
            release_year: 2003,
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
                release_id: "default".to_owned(),
                label: "Default".to_owned(),
                distribution: Distribution::RetailCd,
                version: "1.0".to_owned(),
                region: None,
                executable_path: "Game.exe".to_owned(),
                fingerprint: ExecutableFingerprint {
                    file_size_bytes: Some(123_456),
                    partial_sha256: None,
                    sha256: Some(sha),
                    md5: None,
                    verification_status: HashVerificationStatus::Verified,
                },
                sentinel_files: Vec::new(),
            }],
            known_issues: Vec::new(),
            runtime_requirements: RuntimeRequirements {
                architecture: BinaryArchitecture::Win32,
                requires_16bit_emulator: false,
                native_graphics_api: vec!["Direct3D 8".to_owned()],
                target_graphics_api: Some("Direct3D 11".to_owned()),
                requires_admin: false,
            },
            sandbox: None,
            fixes: Vec::new(),
            launch: LaunchConfig {
                default_executable: "Game.exe".to_owned(),
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
    fn rejects_duplicate_game_id_and_duplicate_hashes() {
        let mut db = RecipeDatabase::new();
        db.insert(minimal_recipe("game-one", '1'), None, true)
            .expect("first recipe should insert cleanly");

        let dup_id_err = db
            .insert(minimal_recipe("game-one", '2'), None, true)
            .expect_err("duplicate game_id must fail");
        assert!(matches!(dup_id_err, RecipeError::DuplicateGameId { .. }));

        let dup_hash_err = db
            .insert(minimal_recipe("game-two", '1'), None, true)
            .expect_err("duplicate SHA-256 across recipes must fail");
        assert!(matches!(dup_hash_err, RecipeError::DuplicateFingerprintHash { .. }));
    }

    #[test]
    fn returns_directory_not_found_for_missing_dir() {
        let missing = PathBuf::from("nonexistent_recipes_directory_12345");
        let err = RecipeDatabase::load_from_dir(&missing, false)
            .expect_err("missing directory must fail cleanly");
        assert!(matches!(err, RecipeError::DirectoryNotFound { .. }));
    }

    #[test]
    fn loads_recipes_from_temp_directory() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let recipe = minimal_recipe("temp-game", 'a');
        let json = serde_json::to_string_pretty(&recipe).expect("serialize");
        fs::write(dir.path().join("temp.json"), json).expect("write temp json");

        let loaded = RecipeDatabase::load_from_dir(dir.path(), true)
            .expect("temp directory should load cleanly under strict mode");
        assert_eq!(loaded.db.len(), 1);
        assert_eq!(loaded.warnings, Vec::new());
        assert_eq!(loaded.db.find_by_executable_name("game.exe").len(), 1);
    }
}
