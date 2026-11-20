use std::os::windows::io::{AsRawHandle, OwnedHandle};

use windows_sys::Win32::System::Threading::{OpenEventW, WaitForSingleObject};

use super::{sections, wide};
use crate::event::consumer::EventApi;

pub(super) struct NativeEvents;

impl EventApi for NativeEvents {
    type Handle = OwnedHandle;

    fn open(&mut self, name: &str, access: u32, inherit: bool) -> Option<OwnedHandle> {
        let name = wide(name);
        // SAFETY: the requested name is NUL-terminated and remains live for the
        // call; the successful handle is owned exactly once for this session.
        sections::owned(unsafe { OpenEventW(access, i32::from(inherit), name.as_ptr()) }).ok()
    }

    fn wait(&mut self, handle: &OwnedHandle, milliseconds: u32) -> u32 {
        // SAFETY: the session retains the handle for the entire call. Reset
        // behavior is defined by the game's event, never guessed by the bridge.
        unsafe { WaitForSingleObject(handle.as_raw_handle(), milliseconds) }
    }
}
