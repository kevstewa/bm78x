# bm78x

A client library for the Brymen BM787BT and BM788BT ("BM78xBT") Bluetooth
multimeters, in pure Rust.

- `protocol` decodes the meter's frames and builds its commands. It does no
  I/O, so it works with any Bluetooth stack.
- `session` runs a connection: the password, notifications, and reconnecting
  after a dropped link.
- `bluez` finds meters and connects to them through BlueZ on Linux, over
  D-Bus.

## Usage

```sh
cargo add bm78x
cargo add tokio --features macros,rt,signal
```

```rust,no_run
use bm78x::bluez::{Bluez, BluezConfig};
use bm78x::session::{Session, SessionConfig, SessionEvent};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bluez = Bluez::system().await?;
    let address = "00:11:22:33:44:55".parse()?;
    let meter = bluez.meter(address, BluezConfig::default());
    let mut session = Session::new(meter, SessionConfig::default());
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            event = session.next_event() => {
                if let SessionEvent::Frame { frame, .. } = event?
                    && let Some(reading) = frame.reading()
                {
                    println!("{}: {reading}", reading.function);
                }
            }
        }
    }
    // The meter takes one connection at a time, so let it go.
    session.close().await?;
    Ok(())
}
```

`Bluez::scan` lists meters in range. To use another Bluetooth stack,
implement `transport::Transport` for it.

## Requirements

`session` and `bluez` run on tokio. `bluez` needs Linux with BlueZ running
and a Bluetooth LE adapter; no C libraries are needed. The meter accepts one
connection at a time, so close Brymen's phone app first. To turn the meter's
Bluetooth on, hold Δ for a second until the Bluetooth annunciator lights.

## Protocol and testing

The protocol follows Brymen's "BM78xBT Wireless Data Communication
Protocol", revision 4. Where that document is silent, the source says what a
BM787BT does, or marks the detail "Inferred". It has been tested on a
BM787BT; the BM788BT, which the same document covers, has not been tried.
[TESTING.md](https://github.com/kevstewa/bm78x/blob/main/TESTING.md) says
what the tests check and what has been confirmed on the meter.

[bm78x-tui](https://github.com/kevstewa/bm78x-tui) is a terminal UI built
on this library.

Licensed under the MIT license.
