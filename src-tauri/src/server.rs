//! The embedded pi-web server: port selection, spawn, readiness, teardown.
//!
//! This is the half of the port that deviates most from the Electron shell, and
//! every deviation is deliberate — see PORTING.md. The two that matter here:
//!
//!   * The port is PERSISTED and reused (坑 1). pi-web keeps the theme, unread
//!     markers, hidden cwds, kiosk state and the sound toggle in `localStorage`,
//!     which is scoped to `http://127.0.0.1:<port>`. A fresh port on every
//!     launch silently resets all of it, and the resulting bug report points at
//!     theme sync, not at the port.
//!   * `--no-open` AND `PI_WEB_NO_OPEN=1` are both set (坑 3). Without them
//!     `bin/pi-web.js` runs `cmd /c start <url>` the moment Next prints Ready,
//!     and a system browser window appears next to ours.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::job::ProcessJob;
use crate::log_dbg;
use crate::proc::NoConsole;
use crate::paths::Paths;

/// The pi CLI package name, as pi-subagents spells it when validating a package
/// root handed to it via this env var (its shared/utils.ts + runs/shared/pi-spawn.ts).
const PI_CODING_AGENT_PACKAGE: &str = "@earendil-works/pi-coding-agent";
const PI_CODING_AGENT_PACKAGE_ROOT_ENV: &str = "PI_SUBAGENTS_PI_CODING_AGENT_PACKAGE_ROOT";

/// Port pool. Deliberately BELOW the Windows dynamic port range (49152-65535):
/// a port the OS never hands out ephemerally is a port that is still free the
/// next time we launch, which is the whole point of persisting it. Also not
/// pi-web's documented default 30141 — a guessable port is one a hostile web
/// page can try by hand.
const PORT_MIN: u16 = 30_000;
const PORT_MAX: u16 = 44_999;

const PORT_FILE: &str = "port.json";

const LOG_CAP: usize = 20_000;

// ---------------------------------------------------------------------------
// Port persistence
// ---------------------------------------------------------------------------

fn port_file(paths: &Paths) -> std::path::PathBuf {
    paths.user_data_dir().join(PORT_FILE)
}

fn read_saved_port(paths: &Paths) -> Option<u16> {
    let text = std::fs::read_to_string(port_file(paths)).ok()?;
    // Tolerate a BOM: this file is small enough to be hand-edited, and every
    // PowerShell text cmdlet writes one.
    let value: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    let port = value.get("port")?.as_u64()?;
    u16::try_from(port).ok().filter(|p| *p >= 1024)
}

fn write_saved_port(paths: &Paths, port: u16) {
    let path = port_file(paths);
    let body = format!("{{\n  \"port\": {port}\n}}\n");
    if let Err(e) = std::fs::write(&path, body) {
        // Cosmetic on this run; the cost is the user's preferences resetting on
        // the NEXT one, so it is worth a log line.
        log_dbg!("port: could not persist {} ({e})", path.display());
    }
}

fn is_free(port: u16) -> bool {
    TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).is_ok()
}

/// Resolve the port for this launch: reuse the persisted one whenever it is
/// still free, otherwise draw a new one and record it.
pub fn resolve_port(paths: &Paths) -> Result<u16, String> {
    if let Some(saved) = read_saved_port(paths) {
        if is_free(saved) {
            log_dbg!("port: reusing persisted {saved}");
            return Ok(saved);
        }
        log_dbg!("port: persisted {saved} is occupied — picking another (localStorage will reset)");
    }

    use rand::Rng;
    let mut rng = rand::thread_rng();
    for _ in 0..200 {
        let candidate = rng.gen_range(PORT_MIN..=PORT_MAX);
        if is_free(candidate) {
            write_saved_port(paths, candidate);
            log_dbg!("port: selected {candidate} and persisted it");
            return Ok(candidate);
        }
    }
    Err("no free port available in 30000-44999".into())
}

/// Whether this runtime's pi-web can authenticate with a session COOKIE rather
/// than only with an `Authorization: Basic` header.
///
/// This decides whether the shell sets a password at all, so it is worth being
/// precise about. WebView2 cannot be driven through a Basic challenge:
///
///   * wry exposes no `BasicAuthenticationRequested` hook, so the shell cannot
///     answer the 401 itself;
///   * embedding the credentials in the top-level navigation (`http://pi:pw@…`)
///     DOES authenticate that one navigation, but it leaves the credentials in
///     the document's base URL — after which every relative `fetch()` in the
///     page dies at construction with "Request cannot be constructed from a URL
///     that includes credentials". Measured, not assumed; see PORTING.md 坑 4.
///
/// pi-web 0.9.1 added `POST /api/web-auth`, which trades the password for an
/// HMAC-signed `pi_web_session` cookie. A cookie is something the shell CAN
/// install, so from that version on the password is usable. Older runtimes get
/// no password — an explicit, documented downgrade rather than a silent one.
pub fn supports_cookie_auth(paths: &Paths) -> bool {
    paths
        .pi_web_pkg_dir()
        .join(".next")
        .join("server")
        .join("app")
        .join("api")
        .join("web-auth")
        .join("route.js")
        .exists()
}

/// A fresh password per launch. Username is always `pi` (hardcoded in pi-web's
/// middleware). It is handed to the child as an env var and exchanged for a
/// session cookie over loopback by `Server::login()` — it is never written to
/// disk and never reaches the page.
pub fn generate_password() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| {
            let n: u8 = rng.gen_range(0..16);
            char::from_digit(n as u32, 16).unwrap_or('0')
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Env
// ---------------------------------------------------------------------------

/// main.js's `bundledPiAgentEnv()`.
///
/// Left alone, pi-subagents probes `process.argv[1]` — which for our server is
/// next's bin, nowhere near pi-coding-agent — then `import.meta.resolve()` from
/// its own install under `~/.pi/agent/npm`, where `@earendil-works` is EMPTY
/// because the desktop never npm-installs the agent there. Both miss, and
/// `getPiSpawnCommand()` falls back to a bare `pi` on PATH: a separately
/// installed, independently versioned global CLI, or nothing at all.
///
/// Validated the way pi-subagents validates it: a root whose package.json name
/// does not match is silently ignored there, so check here too and say so in
/// the log rather than exporting an env var that quietly does nothing.
fn bundled_pi_agent_env(paths: &Paths) -> HashMap<String, String> {
    let root = paths.bundled_pi_agent_dir();
    let manifest = root.join("package.json");
    let mut out = HashMap::new();

    let name = std::fs::read_to_string(&manifest)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_owned));

    match name.as_deref() {
        Some(PI_CODING_AGENT_PACKAGE) => {
            out.insert(
                PI_CODING_AGENT_PACKAGE_ROOT_ENV.to_string(),
                root.to_string_lossy().into_owned(),
            );
        }
        Some(other) => log_dbg!(
            "bundled pi package root is \"{other}\", expected {PI_CODING_AGENT_PACKAGE} — subagents fall back to PATH"
        ),
        None => log_dbg!(
            "bundled pi package root unusable at {} — subagents fall back to PATH",
            root.display()
        ),
    }
    out
}

/// Prepend the bundled node dir to PATH so any node/npx an agent tool spawns
/// resolves to the bundled runtime rather than whatever the machine happens to
/// have (or not have).
fn prefixed_path(node_dir: &Path) -> String {
    let existing = std::env::var("PATH").unwrap_or_default();
    if existing.is_empty() {
        node_dir.to_string_lossy().into_owned()
    } else {
        format!("{};{}", node_dir.display(), existing)
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub struct Server {
    pub port: u16,
    /// None when this runtime predates cookie auth — see `supports_cookie_auth`.
    password: Option<String>,
    child: Arc<Mutex<Option<Child>>>,
    pid: u32,
    log: Arc<Mutex<String>>,
    ready: Arc<AtomicBool>,
    in_job: bool,
    /// `pi_web_session=<token>`, as the server minted it. Set by `login()`.
    cookie: Mutex<Option<String>>,
    /// Flips once the cookie has been installed in the webview for this server.
    pub auth_injected: AtomicBool,
}

impl Server {
    /// Spawn `node bin/pi-web.js -H 127.0.0.1 -p <port> --no-open`.
    ///
    /// `on_exit` fires from a watchdog thread when the child goes away, with the
    /// exit code and the tail of its output. The caller decides whether that was
    /// a crash worth surfacing or a stop it asked for.
    pub fn start(
        paths: &Paths,
        job: &ProcessJob,
        port: u16,
        password: Option<String>,
        on_exit: impl Fn(Option<i32>, String) + Send + 'static,
    ) -> Result<Self, String> {
        let node = paths.node_exe();
        let bin = paths.pi_web_bin();
        let pkg_dir = paths.pi_web_pkg_dir();
        let agent_env = bundled_pi_agent_env(paths);

        log_dbg!(
            "startServer node={} nodeExists={} bin={} binExists={} pkgDir={} nextDirExists={} port={port} piPackageRoot={}",
            node.display(),
            node.exists(),
            bin.display(),
            bin.exists(),
            pkg_dir.display(),
            pkg_dir.join(".next").exists(),
            agent_env
                .get(PI_CODING_AGENT_PACKAGE_ROOT_ENV)
                .map(String::as_str)
                .unwrap_or("(unresolved)")
        );

        if !pkg_dir.join(".next").exists() {
            return Err(format!("pi-web .next not found in runtime: {}", pkg_dir.display()));
        }
        if !bin.exists() {
            return Err(format!("pi-web launcher not found: {}", bin.display()));
        }
        if !node.exists() {
            return Err(format!("bundled node not found: {}", node.display()));
        }

        let mut cmd = Command::new(&node);
        cmd.arg(&bin)
            .arg("-H")
            .arg("127.0.0.1")
            .arg("-p")
            .arg(port.to_string())
            .arg("--no-open")
            .current_dir(&pkg_dir)
            .env("NODE_ENV", "production")
            .env("PATH", prefixed_path(&paths.node_dir()))
            // Belt and braces with --no-open (坑 3): either one alone suppresses
            // the `cmd /c start <url>` browser launch, and the env var also
            // covers a pi-web that ever stops honouring the flag.
            .env("PI_WEB_NO_OPEN", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        match password.as_deref() {
            Some(pw) => {
                cmd.env("PI_WEB_PASSWORD", pw);
            }
            None => log_dbg!(
                "PI_WEB_PASSWORD NOT set — this runtime has no cookie-auth route, and WebView2 \
                 cannot answer a Basic challenge. The port is protected only by the loopback bind \
                 and pi-web's Host allow-list (see PORTING.md 坑 4)."
            ),
        }

        for (k, v) in &agent_env {
            cmd.env(k, v);
        }

        cmd.no_console();

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not spawn the embedded server: {e}"))?;

        // Into the job BEFORE anything else, so even an immediate crash of this
        // shell cannot leave the tree behind. The Next.js grandchild joins the
        // job on its own when bin/pi-web.js spawns it.
        let in_job = job.assign(&child);

        let pid = child.id();
        let log = Arc::new(Mutex::new(String::new()));
        let ready = Arc::new(AtomicBool::new(false));

        if let Some(out) = child.stdout.take() {
            spawn_reader(out, log.clone(), ready.clone(), "out");
        }
        if let Some(err) = child.stderr.take() {
            spawn_reader(err, log.clone(), ready.clone(), "err");
        }

        let child = Arc::new(Mutex::new(Some(child)));
        spawn_watchdog(child.clone(), log.clone(), on_exit);

        Ok(Self {
            port,
            password,
            child,
            pid,
            log,
            ready,
            in_job,
            cookie: Mutex::new(None),
            auth_injected: AtomicBool::new(false),
        })
    }

    /// Origin the page actually runs on. Used for same-origin checks and for
    /// deciding whether the webview is currently showing pi-web.
    pub fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The URL we navigate the webview to.
    ///
    /// Plain, with NO embedded credentials. An earlier build used
    /// `http://pi:<pw>@127.0.0.1:<port>/`, which authenticates the navigation
    /// but poisons the document's base URL: every relative `fetch()` in pi-web
    /// then throws `Request cannot be constructed from a URL that includes
    /// credentials` before it is even sent. Authentication happens through
    /// `login()` + the injected cookie instead.
    pub fn nav_url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    /// Exchange the password for a session cookie, over loopback, from here.
    ///
    /// Doing the exchange in Rust is the whole point: the page only ever sees
    /// the resulting `pi_web_session` token — which it would hold anyway — and
    /// never the password. `POST /api/web-auth` is pi-web's own endpoint for
    /// this (0.9.1+); the shell just echoes back whatever cookie value the
    /// server mints, so nothing here depends on the token's internal format.
    ///
    /// A `None` result means "no password configured", not a failure.
    pub fn login(&self) -> Result<Option<String>, String> {
        let Some(password) = self.password.as_deref() else {
            return Ok(None);
        };
        let body = serde_json::json!({ "password": password }).to_string();
        let request = format!(
            "POST /api/web-auth HTTP/1.1\r\n\
             Host: 127.0.0.1:{}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{}",
            self.port,
            body.len(),
            body
        );
        let response = http_request(self.port, &request)?;

        let status_ok = response
            .lines()
            .next()
            .map(|l| l.contains(" 200"))
            .unwrap_or(false);
        if !status_ok {
            let status = response.lines().next().unwrap_or("(no status line)");
            return Err(format!("pi-web rejected the shell's login: {status}"));
        }

        // Take name=value and drop the attributes; the webview supplies its own
        // Path and the HttpOnly/Secure flags are meaningless for a JS-set cookie.
        let cookie = response
            .lines()
            .find_map(|line| {
                let rest = line.strip_prefix("Set-Cookie:").or_else(|| line.strip_prefix("set-cookie:"))?;
                let pair = rest.trim().split(';').next()?.trim();
                pair.starts_with("pi_web_session=").then(|| pair.to_string())
            })
            .ok_or_else(|| "pi-web accepted the login but set no session cookie".to_string())?;

        log_dbg!("login: session cookie obtained ({} bytes)", cookie.len());
        *self.cookie.lock().unwrap_or_else(|e| e.into_inner()) = Some(cookie.clone());
        Ok(Some(cookie))
    }

    /// `login()`, but tolerant of a server that is answering yet still warming
    /// up. The readiness probe only proves the socket replies; the auth route
    /// can still be compiling on the first request.
    pub fn login_with_retries(&self, attempts: usize) -> Result<Option<String>, String> {
        let mut last = String::new();
        for i in 0..attempts.max(1) {
            match self.login() {
                Ok(v) => return Ok(v),
                Err(e) => {
                    last = e;
                    log_dbg!("login attempt {}/{attempts} failed: {last}", i + 1);
                    std::thread::sleep(Duration::from_millis(500 * (i as u64 + 1)));
                }
            }
        }
        Err(last)
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

    pub fn log_tail(&self, n: usize) -> String {
        let log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        let s: &str = &log;
        let start = s.len().saturating_sub(n);
        // Never split a UTF-8 sequence — the log carries Chinese diagnostics.
        let start = (start..=s.len()).find(|i| s.is_char_boundary(*i)).unwrap_or(0);
        s[start..].to_string()
    }

    /// Block until the server answers, or give up.
    ///
    /// Two independent signals, because each has a hole: `bin/pi-web.js` relays
    /// Next's "Ready" line (fast, but a future pi-web could reword it), and an
    /// HTTP probe proves the socket serves (authoritative, but Next binds the
    /// port slightly before it can answer). A 401 counts as ready — it IS the
    /// server answering, and with a password set it is the expected reply.
    pub fn wait_ready(&self, timeout: Duration) -> Result<(), String> {
        let started = Instant::now();
        while started.elapsed() < timeout {
            if let Some(code) = self.exited() {
                return Err(format!(
                    "server exited before it was ready (code {code:?})\n\n{}",
                    self.log_tail(2000)
                ));
            }
            if self.ready.load(Ordering::Relaxed) && http_probe(self.port) {
                log_dbg!("server ready after {:?} (Ready line + http probe)", started.elapsed());
                return Ok(());
            }
            if http_probe(self.port) {
                log_dbg!("server ready after {:?} (http probe)", started.elapsed());
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Err(format!(
            "server not ready in time\n\n{}",
            self.log_tail(2000)
        ))
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

    /// Stop the server and wait for its handles to go.
    ///
    /// The job object alone would do this when the shell dies, but a deliberate
    /// stop has to happen NOW — a runtime swap cannot rename a directory whose
    /// files are still open, and the next start has to find the port free.
    /// `taskkill /T` is what reaches the Next.js grandchild synchronously.
    pub fn stop(&self) {
        let taken = {
            let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };
        let Some(mut child) = taken else { return };

        log_dbg!("stopping server pid {}", self.pid);

        #[cfg(windows)]
        {
            // /T takes the tree (the Next.js grandchild included), /F skips the
            // graceful path Windows does not really have for console-less nodes.
            //
            // `.no_console()` is what stops this flashing a black console window
            // on the way out — it runs on every app close, so a missing flag
            // here is visible to the user every single time.
            let killed = Command::new("taskkill")
                .args(["/pid", &self.pid.to_string(), "/t", "/f"])
                .no_console()
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if let Err(e) = killed {
                log_dbg!("taskkill failed ({e}) — falling back to direct kill");
                let _ = child.kill();
            }
        }
        #[cfg(not(windows))]
        {
            let _ = child.kill();
        }

        let _ = child.wait();
        log_dbg!("server pid {} stopped", self.pid);
    }
}

/// Stream a child pipe into the ring buffer, and flip `ready` when Next says so.
fn spawn_reader(
    pipe: impl Read + Send + 'static,
    log: Arc<Mutex<String>>,
    ready: Arc<AtomicBool>,
    tag: &'static str,
) {
    std::thread::spawn(move || {
        let reader = BufReader::new(pipe);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            log_dbg!("[server/{tag}] {}", line.trim_end());
            // `bin/pi-web.js` relays Next's stdout verbatim; Next announces
            // itself with a line containing "Ready in <n>ms".
            if line.contains("Ready") {
                ready.store(true, Ordering::Relaxed);
            }
            let mut buf = log.lock().unwrap_or_else(|e| e.into_inner());
            buf.push_str(&line);
            buf.push('\n');
            if buf.len() > LOG_CAP {
                let cut = buf.len() - LOG_CAP;
                let cut = (cut..=buf.len())
                    .find(|i| buf.is_char_boundary(*i))
                    .unwrap_or(0);
                *buf = buf[cut..].to_string();
            }
        }
    });
}

/// Poll rather than block-on-wait so `stop()` can take the Child out from under
/// us without the two fighting over the same handle.
fn spawn_watchdog(
    child: Arc<Mutex<Option<Child>>>,
    log: Arc<Mutex<String>>,
    on_exit: impl Fn(Option<i32>, String) + Send + 'static,
) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(500));
        let status = {
            let mut guard = child.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_mut() {
                // Taken by stop(): this server is retired, so is this watchdog.
                None => return,
                Some(c) => c.try_wait(),
            }
        };
        match status {
            Ok(Some(st)) => {
                let tail = {
                    let buf = log.lock().unwrap_or_else(|e| e.into_inner());
                    let s: &str = &buf;
                    let start = s.len().saturating_sub(2000);
                    let start = (start..=s.len()).find(|i| s.is_char_boundary(*i)).unwrap_or(0);
                    s[start..].to_string()
                };
                log_dbg!("server exit code={:?}", st.code());
                on_exit(st.code(), tail);
                return;
            }
            Ok(None) => {}
            Err(_) => return,
        }
    });
}

/// One bare HTTP/1.1 GET. Any well-formed status line proves the server is up —
/// including the 401 a password-protected pi-web answers with.
fn http_probe(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(1500)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(2500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(1500)));
    let req = format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 16];
    match stream.read(&mut head) {
        Ok(n) if n >= 5 => head.starts_with(b"HTTP/"),
        _ => false,
    }
}

/// Send one raw HTTP/1.1 request over loopback and return the response head.
///
/// Deliberately minimal: the shell's only HTTP needs are a readiness probe and
/// one login POST to a server it just started on 127.0.0.1, so an HTTP client
/// dependency would be all cost. `Connection: close` means "read to EOF" is a
/// complete framing, and only the head is ever inspected.
fn http_request(port: u16, request: &str) -> Result<String, String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .map_err(|e| format!("could not reach the embedded server: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("could not send the request: {e}"))?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|e| format!("could not read the response: {e}"))?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}
