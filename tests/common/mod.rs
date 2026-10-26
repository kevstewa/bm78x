//! Helpers shared by the integration tests.

#![allow(dead_code)]

use bm78x::protocol::{
    Category, CommandId, ControlKind, ControlPacket, DeviceInfo, Failure, Flags, Frame, Function,
    MacAddress, MeterTime, Password, Prefix, Reading, Unit,
};

/// A valid notification from the meter at `mac` whose reading field
/// holds `n`.
pub fn frame(mac: MacAddress, n: i32) -> Vec<u8> {
    let reading = Reading {
        logging_set_id: 1,
        packet_id: 1,
        time: MeterTime::default(),
        flags: Flags::default(),
        device_type: 1,
        function: Function::DcVolts,
        raw_value: n,
        decimal_point: 1,
        prefix: Prefix::Base,
        unit: Unit::Volt,
        digits: 5,
    };
    let frame = Frame {
        info: DeviceInfo {
            category: Category::Multimeter,
            mac,
            battery: 0,
            power_source: 0,
            reading_counts: [4, 0, 0],
            reading_number: 1,
        },
        readings: [Some(reading), None, None, None],
    };
    frame.encode().to_vec()
}

/// A meter's reply to `command`, per Brymen's command table: success echoes
/// the command ID with its result arguments; failure is ID 0x8001 with the
/// failed command and an error code.
pub fn respond(
    mac: MacAddress,
    password: Password,
    model: u8,
    command: &ControlPacket,
) -> ControlPacket {
    let failure = |code| {
        let failure = Failure {
            command: command.id,
            code,
        };
        ControlPacket::failure_response(mac, failure)
    };
    let success = |args: [u8; 14]| ControlPacket {
        kind: ControlKind::Response,
        mac,
        id: command.id,
        args,
    };
    match CommandId::from_code(command.id) {
        Some(CommandId::VerifyPassword) => {
            if Password::from_args(&command.args) == Some(password) {
                success(command.args)
            } else {
                failure(3)
            }
        }
        Some(CommandId::GetModelSeriesId) => {
            let mut args = [0; 14];
            args[0] = model;
            success(args)
        }
        Some(CommandId::SetRtc) => success(command.args),
        Some(CommandId::GetFirmwareVersion) => {
            let mut args = [0; 14];
            args[..3].copy_from_slice(&[0x11, 0x01, 0x00]);
            success(args)
        }
        // Anything else is refused with "invalid arguments".
        _ => failure(5),
    }
}

/// Asserts that no error in `error`'s chain repeats its source's message.
/// An error should show its cause or return it from `source`, not both, or
/// a report that walks the chain prints the cause twice.
pub fn assert_causes_shown_once(error: &dyn std::error::Error) {
    let mut current = error;
    while let Some(source) = current.source() {
        let (shown, cause) = (current.to_string(), source.to_string());
        assert!(
            !shown.contains(&cause),
            "{shown:?} repeats its source {cause:?}"
        );
        current = source;
    }
}
