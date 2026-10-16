package library

import (
	"context"
	"crypto/ecdh"
	"electric-eel-session/internal/phoneprotocol"
	"encoding/json"
	"fmt"
	"math"
	"time"

	"github.com/teslamotors/vehicle-command/pkg/protocol"
	"github.com/teslamotors/vehicle-command/pkg/protocol/protobuf/keys"
	"github.com/teslamotors/vehicle-command/pkg/protocol/protobuf/vcsec"
	"github.com/teslamotors/vehicle-command/pkg/vehicle"
	"google.golang.org/protobuf/encoding/protojson"
)

// IDs are stable ABI values; append new operations rather than reorder them.
// Match Rust COMMAND_CATALOG. Account-only IDs are explicitly unsupported BLE.
var commandNames = [...]string{
	"valet-mode-on", "valet-mode-off", "unlock", "lock", "drive",
	"climate-on", "climate-off", "climate-set-temp", "add-key", "add-key-request",
	"remove-key", "rename-key", "list-keys", "honk", "ping", "flash-lights",
	"keep-accessory-power", "low-power-mode", "charging-set-limit", "charging-set-amps",
	"charging-start", "charging-stop", "charging-schedule", "charging-schedule-cancel",
	"charging-schedule-add", "charging-schedule-remove", "media-set-volume", "media-volume-up",
	"media-volume-down", "media-next-favorite", "media-next-track", "media-previous-track",
	"media-previous-favorite", "media-toggle-playback", "software-update-start", "software-update-cancel",
	"sentry-mode", "wake", "tonneau-open", "tonneau-close", "tonneau-stop", "trunk-open",
	"trunk-move", "trunk-close", "frunk-open", "charge-port-open", "charge-port-close",
	"autosecure-modelx", "session-info", "seat-heater", "steering-wheel-heater", "auto-seat-and-climate",
	"windows-vent", "windows-close", "body-controller-state", "guest-mode-on", "guest-mode-off",
	"erase-guest-data", "precondition-schedule-add", "precondition-schedule-remove", "product-info", "state",
	"vehicle-name", "set-vehicle-name",
}

// Upper bound on a vehicle name accepted across the ABI (Rust enforces the
// user-facing limit in characters).
const maxVehicleNameBytes = 256

// Types: i integer/enum, f finite float, b byte string. Rust handles defaults,
// user parsing, schedule IDs and optional fields before crossing the ABI.
var commandSchema = [...]string{
	"b", "", "", "", "", "", "", "f", "bii", "bii", "b", "", "", "", "", "",
	"i", "i", "i", "i", "", "", "i", "", "iiiiiiffii", "ii", "f", "", "", "", "", "", "", "",
	"i", "", "i", "", "", "", "", "", "", "", "", "", "", "", "bi", "ii", "i", "ii", "", "", "", "", "", "",
	"iiiiffii", "ii", "", "i",
	"", "b",
}

func validateCommand(req Request) (string, error) {
	if req.Command == 0 || int(req.Command) > len(commandNames) {
		return "", fmt.Errorf("unknown BLE command ID")
	}
	name, schema := commandNames[req.Command-1], commandSchema[req.Command-1]
	if name == "rename-key" || name == "product-info" {
		return "", fmt.Errorf("operation requires Fleet API and is unavailable over BLE")
	}
	if len(req.Values) != len(schema) {
		return "", fmt.Errorf("invalid typed parameter count for %s", name)
	}
	for i, kind := range []byte(schema) {
		want := uint32(1)
		if kind == 'f' {
			want = 2
		}
		if kind == 'b' {
			want = 3
		}
		value := req.Values[i]
		if value.Kind != want {
			return "", fmt.Errorf("invalid typed parameter %d for %s", i, name)
		}
		if want == 2 && (math.IsNaN(value.Number) || math.IsInf(value.Number, 0)) {
			return "", fmt.Errorf("non-finite parameter")
		}
	}
	i := func(index int) uint64 { return req.Values[index].Integer }
	f := func(index int) float64 { return req.Values[index].Number }
	valid := true
	switch name {
	case "charging-set-limit":
		valid = i(0) >= 50 && i(0) <= 100
	case "charging-set-amps":
		valid = i(0) >= 1 && i(0) <= 48
	case "charging-schedule":
		valid = i(0) <= 1439
	case "climate-set-temp":
		valid = f(0) >= 15 && f(0) <= 28
	case "media-set-volume":
		valid = f(0) >= 0 && f(0) <= 10
	case "software-update-start":
		valid = i(0) <= uint64(time.Hour)
	case "keep-accessory-power", "low-power-mode", "sentry-mode", "steering-wheel-heater":
		valid = i(0) <= 1
	case "seat-heater":
		valid = i(0) >= 1 && i(0) <= 9 && i(0) != 4 && i(0) != 7 && i(1) <= 3
	case "auto-seat-and-climate":
		valid = i(0) >= 1 && i(0) <= 3 && i(1) <= 1
	case "state":
		valid = i(0) <= 11
	case "session-info":
		valid = i(1) == 1 || i(1) == 2
	case "set-vehicle-name":
		valid = len(req.Values[0].Bytes) > 0 && len(req.Values[0].Bytes) <= maxVehicleNameBytes
	case "add-key", "add-key-request":
		_, role := keys.Role_name[int32(i(1))]
		_, form := vcsec.KeyFormFactor_name[int32(i(2))]
		valid = i(1) <= 8 && i(2) <= 9 && role && form
	case "charging-schedule-remove", "precondition-schedule-remove":
		valid = i(0) <= 3
	case "charging-schedule-add":
		valid = i(1) >= 1 && i(1) <= 127 && i(2) <= 1439 && i(3) <= 1439 && i(4) <= 1 && i(5) <= 1 && (i(4) != 0 || i(5) != 0) && phoneprotocol.ValidLatLon(f(6), f(7)) && i(8) <= 1 && i(9) <= 1
	case "precondition-schedule-add":
		valid = i(1) >= 1 && i(1) <= 127 && i(2) <= 1439 && i(3) == 0 && phoneprotocol.ValidLatLon(f(4), f(5)) && i(6) <= 1 && i(7) <= 1
	}
	if !valid {
		return "", fmt.Errorf("typed parameter outside protocol range for %s", name)
	}
	return name, nil
}

func (t *Tesla) command(ctx context.Context, req Request) Result {
	name, err := validateCommand(req)
	if err != nil {
		return Result{Status: Invalid, Error: err.Error()}
	}
	// Rust connects explicitly; a dropped link is reported, never silently
	// re-established (presence owns reconnection and target freshness).
	if !t.linked() {
		return classify(protocol.ErrNotConnected)
	}
	if err := t.authenticate(ctx, phoneprotocol.CommandDomain(name), false); err != nil {
		return classify(err)
	}
	i := func(index int) uint64 { return req.Values[index].Integer }
	f := func(index int) float32 { return float32(req.Values[index].Number) }
	b := func(index int) []byte { return req.Values[index].Bytes }
	car := t.car
	var payload []byte
	switch name {
	case "valet-mode-on":
		err = car.EnableValetMode(ctx, string(b(0)))
	case "valet-mode-off":
		err = car.DisableValetMode(ctx)
	case "unlock":
		err = car.Unlock(ctx)
	case "lock":
		err = car.Lock(ctx)
	case "drive":
		err = car.RemoteDrive(ctx)
	case "climate-on":
		err = car.ClimateOn(ctx)
	case "climate-off":
		err = car.ClimateOff(ctx)
	case "climate-set-temp":
		err = car.ChangeClimateTemp(ctx, f(0), f(0))
	case "add-key", "add-key-request", "remove-key", "session-info":
		var key *ecdh.PublicKey
		key, err = ecdh.P256().NewPublicKey(b(0))
		if err != nil {
			return Result{Status: Invalid, Error: "invalid public key"}
		}
		switch name {
		case "add-key":
			err = car.AddKeyWithRole(ctx, key, keys.Role(i(1)), vcsec.KeyFormFactor(i(2)))
		case "add-key-request":
			err = car.SendAddKeyRequestWithRole(ctx, key, keys.Role(i(1)), vcsec.KeyFormFactor(i(2)))
		case "remove-key":
			err = car.RemoveKey(ctx, key)
		case "session-info":
			domain := protocol.DomainVCSEC
			if i(1) == 2 {
				domain = protocol.DomainInfotainment
			}
			info, e := car.SessionInfo(ctx, key, domain)
			err = e
			if err == nil {
				payload, err = protojson.Marshal(info)
			}
		}
	case "list-keys":
		summary, e := car.KeySummary(ctx)
		err = e
		if err == nil {
			list := []json.RawMessage{}
			for mask, slot := summary.GetSlotMask(), uint32(0); mask > 0; mask, slot = mask>>1, slot+1 {
				if mask&1 == 0 {
					continue
				}
				info, e := car.KeyInfoBySlot(ctx, slot)
				if e != nil {
					err = e
					break
				}
				data, e := protojson.Marshal(info)
				if e != nil {
					err = e
					break
				}
				list = append(list, data)
			}
			if err == nil {
				payload, err = json.Marshal(list)
			}
		}
	case "honk":
		err = car.HonkHorn(ctx)
	case "ping":
		err = car.Ping(ctx)
	case "flash-lights":
		err = car.FlashLights(ctx)
	case "keep-accessory-power":
		err = car.SetKeepAccessoryPowerMode(ctx, i(0) != 0)
	case "low-power-mode":
		err = car.SetLowPowerMode(ctx, i(0) != 0)
	case "charging-set-limit":
		err = car.ChangeChargeLimit(ctx, int32(i(0)))
	case "charging-set-amps":
		err = car.SetChargingAmps(ctx, int32(i(0)))
	case "charging-start":
		err = car.ChargeStart(ctx)
	case "charging-stop":
		err = car.ChargeStop(ctx)
	case "charging-schedule":
		err = car.ScheduleCharging(ctx, true, time.Duration(i(0))*time.Minute)
	case "charging-schedule-cancel":
		err = car.ScheduleCharging(ctx, false, 0)
	case "charging-schedule-add":
		schedule := vehicle.ChargeSchedule{Id: i(0), DaysOfWeek: int32(i(1)), StartTime: int32(i(2)), EndTime: int32(i(3)), StartEnabled: i(4) != 0, EndEnabled: i(5) != 0, Latitude: f(6), Longitude: f(7), OneTime: i(8) != 0, Enabled: i(9) != 0}
		err = car.AddChargeSchedule(ctx, &schedule)
		if err == nil {
			payload = []byte(fmt.Sprint(schedule.Id))
		}
	case "charging-schedule-remove":
		if i(0) == 3 {
			err = car.RemoveChargeSchedule(ctx, i(1))
		} else {
			err = car.BatchRemoveChargeSchedules(ctx, i(0) == 0, i(0) == 1, i(0) == 2)
		}
	case "media-set-volume":
		err = car.SetVolume(ctx, f(0))
	case "media-volume-up":
		err = car.VolumeUp(ctx)
	case "media-volume-down":
		err = car.VolumeDown(ctx)
	case "media-next-favorite":
		err = car.MediaNextFavorite(ctx)
	case "media-next-track":
		err = car.MediaNextTrack(ctx)
	case "media-previous-track":
		err = car.MediaPreviousTrack(ctx)
	case "media-previous-favorite":
		err = car.MediaPreviousFavorite(ctx)
	case "media-toggle-playback":
		err = car.ToggleMediaPlayback(ctx)
	case "software-update-start":
		err = car.ScheduleSoftwareUpdate(ctx, time.Duration(i(0)))
	case "software-update-cancel":
		err = car.CancelSoftwareUpdate(ctx)
	case "sentry-mode":
		err = car.SetSentryMode(ctx, i(0) != 0)
	case "wake":
		err = car.Wakeup(ctx)
	case "tonneau-open":
		err = car.OpenTonneau(ctx)
	case "tonneau-close":
		err = car.CloseTonneau(ctx)
	case "tonneau-stop":
		err = car.StopTonneau(ctx)
	case "trunk-open":
		err = car.OpenTrunk(ctx)
	case "trunk-move":
		err = car.ActuateTrunk(ctx)
	case "trunk-close":
		err = car.CloseTrunk(ctx)
	case "frunk-open":
		err = car.OpenFrunk(ctx)
	case "charge-port-open":
		err = car.OpenChargePort(ctx)
	case "charge-port-close":
		err = car.CloseChargePort(ctx)
	case "autosecure-modelx":
		err = car.AutoSecureVehicle(ctx)
	case "seat-heater":
		err = car.SetSeatHeater(ctx, map[vehicle.SeatPosition]vehicle.Level{vehicle.SeatPosition(i(0)): vehicle.Level(i(1))})
	case "steering-wheel-heater":
		err = car.SetSteeringWheelHeater(ctx, i(0) != 0)
	case "auto-seat-and-climate":
		var positions []vehicle.SeatPosition
		if i(0)&1 != 0 {
			positions = append(positions, vehicle.SeatFrontLeft)
		}
		if i(0)&2 != 0 {
			positions = append(positions, vehicle.SeatFrontRight)
		}
		err = car.AutoSeatAndClimate(ctx, positions, i(1) != 0)
	case "windows-vent":
		err = car.VentWindows(ctx)
	case "windows-close":
		err = car.CloseWindows(ctx)
	case "body-controller-state":
		return t.Execute(ctx, Request{Operation: BodyControllerState})
	case "guest-mode-on":
		err = car.SetGuestMode(ctx, true)
	case "guest-mode-off":
		err = car.SetGuestMode(ctx, false)
	case "erase-guest-data":
		err = car.EraseGuestData(ctx)
	case "precondition-schedule-add":
		schedule := vehicle.PreconditionSchedule{Id: i(0), DaysOfWeek: int32(i(1)), PreconditionTime: int32(i(2)), Latitude: f(4), Longitude: f(5), OneTime: i(6) != 0, Enabled: i(7) != 0}
		err = car.AddPreconditionSchedule(ctx, &schedule)
		if err == nil {
			payload = []byte(fmt.Sprint(schedule.Id))
		}
	case "precondition-schedule-remove":
		if i(0) == 3 {
			err = car.RemovePreconditionSchedule(ctx, i(1))
		} else {
			err = car.BatchRemovePreconditionSchedules(ctx, i(0) == 0, i(0) == 1, i(0) == 2)
		}
	case "state":
		state, e := car.GetState(ctx, vehicle.StateCategory(i(0)))
		err = e
		if err == nil {
			payload, err = protojson.Marshal(state)
		}
	case "vehicle-name":
		name, e := car.GetVehicleName(ctx)
		err = e
		payload = []byte(name)
	case "set-vehicle-name":
		err = car.SetVehicleName(ctx, string(b(0)))
	default:
		return Result{Status: Invalid, Error: "unsupported command"}
	}
	result := classify(err)
	result.Payload = payload
	if result.Status == Uncertain || result.Status == Cancelled || result.Status == TimedOut || result.Status == NotConnected {
		t.disconnect()
	}
	return result
}
