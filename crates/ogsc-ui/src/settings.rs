//! Persistent application settings and initial bundled-recipe seeding.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ogsc_patcher::HttpDownloader;
use ogsc_scanner::LibraryStore;
use ogsc_wrappers::ToolkitStore;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Schema version for persisted `settings.json` files.
pub const SETTINGS_SCHEMA_VERSION: u32 = 1;

/// Default GitHub repository hosting community compatibility recipes.
pub const DEFAULT_GITHUB_REPO_URL: &str = "https://github.com/Momenzi/ReviveBox";

/// Default Git branch checked during remote recipe sync.
pub const DEFAULT_GITHUB_BRANCH: &str = "main";

/// Default subdirectory inside [`DEFAULT_GITHUB_REPO_URL`] containing `.json` recipes.
pub const DEFAULT_GITHUB_RECIPES_PATH: &str = "recipes/games";

static ATOMIC_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Errors returned while reading or saving [`AppSettings`].
#[derive(Debug, Error)]
pub enum SettingsError {
    /// Filesystem I/O error.
    #[error("settings I/O error at `{path}`: {source}")]
    Io {
        /// Path that failed.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// JSON serialization failure.
    #[error("failed to serialize settings JSON: {source}")]
    Serialize {
        /// Serde error details.
        #[source]
        source: serde_json::Error,
    },
}

/// Persistent desktop launcher configuration stored in `settings.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Integer schema version for forward migration.
    pub schema_version: u32,
    /// Local directory containing active `.json` game compatibility recipes.
    pub recipes_dir: PathBuf,
    /// Path to `library.json` persisting scanned games and fix decisions.
    pub library_path: PathBuf,
    /// Root `runtime_wrappers` directory (`dgVoodoo 2`, `DxWrapper`, `WineD3D`, `XInput Plus`, `DOSBox-X`, `86Box`).
    pub toolkit_dir: PathBuf,
    /// Directory storing SHA-256-verified downloaded patch archives and `bsdiff` patches.
    pub cache_dir: PathBuf,
    /// Local cache directory for downloaded SteamGridDB box art.
    pub covers_dir: PathBuf,
    /// HTTPS GitHub repository URL used by the "Sync Recipes" action.
    pub github_repo_url: String,
    /// Git branch or tag queried by GitHub recipe sync.
    pub github_branch: String,
    /// Subdirectory within the repository holding `.json` recipe manifests.
    pub github_recipes_path: String,
    /// Optional SteamGridDB v2 Bearer API key stored locally for cover art fetching.
    pub steamgriddb_api_key: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        let data_dir = Self::default_data_dir();
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            recipes_dir: data_dir.join("recipes").join("games"),
            library_path: LibraryStore::default_path(),
            toolkit_dir: ToolkitStore::default_dir(),
            cache_dir: HttpDownloader::default_cache_dir(),
            covers_dir: data_dir.join("covers"),
            github_repo_url: DEFAULT_GITHUB_REPO_URL.to_owned(),
            github_branch: DEFAULT_GITHUB_BRANCH.to_owned(),
            github_recipes_path: DEFAULT_GITHUB_RECIPES_PATH.to_owned(),
            steamgriddb_api_key: String::new(),
        }
    }
}

impl AppSettings {
    /// Returns the per-user base application data directory (`%LOCALAPPDATA%\OldGamesSecondChance`).
    pub fn default_data_dir() -> PathBuf {
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            if !local_app_data.is_empty() {
                return PathBuf::from(local_app_data).join("OldGamesSecondChance");
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            if !home.is_empty() {
                return PathBuf::from(home)
                    .join(".local")
                    .join("share")
                    .join("old-games-second-chance");
            }
        }
        PathBuf::from(".ogsc_data")
    }

    /// Returns the default `settings.json` path inside [`AppSettings::default_data_dir`].
    pub fn default_path() -> PathBuf {
        Self::default_data_dir().join("settings.json")
    }

    /// Loads `settings.json` from `path`, falling back cleanly to [`AppSettings::default`] if absent or unreadable.
    pub fn load_or_default(path: &Path) -> Self {
        if let Ok(raw) = fs::read_to_string(path) {
            if let Ok(parsed) = serde_json::from_str::<Self>(&raw) {
                return parsed;
            }
        }
        Self::default()
    }

    /// Atomically persists settings as pretty-printed JSON at `path`.
    pub fn save(&self, path: &Path) -> Result<(), SettingsError> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|source| SettingsError::Serialize { source })?;
        write_bytes_atomic(path, format!("{json}\n").as_bytes())
            .map_err(|source| SettingsError::Io { path: path.to_path_buf(), source })
    }
}

/// Returns candidate filesystem paths where bundled `recipes/games` manifests may reside.
pub fn discover_bundled_recipe_dirs() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    candidates.push(PathBuf::from("recipes").join("games"));

    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            candidates.push(exe_dir.join("recipes").join("games"));
            if let Some(target_dir) = exe_dir.parent().and_then(Path::parent) {
                candidates.push(target_dir.join("recipes").join("games"));
                if let Some(root_dir) = target_dir.parent() {
                    candidates.push(root_dir.join("recipes").join("games"));
                }
            }
        }
    }

    candidates
}

/// Seeds `target_recipes_dir` from the first non-empty bundled directory if `target_recipes_dir`
/// contains no `.json` recipe files yet, without overwriting existing local recipes.
pub fn seed_local_recipes_if_empty(
    target_recipes_dir: &Path,
    bundled_candidates: &[PathBuf],
) -> usize {
    if has_any_json_file(target_recipes_dir) {
        return 0;
    }

    sync_missing_bundled_recipes_and_patches(target_recipes_dir, bundled_candidates)
}

/// Recursively copies bundled `.json` recipes and `.bsdiff` binary patches from the first
/// non-empty bundled directory into `target_recipes_dir` whenever missing, or upgrades an existing
/// `.json` recipe when the bundled copy has the same `game_id` and a strictly newer `metadata.recipe_version`.
pub fn sync_missing_bundled_recipes_and_patches(
    target_recipes_dir: &Path,
    bundled_candidates: &[PathBuf],
) -> usize {
    let Some(source_dir) =
        bundled_candidates.iter().find(|dir| dir.is_dir() && has_any_json_file(dir))
    else {
        return 0;
    };

    if fs::create_dir_all(target_recipes_dir).is_err() {
        return 0;
    }

    copy_missing_recipe_assets_recursive(source_dir, target_recipes_dir)
}

fn copy_missing_recipe_assets_recursive(src_dir: &Path, dest_dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(src_dir) else {
        return 0;
    };

    let mut copied = 0_usize;
    for entry in entries.flatten() {
        let src_path = entry.path();
        let Some(name) = src_path.file_name() else {
            continue;
        };
        let dest_path = dest_dir.join(name);

        if src_path.is_dir() {
            copied += copy_missing_recipe_assets_recursive(&src_path, &dest_path);
            continue;
        }
        if !src_path.is_file() || !is_recipe_or_patch_file(&src_path) {
            continue;
        }
        if dest_path.exists() && !should_update_bundled_asset(&src_path, &dest_path) {
            continue;
        }
        if let Ok(bytes) = fs::read(&src_path) {
            if write_bytes_atomic(&dest_path, &bytes).is_ok() {
                copied += 1;
            }
        }
    }
    copied
}

fn should_update_bundled_asset(src_path: &Path, dest_path: &Path) -> bool {
    if is_json_file(src_path) {
        should_upgrade_local_recipe(src_path, dest_path)
    } else if is_bsdiff_file(src_path) {
        should_upgrade_bsdiff_patch(src_path, dest_path)
    } else {
        false
    }
}

fn should_upgrade_bsdiff_patch(src_path: &Path, dest_path: &Path) -> bool {
    let Ok(src_bytes) = fs::read(src_path) else {
        return false;
    };
    let Ok(dest_bytes) = fs::read(dest_path) else {
        return false;
    };
    src_bytes != dest_bytes
}

fn is_bsdiff_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("bsdiff"))
}

fn is_json_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

fn should_upgrade_local_recipe(src_path: &Path, dest_path: &Path) -> bool {
    if !is_json_file(src_path) || !is_json_file(dest_path) {
        return false;
    }
    let Some((src_id, src_ver)) = read_recipe_id_and_version(src_path) else {
        return false;
    };
    let Some((dest_id, dest_ver)) = read_recipe_id_and_version(dest_path) else {
        return false;
    };
    src_id == dest_id && src_ver > dest_ver
}

fn read_recipe_id_and_version(path: &Path) -> Option<(String, (u64, u64, u64))> {
    let raw = fs::read_to_string(path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let id = val.get("game_id")?.as_str()?.trim();
    if id.is_empty() {
        return None;
    }
    let ver_str = val.get("metadata")?.get("recipe_version")?.as_str()?;
    let ver = parse_semver_tuple(ver_str)?;
    Some((id.to_owned(), ver))
}

fn parse_semver_tuple(raw: &str) -> Option<(u64, u64, u64)> {
    let mut parts = raw.trim().split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    let patch = parts.next()?.parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn is_recipe_or_patch_file(path: &Path) -> bool {
    is_json_file(path) || is_bsdiff_file(path)
}

fn has_any_json_file(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let p = entry.path();
        p.is_file()
            && p.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
    })
}

/// Atomically writes `bytes` to `dest` via a unique sibling temporary file and rename.
pub(crate) fn write_bytes_atomic(dest: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let counter = ATOMIC_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!(
        ".{}.tmp.{}.{counter}",
        dest.file_name().and_then(|n| n.to_str()).unwrap_or("ogsc"),
        std::process::id()
    );
    let tmp_path = dest.with_file_name(tmp_name);

    let write_res = (|| -> Result<(), std::io::Error> {
        let mut file = File::create(&tmp_path)?;
        file.write_all(bytes)?;
        file.flush()?;
        drop(file);
        fs::rename(&tmp_path, dest)?;
        Ok(())
    })();

    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_reloads_settings_and_seeds_bundled_recipes_non_destructively() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let settings_path = temp.path().join("cfg").join("settings.json");

        let custom = AppSettings {
            steamgriddb_api_key: "test-key-123".to_owned(),
            github_branch: "develop".to_owned(),
            ..AppSettings::default()
        };
        custom.save(&settings_path).expect("save settings");

        let reloaded = AppSettings::load_or_default(&settings_path);
        assert_eq!(reloaded, custom);

        // Bundled recipe seeding into empty dir
        let bundled = temp.path().join("bundled");
        let target = temp.path().join("local_recipes");
        fs::create_dir_all(&bundled).expect("mkdir bundled");
        fs::write(bundled.join("game_a.json"), b"{\"a\":1}").expect("write bundled");
        fs::write(bundled.join("notes.txt"), b"ignored").expect("write non-json");

        let seeded = seed_local_recipes_if_empty(&target, std::slice::from_ref(&bundled));
        assert_eq!(seeded, 1);
        assert!(target.join("game_a.json").is_file());
        assert!(!target.join("notes.txt").exists());

        // Subsequent seed_local_recipes_if_empty leaves populated directories untouched
        fs::write(bundled.join("game_b.json"), b"{\"b\":2}").expect("write second");
        let seeded_again = seed_local_recipes_if_empty(&target, std::slice::from_ref(&bundled));
        assert_eq!(seeded_again, 0);
        assert!(!target.join("game_b.json").exists());

        // sync_missing_bundled_recipes_and_patches copies newly added JSON + nested .bsdiff patches
        // without overwriting customized local JSON files
        fs::write(target.join("game_a.json"), b"{\"custom\":true}").expect("customize a");
        fs::create_dir_all(bundled.join("patches").join("barbie")).expect("mkdir patches");
        fs::write(bundled.join("patches").join("barbie").join("fix.bsdiff"), b"BSDIFF")
            .expect("write bsdiff");
        let synced = sync_missing_bundled_recipes_and_patches(&target, &[bundled]);
        assert_eq!(synced, 2);
        assert_eq!(
            fs::read_to_string(target.join("game_a.json")).expect("read a"),
            "{\"custom\":true}"
        );
        assert!(target.join("game_b.json").is_file());
        assert_eq!(
            fs::read(target.join("patches").join("barbie").join("fix.bsdiff"))
                .expect("read bsdiff"),
            b"BSDIFF"
        );
    }

    #[test]
    fn upgrades_local_bundled_recipe_only_when_version_is_newer() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let bundled = temp.path().join("bundled");
        let target = temp.path().join("local_recipes");
        fs::create_dir_all(&bundled).expect("mkdir bundled");
        fs::create_dir_all(&target).expect("mkdir target");

        let mk = |game_id: &str, ver: &str, tag: &str| {
            format!(
                "{{\"game_id\":\"{game_id}\",\"metadata\":{{\"recipe_version\":\"{ver}\",\"tag\":\"{tag}\"}}}}"
            )
        };

        // 1.0.0 -> 1.0.1 with matching game_id: upgrades!
        fs::write(target.join("barbie.json"), mk("barbie-2006", "1.0.0", "old")).expect("w1");
        fs::write(bundled.join("barbie.json"), mk("barbie-2006", "1.0.1", "new")).expect("w2");

        // Equal 1.0.1 -> 1.0.1: retains local customization.
        fs::write(target.join("same.json"), mk("same-game", "1.0.1", "custom-local")).expect("w3");
        fs::write(bundled.join("same.json"), mk("same-game", "1.0.1", "bundled")).expect("w4");

        // Remote/newer 1.0.2 vs bundled 1.0.1: does not downgrade.
        fs::write(target.join("remote.json"), mk("remote-game", "1.0.2", "remote-new"))
            .expect("w5");
        fs::write(bundled.join("remote.json"), mk("remote-game", "1.0.1", "bundled-old"))
            .expect("w6");

        // Mismatched game_id: untouched.
        fs::write(target.join("mismatch.json"), mk("custom-id", "1.0.0", "custom")).expect("w7");
        fs::write(bundled.join("mismatch.json"), mk("other-id", "2.0.0", "bundled")).expect("w8");

        // Updated .bsdiff binary patch overwrites stale local copy; identical .bsdiff does not count.
        fs::write(target.join("patch.bsdiff"), b"OLD_DIFF").expect("w9");
        fs::write(bundled.join("patch.bsdiff"), b"NEW_DIFF_V2").expect("w10");
        fs::write(target.join("same.bsdiff"), b"SAME_DIFF").expect("w11");
        fs::write(bundled.join("same.bsdiff"), b"SAME_DIFF").expect("w12");

        let updated = sync_missing_bundled_recipes_and_patches(&target, &[bundled]);
        assert_eq!(updated, 2);
        assert!(fs::read_to_string(target.join("barbie.json")).expect("r1").contains("\"new\""));
        assert!(fs::read_to_string(target.join("same.json"))
            .expect("r2")
            .contains("\"custom-local\""));
        assert!(fs::read_to_string(target.join("remote.json"))
            .expect("r3")
            .contains("\"remote-new\""));
        assert!(fs::read_to_string(target.join("mismatch.json"))
            .expect("r4")
            .contains("\"custom\""));
        assert_eq!(fs::read(target.join("patch.bsdiff")).expect("r5"), b"NEW_DIFF_V2");
    }
}
