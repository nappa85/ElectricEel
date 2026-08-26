package bluez

import (
	"context"
	"errors"
	"testing"
	"time"
)

// TestConnectRetriesTransientFailures mirrors upstream ble_test.go's
// NewConnectionFromScanResult test: a device that only appears in the object
// tree after a couple of enumeration attempts must not fail the connect, it
// must be retried until it shows up (within ctx).
func TestConnectRetriesTransientFailures(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}
	// The device is visible from the 3rd GetManagedObjects call onward
	// (findAdapter takes #1, the first findDevice #2 fails, the retry's
	// findAdapter #3 + findDevice #4 succeed). deviceVisible must be true so
	// Device1.Connect is accepted once findDevice succeeds.
	bus.deviceVisible = true
	bus.deviceAppearCall = 3
	bus.servicesResolved = true
	bus.gattReady = true

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	cc, err := connect(ctx, bus, "hci0", vin, &ScanResult{Path: bus.dev.path})
	if err != nil {
		t.Fatalf("connect: %v", err)
	}
	if _, ok := cc.(*Connection); !ok {
		t.Fatalf("connect returned %T, want *Connection", cc)
	}
	if !bus.connected {
		t.Error("expected Device1.Connect after the retry succeeded")
	}
	if bus.managedCalls < 3 {
		t.Errorf("expected at least one retry (3 GetManagedObjects), got %d", bus.managedCalls)
	}
}

// TestConnectStopsOnAdapterError verifies that adapter-level failures are not
// retried - the call returns immediately with the adapter error instead of
// looping until ctx expires.
func TestConnectStopsOnAdapterError(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}
	bus.deviceVisible = true

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	// hci9 doesn't exist; findAdapter fails with the "no Bluetooth adapter
	// found" error, which IsAdapterError classifies as permanent.
	start := time.Now()
	_, err := connect(ctx, bus, "hci9", vin, &ScanResult{Path: bus.dev.path})
	if err == nil {
		t.Fatal("expected connect to fail for a missing adapter")
	}
	if !IsAdapterError(err) {
		t.Errorf("error %v should be classified as an adapter error", err)
	}
	if time.Since(start) > time.Second {
		t.Error("connect returned only after retrying - adapter errors must not be retried")
	}
}

func TestIsAdapterError(t *testing.T) {
	cases := []struct {
		err  error
		want bool
	}{
		{errors.New("operation not permitted"), true},
		{errors.New("org.bluez.Error.NotReady: Resource Not Ready"), true},
		{errors.New("org.bluez.Error.NotPowered: RFKILL"), true},
		{errors.New("org.freedesktop.DBus.Error.ServiceUnknown: name org.bluez not found"), true},
		{errors.New("bluez: no Bluetooth adapter found (wanted \"hci9\")"), true},
		{errors.New("org.bluez.Error.Failed: link lost"), false},
		{errors.New("org.bluez.Error.InProgress"), false},
		{errors.New("bluez: device /org/bluez/hci0/dev_1 not found after scan"), false},
		{nil, false},
	}
	for _, c := range cases {
		if got := IsAdapterError(c.err); got != c.want {
			t.Errorf("IsAdapterError(%q) = %v, want %v", c.err, got, c.want)
		}
	}
}

func TestConnectStopsDiscoveryBeforeDeviceConnect(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}
	bus.deviceVisible = true
	bus.servicesResolved = true
	bus.gattReady = true
	bus.discovering = true

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	if _, err := connect(ctx, bus, "hci0", vin, &ScanResult{Path: bus.dev.path}); err != nil {
		t.Fatalf("connect: %v", err)
	}
	stopAt, connectAt := -1, -1
	for i, c := range bus.calls {
		switch c {
		case adapterIface + ".StopDiscovery":
			if stopAt < 0 {
				stopAt = i
			}
		case deviceIface + ".Connect":
			if connectAt < 0 {
				connectAt = i
			}
		}
	}
	if stopAt < 0 {
		t.Fatal("expected StopDiscovery before Device.Connect (scanning during connect causes le-connection-abort-by-local)")
	}
	if connectAt < 0 {
		t.Fatal("expected Device.Connect")
	}
	if stopAt > connectAt {
		t.Errorf("StopDiscovery at call %d after Connect at %d", stopAt, connectAt)
	}
	if bus.discovering {
		t.Error("discovery should stay off after a successful connect")
	}
}

func TestConnectAbortsPendingLinkOnFailure(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}
	bus.deviceVisible = true
	bus.connectErr = errors.New("org.bluez.Error.Failed: le-connection-abort-by-local")

	ctx, cancel := context.WithTimeout(context.Background(), 800*time.Millisecond)
	defer cancel()

	if _, err := connect(ctx, bus, "hci0", vin, &ScanResult{Path: bus.dev.path}); err == nil {
		t.Fatal("expected connect to fail")
	}
	if n := countCalls(bus.calls, deviceIface+".Disconnect"); n == 0 {
		t.Fatal("failed Connect must Disconnect so the next attempt is not aborted-by-local")
	}
}
