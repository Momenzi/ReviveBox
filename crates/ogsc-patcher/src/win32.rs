//! Isolated Windows Win32 host helpers for display metrics, OS build lookup, CPU affinity, and priority.

use std::process::Child;

use ogsc_recipes::ProcessPriority;

use crate::error::PatcherError;
use crate::launcher::{LaunchedProcess, ResolvedLaunchCommand};

/// Queries the primary monitor's `(width, height, refresh_rate_hz)`, falling back cleanly on non-Windows hosts.
#[cfg(windows)]
pub fn query_primary_display() -> (u32, u32, u32) {
    use windows_sys::Win32::Graphics::Gdi::{
        EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

    // SAFETY: `GetSystemMetrics` reads read-only system metrics and accepts constant enum indices.
    // `DEVMODEW` is zero-initialized with `dmSize` set to its exact struct size before passing
    // a valid mutable pointer to `EnumDisplaySettingsW(NULL, ENUM_CURRENT_SETTINGS, ...)`.
    #[allow(unsafe_code)]
    unsafe {
        let mut devmode: DEVMODEW = std::mem::zeroed();
        devmode.dmSize = u16::try_from(std::mem::size_of::<DEVMODEW>()).unwrap_or(0);
        if EnumDisplaySettingsW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &raw mut devmode) != 0
            && devmode.dmPelsWidth > 0
            && devmode.dmPelsHeight > 0
        {
            let hz = if devmode.dmDisplayFrequency > 1 { devmode.dmDisplayFrequency } else { 60 };
            return (devmode.dmPelsWidth, devmode.dmPelsHeight, hz);
        }

        let w = GetSystemMetrics(SM_CXSCREEN);
        let h = GetSystemMetrics(SM_CYSCREEN);
        if w > 0 && h > 0 {
            return (u32::try_from(w).unwrap_or(1920), u32::try_from(h).unwrap_or(1080), 60);
        }
    }

    (1920, 1080, 60)
}

/// Queries the primary monitor's `(width, height, refresh_rate_hz)` (safe default fallback on non-Windows hosts).
#[cfg(not(windows))]
pub fn query_primary_display() -> (u32, u32, u32) {
    (1920, 1080, 60)
}

/// Returns the current Windows build number from `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\CurrentBuildNumber`.
#[cfg(windows)]
pub fn current_windows_build() -> Option<u32> {
    let hklm = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE);
    let key = hklm.open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion").ok()?;
    let build_str: String = key.get_value("CurrentBuildNumber").ok()?;
    build_str.trim().parse::<u32>().ok()
}

/// Returns the current Windows build number (`None` on non-Windows hosts).
#[cfg(not(windows))]
pub fn current_windows_build() -> Option<u32> {
    None
}

/// Applies Windows processor affinity mask and scheduling priority class to a spawned child process.
#[cfg(windows)]
pub fn apply_process_affinity_and_priority(
    child: &Child,
    affinity_mask: Option<u64>,
    priority: Option<ProcessPriority>,
) -> Result<(), PatcherError> {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Threading::{
        SetPriorityClass, SetProcessAffinityMask, ABOVE_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS,
        NORMAL_PRIORITY_CLASS,
    };

    let handle = child.as_raw_handle() as HANDLE;

    if let Some(mask) = affinity_mask {
        let nonzero = usize::try_from(mask).unwrap_or(1).max(1);
        // SAFETY: `handle` comes directly from a live `std::process::Child` owned by the caller
        // with `PROCESS_ALL_ACCESS` permissions from `CreateProcessW`.
        #[allow(unsafe_code)]
        let ok = unsafe { SetProcessAffinityMask(handle, nonzero) };
        if ok == 0 {
            return Err(PatcherError::LaunchFailed {
                program: std::path::PathBuf::from(format!("pid:{}", child.id())),
                reason: format!(
                    "SetProcessAffinityMask failed: {}",
                    std::io::Error::last_os_error()
                ),
            });
        }
    }

    if let Some(prio) = priority {
        let class = match prio {
            ProcessPriority::Normal => NORMAL_PRIORITY_CLASS,
            ProcessPriority::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
            ProcessPriority::High => HIGH_PRIORITY_CLASS,
        };
        // SAFETY: `handle` comes directly from a live `std::process::Child` owned by the caller.
        #[allow(unsafe_code)]
        let ok = unsafe { SetPriorityClass(handle, class) };
        if ok == 0 {
            return Err(PatcherError::LaunchFailed {
                program: std::path::PathBuf::from(format!("pid:{}", child.id())),
                reason: format!("SetPriorityClass failed: {}", std::io::Error::last_os_error()),
            });
        }
    }

    Ok(())
}

/// No-op process affinity/priority stub for non-Windows compilation targets.
#[cfg(not(windows))]
pub fn apply_process_affinity_and_priority(
    _child: &Child,
    _affinity_mask: Option<u64>,
    _priority: Option<ProcessPriority>,
) -> Result<(), PatcherError> {
    Ok(())
}

/// Launches `cmd` through Windows `ShellExecuteExW` (`"runas"`) when `CreateProcessW` returns
/// `ERROR_ELEVATION_REQUIRED` (OS error 740).
#[cfg(windows)]
pub fn launch_elevated_via_shell(
    cmd: &ResolvedLaunchCommand,
) -> Result<LaunchedProcess, PatcherError> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::GetProcessId;
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn to_wide_null(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    let verb = to_wide_null(OsStr::new("runas"));
    let file = to_wide_null(cmd.program.as_os_str());
    let params_str = crate::launcher::format_windows_args(&cmd.args);
    let params = (!cmd.args.is_empty()).then(|| to_wide_null(OsStr::new(&params_str)));
    let dir = to_wide_null(cmd.working_dir.as_os_str());

    // SAFETY: `SHELLEXECUTEINFOW` is zero-initialized with `cbSize` matching its exact byte size.
    // All non-null UTF-16 string buffers (`verb`, `file`, optional `params`, `dir`) are NUL-terminated
    // and outlive the synchronous `ShellExecuteExW` call. Returned `hProcess` (when non-null) is
    // inspected strictly for its PID and closed immediately afterward.
    #[allow(unsafe_code)]
    let (ok, pid) = unsafe {
        let mut sei: SHELLEXECUTEINFOW = std::mem::zeroed();
        sei.cbSize = u32::try_from(std::mem::size_of::<SHELLEXECUTEINFOW>()).unwrap_or(0);
        sei.fMask = SEE_MASK_NOCLOSEPROCESS;
        sei.lpVerb = verb.as_ptr();
        sei.lpFile = file.as_ptr();
        sei.lpParameters = params.as_ref().map_or(std::ptr::null(), Vec::as_ptr);
        sei.lpDirectory = dir.as_ptr();
        sei.nShow = SW_SHOWNORMAL;

        let ok = ShellExecuteExW(&raw mut sei);
        if ok == 0 {
            (false, None)
        } else {
            let pid = if sei.hProcess.is_null() {
                None
            } else {
                let raw_pid = GetProcessId(sei.hProcess);
                CloseHandle(sei.hProcess);
                (raw_pid != 0).then_some(raw_pid)
            };
            (true, pid)
        }
    };

    if !ok {
        let err = std::io::Error::last_os_error();
        let reason = if err.raw_os_error() == Some(1223) {
            "UAC elevation prompt was cancelled by the user".to_owned()
        } else {
            err.to_string()
        };
        return Err(PatcherError::LaunchFailed { program: cmd.program.clone(), reason });
    }

    let note = if cmd.cpu_affinity_mask.is_some()
        || cmd.process_priority.is_some()
        || !cmd.env.is_empty()
    {
        Some(
            "Launched elevated via Windows UAC; process affinity/custom env could not be injected from a standard user process."
                .to_owned(),
        )
    } else {
        None
    };

    Ok(LaunchedProcess { pid, elevated: true, note, child: None })
}

/// Non-Windows stub for elevated shell launch (elevation fallback is Windows-only).
#[cfg(not(windows))]
pub fn launch_elevated_via_shell(
    cmd: &ResolvedLaunchCommand,
) -> Result<LaunchedProcess, PatcherError> {
    Err(PatcherError::LaunchFailed {
        program: cmd.program.clone(),
        reason: "elevated ShellExecuteExW launch fallback is only supported on Windows".to_owned(),
    })
}
