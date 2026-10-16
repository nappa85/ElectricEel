import QtQuick 2.6
import Sailfish.Silica 1.0
import "../js/VehicleState.js" as VState
import "../js/CoverActions.js" as CoverActions
import "../js/CommandText.js" as CommandText

Page {
    id: page
    property var teslaClient

    property string statusText: qsTr("Loading current configuration...")
    // Set while onConfigLoaded is repopulating the form so programmatic
    // field updates are not mistaken for user edits.
    property bool loadingConfig: false
    // False until the first GetConfig reply actually lands. refreshConfig()
    // (below) is async - every field starts at its blank/default value, so
    // opening Settings and hitting Save before the reply arrives submitted
    // those defaults, including an empty VIN, and a blank VIN is treated as
    // "clear the VIN" further down the pipe (validate_config accepts it -
    // see helper/src/config.rs). That silently wiped a previously-configured
    // VIN. Gating Save on this makes that submission impossible instead of
    // just unlikely.
    property bool configReady: false
    property bool configLoading: true
    property string savedModel: ""
    property bool vinDirty: false
    property bool modelDirty: false
    property bool keyNameDirty: false
    property bool connectTimeoutDirty: false
    property bool commandTimeoutDirty: false
    property bool coverLeftDirty: false
    property bool coverRightDirty: false
    property bool vehicleNameDirty: false
    // Read/rename requests for the car name, and their outcome.
    property string nameRequestId: ""
    property string nameStatus: ""
    readonly property bool paired: teslaClient.hasKey && teslaClient.vin.length > 0

    function loadNotificationSettings() {
        var wasLoading = page.loadingConfig
        page.loadingConfig = true
        notificationSwitch.checked = teslaClient.notificationEnabled
        var actions = teslaClient.notificationActions
        for (var i = 0; i < notificationActionFields.count; i++) {
            var field = notificationActionFields.itemAt(i)
            if (field)
                field.currentIndex = i < actions.length ? CoverActions.indexOf(actions[i], -1) + 1 : 0
        }
        page.loadingConfig = wasLoading
    }

    // Notification settings take effect at once, like any Sailfish switch;
    // Rust echoes the saved state back through notificationSettingsChanged.
    function applyNotificationSettings() {
        if (page.loadingConfig)
            return
        teslaClient.setNotificationSettings(notificationSwitch.checked, page.chosenNotificationActions())
    }

    function chosenNotificationActions() {
        var actions = []
        for (var i = 0; i < notificationActionFields.count; i++) {
            var field = notificationActionFields.itemAt(i)
            if (field && field.currentIndex > 0)
                actions.push(CoverActions.ACTIONS[field.currentIndex - 1].id)
        }
        return actions
    }

    function runNameCommand(cmd, args, pendingText) {
        page.nameStatus = pendingText
        page.nameRequestId = teslaClient.newRequestId("settings:" + cmd, teslaClient.vin)
        teslaClient.runCommand(page.nameRequestId, cmd, args, false)
    }

    Connections {
        target: teslaClient
        onConfigLoaded: {
            page.loadingConfig = true
            if (!page.vinDirty && !vinField.activeFocus)
                vinField.text = vin
            if (!page.modelDirty && !modelField.activeFocus) {
                page.savedModel = model
                modelField.currentIndex = VState.modelIndex(model)
            }
            if (!page.keyNameDirty && !keyNameField.activeFocus)
                keyNameField.text = keyName
            if (!page.connectTimeoutDirty && !connectTimeoutSlider.activeFocus)
                connectTimeoutSlider.value = connectTimeoutSec
            if (!page.commandTimeoutDirty && !commandTimeoutSlider.activeFocus)
                commandTimeoutSlider.value = commandTimeoutSec
            if (!page.coverLeftDirty && !coverLeftField.activeFocus)
                coverLeftField.currentIndex = CoverActions.indexOf(coverLeftAction, 0)
            if (!page.coverRightDirty && !coverRightField.activeFocus)
                coverRightField.currentIndex = CoverActions.indexOf(coverRightAction, 1)
            page.loadingConfig = false
            page.configReady = true
            page.configLoading = false
            page.statusText = hasKey ? qsTr("Key on file") : qsTr("No key yet - use Pair Vehicle from the main menu")
        }
        onConfigLoadError: {
            page.configLoading = false
            page.configReady = false
            page.statusText = message
        }
        onConfigSaved: {
            page.statusText = ok ? qsTr("Saved") : qsTr("Save failed: %1").arg(errorMessage)
            if (!ok)
                return
            // The saved values are now the configuration: let the reload show
            // what Rust normalized (trimmed VIN/key name) and follow later
            // changes again.
            page.vinDirty = false
            page.modelDirty = false
            page.keyNameDirty = false
            page.connectTimeoutDirty = false
            page.commandTimeoutDirty = false
            page.coverLeftDirty = false
            page.coverRightDirty = false
            teslaClient.refreshConfig()
        }
        onVehicleNameChanged: {
            if (!vehicleNameField.activeFocus) {
                vehicleNameField.text = teslaClient.vehicleName
                page.vehicleNameDirty = false
            }
        }
        onNotificationSettingsChanged: page.loadNotificationSettings()
        onCommandFinished: {
            if (requestId !== page.nameRequestId)
                return
            page.nameRequestId = ""
            page.nameStatus = ok ? qsTr("Done") : CommandText.failure(stdErr, exitCode)
        }
        onCommandError: {
            if (requestId !== page.nameRequestId)
                return
            page.nameRequestId = ""
            page.nameStatus = message
        }
        onAutomagicExportFinished: {
            page.statusText = ok
                ? qsTr("Exported to %1. In Automagic, use States → Import to select this folder, then Reload Daemon.").arg(path)
                : qsTr("Automagic export failed: %1").arg(errorMessage)
        }
    }

    Component.onCompleted: {
        // TeslaClient already holds the last configuration: show it at once.
        page.loadNotificationSettings()
        vehicleNameField.text = teslaClient.vehicleName
        teslaClient.refreshConfig()
    }

    SilicaFlickable {
        anchors.fill: parent
        contentHeight: column.height

        Column {
            id: column
            width: parent.width
            spacing: Theme.paddingLarge

            PageHeader { title: qsTr("Settings") }

            Label {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.margins: Theme.horizontalPageMargin
                wrapMode: Text.Wrap
                color: Theme.secondaryColor
                font.pixelSize: Theme.fontSizeExtraSmall
                text: page.statusText
            }

            BusyIndicator {
                anchors.horizontalCenter: parent.horizontalCenter
                // Spins until the first GetConfig reply lands (page.configReady
                // gates the Save button below) so the disabled Save reads as
                // "still loading" rather than "broken".
                running: page.configLoading
                visible: running
            }

            TextField {
                id: vinField
                width: parent.width
                label: qsTr("Vehicle VIN")
                // Not a real-looking VIN on purpose: a placeholder that
                // resembles an actual VIN (e.g. "5YJ3E1EA0PF000000") reads
                // as saved content at a glance on a blank field - see
                // docs/limitations.md's Settings load/save race entry.
                placeholderText: qsTr("17-character VIN")
                EnterKey.iconSource: "image://theme/icon-m-enter-next"
                // Typing a VIN does not move the model picker: "Auto" already
                // derives the silhouette from the VIN, and preselecting a
                // guess would persist it as an explicit override on Save.
                onTextChanged: {
                    if (page.loadingConfig || (!activeFocus && !page.configReady))
                        return
                    page.vinDirty = true
                }
            }

            // Model picker for the front-page car graphic. This is a real
            // config override, not just a display setting: the chosen id is
            // persisted (""/"Auto (from VIN)" guesses from the VIN on the
            // front page, anything else forces that model's silhouette). The
            // ids live in sync with helper/src/config.rs's VALID_MODELS.
            ComboBox {
                id: modelField
                width: parent.width
                label: qsTr("Front-page car model")
                description: qsTr("Auto selects the model from the VIN")
                onCurrentIndexChanged: {
                    if (!page.loadingConfig && page.configReady)
                        page.modelDirty = true
                }
                menu: ContextMenu {
                    Repeater {
                        model: VState.MODELS
                        MenuItem { text: modelData.name }
                    }
                }
            }

            TextField {
                id: keyNameField
                width: parent.width
                label: qsTr("Key name")
                placeholderText: qsTr("harbour-electric-eel")
                onTextChanged: {
                    if (!page.loadingConfig && activeFocus)
                        page.keyNameDirty = true
                }
            }

            Slider {
                id: connectTimeoutSlider
                width: parent.width
                label: qsTr("Connect timeout")
                // Max mirrors helper/src/config.rs's MAX_TIMEOUT_SEC (300),
                // so a saved value that high isn't silently clamped down to
                // the old 60 cap and then persisted as 60 on the next Save.
                minimumValue: 5
                maximumValue: 300
                stepSize: 1
                valueText: qsTr("%1 s").arg(value)
                onValueChanged: {
                    if (!page.loadingConfig && page.configReady)
                        page.connectTimeoutDirty = true
                }
            }

            Slider {
                id: commandTimeoutSlider
                width: parent.width
                label: qsTr("Command timeout")
                minimumValue: 2
                maximumValue: 300
                stepSize: 1
                valueText: qsTr("%1 s").arg(value)
                onValueChanged: {
                    if (!page.loadingConfig && page.configReady)
                        page.commandTimeoutDirty = true
                }
            }

            Button {
                anchors.horizontalCenter: parent.horizontalCenter
                text: qsTranslate("FirstPage", "Refresh")
                visible: !page.configReady && !page.configLoading
                onClicked: {
                    page.configLoading = true
                    teslaClient.refreshConfig()
                }
            }

            SectionHeader { text: qsTr("Cover actions") }

            ComboBox {
                id: coverLeftField
                width: parent.width
                label: qsTr("Left action")
                currentIndex: 0
                enabled: page.configReady
                onCurrentIndexChanged: {
                    if (!page.loadingConfig && page.configReady)
                        page.coverLeftDirty = true
                }
                menu: ContextMenu {
                    Repeater {
                        model: CoverActions.ACTIONS
                        MenuItem { text: modelData.title }
                    }
                }
            }

            ComboBox {
                id: coverRightField
                width: parent.width
                label: qsTr("Right action")
                currentIndex: 1
                enabled: page.configReady
                onCurrentIndexChanged: {
                    if (!page.loadingConfig && page.configReady)
                        page.coverRightDirty = true
                }
                menu: ContextMenu {
                    Repeater {
                        model: CoverActions.ACTIONS
                        MenuItem { text: modelData.title }
                    }
                }
            }

            SectionHeader { text: qsTr("Car name") }

            TextField {
                id: vehicleNameField
                width: parent.width
                label: qsTr("Name shown in the car")
                placeholderText: qsTr("Not read from the car yet")
                enabled: page.configReady
                EnterKey.iconSource: "image://theme/icon-m-enter-close"
                EnterKey.onClicked: focus = false
                onTextChanged: {
                    if (!page.loadingConfig && activeFocus)
                        page.vehicleNameDirty = true
                }
            }

            ButtonLayout {
                Button {
                    text: qsTr("Read from car")
                    enabled: page.paired && page.nameRequestId.length === 0
                    onClicked: {
                        page.vehicleNameDirty = false
                        page.runNameCommand("vehicle-name", [], qsTr("Reading name..."))
                    }
                }
                Button {
                    text: qsTr("Rename car")
                    enabled: page.paired && page.nameRequestId.length === 0
                             && vehicleNameField.text.trim().length > 0
                             && vehicleNameField.text.trim() !== teslaClient.vehicleName
                    onClicked: {
                        vehicleNameField.focus = false
                        page.vehicleNameDirty = false
                        page.runNameCommand("set-vehicle-name", [vehicleNameField.text.trim()],
                                            qsTr("Renaming car..."))
                    }
                }
            }

            Label {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.margins: Theme.horizontalPageMargin
                wrapMode: Text.Wrap
                color: Theme.secondaryColor
                font.pixelSize: Theme.fontSizeExtraSmall
                text: page.nameStatus.length > 0 ? page.nameStatus
                      : qsTr("Read over Bluetooth while the car is awake. Renaming changes the name in the car and the Tesla app.")
            }

            SectionHeader { text: qsTr("Status notification") }

            TextSwitch {
                id: notificationSwitch
                text: qsTr("Show status notification")
                description: qsTr("A silent notification in the Events view with the connection, battery and charging state, plus quick actions. Available while ElectricEel is running.")
                enabled: page.configReady
                onCheckedChanged: page.applyNotificationSettings()
            }

            Repeater {
                id: notificationActionFields
                model: 3
                ComboBox {
                    width: column.width
                    label: qsTr("Quick action %1").arg(index + 1)
                    currentIndex: 0
                    enabled: page.configReady && notificationSwitch.checked
                    onCurrentIndexChanged: page.applyNotificationSettings()
                    menu: ContextMenu {
                        MenuItem { text: qsTr("None") }
                        Repeater {
                            model: CoverActions.ACTIONS
                            MenuItem { text: modelData.title }
                        }
                    }
                }
            }

            SectionHeader { text: qsTr("Automagic") }

            Label {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.margins: Theme.horizontalPageMargin
                wrapMode: Text.Wrap
                color: Theme.secondaryColor
                font.pixelSize: Theme.fontSizeExtraSmall
                text: qsTr("Export hotspot flows to Documents, then import them in Automagic. Automagic turns the hotspot on for presence_inside and off for presence_away_confirmed. ElectricEel emits the latter 3 minutes after presence_far unless presence_inside arrives first; repeated presence_far events do not restart the timer. ElectricEel must stay running.")
            }

            Button {
                anchors.horizontalCenter: parent.horizontalCenter
                text: qsTr("Export Automagic flows")
                enabled: !teslaClient.automagicExportBusy
                onClicked: {
                    page.statusText = qsTr("Exporting Automagic flows...")
                    teslaClient.exportAutomagicFlows()
                }
            }

            Label {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.margins: Theme.horizontalPageMargin
                wrapMode: Text.Wrap
                color: Theme.secondaryColor
                font.pixelSize: Theme.fontSizeExtraSmall
                text: qsTr("If you used Add Automagic flows in an earlier version, disable its old hotspot flows before importing this preset. Automagic lets you resolve matching IDs during import. Extra triggers are included for your own flows, such as launching an app.")
            }

            Button {
                anchors.horizontalCenter: parent.horizontalCenter
                text: qsTr("Save")
                // Disabled until the current config has actually loaded (see
                // page.configReady) - otherwise this fires against the
                // fields' blank/default values, not what's really saved.
                enabled: page.configReady
                onClicked: teslaClient.setConfig(vinField.text, page.modelDirty && modelField.currentIndex >= 0 ? VState.MODELS[modelField.currentIndex].id : page.savedModel,
                                                  keyNameField.text,
                                                  connectTimeoutSlider.value, commandTimeoutSlider.value,
                                                  CoverActions.ACTIONS[coverLeftField.currentIndex].id,
                                                  CoverActions.ACTIONS[coverRightField.currentIndex].id)
            }

            Button {
                anchors.horizontalCenter: parent.horizontalCenter
                text: qsTr("Pairing & Keys")
                onClicked: pageStack.push(Qt.resolvedUrl("PairingPage.qml"), { teslaClient: teslaClient })
            }

            SectionHeader { text: qsTr("About") }

            Label {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.margins: Theme.horizontalPageMargin
                wrapMode: Text.Wrap
                font.pixelSize: Theme.fontSizeExtraSmall
                color: Theme.secondaryColor
                text: qsTr("App: ElectricEel %1   |   core: %2")
                    .arg(teslaClient.appVersion).arg(teslaClient.helperVersion)
            }

            VersionWarning { client: teslaClient }
        }
    }
}
