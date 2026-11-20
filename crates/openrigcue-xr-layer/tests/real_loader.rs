//! The layer under the system's Khronos OpenXR loader, installed by `scripts/install.sh` into a
//! temporary `XDG_DATA_HOME`, over the fake runtime of `examples/fake_runtime.rs`
//! (`XR_RUNTIME_JSON`). This checks the manifest, `enable_environment`, negotiation and the
//! dispatch chain as a real application sees them. No VR runtime is started and nothing outside
//! the temporary directory is written.
//!
//! Skipped (passes with a note) when `libopenxr_loader.so.1` is not installed.

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;

use openrigcue_rigpose::{monotonic_now_ns, CorOffset, RigPoseWriter, RigSample, RigValues};
use openxr_sys as xr;
use xr::{pfn, Handle};

struct Lib(*mut c_void);

impl Lib {
    fn open(name: &str) -> Option<Lib> {
        let c = CString::new(name).unwrap();
        // SAFETY: dlopen with a valid C string; the handle is never closed (the loader keeps
        // function pointers into it for the life of the test process).
        let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW) };
        (!h.is_null()).then_some(Lib(h))
    }

    /// # Safety
    /// `T` must be the symbol's real function type.
    unsafe fn sym<T: Copy>(&self, name: &CStr) -> T {
        // SAFETY: valid handle and name.
        let p = unsafe { libc::dlsym(self.0, name.as_ptr()) };
        assert!(!p.is_null(), "{name:?} not found");
        assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
        // SAFETY: guaranteed by the caller; function pointers are pointer-sized.
        unsafe { std::mem::transmute_copy(&p) }
    }
}

struct Xr {
    gipa: pfn::GetInstanceProcAddr,
}

impl Xr {
    /// # Safety
    /// `T` must be the command's real signature.
    unsafe fn get<T: Copy>(&self, instance: xr::Instance, name: &CStr) -> T {
        let mut f = None;
        // SAFETY: the loader's xrGetInstanceProcAddr.
        let result = unsafe { (self.gipa)(instance, name.as_ptr(), &mut f) };
        assert_eq!(result, xr::Result::SUCCESS, "{name:?}");
        let f: pfn::VoidFunction = f.unwrap();
        // SAFETY: guaranteed by the caller.
        unsafe { std::mem::transmute_copy(&f) }
    }
}

fn identity() -> xr::Posef {
    xr::Posef {
        orientation: xr::Quaternionf { x: 0.0, y: 0.0, z: 0.0, w: 1.0 },
        position: xr::Vector3f { x: 0.0, y: 0.0, z: 0.0 },
    }
}

fn c_name<const N: usize>(s: &str) -> [c_char; N] {
    let mut a = [0 as c_char; N];
    for (d, b) in a.iter_mut().zip(s.bytes()) {
        *d = b as c_char;
    }
    a
}

struct App<'a> {
    xr: &'a Xr,
    instance: xr::Instance,
    session: xr::Session,
    local: xr::Space,
    view: xr::Space,
}

impl<'a> App<'a> {
    fn start(xr: &'a Xr) -> App<'a> {
        // SAFETY: every call below is a loader command called with valid, typed structures.
        unsafe {
            let create: pfn::CreateInstance = xr.get(xr::Instance::NULL, c"xrCreateInstance");
            let info = xr::InstanceCreateInfo {
                ty: xr::StructureType::INSTANCE_CREATE_INFO,
                next: ptr::null(),
                create_flags: xr::InstanceCreateFlags::EMPTY,
                application_info: xr::ApplicationInfo {
                    application_name: c_name("real-loader-test"),
                    application_version: 1,
                    engine_name: c_name("none"),
                    engine_version: 1,
                    api_version: xr::Version::new(1, 0, 0),
                },
                enabled_api_layer_count: 0,
                enabled_api_layer_names: ptr::null(),
                enabled_extension_count: 0,
                enabled_extension_names: ptr::null(),
            };
            let mut instance = xr::Instance::NULL;
            assert_eq!(create(&info, &mut instance), xr::Result::SUCCESS);

            let create_session: pfn::CreateSession = xr.get(instance, c"xrCreateSession");
            let sinfo = xr::SessionCreateInfo {
                ty: xr::StructureType::SESSION_CREATE_INFO,
                next: ptr::null(),
                create_flags: xr::SessionCreateFlags::EMPTY,
                system_id: xr::SystemId::from_raw(1),
            };
            let mut session = xr::Session::NULL;
            assert_eq!(create_session(instance, &sinfo, &mut session), xr::Result::SUCCESS);

            let mut app = App { xr, instance, session, local: xr::Space::NULL, view: xr::Space::NULL };
            app.local = app.space(xr::ReferenceSpaceType::LOCAL);
            app.view = app.space(xr::ReferenceSpaceType::VIEW);

            let poll: pfn::PollEvent = xr.get(instance, c"xrPollEvent");
            let mut buf =
                xr::EventDataBuffer { ty: xr::StructureType::EVENT_DATA_BUFFER, next: ptr::null(), varying: [0; 4000] };
            while poll(instance, &mut buf) == xr::Result::SUCCESS {}
            app
        }
    }

    fn space(&self, ty: xr::ReferenceSpaceType) -> xr::Space {
        let info = xr::ReferenceSpaceCreateInfo {
            ty: xr::StructureType::REFERENCE_SPACE_CREATE_INFO,
            next: ptr::null(),
            reference_space_type: ty,
            pose_in_reference_space: identity(),
        };
        let mut space = xr::Space::NULL;
        // SAFETY: the loader's command with valid arguments.
        unsafe {
            let f: pfn::CreateReferenceSpace = self.xr.get(self.instance, c"xrCreateReferenceSpace");
            assert_eq!(f(self.session, &info, &mut space), xr::Result::SUCCESS);
        }
        space
    }

    fn wait_frame(&self) -> xr::Time {
        let mut state = xr::FrameState {
            ty: xr::StructureType::FRAME_STATE,
            next: ptr::null_mut(),
            predicted_display_time: xr::Time::from_nanos(0),
            predicted_display_period: xr::Duration::from_nanos(0),
            should_render: xr::FALSE,
        };
        // SAFETY: the loader's command with valid arguments.
        unsafe {
            let f: pfn::WaitFrame = self.xr.get(self.instance, c"xrWaitFrame");
            assert_eq!(f(self.session, ptr::null(), &mut state), xr::Result::SUCCESS);
        }
        state.predicted_display_time
    }

    fn head(&self, time: xr::Time) -> xr::Posef {
        let mut loc = xr::SpaceLocation {
            ty: xr::StructureType::SPACE_LOCATION,
            next: ptr::null_mut(),
            location_flags: xr::SpaceLocationFlags::EMPTY,
            pose: identity(),
        };
        // SAFETY: the loader's command with valid arguments.
        unsafe {
            let f: pfn::LocateSpace = self.xr.get(self.instance, c"xrLocateSpace");
            assert_eq!(f(self.view, self.local, time, &mut loc), xr::Result::SUCCESS);
        }
        loc.pose
    }

    fn end_frame(&self, time: xr::Time, pose: xr::Posef) {
        let view = xr::CompositionLayerProjectionView {
            ty: xr::StructureType::COMPOSITION_LAYER_PROJECTION_VIEW,
            next: ptr::null(),
            pose,
            fov: xr::Fovf { angle_left: -1.0, angle_right: 1.0, angle_up: 1.0, angle_down: -1.0 },
            sub_image: xr::SwapchainSubImage {
                swapchain: xr::Swapchain::NULL,
                image_rect: xr::Rect2Di::default(),
                image_array_index: 0,
            },
        };
        let layer = xr::CompositionLayerProjection {
            ty: xr::StructureType::COMPOSITION_LAYER_PROJECTION,
            next: ptr::null(),
            layer_flags: xr::CompositionLayerFlags::EMPTY,
            space: self.local,
            view_count: 1,
            views: &view,
        };
        let layers = [(&layer as *const xr::CompositionLayerProjection).cast::<xr::CompositionLayerBaseHeader>()];
        let info = xr::FrameEndInfo {
            ty: xr::StructureType::FRAME_END_INFO,
            next: ptr::null(),
            display_time: time,
            environment_blend_mode: xr::EnvironmentBlendMode::OPAQUE,
            layer_count: 1,
            layers: layers.as_ptr(),
        };
        // SAFETY: the loader's command with a valid frame.
        unsafe {
            let f: pfn::EndFrame = self.xr.get(self.instance, c"xrEndFrame");
            assert_eq!(f(self.session, &info), xr::Result::SUCCESS);
        }
    }
}

impl Drop for App<'_> {
    fn drop(&mut self) {
        // SAFETY: the loader's commands on handles this app created.
        unsafe {
            let ds: pfn::DestroySession = self.xr.get(self.instance, c"xrDestroySession");
            ds(self.session);
            let di: pfn::DestroyInstance = self.xr.get(self.instance, c"xrDestroyInstance");
            di(self.instance);
        }
    }
}

fn target_dir() -> PathBuf {
    // target/<profile>/deps/real_loader-<hash>
    std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().to_owned()
}

/// The first existing path; `cargo test -p openrigcue-xr-layer` builds both libraries (a run
/// filtered with `--test` does not build the example).
fn find(candidates: &[PathBuf]) -> PathBuf {
    candidates.iter().find(|p| p.is_file()).cloned().unwrap_or_else(|| {
        panic!("none of {candidates:?} exists; run `cargo test -p openrigcue-xr-layer` so the cdylibs are built")
    })
}

fn publish(w: &mut RigPoseWriter, values: RigValues) {
    w.publish(&RigSample {
        timestamp_ns: monotonic_now_ns(),
        values,
        rates: None,
        cor_offset: CorOffset::default(),
        enabled: true,
        recenter_generation: 0,
    })
    .unwrap();
}

fn install(data_home: &Path, lib: &Path) {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh");
    let out =
        std::process::Command::new("bash").arg(&script).arg(lib).env("XDG_DATA_HOME", data_home).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(data_home.join("openxr/1/api_layers/implicit.d/openrigcue_xr_layer.json").is_file());
    assert!(data_home.join("openrigcue/xr-layer/libopenrigcue_xr_layer.so").is_file());
}

#[test]
fn layer_runs_under_the_system_loader() {
    let Some(loader) = Lib::open("libopenxr_loader.so.1") else {
        eprintln!("libopenxr_loader.so.1 not installed; skipping");
        return;
    };
    let target = target_dir();
    // Release verification can exercise the exact packaged library; ordinary source tests still
    // use the library from their own build.
    let layer_lib = std::env::var_os("OPENRIGCUE_TEST_XR_LAYER_LIBRARY").map(PathBuf::from).unwrap_or_else(|| {
        find(&[target.join("deps/libopenrigcue_xr_layer.so"), target.join("libopenrigcue_xr_layer.so")])
    });
    let runtime_lib = find(&[target.join("examples/libfake_runtime.so")]);

    let tmp = tempfile::tempdir().unwrap();
    let data_home = tmp.path().join("data");
    install(&data_home, &layer_lib);
    let runtime_json = tmp.path().join("runtime.json");
    std::fs::write(
        &runtime_json,
        format!(
            r#"{{"file_format_version":"1.0.0","runtime":{{"name":"fake","library_path":"{}"}}}}"#,
            runtime_lib.display()
        ),
    )
    .unwrap();
    let pose_path = tmp.path().join("rig-pose");
    let mut writer = RigPoseWriter::create(&pose_path).unwrap();

    // This is the only test in this binary, so changing the environment cannot race another test.
    for (k, v) in [
        ("XDG_DATA_HOME", data_home.as_os_str()),
        ("XDG_CONFIG_HOME", tmp.path().join("config").as_os_str()),
        ("XR_RUNTIME_JSON", runtime_json.as_os_str()),
        ("OPENRIGCUE_RIG_POSE", pose_path.as_os_str()),
        ("OPENRIGCUE_XR_LAYER_ENABLE", "1".as_ref()),
    ] {
        std::env::set_var(k, v);
    }
    std::env::remove_var("XR_ENABLE_API_LAYERS");
    std::env::remove_var("OPENRIGCUE_XR_LAYER_DISABLE");

    // SAFETY: the loader's exported xrGetInstanceProcAddr has this signature.
    let gipa = unsafe { loader.sym::<pfn::GetInstanceProcAddr>(c"xrGetInstanceProcAddr") };
    let xr = Xr { gipa };
    let runtime = Lib::open(runtime_lib.to_str().unwrap()).unwrap();
    // SAFETY: the fake runtime's exported helpers have these signatures.
    let (set_head, submitted, timespec): (
        unsafe extern "C" fn(*const f32),
        unsafe extern "C" fn(*mut f32),
        extern "C" fn() -> bool,
    ) = unsafe {
        (
            runtime.sym(c"fake_runtime_set_head"),
            runtime.sym(c"fake_runtime_submitted"),
            runtime.sym(c"fake_runtime_timespec_enabled"),
        )
    };
    let half = 5f32.to_radians();
    let pitched = [0.0, 1.2, 0.0, half.sin(), 0.0, 0.0, half.cos()];
    let level = [0.0, 1.2, 0.0, 0.0, 0.0, 0.0, 1.0];

    {
        let app = App::start(&xr);
        assert!(timespec(), "the layer enabled XR_KHR_convert_timespec_time");
        // Calibrate at neutral, head level.
        // SAFETY: 7-float pose.
        unsafe { set_head(level.as_ptr()) };
        publish(&mut writer, RigValues::ZERO);
        app.wait_frame();
        // The rig pitches 10° nose up about the head (CoR offset 0): the runtime tracks a pitched head.
        // SAFETY: 7-float pose.
        unsafe { set_head(pitched.as_ptr()) };
        publish(&mut writer, RigValues { pitch: 10.0, ..RigValues::ZERO });
        let t = app.wait_frame();
        let head = app.head(t);
        assert!(head.orientation.x.abs() < 1e-5 && (head.orientation.w - 1.0).abs() < 1e-5, "compensated: {head:?}");
        assert!((head.position.y - 1.2).abs() < 1e-5, "{head:?}");
        // The application renders from the level head; the compositor gets the tracked (pitched) pose back.
        app.end_frame(t, head);
        let mut got = [0f32; 7];
        // SAFETY: 7-float buffer.
        unsafe { submitted(got.as_mut_ptr()) };
        assert!((got[3] - pitched[3]).abs() < 1e-5 && (got[6] - pitched[6]).abs() < 1e-5, "submitted {got:?}");
    }

    // Without the enable variable the loader does not load the layer at all.
    std::env::remove_var("OPENRIGCUE_XR_LAYER_ENABLE");
    {
        let app = App::start(&xr);
        assert!(!timespec(), "layer not loaded, so nothing added the extension");
        publish(&mut writer, RigValues { pitch: 10.0, ..RigValues::ZERO });
        let t = app.wait_frame();
        let head = app.head(t);
        assert!((head.orientation.x - pitched[3]).abs() < 1e-5, "uncompensated: {head:?}");
    }

    // The application's Install action deploys an always-on implicit layer. Verify that
    // registration mode through the real loader without a per-game enable variable.
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh");
    let out = std::process::Command::new("bash")
        .arg(script)
        .arg("--always-on")
        .arg(&layer_lib)
        .env("XDG_DATA_HOME", &data_home)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    {
        let app = App::start(&xr);
        assert!(timespec(), "an always-on deployment loads without the enable variable");
        // SAFETY: each array is a valid seven-float pose.
        unsafe { set_head(level.as_ptr()) };
        publish(&mut writer, RigValues::ZERO);
        app.wait_frame();
        // SAFETY: the array is a valid seven-float pose.
        unsafe { set_head(pitched.as_ptr()) };
        publish(&mut writer, RigValues { pitch: 10.0, ..RigValues::ZERO });
        let head = app.head(app.wait_frame());
        assert!(head.orientation.x.abs() < 1e-5 && (head.orientation.w - 1.0).abs() < 1e-5, "{head:?}");
    }
}
