//! The CAN controller: initialisation, the interrupt handler and frame
//! transmission.
//!
//! Register offsets are relative to the CAN controller base 0x3ff6b000.
/// CAN controller base address.
pub const CAN_BASE: usize = 0x3ff6_b000;
pub const REG_MOD: usize = 0x00;
pub const REG_CMR: usize = 0x04;
pub const REG_IR: usize = 0x0c;
pub const REG_IER: usize = 0x10;
pub const REG_BTR0: usize = 0x18;
pub const REG_BTR1: usize = 0x1c;
pub const REG_OCR: usize = 0x20;
/// Error code capture (read-to-clear).
pub const REG_ECC: usize = 0x30;
pub const REG_RXERR: usize = 0x38;
pub const REG_TXERR: usize = 0x3c;
/// TX/RX buffer: frame information byte, then identifier and data bytes.
pub const REG_FIR: usize = 0x40;
pub const REG_CDR: usize = 0x7c;
/// DPORT_PERIP_CLK_EN / DPORT_PERIP_RST_EN and the CAN bit (bit 19).
pub const DPORT_PERIP_CLK_EN: usize = 0x3ff0_00c0;
pub const DPORT_PERIP_RST_EN: usize = 0x3ff0_00c4;
pub const DPORT_CAN_BIT: u32 = 0x8_0000;

/// FIR bits.
pub const FIR_DLC_MASK: u32 = 0x0f;
pub const FIR_RTR: u32 = 0x40;
pub const FIR_FF: u32 = 0x80;

/// A CAN frame as queued: { FIR (u32), message id (u32), data[8] } = 16 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CanFrame {
    pub fir: u32,
    pub msg_id: u32,
    pub data: [u8; 8],
}

impl CanFrame {
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[..4].copy_from_slice(&self.fir.to_le_bytes());
        b[4..8].copy_from_slice(&self.msg_id.to_le_bytes());
        b[8..].copy_from_slice(&self.data);
        b
    }
}

/// The acceptance filter: FM = 0 (dual), ACR0..3 = 0, AMR0..3 = 0xff.
#[derive(Debug, Clone, Copy)]
pub struct CanFilter {
    pub fm: u8,
    pub acr: [u8; 4],
    pub amr: [u8; 4],
}

pub const DEFAULT_FILTER: CanFilter = CanFilter { fm: 0, acr: [0; 4], amr: [0xff; 4] };

/// Volatile register access (MMIO on target, a map in tests).
pub trait Regs {
    fn read(&mut self, offset: usize) -> u32;
    fn write(&mut self, offset: usize, value: u32);
}

fn rmw<R: Regs>(r: &mut R, off: usize, f: impl FnOnce(u32) -> u32) {
    let v = r.read(off);
    r.write(off, f(v));
}

/// Rounding for the non-negative values reached here (half away from zero).
fn round_pos(x: f64) -> f64 {
    let t = x as u64 as f64;
    if x - t >= 0.5 {
        t + 1.0
    } else {
        t
    }
}

/// The BRP field for a time quantum: half the APB clock (80 MHz) times the quantum,
/// minus 1, divided by 1000000 and rounded, minus 1.
pub fn brp_for_tq(tq_us: f64) -> u32 {
    let v = ((tq_us * 80_000_000.0) * 0.5 - 1.0) / 1_000_000.0;
    (round_pos(v) as u32).wrapping_sub(1)
}

/// The register sequence of the initialisation up to (not including) the interrupt allocation.
/// DPORT clock/reset and GPIO matrix are done by the caller.
pub fn init_controller<R: Regs>(r: &mut R, speed: u32, filter: &CanFilter) {
    // PeliCAN mode
    rmw(r, REG_CDR, |v| v | 0x80);
    // SJW = 1
    rmw(r, REG_BTR0, |v| (v & !0xc0) | 0x40);
    // TSEG2 = 1
    rmw(r, REG_BTR1, |v| (v & !0x70) | 0x10);
    let tq: f64 = match speed {
        800 => {
            rmw(r, REG_BTR1, |v| (v & !0x0f) | 6);
            0.125
        }
        1000 => {
            rmw(r, REG_BTR1, |v| (v & !0x0f) | 4);
            0.125
        }
        200 => {
            rmw(r, REG_BTR1, |v| (v & !0x0f) | 12);
            rmw(r, REG_BTR1, |v| (v & !0x70) | 0x50);
            0.25
        }
        _ => {
            rmw(r, REG_BTR1, |v| (v & !0x0f) | 12);
            // 1000 / speed / 16, computed in single precision.
            (((1000.0f32) / (speed as f32)) * 0.0625f32) as f64
        }
    };
    let brp = brp_for_tq(tq);
    rmw(r, REG_BTR0, |v| (v & !0x3f) | (brp & 0x3f));
    // SAM = triple sampling
    rmw(r, REG_BTR1, |v| v | 0x80);
    r.write(REG_IER, 0xff);
    rmw(r, REG_MOD, |v| (v & !0x08) | (((filter.fm & 1) as u32) << 3));
    for i in 0..4 {
        r.write(REG_FIR + 4 * i, filter.acr[i] as u32);
    }
    for i in 0..4 {
        r.write(REG_FIR + 0x10 + 4 * i, filter.amr[i] as u32);
    }
    // Normal output mode
    rmw(r, REG_OCR, |v| (v & !0x03) | 0x02);
    r.write(REG_TXERR, 0);
    r.write(REG_RXERR, 0);
    // Dummy reads of ECC and IR (both read-to-clear),
    // dropping any latched error capture / interrupt before the ISR is installed.
    let _ = r.read(REG_ECC);
    let _ = r.read(REG_IR);
}

/// Last step of the initialisation (after the ISR and TX semaphore exist): leave reset mode.
pub fn start_controller<R: Regs>(r: &mut R) {
    rmw(r, REG_MOD, |v| v & !0x01);
}

/// The register part of a frame transmission: fill the TX buffer, then request
/// transmission (CMR |= TR). The caller then blocks on the TX semaphore.
pub fn write_frame<R: Regs>(r: &mut R, f: &CanFrame) {
    write_frame_with_data(r, f, |index| {
        if let Some(value) = f.data.get(index) {
            return *value;
        }
        #[cfg(target_arch = "xtensa")]
        unsafe {
            // Data bytes are read up to the DLC, including the machine bytes
            // beyond the 16-byte frame for a malformed DLC of 9..15. The producer
            // normally uses a DLC of 8.
            let value: u32;
            let pointer = (f as *const CanFrame).cast::<u8>().wrapping_add(8 + index);
            core::arch::asm!("l8ui {value},{pointer},0",value=out(reg)value,pointer=in(reg)pointer,options(readonly,nostack));
            value as u8
        }
        #[cfg(not(target_arch = "xtensa"))]
        {
            0
        }
    });
}

/// [`write_frame`] with the data bytes given explicitly, including those
/// beyond data[8], for deterministic host tests.
pub fn write_frame_with_data<R: Regs>(r: &mut R, f: &CanFrame, mut data: impl FnMut(usize) -> u8) {
    r.write(REG_FIR, f.fir);
    let dlc = (f.fir & FIR_DLC_MASK) as usize;
    if f.fir & FIR_FF == 0 {
        r.write(REG_FIR + 0x04, f.msg_id >> 3);
        r.write(REG_FIR + 0x08, f.msg_id << 5);
        for i in 0..dlc {
            // data[i] for i >= 8 are the bytes after the frame.
            r.write(REG_FIR + 0x0c + 4 * i, data(i) as u32);
        }
    } else {
        r.write(REG_FIR + 0x04, f.msg_id >> 21);
        r.write(REG_FIR + 0x08, f.msg_id >> 13);
        r.write(REG_FIR + 0x0c, f.msg_id >> 5);
        r.write(REG_FIR + 0x10, f.msg_id << 3);
        for i in 0..dlc {
            r.write(REG_FIR + 0x14 + 4 * i, data(i) as u32);
        }
    }
    rmw(r, REG_CMR, |v| v | 0x01);
}

/// What one invocation of the interrupt handler did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IsrOutcome {
    /// Frame to push to the receive queue (when RI and a queue exists).
    pub rx: Option<CanFrame>,
    /// Whether to give the transmit-complete semaphore (any interrupt bit other than RI).
    pub give_tx: bool,
    /// Whether to release the receive buffer (`CMR |= RRB`); done after the queue send.
    pub release_rx: bool,
    /// Whether the stack canary differs from the guard at ISR exit, which is
    /// a stack-check failure and aborts.
    pub stack_smash: bool,
}

/// Releases the receive buffer (sets RRB in CMR).
pub fn release_rx<R: Regs>(r: &mut R) {
    rmw(r, REG_CMR, |v| v | 0x04);
}

/// The register reads of the interrupt handler. `have_queue` says whether a receive queue exists,
/// `guard` is the stack guard value (stored as the canary at sp+0x1c on entry, compared at exit).
/// The caller pushes `rx` to the queue, then calls [`release_rx`] when `release_rx`.
///
/// Stack frame: the received frame at sp+0x0c (FIR),
/// sp+0x10 (message id), data at sp+0x14+i, canary at sp+0x1c. A
/// received DLC of 9..15 therefore writes data bytes 8..11 over the canary (12..14 land in
/// unused frame space).
///
/// The received frame is not initialised: only data[0..DLC)
/// is written and all 16 bytes are queued, so data[DLC..8) are whatever was left on the
/// interrupt stack. `stale` supplies those bytes.
pub fn isr<R: Regs>(r: &mut R, have_queue: bool, guard: u32, stale: [u8; 8]) -> IsrOutcome {
    isr_with_storage(r, have_queue, guard, stale, &mut ())
}

pub trait FrameStorage {
    fn fir(&mut self, _value: u32) {}
    fn msg_id(&mut self, _value: u32) {}
    fn data(&mut self, _index: usize, _value: u8) {}
}
impl FrameStorage for () {}

pub fn isr_with_storage<R: Regs, S: FrameStorage>(
    r: &mut R,
    have_queue: bool,
    guard: u32,
    stale: [u8; 8],
    storage: &mut S,
) -> IsrOutcome {
    let mut out = IsrOutcome::default();
    let mut canary = guard.to_le_bytes();
    let ir = r.read(REG_IR);
    if ir & 0x01 != 0 {
        if !have_queue {
            out.release_rx = true;
        } else {
            let fir = r.read(REG_FIR);
            storage.fir(fir);
            let mut f = CanFrame { fir, msg_id: 0, data: stale };
            let dlc = (fir & FIR_DLC_MASK) as usize;
            let data_base = if fir & FIR_FF == 0 {
                let a = r.read(REG_FIR + 0x04);
                let b = r.read(REG_FIR + 0x08);
                f.msg_id = (a << 3) | (b >> 5);
                REG_FIR + 0x0c
            } else {
                let a = r.read(REG_FIR + 0x04);
                let b = r.read(REG_FIR + 0x08);
                let c = r.read(REG_FIR + 0x0c);
                let d = r.read(REG_FIR + 0x10);
                f.msg_id = (a << 21) | (b << 13) | (c << 5) | (d >> 3);
                REG_FIR + 0x14
            };
            storage.msg_id(f.msg_id);
            for i in 0..dlc {
                let v = r.read(data_base + 4 * i) as u8;
                storage.data(i, v);
                if i < 8 {
                    f.data[i] = v;
                } else if i < 12 {
                    canary[i - 8] = v;
                }
            }
            out.rx = Some(f);
            out.release_rx = true;
        }
    }
    if ir & 0xfe != 0 {
        out.give_tx = true;
    }
    out.stack_smash = u32::from_le_bytes(canary) != guard;
    out
}

/// The text printed on a stack-check failure, before the abort.
pub const STACK_CHK_FAIL_MSG: &[u8] = b"\r\nStack smashing protect failure!\r\n\r\n";
