use super::{
    MacAddress, PROTOCOL_VERSION, PacketKind, ProtocolError, Unlisted, check_framing, expect_byte,
    seal,
};

/// A decoded 24-byte device information packet, the first part of every
/// [`Frame`](super::Frame).
///
/// The fields hold every byte Brymen defines; the reserved bytes 14 and 15
/// are dropped.
///
/// Its fields follow the packet, so they are public and fixed; see
/// [the protocol module](super#stability).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceInfo {
    /// What kind of meter this is.
    pub category: Category,
    /// The meter's Bluetooth address.
    pub mac: MacAddress,
    /// Battery status byte (12). See [`DeviceInfo::battery_low`].
    pub battery: u8,
    /// Power source flag (byte 13). Brymen gives only the value 0.
    pub power_source: u8,
    /// Counts of reading packets that follow (bytes 16..=18). The BM78xBT
    /// sends `[4, 0, 0]`.
    pub reading_counts: [u8; 3],
    /// Reading packet number (byte 19). 1 on the BM78xBT, a single-display
    /// meter.
    pub reading_number: u8,
}

impl DeviceInfo {
    /// Size of a device information packet in bytes.
    pub const LEN: usize = 24;

    const HEAD1: u8 = 0x01;
    const TYPE: u8 = 0x04;

    /// Decodes a device information packet.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Length`] if `bytes` is not [`DeviceInfo::LEN`] long,
    /// [`ProtocolError::Framing`] or [`ProtocolError::PacketType`] if a
    /// fixed byte is wrong, or [`ProtocolError::Checksum`].
    pub fn parse(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let kind = PacketKind::DeviceInfo;
        let p = check_framing::<{ Self::LEN }>(bytes, kind, Self::HEAD1)?;
        if p[3] != Self::TYPE {
            return Err(ProtocolError::PacketType { kind, found: p[3] });
        }
        expect_byte(p, kind, 4, PROTOCOL_VERSION)?;
        Ok(DeviceInfo {
            category: Category::from_code(p[5]),
            mac: MacAddress::from_wire(p[6..12].try_into().unwrap()),
            battery: p[12],
            power_source: p[13],
            reading_counts: [p[16], p[17], p[18]],
            reading_number: p[19],
        })
    }

    /// Encodes the device information as a packet.
    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut p = [0u8; Self::LEN];
        p[3] = Self::TYPE;
        p[4] = PROTOCOL_VERSION;
        p[5] = self.category.code();
        p[6..12].copy_from_slice(&self.mac.to_wire());
        p[12] = self.battery;
        p[13] = self.power_source;
        p[16..19].copy_from_slice(&self.reading_counts);
        p[19] = self.reading_number;
        seal(&mut p, Self::HEAD1);
        p
    }

    /// Whether the meter reports a low battery (battery byte 0x02).
    pub fn battery_low(&self) -> bool {
        self.battery == 0x02
    }
}

/// The kind of meter (byte 5 of the device information packet).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Category {
    /// A multimeter, such as the BM787BT.
    Multimeter,
    /// A clamp meter.
    Clamp,
    /// A code the protocol document does not list.
    Unknown(Unlisted<u8>),
}

impl Category {
    /// Decodes a category byte.
    pub fn from_code(code: u8) -> Self {
        match code {
            0x02 => Category::Multimeter,
            0x03 => Category::Clamp,
            other => Category::Unknown(Unlisted(other)),
        }
    }

    /// The category byte.
    pub fn code(self) -> u8 {
        match self {
            Category::Multimeter => 0x02,
            Category::Clamp => 0x03,
            Category::Unknown(code) => code.code(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DeviceInfo {
        DeviceInfo {
            category: Category::Multimeter,
            mac: MacAddress([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]),
            battery: 0,
            power_source: 0,
            reading_counts: [4, 0, 0],
            reading_number: 1,
        }
    }

    #[test]
    fn encodes_brymen_layout() {
        let p = sample().encode();
        assert_eq!(&p[..6], &[0xFF, 0x01, 0x18, 0x04, 0x01, 0x02]);
        assert_eq!(&p[6..12], &[0x55, 0x44, 0x33, 0x22, 0x11, 0x00]);
        assert_eq!(&p[12..20], &[0, 0, 0, 0, 4, 0, 0, 1]);
        assert_eq!(
            u16::from_le_bytes([p[20], p[21]]),
            super::super::crc16(&p[2..20])
        );
        assert_eq!(&p[22..], &[0xFF, 0x03]);
        assert_eq!(DeviceInfo::parse(&p), Ok(sample()));
    }

    #[test]
    fn battery_and_category() {
        let mut info = sample();
        assert!(!info.battery_low());
        info.battery = 0x02;
        info.category = Category::Clamp;
        let parsed = DeviceInfo::parse(&info.encode()).unwrap();
        assert!(parsed.battery_low());
        assert_eq!(parsed.category, Category::Clamp);
        assert!(matches!(Category::from_code(0x09), Category::Unknown(u) if u.code() == 0x09));
        for code in 0..=u8::MAX {
            let category = Category::from_code(code);
            assert_eq!(category.code(), code);
            let listed = matches!(code, 0x02 | 0x03);
            assert_eq!(matches!(category, Category::Unknown(_)), !listed);
        }
    }

    #[test]
    fn rejects_bad_version_and_type() {
        let kind = PacketKind::DeviceInfo;

        let mut version = sample().encode();
        version[4] = 0x02;
        seal(&mut version, DeviceInfo::HEAD1);
        assert_eq!(
            DeviceInfo::parse(&version),
            Err(ProtocolError::Framing {
                kind,
                offset: 4,
                found: 0x02,
                expected: 0x01
            })
        );

        let mut packet_type = sample().encode();
        packet_type[3] = 0x05;
        seal(&mut packet_type, DeviceInfo::HEAD1);
        assert_eq!(
            DeviceInfo::parse(&packet_type),
            Err(ProtocolError::PacketType { kind, found: 0x05 })
        );
    }
}
