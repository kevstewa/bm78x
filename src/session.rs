//! A connection to a meter that delivers its readings: password, optional
//! model check and clock sync, notifications, and reconnecting.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::{Instant, sleep, timeout, timeout_at};

use crate::protocol::{
    Command, ControlPacket, Failure, Frame, MODEL_SERIES_ID, MacAddress, Password, ProtocolError,
    ResponseError, RtcTime,
};
use crate::transport::{LinkEvent, Transport, TransportError};

/// Gives the time to set the meter's clock to. See [`SessionConfig::clock`].
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> RtcTime + Send + Sync>);

impl Clock {
    /// A clock that calls `now`.
    pub fn new(now: impl Fn() -> RtcTime + Send + Sync + 'static) -> Self {
        Clock(Arc::new(now))
    }

    fn now(&self) -> RtcTime {
        (self.0)()
    }
}

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Clock")
    }
}

/// How a [`Session`] behaves. Start from [`SessionConfig::default`] and set
/// the fields that matter.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SessionConfig {
    /// The connection password.
    pub password: Password,
    /// After the password, check that the meter reports the BM78x model
    /// series ID, and fail with [`SessionError::WrongModel`] if not.
    pub check_model: bool,
    /// If set, called after each connect to get the time to set the meter's
    /// clock to. The meter has no clock battery, so its clock is otherwise
    /// wrong.
    pub clock: Option<Clock>,
    /// Silence after which [`SessionEvent::Idle`] is reported. Longer than
    /// the gap between a BM787BT's slowest updates, about 2.5 s, which come
    /// on frequency and current.
    pub idle_after: Duration,
    /// Silence after which the link is treated as dead and reconnected,
    /// even though the transport has not reported a disconnect. Must be
    /// longer than `idle_after`.
    pub stall_after: Duration,
    /// How long to wait for each command and response.
    pub response_timeout: Duration,
    /// How long to wait before retrying a failed connect.
    pub reconnect_delay: Duration,
    /// How long a whole connect may take, handshake included, before it is
    /// abandoned and retried.
    pub connect_timeout: Duration,
    /// How long to wait for a disconnect before giving up on it.
    pub disconnect_timeout: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            password: Password::DEFAULT,
            check_model: true,
            clock: None,
            idle_after: Duration::from_secs(3),
            stall_after: Duration::from_secs(10),
            response_timeout: Duration::from_secs(5),
            reconnect_delay: Duration::from_secs(2),
            connect_timeout: Duration::from_secs(45),
            disconnect_timeout: Duration::from_secs(10),
        }
    }
}

/// What [`Session::next_event`] reports.
#[derive(Debug)]
#[non_exhaustive]
pub enum SessionEvent {
    /// Connected, password accepted and notifications enabled.
    #[non_exhaustive]
    Connected {
        /// The meter's address.
        address: MacAddress,
    },
    /// A connect attempt failed. The session retries after
    /// [`SessionConfig::reconnect_delay`].
    ConnectFailed(SessionError),
    /// A notification that decoded as a frame.
    #[non_exhaustive]
    Frame {
        /// The decoded frame.
        frame: Frame,
        /// The notification as received.
        raw: Vec<u8>,
    },
    /// A notification that did not decode, such as one cut short by a small
    /// ATT MTU.
    #[non_exhaustive]
    BadFrame {
        /// Why it did not decode.
        error: ProtocolError,
        /// The notification as received.
        raw: Vec<u8>,
    },
    /// No notification for [`SessionConfig::idle_after`]. Reported once per
    /// silence; the link is still up.
    Idle,
    /// The transport dropped this many notifications.
    FramesLost(u64),
    /// The link is down. The session reconnects on the next call.
    Disconnected(DisconnectReason),
}

/// Constructors for the variants with named fields, which can gain fields
/// and so cannot be built directly outside this crate. A session makes its
/// own events; these are for testing code that handles them.
impl SessionEvent {
    /// A [`SessionEvent::Connected`] event.
    pub fn connected(address: MacAddress) -> Self {
        SessionEvent::Connected { address }
    }

    /// A [`SessionEvent::Frame`] event.
    pub fn frame(frame: Frame, raw: Vec<u8>) -> Self {
        SessionEvent::Frame { frame, raw }
    }

    /// A [`SessionEvent::BadFrame`] event.
    pub fn bad_frame(error: ProtocolError, raw: Vec<u8>) -> Self {
        SessionEvent::BadFrame { error, raw }
    }
}

/// Why a [`SessionEvent::Disconnected`] happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DisconnectReason {
    /// The transport reported that the link dropped.
    LinkLost,
    /// No notification for [`SessionConfig::stall_after`]; the session
    /// disconnected.
    Stalled,
}

/// Why a session operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SessionError {
    /// The meter rejected the password.
    #[error("the meter rejected the password")]
    PasswordRejected(#[source] Failure),
    /// The meter is not a BM78x.
    #[error("the meter reports model series {0:#04x}, not a BM78x")]
    WrongModel(u8),
    /// The meter rejected a command.
    #[error(transparent)]
    Rejected(Failure),
    /// The meter's response was not the expected one.
    #[error(transparent)]
    Response(ResponseError),
    /// The meter's response did not decode.
    #[error("bad response: {0}")]
    Protocol(ProtocolError),
    /// No response within [`SessionConfig::response_timeout`].
    #[error("the meter did not respond in time")]
    Timeout,
    /// Connecting took longer than [`SessionConfig::connect_timeout`].
    #[error("connecting took too long")]
    ConnectTimeout,
    /// Disconnecting took longer than [`SessionConfig::disconnect_timeout`].
    #[error("disconnecting took too long")]
    DisconnectTimeout,
    /// The operation needs a connection and there is none.
    #[error("not connected")]
    NotConnected,
    /// The transport failed.
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl SessionError {
    /// Whether retrying cannot help: the meter refused the password, a
    /// command, or is the wrong model, or the transport says so (see
    /// [`TransportError::fatal`]). [`Session::next_event`] returns these as
    /// errors instead of retrying.
    pub fn is_fatal(&self) -> bool {
        match self {
            SessionError::PasswordRejected(_)
            | SessionError::WrongModel(_)
            | SessionError::Rejected(_) => true,
            SessionError::Transport(error) => error.is_fatal(),
            _ => false,
        }
    }
}

/// A connection to one meter.
///
/// Call [`Session::next_event`] in a loop. The first call connects. The
/// session reads from the transport only when asked, so a slow caller leaves
/// notifications waiting in the transport's buffer instead of losing them.
#[derive(Debug)]
pub struct Session<T> {
    transport: T,
    config: SessionConfig,
    state: State,
}

#[derive(Debug, Clone, Copy)]
enum State {
    /// Not connected. `retry` means the last attempt failed, so wait
    /// before the next.
    Down { retry: bool },
    /// Connected and receiving.
    Up {
        /// When the last notification arrived, or when the link came up.
        last_heard: Instant,
        /// Whether `Idle` has been reported for the current silence.
        idle: bool,
    },
}

impl<T: Transport> Session<T> {
    /// Creates a session. Nothing happens until [`Session::next_event`].
    ///
    /// # Panics
    ///
    /// If [`SessionConfig::stall_after`] is not longer than
    /// [`SessionConfig::idle_after`]: the session would give up on the link
    /// the moment it reported [`SessionEvent::Idle`].
    pub fn new(transport: T, config: SessionConfig) -> Self {
        assert!(
            config.stall_after > config.idle_after,
            "SessionConfig::stall_after ({:?}) must be longer than idle_after ({:?})",
            config.stall_after,
            config.idle_after,
        );
        Session {
            transport,
            config,
            state: State::Down { retry: false },
        }
    }

    /// Waits for the next event, connecting or reconnecting first if the
    /// link is down.
    ///
    /// # Errors
    ///
    /// Only failures that retrying cannot fix (see
    /// [`SessionError::is_fatal`]): the password or model check failing, the
    /// meter rejecting a command, or a fatal transport error. Other
    /// failures are reported as [`SessionEvent::ConnectFailed`] and retried.
    ///
    /// Cancel-safe while connected: dropping the future loses no
    /// notification, so it can be raced against other work in `select!`.
    /// Dropping it while connecting leaves the link half made; call
    /// [`Session::close`] or `next_event` again afterwards, and either
    /// sorts it out.
    pub async fn next_event(&mut self) -> Result<SessionEvent, SessionError> {
        match self.state {
            State::Down { retry } => {
                if retry {
                    sleep(self.config.reconnect_delay).await;
                }
                match self.establish().await {
                    Ok(()) => {
                        self.state = State::Up {
                            last_heard: Instant::now(),
                            idle: false,
                        };
                        Ok(SessionEvent::connected(self.transport.address()))
                    }
                    Err(error) => {
                        self.state = State::Down { retry: true };
                        if error.is_fatal() {
                            Err(error)
                        } else {
                            Ok(SessionEvent::ConnectFailed(error))
                        }
                    }
                }
            }
            State::Up { last_heard, idle } => {
                // Before Idle, wait out the idle timeout; after it, the
                // longer stall timeout. Both count from the last
                // notification, so a stall is `stall_after` of silence.
                let wait = if idle {
                    self.config.stall_after
                } else {
                    self.config.idle_after
                };
                match timeout_at(last_heard + wait, self.transport.next_event()).await {
                    Ok(LinkEvent::Notification(raw)) => {
                        self.state = State::Up {
                            last_heard: Instant::now(),
                            idle: false,
                        };
                        Ok(match Frame::parse(&raw) {
                            Ok(frame) => SessionEvent::Frame { frame, raw },
                            Err(error) => SessionEvent::BadFrame { error, raw },
                        })
                    }
                    Ok(LinkEvent::Lost(count)) => Ok(SessionEvent::FramesLost(count)),
                    Ok(LinkEvent::Disconnected) => {
                        self.state = State::Down { retry: false };
                        Ok(SessionEvent::Disconnected(DisconnectReason::LinkLost))
                    }
                    Err(_) if !idle => {
                        self.state = State::Up {
                            last_heard,
                            idle: true,
                        };
                        Ok(SessionEvent::Idle)
                    }
                    Err(_) => {
                        // The link is probably dead already; a failure to
                        // disconnect changes nothing.
                        let _ = self.disconnect().await;
                        self.state = State::Down { retry: false };
                        Ok(SessionEvent::Disconnected(DisconnectReason::Stalled))
                    }
                }
            }
        }
    }

    /// Sends a command while connected and returns the response's
    /// arguments. Notifications arriving meanwhile wait in the transport.
    ///
    /// # Errors
    ///
    /// [`SessionError::NotConnected`] if the link is down; otherwise any
    /// failure of the exchange, such as [`SessionError::Rejected`] or
    /// [`SessionError::Timeout`].
    pub async fn request(&mut self, command: Command) -> Result<[u8; 14], SessionError> {
        match self.state {
            State::Up { .. } => self.exchange(&command).await,
            State::Down { .. } => Err(SessionError::NotConnected),
        }
    }

    /// Connects and runs the handshake. If the handshake fails or the whole
    /// takes too long, the link is released: the meter accepts one
    /// connection at a time, and holding it would hide it from every other
    /// program. A transport whose connect fails releases the link itself.
    async fn establish(&mut self) -> Result<(), SessionError> {
        let limit = self.config.connect_timeout;
        let mut connected = false;
        let connect = async {
            self.transport.connect().await?;
            connected = true;
            self.handshake().await
        };
        let (result, release) = match timeout(limit, connect).await {
            Ok(result) => {
                let release = connected && result.is_err();
                (result, release)
            }
            Err(_) => (Err(SessionError::ConnectTimeout), true),
        };
        if release {
            let _ = self.disconnect().await;
        }
        result
    }

    /// Disconnects, giving up after [`SessionConfig::disconnect_timeout`].
    async fn disconnect(&mut self) -> Result<(), SessionError> {
        timeout(self.config.disconnect_timeout, self.transport.disconnect())
            .await
            .map_err(|_| SessionError::DisconnectTimeout)??;
        Ok(())
    }

    /// Brymen's connection flow: verify the password, then enable
    /// notifications. The model check and clock sync go in between.
    async fn handshake(&mut self) -> Result<(), SessionError> {
        let verify = Command::verify_password(&self.config.password);
        match self.exchange(&verify).await {
            Err(SessionError::Rejected(failure)) => {
                return Err(SessionError::PasswordRejected(failure));
            }
            result => result?,
        };
        if self.config.check_model {
            let args = self.exchange(&Command::get_model_series_id()).await?;
            if args[0] != MODEL_SERIES_ID {
                return Err(SessionError::WrongModel(args[0]));
            }
        }
        if let Some(clock) = &self.config.clock {
            self.exchange(&Command::set_rtc(&clock.now())).await?;
        }
        self.transport.subscribe().await?;
        Ok(())
    }

    /// Writes a command, reads the response, and returns its arguments.
    async fn exchange(&mut self, command: &Command) -> Result<[u8; 14], SessionError> {
        let packet = command.encode(self.transport.address());
        let transport = &mut self.transport;
        let bytes = timeout(self.config.response_timeout, async {
            transport.write_command(&packet).await?;
            transport.read_command().await
        })
        .await
        .map_err(|_| SessionError::Timeout)??;
        let response = ControlPacket::parse(&bytes).map_err(SessionError::Protocol)?;
        match response.response_to(command.id()) {
            Ok(args) => Ok(*args),
            Err(ResponseError::Failed(failure)) => Err(SessionError::Rejected(failure)),
            Err(error) => Err(SessionError::Response(error)),
        }
    }

    /// The transport, for backend-specific details such as
    /// [`BluezTransport::mtu`](crate::bluez::BluezTransport::mtu).
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Disconnects. The next [`Session::next_event`] connects again.
    ///
    /// # Errors
    ///
    /// The transport's error if disconnecting fails, or
    /// [`SessionError::DisconnectTimeout`]. The session counts as
    /// disconnected either way.
    pub async fn close(&mut self) -> Result<(), SessionError> {
        self.state = State::Down { retry: false };
        self.disconnect().await
    }

    /// The transport, for reuse once the session is closed.
    pub fn into_transport(self) -> T {
        self.transport
    }
}
