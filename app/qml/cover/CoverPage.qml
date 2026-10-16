import QtQuick 2.6
import Sailfish.Silica 1.0
import "../js/CommandCatalog.js" as Catalog
import "../js/CommandText.js" as CommandText
import "../js/VehicleState.js" as VState
import "../js/PhoneKeyState.js" as PhoneKey
import "../js/CoverActions.js" as CoverActions
import "../js/TapConfirm.js" as TapConfirm

CoverBackground {
    id: cover

    property var teslaClient
    readonly property string vin: teslaClient ? teslaClient.vin : ""
    readonly property string model: teslaClient ? teslaClient.model : ""
    readonly property bool hasKey: teslaClient ? teslaClient.hasKey : false
    readonly property string configError: teslaClient ? teslaClient.configError : ""
    readonly property string leftAction: CoverActions.action(teslaClient ? teslaClient.coverLeftAction : "", 0).id
    readonly property string rightAction: CoverActions.action(teslaClient ? teslaClient.coverRightAction : "", 1).id
    property bool commandBusy: false
    property string pendingCmd: ""
    property string pendingAction: ""
    // Last cover command failure, shown until the next command.
    property string commandFailure: ""
    // Action armed by a first tap; a second tap on it runs it.
    property string armedAction: ""

    function tapCoverAction(cmd) {
        if (!cover.canCommand || cover.commandBusy)
            return
        var next = TapConfirm.tap(cover.armedAction, cmd)
        cover.armedAction = next.armed
        if (next.armed.length > 0)
            armTimer.restart()
        else
            armTimer.stop()
        if (next.run.length > 0)
            cover.runCoverCommand(next.run)
    }

    Timer {
        id: armTimer
        interval: TapConfirm.ARM_MS
        onTriggered: cover.armedAction = ""
    }

    readonly property bool isPaired: cover.hasKey && cover.vin.length > 0

    // unpaired | bluetooth-off | connected | disconnected
    readonly property string connectionKind: PhoneKey.connectionKind(
        teslaClient ? teslaClient.phoneKeyLink : "stopped", isPaired)

    // A command can establish its own link while presence is scanning.
    // Use the dashboard's key/VIN prerequisites, not the connection icon.
    readonly property bool canCommand: !!teslaClient && isPaired && configError.length === 0

    function runCoverCommand(cmd) {
        if (!cover.canCommand || cover.commandBusy)
            return
        cover.commandBusy = true
        cover.commandFailure = ""
        cover.pendingAction = cmd
        cover.pendingCmd = teslaClient.newRequestId("cover:" + cmd, cover.vin)
        // State-changing actions ask Rust for the dashboard's settled refresh.
        teslaClient.runCommand(cover.pendingCmd, cmd, [], Catalog.refreshesStatus(cmd))
    }

    // A pending reply belongs to the previous car.
    onVinChanged: {
        cover.armedAction = ""
        cover.pendingCmd = ""
        cover.commandBusy = false
        cover.commandFailure = ""
    }

    Connections {
        target: teslaClient
        onCommandFinished: {
            if (requestId !== cover.pendingCmd)
                return
            cover.commandBusy = false
            cover.pendingCmd = ""
            if (!ok)
                cover.commandFailure = CoverActions.action(cover.pendingAction, 0).title + ": "
                    + CommandText.failure(stdErr, exitCode)
        }
        onCommandError: {
            if (requestId !== cover.pendingCmd)
                return
            cover.commandBusy = false
            cover.pendingCmd = ""
            cover.commandFailure = CoverActions.action(cover.pendingAction, 0).title + ": " + message
        }
    }

    CoverActionList {
        enabled: cover.canCommand && !cover.commandBusy

        CoverAction {
            iconSource: Qt.resolvedUrl(CoverActions.action(cover.leftAction, 0).icon)
            onTriggered: cover.tapCoverAction(cover.leftAction)
        }

        CoverAction {
            iconSource: Qt.resolvedUrl(CoverActions.action(cover.rightAction, 1).icon)
            onTriggered: cover.tapCoverAction(cover.rightAction)
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
        // Leave room for the native cover actions drawn over the cover.
        anchors.bottomMargin: Theme.paddingMedium + (cover.canCommand ? cover.coverActionArea.height : 0)
        spacing: Theme.paddingSmall
        z: 1

        Label {
            id: commandFailureLabel
            width: parent.width
            visible: cover.commandFailure.length > 0
            height: visible ? implicitHeight : 0
            wrapMode: Text.Wrap
            maximumLineCount: 3
            elide: Text.ElideRight
            font.pixelSize: Theme.fontSizeExtraSmall
            color: "#FF453A"
            text: cover.commandFailure
        }

        Label {
            id: configErrorLabel
            width: parent.width
            visible: cover.configError.length > 0
            height: visible ? implicitHeight : 0
            wrapMode: Text.Wrap
            font.pixelSize: Theme.fontSizeExtraSmall
            text: cover.configError + "\n" + qsTranslate("ConfigErrorBanner", "Retry")
            MouseArea {
                anchors.fill: parent
                onClicked: teslaClient.refreshConfig()
            }
        }

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
            height: Math.max(0, parent.height - statusSlot.height - configErrorLabel.height
                    - commandFailureLabel.height
                    - parent.spacing * (1 + (configErrorLabel.visible ? 1 : 0)
                                          + (commandFailureLabel.visible ? 1 : 0)))

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
                source: "file:///usr/share/icons/hicolor/128x128/apps/harbour-electric-eel.png"
                visible: !cover.isPaired
            }
        }
    }

    // Confirmation prompt while an action is armed: drawn over the car so
    // the layout never moves, on the same dark disc style as the status
    // badge so it reads against the watermark and any ambience.
    Rectangle {
        anchors.centerIn: parent
        anchors.verticalCenterOffset: cover.canCommand ? -cover.coverActionArea.height / 2 : 0
        width: parent.width - Theme.paddingMedium * 2
        height: armedLabel.implicitHeight + Theme.paddingMedium * 2
        radius: Theme.paddingMedium
        color: Theme.rgba("#000000", 0.75)
        visible: cover.armedAction.length > 0
        z: 2

        Label {
            id: armedLabel
            anchors.centerIn: parent
            width: parent.width - Theme.paddingMedium * 2
            horizontalAlignment: Text.AlignHCenter
            wrapMode: Text.Wrap
            maximumLineCount: 3
            elide: Text.ElideRight
            font.pixelSize: Theme.fontSizeSmall
            font.bold: true
            color: "#FFFFFF"
            text: cover.armedAction.length > 0
                  ? qsTr("Tap again: %1").arg(CoverActions.action(cover.armedAction, 0).title)
                  : ""
        }
    }
}
