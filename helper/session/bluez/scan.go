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

// pollInterval is for one-shot scanning and GATT setup only. The continuous
// phone-key watcher waits for BlueZ events instead of polling the object tree.
const pollInterval = 100 * time.Millisecond

// AdapterOffError preserves the diagnostic while exposing a machine-readable
// radio-power failure to the parent. Callers must not parse Error() prose.
type AdapterOffError struct{ Cause error }

func (e *AdapterOffError) Error() string {
	return fmt.Sprintf("bluez: power on adapter: %s", dbusDetail(e.Cause))
}
func (e *AdapterOffError) Unwrap() error { return e.Cause }

func IsBluetoothOff(err error) bool {
	var off *AdapterOffError
	if errors.As(err, &off) {
		return true
	}
	name, _ := dbusErrorParts(err)
	return name == "org.bluez.Error.NotPowered"
}

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
	started := time.Now()
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
		diagnostic("scan start failed: %s", dbusDetail(err))
		return nil, fmt.Errorf("bluez: start discovery: %w", err)
	}
	diagnostic("scan started adapter=%s alreadyDiscovering=%v", adapterPath, already)
	if !already {
		defer func() {
			// The scan deadline may have expired; still try to release discovery.
			stopCtx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
			defer cancel()
			stopDiscovery(stopCtx, bus, adapterPath)
		}()
	}

	polls := 0
	for {
		polls++
		result, err := findBeacon(ctx, bus, adapterPath, name)
		if err != nil {
			diagnostic("scan ended error after=%s polls=%d: %v", time.Since(started).Round(time.Millisecond), polls, err)
			return nil, err
		}
		if result != nil && result.HasRSSI {
			diagnostic("scan found beacon after=%s polls=%d rssiPresent=%v rssi=%d", time.Since(started).Round(time.Millisecond), polls, result.HasRSSI, result.RSSI)
			return result, nil
		}
		if result != nil {
			diagnostic("scan ignoring stale cached device without fresh RSSI after=%s polls=%d", time.Since(started).Round(time.Millisecond), polls)
		}
		select {
		case <-ctx.Done():
			diagnostic("scan ended timeout after=%s polls=%d: %v", time.Since(started).Round(time.Millisecond), polls, ctx.Err())
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
	bus                  dbusBus
	adapterPath          dbus.ObjectPath
	name                 string
	match                []dbus.MatchOption
	ifaceMatch           []dbus.MatchOption
	signalCh             chan *dbus.Signal
	signalStop           chan struct{}
	signalDone           chan struct{}
	nearUpdate           chan struct{}
	updates              chan struct{}
	pending              *ScanResult
	pendingAt            time.Time
	paused               bool
	stopOnce             sync.Once
	discoveryMu          sync.Mutex
	signalMu             sync.Mutex
	devicePath           dbus.ObjectPath
	nearRSSI             int16
	lastRSSIUpdate       time.Time
	rssiUpdates          int
	advertisementUpdates int
	stats                WatchStats // accessed by the presence-loop goroutine only
}

// WatchStats counts D-Bus work done during the last observation window.
// RSSI presence in a Device1 snapshot is not proof of a fresh advertisement.
type WatchStats struct {
	Polls                int
	BeaconResults        int
	RSSIResults          int
	RSSIUpdates          int
	AdvertisementUpdates int
	LastRSSIUpdateAge    time.Duration // -1 means no update signal seen
	Restarts             int
	ReadTotal            time.Duration
	ReadMax              time.Duration
}

func (w *Watcher) TakeStats() WatchStats {
	stats := w.stats
	w.signalMu.Lock()
	stats.RSSIUpdates = w.rssiUpdates
	w.rssiUpdates = 0
	stats.AdvertisementUpdates = w.advertisementUpdates
	w.advertisementUpdates = 0
	stats.LastRSSIUpdateAge = -1
	if !w.lastRSSIUpdate.IsZero() {
		stats.LastRSSIUpdateAge = time.Since(w.lastRSSIUpdate).Round(time.Millisecond)
	}
	w.signalMu.Unlock()
	w.stats = WatchStats{}
	return stats
}

// SetNearRSSI configures which genuine BlueZ RSSI changes can interrupt the
// presence loop's cached-beacon delay. Snapshot polling is unchanged.
func (w *Watcher) SetNearRSSI(threshold int16) {
	w.signalMu.Lock()
	w.nearRSSI = threshold
	w.signalMu.Unlock()
}

func (w *Watcher) NearUpdates() <-chan struct{} { return w.nearUpdate }

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
	w := &Watcher{bus: bus, adapterPath: adapterPath, name: vehicleBeaconName(vin), nearRSSI: -90, nearUpdate: make(chan struct{}, 1), updates: make(chan struct{}, 1)}
	w.match = []dbus.MatchOption{
		dbus.WithMatchSender(bluezService),
		dbus.WithMatchInterface(propsIface),
		dbus.WithMatchMember("PropertiesChanged"),
		dbus.WithMatchPathNamespace(adapterPath),
	}
	if err := bus.addMatch(w.match...); err != nil {
		return nil, fmt.Errorf("bluez: subscribe to advertisements: %w", err)
	}
	w.ifaceMatch = []dbus.MatchOption{
		dbus.WithMatchSender(bluezService), dbus.WithMatchInterface(objMgrIface),
		dbus.WithMatchMember("InterfacesAdded"), dbus.WithMatchObjectPath("/"),
	}
	if err := bus.addMatch(w.ifaceMatch...); err != nil {
		_ = bus.removeMatch(w.match...)
		return nil, fmt.Errorf("bluez: subscribe to discovered devices: %w", err)
	}
	w.signalCh = make(chan *dbus.Signal, 128)
	w.signalStop = make(chan struct{})
	w.signalDone = make(chan struct{})
	bus.subscribeSignals(w.signalCh)
	// Seed identity, never liveness: BlueZ can retain RSSI after departure.
	initial, err := findBeacon(ctx, bus, adapterPath, w.name)
	if initial != nil {
		w.devicePath = initial.Path
	}
	go w.observeRSSIUpdates()
	if err == nil {
		err = startDiscovery(ctx, bus, adapterPath)
	}
	if err != nil {
		w.Stop(context.Background())
		return nil, fmt.Errorf("bluez: start watcher: %w", err)
	}
	diagnostic("event watcher started adapter=%s", adapterPath)
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
	started := time.Now()
	result, err := findBeacon(ctx, w.bus, w.adapterPath, w.name)
	elapsed := time.Since(started)
	w.stats.Polls++
	w.stats.ReadTotal += elapsed
	if elapsed > w.stats.ReadMax {
		w.stats.ReadMax = elapsed
	}
	if result != nil {
		w.signalMu.Lock()
		w.devicePath = result.Path
		w.signalMu.Unlock()
		w.stats.BeaconResults++
		if result.HasRSSI {
			w.stats.RSSIResults++
		}
	}
	return result, err
}

// observeRSSIUpdates records actual Device1.PropertiesChanged RSSI signals.
// GetManagedObjects may report the same cached RSSI indefinitely, so counting
// snapshots alone cannot establish whether the controller heard a new beacon.
// A separate signal channel keeps the GATT rxLoop's notifications intact.
func (w *Watcher) observeRSSIUpdates() {
	defer close(w.signalDone)
	for {
		select {
		case <-w.signalStop:
			return
		case sig, ok := <-w.signalCh:
			if !ok || sig == nil {
				return
			}
			if len(sig.Body) < 2 {
				continue
			}
			path := sig.Path
			var changed map[string]dbus.Variant
			switch sig.Name {
			case objMgrIface + ".InterfacesAdded":
				var ok bool
				path, ok = sig.Body[0].(dbus.ObjectPath)
				if !ok {
					continue
				}
				ifaces, ok := sig.Body[1].(map[string]map[string]dbus.Variant)
				if !ok {
					continue
				}
				changed = ifaces[deviceIface]
			case propsIface + ".PropertiesChanged":
				iface, _ := sig.Body[0].(string)
				if iface == adapterIface && path == w.adapterPath {
					w.wake()
					continue
				}
				if iface != deviceIface {
					continue
				}
				changed, _ = sig.Body[1].(map[string]dbus.Variant)
			default:
				continue
			}
			if !strings.HasPrefix(string(path), string(w.adapterPath)+"/dev_") {
				continue
			}
			name, named := advertisedName(changed, w.name)
			w.signalMu.Lock()
			if named && name == w.name {
				w.devicePath = path
			} else if named && name != w.name && path == w.devicePath {
				w.devicePath = ""
			}
			target := path == w.devicePath && !w.paused
			w.signalMu.Unlock()
			if !target {
				continue
			}
			rssi, hasRSSI := variantInt16(changed["RSSI"])
			signalHadRSSI := hasRSSI
			if !hasRSSI && advertisementUpdate(changed) {
				readCtx, cancel := context.WithTimeout(context.Background(), 400*time.Millisecond)
				v, err := w.bus.object(bluezService, path).getProp(readCtx, deviceIface, "RSSI")
				cancel()
				if err == nil {
					rssi, hasRSSI = variantInt16(v)
				}
			}
			if !hasRSSI {
				continue // Connected/Trusted alone cannot revive cached RSSI.
			}
			w.signalMu.Lock()
			if w.paused {
				w.signalMu.Unlock()
				continue
			}
			w.pendingAt = time.Now()
			w.advertisementUpdates++
			if signalHadRSSI {
				w.lastRSSIUpdate = w.pendingAt
				w.rssiUpdates++
			}
			w.pending = &ScanResult{Path: path, LocalName: w.name, RSSI: rssi, HasRSSI: true}
			if rssi >= w.nearRSSI {
				select {
				case w.nearUpdate <- struct{}{}:
				default:
				}
			}
			w.signalMu.Unlock()
			w.wake()
		}
	}
}

func advertisedName(props map[string]dbus.Variant, want string) (string, bool) {
	name, hasName := variantString(props["Name"])
	alias, hasAlias := variantString(props["Alias"])
	if hasName && name == want {
		return name, true
	}
	if hasAlias && alias == want {
		return alias, true
	}
	if hasName {
		return name, true
	}
	return alias, hasAlias
}

func advertisementUpdate(props map[string]dbus.Variant) bool {
	for _, key := range []string{"ManufacturerData", "ServiceData", "AdvertisingFlags", "TxPower"} {
		if _, ok := props[key]; ok {
			return true
		}
	}
	return false
}

func (w *Watcher) wake() {
	select {
	case w.updates <- struct{}{}:
	default:
	}
}

// Wait consumes the latest fresh advertisement delivered by the observer.
// It does not enumerate BlueZ objects; cached RSSI alone cannot trigger it.
// Discovery is continuous while disconnected. No beacon is (nil, nil).
func (w *Watcher) Wait(ctx context.Context) (*ScanResult, error) {
	if err := w.ensureDiscovering(ctx); err != nil {
		return nil, err
	}
	for {
		if ctx.Err() != nil {
			return nil, nil
		}
		w.signalMu.Lock()
		result := w.pending
		if w.paused || time.Since(w.pendingAt) > 5*time.Second {
			result = nil
		}
		w.pending = nil
		w.signalMu.Unlock()
		if result != nil && ctx.Err() == nil {
			return result, nil
		}
		select {
		case <-ctx.Done():
			return nil, nil
		case <-w.signalDone:
			return nil, errors.New("bluez: watcher signal listener stopped")
		case <-w.updates:
			if err := w.ensureDiscovering(ctx); err != nil {
				return nil, err
			}
		}
	}
}

// ensureDiscovering powers the adapter and starts LE discovery if BlueZ
// is not already scanning. Safe to call on every Peek: a live discovery
// session is a no-op.
func (w *Watcher) ensureDiscovering(ctx context.Context) error {
	w.discoveryMu.Lock()
	defer w.discoveryMu.Unlock()
	w.signalMu.Lock()
	paused := w.paused
	w.signalMu.Unlock()
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
	diagnostic("watcher discovery lost adapter=%s readError=%v", w.adapterPath, err)
	_ = setDiscoveryFilter(ctx, w.bus, w.adapterPath)
	if err := startDiscovery(ctx, w.bus, w.adapterPath); err != nil {
		diagnostic("watcher discovery restart failed: %s", dbusDetail(err))
		return err
	}
	w.stats.Restarts++
	return nil
}

// Stop turns discovery back off. Safe to call once; a Peek after Stop simply
// stops seeing new devices as BlueZ's cache goes stale.
func (w *Watcher) Stop(ctx context.Context) {
	w.stopOnce.Do(func() {
		w.signalMu.Lock()
		w.paused = true
		w.pending = nil
		w.signalMu.Unlock()
		diagnostic("watcher stopping adapter=%s", w.adapterPath)
		_ = w.bus.removeMatch(w.match...)
		_ = w.bus.removeMatch(w.ifaceMatch...)
		w.bus.unsubscribeSignals(w.signalCh)
		close(w.signalStop)
		<-w.signalDone
		w.discoveryMu.Lock()
		defer w.discoveryMu.Unlock()
		stopDiscovery(ctx, w.bus, w.adapterPath)
	})
}

// Pause drops pending advertisements while GATT is active. Continuous
// discovery resumes on link loss; no long scan-off windows while away.
func (w *Watcher) Pause(ctx context.Context) {
	w.discoveryMu.Lock()
	defer w.discoveryMu.Unlock()
	w.signalMu.Lock()
	if w.paused {
		w.signalMu.Unlock()
		return
	}
	w.paused = true
	w.pending = nil
	w.signalMu.Unlock()
	stopDiscovery(ctx, w.bus, w.adapterPath)
}

func (w *Watcher) Resume() {
	w.signalMu.Lock()
	if w.paused {
		w.pending = nil
	}
	w.paused = false
	w.signalMu.Unlock()
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
		return &AdapterOffError{Cause: err}
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
	_, err := bus.object(bluezService, adapterPath).call(ctx, adapterIface+".StopDiscovery")
	if err != nil {
		diagnostic("stop discovery failed adapter=%s: %s", adapterPath, dbusDetail(err))
	}
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
		devName, ok := advertisedName(dev, name)
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
