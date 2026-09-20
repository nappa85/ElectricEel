package main

import (
	"testing"
	"time"
)

// Validation happens before any BLE touch: malformed requests fail fast
// with usage errors (exit 2), no adapter needed.
func TestDispatchNavigateRejectsBadArgs(t *testing.T) {
	s := &session{}
	for _, args := range [][]string{
		{},
		{"bogus", "x"},
		{"gps", "48.8"},
		{"gps", "not-a-number", "2.2"},
		{"gps", "48.8", "not-a-number"},
		{"gps", "48.8", "2.2", "extra"},
		{"address"},
		{"address", "   "},
		{"address", "ok", "extra"},
	} {
		resp := s.dispatchNavigate(request{ID: "n", Cmd: "navigate", Args: args})
		if resp.OK {
			t.Fatalf("expected failure for args %q", args)
		}
		if resp.ExitCode != 2 {
			t.Fatalf("expected exit 2 (usage) for args %q, got %d (%q)", args, resp.ExitCode, resp.Stderr)
		}
		if resp.ID != "n" {
			t.Fatalf("response ID = %q, want correlation", resp.ID)
		}
	}
}

// No key file, no adapter, no vehicle: a well-formed request must fail as
// a well-formed non-ok response (not a hang — this also guards the
// session.mu lock discipline against self-deadlock), with the connect
// error surfaced.
func TestDispatchNavigateReportsConnectFailure(t *testing.T) {
	s := &session{
		vin:            "5YJ3E1EA0PF000000",
		keyFile:        "/nonexistent/path/private_key.pem",
		connectTimeout: 500 * time.Millisecond,
		commandTimeout: 500 * time.Millisecond,
	}
	for _, args := range [][]string{
		{"gps", "48.8584", "2.2945"},
		{"address", "Eiffel Tower"},
	} {
		resp := s.dispatchNavigate(request{ID: "n1", Cmd: "navigate", Args: args})
		if resp.OK {
			t.Fatalf("expected ok=false with no key/vehicle, args %q", args)
		}
		if resp.ID != "n1" {
			t.Errorf("response ID = %q, want %q", resp.ID, "n1")
		}
		if resp.Stderr == "" {
			t.Errorf("expected stderr explaining the failure, args %q", args)
		}
	}
}
