//! Entry point for the independent native maintenance tools.
use clap::Args;
use std::{
    net::Ipv4Addr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
#[derive(Args, Debug)]
pub struct Options {
    /// Serve in a browser instead of opening the native window.
    #[arg(long)]
    pub headless: bool,
    /// Local maintenance UI port (127.0.0.1 only).
    #[arg(long, default_value_t = 33734)]
    pub port: u16,
    /// Print a USB report without starting device control.
    #[arg(long)]
    pub usb_report: Option<PathBuf>,
    /// Use XML when writing a USB report.
    #[arg(long)]
    pub xml: bool,
}
pub fn run(options: Options) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(path) = options.usb_report {
        let devices = crate::usb::enumerate(std::path::Path::new("/sys/bus/usb/devices"))?;
        let report = if options.xml { crate::usb::xml_report(&devices) } else { crate::usb::text_report(&devices) };
        std::fs::write(path, report)?;
        return Ok(());
    }
    let socket = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, options.port))?;
    let port = socket.local_addr()?.port();
    socket.set_nonblocking(true)?;
    let state = Arc::new(Mutex::new(crate::web::Tools::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = stop.clone();
    let tick_stop = stop.clone();
    let server_state = state.clone();
    let server = std::thread::spawn(move || -> Result<(), String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(socket).map_err(|e| e.to_string())?;
            axum::serve(listener, crate::web::router(server_state))
                .with_graceful_shutdown(async move {
                    while !server_stop.load(Ordering::Relaxed) {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                })
                .await
                .map_err(|e| e.to_string())
        })
    });
    let polling = crate::web::poller(state.clone(), tick_stop);
    let signal = stop.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))?;
    let address = format!("http://127.0.0.1:{port}");
    println!("OpenRigCue Tools: {address}");
    let outcome = (|| -> Result<(), Box<dyn std::error::Error>> {
        if options.headless {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
        } else {
            let mut window = openrigcue_desktop::Desktop::new()?;
            let mut settings = openrigcue_config::settings::Settings::default().native_snapshot();
            if let Some(app) =
                settings.get_mut("APP").and_then(openrigcue_model::native_value::LegacyValue::as_dict_mut)
            {
                use openrigcue_model::native_value::LegacyValue;
                app.insert("window_width", LegacyValue::Int(1200));
                app.insert("window_height", LegacyValue::Int(900));
                app.insert("window_x_pos", LegacyValue::Int(60));
                app.insert("window_y_pos", LegacyValue::Int(60));
                app.insert("x_button_exit", LegacyValue::Bool(true));
            }
            window.set_config_reader(Arc::new(move || settings.clone()));
            window.open(&address)?;
            while !stop.load(Ordering::Relaxed) && !window.dispatch_pending() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    let _ = polling.join();
    let result = server.join().map_err(|_| "Maintenance server failed")?;
    result?;
    outcome
}
