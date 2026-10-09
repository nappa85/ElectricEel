# Phone-key integration events

ElectricEel broadcasts phone-key events on the user's **session bus** while
the app is running, including when backgrounded with the screen off.

For a user-facing setup guide and an importable Automagic example, see
[Configure Automagic](automagic.md). App launching and other automation are
configured in Automagic.

| Field | Value |
| --- | --- |
| Service | `org.electriceel.harbour-electric-eel` |
| Object path | `/org/electriceel/PhoneKey` |
| Interface | `org.electriceel.PhoneKey1` |
| Signal | `PhoneKeyEvent` |
| Signature | `ssss` |

The arguments, in order, are:

1. `kind`: the event name below.
2. `vin`: the configured vehicle's VIN.
3. `time`: the source event's RFC 3339 timestamp, including timezone.
4. `error`: an empty string on success, otherwise the diagnostic message.

The object exposes signal introspection metadata. The service name is shared
with the navigation Share adaptor; the event interface has no command methods.

## Typed state signal

The same interface additionally publishes **`PhoneKeyStateChanged(bss)`**:

1. `active`: presence operation is running (not proof of an authenticated link).
2. `link`: a stable machine state from the table below.
3. `status`: human-readable diagnostic text. Never parse this text for state.

| Link | Meaning |
| --- | --- |
| `unpaired` | VIN/key pairing prerequisites are not satisfied. |
| `bluetooth-off` | A typed adapter-power failure was reported. |
| `scanning` | Presence is seeking/re-establishing a connection. |
| `connected` | A phone-key connection was established. |
| `authorized` | An authentication response was sent successfully. |
| `stopped` | Presence is inactive or has stopped. |
| `error` | A startup, presence or authentication failure occurred. |

`active` may remain true while `link` is `error` or `bluetooth-off`: presence
can keep retrying after a failure. A connected icon must be based on `link`,
not on `active`. Equivalent vehicle commands use key/VIN prerequisites rather
than requiring the presence icon to be connected; a command can establish its
own link. Unknown future link values should render as disconnected/degraded.

This signal is emitted when the Qt adapter's complete state changes. It has no
history replay, and unchanged snapshots do not produce duplicate state signals.
The existing `PhoneKeyEvent(ssss)` signal, event kinds and diagnostics remain
compatible; the additional signal does not change the `PhoneKey1` interface
version. Legacy integrations can continue listening only for `PhoneKeyEvent`.

Internally Rust publishes `phone_key_state { active, link, status }` and Qt
exposes `phoneKeyActive`, `phoneKeyLink` and display-only `phoneKeyStatus`.
Adapter-off detection originates from typed BlueZ errors (`error_code` in the
Go-to-Rust event); diagnostic wording is not a machine contract.

## Events

| Kind | Meaning |
| --- | --- |
| `presence_near` | An authenticated phone-key session was established. |
| `presence_inside` | The current session has remained up for at least 45 seconds and a recent VCSEC status reports a user present. Emitted once per connection. |
| `presence_auth_ok` | An authentication response was sent successfully. Can occur repeatedly. |
| `presence_auth_failed` | An authentication response failed. |
| `presence_disconnected` | The phone-key session was lost; scanning continues. |
| `presence_far` | Departure was declared by the presence loop. |
| `presence_error` | Scanning or connection encountered an error. |
| `presence_stopped` | Presence mode stopped. |
| `presence_restarted` | The control core restarted presence mode. |

`presence_inside` carries the fork's settled user-presence semantics. It is
an occupancy heuristic based on the vehicle's report, not proof that this
particular phone or its owner is physically inside. It does not indicate gear
selection or that driving has begun. Unknown, absent, failed, or stale status
does not trigger it. Reconnection resets the settling period and permits a
new event.

Consumers can start an integration on `presence_inside` and reset it on
`presence_disconnected` / `presence_far`. These are live notifications without
history replay. Repeated events are forwarded even if the dashboard status
text is unchanged. Watch the service's `NameOwnerChanged` to handle app exit;
a final presence event is not guaranteed during shutdown. Future event kinds
may be added, so consumers should ignore unknown kinds.

## Inspect and listen

Run as the logged-in Sailfish user:

```sh
dbus-send --session --print-reply \
  --dest=org.electriceel.harbour-electric-eel \
  /org/electriceel/PhoneKey org.freedesktop.DBus.Introspectable.Introspect

dbus-monitor --session \
  "type='signal',sender='org.electriceel.harbour-electric-eel',path='/org/electriceel/PhoneKey',interface='org.electriceel.PhoneKey1',member='PhoneKeyEvent'"
```

A sandboxed consumer needs permission to receive signals from this service in
its own Sailjail/D-Bus policy. Publishing uses ElectricEel's existing app-owned
session-bus name and does not require an additional ElectricEel permission.

## Developer check

The Qt test in `app/tests/phonekeyevents.pro` loads the real QML publisher and
checks introspection and signal delivery, including duplicate events and
string argument types. Build with Qt 5 Core/Qml/DBus/Test and the Nemo.DBus
plugin, then run on an isolated bus:

```sh
qmake app/tests/phonekeyevents.pro
make
dbus-run-session -- ./phonekeyevents-test
```
