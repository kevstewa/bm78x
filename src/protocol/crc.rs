/// Computes the CRC-16/MODBUS checksum (initial value `0xFFFF`, reflected
/// polynomial `0xA001`) of `data`.
///
/// This is a direct translation of Brymen's `crc_chk` reference routine.
/// Brymen describes the polynomial as "0x8005 reverse"; `0xA001` is `0x8005`
/// with its bits mirrored, which lets the register shift right one bit at a
/// time, least significant bit first.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modbus_check_value() {
        // The catalogue "check" value for CRC-16/MODBUS.
        assert_eq!(crc16(b"123456789"), 0x4B37);
    }

    #[test]
    fn empty_input_is_initial_value() {
        assert_eq!(crc16(&[]), 0xFFFF);
    }
}
