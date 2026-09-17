//! The complete page -> shell surface. Four commands, and that is the point.
//!
//! In Electron this lived behind `contextIsolation: true`, so only what
//! `contextBridge` exposed was reachable and the page could not forge calls
//! from arbitrary script. Tauri injects into the page's MAIN world, and the
//! page in question renders agent-produced content — so treat every one of
//! these as callable by a hostile string.
//!
//! Hence the shape of the list: three take no arguments at all, the fourth
//! takes one string that is immediately narrowed to `dark | light`. Their worst
//! cases are, in order: a native folder dialog the user has to confirm; a page
//! reload; an npm install of ONE hardcoded package from ONE configured
//! registry; a repainted title bar. Nothing here accepts a path, a URL, a
//! command line, or a package name from the page.

use tauri::{AppHandle, Manager, WebviewWindow};
use tauri_plugin_dialog::DialogExt;

use crate::app::AppState;
use crate::log_dbg;

/// Backend for `window.piDesktop.selectDirectory()` — the desktop bridge
/// pi-web's session sidebar probes for (upstream v0.7.13+, commit 0fa4c43).
///
/// Contract, as declared in pi-web's components/SessionSidebar.tsx:
///   selectDirectory(): Promise<string | null>
/// resolving to the chosen absolute path, or null when the user cancels.
#[tauri::command]
pub async fn select_directory(window: WebviewWindow) -> Result<Option<String>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    log_dbg!("select_directory: opening folder dialog");
    window
        .dialog()
        .file()
        .set_title("选择工作目录")
        .set_parent(&window)
        .pick_folder(move |picked| {
            log_dbg!("select_directory: dialog callback fired ({picked:?})");
            let _ = tx.send(picked);
        });

    // The callback fires on the UI thread; block for it off the UI thread.
    let picked = tauri::async_runtime::spawn_blocking(move || rx.recv().ok().flatten())
        .await
        .map_err(|e| format!("directory picker failed: {e}"))?;

    let result = picked.map(|p| p.to_string());
    log_dbg!("select_directory: returning {result:?}");
    Ok(result)
}

/// Backend for the injected bottom bar's reload button. Same effect as the
/// menu's 重新加载 (Ctrl+R), reachable without unhiding the menu.
///
/// Electron used `reloadIgnoringCache()` here, because after an embedded-server
/// restart the page must not come back from a stale cache. Tauri has no
/// cache-ignoring variant, so this is an ordinary reload — which still
/// revalidates the top-level document (Chromium sends `Cache-Control: max-age=0`
/// for a user reload), and Next.js serves its assets under content-hashed URLs,
/// so the gap is narrow. Recorded in PORTING.md rather than papered over.
#[tauri::command]
pub fn reload_page(window: WebviewWindow) -> Result<(), String> {
    window.reload().map_err(|e| {
        log_dbg!("reload-page error {e}");
        e.to_string()
    })
}

/// pi-web's light/dark toggle -> the native window frame. Fire-and-forget from
/// the page; nothing there depends on the result.
#[tauri::command]
pub fn theme_changed(app: AppHandle, window: WebviewWindow, theme: String) {
    let state = app.state::<AppState>();
    let applied = crate::theme::set(&window, state.paths.user_data_dir(), &theme);
    log_dbg!("theme-changed: page reported {theme:?} -> {applied:?}");
}

/// CTA action: the user clicked "更新并重启" on a deferred-update notice.
/// Parameterless — the package, the version spec and the registry are all
/// fixed in the shell.
#[tauri::command]
pub fn apply_update(app: AppHandle) {
    std::thread::spawn(move || crate::app::apply_update(&app, true));
}
