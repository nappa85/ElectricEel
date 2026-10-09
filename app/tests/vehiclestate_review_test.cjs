const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");

// Regression tests for VehicleState.js merge* functions.
//
// Bug: mergeClosuresState / mergeClimateState / mergeChargeState treat a
// missing wrapper object as "all zero values" instead of "no data", so an
// error/empty payload wipes a previously known good reading:
//
//   JSON.parse('{"error":"x"}').closuresState || {}  ->  {}
//   s.locked = !!undefined  ->  false
//
// A known locked=true becomes false (hides the Unlock row, flips the
// padlock), a known isClimateOn=true becomes false, a known batteryLevel=80
// becomes null. mergeBodyControllerState already guards this correctly (it
// only touches closure fields when closureStatuses is present); the three
// Infotainment merges must do the same: return the previous status
// untouched when their wrapper is absent.

const qmlRoot = path.join(__dirname, "../qml");
const vehicleStateSource = fs.readFileSync(path.join(qmlRoot, "js/VehicleState.js"), "utf8");

function loadVehicleState() {
    const context = vm.createContext({ qsTr: (s) => s, console, Date });
    vm.runInContext(vehicleStateSource.replace(/^\.pragma library\s*$/m, ""), context);
    return context;
}

test("mergeClosuresState preserves known lock state when wrapper is missing", () => {
    const ctx = loadVehicleState();
    vm.runInContext("var base = emptyStatus(); base.locked = true; base.doorsOpen = true;", ctx);
    const out = vm.runInContext(
        'mergeClosuresState(base, JSON.stringify({error: "ble timeout"}))',
        ctx,
    );
    assert.equal(
        out.locked,
        true,
        "an error payload without closuresState must not flip a known locked=true to false",
    );
});

test("mergeClimateState preserves known climate state when wrapper is missing", () => {
    const ctx = loadVehicleState();
    vm.runInContext("var base = emptyStatus(); base.isClimateOn = true; base.insideTemp = 21.5;", ctx);
    const out = vm.runInContext('mergeClimateState(base, "{}")', ctx);
    assert.equal(
        out.isClimateOn,
        true,
        "an empty payload without climateState must not flip a known isClimateOn=true to false",
    );
});

test("mergeChargeState preserves known charge state when wrapper is missing", () => {
    const ctx = loadVehicleState();
    vm.runInContext(
        'var base = emptyStatus(); base.batteryLevel = 80; base.chargingState = "Charging";',
        ctx,
    );
    const out = vm.runInContext('mergeChargeState(base, "{}")', ctx);
    assert.equal(
        out.batteryLevel,
        80,
        "an empty payload without chargeState must not clear a known batteryLevel",
    );
    assert.equal(
        out.chargingState,
        "Charging",
        "an empty payload without chargeState must not clear a known chargingState",
    );
});

test("fresh body telemetry cannot refresh older climate and battery timestamps", () => {
    let now = 1000;
    const ctx = vm.createContext({ qsTr: (s) => s, console, Date: { now: () => now } });
    vm.runInContext(vehicleStateSource.replace(/^\.pragma library\s*$/m, ""), ctx);
    let status = ctx.mergeChargeState(ctx.emptyStatus(), '{"chargeState":{"batteryLevel":80}}');
    now = 2000;
    status = ctx.mergeClimateState(status, '{"climateState":{"insideTempCelsius":21}}');
    now = 3000;
    status = ctx.mergeBodyControllerState(status, '{"vehicleLockState":"VEHICLELOCKSTATE_LOCKED"}');
    assert.equal(status.bodyUpdatedAt, 3000);
    assert.equal(status.climateUpdatedAt, 2000);
    assert.equal(status.chargeUpdatedAt, 1000);
    assert.equal(status.updatedAt, 1000);
    status = ctx.mergeChargeState(status, '{}');
    assert.equal(status.chargeUpdatedAt, 1000, "missing category must preserve its real age");
    now = 4000;
    status = ctx.mergeChargeState(status, '{"chargeState":{"batteryLevel":81}}');
    assert.equal(status.updatedAt, 2000, "overall age advances only as the oldest category is refreshed");
});
