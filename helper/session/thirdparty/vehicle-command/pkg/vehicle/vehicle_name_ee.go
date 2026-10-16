// ElectricEel addition (not upstream): read the vehicle's user-given name
// over BLE, built without protoc.
//
// The published car_server.proto reserves the GetVehicleData request tags
// that carry the name (13) or omits them (18). Teslemetry's extended
// protos (github.com/Teslemetry/tesla-protocol, proto/command/
// car_server.proto and vehicle.proto), whose python client reads both
// states over BLE, define:
//
//	message GetVehicleData {
//	  GetVehicleDetailState getVehicleDetailState = 13;
//	  GetVehicleState getVehicleState = 18;
//	}
//	message VehicleData {
//	  VehicleState vehicle_state = 18;              // current firmware
//	  VehicleDetailState vehicle_detail_state = 22;
//	}
//	message VehicleState { oneof optional_vehicle_name { string vehicle_name = 118; } }
//	message VehicleDetailState { oneof optional_vehicle_name { string vehicle_name = 2; } }
//
// Both requests are empty messages, so they are hand-encoded with protowire
// and the reply is walked field by field; the generated carserver types are
// only used for the action-status check. vehicle_name_ee_test.go pins the
// exact request bytes and the reply paths.
package vehicle

import (
	"context"
	"fmt"

	"google.golang.org/protobuf/encoding/protowire"
	"google.golang.org/protobuf/proto"

	"github.com/teslamotors/vehicle-command/pkg/protocol"
	carserver "github.com/teslamotors/vehicle-command/pkg/protocol/protobuf/carserver"
	universal "github.com/teslamotors/vehicle-command/pkg/protocol/protobuf/universalmessage"
)

// A GetVehicleData sub-request and where its reply carries the name.
type vehicleNameSource struct {
	request   protowire.Number // GetVehicleData field
	reply     protowire.Number // VehicleData field
	nameField protowire.Number // string field inside the reply state
}

var vehicleNameSources = []vehicleNameSource{
	{request: 18, reply: 18, nameField: 118}, // VehicleState: current firmware
	{request: 13, reply: 22, nameField: 2},   // VehicleDetailState
}

// encodeGetVehicleData builds Action{vehicleAction{getVehicleData{<field>{}}}}.
func encodeGetVehicleData(field protowire.Number) []byte {
	var getVehicleData []byte
	getVehicleData = protowire.AppendTag(getVehicleData, field, protowire.BytesType)
	getVehicleData = protowire.AppendBytes(getVehicleData, nil)
	var vehicleAction []byte
	vehicleAction = protowire.AppendTag(vehicleAction, 1, protowire.BytesType)
	vehicleAction = protowire.AppendBytes(vehicleAction, getVehicleData)
	var action []byte
	action = protowire.AppendTag(action, 2, protowire.BytesType)
	action = protowire.AppendBytes(action, vehicleAction)
	return action
}

// lastBytesField returns the last length-delimited value of field num
// (protobuf merge semantics), or false when absent or malformed.
func lastBytesField(data []byte, num protowire.Number) ([]byte, bool) {
	var value []byte
	found := false
	for len(data) > 0 {
		n, kind, length := protowire.ConsumeTag(data)
		if length < 0 {
			return nil, false
		}
		data = data[length:]
		if n == num && kind == protowire.BytesType {
			v, length := protowire.ConsumeBytes(data)
			if length < 0 {
				return nil, false
			}
			value, found = v, true
			data = data[length:]
			continue
		}
		length = protowire.ConsumeFieldValue(n, kind, data)
		if length < 0 {
			return nil, false
		}
		data = data[length:]
	}
	return value, found
}

// vehicleNameFromResponse extracts Response.vehicleData(2).<reply>.<name>.
func vehicleNameFromResponse(response []byte, source vehicleNameSource) (string, bool) {
	vehicleData, ok := lastBytesField(response, 2)
	if !ok {
		return "", false
	}
	state, ok := lastBytesField(vehicleData, source.reply)
	if !ok {
		return "", false
	}
	name, ok := lastBytesField(state, source.nameField)
	return string(name), ok
}

// rawCarServerResponse sends a pre-encoded Action over the infotainment
// session and returns the raw Response bytes after the action-status check.
func (v *Vehicle) rawCarServerResponse(ctx context.Context, payload []byte) ([]byte, error) {
	responsePayload, err := v.Send(ctx, universal.Domain_DOMAIN_INFOTAINMENT, payload, v.authMethod)
	if err != nil {
		return nil, err
	}
	var response carserver.Response
	if err := proto.Unmarshal(responsePayload, &response); err != nil {
		return nil, &protocol.CommandError{Err: fmt.Errorf("unable to parse vehicle response: %w", err), PossibleSuccess: true, PossibleTemporary: false}
	}
	if response.GetActionStatus().GetResult() == carserver.OperationStatus_E_OPERATIONSTATUS_ERROR {
		description := response.GetActionStatus().GetResultReason().GetPlainText()
		if description == "" {
			description = "unspecified error"
		}
		return nil, &protocol.NominalError{Details: protocol.NewError("car could not execute command: "+description, false, false)}
	}
	return responsePayload, nil
}

// GetVehicleName returns the name the owner gave the car. It asks for the
// current-firmware VehicleState first and falls back to VehicleDetailState.
func (v *Vehicle) GetVehicleName(ctx context.Context) (string, error) {
	var lastErr error
	for _, source := range vehicleNameSources {
		response, err := v.rawCarServerResponse(ctx, encodeGetVehicleData(source.request))
		if err != nil {
			lastErr = err
			continue
		}
		if name, ok := vehicleNameFromResponse(response, source); ok && name != "" {
			return name, nil
		}
	}
	if lastErr != nil {
		return "", lastErr
	}
	return "", fmt.Errorf("vehicle did not report a name")
}
