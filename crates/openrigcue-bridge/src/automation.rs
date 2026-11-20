//! Bounded UI Automation requests, optionally retaining controls for one routine.
use openrigcue_model::automation::{Request, Response};
use std::io::{Read, Write};

#[cfg(any(windows, test))]
mod automation_actions;
#[cfg(any(windows, test))]
mod automation_tree;

#[cfg(windows)]
#[path = "windows/automation.rs"]
mod native;

fn main() {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--session")) {
        session();
        return;
    }
    let response = request();
    let mut output = std::io::stdout().lock();
    if let Ok(bytes) = serde_json::to_vec(&response) {
        let _ = output.write_all(&bytes);
        let _ = output.write_all(b"\n");
    }
}

fn session() {
    use std::io::BufRead;
    #[cfg(windows)]
    let mut native = match native::session::Session::new() {
        Ok(session) => session,
        Err(error) => {
            println!("{}", serde_json::to_string(&Response { error: Some(error), ..Default::default() }).unwrap());
            return;
        }
    };
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    loop {
        let mut bytes = Vec::new();
        match input.by_ref().take(65_537).read_until(b'\n', &mut bytes) {
            Ok(0) => break,
            Ok(_) if bytes.len() <= 65_536 => {}
            _ => break,
        }
        let request = serde_json::from_slice::<openrigcue_model::automation::SessionRequest>(&bytes);
        #[cfg(windows)]
        let result =
            request.map_err(|_| "Invalid routine automation request".to_owned()).and_then(|r| native.request(r));
        #[cfg(not(windows))]
        let result: Result<Response, String> = {
            let _ = request;
            Err("Desktop automation helper must run inside the selected Wine prefix".into())
        };
        let response = result.unwrap_or_else(|error| Response { error: Some(error), ..Default::default() });
        if serde_json::to_writer(&mut output, &response).is_err()
            || output.write_all(b"\n").is_err()
            || output.flush().is_err()
        {
            break;
        }
    }
}
fn request() -> Response {
    let mut bytes = Vec::new();
    if std::io::stdin().take(65_537).read_to_end(&mut bytes).is_err() || bytes.len() > 65_536 {
        return Response { error: Some("Automation request is unreadable or too large".into()), ..Default::default() };
    }
    let request: Request = match serde_json::from_slice(&bytes) {
        Ok(request) => request,
        Err(_) => return Response { error: Some("Invalid automation request".into()), ..Default::default() },
    };
    #[cfg(windows)]
    let result = native::run(request);
    #[cfg(not(windows))]
    let result: Result<Response, String> = {
        let _ = request;
        Err("Desktop automation helper must run inside the selected Wine prefix".into())
    };
    result.unwrap_or_else(|error| Response { error: Some(error), ..Default::default() })
}
