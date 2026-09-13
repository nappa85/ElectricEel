import QtQuick 2.6
import Sailfish.Silica 1.0
import "../js/VehicleState.js" as VState
import "../js/PhoneKeyStatus.js" as PhoneKey

CoverBackground {
    id: cover

    property var teslaClient
    property string vin: ""
    property string model: ""
    property bool hasKey: false
    property bool commandBusy: false

    readonly property bool isPaired: cover.hasKey && cover.vin.length > 0

    // unpaired | bluetooth-off | connected | disconnected | error
    // Same mapping as FirstPage — keep them in PhoneKeyStatus.js.
    readonly property string connectionKind: {
        var status = teslaClient ? teslaClient.phoneKeyStatus : ""
        return PhoneKey.connectionKind(status, cover.isPaired)
    }

    readonly property bool isConnected: connectionKind === "connected"

    // CoverActionList allows two actions. The first cycles this catalog;
    // the second runs the selected command.
    property int actionIndex: 0

    function coverActionAt(i) {
        var items = [
            { cmd: "lock", label: "Lock", icon: "lock.svg" },
            { cmd: "trunk-open", label: "Trunk", icon: "trunk.svg" },
            { cmd: "frunk-open", label: "Frunk", icon: "frunk.svg" },
            { cmd: "climate-on", label: "Climate", icon: "fan1.svg" },
            { cmd: "charge-port-open", label: "Charge port", icon: "ev_station.svg" }
        ]
        return items[(i % items.length + items.length) % items.length]
    }

    readonly property var currentCoverAction: coverActionAt(actionIndex)

    function coverActionIcon(file) {
        return Qt.resolvedUrl("../../img/icons/" + file)
    }

    function cycleCoverAction() {
        cover.actionIndex = (cover.actionIndex + 1) % 5 // lock, trunk, frunk, climate, charge port
    }

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
                        // disconnected and error both use the off icon; color
                        // already flags failure via the red disc.
                        return Qt.resolvedUrl("../../img/icons/wifi_off.svg")
                    }
                }
            }
        }

        Item {
            width: parent.width
            height: parent.height - statusSlot.height - actionLabel.height
                    - parent.spacing * (actionLabel.visible ? 2 : 1)

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

        Label {
            id: actionLabel
            width: parent.width
            visible: cover.isConnected
            horizontalAlignment: Text.AlignHCenter
            font.pixelSize: Theme.fontSizeExtraSmall
            color: Theme.highlightColor
            text: cover.currentCoverAction.label
            truncationMode: TruncationMode.Fade
        }
    }

    CoverActionList {
        enabled: cover.isConnected && !!teslaClient && !cover.commandBusy

        CoverAction {
            iconSource: "image://theme/icon-cover-next"
            onTriggered: cover.cycleCoverAction()
        }

        CoverAction {
            iconSource: cover.coverActionIcon(cover.currentCoverAction.icon)
            onTriggered: cover.runCoverCommand(cover.currentCoverAction.cmd)
        }
    }
}
