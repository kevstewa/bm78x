//! The BM78xBT wire protocol: packet layouts, checksums and decoded types.
//!
//! The meter speaks in three kinds of packet, all framed the same way: a
//! two-byte head (`FF xx`), a length byte, a type byte, a payload, a
//! CRC-16/MODBUS over everything from the length byte to the end of the
//! payload, and a two-byte trailer (`FF 03`). Brymen does not give the CRC's
//! byte order; a BM787BT stores it little-endian.
//!
//! - A [`Frame`] (152 bytes) arrives on every display update. It holds one
//!   [`DeviceInfo`] packet and four [`Reading`] packets, of which the BM78xBT
//!   fills only the first.
//! - A [`ControlPacket`] (32 bytes) carries a [`Command`] to the meter or its
//!   response back.
//!
//! # Stability
//!
//! The decoded packets ([`Frame`], [`DeviceInfo`], [`Reading`],
//! [`ControlPacket`], [`Failure`]) and the values read from them
//! ([`MeterTime`], [`Number`], [`FirmwareVersion`]) have public fields that
//! follow revision 4 of Brymen's document, and can be built and matched
//! field by field. Their fields will not change within a major version of
//! this crate. A protocol revision that gave meaning to a reserved byte
//! would come with a new major version.

mod advert;
mod command;
mod crc;
mod frame;
mod function;
mod info;
mod mac;
mod reading;
mod units;

pub use advert::{COMPANY_ID, MODEL_SERIES_ID, is_meter_manufacturer_data};
pub use command::{
    Command, CommandId, ControlKind, ControlPacket, DeviceName, Failure, FailureKind,
    FirmwareVersion, Password, ResponseError, RtcTime, Weekday,
};
pub use crc::crc16;
pub use frame::Frame;
pub use function::Function;
pub use info::{Category, DeviceInfo};
pub use mac::{MacAddress, ParseMacError};
pub use reading::{DashCount, Flags, MeterTime, Number, Reading, Readout, Text};
pub use units::{Prefix, Unit};

use std::fmt;

/// UUID of the meter's primary GATT service.
pub const SERVICE_UUID: &str = "0003cdd0-0000-1000-8000-00805f9b0131";
/// UUID of the characteristic that notifies a [`Frame`] on each display update.
pub const NOTIFY_UUID: &str = "0003cdd5-0000-1000-8000-00805f9b0131";
/// UUID of the read/write characteristic that carries [`ControlPacket`]s.
pub const COMMAND_UUID: &str = "0003cdd4-0000-1000-8000-00805f9b0131";

/// The ATT MTU Brymen says the central must use. One notification carries a
/// whole 152-byte [`Frame`], which does not fit the 23-byte default.
pub const REQUIRED_MTU: u16 = 185;

/// The only protocol version Brymen has defined.
const PROTOCOL_VERSION: u8 = 0x01;

/// A code that Brymen's protocol document does not list, held by the
/// `Unknown` variant of [`Unit`], [`Prefix`], [`Function`], [`Category`] and
/// [`Text`].
///
/// Only decoding makes one, so it never holds a listed code: an unknown
/// value never stands for a known one, and comparing values compares what
/// they mean.
///
/// ```compile_fail
/// use bm78x::protocol::{Unit, Unlisted};
/// let volt = Unit::Unknown(Unlisted(0x02));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Unlisted<T>(T);

impl<T: Copy> Unlisted<T> {
    /// The code as sent.
    pub fn code(self) -> T {
        self.0
    }
}

/// Which kind of packet a [`ProtocolError`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PacketKind {
    /// A whole 152-byte notification.
    Frame,
    /// The 24-byte device information packet at the start of a frame.
    DeviceInfo,
    /// A 32-byte reading packet.
    Reading,
    /// A 32-byte command or response packet.
    Control,
}

impl fmt::Display for PacketKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PacketKind::Frame => "notification frame",
            PacketKind::DeviceInfo => "device information packet",
            PacketKind::Reading => "reading packet",
            PacketKind::Control => "command/response packet",
        })
    }
}

/// Why bytes could not be decoded, or a value could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProtocolError {
    /// The packet is the wrong size. For a [`Frame`] this usually means the
    /// ATT MTU is too small and the notification was cut short.
    #[error("{kind} must be {expected} bytes, got {actual}")]
    #[non_exhaustive]
    Length {
        /// The packet being decoded.
        kind: PacketKind,
        /// Its fixed size.
        expected: usize,
        /// The number of bytes received.
        actual: usize,
    },
    /// A fixed byte (head, length, type, version or trailer) is wrong.
    #[error("{kind} byte {offset} is {found:#04x}, expected {expected:#04x}")]
    #[non_exhaustive]
    Framing {
        /// The packet being decoded.
        kind: PacketKind,
        /// Index of the byte within the packet.
        offset: usize,
        /// The value received.
        found: u8,
        /// The value the protocol requires.
        expected: u8,
    },
    /// The packet type byte is not one this decoder accepts.
    #[error("{kind} has unknown packet type {found:#04x}")]
    #[non_exhaustive]
    PacketType {
        /// The packet being decoded.
        kind: PacketKind,
        /// The type byte received.
        found: u8,
    },
    /// The stored checksum does not match the data.
    #[error("{kind} checksum mismatch: stored {stored:#06x}, computed {computed:#06x}")]
    #[non_exhaustive]
    Checksum {
        /// The packet being decoded.
        kind: PacketKind,
        /// The CRC carried in the packet.
        stored: u16,
        /// The CRC of the bytes received.
        computed: u16,
    },
    /// A password is not exactly four decimal digits.
    #[error("password must be exactly 4 decimal digits")]
    InvalidPassword,
    /// A device name is not 1 to 12 printable ASCII characters.
    #[error("device name must be 1 to 12 printable ASCII characters")]
    InvalidDeviceName,
    /// An [`RtcTime`] field is outside the range the meter accepts.
    #[error("RTC {field} {value} is out of range")]
    #[non_exhaustive]
    RtcOutOfRange {
        /// The field's name.
        field: &'static str,
        /// The rejected value.
        value: u16,
    },
}

/// Checks the framing shared by every packet kind: size, head, length byte,
/// trailer and checksum. The type byte (offset 3) is left to the caller,
/// since its valid values depend on the packet kind.
fn check_framing<const N: usize>(
    bytes: &[u8],
    kind: PacketKind,
    head1: u8,
) -> Result<&[u8; N], ProtocolError> {
    let packet: &[u8; N] = bytes.try_into().map_err(|_| ProtocolError::Length {
        kind,
        expected: N,
        actual: bytes.len(),
    })?;

    // N is 24 or 32, so it always fits the length byte.
    let fixed = [
        (0, 0xFF),
        (1, head1),
        (2, N as u8),
        (N - 2, 0xFF),
        (N - 1, 0x03),
    ];
    for (offset, expected) in fixed {
        expect_byte(packet, kind, offset, expected)?;
    }

    let stored = u16::from_le_bytes([packet[N - 4], packet[N - 3]]);
    let computed = crc16(&packet[2..N - 4]);
    if stored != computed {
        return Err(ProtocolError::Checksum {
            kind,
            stored,
            computed,
        });
    }
    Ok(packet)
}

/// Fails with [`ProtocolError::Framing`] unless `packet[offset] == expected`.
fn expect_byte(
    packet: &[u8],
    kind: PacketKind,
    offset: usize,
    expected: u8,
) -> Result<(), ProtocolError> {
    let found = packet[offset];
    if found == expected {
        Ok(())
    } else {
        Err(ProtocolError::Framing {
            kind,
            offset,
            found,
            expected,
        })
    }
}

/// Writes the head, length byte, checksum and trailer around a payload that
/// the caller has already placed in `packet[3..N - 4]`.
fn seal<const N: usize>(packet: &mut [u8; N], head1: u8) {
    packet[0] = 0xFF;
    packet[1] = head1;
    packet[2] = N as u8;
    let crc = crc16(&packet[2..N - 4]);
    packet[N - 4..N - 2].copy_from_slice(&crc.to_le_bytes());
    packet[N - 2] = 0xFF;
    packet[N - 1] = 0x03;
}

/// Reads a 24-bit little-endian unsigned field.
fn u24_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0])
}
