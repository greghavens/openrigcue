//! `openrigcue motion diag`: scripted multi-axis sequence with controller feedback logged to CSV.

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::Context;
use openrigcue_devices::motion::{Supervisor, SupervisorConfig, SupervisorState};

use crate::motion_test::{cueing, sliders, test_positions, AxisArg, Rig};

#[derive(clap::Args)]
pub struct Args {
    #[arg(long)]
    platform: String,
    #[arg(long)]
    port: Vec<String>,
    /// Test slider amplitude, percent.
    #[arg(long, default_value_t = 50.0)]
    amplitude: f64,
    /// CSV output path.
    #[arg(long)]
    csv: String,
}

#[derive(Clone, Copy)]
enum Phase {
    Listen,
    Engage,
    Hold,
    Axis(AxisArg, &'static str),
    Release,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Listen => "listen",
            Phase::Engage => "engage",
            Phase::Hold => "hold",
            Phase::Axis(_, name) => name,
            Phase::Release => "release",
        }
    }
}

pub fn run(args: &Args) -> anyhow::Result<()> {
    let platform = openrigcue_cueing::builtin(&args.platform).context("unknown platform")??;
    let mut rig = Rig::connect(&platform, &args.port)?;
    let mut out = std::fs::File::create(&args.csv)?;
    let n = platform.motors;
    let header: Vec<String> = (0..n).flat_map(|i| [format!("cmd{i}"), format!("fb{i}"), format!("fwt{i}")]).collect();
    writeln!(out, "t,phase,state,{}", header.join(","))?;

    let cueing = cueing(&platform);
    let full_scale = platform.max_motor_abs;
    let mut sup = Supervisor::new(SupervisorConfig::centred(n, full_scale));

    let axis_secs = 6.0; // two 3 s cycles
    let script: Vec<(Phase, f64)> = vec![
        (Phase::Listen, 2.0),
        (Phase::Engage, 5.0),
        (Phase::Hold, 2.0),
        (Phase::Axis(AxisArg::Pitch, "pitch"), axis_secs),
        (Phase::Axis(AxisArg::Roll, "roll"), axis_secs),
        (Phase::Axis(AxisArg::SlipRear, "slip_rear"), axis_secs),
        (Phase::Axis(AxisArg::Heave, "heave"), axis_secs),
        (Phase::Hold, 1.0),
        (Phase::Release, 3.5),
        (Phase::Listen, 4.0),
    ];

    let tick = Duration::from_millis(50);
    let start = Instant::now();
    let mut next = start;
    let mut seg = 0usize;
    let mut seg_start = 0.0;
    let mut last_cmd: Vec<u32> = vec![0; n];
    loop {
        let t = start.elapsed().as_secs_f64();
        while seg < script.len() && t - seg_start >= script[seg].1 {
            seg_start += script[seg].1;
            seg += 1;
            if let Some((phase, _)) = script.get(seg) {
                let now = start.elapsed();
                match phase {
                    Phase::Engage => {
                        if let Some(m) = rig.read_positions()? {
                            sup.set_position(&m);
                        }
                        sup.engage(now);
                    }
                    Phase::Release => sup.release(now),
                    _ => {}
                }
            }
        }
        let Some(&(phase, _)) = script.get(seg) else {
            break;
        };
        let now = start.elapsed();
        let local = t - seg_start;

        let target = match phase {
            Phase::Listen | Phase::Release => None,
            Phase::Engage | Phase::Hold => Some(test_positions(&cueing, &sliders(AxisArg::Pitch, 0.0))?),
            Phase::Axis(axis, _) => {
                let value = args.amplitude * (2.0 * std::f64::consts::PI * local / 3.0).sin();
                Some(test_positions(&cueing, &sliders(axis, value))?)
            }
        };
        if let Some(p) = sup.update(now, target.as_deref()) {
            rig.command(&p)?;
            last_cmd = p;
        }
        let fb = rig.collect_feedback()?;
        rig.request_feedback()?;
        let cols: Vec<String> = (0..n)
            .flat_map(|i| {
                let (pos, tgt) = fb
                    .get(i)
                    .copied()
                    .flatten()
                    .map_or((String::new(), String::new()), |(p, g)| (p.to_string(), g.to_string()));
                [last_cmd[i].to_string(), pos, tgt]
            })
            .collect();
        writeln!(out, "{t:.3},{},{:?},{}", phase.name(), sup.state(), cols.join(","))?;
        if sup.state() == SupervisorState::Parked && matches!(phase, Phase::Listen) && local > 3.9 {
            break;
        }
        next += tick;
        if let Some(w) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(w);
        }
    }
    println!("wrote {}", args.csv);
    Ok(())
}
