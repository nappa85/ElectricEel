package main

import (
	"strings"
	"testing"
)

// Production-review: navigation address text must be a single human-readable
// line. Previously, dispatchNavigate only rejected empty and >2000-rune text;
// embedded controls reached the signed BLE action and the car geocoder as
// multi-line or binary payloads. They must fail fast with usage (exit 2)
// before touching the radio, like every other malformed shape.
func TestProductionNavigateRejectsControlCharacters(t *testing.T) {
	s := &session{}
	for _, text := range []string{
		"hello\nworld",
		"hello\rworld",
		"hello\x00world",
		"hello\x1fworld",
		"hello\x7fworld",
		"Eiffel Tower\nhttps://maps.google.com/?q=48.8584,2.2945",
	} {
		resp := s.dispatchNavigate(request{ID: "nav", Cmd: "navigate", Args: []string{"address", text}})
		if resp.OK {
			t.Fatalf("address %q must not succeed", text)
		}
		if resp.ExitCode != 2 {
			t.Fatalf("address %q must fail fast with exit 2 (usage), got %d (%q) — control chars reached the radio path", text, resp.ExitCode, resp.Stderr)
		}
		if !strings.Contains(resp.Stderr, "invalid address") {
			t.Fatalf("address %q stderr must explain the usage error, got %q", text, resp.Stderr)
		}
	}
}
