//! Native `egui / eframe` desktop GUI launcher, SteamGridDB cover fetcher, GitHub recipe sync,
//! and consent-driven Patch Advisor modal for **ReviveBox** (*Old Games Second Chance*).

#![forbid(unsafe_code)]

pub mod advisor;
pub mod covers;
pub mod debug_console;
pub mod recipe_sync;
pub mod settings;

mod app;
mod details;
mod settings_modal;
mod theme;
mod worker;

use app::OgscApp;
use egui::Vec2;

const APP_LOGO_PNG: &[u8] = include_bytes!("../assets/revivebox_logo.png");

pub(crate) fn decode_app_logo_rgba() -> Option<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory_with_format(APP_LOGO_PNG, image::ImageFormat::Png).ok()?;
    let rgba = img.into_rgba8();
    let (width, height) = rgba.dimensions();
    Some((rgba.into_raw(), width, height))
}

/// Launches the native **ReviveBox — Old Games Second Chance** desktop window via `eframe`.
pub fn run_desktop_app() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("ReviveBox — Old Games Second Chance")
        .with_inner_size(Vec2::new(1180.0, 760.0))
        .with_min_inner_size(Vec2::new(820.0, 560.0));
    if let Some((rgba, width, height)) = decode_app_logo_rgba() {
        viewport = viewport.with_icon(egui::IconData { rgba, width, height });
    }

    let options = eframe::NativeOptions { viewport, ..Default::default() };

    eframe::run_native(
        "ReviveBox — Old Games Second Chance",
        options,
        Box::new(|cc| Ok(Box::new(OgscApp::new(cc)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_revivebox_logo_decodes_cleanly() {
        let (rgba, width, height) = decode_app_logo_rgba().expect("bundled logo should decode");
        assert_eq!(width, 256);
        assert_eq!(height, 256);
        assert_eq!(rgba.len(), 256 * 256 * 4);
    }
}
