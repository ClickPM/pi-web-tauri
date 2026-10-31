//! Application menu — main.js's three submenus, the 主机 switcher, and the
//! reload button.
//!
//! The bar is always visible. Electron's `autoHideMenuBar` is not reproduced;
//! PORTING.md records why the reveal-on-Alt version was withdrawn.
//!
//! 刷新 is a TOP-LEVEL item rather than a submenu entry: it is a button, not a
//! menu, and it replaces the floating chip the injected script used to draw in
//! a 30px strip along the bottom of the page.
//!
//! 主机(&H) lists 「Windows 本地」 plus every host in `hosts.json`, as
//! `CheckMenuItem`s used like radio buttons: `sync()` re-ticks them from
//! `AppState::target()` after every switch (muda toggles a check item on click
//! by itself, so a click on the already-active host would otherwise un-tick it).
//! Reloading hosts.json rebuilds the whole menu — simpler and more robust than
//! patching a submenu in place.

use tauri::menu::{
    CheckMenuItem, CheckMenuItemBuilder, Menu, MenuBuilder, MenuItem, MenuItemBuilder,
    PredefinedMenuItem, SubmenuBuilder,
};
use tauri::{AppHandle, Manager};

use crate::app::AppState;
use crate::hosts::{LOCAL_ID, LOCAL_LABEL};
use crate::log_dbg;

pub const ID_CHECK_UPDATES: &str = "check-updates";
pub const ID_RELOAD: &str = "reload";
/// The menu-bar reload button. A separate id from ID_RELOAD because two menu
/// items may not share one; both route to the same action.
pub const ID_RELOAD_BUTTON: &str = "reload-button";
pub const ID_RESTART_SERVER: &str = "restart-server";
pub const ID_DEVTOOLS: &str = "devtools";
pub const ID_RELOAD_HOSTS: &str = "hosts-reload";
pub const ID_OPEN_HOSTS_DIR: &str = "hosts-open-dir";
/// Host items are `host:<id>`; `host:local` is Windows 本地.
const HOST_PREFIX: &str = "host:";

const LABEL_RESTART_LOCAL: &str = "重启内嵌服务";
const LABEL_RESTART_REMOTE: &str = "重连当前远端";

/// The items `sync()` has to touch after a switch. Cloned out of the mutex
/// before use: every setter round-trips through the main thread, and holding
/// the lock across that would deadlock against a menu click handled there.
#[derive(Clone)]
pub struct MenuHandles {
    check_updates: MenuItem<tauri::Wry>,
    restart: MenuItem<tauri::Wry>,
    /// (host id, item), Windows 本地 first.
    hosts: Vec<(String, CheckMenuItem<tauri::Wry>)>,
}

pub fn build(app: &AppHandle) -> tauri::Result<(Menu<tauri::Wry>, MenuHandles)> {
    let state = app.state::<AppState>();
    let hosts = state.hosts();
    let target = state.target();

    // `&` marks the Alt mnemonic. Electron's menus had these; muda needs them
    // spelled out. Latin letters rather than the Chinese labels, because that
    // is what Alt+<key> can actually reach.
    let check_updates = MenuItemBuilder::with_id(ID_CHECK_UPDATES, "检查更新…").build(app)?;
    let restart = MenuItemBuilder::with_id(ID_RESTART_SERVER, LABEL_RESTART_LOCAL).build(app)?;
    let app_menu = SubmenuBuilder::new(app, "&App")
        .item(&check_updates)
        .separator()
        .item(
            &MenuItemBuilder::with_id(ID_RELOAD, "重新加载")
                .accelerator("CmdOrCtrl+R")
                .build(app)?,
        )
        .item(&restart)
        .item(
            &MenuItemBuilder::with_id(ID_DEVTOOLS, "开发者工具")
                .accelerator("CmdOrCtrl+Shift+I")
                .build(app)?,
        )
        .separator()
        .item(&PredefinedMenuItem::quit(app, Some("退出"))?)
        .build()?;

    // --- 主机(&H) --------------------------------------------------------------
    let mut host_items: Vec<(String, CheckMenuItem<tauri::Wry>)> = Vec::new();
    let entries = std::iter::once((LOCAL_ID.to_string(), LOCAL_LABEL.to_string()))
        .chain(hosts.hosts.iter().map(|h| (h.id.clone(), h.label.clone())));
    for (index, (id, label)) in entries.enumerate() {
        let mut builder = CheckMenuItemBuilder::with_id(format!("{HOST_PREFIX}{id}"), &label)
            .checked(id == target);
        // Ctrl+Alt+1..9; hosts beyond the ninth are still clickable.
        if index < 9 {
            builder = builder.accelerator(format!("CmdOrCtrl+Alt+{}", index + 1));
        }
        host_items.push((id, builder.build(app)?));
    }
    let mut host_menu = SubmenuBuilder::new(app, "主机(&H)");
    for (_, item) in &host_items {
        host_menu = host_menu.item(item);
    }
    let host_menu = host_menu
        .separator()
        .item(&MenuItemBuilder::with_id(ID_RELOAD_HOSTS, "重新读取 hosts.json").build(app)?)
        .item(&MenuItemBuilder::with_id(ID_OPEN_HOSTS_DIR, "打开主机配置目录").build(app)?)
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

    // A bare item in the menu bar: clicking it fires straight through to
    // `handle` instead of dropping a submenu. No accelerator here — Ctrl+R is
    // already on the App submenu's 重新加载, and two items cannot claim it.
    let reload_button = MenuItemBuilder::with_id(ID_RELOAD_BUTTON, "刷新(&R)").build(app)?;

    let menu = MenuBuilder::new(app)
        .items(&[&app_menu, &host_menu, &edit_menu, &view_menu, &reload_button])
        .build()?;

    let handles = MenuHandles {
        check_updates,
        restart,
        hosts: host_items,
    };
    Ok((menu, handles))
}

/// Build, install, and remember the handles. Used at startup and by
/// 重新读取 hosts.json.
pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let (menu, handles) = build(app)?;
    app.set_menu(menu)?;
    *app.state::<AppState>()
        .menu
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(handles);
    sync(app);
    Ok(())
}

pub fn rebuild(app: &AppHandle) {
    let app2 = app.clone();
    // Menu construction must happen on the main thread on Windows.
    let _ = app.run_on_main_thread(move || {
        if let Err(e) = install(&app2) {
            log_dbg!("menu rebuild failed: {e}");
        }
    });
}

/// Re-tick the host list and relabel/disable the remote-sensitive items.
pub fn sync(app: &AppHandle) {
    let state = app.state::<AppState>();
    let target = state.target();
    let remote = target != LOCAL_ID;
    let handles = state.menu.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(h) = handles else { return };
    for (id, item) in &h.hosts {
        let _ = item.set_checked(*id == target);
    }
    // 检查更新 is about the embedded runtime; while a remote host is in front
    // it would update (and restart) something the user is not looking at.
    let _ = h.check_updates.set_enabled(!remote);
    let _ = h
        .restart
        .set_text(if remote { LABEL_RESTART_REMOTE } else { LABEL_RESTART_LOCAL });
}

pub fn handle(app: &AppHandle, id: &str) {
    if let Some(host_id) = id.strip_prefix(HOST_PREFIX) {
        // Restore the tick immediately (muda flipped it on click); the switch
        // will set the final state.
        sync(app);
        let app = app.clone();
        let host_id = host_id.to_string();
        std::thread::spawn(move || crate::app::switch_to(&app, &host_id));
        return;
    }
    match id {
        ID_CHECK_UPDATES => {
            if !app.state::<AppState>().target_is_local() {
                return;
            }
            let app = app.clone();
            std::thread::spawn(move || crate::app::check_for_updates(&app, true));
        }
        ID_RELOAD | ID_RELOAD_BUTTON => {
            if let Some(win) = crate::app::main_window(app) {
                let _ = win.reload();
            }
        }
        ID_RESTART_SERVER => {
            let app = app.clone();
            std::thread::spawn(move || {
                if let Err(e) = crate::app::restart_current(&app) {
                    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
                    app.dialog()
                        .message(e)
                        .title(if app.state::<AppState>().target_is_local() {
                            "重启失败"
                        } else {
                            "重连失败"
                        })
                        .kind(MessageDialogKind::Error)
                        .blocking_show();
                }
            });
        }
        ID_RELOAD_HOSTS => {
            let app = app.clone();
            std::thread::spawn(move || crate::app::reload_hosts(&app));
        }
        ID_OPEN_HOSTS_DIR => {
            let dir = app.state::<AppState>().paths.user_data_dir().to_path_buf();
            crate::app::open_folder(&dir);
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
