//! Byte-for-byte checks of the protocol and layout code against fixed
//! wire, register and storage values.

use std::collections::HashMap;

use obd2hud_core::ble;
use obd2hud_core::can::{self, CanFrame, Regs};
use obd2hud_core::consts::*;
use obd2hud_core::eeprom::{Eeprom, NvsBackend};
use obd2hud_core::messenger::{atoi, CmdMessenger, Dispatch, Print, COMMAND_BUFFER_ADDR, MAX_STREAM_BUFFER};
use obd2hud_core::obd::{self, Ecu};
use obd2hud_core::{gate, random, serial, telemetry, xtea};

struct Wire(Vec<u8>);
impl Print for Wire {
    fn write_bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
}

/// A messenger with the firmware's handlers: 0 (who are you), 7 (write licence key)
/// and the default handler.
fn messenger() -> CmdMessenger {
    let mut c = CmdMessenger::default();
    c.attach(cmd::WHO_ARE_YOU);
    c.attach(cmd::WRITE_LICENSE_KEY);
    c.attach_default();
    c
}

fn feed_with(c: &mut CmdMessenger, bytes: &[u8], mut on: impl FnMut(&mut CmdMessenger, Dispatch)) {
    for chunk in bytes.chunks(MAX_STREAM_BUFFER) {
        c.stream_buffer_mut()[..chunk.len()].copy_from_slice(chunk);
        for i in 0..chunk.len() {
            let b = c.stream_byte(i);
            if c.process_byte(b) {
                if let Some(d) = c.handle_message() {
                    on(c, d);
                }
            }
        }
    }
}

fn feed(c: &mut CmdMessenger, bytes: &[u8]) -> Option<Dispatch> {
    let mut last = None;
    feed_with(c, bytes, |_, d| last = Some(d));
    last
}

#[test]
fn who_are_you_is_parsed() {
    let mut c = messenger();
    assert_eq!(feed(&mut c, b"0,abcDEF1234567x;"), Some(Dispatch::Callback(0)));
    assert_eq!(c.read_string_arg(), Some(&b"abcDEF1234567x"[..]));
    assert_eq!(c.read_string_arg(), None);
}

#[test]
fn only_0_and_7_have_callbacks() {
    let mut c = messenger();
    assert_eq!(feed(&mut c, b"2,1,2;"), Some(Dispatch::Default)); // no send_message here
    assert_eq!(feed(&mut c, b"6;"), Some(Dispatch::Default));
    assert_eq!(feed(&mut c, b"50;"), Some(Dispatch::Default));
    assert_eq!(feed(&mut c, b"263;"), Some(Dispatch::Callback(7))); // 263 truncated to 8 bits
    assert_eq!(feed(&mut c, b"abc;"), Some(Dispatch::Callback(0)));
    assert_eq!(feed(&mut c, b";"), None);
    let mut w = Wire(Vec::new());
    assert!(!c.send_cmd_str(&mut w, cmd::ERROR, UNKNOWN_COMMAND_TEXT));
    assert_eq!(w.0, b"8,Command without callback.;");
}

#[test]
fn setup_logger_lines() {
    let mut c = messenger();
    let mut w = Wire(Vec::new());
    c.send_cmd_str(&mut w, cmd::LOGGER, READY_TEXT);
    c.send_cmd_str(&mut w, cmd::LOGGER, BLE_STARTED_TEXT);
    assert_eq!(w.0, b"5,***READY****;5,***BLESTARTED****;");
}

#[test]
fn escaped_separator_does_not_end_a_message() {
    let mut c = messenger();
    assert_eq!(feed(&mut c, b"7,ab/;cd;"), Some(Dispatch::Callback(7)));
    assert_eq!(c.read_string_arg(), Some(&b"ab/;cd"[..]));
}

#[test]
fn buffer_overflow_resets_at_63_bytes() {
    let mut c = messenger();
    let mut long = vec![b'1'; 63];
    long.extend_from_slice(b"7;");
    assert_eq!(feed(&mut c, &long), Some(Dispatch::Callback(7)));
}

/// The escape mismatch after a dropped 63-byte
/// message lets the argument scan run from the command buffer into the stream buffer.
#[test]
fn argument_scan_runs_from_command_buffer_into_stream_buffer() {
    let mut c = messenger();
    let mut wire = vec![b'x'; 62];
    wire.push(b'/');
    wire.extend_from_slice(b"///;");
    wire.extend_from_slice(b"0,hello;");
    let mut seen = Vec::new();
    feed_with(&mut c, &wire, |c, d| {
        let hs = c.read_string_arg().map(|s| s.to_vec());
        seen.push((d, hs, c.last_ptr()));
    });
    let sb = COMMAND_BUFFER_ADDR + 64;
    assert_eq!(
        seen,
        vec![
            (Dispatch::Callback(0), Some(b"hello;".to_vec()), sb + 75),
            (Dispatch::Callback(0), None, COMMAND_BUFFER_ADDR + 1)
        ]
    );
    assert_eq!(COMMAND_BUFFER_ADDR, 0x3ffc_19d9);
}

#[test]
fn atoi_matches_newlib() {
    assert_eq!(atoi(b"  -12x"), -12);
    assert_eq!(atoi(b"99999999999"), i32::MAX);
}

#[test]
fn xtea_and_open_gate() {
    let key: [u8; 16] = core::array::from_fn(|i| i as u8);
    let mut b = *b"ABCDEFGH";
    let k = xtea::key_schedule(&key);
    xtea::encrypt_block(&k, &mut b, 32);
    assert_eq!(b, [0x49, 0x7d, 0xf3, 0xd0, 0x72, 0x61, 0x2c, 0xb5]);
    // The key bytes of the handshake.
    assert_eq!(
        XTEA_KEY,
        [0xe2, 0x6d, 0x6f, 0xa9, 0x6e, 0xc2, 0x55, 0xd6, 0x00, 0x10, 0x4a, 0x33, 0xff, 0x18, 0x48, 0x4f]
    );
    let mut never = || -> u32 { unreachable!() };
    assert_eq!(&gate::open_gate(b"abcDEF1234567x", 1, &mut never), b"4148f86db770f174c75c259ce75d1268");
    assert_eq!(&gate::plaintext(b"0123456789ABCDEFGH", 0, &mut never), b"0123456789ABCD^0");
}

#[test]
fn arduino_random_matches_wmath() {
    let mut r = || 0x8000_0000u32;
    assert_eq!(random::random(0x100, &mut r), 0x80);
    assert_eq!(random::random_range(0x28, 0x7e, &mut || 0x8c00_0000u32), 0x57);
}

#[derive(Default)]
struct FakeNvs {
    blobs: HashMap<String, Vec<u8>>,
    log: Vec<String>,
}

impl NvsBackend for FakeNvs {
    fn open(&mut self, ns: &str) -> i32 {
        self.log.push(format!("open {ns}"));
        0
    }
    fn get_blob(&mut self, key: &str, out: Option<&mut [u8]>, len: &mut usize) -> i32 {
        self.log.push(format!("get {key} {}", out.is_some()));
        match self.blobs.get(key) {
            None => ESP_ERR_NVS_NOT_FOUND,
            Some(v) => {
                if let Some(o) = out {
                    if *len < v.len() {
                        *len = v.len();
                        return 0x110c;
                    }
                    o[..v.len()].copy_from_slice(v);
                }
                *len = v.len();
                0
            }
        }
    }
    fn set_blob(&mut self, key: &str, data: &[u8]) -> i32 {
        self.log.push(format!("set {key} {}", data.len()));
        self.blobs.insert(key.into(), data.to_vec());
        0
    }
    fn erase_key(&mut self, key: &str) -> i32 {
        self.log.push(format!("erase {key}"));
        self.blobs.remove(key);
        0
    }
    fn commit(&mut self) -> i32 {
        self.log.push("commit".into());
        0
    }
    fn close(&mut self) {}
}

#[test]
fn first_boot_identity_serial_and_ble_name() {
    let mut ee = Eeprom::new(FakeNvs::default());
    assert!(ee.begin(EEPROM_SIZE));
    assert_eq!(
        ee.nvs.log,
        [
            "open eeprom",
            "get eeprom false",
            "set expand 73",
            "erase expand",
            "commit",
            "set eeprom 73",
            "commit",
            "get eeprom true"
        ]
    );
    let id = serial::unique_id(&[0x24, 0x0a, 0xc4, 0x01, 0x02, 0xff]);
    let salts = [0x0100_0000u32, 0xab00_0000, 0x0f00_0000, 0x1000_0000, 0xff00_0000];
    let mut k = 0;
    let mut rng = || {
        let v = salts[k];
        k += 1;
        v
    };
    serial::check_eeprom(&mut ee, &id, &mut rng);
    let sn = serial::serial_number(&ee, &id);
    assert_eq!(&sn, b"01ab0f10ffff240ac40102ff");
    // The device name: from serial[18..24], upper case, 13 characters.
    assert_eq!(ble::device_name(&sn), b"OBD2HUD-0102F");
    assert_eq!(ble::to_upper(&sn), b"01AB0F10FFFF240AC40102FF");
}

#[test]
fn license_key_is_stored_and_committed() {
    let mut ee = Eeprom::new(FakeNvs::default());
    ee.begin(EEPROM_SIZE);
    ee.nvs.log.clear();
    // Storing a string commits it.
    ee.write_string(EEPROM_LICENSE_OFFSET, b"NA");
    ee.commit();
    assert_eq!(&ee.bytes()[12..15], &[2, b'N', b'A']);
    assert_eq!(ee.nvs.log, ["set eeprom 73"]);
}

#[test]
fn i_am_reply_bytes() {
    let mut c = messenger();
    let mut w = Wire(Vec::new());
    let mut never = || -> u32 { unreachable!() };
    let gate = gate::open_gate(b"abcDEF1234567x", LICENSE_STATUS_REPORTED, &mut never);
    c.send_cmd_start(&mut w, cmd::I_AM);
    c.send_cmd_arg_str(&mut w, &gate);
    c.send_cmd_bin_arg_u8(&mut w, PRODUCT);
    c.send_cmd_bin_arg_u8(&mut w, MODEL);
    c.send_cmd_bin_arg_u8(&mut w, VERSION);
    c.send_cmd_bin_arg_u8(&mut w, I_AM_LICENSE_BYTE);
    c.send_cmd_arg_str(&mut w, b"01ab0f10ffff240ac40102ff");
    c.send_cmd_end(&mut w);
    assert_eq!(w.0, b"1,4148f86db770f174c75c259ce75d1268,\x03,\x01,\x01,/\x00,01ab0f10ffff240ac40102ff;");
}

const GUARD: u32 = 0xa1b2_c35a;

#[derive(Default)]
struct FakeRegs {
    r: HashMap<usize, u32>,
    writes: Vec<(usize, u32)>,
    ops: Vec<(char, usize)>,
}
impl Regs for FakeRegs {
    fn read(&mut self, o: usize) -> u32 {
        self.ops.push(('r', o));
        *self.r.get(&o).unwrap_or(&0)
    }
    fn write(&mut self, o: usize, v: u32) {
        self.r.insert(o, v);
        self.writes.push((o, v));
        self.ops.push(('w', o));
    }
}

#[test]
fn can_init_programs_1m_bit_timing() {
    assert_eq!(CAN_SPEED, 1000);
    let mut r = FakeRegs::default();
    r.r.insert(can::REG_MOD, 1);
    can::init_controller(&mut r, CAN_SPEED, &can::DEFAULT_FILTER);
    // BRP 4 (8 MHz tq), SJW 1, TSEG1 = 4 + 1, TSEG2 = 1 + 1, SAM: 8 tq per bit = 1 Mbit/s.
    assert_eq!(r.r[&can::REG_BTR0], 0x44);
    assert_eq!(r.r[&can::REG_BTR1], 0x94);
    assert_eq!(r.r[&can::REG_CDR], 0x80);
    assert_eq!(r.r[&can::REG_OCR], 0x02);
    let order: Vec<usize> = r.writes.iter().map(|w| w.0).collect();
    assert_eq!(
        order,
        [
            0x7c, 0x18, 0x1c, 0x1c, 0x18, 0x1c, 0x10, 0x00, 0x40, 0x44, 0x48, 0x4c, 0x50, 0x54, 0x58, 0x5c, 0x20, 0x3c,
            0x38
        ]
    );
    assert_eq!(r.ops[r.ops.len() - 4..], [('w', 0x3c), ('w', 0x38), ('r', 0x30), ('r', 0x0c)]);
    can::start_controller(&mut r);
    assert_eq!(r.r[&can::REG_MOD], 0x00);
}

#[test]
fn obd_response_register_image() {
    // A speed answer 03 41 0D 78 as the HUD receives it from id 0x7E8.
    let mut e = Ecu { slots: telemetry::parse(b"1~120~90~6500~1~55~100~4~0"), ..Default::default() };
    let rsp = e.handle(&CanFrame { fir: 8, msg_id: obd::ID_FUNCTIONAL, data: [2, 1, 0x0d, 0, 0, 0, 0, 0] });
    let mut r = FakeRegs::default();
    can::write_frame(&mut r, &rsp.frames[0]);
    assert_eq!(
        r.writes,
        [
            (0x40, 0x08),
            (0x44, 0xfd),   // 0x7e8 >> 3
            (0x48, 0xfd00), // 0x7e8 << 5: the 8-bit register keeps 0x00
            (0x4c, 0x03),
            (0x50, 0x41),
            (0x54, 0x0d),
            (0x58, 0x78),
            (0x5c, 0x00),
            (0x60, 0x00),
            (0x64, 0x00),
            (0x68, 0x00),
            (0x04, 0x01)
        ]
    );
}

#[test]
fn can_isr_layout_of_this_build() {
    // Standard request from the HUD: 0x7DF = regs 0xfb, 0xe0.
    let mut rx = FakeRegs::default();
    rx.r.insert(can::REG_IR, 0x01);
    rx.r.insert(0x40, 0x08);
    rx.r.insert(0x44, 0xfb);
    rx.r.insert(0x48, 0xe0);
    for (i, b) in [2u32, 1, 0x0c, 0x55, 0x55, 0x55, 0x55, 0x55].iter().enumerate() {
        rx.r.insert(0x4c + 4 * i, *b);
    }
    let o = can::isr(&mut rx, true, GUARD, [0; 8]);
    assert_eq!(o.rx.unwrap().msg_id, 0x7df);
    assert_eq!(o.rx.unwrap().data[..3], [2, 1, 0x0c]);
    assert!(o.release_rx && !o.give_tx && !o.stack_smash);
    // DLC 9: data byte 8 lands on canary byte 0 (sp+0x1c) -> a stack-check failure.
    let mut ov = FakeRegs::default();
    ov.r.insert(can::REG_IR, 0x01);
    ov.r.insert(0x40, 9);
    let o = can::isr(&mut ov, true, GUARD, [0; 8]);
    assert!(o.stack_smash);
    // DLC 15 with bytes 8..11 equal to the guard: canary intact.
    let mut eq = FakeRegs::default();
    eq.r.insert(can::REG_IR, 0x01);
    eq.r.insert(0x40, 15);
    for (k, b) in GUARD.to_le_bytes().iter().enumerate() {
        eq.r.insert(0x4c + 4 * (8 + k), *b as u32);
    }
    assert!(!can::isr(&mut eq, true, GUARD, [0; 8]).stack_smash);
    // DLC 2: only data[0..2] are written; data[2..8] keep the uninitialised stack bytes.
    let mut short = FakeRegs::default();
    short.r.insert(can::REG_IR, 0x01);
    short.r.insert(0x40, 0x02);
    short.r.insert(0x4c, 0x01);
    short.r.insert(0x50, 0x0c);
    let f = can::isr(&mut short, true, GUARD, [0xa5; 8]).rx.unwrap();
    assert_eq!(f.data, [0x01, 0x0c, 0xa5, 0xa5, 0xa5, 0xa5, 0xa5, 0xa5]);
    // TX-complete only.
    let mut tx = FakeRegs::default();
    tx.r.insert(can::REG_IR, 0x02);
    let o = can::isr(&mut tx, true, GUARD, [0; 8]);
    assert!(o.give_tx && o.rx.is_none() && !o.release_rx);
}

#[test]
fn ble_gatt_table_matches_ecu_py() {
    // The UUIDs the host uses.
    assert_eq!(ble::SERVICE_UUID, "c14fcc20-72f1-11ec-90d6-0242ac120003");
    assert_eq!(ble::TELEMETRY_UUID, "c97c6c5a-72f1-11ec-90d6-0242ac120003");
    assert_eq!(ble::OTA_UUID, "173c2db0-7723-11ec-90d6-0242ac120003");
    // The firmware revision without its last character is "1.0.0".
    let v = ble::Chr::FirmwareRevision.initial_value(b"");
    assert_eq!(&std::str::from_utf8(&v).unwrap()[..v.len() - 1], "1.0.0");
    // The host scans for a name that starts with "obd2hud-", ignoring case.
    assert!(ble::device_name(b"000000000000000000abcdef").to_ascii_lowercase().starts_with(b"obd2hud-"));
}
