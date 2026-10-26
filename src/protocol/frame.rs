use super::{DeviceInfo, PacketKind, ProtocolError, Reading};

/// One 152-byte notification: a [`DeviceInfo`] packet followed by four
/// reading packet slots.
///
/// The BM78xBT fills only the first slot and sends the other three as all
/// zeros. An all-zero slot decodes as `None`.
///
/// Its fields are public and fixed; see [the protocol module](super#stability).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Frame {
    /// The device information packet.
    pub info: DeviceInfo,
    /// The four reading packet slots.
    pub readings: [Option<Reading>; 4],
}

impl Frame {
    /// Size of a notification frame in bytes.
    pub const LEN: usize = DeviceInfo::LEN + 4 * Reading::LEN;

    /// Decodes a notification frame.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::Length`] for a frame of any length but
    /// [`Frame::LEN`]; a short one usually means the ATT MTU is below
    /// [`REQUIRED_MTU`](super::REQUIRED_MTU). Otherwise the first error in
    /// its packets, such as [`ProtocolError::Checksum`].
    ///
    /// # Example
    ///
    /// A notification from a BM787BT on DC volts, its LCD showing `3.3021`:
    ///
    /// ```
    /// use bm78x::protocol::{Frame, Function};
    ///
    /// let mut bytes = vec![
    ///     // Device information, from the meter at 00:11:22:33:44:55.
    ///     0xff, 0x01, 0x18, 0x04, 0x01, 0x02, 0x55, 0x44,
    ///     0x33, 0x22, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00,
    ///     0x04, 0x00, 0x00, 0x01, 0x27, 0x9e, 0xff, 0x03,
    ///     // The first reading packet.
    ///     0xff, 0x02, 0x20, 0x05, 0x01, 0x00, 0x00, 0x01,
    ///     0x5f, 0xa7, 0x01, 0x00, 0x48, 0x2e, 0x10, 0x00,
    ///     0x00, 0x01, 0x03, 0x00, 0x01, 0xfd, 0x80, 0x00,
    ///     0x01, 0x00, 0x02, 0x05, 0x94, 0xa7, 0xff, 0x03,
    /// ];
    /// // The other three reading packets are empty.
    /// bytes.resize(Frame::LEN, 0);
    ///
    /// let frame = Frame::parse(&bytes)?;
    /// let reading = frame.reading().expect("the first slot is filled");
    /// assert_eq!(reading.function, Function::DcVolts);
    /// assert_eq!(reading.to_string(), "3.3021 V");
    /// # Ok::<(), bm78x::protocol::ProtocolError>(())
    /// ```
    pub fn parse(bytes: &[u8]) -> Result<Self, ProtocolError> {
        if bytes.len() != Self::LEN {
            return Err(ProtocolError::Length {
                kind: PacketKind::Frame,
                expected: Self::LEN,
                actual: bytes.len(),
            });
        }
        let (info, rest) = bytes.split_at(DeviceInfo::LEN);
        let info = DeviceInfo::parse(info)?;
        let mut readings = [None; 4];
        let (packets, _) = rest.as_chunks::<{ Reading::LEN }>();
        for (slot, packet) in readings.iter_mut().zip(packets) {
            if packet.iter().any(|&b| b != 0) {
                *slot = Some(Reading::parse(packet)?);
            }
        }
        Ok(Frame { info, readings })
    }

    /// Encodes the frame. Empty slots become all-zero packets.
    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut bytes = [0u8; Self::LEN];
        let (info, rest) = bytes.split_at_mut(DeviceInfo::LEN);
        info.copy_from_slice(&self.info.encode());
        let (packets, _) = rest.as_chunks_mut::<{ Reading::LEN }>();
        for (packet, slot) in packets.iter_mut().zip(&self.readings) {
            if let Some(reading) = slot {
                *packet = reading.encode();
            }
        }
        bytes
    }

    /// The reading in the first slot, which is the only one a BM78xBT uses.
    pub fn reading(&self) -> Option<&Reading> {
        self.readings[0].as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Category, MacAddress};

    const DCV_3_2987: [u8; 32] = [
        0xff, 0x02, 0x20, 0x05, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
        0x00, 0x00, 0x01, 0x03, 0x00, 0x01, 0xdb, 0x80, 0x00, 0x01, 0x00, 0x02, 0x05, 0x95, 0x7c,
        0xff, 0x03,
    ];

    fn sample() -> Frame {
        Frame {
            info: DeviceInfo {
                category: Category::Multimeter,
                mac: MacAddress([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]),
                battery: 0,
                power_source: 0,
                reading_counts: [4, 0, 0],
                reading_number: 1,
            },
            readings: [Some(Reading::parse(&DCV_3_2987).unwrap()), None, None, None],
        }
    }

    #[test]
    fn layout() {
        let bytes = sample().encode();
        assert_eq!(bytes.len(), 152);
        assert_eq!(bytes[24..56], DCV_3_2987);
        assert!(bytes[56..].iter().all(|&b| b == 0));
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(frame, sample());
        assert_eq!(format!("{}", frame.reading().unwrap()), "3.2987 V");
    }

    #[test]
    fn short_frame_is_a_length_error() {
        let bytes = sample().encode();
        // What a 23-byte default ATT MTU leaves of a notification.
        assert_eq!(
            Frame::parse(&bytes[..20]),
            Err(ProtocolError::Length {
                kind: PacketKind::Frame,
                expected: 152,
                actual: 20
            })
        );
        let mut long = bytes.to_vec();
        long.push(0);
        assert!(matches!(
            Frame::parse(&long),
            Err(ProtocolError::Length { actual: 153, .. })
        ));
    }

    #[test]
    fn empty_first_slot_is_none() {
        let mut frame = sample();
        frame.readings = [None; 4];
        let parsed = Frame::parse(&frame.encode()).unwrap();
        assert_eq!(parsed.reading(), None);
    }

    #[test]
    fn bad_reading_in_any_slot_is_an_error() {
        let mut bytes = sample().encode();
        bytes[24 + 3 * 32 + 5] = 1;
        assert!(matches!(
            Frame::parse(&bytes),
            Err(ProtocolError::Framing {
                kind: PacketKind::Reading,
                offset: 0,
                ..
            })
        ));
    }
}
