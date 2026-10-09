package bluez

import (
	"errors"
	"fmt"
	"testing"

	"github.com/godbus/dbus"
)

func TestBluetoothOffClassificationUsesTypedCause(t *testing.T) {
	for _, err := range []error{
		&AdapterOffError{Cause: errors.New("arbitrary diagnostic")},
		fmt.Errorf("wrapper: %w", &AdapterOffError{Cause: errors.New("localized message")}),
		dbus.NewError("org.bluez.Error.NotPowered", nil),
	} {
		if !IsBluetoothOff(err) {
			t.Fatalf("expected power failure for %T", err)
		}
	}
	if IsBluetoothOff(errors.New("NotPowered RFKILL power on adapter")) {
		t.Fatal("diagnostic prose must not be treated as a machine error code")
	}
}
