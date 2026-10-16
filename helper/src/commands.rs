/// Every command the app may run, in protocol-library command-ID order: the
/// discriminant is the ABI command ID (see `helper/session/library/commands.go`).
/// The name is the kebab-cased variant. Append; never reorder.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::Display,
    strum::VariantArray,
)]
#[strum(serialize_all = "kebab-case")]
#[repr(u32)]
pub(crate) enum Command {
    ValetModeOn = 1,
    ValetModeOff,
    Unlock,
    Lock,
    Drive,
    ClimateOn,
    ClimateOff,
    ClimateSetTemp,
    AddKey,
    AddKeyRequest,
    RemoveKey,
    RenameKey,
    ListKeys,
    Honk,
    Ping,
    FlashLights,
    KeepAccessoryPower,
    LowPowerMode,
    ChargingSetLimit,
    ChargingSetAmps,
    ChargingStart,
    ChargingStop,
    ChargingSchedule,
    ChargingScheduleCancel,
    ChargingScheduleAdd,
    ChargingScheduleRemove,
    MediaSetVolume,
    MediaVolumeUp,
    MediaVolumeDown,
    MediaNextFavorite,
    MediaNextTrack,
    MediaPreviousTrack,
    MediaPreviousFavorite,
    MediaTogglePlayback,
    SoftwareUpdateStart,
    SoftwareUpdateCancel,
    SentryMode,
    Wake,
    TonneauOpen,
    TonneauClose,
    TonneauStop,
    TrunkOpen,
    TrunkMove,
    TrunkClose,
    FrunkOpen,
    ChargePortOpen,
    ChargePortClose,
    AutosecureModelx,
    SessionInfo,
    SeatHeater,
    SteeringWheelHeater,
    AutoSeatAndClimate,
    WindowsVent,
    WindowsClose,
    BodyControllerState,
    GuestModeOn,
    GuestModeOff,
    EraseGuestData,
    PreconditionScheduleAdd,
    PreconditionScheduleRemove,
    ProductInfo,
    State,
    VehicleName,
    SetVehicleName,
}

impl Command {
    /// The stable protocol-library command ID.
    pub(crate) fn id(self) -> u32 {
        self as u32
    }

    /// Whether the arguments carry a secret PIN. Their argument lists are
    /// redacted from the audit log rather than written to the system journal
    /// in cleartext. valet-mode-on is the only one in the pinned v0.4.1
    /// binary - verified by grepping `commands_vendor.go` for a "PIN"
    /// argument name; the parental-controls-* commands once listed here were
    /// removed since they don't exist upstream at all.
    pub(crate) fn has_pin(self) -> bool {
        self == Self::ValetModeOn
    }

    /// Whether the arguments carry schedule coordinates (location data).
    pub(crate) fn has_location(self) -> bool {
        matches!(
            self,
            Self::ChargingScheduleAdd | Self::PreconditionScheduleAdd
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use strum::VariantArray;

    /// The production Go library's stable command-ID table, in ID order.
    fn library_commands() -> Vec<&'static str> {
        let source = include_str!("../session/library/commands.go");
        let table = source
            .split_once("var commandNames = [...]string{")
            .unwrap()
            .1
            .split_once('}')
            .unwrap()
            .0;
        let regex = regex::Regex::new(r#""([a-z0-9-]+)""#).unwrap();
        let commands: Vec<_> = regex
            .captures_iter(table)
            .map(|capture| capture.get(1).unwrap().as_str())
            .collect();
        assert!(!commands.is_empty(), "library command extraction failed");
        commands
    }

    #[test]
    fn test_pin_commands_coverage() {
        assert!(Command::ValetModeOn.has_pin());
        for cmd in [
            Command::Unlock,
            Command::Honk,
            Command::TrunkOpen,
            Command::Ping,
        ] {
            assert!(
                !cmd.has_pin(),
                "non-secret command {cmd} incorrectly marked as pin-bearing"
            );
        }
    }

    #[test]
    fn test_command_catalog_matches_library_ids() {
        // Command IDs cross the ABI as discriminants, so order matters.
        let names: Vec<&str> = Command::VARIANTS
            .iter()
            .copied()
            .map(<&str>::from)
            .collect();
        assert_eq!(names, library_commands());
        for (index, cmd) in Command::VARIANTS.iter().enumerate() {
            assert_eq!(cmd.id() as usize, index + 1, "{cmd}");
            assert_eq!(cmd.to_string().parse::<Command>().unwrap(), *cmd);
        }
    }

    // Same check, applied to the QML app's own command catalog - the two
    // are maintained by hand independently (see CommandCatalog.js's own
    // header comment on why they aren't generated from one source), so
    // they can drift from upstream, and from each other, in different
    // ways. This is what would have caught findings like
    // parental-controls-* before they shipped as always-erroring buttons.
    #[test]
    fn test_qml_catalog_matches_upstream() {
        let js = include_str!("../../app/qml/js/CommandCatalog.js");
        let re = regex::Regex::new(r#"cmd\("([a-z0-9-]+)""#).unwrap();
        let js_commands: HashSet<&str> = re
            .captures_iter(js)
            .map(|c| c.get(1).unwrap().as_str())
            .collect();
        assert!(
            !js_commands.is_empty(),
            "regex found zero cmd(...) calls in CommandCatalog.js - the \
             file moved, was renamed, or the extraction pattern above is \
             stale; fix the test before trusting a green run"
        );

        let known: HashSet<_> = library_commands().into_iter().collect();
        let mut unknown: Vec<&str> = js_commands
            .iter()
            .copied()
            .filter(|c| !known.contains(c))
            .collect();
        unknown.sort_unstable();
        assert!(
            unknown.is_empty(),
            "CommandCatalog.js lists commands the protocol library \
             does not implement: {unknown:?}"
        );
    }
}
