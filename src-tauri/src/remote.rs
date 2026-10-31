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
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::hosts::HostConfig;
use crate::job::ProcessJob;
use crate::log_dbg;
use crate::proc::NoConsole;
use crate::server;

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

    /// Poll the tunnel's local end until the remote pi-web answers.
    ///
    /// Any HTTP answer below 500 counts: 200 (no password), 401 (API route with
    /// a password), and the 307 → /login pi-web sends for `/` when a password
    /// is set and the request carries no session. Returns whether the remote
    /// demanded authentication.
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
                let status = status_of(&head);
                match status {
                    Some(s) if s < 500 => {
                        let auth = s == 401 || redirects_to_login(&head);
                        log_dbg!(
                            "remote[{}]: ready after {:?} (HTTP {s}{})",
                            self.host.id,
                            started.elapsed(),
                            if auth { ", auth required" } else { "" }
                        );
                        return Ok(auth);
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

fn redirects_to_login(head: &str) -> bool {
    head.lines().any(|l| {
        let lower = l.to_ascii_lowercase();
        lower.starts_with("location:") && lower.contains("/login")
    })
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
        assert!(redirects_to_login("HTTP/1.1 307 Temporary Redirect\r\nlocation: /login\r\n"));
        assert!(!redirects_to_login("HTTP/1.1 200 OK\r\ncontent-type: text/html\r\n"));
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
}
