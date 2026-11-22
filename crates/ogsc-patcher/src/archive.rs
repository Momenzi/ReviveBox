//! Zip-Slip-safe `.zip` and `.7z` archive extraction integrated with [`BackupManager`].

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use glob_match::glob_match;
use ogsc_recipes::ArchiveFormat;
use ogsc_scanner::{sha256_bytes, BackupManager};
use sevenz_rust::{Password, SevenZReader};
use zip::ZipArchive;

use crate::error::PatcherError;
use crate::events::{EventSink, PatchEvent};

/// Options controlling archive extraction into a game installation folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractArchiveOptions<'a> {
    /// Owning fix ID for transactional backup records.
    pub fix_id: &'a str,
    /// Archive container format (`zip` or `7z`).
    pub format: ArchiveFormat,
    /// Game-relative destination directory (`"."` for game root).
    pub extract_to: &'a str,
    /// Optional glob filters; when empty, all regular files are extracted.
    pub include_patterns: &'a [String],
    /// Number of leading path components to strip from archive entry paths.
    pub strip_components: u8,
    /// Whether existing files at destination may be overwritten (originals are backed up first).
    pub overwrite_existing: bool,
}

/// Extracts matching files from a `.zip` or `.7z` archive safely into `game_root`,
/// backing up every modified/created file via `backup`.
pub fn extract_archive(
    archive_path: &Path,
    game_root: &Path,
    opts: &ExtractArchiveOptions<'_>,
    backup: &mut BackupManager,
    sink: &mut dyn EventSink,
) -> Result<Vec<String>, PatcherError> {
    let norm_dest_dir = normalize_safe_relative(opts.extract_to, true)?;
    let mut extracted = Vec::new();

    match opts.format {
        ArchiveFormat::Zip => {
            extract_zip_archive(
                archive_path,
                game_root,
                &norm_dest_dir,
                opts,
                backup,
                sink,
                &mut extracted,
            )?;
        }
        ArchiveFormat::SevenZ => {
            extract_sevenz_archive(
                archive_path,
                game_root,
                &norm_dest_dir,
                opts,
                backup,
                sink,
                &mut extracted,
            )?;
        }
    }

    Ok(extracted)
}

fn extract_zip_archive(
    archive_path: &Path,
    game_root: &Path,
    norm_dest_dir: &str,
    opts: &ExtractArchiveOptions<'_>,
    backup: &mut BackupManager,
    sink: &mut dyn EventSink,
    extracted: &mut Vec<String>,
) -> Result<(), PatcherError> {
    let file = File::open(archive_path)
        .map_err(|source| PatcherError::Io { path: archive_path.to_path_buf(), source })?;
    let mut archive = ZipArchive::new(file).map_err(|err| PatcherError::ArchiveError {
        path: archive_path.to_path_buf(),
        reason: err.to_string(),
    })?;

    for idx in 0..archive.len() {
        let mut entry = archive.by_index(idx).map_err(|err| PatcherError::ArchiveError {
            path: archive_path.to_path_buf(),
            reason: err.to_string(),
        })?;
        if entry.is_dir() {
            continue;
        }
        let raw_name = entry.name().to_owned();
        let Some(dest_rel) = prepare_entry_dest(
            &raw_name,
            norm_dest_dir,
            opts.strip_components,
            opts.include_patterns,
        )?
        else {
            continue;
        };

        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(|err| PatcherError::ArchiveError {
            path: archive_path.to_path_buf(),
            reason: format!("failed reading entry `{raw_name}`: {err}"),
        })?;

        write_extracted_entry(
            game_root,
            opts.fix_id,
            &dest_rel,
            &bytes,
            opts.overwrite_existing,
            backup,
            sink,
            extracted,
        )?;
    }

    Ok(())
}

/// Safely extracts all regular files from a verified official toolkit `.zip` archive into
/// `dest_dir` with Zip-Slip path traversal protection.
pub(crate) fn extract_zip_into_toolkit_dir(
    archive_path: &Path,
    dest_dir: &Path,
) -> Result<usize, PatcherError> {
    let file = File::open(archive_path)
        .map_err(|source| PatcherError::Io { path: archive_path.to_path_buf(), source })?;
    let mut archive = ZipArchive::new(file).map_err(|err| PatcherError::ArchiveError {
        path: archive_path.to_path_buf(),
        reason: err.to_string(),
    })?;

    fs::create_dir_all(dest_dir)
        .map_err(|source| PatcherError::Io { path: dest_dir.to_path_buf(), source })?;

    let mut extracted_count = 0_usize;
    for idx in 0..archive.len() {
        let mut entry = archive.by_index(idx).map_err(|err| PatcherError::ArchiveError {
            path: archive_path.to_path_buf(),
            reason: err.to_string(),
        })?;
        if entry.is_dir() {
            continue;
        }
        let raw_name = entry.name().trim_end_matches(['/', '\\']).to_owned();
        if raw_name.is_empty() {
            continue;
        }
        let norm_rel = normalize_safe_relative(&raw_name, false)?;
        let dest_abs = join_abs_rel(dest_dir, &norm_rel);
        if let Some(parent) = dest_abs.parent() {
            fs::create_dir_all(parent)
                .map_err(|source| PatcherError::Io { path: parent.to_path_buf(), source })?;
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(|err| PatcherError::ArchiveError {
            path: archive_path.to_path_buf(),
            reason: format!("failed reading toolkit archive entry `{raw_name}`: {err}"),
        })?;
        write_file_clearing_readonly(&dest_abs, &bytes)?;
        extracted_count += 1;
    }

    Ok(extracted_count)
}

fn extract_sevenz_archive(
    archive_path: &Path,
    game_root: &Path,
    norm_dest_dir: &str,
    opts: &ExtractArchiveOptions<'_>,
    backup: &mut BackupManager,
    sink: &mut dyn EventSink,
    extracted: &mut Vec<String>,
) -> Result<(), PatcherError> {
    let mut reader = SevenZReader::open(archive_path, Password::empty()).map_err(|err| {
        PatcherError::ArchiveError { path: archive_path.to_path_buf(), reason: err.to_string() }
    })?;
    let mut deferred_error: Option<PatcherError> = None;

    reader
        .for_each_entries(|entry, entry_reader| {
            if entry.is_directory() {
                return Ok(true);
            }
            let raw_name = entry.name().to_owned();
            let dest_rel = match prepare_entry_dest(
                &raw_name,
                norm_dest_dir,
                opts.strip_components,
                opts.include_patterns,
            ) {
                Ok(Some(rel)) => rel,
                Ok(None) => return Ok(true),
                Err(err) => {
                    deferred_error = Some(err);
                    return Ok(false);
                }
            };

            let mut bytes = Vec::new();
            if let Err(err) = entry_reader.read_to_end(&mut bytes) {
                deferred_error = Some(PatcherError::ArchiveError {
                    path: archive_path.to_path_buf(),
                    reason: format!("failed reading 7z entry `{raw_name}`: {err}"),
                });
                return Ok(false);
            }

            if let Err(err) = write_extracted_entry(
                game_root,
                opts.fix_id,
                &dest_rel,
                &bytes,
                opts.overwrite_existing,
                backup,
                sink,
                extracted,
            ) {
                deferred_error = Some(err);
                return Ok(false);
            }
            Ok(true)
        })
        .map_err(|err| PatcherError::ArchiveError {
            path: archive_path.to_path_buf(),
            reason: err.to_string(),
        })?;

    if let Some(err) = deferred_error {
        return Err(err);
    }
    Ok(())
}

/// Extracts a single named member from a `.zip` or `.7z` archive, optionally verifying its SHA-256.
#[allow(clippy::too_many_arguments)]
pub fn extract_single_archive_member(
    archive_path: &Path,
    format: ArchiveFormat,
    archive_member: &str,
    game_root: &Path,
    fix_id: &str,
    destination_path: &str,
    expected_sha256: Option<&str>,
    backup_original: bool,
    backup: &mut BackupManager,
    sink: &mut dyn EventSink,
) -> Result<(), PatcherError> {
    let target_member = normalize_safe_relative(archive_member, false)?;
    let dest_rel = normalize_safe_relative(destination_path, false)?;
    let bytes = read_archive_member(archive_path, format, &target_member)?;

    if let Some(expected) = expected_sha256 {
        let actual = sha256_bytes(&bytes);
        if actual != expected.to_ascii_lowercase() {
            return Err(PatcherError::HashMismatch {
                subject: format!("{}:{target_member}", archive_path.display()),
                expected: expected.to_ascii_lowercase(),
                actual,
            });
        }
    }

    let dest_abs = join_abs_rel(game_root, &dest_rel);
    if backup_original {
        backup.backup_file_before_write(fix_id, &dest_rel)?;
        sink.emit(PatchEvent::FileBackedUp {
            fix_id: fix_id.to_owned(),
            relative_path: dest_rel.clone(),
        });
    }

    ensure_parent_dirs(game_root, &dest_rel, fix_id, backup)?;
    write_file_clearing_readonly(&dest_abs, &bytes)?;
    sink.emit(PatchEvent::FileWritten { fix_id: fix_id.to_owned(), relative_path: dest_rel });
    Ok(())
}

fn read_archive_member(
    archive_path: &Path,
    format: ArchiveFormat,
    target_member: &str,
) -> Result<Vec<u8>, PatcherError> {
    match format {
        ArchiveFormat::Zip => {
            let file = File::open(archive_path)
                .map_err(|source| PatcherError::Io { path: archive_path.to_path_buf(), source })?;
            let mut archive = ZipArchive::new(file).map_err(|err| PatcherError::ArchiveError {
                path: archive_path.to_path_buf(),
                reason: err.to_string(),
            })?;

            for idx in 0..archive.len() {
                let mut entry =
                    archive.by_index(idx).map_err(|err| PatcherError::ArchiveError {
                        path: archive_path.to_path_buf(),
                        reason: err.to_string(),
                    })?;
                if entry.is_dir() {
                    continue;
                }
                let Ok(norm_entry) = normalize_safe_relative(entry.name(), false) else {
                    continue;
                };
                if entry_matches(&norm_entry, target_member) {
                    let mut bytes = Vec::new();
                    entry.read_to_end(&mut bytes).map_err(|err| PatcherError::ArchiveError {
                        path: archive_path.to_path_buf(),
                        reason: err.to_string(),
                    })?;
                    return Ok(bytes);
                }
            }
        }
        ArchiveFormat::SevenZ => {
            let mut reader =
                SevenZReader::open(archive_path, Password::empty()).map_err(|err| {
                    PatcherError::ArchiveError {
                        path: archive_path.to_path_buf(),
                        reason: err.to_string(),
                    }
                })?;
            let mut found: Option<Vec<u8>> = None;
            let mut read_err: Option<PatcherError> = None;

            reader
                .for_each_entries(|entry, entry_reader| {
                    if entry.is_directory() {
                        return Ok(true);
                    }
                    let Ok(norm_entry) = normalize_safe_relative(entry.name(), false) else {
                        return Ok(true);
                    };
                    if entry_matches(&norm_entry, target_member) {
                        let mut bytes = Vec::new();
                        if let Err(err) = entry_reader.read_to_end(&mut bytes) {
                            read_err = Some(PatcherError::ArchiveError {
                                path: archive_path.to_path_buf(),
                                reason: err.to_string(),
                            });
                        } else {
                            found = Some(bytes);
                        }
                        return Ok(false);
                    }
                    Ok(true)
                })
                .map_err(|err| PatcherError::ArchiveError {
                    path: archive_path.to_path_buf(),
                    reason: err.to_string(),
                })?;

            if let Some(err) = read_err {
                return Err(err);
            }
            if let Some(bytes) = found {
                return Ok(bytes);
            }
        }
    }

    Err(PatcherError::ArchiveMemberNotFound {
        archive: archive_path.to_path_buf(),
        member: target_member.to_owned(),
    })
}

fn prepare_entry_dest(
    raw_entry_name: &str,
    norm_dest_dir: &str,
    strip_components: u8,
    include_patterns: &[String],
) -> Result<Option<String>, PatcherError> {
    let norm_entry = normalize_safe_relative(raw_entry_name.trim_end_matches(['/', '\\']), false)?;

    let parts: Vec<&str> = norm_entry.split('/').collect();
    if parts.len() <= usize::from(strip_components) {
        return Ok(None);
    }
    let stripped = parts[usize::from(strip_components)..].join("/");

    if !include_patterns.is_empty() && !matches_any_glob(&stripped, include_patterns) {
        return Ok(None);
    }

    let dest_rel =
        if norm_dest_dir == "." { stripped } else { format!("{norm_dest_dir}/{stripped}") };
    Ok(Some(dest_rel))
}

#[allow(clippy::too_many_arguments)]
fn write_extracted_entry(
    game_root: &Path,
    fix_id: &str,
    dest_rel: &str,
    bytes: &[u8],
    overwrite_existing: bool,
    backup: &mut BackupManager,
    sink: &mut dyn EventSink,
    extracted: &mut Vec<String>,
) -> Result<(), PatcherError> {
    let dest_abs = join_abs_rel(game_root, dest_rel);
    if dest_abs.exists() && !overwrite_existing {
        return Ok(());
    }

    backup.backup_file_before_write(fix_id, dest_rel)?;
    sink.emit(PatchEvent::FileBackedUp {
        fix_id: fix_id.to_owned(),
        relative_path: dest_rel.to_owned(),
    });

    ensure_parent_dirs(game_root, dest_rel, fix_id, backup)?;
    write_file_clearing_readonly(&dest_abs, bytes)?;
    sink.emit(PatchEvent::FileWritten {
        fix_id: fix_id.to_owned(),
        relative_path: dest_rel.to_owned(),
    });
    extracted.push(dest_rel.to_owned());
    Ok(())
}

pub(crate) fn write_file_clearing_readonly(path: &Path, bytes: &[u8]) -> Result<(), PatcherError> {
    match fs::metadata(path) {
        Ok(meta) => {
            let mut perms = meta.permissions();
            if perms.readonly() {
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                fs::set_permissions(path, perms)
                    .map_err(|source| PatcherError::Io { path: path.to_path_buf(), source })?;
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(PatcherError::Io { path: path.to_path_buf(), source }),
    }
    fs::write(path, bytes).map_err(|source| PatcherError::Io { path: path.to_path_buf(), source })
}

pub(crate) fn ensure_parent_dirs(
    game_root: &Path,
    norm_rel_file: &str,
    fix_id: &str,
    backup: &mut BackupManager,
) -> Result<(), PatcherError> {
    let Some(parent_str) = norm_rel_file.rsplit_once('/').map(|(p, _)| p) else {
        return Ok(());
    };
    let mut cur_rel = String::new();
    let mut cur_abs = game_root.to_path_buf();
    for seg in parent_str.split('/').filter(|s| !s.is_empty()) {
        if !cur_rel.is_empty() {
            cur_rel.push('/');
        }
        cur_rel.push_str(seg);
        cur_abs.push(seg);
        if !cur_abs.exists() {
            fs::create_dir(&cur_abs)
                .map_err(|source| PatcherError::Io { path: cur_abs.clone(), source })?;
            backup.record_created_directory(fix_id, &cur_rel)?;
        }
    }
    Ok(())
}

pub(crate) fn normalize_safe_relative(raw: &str, allow_dot: bool) -> Result<String, PatcherError> {
    if raw.is_empty() {
        return Err(PatcherError::UnsafePath {
            path: raw.to_owned(),
            reason: "path must not be empty",
        });
    }
    if raw == "." {
        return if allow_dot {
            Ok(".".to_owned())
        } else {
            Err(PatcherError::UnsafePath {
                path: raw.to_owned(),
                reason: "`.` is only allowed for directory targets",
            })
        };
    }
    if raw.chars().any(|c| c == '\0' || c.is_control()) {
        return Err(PatcherError::UnsafePath {
            path: raw.to_owned(),
            reason: "path contains control or NUL characters",
        });
    }
    if raw.starts_with('/') || raw.starts_with('\\') {
        return Err(PatcherError::UnsafePath {
            path: raw.to_owned(),
            reason: "path must be relative, not rooted or UNC",
        });
    }
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(PatcherError::UnsafePath {
            path: raw.to_owned(),
            reason: "Windows drive prefixes are forbidden",
        });
    }

    let mut segments = Vec::new();
    for seg in raw.split(['/', '\\']) {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(PatcherError::UnsafePath {
                path: raw.to_owned(),
                reason: "empty, `.`, and `..` traversal segments are forbidden",
            });
        }
        if seg.ends_with(' ') || seg.ends_with('.') {
            return Err(PatcherError::UnsafePath {
                path: raw.to_owned(),
                reason: "segments must not end with spaces or `.`",
            });
        }
        if seg.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*')) {
            return Err(PatcherError::UnsafePath {
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
            return Err(PatcherError::UnsafePath {
                path: raw.to_owned(),
                reason: "path contains a reserved Windows DOS device name",
            });
        }
        segments.push(seg);
    }

    Ok(segments.join("/"))
}

pub(crate) fn join_abs_rel(root: &Path, norm_rel: &str) -> PathBuf {
    if norm_rel == "." {
        return root.to_path_buf();
    }
    let mut out = root.to_path_buf();
    for seg in norm_rel.split('/') {
        out.push(seg);
    }
    out
}

fn matches_any_glob(candidate: &str, patterns: &[String]) -> bool {
    let cand_norm = candidate.replace('\\', "/");
    patterns.iter().any(|pat| {
        let pat_norm = pat.replace('\\', "/");
        if cfg!(windows) {
            glob_match(&pat_norm.to_ascii_lowercase(), &cand_norm.to_ascii_lowercase())
        } else {
            glob_match(&pat_norm, &cand_norm)
        }
    })
}

fn entry_matches(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::events::VecEventSink;

    fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).expect("create zip");
        let mut writer = zip::ZipWriter::new(file);
        let opts = SimpleFileOptions::default();
        for (name, data) in entries {
            writer.start_file(name, opts).expect("start file");
            writer.write_all(data).expect("write data");
        }
        writer.finish().expect("finish zip");
    }

    #[test]
    fn extracts_zip_with_glob_strip_and_rejects_zip_slip() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let zip_path = dir.path().join("pkg.zip");
        let game_root = dir.path().join("Game");
        fs::create_dir_all(&game_root).expect("mkdir game");

        make_zip(
            &zip_path,
            &[
                ("root/dinput8.dll", b"ASI_LOADER"),
                ("root/scripts/fix.asi", b"ASI_PLUGIN"),
                ("root/readme.txt", b"IGNORED"),
            ],
        );

        let mut backup =
            BackupManager::open(&game_root, ".ogsc_backup", "g", "r").expect("open backup");
        let mut sink = VecEventSink::default();
        let patterns = vec!["dinput8.dll".to_owned(), "scripts/*.asi".to_owned()];

        let extracted = extract_archive(
            &zip_path,
            &game_root,
            &ExtractArchiveOptions {
                fix_id: "widescreen_fix",
                format: ArchiveFormat::Zip,
                extract_to: ".",
                include_patterns: &patterns,
                strip_components: 1,
                overwrite_existing: true,
            },
            &mut backup,
            &mut sink,
        )
        .expect("extract zip");

        assert_eq!(extracted, vec!["dinput8.dll", "scripts/fix.asi"]);
        assert_eq!(fs::read(game_root.join("dinput8.dll")).expect("read dll"), b"ASI_LOADER");
        assert!(!game_root.join("readme.txt").exists());

        // Single member extraction with expected SHA-256 verification
        let asi_sha = sha256_bytes(b"ASI_PLUGIN");
        extract_single_archive_member(
            &zip_path,
            ArchiveFormat::Zip,
            "root/scripts/fix.asi",
            &game_root,
            "widescreen_fix",
            "scripts/extracted.asi",
            Some(&asi_sha),
            true,
            &mut backup,
            &mut sink,
        )
        .expect("extract single member");
        assert_eq!(
            fs::read(game_root.join("scripts").join("extracted.asi")).expect("read single"),
            b"ASI_PLUGIN"
        );

        // Zip-Slip path traversal must be rejected
        let evil_zip = dir.path().join("evil.zip");
        make_zip(&evil_zip, &[("../../evil.dll", b"PWN")]);
        let err = extract_archive(
            &evil_zip,
            &game_root,
            &ExtractArchiveOptions {
                fix_id: "evil_fix",
                format: ArchiveFormat::Zip,
                extract_to: ".",
                include_patterns: &[],
                strip_components: 0,
                overwrite_existing: true,
            },
            &mut backup,
            &mut sink,
        )
        .expect_err("zip slip must fail");
        assert!(matches!(err, PatcherError::UnsafePath { .. }));
        assert!(!dir.path().join("evil.dll").exists());
    }

    #[test]
    fn extracts_real_7z_archive() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let src_dir = dir.path().join("src_7z");
        let archive_path = dir.path().join("mod.7z");
        let game_root = dir.path().join("Game");
        fs::create_dir_all(src_dir.join("pkg").join("scripts")).expect("mkdir src");
        fs::create_dir_all(&game_root).expect("mkdir game");
        fs::write(src_dir.join("pkg").join("scripts").join("plugin.asi"), b"SEVENZ_ASI")
            .expect("write asi");

        sevenz_rust::compress_to_path(&src_dir, &archive_path).expect("compress 7z");

        let mut backup =
            BackupManager::open(&game_root, ".ogsc_backup", "g", "r").expect("open backup");
        let mut sink = VecEventSink::default();

        let extracted = extract_archive(
            &archive_path,
            &game_root,
            &ExtractArchiveOptions {
                fix_id: "sz_fix",
                format: ArchiveFormat::SevenZ,
                extract_to: ".",
                include_patterns: &[],
                strip_components: 1,
                overwrite_existing: true,
            },
            &mut backup,
            &mut sink,
        )
        .expect("extract 7z");

        assert_eq!(extracted, vec!["scripts/plugin.asi"]);
        assert_eq!(
            fs::read(game_root.join("scripts").join("plugin.asi")).expect("read extracted"),
            b"SEVENZ_ASI"
        );
    }
}
