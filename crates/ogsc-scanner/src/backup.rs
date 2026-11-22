//! Content-addressed transactional backup and rollback journal manager.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::Utc;
use ogsc_recipes::RegistryHive;
use serde::{Deserialize, Serialize};

use crate::error::ScannerError;
use crate::hasher::{compute_sha256, sha256_bytes};
use crate::scanner::clean_absolute_path;

/// Schema version for on-disk `<backup_dir>/journal.json` files.
pub const JOURNAL_SCHEMA_VERSION: u32 = 1;

/// Transactional manifest recording pre-patch vanilla snapshots and per-fix changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupJournal {
    /// Integer format version (must match [`JOURNAL_SCHEMA_VERSION`]).
    pub journal_version: u32,
    /// Owning recipe `game_id`.
    pub game_id: String,
    /// Matched `release_id`.
    pub release_id: String,
    /// RFC 3339 UTC timestamp when the backup journal was created.
    pub created_at: String,
    /// RFC 3339 UTC timestamp of the last recorded transaction.
    pub updated_at: String,
    /// True pre-patch vanilla state of every file path modified or created by any fix.
    #[serde(default)]
    pub original_files: BTreeMap<String, OriginalFileSnapshot>,
    /// Chronological list of currently active `fix_id`s (used to replay rollbacks in reverse).
    #[serde(default)]
    pub fix_order: Vec<String>,
    /// Per-fix transaction logs keyed by `fix_id`.
    #[serde(default)]
    pub applied_fixes: BTreeMap<String, FixTransaction>,
}

/// Vanilla state of a game-relative path captured before the first fix touched it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OriginalFileSnapshot {
    /// File existed before patching; its original bytes are stored at `blobs/<sha256>`.
    Existed {
        /// Lowercase SHA-256 of the backed-up original file.
        sha256: String,
        /// Byte size of the backed-up original file.
        size_bytes: u64,
    },
    /// File did not exist before patching and should be deleted when no active fix needs it.
    DidNotExist,
}

/// Ordered record of files, directories, and registry keys touched by one [`FixOption`](ogsc_recipes::FixOption).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixTransaction {
    /// Fix identifier (`FixOption::id`).
    pub fix_id: String,
    /// RFC 3339 UTC timestamp when this fix began recording changes.
    pub applied_at: String,
    /// Game-relative file paths created or modified by this fix.
    #[serde(default)]
    pub touched_files: Vec<String>,
    /// Game-relative directories created by this fix.
    #[serde(default)]
    pub created_directories: Vec<String>,
    /// Registry edits recorded by this fix for rollback reporting.
    #[serde(default)]
    pub registry_records: Vec<RegistryChangeRecord>,
}

/// Pre-patch Windows Registry state captured before applying `AppCompatFlags\Layers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryChangeRecord {
    /// Target registry hive (`hkcu` or `hklm`).
    pub hive: RegistryHive,
    /// Registry key path under `hive`.
    pub key_path: String,
    /// Value name (typically the canonical Windows path to the game executable).
    pub value_name: String,
    /// Previous registry value before patching (`None` if the value did not exist).
    pub previous_value: Option<String>,
}

/// Summary of files, directories, and registry records reverted by a rollback operation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackSummary {
    /// Fix IDs rolled back in reverse application order.
    pub rolled_back_fixes: Vec<String>,
    /// Number of original game files restored from verified content-addressed blobs.
    pub restored_files: usize,
    /// Number of newly created patch files deleted.
    pub deleted_files: usize,
    /// Number of empty patch-created directories removed.
    pub removed_directories: usize,
    /// Paths skipped because they matched `preserve_paths` (such as user saves in `.ogsc_sandbox`).
    pub skipped_preserved_paths: Vec<String>,
    /// Registry entries requiring restoration by the Milestone 3 Windows registry handler.
    pub registry_actions_to_revert: Vec<RegistryChangeRecord>,
}

/// Manages content-addressed pre-patch backups (`blobs/<sha256>`) and atomic `journal.json` transactions.
#[derive(Debug, Clone)]
pub struct BackupManager {
    game_root: PathBuf,
    backup_rel_dir: String,
    backup_root: PathBuf,
    journal_path: PathBuf,
    blobs_dir: PathBuf,
    journal: BackupJournal,
}

impl BackupManager {
    /// Opens an existing backup journal in `<game_root>/<backup_dir>` or initializes a new one.
    pub fn open(
        game_root: impl AsRef<Path>,
        backup_dir: &str,
        game_id: impl Into<String>,
        release_id: impl Into<String>,
    ) -> Result<Self, ScannerError> {
        let clean_root = clean_absolute_path(game_root.as_ref())?;
        let norm_backup = normalize_safe_relative_path(backup_dir)?;
        let backup_root = resolve_inside_root(&clean_root, &norm_backup)?;
        let journal_path = backup_root.join("journal.json");
        let blobs_dir = backup_root.join("blobs");

        let journal = if journal_path.is_file() {
            read_journal(&journal_path)?
        } else {
            let now = Utc::now().to_rfc3339();
            BackupJournal {
                journal_version: JOURNAL_SCHEMA_VERSION,
                game_id: game_id.into(),
                release_id: release_id.into(),
                created_at: now.clone(),
                updated_at: now,
                original_files: BTreeMap::new(),
                fix_order: Vec::new(),
                applied_fixes: BTreeMap::new(),
            }
        };

        Ok(Self {
            game_root: clean_root,
            backup_rel_dir: norm_backup,
            backup_root,
            journal_path,
            blobs_dir,
            journal,
        })
    }

    /// Loads an existing backup journal from `<game_root>/<backup_dir>/journal.json` for inspection or rollback.
    pub fn load_existing(
        game_root: impl AsRef<Path>,
        backup_dir: &str,
    ) -> Result<Self, ScannerError> {
        let clean_root = clean_absolute_path(game_root.as_ref())?;
        let norm_backup = normalize_safe_relative_path(backup_dir)?;
        let backup_root = resolve_inside_root(&clean_root, &norm_backup)?;
        let journal_path = backup_root.join("journal.json");
        if !journal_path.is_file() {
            return Err(ScannerError::BackupFileMissing { blob_path: journal_path });
        }
        let journal = read_journal(&journal_path)?;
        let blobs_dir = backup_root.join("blobs");

        Ok(Self {
            game_root: clean_root,
            backup_rel_dir: norm_backup,
            backup_root,
            journal_path,
            blobs_dir,
            journal,
        })
    }

    /// Returns an immutable reference to the loaded [`BackupJournal`].
    pub fn journal(&self) -> &BackupJournal {
        &self.journal
    }

    /// Snapshots `relative_path` before `fix_id` creates or overwrites it, persisting `journal.json` atomically.
    ///
    /// If an earlier fix already captured the true pre-patch vanilla state of `relative_path`, that
    /// original snapshot is preserved intact rather than overwritten with already-patched bytes.
    pub fn backup_file_before_write(
        &mut self,
        fix_id: &str,
        relative_path: &str,
    ) -> Result<(), ScannerError> {
        let norm_rel = normalize_safe_relative_path(relative_path)?;
        if path_matches_prefix(&norm_rel, &self.backup_rel_dir) {
            return Err(ScannerError::InvalidRelativePath {
                path: relative_path.to_owned(),
                reason: "cannot back up paths inside the backup directory itself",
            });
        }

        let target_path = resolve_inside_root(&self.game_root, &norm_rel)?;
        let existing_key = find_existing_key(&self.journal.original_files, &norm_rel);

        if existing_key.is_none() {
            let snapshot = match fs::symlink_metadata(&target_path) {
                Ok(meta) if meta.file_type().is_file() => {
                    let sha256 = compute_sha256(&target_path)?;
                    fs::create_dir_all(&self.blobs_dir).map_err(|source| ScannerError::Io {
                        path: self.blobs_dir.clone(),
                        source,
                    })?;
                    let blob_path = self.blobs_dir.join(&sha256);
                    if !blob_path.exists() {
                        let bytes = fs::read(&target_path).map_err(|source| ScannerError::Io {
                            path: target_path.clone(),
                            source,
                        })?;
                        write_file_atomic(&blob_path, &bytes)?;
                    }
                    OriginalFileSnapshot::Existed { sha256, size_bytes: meta.len() }
                }
                Ok(_) => {
                    return Err(ScannerError::InvalidRelativePath {
                        path: relative_path.to_owned(),
                        reason:
                            "target exists as a directory or symlink rather than a regular file",
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    OriginalFileSnapshot::DidNotExist
                }
                Err(source) => return Err(ScannerError::Io { path: target_path, source }),
            };
            self.journal.original_files.insert(norm_rel.clone(), snapshot);
        }

        let canonical_rel = existing_key.unwrap_or(norm_rel);
        let tx = self.ensure_fix_tx(fix_id);
        if !tx.touched_files.iter().any(|p| paths_equal(p, &canonical_rel)) {
            tx.touched_files.push(canonical_rel);
        }
        self.save_journal()
    }

    /// Records a newly created game-relative directory under `fix_id`.
    pub fn record_created_directory(
        &mut self,
        fix_id: &str,
        relative_path: &str,
    ) -> Result<(), ScannerError> {
        let norm_rel = normalize_safe_relative_path(relative_path)?;
        let _ = resolve_inside_root(&self.game_root, &norm_rel)?;
        let tx = self.ensure_fix_tx(fix_id);
        if !tx.created_directories.iter().any(|p| paths_equal(p, &norm_rel)) {
            tx.created_directories.push(norm_rel);
        }
        self.save_journal()
    }

    /// Records a registry mutation under `fix_id` for rollback reporting and reversal.
    pub fn record_registry_change(
        &mut self,
        fix_id: &str,
        record: RegistryChangeRecord,
    ) -> Result<(), ScannerError> {
        let tx = self.ensure_fix_tx(fix_id);
        tx.registry_records.push(record);
        self.save_journal()
    }

    /// Rolls back a single recorded `fix_id`, preserving any files still needed by other fixes or matching `preserve_paths`.
    pub fn rollback_fix(
        &mut self,
        fix_id: &str,
        preserve_paths: &[String],
    ) -> Result<RollbackSummary, ScannerError> {
        if !self.journal.applied_fixes.contains_key(fix_id) {
            return Err(ScannerError::FixNotRecorded { fix_id: fix_id.to_owned() });
        }

        let norm_preserves = normalize_preserves(preserve_paths)?;
        let (remaining_files, remaining_dirs) = self.remaining_paths_excluding(fix_id);
        let tx = self.journal.applied_fixes.get(fix_id).expect("checked above").clone();

        let (verified_restores, deletions, skipped_preserved) =
            self.prepare_fix_rollback(&tx.touched_files, &remaining_files, &norm_preserves)?;

        let mut summary = RollbackSummary {
            rolled_back_fixes: vec![fix_id.to_owned()],
            skipped_preserved_paths: skipped_preserved,
            registry_actions_to_revert: tx.registry_records,
            ..RollbackSummary::default()
        };

        for (orig_key, dest_path, bytes) in verified_restores {
            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|source| ScannerError::Io { path: parent.to_path_buf(), source })?;
            }
            write_file_atomic(&dest_path, &bytes)?;
            self.journal.original_files.remove(&orig_key);
            summary.restored_files += 1;
        }

        for (orig_key, dest_path) in deletions {
            if dest_path.is_file() {
                clear_readonly_if_exists(&dest_path)?;
                fs::remove_file(&dest_path)
                    .map_err(|source| ScannerError::Io { path: dest_path, source })?;
                summary.deleted_files += 1;
            }
            self.journal.original_files.remove(&orig_key);
        }

        for dir_rel in tx.created_directories.iter().rev() {
            if is_preserved(dir_rel, &norm_preserves) {
                summary.skipped_preserved_paths.push(dir_rel.clone());
                continue;
            }
            if remaining_dirs.contains(&path_lookup_key(dir_rel)) {
                continue;
            }
            let dir_path = resolve_inside_root(&self.game_root, dir_rel)?;
            if dir_path.is_dir() && is_dir_empty(&dir_path)? {
                fs::remove_dir(&dir_path)
                    .map_err(|source| ScannerError::Io { path: dir_path, source })?;
                summary.removed_directories += 1;
            }
        }

        self.journal.applied_fixes.remove(fix_id);
        self.journal.fix_order.retain(|id| id != fix_id);
        self.journal
            .original_files
            .retain(|path, _| remaining_files.contains(&path_lookup_key(path)));
        self.prune_unused_blobs()?;
        self.persist_or_cleanup()?;

        Ok(summary)
    }

    fn remaining_paths_excluding(
        &self,
        excluded_fix_id: &str,
    ) -> (HashSet<String>, HashSet<String>) {
        let mut files = HashSet::new();
        let mut dirs = HashSet::new();
        for (id, tx) in &self.journal.applied_fixes {
            if id.as_str() == excluded_fix_id {
                continue;
            }
            files.extend(tx.touched_files.iter().map(|p| path_lookup_key(p)));
            dirs.extend(tx.created_directories.iter().map(|p| path_lookup_key(p)));
        }
        (files, dirs)
    }

    #[allow(clippy::type_complexity)]
    fn prepare_fix_rollback(
        &self,
        touched_files: &[String],
        remaining_files: &HashSet<String>,
        norm_preserves: &[String],
    ) -> Result<(Vec<(String, PathBuf, Vec<u8>)>, Vec<(String, PathBuf)>, Vec<String>), ScannerError>
    {
        let mut verified_restores = Vec::new();
        let mut deletions = Vec::new();
        let mut skipped_preserved = Vec::new();

        for rel in touched_files.iter().rev() {
            if is_preserved(rel, norm_preserves) {
                skipped_preserved.push(rel.clone());
                continue;
            }
            if remaining_files.contains(&path_lookup_key(rel)) {
                continue;
            }

            let Some(orig_key) = find_existing_key(&self.journal.original_files, rel) else {
                continue;
            };
            let snapshot = self.journal.original_files.get(&orig_key).expect("key exists");
            let dest_path = resolve_inside_root(&self.game_root, &orig_key)?;

            match snapshot {
                OriginalFileSnapshot::Existed { sha256, .. } => {
                    let blob_path = self.blobs_dir.join(sha256);
                    if !blob_path.is_file() {
                        return Err(ScannerError::BackupFileMissing { blob_path });
                    }
                    let actual_sha = compute_sha256(&blob_path)?;
                    if actual_sha != *sha256 {
                        return Err(ScannerError::BackupHashMismatch {
                            relative_path: orig_key,
                            expected: sha256.clone(),
                            actual: actual_sha,
                        });
                    }
                    let bytes = fs::read(&blob_path)
                        .map_err(|source| ScannerError::Io { path: blob_path, source })?;
                    verified_restores.push((orig_key, dest_path, bytes));
                }
                OriginalFileSnapshot::DidNotExist => {
                    deletions.push((orig_key, dest_path));
                }
            }
        }

        Ok((verified_restores, deletions, skipped_preserved))
    }

    /// Rolls back every applied fix in reverse chronological order while protecting `preserve_paths`.
    pub fn rollback_all(
        &mut self,
        preserve_paths: &[String],
    ) -> Result<RollbackSummary, ScannerError> {
        let mut order: Vec<String> = self.journal.fix_order.iter().rev().cloned().collect();
        for id in self.journal.applied_fixes.keys() {
            if !order.contains(id) {
                order.push(id.clone());
            }
        }

        let mut combined = RollbackSummary::default();
        for fix_id in order {
            let part = self.rollback_fix(&fix_id, preserve_paths)?;
            combined.rolled_back_fixes.extend(part.rolled_back_fixes);
            combined.restored_files += part.restored_files;
            combined.deleted_files += part.deleted_files;
            combined.removed_directories += part.removed_directories;
            combined.skipped_preserved_paths.extend(part.skipped_preserved_paths);
            combined.registry_actions_to_revert.extend(part.registry_actions_to_revert);
        }
        Ok(combined)
    }

    fn ensure_fix_tx(&mut self, fix_id: &str) -> &mut FixTransaction {
        let now = Utc::now().to_rfc3339();
        if !self.journal.applied_fixes.contains_key(fix_id) {
            self.journal.fix_order.push(fix_id.to_owned());
            self.journal.applied_fixes.insert(
                fix_id.to_owned(),
                FixTransaction {
                    fix_id: fix_id.to_owned(),
                    applied_at: now,
                    touched_files: Vec::new(),
                    created_directories: Vec::new(),
                    registry_records: Vec::new(),
                },
            );
        }
        self.journal.applied_fixes.get_mut(fix_id).expect("just inserted")
    }

    fn save_journal(&mut self) -> Result<(), ScannerError> {
        self.journal.updated_at = Utc::now().to_rfc3339();
        let json = serde_json::to_string_pretty(&self.journal)
            .map_err(|source| ScannerError::BackupSerialize { source })?;
        write_file_atomic(&self.journal_path, format!("{json}\n").as_bytes())
    }

    fn prune_unused_blobs(&self) -> Result<(), ScannerError> {
        if !self.blobs_dir.is_dir() {
            return Ok(());
        }
        let referenced: HashSet<&str> = self
            .journal
            .original_files
            .values()
            .filter_map(|snap| match snap {
                OriginalFileSnapshot::Existed { sha256, .. } => Some(sha256.as_str()),
                OriginalFileSnapshot::DidNotExist => None,
            })
            .collect();

        let entries = fs::read_dir(&self.blobs_dir)
            .map_err(|source| ScannerError::Io { path: self.blobs_dir.clone(), source })?;
        for entry_res in entries {
            let entry = entry_res
                .map_err(|source| ScannerError::Io { path: self.blobs_dir.clone(), source })?;
            let path = entry.path();
            if path.is_file() {
                let keep = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| referenced.contains(n));
                if !keep {
                    clear_readonly_if_exists(&path)?;
                    fs::remove_file(&path).map_err(|source| ScannerError::Io { path, source })?;
                }
            }
        }
        Ok(())
    }

    fn persist_or_cleanup(&mut self) -> Result<(), ScannerError> {
        if self.journal.applied_fixes.is_empty() && self.journal.original_files.is_empty() {
            if self.journal_path.is_file() {
                clear_readonly_if_exists(&self.journal_path)?;
                fs::remove_file(&self.journal_path).map_err(|source| ScannerError::Io {
                    path: self.journal_path.clone(),
                    source,
                })?;
            }
            if self.blobs_dir.is_dir() && is_dir_empty(&self.blobs_dir)? {
                fs::remove_dir(&self.blobs_dir)
                    .map_err(|source| ScannerError::Io { path: self.blobs_dir.clone(), source })?;
            }
            if self.backup_root.is_dir() && is_dir_empty(&self.backup_root)? {
                fs::remove_dir(&self.backup_root).map_err(|source| ScannerError::Io {
                    path: self.backup_root.clone(),
                    source,
                })?;
            }
            Ok(())
        } else {
            self.save_journal()
        }
    }
}

fn read_journal(path: &Path) -> Result<BackupJournal, ScannerError> {
    let raw = fs::read_to_string(path)
        .map_err(|source| ScannerError::Io { path: path.to_path_buf(), source })?;
    let journal: BackupJournal = serde_json::from_str(&raw)
        .map_err(|source| ScannerError::BackupCorrupted { path: path.to_path_buf(), source })?;
    if journal.journal_version != JOURNAL_SCHEMA_VERSION {
        return Err(ScannerError::BackupUnsupportedVersion { version: journal.journal_version });
    }
    Ok(journal)
}

pub(crate) fn write_file_atomic(dest: &Path, bytes: &[u8]) -> Result<(), ScannerError> {
    let parent = dest.parent().ok_or_else(|| ScannerError::Io {
        path: dest.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path has no parent directory",
        ),
    })?;
    fs::create_dir_all(parent)
        .map_err(|source| ScannerError::Io { path: parent.to_path_buf(), source })?;

    let stamp = sha256_bytes(dest.to_string_lossy().as_bytes());
    let temp_name = format!(".ogsc_tmp_{}_{}.part", std::process::id(), &stamp[..12]);
    let temp_path = parent.join(temp_name);

    {
        let mut file = File::create(&temp_path)
            .map_err(|source| ScannerError::Io { path: temp_path.clone(), source })?;
        file.write_all(bytes)
            .map_err(|source| ScannerError::Io { path: temp_path.clone(), source })?;
        file.sync_all().map_err(|source| ScannerError::Io { path: temp_path.clone(), source })?;
    }

    clear_readonly_if_exists(dest)?;

    fs::rename(&temp_path, dest).map_err(|source| {
        let _ = fs::remove_file(&temp_path);
        ScannerError::Io { path: dest.to_path_buf(), source }
    })?;

    Ok(())
}

fn clear_readonly_if_exists(path: &Path) -> Result<(), ScannerError> {
    match fs::metadata(path) {
        Ok(meta) => {
            let mut perms = meta.permissions();
            if perms.readonly() {
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                fs::set_permissions(path, perms)
                    .map_err(|source| ScannerError::Io { path: path.to_path_buf(), source })?;
            }
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ScannerError::Io { path: path.to_path_buf(), source }),
    }
}

fn normalize_safe_relative_path(raw: &str) -> Result<String, ScannerError> {
    if raw.is_empty() {
        return Err(ScannerError::InvalidRelativePath {
            path: raw.to_owned(),
            reason: "path must not be empty",
        });
    }
    if raw.chars().any(|c| c == '\0' || c.is_control()) {
        return Err(ScannerError::InvalidRelativePath {
            path: raw.to_owned(),
            reason: "path contains NUL or control characters",
        });
    }
    if raw.starts_with('/') || raw.starts_with('\\') {
        return Err(ScannerError::InvalidRelativePath {
            path: raw.to_owned(),
            reason: "path must be relative, not rooted or UNC",
        });
    }
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(ScannerError::InvalidRelativePath {
            path: raw.to_owned(),
            reason: "Windows drive prefixes are forbidden",
        });
    }

    let mut segments = Vec::new();
    for seg in raw.split(['/', '\\']) {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(ScannerError::InvalidRelativePath {
                path: raw.to_owned(),
                reason: "empty, `.`, and `..` path segments are forbidden",
            });
        }
        if seg.ends_with(' ') || seg.ends_with('.') {
            return Err(ScannerError::InvalidRelativePath {
                path: raw.to_owned(),
                reason: "segments cannot end with spaces or `.`",
            });
        }
        if seg.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*')) {
            return Err(ScannerError::InvalidRelativePath {
                path: raw.to_owned(),
                reason: "path contains reserved Windows filename characters",
            });
        }
        let stem = seg.split('.').next().unwrap_or(seg).to_ascii_uppercase();
        if matches!(
            stem.as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        ) {
            return Err(ScannerError::InvalidRelativePath {
                path: raw.to_owned(),
                reason: "path contains a reserved Windows DOS device name",
            });
        }
        segments.push(seg);
    }

    Ok(segments.join("/"))
}

fn resolve_inside_root(game_root: &Path, norm_rel: &str) -> Result<PathBuf, ScannerError> {
    let mut target = game_root.to_path_buf();
    for seg in norm_rel.split('/') {
        target.push(seg);
    }

    if game_root.exists() {
        let canonical_root = fs::canonicalize(game_root)
            .map_err(|source| ScannerError::Io { path: game_root.to_path_buf(), source })?;
        let mut probe = target.as_path();
        while !probe.exists() {
            let Some(parent) = probe.parent() else {
                break;
            };
            probe = parent;
        }
        if probe.exists() {
            let canonical_probe = fs::canonicalize(probe)
                .map_err(|source| ScannerError::Io { path: probe.to_path_buf(), source })?;
            if !canonical_probe.starts_with(&canonical_root) {
                return Err(ScannerError::PathEscape { path: norm_rel.to_owned() });
            }
        }
    }

    Ok(target)
}

fn normalize_preserves(preserves: &[String]) -> Result<Vec<String>, ScannerError> {
    preserves.iter().map(|p| normalize_safe_relative_path(p)).collect()
}

fn is_preserved(rel: &str, norm_preserves: &[String]) -> bool {
    norm_preserves.iter().any(|prefix| path_matches_prefix(rel, prefix))
}

fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    let p = path_lookup_key(path);
    let pre = path_lookup_key(prefix);
    p == pre || p.starts_with(&format!("{pre}/"))
}

fn path_lookup_key(path: &str) -> String {
    if cfg!(windows) {
        path.to_ascii_lowercase()
    } else {
        path.to_owned()
    }
}

fn paths_equal(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn find_existing_key<V>(map: &BTreeMap<String, V>, rel: &str) -> Option<String> {
    map.keys().find(|k| paths_equal(k, rel)).cloned()
}

fn is_dir_empty(path: &Path) -> Result<bool, ScannerError> {
    let mut entries = fs::read_dir(path)
        .map_err(|source| ScannerError::Io { path: path.to_path_buf(), source })?;
    Ok(entries.next().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backs_up_and_restores_overlapping_fixes_and_preserves_saves() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let root = dir.path();
        fs::create_dir_all(root.join("scripts")).expect("mkdir scripts");
        let original_ini = b"[MAIN]\r\nResX = 800\r\n";
        fs::write(root.join("scripts").join("fix.ini"), original_ini).expect("write vanilla ini");

        let mut mgr = BackupManager::open(root, ".ogsc_backup", "nfs-mw", "retail-cd")
            .expect("open backup manager");

        // Fix 1 modifies existing fix.ini and creates dinput8.dll
        mgr.backup_file_before_write("widescreen_fix", "scripts/fix.ini")
            .expect("backup ini for fix 1");
        mgr.backup_file_before_write("widescreen_fix", "dinput8.dll").expect("backup new dll");
        fs::write(root.join("scripts").join("fix.ini"), b"[MAIN]\r\nResX = 1920\r\n")
            .expect("patch ini 1");
        fs::write(root.join("dinput8.dll"), b"ASI_LOADER").expect("write dll");

        // Fix 2 also modifies scripts/fix.ini and creates .ogsc_sandbox/saves/career.sav
        mgr.backup_file_before_write("portable_sandbox", "scripts/fix.ini")
            .expect("backup ini for fix 2");
        mgr.record_created_directory("portable_sandbox", ".ogsc_sandbox")
            .expect("record sandbox dir");
        mgr.record_created_directory("portable_sandbox", ".ogsc_sandbox/saves")
            .expect("record saves dir");
        mgr.backup_file_before_write("portable_sandbox", ".ogsc_sandbox/saves/career.sav")
            .expect("backup save");
        fs::write(
            root.join("scripts").join("fix.ini"),
            b"[MAIN]\r\nResX = 1920\r\n[MISC]\r\nCustomUserFilesDirectoryInGameDir = .ogsc_sandbox/saves\r\n",
        )
        .expect("patch ini 2");
        fs::create_dir_all(root.join(".ogsc_sandbox").join("saves")).expect("mkdir saves");
        fs::write(root.join(".ogsc_sandbox").join("saves").join("career.sav"), b"PLAYER_SAVE")
            .expect("write player save");

        // Roll back only Fix 2 while preserving `.ogsc_sandbox` saves
        let preserves = vec![".ogsc_sandbox".to_owned()];
        let sum2 = mgr.rollback_fix("portable_sandbox", &preserves).expect("rollback fix 2");
        assert_eq!(sum2.restored_files, 0, "scripts/fix.ini still referenced by widescreen_fix");
        assert!(root.join(".ogsc_sandbox").join("saves").join("career.sav").is_file());
        assert!(root.join("scripts").join("fix.ini").is_file());

        // Roll back Fix 1 -> original vanilla fix.ini is restored byte-for-byte, dinput8.dll removed!
        let sum1 = mgr.rollback_fix("widescreen_fix", &preserves).expect("rollback fix 1");
        assert_eq!(sum1.restored_files, 1);
        assert_eq!(sum1.deleted_files, 1);
        assert_eq!(fs::read(root.join("scripts").join("fix.ini")).expect("read ini"), original_ini);
        assert!(!root.join("dinput8.dll").exists());
        assert!(root.join(".ogsc_sandbox").join("saves").join("career.sav").is_file());
        assert!(!root.join(".ogsc_backup").exists(), "empty backup dir should clean up");
    }

    #[test]
    fn detects_corrupted_backup_blob_before_overwriting_game_file() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let root = dir.path();
        fs::write(root.join("game.ini"), b"ORIGINAL").expect("write original");

        let mut mgr = BackupManager::open(root, ".ogsc_backup", "g", "r").expect("open");
        mgr.backup_file_before_write("fix_a", "game.ini").expect("backup");
        fs::write(root.join("game.ini"), b"PATCHED").expect("overwrite");

        // Corrupt the stored blob on disk
        let OriginalFileSnapshot::Existed { sha256, .. } =
            &mgr.journal().original_files["game.ini"]
        else {
            panic!("expected Existed snapshot");
        };
        fs::write(root.join(".ogsc_backup").join("blobs").join(sha256), b"TAMPERED")
            .expect("corrupt blob");

        let err = mgr.rollback_all(&[]).expect_err("corrupted blob must fail verification");
        assert!(matches!(err, ScannerError::BackupHashMismatch { .. }));
        assert_eq!(fs::read(root.join("game.ini")).expect("read game.ini"), b"PATCHED");
    }

    #[test]
    fn rejects_path_traversal_and_reserved_names() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut mgr = BackupManager::open(dir.path(), ".ogsc_backup", "g", "r").expect("open");

        for bad in ["../escape.dll", r"C:\Windows\foo.dll", "NUL.ini", ".ogsc_backup/journal.json"]
        {
            let err = mgr
                .backup_file_before_write("fix_a", bad)
                .expect_err("unsafe path must be rejected");
            assert!(matches!(err, ScannerError::InvalidRelativePath { .. }));
        }
    }

    #[test]
    fn backs_up_overwrites_and_rolls_back_readonly_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let root = dir.path();
        let existing_path = root.join("bin").join("game.exe");
        fs::create_dir_all(existing_path.parent().expect("parent")).expect("mkdir bin");
        let vanilla = b"VANILLA_CD_EXE";
        fs::write(&existing_path, vanilla).expect("write vanilla exe");

        let mut perms = fs::metadata(&existing_path).expect("meta").permissions();
        perms.set_readonly(true);
        fs::set_permissions(&existing_path, perms).expect("set readonly");

        let mut mgr = BackupManager::open(root, ".ogsc_backup", "barbie-2006", "retail")
            .expect("open backup manager");
        mgr.backup_file_before_write("exe_fix", "bin/game.exe").expect("backup readonly exe");
        write_file_atomic(&existing_path, b"PATCHED_EXE").expect("overwrite readonly exe");
        assert_eq!(fs::read(&existing_path).expect("read patched"), b"PATCHED_EXE");

        let created_path = root.join("d3d9.dll");
        mgr.backup_file_before_write("exe_fix", "d3d9.dll").expect("record newly created dll");
        fs::write(&created_path, b"WRAPPER").expect("write created dll");
        let mut created_perms = fs::metadata(&created_path).expect("meta").permissions();
        created_perms.set_readonly(true);
        fs::set_permissions(&created_path, created_perms).expect("set created dll readonly");

        let summary = mgr.rollback_all(&[]).expect("rollback readonly files");
        assert_eq!(summary.restored_files, 1);
        assert_eq!(summary.deleted_files, 1);
        assert_eq!(fs::read(&existing_path).expect("read restored"), vanilla);
        assert!(!created_path.exists());
        assert!(!root.join(".ogsc_backup").exists(), "backup root should clean up completely");
    }
}
