//! Consent-driven Patch Advisor modal with `plan_fixes` previews and live `PatchEvent` console.

use std::collections::BTreeMap;
use std::path::PathBuf;

use egui::{Color32, CornerRadius, Frame, Margin, RichText, ScrollArea, Stroke, Ui};
use ogsc_patcher::{plan_fixes, ApplyReport, PatchContext, PatchEvent, PatchPlan, RegistryBackend};
use ogsc_recipes::{FixOption, GameRecipe, HealthStatus};
use ogsc_scanner::LibraryEntry;

use crate::theme::{
    health_badge_style, paint_pill, primary_button, severity_badge_style, ACCENT_AMBER,
    ACCENT_CYAN, BG_CONSOLE, BG_ELEVATED, BG_SURFACE, BORDER_SUBTLE, STATUS_BLOCKER,
    STATUS_NEEDS_FIXES, STATUS_PATCHED, TEXT_MUTED, TEXT_PRIMARY,
};

/// Current interactive phase inside the Patch Advisor modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvisorPhase {
    /// Reviewing `known_issues`, checkboxes, and expandable `plan_fixes` dry-run previews.
    Review,
    /// Executing selected fixes asynchronously with live progress bar and action console.
    Applying,
    /// All selected fixes succeeded cleanly.
    Completed {
        /// Applied fix IDs confirmed by `PatchEngine`.
        applied_fix_ids: Vec<String>,
    },
    /// Execution failed and partial edits from the failing fix were rolled back automatically.
    Failed {
        /// Human-readable failure reason.
        error: String,
    },
}

/// Color severity category for a formatted console log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Standard step/progress information.
    Info,
    /// Successful verification or mutation.
    Success,
    /// Skipped step or warning.
    Warning,
    /// Terminal execution failure.
    Error,
}

/// Single transparent audit log line displayed inside the Patch Advisor execution console.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleLogLine {
    /// Short uppercase subsystem tag (`STEP`, `HTTPS`, `BACKUP`, `FILE`, `WRAPPER`, `REG`, etc.).
    pub tag: &'static str,
    /// Detailed line content showing exact paths/keys.
    pub text: String,
    /// Display severity level.
    pub level: LogLevel,
}

/// User action returned by rendering the Patch Advisor modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvisorAction {
    /// User consented and clicked "Apply Selected Fixes".
    ApplySelected(Vec<String>),
    /// User chose "Decline / Keep Original" to record `DeclinedVanilla` in `library.json`.
    DeclineVanilla(Vec<String>),
    /// User clicked "Launch Game Now" after fixes completed.
    LaunchAfterPatch,
    /// User dismissed the modal without changing patch state.
    Close,
}

/// Complete modal state for reviewing and applying compatibility fixes to one library entry.
#[derive(Debug, Clone)]
pub struct PatchAdvisorState {
    /// Library entry ID being configured.
    pub entry_id: String,
    /// Recipe `game_id`.
    pub game_id: String,
    /// Game title.
    pub title: String,
    /// Release year shown in the modal header.
    pub release_year: u16,
    /// Matched `release_id`.
    pub release_id: String,
    /// Human-readable release edition label.
    pub release_label: String,
    /// Local installation root path.
    pub install_dir: PathBuf,
    /// Full declarative compatibility recipe.
    pub recipe: GameRecipe,
    /// Fixes already applied on disk before opening this dialog.
    pub already_applied_fix_ids: Vec<String>,
    /// Per-fix checkbox selections keyed by `FixOption::id`.
    pub selected_fixes: BTreeMap<String, bool>,
    /// Cached dry-run `plan_fixes` previews keyed by `FixOption::id`.
    pub fix_plans: BTreeMap<String, Result<PatchPlan, String>>,
    /// Current modal phase (`Review`, `Applying`, `Completed`, `Failed`).
    pub phase: AdvisorPhase,
    /// Total active steps scheduled across selected fixes during `Applying`.
    pub total_steps: usize,
    /// Steps completed so far during `Applying`.
    pub completed_steps: usize,
    /// Status text displayed above the progress bar.
    pub current_label: String,
    /// Current HTTPS download progress (`0.0..=1.0`) when `total_bytes` is known.
    pub download_fraction: Option<f32>,
    /// Formatted download byte counter shown alongside the progress bar.
    pub download_detail: Option<String>,
    /// Transparent chronological log stream built from [`PatchEvent`].
    pub console_logs: Vec<ConsoleLogLine>,
}

impl PatchAdvisorState {
    /// Builds a [`PatchAdvisorState`] for `entry` and eagerly computes read-only `plan_fixes` previews.
    pub fn open(
        entry: &LibraryEntry,
        recipe: &GameRecipe,
        ctx: &PatchContext,
        registry: &dyn RegistryBackend,
    ) -> Self {
        let mut selected_fixes = BTreeMap::new();
        let mut fix_plans = BTreeMap::new();
        let has_existing_applied = !entry.applied_fix_ids.is_empty();

        for fix in recipe.fixes_for_release(&entry.release_id) {
            let checked = if has_existing_applied {
                entry.applied_fix_ids.iter().any(|id| id == &fix.id)
            } else {
                fix.recommended
            };
            selected_fixes.insert(fix.id.clone(), checked);

            let plan_res = plan_fixes(
                recipe,
                &entry.release_id,
                &entry.install_dir,
                std::slice::from_ref(&fix.id),
                ctx,
                registry,
            )
            .map_err(|err| err.to_string());
            fix_plans.insert(fix.id.clone(), plan_res);
        }

        Self {
            entry_id: entry.entry_id.clone(),
            game_id: entry.game_id.clone(),
            title: recipe.title.clone(),
            release_year: recipe.release_year,
            release_id: entry.release_id.clone(),
            release_label: entry.release_label.clone(),
            install_dir: entry.install_dir.clone(),
            recipe: recipe.clone(),
            already_applied_fix_ids: entry.applied_fix_ids.clone(),
            selected_fixes,
            fix_plans,
            phase: AdvisorPhase::Review,
            total_steps: 0,
            completed_steps: 0,
            current_label: String::new(),
            download_fraction: None,
            download_detail: None,
            console_logs: Vec::new(),
        }
    }

    /// Returns ordered `FixOption::id`s currently checked by the user.
    pub fn selected_fix_ids(&self) -> Vec<String> {
        self.recipe
            .fixes_for_release(&self.release_id)
            .into_iter()
            .filter(|fix| self.selected_fixes.get(&fix.id).copied().unwrap_or(false))
            .map(|fix| fix.id.clone())
            .collect()
    }

    /// Returns all release-applicable `FixOption::id`s NOT currently checked by the user.
    pub fn declined_fix_ids(&self) -> Vec<String> {
        self.recipe
            .fixes_for_release(&self.release_id)
            .into_iter()
            .filter(|fix| !self.selected_fixes.get(&fix.id).copied().unwrap_or(false))
            .map(|fix| fix.id.clone())
            .collect()
    }

    /// Switches the modal into live execution mode before spawning `spawn_apply_fixes`.
    pub fn begin_applying(&mut self, selected_ids: &[String]) {
        let active_total: usize = selected_ids
            .iter()
            .filter_map(|id| self.fix_plans.get(id).and_then(|res| res.as_ref().ok()))
            .flat_map(|plan| plan.fixes.iter())
            .map(|f| f.active_step_count)
            .sum();

        self.phase = AdvisorPhase::Applying;
        self.total_steps = active_total.max(1);
        self.completed_steps = 0;
        "Initializing transactional backup...".clone_into(&mut self.current_label);
        self.download_fraction = None;
        self.download_detail = None;
        self.console_logs.clear();
        self.console_logs.push(ConsoleLogLine {
            tag: "INIT",
            text: format!("Starting {} selected fix group(s)...", selected_ids.len()),
            level: LogLevel::Info,
        });
    }

    /// Incorporates a streamed [`PatchEvent`] into the progress bar and transparent console log.
    pub fn on_patch_event(&mut self, event: &PatchEvent) {
        match event {
            PatchEvent::StepStarted { step_name, .. } => {
                self.current_label.clone_from(step_name);
                self.download_fraction = None;
                self.download_detail = None;
            }
            PatchEvent::FixStarted { title, .. } => {
                self.current_label = format!("Applying {title}...");
            }
            PatchEvent::DownloadStarted { step_id, .. } => {
                self.current_label = format!("Fetching `{step_id}`...");
            }
            PatchEvent::DownloadProgress { bytes_downloaded, total_bytes, .. } => {
                self.download_fraction = bytes_ratio(*bytes_downloaded, *total_bytes);
                self.download_detail = Some(format_bytes_progress(*bytes_downloaded, *total_bytes));
            }
            PatchEvent::DownloadVerified { bytes, .. } => {
                self.download_fraction = Some(1.0);
                self.download_detail = Some(format!("{bytes} bytes verified"));
            }
            PatchEvent::StepCompleted { .. } => {
                self.completed_steps = (self.completed_steps + 1).min(self.total_steps);
                self.download_fraction = None;
                self.download_detail = None;
            }
            _ => {}
        }

        if let Some(line) = format_event_log_line(event) {
            self.console_logs.push(line);
        }
    }

    /// Marks execution finished cleanly or failed with automatic rollback notice.
    pub fn finish(&mut self, result: Result<ApplyReport, String>) {
        self.download_fraction = None;
        self.download_detail = None;
        match result {
            Ok(report) => {
                self.completed_steps = self.total_steps;
                self.current_label =
                    format!("Applied {} fix group(s) cleanly.", report.applied_fix_ids.len());
                self.console_logs.push(ConsoleLogLine {
                    tag: "READY",
                    text: format!(
                        "All fixes committed ({} applied, {} skipped).",
                        report.applied_fix_ids.len(),
                        report.skipped_steps.len()
                    ),
                    level: LogLevel::Success,
                });
                self.phase = AdvisorPhase::Completed { applied_fix_ids: report.applied_fix_ids };
            }
            Err(error) => {
                "Patch failed — partial edits rolled back.".clone_into(&mut self.current_label);
                self.console_logs.push(ConsoleLogLine {
                    tag: "ERROR",
                    text: error.clone(),
                    level: LogLevel::Error,
                });
                self.console_logs.push(ConsoleLogLine {
                    tag: "ROLLBACK",
                    text: "Any partial filesystem/registry changes from the failing fix were reverted cleanly.".to_owned(),
                    level: LogLevel::Warning,
                });
                self.phase = AdvisorPhase::Failed { error };
            }
        }
    }

    /// Computes overall execution progress in `0.0..=1.0`.
    pub fn progress_fraction(&self) -> f32 {
        match self.phase {
            AdvisorPhase::Review => 0.0,
            AdvisorPhase::Completed { .. } => 1.0,
            AdvisorPhase::Failed { .. } | AdvisorPhase::Applying => {
                let done = f32::from(u16::try_from(self.completed_steps).unwrap_or(u16::MAX));
                let total = f32::from(u16::try_from(self.total_steps.max(1)).unwrap_or(u16::MAX));
                let base = done / total;
                let sub = self.download_fraction.map_or(0.0, |frac| frac / total);
                (base + sub).clamp(0.0, 1.0)
            }
        }
    }
}

fn bytes_ratio(downloaded: u64, total: Option<u64>) -> Option<f32> {
    let total = total?;
    if total == 0 {
        return None;
    }
    let per_mille =
        u16::try_from((downloaded.saturating_mul(1000) / total).min(1000)).unwrap_or(1000);
    Some(f32::from(per_mille) / 1000.0_f32)
}

fn format_event_log_line(event: &PatchEvent) -> Option<ConsoleLogLine> {
    let (tag, text, level) = match event {
        PatchEvent::FixStarted { fix_id, title, total_steps } => (
            "FIX",
            format!("Started `{fix_id}`: {title} ({total_steps} active step(s))"),
            LogLevel::Info,
        ),
        PatchEvent::StepStarted { step_id, step_name, .. } => {
            ("STEP", format!("[{step_id}] {step_name}"), LogLevel::Info)
        }
        PatchEvent::DownloadStarted { step_id, url, from_cache } => {
            let note = if *from_cache { "cached locally" } else { "downloading over HTTPS" };
            ("HTTPS", format!("[{step_id}] {url} ({note})"), LogLevel::Info)
        }
        PatchEvent::DownloadVerified { step_id, sha256, bytes } => (
            "SHA256",
            format!("[{step_id}] verified {bytes} bytes ({})", &sha256[..12.min(sha256.len())]),
            LogLevel::Success,
        ),
        PatchEvent::FileBackedUp { relative_path, .. } => (
            "BACKUP",
            format!("Snapshot saved -> `.ogsc_backup` for `{relative_path}`"),
            LogLevel::Info,
        ),
        PatchEvent::FileWritten { relative_path, .. } => {
            ("FILE", format!("Wrote `{relative_path}`"), LogLevel::Success)
        }
        PatchEvent::WrapperDeployed { wrapper, files, .. } => {
            ("WRAPPER", format!("Deployed `{wrapper}`: {}", files.join(", ")), LogLevel::Success)
        }
        PatchEvent::RegistryUpdated { key_path, value_name, new_value, .. } => {
            ("REG", format!("{key_path}\\{value_name} = `{new_value}`"), LogLevel::Success)
        }
        PatchEvent::CpuAffinityConfigured { summary, .. } => {
            ("LAUNCH", summary.clone(), LogLevel::Info)
        }
        PatchEvent::StepSkipped { step_id, reason, .. } => {
            ("SKIP", format!("[{step_id}] {reason}"), LogLevel::Warning)
        }
        PatchEvent::FixCompleted { fix_id } => {
            ("DONE", format!("Completed fix group `{fix_id}`"), LogLevel::Success)
        }
        PatchEvent::Warning { message } => ("WARN", message.clone(), LogLevel::Warning),
        PatchEvent::DownloadProgress { .. } | PatchEvent::StepCompleted { .. } => return None,
    };
    Some(ConsoleLogLine { tag, text, level })
}

/// Renders the Patch Advisor modal dialog and returns any user-triggered [`AdvisorAction`].
pub fn render_patch_advisor_modal(
    ctx: &egui::Context,
    state: &mut PatchAdvisorState,
) -> Option<AdvisorAction> {
    let mut action = None;
    let max_width = (ctx.screen_rect().width() - 48.0).clamp(480.0, 820.0);
    let max_height = (ctx.screen_rect().height() - 56.0).clamp(380.0, 680.0);

    let frame = Frame::new()
        .fill(BG_SURFACE)
        .stroke(Stroke::new(1.5_f32, BORDER_SUBTLE))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(16));

    egui::Modal::new(egui::Id::new("patch_advisor_modal")).frame(frame).show(ctx, |ui| {
        ui.set_min_width(max_width.min(680.0));
        ui.set_max_width(max_width);
        ui.set_max_height(max_height);

        render_modal_header(ui, state);
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(6.0);

        match state.phase.clone() {
            AdvisorPhase::Review => {
                action = render_review_phase(ui, state, max_height - 130.0);
            }
            AdvisorPhase::Applying
            | AdvisorPhase::Completed { .. }
            | AdvisorPhase::Failed { .. } => {
                action = render_execution_phase(ui, state, max_height - 140.0);
            }
        }
    });

    action
}

fn render_modal_header(ui: &mut Ui, state: &PatchAdvisorState) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!("Patch Advisor — {} ({})", state.title, state.release_year))
                .size(18.0)
                .strong(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (post_lbl, post_col) = health_badge_style(state.recipe.post_fix_health);
            paint_pill(ui, post_lbl, post_col);
            ui.label(RichText::new("->").color(TEXT_MUTED));
            let (base_lbl, base_col) = health_badge_style(state.recipe.baseline_health);
            paint_pill(ui, base_lbl, base_col);
        });
    });
    ui.label(
        RichText::new(format!(
            "Detected: {} ({})   |   Folder: {}",
            state.release_label,
            state.release_id,
            state.install_dir.display()
        ))
        .size(12.0)
        .color(TEXT_MUTED),
    );
    ui.add_space(6.0);
    render_version_requirement_banner(ui, state);
}

fn render_version_requirement_banner(ui: &mut Ui, state: &PatchAdvisorState) {
    let incompatible = state.recipe.is_release_incompatible(&state.release_id);
    let accent = if incompatible { STATUS_BLOCKER } else { ACCENT_AMBER };
    let fill = Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 22);
    let req_text = state.recipe.metadata.required_game_version.clone().unwrap_or_else(|| {
        state
            .recipe
            .releases
            .iter()
            .map(|r| format!("v{} ({})", r.version, r.label))
            .collect::<Vec<_>>()
            .join(", ")
    });

    Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, accent))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let badge = if incompatible { "INCOMPATIBLE VERSION" } else { "REQUIRED VERSION" };
                paint_pill(ui, badge, accent);
                ui.label(RichText::new(format!("Required: {req_text}")).size(12.0).strong());
            });
            if incompatible {
                ui.add_space(2.0);
                ui.label(
                    RichText::new(
                        "Warning: Your detected release is marked incompatible with this recipe's core fix.",
                    )
                    .size(11.5)
                    .color(STATUS_BLOCKER),
                );
            }
        });
}

fn render_review_phase(
    ui: &mut Ui,
    state: &mut PatchAdvisorState,
    body_height: f32,
) -> Option<AdvisorAction> {
    ScrollArea::vertical().max_height(body_height.max(220.0)).auto_shrink([false, true]).show(
        ui,
        |ui| {
            render_known_issues_section(ui, state);
            ui.add_space(10.0);
            render_selectable_fixes_section(ui, state);
        },
    );

    ui.add_space(8.0);
    ui.separator();
    ui.add_space(6.0);

    let selected = state.selected_fix_ids();
    let declined = state.declined_fix_ids();
    let mut action = None;

    ui.horizontal(|ui| {
        let apply_enabled = !selected.is_empty();
        ui.add_enabled_ui(apply_enabled, |ui| {
            let label = format!("Apply Selected Fixes ({})", selected.len());
            if primary_button(ui, &label).clicked() {
                action = Some(AdvisorAction::ApplySelected(selected.clone()));
            }
        });

        if ui.button(RichText::new("Decline / Keep Original").color(TEXT_PRIMARY)).clicked() {
            let all_declined: Vec<String> = state
                .recipe
                .fixes_for_release(&state.release_id)
                .into_iter()
                .map(|f| f.id.clone())
                .collect();
            action = Some(AdvisorAction::DeclineVanilla(if all_declined.is_empty() {
                declined
            } else {
                all_declined
            }));
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Decide Later").clicked() {
                action = Some(AdvisorAction::Close);
            }
        });
    });

    action
}

fn render_known_issues_section(ui: &mut Ui, state: &PatchAdvisorState) {
    ui.label(
        RichText::new(format!(
            "Diagnosis — Known Windows 10/11 Issues ({})",
            state.recipe.known_issues.len()
        ))
        .size(14.0)
        .strong()
        .color(ACCENT_AMBER),
    );
    ui.add_space(4.0);

    if state.recipe.known_issues.is_empty() {
        ui.label(
            RichText::new("No baseline defects cataloged for this release.").color(TEXT_MUTED),
        );
        return;
    }

    for issue in &state.recipe.known_issues {
        Frame::new()
            .fill(BG_ELEVATED)
            .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(Margin::same(10))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let (sev_lbl, sev_col) = severity_badge_style(issue.severity);
                    paint_pill(ui, sev_lbl, sev_col);
                    ui.label(RichText::new(&issue.summary).strong());
                });
                ui.add_space(2.0);
                ui.label(RichText::new(&issue.details).size(12.0).color(TEXT_MUTED));
                if let Some(user_note) = &issue.user_action_required {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!("Action required (BYOG): {user_note}"))
                            .size(12.0)
                            .color(ACCENT_AMBER),
                    );
                }
            });
        ui.add_space(4.0);
    }
}

fn render_selectable_fixes_section(ui: &mut Ui, state: &mut PatchAdvisorState) {
    let release_fixes: Vec<FixOption> =
        state.recipe.fixes_for_release(&state.release_id).into_iter().cloned().collect();

    ui.label(
        RichText::new(format!("Available Fixes ({})", release_fixes.len()))
            .size(14.0)
            .strong()
            .color(ACCENT_CYAN),
    );
    ui.add_space(4.0);

    for fix in release_fixes {
        let is_applied = state.already_applied_fix_ids.iter().any(|id| id == &fix.id);
        let checked = state.selected_fixes.entry(fix.id.clone()).or_insert(fix.recommended);

        Frame::new()
            .fill(BG_ELEVATED)
            .stroke(Stroke::new(1.0_f32, if *checked { ACCENT_AMBER } else { BORDER_SUBTLE }))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(Margin::same(10))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(checked, RichText::new(&fix.title).strong().size(13.5));
                    if fix.recommended {
                        paint_pill(ui, "Recommended", STATUS_PATCHED);
                    } else {
                        paint_pill(ui, "Optional", ACCENT_CYAN);
                    }
                    if is_applied {
                        paint_pill(ui, "Applied", ACCENT_AMBER);
                    }
                });
                ui.label(RichText::new(&fix.description).size(12.0).color(TEXT_MUTED));

                ui.add_space(4.0);
                let preview_id = ui.make_persistent_id(format!("preview_{}", fix.id));
                egui::CollapsingHeader::new(RichText::new("Preview Changes (dry-run)").size(12.0))
                    .id_salt(preview_id)
                    .show(ui, |ui| {
                        render_fix_plan_preview(ui, state.fix_plans.get(&fix.id));
                    });
            });
        ui.add_space(5.0);
    }
}

fn render_wrapper_preview_list(ui: &mut Ui, wrappers: &[ogsc_patcher::PlannedWrapperSummary]) {
    ui.label(RichText::new("Compatibility Wrappers:").size(12.0).strong());
    for wr in wrappers {
        ui.label(
            RichText::new(format!(
                "  • `{}` -> `{}` ({} DLL(s), {} config(s))",
                wr.wrapper_name, wr.target_dir, wr.dll_count, wr.config_count
            ))
            .size(11.5)
            .color(TEXT_MUTED),
        );
        if !wr.missing_toolkit_assets.is_empty() {
            if wr.auto_download_available {
                ui.label(
                    RichText::new(format!(
                        "    Will auto-download verified `{}` toolkit into runtime_wrappers on Apply",
                        wr.wrapper_name
                    ))
                    .size(11.5)
                    .color(ACCENT_CYAN),
                );
            } else {
                ui.label(
                    RichText::new(format!(
                        "    Missing local toolkit assets ({}) — check runtime_wrappers in Settings",
                        wr.missing_toolkit_assets.len()
                    ))
                    .size(11.5)
                    .color(STATUS_BLOCKER),
                );
            }
        }
    }
}

fn render_fix_plan_preview(ui: &mut Ui, plan_res: Option<&Result<PatchPlan, String>>) {
    match plan_res {
        None => {
            ui.label(RichText::new("Preview unavailable.").color(TEXT_MUTED));
        }
        Some(Err(err)) => {
            ui.label(RichText::new(format!("Plan error: {err}")).color(STATUS_BLOCKER));
        }
        Some(Ok(plan)) => {
            if !plan.downloads.is_empty() {
                ui.label(RichText::new("Verified HTTPS Downloads:").size(12.0).strong());
                for dl in &plan.downloads {
                    let cache_note =
                        if dl.cached_locally { "cached locally" } else { "download needed" };
                    ui.label(
                        RichText::new(format!("  • {} ({cache_note})", dl.url))
                            .size(11.5)
                            .monospace()
                            .color(TEXT_MUTED),
                    );
                }
            }

            if !plan.wrappers.is_empty() {
                render_wrapper_preview_list(ui, &plan.wrappers);
            }

            if !plan.file_changes.is_empty() {
                ui.label(RichText::new("Files & Directories:").size(12.0).strong());
                for ch in &plan.file_changes {
                    let backup_tag = if ch.already_exists { "backup + update" } else { "create" };
                    ui.label(
                        RichText::new(format!(
                            "  • `{}` ({backup_tag}) — {}",
                            ch.relative_path, ch.detail
                        ))
                        .size(11.5)
                        .monospace()
                        .color(TEXT_MUTED),
                    );
                }
            }

            if !plan.registry_changes.is_empty() {
                ui.label(RichText::new("Windows Registry Tweaks:").size(12.0).strong());
                for reg in &plan.registry_changes {
                    ui.label(
                        RichText::new(format!(
                            "  • {:?}\\{}\\{} -> `{}` (currently {:?})",
                            reg.hive,
                            reg.key_path,
                            reg.value_name,
                            reg.planned_value,
                            reg.current_value
                        ))
                        .size(11.5)
                        .monospace()
                        .color(TEXT_MUTED),
                    );
                }
            }

            if !plan.launch_notes.is_empty() {
                ui.label(RichText::new("Launch Policy:").size(12.0).strong());
                for note in &plan.launch_notes {
                    ui.label(RichText::new(format!("  • {note}")).size(11.5).color(TEXT_MUTED));
                }
            }

            for warn in &plan.warnings {
                ui.label(
                    RichText::new(format!("  [WARN] {warn}")).size(11.5).color(STATUS_NEEDS_FIXES),
                );
            }
        }
    }
}

fn render_execution_phase(
    ui: &mut Ui,
    state: &mut PatchAdvisorState,
    console_height: f32,
) -> Option<AdvisorAction> {
    let frac = state.progress_fraction();
    ui.horizontal(|ui| {
        ui.label(RichText::new(&state.current_label).strong());
        if let Some(detail) = &state.download_detail {
            ui.label(RichText::new(format!("({detail})")).size(12.0).color(ACCENT_CYAN));
        }
    });
    ui.add_space(4.0);

    let bar = egui::ProgressBar::new(frac)
        .show_percentage()
        .animate(matches!(state.phase, AdvisorPhase::Applying));
    ui.add(bar);
    ui.add_space(8.0);

    Frame::new()
        .fill(BG_CONSOLE)
        .stroke(Stroke::new(1.0_f32, BORDER_SUBTLE))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ScrollArea::vertical()
                .max_height((console_height - 90.0).max(180.0))
                .min_scrolled_height(180.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &state.console_logs {
                        let color = match line.level {
                            LogLevel::Info => TEXT_PRIMARY,
                            LogLevel::Success => STATUS_PATCHED,
                            LogLevel::Warning => STATUS_NEEDS_FIXES,
                            LogLevel::Error => STATUS_BLOCKER,
                        };
                        ui.label(
                            RichText::new(format!("[{:7}] {}", line.tag, line.text))
                                .size(11.5)
                                .monospace()
                                .color(color),
                        );
                    }
                });
        });

    ui.add_space(8.0);
    ui.separator();
    ui.add_space(6.0);

    let mut action = None;
    match &state.phase {
        AdvisorPhase::Applying => {
            ui.label(
                RichText::new("Applying fixes transactionally — do not close...").color(TEXT_MUTED),
            );
        }
        AdvisorPhase::Completed { .. } => {
            ui.horizontal(|ui| {
                let post_health_ready = state.recipe.post_fix_health == HealthStatus::ReadyToPlay;
                let btn_label = if post_health_ready { "Launch Game Now" } else { "Launch Game" };
                if primary_button(ui, btn_label).clicked() {
                    action = Some(AdvisorAction::LaunchAfterPatch);
                }
                if ui.button("Done").clicked() {
                    action = Some(AdvisorAction::Close);
                }
            });
        }
        AdvisorPhase::Failed { .. } => {
            ui.horizontal(|ui| {
                if ui.button("Back to Fix Selection").clicked() {
                    state.phase = AdvisorPhase::Review;
                }
                if ui.button("Close").clicked() {
                    action = Some(AdvisorAction::Close);
                }
            });
        }
        AdvisorPhase::Review => {}
    }

    action
}

fn format_bytes_progress(downloaded: u64, total: Option<u64>) -> String {
    match total {
        Some(t) if t > 0 => format!("{} / {} KiB", downloaded / 1024, t / 1024),
        _ => format!("{} KiB", downloaded / 1024),
    }
}

#[cfg(test)]
mod tests {
    use ogsc_patcher::{MemoryRegistryBackend, RegistryBackend, APP_COMPAT_LAYERS_KEY};
    use ogsc_recipes::{RecipeDatabase, RegistryHive};
    use ogsc_scanner::{ComputedHashes, GameStatus, LibraryEntry, MatchConfidence};
    use ogsc_wrappers::ToolkitStore;

    use super::*;

    #[test]
    fn opens_advisor_computes_dry_run_plans_and_streams_patch_events() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let game_root = temp.path().join("NFS_MW");
        std::fs::create_dir_all(&game_root).expect("mkdir");

        let db =
            RecipeDatabase::load_from_dir(PathBuf::from("../../recipes/games").as_path(), false)
                .expect("load bundled recipes")
                .db;
        let recipe = db.get_by_id("nfs-most-wanted-2005").expect("nfs recipe");

        let entry = LibraryEntry {
            entry_id: "nfs-most-wanted-2005--abc".to_owned(),
            game_id: recipe.game_id.clone(),
            title: recipe.title.clone(),
            release_id: recipe.releases[0].release_id.clone(),
            release_label: recipe.releases[0].label.clone(),
            install_dir: game_root.clone(),
            executable_path: game_root.join("speed.exe"),
            match_confidence: MatchConfidence::ExactSha256,
            detected_sha256: Some("a".repeat(64)),
            status: GameStatus::NeedsFixes,
            applied_fix_ids: Vec::new(),
            declined_fix_ids: Vec::new(),
            custom_cover_art_path: None,
            imported_at: "2026-11-22T00:00:00Z".to_owned(),
            last_scanned_at: "2026-11-22T00:00:00Z".to_owned(),
            last_launched_at: None,
        };

        let ctx = PatchContext::for_game(
            &game_root,
            ToolkitStore::new(temp.path().join("toolkits")),
            temp.path().join("cache"),
            Some(".ogsc_sandbox"),
        );
        let mut reg = MemoryRegistryBackend::new();
        let exe_key = game_root.join("speed.exe").to_string_lossy().into_owned();
        reg.write_string(RegistryHive::Hkcu, APP_COMPAT_LAYERS_KEY, &exe_key, "~ RUNASADMIN")
            .expect("seed reg");

        let mut advisor = PatchAdvisorState::open(&entry, recipe, &ctx, &reg);
        let selected = advisor.selected_fix_ids();
        assert!(selected.contains(&"widescreen_fix".to_owned()));
        assert!(selected.contains(&"win11_stability_tuning".to_owned()));

        // Verify dry-run preview for win11_stability_tuning captured merged HKCU key
        let win11_plan = advisor
            .fix_plans
            .get("win11_stability_tuning")
            .expect("win11 plan key")
            .as_ref()
            .expect("win11 plan ok");
        assert_eq!(win11_plan.registry_changes.len(), 1);
        assert!(win11_plan.registry_changes[0].planned_value.contains("HIGHDPIAWARE"));

        // Stream live PatchEvents through progress & console log
        advisor.begin_applying(&["win11_stability_tuning".to_owned()]);
        assert_eq!(advisor.phase, AdvisorPhase::Applying);

        advisor.on_patch_event(&PatchEvent::FixStarted {
            fix_id: "win11_stability_tuning".to_owned(),
            title: "Win11 Tuning".to_owned(),
            total_steps: 2,
        });
        advisor.on_patch_event(&PatchEvent::StepStarted {
            fix_id: "win11_stability_tuning".to_owned(),
            step_id: "set-compat".to_owned(),
            step_name: "Set HKCU Layers".to_owned(),
        });
        advisor.on_patch_event(&PatchEvent::RegistryUpdated {
            fix_id: "win11_stability_tuning".to_owned(),
            key_path: APP_COMPAT_LAYERS_KEY.to_owned(),
            value_name: exe_key,
            new_value: "~ RUNASADMIN HIGHDPIAWARE".to_owned(),
        });
        advisor.on_patch_event(&PatchEvent::StepCompleted {
            fix_id: "win11_stability_tuning".to_owned(),
            step_id: "set-compat".to_owned(),
        });
        assert!(advisor.progress_fraction() >= 0.49);

        advisor.finish(Ok(ApplyReport {
            applied_fix_ids: vec!["win11_stability_tuning".to_owned()],
            skipped_steps: Vec::new(),
            warnings: Vec::new(),
        }));
        assert!(matches!(advisor.phase, AdvisorPhase::Completed { .. }));
        assert!((advisor.progress_fraction() - 1.0).abs() < f32::EPSILON);
        assert!(advisor.console_logs.iter().any(|line| line.tag == "REG"));
        let _ = std::mem::size_of::<ComputedHashes>();
    }
}
