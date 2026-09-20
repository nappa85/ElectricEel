package main

import (
	"bufio"
	"encoding/json"
	"net"
	"strings"
	"testing"
	"time"
)

// readFrame reads one JSON line from r and returns it decoded. Every
// frame must carry a "type" discriminator.
func readFrame(t *testing.T, r *bufio.Reader) map[string]any {
	t.Helper()
	line, err := r.ReadBytes('\n')
	if err != nil {
		t.Fatalf("read frame: %v", err)
	}
	var f map[string]any
	if err := json.Unmarshal(line, &f); err != nil {
		t.Fatalf("frame is not JSON %q: %v", line, err)
	}
	if _, ok := f["type"]; !ok {
		t.Fatalf("frame has no type discriminator: %q", line)
	}
	return f
}

func TestServeConnHelloFirst(t *testing.T) {
	parent, child := net.Pipe()
	defer parent.Close()
	s := &session{vin: "5YJ3E1EA0PF000000", bleBackend: "bluez",
		connectTimeout: time.Second, commandTimeout: time.Second}
	done := make(chan struct{})
	go func() { s.serveConn(child); close(done) }()

	r := bufio.NewReader(parent)
	// The very first frame must be a versioned hello.
	f := readFrame(t, r)
	if f["type"] != "hello" {
		t.Fatalf("first frame type = %v, want hello", f["type"])
	}
	if v, _ := f["v"].(float64); v != protocolVersion {
		t.Fatalf("hello v = %v, want %d", f["v"], protocolVersion)
	}
	parent.Close()
	<-done // serveConn must exit, not buffer for a dead parent
}

func TestServeConnRoundtripWithoutBLE(t *testing.T) {
	parent, child := net.Pipe()
	defer parent.Close()
	dir := t.TempDir()
	s := &session{vin: "5YJ3E1EA0PF000000", bleBackend: "bluez",
		keyFile:        dir + "/key.pem",
		connectTimeout: time.Second, commandTimeout: time.Second}
	done := make(chan struct{})
	go func() { s.serveConn(child); close(done) }()

	r := bufio.NewReader(parent)
	w := bufio.NewWriter(parent)
	writeLine := func(t *testing.T, w *bufio.Writer, line string) {
		t.Helper()
		if _, err := w.WriteString(line); err != nil {
			t.Fatalf("write: %v", err)
		}
		if err := w.Flush(); err != nil {
			t.Fatalf("flush: %v", err)
		}
	}
	readFrame(t, r) // hello

	// keygen is pure crypto: full request/response cycle, no radio.
	writeLine(t, w, "{\"type\":\"request\",\"id\":\"k1\",\"cmd\":\"keygen\",\"args\":[]}\n")
	f := readFrame(t, r)
	if f["type"] != "response" || f["id"] != "k1" || f["ok"] != true {
		t.Fatalf("keygen response = %v", f)
	}
	if !strings.Contains(f["stdout"].(string), "PUBLIC KEY") {
		t.Fatalf("keygen stdout missing PEM: %v", f["stdout"])
	}

	// Unknown command surfaces as a well-formed error response.
	writeLine(t, w, "{\"type\":\"request\",\"id\":\"k2\",\"cmd\":\"definitely-not-a-command\",\"args\":[]}\n")
	f = readFrame(t, r)
	if f["type"] != "response" || f["id"] != "k2" || f["ok"] != false {
		t.Fatalf("unknown-cmd response = %v", f)
	}

	// Malformed JSON and wrong frame type are errors, not crashes.
	writeLine(t, w, "not json\n")
	f = readFrame(t, r)
	if f["type"] != "response" || f["ok"] != false {
		t.Fatalf("malformed response = %v", f)
	}
	writeLine(t, w, "{\"type\":\"bogus\",\"id\":\"k3\"}\n")
	f = readFrame(t, r)
	if f["type"] != "response" || f["id"] != "k3" || f["ok"] != false {
		t.Fatalf("bad-type response = %v", f)
	}
	parent.Close()
	<-done
}
