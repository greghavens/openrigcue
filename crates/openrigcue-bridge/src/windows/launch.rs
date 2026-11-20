//! Starting the game from the bridge and noticing when it, and everything it started, has exited.
//!
//! The bridge puts itself in a job object before starting the program, so every process the program starts
//! (launchers commonly start the real game and exit) belongs to the same job. The run is over when the direct
//! child has exited and the job holds no process but the bridge.

use std::io;
use std::mem;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::process::{Child, Command};
use std::ptr;

use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation, QueryInformationJobObject,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use crate::paths::program_path;

pub struct Launched {
    child: Child,
    job: Option<OwnedHandle>,
    exit_code: Option<i32>,
}

impl Launched {
    pub fn start(program: &str, args: &[String]) -> io::Result<Launched> {
        let job = join_new_job()
            .inspect_err(|error| tracing::warn!(%error, "no job object; only the direct child is waited for"))
            .ok();
        let program = program_path(program);
        tracing::info!(program, "starting");
        let child = Command::new(&program).args(args).spawn()?;
        Ok(Launched { child, job, exit_code: None })
    }

    /// The program's exit code once it and all processes it started have exited.
    pub fn finished(&mut self) -> Option<i32> {
        if self.exit_code.is_none() {
            match self.child.try_wait() {
                Ok(Some(status)) => self.exit_code = Some(status.code().unwrap_or(1)),
                Ok(None) => return None,
                Err(error) => {
                    tracing::warn!(%error, "cannot query the program");
                    self.exit_code = Some(1);
                }
            }
        }
        match &self.job {
            Some(job) if active_processes(job).is_some_and(|n| n > 1) => None,
            _ => self.exit_code,
        }
    }
}

/// Create an anonymous job object and put the bridge in it, so children inherit it.
fn join_new_job() -> io::Result<OwnedHandle> {
    // SAFETY: anonymous job, default security.
    let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: valid job handle just created and owned by nobody else.
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    // SAFETY: the pseudo handle of the current process and a live job handle.
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), GetCurrentProcess()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

/// Number of processes currently in `job` (including the bridge), if the job can be queried.
fn active_processes(job: &OwnedHandle) -> Option<u32> {
    // SAFETY: plain data filled in by the call below.
    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { mem::zeroed() };
    // SAFETY: `info` is writable for the size passed.
    let ok = unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicAccountingInformation,
            &mut info as *mut _ as *mut _,
            mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            ptr::null_mut(),
        )
    };
    (ok != 0).then_some(info.ActiveProcesses)
}
