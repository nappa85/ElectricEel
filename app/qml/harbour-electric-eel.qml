import QtQuick 2.6
import Sailfish.Silica 1.0
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
