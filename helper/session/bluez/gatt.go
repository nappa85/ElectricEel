package bluez

import (
	"context"
	"fmt"
	"strings"
	"time"

	"github.com/godbus/dbus"
	"github.com/teslamotors/vehicle-command/pkg/connector"
)

const (
	connectRetryInitial = 500 * time.Millisecond
	connectRetryMax     = 3 * time.Second
	// connectAttemptTimeout caps a single Device.Connect. Sailfish
	// bluetoothd can ignore the D-Bus deadline; we abort via Disconnect
	// and let connect() retry with backoff instead of burning 20s.
	connectAttemptTimeout = 8 * time.Second
)

// liveAdvertisement reports a Device1 that is advertising right now.
// Connect to that object. A cached Device1 with no RSSI is the leftover
// that hangs Sailfish bluetoothd.
func liveAdvertisement(t *ScanResult) bool {
	return t != nil && t.Path != "" && t.HasRSSI
}

// connect connects to the vehicle and returns a live connector. target, when
// it is a live advertisement, is the Device1 to Connect — the Tesla Android
// phone key reconnects to the known MAC and never unpairs. A stale Device1
// (no RSSI) is forgotten first; Connect to that leftover hangs bluetoothd.
// Transient link/scan failures are retried until ctx expires; adapter-level
// failures (no controller) are not.
func connect(ctx context.Context, bus dbusBus, adapterID, vin string, target *ScanResult) (connector.Connector, error) {
	var lastErr error
	backoff := connectRetryInitial
	for {
		cc, retry, err := tryConnect(ctx, bus, adapterID, vin, target)
		if err == nil {
			return cc, nil
		}
		if !retry || IsAdapterError(err) {
			return nil, err
		}
		lastErr = err
		select {
		case <-ctx.Done():
			if lastErr != nil {
				return nil, lastErr
			}
			return nil, ctx.Err()
		case <-time.After(backoff):
			// Exponential backoff so a persistently-failing connect can't
			// hammer bluetoothd (observed to crash Sailfish BT when retried
			// every 100ms for a full connect timeout).
			backoff *= 2
			if backoff > connectRetryMax {
				backoff = connectRetryMax
			}
		}
	}
}

func tryConnect(ctx context.Context, bus dbusBus, adapterID, vin string, target *ScanResult) (connector.Connector, bool, error) {
	adapterPath, err := findAdapter(ctx, bus, adapterID)
	if err != nil {
		return nil, false, err // no controller: not a transient condition
	}
	// Prefer the adapter the advertisement arrived on. Jolla Phone 2026
	// exposes the radio as hci1; findAdapter would otherwise pick a
	// different powered Adapter1 if one appears.
	if p := adapterPathForDevice(scanPath(target)); p != "" {
		adapterPath = p
	}

	if !liveAdvertisement(target) {
		// Tesla Android reconnects to the known MAC. Only forget a
		// Device1 that is not advertising: Connect to that leftover
		// hangs Sailfish bluetoothd, then GetManagedObjects times out.
		// RemoveDevice on a live beacon is what the 2026-09-13 log
		// shows — AuthFailed leftover, then we refused to Connect
		// while the Watcher kept seeing RSSI -50..-80.
		forgotten := forgetStaleVehicle(ctx, bus, adapterPath, vin)
		if forgotten != "" {
			leftover, lerr := findBeacon(ctx, bus, adapterPath, vehicleBeaconName(vin))
			if lerr != nil {
				return nil, true, lerr
			}
			if leftover != nil && leftover.Path == forgotten && !leftover.HasRSSI {
				return nil, false, fmt.Errorf("bluez: leftover device %s after RemoveDevice", forgotten)
			}
			if liveAdvertisement(leftover) {
				target = leftover
			}
		}
		if !liveAdvertisement(target) {
			r, err := scan(ctx, bus, adapterID, vin)
			if err != nil {
				return nil, true, err
			}
			target = r
		}
	}
	if !liveAdvertisement(target) {
		return nil, true, fmt.Errorf("bluez: no live vehicle advertisement")
	}

	devPath, err := findDevice(ctx, bus, adapterPath, target.Path)
	if err != nil {
		return nil, true, err
	}
	// Device.Connect while Discovering is the Sailfish/BlueZ source of
	// le-connection-abort-by-local: the adapter cancels the LE create-
	// connection when the scanner is still running. Presence's Watcher
	// restarts discovery on the next Peek if this attempt fails.
	stopDiscovery(ctx, bus, adapterPath)
	waitDiscoveryStopped(ctx, bus, adapterPath)
	// A leftover Connected=true (previous Close still in HCI, or BlueZ
	// AutoConnect) must go down before we Connect, or the old Disconnect
	// completes on top of the new link.
	if already, err := deviceConnected(ctx, bus, devPath); err == nil && already {
		abortDeviceConnect(bus, devPath)
		waitDeviceDisconnected(ctx, bus, devPath)
	}
	if err := connectDevice(ctx, bus, devPath); err != nil {
		releaseDevice(bus, devPath)
		return nil, true, err
	}
	// GATT objects only materialize once the remote services are resolved.
	if err := waitServicesResolved(ctx, bus, devPath); err != nil {
		releaseDevice(bus, devPath)
		return nil, true, err
	}
	svcPath, txPath, rxPath, err := discoverGATT(ctx, bus, devPath)
	if err != nil {
		releaseDevice(bus, devPath)
		return nil, true, err
	}

	match := []dbus.MatchOption{
		dbus.WithMatchSender(bluezService),
		dbus.WithMatchInterface(propsIface),
		dbus.WithMatchMember("PropertiesChanged"),
		dbus.WithMatchPathNamespace(dbus.ObjectPath(string(rxPath))),
	}
	if err := bus.addMatch(match...); err != nil {
		releaseDevice(bus, devPath)
		return nil, true, fmt.Errorf("bluez: subscribe to vehicle RX characteristic: %w", err)
	}
	deviceMatch := []dbus.MatchOption{
		dbus.WithMatchSender(bluezService),
		dbus.WithMatchInterface(propsIface),
		dbus.WithMatchMember("PropertiesChanged"),
		dbus.WithMatchObjectPath(devPath),
	}
	// Best-effort: without this we still detect disconnects by polling
	// DeviceConnected from the presence loop.
	_ = bus.addMatch(deviceMatch...)

	if _, err := bus.object(bluezService, rxPath).call(ctx, gattChrIface+".StartNotify"); err != nil {
		_ = bus.removeMatch(match...)
		_ = bus.removeMatch(deviceMatch...)
		releaseDevice(bus, devPath)
		return nil, true, fmt.Errorf("bluez: subscribe to vehicle RX characteristic: %w", err)
	}

	c := &Connection{
		vin:         vin,
		bus:         bus,
		devPath:     devPath,
		svcPath:     svcPath,
		txPath:      txPath,
		rxPath:      rxPath,
		inbox:       make(chan []byte, connector.BufferSize),
		blockLength: maxExpectedMTU - 3,
		done:        make(chan struct{}),
		loopDone:    make(chan struct{}),
		match:       match,
		deviceMatch: deviceMatch,
		dropped:     make(chan struct{}),
	}
	// Tesla Android requestMtu(250). BlueZ exposes the negotiated ATT
	// MTU on the characteristic; use it so writes match the link.
	if mtu := characteristicMTU(ctx, bus, txPath); mtu >= defaultMTU {
		c.blockLength = mtu - 3
	}
	drainSignals(bus)
	c.armDropped()
	go c.rxLoop()
	return c, false, nil
}

func deviceConnected(ctx context.Context, bus dbusBus, devPath dbus.ObjectPath) (bool, error) {
	v, err := bus.object(bluezService, devPath).getProp(ctx, deviceIface, "Connected")
	if err != nil {
		return false, err
	}
	connected, ok := variantBool(v)
	if !ok {
		return false, fmt.Errorf("bluez: decode device Connected: got %T", v.Value())
	}
	return connected, nil
}

func waitDeviceDisconnected(ctx context.Context, bus dbusBus, devPath dbus.ObjectPath) {
	for {
		connected, err := deviceConnected(ctx, bus, devPath)
		if err != nil || !connected {
			return
		}
		select {
		case <-ctx.Done():
			return
		case <-time.After(50 * time.Millisecond):
		}
	}
}

func waitDiscoveryStopped(ctx context.Context, bus dbusBus, adapterPath dbus.ObjectPath) {
	waited := false
	for {
		discovering, err := adapterIsDiscovering(ctx, bus, adapterPath)
		if err != nil || !discovering {
			break
		}
		waited = true
		select {
		case <-ctx.Done():
			return
		case <-time.After(50 * time.Millisecond):
		}
	}
	if !waited {
		return
	}
	// Sailfish bluetoothd clears Discovering before the LE scanner has
	// actually stopped. Connecting in that window is abort-by-local.
	select {
	case <-ctx.Done():
	case <-time.After(50 * time.Millisecond):
	}
}

// adapterPathForDevice turns /org/bluez/hci0/dev_AA_BB into /org/bluez/hci0.
func adapterPathForDevice(devPath dbus.ObjectPath) dbus.ObjectPath {
	s := string(devPath)
	i := strings.LastIndex(s, "/dev_")
	if i <= 0 {
		return ""
	}
	return dbus.ObjectPath(s[:i])
}

func characteristicMTU(ctx context.Context, bus dbusBus, path dbus.ObjectPath) int {
	v, err := bus.object(bluezService, path).getProp(ctx, gattChrIface, "MTU")
	if err != nil {
		return 0
	}
	switch n := v.Value().(type) {
	case uint16:
		return int(n)
	case uint32:
		return int(n)
	case uint:
		return int(n)
	case int16:
		return int(n)
	case int:
		return n
	default:
		return 0
	}
}

func scanPath(t *ScanResult) dbus.ObjectPath {
	if t == nil {
		return ""
	}
	return t.Path
}

// releaseDevice drops a pending or live GATT link without RemoveDevice.
// Tesla Android disconnects and later reconnects to the same MAC; unpairing
// on Sailfish often fails with AuthFailed and leaves the leftover that
// tryConnect must not Connect.
func releaseDevice(bus dbusBus, devPath dbus.ObjectPath) {
	if devPath == "" {
		return
	}
	abortDeviceConnect(bus, devPath)
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	waitDeviceDisconnected(ctx, bus, devPath)
}

// forgetDevice disconnects and Adapter1.RemoveDevice so a stale (no RSSI)
// Device1 cannot be Connect'd. Safe if the path is already gone. Do not use
// this on a live advertisement or on Close of a working session.
func forgetDevice(bus dbusBus, devPath dbus.ObjectPath) {
	releaseDevice(bus, devPath)
	adapter := adapterPathForDevice(devPath)
	if adapter == "" {
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	_, _ = bus.object(bluezService, adapter).call(ctx, adapterIface+".RemoveDevice", devPath)
}

// forgetStaleVehicle RemoveDevice's a cached vehicle Device1 that is not
// advertising. A live RSSI means Connect that object, do not drop it.
func forgetStaleVehicle(ctx context.Context, bus dbusBus, adapterPath dbus.ObjectPath, vin string) dbus.ObjectPath {
	result, err := findBeacon(ctx, bus, adapterPath, vehicleBeaconName(vin))
	if err != nil || result == nil || result.Path == "" || result.HasRSSI {
		return ""
	}
	forgetDevice(bus, result.Path)
	return result.Path
}

func drainSignals(bus dbusBus) {
	drainSignalChan(bus.signals())
}

func drainSignalChan(ch <-chan *dbus.Signal) {
	for {
		select {
		case <-ch:
		default:
			return
		}
	}
}

// findDevice locates the scanned-for device in the object tree.
func findDevice(ctx context.Context, bus dbusBus, adapterPath dbus.ObjectPath, target dbus.ObjectPath) (dbus.ObjectPath, error) {
	objects, err := managedObjects(ctx, bus)
	if err != nil {
		return "", err
	}
	prefix := string(adapterPath) + "/dev_"
	if string(target) != "" && strings.HasPrefix(string(target), prefix) {
		if _, ok := objects[target]; ok {
			return target, nil
		}
	}
	return "", fmt.Errorf("bluez: device %s not found after scan", target)
}

// connectDevice issues Device1.Connect. The D-Bus method is run in a
// goroutine so a hung bluetoothd Connect cannot ignore ctx; on timeout we
// Disconnect, which is what makes BlueZ abort the in-flight LE create.
func connectDevice(ctx context.Context, bus dbusBus, devPath dbus.ObjectPath) error {
	attemptCtx, cancel := context.WithTimeout(ctx, connectAttemptTimeout)
	defer cancel()
	errCh := make(chan error, 1)
	go func() {
		_, err := bus.object(bluezService, devPath).call(attemptCtx, deviceIface+".Connect")
		errCh <- err
	}()
	select {
	case err := <-errCh:
		if err != nil {
			return fmt.Errorf("bluez: connect to vehicle: %s", dbusDetail(err))
		}
		return nil
	case <-attemptCtx.Done():
		abortDeviceConnect(bus, devPath)
		select {
		case err := <-errCh:
			if err != nil {
				return fmt.Errorf("bluez: connect to vehicle: %s", dbusDetail(err))
			}
			return nil
		case <-time.After(2 * time.Second):
			return fmt.Errorf("bluez: connect to vehicle: %w", attemptCtx.Err())
		}
	}
}

// abortDeviceConnect cancels a pending or partial LE connection. BlueZ
// Device.Connect often keeps the kernel attempt alive after the D-Bus call
// times out; the next Connect then fails with le-connection-abort-by-local.
func abortDeviceConnect(bus dbusBus, devPath dbus.ObjectPath) {
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	_, _ = bus.object(bluezService, devPath).call(ctx, deviceIface+".Disconnect")
}

// waitServicesResolved polls Device1.ServicesResolved until the remote GATT
// database is available.
func waitServicesResolved(ctx context.Context, bus dbusBus, devPath dbus.ObjectPath) error {
	obj := bus.object(bluezService, devPath)
	for {
		v, err := obj.getProp(ctx, deviceIface, "ServicesResolved")
		if err != nil {
			return fmt.Errorf("bluez: read ServicesResolved: %w", err)
		}
		if resolved, ok := variantBool(v); ok && resolved {
			return nil
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(pollInterval):
		}
	}
}

// discoverGATT finds the Tesla service and its TX/RX characteristics under
// the device. Service and characteristic UUIDs are matched by value alone;
// the Tesla UUIDs are unique enough that scoping to the service subtree is
// unnecessary (and keeps the walk simple).
func discoverGATT(ctx context.Context, bus dbusBus, devPath dbus.ObjectPath) (svcPath, txPath, rxPath dbus.ObjectPath, err error) {
	objects, err := managedObjects(ctx, bus)
	if err != nil {
		return "", "", "", err
	}
	for path, ifaces := range objects {
		if svc, ok := ifaces[gattSvcIface]; ok && isUUID(svc["UUID"], vehicleServiceUUID) {
			svcPath = path
			continue
		}
		if chr, ok := ifaces[gattChrIface]; ok {
			switch {
			case isUUID(chr["UUID"], toVehicleUUID):
				txPath = path
			case isUUID(chr["UUID"], fromVehicleUUID):
				rxPath = path
			}
		}
	}
	if svcPath == "" {
		return "", "", "", fmt.Errorf("bluez: vehicle service not found")
	}
	if txPath == "" || rxPath == "" {
		return "", "", "", fmt.Errorf("bluez: vehicle characteristics not found (tx=%s rx=%s)", txPath, rxPath)
	}
	return svcPath, txPath, rxPath, nil
}

// isUUID compares a BlueZ "UUID" property value against a dash-stripped,
// lowercase expectation.
func isUUID(v dbus.Variant, want string) bool {
	s, ok := variantString(v)
	if !ok {
		return false
	}
	return strings.ReplaceAll(strings.ToLower(s), "-", "") == want
}
