//! Right-hand Game Details side panel with launch, Patch Advisor trigger, and rollback controls.

use std::path::PathBuf;

use egui::{
    Color32, CornerRadius, Frame, Margin, RichText, ScrollArea, Stroke, TextureHandle, Ui, Vec2,
};
use ogsc_recipes::{GameRecipe, IssueSeverity};
use ogsc_scanner::{GameStatus, LibraryEntry};

use crate::theme::{
    paint_fallback_cover, paint_pill, status_badge_style, ACCENT_AMBER, ACCENT_CYAN, BG_ELEVATED,
    BG_SURFACE, BORDER_SUBTLE, STATUS_BLOCKER, STATUS_PATCHED, TEXT_MUTED,
};

/// Action emitted by the Game Details panel to `OgscApp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailsAction {
    /// Launch the selected game using `ogsc-patcher::launch_game`.
    Launch(String),
    /// Re-open the consent-driven Patch Advisor popup for this entry.
    OpenAdvisor(String),
    /// Roll back applied patches & `HKCU` registry tweaks cleanly via `BackupManager`.
    Rollback(String),
    /// Prompt the user to select a custom local cover image file for this library entry.
    SetCustomCover(String),
    /// Open the installation directory in the host file manager.
    OpenInstallDir(PathBuf),
    /// Remove the entry from `library.json` (non-destructive; never deletes game files).
    RemoveFromLibrary(String),
    /// Deselect the game and collapse the side panel.
    ClosePanel,
}

/// Renders the right-hand details side panel for `entry`.
pub fn render_details_panel(
    ctx: &egui::Context,
    entry: &LibraryEntry,
    recipe: Option<&GameRecipe>,
    cover_texture: Option<&TextureHandle>,
    is_busy: bool,
) -> Option<DetailsAction> {
    let mut action = None;

    egui::SidePanel::right("game_details_panel")
        .resizable(true)
        .default_width(340.0)
        .min_width(300.0)
        .max_width(440.0)
        .frame(
            Frame::new()
                .fill(BG_SURFACE)
                .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
                .inner_margin(Margin::same(14)),
        )
        .show(ctx, |ui| {
            ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                if render_details_header(ui, entry, recipe, cover_texture) {
                    action = Some(DetailsAction::ClosePanel);
                }
                ui.add_space(10.0);
                if let Some(act) = render_primary_actions(ui, entry, recipe, is_busy) {
                    action = Some(act);
                }
                ui.add_space(10.0);
                if let Some(rec) = recipe {
                    render_version_requirements_card(ui, entry, rec);
                    ui.add_space(8.0);
                }
                render_metadata_card(ui, entry, recipe);
                ui.add_space(8.0);
                render_applied_fixes_card(ui, entry, recipe);
                ui.add_space(10.0);
                if let Some(act) = render_secondary_actions(ui, entry, is_busy) {
                    action = Some(act);
                }
            });
        });

    action
}

fn render_details_header(
    ui: &mut Ui,
    entry: &LibraryEntry,
    recipe: Option<&GameRecipe>,
    cover_texture: Option<&TextureHandle>,
) -> bool {
    let mut close_clicked = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new("Game Details").size(12.0).color(ACCENT_AMBER).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Close").clicked() {
                close_clicked = true;
            }
        });
    });
    ui.add_space(6.0);

    ui.horizontal(|ui| {
        let thumb_size = Vec2::new(96.0, 144.0);
        if let Some(tex) = cover_texture {
            ui.add(
                egui::Image::new(tex)
                    .fit_to_exact_size(thumb_size)
                    .corner_radius(CornerRadius::same(6)),
            );
        } else {
            let (rect, _) = ui.allocate_exact_size(thumb_size, egui::Sense::hover());
            paint_fallback_cover(
                ui,
                rect,
                &entry.game_id,
                &entry.title,
                recipe.map(|r| r.release_year),
                entry.status,
            );
        }

        ui.vertical(|ui| {
            ui.label(RichText::new(&entry.title).size(16.5).strong());
            if let Some(rec) = recipe {
                let studio = rec
                    .developer
                    .as_deref()
                    .or(rec.publisher.as_deref())
                    .unwrap_or("Legacy PC Release");
                ui.label(
                    RichText::new(format!("{} • {studio}", rec.release_year))
                        .size(12.0)
                        .color(TEXT_MUTED),
                );
            }
            ui.add_space(4.0);
            let (badge_lbl, badge_col) = status_badge_style(entry.status);
            paint_pill(ui, badge_lbl, badge_col);
            ui.add_space(4.0);
            ui.label(RichText::new(&entry.release_label).size(12.0).color(TEXT_MUTED));
            ui.label(
                RichText::new(format!("Match: {:?}", entry.match_confidence))
                    .size(11.0)
                    .color(ACCENT_CYAN),
            );
        });
    });

    close_clicked
}

fn render_primary_actions(
    ui: &mut Ui,
    entry: &LibraryEntry,
    recipe: Option<&GameRecipe>,
    is_busy: bool,
) -> Option<DetailsAction> {
    let mut action = None;

    ui.add_enabled_ui(!is_busy && recipe.is_some(), |ui| {
        let launch_label = match entry.status {
            GameStatus::NeedsFixes => "Launch Anyway",
            GameStatus::Patched => "Launch Game (Patched)",
            GameStatus::DeclinedVanilla => "Launch Vanilla",
        };
        let btn_resp = ui.add_sized(
            [ui.available_width(), 34.0],
            egui::Button::new(
                RichText::new(launch_label).strong().color(crate::theme::ACCENT_AMBER_TEXT),
            )
            .fill(ACCENT_AMBER)
            .corner_radius(CornerRadius::same(6)),
        );
        if btn_resp.clicked() {
            action = Some(DetailsAction::Launch(entry.entry_id.clone()));
        }
    });

    ui.add_space(6.0);
    ui.add_enabled_ui(!is_busy && recipe.is_some(), |ui| {
        if primary_button_secondary(ui, "Review / Configure Fixes").clicked() {
            action = Some(DetailsAction::OpenAdvisor(entry.entry_id.clone()));
        }
    });

    let backup_dir = recipe.map_or(".ogsc_backup", |r| r.rollback.backup_directory.as_str());
    let has_backup = !entry.applied_fix_ids.is_empty()
        || entry.install_dir.join(backup_dir).join("journal.json").is_file();

    ui.add_space(4.0);
    ui.add_enabled_ui(!is_busy && recipe.is_some() && has_backup, |ui| {
        let rb_btn = ui.add_sized(
            [ui.available_width(), 28.0],
            egui::Button::new(RichText::new("Restore Original (Rollback)").color(STATUS_BLOCKER))
                .fill(BG_ELEVATED)
                .stroke(Stroke::new(1.0_f32, STATUS_BLOCKER))
                .corner_radius(CornerRadius::same(6)),
        );
        if rb_btn.clicked() {
            action = Some(DetailsAction::Rollback(entry.entry_id.clone()));
        }
    });

    action
}

fn primary_button_secondary(ui: &mut Ui, label: &str) -> egui::Response {
    ui.add_sized(
        [ui.available_width(), 30.0],
        egui::Button::new(RichText::new(label).strong())
            .fill(BG_ELEVATED)
            .stroke(Stroke::new(1.0_f32, ACCENT_CYAN))
            .corner_radius(CornerRadius::same(6)),
    )
}

fn render_version_requirements_card(ui: &mut Ui, entry: &LibraryEntry, recipe: &GameRecipe) {
    let incompatible = recipe.is_release_incompatible(&entry.release_id);
    let border_col = if incompatible {
        STATUS_BLOCKER
    } else if recipe.metadata.required_game_version.is_some() {
        ACCENT_AMBER
    } else {
        BORDER_SUBTLE
    };
    let fill_col = if incompatible {
        Color32::from_rgba_unmultiplied(
            STATUS_BLOCKER.r(),
            STATUS_BLOCKER.g(),
            STATUS_BLOCKER.b(),
            24,
        )
    } else {
        BG_ELEVATED
    };

    Frame::new()
        .fill(fill_col)
        .stroke(Stroke::new(1.0_f32, border_col))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Version & Requirements").size(12.5).strong());
                if incompatible {
                    paint_pill(ui, "INCOMPATIBLE", STATUS_BLOCKER);
                } else {
                    paint_pill(ui, "Supported", STATUS_PATCHED);
                }
            });
            ui.add_space(4.0);
            let detected_ver = recipe
                .release_by_id(&entry.release_id)
                .map_or(entry.release_label.as_str(), |r| r.version.as_str());
            ui.label(
                RichText::new(format!("Detected: v{detected_ver} ({})", entry.release_label))
                    .size(11.5)
                    .color(TEXT_MUTED),
            );
            let req_text = recipe.metadata.required_game_version.clone().unwrap_or_else(|| {
                recipe
                    .releases
                    .iter()
                    .map(|r| format!("v{} ({})", r.version, r.label))
                    .collect::<Vec<_>>()
                    .join(", ")
            });
            let req_col = if incompatible { STATUS_BLOCKER } else { ACCENT_AMBER };
            ui.label(RichText::new(format!("Required: {req_text}")).size(11.5).strong().color(req_col));
            if incompatible {
                ui.add_space(2.0);
                ui.label(
                    RichText::new(
                        "Your installed build is known-incompatible with the core fix. Please use the required version listed above.",
                    )
                    .size(11.5)
                    .color(STATUS_BLOCKER),
                );
            }
        });
}

fn render_metadata_card(ui: &mut Ui, entry: &LibraryEntry, recipe: Option<&GameRecipe>) {
    Frame::new()
        .fill(BG_ELEVATED)
        .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.label(RichText::new("Installation & Sandbox").size(12.5).strong());
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("Folder: {}", entry.install_dir.display()))
                    .size(11.5)
                    .monospace()
                    .color(TEXT_MUTED),
            );
            ui.label(
                RichText::new(format!(
                    "Executable: {}",
                    entry.executable_path.file_name().and_then(|n| n.to_str()).unwrap_or("unknown")
                ))
                .size(11.5)
                .monospace()
                .color(TEXT_MUTED),
            );

            if let Some(rec) = recipe {
                let sb_text = rec.sandbox.as_ref().map_or_else(
                    || "Sandbox: None configured".to_owned(),
                    |sb| {
                        format!(
                            "Sandbox: `{}` (enabled={})",
                            sb.sandbox_root, sb.enabled_by_default
                        )
                    },
                );
                ui.label(RichText::new(sb_text).size(11.5).color(TEXT_MUTED));
                let blockers = rec
                    .known_issues
                    .iter()
                    .filter(|i| matches!(i.severity, IssueSeverity::Blocker | IssueSeverity::Major))
                    .count();
                ui.label(
                    RichText::new(format!(
                        "Cataloged issues: {} ({} blocker/major)",
                        rec.known_issues.len(),
                        blockers
                    ))
                    .size(11.5)
                    .color(TEXT_MUTED),
                );
            }

            if let Some(last) = &entry.last_launched_at {
                let short_ts = last.split('T').next().unwrap_or(last);
                ui.label(
                    RichText::new(format!("Last launched: {short_ts}"))
                        .size(11.5)
                        .color(TEXT_MUTED),
                );
            }
        });
}

fn render_applied_fixes_card(ui: &mut Ui, entry: &LibraryEntry, recipe: Option<&GameRecipe>) {
    Frame::new()
        .fill(BG_ELEVATED)
        .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.label(
                RichText::new(format!("Applied Fixes ({})", entry.applied_fix_ids.len()))
                    .size(12.5)
                    .strong(),
            );
            ui.add_space(4.0);
            if entry.applied_fix_ids.is_empty() {
                let empty_msg = match entry.status {
                    GameStatus::DeclinedVanilla => "User opted to keep vanilla installation.",
                    _ => "No fixes applied yet — click Review / Configure Fixes.",
                };
                ui.label(RichText::new(empty_msg).size(11.5).color(TEXT_MUTED));
            } else {
                for fix_id in &entry.applied_fix_ids {
                    let title = recipe
                        .and_then(|r| r.fix_by_id(fix_id))
                        .map_or(fix_id.as_str(), |f| f.title.as_str());
                    ui.label(
                        RichText::new(format!("  • {title}")).size(12.0).color(STATUS_PATCHED),
                    );
                }
            }
        });
}

fn render_secondary_actions(
    ui: &mut Ui,
    entry: &LibraryEntry,
    is_busy: bool,
) -> Option<DetailsAction> {
    let mut action = None;
    ui.horizontal_wrapped(|ui| {
        if ui.button("Set Cover...").clicked() {
            action = Some(DetailsAction::SetCustomCover(entry.entry_id.clone()));
        }
        if ui.button("Open Folder").clicked() {
            action = Some(DetailsAction::OpenInstallDir(entry.install_dir.clone()));
        }
        ui.add_enabled_ui(!is_busy, |ui| {
            if ui.button("Remove from Library").clicked() {
                action = Some(DetailsAction::RemoveFromLibrary(entry.entry_id.clone()));
            }
        });
    });
    action
}
