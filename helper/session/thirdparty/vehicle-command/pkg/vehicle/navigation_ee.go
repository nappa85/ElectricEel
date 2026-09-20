// ElectricEel addition (not upstream): BLE navigation actions the published
// vehicle-command repo omits, built without protoc.
//
// Schemas come from Teslemetry's extended car_server.proto
// (github.com/Teslemetry/tesla-protocol, proto/command/car_server.proto,
// TESLEMETRY-EXT blocks), whose python client exercises these exact actions
// over BLE, and whose Home Assistant integration exposes
// navigation_gps_request ("Sets the vehicle's navigation to a specific
// latitude and longitude"):
//
//	NavigationRequest navigationRequest = 21;
//	message NavigationRequest { string destination = 1; int32 order = 2; }
//
//	NavigationGpsRequest navigationGpsRequest = 53;
//	message NavigationGpsRequest {
//	  enum RemoteNavTripOrder { UNKNOWN=0; REPLACE=1; PREPEND=2; APPEND=3; }
//	  double lat = 1; double lon = 2; RemoteNavTripOrder order = 3;
//	}
//
// The generated carserver package has no such types (upstream never merged
// them — same gap as PR #443's tag-90 waypoints, which lives in
// navigation.go next to this file), and regenerating car_server.pb.go would
// require the pinned protoc 3.21.9/protoc-gen-go v1.28.1 toolchain. These
// two messages are trivially hand-encodable (fixed scalars only), so they
// are built with protowire directly and sent through the same
// infotainment-domain session path as every other action. The byte layout
// is covered by navigation_ee_test.go's exact-sequence assertions — any
// field-number/type drift fails loudly there, not against the car.
package vehicle

import (
	"context"
	"fmt"
	"math"

	"google.golang.org/protobuf/encoding/protowire"
	"google.golang.org/protobuf/proto"

	"github.com/teslamotors/vehicle-command/pkg/protocol"
	carserver "github.com/teslamotors/vehicle-command/pkg/protocol/protobuf/carserver"
	universal "github.com/teslamotors/vehicle-command/pkg/protocol/protobuf/universalmessage"
)

// Remote-nav trip order, mirroring NavigationGpsRequest.RemoteNavTripOrder.
// Teslemetry's BLE client defaults to UNKNOWN (0, field omitted on the
// wire); 1 replaces the trip, 2 prepends a stop, 3 appends a stop.
const (
	navOrderUnknown = 0
	navOrderReplace = 1
	navOrderPrepend = 2
	navOrderAppend  = 3
)

// encodeNavigationGps builds the Action payload bytes for field-53
// NavigationGpsRequest. order 0 omits the field (proto3 default),
// matching Teslemetry's tested BLE behavior.
func encodeNavigationGps(lat, lon float64, order int32) []byte {
	var inner []byte
	inner = protowire.AppendTag(inner, 1, protowire.Fixed64Type)
	inner = protowire.AppendFixed64(inner, math.Float64bits(lat))
	inner = protowire.AppendTag(inner, 2, protowire.Fixed64Type)
	inner = protowire.AppendFixed64(inner, math.Float64bits(lon))
	if order != navOrderUnknown {
		inner = protowire.AppendTag(inner, 3, protowire.VarintType)
		inner = protowire.AppendVarint(inner, uint64(order))
	}
	var vehicleAction []byte
	vehicleAction = protowire.AppendTag(vehicleAction, 53, protowire.BytesType)
	vehicleAction = protowire.AppendBytes(vehicleAction, inner)
	var action []byte
	action = protowire.AppendTag(action, 2, protowire.BytesType)
	action = protowire.AppendBytes(action, vehicleAction)
	return action
}

// encodeNavigationRequest builds the Action payload bytes for field-21
// NavigationRequest. order is always omitted (Teslemetry's BLE client
// never sets it).
func encodeNavigationRequest(destination string) []byte {
	var inner []byte
	inner = protowire.AppendTag(inner, 1, protowire.BytesType)
	inner = protowire.AppendString(inner, destination)
	var vehicleAction []byte
	vehicleAction = protowire.AppendTag(vehicleAction, 21, protowire.BytesType)
	vehicleAction = protowire.AppendBytes(vehicleAction, inner)
	var action []byte
	action = protowire.AppendTag(action, 2, protowire.BytesType)
	action = protowire.AppendBytes(action, vehicleAction)
	return action
}

// executeRawCarServerAction is getCarServerResponse for pre-encoded
// payloads: same infotainment-domain session send, same response check.
func (v *Vehicle) executeRawCarServerAction(ctx context.Context, payload []byte) error {
	responsePayload, err := v.Send(ctx, universal.Domain_DOMAIN_INFOTAINMENT, payload, v.authMethod)
	if err != nil {
		return err
	}
	var response carserver.Response
	if err := proto.Unmarshal(responsePayload, &response); err != nil {
		return &protocol.CommandError{Err: fmt.Errorf("unable to parse vehicle response: %w", err), PossibleSuccess: true, PossibleTemporary: false}
	}
	if response.GetActionStatus().GetResult() == carserver.OperationStatus_E_OPERATIONSTATUS_ERROR {
		description := response.GetActionStatus().GetResultReason().GetPlainText()
		if description == "" {
			description = "unspecified error"
		}
		return &protocol.NominalError{Details: protocol.NewError("car could not execute command: "+description, false, false)}
	}
	return nil
}

// NavigateToGPS starts navigation to exact coordinates over the live
// session (BLE included). order follows navOrder* (0 = unknown/default).
func (v *Vehicle) NavigateToGPS(ctx context.Context, lat, lon float64, order int32) error {
	if lat < -90 || lat > 90 || !isFinite(lat) {
		return fmt.Errorf("invalid latitude %v (want [-90, 90])", lat)
	}
	if lon < -180 || lon > 180 || !isFinite(lon) {
		return fmt.Errorf("invalid longitude %v (want [-180, 180])", lon)
	}
	if order < navOrderUnknown || order > navOrderAppend {
		return fmt.Errorf("invalid order %d (want 0..3)", order)
	}
	return v.executeRawCarServerAction(ctx, encodeNavigationGps(lat, lon, order))
}

// NavigateToDestination sends an address/place string for the car to
// navigate to (field 21, same shape as Teslemetry's BLE
// navigation_request(value)).
func (v *Vehicle) NavigateToDestination(ctx context.Context, destination string) error {
	if len(destination) == 0 || len(destination) > 2000 {
		return fmt.Errorf("destination must be 1..2000 characters")
	}
	return v.executeRawCarServerAction(ctx, encodeNavigationRequest(destination))
}

func isFinite(f float64) bool {
	return !math.IsNaN(f) && !math.IsInf(f, 0)
}
