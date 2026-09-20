# Navigation share

Send a destination to the car navigation over Bluetooth. Entry points:
the pulley menu (Send Destination), Sailfish Share from any native app,
or copy-paste (including from Android apps — see below).

## Car protocol

Signed infotainment-domain actions over the live BLE session.
Field numbers and schemas:

- Field 53 `NavigationGpsRequest`: `double lat = 1`,
  `double lon = 2`, `RemoteNavTripOrder order = 3`
  (`UNKNOWN=0, REPLACE=1, PREPEND=2, APPEND=3`). Carries exact
  coordinates; order 0 (omitted on the wire) replaces the trip.
- Field 21 `NavigationRequest`: `string destination = 1`. Carries an
  address/place string the car resolves itself.
- Field 90 `NavigationWaypointsRequest`: `string waypoints = 1`
  (`refId:<Google Place ID>,...`, last entry is the destination) plus
  `TripPlanOptions`. Available in code (`NavigateToWaypoints`) but not
  wired to the UI: shares carry coordinates and addresses, never Place
  IDs, and resolving them needs the Google Places API (HTTP).

Provenance: field 90 matches upstream PR
`teslamotors/vehicle-command#443` (unmerged); fields 21/53 match
Teslemetry's extended `car_server.proto` (TESLEMETRY-EXT blocks),
exercised over BLE by their client. Details in
`vehicle-command-patch.md`.

Sending an address starts navigation immediately on the car; there is
no "preview on screen" mode for these actions.

## Destination parsing

One parser (`helper/src/share.rs`): shared text (max 2000 chars) becomes
`LatLon` or `Address`.

- `geo:lat,lon[?q=...]` (RFC 5870): coordinates win;
  `geo:0,0?q=...` is an address search.
- Map URLs: Google (`?q=`/`?query=`/`?daddr=`, `/@lat,lon`,
  `/place/.../@lat,lon`, `!3dLAT!4dLON`), Apple Maps (`?q=`, `?ll=`),
  OpenStreetMap (`#map=z/lat/lon`, `?mlat=&mlon=`). Others pass through
  as address text. An embedded map URL beats surrounding text
  (Android shares often arrive as `Name\n\nhttps://...`).
- Bare `lat,lon` / `lat lon` / `lat;lon` (lat −90..90, lon −180..180).
  Out-of-range pairs are rejected, never auto-swapped and never treated
  as addresses.
- Anything else non-empty is address text. Empty/oversize input is
  rejected before any radio use.

`LatLon` sends field 53, `Address` sends field 21. The Navigation page
previews which one will be sent before confirming.

## Sailfish Share reception

`.desktop` declares `X-Share-Methods=destination` with
`Capabilities=text/plain;text/x-url;`, `SupportsMultipleFiles=no`
(Sharing permission is default-granted; a `Share` permission entry
breaks Harbour QA). Reception is twofold: a `ShareProvider` for
well-formed shares, plus a `DBusAdaptor` on `/share/destination`
(iface `org.sailfishos.share`) for Browser/WebView links, which arrive
as `{type, linkTitle, status}` without the `name`/`data` keys
`ShareResource` requires. The app must already be running when the
share arrives; the handler brings it forward.

## Android apps

Third-party Harbour apps cannot appear in Android sharesheets (Jolla's
Android→native bridge is hardcoded for its own apps). Clipboard is
shared between Android and native apps via the Sailfish keyboard, so
the path from Android apps is copy → paste into the Navigation page.

## Test vectors

| input | result |
|---|---|
| `geo:48.8584,2.2945` | LatLon(48.8584, 2.2945) |
| `geo:48.8584,2.2945?q=Eiffel+Tower` | LatLon(48.8584, 2.2945) |
| `geo:0,0?q=1600+Amphitheatre+Parkway` | Address("1600 Amphitheatre Parkway") |
| `48.8584, 2.2945` | LatLon(48.8584, 2.2945) |
| `https://maps.google.com/?q=48.8584,2.2945` | LatLon(48.8584, 2.2945) |
| `https://www.google.com/maps/place/Eiffel+Tower/@48.8584,2.2945,17z` | LatLon(48.8584, 2.2945) |
| `https://maps.apple.com/?q=Eiffel+Tower&ll=48.8584,2.2945` | LatLon(48.8584, 2.2945) |
| `https://www.openstreetmap.org/#map=17/48.8584/2.2945` | LatLon(48.8584, 2.2945) |
| `1600 Amphitheatre Parkway, Mountain View, CA` | Address (same text) |
| `https://maps.google.com/?q=Eiffel+Tower` | Address("Eiffel Tower") |
| empty | error |
| `999,999` | error (out of range) |
| >2000 chars | error |
