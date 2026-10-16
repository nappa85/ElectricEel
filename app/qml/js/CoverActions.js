.pragma library

// Keep the IDs in sync with helper/src/cover_action.rs. Cover commands must
// need no arguments or dialog; icons are relative to pages/ and cover/.
var ACTIONS = [
    { id: "frunk-open", title: qsTr("Open front trunk"), icon: "../../img/icons/frunk.svg" },
    { id: "trunk-open", title: qsTr("Open rear trunk"), icon: "../../img/icons/trunk.svg" },
    { id: "lock", title: qsTr("Lock"), icon: "../../img/icons/lock.svg" },
    { id: "unlock", title: qsTr("Unlock"), icon: "../../img/icons/lock_open.svg" },
    { id: "flash-lights", title: qsTr("Flash lights"), icon: "../../img/icons/bolt.svg" },
    { id: "honk", title: qsTr("Honk horn"), icon: "image://theme/icon-cover-alarm" }
]

function indexOf(id, fallback) {
    for (var i = 0; i < ACTIONS.length; i++) {
        if (ACTIONS[i].id === id)
            return i
    }
    return fallback
}

function action(id, fallback) {
    return ACTIONS[indexOf(id, fallback)]
}
