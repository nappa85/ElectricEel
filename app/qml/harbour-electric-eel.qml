import QtQuick 2.6
import Sailfish.Silica 1.0
import Sailfish.Share 1.0
import Nemo.DBus 2.0
import harbour.electriceel 1.0
import "cover" as CoverDir
import "pages"

ApplicationWindow
{
    id: appWindow

    // Named teslaClientInstance, not teslaClient: FirstPage declares its own
    // "property var teslaClient", and inside an inline object literal like
    // "FirstPage { teslaClient: teslaClient }" QML resolves the right-hand
    // side against the new instance's own scope first - so a same-named
    // outer id gets shadowed by the not-yet-set property on the object
    // being constructed, silently binding it to itself (undefined).
    TeslaClient {
        id: teslaClientInstance
    }

    // Text received via Sailfish Share while the app wasn't showing the
    // Navigation page. Opened on arrival (activate() brings the window
    // forward — share doesn't do that by itself).
    property string pendingSharedText: ""

    function openNavigation(text) {
        appWindow.pendingSharedText = ""
        appWindow.activate()
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
        onTriggered: {
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
            if (text.length > 0)
                appWindow.openNavigation(text)
            else
                appWindow.openNavigation("")
        }
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
            var resources = shareAction.resources
            var text = ""
            for (var i = 0; i < resources.length; i++) {
                var r = resources[i]
                if (r.status)
                    text += (text.length > 0 ? "\n" : "") + r.status
                else if (r.data)
                    text += (text.length > 0 ? "\n" : "") + r.data
            }
            if (text.length > 0)
                appWindow.openNavigation(text)
        }
    }

    initialPage: Component {
        FirstPage {
            teslaClient: teslaClientInstance
        }
    }
    cover: CoverDir.CoverPage {
        teslaClient: teslaClientInstance
    }
    allowedOrientations: Orientation.All
}
