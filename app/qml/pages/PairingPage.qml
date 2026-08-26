import QtQuick 2.6
import Sailfish.Silica 1.0

Page {
    id: page
    property var teslaClient

    property string publicKeyPem: ""
    property bool generating: false
    property bool pairing: false
    property string pairStatus: ""
    property string keysListOutput: ""
    // False until the first GetConfig reply lands (onConfigLoaded below).
    // Gates the Generate Key button: clicking it before the load finished
    // would run with no knowledge of an already-enrolled key.
    property bool configReady: false

    Connections {
        target: teslaClient
        onKeyGenerated: {
            page.generating = false
            if (ok) {
                page.publicKeyPem = publicKeyPem
                page.pairStatus = "Key generated. Tap \"Pair with Vehicle\", then tap your NFC card on the center console when prompted on the car's screen."
            } else {
                page.pairStatus = "Key generation failed: " + errorMessage
            }
        }
        onPaired: {
            page.pairing = false
            page.pairStatus = ok ? ("Paired.\n" + output) : ("Pairing failed: " + errorMessage)
        }
        onCommandFinished: {
            if (requestId !== "list-keys")
                return
            page.keysListOutput = ok ? stdOut : stdErr
        }
        onCommandError: {
            if (requestId !== "list-keys")
                return
            page.keysListOutput = message
        }
        onConfigLoaded: {
            page.configReady = true
            if (hasKey)
                page.publicKeyPem = publicKeyPem
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

            PageHeader { title: "Pairing & Keys" }

            Label {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.margins: Theme.horizontalPageMargin
                wrapMode: Text.Wrap
                text: "Set the VIN in Settings first. Then generate a key, then pair it with the car over BLE - you'll need to be next to the vehicle and tap the NFC card on the center console to approve."
                font.pixelSize: Theme.fontSizeExtraSmall
                color: Theme.secondaryColor
            }

            Label {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.margins: Theme.horizontalPageMargin
                wrapMode: Text.Wrap
                text: teslaClient.phoneKeyStatus.length > 0
                      ? teslaClient.phoneKeyStatus
                      : "Phone key starting..."
                font.pixelSize: Theme.fontSizeSmall
                color: teslaClient.phoneKeyStatus.indexOf("error") >= 0
                       ? Theme.highlightColor
                       : Theme.secondaryHighlightColor
            }

            Label {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.margins: Theme.horizontalPageMargin
                wrapMode: Text.Wrap
                text: "Phone-key logs: Documents/ElectricEel/phone-key-YYYY-MM-DD.log"
                font.pixelSize: Theme.fontSizeExtraSmall
                color: Theme.secondaryColor
            }

            BusyIndicator {
                anchors.horizontalCenter: parent.horizontalCenter
                // Spins until the first GetConfig reply lands (configReady
                // gates the Generate Key button) so the disabled button reads
                // as "still loading" rather than "broken".
                running: !page.configReady
                visible: running
            }

            Button {
                anchors.horizontalCenter: parent.horizontalCenter
                text: page.generating ? "Generating..." : "Generate Key"
                // Gated on configReady: clicking Generate Key before the
                // config loads would run without knowing a key already
                // exists. Also disabled while pairing - generating during the
                // in-flight add-key-request would invalidate the session
                // mid-pairing.
                enabled: page.configReady && !page.generating && !page.pairing
                onClicked: {
                    page.generating = true
                    teslaClient.generateKey(false)
                }
            }

            Label {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.margins: Theme.horizontalPageMargin
                wrapMode: Text.WrapAnywhere
                visible: page.publicKeyPem.length > 0
                font.pixelSize: Theme.fontSizeExtraSmall
                font.family: "monospace"
                text: page.publicKeyPem
            }

            Button {
                anchors.horizontalCenter: parent.horizontalCenter
                text: page.pairing ? "Waiting for NFC tap..." : "Pair with Vehicle"
                enabled: !page.pairing && page.publicKeyPem.length > 0
                onClicked: {
                    page.pairing = true
                    page.pairStatus = "Requesting pairing over BLE - approve on the car's touchscreen / NFC card now."
                    teslaClient.pair()
                }
            }

            BusyIndicator {
                anchors.horizontalCenter: parent.horizontalCenter
                running: page.pairing
                visible: running
            }

            Label {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.margins: Theme.horizontalPageMargin
                wrapMode: Text.Wrap
                text: page.pairStatus
                font.pixelSize: Theme.fontSizeExtraSmall
            }

            SectionHeader { text: "Enrolled Keys" }

            Button {
                anchors.horizontalCenter: parent.horizontalCenter
                text: "List Enrolled Keys"
                onClicked: teslaClient.runCommand("list-keys", "list-keys", [])
            }

            Label {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.margins: Theme.horizontalPageMargin
                wrapMode: Text.WrapAnywhere
                font.pixelSize: Theme.fontSizeExtraSmall
                font.family: "monospace"
                text: page.keysListOutput
            }
        }
    }
}
