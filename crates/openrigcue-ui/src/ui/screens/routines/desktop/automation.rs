//! Scoped Windows control requests. The helper runs in the selected Wine prefix.
use openrigcue_model::automation::{ControlAction, Element, Request, Response, Selector, SessionRequest};
use std::path::PathBuf;
use std::sync::Arc;

pub trait Provider: Send + Sync {
    fn request(&self, request: Request) -> Result<Response, String>;
    fn control(&self, _request: SessionRequest) -> Result<Response, String> {
        Err("Routine control sessions are unavailable".into())
    }
}

mod session;

#[derive(Clone)]
pub struct UiaWindow {
    provider: Arc<dyn Provider>,
    element: Element,
    control: Option<u64>,
}
impl std::fmt::Debug for UiaWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiaWindow").field("element", &self.element).finish_non_exhaustive()
    }
}
impl UiaWindow {
    pub fn root(provider: Arc<dyn Provider>, window: String) -> Self {
        Self { provider, element: Element { window, path: Vec::new() }, control: None }
    }
    pub(super) fn cached_root(provider: Arc<dyn Provider>, window: String) -> Result<Self, String> {
        let control = provider
            .control(SessionRequest::Root { window: window.clone() })?
            .controls
            .first()
            .copied()
            .ok_or("Routine root control missing")?;
        Ok(Self { provider, element: Element { window, path: vec![] }, control: Some(control) })
    }
    fn query(&self, request: Request) -> Result<Response, String> {
        if let Some(control) = self.control {
            let action = match request {
                Request::Exists { .. } => {
                    return self.provider.control(SessionRequest::Exists {
                        control,
                        timeout_ms: 1000,
                        interval_ms: 500,
                    })
                }
                Request::Name { .. } => ControlAction::Name,
                Request::Children { .. } => ControlAction::Children,
                Request::Activate { .. } => ControlAction::Activate,
                Request::Focus { .. } => ControlAction::Focus,
                Request::Topmost { .. } => ControlAction::Topmost,
                Request::Click { .. } => ControlAction::Click,
                _ => return Err("Invalid routine control action".into()),
            };
            return self.provider.control(SessionRequest::Execute { control, action });
        }
        self.provider.request(request)
    }
    pub fn named(&self, value: &str, depth: u32) -> Result<Self, String> {
        self.child(Selector::Name { value: value.into(), depth })
    }
    pub fn by_id(&self, value: &str, depth: u32) -> Result<Self, String> {
        self.child(Selector::AutomationId { value: value.into(), depth })
    }
    fn child(&self, selector: Selector) -> Result<Self, String> {
        let mut child = self.clone();
        if let Some(parent) = self.control {
            child.control = Some(
                self.provider
                    .control(SessionRequest::Child { parent, selector: selector.clone() })?
                    .controls
                    .first()
                    .copied()
                    .ok_or("Routine child control missing")?,
            );
        }
        child.element.path.push(selector);
        // A control is a lazy selector. Name/children/actions
        // resolve it; construction itself does not query the desktop.
        Ok(child)
    }
    pub fn exists(&self) -> Result<bool, String> {
        Ok(self.query(Request::Exists { element: self.element.clone() })?.found)
    }
    pub(super) fn exists_for(&self, timeout_ms: u64, interval_ms: u64) -> Result<bool, String> {
        let control = self.control.ok_or("Routine root control missing")?;
        Ok(self.provider.control(SessionRequest::Exists { control, timeout_ms, interval_ms })?.found)
    }
    pub fn children(&self) -> Result<Vec<Self>, String> {
        let result = self.query(Request::Children { element: self.element.clone() })?;
        let count = if self.control.is_some() { result.controls.len() } else { result.values.len() };
        Ok((0..count)
            .map(|index| {
                let mut child = self.clone();
                child.element.path.push(Selector::Child { index: index as u32 });
                child.control = result.controls.get(index).copied();
                child
            })
            .collect())
    }
    pub fn name(&self) -> Result<String, String> {
        self.query(Request::Name { element: self.element.clone() })?
            .values
            .into_iter()
            .next()
            .ok_or("Automation name missing".into())
    }
    pub fn activate(&self) -> Result<(), String> {
        self.query(Request::Activate { element: self.element.clone() }).map(|_| ())
    }
    pub fn focus(&self) -> Result<(), String> {
        self.query(Request::Focus { element: self.element.clone() }).map(|_| ())
    }
    pub fn topmost(&self) -> Result<(), String> {
        self.query(Request::Topmost { element: self.element.clone() }).map(|_| ())
    }
    pub fn click(&self) -> Result<(), String> {
        self.query(Request::Click { element: self.element.clone() }).map(|_| ())
    }
}

pub struct Helper {
    pub runner: std::ffi::OsString,
    pub executable: PathBuf,
    pub prefix: Option<std::ffi::OsString>,
}
impl Default for Helper {
    fn default() -> Self {
        let adjacent = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join("../lib/openrigcue/openrigcue-automation.exe")));
        let executable = std::env::var_os("OPENRIGCUE_AUTOMATION_EXE")
            .map(PathBuf::from)
            .or_else(|| adjacent.filter(|p| p.is_file()))
            .unwrap_or_else(|| PathBuf::from("openrigcue-automation.exe"));
        Self {
            runner: std::env::var_os("OPENRIGCUE_WINE").unwrap_or_else(|| "wine".into()),
            executable,
            prefix: std::env::var_os("WINEPREFIX"),
        }
    }
}
impl Helper {
    pub(super) fn session(&self) -> Result<Arc<dyn Provider>, String> {
        Ok(Arc::new(session::Session::start(self)?))
    }
}
impl Provider for Helper {
    fn request(&self, request: Request) -> Result<Response, String> {
        use std::io::{Read, Write};
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let mut command = Command::new(&self.runner);
        command.arg(&self.executable).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(prefix) = &self.prefix {
            command.env("WINEPREFIX", prefix);
        }
        let mut child = command.spawn().map_err(|e| format!("Desktop automation helper: {e}"))?;
        let input = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
        let write = child.stdin.take().ok_or("Automation input unavailable")?.write_all(&input);
        if let Err(error) = write {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.to_string());
        }
        let output = child.stdout.take().ok_or("Automation output unavailable")?;
        let (output_tx, output_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = output.take(1_048_577).read_to_end(&mut bytes).map(|_| bytes);
            let _ = output_tx.send(result);
        });
        // Drain Wine diagnostics separately so an unavailable provider cannot block its pipe.
        let diagnostics = child.stderr.take();
        std::thread::spawn(move || {
            if let Some(mut stderr) = diagnostics {
                let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            }
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("Desktop automation helper timed out".into());
                }
            }
        };
        let bytes = output_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| "Desktop automation output timed out")?
            .map_err(|e| e.to_string())?;
        if bytes.len() > 1_048_576 {
            return Err("Desktop automation response too large".into());
        }
        if !status.success() {
            return Err(format!("Desktop automation helper exited {status}"));
        }
        let response: Response = serde_json::from_slice(&bytes).map_err(|_| "Invalid desktop automation response")?;
        if let Some(error) = &response.error {
            Err(error.clone())
        } else {
            Ok(response)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn helper_transports_one_request_and_keeps_the_selected_prefix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("openrigcue-automation-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let runner = dir.join("runner");
        std::fs::write(&runner,"#!/bin/sh\ncat > \"$1.request\"\nprintf '%s' \"$WINEPREFIX\" > \"$1.prefix\"\nprintf '%s' '{\"found\":true,\"values\":[\"Window\"]}'\n").unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
        let executable = dir.join("fake-helper.exe");
        let helper = Helper {
            runner: runner.into_os_string(),
            executable: executable.clone(),
            prefix: Some("/selected prefix".into()),
        };
        let response = helper.request(Request::Titles).unwrap();
        assert_eq!(response.values, ["Window"]);
        let stored: Request =
            serde_json::from_slice(&std::fs::read(executable.with_extension("exe.request")).unwrap()).unwrap();
        assert_eq!(stored, Request::Titles);
        assert_eq!(std::fs::read_to_string(executable.with_extension("exe.prefix")).unwrap(), "/selected prefix");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
