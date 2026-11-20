//! One helper child per routine; its controls live until the last window drops.
use super::{Helper, Provider};
use openrigcue_model::automation::{Request, Response, SessionRequest};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

pub(super) struct Session {
    child: Mutex<Running>,
}
struct Running {
    child: Child,
    input: Option<ChildStdin>,
    output: mpsc::Receiver<std::io::Result<Vec<u8>>>,
    stopped: bool,
}
impl Session {
    pub(super) fn start(helper: &Helper) -> Result<Self, String> {
        let mut command = Command::new(&helper.runner);
        command
            .arg(&helper.executable)
            .arg("--session")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(prefix) = &helper.prefix {
            command.env("WINEPREFIX", prefix);
        }
        let mut child = command.spawn().map_err(|e| format!("Desktop automation helper: {e}"))?;
        let input = child.stdin.take();
        let stdout = child.stdout.take().ok_or("Routine automation output unavailable")?;
        let stderr = child.stderr.take();
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut output = BufReader::new(stdout);
            loop {
                let mut bytes = Vec::new();
                let read = output.by_ref().take(1_048_577).read_until(b'\n', &mut bytes);
                let stop = bytes.len() > 1_048_576;
                match read {
                    Ok(0) => break,
                    Ok(_) => {
                        if tx.send(Ok(bytes)).is_err() || stop {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Err(error));
                        break;
                    }
                }
            }
        });
        std::thread::spawn(move || {
            if let Some(mut stderr) = stderr {
                let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            }
        });
        Ok(Self { child: Mutex::new(Running { child, input, output: rx, stopped: false }) })
    }
}
impl Running {
    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.input.take();
        let end = Instant::now() + Duration::from_millis(500);
        while Instant::now() < end {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
    fn request(&mut self, request: SessionRequest) -> Result<Response, String> {
        if self.stopped {
            return Err("Routine automation session has ended".into());
        }
        let mut bytes = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
        if bytes.len() > 65_535 {
            return Err("Routine automation request too large".into());
        }
        bytes.push(b'\n');
        let result = (|| {
            self.input
                .as_mut()
                .ok_or("Routine automation input unavailable")?
                .write_all(&bytes)
                .map_err(|e| e.to_string())?;
            let bytes = self
                .output
                .recv_timeout(Duration::from_secs(10))
                .map_err(|_| "Routine automation helper timed out or exited")?
                .map_err(|e| e.to_string())?;
            if bytes.len() > 1_048_576 {
                return Err("Routine automation response too large".into());
            }
            serde_json::from_slice::<Response>(&bytes).map_err(|_| "Invalid routine automation response".into())
        })();
        match result {
            Ok(response) => match response.error {
                Some(error) => Err(error),
                None => Ok(response),
            },
            Err(error) => {
                self.stop();
                Err(error)
            }
        }
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.stop();
    }
}
impl Provider for Session {
    fn request(&self, _: Request) -> Result<Response, String> {
        Err("Routine control reference required".into())
    }
    fn control(&self, request: SessionRequest) -> Result<Response, String> {
        crate::host::lock(&self.child).request(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::screens::routines::desktop::UiaWindow;
    use std::os::unix::fs::PermissionsExt;
    fn helper(script: &str) -> (std::path::PathBuf, Helper) {
        let dir = std::env::temp_dir().join(format!(
            "openrigcue-routine-session-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let runner = dir.join("runner");
        std::fs::write(&runner, script).unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
        let helper = Helper {
            runner: runner.into_os_string(),
            executable: dir.join("helper"),
            prefix: Some("/isolated selected prefix".into()),
        };
        (dir, helper)
    }
    #[test]
    fn routine_session_keeps_distinct_lazy_controls_and_direct_children_until_drop() {
        let (dir, helper) = helper(
            r#"#!/bin/sh
printf '%s' "$2" > "$1.mode"
printf '%s' "$WINEPREFIX" > "$1.prefix"
next=0
while IFS= read -r request; do
 printf '%s\n' "$request" >> "$1.requests"
 case "$request" in
  *'"operation":"root"'*|*'"operation":"child"'*) printf '{"found":true,"controls":[%s]}\n' "$next"; next=$((next+1));;
  *'"action":"children"'*) printf '%s\n' '{"found":true,"controls":[40,41]}';;
  *'"control":40'*) printf '%s\n' '{"found":true,"values":["Chosen"]}';;
  *'"control":41'*) printf '%s\n' '{"found":true,"values":["Other"]}';;
 esac
done
printf done > "$1.closed"
"#,
        );
        let root = UiaWindow::cached_root(helper.session().unwrap(), "Fixture".into()).unwrap();
        let first = root.by_id("profiles", 5).unwrap();
        let independent = root.by_id("profiles", 5).unwrap();
        let children = first.children().unwrap();
        assert_eq!(children[0].name().unwrap(), "Chosen");
        assert_eq!(children[1].name().unwrap(), "Other");
        drop((root, first, independent, children));
        assert_eq!(std::fs::read_to_string(dir.join("helper.closed")).unwrap(), "done");
        assert_eq!(std::fs::read_to_string(dir.join("helper.mode")).unwrap(), "--session");
        assert_eq!(std::fs::read_to_string(dir.join("helper.prefix")).unwrap(), "/isolated selected prefix");
        let requests: Vec<SessionRequest> = std::fs::read_to_string(dir.join("helper.requests"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(matches!(&requests[1], SessionRequest::Child { parent: 0, .. }));
        assert!(matches!(&requests[2], SessionRequest::Child { parent: 0, .. }));
        assert!(matches!(&requests[3], SessionRequest::Execute { control: 1, .. }));
        assert!(matches!(&requests[4], SessionRequest::Execute { control: 40, .. }));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn routine_session_reports_native_failure_without_losing_other_cached_controls() {
        let (dir, helper) = helper(
            r#"#!/bin/sh
next=0
while IFS= read -r request; do
 case "$request" in
  *'"operation":"root"'*) printf '%s\n' '{"found":true,"controls":[0]}';;
  *) if [ "$next" = 0 ]; then printf '%s\n' '{"error":"COM failure"}'; next=1; else printf '%s\n' '{"found":true,"values":["Retained"]}'; fi;;
 esac
done
"#,
        );
        let root = UiaWindow::cached_root(helper.session().unwrap(), "Fixture".into()).unwrap();
        assert_eq!(root.name().unwrap_err(), "COM failure");
        assert_eq!(root.name().unwrap(), "Retained");
        drop(root);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn routine_session_drop_kills_and_reaps_only_its_unresponsive_helper() {
        let (dir, helper) = helper(
            r#"#!/bin/sh
printf '%s' "$$" > "$1.pid"
IFS= read -r request
printf '%s\n' '{"found":true,"controls":[0]}'
exec sleep 60
"#,
        );
        let root = UiaWindow::cached_root(helper.session().unwrap(), "Fixture".into()).unwrap();
        let pid = std::fs::read_to_string(dir.join("helper.pid")).unwrap();
        let started = Instant::now();
        drop(root);
        assert!(started.elapsed() < Duration::from_secs(2));
        let output = Command::new("kill").arg("-0").arg(pid).output().unwrap();
        assert!(!output.status.success(), "owned helper must be reaped");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
