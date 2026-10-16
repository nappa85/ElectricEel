const assert = require("node:assert/strict");
const test = require("node:test");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const { loadLibrary, loadFunctions } = require("./qml_helpers.cjs");

const tapConfirm = loadLibrary("TapConfirm.js");

test("a quick action runs only on the second tap of the same action", () => {
    let state = tapConfirm.tap("", "unlock");
    assert.equal(state.run, "");
    assert.equal(state.armed, "unlock");
    // A different action re-arms instead of running either.
    state = tapConfirm.tap(state.armed, "honk");
    assert.equal(state.run, "");
    assert.equal(state.armed, "honk");
    state = tapConfirm.tap(state.armed, "honk");
    assert.equal(state.run, "honk");
    assert.equal(state.armed, "");
});

test("cover actions need a confirming second tap", () => {
    const source = fs.readFileSync(path.join(__dirname, "../qml/cover/CoverPage.qml"), "utf8");
    const runs = [];
    const timer = { restarts: 0, stops: 0, restart() { this.restarts++; }, stop() { this.stops++; } };
    const cover = { canCommand: true, commandBusy: false, armedAction: "",
                    runCoverCommand: (cmd) => runs.push(cmd) };
    const context = vm.createContext({ cover, armTimer: timer, TapConfirm: tapConfirm });
    loadFunctions(source, ["tapCoverAction"], context);
    context.tapCoverAction("unlock");
    assert.deepEqual(runs, []);
    assert.equal(cover.armedAction, "unlock");
    assert.equal(timer.restarts, 1);
    context.tapCoverAction("unlock");
    assert.deepEqual(runs, ["unlock"]);
    assert.equal(cover.armedAction, "");
    assert.equal(timer.stops, 1);
    // Busy covers ignore taps entirely.
    cover.commandBusy = true;
    context.tapCoverAction("lock");
    assert.equal(cover.armedAction, "");
});

test("cover taps go through the confirmation; notification buttons run directly", () => {
    const cover = fs.readFileSync(path.join(__dirname, "../qml/cover/CoverPage.qml"), "utf8");
    assert.doesNotMatch(cover, /onTriggered:\s*cover\.runCoverCommand/);
    assert.match(cover, /onTriggered:\s*cover\.tapCoverAction\(cover\.leftAction\)/);
    assert.match(cover, /onTriggered:\s*cover\.tapCoverAction\(cover\.rightAction\)/);
    // Notification actions sit behind an expandable menu: one tap runs them,
    // and the notification is resident so lipstick keeps it afterwards.
    const notification = fs.readFileSync(path.join(__dirname, "../qml/StatusNotification.qml"), "utf8");
    assert.doesNotMatch(notification, /TapConfirm/);
    assert.match(notification, /setHintValue\("resident", true\)/);
});
