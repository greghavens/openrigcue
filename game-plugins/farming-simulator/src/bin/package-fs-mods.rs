//! `package-fs-mods <out-dir>`: write FS19/FS22/FS25_TelemetryMod.zip.
//!
//! The output directory is the folder from which the game-patch step copies
//! a zip into `<Documents>\My Games\FarmingSimulator20xx\mods\`.

use std::path::PathBuf;
use std::process::ExitCode;

use openrigcue_fs_mods::package::{build_zip, EDITIONS};

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let (Some(out), None) = (args.next(), args.next()) else {
        eprintln!("usage: package-fs-mods <out-dir>");
        return ExitCode::from(2);
    };
    let out = PathBuf::from(out);
    if let Err(e) = std::fs::create_dir_all(&out) {
        eprintln!("cannot create {}: {e}", out.display());
        return ExitCode::FAILURE;
    }
    for edition in EDITIONS {
        let path = out.join(edition.zip_name());
        let bytes = build_zip(edition);
        if let Err(e) = std::fs::write(&path, &bytes) {
            eprintln!("cannot write {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
        println!("{} ({} bytes)", path.display(), bytes.len());
    }
    ExitCode::SUCCESS
}
