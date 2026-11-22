//! Compatibility wrapper deployment descriptors for bundled open-source and freeware runtimes.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Declarative configuration for deploying a compatibility runtime wrapper into a game folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WrapperDeployment {
    /// Specific compatibility toolkit and per-wrapper configuration.
    pub wrapper: WrapperKind,

    /// Optional minimum bundled toolkit version (SemVer or release tag) required by this recipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_toolkit_version: Option<String>,

    /// Game-relative directory where wrapper DLLs and INI files should be placed (`"."` for game root).
    #[serde(default = "default_target_dir")]
    pub target_dir: String,

    /// Optional DLL rename rules to avoid filename collisions (for example ASI loader + XInput Plus both wanting `dinput8.dll`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dll_chain_renames: Vec<DllChainRule>,
}

/// Rule specifying how an existing or wrapper DLL is chained/renamed when two tools share the same proxy filename.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DllChainRule {
    /// Original proxy DLL filename produced by the wrapper (for example `dinput8.dll`).
    pub original_dll: String,

    /// Target filename to deploy the wrapper DLL as (for example `xinput_dinput8.dll` or `dinput8_hook.dll`).
    pub chained_as: String,

    /// Optional explanation of how the primary loader invokes this chained DLL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Supported compatibility translation layers and 16-bit/DOS emulators in `runtime_wrappers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind")]
pub enum WrapperKind {
    /// dgVoodoo 2 (translates DirectDraw, Direct3D 1–8, and 3dfx Glide to Direct3D 11/12).
    #[serde(rename = "dgvoodoo2")]
    DgVoodoo2 {
        /// Legacy API proxy DLLs to deploy from the dgVoodoo 2 package.
        api_targets: Vec<DgVoodooApi>,
        /// Target executable architecture (`x86` for virtually all 1995–2012 Win32 titles).
        arch: TargetArch,
        /// Overrides merged into `dgVoodoo.conf` as `Section -> Key -> Value`.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        config_overrides: BTreeMap<String, BTreeMap<String, String>>,
    },

    /// DxWrapper (DirectDraw/Direct3D/DirectInput compatibility wrapper and stub DLL injector).
    #[serde(rename = "dxwrapper")]
    DxWrapper {
        /// Proxy stub DLL filenames to copy (for example `ddraw.dll`, `d3d8.dll`, `winmm.dll`).
        stub_dlls: Vec<String>,
        /// Overrides merged into `dxwrapper.ini` as `Section -> Key -> Value`.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        config_overrides: BTreeMap<String, BTreeMap<String, String>>,
    },

    /// WineD3D for Windows (translates DirectDraw and Direct3D 1–11 to OpenGL or Vulkan).
    #[serde(rename = "wined3d")]
    WineD3d {
        /// WineD3D DLL filenames to deploy (for example `ddraw.dll`, `d3d8.dll`, `wined3d.dll`).
        dlls: Vec<String>,
        /// Preferred WineD3D backend renderer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        renderer: Option<WineD3dRenderer>,
    },

    /// XInput Plus (bridges modern XInput controllers to legacy DirectInput / WinMM joystick APIs).
    #[serde(rename = "xinput_plus")]
    XInputPlus {
        /// Target DirectInput / XInput proxy DLLs generated or deployed (for example `dinput8.dll`).
        target_dlls: Vec<String>,
        /// Controller mapping sections merged into `XInputPlus.ini`.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        profile_ini: BTreeMap<String, BTreeMap<String, String>>,
    },

    /// DOSBox-X runtime profile for DOS and 16-bit Windows 3.1/95/98 titles on 64-bit Windows.
    #[serde(rename = "dosbox_x")]
    DosBoxX {
        /// Emulated DOSBox-X machine type (for example `svga_s3` or `voodoo_banshee`).
        machine: String,
        /// Guest operating system profile expected inside DOSBox-X.
        os_profile: LegacyOsProfile,
        /// Configuration sections written into the per-game `dosbox-x.conf`.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        config_overrides: BTreeMap<String, BTreeMap<String, String>>,
    },

    /// 86Box low-level PC hardware emulator profile for Win95/Win98 titles incompatible with Win64.
    #[serde(rename = "86box")]
    Box86 {
        /// Identifier of the 86Box machine hardware preset (motherboard + GPU + sound).
        machine_profile: String,
        /// Guest operating system profile expected inside 86Box.
        os_profile: LegacyOsProfile,
        /// Configuration sections written into the per-game `86box.cfg`.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        config_overrides: BTreeMap<String, BTreeMap<String, String>>,
    },
}

/// Legacy DirectX / 3dfx Glide DLL targets provided by dgVoodoo 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DgVoodooApi {
    /// DirectDraw (`DDraw.dll`).
    #[serde(rename = "ddraw")]
    DDraw,
    /// Direct3D Immediate/Retained Mode (`D3DImm.dll`).
    D3dImm,
    /// Direct3D 8 (`D3D8.dll`).
    D3d8,
    /// Direct3D 9 (`D3D9.dll`).
    D3d9,
    /// Original 3dfx Glide (`Glide.dll`).
    Glide,
    /// 3dfx Glide 2.x (`Glide2x.dll`).
    Glide2x,
    /// 3dfx Glide 3.x (`Glide3x.dll`).
    Glide3x,
}

/// Binary architecture of the target wrapper DLLs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TargetArch {
    /// 32-bit x86 (`MS/x86` in dgVoodoo 2).
    X86,
    /// 64-bit x86_64 (`MS/x64` in dgVoodoo 2).
    X64,
}

/// Graphics backend renderer selection for WineD3D.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WineD3dRenderer {
    /// Translate Direct3D calls to OpenGL.
    OpenGl,
    /// Translate Direct3D calls to Vulkan.
    Vulkan,
    /// Software GDI fallback renderer.
    Gdi,
}

/// Legacy guest OS target for DOSBox-X / 86Box emulator integration hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LegacyOsProfile {
    /// MS-DOS / FreeDOS environment.
    Dos,
    /// Windows 3.1 / 3.11 (16-bit protected mode).
    Win31,
    /// Windows 95.
    Win95,
    /// Windows 98 / 98 SE.
    Win98,
}

fn default_target_dir() -> String {
    ".".to_owned()
}
