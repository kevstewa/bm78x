//! Frames captured from a real BM787BT, checked against what was observed
//! independently of this crate's decoder at the time.
//!
//! `captures/*.hex` hold one notification per line, recorded on
//! 2026-10-23. Each test says what was observed independently. For
//! `bm787bt-dcv.hex` and `bm787bt-dcv-2.hex`, recorded with
//! `cargo run --example monitor`, dial on DC volts, probes on a battery:
//!
//! - the dial position, DC volts;
//! - the LCD: five digits with four decimals, reading `3.302…` during the
//!   first capture and `3.29…` during the second (the battery was drifting);
//! - the meter's address, as BlueZ reports it;
//! - about five display updates a second, from the meter's specification.
//!
//! The meter's address has been replaced with `00:11:22:33:44:55` in the
//! info packets, in the same byte order as the real one (reversed from
//! BlueZ's), and their CRCs recomputed. Nothing else was changed.

use bm78x::protocol::{Frame, Function, MacAddress, Number, Prefix, Readout, Unit};
use bm78x::session::SessionConfig;

/// Stands in for the real address; see above.
const ADDRESS: MacAddress = MacAddress([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);

fn frames(capture: &str, count: usize) -> Vec<Frame> {
    let frames: Vec<Frame> = capture
        .lines()
        .map(|line| {
            let bytes: Vec<u8> = (0..line.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&line[i..i + 2], 16).unwrap())
                .collect();
            assert_eq!(bytes.len(), 152, "every notification is a whole frame");
            Frame::parse(&bytes).unwrap()
        })
        .collect();
    assert_eq!(frames.len(), count);
    for frame in &frames {
        assert_eq!(frame.info.mac, ADDRESS);
        // The BM78xBT fills only the first of the four reading slots.
        assert!(frame.readings[1..].iter().all(Option::is_none));
    }
    frames
}

/// Milliseconds since midnight by the meter's clock.
fn meter_ms(frame: &Frame) -> u32 {
    let t = frame.reading().unwrap().time;
    ((u32::from(t.hour) * 60 + u32::from(t.minute)) * 60 + u32::from(t.second)) * 1000
        + u32::from(t.millisecond)
}

/// Milliseconds between consecutive frames by the meter's clock.
fn gaps(frames: &[Frame]) -> Vec<u32> {
    frames
        .windows(2)
        .map(|pair| meter_ms(&pair[1]) - meter_ms(&pair[0]))
        .collect()
}

/// What holds for every frame of a DC volts capture.
fn check_dc_volts(frames: &[Frame], lcd_prefix: &str) {
    for frame in frames {
        let reading = frame.reading().unwrap();
        assert_eq!(reading.function, Function::DcVolts);
        assert_eq!(reading.unit, Unit::Volt);
        assert_eq!(reading.prefix, Prefix::Base);
        let Readout::Number(number) = reading.readout() else {
            panic!("expected a number, got {:?}", reading.readout());
        };
        assert!(!number.negative);
        assert_eq!(number.decimals, 4, "the LCD shows four decimals");
        let shown = reading.to_string();
        assert!(
            shown.starts_with(lcd_prefix)
                && shown.ends_with(" V")
                && shown.len() == "3.3021 V".len(),
            "{shown} does not match the LCD's {lcd_prefix}…"
        );
    }

    // About five updates a second: consecutive frames are about 200 ms
    // apart by the meter's own clock.
    for gap in gaps(frames) {
        assert!((150..=250).contains(&gap), "frames {gap} ms apart");
    }
}

#[test]
fn dc_volts_first_capture() {
    check_dc_volts(
        &frames(include_str!("captures/bm787bt-dcv.hex"), 10),
        "3.302",
    );
}

#[test]
fn dc_volts_second_capture() {
    check_dc_volts(
        &frames(include_str!("captures/bm787bt-dcv-2.hex"), 10),
        "3.29",
    );
}

/// Probes on a battery, then swapped. The LCD read `3.4548` and then
/// `-3.4547`. The first five frames are the meter's `3.4548`, the last five
/// its `-3.4547`.
#[test]
fn swapped_leads() {
    let frames = frames(include_str!("captures/bm787bt-dcv-swap.hex"), 10);
    let readouts: Vec<Readout> = frames
        .iter()
        .map(|f| f.reading().unwrap().readout())
        .collect();
    let number = |magnitude, negative| {
        Readout::Number(Number {
            magnitude,
            negative,
            decimals: 4,
        })
    };
    assert_eq!(readouts[..5], [number(34548, false); 5]);
    assert_eq!(readouts[5..], [number(34547, true); 5]);
    let shown: Vec<String> = frames
        .iter()
        .map(|f| f.reading().unwrap().to_string())
        .collect();
    assert_eq!(shown[0], "3.4548 V");
    assert_eq!(shown[9], "-3.4547 V");
    assert!(frames[9].reading().unwrap().si_value().unwrap() < 0.0);
}

/// DC volts near zero, the third to fifth frames flagged negative. Nothing
/// was observed independently; this records that the meter does send a
/// negative zero, a sign flag (status 1, bit 6, per Brymen) on a zero value.
#[test]
fn negative_zero() {
    let capture = include_str!("captures/bm787bt-dcv-zero.hex");
    let frames = frames(capture, 7);
    for (i, line) in capture.lines().enumerate() {
        // Reading packet byte 15 is status 1; the value is bytes 21..=23.
        let reading = &line[2 * 24..2 * 56];
        let status1 = u8::from_str_radix(&reading[2 * 15..2 * 16], 16).unwrap();
        let negative = (2..=4).contains(&i);
        assert_eq!(status1 & 0x40 != 0, negative, "frame {i}");
        assert_eq!(&reading[2 * 21..2 * 24], "000000", "frame {i}");

        let reading = frames[i].reading().unwrap();
        let expected = Readout::Number(Number {
            magnitude: 0,
            negative,
            decimals: 4,
        });
        assert_eq!(reading.readout(), expected, "frame {i}");
        let sign = if negative { "-" } else { "" };
        assert_eq!(reading.to_string(), format!("{sign}0.0000 V"));
    }
}

/// Logic Hz (six frames), then DC+AC mA (three) and DC+AC μA (four), leads
/// open. The dial went through other positions between Hz and mA, which are
/// left out; mA to μA was one turn. Observed independently by the host's
/// clock: frames 1.1 to 1.5 s apart, and 2.5 s across the turn from mA to
/// μA. The meter's clock agrees.
#[test]
fn slow_updates() {
    let frames = frames(include_str!("captures/bm787bt-slow.hex"), 13);
    let functions: Vec<Function> = frames
        .iter()
        .map(|f| f.reading().unwrap().function)
        .collect();
    assert_eq!(functions[..6], [Function::LogicHz; 6]);
    assert_eq!(functions[6..9], [Function::AcDcMilliamps; 3]);
    assert_eq!(functions[9..], [Function::AcDcMicroamps; 4]);

    // Gap 5 spans the left-out frames.
    let gaps: Vec<u32> = gaps(&frames)
        .into_iter()
        .enumerate()
        .filter(|&(i, _)| i != 5)
        .map(|(_, gap)| gap)
        .collect();
    for (i, &gap) in gaps.iter().enumerate() {
        let expected = if i == 7 { 2400..=2600 } else { 1050..=1600 };
        assert!(expected.contains(&gap), "gap {i}: {gap} ms");
    }
    // A meter that is working is not reported idle.
    let idle_after = SessionConfig::default().idle_after.as_millis();
    assert!(gaps.iter().all(|&gap| u128::from(gap) < idle_after));
}

/// One frame per display, recorded with the dial or a button set as named
/// and the LCD showing what the comment says. The decoder reports what the
/// meter sends, so for temperature it is an overload where the LCD shows
/// dashes.
#[test]
fn displays() {
    let frames = frames(include_str!("captures/bm787bt-displays.hex"), 13);
    let expected: [(Function, &str); 13] = [
        // AutoV (LoZ), leads open; LCD `Auto`.
        (Function::LozAuto, "Auto"),
        // AutoV on the battery, leads reversed; LCD about -3.448.
        (Function::LozDcVolts, "-3.4480 V"),
        // V, SELECT to VFD; LCD with two decimals.
        (Function::VfdAcVolts, "2.06 V"),
        // Ω, leads open; LCD `OL` for continuity and resistance (the
        // resistance range was not noted).
        (Function::Continuity, "OL Ω"),
        (Function::Resistance, "OL MΩ"),
        // Diode, leads open; LCD `OL`.
        (Function::Diode, "OL V"),
        // Temperature, no probe, °F then °C; LCD `----` for both.
        (Function::T1, "----"),
        (Function::T1, "----"),
        // mA with the lead in the A jack; LCD 0.0001.
        (Function::AcDcAmps, "0.0001 A"),
        // DCV with the lead in the A jack; LCD `InEr`. The meter sends
        // function IDs 00 00, which are not in Brymen's table.
        (Function::from_ids(0x00, 0x00), "InEr"),
        // EF pressed once, then again; LCD `EF-H`, then `EF-L`.
        (Function::EfHigh, "EF-H"),
        (Function::EfLow, "EF-L"),
        // DCV, leads open, AutoHold on; LCD `- - - - -`.
        (Function::DcVolts, "-----"),
    ];
    for (frame, (function, shown)) in frames.iter().zip(expected) {
        let reading = frame.reading().unwrap();
        assert_eq!(reading.function, function);
        assert_eq!(reading.to_string(), shown);
    }
    // The meter flags the temperature frames as overloads (with the top of
    // the type-K range as the value), not as ASCII dashes.
    for frame in &frames[6..8] {
        let reading = frame.reading().unwrap();
        assert!(reading.flags.overload() && !reading.flags.ascii());
    }
}

/// Decoded values can be kept in hash sets, for logging only the changes,
/// and hashing agrees with equality.
#[test]
fn decoded_values_can_be_collected_in_sets() {
    use std::collections::HashSet;
    use std::hash::Hash;

    fn distinct<T: Eq + Hash + Clone>(values: &[T]) {
        let mut by_eq: Vec<T> = Vec::new();
        for value in values {
            if !by_eq.contains(value) {
                by_eq.push(value.clone());
            }
        }
        // Every value twice over: hashing must collapse the copies.
        let set: HashSet<T> = values.iter().chain(values).cloned().collect();
        assert_eq!(set.len(), by_eq.len());
    }

    let frames = frames(include_str!("captures/bm787bt-displays.hex"), 13);
    let readings: Vec<_> = frames.iter().map(|f| *f.reading().unwrap()).collect();
    let readouts: Vec<_> = readings.iter().map(|r| r.readout()).collect();
    distinct(&frames);
    distinct(&frames.iter().map(|f| f.info).collect::<Vec<_>>());
    distinct(&readings);
    distinct(&readouts);
}
