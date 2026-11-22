//! Root `eframe::App` state machine, library grid, top toolbar, and background worker wiring.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

use egui::{
    ColorImage, CornerRadius, Frame, Margin, RichText, ScrollArea, Stroke, TextureHandle, Ui, Vec2,
};
use ogsc_patcher::{launch_game_captured, resolve_launch_command, LaunchOptions};
use ogsc_recipes::RecipeDatabase;
use ogsc_scanner::{GameStatus, LibraryEntry, LibraryStore};
use ogsc_wrappers::ToolkitStore;

use crate::advisor::{render_patch_advisor_modal, AdvisorAction, PatchAdvisorState};
use crate::covers::{CoverLoadOutcome, CoverRequest};
use crate::debug_console::{
    format_exit_code_hex, render_debug_console_panel, DebugConsoleState, DebugLogLevel,
    DebugLogLine,
};
use crate::details::{render_details_panel, DetailsAction};
use crate::recipe_sync::SyncOutcome;
use crate::settings::{
    discover_bundled_recipe_dirs, sync_missing_bundled_recipes_and_patches, AppSettings,
};
use crate::settings_modal::{render_settings_modal, SettingsAction};
use crate::theme::{
    apply_retro_dark_theme, paint_fallback_cover, paint_pill, primary_button, status_badge_style,
    ACCENT_AMBER, ACCENT_CYAN, BG_ELEVATED, BG_SURFACE, BORDER_ACTIVE, BORDER_SUBTLE,
    STATUS_BLOCKER, STATUS_PATCHED, TEXT_MUTED,
};
use crate::worker::{
    build_preview_context, host_registry_backend, spawn_apply_fixes, spawn_load_cover,
    spawn_monitor_launched_game, spawn_rollback, spawn_scan_folders, spawn_sync_recipes,
    ApplyFixesJob, MonitorGameJob, WorkerMessage,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusFilter {
    All,
    NeedsFixes,
    Patched,
    Vanilla,
}

impl StatusFilter {
    const ALL: [Self; 4] = [Self::All, Self::NeedsFixes, Self::Patched, Self::Vanilla];

    const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::NeedsFixes => "Needs Fixes",
            Self::Patched => "Patched & Ready",
            Self::Vanilla => "Vanilla / Declined",
        }
    }

    const fn matches(self, status: GameStatus) -> bool {
        match self {
            Self::All => true,
            Self::NeedsFixes => matches!(status, GameStatus::NeedsFixes),
            Self::Patched => matches!(status, GameStatus::Patched),
            Self::Vanilla => matches!(status, GameStatus::DeclinedVanilla),
        }
    }
}

enum CoverSlot {
    Loading,
    Ready(TextureHandle),
    Fallback,
}

struct StatusBanner {
    message: String,
    is_error: bool,
}

/// Top-level `eframe` application owning library state, settings, cover cache, and worker channels.
pub struct OgscApp {
    settings_path: PathBuf,
    settings: AppSettings,
    recipe_db: RecipeDatabase,
    library_store: LibraryStore,
    selected_entry_id: Option<String>,
    search_query: String,
    status_filter: StatusFilter,
    cover_slots: HashMap<String, CoverSlot>,
    advisor: Option<PatchAdvisorState>,
    pending_advisor_queue: VecDeque<String>,
    settings_modal_draft: Option<AppSettings>,
    show_api_key: bool,
    worker_tx: Sender<WorkerMessage>,
    worker_rx: Receiver<WorkerMessage>,
    is_scanning: bool,
    is_syncing: bool,
    is_rolling_back: bool,
    banner: Option<StatusBanner>,
    debug_console: DebugConsoleState,
    logo_texture: Option<TextureHandle>,
}

impl OgscApp {
    /// Initializes desktop UI state rapidly using local disk caches with zero blocking network I/O.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_retro_dark_theme(&cc.egui_ctx);

        let settings_path = AppSettings::default_path();
        let settings = AppSettings::load_or_default(&settings_path);
        let bundled_dirs = discover_bundled_recipe_dirs();
        let _ = sync_missing_bundled_recipes_and_patches(&settings.recipes_dir, &bundled_dirs);

        let mut banner = None;
        let recipe_db = load_initial_recipes(&settings.recipes_dir, &bundled_dirs);
        let library_store = match LibraryStore::open(&settings.library_path) {
            Ok(store) => store,
            Err(err) => {
                banner = Some(StatusBanner {
                    message: format!("Could not load library.json cleanly: {err}"),
                    is_error: true,
                });
                LibraryStore::open(settings.library_path.with_extension("recovery.json"))
                    .expect("fresh library path should initialize cleanly")
            }
        };

        let logo_texture = crate::decode_app_logo_rgba().and_then(|(rgba, width, height)| {
            let w = usize::try_from(width).ok()?;
            let h = usize::try_from(height).ok()?;
            let img = ColorImage::from_rgba_unmultiplied([w, h], &rgba);
            Some(cc.egui_ctx.load_texture("revivebox_logo", img, egui::TextureOptions::LINEAR))
        });

        let (worker_tx, worker_rx) = mpsc::channel();
        let mut app = Self {
            settings_path,
            settings,
            recipe_db,
            library_store,
            selected_entry_id: None,
            search_query: String::new(),
            status_filter: StatusFilter::All,
            cover_slots: HashMap::new(),
            advisor: None,
            pending_advisor_queue: VecDeque::new(),
            settings_modal_draft: None,
            show_api_key: false,
            worker_tx,
            worker_rx,
            is_scanning: false,
            is_syncing: false,
            is_rolling_back: false,
            banner,
            debug_console: DebugConsoleState::new(),
            logo_texture,
        };
        app.queue_missing_covers(&cc.egui_ctx);
        app
    }

    fn is_busy(&self) -> bool {
        self.is_scanning
            || self.is_syncing
            || self.is_rolling_back
            || self
                .advisor
                .as_ref()
                .is_some_and(|a| matches!(a.phase, crate::advisor::AdvisorPhase::Applying))
    }

    fn set_banner(&mut self, message: impl Into<String>, is_error: bool) {
        self.banner = Some(StatusBanner { message: message.into(), is_error });
    }

    fn queue_missing_covers(&mut self, ctx: &egui::Context) {
        let entries: Vec<LibraryEntry> = self.library_store.entries().cloned().collect();
        for entry in entries {
            if self.cover_slots.contains_key(&entry.game_id) {
                continue;
            }
            self.cover_slots.insert(entry.game_id.clone(), CoverSlot::Loading);
            let recipe = self.recipe_db.get_by_id(&entry.game_id);
            let req = CoverRequest {
                game_id: entry.game_id.clone(),
                title: entry.title.clone(),
                steamgriddb_id: recipe.and_then(|r| r.metadata.steamgriddb_id),
                steam_app_id: recipe.and_then(|r| r.metadata.steam_app_id),
                cover_art_url: recipe.and_then(|r| r.metadata.cover_art_url.clone()),
                custom_cover_art_path: entry.custom_cover_art_path.clone(),
                covers_dir: self.settings.covers_dir.clone(),
                api_key: self.settings.steamgriddb_api_key.clone(),
            };
            spawn_load_cover(self.worker_tx.clone(), ctx.clone(), req);
        }
    }

    fn open_advisor_for_entry(&mut self, entry_id: &str) {
        let Some(entry) = self.library_store.get(entry_id).cloned() else {
            self.set_banner(format!("Game entry `{entry_id}` not found in library."), true);
            return;
        };
        let Some(recipe) = self.recipe_db.get_by_id(&entry.game_id).cloned() else {
            self.set_banner(
                format!("Recipe `{}` is not currently loaded locally.", entry.game_id),
                true,
            );
            return;
        };

        let preview_ctx = build_preview_context(&recipe, &entry.install_dir, &self.settings);
        let reg = host_registry_backend();
        self.advisor = Some(PatchAdvisorState::open(&entry, &recipe, &preview_ctx, reg.as_ref()));
    }

    fn open_next_queued_advisor(&mut self) {
        if self.advisor.is_some() {
            return;
        }
        while let Some(next_id) = self.pending_advisor_queue.pop_front() {
            if self.library_store.get(&next_id).is_some() {
                self.open_advisor_for_entry(&next_id);
                if self.advisor.is_some() {
                    break;
                }
            }
        }
    }

    fn launch_library_entry(&mut self, ctx: &egui::Context, entry_id: &str) {
        let Some(entry) = self.library_store.get(entry_id).cloned() else {
            return;
        };
        let Some(recipe) = self.recipe_db.get_by_id(&entry.game_id) else {
            self.set_banner(format!("Missing recipe `{}` for launching.", entry.game_id), true);
            return;
        };

        let store = ToolkitStore::new(&self.settings.toolkit_dir);
        let opts = LaunchOptions {
            game_root: entry.install_dir.clone(),
            release_id: entry.release_id.clone(),
            applied_fix_ids: entry.applied_fix_ids.clone(),
            extra_args: Vec::new(),
        };

        let cmd = match resolve_launch_command(recipe, &store, &opts) {
            Ok(cmd) => cmd,
            Err(err) => {
                let msg = format!("Launch failed for `{}`: {err}", entry.title);
                self.debug_console.push(DebugLogLine::now(
                    "LAUNCH",
                    msg.clone(),
                    DebugLogLevel::Error,
                ));
                self.debug_console.is_open = true;
                self.debug_console.mark_seen();
                self.set_banner(msg, true);
                return;
            }
        };

        match launch_game_captured(&cmd) {
            Ok(launched) => {
                let _ = self.library_store.record_launched(entry_id);
                let _ = self.library_store.save();
                let pid_txt =
                    launched.id().map_or_else(|| "unknown".to_owned(), |id| id.to_string());
                let mut msg = if launched.elevated {
                    format!("Launched `{}` via UAC elevation (PID {pid_txt}).", entry.title)
                } else {
                    format!("Launched `{}` (PID {pid_txt}).", entry.title)
                };
                if let Some(note) = &launched.note {
                    msg.push(' ');
                    msg.push_str(note);
                }
                self.set_banner(msg, false);
                self.debug_console.is_open = true;
                self.debug_console.mark_seen();
                self.debug_console.active_run_label = Some(entry.title.clone());
                spawn_monitor_launched_game(
                    self.worker_tx.clone(),
                    ctx.clone(),
                    MonitorGameJob { title: entry.title, cmd, launched },
                );
            }
            Err(err) => {
                let msg = format!("Launch failed for `{}`: {err}", entry.title);
                self.debug_console.push(DebugLogLine::now(
                    "LAUNCH",
                    msg.clone(),
                    DebugLogLevel::Error,
                ));
                self.debug_console.is_open = true;
                self.debug_console.mark_seen();
                self.set_banner(msg, true);
            }
        }
    }

    fn poll_worker_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.worker_rx.try_recv() {
            match msg {
                WorkerMessage::ScanFinished {
                    scanned_folders,
                    skipped_missing_folders,
                    result,
                } => {
                    self.handle_scan_finished(
                        ctx,
                        &scanned_folders,
                        &skipped_missing_folders,
                        result,
                    );
                }
                WorkerMessage::SyncFinished { outcome, reloaded_db } => {
                    self.handle_sync_finished(outcome, reloaded_db);
                }
                WorkerMessage::CoverLoaded(outcome) => {
                    self.handle_cover_loaded(ctx, outcome);
                }
                WorkerMessage::PatchEvent { entry_id, event } => {
                    if let Some(advisor) = &mut self.advisor {
                        if advisor.entry_id == entry_id {
                            advisor.on_patch_event(&event);
                        }
                    }
                }
                WorkerMessage::PatchFinished { entry_id, result } => {
                    self.handle_patch_finished(&entry_id, result);
                }
                WorkerMessage::RollbackFinished { entry_id, remaining_applied_fix_ids, result } => {
                    self.handle_rollback_finished(&entry_id, remaining_applied_fix_ids, result);
                }
                WorkerMessage::DebugLog(line) => {
                    self.debug_console.push(line);
                }
                WorkerMessage::GameExited { title, pid, exit_code, elapsed_ms, crashed } => {
                    self.debug_console.active_run_label = None;
                    let secs = elapsed_ms / 1000;
                    let tenths = (elapsed_ms % 1000) / 100;
                    let pid_suffix = pid.map_or_else(String::new, |p| format!(" [PID {p}]"));
                    if crashed {
                        self.debug_console.is_open = true;
                        self.debug_console.mark_seen();
                        let code_txt =
                            exit_code.map_or_else(|| "error".to_owned(), format_exit_code_hex);
                        self.set_banner(
                            format!(
                                "`{title}`{pid_suffix} crashed (exit {code_txt}) after {secs}.{tenths}s — see Debug Console."
                            ),
                            true,
                        );
                    } else {
                        self.set_banner(
                            format!("`{title}`{pid_suffix} exited cleanly after {secs}.{tenths}s."),
                            false,
                        );
                    }
                }
            }
        }
    }

    fn handle_scan_finished(
        &mut self,
        ctx: &egui::Context,
        scanned_folders: &[PathBuf],
        skipped_missing_folders: &[PathBuf],
        result: Result<ogsc_scanner::ScanReport, String>,
    ) {
        self.is_scanning = false;
        match result {
            Ok(report) => {
                for folder in scanned_folders {
                    let _ = self.library_store.add_scan_folder(folder);
                }
                let known_before: BTreeSet<String> =
                    self.library_store.entries().map(|e| e.entry_id.clone()).collect();
                let mut newly_imported_ids = Vec::new();

                for m in &report.matches {
                    let entry = self.library_store.upsert_scan_match(m).clone();
                    if !known_before.contains(&entry.entry_id) {
                        newly_imported_ids.push(entry.entry_id.clone());
                        if let Some(recipe) = self.recipe_db.get_by_id(&entry.game_id) {
                            let has_fixes = !recipe.fixes_for_release(&entry.release_id).is_empty();
                            if !recipe.known_issues.is_empty() && has_fixes {
                                self.pending_advisor_queue.push_back(entry.entry_id.clone());
                            }
                        }
                    }
                }

                if self.selected_entry_id.is_none() {
                    if let Some(first) = newly_imported_ids.first() {
                        self.selected_entry_id = Some(first.clone());
                    }
                }

                let _ = self.library_store.save();
                self.queue_missing_covers(ctx);
                let skipped_note = if skipped_missing_folders.is_empty() {
                    String::new()
                } else {
                    format!(" (skipped {} missing folder(s))", skipped_missing_folders.len())
                };
                self.set_banner(
                    format!(
                        "Scanned {} file(s) in {} ms — found {} game(s) ({} new){skipped_note}.",
                        report.files_inspected,
                        report.elapsed_ms,
                        report.matches.len(),
                        newly_imported_ids.len()
                    ),
                    false,
                );
                self.open_next_queued_advisor();
            }
            Err(err) => {
                self.set_banner(format!("Folder scan failed: {err}"), true);
            }
        }
    }

    fn handle_sync_finished(&mut self, outcome: SyncOutcome, reloaded_db: Option<RecipeDatabase>) {
        self.is_syncing = false;
        if let Some(db) = reloaded_db {
            self.recipe_db = db;
        }
        match outcome {
            SyncOutcome::Synced { fetched_count, skipped_invalid, total_local_recipes } => {
                let extra = if skipped_invalid.is_empty() {
                    String::new()
                } else {
                    format!(" ({} remote file(s) failed validation)", skipped_invalid.len())
                };
                self.set_banner(
                    format!(
                        "Synced {fetched_count} recipe(s) from GitHub — {total_local_recipes} local recipe(s) active{extra}."
                    ),
                    false,
                );
            }
            SyncOutcome::OfflineFallback { reason, total_local_recipes } => {
                self.set_banner(
                    format!(
                        "Offline/sync fallback ({total_local_recipes} local recipe(s) retained): {reason}"
                    ),
                    false,
                );
            }
        }
    }

    fn handle_cover_loaded(&mut self, ctx: &egui::Context, outcome: CoverLoadOutcome) {
        match outcome {
            CoverLoadOutcome::Loaded { game_id, cover, .. } => {
                let color_image =
                    ColorImage::from_rgba_unmultiplied([cover.width, cover.height], &cover.rgba);
                let handle = ctx.load_texture(
                    format!("cover_{game_id}"),
                    color_image,
                    egui::TextureOptions::LINEAR,
                );
                self.cover_slots.insert(game_id, CoverSlot::Ready(handle));
            }
            CoverLoadOutcome::Fallback { game_id, .. } => {
                self.cover_slots.insert(game_id, CoverSlot::Fallback);
            }
        }
    }

    fn handle_patch_finished(
        &mut self,
        entry_id: &str,
        result: Result<ogsc_patcher::ApplyReport, String>,
    ) {
        if let Ok(report) = &result {
            if let Some(entry) = self.library_store.get(entry_id).cloned() {
                let declined =
                    self.recipe_db.get_by_id(&entry.game_id).map_or_else(Vec::new, |r| {
                        r.fixes_for_release(&entry.release_id)
                            .into_iter()
                            .filter(|f| !report.applied_fix_ids.contains(&f.id))
                            .map(|f| f.id.clone())
                            .collect()
                    });
                let _ = self.library_store.record_applied_fixes(
                    entry_id,
                    report.applied_fix_ids.clone(),
                    declined,
                );
                let _ = self.library_store.save();
                self.set_banner(
                    format!(
                        "Patched `{}` cleanly ({} fix group(s)).",
                        entry.title,
                        report.applied_fix_ids.len()
                    ),
                    false,
                );
            }
        } else if let Err(err) = &result {
            self.set_banner(format!("Patch failed (auto-rolled back): {err}"), true);
        }

        if let Some(advisor) = &mut self.advisor {
            if advisor.entry_id == entry_id {
                advisor.finish(result);
            }
        }
    }

    fn handle_rollback_finished(
        &mut self,
        entry_id: &str,
        remaining_applied_fix_ids: Vec<String>,
        result: Result<ogsc_scanner::RollbackSummary, String>,
    ) {
        self.is_rolling_back = false;
        match result {
            Ok(summary) => {
                let _ = self.library_store.mark_rolled_back(entry_id, remaining_applied_fix_ids);
                let _ = self.library_store.save();
                self.set_banner(
                    format!(
                        "Restored vanilla files ({} restored, {} removed, {} registry edit(s) reverted).",
                        summary.restored_files,
                        summary.deleted_files,
                        summary.registry_actions_to_revert.len()
                    ),
                    false,
                );
            }
            Err(err) => {
                self.set_banner(format!("Rollback failed: {err}"), true);
            }
        }
    }

    fn render_top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top_toolbar")
            .frame(
                Frame::new()
                    .fill(BG_SURFACE)
                    .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
                    .inner_margin(Margin::symmetric(14, 10)),
            )
            .show(ctx, |ui| {
                self.render_toolbar_row(ui, ctx);
                ui.add_space(6.0);
                self.render_filter_row(ui);
                if let Some(banner) = self.banner.take() {
                    let keep = Self::render_status_banner(ui, &banner);
                    if keep {
                        self.banner = Some(banner);
                    }
                }
            });
    }

    fn render_toolbar_row(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let busy = self.is_busy();
        ui.horizontal_wrapped(|ui| {
            if let Some(logo) = &self.logo_texture {
                ui.add(egui::Image::new(logo).fit_to_exact_size(Vec2::new(26.0, 26.0)));
            }
            ui.label(RichText::new("REVIVEBOX").size(17.0).strong().color(ACCENT_AMBER));
            ui.label(RichText::new("Old Games Second Chance").size(12.0).color(TEXT_MUTED));
            paint_pill(ui, &format!("{} recipes", self.recipe_db.len()), ACCENT_CYAN);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Settings").clicked() {
                    self.settings_modal_draft = Some(self.settings.clone());
                }

                let console_label = if self.debug_console.unread_errors > 0 {
                    format!("Console ({})", self.debug_console.unread_errors)
                } else if self.debug_console.is_open {
                    "Console *".to_owned()
                } else {
                    "Console".to_owned()
                };
                let console_text = if self.debug_console.unread_errors > 0 {
                    RichText::new(console_label).color(STATUS_BLOCKER).strong()
                } else if self.debug_console.is_open {
                    RichText::new(console_label).color(ACCENT_CYAN).strong()
                } else {
                    RichText::new(console_label)
                };
                if ui.button(console_text).clicked() {
                    self.debug_console.is_open = !self.debug_console.is_open;
                    if self.debug_console.is_open {
                        self.debug_console.mark_seen();
                    }
                }

                ui.add_enabled_ui(!self.is_syncing, |ui| {
                    let label = if self.is_syncing { "Syncing..." } else { "Sync Recipes" };
                    if ui.button(label).clicked() {
                        self.is_syncing = true;
                        spawn_sync_recipes(
                            self.worker_tx.clone(),
                            ctx.clone(),
                            self.settings.clone(),
                        );
                    }
                });

                let scan_folders = self.library_store.library().scan_folders.clone();
                ui.add_enabled_ui(!busy && !scan_folders.is_empty(), |ui| {
                    if ui.button("Rescan").clicked() {
                        self.is_scanning = true;
                        spawn_scan_folders(
                            self.worker_tx.clone(),
                            ctx.clone(),
                            scan_folders,
                            self.recipe_db.clone(),
                        );
                    }
                });

                ui.add_enabled_ui(!busy, |ui| {
                    if primary_button(ui, "+ Add Game Folder").clicked() {
                        self.pick_and_scan_folder(ctx);
                    }
                });
            });
        });
    }

    fn render_filter_row(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Search:").size(12.0).color(TEXT_MUTED));
            ui.add(
                egui::TextEdit::singleline(&mut self.search_query)
                    .hint_text("Filter library by title...")
                    .desired_width(220.0),
            );
            ui.add_space(8.0);
            for filter in StatusFilter::ALL {
                if ui.selectable_label(self.status_filter == filter, filter.label()).clicked() {
                    self.status_filter = filter;
                }
            }
            if self.is_busy() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spinner();
                    ui.label(RichText::new("Working...").size(12.0).color(ACCENT_AMBER));
                });
            }
        });
    }

    fn render_status_banner(ui: &mut Ui, banner: &StatusBanner) -> bool {
        let mut keep = true;
        ui.add_space(4.0);
        let accent = if banner.is_error { STATUS_BLOCKER } else { STATUS_PATCHED };
        Frame::new()
            .fill(translucent_tint(accent))
            .stroke(Stroke::new(1.0_f32, accent))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(Margin::symmetric(10, 5))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&banner.message).size(12.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Dismiss").clicked() {
                            keep = false;
                        }
                    });
                });
            });
        keep
    }

    fn pick_and_scan_folder(&mut self, ctx: &egui::Context) {
        if let Some(folder) =
            rfd::FileDialog::new().set_title("Select Game Folder to Scan").pick_folder()
        {
            self.is_scanning = true;
            spawn_scan_folders(
                self.worker_tx.clone(),
                ctx.clone(),
                vec![folder],
                self.recipe_db.clone(),
            );
        }
    }

    fn render_central_library(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(Frame::new().fill(crate::theme::BG_CANVAS).inner_margin(Margin::same(16)))
            .show(ctx, |ui| {
                let total_entries = self.library_store.library().entries.len();
                if total_entries == 0 {
                    self.render_empty_library_state(ui, ctx);
                    return;
                }

                let query_lower = self.search_query.trim().to_ascii_lowercase();
                let filtered: Vec<LibraryEntry> = self
                    .library_store
                    .entries()
                    .filter(|e| {
                        self.status_filter.matches(e.status)
                            && (query_lower.is_empty()
                                || e.title.to_ascii_lowercase().contains(&query_lower)
                                || e.release_label.to_ascii_lowercase().contains(&query_lower))
                    })
                    .cloned()
                    .collect();

                if filtered.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(60.0);
                        ui.label(
                            RichText::new(
                                "No imported games match the current search/status filter.",
                            )
                            .size(15.0)
                            .color(TEXT_MUTED),
                        );
                    });
                    return;
                }

                let card_w = 192.0_f32;
                let gap = 14.0_f32;
                let avail_w = ui.available_width().max(card_w);
                let cols = grid_column_count(avail_w, card_w, gap);
                let mut clicked_id: Option<String> = None;

                ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for row in filtered.chunks(cols) {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = gap;
                            for entry in row {
                                if self.render_game_card(ui, entry, card_w) {
                                    clicked_id = Some(entry.entry_id.clone());
                                }
                            }
                        });
                        ui.add_space(gap);
                    }
                });

                if let Some(id) = clicked_id {
                    self.selected_entry_id = Some(id);
                }
            });
    }

    fn render_empty_library_state(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        ui.vertical_centered(|ui| {
            ui.add_space(60.0);
            Frame::new()
                .fill(BG_SURFACE)
                .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::same(24))
                .show(ui, |ui| {
                    ui.set_max_width(480.0);
                    if let Some(logo) = &self.logo_texture {
                        ui.add(egui::Image::new(logo).fit_to_exact_size(Vec2::new(72.0, 72.0)));
                        ui.add_space(6.0);
                    }
                    ui.label(RichText::new("ReviveBox — Old Games Second Chance").size(18.0).strong());
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "100% BYOG (Bring Your Own Game): Point the scanner at your Retail CD, GOG, Steam, or EA App folders. Executables are fingerprinted locally, backed up to `.ogsc_backup` before edits, and never altered without your consent.",
                        )
                        .size(13.0)
                        .color(TEXT_MUTED),
                    );
                    ui.add_space(12.0);
                    ui.add_enabled_ui(!self.is_busy(), |ui| {
                        if primary_button(ui, "+ Add Game Folder").clicked() {
                            self.pick_and_scan_folder(ctx);
                        }
                    });
                });
        });
    }

    fn render_game_card(&self, ui: &mut Ui, entry: &LibraryEntry, card_w: f32) -> bool {
        let is_selected = self.selected_entry_id.as_deref() == Some(entry.entry_id.as_str());
        let border = if is_selected {
            Stroke::new(2.0_f32, BORDER_ACTIVE)
        } else {
            Stroke::new(1.0_f32, BORDER_SUBTLE)
        };
        let recipe = self.recipe_db.get_by_id(&entry.game_id);
        let year = recipe.map(|r| r.release_year);

        let resp = ui
            .push_id(&entry.entry_id, |ui| {
                Frame::new()
                    .fill(BG_ELEVATED)
                    .stroke(border)
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::same(8))
                    .show(ui, |ui| {
                        let inner_w = card_w - 16.0;
                        ui.vertical(|ui| {
                            ui.set_width(inner_w);
                            ui.set_max_width(inner_w);
                            let cover_size = Vec2::new(inner_w, 236.0);

                            if let Some(CoverSlot::Ready(tex)) =
                                self.cover_slots.get(&entry.game_id)
                            {
                                ui.add(
                                    egui::Image::new(tex)
                                        .fit_to_exact_size(cover_size)
                                        .corner_radius(CornerRadius::same(6)),
                                );
                            } else {
                                let (rect, _) =
                                    ui.allocate_exact_size(cover_size, egui::Sense::hover());
                                paint_fallback_cover(
                                    ui,
                                    rect,
                                    &entry.game_id,
                                    &entry.title,
                                    year,
                                    entry.status,
                                );
                            }

                            ui.add_space(6.0);
                            ui.add(
                                egui::Label::new(RichText::new(&entry.title).strong().size(13.5))
                                    .truncate(),
                            );
                            ui.add_space(4.0);
                            ui.allocate_ui_with_layout(
                                Vec2::new(inner_w, 20.0),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    let year_txt =
                                        year.map_or_else(|| "Legacy".to_owned(), |y| y.to_string());
                                    ui.label(RichText::new(year_txt).size(11.5).color(TEXT_MUTED));
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            let (badge_lbl, badge_col) =
                                                status_badge_style(entry.status);
                                            paint_pill(ui, badge_lbl, badge_col);
                                        },
                                    );
                                },
                            );
                        });
                    })
                    .response
            })
            .inner;

        resp.interact(egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
    }

    fn render_details_if_selected(&mut self, ctx: &egui::Context) {
        let Some(selected_id) = self.selected_entry_id.clone() else {
            return;
        };
        let Some(entry) = self.library_store.get(&selected_id).cloned() else {
            self.selected_entry_id = None;
            return;
        };
        let recipe = self.recipe_db.get_by_id(&entry.game_id).cloned();
        let cover_tex = match self.cover_slots.get(&entry.game_id) {
            Some(CoverSlot::Ready(tex)) => Some(tex),
            _ => None,
        };

        if let Some(action) =
            render_details_panel(ctx, &entry, recipe.as_ref(), cover_tex, self.is_busy())
        {
            match action {
                DetailsAction::Launch(id) => self.launch_library_entry(ctx, &id),
                DetailsAction::OpenAdvisor(id) => self.open_advisor_for_entry(&id),
                DetailsAction::Rollback(id) => {
                    if let Some(rec) = recipe {
                        self.is_rolling_back = true;
                        spawn_rollback(
                            self.worker_tx.clone(),
                            ctx.clone(),
                            id,
                            rec,
                            entry.install_dir,
                        );
                    }
                }
                DetailsAction::SetCustomCover(id) => {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_title("Select Cover Artwork Image")
                        .add_filter("Images", &["png", "jpg", "jpeg", "webp"])
                        .pick_file()
                    {
                        if let Err(err) = self.library_store.set_custom_cover_art(&id, Some(path)) {
                            self.set_banner(format!("Could not set custom cover: {err}"), true);
                        } else {
                            let _ = self.library_store.save();
                            self.cover_slots.remove(&entry.game_id);
                            self.queue_missing_covers(ctx);
                        }
                    }
                }
                DetailsAction::OpenInstallDir(path) => open_folder_in_explorer(&path),
                DetailsAction::RemoveFromLibrary(id) => {
                    let _ = self.library_store.remove_entry(&id);
                    let _ = self.library_store.save();
                    if self.selected_entry_id.as_deref() == Some(id.as_str()) {
                        self.selected_entry_id = None;
                    }
                }
                DetailsAction::ClosePanel => self.selected_entry_id = None,
            }
        }
    }

    fn render_active_modals(&mut self, ctx: &egui::Context) {
        if let Some(mut advisor) = self.advisor.take() {
            let act = render_patch_advisor_modal(ctx, &mut advisor);
            let mut keep_open = true;
            match act {
                Some(AdvisorAction::ApplySelected(fix_ids)) => {
                    advisor.begin_applying(&fix_ids);
                    spawn_apply_fixes(
                        self.worker_tx.clone(),
                        ctx.clone(),
                        ApplyFixesJob {
                            entry_id: advisor.entry_id.clone(),
                            recipe: advisor.recipe.clone(),
                            release_id: advisor.release_id.clone(),
                            install_dir: advisor.install_dir.clone(),
                            selected_fix_ids: fix_ids,
                            settings: self.settings.clone(),
                        },
                    );
                }
                Some(AdvisorAction::DeclineVanilla(declined)) => {
                    let _ = self.library_store.mark_declined_vanilla(&advisor.entry_id, declined);
                    let _ = self.library_store.save();
                    self.set_banner(
                        format!("Marked `{}` as Vanilla (unchanged).", advisor.title),
                        false,
                    );
                    keep_open = false;
                }
                Some(AdvisorAction::LaunchAfterPatch) => {
                    let entry_id = advisor.entry_id.clone();
                    keep_open = false;
                    self.launch_library_entry(ctx, &entry_id);
                }
                Some(AdvisorAction::Close) => {
                    keep_open = false;
                }
                None => {}
            }
            if keep_open {
                self.advisor = Some(advisor);
            } else {
                self.open_next_queued_advisor();
            }
        }

        if let Some(mut draft) = self.settings_modal_draft.take() {
            let folders = self.library_store.library().scan_folders.clone();
            let act = render_settings_modal(ctx, &mut draft, &folders, &mut self.show_api_key);
            match act {
                Some(SettingsAction::SaveAndClose) => {
                    self.commit_settings(ctx, draft);
                }
                Some(SettingsAction::SaveAndSyncRecipes) => {
                    self.commit_settings(ctx, draft);
                    self.is_syncing = true;
                    spawn_sync_recipes(self.worker_tx.clone(), ctx.clone(), self.settings.clone());
                }
                Some(SettingsAction::RescanAllFolders) => {
                    if !folders.is_empty() {
                        self.is_scanning = true;
                        spawn_scan_folders(
                            self.worker_tx.clone(),
                            ctx.clone(),
                            folders,
                            self.recipe_db.clone(),
                        );
                    }
                }
                Some(SettingsAction::RemoveScanFolder(dir)) => {
                    let _ = self.library_store.remove_scan_folder(&dir);
                    let _ = self.library_store.save();
                    self.settings_modal_draft = Some(draft);
                }
                Some(SettingsAction::Cancel) => {}
                None => {
                    self.settings_modal_draft = Some(draft);
                }
            }
        }
    }

    fn commit_settings(&mut self, ctx: &egui::Context, draft: AppSettings) {
        let key_or_dir_changed = draft.steamgriddb_api_key != self.settings.steamgriddb_api_key
            || draft.covers_dir != self.settings.covers_dir;
        self.settings = draft;
        if let Err(err) = self.settings.save(&self.settings_path) {
            self.set_banner(format!("Failed saving settings.json: {err}"), true);
            return;
        }
        if key_or_dir_changed {
            self.cover_slots.retain(|_, slot| matches!(slot, CoverSlot::Ready(_)));
            self.queue_missing_covers(ctx);
        }
        self.set_banner("Saved launcher settings.", false);
    }
}

impl eframe::App for OgscApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_worker_messages(ctx);
        self.render_top_bar(ctx);
        render_debug_console_panel(ctx, &mut self.debug_console);
        self.render_details_if_selected(ctx);
        self.render_central_library(ctx);
        self.render_active_modals(ctx);
    }
}

fn load_initial_recipes(recipes_dir: &std::path::Path, bundled_dirs: &[PathBuf]) -> RecipeDatabase {
    if recipes_dir.is_dir() {
        if let Ok(loaded) = RecipeDatabase::load_from_dir(recipes_dir, false) {
            if !loaded.db.is_empty() {
                return loaded.db;
            }
        }
    }
    for candidate in bundled_dirs {
        if candidate.is_dir() {
            if let Ok(loaded) = RecipeDatabase::load_from_dir(candidate, false) {
                return loaded.db;
            }
        }
    }
    RecipeDatabase::new()
}

fn grid_column_count(available_width: f32, card_width: f32, gap: f32) -> usize {
    let mut cols = 1_usize;
    for next in 2..=16_u16 {
        let needed = f32::from(next) * card_width + f32::from(next - 1) * gap;
        if needed > available_width {
            break;
        }
        cols = usize::from(next);
    }
    cols
}

fn translucent_tint(color: egui::Color32) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 26)
}

#[cfg(windows)]
fn open_folder_in_explorer(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}

#[cfg(not(windows))]
fn open_folder_in_explorer(path: &std::path::Path) {
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}
