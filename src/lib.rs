//! Client library for the Brymen BM78xBT family of Bluetooth multimeters
//! (BM787BT and BM788BT).
//!
//! - [`protocol`] decodes the meter's notification frames and builds its
//!   command packets. It does no I/O, so it can be tested without a meter.
//! - [`session`] runs a connection: password, notifications, reconnecting.
//! - [`transport`] is the interface between a session and the Bluetooth
//!   stack.
//! - [`bluez`] implements it over BlueZ, the Linux Bluetooth stack, and
//!   finds meters.
//!
//! The protocol is described in Brymen's "BM78xBT Wireless Data Communication
//! Protocol" (revision 4). Where that document is silent, the source says
//! what a BM787BT does, or marks the detail "Inferred". The repository's
//! [`TESTING.md`](https://github.com/kevstewa/bm78x/blob/main/TESTING.md)
//! says what has been checked on a real meter.
//!
//! # Example
//!
//! Printing a meter's readings until Ctrl-C. Besides this crate, it needs
//! tokio's `macros`, `rt` and `signal` features.
//!
//! ```no_run
//! use bm78x::bluez::{Bluez, BluezConfig};
//! use bm78x::session::{Session, SessionConfig, SessionEvent};
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let bluez = Bluez::system().await?;
//!     let address = "00:11:22:33:44:55".parse()?;
//!     let meter = bluez.meter(address, BluezConfig::default());
//!     let mut session = Session::new(meter, SessionConfig::default());
//!     loop {
//!         tokio::select! {
//!             _ = tokio::signal::ctrl_c() => break,
//!             event = session.next_event() => {
//!                 if let SessionEvent::Frame { frame, .. } = event?
//!                     && let Some(reading) = frame.reading()
//!                 {
//!                     println!("{}: {reading}", reading.function);
//!                 }
//!             }
//!         }
//!     }
//!     // The meter takes one connection at a time, so let it go.
//!     session.close().await?;
//!     Ok(())
//! }
//! ```
//!
//! [`Frame::parse`](protocol::Frame::parse) shows decoding a notification
//! without a meter.

#![warn(missing_docs)]

pub mod bluez;
pub mod protocol;
pub mod session;
pub mod transport;

// Compiles the README's example, so it cannot go stale.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

/// What the public API must not allow, checked by compiling each example
/// and expecting it to fail.
///
/// An [`RtcTime`](protocol::RtcTime) is made only by its checked
/// constructor:
///
/// ```compile_fail
/// let _ = bm78x::protocol::RtcTime {
///     year: 2026, month: 10, day: 23, weekday: 5, hour: 21, minute: 47, second: 59,
/// };
/// ```
///
/// zbus errors do not convert into [`BluezError`](bluez::BluezError), so a
/// zbus upgrade is not a change to this crate's API:
///
/// ```compile_fail
/// let _: bm78x::bluez::BluezError = zbus::Error::InvalidReply.into();
/// ```
///
/// Variants with named fields can gain fields, so patterns need `..`:
///
/// ```compile_fail
/// fn address(event: bm78x::session::SessionEvent) {
///     if let bm78x::session::SessionEvent::Connected { address } = event {
///         let _ = address;
///     }
/// }
/// ```
#[cfg(doctest)]
struct ApiLimits;

#[cfg(test)]
mod tests {
    use std::fmt::Debug;
    use std::hash::Hash;

    use crate::bluez::{Bluez, BluezConfig, BluezError, BluezTransport, Discovered, Scan};
    use crate::protocol::*;
    use crate::session::{DisconnectReason, Session, SessionError, SessionEvent};
    use crate::transport::{LinkEvent, TransportError};

    /// The public types have the common traits that fit them (Rust API
    /// Guidelines, C-COMMON-TRAITS, C-DEBUG and C-SEND-SYNC).
    #[test]
    fn public_types_have_the_common_traits() {
        fn value<T: Debug + Clone + Eq + Hash + Send + Sync>() {}
        fn handle<T: Debug + Send + Sync>() {}
        value::<Frame>();
        value::<DeviceInfo>();
        value::<Reading>();
        value::<Readout>();
        value::<Number>();
        value::<MeterTime>();
        value::<Command>();
        value::<ControlPacket>();
        value::<PacketKind>();
        value::<RtcTime>();
        value::<Weekday>();
        value::<Discovered>();
        value::<DisconnectReason>();
        value::<LinkEvent>();
        fn config<T: Debug + Clone + Eq + Send + Sync>() {}
        config::<BluezConfig>();
        handle::<Session<BluezTransport>>();
        handle::<Bluez>();
        handle::<Scan>();
        handle::<BluezTransport>();
        handle::<SessionEvent>();
        handle::<SessionError>();
        handle::<BluezError>();
        handle::<TransportError>();
        assert_eq!(DeviceName::default(), DeviceName::new("BM78xBT").unwrap());
    }
}
