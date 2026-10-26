//! The interface between a [`Session`](crate::session::Session) and the
//! Bluetooth stack.
//!
//! A [`Transport`] reaches one meter's GATT characteristics. The BlueZ
//! backend implements it; so can a fake for tests, or a backend for another
//! Bluetooth stack.

use std::future::Future;

use crate::protocol::MacAddress;

/// One meter's GATT characteristics.
///
/// The session calls these methods one at a time, never concurrently.
///
/// # Errors
///
/// Methods fail with [`TransportError::new`] for anything a reconnect may
/// fix, such as the meter being out of range or the link dropping mid-call:
/// the session reports it and retries. They fail with
/// [`TransportError::fatal`] only when retrying cannot help, such as the
/// connection to the Bluetooth stack closing: the session then gives up.
///
/// # Generic only
///
/// The methods return `impl Future`, so the trait cannot be used as
/// `dyn Transport`; a [`Session`](crate::session::Session) is generic over
/// its transport instead.
pub trait Transport: Send {
    /// The meter's Bluetooth address, which command packets carry.
    fn address(&self) -> MacAddress;

    /// Connects to the meter, or reconnects after the link dropped.
    fn connect(&mut self) -> impl Future<Output = Result<(), TransportError>> + Send;

    /// Disconnects. Called when the session gives up on a stalled link.
    fn disconnect(&mut self) -> impl Future<Output = Result<(), TransportError>> + Send;

    /// Writes a packet to the command characteristic, with response.
    fn write_command(
        &mut self,
        packet: &[u8; 32],
    ) -> impl Future<Output = Result<(), TransportError>> + Send;

    /// Reads the command characteristic, where the meter leaves its
    /// response to the last command.
    fn read_command(&mut self) -> impl Future<Output = Result<Vec<u8>, TransportError>> + Send;

    /// Enables notifications on the notify characteristic.
    fn subscribe(&mut self) -> impl Future<Output = Result<(), TransportError>> + Send;

    /// Waits for the next notification or link event.
    ///
    /// The transport must buffer notifications that arrive between calls,
    /// so a slow caller loses nothing. If a buffer overflows anyway, it
    /// reports how many were dropped with [`LinkEvent::Lost`].
    ///
    /// This must be cancel-safe: the session drops the future when it times
    /// out, and no notification may be lost when that happens.
    fn next_event(&mut self) -> impl Future<Output = LinkEvent> + Send;
}

/// What [`Transport::next_event`] reports.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LinkEvent {
    /// A notification from the notify characteristic, as received.
    Notification(Vec<u8>),
    /// This many notifications were dropped because a buffer was full.
    Lost(u64),
    /// The link dropped.
    Disconnected,
}

/// An error from a [`Transport`], such as a D-Bus failure or the meter not
/// being found.
#[derive(Debug)]
pub struct TransportError {
    error: Box<dyn std::error::Error + Send + Sync>,
    fatal: bool,
}

impl TransportError {
    /// Wraps any error. The session retries after it.
    pub fn new(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        TransportError {
            error: error.into(),
            fatal: false,
        }
    }

    /// Wraps an error that retrying cannot fix, such as the connection to
    /// the Bluetooth stack closing. The session gives up on it.
    pub fn fatal(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        TransportError {
            error: error.into(),
            fatal: true,
        }
    }

    /// Whether this was made with [`TransportError::fatal`].
    pub fn is_fatal(&self) -> bool {
        self.fatal
    }

    /// The wrapped error, if it is an `E`, such as a
    /// [`BluezError`](crate::bluez::BluezError).
    pub fn downcast_ref<E: std::error::Error + 'static>(&self) -> Option<&E> {
        self.error.downcast_ref()
    }
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}
