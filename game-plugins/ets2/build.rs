// Link settings of the plugin DLL: its export table, start-up and header.
use std::env;

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    if target.ends_with("-pc-windows-gnu") {
        // Only scs_telemetry_init / scs_telemetry_shutdown are exported
        // (ordinals 1, 2), under their bare names, from a DLL whose export
        // directory is named ets2-telemetry.dll in both builds.
        let def = if target.starts_with("i686") { "ets2-telemetry_32.def" } else { "ets2-telemetry.def" };
        let dir = env::var("CARGO_MANIFEST_DIR").unwrap();
        println!("cargo:rerun-if-changed={def}");
        println!("cargo:rustc-cdylib-link-arg={dir}/{def}");
        if target.starts_with("i686") {
            // stdcall exports without the @N decoration (the names the game looks up);
            // the .def names the decorated symbols and ld resolves the bare
            // names to them (stdcall fixup), which is wanted, not a warning.
            println!("cargo:rustc-cdylib-link-arg=-Wl,--kill-at");
            println!("cargo:rustc-cdylib-link-arg=-Wl,--enable-stdcall-fixup");
        }
        // The release DLL (no `std` feature, no debug assertions: the cfg
        // under which src/crt.rs is compiled) uses the CRT
        // start-up from crt.rs instead of mingw-w64's: no mingw start files,
        // its own entry point, and its own load-configuration directory.
        let crt = env::var_os("CARGO_FEATURE_STD").is_none() && env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_none();
        if crt {
            let i686 = target.starts_with("i686");
            println!("cargo:rustc-cdylib-link-arg=-nostartfiles");
            // Section layout (.text .rdata .data [.pdata] .rsrc .reloc).
            println!("cargo:rerun-if-changed=ets2-telemetry.ld");
            println!("cargo:rustc-cdylib-link-arg=-Wl,-T,{dir}/ets2-telemetry.ld");
            if i686 {
                println!("cargo:rustc-cdylib-link-arg=-Wl,--entry=__DllMainCRTStartup@12");
                println!("cargo:rustc-cdylib-link-arg=-Wl,--require-defined=__load_config_used");
            } else {
                println!("cargo:rustc-cdylib-link-arg=-Wl,--entry=_DllMainCRTStartup");
                println!("cargo:rustc-cdylib-link-arg=-Wl,--require-defined=_load_config_used");
            }
            // The header fields and data directories ld cannot produce (time
            // stamps, characteristics, linker version, zero checksum; export,
            // import, IAT, debug and load-configuration directories inside
            // .rdata) are set after the link by scripts/pe-link.sh, which gcc
            // runs the linker through.
            println!("cargo:rerun-if-changed=scripts/pe-link.sh");
            println!("cargo:rustc-cdylib-link-arg=-wrapper");
            println!("cargo:rustc-cdylib-link-arg=sh,{dir}/scripts/pe-link.sh");
            if i686 {
                // Characteristics 0x2102: not large-address-aware.
                println!("cargo:rustc-cdylib-link-arg=-Wl,--disable-large-address-aware");
            }
            // Optional header: GUI subsystem 6.0, OS
            // version 6.0, image version 0.0, 1 MiB / 4 KiB stack and heap.
            for arg in [
                "--subsystem=windows:6.0",
                "--major-os-version=6",
                "--minor-os-version=0",
                "--major-image-version=0",
                "--minor-image-version=0",
                "--stack=0x100000,0x1000",
                "--heap=0x100000,0x1000",
            ] {
                // -Xlinker: these contain commas, which -Wl would split.
                println!("cargo:rustc-cdylib-link-arg=-Xlinker");
                println!("cargo:rustc-cdylib-link-arg={arg}");
            }
            // The RT_MANIFEST resource is src/rsrc.s, which includes the
            // manifest with .incbin (a dependency cargo does not see).
            println!("cargo:rerun-if-changed=res/ets2-telemetry.manifest");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}
