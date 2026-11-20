use openrigcue_devices::discovery::{self, PortInfo};
use openrigcue_devices::motion::smc3;
use openrigcue_devices::motion::SystemHost;

/// USB adapters used by SMC3-class controllers: CH340, STM32 virtual COM, FTDI, CP210x, Arduino.
const CONTROLLER_USB_IDS: &[(u16, Option<u16>)] =
    &[(0x1a86, Some(0x7523)), (0x0483, Some(0x5740)), (0x0403, Some(0x6001)), (0x10c4, Some(0xea60)), (0x2341, None)];

#[derive(clap::Args)]
pub struct Args {
    /// Probe only this port.
    #[arg(long)]
    port: Option<String>,
    /// Also probe ports whose USB ids are not known controller adapters.
    #[arg(long)]
    all: bool,
    /// Only list ports; do not send anything.
    #[arg(long)]
    list: bool,
}

pub fn is_controller_candidate(p: &PortInfo) -> bool {
    p.usb.as_ref().is_some_and(|u| {
        CONTROLLER_USB_IDS.iter().any(|&(v, pid)| u.vendor_id == v && pid.is_none_or(|pid| pid == u.product_id))
    })
}

/// Ports to probe for controllers, in a stable order.
pub fn candidates(port: Option<&str>, all: bool) -> Vec<String> {
    match port {
        Some(p) => vec![p.to_string()],
        None => discovery::serial_ports()
            .into_iter()
            .filter(|p| all || is_controller_candidate(p))
            .map(|p| p.path)
            .collect(),
    }
}

pub fn run(args: &Args) -> anyhow::Result<()> {
    let ports = discovery::serial_ports();
    if ports.is_empty() {
        println!("No USB serial devices found. Is the motion controller plugged in and powered?");
        return Ok(());
    }
    println!("USB serial devices:");
    for p in &ports {
        let mark = if is_controller_candidate(p) { "*" } else { " " };
        println!(" {mark} {:<16} {:<40} {}", p.path, p.describe(), p.stable_path.as_deref().unwrap_or(""));
    }
    if args.list {
        return Ok(());
    }
    let targets = candidates(args.port.as_deref(), args.all);
    if targets.is_empty() {
        println!("\nNo known controller adapter found; use --all or --port to probe anyway.");
        return Ok(());
    }
    println!("\nProbing (up to 10 s per port; the controller resets when its port opens)...");
    let mut host = SystemHost::default();
    for path in targets {
        match smc3::identify_port(&path, &mut host) {
            Ok(id) => println!(
                "  {path}: SMC3 version {} ({}-bit), model id {}, box {}",
                id.version.version,
                id.resolution.0,
                id.model_id.map_or("-".to_string(), |m| m.to_string()),
                id.box_slot.map_or("-".to_string(), |b| (b.index() + 1).to_string()),
            ),
            Err(e) => println!("  {path}: {e}"),
        }
    }
    Ok(())
}
