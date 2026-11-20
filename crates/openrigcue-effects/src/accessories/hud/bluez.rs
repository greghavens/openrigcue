//! The OBD2HUD's BLE stack on BlueZ (`org.bluez` on the system D-Bus):
//! an LE discovery filter, five seconds of discovery, `Device1.Connect` and the wait for
//! `ServicesResolved`, the GATT objects as services, `ReadValue` / `WriteValue` with `type` `request` or
//! `command`, `StartNotify` with the `Value` changes as notifications. Pure Rust (zbus); nothing is opened
//! until the first discovery. Signal subscriptions exist only while something reads them (the discovery
//! window, and a notifying characteristic from `start_notify` on), so an unread queue can never fill up and
//! stall the shared connection.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{FutureExt, StreamExt};
use zbus::blocking::Connection;
use zbus::fdo::ManagedObjects;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{MatchRule, MessageStream};

use super::{Advertiser, BleError, Central, Characteristic, Peripheral, Service};

const BLUEZ: &str = "org.bluez";
const ADAPTER: &str = "org.bluez.Adapter1";
const DEVICE: &str = "org.bluez.Device1";
const GATT_SERVICE: &str = "org.bluez.GattService1";
const GATT_CHARACTERISTIC: &str = "org.bluez.GattCharacteristic1";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";
/// Queue length of a signal subscription; every subscription is drained at least every [`POLL`].
const SIGNAL_QUEUE: usize = 1024;

/// Length of one discovery.
pub const DISCOVERY_TIME: Duration = Duration::from_secs(5);
/// Time limit of the connection and the service resolution; also the limit of every D-Bus
/// call of this transport.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How often waits look at the stop flag and at `ServicesResolved`.
const POLL: Duration = Duration::from_millis(50);

fn ble(error: impl std::fmt::Display) -> BleError {
    BleError(error.to_string())
}

/// The system bus and its first Bluetooth adapter.
pub struct BluezCentral {
    stop: Arc<AtomicBool>,
    bus: Option<Connection>,
    /// The adapter and the device object of every address of the last discovery.
    adapter: String,
    devices: HashMap<String, String>,
}

impl BluezCentral {
    /// A central that connects to the system bus at its first discovery; `stop` ends its waits.
    pub fn new(stop: Arc<AtomicBool>) -> BluezCentral {
        BluezCentral { stop, bus: None, adapter: String::new(), devices: HashMap::new() }
    }

    fn bus(&mut self) -> Result<Connection, BleError> {
        if let Some(bus) = &self.bus {
            return Ok(bus.clone());
        }
        let bus = zbus::blocking::connection::Builder::system()
            .map_err(ble)?
            .method_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(ble)?;
        self.bus = Some(bus.clone());
        Ok(bus)
    }

    /// Waits `duration` unless the service stops first (`false`).
    fn wait(&self, duration: Duration) -> bool {
        self.wait_polling(duration, || {})
    }

    /// [`Self::wait`], calling `poll` at every [`POLL`] step and once at the end.
    fn wait_polling(&self, duration: Duration, mut poll: impl FnMut()) -> bool {
        let end = Instant::now() + duration;
        while Instant::now() < end {
            poll();
            if self.stop.load(Ordering::SeqCst) {
                return false;
            }
            std::thread::sleep(POLL.min(end.saturating_duration_since(Instant::now())));
        }
        poll();
        !self.stop.load(Ordering::SeqCst)
    }
}

impl Central for BluezCentral {
    fn discover(&mut self) -> Result<Vec<Advertiser>, BleError> {
        let bus = match self.bus() {
            Ok(bus) => bus,
            Err(error) => {
                // A bus that went away is reopened at the next scan.
                self.bus = None;
                return Err(error);
            }
        };
        let objects = managed_objects(&bus).inspect_err(|_| self.bus = None)?;
        self.adapter = first_adapter(&objects).ok_or_else(|| BleError("No Bluetooth adapters found.".into()))?;
        // Every BlueZ signal of the discovery window, in arrival order: the discovery lists its devices
        // in first-seen order.
        let rule = MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(BLUEZ).map_err(ble)?.build();
        let mut signals =
            zbus::block_on(MessageStream::for_match_rule(rule, bus.inner(), Some(SIGNAL_QUEUE))).map_err(ble)?;
        let mut first_seen: Vec<String> = Vec::new();
        let mut drain = |signals: &mut MessageStream| {
            while let Some(Some(message)) = signals.next().now_or_never() {
                if let Some(device) = message.ok().and_then(|m| advertised_device(&m, &self.adapter)) {
                    if !first_seen.contains(&device) {
                        first_seen.push(device);
                    }
                }
            }
        };
        let filter: HashMap<&str, Value> =
            HashMap::from([("Transport", Value::from("le")), ("DuplicateData", Value::from(false))]);
        call(&bus, &self.adapter, ADAPTER, "SetDiscoveryFilter", &(filter,))?;
        call(&bus, &self.adapter, ADAPTER, "StartDiscovery", &())?;
        let completed = self.wait_polling(DISCOVERY_TIME, || drain(&mut signals));
        let seen = managed_objects(&bus).map(|objects| discovered_devices(&objects, &self.adapter));
        let _ = call(&bus, &self.adapter, ADAPTER, "StopDiscovery", &());
        // Signals that arrived before the `GetManagedObjects` reply are queued by now.
        drain(&mut signals);
        drop(signals);
        if !completed {
            return Err(BleError("stopped".into()));
        }
        let seen = in_first_seen_order(seen?, &first_seen);
        self.devices = seen.iter().map(|(path, device)| (device.address.clone(), path.clone())).collect();
        Ok(seen.into_iter().map(|(_, device)| device).collect())
    }

    fn connect(&mut self, address: &str) -> Result<Box<dyn Peripheral>, BleError> {
        let bus = self.bus()?;
        let path = self
            .devices
            .get(address)
            .cloned()
            .ok_or_else(|| BleError(format!("Device with address {address} was not found.")))?;
        let started = Instant::now();
        call(&bus, &path, DEVICE, "Connect", &())?;
        loop {
            if property(&bus, &path, DEVICE, "ServicesResolved").ok().and_then(as_bool) == Some(true) {
                break;
            }
            if started.elapsed() >= CONNECT_TIMEOUT || !self.wait(POLL) {
                let _ = call(&bus, &path, DEVICE, "Disconnect", &());
                return Err(BleError(format!("Device with address {address} did not resolve its services")));
            }
        }
        Ok(Box::new(BluezPeripheral { bus, path, notifying: None }))
    }
}

/// A connected device.
struct BluezPeripheral {
    bus: Connection,
    path: String,
    /// The notifying characteristic and its `PropertiesChanged` since `start_notify`.
    notifying: Option<(String, MessageStream)>,
}

impl BluezPeripheral {
    fn characteristic_path(&mut self, uuid: &str) -> Result<String, BleError> {
        self.services()?
            .into_iter()
            .flat_map(|s| s.characteristics)
            .find(|c| c.uuid == uuid)
            .map(|c| c.handle)
            .ok_or_else(|| BleError(format!("Characteristic with UUID {uuid} could not be found!")))
    }
}

impl Peripheral for BluezPeripheral {
    fn services(&mut self) -> Result<Vec<Service>, BleError> {
        Ok(gatt_services(&managed_objects(&self.bus)?, &self.path))
    }

    fn read(&mut self, uuid: &str) -> Result<Vec<u8>, BleError> {
        let path = self.characteristic_path(uuid)?;
        let options: HashMap<&str, Value> = HashMap::new();
        let reply = call(&self.bus, &path, GATT_CHARACTERISTIC, "ReadValue", &(options,))?;
        reply.body().deserialize::<Vec<u8>>().map_err(ble)
    }

    fn write(&mut self, characteristic: &Characteristic, data: &[u8], with_response: bool) -> Result<(), BleError> {
        let options: HashMap<&str, Value> =
            HashMap::from([("type", Value::from(if with_response { "request" } else { "command" }))]);
        call(&self.bus, &characteristic.handle, GATT_CHARACTERISTIC, "WriteValue", &(data, options)).map(drop)
    }

    fn start_notify(&mut self, characteristic: &Characteristic) -> Result<(), BleError> {
        // Subscribed before `StartNotify`, so that no notification after it is missed.
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(BLUEZ)
            .and_then(|r| r.interface(PROPERTIES))
            .and_then(|r| r.member("PropertiesChanged"))
            .and_then(|r| r.path(characteristic.handle.as_str()))
            .map_err(ble)?
            .build();
        let signals =
            zbus::block_on(MessageStream::for_match_rule(rule, self.bus.inner(), Some(SIGNAL_QUEUE))).map_err(ble)?;
        call(&self.bus, &characteristic.handle, GATT_CHARACTERISTIC, "StartNotify", &())?;
        self.notifying = Some((characteristic.handle.clone(), signals));
        Ok(())
    }

    fn take_notification(&mut self) -> bool {
        let Some((handle, signals)) = &mut self.notifying else { return false };
        let mut notified = false;
        while let Some(Some(message)) = signals.next().now_or_never() {
            if let Ok(message) = message {
                notified |= is_value_notification(&message, handle);
            }
        }
        notified
    }

    fn is_connected(&mut self) -> bool {
        property(&self.bus, &self.path, DEVICE, "Connected").ok().and_then(as_bool) == Some(true)
    }

    fn disconnect(&mut self) -> Result<(), BleError> {
        call(&self.bus, &self.path, DEVICE, "Disconnect", &()).map(drop)
    }
}

fn call<B>(bus: &Connection, path: &str, interface: &str, method: &str, body: &B) -> Result<zbus::Message, BleError>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    bus.call_method(Some(BLUEZ), path, Some(interface), method, body).map_err(ble)
}

fn managed_objects(bus: &Connection) -> Result<ManagedObjects, BleError> {
    let reply = bus
        .call_method(Some(BLUEZ), "/", Some("org.freedesktop.DBus.ObjectManager"), "GetManagedObjects", &())
        .map_err(ble)?;
    reply.body().deserialize::<ManagedObjects>().map_err(ble)
}

fn property(bus: &Connection, path: &str, interface: &str, name: &str) -> Result<OwnedValue, BleError> {
    let reply = call(bus, path, PROPERTIES, "Get", &(interface, name))?;
    reply.body().deserialize::<OwnedValue>().map_err(ble)
}

fn as_bool(value: OwnedValue) -> Option<bool> {
    match &*value {
        Value::Bool(b) => Some(*b),
        Value::Value(inner) => match &**inner {
            Value::Bool(b) => Some(*b),
            _ => None,
        },
        _ => None,
    }
}

fn as_str(value: &Value<'_>) -> Option<String> {
    match value {
        Value::Str(s) => Some(s.as_str().to_owned()),
        Value::ObjectPath(p) => Some(p.as_str().to_owned()),
        Value::Value(inner) => as_str(inner),
        _ => None,
    }
}

type Properties = HashMap<String, OwnedValue>;

/// The device object of `adapter` that a BlueZ signal reports as seen: an `InterfacesAdded` with `Device1`
/// or a `Device1` `PropertiesChanged`.
pub fn advertised_device(message: &zbus::Message, adapter: &str) -> Option<String> {
    let header = message.header();
    let is_device = |path: &str| {
        path.strip_prefix(adapter).and_then(|rest| rest.strip_prefix('/')).is_some_and(|rest| !rest.contains('/'))
    };
    match (header.interface()?.as_str(), header.member()?.as_str()) {
        (OBJECT_MANAGER, "InterfacesAdded") => {
            let (path, interfaces) =
                message.body().deserialize::<(zbus::zvariant::OwnedObjectPath, HashMap<String, Properties>)>().ok()?;
            (interfaces.contains_key(DEVICE) && is_device(path.as_str())).then(|| path.as_str().to_owned())
        }
        (PROPERTIES, "PropertiesChanged") => {
            let path = header.path()?.as_str();
            if !is_device(path) {
                return None;
            }
            let (interface, _, _) = message.body().deserialize::<(String, Properties, Vec<String>)>().ok()?;
            (interface == DEVICE).then(|| path.to_owned())
        }
        _ => None,
    }
}

/// A `PropertiesChanged` of `characteristic` that carries a new `Value`: a notification.
fn is_value_notification(message: &zbus::Message, characteristic: &str) -> bool {
    if message.header().path().map(|p| p.as_str()) != Some(characteristic) {
        return false;
    }
    matches!(
        message.body().deserialize::<(String, Properties, Vec<String>)>(),
        Ok((interface, changed, _)) if interface == GATT_CHARACTERISTIC && changed.contains_key("Value")
    )
}

/// The `seen` devices in the order the discovery's signals first reported them; devices that had an RSSI
/// without any signal in the window (another client's discovery was already running) follow in object
/// path order.
pub fn in_first_seen_order(mut seen: Vec<(String, Advertiser)>, first_seen: &[String]) -> Vec<(String, Advertiser)> {
    seen.sort_by_key(|(path, _)| first_seen.iter().position(|p| p == path).unwrap_or(usize::MAX));
    seen
}

/// The properties of `interface` on every object, by object path, sorted.
fn objects_with<'a>(objects: &'a ManagedObjects, interface: &str) -> Vec<(&'a str, &'a Properties)> {
    let mut found: Vec<(&str, &Properties)> = objects
        .iter()
        .filter_map(|(path, interfaces)| {
            interfaces.iter().find(|(name, _)| name.as_str() == interface).map(|(_, props)| (path.as_str(), props))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(b.0));
    found
}

fn text(props: &Properties, name: &str) -> Option<String> {
    props.get(name).and_then(|v| as_str(v))
}

/// The default adapter: the first `Adapter1` object.
pub fn first_adapter(objects: &ManagedObjects) -> Option<String> {
    objects_with(objects, ADAPTER).first().map(|(path, _)| (*path).to_owned())
}

/// The devices of `adapter` seen by the running discovery (BlueZ only has their `RSSI` then), with
/// `Address` and `Alias` (used as the device name).
pub fn discovered_devices(objects: &ManagedObjects, adapter: &str) -> Vec<(String, Advertiser)> {
    let prefix = format!("{adapter}/");
    objects_with(objects, DEVICE)
        .into_iter()
        .filter(|(path, props)| path.starts_with(&prefix) && props.contains_key("RSSI"))
        .filter_map(|(path, props)| {
            let address = text(props, "Address")?;
            Some((path.to_owned(), Advertiser { address, name: text(props, "Alias") }))
        })
        .collect()
}

/// The GATT services of `device`, in handle (object path) order, with their characteristics.
pub fn gatt_services(objects: &ManagedObjects, device: &str) -> Vec<Service> {
    let prefix = format!("{device}/");
    let characteristics = objects_with(objects, GATT_CHARACTERISTIC);
    objects_with(objects, GATT_SERVICE)
        .into_iter()
        .filter(|(path, _)| path.starts_with(&prefix))
        .filter_map(|(path, props)| {
            let uuid = text(props, "UUID")?.to_lowercase();
            let characteristics = characteristics
                .iter()
                .filter(|(_, c)| text(c, "Service").as_deref() == Some(path))
                .filter_map(|(c_path, c)| {
                    Some(Characteristic { uuid: text(c, "UUID")?.to_lowercase(), handle: (*c_path).to_owned() })
                })
                .collect();
            Some(Service { uuid, characteristics })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::names::OwnedInterfaceName;
    use zbus::zvariant::OwnedObjectPath;

    fn object(interface: &str, props: &[(&str, Value<'static>)]) -> (OwnedInterfaceName, Properties) {
        (
            OwnedInterfaceName::try_from(interface).unwrap(),
            props
                .iter()
                .map(|(k, v)| ((*k).to_owned(), OwnedValue::try_from(v.try_clone().unwrap()).unwrap()))
                .collect(),
        )
    }

    fn tree(entries: Vec<(&str, Vec<(OwnedInterfaceName, Properties)>)>) -> ManagedObjects {
        entries
            .into_iter()
            .map(|(path, ifaces)| (OwnedObjectPath::try_from(path).unwrap(), ifaces.into_iter().collect()))
            .collect()
    }

    /// A `GetManagedObjects` reply as BlueZ gives it (no bus involved): the first adapter, the devices that
    /// have an RSSI in this discovery with their Alias as name, the HUD's services and characteristics.
    #[test]
    fn bluez_objects_are_read_like_bleak() {
        let hud = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
        let objects = tree(vec![
            ("/org/bluez/hci1", vec![object(ADAPTER, &[])]),
            ("/org/bluez/hci0", vec![object(ADAPTER, &[])]),
            (
                hud,
                vec![object(
                    DEVICE,
                    &[
                        ("Address", Value::from("AA:BB:CC:DD:EE:FF")),
                        ("Alias", Value::from("OBD2HUD-01")),
                        ("RSSI", Value::from(-60i16)),
                    ],
                )],
            ),
            (
                "/org/bluez/hci0/dev_11_22_33_44_55_66",
                vec![object(DEVICE, &[("Address", Value::from("11:22:33:44:55:66")), ("Alias", Value::from("old"))])],
            ),
            (
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0010",
                vec![object(GATT_SERVICE, &[("UUID", Value::from("C14FCC20-72F1-11EC-90D6-0242AC120003"))])],
            ),
            (
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0010/char0014",
                vec![object(
                    GATT_CHARACTERISTIC,
                    &[
                        ("UUID", Value::from(super::super::FIRMWARE_FILE_CHARACTERISTIC)),
                        (
                            "Service",
                            Value::from(
                                zbus::zvariant::ObjectPath::try_from(
                                    "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0010",
                                )
                                .unwrap(),
                            ),
                        ),
                    ],
                )],
            ),
            (
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0010/char0011",
                vec![object(
                    GATT_CHARACTERISTIC,
                    &[
                        ("UUID", Value::from(super::super::SENDER_CHARACTERISTIC)),
                        (
                            "Service",
                            Value::from(
                                zbus::zvariant::ObjectPath::try_from(
                                    "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0010",
                                )
                                .unwrap(),
                            ),
                        ),
                    ],
                )],
            ),
        ]);
        assert_eq!(first_adapter(&objects).as_deref(), Some("/org/bluez/hci0"));
        let devices = discovered_devices(&objects, "/org/bluez/hci0");
        assert_eq!(
            devices,
            [(hud.to_owned(), Advertiser { address: "AA:BB:CC:DD:EE:FF".into(), name: Some("OBD2HUD-01".into()) })]
        );
        let services = gatt_services(&objects, hud);
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].uuid, super::super::HUD_SERVICE);
        let uuids: Vec<&str> = services[0].characteristics.iter().map(|c| c.uuid.as_str()).collect();
        assert_eq!(uuids, [super::super::SENDER_CHARACTERISTIC, super::super::FIRMWARE_FILE_CHARACTERISTIC]);
        assert!(gatt_services(&objects, "/org/bluez/hci0/dev_11_22_33_44_55_66").is_empty());
        assert!(first_adapter(&ManagedObjects::new()).is_none());
    }

    fn signal<B>(path: &str, interface: &str, member: &str, body: &B) -> zbus::Message
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        zbus::Message::signal(path, interface, member).unwrap().build(body).unwrap()
    }

    fn changed(interface: &str, name: &str) -> (String, Properties, Vec<String>) {
        let props = Properties::from([(name.to_owned(), OwnedValue::from(-60i16))]);
        (interface.to_owned(), props, Vec::new())
    }

    /// Devices are listed in the order the signals first reported them
    /// (`InterfacesAdded` with `Device1`, or a `Device1` `PropertiesChanged`), not by address.
    #[test]
    fn discovery_keeps_first_seen_order() {
        let adapter = "/org/bluez/hci0";
        let added = |path: &str| {
            let device = Properties::from([("Address".to_owned(), OwnedValue::from(1u8))]);
            let interfaces = HashMap::from([(DEVICE.to_owned(), device)]);
            signal(
                "/",
                OBJECT_MANAGER,
                "InterfacesAdded",
                &(zbus::zvariant::ObjectPath::try_from(path).unwrap(), interfaces),
            )
        };
        let late = "/org/bluez/hci0/dev_FF_00_00_00_00_01";
        let early = "/org/bluez/hci0/dev_00_00_00_00_00_02";
        let messages = [
            added(late),
            signal(early, PROPERTIES, "PropertiesChanged", &changed(DEVICE, "RSSI")),
            signal(late, PROPERTIES, "PropertiesChanged", &changed(DEVICE, "RSSI")),
            // Not a device of hci0, not a device, not Device1.
            added("/org/bluez/hci1/dev_00_00_00_00_00_03"),
            signal(
                "/org/bluez/hci0/dev_00_00_00_00_00_02/service0010",
                PROPERTIES,
                "PropertiesChanged",
                &changed(DEVICE, "RSSI"),
            ),
            signal(early, PROPERTIES, "PropertiesChanged", &changed(GATT_SERVICE, "UUID")),
        ];
        let reported: Vec<Option<String>> = messages.iter().map(|m| advertised_device(m, adapter)).collect();
        assert_eq!(reported, [Some(late.to_owned()), Some(early.to_owned()), Some(late.to_owned()), None, None, None]);

        let device = |path: &str, address: &str| {
            (path.to_owned(), Advertiser { address: address.into(), name: Some("OBD2HUD-x".into()) })
        };
        let unsignalled = "/org/bluez/hci0/dev_00_00_00_00_00_00";
        // The object list gives them by path; the signals put `late` first, the unsignalled one last.
        let by_path = vec![device(unsignalled, "0"), device(early, "2"), device(late, "1")];
        let ordered = in_first_seen_order(by_path, &[late.to_owned(), early.to_owned()]);
        let paths: Vec<&str> = ordered.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, [late, early, unsignalled]);
    }

    /// Only a `Value` change of the notifying characteristic is a notification.
    #[test]
    fn notifications_are_value_changes_of_the_characteristic() {
        let chr = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0010/char0014";
        let value = signal(chr, PROPERTIES, "PropertiesChanged", &changed(GATT_CHARACTERISTIC, "Value"));
        assert!(is_value_notification(&value, chr));
        assert!(!is_value_notification(&value, "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0010/char0011"));
        let other = signal(chr, PROPERTIES, "PropertiesChanged", &changed(GATT_CHARACTERISTIC, "Notifying"));
        assert!(!is_value_notification(&other, chr));
    }
}
