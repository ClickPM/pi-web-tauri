//! Native theme sync — keeps the OS window chrome in step with the theme the
//! user picks INSIDE pi-web.
//!
//! The window has a normal Windows-drawn title bar, and pi-web's own light/dark
//! toggle only touches the page: on a light-themed Windows the title bar stayed
//! white above a dark pi-web UI. The injected script watches the `dark` class on
//! `<html>` (which is what pi-web toggles, persisted in `localStorage["pi-theme"]`)
//! and reports it; this module drives the window theme from that.
//!
//! The persisted file keeps the EXACT format the Electron shell wrote —
//! `theme-state.json` holding `{"theme":"dark"}` in `%APPDATA%\Pi` — so an
//! upgrading user's window opens in the right colour on the very first frame
//! instead of flashing the OS theme for the second it takes pi-web to load.

use std::path::{Path, PathBuf};

use tauri::Theme;

use crate::log_dbg;

const STATE_FILE: &str = "theme-state.json";

fn state_file(user_data: &Path) -> PathBuf {
    user_data.join(STATE_FILE)
}

fn normalize(theme: &str) -> Option<Theme> {
    match theme {
        "dark" => Some(Theme::Dark),
        "light" => Some(Theme::Light),
        _ => None,
    }
}

fn name(theme: Theme) -> &'static str {
    match theme {
        Theme::Dark => "dark",
        _ => "light",
    }
}

/// The persisted theme, or None when absent (first run) or unreadable.
pub fn read_persisted(user_data: &Path) -> Option<Theme> {
    // We always write BOM-less UTF-8, but this file is small enough to be hand
    // edited and every Windows PowerShell text cmdlet adds a BOM — which a JSON
    // parser rejects outright. Strip it rather than silently reverting to the
    // OS theme.
    let text = std::fs::read_to_string(state_file(user_data)).ok()?;
    let value: serde_json::Value =
        serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    normalize(value.get("theme")?.as_str()?)
}

fn persist(user_data: &Path, theme: Theme) {
    let body = format!("{{\n  \"theme\": \"{}\"\n}}\n", name(theme));
    // Best-effort: a failed write only costs the correct frame on next boot.
    let _ = std::fs::write(state_file(user_data), body);
}

/// Window background per theme — pi-web's own `--bg` (globals.css `:root` /
/// `.dark`). Only shows in the areas the webview exposes before the page paints
/// (mainly during a resize), so it must never be the opposite colour.
const BG_DARK: tauri::window::Color = tauri::window::Color(0x1A, 0x1A, 0x1A, 0xFF);
const BG_LIGHT: tauri::window::Color = tauri::window::Color(0xFF, 0xFF, 0xFF, 0xFF);

/// Adopt the theme the page just reported. Returns the applied theme, or None
/// when the page sent something we do not understand.
pub fn set(window: &tauri::WebviewWindow, user_data: &Path, theme: &str) -> Option<Theme> {
    let t = normalize(theme)?;
    if let Err(e) = window.set_theme(Some(t)) {
        // Cosmetic only — never fail the page's fire-and-forget report on it.
        log_dbg!("theme: set_theme({}) failed: {e}", name(t));
    }
    // Repainted even when the theme did not change: the window is created with
    // the dark splash colour regardless, so the first report after a
    // light-theme launch still has to correct it.
    let bg = match t {
        Theme::Dark => BG_DARK,
        _ => BG_LIGHT,
    };
    if let Err(e) = window.set_background_color(Some(bg)) {
        log_dbg!("theme: set_background_color failed: {e}");
    }
    if read_persisted(user_data) != Some(t) {
        persist(user_data, t);
    }
    Some(t)
}
