use std::fmt;

use super::{
    Function, PacketKind, Prefix, ProtocolError, Unit, Unlisted, check_framing, seal, u24_le,
};

/// A decoded 32-byte device reading packet: one update of the meter's display.
///
/// The fields hold every byte Brymen defines, so [`Reading::encode`] can
/// rebuild the packet. Only the reserved byte 19 and the top five bits of
/// byte 11, which no field uses, are dropped. [`Reading::readout`] and
/// [`Reading::si_value`] interpret the fields.
///
/// Its fields follow the packet, so they are public and fixed; see
/// [the protocol module](super#stability).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Reading {
    /// Logging data set ID (bytes 4..=6, 24 bits). Always 1 on the BM78xBT.
    pub logging_set_id: u32,
    /// Reading packet ID (byte 7). Always 1 on the BM78xBT.
    pub packet_id: u8,
    /// The meter's clock when it took the reading.
    pub time: MeterTime,
    /// Status flags (bytes 14..=16).
    pub flags: Flags,
    /// Device type (byte 17): 0 for a sensor, 1 for a meter.
    pub device_type: u8,
    /// The selected function.
    pub function: Function,
    /// The 24-bit reading field (bytes 21..=23), sign-extended. Use
    /// [`Reading::readout`] rather than this: the displayed sign comes from
    /// [`Flags::negative`], and some displays are text or overload.
    pub raw_value: i32,
    /// Decimal point position (byte 24). See [`Reading::decimals`].
    pub decimal_point: u8,
    /// Metric prefix of the displayed value.
    pub prefix: Prefix,
    /// Unit of the displayed value.
    pub unit: Unit,
    /// Number of digits on the display (byte 27): 3, 4, 5 or 6.
    pub digits: u8,
}

impl Reading {
    /// Size of a reading packet in bytes.
    pub const LEN: usize = 32;

    const HEAD1: u8 = 0x02;
    const TYPE: u8 = 0x05;

    /// Decodes a reading packet.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Length`] if `bytes` is not [`Reading::LEN`] long,
    /// [`ProtocolError::Framing`] or [`ProtocolError::PacketType`] if a
    /// fixed byte is wrong, or [`ProtocolError::Checksum`].
    pub fn parse(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let kind = PacketKind::Reading;
        let p = check_framing::<{ Self::LEN }>(bytes, kind, Self::HEAD1)?;
        if p[3] != Self::TYPE {
            return Err(ProtocolError::PacketType { kind, found: p[3] });
        }

        // Brymen calls bytes 21..=23 "signed bytes" with byte 23 most
        // significant (its example: FF 80 00 is -32768), so this reads them
        // as a signed little-endian 24-bit number, which a BM787BT's frames
        // bear out. Shifting left then arithmetic-shifting right copies
        // bit 23 into the top byte.
        let raw_value = ((u24_le(&p[21..24]) << 8) as i32) >> 8;

        Ok(Reading {
            logging_set_id: u24_le(&p[4..7]),
            packet_id: p[7],
            time: MeterTime::decode(p[8..14].try_into().unwrap()),
            flags: Flags {
                status0: p[14],
                status1: p[15],
                status2: p[16],
            },
            device_type: p[17],
            function: Function::from_ids(p[18], p[20]),
            raw_value,
            decimal_point: p[24],
            prefix: Prefix::from_exponent(p[25] as i8),
            unit: Unit::from_code(p[26]),
            digits: p[27],
        })
    }

    /// Encodes the reading as a packet. Fields wider than their wire format
    /// (such as `logging_set_id` above 24 bits) are truncated.
    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut p = [0u8; Self::LEN];
        p[3] = Self::TYPE;
        p[4..7].copy_from_slice(&self.logging_set_id.to_le_bytes()[..3]);
        p[7] = self.packet_id;
        p[8..14].copy_from_slice(&self.time.encode());
        p[14] = self.flags.status0;
        p[15] = self.flags.status1;
        p[16] = self.flags.status2;
        p[17] = self.device_type;
        (p[18], p[20]) = self.function.ids();
        p[21..24].copy_from_slice(&self.raw_value.to_le_bytes()[..3]);
        p[24] = self.decimal_point;
        p[25] = self.prefix.exponent() as u8;
        p[26] = self.unit.code();
        p[27] = self.digits;
        seal(&mut p, Self::HEAD1);
        p
    }

    /// The number of digits after the decimal point.
    ///
    /// With `digits` digits and decimal point position `dp`, there are
    /// `digits - dp` decimals, or none when `dp` is 0: five digits with `dp`
    /// 1 is `X.XXXX`, and four digits with `dp` 3 is `XXX.X`. Brymen's table
    /// covers four and five digits; this applies the same rule to three and
    /// six, and caps the result at 6, the widest display Brymen lists.
    pub fn decimals(&self) -> u8 {
        if self.decimal_point == 0 {
            0
        } else {
            self.digits.saturating_sub(self.decimal_point).min(6)
        }
    }

    /// What the display shows.
    ///
    /// Overload takes precedence: Brymen says the reading bytes "can be
    /// ignored" when the OL flag is set, and does not say what happens if the
    /// ASCII flag is set too.
    ///
    /// A temperature overload is `----`: a BM787BT with no thermocouple sets
    /// the OL flag, and its LCD shows four dashes. What it shows for a
    /// thermocouple beyond its range is unconfirmed.
    pub fn readout(&self) -> Readout {
        if self.flags.overload() {
            if matches!(self.unit, Unit::Celsius | Unit::Fahrenheit) {
                Readout::Text(Text::Dashes(DashCount(4)))
            } else {
                Readout::Overload
            }
        } else if self.flags.ascii() {
            Readout::Text(Text::from_code(self.raw_value as u32 & 0x00FF_FFFF))
        } else {
            Readout::Number(Number {
                // Brymen calls the field signed and also has a sign flag.
                // The flag is what the display shows, so it alone sets the
                // sign, and the field gives only the digits. A field that is
                // itself negative is then not negated twice.
                magnitude: self.raw_value.unsigned_abs(),
                negative: self.flags.negative(),
                decimals: self.decimals(),
            })
        }
    }

    /// The reading in base units (volts rather than millivolts), or `None`
    /// if the display is not a number or the prefix is unknown.
    pub fn si_value(&self) -> Option<f64> {
        match self.readout() {
            Readout::Number(number) => Some(number.value() * self.prefix.multiplier()?),
            _ => None,
        }
    }
}

impl fmt::Display for Reading {
    /// Formats the display as the meter shows it, such as `3.2987 V`,
    /// `OL mV` or `Auto`. See [`Reading::readout`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.readout() {
            Readout::Number(number) => write!(f, "{number} {}{}", self.prefix, self.unit),
            Readout::Overload => write!(f, "OL {}{}", self.prefix, self.unit),
            Readout::Text(text) => write!(f, "{text}"),
        }
    }
}

/// What the meter's display shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Readout {
    /// A numeric reading.
    Number(Number),
    /// `OL`: the input is beyond the range.
    Overload,
    /// A text display such as `Auto` or `----`.
    Text(Text),
}

/// A numeric reading as displayed, in prefixed units.
///
/// The sign is kept separate from the magnitude so that a sign flag on a zero
/// reading is not lost: a BM787BT shows `-0.0000` on DC volts with the leads
/// shorted.
///
/// Its fields are public and fixed; see [the protocol module](super#stability).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Number {
    /// The displayed digits without the decimal point, e.g. 32987 for 3.2987.
    pub magnitude: u32,
    /// Whether the display shows a minus sign.
    pub negative: bool,
    /// Digits after the decimal point (0 to 6).
    pub decimals: u8,
}

impl Number {
    /// The value as a float, e.g. 3.2987 (still in prefixed units).
    pub fn value(&self) -> f64 {
        let magnitude = f64::from(self.magnitude) / f64::from(self.scale());
        if self.negative { -magnitude } else { magnitude }
    }

    fn scale(&self) -> u32 {
        10u32.pow(u32::from(self.decimals.min(6)))
    }
}

impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.negative { "-" } else { "" };
        let scale = self.scale();
        let (whole, fraction) = (self.magnitude / scale, self.magnitude % scale);
        match self.decimals.min(6) {
            0 => write!(f, "{sign}{whole}"),
            width => write!(
                f,
                "{sign}{whole}.{fraction:0width$}",
                width = usize::from(width)
            ),
        }
    }
}

/// A text display, shown when [`Flags::ascii`] is set. The reading field
/// selects the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Text {
    /// `Auto` (code 1).
    Auto,
    /// `InEr`, input error (code 2).
    InputError,
    /// One to five dashes (codes 3 to 7).
    Dashes(DashCount),
    /// `EF-H`, strong electric field (code 0x0A).
    EfHigh,
    /// `EF-L`, weak electric field (code 0x0B).
    EfLow,
    /// A code the protocol document does not list.
    Unknown(Unlisted<u32>),
}

/// How many dashes a [`Text::Dashes`] display shows: 1 to 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DashCount(u8);

impl DashCount {
    /// The number of dashes.
    pub fn get(self) -> u8 {
        self.0
    }
}

impl Text {
    /// Decodes the reading field of a text display.
    pub fn from_code(code: u32) -> Self {
        match code {
            0x01 => Text::Auto,
            0x02 => Text::InputError,
            0x03..=0x07 => Text::Dashes(DashCount(code as u8 - 2)),
            0x0A => Text::EfHigh,
            0x0B => Text::EfLow,
            other => Text::Unknown(Unlisted(other)),
        }
    }

    /// The reading field value for this text.
    pub fn code(self) -> u32 {
        match self {
            Text::Auto => 0x01,
            Text::InputError => 0x02,
            Text::Dashes(n) => u32::from(n.get()) + 2,
            Text::EfHigh => 0x0A,
            Text::EfLow => 0x0B,
            Text::Unknown(code) => code.code(),
        }
    }
}

impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Text::Auto => f.write_str("Auto"),
            Text::InputError => f.write_str("InEr"),
            Text::Dashes(n) => (0..n.get()).try_for_each(|_| f.write_str("-")),
            Text::EfHigh => f.write_str("EF-H"),
            Text::EfLow => f.write_str("EF-L"),
            Text::Unknown(code) => write!(f, "text({:#08x})", code.code()),
        }
    }
}

/// The three status flag bytes of a reading packet.
///
/// Status flag 2 has no defined bits; it is kept so a packet can be rebuilt
/// exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Flags {
    /// Status flag 0 (byte 14).
    pub status0: u8,
    /// Status flag 1 (byte 15).
    pub status1: u8,
    /// Status flag 2 (byte 16).
    pub status2: u8,
}

impl Flags {
    /// CREST (peak) mode.
    pub fn crest(&self) -> bool {
        self.status0 & 0x80 != 0
    }
    /// REL (relative) mode.
    pub fn relative(&self) -> bool {
        self.status0 & 0x40 != 0
    }
    /// HOLD: the display is frozen.
    pub fn hold(&self) -> bool {
        self.status0 & 0x20 != 0
    }
    /// Auto-ranging (otherwise manual range).
    pub fn auto_range(&self) -> bool {
        self.status0 & 0x10 != 0
    }
    /// AUTO-HOLD mode.
    pub fn auto_hold(&self) -> bool {
        self.status0 & 0x08 != 0
    }
    /// The display is text, not a number. See [`Text`].
    pub fn ascii(&self) -> bool {
        self.status0 & 0x04 != 0
    }
    /// The display shows a minus sign.
    pub fn negative(&self) -> bool {
        self.status1 & 0x40 != 0
    }
    /// The display shows `OL`.
    pub fn overload(&self) -> bool {
        self.status1 & 0x20 != 0
    }
    /// RECORD (MAX/MIN/AVG recording) mode.
    pub fn record(&self) -> bool {
        self.status1 & 0x10 != 0
    }
    /// The `MAX` annunciator is on.
    pub fn max(&self) -> bool {
        self.status1 & 0x08 != 0
    }
    /// The `MIN` annunciator is on.
    pub fn min(&self) -> bool {
        self.status1 & 0x04 != 0
    }
    /// The `AVG` annunciator is on.
    pub fn avg(&self) -> bool {
        self.status1 & 0x02 != 0
    }
}

/// The meter's clock, from bytes 8..=13 of a reading packet.
///
/// The meter has no clock battery, so this is only meaningful after the
/// clock has been set with [`Command::set_rtc`](super::Command::set_rtc).
/// No range checks are made; the fields hold whatever the meter sent, and
/// format as they are.
///
/// Its fields follow the packet, so they are public and fixed; see
/// [the protocol module](super#stability).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct MeterTime {
    /// Year, 2000 to 2127.
    pub year: u16,
    /// Month, 1 to 12.
    pub month: u8,
    /// Day of the month, 1 to 31.
    pub day: u8,
    /// Hour, 0 to 23.
    pub hour: u8,
    /// Minute, 0 to 59.
    pub minute: u8,
    /// Second, 0 to 59.
    pub second: u8,
    /// Millisecond, 0 to 999. The field has room for up to 1023.
    pub millisecond: u16,
}

impl MeterTime {
    /// Decodes bytes 8..=13 of a reading packet.
    ///
    /// Bytes 13 and 12, read as a big-endian `u16`, hold the year since 2000
    /// (bits 15..=9), month (8..=5) and day (4..=0). Bytes 11 to 8, read as a
    /// big-endian `u32`, hold the hour (bits 26..=22), minute (21..=16),
    /// second (15..=10) and millisecond (9..=0).
    pub fn decode(bytes: [u8; 6]) -> Self {
        let date = u16::from_be_bytes([bytes[5], bytes[4]]);
        let time = u32::from_be_bytes([bytes[3], bytes[2], bytes[1], bytes[0]]);
        MeterTime {
            year: 2000 + (date >> 9),
            month: ((date >> 5) & 0x0F) as u8,
            day: (date & 0x1F) as u8,
            hour: ((time >> 22) & 0x1F) as u8,
            minute: ((time >> 16) & 0x3F) as u8,
            second: ((time >> 10) & 0x3F) as u8,
            millisecond: (time & 0x3FF) as u16,
        }
    }

    /// Encodes the time as bytes 8..=13 of a reading packet. Each field is
    /// truncated to its bit width.
    pub fn encode(&self) -> [u8; 6] {
        let date = (self.year.wrapping_sub(2000) & 0x7F) << 9
            | u16::from(self.month & 0x0F) << 5
            | u16::from(self.day & 0x1F);
        let time = u32::from(self.hour & 0x1F) << 22
            | u32::from(self.minute & 0x3F) << 16
            | u32::from(self.second & 0x3F) << 10
            | u32::from(self.millisecond & 0x3FF);
        let [d1, d0] = date.to_be_bytes();
        let [t3, t2, t1, t0] = time.to_be_bytes();
        [t0, t1, t2, t3, d0, d1]
    }
}

impl fmt::Display for MeterTime {
    /// Formats as `YYYY-MM-DD HH:MM:SS.mmm`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            self.year, self.month, self.day, self.hour, self.minute, self.second, self.millisecond
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "3.2987 V DC, auto-ranging", built by hand from Brymen's tables.
    const DCV_3_2987: [u8; 32] = [
        0xff, 0x02, 0x20, 0x05, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
        0x00, 0x00, 0x01, 0x03, 0x00, 0x01, 0xdb, 0x80, 0x00, 0x01, 0x00, 0x02, 0x05, 0x95, 0x7c,
        0xff, 0x03,
    ];

    fn sample() -> Reading {
        Reading::parse(&DCV_3_2987).unwrap()
    }

    /// The sample packet with status flags 0 and 1 (bytes 14 and 15) and the
    /// reading field (bytes 21..=23, as sent) replaced, and the CRC redone.
    /// This patches bytes at Brymen's offsets rather than calling
    /// `Reading::encode`, so the decoder is not checked against itself.
    fn patched(status0: u8, status1: u8, reading: [u8; 3]) -> Reading {
        let mut p = DCV_3_2987;
        p[14] = status0;
        p[15] = status1;
        p[21..24].copy_from_slice(&reading);
        let crc = super::super::crc16(&p[2..28]);
        p[28..30].copy_from_slice(&crc.to_le_bytes());
        Reading::parse(&p).unwrap()
    }

    #[test]
    fn known_packet() {
        let r = sample();
        assert_eq!(r.function, Function::DcVolts);
        assert_eq!(r.unit, Unit::Volt);
        assert_eq!(r.prefix, Prefix::Base);
        assert_eq!(r.raw_value, 32987);
        assert_eq!(r.digits, 5);
        assert_eq!(r.decimals(), 4);
        assert!(r.flags.auto_range());
        assert_eq!(r.si_value(), Some(3.2987));
        assert_eq!(format!("{r}"), "3.2987 V");
        assert_eq!(r.logging_set_id, 1);
        assert_eq!(r.packet_id, 1);
        assert_eq!(r.device_type, 1);
        assert_eq!(r.encode(), DCV_3_2987);
    }

    #[test]
    fn decimal_point_table() {
        // (digits, dp, decimals). Brymen's table covers 5 and 4 digits.
        let table = [
            (5, 0, 0),
            (5, 1, 4),
            (5, 2, 3),
            (5, 3, 2),
            (5, 4, 1),
            (4, 0, 0),
            (4, 1, 3),
            (4, 2, 2),
            (4, 3, 1),
            // Not in Brymen's table: the same rule applied to 6 and 3
            // digits, and this crate's choice for a dp beyond the digits.
            (6, 1, 5),
            (3, 1, 2),
            (4, 5, 0),
        ];
        for (digits, dp, decimals) in table {
            let mut r = sample();
            r.digits = digits;
            r.decimal_point = dp;
            assert_eq!(r.decimals(), decimals, "digits {digits} dp {dp}");
        }
    }

    #[test]
    fn number_formatting() {
        let n = |magnitude, negative, decimals| {
            format!(
                "{}",
                Number {
                    magnitude,
                    negative,
                    decimals
                }
            )
        };
        assert_eq!(n(32987, false, 4), "3.2987");
        assert_eq!(n(12, false, 4), "0.0012");
        assert_eq!(n(0, true, 4), "-0.0000");
        assert_eq!(n(60000, true, 0), "-60000");
        assert_eq!(n(59999, false, 1), "5999.9");
    }

    #[test]
    fn sign_comes_from_the_flag_only() {
        // 1234 is D2 04 00 on the wire; -1234 (0xFFFB2E) is 2E FB FF.
        let plus = [0xD2, 0x04, 0x00];
        let minus = [0x2E, 0xFB, 0xFF];
        // Positive field, no sign flag.
        assert_eq!(patched(0, 0, plus).si_value(), Some(0.1234));
        // Positive field, sign flag.
        assert_eq!(patched(0, 0x40, plus).si_value(), Some(-0.1234));
        // Negative field with the sign flag: still negative, not doubly so.
        assert_eq!(patched(0, 0x40, minus).si_value(), Some(-0.1234));
        // Negative field without the sign flag: the flag wins.
        assert_eq!(patched(0, 0, minus).si_value(), Some(0.1234));
    }

    #[test]
    fn negative_raw_value_sign_extends() {
        // Brymen's example: FF 80 00 (byte 23 first) is -32768.
        let mut packet = DCV_3_2987;
        packet[21..24].copy_from_slice(&[0x00, 0x80, 0xFF]);
        let crc = super::super::crc16(&packet[2..28]);
        packet[28..30].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(Reading::parse(&packet).unwrap().raw_value, -32768);
    }

    #[test]
    fn overload() {
        let r = patched(0x10, 0x20, [0x56, 0x34, 0x12]);
        assert_eq!(r.readout(), Readout::Overload);
        assert_eq!(r.si_value(), None);
        assert_eq!(format!("{r}"), "OL V");
        // OL wins over the ASCII flag.
        assert_eq!(
            patched(0x04, 0x20, [0x01, 0, 0]).readout(),
            Readout::Overload
        );
    }

    #[test]
    fn every_text_display() {
        let table = [
            (0x01, Text::Auto, "Auto"),
            (0x02, Text::InputError, "InEr"),
            (0x03, Text::Dashes(DashCount(1)), "-"),
            (0x04, Text::Dashes(DashCount(2)), "--"),
            (0x05, Text::Dashes(DashCount(3)), "---"),
            (0x06, Text::Dashes(DashCount(4)), "----"),
            (0x07, Text::Dashes(DashCount(5)), "-----"),
            (0x0A, Text::EfHigh, "EF-H"),
            (0x0B, Text::EfLow, "EF-L"),
        ];
        for (code, text, shown) in table {
            let r = patched(0x04, 0, [code, 0, 0]);
            assert_eq!(r.readout(), Readout::Text(text));
            assert_eq!(text.code(), u32::from(code));
            assert_eq!(format!("{r}"), shown);
            assert_eq!(r.si_value(), None);
        }
        let unknown = |reading| match patched(0x04, 0, reading).readout() {
            Readout::Text(Text::Unknown(code)) => Some(code.code()),
            _ => None,
        };
        assert_eq!(unknown([0x08, 0, 0]), Some(8));
        // A field with bit 23 set is read unsigned here, unlike a number.
        assert_eq!(unknown([0xFF, 0xFF, 0xFF]), Some(0xFF_FFFF));
        // Bytes 22 and 23 are the high bytes, so this is not code 1.
        assert_eq!(unknown([0x00, 0x00, 0x01]), Some(0x01_0000));
    }

    #[test]
    fn only_unlisted_text_codes_are_unknown() {
        for code in 0..=0x1FF {
            let text = Text::from_code(code);
            assert_eq!(text.code(), code);
            let listed = matches!(code, 0x01..=0x07 | 0x0A | 0x0B);
            assert_eq!(matches!(text, Text::Unknown(_)), !listed, "{code:#x}");
        }
        for (code, count) in (0x03..=0x07).zip(1..=5) {
            assert!(matches!(Text::from_code(code), Text::Dashes(n) if n.get() == count));
        }
    }

    #[test]
    fn a_thermocouple_overload_shows_dashes() {
        // A BM787BT with no thermocouple sets OL and sends the top of the
        // type-K range, 1372 °C or 2502 °F, but its LCD shows `----`. See
        // `displays` in tests/captures.rs.
        for (unit, top) in [(0x14u8, 1372u32), (0x15, 2502)] {
            let mut p = DCV_3_2987;
            (p[18], p[20]) = (0x0C, 0x00); // T1
            p[15] = 0x20; // OL
            p[21..24].copy_from_slice(&top.to_le_bytes()[..3]);
            (p[24], p[26], p[27]) = (0, unit, 4);
            let crc = super::super::crc16(&p[2..28]);
            p[28..30].copy_from_slice(&crc.to_le_bytes());
            let r = Reading::parse(&p).unwrap();
            assert!(matches!(r.readout(), Readout::Text(Text::Dashes(n)) if n.get() == 4));
            assert_eq!(r.to_string(), "----");
            assert_eq!(r.si_value(), None);
        }
    }

    #[test]
    fn every_flag_bit() {
        type Getter = fn(&Flags) -> bool;
        let bits: [(u8, u8, Getter); 12] = [
            (0x80, 0, Flags::crest),
            (0x40, 0, Flags::relative),
            (0x20, 0, Flags::hold),
            (0x10, 0, Flags::auto_range),
            (0x08, 0, Flags::auto_hold),
            (0x04, 0, Flags::ascii),
            (0, 0x40, Flags::negative),
            (0, 0x20, Flags::overload),
            (0, 0x10, Flags::record),
            (0, 0x08, Flags::max),
            (0, 0x04, Flags::min),
            (0, 0x02, Flags::avg),
        ];
        for (i, &(s0, s1, _)) in bits.iter().enumerate() {
            let flags = Flags {
                status0: s0,
                status1: s1,
                status2: 0,
            };
            for (j, &(_, _, other)) in bits.iter().enumerate() {
                assert_eq!(other(&flags), i == j, "bit {i} read by getter {j}");
            }
        }
        // The don't-care bits set nothing.
        let flags = Flags {
            status0: 0x03,
            status1: 0x81,
            status2: 0xFF,
        };
        assert!(bits.iter().all(|&(_, _, getter)| !getter(&flags)));
    }

    #[test]
    fn clock_fields() {
        let time = MeterTime {
            year: 2026,
            month: 10,
            day: 23,
            hour: 21,
            minute: 47,
            second: 59,
            millisecond: 789,
        };
        let bytes = time.encode();
        // Worked by hand from the bit layout.
        // date = 26 << 9 | 10 << 5 | 23 = 0x3557; time = 21 << 22 | 47 << 16
        //      | 59 << 10 | 789 = 0x056F_EF15.
        assert_eq!(bytes, [0x15, 0xEF, 0x6F, 0x05, 0x57, 0x35]);
        assert_eq!(MeterTime::decode(bytes), time);
        assert_eq!(format!("{time}"), "2026-10-23 21:47:59.789");
    }

    #[test]
    fn clock_fields_at_their_maximum() {
        // Every bit of every field set, and nothing else.
        let bytes = [0xFF, 0xFF, 0xFF, 0x07, 0xFF, 0xFF];
        let time = MeterTime::decode(bytes);
        assert_eq!(
            (
                time.year,
                time.month,
                time.day,
                time.hour,
                time.minute,
                time.second
            ),
            (2127, 15, 31, 31, 63, 63)
        );
        // The field holds up to 1023 ms, beyond Brymen's 999; it is kept as
        // sent, so the packet can be rebuilt.
        assert_eq!(time.millisecond, 1023);
        assert_eq!(time.encode(), bytes);
        // The top five bits of byte 11 are not part of any field.
        assert_eq!(
            MeterTime::decode([0, 0, 0, 0xF8, 0, 0]),
            MeterTime::decode([0; 6])
        );
    }

    #[test]
    fn bad_packets() {
        let error = |packet: &[u8]| Reading::parse(packet).unwrap_err();
        let kind = PacketKind::Reading;

        assert_eq!(
            error(&DCV_3_2987[..31]),
            ProtocolError::Length {
                kind,
                expected: 32,
                actual: 31
            }
        );
        assert_eq!(
            error(&[0; 32]),
            ProtocolError::Framing {
                kind,
                offset: 0,
                found: 0,
                expected: 0xFF
            }
        );

        let mut head = DCV_3_2987;
        head[1] = 0x01;
        assert_eq!(
            error(&head),
            ProtocolError::Framing {
                kind,
                offset: 1,
                found: 0x01,
                expected: 0x02
            }
        );

        let mut length = DCV_3_2987;
        length[2] = 0x18;
        assert_eq!(
            error(&length),
            ProtocolError::Framing {
                kind,
                offset: 2,
                found: 0x18,
                expected: 0x20
            }
        );

        let mut trailer = DCV_3_2987;
        trailer[31] = 0x04;
        assert_eq!(
            error(&trailer),
            ProtocolError::Framing {
                kind,
                offset: 31,
                found: 0x04,
                expected: 0x03
            }
        );

        let mut crc = DCV_3_2987;
        crc[21] ^= 1;
        assert_eq!(
            error(&crc),
            ProtocolError::Checksum {
                kind,
                stored: 0x7C95,
                computed: super::super::crc16(&crc[2..28])
            }
        );

        let mut packet_type = sample().encode();
        packet_type[3] = 0x04;
        seal(&mut packet_type, Reading::HEAD1);
        assert_eq!(
            error(&packet_type),
            ProtocolError::PacketType { kind, found: 0x04 }
        );
    }

    #[test]
    fn prefixed_si_value() {
        let mut r = sample();
        r.prefix = Prefix::Milli;
        assert_eq!(r.si_value(), Some(3.2987 * 1e-3));
        assert_eq!(format!("{r}"), "3.2987 mV");
        r.prefix = Prefix::from_exponent(2);
        assert_eq!(r.si_value(), None);
    }
}
