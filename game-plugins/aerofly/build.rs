//! Windows target only: link the DLL without C run-time start files (the
//! entry point is `scrt.rs`'s `DllMainCRTStartup`), with fixed
//! PE header values (subsystem 2 = Windows GUI 6.0, OS version 6.0, stack and
//! heap reserve/commit 0x100000/0x1000), and with its RT_MANIFEST resource
//! (res/manifest.xml, compiled by windres).
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=res/aerofly.rc");
    println!("cargo:rerun-if-changed=res/manifest.xml");
    let target = env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        return;
    }
    for arg in [
        "-nostartfiles",
        "-Wl,--entry,DllMainCRTStartup",
        // keep the load configuration (nothing references it; ld fills the
        // LOAD_CONFIG directory from this symbol)
        "-Wl,--undefined,_load_config_used",
        "-Wl,--subsystem,windows:6.0",
        "-Wl,--major-os-version,6",
        "-Wl,--minor-os-version,0",
        "-Wl,--stack,0x100000", // commit stays ld's default 0x1000
        "-Wl,--heap,0x100000",
    ] {
        println!("cargo:rustc-cdylib-link-arg={arg}");
    }
    // GNU ld does not fill the LOAD_CONFIG data directory; lld (MinGW mode)
    // takes it from `_load_config_used` as link.exe does. The toolchain's own
    // rust-lld is used through its gcc-ld wrapper.
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let sysroot = Command::new(&rustc).args(["--print", "sysroot"]).output().expect("rustc --print sysroot");
    let sysroot = String::from_utf8(sysroot.stdout).expect("utf-8 sysroot");
    let host = env::var("HOST").expect("HOST");
    let gcc_ld = PathBuf::from(sysroot.trim()).join("lib/rustlib").join(&host).join("bin/gcc-ld");
    assert!(gcc_ld.join("ld.lld").exists(), "rust-lld gcc-ld wrapper not found at {}", gcc_ld.display());
    println!("cargo:rustc-cdylib-link-arg=-B{}", gcc_ld.display());
    println!("cargo:rustc-cdylib-link-arg=-fuse-ld=lld");
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let obj = out.join("aerofly_rsrc.o");
    let windres = env::var("WINDRES").unwrap_or_else(|_| "x86_64-w64-mingw32-windres".to_string());
    let status = Command::new(&windres)
        .args(["--input-format=rc", "--output-format=coff", "-I", "res", "-i", "res/aerofly.rc", "-o"])
        .arg(&obj)
        .status()
        .expect("run windres");
    assert!(status.success(), "windres failed");
    println!("cargo:rustc-cdylib-link-arg={}", obj.display());
}
