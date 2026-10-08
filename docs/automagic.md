# Configure Automagic for ElectricEel

ElectricEel publishes phone-key events. **Automagic owns the automation:**
it listens for an event and runs the flow you choose, such as launching an
app. Configure the trigger and action in Automagic.

This guide is for [harbour-automagic](https://github.com/sailfishos-chum/harbour-automagic)
on Sailfish OS, with its required `automagic-daemon` package. Open Automagic
once and confirm it connects to the daemon. Keep ElectricEel running with
phone-key mode enabled; background/cover and screen-off are supported.

## Quick setup: import the logging example

The [automagic-example](automagic-example/) folder contains an importable
example with two D-Bus triggers, a log action, and a flow. It logs entering
and leaving events so you can confirm the connection before adding your app.

1. Copy **the whole folder and its four JSON files** into a folder under your
   phone's Documents directory, for example `Documents/ElectricEel-Automagic`.
2. In Automagic's **States** tab, open the pull-down menu → **Import**.
   Select that folder and tap **Import**.
3. Return to **States** → pull-down → **Reload Daemon**.
4. Check **Sources** for **ElectricEel inside car** and **ElectricEel away**,
   and **Flows** for **ElectricEel event log**. They should be enabled.
5. To check the flow without a car visit, use its context menu → **Execute
   Flow**. This checks the log action only; event variables will be absent.
6. On a real visit, allow the phone-key connection to stay up for at least
   45 seconds while the car reports a user present. The Log view should show
   `ElectricEel presence_inside` with the VIN and event timestamp. Departure
   produces `presence_far` once ElectricEel declares the vehicle away.

The folder is an Automagic import, not a replacement for its existing config
directory. Automagic handles importing and any conflicting example IDs.

## Manual setup: create the inside-car trigger

In **Sources** → pull-down → **Add Source**, set:

| Automagic field | Value |
| --- | --- |
| Name | `ElectricEel inside car` |
| Enabled | On |
| Act as Trigger | On |
| Protocol | `DBUS` |
| Bus Address | `session` |
| Destination | `org.electriceel.harbour-electric-eel` |
| Object Path | `/org/electriceel/PhoneKey` |
| Interface | `org.electriceel.PhoneKey1` |
| Signal | `PhoneKeyEvent` |
| Method | Leave empty |
| D-Bus arguments | Leave empty; this source listens to a signal |

Under **Filters**, tap **Add Filter**:

| Key | Value |
| --- | --- |
| `arg0` | `presence_inside` |

Use the value literally, without quotes or a regular expression. This filter
matters: `PhoneKeyEvent` also carries connection, authorization, and error
notifications, which should not all launch your app. To restrict a flow to
one car, add another filter with Key `arg1` and Value your VIN.

Under **Transformations**, add these four transformations, each of Type
`copy`:

| Input Variable | Output Variable |
| --- | --- |
| `arg0` | `kind` |
| `arg1` | `vin` |
| `arg2` | `event_time` |
| `arg3` | `error` |

Automagic's D-Bus trigger filters the original `arg0`–`arg3` values, then
passes the transformation outputs to the flow. These copies make the event
data available to conditions and action templates such as `{{vin}}`.
Save the source.

For an away trigger, copy this source, name it **ElectricEel away**, and
change only the `arg0` filter to `presence_far`.

## Have Automagic launch your chosen app

1. In **Actions** → pull-down → **Add Action**, create an enabled action
   named **Launch my car app** with **Action Type = SHELL**.
2. Set **Command** to the chosen app's normal launcher command. The app's
   installed `.desktop` entry contains it in the `Exec=` line:
   - Native apps: `/usr/share/applications/*.desktop`.
   - Android apps: `~/.local/share/applications/apkd_launcher_*.desktop`.
   Copy the command after `Exec=`. If it contains desktop field codes such
   as `%u`, `%U`, `%f`, or `%F`, remove those unused file/URL placeholders for
   a launch without an argument. Other field codes need their corresponding
   desktop-entry values; do not pass them literally to the shell.
3. Leave **Run As User** empty to use Automagic's detected session user.
   App launching should run in that user's session. Save the action.
4. In **Flows** → pull-down → **Add Flow**, name the flow **Launch car app**.
   Enable it, tap **Add Trigger**, and select **ElectricEel inside car**.
5. Pull down → **Add Action**, then select **Launch my car app** for the
   action step. Save the flow.
6. In **States**, use **Reload Daemon**. Use the flow's **Execute Flow**
   context-menu action to verify the launch command before testing at the car.

The `presence_inside` notification is emitted once per Bluetooth connection,
not once per journey. Reconnecting can produce another notification after a
new 45-second settling period. If you want one launch per journey, use
Automagic's internal state and step conditions to remember that the app was
launched, then clear that state from a `presence_far` flow. A Throttle step
can also suppress rapid repeats.

## Timing and troubleshooting

- **No event yet:** the 45-second settling period and a recent vehicle status
  reporting a user present are both required. This is an occupancy heuristic,
  not a gear-selection or driving-start signal. See
  [phone-key-events.md](phone-key-events.md) for the full semantics.
- **No flow run:** verify the source and flow are enabled, **Act as Trigger**
  is on, and you used **Reload Daemon** after saving/importing. Automagic must
  be connected to its daemon and the daemon must use your logged-in user's
  session bus.
- **Event data is `null` in a log:** check the four `copy` transformations.
  Manual **Execute Flow** does not supply signal arguments.
- **Flow runs, app does not open:** check the Shell action's command and user
  with **Execute Flow**, and inspect Automagic's logs. This distinguishes a
  launch-command problem from a D-Bus trigger problem.
- **Check ElectricEel independently:** use the `dbus-monitor` command in
  [phone-key-events.md](phone-key-events.md). Match the session bus and the
  `PhoneKeyEvent` signal, not a system-bus method.
- **Missed events:** this is a live feed with no replay. Automagic's daemon
  must be listening before the event occurs.

The `presence_far` event itself is immediate. The hotspot flow installed
from Settings waits 3 minutes and turns the hotspot off only if
`presence_inside` has not arrived. Another `presence_far` during that wait
does not restart it. Other automations that need their own delay should
implement that timing in Automagic.

## Reference versions

The setup labels and JSON format were checked against harbour-automagic
[`95e80e7`](https://github.com/sailfishos-chum/harbour-automagic/tree/95e80e72014ae6c5070b27208c3b897cff9c4e3d)
and automagic-daemon
[`b210f8c`](https://github.com/sailfishos-chum/automagic-daemon/tree/b210f8cc42ddef0e10f5d587e2b796d4a00533e5).

Settings → **Add Automagic flows** writes triggers for this build's
`org.electriceel.PhoneKey1.PhoneKeyEvent` signal (`presence_inside`,
`presence_far`, `presence_near`, `presence_auth_ok`, and an unfiltered
presence source) plus ConnMan hotspot flows: on for `presence_inside`, and
off 3 minutes after `presence_far` unless `presence_inside` arrives first.
The importable example above is a separate logging flow and is left in place.
