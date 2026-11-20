//! `openrigcue-bridge.exe` entry point.

#[cfg(windows)]
fn main() {
    use clap::Parser;
    let options = openrigcue_bridge::cli::Options::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("OPENRIGCUE_BRIDGE_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    std::process::exit(openrigcue_bridge::windows::run(options));
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!(
        "openrigcue-bridge is a Windows program: build it for x86_64-pc-windows-gnu and run it inside the game's Proton prefix"
    );
    std::process::ExitCode::from(2)
}
