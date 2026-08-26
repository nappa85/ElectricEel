// Data-driven catalog of tesla-control's subcommands, grouped the way the
// official Tesla app groups its own controls. One generic CategoryPage.qml
// renders whichever category is passed to it from this list, instead of
// hand-writing ~10 nearly-identical pages.
//
// Argument bounds/enum values here are best-effort from the public
// vehicle-command docs and are meant to make entering *valid-looking*
// values easy - tesla-control itself is still the final authority and will
// reject anything it doesn't like. If `tesla-control help <cmd>` on-device
// disagrees with something here, trust the device and fix this file.
//
// arg.type: "none" | "int" | "float" | "string" | "pin" | "enum"
//
// arg.stateDefault (optional): name of a VehicleState.js status field whose
// current value should pre-fill this arg's `def` when the dialog opens (the
// slider/field starts at what the vehicle reports, not a hardcoded default).
// CategoryPage.qml clones the arg and applies it - the catalog's own `def`
// stays untouched as a fallback for when status isn't loaded yet.
//
// Toggle pairs (start/stop charging, open/close charge port, climate on/off,
// lock/unlock, vent/close windows, open/close trunk) carry a `visibleWhen`
// predicate that CategoryPage evaluates against the dashboard status
// snapshot: only the action matching the current vehicle state is listed,
// mirroring the front-page quick actions rule that the UI always reflects
// reality rather than the state a tap would produce. Commands without a
// predicate are always listed.

.pragma library

var CATEGORIES = [
  {
    id: "attention",
    title: "Attention",
    icon: "../../img/icons/power.svg",
    commands: [
      cmd("honk", "Honk Horn"),
      cmd("flash-lights", "Flash Lights"),
      cmd("wake", "Wake Vehicle"),
    ]
  },
  {
    id: "climate",
    title: "Climate",
    icon: "../../img/icons/fan1.svg",
    commands: [
      cmd("climate-on", "Climate On", [], climateOff),
      cmd("climate-off", "Climate Off", [], climateOn),
      // sendSuffix "C": tesla-control's climate-set-temp expects the unit
      // glued directly onto the number (22C or 72F) - a bare number always
      // failed to parse. See ArgumentDialog.qml's sliderField.
      cmd("climate-set-temp", "Set Temperature", [
        arg("TEMP", "float", { min: 15, max: 28, step: 0.5, unit: "°C", sendSuffix: "C", def: 21, stateDefault: "driverTempSetting" })
      ]),
      // SEAT/LEVEL values are commands_vendor.go's own seats/levels map
      // keys verbatim - hyphenated positions, off|low|medium|high levels
      // (not the Fleet-API-flavored underscore names or a numeric level
      // this previously guessed).
      cmd("seat-heater", "Seat Heater", [
        arg("SEAT", "enum", { values: ["front-left","front-right","2nd-row-left","2nd-row-center","2nd-row-right","3rd-row-left","3rd-row-right"] }),
        arg("LEVEL", "enum", { values: ["off","low","medium","high"] })
      ]),
      cmd("steering-wheel-heater", "Steering Wheel Heater", [
        arg("STATE", "enum", { values: ["on","off"] })
      ]),
      // POSITIONS is 'L', 'R', or 'LR' verbatim (commands_vendor.go checks
      // strings.Contains for each letter) - not a comma-separated seat list.
      cmd("auto-seat-and-climate", "Auto Seat & Climate", [
        arg("POSITIONS", "enum", { values: ["L","R","LR"] }),
        arg("STATE", "enum", { values: ["on","off"], optional: true })
      ]),
      cmd("precondition-schedule-add", "Add Preconditioning Schedule", preconditionScheduleAddArgs()),
      cmd("precondition-schedule-remove", "Remove Preconditioning Schedule", scheduleRemoveArgs()),
    ]
  },
  {
    id: "charging",
    title: "Charging",
    icon: "../../img/icons/bolt.svg",
    commands: [
      cmd("charging-start", "Start Charging", [], notCharging),
      cmd("charging-stop", "Stop Charging", [], charging),
      cmd("charge-port-open", "Open Charge Port", [], portClosed),
      cmd("charge-port-close", "Close Charge Port", [], portOpen),
      cmd("charging-set-limit", "Set Charge Limit", [
        arg("PERCENT", "int", { min: 50, max: 100, unit: "%", def: 80, stateDefault: "chargeLimitSoc" })
      ]),
      cmd("charging-set-amps", "Set Charge Current", [
        arg("AMPS", "int", { min: 1, max: 48, unit: "A", def: 16, stateDefault: "chargeCurrent" })
      ]),
      cmd("charging-schedule", "Schedule Charging", [
        arg("MINS", "int", { min: 0, max: 1439, unit: "min after midnight", def: 0 })
      ]),
      cmd("charging-schedule-cancel", "Cancel Scheduled Charging"),
      cmd("charging-schedule-add", "Add Charge Schedule", chargeScheduleAddArgs()),
      cmd("charging-schedule-remove", "Remove Charge Schedule", scheduleRemoveArgs()),
    ]
  },
  {
    id: "security",
    title: "Locks & Security",
    icon: "../../img/icons/security.svg",
    commands: [
      cmd("lock", "Lock", [], unlockedState),
      cmd("unlock", "Unlock", [], lockedState),
      // drive = remote start / keyless drive (RKE_ACTION_REMOTE_DRIVE): lets
      // a keyless driver drive for a short window after unlock. Distinct from
      // phone-key driving, which needs no command at all - the car authorizes
      // it from an authenticated VCSEC session with a valid key. Kept in
      // Locks & Security as it's a drive-authorization action.
      cmd("drive", "Remote Start (Keyless Drive)", []),
      cmd("sentry-mode", "Sentry Mode", [ arg("STATE", "enum", { values: ["on","off"] }) ]),
      cmd("valet-mode-on", "Valet Mode On", [ arg("PIN", "pin", {}) ]),
      cmd("valet-mode-off", "Valet Mode Off"),
      cmd("guest-mode-on", "Guest Mode On"),
      cmd("guest-mode-off", "Guest Mode Off"),
      cmd("erase-guest-data", "Erase Guest Data"),
      cmd("autosecure-modelx", "Auto-Secure (Model X)"),
      // parental-controls-* commands used to be listed here, but they don't
      // exist in cmd/tesla-control/commands.go at the pinned v0.4.1 tag -
      // "parental-controls" is only a `state` CATEGORY name (read-only
      // status query), not a family of action commands. Every one of these
      // would fail with "unrecognized command" before ever reaching BLE.
    ]
  },
  {
    id: "body",
    title: "Trunk, Frunk & Windows",
    icon: "../../img/icons/window.svg",
    commands: [
      cmd("trunk-open", "Open Rear Trunk", [], trunkClosed),
      cmd("trunk-move", "Move Rear Trunk"),
      cmd("trunk-close", "Close Rear Trunk", [], trunkOpen),
      cmd("frunk-open", "Open Front Trunk"),
      cmd("tonneau-open", "Open Tonneau (Cybertruck)"),
      cmd("tonneau-close", "Close Tonneau (Cybertruck)"),
      cmd("tonneau-stop", "Stop Tonneau (Cybertruck)"),
      cmd("windows-vent", "Vent Windows", [], windowsClosed),
      cmd("windows-close", "Close Windows", [], windowsOpen),
    ]
  },
  {
    id: "media",
    title: "Media",
    icon: "../../img/icons/navigation.svg",
    commands: [
      cmd("media-toggle-playback", "Play / Pause"),
      cmd("media-next-track", "Next Track"),
      cmd("media-previous-track", "Previous Track"),
      cmd("media-next-favorite", "Next Favorite"),
      cmd("media-previous-favorite", "Previous Favorite"),
      cmd("media-volume-up", "Volume Up"),
      cmd("media-volume-down", "Volume Down"),
      cmd("media-set-volume", "Set Volume", [
        arg("VOLUME", "float", { min: 0, max: 10, step: 0.5, def: 5 })
      ]),
    ]
  },
  {
    id: "software",
    title: "Software",
    icon: "../../img/icons/upgrades.svg",
    commands: [
      // sendSuffix "s": upstream parses DELAY with Go's time.ParseDuration,
      // which requires a unit (10m, 2h, ...) - a bare integer only ever
      // worked by accident at DELAY=0, the one value ParseDuration accepts
      // unitless.
      cmd("software-update-start", "Start Software Update", [
        arg("DELAY", "int", { min: 0, max: 3600, unit: "s", sendSuffix: "s", def: 0 })
      ]),
      cmd("software-update-cancel", "Cancel Software Update"),
    ]
  },
  {
    id: "keys",
    title: "Keys",
    icon: "../../img/icons/lock.svg",
    commands: [
      cmd("list-keys", "List Enrolled Keys"),
      // FORM_FACTOR is vcsec.KeyFormFactor's own value names (minus the
      // KEY_FORM_FACTOR_ prefix, case-insensitive) - "phone_key" isn't one
      // of them. This app's own pairing (helper/src/core.rs) enrolls as
      // android_device, a phone form factor the vehicle treats as a real
      // drive-authorizing key (cloud_key would remote-control but not
      // authorize driving).
      cmd("add-key", "Add Key", [
        arg("PUBLIC_KEY", "string", { placeholder: "path to public_key.pem" }),
        arg("ROLE", "enum", { values: ["owner","driver"] }),
        arg("FORM_FACTOR", "enum", { values: ["nfc_card","ios_device","android_device","cloud_key"] }),
      ]),
      cmd("remove-key", "Remove Key", [
        arg("PUBLIC_KEY", "string", { placeholder: "path to public_key.pem" }),
      ]),
      // rename-key omitted: requiresFleetAPI is true
      // (commands_vendor.go:438) and this app is BLE-only, so it always
      // fails with "command requires a FleetAPI OAuth token" - no point
      // offering a button guaranteed to error.
      cmd("session-info", "Session Info", [
        arg("PUBLIC_KEY", "string", { placeholder: "path to public_key.pem" }),
        arg("DOMAIN", "enum", { values: ["vcsec","infotainment"] }),
      ]),
    ]
  },
  {
    id: "diagnostics",
    title: "Diagnostics",
    icon: "../../img/icons/service.svg",
    commands: [
      cmd("ping", "Ping Vehicle"),
      // CATEGORY values are cmd/tesla-control's own categoriesByName keys
      // (pkg/vehicle/state.go's StateCategory constants), not the Fleet
      // API's vehicle_data JSON section names - short/hyphenated, not
      // "_state"-suffixed. Verified against the v0.4.1 tag this project
      // pins (see KNOWN_ISSUES.md); a previous guess here used the wrong
      // (Fleet API) names and made every "state" call fail before it ever
      // reached BLE.
      cmd("state", "Get Vehicle State", [
        arg("CATEGORY", "enum", { values: ["charge","climate","drive","location","closures","charge-schedule","precondition-schedule","tire-pressure","media","media-detail","software-update","parental-controls"] })
      ]),
      cmd("body-controller-state", "Body Controller State"),
      // product-info omitted: requiresFleetAPI is true
      // (commands_vendor.go:963), always fails the same way rename-key
      // does in this BLE-only app.
      cmd("keep-accessory-power", "Keep Accessory Power", [ arg("STATE", "enum", { values: ["on","off"] }) ]),
      cmd("low-power-mode", "Low Power Mode", [ arg("STATE", "enum", { values: ["on","off"] }) ]),
    ]
  },
]

function cmd(id, label, args, visibleWhen) {
  return { id: id, label: label, args: args || [], visibleWhen: visibleWhen }
}

function arg(name, type, opts) {
  opts = opts || {}
  opts.name = name
  opts.type = type
  return opts
}

// visibleWhen predicates. Each receives the status snapshot object that
// CategoryPage was opened with and returns whether its command row should be
// listed. Unknown states (null/"") deliberately fall to the "not active"
// reading so the forward action (climate on, start charging, ...) stays
// reachable instead of leaving a category empty. See VehicleState.js for the
// exact field names (protojson camelCase of vehicle.proto's fields, and
// chargingState is a oneof VARIANT name - e.g. "Charging" - not a plain string).
function climateOn(s)  { return !!s.isClimateOn }
function climateOff(s) { return !s.isClimateOn }
function nearlyCharging(s) { return s.chargingState === "Charging" || s.chargingState === "Starting" || s.chargingState === "Calibrating" }
function charging(s)   { return nearlyCharging(s) }
function notCharging(s){ return !nearlyCharging(s) }
function portOpen(s)   { return !!s.chargePortOpen }
function portClosed(s) { return !s.chargePortOpen }
function lockedState(s)  { return s.locked !== false }
function unlockedState(s){ return s.locked === false }
function windowsOpen(s)  { return !!s.windowsOpen }
function windowsClosed(s){ return !s.windowsOpen }
function trunkOpen(s)  { return !!s.trunkRearOpen }
function trunkClosed(s){ return !s.trunkRearOpen }

// DAYS is GetDays()'s own dayNamesBitMask keys (case-insensitive): Sun,
// Mon, Tues, Wed, Thurs, Fri, Sat, or all/weekdays - note "Tues"/"Thurs",
// not "Tue"/"Thu".
// REPEAT/ID/ENABLED are genuinely optional upstream (a schedule repeats
// weekly and gets an auto-generated ID unless told otherwise) - marked
// optional here relies on ArgumentDialog.qml's "not set" choice actually
// omitting the arg, not just defaulting to the first enum value / 0.
// Note: unlike precondition-schedule-add, charging-schedule-add does NOT
// honor an ID argument - commands_vendor.go's handler stamps the schedule's
// Id from time.Now().Unix() and never reads args["ID"] (upstream v0.4.1
// behavior, commands_vendor.go "charging-schedule-add"). So no ID field is
// offered here; preconditionScheduleAddArgs() keeps one because its handler
// does read it. Use charging-schedule-remove with TYPE "id" to target an
// existing schedule instead.
function chargeScheduleAddArgs() {
  return [
    arg("DAYS", "string", { placeholder: "Mon,Tues,Wed" }),
    arg("TIME", "string", { placeholder: "22:00-06:00 (start or end may be blank)" }),
    arg("LATITUDE", "float", { min: -90, max: 90, step: 0.000001, def: 0 }),
    arg("LONGITUDE", "float", { min: -180, max: 180, step: 0.000001, def: 0 }),
    arg("REPEAT", "enum", { values: ["once"], optional: true }),
    arg("ENABLED", "enum", { values: ["true","false"], optional: true }),
  ]
}

// Same shape as chargeScheduleAddArgs() except TIME: preconditioning has a
// single trigger time, not a start-end range.
function preconditionScheduleAddArgs() {
  return [
    arg("DAYS", "string", { placeholder: "Mon,Tues,Wed" }),
    arg("TIME", "string", { placeholder: "22:00" }),
    arg("LATITUDE", "float", { min: -90, max: 90, step: 0.000001, def: 0 }),
    arg("LONGITUDE", "float", { min: -180, max: 180, step: 0.000001, def: 0 }),
    arg("REPEAT", "enum", { values: ["once"], optional: true }),
    arg("ID", "int", { placeholder: "leave blank for a new schedule", optional: true }),
    arg("ENABLED", "enum", { values: ["true","false"], optional: true }),
  ]
}

function scheduleRemoveArgs() {
  return [
    // home|work|other remove by category; "id" removes one schedule by
    // its numeric ID (see list-keys-style ID from the *-add commands'
    // output). "favorite" was never a real TYPE value.
    arg("TYPE", "enum", { values: ["home","work","other","id"] }),
    arg("ID", "int", { placeholder: "required when TYPE is id", optional: true }),
  ]
}

function findCategory(categoryId) {
  for (var i = 0; i < CATEGORIES.length; i++) {
    if (CATEGORIES[i].id === categoryId)
      return CATEGORIES[i]
  }
  return null
}
