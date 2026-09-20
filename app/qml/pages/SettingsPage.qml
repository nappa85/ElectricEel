import QtQuick 2.6
import Sailfish.Silica 1.0
import "../js/VehicleState.js" as VState

Page {
    id: page
    property var teslaClient

    property string statusText: qsTr("Loading current configuration...")
    // Set while onConfigLoaded is repopulating the form so vinField's
    // onTextChanged doesn't auto-guess the model and clobber the model value
    // that was actually saved for this VIN (see modelField's comment).
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

    Connections {
        target: teslaClient
        onConfigLoaded: {
            page.loadingConfig = true
            vinField.text = vin
            modelField.currentIndex = VState.modelIndex(model)
            keyNameField.text = keyName
            connectTimeoutSlider.value = connectTimeoutSec
            commandTimeoutSlider.value = commandTimeoutSec
            page.loadingConfig = false
            page.configReady = true
            page.statusText = hasKey ? qsTr("Key on file") : qsTr("No key yet - use Pair Vehicle from the main menu")
        }
        onConfigSaved: {
            page.statusText = ok ? qsTr("Saved") : qsTr("Save failed: %1").arg(errorMessage)
        }
    }

    Component.onCompleted: teslaClient.refreshConfig()

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
                running: !page.configReady
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
                // The VIN parser preselects the matching model in the list
                // below as soon as a recognizable prefix appears. A manual
                // selection made before the VIN was pasted is overwritten,
                // but that's the point of "Auto" preselect - and the user
                // can always re-pick after. Suppressed while config loads so
                // a saved explicit/model value survives repopulation.
                onTextChanged: {
                    if (page.loadingConfig)
                        return
                    modelField.currentIndex = VState.modelIndex(VState.guessModel(text))
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
                valueText: value + qsTr(" s")
            }

            Slider {
                id: commandTimeoutSlider
                width: parent.width
                label: qsTr("Command timeout")
                minimumValue: 2
                maximumValue: 300
                stepSize: 1
                valueText: value + qsTr(" s")
            }

            Button {
                anchors.horizontalCenter: parent.horizontalCenter
                text: qsTr("Save")
                // Disabled until the current config has actually loaded (see
                // page.configReady) - otherwise this fires against the
                // fields' blank/default values, not what's really saved.
                enabled: page.configReady
                onClicked: teslaClient.setConfig(vinField.text, VState.MODELS[modelField.currentIndex].id,
                                                  keyNameField.text,
                                                  connectTimeoutSlider.value, commandTimeoutSlider.value)
            }

            Button {
                anchors.horizontalCenter: parent.horizontalCenter
                text: qsTr("Pairing & Keys")
                onClicked: pageStack.push(Qt.resolvedUrl("PairingPage.qml"), { teslaClient: teslaClient })
            }

            SectionHeader { text: qsTr("About") }

            // Versions of the app and its in-process control core. They're
            // stamped from the same release tag and compiled together, so a
            // difference (or an empty core version, i.e. one too old to have
            // GetVersion) means something's wrong with the install - exactly
            // the state that used to surface as a silent "No VIN configured".
            Label {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.margins: Theme.horizontalPageMargin
                wrapMode: Text.Wrap
                font.pixelSize: Theme.fontSizeExtraSmall
                color: Theme.secondaryColor
                text: qsTr("App: ElectricEel %1   |   core: %2")
                    .arg(teslaClient.appVersion)
                    .arg(teslaClient.helperVersion.length > 0
                         ? teslaClient.helperVersion
                         : qsTr("(too old / unknown)"))
            }

            Label {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.margins: Theme.horizontalPageMargin
                wrapMode: Text.Wrap
                font.pixelSize: Theme.fontSizeExtraSmall
                color: Theme.highlightColor
                visible: teslaClient.helperVersion.length === 0
                        || teslaClient.helperVersion !== teslaClient.appVersion
                text: teslaClient.helperVersion.length === 0
                    ? qsTr("The control core is too old to report a version - reinstall the app.")
                    : qsTr("Version mismatch: core %1 vs app %2 - reinstall the app.")
                        .arg(teslaClient.helperVersion).arg(teslaClient.appVersion)
            }
        }
    }
}
