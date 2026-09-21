//! applog.rs — a single, per-session log covering everything the app does
//! (resolved launch targets, spawned commands, setup steps, user actions),
//! plus an in-app way to export it so a user can hand it to whoever's
//! troubleshooting without needing a terminal.
//!
//! The trainer subprocess's own stdout/stderr is also redirected into this
//! same file (see launcher::launch_trainer), so one export captures the
//! whole picture for a failed launch.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static LOCK: Mutex<()> = Mutex::new(());

pub fn log_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local/share/steam-punk/steam-punk.log")
}

/// Starts a fresh log for this run — truncated rather than appended forever,
/// so an exported log stays focused on the session that actually hit the
/// problem being reported.
pub fn init() {
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, "");
    log(&format!(
        "Steam Punk v{} starting (timestamps below are UTC)",
        env!("CARGO_PKG_VERSION")
    ));
}

pub fn log(msg: &str) {
    let _guard = LOCK.lock().unwrap();
    let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path()) else {
        return;
    };
    let _ = writeln!(f, "[{}] {msg}", timestamp());
}

/// Bundles the app log with the privileged setup log (root-owned, written by
/// pkexec's privileged-setup.sh) into one file at `dest`, best-effort on the
/// latter since a normal user may not have read access to it.
pub fn export_to(dest: &std::path::Path) -> anyhow::Result<()> {
    let mut out = String::new();
    out.push_str("==== Steam Punk app log ====\n");
    match std::fs::read_to_string(log_path()) {
        Ok(s) => out.push_str(&s),
        Err(e) => out.push_str(&format!("(could not read: {e})\n")),
    }

    out.push_str(&format!(
        "\n==== privileged setup log ({}) ====\n",
        crate::setup::LOGFILE
    ));
    match std::fs::read_to_string(crate::setup::LOGFILE) {
        Ok(s) => out.push_str(&s),
        Err(e) => out.push_str(&format!("(not available: {e})\n")),
    }

    std::fs::write(dest, out)?;
    Ok(())
}

/// A filesystem-safe timestamp for default export filenames, e.g.
/// `20260804-231502`.
pub fn filename_timestamp() -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix(unix_now());
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn timestamp() -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix(unix_now());
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// Howard Hinnant's `civil_from_days` algorithm (public domain) — converts a
/// Unix timestamp to UTC Y/M/D H:M:S without pulling in a date/time crate
/// just for this one log line format.
fn civil_from_unix(unix: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = unix.div_euclid(86400);
    let secs_of_day = unix.rem_euclid(86400);
    let (h, mi, s) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );

    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };

    (y, m, d, h as u32, mi as u32, s as u32)
}
