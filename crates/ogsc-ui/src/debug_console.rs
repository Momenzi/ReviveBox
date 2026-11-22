//! Live launch & crash diagnostic console: PE/DRM inspection, Windows NTSTATUS decoding,
//! read-only Application Event Log (`wevtutil`) crash analysis, and bottom drawer UI.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use egui::{CornerRadius, Frame, Margin, RichText, ScrollArea, Stroke, Ui};

use crate::theme::{
    paint_pill, ACCENT_AMBER, ACCENT_CYAN, BG_CONSOLE, BG_SURFACE, BORDER_SUBTLE, STATUS_BLOCKER,
    STATUS_NEEDS_FIXES, STATUS_PATCHED, TEXT_MUTED, TEXT_PRIMARY,
};

const MAX_CONSOLE_LINES: usize = 1500;

const KNOWN_PROXY_FILES: [&str; 10] = [
    "dinput8.dll",
    "d3d9.dll",
    "d3d8.dll",
    "ddraw.dll",
    "dsound.dll",
    "winmm.dll",
    "version.dll",
    "binkw32.dll",
    "dgvoodoo.conf",
    "dxwrapper.ini",
];

/// Severity / stream classification for a [`DebugLogLine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugLogLevel {
    /// Informational diagnostic line.
    Info,
    /// Successful milestone or clean exit.
    Success,
    /// Non-fatal warning or actionable diagnostic hint.
    Warning,
    /// Launch error, non-zero crash exit, or Event Viewer error record.
    Error,
    /// Captured child `stdout` line.
    Stdout,
    /// Captured child `stderr` line.
    Stderr,
}

/// Single timestamped entry inside [`DebugConsoleState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugLogLine {
    /// Local wall-clock timestamp (`HH:MM:SS.mmm`).
    pub timestamp: String,
    /// Short subsystem tag (e.g., `LAUNCH`, `PE`, `MODS`, `EXIT`, `EVENTLOG`, `HINT`).
    pub tag: String,
    /// Human-readable message payload.
    pub text: String,
    /// Display severity / stream type.
    pub level: DebugLogLevel,
}

impl DebugLogLine {
    /// Creates a new log entry stamped with the current local time.
    pub fn now(tag: impl Into<String>, text: impl Into<String>, level: DebugLogLevel) -> Self {
        Self {
            timestamp: chrono::Local::now().format("%H:%M:%S%.3f").to_string(),
            tag: tag.into(),
            text: text.into(),
            level,
        }
    }

    /// Formats this log entry as plain single-line text for clipboard export.
    pub fn format_plain(&self) -> String {
        format!("[{}] [{:<8}] {}", self.timestamp, self.tag, self.text)
    }
}

/// Stateful ring-buffer backing the docked bottom Debug Console drawer.
#[derive(Debug, Clone)]
pub struct DebugConsoleState {
    /// Whether the bottom Debug Console drawer is currently visible.
    pub is_open: bool,
    /// Whether the log viewport automatically sticks to the newest lines.
    pub auto_scroll: bool,
    /// Title of the game currently being monitored, if any.
    pub active_run_label: Option<String>,
    /// Number of error-level lines received while the console drawer was closed.
    pub unread_errors: usize,
    lines: VecDeque<DebugLogLine>,
}

impl Default for DebugConsoleState {
    fn default() -> Self {
        Self::new()
    }
}

impl DebugConsoleState {
    /// Creates an empty, initially closed console buffer with auto-scroll enabled.
    pub fn new() -> Self {
        Self {
            is_open: false,
            auto_scroll: true,
            active_run_label: None,
            unread_errors: 0,
            lines: VecDeque::with_capacity(256),
        }
    }

    /// Appends a line to the ring buffer, evicting the oldest entries past [`MAX_CONSOLE_LINES`].
    pub fn push(&mut self, line: DebugLogLine) {
        if !self.is_open && matches!(line.level, DebugLogLevel::Error) {
            self.unread_errors = self.unread_errors.saturating_add(1);
        }
        while self.lines.len() >= MAX_CONSOLE_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// Clears all buffered log lines and resets the unread-error counter.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.unread_errors = 0;
    }

    /// Marks any unread error notifications as seen by the user.
    pub fn mark_seen(&mut self) {
        self.unread_errors = 0;
    }

    /// Returns a read-only view of buffered log entries.
    pub fn lines(&self) -> &VecDeque<DebugLogLine> {
        &self.lines
    }

    /// Exports all buffered log lines joined with newlines for clipboard copy.
    pub fn export_text(&self) -> String {
        let mut out = String::new();
        for (idx, line) in self.lines.iter().enumerate() {
            if idx > 0 {
                out.push('\n');
            }
            out.push_str(&line.format_plain());
        }
        out
    }
}

/// Lightweight summary parsed safely from a Windows PE executable header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeSummary {
    /// Total executable size on disk in bytes.
    pub file_size_bytes: u64,
    /// Target CPU architecture label (e.g., `"x86 (32-bit)"`).
    pub architecture: &'static str,
    /// Raw COFF `TimeDateStamp` field (seconds since Unix epoch).
    pub time_date_stamp: u32,
    /// Optional-header `AddressOfEntryPoint` RVA.
    pub entry_point_rva: u32,
    /// Section names extracted from the PE section table.
    pub sections: Vec<String>,
    /// Legacy DRM / packer signatures detected purely from section names (e.g., `"SafeDisc"`).
    pub drm_markers: Vec<String>,
}

/// Wrapper DLLs and ASI plugins discovered alongside a launched executable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DetectedGameMods {
    /// Known wrapper/proxy DLLs or wrapper configs present in the executable directory.
    pub proxy_dlls: Vec<String>,
    /// `.asi` plugins found inside `<exe_dir>/scripts`.
    pub asi_plugins: Vec<String>,
}

/// Parsed crash record from `wevtutil` Windows Application Event Log (`EventID` `1000` or `1005`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashEventSummary {
    /// Windows Event ID (`1000` = Application Error, `1005` = File/Media Access Crash).
    pub event_id: u32,
    /// Raw timestamp string reported by `wevtutil`.
    pub timestamp_raw: String,
    /// Faulting application binary name (without trailing version details).
    pub faulting_app: Option<String>,
    /// Faulting DLL or executable module name reported in Event ID `1000`.
    pub faulting_module: Option<String>,
    /// Raw hexadecimal NTSTATUS exception code (e.g., `"0xc0000096"`).
    pub exception_code: Option<String>,
    /// Relative fault offset within the faulting module (e.g., `"0x0049b4a7"`).
    pub fault_offset: Option<String>,
    /// Hexadecimal Windows process ID string (e.g., `"0x4FB4"`).
    pub process_id_hex: Option<String>,
    /// Full path to the faulting executable, when present.
    pub faulting_app_path: Option<String>,
    /// Additional error code from Event ID `1005` (`Error value: ...`), when present.
    pub error_value: Option<String>,
}

/// Inspects up to the first 64 KiB of `path` to summarize its PE header and DRM section markers.
pub fn inspect_pe_summary(path: &Path) -> Result<PeSummary, String> {
    let file =
        File::open(path).map_err(|err| format!("failed opening `{}`: {err}", path.display()))?;
    let file_size_bytes = file.metadata().map_or(0, |m| m.len());
    let mut buf = Vec::with_capacity(4096);
    file.take(65_536)
        .read_to_end(&mut buf)
        .map_err(|err| format!("failed reading PE header: {err}"))?;
    parse_pe_header_bytes(&buf, file_size_bytes)
}

/// Parses COFF/Optional PE metadata and section names from a pure byte slice without `unsafe`.
pub fn parse_pe_header_bytes(bytes: &[u8], file_size_bytes: u64) -> Result<PeSummary, String> {
    if bytes.len() < 0x40 || &bytes[0..2] != b"MZ" {
        return Err("missing DOS `MZ` header".to_owned());
    }
    let e_lfanew = read_u32_le(bytes, 0x3C)?;
    let pe_off = usize::try_from(e_lfanew).map_err(|_| "invalid e_lfanew offset".to_owned())?;
    let coff_end = pe_off.checked_add(24).ok_or_else(|| "PE header offset overflow".to_owned())?;
    if coff_end > bytes.len() || &bytes[pe_off..pe_off + 4] != b"PE\0\0" {
        return Err("missing `PE\\0\\0` signature".to_owned());
    }

    let machine = read_u16_le(bytes, pe_off + 4)?;
    let num_sections = read_u16_le(bytes, pe_off + 6)?;
    let time_date_stamp = read_u32_le(bytes, pe_off + 8)?;
    let opt_size = usize::from(read_u16_le(bytes, pe_off + 20)?);

    let architecture = match machine {
        0x014C => "x86 (32-bit)",
        0x8664 => "x64 (64-bit)",
        0xAA64 => "ARM64",
        _ => "Unknown",
    };

    let opt_off = pe_off + 24;
    let entry_point_rva = if opt_size >= 20 && opt_off + 20 <= bytes.len() {
        read_u32_le(bytes, opt_off + 16)?
    } else {
        0
    };

    let sec_table =
        opt_off.checked_add(opt_size).ok_or_else(|| "section table overflow".to_owned())?;
    let mut sections = Vec::new();
    let mut drm_markers = Vec::new();

    for idx in 0..usize::from(num_sections).min(96) {
        let Some(sec_off) = sec_table.checked_add(idx * 40) else {
            break;
        };
        if sec_off + 40 > bytes.len() {
            break;
        }
        let raw_name = &bytes[sec_off..sec_off + 8];
        let end = raw_name.iter().position(|&b| b == 0).unwrap_or(raw_name.len());
        let name = String::from_utf8_lossy(&raw_name[..end]).into_owned();
        if let Some(marker) = detect_drm_marker(&name) {
            if !drm_markers.iter().any(|existing| existing == marker) {
                drm_markers.push(marker.to_owned());
            }
        }
        sections.push(name);
    }

    Ok(PeSummary {
        file_size_bytes,
        architecture,
        time_date_stamp,
        entry_point_rva,
        sections,
        drm_markers,
    })
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let end = offset.checked_add(2).ok_or_else(|| "u16 offset overflow".to_owned())?;
    let slice = bytes.get(offset..end).ok_or_else(|| "truncated PE header (u16)".to_owned())?;
    let arr: [u8; 2] = slice.try_into().map_err(|_| "invalid u16 slice".to_owned())?;
    Ok(u16::from_le_bytes(arr))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let end = offset.checked_add(4).ok_or_else(|| "u32 offset overflow".to_owned())?;
    let slice = bytes.get(offset..end).ok_or_else(|| "truncated PE header (u32)".to_owned())?;
    let arr: [u8; 4] = slice.try_into().map_err(|_| "invalid u32 slice".to_owned())?;
    Ok(u32::from_le_bytes(arr))
}

fn detect_drm_marker(section_name: &str) -> Option<&'static str> {
    let lower = section_name.to_ascii_lowercase();
    if lower.starts_with("stxt") {
        Some("SafeDisc")
    } else if lower == ".securom" || lower.starts_with("cms_t") || lower.starts_with("cms_d") {
        Some("SecuROM")
    } else if lower == ".sforce" {
        Some("StarForce")
    } else {
        None
    }
}

/// Scans `exe_path.parent()` for known wrapper/proxy DLLs and `<exe_dir>/scripts/*.asi` plugins.
pub fn inspect_game_directory_mods(exe_path: &Path) -> DetectedGameMods {
    let mut result = DetectedGameMods::default();
    let Some(exe_dir) = exe_path.parent() else {
        return result;
    };

    if let Ok(entries) = fs::read_dir(exe_dir) {
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|ft| ft.is_file()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let lower = name.to_ascii_lowercase();
            if KNOWN_PROXY_FILES.contains(&lower.as_str()) {
                result.proxy_dlls.push(name);
            }
        }
    }
    result.proxy_dlls.sort_by_key(|s| s.to_ascii_lowercase());

    let scripts_dir = exe_dir.join("scripts");
    if let Ok(entries) = fs::read_dir(scripts_dir) {
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|ft| ft.is_file()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.to_ascii_lowercase().ends_with(".asi") {
                result.asi_plugins.push(name);
            }
        }
    }
    result.asi_plugins.sort_by_key(|s| s.to_ascii_lowercase());

    result
}

/// Formats a signed process exit code in Windows canonical unsigned `0xXXXXXXXX` hex form.
pub fn format_exit_code_hex(code: i32) -> String {
    let raw = u32::from_ne_bytes(code.to_ne_bytes());
    format!("0x{raw:08X}")
}

/// Maps common Windows `NTSTATUS` exit codes to human-readable diagnostics.
pub const fn describe_exit_code(code: i32) -> Option<&'static str> {
    let raw = u32::from_ne_bytes(code.to_ne_bytes());
    match raw {
        0x0000_0000 => Some("Clean exit (OK)"),
        0xC000_0005 => Some("STATUS_ACCESS_VIOLATION (Invalid memory access or null pointer dereference)"),
        0xC000_0096 => Some("STATUS_PRIVILEGED_INSTRUCTION (Executed invalid/encrypted opcode — common with SafeDisc-packed EXEs or mismatched hooks)"),
        0xC000_0135 => Some("STATUS_DLL_NOT_FOUND (Required DLL missing from game folder or Windows runtime)"),
        0xC000_007B => Some("STATUS_INVALID_IMAGE_FORMAT (32-bit vs 64-bit DLL mismatch or corrupted binary)"),
        0xC000_001D => Some("STATUS_ILLEGAL_INSTRUCTION (Unsupported CPU instruction or invalid jump target)"),
        0xC000_0094 => Some("STATUS_INTEGER_DIVIDE_BY_ZERO"),
        0xC000_00FD => Some("STATUS_STACK_OVERFLOW"),
        0xC000_0142 => Some("STATUS_DLL_INIT_FAILED (Wrapper or proxy DLL failed during initialization)"),
        0xC000_0409 => Some("STATUS_STACK_BUFFER_OVERRUN (Security fast-fail check triggered)"),
        0x8000_0003 => Some("STATUS_BREAKPOINT (Debug trap / assertion failure)"),
        _ => None,
    }
}

#[derive(Default)]
struct RawEventBuilder {
    event_id: Option<u32>,
    timestamp_raw: String,
    faulting_app: Option<String>,
    faulting_module: Option<String>,
    exception_code: Option<String>,
    fault_offset: Option<String>,
    process_id_hex: Option<String>,
    faulting_app_path: Option<String>,
    error_value: Option<String>,
}

impl RawEventBuilder {
    fn finalize(self, exe_name: &str, pid: Option<u32>) -> Option<CrashEventSummary> {
        let event_id = self.event_id?;
        if event_id != 1000 && event_id != 1005 {
            return None;
        }
        let app_matches =
            self.faulting_app.as_deref().is_some_and(|app| app.eq_ignore_ascii_case(exe_name))
                || self.faulting_app_path.as_deref().is_some_and(|path| {
                    path.rsplit(['\\', '/'])
                        .next()
                        .is_some_and(|file| file.eq_ignore_ascii_case(exe_name))
                });
        if !app_matches {
            return None;
        }
        if let (Some(expected_pid), Some(hex_str)) = (pid, self.process_id_hex.as_deref()) {
            let trimmed = hex_str.trim().trim_start_matches("0x").trim_start_matches("0X");
            if let Ok(parsed_pid) = u32::from_str_radix(trimmed, 16) {
                if parsed_pid != expected_pid {
                    return None;
                }
            }
        }
        Some(CrashEventSummary {
            event_id,
            timestamp_raw: self.timestamp_raw,
            faulting_app: self.faulting_app,
            faulting_module: self.faulting_module,
            exception_code: self.exception_code,
            fault_offset: self.fault_offset,
            process_id_hex: self.process_id_hex,
            faulting_app_path: self.faulting_app_path,
            error_value: self.error_value,
        })
    }
}

/// Parses `wevtutil qe Application ... /f:text` output for Event ID `1000` / `1005` crashes of `exe_name`.
pub fn parse_wevtutil_crash_events(
    text: &str,
    exe_name: &str,
    pid: Option<u32>,
) -> Vec<CrashEventSummary> {
    let mut out = Vec::new();
    let mut current: Option<RawEventBuilder> = None;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.starts_with("Event[") {
            if let Some(prev) = current.take() {
                if let Some(ev) = prev.finalize(exe_name, pid) {
                    out.push(ev);
                }
            }
            current = Some(RawEventBuilder::default());
            continue;
        }
        let Some(builder) = current.as_mut() else {
            continue;
        };
        apply_wevtutil_line(builder, line);
    }

    if let Some(last) = current.take() {
        if let Some(ev) = last.finalize(exe_name, pid) {
            out.push(ev);
        }
    }
    out
}

fn apply_wevtutil_line(builder: &mut RawEventBuilder, line: &str) {
    if let Some(val) = line.strip_prefix("Date:") {
        val.trim().clone_into(&mut builder.timestamp_raw);
    } else if let Some(val) = line.strip_prefix("Event ID:") {
        builder.event_id = val.trim().parse::<u32>().ok();
    } else if let Some(val) = line.strip_prefix("Faulting application name:") {
        let name = val.split(',').next().unwrap_or("").trim();
        if !name.is_empty() {
            builder.faulting_app = Some(name.to_owned());
        }
    } else if let Some(val) = line.strip_prefix("Program:") {
        if builder.faulting_app.is_none() {
            let name = val.trim();
            if !name.is_empty() {
                builder.faulting_app = Some(name.to_owned());
            }
        }
    } else if let Some(val) = line.strip_prefix("Faulting module name:") {
        let name = val.split(',').next().unwrap_or("").trim();
        if !name.is_empty() {
            builder.faulting_module = Some(name.to_owned());
        }
    } else if let Some(val) = line.strip_prefix("Exception code:") {
        builder.exception_code = Some(val.trim().to_owned());
    } else if let Some(val) = line.strip_prefix("Fault offset:") {
        builder.fault_offset = Some(val.trim().to_owned());
    } else if let Some(val) = line.strip_prefix("Faulting process id:") {
        builder.process_id_hex = Some(val.trim().to_owned());
    } else if let Some(val) = line.strip_prefix("Faulting application path:") {
        let val = val.trim();
        if !val.is_empty() {
            builder.faulting_app_path = Some(val.to_owned());
        }
    } else if let Some(val) = line.strip_prefix("Error value:") {
        let val = val.trim();
        if !val.is_empty() {
            builder.error_value = Some(val.to_owned());
        }
    }
}

/// Queries recent Windows Application Event Log entries (`EventID` `1000` / `1005`) for `exe_name`.
pub fn query_recent_windows_crash_events(
    exe_name: &str,
    pid: Option<u32>,
    lookback_ms: u64,
) -> Result<Vec<CrashEventSummary>, String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let xpath = format!(
            "/q:*[System[(EventID=1000 or EventID=1005) and TimeCreated[timediff(@SystemTime) <= {lookback_ms}]]]"
        );
        let output = std::process::Command::new("wevtutil")
            .args(["qe", "Application", &xpath, "/c:8", "/rd:true", "/f:text"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|err| format!("failed running wevtutil: {err}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("wevtutil exited with {}: {}", output.status, stderr.trim()));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(parse_wevtutil_crash_events(&text, exe_name, pid))
    }

    #[cfg(not(windows))]
    {
        let _ = (exe_name, pid, lookback_ms);
        Ok(Vec::new())
    }
}

/// Generates concise, actionable diagnostic hints from the exit code, PE markers, Event Log, and mods.
pub fn diagnose_crash_hints(
    exit_code: Option<i32>,
    pe: Option<&PeSummary>,
    events: &[CrashEventSummary],
    mods: &DetectedGameMods,
) -> Vec<String> {
    let mut hints = Vec::new();
    let has_c0000096 = exit_code_matches(exit_code, 0xC000_0096)
        || events.iter().any(|e| {
            e.exception_code.as_deref().is_some_and(|c| c.eq_ignore_ascii_case("0xc0000096"))
        });
    let is_safedisc = pe.is_some_and(|p| p.drm_markers.iter().any(|m| m == "SafeDisc"));

    if has_c0000096 && is_safedisc {
        let mod_note = if !mods.asi_plugins.is_empty() || !mods.proxy_dlls.is_empty() {
            format!(
                " Detected hooks/proxies (proxies: [{}], scripts/*.asi: [{}]).",
                mods.proxy_dlls.join(", "),
                mods.asi_plugins.join(", ")
            )
        } else {
            String::new()
        };
        hints.push(format!(
            "SafeDisc-packed EXE (stxt* sections) crashed with 0xC0000096 (STATUS_PRIVILEGED_INSTRUCTION). Windows 10/11 blocks secdrv.sys, and hooking encrypted .text addresses frequently traps on invalid opcodes.{mod_note} Try 'Restore Original (Rollback)' to test vanilla behavior, or verify with an official DRM-free build."
        ));
    } else if has_c0000096 {
        hints.push(
            "0xC0000096 (STATUS_PRIVILEGED_INSTRUCTION) usually means an invalid hook/patch target or packed DRM stub executed an invalid opcode.".to_owned(),
        );
    }

    if events.iter().any(|e| e.event_id == 1005) {
        hints.push(
            "Windows Event 1005 logged alongside crash: OS terminated the process on a media/file access check (commonly triggered by CD/DVD DRM checks when no disc/drive matches).".to_owned(),
        );
    }

    for ev in events {
        if ev.event_id != 1000 {
            continue;
        }
        if let Some(module) = ev.faulting_module.as_deref() {
            let mod_name = module.rsplit(['\\', '/']).next().unwrap_or(module).to_ascii_lowercase();
            if matches!(
                mod_name.as_str(),
                "d3d9.dll" | "ddraw.dll" | "d3d8.dll" | "dinput8.dll" | "dsound.dll"
            ) {
                hints.push(format!(
                    "Faulting module `{module}` is a local wrapper/proxy DLL. Try disabling or rolling back the wrapper/controller fix group for this module."
                ));
                break;
            }
        }
    }

    let has_dll_error = exit_code_matches(exit_code, 0xC000_0135)
        || exit_code_matches(exit_code, 0xC000_007B)
        || events.iter().any(|e| {
            e.exception_code.as_deref().is_some_and(|c| {
                c.eq_ignore_ascii_case("0xc0000135") || c.eq_ignore_ascii_case("0xc000007b")
            })
        });
    if has_dll_error {
        hints.push(
            "Missing or 32-bit/64-bit mismatched DLL detected (0xC0000135 / 0xC000007B). Check game folder proxy DLLs and required DirectX / Visual C++ x86 runtimes.".to_owned(),
        );
    }

    let has_av = exit_code_matches(exit_code, 0xC000_0005)
        || events.iter().any(|e| {
            e.exception_code.as_deref().is_some_and(|c| c.eq_ignore_ascii_case("0xc0000005"))
        });
    if has_av {
        hints.push(
            "0xC0000005 (Access Violation): try toggling compatibility shims, CPU affinity cap, or wrapper DLLs in 'Review / Configure Fixes'.".to_owned(),
        );
    }

    hints
}

fn exit_code_matches(exit_code: Option<i32>, target_u32: u32) -> bool {
    exit_code.is_some_and(|code| u32::from_ne_bytes(code.to_ne_bytes()) == target_u32)
}

/// Renders the docked bottom Debug Console drawer when `state.is_open` is true.
pub fn render_debug_console_panel(ctx: &egui::Context, state: &mut DebugConsoleState) {
    if !state.is_open {
        return;
    }
    state.mark_seen();

    egui::TopBottomPanel::bottom("debug_console_panel")
        .resizable(true)
        .default_height(225.0)
        .min_height(130.0)
        .max_height(460.0)
        .frame(
            Frame::new()
                .fill(BG_SURFACE)
                .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
                .inner_margin(Margin::symmetric(12, 8)),
        )
        .show(ctx, |ui| {
            render_console_header(ctx, ui, state);
            ui.add_space(6.0);
            render_console_body(ui, state);
        });
}

fn render_console_header(ctx: &egui::Context, ui: &mut Ui, state: &mut DebugConsoleState) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("DEBUG CONSOLE").size(13.0).strong().color(ACCENT_AMBER));
        if let Some(title) = &state.active_run_label {
            paint_pill(ui, &format!("Running: {title}"), ACCENT_CYAN);
        } else {
            paint_pill(ui, "Idle", TEXT_MUTED);
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Close").clicked() {
                state.is_open = false;
            }
            if ui.small_button("Clear").clicked() {
                state.clear();
            }
            if ui.small_button("Copy All").clicked() {
                ctx.copy_text(state.export_text());
            }
            ui.checkbox(&mut state.auto_scroll, "Auto-scroll");
        });
    });
}

fn render_console_body(ui: &mut Ui, state: &DebugConsoleState) {
    Frame::new()
        .fill(BG_CONSOLE)
        .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(8))
        .show(ui, |ui| {
            ScrollArea::vertical()
                .id_salt("debug_console_scroll")
                .auto_shrink([false, false])
                .stick_to_bottom(state.auto_scroll)
                .show(ui, |ui| {
                    if state.lines().is_empty() {
                        ui.label(
                            RichText::new(
                                "No launch diagnostics yet. Launch any game to view PE headers, proxy DLLs/ASI plugins, live stdout/stderr, and Event Viewer crash reports.",
                            )
                            .monospace()
                            .size(11.5)
                            .color(TEXT_MUTED),
                        );
                        return;
                    }
                    for line in state.lines() {
                        let color = level_color(line.level);
                        ui.label(
                            RichText::new(line.format_plain())
                                .monospace()
                                .size(11.5)
                                .color(color),
                        );
                    }
                });
        });
}

const fn level_color(level: DebugLogLevel) -> egui::Color32 {
    match level {
        DebugLogLevel::Info => TEXT_PRIMARY,
        DebugLogLevel::Success => STATUS_PATCHED,
        DebugLogLevel::Warning => STATUS_NEEDS_FIXES,
        DebugLogLevel::Error => STATUS_BLOCKER,
        DebugLogLevel::Stdout => ACCENT_CYAN,
        DebugLogLevel::Stderr => ACCENT_AMBER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_synthetic_pe32(sections: &[&[u8; 8]], entry_rva: u32) -> Vec<u8> {
        let mut buf = vec![0_u8; 1024];
        buf[0..2].copy_from_slice(b"MZ");
        let pe_off: usize = 0x80;
        buf[0x3C..0x40].copy_from_slice(&(u32::try_from(pe_off).unwrap()).to_le_bytes());

        buf[pe_off..pe_off + 4].copy_from_slice(b"PE\0\0");
        buf[pe_off + 4..pe_off + 6].copy_from_slice(&0x014C_u16.to_le_bytes()); // x86
        let sec_count = u16::try_from(sections.len()).unwrap();
        buf[pe_off + 6..pe_off + 8].copy_from_slice(&sec_count.to_le_bytes());
        buf[pe_off + 8..pe_off + 12].copy_from_slice(&0x438A_E75E_u32.to_le_bytes());
        let opt_size: u16 = 0xE0;
        buf[pe_off + 20..pe_off + 22].copy_from_slice(&opt_size.to_le_bytes());

        let opt_off = pe_off + 24;
        buf[opt_off..opt_off + 2].copy_from_slice(&0x010B_u16.to_le_bytes()); // PE32 magic
        buf[opt_off + 16..opt_off + 20].copy_from_slice(&entry_rva.to_le_bytes());

        let sec_table = opt_off + usize::from(opt_size);
        for (idx, name) in sections.iter().enumerate() {
            let off = sec_table + idx * 40;
            buf[off..off + 8].copy_from_slice(*name);
        }
        buf
    }

    #[test]
    fn parses_pe_headers_and_detects_safedisc_sections() {
        let pe_bytes =
            build_synthetic_pe32(&[b".text\0\0\0", b"stxt774\0", b"stxt371\0"], 0x00AA_C06E);
        let summary = parse_pe_header_bytes(&pe_bytes, 7_435_120).expect("valid synthetic PE");

        assert_eq!(summary.architecture, "x86 (32-bit)");
        assert_eq!(summary.file_size_bytes, 7_435_120);
        assert_eq!(summary.entry_point_rva, 0x00AA_C06E);
        assert_eq!(summary.sections, vec![".text", "stxt774", "stxt371"]);
        assert_eq!(summary.drm_markers, vec!["SafeDisc"]);
    }

    #[test]
    fn decodes_common_windows_ntstatus_crash_codes() {
        assert_eq!(describe_exit_code(0), Some("Clean exit (OK)"));
        assert!(describe_exit_code(-1_073_741_819)
            .expect("0xC0000005")
            .contains("STATUS_ACCESS_VIOLATION"));
        assert!(describe_exit_code(-1_073_741_674)
            .expect("0xC0000096")
            .contains("STATUS_PRIVILEGED_INSTRUCTION"));
        assert!(describe_exit_code(-1_073_741_515)
            .expect("0xC0000135")
            .contains("STATUS_DLL_NOT_FOUND"));
        assert!(describe_exit_code(-1_073_741_701)
            .expect("0xC000007B")
            .contains("STATUS_INVALID_IMAGE_FORMAT"));
        assert_eq!(format_exit_code_hex(-1_073_741_674), "0xC0000096");
    }

    #[test]
    fn parses_wevtutil_application_error_1000_and_1005_events() {
        let sample_output = r"
Event[0]
  Log Name: Application
  Source: Application Error
  Date: 2026-11-22T14:28:40.4880000Z
  Event ID: 1005
  Description:
Windows closed the program speed.exe because of this error.
Program: speed.exe
Additional Data
Error value: 0x0

Event[1]
  Log Name: Application
  Source: Application Error
  Date: 2026-11-22T14:28:40.4880000Z
  Event ID: 1000
  Description:
Faulting application name: speed.exe, version: 0.0.0.0, time stamp: 0x438ae75e
Faulting module name: speed.exe, version: 0.0.0.0, time stamp: 0x438ae75e
Exception code: 0xc0000096
Fault offset: 0x0049b4a7
Faulting process id: 0x4FB4
Faulting application path: G:\Games\Need For Speed Most Wanted\speed.exe

Event[2]
  Log Name: Application
  Source: Application Error
  Date: 2026-11-22T14:20:00.0000000Z
  Event ID: 1000
  Description:
Faulting application name: other.exe, version: 1.0.0.0
Faulting module name: other.exe
Exception code: 0xc0000005
Fault offset: 0x00001234
Faulting process id: 0x1111
";

        let parsed = parse_wevtutil_crash_events(sample_output, "speed.exe", Some(0x4FB4));
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].event_id, 1005);
        assert_eq!(parsed[0].error_value.as_deref(), Some("0x0"));
        assert_eq!(parsed[1].event_id, 1000);
        assert_eq!(parsed[1].faulting_module.as_deref(), Some("speed.exe"));
        assert_eq!(parsed[1].exception_code.as_deref(), Some("0xc0000096"));
        assert_eq!(parsed[1].fault_offset.as_deref(), Some("0x0049b4a7"));

        let pe = PeSummary {
            file_size_bytes: 7_435_120,
            architecture: "x86 (32-bit)",
            time_date_stamp: 0x438A_E75E,
            entry_point_rva: 0x00AA_C06E,
            sections: vec![".text".to_owned(), "stxt774".to_owned(), "stxt371".to_owned()],
            drm_markers: vec!["SafeDisc".to_owned()],
        };
        let mods = DetectedGameMods {
            proxy_dlls: vec!["dinput8.dll".to_owned()],
            asi_plugins: vec!["NFSMostWanted.WidescreenFix.asi".to_owned()],
        };
        let hints = diagnose_crash_hints(Some(-1_073_741_674), Some(&pe), &parsed, &mods);
        assert!(hints.iter().any(|h| h.contains("SafeDisc-packed EXE")));
        assert!(hints.iter().any(|h| h.contains("Event 1005")));
    }
}
