/// Brymen's Bluetooth SIG company identifier, the key of the meter's
/// manufacturer-specific advertising data.
pub const COMPANY_ID: u16 = 0x0131;

/// The model series ID of the BM78x family, in advertisements and in the
/// reply to [`CommandId::GetModelSeriesId`](super::CommandId::GetModelSeriesId).
pub const MODEL_SERIES_ID: u8 = 0x0B;

/// Returns whether manufacturer-specific advertising data comes from a
/// BM78xBT: company [`COMPANY_ID`] and a payload starting `B`, `M`,
/// [`MODEL_SERIES_ID`].
///
/// `data` is the payload after the company ID, as BlueZ reports it. Match on
/// this or on [`SERVICE_UUID`](super::SERVICE_UUID), not on the device name,
/// which the user can change.
pub fn is_meter_manufacturer_data(company_id: u16, data: &[u8]) -> bool {
    company_id == COMPANY_ID && data.starts_with(&[b'B', b'M', MODEL_SERIES_ID])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_brymen_payload() {
        assert!(is_meter_manufacturer_data(
            0x0131,
            &[0x42, 0x4D, 0x0B, 0x00]
        ));
        assert!(is_meter_manufacturer_data(0x0131, &[0x42, 0x4D, 0x0B]));
    }

    #[test]
    fn rejects_others() {
        assert!(!is_meter_manufacturer_data(
            0x004C,
            &[0x42, 0x4D, 0x0B, 0x00]
        ));
        assert!(!is_meter_manufacturer_data(
            0x0131,
            &[0x42, 0x4D, 0x0C, 0x00]
        ));
        assert!(!is_meter_manufacturer_data(0x0131, &[0x42, 0x4D]));
        assert!(!is_meter_manufacturer_data(0x0131, &[]));
    }
}
