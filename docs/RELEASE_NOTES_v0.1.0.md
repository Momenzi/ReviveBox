<div align="center">

# ReviveBox `v0.1.0` — *Old Games Second Chance*

Portable **Windows 10 / 11 (x64)** compatibility launcher & binary/wrapper repair toolkit for classic PC games.

[![Version](https://img.shields.io/badge/Release-v0.1.0-F59E0B?style=for-the-badge&logo=github)](https://github.com/Momenzi/ReviveBox/releases/tag/v0.1.0)
[![Platform](https://img.shields.io/badge/Platform-Windows%2010%2F11%20x64-0078D4?style=for-the-badge&logo=windows)](https://github.com/Momenzi/ReviveBox/releases/tag/v0.1.0)
[![Rust](https://img.shields.io/badge/Built%20with-Pure%20Rust-CE422B?style=for-the-badge&logo=rust)](https://www.rust-lang.org/)
[![Policy](https://img.shields.io/badge/Policy-100%25%20BYOG-10B981?style=for-the-badge)](https://github.com/Momenzi/ReviveBox)
[![License](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-6366F1?style=for-the-badge)](https://github.com/Momenzi/ReviveBox)

`windows-x64` · `pure-rust` · `egui` · `game-preservation` · `legacy-pc-games` · `byog` · `dgvoodoo2` · `dxwrapper` · `bsdiff` · `transactional-rollback`

</div>

---

## 📦 What's Included in the Windows x64 Portable Release

Extract `ReviveBox-v0.1.0-windows-x64.zip` into any writable folder (keeping the directory layout intact):

| File / Folder | Purpose |
| :--- | :--- |
| **`ReviveBox.exe`** | Portable Windows x64 desktop launcher GUI (`ogsc-launcher`, built with `egui` / `eframe`) with library scanner, fix advisor, cover-art cache, and built-in Debug Console. |
| **`ogsc.exe`** | Companion CLI (`ogsc-cli`) for recipe validation, library management, dry-run/apply patching, `.ogsc_backup` rollback, and launching games from scripts or terminals. |
| **`recipes/games/`** | Bundled JSON compatibility recipes and verified `.bsdiff` binary patches auto-discovered next to `ReviveBox.exe`. |
| **`README.md`** | Quick-start documentation, supported games overview, and project roadmap. |

---

## ✨ Key Highlights in `v0.1.0`

- **Desktop Launcher & Library Scanner (`ReviveBox.exe`)**
  - Custom **ReviveBox** branding with embedded icon/artwork and automatic cover-art caching.
  - Scans user-selected game folders to fingerprint installed releases (SHA-256 / partial SHA-256 / size / sentinel files) and matches them against bundled or synced recipes.
  - Gracefully skips temporarily disconnected drives or missing scan roots during **Rescan** without aborting the remaining folders.
  - Built-in remote recipe sync pointing to `https://github.com/Momenzi/ReviveBox` (`main` branch, `recipes/games`).

- **Transactional Patching & Atomic Rollback**
  - Saves content-addressed snapshots in `<game_root>/.ogsc_backup` before modifying any file or registry key.
  - Automatically rolls back partial changes if a SHA-256 verification check fails mid-apply, preventing half-patched broken installs.
  - Supports **idempotent `bsdiff` patching**, recognizing already-patched targets cleanly instead of failing on repeated runs.

- **On-Demand Wrapper & Compatibility Orchestration**
  - Automates SHA-256-verified deployment of graphics/API wrappers (**dgVoodoo 2**, **DxWrapper**, ASI loaders/fixes), INI/XML config edits, HKCU registry entries, per-fix executable overrides, and Windows CPU affinity launch rules.

- **Version & Requirements Advisor**
  - Surfaces clear edition requirements and `INCOMPATIBLE` release warnings directly in the game details pane (such as optical SafeDisc `secdrv.sys` limitations or Darkspore Retail DVD `v5.3.0.15` vs ReCap-compatible `v5.3.0.103` / `v5.3.0.127`).

- **Integrated Debug Console & Crash Diagnostics**
  - Toggleable console drawer capturing live game `stdout`/`stderr` streams during launch.
  - Runs static PE checks (architecture, imports, DRM indicators, ASI modules), decodes common Windows `NTSTATUS` exit codes, and queries Windows Event Viewer application crash records (`Event ID 1000 / 1005`) for fast troubleshooting.

---

## 🎮 Supported Games in `v0.1.0`

| Game | Year | Verified / Target Build | Included Compatibility Fixes |
| :--- | :---: | :--- | :--- |
| **Need for Speed: Most Wanted** | 2005 | Official `v1.3` (EU/PAL live-verified; US/Black Edition template) | WidescreenFix support, SafeDisc `secdrv.sys` build guidance, modern Windows display & launch fixes |
| **Darkspore** | 2011 | Steam Demo `v5.3.0.103` (verified) / `v5.3.0.127` *(DVD `v5.3.0.15` flagged incompatible with ReCap)* | Resurrection Capsule (`ReCap`) local server integration, `ConfigManager.txt` 60 FPS & modern GPU `bsdiff` patch, per-fix executable override |
| **The Barbie Diaries: High School Mystery** | 2006 | Retail PC `v1.0` (`2006-09-21` build, verified) | Binary `bsdiff` crash fixes in `bin/game.exe` (`ConversationMgr::clearAll` & `RwFreeListFree` NULL guards, Flash ActiveX guards), `EeSettings` override XML, D3D9 & shortcut directory fixes |
| **Tom Clancy's Splinter Cell: Pandora Tomorrow** | 2004 | Retail PC `v1.31` *(template)* | `dgVoodoo 2` Direct3D 8 → Direct3D 11 wrapper deployment for modern GPU shadow buffers, widescreen/FOV Unreal Engine 2 INI tweaks |

---

## 🚀 Quick Start (Windows 10 / 11 x64)

1. Download **`ReviveBox-v0.1.0-windows-x64.zip`** and **`SHA256SUMS.txt`** from the release assets below.
2. *(Optional but recommended)* Verify the download integrity in PowerShell:
   ```powershell
   Get-FileHash .\ReviveBox-v0.1.0-windows-x64.zip -Algorithm SHA256
   ```
3. Extract the `.zip` archive to a folder you have write access to (e.g., `C:\Tools\ReviveBox\`), keeping `recipes\games\` beside `ReviveBox.exe`.
4. Launch **`ReviveBox.exe`**, add your game folder(s), review recommended fixes or release notes, and click **Apply Fixes** → **Launch**.

> **Note:** On the first run of an unsigned community binary, Windows SmartScreen may show a prompt (*"More info" → "Run anyway"*). You can always verify the exact binary hashes against `SHA256SUMS.txt` or inspect the full source code and build logs in GitHub Actions.

---

## 🛡️ Integrity & 100% BYOG Policy

ReviveBox strictly follows a **Bring Your Own Game (BYOG)** model: **no copyrighted game binaries, optical images, or proprietary ROMs are ever bundled or redistributed**. Patches, wrappers, and tools are either reversible configuration transforms, binary `.bsdiff` diffs verified against known-good hashes, or dynamically downloaded from upstream maintainers with SHA-256 verification.

Licensed under **`MIT OR Apache-2.0`**.
