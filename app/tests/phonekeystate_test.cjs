const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const test = require("node:test");

const state = vm.createContext({});
vm.runInContext(fs.readFileSync(path.join(__dirname, "../qml/js/PhoneKeyState.js"), "utf8")
    .replace(/^\.pragma library\s*$/m, ""), state);

test("cover maps machine states, including future unknown values", () => {
    for (const link of ["connected", "authorized"])
        assert.equal(state.connectionKind(link, true), "connected");
    for (const link of ["scanning", "stopped", "error", "future-state", ""])
        assert.equal(state.connectionKind(link, true), "disconnected");
    assert.equal(state.connectionKind("bluetooth-off", true), "bluetooth-off");
    assert.equal(state.connectionKind("unpaired", true), "unpaired");
    assert.equal(state.connectionKind("connected", false), "unpaired");
    assert.equal(state.isError("bluetooth-off"), true);
    assert.equal(state.isError("error"), true);
    assert.equal(state.isError("scanning"), false);
});

test("diagnostic prose is not interpreted by cover or pairing badge", () => {
    const cover = fs.readFileSync(path.join(__dirname, "../qml/cover/CoverPage.qml"), "utf8");
    const pairing = fs.readFileSync(path.join(__dirname, "../qml/pages/PairingPage.qml"), "utf8");
    assert.match(cover, /PhoneKey\.connectionKind/);
    assert.doesNotMatch(cover, /phoneKeyStatus/);
    assert.match(pairing, /PhoneKey\.isError\(teslaClient\.phoneKeyLink\)/);
    assert.doesNotMatch(pairing, /phoneKeyStatus\.indexOf/);
});
