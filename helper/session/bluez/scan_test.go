package bluez

import (
	"context"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/godbus/dbus"
	"github.com/teslamotors/vehicle-command/pkg/connector/ble"
)

func TestVehicleBeaconNameMatchesUpstream(t *testing.T) {
	// Guard against accidentally re-implementing the name format: the value
	// must exactly match upstream's ble.VehicleLocalName.
	for _, vin := range []string{
		"5YJ3E1EA0PF000000",
		"LRW3E7FA9NC123456",
		"SAYHCBAFXGC123456",
	} {
		if got, want := vehicleBeaconName(vin), ble.VehicleLocalName(vin); got != want {
			t.Errorf("vehicleBeaconName(%q) = %q, want %q (must match upstream)", vin, got, want)
		}
		if !strings.HasPrefix(vehicleBeaconName(vin), "S") || !strings.HasSuffix(vehicleBeaconName(vin), "C") {
			t.Errorf("vehicleBeaconName(%q) = %q, expected S...C beacon format", vin, vehicleBeaconName(vin))
		}
	}
}

func TestScanFindsVehicleBeacon(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{
		path: dbus.ObjectPath("/org/bluez/hci0/dev_DDEADBEEF001"),
		name: vehicleBeaconName(vin),
		rssi: -55,
	}
	bus.deviceVisible = true

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	res, err := scan(ctx, bus, "", vin)
	if err != nil {
		t.Fatalf("scan: %v", err)
	}
	if res.Path != bus.dev.path {
		t.Errorf("res.Path = %s, want %s", res.Path, bus.dev.path)
	}
	if res.LocalName != bus.dev.name {
		t.Errorf("res.LocalName = %q, want %q", res.LocalName, bus.dev.name)
	}
	if res.RSSI != -55 {
		t.Errorf("res.RSSI = %d, want -55", res.RSSI)
	}
	if !res.HasRSSI {
		t.Error("expected HasRSSI=true when fake reports RSSI")
	}
	if bus.discovering {
		t.Error("expected discovery to be stopped after scan returned")
	}
	// The scan must have started (and therefore stopped) discovery.
	if !hasCall(bus.calls, adapterIface+".StartDiscovery") {
		t.Error("expected StartDiscovery to have been called")
	}
}

func TestScanLeavesSharedDiscoveryRunning(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}
	bus.deviceVisible = true
	bus.discovering = true

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	if _, err := scan(ctx, bus, "", vin); err != nil {
		t.Fatalf("scan: %v", err)
	}
	if !bus.discovering {
		t.Error("scan must not StopDiscovery when presence already holds it")
	}
}

func TestFindAdapterPrefersPowered(t *testing.T) {
	bus := newFakeBluez()
	bus.powered = true
	bus.extraAdapters = map[string]bool{
		"hci1": false,
		"zzz":  false,
	}

	path, err := findAdapter(context.Background(), bus, "")
	if err != nil {
		t.Fatalf("findAdapter: %v", err)
	}
	if path != dbus.ObjectPath("/org/bluez/hci0") {
		t.Fatalf("findAdapter picked %s, want powered hci0", path)
	}
}

func TestEnsurePoweredKeepsDBusErrorName(t *testing.T) {
	bus := newFakeBluez()
	bus.powered = false
	bus.setPoweredErr = dbus.Error{Name: "org.bluez.Error.Failed", Body: []interface{}{""}}

	err := ensurePowered(context.Background(), bus, dbus.ObjectPath("/org/bluez/hci0"))
	if err == nil {
		t.Fatal("expected ensurePowered to fail when Set Powered is denied")
	}
	if !strings.Contains(err.Error(), "org.bluez.Error.Failed") {
		t.Fatalf("error %q should include the D-Bus name (logs used to show an empty suffix)", err)
	}
}

func TestScanHonorsSpecificAdapter(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}
	bus.deviceVisible = true

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	if _, err := scan(ctx, bus, "hci0", vin); err != nil {
		t.Fatalf("scan with matching adapter: %v", err)
	}
	if _, err := scan(ctx, bus, "hci9", vin); err == nil {
		t.Fatal("scan with nonexistent adapter should fail")
	}
}

func TestScanPowersOnAdapter(t *testing.T) {
	bus := newFakeBluez()
	bus.powered = false
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}
	bus.deviceVisible = true

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	if _, err := scan(ctx, bus, "", vin); err != nil {
		t.Fatalf("scan: %v", err)
	}
	if !bus.powered {
		t.Error("expected scan to power the adapter back on")
	}
}

func TestScanWaitsForBeaconToAppear(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}
	bus.deviceVisible = false
	bus.deviceAppearCall = 3 // beacon only shows up after a few polls
	bus.gattReady = false

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	res, err := scan(ctx, bus, "", vin)
	if err != nil {
		t.Fatalf("scan: %v", err)
	}
	if res.LocalName != bus.dev.name {
		t.Errorf("scan returned %q after delayed appearance", res.LocalName)
	}
}

func TestScanTimesOutWithoutBeacon(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}
	bus.deviceVisible = false

	ctx, cancel := context.WithTimeout(context.Background(), 150*time.Millisecond)
	defer cancel()

	if _, err := scan(ctx, bus, "", vin); err == nil {
		t.Fatal("expected scan to time out when no beacon ever appears")
	}
}

func TestScanIgnoresOtherDevices(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	// A different device is present and advertising a different name; the
	// vehicle is never seen.
	bus.dev = &fakeDevice{path: bus.devPath(), name: "SOME OTHER SOUNDBAR"}
	bus.deviceVisible = true

	ctx, cancel := context.WithTimeout(context.Background(), 150*time.Millisecond)
	defer cancel()

	if _, err := scan(ctx, bus, "", vin); err == nil {
		t.Fatal("expected scan not to match a non-vehicle device name")
	}
}

func TestFindBeaconReportsMissingRSSI(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin), omitRSSI: true}
	bus.deviceVisible = true

	res, err := findBeacon(context.Background(), bus, dbus.ObjectPath("/org/bluez/hci0"), vehicleBeaconName(vin))
	if err != nil {
		t.Fatalf("findBeacon: %v", err)
	}
	if res == nil {
		t.Fatal("expected cached device to still be found")
	}
	if res.HasRSSI {
		t.Fatal("expected HasRSSI=false when BlueZ omits RSSI")
	}
}

func TestStartDiscoveryToleratesAlreadyInProgress(t *testing.T) {
	if !isDiscoveryInProgress(errors.New("org.bluez.Error.InProgress")) {
		t.Fatal("expected InProgress error to be recognized")
	}
	bus := newFakeBluez()
	bus.discovering = true
	ctx := context.Background()
	adapterPath := dbus.ObjectPath("/org/bluez/hci0")
	if err := startDiscovery(ctx, bus, adapterPath); err != nil {
		t.Fatalf("startDiscovery with already discovering should succeed: %v", err)
	}
}

func hasCall(calls []string, want string) bool {
	for _, c := range calls {
		if c == want {
			return true
		}
	}
	return false
}

func countCalls(calls []string, want string) int {
	n := 0
	for _, c := range calls {
		if c == want {
			n++
		}
	}
	return n
}
