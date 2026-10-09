package main

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"net"
	"sync"
	"testing"
	"time"

	"github.com/teslamotors/vehicle-command/pkg/connector"
	"github.com/teslamotors/vehicle-command/pkg/protocol"
	universal "github.com/teslamotors/vehicle-command/pkg/protocol/protobuf/universalmessage"
	"github.com/teslamotors/vehicle-command/pkg/vehicle"
	"google.golang.org/protobuf/proto"
)

// Observe real Vehicle/dispatcher handshake traffic without a radio. Rejecting
// the handshake is intentional: this regression concerns attempting the missing
// authentication before declaring an existing link ready for a command.
type reviewHandshakeConnector struct {
	inbox   chan []byte
	mu      sync.Mutex
	domains []protocol.Domain
}

func (c *reviewHandshakeConnector) Receive() <-chan []byte { return c.inbox }
func (c *reviewHandshakeConnector) VIN() string            { return "5YJ3E1EA0PF000000" }
func (c *reviewHandshakeConnector) Close()                 {}
func (c *reviewHandshakeConnector) PreferredAuthMethod() connector.AuthMethod {
	return connector.AuthMethodGCM
}
func (c *reviewHandshakeConnector) RetryInterval() time.Duration  { return time.Millisecond }
func (c *reviewHandshakeConnector) AllowedLatency() time.Duration { return time.Second }
func (c *reviewHandshakeConnector) Send(_ context.Context, data []byte) error {
	var message universal.RoutableMessage
	if err := proto.Unmarshal(data, &message); err != nil {
		return err
	}
	c.mu.Lock()
	c.domains = append(c.domains, message.GetToDestination().GetDomain())
	c.mu.Unlock()
	return errors.New("test vehicle rejects handshake")
}

func TestProductionReviewReusedLinkAuthenticatesInfotainmentCommands(t *testing.T) {
	// Both dashboard queries and ordinary actions must add the missing domain.
	for _, command := range []string{"state", "navigate", "climate-on", "charging-start", "media-toggle-playback", "windows-close", "charge-port-open"} {
		t.Run(command, func(t *testing.T) {
			link := &reviewHandshakeConnector{inbox: make(chan []byte)}
			scalar := make([]byte, 32)
			scalar[31] = 1
			key := protocol.UnmarshalECDHPrivateKey(scalar)
			if key == nil {
				t.Fatal("invalid fixture key")
			}
			car, err := vehicle.NewVehicle(link, key, nil)
			if err != nil {
				t.Fatal(err)
			}
			ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
			defer cancel()
			if err := car.Connect(ctx); err != nil {
				t.Fatal(err)
			}
			defer car.Disconnect()
			s := &session{car: car, conn: link}
			// Infotainment has not been authenticated, as on a presence-only link.
			_ = s.ensureConnectedLocked(ctx, command, nil)
			link.mu.Lock()
			defer link.mu.Unlock()
			for _, domain := range link.domains {
				if domain == protocol.DomainInfotainment {
					return
				}
			}
			t.Fatalf("reused link skipped infotainment authentication for %s; handshake domains = %v", command, link.domains)
		})
	}
}

type reviewResponseWriteFailure struct {
	net.Conn
	writes int
}

func (c *reviewResponseWriteFailure) Write(data []byte) (int, error) {
	c.writes++
	if c.writes > 1 { // Allow hello, then model a fatal outbound transport error.
		return 0, errors.New("parent no longer accepts frames")
	}
	return c.Conn.Write(data)
}

func TestProductionReviewResponseWriteFailureStopsServing(t *testing.T) {
	parent, child := net.Pipe()
	s := &session{bleBackend: "bluez"}
	done := make(chan struct{})
	go func() {
		defer close(done)
		s.serveConn(&reviewResponseWriteFailure{Conn: child})
	}()
	defer func() {
		parent.Close()
		select {
		case <-done:
		case <-time.After(time.Second):
			t.Error("server did not stop after test cleanup")
		}
	}()
	if err := parent.SetDeadline(time.Now().Add(2 * time.Second)); err != nil {
		t.Fatal(err)
	}
	readFrame(t, bufio.NewReader(parent))
	if _, err := parent.Write([]byte("{\"type\":\"request\",\"id\":\"failure\",\"cmd\":\"unknown\"}\n")); err != nil {
		t.Fatal(err)
	}
	select {
	case <-done:
	case <-time.After(time.Second):
		t.Error("fatal response write left serveConn waiting for more commands instead of closing and releasing BLE state")
	}
}

func TestProductionReviewEventWriteFailureDoesNotDeadlockSessionOwner(t *testing.T) {
	parent, child := net.Pipe()
	defer parent.Close()
	s := &session{bleBackend: "bluez"}
	done := make(chan struct{})
	go func() {
		defer close(done)
		s.serveConn(&reviewResponseWriteFailure{Conn: child})
	}()
	if err := parent.SetDeadline(time.Now().Add(2 * time.Second)); err != nil {
		t.Fatal(err)
	}
	readFrame(t, bufio.NewReader(parent))
	s.mu.Lock() // The presence loop emits while owning the session mutex.
	s.emitEvent("presence_error", errors.New("test link error"))
	s.mu.Unlock()
	select {
	case <-done:
	case <-time.After(time.Second):
		parent.Close()
		t.Fatal("failed event write did not wake shutdown")
	}
}

func TestProductionReviewWriteFailureCancelsInFlightWork(t *testing.T) {
	parent, child := net.Pipe()
	defer parent.Close()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	conn := &reviewResponseWriteFailure{Conn: child, writes: 1}
	s := &session{parentContext: ctx, parentCancel: cancel, parentConn: conn, enc: json.NewEncoder(conn)}
	command, commandCancel := context.WithTimeout(s.requestContext(), time.Minute)
	defer commandCancel()
	if err := s.encodeWithDeadline(heartbeatFrame{Type: "heartbeat"}); err == nil {
		t.Fatal("fixture must fail the heartbeat write")
	}
	if !errors.Is(command.Err(), context.Canceled) {
		t.Fatal("fatal heartbeat write left in-flight BLE work running until its command deadline")
	}
}
