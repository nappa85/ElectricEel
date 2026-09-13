package bluez

import (
	"context"
	"errors"
	"fmt"
	"time"

	"github.com/godbus/dbus"
)

// fakeBluez is an in-memory stand-in for the org.bluez service. It
// implements enough of the D-Bus surface (via dbusBus/dbusCaller) for the
// transport logic to be unit-tested without a system bus: method calls are
// dispatched by name against a small device/state model, and tests can pump
// PropertiesChanged signals (e.g. GATT notification Values) into signals().
type fakeBluez struct {
	adapterID   string
	powered     bool
	discovering bool

	dev               *fakeDevice
	deviceVisible     bool // device present in GetManagedObjects (turn on after discovery)
	deviceAppearCall  int  // 0 = ignore; when >0, include dev after this many GetManagedObjects calls
	managedCalls      int
	servicesResolved  bool
	connected         bool
	gattReady         bool
	startedNotify     bool
	stoppedNotify     bool
	failLargeWrites   bool  // fail WriteValue when the chunk exceeds 20 bytes (ATT MTU 23)
	startDiscoveryErr error // when set, StartDiscovery returns this error
	connectErr        error // when set, Device1.Connect returns this error
	connectHang       time.Duration // when >0, Device1.Connect blocks this long (or until ctx ends)
	setPoweredErr     error // when set, Properties.Set(Powered) returns this error
	// extraAdapters are additional Adapter1 objects keyed by id ("hci1").
	// The value is the Powered flag. Used to test powered-adapter preference.
	extraAdapters map[string]bool
	autoConnect   bool
	trusted       bool
	// removedUntilDiscovery is set by Adapter1.RemoveDevice. The next
	// StartDiscovery makes the vehicle Device1 visible again, matching
	// BlueZ re-creating the object from a fresh advertisement.
	removedUntilDiscovery bool
	// reappearAfterManaged delays that reappearance by N GetManagedObjects
	// calls when discovery was already running, so a leftover-device check
	// right after RemoveDevice still sees the object gone.
	reappearAfterManaged int
	removeDeviceErr      error // when set, Adapter1.RemoveDevice fails and keeps the Device1
	mtu                  uint16 // GattCharacteristic1.MTU; 0 = property absent

	writes         [][]byte
	calls          []string
	sig            chan *dbus.Signal
	listeners      []chan *dbus.Signal
	matches        int
	addMatchErrAt  int
	removedMatches int
	removedMatch   bool
	removeDeviceN  int
}

type fakeDevice struct {
	path     dbus.ObjectPath
	name     string
	alias    string
	rssi     int16
	omitRSSI bool // when true, RSSI property is absent (BlueZ cache after ads stop)
}

func newFakeBluez() *fakeBluez {
	return &fakeBluez{
		adapterID: "hci0",
		powered:   true,
		sig:       make(chan *dbus.Signal, 16),
	}
}

func (f *fakeBluez) object(dest string, path dbus.ObjectPath) dbusCaller {
	return &fakeCaller{b: f, path: path}
}

func (f *fakeBluez) signals() <-chan *dbus.Signal { return f.sig }

func (f *fakeBluez) attachSignals() (<-chan *dbus.Signal, func()) {
	ch := make(chan *dbus.Signal, 16)
	f.listeners = append(f.listeners, ch)
	return ch, func() {
		for i, existing := range f.listeners {
			if existing == ch {
				f.listeners = append(f.listeners[:i], f.listeners[i+1:]...)
				return
			}
		}
	}
}

func (f *fakeBluez) emit(sig *dbus.Signal) {
	select {
	case f.sig <- sig:
	default:
	}
	for _, ch := range f.listeners {
		ch <- sig
	}
}

func (f *fakeBluez) addMatch(_ ...dbus.MatchOption) error {
	f.matches++
	if f.addMatchErrAt == f.matches {
		return errors.New("org.freedesktop.DBus.Error.MatchRuleInvalid")
	}
	return nil
}
func (f *fakeBluez) removeMatch(_ ...dbus.MatchOption) error {
	f.removedMatch = true
	f.removedMatches++
	return nil
}

// advertiseAdded simulates BlueZ materializing a newly discovered Device1.
func (f *fakeBluez) advertiseAdded() {
	if f.dev == nil {
		return
	}
	f.deviceVisible = true
	props := map[string]dbus.Variant{"Name": dbus.MakeVariant(f.dev.name)}
	if f.dev.alias != "" {
		props["Alias"] = dbus.MakeVariant(f.dev.alias)
	}
	if !f.dev.omitRSSI {
		props["RSSI"] = dbus.MakeVariant(f.dev.rssi)
	}
	f.emit(&dbus.Signal{
		Name: objMgrIface + ".InterfacesAdded",
		Path: "/",
		Body: []interface{}{
			f.dev.path,
			map[string]map[string]dbus.Variant{deviceIface: props},
		},
	})
}

// advertiseRSSI simulates the fresh Device1 RSSI update emitted for each
// advertisement when DuplicateData is enabled.
func (f *fakeBluez) advertiseRSSI(rssi int16) {
	if f.dev == nil {
		return
	}
	f.dev.rssi = rssi
	f.deviceVisible = true
	f.advertiseProps(map[string]dbus.Variant{"RSSI": dbus.MakeVariant(rssi)})
}

func (f *fakeBluez) advertiseProps(changed map[string]dbus.Variant) {
	if f.dev == nil {
		return
	}
	f.deviceVisible = true
	f.emit(&dbus.Signal{
		Name: propsIface + ".PropertiesChanged",
		Path: f.dev.path,
		Body: []interface{}{
			deviceIface,
			changed,
			[]string{},
		},
	})
}

func (f *fakeBluez) discoveryChanged(discovering bool) {
	f.discovering = discovering
	f.emit(&dbus.Signal{
		Name: propsIface + ".PropertiesChanged",
		Path: dbus.ObjectPath("/org/bluez/" + f.adapterID),
		Body: []interface{}{
			adapterIface,
			map[string]dbus.Variant{"Discovering": dbus.MakeVariant(discovering)},
			[]string{},
		},
	})
}

// notify simulates an org.bluez GattCharacteristic1 PropertiesChanged signal
// carrying a notification Value.
func (f *fakeBluez) notify(path dbus.ObjectPath, value []byte) {
	f.emit(&dbus.Signal{
		Name: propsIface + ".PropertiesChanged",
		Path: path,
		Body: []interface{}{
			gattChrIface,
			map[string]dbus.Variant{"Value": dbus.MakeVariant(value)},
			[]string{},
		},
	})
}

// devPath returns the device object path used by default fixtures.
func (f *fakeBluez) devPath() dbus.ObjectPath {
	if f.dev != nil {
		return f.dev.path
	}
	return dbus.ObjectPath("/org/bluez/" + f.adapterID + "/dev_AABBCCDDEEFF")
}

// svcPath/txPath/rxPath derive the GATT paths under the device path, matching
// how a real BlueZ object tree nests service/characteristic objects.
func (f *fakeBluez) svcPath() dbus.ObjectPath {
	return dbus.ObjectPath(string(f.devPath()) + "/service0021")
}

func (f *fakeBluez) txPath() dbus.ObjectPath {
	return dbus.ObjectPath(string(f.svcPath()) + "/char0021")
}

func (f *fakeBluez) rxPath() dbus.ObjectPath {
	return dbus.ObjectPath(string(f.svcPath()) + "/char0022")
}

// managedObjects builds an object tree reflecting the fake's current state.
func (f *fakeBluez) managedObjects() map[dbus.ObjectPath]map[string]map[string]dbus.Variant {
	f.managedCalls++
	if f.reappearAfterManaged > 0 {
		f.reappearAfterManaged--
	} else if f.removedUntilDiscovery && f.discovering {
		f.deviceVisible = true
		f.removedUntilDiscovery = false
	}
	m := map[dbus.ObjectPath]map[string]map[string]dbus.Variant{
		dbus.ObjectPath("/org/bluez/" + f.adapterID): {
			adapterIface: {
				"Powered":     dbus.MakeVariant(f.powered),
				"Discovering": dbus.MakeVariant(f.discovering),
			},
		},
	}
	for id, powered := range f.extraAdapters {
		m[dbus.ObjectPath("/org/bluez/"+id)] = map[string]map[string]dbus.Variant{
			adapterIface: {
				"Powered":     dbus.MakeVariant(powered),
				"Discovering": dbus.MakeVariant(false),
			},
		}
	}
	visible := f.deviceVisible
	if f.deviceAppearCall > 0 && f.managedCalls >= f.deviceAppearCall {
		visible = true
	}
	if f.dev != nil && visible {
		props := map[string]dbus.Variant{
			"Name":             dbus.MakeVariant(f.dev.name),
			"Connected":        dbus.MakeVariant(f.connected),
			"ServicesResolved": dbus.MakeVariant(f.servicesResolved),
			"Trusted":          dbus.MakeVariant(f.trusted),
			"AutoConnect":      dbus.MakeVariant(f.autoConnect),
		}
		if f.dev.alias != "" {
			props["Alias"] = dbus.MakeVariant(f.dev.alias)
		}
		if !f.dev.omitRSSI {
			props["RSSI"] = dbus.MakeVariant(f.dev.rssi)
		}
		m[f.dev.path] = map[string]map[string]dbus.Variant{
			deviceIface: props,
		}
		if f.gattReady {
			svc := f.svcPath()
			m[svc] = map[string]map[string]dbus.Variant{
				gattSvcIface: {"UUID": dbus.MakeVariant(vehicleServiceUUID)},
			}
			m[f.txPath()] = map[string]map[string]dbus.Variant{
				gattChrIface: {"UUID": dbus.MakeVariant(toVehicleUUID)},
			}
			m[f.rxPath()] = map[string]map[string]dbus.Variant{
				gattChrIface: {"UUID": dbus.MakeVariant(fromVehicleUUID)},
			}
		}
	}
	return m
}

// fakeCaller dispatches method calls against the fake's state model.
type fakeCaller struct {
	b    *fakeBluez
	path dbus.ObjectPath
}

func (fc *fakeCaller) call(ctx context.Context, method string, args ...interface{}) ([]interface{}, error) {
	fc.b.calls = append(fc.b.calls, method)
	switch method {
	case objMgrIface + ".GetManagedObjects":
		return []interface{}{fc.b.managedObjects()}, nil
	case adapterIface + ".SetDiscoveryFilter":
		return nil, nil
	case adapterIface + ".RemoveDevice":
		fc.b.removeDeviceN++
		if fc.b.removeDeviceErr != nil {
			return nil, fc.b.removeDeviceErr
		}
		wasVisible := fc.b.deviceVisible
		fc.b.connected = false
		fc.b.deviceVisible = false
		fc.b.servicesResolved = false
		fc.b.gattReady = false
		// Only a previously-visible Device1 comes back from the next
		// advertisement. RemoveDevice on a missing path must not invent one.
		if wasVisible {
			fc.b.removedUntilDiscovery = true
			if fc.b.dev != nil {
				// A Device1 recreated from a fresh advertisement has RSSI.
				fc.b.dev.omitRSSI = false
			}
			if fc.b.discovering {
				// Stay hidden for the next GetManagedObjects so
				// tryConnect's leftover check sees a successful drop.
				fc.b.reappearAfterManaged = 1
			}
		}
		return nil, nil
	case adapterIface + ".StartDiscovery":
		if fc.b.startDiscoveryErr != nil {
			return nil, fc.b.startDiscoveryErr
		}
		if fc.b.removedUntilDiscovery {
			fc.b.removedUntilDiscovery = false
			fc.b.deviceVisible = true
		}
		if fc.b.discovering {
			return nil, errors.New("org.bluez.Error.InProgress")
		}
		fc.b.discovering = true
		return nil, nil
	case adapterIface + ".StopDiscovery":
		fc.b.discovering = false
		return nil, nil
	case deviceIface + ".Connect":
		if fc.b.connectHang > 0 {
			timer := time.NewTimer(fc.b.connectHang)
			defer timer.Stop()
			select {
			case <-ctx.Done():
				return nil, ctx.Err()
			case <-timer.C:
			}
		}
		if fc.b.connectErr != nil {
			return nil, fc.b.connectErr
		}
		if fc.b.dev == nil || !fc.b.deviceVisible {
			return nil, errors.New("org.bluez.Error.Failed: no such device")
		}
		fc.b.connected = true
		// A fresh Device1 after RemoveDevice resolves GATT only once
		// Connect succeeds - mirror that so tests that forget+rescan still
		// find the Tesla service.
		fc.b.servicesResolved = true
		fc.b.gattReady = true
		return nil, nil
	case deviceIface + ".Disconnect":
		fc.b.connected = false
		return nil, nil
	case gattChrIface + ".StartNotify":
		fc.b.startedNotify = true
		return nil, nil
	case gattChrIface + ".StopNotify":
		fc.b.stoppedNotify = true
		return nil, nil
	case gattChrIface + ".WriteValue":
		b, ok := args[0].([]byte)
		if !ok {
			return nil, errors.New("WriteValue: expected []byte argument")
		}
		// Real org.bluez signature is WriteValue(ay value, a{sv} options) -
		// a live BlueZ rejects anything else (e.g. a bare string) as a
		// signature mismatch. Enforcing the real shape here is what would
		// have caught that bug in this suite instead of only against
		// hardware.
		if _, ok := args[1].(map[string]dbus.Variant); !ok {
			return nil, fmt.Errorf("WriteValue: expected a{sv} options argument, got %T", args[1])
		}
		if fc.b.failLargeWrites && len(b) > defaultMTU-3 {
			return nil, errors.New("org.bluez.Error.Failed: attribute value too large")
		}
		fc.b.writes = append(fc.b.writes, append([]byte(nil), b...))
		return nil, nil
	}
	return nil, fmt.Errorf("unexpected D-Bus method %q", method)
}

func (fc *fakeCaller) getProp(ctx context.Context, iface, prop string) (dbus.Variant, error) {
	switch prop {
	case "Powered":
		return dbus.MakeVariant(fc.b.powered), nil
	case "Discovering":
		return dbus.MakeVariant(fc.b.discovering), nil
	case "Power":
		return dbus.Variant{}, fmt.Errorf("org.freedesktop.DBus.Error.InvalidArgs: No such property '%s'", prop)
	case "ServicesResolved":
		return dbus.MakeVariant(fc.b.servicesResolved), nil
	case "Connected":
		return dbus.MakeVariant(fc.b.connected), nil
	case "Trusted":
		return dbus.MakeVariant(fc.b.trusted), nil
	case "AutoConnect":
		return dbus.MakeVariant(fc.b.autoConnect), nil
	case "RSSI":
		if fc.b.dev == nil || fc.b.dev.omitRSSI {
			return dbus.Variant{}, fmt.Errorf("org.freedesktop.DBus.Error.InvalidArgs: No such property 'RSSI'")
		}
		return dbus.MakeVariant(fc.b.dev.rssi), nil
	case "MTU":
		if fc.b.mtu == 0 {
			return dbus.Variant{}, fmt.Errorf("org.freedesktop.DBus.Error.InvalidArgs: No such property 'MTU'")
		}
		return dbus.MakeVariant(fc.b.mtu), nil
	}
	return dbus.Variant{}, fmt.Errorf("unexpected property %q", prop)
}

func (fc *fakeCaller) setProp(ctx context.Context, iface, prop string, value interface{}) error {
	switch prop {
	case "Powered":
		if fc.b.setPoweredErr != nil {
			return fc.b.setPoweredErr
		}
		b, ok := value.(bool)
		if !ok {
			return fmt.Errorf("Powered: expected bool, got %T", value)
		}
		fc.b.powered = b
		return nil
	case "Trusted":
		b, ok := value.(bool)
		if !ok {
			return fmt.Errorf("Trusted: expected bool, got %T", value)
		}
		fc.b.trusted = b
		return nil
	case "AutoConnect":
		b, ok := value.(bool)
		if !ok {
			return fmt.Errorf("AutoConnect: expected bool, got %T", value)
		}
		fc.b.autoConnect = b
		return nil
	}
	return fmt.Errorf("unexpected property %q", prop)
}
