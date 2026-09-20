// Parent transport: Unix-domain socket with tagged JSON-lines frames.
//
// The parent (Rust SessionClient) creates a private socket path, spawns
// this process with -socket-path, and accepts exactly one connection; the
// path is unlinked right after accept, so no other same-UID process can
// dial in later and spoof responses. Frames:
//
//	parent -> child  {"type":"request","id","cmd","args"}
//	child -> parent  {"type":"hello","v","ble_backend"}          (first line)
//	child -> parent  {"type":"response","id","ok",...}           (replies)
//	child -> parent  {"type":"event","kind",...}                 (presence)
//	child -> parent  {"type":"heartbeat","unix"}                 (liveness)
//
// stdin/stdout are NOT the protocol anymore: stdout is plain logs (the
// vendored command handlers still print through it, captured per-command
// by captureOutput), which removes the entire class of
// response-vs-command-output interleaving bugs the old stdio transport
// had. Heartbeats flow even mid-command, so they prove the process is
// alive as well as the connection; a failed heartbeat write means the
// parent is gone and this process exits instead of lingering.
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"net"
	"os"
	"time"
)

// taggedEvent is event with its frame discriminator. Set at encode time
// (not at construction) so the dozens of emitEvent call sites stay
// untouched.
type taggedEvent struct {
	Type string `json:"type"`
	event
}

// heartbeatLoop ticks heartbeat frames until done is closed. A failed
// write means the parent is gone; it returns and lets the read loop drive
// the shutdown below (which also fires, since the connection is dead).
func (s *session) heartbeatLoop(done <-chan struct{}) {
	ticker := time.NewTicker(heartbeatInterval)
	defer ticker.Stop()
	for {
		select {
		case <-done:
			return
		case <-ticker.C:
			s.writeMu.Lock()
			if s.enc != nil {
				if err := s.enc.Encode(heartbeatFrame{Type: "heartbeat", Unix: time.Now().Unix()}); err != nil {
					s.writeMu.Unlock()
					keylog("session", "heartbeat write failed - parent gone")
					return
				}
			}
			s.writeMu.Unlock()
		}
	}
}

// serveConn runs the request loop over an established parent connection.
// It returns only when the connection breaks (parent gone): it tears down
// BLE state and returns instead of buffering work for a parent that will
// never read it. The process exit lives in main, not here, so tests can
// drive this loop over net.Pipe.
func (s *session) serveConn(conn net.Conn) {
	s.enc = json.NewEncoder(conn)
	if err := s.enc.Encode(helloFrame{Type: "hello", Version: protocolVersion, BLEBackend: s.bleBackend}); err != nil {
		keylog("session", "hello write failed: %v", err)
		os.Exit(1)
	}

	hbDone := make(chan struct{})
	defer close(hbDone)
	go s.heartbeatLoop(hbDone)

	scanner := bufio.NewScanner(conn)
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
		if req.Type != "request" {
			s.writeResponse(response{ID: req.ID, OK: false, Stderr: fmt.Sprintf("tesla-session: unknown frame type %q", req.Type), ExitCode: 1})
			continue
		}
		s.writeResponse(s.dispatch(req))
	}

	keylog("session", "parent connection closed - shutting down")
	s.shutdown()
}

// shutdown releases all BLE state. Idempotent; safe with no live session.
func (s *session) shutdown() {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.stopPresenceLocked()
	s.teardownLocked()
	s.closeBluezLocked()
}

// dialParent connects to the parent's socket with a deadline. A failure
// here is a parent-side bug (it listens before spawning); exit loudly
// rather than falling back to anything.
func dialParent(socketPath string) net.Conn {
	conn, err := net.DialTimeout("unix", socketPath, 5*time.Second)
	if err != nil {
		fmt.Fprintf(os.Stderr, "tesla-session: cannot dial parent socket: %s\n", err)
		os.Exit(1)
	}
	return conn
}
