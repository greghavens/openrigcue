//! End-to-end tests: the layer's real entry points, negotiated and created like the loader does,
//! over a fake runtime that tracks a head and a controller moved by a simulated rig.

use std::collections::{HashMap, VecDeque};
use std::ffi::{c_char, CStr};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use openrigcue_rigpose::math::{Pose, Quat, Vec3};
use openrigcue_rigpose::{
    monotonic_now_ns, ConsumerKind, CorOffset, RigPoseReader, RigPoseWriter, RigSample, RigValues,
};
use openxr_sys as xr;
use xr::{pfn, Handle};

use crate::compensation::Status;
use crate::config::Settings;
use crate::ffi::{test_access, TEST_SETTINGS};
use crate::motion::{pose_from_xr, pose_to_xr, vec_from_xr, Motion};

// ---------------------------------------------------------------------------------------------
// The fake runtime
// ---------------------------------------------------------------------------------------------

static SERIAL: Mutex<()> = Mutex::new(());
static WORLD: Mutex<Option<World>> = Mutex::new(None);
static HANDLES: AtomicU64 = AtomicU64::new(0x1000);

fn new_handle() -> u64 {
    HANDLES.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone, Copy)]
enum FakeSpace {
    Reference(xr::ReferenceSpaceType, Pose),
    Controller,
}

#[derive(Debug)]
enum FakeEvent {
    State(xr::Session, xr::SessionState),
    SpaceChange(xr::Session, xr::ReferenceSpaceType, i64),
}

#[derive(Debug, Clone)]
struct Submitted {
    ty: xr::StructureType,
    space: xr::Space,
    poses: Vec<Pose>,
}

#[derive(Debug, Default)]
struct World {
    spaces: HashMap<u64, FakeSpace>,
    room_from_local: Pose,
    /// room_from_head, as tracked (moved by the rig).
    head: Pose,
    /// Linear and angular velocity of the tracked head in the room.
    head_velocity: (Vec3, Vec3),
    controller: Pose,
    events: VecDeque<FakeEvent>,
    submitted: Vec<Vec<Submitted>>,
    enabled_extensions: Vec<String>,
    offer_timespec: bool,
    predicted: i64,
    /// `xrLocateViews` reports the views without valid orientation (tracking lost).
    views_untracked: bool,
}

fn world() -> MutexGuard<'static, Option<World>> {
    WORLD.lock().unwrap_or_else(|e| e.into_inner())
}

fn with_world<R>(f: impl FnOnce(&mut World) -> R) -> R {
    f(world().as_mut().expect("fake world set up"))
}

const EYE: f64 = 0.032;

impl World {
    fn room_from(&self, space: xr::Space) -> Pose {
        match self.spaces.get(&space.into_raw()).copied() {
            Some(FakeSpace::Reference(xr::ReferenceSpaceType::LOCAL, p)) => self.room_from_local.then(p),
            Some(FakeSpace::Reference(xr::ReferenceSpaceType::VIEW, p)) => self.head.then(p),
            Some(FakeSpace::Reference(_, p)) => p,
            Some(FakeSpace::Controller) => self.controller,
            None => panic!("unknown fake space {space:?}"),
        }
    }

    fn is_view(&self, space: xr::Space) -> bool {
        matches!(self.spaces.get(&space.into_raw()), Some(FakeSpace::Reference(xr::ReferenceSpaceType::VIEW, _)))
    }

    /// Location of `space` in `base`, and the velocity when `space` is the head in a static base.
    fn locate(&self, space: xr::Space, base: xr::Space) -> (Pose, Option<(Vec3, Vec3)>) {
        let base_pose = self.room_from(base);
        let pose = base_pose.inverse().then(self.room_from(space));
        let velocity = (self.is_view(space) && !self.is_view(base)).then(|| {
            let r = base_pose.rotation.conjugate();
            (r.rotate(self.head_velocity.0), r.rotate(self.head_velocity.1))
        });
        (pose, velocity)
    }
}

macro_rules! erase {
    ($f:expr, $ty:ty) => {{
        let f: $ty = $f;
        // SAFETY: type erasure as PFN_xrVoidFunction; the layer casts it back to `$ty`.
        Some(unsafe { std::mem::transmute::<$ty, pfn::VoidFunction>(f) })
    }};
}

unsafe extern "system" fn fake_gipa(
    _instance: xr::Instance,
    name: *const c_char,
    function: *mut Option<pfn::VoidFunction>,
) -> xr::Result {
    // SAFETY: the layer passes NUL-terminated names.
    let name = unsafe { CStr::from_ptr(name) }.to_bytes();
    let timespec = with_world(|w| w.enabled_extensions.iter().any(|e| e == "XR_KHR_convert_timespec_time"));
    let f = match name {
        b"xrEnumerateInstanceExtensionProperties" => {
            erase!(fake_enumerate_extensions, pfn::EnumerateInstanceExtensionProperties)
        }
        b"xrDestroyInstance" => erase!(fake_destroy_instance, pfn::DestroyInstance),
        b"xrGetInstanceProperties" => erase!(fake_instance_properties, pfn::GetInstanceProperties),
        b"xrPollEvent" => erase!(fake_poll_event, pfn::PollEvent),
        b"xrCreateSession" => erase!(fake_create_session, pfn::CreateSession),
        b"xrDestroySession" => erase!(fake_destroy_session, pfn::DestroySession),
        b"xrCreateReferenceSpace" => erase!(fake_create_reference_space, pfn::CreateReferenceSpace),
        b"xrCreateActionSpace" => erase!(fake_create_action_space, pfn::CreateActionSpace),
        b"xrDestroySpace" => erase!(fake_destroy_space, pfn::DestroySpace),
        b"xrLocateSpace" => erase!(fake_locate_space, pfn::LocateSpace),
        b"xrLocateSpaces" => erase!(fake_locate_spaces, pfn::LocateSpaces),
        b"xrLocateViews" => erase!(fake_locate_views, pfn::LocateViews),
        b"xrWaitFrame" => erase!(fake_wait_frame, pfn::WaitFrame),
        b"xrEndFrame" => erase!(fake_end_frame, pfn::EndFrame),
        b"xrConvertTimeToTimespecTimeKHR" if timespec => {
            erase!(fake_convert_time, pfn::ConvertTimeToTimespecTimeKHR)
        }
        _ => None,
    };
    // SAFETY: valid out-pointer from the layer.
    unsafe { *function = f };
    if f.is_some() {
        xr::Result::SUCCESS
    } else {
        xr::Result::ERROR_FUNCTION_UNSUPPORTED
    }
}

unsafe extern "system" fn fake_create_instance(
    info: *const xr::InstanceCreateInfo,
    _layer_info: *const xr::ApiLayerCreateInfo,
    instance: *mut xr::Instance,
) -> xr::Result {
    // SAFETY: the layer passes a valid create info with `enabled_extension_count` names.
    let info = unsafe { &*info };
    let names = (0..info.enabled_extension_count as usize)
        .map(|i| {
            // SAFETY: as above.
            let name = unsafe { CStr::from_ptr(*info.enabled_extension_names.add(i)) };
            name.to_string_lossy().into_owned()
        })
        .collect();
    with_world(|w| w.enabled_extensions = names);
    // SAFETY: valid out-pointer.
    unsafe { *instance = xr::Instance::from_raw(new_handle()) };
    xr::Result::SUCCESS
}

unsafe extern "system" fn fake_enumerate_extensions(
    _layer: *const c_char,
    capacity: u32,
    count: *mut u32,
    props: *mut xr::ExtensionProperties,
) -> xr::Result {
    let n = u32::from(with_world(|w| w.offer_timespec));
    // SAFETY: valid out-pointer.
    unsafe { *count = n };
    if capacity >= n && n == 1 {
        // SAFETY: the caller provides `capacity` elements.
        let p = unsafe { &mut *props };
        for (dst, src) in p.extension_name.iter_mut().zip(b"XR_KHR_convert_timespec_time\0") {
            *dst = *src as c_char;
        }
        p.extension_version = 1;
    }
    xr::Result::SUCCESS
}

unsafe extern "system" fn fake_destroy_instance(_instance: xr::Instance) -> xr::Result {
    xr::Result::SUCCESS
}

unsafe extern "system" fn fake_instance_properties(
    _instance: xr::Instance,
    props: *mut xr::InstanceProperties,
) -> xr::Result {
    // SAFETY: valid out-structure.
    let p = unsafe { &mut *props };
    for (dst, src) in p.runtime_name.iter_mut().zip(b"FakeXR\0") {
        *dst = *src as c_char;
    }
    xr::Result::SUCCESS
}

unsafe extern "system" fn fake_poll_event(_instance: xr::Instance, buffer: *mut xr::EventDataBuffer) -> xr::Result {
    let Some(ev) = with_world(|w| w.events.pop_front()) else {
        return xr::Result::EVENT_UNAVAILABLE;
    };
    match ev {
        FakeEvent::State(session, state) => {
            let e = xr::EventDataSessionStateChanged {
                ty: xr::StructureType::EVENT_DATA_SESSION_STATE_CHANGED,
                next: ptr::null(),
                session,
                state,
                time: xr::Time::from_nanos(0),
            };
            // SAFETY: the 4000-byte event buffer holds any event structure.
            unsafe { ptr::write(buffer as *mut xr::EventDataSessionStateChanged, e) };
        }
        FakeEvent::SpaceChange(session, ty, at) => {
            let e = xr::EventDataReferenceSpaceChangePending {
                ty: xr::StructureType::EVENT_DATA_REFERENCE_SPACE_CHANGE_PENDING,
                next: ptr::null(),
                session,
                reference_space_type: ty,
                change_time: xr::Time::from_nanos(at),
                pose_valid: xr::FALSE,
                pose_in_previous_space: pose_to_xr(Pose::IDENTITY),
            };
            // SAFETY: as above.
            unsafe { ptr::write(buffer as *mut xr::EventDataReferenceSpaceChangePending, e) };
        }
    }
    xr::Result::SUCCESS
}

unsafe extern "system" fn fake_create_session(
    _i: xr::Instance,
    _info: *const xr::SessionCreateInfo,
    session: *mut xr::Session,
) -> xr::Result {
    // SAFETY: valid out-pointer.
    unsafe { *session = xr::Session::from_raw(new_handle()) };
    xr::Result::SUCCESS
}

unsafe extern "system" fn fake_destroy_session(_session: xr::Session) -> xr::Result {
    xr::Result::SUCCESS
}

unsafe extern "system" fn fake_create_reference_space(
    _session: xr::Session,
    info: *const xr::ReferenceSpaceCreateInfo,
    space: *mut xr::Space,
) -> xr::Result {
    // SAFETY: valid create info.
    let info = unsafe { &*info };
    let h = new_handle();
    with_world(|w| {
        w.spaces.insert(h, FakeSpace::Reference(info.reference_space_type, pose_from_xr(&info.pose_in_reference_space)))
    });
    // SAFETY: valid out-pointer.
    unsafe { *space = xr::Space::from_raw(h) };
    xr::Result::SUCCESS
}

unsafe extern "system" fn fake_create_action_space(
    _session: xr::Session,
    _info: *const xr::ActionSpaceCreateInfo,
    space: *mut xr::Space,
) -> xr::Result {
    let h = new_handle();
    with_world(|w| w.spaces.insert(h, FakeSpace::Controller));
    // SAFETY: valid out-pointer.
    unsafe { *space = xr::Space::from_raw(h) };
    xr::Result::SUCCESS
}

unsafe extern "system" fn fake_destroy_space(space: xr::Space) -> xr::Result {
    with_world(|w| w.spaces.remove(&space.into_raw()));
    xr::Result::SUCCESS
}

fn all_valid() -> xr::SpaceLocationFlags {
    xr::SpaceLocationFlags::from_raw(0b1111)
}

unsafe extern "system" fn fake_locate_space(
    space: xr::Space,
    base: xr::Space,
    _t: xr::Time,
    loc: *mut xr::SpaceLocation,
) -> xr::Result {
    let (pose, velocity) = with_world(|w| w.locate(space, base));
    // SAFETY: valid out-structure; a chained velocity is a valid XrSpaceVelocity.
    let loc = unsafe { &mut *loc };
    loc.location_flags = all_valid();
    loc.pose = pose_to_xr(pose);
    if !loc.next.is_null() {
        // SAFETY: the only structure the tests chain is XrSpaceVelocity.
        let v = unsafe { &mut *(loc.next as *mut xr::SpaceVelocity) };
        assert_eq!(v.ty, xr::StructureType::SPACE_VELOCITY);
        match velocity {
            Some((lin, ang)) => {
                v.velocity_flags = xr::SpaceVelocityFlags::LINEAR_VALID | xr::SpaceVelocityFlags::ANGULAR_VALID;
                v.linear_velocity = crate::motion::vec_to_xr(lin);
                v.angular_velocity = crate::motion::vec_to_xr(ang);
            }
            None => v.velocity_flags = xr::SpaceVelocityFlags::EMPTY,
        }
    }
    xr::Result::SUCCESS
}

unsafe extern "system" fn fake_locate_spaces(
    _session: xr::Session,
    info: *const xr::SpacesLocateInfo,
    out: *mut xr::SpaceLocations,
) -> xr::Result {
    // SAFETY: valid structures with `space_count` spaces and locations.
    let (info, out) = unsafe { (&*info, &mut *out) };
    for i in 0..info.space_count as usize {
        // SAFETY: as above.
        let (space, loc) = unsafe { (*info.spaces.add(i), &mut *out.locations.add(i)) };
        let (pose, _) = with_world(|w| w.locate(space, info.base_space));
        loc.location_flags = all_valid();
        loc.pose = pose_to_xr(pose);
    }
    xr::Result::SUCCESS
}

fn eye(i: usize) -> Pose {
    Pose::new(Quat::IDENTITY, Vec3::new(if i == 0 { -EYE } else { EYE }, 0.0, 0.0))
}

unsafe extern "system" fn fake_locate_views(
    _session: xr::Session,
    info: *const xr::ViewLocateInfo,
    state: *mut xr::ViewState,
    capacity: u32,
    count: *mut u32,
    views: *mut xr::View,
) -> xr::Result {
    // SAFETY: valid in/out structures.
    unsafe {
        *count = 2;
        (*state).view_state_flags = if with_world(|w| w.views_untracked) {
            xr::ViewStateFlags::EMPTY
        } else {
            xr::ViewStateFlags::from_raw(0b1111)
        };
    }
    if capacity < 2 {
        return if capacity == 0 { xr::Result::SUCCESS } else { xr::Result::ERROR_SIZE_INSUFFICIENT };
    }
    // SAFETY: valid info.
    let space = unsafe { (*info).space };
    let head_in_base = with_world(|w| w.room_from(space).inverse().then(w.head));
    for i in 0..2 {
        // SAFETY: the caller provides `capacity` views.
        unsafe { (*views.add(i)).pose = pose_to_xr(head_in_base.then(eye(i))) };
    }
    xr::Result::SUCCESS
}

unsafe extern "system" fn fake_wait_frame(
    _s: xr::Session,
    _info: *const xr::FrameWaitInfo,
    state: *mut xr::FrameState,
) -> xr::Result {
    let t = with_world(|w| w.predicted);
    // SAFETY: valid out-structure.
    let state = unsafe { &mut *state };
    state.predicted_display_time = xr::Time::from_nanos(t);
    state.predicted_display_period = xr::Duration::from_nanos(11_111_111);
    state.should_render = xr::TRUE;
    xr::Result::SUCCESS
}

unsafe extern "system" fn fake_end_frame(_s: xr::Session, info: *const xr::FrameEndInfo) -> xr::Result {
    // SAFETY: valid frame description with `layer_count` layers of the types in their headers.
    let info = unsafe { &*info };
    let mut frame = Vec::new();
    for i in 0..info.layer_count as usize {
        // SAFETY: as above.
        let layer = unsafe { *info.layers.add(i) };
        // SAFETY: as above.
        let base = unsafe { &*layer };
        let poses = match base.ty {
            xr::StructureType::COMPOSITION_LAYER_PROJECTION => {
                // SAFETY: typed by the header.
                let p = unsafe { &*(layer as *const xr::CompositionLayerProjection) };
                // SAFETY: `view_count` views.
                (0..p.view_count as usize).map(|v| pose_from_xr(unsafe { &(*p.views.add(v)).pose })).collect()
            }
            xr::StructureType::COMPOSITION_LAYER_QUAD => {
                // SAFETY: typed by the header.
                vec![pose_from_xr(unsafe { &(*(layer as *const xr::CompositionLayerQuad)).pose })]
            }
            _ => vec![],
        };
        frame.push(Submitted { ty: base.ty, space: base.space, poses });
    }
    with_world(|w| w.submitted.push(frame));
    xr::Result::SUCCESS
}

unsafe extern "system" fn fake_convert_time(_i: xr::Instance, t: xr::Time, ts: *mut libc::timespec) -> xr::Result {
    let n = t.as_nanos();
    // SAFETY: valid out-pointer.
    unsafe {
        (*ts).tv_sec = n / 1_000_000_000;
        (*ts).tv_nsec = n % 1_000_000_000;
    }
    xr::Result::SUCCESS
}

// ---------------------------------------------------------------------------------------------
// The harness: the layer as the loader and an application would drive it
// ---------------------------------------------------------------------------------------------

struct Fns {
    destroy_instance: pfn::DestroyInstance,
    poll_event: pfn::PollEvent,
    create_session: pfn::CreateSession,
    destroy_session: pfn::DestroySession,
    create_reference_space: pfn::CreateReferenceSpace,
    create_action_space: pfn::CreateActionSpace,
    locate_space: pfn::LocateSpace,
    locate_spaces: pfn::LocateSpaces,
    locate_views: pfn::LocateViews,
    wait_frame: pfn::WaitFrame,
    end_frame: pfn::EndFrame,
}

struct Harness {
    _serial: MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
    path: PathBuf,
    writer: Option<RigPoseWriter>,
    f: Fns,
    instance: xr::Instance,
    session: xr::Session,
    local: xr::Space,
    stage: xr::Space,
    view: xr::Space,
    controller: xr::Space,
    generation: u32,
}

fn c_name<const N: usize>(s: &str) -> [c_char; N] {
    let mut a = [0 as c_char; N];
    for (d, b) in a.iter_mut().zip(s.bytes()) {
        *d = b as c_char;
    }
    a
}

fn negotiate(name: &CStr, min_api: xr::Version, max_api: xr::Version) -> (xr::Result, xr::NegotiateApiLayerRequest) {
    let info = xr::NegotiateLoaderInfo {
        struct_type: xr::NegotiateLoaderInfo::TYPE,
        struct_version: xr::NegotiateLoaderInfo::VERSION,
        struct_size: std::mem::size_of::<xr::NegotiateLoaderInfo>(),
        min_interface_version: 1,
        max_interface_version: 1,
        min_api_version: min_api,
        max_api_version: max_api,
    };
    let mut req = xr::NegotiateApiLayerRequest {
        struct_type: xr::NegotiateApiLayerRequest::TYPE,
        struct_version: xr::NegotiateApiLayerRequest::VERSION,
        struct_size: std::mem::size_of::<xr::NegotiateApiLayerRequest>(),
        layer_interface_version: 0,
        layer_api_version: xr::Version::new(0, 0, 0),
        get_instance_proc_addr: None,
        create_api_layer_instance: None,
    };
    // SAFETY: valid negotiation structures, as the loader passes them.
    let r = unsafe { (test_access::negotiate_entry())(&info, name.as_ptr(), &mut req) };
    (r, req)
}

macro_rules! resolve {
    ($gipa:expr, $instance:expr, $name:literal, $ty:ty) => {{
        let mut f = None;
        // SAFETY: the layer's xrGetInstanceProcAddr with a valid name and out-pointer.
        let r = unsafe { ($gipa)($instance, $name.as_ptr(), &mut f) };
        assert_eq!(r, xr::Result::SUCCESS, "{:?}", $name);
        // SAFETY: the command's real signature.
        unsafe { std::mem::transmute::<pfn::VoidFunction, $ty>(f.expect("resolved")) }
    }};
}

impl Harness {
    fn new(offer_timespec: bool, tweak: impl FnOnce(&mut Settings)) -> Harness {
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rig-pose");
        let writer = RigPoseWriter::create(&path).unwrap();
        let mut settings = Settings { pose_path: path.clone(), log_level: 0, ..Settings::default() };
        tweak(&mut settings);
        *TEST_SETTINGS.lock().unwrap() = Some(settings);
        *world() = Some(World {
            room_from_local: Pose::new(Quat::rot_y(0.6), Vec3::new(0.3, 1.1, -0.2)),
            offer_timespec,
            ..World::default()
        });

        let (r, req) = negotiate(
            c"XR_APILAYER_OPENRIGCUE_motion_compensation",
            xr::Version::new(1, 0, 0),
            xr::Version::new(1, 1, 0),
        );
        assert_eq!(r, xr::Result::SUCCESS);
        let gipa = req.get_instance_proc_addr.unwrap();
        let create = req.create_api_layer_instance.unwrap();

        let mut next = xr::ApiLayerNextInfo {
            struct_type: xr::ApiLayerNextInfo::TYPE,
            struct_version: xr::ApiLayerNextInfo::VERSION,
            struct_size: std::mem::size_of::<xr::ApiLayerNextInfo>(),
            layer_name: c_name(crate::LAYER_NAME),
            next_get_instance_proc_addr: Some(fake_gipa),
            next_create_api_layer_instance: Some(fake_create_instance),
            next: ptr::null_mut(),
        };
        let layer_info = xr::ApiLayerCreateInfo {
            struct_type: xr::ApiLayerCreateInfo::TYPE,
            struct_version: xr::ApiLayerCreateInfo::VERSION,
            struct_size: std::mem::size_of::<xr::ApiLayerCreateInfo>(),
            loader_instance: ptr::null_mut(),
            settings_file_location: [0; xr::API_LAYER_MAX_SETTINGS_PATH_SIZE],
            next_info: &mut next,
        };
        let app_ext = [c"XR_KHR_vulkan_enable2".as_ptr()];
        let info = xr::InstanceCreateInfo {
            ty: xr::StructureType::INSTANCE_CREATE_INFO,
            next: ptr::null(),
            create_flags: xr::InstanceCreateFlags::EMPTY,
            application_info: xr::ApplicationInfo {
                application_name: c_name("test"),
                application_version: 1,
                engine_name: c_name("test"),
                engine_version: 1,
                api_version: xr::Version::new(1, 0, 0),
            },
            enabled_api_layer_count: 0,
            enabled_api_layer_names: ptr::null(),
            enabled_extension_count: 1,
            enabled_extension_names: app_ext.as_ptr(),
        };
        let mut instance = xr::Instance::NULL;
        // SAFETY: valid create structures, as the loader passes them.
        let result = unsafe { create(&info, &layer_info, &mut instance) };
        assert_eq!(result, xr::Result::SUCCESS);

        let f = Fns {
            destroy_instance: resolve!(gipa, instance, c"xrDestroyInstance", pfn::DestroyInstance),
            poll_event: resolve!(gipa, instance, c"xrPollEvent", pfn::PollEvent),
            create_session: resolve!(gipa, instance, c"xrCreateSession", pfn::CreateSession),
            destroy_session: resolve!(gipa, instance, c"xrDestroySession", pfn::DestroySession),
            create_reference_space: resolve!(gipa, instance, c"xrCreateReferenceSpace", pfn::CreateReferenceSpace),
            create_action_space: resolve!(gipa, instance, c"xrCreateActionSpace", pfn::CreateActionSpace),
            locate_space: resolve!(gipa, instance, c"xrLocateSpace", pfn::LocateSpace),
            locate_spaces: resolve!(gipa, instance, c"xrLocateSpaces", pfn::LocateSpaces),
            locate_views: resolve!(gipa, instance, c"xrLocateViews", pfn::LocateViews),
            wait_frame: resolve!(gipa, instance, c"xrWaitFrame", pfn::WaitFrame),
            end_frame: resolve!(gipa, instance, c"xrEndFrame", pfn::EndFrame),
        };
        // The layer hands out its own entry points for intercepted commands.
        let ours: pfn::LocateSpace = resolve!(gipa, instance, c"xrLocateSpace", pfn::LocateSpace);
        assert_ne!(ours as usize, fake_locate_space as pfn::LocateSpace as usize);

        let session_info = xr::SessionCreateInfo {
            ty: xr::StructureType::SESSION_CREATE_INFO,
            next: ptr::null(),
            create_flags: xr::SessionCreateFlags::EMPTY,
            system_id: xr::SystemId::from_raw(1),
        };
        let mut session = xr::Session::NULL;
        // SAFETY: valid arguments.
        let result = unsafe { (f.create_session)(instance, &session_info, &mut session) };
        assert_eq!(result, xr::Result::SUCCESS);

        let mut h = Harness {
            _serial: serial,
            _dir: dir,
            path,
            writer: Some(writer),
            f,
            instance,
            session,
            local: xr::Space::NULL,
            stage: xr::Space::NULL,
            view: xr::Space::NULL,
            controller: xr::Space::NULL,
            generation: 0,
        };
        h.local = h.reference_space(xr::ReferenceSpaceType::LOCAL);
        h.stage = h.reference_space(xr::ReferenceSpaceType::STAGE);
        h.view = h.reference_space(xr::ReferenceSpaceType::VIEW);
        let action_info = xr::ActionSpaceCreateInfo {
            ty: xr::StructureType::ACTION_SPACE_CREATE_INFO,
            next: ptr::null(),
            action: xr::Action::NULL,
            subaction_path: xr::Path::NULL,
            pose_in_action_space: pose_to_xr(Pose::IDENTITY),
        };
        // SAFETY: valid arguments.
        let result = unsafe { (h.f.create_action_space)(session, &action_info, &mut h.controller) };
        assert_eq!(result, xr::Result::SUCCESS);
        h
    }

    fn reference_space(&self, ty: xr::ReferenceSpaceType) -> xr::Space {
        let info = xr::ReferenceSpaceCreateInfo {
            ty: xr::StructureType::REFERENCE_SPACE_CREATE_INFO,
            next: ptr::null(),
            reference_space_type: ty,
            pose_in_reference_space: pose_to_xr(Pose::IDENTITY),
        };
        let mut space = xr::Space::NULL;
        // SAFETY: valid arguments.
        let result = unsafe { (self.f.create_reference_space)(self.session, &info, &mut space) };
        assert_eq!(result, xr::Result::SUCCESS);
        space
    }

    fn publish(&mut self, sample: RigSample) {
        self.writer.as_mut().expect("writer").publish(&sample).unwrap();
    }

    /// Publishes `values` measured now, with the harness's generation and the scene's CoR offset.
    fn publish_values(&mut self, values: RigValues) -> RigSample {
        let s = RigSample {
            timestamp_ns: monotonic_now_ns(),
            values,
            rates: None,
            cor_offset: OFFSET,
            enabled: true,
            recenter_generation: self.generation,
        };
        self.publish(s);
        s
    }

    fn event(&self, ev: FakeEvent) {
        with_world(|w| w.events.push_back(ev));
        let mut buf =
            xr::EventDataBuffer { ty: xr::StructureType::EVENT_DATA_BUFFER, next: ptr::null(), varying: [0; 4000] };
        // SAFETY: valid instance and buffer.
        while unsafe { (self.f.poll_event)(self.instance, &mut buf) } == xr::Result::SUCCESS {}
    }

    /// Runs xrWaitFrame for a frame displayed at `display` (monotonic ns = XrTime in the fake).
    fn frame_at(&self, display: u64) -> xr::Time {
        with_world(|w| w.predicted = display as i64);
        let mut state = xr::FrameState {
            ty: xr::StructureType::FRAME_STATE,
            next: ptr::null_mut(),
            predicted_display_time: xr::Time::from_nanos(0),
            predicted_display_period: xr::Duration::from_nanos(0),
            should_render: xr::FALSE,
        };
        let wait = xr::FrameWaitInfo { ty: xr::StructureType::FRAME_WAIT_INFO, next: ptr::null() };
        // SAFETY: valid arguments.
        let result = unsafe { (self.f.wait_frame)(self.session, &wait, &mut state) };
        assert_eq!(result, xr::Result::SUCCESS);
        state.predicted_display_time
    }

    fn frame(&self) -> xr::Time {
        self.frame_at(monotonic_now_ns() + 11_000_000)
    }

    fn views(&self, space: xr::Space, time: xr::Time) -> Vec<Pose> {
        let info = xr::ViewLocateInfo {
            ty: xr::StructureType::VIEW_LOCATE_INFO,
            next: ptr::null(),
            view_configuration_type: xr::ViewConfigurationType::PRIMARY_STEREO,
            display_time: time,
            space,
        };
        let mut state = xr::ViewState {
            ty: xr::StructureType::VIEW_STATE,
            next: ptr::null_mut(),
            view_state_flags: xr::ViewStateFlags::EMPTY,
        };
        let blank = xr::View {
            ty: xr::StructureType::VIEW,
            next: ptr::null_mut(),
            pose: pose_to_xr(Pose::IDENTITY),
            fov: xr::Fovf { angle_left: -1.0, angle_right: 1.0, angle_up: 1.0, angle_down: -1.0 },
        };
        let mut views = [blank; 2];
        let mut n = 0;
        let locate = self.f.locate_views;
        // SAFETY: valid arguments with a 2-element view array.
        let result = unsafe { locate(self.session, &info, &mut state, 2, &mut n, views.as_mut_ptr()) };
        assert_eq!(result, xr::Result::SUCCESS);
        views[..n as usize].iter().map(|v| pose_from_xr(&v.pose)).collect()
    }

    fn locate(&self, space: xr::Space, base: xr::Space, time: xr::Time) -> Pose {
        self.locate_with_velocity(space, base, time).0
    }

    fn locate_with_velocity(&self, space: xr::Space, base: xr::Space, time: xr::Time) -> (Pose, Option<(Vec3, Vec3)>) {
        let mut vel = xr::SpaceVelocity {
            ty: xr::StructureType::SPACE_VELOCITY,
            next: ptr::null_mut(),
            velocity_flags: xr::SpaceVelocityFlags::EMPTY,
            linear_velocity: crate::motion::vec_to_xr(Vec3::ZERO),
            angular_velocity: crate::motion::vec_to_xr(Vec3::ZERO),
        };
        let mut loc = xr::SpaceLocation {
            ty: xr::StructureType::SPACE_LOCATION,
            next: (&mut vel as *mut xr::SpaceVelocity).cast(),
            location_flags: xr::SpaceLocationFlags::EMPTY,
            pose: pose_to_xr(Pose::IDENTITY),
        };
        // SAFETY: valid arguments with a chained XrSpaceVelocity.
        let result = unsafe { (self.f.locate_space)(space, base, time, &mut loc) };
        assert_eq!(result, xr::Result::SUCCESS);
        let v = (!vel.velocity_flags.is_empty())
            .then(|| (vec_from_xr(&vel.linear_velocity), vec_from_xr(&vel.angular_velocity)));
        (pose_from_xr(&loc.pose), v)
    }

    fn locate_many(&self, spaces: &[xr::Space], base: xr::Space, time: xr::Time) -> Vec<Pose> {
        let info = xr::SpacesLocateInfo {
            ty: xr::StructureType::SPACES_LOCATE_INFO,
            next: ptr::null(),
            base_space: base,
            time,
            space_count: spaces.len() as u32,
            spaces: spaces.as_ptr(),
        };
        let mut data = vec![
            xr::SpaceLocationData {
                location_flags: xr::SpaceLocationFlags::EMPTY,
                pose: pose_to_xr(Pose::IDENTITY)
            };
            spaces.len()
        ];
        let mut out = xr::SpaceLocations {
            ty: xr::StructureType::SPACE_LOCATIONS,
            next: ptr::null_mut(),
            location_count: data.len() as u32,
            locations: data.as_mut_ptr(),
        };
        // SAFETY: valid arguments.
        let result = unsafe { (self.f.locate_spaces)(self.session, &info, &mut out) };
        assert_eq!(result, xr::Result::SUCCESS);
        data.iter().map(|d| pose_from_xr(&d.pose)).collect()
    }

    /// Submits a projection layer with `views` and quads `(space, pose)`; returns what the runtime got.
    fn end_frame(
        &self,
        time: xr::Time,
        projection_space: xr::Space,
        views: &[Pose],
        quads: &[(xr::Space, Pose)],
    ) -> Vec<Submitted> {
        let sub_image = xr::SwapchainSubImage {
            swapchain: xr::Swapchain::NULL,
            image_rect: xr::Rect2Di::default(),
            image_array_index: 0,
        };
        let fov = xr::Fovf { angle_left: -1.0, angle_right: 1.0, angle_up: 1.0, angle_down: -1.0 };
        let pviews: Vec<xr::CompositionLayerProjectionView> = views
            .iter()
            .map(|p| xr::CompositionLayerProjectionView {
                ty: xr::StructureType::COMPOSITION_LAYER_PROJECTION_VIEW,
                next: ptr::null(),
                pose: pose_to_xr(*p),
                fov,
                sub_image,
            })
            .collect();
        let projection = xr::CompositionLayerProjection {
            ty: xr::StructureType::COMPOSITION_LAYER_PROJECTION,
            next: ptr::null(),
            layer_flags: xr::CompositionLayerFlags::EMPTY,
            space: projection_space,
            view_count: pviews.len() as u32,
            views: pviews.as_ptr(),
        };
        let quad_layers: Vec<xr::CompositionLayerQuad> = quads
            .iter()
            .map(|(space, pose)| xr::CompositionLayerQuad {
                ty: xr::StructureType::COMPOSITION_LAYER_QUAD,
                next: ptr::null(),
                layer_flags: xr::CompositionLayerFlags::EMPTY,
                space: *space,
                eye_visibility: xr::EyeVisibility::BOTH,
                sub_image,
                pose: pose_to_xr(*pose),
                size: xr::Extent2Df { width: 1.0, height: 1.0 },
            })
            .collect();
        let mut layers: Vec<*const xr::CompositionLayerBaseHeader> =
            vec![(&projection as *const xr::CompositionLayerProjection).cast()];
        layers.extend(
            quad_layers.iter().map(|q| (q as *const xr::CompositionLayerQuad).cast::<xr::CompositionLayerBaseHeader>()),
        );
        let info = xr::FrameEndInfo {
            ty: xr::StructureType::FRAME_END_INFO,
            next: ptr::null(),
            display_time: time,
            environment_blend_mode: xr::EnvironmentBlendMode::OPAQUE,
            layer_count: layers.len() as u32,
            layers: layers.as_ptr(),
        };
        // SAFETY: valid frame description; all layers live until the call returns.
        let result = unsafe { (self.f.end_frame)(self.session, &info) };
        assert_eq!(result, xr::Result::SUCCESS);
        // The application's own structures are never modified.
        assert_eq!(pose_from_xr(&pviews[0].pose), views[0]);
        with_world(|w| w.submitted.pop().unwrap())
    }

    fn status(&self) -> Option<Status> {
        test_access::status(self.session)
    }

    /// Sets the tracked head and controller for rig values `rig` and true (cockpit) poses.
    fn track(&self, rig: &RigValues, head_true: Pose, controller_true: Pose) {
        with_world(|w| {
            w.head = tracked(rig, head_true);
            w.controller = tracked(rig, controller_true);
        });
    }

    /// Calibrates at rig values `rig` with the head at the scene's neutral head pose.
    fn calibrate(&mut self, rig: RigValues) {
        self.publish_values(rig);
        self.track(&rig, head0(), controller0());
        self.event(FakeEvent::State(self.session, xr::SessionState::FOCUSED));
        self.frame();
        assert_eq!(self.status(), Some(Status::Active));
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // SAFETY: valid handles created by this harness.
        unsafe {
            (self.f.destroy_session)(self.session);
            (self.f.destroy_instance)(self.instance);
        }
        *TEST_SETTINGS.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

// ---------------------------------------------------------------------------------------------
// The scene
// ---------------------------------------------------------------------------------------------

const OFFSET: CorOffset = CorOffset { forward: 0.1, down: 0.6, right: 0.0 };

/// The neutral rig frame in the room (the fake STAGE = room).
fn cor() -> Pose {
    Pose::new(Quat::rot_y(0.3), Vec3::new(0.2, 0.45, -0.5))
}

/// The head at calibration: `OFFSET` from the CoR, facing the rig's forward.
fn head0() -> Pose {
    cor().then(Pose::new(Quat::IDENTITY, -OFFSET.to_vec3()))
}

fn controller0() -> Pose {
    cor().then(Pose::new(Quat::rot_x(-0.4), Vec3::new(0.25, 0.3, -0.35)))
}

/// What the runtime tracks when the rig is at `rig` and the device is at `true_pose` in the cockpit.
fn tracked(rig: &RigValues, true_pose: Pose) -> Pose {
    cor().then(rig.transform()).then(cor().inverse()).then(true_pose)
}

/// `K = C·P⁻¹·C⁻¹` in the room.
fn compensation(rig: &RigValues) -> Pose {
    cor().then(rig.transform().inverse()).then(cor().inverse())
}

fn local_from_room() -> Pose {
    with_world(|w| w.room_from_local.inverse())
}

const TOL: f64 = 2e-5;

#[track_caller]
fn assert_pose(a: Pose, b: Pose) {
    assert!(a.distance(b) < TOL, "\n  got  {a:?}\n  want {b:?}");
}

const MOVED: RigValues = RigValues { sway: 4.0, surge: -10.0, heave: 30.0, yaw: 2.0, roll: 5.0, pitch: 8.0 };

fn head1() -> Pose {
    head0().then(Pose::new(Quat::rot_y(0.2) * Quat::rot_x(-0.1), Vec3::new(0.03, 0.0, 0.01)))
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[test]
fn negotiation_checks_name_and_versions() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (r, req) =
        negotiate(c"XR_APILAYER_OPENRIGCUE_motion_compensation", xr::Version::new(1, 0, 0), xr::Version::new(1, 1, 0));
    assert_eq!(r, xr::Result::SUCCESS);
    assert_eq!(req.layer_interface_version, 1);
    assert!(req.layer_api_version <= xr::Version::new(1, 1, 0));
    assert!(req.get_instance_proc_addr.is_some() && req.create_api_layer_instance.is_some());
    assert_eq!(
        negotiate(c"XR_APILAYER_SOMEONE_else", xr::Version::new(1, 0, 0), xr::Version::new(1, 1, 0)).0,
        xr::Result::ERROR_INITIALIZATION_FAILED
    );
    assert_eq!(
        negotiate(c"XR_APILAYER_OPENRIGCUE_motion_compensation", xr::Version::new(2, 0, 0), xr::Version::new(2, 0, 0))
            .0,
        xr::Result::ERROR_INITIALIZATION_FAILED
    );
}

#[test]
fn compensates_views_spaces_and_reverts_submitted_layers() {
    let mut h = Harness::new(true, |_| {});
    assert!(
        with_world(|w| w.enabled_extensions.iter().any(|e| e == "XR_KHR_convert_timespec_time")),
        "layer enables timespec conversion"
    );
    assert!(
        with_world(|w| w.enabled_extensions.iter().any(|e| e == "XR_KHR_vulkan_enable2")),
        "application extensions kept"
    );
    assert_eq!(
        with_world(|w| w.spaces.get(&test_access::tracking_space(h.session).unwrap().into_raw()).copied())
            .map(|s| matches!(s, FakeSpace::Reference(xr::ReferenceSpaceType::STAGE, _))),
        Some(true)
    );

    // Calibrate off-neutral.
    h.calibrate(RigValues { pitch: 3.0, roll: -2.0, ..RigValues::ZERO });

    // The rig moves; the user turns their head a little in the cockpit.
    let controller_true = controller0().then(Pose::new(Quat::rot_z(0.2), Vec3::new(0.0, 0.05, 0.0)));
    h.publish_values(MOVED);
    h.track(&MOVED, head1(), controller_true);
    let t = h.frame();
    let local = local_from_room();

    // Views: the eyes where they are relative to the cockpit.
    let views = h.views(h.local, t);
    assert_eq!(views.len(), 2);
    for (i, v) in views.iter().enumerate() {
        assert_pose(*v, local.then(head1()).then(eye(i)));
    }
    // Views located in VIEW space are head-relative and unchanged.
    assert_pose(h.views(h.view, t)[1], eye(1));

    // Spaces: head and controller in LOCAL and STAGE, and the inverse direction.
    assert_pose(h.locate(h.view, h.local, t), local.then(head1()));
    assert_pose(h.locate(h.view, h.stage, t), head1());
    assert_pose(h.locate(h.local, h.view, t), local.then(head1()).inverse());
    assert_pose(h.locate(h.controller, h.local, t), local.then(controller_true));
    assert_pose(h.locate(h.controller, h.view, t), head1().inverse().then(controller_true));
    assert_pose(h.locate(h.stage, h.local, t), local);
    let many = h.locate_many(&[h.view, h.controller, h.stage], h.local, t);
    assert_pose(many[0], local.then(head1()));
    assert_pose(many[1], local.then(controller_true));
    assert_pose(many[2], local);

    // Submission: the projection views go back to the tracked eye poses, a world-locked quad gets the
    // inverse compensation, a head-locked quad is untouched.
    let quad = Pose::new(Quat::rot_y(-0.2), Vec3::new(0.0, 1.0, -2.0));
    let hud = Pose::new(Quat::IDENTITY, Vec3::new(0.0, 0.0, -1.0));
    let submitted = h.end_frame(t, h.local, &views, &[(h.local, quad), (h.view, hud)]);
    assert_eq!(submitted.len(), 3);
    assert_eq!(submitted[0].ty, xr::StructureType::COMPOSITION_LAYER_PROJECTION);
    assert_eq!(submitted[0].space, h.local);
    let tracked_head = tracked(&MOVED, head1());
    for (i, p) in submitted[0].poses.iter().enumerate() {
        assert_pose(*p, local.then(tracked_head).then(eye(i)));
    }
    let x = local.inverse();
    let k_in_local = x.inverse().then(compensation(&MOVED)).then(x);
    assert_pose(submitted[1].poses[0], k_in_local.inverse().then(quad));
    assert_pose(submitted[2].poses[0], hud);
}

#[test]
fn submission_reuses_the_transform_the_frame_was_located_with() {
    let mut h = Harness::new(true, |_| {});
    h.calibrate(RigValues::ZERO);
    h.publish_values(MOVED);
    h.track(&MOVED, head1(), controller0());
    let t = h.frame();
    let views = h.views(h.local, t);
    // The rig moves on before the frame is submitted.
    h.publish_values(RigValues { pitch: -6.0, ..RigValues::ZERO });
    let submitted = h.end_frame(t, h.local, &views, &[]);
    for (i, p) in submitted[0].poses.iter().enumerate() {
        assert_pose(*p, local_from_room().then(tracked(&MOVED, head1())).then(eye(i)));
    }
}

#[test]
fn frames_located_uncompensated_are_submitted_unchanged() {
    let mut h = Harness::new(true, |_| {});
    h.calibrate(RigValues::ZERO);
    h.publish_values(MOVED);
    h.track(&MOVED, head1(), controller0());
    // A compensated frame, so a non-identity transform is cached close to the next one.
    let t1 = h.frame();
    let views1 = h.views(h.local, t1);
    h.end_frame(t1, h.local, &views1, &[]);
    // Tracking drops for one frame: its views are not compensated, so nothing may be reverted.
    with_world(|w| w.views_untracked = true);
    let t2 = h.frame();
    let views2 = h.views(h.local, t2);
    let quad = Pose::new(Quat::rot_y(-0.2), Vec3::new(0.0, 1.0, -2.0));
    let submitted = h.end_frame(t2, h.local, &views2, &[(h.local, quad)]);
    for (p, v) in submitted[0].poses.iter().zip(&views2) {
        assert_pose(*p, *v);
    }
    assert_pose(submitted[1].poses[0], quad);
    // Tracking is back: the next frame is compensated and reverted again.
    with_world(|w| w.views_untracked = false);
    let t3 = h.frame();
    let views3 = h.views(h.local, t3);
    let submitted = h.end_frame(t3, h.local, &views3, &[]);
    for (i, p) in submitted[0].poses.iter().enumerate() {
        assert_pose(*p, local_from_room().then(tracked(&MOVED, head1())).then(eye(i)));
    }
}

#[test]
fn passes_through_when_stale_disabled_or_claimed_by_the_driver() {
    let mut h = Harness::new(true, |_| {});
    h.calibrate(RigValues::ZERO);
    let raw = |h: &Harness, t| {
        let head = with_world(|w| w.head);
        assert_pose(h.views(h.local, t)[0], local_from_room().then(head).then(eye(0)));
        assert_pose(h.locate(h.view, h.stage, t), head);
    };

    // Stale: measured 200 ms ago.
    let s = RigSample { timestamp_ns: monotonic_now_ns() - 200_000_000, ..h.publish_values(MOVED) };
    h.publish(s);
    h.track(&MOVED, head1(), controller0());
    let t = h.frame();
    assert!(matches!(h.status(), Some(Status::Unusable(openrigcue_rigpose::Unusable::Stale { .. }))));
    raw(&h, t);
    // A pass-through frame is submitted unchanged.
    let views = h.views(h.local, t);
    let submitted = h.end_frame(t, h.local, &views, &[]);
    assert_pose(submitted[0].poses[0], views[0]);

    // Switched off.
    let s = RigSample { enabled: false, ..h.publish_values(MOVED) };
    h.publish(s);
    let t = h.frame();
    assert_eq!(h.status(), Some(Status::Unusable(openrigcue_rigpose::Unusable::Disabled)));
    raw(&h, t);

    // The SteamVR driver claims the pose: it compensates everything on SteamVR itself.
    h.publish_values(MOVED);
    let driver = RigPoseReader::open(&h.path).unwrap();
    driver.claim(ConsumerKind::STEAMVR_DRIVER, monotonic_now_ns()).unwrap();
    let t = h.frame();
    assert_eq!(h.status(), Some(Status::Yielded(ConsumerKind::STEAMVR_DRIVER)));
    raw(&h, t);

    // The driver goes away: the layer takes over with its calibration intact.
    driver.release(ConsumerKind::STEAMVR_DRIVER).unwrap();
    h.publish_values(MOVED);
    let t = h.frame();
    assert_eq!(h.status(), Some(Status::Active));
    assert_pose(h.locate(h.view, h.stage, t), head1());
    // And it holds the claim itself now.
    assert_eq!(driver.consumer().unwrap().map(|c| c.kind), Some(ConsumerKind::XR_LAYER));
}

#[test]
fn passes_through_without_a_pose_file() {
    let mut h = Harness::new(true, |_| {});
    h.writer = None;
    std::fs::remove_file(&h.path).unwrap();
    h.track(&MOVED, head1(), controller0());
    h.event(FakeEvent::State(h.session, xr::SessionState::FOCUSED));
    let t = h.frame();
    assert_eq!(h.status(), Some(Status::NoPose));
    let head = with_world(|w| w.head);
    assert_pose(h.locate(h.view, h.stage, t), head);
    let views = h.views(h.local, t);
    assert_pose(views[1], local_from_room().then(head).then(eye(1)));
    assert_pose(h.end_frame(t, h.local, &views, &[])[0].poses[1], views[1]);
}

#[test]
fn recenter_generation_recalibrates() {
    let mut h = Harness::new(true, |_| {});
    // A bad first calibration: the user looked 30° to the left.
    h.publish_values(RigValues::ZERO);
    with_world(|w| w.head = head0().then(Pose::new(Quat::rot_y(0.5), Vec3::ZERO)));
    h.event(FakeEvent::State(h.session, xr::SessionState::FOCUSED));
    h.frame();
    h.publish_values(MOVED);
    h.track(&MOVED, head1(), controller0());
    let t = h.frame();
    assert!(h.locate(h.view, h.stage, t).distance(head1()) > 1e-3, "wrong calibration gives wrong compensation");

    // Re-centre from the daemon with the user facing forward.
    h.generation += 1;
    h.publish_values(RigValues::ZERO);
    h.track(&RigValues::ZERO, head0(), controller0());
    h.frame();
    h.publish_values(MOVED);
    h.track(&MOVED, head1(), controller0());
    let t = h.frame();
    assert_pose(h.locate(h.view, h.stage, t), head1());
}

#[test]
fn auto_calibration_off_waits_for_a_recenter() {
    let mut h = Harness::new(true, |s| s.auto_calibrate = false);
    h.publish_values(RigValues::ZERO);
    h.track(&RigValues::ZERO, head0(), controller0());
    h.event(FakeEvent::State(h.session, xr::SessionState::FOCUSED));
    h.frame();
    assert_eq!(h.status(), Some(Status::Uncalibrated));
    h.generation += 1;
    h.publish_values(RigValues::ZERO);
    h.frame();
    assert_eq!(h.status(), Some(Status::Active));
}

#[test]
fn runtime_recenter_of_the_tracking_space_recalibrates_after_the_change() {
    let mut h = Harness::new(true, |_| {});
    h.calibrate(RigValues::ZERO);
    let change = monotonic_now_ns() + 50_000_000;
    h.event(FakeEvent::SpaceChange(h.session, xr::ReferenceSpaceType::STAGE, change as i64));
    h.publish_values(MOVED);
    h.track(&MOVED, head1(), controller0());
    let t = h.frame_at(change - 1);
    assert_eq!(h.status(), Some(Status::Uncalibrated));
    assert_pose(h.locate(h.view, h.stage, t), tracked(&MOVED, head1()));
    // A LOCAL change only drops cached offsets; it keeps the calibration (it lives in STAGE).
    h.publish_values(RigValues::ZERO);
    h.track(&RigValues::ZERO, head0(), controller0());
    h.frame_at(change);
    assert_eq!(h.status(), Some(Status::Active));
    with_world(|w| w.room_from_local = Pose::new(Quat::rot_y(-1.0), Vec3::new(1.0, 0.0, 0.0)));
    h.event(FakeEvent::SpaceChange(h.session, xr::ReferenceSpaceType::LOCAL, change as i64));
    h.publish_values(MOVED);
    h.track(&MOVED, head1(), controller0());
    let t = h.frame();
    assert_eq!(h.status(), Some(Status::Active));
    assert_pose(h.locate(h.view, h.local, t), local_from_room().then(head1()));
}

#[test]
fn velocities_lose_the_rig_motion_with_prediction() {
    let mut h = Harness::new(true, |_| {});
    h.calibrate(RigValues::ZERO);
    let rates = RigValues { sway: 20.0, surge: -15.0, heave: 60.0, yaw: 5.0, roll: -25.0, pitch: 40.0 };
    let sample = RigSample { rates: Some(rates), ..h.publish_values(MOVED) };
    h.publish(sample);
    let display = sample.timestamp_ns + 11_000_000;
    // The runtime sees the rig where it will be at display time, moving at the published rates; the
    // head itself is still in the cockpit.
    let predicted = |dt: f64| sample.extrapolated(0.011 + dt);
    let step = 1e-4;
    let a = tracked(&predicted(0.0), head0());
    let b = tracked(&predicted(step), head0());
    let m = Motion::from_poses(a, b, step);
    with_world(|w| {
        w.head = a;
        w.head_velocity = (m.linear, m.angular);
    });
    let t = h.frame_at(display);
    let (pose, velocity) = h.locate_with_velocity(h.view, h.stage, t);
    assert_pose(pose, head0());
    let (lin, ang) = velocity.expect("velocity kept valid");
    assert!(m.linear.length() > 0.05 && m.angular.length() > 0.3, "the test rig moves");
    assert!(lin.length() < 2e-3, "{lin:?}");
    assert!(ang.length() < 2e-3, "{ang:?}");
}

#[test]
fn works_without_timespec_conversion() {
    let mut h = Harness::new(false, |_| {});
    assert!(!with_world(|w| w.enabled_extensions.iter().any(|e| e == "XR_KHR_convert_timespec_time")));
    h.calibrate(RigValues::ZERO);
    h.publish_values(MOVED);
    h.track(&MOVED, head1(), controller0());
    let t = h.frame();
    assert_pose(h.locate(h.view, h.local, t), local_from_room().then(head1()));
}

#[test]
fn controllers_can_be_left_uncompensated() {
    let mut h = Harness::new(true, |s| s.compensate_controllers = false);
    h.calibrate(RigValues::ZERO);
    h.publish_values(MOVED);
    h.track(&MOVED, head1(), controller0());
    let t = h.frame();
    let raw_controller = with_world(|w| w.controller);
    assert_pose(h.locate(h.controller, h.stage, t), raw_controller);
    // Relative to the (compensated) head the controller now carries the rig motion.
    assert_pose(h.locate(h.controller, h.view, t), head1().inverse().then(raw_controller));
}

#[test]
fn manifest_template_is_a_valid_implicit_layer_manifest() {
    let text = include_str!("../manifest/openrigcue_xr_layer.json.in")
        .replace("@LIBRARY_PATH@", "/home/u/.local/share/openrigcue/libopenrigcue_xr_layer.so");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["file_format_version"], "1.0.0");
    let layer = &v["api_layer"];
    assert_eq!(layer["name"], crate::LAYER_NAME);
    assert_eq!(layer["library_path"], "/home/u/.local/share/openrigcue/libopenrigcue_xr_layer.so");
    assert_eq!(layer["functions"]["xrNegotiateLoaderApiLayerInterface"], "xrNegotiateLoaderApiLayerInterface");
    assert!(
        layer["disable_environment"].as_str().is_some_and(|s| !s.is_empty()),
        "implicit layers need disable_environment"
    );
    assert!(layer["enable_environment"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(layer["api_version"].as_str().is_some());
}
