//! Local compatibility toolkit catalog and asset path resolution under `runtime_wrappers`.

use std::path::{Path, PathBuf};

use ogsc_recipes::{DgVoodooApi, TargetArch, WrapperKind};

use crate::error::WrapperError;

/// Official SHA-256-verified HTTPS distribution archive for auto-provisioning a toolkit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolkitDownloadSpec {
    /// Target toolkit kind.
    pub kind: ToolkitKind,
    /// Upstream release tag or version string.
    pub version: &'static str,
    /// Canonical HTTPS release asset URL.
    pub url: &'static str,
    /// Verified lowercase hex SHA-256 digest of the downloaded archive.
    pub sha256: &'static str,
}

/// Supported compatibility translation layers and legacy PC emulators managed by [`ToolkitStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolkitKind {
    /// dgVoodoo 2 (DirectDraw / Direct3D 1–9 / Glide -> Direct3D 11/12).
    DgVoodoo2,
    /// DxWrapper (DirectDraw / Direct3D / DirectInput compatibility stub injector).
    DxWrapper,
    /// WineD3D for Windows (DirectDraw / Direct3D -> OpenGL / Vulkan).
    WineD3d,
    /// XInput Plus (XInput controller to DirectInput translator).
    XInputPlus,
    /// DOSBox-X (DOS and Windows 3.1/95/98 emulator).
    DosBoxX,
    /// 86Box (low-level x86 PC hardware emulator).
    Box86,
}

impl ToolkitKind {
    /// Returns all supported toolkit variants in deterministic display order.
    pub const fn all() -> &'static [Self] {
        &[
            Self::DgVoodoo2,
            Self::DxWrapper,
            Self::WineD3d,
            Self::XInputPlus,
            Self::DosBoxX,
            Self::Box86,
        ]
    }

    /// Returns the canonical subdirectory slug under `runtime_wrappers`.
    pub const fn slug(self) -> &'static str {
        match self {
            Self::DgVoodoo2 => "dgvoodoo2",
            Self::DxWrapper => "dxwrapper",
            Self::WineD3d => "wined3d",
            Self::XInputPlus => "xinput_plus",
            Self::DosBoxX => "dosbox_x",
            Self::Box86 => "86box",
        }
    }

    /// Maps a declarative [`WrapperKind`] to its owning [`ToolkitKind`].
    pub const fn from_wrapper_kind(wrapper: &WrapperKind) -> Self {
        match wrapper {
            WrapperKind::DgVoodoo2 { .. } => Self::DgVoodoo2,
            WrapperKind::DxWrapper { .. } => Self::DxWrapper,
            WrapperKind::WineD3d { .. } => Self::WineD3d,
            WrapperKind::XInputPlus { .. } => Self::XInputPlus,
            WrapperKind::DosBoxX { .. } => Self::DosBoxX,
            WrapperKind::Box86 { .. } => Self::Box86,
        }
    }

    /// Parses a `runtime_wrappers` subdirectory slug into a [`ToolkitKind`].
    pub fn from_slug(slug: &str) -> Option<Self> {
        let trimmed = slug.trim();
        if trimmed.eq_ignore_ascii_case("dgvoodoo2") {
            Some(Self::DgVoodoo2)
        } else if trimmed.eq_ignore_ascii_case("dxwrapper") {
            Some(Self::DxWrapper)
        } else if trimmed.eq_ignore_ascii_case("wined3d") {
            Some(Self::WineD3d)
        } else if trimmed.eq_ignore_ascii_case("xinput_plus") {
            Some(Self::XInputPlus)
        } else if trimmed.eq_ignore_ascii_case("dosbox_x") {
            Some(Self::DosBoxX)
        } else if trimmed.eq_ignore_ascii_case("86box") {
            Some(Self::Box86)
        } else {
            None
        }
    }

    /// Returns an official SHA-256-verified HTTPS download specification for auto-provisioning, if supported.
    pub const fn official_download(self) -> Option<ToolkitDownloadSpec> {
        match self {
            Self::DgVoodoo2 => Some(ToolkitDownloadSpec {
                kind: Self::DgVoodoo2,
                version: "2.87.6",
                url: "https://github.com/dege-diosg/dgVoodoo2/releases/download/2.87.6/dgVoodoo2_87_6.zip",
                sha256: "9afa493396feec7108f42a2d97661eef96bbaac5addf0dfb5cfbc3001cd76a0a",
            }),
            Self::DxWrapper => Some(ToolkitDownloadSpec {
                kind: Self::DxWrapper,
                version: "1.9.8700.25",
                url: "https://github.com/elishacloud/dxwrapper/releases/download/v1.9.8700.25/dxwrapper.zip",
                sha256: "e6aa4fc685115c8f73dee4ea2d446b6a1f8b57dd04db9889d2c6d1b6d7b8c825",
            }),
            Self::WineD3d | Self::XInputPlus | Self::DosBoxX | Self::Box86 => None,
        }
    }
}

/// Availability status for a single toolkit directory inside [`ToolkitStore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolkitAvailability {
    /// Toolkit identifier.
    pub kind: ToolkitKind,
    /// Expected directory inside the `runtime_wrappers` store.
    pub directory: PathBuf,
    /// Whether the toolkit directory exists on disk.
    pub installed: bool,
}

/// Locates and validates wrapper DLLs and emulator executables inside a local `runtime_wrappers` store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolkitStore {
    root_dir: PathBuf,
}

impl ToolkitStore {
    /// Opens a [`ToolkitStore`] rooted at `root_dir`.
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        Self { root_dir: root_dir.into() }
    }

    /// Returns the default `runtime_wrappers` store path (`OGSC_TOOLKITS_DIR`, `%LOCALAPPDATA%\OldGamesSecondChance\runtime_wrappers`, or `./runtime_wrappers`).
    pub fn default_dir() -> PathBuf {
        if let Some(override_dir) = std::env::var_os("OGSC_TOOLKITS_DIR") {
            if !override_dir.is_empty() {
                return PathBuf::from(override_dir);
            }
        }
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local_app_data)
                .join("OldGamesSecondChance")
                .join("runtime_wrappers");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("old-games-second-chance")
                .join("runtime_wrappers");
        }
        PathBuf::from("runtime_wrappers")
    }

    /// Returns the store's root directory path.
    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    /// Returns the directory where `kind` lives inside this store.
    pub fn toolkit_dir(&self, kind: ToolkitKind) -> PathBuf {
        self.root_dir.join(kind.slug())
    }

    /// Checks whether each supported toolkit directory exists on disk.
    pub fn availability(&self) -> Vec<ToolkitAvailability> {
        ToolkitKind::all()
            .iter()
            .map(|&kind| {
                let directory = self.toolkit_dir(kind);
                let installed = directory.is_dir();
                ToolkitAvailability { kind, directory, installed }
            })
            .collect()
    }

    /// Resolves and verifies a specific relative asset path inside a named toolkit directory.
    pub fn resolve_bundled_file(
        &self,
        toolkit_slug: &str,
        relative_path: &str,
    ) -> Result<PathBuf, WrapperError> {
        let safe_slug = normalize_safe_relative(toolkit_slug, false)?;
        let safe_rel = normalize_safe_relative(relative_path, false)?;
        let toolkit_root = self.root_dir.join(safe_slug);
        if !toolkit_root.is_dir() {
            return Err(WrapperError::ToolkitNotInstalled {
                toolkit: toolkit_slug.to_owned(),
                expected_dir: toolkit_root,
            });
        }
        let mut full = toolkit_root;
        for seg in safe_rel.split('/') {
            full.push(seg);
        }
        if !full.is_file() {
            return Err(WrapperError::MissingToolkitAsset {
                toolkit: toolkit_slug.to_owned(),
                expected_path: full,
            });
        }
        Ok(full)
    }

    /// Returns the expected source path and target DLL filename for a dgVoodoo 2 API DLL.
    pub(crate) fn dgvoodoo_asset(
        &self,
        api: DgVoodooApi,
        arch: TargetArch,
    ) -> (PathBuf, &'static str) {
        let arch_dir = match arch {
            TargetArch::X86 => "x86",
            TargetArch::X64 => "x64",
        };
        let (sub_root, dll_name) = match api {
            DgVoodooApi::DDraw => ("MS", "DDraw.dll"),
            DgVoodooApi::D3dImm => ("MS", "D3DImm.dll"),
            DgVoodooApi::D3d8 => ("MS", "D3D8.dll"),
            DgVoodooApi::D3d9 => ("MS", "D3D9.dll"),
            DgVoodooApi::Glide => ("3Dfx", "Glide.dll"),
            DgVoodooApi::Glide2x => ("3Dfx", "Glide2x.dll"),
            DgVoodooApi::Glide3x => ("3Dfx", "Glide3x.dll"),
        };
        let path =
            self.toolkit_dir(ToolkitKind::DgVoodoo2).join(sub_root).join(arch_dir).join(dll_name);
        (path, dll_name)
    }

    /// Returns the expected source path for a DxWrapper stub DLL (`Stub/<dll>` or `<dll>` fallback).
    pub(crate) fn dxwrapper_stub_asset(&self, stub_dll: &str) -> PathBuf {
        let base = self.toolkit_dir(ToolkitKind::DxWrapper);
        let in_stub_dir = base.join("Stub").join(stub_dll);
        if in_stub_dir.is_file() {
            in_stub_dir
        } else {
            let flat = base.join(stub_dll);
            if flat.is_file() {
                flat
            } else {
                in_stub_dir
            }
        }
    }

    /// Returns the expected source path for `dxwrapper.dll`.
    pub(crate) fn dxwrapper_core_dll(&self) -> PathBuf {
        self.toolkit_dir(ToolkitKind::DxWrapper).join("dxwrapper.dll")
    }

    /// Returns the expected source path for a WineD3D DLL.
    pub(crate) fn wined3d_asset(&self, dll: &str) -> PathBuf {
        self.toolkit_dir(ToolkitKind::WineD3d).join(dll)
    }

    /// Returns the expected source path for an XInput Plus DLL.
    pub(crate) fn xinput_plus_asset(&self, dll: &str) -> PathBuf {
        self.toolkit_dir(ToolkitKind::XInputPlus).join(dll)
    }

    /// Returns the expected DOSBox-X executable path inside the store.
    pub fn dosbox_x_executable(&self) -> PathBuf {
        self.toolkit_dir(ToolkitKind::DosBoxX).join("dosbox-x.exe")
    }

    /// Returns the expected 86Box executable path inside the store.
    pub fn box86_executable(&self) -> PathBuf {
        self.toolkit_dir(ToolkitKind::Box86).join("86Box.exe")
    }
}

pub(crate) fn normalize_safe_relative(raw: &str, allow_dot: bool) -> Result<String, WrapperError> {
    if raw.is_empty() {
        return Err(WrapperError::UnsafePath {
            path: raw.to_owned(),
            reason: "path must not be empty",
        });
    }
    if raw == "." {
        return if allow_dot {
            Ok(".".to_owned())
        } else {
            Err(WrapperError::UnsafePath {
                path: raw.to_owned(),
                reason: "`.` is only allowed for directory targets",
            })
        };
    }
    if raw.chars().any(|c| c == '\0' || c.is_control()) {
        return Err(WrapperError::UnsafePath {
            path: raw.to_owned(),
            reason: "path contains control or NUL characters",
        });
    }
    if raw.starts_with('/') || raw.starts_with('\\') {
        return Err(WrapperError::UnsafePath {
            path: raw.to_owned(),
            reason: "path must be relative, not rooted or UNC",
        });
    }
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(WrapperError::UnsafePath {
            path: raw.to_owned(),
            reason: "Windows drive prefixes are forbidden",
        });
    }

    let mut segments = Vec::new();
    for seg in raw.split(['/', '\\']) {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(WrapperError::UnsafePath {
                path: raw.to_owned(),
                reason: "empty, `.`, and `..` path segments are forbidden",
            });
        }
        if seg.ends_with(' ') || seg.ends_with('.') {
            return Err(WrapperError::UnsafePath {
                path: raw.to_owned(),
                reason: "segments must not end with whitespace or `.`",
            });
        }
        if seg.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*')) {
            return Err(WrapperError::UnsafePath {
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
            return Err(WrapperError::UnsafePath {
                path: raw.to_owned(),
                reason: "path contains a reserved Windows DOS device name",
            });
        }
        segments.push(seg);
    }

    Ok(segments.join("/"))
}
