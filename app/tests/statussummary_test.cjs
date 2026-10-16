const assert = require("node:assert/strict");
const test = require("node:test");
const fs = require("node:fs");
const path = require("node:path");
const { loadLibrary } = require("./qml_helpers.cjs");

const summary = loadLibrary("StatusSummary.js");
const catalog = loadLibrary("CommandCatalog.js");
const coverActions = loadLibrary("CoverActions.js");
const labels = {
    connected: "Connected", disconnected: "Not connected", bluetoothOff: "Bluetooth off",
    unpaired: "Not paired", charging: "Charging", locked: "Locked", unlocked: "Unlocked",
};

test("status line lists only known facts, compactly", () => {
    assert.deepEqual(Array.from(summary.statusParts("disconnected", null, labels, catalog.nearlyCharging)),
                     ["Not connected"]);
    const status = { batteryLevel: 78, chargingState: "Charging", locked: true };
    assert.deepEqual(Array.from(summary.statusParts("connected", status, labels, catalog.nearlyCharging)),
                     ["Connected", "78%", "Charging", "Locked"]);
    const unknown = { batteryLevel: null, chargingState: "Disconnected", locked: null };
    assert.deepEqual(Array.from(summary.statusParts("bluetooth-off", unknown, labels, catalog.nearlyCharging)),
                     ["Bluetooth off"]);
    assert.deepEqual(Array.from(summary.statusParts("connected", { batteryLevel: 0, locked: false }, labels,
                                                    catalog.nearlyCharging)),
                     ["Connected", "0%", "Unlocked"]);
});

test("notification offers only known actions, once each, in order", () => {
    assert.deepEqual(Array.from(summary.offeredActions(["honk", "rm -rf", "lock", "honk"], coverActions.ACTIONS)),
                     ["honk", "lock"]);
    assert.deepEqual(Array.from(summary.offeredActions(null, coverActions.ACTIONS)), []);
});

test("D-Bus quick actions are restricted to the offered actions", () => {
    const source = fs.readFileSync(path.join(__dirname, "../qml/StatusNotification.qml"), "utf8");
    const body = source.match(/function runAction\(id\) \{([\s\S]*?)\n    \}/);
    assert.ok(body, "runAction must be discoverable");
    assert.match(body[1], /actionIds\.indexOf\(id\) < 0\)\s*return/);
});
