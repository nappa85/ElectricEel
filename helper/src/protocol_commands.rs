//! The single application-argument parser: validates user input (ranges,
//! arity, units) and produces the typed operation sent to Go. Nothing else
//! re-parses command arguments.
use crate::commands::Command;
use crate::tesla_ble::{Domain, Operation, Value};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

/// Declares a case-insensitive argument vocabulary whose discriminant is the
/// value sent to the protocol library.
macro_rules! arg_enum {
    ($(#[$meta:meta])* enum $name:ident {
        $($(#[$variant_meta:meta])* $variant:ident = $value:literal,)+
    }) => {
        #[derive(Clone, Copy, PartialEq, Eq, strum::EnumString)]
        #[strum(ascii_case_insensitive)]
        $(#[$meta])*
        #[repr(u64)]
        enum $name {
            $($(#[$variant_meta])* $variant = $value,)+
        }

        impl From<$name> for Value {
            fn from(value: $name) -> Self {
                Value::Integer(value as u64)
            }
        }
    };
}

arg_enum! {
    enum Switch {
        #[strum(serialize = "on", serialize = "true")]
        On = 1,
        #[strum(serialize = "off", serialize = "false")]
        Off = 0,
    }
}

arg_enum! {
    #[strum(serialize_all = "snake_case")]
    enum KeyRole {
        Owner = 2,
        Driver = 3,
        Fm = 4,
        VehicleMonitor = 5,
        ChargingManager = 6,
    }
}

arg_enum! {
    #[strum(serialize_all = "snake_case")]
    enum KeyFormFactor {
        NfcCard = 1,
        IosDevice = 6,
        AndroidDevice = 7,
        CloudKey = 9,
    }
}

arg_enum! {
    #[strum(serialize_all = "lowercase")]
    enum SessionDomain {
        Vcsec = 1,
        Infotainment = 2,
    }
}

arg_enum! {
    enum Seat {
        #[strum(serialize = "front-left")]
        FrontLeft = 1,
        #[strum(serialize = "front-right")]
        FrontRight = 2,
        #[strum(serialize = "2nd-row-left")]
        SecondRowLeft = 3,
        #[strum(serialize = "2nd-row-center")]
        SecondRowCenter = 5,
        #[strum(serialize = "2nd-row-right")]
        SecondRowRight = 6,
        #[strum(serialize = "3rd-row-left")]
        ThirdRowLeft = 8,
        #[strum(serialize = "3rd-row-right")]
        ThirdRowRight = 9,
    }
}

arg_enum! {
    #[strum(serialize_all = "lowercase")]
    enum HeaterLevel {
        Off = 0,
        Low = 1,
        Medium = 2,
        High = 3,
    }
}

arg_enum! {
    /// Seats for auto seat-and-climate, as a left/right bitmask.
    enum FrontSeats {
        #[strum(serialize = "L")]
        Left = 1,
        #[strum(serialize = "R")]
        Right = 2,
        #[strum(serialize = "LR", serialize = "RL")]
        Both = 3,
    }
}

arg_enum! {
    #[strum(serialize_all = "kebab-case")]
    enum StateCategory {
        Charge = 0,
        Climate = 1,
        Drive = 2,
        Location = 3,
        Closures = 4,
        ChargeSchedule = 5,
        PreconditionSchedule = 6,
        TirePressure = 7,
        Media = 8,
        MediaDetail = 9,
        SoftwareUpdate = 10,
        ParentalControls = 11,
    }
}

arg_enum! {
    #[strum(serialize_all = "lowercase")]
    enum ScheduleSelection {
        Home = 0,
        Work = 1,
        Other = 2,
        Id = 3,
    }
}

arg_enum! {
    /// Whether a schedule runs once (1) or repeats weekly (0).
    #[strum(serialize_all = "lowercase")]
    enum Repeat {
        Once = 1,
        Weekly = 0,
    }
}

arg_enum! {
    /// Schedule days as a Sunday-first bitmask.
    enum Days {
        #[strum(serialize = "sun", serialize = "sunday")]
        Sunday = 1,
        #[strum(serialize = "mon", serialize = "monday")]
        Monday = 2,
        #[strum(serialize = "tue", serialize = "tues", serialize = "tuesday")]
        Tuesday = 4,
        #[strum(serialize = "wed", serialize = "wednesday")]
        Wednesday = 8,
        #[strum(serialize = "thu", serialize = "thur", serialize = "thurs", serialize = "thursday")]
        Thursday = 16,
        #[strum(serialize = "fri", serialize = "friday")]
        Friday = 32,
        #[strum(serialize = "sat", serialize = "saturday")]
        Saturday = 64,
        #[strum(serialize = "all")]
        All = 127,
        #[strum(serialize = "weekdays")]
        Weekdays = 62,
    }
}

fn integer(value: u64) -> Value {
    Value::Integer(value)
}
fn finite(text: &str) -> Result<f64, String> {
    let value: f64 = text.trim().parse().map_err(|_| "expected a number")?;
    if !value.is_finite() {
        return Err("expected a finite number".into());
    }
    Ok(value)
}
fn bounded_number(text: &str, low: f64, high: f64) -> Result<Value, String> {
    let value = finite(text)?;
    if !(low..=high).contains(&value) {
        return Err(format!("{value} out of range [{low}, {high}]"));
    }
    Ok(Value::Number(value))
}
fn unsigned(text: &str) -> Result<Value, String> {
    Ok(integer(
        text.trim()
            .parse()
            .map_err(|_| "expected a nonnegative integer")?,
    ))
}
fn bounded(text: &str, low: u64, high: u64) -> Result<Value, String> {
    let value: u64 = text
        .trim()
        .parse()
        .map_err(|_| "expected a nonnegative integer")?;
    if !(low..=high).contains(&value) {
        return Err(format!("{value} out of range [{low}, {high}]"));
    }
    Ok(integer(value))
}
/// Parses one argument of a named vocabulary, ignoring surrounding whitespace.
fn arg<T: FromStr>(text: &str, error: &str) -> Result<T, String> {
    text.trim().parse().map_err(|_| error.into())
}
fn on(text: &str) -> Result<Value, String> {
    arg::<Switch>(text, "expected on/off or true/false").map(Value::from)
}
fn named<T: FromStr + Into<Value>>(text: &str) -> Result<Value, String> {
    arg::<T>(text, "invalid enum value").map(Into::into)
}
fn minutes(text: &str) -> Result<u64, String> {
    let (hour, minute) = text.split_once(':').ok_or("expected HH:MM")?;
    let hour: u64 = hour.trim().parse().map_err(|_| "invalid hour")?;
    let minute: u64 = minute.trim().parse().map_err(|_| "invalid minute")?;
    if hour > 23 || minute > 59 {
        return Err("invalid time".into());
    }
    Ok(hour * 60 + minute)
}
fn days(text: &str) -> Result<u64, String> {
    let mut mask = 0;
    for day in text.split(',') {
        mask |= arg::<Days>(day, "invalid schedule day")? as u64;
    }
    Ok(mask)
}
fn arity(args: &[String], min: usize, max: usize) -> Result<(), String> {
    if (min..=max).contains(&args.len()) {
        Ok(())
    } else {
        Err("invalid argument count".into())
    }
}
/// Splits an optional single trailing unit letter: "21C" -> ("21", Some('c')).
fn unit(text: &str) -> (&str, Option<char>) {
    let text = text.trim();
    match text.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => {
            (&text[..text.len() - 1], Some(c.to_ascii_lowercase()))
        }
        _ => (text, None),
    }
}
/// Celsius, accepting an optional C/F suffix. Bounds match CommandCatalog.js.
fn temperature(text: &str) -> Result<Value, String> {
    let (value, unit) = unit(text);
    let value = finite(value).map_err(|_| "invalid temperature")?;
    let celsius = match unit {
        None | Some('c') => value,
        Some('f') if (59.0..=82.0).contains(&value) => (value - 32.0) * 5.0 / 9.0,
        _ => return Err("invalid temperature".into()),
    };
    if unit != Some('f') && !(15.0..=28.0).contains(&celsius) {
        return Err("temperature out of range".into());
    }
    Ok(Value::Number(celsius))
}
/// Delay in nanoseconds: bare/`s` integer seconds, or `m`/`h` decimals; at
/// most one hour and never negative.
fn delay(text: &str) -> Result<Value, String> {
    if text.trim().starts_with('-') {
        return Err("invalid delay".into());
    }
    let (value, unit) = unit(text);
    let seconds = match unit {
        // Integer seconds are exactly representable in f64.
        #[allow(clippy::cast_precision_loss)]
        None | Some('s') => value.trim().parse::<u64>().map_err(|_| "invalid delay")? as f64,
        Some('m') => finite(value).map_err(|_| "invalid delay")? * 60.0,
        Some('h') => finite(value).map_err(|_| "invalid delay")? * 3600.0,
        _ => return Err("invalid delay".into()),
    };
    if !(0.0..=3600.0).contains(&seconds) {
        return Err("invalid delay".into());
    }
    Ok(integer(
        std::time::Duration::from_secs_f64(seconds)
            .as_nanos()
            .try_into()
            .map_err(|_| "delay overflow")?,
    ))
}

/// The authentication domain a command needs on its link.
pub(crate) fn domain(cmd: Command) -> Domain {
    match cmd {
        Command::AddKeyRequest | Command::SessionInfo => Domain::Unauthenticated,
        Command::Lock
        | Command::Unlock
        | Command::Drive
        | Command::Wake
        | Command::BodyControllerState
        | Command::AddKey
        | Command::RemoveKey
        | Command::ListKeys
        | Command::TrunkOpen
        | Command::TrunkMove
        | Command::TrunkClose
        | Command::FrunkOpen
        | Command::TonneauOpen
        | Command::TonneauClose
        | Command::TonneauStop
        | Command::AutosecureModelx => Domain::Vcsec,
        _ => Domain::Infotainment,
    }
}

#[allow(clippy::too_many_lines)] // Exhaustive typed mapping of the app command catalogue.
pub(crate) fn parse(cmd: Command, args: &[String]) -> Result<Operation, String> {
    let values = match cmd {
        Command::RenameKey | Command::ProductInfo => {
            return Err("operation requires Fleet API and is unavailable over BLE".into())
        }
        Command::ValetModeOn => {
            arity(args, 1, 1)?;
            vec![Value::Bytes(args[0].as_bytes().to_vec())]
        }
        Command::SetVehicleName => {
            arity(args, 1, 1)?;
            let name = crate::config::validate_vehicle_name(&args[0])?;
            vec![Value::Bytes(name.as_bytes().to_vec())]
        }
        Command::ClimateSetTemp => {
            arity(args, 1, 1)?;
            vec![temperature(&args[0])?]
        }
        Command::KeepAccessoryPower
        | Command::LowPowerMode
        | Command::SentryMode
        | Command::SteeringWheelHeater => {
            arity(args, 1, 1)?;
            vec![on(&args[0])?]
        }
        Command::ChargingSetLimit => {
            arity(args, 1, 1)?;
            vec![bounded(&args[0], 50, 100)?]
        }
        Command::ChargingSetAmps => {
            arity(args, 1, 1)?;
            vec![bounded(&args[0], 1, 48)?]
        }
        Command::ChargingSchedule => {
            arity(args, 1, 1)?;
            vec![bounded(&args[0], 0, 1439)?]
        }
        Command::MediaSetVolume => {
            arity(args, 1, 1)?;
            vec![bounded_number(&args[0], 0.0, 10.0)?]
        }
        Command::SoftwareUpdateStart => {
            arity(args, 1, 1)?;
            vec![delay(&args[0])?]
        }
        Command::AddKey | Command::AddKeyRequest => {
            arity(args, 3, 3)?;
            vec![
                Value::Bytes(crate::keys::public_bytes(std::path::Path::new(&args[0]))?),
                named::<KeyRole>(&args[1])?,
                named::<KeyFormFactor>(&args[2])?,
            ]
        }
        Command::RemoveKey => {
            arity(args, 1, 1)?;
            vec![Value::Bytes(crate::keys::public_bytes(
                std::path::Path::new(&args[0]),
            )?)]
        }
        Command::SessionInfo => {
            arity(args, 2, 2)?;
            vec![
                Value::Bytes(crate::keys::public_bytes(std::path::Path::new(&args[0]))?),
                named::<SessionDomain>(&args[1])?,
            ]
        }
        Command::SeatHeater => {
            arity(args, 2, 2)?;
            vec![named::<Seat>(&args[0])?, named::<HeaterLevel>(&args[1])?]
        }
        Command::AutoSeatAndClimate => {
            arity(args, 1, 2)?;
            vec![
                arg::<FrontSeats>(&args[0], "invalid seat selection")?.into(),
                on(args
                    .get(1)
                    .filter(|value| !value.is_empty())
                    .map_or("on", String::as_str))?,
            ]
        }
        Command::State => {
            arity(args, 1, 1)?;
            vec![named::<StateCategory>(&args[0])?]
        }
        Command::ChargingScheduleRemove | Command::PreconditionScheduleRemove => {
            arity(args, 1, 2)?;
            let selection = arg::<ScheduleSelection>(&args[0], "invalid schedule selection")?;
            let id = if selection == ScheduleSelection::Id {
                unsigned(args.get(1).ok_or("missing schedule ID")?)?
            } else {
                integer(0)
            };
            vec![selection.into(), id]
        }
        Command::ChargingScheduleAdd | Command::PreconditionScheduleAdd => {
            arity(args, 4, 7)?;
            let repeat = args
                .get(4)
                .filter(|arg| !arg.is_empty())
                .map_or("weekly", String::as_str);
            let once = arg::<Repeat>(repeat, "invalid repeat policy")?;
            let id = if let Some(id) = args.get(5).filter(|id| !id.is_empty()) {
                unsigned(id)?
            } else {
                integer(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_err(|_| "invalid system clock")?
                        .as_nanos()
                        .try_into()
                        .map_err(|_| "schedule ID overflow")?,
                )
            };
            let enabled = on(args
                .get(6)
                .filter(|arg| !arg.is_empty())
                .map_or("true", String::as_str))?;
            let mut values = vec![id, integer(days(&args[0])?)];
            if cmd == Command::ChargingScheduleAdd {
                let (start, end) = args[1].split_once('-').ok_or("invalid time interval")?;
                if start.is_empty() && end.is_empty() {
                    return Err("empty time interval".into());
                }
                values.extend([
                    integer(if start.is_empty() { 0 } else { minutes(start)? }),
                    integer(if end.is_empty() { 0 } else { minutes(end)? }),
                    integer(u64::from(!start.is_empty())),
                    integer(u64::from(!end.is_empty())),
                ]);
            } else {
                values.extend([integer(minutes(&args[1])?), integer(0)]);
            }
            values.extend([
                bounded_number(&args[2], -90.0, 90.0)?,
                bounded_number(&args[3], -180.0, 180.0)?,
                once.into(),
                enabled,
            ]);
            values
        }
        _ => {
            arity(args, 0, 0)?;
            Vec::new()
        }
    };
    Ok(Operation::Command {
        command: cmd.id(),
        values,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn optional_schedule_slots_preserve_end_only_time_and_existing_id() {
        let args = ["weekdays", "-06:00", "-33.4", "151.2", "", "123", "false"].map(str::to_string);
        let Operation::Command { values, .. } = parse(Command::ChargingScheduleAdd, &args).unwrap()
        else {
            panic!("command")
        };
        assert!(matches!(values[0], Value::Integer(123)));
        assert!(matches!(values[3], Value::Integer(360)));
        assert!(matches!(values[4], Value::Integer(0)));
        assert!(matches!(values[5], Value::Integer(1)));
        assert!(matches!(values[9], Value::Integer(0)));
    }
    #[test]
    fn every_zero_argument_ble_command_has_a_typed_mapping() {
        for command in [
            Command::Lock,
            Command::Unlock,
            Command::Drive,
            Command::Honk,
            Command::BodyControllerState,
            Command::WindowsClose,
            Command::GuestModeOn,
            Command::ListKeys,
        ] {
            assert!(parse(command, &[]).is_ok(), "{command}");
        }
        assert!(parse(Command::Lock, &["extra".into()]).is_err());
        assert!(parse(Command::ProductInfo, &[]).is_err());
    }
    fn ok(cmd: Command, args: &[&str]) -> bool {
        parse(
            cmd,
            &args.iter().map(ToString::to_string).collect::<Vec<_>>(),
        )
        .is_ok()
    }

    #[test]
    fn named_arguments_map_to_protocol_values_case_insensitively() {
        let values = |cmd, args: &[&str]| {
            let args: Vec<String> = args.iter().map(ToString::to_string).collect();
            let Operation::Command { values, .. } = parse(cmd, &args).unwrap() else {
                panic!("command")
            };
            values
                .into_iter()
                .map(|value| match value {
                    Value::Integer(integer) => integer,
                    other => panic!("{other:?}"),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            values(Command::SeatHeater, &["2nd-row-left", "HIGH"]),
            [3, 3]
        );
        assert_eq!(values(Command::State, &[" Tire-Pressure "]), [7]);
        assert_eq!(values(Command::State, &["drive"]), [2]);
        assert_eq!(values(Command::AutoSeatAndClimate, &["rl", "off"]), [3, 0]);
        assert_eq!(values(Command::ChargingScheduleRemove, &["work"]), [1, 0]);
        assert_eq!(
            values(Command::ChargingScheduleRemove, &["ID", "9"]),
            [3, 9]
        );
        assert_eq!(days("Thurs, sat").unwrap(), 80);
        assert_eq!(days("weekdays").unwrap(), 62);
        assert!(days("someday").is_err());
        assert!(!ok(Command::SeatHeater, &["front-left", "max"]));
        assert!(!ok(Command::ChargingScheduleRemove, &["id"]));
    }

    #[test]
    fn bounded_commands_enforce_catalog_ranges() {
        assert!(ok(Command::ChargingSetLimit, &["50"]));
        assert!(ok(Command::ChargingSetLimit, &["100"]));
        assert!(!ok(Command::ChargingSetLimit, &["49"]));
        assert!(!ok(Command::ChargingSetLimit, &["101"]));
        assert!(!ok(Command::ChargingSetLimit, &["abc"]));
        assert!(!ok(Command::ChargingSetLimit, &[]));
        assert!(!ok(Command::ChargingSetLimit, &["80", "extra"]));
        assert!(ok(Command::ChargingSetAmps, &["1"]));
        assert!(ok(Command::ChargingSetAmps, &["48"]));
        assert!(!ok(Command::ChargingSetAmps, &["0"]));
        assert!(!ok(Command::ChargingSetAmps, &["49"]));
        assert!(!ok(Command::ChargingSchedule, &["9999"]));
        assert!(ok(Command::MediaSetVolume, &["10"]));
        assert!(!ok(Command::MediaSetVolume, &["99"]));
        assert!(!ok(Command::MediaSetVolume, &["NaN"]));
        assert!(!ok(Command::ClimateSetTemp, &["999C"]));
        assert!(!ok(Command::ClimateSetTemp, &["21x"]));
        assert!(ok(Command::ClimateSetTemp, &["21"]));
        assert!(ok(Command::ClimateSetTemp, &["21 C"]));
        assert!(ok(Command::ClimateSetTemp, &["70F"]));
        assert!(!ok(Command::ClimateSetTemp, &["21F"]));
    }

    #[test]
    fn fahrenheit_is_converted_to_celsius() {
        let Operation::Command { values, .. } =
            parse(Command::ClimateSetTemp, &["68F".into()]).unwrap()
        else {
            panic!("command")
        };
        assert!(matches!(values[0], Value::Number(c) if (c - 20.0).abs() < 1e-9));
    }

    #[test]
    fn software_update_delay_rejects_negative_garbage_and_long_values() {
        for bad in [
            "-1s", "-1m", "-1h", " -1h ", "10x", "abc", "10z", "999999h", "999999m", "99999s",
            "1.5", "1.5s",
        ] {
            assert!(!ok(Command::SoftwareUpdateStart, &[bad]), "{bad:?}");
        }
        assert!(!ok(Command::SoftwareUpdateStart, &["0", "extra"]));
        for good in ["600", "600s", "1.5m", "1h", "0"] {
            assert!(ok(Command::SoftwareUpdateStart, &[good]), "{good:?}");
        }
    }

    #[test]
    fn schedules_accept_signed_coordinates_and_reject_out_of_range_ones() {
        for args in [
            ["all", "22:00-06:00", "-33.8688", "151.2093"],
            ["all", "22:00-06:00", "37.7749", "-122.4194"],
            ["all", "-06:00", "48.8584", "2.2945"],
        ] {
            assert!(ok(Command::ChargingScheduleAdd, &args), "{args:?}");
        }
        assert!(!ok(
            Command::ChargingScheduleAdd,
            &["all", "22:00-06:00", "91", "0"]
        ));
        assert!(!ok(
            Command::PreconditionScheduleAdd,
            &["all", "22:00", "0", "181"]
        ));
    }

    #[test]
    fn audit_software_update_accepts_uppercase_duration_units() {
        // Duration units are case-insensitive.
        for arg in ["1H", "10M", "10S", "0H", "5M"] {
            assert!(
                parse(Command::SoftwareUpdateStart, &[arg.to_string()]).is_ok(),
                "software-update-start {arg:?} must be accepted"
            );
        }
        assert!(parse(Command::SoftwareUpdateStart, &["10H".to_string()]).is_err());
    }
    #[test]
    fn audit_schedule_time_trims_surrounding_whitespace() {
        // minutes() parses hour/minute without trimming, unlike days() which
        // trims each element. A pasted "22:00 " or " 22:00-06:00 " fails with
        // "invalid minute/hour" instead of scheduling. Whitespace from
        // copy-paste must be tolerated.
        let args = ["weekdays", "22:00-06:00 ", "48.85", "2.29"].map(str::to_string);
        assert!(
            parse(Command::ChargingScheduleAdd, &args).is_ok(),
            "trailing space in TIME must be trimmed, not rejected"
        );
        let args = ["weekdays", " 22:00", "48.85", "2.29"].map(str::to_string);
        assert!(
            parse(Command::PreconditionScheduleAdd, &args).is_ok(),
            "leading space in TIME must be trimmed, not rejected"
        );
    }
}
