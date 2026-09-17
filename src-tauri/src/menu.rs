//! Application menu — the same three submenus main.js built.
//!
//! One behavioural difference from Electron: `autoHideMenuBar` has no Tauri
//! equivalent (there is `hide_menu`/`show_menu`, but nothing surfaces the Alt
//! press that would bring it back), so the bar is simply visible. Noted in
//! PORTING.md rather than silently dropped.

use tauri::menu::{Menu, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};
use tauri::{AppHandle, Manager, Runtime};

pub const ID_CHECK_UPDATES: &str = "check-updates";
pub const ID_RELOAD: &str = "reload";
pub const ID_RESTART_SERVER: &str = "restart-server";
pub const ID_DEVTOOLS: &str = "devtools";

pub fn build<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    // `&` marks the Alt mnemonic. Electron's menus had these; muda needs them
    // spelled out. Latin letters rather than the Chinese labels, because that
    // is what Alt+<key> can actually reach.
    let app_menu = SubmenuBuilder::new(app, "&App")
        .item(
            &MenuItemBuilder::with_id(ID_CHECK_UPDATES, "检查更新…").build(app)?,
        )
        .separator()
        .item(
            &MenuItemBuilder::with_id(ID_RELOAD, "重新加载")
                .accelerator("CmdOrCtrl+R")
                .build(app)?,
        )
        .item(&MenuItemBuilder::with_id(ID_RESTART_SERVER, "重启内嵌服务").build(app)?)
        .item(
            &MenuItemBuilder::with_id(ID_DEVTOOLS, "开发者工具")
                .accelerator("CmdOrCtrl+Shift+I")
                .build(app)?,
        )
        .separator()
        .item(&PredefinedMenuItem::quit(app, Some("退出"))?)
        .build()?;

    let edit_menu = SubmenuBuilder::new(app, "编辑(&E)")
        .item(&PredefinedMenuItem::undo(app, Some("撤销"))?)
        .item(&PredefinedMenuItem::redo(app, Some("重做"))?)
        .separator()
        .item(&PredefinedMenuItem::cut(app, Some("剪切"))?)
        .item(&PredefinedMenuItem::copy(app, Some("复制"))?)
        .item(&PredefinedMenuItem::paste(app, Some("粘贴"))?)
        .item(&PredefinedMenuItem::select_all(app, Some("全选"))?)
        .build()?;

    let view_menu = SubmenuBuilder::new(app, "视图(&V)")
        .item(&PredefinedMenuItem::fullscreen(app, Some("全屏"))?)
        .build()?;

    MenuBuilder::new(app)
        .items(&[&app_menu, &edit_menu, &view_menu])
        .build()
}

pub fn handle(app: &AppHandle, id: &str) {
    match id {
        ID_CHECK_UPDATES => {
            let app = app.clone();
            std::thread::spawn(move || crate::app::check_for_updates(&app, true));
        }
        ID_RELOAD => {
            if let Some(win) = crate::app::main_window(app) {
                let _ = win.reload();
            }
        }
        ID_RESTART_SERVER => {
            let app = app.clone();
            std::thread::spawn(move || {
                if let Err(e) = crate::app::start_or_restart_server(&app) {
                    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
                    app.dialog()
                        .message(e)
                        .title("重启失败")
                        .kind(MessageDialogKind::Error)
                        .blocking_show();
                }
            });
        }
        ID_DEVTOOLS => {
            // The `devtools` feature is enabled on the tauri dependency, so
            // these are always present in this build.
            if let Some(win) = app.get_webview_window(crate::app::MAIN_WINDOW) {
                if win.is_devtools_open() {
                    win.close_devtools();
                } else {
                    win.open_devtools();
                }
            }
        }
        _ => {}
    }
}
