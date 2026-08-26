# ElectricEel — Sailfish OS GUI for Tesla

A Silica (Sailfish OS) app that gives a Tesla-app-like GUI over
[teslamotors/vehicle-command](https://github.com/teslamotors/vehicle-command)'s
command protocol, controlling a Tesla vehicle over local Bluetooth. Built and
tested against a Jolla Phone 2026 (aarch64, Sailfish OS 5.2.0.16).

![electric-eel](./electric-eel.png)

## One self-contained package

The app talks to the vehicle through `tesla-session`, a small Go companion
that uses a **cooperative `org.bluez` D-Bus backend** (no raw-HCI adapter
takeover, so other radio users like a soundbar are never disturbed), driven
by an **in-process Rust control core** linked into the app as a staticlib
(cbindgen C ABI, see `BLUEZ_BACKEND_PLAN.md`). Everything ships in a single
Harbour RPM: the Rust core, the Go `tesla-session` child, and the Silica UI.

This removes the old two-package split (sandboxed UI + privileged
`electric-eel-daemon` system service) — no systemd service, no
`setcap`/`CAP_NET_ADMIN` grant, no `devel-su` install. The BLE work that
previously needed raw HCI + the capability now goes through `org.bluez`,
which Sailjail's stock `Bluetooth.permission` already grants the sandboxed
app.

## Layout

```
helper/                          Rust in-process control core (staticlib) + Go child
  src/lib.rs                     crate root: staticlib + rlib (no zbus in the app build)
  src/core.rs                    Core handle: run/generate_key/pair/set_config/get_config
  src/ffi.rs                     cbindgen C ABI (core_new/.../core_free, see
                                  electriceelcore.h)
  src/config.rs                  Config persistence + validation
  src/commands.rs                command catalog
  src/session_client.rs          talks to tesla-session (the Go child)
  electriceelcore.h               generated header (build.rs, cbindgen)
  session/                       Go BLE child (bluez backend)
    main.go                      keeps one session alive across commands
  make-app-bundle.sh             cross-builds the staticlib + tesla-session into app/

app/                              Harbour Silica app (qmake project)
  harbour-electric-eel.pro        links thirdparty/libelectriceelcore.a, installs bin/
  src/teslaclient.{h,cpp}         thin QObject wrapper: worker thread calls the C ABI
  src/harbour-electric-eel.cpp    main()
  qml/harbour-electric-eel.qml
  qml/js/CommandCatalog.js        every command, grouped like the Tesla app
  qml/pages/                      FirstPage, CategoryPage (generic), ArgumentDialog, PairingPage, SettingsPage
  rpm/harbour-electric-eel.spec
  harbour-electric-eel.desktop
  icons/                          launcher icons (86/108/128/172px, from the ElectricEel artwork)
```

## Feasibility findings (Phase 0 spike)

Tested directly against a Jolla Phone 2026 over SSH before building the GUI:

1. `CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build` cross-compiles the Go
   BLE tooling cleanly — the BLE stack is pure Go, no cgo, no Sailfish
   Platform SDK needed for that half.
2. As the normal `defaultuser` account, raw-HCI BLE needs `CAP_NET_ADMIN`
   (`can't down device: operation not permitted` without a `setcap` grant).
3. `/etc/sailjail/permissions/Base.permission` applies `caps.drop all` to
   _every_ sandboxed app with no opt-out, and the stock `Bluetooth.permission`
   only grants D-Bus access to `org.bluez` — so sandboxed raw HCI is a dead
   end. → the cooperative `org.bluez` backend, where the sandboxed app (and
   its spawned child) can do everything through the stock permission.
4. Sailjail doesn't change the process UID (just capabilities/namespaces via
   firejail), which is why the app's child can use the system bus directly.

## Build

### 1. Stage the in-app core (Rust staticlib + Go tesla-session child)

`helper/make-app-bundle.sh` cross-builds the Rust control core as a
`aarch64-unknown-linux-gnu` **staticlib** (glibc — it links against the
app's Qt/Silica; the musl target used for the old standalone daemon would
clash) and the Go `tesla-session` child, staging both into the app's qmake
tree:

```sh
rustup target add aarch64-unknown-linux-gnu   # if not already installed
./helper/make-app-bundle.sh
# -> app/thirdparty/{libelectriceelcore.a,electriceelcore.h}
# -> app/bin/tesla-session
```

The staticlib must be rebuilt whenever `helper/` (the core) changes; nothing
in `app/thirdparty` or `app/bin` is committed to git.

### 2. Build the app RPM with the Sailfish Platform SDK (Docker)

Verified working with the `coderus/sailfishos-platform-sdk-aarch64` image
(~13GB, includes a ready `SailfishOS-5.2.0.15-aarch64` mb2/sb2 build
target - one patch version behind the phone's 5.2.0.16, close enough).
Host-container UID mismatch means bind-mounting the source tree directly
doesn't work cleanly - `docker cp` in, build, `docker cp` out:

```sh
docker pull coderus/sailfishos-platform-sdk-aarch64

docker create --name electric-eel-build coderus/sailfishos-platform-sdk-aarch64 sleep infinity
docker start electric-eel-build

# harbour-electric-eel.aarch64.rpm - compiles the C++/QML app, links the
# staticlib, and installs bin/tesla-session under /usr/share/harbour-electric-eel/bin
docker cp app electric-eel-build:/home/mersdk/app
docker exec -u root electric-eel-build chown -R mersdk:mersdk /home/mersdk/app
docker exec -w /home/mersdk/app electric-eel-build \
  mb2 --target SailfishOS-5.2.0.15-aarch64 build
docker cp electric-eel-build:/home/mersdk/app/RPMS/harbour-electric-eel-0.2.9-1.aarch64.rpm app/RPMS/

docker rm -f electric-eel-build
```

Gotcha already fixed in the committed spec: rpmlint's Sailfish config only
accepts old Fedora short license names (`ASL 2.0`, not `Apache-2.0`) and
requires a `%changelog` section.

### 3. Install on the phone

One package, no helper service to install or configure (devel-su only for
`pkcon` itself):

```sh
scp app/RPMS/harbour-electric-eel-0.2.9-1.aarch64.rpm defaultuser@<phone-ip>:~/
ssh defaultuser@<phone-ip>
devel-su pkcon install-local ~/harbour-electric-eel-0.2.9-1.aarch64.rpm
```

Renamed from `harbour-teslacontrol` - the two package names don't collide
or upgrade each other, so remove the old one first if it's still
installed: `devel-su pkcon remove harbour-teslacontrol`.

## Releasing (GitHub CI)

`.github/workflows/release.yml` builds the app RPM and publishes it. Push a
tag to trigger it:

```sh
git tag v0.2.9
git push origin v0.2.9
```

- The RPM version is taken from the tag (leading `v` stripped); the spec,
  the app's `.pro`, and `helper/Cargo.toml` are all stamped with it at
  build time so `appVersion` and `core_version()` always match.
- The in-app core bundle (Rust staticlib + Go `tesla-session`) is built
  fresh on the runner by `helper/make-app-bundle.sh` — nothing binary is
  committed to git.
- On a tag push the RPM is attached to the GitHub release for that tag; on
  a manual `workflow_dispatch` run it is uploaded as a workflow artifact
  instead (optional `version` input, else it fails).
- The build pulls the ~13GB `coderus/sailfishos-platform-sdk-aarch64` image,
  so the job is slow (~30+ min) and needs the preinstalled Android/.NET/etc.
  removed first to fit on the runner disk (the workflow does this).

## Testing runbook

1. Launch ElectricEel → pull down → **Settings** → enter VIN → Save.
2. Pull down → **Pair Vehicle** → **Generate Key** → **Pair with Vehicle** →
   tap the NFC card on the center console when the car prompts.
   Pairing automatically starts phone-key mode whenever the app is running:
   it scans in the background, holds an authenticated BLE session while the
   vehicle is nearby, and answers passive UNLOCK/DRIVE challenges. It never
   proactively unlocks or locks; handle-pull and the vehicle's Walk-Away
   Door Lock setting remain authoritative.
3. Start with read-only commands (Diagnostics → Ping, Keys → List Enrolled
   Keys) before actuation commands (Lock/Unlock, Climate, Trunk).
4. If the core failed to initialize (`helperAvailable` false), `FirstPage`
   shows a banner; check the app's own log (`journalctl -t
   harbour-electric-eel`) for the `core_new` error, and on-device that the
   bundled `tesla-session` exists and runs (`ls -l
   /usr/share/harbour-electric-eel/bin/tesla-session`).
5. The launcher runs the app with `--single-instance`: backgrounding it
   via the multitasking view and relaunching from the app grid resumes
   the existing process rather than starting fresh, so `Component.onCompleted`
   won't re-run and any newly-deployed QML won't take effect. Kill it
   explicitly when iterating: `devel-su pkill -f harbour-electric-eel`.

## Security notes

There is no privileged helper anymore. The Rust control core runs
**in-process** inside the sandboxed app, and BLE goes over D-Bus to the
stock `org.bluez` (`tesla-session` in "bluez" mode), so the app needs no
setcap, no `CAP_NET_ADMIN`, and nothing beyond the stock
`Bluetooth.permission` Sailjail grants - precisely why the raw-HCI route
(Phase 0 findings 2/3) was abandoned.

The only cross-process surface is the `tesla-session` child, spawned and
fed commands exclusively by the in-process core over a private
stdin/stdout pipe (newline-delimited JSON, one request in flight at a
time) - there is no ambient D-Bus service to call, so no unauthorized
process can invoke control commands. BLE to the car still uses the
session-token auth that `tesla-control` itself uses, with the private key
stored under the app's own data dir.

The legacy raw-HCI (`hci`) transport still exists in `tesla-session` for
dev-hardware diagnostics (it requires `CAP_NET_ADMIN`), but it is off by
default and not what the app is packaged to use.

## Known gaps / next steps

- The RPM builds, installs, and has been exercised against a real vehicle:
  BLE commands (lock/unlock/climate/trunk/...) work over the `org.bluez`
  backend. Driving is authorized by the enrolled key: with the pairing form
  factor now set to a phone key (`android_device`, see `helper/src/core.rs`),
  the connected session is treated as a drive-authorizing phone key. If a
  re-enrolled key still requires a physical NFC-card tap to drive, re-pair
  and confirm the vehicle's key list shows the key as a phone device, not a
  cloud key.
- Automatic phone-key protocol handling is unit-tested but still needs a
  real-car pass after installation: background the app, verify handle-pull
  unlock, drive without an NFC tap, walk away with the vehicle setting both
  enabled and disabled, then return after BLE signal loss and verify it
  reconnects. The pairing page shows live phone-key state and errors.
- QML files aren't compiled by `mb2`, only reviewed, so runtime QML errors
  are still possible even though the C++/Qt build is clean.
- `CommandCatalog.js` argument bounds/enum values (STATE on/off, ROLE,
  FORM_FACTOR, `state` CATEGORY names, etc.) are best-effort from public
  docs, not verified against `tesla-control help <cmd>` on this exact
  binary version - check that if a command rejects otherwise-sane input.
- Fleet API / internet mode (`-proxy`, `get`/`post` Fleet API passthrough)
  is intentionally out of scope for v1 - BLE only.
- Built RPMs and rpmbuild staging output are no longer tracked in git (see
  `.gitignore`) - they're build output, not source, and get regenerated by
  the steps above.
- See [KNOWN_ISSUES.md](KNOWN_ISSUES.md) for anything else open.
