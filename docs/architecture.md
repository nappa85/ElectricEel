# Architecture

Single Harbour RPM (`harbour-electric-eel`). No system service, no
capabilities, no `devel-su` install. Sailjail permissions: `Bluetooth;Documents`
(`Documents` covers the `Documents/ElectricEel` phone-key logs).

## Components

- **Application entrypoint** (`helper/src/app.rs`, Rust, `app-entry` feature):
  supplies the executable's `main` from the static library. It calls Qt's
  platform setup to resolve Sailjail paths, constructs and owns the runtime,
  runs the UI with a borrowed runtime handle, then joins Rust's threads,
  stops presence and reaps Go before Qt teardown. There is one application
  process; the UI does not construct, start, poll or free the Rust core.
- **Application runtime** (`helper/src/runtime.rs`): starts phone-key mode
  independently of the UI, processes a bounded queue of typed UI actions on
  its own Rust thread, drains presence events on a one-second deadline and
  drives the core's five-second retry deadlines. It owns phone-key state,
  resume recovery and the 2.5-second post-toggle status-refresh delay. A
  separate Rust lease thread keeps renewing even during blocking BLE work.
- **Silica UI** (`app/qml`, `app/src`): the `TeslaClient` QObject is injected
  into QML by the UI callback. It serializes UI actions to `runtime_submit`
  and copies pushed Rust notifications into queued Qt deliveries, updating
  properties and emitting signals on the GUI thread. Callback detachment is
  a synchronization barrier before UI destruction. There are no Qt worker
  threads or backend timers. QML retains only the dashboard's label-age
  timer, which updates presentation without touching the car. QML pages:
  FirstPage (dashboard + categories), CategoryPage (generic command list),
  ArgumentDialog (per-command form), PairingPage, SettingsPage,
  NavigationPage.
- **Control core** (`helper/src`, Rust staticlib `libelectriceelcore.a`,
  C ABI via cbindgen `electriceelcore.h`): config + key files
  (`config.json`, `private_key.pem`, `public_key.pem` under the app data
  dir), command allow-list, destination parser (`share.rs`), spawning and
  speaking to the session child. One BLE command at a time (`ble_sem`).
- **Session child** (`helper/session`, Go `tesla-session` binary in
  `/usr/share/harbour-electric-eel/bin/`): holds one authenticated BLE
  session across commands, driven over a private Unix-domain socket
  (one request in flight). Commands execute through the vendored
  `tesla-control` handlers (`commands_vendor.go`, pinned
  vehicle-command v0.4.1). Navigation dispatches to `navigate.go`
  (field-53/field-21 actions, see `navigation-share.md`).

## Parent/child protocol

Unix socket at `<state-dir>/tesla-session-<pid>-<n>.sock`, created by
the parent per spawn (`--socket-path`): accept exactly once, then
unlink the path so no other same-UID process can dial in later.
Tagged newline-delimited JSON frames (`{"type":...}`, never
shape-sniffed), versioned by a `hello` handshake (`v: 1` both sides —
mismatch kills the child instead of parsing unknown frames):

- parent → child: `request` (`id`, `cmd`, `args`)
- child → parent: `hello` (first line), `response` (replies),
  `event` (unsolicited presence updates), `heartbeat` (every 10 s,
  including mid-command)

stdin/stdout are not the protocol: stdout is plain logs. Command handlers
receive explicit per-command writers, so replies contain only their output
and background diagnostics cannot race process-wide stdout/stderr swaps. On any
transport failure the child is dropped and the error surfaces — never
a silent fallback, never an unbounded buffer. A failed heartbeat
(dead parent) or closed connection makes the child tear down BLE state
and exit rather than linger. Any frame gap over 30 s (no response,
event, or heartbeat) kills the child as wedged instead of hanging the
caller until the command deadline.

Reader failures also publish `presence_stopped` while idle. The core reaps
the old reader/process before restarting presence; failed restarts are retried
at five-second intervals. Intentional stops cancel retries. Optional command
arguments retain their positional slots: an empty optional value means omitted,
and only trailing empty slots can be removed from the request.

Child processes are never leaked: explicit kill-and-wait on every error
path, plus drop-based reaping (`KillOnDrop` for the process,
`ChildHandle::drop` for the socket path) covering early-returns and
panics. Core orchestration uses plain threads with blocking waits; the MCE
client uses zbus's blocking API and its internal I/O executor.

Configuration changes are persisted before runtime settings/session changes.
Unknown newer schemas are read-only (or rejected if their shape is unsupported).
Explicit unpaired state survives restarts; only pre-phone-key V0 configurations
use the legacy key-file migration. Key generation validates reuse and requires
force to replace an unreadable key. Private/public key replacements are atomic
and synced, with enrollment reset persisted before key rotation.

Pairing requires the persistent session. After transmitting the NFC request,
the child checks enrollment of the requested key for up to 90 seconds; pairing
this phone's key also requires a successful authenticated VCSEC handshake.
Transmission alone is never persisted as successful pairing.
The internal `pair` request also checks that the public key belongs to this
phone's private key before touching BLE, so an interrupted key-file update
cannot pair a stale public half and mark a different private key enrolled.

## Bluetooth transport

Cooperative `org.bluez` D-Bus backend (`helper/session/bluez/`):
discovery, GATT connect, notify/write framing over the system bus. The
radio stays owned by the OS stack, so other Bluetooth users (e.g. a
soundbar) are never disturbed. The legacy raw-HCI (`hci`) backend still
exists for dev-hardware diagnostics only; it needs `CAP_NET_ADMIN` and
is not shipped as default.

## Phone-key presence

After NFC pairing, the core runs a proximity loop (`presence-start`)
while the app is alive: it keeps an authenticated session up while the
vehicle beacon is near (RSSI hysteresis) and answers VCSEC
`AuthenticationRequest` messages, so handle-pull unlock and drive work
without tapping. It never locks/unlocks proactively; walk-away locking
stays the vehicle's own setting.

Disconnected discovery is continuous and event-driven: an independent BlueZ
signal subscription receives new devices and advertisement property updates.
Cached RSSI snapshots seed device identity only; they cannot trigger presence
connections. GATT links each own a fresh signal queue to avoid replaying an
old disconnect on a replacement connection.

While the core's `phone_key_enabled` flag is true, Rust's `CpuKeepAlive` holds an MCE
CPU lease on the system bus (`com.nokia.mce`, `/com/nokia/mce/request`). The
lease thread reads mode intent directly from the core's atomic flag, checking
every 100 ms independently of commands or UI callbacks. It acquires the lease
even while an initial presence start is still blocking. Failed starts and `presence_stopped` retries keep
the lease: connection status is separate from mode intent. A dedicated thread
queries `req_cpu_keepalive_period` and renews `req_cpu_keepalive_start` with the
stable ID `harbour-electric-eel-phone-key`, sleeping for half the granted period
(clamped to 100 ms–15 s). Blocking D-Bus calls have a one-second timeout; failures
retry after one second. Renewal needs neither a GUI timer nor a Qt event loop.
`req_cpu_keepalive_stop` releases the lease when mode is off or on shutdown.
Sailjail's base permission already permits MCE access.

Display blanking remains enabled. This trades increased screen-off power use
for prompt passive entry. Daily phone-key logs record the granted period, first
successful hold, every D-Bus failure, and display status (`0` unknown, `1` off,
`2` dimmed, `3` on), separately from Qt app lifecycle. Periodic wakeups cannot
preserve an authenticated GATT session and are not used.

Phone-key events are also published on the session bus through
`org.electriceel.PhoneKey1`, including the fork's settled `presence_inside`
notification. See [phone-key-events.md](phone-key-events.md) for the signal
contract and integration examples.

## Source layout

```
helper/src/{app,runtime,cpukeepalive,lib,core,ffi,config,commands,share,session_client}.rs
helper/session/{main,commands_vendor,navigate,auth}.go
helper/session/bluez/            org.bluez D-Bus transport
helper/session/thirdparty/vehicle-command/   patched upstream (see vehicle-command-patch.md)
app/src/teslaclient.{h,cpp}      UI-only action/notification adapter
app/qml/{harbour-electric-eel.qml,cover/CoverPage.qml,pages/*.qml,js/*.js}
app/translations/                qsTr catalogs (see translations.md)
app/rpm/harbour-electric-eel.spec
helper/make-app-bundle.sh        stages staticlib + tesla-session into app/
```

`app/thirdparty/` and `app/bin/` are build output (gitignored),
regenerated by `make-app-bundle.sh`. The Rust staticlib targets
`aarch64-unknown-linux-gnu` (glibc, links against Qt); the Go child
builds with `CGO_ENABLED=0 GOOS=linux GOARCH=arm64`.
