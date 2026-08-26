package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/teslamotors/vehicle-command/pkg/protocol"
)

// execute() and the commands map are vendored verbatim from upstream (see
// commands_vendor.go) - these tests exercise the same readiness-check code
// path production traffic goes through, without needing a live vehicle,
// the same spirit as this project's existing helper/src tests ("Run
// (unknown command, ...)" against a fake tesla-control stand-in rather than
// a real BLE device).
func TestExecuteReadinessChecks(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()

	cases := []struct {
		name    string
		args    []string
		wantErr error
	}{
		{"unknown command", []string{"definitely-not-a-command"}, ErrUnknownCommand},
		{"missing VIN (no car)", []string{"lock"}, ErrRequiresVIN},
		{"FleetAPI-only command with no account", []string{"product-info"}, ErrRequiresOAuth},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			err := execute(ctx, nil, nil, c.args)
			if !errors.Is(err, c.wantErr) {
				t.Errorf("execute(%v) = %v, want %v", c.args, err, c.wantErr)
			}
		})
	}
}

func TestCaptureOutputIsolatesAndRestores(t *testing.T) {
	origOut, origErr := os.Stdout, os.Stderr

	stdout, stderr := captureOutput(func() {
		fmt.Println("hello stdout")
		fmt.Fprintln(os.Stderr, "hello stderr")
	})

	if !strings.Contains(stdout, "hello stdout") {
		t.Errorf("stdout = %q, want it to contain %q", stdout, "hello stdout")
	}
	if !strings.Contains(stderr, "hello stderr") {
		t.Errorf("stderr = %q, want it to contain %q", stderr, "hello stderr")
	}
	if os.Stdout != origOut || os.Stderr != origErr {
		t.Fatal("captureOutput did not restore os.Stdout/os.Stderr")
	}

	// A second call must not see leftover state from the first (guards
	// against the pipe-closing/goroutine-draining logic leaking a stale
	// reader across calls).
	stdout2, _ := captureOutput(func() { fmt.Println("second call") })
	if strings.Contains(stdout2, "hello stdout") {
		t.Errorf("second capture leaked first call's output: %q", stdout2)
	}
}

// TestSessionDomainsScopesBodyControllerState checks the one command whose
// domain restriction actually goes through commands_vendor.go's own field
// (session-info takes its domain as a runtime argument instead, so it's
// correctly nil here - no static override wanted).
func TestSessionDomainsScopesBodyControllerState(t *testing.T) {
	cases := []struct {
		cmd  string
		want []protocol.Domain
	}{
		{"body-controller-state", []protocol.Domain{protocol.DomainVCSEC}},
		{"state", []protocol.Domain{protocol.DomainInfotainment}},
		{"session-info", nil},
		{"lock", nil},
		{"unlock", nil},
		{"add-key-request", nil}, // skips StartSession entirely - see commandsWithoutSession
		{"", []protocol.Domain{protocol.DomainVCSEC}}, // presence mode: VCSEC only
		{"not-a-real-command", nil},
	}
	for _, c := range cases {
		t.Run(c.cmd, func(t *testing.T) {
			got := sessionDomains(c.cmd)
			if !reflect.DeepEqual(got, c.want) {
				t.Errorf("sessionDomains(%q) = %v, want %v", c.cmd, got, c.want)
			}
		})
	}
}

// TestCommandsWithoutSessionSkipsStartSession is a regression test for a
// real bug, found live against a real vehicle: ensureConnectedLocked used
// to call StartSession unconditionally, but StartSession's own SessionInfo
// handshake rejects an unrecognized key for every domain once the vehicle
// already has other keys enrolled - VCSEC included, so scoping the domain
// down doesn't help either (a prior, insufficient version of this fix
// tried exactly that). add-key-request's handler never depends on
// StartSession succeeding (it sends an unauthenticated, self-identifying
// message directly), so it must skip StartSession rather than request a
// narrower one.
func TestCommandsWithoutSessionSkipsStartSession(t *testing.T) {
	if !commandsWithoutSession["add-key-request"] {
		t.Error(`commandsWithoutSession["add-key-request"] = false, want true`)
	}
	for _, cmd := range []string{"lock", "unlock", "add-key", "list-keys", ""} {
		if commandsWithoutSession[cmd] {
			t.Errorf("commandsWithoutSession[%q] = true, want false", cmd)
		}
	}
}

func TestEnsureConnectedMissingKeyFile(t *testing.T) {
	s := &session{
		vin:            "5YJ3E1EA0PF000000",
		keyFile:        "/nonexistent/path/private_key.pem",
		connectTimeout: 2 * time.Second,
	}
	s.mu.Lock()
	defer s.mu.Unlock()

	err := s.ensureConnectedLocked(context.Background(), "lock", nil)
	if err == nil {
		t.Fatal("expected an error loading a private key that doesn't exist, got nil")
	}
	if s.car != nil {
		t.Error("session.car should remain nil after a failed connect")
	}
}

func TestDispatchReportsConnectFailure(t *testing.T) {
	// No real adapter/vehicle in this environment - this is exactly the
	// "can't verify without hardware" boundary. What *is* verifiable here:
	// a connect failure is surfaced as a well-formed, non-panicking
	// response rather than as a hang or a crash.
	s := &session{
		vin:            "5YJ3E1EA0PF000000",
		keyFile:        "/nonexistent/path/private_key.pem",
		connectTimeout: 500 * time.Millisecond,
		commandTimeout: 500 * time.Millisecond,
		idleTimeout:    time.Second,
	}
	resp := s.dispatch(request{ID: "t1", Cmd: "lock"})
	if resp.OK {
		t.Fatal("expected ok=false with no reachable vehicle")
	}
	if resp.ID != "t1" {
		t.Errorf("response ID = %q, want %q (request/response correlation)", resp.ID, "t1")
	}
	if resp.Stderr == "" {
		t.Error("expected a non-empty stderr explaining the failure")
	}
}

func TestRequestResponseJSONRoundTrip(t *testing.T) {
	req := request{ID: "abc", Cmd: "seat-heater", Args: []string{"front-left", "2"}}
	data, err := json.Marshal(req)
	if err != nil {
		t.Fatal(err)
	}
	var got request
	if err := json.Unmarshal(data, &got); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(got, req) {
		t.Errorf("round-trip mismatch: got %+v, want %+v", got, req)
	}

	resp := response{ID: "abc", OK: true, Stdout: "ok", ExitCode: 0}
	data, err = json.Marshal(resp)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(data), `"exit_code":0`) {
		t.Errorf("expected snake_case exit_code field in JSON, got %s", data)
	}
}

// TestKeygenCreatesLoadableKey exercises the Phase 3 keygen request end to
// end without any BLE involvement: a fresh key is generated and saved, its
// public half is returned as PEM on Stdout, and the saved private key loads
// back through protocol.LoadPrivateKey (the same call the connection path
// uses).
func TestKeygenCreatesLoadableKey(t *testing.T) {
	dir := t.TempDir()
	keyFile := filepath.Join(dir, "private_key.pem")
	s := &session{keyFile: keyFile}

	resp := s.dispatch(request{ID: "k1", Cmd: "keygen"})
	if !resp.OK {
		t.Fatalf("keygen failed: %s", resp.Stderr)
	}
	if resp.ExitCode != 0 {
		t.Errorf("exit_code = %d, want 0", resp.ExitCode)
	}
	if resp.ID != "k1" {
		t.Errorf("response ID = %q, want %q", resp.ID, "k1")
	}
	pubPEM := strings.TrimSpace(resp.Stdout)
	if !strings.Contains(pubPEM, "BEGIN PUBLIC KEY") {
		t.Errorf("stdout is not a PEM public key:\n%s", resp.Stdout)
	}

	// The public key written must round-trip through LoadPublicKey.
	pubPath := filepath.Join(dir, "public_key.pem")
	if err := os.WriteFile(pubPath, []byte(pubPEM+"\n"), 0644); err != nil {
		t.Fatal(err)
	}
	if _, err := protocol.LoadPublicKey(pubPath); err != nil {
		t.Errorf("generated public key does not parse: %v", err)
	}

	// The private key must load back for the connection path.
	if _, err := protocol.LoadPrivateKey(keyFile); err != nil {
		t.Errorf("generated private key does not load: %v", err)
	}
	// And be private (0600, matching tesla-keygen's file keyring write).
	info, err := os.Stat(keyFile)
	if err != nil {
		t.Fatal(err)
	}
	if perm := info.Mode().Perm(); perm != 0600 {
		t.Errorf("private key permissions = %o, want 600", perm)
	}
}

// TestKeygenWithoutForceReusesExistingKey verifies the -f/force semantics of
// tesla-keygen create: with no force, an existing key is not overwritten,
// just re-printed; with -f it is regenerated (so the public key changes).
func TestKeygenForceSemantics(t *testing.T) {
	dir := t.TempDir()
	keyFile := filepath.Join(dir, "private_key.pem")
	s := &session{keyFile: keyFile}

	first := s.dispatch(request{ID: "k1", Cmd: "keygen"})
	if !first.OK {
		t.Fatalf("first keygen failed: %s", first.Stderr)
	}

	second := s.dispatch(request{ID: "k2", Cmd: "keygen"})
	if !second.OK {
		t.Fatalf("second keygen failed: %s", second.Stderr)
	}
	if second.Stdout != first.Stdout {
		t.Errorf("non-forced keygen regenerated the key; public key changed:\n%q\nvs\n%q", first.Stdout, second.Stdout)
	}

	forced := s.dispatch(request{ID: "k3", Cmd: "keygen", Args: []string{"-f"}})
	if !forced.OK {
		t.Fatalf("forced keygen failed: %s", forced.Stderr)
	}
	if forced.Stdout == first.Stdout {
		t.Error("forced keygen (-f) returned the same public key - it must regenerate")
	}
}

// TestPresenceStepArrivalRequiresConsecutiveNearSamples verifies the
// hysteresis debounce: a single strong RSSI sample must not be enough to
// declare arrival, guarding against a false unlock-readiness trigger from
// one noisy reading.
func TestPresenceStepArrivalRequiresConsecutiveNearSamples(t *testing.T) {
	cfg := presenceConfig{nearRSSI: -70, nearConfirm: 3, farTimeout: 30 * time.Second, scanInterval: time.Second}
	now := time.Now()

	near, consecNear, _, action := presenceStep(cfg, false, 0, time.Time{}, now, true, -60, false)
	if near || action != presenceActionNone {
		t.Fatalf("1st near sample: near=%v action=%v, want near=false action=None", near, action)
	}
	near, consecNear, _, action = presenceStep(cfg, near, consecNear, time.Time{}, now, true, -60, false)
	if near || action != presenceActionNone {
		t.Fatalf("2nd near sample: near=%v action=%v, want near=false action=None", near, action)
	}
	near, _, _, action = presenceStep(cfg, near, consecNear, time.Time{}, now, true, -60, false)
	if !near || action != presenceActionArrive {
		t.Fatalf("3rd near sample: near=%v action=%v, want near=true action=Arrive", near, action)
	}
}

// TestPresenceStepArrivalResetsOnWeakSample ensures a single weak/absent
// reading between strong ones resets the debounce counter rather than
// letting intermittent detections accumulate toward arrival.
func TestPresenceStepArrivalResetsOnWeakSample(t *testing.T) {
	cfg := presenceConfig{nearRSSI: -70, nearConfirm: 2, farTimeout: 30 * time.Second, scanInterval: time.Second}
	now := time.Now()

	near, consecNear, _, _ := presenceStep(cfg, false, 0, time.Time{}, now, true, -60, false)
	near, consecNear, _, _ = presenceStep(cfg, near, consecNear, time.Time{}, now, false, 0, false) // weak sample resets
	near, _, _, action := presenceStep(cfg, near, consecNear, time.Time{}, now, true, -60, false)
	if near || action != presenceActionNone {
		t.Fatalf("after reset + 1 near sample: near=%v action=%v, want near=false action=None (needs 2 consecutive)", near, action)
	}
}

// TestPresenceStepStaysNearOnEverySeenSample checks the steady-state branch:
// once near, every tick that still sees the beacon must yield StayNear (so
// the caller keeps resetting the idle timer) rather than re-triggering
// Arrive or falling through to None.
func TestPresenceStepStaysNearOnEverySeenSample(t *testing.T) {
	cfg := presenceConfig{nearRSSI: -70, nearConfirm: 1, farTimeout: 30 * time.Second, scanInterval: time.Second}
	now := time.Now()

	near, consecNear, lastSeen, action := presenceStep(cfg, false, 0, time.Time{}, now, true, -60, false)
	if !near || action != presenceActionArrive {
		t.Fatalf("initial arrival: near=%v action=%v", near, action)
	}
	later := now.Add(5 * time.Second)
	near, _, lastSeen, action = presenceStep(cfg, near, consecNear, lastSeen, later, true, -55, false)
	if !near || action != presenceActionStayNear {
		t.Fatalf("still-near tick: near=%v action=%v, want near=true action=StayNear", near, action)
	}
	if !lastSeen.Equal(later) {
		t.Errorf("lastSeen = %v, want it advanced to %v", lastSeen, later)
	}
}

// TestPresenceStepDepartsAfterFarTimeout verifies departure only fires once
// the beacon has been unseen for longer than farTimeout, not on the first
// missed sample (walk-in-place / brief signal dropout tolerance).
func TestPresenceStepDepartsAfterFarTimeout(t *testing.T) {
	cfg := presenceConfig{nearRSSI: -70, nearConfirm: 1, farTimeout: 10 * time.Second, scanInterval: time.Second}
	now := time.Now()

	near, consecNear, lastSeen, action := presenceStep(cfg, false, 0, time.Time{}, now, true, -60, false)
	if !near || action != presenceActionArrive {
		t.Fatalf("initial arrival: near=%v action=%v", near, action)
	}

	// Beacon vanishes but not for long enough yet: keep trying to reconnect
	// rather than idling (Tesla often stops advertising after a GATT drop).
	soon := now.Add(5 * time.Second)
	near, consecNear, lastSeen, action = presenceStep(cfg, near, consecNear, lastSeen, soon, false, 0, false)
	if !near || action != presenceActionStayNear {
		t.Fatalf("brief dropout: near=%v action=%v, want near=true action=StayNear (still within farTimeout)", near, action)
	}

	// Now past farTimeout since lastSeen.
	later := now.Add(11 * time.Second)
	near, _, _, action = presenceStep(cfg, near, consecNear, lastSeen, later, false, 0, false)
	if near || action != presenceActionDepart {
		t.Fatalf("after farTimeout: near=%v action=%v, want near=false action=Depart", near, action)
	}
}

// TestPresenceStepLiveGATTHoldsNearWithoutAdvertisement is the regression
// for the "robust" rewrite: after connect Tesla stops advertising, BlueZ
// drops RSSI, and treating that as unseen departed a working phone key.
func TestPresenceStepLiveGATTHoldsNearWithoutAdvertisement(t *testing.T) {
	cfg := presenceConfig{nearRSSI: -90, nearConfirm: 1, farTimeout: 10 * time.Second, scanInterval: time.Second}
	now := time.Now()

	near, consecNear, lastSeen, action := presenceStep(cfg, false, 0, time.Time{}, now, true, -70, false)
	if !near || action != presenceActionArrive {
		t.Fatalf("arrival: near=%v action=%v", near, action)
	}

	later := now.Add(30 * time.Second)
	near, _, lastSeen, action = presenceStep(cfg, near, consecNear, lastSeen, later, false, 0, true)
	if !near || action != presenceActionStayNear {
		t.Fatalf("GATT up, no ad: near=%v action=%v, want StayNear", near, action)
	}
	if !lastSeen.Equal(later) {
		t.Errorf("lastSeen = %v, want it advanced to %v while GATT is up", lastSeen, later)
	}
}

// TestPresenceLiveNearRejectsWeakAndCachedBeacons is the regression for
// connecting at RSSI -97..-100 (and to leftover Device1 cache) after a GATT
// drop: those attempts hang bluetoothd with deadline exceeded / abort-by-local.
func TestPresenceLiveNearRejectsWeakAndCachedBeacons(t *testing.T) {
	if !presenceLiveNear(true, -90, -90) {
		t.Fatal("RSSI at the near threshold must be a connect signal")
	}
	if !presenceLiveNear(true, -70, -90) {
		t.Fatal("strong RSSI must be a connect signal")
	}
	if presenceLiveNear(true, -97, -90) {
		t.Fatal("weak live RSSI must not start GATT")
	}
	if presenceLiveNear(false, 0, -90) {
		t.Fatal("cached Device1 without RSSI must not start GATT")
	}
}

// TestPresenceStepExistingGATTCountsAsArrival covers a leftover authenticated
// session (manual command) so presence attaches the auth tap without waiting
// for a fresh advertisement.
func TestPresenceStepExistingGATTCountsAsArrival(t *testing.T) {
	cfg := presenceConfig{nearRSSI: -90, nearConfirm: 2, farTimeout: 10 * time.Second, scanInterval: time.Second}
	now := time.Now()

	near, _, _, action := presenceStep(cfg, false, 0, time.Time{}, now, false, 0, true)
	if !near || action != presenceActionArrive {
		t.Fatalf("existing GATT: near=%v action=%v, want Arrive", near, action)
	}
}

// TestPresenceDepartActionDoesNotImplyLock documents phase-1 policy: Depart
// is only a session-teardown signal. Explicit lock on walk-away was removed
// so the vehicle's Walk-Away Door Lock setting remains authoritative.
func TestPresenceDepartActionDoesNotImplyLock(t *testing.T) {
	if presenceActionDepart == presenceActionNone {
		t.Fatal("Depart must remain a distinct action for teardown-without-lock")
	}
	// Sanity: Arrive still exists and is distinct from Depart (arrival must
	// never be conflated with an unlock/lock actuation).
	if presenceActionArrive == presenceActionDepart {
		t.Fatal("Arrive and Depart must remain distinct")
	}
}

func TestVCSECPrimeDue(t *testing.T) {
	s := &session{}
	now := time.Now()
	if !s.vcsecPrimeDueLocked(now) {
		t.Fatal("never-primed session must be due")
	}
	s.lastVCSECPrime = now
	if s.vcsecPrimeDueLocked(now) {
		t.Fatal("just-primed session must not be due")
	}
	if s.vcsecPrimeDueLocked(now.Add(vcsecPrimeInterval - time.Second)) {
		t.Fatal("prime must not be due before the interval")
	}
	if !s.vcsecPrimeDueLocked(now.Add(vcsecPrimeInterval)) {
		t.Fatal("prime must be due at the interval")
	}
}

// TestParsePresenceArgsDefaults verifies presence-start with no args gets
// sane hysteresis defaults rather than zero values (a zero nearConfirm, for
// instance, would disable the arrival debounce entirely).
func TestParsePresenceArgsDefaults(t *testing.T) {
	cfg, err := parsePresenceArgs(nil)
	if err != nil {
		t.Fatalf("parsePresenceArgs(nil): %v", err)
	}
	want := defaultPresenceConfig()
	if cfg != want {
		t.Errorf("parsePresenceArgs(nil) = %+v, want defaults %+v", cfg, want)
	}
	if cfg.nearRSSI != -90 || cfg.nearConfirm != 1 {
		t.Errorf("defaults nearRSSI=%d nearConfirm=%d, want -90 / 1 (connect on first live beacon)", cfg.nearRSSI, cfg.nearConfirm)
	}
	if cfg.farTimeout != 15*time.Second {
		t.Errorf("default farTimeout = %v, want 15s", cfg.farTimeout)
	}
	if cfg.scanInterval != 2*time.Second {
		t.Errorf("default scanInterval = %v, want 2s", cfg.scanInterval)
	}
}

// TestParsePresenceArgsOverrides checks each flag actually reaches the
// returned config, not just that parsing doesn't error.
func TestParsePresenceArgsOverrides(t *testing.T) {
	cfg, err := parsePresenceArgs([]string{
		"-near-rssi", "-80",
		"-near-confirm", "5",
		"-away-timeout", "45s",
		"-scan-interval", "500ms",
	})
	if err != nil {
		t.Fatalf("parsePresenceArgs: %v", err)
	}
	want := presenceConfig{nearRSSI: -80, nearConfirm: 5, farTimeout: 45 * time.Second, scanInterval: 500 * time.Millisecond}
	if cfg != want {
		t.Errorf("parsePresenceArgs overrides = %+v, want %+v", cfg, want)
	}
}

// TestParsePresenceArgsRejectsUnknownFlag ensures a typo'd flag surfaces as
// an error rather than being silently ignored.
func TestParsePresenceArgsRejectsUnknownFlag(t *testing.T) {
	if _, err := parsePresenceArgs([]string{"-near-rssi-typo", "-80"}); err == nil {
		t.Fatal("expected an error for an unrecognized flag")
	}
}

// TestConnectBackoffDoubles verifies failed presence connects back off
// exponentially so a flaky link cannot hammer bluetoothd.
func TestConnectBackoffDoubles(t *testing.T) {
	s := &session{}
	now := time.Now()
	s.scheduleConnectBackoffLocked()
	first := s.connectBackoffUntil
	if !s.connectBackoffActive(now) {
		t.Fatal("expected backoff to be active immediately after scheduling")
	}
	s.scheduleConnectBackoffLocked()
	second := s.connectBackoffUntil
	if !second.After(first) {
		t.Errorf("second backoff %v should be after first %v", second, first)
	}
	s.clearConnectBackoffLocked()
	if s.connectBackoffActive(now) {
		t.Error("backoff should be cleared after successful connect")
	}
}

func TestConnectBackoffDoublesAfterExpiry(t *testing.T) {
	s := &session{}
	s.scheduleConnectBackoffLocked()
	first := s.connectBackoff
	if first != time.Second {
		t.Fatalf("first backoff = %v, want 1s", first)
	}
	s.connectBackoffUntil = time.Now().Add(-time.Millisecond)
	s.scheduleConnectBackoffLocked()
	if s.connectBackoff != first*2 {
		t.Fatalf("backoff after expiry = %v, want %v (must not reset to 1s)", s.connectBackoff, first*2)
	}
}

// TestEnqueueAuthDatagramKeepsLatest verifies the auth inbox drops the oldest
// item when full so a handle-pull AuthenticationRequest is not lost.
func TestEnqueueAuthDatagramKeepsLatest(t *testing.T) {
	inbox := make(chan []byte, 2)
	enqueueAuthDatagram(inbox, []byte("first"))
	enqueueAuthDatagram(inbox, []byte("second"))
	enqueueAuthDatagram(inbox, []byte("third"))
	got1 := <-inbox
	got2 := <-inbox
	if string(got1) != "second" || string(got2) != "third" {
		t.Errorf("drop-oldest queue = %q, %q; want second, third", got1, got2)
	}
}

// TestDispatchPresenceStartRequiresBluezBackend guards the reason presence
// mode is gated to the bluez backend: raw-HCI's InitAdapterWithID takes
// exclusive control of the controller, which continuous discovery for
// proximity polling can't share.
func TestDispatchPresenceStartRequiresBluezBackend(t *testing.T) {
	s := &session{bleBackend: "hci"}
	resp := s.dispatch(request{ID: "p1", Cmd: "presence-start"})
	if resp.OK {
		t.Fatal("expected presence-start to fail on the hci backend")
	}
	if !strings.Contains(resp.Stderr, "bluez") {
		t.Errorf("stderr = %q, want it to mention the bluez backend requirement", resp.Stderr)
	}
	if s.presenceCancel != nil {
		t.Error("presenceCancel should remain nil when presence-start is rejected")
	}
}

// TestDispatchPresenceStartRejectsBadArgs ensures a malformed flag is
// reported as a request error rather than silently falling back to
// defaults.
func TestDispatchPresenceStartRejectsBadArgs(t *testing.T) {
	s := &session{bleBackend: "bluez"}
	resp := s.dispatch(request{ID: "p1", Cmd: "presence-start", Args: []string{"-not-a-flag"}})
	if resp.OK {
		t.Fatal("expected presence-start to reject an unrecognized flag")
	}
}

// TestDispatchPresenceStopWithoutStartIsSafe ensures presence-stop is a
// harmless no-op when presence mode was never started (e.g. the Rust core
// stops it unconditionally on shutdown regardless of whether it was ever
// running).
func TestDispatchPresenceStopWithoutStartIsSafe(t *testing.T) {
	s := &session{}
	resp := s.dispatch(request{ID: "p1", Cmd: "presence-stop"})
	if !resp.OK {
		t.Fatalf("expected presence-stop with nothing running to succeed, got: %s", resp.Stderr)
	}
	if s.presenceGeneration == 0 {
		t.Fatal("presence-stop must invalidate any exiting loop generation")
	}
}

// TestKeygenReportsMalformedKeyWithoutForce ensures a corrupt existing key
// doesn't wedge keygen: without -f it regenerates rather than erroring (and
// without panicking), mirroring upstream's create fall-through.
func TestKeygenRecoversFromCorruptKey(t *testing.T) {
	dir := t.TempDir()
	keyFile := filepath.Join(dir, "private_key.pem")
	if err := os.WriteFile(keyFile, []byte("garbage\n"), 0644); err != nil {
		t.Fatal(err)
	}
	s := &session{keyFile: keyFile}

	resp := s.dispatch(request{ID: "k1", Cmd: "keygen"})
	if !resp.OK {
		t.Fatalf("keygen with corrupt existing key failed: %s", resp.Stderr)
	}
	if _, err := protocol.LoadPrivateKey(keyFile); err != nil {
		t.Errorf("regenerated key does not load: %v", err)
	}
}
