//! Wheel and event effects: random vibration added to the slots under the wheels.

use std::sync::{Mutex, OnceLock, PoisonError};

use crate::cueing::NativePositions;
#[cfg(test)]
use crate::platform::SLOTS;
use num_bigint::{BigInt, BigUint};
use num_traits::{One, ToPrimitive};
use openrigcue_model::native_value::LegacyValue;
use rand::RngCore;

use crate::error::{mh, CueError};
use crate::mixing::map_arg;
use crate::platform::{Platform, Wheel, WheelLayout, WheelSlots};
use crate::pymath::rescale;
use crate::pyvalue::{Op, PyValue};
use crate::views::{game_item, game_item2, Sources};

/// Source of the effects' random integers.
pub trait EffectRng: Send {
    /// A uniform integer in `low..=high`. Only called with `low <= high`.
    fn randint(&mut self, low: i128, high: i128) -> i128;
    /// A uniform integer in an interval of any size. Deterministic small fixture generators only
    /// need to provide [`EffectRng::randint`].
    fn randint_native(&mut self, low: &BigInt, high: &BigInt) -> BigInt {
        if let (Some(low), Some(high)) = (low.to_i128(), high.to_i128()) {
            return self.randint(low, high).into();
        }
        let n = (high - low + BigInt::one()).to_biguint().expect("nonempty interval");
        let bits = n.bits();
        loop {
            let mut bytes = Vec::with_capacity(bits.div_ceil(32) as usize * 4);
            let mut shift = 0;
            while shift < bits {
                let count = (bits - shift).min(32) as u32;
                let word = (self.randint(0, u32::MAX as i128) as u32) >> (32 - count);
                bytes.extend(word.to_le_bytes());
                shift += 32;
            }
            let value = BigUint::from_bytes_le(&bytes);
            if value < n {
                return low + BigInt::from(value);
            }
        }
    }
}

/// The process-wide random generator, seeded from the operating system on first use. Every draw of
/// the program goes through it, so all users advance the same generator.
pub fn python_random() -> &'static Mutex<DefaultRng> {
    static RANDOM: OnceLock<Mutex<DefaultRng>> = OnceLock::new();
    RANDOM.get_or_init(|| Mutex::new(DefaultRng::from_entropy()))
}

/// [`EffectRng`] over [`python_random`]: what the motion effects draw from.
#[derive(Debug, Clone, Copy, Default)]
pub struct SharedRandom;

impl EffectRng for SharedRandom {
    fn randint(&mut self, low: i128, high: i128) -> i128 {
        self.randint_native(&BigInt::from(low), &BigInt::from(high)).to_i128().expect("bounded interval")
    }
    fn randint_native(&mut self, low: &BigInt, high: &BigInt) -> BigInt {
        let n = (high - low + BigInt::one()).to_biguint().expect("nonempty interval");
        let bits = n.bits();
        loop {
            // The generator lock is held for one draw only, not across the rejection loop.
            // Other effects may advance the generator here.
            let draw = { python_random().lock().unwrap_or_else(PoisonError::into_inner).getrandbits_native(bits) };
            if draw < n {
                return low + BigInt::from(draw);
            }
        }
    }
}

/// The default generator: MT19937 with bit draws and rejection-sampled integer intervals.
#[derive(Clone)]
pub struct DefaultRng {
    state: [u32; MT_N],
    index: usize,
}

const MT_N: usize = 624;
const MT_M: usize = 397;

impl DefaultRng {
    /// Seeded by array with 624 words from the operating system's random source.
    pub fn from_entropy() -> Self {
        let mut bytes = [0u8; MT_N * 4];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let key: Vec<u32> =
            bytes.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        Self::from_key(&key)
    }

    /// Seeded by array with the 32-bit words of a non-negative integer, least significant first
    /// (at least one word).
    pub fn seeded(seed: u64) -> Self {
        let key = if seed >> 32 == 0 { vec![seed as u32] } else { vec![seed as u32, (seed >> 32) as u32] };
        Self::from_key(&key)
    }

    fn init_genrand(seed: u32) -> Self {
        let mut state = [0u32; MT_N];
        state[0] = seed;
        for i in 1..MT_N {
            state[i] = 1_812_433_253u32.wrapping_mul(state[i - 1] ^ (state[i - 1] >> 30)).wrapping_add(i as u32);
        }
        DefaultRng { state, index: MT_N }
    }

    /// The MT19937 seeding from an array of words.
    fn from_key(key: &[u32]) -> Self {
        let mut rng = Self::init_genrand(19_650_218);
        let mt = &mut rng.state;
        let (mut i, mut j) = (1usize, 0usize);
        for _ in 0..MT_N.max(key.len()) {
            mt[i] = (mt[i] ^ (mt[i - 1] ^ (mt[i - 1] >> 30)).wrapping_mul(1_664_525))
                .wrapping_add(key[j])
                .wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= MT_N {
                mt[0] = mt[MT_N - 1];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
        }
        for _ in 0..MT_N - 1 {
            mt[i] = (mt[i] ^ (mt[i - 1] ^ (mt[i - 1] >> 30)).wrapping_mul(1_566_083_941)).wrapping_sub(i as u32);
            i += 1;
            if i >= MT_N {
                mt[0] = mt[MT_N - 1];
                i = 1;
            }
        }
        mt[0] = 0x8000_0000;
        rng
    }

    /// The next 32-bit output word.
    fn next_u32(&mut self) -> u32 {
        const UPPER: u32 = 0x8000_0000;
        const LOWER: u32 = 0x7fff_ffff;
        const MATRIX_A: u32 = 0x9908_b0df;
        if self.index >= MT_N {
            let mt = &mut self.state;
            for k in 0..MT_N {
                let y = (mt[k] & UPPER) | (mt[(k + 1) % MT_N] & LOWER);
                let mag = if y & 1 == 1 { MATRIX_A } else { 0 };
                mt[k] = mt[(k + MT_M) % MT_N] ^ (y >> 1) ^ mag;
            }
            self.index = 0;
        }
        let mut y = self.state[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^ (y >> 18)
    }

    fn getrandbits_native(&mut self, k: u64) -> BigUint {
        let mut words = Vec::with_capacity(k.div_ceil(32) as usize);
        let mut shift = 0;
        while shift < k {
            let bits = (k - shift).min(32) as u32;
            words.push(self.next_u32() >> (32 - bits));
            shift += 32;
        }
        BigUint::new(words)
    }

    /// `k` random bits for `k <= 128`: 32-bit words from the least significant up, the last one
    /// shifted down to the remaining bits.
    fn getrandbits(&mut self, k: u32) -> u128 {
        let mut value = 0;
        let mut shift = 0;
        while shift < k {
            let bits = (k - shift).min(32);
            value |= u128::from(self.next_u32() >> (32 - bits)) << shift;
            shift += bits;
        }
        value
    }
}

impl EffectRng for DefaultRng {
    fn randint_native(&mut self, low: &BigInt, high: &BigInt) -> BigInt {
        if let (Some(low), Some(high)) = (low.to_i128(), high.to_i128()) {
            return self.randint(low, high).into();
        }
        let n = (high - low + BigInt::one()).to_biguint().expect("nonempty interval");
        let bits = n.bits();
        loop {
            let value = self.getrandbits_native(bits);
            if value < n {
                return low + BigInt::from(value);
            }
        }
    }
    /// A uniform integer in `a..=b`: `a` plus a value below `n = b - a + 1`, found by drawing
    /// as many bits as `n` has until the value is below `n`.
    fn randint(&mut self, low: i128, high: i128) -> i128 {
        let n = (high as u128).wrapping_sub(low as u128).wrapping_add(1);
        if n == 0 {
            // All 2^128 represented integers: 129 bits are drawn,
            // rejecting when the high bit of the fifth MT word is set.
            loop {
                let r = self.getrandbits(128);
                if self.getrandbits(1) == 0 {
                    return low.wrapping_add(r as i128);
                }
            }
        }
        let k = 128 - n.leading_zeros();
        let mut r = self.getrandbits(k);
        while r >= n {
            r = self.getrandbits(k);
        }
        low.wrapping_add(r as i128)
    }
}

/// The telemetry channels the effects read, NaN already replaced by 0.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EventInputs {
    pub native: Option<openrigcue_model::Telemetry>,
    pub wheel_slip: WheelSlots<f64>,
    pub rumble: WheelSlots<f64>,
    pub canopy: f64,
    pub afterburner: f64,
    pub landing_gear_left: f64,
    pub landing_gear_right: f64,
    pub cannon_firing: f64,
    pub missile_firing: f64,
    pub abs_lock: f64,
    pub rpm: PyValue,
    pub max_rpm: PyValue,
}

impl EventInputs {
    fn value(&self, key: &str, fallback: f64) -> PyValue {
        self.native.as_ref().and_then(|t| t.native_value(key)).map_or(PyValue::Float(fallback), PyValue::from)
    }
}

/// The engine vibration amplitude: an amplitude from 3 to 4.5 below half the rev range,
/// falling from 4 to 0 above it; 0 at or below 400 rpm or without a maximum.
pub(crate) fn engine_vibration(rpm: &PyValue, max_rpm: &PyValue) -> Result<f64, CueError> {
    // The division is done on the exact numbers before the map converts arguments
    // to doubles. Large integer ratios can land on the other side of 0.5.
    let calculate = || -> Result<f64, CueError> {
        let ratio = rpm.binop(Op::Div, max_rpm)?;
        let low = ratio.compare(crate::pyvalue::Cmp::Lt, &PyValue::Float(0.5))?;
        let (rpm, max_rpm) = (map_arg(rpm)?, map_arg(max_rpm)?);
        Ok(if low { 3.0 + rescale(rpm, 0.0, max_rpm, 0.0, 3.0) } else { 8.0 - rescale(rpm, 0.0, max_rpm, 0.0, 8.0) })
    };
    if rpm.compare(crate::pyvalue::Cmp::Gt, &PyValue::Int(400))? {
        Ok(calculate().unwrap_or(0.0))
    } else {
        Ok(0.0)
    }
}

/// The amplitude of one effect in position units:
/// `slider / 100 * max_motor_abs * motion_max_power * wheel_shake_factor / 100`, truncated.
fn effect_amplitude(
    slider: f64,
    platform: &Platform,
    sources: &Sources<'_>,
    shake_factor: f64,
) -> Result<BigInt, CueError> {
    let at = |e: CueError| e.at(mh("calc_effect_wheel", 1056));
    let max_power = game_item(sources.game, "motion_max_power").map_err(at)?;
    let value = PyValue::Float(slider / 100.0 * platform.max_motor_abs())
        .binop(Op::Mul, &max_power)
        .and_then(|v| v.binop(Op::Mul, &PyValue::Float(shake_factor)))
        .and_then(|v| v.binop(Op::Div, &PyValue::Int(100)))
        .and_then(|v| v.to_integer_value())
        .map_err(at)?
        .to_native()
        .integer()
        .expect("int conversion");
    Ok(value)
}

/// Positions are kept in `[margin, max_motor_abs − margin]` after each effect step.
#[derive(Debug, Clone, Copy)]
struct Bounds {
    low: i64,
    high: i64,
}

/// One application pass of the effects over the four wheels.
struct WheelPass<'a> {
    positions: &'a mut NativePositions,
    layout: &'a WheelLayout,
    bounds: Bounds,
    rng: &'a mut dyn EffectRng,
}

impl WheelPass<'_> {
    fn nudge(&mut self, wheel: Wheel, low: impl Into<BigInt>, high: impl Into<BigInt>) {
        let (low, high) = (low.into(), high.into());
        let slot = self.layout.slots.get(wheel);
        let delta = self.rng.randint_native(&low, &high) * BigInt::from(self.layout.direction(wheel));
        let position = self.positions[slot].integer().expect("motion position int") + delta;
        self.positions[slot] =
            LegacyValue::from_integer(position.max(self.bounds.low.into()).min(self.bounds.high.into()));
    }
    fn proportional(&mut self, wheel: Wheel, value: &PyValue, amplitude: BigInt, line: u32) -> Result<(), CueError> {
        let high = value
            .to_native()
            .py_mul(&LegacyValue::from_integer(amplitude))
            .and_then(|v| v.py_integer())
            .map_err(crate::pyvalue::native_error)
            .map_err(|e| e.at(mh("send_stream", line)))?;
        self.nudge(wheel, BigInt::one(), high.max(BigInt::one()));
        Ok(())
    }
    fn burst(&mut self, wheel: Wheel, amplitude: impl Into<BigInt>) -> Result<(), CueError> {
        let high = amplitude.into() * 7;
        if high < BigInt::from(3) {
            return Err(CueError::value_error(format!("empty range in randrange(3, {})", high + 1))
                .at(mh("calc_wheel_effect", 1066)));
        }
        self.nudge(wheel, BigInt::from(3), high);
        Ok(())
    }
}

/// The effects whose sliders the stream path computes, in evaluation order.
const SLIDER_EFFECTS: [&str; 9] = [
    "engine",
    "wheel_slip",
    "rumble_strips",
    "afterburner",
    "canopy",
    "missile_firing",
    "cannon_firing",
    "landing_gear",
    "abs_lock",
];

/// The slider of one effect: its `motion_effects` value times its integer `motion_switch`,
/// mapped from 0..20 onto 0..100.
fn effect_slider(sources: &Sources<'_>, effect: &str) -> Result<f64, CueError> {
    let at = |e: CueError| e.at(mh("send_stream", 914));
    let gain = game_item2(sources.game, "motion_effects", effect).map_err(at)?;
    let switch = game_item2(sources.game, "motion_switch", effect).map_err(at)?.to_integer_value().map_err(at)?;
    let product = gain.binop(Op::Mul, &switch).map_err(at)?;
    Ok(rescale(map_arg(&product).map_err(at)?, 0.0, 20.0, 0.0, 100.0))
}

/// An event effect of the burst form, with its per-wheel input, trigger and the trace lines of its
/// amplitude and application steps.
struct Burst {
    effect: &'static str,
    input: WheelSlots<f64>,
    lines: (u32, u32),
}

/// Applies the wheel and event effects of one stream frame.
///
/// The nine sliders are computed first; effects then run in a fixed order (wheel slip, engine,
/// rumble strips, afterburner, canopy, cannon, ABS lock, missile, landing gear), each over the wheels in
/// the order rear-left, front-left, front-right, rear-right, which fixes the sequence of the random
/// draws.
pub(crate) fn apply_native_wheel_effects(
    positions: &mut NativePositions,
    platform: &Platform,
    sources: &Sources<'_>,
    inputs: &EventInputs,
    rng: &mut dyn EffectRng,
) -> Result<(), CueError> {
    let engine_value = engine_vibration(&inputs.rpm, &inputs.max_rpm).map_err(|e| e.at(mh("send_stream", 869)))?;
    let selected_wheels = sources.model_field("wheels");
    let mut native_layout = match selected_wheels {
        Ok(Some(wheels)) => sources.model_field("wheel_shake_factor").ok().flatten().and_then(|factor| {
            let slot = |key: &str| wheels.item_owned(key).ok()?.to_native().integer()?.try_into().ok();
            Some(WheelLayout {
                slots: WheelSlots { lr: slot("lr")?, lf: slot("lf")?, rf: slot("rf")?, rr: slot("rr")? },
                directions: None,
                shake_factor: factor.as_double().ok()?,
            })
        }),
        Ok(None) => platform.wheels.clone(),
        Err(_) => None,
    };
    let directions = match sources.model_field("wheels_motor_dir") {
        Ok(Some(values)) => {
            let direction = |key: &str| values.item_owned(key).ok()?.to_native().integer()?.try_into().ok();
            (move || {
                Some(WheelSlots {
                    lr: direction("lr")?,
                    lf: direction("lf")?,
                    rf: direction("rf")?,
                    rr: direction("rr")?,
                })
            })()
        }
        Ok(None) => platform.wheels.as_ref().and_then(|layout| layout.directions),
        Err(_) => None,
    };
    if let Some(layout) = &mut native_layout {
        layout.directions = directions;
    }
    let Some(layout) = native_layout.as_ref() else {
        return Ok(());
    };
    let mut sliders = [0.0; SLIDER_EFFECTS.len()];
    for (slider, effect) in sliders.iter_mut().zip(SLIDER_EFFECTS) {
        *slider = effect_slider(sources, effect)?;
    }
    let slider = |effect: &str| sliders[SLIDER_EFFECTS.iter().position(|&e| e == effect).unwrap_or(0)];
    let margin = platform.margin() as i64;
    let mut pass = WheelPass {
        positions,
        layout,
        bounds: Bounds { low: margin, high: i64::from(platform.max_motor_abs) - margin },
        rng,
    };
    let amplitude = |s: f64, line: u32| {
        effect_amplitude(s, platform, sources, layout.shake_factor).map_err(|e| e.at(mh("send_stream", line)))
    };

    let slip = slider("wheel_slip");
    if slip != 0.0 {
        let a = amplitude(slip, 917)?;
        for wheel in Wheel::ORDER {
            let key = match wheel {
                Wheel::RearLeft => "wheel_slip_lr",
                Wheel::FrontLeft => "wheel_slip_lf",
                Wheel::FrontRight => "wheel_slip_rf",
                Wheel::RearRight => "wheel_slip_rr",
            };
            let value = inputs.value(key, inputs.wheel_slip.get(wheel));
            if value.compare(crate::pyvalue::Cmp::Gt, &PyValue::Int(2)).map_err(|e| e.at(mh("send_stream", 921)))? {
                let value = if value
                    .compare(crate::pyvalue::Cmp::Gt, &PyValue::Int(10))
                    .map_err(|e| e.at(mh("send_stream", 922)))?
                {
                    PyValue::Int(10)
                } else {
                    value
                };
                pass.proportional(wheel, &value, a.clone(), 930)?;
            }
        }
    }

    let engine = slider("engine");
    if engine != 0.0 {
        let a = amplitude(engine, 944)?;
        let value = engine_value;
        for wheel in Wheel::ORDER {
            if value > 0.1 {
                pass.proportional(wheel, &PyValue::Float(value), a.clone(), 956)?;
            }
        }
    }

    let all = |v: f64| WheelSlots { lr: v, lf: v, rf: v, rr: v };
    let front = |v: f64| WheelSlots { lr: 0.0, lf: v, rf: v, rr: 0.0 };
    let bursts = [
        Burst { effect: "rumble_strips", input: inputs.rumble, lines: (970, 975) },
        Burst {
            effect: "afterburner",
            input: WheelSlots { lr: inputs.afterburner, lf: 0.0, rf: 0.0, rr: inputs.afterburner },
            lines: (979, 984),
        },
        Burst { effect: "canopy", input: front(inputs.canopy), lines: (988, 993) },
        Burst { effect: "cannon_firing", input: all(inputs.cannon_firing), lines: (997, 1003) },
        Burst { effect: "abs_lock", input: front(inputs.abs_lock), lines: (1007, 1013) },
        Burst { effect: "missile_firing", input: all(inputs.missile_firing), lines: (1017, 1023) },
        Burst {
            effect: "landing_gear",
            input: WheelSlots {
                lr: inputs.landing_gear_left,
                lf: inputs.landing_gear_left,
                rf: inputs.landing_gear_right,
                rr: inputs.landing_gear_right,
            },
            lines: (1027, 1033),
        },
    ];
    for burst in bursts {
        let s = slider(burst.effect);
        if s == 0.0 {
            continue;
        }
        let a = amplitude(s, burst.lines.0)?;
        for wheel in Wheel::ORDER {
            let fallback = burst.input.get(wheel);
            let key = match burst.effect {
                "rumble_strips" => Some(match wheel {
                    Wheel::RearLeft => "rumble_pitch_lr",
                    Wheel::FrontLeft => "rumble_pitch_lf",
                    Wheel::FrontRight => "rumble_pitch_rf",
                    Wheel::RearRight => "rumble_pitch_rr",
                }),
                "afterburner" if matches!(wheel, Wheel::RearLeft | Wheel::RearRight) => Some("afterburner"),
                "canopy" | "abs_lock" if matches!(wheel, Wheel::FrontLeft | Wheel::FrontRight) => Some(burst.effect),
                "cannon_firing" | "missile_firing" => Some(burst.effect),
                "landing_gear" => Some(if matches!(wheel, Wheel::RearLeft | Wheel::FrontLeft) {
                    "landing_gear_left"
                } else {
                    "landing_gear_right"
                }),
                _ => None,
            };
            let value = key.map_or(PyValue::Int(0), |key| inputs.value(key, fallback));
            let fires = match burst.effect {
                "rumble_strips" => value.compare(crate::pyvalue::Cmp::Gt, &PyValue::Int(5)),
                "canopy" | "landing_gear" => {
                    let value = value.to_native();
                    value
                        .py_ge(&LegacyValue::Float(0.15))
                        .and_then(|lower| if lower { value.py_le(&LegacyValue::Float(0.85)) } else { Ok(false) })
                        .map_err(crate::pyvalue::native_error)
                }
                _ => value.equals_checked(&PyValue::Int(1)),
            }
            .map_err(|e| {
                e.at(mh(
                    "send_stream",
                    match burst.effect {
                        "rumble_strips" => 974,
                        "afterburner" => 983,
                        "canopy" => 992,
                        "cannon_firing" => 1001,
                        "abs_lock" => 1011,
                        "missile_firing" => 1021,
                        _ => 1031,
                    },
                ))
            })?;
            if fires {
                pass.burst(wheel, a.clone()).map_err(|e| e.at(mh("send_stream", burst.lines.1)))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn apply_wheel_effects(
    positions: &mut [i64; SLOTS],
    platform: &Platform,
    sources: &Sources<'_>,
    inputs: &EventInputs,
    rng: &mut dyn EffectRng,
) -> Result<(), CueError> {
    let mut native = positions.map(LegacyValue::Int);
    let result = apply_native_wheel_effects(&mut native, platform, sources, inputs, rng);
    for (i, value) in native.into_iter().enumerate() {
        positions[i] = value.integer().unwrap().to_i64().expect("bounded fixture frame");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::builtin;
    use crate::settings::MotionSettings;
    use crate::tune::MotionTune;
    use crate::views::{ConfigView, GameConfigView};

    fn effect_amplitude(
        slider: f64,
        platform: &Platform,
        sources: &Sources<'_>,
        factor: f64,
    ) -> Result<i128, CueError> {
        super::effect_amplitude(slider, platform, sources, factor)
            .map(|v| v.to_i128().expect("bounded fixture amplitude"))
    }

    fn src<'a>(config: &'a dyn ConfigView, game: &'a dyn GameConfigView) -> Sources<'a> {
        Sources { config, game, manufacturer: "DOF REALITY", game_running: "dirtrally2" }
    }

    #[test]
    fn randint_is_pythons_mersenne_twister() {
        // MT19937 seeded with 12345: five draws from 1..=400, then 3..=280,
        // 1..=1, 1..=2^40 and 3..=2^33 + 5.
        let mut r = DefaultRng::seeded(12345);
        let draws: Vec<i128> = (0..5).map(|_| r.randint(1, 400)).collect();
        assert_eq!(draws, [214, 376, 6, 153, 189]);
        assert_eq!(r.randint(3, 280), 102);
        assert_eq!(r.randint(1, 1), 1);
        assert_eq!(r.randint(1, 1 << 40), 960208693574);
        assert_eq!(r.randint(3, (1 << 33) + 5), 4989411214);
        // A two-word seed: 2^40 + 7.
        let mut r = DefaultRng::seeded((1 << 40) + 7);
        let draws: Vec<i128> = (0..5).map(|_| r.randint(1, 10)).collect();
        assert_eq!(draws, [10, 10, 8, 5, 10]);
        let mut r = DefaultRng::seeded(12345);
        assert_eq!([r.getrandbits(32), r.getrandbits(32)], [1789368711, 3146859322]);
    }

    #[test]
    fn randint_full_signed_range_keeps_the_sixty_fifth_rejection_bit() {
        let mut rng = DefaultRng::seeded(12345);
        // A 65-bit draw consumes three source words. The first two form
        // the candidate; the high bit of the third word determines rejection.
        assert_eq!(rng.randint(i128::from(i64::MIN), i128::from(i64::MAX)), 4_292_285_838_037_326_215);
        let mut reference = DefaultRng::seeded(12345);
        reference.next_u32();
        reference.next_u32();
        assert_eq!(reference.next_u32() >> 31, 0);
        assert_eq!(rng.next_u32(), reference.next_u32());
    }

    #[test]
    fn randint_wide_ranges_match_independent_bigint_reference() {
        let mut previous_seed = None;
        let mut rng = DefaultRng::seeded(0);
        for line in include_str!("../tests/data/random-wide-reference.txt").lines() {
            let values: Vec<i128> = line.split_whitespace().map(|v| v.parse().unwrap()).collect();
            let seed = values[0] as u64;
            if previous_seed != Some(seed) {
                rng = DefaultRng::seeded(seed);
                previous_seed = Some(seed);
            }
            assert_eq!(rng.randint(values[1], values[2]), values[3], "{line}");
        }
    }

    #[test]
    fn wide_effect_amplitudes_keep_integer_range_before_clamping() {
        let mut p = builtin("dof-reality-p3").unwrap().unwrap();
        p.wheels.as_mut().unwrap().shake_factor = 1.0;
        let s = MotionSettings::default();
        let tune = MotionTune { max_power: 1.0e20, ..MotionTune::default() };
        let amplitude = effect_amplitude(20.0, &p, &src(&s, &tune), 1.0).unwrap();
        assert!(amplitude > i128::from(i64::MAX));
        let mut positions = std::array::from_fn(|_| LegacyValue::Int(2000));
        let mut rng = MaxRng::default();
        let mut pass = WheelPass {
            positions: &mut positions,
            layout: p.wheels.as_ref().unwrap(),
            bounds: Bounds { low: 204, high: 3891 },
            rng: &mut rng,
        };
        pass.proportional(Wheel::RearLeft, &PyValue::Float(10.0), amplitude.into(), 930).unwrap();
        pass.burst(Wheel::RearRight, amplitude).unwrap();
        assert_eq!(rng.0, [(1, crate::pyvalue::float_to_int(10.0 * amplitude as f64).unwrap()), (3, 7 * amplitude)]);
        assert_eq!((positions[0].clone(), positions[1].clone()), (LegacyValue::Int(3891), LegacyValue::Int(204)));
    }

    /// Records every randint call and answers with the upper bound.
    #[derive(Default)]
    struct MaxRng(Vec<(i128, i128)>);

    impl EffectRng for MaxRng {
        fn randint(&mut self, low: i128, high: i128) -> i128 {
            self.0.push((low, high));
            high
        }
    }

    fn quiet() -> EventInputs {
        let zero = WheelSlots { lr: 0.0, lf: 0.0, rf: 0.0, rr: 0.0 };
        EventInputs {
            native: None,
            wheel_slip: zero,
            rumble: zero,
            canopy: 0.0,
            afterburner: 0.0,
            landing_gear_left: 0.0,
            landing_gear_right: 0.0,
            cannon_firing: 0.0,
            missile_firing: 0.0,
            abs_lock: 0.0,
            rpm: PyValue::Float(0.0),
            max_rpm: PyValue::Float(0.0),
        }
    }

    #[test]
    fn the_random_instance_is_process_wide() {
        // Two cueings draw from the one process-wide generator.
        let before = python_random().lock().unwrap().clone();
        let (a, b) = (SharedRandom.randint(1, 1 << 30), SharedRandom.randint(1, 1 << 30));
        let mut replay = before;
        assert_eq!((replay.randint(1, 1 << 30), replay.randint(1, 1 << 30)), (a, b));
    }

    #[test]
    fn engine_rpm_block() {
        let vibration = |a, b| engine_vibration(&PyValue::Float(a), &PyValue::Float(b)).unwrap();
        assert_eq!(vibration(400.0, 8000.0), 0.0);
        assert_eq!(vibration(2000.0, 8000.0), 3.0 + 0.75);
        assert_eq!(vibration(6000.0, 8000.0), 8.0 - 6.0);
        assert_eq!(vibration(9000.0, 8000.0), 0.0); // map clamps at 8
        assert_eq!(vibration(5000.0, 0.0), 0.0); // division by zero -> 0
    }

    #[test]
    fn engine_half_range_uses_native_integer_ratio_before_map_conversion() {
        let rpm = PyValue::Int(4_503_599_627_370_496);
        let maximum = PyValue::Int(9_007_199_254_740_993);
        assert_eq!(engine_vibration(&rpm, &maximum), Ok(4.5));
        assert_eq!(4_503_599_627_370_496_f64 / 9_007_199_254_740_993_i128 as f64, 0.5);
        assert_eq!(engine_vibration(&rpm, &PyValue::Float(9_007_199_254_740_992.0)), Ok(4.0));
        assert_eq!(engine_vibration(&rpm, &PyValue::Str("9007199254740993".into())), Ok(0.0));
        assert_eq!(
            engine_vibration(&PyValue::Str("500".into()), &maximum).unwrap_err().to_string(),
            "'>' not supported between instances of 'str' and 'int'"
        );
    }

    #[test]
    fn engine_rpm_gate_precedes_wheel_presence_and_slider_reads() {
        let mut p = builtin("dof-reality-p3").unwrap().unwrap();
        p.wheels = None;
        let mut inputs = quiet();
        inputs.rpm = PyValue::Str("500".into());
        let config = MotionSettings::default();
        let missing = crate::RawGameConfig(PyValue::None);
        let error =
            apply_wheel_effects(&mut [2000; SLOTS], &p, &src(&config, &missing), &inputs, &mut MaxRng::default())
                .unwrap_err();
        assert_eq!(error.to_string(), "'>' not supported between instances of 'str' and 'int'");
        assert_eq!(error.frames().next().unwrap().line, 869);
    }

    #[test]
    fn calc_effect_wheel_amplitude() {
        let p = builtin("dof-reality-p3").unwrap().unwrap();
        let (s, tune) = (MotionSettings::default(), MotionTune::default());
        // 4 mapped from 0..20 onto 0..100 = 20 -> 20 / 100 · 4095 · 100 · 0.05 / 100 = 40.95, truncated to 40.
        assert_eq!(effect_amplitude(20.0, &p, &src(&s, &tune), 0.05), Ok(40));
    }

    #[test]
    fn wheel_slip_and_rumble_on_a_p3() {
        let p = builtin("dof-reality-p3").unwrap().unwrap();
        let tune = MotionTune::default();
        let mut inputs = quiet();
        inputs.wheel_slip.lr = 12.0; // capped at 10
        inputs.wheel_slip.rf = 2.0; // not above 2
        inputs.rumble.rr = 6.0;
        let mut positions = [2000; SLOTS];
        let mut rng = MaxRng::default();
        apply_wheel_effects(&mut positions, &p, &src(&MotionSettings::default(), &tune), &inputs, &mut rng).unwrap();
        // slip lr -> slot 0, direction +1: randint(1, 10 · 40) = 400.
        // rumble rr -> slot 1, direction -1: randint(3, 7 · 40) = 280.
        assert_eq!(rng.0, vec![(1, 400), (3, 280)]);
        assert_eq!(positions[0], 2400);
        assert_eq!(positions[1], 1720);
    }

    #[test]
    fn effects_clamp_to_the_margin_band() {
        let p = builtin("dof-reality-p3").unwrap().unwrap();
        let tune = MotionTune::default();
        let mut inputs = quiet();
        inputs.cannon_firing = 1.0;
        let mut positions = [3800; SLOTS];
        positions[1] = 300;
        let mut rng = MaxRng::default();
        apply_wheel_effects(&mut positions, &p, &src(&MotionSettings::default(), &tune), &inputs, &mut rng).unwrap();
        // Four wheels: lr, lf -> slot 0 (+280 twice, capped at 3891); rf, rr -> slot 1 (-280 twice, floored at 204).
        assert_eq!(rng.0.len(), 4);
        assert_eq!(positions[0], 3891);
        assert_eq!(positions[1], 204);
    }

    #[test]
    fn effect_clamps_after_wide_position_and_direction_arithmetic() {
        let layout = WheelLayout {
            slots: WheelSlots { lr: 0, lf: 1, rf: 2, rr: 3 },
            directions: Some(WheelSlots { lr: 1, lf: -1, rf: i64::MAX, rr: i64::MIN }),
            shake_factor: 0.05,
        };
        let mut positions = std::array::from_fn(|_| LegacyValue::Int(2000));
        let mut rng = MaxRng::default();
        let mut pass = WheelPass {
            positions: &mut positions,
            layout: &layout,
            bounds: Bounds { low: 204, high: 3891 },
            rng: &mut rng,
        };
        // Every argument fits the existing i64 RNG contract. The
        // addition/multiplication is done on integers before clamping.
        pass.nudge(Wheel::RearLeft, 1, i128::from(i64::MAX));
        pass.nudge(Wheel::FrontLeft, i128::from(i64::MIN), i128::from(i64::MIN));
        pass.nudge(Wheel::FrontRight, 1, i128::from(i64::MAX));
        pass.nudge(Wheel::RearRight, 1, i128::from(i64::MAX));
        pass.nudge(Wheel::FrontRight, 1, i128::MAX);
        pass.nudge(Wheel::RearRight, 1, i128::MAX);
        assert_eq!(
            &positions[..4],
            &[LegacyValue::Int(3891), LegacyValue::Int(3891), LegacyValue::Int(3891), LegacyValue::Int(204)]
        );
    }

    #[test]
    fn calc_wheel_effect_empty_range_fails_the_frame() {
        let p = builtin("dof-reality-p3").unwrap().unwrap();
        // amplitude 0.4095 truncated to 0 -> randint(3, 0)
        let tune = MotionTune { max_power: 1.0, ..MotionTune::default() };
        let mut inputs = quiet();
        inputs.abs_lock = 1.0;
        let mut positions = [2000; SLOTS];
        let s = MotionSettings::default();
        let err =
            apply_wheel_effects(&mut positions, &p, &src(&s, &tune), &inputs, &mut MaxRng::default()).unwrap_err();
        assert_eq!(err.to_string(), "empty range in randrange(3, 1)");
        let frames: Vec<_> = err.frames().map(|f| (f.function, f.line)).collect();
        assert_eq!(frames, [("send_stream", 1013), ("calc_wheel_effect", 1066)]);
    }

    #[test]
    fn engine_effect_needs_its_switch() {
        let p = builtin("dof-reality-p3").unwrap().unwrap();
        let mut tune = MotionTune::default();
        let mut inputs = quiet();
        inputs.rpm = PyValue::Float(2000.0);
        inputs.max_rpm = PyValue::Float(8000.0);
        let mut rng = MaxRng::default();
        apply_wheel_effects(&mut [2000; SLOTS], &p, &src(&MotionSettings::default(), &tune), &inputs, &mut rng)
            .unwrap();
        assert!(rng.0.is_empty());
        tune.switches.engine = true;
        apply_wheel_effects(&mut [2000; SLOTS], &p, &src(&MotionSettings::default(), &tune), &inputs, &mut rng)
            .unwrap();
        // 1 mapped from 0..20 onto 0..100 = 5 -> 5 / 100 · 4095 · 100 · 0.05 / 100 truncated to 10; 3.75 · 10 truncated to 37.
        assert_eq!(rng.0, vec![(1, 37); 4]);
    }

    #[test]
    fn landing_gear_and_canopy_fire_while_moving() {
        let p = builtin("dof-reality-h6").unwrap().unwrap();
        let tune = MotionTune::default();
        let mut inputs = quiet();
        inputs.landing_gear_left = 0.15;
        inputs.landing_gear_right = 0.86;
        inputs.canopy = 0.5;
        let mut rng = MaxRng::default();
        let mut positions = [2000; SLOTS];
        apply_wheel_effects(&mut positions, &p, &src(&MotionSettings::default(), &tune), &inputs, &mut rng).unwrap();
        // canopy: lf (slot 0), rf (slot 3); landing gear left: lr (slot 1), lf (slot 0).
        assert_eq!(rng.0.len(), 4);
        assert_eq!(positions[0], 2000 + 280 * 2);
        assert_eq!(positions[1], 2280);
        assert_eq!(positions[3], 2280);
        assert_eq!(positions[4], 2000);
    }
}
