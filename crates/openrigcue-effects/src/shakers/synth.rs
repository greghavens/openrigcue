//! Built-in shaker sounds: OpenRigCue's own 14 effect sounds. These are the default; importing
//! the user's own SRS sound files (`sounds/<folder>/<n>.ogg`) is an option
//! ([`super::samples::SampleBank::import`]).
//!
//! Each sound is synthesised sample by sample from a few figures
//! ([`super::synth_data`]): duration, dominant frequency, spectral centroid below 400 Hz, overall
//! RMS and the RMS envelope in 5 ms windows. No recorded audio is stored or shipped.
//!
//! Recipe, per sound:
//! - carrier `sin(2π f0 t)` at the dominant frequency `f0`, plus `beta` times every
//!   harmonic `k f0` below 400 Hz (`beta > 0`) or `|beta|` times the half-frequency partial
//!   `f0 / 2` (`beta < 0`); `beta` sets the spectral centroid;
//! - amplitude: the 5 ms envelope, smoothed by an RMS over half a period of the lowest
//!   partial (the 5 ms windows of a low tone ripple with its phase), interpolated linearly
//!   between window centres;
//! - scaled to the target RMS. The sound is mono; the mixer feeds it to every speaker through
//!   the group levels.

use std::f64::consts::TAU;

use super::samples::{Sample, SampleKey};
use super::synth_data::MEASURED;

/// Window of the envelope, seconds.
pub(super) const ENVELOPE_STEP: f64 = 0.005;
/// Partials stop below this frequency (the shaker band).
const BAND_HZ: f64 = 400.0;

/// The figures of one sound, plus the tuned partial weight.
#[derive(Debug, Clone, Copy)]
pub(super) struct Measured {
    pub key: SampleKey,
    /// Sample rate of the built-in sound.
    pub rate: u32,
    pub seconds: f64,
    pub dominant_hz: f64,
    /// Spectral centroid below 400 Hz (checked by the comparison test; `beta` encodes it).
    #[cfg_attr(not(test), allow(dead_code))]
    pub centroid_hz: f64,
    /// Target RMS.
    pub rms: f64,
    /// Weight of the extra partials (see the module doc).
    pub beta: f64,
    /// RMS per 5 ms window, relative to the loudest window.
    pub envelope: &'static [f64],
}

impl Measured {
    pub(super) fn of(key: SampleKey) -> Option<&'static Measured> {
        MEASURED.iter().find(|m| m.key == key)
    }

    /// Frequency of the lowest partial of the carrier.
    fn lowest_hz(&self) -> f64 {
        if self.beta < 0.0 {
            self.dominant_hz / 2.0
        } else {
            self.dominant_hz
        }
    }

    /// Envelope windows per smoothing RMS: half a period of the lowest partial, at least one.
    pub(super) fn smoothing_steps(&self) -> usize {
        if self.dominant_hz <= 0.0 {
            return 1;
        }
        ((1.0 / (2.0 * self.lowest_hz() * ENVELOPE_STEP)).round() as usize).max(1)
    }

    fn carrier(&self, t: f64) -> f64 {
        let f0 = self.dominant_hz;
        let mut c = (TAU * f0 * t).sin();
        if self.beta >= 0.0 {
            let top = (BAND_HZ / f0).floor() as u32;
            for k in 2..=top {
                c += self.beta * (TAU * f64::from(k) * f0 * t).sin();
            }
        } else {
            c += -self.beta * (TAU * f0 / 2.0 * t).sin();
        }
        c
    }
}

/// RMS over `steps` neighbouring values (centred, clipped at the ends) of `env`.
pub(super) fn smooth_envelope(env: &[f64], steps: usize) -> Vec<f64> {
    let half = steps / 2;
    (0..env.len())
        .map(|k| {
            let window = &env[k.saturating_sub(half)..(k + half + 1).min(env.len())];
            (window.iter().map(|v| v * v).sum::<f64>() / window.len() as f64).sqrt()
        })
        .collect()
}

/// Linear interpolation of `values` placed at the 5 ms window centres, held at both ends.
fn interpolate(values: &[f64], t: f64) -> f64 {
    let x = t / ENVELOPE_STEP - 0.5;
    if x <= 0.0 {
        return values[0];
    }
    let i = x.floor() as usize;
    if i + 1 >= values.len() {
        return values[values.len() - 1];
    }
    let frac = x - i as f64;
    values[i] + (values[i + 1] - values[i]) * frac
}

/// Render the recipe as mono samples.
fn render(m: &Measured) -> Vec<f32> {
    let frames = (m.seconds * f64::from(m.rate)).round() as usize;
    if m.rms <= 0.0 || m.envelope.is_empty() {
        return vec![0.0; frames];
    }
    let amplitude = smooth_envelope(m.envelope, m.smoothing_steps());
    let rate = f64::from(m.rate);
    let raw: Vec<f64> = (0..frames)
        .map(|n| {
            let t = n as f64 / rate;
            interpolate(&amplitude, t) * m.carrier(t)
        })
        .collect();
    let rms = (raw.iter().map(|v| v * v).sum::<f64>() / frames.max(1) as f64).sqrt();
    let gain = if rms > 0.0 { m.rms / rms } else { 0.0 };
    raw.into_iter().map(|v| (v * gain) as f32).collect()
}

/// The built-in sound for `key`, or `None` for keys the engine never plays.
pub fn synthesize(key: SampleKey) -> Option<Sample> {
    let m = Measured::of(key)?;
    Some(Sample { rate: m.rate, channels: 1, data: render(m) })
}

#[cfg(test)]
mod tests {
    //! Offline check of every built-in sound against its recipe figures.
    //! The measurement is the one of `testdata/measure.py`: Hann window, spectrum
    //! zero-padded to 8x the length, band below 400 Hz; RMS in windows of `rate * 0.005`
    //! samples (truncated), relative to the loudest. Tolerances:
    //! - duration: within one millisecond;
    //! - dominant frequency: within 2 % (at least 1 Hz);
    //! - spectral centroid: within 5 %;
    //! - overall RMS (the relative level between sounds): within 0.25 dB;
    //! - envelope, both sides smoothed as the recipe smooths it: mean absolute difference at
    //!   most 0.12 and no window off by more than 0.45 (relative to the loudest window); the
    //!   loudest window within two smoothing widths (at least three windows) of the target's,
    //!   or on a window where the target is within 90 % of its peak (several envelopes have
    //!   repeated bursts of equal height); the last window above -20 dB (the end of the decay)
    //!   within two smoothing widths.
    use super::*;
    use crate::shakers::samples::SampleFolder;

    struct Analysis {
        seconds: f64,
        dominant_hz: f64,
        centroid_hz: f64,
        rms: f64,
        envelope: Vec<f64>,
    }

    /// `measure.py`'s figures for mono `x` at `rate`.
    fn analyse(x: &[f32], rate: u32) -> Analysis {
        let n = x.len();
        let sr = f64::from(rate);
        let windowed: Vec<f64> = x
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                let w = 0.5 - 0.5 * (TAU * i as f64 / (n as f64 - 1.0)).cos();
                f64::from(v) * w
            })
            .collect();
        // Goertzel at every bin of the 8x zero-padded transform below 400 Hz.
        let padded = (8 * n) as f64;
        let (mut best, mut best_hz, mut sum, mut weighted) = (0.0f64, 0.0, 0.0, 0.0);
        let mut j = 0usize;
        loop {
            let hz = j as f64 * sr / padded;
            if hz >= BAND_HZ {
                break;
            }
            let coeff = 2.0 * (TAU * j as f64 / padded).cos();
            let (mut s1, mut s2) = (0.0f64, 0.0f64);
            for &v in &windowed {
                let s0 = v + coeff * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            let mag = (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt();
            if mag > best {
                best = mag;
                best_hz = hz;
            }
            sum += mag;
            weighted += mag * hz;
            j += 1;
        }
        let hop = (sr * ENVELOPE_STEP) as usize;
        let raw: Vec<f64> = x
            .chunks(hop)
            .map(|c| (c.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / c.len() as f64).sqrt())
            .collect();
        let peak = raw.iter().copied().fold(0.0, f64::max);
        Analysis {
            seconds: n as f64 / sr,
            dominant_hz: best_hz,
            centroid_hz: if sum > 0.0 { weighted / sum } else { 0.0 },
            rms: (x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / n as f64).sqrt(),
            envelope: raw.iter().map(|v| v / peak).collect(),
        }
    }

    fn normalised(v: Vec<f64>) -> Vec<f64> {
        let peak = v.iter().copied().fold(0.0, f64::max);
        v.into_iter().map(|x| x / peak).collect()
    }

    fn argmax(v: &[f64]) -> usize {
        v.iter().enumerate().fold((0, f64::MIN), |(bi, bv), (i, &x)| if x > bv { (i, x) } else { (bi, bv) }).0
    }

    fn last_above(v: &[f64], level: f64) -> usize {
        v.iter().rposition(|&x| x >= level).unwrap_or(0)
    }

    /// The comparison test: every sound against its recipe figures.
    #[test]
    fn built_in_sounds_match_srs_sample_measurements() {
        for m in &MEASURED {
            let s = synthesize(m.key).unwrap();
            assert_eq!((s.rate, s.channels), (m.rate, 1), "{:?}", m.key);
            let a = analyse(&s.data, s.rate);
            let name = m.key.relative_path();
            assert!((a.seconds - m.seconds).abs() <= 0.001, "{name}: {} s", a.seconds);
            if m.rms == 0.0 {
                assert!(s.data.iter().all(|&v| v == 0.0), "{name} must be silent");
                continue;
            }
            let hz_tol = (m.dominant_hz * 0.02).max(1.0);
            assert!(
                (a.dominant_hz - m.dominant_hz).abs() <= hz_tol,
                "{name}: dominant {} Hz, measured {}",
                a.dominant_hz,
                m.dominant_hz
            );
            assert!(
                (a.centroid_hz - m.centroid_hz).abs() <= m.centroid_hz * 0.05,
                "{name}: centroid {} Hz, measured {}",
                a.centroid_hz,
                m.centroid_hz
            );
            let db = 20.0 * (a.rms / m.rms).log10();
            assert!(db.abs() <= 0.25, "{name}: level {db:+.2} dB off");

            let steps = m.smoothing_steps();
            let want = normalised(smooth_envelope(m.envelope, steps));
            let got = normalised(smooth_envelope(&a.envelope, steps));
            let len = want.len().min(got.len());
            assert!(want.len().abs_diff(got.len()) <= 1, "{name}: envelope length");
            let diffs: Vec<f64> = (0..len).map(|k| (want[k] - got[k]).abs()).collect();
            let mean = diffs.iter().sum::<f64>() / len as f64;
            let worst = diffs.iter().copied().fold(0.0, f64::max);
            assert!(mean <= 0.12, "{name}: envelope mean difference {mean:.3}");
            assert!(worst <= 0.45, "{name}: envelope worst difference {worst:.3}");
            let reach = 2 * steps;
            assert!(
                argmax(&want).abs_diff(argmax(&got)) <= reach.max(3) || want[argmax(&got)] >= 0.9,
                "{name}: envelope peak at window {} instead of {}",
                argmax(&got),
                argmax(&want)
            );
            assert!(
                last_above(&want, 0.1).abs_diff(last_above(&got, 0.1)) <= reach,
                "{name}: decay ends at window {} instead of {}",
                last_above(&got, 0.1),
                last_above(&want, 0.1)
            );
        }
    }

    #[test]
    fn every_played_key_has_a_built_in_sound() {
        for key in SampleKey::PLAYED {
            assert!(synthesize(key).is_some(), "{key:?}");
        }
        assert_eq!(MEASURED.len(), SampleKey::PLAYED.len());
        assert!(synthesize(SampleKey::new(SampleFolder::Transmission, 2)).is_none());
    }

    /// The level order of the suspension set (samples 0..6 are picked by
    /// intensity): 0 is silence, and 2 < 3 < 4 < 5 < 6 in RMS.
    #[test]
    fn suspension_set_rises_in_level() {
        let rms = |i| {
            let s = synthesize(SampleKey::new(SampleFolder::Suspension, i)).unwrap();
            (s.data.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / s.data.len() as f64).sqrt()
        };
        assert_eq!(rms(0), 0.0);
        for i in 2..6 {
            assert!(rms(i) < rms(i + 1), "suspension/{i}");
        }
    }

    #[test]
    fn turbulence_keeps_its_48_khz_rate() {
        let t = synthesize(SampleKey::new(SampleFolder::Turbulence, 0)).unwrap();
        assert_eq!(t.rate, 48_000);
        assert_eq!(t.frames(), (0.4954f64 * 48_000.0).round() as usize);
    }

    #[test]
    fn smoothing_is_a_centred_rms() {
        // Two steps: one neighbour on each side.
        let s = smooth_envelope(&[0.0, 1.0, 0.0, 1.0], 2);
        let want = [0.5f64.sqrt(), (1.0f64 / 3.0).sqrt(), (2.0f64 / 3.0).sqrt(), 0.5f64.sqrt()];
        for (a, b) in s.iter().zip(want) {
            assert!((a - b).abs() < 1e-12);
        }
        assert_eq!(smooth_envelope(&[0.2, 0.4], 1), vec![0.2, 0.4]);
    }
}
