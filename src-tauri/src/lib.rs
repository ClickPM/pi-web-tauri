//! Pi — Tauri 2 desktop shell for pi-web.
//!
//! Entry point and wiring only; the interesting parts live in `app` (boot and
//! update state machine), `server` (the embedded pi-web process), `guard` (the
//! runtime-integrity modules we are forbidden to rewrite) and `job` (the kernel
//! guarantee that killing this process kills the node tree).

mod app;
mod commands;
mod guard;
mod job;
pub mod log;
mod menu;
mod migrate;
mod paths;
mod server;
mod theme;

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

use crate::app::AppState;
use crate::paths::Paths;

/// AppUserModelID for the Windows taskbar and shortcuts. Must stay
/// `com.agegr.piwebdesktop` (it is also the bundle identifier) so a new
/// installer upgrades in place rather than landing beside the old app.
const APP_USER_MODEL_ID: &str = "com.agegr.piwebdesktop";

/// Injected into every page at document start — before any page script, which
/// is what `window.piDesktop` requires (pi-web probes for it while the session
/// sidebar mounts).
const PRELOAD: &str = include_str!("../inject/preload.js");

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    log_dbg!("---- shell start (pid {}) ----", std::process::id());

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // Focus the window we already have rather than opening a second one.
            if let Some(win) = app::main_window(app) {
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
        }))
        .invoke_handler(tauri::generate_handler![
            commands::select_directory,
            commands::reload_page,
            commands::theme_changed,
            commands::apply_update,
        ])
        .setup(|tauri_app| {
            let handle = tauri_app.handle().clone();

            let paths = Paths::resolve(&handle);
            // Restore the frame colour BEFORE the window exists: a light-themed
            // pi-web on a dark Windows (or the reverse) would otherwise show the
            // wrong title bar for the seconds the page takes to load and report.
            let restored = theme::read_persisted(paths.user_data_dir());
            log_dbg!(
                "native theme restored: {}",
                restored
                    .map(|t| format!("{t:?}"))
                    .unwrap_or_else(|| "none recorded (following the OS)".into())
            );

            tauri_app.manage(AppState::new(paths));

            let mut window = WebviewWindowBuilder::new(
                tauri_app,
                app::MAIN_WINDOW,
                WebviewUrl::App("index.html".into()),
            )
            .title("Pi")
            .inner_size(1320.0, 880.0)
            .min_inner_size(900.0, 600.0)
            // pi-web's Metro dark canvas (--bg) — avoids a pre-paint flash.
            .background_color(tauri::window::Color(0x0A, 0x0A, 0x0A, 0xFF))
            .initialization_script(PRELOAD)
            .on_page_load(|webview, payload| {
                if matches!(payload.event(), tauri::webview::PageLoadEvent::Finished) {
                    let handle = webview.app_handle();
                    // Backstop for the capture in setup(): the first page to
                    // load IS a bundled one, so its URL is the local base.
                    handle.state::<AppState>().capture_local_base(payload.url());
                    // First load of the pi-web origin: hand the page the session
                    // cookie the shell already obtained, then let it reload.
                    app::inject_session_cookie(handle);
                    // Deliver any update-result CTA queued while the page was
                    // (re)loading — e.g. the "更新完成" notice set right after
                    // an update reloads the server.
                    app::flush_update_notice(handle);
                }
            });
            if let Some(theme) = restored {
                window = window.theme(Some(theme));
            }
            let window = window.build()?;

            // The base every later local navigation is resolved against
            // (http://tauri.localhost/ on Windows). Captured rather than
            // hardcoded because the local scheme differs by platform and by
            // wry version; a miss here is what made the first build show pi-web
            // during an update instead of updating.html, so it is logged either
            // way and `on_page_load` re-captures it as a backstop.
            // At this point the webview has been told where to go but has not
            // been there yet, so this is normally `about:blank` — which is
            // useless as a base and, taken at face value, is what made the
            // first build show pi-web during an update instead of updating.html.
            // Anything that is not a real http(s) URL is ignored and the
            // `on_page_load` backstop below supplies the base instead.
            match window.url() {
                Ok(url) if matches!(url.scheme(), "http" | "https") => {
                    log_dbg!("local base = {url}");
                    tauri_app.state::<AppState>().set_local_base(url);
                }
                Ok(url) => log_dbg!("local base not settled at setup ({url}) — taking it from the first page load"),
                Err(e) => log_dbg!("local base unavailable at setup ({e}) — taking it from the first page load"),
            }

            // Keep the native window/taskbar title as the app name: the embedded
            // pi-web page sets its own <title>, and letting that reach the OS
            // window would make the shell present as something else.
            window.set_title("Pi")?;

            let menu = menu::build(&handle)?;
            tauri_app.set_menu(menu)?;
            tauri_app.on_menu_event(|app, event| {
                menu::handle(app, event.id().as_ref());
            });

            let boot_handle = handle.clone();
            std::thread::spawn(move || app::boot(boot_handle));

            Ok(())
        });

    builder
        .build(tauri::generate_context!())
        .expect("error while building the Pi shell")
        .run(|handle, event| match event {
            tauri::RunEvent::Ready => {
                #[cfg(windows)]
                set_app_user_model_id();
            }
            tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit => {
                let state = handle.state::<AppState>();
                if !state.is_quitting() {
                    state.mark_quitting();
                    // The job object would reap the tree on its own once this
                    // process dies; doing it explicitly frees the port and the
                    // runtime's file handles immediately, which matters when the
                    // user relaunches straight away.
                    app::kill_server(handle);
                }
            }
            _ => {}
        });
}

/// Set via the taskbar-facing API so shortcuts and the running window group
/// under one identity. Tauri already derives this from the bundle identifier
/// for packaged builds; doing it here keeps `tauri dev` consistent too.
#[cfg(windows)]
fn set_app_user_model_id() {
    // Tauri exposes no safe binding for this, and calling SetCurrentProcess-
    // ExplicitAppUserModelID directly would need the raw-FFI escape hatch this
    // project bans. The identifier is already applied by the NSIS shortcut
    // (AppUserModelID property) and by Tauri's own bundle identity, so the only
    // gap is `tauri dev`. Recorded in PORTING.md as an accepted, cosmetic-only
    // shortfall.
    log_dbg!("AppUserModelID: relying on bundle identity ({APP_USER_MODEL_ID})");
}
