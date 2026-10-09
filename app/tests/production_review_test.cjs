const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");
const { loadFunctions, runHandler } = require("./qml_helpers.cjs");

// Execute production handlers and assert outcomes rather than requiring a
// particular guard, formatting method, or request-ID implementation.
const qmlRoot = path.join(__dirname, "../qml");
const categorySource = fs.readFileSync(path.join(qmlRoot, "pages/CategoryPage.qml"), "utf8");
const navigationSource = fs.readFileSync(path.join(qmlRoot, "pages/NavigationPage.qml"), "utf8");
const dialogSource = fs.readFileSync(path.join(qmlRoot, "pages/ArgumentDialog.qml"), "utf8");
const firstPageSource = fs.readFileSync(path.join(qmlRoot, "pages/FirstPage.qml"), "utf8");

test("same-command requests from different pages have distinct correlation IDs", () => {
    const fixedClock = { now: () => 12345 };
    for (const kind of ["category", "navigation"]) {
        const ids = [];
        for (let i = 0; i < 2; i++) {
            const context = vm.createContext({
                Date: fixedClock,
                page: { requestSeq: 0, navSeq: 0, sending: false },
                destField: { text: "Destination" },
                teslaClient: {
                    runCommand: (id) => ids.push(id),
                    shareDestination: (id) => ids.push(id),
                },
            });
            if (kind === "category") {
                loadFunctions(categorySource, ["execute"], context);
                context.execute({ id: "lock", label: "Lock" }, []);
            } else {
                loadFunctions(navigationSource, ["send"], context);
                context.send();
            }
        }
        assert.equal(ids.length, 2);
        assert.notEqual(ids[0], ids[1], `${kind} pages collided in the same millisecond`);
    }
});

test("slider initialization and changes send exactly the displayed precision", () => {
    const slider = dialogSource.match(/Slider\s*\{([\s\S]*?)\n            \}/);
    assert.ok(slider);
    const label = slider[1].match(/valueText:\s*([^\n]+)/);
    assert.ok(label);
    for (const event of ["onValueChanged", "Component.onCompleted"]) {
        const handler = slider[1].match(new RegExp(`${event.replaceAll(".", "\\.")}:\\s*\\{([\\s\\S]*?)\\n                \\}`));
        assert.ok(handler);
        const context = vm.createContext({
            value: 48.858400000001,
            argSpec: { step: 0.000001, sendSuffix: "C" },
            dialog: { revalidate: () => {} },
        });
        loadFunctions(dialogSource, ["sliderDecimals"], context);
        context.dialog.sliderDecimals = context.sliderDecimals;
        const displayed = vm.runInContext(label[1], context);
        vm.runInContext(handler[1], context);
        assert.equal(context.argSpec.__value, displayed + "C");
        assert.equal(Number(displayed), 48.8584);
    }
});

test("a replacement share ignores the old preview and double send preserves correlation", () => {
    const requests = [];
    const page = {
        navSeq: 0, previewing: false, sending: false,
        pendingPreviewId: "", pendingSendId: "", resultText: "", previewText: "",
    };
    const context = vm.createContext({
        page, destField: { text: "" }, qsTr: (text) => text,
        teslaClient: {
            previewDestination: (id, text) => requests.push({ op: "preview", id, text }),
            shareDestination: (id, text) => requests.push({ op: "send", id, text }),
        },
    });
    loadFunctions(navigationSource, ["preview", "setSharedText", "send"], context);
    page.setSharedText("Old destination");
    const oldId = page.pendingPreviewId;
    page.setSharedText("New destination");
    assert.notEqual(page.pendingPreviewId, oldId);
    assert.equal(requests[1].text, "New destination");
    context.requestId = oldId;
    context.ok = false;
    runHandler(navigationSource, "onDestinationPreviewed", context);
    assert.equal(page.previewing, true);
    assert.equal(page.previewText, "Checking...");
    page.send();
    const sendId = page.pendingSendId;
    page.send();
    assert.equal(page.pendingSendId, sendId);
    assert.equal(requests.filter((request) => request.op === "send").length, 1);
});

for (const hardFailure of [false, true]) {
    test(`a failed dashboard toggle restores confirmed state (${hardFailure ? "transport" : "vehicle"} failure)`, () => {
        const confirmed = { locked: true };
        const page = {
            vin: "VIN", statusSeq: 0, statusInstance: "test", statusRequestId: "",
            vehicleStatus: confirmed, statusBeforeToggle: null, statusStage: "", statusError: "",
        };
        const context = vm.createContext({
            page, VState: { clone: (state) => ({ ...state }) },
            teslaClient: { runCommand: () => {} },
            ok: false, stdErr: "Vehicle refused", exitCode: 1, message: "Transport failed",
        });
        loadFunctions(firstPageSource, ["runStatus", "setOptimistic", "toggleLock"], context);
        context.toggleLock();
        assert.equal(page.vehicleStatus.locked, false);
        context.requestId = page.statusRequestId;
        runHandler(firstPageSource, hardFailure ? "onCommandError" : "onCommandFinished", context);
        assert.equal(page.vehicleStatus, confirmed);
        assert.equal(page.statusError, hardFailure ? "Transport failed" : "Vehicle refused");
        assert.equal(page.statusStage, "");
        assert.equal(page.statusBeforeToggle, null);
    });
}
