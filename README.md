# ElectricEel — Sailfish OS GUI for Tesla

Control your Tesla from Sailfish OS over Bluetooth, with a GUI grouped
like the official Tesla app. No account, no servers, no internet: your
phone talks to the car directly over Bluetooth Low Energy.

![electric-eel](./electric-eel.png)

## Features

- Lock/unlock, climate, charging, trunk/frunk/windows, media, software
  updates, keys and diagnostics — the same groups as the Tesla app.
- Phone key: after pairing, the car unlocks on handle-pull and lets you
  drive, like a Tesla phone key. Nothing unlocks or locks by itself.
- Share destinations to the car navigation: from any Sailfish app via
  Share, or copy-paste (including from Android apps) into Send
  Destination. Coordinates navigate exactly; addresses are resolved by
  the car.
- Interface in 39 languages; untranslated locales fall back to English
  (see `docs/translations.md`).

## Requirements

- Sailfish OS phone with Bluetooth, next to the car for pairing and use.
- A Tesla supporting phone keys and an NFC key card (needed once, to
  approve pairing at the center console).

## Install

Download the latest `harbour-electric-eel-*.aarch64.rpm` from
[GitHub releases](https://github.com/nappa85/ElectricEel/releases),
copy it to the phone and install:

```sh
scp harbour-electric-eel-*.aarch64.rpm defaultuser@<phone-ip>:/tmp/
ssh defaultuser@<phone-ip>
devel-su pkcon install-local /tmp/harbour-electric-eel-*.aarch64.rpm
```

## First use

1. Launch ElectricEel → pull down → **Settings** → enter the VIN → Save.
2. Pull down → **Pair Vehicle** → **Generate Key** → **Pair with
   Vehicle** → tap the NFC card on the center console when the car
   prompts.
3. Start with read-only commands (Diagnostics → Ping) before actuation
   commands (Lock/Unlock, Climate, Trunk).

To navigate somewhere: pull down → **Send Destination**, paste an
address, coordinates or a map link, preview it, and send. The car must
be in Bluetooth range.

## Documentation

Technical details live in `docs/`: `architecture.md` (how the app is
built), `navigation-share.md` (destination sharing), `translations.md`
(localization workflow), `build.md` (building and releasing),
`limitations.md` (current limits), `vehicle-command-patch.md`
(Bluetooth protocol additions).
