//! Drop-based child reaping: the std-only equivalent of tokio's
//! `kill_on_drop`.
//!
//! There is deliberately no async runtime on the Rust side — everything
//! runs on plain threads with blocking waits (the Qt worker thread calls
//! into blocking C ABI functions by design), so pulling in tokio just for
//! this one property would drag a whole runtime for nothing. This wrapper
//! gives the same guarantee with std: whichever path drops the value —
//! normal return, `?` early-out, or panic unwind — the child is `SIGKILL`ed
//! and reaped, never leaked as a live process or left as a zombie.
//!
//! Explicit kill-and-wait (`SessionClient::kill`, `run_binary`'s timeout
//! arm) stays for promptness; this is the backstop. Drop never blocks
//! long (SIGKILL is uncatchable, so the reaping wait returns at once),
//! never panics (all results ignored), and never touches locks (safe
//! during unwind while a mutex guard is held).
use std::ops::{Deref, DerefMut};
use std::process::Child;

#[derive(Debug)]
pub(crate) struct KillOnDrop(pub(crate) Child);

impl Deref for KillOnDrop {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl DerefMut for KillOnDrop {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Both tests are Linux-gated on /proc for asserting the process is
    // actually gone (kill() succeeding is not proof: ESRCH is ignored by
    // design). This crate only ever runs on Linux (Sailfish target, CI
    // ubuntu), so the gate documents rather than limits.
    fn pid_gone(pid: u32) -> bool {
        !std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_drop_kills_live_child() {
        let child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("sleep must spawn");
        let pid = child.id();
        assert!(!pid_gone(pid), "sleep should be alive before drop");
        drop(KillOnDrop(child));
        let start = std::time::Instant::now();
        while !pid_gone(pid) {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "dropped child {pid} still alive after 5s"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn test_drop_on_reaped_child_is_noop() {
        let mut wrapper = KillOnDrop(
            std::process::Command::new("true")
                .spawn()
                .expect("true must spawn"),
        );
        // Explicit reap first: Drop must then ignore ESRCH/ECHILD, not panic.
        let _ = wrapper.wait();
        drop(wrapper);
    }
}
