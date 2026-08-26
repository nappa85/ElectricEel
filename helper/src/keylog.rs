//! Daily phone-key log files under Documents/ElectricEel (or
//! `$ELECTRIC_EEL_LOG_DIR`). Written by both the Rust core and the
//! tesla-session child so start/stop/resume and BLE presence share one
//! readable trail on the phone.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const ENV_DIR: &str = "ELECTRIC_EEL_LOG_DIR";
const KEEP_DAYS: i64 = 7;

static LOG_MU: Mutex<()> = Mutex::new(());

/// Directory for phone-key logs: env override, else `$HOME/Documents/ElectricEel`.
#[must_use]
pub(crate) fn log_dir() -> PathBuf {
    if let Ok(dir) = std::env::var(ENV_DIR) {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join("Documents").join("ElectricEel")
}

/// Append one line to today's `phone-key-YYYY-MM-DD.log`. Never panics.
pub(crate) fn log(tag: &str, message: &str) {
    let dir = log_dir();
    let _guard = LOG_MU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = fs::create_dir_all(&dir);
    prune_old_logs(&dir);
    let day = local_day_string();
    let path = dir.join(format!("phone-key-{day}.log"));
    let created = !path.exists();
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        if created {
            let _ = writeln!(f, "# ElectricEel phone-key log {day}");
            let _ = writeln!(f, "# tags: session presence connect auth link core");
        }
        let _ = writeln!(f, "{}  {:<10}  {message}", local_stamp(), tag);
    }
    eprintln!("phone-key: {tag}  {message}");
}

fn local_stamp() -> String {
    // SAFETY: libc time/localtime_r/gettimeofday are process-global but we
    // only format into a stack buffer here; LOG_MU serializes file writes.
    unsafe {
        let mut ts: libc::time_t = 0;
        libc::time(&raw mut ts);
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&raw const ts, &raw mut tm);
        let mut tv = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        libc::gettimeofday(&raw mut tv, std::ptr::null_mut());
        let ms = u32::try_from(tv.tv_usec / 1000).unwrap_or(0);
        format!(
            "{:02}:{:02}:{:02}.{:03}",
            tm.tm_hour, tm.tm_min, tm.tm_sec, ms
        )
    }
}

fn local_day_string() -> String {
    // SAFETY: see local_stamp.
    unsafe {
        let mut ts: libc::time_t = 0;
        libc::time(&raw mut ts);
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&raw const ts, &raw mut tm);
        format!(
            "{:04}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        )
    }
}

fn prune_old_logs(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let today = local_day_string();
    let Ok(today_ord) = civil_ord(&today) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(day) = name
            .strip_prefix("phone-key-")
            .and_then(|s| s.strip_suffix(".log"))
        else {
            continue;
        };
        let Ok(ord) = civil_ord(day) else {
            continue;
        };
        if today_ord - ord > KEEP_DAYS {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn civil_ord(day: &str) -> Result<i64, ()> {
    let parts: Vec<_> = day.split('-').collect();
    if parts.len() != 3 {
        return Err(());
    }
    let y: i64 = parts[0].parse().map_err(|_| ())?;
    let m: i64 = parts[1].parse().map_err(|_| ())?;
    let d: i64 = parts[2].parse().map_err(|_| ())?;
    Ok(y * 372 + m * 31 + d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_civil_ord_orders_dates() {
        let a = civil_ord("2026-08-16").unwrap();
        let b = civil_ord("2026-08-24").unwrap();
        assert!(b - a > KEEP_DAYS);
        assert!(b > a);
    }

    #[test]
    fn test_local_day_string_looks_like_iso_date() {
        let day = local_day_string();
        assert_eq!(day.len(), 10);
        assert_eq!(&day[4..5], "-");
        assert_eq!(&day[7..8], "-");
    }
}
