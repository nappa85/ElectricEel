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
)

// connect connects to the vehicle and returns a live connector. If target is
// nil the vehicle's beacon is scanned for first. Like upstream
// NewConnectionFromScanResult, transient link/scan failures are retried until
// ctx expires; adapter-level failures (no controller) are not.
func connect(ctx context.Context, bus dbusBus, adapterID, vin string, target *ScanResult) (connector.Connector, error) {
	started := time.Now()
	var lastErr error
	backoff := connectRetryInitial
	attempts := 0
	for {
		attempts++
		attemptStart := time.Now()
		cc, retry, err := tryConnect(ctx, bus, adapterID, vin, target)
		if err == nil {
			diagnostic("connect ready attempts=%d elapsed=%s", attempts, time.Since(started).Round(time.Millisecond))
			return cc, nil
		}
		diagnostic("connect attempt=%d duration=%s retry=%v: %s", attempts, time.Since(attemptStart).Round(time.Millisecond), retry, dbusDetail(err))
		if !retry || IsAdapterError(err) {
			return nil, err
		}
		lastErr = err
		select {
		case <-ctx.Done():
			diagnostic("connect deadline attempts=%d elapsed=%s", attempts, time.Since(started).Round(time.Millisecond))
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
	if target == nil {
		r, err := scan(ctx, bus, adapterID, vin)
		if err != nil {
			return nil, true, err
		}
		target = r
	}

	adapterPath, err := findAdapter(ctx, bus, adapterID)
	if err != nil {
		return nil, false, err // no controller: not a transient condition
	}
	devPath, err := findDevice(ctx, bus, adapterPath, target.Path)
	if err != nil {
		return nil, true, err
	}
	// Device.Connect while Discovering is the Sailfish/BlueZ source of
	// le-connection-abort-by-local: the adapter cancels the LE create-
	// connection when the scanner is still running. Presence's Watcher
	// restarts discovery on the next Peek if this attempt fails.
	diagnostic("connect stopping discovery before Device1.Connect")
	stopDiscovery(ctx, bus, adapterPath)
	if err := waitDiscoveryStopped(ctx, bus, adapterPath); err != nil {
		return nil, true, err
	}
	// Do not let completion of an old Disconnect tear down the new link.
	if connected, err := deviceConnected(ctx, bus, devPath); err == nil && connected {
		abortDeviceConnect(bus, devPath)
		if err := waitDeviceDisconnected(ctx, bus, devPath); err != nil {
			return nil, true, err
		}
	}
	// BlueZ's Discovering property can change before the controller settles.
	select {
	case <-ctx.Done():
		return nil, true, ctx.Err()
	case <-time.After(150 * time.Millisecond):
	}
	diagnostic("connect Device1.Connect begin")
	if err := connectDevice(ctx, bus, devPath); err != nil {
		abortDeviceConnect(bus, devPath)
		return nil, true, err
	}
	diagnostic("connect Device1.Connect ok; waiting for ServicesResolved")
	// GATT objects only materialize once the remote services are resolved.
	if err := waitServicesResolved(ctx, bus, devPath); err != nil {
		// The connect deadline has usually expired here. Probe with a fresh,
		// short context so we can distinguish a missing link from an
		// unresponsive bluetoothd before Disconnect changes the state.
		logDeviceState(bus, devPath, "services wait failed")
		abortDeviceConnect(bus, devPath)
		return nil, true, err
	}
	diagnostic("connect services resolved; finding GATT characteristics")
	svcPath, txPath, rxPath, err := discoverGATT(ctx, bus, devPath)
	if err != nil {
		abortDeviceConnect(bus, devPath)
		return nil, true, err
	}

	match := []dbus.MatchOption{
		dbus.WithMatchSender(bluezService),
		dbus.WithMatchInterface(propsIface),
		dbus.WithMatchMember("PropertiesChanged"),
		dbus.WithMatchPathNamespace(dbus.ObjectPath(string(rxPath))),
	}
	if err := bus.addMatch(match...); err != nil {
		abortDeviceConnect(bus, devPath)
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

	// Subscribe only after the new link is resolved, but before StartNotify
	// can deliver data. Signals queued during old-link cleanup stay out of
	// this connection's RX loop.
	signalCh := make(chan *dbus.Signal, 128)
	bus.subscribeSignals(signalCh)
	if _, err := bus.object(bluezService, rxPath).call(ctx, gattChrIface+".StartNotify"); err != nil {
		bus.unsubscribeSignals(signalCh)
		_ = bus.removeMatch(match...)
		_ = bus.removeMatch(deviceMatch...)
		abortDeviceConnect(bus, devPath)
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
		signalCh:    signalCh,
		match:       match,
		deviceMatch: deviceMatch,
		dropped:     make(chan struct{}),
	}
	if v, err := bus.object(bluezService, txPath).getProp(ctx, gattChrIface, "MTU"); err == nil {
		if mtu, ok := v.Value().(uint16); ok && mtu >= defaultMTU {
			c.blockLength = int(mtu) - 3
		}
	}
	go c.rxLoop()
	return c, false, nil
}

func deviceConnected(ctx context.Context, bus dbusBus, path dbus.ObjectPath) (bool, error) {
	v, err := bus.object(bluezService, path).getProp(ctx, deviceIface, "Connected")
	if err != nil {
		return false, err
	}
	connected, ok := variantBool(v)
	if !ok {
		return false, fmt.Errorf("bluez: invalid Connected property")
	}
	return connected, nil
}

func waitDeviceDisconnected(ctx context.Context, bus dbusBus, path dbus.ObjectPath) error {
	for {
		connected, err := deviceConnected(ctx, bus, path)
		if err != nil || !connected {
			return err
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(50 * time.Millisecond):
		}
	}
}

func waitDiscoveryStopped(ctx context.Context, bus dbusBus, path dbus.ObjectPath) error {
	for {
		discovering, err := adapterIsDiscovering(ctx, bus, path)
		if err != nil || !discovering {
			return err
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(50 * time.Millisecond):
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

// connectDevice issues Device1.Connect.
func connectDevice(ctx context.Context, bus dbusBus, devPath dbus.ObjectPath) error {
	if _, err := bus.object(bluezService, devPath).call(ctx, deviceIface+".Connect"); err != nil {
		return fmt.Errorf("bluez: connect to vehicle: %w", err)
	}
	return nil
}

// abortDeviceConnect cancels a pending or partial LE connection. BlueZ
// Device.Connect often keeps the kernel attempt alive after the D-Bus call
// times out; the next Connect then fails with le-connection-abort-by-local.
func abortDeviceConnect(bus dbusBus, devPath dbus.ObjectPath) {
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	started := time.Now()
	_, err := bus.object(bluezService, devPath).call(ctx, deviceIface+".Disconnect")
	diagnostic("connect cleanup Disconnect duration=%s error=%s", time.Since(started).Round(time.Millisecond), dbusDetail(err))
}

// waitServicesResolved polls Device1.ServicesResolved until the remote GATT
// database is available.
func waitServicesResolved(ctx context.Context, bus dbusBus, devPath dbus.ObjectPath) error {
	obj := bus.object(bluezService, devPath)
	started := time.Now()
	lastProgress := started
	polls := 0
	for {
		v, err := obj.getProp(ctx, deviceIface, "ServicesResolved")
		if err != nil {
			diagnostic("connect services read failed elapsed=%s polls=%d error=%s", time.Since(started).Round(time.Millisecond), polls, dbusDetail(err))
			return fmt.Errorf("bluez: read ServicesResolved: %w", err)
		}
		polls++
		if resolved, ok := variantBool(v); ok && resolved {
			diagnostic("connect services wait complete elapsed=%s polls=%d", time.Since(started).Round(time.Millisecond), polls)
			return nil
		}
		if time.Since(lastProgress) >= 3*time.Second {
			lastProgress = time.Now()
			connected, err := obj.getProp(ctx, deviceIface, "Connected")
			if err != nil {
				diagnostic("connect services pending elapsed=%s polls=%d connectedError=%s", time.Since(started).Round(time.Millisecond), polls, dbusDetail(err))
			} else {
				value, ok := variantBool(connected)
				diagnostic("connect services pending elapsed=%s polls=%d connected=%v valid=%v", time.Since(started).Round(time.Millisecond), polls, value, ok)
			}
		}
		select {
		case <-ctx.Done():
			diagnostic("connect services wait timeout elapsed=%s polls=%d", time.Since(started).Round(time.Millisecond), polls)
			return ctx.Err()
		case <-time.After(pollInterval):
		}
	}
}

// logDeviceState uses a bounded independent deadline because the failed
// connect's context is generally already cancelled. Never log the device
// path: it contains the Bluetooth address.
func logDeviceState(bus dbusBus, devPath dbus.ObjectPath, phase string) {
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	obj := bus.object(bluezService, devPath)
	for _, property := range []string{"Connected", "ServicesResolved"} {
		v, err := obj.getProp(ctx, deviceIface, property)
		if err != nil {
			diagnostic("connect %s %s error=%s", phase, property, dbusDetail(err))
			continue
		}
		value, ok := variantBool(v)
		diagnostic("connect %s %s=%v valid=%v", phase, property, value, ok)
	}
}

// discoverGATT finds the Tesla service and its TX/RX characteristics under
// the device. Paths are scoped to the device subtree: with two nearby
// Teslas, matching UUIDs by value alone could mix tx from car A with rx
// from car B.
func discoverGATT(ctx context.Context, bus dbusBus, devPath dbus.ObjectPath) (svcPath, txPath, rxPath dbus.ObjectPath, err error) {
	objects, err := managedObjects(ctx, bus)
	if err != nil {
		return "", "", "", err
	}
	prefix := string(devPath) + "/"
	for path, ifaces := range objects {
		if !strings.HasPrefix(string(path), prefix) {
			continue
		}
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
