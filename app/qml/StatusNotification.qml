import QtQuick 2.6
import Nemo.DBus 2.0
import Nemo.Notifications 1.0
import "js/CommandCatalog.js" as Catalog
import "js/CommandText.js" as CommandText
import "js/CoverActions.js" as CoverActions
import "js/PhoneKeyState.js" as PhoneKey
import "js/StatusSummary.js" as Summary

// Persistent, silently updated status notification with the quick actions
// chosen in Settings. A Harbour-safe stand-in for an Events view widget:
// third-party widgets need files outside /usr/share/<app>, which Harbour
// rejects. Actions call back into this process over the app's own D-Bus
// name, so they work while ElectricEel runs (it removes the notification on
// exit).
Item {
    id: root
    property var client: null
    property var vehicleStatus: null
    // Opens the main window; set by the application window.
    property var activateApp: null

    readonly property string dbusService: "org.electriceel.harbour-electric-eel"
    readonly property string dbusPath: "/org/electriceel/QuickAction"
    readonly property string dbusInterface: "org.electriceel.QuickAction1"

    readonly property bool enabled: !!client && client.notificationEnabled && client.configReady
    readonly property bool paired: !!client && client.hasKey && client.vin.length > 0
    readonly property var actionIds: client ? Summary.offeredActions(client.notificationActions, CoverActions.ACTIONS) : []

    // Outcome of the last notification action, shown until the next one.
    property string actionText: ""
    property string pendingRequestId: ""
    property string pendingAction: ""
    // When a notification button last called us: lipstick may close the
    // notification as part of invoking an action, which is not a dismissal.
    property real lastActionAt: 0
    // Dismissed by the user: stay away until the settings change or restart.
    property bool dismissed: false
    property string publishedKey: ""

    readonly property string summaryText: client && client.vehicleName.length > 0
                                          ? client.vehicleName : "ElectricEel"
    readonly property string bodyText: {
        var parts = Summary.statusParts(
            PhoneKey.connectionKind(client ? client.phoneKeyLink : "stopped", paired),
            vehicleStatus,
            {
                connected: qsTr("Connected"),
                disconnected: qsTr("Not connected"),
                bluetoothOff: qsTr("Bluetooth off"),
                unpaired: qsTr("Not paired"),
                charging: qsTr("Charging"),
                locked: qsTr("Locked"),
                unlocked: qsTr("Unlocked")
            },
            Catalog.nearlyCharging)
        if (actionText.length > 0)
            parts.push(actionText)
        return parts.join(" · ")
    }

    function actionTitle(id) {
        return CoverActions.action(id, 0).title
    }

    function sync() {
        if (!enabled || dismissed) {
            if (publishedKey.length > 0) {
                notification.close()
                publishedKey = ""
            }
            return
        }
        // Republish only on visible changes: each publish is a D-Bus round
        // trip to lipstick.
        var key = summaryText + "\n" + bodyText + "\n" + actionIds.join(",")
        if (key === publishedKey)
            return
        var actions = [{
            name: "default",
            displayName: qsTr("Open ElectricEel"),
            service: dbusService, path: dbusPath, iface: dbusInterface,
            method: "openApp", arguments: []
        }]
        for (var i = 0; i < actionIds.length; i++) {
            actions.push({
                name: "action-" + actionIds[i],
                displayName: actionTitle(actionIds[i]),
                service: dbusService, path: dbusPath, iface: dbusInterface,
                method: "runAction", arguments: [actionIds[i]]
            })
        }
        notification.summary = summaryText
        notification.body = bodyText
        notification.remoteActions = actions
        notification.publish()
        publishedKey = key
    }

    function runAction(id) {
        lastActionAt = Date.now()
        if (!enabled || !paired || pendingRequestId.length > 0)
            return
        // Only the actions offered on the notification: the D-Bus method is
        // reachable by any session process.
        // No second tap here: the buttons only appear after expanding the
        // notification, which already guards against stray touches.
        if (actionIds.indexOf(id) < 0)
            return
        pendingAction = id
        pendingRequestId = client.newRequestId("notification:" + id, client.vin)
        actionText = qsTr("%1…").arg(actionTitle(id))
        client.runCommand(pendingRequestId, id, [], Catalog.refreshesStatus(id))
    }

    Notification {
        id: notification
        appName: "ElectricEel"
        appIcon: "harbour-electric-eel"
        // No preview: updates land silently in the Events view.
        previewSummary: ""
        previewBody: ""
        urgency: Notification.Low
        isTransient: false
        expireTimeout: 0
        onClosed: {
            // 2: dismissed by the user. Other reasons (our own close,
            // expiry) are not a request to stay hidden.
            if (reason === 2 && root.publishedKey.length > 0
                    && Date.now() - root.lastActionAt > 2000) {
                root.dismissed = true
                root.publishedKey = ""
            }
        }
    }

    DBusAdaptor {
        service: root.dbusService
        path: root.dbusPath
        iface: root.dbusInterface
        xml: '<interface name="org.electriceel.QuickAction1">'
             + '<method name="runAction"><arg name="action" type="s" direction="in"/></method>'
             + '<method name="openApp"/>'
             + '</interface>'

        function runAction(action) { root.runAction(String(action)) }
        function openApp() {
            root.lastActionAt = Date.now()
            if (root.activateApp)
                root.activateApp()
        }
    }

    Connections {
        target: root.client
        ignoreUnknownSignals: true
        onCommandFinished: {
            if (requestId !== root.pendingRequestId)
                return
            root.actionText = ok ? qsTr("%1 done").arg(root.actionTitle(root.pendingAction))
                                 : root.actionTitle(root.pendingAction) + ": "
                                   + CommandText.failure(stdErr, exitCode)
            root.pendingRequestId = ""
        }
        onCommandError: {
            if (requestId !== root.pendingRequestId)
                return
            root.actionText = root.actionTitle(root.pendingAction) + ": " + message
            root.pendingRequestId = ""
        }
        onNotificationSettingsChanged: root.dismissed = false
        onVinChanged: {
            root.actionText = ""
            root.pendingRequestId = ""
        }
    }

    onEnabledChanged: sync()
    onDismissedChanged: sync()
    onSummaryTextChanged: sync()
    onBodyTextChanged: sync()
    onActionIdsChanged: sync()
    Component.onCompleted: {
        // Lipstick deletes non-resident notifications after any action.
        notification.setHintValue("resident", true)
        sync()
    }
    Component.onDestruction: {
        if (publishedKey.length > 0)
            notification.close()
    }
}
