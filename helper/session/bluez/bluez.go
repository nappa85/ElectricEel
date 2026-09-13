// Package bluez provides a connector.Connector for Tesla vehicles that
// talks to the vehicle over BlueZ's org.bluez D-Bus interface instead of
// go-ble's raw HCI channel.
//
// The raw HCI approach requires exclusive, whole-adapter control: go-ble's
// socket setup brings the controller DOWN and binds an HCI user channel
// (HCI_CHANNEL_USER), which forcibly drops every other Bluetooth connection
// on the phone - e.g. an A2DP audio stream to a soundbar. Routing through
// org.bluez keeps the radio owned by the OS Bluetooth stack, so existing
// connections are never disturbed.
//
// This package reimplements only the transport, mirroring the behavior of
// upstream pkg/connector/ble (same 2-byte length-prefixed message framing,
// same blockLength chunking, same error/latency semantics). Everything above
// the transport (session crypto, command dispatch) is untouched and comes
// from the vendored upstream code.
package bluez

import (
	"context"
	"strings"
	"time"

	"github.com/godbus/dbus"
	"github.com/teslamotors/vehicle-command/pkg/connector"
)

// D-Bus interface/object names for the org.bluez service. Collected here so
// the transport logic and the tests reference a single set of names.
const (
	bluezService = "org.bluez"

	objMgrIface = "org.freedesktop.DBus.ObjectManager"
	propsIface  = "org.freedesktop.DBus.Properties"

	adapterIface = "org.bluez.Adapter1"
	deviceIface  = "org.bluez.Device1"
	gattSvcIface = "org.bluez.GattService1"
	gattChrIface = "org.bluez.GattCharacteristic1"
)

// Tesla GATT service/characteristic UUIDs in BlueZ's on-the-wire lowercase,
// dash-stripped form.
const (
	vehicleServiceUUID = "00000211b2d143f09b88960cebf8b91e"
	toVehicleUUID      = "00000212b2d143f09b88960cebf8b91e"
	fromVehicleUUID    = "00000213b2d143f09b88960cebf8b91e"
)

// Transport constants mirroring upstream pkg/connector/ble:
//   - maxMessageSize is the cap on a single command message (upstream
//     maxBLEMessageSize).
//   - maxExpectedMTU is go-ble's MaxMTU (512 + 3 bytes of ATT header); Linux
//     BlueZ negotiates at least this, so writes are chunked at MTU-3.
//   - defaultMTU is go-ble's DefaultMTU; 23 is the minimum ATT MTU every
//     device supports, so MTU-3 = 20 bytes is the guaranteed-safe fallback
//     chunk size when the assumed MTU turns out to be too large.
const (
	maxMessageSize = 1024
	maxExpectedMTU = 512 + 3
	defaultMTU     = 23
)

// rxTimeout is the gap between received chunks that resets the RX
// reassembly buffer (upstream ble.rxTimeout).
const rxTimeout = time.Second

// dbusBus is the slice of the D-Bus connection this package uses. It exists
// so tests can substitute a fake that answers method calls and delivers
// signals without a real system bus.
type dbusBus interface {
	// object returns a handle for method calls/property access on dest/path.
	object(dest string, path dbus.ObjectPath) dbusCaller
	// signals returns the process-lifetime channel used by Connection.rxLoop.
	signals() <-chan *dbus.Signal
	// attachSignals registers an extra godbus listener so the phone-key
	// Watcher can receive advertisements without stealing GATT notifications
	// from signals(). The returned func unregisters that listener.
	attachSignals() (<-chan *dbus.Signal, func())
	// addMatch/removeMatch register/unregister a signal match rule.
	addMatch(options ...dbus.MatchOption) error
	removeMatch(options ...dbus.MatchOption) error
}

// dbusCaller is the slice of a D-Bus object this package uses.
type dbusCaller interface {
	// call invokes a method and returns the response body.
	call(ctx context.Context, method string, args ...interface{}) ([]interface{}, error)
	// getProp/setProp read/write a property via org.freedesktop.DBus.Properties.
	getProp(ctx context.Context, iface, prop string) (dbus.Variant, error)
	setProp(ctx context.Context, iface, prop string, value interface{}) error
}

// variant decoders. This repo's pinned godbus version predates
// Variant.Store, so property values are decoded from Variant.Value directly.
// The ok-returning helpers keep each decode site a one-liner.
func variantBool(v dbus.Variant) (bool, bool) {
	b, ok := v.Value().(bool)
	return b, ok
}

func variantString(v dbus.Variant) (string, bool) {
	s, ok := v.Value().(string)
	return s, ok
}

func variantBytes(v dbus.Variant) ([]byte, bool) {
	b, ok := v.Value().([]byte)
	return b, ok
}

// variantInt16 tolerates the handful of int widths a D-Bus int16 may decode
// to across implementations.
func variantInt16(v dbus.Variant) (int16, bool) {
	switch n := v.Value().(type) {
	case int16:
		return n, true
	case int32:
		return int16(n), true
	case int:
		return int16(n), true
	}
	return 0, false
}

// godbusConn adapts a *dbus.Conn to dbusBus.
type godbusConn struct {
	c   *dbus.Conn
	sig chan *dbus.Signal
}

func adaptConn(conn *dbus.Conn) *godbusConn {
	sig := make(chan *dbus.Signal, 64)
	conn.Signal(sig)
	return &godbusConn{c: conn, sig: sig}
}

func (g *godbusConn) object(dest string, path dbus.ObjectPath) dbusCaller {
	return &godbusObject{obj: g.c.Object(dest, path)}
}

func (g *godbusConn) signals() <-chan *dbus.Signal { return g.sig }

func (g *godbusConn) attachSignals() (<-chan *dbus.Signal, func()) {
	// godbus broadcasts each incoming signal to every registered channel,
	// so this listener is independent of signals() / rxLoop.
	ch := make(chan *dbus.Signal, 64)
	g.c.Signal(ch)
	return ch, func() { g.c.RemoveSignal(ch) }
}

func (g *godbusConn) addMatch(options ...dbus.MatchOption) error {
	return g.c.AddMatchSignal(options...)
}

func (g *godbusConn) removeMatch(options ...dbus.MatchOption) error {
	return g.c.RemoveMatchSignal(options...)
}

// godbusObject adapts a *dbus.Object to dbusCaller.
type godbusObject struct {
	obj dbus.BusObject
}

func (o *godbusObject) call(ctx context.Context, method string, args ...interface{}) ([]interface{}, error) {
	call := o.obj.CallWithContext(ctx, method, 0, args...)
	if call.Err != nil {
		return nil, call.Err
	}
	return call.Body, nil
}

func (o *godbusObject) getProp(ctx context.Context, iface, prop string) (dbus.Variant, error) {
	body, err := o.call(ctx, propsIface+".Get", iface, prop)
	if err != nil {
		return dbus.Variant{}, err
	}
	if len(body) != 1 {
		return dbus.Variant{}, &dbus.Error{Name: "bluez", Body: []interface{}{"unexpected Properties.Get reply"}}
	}
	v, ok := body[0].(dbus.Variant)
	if !ok {
		return dbus.Variant{}, &dbus.Error{Name: "bluez", Body: []interface{}{"unexpected Properties.Get reply type"}}
	}
	return v, nil
}

func (o *godbusObject) setProp(ctx context.Context, iface, prop string, value interface{}) error {
	_, err := o.call(ctx, propsIface+".Set", iface, prop, dbus.MakeVariant(value))
	return err
}

// Conn is a handle to a system-bus connection prepared for org.bluez calls.
// It shares one D-Bus signal registration across Scan/Connect, so callers
// should create one Conn and reuse it for the lifetime of the app.
type Conn struct {
	raw *dbus.Conn
	bus dbusBus
}

// Open establishes a system-bus connection for BlueZ access.
func Open() (*Conn, error) {
	raw, err := dbus.SystemBus()
	if err != nil {
		return nil, err
	}
	return &Conn{raw: raw, bus: adaptConn(raw)}, nil
}

// Scan scans for the vehicle's BLE beacon and returns as soon as a matching
// device appears (or ctx is cancelled).
func (c *Conn) Scan(ctx context.Context, adapterID, vin string) (*ScanResult, error) {
	return scan(ctx, c.bus, adapterID, vin)
}

// Watch starts BLE discovery and returns a Watcher for repeated,
// non-blocking beacon snapshots - the primitive a presence-maintenance loop
// polls on its own schedule, as opposed to Scan's block-until-found shape.
func (c *Conn) Watch(ctx context.Context, adapterID, vin string) (*Watcher, error) {
	return newWatcher(ctx, c.bus, adapterID, vin)
}

// Connect connects to the vehicle. A live target (HasRSSI) is Connected
// directly, matching Tesla Android's reconnect-to-known-MAC. A stale
// Device1 without RSSI is forgotten first so bluetoothd does not hang.
func (c *Conn) Connect(ctx context.Context, adapterID, vin string, target *ScanResult) (connector.Connector, error) {
	return connect(ctx, c.bus, adapterID, vin, target)
}

// Close releases the system-bus connection.
func (c *Conn) Close() error {
	return c.raw.Close()
}

// IsAdapterError reports whether err is a permanent adapter/radio-level
// failure rather than a transient link or scan condition. It mirrors
// upstream pkg/connector/ble.IsAdapterError for the D-Bus path: a missing
// controller, a controller the OS refuses to use, or org.bluez itself being
// unavailable are all conditions retrying will not fix, so callers should
// surface them immediately instead of burning the remaining deadline.
func IsAdapterError(err error) bool {
	if err == nil {
		return false
	}
	s := err.Error()
	switch {
	case strings.Contains(s, "operation not permitted"):
		return true
	case strings.Contains(s, "org.bluez.Error.NotReady"):
		return true
	case strings.Contains(s, "org.bluez.Error.NotPowered"):
		return true
	case strings.Contains(s, "org.freedesktop.DBus.Error.ServiceUnknown"):
		return true
	case strings.Contains(s, "no Bluetooth adapter found"):
		return true
	}
	return false
}
