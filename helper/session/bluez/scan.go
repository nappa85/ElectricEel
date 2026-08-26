package bluez

import (
	"context"
	"errors"
	"fmt"
	"sort"
	"strings"
	"time"

	"github.com/godbus/dbus"
	"github.com/teslamotors/vehicle-command/pkg/connector/ble"
)

// pollInterval is how often GetManagedObjects is re-read while waiting for a
// beacon. The car advertises every 20-150ms, so a 100ms poll notices it
// within a connect timeout. Polling (rather than subscribing to
// org.bluez signals) keeps the scan loop deterministic and trivially
// testable; the signal-based path is only required later, for GATT
// notifications, where events cannot be polled.
const pollInterval = 100 * time.Millisecond

// ScanResult describes a discovered vehicle beacon. Path is the org.bluez
// Device1 object path (the identifier Connect needs).
type ScanResult struct {
	Path      dbus.ObjectPath
	LocalName string
	RSSI      int16
	// HasRSSI is true when BlueZ reported an RSSI property on this snapshot.
	// Cached Device1 objects linger after the vehicle stops advertising, but
	// without a fresh RSSI; presence polling must not treat those as live.
	HasRSSI bool
}

// vehicleBeaconName returns the advertising local name the vehicle exposes
// for a given VIN. It reuses upstream's VehicleLocalName so the name format
// can never drift from the upstream implementation.
func vehicleBeaconName(vin string) string {
	return ble.VehicleLocalName(vin)
}

// scan finds the vehicle's beacon. If this call started discovery, it
// stops it on the way out. If another caller (presenceLoop's Watcher)
// already had discovery open, that session is left running - a dashboard
// refresh must not tear down the phone-key scanner.
func scan(ctx context.Context, bus dbusBus, adapterID, vin string) (*ScanResult, error) {
	name := vehicleBeaconName(vin)

	adapterPath, err := findAdapter(ctx, bus, adapterID)
	if err != nil {
		return nil, err
	}
	if err := ensurePowered(ctx, bus, adapterPath); err != nil {
		return nil, err
	}
	// Best-effort filter: restricting discovery to LE keeps the scan on the
	// car's PHY and avoids classic (BR/EDR) traffic. Failure is tolerated
	// because an unfiltered scan still finds the car.
	_ = setDiscoveryFilter(ctx, bus, adapterPath)

	already, _ := adapterIsDiscovering(ctx, bus, adapterPath)
	if err := startDiscovery(ctx, bus, adapterPath); err != nil {
		return nil, fmt.Errorf("bluez: start discovery: %w", err)
	}
	if !already {
		defer stopDiscovery(ctx, bus, adapterPath)
	}

	for {
		result, err := findBeacon(ctx, bus, adapterPath, name)
		if err != nil {
			return nil, err
		}
		if result != nil {
			return result, nil
		}
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-time.After(pollInterval):
		}
	}
}

// Watcher keeps BlueZ discovery running so Peek can be called repeatedly
// without scan()'s per-call Start/StopDiscovery churn - the shape a
// presence-maintenance loop needs (poll every couple seconds for as long as
// it runs), as opposed to scan()'s "block until found once" shape.
type Watcher struct {
	bus         dbusBus
	adapterPath dbus.ObjectPath
	name        string
}

// newWatcher starts discovery on adapterID (or the first available adapter)
// and returns a Watcher ready for repeated Peek calls. Callers must call
// Stop when done to turn discovery back off.
func newWatcher(ctx context.Context, bus dbusBus, adapterID, vin string) (*Watcher, error) {
	adapterPath, err := findAdapter(ctx, bus, adapterID)
	if err != nil {
		return nil, err
	}
	if err := ensurePowered(ctx, bus, adapterPath); err != nil {
		return nil, err
	}
	_ = setDiscoveryFilter(ctx, bus, adapterPath)
	if err := startDiscovery(ctx, bus, adapterPath); err != nil {
		return nil, fmt.Errorf("bluez: start discovery: %w", err)
	}
	return &Watcher{bus: bus, adapterPath: adapterPath, name: vehicleBeaconName(vin)}, nil
}

// Peek returns the vehicle's current beacon snapshot, or (nil, nil) if it
// isn't visible right now. Unlike Scan, it never blocks waiting for the
// beacon to appear - callers poll it on their own schedule.
//
// Each Peek re-asserts that the adapter is powered and still discovering.
// Sailfish bluetoothd often drops Discovering after a timeout or while the
// radio idles; without this the Watcher would keep polling a dead scan
// until a dashboard refresh's scan() woke it up.
func (w *Watcher) Peek(ctx context.Context) (*ScanResult, error) {
	if err := w.ensureDiscovering(ctx); err != nil {
		return nil, err
	}
	return findBeacon(ctx, w.bus, w.adapterPath, w.name)
}

// Wait polls until the vehicle beacon is visible or ctx is done. Unlike
// Peek (one snapshot) this is what a dashboard refresh's scan() does:
// keep looking until the advertisement shows up. Discovery is left
// running. A timeout with no beacon is (nil, nil), not an error.
func (w *Watcher) Wait(ctx context.Context) (*ScanResult, error) {
	for {
		result, err := w.Peek(ctx)
		if err != nil {
			return nil, err
		}
		if result != nil {
			return result, nil
		}
		select {
		case <-ctx.Done():
			return nil, nil
		case <-time.After(pollInterval):
		}
	}
}

// ensureDiscovering powers the adapter and starts LE discovery if BlueZ
// is not already scanning. Safe to call on every Peek: a live discovery
// session is a no-op.
func (w *Watcher) ensureDiscovering(ctx context.Context) error {
	if err := ensurePowered(ctx, w.bus, w.adapterPath); err != nil {
		return err
	}
	discovering, err := adapterIsDiscovering(ctx, w.bus, w.adapterPath)
	if err == nil && discovering {
		return nil
	}
	_ = setDiscoveryFilter(ctx, w.bus, w.adapterPath)
	return startDiscovery(ctx, w.bus, w.adapterPath)
}

// Stop turns discovery back off. Safe to call once; a Peek after Stop simply
// stops seeing new devices as BlueZ's cache goes stale.
func (w *Watcher) Stop(ctx context.Context) {
	stopDiscovery(ctx, w.bus, w.adapterPath)
}

// managedObjects returns BlueZ's full object tree keyed by object path.
func managedObjects(ctx context.Context, bus dbusBus) (map[dbus.ObjectPath]map[string]map[string]dbus.Variant, error) {
	body, err := bus.object(bluezService, "/").call(ctx, objMgrIface+".GetManagedObjects")
	if err != nil {
		return nil, fmt.Errorf("bluez: enumerate objects: %w", err)
	}
	if len(body) != 1 {
		return nil, errors.New("bluez: unexpected GetManagedObjects reply")
	}
	m, ok := body[0].(map[dbus.ObjectPath]map[string]map[string]dbus.Variant)
	if !ok {
		return nil, fmt.Errorf("bluez: unexpected GetManagedObjects reply type %T", body[0])
	}
	return m, nil
}

// findAdapter locates the org.bluez Adapter1 object. If adapterID names a
// specific controller ("hci0", ...), that one is required; otherwise a
// powered adapter is preferred (stable path order). Picking an unpowered
// extra Adapter1 at random made Watch fail with "power on adapter" while
// a later refresh happened to land on hci0.
func findAdapter(ctx context.Context, bus dbusBus, adapterID string) (dbus.ObjectPath, error) {
	objects, err := managedObjects(ctx, bus)
	if err != nil {
		return "", err
	}
	var powered, unpowered []dbus.ObjectPath
	for path, ifaces := range objects {
		if _, ok := ifaces[adapterIface]; !ok {
			continue
		}
		base := strings.TrimPrefix(string(path), "/org/bluez/")
		if adapterID != "" {
			if base == adapterID {
				return path, nil
			}
			continue
		}
		if adapterPoweredIn(ifaces) {
			powered = append(powered, path)
		} else {
			unpowered = append(unpowered, path)
		}
	}
	sort.Slice(powered, func(i, j int) bool { return powered[i] < powered[j] })
	sort.Slice(unpowered, func(i, j int) bool { return unpowered[i] < unpowered[j] })
	if len(powered) > 0 {
		return powered[0], nil
	}
	if len(unpowered) > 0 {
		return unpowered[0], nil
	}
	return "", fmt.Errorf("bluez: no Bluetooth adapter found (wanted %q)", adapterID)
}

func adapterPoweredIn(ifaces map[string]map[string]dbus.Variant) bool {
	props, ok := ifaces[adapterIface]
	if !ok {
		return false
	}
	powered, ok := variantBool(props["Powered"])
	return ok && powered
}

// ensurePowered turns the adapter on if it is off. Under a healthy
// bluetoothd it is already powered; this is a convenience that mirrors
// go-ble bringing the device up.
func ensurePowered(ctx context.Context, bus dbusBus, adapterPath dbus.ObjectPath) error {
	obj := bus.object(bluezService, adapterPath)
	v, err := obj.getProp(ctx, adapterIface, "Powered")
	if err != nil {
		return fmt.Errorf("bluez: read adapter Powered: %w", err)
	}
	powered, ok := variantBool(v)
	if !ok {
		return fmt.Errorf("bluez: decode adapter Powered: got %T", v.Value())
	}
	if powered {
		return nil
	}
	if err := obj.setProp(ctx, adapterIface, "Powered", true); err != nil {
		// Sailfish often denies Adapter1.Powered writes (ConnMan owns
		// radio power). The D-Bus error body is frequently empty, which
		// used to log as "power on adapter:" with nothing after the colon.
		// Re-read: the adapter may already be coming up.
		if v2, e2 := obj.getProp(ctx, adapterIface, "Powered"); e2 == nil {
			if nowOn, ok := variantBool(v2); ok && nowOn {
				return nil
			}
		}
		return fmt.Errorf("bluez: power on adapter: %s", dbusDetail(err))
	}
	return nil
}

// dbusDetail keeps the BlueZ error name when the message body is empty.
func dbusDetail(err error) string {
	if err == nil {
		return ""
	}
	if name, msg := dbusErrorParts(err); name != "" || msg != "" {
		if name != "" && msg != "" && msg != name {
			return name + ": " + msg
		}
		if name != "" {
			return name
		}
		return msg
	}
	if s := err.Error(); s != "" {
		return s
	}
	return fmt.Sprintf("%T", err)
}

func dbusErrorParts(err error) (name, msg string) {
	var dberr dbus.Error
	if errors.As(err, &dberr) {
		return dberr.Name, dberr.Error()
	}
	var ptr *dbus.Error
	if errors.As(err, &ptr) && ptr != nil {
		return ptr.Name, ptr.Error()
	}
	return "", ""
}

func adapterIsDiscovering(ctx context.Context, bus dbusBus, adapterPath dbus.ObjectPath) (bool, error) {
	v, err := bus.object(bluezService, adapterPath).getProp(ctx, adapterIface, "Discovering")
	if err != nil {
		return false, fmt.Errorf("bluez: read adapter Discovering: %w", err)
	}
	discovering, ok := variantBool(v)
	if !ok {
		return false, fmt.Errorf("bluez: decode adapter Discovering: got %T", v.Value())
	}
	return discovering, nil
}

func setDiscoveryFilter(ctx context.Context, bus dbusBus, adapterPath dbus.ObjectPath) error {
	filter := map[string]dbus.Variant{
		"Transport": dbus.MakeVariant("le"),
		// DuplicateData=true asks BlueZ to emit RSSI updates on every
		// advertisement instead of collapsing them. Presence polling needs
		// those updates; the default (false) leaves a stale RSSI on a
		// cached device and looks like the car is still nearby.
		"DuplicateData": dbus.MakeVariant(true),
	}
	_, err := bus.object(bluezService, adapterPath).call(ctx, adapterIface+".SetDiscoveryFilter", filter)
	return err
}

func startDiscovery(ctx context.Context, bus dbusBus, adapterPath dbus.ObjectPath) error {
	_, err := bus.object(bluezService, adapterPath).call(ctx, adapterIface+".StartDiscovery")
	if err != nil && isDiscoveryInProgress(err) {
		// Another caller (typically presenceLoop's Watcher) already holds
		// discovery open. Treat as success so manual commands don't fight
		// the phone-key scanner.
		return nil
	}
	return err
}

// isDiscoveryInProgress reports whether err is BlueZ's "discovery already
// active" condition, which is benign when two code paths share one adapter.
func isDiscoveryInProgress(err error) bool {
	if err == nil {
		return false
	}
	s := err.Error()
	return strings.Contains(s, "InProgress") ||
		strings.Contains(s, "already in progress")
}

func stopDiscovery(ctx context.Context, bus dbusBus, adapterPath dbus.ObjectPath) {
	_, _ = bus.object(bluezService, adapterPath).call(ctx, adapterIface+".StopDiscovery")
}

// findBeacon inspects the current object tree for a device advertising the
// target local name. Returns (nil, nil) when no match is present yet.
func findBeacon(ctx context.Context, bus dbusBus, adapterPath dbus.ObjectPath, name string) (*ScanResult, error) {
	objects, err := managedObjects(ctx, bus)
	if err != nil {
		return nil, err
	}
	prefix := string(adapterPath) + "/dev_"
	for path, ifaces := range objects {
		dev, ok := ifaces[deviceIface]
		if !ok || !strings.HasPrefix(string(path), prefix) {
			continue
		}
		devName, ok := variantString(dev["Name"])
		if !ok || devName != name {
			continue
		}
		result := &ScanResult{Path: path, LocalName: devName}
		if rssi, ok := variantInt16(dev["RSSI"]); ok {
			result.RSSI = rssi
			result.HasRSSI = true
		}
		return result, nil
	}
	return nil, nil
}
