.pragma library

// Compact status line pieces for the status notification: connection,
// battery, charging and lock state, skipping what is unknown.
function statusParts(connectionKind, status, labels, nearlyCharging) {
    var parts = []
    if (connectionKind === "connected") parts.push(labels.connected)
    else if (connectionKind === "bluetooth-off") parts.push(labels.bluetoothOff)
    else if (connectionKind === "unpaired") parts.push(labels.unpaired)
    else parts.push(labels.disconnected)
    if (!status)
        return parts
    if (status.batteryLevel !== null && status.batteryLevel !== undefined)
        parts.push(status.batteryLevel + "%")
    if (nearlyCharging(status))
        parts.push(labels.charging)
    if (status.locked === true) parts.push(labels.locked)
    else if (status.locked === false) parts.push(labels.unlocked)
    return parts
}

// The configured action ids that exist in the catalogue, in order, once each.
function offeredActions(ids, catalogue) {
    var known = {}
    for (var i = 0; i < catalogue.length; i++)
        known[catalogue[i].id] = true
    var offered = []
    for (var j = 0; ids && j < ids.length; j++) {
        var id = String(ids[j])
        if (known[id] && offered.indexOf(id) < 0)
            offered.push(id)
    }
    return offered
}
