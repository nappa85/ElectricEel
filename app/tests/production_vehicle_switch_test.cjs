const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");
const { loadFunctions, runHandler } = require("./qml_helpers.cjs");

test("switching VIN clears previous vehicle telemetry before choosing a toggle command", () => {
    const root = path.join(__dirname, "../qml");
    const source = fs.readFileSync(path.join(root, "pages/FirstPage.qml"), "utf8");
    const context = vm.createContext({ qsTr: (text) => text, console });
    vm.runInContext(fs.readFileSync(path.join(root, "js/VehicleState.js"), "utf8")
        .replace(/^\.pragma library\s*$/m, ""), context);
    context.VState = {
        emptyStatus: context.emptyStatus,
        clone: context.clone,
    };
    const oldStatus = context.emptyStatus();
    oldStatus.locked = false;
    oldStatus.batteryLevel = 83;
    oldStatus.updatedAt = Date.now();
    context.page = {
        vin: "5YJ3E1EA0PF000000", model: "", hasKey: true,
        vehicleStatus: oldStatus, statusStage: "", statusBeforeToggle: null,
        refreshStatus: () => {},
    };
    const commands = [];
    context.teslaClient = { runCommand: (_id, cmd) => commands.push(cmd) };
    loadFunctions(source, ["runStatus", "setOptimistic", "toggleLock"], context);
    const handler = source.match(/onConfigLoaded:\s*\{([\s\S]*?)\n        \}/);
    assert.ok(handler, "onConfigLoaded must be discoverable");
    context.vin = "7SAYGDEE0PF000001";
    context.model = "modely";
    context.hasKey = true;
    vm.runInContext(`(function() { ${handler[1]} })()`, context);
    assert.equal(context.page.vehicleStatus.locked, null, "new VIN inherited old car's unlocked state");
    assert.equal(context.page.vehicleStatus.batteryLevel, null, "new VIN inherited old car's battery reading");
    assert.equal(context.page.vehicleStatus.updatedAt, 0, "new VIN inherited old car's freshness timestamp");
    context.requestId = "status:body#old-vehicle";
    context.ok = true;
    context.stdOut = '{}';
    runHandler(source, "onCommandFinished", context);
    context.message = "stale failure";
    runHandler(source, "onCommandError", context);
    assert.equal(context.page.statusStage, "", "old replies must not start a new refresh chain");
    assert.equal(context.page.statusError, "", "old errors must not overwrite the new VIN's status");
    vm.runInContext("toggleLock()", context);
    assert.deepEqual(commands, ["unlock"], "unknown new-car lock state must use the dashboard's initial unlock behavior");
});
