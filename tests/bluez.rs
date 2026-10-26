//! The BlueZ transport against a fake BlueZ served over a peer-to-peer
//! D-Bus connection, with no Bluetooth hardware or system bus.
//!
//! The fake follows BlueZ's D-Bus API documentation (doc/org.bluez.Adapter.rst,
//! org.bluez.Device.rst and org.bluez.GattCharacteristic.rst in BlueZ 5.87)
//! and, where those are silent, BlueZ's source (src/adapter.c, src/device.c
//! and src/gatt-client.c):
//!
//! - an ObjectManager at `/`, the adapter at `/org/bluez/hci0`, devices at
//!   `dev_XX_XX_XX_XX_XX_XX` below it;
//! - one discovery per client: a second `StartDiscovery` is refused with
//!   `org.bluez.Error.Busy`;
//! - GATT objects that appear after connecting, with `ServicesResolved`
//!   turning true once they have, and that stay after a disconnect, as with
//!   BlueZ's default `Cache = always`;
//! - one notify session per client, kept across a disconnect and
//!   re-registered once the link is back: `StartNotify` on a kept session
//!   succeeds, or fails with `org.bluez.Error.InProgress` before it is
//!   re-registered;
//! - notifications as `PropertiesChanged` on the characteristic's `Value`,
//!   cut to the ATT MTU less 3 bytes.
//!
//! Its command characteristic answers as Brymen's command table describes.
//! Being peer-to-peer, the connection has no bus names, so the transport's
//! match on BlueZ's name is not exercised.

mod common;

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bm78x::bluez::{Bluez, BluezConfig, BluezError, Discovered};
use bm78x::protocol::{
    COMMAND_UUID, Command, ControlPacket, MacAddress, NOTIFY_UUID, Password, SERVICE_UUID,
};
use bm78x::session::{DisconnectReason, Session, SessionConfig, SessionEvent};
use bm78x::transport::{LinkEvent, Transport, TransportError};
use tokio::time::timeout;
use zbus::connection::Builder;
use zbus::object_server::{ObjectServer, SignalEmitter};
use zbus::{Connection, Guid, OwnedVariant, Variant, fdo, interface};

const METER: MacAddress = MacAddress([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
const OTHER_METER: MacAddress = MacAddress([0x00, 0x11, 0x22, 0x33, 0x44, 0x66]);
const NOT_A_METER: MacAddress = MacAddress([0xA0, 0xB1, 0xC2, 0xD3, 0xE4, 0xF5]);
const ADAPTER: &str = "/org/bluez/hci0";

/// Manufacturer data a BM78xBT advertises: Brymen's company ID, then
/// `B`, `M`, model series 0x0B and status 0x00.
fn brymen_data() -> Option<(u16, Vec<u8>)> {
    Some((0x0131, vec![0x42, 0x4D, 0x0B, 0x00]))
}

fn device_path(adapter: &str, address: MacAddress) -> String {
    format!("{adapter}/dev_{}", address.to_string().replace(':', "_"))
}

/// BlueZ's error replies.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum FakeError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Busy(String),
    Failed(String),
    InProgress(String),
}

/// A D-Bus call the fake received, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    SetDiscoveryFilter {
        transport: Option<String>,
    },
    StartDiscovery,
    StopDiscovery,
    Connect(MacAddress),
    Disconnect(MacAddress),
    StartNotify,
    StopNotify,
    WriteValue {
        value: Vec<u8>,
        write_type: Option<String>,
    },
    ReadValue,
}

type Log = Arc<Mutex<Vec<Call>>>;

/// A remote device as BlueZ would expose it.
#[derive(Debug, Clone)]
struct DeviceSpec {
    address: MacAddress,
    name: Option<String>,
    rssi: Option<i16>,
    manufacturer: Option<(u16, Vec<u8>)>,
    uuids: Vec<String>,
    /// Whether GATT services ever resolve after connecting.
    resolves: bool,
    has_command: bool,
    mtu: u16,
    /// How long BlueZ takes to answer `Connect`.
    connect_delay: Duration,
    /// `Connect` fails with `org.bluez.Error.Failed` and this message.
    connect_error: Option<String>,
    /// How many RSSI changes BlueZ signals while connecting.
    chatter: usize,
    /// Whether GATT objects stay after a disconnect (`Cache = always`, the
    /// default) or go (`Cache = no`).
    gatt_cache: bool,
    /// Whether a kept notify session is re-registered only some time after
    /// `ServicesResolved`.
    late_reregister: bool,
}

impl DeviceSpec {
    fn meter(address: MacAddress) -> Self {
        DeviceSpec {
            address,
            name: Some("BM78xBT".into()),
            rssi: Some(-60),
            manufacturer: brymen_data(),
            uuids: Vec::new(),
            resolves: true,
            has_command: true,
            mtu: 185,
            connect_delay: Duration::from_millis(30),
            connect_error: None,
            chatter: 0,
            gatt_cache: true,
            late_reregister: false,
        }
    }
}

struct FakeAdapter {
    path: String,
    log: Log,
    discovering: bool,
    /// Devices that appear once discovery starts.
    in_range: Arc<Mutex<Vec<DeviceSpec>>>,
}

#[interface(name = "org.bluez.Adapter1")]
impl FakeAdapter {
    async fn set_discovery_filter(&self, filter: HashMap<String, OwnedVariant>) {
        let transport = filter
            .get("Transport")
            .and_then(|v| String::try_from(v.clone()).ok());
        self.log
            .lock()
            .unwrap()
            .push(Call::SetDiscoveryFilter { transport });
    }

    async fn start_discovery(
        &mut self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), FakeError> {
        self.log.lock().unwrap().push(Call::StartDiscovery);
        // "Every client can only start one discovery."
        if self.discovering {
            return Err(FakeError::Busy("Operation already in progress".into()));
        }
        self.discovering = true;
        self.discovering_changed(&emitter).await?;
        let found: Vec<_> = self.in_range.lock().unwrap().drain(..).collect();
        for spec in found {
            add_device(server, &self.path, spec, self.log.clone()).await;
        }
        Ok(())
    }

    async fn stop_discovery(
        &mut self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), FakeError> {
        self.log.lock().unwrap().push(Call::StopDiscovery);
        if !self.discovering {
            return Err(FakeError::Failed("No discovery started".into()));
        }
        self.discovering = false;
        self.discovering_changed(&emitter).await?;
        Ok(())
    }

    #[zbus(property)]
    fn address(&self) -> String {
        "AA:BB:CC:DD:EE:00".into()
    }

    #[zbus(property)]
    fn powered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn discovering(&self) -> bool {
        self.discovering
    }
}

struct FakeDevice {
    path: String,
    spec: DeviceSpec,
    log: Log,
    connected: bool,
    services_resolved: bool,
}

fn not_present() -> fdo::Error {
    fdo::Error::UnknownProperty("not present".into())
}

#[interface(name = "org.bluez.Device1")]
impl FakeDevice {
    async fn connect(
        &mut self,
        #[zbus(connection)] connection: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), FakeError> {
        self.log
            .lock()
            .unwrap()
            .push(Call::Connect(self.spec.address));
        if let Some(message) = &self.spec.connect_error {
            return Err(FakeError::Failed(message.clone()));
        }
        // Advertisements keep arriving while the link comes up.
        for n in 0..self.spec.chatter {
            let rssi = -40 - (n % 40) as i16;
            self.spec.rssi = Some(rssi);
            self.r_s_s_i_changed(&emitter).await?;
        }
        // BlueZ answers Connect once the LE link is up, which takes a while.
        tokio::time::sleep(self.spec.connect_delay).await;
        self.connected = true;
        self.connected_changed(&emitter).await?;
        if self.spec.resolves {
            // BlueZ returns from Connect before service discovery finishes.
            tokio::spawn(resolve_services(
                connection.clone(),
                self.path.clone(),
                self.spec.clone(),
                self.log.clone(),
            ));
        }
        Ok(())
    }

    async fn disconnect(
        &mut self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        self.log
            .lock()
            .unwrap()
            .push(Call::Disconnect(self.spec.address));
        // A BM787BT takes about 2.8 s; a little delay is enough to show the
        // transport waits for it.
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.link_down(server, &emitter).await
    }

    #[zbus(property)]
    fn address(&self) -> String {
        self.spec.address.to_string()
    }

    #[zbus(property)]
    fn name(&self) -> fdo::Result<String> {
        self.spec.name.clone().ok_or_else(not_present)
    }

    #[zbus(property, name = "RSSI")]
    fn rssi(&self) -> fdo::Result<i16> {
        self.spec.rssi.ok_or_else(not_present)
    }

    #[zbus(property)]
    fn manufacturer_data(&self) -> fdo::Result<HashMap<u16, OwnedVariant>> {
        let (company, data) = self.spec.manufacturer.clone().ok_or_else(not_present)?;
        let value = OwnedVariant::try_from(Variant::from(data)).unwrap();
        Ok(HashMap::from([(company, value)]))
    }

    #[zbus(property, name = "UUIDs")]
    fn uuids(&self) -> Vec<String> {
        self.spec.uuids.clone()
    }

    #[zbus(property)]
    fn connected(&self) -> bool {
        self.connected
    }

    #[zbus(property)]
    fn services_resolved(&self) -> bool {
        self.services_resolved
    }
}

impl FakeDevice {
    /// The link goes down: BlueZ clears `Connected` and `ServicesResolved`.
    /// With the GATT cache, the objects stay and notify sessions wait to be
    /// re-registered; without it, the objects go.
    async fn link_down(
        &mut self,
        server: &ObjectServer,
        emitter: &SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        self.connected = false;
        self.services_resolved = false;
        self.connected_changed(emitter).await?;
        self.services_resolved_changed(emitter).await?;
        let path = &self.path;
        if self.spec.gatt_cache {
            if let Ok(notify) = server
                .interface::<_, FakeNotify>(format!("{path}/service0010/char0011"))
                .await
            {
                notify.get_mut().await.registered = false;
            }
        } else {
            let _ = server
                .remove::<FakeNotify, _>(format!("{path}/service0010/char0011"))
                .await;
            let _ = server
                .remove::<FakeCommand, _>(format!("{path}/service0010/char0014"))
                .await;
            let _ = server
                .remove::<FakeService, _>(format!("{path}/service0010"))
                .await;
        }
        Ok(())
    }
}

struct FakeService;

#[interface(name = "org.bluez.GattService1")]
impl FakeService {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        SERVICE_UUID.into()
    }
}

struct FakeNotify {
    log: Log,
    value: Vec<u8>,
    /// The client has a notify session.
    session: bool,
    /// The session is registered with the meter, so notifications flow.
    registered: bool,
    mtu: u16,
}

#[interface(name = "org.bluez.GattCharacteristic1")]
impl FakeNotify {
    async fn start_notify(
        &mut self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), FakeError> {
        self.log.lock().unwrap().push(Call::StartNotify);
        // "Each client can only have one active notify session."
        if self.session {
            return if self.registered {
                Ok(())
            } else {
                Err(FakeError::InProgress("In Progress".into()))
            };
        }
        self.session = true;
        self.registered = true;
        self.notifying_changed(&emitter).await?;
        Ok(())
    }

    async fn stop_notify(
        &mut self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), FakeError> {
        self.log.lock().unwrap().push(Call::StopNotify);
        if !self.session {
            return Err(FakeError::Failed("No notify session started".into()));
        }
        self.session = false;
        self.registered = false;
        self.notifying_changed(&emitter).await?;
        Ok(())
    }

    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        NOTIFY_UUID.into()
    }

    #[zbus(property)]
    fn flags(&self) -> Vec<String> {
        vec!["notify".into()]
    }

    #[zbus(property)]
    fn value(&self) -> Vec<u8> {
        self.value.clone()
    }

    #[zbus(property)]
    fn notifying(&self) -> bool {
        self.session && self.registered
    }

    #[zbus(property, name = "MTU")]
    fn mtu(&self) -> u16 {
        self.mtu
    }
}

struct FakeCommand {
    log: Log,
    address: MacAddress,
    /// The characteristic's value: the meter's last response.
    response: Vec<u8>,
}

#[interface(name = "org.bluez.GattCharacteristic1")]
impl FakeCommand {
    async fn write_value(
        &mut self,
        value: Vec<u8>,
        options: HashMap<String, OwnedVariant>,
    ) -> fdo::Result<()> {
        let write_type = options
            .get("type")
            .and_then(|v| String::try_from(v.clone()).ok());
        self.log.lock().unwrap().push(Call::WriteValue {
            value: value.clone(),
            write_type,
        });
        let command = ControlPacket::parse(&value)
            .map_err(|e| fdo::Error::Failed(format!("meter ignores a bad packet: {e}")))?;
        let reply = common::respond(self.address, Password::DEFAULT, 0x0B, &command);
        self.response = reply.encode().to_vec();
        Ok(())
    }

    async fn read_value(&self, _options: HashMap<String, OwnedVariant>) -> Vec<u8> {
        self.log.lock().unwrap().push(Call::ReadValue);
        self.response.clone()
    }

    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        COMMAND_UUID.into()
    }

    #[zbus(property)]
    fn flags(&self) -> Vec<String> {
        vec!["read".into(), "write".into()]
    }
}

async fn add_device(server: &ObjectServer, adapter: &str, spec: DeviceSpec, log: Log) {
    let path = device_path(adapter, spec.address);
    let device = FakeDevice {
        path: path.clone(),
        spec,
        log,
        connected: false,
        services_resolved: false,
    };
    assert!(server.at(path, device).await.unwrap().is_none());
}

async fn resolve_services(connection: Connection, path: String, spec: DeviceSpec, log: Log) {
    tokio::time::sleep(Duration::from_millis(50)).await;
    let server = connection.object_server();
    // Service discovery stops if the link went down meanwhile.
    let Ok(device) = server.interface::<_, FakeDevice>(path.as_str()).await else {
        return;
    };
    if !device.get().await.connected {
        return;
    }
    let notify_path = format!("{path}/service0010/char0011");
    match server
        .interface::<_, FakeNotify>(notify_path.as_str())
        .await
    {
        // Cached from an earlier connection: re-register its session.
        Ok(notify) => {
            if spec.late_reregister {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let mut notify = notify.get_mut().await;
                    notify.registered = notify.session;
                });
            } else {
                let mut notify = notify.get_mut().await;
                notify.registered = notify.session;
            }
        }
        Err(_) => {
            server
                .at(format!("{path}/service0010"), FakeService)
                .await
                .unwrap();
            let notify = FakeNotify {
                log: log.clone(),
                value: Vec::new(),
                session: false,
                registered: false,
                mtu: spec.mtu,
            };
            server.at(notify_path, notify).await.unwrap();
            if spec.has_command {
                let command = FakeCommand {
                    log,
                    address: spec.address,
                    response: Vec::new(),
                };
                server
                    .at(format!("{path}/service0010/char0014"), command)
                    .await
                    .unwrap();
            }
        }
    }
    let mut device_mut = device.get_mut().await;
    device_mut.services_resolved = true;
    device_mut
        .services_resolved_changed(device.signal_emitter())
        .await
        .unwrap();
}

/// The test's side of the fake.
struct Fake {
    server: Connection,
    log: Log,
    in_range: Arc<Mutex<Vec<DeviceSpec>>>,
}

impl Fake {
    fn calls(&self) -> Vec<Call> {
        self.log.lock().unwrap().clone()
    }

    fn count(&self, call: &Call) -> usize {
        self.calls().iter().filter(|c| *c == call).count()
    }

    /// Waits until the fake has received `call`.
    async fn received(&self, call: Call) {
        while !self.calls().contains(&call) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// The meter sends a notification, if its notify session is running.
    /// BlueZ delivers what fits the ATT MTU, less 3 bytes of header.
    async fn notify(&self, address: MacAddress, bytes: Vec<u8>) {
        let path = format!("{}/service0010/char0011", device_path(ADAPTER, address));
        let notify = self
            .server
            .object_server()
            .interface::<_, FakeNotify>(path)
            .await
            .unwrap();
        let mut notify_mut = notify.get_mut().await;
        if notify_mut.session && notify_mut.registered {
            let fits = usize::from(notify_mut.mtu - 3);
            notify_mut.value = bytes[..bytes.len().min(fits)].to_vec();
            notify_mut
                .value_changed(notify.signal_emitter())
                .await
                .unwrap();
        }
    }

    /// The link drops, as when the meter's Bluetooth is switched off.
    async fn drop_link(&self, address: MacAddress) {
        let server = self.server.object_server();
        let device = server
            .interface::<_, FakeDevice>(device_path(ADAPTER, address))
            .await
            .unwrap();
        let mut device_mut = device.get_mut().await;
        device_mut
            .link_down(server, device.signal_emitter())
            .await
            .unwrap();
    }

    /// A new advertisement arrives. BlueZ queues a device's property
    /// changes and sends them as one `PropertiesChanged`, so this does too.
    async fn advertise(&self, address: MacAddress, manufacturer: (u16, Vec<u8>), rssi: i16) {
        let device = self
            .server
            .object_server()
            .interface::<_, FakeDevice>(device_path(ADAPTER, address))
            .await
            .unwrap();
        let mut device_mut = device.get_mut().await;
        device_mut.spec.manufacturer = Some(manufacturer.clone());
        device_mut.spec.rssi = Some(rssi);
        let (company, data) = manufacturer;
        let data = HashMap::from([(company, Variant::from(data))]);
        let changed = HashMap::from([
            ("ManufacturerData", Variant::from(data)),
            ("RSSI", Variant::from(rssi)),
        ]);
        let interface = zbus::names::InterfaceName::try_from("org.bluez.Device1").unwrap();
        fdo::Properties::properties_changed(
            device.signal_emitter(),
            interface,
            changed,
            (&[][..]).into(),
        )
        .await
        .unwrap();
    }

    /// A device comes into range of a running discovery.
    async fn appear(&self, spec: DeviceSpec) {
        add_device(self.server.object_server(), ADAPTER, spec, self.log.clone()).await;
    }

    /// A device that appears once discovery starts.
    fn in_range(&self, spec: DeviceSpec) {
        self.in_range.lock().unwrap().push(spec);
    }

    /// The adapter is unplugged, taking its devices with it, and plugged
    /// back in, as BlueZ's next adapter, with `devices`.
    async fn replug(&self, devices: Vec<DeviceSpec>) -> String {
        let server = self.server.object_server();
        for address in [METER, OTHER_METER, NOT_A_METER] {
            let path = device_path(ADAPTER, address);
            let _ = server
                .remove::<FakeNotify, _>(format!("{path}/service0010/char0011"))
                .await;
            let _ = server
                .remove::<FakeCommand, _>(format!("{path}/service0010/char0014"))
                .await;
            let _ = server
                .remove::<FakeService, _>(format!("{path}/service0010"))
                .await;
            let _ = server.remove::<FakeDevice, _>(path).await;
        }
        assert!(server.remove::<FakeAdapter, _>(ADAPTER).await.unwrap());
        let path = "/org/bluez/hci1".to_string();
        let adapter = FakeAdapter {
            path: path.clone(),
            log: self.log.clone(),
            discovering: false,
            in_range: self.in_range.clone(),
        };
        assert!(server.at(path.as_str(), adapter).await.unwrap().is_none());
        for spec in devices {
            add_device(server, &path, spec, self.log.clone()).await;
        }
        path
    }
}

/// Starts a fake BlueZ knowing `devices`, and a `Bluez` connected to it.
async fn start(devices: Vec<DeviceSpec>) -> (Bluez, Fake) {
    let (server_socket, client_socket) = UnixStream::pair().unwrap();
    let log = Log::default();
    let in_range = Arc::new(Mutex::new(Vec::new()));
    let adapter = FakeAdapter {
        path: ADAPTER.into(),
        log: log.clone(),
        discovering: false,
        in_range: in_range.clone(),
    };
    let server = Builder::unix_stream(server_socket)
        .server(Guid::generate())
        .p2p()
        .serve_at("/", fdo::ObjectManager)
        .serve_at(ADAPTER, adapter)
        .build();
    let client = Builder::unix_stream(client_socket).p2p().build();
    let (server, client) = tokio::join!(server, client);
    let (server, client) = (server.unwrap(), client.unwrap());
    for spec in devices {
        add_device(server.object_server(), ADAPTER, spec, log.clone()).await;
    }
    let bluez = Bluez::from_connection(client).await.unwrap();
    (
        bluez,
        Fake {
            server,
            log,
            in_range,
        },
    )
}

/// Fails a test that waits too long, instead of hanging it.
async fn within<F: Future>(future: F) -> F::Output {
    timeout(Duration::from_secs(5), future)
        .await
        .expect("timed out")
}

fn bluez_error(error: &TransportError) -> &BluezError {
    error
        .downcast_ref::<BluezError>()
        .expect("not a BluezError")
}

fn summary(found: &Discovered) -> (MacAddress, Option<&str>, Option<i16>) {
    (found.address, found.name.as_deref(), found.rssi)
}

fn frame_value(event: SessionEvent) -> i32 {
    match event {
        SessionEvent::Frame { frame, .. } => frame.reading().unwrap().raw_value,
        other => panic!("expected a frame, got {other:?}"),
    }
}

/// A session config that retries quickly, for tests that reconnect.
fn quick_retries() -> SessionConfig {
    let mut config = SessionConfig::default();
    config.reconnect_delay = Duration::from_millis(50);
    config
}

#[tokio::test]
async fn scan_lists_meters_and_ignores_other_devices() {
    let renamed = DeviceSpec {
        name: Some("Bench".into()),
        rssi: Some(-70),
        ..DeviceSpec::meter(OTHER_METER)
    };
    let other = DeviceSpec {
        name: Some("Phone".into()),
        manufacturer: Some((0x004C, vec![0x42, 0x4D, 0x0B, 0x00])),
        ..DeviceSpec::meter(NOT_A_METER)
    };
    let (bluez, fake) = start(vec![DeviceSpec::meter(METER), renamed, other]).await;

    let mut scan = within(bluez.scan()).await.unwrap();
    let mut seen = Vec::new();
    for _ in 0..2 {
        seen.push(within(scan.next()).await.unwrap());
    }
    seen.sort_by_key(|d| d.address);
    assert_eq!(
        seen.iter().map(summary).collect::<Vec<_>>(),
        vec![
            (METER, Some("BM78xBT"), Some(-60)),
            (OTHER_METER, Some("Bench"), Some(-70)),
        ]
    );
    // The phone is never reported.
    assert!(
        timeout(Duration::from_millis(300), scan.next())
            .await
            .is_err()
    );

    within(scan.stop()).await.unwrap();
    assert_eq!(
        fake.calls(),
        vec![
            Call::SetDiscoveryFilter {
                transport: Some("le".into())
            },
            Call::StartDiscovery,
            Call::StopDiscovery,
        ]
    );
}

#[tokio::test]
async fn scan_matches_the_service_uuid_too() {
    // A meter BlueZ has connected to before lists its services in UUIDs.
    let known = DeviceSpec {
        manufacturer: None,
        rssi: None,
        uuids: vec![SERVICE_UUID.into()],
        ..DeviceSpec::meter(METER)
    };
    let (bluez, _fake) = start(vec![known]).await;

    let mut scan = within(bluez.scan()).await.unwrap();
    let found = within(scan.next()).await.unwrap();
    assert_eq!(summary(&found), (METER, Some("BM78xBT"), None));
}

#[tokio::test]
async fn scan_reports_meters_as_their_advertisements_arrive() {
    let (bluez, fake) = start(Vec::new()).await;
    // BlueZ often creates the device object before it has the
    // manufacturer data.
    fake.in_range(DeviceSpec {
        manufacturer: None,
        ..DeviceSpec::meter(METER)
    });

    let mut scan = within(bluez.scan()).await.unwrap();
    assert!(
        timeout(Duration::from_millis(300), scan.next())
            .await
            .is_err()
    );

    fake.advertise(METER, brymen_data().unwrap(), -58).await;
    let found = within(scan.next()).await.unwrap();
    assert_eq!((found.address, found.rssi), (METER, Some(-58)));

    fake.advertise(METER, brymen_data().unwrap(), -49).await;
    let updated = within(scan.next()).await.unwrap();
    assert_eq!((updated.address, updated.rssi), (METER, Some(-49)));
}

#[tokio::test]
async fn a_scan_that_is_not_read_does_not_block_bluez() {
    // Signals for a scan nobody reads must not back up and stall every
    // other call on the connection.
    let (bluez, fake) = start(vec![
        DeviceSpec::meter(METER),
        DeviceSpec::meter(OTHER_METER),
    ])
    .await;
    let scan = within(bluez.scan()).await.unwrap();
    for n in 0..200 {
        within(fake.advertise(OTHER_METER, brymen_data().unwrap(), -40 - n % 40)).await;
    }
    let mut transport = bluez.meter(METER, BluezConfig::default());
    within(transport.connect()).await.unwrap();
    // The scan still has the latest news, and only once per meter.
    let mut scan = scan;
    let first = within(scan.next()).await.unwrap();
    let second = within(scan.next()).await.unwrap();
    let mut seen = [summary(&first), summary(&second)];
    seen.sort();
    assert_eq!(
        seen,
        [
            (METER, Some("BM78xBT"), Some(-60)),
            (OTHER_METER, Some("BM78xBT"), Some(-79)),
        ]
    );
}

#[tokio::test]
async fn signals_while_connecting_do_not_block_the_connect() {
    let spec = DeviceSpec {
        chatter: 200,
        ..DeviceSpec::meter(METER)
    };
    let (bluez, _fake) = start(vec![spec]).await;
    let mut transport = bluez.meter(METER, BluezConfig::default());
    within(transport.connect()).await.unwrap();
}

#[tokio::test]
async fn a_scan_and_a_connect_share_one_discovery() {
    // BlueZ gives each client one discovery: the connect's search for an
    // unknown meter must not fail as Busy, nor stop the scan's.
    let (bluez, fake) = start(Vec::new()).await;
    let mut scan = within(bluez.scan()).await.unwrap();
    let mut transport = bluez.meter(METER, BluezConfig::default());
    // The meter comes into range while the connect is looking for it.
    let appears = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        fake.appear(DeviceSpec::meter(METER)).await;
    };
    let (connected, ()) = within(async { tokio::join!(transport.connect(), appears) }).await;
    connected.unwrap();
    assert_eq!(fake.count(&Call::StartDiscovery), 1);
    assert_eq!(fake.count(&Call::StopDiscovery), 0);
    assert_eq!(within(scan.next()).await.unwrap().address, METER);
    within(scan.stop()).await.unwrap();
    assert_eq!(fake.calls().last(), Some(&Call::StopDiscovery));
}

#[tokio::test]
async fn dropping_a_scan_stops_discovery() {
    let (bluez, fake) = start(Vec::new()).await;
    let scan = within(bluez.scan()).await.unwrap();
    drop(scan);
    within(fake.received(Call::StopDiscovery)).await;
}

#[tokio::test]
async fn an_abandoned_search_stops_discovery() {
    let (bluez, fake) = start(Vec::new()).await;
    let mut transport = bluez.meter(METER, BluezConfig::default());
    tokio::select! {
        _ = transport.connect() => panic!("the meter is not in range"),
        _ = fake.received(Call::StartDiscovery) => {}
    }
    within(fake.received(Call::StopDiscovery)).await;
}

#[tokio::test]
async fn session_streams_frames_over_bluez() {
    let (bluez, fake) = start(vec![DeviceSpec::meter(METER)]).await;
    let transport = bluez.meter(METER, BluezConfig::default());
    let mut session = Session::new(transport, SessionConfig::default());

    match within(session.next_event()).await.unwrap() {
        SessionEvent::Connected { address, .. } => assert_eq!(address, METER),
        other => panic!("expected Connected, got {other:?}"),
    }
    let request = |value: [u8; 32]| Call::WriteValue {
        value: value.to_vec(),
        write_type: Some("request".into()),
    };
    assert_eq!(
        fake.calls(),
        vec![
            Call::Connect(METER),
            request(Command::verify_password(&Password::DEFAULT).encode(METER)),
            Call::ReadValue,
            request(Command::get_model_series_id().encode(METER)),
            Call::ReadValue,
            Call::StartNotify,
        ]
    );

    for n in 1..=3 {
        fake.notify(METER, common::frame(METER, n)).await;
    }
    for n in 1..=3 {
        assert_eq!(frame_value(within(session.next_event()).await.unwrap()), n);
    }
}

#[tokio::test]
async fn a_small_mtu_cuts_frames_short() {
    let spec = DeviceSpec {
        mtu: 23,
        ..DeviceSpec::meter(METER)
    };
    let (bluez, fake) = start(vec![spec]).await;
    let mut transport = bluez.meter(METER, BluezConfig::default());
    assert_eq!(transport.mtu(), None);
    within(transport.connect()).await.unwrap();
    assert_eq!(transport.mtu(), Some(23));

    let mut session = Session::new(transport, SessionConfig::default());
    within(session.next_event()).await.unwrap();
    fake.notify(METER, common::frame(METER, 1)).await;
    match within(session.next_event()).await.unwrap() {
        SessionEvent::BadFrame { raw, .. } => assert_eq!(raw.len(), 20),
        other => panic!("expected a short frame, got {other:?}"),
    }
}

#[tokio::test]
async fn services_that_never_resolve_time_out() {
    let spec = DeviceSpec {
        resolves: false,
        ..DeviceSpec::meter(METER)
    };
    let (bluez, fake) = start(vec![spec]).await;
    let mut config = BluezConfig::default();
    config.services_timeout = Duration::from_millis(200);
    let mut transport = bluez.meter(METER, config);
    let error = within(transport.connect()).await.unwrap_err();
    assert!(
        matches!(bluez_error(&error), BluezError::ServicesTimeout),
        "{error}"
    );
    // The half-made link is released, or the meter stays invisible to
    // every other program.
    assert_eq!(fake.calls().last(), Some(&Call::Disconnect(METER)));
}

#[tokio::test]
async fn missing_command_characteristic_is_an_error() {
    let spec = DeviceSpec {
        has_command: false,
        ..DeviceSpec::meter(METER)
    };
    let (bluez, fake) = start(vec![spec]).await;
    let mut transport = bluez.meter(METER, BluezConfig::default());
    let error = within(transport.connect()).await.unwrap_err();
    assert!(
        matches!(
            bluez_error(&error),
            BluezError::MissingCharacteristic("command")
        ),
        "{error}"
    );
    assert_eq!(fake.calls().last(), Some(&Call::Disconnect(METER)));
}

#[tokio::test]
async fn bluez_errors_carry_their_name() {
    let spec = DeviceSpec {
        connect_error: Some("le-connection-abort-by-local".into()),
        ..DeviceSpec::meter(METER)
    };
    let (bluez, _fake) = start(vec![spec]).await;
    let mut transport = bluez.meter(METER, BluezConfig::default());
    let error = within(transport.connect()).await.unwrap_err();
    assert!(!error.is_fatal());
    match bluez_error(&error) {
        BluezError::Dbus(error) => {
            assert_eq!(error.name(), Some("org.bluez.Error.Failed"));
            assert_eq!(error.message(), Some("le-connection-abort-by-local"));
        }
        other => panic!("expected a D-Bus error, got {other:?}"),
    }
    common::assert_causes_shown_once(&error);
}

#[tokio::test]
async fn scans_and_transports_can_be_debugged() {
    let (bluez, _fake) = start(vec![DeviceSpec::meter(METER)]).await;
    let transport = bluez.meter(METER, BluezConfig::default());
    let debug = format!("{transport:?}");
    assert!(debug.starts_with("BluezTransport"), "{debug}");
    assert!(debug.contains("00:11:22:33:44:55"), "{debug}");
    let scan = within(bluez.scan()).await.unwrap();
    assert!(format!("{scan:?}").starts_with("Scan"));
    within(scan.stop()).await.unwrap();
}

#[tokio::test]
async fn unknown_meter_is_discovered_before_connecting() {
    let (bluez, fake) = start(Vec::new()).await;
    fake.in_range(DeviceSpec::meter(METER));
    let mut transport = bluez.meter(METER, BluezConfig::default());

    within(transport.connect()).await.unwrap();
    assert_eq!(
        fake.calls(),
        vec![
            Call::SetDiscoveryFilter {
                transport: Some("le".into())
            },
            Call::StartDiscovery,
            Call::StopDiscovery,
            Call::Connect(METER),
        ]
    );
}

#[tokio::test]
async fn meter_out_of_range_is_not_found() {
    let (bluez, fake) = start(Vec::new()).await;
    let mut config = BluezConfig::default();
    config.discovery_timeout = Duration::from_millis(200);
    let mut transport = bluez.meter(METER, config);

    let error = within(transport.connect()).await.unwrap_err();
    assert!(
        matches!(bluez_error(&error), BluezError::NotFound(METER)),
        "{error}"
    );
    // What to do about it is for the application to say.
    assert_eq!(error.to_string(), "meter 00:11:22:33:44:55 not found");
    assert_eq!(fake.calls().last(), Some(&Call::StopDiscovery));
}

#[tokio::test]
async fn link_drop_is_reported_and_the_session_reconnects() {
    let (bluez, fake) = start(vec![DeviceSpec::meter(METER)]).await;
    let mut session = Session::new(
        bluez.meter(METER, BluezConfig::default()),
        SessionConfig::default(),
    );
    assert!(matches!(
        within(session.next_event()).await.unwrap(),
        SessionEvent::Connected { .. }
    ));

    fake.drop_link(METER).await;
    match within(session.next_event()).await.unwrap() {
        SessionEvent::Disconnected(DisconnectReason::LinkLost) => {}
        other => panic!("expected LinkLost, got {other:?}"),
    }
    assert!(matches!(
        within(session.next_event()).await.unwrap(),
        SessionEvent::Connected { .. }
    ));
    assert_eq!(fake.count(&Call::Connect(METER)), 2);
    assert_eq!(fake.count(&Call::StartNotify), 2);

    fake.notify(METER, common::frame(METER, 7)).await;
    assert_eq!(frame_value(within(session.next_event()).await.unwrap()), 7);
}

#[tokio::test]
async fn reconnects_whatever_bluez_does_with_the_notify_session() {
    // BlueZ may still be re-registering the kept session when the
    // transport asks again, or may have dropped the GATT objects.
    let late = DeviceSpec {
        late_reregister: true,
        ..DeviceSpec::meter(METER)
    };
    let uncached = DeviceSpec {
        gatt_cache: false,
        ..DeviceSpec::meter(METER)
    };
    for spec in [late, uncached] {
        let (bluez, fake) = start(vec![spec.clone()]).await;
        let mut session = Session::new(bluez.meter(METER, BluezConfig::default()), quick_retries());
        within(session.next_event()).await.unwrap();
        fake.drop_link(METER).await;
        within(session.next_event()).await.unwrap();
        match within(session.next_event()).await.unwrap() {
            SessionEvent::Connected { .. } => {}
            other => panic!("expected Connected, got {other:?} with {spec:?}"),
        }
        // Wait out a late re-registration.
        tokio::time::sleep(Duration::from_millis(300)).await;
        fake.notify(METER, common::frame(METER, 9)).await;
        assert_eq!(frame_value(within(session.next_event()).await.unwrap()), 9);
    }
}

#[tokio::test]
async fn a_replaced_adapter_is_found_again() {
    // An unplugged and replugged dongle comes back as hci1.
    let (bluez, fake) = start(vec![DeviceSpec::meter(METER)]).await;
    let mut session = Session::new(bluez.meter(METER, BluezConfig::default()), quick_retries());
    within(session.next_event()).await.unwrap();
    fake.drop_link(METER).await;
    within(session.next_event()).await.unwrap();
    fake.replug(vec![DeviceSpec::meter(METER)]).await;
    let connected = async {
        loop {
            match session.next_event().await.unwrap() {
                SessionEvent::Connected { .. } => break,
                SessionEvent::ConnectFailed(_) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
    };
    within(connected).await;
}

#[tokio::test]
async fn losing_bluez_ends_the_session() {
    // With the D-Bus connection gone, retrying cannot help.
    let (bluez, fake) = start(vec![DeviceSpec::meter(METER)]).await;
    let mut session = Session::new(bluez.meter(METER, BluezConfig::default()), quick_retries());
    within(session.next_event()).await.unwrap();
    fake.server.close().await.unwrap();
    let ended = async {
        loop {
            match session.next_event().await {
                Err(error) => break error,
                Ok(SessionEvent::Disconnected(_)) => {}
                Ok(other) => panic!("unexpected {other:?}"),
            }
        }
    };
    let error = within(ended).await;
    assert!(error.is_fatal(), "{error}");
}

#[tokio::test]
async fn overflow_is_counted_not_hidden() {
    let (bluez, fake) = start(vec![DeviceSpec::meter(METER)]).await;
    let mut config = BluezConfig::default();
    config.queue_capacity = NonZeroUsize::new(4).unwrap();
    let mut transport = bluez.meter(METER, config);
    within(transport.connect()).await.unwrap();
    within(transport.subscribe()).await.unwrap();

    for n in 0..10 {
        fake.notify(METER, common::frame(METER, n)).await;
    }
    // Give the transport time to take all ten before reading any.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(within(transport.next_event()).await, LinkEvent::Lost(6));
    for n in 0..4 {
        assert_eq!(
            within(transport.next_event()).await,
            LinkEvent::Notification(common::frame(METER, n))
        );
    }
}

#[tokio::test]
async fn disconnect_ends_the_notify_session_then_the_link() {
    let (bluez, fake) = start(vec![DeviceSpec::meter(METER)]).await;
    let mut transport = bluez.meter(METER, BluezConfig::default());
    within(transport.connect()).await.unwrap();
    within(transport.subscribe()).await.unwrap();
    within(transport.disconnect()).await.unwrap();
    let calls = fake.calls();
    assert_eq!(
        &calls[calls.len() - 2..],
        &[Call::StopNotify, Call::Disconnect(METER)]
    );
}

#[tokio::test]
async fn dropping_a_connected_transport_releases_the_meter() {
    let (bluez, fake) = start(vec![DeviceSpec::meter(METER)]).await;
    let mut transport = bluez.meter(METER, BluezConfig::default());
    within(transport.connect()).await.unwrap();
    drop(transport);
    within(fake.received(Call::Disconnect(METER))).await;
}

#[tokio::test]
async fn no_adapter_is_an_error() {
    let (server_socket, client_socket) = UnixStream::pair().unwrap();
    let server = Builder::unix_stream(server_socket)
        .server(Guid::generate())
        .p2p()
        .serve_at("/", fdo::ObjectManager)
        .build();
    let client = Builder::unix_stream(client_socket).p2p().build();
    let (_server, client) = tokio::join!(server, client);
    let result = Bluez::from_connection(client.unwrap()).await;
    assert!(matches!(result, Err(BluezError::NoAdapter)), "{result:?}");
}

#[tokio::test]
async fn disconnect_after_an_abandoned_connect_releases_the_meter() {
    // An app that quits mid-connect drops the connect future, then
    // disconnects. BlueZ may already hold the link by then.
    let spec = DeviceSpec {
        connect_delay: Duration::from_secs(2),
        ..DeviceSpec::meter(METER)
    };
    let (bluez, fake) = start(vec![spec]).await;
    let mut transport = bluez.meter(METER, BluezConfig::default());
    tokio::select! {
        _ = transport.connect() => panic!("connect should still be waiting"),
        _ = fake.received(Call::Connect(METER)) => {}
    }
    within(transport.disconnect()).await.unwrap();
    assert_eq!(fake.calls().last(), Some(&Call::Disconnect(METER)));
}
