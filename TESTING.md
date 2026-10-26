# Testing

## Without a meter

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

What the tests are checked against:

| Source | Tests |
|---|---|
| Brymen's protocol document, rev 4 | byte layouts, flag bits, the function table copied entry by entry, unit and prefix tables, decimal point table, the `FF 80 00` = -32768 example, both firmware version examples |
| CRC-16/MODBUS catalogue | check value `0x4B37` for `123456789` |
| Properties | `tests/properties.rs`: arbitrary bytes never panic, nor do well-framed packets with any contents, decoding keeps every defined byte, single-bit errors are rejected |
| A fake meter that answers commands as Brymen's command table describes | `tests/session.rs`: password accepted and refused, model check, clock sync, short frames, pauses, stalls, link drops and reconnects, retries, hung connects and disconnects, fatal transport errors, cancelling, a slow consumer |
| Frames captured from a BM787BT, and what its LCD showed | `tests/captures.rs` |
| A fake BlueZ served over peer-to-peer D-Bus, following BlueZ 5.87's API documentation and, where that is silent, its source | `tests/bluez.rs`: scanning and matching meters, one discovery shared between a scan and a connect, a scan nobody reads, discovering an unknown meter, waiting for services, the MTU cutting frames short, link drops and the kept notify session, a replaced adapter, losing BlueZ, queue overflow, releasing the meter |

None of these is the meter itself, and Brymen's document leaves some details
out, so the checks below are the real test. The fake BlueZ cannot show timing
races in the real one, and, being peer-to-peer, it has no bus names, so the
transport's match on BlueZ's name is not exercised.

## Confirmed on a BM787BT

On 2026-10-23:

- The scan finds it by manufacturer data (`42 4D 0B 00` under company
  0x0131). BlueZ negotiates an MTU of 185 and frames are 152 bytes.
- Packets carry the MAC reversed from how BlueZ shows it, and the meter
  accepts commands addressed that way. The CRC is stored little-endian.
- The default password `0000` sent as digit values connects. A wrong
  password (`1234`) is refused with error code 3, and the meter is free again
  at once: BlueZ shows it disconnected and `0000` connects straight after.
- The model check passes. The firmware version reply is `05 00 01`, which
  this crate reads as 1.0.5.
- DC volts on a battery decodes to what the LCD shows, `3.4548`, and
  `-3.4547` with the leads swapped.
- Negative zero happens: near zero the meter sometimes flags a zero reading
  negative (seen on DC volts, nS and DC μA), and the decoder keeps the sign.
- Every dial position and SELECT mode decodes to a function in Brymen's
  table: AUTO, LoZ-DCV and LoZ-ACV on AutoV; ACV and VFD-ACV; DCV and DC+ACV;
  Logic-Hz, Duty, DCmV, ACmV and DC+ACmV; Continuity, nS and Resistance;
  Diode and Capacitance; T1 in °F and °C; DC, AC and DC+AC on mA, μA, and on
  mA with the lead in the A jack (amps). EF-Hi and EF-Lo from the EF button.
  VFD-ACV shows two decimals, as decoded.
- Displays, each matching the LCD: `OL` on continuity, resistance and diode
  with open leads; `Auto` on AutoV; `InEr` with the lead in the A jack on
  DCV; `EF-H` and `EF-L`; `-----` while AutoHold waits. With no thermocouple
  the meter flags an overload, with the top of the type-K range as the value,
  but the LCD shows `----`, and so does the decoder. With `InEr` the meter
  sends function IDs 00 00, which Brymen's table does not list. OL and ASCII
  were never flagged together.
- How often frames come depends on the function: every 200 ms on DC volts,
  200 to 440 ms on AUTO, 1.1 to 1.2 s on Logic Hz, 1.5 s on mA and μA.
  Turning the dial gives no longer pause than that, at most 2.5 s, so `Idle`
  waits 3 s by default.
- Switching the meter's Bluetooth off, or turning the dial through OFF, is
  reported as a lost link about 6 s after the last frame. Switching Bluetooth
  on again reconnects without help, within seconds.
- The meter's clock restarts at 2023-02-08 00:00 at power-on, and setting it
  works: afterwards it reads the host's local time, about half a second ahead,
  as the command carries only whole seconds. When the meter switches itself
  off, it stops advertising and a connect reports it not found. BlueZ takes
  about 2.8 s to disconnect from it.

On 2026-10-24, through bm78x-tui:

- Switching the meter's Bluetooth off and on: no data after 3 s, the link
  reported lost after 6 s, and frames resume after reconnecting, so the
  notify session BlueZ keeps is picked up again.
- The same while a scan runs: it reconnects, once after BlueZ aborted the
  first attempt (`le-connection-abort-by-local`).

The frames are in `tests/captures/`, and `tests/captures.rs` checks them
against what was observed.

## Still to check on a meter

Details neither Brymen's document nor the tests can settle:

- **Firmware version byte order**: is the reply `05 00 01` version 1.0.5 or
  5.0.1? This crate reads Brymen's `Arg[2:0]` as highest argument first, as
  for every other field. Brymen's phone app does not show the version, and
  the one the meter shows when switched on holding Δ (`T8712`) is the
  meter's own firmware, not the Bluetooth module's.
- **Weekday**: does the meter count Monday or Sunday as 1? Only the RTC
  command carries it, and the meter neither sends it back nor displays it.
