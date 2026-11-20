//! The FMOD Core and Studio C API (2.02), loaded at run time from the user's `libfmod.so` and
//! `libfmodstudio.so`. This is the only module of the crate that uses `unsafe`.
//!
//! Only the calls OpenRigCue makes are bound. Signatures and struct layouts are those of the
//! FMOD 2.02 headers (`fmod.h`, `fmod_dsp.h`, `fmod_studio.h`; plugin SDK version 110).
//! [`Api`] is a plain table of function pointers, so the tests fill it with fakes.
//!
//! [`Studio`] is the safe wrapper: it owns one FMOD Studio system set up the way OpenRigCue
//! uses it (non-real-time output, 5.1 software format, a capture DSP on the master channel
//! group that hands every mixed block to Rust).

#![allow(unsafe_code)]

use std::collections::VecDeque;
use std::ffi::{c_char, c_float, c_int, c_uint, c_void, CStr, CString};
use std::path::Path;
use std::ptr;
use std::sync::{Arc, Mutex, PoisonError};

use super::FmodError;

type Handle = *mut c_void;
type FmodResult = c_int;

const FMOD_OK: FmodResult = 0;
/// `FMOD_OUTPUTTYPE_NOSOUND_NRT`: no device, the mixer runs once per `update`.
const OUTPUT_NOSOUND_NRT: c_int = 4;
/// `FMOD_SPEAKERMODE_5POINT1`.
const SPEAKERMODE_5POINT1: c_int = 6;
/// `FMOD_STUDIO_INIT_SYNCHRONOUS_UPDATE`: Studio commands run inside `update`.
const STUDIO_INIT_SYNCHRONOUS_UPDATE: c_uint = 0x4;
/// `FMOD_INIT_NORMAL`.
const INIT_NORMAL: c_uint = 0;
/// `FMOD_STUDIO_LOAD_BANK_NORMAL`.
const LOAD_BANK_NORMAL: c_uint = 0;
/// `FMOD_STUDIO_LOAD_MEMORY`: FMOD copies the bank data.
const LOAD_MEMORY: c_int = 0;
/// `FMOD_CHANNELCONTROL_DSP_HEAD`: the output end of the master group's chain.
const DSP_HEAD: c_int = -1;
/// `FMOD_PLUGIN_SDK_VERSION` of FMOD 2.02.
const PLUGIN_SDK_VERSION: c_uint = 110;
/// FMOD's 5.1 channel order is FL, FR, C, LFE, SL, SR: the order of [`crate::shakers::Speaker::ALL`].
pub(super) const CHANNELS: usize = 6;
/// Header version handed to `FMOD_System_Create` for the version probe: the 2.02.10 SDK.
/// FMOD accepts any library of the same major.minor (2.02).
const HEADER_VERSION: c_uint = 0x0002_0210;

/// `FMOD_DSP_READ_CALLBACK`.
type ReadCallback = unsafe extern "C" fn(
    state: *mut DspState,
    inbuffer: *mut c_float,
    outbuffer: *mut c_float,
    length: c_uint,
    inchannels: c_int,
    outchannels: *mut c_int,
) -> FmodResult;

/// `FMOD_DSP_GETUSERDATA_FUNC`.
type GetUserData = unsafe extern "C" fn(state: *mut DspState, userdata: *mut Handle) -> FmodResult;

/// `FMOD_DSP_STATE`.
#[repr(C)]
pub(super) struct DspState {
    instance: Handle,
    plugindata: Handle,
    channelmask: c_uint,
    source_speakermode: c_int,
    sidechaindata: *mut c_float,
    sidechainchannels: c_int,
    functions: *const DspStateFunctions,
    systemobject: c_int,
}

/// `FMOD_DSP_STATE_FUNCTIONS`; only `getuserdata` is used, the others are opaque pointers.
#[repr(C)]
pub(super) struct DspStateFunctions {
    alloc: Handle,
    realloc: Handle,
    free: Handle,
    getsamplerate: Handle,
    getblocksize: Handle,
    dft: Handle,
    pan: Handle,
    getspeakermode: Handle,
    getclock: Handle,
    getlistenerattributes: Handle,
    log: Handle,
    getuserdata: Option<GetUserData>,
}

/// `FMOD_DSP_DESCRIPTION`; unused callbacks are null.
#[repr(C)]
pub(super) struct DspDescription {
    pluginsdkversion: c_uint,
    name: [c_char; 32],
    version: c_uint,
    numinputbuffers: c_int,
    numoutputbuffers: c_int,
    create: Handle,
    release: Handle,
    reset: Handle,
    read: Option<ReadCallback>,
    process: Handle,
    setposition: Handle,
    numparameters: c_int,
    paramdesc: Handle,
    setparameterfloat: Handle,
    setparameterint: Handle,
    setparameterbool: Handle,
    setparameterdata: Handle,
    getparameterfloat: Handle,
    getparameterint: Handle,
    getparameterbool: Handle,
    getparameterdata: Handle,
    shouldiprocess: Handle,
    userdata: Handle,
    sys_register: Handle,
    sys_deregister: Handle,
    sys_mix: Handle,
}

/// The bound FMOD functions (C names in the doc of each field).
#[derive(Clone, Copy)]
pub(super) struct Api {
    /// `FMOD_System_Create`
    pub system_create: unsafe extern "C" fn(*mut Handle, c_uint) -> FmodResult,
    /// `FMOD_System_GetVersion`
    pub system_get_version: unsafe extern "C" fn(Handle, *mut c_uint) -> FmodResult,
    /// `FMOD_System_Close`
    pub system_close: unsafe extern "C" fn(Handle) -> FmodResult,
    /// `FMOD_System_Release`
    pub system_release: unsafe extern "C" fn(Handle) -> FmodResult,
    /// `FMOD_System_Update`
    pub system_update: unsafe extern "C" fn(Handle) -> FmodResult,
    /// `FMOD_System_SetOutput`
    pub system_set_output: unsafe extern "C" fn(Handle, c_int) -> FmodResult,
    /// `FMOD_System_GetSoftwareFormat`
    pub system_get_software_format: unsafe extern "C" fn(Handle, *mut c_int, *mut c_int, *mut c_int) -> FmodResult,
    /// `FMOD_System_SetSoftwareFormat`
    pub system_set_software_format: unsafe extern "C" fn(Handle, c_int, c_int, c_int) -> FmodResult,
    /// `FMOD_System_SetDSPBufferSize`
    pub system_set_dsp_buffer_size: unsafe extern "C" fn(Handle, c_uint, c_int) -> FmodResult,
    /// `FMOD_System_CreateDSP`
    pub system_create_dsp: unsafe extern "C" fn(Handle, *const DspDescription, *mut Handle) -> FmodResult,
    /// `FMOD_System_GetMasterChannelGroup`
    pub system_get_master_channel_group: unsafe extern "C" fn(Handle, *mut Handle) -> FmodResult,
    /// `FMOD_ChannelGroup_AddDSP`
    pub channel_group_add_dsp: unsafe extern "C" fn(Handle, c_int, Handle) -> FmodResult,
    /// `FMOD_ChannelGroup_RemoveDSP`
    pub channel_group_remove_dsp: unsafe extern "C" fn(Handle, Handle) -> FmodResult,
    /// `FMOD_DSP_Release`
    pub dsp_release: unsafe extern "C" fn(Handle) -> FmodResult,
    /// `FMOD_Studio_System_Create`
    pub studio_create: unsafe extern "C" fn(*mut Handle, c_uint) -> FmodResult,
    /// `FMOD_Studio_System_GetCoreSystem`
    pub studio_get_core_system: unsafe extern "C" fn(Handle, *mut Handle) -> FmodResult,
    /// `FMOD_Studio_System_Initialize`
    pub studio_initialize: unsafe extern "C" fn(Handle, c_int, c_uint, c_uint, Handle) -> FmodResult,
    /// `FMOD_Studio_System_LoadBankMemory`
    pub studio_load_bank_memory:
        unsafe extern "C" fn(Handle, *const c_char, c_int, c_int, c_uint, *mut Handle) -> FmodResult,
    /// `FMOD_Studio_System_GetEvent`
    pub studio_get_event: unsafe extern "C" fn(Handle, *const c_char, *mut Handle) -> FmodResult,
    /// `FMOD_Studio_System_Update`
    pub studio_update: unsafe extern "C" fn(Handle) -> FmodResult,
    /// `FMOD_Studio_System_Release`
    pub studio_release: unsafe extern "C" fn(Handle) -> FmodResult,
    /// `FMOD_Studio_EventDescription_CreateInstance`
    pub description_create_instance: unsafe extern "C" fn(Handle, *mut Handle) -> FmodResult,
    /// `FMOD_Studio_EventInstance_Start`
    pub instance_start: unsafe extern "C" fn(Handle) -> FmodResult,
    /// `FMOD_Studio_EventInstance_SetPaused`
    pub instance_set_paused: unsafe extern "C" fn(Handle, c_int) -> FmodResult,
    /// `FMOD_Studio_EventInstance_GetPaused`
    pub instance_get_paused: unsafe extern "C" fn(Handle, *mut c_int) -> FmodResult,
    /// `FMOD_Studio_EventInstance_SetParameterByName`
    pub instance_set_parameter_by_name: unsafe extern "C" fn(Handle, *const c_char, c_float, c_int) -> FmodResult,
}

/// The loaded FMOD libraries and their function table.
pub struct FmodLibrary {
    api: Api,
    /// Kept open for as long as [`Api`] may be called (core first, then Studio).
    _libraries: Vec<libloading::Library>,
}

impl std::fmt::Debug for FmodLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FmodLibrary").finish_non_exhaustive()
    }
}

/// File names tried for the core and the Studio library (FMOD 2.02 is soname 13).
const CORE_NAMES: [&str; 2] = ["libfmod.so.13", "libfmod.so"];
const STUDIO_NAMES: [&str; 2] = ["libfmodstudio.so.13", "libfmodstudio.so"];

impl FmodLibrary {
    /// Load `libfmod.so` and `libfmodstudio.so` from `dir`, or through the dynamic linker's
    /// search path when `dir` is `None`.
    pub fn load(dir: Option<&Path>) -> Result<Arc<FmodLibrary>, FmodError> {
        // The core library is loaded first and globally, so the Studio library's own
        // dependency on `libfmod.so.13` resolves to it.
        let core = open_first(dir, &CORE_NAMES)?;
        let studio = open_first(dir, &STUDIO_NAMES)?;
        let api = Api {
            system_create: symbol(&core, "FMOD_System_Create")?,
            system_get_version: symbol(&core, "FMOD_System_GetVersion")?,
            system_close: symbol(&core, "FMOD_System_Close")?,
            system_release: symbol(&core, "FMOD_System_Release")?,
            system_update: symbol(&core, "FMOD_System_Update")?,
            system_set_output: symbol(&core, "FMOD_System_SetOutput")?,
            system_get_software_format: symbol(&core, "FMOD_System_GetSoftwareFormat")?,
            system_set_software_format: symbol(&core, "FMOD_System_SetSoftwareFormat")?,
            system_set_dsp_buffer_size: symbol(&core, "FMOD_System_SetDSPBufferSize")?,
            system_create_dsp: symbol(&core, "FMOD_System_CreateDSP")?,
            system_get_master_channel_group: symbol(&core, "FMOD_System_GetMasterChannelGroup")?,
            channel_group_add_dsp: symbol(&core, "FMOD_ChannelGroup_AddDSP")?,
            channel_group_remove_dsp: symbol(&core, "FMOD_ChannelGroup_RemoveDSP")?,
            dsp_release: symbol(&core, "FMOD_DSP_Release")?,
            studio_create: symbol(&studio, "FMOD_Studio_System_Create")?,
            studio_get_core_system: symbol(&studio, "FMOD_Studio_System_GetCoreSystem")?,
            studio_initialize: symbol(&studio, "FMOD_Studio_System_Initialize")?,
            studio_load_bank_memory: symbol(&studio, "FMOD_Studio_System_LoadBankMemory")?,
            studio_get_event: symbol(&studio, "FMOD_Studio_System_GetEvent")?,
            studio_update: symbol(&studio, "FMOD_Studio_System_Update")?,
            studio_release: symbol(&studio, "FMOD_Studio_System_Release")?,
            description_create_instance: symbol(&studio, "FMOD_Studio_EventDescription_CreateInstance")?,
            instance_start: symbol(&studio, "FMOD_Studio_EventInstance_Start")?,
            instance_set_paused: symbol(&studio, "FMOD_Studio_EventInstance_SetPaused")?,
            instance_get_paused: symbol(&studio, "FMOD_Studio_EventInstance_GetPaused")?,
            instance_set_parameter_by_name: symbol(&studio, "FMOD_Studio_EventInstance_SetParameterByName")?,
        };
        Ok(Arc::new(FmodLibrary { api, _libraries: vec![core, studio] }))
    }

    /// A library whose functions are `api` (the tests' fakes).
    #[cfg(test)]
    pub(super) fn from_api(api: Api) -> Arc<FmodLibrary> {
        Arc::new(FmodLibrary { api, _libraries: Vec::new() })
    }

    /// The loaded runtime's version (`System::getVersion` of a throw-away core system).
    #[cfg(test)]
    pub(super) fn version(&self) -> Result<c_uint, FmodError> {
        probe_version(&self.api)
    }
}

fn open_first(dir: Option<&Path>, names: &[&str]) -> Result<libloading::Library, FmodError> {
    let mut last = String::new();
    for name in names {
        let path = dir.map_or_else(|| name.into(), |d| d.join(name));
        // SAFETY: loading a shared library runs its initialisers. These are the FMOD libraries
        // the user put in an FMOD search location; nothing else is loaded from there.
        let opened = unsafe {
            libloading::os::unix::Library::open(
                Some(&path),
                libloading::os::unix::RTLD_NOW | libloading::os::unix::RTLD_GLOBAL,
            )
        };
        match opened {
            Ok(lib) => return Ok(lib.into()),
            Err(e) => last = format!("{}: {e}", path.display()),
        }
    }
    Err(FmodError::Library(last))
}

fn symbol<T: Copy>(lib: &libloading::Library, name: &str) -> Result<T, FmodError> {
    let c_name = CString::new(name).expect("symbol names have no NUL");
    // SAFETY: every `T` requested here is the `unsafe extern "C" fn` type of the FMOD 2.02
    // function `name`, as declared in the field docs of `Api`. The pointer is used only while
    // `FmodLibrary` keeps `lib` loaded.
    unsafe {
        lib.get::<T>(c_name.as_bytes_with_nul()).map(|s| *s).map_err(|e| FmodError::Library(format!("{name}: {e}")))
    }
}

/// Mixed 5.1 frames handed over by the capture DSP; the DSP's user data points at it.
#[derive(Default)]
struct Capture {
    frames: Mutex<VecDeque<[f32; CHANNELS]>>,
}

/// Blocks kept before the oldest are dropped (only reached if nobody renders).
const CAPTURE_LIMIT_BLOCKS: usize = 8;

impl Capture {
    fn push(&self, input: &[f32], channels: usize, block: usize) {
        let mut frames = self.frames.lock().unwrap_or_else(PoisonError::into_inner);
        for frame in input.chunks_exact(channels.max(1)) {
            let mut out = [0.0; CHANNELS];
            for (o, v) in out.iter_mut().zip(frame) {
                *o = *v;
            }
            frames.push_back(out);
        }
        let limit = CAPTURE_LIMIT_BLOCKS * block.max(1);
        while frames.len() > limit {
            frames.pop_front();
        }
    }
}

/// The capture DSP's read callback: passes the master mix through and keeps a copy.
///
/// # Safety
/// Called by FMOD with a valid `state` and buffers of `length * inchannels` and
/// `length * *outchannels` floats; the DSP's user data is the `Capture` owned by the `Studio`
/// that created the DSP, which outlives it.
unsafe extern "C" fn capture_read(
    state: *mut DspState,
    inbuffer: *mut c_float,
    outbuffer: *mut c_float,
    length: c_uint,
    inchannels: c_int,
    outchannels: *mut c_int,
) -> FmodResult {
    let len = length as usize;
    let in_ch = usize::try_from(inchannels).unwrap_or(0);
    let out_ch = if outchannels.is_null() {
        in_ch
    } else {
        // SAFETY: FMOD passes a valid pointer to the output channel count.
        usize::try_from(unsafe { *outchannels }).unwrap_or(0)
    };
    if inbuffer.is_null() || in_ch == 0 {
        return FMOD_OK;
    }
    // SAFETY: FMOD's input buffer holds `length * inchannels` floats.
    let input = unsafe { std::slice::from_raw_parts(inbuffer, len * in_ch) };
    if !outbuffer.is_null() && out_ch > 0 {
        // SAFETY: FMOD's output buffer holds `length * outchannels` floats.
        let output = unsafe { std::slice::from_raw_parts_mut(outbuffer, len * out_ch) };
        for (o, i) in output.chunks_exact_mut(out_ch).zip(input.chunks_exact(in_ch)) {
            for (k, v) in o.iter_mut().enumerate() {
                *v = i.get(k).copied().unwrap_or(0.0);
            }
        }
    }
    // SAFETY: `state` is valid for the call; `functions` is FMOD's table (null-checked).
    let get_user_data = unsafe {
        if state.is_null() || (*state).functions.is_null() {
            None
        } else {
            (*(*state).functions).getuserdata
        }
    };
    let Some(get_user_data) = get_user_data else {
        return FMOD_OK;
    };
    let mut user: Handle = ptr::null_mut();
    // SAFETY: FMOD's own accessor for the user data given in the DSP description.
    if unsafe { get_user_data(state, &mut user) } != FMOD_OK || user.is_null() {
        return FMOD_OK;
    }
    // SAFETY: the user data is the `Capture` boxed in the `Studio` that owns this DSP; the DSP
    // is removed and released before that box is dropped. `Capture` is `Sync`.
    let capture = unsafe { &*(user as *const Capture) };
    capture.push(input, in_ch, len);
    FMOD_OK
}

/// An event instance of a [`Studio`] (index into its instance list).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct InstanceId(usize);

/// One FMOD Studio system: non-real-time output, 5.1, master mix captured for Rust.
pub(super) struct Studio {
    lib: Arc<FmodLibrary>,
    studio: Handle,
    core: Handle,
    master: Handle,
    dsp: Handle,
    instances: Vec<Handle>,
    /// Kept alive while the DSP may read them.
    _description: Box<DspDescription>,
    capture: Box<Capture>,
    block: usize,
}

// SAFETY: the handles are FMOD objects, which FMOD lets any thread use (the system is created
// without `FMOD_INIT_THREAD_UNSAFE`); `Studio` is only reached through `&mut self` (in the mixer,
// behind its mutex), so its calls never overlap, and `release` happens in `drop`. `Capture` is
// `Sync`.
unsafe impl Send for Studio {}

fn check(call: &'static str, code: FmodResult) -> Result<(), FmodError> {
    if code == FMOD_OK {
        Ok(())
    } else {
        Err(FmodError::Call { call, code })
    }
}

impl Studio {
    /// A Studio system mixing 5.1 at `rate` in blocks of `block` frames.
    ///
    /// A throw-away core system gives the library version, which is passed as the header
    /// version; then the Studio system, its core system, the 5.1 software format and
    /// initialisation with 1024 channels, with
    /// non-real-time output, the rate, the block size, synchronous updates and the capture DSP.
    pub(super) fn create(lib: Arc<FmodLibrary>, rate: u32, block: u32) -> Result<Studio, FmodError> {
        let api = lib.api;
        let version = probe_version(&api)?;
        let mut studio = Studio {
            lib,
            studio: ptr::null_mut(),
            core: ptr::null_mut(),
            master: ptr::null_mut(),
            dsp: ptr::null_mut(),
            instances: Vec::new(),
            _description: Box::new(capture_description()),
            capture: Box::default(),
            block: block as usize,
        };
        let rate = c_int::try_from(rate).map_err(|_| FmodError::Rate(rate))?;
        // SAFETY: every call gets live handles created above it (Drop releases on error), out
        // pointers to locals, and the constants of the FMOD 2.02 headers.
        unsafe {
            check("FMOD_Studio_System_Create", (api.studio_create)(&mut studio.studio, version))?;
            check("FMOD_Studio_System_GetCoreSystem", (api.studio_get_core_system)(studio.studio, &mut studio.core))?;
            check("FMOD_System_SetOutput", (api.system_set_output)(studio.core, OUTPUT_NOSOUND_NRT))?;
            let (mut current_rate, mut mode, mut raw) = (0, 0, 0);
            check(
                "FMOD_System_GetSoftwareFormat",
                (api.system_get_software_format)(studio.core, &mut current_rate, &mut mode, &mut raw),
            )?;
            check(
                "FMOD_System_SetSoftwareFormat",
                (api.system_set_software_format)(studio.core, rate, SPEAKERMODE_5POINT1, raw),
            )?;
            check("FMOD_System_SetDSPBufferSize", (api.system_set_dsp_buffer_size)(studio.core, block, 4))?;
            check(
                "FMOD_Studio_System_Initialize",
                (api.studio_initialize)(
                    studio.studio,
                    1024,
                    STUDIO_INIT_SYNCHRONOUS_UPDATE,
                    INIT_NORMAL,
                    ptr::null_mut(),
                ),
            )?;
            studio._description.userdata = &*studio.capture as *const Capture as Handle;
            check(
                "FMOD_System_CreateDSP",
                (api.system_create_dsp)(studio.core, &*studio._description, &mut studio.dsp),
            )?;
            check(
                "FMOD_System_GetMasterChannelGroup",
                (api.system_get_master_channel_group)(studio.core, &mut studio.master),
            )?;
            check("FMOD_ChannelGroup_AddDSP", (api.channel_group_add_dsp)(studio.master, DSP_HEAD, studio.dsp))?;
        }
        Ok(studio)
    }

    /// Load a bank into the Studio system from memory (FMOD copies the data).
    pub(super) fn load_bank(&mut self, data: &[u8]) -> Result<(), FmodError> {
        let len = c_int::try_from(data.len()).map_err(|_| FmodError::BankTooLarge(data.len()))?;
        let mut bank: Handle = ptr::null_mut();
        // SAFETY: `data` is valid for `len` bytes during the call (LOAD_MEMORY copies it).
        let code = unsafe {
            (self.lib.api.studio_load_bank_memory)(
                self.studio,
                data.as_ptr().cast(),
                len,
                LOAD_MEMORY,
                LOAD_BANK_NORMAL,
                &mut bank,
            )
        };
        check("FMOD_Studio_System_LoadBankMemory", code)
    }

    /// Look the event at `path` up and create an instance of it.
    pub(super) fn event_instance(&mut self, path: &CStr) -> Result<InstanceId, FmodError> {
        let mut description: Handle = ptr::null_mut();
        let mut instance: Handle = ptr::null_mut();
        // SAFETY: live Studio handle, NUL-terminated path, out pointers to locals; the
        // description comes from the call before.
        unsafe {
            check(
                "FMOD_Studio_System_GetEvent",
                (self.lib.api.studio_get_event)(self.studio, path.as_ptr(), &mut description),
            )?;
            check(
                "FMOD_Studio_EventDescription_CreateInstance",
                (self.lib.api.description_create_instance)(description, &mut instance),
            )?;
        }
        self.instances.push(instance);
        Ok(InstanceId(self.instances.len() - 1))
    }

    /// Start the instance.
    pub(super) fn start(&mut self, id: InstanceId) -> Result<(), FmodError> {
        // SAFETY: an instance created by this system.
        check("FMOD_Studio_EventInstance_Start", unsafe { (self.lib.api.instance_start)(self.instances[id.0]) })
    }

    /// Pause or un-pause the instance.
    pub(super) fn set_paused(&mut self, id: InstanceId, paused: bool) -> Result<(), FmodError> {
        // SAFETY: an instance created by this system.
        check("FMOD_Studio_EventInstance_SetPaused", unsafe {
            (self.lib.api.instance_set_paused)(self.instances[id.0], c_int::from(paused))
        })
    }

    /// Whether the instance is paused, as FMOD reports it.
    pub(super) fn paused(&mut self, id: InstanceId) -> Result<bool, FmodError> {
        let mut paused: c_int = 0;
        // SAFETY: an instance created by this system; out pointer to a local.
        check("FMOD_Studio_EventInstance_GetPaused", unsafe {
            (self.lib.api.instance_get_paused)(self.instances[id.0], &mut paused)
        })?;
        Ok(paused != 0)
    }

    /// Set the instance's parameter `name` to `value`, not ignoring the seek speed.
    pub(super) fn set_parameter(&mut self, id: InstanceId, name: &CStr, value: f32) -> Result<(), FmodError> {
        // SAFETY: an instance created by this system; NUL-terminated name.
        check("FMOD_Studio_EventInstance_SetParameterByName", unsafe {
            (self.lib.api.instance_set_parameter_by_name)(self.instances[id.0], name.as_ptr(), value, 0)
        })
    }

    /// The Studio update: runs the queued commands and mixes one block, which the
    /// capture DSP stores.
    ///
    /// The setters bound here validate on the calling
    /// thread and append a command to the system's buffer; a command runs at
    /// once only when issued from inside Studio's own processing on its processing thread. The
    /// synchronous-update flag does not change that, so the buffer runs only here.
    pub(super) fn update(&mut self) -> Result<(), FmodError> {
        // SAFETY: live Studio handle.
        check("FMOD_Studio_System_Update", unsafe { (self.lib.api.studio_update)(self.studio) })
    }

    /// The core system's `update()`: with the non-real-time output it mixes one block from the
    /// commands Studio has already run, leaving the Studio buffer untouched. This is
    /// the rendering that goes on between two submissions.
    pub(super) fn mix_core_block(&mut self) -> Result<(), FmodError> {
        // SAFETY: live core handle owned by this Studio system.
        check("FMOD_System_Update", unsafe { (self.lib.api.system_update)(self.core) })
    }

    /// The oldest captured 5.1 frame.
    pub(super) fn pop_frame(&mut self) -> Option<[f32; CHANNELS]> {
        self.capture.frames.lock().unwrap_or_else(PoisonError::into_inner).pop_front()
    }

    /// Captured frames not yet handed out.
    #[cfg(test)]
    pub(super) fn captured(&self) -> usize {
        self.capture.frames.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    /// Frames mixed per update.
    pub(super) fn block(&self) -> usize {
        self.block
    }
}

impl Drop for Studio {
    fn drop(&mut self) {
        let api = self.lib.api;
        // SAFETY: each handle is released once, only if it was created; the DSP leaves the
        // graph before it is released, and before `capture` is dropped (after this body).
        unsafe {
            if !self.dsp.is_null() {
                if !self.master.is_null() {
                    (api.channel_group_remove_dsp)(self.master, self.dsp);
                }
                (api.dsp_release)(self.dsp);
            }
            if !self.studio.is_null() {
                (api.studio_release)(self.studio);
            }
        }
    }
}

/// The library version, asked of a throw-away core system.
fn probe_version(api: &Api) -> Result<c_uint, FmodError> {
    let mut system: Handle = ptr::null_mut();
    let mut version: c_uint = 0;
    // SAFETY: the system is created, queried, closed and released here and nowhere else.
    unsafe {
        check("FMOD_System_Create", (api.system_create)(&mut system, HEADER_VERSION))?;
        let got = (api.system_get_version)(system, &mut version);
        (api.system_close)(system);
        (api.system_release)(system);
        check("FMOD_System_GetVersion", got)?;
    }
    Ok(version)
}

fn capture_description() -> DspDescription {
    let mut name = [0 as c_char; 32];
    for (n, b) in name.iter_mut().zip(b"OpenRigCue capture") {
        *n = *b as c_char;
    }
    DspDescription {
        pluginsdkversion: PLUGIN_SDK_VERSION,
        name,
        version: 1,
        numinputbuffers: 1,
        numoutputbuffers: 1,
        create: ptr::null_mut(),
        release: ptr::null_mut(),
        reset: ptr::null_mut(),
        read: Some(capture_read),
        process: ptr::null_mut(),
        setposition: ptr::null_mut(),
        numparameters: 0,
        paramdesc: ptr::null_mut(),
        setparameterfloat: ptr::null_mut(),
        setparameterint: ptr::null_mut(),
        setparameterbool: ptr::null_mut(),
        setparameterdata: ptr::null_mut(),
        getparameterfloat: ptr::null_mut(),
        getparameterint: ptr::null_mut(),
        getparameterbool: ptr::null_mut(),
        getparameterdata: ptr::null_mut(),
        shouldiprocess: ptr::null_mut(),
        userdata: ptr::null_mut(),
        sys_register: ptr::null_mut(),
        sys_deregister: ptr::null_mut(),
        sys_mix: ptr::null_mut(),
    }
}

/// A fake FMOD for the tests: a function table that records every call, mixes blocks whose
/// six channels are the sums of the unpaused instances' `fl`..`sr` parameters, and drives the
/// capture DSP through its read callback and `getuserdata`, as FMOD does.
#[cfg(test)]
pub(super) mod fake {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::*;

    /// Parameter names whose values become the six channels of a fake block.
    const SPEAKER_PARAMS: [&str; CHANNELS] = ["fl", "fr", "ce", "sub", "sl", "sr"];
    pub const EVENTS: [&str; 2] = ["event:/Vehicles/Car Engine 1", "event:/Vehicles/Car Engine 2"];

    #[derive(Default)]
    pub struct State {
        pub calls: Vec<String>,
        /// A call (C name) that returns an error.
        pub fail: Option<&'static str>,
        /// Fail only this Studio parameter, retaining writes before its setter.
        pub fail_parameter: Option<&'static str>,
        pub banks: Vec<usize>,
        pub version_asked: Option<c_uint>,
        pub studio_version: Option<c_uint>,
        pub format: Option<(c_int, c_int, c_int)>,
        pub output: Option<c_int>,
        pub block: c_uint,
        pub init: Option<(c_int, c_uint, c_uint)>,
        dsp: Option<(ReadCallback, Handle)>,
        pub dsp_added: Option<c_int>,
        pub released: Vec<&'static str>,
        /// Values the Studio processing has run (what a mixed block hears).
        params: HashMap<(usize, String), f32>,
        paused: HashMap<usize, bool>,
        /// The API-side `paused` copy that `setPaused` writes and `getPaused` reads at once
        /// (written when the command is accepted, before it runs).
        api_paused: HashMap<usize, bool>,
        /// Validated commands waiting for `FMOD_Studio_System_Update`.
        queue: Vec<Queued>,
        started: Vec<usize>,
    }

    /// A validated Studio command in the system's buffer.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Queued {
        Paused(usize, bool),
        Parameter(usize, String, f32),
    }

    /// The parameters the engine events are driven with; any other name is `FMOD_ERR_EVENT_NOTFOUND`.
    const KNOWN_PARAMS: [&str; 9] = ["rpm", "load", "fl", "fr", "ce", "sub", "sl", "sr", "pitch"];

    thread_local! {
        pub static STATE: RefCell<State> = RefCell::new(State::default());
    }

    pub fn reset() {
        STATE.with(|s| *s.borrow_mut() = State::default());
    }

    pub fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
        STATE.with(|s| f(&mut s.borrow_mut()))
    }

    pub fn calls() -> Vec<String> {
        with(|s| s.calls.clone())
    }

    /// The parameter of instance `i` (0 = `Car Engine 1`, 1 = `Car Engine 2`) as Studio has run it.
    pub fn param(i: usize, name: &str) -> Option<f32> {
        with(|s| s.params.get(&(i, name.to_string())).copied())
    }

    /// Whether Studio has run a pause of instance `i`.
    pub fn is_paused(i: usize) -> bool {
        with(|s| s.paused.get(&i).copied().unwrap_or(false))
    }

    /// The API-side paused copy `getPaused` returns.
    pub fn api_paused(i: usize) -> bool {
        with(|s| s.api_paused.get(&i).copied().unwrap_or(false))
    }

    /// The commands waiting for the next Studio update.
    pub fn queued() -> Vec<Queued> {
        with(|s| s.queue.clone())
    }

    pub fn started() -> Vec<usize> {
        with(|s| s.started.clone())
    }

    fn record(call: &'static str, detail: String) -> FmodResult {
        with(|s| {
            s.calls.push(if detail.is_empty() { call.to_string() } else { format!("{call} {detail}") });
            if s.fail == Some(call) {
                30 // FMOD_ERR_INVALID_HANDLE
            } else {
                FMOD_OK
            }
        })
    }

    const CORE: usize = 0x100;
    const STUDIO: usize = 0x200;
    const DSP: usize = 0x300;
    const MASTER: usize = 0x400;
    const DESCRIPTION: usize = 0x500;
    const INSTANCE: usize = 0x600;

    fn h(v: usize) -> Handle {
        v as Handle
    }

    unsafe extern "C" fn system_create(out: *mut Handle, version: c_uint) -> FmodResult {
        with(|s| s.version_asked = Some(version));
        unsafe { *out = h(CORE + 1) };
        record("FMOD_System_Create", String::new())
    }
    unsafe extern "C" fn system_get_version(_: Handle, out: *mut c_uint) -> FmodResult {
        unsafe { *out = 0x0002_0210 };
        record("FMOD_System_GetVersion", String::new())
    }
    unsafe extern "C" fn system_close(_: Handle) -> FmodResult {
        record("FMOD_System_Close", String::new())
    }
    unsafe extern "C" fn system_release(_: Handle) -> FmodResult {
        record("FMOD_System_Release", String::new())
    }
    unsafe extern "C" fn system_set_output(_: Handle, output: c_int) -> FmodResult {
        with(|s| s.output = Some(output));
        record("FMOD_System_SetOutput", output.to_string())
    }
    unsafe extern "C" fn system_get_software_format(
        _: Handle,
        rate: *mut c_int,
        mode: *mut c_int,
        raw: *mut c_int,
    ) -> FmodResult {
        unsafe {
            *rate = 48_000;
            *mode = 3; // stereo, the device's own format
            *raw = 0;
        }
        record("FMOD_System_GetSoftwareFormat", String::new())
    }
    unsafe extern "C" fn system_set_software_format(_: Handle, rate: c_int, mode: c_int, raw: c_int) -> FmodResult {
        with(|s| s.format = Some((rate, mode, raw)));
        record("FMOD_System_SetSoftwareFormat", format!("{rate} {mode} {raw}"))
    }
    unsafe extern "C" fn system_set_dsp_buffer_size(_: Handle, len: c_uint, _: c_int) -> FmodResult {
        with(|s| s.block = len);
        record("FMOD_System_SetDSPBufferSize", len.to_string())
    }
    unsafe extern "C" fn system_create_dsp(_: Handle, desc: *const DspDescription, out: *mut Handle) -> FmodResult {
        let (read, user, sdk) = unsafe { ((*desc).read, (*desc).userdata, (*desc).pluginsdkversion) };
        if let Some(read) = read {
            with(|s| s.dsp = Some((read, user)));
        }
        unsafe { *out = h(DSP) };
        record("FMOD_System_CreateDSP", format!("sdk {sdk}"))
    }
    unsafe extern "C" fn system_get_master_channel_group(_: Handle, out: *mut Handle) -> FmodResult {
        unsafe { *out = h(MASTER) };
        record("FMOD_System_GetMasterChannelGroup", String::new())
    }
    unsafe extern "C" fn channel_group_add_dsp(_: Handle, index: c_int, _: Handle) -> FmodResult {
        with(|s| s.dsp_added = Some(index));
        record("FMOD_ChannelGroup_AddDSP", index.to_string())
    }
    unsafe extern "C" fn channel_group_remove_dsp(_: Handle, _: Handle) -> FmodResult {
        with(|s| s.released.push("dsp-removed"));
        record("FMOD_ChannelGroup_RemoveDSP", String::new())
    }
    unsafe extern "C" fn dsp_release(_: Handle) -> FmodResult {
        with(|s| {
            s.released.push("dsp");
            s.dsp = None;
        });
        record("FMOD_DSP_Release", String::new())
    }
    unsafe extern "C" fn studio_create(out: *mut Handle, version: c_uint) -> FmodResult {
        with(|s| s.studio_version = Some(version));
        unsafe { *out = h(STUDIO) };
        record("FMOD_Studio_System_Create", String::new())
    }
    unsafe extern "C" fn studio_get_core_system(_: Handle, out: *mut Handle) -> FmodResult {
        unsafe { *out = h(CORE) };
        record("FMOD_Studio_System_GetCoreSystem", String::new())
    }
    unsafe extern "C" fn studio_initialize(
        _: Handle,
        max: c_int,
        studio_flags: c_uint,
        flags: c_uint,
        _: Handle,
    ) -> FmodResult {
        with(|s| s.init = Some((max, studio_flags, flags)));
        record("FMOD_Studio_System_Initialize", max.to_string())
    }
    unsafe extern "C" fn studio_load_bank_memory(
        _: Handle,
        _: *const c_char,
        len: c_int,
        mode: c_int,
        _: c_uint,
        out: *mut Handle,
    ) -> FmodResult {
        with(|s| s.banks.push(len as usize));
        unsafe { *out = h(0x700) };
        record("FMOD_Studio_System_LoadBankMemory", format!("{len} mode {mode}"))
    }
    unsafe extern "C" fn studio_get_event(_: Handle, path: *const c_char, out: *mut Handle) -> FmodResult {
        let path = unsafe { CStr::from_ptr(path) }.to_string_lossy().into_owned();
        let code = record("FMOD_Studio_System_GetEvent", path.clone());
        match EVENTS.iter().position(|e| *e == path) {
            Some(i) if code == FMOD_OK => {
                unsafe { *out = h(DESCRIPTION + i) };
                FMOD_OK
            }
            Some(_) => code,
            None => 74, // FMOD_ERR_EVENT_NOTFOUND
        }
    }
    /// `Studio::System::update`: the buffered commands run in order, then the core mixes one
    /// block. A failing update returns before the buffer is touched.
    unsafe extern "C" fn studio_update(_: Handle) -> FmodResult {
        let code = record("FMOD_Studio_System_Update", String::new());
        if code != FMOD_OK {
            return code;
        }
        with(|s| {
            for command in std::mem::take(&mut s.queue) {
                match command {
                    Queued::Paused(i, paused) => {
                        s.paused.insert(i, paused);
                    }
                    Queued::Parameter(i, name, value) => {
                        s.params.insert((i, name), value);
                    }
                }
            }
        });
        mix_block();
        code
    }
    /// The core `System::update` with the non-real-time output: one block of what Studio has
    /// already run; the Studio buffer stays as it is.
    unsafe extern "C" fn system_update(_: Handle) -> FmodResult {
        let code = record("FMOD_System_Update", String::new());
        if code == FMOD_OK {
            mix_block();
        }
        code
    }
    fn mix_block() {
        let (dsp, block, frame) = with(|s| {
            let mut frame = [0.0f32; CHANNELS];
            for i in 0..2 {
                if s.started.contains(&i) && !s.paused.get(&i).copied().unwrap_or(false) {
                    for (c, p) in SPEAKER_PARAMS.iter().enumerate() {
                        frame[c] += s.params.get(&(i, p.to_string())).copied().unwrap_or(0.0);
                    }
                }
            }
            (s.dsp, s.block as usize, frame)
        });
        if let Some((read, user)) = dsp {
            let mut input: Vec<f32> = frame.repeat(block);
            let mut output = vec![9.0f32; block * CHANNELS];
            let functions = DspStateFunctions {
                alloc: ptr::null_mut(),
                realloc: ptr::null_mut(),
                free: ptr::null_mut(),
                getsamplerate: ptr::null_mut(),
                getblocksize: ptr::null_mut(),
                dft: ptr::null_mut(),
                pan: ptr::null_mut(),
                getspeakermode: ptr::null_mut(),
                getclock: ptr::null_mut(),
                getlistenerattributes: ptr::null_mut(),
                log: ptr::null_mut(),
                getuserdata: Some(get_user_data),
            };
            let mut state = DspState {
                instance: h(DSP),
                plugindata: user,
                channelmask: 0,
                source_speakermode: SPEAKERMODE_5POINT1,
                sidechaindata: ptr::null_mut(),
                sidechainchannels: 0,
                functions: &functions,
                systemobject: 0,
            };
            let mut out_ch = CHANNELS as c_int;
            unsafe {
                read(
                    &mut state,
                    input.as_mut_ptr(),
                    output.as_mut_ptr(),
                    block as c_uint,
                    CHANNELS as c_int,
                    &mut out_ch,
                );
            }
            assert_eq!(input, output, "the capture DSP passes the mix through");
        }
    }
    /// The fake keeps the description's user data in `plugindata`.
    unsafe extern "C" fn get_user_data(state: *mut DspState, out: *mut Handle) -> FmodResult {
        unsafe { *out = (*state).plugindata };
        FMOD_OK
    }
    unsafe extern "C" fn studio_release(_: Handle) -> FmodResult {
        with(|s| s.released.push("studio"));
        record("FMOD_Studio_System_Release", String::new())
    }
    unsafe extern "C" fn description_create_instance(desc: Handle, out: *mut Handle) -> FmodResult {
        let i = desc as usize - DESCRIPTION;
        unsafe { *out = h(INSTANCE + i) };
        record("FMOD_Studio_EventDescription_CreateInstance", i.to_string())
    }
    unsafe extern "C" fn instance_start(inst: Handle) -> FmodResult {
        let i = inst as usize - INSTANCE;
        with(|s| s.started.push(i));
        record("FMOD_Studio_EventInstance_Start", i.to_string())
    }
    /// `setPaused`: accepting the command writes the API-side copy,
    /// then the command joins the buffer.
    unsafe extern "C" fn instance_set_paused(inst: Handle, paused: c_int) -> FmodResult {
        let i = inst as usize - INSTANCE;
        let code = record("FMOD_Studio_EventInstance_SetPaused", format!("{i} {paused}"));
        if code == FMOD_OK {
            with(|s| {
                s.api_paused.insert(i, paused != 0);
                s.queue.push(Queued::Paused(i, paused != 0));
            });
        }
        code
    }
    /// `getPaused` reads the API-side copy, so a pause set
    /// before the next update is already visible.
    unsafe extern "C" fn instance_get_paused(inst: Handle, out: *mut c_int) -> FmodResult {
        let i = inst as usize - INSTANCE;
        let p = with(|s| s.api_paused.get(&i).copied().unwrap_or(false));
        unsafe { *out = c_int::from(p) };
        record("FMOD_Studio_EventInstance_GetPaused", i.to_string())
    }
    /// `setParameterByName`: a null name
    /// or one of 128 bytes or more is `FMOD_ERR_INVALID_PARAM`, an infinite or NaN value
    /// `FMOD_ERR_INVALID_FLOAT`, an unknown name `FMOD_ERR_EVENT_NOTFOUND`; all at the call,
    /// before anything is buffered. The command runs at the next update.
    unsafe extern "C" fn instance_set_parameter_by_name(
        inst: Handle,
        name: *const c_char,
        value: c_float,
        ignore_seek_speed: c_int,
    ) -> FmodResult {
        let i = inst as usize - INSTANCE;
        let name = unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned();
        let mut code =
            record("FMOD_Studio_EventInstance_SetParameterByName", format!("{i} {name}={value} {ignore_seek_speed}"));
        if code != FMOD_OK {
            return code;
        }
        if name.len() >= 0x80 {
            return 31; // FMOD_ERR_INVALID_PARAM
        }
        if with(|s| s.fail_parameter == Some(name.as_str())) {
            code = 30;
        } else if value.to_bits() & 0x7f80_0000 == 0x7f80_0000 {
            code = 29; // FMOD_ERR_INVALID_FLOAT
        } else if !KNOWN_PARAMS.contains(&name.as_str()) {
            code = 74; // FMOD_ERR_EVENT_NOTFOUND
        }
        if code == FMOD_OK {
            with(|s| s.queue.push(Queued::Parameter(i, name, value)));
        }
        code
    }

    pub fn api() -> Api {
        Api {
            system_create,
            system_get_version,
            system_close,
            system_release,
            system_update,
            system_set_output,
            system_get_software_format,
            system_set_software_format,
            system_set_dsp_buffer_size,
            system_create_dsp,
            system_get_master_channel_group,
            channel_group_add_dsp,
            channel_group_remove_dsp,
            dsp_release,
            studio_create,
            studio_get_core_system,
            studio_initialize,
            studio_load_bank_memory,
            studio_get_event,
            studio_update,
            studio_release,
            description_create_instance,
            instance_start,
            instance_set_paused,
            instance_get_paused,
            instance_set_parameter_by_name,
        }
    }

    pub fn library() -> Arc<FmodLibrary> {
        reset();
        FmodLibrary::from_api(api())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The call order of [`Studio::create`]: version probe, the Studio system, its core system,
    /// software format 5.1, initialisation with 1024 channels; then the capture DSP at the head of the master group.
    #[test]
    fn reinit_studio_sequence() {
        let lib = fake::library();
        let studio = Studio::create(lib, 44_100, 256).unwrap();
        let calls = fake::calls();
        let names: Vec<&str> = calls.iter().map(|c| c.split(' ').next().unwrap()).collect();
        assert_eq!(
            names,
            [
                "FMOD_System_Create",
                "FMOD_System_GetVersion",
                "FMOD_System_Close",
                "FMOD_System_Release",
                "FMOD_Studio_System_Create",
                "FMOD_Studio_System_GetCoreSystem",
                "FMOD_System_SetOutput",
                "FMOD_System_GetSoftwareFormat",
                "FMOD_System_SetSoftwareFormat",
                "FMOD_System_SetDSPBufferSize",
                "FMOD_Studio_System_Initialize",
                "FMOD_System_CreateDSP",
                "FMOD_System_GetMasterChannelGroup",
                "FMOD_ChannelGroup_AddDSP",
            ]
        );
        fake::with(|s| {
            assert_eq!(s.version_asked, Some(HEADER_VERSION));
            assert_eq!(s.studio_version, Some(0x0002_0210));
            assert_eq!(s.output, Some(OUTPUT_NOSOUND_NRT));
            assert_eq!(s.format, Some((44_100, SPEAKERMODE_5POINT1, 0)));
            assert_eq!(s.init, Some((1024, STUDIO_INIT_SYNCHRONOUS_UPDATE, INIT_NORMAL)));
            assert_eq!(s.dsp_added, Some(DSP_HEAD));
        });
        assert!(calls.contains(&"FMOD_System_CreateDSP sdk 110".to_string()));
        assert_eq!(studio.block(), 256);
        drop(studio);
        fake::with(|s| assert_eq!(s.released, ["dsp-removed", "dsp", "studio"]));
    }

    #[test]
    fn capture_dsp_hands_each_mixed_block_over() {
        let lib = fake::library();
        let mut studio = Studio::create(lib, 48_000, 4).unwrap();
        let id = studio.event_instance(c"event:/Vehicles/Car Engine 1").unwrap();
        studio.start(id).unwrap();
        studio.set_parameter(id, c"fr", 0.5).unwrap();
        studio.set_parameter(id, c"sub", 2.0).unwrap();
        assert_eq!(studio.pop_frame(), None);
        studio.update().unwrap();
        for _ in 0..4 {
            assert_eq!(studio.pop_frame(), Some([0.0, 0.5, 0.0, 2.0, 0.0, 0.0]));
        }
        assert_eq!(studio.pop_frame(), None);
    }

    #[test]
    fn capture_keeps_a_bounded_backlog() {
        let lib = fake::library();
        let mut studio = Studio::create(lib, 48_000, 2).unwrap();
        for _ in 0..(CAPTURE_LIMIT_BLOCKS + 5) {
            studio.update().unwrap();
        }
        let mut n = 0;
        while studio.pop_frame().is_some() {
            n += 1;
        }
        assert_eq!(n, CAPTURE_LIMIT_BLOCKS * 2);
    }

    #[test]
    fn errors_name_the_call_and_release_what_was_made() {
        let lib = fake::library();
        fake::with(|s| s.fail = Some("FMOD_Studio_System_Initialize"));
        let err = Studio::create(lib, 48_000, 256).err().unwrap();
        assert!(matches!(err, FmodError::Call { call: "FMOD_Studio_System_Initialize", .. }));
        fake::with(|s| assert_eq!(s.released, ["studio"]));
    }

    #[test]
    fn unknown_event_is_an_error() {
        let lib = fake::library();
        let mut studio = Studio::create(lib, 48_000, 256).unwrap();
        assert!(matches!(studio.event_instance(c"event:/Vehicles/Nope"), Err(FmodError::Call { code: 74, .. })));
    }

    #[test]
    fn missing_library_is_reported() {
        let dir = std::env::temp_dir().join(format!("openrigcue-no-fmod-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = FmodLibrary::load(Some(&dir)).err().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(matches!(err, FmodError::Library(m) if m.contains("libfmod.so")));
    }

    #[test]
    fn struct_layouts_match_the_c_headers() {
        use std::mem::{offset_of, size_of};
        // x86-64 / aarch64 LP64 layouts of fmod_dsp.h (2.02).
        assert_eq!(offset_of!(DspDescription, create), 48);
        assert_eq!(offset_of!(DspDescription, read), 72);
        assert_eq!(offset_of!(DspDescription, numparameters), 96);
        assert_eq!(offset_of!(DspDescription, userdata), 184);
        assert_eq!(size_of::<DspDescription>(), 216);
        assert_eq!(offset_of!(DspState, functions), 40);
        assert_eq!(size_of::<DspState>(), 56);
        assert_eq!(offset_of!(DspStateFunctions, getuserdata), 88);
    }
}
