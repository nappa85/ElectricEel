// Tests for navigation_ee.go (ElectricEel BLE navigation actions).
//
// The expected byte strings below were computed INDEPENDENTLY with Python's
// struct module (see the commit message / spike doc for the generator), not
// with protowire itself — so they pin the exact field numbers (21/53),
// wire types, and float encoding the car expects. A schema drift fails here,
// not against the vehicle.
package vehicle

import (
	"context"
	"encoding/hex"
	"math"
	"testing"

	"google.golang.org/protobuf/encoding/protowire"
)

func mustHex(t *testing.T, s string) []byte {
	t.Helper()
	b, err := hex.DecodeString(s)
	if err != nil {
		t.Fatalf("bad hex fixture: %v", err)
	}
	return b
}

func TestEncodeNavigationGpsExactBytes(t *testing.T) {
	got := encodeNavigationGps(48.8584, 2.2945, navOrderUnknown)
	want := mustHex(t, "1215aa03120976711b0de06d4840114260e5d0225b0240")
	if string(got) != string(want) {
		t.Fatalf("order-0 encoding mismatch:\n got %x\nwant %x", got, want)
	}

	got = encodeNavigationGps(48.8584, 2.2945, navOrderReplace)
	want = mustHex(t, "1217aa03140976711b0de06d4840114260e5d0225b02401801")
	if string(got) != string(want) {
		t.Fatalf("order-1 encoding mismatch:\n got %x\nwant %x", got, want)
	}
}

func TestEncodeNavigationRequestExactBytes(t *testing.T) {
	got := encodeNavigationRequest("Eiffel Tower")
	want := mustHex(t, "1211aa010e0a0c45696666656c20546f776572")
	if string(got) != string(want) {
		t.Fatalf("address encoding mismatch:\n got %x\nwant %x", got, want)
	}
}

// Structural decode: outer field 2 -> VehicleAction field 53 -> fixed64
// lat/lon fields 1/2 and NO order field when 0. Guards against a future
// edit silently renumbering a field while keeping the encoder compiling.
func TestEncodeNavigationGpsStructure(t *testing.T) {
	payload := encodeNavigationGps(-33.8688, 151.2093, navOrderUnknown)
	num, typ, n := protowire.ConsumeTag(payload)
	if n < 0 || num != 2 || typ != protowire.BytesType {
		t.Fatalf("outer tag = (%d,%d), want (2,bytes)", num, typ)
	}
	va, _ := protowire.ConsumeBytes(payload[n:])
	num, typ, n = protowire.ConsumeTag(va)
	if n < 0 || num != 53 || typ != protowire.BytesType {
		t.Fatalf("action tag = (%d,%d), want (53,bytes)", num, typ)
	}
	inner, _ := protowire.ConsumeBytes(va[n:])
	seen := map[protowire.Number]bool{}
	for len(inner) > 0 {
		num, typ, n = protowire.ConsumeTag(inner)
		if n < 0 {
			t.Fatalf("bad inner tag: %v", protowire.ParseError(n))
		}
		inner = inner[n:]
		seen[num] = true
		switch num {
		case 1, 2:
			if typ != protowire.Fixed64Type {
				t.Fatalf("field %d type = %d, want fixed64", num, typ)
			}
			v, n := protowire.ConsumeFixed64(inner)
			if n < 0 {
				t.Fatalf("bad fixed64: %v", protowire.ParseError(n))
			}
			inner = inner[n:]
			want := -33.8688
			if num == 2 {
				want = 151.2093
			}
			if math.Float64frombits(v) != want {
				t.Fatalf("field %d = %v, want %v", num, math.Float64frombits(v), want)
			}
		default:
			t.Fatalf("unexpected inner field %d (order must be omitted when 0)", num)
		}
	}
	if !seen[1] || !seen[2] {
		t.Fatalf("missing lat/lon fields: %v", seen)
	}
}

func TestNavigateValidation(t *testing.T) {
	ctx := context.Background()
	v := &Vehicle{}
	if err := v.NavigateToGPS(ctx, 999, 0, navOrderUnknown); err == nil {
		t.Fatal("expected error for lat 999")
	}
	if err := v.NavigateToGPS(ctx, 0, 999, navOrderUnknown); err == nil {
		t.Fatal("expected error for lon 999")
	}
	if err := v.NavigateToGPS(ctx, math.NaN(), 0, navOrderUnknown); err == nil {
		t.Fatal("expected error for NaN lat")
	}
	if err := v.NavigateToGPS(ctx, 0, 0, 4); err == nil {
		t.Fatal("expected error for order 4")
	}
	if err := v.NavigateToDestination(ctx, ""); err == nil {
		t.Fatal("expected error for empty destination")
	}
	// Validation runs before any transport: these must fail even with no
	// connection, proving the checks precede the send path.
}
