//! Loads the built `.so` exactly as vrserver does (dlopen + dlsym("HmdDriverFactory")) and checks
//! the C++-ABI provider object it hands out. `Init` is not called here: the fake-vrserver test
//! drives the same code through the rlib.

use std::ffi::{c_char, c_void, CStr};
use std::path::PathBuf;

/// The `.so` built by this very `cargo test` run.
///
/// The library target is `crate-type = ["cdylib", "rlib"]`, so the one rustc invocation that
/// produces the rlib this test links against also writes `libopenrigcue_steamvr_driver.so` next to
/// it (cargo only copies it up to `<target>/<profile>/` for `cargo build`). Loading that copy means
/// plain `cargo test` on a fresh target dir works, and a stale `.so` from an earlier `cargo build`
/// is never the one tested.
///
/// Where that is depends on cargo's target layout: older cargo puts the test binary and the
/// library together in `<target>/<profile>/deps/`; newer cargo gives every unit its own
/// `<target>/<profile>/build/<package>/<hash>/out/`, so the library is in a sibling of the test
/// binary's folder. Of several library builds there, the newest is the one just built for this run.
fn cdylib_path() -> PathBuf {
    // Explicit release checks load the packaged artifact, independently of the source-test build.
    if let Some(path) = std::env::var_os("OPENRIGCUE_TEST_STEAMVR_LIBRARY") {
        return PathBuf::from(path);
    }
    const SO: &str = "libopenrigcue_steamvr_driver.so";
    const RLIB: &str = "libopenrigcue_steamvr_driver.rlib";
    let modified = |p: &PathBuf| {
        std::fs::metadata(p).and_then(|m| m.modified()).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    };
    let exe = std::env::current_exe().unwrap();
    let own = exe.parent().unwrap().to_path_buf();
    let mut folders = vec![own.clone()];
    // Per-unit layout: `<package>/<hash>/out/<test binary>`; the library's unit is `<package>/<other hash>/out/`.
    if let Some(package) = own.parent().and_then(|hash| hash.parent()) {
        if let Ok(units) = std::fs::read_dir(package) {
            folders.extend(units.flatten().map(|unit| unit.path().join("out")));
        }
    }
    let deps = folders
        .into_iter()
        .filter(|folder| folder.join(SO).is_file() && folder.join(RLIB).is_file())
        .max_by_key(|folder| modified(&folder.join(RLIB)))
        .unwrap_or_else(|| panic!("{SO} and {RLIB} not found beside or near {}", exe.display()));
    let so = deps.join(SO);
    let rlib = deps.join(RLIB);
    let (so_time, rlib_time) = (modified(&so), modified(&rlib));
    // Same rustc invocation: the .so is written within moments of the rlib, never long before.
    assert!(
        so_time + std::time::Duration::from_secs(5) >= rlib_time,
        "{} is older than the rlib this test links; it is stale",
        so.display()
    );
    so
}

type Factory = unsafe extern "C" fn(*const c_char, *mut i32) -> *mut c_void;

#[test]
fn the_shared_library_exports_the_driver_factory() {
    let path = cdylib_path();
    assert!(path.exists(), "{} not built", path.display());
    let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    // SAFETY: loading our own library; its initialisers have no side effects.
    let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    assert!(!handle.is_null(), "dlopen failed");
    // SAFETY: valid handle and NUL-terminated symbol name.
    let symbol = unsafe { libc::dlsym(handle, c"HmdDriverFactory".as_ptr()) };
    assert!(!symbol.is_null(), "HmdDriverFactory not exported");
    // SAFETY: the symbol is `extern "C" fn(const char*, int*) -> void*`.
    let factory: Factory = unsafe { std::mem::transmute::<*mut c_void, Factory>(symbol) };

    let mut rc = 0;
    // SAFETY: arguments as vrserver passes them.
    let missing = unsafe { factory(c"IServerTrackedDeviceProvider_003".as_ptr(), &mut rc) };
    assert!(missing.is_null());
    assert_eq!(rc, 105, "VRInitError_Init_InterfaceNotFound");

    // SAFETY: as above.
    let provider = unsafe { factory(c"IServerTrackedDeviceProvider_004".as_ptr(), &mut rc) };
    assert!(!provider.is_null());
    // SAFETY: Itanium layout: vptr at offset 0; slots GetInterfaceVersions (2), RunFrame (3),
    // ShouldBlockStandbyMode (4), EnterStandby (5), LeaveStandby (6). None of these needs Init.
    unsafe {
        let vtable = *(provider as *const *const usize);
        // offset-to-top word just before the address point, as a C++ compiler emits it.
        assert_eq!(*vtable.sub(2), 0);
        let versions: unsafe extern "C" fn(*mut c_void) -> *const *const c_char = std::mem::transmute(*vtable.add(2));
        let mut list = versions(provider);
        let mut names = Vec::new();
        while !(*list).is_null() {
            names.push(CStr::from_ptr(*list).to_str().unwrap().to_owned());
            list = list.add(1);
        }
        assert!(names.contains(&"IServerTrackedDeviceProvider_004".to_owned()), "{names:?}");
        let run_frame: unsafe extern "C" fn(*mut c_void) = std::mem::transmute(*vtable.add(3));
        run_frame(provider);
        let block: unsafe extern "C" fn(*mut c_void) -> bool = std::mem::transmute(*vtable.add(4));
        assert!(!block(provider));
        let enter: unsafe extern "C" fn(*mut c_void) = std::mem::transmute(*vtable.add(5));
        enter(provider);
        let leave: unsafe extern "C" fn(*mut c_void) = std::mem::transmute(*vtable.add(6));
        leave(provider);
    }
}
