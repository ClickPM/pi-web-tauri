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
use crate::job::ProcessJob;
use crate::log_dbg;
use crate::paths::Paths;
use crate::server::{self, Server};

pub const MAIN_WINDOW: &str = "main";

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

pub struct AppState {
    pub paths: Paths,
    pub job: ProcessJob,
    server: Mutex<Option<Arc<Server>>>,
    /// Base URL of the bundled status pages, captured once at window creation
    /// (Windows serves them from http://tauri.localhost/).
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
    last_known_latest: Mutex<Option<String>>,
}

impl AppState {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            job: ProcessJob::create(),
            server: Mutex::new(None),
            local_base: Mutex::new(None),
            runtime_busy: AtomicBool::new(false),
            last_provision_ms: AtomicU64::new(0),
            quitting: AtomicBool::new(false),
            restarting: AtomicBool::new(false),
            stopping_for_update: AtomicBool::new(false),
            password_disabled: AtomicBool::new(false),
            pending_notice: Mutex::new(None),
            last_known_latest: Mutex::new(None),
        }
    }

    pub fn server(&self) -> Option<Arc<Server>> {
        self.server
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_local_base(&self, url: url::Url) {
        *self.local_base.lock().unwrap_or_else(|e| e.into_inner()) = Some(url);
    }

    /// Record the base from a finished page load, but only from one of OUR
    /// pages — capturing the pi-web origin here would make every later
    /// `show_local` navigate back into the very server we are about to stop.
    pub fn capture_local_base(&self, url: &url::Url) {
        let is_ours = self
            .server()
            .map(|s| !url.as_str().starts_with(&s.origin()))
            .unwrap_or(true);
        if !is_ours {
            return;
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

pub fn main_window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window(MAIN_WINDOW)
}

fn show_local(app: &AppHandle, page: &str) {
    let state = app.state::<AppState>();
    let Some(url) = state.local_page(page) else {
        log_dbg!("show_local({page}): no local base captured yet");
        return;
    };
    if let Some(win) = main_window(app) {
        if let Err(e) = win.navigate(url) {
            log_dbg!("show_local({page}) failed: {e}");
        }
    }
}

fn show_pi_web(app: &AppHandle) {
    let state = app.state::<AppState>();
    let Some(server) = state.server() else { return };
    let Ok(url) = server.nav_url().parse::<url::Url>() else {
        return;
    };
    if let Some(win) = main_window(app) {
        if let Err(e) = win.navigate(url) {
            log_dbg!("navigate to pi-web failed: {e}");
        }
    }
}

pub fn show_error(app: &AppHandle, message: &str, log: &str) {
    show_local(app, "error.html");
    let script = format!(
        "(function(){{var f=function(){{if(window.__setError)window.__setError({},{});else setTimeout(f,60)}};f()}})()",
        json_string(message),
        json_string(log)
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
        let _ = win.eval(&format!(
            "window.__piwdProgress&&window.__piwdProgress({})",
            json_string(text)
        ));
    }
}

// ---------------------------------------------------------------------------
// Update-result CTA
// ---------------------------------------------------------------------------

/// Install the session cookie in the webview, once per server instance.
///
/// Called on the first finished load of the pi-web origin. The page is reloaded
/// afterwards so pi-web's own auth check re-runs with the cookie present —
/// without that it has already decided it is logged out and rendered its login
/// form. The reload fires another page-load, which the flag stops from looping.
pub fn inject_session_cookie(app: &AppHandle) {
    let state = app.state::<AppState>();
    let Some(server) = state.server() else { return };
    let Some(win) = main_window(app) else { return };

    let on_pi_web = win
        .url()
        .map(|u| u.as_str().starts_with(&server.origin()))
        .unwrap_or(false);
    if !on_pi_web {
        return;
    }
    let Some(cookie) = server.session_cookie() else { return };
    if server
        .auth_injected
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    // `path=/` and no expiry: a session cookie for this origin, exactly as the
    // server set it. SameSite=Lax matches a same-origin-only app.
    let script = format!(
        "document.cookie={}+\"; path=/; SameSite=Lax\";location.reload()",
        json_string(&cookie)
    );
    match win.eval(&script) {
        Ok(()) => log_dbg!("session cookie installed in the webview"),
        Err(e) => log_dbg!("could not install the session cookie: {e}"),
    }
}

/// Deliver the queued CTA once a pi-web page is present. Called after every
/// page load, because a successful update reloads the page before we can report.
pub fn flush_update_notice(app: &AppHandle) {
    let state = app.state::<AppState>();
    let Some(server) = state.server() else { return };
    let Some(win) = main_window(app) else { return };

    let on_pi_web = win
        .url()
        .map(|u| u.as_str().starts_with(&server.origin()))
        .unwrap_or(false);
    if !on_pi_web {
        return;
    }

    let notice = {
        let mut guard = state.pending_notice.lock().unwrap_or_else(|e| e.into_inner());
        guard.take()
    };
    let Some(notice) = notice else { return };
    let payload = serde_json::to_string(&notice).unwrap_or_else(|_| "null".into());
    if let Err(e) = win.eval(&format!("window.__piwdNotice&&window.__piwdNotice({payload})")) {
        log_dbg!("flush_update_notice failed: {e}");
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
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
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
        server.wait_ready(Duration::from_secs(90))?;

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
                return Err(LOGIN_FAILED.into());
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
            show_pi_web(app);
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

    {
        let mut last = state.last_known_latest.lock().unwrap_or_else(|e| e.into_inner());
        *last = Some(latest.latest.clone());
    }

    let installed = latest.installed.clone().unwrap_or_else(|| "未知".into());
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
    let guard = Guard::new(&state.paths);
    let installed_before = guard
        .installed()
        .ok()
        .and_then(|i| i.pi_web)
        .unwrap_or_else(|| "未知".into());

    show_local(app, "updating.html");

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
                show_error(app, &e, "");
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
                show_error(app, &e2, "");
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

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

pub fn boot(app: AppHandle) {
    let result = (|| -> Result<(), String> {
        let state = app.state::<AppState>();

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
        ensure_runtime_healthy(&app, &guard)?;
        start_or_restart_server(&app)?;
        log_dbg!("start_or_restart_server returned ok");
        Ok(())
    })();

    if let Err(e) = result {
        log_dbg!("BOOT ERROR {e}");
        let log = {
            let state = app.state::<AppState>();
            state
                .server()
                .map(|s| s.log_tail(3000))
                .unwrap_or_default()
        };
        show_error(&app, &e, &log);
        return;
    }

    if std::env::var("PI_WEB_AUTO_UPDATE_CHECK").as_deref() != Ok("0") {
        std::thread::sleep(Duration::from_secs(5));
        let state = app.state::<AppState>();
        let last = state.last_provision_ms.load(Ordering::SeqCst);
        // Skip when a self-heal just reinstalled the runtime: the user has
        // already waited through one install, and an upgrade can wait for the
        // next launch. (The lock would serialize them anyway — this is about
        // not making them sit through two in a row.)
        if last != 0 && now_ms().saturating_sub(last) < 120_000 {
            log_dbg!("auto update check skipped — runtime was just provisioned");
            return;
        }
        check_for_updates(&app, false);
    }
}
