//! `openrigcue motion test`: slow single-axis motion through the test-mode cueing and the opt-in safety supervisor.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use openrigcue_cueing::{ClipFactors, MotionCueing, MotionSettings, MotionTune, Platform, TestContext, TestSliders};
use openrigcue_devices::motion::smc3::{Smc3Driver, Smc3Settings};
use openrigcue_devices::motion::{
    motion_model, LinkState, LiveSettings, ModelSpec, MotionContext, MotionDriver, Supervisor, SupervisorConfig,
    SupervisorState, SystemHost,
};
use tracing::{info, warn};

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum AxisArg {
    Pitch,
    Roll,
    Yaw,
    Sway,
    Surge,
    Heave,
    SlipRear,
    SlipFront,
}

/// Test sliders with `value` (−100…100) on `axis` only.
pub fn sliders(axis: AxisArg, value: f64) -> TestSliders {
    let mut s = TestSliders::default();
    let slot = match axis {
        AxisArg::Pitch => &mut s.pitch,
        AxisArg::Roll => &mut s.roll,
        AxisArg::Yaw => &mut s.yaw,
        AxisArg::Sway => &mut s.sway,
        AxisArg::Surge => &mut s.surge,
        AxisArg::Heave => &mut s.heave,
        AxisArg::SlipRear => &mut s.traction_loss,
        AxisArg::SlipFront => &mut s.traction_loss_front,
    };
    *slot = value;
    s
}

#[derive(clap::Args)]
pub struct Args {
    /// Platform model id (see `openrigcue platforms`), e.g. `dof-reality-p3`.
    #[arg(long)]
    platform: String,
    /// Controller port(s); probed automatically when omitted.
    #[arg(long)]
    port: Vec<String>,
    #[arg(long, value_enum, default_value = "pitch")]
    axis: AxisArg,
    /// Test slider amplitude, percent (0..100). 0 holds the centre.
    #[arg(long, default_value_t = 20.0)]
    amplitude: f64,
    /// Seconds per back-and-forth cycle.
    #[arg(long, default_value_t = 4.0)]
    period: f64,
    /// Seconds of motion before releasing to rest.
    #[arg(long, default_value_t = 20.0)]
    duration: f64,
    #[arg(long, default_value_t = 100)]
    rate: u32,
    /// Seconds to only listen to controller feedback before engaging and after releasing.
    #[arg(long, default_value_t = 8.0)]
    listen: f64,
    /// Compute and print positions without opening any device.
    #[arg(long)]
    dry_run: bool,
}

/// Test-mode positions of the platform's actuators for `sliders`.
pub fn test_positions(cueing: &MotionCueing, sliders: &TestSliders) -> anyhow::Result<Vec<u32>> {
    let (settings, tune) = (MotionSettings::default(), MotionTune::default());
    let ctx = TestContext { settings: &settings, tune: &tune, game: "none" };
    let (Some(platform), Some(slots)) =
        (cueing.platform(), cueing.test(sliders, &ctx).map_err(|e| anyhow::anyhow!("{e:?}"))?)
    else {
        return Ok(Vec::new());
    };
    Ok(slots.iter().take(platform.motors).map(|&v| u32::try_from(v.max(0)).unwrap_or(0)).collect())
}

pub fn cueing(platform: &Platform) -> MotionCueing {
    MotionCueing::new(platform.clone(), ClipFactors::default())
}

/// A connected SMC3 platform for the bring-up commands.
pub struct Rig {
    pub driver: Smc3Driver,
    host: SystemHost,
    link: LinkState,
    model: &'static ModelSpec,
}

impl Rig {
    /// Scan `ports` (or the known controller adapters) and connect, without touching gains.
    pub fn connect(platform: &Platform, ports: &[String]) -> anyhow::Result<Rig> {
        if platform.controller != "smc3" {
            bail!("{}: only DOF Reality / SMC3 platforms are supported by this command", platform.id);
        }
        let model = motion_model(&platform.model, false)
            .with_context(|| format!("{}: no motion model {:?}", platform.id, platform.model))?;
        let candidates = if ports.is_empty() { crate::probe::candidates(None, false) } else { ports.to_vec() };
        if candidates.is_empty() {
            bail!("no controller port found; pass --port");
        }
        let mut host = SystemHost::default();
        host.allowed_com_ports = candidates.join(",");
        let settings = Smc3Settings { update_kp: false, ..Smc3Settings::default() };
        let mut driver = Smc3Driver::new(LiveSettings::new(settings));
        let mut link = LinkState::default();
        driver.connect(&mut MotionContext { model, link: &mut link, host: &mut host })?;
        if !link.is_connected() || link.ports.len() != model.controllers {
            bail!("expected {} controller(s), connected {:?}", model.controllers, link.ports);
        }
        Ok(Rig { driver, host, link, model })
    }

    /// Send one frame of actuator positions; unused slots hold the centre.
    pub fn command(&mut self, positions: &[u32]) -> anyhow::Result<()> {
        let centre = (self.model.max_motor_abs + 1) / 2;
        let mut motors = [centre; 12];
        for (slot, &p) in motors.iter_mut().zip(positions) {
            *slot = i64::from(p);
        }
        self.driver.send_stream(
            &motors,
            &mut MotionContext { model: self.model, link: &mut self.link, host: &mut self.host },
        )?;
        if !self.link.is_connected() {
            bail!("controller write failed");
        }
        Ok(())
    }

    pub fn request_feedback(&mut self) -> anyhow::Result<()> {
        Ok(self.driver.request_feedback()?)
    }

    pub fn collect_feedback(&mut self) -> anyhow::Result<Vec<Option<(u32, u32)>>> {
        Ok(self.driver.collect_feedback(self.model.motors)?)
    }

    pub fn read_positions(&mut self) -> anyhow::Result<Option<Vec<u32>>> {
        let mut clock = openrigcue_devices::motion::clock::SystemClock::default();
        Ok(self.driver.read_positions(self.model.motors, &mut clock)?)
    }

    pub fn describe(&self) -> String {
        self.driver.describe()
    }
}

pub fn fmt_feedback(fb: &[Option<(u32, u32)>]) -> String {
    let parts: Vec<String> =
        fb.iter().map(|v| v.map_or("-".to_string(), |(pos, tgt)| format!("{pos}/{tgt}"))).collect();
    parts.join(" ")
}

/// Poll controller feedback for `seconds` without sending positions.
fn listen(rig: &mut Rig, seconds: f64, label: &str, stop: &AtomicBool) -> anyhow::Result<()> {
    let start = Instant::now();
    println!("{label}: listening only for {seconds:.0} s (no position commands)");
    while start.elapsed().as_secs_f64() < seconds && !stop.load(Ordering::SeqCst) {
        rig.request_feedback()?;
        std::thread::sleep(Duration::from_millis(250));
        let fb = rig.collect_feedback()?;
        println!("{:>6.2}s {label:<9} feedback(pos/fw-target) {}", start.elapsed().as_secs_f64(), fmt_feedback(&fb));
    }
    Ok(())
}

pub fn run(args: &Args) -> anyhow::Result<()> {
    let platform = openrigcue_cueing::builtin(&args.platform)
        .with_context(|| format!("unknown platform {:?}; see `openrigcue platforms`", args.platform))??;
    let amplitude = args.amplitude.clamp(0.0, 100.0);

    let mut rig = if args.dry_run { None } else { Some(Rig::connect(&platform, &args.port)?) };
    if let Some(r) = &rig {
        info!("{}", r.describe());
    }

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst)).context("installing Ctrl-C handler")?;
    }

    let cueing = cueing(&platform);
    let full_scale = platform.max_motor_abs;
    let mut supervisor = Supervisor::new(SupervisorConfig::centred(platform.motors, full_scale));

    if let Some(r) = rig.as_mut() {
        listen(r, args.listen, "before", &stop)?;
    }

    // Start the engage ramp from where the rig really is. Opening the port resets the controller, which
    // lets the rig settle low, so without a measurement assume the bottom of travel rather than the centre.
    let low = vec![(f64::from(full_scale) * 0.05) as u32; platform.motors];
    let start_from = match rig.as_mut().map(|r| r.read_positions()) {
        Some(Ok(Some(measured))) => {
            println!("Measured positions: {measured:?}");
            measured
        }
        Some(Ok(None)) | None => low,
        Some(Err(e)) => {
            warn!("could not read positions ({e}); assuming the rig is at the bottom of travel");
            low
        }
    };
    supervisor.set_position(&start_from);

    let period = Duration::from_secs_f64(1.0 / f64::from(args.rate.max(1)));
    let start = Instant::now();
    supervisor.engage(Duration::ZERO);
    println!("Engaging {} ({}), amplitude {amplitude} %. Ctrl-C releases to rest.", platform.name, platform.id);

    let mut next = start;
    let mut last_print = Duration::ZERO;
    loop {
        let now = start.elapsed();
        let releasing = stop.load(Ordering::SeqCst) || now.as_secs_f64() >= args.duration;
        if releasing && matches!(supervisor.state(), SupervisorState::Engaging | SupervisorState::Running) {
            println!("Releasing to rest...");
            supervisor.release(now);
        }
        let target = if releasing {
            None
        } else {
            // Hold still during the engage ramp, then oscillate.
            let t = now.as_secs_f64() - 5.0;
            let value = if t > 0.0 { amplitude * (2.0 * std::f64::consts::PI * t / args.period).sin() } else { 0.0 };
            Some(test_positions(&cueing, &sliders(args.axis, value))?)
        };
        if let Some(positions) = supervisor.update(now, target.as_deref()) {
            if let Some(r) = rig.as_mut() {
                if let Err(e) = r.command(&positions) {
                    warn!("controller error: {e}; stopping");
                    supervisor.fault();
                    break;
                }
            }
            if now - last_print >= Duration::from_millis(250) {
                let fb = match rig.as_mut() {
                    Some(r) => {
                        let fb = r.collect_feedback()?;
                        r.request_feedback()?;
                        fmt_feedback(&fb)
                    }
                    None => String::new(),
                };
                println!(
                    "{:>6.2}s {:<9} cmd {:?} feedback(pos/fw-target) {fb}",
                    now.as_secs_f64(),
                    format!("{:?}", supervisor.state()),
                    positions
                );
                last_print = now;
            }
        }
        if supervisor.state() == SupervisorState::Parked {
            println!("At rest.");
            if let Some(r) = rig.as_mut() {
                listen(r, args.listen, "after", &stop)?;
            }
            break;
        }
        next += period;
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
    Ok(())
}
