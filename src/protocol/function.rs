/// Defines [`Function`] and its conversions from one table, so each entry's
/// IDs and label are written exactly once.
macro_rules! functions {
    ($($variant:ident = ($main:literal, $sub:literal, $label:literal),)*) => {
        /// The meter function selected by the dial and buttons (bytes 18 and
        /// 20 of a reading packet: the main and sub-function IDs).
        ///
        /// Each variant's documentation is its label from Brymen's function
        /// table, which [`Function::label`] returns.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum Function {
            $(
                #[doc = $label]
                $variant,
            )*
            /// A pair of IDs the protocol document does not list, as
            /// `(main, sub)`.
            Unknown(super::Unlisted<(u8, u8)>),
        }

        impl Function {
            /// Every known function, in the order of Brymen's table.
            pub const ALL: &[Function] = &[$(Function::$variant),*];

            /// Decodes a main and sub-function ID pair.
            pub fn from_ids(main: u8, sub: u8) -> Self {
                match (main, sub) {
                    $(($main, $sub) => Function::$variant,)*
                    _ => Function::Unknown(super::Unlisted((main, sub))),
                }
            }

            /// The `(main, sub)` function IDs.
            pub fn ids(self) -> (u8, u8) {
                match self {
                    $(Function::$variant => ($main, $sub),)*
                    Function::Unknown(ids) => ids.code(),
                }
            }

            /// The function's name as Brymen's table gives it, or `None` for
            /// [`Function::Unknown`].
            pub fn label(self) -> Option<&'static str> {
                match self {
                    $(Function::$variant => Some($label),)*
                    Function::Unknown(_) => None,
                }
            }
        }
    };
}

// Labels are the "Display Function" column of Brymen's table, verbatim.
functions! {
    LozAcVolts = (0x02, 0x00, "LoZ-ACV"),
    LozDcVolts = (0x02, 0x01, "LoZ-DCV"),
    LozAuto = (0x02, 0x03, "AUTO"),
    AcVolts = (0x03, 0x00, "ACV"),
    DcVolts = (0x03, 0x01, "DCV"),
    AcDcVolts = (0x03, 0x02, "DC+ACV"),
    LineVoltsHz = (0x03, 0x03, "Hz of Line Volt"),
    VfdHz = (0x17, 0x00, "Hz of VFD-ACV"),
    VfdAcVolts = (0x17, 0x01, "VFD-ACV"),
    AcMillivolts = (0x04, 0x00, "ACmV"),
    DcMillivolts = (0x04, 0x01, "DCmV"),
    AcDcMillivolts = (0x04, 0x02, "DC+ACmV"),
    AcMicroamps = (0x05, 0x00, "ACμA"),
    DcMicroamps = (0x05, 0x01, "DCμA"),
    AcDcMicroamps = (0x05, 0x02, "DC+ACμA"),
    MicroampsHz = (0x05, 0x03, "Hz of μA"),
    AcMilliamps = (0x06, 0x00, "ACmA"),
    DcMilliamps = (0x06, 0x01, "DCmA"),
    AcDcMilliamps = (0x06, 0x02, "DC+ACmA"),
    MilliampsHz = (0x06, 0x03, "Hz of mA"),
    LoopPercent = (0x06, 0x08, "%4~20mA"),
    AcAmps = (0x07, 0x00, "ACA"),
    DcAmps = (0x07, 0x01, "DCA"),
    AcDcAmps = (0x07, 0x02, "DC+ACA"),
    AmpsHz = (0x07, 0x03, "Hz of A"),
    T1 = (0x0C, 0x00, "T1"),
    T2 = (0x0C, 0x01, "T2"),
    T1MinusT2 = (0x0C, 0x02, "T1 - T2"),
    Resistance = (0x0D, 0x00, "Resistance"),
    Capacitance = (0x0E, 0x00, "Capacitance"),
    Continuity = (0x0F, 0x00, "Continuity"),
    Diode = (0x10, 0x00, "Diode"),
    Conductance = (0x11, 0x00, "nS Conductance"),
    DutyCycle = (0x12, 0x00, "Duty Cycle (%)"),
    LogicHz = (0x13, 0x00, "Logic-Hz"),
    EfLow = (0x22, 0x00, "EF-Lo"),
    EfHigh = (0x22, 0x01, "EF-Hi"),
    LineSignalHz = (0x23, 0x00, "Hz of Line Volt/Current"),
}

impl std::fmt::Display for Function {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.label() {
            Some(label) => f.write_str(label),
            None => {
                let (main, sub) = self.ids();
                write!(f, "function({main:#04x}, {sub:#04x})")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Brymen rev 4's function ID table (page 14), copied from the PDF
    /// rather than from the code: (main ID, sub-function ID, display
    /// function).
    const BRYMEN_TABLE: [(u8, u8, &str); 38] = [
        (0x02, 0x00, "LoZ-ACV"),
        (0x02, 0x01, "LoZ-DCV"),
        (0x02, 0x03, "AUTO"),
        (0x03, 0x00, "ACV"),
        (0x03, 0x01, "DCV"),
        (0x03, 0x02, "DC+ACV"),
        (0x03, 0x03, "Hz of Line Volt"),
        (0x17, 0x00, "Hz of VFD-ACV"),
        (0x17, 0x01, "VFD-ACV"),
        (0x04, 0x00, "ACmV"),
        (0x04, 0x01, "DCmV"),
        (0x04, 0x02, "DC+ACmV"),
        (0x05, 0x00, "ACμA"),
        (0x05, 0x01, "DCμA"),
        (0x05, 0x02, "DC+ACμA"),
        (0x05, 0x03, "Hz of μA"),
        (0x06, 0x00, "ACmA"),
        (0x06, 0x01, "DCmA"),
        (0x06, 0x02, "DC+ACmA"),
        (0x06, 0x03, "Hz of mA"),
        (0x06, 0x08, "%4~20mA"),
        (0x07, 0x00, "ACA"),
        (0x07, 0x01, "DCA"),
        (0x07, 0x02, "DC+ACA"),
        (0x07, 0x03, "Hz of A"),
        (0x0C, 0x00, "T1"),
        (0x0C, 0x01, "T2"),
        (0x0C, 0x02, "T1 - T2"),
        (0x0D, 0x00, "Resistance"),
        (0x0E, 0x00, "Capacitance"),
        (0x0F, 0x00, "Continuity"),
        (0x10, 0x00, "Diode"),
        (0x11, 0x00, "nS Conductance"),
        (0x12, 0x00, "Duty Cycle (%)"),
        (0x13, 0x00, "Logic-Hz"),
        (0x22, 0x00, "EF-Lo"),
        (0x22, 0x01, "EF-Hi"),
        (0x23, 0x00, "Hz of Line Volt/Current"),
    ];

    #[test]
    fn every_brymen_entry() {
        for (main, sub, label) in BRYMEN_TABLE {
            let function = Function::from_ids(main, sub);
            assert_eq!(function.label(), Some(label), "{main:#04x} {sub:#04x}");
            assert_eq!(function.ids(), (main, sub));
            assert_eq!(function.to_string(), label);
        }
        // And nothing that is not in Brymen's table.
        assert_eq!(Function::ALL.len(), BRYMEN_TABLE.len());
        for &function in Function::ALL {
            let (main, sub) = function.ids();
            assert!(BRYMEN_TABLE.iter().any(|&(m, s, _)| (m, s) == (main, sub)));
        }
    }

    #[test]
    fn variants_name_the_right_entries() {
        // A few the rest of this crate and the TUI rely on by name.
        assert_eq!(Function::DcVolts.ids(), (0x03, 0x01));
        assert_eq!(Function::LozAuto.ids(), (0x02, 0x03));
        assert_eq!(Function::AcDcAmps.ids(), (0x07, 0x02));
        assert_eq!(Function::T1.ids(), (0x0C, 0x00));
        assert_eq!(Function::EfLow.ids(), (0x22, 0x00));
        assert_eq!(Function::EfHigh.ids(), (0x22, 0x01));
        assert_eq!(Function::LineSignalHz.ids(), (0x23, 0x00));
    }

    #[test]
    fn every_other_pair_is_unknown() {
        for main in 0..=u8::MAX {
            for sub in 0..=u8::MAX {
                let function = Function::from_ids(main, sub);
                assert_eq!(function.ids(), (main, sub));
                // Only a pair missing from Brymen's table is unknown.
                let listed = BRYMEN_TABLE.iter().any(|&(m, s, _)| (m, s) == (main, sub));
                assert_eq!(matches!(function, Function::Unknown(_)), !listed);
            }
        }
        let unknown = Function::from_ids(0x02, 0x02);
        assert!(matches!(unknown, Function::Unknown(u) if u.code() == (0x02, 0x02)));
        assert_eq!(unknown.label(), None);
        assert_eq!(format!("{unknown}"), "function(0x02, 0x02)");
    }
}
