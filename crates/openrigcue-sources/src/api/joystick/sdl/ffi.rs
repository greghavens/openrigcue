//! SDL2's stable C ABI, isolated behind the safe joystick backend.
//! Signatures/layouts follow the retained SDL 2.32.10 headers. The only other
//! unsafe code of this crate is the resolver call of `udp_racing::support::resolver`.
//! Loading a library does not initialize SDL; initialization and every joystick call
//! stay on the joystick polling worker.

#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr::NonNull;

use super::{Backend as BackendTrait, Device, Event};

const INIT_JOYSTICK: u32 = 0x200;

#[repr(C)]
#[derive(Clone, Copy)]
struct Guid {
    data: [u8; 16],
}

// SDL_Event's explicit ABI padding is 56 bytes on 32/64-bit platforms.
// Its pointer-containing variants require pointer alignment on x86_64.
#[repr(C, align(8))]
struct RawEvent {
    bytes: [u8; 56],
}

impl RawEvent {
    fn decode(&self) -> Event {
        let kind = u32::from_ne_bytes(self.bytes[..4].try_into().unwrap());
        let which = i32::from_ne_bytes(self.bytes[8..12].try_into().unwrap());
        match kind {
            0x600 => Event::Axis {
                instance: which,
                index: u32::from(self.bytes[12]),
                value: i32::from(i16::from_ne_bytes(self.bytes[16..18].try_into().unwrap())),
            },
            0x602 => Event::Hat { instance: which, index: u32::from(self.bytes[12]) },
            0x603 | 0x604 => {
                Event::Button { instance: which, index: u32::from(self.bytes[12]), pressed: kind == 0x603 }
            }
            0x605 => Event::Added { index: which },
            0x606 => Event::Removed { instance: which },
            _ => Event::Other { instance: which },
        }
    }
}

type Joystick = *mut c_void;

pub(super) struct Backend {
    // Function pointers never outlive the library owning their code.
    _library: libloading::Library,
    init: unsafe extern "C" fn(u32) -> c_int,
    quit: unsafe extern "C" fn(u32),
    count: unsafe extern "C" fn() -> c_int,
    open: unsafe extern "C" fn(c_int) -> Joystick,
    close: unsafe extern "C" fn(Joystick),
    guid: unsafe extern "C" fn(Joystick) -> Guid,
    guid_string: unsafe extern "C" fn(Guid, *mut c_char, c_int),
    name: unsafe extern "C" fn(Joystick) -> *const c_char,
    axes: unsafe extern "C" fn(Joystick) -> c_int,
    instance: unsafe extern "C" fn(Joystick) -> i32,
    initial_axis: unsafe extern "C" fn(Joystick, c_int, *mut i16) -> c_int,
    get_axis: unsafe extern "C" fn(Joystick, c_int) -> i16,
    poll: unsafe extern "C" fn(*mut RawEvent) -> c_int,
    handles: Vec<NonNull<c_void>>,
    initialized: bool,
}

impl Backend {
    pub(super) fn load() -> Result<Self, String> {
        // SDL2 has a stable ABI; SDL3 is intentionally not a candidate.
        let library = unsafe { libloading::Library::new("libSDL2-2.0.so.0") }.map_err(|error| error.to_string())?;
        unsafe {
            macro_rules! symbol {
                ($name:literal) => {
                    *library.get(concat!($name, "\0").as_bytes()).map_err(|error| error.to_string())?
                };
            }
            Ok(Self {
                init: symbol!("SDL_Init"),
                quit: symbol!("SDL_QuitSubSystem"),
                count: symbol!("SDL_NumJoysticks"),
                open: symbol!("SDL_JoystickOpen"),
                close: symbol!("SDL_JoystickClose"),
                guid: symbol!("SDL_JoystickGetGUID"),
                guid_string: symbol!("SDL_JoystickGetGUIDString"),
                name: symbol!("SDL_JoystickName"),
                axes: symbol!("SDL_JoystickNumAxes"),
                instance: symbol!("SDL_JoystickInstanceID"),
                initial_axis: symbol!("SDL_JoystickGetAxisInitialState"),
                get_axis: symbol!("SDL_JoystickGetAxis"),
                poll: symbol!("SDL_PollEvent"),
                _library: library,
                handles: Vec::new(),
                initialized: false,
            })
        }
    }

    pub(super) fn initialize(&mut self) {
        // A failed SDL_Init is recorded, not reported.
        self.initialized = unsafe { (self.init)(INIT_JOYSTICK) } == 0;
    }
}

impl BackendTrait for Backend {
    fn devices(&mut self) -> Result<Vec<Device>, String> {
        let mut devices = Vec::new();
        // SDL owns these handles and NUL-terminated strings. Every successful
        // Open is retained, including those of repeated hotplug reloads.
        unsafe {
            for index in 0..(self.count)() {
                let handle = NonNull::new((self.open)(index)).ok_or("SDL_JoystickOpen returned NULL")?;
                let slot = self.handles.len();
                self.handles.push(handle);
                let pointer = handle.as_ptr();
                let mut guid = [0 as c_char; 1024];
                (self.guid_string)((self.guid)(pointer), guid.as_mut_ptr(), 1024);
                let guid = CStr::from_ptr(guid.as_ptr()).to_str().map_err(|error| error.to_string())?.to_owned();
                let name = (self.name)(pointer);
                if name.is_null() {
                    return Err("SDL_JoystickName returned NULL".into());
                }
                let name = CStr::from_ptr(name).to_str().map_err(|error| error.to_string())?.to_owned();
                let axes = (self.axes)(pointer);
                let mut initial = BTreeMap::new();
                for axis in 0..axes {
                    let mut value = 0;
                    if (self.initial_axis)(pointer, axis, &mut value) != 0 {
                        initial.insert(axis as u32, i32::from(value));
                    }
                }
                devices.push(Device { guid, name, instance: (self.instance)(pointer), handle: slot, axes, initial });
            }
        }
        Ok(devices)
    }

    fn event(&mut self) -> Option<Event> {
        let mut raw = RawEvent { bytes: [0; 56] };
        // SDL_PollEvent writes one SDL_Event when its return value is nonzero.
        (unsafe { (self.poll)(&mut raw) } != 0).then(|| raw.decode())
    }

    fn axis(&self, device: &Device, index: u32) -> i32 {
        let handle = self.handles[device.handle];
        // Devices/indices originate from this backend's enumeration and axis count.
        i32::from(unsafe { (self.get_axis)(handle.as_ptr(), index as c_int) })
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        // Runs on the joystick polling worker, before the library unloads:
        // an explicit shutdown releases every handle.
        unsafe {
            for handle in self.handles.drain(..) {
                (self.close)(handle.as_ptr());
            }
            if self.initialized {
                (self.quit)(INIT_JOYSTICK);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "package symbol check only; loads SDL2 without initialization or device calls"]
    fn library_symbols_load_without_initializing_or_opening_devices() {
        let backend = Backend::load().unwrap();
        assert!(!backend.initialized);
        assert!(backend.handles.is_empty());
    }

    #[test]
    fn abi_and_signed_axis_event_follow_the_sdl_headers() {
        assert_eq!(std::mem::size_of::<RawEvent>(), 56);
        assert_eq!(std::mem::align_of::<RawEvent>(), 8);
        assert_eq!(std::mem::size_of::<Guid>(), 16);
        let mut raw = RawEvent { bytes: [0; 56] };
        raw.bytes[..4].copy_from_slice(&0x600_u32.to_ne_bytes());
        raw.bytes[8..12].copy_from_slice(&(-123_i32).to_ne_bytes());
        raw.bytes[12] = 7;
        raw.bytes[16..18].copy_from_slice(&i16::MIN.to_ne_bytes());
        assert_eq!(raw.decode(), Event::Axis { instance: -123, index: 7, value: -32768 });
    }
}
