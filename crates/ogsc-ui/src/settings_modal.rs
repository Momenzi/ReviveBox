//! Settings modal managing GitHub recipe sync, SteamGridDB key, `runtime_wrappers`, and scan folders.

use std::path::PathBuf;

use egui::{CornerRadius, Frame, Margin, RichText, ScrollArea, Stroke, Ui};
use ogsc_wrappers::ToolkitStore;

use crate::settings::AppSettings;
use crate::theme::{
    paint_pill, primary_button, ACCENT_AMBER, ACCENT_CYAN, BG_ELEVATED, BG_SURFACE, BORDER_SUBTLE,
    STATUS_NEEDS_FIXES, STATUS_PATCHED, TEXT_MUTED,
};

/// Action returned by the Settings modal dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsAction {
    /// Persist updated settings to `settings.json` and close the modal.
    SaveAndClose,
    /// Save settings and trigger immediate GitHub remote recipe sync.
    SaveAndSyncRecipes,
    /// Rescan all configured library folders now.
    RescanAllFolders,
    /// Remove a saved scan root folder from `library.json`.
    RemoveScanFolder(PathBuf),
    /// Discard edits and close the modal.
    Cancel,
}

/// Renders the centered Settings modal dialog.
pub fn render_settings_modal(
    ctx: &egui::Context,
    draft: &mut AppSettings,
    scan_folders: &[PathBuf],
    show_api_key: &mut bool,
) -> Option<SettingsAction> {
    let mut action = None;
    let max_width = (ctx.screen_rect().width() - 48.0).clamp(460.0, 720.0);
    let max_height = (ctx.screen_rect().height() - 56.0).clamp(360.0, 620.0);

    let frame = Frame::new()
        .fill(BG_SURFACE)
        .stroke(Stroke::new(1.5_f32, BORDER_SUBTLE))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(16));

    egui::Modal::new(egui::Id::new("settings_modal")).frame(frame).show(ctx, |ui| {
        ui.set_min_width(max_width.min(620.0));
        ui.set_max_width(max_width);
        ui.set_max_height(max_height);

        ui.label(RichText::new("Launcher & Engine Settings").size(18.0).strong());
        ui.label(
            RichText::new(
                "All credentials and paths remain local in `%LOCALAPPDATA%\\OldGamesSecondChance`.",
            )
            .size(12.0)
            .color(TEXT_MUTED),
        );
        ui.add_space(6.0);
        ui.separator();

        ScrollArea::vertical().max_height((max_height - 125.0).max(200.0)).show(ui, |ui| {
            render_github_sync_section(ui, draft);
            ui.add_space(8.0);
            render_steamgriddb_section(ui, draft, show_api_key);
            ui.add_space(8.0);
            render_toolkits_section(ui, draft);
            ui.add_space(8.0);
            if let Some(act) = render_scan_folders_section(ui, scan_folders) {
                action = Some(act);
            }
        });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if primary_button(ui, "Save Settings").clicked() {
                action = Some(SettingsAction::SaveAndClose);
            }
            if ui.button("Save & Sync Recipes").clicked() {
                action = Some(SettingsAction::SaveAndSyncRecipes);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    action = Some(SettingsAction::Cancel);
                }
            });
        });
    });

    action
}

fn render_github_sync_section(ui: &mut Ui, draft: &mut AppSettings) {
    Frame::new()
        .fill(BG_ELEVATED)
        .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.label(
                RichText::new("GitHub Remote Recipe Sync").size(13.5).strong().color(ACCENT_AMBER),
            );
            ui.add_space(4.0);
            egui::Grid::new("settings_github_grid").num_columns(2).spacing([8.0, 6.0]).show(
                ui,
                |ui| {
                    ui.label("Repository URL:");
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.github_repo_url).desired_width(420.0),
                    );
                    ui.end_row();

                    ui.label("Branch:");
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.github_branch).desired_width(180.0),
                    );
                    ui.end_row();

                    ui.label("Recipes Folder:");
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.github_recipes_path)
                            .desired_width(280.0),
                    );
                    ui.end_row();
                },
            );
            ui.label(
                RichText::new(format!("Local recipes directory: {}", draft.recipes_dir.display()))
                    .size(11.5)
                    .monospace()
                    .color(TEXT_MUTED),
            );
        });
}

fn render_steamgriddb_section(ui: &mut Ui, draft: &mut AppSettings, show_api_key: &mut bool) {
    Frame::new()
        .fill(BG_ELEVATED)
        .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.label(RichText::new("SteamGridDB Cover Art (Optional)").size(13.5).strong().color(ACCENT_CYAN));
            ui.label(
                RichText::new("When blank or offline, sharp typographic fallback covers are rendered automatically.")
                    .size(11.5)
                    .color(TEXT_MUTED),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("API Key:");
                ui.add(
                    egui::TextEdit::singleline(&mut draft.steamgriddb_api_key)
                        .password(!*show_api_key)
                        .hint_text("Optional SteamGridDB v2 Bearer token")
                        .desired_width(340.0),
                );
                let toggle_lbl = if *show_api_key { "Hide" } else { "Show" };
                if ui.button(toggle_lbl).clicked() {
                    *show_api_key = !*show_api_key;
                }
            });
        });
}

fn render_toolkits_section(ui: &mut Ui, draft: &mut AppSettings) {
    Frame::new()
        .fill(BG_ELEVATED)
        .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.label(RichText::new("Compatibility Wrappers & Emulators").size(13.5).strong());
            ui.add_space(4.0);

            let mut toolkit_str = draft.toolkit_dir.to_string_lossy().into_owned();
            ui.horizontal(|ui| {
                ui.label("runtime_wrappers:");
                if ui
                    .add(egui::TextEdit::singleline(&mut toolkit_str).desired_width(320.0))
                    .changed()
                {
                    draft.toolkit_dir = PathBuf::from(&toolkit_str);
                }
                if ui.button("Browse...").clicked() {
                    if let Some(dir) = rfd::FileDialog::new()
                        .set_title("Select runtime_wrappers Folder")
                        .pick_folder()
                    {
                        draft.toolkit_dir = dir;
                    }
                }
            });

            ui.add_space(4.0);
            let store = ToolkitStore::new(&draft.toolkit_dir);
            ui.horizontal_wrapped(|ui| {
                for item in store.availability() {
                    let (label, color) = if item.installed {
                        (format!("{} (ready)", item.kind.slug()), STATUS_PATCHED)
                    } else if item.kind.official_download().is_some() {
                        (format!("{} (auto-download)", item.kind.slug()), ACCENT_CYAN)
                    } else {
                        (format!("{} (missing)", item.kind.slug()), STATUS_NEEDS_FIXES)
                    };
                    paint_pill(ui, &label, color);
                }
            });
        });
}

fn render_scan_folders_section(ui: &mut Ui, scan_folders: &[PathBuf]) -> Option<SettingsAction> {
    let mut action = None;
    Frame::new()
        .fill(BG_ELEVATED)
        .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("Tracked Game Folders ({})", scan_folders.len()))
                        .size(13.5)
                        .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !scan_folders.is_empty() && ui.small_button("Rescan All").clicked() {
                        action = Some(SettingsAction::RescanAllFolders);
                    }
                });
            });
            ui.add_space(4.0);
            if scan_folders.is_empty() {
                ui.label(
                    RichText::new(
                        "No game folders added yet. Click `+ Add Game Folder` in the top bar.",
                    )
                    .size(11.5)
                    .color(TEXT_MUTED),
                );
            } else {
                for dir in scan_folders {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(dir.display().to_string()).size(11.5).monospace());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Remove").clicked() {
                                action = Some(SettingsAction::RemoveScanFolder(dir.clone()));
                            }
                        });
                    });
                }
            }
        });
    action
}
