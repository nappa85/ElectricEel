package vehicle

import (
	"bytes"
	"testing"

	"google.golang.org/protobuf/encoding/protowire"
)

func TestEncodeGetVehicleDataExactBytes(t *testing.T) {
	// Action{2: VehicleAction{1: GetVehicleData{18: {}}}}
	want := []byte{0x12, 0x05, 0x0a, 0x03, 0x92, 0x01, 0x00}
	if got := encodeGetVehicleData(18); !bytes.Equal(got, want) {
		t.Fatalf("getVehicleState: got % x want % x", got, want)
	}
	// Action{2: VehicleAction{1: GetVehicleData{13: {}}}}
	want = []byte{0x12, 0x04, 0x0a, 0x02, 0x6a, 0x00}
	if got := encodeGetVehicleData(13); !bytes.Equal(got, want) {
		t.Fatalf("getVehicleDetailState: got % x want % x", got, want)
	}
}

func message(fields ...[]byte) []byte {
	return bytes.Join(fields, nil)
}

func bytesField(num protowire.Number, value []byte) []byte {
	b := protowire.AppendTag(nil, num, protowire.BytesType)
	return protowire.AppendBytes(b, value)
}

func varintField(num protowire.Number, value uint64) []byte {
	b := protowire.AppendTag(nil, num, protowire.VarintType)
	return protowire.AppendVarint(b, value)
}

func TestVehicleNameFromResponse(t *testing.T) {
	state := message(varintField(43, 12345), bytesField(118, []byte("Eel")))
	response := message(
		bytesField(1, varintField(1, 0)), // actionStatus OK
		bytesField(2, message(bytesField(3, varintField(1, 80)), bytesField(18, state))),
	)
	name, ok := vehicleNameFromResponse(response, vehicleNameSources[0])
	if !ok || name != "Eel" {
		t.Fatalf("VehicleState name: got %q %v", name, ok)
	}
	if _, ok := vehicleNameFromResponse(response, vehicleNameSources[1]); ok {
		t.Fatal("detail state is absent")
	}

	detail := message(bytesField(2, []byte("Detail")))
	response = message(bytesField(2, bytesField(22, detail)))
	name, ok = vehicleNameFromResponse(response, vehicleNameSources[1])
	if !ok || name != "Detail" {
		t.Fatalf("VehicleDetailState name: got %q %v", name, ok)
	}

	if _, ok := vehicleNameFromResponse([]byte{0x12, 0x05, 0x01}, vehicleNameSources[0]); ok {
		t.Fatal("malformed reply must not yield a name")
	}
}
