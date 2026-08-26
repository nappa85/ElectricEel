package bluez

import (
	"context"
	"fmt"
	"sync"
	"time"

	"github.com/godbus/dbus"
	"github.com/teslamotors/vehicle-command/pkg/connector"
)

// Connection implements connector.Connector over a BlueZ GATT link. It
// mirrors the transport semantics of upstream *ble.Connection: outbound
// messages are framed with a 2-byte big-endian length prefix and written in
// blockLength chunks; inbound notifications are reassembled from the same
// framing and delivered as datagrams on Receive().
// InboundObserver is invoked with a copy of each reassembled RX datagram.
// It must not block: the RX loop calls it synchronously before delivering
// the same datagram to Receive(), and a slow observer would stall GATT
// reassembly (and therefore vehicle-command's dispatcher). Prefer
// non-blocking channel sends with drop-on-full.
type InboundObserver func([]byte)

type Connection struct {
	vin     string
	bus     dbusBus
	devPath dbus.ObjectPath
	svcPath dbus.ObjectPath
	txPath  dbus.ObjectPath
	rxPath  dbus.ObjectPath

	inbox       chan []byte
	blockLength int

	mu        sync.Mutex // serializes Send
	closeOnce sync.Once
	done      chan struct{}
	// loopDone is closed by rxLoop when it returns. Close() waits on it
	// before returning: rxLoop reads directly off the dbusBus's single
	// process-lifetime signal channel (shared by every Connection created
	// from the same bluez.Conn, see bluez.go's adaptConn), so without this
	// wait a caller that tears down and immediately reconnects (idle
	// timeout followed by a fresh command, or presenceLoop's arrival
	// reconnect) could start a new rxLoop that races the outgoing one for
	// that same channel, occasionally losing a signal to the goroutine
	// that's on its way out. nil (left unset) when no rxLoop was ever
	// started, e.g. connection_test.go's newTestConnection - Close() must
	// not block forever waiting for a close that will never come.
	loopDone    chan struct{}
	match       []dbus.MatchOption
	deviceMatch []dbus.MatchOption

	// observerMu protects inboundObserver. Separate from mu so the RX path
	// can fan out copies without contending with Send.
	observerMu      sync.Mutex
	inboundObserver InboundObserver

	// dropped is closed once when BlueZ reports the GATT link is gone (or
	// Close runs). Presence mode selects on Dropped() so it can rebuild the
	// authenticated session instead of sitting on a zombie Connection.
	dropped     chan struct{}
	droppedOnce sync.Once

	// RX reassembly state. Touched only by the rxLoop goroutine (the single
	// writer), so it needs no lock of its own.
	rxBuf  []byte
	lastRx time.Time
}

// SetInboundObserver registers (or clears, with nil) a tap on inbound
// datagrams. The observer receives a private copy of each reassembled
// message; Receive() still gets its own copy, so vehicle-command continues
// to see every reply. Used by presence-mode passive-entry handling to
// inspect unsolicited VCSEC AuthenticationRequest messages that the
// upstream dispatcher would otherwise drop (no matching request handler).
func (c *Connection) SetInboundObserver(obs InboundObserver) {
	c.observerMu.Lock()
	c.inboundObserver = obs
	c.observerMu.Unlock()
}

func (c *Connection) Receive() <-chan []byte {
	return c.inbox
}

func (c *Connection) VIN() string {
	return c.vin
}

// DevicePath returns the org.bluez Device1 object path for this link.
func (c *Connection) DevicePath() dbus.ObjectPath {
	return c.devPath
}

// Dropped is closed when BlueZ reports Connected=false or Close runs.
func (c *Connection) Dropped() <-chan struct{} {
	return c.dropped
}

func (c *Connection) notifyDropped() {
	if c.dropped == nil {
		return
	}
	c.droppedOnce.Do(func() { close(c.dropped) })
}

// DeviceConnected reports whether BlueZ still considers the GATT link up.
func (c *Connection) DeviceConnected(ctx context.Context) (bool, error) {
	v, err := c.bus.object(bluezService, c.devPath).getProp(ctx, deviceIface, "Connected")
	if err != nil {
		return false, err
	}
	connected, ok := variantBool(v)
	if !ok {
		return false, fmt.Errorf("bluez: decode device Connected: got %T", v.Value())
	}
	return connected, nil
}

// SetTrusted marks the vehicle as a trusted BlueZ device so reconnects do
// not require interactive pairing prompts.
func (c *Connection) SetTrusted(trusted bool) error {
	return c.bus.object(bluezService, c.devPath).setProp(context.Background(), deviceIface, "Trusted", trusted)
}

// SetAutoConnect asks bluetoothd to reconnect when the vehicle advertises.
// Uses Properties.Set (Device1 has no SetProperty method). Presence mode
// still watches Dropped() and re-runs StartSession after a reconnect.
func (c *Connection) SetAutoConnect(enabled bool) error {
	return c.bus.object(bluezService, c.devPath).setProp(context.Background(), deviceIface, "AutoConnect", enabled)
}

func (c *Connection) PreferredAuthMethod() connector.AuthMethod {
	return connector.AuthMethodGCM
}

func (c *Connection) RetryInterval() time.Duration {
	return time.Second
}

func (c *Connection) AllowedLatency() time.Duration {
	return 4 * time.Second
}

// Send frames buffer with a 2-byte big-endian length prefix and writes it in
// blockLength-sized chunks (mirroring upstream ble.go Send). If a chunk write
// fails while blockLength is still at the assumed maximum MTU, blockLength is
// shrunk to the guaranteed minimum (ATT MTU 23 - 3) and the chunk is retried
// once. Thread-safe.
func (c *Connection) Send(_ context.Context, buffer []byte) error {
	c.mu.Lock()
	defer c.mu.Unlock()

	out := make([]byte, 0, len(buffer)+2)
	out = append(out, byte(len(buffer)>>8), byte(len(buffer)))
	out = append(out, buffer...)

	for len(out) > 0 {
		blk := len(out)
		if c.blockLength < blk {
			blk = c.blockLength
		}
		if err := c.writeChunk(out[:blk]); err != nil {
			if c.blockLength <= defaultMTU-3 {
				return err
			}
			// The remote may have negotiated a smaller ATT MTU than the
			// assumed maximum; fall back to the guaranteed chunk size and
			// retry (bounded: the next iteration either succeeds at 20-byte
			// chunks or returns err).
			c.blockLength = defaultMTU - 3
			continue
		}
		out = out[blk:]
	}
	return nil
}

// writeChunk calls org.bluez.GattCharacteristic1.WriteValue(ay value, a{sv}
// options) - options is a real D-Bus dict, not a bare string; passing a
// plain string as the second argument (as this used to) produces a
// signature mismatch ("ays" vs the real "aya{sv}") that only a live BlueZ
// rejects, since the test fake never validated the argument's D-Bus type.
// "type": "request" asks for a write-with-response, matching upstream
// ble.go's use of WithResponse writes.
func (c *Connection) writeChunk(b []byte) error {
	options := map[string]dbus.Variant{"type": dbus.MakeVariant("request")}
	_, err := c.bus.object(bluezService, c.txPath).call(context.Background(), gattChrIface+".WriteValue", b, options)
	return err
}

// Close tears down the RX subscription and the device link. Idempotent.
// Blocks until rxLoop has actually exited (see loopDone's doc comment) so a
// caller that reconnects right after Close() returns can't race the old
// rxLoop for the shared signal channel.
func (c *Connection) Close() {
	c.closeOnce.Do(func() {
		c.notifyDropped()
		close(c.done)
		if c.loopDone != nil {
			<-c.loopDone
		}
		ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		defer cancel()
		// Best-effort: the link is going away regardless.
		_, _ = c.bus.object(bluezService, c.rxPath).call(ctx, gattChrIface+".StopNotify")
		_, _ = c.bus.object(bluezService, c.devPath).call(ctx, deviceIface+".Disconnect")
		_ = c.bus.removeMatch(c.match...)
		if len(c.deviceMatch) > 0 {
			_ = c.bus.removeMatch(c.deviceMatch...)
		}
	})
}

// rxLoop dispatches incoming D-Bus signals until the connection is closed.
func (c *Connection) rxLoop() {
	defer close(c.loopDone)
	for {
		select {
		case <-c.done:
			return
		case sig, ok := <-c.bus.signals():
			if !ok {
				return
			}
			c.handleSignal(sig)
		}
	}
}

func (c *Connection) handleSignal(sig *dbus.Signal) {
	if sig.Path == c.devPath {
		c.handleDeviceSignal(sig)
		return
	}
	if sig.Path != c.rxPath {
		return
	}
	if len(sig.Body) < 2 {
		return
	}
	iface, ok := sig.Body[0].(string)
	if !ok || iface != gattChrIface {
		return
	}
	changed, ok := sig.Body[1].(map[string]dbus.Variant)
	if !ok {
		return
	}
	v, ok := changed["Value"]
	if !ok {
		return
	}
	b, ok := variantBytes(v)
	if !ok {
		return
	}
	c.rx(b)
}

func (c *Connection) handleDeviceSignal(sig *dbus.Signal) {
	if len(sig.Body) < 2 {
		return
	}
	iface, ok := sig.Body[0].(string)
	if !ok || iface != deviceIface {
		return
	}
	changed, ok := sig.Body[1].(map[string]dbus.Variant)
	if !ok {
		return
	}
	v, ok := changed["Connected"]
	if !ok {
		return
	}
	connected, ok := variantBool(v)
	if ok && !connected {
		c.notifyDropped()
	}
}

// rx appends inbound bytes to the reassembly buffer and flushes any complete
// length-prefixed messages. A gap longer than rxTimeout resets the buffer,
// mirroring upstream ble.rx.
func (c *Connection) rx(p []byte) {
	if time.Since(c.lastRx) > rxTimeout {
		c.rxBuf = nil
	}
	c.lastRx = time.Now()
	c.rxBuf = append(c.rxBuf, p...)
	for c.flush() {
	}
}

func (c *Connection) flush() bool {
	if len(c.rxBuf) < 2 {
		return false
	}
	msgLength := 256*int(c.rxBuf[0]) + int(c.rxBuf[1])
	if msgLength > maxMessageSize {
		// Malformed length; reset reassembly (upstream ble.go behavior).
		c.rxBuf = nil
		return false
	}
	if len(c.rxBuf) < 2+msgLength {
		return false
	}
	// Copy the datagram out of the backing array: subsequent appends to
	// c.rxBuf may reuse the array and would otherwise corrupt the slice we
	// hand to the consumer.
	buf := make([]byte, msgLength)
	copy(buf, c.rxBuf[2:2+msgLength])
	c.observerMu.Lock()
	obs := c.inboundObserver
	c.observerMu.Unlock()
	if obs != nil {
		// Private copy so an observer that retains the slice cannot corrupt
		// the inbox payload (or vice versa).
		obsCopy := make([]byte, msgLength)
		copy(obsCopy, buf)
		obs(obsCopy)
	}
	select {
	case c.inbox <- buf:
	default:
		// Consumer behind; drop rather than block the RX loop.
	}
	c.rxBuf = c.rxBuf[2+msgLength:]
	return true
}
