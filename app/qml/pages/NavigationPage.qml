import QtQuick 2.6
import Sailfish.Silica 1.0

// Send a destination to the car navigation over Bluetooth (see
// docs/navigation-share.md). Entry points: the app pulley menu,
// Sailfish Share (X-Share-Methods "destination" in the .desktop, handled
// in harbour-electric-eel.qml), or plain copy-paste from any app —
// including Android apps, whose sharesheets can't list Harbour apps
// directly (Jolla hardcodes that bridge for its own apps only), but whose
// clipboard IS shared with native apps via the Sailfish keyboard.
//
// Transport is the signed BLE nav action via the session child, exactly
// like lock/unlock: coordinates go as field-53 NavigationGpsRequest,
// anything else as field-21 NavigationRequest text the car resolves.
// No network, no token, no Fleet API.
Page {
    id: page
    property var teslaClient
    // Prefilled by the Share handler (harbour-electric-eel.qml) or left
    // empty for manual paste/typing.
    property string initialText: ""

    property bool sending: false
    property string previewText: ""
    property string resultText: ""

    function preview() {
        page.resultText = ""
        if (destField.text.trim().length === 0) {
            page.previewText = qsTr("Paste or type a destination first.")
            return
        }
        page.previewText = qsTr("Checking...")
        teslaClient.previewDestination("nav:preview", destField.text)
    }

    function send() {
        var text = destField.text
        if (text.trim().length === 0) {
            page.resultText = qsTr("Nothing to send.")
            return
        }
        page.sending = true
        page.resultText = ""
        teslaClient.shareDestination("nav:send", text)
    }

    Connections {
        target: teslaClient
        onDestinationPreviewed: {
            if (requestId !== "nav:preview")
                return
            if (!ok) {
                page.previewText = qsTr("Cannot use this: %1").arg(errorMessage)
                return
            }
            if (kind === "gps")
                page.previewText = qsTr("Coordinates %1, %2 — navigation will start there.")
                    .arg(value1).arg(value2)
            else
                page.previewText = qsTr("Address \"%1\" — the car will look it up.").arg(value1)
        }
        onShareFinished: {
            if (requestId !== "nav:send")
                return
            page.sending = false
            page.resultText = ok ? output : qsTr("Send failed: %1").arg(errorMessage)
        }
    }

    Component.onCompleted: {
        if (page.initialText.length > 0) {
            destField.text = page.initialText
            page.preview()
        }
    }

    SilicaFlickable {
        anchors.fill: parent
        contentHeight: column.height

        Column {
            id: column
            width: parent.width
            spacing: Theme.paddingLarge

            PageHeader { title: qsTr("Car Navigation") }

            TextArea {
                id: destField
                width: parent.width
                label: qsTr("Destination")
                placeholderText: qsTr("Paste address, coordinates, or map link")
                // Multi-line: Android shares often arrive as
                // "Name\n\nhttps://..." — keep everything, the parser
                // prefers an embedded map URL over surrounding text.
            }

            Button {
                anchors.horizontalCenter: parent.horizontalCenter
                text: qsTr("Preview")
                enabled: !page.sending
                onClicked: page.preview()
            }

            Label {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.margins: Theme.horizontalPageMargin
                wrapMode: Text.Wrap
                font.pixelSize: Theme.fontSizeSmall
                color: Theme.secondaryColor
                visible: page.previewText.length > 0
                text: page.previewText
            }

            Button {
                anchors.horizontalCenter: parent.horizontalCenter
                text: qsTr("Send to Car")
                enabled: !page.sending
                onClicked: page.send()
            }

            BusyIndicator {
                anchors.horizontalCenter: parent.horizontalCenter
                running: page.sending
                visible: running
            }

            Label {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.margins: Theme.horizontalPageMargin
                wrapMode: Text.Wrap
                font.pixelSize: Theme.fontSizeSmall
                color: Theme.secondaryHighlightColor
                visible: page.resultText.length > 0
                text: page.resultText
            }

            SectionHeader { text: qsTr("Notes") }

            Label {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.margins: Theme.horizontalPageMargin
                wrapMode: Text.Wrap
                font.pixelSize: Theme.fontSizeExtraSmall
                color: Theme.secondaryColor
                text: qsTr("This uses Bluetooth, like lock/unlock — the car must " +
                      "be in range, no internet needed on either side. " +
                      "Coordinates are sent exactly; addresses and links are " +
                      "looked up by the car itself, so unusual spellings may " +
                      "resolve differently than on your phone. From Android " +
                      "apps: copy the address or link, then paste it above.")
            }
        }
    }
}
