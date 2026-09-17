//! Rust-side driver for `guard-cli.js`.
//!
//! IRON RULE 3: `runtime-guard.js` and `updater.js` are NOT reimplemented here.
//! They sit in `resources/guard/`, byte-for-byte as they shipped in the Electron
//! app, still covered by `npm run test:guard`. This module's entire job is to
//! run them in the bundled node and read one line of JSON back. Anything that
//! looks like runtime-integrity logic in this file is a bug.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use crate::log_dbg;
use crate::paths::Paths;
use crate::proc::NoConsole;

pub const DEFAULT_REGISTRY: &str = "https://registry.npmmirror.com";

/// npm's stderr, one line at a time, for the updating/healing page.
pub type ProgressFn = Arc<dyn Fn(String) + Send + Sync>;

pub struct Guard {
    node: PathBuf,
    cli: PathBuf,
    npm_cli: PathBuf,
    node_dir: PathBuf,
    runtime: PathBuf,
    registry: String,
    log: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(flatten)]
    body: serde_json::Value,
}

#[derive(Debug, Deserialize, Default)]
pub struct Verify {
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub healable: bool,
    #[serde(default)]
    pub summary: String,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Installed {
    #[serde(rename = "piWeb")]
    pub pi_web: Option<String>,
    pub agent: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct Latest {
    #[serde(default)]
    pub latest: String,
    pub installed: Option<String>,
    pub agent: Option<String>,
    #[serde(default)]
    pub newer: bool,
}

impl Guard {
    pub fn new(paths: &Paths) -> Self {
        Self {
            node: paths.node_exe(),
            cli: paths.guard_cli(),
            npm_cli: paths.npm_cli(),
            node_dir: paths.node_dir(),
            runtime: paths.runtime_dir().to_path_buf(),
            registry: std::env::var("PI_WEB_REGISTRY").unwrap_or_else(|_| DEFAULT_REGISTRY.into()),
            log: crate::log::log_path(),
        }
    }

    /// A guard for `recover` ONLY.
    ///
    /// `Guard::new` asks `Paths::runtime_dir()` where the runtime is, and that
    /// answer is cached for the life of the process. At the moment recovery
    /// runs, the seed dir may be temporarily absent (a crash between the swap's
    /// two renames), so asking would latch onto the userData fallback forever
    /// and then fail to seed from a tree sitting right there in
    /// `.runtime-seed.trash`. `recover` takes its directories as `--dirs` and
    /// ignores `--runtime`, so leaving it empty is exactly right.
    pub fn new_for_recovery(paths: &Paths) -> Self {
        Self {
            node: paths.node_exe(),
            cli: paths.guard_cli(),
            npm_cli: paths.npm_cli(),
            node_dir: paths.node_dir(),
            runtime: PathBuf::new(),
            registry: std::env::var("PI_WEB_REGISTRY").unwrap_or_else(|_| DEFAULT_REGISTRY.into()),
            log: crate::log::log_path(),
        }
    }

    #[allow(dead_code)]
    pub fn registry(&self) -> &str {
        &self.registry
    }

    fn base_command(&self, command: &str) -> Command {
        let mut cmd = Command::new(&self.node);
        cmd.arg(&self.cli)
            .arg(command)
            .arg("--node")
            .arg(&self.node)
            .arg("--npm-cli")
            .arg(&self.npm_cli)
            .arg("--node-dir")
            .arg(&self.node_dir)
            .arg("--runtime")
            .arg(&self.runtime)
            .arg("--registry")
            .arg(&self.registry)
            .arg("--log")
            .arg(&self.log)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .no_console();
        cmd
    }

    /// Run a command, streaming each `{"progress":...}` stderr line to
    /// `on_progress`, and parse the single stdout line.
    fn run<T: for<'de> Deserialize<'de>>(
        &self,
        mut cmd: Command,
        label: &str,
        on_progress: Option<ProgressFn>,
    ) -> Result<T, String> {
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not run the runtime guard ({label}): {e}"))?;

        // stderr carries progress AND, when npm fails, the diagnosis; keep the
        // tail so a failure is reportable even if the JSON line never arrives.
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let reader = child.stderr.take().map(|err| {
            let tail = stderr_tail.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines() {
                    let Ok(line) = line else { break };
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                        if let Some(text) = v.get("progress").and_then(|p| p.as_str()) {
                            if let Some(p) = on_progress.as_ref() {
                                p(text.trim().to_string());
                            }
                            continue;
                        }
                    }
                    let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                    t.push_str(&line);
                    t.push('\n');
                    if t.len() > 8192 {
                        let cut = t.len() - 8192;
                        let cut = (cut..=t.len()).find(|i| t.is_char_boundary(*i)).unwrap_or(0);
                        *t = t[cut..].to_string();
                    }
                }
            })
        });

        let mut last_line = String::new();
        if let Some(out) = child.stdout.take() {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if !line.trim().is_empty() {
                    last_line = line;
                }
            }
        }
        let status = child
            .wait()
            .map_err(|e| format!("runtime guard ({label}) could not be awaited: {e}"))?;
        if let Some(handle) = reader {
            let _ = handle.join();
        }

        if last_line.trim().is_empty() {
            let tail = stderr_tail.lock().unwrap_or_else(|e| e.into_inner()).clone();
            return Err(format!(
                "runtime guard ({label}) produced no result (exit {:?})\n{}",
                status.code(),
                tail.trim()
            ));
        }

        let env: Envelope = serde_json::from_str(&last_line).map_err(|e| {
            format!("runtime guard ({label}) returned unparseable output: {e}\n{last_line}")
        })?;
        if !env.ok {
            let mut msg = env.error.unwrap_or_else(|| "unknown failure".into());
            if let Some(detail) = env.detail {
                msg.push_str("\n\n");
                msg.push_str(&detail);
            }
            return Err(msg);
        }
        serde_json::from_value(env.body)
            .map_err(|e| format!("runtime guard ({label}) result did not match: {e}"))
    }

    /// Reconcile an interrupted swap in every candidate location. MUST run
    /// before anything calls `Paths::runtime_dir()`.
    pub fn recover(&self, dirs: &[PathBuf]) -> Result<(), String> {
        let joined = dirs
            .iter()
            .map(|d| d.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(",");
        let mut cmd = self.base_command("recover");
        cmd.arg("--dirs").arg(joined);
        let _: serde_json::Value = self.run(cmd, "recover", None)?;
        Ok(())
    }

    pub fn verify(&self) -> Result<Verify, String> {
        self.run(self.base_command("verify"), "verify", None)
    }

    #[allow(dead_code)]
    pub fn verify_dir(&self, dir: &Path) -> Result<Verify, String> {
        let mut cmd = self.base_command("verify");
        cmd.arg("--dir").arg(dir);
        self.run(cmd, "verify", None)
    }

    pub fn installed(&self) -> Result<Installed, String> {
        self.run(self.base_command("installed"), "installed", None)
    }

    pub fn latest(&self) -> Result<Latest, String> {
        self.run(self.base_command("latest"), "latest", None)
    }

    /// staging install -> verify -> atomic swap. `spec` of `None` reinstalls
    /// what the lockfile pins (self-heal) rather than sneaking in an upgrade.
    pub fn provision(
        &self,
        spec: Option<&str>,
        reason: &str,
        on_progress: ProgressFn,
    ) -> Result<Installed, String> {
        let mut cmd = self.base_command("provision");
        cmd.arg("--reason").arg(reason);
        if let Some(s) = spec {
            cmd.arg("--spec").arg(s);
        }
        log_dbg!("provision[{reason}] spec={}", spec.unwrap_or("(lockfile)"));
        self.run(cmd, "provision", Some(on_progress))
    }
}
