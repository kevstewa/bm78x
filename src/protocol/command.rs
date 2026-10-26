use std::fmt;
use std::str::FromStr;

use super::{
    MacAddress, PROTOCOL_VERSION, PacketKind, ProtocolError, check_framing, expect_byte, seal,
};

/// The commands this library can send.
///
/// Brymen also defines 0x0040, "Set BLE to OTA standby mode", which puts the
/// meter's radio into firmware-update mode. It is deliberately left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CommandId {
    /// 0x0004: read the Bluetooth module's firmware version.
    GetFirmwareVersion,
    /// 0x0010: set the meter's clock.
    SetRtc,
    /// 0x0116: read the model series ID ([`MODEL_SERIES_ID`](super::MODEL_SERIES_ID)
    /// for the BM78x).
    GetModelSeriesId,
    /// 0x0140: change the connection password.
    SetPassword,
    /// 0x0141: read the connection password.
    GetPassword,
    /// 0x0142: change the advertised device name.
    SetDeviceName,
    /// 0x0143: read the advertised device name.
    GetDeviceName,
    /// 0x0151: verify the connection password. Required after every
    /// connect before the meter sends readings.
    VerifyPassword,
}

impl CommandId {
    /// The command's 16-bit ID.
    pub fn code(self) -> u16 {
        match self {
            CommandId::GetFirmwareVersion => 0x0004,
            CommandId::SetRtc => 0x0010,
            CommandId::GetModelSeriesId => 0x0116,
            CommandId::SetPassword => 0x0140,
            CommandId::GetPassword => 0x0141,
            CommandId::SetDeviceName => 0x0142,
            CommandId::GetDeviceName => 0x0143,
            CommandId::VerifyPassword => 0x0151,
        }
    }

    /// Looks up a command by its 16-bit ID.
    pub fn from_code(code: u16) -> Option<Self> {
        Some(match code {
            0x0004 => CommandId::GetFirmwareVersion,
            0x0010 => CommandId::SetRtc,
            0x0116 => CommandId::GetModelSeriesId,
            0x0140 => CommandId::SetPassword,
            0x0141 => CommandId::GetPassword,
            0x0142 => CommandId::SetDeviceName,
            0x0143 => CommandId::GetDeviceName,
            0x0151 => CommandId::VerifyPassword,
            _ => return None,
        })
    }
}

/// A command and its arguments, ready to be addressed to a meter with
/// [`Command::encode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Command {
    id: CommandId,
    args: [u8; 14],
}

impl Command {
    fn new(id: CommandId, prefix: &[u8]) -> Self {
        let mut args = [0u8; 14];
        args[..prefix.len()].copy_from_slice(prefix);
        Command { id, args }
    }

    /// Verify Connection Password (0x0151).
    pub fn verify_password(password: &Password) -> Self {
        Self::new(CommandId::VerifyPassword, &password.to_args())
    }

    /// Get BLE Firmware Version (0x0004). Decode the reply with
    /// [`FirmwareVersion::from_args`].
    pub fn get_firmware_version() -> Self {
        Self::new(CommandId::GetFirmwareVersion, &[])
    }

    /// RTC Time Calibration (0x0010).
    pub fn set_rtc(time: &RtcTime) -> Self {
        Self::new(CommandId::SetRtc, &time.to_args())
    }

    /// Get Model Series ID (0x0116). The reply's first argument is the ID.
    pub fn get_model_series_id() -> Self {
        Self::new(CommandId::GetModelSeriesId, &[])
    }

    /// Set Connection Password (0x0140).
    pub fn set_password(password: &Password) -> Self {
        Self::new(CommandId::SetPassword, &password.to_args())
    }

    /// Get Connection Password (0x0141). Decode the reply with
    /// [`Password::from_args`].
    pub fn get_password() -> Self {
        Self::new(CommandId::GetPassword, &[])
    }

    /// Set Device Name (0x0142).
    pub fn set_device_name(name: &DeviceName) -> Self {
        Self::new(CommandId::SetDeviceName, name.as_str().as_bytes())
    }

    /// Get Device Name (0x0143). Decode the reply with
    /// [`DeviceName::from_args`].
    pub fn get_device_name() -> Self {
        Self::new(CommandId::GetDeviceName, &[])
    }

    /// The command ID.
    pub fn id(&self) -> CommandId {
        self.id
    }

    /// The fourteen argument bytes, zero-padded.
    pub fn args(&self) -> &[u8; 14] {
        &self.args
    }

    /// The command as a packet addressed to the meter at `mac`.
    pub fn to_packet(&self, mac: MacAddress) -> ControlPacket {
        ControlPacket {
            kind: ControlKind::Command,
            mac,
            id: self.id.code(),
            args: self.args,
        }
    }

    /// Encodes the command as a packet addressed to the meter at `mac`.
    pub fn encode(&self, mac: MacAddress) -> [u8; ControlPacket::LEN] {
        self.to_packet(mac).encode()
    }
}

/// Whether a [`ControlPacket`] is a command or a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlKind {
    /// Sent to the meter (type 0x01).
    Command,
    /// Sent by the meter (type 0x02).
    Response,
}

/// The ID a response carries when the command failed.
const FAILURE_ID: u16 = 0x8001;

/// A decoded 32-byte command or response packet. Both directions share one
/// layout.
///
/// Its fields follow the packet, so they are public and fixed; see
/// [the protocol module](super#stability).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ControlPacket {
    /// Command or response.
    pub kind: ControlKind,
    /// The meter's Bluetooth address.
    pub mac: MacAddress,
    /// The command ID, or 0x8001 in a failure response. Kept as a number
    /// because a response can carry IDs that [`CommandId`] does not name.
    pub id: u16,
    /// The fourteen argument bytes.
    pub args: [u8; 14],
}

impl ControlPacket {
    /// Size of a command or response packet in bytes.
    pub const LEN: usize = 32;

    const HEAD1: u8 = 0x01;
    /// Byte 13. Brymen gives this single value and does not explain it.
    const PASSWORD_ID: u8 = 0x01;

    /// Decodes a command or response packet.
    ///
    /// Byte 13, the "password identification", is not checked: Brymen gives
    /// only the value to send, not what a response carries there.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Length`] if `bytes` is not [`ControlPacket::LEN`] long,
    /// [`ProtocolError::Framing`] or [`ProtocolError::PacketType`] if a
    /// fixed byte is wrong, or [`ProtocolError::Checksum`].
    pub fn parse(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let kind = PacketKind::Control;
        let p = check_framing::<{ Self::LEN }>(bytes, kind, Self::HEAD1)?;
        let control_kind = match p[3] {
            0x01 => ControlKind::Command,
            0x02 => ControlKind::Response,
            found => return Err(ProtocolError::PacketType { kind, found }),
        };
        expect_byte(p, kind, 4, PROTOCOL_VERSION)?;
        Ok(ControlPacket {
            kind: control_kind,
            mac: MacAddress::from_wire(p[5..11].try_into().unwrap()),
            id: u16::from_le_bytes([p[11], p[12]]),
            args: p[14..28].try_into().unwrap(),
        })
    }

    /// Encodes the packet.
    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut p = [0u8; Self::LEN];
        p[3] = match self.kind {
            ControlKind::Command => 0x01,
            ControlKind::Response => 0x02,
        };
        p[4] = PROTOCOL_VERSION;
        p[5..11].copy_from_slice(&self.mac.to_wire());
        p[11..13].copy_from_slice(&self.id.to_le_bytes());
        p[13] = Self::PASSWORD_ID;
        p[14..28].copy_from_slice(&self.args);
        seal(&mut p, Self::HEAD1);
        p
    }

    /// A response reporting `failure`, as the meter at `mac` would send it.
    /// Useful for simulating a meter.
    pub fn failure_response(mac: MacAddress, failure: Failure) -> Self {
        let mut args = [0u8; 14];
        args[0..2].copy_from_slice(&failure.command.to_le_bytes());
        args[2..4].copy_from_slice(&failure.code.to_le_bytes());
        ControlPacket {
            kind: ControlKind::Response,
            mac,
            id: FAILURE_ID,
            args,
        }
    }

    /// The failure this packet reports, if it is a failure response.
    pub fn failure(&self) -> Option<Failure> {
        (self.kind == ControlKind::Response && self.id == FAILURE_ID).then(|| Failure {
            command: u16::from_le_bytes([self.args[0], self.args[1]]),
            code: u16::from_le_bytes([self.args[2], self.args[3]]),
        })
    }

    /// Checks that this is a successful response to `command` and returns its
    /// arguments.
    ///
    /// # Errors
    ///
    /// [`ResponseError::Failed`] if the meter rejected the command,
    /// [`ResponseError::Mismatch`] if the response, or the failure it
    /// reports, is for another command, or [`ResponseError::NotAResponse`].
    pub fn response_to(&self, command: CommandId) -> Result<&[u8; 14], ResponseError> {
        if self.kind != ControlKind::Response {
            return Err(ResponseError::NotAResponse);
        }
        if let Some(failure) = self.failure() {
            // A failure names the command it is for; a late one for an
            // earlier command is not this command's answer.
            if failure.command != command.code() {
                return Err(ResponseError::Mismatch {
                    expected: command.code(),
                    found: failure.command,
                });
            }
            return Err(ResponseError::Failed(failure));
        }
        if self.id != command.code() {
            return Err(ResponseError::Mismatch {
                expected: command.code(),
                found: self.id,
            });
        }
        Ok(&self.args)
    }
}

/// Why a [`ControlPacket`] is not the expected successful response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ResponseError {
    /// The meter rejected the command.
    #[error(transparent)]
    Failed(Failure),
    /// The response, or the failure it reports, is for a different command.
    #[error("expected a response to command {expected:#06x}, got {found:#06x}")]
    #[non_exhaustive]
    Mismatch {
        /// The command that was sent.
        expected: u16,
        /// The command the response is for.
        found: u16,
    },
    /// The packet is a command, not a response.
    #[error("expected a response, got a command packet")]
    NotAResponse,
}

/// A failure response from the meter (ID 0x8001).
///
/// Its fields follow the packet, so they are public and fixed; see
/// [the protocol module](super#stability).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[error("meter rejected command {command:#06x}: {} (error code {code})", self.kind())]
pub struct Failure {
    /// The ID of the command that failed.
    pub command: u16,
    /// The meter's error code.
    pub code: u16,
}

impl Failure {
    /// What the error code means.
    pub fn kind(&self) -> FailureKind {
        match self.code {
            0 => FailureKind::Checksum,
            1 => FailureKind::InvalidChannel,
            2 => FailureKind::OutOfRange,
            3 | 4 => FailureKind::InvalidPassword,
            5 => FailureKind::InvalidArguments,
            6 => FailureKind::InsufficientPermissions,
            _ => FailureKind::Unknown,
        }
    }
}

/// The meaning of a [`Failure`] error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FailureKind {
    /// Code 0: the command's checksum was wrong.
    Checksum,
    /// Code 1: invalid channel ID.
    InvalidChannel,
    /// Code 2: an argument is out of range.
    OutOfRange,
    /// Codes 3 and 4: wrong password. Brymen lists both codes with the same
    /// meaning.
    InvalidPassword,
    /// Code 5: invalid arguments.
    InvalidArguments,
    /// Code 6: insufficient permissions.
    InsufficientPermissions,
    /// A code the protocol document does not list.
    Unknown,
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FailureKind::Checksum => "checksum error",
            FailureKind::InvalidChannel => "invalid channel ID",
            FailureKind::OutOfRange => "out of setting range",
            FailureKind::InvalidPassword => "invalid password",
            FailureKind::InvalidArguments => "invalid arguments",
            FailureKind::InsufficientPermissions => "insufficient permissions",
            FailureKind::Unknown => "unknown error",
        })
    }
}

/// A four-digit connection password. The default is `0000`.
///
/// Its `Debug` output hides the digits, so it can be logged; `Display` shows
/// them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Password([u8; 4]);

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(****)")
    }
}

impl Password {
    /// The factory default password, `0000`.
    pub const DEFAULT: Password = Password([0; 4]);

    /// Parses four decimal digits, such as `"0000"`.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::InvalidPassword`] if `digits` is not exactly four
    /// ASCII digits.
    pub fn new(digits: &str) -> Result<Self, ProtocolError> {
        let bytes: &[u8; 4] = digits
            .as_bytes()
            .try_into()
            .map_err(|_| ProtocolError::InvalidPassword)?;
        let mut values = [0u8; 4];
        for (value, &byte) in values.iter_mut().zip(bytes) {
            if !byte.is_ascii_digit() {
                return Err(ProtocolError::InvalidPassword);
            }
            *value = byte - b'0';
        }
        Ok(Password(values))
    }

    /// The password as command arguments: one byte per digit, holding the
    /// digit's value (`"1234"` is `01 02 03 04`).
    ///
    /// Brymen's document does not give the encoding. A BM787BT accepts
    /// `0000` sent this way and refuses a wrong password, so this is how it
    /// reads them.
    pub fn to_args(self) -> [u8; 4] {
        self.0
    }

    /// Decodes the password from a Get Connection Password reply. Returns
    /// `None` if any byte is not a digit value. See [`Password::to_args`].
    pub fn from_args(args: &[u8; 14]) -> Option<Self> {
        let digits: [u8; 4] = args[..4].try_into().unwrap();
        digits.iter().all(|&d| d <= 9).then_some(Password(digits))
    }
}

impl FromStr for Password {
    type Err = ProtocolError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Password::new(s)
    }
}

impl fmt::Display for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|d| write!(f, "{d}"))
    }
}

/// An advertised device name: 1 to 12 printable ASCII characters. The
/// default is `BM78xBT`, the name a meter has from the factory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceName {
    bytes: [u8; 12],
    len: u8,
}

impl Default for DeviceName {
    fn default() -> Self {
        DeviceName::new("BM78xBT").unwrap()
    }
}

impl DeviceName {
    /// The longest name the meter accepts.
    pub const MAX_LEN: usize = 12;

    /// Checks and stores a name.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::InvalidDeviceName`] if `name` is empty, longer than
    /// [`DeviceName::MAX_LEN`], or not printable ASCII.
    pub fn new(name: &str) -> Result<Self, ProtocolError> {
        let len = name.len();
        let printable = name.bytes().all(|b| (0x20..=0x7E).contains(&b));
        if len == 0 || len > Self::MAX_LEN || !printable {
            return Err(ProtocolError::InvalidDeviceName);
        }
        let mut bytes = [0u8; 12];
        bytes[..len].copy_from_slice(name.as_bytes());
        Ok(DeviceName {
            bytes,
            len: len as u8,
        })
    }

    /// Decodes the name from a Get Device Name reply: arguments 0 to 11, up
    /// to the first zero byte.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::InvalidDeviceName`] if the reply holds no valid name.
    pub fn from_args(args: &[u8; 14]) -> Result<Self, ProtocolError> {
        let field = &args[..Self::MAX_LEN];
        let len = field.iter().position(|&b| b == 0).unwrap_or(Self::MAX_LEN);
        let name =
            std::str::from_utf8(&field[..len]).map_err(|_| ProtocolError::InvalidDeviceName)?;
        Self::new(name)
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        // `new` admits only ASCII, so this cannot fail.
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap()
    }
}

impl FromStr for DeviceName {
    type Err = ProtocolError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        DeviceName::new(s)
    }
}

impl fmt::Display for DeviceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The Bluetooth module's firmware version, from a Get BLE Firmware Version
/// reply.
///
/// Its fields are public and fixed; see [the protocol module](super#stability).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FirmwareVersion {
    /// Major version.
    pub major: u8,
    /// Minor version.
    pub minor: u8,
    /// Patch version.
    pub patch: u8,
}

impl FirmwareVersion {
    /// Decodes the reply's arguments.
    ///
    /// Inferred: Brymen writes `Arg[2:0] = [0x00, 0x01, 0x11]` for version
    /// 0.1.17. Its `[high:low]` notation elsewhere (such as `Arg[1:0]:
    /// 0x0004` for a little-endian ID) suggests the list runs from argument 2
    /// down to argument 0, so argument 2 is the major version. A BM787BT
    /// replies `05 00 01`, read here as 1.0.5; whether its maker would call
    /// that 1.0.5 or 5.0.1 is unconfirmed.
    pub fn from_args(args: &[u8; 14]) -> Self {
        FirmwareVersion {
            major: args[2],
            minor: args[1],
            patch: args[0],
        }
    }
}

impl fmt::Display for FirmwareVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A day of the week, for [`RtcTime`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Weekday {
    /// Monday.
    Monday,
    /// Tuesday.
    Tuesday,
    /// Wednesday.
    Wednesday,
    /// Thursday.
    Thursday,
    /// Friday.
    Friday,
    /// Saturday.
    Saturday,
    /// Sunday.
    Sunday,
}

impl Weekday {
    /// The day as the meter's clock command carries it, 1 to 7.
    ///
    /// Inferred: Monday is 1 and Sunday is 7, as in ISO 8601. Brymen gives
    /// only the range, and the meter neither reports the weekday back nor
    /// displays it.
    fn number(self) -> u8 {
        match self {
            Weekday::Monday => 1,
            Weekday::Tuesday => 2,
            Weekday::Wednesday => 3,
            Weekday::Thursday => 4,
            Weekday::Friday => 5,
            Weekday::Saturday => 6,
            Weekday::Sunday => 7,
        }
    }
}

/// A time to set the meter's clock to with [`Command::set_rtc`], checked to
/// be one the meter accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RtcTime {
    year: u16,
    month: u8,
    day: u8,
    weekday: Weekday,
    hour: u8,
    minute: u8,
    second: u8,
}

impl RtcTime {
    /// A time with the given fields.
    ///
    /// The weekday is not checked against the date: the meter takes both as
    /// given.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::RtcOutOfRange`] if a field is outside the range the
    /// meter accepts: years 2000 to 2099, months 1 to 12, days 1 to 31,
    /// hours 0 to 23, minutes and seconds 0 to 59.
    pub fn new(
        year: u16,
        month: u8,
        day: u8,
        weekday: Weekday,
        hour: u8,
        minute: u8,
        second: u8,
    ) -> Result<Self, ProtocolError> {
        let check = |field, value: u16, range: std::ops::RangeInclusive<u16>| {
            if range.contains(&value) {
                Ok(())
            } else {
                Err(ProtocolError::RtcOutOfRange { field, value })
            }
        };
        check("year", year, 2000..=2099)?;
        check("month", month.into(), 1..=12)?;
        check("day", day.into(), 1..=31)?;
        check("hour", hour.into(), 0..=23)?;
        check("minute", minute.into(), 0..=59)?;
        check("second", second.into(), 0..=59)?;
        Ok(RtcTime {
            year,
            month,
            day,
            weekday,
            hour,
            minute,
            second,
        })
    }

    /// The year, 2000 to 2099.
    pub fn year(&self) -> u16 {
        self.year
    }

    /// The month, 1 to 12.
    pub fn month(&self) -> u8 {
        self.month
    }

    /// The day of the month, 1 to 31.
    pub fn day(&self) -> u8 {
        self.day
    }

    /// The day of the week.
    pub fn weekday(&self) -> Weekday {
        self.weekday
    }

    /// The hour, 0 to 23.
    pub fn hour(&self) -> u8 {
        self.hour
    }

    /// The minute, 0 to 59.
    pub fn minute(&self) -> u8 {
        self.minute
    }

    /// The second, 0 to 59.
    pub fn second(&self) -> u8 {
        self.second
    }

    /// The command arguments: second, minute, hour, day, weekday, month and
    /// year minus 2000.
    pub fn to_args(self) -> [u8; 7] {
        [
            self.second,
            self.minute,
            self.hour,
            self.day,
            self.weekday.number(),
            self.month,
            // `new` admits only 2000 to 2099.
            (self.year - 2000) as u8,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::crc16;

    const MAC: MacAddress = MacAddress([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);

    /// Checks the fixed parts of a command packet and returns its arguments.
    fn check_command(p: &[u8; 32], id: u16) -> [u8; 14] {
        assert_eq!(&p[..5], &[0xFF, 0x01, 0x20, 0x01, 0x01]);
        assert_eq!(&p[5..11], &[0x55, 0x44, 0x33, 0x22, 0x11, 0x00]);
        assert_eq!(u16::from_le_bytes([p[11], p[12]]), id);
        assert_eq!(p[13], 0x01);
        assert_eq!(u16::from_le_bytes([p[28], p[29]]), crc16(&p[2..28]));
        assert_eq!(&p[30..], &[0xFF, 0x03]);
        p[14..28].try_into().unwrap()
    }

    #[test]
    fn verify_password_packet() {
        let password = Password::new("1234").unwrap();
        let p = Command::verify_password(&password).encode(MAC);
        #[rustfmt::skip]
        let expected_head = [
            0xFF, 0x01, 0x20, 0x01, 0x01,
            0x55, 0x44, 0x33, 0x22, 0x11, 0x00,
            0x51, 0x01,
            0x01,
            0x01, 0x02, 0x03, 0x04,
        ];
        assert_eq!(&p[..18], &expected_head);
        assert!(p[18..28].iter().all(|&b| b == 0));
        check_command(&p, 0x0151);

        let default = Command::verify_password(&Password::DEFAULT).encode(MAC);
        assert_eq!(check_command(&default, 0x0151), [0; 14]);
    }

    #[test]
    fn set_rtc_packet() {
        // 2026-10-23 was a Friday.
        let time = RtcTime::new(2026, 10, 23, Weekday::Friday, 21, 47, 59).unwrap();
        let p = Command::set_rtc(&time).encode(MAC);
        let args = check_command(&p, 0x0010);
        assert_eq!(args, [59, 47, 21, 23, 5, 10, 26, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn weekdays_count_from_monday() {
        let days = [
            Weekday::Monday,
            Weekday::Tuesday,
            Weekday::Wednesday,
            Weekday::Thursday,
            Weekday::Friday,
            Weekday::Saturday,
            Weekday::Sunday,
        ];
        for (n, day) in (1..).zip(days) {
            let time = RtcTime::new(2026, 10, 23, day, 0, 0, 0).unwrap();
            assert_eq!(time.weekday(), day);
            assert_eq!(time.to_args()[4], n, "{day:?}");
        }
    }

    #[test]
    fn rtc_times_are_checked_when_made() {
        let new = |year, month, day, hour, minute, second| {
            RtcTime::new(year, month, day, Weekday::Sunday, hour, minute, second)
        };
        let late = new(2099, 12, 31, 23, 59, 59).unwrap();
        assert_eq!((late.year(), late.month(), late.day()), (2099, 12, 31));
        assert_eq!((late.hour(), late.minute(), late.second()), (23, 59, 59));
        assert_eq!(late.to_args()[6], 99);
        let early = new(2000, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(early.to_args(), [0, 0, 0, 1, 7, 1, 0]);
        let bad = [
            (new(1999, 12, 31, 23, 59, 59), "year", 1999),
            (new(2100, 12, 31, 23, 59, 59), "year", 2100),
            (new(2099, 0, 31, 23, 59, 59), "month", 0),
            (new(2099, 13, 31, 23, 59, 59), "month", 13),
            (new(2099, 12, 0, 23, 59, 59), "day", 0),
            (new(2099, 12, 32, 23, 59, 59), "day", 32),
            (new(2099, 12, 31, 24, 59, 59), "hour", 24),
            (new(2099, 12, 31, 23, 60, 59), "minute", 60),
            (new(2099, 12, 31, 23, 59, 60), "second", 60),
        ];
        for (time, field, value) in bad {
            assert_eq!(time, Err(ProtocolError::RtcOutOfRange { field, value }));
        }
    }

    #[test]
    fn other_commands() {
        let cases = [
            (Command::get_firmware_version(), 0x0004),
            (Command::get_model_series_id(), 0x0116),
            (Command::get_password(), 0x0141),
            (Command::get_device_name(), 0x0143),
        ];
        for (command, id) in cases {
            assert_eq!(check_command(&command.encode(MAC), id), [0; 14]);
        }

        let set = Command::set_password(&Password::new("9081").unwrap());
        assert_eq!(&check_command(&set.encode(MAC), 0x0140)[..4], &[9, 0, 8, 1]);

        let name = DeviceName::new("Bench meter").unwrap();
        let args = check_command(&Command::set_device_name(&name).encode(MAC), 0x0142);
        assert_eq!(&args[..12], b"Bench meter\0");
    }

    #[test]
    fn command_ids_round_trip() {
        for code in 0..=u16::MAX {
            if let Some(id) = CommandId::from_code(code) {
                assert_eq!(id.code(), code);
            }
        }
        // The OTA command must stay unrepresentable.
        assert_eq!(CommandId::from_code(0x0040), None);
    }

    fn response(id: u16, args: [u8; 14]) -> ControlPacket {
        let p = ControlPacket {
            kind: ControlKind::Response,
            mac: MAC,
            id,
            args,
        };
        ControlPacket::parse(&p.encode()).unwrap()
    }

    #[test]
    fn successful_response() {
        let mut args = [0u8; 14];
        args[..3].copy_from_slice(&[0x11, 0x01, 0x00]);
        let r = response(0x0004, args);
        let args = r.response_to(CommandId::GetFirmwareVersion).unwrap();
        let version = FirmwareVersion::from_args(args);
        assert_eq!(format!("{version}"), "0.1.17");
        assert_eq!(r.failure(), None);
        assert_eq!(
            r.response_to(CommandId::VerifyPassword),
            Err(ResponseError::Mismatch {
                expected: 0x0151,
                found: 0x0004
            })
        );
    }

    #[test]
    fn failure_response() {
        // A wrong password: failed command 0x0151, error code 3.
        let mut args = [0u8; 14];
        args[..4].copy_from_slice(&[0x51, 0x01, 0x03, 0x00]);
        let r = response(0x8001, args);
        let failure = Failure {
            command: 0x0151,
            code: 3,
        };
        assert_eq!(r, ControlPacket::failure_response(MAC, failure));
        assert_eq!(r.failure(), Some(failure));
        assert_eq!(failure.kind(), FailureKind::InvalidPassword);
        assert_eq!(
            r.response_to(CommandId::VerifyPassword),
            Err(ResponseError::Failed(failure))
        );
        assert_eq!(
            format!("{failure}"),
            "meter rejected command 0x0151: invalid password (error code 3)"
        );
    }

    #[test]
    fn a_failure_for_another_command_is_a_mismatch() {
        // A late failure for an earlier command must not be taken as the
        // answer to this one.
        let failure = Failure {
            command: 0x0010,
            code: 2,
        };
        let r =
            ControlPacket::parse(&ControlPacket::failure_response(MAC, failure).encode()).unwrap();
        assert_eq!(
            r.response_to(CommandId::VerifyPassword),
            Err(ResponseError::Mismatch {
                expected: 0x0151,
                found: 0x0010
            })
        );
        assert!(matches!(
            r.response_to(CommandId::SetRtc),
            Err(ResponseError::Failed(Failure {
                command: 0x0010,
                code: 2
            }))
        ));
    }

    #[test]
    fn firmware_versions() {
        let version = |arg: [u8; 3]| {
            let mut args = [0u8; 14];
            args[..3].copy_from_slice(&arg);
            FirmwareVersion::from_args(&args).to_string()
        };
        // Brymen's two examples, written Arg[2:0].
        assert_eq!(version([0x11, 0x01, 0x00]), "0.1.17");
        assert_eq!(version([0x14, 0x02, 0x01]), "1.2.20");
        // A BM787BT's reply, Arg[0] first: 05 00 01.
        assert_eq!(version([0x05, 0x00, 0x01]), "1.0.5");
    }

    #[test]
    fn debug_does_not_show_the_password() {
        let debug = format!("{:?}", Password::new("1234").unwrap());
        assert!(
            !debug.contains("1234") && !debug.contains("1, 2, 3, 4"),
            "{debug}"
        );
        assert!(debug.starts_with("Password"));
    }

    #[test]
    fn every_failure_code() {
        let kinds = [
            FailureKind::Checksum,
            FailureKind::InvalidChannel,
            FailureKind::OutOfRange,
            FailureKind::InvalidPassword,
            FailureKind::InvalidPassword,
            FailureKind::InvalidArguments,
            FailureKind::InsufficientPermissions,
            FailureKind::Unknown,
        ];
        for (code, kind) in kinds.into_iter().enumerate() {
            assert_eq!(
                Failure {
                    command: 0,
                    code: code as u16
                }
                .kind(),
                kind
            );
        }
    }

    #[test]
    fn command_packet_is_not_a_response() {
        let p = ControlPacket::parse(&Command::get_password().encode(MAC)).unwrap();
        assert_eq!(p.kind, ControlKind::Command);
        assert_eq!(
            p.response_to(CommandId::GetPassword),
            Err(ResponseError::NotAResponse)
        );
        // A command with the failure ID is not a failure response.
        let mut p = p;
        p.id = 0x8001;
        assert_eq!(p.failure(), None);
    }

    #[test]
    fn rejects_bad_type_and_version() {
        let kind = PacketKind::Control;
        let mut p = Command::get_password().encode(MAC);
        p[3] = 0x03;
        seal(&mut p, ControlPacket::HEAD1);
        assert_eq!(
            ControlPacket::parse(&p),
            Err(ProtocolError::PacketType { kind, found: 0x03 })
        );

        let mut p = Command::get_password().encode(MAC);
        p[4] = 0x00;
        seal(&mut p, ControlPacket::HEAD1);
        assert_eq!(
            ControlPacket::parse(&p),
            Err(ProtocolError::Framing {
                kind,
                offset: 4,
                found: 0x00,
                expected: 0x01
            })
        );
    }

    #[test]
    fn passwords() {
        assert_eq!(Password::new("0000"), Ok(Password::DEFAULT));
        assert_eq!(format!("{}", Password::new("0457").unwrap()), "0457");
        for bad in ["", "000", "00000", "12a4", "١٢٣٤", " 123"] {
            assert_eq!(
                Password::new(bad),
                Err(ProtocolError::InvalidPassword),
                "{bad:?}"
            );
        }
        let mut args = [0u8; 14];
        args[..4].copy_from_slice(&[4, 3, 2, 1]);
        assert_eq!(Password::from_args(&args), Password::new("4321").ok());
        args[0] = 10;
        assert_eq!(Password::from_args(&args), None);
    }

    #[test]
    fn device_names() {
        assert_eq!(DeviceName::new("BM78xBT").unwrap().as_str(), "BM78xBT");
        // The name a meter has from the factory.
        assert_eq!(DeviceName::default().as_str(), "BM78xBT");
        assert_eq!(DeviceName::new("123456789012").unwrap().as_str().len(), 12);
        for bad in ["", "1234567890123", "tab\there", "µmeter"] {
            assert_eq!(
                DeviceName::new(bad),
                Err(ProtocolError::InvalidDeviceName),
                "{bad:?}"
            );
        }
        let mut args = [0u8; 14];
        args[..7].copy_from_slice(b"BM78xBT");
        assert_eq!(DeviceName::from_args(&args).unwrap().as_str(), "BM78xBT");
        args[..12].copy_from_slice(b"ABCDEFGHIJKL");
        args[12] = b'M';
        assert_eq!(
            DeviceName::from_args(&args).unwrap().as_str(),
            "ABCDEFGHIJKL"
        );
        assert_eq!(
            DeviceName::from_args(&[0; 14]),
            Err(ProtocolError::InvalidDeviceName)
        );
    }
}
