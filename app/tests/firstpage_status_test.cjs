const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");
const { loadFunctions, runHandler } = require("./qml_helpers.cjs");

const root = path.join(__dirname, "../qml");
const source = fs.readFileSync(path.join(root, "pages/FirstPage.qml"), "utf8");

function dashboard() {
    const requests = [];
    const context = vm.createContext({ qsTr: (text) => text, console });
    vm.runInContext(fs.readFileSync(path.join(root, "js/VehicleState.js"), "utf8")
        .replace(/^\.pragma library\s*$/m, ""), context);
    context.VState = context;
    context.page = {
        vin: "VIN A", hasKey: true, vehicleStatus: context.emptyStatus(),
        statusBeforeToggle: null, statusStage: "", statusRequestId: "", statusError: "",
        statusSeq: 0, statusInstance: "test",
    };
    context.teslaClient = {
        helperAvailable: true,
        runCommand: (id, command, args) => requests.push({ id, command, args }),
    };
    loadFunctions(source, ["runStatus", "refreshStatus"], context);
    return { context, requests };
}

test("refresh replies remain single-flight and duplicate replies cannot advance the chain", () => {
    const { context, requests } = dashboard();
    context.refreshStatus();
    const firstId = context.page.statusRequestId;
    for (const [stage, payload] of [
        ["body", '{"vehicleLockState":"VEHICLELOCKSTATE_LOCKED"}'],
        ["closures", '{"closuresState":{"locked":true}}'],
        ["climate", '{"climateState":{"insideTempCelsius":21}}'],
        ["charge", '{"chargeState":{"batteryLevel":80}}'],
    ]) {
        assert.equal(context.page.statusStage, stage);
        context.requestId = context.page.statusRequestId;
        context.ok = true;
        context.stdOut = payload;
        runHandler(source, "onCommandFinished", context);
        if (stage === "body") {
            context.requestId = firstId;
            runHandler(source, "onCommandFinished", context);
            assert.equal(requests.length, 2, "duplicate body reply queued another request");
        }
    }
    assert.equal(requests.length, 4);
    assert.equal(context.page.statusStage, "");
    assert.equal(context.page.vehicleStatus.batteryLevel, 80);
    assert.equal(context.page.vehicleStatus.insideTemp, 21);
});

test("late success and failure for a previous VIN cannot modify a new refresh", () => {
    const { context, requests } = dashboard();
    context.refreshStatus();
    const oldId = context.page.statusRequestId;
    context.vin = "VIN B";
    context.model = "modely";
    context.hasKey = true;
    runHandler(source, "onConfigLoaded", context);
    const newId = context.page.statusRequestId;
    assert.notEqual(newId, oldId);
    context.requestId = oldId;
    context.ok = true;
    context.stdOut = '{"vehicleLockState":"VEHICLELOCKSTATE_UNLOCKED"}';
    runHandler(source, "onCommandFinished", context);
    context.message = "old vehicle transport error";
    runHandler(source, "onCommandError", context);
    assert.equal(context.page.statusRequestId, newId);
    assert.equal(context.page.statusStage, "body");
    assert.equal(context.page.vehicleStatus.locked, null);
    assert.equal(context.page.statusError, "");
    assert.equal(requests.length, 2);
});
