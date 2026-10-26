use std::fmt;
use std::str::FromStr;

/// A Bluetooth device address, stored most significant byte first, as it is
/// written (`AA:BB:CC:DD:EE:FF`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct MacAddress(pub [u8; 6]);

impl MacAddress {
    /// Converts from the byte order used inside packets: least significant
    /// byte first, the order Bluetooth itself sends addresses in.
    ///
    /// Brymen's document lists six address bytes but not their order. A
    /// BM787BT's packets carry its address reversed from how BlueZ shows it,
    /// and it accepts commands addressed that way.
    pub fn from_wire(bytes: [u8; 6]) -> Self {
        let mut b = bytes;
        b.reverse();
        MacAddress(b)
    }

    /// Converts to the byte order used inside packets. See
    /// [`MacAddress::from_wire`].
    pub fn to_wire(self) -> [u8; 6] {
        let mut b = self.0;
        b.reverse();
        b
    }
}

impl fmt::Debug for MacAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MacAddress({self})")
    }
}

impl fmt::Display for MacAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02X}:{b:02X}:{c:02X}:{d:02X}:{e:02X}:{g:02X}")
    }
}

/// The error returned when a string is not a `XX:XX:XX:XX:XX:XX` address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected a Bluetooth address like AA:BB:CC:DD:EE:FF")]
pub struct ParseMacError;

impl FromStr for MacAddress {
    type Err = ParseMacError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut bytes = [0u8; 6];
        let mut parts = s.split(':');
        for byte in &mut bytes {
            let part = parts.next().ok_or(ParseMacError)?;
            // `from_str_radix` alone would also accept a sign, as in "+A".
            if part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(ParseMacError);
            }
            *byte = u8::from_str_radix(part, 16).map_err(|_| ParseMacError)?;
        }
        if parts.next().is_some() {
            return Err(ParseMacError);
        }
        Ok(MacAddress(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_shows_the_address_as_written() {
        let mac = MacAddress([0x00, 0x11, 0x22, 0x33, 0x44, 0xAB]);
        assert_eq!(format!("{mac:?}"), "MacAddress(00:11:22:33:44:AB)");
    }

    #[test]
    fn wire_order_is_reversed() {
        let mac = MacAddress([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert_eq!(mac.to_wire(), [0x55, 0x44, 0x33, 0x22, 0x11, 0x00]);
        assert_eq!(MacAddress::from_wire(mac.to_wire()), mac);
    }

    #[test]
    fn display_and_parse() {
        let mac = MacAddress([0xAA, 0xBB, 0x0C, 0x0D, 0xEE, 0x0F]);
        let text = format!("{mac}");
        assert_eq!(text, "AA:BB:0C:0D:EE:0F");
        assert_eq!(text.parse(), Ok(mac));
        assert_eq!("aa:bb:0c:0d:ee:0f".parse(), Ok(mac));
    }

    #[test]
    fn parse_rejects_malformed() {
        for bad in [
            "",
            "AA:BB:CC:DD:EE",
            "AA:BB:CC:DD:EE:FF:00",
            "AABBCCDDEEFF",
            "A:BB:CC:DD:EE:FFF",
            "GG:BB:CC:DD:EE:FF",
            "+A:BB:CC:DD:EE:FF",
        ] {
            assert_eq!(bad.parse::<MacAddress>(), Err(ParseMacError), "{bad:?}");
        }
    }
}
