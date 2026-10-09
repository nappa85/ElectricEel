//! Daily phone-key log files under Documents/ElectricEel (or
//! `$ELECTRIC_EEL_LOG_DIR`). Written by both the Rust core and the
//! tesla-session child so start/stop/resume and BLE presence share one
//! readable trail on the phone.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const ENV_DIR: &str = "ELECTRIC_EEL_LOG_DIR";
const KEEP_DAYS: i64 = 7;
// Shared with Go's keyLogMaxBytes. Both writers lock the same daily file.
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

static DAILY_LOG: Mutex<DailyLog> = Mutex::new(DailyLog {
    path: None,
    file: None,
});

struct DailyLog {
    path: Option<PathBuf>,
    file: Option<File>,
}

impl DailyLog {
    fn append(&mut self, dir: &Path, day: &str, line: &[u8]) -> io::Result<()> {
        let path = dir.join(format!("phone-key-{day}.log"));
        if self.path.as_ref() != Some(&path) || self.file.is_none() {
            self.file = None;
            fs::create_dir_all(dir)?;
            prune_old_logs(dir, day);
            let mut options = OpenOptions::new();
            options.create(true).append(true);
            options.mode(0o600);
            let mut file = options.open(&path)?;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            if file.metadata()?.len() == 0 {
                write_capped(&mut file, format!("# ElectricEel phone-key log {day}\n# tags: session presence connect auth link core keepalive ui\n").as_bytes())?;
            }
            self.path = Some(path);
            self.file = Some(file);
        }
        let result = write_capped(self.file.as_mut().unwrap(), line);
        if result.is_err() {
            self.file = None; // Retry opening on the next entry after an I/O failure.
        }
        result
    }
}

struct FileLock(i32);

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: the file remains open for the entire lock scope.
        unsafe { libc::flock(self.0, libc::LOCK_UN) };
    }
}

fn write_capped(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: fd belongs to the open file. Go uses flock on the same inode,
    // making the size check + append atomic between the two processes.
    if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let _lock = FileLock(fd);
    if file.metadata()?.len().saturating_add(bytes.len() as u64) <= MAX_LOG_BYTES {
        file.write_all(bytes)?;
    }
    Ok(())
}

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
    let mut log = DAILY_LOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let day = local_day_string();
    let line = format!("{}  {tag:<10}  {message}\n", local_stamp());
    let _ = log.append(&dir, &day, line.as_bytes());
    drop(log);
    eprintln!("phone-key: {tag}  {message}");
}

pub(crate) fn utc_stamp() -> String {
    // SAFETY: libc writes only into these stack-owned time structures.
    unsafe {
        let mut tv = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        libc::gettimeofday(&raw mut tv, std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::gmtime_r(&raw const tv.tv_sec, &raw mut tm);
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec,
            tv.tv_usec / 1000
        )
    }
}

fn local_stamp() -> String {
    // SAFETY: libc time/localtime_r/gettimeofday are process-global but we
    // only format into a stack buffer here; DAILY_LOG serializes file writes.
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

fn prune_old_logs(dir: &Path, today: &str) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let Ok(today_ord) = civil_ord(today) else {
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
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(());
    }
    // Days since civil 1970-01-01 (Howard Hinnant's days_from_civil):
    // correct across month lengths and leap years, unlike y*372+m*31+d.
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj.rem_euclid(400);
    let mp = (m + 9).rem_euclid(12);
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Ok(era * 146_097 + doe - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_log_rollover_prunes_and_switches_files() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("phone-key-2026-01-01.log");
        fs::write(&old, "old").unwrap();
        let mut log = DailyLog {
            path: None,
            file: None,
        };
        log.append(dir.path(), "2026-01-10", b"first\n").unwrap();
        assert!(!old.exists());
        fs::write(&old, "old").unwrap();
        log.append(dir.path(), "2026-01-10", b"second\n").unwrap();
        assert!(
            old.exists(),
            "steady-state appends should not enumerate/prune the directory"
        );
        log.append(dir.path(), "2026-01-11", b"third\n").unwrap();
        assert!(!old.exists());
        let first = fs::read_to_string(dir.path().join("phone-key-2026-01-10.log")).unwrap();
        assert!(first.ends_with("first\nsecond\n"));
        assert!(!first.contains("third"));
        assert!(
            fs::read_to_string(dir.path().join("phone-key-2026-01-11.log"))
                .unwrap()
                .ends_with("third\n")
        );
    }

    #[test]
    fn daily_log_caps_appends_without_truncating_existing_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.path().join("log"))
            .unwrap();
        file.set_len(MAX_LOG_BYTES - 2).unwrap();
        write_capped(&mut file, b"too long").unwrap();
        assert_eq!(file.metadata().unwrap().len(), MAX_LOG_BYTES - 2);
        write_capped(&mut file, b"ok").unwrap();
        write_capped(&mut file, b"x").unwrap();
        assert_eq!(file.metadata().unwrap().len(), MAX_LOG_BYTES);
    }

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

    #[test]
    fn test_civil_ord_counts_calendar_days() {
        // y*372+m*31+d assumes every month has 31 days, so Feb 22 -> Mar 1
        // 2026 (7 real calendar days) computes as 10. prune_old_logs deletes
        // >KEEP_DAYS, so a 7-day-old log is dropped early. Use real calendar
        // distance instead of a month*31 approximation.
        let feb22 = civil_ord("2026-02-22").unwrap();
        let mar01 = civil_ord("2026-03-01").unwrap();
        assert_eq!(
            mar01 - feb22,
            7,
            "Feb 22 -> Mar 1 2026 is 7 calendar days, not 10"
        );
    }
}
