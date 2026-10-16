import QtQuick 2.6
import Sailfish.Silica 1.0
import Sailfish.Share 1.0
import Nemo.DBus 2.0
import Nemo.KeepAlive 1.2
import "cover" as CoverDir
import "pages"

ApplicationWindow
{
    id: appWindow

    // Rust supplies the client through the UI context. Named
    // teslaClientInstance, not teslaClient: FirstPage declares its own
    // "property var teslaClient", and inside an inline object literal like
    // "FirstPage { teslaClient: teslaClient }" QML resolves the right-hand
    // side against the new instance's own scope first - so a same-named
    // outer id gets shadowed by the not-yet-set property on the object
    // being constructed, silently binding it to itself (undefined).
    property var teslaClientInstance: electricEelClient
    // The dashboard's latest vehicle telemetry, mirrored for the status
    // notification (FirstPage owns the refresh chain).
    property var vehicleStatus: null
    Connections {
        target: DisplayBlanking
        onStatusChanged: teslaClientInstance.logPowerState("display status=" + DisplayBlanking.status)
    }
    Component.onCompleted: {
        teslaClientInstance.logPowerState("display status=" + DisplayBlanking.status)
        // The single startup configuration read. TeslaClient publishes the
        // result as properties; pages bind to them instead of re-reading.
        teslaClientInstance.refreshConfig()
    }

    PhoneKeyEvents { client: teslaClientInstance }

    StatusNotification {
        client: teslaClientInstance
        vehicleStatus: appWindow.vehicleStatus
        activateApp: function() { appWindow.activate() }
    }

    // Single helper for both share entry points (ShareProvider and the
    // Browser-quirk DBusAdaptor): same resource priority (data, then
    // status), same empty-text behavior (open with "" so the page shows its
    // hint instead of silently dropping).
    function extractSharedText(resources) {
        var text = ""
        for (var i = 0; i < resources.length; i++) {
            var r = resources[i]
            // StringData shape: {name, data}; join multi-shares
            // line-wise, the parser prefers an embedded map URL.
            if (r.data)
                text += (text.length > 0 ? "\n" : "") + r.data
            else if (r.status)
                text += (text.length > 0 ? "\n" : "") + r.status
        }
        return text
    }

    function openNavigation(text) {
        appWindow.activate()
        var navigation = pageStack.find(function(candidate) {
            return candidate.objectName === "navigationPage"
        })
        if (navigation) {
            if (pageStack.currentPage !== navigation)
                pageStack.pop(navigation)
            navigation.setSharedText(text)
            return
        }
        pageStack.push(Qt.resolvedUrl("pages/NavigationPage.qml"), {
            teslaClient: teslaClientInstance,
            initialText: text
        })
    }

    // Well-formed shares (Maps/Notes/... sending text/plain with name+data).
    ShareProvider {
        method: "destination"
        capabilities: ["text/plain", "text/x-url"]
        registerName: true
        onTriggered: appWindow.openNavigation(appWindow.extractSharedText(resources))
    }

    // Used by the Browser-quirk adaptor below to read the raw share
    // configuration the same way a ShareAction would.
    ShareAction { id: shareAction }

    // The Browser/WebView shares text/x-url as {type, linkTitle, status}
    // WITHOUT the name/data keys ShareResource requires, so the
    // ShareProvider above errors with "No usable resources" for it (see
    // sailfish-components-webview#180). Same workaround as the Messages
    // and Email apps: serve org.sailfishos.share on /share/<method>
    // directly and read resources[0].status.
    DBusAdaptor {
        service: "org.electriceel.harbour-electric-eel"
        path: "/share/destination"
        iface: "org.sailfishos.share"

        function share(shareConfiguration) {
            shareAction.loadConfiguration(shareConfiguration)
            appWindow.openNavigation(appWindow.extractSharedText(shareAction.resources))
        }
    }

    initialPage: Component {
        FirstPage {
            teslaClient: teslaClientInstance
            onVehicleStatusChanged: appWindow.vehicleStatus = vehicleStatus
        }
    }
    cover: CoverDir.CoverPage {
        teslaClient: teslaClientInstance
    }
    allowedOrientations: Orientation.All
}
