//! Lists BM78xBT meters in range: `cargo run --example scan [SECONDS]`.

use std::time::Duration;

use bm78x::bluez::Bluez;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seconds = std::env::args().nth(1).map_or(Ok(10), |s| s.parse())?;
    let bluez = Bluez::system().await?;
    let mut scan = bluez.scan().await?;
    eprintln!("Scanning for {seconds} s. Turn the meter's Bluetooth on by holding Δ for a second.");

    let deadline = tokio::time::sleep(Duration::from_secs(seconds));
    tokio::pin!(deadline);
    loop {
        let found = tokio::select! {
            _ = &mut deadline => break,
            _ = tokio::signal::ctrl_c() => break,
            found = scan.next() => found?,
        };
        let name = found.name.as_deref().unwrap_or("?");
        let rssi = found.rssi.map_or("?".into(), |r| format!("{r} dBm"));
        println!("{}  {name}  {rssi}", found.address);
    }
    scan.stop().await?;
    Ok(())
}
