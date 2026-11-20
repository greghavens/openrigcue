// Calls the public Rust implementations; the C implementation they are compared
// with has separate control flow and byte/word storage.
extern crate belt_fw as belt;
extern crate obd2hud_core as obd;
struct Hash(u64);
impl Hash {
    fn word(&mut self, v: u32) {
        for b in v.to_le_bytes() {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(1099511628211);
        }
    }
    fn bytes(&mut self, v: &[u8]) {
        self.word(v.len() as u32);
        for b in v {
            self.word(u32::from(*b));
        }
    }
}
// A single trace must interleave register reads and stack stores; an Rc trace
// lets the two independent production traits publish into the same sequence.
use std::cell::RefCell;
use std::rc::Rc;
struct Regs {
    regs: [u32; 64],
    trace: Rc<RefCell<Hash>>,
}
struct Stack {
    memory: [u8; 64],
    frame: usize,
    trace: Rc<RefCell<Hash>>,
}
impl Regs {
    fn read(&mut self, off: usize) -> u32 {
        let v = self.regs[off / 4];
        let mut h = self.trace.borrow_mut();
        h.word(1);
        h.word(off as u32);
        h.word(v);
        v
    }
    fn write(&mut self, off: usize, v: u32) {
        let mut h = self.trace.borrow_mut();
        h.word(2);
        h.word(off as u32);
        h.word(v);
        self.regs[off / 4] = v;
    }
}
macro_rules! actual_traits {
    ($module:ident) => {
        impl $module::can::Regs for Regs {
            fn read(&mut self, off: usize) -> u32 {
                self.read(off)
            }
            fn write(&mut self, off: usize, v: u32) {
                self.write(off, v)
            }
        }
        impl $module::can::FrameStorage for Stack {
            fn fir(&mut self, v: u32) {
                self.store(self.frame, v, 4)
            }
            fn msg_id(&mut self, v: u32) {
                self.store(self.frame + 4, v, 4)
            }
            fn data(&mut self, index: usize, v: u8) {
                self.store(self.frame + 8 + index, u32::from(v), 1)
            }
        }
    };
}
actual_traits!(belt);
actual_traits!(obd);
impl Stack {
    fn store(&mut self, off: usize, v: u32, width: usize) {
        let mut h = self.trace.borrow_mut();
        h.word(3);
        h.word(off as u32);
        h.word(v);
        h.word(width as u32);
        self.memory[off..off + width].copy_from_slice(&v.to_le_bytes()[..width]);
    }
}
fn receive(h: &mut Hash, board: u32, ir: u32, queue: bool, ff: u32, dlc: u32, pattern: u32) {
    let trace = Rc::new(RefCell::new(Hash(h.0)));
    let mut memory = std::array::from_fn(|i| ((i as u32 * 29 + pattern * 71) ^ 0xb6) as u8);
    let frame = if board == 0 { 8 } else { 12 };
    let guard = 0x91e2c3b4u32;
    memory[28..32].copy_from_slice(&guard.to_le_bytes());
    let stale = memory[frame + 8..frame + 16].try_into().unwrap();
    let mut regs = std::array::from_fn(|i| 0x12340000u32.wrapping_add(i as u32 * 0x1937 + pattern * 0x4381));
    regs[3] = ir;
    regs[16] = (if ff == 0 { 0 } else { 128 }) | dlc;
    let mut r = Regs { regs, trace: trace.clone() };
    let mut s = Stack { memory, frame, trace: trace.clone() };
    let (have, give, release, smash, fir, id, data) = if board == 0 {
        let o = belt::can::isr_with_storage(&mut r, queue, guard, stale, &mut s);
        let f = o.rx.unwrap_or_default();
        (o.rx.is_some(), o.give_tx, o.release_rx, o.stack_smash, f.fir, f.msg_id, f.data)
    } else {
        let o = obd::can::isr_with_storage(&mut r, queue, guard, stale, &mut s);
        let f = o.rx.unwrap_or_default();
        (o.rx.is_some(), o.give_tx, o.release_rx, o.stack_smash, f.fir, f.msg_id, f.data)
    };
    let mut out = trace.borrow_mut();
    out.word(have as u32);
    out.word(give as u32);
    out.word(release as u32);
    out.word(smash as u32);
    if have {
        out.word(fir);
        out.word(id);
        out.bytes(&data);
    }
    out.bytes(&s.memory);
    h.0 = out.0;
}
fn transmit(h: &mut Hash, board: u32, ff: u32, dlc: u32, pattern: u32) {
    let regs = std::array::from_fn(|i| 0xabc00000 + i as u32 * 19 + pattern);
    let trace = Rc::new(RefCell::new(Hash(h.0)));
    let mut r = Regs { regs, trace: trace.clone() };
    let fir = (if ff == 0 { 0 } else { 128 }) | dlc;
    let msg_id = 0x932a51bc + pattern;
    let data = |i: usize| ((i as u32 * 31 + pattern * 73) ^ 0xa9) as u8;
    if board == 0 {
        let f = belt::can::CanFrame { fir, msg_id, data: std::array::from_fn(data) };
        belt::can::write_frame_with_data(&mut r, &f, data);
    } else {
        let f = obd::can::CanFrame { fir, msg_id, data: std::array::from_fn(data) };
        obd::can::write_frame_with_data(&mut r, &f, data);
    }
    h.0 = trace.borrow().0;
}
fn main() {
    let mut h = Hash(14695981039346656037);
    for board in 0..2 {
        for ir in [0, 1, 2, 3, 254, 255] {
            for queue in [false, true] {
                for ff in 0..2 {
                    for dlc in 0..16 {
                        for pattern in 0..2 {
                            receive(&mut h, board, ir, queue, ff, dlc, pattern);
                        }
                    }
                }
            }
        }
    }
    for board in 0..2 {
        for ff in 0..2 {
            for dlc in 0..16 {
                for pattern in 0..2 {
                    transmit(&mut h, board, ff, dlc, pattern);
                }
            }
        }
    }
    for board in 0..2 {
        for length in 0..20 {
            for status in [0, 1, 2, 3, 255] {
                for pattern in 0..3 {
                    let input: [u8; 20] = std::array::from_fn(|i| {
                        if pattern == 1 {
                            b'?'
                        } else if pattern == 2 && i == 5 {
                            0
                        } else {
                            b'a' + i as u8
                        }
                    });
                    let tail = std::array::from_fn(|i| (i * 17 + length) as u8);
                    let mut seed = 0x1234abcdu32;
                    let mut rng = || {
                        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                        seed
                    };
                    let out = if board == 0 {
                        belt::gate::plaintext_with_tail(&input[..length], status, &mut rng, tail)
                    } else {
                        obd::gate::plaintext_with_tail(&input[..length], status, &mut rng, tail)
                    };
                    h.bytes(&out);
                }
            }
        }
    }
    for p in 0..256u32 {
        let args = std::array::from_fn(|i| (i as u32 * 37 + p) as u8);
        let old = 0xdead0000 | p;
        let f = belt::can::frame_from_send_message_with_fir(&args, old);
        h.word(f.fir);
        h.word(f.msg_id);
        h.bytes(&f.data);
        let f = obd::obd::make_frame_with_tail(p as u8, (p ^ 0x73) as u8, old, [0xa9, 0x4d, 0xe2]);
        h.word(f.fir);
        h.word(f.msg_id);
        h.bytes(&f.data);
    }
    println!("native_state={:016x}", h.0);
}
