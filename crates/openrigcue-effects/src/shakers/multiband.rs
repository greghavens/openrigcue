//! The multiband EQ filter modes of the shaker effects.
//!
//! A filter is one, two or four identical biquad sections. The gain stays at 0 dB.
//! Coefficients ramp over a processing block. Sine and cosine come from a
//! bounded-angle polynomial kernel; hardware callback scheduling is separate.

/// Sine and cosine of a bounded angle: the angle and
/// abs(angle)+pi/2 go through one float32 sine polynomial. The coefficient
/// generator's range needs no large-angle
/// reduction or tiny-angle shortcut: normalized frequency is 0.001..0.999.
fn native_sin_cos(angle: f32) -> (f32, f32) {
    fn sine(angle: f32) -> f32 {
        let magnitude = angle.abs();
        let rounded = magnitude * f32::from_bits(0x3ea2_f983) + f32::from_bits(0x4b40_0000);
        let multiple = rounded - f32::from_bits(0x4b40_0000);
        let mut reduced = magnitude;
        // Pi is subtracted in split pieces, in this order; combining these pieces
        // loses the float32 reduction rounding near ordinary filter cutoffs.
        for bits in [0x4049_0000, 0x3a7d_a000, 0x3422_2000, 0x2cb4_611a] {
            reduced -= f32::from_bits(bits) * multiple;
        }
        let square = reduced * reduced;
        reduced = f32::from_bits(reduced.to_bits() ^ (rounded.to_bits() << 31));
        let mut polynomial = f32::from_bits(0x362e_def8) * square;
        for bits in [0xb94f_b7ff, 0x3c08_8766, 0xbe2a_aaa6] {
            polynomial = (polynomial + f32::from_bits(bits)) * square;
        }
        let result = reduced + polynomial * reduced;
        f32::from_bits(result.to_bits() ^ (angle.to_bits() & 0x8000_0000))
    }
    (sine(angle), sine(angle.abs() + f32::from_bits(0x3fc9_0fdb)))
}

#[derive(Clone, Copy, Debug)]
struct Coefficients {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl Default for Coefficients {
    fn default() -> Self {
        Self { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 }
    }
}

impl Coefficients {
    fn zero() -> Self {
        Self { b0: 0.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 }
    }

    fn plus(self, offset: Self) -> Self {
        Self {
            b0: self.b0 + offset.b0,
            b1: self.b1 + offset.b1,
            b2: self.b2 + offset.b2,
            a1: self.a1 + offset.a1,
            a2: self.a2 + offset.a2,
        }
    }

    fn difference(self, old: Self, frames: f32) -> Self {
        Self {
            b0: (self.b0 - old.b0) / frames,
            b1: (self.b1 - old.b1) / frames,
            b2: (self.b2 - old.b2) / frames,
            a1: (self.a1 - old.a1) / frames,
            a2: (self.a2 - old.a2) / frames,
        }
    }

    fn increment(&mut self, step: Self) {
        self.b0 += step.b0;
        self.b1 += step.b1;
        self.b2 += step.b2;
        self.a1 += step.a1;
        self.a2 += step.a2;
    }

    fn for_filter(kind: i32, frequency: f32, q: f32, rate: f32) -> Self {
        let q = match kind {
            2 | 5 => q.sqrt(),
            3 | 6 => q.sqrt().sqrt(),
            _ => q,
        };
        // Normalized frequency is clamped, after the
        // 20..22000 Hz parameter clamp, at 0.001..0.999 of Nyquist.
        let angle = ((2.0 / rate) * frequency).clamp(0.001, 0.999) * std::f32::consts::PI;
        let (sin, cos) = native_sin_cos(angle);
        let alpha = (0.5 / q) * sin;
        let (b0, b1, b2, a0, a1, a2) = match kind {
            1..=3 => {
                let middle = 1.0 - cos;
                (middle * 0.5, middle, middle * 0.5, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            4..=6 => {
                let middle = 1.0 + cos;
                (middle * 0.5, -middle, middle * 0.5, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            // The gain stays at 0 dB, so A=10^(gain/40)=1.
            // The shelves/peak still keep nonzero feedback and delay
            // state. Algebraic cancellation to identity loses those state
            // transitions and the rounded reciprocal-product gain.
            7 | 8 => {
                let term = 2.0_f32.sqrt() * sin;
                let top = 2.0 + term;
                let bottom = 2.0 - term;
                let middle = -4.0 * cos;
                (top, middle, bottom, top, middle, bottom)
            }
            9 => (1.0 + alpha, -2.0 * cos, 1.0 - alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            10 => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            11 => (1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            12 => (1.0 - alpha, -2.0 * cos, 1.0 + alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            _ => return Self::default(),
        };
        let inverse = 1.0 / a0;
        Self { b0: inverse * b0, b1: inverse * b1, b2: inverse * b2, a1: inverse * a1, a2: inverse * a2 }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Multiband {
    coefficients: Coefficients,
    target: Coefficients,
    block_target: Coefficients,
    step: Coefficients,
    offset: Coefficients,
    pending: bool,
    initialized: bool,
    remaining: usize,
    requested_sections: usize,
    delay: [[f32; 2]; 4],
    sections: usize,
}

impl Multiband {
    pub(super) fn tune(&mut self, kind: i32, frequency: f64, q: f64, rate: f64) {
        self.target = Coefficients::for_filter(kind, frequency as f32, q as f32, rate as f32);
        self.requested_sections = match kind {
            2 | 5 => 2,
            3 | 6 => 4,
            1 | 4 | 7..=12 => 1,
            _ => 0,
        };
        self.pending = true;
    }

    pub(super) fn begin_block(&mut self, frames: usize) {
        self.sections = self.requested_sections;
        if self.sections == 0 {
            return;
        }
        if self.pending {
            self.block_target = self.target;
            if self.initialized {
                self.step = self.target.difference(self.coefficients, frames as f32);
                self.offset = Coefficients::zero();
                self.remaining = frames;
            } else {
                self.coefficients = self.block_target;
                self.remaining = 0;
                self.initialized = true;
            }
            self.pending = false;
        }
    }

    pub(super) fn process(&mut self, input: f64) -> f64 {
        let c = self.current_coefficients();
        let mut x = input as f32;
        for delay in &mut self.delay[..self.sections] {
            let output = c.b0 * x + delay[0];
            delay[0] = (c.b1 * x - c.a1 * output) + delay[1];
            delay[1] = c.b2 * x - c.a2 * output;
            x = output;
        }
        if self.remaining != 0 {
            self.offset.increment(self.step);
            self.remaining -= 1;
            if self.remaining == 0 {
                // The processing callback copies the exact target after the block.
                self.coefficients = self.block_target;
            }
        }
        f64::from(x)
    }

    fn current_coefficients(&self) -> Coefficients {
        if self.remaining == 0 {
            self.coefficients
        } else {
            self.coefficients.plus(self.offset)
        }
    }

    #[cfg(test)]
    pub(super) fn is_quiet(&self) -> bool {
        self.delay[..self.sections].iter().flatten().all(|v| v.abs() < 1e-9)
    }

    pub(super) fn is_initialized(&self) -> bool {
        self.initialized
    }

    #[cfg(test)]
    pub(super) fn coefficients_for_test(&self) -> [f32; 5] {
        let c = self.current_coefficients();
        [c.b0, c.b1, c.b2, c.a1, c.a2]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_multiband_sincos_preserves_source_float_operations() {
        // Independent SSE lane calculations in the same operation order, at ordinary audio
        // frequencies. Host libm differs by one or two bits in most rows.
        for (frequency, sin, cos) in [
            (20.0, 0x3b4d_e319, 0x3f7f_ffac),
            (60.0, 0x3c00_ada6, 0x3f7f_fdfa),
            (100.0, 0x3c56_75be, 0x3f7f_fa61),
            (500.0, 0x3d85_f214, 0x3f7f_73ae),
            (1_000.0, 0x3e05_a8a9, 0x3f7d_cf54),
            (8_000.0, 0x3f5d_b3d8, 0x3f00_0001),
            (16_000.0, 0x3f5d_b3d6, 0xbf00_0003),
            (22_000.0, 0x3e84_83e9, 0xbf77_46ea),
        ] {
            let angle = ((2.0_f32 / 48_000.0) * frequency).clamp(0.001, 0.999) * std::f32::consts::PI;
            let actual = native_sin_cos(angle);
            assert_eq!((actual.0.to_bits(), actual.1.to_bits()), (sin, cos), "frequency={frequency}");
        }
    }

    #[test]
    fn native_multiband_coefficients_use_the_native_trig_bits() {
        // SSE implementation of the low-pass path and the
        // common normalization, Q 0.707 at 48 kHz.
        for (frequency, expected) in [
            (20.0, [0x3627_a0a8, 0x36a7_a0a8, 0x3627_a0a8, 0xbfff_6e63, 0x3f7e_dd6f]),
            (100.0, [0x3832_39a3, 0x38b2_39a3, 0x3832_39a3, 0xbffd_a151, 0x3f7b_4dc7]),
            (8_000.0, [0x3e1e_c35c, 0x3e9e_c35c, 0x3e1e_c35c, 0xbf1e_c35e, 0x3e76_1ae9]),
            (16_000.0, [0x3eee_250e, 0x3f6e_250e, 0x3eee_250e, 0x3f1e_c361, 0x3e76_1aee]),
        ] {
            let c = Coefficients::for_filter(1, frequency, 0.707, 48_000.0);
            assert_eq!([c.b0, c.b1, c.b2, c.a1, c.a2].map(f32::to_bits), expected, "frequency={frequency}");
        }
    }

    #[test]
    fn native_multiband_coefficient_switch_and_cascade_orders() {
        // At one quarter of the sample rate the ideal cos=0, sin=1. With Q=1/2,
        // the sine polynomial stays within float rounding of these forms.
        for (kind, expected) in [
            (1, [0.25, 0.5, 0.25, 0.0, 0.0]),
            (4, [0.25, -0.5, 0.25, 0.0, 0.0]),
            (10, [0.5, 0.0, -0.5, 0.0, 0.0]),
            (11, [0.5, 0.0, 0.5, 0.0, 0.0]),
            (12, [0.0, 0.0, 1.0, 0.0, 0.0]),
        ] {
            let c = Coefficients::for_filter(kind, 12_000.0, 0.5, 48_000.0);
            for (actual, expected) in [c.b0, c.b1, c.b2, c.a1, c.a2].into_iter().zip(expected) {
                assert!((actual - expected).abs() < 1e-6, "kind {kind}: {actual} != {expected}");
            }
        }
        for (kind, q, sections) in [(2, 0.25, 2), (3, 0.0625, 4), (5, 0.25, 2), (6, 0.0625, 4)] {
            let mut filter = Multiband::default();
            filter.tune(kind, 12_000.0, q, 48_000.0);
            filter.begin_block(1024);
            assert_eq!(filter.sections, sections);
            assert!((filter.coefficients.b0 - 0.25).abs() < 1e-6);
            assert!((filter.process(1.0) - 0.25_f64.powi(sections as i32)).abs() < 1e-6);
        }
    }

    #[test]
    fn native_multiband_normalized_frequency_and_zero_gain_modes() {
        for (frequency, boundary) in [(20.0, 24.0), (22_000.0, 23_976.0)] {
            // At 48 kHz only the first input reaches the normalized lower
            // clamp; use a low rate to check the upper clamp separately.
            let (frequency, boundary, rate) =
                if frequency == 22_000.0 { (frequency, 3_996.0, 8_000.0) } else { (frequency, boundary, 48_000.0) };
            let a = Coefficients::for_filter(4, frequency, 1.0, rate);
            let b = Coefficients::for_filter(4, boundary, 1.0, rate);
            assert_eq!(a.b0, b.b0);
            assert_eq!(a.a2, b.a2);
        }
        for kind in 7..=9 {
            let mut filter = Multiband::default();
            filter.tune(kind, 100.0, 3.0, 48_000.0);
            filter.begin_block(1024);
            assert_eq!(filter.sections, 1);
            assert!((filter.process(0.75) - 0.75).abs() < 1e-6);
        }
    }

    #[test]
    fn native_zero_gain_modes_keep_feedback_coefficients_and_rounding() {
        for (kind, expected) in [
            (7, [0x3f7f_ffff, 0xbf1e_c5b3, 0x3e76_2d90, 0xbf1e_c5b3, 0x3e76_2d90]),
            (8, [0x3f7f_ffff, 0xbf1e_c5b3, 0x3e76_2d90, 0xbf1e_c5b3, 0x3e76_2d90]),
            (9, [0x3f7f_ffff, 0xbf1e_c35e, 0x3e76_1ae9, 0xbf1e_c35e, 0x3e76_1ae9]),
        ] {
            // SSE implementation of the zero-dB paths and the
            // common reciprocal/multiply normalization.
            let c = Coefficients::for_filter(kind, 8_000.0, 0.707, 48_000.0);
            assert_eq!([c.b0, c.b1, c.b2, c.a1, c.a2].map(f32::to_bits), expected, "kind={kind}");
            let mut filter = Multiband::default();
            filter.tune(kind, 8_000.0, 0.707, 48_000.0);
            filter.begin_block(4);
            assert_eq!((filter.process(1.0) as f32).to_bits(), expected[0]);
            assert_ne!(filter.delay[0], [0.0; 2], "rounded feedforward gain leaves native feedback state");
        }
    }

    #[test]
    fn native_zero_gain_switch_preserves_and_processes_held_filter_state() {
        for kind in 7..=9 {
            let mut filter = Multiband::default();
            filter.tune(1, 800.0, 0.707, 48_000.0);
            filter.begin_block(4);
            for value in [1.0, 0.0, 0.0, 0.0] {
                filter.process(value);
            }
            let held = filter.delay[0];
            filter.tune(kind, 8_000.0, 0.707, 48_000.0);
            filter.begin_block(4);
            for _ in 0..4 {
                filter.process(0.0);
            }
            assert_ne!(filter.delay[0], [0.0; 2]);
            assert_ne!(filter.delay[0], held);
            let state = filter.delay[0];
            let coefficients = filter.coefficients;
            let output = state[0];
            let next0 = (-coefficients.a1 * output) + state[1];
            let next1 = -coefficients.a2 * output;
            assert_eq!(filter.process(0.0), f64::from(output));
            assert_eq!(filter.delay[0], [next0, next1], "kind={kind}");
            assert_ne!(next1, 0.0, "native zero-gain feedback must survive the type switch");
        }
    }

    #[test]
    fn native_multiband_delay_survives_retuning_and_processing_boundaries() {
        for kind in [2, 3, 4, 5, 6, 10, 11, 12] {
            let mut filter = Multiband::default();
            filter.tune(kind, 600.0, 0.707, 44_100.0);
            filter.begin_block(1024);
            for n in 0..64 {
                filter.process(if n == 0 { 1.0 } else { 0.0 });
            }
            assert!(!filter.is_quiet(), "kind {kind} prematurely discarded its delay");
            let mut same = filter;
            filter.tune(kind, 600.0, 0.707, 44_100.0);
            filter.begin_block(1024);
            for _ in 0..64 {
                assert_eq!(filter.process(0.0), same.process(0.0));
            }
            let before = filter.delay;
            filter.tune(kind, 800.0, 1.5, 48_000.0);
            assert_eq!(filter.delay, before, "kind {kind} reset on parameter/rate change");
            for _ in 0..32_768 {
                assert!(filter.process(0.0).is_finite());
            }
            assert!(filter.is_quiet());
        }
    }

    #[test]
    fn native_multiband_ramps_after_the_first_processed_block() {
        let mut filter = Multiband::default();
        filter.tune(1, 12_000.0, 0.5, 48_000.0);
        filter.begin_block(4);
        assert_eq!(filter.coefficients.b1.to_bits(), 0x3f00_0001);
        let initial = filter.coefficients.b1;
        for _ in 0..4 {
            filter.process(0.0);
        }
        filter.tune(4, 12_000.0, 0.5, 48_000.0);
        // A control write alone does not change the current audio block.
        assert_eq!(filter.coefficients.b1, initial);
        filter.begin_block(4);
        // The sine polynomial makes quarter-rate cosine slightly negative,
        // so these are close to, but not exactly, ideal half/quarter steps.
        for expected in [0.5, 0.25, 0.0, -0.25] {
            assert!((filter.current_coefficients().b1 - expected).abs() < 1e-6);
            filter.process(0.0);
        }
        assert_eq!(filter.coefficients.b1, filter.target.b1);
        assert_eq!(filter.remaining, 0);
        // Repeated parameter writes still mark the filter dirty; equal targets yield zero increments.
        filter.tune(4, 12_000.0, 0.5, 48_000.0);
        filter.begin_block(4);
        assert_eq!(filter.step.b1, 0.0);
        assert_eq!(filter.remaining, 4);
    }

    #[test]
    fn native_multiband_mid_block_request_does_not_replace_the_committing_target() {
        let mut filter = Multiband::default();
        filter.tune(1, 12_000.0, 0.5, 48_000.0);
        filter.begin_block(4);
        filter.tune(4, 12_000.0, 0.5, 48_000.0);
        filter.begin_block(4);
        filter.process(0.0);
        filter.tune(11, 12_000.0, 0.5, 48_000.0);
        for _ in 0..3 {
            filter.process(0.0);
        }
        let previous = filter.coefficients.b1;
        assert!((previous + 0.5).abs() < 1e-6);
        filter.begin_block(4);
        assert_eq!(filter.coefficients.b1, previous);
        for _ in 0..4 {
            filter.process(0.0);
        }
        assert!(filter.coefficients.b1.abs() < 1e-6);
    }

    #[test]
    fn native_multiband_keeps_all_four_section_delays_across_type_changes() {
        let mut filter = Multiband::default();
        filter.tune(3, 300.0, 0.707, 48_000.0);
        filter.begin_block(16);
        for sample in 0..16 {
            filter.process(if sample == 0 { 1.0 } else { 0.0 });
        }
        let delay = filter.delay;
        assert!(delay.iter().all(|pair| pair.iter().any(|value| *value != 0.0)));
        filter.tune(4, 400.0, 0.707, 48_000.0);
        filter.begin_block(16);
        assert_eq!(filter.delay, delay);
        for _ in 0..16 {
            filter.process(0.0);
        }
        assert_eq!(&filter.delay[1..], &delay[1..]);
        filter.tune(3, 300.0, 0.707, 48_000.0);
        filter.begin_block(16);
        assert_eq!(&filter.delay[1..], &delay[1..]);
    }
}
