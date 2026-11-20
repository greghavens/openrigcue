use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn directories(path: &Path, all: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(path).unwrap().map(Result::unwrap) {
        let p = entry.path();
        if p.is_dir() {
            all.push(p.clone());
            directories(&p, all);
        }
    }
}
fn native_source(from: &Path, to: &Path, relative: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap().map(Result::unwrap) {
        let rel = relative.join(entry.file_name());
        let dst = to.join(entry.file_name());
        if entry.path().is_dir() {
            native_source(&entry.path(), &dst, &rel);
            continue;
        }
        if entry.path().extension().is_some_and(|x| x == "c" || x == "h") {
            let original = format!(
                ".pio\\libdeps\\wemos_d1_mini32\\NimBLE-Arduino\\src\\{}",
                rel.to_string_lossy().replace('/', "\\")
            );
            let directive = format!("#line 1 {:?}\n", original);
            fs::write(dst, directive + &fs::read_to_string(entry.path()).unwrap()).unwrap();
        } else {
            fs::copy(entry.path(), dst).unwrap();
        }
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=OPENRIGCUE_IDF33_ROOT");
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=../tools/esp32-runtime.sh");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tools/esp32-runtime.sh");
    let output = Command::new("bash").arg(&script).output().unwrap();
    if !output.status.success() {
        panic!("Historical runtime preparation: {}", String::from_utf8_lossy(&output.stderr));
    }
    let runtime = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    let sdk = runtime.join("esp32-1.0.6/tools/sdk");
    let compiler = runtime.join("xtensa-esp32-elf/bin/xtensa-esp32-elf-gcc");
    let original_nimble = runtime.join("NimBLE-Arduino/src");
    let ble = env::var_os("CARGO_FEATURE_NIMBLE").is_some();
    let nimble = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("native-nimble");
    if ble {
        native_source(&original_nimble, &nimble, Path::new(""));
    }
    let mut includes = fs::read_dir(sdk.join("include"))
        .unwrap()
        .map(Result::unwrap)
        .map(|x| x.path())
        .filter(|x| x.is_dir())
        .collect::<Vec<_>>();
    includes.push(runtime.join("xtensa-esp32-elf/xtensa-esp32-elf/include"));
    includes.push(runtime.join("xtensa-esp32-elf/lib/gcc/xtensa-esp32-elf/5.2.0/include"));
    includes.push(runtime.join("xtensa-esp32-elf/lib/gcc/xtensa-esp32-elf/5.2.0/include-fixed"));
    if ble {
        includes.push(nimble.clone());
        let mut dirs = Vec::new();
        directories(&nimble, &mut dirs);
        includes.extend(dirs.into_iter().filter(|x| x.file_name().is_some_and(|n| n == "include")));
    }
    let mut args = vec![
        "--target=xtensa-esp32-none-elf".to_owned(),
        "-nostdinc".into(),
        "-DESP_PLATFORM".into(),
        "-DARDUINO_ARCH_ESP32".into(),
        "-DARDUINO=10607".into(),
        "-DHAVE_CONFIG_H".into(),
        "-DWITH_POSIX".into(),
        "-DGCC_NOT_5_2_0=0".into(),
    ];
    args.extend(includes.iter().map(|p| format!("-I{}", p.display())));
    if ble {
        args.push("-DOPENRIGCUE_NIMBLE".into());
    }
    let bindings=bindgen::Builder::default().header("wrapper.h").clang_args(&args)
        .use_core().ctypes_prefix("core::ffi").derive_default(true).generate_comments(false)
        .layout_tests(false).prepend_enum_name(true)
        .allowlist_function("(esp_.*|nvs_.*|gpio_.*|adc.*|xQueue.*|vQueue.*|uxQueue.*|xTask.*|vTask.*|vPort.*|ets_.*|ble_.*|nimble_.*|os_mbuf_.*|_esp_.*|__assert_func|abort|printf|malloc|memalign|free|realloc)")
        .allowlist_type("(QueueHandle_t|TaskHandle_t|portMUX_TYPE|esp_.*|ble_.*|os_mbuf.*|adc.*|gpio_.*|nvs_.*|periph_interrupt_t)")
        .allowlist_var("(ESP_.*|ETS_.*|BLE_.*|CONFIG_.*|BT_.*|BTDM_.*|SCAN_.*|MESH_.*|ble_hs_cfg)")
        .generate().expect("Generate bindings from the actual pinned IDF3.3 headers");
    bindings.write_to_file(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("bindings.rs")).unwrap();
    if ble {
        let mut dirs = vec![nimble.clone()];
        directories(&nimble, &mut dirs);
        let files = dirs
            .into_iter()
            .flat_map(|p| fs::read_dir(p).unwrap().map(Result::unwrap).map(|x| x.path()).collect::<Vec<_>>())
            .filter(|p| p.extension().is_some_and(|x| x == "c"))
            .collect::<Vec<_>>();
        let mut build = cc::Build::new();
        build
            .compiler(&compiler)
            .includes(&includes)
            .files(&files)
            .opt_level_str("s")
            .flag("-std=gnu99")
            .flag("-fstack-protector")
            .flag("-ffunction-sections")
            .flag("-fdata-sections")
            .flag("-fstrict-volatile-bitfields")
            .flag("-mlongcalls")
            .flag("-nostdlib")
            .define("ESP_PLATFORM", None)
            .define("ARDUINO_ARCH_ESP32", None)
            .define("ARDUINO", "10607")
            .define("HAVE_CONFIG_H", None)
            .define("WITH_POSIX", None)
            .define("GCC_NOT_5_2_0", "0")
            .warnings(false)
            .compile("orc_idf33_nimble");
    }
    println!("cargo:rustc-link-search=native={}", sdk.join("lib").display());
    println!("cargo:runtime={}", runtime.display());
}
