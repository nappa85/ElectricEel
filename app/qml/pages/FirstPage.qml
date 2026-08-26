import QtQuick 2.6
import Sailfish.Silica 1.0
import "../js/CommandCatalog.js" as Catalog
import "../js/VehicleState.js" as VState

Page {
    id: page
    property var teslaClient

    property string vin: ""
    property string model: ""
    property bool hasKey: false

    // Live dashboard state. refreshStatus() starts with body-controller-state
    // (VCSEC: lock/doors while the car sleeps), then chains the three
    // Infotainment `state` categories. Sequential because each is its own
    // BLE round-trip. statusStage tracks which leg is in flight so the UI
    // can show a single busy indicator and refuse to stack requests.
    property var vehicleStatus: VState.emptyStatus()
    property string statusStage: ""
    // Set from stdErr when a status leg fails. Climate/charge/closures go
    // through Infotainment and fail on a sleeping vehicle; lock/unlock and
    // body-controller-state use VCSEC and still work then (see
    // VehicleState.js). A sleeping vehicle is the expected way for the
    // Infotainment legs to fail even with everything else configured.
    property string statusError: ""
    // Monotonic tick bumped by statusAgeTimer below. The dashboard's "Updated
    // Xm ago" label is computed from status.updatedAt, which only changes when
    // a refresh returns a new BLE reading - so without re-reading a changing
    // value here the label would freeze on "Updated just now" forever (even
    // after navigating away and back, since both pages stay alive).
    property int statusAgeTick: 0

    Timer {
        id: statusAgeTimer
        interval: 30000
        repeat: true
        running: page.vehicleStatus.updatedAt > 0
        onTriggered: page.statusAgeTick++
    }

    // Settle delay between a toggle command completing and re-fetching
    // status to confirm it. Firing that re-fetch immediately (the original
    // behavior) reads Infotainment's closures/climate telemetry before it
    // has caught up with what VCSEC (which lock/unlock/climate/windows all
    // go through) just actuated - so the "authoritative" re-fetch stomps
    // the optimistic update in toggleLock/etc. right back to the stale
    // pre-command value. Confirmed live: the optimistic update alone wasn't
    // enough: the icon looked unchanged because this immediate re-fetch was
    // overwriting it a moment later, not because the optimistic update
    // itself failed to apply.
    Timer {
        id: toggleSettleTimer
        interval: 2500
        repeat: false
        onTriggered: page.refreshStatus()
    }

    function refresh() {
        teslaClient.refreshHelperAvailable()
        teslaClient.refreshConfig()
    }

    function refreshStatus() {
        if (!teslaClient.helperAvailable || !page.hasKey || page.vin.length === 0 || page.statusStage.length > 0)
            return
        page.statusError = ""
        page.statusStage = "body"
        teslaClient.runCommand("status:body", "body-controller-state", [])
    }

    // Flips field to !field's current value on a clone of vehicleStatus and
    // assigns it (a plain in-place mutation wouldn't trigger the QML
    // property's change notification), so the icon reflects the command
    // we're about to send immediately rather than waiting on refreshStatus's
    // re-fetch. That re-fetch still runs right after (see onCommandFinished)
    // and is what's actually authoritative - this is just to stop the UI
    // showing a stale icon for the second or so of real lag between VCSEC
    // physically actuating (lock/unlock, climate, windows are all VCSEC
    // commands) and Infotainment's own closures/climate telemetry catching
    // up, which is what "closures"/"climate" reads afterward. Confirmed live:
    // without this, each button press displayed the *previous* press's
    // now-settled result, one step behind.
    function optimistic(field) {
        var next = VState.clone(page.vehicleStatus)
        next[field] = !page.vehicleStatus[field]
        page.vehicleStatus = next
    }

    function toggleLock() {
        if (page.statusStage.length > 0)
            return
        // The padlock icon shows locked unless we *know* the car is unlocked
        // (`locked === false`). Match that: unknown/null used to be falsy and
        // sent lock, so the first tap did nothing until Refresh Status filled
        // in `locked`. Unlock unless we know the doors are already open.
        var sendLock = page.vehicleStatus.locked === false
        optimistic("locked")
        page.statusStage = "toggle"
        teslaClient.runCommand("status:toggle", sendLock ? "lock" : "unlock", [])
    }

    function toggleClimate() {
        if (page.statusStage.length > 0)
            return
        var wasOn = page.vehicleStatus.isClimateOn
        optimistic("isClimateOn")
        page.statusStage = "toggle"
        teslaClient.runCommand("status:toggle", wasOn ? "climate-off" : "climate-on", [])
    }

    function toggleWindows() {
        if (page.statusStage.length > 0)
            return
        var wereOpen = page.vehicleStatus.windowsOpen
        optimistic("windowsOpen")
        page.statusStage = "toggle"
        teslaClient.runCommand("status:toggle", wereOpen ? "windows-close" : "windows-vent", [])
    }

    function openFrunk() {
        if (page.statusStage.length > 0)
            return
        page.statusStage = "toggle"
        teslaClient.runCommand("status:toggle", "frunk-open", [])
    }

    function openTrunk() {
        if (page.statusStage.length > 0)
            return
        page.statusStage = "toggle"
        teslaClient.runCommand("status:toggle", "trunk-open", [])
    }

    function batteryIconSource() {
        var level = page.vehicleStatus.batteryLevel
        if (level === null) return "../../img/icons/battery5.svg"
        if (level > 90) return "../../img/icons/battery100.svg"
        if (level > 60) return "../../img/icons/battery75.svg"
        if (level > 40) return "../../img/icons/battery50.svg"
        if (level > 9) return "../../img/icons/battery10.svg"
        return "../../img/icons/battery5.svg"
    }

    Connections {
        target: teslaClient
        onConfigLoaded: {
            page.vin = vin
            page.model = model
            page.hasKey = hasKey
            page.refreshStatus()
        }
    }

    Connections {
        target: teslaClient
        onCommandFinished: {
            if (requestId === "status:body") {
                if (ok)
                    page.vehicleStatus = VState.mergeBodyControllerState(page.vehicleStatus, stdOut)
                else
                    page.statusError = stdErr.length ? stdErr : ("exit code " + exitCode)
                page.statusStage = "closures"
                teslaClient.runCommand("status:closures", "state", ["closures"])
            } else if (requestId === "status:closures") {
                if (ok)
                    page.vehicleStatus = VState.mergeClosuresState(page.vehicleStatus, stdOut)
                else if (page.statusError.length === 0)
                    page.statusError = stdErr.length ? stdErr : ("exit code " + exitCode)
                page.statusStage = "climate"
                teslaClient.runCommand("status:climate", "state", ["climate"])
            } else if (requestId === "status:climate") {
                if (ok)
                    page.vehicleStatus = VState.mergeClimateState(page.vehicleStatus, stdOut)
                else if (page.statusError.length === 0)
                    page.statusError = stdErr.length ? stdErr : ("exit code " + exitCode)
                page.statusStage = "charge"
                teslaClient.runCommand("status:charge", "state", ["charge"])
            } else if (requestId === "status:charge") {
                if (ok)
                    page.vehicleStatus = VState.mergeChargeState(page.vehicleStatus, stdOut)
                else if (page.statusError.length === 0)
                    page.statusError = stdErr.length ? stdErr : ("exit code " + exitCode)
                page.statusStage = ""
            } else if (requestId === "status:toggle") {
                // Whether the lock/climate/windows toggle or the frunk/trunk
                // open succeeded or not, re-fetch so the dashboard reflects
                // reality rather than an assumed new state (the command can
                // "succeed" over BLE without the vehicle confirming - see
                // protocol.MayHaveSucceeded usage elsewhere in this app).
                // Delayed via toggleSettleTimer, not fired immediately - see
                // its doc comment for why.
                page.statusStage = ""
                toggleSettleTimer.restart()
            }
        }
        onCommandError: {
            if (requestId.indexOf("status:") === 0) {
                page.statusStage = ""
                if (page.statusError.length === 0)
                    page.statusError = message
                // status:toggle's optimistic() flip (toggleLock/toggleClimate/
                // toggleWindows) assumed the command would reach the vehicle -
                // a hard failure (busy adapter, no session, core not
                // initialized) means it never did, so the icon is now
                // showing a state that was never actually requested. Re-fetch
                // the same way onCommandFinished does for a soft (ok=false)
                // failure, rather than leaving the wrong icon up until the
                // user happens to pull down to refresh.
                if (requestId === "status:toggle")
                    toggleSettleTimer.restart()
            }
        }
    }

    // Covers both the initial load (this page becomes Active as soon as it's
    // pushed) and returning here later. Sailfish keeps popped/pushed pages
    // alive on the stack rather than recreating them, so a plain
    // Component.onCompleted would only ever fire once - saving a VIN/model in
    // SettingsPage and navigating back would then leave page.vin/page.model
    // stale (whatever the very first refresh() returned) until the user
    // pulled down "Refresh" manually.
    onStatusChanged: {
        if (status === PageStatus.Active)
            refresh()
    }

    SilicaListView {
        id: listView
        anchors.fill: parent
        model: Catalog.CATEGORIES

        header: Column {
            width: listView.width

            PageHeader {
                title: "ElectricEel"
            }

            // Warns when the helper half is missing, too old to report its
            // own version, or a different version than this app. All three
            // are the states that previously announced themselves as a
            // silent "No VIN configured" (see KNOWN_ISSUES.md); GetVersion
            // + APP_VERSION make them visible up front instead.
            Rectangle {
                id: versionBanner
                property bool helperProblem: !teslaClient.helperAvailable
                        || teslaClient.helperVersion.length === 0
                        || teslaClient.helperVersion !== teslaClient.appVersion
                width: parent.width
                height: helperWarning.visible ? helperWarning.height + Theme.paddingMedium * 2 : 0
                color: Theme.rgba(Theme.highlightBackgroundColor, 0.15)
                visible: helperProblem

                Column {
                    id: helperWarning
                    anchors.centerIn: parent
                    width: parent.width - Theme.horizontalPageMargin * 2
                    visible: versionBanner.helperProblem
                    spacing: Theme.paddingSmall

                    Label {
                        width: parent.width
                        wrapMode: Text.Wrap
                        color: Theme.highlightColor
                        font.pixelSize: Theme.fontSizeSmall
                        text: !teslaClient.helperAvailable
                            ? "The control core failed to start. Reinstall the app, then pull down to refresh."
                            : teslaClient.helperVersion.length === 0
                                ? "The control core is too old to report its version. Reinstall the app (" + teslaClient.appVersion + "), then pull down to refresh."
                                : "Version mismatch: app " + teslaClient.appVersion + ", core " + teslaClient.helperVersion + ". Reinstall the app, then pull down to refresh."
                    }
                }
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
                        text: page.vin.length > 0 ? page.vin : "No VIN configured"
                        color: page.vin.length > 0 ? Theme.primaryColor : Theme.secondaryColor
                    }
                    Label {
                        text: page.hasKey ? "Key ready" : "No key - tap for Settings / Pairing"
                        font.pixelSize: Theme.fontSizeExtraSmall
                        color: page.hasKey ? Theme.secondaryHighlightColor : Theme.secondaryColor
                    }
                }
            }

            // Car graphic + battery / climate readouts. The icon set and
            // presentation mirror the harbour-tcarint app this project shares
            // its icon style with (see ../harbour-tcarint/qml/pages/MainPage.qml).
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
                        height: 170
                        anchors.horizontalCenter: parent.horizontalCenter
                        fillMode: Image.PreserveAspectFit
                        // An explicit config model override wins; otherwise
                        // the VIN is parsed for the model. See VehicleState.js
                        // carImageSource().
                        source: VState.carImageSource(page.model, page.vin)
                        sourceSize: Qt.size(width * 2, height * 2)
                    }

                    Row {
                        anchors.horizontalCenter: parent.horizontalCenter
                        spacing: Theme.paddingMedium

                        Icon {
                            id: batteryIcon
                            source: page.batteryIconSource()
                            height: 48
                            width: height
                        }

                        Label {
                            id: batteryLabel
                            anchors.verticalCenter: batteryIcon.verticalCenter
                            font.pixelSize: Theme.fontSizeSmall
                            color: Theme.primaryColor
                            text: page.vehicleStatus.batteryLevel === null ? "--" :
                                  (page.vehicleStatus.batteryLevel + "%" +
                                   (page.vehicleStatus.chargingState === "Charging" ? " • charging" : ""))
                        }

                        Label {
                            id: tempLabel
                            anchors.verticalCenter: batteryIcon.verticalCenter
                            visible: page.vehicleStatus.insideTemp !== null
                            font.pixelSize: Theme.fontSizeSmall
                            color: Theme.secondaryColor
                            text: "• " + page.vehicleStatus.insideTemp.toFixed(0) + "°C"
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
                                    return "Updating..."
                                if (page.statusError.length > 0 && age < 0)
                                    return "Status unavailable (" + page.statusError + "). Vehicle may be asleep - try Wake Vehicle (Attention), then Refresh Status."
                                if (age < 0)
                                    return "Pull down to refresh status"
                                if (age === 0)
                                    return "Updated just now"
                                return "Updated " + age + "m ago"
                            }
                        }
                    }
                }
            }

            // Quick actions: small icon buttons straight on the front page -
            // no submenu needed. Each icon always mirrors the vehicle's actual
            // state (closed padlock when locked, open padlock when unlocked,
            // highlighted fan/window when climate/windows are on), not the
            // action a tap triggers, so the row can't be misread as showing
            // the pre-tap state. Frunk/trunk have no telemetry to reflect,
            // so those stay plain "open" actions.
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

                    SecondaryButton {
                        width: 96
                        height: 96
                        icon.height: 48
                        icon.width: 48
                        icon.source: page.vehicleStatus.locked === false
                                    ? "../../img/icons/lock_open.svg"
                                    : "../../img/icons/lock.svg"
                        icon.color: "#5f6368"
                        icon.highlightColor: "white"
                        icon.highlighted: !!page.vehicleStatus.locked
                        enabled: page.statusStage.length === 0
                        onClicked: page.toggleLock()
                    }

                    SecondaryButton {
                        width: 96
                        height: 96
                        icon.height: 48
                        icon.width: 48
                        icon.source: "../../img/icons/fan1.svg"
                        icon.color: "#5f6368"
                        icon.highlightColor: "white"
                        icon.highlighted: page.vehicleStatus.isClimateOn
                        enabled: page.statusStage.length === 0
                        onClicked: page.toggleClimate()
                    }

                    SecondaryButton {
                        width: 96
                        height: 96
                        icon.height: 48
                        icon.width: 48
                        icon.source: "../../img/icons/frunk.svg"
                        icon.color: "#5f6368"
                        icon.highlightColor: "white"
                        enabled: page.statusStage.length === 0
                        onClicked: page.openFrunk()
                    }

                    SecondaryButton {
                        width: 96
                        height: 96
                        icon.height: 48
                        icon.width: 48
                        icon.source: "../../img/icons/trunk.svg"
                        icon.color: "#5f6368"
                        icon.highlightColor: "white"
                        enabled: page.statusStage.length === 0
                        onClicked: page.openTrunk()
                    }

                    SecondaryButton {
                        width: 96
                        height: 96
                        icon.height: 48
                        icon.width: 48
                        icon.source: "../../img/icons/window.svg"
                        icon.color: "#5f6368"
                        icon.highlightColor: "white"
                        icon.highlighted: page.vehicleStatus.windowsOpen
                        enabled: page.statusStage.length === 0
                        onClicked: page.toggleWindows()
                    }
                }
            }

            SectionHeader { text: "Categories" }
        }

        delegate: BackgroundItem {
            width: listView.width
            height: Theme.itemSizeMedium

            onClicked: pageStack.push(Qt.resolvedUrl("CategoryPage.qml"), {
                teslaClient: teslaClient,
                categoryId: modelData.id,
                categoryTitle: modelData.title,
                vehicleStatus: page.vehicleStatus
            })

            Icon {
                id: categoryIcon
                source: modelData.icon
                height: 44
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
                height: 40
                width: height
                color: "#5f6368"
                anchors.verticalCenter: parent.verticalCenter
                anchors.right: parent.right
                anchors.rightMargin: Theme.horizontalPageMargin
            }
        }

        PullDownMenu {
            MenuItem {
                text: "Pair Vehicle"
                onClicked: pageStack.push(Qt.resolvedUrl("PairingPage.qml"), { teslaClient: teslaClient })
            }
            MenuItem {
                text: "Settings"
                onClicked: pageStack.push(Qt.resolvedUrl("SettingsPage.qml"), { teslaClient: teslaClient })
            }
            MenuItem {
                text: "Refresh"
                onClicked: page.refresh()
            }
            MenuItem {
                text: "Refresh Status"
                visible: page.hasKey && page.vin.length > 0
                onClicked: page.refreshStatus()
            }
        }

        VerticalScrollDecorator {}
    }
}
