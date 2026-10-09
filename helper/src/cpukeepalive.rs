//! MCE lease renewal on a Rust-owned thread, independent of UI and BLE work.
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::core::Core;

const ID: &str = "harbour-electric-eel-phone-key";
const RETRY: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

pub(crate) struct CpuKeepAlive {
    quit: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl CpuKeepAlive {
    pub(crate) fn new(core: Arc<Core>) -> Self {
        Self::at_address(core, None)
    }

    fn at_address(core: Arc<Core>, address: Option<String>) -> Self {
        let quit = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_quit = Arc::clone(&quit);
        let thread = thread::spawn(move || renew(&core, &worker_quit, address.as_deref()));
        Self {
            quit,
            thread: Some(thread),
        }
    }
}

impl Drop for CpuKeepAlive {
    fn drop(&mut self) {
        *self
            .quit
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.quit.1.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn interval(period: i32) -> Result<Duration, String> {
    let seconds = u64::try_from(period)
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| "invalid period reply".to_string())?;
    Ok(Duration::from_millis((seconds * 500).clamp(100, 15_000)))
}

fn call(connection: &zbus::blocking::Connection, method: &str) -> Result<zbus::Message, String> {
    connection
        .call_method(
            Some("com.nokia.mce"),
            "/com/nokia/mce/request",
            Some("com.nokia.mce.request"),
            method,
            &(ID,),
        )
        .map_err(|e| e.to_string())
}

fn hold(connection: &zbus::blocking::Connection, method: &str) -> Result<(), String> {
    let accepted: bool = call(connection, method)?
        .body()
        .deserialize()
        .map_err(|e| e.to_string())?;
    if accepted {
        Ok(())
    } else {
        Err("MCE rejected request".to_string())
    }
}

fn failure(method: &str, error: &str) {
    crate::keylog::log("keepalive", &format!("{method} failed: {error}"));
}

#[allow(clippy::too_many_lines)]
fn renew(core: &Core, quit: &(Mutex<bool>, Condvar), address: Option<&str>) {
    let mut connection = None;
    let mut requested = false;
    let mut first_hold = true;
    let mut period_known = false;
    let mut delay = RETRY;
    let mut backoff = RETRY;
    let mut next = Instant::now();
    loop {
        let guard = quit
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *guard {
            break;
        }
        drop(guard);
        // Sleep target: when disabled and idle, wait on the condvar with a
        // long deadline instead of waking 10x/sec; renewals use half the
        // granted period. Assigned fresh in every branch below.
        let mut sleep_for: Duration;
        if core.phone_key_enabled() {
            if Instant::now() >= next {
                if connection.is_none() {
                    let builder = address.map_or_else(
                        zbus::blocking::connection::Builder::system,
                        zbus::blocking::connection::Builder::address,
                    );
                    match builder.and_then(|b| b.method_timeout(RETRY).build()) {
                        Ok(bus) => {
                            connection = Some(bus);
                            backoff = RETRY;
                        }
                        Err(e) => {
                            failure("system bus connect", &e.to_string());
                            // Exponential backoff with cap instead of a 1s
                            // hammer when bluetoothd/MCE is down.
                            next = Instant::now() + backoff;
                            backoff = (backoff * 2).min(MAX_BACKOFF);
                        }
                    }
                }
                if let Some(bus) = &connection {
                    // A period query itself grants a short lease; stop it even
                    // if the subsequent start fails or the mode is disabled.
                    requested = true;
                    if !period_known {
                        let result = call(bus, "req_cpu_keepalive_period").and_then(|reply| {
                            let period: i32 =
                                reply.body().deserialize().map_err(|e| e.to_string())?;
                            interval(period).map(|delay| (period, delay))
                        });
                        match result {
                            Ok((period, granted_delay)) => {
                                delay = granted_delay;
                                period_known = true;
                                backoff = RETRY;
                                crate::keylog::log(
                                    "keepalive",
                                    &format!(
                                        "granted period={period}s renewal={}ms",
                                        delay.as_millis()
                                    ),
                                );
                            }
                            Err(e) => {
                                failure("req_cpu_keepalive_period", &e);
                                next = Instant::now() + backoff;
                                backoff = (backoff * 2).min(MAX_BACKOFF);
                            }
                        }
                    }
                    if period_known || delay != RETRY {
                        match hold(bus, "req_cpu_keepalive_start") {
                            Ok(()) => {
                                if first_hold {
                                    crate::keylog::log("keepalive", "first successful CPU hold");
                                    first_hold = false;
                                }
                                backoff = RETRY;
                                next = Instant::now() + delay;
                            }
                            Err(e) => {
                                failure("req_cpu_keepalive_start", &e);
                                // Reconnect after a lost bus; MCE drops leases when
                                // the sender disconnects. Re-query after reconnect.
                                connection = None;
                                period_known = false;
                                next = Instant::now() + backoff;
                                backoff = (backoff * 2).min(MAX_BACKOFF);
                            }
                        }
                    }
                } else {
                    next = Instant::now() + backoff;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
            // Sleep until the next renewal deadline (or quit).
            sleep_for = next.saturating_duration_since(Instant::now());
            if sleep_for.is_zero() {
                sleep_for = RETRY;
            }
        } else if requested {
            if let Some(bus) = &connection {
                if let Err(e) = hold(bus, "req_cpu_keepalive_stop") {
                    failure("req_cpu_keepalive_stop", &e);
                }
            }
            // Disconnect also releases any lease if stop failed.
            connection = None;
            requested = false;
            period_known = false;
            first_hold = true;
            delay = RETRY;
            backoff = RETRY;
            next = Instant::now();
            // Re-check promptly: mode may flip back to enabled at any time
            // (the test toggles stop/start within seconds).
            sleep_for = RETRY;
        } else {
            // Disabled and idle: park on the condvar instead of waking 10x/s.
            sleep_for = Duration::from_secs(30);
        }
        let guard = quit
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *guard {
            break;
        }
        let _ = quit.1.wait_timeout(guard, sleep_for);
    }
    if requested {
        if let Some(bus) = &connection {
            if let Err(e) = hold(bus, "req_cpu_keepalive_stop") {
                failure("req_cpu_keepalive_stop", &e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;

    struct Mce {
        calls: mpsc::Sender<(String, String)>,
        starts: usize,
    }

    #[zbus::interface(name = "com.nokia.mce.request")]
    impl Mce {
        #[zbus(name = "req_cpu_keepalive_period")]
        fn req_cpu_keepalive_period(&self, id: &str) -> i32 {
            self.calls.send(("period".into(), id.into())).unwrap();
            2
        }
        #[zbus(name = "req_cpu_keepalive_start")]
        fn req_cpu_keepalive_start(&mut self, id: &str) -> bool {
            self.calls.send(("start".into(), id.into())).unwrap();
            self.starts += 1;
            self.starts > 1
        }
        #[zbus(name = "req_cpu_keepalive_stop")]
        fn req_cpu_keepalive_stop(&self, id: &str) -> bool {
            self.calls.send(("stop".into(), id.into())).unwrap();
            true
        }
    }

    fn wait_for(receiver: &mpsc::Receiver<(String, String)>, method: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let (call, id) = receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            assert_eq!(id, ID);
            if call == method {
                return;
            }
        }
    }

    #[test]
    fn rust_lease_renews_during_failed_start_and_stops_without_any_ui() {
        let child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon installed for integration test");
        let mut daemon = crate::child::KillOnDrop(child);
        let mut address = String::new();
        BufReader::new(daemon.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let (sender, receiver) = mpsc::channel();
        let _server = zbus::blocking::connection::Builder::address(address.trim())
            .unwrap()
            .serve_at(
                "/com/nokia/mce/request",
                Mce {
                    calls: sender,
                    starts: 0,
                },
            )
            .unwrap()
            .name("com.nokia.mce")
            .unwrap()
            .build()
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config {
            vin: "5YJ3E1EA0PF000000".into(),
            vin_state: crate::config::VinState::Paired,
            ..crate::config::Config::default()
        };
        cfg.save(&dir.path().join("config.json")).unwrap();
        std::fs::write(dir.path().join("private_key.pem"), "private").unwrap();
        let state = dir.path().to_string_lossy().into_owned();
        let session = crate::session_client::SessionClient::new(
            dir.path().join("missing"),
            "bluez",
            dir.path().into(),
        );
        let core = Arc::new(Core::new(state.clone(), state, Some(session)).unwrap());
        assert!(core.start_phone_key().is_err());
        assert!(core.phone_key_enabled());
        {
            let lease = CpuKeepAlive::at_address(Arc::clone(&core), Some(address.trim().into()));
            wait_for(&receiver, "period");
            wait_for(&receiver, "start"); // Rejection.
            wait_for(&receiver, "start"); // Retry succeeds.
            wait_for(&receiver, "start"); // Renewal with no UI event loop.
            core.stop_phone_key();
            wait_for(&receiver, "stop");
            assert!(core.start_phone_key().is_err());
            wait_for(&receiver, "start");
            drop(lease); // Process shutdown also stops an enabled lease.
            wait_for(&receiver, "stop");
        }
        core.stop_phone_key();
    }

    #[test]
    fn renewal_period_is_bounded_and_invalid_grants_are_rejected() {
        assert_eq!(super::interval(60).unwrap().as_millis(), 15_000);
        assert_eq!(super::interval(2).unwrap().as_millis(), 1000);
        assert_eq!(super::interval(i32::MAX).unwrap().as_millis(), 15_000);
        assert!(super::interval(0).is_err());
        assert!(super::interval(-1).is_err());
    }
}
