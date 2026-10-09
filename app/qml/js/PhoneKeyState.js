.pragma library

// Stable wire states, independent of diagnostic wording.
function connectionKind(link, paired) {
    if (!paired || link === "unpaired")
        return "unpaired"
    switch (link) {
    case "bluetooth-off": return "bluetooth-off"
    case "connected":
    case "authorized": return "connected"
    default: return "disconnected"
    }
}

function isError(link) {
    return link === "error" || link === "bluetooth-off"
}
