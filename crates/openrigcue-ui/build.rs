//! Embeds the interface's style sheet, script, fonts and OpenRigCue's authored images at `/res:…`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> =
        std::fs::read_dir(dir).expect("res folder").map(|e| e.expect("res entry").path()).collect();
    entries.sort();
    for path in entries {
        // Editable artwork is retained beside the runtime resources, but only
        // its rendered derivatives belong in the application binary.
        if path.is_dir() && path.file_name().is_some_and(|name| name == "source") {
            continue;
        }
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let res = manifest.join("res");
    println!("cargo:rerun-if-changed={}", res.display());
    let mut files = Vec::new();
    walk(&res, &mut files);
    let mut code = String::from(
        "/// Runtime files of `res/` (excluding editable `source/` artwork).\npub const RES: &[(&str, &[u8])] = &[\n",
    );
    for file in &files {
        println!("cargo:rerun-if-changed={}", file.display());
        let relative = file.strip_prefix(&res).expect("below res").to_string_lossy().replace('\\', "/");
        writeln!(code, "    ({relative:?}, include_bytes!({:?})),", file.display().to_string()).expect("write");
    }
    code.push_str("];\n");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("out dir")).join("res_files.rs");
    std::fs::write(out, code).expect("generated res table");
}
