package bluez

import (
	"context"
	"errors"
	"fmt"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/godbus/dbus"
	"github.com/teslamotors/vehicle-command/pkg/connector/ble"
)

// pollInterval is how often one-shot scans and GATT setup re-read BlueZ
// state. The long-running phone-key watcher uses D-Bus signals instead, so
// an absent vehicle does not wake the process ten times per second.
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
		// A cached Device1 without a live RSSI is leftover after ads stop.
		// Device.Connect to that object is the 2026-09-09 morning hang:
		// bluetoothd sits on Connect until our deadline, then
		// GetManagedObjects itself times out.
		if result != nil && result.HasRSSI {
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
	devicePath  dbus.ObjectPath
	mu          sync.Mutex
	paused      bool
	sigs        <-chan *dbus.Signal
	detachSigs  func()
	ifaceMatch  []dbus.MatchOption
	propsMatch  []dbus.MatchOption
}

// newWatcher subscribes before starting discovery so no first advertisement
// can be missed, then seeds its target from one object-tree snapshot. Callers
// must call Stop when done to remove the matches and turn discovery back off.
func newWatcher(ctx context.Context, bus dbusBus, adapterID, vin string) (*Watcher, error) {
	adapterPath, err := findAdapter(ctx, bus, adapterID)
	if err != nil {
		return nil, err
	}
	if err := ensurePowered(ctx, bus, adapterPath); err != nil {
		return nil, err
	}
	_ = setDiscoveryFilter(ctx, bus, adapterPath)
	// Own listener: Connection.rxLoop already owns bus.signals(). Sharing
	// that channel made Wait() steal GATT notifications (and vice versa).
	sigs, detachSigs := bus.attachSignals()
	ifaceMatch := []dbus.MatchOption{
		dbus.WithMatchSender(bluezService),
		dbus.WithMatchInterface(objMgrIface),
		dbus.WithMatchMember("InterfacesAdded"),
		dbus.WithMatchObjectPath("/"),
	}
	propsMatch := []dbus.MatchOption{
		dbus.WithMatchSender(bluezService),
		dbus.WithMatchInterface(propsIface),
		dbus.WithMatchMember("PropertiesChanged"),
		dbus.WithMatchPathNamespace(adapterPath),
	}
	if err := bus.addMatch(ifaceMatch...); err != nil {
		detachSigs()
		return nil, fmt.Errorf("bluez: subscribe to discovered devices: %w", err)
	}
	if err := bus.addMatch(propsMatch...); err != nil {
		_ = bus.removeMatch(ifaceMatch...)
		detachSigs()
		return nil, fmt.Errorf("bluez: subscribe to advertisement updates: %w", err)
	}
	if err := startDiscovery(ctx, bus, adapterPath); err != nil {
		_ = bus.removeMatch(propsMatch...)
		_ = bus.removeMatch(ifaceMatch...)
		detachSigs()
		return nil, fmt.Errorf("bluez: start discovery: %w", err)
	}
	w := &Watcher{
		bus:         bus,
		adapterPath: adapterPath,
		name:        vehicleBeaconName(vin),
		sigs:        sigs,
		detachSigs:  detachSigs,
		ifaceMatch:  ifaceMatch,
		propsMatch:  propsMatch,
	}
	initial, err := findBeacon(ctx, bus, adapterPath, w.name)
	if err != nil {
		w.Stop(context.Background())
		return nil, err
	}
	if initial != nil {
		// Remember the path so later RSSI-only signals match, but do not
		// treat a leftover RSSI as live. BlueZ keeps Device1 objects (and
		// often the last RSSI) after ads stop; connecting to that cache
		// is what made GATT time out while the car stayed disconnected.
		w.devicePath = initial.Path
	}
	return w, nil
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

// Wait blocks on BlueZ signals until a fresh advertisement appears or ctx is
// done. A cached Device1 RSSI is not "found": BlueZ keeps those objects after
// ads stop, and connecting to them times out GATT. Discovery is left running.
// A timeout with no live beacon is (nil, nil), not an error.
func (w *Watcher) Wait(ctx context.Context) (*ScanResult, error) {
	if err := w.ensureDiscovering(ctx); err != nil {
		return nil, err
	}
	for {
		select {
		case <-ctx.Done():
			return nil, nil
		case sig := <-w.sigs:
			result, restart, ok := w.handleSignal(ctx, sig)
			if restart {
				if err := w.ensureDiscovering(ctx); err != nil {
					return nil, err
				}
			}
			if ok {
				return result, nil
			}
		}
	}
}

// handleSignal extracts a fresh target advertisement and notices when BlueZ
// drops discovery. Its booleans are restartDiscovery and resultAvailable.
func (w *Watcher) handleSignal(ctx context.Context, sig *dbus.Signal) (*ScanResult, bool, bool) {
	if sig == nil {
		return nil, false, false
	}
	switch sig.Name {
	case objMgrIface + ".InterfacesAdded":
		if len(sig.Body) < 2 {
			return nil, false, false
		}
		path, ok := sig.Body[0].(dbus.ObjectPath)
		if !ok || !strings.HasPrefix(string(path), string(w.adapterPath)+"/dev_") {
			return nil, false, false
		}
		props, ok := devicePropsFromInterfaces(sig.Body[1])
		if !ok {
			return nil, false, false
		}
		return w.resultFromDeviceProperties(ctx, path, props)

	case propsIface + ".PropertiesChanged":
		if len(sig.Body) < 2 {
			return nil, false, false
		}
		iface, ok := sig.Body[0].(string)
		if !ok {
			return nil, false, false
		}
		changed, ok := sig.Body[1].(map[string]dbus.Variant)
		if !ok {
			return nil, false, false
		}
		if iface == adapterIface && sig.Path == w.adapterPath {
			if discovering, ok := variantBool(changed["Discovering"]); ok && !discovering {
				return nil, true, false
			}
			return nil, false, false
		}
		if iface != deviceIface {
			return nil, false, false
		}
		return w.resultFromDeviceProperties(ctx, sig.Path, changed)
	}
	return nil, false, false
}

func devicePropsFromInterfaces(v interface{}) (map[string]dbus.Variant, bool) {
	ifaces, ok := v.(map[string]map[string]dbus.Variant)
	if !ok {
		return nil, false
	}
	props, ok := ifaces[deviceIface]
	return props, ok
}

func deviceAdvertisedName(props map[string]dbus.Variant, want string) (string, bool) {
	name, hasName := variantString(props["Name"])
	alias, hasAlias := variantString(props["Alias"])
	// Prefer the field that matches the Tesla beacon. BlueZ often leaves
	// Name as the MAC while the advertising local name is only in Alias.
	switch {
	case hasName && name == want:
		return name, true
	case hasAlias && alias == want:
		return alias, true
	case hasName && name != "":
		return name, true
	case hasAlias && alias != "":
		return alias, true
	}
	return "", false
}

func (w *Watcher) resultFromDeviceProperties(ctx context.Context, path dbus.ObjectPath, props map[string]dbus.Variant) (*ScanResult, bool, bool) {
	name, hasName := deviceAdvertisedName(props, w.name)
	switch {
	case hasName && name != w.name:
		if path == w.devicePath {
			w.devicePath = ""
		}
		return nil, false, false
	case hasName:
		w.devicePath = path
	case path != w.devicePath:
		return nil, false, false
	}

	rssi, hasRSSI := variantInt16(props["RSSI"])
	if !hasRSSI && isAdvertisementUpdate(props) {
		// DuplicateData updates frequently carry ManufacturerData /
		// ServiceData without RSSI in the same dict. Only fetch RSSI
		// for those; Trusted/Connected noise must not revive a stale cache.
		rssi, hasRSSI = w.readRSSI(ctx, path)
	}
	if !hasRSSI {
		return nil, false, false
	}
	if !hasName {
		name = w.name
	}
	return &ScanResult{Path: path, LocalName: name, RSSI: rssi, HasRSSI: true}, false, true
}

func (w *Watcher) readRSSI(ctx context.Context, path dbus.ObjectPath) (int16, bool) {
	v, err := w.bus.object(bluezService, path).getProp(ctx, deviceIface, "RSSI")
	if err != nil {
		return 0, false
	}
	return variantInt16(v)
}

func isAdvertisementUpdate(props map[string]dbus.Variant) bool {
	if _, ok := variantInt16(props["RSSI"]); ok {
		return true
	}
	for _, key := range []string{"ManufacturerData", "ServiceData", "AdvertisingFlags", "TxPower"} {
		if _, ok := props[key]; ok {
			return true
		}
	}
	return false
}

// Pause stops LE discovery so Device.Connect is not aborted by a live
// scan. Resume lets the next Wait/Peek start discovery again.
func (w *Watcher) Pause() {
	w.mu.Lock()
	w.paused = true
	w.mu.Unlock()
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	stopDiscovery(ctx, w.bus, w.adapterPath)
}

func (w *Watcher) Resume() {
	w.mu.Lock()
	wasPaused := w.paused
	w.paused = false
	w.mu.Unlock()
	if wasPaused {
		// PropertiesChanged from the previous GATT session (including a
		// leftover RSSI) sits on this buffer and would look like a live
		// arrival the moment Wait runs after a drop.
		drainSignalChan(w.sigs)
	}
}

// ensureDiscovering powers the adapter and starts LE discovery if BlueZ
// is not already scanning. Safe to call on every Peek: a live discovery
// session is a no-op.
func (w *Watcher) ensureDiscovering(ctx context.Context) error {
	w.mu.Lock()
	paused := w.paused
	w.mu.Unlock()
	if paused {
		return nil
	}
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

// Stop removes this watcher's signal subscriptions and turns discovery off.
// Safe to call once; a Peek after Stop simply stops seeing new devices as
// BlueZ's cache goes stale.
func (w *Watcher) Stop(ctx context.Context) {
	_ = w.bus.removeMatch(w.propsMatch...)
	_ = w.bus.removeMatch(w.ifaceMatch...)
	if w.detachSigs != nil {
		w.detachSigs()
		w.detachSigs = nil
	}
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

// DBusDetail is the exported form of dbusDetail for session-level logs.
func DBusDetail(err error) string {
	return dbusDetail(err)
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
		devName, ok := deviceAdvertisedName(dev, name)
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
