package bluez

import (
	"context"
	"testing"
	"time"
)

func TestWatcherPeekTracksBeaconVisibility(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin), rssi: -60}
	bus.deviceVisible = false

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	w, err := newWatcher(ctx, bus, "", vin)
	if err != nil {
		t.Fatalf("newWatcher: %v", err)
	}
	defer w.Stop(ctx)

	res, err := w.Peek(ctx)
	if err != nil {
		t.Fatalf("Peek (not yet visible): %v", err)
	}
	if res != nil {
		t.Fatalf("Peek returned a result before the beacon appeared: %+v", res)
	}

	bus.deviceVisible = true
	res, err = w.Peek(ctx)
	if err != nil {
		t.Fatalf("Peek (visible): %v", err)
	}
	if res == nil {
		t.Fatal("Peek returned nil after the beacon appeared")
	}
	if res.RSSI != -60 {
		t.Errorf("res.RSSI = %d, want -60", res.RSSI)
	}

	bus.deviceVisible = false
	res, err = w.Peek(ctx)
	if err != nil {
		t.Fatalf("Peek (departed): %v", err)
	}
	if res != nil {
		t.Fatalf("Peek returned a result after the beacon disappeared: %+v", res)
	}
}

func TestWatcherStartsDiscoveryOnceAndStopStopsIt(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	w, err := newWatcher(ctx, bus, "", vin)
	if err != nil {
		t.Fatalf("newWatcher: %v", err)
	}
	if !bus.discovering {
		t.Error("expected newWatcher to start discovery")
	}

	for i := 0; i < 5; i++ {
		if _, err := w.Peek(ctx); err != nil {
			t.Fatalf("Peek #%d: %v", i, err)
		}
	}
	if n := countCalls(bus.calls, adapterIface+".StartDiscovery"); n != 1 {
		t.Errorf("StartDiscovery called %d times across repeated Peek, want 1 (Peek must not restart a live discovery)", n)
	}

	w.Stop(ctx)
	if bus.discovering {
		t.Error("expected Stop to stop discovery")
	}
}

func TestWatcherRestartsDiscoveryWhenDropped(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	w, err := newWatcher(ctx, bus, "", vin)
	if err != nil {
		t.Fatalf("newWatcher: %v", err)
	}
	defer w.Stop(ctx)

	bus.discovering = false
	if _, err := w.Peek(ctx); err != nil {
		t.Fatalf("Peek after discovery dropped: %v", err)
	}
	if !bus.discovering {
		t.Error("expected Peek to start discovery again after bluetoothd dropped it")
	}
	if n := countCalls(bus.calls, adapterIface+".StartDiscovery"); n != 2 {
		t.Errorf("StartDiscovery called %d times, want 2 (once at Watch, once after drop)", n)
	}
}

func TestWatcherRepowersAdapterOnPeek(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	w, err := newWatcher(ctx, bus, "", vin)
	if err != nil {
		t.Fatalf("newWatcher: %v", err)
	}
	defer w.Stop(ctx)

	bus.powered = false
	bus.discovering = false
	if _, err := w.Peek(ctx); err != nil {
		t.Fatalf("Peek after adapter powered off: %v", err)
	}
	if !bus.powered {
		t.Error("expected Peek to power the adapter back on")
	}
	if !bus.discovering {
		t.Error("expected Peek to restart discovery after powering the adapter")
	}
}

func TestWatcherWaitFindsDelayedBeacon(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin), rssi: -50}
	bus.deviceVisible = false
	bus.deviceAppearCall = 3

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	w, err := newWatcher(ctx, bus, "", vin)
	if err != nil {
		t.Fatalf("newWatcher: %v", err)
	}
	defer w.Stop(ctx)

	res, err := w.Wait(ctx)
	if err != nil {
		t.Fatalf("Wait: %v", err)
	}
	if res == nil {
		t.Fatal("Wait returned nil after the beacon appeared")
	}
	if res.RSSI != -50 {
		t.Errorf("res.RSSI = %d, want -50", res.RSSI)
	}
}

func TestWatcherWaitTimeoutIsNotAnError(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"
	bus.dev = &fakeDevice{path: bus.devPath(), name: vehicleBeaconName(vin)}
	bus.deviceVisible = false

	wctx, wcancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer wcancel()
	w, err := newWatcher(wctx, bus, "", vin)
	if err != nil {
		t.Fatalf("newWatcher: %v", err)
	}
	defer w.Stop(wctx)

	waitCtx, cancel := context.WithTimeout(context.Background(), 150*time.Millisecond)
	defer cancel()
	res, err := w.Wait(waitCtx)
	if err != nil {
		t.Fatalf("Wait on timeout: %v", err)
	}
	if res != nil {
		t.Fatalf("Wait should return nil when no beacon appears, got %+v", res)
	}
}

func TestWatcherHonorsSpecificAdapter(t *testing.T) {
	bus := newFakeBluez()
	vin := "5YJ3E1EA0PF000000"

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	if _, err := newWatcher(ctx, bus, "hci9", vin); err == nil {
		t.Fatal("newWatcher with nonexistent adapter should fail")
	}
}
