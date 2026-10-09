import QtQuick 2.6
import Sailfish.Silica 1.0
import "../js/PhoneKeyState.js" as PhoneKey

Page {
    id: page
    property var teslaClient

    property string publicKeyPem: ""
    property bool generating: false
    property bool pairing: false
    property string pairStatus: ""
    property string keysListOutput: ""
    property string vin: ""
    property bool hasVin: false
    // False until the first GetConfig reply lands (onConfigLoaded below).
    // Gates the Generate Key button: clicking it before the load finished
    // would run with no knowledge of an already-enrolled key.
    property bool configReady: false
    // Per-page list-keys id so two PairingPages never consume each other's
    // replies.
    property string pendingListKeysId: ""

    Connections {
        target: teslaClient
        onKeyGenerated: {
            page.generating = false
            if (ok) {
                page.publicKeyPem = publicKeyPem
                page.pairStatus = qsTr("Key generated. Tap \"Pair with Vehicle\", then tap your NFC card on the center console when prompted on the car's screen.")
            } else {
                page.pairStatus = qsTr("Key generation failed: %1").arg(errorMessage)
            }
        }
        onPaired: {
            page.pairing = false
            page.pairStatus = ok ? (qsTr("Paired.") + "\n" + output) : qsTr("Pairing failed: %1").arg(errorMessage)
        }
        onCommandFinished: {
            if (requestId !== page.pendingListKeysId)
                return
            page.keysListOutput = ok
                ? (stdOut.length > 0 ? stdOut : qsTr("No keys listed"))
                : stdErr
        }
        onCommandError: {
            if (requestId !== page.pendingListKeysId)
                return
            page.keysListOutput = message
        }
        onConfigLoaded: {
            page.configReady = true
            page.vin = vin
            page.hasVin = vin.length > 0
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

            PageHeader { title: qsTr("Pairing & Keys") }

            Label {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.margins: Theme.horizontalPageMargin
                wrapMode: Text.Wrap
                text: qsTr("Set the VIN in Settings first. Then generate a key, then pair it with the car over BLE - you'll need to be next to the vehicle and tap the NFC card on the center console to approve.")
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
                      : qsTr("Phone key starting...")
                font.pixelSize: Theme.fontSizeSmall
                color: PhoneKey.isError(teslaClient.phoneKeyLink)
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
                text: page.generating ? qsTr("Generating...") : qsTr("Generate Key")
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
                text: page.pairing ? qsTr("Waiting for NFC tap...") : qsTr("Pair with Vehicle")
                // VIN gate: pairing without a VIN can never succeed (the
                // session needs it for the BLE handshake).
                enabled: !page.pairing && page.publicKeyPem.length > 0 && page.hasVin
                onClicked: {
                    page.pairing = true
                    page.pairStatus = qsTr("Requesting pairing over BLE - approve on the car's touchscreen / NFC card now.")
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

            SectionHeader { text: qsTr("Enrolled Keys") }

            Button {
                anchors.horizontalCenter: parent.horizontalCenter
                text: qsTr("List Enrolled Keys")
                onClicked: {
                    page.pendingListKeysId = "list-keys#" + Date.now() + "-" + Math.floor(Math.random() * 1000000)
                    teslaClient.runCommand(page.pendingListKeysId, "list-keys", [])
                }
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
