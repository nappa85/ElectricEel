const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");
const { runHandler } = require("./qml_helpers.cjs");

test("failed config loads preserve settings fields and keep Save disabled", () => {
    const source = fs.readFileSync(path.join(__dirname, "../qml/pages/SettingsPage.qml"), "utf8");
    const context = vm.createContext({
        page: { configReady: false, configLoading: true, statusText: "Loading" },
        vinField: { text: "saved VIN" },
        keyNameField: { text: "saved key name" },
        message: "Request queue full",
    });
    runHandler(source, "onConfigLoadError", context);
    assert.equal(context.page.configReady, false);
    assert.equal(context.page.configLoading, false);
    assert.equal(context.page.statusText, "Request queue full");
    assert.equal(context.vinField.text, "saved VIN");
    assert.equal(context.keyNameField.text, "saved key name");
});
