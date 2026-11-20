//! The Bluetooth LE side of the firmware: the GATT table, the device name and the
//! advertising payload, and the server behaviour:
//!
//! * the layout of the advertising and scan-response payloads,
//! * the GAP events (connect / disconnect / subscribe),
//! * the read / write access of a characteristic
//!   (values above 512 bytes are ignored),
//! * what a disconnect does.
//!
//! Everything here is target-independent; the NimBLE host calls live in the application.

use alloc::vec::Vec;

/// The prefix of the device name.
pub const NAME_PREFIX: &[u8] = b"OBD2HUD-";
/// The part of the serial number in the device name: of the 24-character serial it gives 6.
pub const NAME_SERIAL_FROM: u32 = 0x12;
pub const NAME_SERIAL_TO: u32 = 0x19;
/// The size of the name buffer: at most 13 characters make the device name.
pub const NAME_BUF: usize = 0xe;

/// The service.
pub const SERVICE_UUID: &str = "c14fcc20-72f1-11ec-90d6-0242ac120003";
/// The telemetry characteristic (WRITE).
pub const TELEMETRY_UUID: &str = "c97c6c5a-72f1-11ec-90d6-0242ac120003";
/// The firmware file characteristic (WRITE | NOTIFY).
pub const OTA_UUID: &str = "173c2db0-7723-11ec-90d6-0242ac120003";

/// Characteristic properties (NimBLE `BLE_GATT_CHR_F_*`).
pub mod prop {
    pub const READ: u16 = 0x0002;
    pub const WRITE_NR: u16 = 0x0004;
    pub const WRITE: u16 = 0x0008;
    pub const NOTIFY: u16 = 0x0010;
    pub const INDICATE: u16 = 0x0020;
}

/// The maximum attribute length (`BLE_ATT_ATTR_MAX_LEN`): longer values are not set, longer writes fail.
pub const ATTR_MAX_LEN: usize = 512;
/// ATT error codes returned by the access callback.
pub const ATT_ERR_INVALID_ATTR_VALUE_LEN: i32 = 0x0d;
pub const ATT_ERR_UNLIKELY: i32 = 0x0e;
pub const ATT_ERR_INSUFFICIENT_RES: i32 = 0x11;

/// A GATT UUID as NimBLE stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Uuid {
    U16(u16),
    /// Little-endian (NimBLE `ble_uuid128_t.value`) byte order.
    U128([u8; 16]),
}

/// A 128-bit UUID from its canonical 36-character form: the 16 bytes
/// reversed into NimBLE's little-endian order.
pub const fn uuid128(s: &str) -> [u8; 16] {
    let b = s.as_bytes();
    let mut be = [0u8; 16];
    let mut i = 0;
    let mut n = 0;
    while i < b.len() {
        if b[i] != b'-' {
            let h = hex(b[i]);
            let l = hex(b[i + 1]);
            be[n] = (h << 4) | l;
            n += 1;
            i += 2;
        } else {
            i += 1;
        }
    }
    let mut le = [0u8; 16];
    let mut k = 0;
    while k < 16 {
        le[k] = be[15 - k];
        k += 1;
    }
    le
}

const fn hex(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("bad uuid"),
    }
}

pub const SERVICE: [u8; 16] = uuid128(SERVICE_UUID);
pub const TELEMETRY: [u8; 16] = uuid128(TELEMETRY_UUID);
pub const OTA: [u8; 16] = uuid128(OTA_UUID);

/// The characteristics, in creation order (= handle order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chr {
    Telemetry,
    ModelNumber,
    SerialNumber,
    FirmwareRevision,
    HardwareRevision,
    Manufacturer,
    Ota,
}

pub const CHARACTERISTICS: [Chr; 7] = [
    Chr::Telemetry,
    Chr::ModelNumber,
    Chr::SerialNumber,
    Chr::FirmwareRevision,
    Chr::HardwareRevision,
    Chr::Manufacturer,
    Chr::Ota,
];

/// The model number: the terminating NUL is part of the value.
pub const MODEL_NUMBER_VALUE: &[u8] = b"OBD2HUD\0";
/// The value of 0x2A26 and 0x2A27. The host reads 0x2A26 and drops
/// the last character, i.e. this NUL.
pub const REVISION_VALUE: &[u8] = b"1.0.0\0";
/// The manufacturer name, with its terminating NUL.
pub const MANUFACTURER_VALUE: &[u8] = b"SIMRACINGSTUDIO\0";
/// The value set and notified after every write of the firmware characteristic.
pub const OTA_ACK_VALUE: [u8; 5] = [1, 2, 3, 4, 5];

impl Chr {
    pub fn uuid(self) -> Uuid {
        match self {
            Chr::Telemetry => Uuid::U128(TELEMETRY),
            Chr::ModelNumber => Uuid::U16(0x2a24),
            Chr::SerialNumber => Uuid::U16(0x2a25),
            Chr::FirmwareRevision => Uuid::U16(0x2a26),
            Chr::HardwareRevision => Uuid::U16(0x2a27),
            Chr::Manufacturer => Uuid::U16(0x2a29),
            Chr::Ota => Uuid::U128(OTA),
        }
    }

    /// The properties of the characteristic: 8, 2 (x5), 0x18.
    pub fn properties(self) -> u16 {
        match self {
            Chr::Telemetry => prop::WRITE,
            Chr::Ota => prop::WRITE | prop::NOTIFY,
            _ => prop::READ,
        }
    }

    /// The value set at set-up (empty for the two written characteristics).
    pub fn initial_value(self, serial_upper: &[u8]) -> Vec<u8> {
        match self {
            Chr::Telemetry | Chr::Ota => Vec::new(),
            Chr::ModelNumber => MODEL_NUMBER_VALUE.to_vec(),
            Chr::SerialNumber => serial_upper.to_vec(),
            Chr::FirmwareRevision | Chr::HardwareRevision => REVISION_VALUE.to_vec(),
            Chr::Manufacturer => MANUFACTURER_VALUE.to_vec(),
        }
    }
}

/// Upper case, per ASCII byte.
pub fn to_upper(s: &[u8]) -> Vec<u8> {
    s.iter().map(|b| b.to_ascii_uppercase()).collect()
}

/// The device name: the prefix plus characters 0x12..0x19 of the serial number,
/// in upper case, cut to 13 characters.
pub fn device_name(serial: &[u8]) -> Vec<u8> {
    let mut s = NAME_PREFIX.to_vec();
    s.extend_from_slice(crate::telemetry::substring(serial, NAME_SERIAL_FROM, NAME_SERIAL_TO));
    let mut s = to_upper(&s);
    s.truncate(NAME_BUF - 1);
    s
}

/// The maximum size of an advertising payload (`BLE_HS_ADV_MAX_SZ`).
pub const ADV_MAX: usize = 31;
/// The advertising flags: `BLE_HS_ADV_F_DISC_GEN | BLE_HS_ADV_F_BREDR_UNSUP`.
pub const ADV_FLAGS: u8 = 0x06;

/// The advertising and scan-response payloads of the firmware
/// (flags 6, name = GAP device name, complete; no TX power field;
/// a scan response; one service UUID).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvPayload {
    pub adv: Vec<u8>,
    pub scan_rsp: Vec<u8>,
}

/// The length accounting and field placement for 128-bit service
/// UUIDs and a name, then the NimBLE field encoding (type order: flags,
/// 128-bit UUIDs, name).
pub fn adv_payload(services: &[[u8; 16]], name: &[u8]) -> AdvPayload {
    let mut len: usize = 3; // flags
    let mut uuids: Vec<[u8; 16]> = Vec::new();
    let mut uuids_complete = false;
    for u in services {
        let need = if uuids.is_empty() { 0x12 } else { 0x10 };
        if len + need < 0x20 {
            len += need;
            uuids.push(*u);
            uuids_complete = true;
        } else {
            uuids_complete = false;
        }
    }
    let mut adv_name: Option<(&[u8], bool)> = Some((name, true));
    let mut rsp_name: Option<(&[u8], bool)> = None;
    if len + 2 + name.len() > ADV_MAX {
        // The name does not fit: move it to the scan response.
        let n = name.len().min(0x1d);
        rsp_name = Some((&name[..n], name.len() < 0x1e));
        adv_name = None;
    }
    let mut adv = Vec::new();
    adv.extend_from_slice(&[2, 0x01, ADV_FLAGS]);
    if !uuids.is_empty() {
        adv.push((1 + 16 * uuids.len()) as u8);
        adv.push(if uuids_complete { 0x07 } else { 0x06 });
        for u in &uuids {
            adv.extend_from_slice(u);
        }
    }
    let put_name = |v: &mut Vec<u8>, (n, complete): (&[u8], bool)| {
        if !n.is_empty() {
            v.push((1 + n.len()) as u8);
            v.push(if complete { 0x09 } else { 0x08 });
            v.extend_from_slice(n);
        }
    };
    if let Some(n) = adv_name {
        put_name(&mut adv, n);
    }
    let mut scan_rsp = Vec::new();
    if let Some(n) = rsp_name {
        put_name(&mut scan_rsp, n);
    }
    AdvPayload { adv, scan_rsp }
}

/// The write path of a characteristic: the mbuf chain is gathered into a
/// 512-byte buffer; a chain longer than that is refused before anything is stored.
pub fn assemble_write<'a, I: IntoIterator<Item = &'a [u8]>>(chain: I) -> Result<Vec<u8>, i32> {
    let mut buf = Vec::new();
    for seg in chain {
        if buf.len() + seg.len() > ATTR_MAX_LEN {
            return Err(ATT_ERR_INVALID_ATTR_VALUE_LEN);
        }
        buf.extend_from_slice(seg);
    }
    Ok(buf)
}

/// The notify and indicate bits of a subscription.
pub const SUB_NOTIFY: u8 = 1;
pub const SUB_INDICATE: u8 = 2;

/// The subscriptions of a characteristic: connection and subscription bits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Subscriptions(pub Vec<(u16, u8)>);

impl Subscriptions {
    /// Records a subscribe event: the bits are notify and indicate, each masked by
    /// the properties; a known connection is updated (removed when
    /// 0), a new one is appended only when the bits are not 0.
    pub fn set(&mut self, props: u16, conn: u16, cur_notify: bool, cur_indicate: bool) {
        let v = ((cur_notify && props & prop::NOTIFY != 0) as u8)
            | (((cur_indicate && props & prop::INDICATE != 0) as u8) << 1);
        if let Some(i) = self.0.iter().position(|e| e.0 == conn) {
            if v == 0 {
                self.0.remove(i);
            } else {
                self.0[i].1 = v;
            }
        } else if v != 0 {
            self.0.push((conn, v));
        }
    }

    /// The connections a notification is sent to:
    /// every entry whose peer MTU is
    /// not 0. A characteristic without INDICATE never falls back to an indication.
    pub fn notify_targets(&self, mtu: impl Fn(u16) -> u16) -> Vec<u16> {
        self.0.iter().filter(|(c, v)| mtu(*c) != 0 && *v != 0).map(|(c, _)| *c).collect()
    }
}

/// The server state the event handler uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    /// Whether the GATT server has started.
    pub started: bool,
    /// Whether advertising restarts after a disconnect (it does).
    pub advertise_on_disconnect: bool,
    /// Whether the services changed (never: no service is removed).
    pub svc_changed: bool,
    /// The connected peers.
    pub peers: Vec<u16>,
}

impl Default for Server {
    fn default() -> Self {
        Self { started: false, advertise_on_disconnect: true, svc_changed: false, peers: Vec::new() }
    }
}

/// With 3 connections at most, advertising does not start with more than 2 peers.
pub const MAX_PEERS_FOR_ADV: usize = 2;

/// What the application has to do after a GAP event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Start advertising.
    StartAdvertising,
    /// The firmware's disconnect handling: HUD power output low, power slot 0, then start advertising.
    SketchDisconnect,
}

impl Server {
    /// BLE_GAP_EVENT_CONNECT: success records the peer;
    /// a failed connection restarts advertising.
    pub fn on_connect(&mut self, status: i32, conn: u16) -> Vec<Step> {
        if status == 0 {
            self.peers.push(conn);
            Vec::new()
        } else {
            alloc::vec![Step::StartAdvertising]
        }
    }

    /// BLE_GAP_EVENT_DISCONNECT: the peer is removed, the firmware's disconnect handling runs,
    /// then advertising restarts when `advertise_on_disconnect` is set.
    pub fn on_disconnect(&mut self, conn: u16) -> Vec<Step> {
        self.peers.retain(|&p| p != conn);
        let mut v = alloc::vec![Step::SketchDisconnect];
        if self.advertise_on_disconnect {
            v.push(Step::StartAdvertising);
        }
        v
    }

    /// Whether advertising may start: host synced, at most 2 peers,
    /// not already advertising.
    pub fn may_advertise(&self, synced: bool, adv_active: bool) -> bool {
        synced && self.peers.len() <= MAX_PEERS_FOR_ADV && !adv_active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERIAL: &[u8; 24] = b"01ab0f10ffff240ac40102ff";

    #[test]
    fn uuids_in_nimble_byte_order() {
        assert_eq!(
            SERVICE,
            [0x03, 0x00, 0x12, 0xac, 0x42, 0x02, 0xd6, 0x90, 0xec, 0x11, 0xf1, 0x72, 0x20, 0xcc, 0x4f, 0xc1]
        );
        assert_eq!(OTA[15], 0x17);
        assert_eq!(TELEMETRY[12..], [0x5a, 0x6c, 0x7c, 0xc9]);
    }

    #[test]
    fn device_name_is_13_characters() {
        // Characters 18..25 of the 24-character serial = "0102ff"; the name keeps 13 characters.
        assert_eq!(device_name(SERIAL), b"OBD2HUD-0102F");
        assert_eq!(device_name(SERIAL).len(), 13);
    }

    #[test]
    fn characteristic_table() {
        let props: Vec<u16> = CHARACTERISTICS.iter().map(|c| c.properties()).collect();
        assert_eq!(props, [8, 2, 2, 2, 2, 2, 0x18]);
        let up = to_upper(SERIAL);
        assert_eq!(Chr::SerialNumber.initial_value(&up), b"01AB0F10FFFF240AC40102FF");
        assert_eq!(Chr::FirmwareRevision.initial_value(&up), b"1.0.0\0");
        assert_eq!(Chr::ModelNumber.initial_value(&up).len(), 8);
        assert_eq!(Chr::Manufacturer.initial_value(&up).len(), 16);
        assert_eq!(Chr::FirmwareRevision.uuid(), Uuid::U16(0x2a26));
    }

    #[test]
    fn name_moves_to_the_scan_response() {
        let p = adv_payload(&[SERVICE], &device_name(SERIAL));
        let mut adv = alloc::vec![2, 1, 6, 17, 7];
        adv.extend_from_slice(&SERVICE);
        assert_eq!(p.adv, adv);
        let mut rsp = alloc::vec![14, 9];
        rsp.extend_from_slice(b"OBD2HUD-0102F");
        assert_eq!(p.scan_rsp, rsp);
        assert!(p.adv.len() <= ADV_MAX);
        // A short name stays in the advertising data.
        let p = adv_payload(&[SERVICE], b"ab");
        assert_eq!(&p.adv[21..], &[3, 9, b'a', b'b']);
        assert!(p.scan_rsp.is_empty());
    }

    #[test]
    fn writes_over_512_bytes_are_refused() {
        let a = [7u8; 300];
        let b = [8u8; 212];
        assert_eq!(assemble_write([&a[..], &b[..]]).unwrap().len(), 512);
        assert_eq!(assemble_write([&a[..], &b[..], &[1u8][..]]), Err(ATT_ERR_INVALID_ATTR_VALUE_LEN));
    }

    #[test]
    fn subscriptions() {
        let p = prop::WRITE | prop::NOTIFY;
        let mut s = Subscriptions::default();
        s.set(p, 1, false, false);
        assert!(s.0.is_empty());
        s.set(p, 1, true, false);
        // Indications are masked by the properties: nothing to record.
        s.set(p, 2, false, true);
        assert_eq!(s.0, [(1, 1)]);
        s.set(p, 3, true, true);
        assert_eq!(s.notify_targets(|_| 255), [1, 3]);
        assert_eq!(s.notify_targets(|c| if c == 1 { 0 } else { 23 }), [3]);
        s.set(p, 1, false, false);
        assert_eq!(s.0, [(3, 1)]);
    }

    #[test]
    fn server_events() {
        let mut s = Server::default();
        assert!(s.on_connect(0, 1).is_empty());
        assert_eq!(s.on_connect(13, 0xffff), [Step::StartAdvertising]);
        assert!(s.may_advertise(true, false));
        assert!(!s.may_advertise(true, true));
        s.on_connect(0, 2);
        s.on_connect(0, 3);
        assert!(!s.may_advertise(true, false));
        assert_eq!(s.on_disconnect(1), [Step::SketchDisconnect, Step::StartAdvertising]);
        assert_eq!(s.peers, [2, 3]);
        assert!(s.may_advertise(true, false));
        assert!(!s.may_advertise(false, false));
    }
}
