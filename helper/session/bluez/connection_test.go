package bluez

import (
	"bytes"
	"context"
	"testing"
	"time"

	"github.com/godbus/dbus"
	"github.com/teslamotors/vehicle-command/pkg/connector"
)

// newTestConnection returns a Connection wired to the fake, primed for
// Send/RX unit tests (no rxLoop goroutine; rx/framing is exercised directly).
func newTestConnection(bus *fakeBluez) *Connection {
	return &Connection{
		vin:         "5YJ3E1EA0PF000000",
		bus:         bus,
		devPath:     bus.devPath(),
		svcPath:     bus.svcPath(),
		txPath:      bus.txPath(),
		rxPath:      bus.rxPath(),
		inbox:       make(chan []byte, connector.BufferSize),
		blockLength: maxExpectedMTU - 3,
		done:        make(chan struct{}),
		dropped:     make(chan struct{}),
	}
}

func TestConnectFromScanResult(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}
	bus.deviceVisible = true
	bus.servicesResolved = true
	bus.gattReady = true

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	cc, err := connect(ctx, bus, "hci0", vin, &ScanResult{Path: bus.dev.path})
	if err != nil {
		t.Fatalf("connect: %v", err)
	}
	c, ok := cc.(*Connection)
	if !ok {
		t.Fatalf("connect returned %T, want *Connection", cc)
	}
	if !bus.connected {
		t.Error("expected Device1.Connect to have been called")
	}
	if !bus.startedNotify {
		t.Error("expected StartNotify on the RX characteristic")
	}
	if c.blockLength != maxExpectedMTU-3 {
		t.Errorf("blockLength = %d, want %d", c.blockLength, maxExpectedMTU-3)
	}
	if c.VIN() != vin {
		t.Errorf("VIN() = %q, want %q", c.VIN(), vin)
	}
	if bus.matches != 2 {
		t.Errorf("match rules registered = %d, want 2 (RX + Device Connected)", bus.matches)
	}
}

func TestConnectScansWhenNoTarget(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}
	bus.deviceVisible = true
	bus.servicesResolved = true
	bus.gattReady = true

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	if _, err := connect(ctx, bus, "", vin, nil); err != nil {
		t.Fatalf("connect(no target): %v", err)
	}
	if !hasCall(bus.calls, adapterIface+".StartDiscovery") {
		t.Error("expected a scan to run when no target was given")
	}
}

// TestConnectDoesNotScanWithTarget locks in the assumption
// electric-eel-session/main.go's ensureConnectedLocked depends on: passing
// a target skips connect()'s own scan entirely. Without this, a caller that
// already holds its own long-lived discovery session (presenceLoop's
// Watcher) and then calls Connect with a target would still be safe from
// self-collision; if this ever regressed to scanning regardless, that
// caller would silently start colliding with itself again - confirmed live
// as "bluez: start discovery: Operation already in progress" before
// ensureConnectedLocked was fixed to pass its Watcher's last Peek() result
// through instead of nil.
func TestConnectDoesNotScanWithTarget(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}
	bus.deviceVisible = true
	bus.servicesResolved = true
	bus.gattReady = true

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	if _, err := connect(ctx, bus, "", vin, &ScanResult{Path: bus.dev.path}); err != nil {
		t.Fatalf("connect(with target): %v", err)
	}
	if hasCall(bus.calls, adapterIface+".StartDiscovery") {
		t.Error("connect with a target must not scan - it would collide with a caller's own already-open discovery session")
	}
}

func TestConnectFailsWhenDeviceNeverShowsUp(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}
	bus.deviceVisible = false

	ctx, cancel := context.WithTimeout(context.Background(), 150*time.Millisecond)
	defer cancel()

	if _, err := connect(ctx, bus, "hci0", vin, &ScanResult{Path: bus.devPath()}); err == nil {
		t.Fatal("expected connect to fail when the device is not in the object tree")
	}
}

func TestConnectDeliversNotifications(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}
	bus.deviceVisible = true
	bus.servicesResolved = true
	bus.gattReady = true

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	cc, err := connect(ctx, bus, "hci0", vin, &ScanResult{Path: bus.dev.path})
	if err != nil {
		t.Fatalf("connect: %v", err)
	}
	c := cc.(*Connection)

	// A single notification carrying one length-prefixed message.
	framed := []byte{0x00, 0x03, 'f', 'o', 'o'}
	bus.notify(c.rxPath, framed)

	select {
	case msg := <-c.Receive():
		if string(msg) != "foo" {
			t.Errorf("received %q, want %q", msg, "foo")
		}
	case <-time.After(time.Second):
		t.Fatal("timed out waiting for RX datagram")
	}
}

func TestRXFramingAcrossChunks(t *testing.T) {
	c := newTestConnection(newFakeBluez())

	// Message "hello world" (11 bytes) framed as [0x00,0x0b,...], delivered
	// in awkward split chunks across multiple rx() calls.
	frame := append([]byte{0x00, 0x0b}, []byte("hello world")...)
	split1, split2, split3 := frame[:3], frame[3:8], frame[8:]

	c.rx(split1)
	c.rx(split2)
	c.rx(split3)

	select {
	case msg := <-c.Receive():
		if string(msg) != "hello world" {
			t.Errorf("received %q, want %q", msg, "hello world")
		}
	default:
		t.Fatal("expected a reassembled datagram after all chunks arrived")
	}
}

func TestRXRejectsOversizedLength(t *testing.T) {
	c := newTestConnection(newFakeBluez())

	// Length prefix claims 0xFFFF bytes (> maxMessageSize); must reset the
	// buffer rather than allocate.
	c.rx([]byte{0xff, 0xff})
	c.rx([]byte("some trailing bytes"))

	select {
	case msg := <-c.Receive():
		t.Errorf("received %q after oversized length prefix, want nothing", msg)
	default:
	}
}

func TestSendFramesAndChunks(t *testing.T) {
	bus := newFakeBluez()
	c := newTestConnection(bus)

	payload := bytes.Repeat([]byte{0xAB}, 1000)
	if err := c.Send(context.Background(), payload); err != nil {
		t.Fatalf("Send: %v", err)
	}

	// Reassemble the captured chunks and verify framing matches upstream.
	var got []byte
	for _, w := range bus.writes {
		got = append(got, w...)
	}
	want := append([]byte{0x03, 0xe8}, payload...) // 1000 = 0x03e8
	if !bytes.Equal(got, want) {
		t.Errorf("framed output mismatch: got %d bytes, want %d", len(got), len(want))
	}
	// Chunks other than the final one must be exactly blockLength.
	maxChunk := maxExpectedMTU - 3
	for i, w := range bus.writes {
		if i < len(bus.writes)-1 && len(w) != maxChunk {
			t.Errorf("chunk %d is %d bytes, want %d", i, len(w), maxChunk)
		}
	}
}

func TestSendShrinksBlockLengthOnMtuError(t *testing.T) {
	bus := newFakeBluez()
	bus.failLargeWrites = true // remote only supports ATT MTU 23
	c := newTestConnection(bus)

	payload := bytes.Repeat([]byte{0x42}, 500)
	if err := c.Send(context.Background(), payload); err != nil {
		t.Fatalf("Send: %v", err)
	}

	// After the shrink, every captured write must fit the guaranteed size.
	for i, w := range bus.writes {
		if len(w) > defaultMTU-3 {
			t.Errorf("write %d is %d bytes, want <= %d (must shrink on MTU error)", i, len(w), defaultMTU-3)
		}
	}
	if c.blockLength != defaultMTU-3 {
		t.Errorf("blockLength after shrink = %d, want %d", c.blockLength, defaultMTU-3)
	}

	// Reassembled payload must still match framing + content.
	var got []byte
	for _, w := range bus.writes {
		got = append(got, w...)
	}
	want := append([]byte{0x01, 0xf4}, payload...) // 500 = 0x01f4
	if !bytes.Equal(got, want) {
		t.Errorf("framed output after shrink mismatch: got %d bytes, want %d", len(got), len(want))
	}
}

func TestCloseIsIdempotent(t *testing.T) {
	bus := newFakeBluez()
	c := newTestConnection(bus)
	bus.connected = true

	c.Close()
	c.Close() // must be a no-op, not a panic

	if !bus.stoppedNotify {
		t.Error("expected StopNotify on Close")
	}
	if bus.connected {
		t.Error("expected Disconnect on Close")
	}
	if bus.matches != 0 {
		t.Errorf("match rule not removed: matches = %d", bus.matches)
	}
	if !bus.removedMatch {
		t.Error("expected RemoveMatch on Close")
	}
	// The closed channel must have stopped the RX loop; rx() on a closed
	// connection is an internal invariant (Close is terminal).
	select {
	case _, ok := <-c.done:
		if ok {
			t.Error("done channel should be closed")
		}
	case <-time.After(100 * time.Millisecond):
		t.Error("done channel should be closed after Close")
	}
}

func TestDeviceConnectedAndAutoConnect(t *testing.T) {
	bus := newFakeBluez()
	c := newTestConnection(bus)
	ctx := context.Background()

	bus.connected = true
	connected, err := c.DeviceConnected(ctx)
	if err != nil || !connected {
		t.Fatalf("DeviceConnected = %v, %v; want true, nil", connected, err)
	}
	if err := c.SetTrusted(true); err != nil {
		t.Fatalf("SetTrusted: %v", err)
	}
	if !bus.trusted {
		t.Fatal("expected Trusted=true on fake")
	}
	if err := c.SetAutoConnect(true); err != nil {
		t.Fatalf("SetAutoConnect: %v", err)
	}
	if !bus.autoConnect {
		t.Fatal("expected AutoConnect=true on fake")
	}
}

func TestDroppedClosesOnDeviceDisconnectSignal(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin), rssi: -55}
	bus.deviceVisible = true
	bus.servicesResolved = true
	bus.gattReady = true

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	cc, err := connect(ctx, bus, "hci0", vin, &ScanResult{Path: bus.dev.path, HasRSSI: true, RSSI: -55})
	if err != nil {
		t.Fatalf("connect: %v", err)
	}
	c := cc.(*Connection)
	defer c.Close()

	bus.sig <- &dbus.Signal{
		Name: propsIface + ".PropertiesChanged",
		Path: bus.devPath(),
		Body: []interface{}{
			deviceIface,
			map[string]dbus.Variant{"Connected": dbus.MakeVariant(false)},
			[]string{},
		},
	}
	select {
	case <-c.Dropped():
	case <-time.After(time.Second):
		t.Fatal("expected Dropped() after Connected=false signal")
	}
}
