.pragma library

// Shared phone-key connection mapping used by the cover badge and the
// dashboard. Both surfaces must agree: a live GATT session is "connected"
// even when Infotainment telemetry (state climate/charge/...) fails because
// the vehicle is asleep.

function isBluetoothOff(status) {
    if (!status)
        return false
    return status.indexOf("NotPowered") >= 0
        || status.indexOf("RFKILL") >= 0
        || status.indexOf("power on adapter") >= 0
}

function isConnected(status) {
    return status === "Phone key connected"
        || status === "Phone key authorized"
}

function isError(status) {
    if (!status)
        return false
    return status.indexOf("Phone key error") >= 0
        || status.indexOf("error") >= 0
}

// unpaired | bluetooth-off | connected | disconnected | error
function connectionKind(status, paired) {
    if (isBluetoothOff(status))
        return "bluetooth-off"
    if (!paired)
        return "unpaired"
    if (isConnected(status))
        return "connected"
    if (isError(status))
        return "error"
    return "disconnected"
}

function label(status, paired) {
    var kind = connectionKind(status, paired)
    if (kind === "unpaired")
        return ""
    if (kind === "bluetooth-off")
        return "Bluetooth off"
    if (kind === "connected")
        return status && status.length ? status : "Phone key connected"
    if (kind === "error")
        return status && status.length ? status : "Phone key error"
    if (status && status.length)
        return status
    return "Phone key scanning"
}
