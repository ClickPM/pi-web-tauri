//! Shell state and the boot/update state machine — main.js's orchestration half.
//!
//! The sequencing is copied deliberately, including the parts that look
//! paranoid, because each one is a bug that was already paid for:
//!
//!   * swap recovery runs before ANYTHING calls `runtime_dir()`, because an
//!     interrupted swap can leave the seed dir momentarily absent and
//!     `runtime_dir()` caches its answer for the rest of the session;
//!   * the boot self-heal reinstalls the PINNED version, never `@latest` — a
//!     damaged install is not a reason to also change versions;
//!   * the self-heal and both update paths share ONE lock, and the auto-check
//!     skips itself for two minutes after a provision so the user is never made
//!     to sit through two installs back to back.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::{AppHandle, Manager, WebviewWindow};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

use crate::guard::Guard;
use crate::hosts::{self, HostsFile, LOCAL_ID};
use crate::job::ProcessJob;
use crate::log_dbg;
use crate::paths::Paths;
use crate::proc::NoConsole;
use crate::remote::{self, ProbeGate, RemoteSession};
use crate::server::{self, Server};

pub const MAIN_WINDOW: &str = "main";

/// Every port a pi-web backend can sit on: the embedded server's pool
/// (30000..=44999, server.rs) followed by the tunnel pool (45000..=48999,
/// hosts.rs). A bundled status page is never served from inside it — `tauri
/// dev` uses :1430 and a packaged build has no port at all.
const BACKEND_PORTS: std::ops::RangeInclusive<u16> = 30_000..=hosts::REMOTE_PORT_MAX;

/// What the window is pointed at right now: the embedded server or one remote
/// host's tunnel. Everything that used to ask `state.server()` "is this our
/// page / where do we go / which cookie" asks this instead.
#[derive(Clone)]
pub enum ActiveBackend {
    Local(Arc<Server>),
    Remote(Arc<RemoteSession>),
}

impl ActiveBackend {
    pub fn origin(&self) -> String {
        match self {
            Self::Local(s) => s.origin(),
            Self::Remote(r) => r.origin(),
        }
    }

    pub fn nav_url(&self) -> String {
        match self {
            Self::Local(s) => s.nav_url(),
            Self::Remote(r) => r.nav_url(),
        }
    }

    pub fn session_cookie(&self) -> Option<String> {
        match self {
            Self::Local(s) => s.session_cookie(),
            Self::Remote(r) => r.session_cookie(),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Local(s) => format!("local {}", s.origin()),
            Self::Remote(r) => format!("remote {} {}", r.host.id, r.origin()),
        }
    }
}

/// Whether `url` is on exactly `origin` (scheme + host + port).
fn same_origin(url: &url::Url, origin: &str) -> bool {
    url::Url::parse(origin)
        .map(|o| o.origin() == url.origin())
        .unwrap_or(false)
}

/// Internal sentinel: "the server is up but the shell could not log in to it".
/// Never shown to the user — it only routes control to the password-less retry.
const LOGIN_FAILED: &str = "__piwd_login_failed__";

/// The update-result CTA the injected script renders. Mirrors the object
/// main.js sent over IPC, field for field, so preload.js needed no changes.
#[derive(Debug, Clone, Serialize)]
pub struct Notice {
    pub status: &'static str,
    pub title: String,
    pub message: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<NoticeAction>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NoticeAction {
    pub id: &'static str,
    pub label: &'static str,
}

#[derive(Default)]
struct PortCoordination {
    connecting_port: Option<u16>,
    probe: Option<Arc<ProbeGate>>,
}

pub struct AppState {
    pub paths: Paths,
    pub job: ProcessJob,
    /// The embedded server. With `keepLocalRunning` it stays up in the
    /// background while a remote host is in front, so it is NOT the same thing
    /// as "the active backend" — see `active_backend()`.
    server: Mutex<Option<Arc<Server>>>,
    /// The live tunnel, when the target is a remote host and it connected.
    remote: Mutex<Option<Arc<RemoteSession>>>,
    /// What the user selected: `LOCAL_ID` or a host id. Set at the START of a
    /// switch (so the title and the menu tick follow the click at once) and
    /// kept on a failed connect, so 重连当前远端 knows what to retry.
    target: Mutex<String>,
    hosts: Mutex<HostsFile>,
    /// Serialises switches, reconnects, reloads and Host Manager saves against
    /// each other so target transitions and `hosts.json` updates form one
    /// linearizable sequence.
    switch_lock: Mutex<()>,
    /// Monotonic counter bumped under `switch_lock` whenever the target or
    /// host configuration changes, so deferred reconnect tasks abort if a
    /// newer user action superseded them.
    switch_epoch: AtomicU64,
    pub probe_lock: Mutex<()>,
    port_coord: Mutex<PortCoordination>,
    /// The boot-time recover/migrate/verify sequence has run. When the app
    /// boots straight into a remote host it is deferred until the first switch
    /// to local, and it must run exactly once (runtime_dir() caches).
    local_prepared: AtomicBool,
    /// The `pi_web_session=` value last installed in the webview's cookie jar.
    ///
    /// Cookies are scoped by HOST, not by port: the embedded server and every
    /// tunnel are all `127.0.0.1`, so they share ONE `pi_web_session` slot and
    /// each backend's login overwrites the previous one's. Comparing against
    /// what is actually installed (rather than a per-backend "done" flag) is
    /// what makes switching back re-install the right token.
    installed_cookie: Mutex<Option<String>>,
    pub menu: Mutex<Option<crate::menu::MenuHandles>>,
    /// Base URL of the bundled status pages. Captured rather than hardcoded
    /// because it differs between modes: `tauri dev` serves them from
    /// http://127.0.0.1:1430/, a packaged build from http://tauri.localhost/.
    local_base: Mutex<Option<url::Url>>,

    /// EVERY path that mutates the runtime tree goes through this gate. Two of
    /// them at once would race on the same staging dir and the same swap
    /// journal, so overlapping requests are DROPPED rather than queued: the
    /// loser has nothing useful to do by the time the winner finishes.
    runtime_busy: AtomicBool,
    /// When the runtime was last (re)provisioned, so the boot auto-check can
    /// avoid immediately re-installing over a self-heal that just finished.
    last_provision_ms: AtomicU64,

    quitting: AtomicBool,
    restarting: AtomicBool,
    stopping_for_update: AtomicBool,
    /// Set after a failed login, so the retry starts pi-web with no password at
    /// all. Latching it (rather than retrying with a password forever) is what
    /// stops the fallback from looping.
    password_disabled: AtomicBool,

    pending_notice: Mutex<Option<Notice>>,
}

impl AppState {
    pub fn new(paths: Paths) -> Self {
        let hosts = hosts::load(paths.user_data_dir());
        Self {
            target: Mutex::new(hosts.active.clone()),
            hosts: Mutex::new(hosts),
            paths,
            job: ProcessJob::create(),
            server: Mutex::new(None),
            remote: Mutex::new(None),
            switch_lock: Mutex::new(()),
            switch_epoch: AtomicU64::new(1),
            probe_lock: Mutex::new(()),
            port_coord: Mutex::new(PortCoordination::default()),
            local_prepared: AtomicBool::new(false),
            installed_cookie: Mutex::new(None),
            menu: Mutex::new(None),
            local_base: Mutex::new(None),
            runtime_busy: AtomicBool::new(false),
            last_provision_ms: AtomicU64::new(0),
            quitting: AtomicBool::new(false),
            restarting: AtomicBool::new(false),
            stopping_for_update: AtomicBool::new(false),
            password_disabled: AtomicBool::new(false),
            pending_notice: Mutex::new(None),
        }
    }

    pub fn server(&self) -> Option<Arc<Server>> {
        self.server
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn remote(&self) -> Option<Arc<RemoteSession>> {
        self.remote
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// `LOCAL_ID` or the selected host's id.
    pub fn target(&self) -> String {
        self.target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn target_is_local(&self) -> bool {
        self.target() == LOCAL_ID
    }

    fn set_target(&self, id: &str) {
        *self.target.lock().unwrap_or_else(|e| e.into_inner()) = id.to_string();
    }

    fn bump_switch_epoch(&self) -> u64 {
        self.switch_epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn reserve_probe_gate(&self, requested_port: u16) -> Result<(Arc<ProbeGate>, bool), String> {
        let active_remote_port = self.remote().map(|r| r.host.local_port);
        let configured_ports: Vec<u16> = self.hosts().hosts.iter().map(|h| h.local_port).collect();
        let mut coord = self.port_coord.lock().unwrap_or_else(|e| e.into_inner());
        let held = active_remote_port == Some(requested_port) || coord.connecting_port == Some(requested_port);
        let (probe_port, borrowed) = if held {
            let mut used = configured_ports;
            used.push(requested_port);
            if let Some(cp) = coord.connecting_port { used.push(cp); }
            if let Some(ap) = active_remote_port { used.push(ap); }
            let temp = hosts::next_available_local_port(&used, server::is_free)
                .ok_or_else(|| "45000-48999 中无可用临时端口用于连接测试".to_string())?;
            (temp, true)
        } else {
            (requested_port, false)
        };
        let gate = Arc::new(ProbeGate::new(probe_port));
        coord.probe = Some(gate.clone());
        Ok((gate, borrowed))
    }

    pub fn clear_probe_gate(&self, gate: &Arc<ProbeGate>) {
        let mut coord = self.port_coord.lock().unwrap_or_else(|e| e.into_inner());
        if coord.probe.as_ref().map(|g| Arc::ptr_eq(g, gate)).unwrap_or(false) { coord.probe = None; }
    }

    fn begin_session_connect(&self, port: u16) {
        let conflicting = {
            let mut coord = self.port_coord.lock().unwrap_or_else(|e| e.into_inner());
            coord.connecting_port = Some(port);
            if coord.probe.as_ref().map(|g| g.port()) == Some(port) { coord.probe.take() } else { None }
        };
        if let Some(gate) = conflicting {
            log_dbg!("preempting in-flight diagnostic probe on port {port} for real switch");
            gate.preempt_and_stop();
        }
    }

    fn end_session_connect(&self, port: u16) {
        let mut coord = self.port_coord.lock().unwrap_or_else(|e| e.into_inner());
        if coord.connecting_port == Some(port) { coord.connecting_port = None; }
    }

    pub fn hosts(&self) -> HostsFile {
        self.hosts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The backend the window should be showing, or None while it is not up
    /// (booting, connecting, or the connect failed).
    pub fn active_backend(&self) -> Option<ActiveBackend> {
        let target = self.target();
        if target == LOCAL_ID {
            return self.server().map(ActiveBackend::Local);
        }
        self.remote()
            .filter(|r| r.host.id == target)
            .map(ActiveBackend::Remote)
    }

    /// The native window title for the current target: plain `Pi` for the
    /// embedded server, `Pi — <label>` for a remote host, so it is always
    /// obvious which machine the agent in front of you is running on.
    pub fn expected_title(&self) -> String {
        let target = self.target();
        if target == LOCAL_ID {
            return "Pi".into();
        }
        let label = self
            .hosts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .find(&target)
            .map(|h| h.label.clone())
            .unwrap_or(target);
        format!("Pi — {label}")
    }

    pub fn set_local_base(&self, url: url::Url) {
        *self.local_base.lock().unwrap_or_else(|e| e.into_inner()) = Some(url);
    }

    /// Record the base from a finished page load, but only from one of OUR
    /// pages — capturing a pi-web origin here would make every later
    /// `show_local` navigate back into the very backend we are leaving.
    ///
    /// "A pi-web origin" is decided by PORT RANGE, not by asking which backend
    /// is active: during a switch the previous host's page can still finish
    /// loading after the state already points elsewhere, and a remote tunnel
    /// (127.0.0.1:4500x) looks exactly like a local page to any host check.
    pub fn capture_local_base(&self, url: &url::Url) {
        if !matches!(url.scheme(), "http" | "https") || is_backend_url(url) {
            return;
        }
        if let Some(active) = self.active_backend() {
            if same_origin(url, &active.origin()) {
                return;
            }
        }
        let mut guard = self.local_base.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            log_dbg!("local base captured from first page load = {url}");
            *guard = Some(url.clone());
        }
    }

    fn local_page(&self, page: &str) -> Option<url::Url> {
        self.local_base
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|base| base.join(page).ok())
    }

    pub fn mark_quitting(&self) {
        self.quitting.store(true, Ordering::SeqCst);
    }

    pub fn is_quitting(&self) -> bool {
        self.quitting.load(Ordering::SeqCst)
    }

    /// Whether `url` belongs to the shell itself (bundled status pages, the
    /// embedded pi-web server, or the active remote host's tunnel). Everything
    /// else — links in agent replies, OAuth pages, docs, another dev server on
    /// localhost, or a DIFFERENT host's tunnel port — is external and belongs in
    /// the user's browser rather than inside the app window.
    pub fn is_internal_url(&self, url: &url::Url) -> bool {
        if url.as_str() == "about:blank"
            || url.scheme() == "tauri"
            || url.host_str() == Some("tauri.localhost")
        {
            return true;
        }
        if let Some(server) = self.server() {
            if same_origin(url, &server.origin()) {
                return true;
            }
        }
        if let Some(remote) = self.remote() {
            if remote.host.id == self.target() && same_origin(url, &remote.origin()) {
                return true;
            }
        }
        if let Some(base) = self
            .local_base
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            if url.origin() == base.origin() {
                return true;
            }
        }
        false
    }

    /// Stricter variant for `hosts-manager`: only the shell's own bundled
    /// pages are allowed, never a local or tunnelled pi-web backend port.
    pub fn is_shell_asset_url(&self, url: &url::Url) -> bool {
        if url.as_str() == "about:blank"
            || url.scheme() == "tauri"
            || url.host_str() == Some("tauri.localhost")
        {
            return true;
        }
        if is_backend_url(url) {
            return false;
        }
        self.local_base
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|base| url.origin() == base.origin())
            .unwrap_or(false)
    }
}

/// A URL on `127.0.0.1` / `localhost` whose port is in the backend pools — i.e.
/// some pi-web (embedded or tunnelled), never one of the shell's own pages.
fn is_backend_url(url: &url::Url) -> bool {
    matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"))
        && url.port().map(|p| BACKEND_PORTS.contains(&p)).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Runtime write lock
// ---------------------------------------------------------------------------

/// Returns None when another runtime operation is already in flight.
fn try_runtime_lock<'a>(state: &'a AppState, label: &str) -> Option<RuntimeLock<'a>> {
    if state
        .runtime_busy
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        log_dbg!("runtime lock held — skipping {label}");
        return None;
    }
    log_dbg!("runtime lock acquired by {label}");
    Some(RuntimeLock {
        state,
        label: label.to_string(),
    })
}

pub struct RuntimeLock<'a> {
    state: &'a AppState,
    label: String,
}

impl Drop for RuntimeLock<'_> {
    fn drop(&mut self) {
        self.state.runtime_busy.store(false, Ordering::SeqCst);
        self.state.last_provision_ms.store(now_ms(), Ordering::SeqCst);
        log_dbg!("runtime lock released by {}", self.label);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Navigation helpers
// ---------------------------------------------------------------------------

/// Open an external link in the user's default browser / mail client.
///
/// Mirrors Electron's `setWindowOpenHandler` + `shell.openExternal`: only
/// `http`, `https` and `mailto` are forwarded to the OS handler, so an
/// agent-rendered link cannot trigger `file:` or custom local URI schemes.
pub fn open_external(url: &url::Url) {
    if !matches!(url.scheme(), "http" | "https" | "mailto") {
        log_dbg!("open_external: blocked non-web scheme in {url}");
        return;
    }
    log_dbg!("open_external: {url}");

    #[cfg(windows)]
    let result = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url.as_str()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .no_console()
        .spawn();

    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open")
        .arg(url.as_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open")
        .arg(url.as_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    if let Err(e) = result {
        log_dbg!("open_external failed for {url}: {e}");
    }
}

pub fn main_window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window(MAIN_WINDOW)
}

/// Open a folder in Explorer (「打开主机配置目录」). Only ever called with the
/// shell's own user-data dir — no page input reaches it.
pub fn open_folder(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    #[cfg(windows)]
    let result = std::process::Command::new("explorer").arg(dir).no_console().spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(dir).spawn();
    if let Err(e) = result {
        log_dbg!("open_folder({}) failed: {e}", dir.display());
    }
}

fn show_local(app: &AppHandle, page: &str) {
    let state = app.state::<AppState>();
    // During the first ~50ms after window creation `index.html` is still
    // finishing its initial load, so `local_base` is not captured yet. If the
    // caller only wanted `index.html`, the webview is already going there; for
    // `error.html` / `healing.html` / `updating.html`, wait briefly (we are
    // always on a worker thread) so a fast boot failure never drops its page.
    let mut url = state.local_page(page);
    if url.is_none() && page != "index.html" {
        for _ in 0..80 {
            std::thread::sleep(Duration::from_millis(25));
            url = state.local_page(page);
            if url.is_some() {
                break;
            }
        }
    }
    let Some(url) = url else {
        if page != "index.html" {
            log_dbg!("show_local({page}): no local base captured yet");
        }
        return;
    };
    if let Some(win) = main_window(app) {
        if let Err(e) = win.navigate(url) {
            log_dbg!("show_local({page}) failed: {e}");
        }
    }
}

/// Navigate to the ACTIVE backend (local or remote). No-op when it is not up.
fn show_pi_web(app: &AppHandle) {
    let state = app.state::<AppState>();
    let Some(active) = state.active_backend() else { return };
    let Ok(url) = active.nav_url().parse::<url::Url>() else {
        return;
    };
    log_dbg!("navigate -> {}", active.describe());
    if let Some(win) = main_window(app) {
        if let Err(e) = win.navigate(url) {
            log_dbg!("navigate to pi-web failed: {e}");
        }
    }
}

/// Re-apply the native title for the current target. Called after every page
/// load and every switch, because pi-web's `<title>` must never reach the OS
/// window and the remote label must always be visible.
pub fn sync_title(app: &AppHandle) {
    let title = app.state::<AppState>().expected_title();
    if let Some(win) = main_window(app) {
        if win.title().ok().as_deref() != Some(title.as_str()) {
            let _ = win.set_title(&title);
        }
    }
}

pub fn show_error(app: &AppHandle, message: &str, log: &str) {
    // Remote failures get their own heading; "the local service did not
    // start" would send the user looking in the wrong place.
    let state = app.state::<AppState>();
    let (head, hint) = if state.target_is_local() {
        (String::new(), String::new())
    } else {
        (
            format!("{} 连接失败", state.expected_title()),
            "远端主机不可用。下方是 SSH 的输出与排障提示；可随时用菜单「主机 → Windows 本地」(Ctrl+Alt+1) 切回本机。"
                .to_string(),
        )
    };
    show_local(app, "error.html");
    let script = format!(
        "(function(){{var f=function(){{if(window.__setError)window.__setError({},{},{},{});else setTimeout(f,60)}};f()}})()",
        json_string(message),
        json_string(log),
        json_string(&head),
        json_string(&hint)
    );
    // The page may still be loading; the retry loop above covers that.
    if let Some(win) = main_window(app) {
        let _ = win.eval(&script);
    }
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Push npm's progress into the updating/healing page.
fn show_progress(app: &AppHandle, text: &str) {
    if let Some(win) = main_window(app) {
        let _ = win.eval(format!(
            "window.__piwdProgress&&window.__piwdProgress({})",
            json_string(text)
        ));
    }
}

// ---------------------------------------------------------------------------
// Update-result CTA
// ---------------------------------------------------------------------------

/// Install the active backend's session cookie in the webview, local or remote.
///
/// Called on every finished page load. Acts only when the window is on the
/// active backend's origin AND that backend's token is not the one currently
/// in the cookie jar. Cookies are host-scoped (ports are ignored), so the
/// embedded server and every tunnel share one `pi_web_session` slot on
/// 127.0.0.1 — comparing against what is installed is what makes switching
/// back and forth re-install the right token each time. The page is reloaded
/// so pi-web's own auth check re-runs with the cookie present; the reload
/// fires another page-load, which the comparison stops from looping.
/// Returns true when it reloaded the page.
pub fn inject_session_cookie(app: &AppHandle) -> bool {
    let state = app.state::<AppState>();
    let Some(active) = state.active_backend() else {
        return false;
    };
    let Some(win) = main_window(app) else {
        return false;
    };

    let on_backend = win
        .url()
        .map(|u| same_origin(&u, &active.origin()))
        .unwrap_or(false);
    if !on_backend {
        return false;
    }
    let Some(cookie) = active.session_cookie() else {
        return false;
    };
    {
        let mut installed = state.installed_cookie.lock().unwrap_or_else(|e| e.into_inner());
        if installed.as_deref() == Some(cookie.as_str()) {
            return false;
        }
        *installed = Some(cookie.clone());
    }

    // `path=/` and no expiry: a session cookie for this origin, exactly as the
    // server set it. SameSite=Lax matches a same-origin-only app.
    let script = format!(
        "document.cookie={}+\"; path=/; SameSite=Lax\";location.reload()",
        json_string(&cookie)
    );
    match win.eval(&script) {
        Ok(()) => {
            log_dbg!("session cookie installed for {}; reloading", active.describe());
            true
        }
        Err(e) => {
            log_dbg!("could not install the session cookie: {e}");
            *state.installed_cookie.lock().unwrap_or_else(|e| e.into_inner()) = None;
            false
        }
    }
}

/// Deliver the queued CTA once the LOCAL pi-web page is present. Called after
/// every page load, because a successful update reloads the page before we can
/// report. Update notices are about the embedded runtime, so they are held
/// while a remote host is in front and delivered when the user switches back.
pub fn flush_update_notice(app: &AppHandle) {
    let state = app.state::<AppState>();
    let Some(ActiveBackend::Local(server)) = state.active_backend() else { return };
    let Some(win) = main_window(app) else { return };

    let on_pi_web = win
        .url()
        .map(|u| same_origin(&u, &server.origin()))
        .unwrap_or(false);
    if !on_pi_web {
        let has_pending = state
            .pending_notice
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        if has_pending {
            log_dbg!("update notice held — the window is not on a pi-web page yet");
        }
        return;
    }

    let notice = {
        let mut guard = state.pending_notice.lock().unwrap_or_else(|e| e.into_inner());
        guard.take()
    };
    let Some(notice) = notice else { return };
    let payload = serde_json::to_string(&notice).unwrap_or_else(|_| "null".into());
    match win.eval(format!("window.__piwdNotice&&window.__piwdNotice({payload})")) {
        Ok(()) => log_dbg!("update notice delivered: {} / {}", notice.status, notice.title),
        Err(e) => log_dbg!("update notice delivery FAILED: {e}"),
    }
}

/// Queue an update-result CTA. Delivered immediately when a pi-web page is up;
/// otherwise held until the next page load flushes it.
fn notify_update(app: &AppHandle, notice: Notice) {
    {
        let state = app.state::<AppState>();
        let mut guard = state.pending_notice.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(notice);
    }
    flush_update_notice(app);
}

// ---------------------------------------------------------------------------
// Runtime seeding (read-only install copies the seed to a writable dir)
// ---------------------------------------------------------------------------

/// main.js's `copyRuntime()`: robocopy on Windows, because `fs.cp` aborted
/// partway through node_modules trees with long paths. Exit codes below 8 are
/// success (0 = nothing to do, 1 = files copied, …).
fn copy_runtime(src: &Path, dst: &Path) -> Result<(), String> {
    let mut cmd = std::process::Command::new("robocopy");
    cmd.arg(src)
        .arg(dst)
        .args(["/E", "/NFL", "/NDL", "/NJH", "/NJS", "/NP", "/R:1", "/W:1", "/MT:16"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .no_console();
    let status = cmd.status().map_err(|e| format!("robocopy failed to run: {e}"))?;
    match status.code() {
        Some(code) if code >= 8 => Err(format!("robocopy failed (code {code})")),
        _ => Ok(()),
    }
}

/// main.js's `ensureRuntime()`. Returns the installed pi-web version.
fn ensure_runtime(state: &AppState, guard: &Guard) -> Result<String, String> {
    let rt = state.paths.runtime_dir().to_path_buf();
    let seed = state.paths.seed_dir();

    let react_ok = |dir: &Path| dir.join("node_modules").join("react").join("package.json").exists();

    if state.paths.runtime_is_in_place() {
        let v = guard.installed()?.pi_web;
        let ok = react_ok(&rt);
        log_dbg!("ensureRuntime in-place v={v:?} reactOk={ok}");
        return match (v, ok) {
            (Some(v), true) => Ok(v),
            _ => Err(format!("in-place runtime incomplete at {}", rt.display())),
        };
    }

    // Fallback (read-only install): copy seed -> writable user dir.
    let marker = rt.join(".seeded");
    if marker.exists() {
        if let Some(v) = guard.installed()?.pi_web {
            return Ok(v);
        }
    }
    if !seed
        .join("node_modules")
        .join("@agegr")
        .join("pi-web")
        .join(".next")
        .exists()
    {
        return Err(format!(
            "runtime seed not found or incomplete at {}",
            seed.display()
        ));
    }
    std::fs::create_dir_all(&rt).map_err(|e| format!("could not create {}: {e}", rt.display()))?;
    log_dbg!("seeding runtime via robocopy: {} -> {}", seed.display(), rt.display());
    copy_runtime(&seed, &rt)?;

    let v = guard.installed()?.pi_web;
    let ok = react_ok(&rt);
    log_dbg!("seed copy done: version={v:?} reactOk={ok}");
    match (v, ok) {
        (Some(v), true) => {
            let _ = std::fs::write(&marker, &v);
            Ok(v)
        }
        (v, ok) => Err(format!("seed copy incomplete (version={v:?}, react={ok})")),
    }
}

// ---------------------------------------------------------------------------
// Boot preflight + self-heal
// ---------------------------------------------------------------------------

/// Verifies the runtime actually loads and, when it does not, repairs it through
/// the same atomic path an update uses.
fn ensure_runtime_healthy(app: &AppHandle, guard: &Guard) -> Result<(), String> {
    let check = guard.verify()?;
    if check.verified {
        log_dbg!("preflight: runtime verified");
        return Ok(());
    }
    log_dbg!(
        "preflight FAILED: {} (healable={})",
        check.summary,
        check.healable
    );

    if !check.healable {
        // Broken in a way reinstalling cannot fix (bad ABI, missing system
        // library). Reinstalling in a loop would waste minutes and still fail.
        return Err(format!(
            "运行时组件无法加载：{}\n\n这通常不是安装损坏，而是运行环境问题（缺少系统依赖或架构不匹配）。",
            check.summary
        ));
    }

    // Its own page rather than updating.html, so a repair never reads as
    // "you are being upgraded". A silent multi-minute stall looks identical to
    // a hang, which is what this avoids.
    show_local(app, "healing.html");

    let state = app.state::<AppState>();
    let Some(_lock) = try_runtime_lock(&state, "self-heal") else {
        // An update is already provisioning a fresh tree; its swap supersedes ours.
        log_dbg!("preflight: heal skipped, another runtime operation is in flight");
        return Ok(());
    };

    let summary = check.summary.clone();
    let progress_app = app.clone();
    let on_progress: crate::guard::ProgressFn =
        Arc::new(move |text: String| show_progress(&progress_app, &text));

    // spec = None: reinstall the pinned version, don't sneak in an upgrade.
    guard.provision(None, "self-heal", on_progress)?;

    state.stopping_for_update.store(false, Ordering::SeqCst);
    notify_update(
        app,
        Notice {
            status: "updated",
            title: "运行时已修复".into(),
            message: "检测到安装文件不完整，已自动重新安装".into(),
            detail: summary,
            action: None,
        },
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Server lifecycle
// ---------------------------------------------------------------------------

pub fn kill_server(app: &AppHandle) {
    let state = app.state::<AppState>();
    let taken = {
        let mut guard = state.server.lock().unwrap_or_else(|e| e.into_inner());
        guard.take()
    };
    if let Some(server) = taken {
        server.stop();
    }
}

/// main.js's `startOrRestartServer()`: stop, wait for handles to release, start,
/// wait for ready, navigate.
pub fn start_or_restart_server(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.restarting.store(true, Ordering::SeqCst);
    kill_server(app);
    std::thread::sleep(Duration::from_millis(400)); // let Windows release handles

    let result = (|| -> Result<Arc<Server>, String> {
        let port = server::resolve_port(&state.paths)?;
        // No password at all on runtimes that predate the cookie-auth route:
        // WebView2 cannot answer a Basic challenge, so setting one there would
        // lock the user out of their own app. See server::supports_cookie_auth.
        let password = (server::supports_cookie_auth(&state.paths)
            && !state.password_disabled.load(Ordering::SeqCst))
        .then(server::generate_password);

        let crash_app = app.clone();
        let server = Server::start(
            &state.paths,
            &state.job,
            port,
            password,
            move |code, tail| on_server_exit(&crash_app, code, tail),
        )?;

        // From here the child is running but is not yet owned by AppState, so
        // every failure path has to stop it by hand. Leaving it alive would keep
        // the port bound, and the next attempt would have to pick a different
        // one — which resets every pi-web preference (坑 1). The job object only
        // catches this when the whole shell exits, which is far too late.
        let started = |e: String, server: Server| -> String {
            server.stop();
            e
        };

        if let Err(e) = server.wait_ready(Duration::from_secs(90)) {
            return Err(started(e, server));
        }

        // Trade the password for a session cookie BEFORE the webview ever sees
        // the origin, so the only secret that reaches the page is the token.
        //
        // If this fails there is no way for the user to recover on their own —
        // pi-web would ask for a password only the shell ever knew — so a few
        // retries, and then a password-less restart, which at least leaves a
        // working app behind the loopback bind.
        match server.login_with_retries(3) {
            Ok(Some(_)) => log_dbg!("shell authenticated to pi-web"),
            Ok(None) => log_dbg!("no password configured — nothing to authenticate"),
            Err(e) => {
                log_dbg!("shell login FAILED ({e}) — restarting without a password");
                return Err(started(LOGIN_FAILED.into(), server));
            }
        }

        Ok(Arc::new(server))
    })();

    // The password-less retry, taken exactly once.
    if matches!(&result, Err(e) if e == LOGIN_FAILED)
        && state
            .password_disabled
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    {
        state.restarting.store(false, Ordering::SeqCst);
        return start_or_restart_server(app);
    }

    match result {
        Ok(server) => {
            log_dbg!(
                "server up at {} (job object: {})",
                server.origin(),
                if server.in_job() { "yes" } else { "NO — taskkill only" }
            );
            {
                let mut guard = state.server.lock().unwrap_or_else(|e| e.into_inner());
                *guard = Some(server);
            }
            state.restarting.store(false, Ordering::SeqCst);
            // A background restart of the embedded server (an update finishing
            // while a remote host is in front) must not yank the window away
            // from the host the user is looking at.
            if state.target_is_local() {
                show_pi_web(app);
            }
            Ok(())
        }
        Err(e) => {
            state.restarting.store(false, Ordering::SeqCst);
            Err(e)
        }
    }
}

/// The server process went away. Suppress the popup for INTENTIONAL stops — an
/// app quit, a restart, or an update that kills the old server before
/// reinstalling. Only a genuinely unexpected crash should alarm the user.
fn on_server_exit(app: &AppHandle, code: Option<i32>, tail: String) {
    let state = app.state::<AppState>();
    if state.is_quitting()
        || state.restarting.load(Ordering::SeqCst)
        || state.stopping_for_update.load(Ordering::SeqCst)
    {
        return;
    }
    if !state.target_is_local() {
        // keepLocalRunning: the embedded server died in the background while
        // a remote host is in front. No modal over the remote session — drop
        // the dead server so switching back starts a fresh one.
        log_dbg!("embedded server exited in the background (code={code:?}) — will restart on switch back");
        state.server.lock().unwrap_or_else(|e| e.into_inner()).take();
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        app.dialog()
            .message(format!(
                "内嵌服务意外退出 (code={code:?})。\n\n最近输出:\n{}",
                tail_chars(&tail, 2000)
            ))
            .title("Pi 服务已停止")
            .kind(MessageDialogKind::Error)
            .blocking_show();
    });
}

fn tail_chars(s: &str, n: usize) -> String {
    let start = s.len().saturating_sub(n);
    let start = (start..=s.len()).find(|i| s.is_char_boundary(*i)).unwrap_or(0);
    s[start..].to_string()
}

// ---------------------------------------------------------------------------
// Updates
// ---------------------------------------------------------------------------

pub fn check_for_updates(app: &AppHandle, interactive: bool) {
    let state = app.state::<AppState>();
    if state.runtime_busy.load(Ordering::SeqCst) {
        return;
    }
    let guard = Guard::new(&state.paths);

    let latest = match guard.latest() {
        Ok(l) => l,
        Err(e) => {
            if interactive {
                app.dialog()
                    .message(tail_chars(&e, 1500))
                    .title("检查更新失败")
                    .kind(MessageDialogKind::Error)
                    .blocking_show();
            }
            notify_update(
                app,
                Notice {
                    status: "error",
                    title: "检查更新失败".into(),
                    message: "无法获取最新版本信息".into(),
                    detail: "请检查网络连接后重试。".into(),
                    action: None,
                },
            );
            return;
        }
    };

    let installed = latest.installed.clone().unwrap_or_else(|| "未知".into());
    log_dbg!(
        "update check ({}): installed={installed} latest={} newer={}",
        if interactive { "manual" } else { "auto" },
        latest.latest,
        latest.newer
    );
    if !latest.newer {
        notify_update(
            app,
            Notice {
                status: "latest",
                title: "已是最新版本".into(),
                message: format!("pi-web {installed}"),
                detail: latest
                    .agent
                    .map(|a| format!("pi-coding-agent {a} · 无需更新"))
                    .unwrap_or_else(|| "无需更新".into()),
                action: None,
            },
        );
        return;
    }

    // A newer version exists. The boot-time auto check updates silently; a
    // manual "检查更新…" asks first so the user controls the restart.
    if interactive {
        let answer = app
            .dialog()
            .message(format!(
                "当前 {installed} → 最新 {}\n\n将下载并自动重启内嵌服务（含 pi-coding-agent）。",
                latest.latest
            ))
            .title(format!("发现 pi-web 新版本 {}", latest.latest))
            .buttons(MessageDialogButtons::OkCancelCustom(
                "更新并重启".into(),
                "以后再说".into(),
            ))
            .blocking_show();
        if !answer {
            // Deferred — leave an actionable CTA the user can trigger later.
            notify_update(
                app,
                Notice {
                    status: "available",
                    title: "发现新版本".into(),
                    message: format!("pi-web {} 可更新", latest.latest),
                    detail: format!("当前 {installed} → {}", latest.latest),
                    action: Some(NoticeAction {
                        id: "apply-update",
                        label: "更新并重启",
                    }),
                },
            );
            return;
        }
    }

    apply_update(app, interactive);
}

pub fn apply_update(app: &AppHandle, interactive: bool) {
    let state = app.state::<AppState>();
    if state.runtime_busy.load(Ordering::SeqCst) {
        return;
    }
    // The `apply_update` command is reachable from ANY 127.0.0.1 page, remote
    // tunnels included. Updating the embedded runtime from a remote host's
    // page is never what the user asked for, so it is refused there.
    if !state.target_is_local() {
        log_dbg!("apply_update ignored — a remote host is in front");
        return;
    }
    let guard = Guard::new(&state.paths);
    let installed_before = guard
        .installed()
        .ok()
        .and_then(|i| i.pi_web)
        .unwrap_or_else(|| "未知".into());

    // The update page only replaces the LOCAL pi-web. A remote host in front
    // keeps being shown; the result notice waits for the switch back.
    if state.target_is_local() {
        show_local(app, "updating.html");
    }

    let outcome = {
        let Some(_lock) = try_runtime_lock(&state, "update") else {
            return;
        };

        // The old server has to be down before the guard runs: the swap renames
        // a directory whose files node still holds open. Electron could keep it
        // serving through the download because the guard called back into the
        // main process; here the call is one-way, so we stop first. Documented
        // in PORTING.md — it costs a longer offline window, never correctness.
        state.stopping_for_update.store(true, Ordering::SeqCst);
        kill_server(app);
        std::thread::sleep(Duration::from_millis(600));

        let progress_app = app.clone();
        let on_progress: crate::guard::ProgressFn =
            Arc::new(move |text: String| show_progress(&progress_app, &text));
        guard.provision(Some("@agegr/pi-web@latest"), "update", on_progress)
    };

    state.stopping_for_update.store(false, Ordering::SeqCst);

    match outcome {
        Ok(installed) => {
            if let Err(e) = start_or_restart_server(app) {
                show_local_error(app, &e, "");
                return;
            }
            let v = installed.pi_web.unwrap_or_default();
            notify_update(
                app,
                Notice {
                    status: "updated",
                    title: "更新完成".into(),
                    message: format!("pi-web 已更新到 {v}"),
                    detail: format!(
                        "{installed_before} → {v}{}",
                        installed
                            .agent
                            .map(|a| format!(" · pi-coding-agent {a}"))
                            .unwrap_or_default()
                    ),
                    action: None,
                },
            );
        }
        Err(e) => {
            log_dbg!("update failed: {e}");
            if interactive {
                app.dialog()
                    .message(tail_chars(&e, 2000))
                    .title("更新失败")
                    .kind(MessageDialogKind::Error)
                    .blocking_show();
            }
            // A failed staging install leaves the live runtime bit-for-bit
            // unchanged, so recovery is just getting a server back in front of
            // the user.
            if let Err(e2) = start_or_restart_server(app) {
                show_local_error(app, &e2, "");
                return;
            }
            notify_update(
                app,
                Notice {
                    status: "error",
                    title: "更新失败".into(),
                    message: "自动更新未完成，当前版本未受影响".into(),
                    detail: "可稍后通过菜单「检查更新…」重试。".into(),
                    action: None,
                },
            );
        }
    }
}

/// A local-runtime failure. Shown in the window only when local is what the
/// user is looking at; otherwise logged, and surfaced when they switch back.
fn show_local_error(app: &AppHandle, message: &str, log: &str) {
    if app.state::<AppState>().target_is_local() {
        show_error(app, message, log);
    } else {
        log_dbg!("local runtime error while a remote host is in front: {message}");
    }
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

/// Recover → migrate → ensure → verify/heal: everything the embedded server
/// needs before its first start. Runs once per process — at boot when booting
/// into local, or on the first switch to local when booting into a remote.
fn prepare_local_runtime(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    if state.local_prepared.load(Ordering::SeqCst) {
        return Ok(());
    }

    // Order matters. Recovery runs before ANY call to runtime_dir(), because
    // an interrupted swap can leave the seed dir momentarily missing and
    // that would poison runtime_dir()'s cached choice for the rest of the
    // session. `recover` gets the candidate list rather than an answer.
    let candidates = state.paths.runtime_candidates();
    {
        // A Guard built here would call runtime_dir() through Paths, so use
        // a candidate-only construction: `recover` ignores `--runtime`.
        let g = Guard::new_for_recovery(&state.paths);
        if let Err(e) = g.recover(&candidates) {
            log_dbg!("recover failed (non-fatal): {e}");
        }
    }

    // Only now may anything ask where the runtime is.
    crate::migrate::run(&state.paths);

    let guard = Guard::new(&state.paths);
    log_dbg!(
        "ensureRuntime; seedDir={} runtimeDir={}",
        state.paths.seed_dir().display(),
        state.paths.runtime_dir().display()
    );
    let v = ensure_runtime(&state, &guard)?;
    log_dbg!("runtime ready v={v}");

    // Preflight: the runtime EXISTS — but does it actually load? Repairs
    // itself if not, so a torn install no longer surfaces as an opaque
    // "server not ready in time" sixty seconds later.
    ensure_runtime_healthy(app, &guard)?;
    state.local_prepared.store(true, Ordering::SeqCst);
    Ok(())
}

pub fn boot(app: AppHandle) {
    let state = app.state::<AppState>();
    menu_sync(&app);
    sync_title(&app);

    let target = state.target();
    if target != LOCAL_ID {
        // Booting straight into a remote host: the embedded server is started
        // lazily, on the first switch to local (keepLocalRunning only keeps an
        // ALREADY-running server alive — it never forces one up).
        log_dbg!("boot: active host is {target} — local runtime deferred");
        let _switch = state.switch_lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = connect_remote(&app, &target) {
            log_dbg!("boot: remote connect to {target} FAILED: {}", e.0);
            show_error(&app, &e.0, &e.1);
        }
        // The auto update check is about the embedded runtime, which is not
        // running; it happens on a later launch that boots into local.
        return;
    }

    let result = {
        let _switch = state.switch_lock.lock().unwrap_or_else(|e| e.into_inner());
        prepare_local_runtime(&app).and_then(|()| start_or_restart_server(&app))
    };

    if let Err(e) = result {
        log_dbg!("BOOT ERROR {e}");
        let log = state
            .server()
            .map(|s| s.log_tail(3000))
            .unwrap_or_default();
        show_error(&app, &e, &log);
        return;
    }
    log_dbg!("start_or_restart_server returned ok");

    if std::env::var("PI_WEB_AUTO_UPDATE_CHECK").as_deref() != Ok("0") {
        std::thread::sleep(Duration::from_secs(5));
        let last = state.last_provision_ms.load(Ordering::SeqCst);
        // Skip when a self-heal just reinstalled the runtime: the user has
        // already waited through one install, and an upgrade can wait for the
        // next launch. (The lock would serialize them anyway — this is about
        // not making them sit through two in a row.)
        if last != 0 && now_ms().saturating_sub(last) < 120_000 {
            log_dbg!("auto update check skipped — runtime was just provisioned");
            return;
        }
        // The silent auto-update stops and restarts the embedded server. Only
        // do that while the user is on local: if they switched to a remote
        // host in these five seconds, an install + restart behind their back
        // is exactly the interruption keepLocalRunning exists to avoid.
        if !state.target_is_local() || state.server().is_none() {
            log_dbg!("auto update check skipped — a remote host is in front");
            return;
        }
        check_for_updates(&app, false);
    }
}

// ---------------------------------------------------------------------------
// Host switching
// ---------------------------------------------------------------------------

/// (message, log) for error.html.
type SwitchError = (String, String);

fn take_remote(state: &AppState) -> Option<Arc<RemoteSession>> {
    state.remote.lock().unwrap_or_else(|e| e.into_inner()).take()
}

pub fn kill_remote(app: &AppHandle) {
    if let Some(r) = take_remote(&app.state::<AppState>()) {
        r.stop();
    }
}

struct ConnectingPortGuard<'a> { state: &'a AppState, port: u16 }
impl<'a> ConnectingPortGuard<'a> {
    fn enter(state: &'a AppState, port: u16) -> Self { state.begin_session_connect(port); Self { state, port } }
}
impl Drop for ConnectingPortGuard<'_> {
    fn drop(&mut self) { self.state.end_session_connect(self.port); }
}

pub fn kill_all(app: &AppHandle) {
    let state = app.state::<AppState>();
    let probe = state.port_coord.lock().unwrap_or_else(|e| e.into_inner()).probe.take();
    if let Some(gate) = probe { gate.preempt_and_stop(); }
    kill_remote(app);
    kill_server(app);
}

/// Bring up the tunnel for `id`, authenticate, and navigate. Caller holds
/// `switch_lock` and has already set the target.
fn connect_remote(app: &AppHandle, id: &str) -> Result<(), SwitchError> {
    let state = app.state::<AppState>();
    let Some(host) = state.hosts().find(id).cloned() else {
        return Err((
            format!("hosts.json 中没有 id 为「{id}」的主机"),
            String::new(),
        ));
    };
    let _connecting = ConnectingPortGuard::enter(&state, host.local_port);

    // The "connecting" page, so the previous host's UI is never left on
    // screen — and clickable — while this one comes up.
    show_local(app, "index.html");
    set_status(app, &format!("正在连接 {}…", host.label), &format!(
        "SSH 隧道 {} → 127.0.0.1:{}",
        host.ssh_target, host.local_port
    ));

    let exit_app = app.clone();
    let session = RemoteSession::start(&host, &state.job, move |serial, code, log| {
        on_remote_exit(&exit_app, serial, code, log)
    })
    .map_err(|e| (e, String::new()))?;
    log_dbg!(
        "remote[{}]: ssh pid {} (job object: {})",
        host.id,
        session.pid(),
        if session.in_job() { "yes" } else { "NO" }
    );

    let auth_required = match session.wait_ready(remote::READY_TIMEOUT) {
        Ok(a) => a,
        Err(e) => {
            let log = session.log_tail(4000);
            session.stop();
            return Err((e, log));
        }
    };
    if auth_required {
        match session.login() {
            Ok(true) => log_dbg!("remote[{}]: authenticated", host.id),
            Ok(false) => {
                let log = session.log_tail(4000);
                session.stop();
                return Err((
                    format!(
                        "远端「{}」的 pi-web 设了口令，但 hosts.json 里没有配置。\n\n\
                         请在该主机条目里设置 passwordEnv（推荐，指向一个用户环境变量）或 password，\
                         然后用「主机 → 管理远程主机…」保存或重新读取配置。",
                        host.label
                    ),
                    log,
                ));
            }
            Err(e) => {
                let log = session.log_tail(4000);
                session.stop();
                return Err((e, log));
            }
        }
    } else {
        log_dbg!("remote[{}]: remote requires no password", host.id);
    }

    *state.remote.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(session));
    show_pi_web(app);
    Ok(())
}

/// Write a line into the connecting page (index.html), best-effort. The DOM is
/// touched directly from here because that page's CSP allows no script of its
/// own; `eval` runs through the webview host and is not subject to it.
fn set_status(app: &AppHandle, title: &str, sub: &str) {
    let script = format!(
        "(function(){{var n=0,f=function(){{var t=document.querySelector('.title'),s=document.querySelector('.sub');\
         if(t&&s&&document.querySelector('meta[name=piwd-shell]')){{t.textContent={};s.textContent={}}}else if(n++<60)setTimeout(f,50)}};f()}})()",
        json_string(title),
        json_string(sub)
    );
    if let Some(win) = main_window(app) {
        let _ = win.eval(&script);
    }
}

/// The tunnel died on its own (network drop, remote reboot, sshd restart).
fn on_remote_exit(app: &AppHandle, serial: u64, code: Option<i32>, log: String) {
    let state = app.state::<AppState>();
    if state.is_quitting() {
        return;
    }
    // Only the CURRENT session matters; a retired one reporting late does not.
    let current = state.remote().filter(|r| r.serial == serial);
    let Some(session) = current else { return };
    let still_active = session.host.id == state.target();
    // Drop it so a reconnect starts clean and is_internal_url stops trusting it.
    {
        let mut guard = state.remote.lock().unwrap_or_else(|e| e.into_inner());
        if guard.as_ref().map(|r| r.serial) == Some(serial) {
            guard.take();
        }
    }
    if !still_active {
        return;
    }
    let message = format!(
        "与「{}」的 SSH 隧道意外断开（ssh code {code:?}）。\n\n{}",
        session.host.label,
        remote::diagnose(&session.host, &log)
    );
    let app = app.clone();
    // Off the watchdog thread: show_error navigates and evals.
    std::thread::spawn(move || show_error(&app, &message, &log));
}

/// Make `id` (`LOCAL_ID` or a host id) the backend in the window.
///
/// Ordering, so the user never sees or clicks the wrong machine:
///   1. target + title + menu tick switch at once (immediate feedback);
///   2. the window goes to the local "connecting" page;
///   3. the backend is brought up and authenticated;
///   4. only then does the window navigate to it.
pub fn switch_to(app: &AppHandle, id: &str) {
    let state = app.state::<AppState>();
    let _switch = state.switch_lock.lock().unwrap_or_else(|e| e.into_inner());
    switch_to_locked(app, id);
}

fn switch_to_locked(app: &AppHandle, id: &str) {
    let state = app.state::<AppState>();
    let previous = state.target();
    if previous == id && state.active_backend().is_some() {
        log_dbg!("switch: already on {id}");
        menu_sync(app);
        return;
    }
    if id != LOCAL_ID && state.hosts().find(id).is_none() {
        log_dbg!("switch: unknown host {id}");
        menu_sync(app);
        return;
    }
    log_dbg!("switch: {previous} -> {id}");
    state.bump_switch_epoch();
    state.set_target(id);
    sync_title(app);
    menu_sync(app);

    // Leaving a remote host always closes its tunnel — an idle SSH session
    // per host would just be more things to go wrong.
    kill_remote(app);

    let keep_local = state.hosts().keep_local_running;
    let result: Result<(), SwitchError> = if id == LOCAL_ID {
        switch_to_local(app)
    } else {
        if !keep_local && state.server().is_some() {
            log_dbg!("switch: keepLocalRunning=false — stopping the embedded server");
            state.restarting.store(true, Ordering::SeqCst);
            kill_server(app);
            state.restarting.store(false, Ordering::SeqCst);
        }
        connect_remote(app, id)
    };

    match result {
        Ok(()) => {
            if let Err(e) = hosts::save_active(state.paths.user_data_dir(), id) {
                log_dbg!("hosts: could not persist active={id}: {e}");
            }
        }
        Err((message, log)) => {
            log_dbg!("switch to {id} FAILED: {message}");
            show_error(app, &message, &log);
        }
    }
    menu_sync(app);
}

fn switch_to_local(app: &AppHandle) -> Result<(), SwitchError> {
    let state = app.state::<AppState>();
    if state.server().is_some() {
        // Kept running in the background: back in one navigation.
        show_pi_web(app);
        return Ok(());
    }
    show_local(app, "index.html");
    let local_log = |state: &AppState| state.server().map(|s| s.log_tail(3000)).unwrap_or_default();
    prepare_local_runtime(app).map_err(|e| (e, String::new()))?;
    start_or_restart_server(app).map_err(|e| (e, local_log(&state)))
}

/// 「重启内嵌服务」 / 「重连当前远端」: restart the local server, or rebuild
/// the tunnel for the current host and log in again.
pub fn restart_current(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let _switch = state.switch_lock.lock().unwrap_or_else(|e| e.into_inner());
    restart_current_locked(app)
}

fn restart_current_locked(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.bump_switch_epoch();
    let target = state.target();
    let result = if target == LOCAL_ID {
        prepare_local_runtime(app).and_then(|()| start_or_restart_server(app))
    } else {
        kill_remote(app);
        connect_remote(app, &target).map_err(|(message, log)| {
            show_error(app, &message, &log);
            message
        })
    };
    menu_sync(app);
    result
}

/// 「重新读取 hosts.json」: reload the list and rebuild the menu under
/// `switch_lock`. When the current remote host disappeared or its settings
/// diverged from the live tunnel, fall back to / reconnect accordingly.
pub fn reload_hosts(app: &AppHandle) {
    let state = app.state::<AppState>();
    let _switch = state.switch_lock.lock().unwrap_or_else(|e| e.into_inner());
    state.bump_switch_epoch();

    let fresh = hosts::load(state.paths.user_data_dir());
    let warnings = fresh.warnings.clone();
    let keep_local = fresh.keep_local_running;
    let target = state.target();
    let new_host = fresh.find(&target).cloned();
    *state.hosts.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
    crate::menu::rebuild(app);
    sync_title(app);

    if !warnings.is_empty() {
        let app2 = app.clone();
        std::thread::spawn(move || {
            app2.dialog()
                .message(format!("以下条目有问题，已跳过：\n\n{}", warnings.join("\n")))
                .title("hosts.json")
                .kind(MessageDialogKind::Warning)
                .blocking_show();
        });
    }

    if target == LOCAL_ID {
        if state.server().is_none() { switch_to_locked(app, LOCAL_ID); }
        return;
    }

    match new_host {
        None => {
            log_dbg!("hosts: current host {target} is gone — back to local");
            switch_to_locked(app, LOCAL_ID);
        }
        Some(h) => {
            if !keep_local && state.server().is_some() {
                log_dbg!("hosts: keepLocalRunning=false — stopping background embedded server");
                state.restarting.store(true, Ordering::SeqCst);
                kill_server(app);
                state.restarting.store(false, Ordering::SeqCst);
            }
            let live_matches = state.remote().map(|r| r.host == h).unwrap_or(false);
            if !live_matches {
                log_dbg!("hosts: active remote {target} changed or is disconnected — reconnecting");
                let _ = restart_current_locked(app);
            }
        }
    }
}

/// Atomic Host Manager save + hot-reload transaction.
///
/// Holds `switch_lock` across reading `target`, writing `hosts.json`, updating
/// `AppState`, rebuilding the menu, and tearing down any invalidated tunnel or
/// background local server — so a concurrent menu switch can never clobber the
/// saved file via `save_active`, and a renamed/deleted active host never leaves
/// `RemoteSession.host.id` out of sync with `target`. Any slow network
/// reconnect runs afterwards in an epoch-checked worker under `switch_lock`.
pub fn save_and_reload_hosts(
    app: &AppHandle,
    keep_local_running: bool,
    drafts: &[hosts::HostDraft],
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let _switch = state.switch_lock.lock().unwrap_or_else(|e| e.into_inner());

    let previous_target = state.target();
    let resolved_active = hosts::save_config(
        state.paths.user_data_dir(),
        keep_local_running,
        drafts,
        &previous_target,
    )?;

    let fresh = hosts::load(state.paths.user_data_dir());
    let new_host = fresh.find(&resolved_active).cloned();
    *state.hosts.lock().unwrap_or_else(|e| e.into_inner()) = fresh;

    state.set_target(&resolved_active);
    let epoch = state.bump_switch_epoch();

    let mut deferred_action: Option<String> = None;

    if resolved_active == LOCAL_ID {
        if previous_target != LOCAL_ID {
            log_dbg!("hosts_save: active remote {previous_target} deleted — switching back to local");
            kill_remote(app);
            if state.server().is_some() { show_pi_web(app); }
        }
        if state.server().is_none() {
            show_local(app, "index.html");
            deferred_action = Some(LOCAL_ID.to_owned());
        }
    } else {
        if !keep_local_running && state.server().is_some() {
            log_dbg!("hosts_save: keepLocalRunning=false — stopping background embedded server");
            state.restarting.store(true, Ordering::SeqCst);
            kill_server(app);
            state.restarting.store(false, Ordering::SeqCst);
        }
        // Compare against the LIVE tunnel's `HostConfig` (including its `id`),
        // so renaming `a -> b` with identical connection parameters still
        // rebuilds the session under the new identity `b`.
        let live_matches = new_host
            .as_ref()
            .and_then(|h| state.remote().map(|r| &r.host == h))
            .unwrap_or(false);
        if !live_matches {
            log_dbg!(
                "hosts_save: active remote {resolved_active} changed (was {previous_target}) — scheduling reconnect"
            );
            kill_remote(app);
            show_local(app, "index.html");
            deferred_action = Some(resolved_active.clone());
        }
    }

    crate::menu::rebuild(app);
    sync_title(app);

    if let Some(goal) = deferred_action {
        let app_bg = app.clone();
        std::thread::spawn(move || {
            let st = app_bg.state::<AppState>();
            let _sw = st.switch_lock.lock().unwrap_or_else(|e| e.into_inner());
            if st.switch_epoch.load(Ordering::SeqCst) != epoch {
                log_dbg!("hosts_save deferred connect to {goal} superseded by newer switch");
                return;
            }
            let res: Result<(), SwitchError> = if goal == LOCAL_ID {
                switch_to_local(&app_bg)
            } else {
                connect_remote(&app_bg, &goal)
            };
            if let Err((message, log)) = res {
                log_dbg!("hosts_save deferred connect to {goal} FAILED: {message}");
                show_error(&app_bg, &message, &log);
            }
            menu_sync(&app_bg);
        });
    }

    Ok(())
}

/// Push the current state into the menu: the tick, the remote-only labels,
/// and 检查更新 being unavailable while a remote host is in front.
pub fn menu_sync(app: &AppHandle) {
    crate::menu::sync(app);
}
