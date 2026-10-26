use std::fmt;

use super::Unlisted;

/// The unit of a reading (byte 26 of a reading packet).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Unit {
    /// Volt.
    Volt,
    /// Ampere.
    Ampere,
    /// Ohm.
    Ohm,
    /// Siemens.
    Siemens,
    /// Farad.
    Farad,
    /// Hertz.
    Hertz,
    /// Percent (duty cycle).
    Percent,
    /// Degree Celsius.
    Celsius,
    /// Degree Fahrenheit.
    Fahrenheit,
    /// Percent of a 4-20 mA current loop.
    LoopPercent,
    /// A code the protocol document does not list.
    Unknown(Unlisted<u8>),
}

impl Unit {
    /// Decodes a unit byte.
    pub fn from_code(code: u8) -> Self {
        match code {
            0x02 => Unit::Volt,
            0x03 => Unit::Ampere,
            0x04 => Unit::Ohm,
            0x05 => Unit::Siemens,
            0x06 => Unit::Farad,
            0x08 => Unit::Hertz,
            0x0A => Unit::Percent,
            0x14 => Unit::Celsius,
            0x15 => Unit::Fahrenheit,
            0x4F => Unit::LoopPercent,
            other => Unit::Unknown(Unlisted(other)),
        }
    }

    /// The unit byte for this unit.
    pub fn code(self) -> u8 {
        match self {
            Unit::Volt => 0x02,
            Unit::Ampere => 0x03,
            Unit::Ohm => 0x04,
            Unit::Siemens => 0x05,
            Unit::Farad => 0x06,
            Unit::Hertz => 0x08,
            Unit::Percent => 0x0A,
            Unit::Celsius => 0x14,
            Unit::Fahrenheit => 0x15,
            Unit::LoopPercent => 0x4F,
            Unit::Unknown(code) => code.code(),
        }
    }

    /// The unit's symbol, or `None` for [`Unit::Unknown`].
    pub fn symbol(self) -> Option<&'static str> {
        Some(match self {
            Unit::Volt => "V",
            Unit::Ampere => "A",
            Unit::Ohm => "Ω",
            Unit::Siemens => "S",
            Unit::Farad => "F",
            Unit::Hertz => "Hz",
            Unit::Percent => "%",
            Unit::Celsius => "°C",
            Unit::Fahrenheit => "°F",
            Unit::LoopPercent => "%4~20mA",
            Unit::Unknown(_) => return None,
        })
    }
}

impl fmt::Display for Unit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.symbol() {
            Some(symbol) => f.write_str(symbol),
            None => write!(f, "unit({:#04x})", self.code()),
        }
    }
}

/// The metric prefix of a reading (byte 25 of a reading packet, a signed
/// power of ten).
///
/// The reading's digits are already in prefixed units: `12.345` with
/// [`Prefix::Milli`] and [`Unit::Ampere`] means 12.345 mA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Prefix {
    /// n, 10⁻⁹.
    Nano,
    /// μ, 10⁻⁶.
    Micro,
    /// m, 10⁻³.
    Milli,
    /// No prefix.
    Base,
    /// k, 10³.
    Kilo,
    /// M, 10⁶.
    Mega,
    /// G, 10⁹.
    Giga,
    /// An exponent the protocol document does not list.
    Unknown(Unlisted<i8>),
}

impl Prefix {
    /// Decodes a prefix byte, which holds the power of ten.
    pub fn from_exponent(exponent: i8) -> Self {
        match exponent {
            -9 => Prefix::Nano,
            -6 => Prefix::Micro,
            -3 => Prefix::Milli,
            0 => Prefix::Base,
            3 => Prefix::Kilo,
            6 => Prefix::Mega,
            9 => Prefix::Giga,
            other => Prefix::Unknown(Unlisted(other)),
        }
    }

    /// The power of ten, which is also the prefix byte.
    pub fn exponent(self) -> i8 {
        match self {
            Prefix::Nano => -9,
            Prefix::Micro => -6,
            Prefix::Milli => -3,
            Prefix::Base => 0,
            Prefix::Kilo => 3,
            Prefix::Mega => 6,
            Prefix::Giga => 9,
            Prefix::Unknown(exponent) => exponent.code(),
        }
    }

    /// The factor that converts prefixed units to base units, or `None` for
    /// [`Prefix::Unknown`].
    pub fn multiplier(self) -> Option<f64> {
        Some(match self {
            Prefix::Nano => 1e-9,
            Prefix::Micro => 1e-6,
            Prefix::Milli => 1e-3,
            Prefix::Base => 1.0,
            Prefix::Kilo => 1e3,
            Prefix::Mega => 1e6,
            Prefix::Giga => 1e9,
            Prefix::Unknown(_) => return None,
        })
    }

    /// The prefix's symbol (empty for [`Prefix::Base`]), or `None` for
    /// [`Prefix::Unknown`]. Micro is U+03BC, as in Brymen's document.
    pub fn symbol(self) -> Option<&'static str> {
        Some(match self {
            Prefix::Nano => "n",
            Prefix::Micro => "μ",
            Prefix::Milli => "m",
            Prefix::Base => "",
            Prefix::Kilo => "k",
            Prefix::Mega => "M",
            Prefix::Giga => "G",
            Prefix::Unknown(_) => return None,
        })
    }
}

impl fmt::Display for Prefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.symbol() {
            Some(symbol) => f.write_str(symbol),
            None => write!(f, "10^{}·", self.exponent()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNITS: [(u8, Unit, &str); 10] = [
        (0x02, Unit::Volt, "V"),
        (0x03, Unit::Ampere, "A"),
        (0x04, Unit::Ohm, "Ω"),
        (0x05, Unit::Siemens, "S"),
        (0x06, Unit::Farad, "F"),
        (0x08, Unit::Hertz, "Hz"),
        (0x0A, Unit::Percent, "%"),
        (0x14, Unit::Celsius, "°C"),
        (0x15, Unit::Fahrenheit, "°F"),
        (0x4F, Unit::LoopPercent, "%4~20mA"),
    ];

    #[test]
    fn every_unit_code() {
        for (code, unit, symbol) in UNITS {
            assert_eq!(Unit::from_code(code), unit);
            assert_eq!(unit.code(), code);
            assert_eq!(unit.symbol(), Some(symbol));
        }
    }

    #[test]
    fn unknown_unit_round_trips() {
        for code in 0..=u8::MAX {
            let unit = Unit::from_code(code);
            assert_eq!(unit.code(), code);
            // Only a code missing from Brymen's table is unknown, so an
            // unknown unit never stands for a known one.
            let listed = UNITS.iter().any(|&(c, ..)| c == code);
            assert_eq!(matches!(unit, Unit::Unknown(_)), !listed, "{code:#04x}");
        }
        let unknown = Unit::from_code(0x07);
        assert!(matches!(unknown, Unit::Unknown(u) if u.code() == 0x07));
        assert_eq!(unknown.symbol(), None);
        assert_eq!(format!("{unknown}"), "unit(0x07)");
    }

    const PREFIXES: [(i8, Prefix, &str, f64); 7] = [
        (-9, Prefix::Nano, "n", 1e-9),
        (-6, Prefix::Micro, "\u{3bc}", 1e-6),
        (-3, Prefix::Milli, "m", 1e-3),
        (0, Prefix::Base, "", 1.0),
        (3, Prefix::Kilo, "k", 1e3),
        (6, Prefix::Mega, "M", 1e6),
        (9, Prefix::Giga, "G", 1e9),
    ];

    #[test]
    fn every_prefix() {
        for (exponent, prefix, symbol, multiplier) in PREFIXES {
            assert_eq!(Prefix::from_exponent(exponent), prefix);
            assert_eq!(prefix.exponent(), exponent);
            assert_eq!(prefix.symbol(), Some(symbol));
            assert_eq!(prefix.multiplier(), Some(multiplier));
        }
    }

    #[test]
    fn unknown_prefix_round_trips() {
        for exponent in i8::MIN..=i8::MAX {
            let prefix = Prefix::from_exponent(exponent);
            assert_eq!(prefix.exponent(), exponent);
            let listed = PREFIXES.iter().any(|&(e, ..)| e == exponent);
            assert_eq!(matches!(prefix, Prefix::Unknown(_)), !listed, "{exponent}");
        }
        let unknown = Prefix::from_exponent(2);
        assert!(matches!(unknown, Prefix::Unknown(u) if u.code() == 2));
        assert_eq!(unknown.multiplier(), None);
    }
}
