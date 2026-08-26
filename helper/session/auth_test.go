package main

import (
	"bytes"
	"testing"

	"github.com/teslamotors/vehicle-command/pkg/protocol"
	universal "github.com/teslamotors/vehicle-command/pkg/protocol/protobuf/universalmessage"
	"google.golang.org/protobuf/encoding/protowire"
	"google.golang.org/protobuf/proto"
)

// teslabtapiExampleAuthRequest is the length-stripped FromVCSECMessage from
// https://www.teslabtapi.com/docs/start (AuthenticationRequest for DRIVE).
// Wire: field 3 { field 2 { field 1 = token }, field 3 = 2 }.
var teslabtapiExampleAuthRequest = []byte{
	0x1a, 0x0a, 0x12, 0x06, 0x0a, 0x04, 0x00, 0x01, 0x0f, 0x2c, 0x18, 0x02,
}

func TestParseAuthenticationRequestBareFromVCSEC(t *testing.T) {
	req, ok := parseAuthenticationRequest(teslabtapiExampleAuthRequest)
	if !ok {
		t.Fatal("expected AuthenticationRequest from bare FromVCSECMessage")
	}
	if req.RequestedLevel != authLevelDrive {
		t.Errorf("RequestedLevel = %d, want DRIVE (%d)", req.RequestedLevel, authLevelDrive)
	}
	wantToken := []byte{0x00, 0x01, 0x0f, 0x2c}
	if !bytes.Equal(req.Token, wantToken) {
		t.Errorf("Token = %x, want %x", req.Token, wantToken)
	}
}

func TestParseAuthenticationRequestInsideRoutableMessage(t *testing.T) {
	rm := &universal.RoutableMessage{
		ToDestination: &universal.Destination{
			SubDestination: &universal.Destination_RoutingAddress{
				RoutingAddress: []byte{1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16},
			},
		},
		FromDestination: &universal.Destination{
			SubDestination: &universal.Destination_Domain{
				Domain: universal.Domain_DOMAIN_VEHICLE_SECURITY,
			},
		},
		Payload: &universal.RoutableMessage_ProtobufMessageAsBytes{
			ProtobufMessageAsBytes: teslabtapiExampleAuthRequest,
		},
	}
	raw, err := proto.Marshal(rm)
	if err != nil {
		t.Fatal(err)
	}
	req, ok := parseAuthenticationRequest(raw)
	if !ok {
		t.Fatal("expected AuthenticationRequest inside RoutableMessage")
	}
	if req.RequestedLevel != authLevelDrive {
		t.Errorf("RequestedLevel = %d, want DRIVE", req.RequestedLevel)
	}
}

func TestParseAuthenticationRequestOmittedLevelIsIgnored(t *testing.T) {
	// Identification-style AuthenticationRequest with a token and no
	// requestedLevel must not parse as a grantable NONE request. Treating
	// it as NONE made the responder send AuthenticationResponse{NONE} and
	// broke refresh+handle-pull.
	tokenInner := protowire.AppendTag(nil, 1, protowire.BytesType)
	tokenInner = protowire.AppendBytes(tokenInner, []byte{0x00, 0x01, 0x0f, 0x2c})
	authReq := protowire.AppendTag(nil, 2, protowire.BytesType)
	authReq = protowire.AppendBytes(authReq, tokenInner)
	from := protowire.AppendTag(nil, 3, protowire.BytesType)
	from = protowire.AppendBytes(from, authReq)

	if _, ok := parseAuthenticationRequest(from); ok {
		t.Fatal("omitted requestedLevel must not parse as a grantable request")
	}
}

func TestParseAuthenticationRequestUnlockLevel(t *testing.T) {
	// FromVCSECMessage { authenticationRequest { requestedLevel = UNLOCK } }
	authReq := protowire.AppendTag(nil, 3, protowire.VarintType)
	authReq = protowire.AppendVarint(authReq, authLevelUnlock)
	from := protowire.AppendTag(nil, 3, protowire.BytesType)
	from = protowire.AppendBytes(from, authReq)

	req, ok := parseAuthenticationRequest(from)
	if !ok {
		t.Fatal("expected AuthenticationRequest")
	}
	if req.RequestedLevel != authLevelUnlock {
		t.Errorf("RequestedLevel = %d, want UNLOCK", req.RequestedLevel)
	}
}

func TestParseAuthenticationRequestIgnoresOrdinaryPayload(t *testing.T) {
	cases := [][]byte{
		nil,
		{},
		{0x01, 0x02, 0x03},
		// FromVCSECMessage with CommandStatus (field 4), not auth request.
		protowire.AppendVarint(protowire.AppendTag(nil, 4, protowire.BytesType), 0),
	}
	for i, c := range cases {
		if _, ok := parseAuthenticationRequest(c); ok {
			t.Errorf("case %d: unexpectedly parsed AuthenticationRequest", i)
		}
	}
}

func TestEncodeAuthenticationResponseWireLayout(t *testing.T) {
	got := encodeAuthenticationResponse(authLevelDrive)
	// UnsignedMessage { authenticationResponse { authenticationLevel = 2 } }
	// field 3 (bytes) containing field 1 (varint) = 2
	want := []byte{0x1a, 0x02, 0x08, 0x02}
	if !bytes.Equal(got, want) {
		t.Errorf("encodeAuthenticationResponse(DRIVE) = %x, want %x", got, want)
	}

	gotUnlock := encodeAuthenticationResponse(authLevelUnlock)
	wantUnlock := []byte{0x1a, 0x02, 0x08, 0x01}
	if !bytes.Equal(gotUnlock, wantUnlock) {
		t.Errorf("encodeAuthenticationResponse(UNLOCK) = %x, want %x", gotUnlock, wantUnlock)
	}
}

func TestEncodeAuthenticationResponseRoundTripLevel(t *testing.T) {
	for _, level := range []int{authLevelUnlock, authLevelDrive} {
		encoded := encodeAuthenticationResponse(level)
		// Re-parse the AuthenticationResponse message itself (field 1).
		num, typ, n := protowire.ConsumeTag(encoded)
		if n < 0 || num != 3 || typ != protowire.BytesType {
			t.Fatalf("level %d: outer tag malformed", level)
		}
		inner, m := protowire.ConsumeBytes(encoded[n:])
		if m < 0 {
			t.Fatalf("level %d: outer bytes malformed", level)
		}
		num, typ, n = protowire.ConsumeTag(inner)
		if n < 0 || num != 1 || typ != protowire.VarintType {
			t.Fatalf("level %d: inner tag malformed", level)
		}
		v, m := protowire.ConsumeVarint(inner[n:])
		if m < 0 || int(v) != level {
			t.Fatalf("level %d: got varint %d", level, v)
		}
	}
}

func TestSendAuthenticationResponseRejectsNoneAndUnknown(t *testing.T) {
	if err := sendAuthenticationResponse(nil, nil, authLevelNone); err == nil {
		t.Fatal("expected error for NONE level")
	}
	if err := sendAuthenticationResponse(nil, nil, 99); err == nil {
		t.Fatal("expected error for unknown level")
	}
}

func TestSessionDroppedError(t *testing.T) {
	if sessionDroppedError(nil) {
		t.Fatal("nil must not count as dropped")
	}
	if !sessionDroppedError(protocol.ErrNotConnected) {
		t.Fatal("ErrNotConnected should count as dropped")
	}
	if !sessionDroppedError(protocol.ErrNoSession) {
		t.Fatal("ErrNoSession should count as dropped")
	}
}
