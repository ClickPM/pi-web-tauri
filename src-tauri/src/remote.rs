//! A remote pi-web reached through a shell-owned SSH local port forward.
//!
//! The remote pi-web only listens on ITS OWN loopback (`127.0.0.1:30141` by
//! default). The shell runs the Windows OpenSSH client with `-N -L`, which binds
//! `127.0.0.1:<localPort>` here and carries every connection over the encrypted
//! SSH channel. Consequences, all deliberate:
//!
//!   * nothing on the LAN ever sees pi-web in clear text, and the ACL in
//!     `capabilities/remote.json` stays at `http://127.0.0.1:*` — the webview
//!     only ever talks to loopback;
//!   * each host gets a fixed `localPort` in 45000..=48999, i.e. its own origin
//!     and therefore its own persistent `localStorage` (see `hosts.rs`);
//!   * `ssh.exe` is spawned with `.no_console()` and put in the shell's job
//!     object, so End Task on the shell takes the tunnel with it instead of
//!     leaving an orphan that holds the port until the next reboot.
//!
//! WHICH ssh: `C:\Windows\System32\OpenSSH\ssh.exe`, never Git-for-Windows' or
//! MSYS2's. Those resolve `~/.ssh` through their own HOME emulation, so config,
//! keys and host aliases silently go missing (`ssh 130` even ends up as
//! `0.0.0.130`). PATH is only a fallback, and MSYS-looking entries are skipped.

use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::hosts::HostConfig;
use crate::job::ProcessJob;
use crate::log_dbg;
use crate::proc::NoConsole;
use crate::server::{self, PiWebApiProbe, WebAuthProbe};

const LOG_CAP: usize = 20_000;
pub const READY_TIMEOUT: Duration = Duration::from_secs(12);

static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);

pub struct RemoteSession {
    pub host: HostConfig,
    /// Unique per session, so a late exit report from a retired tunnel can
    /// never be mistaken for the current one going down.
    pub serial: u64,
    child: Arc<Mutex<Option<Child>>>,
    pid: u32,
    log: Arc<Mutex<String>>,
    in_job: bool,
    /// Set by `stop()` BEFORE the kill, so the watchdog knows the exit was asked for.
    retired: Arc<AtomicBool>,
    /// `pi_web_session=<token>` from the remote's `/api/web-auth`.
    cookie: Mutex<Option<String>>,
}

impl RemoteSession {
    /// Spawn the tunnel. Returns as soon as ssh.exe is running; call
    /// `wait_ready()` to find out whether it actually got anywhere.
    ///
    /// `on_exit(code, stderr_tail)` fires from the watchdog when ssh.exe dies
    /// on its own — never after `stop()`.
    pub fn start(
        host: &HostConfig,
        job: &ProcessJob,
        on_exit: impl Fn(u64, Option<i32>, String) + Send + 'static,
    ) -> Result<Self, String> {
        let port = host.local_port;
        if !server::is_free(port) {
            return Err(format!(
                "本地端口 127.0.0.1:{port} 已被占用，无法建立到「{}」的隧道。\n\
                 请关闭占用它的程序（PowerShell: Get-NetTCPConnection -LocalPort {port}），\
                 或在 hosts.json 中为该主机换一个 45000-48999 之间的 localPort。",
                host.label
            ));
        }

        let ssh = ssh_exe()?;
        let args = ssh_args(host);
        let serial = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
        log_dbg!(
            "remote[{}#{serial}]: {} {}",
            host.id,
            ssh.display(),
            args.join(" ")
        );

        let mut cmd = Command::new(&ssh);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .no_console();
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("无法启动 {}：{e}", ssh.display()))?;

        // Into the job before anything else can go wrong, so a crash of the
        // shell from here on cannot orphan the tunnel.
        let in_job = job.assign(&child);
        let pid = child.id();

        let log = Arc::new(Mutex::new(String::new()));
        if let Some(err) = child.stderr.take() {
            spawn_stderr_reader(err, log.clone(), format!("{}#{serial}", host.id));
        }

        let child = Arc::new(Mutex::new(Some(child)));
        let retired = Arc::new(AtomicBool::new(false));
        spawn_watchdog(
            child.clone(),
            log.clone(),
            retired.clone(),
            serial,
            host.id.clone(),
            on_exit,
        );

        Ok(Self {
            host: host.clone(),
            serial,
            child,
            pid,
            log,
            in_job,
            retired,
            cookie: Mutex::new(None),
        })
    }

    pub fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.host.local_port)
    }

    /// Plain URL, no embedded credentials — same reasoning as `Server::nav_url`.
    pub fn nav_url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.host.local_port)
    }

    pub fn session_cookie(&self) -> Option<String> {
        self.cookie
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn in_job(&self) -> bool {
        self.in_job
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn log_tail(&self, n: usize) -> String {
        tail(&self.log.lock().unwrap_or_else(|e| e.into_inner()), n)
    }

    fn exited(&self) -> Option<Option<i32>> {
        let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(c) => match c.try_wait() {
                Ok(Some(status)) => Some(status.code()),
                _ => None,
            },
            None => Some(None),
        }
    }

    pub fn is_retired(&self) -> bool {
        self.retired.load(Ordering::SeqCst)
    }

    /// Poll the tunnel's local end until the remote pi-web answers.
    ///
    /// Only genuine pi-web entry responses count: `200` (further checked via
    /// `/api/sessions` when open), `401`, and a `3xx` redirect to `/login`.
    /// Returns whether the remote demanded authentication.
    ///
    /// Gives up immediately when ssh.exe exits (bad key, unknown host, refused
    /// forward …) and reports its stderr, rather than sitting out the timeout.
    pub fn wait_ready(&self, timeout: Duration) -> Result<bool, String> {
        let started = Instant::now();
        let port = self.host.local_port;
        let mut connect_failures_seen = 0usize;
        while started.elapsed() < timeout {
            if let Some(code) = self.exited() {
                // Give the stderr reader a moment to drain the last lines.
                std::thread::sleep(Duration::from_millis(150));
                return Err(self.failure(&format!("ssh 已退出（code {code:?}）")));
            }
            if let Some(head) = server::http_head(port) {
                match classify_root_http(&head, port) {
                    Some(RootHttpVerdict::AuthRequired { status, .. }) => {
                        log_dbg!(
                            "remote[{}]: ready after {:?} (HTTP {status}, auth required)",
                            self.host.id,
                            started.elapsed()
                        );
                        return Ok(true);
                    }
                    Some(RootHttpVerdict::Candidate200) => match server::probe_pi_web_api(port) {
                        PiWebApiProbe::OpenPiWeb => {
                            log_dbg!(
                                "remote[{}]: ready after {:?} (HTTP 200, open pi-web)",
                                self.host.id,
                                started.elapsed()
                            );
                            return Ok(false);
                        }
                        PiWebApiProbe::AuthRequired => {
                            log_dbg!(
                                "remote[{}]: ready after {:?} (HTTP 200 + API 401, auth required)",
                                self.host.id,
                                started.elapsed()
                            );
                            return Ok(true);
                        }
                        PiWebApiProbe::NotPiWeb(reason) => {
                            return Err(self.failure(&format!(
                                "远端 {}:{} 虽有 HTTP 响应但并非 pi-web（{reason}）",
                                self.host.remote_host, self.host.remote_port
                            )));
                        }
                    },
                    Some(RootHttpVerdict::UnexpectedStatus(s)) if (400..500).contains(&s) => {
                        return Err(self.failure(&format!(
                            "远端 {}:{} 返回 HTTP {s}（并非预期的 pi-web 服务入口）",
                            self.host.remote_host, self.host.remote_port
                        )));
                    }
                    _ => {}
                }
            }
            // The tunnel is up but the remote end refused: nothing listens on
            // remoteHost:remotePort over there. Seen twice so a transient
            // restart of the remote service does not fail the switch outright.
            let refused = self
                .log
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .matches("connect failed")
                .count();
            if refused >= 2 && refused > connect_failures_seen {
                connect_failures_seen = refused;
                if started.elapsed() > Duration::from_secs(3) {
                    return Err(self.failure(&format!(
                        "SSH 隧道已建立，但远端 {}:{} 没有 pi-web 在监听",
                        self.host.remote_host, self.host.remote_port
                    )));
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Err(self.failure(&format!(
            "{} 秒内未能通过隧道连上远端 pi-web",
            timeout.as_secs()
        )))
    }

    /// Trade the configured password for a session cookie, through the tunnel.
    /// Ok(false) = no password configured, nothing to do.
    pub fn login(&self) -> Result<bool, String> {
        let Some(password) = self.host.password.as_deref() else {
            return Ok(false);
        };
        let cookie = server::with_retries(3, || server::web_auth_login(self.host.local_port, password))
            .map_err(|e| {
                format!(
                    "远端 pi-web 拒绝了口令登录：{e}\n\n请确认 hosts.json 中「{}」的 passwordEnv / password \
                     与远端 PI_WEB_PASSWORD 一致（远端 pi-web 需 ≥ 0.9.1 才有 /api/web-auth）。",
                    self.host.label
                )
            })?;
        *self.cookie.lock().unwrap_or_else(|e| e.into_inner()) = Some(cookie);
        Ok(true)
    }

    /// The error text for a failed connect: what happened, ssh's own words, and
    /// what to try.
    pub fn failure(&self, what: &str) -> String {
        let log = self.log_tail(4000);
        format!(
            "无法连接到「{}」：{what}。\n\n{}",
            self.host.label,
            diagnose(&self.host, &log)
        )
    }

    /// Kill ssh.exe and wait until the local port is free again, so an
    /// immediate reconnect does not trip over our own previous tunnel.
    pub fn stop(&self) {
        self.retired.store(true, Ordering::SeqCst);
        let taken = {
            let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };
        let Some(mut child) = taken else { return };
        log_dbg!("remote[{}]: stopping ssh pid {}", self.host.id, self.pid);
        let _ = child.kill();
        let _ = child.wait();

        let port = self.host.local_port;
        let deadline = Instant::now() + Duration::from_secs(3);
        while !server::is_free(port) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        log_dbg!(
            "remote[{}]: ssh pid {} stopped (port {port} {})",
            self.host.id,
            self.pid,
            if server::is_free(port) { "released" } else { "STILL BUSY" }
        );
    }
}

fn status_of(head: &str) -> Option<u16> {
    head.lines().next()?.split_whitespace().nth(1)?.parse().ok()
}

fn redirects_to_login(head: &str, port: u16) -> bool {
    server::is_same_origin_login_redirect(head, port)
}

/// The argument vector, separated out so it can be unit-tested.
pub fn ssh_args(host: &HostConfig) -> Vec<String> {
    let mut args: Vec<String> = [
        "-N",
        "-o",
        "BatchMode=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ConnectTimeout=8",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push("-L".into());
    // IPv6 literals need brackets in a forward spec.
    let remote = if host.remote_host.contains(':') && !host.remote_host.starts_with('[') {
        format!("[{}]", host.remote_host)
    } else {
        host.remote_host.clone()
    };
    args.push(format!(
        "127.0.0.1:{}:{}:{}",
        host.local_port, remote, host.remote_port
    ));
    if let Some(p) = host.ssh_port {
        args.push("-p".into());
        args.push(p.to_string());
    }
    // `--` ends option parsing: belt and braces with hosts.rs refusing any
    // sshTarget that starts with '-'.
    args.push("--".into());
    args.push(host.ssh_target.clone());
    args
}

/// Resolve the Windows OpenSSH client. See the module docs for why the
/// System32 copy is preferred and MSYS/Git copies are refused.
#[cfg(windows)]
pub fn ssh_exe() -> Result<PathBuf, String> {
    let root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("windir"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let system = root.join("System32").join("OpenSSH").join("ssh.exe");
    if system.is_file() {
        return Ok(system);
    }

    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("ssh.exe");
        if !candidate.is_file() {
            continue;
        }
        if is_msys_like(&candidate) {
            log_dbg!("ssh: skipping MSYS/Git ssh at {}", candidate.display());
            continue;
        }
        return Ok(candidate);
    }
    Err(format!(
        "找不到 Windows 自带的 OpenSSH 客户端（{}）。\n\
         请在「设置 → 系统 → 可选功能」中添加「OpenSSH 客户端」。\
         Git Bash / MSYS 自带的 ssh 会解析坏 ~/.ssh 与主机别名，不会被使用。",
        system.display()
    ))
}

#[cfg(not(windows))]
pub fn ssh_exe() -> Result<PathBuf, String> {
    Ok(PathBuf::from("ssh"))
}

/// Git for Windows, MSYS2 and Cygwin all ship their own ssh.exe whose HOME
/// emulation breaks `~/.ssh`. Recognised by path, which is what they share.
#[cfg(any(windows, test))]
pub fn is_msys_like(path: &std::path::Path) -> bool {
    let p = path.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
    ["\\git\\", "msys", "mingw", "cygwin", "\\usr\\bin\\"]
        .iter()
        .any(|needle| p.contains(needle))
}

/// Turn ssh's stderr into "here is what it said, and what to do about it".
pub fn diagnose(host: &HostConfig, log: &str) -> String {
    let lower = log.to_ascii_lowercase();
    let mut tips: Vec<String> = Vec::new();
    let manual = format!(
        "C:\\Windows\\System32\\OpenSSH\\ssh.exe {}{}",
        host.ssh_port.map(|p| format!("-p {p} ")).unwrap_or_default(),
        host.ssh_target
    );

    if lower.contains("permission denied") {
        tips.push(format!(
            "SSH 认证失败。外壳以 BatchMode 运行、不会弹密码框，必须是密钥免密登录。\
             先在 PowerShell 里手动跑一次：{manual}"
        ));
    }
    if lower.contains("host key verification failed") || lower.contains("remote host identification has changed") {
        tips.push(format!(
            "主机指纹未被信任或已变化。请手动执行一次 {manual} 并确认指纹（known_hosts 在 %USERPROFILE%\\.ssh）。"
        ));
    }
    if lower.contains("could not resolve hostname") {
        tips.push(format!(
            "无法解析「{}」。确认 %USERPROFILE%\\.ssh\\config 里有这个 Host 别名，或直接写 user@IP。",
            host.ssh_target
        ));
    }
    if lower.contains("timed out") || lower.contains("no route to host") || lower.contains("network is unreachable") {
        tips.push("连接超时 / 网络不可达：确认主机已开机、与本机在同一局域网、22 端口未被防火墙拦截。".into());
    }
    if lower.contains("connection refused") && !lower.contains("connect failed") {
        tips.push("SSH 端口拒绝连接：确认远端 sshd 已启动（macOS：系统设置 → 通用 → 共享 → 远程登录）。".into());
    }
    if lower.contains("connect failed") || lower.contains("open failed") {
        tips.push(format!(
            "隧道已通，但远端 {}:{} 无服务。Arch：systemctl --user status pi-web；\
             macOS：launchctl print gui/$(id -u)/com.agegr.piweb。模板见 scripts/remote/。",
            host.remote_host, host.remote_port
        ));
    }
    if lower.contains("address already in use") || lower.contains("cannot listen to port") {
        tips.push(format!(
            "本地端口 {} 被占用：换一个 45000-48999 之间的 localPort。",
            host.local_port
        ));
    }
    if tips.is_empty() {
        tips.push(format!("可先在 PowerShell 手动验证：{manual}"));
    }
    tips.push("可随时从菜单「主机 → Windows 本地」(Ctrl+Alt+1) 切回本机，或用「App → 重连当前远端」重试。".into());

    let mut out = String::from("排障提示：\n");
    for t in tips {
        out.push_str("  • ");
        out.push_str(&t);
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Non-destructive 3-step connection diagnostic (Host Manager UI)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepResult {
    pub step: &'static str,
    pub title: &'static str,
    /// `"pass"` | `"fail"` | `"skipped"`
    pub status: &'static str,
    pub duration_ms: u64,
    pub summary: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionTestReport {
    pub ok: bool,
    pub total_ms: u64,
    pub probe_port: u16,
    pub borrowed_port: bool,
    pub ssh_exe: String,
    pub steps: [StepResult; 3],
}

struct SessionStopGuard(Arc<RemoteSession>);

impl Drop for SessionStopGuard {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// Classification of the initial `GET /` response head on a tunnelled port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootHttpVerdict {
    /// `HTTP 200` — inspect `/api/sessions` next to confirm it is pi-web.
    Candidate200,
    /// `HTTP 401` or `3xx` with `Location: .../login`.
    AuthRequired { status: u16, via_redirect: bool },
    /// Any other HTTP status (`403`, `404`, unrelated `3xx`, `5xx`, …).
    UnexpectedStatus(u16),
}

pub struct ProbeGate {
    port: u16,
    inner: Mutex<ProbeGateSlot>,
}

struct ProbeGateSlot {
    preempted: bool,
    session: Option<Arc<RemoteSession>>,
}

impl ProbeGate {
    pub fn new(port: u16) -> Self {
        Self { port, inner: Mutex::new(ProbeGateSlot { preempted: false, session: None }) }
    }
    pub fn port(&self) -> u16 { self.port }
    pub fn is_preempted(&self) -> bool { self.inner.lock().unwrap_or_else(|e| e.into_inner()).preempted }
    fn start_tunnel(&self, host: &HostConfig, job: &ProcessJob) -> Result<Option<Arc<RemoteSession>>, String> {
        let mut slot = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if slot.preempted { return Ok(None); }
        let session = Arc::new(RemoteSession::start(host, job, |_, _, _| {})?);
        slot.session = Some(session.clone());
        Ok(Some(session))
    }
    fn with_live_tunnel<T>(&self, op: impl FnOnce(u16) -> T) -> Option<T> {
        let slot = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if slot.preempted { return None; }
        let session = slot.session.as_ref()?;
        if session.is_retired() || session.exited().is_some() { return None; }
        Some(op(self.port))
    }
    pub fn preempt_and_stop(&self) {
        let mut slot = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        slot.preempted = true;
        if let Some(session) = slot.session.take() { session.stop(); }
    }
    fn finish_and_stop(&self) {
        let session = self.inner.lock().unwrap_or_else(|e| e.into_inner()).session.take();
        if let Some(s) = session { s.stop(); }
    }
}

struct GateFinishGuard<'a>(&'a ProbeGate);
impl Drop for GateFinishGuard<'_> {
    fn drop(&mut self) { self.0.finish_and_stop(); }
}

pub fn classify_root_http(head: &str, port: u16) -> Option<RootHttpVerdict> {
    let status = status_of(head)?;
    match status {
        200 => Some(RootHttpVerdict::Candidate200),
        401 => Some(RootHttpVerdict::AuthRequired { status, via_redirect: false }),
        301 | 302 | 303 | 307 | 308 if redirects_to_login(head, port) => {
            Some(RootHttpVerdict::AuthRequired { status, via_redirect: true })
        }
        other => Some(RootHttpVerdict::UnexpectedStatus(other)),
    }
}

fn skipped_step(step: &'static str, title: &'static str, reason: &str) -> StepResult {
    StepResult {
        step,
        title,
        status: "skipped",
        duration_ms: 0,
        summary: "未执行".into(),
        detail: reason.into(),
    }
}

fn summarize_ssh_failure(log: &str, code: Option<i32>) -> String {
    let lower = log.to_ascii_lowercase();
    if lower.contains("permission denied") {
        "SSH 密钥认证被拒绝（Permission denied）".into()
    } else if lower.contains("host key verification failed")
        || lower.contains("remote host identification has changed")
    {
        "SSH 主机指纹未受信任或已变化".into()
    } else if lower.contains("could not resolve hostname") {
        "无法解析 SSH 主机名或别名".into()
    } else if lower.contains("timed out")
        || lower.contains("no route to host")
        || lower.contains("network is unreachable")
    {
        "SSH 连接超时或目标网络不可达".into()
    } else if lower.contains("connection refused") && !lower.contains("connect failed") {
        "目标 SSH 端口拒绝连接（sshd 未启动或端口有误）".into()
    } else if lower.contains("address already in use") || lower.contains("cannot listen to port") {
        "本机隧道端口已被占用".into()
    } else {
        format!("SSH 进程提前退出（code {code:?}）")
    }
}

fn format_probe_detail(host: &HostConfig, log: &str) -> String {
    let trimmed = log.trim();
    let diag = diagnose(host, trimmed);
    if trimmed.is_empty() {
        diag
    } else {
        format!("{trimmed}\n\n{diag}")
    }
}

/// Run a real, non-destructive 3-step connection probe (`SSH 隧道` -> `pi-web 响应`
/// -> `口令鉴权`) against `host`, always tearing down the temporary `ssh.exe`
/// tunnel and waiting for `host.local_port` to be released before returning.
///
/// `register_session` lets `AppState` hold a handle to the live probe tunnel so
/// a concurrent user switch onto the same port can preempt the probe at once.
pub fn probe_connection(
    host: &HostConfig,
    unset_password_env: Option<&str>,
    borrowed_port: bool,
    job: &ProcessJob,
    gate: &ProbeGate,
) -> ConnectionTestReport {
    const S1: (&str, &str) = ("ssh_tunnel", "1. SSH 隧道建立");
    const S2: (&str, &str) = ("http_probe", "2. 远端 pi-web 响应");
    const S3: (&str, &str) = ("web_auth", "3. 口令鉴权验证");

    let total_started = Instant::now();
    let probe_port = host.local_port;
    let preempted_report = |ssh_str: String, elapsed_ms: u64| ConnectionTestReport {
        ok: false,
        total_ms: total_started.elapsed().as_millis() as u64,
        probe_port,
        borrowed_port,
        ssh_exe: ssh_str,
        steps: [
            StepResult {
                step: S1.0,
                title: S1.1,
                status: "fail",
                duration_ms: elapsed_ms,
                summary: "连接测试已中止（主窗口正在切换到该主机）".into(),
                detail: "为避免占用正式隧道端口，诊断已自动让路给主窗口连接。".into(),
            },
            skipped_step(S2.0, S2.1, "诊断已让路给主窗口切换"),
            skipped_step(S3.0, S3.1, "诊断已让路给主窗口切换"),
        ],
    };

    let ssh_path = match ssh_exe() {
        Ok(p) => p,
        Err(e) => {
            return ConnectionTestReport {
                ok: false,
                total_ms: total_started.elapsed().as_millis() as u64,
                probe_port,
                borrowed_port,
                ssh_exe: String::new(),
                steps: [
                    StepResult {
                        step: S1.0,
                        title: S1.1,
                        status: "fail",
                        duration_ms: 0,
                        summary: "未找到系统 OpenSSH 客户端".into(),
                        detail: e,
                    },
                    skipped_step(S2.0, S2.1, "前置步骤未通过"),
                    skipped_step(S3.0, S3.1, "前置步骤未通过"),
                ],
            };
        }
    };
    let ssh_display = ssh_path.display().to_string();

    // --- Step 1: SSH tunnel --------------------------------------------------
    let t1 = Instant::now();
    let _finish = GateFinishGuard(gate);
    let session_arc = match gate.start_tunnel(host, job) {
        Ok(Some(s)) => s,
        Ok(None) => return preempted_report(ssh_display, t1.elapsed().as_millis() as u64),
        Err(e) => {
            return ConnectionTestReport {
                ok: false,
                total_ms: total_started.elapsed().as_millis() as u64,
                probe_port,
                borrowed_port,
                ssh_exe: ssh_display,
                steps: [
                    StepResult {
                        step: S1.0,
                        title: S1.1,
                        status: "fail",
                        duration_ms: t1.elapsed().as_millis() as u64,
                        summary: "无法启动本机 SSH 隧道".into(),
                        detail: e,
                    },
                    skipped_step(S2.0, S2.1, "SSH 隧道未建立"),
                    skipped_step(S3.0, S3.1, "SSH 隧道未建立"),
                ],
            };
        }
    };

    let session = SessionStopGuard(session_arc);
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, probe_port));
    let mut connected_stream: Option<TcpStream> = None;
    let tunnel_timeout = Duration::from_secs(10);
    while t1.elapsed() < tunnel_timeout {
        if gate.is_preempted() || session.0.is_retired() {
            return preempted_report(ssh_display, t1.elapsed().as_millis() as u64);
        }
        if let Some(code) = session.0.exited() {
            std::thread::sleep(Duration::from_millis(150));
            let log = session.0.log_tail(4000);
            return ConnectionTestReport {
                ok: false,
                total_ms: total_started.elapsed().as_millis() as u64,
                probe_port,
                borrowed_port,
                ssh_exe: ssh_display,
                steps: [
                    StepResult {
                        step: S1.0,
                        title: S1.1,
                        status: "fail",
                        duration_ms: t1.elapsed().as_millis() as u64,
                        summary: summarize_ssh_failure(&log, code),
                        detail: format_probe_detail(host, &log),
                    },
                    skipped_step(S2.0, S2.1, "SSH 隧道建立失败"),
                    skipped_step(S3.0, S3.1, "SSH 隧道建立失败"),
                ],
            };
        }
        // OpenSSH binds `-L` only after SSH authentication succeeds, so the
        // moment loopback accepts a connection the SSH session is up.
        if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(250)) {
            std::thread::sleep(Duration::from_millis(20));
            if session.0.exited().is_none() {
                connected_stream = Some(stream);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(120));
    }

    let Some(first_stream) = connected_stream else {
        let log = session.0.log_tail(4000);
        return ConnectionTestReport {
            ok: false,
            total_ms: total_started.elapsed().as_millis() as u64,
            probe_port,
            borrowed_port,
            ssh_exe: ssh_display,
            steps: [
                StepResult {
                    step: S1.0,
                    title: S1.1,
                    status: "fail",
                    duration_ms: t1.elapsed().as_millis() as u64,
                    summary: format!("{} 秒内未能建立 SSH 隧道", tunnel_timeout.as_secs()),
                    detail: format_probe_detail(host, &log),
                },
                skipped_step(S2.0, S2.1, "SSH 隧道建立超时"),
                skipped_step(S3.0, S3.1, "SSH 隧道建立超时"),
            ],
        };
    };

    let step1 = StepResult {
        step: S1.0,
        title: S1.1,
        status: "pass",
        duration_ms: t1.elapsed().as_millis() as u64,
        summary: if borrowed_port {
            format!(
                "隧道已建立（目标端口正被激活会话占用，借用临时端口 127.0.0.1:{probe_port}）"
            )
        } else {
            format!(
                "隧道已建立（127.0.0.1:{probe_port} → {}:{}）",
                host.remote_host, host.remote_port
            )
        },
        detail: format!("客户端：{ssh_display}（已纳入 Job Object 守护）"),
    };

    // --- Step 2: Remote pi-web HTTP response ---------------------------------
    let t2 = Instant::now();
    let http_timeout = Duration::from_secs(5);
    let mut head = gate.with_live_tunnel(|p| server::http_head_on_stream(first_stream, p)).flatten();
    while head.is_none() && t2.elapsed() < http_timeout {
        if gate.is_preempted() || session.0.is_retired() {
            return preempted_report(ssh_display, step1.duration_ms);
        }
        if let Some(code) = session.0.exited() {
            std::thread::sleep(Duration::from_millis(150));
            let log = session.0.log_tail(4000);
            return ConnectionTestReport {
                ok: false,
                total_ms: total_started.elapsed().as_millis() as u64,
                probe_port,
                borrowed_port,
                ssh_exe: ssh_display,
                steps: [
                    step1,
                    StepResult {
                        step: S2.0,
                        title: S2.1,
                        status: "fail",
                        duration_ms: t2.elapsed().as_millis() as u64,
                        summary: format!("转发期间 SSH 断开（code {code:?}）"),
                        detail: format_probe_detail(host, &log),
                    },
                    skipped_step(S3.0, S3.1, "远端 pi-web 未响应"),
                ],
            };
        }
        let log_snapshot = session.0.log_tail(4000);
        let lower = log_snapshot.to_ascii_lowercase();
        let refused = lower.matches("connect failed").count() + lower.matches("open failed").count();
        if refused >= 2 {
            return ConnectionTestReport {
                ok: false,
                total_ms: total_started.elapsed().as_millis() as u64,
                probe_port,
                borrowed_port,
                ssh_exe: ssh_display,
                steps: [
                    step1,
                    StepResult {
                        step: S2.0,
                        title: S2.1,
                        status: "fail",
                        duration_ms: t2.elapsed().as_millis() as u64,
                        summary: format!(
                            "远端 {}:{} 无服务监听（连接被拒绝）",
                            host.remote_host, host.remote_port
                        ),
                        detail: format_probe_detail(host, &log_snapshot),
                    },
                    skipped_step(S3.0, S3.1, "远端 pi-web 未响应"),
                ],
            };
        }
        std::thread::sleep(Duration::from_millis(250));
        match gate.with_live_tunnel(server::http_head) {
            Some(h) => head = h,
            None if gate.is_preempted() || session.0.is_retired() => {
                return preempted_report(ssh_display, step1.duration_ms);
            }
            None => {}
        }
    }

    let Some(head) = head else {
        let log = session.0.log_tail(4000);
        return ConnectionTestReport {
            ok: false,
            total_ms: total_started.elapsed().as_millis() as u64,
            probe_port,
            borrowed_port,
            ssh_exe: ssh_display,
            steps: [
                step1,
                StepResult {
                    step: S2.0,
                    title: S2.1,
                    status: "fail",
                    duration_ms: t2.elapsed().as_millis() as u64,
                    summary: format!(
                        "远端 {}:{} 未返回有效 HTTP 响应",
                        host.remote_host, host.remote_port
                    ),
                    detail: format_probe_detail(host, &log),
                },
                skipped_step(S3.0, S3.1, "远端 pi-web 未响应"),
            ],
        };
    };

    let Some(verdict) = classify_root_http(&head, probe_port) else {
        return ConnectionTestReport {
            ok: false,
            total_ms: total_started.elapsed().as_millis() as u64,
            probe_port,
            borrowed_port,
            ssh_exe: ssh_display,
            steps: [
                step1,
                StepResult {
                    step: S2.0,
                    title: S2.1,
                    status: "fail",
                    duration_ms: t2.elapsed().as_millis() as u64,
                    summary: "远端未返回合法的 HTTP 状态行".into(),
                    detail: head.lines().next().unwrap_or_default().to_string(),
                },
                skipped_step(S3.0, S3.1, "远端 HTTP 状态异常"),
            ],
        };
    };

    let (auth_required, step2_summary) = match verdict {
        RootHttpVerdict::UnexpectedStatus(status_code) => {
            return ConnectionTestReport {
                ok: false,
                total_ms: total_started.elapsed().as_millis() as u64,
                probe_port,
                borrowed_port,
                ssh_exe: ssh_display,
                steps: [
                    step1,
                    StepResult {
                        step: S2.0,
                        title: S2.1,
                        status: "fail",
                        duration_ms: t2.elapsed().as_millis() as u64,
                        summary: format!(
                            "远端返回 HTTP {status_code}（非预期的 pi-web 服务入口）"
                        ),
                        detail: format!(
                            "响应首行：{}\n请确认 {}:{} 运行的是 pi-web 而非其他 Web 服务。",
                            head.lines().next().unwrap_or_default(),
                            host.remote_host,
                            host.remote_port
                        ),
                    },
                    skipped_step(S3.0, S3.1, "远端未确认是 pi-web"),
                ],
            };
        }
        RootHttpVerdict::AuthRequired {
            status,
            via_redirect,
        } => (
            true,
            if via_redirect {
                format!("HTTP {status} → /login（远端 pi-web 已启用口令保护）")
            } else {
                format!("HTTP {status}（远端 pi-web 已启用口令保护）")
            },
        ),
        RootHttpVerdict::Candidate200 => {
            let Some(api_probe) = gate.with_live_tunnel(server::probe_pi_web_api) else {
                return preempted_report(ssh_display, step1.duration_ms);
            };
            match api_probe {
            PiWebApiProbe::OpenPiWeb => (
                false,
                "HTTP 200（已验证 /api/sessions，远端 pi-web 在线且未设口令）".into(),
            ),
            PiWebApiProbe::AuthRequired => (
                true,
                "HTTP 200 + API 401（远端 pi-web 已启用口令保护）".into(),
            ),
            PiWebApiProbe::NotPiWeb(reason) => {
                return ConnectionTestReport {
                    ok: false,
                    total_ms: total_started.elapsed().as_millis() as u64,
                    probe_port,
                    borrowed_port,
                    ssh_exe: ssh_display,
                    steps: [
                        step1,
                        StepResult {
                            step: S2.0,
                            title: S2.1,
                            status: "fail",
                            duration_ms: t2.elapsed().as_millis() as u64,
                            summary: "远端端口有 HTTP 服务，但并非 pi-web".into(),
                            detail: format!(
                                "{reason}\n请检查 {}:{} 是否指向了正确的 pi-web 端口（默认 30141）。",
                                host.remote_host, host.remote_port
                            ),
                        },
                        skipped_step(S3.0, S3.1, "远端服务并非 pi-web"),
                    ],
                };
            }
            }
        }
    };

    let step2 = StepResult {
        step: S2.0,
        title: S2.1,
        status: "pass",
        duration_ms: t2.elapsed().as_millis() as u64,
        summary: step2_summary,
        detail: format!("远端监听 {}:{} 响应正常", host.remote_host, host.remote_port),
    };

    // --- Step 3: Password / cookie auth verification -------------------------
    let (ok, step3) = if !auth_required {
        (
            true,
            StepResult {
                step: S3.0,
                title: S3.1,
                status: "pass",
                duration_ms: 0,
                summary: "无需口令（远端未设 PI_WEB_PASSWORD）".into(),
                detail: if host.password.is_some() {
                    "提示：远端未开启口令保护，当前填写的口令不会被使用。".into()
                } else {
                    "远端无需口令即可直接访问。".into()
                },
            },
        )
    } else if let Some(password) = host.password.as_deref() {
        let t3 = Instant::now();
        let Some(outcome) = gate.with_live_tunnel(|p| server::probe_web_auth(p, password)) else {
            return preempted_report(ssh_display, step1.duration_ms);
        };
        let d3 = t3.elapsed().as_millis() as u64;
        match outcome {
            WebAuthProbe::Authenticated => (
                true,
                StepResult {
                    step: S3.0,
                    title: S3.1,
                    status: "pass",
                    duration_ms: d3,
                    summary: "鉴权通过".into(),
                    detail: "POST /api/web-auth 返回 HTTP 200，已确认可正常换取 pi_web_session 会话令牌。"
                        .into(),
                },
            ),
            WebAuthProbe::WrongPassword(code) => (
                false,
                StepResult {
                    step: S3.0,
                    title: S3.1,
                    status: "fail",
                    duration_ms: d3,
                    summary: "口令错误".into(),
                    detail: format!(
                        "POST /api/web-auth 返回 HTTP {code}，口令与远端 PI_WEB_PASSWORD 不一致。"
                    ),
                },
            ),
            WebAuthProbe::RateLimited => (
                false,
                StepResult {
                    step: S3.0,
                    title: S3.1,
                    status: "fail",
                    duration_ms: d3,
                    summary: "远端鉴权触发频率限制（HTTP 429）".into(),
                    detail: "短时间内失败次数过多，请稍候再试并核对口令。".into(),
                },
            ),
            WebAuthProbe::UnsupportedRoute(code) => (
                false,
                StepResult {
                    step: S3.0,
                    title: S3.1,
                    status: "fail",
                    duration_ms: d3,
                    summary: "远端 pi-web 版本过旧（无 /api/web-auth）".into(),
                    detail: format!(
                        "POST /api/web-auth 返回 HTTP {code}；请在远端升级 @agegr/pi-web ≥ 0.9.1。"
                    ),
                },
            ),
            WebAuthProbe::Failed(err) => (
                false,
                StepResult {
                    step: S3.0,
                    title: S3.1,
                    status: "fail",
                    duration_ms: d3,
                    summary: "鉴权请求失败".into(),
                    detail: err,
                },
            ),
        }
    } else {
        let detail = match unset_password_env {
            Some(env_name) => format!(
                "远端已设口令，表单填写的环境变量「{env_name}」在当前系统进程环境中未设置或为空，且未填写直接口令。"
            ),
            None => "远端已设口令（需要鉴权），请填写有效的「口令环境变量名」或「直接口令」。".into(),
        };
        (
            false,
            StepResult {
                step: S3.0,
                title: S3.1,
                status: "fail",
                duration_ms: 0,
                summary: "远端设了口令但未填".into(),
                detail,
            },
        )
    };

    // Explicitly drop `session` before computing `total_ms`? Stopping waits for
    // the port to release (~0-100ms), and dropping here guarantees the borrowed
    // port is already free again the instant the UI gets its response.
    drop(session);

    ConnectionTestReport {
        ok,
        total_ms: total_started.elapsed().as_millis() as u64,
        probe_port,
        borrowed_port,
        ssh_exe: ssh_display,
        steps: [step1, step2, step3],
    }
}

fn tail(s: &str, n: usize) -> String {
    let start = s.len().saturating_sub(n);
    let start = (start..=s.len()).find(|i| s.is_char_boundary(*i)).unwrap_or(0);
    s[start..].to_string()
}

fn spawn_stderr_reader(mut pipe: impl Read + Send + 'static, log: Arc<Mutex<String>>, tag: String) {
    std::thread::spawn(move || {
        // Raw bytes rather than lines: ssh's messages are short and the last
        // one before an exit often has no trailing newline.
        let mut buf = [0u8; 2048];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                    for line in text.lines().filter(|l| !l.trim().is_empty()) {
                        log_dbg!("[ssh/{tag}] {}", line.trim_end());
                    }
                    let mut log = log.lock().unwrap_or_else(|e| e.into_inner());
                    log.push_str(&text);
                    if log.len() > LOG_CAP {
                        let cut = log.len() - LOG_CAP;
                        let cut = (cut..=log.len()).find(|i| log.is_char_boundary(*i)).unwrap_or(0);
                        *log = log[cut..].to_string();
                    }
                }
            }
        }
    });
}

fn spawn_watchdog(
    child: Arc<Mutex<Option<Child>>>,
    log: Arc<Mutex<String>>,
    retired: Arc<AtomicBool>,
    serial: u64,
    id: String,
    on_exit: impl Fn(u64, Option<i32>, String) + Send + 'static,
) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(500));
        let status = {
            let mut guard = child.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_mut() {
                None => return, // taken by stop()
                Some(c) => c.try_wait(),
            }
        };
        match status {
            Ok(Some(st)) => {
                log_dbg!("remote[{id}#{serial}]: ssh exited code={:?}", st.code());
                if retired.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(150));
                let t = tail(&log.lock().unwrap_or_else(|e| e.into_inner()), 4000);
                on_exit(serial, st.code(), t);
                return;
            }
            Ok(None) => {}
            Err(_) => return,
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> HostConfig {
        HostConfig {
            id: "arch-130".into(),
            label: "Arch Linux (130)".into(),
            ssh_target: "130".into(),
            ssh_port: Some(2222),
            local_port: 45002,
            remote_host: "127.0.0.1".into(),
            remote_port: 30141,
            password: None,
        }
    }

    #[test]
    fn args_match_the_spec() {
        let a = ssh_args(&host());
        assert_eq!(
            a.join(" "),
            "-N -o BatchMode=yes -o ExitOnForwardFailure=yes -o ConnectTimeout=8 \
             -o ServerAliveInterval=15 -o ServerAliveCountMax=3 \
             -L 127.0.0.1:45002:127.0.0.1:30141 -p 2222 -- 130"
        );
        let mut h = host();
        h.ssh_port = None;
        h.remote_host = "::1".into();
        let a = ssh_args(&h).join(" ");
        assert!(!a.contains("-p "));
        assert!(a.contains("-L 127.0.0.1:45002:[::1]:30141"));
    }

    #[test]
    fn msys_paths_are_recognised() {
        for p in [
            r"C:\Program Files\Git\usr\bin\ssh.exe",
            r"C:\msys64\usr\bin\ssh.exe",
            r"C:\cygwin64\bin\ssh.exe",
            r"D:\Tools\mingw64\bin\ssh.exe",
        ] {
            assert!(is_msys_like(std::path::Path::new(p)), "{p}");
        }
        assert!(!is_msys_like(std::path::Path::new(r"C:\Windows\System32\OpenSSH\ssh.exe")));
        assert!(!is_msys_like(std::path::Path::new(r"C:\Program Files\OpenSSH\ssh.exe")));
    }

    #[cfg(windows)]
    #[test]
    fn prefers_the_system_openssh() {
        let p = ssh_exe().expect("this machine has the Windows OpenSSH client");
        assert!(
            p.to_string_lossy().to_ascii_lowercase().ends_with(r"system32\openssh\ssh.exe"),
            "{}",
            p.display()
        );
    }

    #[test]
    fn login_redirect_counts_as_auth_required() {
        assert!(redirects_to_login("HTTP/1.1 307 Temporary Redirect\r\nlocation: /login\r\n", 45001));
        assert!(redirects_to_login("HTTP/1.1 307 Temporary Redirect\r\nLocation: /login?from=%2F\r\n", 45001));
        assert!(redirects_to_login("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:45001/login\r\n", 45001));
        assert!(!redirects_to_login("HTTP/1.1 307 Temporary Redirect\r\nLocation: https://unrelated.example/login\r\n", 45001));
        assert!(!redirects_to_login("HTTP/1.1 307 Temporary Redirect\r\nLocation: //evil.example/login\r\n", 45001));
        assert!(!redirects_to_login("HTTP/1.1 302 Found\r\nLocation: /other?next=/login\r\n", 45001));
        assert!(!redirects_to_login("HTTP/1.1 200 OK\r\ncontent-type: text/html\r\n", 45001));
        assert_eq!(status_of("HTTP/1.1 401 Unauthorized"), Some(401));
    }

    #[test]
    fn diagnose_names_the_likely_cause() {
        let d = diagnose(&host(), "chenkun@192.168.100.130: Permission denied (publickey).");
        assert!(d.contains("认证失败"));
        let d = diagnose(&host(), "channel 2: open failed: connect failed: Connection refused");
        assert!(d.contains("无服务"));
        assert!(!d.contains("sshd"), "a refused forward is not a refused sshd");
        assert!(diagnose(&host(), "").contains("Windows 本地"));
    }

    #[test]
    fn ssh_failure_summary_classifies_stderr_causes() {
        assert!(summarize_ssh_failure("Permission denied (publickey).", Some(255)).contains("认证被拒绝"));
        assert!(summarize_ssh_failure("Host key verification failed.", Some(255)).contains("指纹"));
        assert!(summarize_ssh_failure("ssh: Could not resolve hostname nope", Some(255)).contains("无法解析"));
        assert!(summarize_ssh_failure("Connection timed out", Some(255)).contains("超时"));
        assert!(summarize_ssh_failure("connect to host 10.0.0.2 port 22: Connection refused", Some(255)).contains("拒绝连接"));
    }

    #[test]
    fn root_http_classifier_rejects_404_403_and_foreign_redirects() {
        assert_eq!(classify_root_http("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n", 45002), Some(RootHttpVerdict::Candidate200));
        assert_eq!(classify_root_http("HTTP/1.1 307 Temporary Redirect\r\nLocation: /login\r\n", 45002), Some(RootHttpVerdict::AuthRequired { status: 307, via_redirect: true }));
        assert_eq!(classify_root_http("HTTP/1.1 401 Unauthorized\r\n", 45002), Some(RootHttpVerdict::AuthRequired { status: 401, via_redirect: false }));
        assert_eq!(classify_root_http("HTTP/1.1 404 Not Found\r\n", 45002), Some(RootHttpVerdict::UnexpectedStatus(404)));
        assert_eq!(classify_root_http("HTTP/1.1 403 Forbidden\r\n", 45002), Some(RootHttpVerdict::UnexpectedStatus(403)));
        assert_eq!(classify_root_http("HTTP/1.1 302 Found\r\nLocation: https://example.com/login\r\n", 45002), Some(RootHttpVerdict::UnexpectedStatus(302)));
        assert_eq!(classify_root_http("HTTP/1.1 302 Found\r\nLocation: /other?next=/login\r\n", 45002), Some(RootHttpVerdict::UnexpectedStatus(302)));
    }

    #[test]
    fn probe_gate_blocks_all_requests_once_preempted() {
        let gate = ProbeGate::new(45099);
        assert!(!gate.is_preempted());
        gate.preempt_and_stop();
        assert!(gate.is_preempted());
        let mut called = false;
        assert_eq!(gate.with_live_tunnel(|_| { called = true; 42 }), None);
        assert!(!called);
    }
}
