import QtQuick 2.6
import Sailfish.Silica 1.0
import "../js/VehicleState.js" as VState

CoverBackground {
    id: cover

    property var teslaClient
    property string vin: ""
    property string model: ""
    property bool hasKey: false
    property bool commandBusy: false

    function phoneKeyStatusIsBluetoothOff(status) {
        return status.indexOf("NotPowered") >= 0
            || status.indexOf("RFKILL") >= 0
            || status.indexOf("power on adapter") >= 0
    }

    function phoneKeyStatusIsConnected(status) {
        return status === "Phone key connected"
            || status === "Phone key authorized"
    }

    readonly property bool isPaired: cover.hasKey && cover.vin.length > 0

    // unpaired | bluetooth-off | connected | disconnected
    readonly property string connectionKind: {
        var status = teslaClient ? teslaClient.phoneKeyStatus : ""
        if (phoneKeyStatusIsBluetoothOff(status))
            return "bluetooth-off"
        if (!isPaired)
            return "unpaired"
        if (phoneKeyStatusIsConnected(status))
            return "connected"
        return "disconnected"
    }

    readonly property bool isConnected: connectionKind === "connected"

    function runCoverCommand(cmd) {
        if (!teslaClient || cover.commandBusy || !cover.isConnected)
            return
        cover.commandBusy = true
        teslaClient.runCommand("cover:" + cmd, cmd, [])
    }

    Component.onCompleted: {
        if (teslaClient)
            teslaClient.refreshConfig()
    }

    Connections {
        target: teslaClient
        onConfigLoaded: {
            cover.vin = vin
            cover.model = model
            cover.hasKey = hasKey
        }
        onCommandFinished: {
            if (requestId.indexOf("cover:") === 0)
                cover.commandBusy = false
        }
        onCommandError: {
            if (requestId.indexOf("cover:") === 0)
                cover.commandBusy = false
        }
    }

    // Tesla "T" watermark: large but readable, anchored bottom-right with a
    // slice spilling off the edge (not so big that only an unrecognizable
    // fragment remains on the cover).
    Image {
        id: logoWatermark
        height: parent.height * 1.45
        width: height * 254.584 / 253.502
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        anchors.rightMargin: -width * 0.28
        anchors.bottomMargin: -height * 0.30
        opacity: 0.22
        z: 0
        source: Qt.resolvedUrl("../../img/icons/tesla_logo.svg")
        fillMode: Image.PreserveAspectFit
        sourceSize: Qt.size(width * 2, height * 2)
        clip: false
    }

    Column {
        anchors.fill: parent
        anchors.margins: Theme.paddingMedium
        spacing: Theme.paddingSmall
        z: 1

        // Status badge: filled glyphs on a dark disc so they read against
        // the Tesla watermark. Not an IconButton / MouseArea.
        Item {
            id: statusSlot
            width: parent.width
            visible: cover.connectionKind !== "unpaired"
            height: visible ? statusBadge.height : 0

            Item {
                id: statusBadge
                anchors.horizontalCenter: parent.horizontalCenter
                width: Theme.iconSizeMedium + Theme.paddingSmall * 2
                height: width

                Rectangle {
                    anchors.fill: parent
                    radius: width / 2
                    color: Theme.rgba("#000000", 0.62)
                }

                HighlightImage {
                    id: statusIcon
                    anchors.centerIn: parent
                    width: Theme.iconSizeMedium
                    height: Theme.iconSizeMedium
                    sourceSize: Qt.size(width, height)
                    color: cover.connectionKind === "connected" ? "#4CD964" : "#FF453A"
                    source: {
                        if (cover.connectionKind === "connected")
                            return Qt.resolvedUrl("../../img/icons/wifi.svg")
                        if (cover.connectionKind === "bluetooth-off")
                            return Qt.resolvedUrl("../../img/icons/bluetooth_disabled.svg")
                        return Qt.resolvedUrl("../../img/icons/wifi_off.svg")
                    }
                }
            }
        }

        Item {
            width: parent.width
            height: parent.height - statusSlot.height - actionRow.height
                    - parent.spacing * (actionRow.visible ? 2 : 1)

            Image {
                id: carImage
                anchors.fill: parent
                fillMode: Image.PreserveAspectFit
                source: VState.carImageSource(cover.model, cover.vin)
                sourceSize: Qt.size(width * 2, height * 2)
                visible: cover.isPaired
            }

            Image {
                id: appIcon
                anchors.centerIn: parent
                width: Math.min(parent.width * 0.55, parent.height * 0.55)
                height: width
                fillMode: Image.PreserveAspectFit
                source: "image://theme/icon-size-large?file:///usr/share/icons/hicolor/128x128/apps/harbour-electric-eel.png"
                visible: !cover.isPaired
            }
        }

        Row {
            id: actionRow
            width: parent.width
            visible: cover.isConnected
            height: visible ? Theme.iconSizeMedium : 0
            spacing: 0

            // IconButton + the 1200–1440px trunk/frunk SVGs overflow the
            // cover (native image size, stacked). HighlightImage + sourceSize
            // keeps a compact three-across row.
            Item {
                width: actionRow.width / 3
                height: actionRow.height
                enabled: !!teslaClient && !cover.commandBusy
                opacity: enabled ? 1.0 : Theme.opacityLow

                HighlightImage {
                    anchors.centerIn: parent
                    width: Theme.iconSizeSmall
                    height: Theme.iconSizeSmall
                    sourceSize: Qt.size(width, height)
                    source: Qt.resolvedUrl("../../img/icons/lock.svg")
                    color: Theme.primaryColor
                }

                MouseArea {
                    anchors.fill: parent
                    onClicked: cover.runCoverCommand("lock")
                }
            }

            Item {
                width: actionRow.width / 3
                height: actionRow.height
                enabled: !!teslaClient && !cover.commandBusy
                opacity: enabled ? 1.0 : Theme.opacityLow

                HighlightImage {
                    anchors.centerIn: parent
                    width: Theme.iconSizeSmall
                    height: Theme.iconSizeSmall
                    sourceSize: Qt.size(width, height)
                    source: Qt.resolvedUrl("../../img/icons/trunk.svg")
                    color: Theme.primaryColor
                }

                MouseArea {
                    anchors.fill: parent
                    onClicked: cover.runCoverCommand("trunk-open")
                }
            }

            Item {
                width: actionRow.width / 3
                height: actionRow.height
                enabled: !!teslaClient && !cover.commandBusy
                opacity: enabled ? 1.0 : Theme.opacityLow

                HighlightImage {
                    anchors.centerIn: parent
                    width: Theme.iconSizeSmall
                    height: Theme.iconSizeSmall
                    sourceSize: Qt.size(width, height)
                    source: Qt.resolvedUrl("../../img/icons/frunk.svg")
                    color: Theme.primaryColor
                }

                MouseArea {
                    anchors.fill: parent
                    onClicked: cover.runCoverCommand("frunk-open")
                }
            }
        }
    }
}
