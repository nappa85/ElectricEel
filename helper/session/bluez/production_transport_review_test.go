package bluez

import (
	"context"
	"errors"
	"testing"

	"github.com/godbus/dbus"
)

// Fail after one ATT chunk has reached the vehicle, leaving an incomplete
// length-prefixed datagram in the remote receiver.
type reviewPartialWriteBus struct {
	*fakeBluez
	chunkCalls int
}

func (b *reviewPartialWriteBus) object(dest string, path dbus.ObjectPath) dbusCaller {
	return &reviewPartialWriteCaller{dbusCaller: b.fakeBluez.object(dest, path), bus: b}
}

type reviewPartialWriteCaller struct {
	dbusCaller
	bus *reviewPartialWriteBus
}

func (c *reviewPartialWriteCaller) call(ctx context.Context, method string, args ...interface{}) ([]interface{}, error) {
	if method == gattChrIface+".WriteValue" {
		c.bus.chunkCalls++
		if c.bus.chunkCalls == 2 {
			return nil, errors.New("ATT write failed after first chunk")
		}
	}
	return c.dbusCaller.call(ctx, method, args...)
}

func TestProductionReviewPartialSendRetiresFramingStream(t *testing.T) {
	fake := newFakeBluez()
	bus := &reviewPartialWriteBus{fakeBluez: fake}
	c := newTestConnection(fake)
	c.bus = bus
	c.blockLength = defaultMTU - 3
	defer c.Close()
	if err := c.Send(context.Background(), make([]byte, 40)); err == nil {
		t.Fatal("fixture must fail after transmitting the first chunk")
	}
	if len(fake.writes) != 1 {
		t.Fatalf("fixture transmitted %d chunks, want one", len(fake.writes))
	}
	select {
	case <-c.Dropped():
	default:
		t.Error("partial datagram failure did not notify presence that the framing stream is unusable")
	}
	before := len(fake.writes)
	if err := c.Send(context.Background(), []byte("next command")); err == nil {
		t.Error("next command succeeded on a stream containing an unfinished previous frame")
	}
	if len(fake.writes) != before {
		t.Error("new command bytes were appended to the vehicle's incomplete previous frame")
	}
}
