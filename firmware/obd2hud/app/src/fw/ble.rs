//! NimBLE host glue: BLE set-up (device, server, service, advertising), the host
//! sync and reset callbacks, the GAP events, characteristic access, values,
//! subscriptions and notifications, on the pinned NimBLE host and IDF3.3 runtime.
//!
//! The decisions (payloads, value lengths, subscription and server bookkeeping, OTA steps) are
//! in `obd2hud_core::{ble, ota}`; this file makes the host calls.

use alloc::{boxed::Box, ffi::CString, vec, vec::Vec};
use core::ffi::{c_int, c_void};
use core::mem::MaybeUninit;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use esp_idf_sys::runtime::{CriticalMutex, Mutex, MutexGuard};

use esp_idf_sys as sys;
use obd2hud_core::ble::{self as b, Chr, Server, Step, Subscriptions, Uuid, CHARACTERISTICS};
use obd2hud_core::consts::{ESP_ERR_NVS_NEW_VERSION_FOUND, ESP_ERR_NVS_NO_FREE_PAGES, POWER_PIN};
use obd2hud_core::ota::{Ota, OtaHost, OTA_SIZE_UNKNOWN};
use obd2hud_core::telemetry::{self, slot};

use super::hw;

extern "C" {
    /// Initialises the NimBLE store configuration (not in the generated bindings).
    fn ble_store_config_init();
    /// Initialises the NimBLE HCI layer (not in the generated bindings).
    fn esp_nimble_hci_init() -> sys::esp_err_t;
}

/// The size of the scan duplicate cache.
const SCAN_DUPLICATE_SIZE: u16 = 20;
/// The scan duplicate filter mode.
const SCAN_FILTER_MODE: u8 = 0;

const N: usize = CHARACTERISTICS.len();
const TELEMETRY: usize = 0;
const OTA_CHR: usize = 6;

/// Whether the host has synced, and whether the device is initialised.
static SYNCED: AtomicBool = AtomicBool::new(false);
static INITIALIZED: AtomicBool = AtomicBool::new(false);
/// Whether the advertising state exists, whether its data is set, and its duration
/// (BLE_HS_FOREVER after a reset, 0 once advertising was started).
static ADV_EXISTS: AtomicBool = AtomicBool::new(false);
static ADV_DATA_SET: AtomicBool = AtomicBool::new(false);
static ADV_DURATION: AtomicU32 = AtomicU32::new(i32::MAX as u32);
/// The advertised name, captured from the GAP device name when advertising is reset.
static ADV_NAME: Mutex<Vec<u8>> = Mutex::new(Vec::new());
/// Characteristic value handles (filled in by `ble_gatts_start`).
static VAL_HANDLES: [AtomicU16; N] = [const { AtomicU16::new(0) }; N];
/// The value of each characteristic (guarded by a spinlock).
static VALUES: [CriticalMutex<Vec<u8>>; N] = [const { CriticalMutex::new(Vec::new()) }; N];
/// The subscriptions of the firmware characteristic (the only one with NOTIFY).
static OTA_SUBS: Mutex<Subscriptions> = Mutex::new(Subscriptions(Vec::new()));
static SERVER: Mutex<Option<Server>> = Mutex::new(None);
/// The update flag and the update handle.
static OTA_STATE: Mutex<Ota> = Mutex::new(Ota { update_flag: false });
static OTA_HANDLE: AtomicU32 = AtomicU32::new(0);

/// `BLE_GATT_ACCESS_OP_*`.
const OP_READ_CHR: u8 = 0;
const OP_WRITE_CHR: u8 = 1;
/// `BLE_HS_FOREVER`.
const BLE_HS_FOREVER: i32 = i32::MAX;
/// The `ble_hs_cfg` security settings: NO_IO, no bonding, no
/// MITM, LE Secure Connections, our key distribution ENC, theirs ENC | ID.
const SM_IO_CAP_NO_IO: u8 = 3;
const SM_OUR_KEY_DIST: u8 = 1;
const SM_THEIR_KEY_DIST: u8 = 3;
/// The passkey given for a passkey display or input request.
const DEFAULT_PASSKEY: u32 = 123_456;
/// Disconnect reasons treated as a host reset (mask 0x481800: BLE_HS_EOS,
/// BLE_HS_ECONTROLLER, BLE_HS_ETIMEOUT_HCI, BLE_HS_ENOTSYNCED).
const HOST_RESET_REASONS: u32 = 0x0048_1800;

const CHR_CPP: &core::ffi::CStr = c".pio\\libdeps\\wemos_d1_mini32\\NimBLE-Arduino\\src\\NimBLECharacteristic.cpp";
const CHR_FN: &core::ffi::CStr =
    c"static int NimBLECharacteristic::handleGapEvent(uint16_t, uint16_t, ble_gatt_access_ctxt*, void*)";
const DEV_CPP: &core::ffi::CStr = c".pio\\libdeps\\wemos_d1_mini32\\NimBLE-Arduino\\src\\NimBLEDevice.cpp";
const SYNC_FN: &core::ffi::CStr = c"static void NimBLEDevice::onSync()";
const INIT_FN: &core::ffi::CStr = c"static void NimBLEDevice::init(const string&)";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock()
}

/// Asserts that a NimBLE result is 0: otherwise the C assertion failure is reported, then abort.
fn assert_rc(rc: c_int, file: &core::ffi::CStr, line: c_int, func: &core::ffi::CStr) {
    if rc != 0 {
        unsafe { sys::__assert_func(file.as_ptr(), line, func.as_ptr(), c"rc == 0".as_ptr()) };
    }
}

// ------------------------------------------------------------------------- set-up

/// Sets up BLE: device, server, service, advertising.
pub fn ble_setup(serial_number: &[u8; 24]) {
    // The device is initialised with its name.
    device_init(&b::device_name(serial_number));
    // The server.
    create_server();
    // The seven characteristics of the service get their initial values.
    let upper = b::to_upper(serial_number);
    for (i, c) in CHARACTERISTICS.iter().enumerate() {
        set_value(i, &c.initial_value(&upper));
    }
    // The service is registered.
    service_start();
    // The advertising state is created; it advertises the service UUID.
    advertising_reset();
    // Advertising starts.
    start_advertising();
}

/// Initialises NVS, the BLE controller and the NimBLE host under the device name `name`,
/// and waits for the host to sync.
fn device_init(name: &[u8]) {
    if !INITIALIZED.load(Ordering::Acquire) {
        unsafe {
            let mut rc = sys::nvs_flash_init();
            if rc == ESP_ERR_NVS_NO_FREE_PAGES || rc == ESP_ERR_NVS_NEW_VERSION_FOUND {
                let e = sys::nvs_flash_erase();
                if e != 0 {
                    error_check_failed(e, 0x2ef, c"nvs_flash_erase()");
                }
                rc = sys::nvs_flash_init();
            }
            if rc != 0 {
                error_check_failed(rc, 0x2f3, c"errRc");
            }
            sys::esp_bt_controller_mem_release(sys::esp_bt_mode_t_ESP_BT_MODE_CLASSIC_BT);
            // The default controller configuration with the BLE overrides.
            let mut bt_cfg = bt_controller_config();
            let rc = sys::esp_bt_controller_init(&mut bt_cfg);
            if rc != 0 {
                error_check_failed(rc, 0x301, c"esp_bt_controller_init(&bt_cfg)");
            }
            let rc = sys::esp_bt_controller_enable(sys::esp_bt_mode_t_ESP_BT_MODE_BLE);
            if rc != 0 {
                error_check_failed(rc, 0x302, c"esp_bt_controller_enable(ESP_BT_MODE_BLE)");
            }
            // The HCI result is checked before the separate host initializer.
            let rc = esp_nimble_hci_init();
            if rc != 0 {
                error_check_failed(rc, 0x303, c"esp_nimble_hci_init()");
            }
            sys::nimble_port_init();
            let cfg = &mut *ptr::addr_of_mut!(sys::ble_hs_cfg);
            cfg.reset_cb = Some(on_reset);
            cfg.sync_cb = Some(on_sync);
            cfg.sm_io_cap = SM_IO_CAP_NO_IO;
            cfg.set_sm_bonding(0);
            cfg.set_sm_mitm(0);
            cfg.set_sm_sc(1);
            cfg.sm_our_key_dist = SM_OUR_KEY_DIST;
            cfg.sm_their_key_dist = SM_THEIR_KEY_DIST;
            cfg.store_status_cb = Some(sys::ble_store_util_status_rr);
            let cname = CString::new(name.to_vec()).unwrap_or_default();
            assert_rc(sys::ble_svc_gap_device_name_set(cname.as_ptr()), DEV_CPP, 0x316, INIT_FN);
            ble_store_config_init();
            sys::nimble_port_freertos_init(Some(host_task));
        }
    }
    while !SYNCED.load(Ordering::Acquire) {
        unsafe { sys::vTaskDelay(1) };
    }
    INITIALIZED.store(true, Ordering::Release);
}

/// Reports a failed ESP error check of the device initialisation: prints, then aborts.
fn error_check_failed(rc: sys::esp_err_t, line: c_int, expr: &core::ffi::CStr) -> ! {
    unsafe { sys::_esp_error_check_failed(rc, DEV_CPP.as_ptr(), line, INIT_FN.as_ptr(), expr.as_ptr()) }
}

/// The controller configuration: the IDF 3.3 defaults, then BLE mode, at most 3 BLE
/// connections, a scan duplicate cache of 20 and scan duplicate filter mode 0.
fn bt_controller_config() -> sys::esp_bt_controller_config_t {
    sys::esp_bt_controller_config_t {
        controller_task_stack_size: sys::ESP_TASK_BT_CONTROLLER_STACK as _,
        controller_task_prio: sys::ESP_TASK_BT_CONTROLLER_PRIO as _,
        hci_uart_no: sys::BT_HCI_UART_NO_DEFAULT as _,
        hci_uart_baudrate: sys::BT_HCI_UART_BAUDRATE_DEFAULT,
        scan_duplicate_mode: sys::SCAN_DUPLICATE_MODE as _,
        scan_duplicate_type: SCAN_FILTER_MODE,
        normal_adv_size: SCAN_DUPLICATE_SIZE,
        mesh_adv_size: sys::MESH_DUPLICATE_SCAN_CACHE_SIZE as _,
        send_adv_reserved_size: sys::SCAN_SEND_ADV_RESERVED_SIZE as _,
        controller_debug_flag: 0,
        mode: sys::esp_bt_mode_t_ESP_BT_MODE_BLE as _,
        ble_max_conn: 3,
        // The IDF 3.3 default, 2 (Classic only; mode is BLE).
        bt_max_acl_conn: 2,
        bt_sco_datapath: sys::CONFIG_BTDM_CTRL_BR_EDR_SCO_DATA_PATH_EFF as _,
        auto_latency: false,
        // The IDF 3.3 default, 1 (Classic only; mode is BLE).
        bt_legacy_auth_vs_evt: true,
        bt_max_sync_conn: 0,
        magic: sys::ESP_BT_CONTROLLER_CONFIG_MAGIC_VAL,
    }
}

/// The NimBLE host task: runs the port, then deinitialises it.
extern "C" fn host_task(_p: *mut c_void) {
    unsafe {
        sys::nimble_port_run();
        sys::nimble_port_freertos_deinit();
    }
}

/// The host sync callback.
unsafe extern "C" fn on_sync() {
    if SYNCED.load(Ordering::Acquire) {
        return;
    }
    assert_rc(sys::ble_hs_util_ensure_addr(0), DEV_CPP, 0x2b9, SYNC_FN);
    // Yield once.
    sys::vPortYield();
    SYNCED.store(true, Ordering::Release);
    if INITIALIZED.load(Ordering::Acquire) && ADV_EXISTS.load(Ordering::Acquire) {
        // After a host sync the advertising data is set again and
        // advertising that was running restarts.
        ADV_DATA_SET.store(false, Ordering::Release);
        if ADV_DURATION.load(Ordering::Acquire) == 0 {
            start_advertising();
        }
    }
}

/// The host reset callback.
unsafe extern "C" fn on_reset(reason: c_int) {
    if !SYNCED.swap(false, Ordering::AcqRel) {
        return;
    }
    // The reason has no text.
    sys::esp_log_write(
        sys::esp_log_level_t_ESP_LOG_ERROR,
        c"NimBLE".as_ptr(),
        c"CRIT %s: \"Resetting state; reason=%d, %s\"\n".as_ptr(),
        c"NimBLEDevice".as_ptr(),
        reason,
        c"".as_ptr(),
    );
}

/// Creates the server: the GATT server is reset and the GAP and GATT services are
/// initialised. The GAP service has two characteristics (Device Name 0x2A00,
/// Appearance 0x2A01) and the GATT service one (Service Changed 0x2A05); the handles of
/// the firmware's own service follow from those.
fn create_server() {
    unsafe {
        sys::ble_gatts_reset();
        sys::ble_svc_gap_init();
        sys::ble_svc_gatt_init();
    }
    *lock(&SERVER) = Some(Server::default());
}

/// Sets the value of characteristic `i`: values above 512 bytes are ignored.
fn set_value(i: usize, v: &[u8]) {
    if v.len() <= b::ATTR_MAX_LEN {
        *VALUES[i].lock() = v.to_vec();
    }
}

fn get_value(i: usize) -> Vec<u8> {
    VALUES[i].lock().clone()
}

fn uuid_ptr(u: Uuid) -> *const sys::ble_uuid_t {
    match u {
        Uuid::U16(v) => {
            let p = Box::leak(Box::new(sys::ble_uuid16_t {
                u: sys::ble_uuid_t { type_: sys::BLE_UUID_TYPE_16 as u8 },
                value: v,
            }));
            p as *const sys::ble_uuid16_t as *const sys::ble_uuid_t
        }
        Uuid::U128(bytes) => {
            let p = Box::leak(Box::new(sys::ble_uuid128_t {
                u: sys::ble_uuid_t { type_: sys::BLE_UUID_TYPE_128 as u8 },
                value: bytes,
            }));
            p as *const sys::ble_uuid128_t as *const sys::ble_uuid_t
        }
    }
}

/// Registers the service: one primary service with the seven characteristics
/// (no user descriptors; NimBLE adds the CCCD of the NOTIFY one), `ble_gatts_count_cfg` +
/// `ble_gatts_add_svcs`. Errors are not reported.
fn service_start() {
    let mut chrs: Vec<sys::ble_gatt_chr_def> = CHARACTERISTICS
        .iter()
        .enumerate()
        .map(|(i, c)| sys::ble_gatt_chr_def {
            uuid: uuid_ptr(c.uuid()),
            access_cb: Some(chr_access),
            arg: i as *mut c_void,
            descriptors: ptr::null_mut(),
            flags: c.properties() as sys::ble_gatt_chr_flags,
            min_key_size: 0,
            val_handle: VAL_HANDLES[i].as_ptr(),
            ..Default::default()
        })
        .collect();
    chrs.push(sys::ble_gatt_chr_def::default());
    let chrs: &'static [sys::ble_gatt_chr_def] = chrs.leak();
    let svcs: &'static mut [sys::ble_gatt_svc_def] = vec![
        sys::ble_gatt_svc_def {
            type_: sys::BLE_GATT_SVC_TYPE_PRIMARY as u8,
            uuid: uuid_ptr(Uuid::U128(b::SERVICE)),
            includes: ptr::null_mut(),
            characteristics: chrs.as_ptr(),
        },
        sys::ble_gatt_svc_def::default(),
    ]
    .leak();
    unsafe {
        if sys::ble_gatts_count_cfg(svcs.as_ptr()) == 0 {
            sys::ble_gatts_add_svcs(svcs.as_ptr());
        }
    }
}

/// Creates the advertising state: the name is the GAP device name, flags 6, scan
/// response on, and the advertising data is not yet set.
fn advertising_reset() {
    let name = unsafe { core::ffi::CStr::from_ptr(sys::ble_svc_gap_device_name()) };
    *lock(&ADV_NAME) = name.to_bytes().to_vec();
    ADV_DURATION.store(BLE_HS_FOREVER as u32, Ordering::Release);
    ADV_DATA_SET.store(false, Ordering::Release);
    ADV_EXISTS.store(true, Ordering::Release);
}

/// Starts the GATT server (abort on failure).
fn server_start() {
    let rc = unsafe { sys::ble_gatts_start() };
    if rc != 0 {
        unsafe { sys::abort() };
    }
}

/// Starts advertising without a time limit; false when it could not start.
fn start_advertising() -> bool {
    if !SYNCED.load(Ordering::Acquire) {
        return false;
    }
    {
        let mut g = lock(&SERVER);
        if let Some(s) = g.as_mut() {
            if !s.started {
                s.started = true;
                drop(g);
                server_start();
            } else if s.peers.len() > b::MAX_PEERS_FOR_ADV {
                return false;
            }
        }
    }
    if unsafe { sys::ble_gap_adv_active() } != 0 {
        return false;
    }
    ADV_DURATION.store(0, Ordering::Release);
    // The advertising parameters: conn_mode UND (2), disc_mode GEN (2); intervals and channel map 0 = host defaults.
    let params = sys::ble_gap_adv_params { conn_mode: 2, disc_mode: 2, ..Default::default() };
    if !ADV_DATA_SET.load(Ordering::Acquire) {
        let name = lock(&ADV_NAME).clone();
        let p = b::adv_payload(&[b::SERVICE], &name);
        let mut rc = unsafe { sys::ble_gap_adv_rsp_set_data(p.scan_rsp.as_ptr(), p.scan_rsp.len() as c_int) };
        if rc == 0 {
            rc = unsafe { sys::ble_gap_adv_set_data(p.adv.as_ptr(), p.adv.len() as c_int) };
        }
        if rc != 0 {
            return false;
        }
        ADV_DATA_SET.store(true, Ordering::Release);
    }
    let rc = unsafe {
        sys::ble_gap_adv_start(0, ptr::null(), BLE_HS_FOREVER, &params, Some(server_gap_event), ptr::null_mut())
    };
    rc == 0
}

// ------------------------------------------------------------------------- GAP events

/// The GAP event handler of the server, given as the advertising callback.
unsafe extern "C" fn server_gap_event(event: *mut sys::ble_gap_event, _arg: *mut c_void) -> c_int {
    let ev = &*event;
    let u = &ev.__bindgen_anon_1;
    match ev.type_ as u32 {
        sys::BLE_GAP_EVENT_CONNECT => {
            let c = u.connect;
            let steps = lock(&SERVER).as_mut().map(|s| s.on_connect(c.status, c.conn_handle)).unwrap_or_default();
            if c.status == 0 {
                // The connection is looked up; nothing else happens on a connect.
                let mut d = MaybeUninit::<sys::ble_gap_conn_desc>::zeroed();
                let _ = sys::ble_gap_conn_find(c.conn_handle, d.as_mut_ptr());
            }
            run_steps(&steps);
            0
        }
        sys::BLE_GAP_EVENT_DISCONNECT => {
            let d = u.disconnect;
            let reason = d.reason as u32;
            if reason < 0x17 && (1u32 << reason) & HOST_RESET_REASONS != 0 {
                sys::esp_log_write(
                    sys::esp_log_level_t_ESP_LOG_ERROR,
                    c"NimBLE".as_ptr(),
                    c"CRIT %s: \"Disconnect - host reset, rc=%d\"\n".as_ptr(),
                    c"NimBLEServer".as_ptr(),
                    d.reason,
                );
                on_reset(d.reason);
            }
            let steps = lock(&SERVER).as_mut().map(|s| s.on_disconnect(d.conn.conn_handle)).unwrap_or_default();
            run_steps(&steps);
            0
        }
        sys::BLE_GAP_EVENT_ADV_COMPLETE => 0, // nothing to do
        sys::BLE_GAP_EVENT_ENC_CHANGE => {
            let mut d = MaybeUninit::<sys::ble_gap_conn_desc>::zeroed();
            if sys::ble_gap_conn_find(u.enc_change.conn_handle, d.as_mut_ptr()) != 0 {
                return 1;
            }
            0 // nothing to do once the connection is found
        }
        sys::BLE_GAP_EVENT_PASSKEY_ACTION => {
            // Unreachable with NO_IO (just works); handled with default answers.
            let pk = u.passkey;
            let mut io: sys::ble_sm_io = MaybeUninit::zeroed().assume_init();
            io.action = pk.params.action;
            let ok = match pk.params.action as u32 {
                sys::BLE_SM_IOACT_DISP | sys::BLE_SM_IOACT_INPUT => {
                    io.__bindgen_anon_1.passkey = DEFAULT_PASSKEY;
                    true
                }
                sys::BLE_SM_IOACT_NUMCMP => {
                    io.__bindgen_anon_1.numcmp_accept = 1;
                    true
                }
                sys::BLE_SM_IOACT_OOB => true, // 16 zero bytes
                _ => false,
            };
            if ok {
                sys::ble_sm_inject_io(pk.conn_handle, &mut io);
            }
            0
        }
        sys::BLE_GAP_EVENT_SUBSCRIBE => {
            let s = u.subscribe;
            if s.attr_handle == VAL_HANDLES[OTA_CHR].load(Ordering::Acquire) {
                // The subscription is recorded only for a known connection.
                let mut d = MaybeUninit::<sys::ble_gap_conn_desc>::zeroed();
                if sys::ble_gap_conn_find(s.conn_handle, d.as_mut_ptr()) == 0 {
                    lock(&OTA_SUBS).set(
                        Chr::Ota.properties(),
                        s.conn_handle,
                        s.cur_notify() != 0,
                        s.cur_indicate() != 0,
                    );
                }
            }
            0
        }
        sys::BLE_GAP_EVENT_MTU => 0, // nothing to do on an MTU change
        sys::BLE_GAP_EVENT_REPEAT_PAIRING => {
            let mut d = MaybeUninit::<sys::ble_gap_conn_desc>::zeroed();
            if sys::ble_gap_conn_find(u.repeat_pairing.conn_handle, d.as_mut_ptr()) != 0 {
                return sys::BLE_GAP_REPEAT_PAIRING_IGNORE as c_int;
            }
            sys::ble_store_util_delete_peer(&(*d.as_ptr()).peer_id_addr);
            sys::BLE_GAP_REPEAT_PAIRING_RETRY as c_int
        }
        _ => 0,
    }
}

fn run_steps(steps: &[Step]) {
    for s in steps {
        match s {
            Step::StartAdvertising => {
                start_advertising();
            }
            Step::SketchDisconnect => on_disconnect(),
        }
    }
}

/// The firmware's disconnect handling: HUD power output low, power slot 0,
/// then start advertising.
fn on_disconnect() {
    hw::digital_write(POWER_PIN, false);
    super::set_slot(slot::POWER, 0.0);
    start_advertising();
}

// ------------------------------------------------------------------------- GATT access

/// The access callback of the characteristics: reads and writes.
unsafe extern "C" fn chr_access(
    conn: u16,
    _attr: u16,
    ctxt: *mut sys::ble_gatt_access_ctxt,
    arg: *mut c_void,
) -> c_int {
    let i = arg as usize;
    let ctxt = &mut *ctxt;
    match ctxt.op {
        OP_READ_CHR => {
            // For a read with a packet header longer than 8, the connection
            // must be found.
            if (*ctxt.om).om_pkthdr_len > 8 {
                let mut d = MaybeUninit::<sys::ble_gap_conn_desc>::zeroed();
                assert_rc(sys::ble_gap_conn_find(conn, d.as_mut_ptr()), CHR_CPP, 0x11f, CHR_FN);
            }
            let v = VALUES[i].lock();
            let rc = sys::os_mbuf_append(ctxt.om, v.as_ptr() as *const c_void, v.len() as u16);
            if rc == 0 {
                0
            } else {
                b::ATT_ERR_INSUFFICIENT_RES
            }
        }
        OP_WRITE_CHR => {
            let mut segs: Vec<&[u8]> = Vec::new();
            let mut om = ctxt.om;
            while !om.is_null() {
                segs.push(core::slice::from_raw_parts((*om).om_data, (*om).om_len as usize));
                om = (*om).om_next.sle_next;
            }
            let buf = match b::assemble_write(segs) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let mut d = MaybeUninit::<sys::ble_gap_conn_desc>::zeroed();
            assert_rc(sys::ble_gap_conn_find(conn, d.as_mut_ptr()), CHR_CPP, 0x140, CHR_FN);
            set_value(i, &buf);
            // The write handler of the characteristic, if it has one.
            match i {
                TELEMETRY => telemetry_on_write(),
                OTA_CHR => ota_on_write(),
                _ => {}
            }
            0
        }
        _ => b::ATT_ERR_UNLIKELY,
    }
}

/// The write handler of the telemetry characteristic: nine float slot stores.
fn telemetry_on_write() {
    let v = get_value(TELEMETRY);
    telemetry::publish(&v, super::set_slot);
}

/// The write handler of the firmware characteristic.
fn ota_on_write() {
    let v = get_value(OTA_CHR);
    let mut st = lock(&OTA_STATE);
    st.on_write(&v, &mut OtaGlue);
}

/// Notifies the value of characteristic `i` to its subscribers.
fn notify(i: usize) {
    let subs = lock(&OTA_SUBS).clone();
    if subs.0.is_empty() {
        return;
    }
    let v = get_value(i);
    let handle = VAL_HANDLES[i].load(Ordering::Acquire);
    for conn in subs.notify_targets(|c| unsafe { sys::ble_att_mtu(c) }) {
        unsafe {
            let om = sys::ble_hs_mbuf_from_flat(v.as_ptr() as *const c_void, v.len() as u16);
            sys::ble_gattc_notify_custom(conn, handle, om);
        }
    }
}

struct OtaGlue;

impl OtaHost for OtaGlue {
    fn println(&mut self, text: &[u8]) {
        // The text, then "\r\n".
        hw::Uart0.write(text);
        hw::Uart0.write(b"\r\n");
    }

    fn begin(&mut self) {
        unsafe {
            let part = sys::esp_ota_get_next_update_partition(ptr::null());
            let mut h: sys::esp_ota_handle_t = OTA_HANDLE.load(Ordering::Acquire);
            sys::esp_ota_begin(part, OTA_SIZE_UNKNOWN as usize, &mut h);
            OTA_HANDLE.store(h, Ordering::Release);
        }
    }

    fn write(&mut self, data: &[u8]) {
        unsafe {
            sys::esp_ota_write(OTA_HANDLE.load(Ordering::Acquire), data.as_ptr() as *const c_void, data.len());
        }
    }

    fn end(&mut self) {
        unsafe {
            sys::esp_ota_end(OTA_HANDLE.load(Ordering::Acquire));
        }
    }

    fn set_boot_partition(&mut self) -> bool {
        unsafe { sys::esp_ota_set_boot_partition(sys::esp_ota_get_next_update_partition(ptr::null())) == 0 }
    }

    fn restart(&mut self, delay_ms: u32) {
        hw::delay(delay_ms);
        unsafe { sys::esp_restart() };
    }

    fn ack(&mut self, value: &[u8]) {
        set_value(OTA_CHR, value);
        notify(OTA_CHR);
    }
}
