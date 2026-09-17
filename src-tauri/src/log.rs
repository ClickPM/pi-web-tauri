//! Debug log — `%TEMP%/pi-web-desktop-debug.log`.
//!
//! Same file, same one-line-per-event format as the Electron shell's `dbg()`,
//! because that file is how every production boot failure so far has actually
//! been diagnosed. `guard-cli.js` appends to it too (tagged `[guard]`), so one
//! file still tells the whole story across the process boundary.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn log_path() -> PathBuf {
    std::env::temp_dir().join("pi-web-desktop-debug.log")
}

/// Wall-clock stamp in the same shape as JS's `new Date().toISOString()`, built
/// by hand so the shell does not take a date-time crate just for a log prefix.
pub fn iso_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let ms = now.subsec_millis();

    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    // Civil-from-days (Howard Hinnant's algorithm), shifted to a March-based year.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
}

pub fn dbg(msg: impl AsRef<str>) {
    let line = format!("[{}] {}\n", iso_now(), msg.as_ref());
    // Mirrored to stdout so `tauri dev` shows the same trace without tailing a file.
    print!("{line}");
    let _ = std::io::stdout().flush();
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `dbg!` is std's; this one is ours.
#[macro_export]
macro_rules! log_dbg {
    ($($arg:tt)*) => { $crate::log::dbg(format!($($arg)*)) };
}
