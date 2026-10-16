//! Pure application policy; no Go state machine or wall-clock comparisons.
use std::time::{Duration, Instant};

/// A phone-key lifecycle/state event. The wire name is the snake-cased
/// variant with a `presence_` prefix (`AuthOk` -> `"presence_auth_ok"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr, strum::Display)]
#[strum(serialize_all = "snake_case", prefix = "presence_")]
pub(crate) enum PresenceEvent {
    Near,
    Far,
    Inside,
    AuthOk,
    AuthFailed,
    Disconnected,
    Error,
    Stopped,
    Restarted,
    AwayConfirmed,
}

impl serde::Serialize for PresenceEvent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.into())
    }
}

/// RSSI at or above which an advertisement counts toward entering "near".
/// Field logs show an approaching car mostly between -91 and -85 dBm.
pub(crate) const ENTER_RSSI: i32 = -90;
/// RSSI at or above which a single advertisement enters "near" at once: the
/// phone is next to the car, so waiting for a streak only delays unlocking.
pub(crate) const IMMEDIATE_RSSI: i32 = -80;
/// RSSI below which an advertisement is "far": it resets the confirmation
/// streak and never refreshes presence. Between the two thresholds a sample
/// only keeps an already-near vehicle near (hysteresis), so a beacon hovering
/// around one threshold cannot flap.
pub(crate) const EXIT_RSSI: i32 = -95;
/// Consecutive strong samples needed to enter "near"; a single reflection
/// spike must not trigger a connection attempt.
pub(crate) const ENTER_CONFIRMATIONS: u32 = 2;
/// A strong sample older than this no longer counts toward the streak. The
/// car advertises every ~4.5-7 s, so this must span more than one interval.
pub(crate) const CONFIRMATION_WINDOW: Duration = Duration::from_secs(15);

pub(crate) struct Proximity {
    pub near: bool,
    confirmations: u32,
    last_strong: Option<Instant>,
    last_seen: Option<Instant>,
    pub target: u64,
    pub target_at: Option<Instant>,
    pub retry_at: Option<Instant>,
    backoff: Duration,
}

impl Default for Proximity {
    fn default() -> Self {
        Self {
            near: false,
            confirmations: 0,
            last_strong: None,
            last_seen: None,
            target: 0,
            target_at: None,
            retry_at: None,
            backoff: Duration::ZERO,
        }
    }
}

impl Proximity {
    pub fn advertisement(&mut self, now: Instant, target: u64, rssi: i32) {
        if rssi >= ENTER_RSSI {
            let streak = self
                .last_strong
                .is_some_and(|at| now.saturating_duration_since(at) <= CONFIRMATION_WINDOW);
            self.confirmations = if streak {
                self.confirmations.saturating_add(1)
            } else {
                1
            };
            self.last_strong = Some(now);
            if self.confirmations >= ENTER_CONFIRMATIONS || rssi >= IMMEDIATE_RSSI {
                self.near = true;
            }
        } else if rssi < EXIT_RSSI {
            self.confirmations = 0;
            self.last_strong = None;
            self.target = 0;
            self.target_at = None;
            return;
        } else if !self.near {
            // Hysteresis band: neither confirms nor resets a pending streak.
            return;
        }
        if self.near {
            self.last_seen = Some(now);
            self.target = target;
            self.target_at = Some(now);
        }
    }
    pub fn tick(&mut self, now: Instant, connected: bool) -> bool {
        if connected {
            self.near = true;
            self.last_seen = Some(now);
            return false;
        }
        if self.near
            && self
                .last_seen
                .is_some_and(|seen| now.duration_since(seen) > Duration::from_secs(15))
        {
            self.near = false;
            self.confirmations = 0;
            self.last_strong = None;
            self.target = 0;
            self.target_at = None;
            self.retry_at = None;
            self.backoff = Duration::ZERO;
            return true;
        }
        false
    }
    pub fn can_connect(&self, now: Instant) -> bool {
        self.near
            && self.target != 0
            && self
                .target_at
                .is_some_and(|seen| now.duration_since(seen) <= Duration::from_secs(4))
            && self.retry_at.is_none_or(|deadline| now >= deadline)
    }
    pub fn failed(&mut self, now: Instant) {
        self.backoff = (self.backoff * 2)
            .max(Duration::from_secs(1))
            .min(Duration::from_secs(60));
        self.retry_at = Some(now + self.backoff);
    }
    pub fn connected(&mut self) {
        self.backoff = Duration::ZERO;
        self.retry_at = None;
    }
}

pub(crate) fn inside(connected: Instant, status_at: Instant, now: Instant, present: bool) -> bool {
    present
        && now.duration_since(connected) >= Duration::from_secs(45)
        && now.duration_since(status_at) <= Duration::from_secs(30)
}

/// A single runtime-owned deadline, independent of Automagic flow concurrency.
/// Raw `presence_far` remains immediate. Only `presence_inside` cancels a departure
/// within a session; stopped/restarted sessions and VIN changes discard it.
#[derive(Default)]
pub(crate) struct AwayDeadline {
    pending: Option<PendingAway>,
}

struct PendingAway {
    deadline: Instant,
    vin: String,
    generation: u64,
}

impl AwayDeadline {
    pub(crate) fn observe(
        &mut self,
        kind: PresenceEvent,
        vin: &str,
        generation: u64,
        now: Instant,
    ) {
        self.discard_other_session(vin, generation);
        match kind {
            PresenceEvent::Far if self.pending.is_none() => {
                self.pending = Some(PendingAway {
                    deadline: now + Duration::from_secs(180),
                    vin: vin.into(),
                    generation,
                });
            }
            PresenceEvent::Inside | PresenceEvent::Stopped | PresenceEvent::Restarted => {
                self.pending = None;
            }
            _ => (),
        }
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|away| away.deadline)
    }

    pub(crate) fn take_due(&mut self, now: Instant, vin: &str, generation: u64) -> Option<String> {
        self.discard_other_session(vin, generation);
        if self.deadline().is_some_and(|deadline| now >= deadline) {
            return self.pending.take().map(|away| away.vin);
        }
        None
    }

    fn discard_other_session(&mut self, vin: &str, generation: u64) {
        if self
            .pending
            .as_ref()
            .is_some_and(|away| away.vin != vin || away.generation != generation)
        {
            self.pending = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn connected_link_survives_missing_advertisements_and_stale_target_cannot_reconnect() {
        let now = Instant::now();
        let mut state = Proximity::default();
        state.advertisement(now, 1, -80);
        state.advertisement(now, 1, -80);
        assert!(state.can_connect(now));
        assert!(!state.tick(now + Duration::from_secs(60), true));
        assert!(state.near);
        assert!(!state.can_connect(now + Duration::from_secs(60)));
        assert!(state.tick(now + Duration::from_secs(76), false));
        assert!(!state.near);
    }
    #[test]
    fn weak_samples_and_failures_do_not_spin_reconnects() {
        let now = Instant::now();
        let mut state = Proximity::default();
        state.advertisement(now, 1, -100);
        assert!(!state.can_connect(now));
        state.advertisement(now, 2, -80);
        state.advertisement(now, 2, -80);
        state.failed(now);
        assert!(!state.can_connect(now));
        assert!(state.can_connect(now + Duration::from_secs(1)));
        state.advertisement(now + Duration::from_secs(2), 3, -100);
        assert!(!state.can_connect(now + Duration::from_secs(2)));
    }

    #[test]
    fn entering_near_needs_consecutive_strong_samples() {
        let now = Instant::now();
        let mut state = Proximity::default();
        state.advertisement(now, 1, ENTER_RSSI);
        assert!(!state.near, "one sample is not a confirmation streak");
        // A stale first sample does not combine with a fresh one.
        let later = now + CONFIRMATION_WINDOW + Duration::from_secs(1);
        state.advertisement(later, 1, ENTER_RSSI);
        assert!(!state.near);
        // Samples in the hysteresis band neither confirm nor reset.
        state.advertisement(later, 1, EXIT_RSSI);
        assert!(!state.near);
        state.advertisement(later + Duration::from_secs(1), 1, ENTER_RSSI);
        assert!(state.near);
        assert!(state.can_connect(later + Duration::from_secs(1)));
        // A far sample resets a pending streak.
        let mut state = Proximity::default();
        state.advertisement(now, 1, ENTER_RSSI);
        state.advertisement(now, 1, EXIT_RSSI - 1);
        state.advertisement(now, 1, ENTER_RSSI);
        assert!(!state.near);
    }

    #[test]
    fn field_advertising_cadence_enters_near() {
        // Recorded arrivals: strong samples ~5.2 s apart, the second only
        // just above the band, and a lone sample right next to the car.
        let now = Instant::now();
        let mut state = Proximity::default();
        state.advertisement(now, 1, -90);
        assert!(!state.near);
        state.advertisement(now + Duration::from_millis(5226), 1, -89);
        assert!(state.near);
        let mut state = Proximity::default();
        state.advertisement(now, 1, -53);
        assert!(state.near);
        assert!(state.can_connect(now));
    }

    #[test]
    fn hysteresis_band_keeps_a_near_vehicle_near_until_samples_turn_far() {
        let now = Instant::now();
        let mut state = Proximity::default();
        state.advertisement(now, 1, -60);
        state.advertisement(now, 1, -60);
        assert!(state.near);
        // Fading into the band keeps presence and the target fresh.
        let mut at = now;
        for _ in 0..10 {
            at += Duration::from_secs(5);
            state.advertisement(at, 2, i32::midpoint(ENTER_RSSI, EXIT_RSSI));
            assert!(!state.tick(at, false), "band samples must not depart");
        }
        assert!(state.can_connect(at));
        assert_eq!(state.target, 2);
        // Far samples do not refresh presence: departure follows the timeout.
        for _ in 0..4 {
            at += Duration::from_secs(5);
            state.advertisement(at, 3, EXIT_RSSI - 1);
        }
        assert!(!state.can_connect(at));
        assert!(state.tick(at, false));
        assert!(!state.near);
        // Re-entering needs a fresh confirmation streak.
        state.advertisement(at, 4, ENTER_RSSI);
        assert!(!state.near);
    }

    #[test]
    fn inside_requires_settled_connection_and_fresh_positive_vehicle_report() {
        let connected = Instant::now();
        let now = connected + Duration::from_secs(46);
        assert!(inside(connected, now, now, true));
        assert!(!inside(connected, now, now, false));
        assert!(!inside(now, now, now, true));
        assert!(!inside(connected, connected, now, true));
    }

    #[test]
    fn repeated_departures_keep_first_deadline_and_expire_only_once() {
        let now = Instant::now();
        let mut away = AwayDeadline::default();
        away.observe(PresenceEvent::Far, "VIN", 1, now);
        away.observe(PresenceEvent::Far, "VIN", 1, now + Duration::from_secs(120));
        assert!(away
            .take_due(now + Duration::from_secs(179), "VIN", 1)
            .is_none());
        assert_eq!(
            away.take_due(now + Duration::from_secs(180), "VIN", 1),
            Some("VIN".into())
        );
        assert!(away
            .take_due(now + Duration::from_secs(300), "VIN", 1)
            .is_none());
    }

    #[test]
    fn inside_at_deadline_cancels_and_next_departure_gets_a_fresh_timer() {
        let now = Instant::now();
        let mut away = AwayDeadline::default();
        away.observe(PresenceEvent::Far, "VIN", 1, now);
        let later = now + Duration::from_secs(180);
        away.observe(PresenceEvent::Inside, "VIN", 1, later);
        assert!(away.take_due(later, "VIN", 1).is_none());
        away.observe(PresenceEvent::Far, "VIN", 1, later);
        assert!(away
            .take_due(later + Duration::from_secs(179), "VIN", 1)
            .is_none());
        assert_eq!(
            away.take_due(later + Duration::from_secs(180), "VIN", 1),
            Some("VIN".into())
        );
    }

    #[test]
    fn only_inside_cancels_within_session_but_stops_and_identity_changes_discard_timer() {
        let now = Instant::now();
        let later = now + Duration::from_secs(180);
        for kind in [
            PresenceEvent::Near,
            PresenceEvent::AuthOk,
            PresenceEvent::Disconnected,
            PresenceEvent::Error,
        ] {
            let mut away = AwayDeadline::default();
            away.observe(PresenceEvent::Far, "VIN", 1, now);
            away.observe(kind, "VIN", 1, now);
            assert_eq!(away.take_due(later, "VIN", 1), Some("VIN".into()));
        }
        for kind in [PresenceEvent::Stopped, PresenceEvent::Restarted] {
            let mut away = AwayDeadline::default();
            away.observe(PresenceEvent::Far, "VIN", 1, now);
            away.observe(kind, "VIN", 1, now);
            assert!(away.take_due(later, "VIN", 1).is_none());
        }
        for (vin, generation) in [("OTHER", 1), ("VIN", 2)] {
            let mut away = AwayDeadline::default();
            away.observe(PresenceEvent::Far, "VIN", 1, now);
            assert!(away.take_due(later, vin, generation).is_none());
            assert!(away.take_due(later, "VIN", 1).is_none());
        }
    }
}
