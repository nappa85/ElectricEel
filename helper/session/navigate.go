// BLE navigation dispatch for tesla-session.
//
// The Rust core (helper/src/share.rs) is the authoritative destination
// parser: it sends this process a pre-parsed `navigate` request over
// stdin — ["gps", lat, lon] or ["address", text] — and this file
// re-validates the values (defense in depth) and executes the matching
// signed navigation action over the live BLE session. No HTTP, no Fleet
// API, no OAuth: everything rides the existing authenticated
// BLE connection (BlueZ or HCI), exactly like lock/unlock do.
// See docs/navigation-share.md §1.
package main

import (
	"context"
	"strconv"
	"strings"
)

// maxNavigateTextLen mirrors helper/src/share.rs MAX_SHARE_TEXT_LEN.
const maxNavigateTextLen = 2000

// dispatchNavigate handles the session "navigate" request. Args come
// pre-parsed from the Rust core:
//
//	["gps", lat, lon]     -> field-53 NavigationGpsRequest (exact coordinates)
//	["address", text]     -> field-21 NavigationRequest (car resolves the text)
//
// Shape validation runs BEFORE connecting, so usage errors fail fast
// without touching the radio.
func (s *session) dispatchNavigate(req request) response {
	if len(req.Args) < 1 {
		return response{ID: req.ID, OK: false, Stderr: "navigate: missing kind argument\n", ExitCode: 2}
	}

	var (
		lat, lon float64
		text     string
		isGPS    bool
	)
	switch req.Args[0] {
	case "gps":
		if len(req.Args) != 3 {
			return response{ID: req.ID, OK: false, Stderr: "navigate: gps needs lat and lon\n", ExitCode: 2}
		}
		var err error
		if lat, err = strconv.ParseFloat(strings.TrimSpace(req.Args[1]), 64); err != nil {
			return response{ID: req.ID, OK: false, Stderr: "navigate: invalid lat\n", ExitCode: 2}
		}
		if lon, err = strconv.ParseFloat(strings.TrimSpace(req.Args[2]), 64); err != nil {
			return response{ID: req.ID, OK: false, Stderr: "navigate: invalid lon\n", ExitCode: 2}
		}
		isGPS = true
	case "address":
		if len(req.Args) != 2 {
			return response{ID: req.ID, OK: false, Stderr: "navigate: address needs text\n", ExitCode: 2}
		}
		text = strings.TrimSpace(req.Args[1])
		if text == "" || len([]rune(text)) > maxNavigateTextLen {
			return response{ID: req.ID, OK: false, Stderr: "navigate: invalid address text\n", ExitCode: 2}
		}
	default:
		return response{ID: req.ID, OK: false, Stderr: "navigate: unknown kind (want gps or address)\n", ExitCode: 2}
	}

	// A general-purpose session (all domains, infotainment included):
	// "" is not a commandsWithoutSession command, so ensureConnectedLocked
	// performs the full StartSession handshake like any ordinary command.
	connectCtx, cancel := context.WithTimeout(context.Background(), s.connectTimeout)
	s.mu.Lock()
	connectErr := s.ensureConnectedLocked(connectCtx, "", nil)
	s.mu.Unlock()
	cancel()
	if connectErr != nil {
		return response{ID: req.ID, OK: false, Stderr: "navigate: " + connectErr.Error() + "\n", ExitCode: 1}
	}

	cmdCtx, cancel := context.WithTimeout(context.Background(), s.commandTimeout)
	defer cancel()

	s.mu.Lock()
	defer s.mu.Unlock()
	// The connection may have dropped between ensure and use; reuse the
	// same lock discipline as dispatch().
	if s.car == nil {
		return response{ID: req.ID, OK: false, Stderr: "navigate: session lost\n", ExitCode: 1}
	}

	if isGPS {
		// Order 0 (unknown/default): matches Teslemetry's tested BLE
		// behavior — the car replaces the trip.
		if err := s.car.NavigateToGPS(cmdCtx, lat, lon, 0); err != nil {
			return response{ID: req.ID, OK: false, Stderr: "navigate: " + err.Error() + "\n", ExitCode: 1}
		}
	} else {
		if err := s.car.NavigateToDestination(cmdCtx, text); err != nil {
			return response{ID: req.ID, OK: false, Stderr: "navigate: " + err.Error() + "\n", ExitCode: 1}
		}
	}
	s.resetIdleTimerLocked()
	return response{ID: req.ID, OK: true, Stdout: "navigation started\n", ExitCode: 0}
}
