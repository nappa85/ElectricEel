// tesla-session is a persistent companion to tesla-control: instead of
// connecting, running one command, and exiting (paying a full BLE
// connect+StartSession handshake every time), it holds a live
// *vehicle.Vehicle across many commands and only reconnects after a period
// of inactivity. It's spoken to over stdin/stdout by the in-process Rust
// control core, which falls back to spawning tesla-control directly
// (today's behavior) if this process is unreachable or misbehaves - see
// helper/src/session_client.rs.
//
// Every command still goes through commands_vendor.go's execute() and
// commands map, unmodified - this file only changes when the BLE
// connect/handshake happens, not what any individual command does.
package main

import (
	"bufio"
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/x509"
	"electric-eel-session/bluez"
	"encoding/json"
	"encoding/pem"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"os/signal"
	"sync"
	"syscall"
	"time"

	"github.com/teslamotors/vehicle-command/pkg/connector"
	"github.com/teslamotors/vehicle-command/pkg/connector/ble"
	"github.com/teslamotors/vehicle-command/pkg/protocol"
	"github.com/teslamotors/vehicle-command/pkg/vehicle"
)

// writeErr is referenced by a couple of commands_vendor.go's handlers
// (list-keys, add-key-request) - identical to upstream cmd/tesla-control/
// main.go's own definition, needed here because main.go itself wasn't
// vendored (only commands.go was; see commands_vendor.go's header).
func writeErr(format string, a ...interface{}) {
	fmt.Fprintf(os.Stderr, format, a...)
	fmt.Fprintf(os.Stderr, "\n")
}

type request struct {
	ID   string   `json:"id"`
	Cmd  string   `json:"cmd"`
	Args []string `json:"args"`
}

type response struct {
	ID       string `json:"id"`
	OK       bool   `json:"ok"`
	Stdout   string `json:"stdout"`
	Stderr   string `json:"stderr"`
	ExitCode int    `json:"exit_code"`
}

// session owns the (possibly absent) live vehicle connection. All access is
// serialized by mu - the caller (the in-process Rust core, via its own
// ble_sem) never issues concurrent commands, and this process only ever has
// one goroutine
// (the stdin loop) calling dispatch, but idleTimer's callback runs on its
// own goroutine and must take mu too.
type session struct {
	mu sync.Mutex

	vin            string
	keyFile        string
	adapterID      string
	bleBackend     string
	connectTimeout time.Duration
	commandTimeout time.Duration
	idleTimeout    time.Duration

	car   *vehicle.Vehicle
	conn  connector.Connector
	bluez *bluez.Conn

	idleTimer *time.Timer

	// presenceCancel is non-nil while the presence-maintenance loop (see
	// presenceLoop) is running; presence-start/presence-stop set/clear it.
	presenceCancel     context.CancelFunc
	presenceGeneration uint64

	// authCancel / authInbox drive the passive-entry AuthenticationRequest
	// responder while presence mode holds a live BlueZ session. Cleared by
	// stopAuthTapLocked (via teardownLocked / presence stop).
	authCancel context.CancelFunc
	authInbox  chan []byte

	// lastBeacon is the most recent Watcher.Peek() result while presence mode
	// runs. Manual commands reuse it as a connect target so they never start
	// a colliding one-shot scan while the phone-key Watcher already holds
	// discovery open. Cleared by stopPresenceLocked.
	lastBeacon *bluez.ScanResult

	// connectBackoffUntil gates presence-mode reconnect attempts after a
	// failed connect, so a flaky link can't hammer bluetoothd in a tight loop.
	connectBackoffUntil time.Time
	connectBackoff      time.Duration

	// lastVCSECPrime is the last successful BodyControllerState round-trip
	// while presence holds a session. StartSession alone is not enough for
	// handle-pull: the vehicle only treats the key as present after a VCSEC
	// GET_STATUS, which is what a dashboard status query happened to send.
	lastVCSECPrime time.Time

	// writeMu serializes stdout writes: dispatch's replies and the presence
	// loop's unsolicited events both encode onto enc from different
	// goroutines, and json.Encoder is not safe for concurrent use.
	writeMu sync.Mutex
	enc     *json.Encoder
}

// teardownLocked closes the live BLE session, if any, so the vehicle (and
// the phone's BLE radio) can go back to sleep. Safe to call when already
// torn down. Caller holds mu.
func (s *session) teardownLocked() {
	if s.idleTimer != nil {
		s.idleTimer.Stop()
		s.idleTimer = nil
	}
	s.stopAuthTapLocked()
	if s.car != nil || s.conn != nil {
		keylog("link", "teardown session")
	}
	if s.car != nil {
		s.car.Disconnect()
		s.car = nil
	}
	if s.conn != nil {
		s.conn.Close()
		s.conn = nil
	}
	s.lastVCSECPrime = time.Time{}
}

// closeBluezLocked releases the system-bus connection held for the bluez
// backend (it registers a D-Bus signal channel, so it must be closed exactly
// once for the life of the process, not per connection). Caller holds mu.
func (s *session) closeBluezLocked() {
	if s.bluez != nil {
		_ = s.bluez.Close()
		s.bluez = nil
	}
}

// commandsWithoutSession lists commands that must connect *without* calling
// StartSession at all. add-key-request is the one command that must work
// before this key is paired with the vehicle, and per the vehicle-command
// source (internal/dispatcher/dispatcher.go's tryStartSession), StartSession
// works by requesting SessionInfo and failing on
// SESSION_INFO_STATUS_KEY_NOT_ON_WHITELIST - which an unrecognized key gets
// for *every* domain once the vehicle already has other keys enrolled, VCSEC
// included. (An earlier fix here tried scoping the request to VCSEC only,
// on the theory VCSEC alone would tolerate an unpaired key; confirmed live
// against a real vehicle that it does not - same rejection.) The reason
// add-key-request works at all is that its handler (SendAddKeyRequestWithRole)
// never goes through the session/dispatcher machinery in the first place -
// it sends a self-identifying, unauthenticated message directly via the raw
// connector. So the fix is to skip StartSession for it entirely, not to
// narrow its domain.
var commandsWithoutSession = map[string]bool{
	"add-key-request": true,
}

// addKeyRequestGracePeriod is how long dispatch holds the BLE connection
// open after a successful add-key-request before disconnecting - see the
// call site in dispatch.
const addKeyRequestGracePeriod = 90 * time.Second

// sessionDomains picks which domains StartSession should handshake with for
// cmd, mirroring upstream's configureFlags (vendored into commands_vendor.go
// but, unlike here, never actually called - see ensureConnectedLocked).
// Requesting every domain (nil) is right for ordinary commands; only
// commands_vendor.go's own domain field (used by body-controller-state)
// narrows it - commandsWithoutSession commands never reach this at all.
//
// Presence mode (cmd == "") requests VCSEC only: that is the body controller
// that handles passive entry, and it stays awake while the car sleeps.
// Requesting infotainment domains against an asleep vehicle is why phone-key
// used to work "only after an NFC unlock" - NFC wakes the whole car.
func sessionDomains(cmd string) []protocol.Domain {
	if cmd == "" {
		return []protocol.Domain{protocol.DomainVCSEC}
	}
	if cmd == "state" {
		return []protocol.Domain{protocol.DomainInfotainment}
	}
	if info, ok := commands[cmd]; ok && info.domain != protocol.DomainNone {
		return []protocol.Domain{info.domain}
	}
	return nil
}

// ensureConnectedLocked reuses the live session if there is one; otherwise
// performs the same connect+StartSession sequence
// cli.Config.Connect/ConnectLocal does in upstream tesla-control - except
// for commandsWithoutSession commands, which connect but skip StartSession
// (see that map's doc). cmd selects the StartSession domains via
// sessionDomains; pass "" for presence mode (VCSEC only).
//
// A connection established for a commandsWithoutSession command is
// deliberately never cached into s.car/s.conn (dispatch tears it down right
// after the command runs, win or lose) - caching it would let a later,
// ordinary command silently reuse a connection that never authenticated,
// instead of the failure that command needs to see.
//
// target, if non-nil, is passed straight through to bluez.Conn.Connect
// instead of letting it scan for the vehicle itself. presenceLoop passes
// its own Watcher's last Peek() result here: without it, an arrival-
// triggered connect would call bluez's own scan()/startDiscovery() while
// presenceLoop's Watcher already has discovery open for RSSI polling,
// colliding with itself - confirmed live ("bluez: start discovery:
// Operation already in progress", repeatedly, since each failed attempt
// reset presenceStep back to "away" and immediately re-triggered arrival).
// Every other caller passes nil (nothing to reuse). Caller holds mu.
func (s *session) ensureConnectedLocked(ctx context.Context, cmd string, target *bluez.ScanResult) error {
	if s.car != nil {
		if !commandsWithoutSession[cmd] {
			s.ensureAuthTapLocked(context.Background())
			domains := sessionDomains(cmd)
			// nil means "all domains" (lock/unlock/etc.). Those can use the
			// live VCSEC session as-is; re-handshaking infotainment here
			// would fail against a sleeping car and block RKE. A command
			// that names a domain (state → infotainment, body-controller-
			// state → VCSEC) still needs that handshake if presence only
			// started VCSEC.
			if len(domains) > 0 {
				keylog("connect", "StartSession additional domains=%v", domains)
				if err := s.car.StartSession(ctx, domains); err != nil {
					keylog("connect", "StartSession additional failed: %v", err)
					return err
				}
			}
		}
		return nil
	}

	skey, err := protocol.LoadPrivateKey(s.keyFile)
	if err != nil {
		return fmt.Errorf("load private key: %w", err)
	}

	connCtx, cancel := context.WithTimeout(ctx, s.connectTimeout)
	defer cancel()

	keylog("connect", "begin cmd=%q target=%v timeout=%s", cmd, target != nil, s.connectTimeout)

	var conn connector.Connector
	if s.bleBackend == "bluez" {
		// org.bluez D-Bus transport: the radio stays owned by the OS
		// Bluetooth stack, so other connections (e.g. a soundbar) survive.
		if err := s.ensureBluezLocked(); err != nil {
			return err
		}
		conn, err = s.bluez.Connect(connCtx, s.adapterID, s.vin, target)
		if err != nil {
			keylog("connect", "GATT failed: %v", err)
			return err
		}
	} else {
		// Default: upstream go-ble raw HCI. The only path that calls
		// InitAdapterWithID - which brings the controller down and binds an
		// exclusive HCI user channel (see KNOWN_ISSUES.md). This is exactly
		// the behavior the bluez backend exists to avoid.
		if err := ble.InitAdapterWithID(s.adapterID); err != nil {
			return err
		}
		conn, err = ble.NewConnection(connCtx, s.vin)
		if err != nil {
			return err
		}
	}
	car, err := vehicle.NewVehicle(conn, skey, nil)
	if err != nil {
		conn.Close()
		return err
	}
	if err := car.Connect(connCtx); err != nil {
		conn.Close()
		keylog("connect", "Vehicle.Connect failed: %v", err)
		return err
	}

	// Cache the live link and attach the auth observer before StartSession.
	// The vehicle can send AuthenticationRequest during the handshake; if
	// the observer is only attached afterwards, that first handle-pull is
	// dropped and unlock appears to need a dashboard refresh.
	s.car = car
	s.conn = conn
	if !commandsWithoutSession[cmd] {
		s.ensureAuthTapLocked(context.Background())
		domains := sessionDomains(cmd)
		keylog("connect", "StartSession domains=%v", domains)
		if err := car.StartSession(connCtx, domains); err != nil {
			s.teardownLocked()
			keylog("connect", "StartSession failed: %v", err)
			return err
		}
	}
	if s.bleBackend == "bluez" {
		s.enableTrustedLocked()
	}
	keylog("connect", "ready cmd=%q", cmd)
	return nil
}

// enableTrustedLocked marks the vehicle Trusted so reconnects do not prompt.
// AutoConnect is explicitly cleared: bluetoothd would otherwise reattach the
// GATT link on its own, without StartSession or the auth responder, and
// presenceLoop's Connect() then races that leftover link. A previous build
// set AutoConnect=true, so this must write false to undo a sticky Device1
// property. Caller holds mu.
func (s *session) enableTrustedLocked() {
	bzConn, ok := s.conn.(*bluez.Connection)
	if !ok || bzConn == nil {
		return
	}
	if err := bzConn.SetTrusted(true); err != nil {
		keylog("link", "SetTrusted failed: %v", err)
	}
	if err := bzConn.SetAutoConnect(false); err != nil {
		keylog("link", "SetAutoConnect(false) failed: %v", err)
	}
}

// ensureBluezLocked opens the system-bus connection used for the bluez
// backend, if not already open. It's shared by ensureConnectedLocked (one
// GATT connect) and presenceLoop (discovery only, no GATT connect) - both
// need the same lazily-opened, process-lifetime bus handle. Caller holds mu.
func (s *session) ensureBluezLocked() error {
	if s.bluez != nil {
		return nil
	}
	bz, err := bluez.Open()
	if err != nil {
		return fmt.Errorf("open bluez connection: %w", err)
	}
	s.bluez = bz
	return nil
}

// resetIdleTimerLocked (re)starts the countdown to teardownLocked. Called
// after every command, successful or not - a command that fails still
// proves the link is alive right now, and a wedged link will surface on
// the *next* attempt regardless of whether idle teardown ran in between.
//
// While presence mode is active the idle timer is suppressed entirely:
// presenceLoop already decides when to hold or release the session based on
// proximity, and an idle teardown mid-visit would drop passive unlock/drive
// capability even though the phone is still near the car.
// Caller holds mu.
func (s *session) resetIdleTimerLocked() {
	if s.presenceCancel != nil {
		if s.idleTimer != nil {
			s.idleTimer.Stop()
			s.idleTimer = nil
		}
		return
	}
	if s.idleTimer != nil {
		s.idleTimer.Stop()
	}
	s.idleTimer = time.AfterFunc(s.idleTimeout, func() {
		s.mu.Lock()
		defer s.mu.Unlock()
		keylog("session", "idle timeout - tearing down BLE session")
		s.teardownLocked()
	})
}

// presenceBeaconTargetLocked returns the last Watcher snapshot for connect
// when phone-key scanning is active, so callers never start a second
// discovery session. Caller holds mu.
func (s *session) presenceBeaconTargetLocked() *bluez.ScanResult {
	if s.presenceCancel == nil {
		return nil
	}
	return s.lastBeacon
}

// connectTargetLocked prefers this tick's Peek result, then the last live
// beacon. Never returns nil just to force scan(): scan() stops discovery on
// the way out and would kill presenceLoop's Watcher. Caller holds mu.
func (s *session) connectTargetLocked(peek *bluez.ScanResult) *bluez.ScanResult {
	if peek != nil {
		return peek
	}
	return s.lastBeacon
}

// scheduleConnectBackoffLocked sets a reconnect quiet period after a failed
// presence connect. Backoff doubles on repeated failures up to one minute.
// Caller holds mu.
func (s *session) scheduleConnectBackoffLocked() {
	const maxBackoff = time.Minute
	backoff := s.connectBackoff * 2
	if backoff < time.Second {
		backoff = time.Second
	}
	if backoff > maxBackoff {
		backoff = maxBackoff
	}
	s.connectBackoff = backoff
	s.connectBackoffUntil = time.Now().Add(backoff)
	keylog("connect", "backoff until %s", s.connectBackoffUntil.Format("15:04:05"))
}

func (s *session) clearConnectBackoffLocked() {
	s.connectBackoffUntil = time.Time{}
	s.connectBackoff = 0
}

func (s *session) connectBackoffActive(now time.Time) bool {
	return !s.connectBackoffUntil.IsZero() && now.Before(s.connectBackoffUntil)
}

// vcsecPrimeInterval is how often presence re-sends GET_STATUS while the
// GATT link is up, so the vehicle does not forget this key is nearby.
const vcsecPrimeInterval = 15 * time.Second

// primeVCSECLocked sends VCSEC GET_STATUS (body-controller-state) on the
// live session. Handle-pull unlock was only working after a dashboard
// status query because that query performed this same round-trip; presence
// connect used to stop at StartSession. Caller holds mu; s.car must be set.
func (s *session) primeVCSECLocked(ctx context.Context) {
	if s.car == nil {
		return
	}
	_, err := s.car.BodyControllerState(ctx)
	if err != nil {
		keylog("auth", "VCSEC prime failed: %v", err)
		return
	}
	s.lastVCSECPrime = time.Now()
	keylog("auth", "VCSEC primed")
}

func (s *session) vcsecPrimeDueLocked(now time.Time) bool {
	return s.lastVCSECPrime.IsZero() || now.Sub(s.lastVCSECPrime) >= vcsecPrimeInterval
}

// enqueueAuthDatagram delivers an observed inbound datagram to the auth
// responder, dropping the oldest queued item when full so a burst of VCSEC
// AuthenticationRequest messages cannot be lost silently (which would make
// passive unlock appear "randomly broken").
func enqueueAuthDatagram(inbox chan []byte, datagram []byte) {
	select {
	case inbox <- datagram:
	default:
		select {
		case <-inbox:
		default:
		}
		select {
		case inbox <- datagram:
		default:
		}
	}
}

// keygen generates (or, when an existing key exists and force is unset,
// re-prints the existing key's) P256 private/public key pair and saves the
// private key to s.keyFile - the same work upstream tesla-keygen's `create`
// subcommand does, so the Rust helper no longer needs to exec a privileged
// binary for key management. The PEM-encoded public key is returned in the
// response's Stdout, matching what tesla-keygen writes to stdout.
//
// No BLE connection is involved; this runs even when the session has never
// connected (and does not need to).
func (s *session) dispatchKeygen(req request) response {
	force := false
	for _, a := range req.Args {
		if a == "-f" {
			force = true
		}
	}

	if !force {
		if skey, err := protocol.LoadPrivateKey(s.keyFile); err == nil {
			if pub, ok := publicKeyPEM(skey); ok {
				return response{ID: req.ID, OK: true, Stdout: pub, ExitCode: 0}
			}
		}
	}

	newKey, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return response{ID: req.ID, OK: false, Stderr: fmt.Sprintf("Failed to generate private key: %s\n", err), ExitCode: 1}
	}
	scalar := make([]byte, 32)
	newKey.D.FillBytes(scalar)
	skey := protocol.UnmarshalECDHPrivateKey(scalar)
	if skey == nil {
		return response{ID: req.ID, OK: false, Stderr: "failed to build private key from generated scalar\n", ExitCode: 1}
	}
	if err := protocol.SavePrivateKey(skey, s.keyFile); err != nil {
		return response{ID: req.ID, OK: false, Stderr: fmt.Sprintf("Failed to save private key: %s\n", err), ExitCode: 1}
	}
	pub, ok := publicKeyPEM(skey)
	if !ok {
		return response{ID: req.ID, OK: false, Stderr: "Failed to parse key. The keyring may be corrupted. Run with -f to generate new key.\n", ExitCode: 1}
	}
	return response{ID: req.ID, OK: true, Stdout: pub, ExitCode: 0}
}

// publicKeyPEM encodes skey's public half as a PEM "PUBLIC KEY" block,
// mirroring tesla-keygen's printPublicKey.
func publicKeyPEM(skey protocol.ECDHPrivateKey) (string, bool) {
	pkey := ecdsa.PublicKey{Curve: elliptic.P256()}
	pkey.X, pkey.Y = elliptic.Unmarshal(elliptic.P256(), skey.PublicBytes())
	if pkey.X == nil {
		return "", false
	}
	derPublicKey, err := x509.MarshalPKIXPublicKey(&pkey)
	if err != nil {
		return "", false
	}
	return string(pem.EncodeToMemory(&pem.Block{Type: "PUBLIC KEY", Bytes: derPublicKey})), true
}

// presenceConfig tunes the presence-maintenance loop's proximity hysteresis.
type presenceConfig struct {
	nearRSSI     int16         // beacon must be at/above this to count as nearby
	nearConfirm  int           // consecutive near samples required before treating the vehicle as present (debounce false triggers on a single strong sample)
	farTimeout   time.Duration // how long the beacon may go unseen/weak before departure is declared
	scanInterval time.Duration
}

func defaultPresenceConfig() presenceConfig {
	return presenceConfig{
		// Connect as soon as we hear a live advertisement. Waiting for a
		// strong -70 meant the StartSession handshake often finished only
		// after the handle-pull timeout when walking up to a sleeping car.
		nearRSSI:     -90,
		nearConfirm:  1,
		farTimeout:   15 * time.Second,
		scanInterval: 2 * time.Second,
	}
}

// parsePresenceArgs parses presence-start's optional flags out of the
// request's Args. Never a real CLI (this is driven over stdin by the Rust
// core), but flag.FlagSet gives free -name value/-name=value parsing and
// error text for free.
func parsePresenceArgs(args []string) (presenceConfig, error) {
	cfg := defaultPresenceConfig()
	fs := flag.NewFlagSet("presence-start", flag.ContinueOnError)
	fs.SetOutput(io.Discard)
	nearRSSI := int(cfg.nearRSSI)
	fs.IntVar(&nearRSSI, "near-rssi", nearRSSI, "RSSI (dBm) at/above which the vehicle counts as nearby")
	fs.IntVar(&cfg.nearConfirm, "near-confirm", cfg.nearConfirm, "consecutive near samples required before treating the vehicle as present")
	fs.DurationVar(&cfg.farTimeout, "away-timeout", cfg.farTimeout, "how long the beacon may go unseen/weak before departure is declared")
	fs.DurationVar(&cfg.scanInterval, "scan-interval", cfg.scanInterval, "interval between beacon snapshots")
	if err := fs.Parse(args); err != nil {
		return presenceConfig{}, err
	}
	cfg.nearRSSI = int16(nearRSSI)
	return cfg, nil
}

// presenceAction is presenceStep's verdict for the current tick.
type presenceAction int

const (
	presenceActionNone presenceAction = iota
	presenceActionArrive
	presenceActionStayNear
	presenceActionDepart
)

// presenceStep is the pure decision core of the presence loop: given the
// current near/away state and this tick's beacon snapshot, it decides what
// to do and returns the updated state. Kept separate from presenceLoop's I/O
// (BLE scanning, connecting, locking) so the hysteresis logic is
// unit-testable without a real adapter.
//
// Deliberately does not decide to unlock or lock: the vehicle's own
// passive-entry check (handle pull) authorizes unlock using the live
// session and any AuthenticationResponse this process replies with, and
// walk-away lock remains the vehicle's own setting. This state machine's
// job is only to keep an authenticated session up while nearby and release
// it on departure.
func presenceStep(cfg presenceConfig, near bool, consecNear int, lastSeen, now time.Time, seen bool, rssi int16, gattUp bool) (nextNear bool, nextConsecNear int, nextLastSeen time.Time, action presenceAction) {
	if seen && rssi >= cfg.nearRSSI {
		lastSeen = now
		consecNear++
	} else if gattUp {
		// Tesla stops advertising once GATT is up. Missing RSSI is then
		// normal and must not age lastSeen toward departure - that is what
		// made the "robust" rewrite tear down a working phone key ~15s
		// after connect.
		lastSeen = now
		consecNear = 0
	} else {
		consecNear = 0
	}

	switch {
	case !near && consecNear >= cfg.nearConfirm:
		return true, 0, lastSeen, presenceActionArrive
	case !near && gattUp:
		// A command (or leftover session) already has the link; attach the
		// auth responder instead of waiting for another advertisement.
		return true, 0, lastSeen, presenceActionArrive
	case near && now.Sub(lastSeen) > cfg.farTimeout:
		return false, 0, lastSeen, presenceActionDepart
	case near:
		// Still near: live beacon, live GATT, or a brief dropout inside
		// farTimeout. StayNear reconnects if the session is gone.
		return true, consecNear, lastSeen, presenceActionStayNear
	default:
		return near, consecNear, lastSeen, presenceActionNone
	}
}

// presenceLiveNear reports a fresh advertisement strong enough to start
// (or resume) a GATT session. Weak RSSI and cached Device1 entries without
// RSSI must not connect: BlueZ keeps Device1 objects forever, and connecting
// at -97 dBm wedges bluetoothd (deadline exceeded / le-connection-abort-by-local).
func presenceLiveNear(live bool, rssi, nearRSSI int16) bool {
	return live && rssi >= nearRSSI
}

// event is an unsolicited JSON line the presence loop pushes to the Rust
// core outside the request/response cycle - distinguished from response by
// its "kind" field (response never sets one) so the reader can tell the two
// apart on the same stdout stream. Time is RFC 3339 with sub-second
// precision - added after a live test where a plain log of bare kinds left
// no way to tell whether a handle-pull happened before or after the
// preceding presence_near, which mattered a great deal to what the result
// could be taken to mean.
type event struct {
	Kind  string    `json:"kind"`
	VIN   string    `json:"vin"`
	Time  time.Time `json:"time"`
	Error string    `json:"error,omitempty"`
}

func (s *session) emitEvent(kind string, err error) {
	e := event{Kind: kind, VIN: s.vin, Time: time.Now()}
	if err != nil {
		e.Error = err.Error()
	}
	keylog("presence", "event %s err=%v", kind, err)
	s.writeMu.Lock()
	defer s.writeMu.Unlock()
	if s.enc != nil {
		_ = s.enc.Encode(e)
	}
}

// emitPresenceDisconnectedLocked notifies the UI that the live GATT session
// dropped while presence mode remains active. Caller holds mu.
func (s *session) emitPresenceDisconnectedLocked(err error) {
	if s.presenceCancel == nil {
		return
	}
	s.emitEvent("presence_disconnected", err)
}

// writeResponse serializes resp to stdout, synchronized with emitEvent so
// the two goroutines that write to the process's single JSON-lines stdout
// (the request loop and the presence loop) never interleave partial writes.
func (s *session) writeResponse(resp response) {
	s.writeMu.Lock()
	defer s.writeMu.Unlock()
	if s.enc != nil {
		_ = s.enc.Encode(resp)
	}
}

// dispatchPresenceStart begins (or no-ops if already running) the presence
// loop. Requires the bluez backend: the raw-HCI backend's InitAdapterWithID
// takes exclusive control of the controller for GATT connects, which is
// incompatible with also running continuous org.bluez discovery for
// proximity polling (see KNOWN_ISSUES.md).
func (s *session) dispatchPresenceStart(req request) response {
	cfg, err := parsePresenceArgs(req.Args)
	if err != nil {
		return response{ID: req.ID, OK: false, Stderr: err.Error() + "\n", ExitCode: 2}
	}
	if s.bleBackend != "bluez" {
		return response{ID: req.ID, OK: false, Stderr: "presence mode requires -ble-backend=bluez\n", ExitCode: 1}
	}

	s.mu.Lock()
	defer s.mu.Unlock()
	if s.presenceCancel != nil {
		keylog("session", "presence-start: already running")
		return response{ID: req.ID, OK: true, Stdout: "presence mode already running\n", ExitCode: 0}
	}
	ctx, cancel := context.WithCancel(context.Background())
	s.presenceGeneration++
	generation := s.presenceGeneration
	s.presenceCancel = cancel
	keylog("session", "presence-start nearRSSI=%d nearConfirm=%d away=%s scan=%s",
		cfg.nearRSSI, cfg.nearConfirm, cfg.farTimeout, cfg.scanInterval)
	go s.presenceLoop(ctx, cfg, generation)
	return response{ID: req.ID, OK: true, Stdout: "presence mode started\n", ExitCode: 0}
}

func (s *session) dispatchPresenceStop(req request) response {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.stopPresenceLocked()
	s.teardownLocked()
	return response{ID: req.ID, OK: true, Stdout: "presence mode stopped\n", ExitCode: 0}
}

// stopPresenceLocked cancels the presence loop, if running. Safe to call
// when it isn't. Also stops the AuthenticationRequest responder so a later
// presence-start does not reuse a cancelled auth inbox. Caller holds mu.
func (s *session) stopPresenceLocked() {
	if s.presenceCancel != nil {
		s.presenceCancel()
		s.presenceCancel = nil
	}
	// Invalidates the cancelled loop's deferred cleanup so it cannot clear a
	// newer presence-start that races ahead before the old goroutine exits.
	s.presenceGeneration++
	s.stopAuthTapLocked()
	s.lastBeacon = nil
	s.clearConnectBackoffLocked()
	keylog("session", "presence-stop")
}

// ensureAuthTapLocked attaches a non-destructive BlueZ inbound observer and
// starts the AuthenticationRequest responder if needed. Caller holds mu;
// s.conn must already be a live *bluez.Connection (presence mode is bluez
// only). parent is the presence-loop context so the responder stops when
// presence stops even if teardown hasn't run yet.
func (s *session) ensureAuthTapLocked(parent context.Context) {
	bz, ok := s.conn.(*bluez.Connection)
	if !ok || bz == nil {
		return
	}
	if s.authCancel == nil {
		inbox := make(chan []byte, 64)
		s.authInbox = inbox
		authCtx, cancel := context.WithCancel(parent)
		s.authCancel = cancel
		go s.authResponderLoop(authCtx, inbox)
		keylog("auth", "responder started")
	}
	inbox := s.authInbox
	bz.SetInboundObserver(func(datagram []byte) {
		enqueueAuthDatagram(inbox, datagram)
	})
}

// stopAuthTapLocked clears the BlueZ inbound observer and stops the
// AuthenticationRequest responder. Safe when neither is active. Caller holds mu.
func (s *session) stopAuthTapLocked() {
	if bz, ok := s.conn.(*bluez.Connection); ok && bz != nil {
		bz.SetInboundObserver(nil)
	}
	if s.authCancel != nil {
		s.authCancel()
		s.authCancel = nil
	}
	s.authInbox = nil
}

// authResponderLoop consumes observed inbound datagrams, detects VCSEC
// AuthenticationRequest messages, and replies with a matching
// AuthenticationResponse over the authenticated session. Holds session.mu
// across vehicle.Send, matching dispatch(): teardown must not concurrently
// disconnect the Vehicle while its dispatcher is signing/sending the reply.
func (s *session) authResponderLoop(ctx context.Context, inbox <-chan []byte) {
	var lastIgnored time.Time
	for {
		select {
		case <-ctx.Done():
			return
		case datagram, ok := <-inbox:
			if !ok {
				return
			}
			req, found := parseAuthenticationRequest(datagram)
			if !found || req.RequestedLevel == authLevelNone {
				if time.Since(lastIgnored) > 5*time.Second {
					keylog("auth", "rx ignored bytes=%d (not AuthenticationRequest)", len(datagram))
					lastIgnored = time.Now()
				}
				continue
			}
			keylog("auth", "request level=%s token=%dB", authLevelName(req.RequestedLevel), len(req.Token))
			s.mu.Lock()
			car := s.car
			if car == nil {
				s.mu.Unlock()
				keylog("auth", "request dropped: no live session")
				s.emitEvent("presence_auth_failed", fmt.Errorf("no live session"))
				continue
			}
			sendCtx, cancel := context.WithTimeout(ctx, s.commandTimeout)
			err := sendAuthenticationResponse(sendCtx, car, req.RequestedLevel)
			cancel()
			if sessionDroppedError(err) {
				s.teardownLocked()
				s.emitPresenceDisconnectedLocked(err)
			}
			s.mu.Unlock()
			if err == nil {
				keylog("auth", "response ok level=%s", authLevelName(req.RequestedLevel))
				s.emitEvent("presence_auth_ok", nil)
				continue
			}
			keylog("auth", "response failed level=%s: %v", authLevelName(req.RequestedLevel), err)
			s.emitEvent("presence_auth_failed", err)
		}
	}
}

// presenceLoop polls the vehicle's beacon on cfg.scanInterval and drives
// presenceStep, translating its verdicts into the one BLE side effect that
// matters: keeping a live authenticated VCSEC session up while the vehicle
// is judged nearby, answering VCSEC AuthenticationRequest messages for
// passive unlock/drive, and releasing the session on departure without an
// explicit lock so the vehicle's Walk-Away Door Lock setting remains
// authoritative. Runs until ctx is cancelled by stopPresenceLocked.
func (s *session) presenceLoop(ctx context.Context, cfg presenceConfig, generation uint64) {
	defer func() {
		s.mu.Lock()
		current := s.presenceGeneration == generation
		if current {
			s.presenceCancel = nil
		}
		s.mu.Unlock()
		if current {
			s.emitEvent("presence_stopped", nil)
		}
	}()

	s.mu.Lock()
	err := s.ensureBluezLocked()
	s.mu.Unlock()
	if err != nil {
		s.emitEvent("presence_error", err)
		return
	}

	// Keep trying to start the scanner for as long as presence mode is
	// requested. A denied Powered write or a not-yet-ready bluetoothd used
	// to emit presence_stopped on the first tick; the listener then stayed
	// dead until a dashboard refresh spawned a new connect path.
	for {
		if ctx.Err() != nil {
			return
		}
		watchCtx, watchCancel := context.WithTimeout(ctx, s.connectTimeout)
		watcher, err := s.bluez.Watch(watchCtx, s.adapterID, s.vin)
		watchCancel()
		if err != nil {
			keylog("presence", "watcher start failed: %v (retrying)", err)
			s.emitEvent("presence_error", err)
			select {
			case <-ctx.Done():
				return
			case <-time.After(cfg.scanInterval):
			}
			continue
		}
		keylog("presence", "watcher started")
		s.runPresenceWatcher(ctx, cfg, watcher)
		watcher.Stop(context.Background())
	}
}

func (s *session) runPresenceWatcher(ctx context.Context, cfg presenceConfig, watcher *bluez.Watcher) {
	var (
		near           bool
		consecNear     int
		lastSeen       time.Time
		peekErrors     int
		lastLog        time.Time
		lastLive       bool
		lastRSSI       int16
		lastGATT       bool
		linkDownStreak int
	)

	restartWatcher := func() bool {
		keylog("presence", "restarting watcher after peek errors")
		watcher.Stop(context.Background())
		watchCtx, watchCancel := context.WithTimeout(ctx, s.connectTimeout)
		newWatcher, err := s.bluez.Watch(watchCtx, s.adapterID, s.vin)
		watchCancel()
		if err != nil {
			s.emitEvent("presence_error", err)
			return false
		}
		watcher = newWatcher
		peekErrors = 0
		return true
	}

	for {
		if ctx.Err() != nil {
			return
		}

		s.mu.Lock()
		var bzConn *bluez.Connection
		if bz, ok := s.conn.(*bluez.Connection); ok {
			bzConn = bz
		}
		gattUp := s.car != nil
		if bzConn != nil {
			select {
			case <-bzConn.Dropped():
				keylog("link", "GATT Dropped() signal")
				s.teardownLocked()
				s.emitPresenceDisconnectedLocked(fmt.Errorf("GATT link dropped"))
				gattUp = false
				bzConn = nil
			default:
			}
		}
		s.mu.Unlock()

		// Do not Peek/StartDiscovery while GATT is up. Tesla stops
		// advertising after connect, and BlueZ aborts the LE link
		// (Dropped / le-connection-abort-by-local) if discovery restarts.
		if gattUp {
			near = true
			lastSeen = time.Now()
			if bzConn != nil {
				select {
				case <-ctx.Done():
					return
				case <-bzConn.Dropped():
					s.mu.Lock()
					if bz, ok := s.conn.(*bluez.Connection); ok && bz == bzConn {
						keylog("link", "GATT Dropped() signal")
						s.teardownLocked()
						s.emitPresenceDisconnectedLocked(fmt.Errorf("GATT link dropped"))
					}
					s.mu.Unlock()
					linkDownStreak = 0
					continue
				case <-time.After(cfg.scanInterval):
				}

				linkCtx, linkCancel := context.WithTimeout(ctx, 400*time.Millisecond)
				connected, linkErr := bzConn.DeviceConnected(linkCtx)
				linkCancel()
				if linkErr != nil {
					keylog("link", "Connected read failed (keeping session): %v", linkErr)
				} else if !connected {
					linkDownStreak++
					if linkDownStreak >= 2 {
						s.mu.Lock()
						if bz, ok := s.conn.(*bluez.Connection); ok && bz == bzConn {
							keylog("link", "BlueZ Connected=false - tearing down")
							s.teardownLocked()
							s.emitPresenceDisconnectedLocked(fmt.Errorf("BlueZ reports disconnected"))
						}
						s.mu.Unlock()
						linkDownStreak = 0
						continue
					}
				} else {
					linkDownStreak = 0
				}
			} else {
				select {
				case <-ctx.Done():
					return
				case <-time.After(cfg.scanInterval):
				}
			}

			now := time.Now()
			if now.Sub(lastLog) >= 30*time.Second || !lastGATT {
				keylog("presence", "tick live=false rssi=0 near=true gatt=true known=false action=stay")
				lastLog, lastLive, lastRSSI, lastGATT = now, false, 0, true
			}
			s.mu.Lock()
			if s.car != nil {
				if s.vcsecPrimeDueLocked(now) {
					primeCtx, primeCancel := context.WithTimeout(ctx, s.commandTimeout)
					s.primeVCSECLocked(primeCtx)
					primeCancel()
				}
				s.resetIdleTimerLocked()
			}
			s.mu.Unlock()
			continue
		}
		linkDownStreak = 0

		now := time.Now()
		// Wait (not a single Peek) so the first advertisement is caught
		// the same way refresh's scan() does, and so we do not sit idle
		// for scanInterval before even looking.
		waitCtx, cancel := context.WithTimeout(ctx, cfg.scanInterval)
		result, err := watcher.Wait(waitCtx)
		cancel()
		if err != nil {
			peekErrors++
			keylog("presence", "peek error (%d): %v", peekErrors, err)
			if peekErrors >= 5 && !restartWatcher() {
				return
			}
			continue
		}
		peekErrors = 0

		live := result != nil && result.HasRSSI
		var rssi int16
		if live {
			rssi = result.RSSI
		}
		if result != nil {
			s.mu.Lock()
			s.lastBeacon = result
			s.mu.Unlock()
		}

		var action presenceAction
		near, consecNear, lastSeen, action = presenceStep(cfg, near, consecNear, lastSeen, now, live, rssi, false)

		if action != presenceActionNone || live != lastLive || rssi != lastRSSI || lastGATT || now.Sub(lastLog) >= 30*time.Second {
			keylog("presence", "tick live=%v rssi=%d near=%v gatt=false known=%v action=%s",
				live, rssi, near, result != nil, presenceActionName(action))
			lastLog, lastLive, lastRSSI, lastGATT = now, live, rssi, false
		}

		switch action {
		case presenceActionArrive, presenceActionStayNear:
			if !presenceLiveNear(live, rssi, cfg.nearRSSI) {
				continue
			}
			s.mu.Lock()
			inBackoff := s.connectBackoffActive(now)
			target := s.connectTargetLocked(result)
			s.mu.Unlock()
			if inBackoff || target == nil {
				continue
			}
			connCtx, cancel := context.WithTimeout(ctx, s.connectTimeout)
			s.mu.Lock()
			connErr := s.ensureConnectedLocked(connCtx, "", target)
			if connErr == nil {
				s.clearConnectBackoffLocked()
				s.ensureAuthTapLocked(ctx)
				s.primeVCSECLocked(connCtx)
				s.resetIdleTimerLocked()
			} else {
				s.scheduleConnectBackoffLocked()
			}
			s.mu.Unlock()
			cancel()
			if connErr != nil {
				near, consecNear = false, 0
				s.emitEvent("presence_error", connErr)
				continue
			}
			s.emitEvent("presence_near", nil)
		case presenceActionDepart:
			s.mu.Lock()
			s.clearConnectBackoffLocked()
			s.teardownLocked()
			s.mu.Unlock()
			s.emitEvent("presence_far", nil)
		}
	}
}

func presenceActionName(a presenceAction) string {
	switch a {
	case presenceActionNone:
		return "none"
	case presenceActionArrive:
		return "arrive"
	case presenceActionStayNear:
		return "stay"
	case presenceActionDepart:
		return "depart"
	default:
		return fmt.Sprintf("%d", a)
	}
}

// dispatch runs one command against the (lazily connected) session and
// captures its output. Mirrors cmd/tesla-control/main.go's runCommand()
// error-text formatting (not vendored - main.go isn't reused wholesale,
// only commands.go is) so switching between this path and the core's
// one-shot subprocess fallback doesn't change what error text reaches the
// app.
func (s *session) dispatch(req request) response {
	// keygen is pure crypto, no BLE session required (or wanted - it must
	// work on first run, before any connection exists). presence-start/-stop
	// manage their own goroutine and locking, independent of the
	// connect+execute path below.
	switch req.Cmd {
	case "keygen":
		s.mu.Lock()
		defer s.mu.Unlock()
		return s.dispatchKeygen(req)
	case "presence-start":
		return s.dispatchPresenceStart(req)
	case "presence-stop":
		return s.dispatchPresenceStop(req)
	}

	s.mu.Lock()
	defer s.mu.Unlock()

	connectTarget := s.presenceBeaconTargetLocked()
	connectCtx, cancel := context.WithTimeout(context.Background(), s.connectTimeout)
	connectErr := s.ensureConnectedLocked(connectCtx, req.Cmd, connectTarget)
	cancel()
	if connectErr != nil {
		return response{ID: req.ID, OK: false, Stderr: connectErr.Error(), ExitCode: 1}
	}

	cmdCtx, cancel := context.WithTimeout(context.Background(), s.commandTimeout)
	defer cancel()

	args := append([]string{req.Cmd}, req.Args...)
	var execErr error
	stdout, stderr := captureOutput(func() {
		execErr = execute(cmdCtx, nil, s.car, args)
	})

	if commandsWithoutSession[req.Cmd] {
		if execErr == nil {
			// Hold the connection open rather than disconnecting the
			// instant the write succeeds: SendAddKeyRequestWithRole's own
			// doc comment warns a nil return only means the request was
			// transmitted, not that the vehicle did anything with it -
			// confirmed live, a request sent by a process that then
			// immediately disconnected produced no visible NFC-confirmation
			// prompt on the car's screen at all. This also gives the human
			// operator time to physically walk up and tap the card.
			time.Sleep(addKeyRequestGracePeriod)
		}
		// This connection never called StartSession (see
		// commandsWithoutSession) - it must not survive to be reused by a
		// later command that expects an authenticated one.
		s.teardownLocked()
	} else {
		s.resetIdleTimerLocked()
	}

	if execErr == nil {
		return response{ID: req.ID, OK: true, Stdout: stdout, Stderr: stderr, ExitCode: 0}
	}
	switch {
	case protocol.MayHaveSucceeded(execErr):
		stderr += fmt.Sprintf("Couldn't verify success: %s\n", execErr)
	case errors.Is(execErr, protocol.ErrNoSession):
		stderr += "You must provide a private key with -key-name or -key-file to execute this command\n"
	default:
		stderr += fmt.Sprintf("Failed to execute command: %s\n", execErr)
	}
	return response{ID: req.ID, OK: false, Stdout: stdout, Stderr: stderr, ExitCode: 1}
}

// captureOutput redirects the process-wide os.Stdout/os.Stderr for the
// duration of f, so commands_vendor.go's handlers (which fmt.Println
// straight to them, same as upstream tesla-control) don't collide with
// this process's own stdout, which is reserved for the JSON response/event
// stream. Safe only because its sole production caller - dispatch() - holds
// session.mu for its entire call, so no two invocations of captureOutput
// (which swaps process-wide globals) can ever run concurrently; a future
// caller into execute() that doesn't hold mu would silently break this.
func captureOutput(f func()) (stdout, stderr string) {
	origOut, origErr := os.Stdout, os.Stderr
	outR, outW, err := os.Pipe()
	if err != nil {
		f()
		return "", ""
	}
	errR, errW, err := os.Pipe()
	if err != nil {
		outR.Close()
		outW.Close()
		f()
		return "", ""
	}
	os.Stdout, os.Stderr = outW, errW

	outCh := make(chan string, 1)
	errCh := make(chan string, 1)
	go func() { b, _ := io.ReadAll(outR); outCh <- string(b) }()
	go func() { b, _ := io.ReadAll(errR); errCh <- string(b) }()

	// Run the wrapped handler, but capture any panic and let the cleanup
	// below run regardless, so a panicking handler can't leave the
	// process-global os.Stdout/os.Stderr swapped to a pipe (which would
	// silently swallow every later response/event) or strand the reader
	// goroutines.
	var panicValue interface{}
	func() {
		defer func() { panicValue = recover() }()
		f()
	}()

	// Closing the write ends lets the readers hit EOF and finish; the pipe
	// is drained only after f returns so a handler that fills the OS buffer
	// mid-call can't deadlock against us.
	outW.Close()
	errW.Close()
	os.Stdout, os.Stderr = origOut, origErr

	out := <-outCh
	errOut := <-errCh
	_ = outR.Close()
	_ = errR.Close()

	if panicValue != nil {
		panic(panicValue)
	}
	return out, errOut
}

func main() {
	var (
		vin            string
		keyFile        string
		adapterID      string
		bleBackend     string
		connectTimeout time.Duration
		commandTimeout time.Duration
		idleTimeout    time.Duration
		logDir         string
	)
	flag.StringVar(&vin, "vin", "", "Vehicle Identification Number (required)")
	flag.StringVar(&keyFile, "key-file", "", "Private key file (required)")
	flag.StringVar(&adapterID, "bt-adapter-id", "", "Bluetooth adapter ID (Linux only)")
	flag.StringVar(&bleBackend, "ble-backend", "hci", "BLE transport backend: hci (go-ble raw HCI) or bluez (org.bluez D-Bus)")
	flag.DurationVar(&connectTimeout, "connect-timeout", 20*time.Second, "Timeout for establishing a connection")
	flag.DurationVar(&commandTimeout, "command-timeout", 5*time.Second, "Timeout for each command sent to the vehicle")
	flag.DurationVar(&idleTimeout, "idle-timeout", 90*time.Second, "Tear down the BLE session after this much inactivity")
	flag.StringVar(&logDir, "log-dir", "", "Directory for daily phone-key logs (default: $ELECTRIC_EEL_LOG_DIR or ~/Documents/ElectricEel)")
	flag.Parse()

	if vin == "" || keyFile == "" {
		fmt.Fprintln(os.Stderr, "tesla-session: -vin and -key-file are required")
		os.Exit(2)
	}
	if bleBackend != "hci" && bleBackend != "bluez" {
		fmt.Fprintf(os.Stderr, "tesla-session: invalid -ble-backend %q (want hci or bluez)\n", bleBackend)
		os.Exit(2)
	}

	initKeyLog(logDir)
	keylog("session", "tesla-session start vin=%s backend=%s connect=%s command=%s idle=%s",
		vin, bleBackend, connectTimeout, commandTimeout, idleTimeout)

	s := &session{
		vin:            vin,
		keyFile:        keyFile,
		adapterID:      adapterID,
		bleBackend:     bleBackend,
		connectTimeout: connectTimeout,
		commandTimeout: commandTimeout,
		idleTimeout:    idleTimeout,
	}
	s.enc = json.NewEncoder(os.Stdout)

	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGTERM, syscall.SIGINT)
	go func() {
		<-sigCh
		keylog("session", "signal - shutting down")
		s.mu.Lock()
		s.stopPresenceLocked()
		s.teardownLocked()
		s.closeBluezLocked()
		s.mu.Unlock()
		os.Exit(0)
	}()

	scanner := bufio.NewScanner(os.Stdin)
	scanner.Buffer(make([]byte, 0, 64*1024), 1024*1024)
	for scanner.Scan() {
		line := scanner.Bytes()
		if len(line) == 0 {
			continue
		}
		var req request
		if err := json.Unmarshal(line, &req); err != nil {
			s.writeResponse(response{OK: false, Stderr: fmt.Sprintf("tesla-session: malformed request: %s", err), ExitCode: 1})
			continue
		}
		s.writeResponse(s.dispatch(req))
	}

	keylog("session", "stdin closed - shutting down")
	s.mu.Lock()
	s.stopPresenceLocked()
	s.teardownLocked()
	s.closeBluezLocked()
	s.mu.Unlock()
}
