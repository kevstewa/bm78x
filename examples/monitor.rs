//! Connects to a meter and prints each frame, raw and decoded:
//! `cargo run --example monitor ADDRESS [FRAMES]`.

use bm78x::bluez::{Bluez, BluezConfig};
use bm78x::protocol::MacAddress;
use bm78x::session::{Session, SessionConfig, SessionEvent};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let address: MacAddress = args
        .next()
        .ok_or("usage: monitor ADDRESS [FRAMES]")?
        .parse()?;
    let frames: usize = args.next().map_or(Ok(10), |s| s.parse())?;

    let bluez = Bluez::system().await?;
    let mut session = Session::new(
        bluez.meter(address, BluezConfig::default()),
        SessionConfig::default(),
    );

    let mut seen = 0;
    let result = loop {
        let event = tokio::select! {
            _ = tokio::signal::ctrl_c() => break Ok(()),
            event = session.next_event() => event,
        };
        match event {
            Ok(SessionEvent::Connected { address, .. }) => {
                let mtu = session.transport().mtu();
                eprintln!("connected to {address}, MTU {mtu:?}");
            }
            Ok(SessionEvent::Frame { frame, raw, .. }) => {
                seen += 1;
                println!("{}", hex(&raw));
                let info = &frame.info;
                match frame.reading() {
                    Some(r) => eprintln!(
                        "  {} | {} | {:?} | flags {:02x} {:02x} {:02x} | meter clock {} | info mac {} battery {:#04x}",
                        r,
                        r.function,
                        r.si_value(),
                        r.flags.status0,
                        r.flags.status1,
                        r.flags.status2,
                        r.time,
                        info.mac,
                        info.battery
                    ),
                    None => eprintln!("  no reading | info mac {}", info.mac),
                }
                if seen == frames {
                    break Ok(());
                }
            }
            Ok(other) => eprintln!("{other:?}"),
            Err(error) => break Err(error),
        }
    };
    // Release the meter, which takes one connection at a time, even after
    // Ctrl-C.
    session.close().await?;
    Ok(result?)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
