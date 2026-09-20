# vehicle-command patch

Applies to `helper/session/thirdparty/vehicle-command`.

Local copy of `github.com/teslamotors/vehicle-command` at the **v0.4.1**
tag (same pin as `helper/session/go.mod` before the `replace`
directive), with upstream **PR #443** ("Add support for
`navigation_waypoints_request`", by matthieu6010, branch
`add-navigation-waypoints-request` @ `281b3a7`) applied.

Files taken from that PR, verbatim:

- `pkg/protocol/protobuf/car_server.proto` — `NavigationWaypointsRequest`
  message + `navigationWaypointsRequest = 90` oneof wiring (hand-applied,
  same 10-line hunk).
- `pkg/protocol/protobuf/carserver/car_server.pb.go` — the PR's
  regenerated file, verbatim (protoc 3.21.9 / protoc-gen-go v1.28.1,
  same versions as the v0.4.1 header it replaced).
- `pkg/vehicle/navigation.go` — `NavigateToWaypoints` /
  `NavigateToWaypointsWithOptions`, verbatim.

Deliberately NOT included:

- the PR's `pkg/proxy/command.go` hunk — the proxy is the Fleet/internet
  path; this app is BLE-only.

ElectricEel additions on top (same "minimum copy" principle):

- `pkg/vehicle/navigation_ee.go` + `navigation_ee_test.go` — BLE
  navigation actions the published repo omits AND PR #443 doesn't cover:
  field-21 `NavigationRequest` (address text) and field-53
  `NavigationGpsRequest` (coordinates), schemas from Teslemetry's
  extended `car_server.proto` (TESLEMETRY-EXT blocks, exercised over
  BLE by their client). Hand-encoded with `protowire` (no protoc
  needed); byte-exact tests included. Field-106 and other Teslemetry
  extensions are NOT copied — nothing in this app needs them.

Everything else in this directory is byte-identical to v0.4.1
(`git clone https://github.com/teslamotors/vehicle-command.git &&
git checkout v0.4.1`, minus `.git`). If upstream merges #443 (or
releases a tag containing it), delete this directory and drop the
`replace` directive from `helper/session/go.mod`, pinning the new tag
instead.
