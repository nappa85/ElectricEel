const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");
const { loadFunctions, runHandler } = require("./qml_helpers.cjs");

function catalogue() {
    const context = vm.createContext({ qsTr: text => text });
    const source = fs.readFileSync(path.join(__dirname, "../qml/js/CoverActions.js"), "utf8");
    vm.runInContext(source.replace(/^\.pragma library\s*/, ""), context);
    return context;
}

test("cover catalogue matches the Rust enum and all custom icons exist", () => {
    const actions = catalogue();
    const rust = fs.readFileSync(path.join(__dirname, "../../helper/src/cover_action.rs"), "utf8");
    const variants = rust.match(/enum CoverAction \{([^}]+)\}/)[1];
    const ids = Array.from(variants.matchAll(/^\s*(\w+),/gm), match =>
        match[1].replace(/[A-Z]/g, (letter, offset) => (offset ? "-" : "") + letter.toLowerCase()));
    assert.deepEqual(Array.from(actions.ACTIONS, action => action.id), ids);
    for (const action of actions.ACTIONS) {
        if (!action.icon.startsWith("image://"))
            assert.ok(fs.existsSync(path.resolve(__dirname, "../qml/cover", action.icon)));
    }
    assert.equal(actions.action(null, 0).id, "frunk-open");
    assert.equal(actions.action("unknown", 1).id, "trunk-open");
});

test("native cover actions use loaded choices and serialize commands while busy", () => {
    const source = fs.readFileSync(path.join(__dirname, "../qml/cover/CoverPage.qml"), "utf8");
    const requests = [];
    const context = vm.createContext({
        CoverActions: catalogue(),
        cover: { vin: "VIN", canCommand: true, commandBusy: false, requestSeq: 0 },
        teslaClient: {
            runCommand: (...args) => requests.push(args),
            coverLeftAction: "flash-lights", coverRightAction: "unlock",
        },
        ok: true,
    });
    loadFunctions(source, ["runCoverCommand"], context);
    context.cover.runCoverCommand = context.runCoverCommand;
    // The cover binds its actions to TeslaClient's configuration properties.
    for (const side of ["leftAction", "rightAction"]) {
        const binding = source.match(new RegExp(`property string ${side}:\\s*([^\\n]+)`));
        assert.ok(binding, `${side} binding must be discoverable`);
        context.cover[side] = vm.runInContext(binding[1], context);
    }
    const triggers = Array.from(source.matchAll(/onTriggered:\s*([^\n]+)/g), match => match[1]);
    assert.equal(triggers.length, 2);
    vm.runInContext(triggers[0], context);
    vm.runInContext(triggers[1], context);
    assert.equal(requests.length, 1);
    assert.equal(requests[0][1], "flash-lights");
    context.requestId = requests[0][0];
    runHandler(source, "onCommandFinished", context);
    vm.runInContext(triggers[1], context);
    assert.equal(requests[1][1], "unlock");
});

test("settings loads cover choices without overwriting unsaved edits", () => {
    const source = fs.readFileSync(path.join(__dirname, "../qml/pages/SettingsPage.qml"), "utf8");
    const context = vm.createContext({
        CoverActions: catalogue(), qsTr: text => text,
        page: {},
        VState: { modelIndex: () => 0 },
        vinField: {}, modelField: {}, keyNameField: {},
        connectTimeoutSlider: {}, commandTimeoutSlider: {},
        coverLeftField: { currentIndex: 0 }, coverRightField: { currentIndex: 1 },
        vin: "VIN", model: "model3", keyName: "phone", hasKey: true,
        connectTimeoutSec: 20, commandTimeoutSec: 5,
        coverLeftAction: "honk", coverRightAction: "flash-lights",
    });
    runHandler(source, "onConfigLoaded", context);
    assert.equal(context.coverLeftField.currentIndex, 5);
    assert.equal(context.coverRightField.currentIndex, 4);
    context.page.coverLeftDirty = true;
    context.coverLeftField.currentIndex = 2;
    runHandler(source, "onConfigLoaded", context);
    assert.equal(context.coverLeftField.currentIndex, 2);
});
