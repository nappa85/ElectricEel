const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");

// Production-review regressions for the QML UI layer. Each test executes the
// real QML/JS sources (no copies of the buggy logic) and asserts the correct
// behavior, retaining coverage for previously fixed defects.

const qmlRoot = path.join(__dirname, "../qml");
const catalogSource = fs.readFileSync(path.join(qmlRoot, "js/CommandCatalog.js"), "utf8");
const dialogSource = fs.readFileSync(path.join(qmlRoot, "pages/ArgumentDialog.qml"), "utf8");
const firstPageSource = fs.readFileSync(path.join(qmlRoot, "pages/FirstPage.qml"), "utf8");

function loadCatalog() {
    const context = vm.createContext({ qsTr: (s) => s });
    vm.runInContext(catalogSource.replace(/^\.pragma library\s*$/m, ""), context);
    return context;
}

function extractFunction(source, name) {
    const re = new RegExp(`function ${name}\\([^)]*\\) \\{([\\s\\S]*?)\\n    \\}`);
    const m = source.match(re);
    assert.ok(m, `${name} must be discoverable in source`);
    return m[0];
}

// --- 1. toggleLock with unknown state sends unlock but shows locked --------
// emptyStatus().locked is null. Previously, the optimistic flip painted a
// closed padlock for an unlock command. Displayed state must match intent.
test("toggleLock with unknown state must display the unlock it sends", () => {
    const toggleLock = extractFunction(firstPageSource, "toggleLock");
    // optimistic() is kept as a deprecated wrapper; the fixed toggleLock
    // uses setOptimistic(). Provide both so the test runs the live code path.
    let optimistic = "";
    try {
        optimistic = extractFunction(firstPageSource, "optimistic");
    } catch {}
    let setOptimistic = "";
    try {
        setOptimistic = extractFunction(firstPageSource, "setOptimistic");
    } catch {}
    const context = vm.createContext({
        page: { vehicleStatus: { locked: null }, statusStage: "", statusSeq: 0,
            statusInstance: "test", statusRequestId: "", vin: "VIN" },
        VState: { clone: (s) => ({ ...s }) },
        teslaClient: { runCommand: function (id, cmd) { this.last = cmd; } },
    });
    vm.runInContext(`${extractFunction(firstPageSource, "runStatus")}\n${setOptimistic}\n${optimistic}\n${toggleLock}`, context);
    vm.runInContext("toggleLock()", context);
    const sent = vm.runInContext("teslaClient.last", context);
    const shown = vm.runInContext("page.vehicleStatus.locked", context);
    assert.equal(sent, "unlock", "unknown state must send unlock");
    assert.equal(
        shown,
        false,
        `displayed locked=${shown} contradicts sent ${sent}`,
    );
});

// --- 2. Slider valueText truncates coordinate precision --------------------
// chargeScheduleAddArgs LATITUDE/LONGITUDE use step 0.000001. The dialog's
// sliderDecimals(step) helper must yield enough decimals to preserve a real
// coordinate (48.8584) through display -> parse.
test("coordinate slider display must round-trip the sent value", () => {
    const fn = extractFunction(dialogSource, "sliderDecimals");
    const context = vm.createContext({ Math });
    vm.runInContext(`${fn}\nthis.__dec = sliderDecimals(0.000001);`, context);
    const decimals = vm.runInContext("__dec", context);
    const value = 48.8584;
    const displayed = value.toFixed(decimals);
    assert.equal(
        Number(displayed),
        value,
        `step 0.000001 renders "${displayed}" for ${value}`,
    );
    // Coarse steps keep coarse labels.
    vm.runInContext("this.__dec05 = sliderDecimals(0.5);", context);
    assert.equal(vm.runInContext("__dec05", context), 1, "step 0.5 must yield 1 decimal");
});

// --- 3. schedule-remove TYPE=id accepts an empty ID ------------------------
// ID is optional; previously Dialog.revalidate() passed with TYPE=id and
// ID="". The Go handler then rejected ["id"] with "missing schedule ID"
// after a BLE round-trip. The test runs the live revalidate() and requires
// the form to be invalid for that combination.
test("schedule-remove with TYPE=id and empty ID must be invalid", () => {
    const revalidate = extractFunction(dialogSource, "revalidate");
    const ctx = loadCatalog();
    const args = vm.runInContext("scheduleRemoveArgs()", ctx);
    args.find((a) => a.name === "TYPE").__value = "id";
    args.find((a) => a.name === "ID").__value = "";
    const context = vm.createContext({
        commandDef: { args },
        dialog: { formValid: true },
    });
    // The QML function assigns to bare `formValid`; alias it into scope.
    vm.runInContext(`var formValid = true;\n${revalidate}\nrevalidate();\nthis.__result = formValid;`, context);
    const valid = vm.runInContext("__result", context);
    assert.equal(
        valid,
        false,
        "TYPE=id with an empty ID must not validate",
    );
});

// --- 4. Battery icon boundaries --------------------------------------------
// Previously strict comparisons rendered 90% with the 75 icon. Tier tops
// must belong to the higher icon.
test("battery icon tiers include their boundary values", () => {
    const fn = extractFunction(firstPageSource, "batteryIconSource");
    function liveIcon(level) {
        const context = vm.createContext({ page: { vehicleStatus: { batteryLevel: level } } });
        vm.runInContext(`${fn}\nthis.__icon = batteryIconSource();`, context);
        return vm.runInContext("__icon", context);
    }
    assert.ok(firstPageSource.includes("batteryIconSource"), "batteryIconSource must exist");
    assert.equal(
        liveIcon(90),
        "../../img/icons/battery100.svg",
        "90% must render the full icon",
    );
    assert.equal(
        liveIcon(60),
        "../../img/icons/battery75.svg",
        "60% must render the 75 icon",
    );
});
