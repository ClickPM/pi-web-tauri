//! Where everything lives — a direct port of main.js's path helpers.
//!
//! Two decisions here are load-bearing and are explained where they are made:
//!   * `runtime_dir()` reproduces main.js's in-place-vs-copy choice, INCLUDING
//!     its cache, and must not be called before the swap recovery has run.
//!   * `user_data_dir()` deliberately does NOT use Tauri's identifier-derived
//!     directory. See the comment on it.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::log_dbg;

/// Product name, as both Electron builds spelled it. Drives `user_data_dir()`.
pub const PRODUCT_NAME: &str = "Pi";

pub struct Paths {
    /// `tauri::is_dev()` — resources are laid out differently in the two modes.
    dev: bool,
    /// dev: the repo root. release: the app's resource dir.
    base: PathBuf,
    user_data: PathBuf,
    runtime: OnceLock<PathBuf>,
}

impl Paths {
    pub fn resolve(app: &tauri::AppHandle) -> Self {
        use tauri::Manager;

        let dev = tauri::is_dev();
        let base = if dev {
            // CARGO_MANIFEST_DIR is src-tauri/; the build inputs sit one level up.
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        } else {
            app.path()
                .resource_dir()
                .expect("resource dir must resolve in a packaged build")
        };

        let user_data = default_user_data_dir();
        let _ = std::fs::create_dir_all(&user_data);

        let p = Self {
            dev,
            base,
            user_data,
            runtime: OnceLock::new(),
        };
        log_dbg!(
            "paths: dev={} base={} userData={}",
            p.dev,
            p.base.display(),
            p.user_data.display()
        );
        p
    }

    /// Bundled Node.js runtime — `vendor/node` in dev, `resources/node` packaged.
    pub fn node_dir(&self) -> PathBuf {
        if self.dev {
            self.base.join("vendor").join("node")
        } else {
            self.base.join("node")
        }
    }

    pub fn node_exe(&self) -> PathBuf {
        self.node_dir().join("node.exe")
    }

    pub fn npm_cli(&self) -> PathBuf {
        self.node_dir()
            .join("node_modules")
            .join("npm")
            .join("bin")
            .join("npm-cli.js")
    }

    /// The shipped `@agegr/pi-web` production install.
    pub fn seed_dir(&self) -> PathBuf {
        if self.dev {
            self.base.join("runtime-seed")
        } else {
            self.base.join("runtime-seed")
        }
    }

    /// runtime-guard.js / updater.js / guard-cli.js.
    pub fn guard_dir(&self) -> PathBuf {
        if self.dev {
            self.base
                .join("src-tauri")
                .join("resources")
                .join("guard")
        } else {
            self.base.join("guard")
        }
    }

    pub fn guard_cli(&self) -> PathBuf {
        self.guard_dir().join("guard-cli.js")
    }

    /// Per-user state: `port.json`, `theme-state.json`, the migration marker,
    /// and the read-only-install runtime fallback.
    pub fn user_data_dir(&self) -> &Path {
        &self.user_data
    }

    /// Where a read-only install copies the seed to.
    pub fn fallback_runtime_dir(&self) -> PathBuf {
        self.user_data.join("runtime")
    }

    /// Both places a runtime can live. `recover` must sweep BOTH before anything
    /// calls `runtime_dir()`, because a crash between the swap's two renames
    /// leaves the seed dir momentarily absent — and `runtime_dir()` caches its
    /// answer forever.
    pub fn runtime_candidates(&self) -> Vec<PathBuf> {
        vec![self.seed_dir(), self.fallback_runtime_dir()]
    }

    /// main.js's `runtimeDir()`, cache and all.
    ///
    /// Preferred: run pi-web IN PLACE from the (writable) install dir — instant,
    /// no first-run copy. Per-user installs and `tauri dev` are writable.
    /// Fallback (read-only install, e.g. Program Files): a writable user dir.
    pub fn runtime_dir(&self) -> &Path {
        self.runtime.get_or_init(|| {
            let seed = self.seed_dir();
            if seed.exists() && is_writable(&seed) {
                log_dbg!("runtimeDir = seed (in-place, writable): {}", seed.display());
                seed
            } else {
                let fallback = self.fallback_runtime_dir();
                log_dbg!("runtimeDir = userData (seed read-only): {}", fallback.display());
                fallback
            }
        })
    }

    pub fn runtime_is_in_place(&self) -> bool {
        self.runtime_dir() == self.seed_dir().as_path()
    }

    pub fn pi_web_pkg_dir(&self) -> PathBuf {
        self.runtime_dir()
            .join("node_modules")
            .join("@agegr")
            .join("pi-web")
    }

    /// `bin/pi-web.js` — the launcher we spawn, NOT next's own bin. It is what
    /// understands `--no-open`, `PI_WEB_PASSWORD` and the loopback host check.
    pub fn pi_web_bin(&self) -> PathBuf {
        self.pi_web_pkg_dir().join("bin").join("pi-web.js")
    }

    /// The pi CLI package inside our runtime — i.e. what `pi` actually IS here.
    pub fn bundled_pi_agent_dir(&self) -> PathBuf {
        self.runtime_dir()
            .join("node_modules")
            .join("@earendil-works")
            .join("pi-coding-agent")
    }
}

/// `%APPDATA%\Pi`.
///
/// NOT Tauri's `app_data_dir()`, which would be `%APPDATA%\com.agegr.piwebdesktop`.
/// Electron derived userData from `productName`, so every existing install
/// already keeps `theme-state.json` — and, on read-only installs, a ~900MB
/// copied runtime — under `%APPDATA%\Pi`. Pointing at the same directory turns
/// the upgrade from "re-copy a gigabyte and reset the user's theme" into a
/// no-op. Tauri's own WebView2 profile stays where Tauri puts it; the two never
/// needed to be the same directory.
fn default_user_data_dir() -> PathBuf {
    let roaming = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs_home()
                .join("AppData")
                .join("Roaming")
        });
    roaming.join(PRODUCT_NAME)
}

fn dirs_home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Legacy userData dirs from earlier product names, newest intent first.
/// `pi-web-desktop` is the pre-rename package name; the rest are Electron
/// `productName` values this app shipped under.
pub fn legacy_user_data_dirs() -> Vec<PathBuf> {
    let parent = match default_user_data_dir().parent() {
        Some(p) => p.to_path_buf(),
        None => return Vec::new(),
    };
    ["Pi Agent", "Pi Dsh", "Pi&Dsh", "pi-web-desktop"]
        .iter()
        .map(|n| parent.join(n))
        .collect()
}

/// main.js's `isWritable()`: actually write a probe file rather than trusting
/// permissions metadata, because that is what the failure mode is about.
pub fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".wtest-{}", std::process::id()));
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}
