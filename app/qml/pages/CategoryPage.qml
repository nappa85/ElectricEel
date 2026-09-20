import QtQuick 2.6
import Sailfish.Silica 1.0
import "../js/CommandCatalog.js" as Catalog

Page {
    id: page
    property var teslaClient
    property string categoryId
    property string categoryTitle
    // Last dashboard reading from FirstPage, passed through at push time -
    // shown as a subheader below. Not re-fetched here: it's a snapshot, not
    // a live binding, so it can go stale while this page is open (matches
    // FirstPage's own "Updated Xm ago" label, which is visible again as
    // soon as the user goes back).
    property var vehicleStatus: null

    property var category: Catalog.findCategory(categoryId)
    property string pendingCmd: ""
    property string lastResult: ""
    property bool lastOk: true

    // Commands gated by status (see CommandCatalog's visibleWhen): only the
    // toggle action matching the current vehicle state is listed, so the page
    // mirrors reality instead of offering a mix of "on" and "off" rows. The
    // status here is the snapshot FirstPage passed at push time, same as the
    // subtitle - intentionally not re-fetched live.
    property var visibleCommands: page.category
                                   ? page.category.commands.filter(function(commandDef) { return page.commandVisible(commandDef) })
                                   : []

    function commandVisible(commandDef) {
        if (!page.vehicleStatus || !commandDef.visibleWhen)
            return true
        return commandDef.visibleWhen(page.vehicleStatus)
    }

    function statusSubtitle() {
        if (!vehicleStatus)
            return ""
        if (categoryId === "climate") {
            if (vehicleStatus.insideTemp === null)
                return ""
            return qsTr("%1 • %2°C inside")
                .arg(vehicleStatus.isClimateOn ? qsTr("Climate on") : qsTr("Climate off"))
                .arg(vehicleStatus.insideTemp.toFixed(0))
        }
        if (categoryId === "charging") {
            if (vehicleStatus.batteryLevel === null)
                return ""
            return qsTr("%1% battery%2")
                .arg(vehicleStatus.batteryLevel)
                .arg(vehicleStatus.chargingState === "Charging" ? qsTr(" • Charging") : "")
        }
        if (categoryId === "security") {
            if (vehicleStatus.locked === null)
                return ""
            return vehicleStatus.locked ? qsTr("Doors locked") : qsTr("Doors unlocked")
        }
        return ""
    }

    Connections {
        target: teslaClient
        onCommandFinished: {
            if (requestId !== page.pendingCmd)
                return
            page.pendingCmd = ""
            page.lastOk = ok
            page.lastResult = ok ? (stdOut.length ? stdOut : qsTr("OK")) : (stdErr.length ? stdErr : qsTr("exit code %1").arg(exitCode))
        }
        onCommandError: {
            if (requestId !== page.pendingCmd)
                return
            page.pendingCmd = ""
            page.lastOk = false
            page.lastResult = message
        }
    }

    function execute(commandDef, argValues) {
        page.pendingCmd = commandDef.id
        page.lastResult = ""
        teslaClient.runCommand(commandDef.id, commandDef.id, argValues || [])
    }

    function openArgs(commandDef) {
        var dialog = pageStack.push(Qt.resolvedUrl("ArgumentDialog.qml"), { commandDef: page.commandWithStateDefaults(commandDef) })
        dialog.accepted.connect(function() {
            execute(commandDef, dialog.values)
        })
    }

    // Clones a commandDef whose args' `def` values are seeded from the last
    // status snapshot wherever the catalog marked a `stateDefault` field and
    // the reading exists and is within the arg's own min/max. The clone is
    // required because the catalog (CommandCatalog.js) is a shared library
    // singleton - writing `def` in place would poison every future dialog.
    // Fallback to the catalog's hardcoded `def` when status isn't loaded.
    function commandWithStateDefaults(commandDef) {
        if (!page.vehicleStatus || !commandDef.args || commandDef.args.length === 0)
            return commandDef
        var copy = {
            id: commandDef.id,
            label: commandDef.label,
            visibleWhen: commandDef.visibleWhen,
            args: commandDef.args.map(function(a) {
                var c = {}
                for (var k in a)
                    c[k] = a[k]
                var cur = page.vehicleStatus[c.stateDefault]
                if (c.stateDefault && cur !== undefined && cur !== null
                        && (c.min === undefined || cur >= c.min)
                        && (c.max === undefined || cur <= c.max)) {
                    c.def = cur
                    delete c.__value
                }
                return c
            })
        }
        return copy
    }

    SilicaListView {
        id: listView
        anchors.fill: parent
        model: page.visibleCommands

        header: Column {
            width: listView.width

            PageHeader { title: categoryTitle }

            Label {
                width: parent.width - Theme.horizontalPageMargin * 2
                anchors.horizontalCenter: parent.horizontalCenter
                visible: text.length > 0
                wrapMode: Text.Wrap
                font.pixelSize: Theme.fontSizeExtraSmall
                color: Theme.secondaryColor
                text: page.statusSubtitle()
            }

            Rectangle {
                width: parent.width
                height: resultLabel.height + Theme.paddingMedium * 2
                visible: page.pendingCmd.length > 0 || page.lastResult.length > 0
                color: Theme.rgba(page.lastOk ? Theme.secondaryHighlightColor : Theme.highlightBackgroundColor, 0.15)

                Row {
                    id: resultLabel
                    anchors.left: parent.left
                    anchors.right: parent.right
                    anchors.margins: Theme.horizontalPageMargin
                    anchors.verticalCenter: parent.verticalCenter
                    spacing: Theme.paddingSmall

                    BusyIndicator {
                        size: BusyIndicatorSize.Small
                        running: page.pendingCmd.length > 0
                        visible: running
                    }
                    Label {
                        width: parent.width - (page.pendingCmd.length > 0 ? Theme.itemSizeSmall : 0)
                        wrapMode: Text.Wrap
                        font.pixelSize: Theme.fontSizeExtraSmall
                        color: page.lastOk ? Theme.primaryColor : Theme.highlightColor
                        text: page.pendingCmd.length > 0 ? qsTr("Running %1...").arg(page.pendingCmd) : page.lastResult
                    }
                }
            }
        }

        delegate: BackgroundItem {
            width: listView.width
            height: Theme.itemSizeMedium
            enabled: page.pendingCmd.length === 0

            onClicked: {
                if (modelData.args && modelData.args.length > 0)
                    page.openArgs(modelData)
                else
                    page.execute(modelData, [])
            }

            Label {
                anchors.verticalCenter: parent.verticalCenter
                anchors.left: parent.left
                anchors.leftMargin: Theme.horizontalPageMargin
                anchors.right: parent.right
                anchors.rightMargin: Theme.horizontalPageMargin
                text: modelData.label
                truncationMode: TruncationMode.Fade
            }
        }

        VerticalScrollDecorator {}
    }
}
