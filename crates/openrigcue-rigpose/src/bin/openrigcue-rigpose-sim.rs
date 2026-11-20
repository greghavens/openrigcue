//! Publishes a synthetic moving rig pose for testing VR motion compensation on the headset without
//! moving the rig.
//!
//! With the rig standing still, a consumer that applies this pose moves the *virtual* view the
//! opposite way: `--axis pitch` (nose up first) makes the virtual camera pitch nose-down about the
//! centre of rotation, so the cockpit and horizon appear to rise. One axis at a time verifies the
//! sign convention documented in `RigValues`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use openrigcue_rigpose::{monotonic_now_ns, CorOffset, RigPoseWriter, RigSample, RigValues};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Axis {
    Sway,
    Surge,
    Heave,
    Yaw,
    Roll,
    Pitch,
    /// Every axis at once, each with its own period (a Lissajous-style motion).
    All,
    /// Publish a neutral (all-zero) pose.
    None,
}

/// Publish a synthetic sine-wave rig pose to the OpenRigCue rig-pose file.
#[derive(Debug, Parser)]
#[command(name = "openrigcue-rigpose-sim", version)]
struct Args {
    /// The rig-pose file (default: $OPENRIGCUE_RIG_POSE or /dev/shm/openrigcue/rig-pose).
    #[arg(long)]
    path: Option<PathBuf>,
    /// Which axis to move.
    #[arg(long, value_enum, default_value_t = Axis::Pitch)]
    axis: Axis,
    /// Peak amplitude: degrees for yaw/roll/pitch, millimetres for sway/surge/heave.
    #[arg(long, default_value_t = 5.0)]
    amplitude: f64,
    /// Period of the sine in seconds.
    #[arg(long, default_value_t = 4.0)]
    period: f64,
    /// Publish rate in Hz (the IMU path runs at 200 Hz).
    #[arg(long, default_value_t = 200.0)]
    rate: f64,
    /// Stop after this many seconds (default: run until interrupted).
    #[arg(long)]
    duration: Option<f64>,
    /// Publish with compensation switched off (consumers must pass through).
    #[arg(long)]
    disabled: bool,
    /// Do not publish rates (consumers then cannot predict ahead).
    #[arg(long)]
    no_rates: bool,
    /// Bump the re-centre generation every this many seconds.
    #[arg(long)]
    recenter_every: Option<f64>,
    /// Centre of rotation: metres in front of the eyes.
    #[arg(long, default_value_t = 0.0)]
    cor_forward: f64,
    /// Centre of rotation: metres below the eyes.
    #[arg(long, default_value_t = 0.0)]
    cor_down: f64,
    /// Centre of rotation: metres to the right of the eyes.
    #[arg(long, default_value_t = 0.0)]
    cor_right: f64,
}

/// The value and rate of one axis at time `t` seconds: `a·sin(2πt/T)`.
fn sine(amplitude: f64, period: f64, t: f64) -> (f64, f64) {
    let w = std::f64::consts::TAU / period;
    (amplitude * (w * t).sin(), amplitude * w * (w * t).cos())
}

fn pose_at(args: &Args, t: f64) -> (RigValues, RigValues) {
    let mut v = [0.0; 6];
    let mut r = [0.0; 6];
    let mut set = |i: usize, amplitude: f64, period: f64| {
        let (value, rate) = sine(amplitude, period, t);
        v[i] = value;
        r[i] = rate;
    };
    match args.axis {
        Axis::Sway => set(0, args.amplitude, args.period),
        Axis::Surge => set(1, args.amplitude, args.period),
        Axis::Heave => set(2, args.amplitude, args.period),
        Axis::Yaw => set(3, args.amplitude, args.period),
        Axis::Roll => set(4, args.amplitude, args.period),
        Axis::Pitch => set(5, args.amplitude, args.period),
        Axis::All => {
            for (i, f) in [1.0, 1.3, 0.7, 1.7, 1.1, 0.9].into_iter().enumerate() {
                set(i, args.amplitude, args.period * f);
            }
        }
        Axis::None => {}
    }
    (RigValues::from_array(v), RigValues::from_array(r))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if !(args.rate.is_finite() && args.rate > 0.0 && args.period.is_finite() && args.period > 0.0) {
        return Err("--rate and --period must be positive".into());
    }
    let path = args.path.clone().unwrap_or_else(openrigcue_rigpose::default_path);
    let mut writer = RigPoseWriter::create(&path)?;
    eprintln!(
        "publishing {:?} ±{} every {} s at {} Hz to {} (Ctrl-C to stop)",
        args.axis,
        args.amplitude,
        args.period,
        args.rate,
        path.display()
    );
    let cor_offset = CorOffset { forward: args.cor_forward, down: args.cor_down, right: args.cor_right };
    let interval = Duration::from_secs_f64(1.0 / args.rate);
    let start = Instant::now();
    let mut next = start;
    let mut generation = 0u32;
    let mut last_recenter = 0.0;
    loop {
        let t = start.elapsed().as_secs_f64();
        if args.duration.is_some_and(|d| t >= d) {
            break;
        }
        if let Some(every) = args.recenter_every.filter(|e| *e > 0.0) {
            if t - last_recenter >= every {
                last_recenter = t;
                generation = generation.wrapping_add(1);
                eprintln!("re-centre generation {generation}");
            }
        }
        let (values, rates) = pose_at(&args, t);
        writer.publish(&RigSample {
            timestamp_ns: monotonic_now_ns(),
            values,
            rates: (!args.no_rates).then_some(rates),
            cor_offset,
            enabled: !args.disabled,
            recenter_generation: generation,
        })?;
        next += interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_rate_is_the_derivative() {
        let (a, p, t, h) = (5.0, 4.0, 0.7, 1e-6);
        let (v0, r) = sine(a, p, t);
        let (v1, _) = sine(a, p, t + h);
        assert!(((v1 - v0) / h - r).abs() < 1e-4);
    }

    #[test]
    fn single_axis_moves_only_that_axis() {
        let args = Args::parse_from(["sim", "--axis", "roll", "--amplitude", "3"]);
        let (v, _) = pose_at(&args, 1.0);
        assert!((v.roll - 3.0).abs() < 1e-12);
        assert_eq!(RigValues { roll: 0.0, ..v }, RigValues::ZERO);
    }
}
