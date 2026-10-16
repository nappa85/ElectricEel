.pragma library

// Two-tap confirmation for the cover's quick actions, where no dialog can be
// shown and a stray touch fires an action: the first tap arms an action,
// a second tap on the same action within ARM_MS runs it. A tap on another
// action re-arms instead.
var ARM_MS = 4000

// Returns the new state after a tap: { run: <action to run or "">,
// armed: <action now armed or ""> }.
function tap(armed, action) {
    if (armed.length > 0 && armed === action)
        return { run: action, armed: "" }
    return { run: "", armed: action }
}
