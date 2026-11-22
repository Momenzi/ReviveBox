<img src="crates/ogsc-ui/assets/revivebox_logo.png" alt="ReviveBox Logo" width="96" />

# ReviveBox

> **Old Games Second Chance**

An open-source, ultra-fast Windows desktop launcher and declarative compatibility engine designed to run legacy PC games (1995–2012) smoothly on Windows 10 and Windows 11 with one click.

## Core Philosophy

- **Bring Your Own Game (BYOG):** Zero copyrighted game binaries, zero piracy cracks, and zero bypasses. Users point the launcher at legally owned local game installations (Retail CD/DVD, Steam, GOG, EA App, Ubisoft Connect, etc.). Executable fingerprinting identifies the exact release and applies verified open-source wrappers, community fixes, configuration tweaks, and Windows compatibility flags.
- **Zero Bloat & Native Speed:** Built in pure Rust with `egui / eframe` for `<500 ms` cold startup and minimal idle memory overhead.
- **Non-Destructive & Portable:** Originals are backed up content-addressed in `.ogsc_backup/` prior to patching, global Registry/Documents pollution is avoided (`HKCU`-scoped flags and `.ogsc_sandbox/` save redirection), and every fix group supports transactional rollback.

## Workspace Architecture

### Implemented (Milestones 1–4)

- `crates/ogsc-recipes` *(Milestone 1)* — Declarative `GameRecipe` schema with selectable `FixOption` groups, strict security/semantic validator, multi-file JSON database loader, and Draft 2020-12 JSON Schema generator.
- `crates/ogsc-scanner` *(Milestone 2)* — Multi-stage parallel directory scanner (`walkdir` + `rayon`), buffered streaming SHA-256 / partial SHA-256 / MD5 hasher, content-addressed transactional backup & rollback journal (`BackupManager`), and atomic local `library.json` persistence (`LibraryStore`).
- `crates/ogsc-wrappers` *(Milestone 3)* — Bundled compatibility toolkit catalog (`ToolkitStore`), deterministic CRLF config generators, dry-run wrapper planner, and transactional deployment for `dgVoodoo 2`, `DxWrapper`, `WineD3D`, `XInput Plus`, `DOSBox-X`, and `86Box`.
- `crates/ogsc-patcher` *(Milestone 3)* — Dry-run `plan_fixes` preview, structured `PatchEvent` progress/log stream, verified HTTPS streaming downloader (`ureq`), Zip-Slip-safe `.zip`/`.7z` extractor, verified `bsdiff` binary patcher, comment-preserving encoding-aware INI editor (`UTF-8` / `Windows-1252` / `UTF-16LE`), `HKCU` `AppCompatFlags\Layers` registry backend, CPU affinity / priority game launcher, and transactional `PatchEngine` with automatic per-fix rollback on failure.
- `crates/ogsc-ui` *(Milestone 4)* — Native `egui / eframe` desktop interface (`ogsc-launcher`) with async folder scanner, card grid + SteamGridDB cover art cache & typographic offline fallback covers, Game Details side panel, consent-driven Patch Advisor modal (expandable `plan_fixes` previews + live `PatchEvent` progress bar & log console), and GitHub remote recipe sync.
- `crates/ogsc-cli` — Developer and diagnostic CLI (`validate`, `inspect`, `export-schema`, `hash`, `scan`, `library`, `backup`, `apply`, `launch`).
- `recipes/games/` — Versioned declarative JSON compatibility recipes (`nfs_most_wanted_2005.json`, `splinter_cell_pandora_tomorrow_2004.json`).
- `recipes/schema/` — Auto-generated JSON Schema (`game_recipe.schema.json`).

## Local Verification (Windows 10/11)

```powershell
# Launch the Desktop GUI
cargo run -p ogsc-ui

# Or run an optimized release binary
cargo run --release -p ogsc-ui
```
# Export/refresh JSON Schema
cargo run -p ogsc-cli -- export-schema --output recipes/schema/game_recipe.schema.json

# Format & lint
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

# Run all unit & integration tests across all 5 crates
cargo test --workspace

# Validate and inspect recipes
cargo run -p ogsc-cli -- validate recipes/games
cargo run -p ogsc-cli -- validate --strict recipes/games
cargo run -p ogsc-cli -- inspect recipes/games/nfs_most_wanted_2005.json

# Scan, preview dry-run fixes, and preview launch command
cargo run -p ogsc-cli -- scan "C:\Games" --library library.json
cargo run -p ogsc-cli -- apply recipes/games/nfs_most_wanted_2005.json --game-root "C:\Games\NFS Most Wanted" --dry-run
cargo run -p ogsc-cli -- launch recipes/games/nfs_most_wanted_2005.json --game-root "C:\Games\NFS Most Wanted" --applied-fix win11_stability_tuning --dry-run
```
