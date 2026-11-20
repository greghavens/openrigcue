//! OBD-II responder: the firmware plays ECU 0x7E8 for the head-up display on the CAN bus.
//!
//! * the response frame of a mode and PID
//! * mode 1 with its value encoders (+40, /3, RPM, *255/100, plain)
//! * mode 9
//! * the CAN part of the main loop
//!
//! The functions here are pure: they return the frames to send, in order,
//! and the lines to print (on UART0), in order.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::can::CanFrame;
use crate::fpu::{trunc, ufloat, utrunc};
use crate::telemetry::{slot, SLOT_COUNT};

/// Functional request id the HUD sends to.
pub const ID_FUNCTIONAL: u32 = 0x7df;
/// Physical request id of this ECU.
pub const ID_PHYSICAL: u32 = 0x7e0;
/// Response id.
pub const ID_RESPONSE: u32 = 0x7e8;
/// ISO 15765-2 flow-control first byte that triggers the queued-frame flush.
pub const FLOW_CONTROL: u8 = 0x30;
/// Queued consecutive frames: 5 entries of 8 bytes.
/// Nothing ever queues a frame, so the queue is zero
/// for the whole run and the flush never sends a frame.
pub const QUEUE_ENTRIES: usize = 5;

/// The host value of the bytes [`make_frame`] does not write (data[3..6])
/// and of the FIR bits 4/5 it keeps. On the
/// device those are whatever the stack holds.
pub const UNINIT_FILL: u8 = 0;

/// The response frame of a mode and PID:
/// `FIR = ((stack & 0xf0) | 8) & 0xffffff38` (DLC 8, standard, data; bits 4/5 from stack),
/// message id 0x7e8, `data = [2, mode + 0x40, pid, ?, ?, ?, 0, 0]`.
#[cfg_attr(target_arch = "xtensa", inline(never))]
pub fn make_frame(mode: u8, pid: u8) -> CanFrame {
    #[cfg(target_arch = "xtensa")]
    unsafe {
        let mut storage = core::mem::MaybeUninit::<CanFrame>::uninit();
        let pointer = storage.as_mut_ptr().cast::<u8>();
        let original: u32;
        core::arch::asm!("l32i {value},{pointer},0",value=out(reg)original,pointer=in(reg)pointer,options(readonly,nostack));
        pointer.cast::<u32>().write_volatile(((original & 0xf0) | 8) & 0xffff_ff38);
        pointer.add(8).write_volatile(2);
        pointer.add(9).write_volatile(mode.wrapping_add(0x40));
        pointer.add(10).write_volatile(pid);
        pointer.add(4).cast::<u32>().write_volatile(ID_RESPONSE);
        pointer.add(14).write_volatile(0);
        pointer.add(15).write_volatile(0);
        let mut bytes = [0u8; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let value: u32;
            core::arch::asm!("l8ui {value},{pointer},0",value=out(reg)value,pointer=in(reg)pointer.add(index),options(readonly,nostack));
            *byte = value as u8;
        }
        return CanFrame {
            fir: u32::from_le_bytes(bytes[..4].try_into().unwrap()),
            msg_id: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            data: bytes[8..].try_into().unwrap(),
        };
    }
    #[cfg(not(target_arch = "xtensa"))]
    {
        make_frame_with_tail(mode, pid, UNINIT_FILL as u32, [UNINIT_FILL; 3])
    }
}

/// [`make_frame`] with the unwritten FIR bits and data bytes given explicitly, for deterministic host tests.
pub fn make_frame_with_tail(mode: u8, pid: u8, stack_fir: u32, tail: [u8; 3]) -> CanFrame {
    let fir = ((stack_fir & 0xf0) | 8) & 0xffff_ff38;
    CanFrame { fir, msg_id: ID_RESPONSE, data: [2, mode.wrapping_add(0x40), pid, tail[0], tail[1], tail[2], 0, 0] }
}

/// The `+40` encoder (coolant / oil temperature): `utrunc.s(x + 40.0)`, low byte.
pub fn enc_plus40(x: f32) -> u8 {
    utrunc(x + 40.0) as u8
}

/// The `/3` encoder (PID 0x0A): `utrunc.s(x / 3.0)`, low byte.
pub fn enc_div3(x: f32) -> u8 {
    utrunc(x / 3.0) as u8
}

/// The RPM encoder: `v = trunc.s(x * 4.0)`; A is `v / 256` rounded toward zero,
/// B is `v % 256` (low byte).
pub fn enc_rpm(x: f32) -> (u8, u8) {
    let v = trunc(x * 4.0);
    let a = (if v < 0 { v.wrapping_add(0xff) } else { v }) >> 8;
    (a as u8, (v % 256) as u8)
}

/// The `*255/100` encoder (PID 0x2F): `utrunc.s(x * 255.0 / 100.0)`, low byte.
pub fn enc_percent(x: f32) -> u8 {
    utrunc((x * 255.0) / 100.0) as u8
}

/// The plain encoder: `utrunc.s(x)`, low byte.
pub fn enc_plain(x: f32) -> u8 {
    utrunc(x) as u8
}

/// What one handled request produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Response {
    /// Frames to transmit, in order.
    pub frames: Vec<CanFrame>,
    /// Printed output, in order (text as formatted, before the `\n` translation of the output).
    pub printf: Vec<String>,
}

/// The state the responder reads or keeps between requests.
#[derive(Debug, Clone, PartialEq)]
pub struct Ecu {
    /// The float slots (written by the telemetry characteristic).
    pub slots: [f32; SLOT_COUNT],
    /// The LED status, after it was toggled for this frame.
    pub led_status: bool,
    /// The RPM value sent last.
    pub rpm_sent: f32,
    /// Queued frames.
    pub queue: [[u8; 8]; QUEUE_ENTRIES],
}

impl Default for Ecu {
    fn default() -> Self {
        Self {
            slots: [0.0; SLOT_COUNT],
            led_status: crate::consts::LED_STATUS_INITIAL,
            rpm_sent: 0.0,
            queue: [[0; 8]; QUEUE_ENTRIES],
        }
    }
}

impl Ecu {
    /// Mode 1: always sends one frame; unknown PIDs print first.
    pub fn mode1(&mut self, pid: u8, out: &mut Response) {
        let slots = self.slots;
        self.mode1_live(pid, out, |slot| slots[slot]);
    }

    /// Mode 1 that reads the requested shared telemetry slot inside its PID
    /// branch. A concurrently running BLE callback may have published only a
    /// prefix of its new telemetry line at that point.
    pub fn mode1_live(&mut self, pid: u8, out: &mut Response, mut read: impl FnMut(usize) -> f32) {
        let mut f = make_frame(1, pid);
        match pid {
            0x0c => {
                f.data[0] = f.data[0].wrapping_add(2);
                // With the LED on +1, otherwise -1 unless that drops below 1.
                // RPM is read once, before computing the dither.
                let rpm = read(slot::RPM);
                self.rpm_sent = if self.led_status {
                    rpm + 1.0
                } else {
                    let m = rpm - 1.0;
                    // An ordered comparison: a NaN keeps the RPM itself.
                    if 1.0 <= m {
                        m
                    } else {
                        rpm
                    }
                };
                let (a, b) = enc_rpm(self.rpm_sent);
                f.data[3] = a;
                f.data[4] = b;
            }
            0x05 => {
                f.data[0] = f.data[0].wrapping_add(1);
                f.data[3] = enc_plus40(read(slot::COOLANT));
            }
            0x00 => {
                // Supported PIDs 01-20 = 08 78 00 01 (data[5] = pid = 0).
                f.data[0] = f.data[0].wrapping_add(4);
                f.data[3] = 0x08;
                f.data[4] = 0x78;
                f.data[5] = pid;
                f.data[6] = 1;
            }
            0x0a => {
                f.data[0] = f.data[0].wrapping_add(1);
                f.data[3] = enc_div3(read(slot::FUEL_PRESSURE));
            }
            0x0b => {
                f.data[0] = f.data[0].wrapping_add(1);
                f.data[3] = enc_plain(read(slot::MAP));
            }
            0x2f => {
                f.data[0] = f.data[0].wrapping_add(1);
                f.data[3] = enc_percent(read(slot::FUEL_LEVEL));
            }
            0x0d => {
                // `utrunc.s` then `ufloat.s` before the plain encoder.
                f.data[0] = f.data[0].wrapping_add(1);
                f.data[3] = enc_plain(ufloat(utrunc(read(slot::SPEED))));
            }
            0x20 => {
                // Supported PIDs 21-40 = 40 02 00 01.
                f.data[0] = f.data[0].wrapping_add(4);
                f.data[3] = 0x40;
                f.data[4] = 0x02;
                f.data[5] = 0;
                f.data[6] = 1;
            }
            0x33 => {
                f.data[0] = f.data[0].wrapping_add(1);
                f.data[3] = enc_plain(read(slot::BARO));
            }
            0x5c => {
                f.data[0] = f.data[0].wrapping_add(1);
                f.data[3] = enc_plus40(read(slot::OIL_TEMP));
            }
            _ => {
                let mut l = String::new();
                let _ = writeln!(l, "Unsupported pid {} !", pid);
                out.printf.push(l);
            }
        }
        out.frames.push(f);
    }

    /// Mode 9: prints, then answers only PID 0 (`06 49 00 40 00 00 00 00`).
    pub fn mode9(&mut self, pid: u8, out: &mut Response) {
        let mut l = String::new();
        let _ = writeln!(l, "Responding to Mode 9 PID 0x{:02x}", pid);
        out.printf.push(l);
        let mut f = make_frame(9, pid);
        if pid == 0 {
            f.data[0] = f.data[0].wrapping_add(4);
            f.data[3] = 0x40;
            f.data[4] = 0;
            f.data[5] = 0;
            f.data[6] = 0;
            out.frames.push(f);
        }
    }

    /// The CAN part of the main loop for one received frame (the LED was already
    /// toggled; `led_status` is the toggled value).
    pub fn handle(&mut self, rx: &CanFrame) -> Response {
        let slots = self.slots;
        self.handle_live(rx, |slot| slots[slot])
    }

    /// Live counterpart to [`Self::handle`] for the separate BLE and CAN tasks.
    pub fn handle_live(&mut self, rx: &CanFrame, read: impl FnMut(usize) -> f32) -> Response {
        let mut out = Response::default();
        if rx.msg_id == ID_FUNCTIONAL {
            match rx.data[1] {
                1 => self.mode1_live(rx.data[2], &mut out, read),
                9 => self.mode9(rx.data[2], &mut out),
                m => {
                    let mut l = String::new();
                    let _ = writeln!(l, "Unsupported mode {} !", m);
                    out.printf.push(l);
                }
            }
        } else if rx.msg_id == ID_PHYSICAL {
            // The line carries the message id (0x7e0).
            let mut l = String::new();
            let _ = writeln!(l, "ECU MSG {} !", rx.msg_id as i32);
            out.printf.push(l);
            if rx.data[0] == FLOW_CONTROL {
                let mut f = make_frame(0, 0);
                for e in self.queue.iter() {
                    if e[0] != 0 {
                        f.data = *e;
                        out.frames.push(f);
                    }
                }
                self.queue = [[0; 8]; QUEUE_ENTRIES];
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;

    fn req(id: u32, d: [u8; 8]) -> CanFrame {
        CanFrame { fir: 8, msg_id: id, data: d }
    }

    fn ecu(slots: [f32; SLOT_COUNT]) -> Ecu {
        Ecu { slots, ..Default::default() }
    }

    #[test]
    fn make_frame_layout() {
        let f = make_frame(1, 0x0d);
        assert_eq!(f.fir, 8);
        assert_eq!(f.msg_id, 0x7e8);
        assert_eq!(f.data, [2, 0x41, 0x0d, 0, 0, 0, 0, 0]);
        assert_eq!(make_frame(9, 2).data[1], 0x49);
        assert_eq!(make_frame(0, 0).data[1], 0x40);
    }

    #[test]
    fn supported_pid_bitmaps() {
        let mut e = Ecu::default();
        let r = e.handle(&req(0x7df, [2, 1, 0x00, 0, 0, 0, 0, 0]));
        assert_eq!(r.frames[0].data, [6, 0x41, 0x00, 0x08, 0x78, 0x00, 0x01, 0]);
        let r = e.handle(&req(0x7df, [2, 1, 0x20, 0, 0, 0, 0, 0]));
        assert_eq!(r.frames[0].data, [6, 0x41, 0x20, 0x40, 0x02, 0x00, 0x01, 0]);
        assert!(r.printf.is_empty());
    }

    #[test]
    fn srs_line_encodes_per_pid() {
        // The line: POWER~SPEED~WATER~RPM~TURBO~FUEL~OIL_T~OIL_P~VACUUM
        let mut e = ecu(crate::telemetry::parse(b"1~120~90~6500~1~55~100~4~0"));
        let one = |e: &mut Ecu, pid: u8| e.handle(&req(0x7df, [2, 1, pid, 0, 0, 0, 0, 0])).frames[0].data;
        assert_eq!(one(&mut e, 0x0d), [3, 0x41, 0x0d, 120, 0, 0, 0, 0]);
        assert_eq!(one(&mut e, 0x05), [3, 0x41, 0x05, 130, 0, 0, 0, 0]);
        assert_eq!(one(&mut e, 0x0b), [3, 0x41, 0x0b, 1, 0, 0, 0, 0]);
        assert_eq!(one(&mut e, 0x2f), [3, 0x41, 0x2f, 140, 0, 0, 0, 0]); // 55*255/100 = 140.25
        assert_eq!(one(&mut e, 0x5c), [3, 0x41, 0x5c, 140, 0, 0, 0, 0]);
        assert_eq!(one(&mut e, 0x0a), [3, 0x41, 0x0a, 1, 0, 0, 0, 0]); // 4/3
        assert_eq!(one(&mut e, 0x33), [3, 0x41, 0x33, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn rpm_dithers_with_the_led() {
        let mut s = [0.0; SLOT_COUNT];
        s[slot::RPM] = 6500.0;
        let mut e = ecu(s);
        e.led_status = true;
        let r = e.handle(&req(0x7df, [2, 1, 0x0c, 0, 0, 0, 0, 0]));
        // (6501 * 4) = 26004 = 0x6594
        assert_eq!(r.frames[0].data, [4, 0x41, 0x0c, 0x65, 0x94, 0, 0, 0]);
        assert_eq!(e.rpm_sent, 6501.0);
        e.led_status = false;
        let r = e.handle(&req(0x7df, [2, 1, 0x0c, 0, 0, 0, 0, 0]));
        // 6499 * 4 = 25996 = 0x658c
        assert_eq!(r.frames[0].data[3..5], [0x65, 0x8c]);
        // Below 2 the LED-off value stays the RPM itself.
        e.slots[slot::RPM] = 1.5;
        e.handle(&req(0x7df, [2, 1, 0x0c, 0, 0, 0, 0, 0]));
        assert_eq!(e.rpm_sent, 1.5);
        e.slots[slot::RPM] = 2.0;
        e.handle(&req(0x7df, [2, 1, 0x0c, 0, 0, 0, 0, 0]));
        assert_eq!(e.rpm_sent, 1.0);
    }

    #[test]
    fn encoder_saturation_follows_the_fpu() {
        assert_eq!(enc_plain(-5.0), 0); // utrunc < 0 -> 0x80000000
        assert_eq!(enc_plain(300.0), 44); // low byte of 300
        assert_eq!(enc_plain(f32::NAN), 0xff);
        assert_eq!(enc_plus40(-50.0), 0);
        assert_eq!(enc_rpm(-1.0), (0, 0xfc)); // v = -4: (-4 + 255) >> 8 = 0, -4 % 256 = -4
        assert_eq!(enc_rpm(-100.0), (0xff, 0x70)); // v = -400: (-145) >> 8 = -1, -400 % 256 = -144
        assert_eq!(enc_rpm(f32::NAN), (0xff, 0xff)); // trunc.s NaN -> 0x7fffffff: 0x7fffff, 0xff
        assert_eq!(enc_rpm(20000.0), (0x38, 0x80)); // 80000 = 0x13880
    }

    #[test]
    fn unsupported_pid_still_answers() {
        let mut e = Ecu::default();
        let r = e.handle(&req(0x7df, [2, 1, 0x42, 0, 0, 0, 0, 0]));
        assert_eq!(r.printf, ["Unsupported pid 66 !\n"]);
        assert_eq!(r.frames[0].data, [2, 0x41, 0x42, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn mode9_and_unknown_modes() {
        let mut e = Ecu::default();
        let r = e.handle(&req(0x7df, [2, 9, 0, 0, 0, 0, 0, 0]));
        assert_eq!(r.printf, ["Responding to Mode 9 PID 0x00\n"]);
        assert_eq!(r.frames[0].data, [6, 0x49, 0, 0x40, 0, 0, 0, 0]);
        let r = e.handle(&req(0x7df, [2, 9, 2, 0, 0, 0, 0, 0]));
        assert_eq!(r.printf, ["Responding to Mode 9 PID 0x02\n"]);
        assert!(r.frames.is_empty());
        let r = e.handle(&req(0x7df, [2, 3, 0, 0, 0, 0, 0, 0]));
        assert_eq!(r.printf, ["Unsupported mode 3 !\n"]);
        assert!(r.frames.is_empty());
    }

    #[test]
    fn physical_id_prints_and_flushes_the_empty_queue() {
        let mut e = Ecu::default();
        let r = e.handle(&req(0x7e0, [0x30, 0, 0, 0, 0, 0, 0, 0]));
        assert_eq!(r.printf, ["ECU MSG 2016 !\n"]);
        assert!(r.frames.is_empty());
        e.queue[1] = [0x21, 1, 2, 3, 4, 5, 6, 7];
        let r = e.handle(&req(0x7e0, [0x30, 0, 0, 0, 0, 0, 0, 0]));
        assert_eq!(r.frames, [CanFrame { fir: 8, msg_id: 0x7e8, data: [0x21, 1, 2, 3, 4, 5, 6, 7] }]);
        assert_eq!(e.queue, [[0; 8]; QUEUE_ENTRIES]);
        let r = e.handle(&req(0x7e0, [0x02, 1, 0x0c, 0, 0, 0, 0, 0]));
        assert_eq!(r.printf.len(), 1);
        assert!(r.frames.is_empty());
        // Other ids are ignored.
        assert_eq!(e.handle(&req(0x123, [2, 1, 0, 0, 0, 0, 0, 0])), Response::default());
    }

    #[test]
    fn live_pid_reads_only_its_source_slot_once() {
        // Each value PID of mode 1 reads its one slot.
        // The bitmap PIDs, unknown PIDs and mode 9 read no slot.
        for (pid, wanted) in [
            (0x0c, Some(slot::RPM)),
            (0x0d, Some(slot::SPEED)),
            (0x05, Some(slot::COOLANT)),
            (0x0a, Some(slot::FUEL_PRESSURE)),
            (0x0b, Some(slot::MAP)),
            (0x2f, Some(slot::FUEL_LEVEL)),
            (0x5c, Some(slot::OIL_TEMP)),
            (0x33, Some(slot::BARO)),
            (0x00, None),
            (0x20, None),
            (0x42, None),
        ] {
            for led in [false, true] {
                let mut reads = Vec::new();
                let mut e = Ecu { led_status: led, ..Default::default() };
                let result = e.handle_live(&req(ID_FUNCTIONAL, [2, 1, pid, 0, 0, 0, 0, 0]), |index| {
                    reads.push(index);
                    40.0
                });
                assert_eq!(reads, wanted.into_iter().collect::<Vec<_>>(), "PID {pid:x}, LED {led}");
                assert_eq!(result.frames.len(), 1);
            }
        }
        for frame in [
            req(ID_FUNCTIONAL, [2, 9, 0, 0, 0, 0, 0, 0]),
            req(ID_FUNCTIONAL, [2, 3, 0, 0, 0, 0, 0, 0]),
            req(ID_PHYSICAL, [FLOW_CONTROL, 0, 0, 0, 0, 0, 0, 0]),
            req(0x123, [0; 8]),
        ] {
            Ecu::default().handle_live(&frame, |_| panic!("branch must not read telemetry"));
        }
    }

    #[test]
    fn each_published_telemetry_prefix_is_visible_to_every_pid() {
        let shared = RefCell::new([0.0; SLOT_COUNT]);
        let updates = [1.0, 120.0, 90.0, 6500.0, 1.0, 55.0, 100.0, 4.0, 0.0];
        let mut count = 0;
        crate::telemetry::publish(b"1~120~90~6500~1~55~100~4~0", |index, value| {
            assert_eq!(index, count);
            count += 1;
            shared.borrow_mut()[index] = value;
            let snapshot = *shared.borrow();
            assert_eq!(&snapshot[..count], &updates[..count]);
            assert!(snapshot[count..].iter().all(|v| *v == 0.0));
            for led in [false, true] {
                for pid in [0x0c, 0x0d, 0x05, 0x0a, 0x0b, 0x2f, 0x5c, 0x33, 0x00, 0x20, 0x42] {
                    let mut expected = Ecu { slots: snapshot, led_status: led, ..Default::default() };
                    let mut live = Ecu { led_status: led, ..Default::default() };
                    let frame = req(ID_FUNCTIONAL, [2, 1, pid, 0, 0, 0, 0, 0]);
                    let output = live.handle_live(&frame, |slot| shared.borrow()[slot]);
                    assert_eq!(output, expected.handle(&frame), "prefix {count}, PID {pid:x}");
                    assert_eq!(live.rpm_sent.to_bits(), expected.rpm_sent.to_bits());
                }
            }
        });
        assert_eq!(count, SLOT_COUNT);
    }

    #[test]
    fn complete_pid_space_matches_independent_c_at_each_publication_boundary() {
        let mut shared = [0.0; SLOT_COUNT];
        let mut hash = 14_695_981_039_346_656_037u64;
        let mut reads = 0;
        let mut frames = 0;
        crate::telemetry::publish(b"1~120~90~6500~1~55~100~4~0", |index, value| {
            shared[index] = value;
            for led_status in [false, true] {
                for pid in 0..=255 {
                    let mut ecu = Ecu { led_status, ..Default::default() };
                    let result = ecu.handle_live(&req(ID_FUNCTIONAL, [2, 1, pid, 0, 0, 0, 0, 0]), |slot| {
                        reads += 1;
                        shared[slot]
                    });
                    for byte in result.frames[0].data {
                        hash ^= u64::from(byte);
                        hash = hash.wrapping_mul(1_099_511_628_211);
                    }
                    frames += 1;
                }
            }
        });
        // The hash, frame count and read count over the finite input family.
        assert_eq!((hash, frames, reads), (0x1f21_4856_61b3_98b9, 4608, 144));
    }
}
