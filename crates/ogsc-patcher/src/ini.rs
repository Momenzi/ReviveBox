//! Comment-preserving, encoding-aware INI editor supporting dynamic host parameter substitution.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use encoding_rs::WINDOWS_1252;
use ogsc_recipes::{DynamicValueKind, IniEncoding, IniValue, LineEnding};

use crate::error::PatcherError;

/// Host runtime parameters substituted into [`IniValue::Dynamic`] entries during patching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicContext {
    /// Primary display desktop width in pixels.
    pub desktop_width: u32,
    /// Primary display desktop height in pixels.
    pub desktop_height: u32,
    /// Primary display refresh rate in Hz.
    pub refresh_rate_hz: u32,
    /// Resolved path to the game's portable `.ogsc_sandbox` directory.
    pub game_sandbox_dir: PathBuf,
}

impl DynamicContext {
    /// Resolves a [`DynamicValueKind`] into its formatted INI string value.
    pub fn resolve(&self, kind: DynamicValueKind) -> String {
        match kind {
            DynamicValueKind::DesktopWidth => self.desktop_width.to_string(),
            DynamicValueKind::DesktopHeight => self.desktop_height.to_string(),
            DynamicValueKind::RefreshRateHz => self.refresh_rate_hz.to_string(),
            DynamicValueKind::GameSandboxDir => {
                self.game_sandbox_dir.to_string_lossy().into_owned()
            }
        }
    }
}

/// Formats an [`IniValue`] into its literal INI representation.
pub fn format_ini_value(value: &IniValue, ctx: &DynamicContext) -> String {
    match value {
        IniValue::Dynamic(d) => ctx.resolve(d.dynamic),
        IniValue::Boolean(b) => {
            if *b {
                "True".to_owned()
            } else {
                "False".to_owned()
            }
        }
        IniValue::Integer(i) => i.to_string(),
        IniValue::Float(f) => f.to_string(),
        IniValue::String(s) => s.clone(),
    }
}

/// Updates or creates INI `sections` in `existing_bytes` while preserving comments, blank lines,
/// unrelated sections/keys, and requested character `encoding` / `line_ending`.
pub fn apply_ini_updates(
    existing_bytes: Option<&[u8]>,
    target_file: &str,
    encoding: IniEncoding,
    line_ending: LineEnding,
    sections: &BTreeMap<String, BTreeMap<String, IniValue>>,
    ctx: &DynamicContext,
) -> Result<Vec<u8>, PatcherError> {
    let text = match existing_bytes {
        Some(bytes) => decode_ini_bytes(bytes, target_file, encoding)?,
        None => String::new(),
    };

    let updated = merge_ini_text(&text, sections, line_ending, ctx);
    encode_ini_text(&updated, target_file, encoding)
}

fn decode_ini_bytes(
    bytes: &[u8],
    target_file: &str,
    encoding: IniEncoding,
) -> Result<String, PatcherError> {
    match encoding {
        IniEncoding::Utf8 => {
            let trimmed = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
            std::str::from_utf8(trimmed).map(str::to_owned).map_err(|err| {
                PatcherError::IniEncodingError {
                    target_file: target_file.to_owned(),
                    reason: format!("invalid UTF-8: {err}"),
                }
            })
        }
        IniEncoding::Windows1252 => WINDOWS_1252
            .decode_without_bom_handling_and_without_replacement(bytes)
            .map(std::borrow::Cow::into_owned)
            .ok_or_else(|| PatcherError::IniEncodingError {
                target_file: target_file.to_owned(),
                reason: "bytes contain unmapped Windows-1252 characters".to_owned(),
            }),
        IniEncoding::Utf16Le => decode_utf16le(bytes, target_file),
    }
}

fn decode_utf16le(bytes: &[u8], target_file: &str) -> Result<String, PatcherError> {
    let payload = bytes.strip_prefix(&[0xFF, 0xFE]).unwrap_or(bytes);
    if payload.len() % 2 != 0 {
        return Err(PatcherError::IniEncodingError {
            target_file: target_file.to_owned(),
            reason: "UTF-16LE payload has an odd byte count".to_owned(),
        });
    }
    let units: Vec<u16> =
        payload.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
    String::from_utf16(&units).map_err(|err| PatcherError::IniEncodingError {
        target_file: target_file.to_owned(),
        reason: format!("invalid UTF-16LE sequence: {err}"),
    })
}

fn encode_ini_text(
    text: &str,
    target_file: &str,
    encoding: IniEncoding,
) -> Result<Vec<u8>, PatcherError> {
    match encoding {
        IniEncoding::Utf8 => Ok(text.as_bytes().to_vec()),
        IniEncoding::Windows1252 => {
            let (encoded, _, had_errors) = WINDOWS_1252.encode(text);
            if had_errors {
                return Err(PatcherError::IniEncodingError {
                    target_file: target_file.to_owned(),
                    reason: "INI text contains characters unsupported in Windows-1252".to_owned(),
                });
            }
            Ok(encoded.into_owned())
        }
        IniEncoding::Utf16Le => {
            let mut out = Vec::with_capacity(2 + text.len() * 2);
            out.extend_from_slice(&[0xFF, 0xFE]);
            for unit in text.encode_utf16() {
                out.extend_from_slice(&unit.to_le_bytes());
            }
            Ok(out)
        }
    }
}

fn merge_ini_text(
    text: &str,
    updates: &BTreeMap<String, BTreeMap<String, IniValue>>,
    line_ending: LineEnding,
    ctx: &DynamicContext,
) -> String {
    let eol = match line_ending {
        LineEnding::Crlf => "\r\n",
        LineEnding::Lf => "\n",
    };

    // Map lowercase section name -> original requested section name + lowercase key map.
    let mut section_lookup: BTreeMap<String, &String> = BTreeMap::new();
    for sec in updates.keys() {
        section_lookup.insert(sec.to_ascii_lowercase(), sec);
    }

    let mut sections_handled: BTreeSet<String> = BTreeSet::new();
    let mut keys_written_per_section: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut out_lines: Vec<String> = Vec::new();
    let mut current_sec_lower: Option<String> = None;

    for raw_line in text.lines() {
        let trimmed = raw_line.trim();
        if let Some(sec_name) = parse_section_header(trimmed) {
            flush_missing_keys_for_section(
                current_sec_lower.as_deref(),
                &section_lookup,
                updates,
                &mut keys_written_per_section,
                &mut out_lines,
                ctx,
            );
            let sec_lower = sec_name.to_ascii_lowercase();
            current_sec_lower = Some(sec_lower.clone());
            if section_lookup.contains_key(&sec_lower) {
                sections_handled.insert(sec_lower);
            }
            out_lines.push(raw_line.to_owned());
            continue;
        }

        if let Some(sec_lower) = current_sec_lower.as_deref() {
            if let Some(&canonical_sec) = section_lookup.get(sec_lower) {
                if let Some((key, inline_comment)) = parse_key_line(raw_line) {
                    let key_lower = key.to_ascii_lowercase();
                    if let Some((_, val)) = updates[canonical_sec]
                        .iter()
                        .find(|(k, _)| k.to_ascii_lowercase() == key_lower)
                    {
                        let rendered = format_ini_value(val, ctx);
                        let line = match inline_comment {
                            Some(comment) => format!("{key} = {rendered} {comment}"),
                            None => format!("{key} = {rendered}"),
                        };
                        out_lines.push(line);
                        keys_written_per_section
                            .entry(sec_lower.to_owned())
                            .or_default()
                            .insert(key_lower);
                        continue;
                    }
                }
            }
        }

        out_lines.push(raw_line.to_owned());
    }

    flush_missing_keys_for_section(
        current_sec_lower.as_deref(),
        &section_lookup,
        updates,
        &mut keys_written_per_section,
        &mut out_lines,
        ctx,
    );

    // Append entirely new sections not present in the original INI file.
    for (sec_name, kv_map) in updates {
        let sec_lower = sec_name.to_ascii_lowercase();
        if sections_handled.contains(&sec_lower) {
            continue;
        }
        if !out_lines.is_empty() && out_lines.last().is_some_and(|l| !l.trim().is_empty()) {
            out_lines.push(String::new());
        }
        out_lines.push(format!("[{sec_name}]"));
        for (key, val) in kv_map {
            out_lines.push(format!("{key} = {}", format_ini_value(val, ctx)));
        }
    }

    if out_lines.is_empty() {
        return String::new();
    }
    let mut result = out_lines.join(eol);
    result.push_str(eol);
    result
}

fn flush_missing_keys_for_section(
    current_sec_lower: Option<&str>,
    section_lookup: &BTreeMap<String, &String>,
    updates: &BTreeMap<String, BTreeMap<String, IniValue>>,
    keys_written_per_section: &mut BTreeMap<String, BTreeSet<String>>,
    out_lines: &mut Vec<String>,
    ctx: &DynamicContext,
) {
    let Some(sec_lower) = current_sec_lower else {
        return;
    };
    let Some(&canonical_sec) = section_lookup.get(sec_lower) else {
        return;
    };

    let written = keys_written_per_section.get(sec_lower);
    let mut missing: Vec<String> = Vec::new();
    for (key, val) in &updates[canonical_sec] {
        let k_lower = key.to_ascii_lowercase();
        if written.is_none_or(|set| !set.contains(&k_lower)) {
            missing.push(format!("{key} = {}", format_ini_value(val, ctx)));
        }
    }

    if missing.is_empty() {
        return;
    }

    // Insert new keys right before trailing blank/comment lines at the end of the section.
    let mut insert_idx = out_lines.len();
    while insert_idx > 0 && out_lines[insert_idx - 1].trim().is_empty() {
        insert_idx -= 1;
    }
    for line in missing.into_iter().rev() {
        out_lines.insert(insert_idx, line);
    }
}

fn parse_section_header(trimmed: &str) -> Option<&str> {
    if trimmed.starts_with('[') {
        if let Some(close) = trimmed.find(']') {
            let after = trimmed[close + 1..].trim();
            if after.is_empty() || after.starts_with(';') || after.starts_with('#') {
                return Some(trimmed[1..close].trim());
            }
        }
    }
    None
}

fn parse_key_line(raw_line: &str) -> Option<(&str, Option<&str>)> {
    let trimmed = raw_line.trim_start();
    if trimmed.is_empty()
        || trimmed.starts_with(';')
        || trimmed.starts_with('#')
        || trimmed.starts_with("//")
        || trimmed.starts_with('[')
    {
        return None;
    }
    let (lhs, rhs) = trimmed.split_once('=')?;
    let key = lhs.trim();
    if key.is_empty() {
        return None;
    }
    let comment = find_inline_comment(rhs);
    Some((key, comment))
}

fn find_inline_comment(rhs: &str) -> Option<&str> {
    let mut in_quotes = false;
    for (idx, ch) in rhs.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ';' | '#' if !in_quotes => {
                return Some(rhs[idx..].trim_end());
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use ogsc_recipes::{DynamicIniValue, DynamicValueKind};

    use super::*;

    fn test_ctx() -> DynamicContext {
        DynamicContext {
            desktop_width: 2560,
            desktop_height: 1440,
            refresh_rate_hz: 144,
            game_sandbox_dir: PathBuf::from(r"C:\Games\NFS\.ogsc_sandbox"),
        }
    }

    #[test]
    fn preserves_comments_unrelated_sections_and_substitutes_dynamic_values() {
        let initial = "; Leading comment\r\n[main]\r\nResX = 800 ; native width\r\nCustomUntouched = keep\r\n\r\n[AUDIO]\r\nVolume = 80\r\n";
        let mut updates = BTreeMap::new();
        updates.insert(
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
                ("FixHUD".to_owned(), IniValue::Integer(1)),
            ]),
        );
        updates.insert(
            "MISC".to_owned(),
            BTreeMap::from([("ShadowsRes".to_owned(), IniValue::Integer(4096))]),
        );

        let out = apply_ini_updates(
            Some(initial.as_bytes()),
            "fix.ini",
            IniEncoding::Utf8,
            LineEnding::Crlf,
            &updates,
            &test_ctx(),
        )
        .expect("update utf8 ini");
        let text = String::from_utf8(out).expect("valid utf8");

        assert!(text.contains("; Leading comment\r\n"));
        assert!(text.contains("ResX = 2560 ; native width\r\n"));
        assert!(text.contains("CustomUntouched = keep\r\n"));
        assert!(text.contains("FixHUD = 1\r\n"));
        assert!(text.contains("ResY = 1440\r\n"));
        assert!(text.contains("[AUDIO]\r\nVolume = 80\r\n"));
        assert!(text.contains("[MISC]\r\nShadowsRes = 4096\r\n"));
    }

    #[test]
    fn supports_windows1252_and_utf16le_with_bom() {
        let mut updates = BTreeMap::new();
        updates.insert(
            "WinDrv.WindowsClient".to_owned(),
            BTreeMap::from([
                ("FullscreenViewportX".to_owned(), IniValue::Integer(1920)),
                ("StartupFullscreen".to_owned(), IniValue::Boolean(true)),
            ]),
        );

        // Windows-1252 with French accent character (0xE9 = é)
        let win1252_in =
            b"; R\xe9solution\r\n[WinDrv.WindowsClient]\r\nFullscreenViewportX=800\r\n";
        let out_1252 = apply_ini_updates(
            Some(win1252_in),
            "sc2.ini",
            IniEncoding::Windows1252,
            LineEnding::Crlf,
            &updates,
            &test_ctx(),
        )
        .expect("update win1252");
        assert!(out_1252.windows(b"; R\xe9solution".len()).any(|w| w == b"; R\xe9solution"));
        assert!(out_1252
            .windows(b"FullscreenViewportX = 1920".len())
            .any(|w| w == b"FullscreenViewportX = 1920"));

        // UTF-16LE roundtrip
        let out_u16 = apply_ini_updates(
            None,
            "u16.ini",
            IniEncoding::Utf16Le,
            LineEnding::Crlf,
            &updates,
            &test_ctx(),
        )
        .expect("encode utf16le");
        assert_eq!(&out_u16[..2], &[0xFF, 0xFE]);
        let decoded = decode_utf16le(&out_u16, "u16.ini").expect("decode utf16le");
        assert!(decoded.contains("StartupFullscreen = True\r\n"));
    }
}
