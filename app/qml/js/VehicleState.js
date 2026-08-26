// Parses the protojson output of `tesla-control state <category>` (run via
// TeslaClient.runCommand("state", [category])) into a flat status object the
// dashboard binds to. One category per BLE round-trip - see FirstPage.qml's
// refreshStatus() for why they're chained sequentially rather than fired in
// parallel.
//
// Two things that are easy to get wrong here, both confirmed against the
// v0.4.1 tag this project pins (cmd/tesla-control/commands.go,
// pkg/vehicle/state.go, pkg/protocol/protobuf/vehicle.proto) after the
// first version of this file got both wrong at once:
//
// 1. CATEGORY argument values are tesla-control's own short names ("charge",
//    "climate", "closures", ...), not the Fleet API's "_state"-suffixed
//    vehicle_data JSON section names. GetCategory() fails fast on an unknown
//    name (before any BLE I/O), so passing the wrong name looks like the
//    command silently does nothing rather than an obvious error.
// 2. `car.GetState()` returns a `VehicleData` wrapper message, so the
//    parsed JSON is `{"closuresState": {...fields...}}`, not the fields
//    directly at the top level - each merge* function below unwraps its
//    category's field first.
//
// Also, per vehicle.proto: every state category is served over the
// Infotainment domain (car.GetState's own doc comment), same as
// climate/charge commands - unlike lock/unlock and body-controller-state,
// which go through VCSEC and work even while infotainment is asleep. So all
// three legs of the dashboard refresh can fail while lock/unlock keep
// working fine, if the vehicle is asleep. FirstPage.qml surfaces that as a
// hint to wake the vehicle rather than a bare "?".
//
// protojson.Format (used by tesla-control, see cmd/tesla-control/state.go)
// omits any field still at its zero value (false/0/"") instead of emitting
// it - so a missing key means "false"/"0", not "unknown". That's indistin-
// guishable from a genuinely-absent field here; merge* below treats both the
// same way (falls back to the zero value), which is the correct read for
// every field currently surfaced except battery level 0%, a real ambiguity
// this CLI wrapper has no way to resolve.

.pragma library

// Tesla-model metadata for the car graphic on the front page. The `id`
// doubles as the value of config.json's `model` field (see helper/src/
// config.rs): "" is "Auto" (guess from VIN, override nothing), the rest
// force that model's silhouette. Keep MODELS, config.rs's VALID_MODELS and
// the trylist guessModel() below in sync when adding models.
var MODELS = [
    { id: "",           name: "Auto (from VIN)", image: "" },
    { id: "model3",     name: "Model 3",     image: "../../img/model3.png" },
    { id: "models",     name: "Model S",     image: "../../img/models.png" },
    { id: "modelx",     name: "Model X",     image: "../../img/modelx.png" },
    { id: "modely",     name: "Model Y",     image: "../../img/modely.png" },
    { id: "cybertruck", name: "Cybertruck",  image: "../../img/cybertruck.png" },
]

function modelImage(id) {
    for (var i = 0; i < MODELS.length; i++) {
        if (MODELS[i].id === id && MODELS[i].image.length > 0)
            return MODELS[i].image
    }
    // Unknown/empty id (and the Auto entry, which has no image of its own)
    // fall back to the Model 3 silhouette.
    return "../../img/model3.png"
}

function modelName(id) {
    for (var i = 0; i < MODELS.length; i++) {
        if (MODELS[i].id === id)
            return MODELS[i].name
    }
    return "Model 3"
}

function modelIndex(id) {
    for (var i = 0; i < MODELS.length; i++) {
        if (MODELS[i].id === id)
            return i
    }
    return 0
}

// Effective model id to render on the front page: an explicit config
// override wins, otherwise the VIN guess (which modelImage() maps to the
// default silhouette when it comes back empty).
function effectiveModel(model, vin) {
    if (model && model.length > 0)
        return model
    return guessModel(vin)
}

// The front-page car graphic. Pass the config's `model` field and the VIN;
// ""/"Auto" falls through to a VIN-based guess.
function carImageSource(model, vin) {
    return modelImage(effectiveModel(model, vin))
}

// Best-effort VIN -> model. This is a heuristic, not an authoritative
// decode: over BLE this app has no way to ask the vehicle its model (the
// state categories tesla-control exposes contain no car-type field and
// product-info needs a Fleet API token), so the closest stable signal is
// the VIN's WMI/prefix. The patterns below are the most commonly seen
// modern Tesla VIN prefixes; anything that doesn't match (older or unusual
// builds) returns "" so the Settings override/default still applies. If a
// real VIN mis-guesses here, the manual model picker in Settings is the
// source of truth, not this function.
function guessModel(vin) {
    vin = (vin || "").toUpperCase()
    if (vin.length < 4)
        return ""
    var table = [
        ["LRW3", "model3"],    // Shanghai Model 3 (LRW3E...)
        ["LRWY", "modely"],    // Shanghai Model Y (LRWY...)
        ["5YJ3", "model3"],    // Fremont Model 3 (5YJ3E...)
        ["5Y3",  "model3"],    // alternative Model 3 WMI
        ["5YJY", "modely"],    // Fremont Model Y 2020-21 (5YJYGD...)
        ["5YJX", "modelx"],    // Fremont Model X (5YJXC...)
        ["5YFY", "modely"],    // Fremont Model Y, alternative prefix
        ["5YFS", "models"],    // Fremont Model S (5YFS...)
        ["5YJS", "models"],    // Fremont Model S (5YJSA/R...)
        ["7SAS", "models"],    // Fremont Model S, 2021+ (7SAS...)
        ["7SAX", "modelx"],    // Fremont Model X, 2021+
        ["7SAY", "modely"],    // Fremont/Austin Model Y, 2021+
        ["7G2Z", "cybertruck"],// Austin Cybertruck (7G2Z...)
        ["7G2Y", "modely"],    // Berlin Model Y (7G2Y...)
        ["XP7",  "modely"],    // Fremont Model Y (XP7...)
        ["LRW",  "modely"],    // Shanghai Model 3/Y, unknown 4th char
    ]
    for (var i = 0; i < table.length; i++) {
        if (vin.indexOf(table[i][0]) === 0)
            return table[i][1]
    }
    return ""
}

function emptyStatus() {
    return {
        locked: null,
        doorsOpen: false,
        trunkFrontOpen: false,
        trunkRearOpen: false,
        windowsOpen: false,
        insideTemp: null,
        outsideTemp: null,
        isClimateOn: false,
        driverTempSetting: null,
        batteryLevel: null,
        chargingState: "",
        chargePortOpen: false,
        minutesToFullCharge: 0,
        chargeLimitSoc: null,
        chargeCurrent: null,
        updatedAt: 0
    }
}

function clone(status) {
    var copy = {}
    for (var k in status)
        copy[k] = status[k]
    return copy
}

function closureIsOpen(state) {
    return state === "CLOSURESTATE_OPEN"
        || state === "CLOSURESTATE_AJAR"
        || state === "CLOSURESTATE_OPENING"
}

// Parses `body-controller-state` (VCSEC VehicleStatus). This is the lock
// and door readout that works while infotainment is asleep, so the lock
// toggle can send unlock/lock correctly without a prior `state closures`
// query. protojson enum names are the generated VCSEC constants.
function mergeBodyControllerState(status, jsonText) {
    var s = clone(status)
    try {
        var obj = JSON.parse(jsonText) || {}
        var lock = obj.vehicleLockState || ""
        if (lock === "VEHICLELOCKSTATE_LOCKED" || lock === "VEHICLELOCKSTATE_INTERNAL_LOCKED")
            s.locked = true
        else if (lock === "VEHICLELOCKSTATE_UNLOCKED" || lock === "VEHICLELOCKSTATE_SELECTIVE_UNLOCKED")
            s.locked = false
        var c = obj.closureStatuses || {}
        s.doorsOpen = closureIsOpen(c.frontDriverDoor) || closureIsOpen(c.frontPassengerDoor)
            || closureIsOpen(c.rearDriverDoor) || closureIsOpen(c.rearPassengerDoor)
        s.trunkFrontOpen = closureIsOpen(c.frontTrunk)
        s.trunkRearOpen = closureIsOpen(c.rearTrunk)
        s.updatedAt = Date.now()
    } catch (e) {
        console.log("VehicleState: body-controller-state parse failed:", e, jsonText)
    }
    return s
}

function mergeClosuresState(status, jsonText) {
    var s = clone(status)
    try {
        var obj = JSON.parse(jsonText).closuresState || {}
        s.locked = !!obj.locked
        s.doorsOpen = !!(obj.doorOpenDriverFront || obj.doorOpenDriverRear ||
                          obj.doorOpenPassengerFront || obj.doorOpenPassengerRear ||
                          obj.doorOpenTrunkFront || obj.doorOpenTrunkRear)
        s.trunkFrontOpen = !!obj.doorOpenTrunkFront
        s.trunkRearOpen = !!obj.doorOpenTrunkRear
        s.windowsOpen = !!(obj.windowOpenDriverFront || obj.windowOpenPassengerFront ||
                            obj.windowOpenDriverRear || obj.windowOpenPassengerRear)
        s.updatedAt = Date.now()
    } catch (e) {
        console.log("VehicleState: closures parse failed:", e, jsonText)
    }
    return s
}

function mergeClimateState(status, jsonText) {
    var s = clone(status)
    try {
        var obj = JSON.parse(jsonText).climateState || {}
        s.insideTemp = obj.insideTempCelsius !== undefined ? obj.insideTempCelsius : null
        s.outsideTemp = obj.outsideTempCelsius !== undefined ? obj.outsideTempCelsius : null
        s.driverTempSetting = obj.driverTempSetting !== undefined ? obj.driverTempSetting : null
        s.isClimateOn = !!obj.isClimateOn
        s.updatedAt = Date.now()
    } catch (e) {
        console.log("VehicleState: climate parse failed:", e, jsonText)
    }
    return s
}

// ChargingState is a proto oneof-of-empty-messages (vehicle.proto's
// "oneof type { Void Charging = 5; ... }" pattern used for several
// enum-like fields in this schema), not a plain string enum - protojson
// serializes the active variant as an object keyed by the variant's exact
// field name, e.g. {"Charging": {}}, not the string "Charging" or
// "CHARGING". Confirmed empirically (not guessed) by constructing a
// ChargeState_ChargingState_Charging via the v0.4.1 generated Go code and
// running it through protojson.Format directly. This is why a plain
// `obj.chargingState` read used to be compared against a string in
// FirstPage.qml and could never match - not a casing bug, a type one.
function oneofVariantName(oneofObj) {
    if (!oneofObj)
        return ""
    var keys = Object.keys(oneofObj)
    return keys.length ? keys[0] : ""
}

function mergeChargeState(status, jsonText) {
    var s = clone(status)
    try {
        var obj = JSON.parse(jsonText).chargeState || {}
        s.batteryLevel = obj.batteryLevel !== undefined ? obj.batteryLevel : null
        s.chargingState = oneofVariantName(obj.chargingState)
        s.chargePortOpen = !!obj.chargePortDoorOpen
        s.minutesToFullCharge = obj.minutesToFullCharge || 0
        // chargeLimitSoc is the car's configured charge limit in % (the
        // value charging-set-limit writes); chargeCurrent is the charging
        // current setting in A (what charging-set-amps writes), preferring
        // the "requested" current that persists when unplugged and falling
        // back to the currently-flowing amps while charging.
        s.chargeLimitSoc = obj.chargeLimitSoc !== undefined ? obj.chargeLimitSoc : null
        s.chargeCurrent = obj.chargeCurrentRequest !== undefined ? obj.chargeCurrentRequest
            : (obj.chargingAmps !== undefined ? obj.chargingAmps : null)
        s.updatedAt = Date.now()
    } catch (e) {
        console.log("VehicleState: charge parse failed:", e, jsonText)
    }
    return s
}

// Returns -1 if there's no reading yet, so callers can hide the "updated
// ago" label entirely rather than showing a bogus "NaN minutes ago".
function minutesAgo(timestampMs) {
    if (!timestampMs)
        return -1
    return Math.floor((Date.now() - timestampMs) / 60000)
}
