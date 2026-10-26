//! Session behaviour against a fake meter.
//!
//! The fake answers commands as Brymen's document describes: a success
//! response echoes the command ID with its result arguments, and a failure is
//! ID 0x8001 carrying the failed command and an error code. Time is Tokio's
//! paused test clock, so timeouts take no real time and are exact.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bm78x::protocol::{
    Command, ControlKind, ControlPacket, Failure, Frame, MacAddress, PacketKind, Password,
    ProtocolError, ResponseError, RtcTime, Weekday,
};
use bm78x::session::{Clock, DisconnectReason, Session, SessionConfig, SessionError, SessionEvent};
use bm78x::transport::{LinkEvent, Transport, TransportError};
use tokio::sync::mpsc;
use tokio::time::Instant;

const MAC: MacAddress = MacAddress([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);

/// Something the fake meter saw, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    Connect,
    Disconnect,
    Subscribe,
    /// A command packet, by ID.
    Command(u16),
}

/// The fake meter's state, shared between the transport and the test.
#[derive(Debug)]
struct Meter {
    password: Password,
    model: u8,
    /// Fail this many connects before succeeding.
    connect_failures: usize,
    /// Connects fail with an error the transport marks fatal.
    fatal_connects: bool,
    /// How long each connect takes.
    connect_delay: Duration,
    /// This many connects never complete.
    hanging_connects: usize,
    /// This many disconnects never complete.
    hanging_disconnects: usize,
    /// This many command reads never complete.
    hanging_reads: usize,
    /// This many responses carry the wrong command ID.
    wrong_responses: usize,
    /// This many responses have a bad checksum.
    garbled_responses: usize,
    ops: Vec<Op>,
    /// Every packet written to the command characteristic, as bytes.
    written: Vec<Vec<u8>>,
    /// The command characteristic's value: the last response.
    response: Vec<u8>,
}

impl Meter {
    fn new() -> Self {
        Meter {
            password: Password::DEFAULT,
            model: 0x0B,
            connect_failures: 0,
            fatal_connects: false,
            connect_delay: Duration::ZERO,
            hanging_connects: 0,
            hanging_disconnects: 0,
            hanging_reads: 0,
            wrong_responses: 0,
            garbled_responses: 0,
            ops: Vec::new(),
            written: Vec::new(),
            response: Vec::new(),
        }
    }

    /// The meter's reply to a command, with the fake's faults applied.
    fn respond(&mut self, command: &ControlPacket) -> ControlPacket {
        let mut reply = common::respond(MAC, self.password, self.model, command);
        if self.wrong_responses > 0 {
            self.wrong_responses -= 1;
            reply.id = 0x0004;
        }
        reply
    }
}

#[derive(Debug)]
struct FakeTransport {
    meter: Arc<Mutex<Meter>>,
    events: mpsc::UnboundedReceiver<LinkEvent>,
}

/// The test's side of the fake: push notifications and link events.
struct Handle {
    meter: Arc<Mutex<Meter>>,
    events: mpsc::UnboundedSender<LinkEvent>,
}

impl Handle {
    fn send(&self, event: LinkEvent) {
        self.events.send(event).unwrap();
    }

    fn notify(&self, bytes: Vec<u8>) {
        self.send(LinkEvent::Notification(bytes));
    }

    fn ops(&self) -> Vec<Op> {
        self.meter.lock().unwrap().ops.clone()
    }

    fn written(&self) -> Vec<Vec<u8>> {
        self.meter.lock().unwrap().written.clone()
    }
}

fn fake(meter: Meter) -> (FakeTransport, Handle) {
    let meter = Arc::new(Mutex::new(meter));
    let (sender, receiver) = mpsc::unbounded_channel();
    let transport = FakeTransport {
        meter: meter.clone(),
        events: receiver,
    };
    (
        transport,
        Handle {
            meter,
            events: sender,
        },
    )
}

#[derive(Debug, thiserror::Error)]
#[error("fake connect failure")]
struct ConnectRefused;

impl Transport for FakeTransport {
    fn address(&self) -> MacAddress {
        MAC
    }

    async fn connect(&mut self) -> Result<(), TransportError> {
        let (delay, hang) = {
            let mut meter = self.meter.lock().unwrap();
            meter.ops.push(Op::Connect);
            if meter.fatal_connects {
                return Err(TransportError::fatal(ConnectRefused));
            }
            if meter.connect_failures > 0 {
                meter.connect_failures -= 1;
                return Err(TransportError::new(ConnectRefused));
            }
            let hang = meter.hanging_connects > 0;
            if hang {
                meter.hanging_connects -= 1;
            }
            (meter.connect_delay, hang)
        };
        tokio::time::sleep(delay).await;
        if hang {
            std::future::pending::<()>().await;
        }
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        let hang = {
            let mut meter = self.meter.lock().unwrap();
            meter.ops.push(Op::Disconnect);
            let hang = meter.hanging_disconnects > 0;
            if hang {
                meter.hanging_disconnects -= 1;
            }
            hang
        };
        if hang {
            std::future::pending::<()>().await;
        }
        Ok(())
    }

    async fn write_command(&mut self, packet: &[u8; 32]) -> Result<(), TransportError> {
        let mut meter = self.meter.lock().unwrap();
        meter.written.push(packet.to_vec());
        let command = ControlPacket::parse(packet).expect("session wrote a bad packet");
        assert_eq!(command.kind, ControlKind::Command);
        meter.ops.push(Op::Command(command.id));
        meter.response = meter.respond(&command).encode().to_vec();
        if meter.garbled_responses > 0 {
            meter.garbled_responses -= 1;
            meter.response[28] ^= 0xFF;
        }
        Ok(())
    }

    async fn read_command(&mut self) -> Result<Vec<u8>, TransportError> {
        let hang = {
            let mut meter = self.meter.lock().unwrap();
            let hang = meter.hanging_reads > 0;
            if hang {
                meter.hanging_reads -= 1;
            }
            hang
        };
        if hang {
            std::future::pending::<()>().await;
        }
        Ok(self.meter.lock().unwrap().response.clone())
    }

    async fn subscribe(&mut self) -> Result<(), TransportError> {
        self.meter.lock().unwrap().ops.push(Op::Subscribe);
        Ok(())
    }

    async fn next_event(&mut self) -> LinkEvent {
        match self.events.recv().await {
            Some(event) => event,
            // The test dropped its handle: the meter goes silent.
            None => std::future::pending().await,
        }
    }
}

fn frame(n: i32) -> Vec<u8> {
    common::frame(MAC, n)
}

/// The reading field of a frame event, or a panic naming the event.
fn reading_of(event: SessionEvent) -> i32 {
    match event {
        SessionEvent::Frame { frame, raw, .. } => {
            assert_eq!(Frame::parse(&raw).unwrap(), frame);
            frame.reading().unwrap().raw_value
        }
        other => panic!("expected a frame, got {other:?}"),
    }
}

async fn next(session: &mut Session<FakeTransport>) -> SessionEvent {
    session.next_event().await.expect("unexpected fatal error")
}

async fn expect_connected(session: &mut Session<FakeTransport>) {
    match next(session).await {
        SessionEvent::Connected { address, .. } => assert_eq!(address, MAC),
        other => panic!("expected Connected, got {other:?}"),
    }
}

/// The ops of one successful connect with the default config.
fn connect_ops() -> Vec<Op> {
    vec![
        Op::Connect,
        Op::Command(0x0151),
        Op::Command(0x0116),
        Op::Subscribe,
    ]
}

#[tokio::test(start_paused = true)]
async fn connects_then_streams_frames_in_order() {
    let (transport, handle) = fake(Meter::new());
    for n in 1..=3 {
        handle.notify(frame(n));
    }
    let mut session = Session::new(transport, SessionConfig::default());

    expect_connected(&mut session).await;
    for n in 1..=3 {
        assert_eq!(reading_of(next(&mut session).await), n);
    }
    assert_eq!(handle.ops(), connect_ops());
    // The password packet is exactly what the protocol module builds.
    assert_eq!(
        handle.written()[0],
        Command::verify_password(&Password::DEFAULT)
            .encode(MAC)
            .to_vec()
    );
}

#[tokio::test(start_paused = true)]
async fn wrong_password_is_fatal() {
    let mut meter = Meter::new();
    meter.password = Password::new("1234").unwrap();
    let (transport, handle) = fake(meter);
    let mut session = Session::new(transport, SessionConfig::default());

    match session.next_event().await {
        Err(SessionError::PasswordRejected(failure)) => {
            assert_eq!(
                failure,
                Failure {
                    command: 0x0151,
                    code: 3
                }
            );
        }
        other => panic!("expected PasswordRejected, got {other:?}"),
    }
    // No retry, notifications were never enabled, and the link was released:
    // the meter takes one connection at a time, so holding it would hide it
    // from other programs.
    assert_eq!(
        handle.ops(),
        vec![Op::Connect, Op::Command(0x0151), Op::Disconnect]
    );
}

#[tokio::test(start_paused = true)]
async fn configured_password_is_sent() {
    let mut meter = Meter::new();
    meter.password = Password::new("1234").unwrap();
    let (transport, handle) = fake(meter);
    let mut config = SessionConfig::default();
    config.password = Password::new("1234").unwrap();
    let mut session = Session::new(transport, config);

    expect_connected(&mut session).await;
    let password = ControlPacket::parse(&handle.written()[0]).unwrap();
    assert_eq!(&password.args[..4], &[1, 2, 3, 4]);
}

#[tokio::test(start_paused = true)]
async fn wrong_model_is_fatal() {
    let mut meter = Meter::new();
    meter.model = 0x0C;
    let (transport, handle) = fake(meter);
    let mut session = Session::new(transport, SessionConfig::default());

    match session.next_event().await {
        Err(SessionError::WrongModel(0x0C)) => {}
        other => panic!("expected WrongModel, got {other:?}"),
    }
    assert!(!handle.ops().contains(&Op::Subscribe));
    assert_eq!(handle.ops().last(), Some(&Op::Disconnect));
}

#[tokio::test(start_paused = true)]
async fn model_check_can_be_skipped() {
    let mut meter = Meter::new();
    meter.model = 0x0C;
    let (transport, handle) = fake(meter);
    let mut config = SessionConfig::default();
    config.check_model = false;
    let mut session = Session::new(transport, config);

    expect_connected(&mut session).await;
    assert_eq!(
        handle.ops(),
        vec![Op::Connect, Op::Command(0x0151), Op::Subscribe]
    );
}

fn fixed_time() -> RtcTime {
    RtcTime::new(2026, 10, 23, Weekday::Friday, 21, 47, 59).unwrap()
}

#[tokio::test(start_paused = true)]
async fn clock_is_set_before_subscribing() {
    let (transport, handle) = fake(Meter::new());
    let mut config = SessionConfig::default();
    config.clock = Some(Clock::new(fixed_time));
    let mut session = Session::new(transport, config);

    expect_connected(&mut session).await;
    assert_eq!(
        handle.ops(),
        vec![
            Op::Connect,
            Op::Command(0x0151),
            Op::Command(0x0116),
            Op::Command(0x0010),
            Op::Subscribe,
        ]
    );
    let set_rtc = ControlPacket::parse(&handle.written()[2]).unwrap();
    assert_eq!(&set_rtc.args[..7], &[59, 47, 21, 23, 5, 10, 26]);
}

#[tokio::test(start_paused = true)]
async fn short_frame_is_reported_and_the_stream_continues() {
    let (transport, handle) = fake(Meter::new());
    let short = frame(1)[..20].to_vec();
    handle.notify(short.clone());
    handle.notify(frame(2));
    let mut session = Session::new(transport, SessionConfig::default());

    expect_connected(&mut session).await;
    match next(&mut session).await {
        SessionEvent::BadFrame { error, raw, .. } => {
            assert!(
                matches!(
                    error,
                    ProtocolError::Length {
                        kind: PacketKind::Frame,
                        expected: 152,
                        actual: 20,
                        ..
                    }
                ),
                "{error:?}"
            );
            assert_eq!(raw, short);
        }
        other => panic!("expected BadFrame, got {other:?}"),
    }
    assert_eq!(reading_of(next(&mut session).await), 2);
}

#[tokio::test(start_paused = true)]
async fn silence_reports_idle_and_keeps_the_link() {
    let (transport, handle) = fake(Meter::new());
    handle.notify(frame(1));
    let mut session = Session::new(transport, SessionConfig::default());

    expect_connected(&mut session).await;
    assert_eq!(reading_of(next(&mut session).await), 1);

    let start = Instant::now();
    assert!(matches!(next(&mut session).await, SessionEvent::Idle));
    assert_eq!(start.elapsed(), Duration::from_secs(3));

    // The meter speaks again.
    handle.notify(frame(2));
    assert_eq!(reading_of(next(&mut session).await), 2);
    assert_eq!(handle.ops(), connect_ops());
}

/// A BM787BT sends a frame every 200 ms on DC volts, but every 1.1 to 1.5 s
/// on frequency and current, and once 2.5 s apart. That is the meter
/// working, not idle.
#[tokio::test(start_paused = true)]
async fn slow_functions_are_not_idle() {
    let (transport, handle) = fake(Meter::new());
    let events = handle.events.clone();
    let mut session = Session::new(transport, SessionConfig::default());
    expect_connected(&mut session).await;

    tokio::spawn(async move {
        for n in 0..10 {
            tokio::time::sleep(Duration::from_millis(2540)).await;
            events.send(LinkEvent::Notification(frame(n))).unwrap();
        }
    });
    for n in 0..10 {
        assert_eq!(reading_of(next(&mut session).await), n);
    }
}

#[tokio::test(start_paused = true)]
async fn idle_is_reported_once_per_silence() {
    let (transport, handle) = fake(Meter::new());
    let mut session = Session::new(transport, SessionConfig::default());
    expect_connected(&mut session).await;

    assert!(matches!(next(&mut session).await, SessionEvent::Idle));
    // A frame ends the silence; the next silence is reported again.
    handle.notify(frame(1));
    assert_eq!(reading_of(next(&mut session).await), 1);
    assert!(matches!(next(&mut session).await, SessionEvent::Idle));
}

#[tokio::test(start_paused = true)]
async fn long_silence_reconnects() {
    let (transport, handle) = fake(Meter::new());
    handle.notify(frame(1));
    let mut session = Session::new(transport, SessionConfig::default());

    expect_connected(&mut session).await;
    assert_eq!(reading_of(next(&mut session).await), 1);
    let start = Instant::now();
    assert!(matches!(next(&mut session).await, SessionEvent::Idle));
    match next(&mut session).await {
        SessionEvent::Disconnected(DisconnectReason::Stalled) => {}
        other => panic!("expected a stall, got {other:?}"),
    }
    // Ten seconds after the last frame, not after the Idle event.
    assert_eq!(start.elapsed(), Duration::from_secs(10));

    expect_connected(&mut session).await;
    let mut expected = connect_ops();
    expected.push(Op::Disconnect);
    expected.extend(connect_ops());
    assert_eq!(handle.ops(), expected);
}

#[tokio::test(start_paused = true)]
async fn link_drop_reconnects_and_sends_the_password_again() {
    let (transport, handle) = fake(Meter::new());
    handle.notify(frame(1));
    handle.send(LinkEvent::Disconnected);
    let mut session = Session::new(transport, SessionConfig::default());

    expect_connected(&mut session).await;
    assert_eq!(reading_of(next(&mut session).await), 1);
    match next(&mut session).await {
        SessionEvent::Disconnected(DisconnectReason::LinkLost) => {}
        other => panic!("expected LinkLost, got {other:?}"),
    }

    handle.notify(frame(2));
    expect_connected(&mut session).await;
    assert_eq!(reading_of(next(&mut session).await), 2);
    let mut expected = connect_ops();
    expected.extend(connect_ops());
    assert_eq!(handle.ops(), expected);
}

#[tokio::test(start_paused = true)]
async fn failed_connects_are_retried_after_a_delay() {
    let mut meter = Meter::new();
    meter.connect_failures = 2;
    let (transport, handle) = fake(meter);
    let mut session = Session::new(transport, SessionConfig::default());

    let start = Instant::now();
    for _ in 0..2 {
        match next(&mut session).await {
            SessionEvent::ConnectFailed(SessionError::Transport(_)) => {}
            other => panic!("expected ConnectFailed, got {other:?}"),
        }
    }
    expect_connected(&mut session).await;
    // The first attempt is immediate; each retry waits the 2 s delay.
    assert_eq!(start.elapsed(), Duration::from_secs(4));
    assert_eq!(&handle.ops()[..2], &[Op::Connect, Op::Connect]);
}

#[tokio::test(start_paused = true)]
async fn unanswered_command_times_out_and_retries() {
    let mut meter = Meter::new();
    meter.hanging_reads = 1;
    let (transport, handle) = fake(meter);
    let mut session = Session::new(transport, SessionConfig::default());

    let start = Instant::now();
    match next(&mut session).await {
        SessionEvent::ConnectFailed(SessionError::Timeout) => {}
        other => panic!("expected a timeout, got {other:?}"),
    }
    assert_eq!(start.elapsed(), Duration::from_secs(5));
    // The half-made connection is released before retrying.
    assert_eq!(
        handle.ops(),
        vec![Op::Connect, Op::Command(0x0151), Op::Disconnect]
    );
    expect_connected(&mut session).await;
    assert_eq!(start.elapsed(), Duration::from_secs(7));
}

#[tokio::test(start_paused = true)]
async fn mismatched_response_retries() {
    let mut meter = Meter::new();
    meter.wrong_responses = 1;
    let (transport, _handle) = fake(meter);
    let mut session = Session::new(transport, SessionConfig::default());

    match next(&mut session).await {
        SessionEvent::ConnectFailed(SessionError::Response(ResponseError::Mismatch {
            expected: 0x0151,
            found: 0x0004,
            ..
        })) => {}
        other => panic!("expected a mismatch, got {other:?}"),
    }
    expect_connected(&mut session).await;
}

#[tokio::test(start_paused = true)]
async fn slow_consumer_gets_every_frame() {
    let (transport, handle) = fake(Meter::new());
    for n in 0..200 {
        handle.notify(frame(n));
    }
    let mut session = Session::new(transport, SessionConfig::default());
    expect_connected(&mut session).await;

    // Slower than the meter's 5 frames a second, and slower than the idle
    // timeout. The frames are waiting, so none is lost and none is Idle.
    for n in 0..200 {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(reading_of(next(&mut session).await), n);
    }
}

#[tokio::test(start_paused = true)]
async fn lost_notifications_are_reported() {
    let (transport, handle) = fake(Meter::new());
    handle.send(LinkEvent::Lost(3));
    let mut session = Session::new(transport, SessionConfig::default());

    expect_connected(&mut session).await;
    assert!(matches!(
        next(&mut session).await,
        SessionEvent::FramesLost(3)
    ));
}

#[tokio::test(start_paused = true)]
async fn request_while_streaming() {
    let (transport, handle) = fake(Meter::new());
    let mut session = Session::new(transport, SessionConfig::default());

    assert!(matches!(
        session.request(Command::get_firmware_version()).await,
        Err(SessionError::NotConnected)
    ));

    expect_connected(&mut session).await;
    handle.notify(frame(1));
    let args = session
        .request(Command::get_firmware_version())
        .await
        .unwrap();
    assert_eq!(&args[..3], &[0x11, 0x01, 0x00]);
    // The frame that arrived during the request is still delivered.
    assert_eq!(reading_of(next(&mut session).await), 1);
}

#[tokio::test(start_paused = true)]
async fn rejected_request_is_an_error() {
    let (transport, _handle) = fake(Meter::new());
    let mut session = Session::new(transport, SessionConfig::default());
    expect_connected(&mut session).await;

    match session.request(Command::get_device_name()).await {
        Err(SessionError::Rejected(failure)) => {
            assert_eq!(
                failure,
                Failure {
                    command: 0x0143,
                    code: 5
                }
            );
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn close_disconnects() {
    let (transport, handle) = fake(Meter::new());
    let mut session = Session::new(transport, SessionConfig::default());
    expect_connected(&mut session).await;

    session.close().await.unwrap();
    assert_eq!(handle.ops().last(), Some(&Op::Disconnect));
    // A closed session connects again if asked.
    expect_connected(&mut session).await;
    let mut expected = connect_ops();
    expected.push(Op::Disconnect);
    expected.extend(connect_ops());
    assert_eq!(handle.ops(), expected);
}

#[tokio::test(start_paused = true)]
async fn the_clock_can_capture_state() {
    let (transport, handle) = fake(Meter::new());
    let offset = 3;
    let mut config = SessionConfig::default();
    config.clock = Some(Clock::new(move || {
        RtcTime::new(2026, 10, 23, Weekday::Friday, 21 - offset, 47, 59).unwrap()
    }));
    let mut session = Session::new(transport, config);
    expect_connected(&mut session).await;
    let set_rtc = ControlPacket::parse(&handle.written()[2]).unwrap();
    assert_eq!(set_rtc.args[2], 18);
}

#[tokio::test(start_paused = true)]
async fn a_fatal_transport_error_ends_the_session() {
    // Such as the D-Bus connection to BlueZ closing: retrying cannot help.
    let mut meter = Meter::new();
    meter.fatal_connects = true;
    let (transport, handle) = fake(meter);
    let mut session = Session::new(transport, SessionConfig::default());
    match session.next_event().await {
        Err(error @ SessionError::Transport(_)) => assert!(error.is_fatal()),
        other => panic!("expected a fatal transport error, got {other:?}"),
    }
    assert_eq!(handle.ops(), vec![Op::Connect]);
}

#[tokio::test(start_paused = true)]
async fn a_hung_connect_times_out_and_releases_the_meter() {
    let mut meter = Meter::new();
    meter.hanging_connects = 1;
    let (transport, handle) = fake(meter);
    let mut session = Session::new(transport, SessionConfig::default());
    let start = Instant::now();
    match next(&mut session).await {
        SessionEvent::ConnectFailed(SessionError::ConnectTimeout) => {}
        other => panic!("expected a connect timeout, got {other:?}"),
    }
    assert_eq!(start.elapsed(), SessionConfig::default().connect_timeout);
    // BlueZ may have made the link by then, so it is released.
    assert_eq!(handle.ops(), vec![Op::Connect, Op::Disconnect]);
    expect_connected(&mut session).await;
}

#[tokio::test(start_paused = true)]
async fn a_hung_disconnect_after_a_stall_is_given_up_on() {
    let mut meter = Meter::new();
    meter.hanging_disconnects = 1;
    let (transport, handle) = fake(meter);
    let mut session = Session::new(transport, SessionConfig::default());
    expect_connected(&mut session).await;
    let start = Instant::now();
    assert!(matches!(next(&mut session).await, SessionEvent::Idle));
    assert!(matches!(
        next(&mut session).await,
        SessionEvent::Disconnected(DisconnectReason::Stalled)
    ));
    let config = SessionConfig::default();
    assert_eq!(
        start.elapsed(),
        config.stall_after + config.disconnect_timeout
    );
    expect_connected(&mut session).await;
    assert_eq!(
        handle.ops().iter().filter(|&op| *op == Op::Connect).count(),
        2
    );
}

#[tokio::test(start_paused = true)]
async fn a_hung_close_is_given_up_on() {
    let mut meter = Meter::new();
    meter.hanging_disconnects = 1;
    let (transport, _handle) = fake(meter);
    let mut session = Session::new(transport, SessionConfig::default());
    expect_connected(&mut session).await;
    let start = Instant::now();
    assert!(matches!(
        session.close().await,
        Err(SessionError::DisconnectTimeout)
    ));
    assert_eq!(start.elapsed(), SessionConfig::default().disconnect_timeout);
}

#[tokio::test(start_paused = true)]
async fn cancelling_while_connected_loses_nothing() {
    // As in `select!` against a quit signal or a redraw timer.
    let (transport, handle) = fake(Meter::new());
    let mut session = Session::new(transport, SessionConfig::default());
    expect_connected(&mut session).await;
    let mut seen = Vec::new();
    for n in 1..=5 {
        // Each frame arrives after a cancelled wait.
        let cancelled = tokio::time::timeout(Duration::from_millis(100), session.next_event());
        assert!(cancelled.await.is_err());
        handle.notify(frame(n));
        seen.push(reading_of(next(&mut session).await));
    }
    assert_eq!(seen, [1, 2, 3, 4, 5]);
}

#[tokio::test(start_paused = true)]
async fn close_after_a_cancelled_connect_releases_the_meter() {
    let mut meter = Meter::new();
    meter.connect_delay = Duration::from_secs(1);
    let (transport, handle) = fake(meter);
    let mut session = Session::new(transport, SessionConfig::default());
    let cancelled = tokio::time::timeout(Duration::from_millis(100), session.next_event());
    assert!(cancelled.await.is_err());
    session.close().await.unwrap();
    assert_eq!(handle.ops(), vec![Op::Connect, Op::Disconnect]);
    // Nothing more happens to the transport after `into_transport`.
    let _transport = session.into_transport();
}

#[test]
#[should_panic(expected = "stall_after")]
fn a_stall_timeout_no_longer_than_idle_is_refused() {
    // Otherwise the session would give up on the link the moment it
    // reported Idle.
    let (transport, _handle) = fake(Meter::new());
    let mut config = SessionConfig::default();
    config.stall_after = config.idle_after;
    let _ = Session::new(transport, config);
}

#[tokio::test(start_paused = true)]
async fn a_garbled_response_says_why_once() {
    // An error either shows its cause or returns it as its source, not
    // both, or a report that walks the chain prints it twice.
    let mut meter = Meter::new();
    meter.garbled_responses = 1;
    let (transport, _handle) = fake(meter);
    let mut session = Session::new(transport, SessionConfig::default());
    let error = match next(&mut session).await {
        SessionEvent::ConnectFailed(error @ SessionError::Protocol(_)) => error,
        other => panic!("expected a protocol error, got {other:?}"),
    };
    common::assert_causes_shown_once(&error);
    assert!(error.to_string().contains("checksum"), "{error}");
}

#[test]
fn a_session_can_be_debugged() {
    let (transport, _handle) = fake(Meter::new());
    let session = Session::new(transport, SessionConfig::default());
    let debug = format!("{session:?}");
    assert!(debug.starts_with("Session"), "{debug}");
    assert!(debug.contains("idle_after"), "{debug}");
}
