package bluez

import (
	"context"
	"errors"
	"testing"
	"time"

	"github.com/godbus/dbus"
)

// TestConnectRetriesTransientFailures mirrors upstream ble_test.go's
// NewConnectionFromScanResult test: a device that only appears in the object
// tree after a couple of enumeration attempts must not fail the connect, it
// must be retried until it shows up (within ctx).
func TestLiveAdvertisement(t *testing.T) {
	if liveAdvertisement(nil) {
		t.Fatal("nil is not a live advertisement")
	}
	if liveAdvertisement(&ScanResult{Path: "/org/bluez/hci1/dev_AA"}) {
		t.Fatal("Device1 without RSSI is the leftover that hangs bluetoothd")
	}
	if !liveAdvertisement(&ScanResult{Path: "/org/bluez/hci1/dev_AA", HasRSSI: true, RSSI: -60}) {
		t.Fatal("advertising Device1 must be treated as live")
	}
}

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
	if bus.removeDeviceN != 0 {
		t.Fatal("failed Connect must not RemoveDevice; the next attempt reconnects to the same MAC")
	}
}

func TestConnectDoesNotConnectLeftoverDeviceWithoutRSSI(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin), omitRSSI: true}
	bus.deviceVisible = true
	bus.removeDeviceErr = errors.New("org.freedesktop.DBus.Error.AuthFailed")

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	start := time.Now()
	_, err := connect(ctx, bus, "hci0", vin, &ScanResult{Path: bus.dev.path})
	if err == nil {
		t.Fatal("expected leftover Device1 with no RSSI after failed RemoveDevice to abort connect")
	}
	if elapsed := time.Since(start); elapsed > 200*time.Millisecond {
		t.Fatalf("leftover Device1 returned after %v; must fail immediately, not burn the connect deadline", elapsed)
	}
	if n := countCalls(bus.calls, deviceIface+".Connect"); n != 0 {
		t.Fatalf("Device.Connect called %d times on a leftover Device1, want 0", n)
	}
}

func TestConnectProceedsWhenLeftoverHasLiveRSSI(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin), rssi: -52}
	bus.deviceVisible = true
	bus.servicesResolved = true
	bus.gattReady = true
	bus.removeDeviceErr = errors.New("org.freedesktop.DBus.Error.AuthFailed")

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	if _, err := connect(ctx, bus, "hci0", vin, &ScanResult{Path: bus.dev.path, HasRSSI: true, RSSI: -52}); err != nil {
		t.Fatalf("live advertisement must connect even if RemoveDevice would fail: %v", err)
	}
	if n := countCalls(bus.calls, deviceIface+".Connect"); n == 0 {
		t.Fatal("expected Device.Connect when the Device1 still has RSSI")
	}
	if bus.removeDeviceN != 0 {
		t.Fatal("live RSSI must skip RemoveDevice")
	}
}

func TestConnectForgetsStaleDeviceBeforeRescan(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin), rssi: -55, omitRSSI: true}
	bus.deviceVisible = true
	bus.servicesResolved = true
	bus.gattReady = true

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	// No live target: a cached Device1 without RSSI must be dropped so the
	// subsequent scan can materialize a fresh advertisement.
	if _, err := connect(ctx, bus, "hci0", vin, nil); err != nil {
		t.Fatalf("connect: %v", err)
	}
	if bus.removeDeviceN == 0 {
		t.Fatal("stale Device1 with no RSSI must be RemoveDevice'd before rescanning")
	}
	if !hasCall(bus.calls, adapterIface+".StartDiscovery") {
		t.Fatal("connect must rescan after forgetting the stale Device1")
	}
	if !bus.connected {
		t.Fatal("expected a live connection after forget+rescan")
	}
}

func TestConnectUsesAdvertisementAdapter(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	hci1Path := dbus.ObjectPath("/org/bluez/hci1/dev_98_04_ED_D7_EE_5E")
	bus.extraAdapters = map[string]bool{"hci1": true}
	bus.dev = &fakeDevice{path: hci1Path, name: vehicleBeaconName(vin), rssi: -58}
	bus.deviceVisible = true
	bus.servicesResolved = true
	bus.gattReady = true

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	if _, err := connect(ctx, bus, "", vin, &ScanResult{Path: hci1Path, HasRSSI: true, RSSI: -58}); err != nil {
		t.Fatalf("connect on hci1 advertisement: %v", err)
	}
	if !bus.connected {
		t.Fatal("expected Connect on the advertisement's adapter, not a different Adapter1")
	}
}

func TestConnectUsesNegotiatedMTU(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin), rssi: -55}
	bus.deviceVisible = true
	bus.servicesResolved = true
	bus.gattReady = true
	bus.mtu = 250 // Tesla Android requestMtu(250)

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	cc, err := connect(ctx, bus, "hci0", vin, &ScanResult{Path: bus.dev.path, HasRSSI: true, RSSI: -55})
	if err != nil {
		t.Fatalf("connect: %v", err)
	}
	c, ok := cc.(*Connection)
	if !ok {
		t.Fatalf("got %T", cc)
	}
	if c.blockLength != 247 {
		t.Fatalf("blockLength = %d, want 247 (negotiated MTU 250 - 3)", c.blockLength)
	}
}

func TestConnectDeviceAbortsHungConnect(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}
	bus.deviceVisible = true
	bus.connectHang = 5 * time.Second

	ctx, cancel := context.WithTimeout(context.Background(), 200*time.Millisecond)
	defer cancel()
	start := time.Now()
	err := connectDevice(ctx, bus, bus.devPath())
	if err == nil {
		t.Fatal("expected hung Connect to fail when ctx expires")
	}
	if elapsed := time.Since(start); elapsed > time.Second {
		t.Fatalf("hung Connect returned after %v, want roughly the 200ms deadline", elapsed)
	}
	if n := countCalls(bus.calls, deviceIface+".Disconnect"); n == 0 {
		t.Fatal("expired Connect must Disconnect to cancel the in-flight LE create")
	}
}

func TestAdapterPathForDevice(t *testing.T) {
	if got := adapterPathForDevice("/org/bluez/hci0/dev_AA_BB_CC"); got != "/org/bluez/hci0" {
		t.Fatalf("adapterPathForDevice = %q, want /org/bluez/hci0", got)
	}
	if got := adapterPathForDevice("/org/bluez/hci0"); got != "" {
		t.Fatalf("adapterPathForDevice(adapter) = %q, want empty", got)
	}
}
