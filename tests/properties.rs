//! Property tests: decoding arbitrary bytes never panics, and decoding keeps
//! every byte the protocol defines.

use bm78x::protocol::{ControlPacket, DeviceInfo, Frame, Reading, crc16};
use proptest::prelude::*;

/// Builds a packet of `N` bytes from a payload for bytes 3..N-4, with the head,
/// length byte, CRC and trailer as Brymen's tables give them.
fn framed<const N: usize>(head1: u8, payload: &[u8]) -> [u8; N] {
    let mut p = [0u8; N];
    p[0] = 0xFF;
    p[1] = head1;
    p[2] = N as u8;
    p[3..N - 4].copy_from_slice(payload);
    let crc = crc16(&p[2..N - 4]);
    p[N - 4..N - 2].copy_from_slice(&crc.to_le_bytes());
    p[N - 2] = 0xFF;
    p[N - 1] = 0x03;
    p
}

proptest! {
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
        let _ = Frame::parse(&bytes);
        let _ = Reading::parse(&bytes);
        let _ = DeviceInfo::parse(&bytes);
        let _ = ControlPacket::parse(&bytes);
    }

    /// Any well-framed reading packet decodes, displays without panicking,
    /// and re-encodes to the same bytes, apart from the bits the protocol
    /// does not define.
    #[test]
    fn reading_keeps_every_defined_byte(mut payload in any::<[u8; 25]>()) {
        // Payload index i is packet byte i + 3.
        payload[0] = 0x05; // type
        payload[16] = 0; // byte 19, reserved
        payload[8] &= 0x07; // byte 11: the top five bits are not in any field

        let packet = framed::<32>(0x02, &payload);
        let reading = Reading::parse(&packet).unwrap();
        let _ = reading.to_string();
        let _ = reading.si_value();
        prop_assert_eq!(reading.encode(), packet);
    }

    /// Packets with correct framing, checksum and type but any contents
    /// decode and display without panicking, alone and as a frame.
    #[test]
    fn well_framed_packets_with_any_contents_never_panic(
        mut info in any::<[u8; 17]>(),
        slots in any::<[[u8; 25]; 4]>(),
        response in any::<bool>(),
    ) {
        (info[0], info[1]) = (0x04, 0x01);
        let mut frame = Vec::from(framed::<24>(0x01, &info));
        for mut payload in slots {
            payload[0] = 0x05;
            let packet = framed::<32>(0x02, &payload);
            let reading = Reading::parse(&packet).unwrap();
            let _ = (reading.to_string(), reading.time.to_string(), reading.si_value());
            frame.extend(packet);

            (payload[0], payload[1]) = (if response { 0x02 } else { 0x01 }, 0x01);
            let control = ControlPacket::parse(&framed::<32>(0x01, &payload)).unwrap();
            let _ = control.failure().map(|f| f.to_string());
        }
        let _ = Frame::parse(&frame).map(|f| f.reading().map(|r| r.to_string()));
    }

    #[test]
    fn info_keeps_every_defined_byte(mut payload in any::<[u8; 17]>()) {
        payload[0] = 0x04; // type
        payload[1] = 0x01; // protocol version
        payload[11] = 0; // byte 14, reserved
        payload[12] = 0; // byte 15, reserved
        let packet = framed::<24>(0x01, &payload);
        prop_assert_eq!(DeviceInfo::parse(&packet).unwrap().encode(), packet);
    }

    #[test]
    fn control_keeps_every_defined_byte(mut payload in any::<[u8; 25]>(), response in any::<bool>()) {
        payload[0] = if response { 0x02 } else { 0x01 };
        payload[1] = 0x01; // protocol version
        payload[10] = 0x01; // byte 13, password identification
        let packet = framed::<32>(0x01, &payload);
        prop_assert_eq!(ControlPacket::parse(&packet).unwrap().encode(), packet);
    }

    /// A frame decodes when its info packet is valid and each reading slot is
    /// either all zeros or a valid reading.
    #[test]
    fn frame_keeps_every_defined_byte(
        info in any::<[u8; 17]>(),
        slots in any::<[Option<[u8; 25]>; 4]>(),
    ) {
        let mut info = info;
        info[0] = 0x04;
        info[1] = 0x01;
        info[11] = 0;
        info[12] = 0;
        let mut bytes = Vec::from(framed::<24>(0x01, &info));
        for slot in slots {
            match slot {
                Some(mut payload) => {
                    payload[0] = 0x05;
                    payload[16] = 0;
                    payload[8] &= 0x07;
                    bytes.extend(framed::<32>(0x02, &payload));
                }
                None => bytes.extend([0u8; 32]),
            }
        }
        let frame = Frame::parse(&bytes).unwrap();
        for (decoded, slot) in frame.readings.iter().zip(&slots) {
            prop_assert_eq!(decoded.is_some(), slot.is_some());
        }
        prop_assert_eq!(Vec::from(frame.encode()), bytes);
    }

    /// Flipping any single bit of a valid packet is always detected.
    #[test]
    fn single_bit_errors_are_rejected(payload in any::<[u8; 25]>(), bit in 0usize..32 * 8) {
        let mut payload = payload;
        payload[0] = 0x05;
        let mut packet = framed::<32>(0x02, &payload);
        packet[bit / 8] ^= 1 << (bit % 8);
        prop_assert!(Reading::parse(&packet).is_err());
    }
}
