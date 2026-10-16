import QtQuick 2.6
import Sailfish.Silica 1.0
import "../js/CommandCatalog.js" as Catalog
import "../js/CommandText.js" as CommandText
import "../js/VehicleState.js" as VState

Page {
    id: page
    property var teslaClient

    // The configuration lives in TeslaClient; these only name it locally.
    readonly property string vin: teslaClient ? teslaClient.vin : ""
    readonly property string model: teslaClient ? teslaClient.model : ""
    readonly property bool hasKey: teslaClient ? teslaClient.hasKey : false
    readonly property string vehicleName: teslaClient ? teslaClient.vehicleName : ""

    // Live dashboard state. refreshStatus() runs VState.STATUS_LEGS in order
    // (body-controller-state over VCSEC, then the infotainment categories),
    // one BLE round-trip at a time. statusStage tracks which leg is in
    // flight so the UI shows one busy indicator and never stacks requests.
    property var vehicleStatus: VState.emptyStatus()
    property var statusBeforeToggle: null
    property string statusStage: ""
    property string statusRequestId: ""
    // A refresh requested while a chain runs (e.g. Rust's post-command
    // refresh landing mid-chain) re-runs the chain once it finishes, since
    // the running legs may have read the pre-command state.
    property bool refreshPending: false
    // Set from stdErr when a status leg fails. Climate/charge/closures go
    // through Infotainment and fail on a sleeping vehicle; lock/unlock and
    // body-controller-state use VCSEC and still work then (see
    // VehicleState.js).
    property string statusError: ""
    // Monotonic tick bumped by statusAgeTimer below, so the "Updated Xm ago"
    // label recomputes between refreshes.
    property int statusAgeTick: 0

    Timer {
        id: statusAgeTimer
        interval: 30000
        repeat: true
        running: page.vehicleStatus.updatedAt > 0
        onTriggered: page.statusAgeTick++
    }

    // Rust schedules the post-toggle settle delay before asking this dashboard
    // to render a fresh reading. Only the label-age timer above stays in QML.

    function refreshStatus() {
        if (page.statusStage.length > 0) {
            page.refreshPending = true
            return
        }
        page.refreshPending = false
        if (!page.hasKey) {
            page.statusError = qsTr("No key: generate and pair a key first")
            return
        }
        if (page.vin.length === 0) {
            page.statusError = qsTr("No VIN configured")
            return
        }
        page.statusError = ""
        var leg = VState.STATUS_LEGS[0]
        runStatus(leg.stage, leg.command, leg.args)
    }

    // The car's name rarely changes: read it once per run while unknown,
    // after a refresh proved the infotainment link answers. Rust caches it.
    property bool vehicleNameRequested: false
    onStatusStageChanged: {
        if (page.statusStage.length === 0 && !page.refreshPending)
            page.readVehicleNameOnce()
    }
    function readVehicleNameOnce() {
        if (page.vehicleNameRequested || page.statusError.length > 0
                || !page.hasKey || page.vin.length === 0 || page.vehicleName.length > 0)
            return
        page.vehicleNameRequested = true
        teslaClient.runCommand(teslaClient.newRequestId("name", page.vin), "vehicle-name", [], false)
    }

    function runStatus(stage, command, args, refresh) {
        page.statusStage = stage
        page.statusRequestId = teslaClient.newRequestId("status:" + stage, page.vin)
        teslaClient.runCommand(page.statusRequestId, command, args, refresh || false)
    }

    // Sets field to an explicit value on a clone of vehicleStatus and
    // assigns it (a plain in-place mutation wouldn't trigger the QML
    // property's change notification), so the icon reflects the command
    // we're about to send immediately rather than waiting for Rust's
    // post-command refresh, which stays authoritative.
    //
    // Takes the intended value explicitly: flipping `!current` inverts
    // unknown null into a confident true (`!null === true`).
    function setOptimistic(field, value) {
        page.statusBeforeToggle = page.vehicleStatus
        var next = VState.clone(page.vehicleStatus)
        next[field] = value
        page.vehicleStatus = next
    }

    function toggleLock() {
        if (page.statusStage.length > 0)
            return
        // The padlock shows locked unless we *know* the car is unlocked
        // (`locked === false`), so unknown sends unlock, matching the icon.
        var sendLock = page.vehicleStatus.locked === false
        setOptimistic("locked", sendLock)
        runStatus("toggle", sendLock ? "lock" : "unlock", [], true)
    }

    function toggleClimate() {
        if (page.statusStage.length > 0)
            return
        var wasOn = page.vehicleStatus.isClimateOn
        setOptimistic("isClimateOn", !wasOn)
        runStatus("toggle", wasOn ? "climate-off" : "climate-on", [], true)
    }

    function toggleWindows() {
        if (page.statusStage.length > 0)
            return
        // Unknown (null) windows close: venting is only offered when the car
        // reported them closed.
        var vent = page.vehicleStatus.windowsOpen === false
        setOptimistic("windowsOpen", vent)
        runStatus("toggle", vent ? "windows-vent" : "windows-close", [], true)
    }

    function openFrunk() {
        if (page.statusStage.length > 0)
            return
        runStatus("toggle", "frunk-open", [], true)
    }

    function openTrunk() {
        if (page.statusStage.length > 0)
            return
        runStatus("toggle", "trunk-open", [], true)
    }

    function quickAction(action) {
        if (action === "lock") toggleLock()
        else if (action === "climate") toggleClimate()
        else if (action === "frunk") openFrunk()
        else if (action === "trunk") openTrunk()
        else if (action === "windows") toggleWindows()
    }

    function batteryIconSource() {
        var level = page.vehicleStatus.batteryLevel
        if (level === null) return "../../img/icons/battery5.svg"
        if (level >= 90) return "../../img/icons/battery100.svg"
        if (level >= 60) return "../../img/icons/battery75.svg"
        if (level >= 40) return "../../img/icons/battery50.svg"
        if (level >= 10) return "../../img/icons/battery10.svg"
        return "../../img/icons/battery5.svg"
    }

    // A different VIN is a different car: drop its telemetry, optimistic
    // snapshot and pending replies (their request IDs carry the old VIN).
    // Unchanged configuration replies change no property and so start no
    // BLE traffic.
    onVinChanged: {
        page.vehicleStatus = VState.emptyStatus()
        page.statusBeforeToggle = null
        page.statusStage = ""
        page.statusRequestId = ""
        page.statusError = ""
        page.vehicleNameRequested = false
        page.refreshStatus()
    }

    onHasKeyChanged: {
        if (page.statusStage.length === 0)
            page.refreshStatus()
    }

    Connections {
        target: teslaClient
        onStatusRefreshRequested: page.refreshStatus()
        onCommandFinished: {
            if (requestId !== page.statusRequestId || page.statusStage.length === 0)
                return
            if (page.statusStage === "toggle") {
                if (!ok) {
                    page.statusError = CommandText.failure(stdErr, exitCode)
                    if (page.statusBeforeToggle !== null)
                        page.vehicleStatus = page.statusBeforeToggle
                }
                page.statusBeforeToggle = null
                // Success or not, Rust re-fetches after the telemetry settles
                // (the command can "succeed" over BLE without the vehicle
                // confirming), so the dashboard reflects reality.
                page.statusStage = ""
                page.statusRequestId = ""
                return
            }
            var index = VState.statusLegIndex(page.statusStage)
            var leg = VState.STATUS_LEGS[index]
            if (ok)
                page.vehicleStatus = leg.merge(page.vehicleStatus, stdOut)
            else if (page.statusError.length === 0)
                page.statusError = CommandText.failure(stdErr, exitCode)
            var next = VState.STATUS_LEGS[index + 1]
            if (next) {
                runStatus(next.stage, next.command, next.args)
            } else {
                page.statusStage = ""
                page.statusRequestId = ""
                if (page.refreshPending)
                    refreshStatus()
            }
        }
        onCommandError: {
            if (requestId !== page.statusRequestId || page.statusStage.length === 0)
                return
            var wasToggle = page.statusStage === "toggle"
            if (wasToggle) {
                if (page.statusBeforeToggle !== null)
                    page.vehicleStatus = page.statusBeforeToggle
                page.statusBeforeToggle = null
            }
            page.statusStage = ""
            page.statusRequestId = ""
            if (page.statusError.length === 0)
                page.statusError = message
            // Local submission failures never reach Rust's settle scheduler.
            if (wasToggle || page.refreshPending)
                refreshStatus()
        }
    }

    SilicaListView {
        id: listView
        anchors.fill: parent
        model: Catalog.CATEGORIES

        // The header grows after the first layout (config, banners, status
        // rows arrive asynchronously) and ListView keeps the first category
        // in place, which would open the page scrolled past the header and
        // the pull-down menu. Stay at the top until the user scrolls.
        property bool userScrolled: false
        onMovementStarted: userScrolled = true

        header: Column {
            width: listView.width
            onHeightChanged: if (!listView.userScrolled) listView.positionViewAtBeginning()

            PageHeader {
                title: "ElectricEel"
            }

            VersionWarning { client: teslaClient }

            ConfigErrorBanner {
                client: teslaClient
            }

            BackgroundItem {
                width: parent.width
                height: Theme.itemSizeMedium
                onClicked: pageStack.push(Qt.resolvedUrl("SettingsPage.qml"), { teslaClient: teslaClient })

                Column {
                    anchors.verticalCenter: parent.verticalCenter
                    anchors.left: parent.left
                    anchors.leftMargin: Theme.horizontalPageMargin
                    anchors.right: parent.right
                    anchors.rightMargin: Theme.horizontalPageMargin

                    Label {
                        // The car's own name once read, else the VIN.
                        text: page.vehicleName.length > 0 ? page.vehicleName
                              : page.vin.length > 0 ? page.vin : qsTr("No VIN configured")
                        color: page.vin.length > 0 ? Theme.primaryColor : Theme.secondaryColor
                    }
                    Label {
                        text: page.hasKey ? qsTr("Key ready") : qsTr("No key - tap for Settings / Pairing")
                        font.pixelSize: Theme.fontSizeExtraSmall
                        color: page.hasKey ? Theme.secondaryHighlightColor : Theme.secondaryColor
                    }
                }
            }

            // Car graphic + battery / climate readouts. The icon set and
            // presentation mirror the harbour-tcarint app this project shares
            // its icon style with.
            Rectangle {
                width: parent.width
                height: carColumn.height + Theme.paddingMedium * 2
                visible: page.hasKey && page.vin.length > 0
                color: Theme.rgba(Theme.highlightBackgroundColor, 0.08)

                Column {
                    id: carColumn
                    width: parent.width - Theme.horizontalPageMargin * 2
                    anchors.horizontalCenter: parent.horizontalCenter
                    anchors.verticalCenter: parent.verticalCenter
                    spacing: Theme.paddingSmall

                    Image {
                        id: carImage
                        width: parent.width * 0.85
                        height: Theme.itemSizeExtraLarge * 1.5
                        anchors.horizontalCenter: parent.horizontalCenter
                        fillMode: Image.PreserveAspectFit
                        // An explicit config model override wins; otherwise
                        // the VIN is parsed for the model.
                        source: VState.carImageSource(page.model, page.vin)
                        sourceSize: Qt.size(width * 2, height * 2)
                    }

                    Row {
                        anchors.horizontalCenter: parent.horizontalCenter
                        spacing: Theme.paddingMedium

                        Icon {
                            id: batteryIcon
                            source: page.batteryIconSource()
                            height: Theme.iconSizeSmallPlus
                            width: height
                        }

                        Label {
                            id: batteryLabel
                            anchors.verticalCenter: batteryIcon.verticalCenter
                            font.pixelSize: Theme.fontSizeSmall
                            color: Theme.primaryColor
                            text: page.vehicleStatus.batteryLevel === null ? "--" :
                                  (page.vehicleStatus.batteryLevel + "%" +
                                   // Charging, Starting and Calibrating match the charging toggle.
                                   (Catalog.nearlyCharging(page.vehicleStatus) ? qsTr(" • charging") : ""))
                        }

                        Label {
                            id: tempLabel
                            anchors.verticalCenter: batteryIcon.verticalCenter
                            visible: page.vehicleStatus.insideTemp !== null
                            font.pixelSize: Theme.fontSizeSmall
                            color: Theme.secondaryColor
                            text: visible ? qsTr("• %1°C").arg(page.vehicleStatus.insideTemp.toFixed(0)) : ""
                        }
                    }

                    Row {
                        width: carColumn.width
                        spacing: Theme.paddingSmall

                        BusyIndicator {
                            size: BusyIndicatorSize.ExtraSmall
                            running: page.statusStage.length > 0
                            visible: running
                        }
                        Label {
                            width: parent.width - Theme.itemSizeSmall
                            wrapMode: Text.Wrap
                            horizontalAlignment: Text.AlignHCenter
                            font.pixelSize: Theme.fontSizeTiny
                            color: page.statusError.length > 0 && page.statusStage.length === 0 ? Theme.highlightColor : Theme.secondaryColor
                            text: {
                                // Read the periodic tick so elapsed time recomputes
                                // between refreshes (see statusAgeTimer above).
                                var _ = page.statusAgeTick
                                var age = VState.minutesAgo(page.vehicleStatus.updatedAt)
                                if (page.statusStage.length > 0)
                                    return qsTr("Updating...")
                                if (page.statusError.length > 0)
                                    return qsTr("Status unavailable (%1). Vehicle may be asleep - try Wake Vehicle (Attention), then Refresh Status.").arg(page.statusError)
                                if (age < 0)
                                    return qsTr("Pull down to refresh status")
                                if (age === 0)
                                    return qsTr("Updated just now")
                                return qsTr("Updated %1m ago").arg(age)
                            }
                        }
                    }
                }
            }

            // Quick actions straight on the front page. Each icon mirrors the
            // vehicle's actual state (closed padlock when locked, highlighted
            // fan/window when climate/windows are on), not the action a tap
            // triggers. Frunk/trunk have no telemetry, so they stay plain
            // "open" actions.
            Rectangle {
                width: parent.width
                height: quickRow.height + Theme.paddingMedium * 2
                visible: page.hasKey && page.vin.length > 0
                color: Theme.rgba(Theme.highlightBackgroundColor, 0.08)

                Row {
                    id: quickRow
                    anchors.horizontalCenter: parent.horizontalCenter
                    anchors.verticalCenter: parent.verticalCenter
                    spacing: Theme.paddingSmall
                    readonly property real buttonSize: Math.min(Theme.itemSizeMedium,
                        (parent.width - Theme.horizontalPageMargin * 2 - spacing * 4) / 5)

                    Repeater {
                        model: [
                            { action: "lock", highlighted: page.vehicleStatus.locked === true,
                              icon: page.vehicleStatus.locked === false ? "../../img/icons/lock_open.svg" : "../../img/icons/lock.svg" },
                            { action: "climate", highlighted: page.vehicleStatus.isClimateOn === true,
                              icon: "../../img/icons/fan1.svg" },
                            { action: "frunk", highlighted: false, icon: "../../img/icons/frunk.svg" },
                            { action: "trunk", highlighted: false, icon: "../../img/icons/trunk.svg" },
                            { action: "windows", highlighted: page.vehicleStatus.windowsOpen === true,
                              icon: "../../img/icons/window.svg" }
                        ]

                        SecondaryButton {
                            width: quickRow.buttonSize
                            height: quickRow.buttonSize
                            icon.height: Theme.iconSizeSmallPlus
                            icon.width: Theme.iconSizeSmallPlus
                            icon.source: modelData.icon
                            icon.color: "#5f6368"
                            icon.highlightColor: "white"
                            icon.highlighted: modelData.highlighted
                            enabled: page.statusStage.length === 0
                            onClicked: page.quickAction(modelData.action)
                        }
                    }
                }
            }

            SectionHeader { text: qsTr("Categories") }
        }

        delegate: BackgroundItem {
            width: listView.width
            height: Theme.itemSizeMedium

            onClicked: pageStack.push(Qt.resolvedUrl("CategoryPage.qml"), {
                teslaClient: teslaClient,
                categoryId: modelData.id,
                categoryTitle: modelData.title,
                dashboard: page
            })

            Icon {
                id: categoryIcon
                source: modelData.icon
                height: Theme.iconSizeSmallPlus
                width: height
                color: "#5f6368"
                anchors.verticalCenter: parent.verticalCenter
                anchors.left: parent.left
                anchors.leftMargin: Theme.horizontalPageMargin
            }

            Label {
                anchors.verticalCenter: parent.verticalCenter
                anchors.left: categoryIcon.right
                anchors.leftMargin: Theme.paddingMedium
                anchors.right: forwardIcon.left
                anchors.rightMargin: Theme.paddingMedium
                text: modelData.title
                truncationMode: TruncationMode.Fade
            }

            Icon {
                id: forwardIcon
                source: "../../img/icons/arrow_forward.svg"
                height: Theme.iconSizeSmall
                width: height
                color: "#5f6368"
                anchors.verticalCenter: parent.verticalCenter
                anchors.right: parent.right
                anchors.rightMargin: Theme.horizontalPageMargin
            }
        }

        PullDownMenu {
            MenuItem {
                text: qsTr("Send Destination")
                onClicked: pageStack.push(Qt.resolvedUrl("NavigationPage.qml"), { teslaClient: teslaClient })
            }
            MenuItem {
                text: qsTr("Pair Vehicle")
                onClicked: pageStack.push(Qt.resolvedUrl("PairingPage.qml"), { teslaClient: teslaClient })
            }
            MenuItem {
                text: qsTr("Settings")
                onClicked: pageStack.push(Qt.resolvedUrl("SettingsPage.qml"), { teslaClient: teslaClient })
            }
            MenuItem {
                text: qsTr("Refresh")
                onClicked: teslaClient.refreshConfig()
            }
            MenuItem {
                text: qsTr("Refresh Status")
                visible: page.hasKey && page.vin.length > 0
                onClicked: page.refreshStatus()
            }
        }

        VerticalScrollDecorator {}
    }
}
