//! Persistent local Game Library store (`library.json`) tracking imported games and fix decisions.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::backup::write_file_atomic;
use crate::error::ScannerError;
use crate::hasher::sha256_bytes;
use crate::scanner::{clean_absolute_path, normalized_path_key, MatchConfidence, ScanMatch};

/// Schema version for persisted `library.json` files.
pub const LIBRARY_SCHEMA_VERSION: u32 = 1;

/// Patching lifecycle status of an imported game entry in `library.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GameStatus {
    /// Game was scanned and needs compatibility fixes (or previous fixes were rolled back).
    NeedsFixes,
    /// One or more compatibility fixes have been applied cleanly.
    Patched,
    /// User inspected the Patch Advisor popup and chose to keep/play the game unpatched (vanilla).
    DeclinedVanilla,
}

/// Persisted record for a single scanned or imported game installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryEntry {
    /// Deterministic unique ID derived from `game_id` and normalized `install_dir`.
    pub entry_id: String,
    /// Matched recipe `game_id`.
    pub game_id: String,
    /// Game title.
    pub title: String,
    /// Matched `release_id`.
    pub release_id: String,
    /// Matched release display label.
    pub release_label: String,
    /// Installation root directory.
    pub install_dir: PathBuf,
    /// Path to the primary matched executable.
    pub executable_path: PathBuf,
    /// Strength of the last scan match.
    pub match_confidence: MatchConfidence,
    /// Full lowercase SHA-256 of the executable observed during scanning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected_sha256: Option<String>,
    /// Current patching lifecycle status.
    pub status: GameStatus,
    /// Fix IDs currently applied to this installation.
    #[serde(default)]
    pub applied_fix_ids: Vec<String>,
    /// Fix IDs explicitly declined by the user in the Patch Advisor popup.
    #[serde(default)]
    pub declined_fix_ids: Vec<String>,
    /// Optional local cover artwork path cached by the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_cover_art_path: Option<PathBuf>,
    /// RFC 3339 UTC timestamp when this installation was first imported.
    pub imported_at: String,
    /// RFC 3339 UTC timestamp when this installation was last verified by a scan.
    pub last_scanned_at: String,
    /// RFC 3339 UTC timestamp when the user last launched this game from the launcher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_launched_at: Option<String>,
}

/// Serializable root document written to `library.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameLibrary {
    /// Integer schema version (must match [`LIBRARY_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// Saved user library scan roots.
    #[serde(default)]
    pub scan_folders: Vec<PathBuf>,
    /// Imported game entries keyed by [`LibraryEntry::entry_id`].
    #[serde(default)]
    pub entries: BTreeMap<String, LibraryEntry>,
}

impl Default for GameLibrary {
    fn default() -> Self {
        Self {
            schema_version: LIBRARY_SCHEMA_VERSION,
            scan_folders: Vec::new(),
            entries: BTreeMap::new(),
        }
    }
}

/// Manages loading, mutating, and atomically persisting `library.json`.
#[derive(Debug, Clone)]
pub struct LibraryStore {
    path: PathBuf,
    library: GameLibrary,
}

impl LibraryStore {
    /// Opens an existing `library.json` or initializes an empty in-memory library at `path`.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, ScannerError> {
        let path = path.into();
        let library = if path.is_file() {
            let raw = fs::read_to_string(&path)
                .map_err(|source| ScannerError::Io { path: path.clone(), source })?;
            let lib: GameLibrary = serde_json::from_str(&raw)
                .map_err(|source| ScannerError::LibraryParse { path: path.clone(), source })?;
            if lib.schema_version != LIBRARY_SCHEMA_VERSION {
                return Err(ScannerError::LibraryUnsupportedVersion {
                    version: lib.schema_version,
                });
            }
            lib
        } else {
            GameLibrary::default()
        };

        Ok(Self { path, library })
    }

    /// Returns the default per-user `library.json` location (`%LOCALAPPDATA%\OldGamesSecondChance\library.json` on Windows).
    pub fn default_path() -> PathBuf {
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local_app_data).join("OldGamesSecondChance").join("library.json");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("old-games-second-chance")
                .join("library.json");
        }
        PathBuf::from("library.json")
    }

    /// Returns the filesystem path associated with this [`LibraryStore`].
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns an immutable reference to the underlying [`GameLibrary`].
    pub fn library(&self) -> &GameLibrary {
        &self.library
    }

    /// Atomically persists the current library state to disk, creating parent directories if needed.
    pub fn save(&self) -> Result<(), ScannerError> {
        let json = serde_json::to_string_pretty(&self.library)
            .map_err(|source| ScannerError::LibrarySerialize { source })?;
        write_file_atomic(&self.path, format!("{json}\n").as_bytes())
    }

    /// Registers a scan root folder if not already present (case-insensitive on Windows).
    pub fn add_scan_folder(&mut self, folder: impl AsRef<Path>) -> Result<bool, ScannerError> {
        let clean = clean_absolute_path(folder.as_ref())?;
        let key = normalized_path_key(&clean);
        if self.library.scan_folders.iter().any(|p| normalized_path_key(p) == key) {
            return Ok(false);
        }
        self.library.scan_folders.push(clean);
        Ok(true)
    }

    /// Unregisters a scan root folder, returning `true` if a matching folder was removed.
    pub fn remove_scan_folder(&mut self, folder: impl AsRef<Path>) -> bool {
        let key = clean_absolute_path(folder.as_ref())
            .map_or_else(|_| normalized_path_key(folder.as_ref()), |p| normalized_path_key(&p));
        let before = self.library.scan_folders.len();
        self.library.scan_folders.retain(|p| normalized_path_key(p) != key);
        self.library.scan_folders.len() != before
    }

    /// Inserts or updates a [`LibraryEntry`] from a [`ScanMatch`], preserving user patch decisions on rescans.
    pub fn upsert_scan_match(&mut self, scan_match: &ScanMatch) -> &LibraryEntry {
        let entry_id = compute_entry_id(&scan_match.game_id, &scan_match.game_root);
        let now = Utc::now().to_rfc3339();
        let detected_sha = scan_match.observed_hashes.as_ref().map(|h| h.sha256.clone());

        if let Some(existing) = self.library.entries.get_mut(&entry_id) {
            existing.title.clone_from(&scan_match.title);
            existing.release_id.clone_from(&scan_match.release_id);
            existing.release_label.clone_from(&scan_match.release_label);
            existing.install_dir.clone_from(&scan_match.game_root);
            existing.executable_path.clone_from(&scan_match.executable_path);
            existing.match_confidence = scan_match.confidence;
            if detected_sha.is_some() {
                existing.detected_sha256 = detected_sha;
            }
            existing.last_scanned_at = now;
        } else {
            let entry = LibraryEntry {
                entry_id: entry_id.clone(),
                game_id: scan_match.game_id.clone(),
                title: scan_match.title.clone(),
                release_id: scan_match.release_id.clone(),
                release_label: scan_match.release_label.clone(),
                install_dir: scan_match.game_root.clone(),
                executable_path: scan_match.executable_path.clone(),
                match_confidence: scan_match.confidence,
                detected_sha256: detected_sha,
                status: GameStatus::NeedsFixes,
                applied_fix_ids: Vec::new(),
                declined_fix_ids: Vec::new(),
                custom_cover_art_path: None,
                imported_at: now.clone(),
                last_scanned_at: now,
                last_launched_at: None,
            };
            self.library.entries.insert(entry_id.clone(), entry);
        }

        self.library.entries.get(&entry_id).expect("just inserted or updated")
    }

    /// Records the user's Patch Advisor fix selection (`applied` -> `Patched`, empty `applied` -> `DeclinedVanilla`).
    pub fn record_applied_fixes(
        &mut self,
        entry_id: &str,
        applied_fix_ids: Vec<String>,
        declined_fix_ids: Vec<String>,
    ) -> Result<&LibraryEntry, ScannerError> {
        let entry = self
            .library
            .entries
            .get_mut(entry_id)
            .ok_or_else(|| ScannerError::GameNotInLibrary { entry_id: entry_id.to_owned() })?;
        entry.status = if applied_fix_ids.is_empty() {
            GameStatus::DeclinedVanilla
        } else {
            GameStatus::Patched
        };
        entry.applied_fix_ids = applied_fix_ids;
        entry.declined_fix_ids = declined_fix_ids;
        Ok(entry)
    }

    /// Marks a game as intentionally left vanilla (`DeclinedVanilla`) after user confirmation.
    pub fn mark_declined_vanilla(
        &mut self,
        entry_id: &str,
        declined_fix_ids: Vec<String>,
    ) -> Result<&LibraryEntry, ScannerError> {
        let entry = self
            .library
            .entries
            .get_mut(entry_id)
            .ok_or_else(|| ScannerError::GameNotInLibrary { entry_id: entry_id.to_owned() })?;
        entry.status = GameStatus::DeclinedVanilla;
        entry.applied_fix_ids.clear();
        entry.declined_fix_ids = declined_fix_ids;
        Ok(entry)
    }

    /// Updates a game's status and applied fix list after a partial or full rollback.
    pub fn mark_rolled_back(
        &mut self,
        entry_id: &str,
        remaining_applied_fix_ids: Vec<String>,
    ) -> Result<&LibraryEntry, ScannerError> {
        let entry = self
            .library
            .entries
            .get_mut(entry_id)
            .ok_or_else(|| ScannerError::GameNotInLibrary { entry_id: entry_id.to_owned() })?;
        entry.status = if remaining_applied_fix_ids.is_empty() {
            GameStatus::NeedsFixes
        } else {
            GameStatus::Patched
        };
        entry.applied_fix_ids = remaining_applied_fix_ids;
        Ok(entry)
    }

    /// Records the current UTC timestamp as `last_launched_at` for `entry_id`.
    pub fn record_launched(&mut self, entry_id: &str) -> Result<&LibraryEntry, ScannerError> {
        let entry = self
            .library
            .entries
            .get_mut(entry_id)
            .ok_or_else(|| ScannerError::GameNotInLibrary { entry_id: entry_id.to_owned() })?;
        entry.last_launched_at = Some(Utc::now().to_rfc3339());
        Ok(entry)
    }

    /// Sets or clears a user-chosen local `custom_cover_art_path` on `entry_id`.
    pub fn set_custom_cover_art(
        &mut self,
        entry_id: &str,
        cover_path: Option<PathBuf>,
    ) -> Result<&LibraryEntry, ScannerError> {
        let entry = self
            .library
            .entries
            .get_mut(entry_id)
            .ok_or_else(|| ScannerError::GameNotInLibrary { entry_id: entry_id.to_owned() })?;
        entry.custom_cover_art_path = cover_path;
        Ok(entry)
    }

    /// Removes `entry_id` from the library without touching any game files on disk.
    pub fn remove_entry(&mut self, entry_id: &str) -> Option<LibraryEntry> {
        self.library.entries.remove(entry_id)
    }

    /// Looks up a [`LibraryEntry`] by `entry_id`.
    pub fn get(&self, entry_id: &str) -> Option<&LibraryEntry> {
        self.library.entries.get(entry_id)
    }

    /// Iterates over all entries in deterministic `entry_id` order.
    pub fn entries(&self) -> impl Iterator<Item = &LibraryEntry> {
        self.library.entries.values()
    }
}

/// Computes the deterministic `entry_id` (`<game_id>--<12-hex-hash>`) for an installation folder.
pub fn compute_entry_id(game_id: &str, install_dir: &Path) -> String {
    let norm = normalized_path_key(install_dir);
    let digest = sha256_bytes(norm.as_bytes());
    format!("{game_id}--{}", &digest[..12])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hasher::ComputedHashes;

    fn sample_match(game_id: &str, install_dir: &Path) -> ScanMatch {
        ScanMatch {
            game_id: game_id.to_owned(),
            title: format!("Title {game_id}"),
            release_id: "retail-v1.3".to_owned(),
            release_label: "Retail v1.3".to_owned(),
            game_root: install_dir.to_path_buf(),
            executable_path: install_dir.join("speed.exe"),
            confidence: MatchConfidence::ExactSha256,
            observed_hashes: Some(ComputedHashes {
                file_size_bytes: 1024,
                sha256: std::iter::repeat_n('a', 64).collect(),
                md5: std::iter::repeat_n('b', 32).collect(),
            }),
            matched_sentinels: vec!["GLOBAL/GlobalB.lzc".to_owned()],
            missing_sentinels: Vec::new(),
        }
    }

    #[test]
    fn persists_state_and_preserves_user_fix_decisions_across_rescans() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let lib_path = dir.path().join("nested").join("library.json");
        let install_a = dir.path().join("Games").join("NFS_MW");
        let install_b = dir.path().join("Games").join("NFS_MW_SecondCopy");

        let mut store = LibraryStore::open(&lib_path).expect("open new store");
        assert!(store.add_scan_folder(dir.path().join("Games")).expect("add folder"));
        assert!(!store.add_scan_folder(dir.path().join("Games")).expect("dedup folder"));

        let m_a = sample_match("nfs-most-wanted-2005", &install_a);
        let m_b = sample_match("nfs-most-wanted-2005", &install_b);

        let first_id = store.upsert_scan_match(&m_a).entry_id.clone();
        let second_id = store.upsert_scan_match(&m_b).entry_id.clone();
        assert_ne!(first_id, second_id, "separate install folders must have distinct IDs");

        store
            .record_applied_fixes(
                &first_id,
                vec!["widescreen_fix".to_owned()],
                vec!["dgvoodoo2_wrapper".to_owned()],
            )
            .expect("record applied fixes");
        store
            .mark_declined_vanilla(&second_id, vec!["widescreen_fix".to_owned()])
            .expect("decline vanilla");
        store.record_launched(&first_id).expect("record launch");
        store.save().expect("save library");

        // Reload from disk and rescan
        let mut reloaded = LibraryStore::open(&lib_path).expect("reload library");
        reloaded.upsert_scan_match(&m_a);
        reloaded.upsert_scan_match(&m_b);

        let entry_first = reloaded.get(&first_id).expect("first entry");
        assert_eq!(entry_first.status, GameStatus::Patched);
        assert_eq!(entry_first.applied_fix_ids, vec!["widescreen_fix"]);
        assert_eq!(entry_first.declined_fix_ids, vec!["dgvoodoo2_wrapper"]);
        assert!(entry_first.last_launched_at.is_some());

        let entry_second = reloaded.get(&second_id).expect("second entry");
        assert_eq!(entry_second.status, GameStatus::DeclinedVanilla);

        // Rolling back all fixes transitions back to NeedsFixes
        reloaded.mark_rolled_back(&first_id, Vec::new()).expect("rollback status");
        assert_eq!(reloaded.get(&first_id).expect("first entry").status, GameStatus::NeedsFixes);
    }
}
