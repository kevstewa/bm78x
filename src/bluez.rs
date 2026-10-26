//! A [`Transport`] over BlueZ, the Linux Bluetooth stack, spoken to over
//! D-Bus with `zbus`.
//!
//! [`Bluez::scan`] finds meters; [`Bluez::meter`] gives a transport for one.
//! Everything here needs a Tokio runtime.
//!
//! The D-Bus calls follow BlueZ's API documentation (`doc/org.bluez.*.rst`
//! in the BlueZ source). Notifications arrive as `PropertiesChanged` signals
//! on the characteristic's `Value`, which BlueZ emits for each one after
//! `StartNotify`.
//!
//! Signals are read as they arrive, by tasks of their own, whether or not
//! anyone is waiting for them: zbus stops reading the connection while any
//! signal stream is full, which would stall every call on it.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::pending;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout, timeout_at};
use zbus::fdo::{ManagedObjects, ObjectManagerProxy, PropertiesChanged, PropertiesProxy};
use zbus::message::Type as MessageType;
use zbus::proxy::CacheProperties;
use zbus::{
    Connection, DBusError as _, MatchRule, MessageStream, ObjectPath, OwnedObjectPath, Variant,
};

use crate::protocol::{
    COMMAND_UUID, MacAddress, NOTIFY_UUID, SERVICE_UUID, is_meter_manufacturer_data,
};
use crate::transport::{LinkEvent, Transport, TransportError};

use futures_util::StreamExt;

const BLUEZ: &str = "org.bluez";
const ADAPTER_IFACE: &str = "org.bluez.Adapter1";
const DEVICE_IFACE: &str = "org.bluez.Device1";
const CHARACTERISTIC_IFACE: &str = "org.bluez.GattCharacteristic1";

#[zbus::proxy(interface = "org.bluez.Adapter1", default_service = "org.bluez")]
trait Adapter1 {
    fn set_discovery_filter(&self, filter: HashMap<&str, Variant<'_>>) -> zbus::Result<()>;
    fn start_discovery(&self) -> zbus::Result<()>;
    fn stop_discovery(&self) -> zbus::Result<()>;
}

#[zbus::proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
trait Device1 {
    fn connect(&self) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn services_resolved(&self) -> zbus::Result<bool>;
}

#[zbus::proxy(
    interface = "org.bluez.GattCharacteristic1",
    default_service = "org.bluez"
)]
trait GattCharacteristic1 {
    fn read_value(&self, options: HashMap<&str, Variant<'_>>) -> zbus::Result<Vec<u8>>;
    fn write_value(&self, value: &[u8], options: HashMap<&str, Variant<'_>>) -> zbus::Result<()>;
    fn start_notify(&self) -> zbus::Result<()>;
    fn stop_notify(&self) -> zbus::Result<()>;
    #[zbus(property, name = "MTU")]
    fn mtu(&self) -> zbus::Result<u16>;
}

/// Why a BlueZ operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BluezError {
    /// BlueZ reports no Bluetooth adapter.
    #[error("no Bluetooth adapter found")]
    NoAdapter,
    /// The meter was not seen during discovery: it is out of range, its
    /// Bluetooth is off, or another device is connected to it.
    #[error("meter {0} not found")]
    NotFound(MacAddress),
    /// The meter connected but its GATT services were not resolved in time.
    #[error("the meter's services were not resolved in time")]
    ServicesTimeout,
    /// The link dropped while connecting.
    #[error("the meter disconnected while connecting")]
    Disconnected,
    /// The operation needs a connection and there is none.
    #[error("not connected")]
    NotConnected,
    /// The meter lacks a characteristic the protocol needs.
    #[error("the meter has no {0} characteristic")]
    MissingCharacteristic(&'static str),
    /// The D-Bus connection closed. Nothing more can be done with it, so a
    /// session gives up rather than retrying.
    #[error("the D-Bus connection closed")]
    Closed,
    /// A D-Bus call failed.
    #[error(transparent)]
    Dbus(DbusError),
}

/// A failed D-Bus call, such as BlueZ refusing a request.
#[derive(Debug)]
pub struct DbusError {
    error: zbus::Error,
    name: Option<String>,
}

impl DbusError {
    /// The D-Bus error name, such as `org.bluez.Error.Failed`, if the
    /// other side replied with an error.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The error reply's message, if it has one.
    pub fn message(&self) -> Option<&str> {
        match &self.error {
            zbus::Error::MethodError(_, message, _) => message.as_deref(),
            zbus::Error::FDO(error) => error.description(),
            _ => None,
        }
    }
}

impl fmt::Display for DbusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for DbusError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}

impl BluezError {
    fn from_zbus(error: zbus::Error) -> Self {
        use std::io::ErrorKind;
        let name = match &error {
            zbus::Error::MethodError(name, _, _) => Some(name.to_string()),
            zbus::Error::FDO(error) => Some(error.name().to_string()),
            zbus::Error::InputOutput(io) => match io.kind() {
                ErrorKind::BrokenPipe
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::NotConnected
                | ErrorKind::UnexpectedEof => return BluezError::Closed,
                _ => None,
            },
            _ => None,
        };
        BluezError::Dbus(DbusError { error, name })
    }
}

/// Turns a zbus result into this module's. Not a `From` impl, which would
/// make zbus's error types part of this crate's API.
trait OrDbus<T> {
    fn dbus(self) -> Result<T, BluezError>;
}

impl<T, E> OrDbus<T> for Result<T, E>
where
    zbus::Error: From<E>,
{
    fn dbus(self) -> Result<T, BluezError> {
        self.map_err(|error| BluezError::from_zbus(error.into()))
    }
}

impl From<BluezError> for TransportError {
    fn from(error: BluezError) -> Self {
        if matches!(error, BluezError::Closed) {
            TransportError::fatal(error)
        } else {
            TransportError::new(error)
        }
    }
}

impl BluezError {
    /// Whether BlueZ refused the call as already in progress.
    fn in_progress(&self) -> bool {
        matches!(self, BluezError::Dbus(e) if e.name() == Some("org.bluez.Error.InProgress"))
    }
}

/// A connection to BlueZ and one of its adapters.
#[derive(Clone)]
pub struct Bluez {
    connection: Connection,
    shared: Arc<Shared>,
}

/// What every clone of a [`Bluez`] shares.
struct Shared {
    /// The adapter in use. If BlueZ loses it, as when a dongle is
    /// unplugged, the first adapter it has takes its place.
    adapter: Mutex<OwnedObjectPath>,
    /// BlueZ gives each D-Bus client one discovery, so scans and searches
    /// for a meter share it.
    discovery: tokio::sync::Mutex<DiscoveryState>,
}

#[derive(Default)]
struct DiscoveryState {
    users: usize,
    adapter: Option<Adapter1Proxy<'static>>,
}

impl fmt::Debug for Bluez {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bluez")
            .field("adapter", &*self.shared.adapter.lock().unwrap())
            .finish_non_exhaustive()
    }
}

impl Bluez {
    /// Connects to BlueZ on the system bus and picks the first adapter.
    ///
    /// # Errors
    ///
    /// [`BluezError::Dbus`] if the system bus or BlueZ cannot be reached,
    /// [`BluezError::Closed`] if the bus connection closes, or
    /// [`BluezError::NoAdapter`].
    pub async fn system() -> Result<Self, BluezError> {
        Self::from_connection(Connection::system().await.dbus()?).await
    }

    /// Uses an existing `zbus` 6 connection, such as one to a fake BlueZ in
    /// tests, and picks the first adapter.
    ///
    /// # Errors
    ///
    /// [`BluezError::Dbus`] if BlueZ does not answer on `connection`,
    /// [`BluezError::Closed`] if the connection closes, or
    /// [`BluezError::NoAdapter`].
    pub async fn from_connection(connection: Connection) -> Result<Self, BluezError> {
        let objects = managed_objects(&connection).await?;
        let adapter = first_adapter(&objects).ok_or(BluezError::NoAdapter)?;
        Ok(Bluez {
            connection,
            shared: Arc::new(Shared {
                adapter: Mutex::new(adapter),
                discovery: Default::default(),
            }),
        })
    }

    /// Starts discovering meters. Call [`Scan::stop`] when done; dropping
    /// the scan stops it too, in the background.
    ///
    /// # Errors
    ///
    /// [`BluezError::Dbus`] if BlueZ refuses to discover,
    /// [`BluezError::Closed`] if the D-Bus connection closes, or
    /// [`BluezError::NoAdapter`] if the adapter is gone.
    pub async fn scan(&self) -> Result<Scan, BluezError> {
        // Listen before asking, so nothing found in between is missed.
        let added = device_added_stream(&self.connection).await?;
        let changed = device_changed_stream(&self.connection).await?;
        let objects = managed_objects(&self.connection).await?;
        let adapter = self.adapter(&objects)?;

        let state = Arc::new(ScanState::default());
        let mut devices = ScanDevices {
            adapter: adapter.clone(),
            devices: HashMap::new(),
            reported: HashMap::new(),
            state: state.clone(),
        };
        for (path, interfaces) in objects {
            if let Some(properties) = interfaces.get(DEVICE_IFACE) {
                let properties = properties.iter().map(|(k, v)| (k.as_str(), &**v));
                devices.update(path, properties);
            }
        }
        let task = tokio::spawn(devices.run(added, changed));
        let mut scan = Scan {
            state,
            task,
            discovery: None,
        };
        scan.discovery = Some(self.start_discovery(&adapter).await?);
        Ok(scan)
    }

    /// A transport for the meter at `address`. Nothing happens until the
    /// session connects.
    pub fn meter(&self, address: MacAddress, config: BluezConfig) -> BluezTransport {
        BluezTransport {
            bluez: self.clone(),
            address,
            config,
            device: None,
            link: None,
        }
    }

    /// The adapter to use: the one in use if BlueZ still has it, or else
    /// the first one it has.
    fn adapter(&self, objects: &ManagedObjects) -> Result<OwnedObjectPath, BluezError> {
        let mut current = self.shared.adapter.lock().unwrap();
        let present = objects
            .get(&*current)
            .is_some_and(|interfaces| interfaces.contains_key(ADAPTER_IFACE));
        if !present {
            *current = first_adapter(objects).ok_or(BluezError::NoAdapter)?;
        }
        Ok(current.clone())
    }

    /// Joins this connection's discovery, starting it on `adapter` if no
    /// one else has.
    async fn start_discovery(&self, adapter: &OwnedObjectPath) -> Result<Discovery, BluezError> {
        let mut state = self.shared.discovery.lock().await;
        state.users += 1;
        // Joined before asking BlueZ, so a start that fails or is abandoned
        // part-way is undone when this is dropped.
        let discovery = Discovery {
            shared: Some(self.shared.clone()),
        };
        if state.users == 1 {
            let proxy = Adapter1Proxy::builder(&self.connection)
                .path(adapter.clone())
                .cache_properties(CacheProperties::No)
                .build()
                .await
                .dbus()?;
            state.adapter = Some(proxy.clone());
            // LE only: the meter is a BLE device.
            proxy
                .set_discovery_filter(HashMap::from([("Transport", Variant::from("le"))]))
                .await
                .dbus()?;
            proxy.start_discovery().await.dbus()?;
        }
        Ok(discovery)
    }

    /// Finds the device object for `address`, discovering for up to
    /// `wait` if BlueZ does not know it yet.
    async fn find_device(
        &self,
        address: MacAddress,
        wait: Duration,
    ) -> Result<OwnedObjectPath, BluezError> {
        let objects = managed_objects(&self.connection).await?;
        let adapter = self.adapter(&objects)?;
        if let Some(path) = device_in(&objects, &adapter, address) {
            return Ok(path);
        }
        let mut added = device_added_stream(&self.connection).await?;
        let discovery = self.start_discovery(&adapter).await?;
        let found = async {
            // It may have appeared between the first look and the stream
            // starting.
            let objects = managed_objects(&self.connection).await?;
            if let Some(path) = device_in(&objects, &adapter, address) {
                return Ok(Some(path));
            }
            let deadline = Instant::now() + wait;
            while let Ok(Some(signal)) = timeout_at(deadline, added.next()).await {
                let args = signal.args().dbus()?;
                let is_it = args
                    .interfaces_and_properties()
                    .get(DEVICE_IFACE)
                    .and_then(|props| props.get("Address"))
                    .and_then(address_of)
                    == Some(address);
                if is_it && is_below(args.object_path(), &adapter) {
                    return Ok(Some(args.object_path().clone().into()));
                }
            }
            Ok::<_, BluezError>(None)
        }
        .await;
        // Stop whether or not it was found.
        discovery.stop().await?;
        found?.ok_or(BluezError::NotFound(address))
    }
}

/// A share of the connection's discovery. The last share to stop, or to be
/// dropped, stops BlueZ discovering.
struct Discovery {
    shared: Option<Arc<Shared>>,
}

impl Discovery {
    async fn stop(mut self) -> Result<(), BluezError> {
        match self.shared.take() {
            Some(shared) => leave_discovery(&shared).await,
            None => Ok(()),
        }
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = leave_discovery(&shared).await;
            });
        }
    }
}

async fn leave_discovery(shared: &Shared) -> Result<(), BluezError> {
    let mut state = shared.discovery.lock().await;
    state.users = state.users.saturating_sub(1);
    if state.users == 0
        && let Some(adapter) = state.adapter.take()
    {
        adapter.stop_discovery().await.dbus()?;
    }
    Ok(())
}

fn first_adapter(objects: &ManagedObjects) -> Option<OwnedObjectPath> {
    objects
        .iter()
        .filter(|(_, interfaces)| interfaces.contains_key(ADAPTER_IFACE))
        .map(|(path, _)| path.clone())
        .min_by(|a, b| a.as_str().cmp(b.as_str()))
}

/// Whether `path` is an object below `parent`, such as an adapter's device
/// or a device's characteristic.
fn is_below(path: &ObjectPath<'_>, parent: &ObjectPath<'_>) -> bool {
    path.as_str()
        .strip_prefix(parent.as_str())
        .is_some_and(|rest| rest.starts_with('/'))
}

/// The device object for `address` on `adapter`, if BlueZ has one.
fn device_in(
    objects: &ManagedObjects,
    adapter: &OwnedObjectPath,
    address: MacAddress,
) -> Option<OwnedObjectPath> {
    objects.iter().find_map(|(path, interfaces)| {
        let device = interfaces.get(DEVICE_IFACE)?;
        let found = address_of(device.get("Address")?)?;
        (found == address && is_below(path, adapter)).then(|| path.clone())
    })
}

async fn managed_objects(connection: &Connection) -> Result<ManagedObjects, BluezError> {
    let manager = ObjectManagerProxy::builder(connection)
        .destination(BLUEZ)
        .path("/")
        .build()
        .await
        .dbus()?;
    manager.get_managed_objects().await.dbus()
}

async fn device_added_stream(
    connection: &Connection,
) -> Result<zbus::fdo::InterfacesAddedStream, BluezError> {
    let manager = ObjectManagerProxy::builder(connection)
        .destination(BLUEZ)
        .path("/")
        .build()
        .await
        .dbus()?;
    manager.receive_interfaces_added().await.dbus()
}

/// `PropertiesChanged` for every device, on any adapter.
async fn device_changed_stream(connection: &Connection) -> Result<MessageStream, BluezError> {
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(BLUEZ)
        .interface("org.freedesktop.DBus.Properties")
        .member("PropertiesChanged")
        .path_namespace("/org/bluez")
        .arg(0, DEVICE_IFACE)
        .build()
        .dbus()?;
    MessageStream::for_match_rule(rule, connection, None)
        .await
        .dbus()
}

/// Reads a property value, or `None` if it has another type.
fn get<'a, T>(value: &Variant<'a>) -> Option<T>
where
    T: TryFrom<Variant<'a>>,
    T::Error: Into<zbus::Error>,
{
    value.clone().downcast().ok()
}

/// Reads an `Address` property.
fn address_of(value: &Variant<'_>) -> Option<MacAddress> {
    get::<String>(value)?.parse().ok()
}

/// A meter seen during a [`Scan`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Discovered {
    /// The meter's Bluetooth address.
    pub address: MacAddress,
    /// Its advertised name, which the user can change.
    pub name: Option<String>,
    /// Signal strength in dBm, if BlueZ has heard it during this
    /// discovery.
    pub rssi: Option<i16>,
}

impl Discovered {
    /// A meter with no name or signal strength known.
    pub fn new(address: MacAddress) -> Self {
        Discovered {
            address,
            name: None,
            rssi: None,
        }
    }
}

/// What a scan knows about one device object.
#[derive(Debug, Default)]
struct DeviceInfo {
    address: Option<MacAddress>,
    name: Option<String>,
    rssi: Option<i16>,
    manufacturer: HashMap<u16, Vec<u8>>,
    uuids: Vec<String>,
}

impl DeviceInfo {
    fn merge<'a>(&mut self, properties: impl IntoIterator<Item = (&'a str, &'a Variant<'a>)>) {
        for (name, value) in properties {
            match name {
                "Address" => self.address = address_of(value),
                "Name" => self.name = get(value),
                "RSSI" => self.rssi = get(value),
                "ManufacturerData" => {
                    let entries: HashMap<u16, Variant<'_>> = get(value).unwrap_or_default();
                    self.manufacturer = entries
                        .iter()
                        .filter_map(|(&company, data)| Some((company, get(data)?)))
                        .collect();
                }
                "UUIDs" => self.uuids = get(value).unwrap_or_default(),
                _ => {}
            }
        }
    }

    /// The device as a [`Discovered`] meter, if it is one. BlueZ merges
    /// every client's discovery filters, so each device is checked here.
    fn as_meter(&self) -> Option<Discovered> {
        let by_data = self
            .manufacturer
            .iter()
            .any(|(&company, data)| is_meter_manufacturer_data(company, data));
        let by_service = self
            .uuids
            .iter()
            .any(|u| u.eq_ignore_ascii_case(SERVICE_UUID));
        if !(by_data || by_service) {
            return None;
        }
        Some(Discovered {
            address: self.address?,
            name: self.name.clone(),
            rssi: self.rssi,
        })
    }
}

/// A running discovery.
pub struct Scan {
    state: Arc<ScanState>,
    task: JoinHandle<()>,
    discovery: Option<Discovery>,
}

impl fmt::Debug for Scan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scan")
            .field("discovering", &self.discovery.is_some())
            .finish_non_exhaustive()
    }
}

/// Reports waiting for [`Scan::next`].
#[derive(Default)]
struct ScanState {
    reports: Mutex<Reports>,
    changed: Notify,
}

/// At most one report per meter, the latest, in the order they came.
#[derive(Default)]
struct Reports {
    order: VecDeque<OwnedObjectPath>,
    latest: HashMap<OwnedObjectPath, Discovered>,
    error: Option<BluezError>,
    ended: bool,
}

/// The scan task's view of every device under the adapter.
struct ScanDevices {
    adapter: OwnedObjectPath,
    devices: HashMap<OwnedObjectPath, DeviceInfo>,
    reported: HashMap<OwnedObjectPath, Discovered>,
    state: Arc<ScanState>,
}

impl ScanDevices {
    async fn run(
        mut self,
        mut added: zbus::fdo::InterfacesAddedStream,
        mut changed: MessageStream,
    ) {
        let error = loop {
            tokio::select! {
                signal = added.next() => {
                    let Some(signal) = signal else { break BluezError::Closed };
                    let Ok(args) = signal.args() else { continue };
                    if let Some(properties) = args.interfaces_and_properties().get(DEVICE_IFACE) {
                        let path = args.object_path().clone().into();
                        let properties = properties.iter().map(|(k, v)| (*k, v));
                        self.update(path, properties);
                    }
                }
                message = changed.next() => {
                    let message = match message {
                        Some(Ok(message)) => message,
                        Some(Err(error)) => break BluezError::from_zbus(error),
                        None => break BluezError::Closed,
                    };
                    let Some(path) = message.header().path().map(|p| p.to_owned().into()) else {
                        continue;
                    };
                    let Some(signal) = PropertiesChanged::from_message(message) else {
                        continue;
                    };
                    let Ok(args) = signal.args() else { continue };
                    let properties = args.changed_properties().iter().map(|(k, v)| (*k, v));
                    self.update(path, properties);
                }
            }
        };
        let mut reports = self.state.reports.lock().unwrap();
        reports.error = Some(error);
        reports.ended = true;
        drop(reports);
        self.state.changed.notify_one();
    }

    /// Records new properties for a device, and queues a report if it is a
    /// meter on the adapter whose report would change.
    fn update<'a>(
        &mut self,
        path: OwnedObjectPath,
        properties: impl IntoIterator<Item = (&'a str, &'a Variant<'a>)>,
    ) {
        if !is_below(&path, &self.adapter) {
            return;
        }
        let device = self.devices.entry(path.clone()).or_default();
        device.merge(properties);
        if let Some(found) = device.as_meter()
            && self.reported.get(&path) != Some(&found)
        {
            self.reported.insert(path.clone(), found.clone());
            let mut reports = self.state.reports.lock().unwrap();
            if reports.latest.insert(path.clone(), found).is_none() {
                reports.order.push_back(path);
            }
            drop(reports);
            self.state.changed.notify_one();
        }
    }
}

impl Scan {
    /// Waits for a meter to appear or for its name or signal strength to
    /// change. Meters BlueZ already knew about are reported first. Changes
    /// that come faster than this is called are merged, so a scan read
    /// slowly, or not at all, holds one report per meter.
    ///
    /// # Errors
    ///
    /// The error that ended the scan, such as [`BluezError::Closed`] when
    /// the D-Bus connection closes. Once it has failed, the scan stays
    /// failed.
    pub async fn next(&mut self) -> Result<Discovered, BluezError> {
        loop {
            {
                let mut reports = self.state.reports.lock().unwrap();
                if let Some(path) = reports.order.pop_front() {
                    return Ok(reports.latest.remove(&path).expect("queued with a report"));
                }
                if reports.ended {
                    return Err(reports.error.take().unwrap_or(BluezError::Closed));
                }
            }
            self.state.changed.notified().await;
        }
    }

    /// Stops discovery.
    ///
    /// # Errors
    ///
    /// [`BluezError::Dbus`] if BlueZ refuses to stop discovering, or
    /// [`BluezError::Closed`].
    pub async fn stop(mut self) -> Result<(), BluezError> {
        self.task.abort();
        match self.discovery.take() {
            Some(discovery) => discovery.stop().await,
            None => Ok(()),
        }
    }
}

impl Drop for Scan {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// How a [`BluezTransport`] behaves. Start from [`BluezConfig::default`]
/// and set the fields that matter.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BluezConfig {
    /// How long to discover for when BlueZ does not know the meter yet.
    pub discovery_timeout: Duration,
    /// How long to wait after connecting for the GATT services.
    pub services_timeout: Duration,
    /// How many notifications to hold for a slow reader before dropping.
    pub queue_capacity: NonZeroUsize,
}

impl Default for BluezConfig {
    fn default() -> Self {
        BluezConfig {
            discovery_timeout: Duration::from_secs(10),
            services_timeout: Duration::from_secs(10),
            // About three minutes of frames at five a second.
            queue_capacity: NonZeroUsize::new(1024).unwrap(),
        }
    }
}

/// A [`Transport`] to one meter through BlueZ.
///
/// Dropping it while it holds the meter asks BlueZ to disconnect in the
/// background, which needs the Tokio runtime to keep running a moment.
/// [`Transport::disconnect`], or
/// [`Session::close`](crate::session::Session::close), does it for certain.
pub struct BluezTransport {
    bluez: Bluez,
    address: MacAddress,
    config: BluezConfig,
    /// The device, from the moment `Connect` is called. Kept apart from
    /// `link` so a connect abandoned part-way can still be undone.
    device: Option<Device1Proxy<'static>>,
    link: Option<Link>,
}

impl fmt::Debug for BluezTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BluezTransport")
            .field("address", &self.address)
            .field("config", &self.config)
            .field("connected", &self.link.is_some())
            .finish_non_exhaustive()
    }
}

/// One connection's worth of state. Dropping it stops its tasks.
struct Link {
    notify: GattCharacteristic1Proxy<'static>,
    command: GattCharacteristic1Proxy<'static>,
    mtu: Option<u16>,
    /// For the notification task; `subscribe` takes it.
    notify_sender: Option<mpsc::Sender<LinkEvent>>,
    events: mpsc::Receiver<LinkEvent>,
    lost: Arc<AtomicU64>,
    tasks: Vec<Task>,
}

/// A task that is aborted when dropped.
struct Task(JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Drop for BluezTransport {
    fn drop(&mut self) {
        if let Some(device) = self.device.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = device.disconnect().await;
            });
        }
    }
}

/// What the device's signals have said since `Connect`.
#[derive(Debug, Clone, Copy, Default)]
struct DeviceState {
    services_resolved: bool,
    disconnected: bool,
}

/// The notify and command characteristics, and the MTU.
type Characteristics = (
    GattCharacteristic1Proxy<'static>,
    GattCharacteristic1Proxy<'static>,
    Option<u16>,
);

impl BluezTransport {
    /// The ATT MTU BlueZ negotiated, once connected, if BlueZ reports it.
    /// Brymen requires 185 for a frame to fit one notification.
    pub fn mtu(&self) -> Option<u16> {
        self.link.as_ref()?.mtu
    }

    fn link(&self) -> Result<&Link, BluezError> {
        self.link.as_ref().ok_or(BluezError::NotConnected)
    }

    async fn open(&mut self) -> Result<(), BluezError> {
        let connection = self.bluez.connection.clone();
        let path = self
            .bluez
            .find_device(self.address, self.config.discovery_timeout)
            .await?;
        let device = Device1Proxy::builder(&connection)
            .path(path.clone())
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .dbus()?;
        // Watch the device before connecting, so no change is missed, and
        // from a task of its own, so its signals never back up.
        let changes = PropertiesProxy::builder(&connection)
            .destination(BLUEZ)
            .path(path.clone())
            .build()
            .await
            .dbus()?
            .receive_properties_changed()
            .await
            .dbus()?;
        let (sender, events) = mpsc::channel(self.config.queue_capacity.get());
        let (state, mut watched) = watch::channel(DeviceState::default());
        let watcher = Task(tokio::spawn(watch_device(changes, state, sender.clone())));

        self.device = Some(device.clone());
        device.connect().await.dbus()?;
        match self.ready(&device, &mut watched, &path).await {
            Ok((notify, command, mtu)) => {
                self.link = Some(Link {
                    notify,
                    command,
                    mtu,
                    notify_sender: Some(sender),
                    events,
                    lost: Arc::new(AtomicU64::new(0)),
                    tasks: vec![watcher],
                });
                Ok(())
            }
            Err(error) => {
                // Release the half-made link: the meter takes one
                // connection at a time.
                let _ = device.disconnect().await;
                Err(error)
            }
        }
    }

    /// Waits for the GATT services and finds the two characteristics.
    async fn ready(
        &self,
        device: &Device1Proxy<'static>,
        watched: &mut watch::Receiver<DeviceState>,
        path: &OwnedObjectPath,
    ) -> Result<Characteristics, BluezError> {
        if !device.services_resolved().await.dbus()? {
            let state = timeout(
                self.config.services_timeout,
                watched.wait_for(|s| s.services_resolved || s.disconnected),
            )
            .await
            .map_err(|_| BluezError::ServicesTimeout)?
            .map_err(|_| BluezError::Closed)?;
            if state.disconnected {
                return Err(BluezError::Disconnected);
            }
        }

        let connection = &self.bluez.connection;
        let objects = managed_objects(connection).await?;
        let find = |uuid: &str| {
            objects.iter().find_map(|(object, interfaces)| {
                let characteristic = interfaces.get(CHARACTERISTIC_IFACE)?;
                let found: String = get(characteristic.get("UUID")?)?;
                (is_below(object, path) && found.eq_ignore_ascii_case(uuid)).then(|| object.clone())
            })
        };
        let notify_path = find(NOTIFY_UUID).ok_or(BluezError::MissingCharacteristic("notify"))?;
        let command_path =
            find(COMMAND_UUID).ok_or(BluezError::MissingCharacteristic("command"))?;
        let characteristic = |path: OwnedObjectPath| async move {
            GattCharacteristic1Proxy::builder(connection)
                .path(path)
                .cache_properties(CacheProperties::No)
                .build()
                .await
        };
        let notify = characteristic(notify_path).await.dbus()?;
        let command = characteristic(command_path).await.dbus()?;
        // BlueZ before 5.62 has no MTU property.
        let mtu = notify.mtu().await.ok();
        Ok((notify, command, mtu))
    }
}

/// Follows the device's properties: tells the connect when the services
/// resolve, and the reader when the link drops. Waits for room in the
/// queue rather than losing that event.
async fn watch_device(
    mut changes: zbus::fdo::PropertiesChangedStream,
    state: watch::Sender<DeviceState>,
    sender: mpsc::Sender<LinkEvent>,
) {
    while let Some(signal) = changes.next().await {
        let Ok(args) = signal.args() else { continue };
        let changed = args.changed_properties();
        let flag = |name| {
            changed
                .get(name)
                .and_then(|v| v.downcast_ref::<bool>().ok())
        };
        if flag("ServicesResolved") == Some(true) {
            state.send_modify(|s| s.services_resolved = true);
        }
        if flag("Connected") == Some(false) {
            state.send_modify(|s| s.disconnected = true);
            break;
        }
    }
    let _ = sender.send(LinkEvent::Disconnected).await;
}

/// Queues each notification, counting those that do not fit rather than
/// waiting: a stalled reader must not stall BlueZ's signals.
async fn forward_notifications(
    mut changes: zbus::fdo::PropertiesChangedStream,
    sender: mpsc::Sender<LinkEvent>,
    lost: Arc<AtomicU64>,
) {
    while let Some(signal) = changes.next().await {
        let Ok(args) = signal.args() else { continue };
        let Some(value) = args.changed_properties().get("Value") else {
            continue;
        };
        let Some(bytes) = get::<Vec<u8>>(value) else {
            continue;
        };
        match sender.try_send(LinkEvent::Notification(bytes)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                lost.fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => break,
        }
    }
}

impl Transport for BluezTransport {
    fn address(&self) -> MacAddress {
        self.address
    }

    async fn connect(&mut self) -> Result<(), TransportError> {
        // A reconnect starts from scratch.
        self.link = None;
        Ok(self.open().await?)
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        if let Some(link) = self.link.take()
            && link.notify_sender.is_none()
        {
            // End the notify session, which BlueZ would otherwise keep for
            // the next connection. The link may be gone already.
            let _ = link.notify.stop_notify().await;
        }
        if let Some(device) = self.device.take() {
            device.disconnect().await.dbus()?;
        }
        Ok(())
    }

    async fn write_command(&mut self, packet: &[u8; 32]) -> Result<(), TransportError> {
        // "request" is a write with response, so BlueZ returns only once the
        // meter has acknowledged it.
        let options = HashMap::from([("type", Variant::from("request"))]);
        let command = &self.link()?.command;
        command.write_value(packet, options).await.dbus()?;
        Ok(())
    }

    async fn read_command(&mut self) -> Result<Vec<u8>, TransportError> {
        let command = &self.link()?.command;
        Ok(command.read_value(HashMap::new()).await.dbus()?)
    }

    async fn subscribe(&mut self) -> Result<(), TransportError> {
        let connection = self.bluez.connection.clone();
        let link = self.link.as_mut().ok_or(BluezError::NotConnected)?;
        let sender = link.notify_sender.take().ok_or(BluezError::NotConnected)?;
        // Listen before enabling, so the first notification is not missed.
        let changes = async {
            PropertiesProxy::builder(&connection)
                .destination(BLUEZ)
                .path(link.notify.inner().path().to_owned())
                .build()
                .await?
                .receive_properties_changed()
                .await
        }
        .await
        .dbus()?;
        let forward = forward_notifications(changes, sender, link.lost.clone());
        link.tasks.push(Task(tokio::spawn(forward)));
        match link.notify.start_notify().await.dbus() {
            // BlueZ keeps a notify session across a disconnect and
            // re-registers it once the link is back; until then it says the
            // session is in progress.
            Err(error) if error.in_progress() => Ok(()),
            result => Ok(result?),
        }
    }

    async fn next_event(&mut self) -> LinkEvent {
        let Some(link) = self.link.as_mut() else {
            return pending().await;
        };
        let lost = link.lost.swap(0, Ordering::Relaxed);
        if lost > 0 {
            return LinkEvent::Lost(lost);
        }
        // The queue ends once every task feeding it has, which leaves no
        // way to hear from the meter.
        link.events.recv().await.unwrap_or(LinkEvent::Disconnected)
    }
}
