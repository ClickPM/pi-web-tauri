//! One-time carry-over from the Electron builds.
//!
//! Two separate things move, and the second one is the expensive one:
//!
//!   1. STATE FILES (`theme-state.json`, `extensions-state.json`). Electron
//!      derived userData from `productName`, so earlier builds wrote to
//!      `%APPDATA%\Pi Agent`, `Pi Dsh`, `Pi&Dsh` or `pi-web-desktop`. The
//!      current build already reads `%APPDATA%\Pi` (see paths::user_data_dir),
//!      so the common upgrade path needs no copy at all — this covers the
//!      users who last ran one of the older names.
//!
//!   2. The READ-ONLY-INSTALL RUNTIME FALLBACK. A user who installed the
//!      Electron build into Program Files has ~900MB of copied runtime sitting
//!      in `<legacy userData>\runtime`. Leaving it there would make this build
//!      spend one to two minutes re-copying the identical tree from the seed.
//!      A same-volume rename moves it in milliseconds instead.
//!
//! The marker file is what stops (1) from resurrecting files the user has since
//! deleted on purpose. (2) is naturally idempotent: it only ever runs when the
//! destination is absent.

use std::path::Path;

use crate::log_dbg;
use crate::paths::{legacy_user_data_dirs, Paths};

const MARKER: &str = ".migrated-user-data";
const STATE_FILES: [&str; 2] = ["theme-state.json", "extensions-state.json"];

pub fn run(paths: &Paths) {
    migrate_state_files(paths.user_data_dir());
    adopt_legacy_runtime(paths);
}

fn migrate_state_files(now: &Path) {
    let marker = now.join(MARKER);
    if marker.exists() {
        return;
    }

    let sources: Vec<_> = legacy_user_data_dirs()
        .into_iter()
        .filter(|d| d != now && d.exists())
        .collect();
    if sources.is_empty() {
        return;
    }

    let _ = std::fs::create_dir_all(now);
    let mut carried: Vec<String> = Vec::new();
    for name in STATE_FILES {
        let to = now.join(name);
        // Never overwrite: a file already here was written by the current build
        // and is newer than anything a legacy directory holds.
        if to.exists() {
            continue;
        }
        let Some(from) = sources.iter().map(|d| d.join(name)).find(|p| p.exists()) else {
            continue;
        };
        if std::fs::copy(&from, &to).is_ok() {
            let origin = from
                .parent()
                .and_then(Path::file_name)
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            carried.push(format!("{name} <- {origin}"));
        }
    }

    let searched = sources
        .iter()
        .filter_map(|d| d.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", ");
    let body = format!(
        "{}\r\nsearched: {searched}\r\ncarried: {}\r\n",
        crate::log::iso_now(),
        if carried.is_empty() {
            "(nothing)".to_string()
        } else {
            carried.join("; ")
        }
    );
    // Never block startup on this: the worst case is the old defaults are gone.
    let _ = std::fs::write(&marker, body);
    log_dbg!(
        "userData migration: carried {} file(s) [{}]",
        carried.len(),
        carried.join("; ")
    );
}

/// Move a legacy read-only-install runtime into place instead of re-copying the
/// seed. Only runs when this build has no runtime of its own yet.
fn adopt_legacy_runtime(paths: &Paths) {
    // In-place (writable install) never uses the fallback dir at all.
    if paths.runtime_is_in_place() {
        return;
    }
    let target = paths.fallback_runtime_dir();
    if target.exists() {
        return;
    }

    for legacy_user_data in legacy_user_data_dirs() {
        let candidate = legacy_user_data.join("runtime");
        // `.seeded` is what ensureRuntime() writes once a copy completed; an
        // interrupted copy has no marker and must NOT be adopted.
        if !candidate.join(".seeded").exists() {
            continue;
        }
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::rename(&candidate, &target) {
            Ok(()) => {
                log_dbg!(
                    "adopted legacy runtime {} -> {} (skipped a ~900MB re-copy)",
                    candidate.display(),
                    target.display()
                );
                return;
            }
            Err(e) => {
                // Different volume, or a stray handle. Falling through means the
                // ordinary seed copy runs — slow, but correct.
                log_dbg!(
                    "could not adopt legacy runtime {}: {e} — the seed copy will run instead",
                    candidate.display()
                );
            }
        }
    }
}
