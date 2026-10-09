import QtQuick 2.6
import Sailfish.Silica 1.0

// Builds an input form from a CommandCatalog command's `args` array and,
// on accept, fills `values` with one string per arg (in order) ready to
// pass straight to TeslaClient.runCommand(). One dialog handles every
// tesla-control subcommand's argument shape instead of one per command.
Dialog {
    id: dialog

    property var commandDef
    property var values: []
    property bool formValid: false

    // Recompute formValid. Called from each field's value handlers and from
    // Component.onCompleted. It cannot be a plain binding expression because
    // QML does not track plain JS object properties (argSpec.__value), so a
    // canAccept binding would be evaluated once and never again, leaving the
    // Run button permanently disabled.
    function revalidate() {
        if (!commandDef) {
            formValid = false
            return
        }
        for (var i = 0; i < commandDef.args.length; i++) {
            var a = commandDef.args[i]
            if (!a.optional && (!a.__value || a.__value.length === 0)) {
                formValid = false
                return
            }
        }
        // Cross-field rule: schedule-remove TYPE=id requires a numeric ID.
        // ID is optional in the catalog (omitted for home/work/other), so
        // the loop above passes TYPE=id with ID=""; the backend would then
        // reject ["id"] with "missing schedule ID" after a BLE round-trip.
        for (var j = 0; j < commandDef.args.length; j++) {
            if (commandDef.args[j].name === "TYPE"
                    && commandDef.args[j].__value === "id") {
                for (var k = 0; k < commandDef.args.length; k++) {
                    if (commandDef.args[k].name === "ID"
                            && (!commandDef.args[k].__value
                                || commandDef.args[k].__value.length === 0)) {
                        formValid = false
                        return
                    }
                }
            }
        }
        formValid = true
    }

    // Decimals for a slider step so the label round-trips the sent value:
    // step 0.5 -> 1 decimal, step 1e-6 -> 6 decimals. The old
    // `step < 1 ? 1 : 0` showed 48.9 for a 48.8584 send.
    function sliderDecimals(step) {
        if (!step)
            return 0
        return Math.max(0, Math.ceil(-Math.log(step) / Math.LN10))
    }

    canAccept: dialog.formValid

    Component.onCompleted: dialog.revalidate()

    DialogHeader {
        title: commandDef ? commandDef.label : ""
        acceptText: qsTr("Run")
    }

    SilicaFlickable {
        anchors.fill: parent
        contentHeight: column.height + Theme.paddingLarge

        Column {
            id: column
            width: parent.width
            spacing: Theme.paddingMedium
            anchors.top: parent.top
            anchors.topMargin: Theme.itemSizeLarge + Theme.paddingMedium

            Repeater {
                model: commandDef ? commandDef.args : []

                delegate: Loader {
                    width: column.width
                    active: !modelData.hidden
                    property var argSpec: modelData
                    sourceComponent: {
                        if (modelData.type === "enum") return enumField
                        if (modelData.type === "pin") return pinField
                        // Sliders always have *some* value under the thumb,
                        // so an optional numeric field can't use one - it
                        // would always send a value, never really skip it
                        // (see textField's onAccepted skip-when-empty logic,
                        // which a slider can never trigger). Fall back to
                        // free text, which starts empty unless a def is set.
                        if ((modelData.type === "int" || modelData.type === "float")
                                && modelData.min !== undefined && modelData.max !== undefined
                                && !modelData.optional)
                            return sliderField
                        return textField
                    }
                    onLoaded: {
                        // Loader completes the item before assigning argSpec.
                        // Initialize the payload here too: an unchanged enum
                        // index or zero-valued slider emits no value change.
                        var value = ""
                        if (argSpec.type === "enum" && !argSpec.optional && argSpec.values.length)
                            value = argSpec.values[0]
                        else if (argSpec.def !== undefined)
                            value = String(argSpec.def)
                        else if (!argSpec.optional && argSpec.min !== undefined)
                            value = String(argSpec.min)
                        if (!argSpec.optional && (argSpec.type === "int" || argSpec.type === "float"))
                            value += argSpec.sendSuffix || ""
                        argSpec.__value = value
                        item.argSpec = argSpec
                        dialog.revalidate()
                    }
                }
            }
        }
    }

    Component {
        id: enumField
        ComboBox {
            property var argSpec
            // Optional enums get a real "not set" choice at index 0, whose
            // __value is "" so the backend retains its default while later
            // arguments keep their positions. Without this, an optional enum always
            // has *some* selected value (menus have no "nothing selected"
            // state) and so was never actually skippable.
            property var choices: argSpec ? (argSpec.optional ? [""].concat(argSpec.values) : argSpec.values) : []
            label: argSpec ? (argSpec.name + (argSpec.optional ? qsTr(" (optional)") : "")) : ""
            menu: ContextMenu {
                Repeater {
                    model: choices
                    MenuItem { text: modelData === "" ? qsTr("(not set)") : modelData }
                }
            }
            onCurrentIndexChanged: {
                if (argSpec && choices.length) argSpec.__value = choices[currentIndex]
                dialog.revalidate()
            }
            Component.onCompleted: {
                if (argSpec && choices.length) argSpec.__value = choices[0]
                dialog.revalidate()
            }
        }
    }

    Component {
        id: pinField
        TextField {
            property var argSpec
            label: argSpec ? argSpec.name : ""
            echoMode: TextInput.Password
            inputMethodHints: Qt.ImhDigitsOnly
            onTextChanged: {
                if (argSpec) argSpec.__value = text
                dialog.revalidate()
            }
        }
    }

    Component {
        id: sliderField
        Column {
            property var argSpec
            width: parent.width
            spacing: Theme.paddingSmall

            // Field title on its own row instead of the Slider's built-in
            // label: Silica lays the built-in label and valueText out on the
            // same line at the top of the Slider, where a long title runs
            // straight into the value. A separate Label above the Slider
            // keeps the two apart no matter how wide the title is.
            Label {
                width: parent.width
                wrapMode: Text.Wrap
                font.pixelSize: Theme.fontSizeExtraSmall
                color: Theme.secondaryColor
                text: argSpec ? (argSpec.name + (argSpec.unit ? (" (" + argSpec.unit + ")") : "")) : ""
            }

            Slider {
                width: parent.width
                minimumValue: argSpec ? argSpec.min : 0
                maximumValue: argSpec ? argSpec.max : 100
                stepSize: argSpec && argSpec.step ? argSpec.step : 1
                value: argSpec && argSpec.def !== undefined ? argSpec.def : minimumValue
                // Decimals derive from the step so the label round-trips the
                // sent value (see sliderDecimals above).
                valueText: value.toFixed(dialog.sliderDecimals(argSpec ? argSpec.step : 0))
                // sendSuffix is for values tesla-control wants glued directly to
                // the number with no space (e.g. "21C", "600s") - distinct from
                // `unit`, which is display-only text shown in the label (e.g.
                // "°C") and would be invalid if sent as-is.
                onValueChanged: {
                    if (argSpec) argSpec.__value = value.toString() + (argSpec.sendSuffix || "")
                    dialog.revalidate()
                }
                Component.onCompleted: {
                    if (argSpec) argSpec.__value = value.toString() + (argSpec.sendSuffix || "")
                    dialog.revalidate()
                }
            }
        }
    }

    Component {
        id: textField
        TextField {
            property var argSpec
            label: argSpec ? (argSpec.name + (argSpec.optional ? qsTr(" (optional)") : "")) : ""
            placeholderText: argSpec && argSpec.placeholder ? argSpec.placeholder : ""
            text: argSpec && argSpec.def !== undefined ? String(argSpec.def) : ""
            onTextChanged: {
                if (argSpec) argSpec.__value = text
                dialog.revalidate()
            }
            Component.onCompleted: {
                if (argSpec) argSpec.__value = text
                dialog.revalidate()
            }
        }
    }

    onAccepted: {
        var out = []
        for (var i = 0; i < commandDef.args.length; i++) {
            var a = commandDef.args[i]
            var v = a.__value !== undefined ? a.__value : ""
            out.push(v)
        }
        while (out.length > 0 && commandDef.args[out.length - 1].optional
               && out[out.length - 1] === "")
            out.pop()
        values = out
    }
}
