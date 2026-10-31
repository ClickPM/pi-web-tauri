//! Isolated backend for the `hosts-manager` window (`dist/hosts.html`).
//!
//! Security boundary:
//!   * Every command in this module is granted ONLY to `"windows": ["hosts-manager"]`
//!     in `capabilities/hosts-manager.json` (with no `remote.urls`), never to
//!     `main` and never to `http://127.0.0.1:*`.
//!   * Every handler also checks `window.label() == HOSTS_WINDOW` in Rust as a
//!     second line of defence.
//!   * Read endpoints never return plaintext passwords or environment-variable
//!     values; write/probe endpoints apply the same strict allow-list as
//!     `hosts::parse` before touching disk or spawning `ssh.exe`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::app::{self, AppState};
use crate::hosts::{self, HostDraft, HostSummary};
use crate::log_dbg;
use crate::remote::{self, ConnectionTestReport};
use crate::server;

pub const HOSTS_WINDOW: &str = "hosts-manager";

static OPENING_WINDOW: AtomicBool = AtomicBool::new(false);

struct OpeningGuard;

impl Drop for OpeningGuard {
    fn drop(&mut self) {
        OPENING_WINDOW.store(false, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostsManagerState {
    pub config_path: String,
    pub file_exists: bool,
    pub active_target: String,
    pub keep_local_running: bool,
    pub hosts: Vec<HostSummary>,
    pub warnings: Vec<String>,
    pub ssh_aliases: Vec<String>,
    pub suggested_port: u16,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvCheckStatus {
    pub valid: bool,
    pub is_set: bool,
}

fn ensure_hosts_window(window: &WebviewWindow) -> Result<(), String> {
    if window.label() == HOSTS_WINDOW {
        Ok(())
    } else {
        log_dbg!(
            "hosts_ui: rejected call from unexpected window {:?}",
            window.label()
        );
        Err("此命令仅允许在远程主机管理窗口中调用".into())
    }
}

/// Open `hosts-manager`, or unminimize and focus it when already open.
pub fn open_or_focus(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(HOSTS_WINDOW) {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
        return;
    }
    if OPENING_WINDOW
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    // Spawn off the UI thread: WebView2 creation on Windows pumps COM callbacks
    // back onto the main message loop and deadlocks if invoked directly inside
    // a synchronous menu event callback.
    let app = app.clone();
    std::thread::spawn(move || {
        let _guard = OpeningGuard;
        if let Some(win) = app.get_webview_window(HOSTS_WINDOW) {
            let _ = win.unminimize();
            let _ = win.show();
            let _ = win.set_focus();
            return;
        }

        let nav_app = app.clone();
        let builder = WebviewWindowBuilder::new(
            &app,
            HOSTS_WINDOW,
            WebviewUrl::App("hosts.html".into()),
        )
        .title("管理远程主机 — Pi")
        .inner_size(780.0, 620.0)
        .min_inner_size(680.0, 500.0)
        .center()
        .background_color(tauri::window::Color(0x0A, 0x0A, 0x0A, 0xFF))
        .theme(Some(tauri::Theme::Dark))
        .on_navigation(move |url| {
            if nav_app.state::<AppState>().is_shell_asset_url(url) {
                return true;
            }
            app::open_external(url);
            false
        })
        .on_new_window(|url, _features| {
            app::open_external(&url);
            tauri::webview::NewWindowResponse::Deny
        });

        match builder.build() {
            Ok(win) => {
                // `app.set_menu(...)` attaches the main menu bar to every
                // window by default; strip it from this dialog window.
                let _ = win.hide_menu();
                let _ = win.set_focus();
                log_dbg!("hosts_ui: opened {HOSTS_WINDOW} window");
            }
            Err(e) => {
                log_dbg!("hosts_ui: failed to open {HOSTS_WINDOW} window: {e}");
            }
        }
    });
}

fn build_manager_state(app: &AppHandle) -> HostsManagerState {
    let state = app.state::<AppState>();
    let dir = state.paths.user_data_dir();
    let snap = hosts::load_ui_snapshot(dir);
    let used_ports: Vec<u16> = snap.hosts.iter().map(|h| h.local_port).collect();
    let suggested_port =
        hosts::next_available_local_port(&used_ports, server::is_free).unwrap_or(hosts::REMOTE_PORT_MIN);

    HostsManagerState {
        config_path: hosts::hosts_path(dir).display().to_string(),
        file_exists: snap.exists,
        active_target: state.target(),
        keep_local_running: snap.keep_local_running,
        hosts: snap.hosts,
        warnings: snap.warnings,
        ssh_aliases: hosts::load_ssh_config_hosts(),
        suggested_port,
    }
}

#[tauri::command]
pub fn hosts_get_state(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<HostsManagerState, String> {
    ensure_hosts_window(&window)?;
    Ok(build_manager_state(&app))
}

#[tauri::command]
pub fn hosts_suggest_port(
    window: WebviewWindow,
    used_ports: Vec<u16>,
) -> Result<u16, String> {
    ensure_hosts_window(&window)?;
    hosts::next_available_local_port(&used_ports, server::is_free)
        .ok_or_else(|| "45000-48999 隧道端口池已无空闲端口".into())
}

#[tauri::command]
pub fn hosts_check_env(
    window: WebviewWindow,
    name: String,
) -> Result<EnvCheckStatus, String> {
    ensure_hosts_window(&window)?;
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Ok(EnvCheckStatus {
            valid: true,
            is_set: false,
        });
    }
    Ok(EnvCheckStatus {
        valid: hosts::is_valid_env_name(trimmed),
        is_set: hosts::is_env_var_set(trimmed),
    })
}

struct ProbeGateClear<'a> {
    state: &'a AppState,
    gate: Arc<remote::ProbeGate>,
}

impl Drop for ProbeGateClear<'_> {
    fn drop(&mut self) {
        self.state.clear_probe_gate(&self.gate);
    }
}

#[tauri::command]
pub async fn hosts_test_connection(
    app: AppHandle,
    window: WebviewWindow,
    draft: HostDraft,
) -> Result<ConnectionTestReport, String> {
    ensure_hosts_window(&window)?;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let _probe = state.probe_lock.lock().unwrap_or_else(|e| e.into_inner());

        let (mut host_cfg, unset_env) =
            hosts::validate_draft_for_probe(state.paths.user_data_dir(), &draft)?;

        let requested_port = host_cfg.local_port;
        let (gate, borrowed_port) = state.reserve_probe_gate(requested_port)?;
        let _clear = ProbeGateClear {
            state: &state,
            gate: gate.clone(),
        };
        if borrowed_port {
            log_dbg!(
                "hosts_ui: localPort {} is held by active/connecting session; borrowing {} for probe",
                requested_port,
                gate.port()
            );
            host_cfg.local_port = gate.port();
        }

        Ok(remote::probe_connection(
            &host_cfg,
            unset_env.as_deref(),
            borrowed_port,
            &state.job,
            &gate,
        ))
    })
    .await
    .map_err(|e| format!("连接测试任务异常结束：{e}"))?
}

#[tauri::command]
pub async fn hosts_save(
    app: AppHandle,
    window: WebviewWindow,
    keep_local_running: bool,
    hosts: Vec<HostDraft>,
) -> Result<HostsManagerState, String> {
    ensure_hosts_window(&window)?;
    tauri::async_runtime::spawn_blocking(move || {
        app::save_and_reload_hosts(&app, keep_local_running, &hosts)?;
        Ok(build_manager_state(&app))
    })
    .await
    .map_err(|e| format!("保存任务异常结束：{e}"))?
}

#[tauri::command]
pub fn hosts_open_dir(app: AppHandle, window: WebviewWindow) -> Result<(), String> {
    ensure_hosts_window(&window)?;
    let dir = app.state::<AppState>().paths.user_data_dir().to_path_buf();
    app::open_folder(&dir);
    Ok(())
}

#[tauri::command]
pub fn hosts_close_window(window: WebviewWindow) -> Result<(), String> {
    ensure_hosts_window(&window)?;
    window.close().map_err(|e| e.to_string())
}
