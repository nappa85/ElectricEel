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
	stopDiscovery(ctx, bus, adapterPath)
	if err := connectDevice(ctx, bus, devPath); err != nil {
		abortDeviceConnect(bus, devPath)
		return nil, true, err
	}
	// GATT objects only materialize once the remote services are resolved.
	if err := waitServicesResolved(ctx, bus, devPath); err != nil {
		abortDeviceConnect(bus, devPath)
		return nil, true, err
	}
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

	if _, err := bus.object(bluezService, rxPath).call(ctx, gattChrIface+".StartNotify"); err != nil {
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
		match:       match,
		deviceMatch: deviceMatch,
		dropped:     make(chan struct{}),
	}
	go c.rxLoop()
	return c, false, nil
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
