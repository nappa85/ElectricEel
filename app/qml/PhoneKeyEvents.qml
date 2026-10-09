import QtQuick 2.6
import Nemo.DBus 2.0

// Reuses the session-bus name owned by the main window's Share adaptor.
// No remote command methods: this is an introspectable event source.
Item {
    id: root
    property var client: null

    DBusAdaptor {
        id: eventBus
        bus: DBus.SessionBus
        path: "/org/electriceel/PhoneKey"
        iface: "org.electriceel.PhoneKey1"
        xml: '<interface name="org.electriceel.PhoneKey1">'
             + '<signal name="PhoneKeyEvent">'
             + '<arg name="kind" type="s"/>'
             + '<arg name="vin" type="s"/>'
             + '<arg name="time" type="s"/>'
             + '<arg name="error" type="s"/>'
             + '</signal>'
             + '<signal name="PhoneKeyStateChanged">'
             + '<arg name="active" type="b"/>'
             + '<arg name="link" type="s"/>'
             + '<arg name="status" type="s"/>'
             + '</signal></interface>'
    }

    Connections {
        target: root.client
        ignoreUnknownSignals: true
        onPhoneKeyEvent: eventBus.emitSignal("PhoneKeyEvent", [kind, vin, time, errorMessage])
        onPhoneKeyStateChanged: eventBus.emitSignal("PhoneKeyStateChanged", [active, link, status])
    }
}
