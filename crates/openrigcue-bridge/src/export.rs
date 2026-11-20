//! Publishing openrigcue-written Linux files as Windows sections inside the prefix (the reverse of [`crate::mirror`]).
//!
//! `Local\SimRacingStudioMotionRigPose` and `YawVRGEFile` are published for other Windows programs
//! (catalog [`crate::catalog::EXPORTS`]). On Linux openrigcue writes `<mirror dir>/<flattened name>`; while that file
//! exists the bridge holds a section of the structure's size ([`ExportSpec::size`]) under the
//! Windows name and copies the file into it (a shorter file leaves the rest zero, a longer one is cut).

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::catalog::ExportSpec;
use crate::paths::mirror_file_name;

/// Access to Windows sections the bridge owns (Win32 inside Wine; fakes in tests).
pub trait SectionPublisher {
    /// Make section `name` contain `bytes`, creating it with `bytes.len()` bytes first if needed.
    fn publish(&mut self, name: &'static str, bytes: &[u8]) -> io::Result<()>;
    /// Close the bridge's handle to section `name`.
    fn withdraw(&mut self, name: &'static str);
}

struct ExportState {
    spec: ExportSpec,
    active: bool,
}

/// The export loop body: every export's file is read on every tick.
pub struct Exports {
    dir: PathBuf,
    states: Vec<ExportState>,
    buf: Vec<u8>,
}

impl Exports {
    pub fn new(dir: impl Into<PathBuf>, specs: &[ExportSpec]) -> Exports {
        Exports {
            dir: dir.into(),
            states: specs.iter().map(|&spec| ExportState { spec, active: false }).collect(),
            buf: Vec::new(),
        }
    }

    /// One export pass: each section holds openrigcue's latest file content.
    pub fn tick(&mut self, publisher: &mut impl SectionPublisher) {
        for state in &mut self.states {
            let read = read_file(&self.dir.join(mirror_file_name(state.spec.name)), &mut self.buf);
            match read.map(|()| self.buf.resize(state.spec.size, 0)) {
                Ok(()) => match publisher.publish(state.spec.name, &self.buf) {
                    Ok(()) => {
                        if !state.active {
                            tracing::info!(section = state.spec.name, bytes = state.spec.size, "exporting");
                        }
                        state.active = true;
                    }
                    Err(error) => {
                        tracing::warn!(section = state.spec.name, %error, "export failed");
                        publisher.withdraw(state.spec.name);
                        state.active = false;
                    }
                },
                Err(_) if state.active => {
                    tracing::info!(section = state.spec.name, "export source gone");
                    publisher.withdraw(state.spec.name);
                    state.active = false;
                }
                Err(_) => {}
            }
        }
    }

    /// Release every exported section.
    pub fn finish(&mut self, publisher: &mut impl SectionPublisher) {
        for state in self.states.iter_mut().filter(|s| s.active) {
            publisher.withdraw(state.spec.name);
            state.active = false;
        }
    }
}

fn read_file(path: &Path, buf: &mut Vec<u8>) -> io::Result<()> {
    buf.clear();
    File::open(path)?.read_to_end(buf)?;
    if buf.is_empty() {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;

    #[derive(Default)]
    struct FakeWindows {
        sections: HashMap<&'static str, Vec<u8>>,
    }

    impl SectionPublisher for FakeWindows {
        fn publish(&mut self, name: &'static str, bytes: &[u8]) -> io::Result<()> {
            self.sections.insert(name, bytes.to_vec());
            Ok(())
        }
        fn withdraw(&mut self, name: &'static str) {
            self.sections.remove(name);
        }
    }

    /// The pose section appears on the first tick after openrigcue writes its file, follows every
    /// change, and goes away with the file.
    #[test]
    fn pose_file_is_published_and_withdrawn() {
        let root = std::env::temp_dir().join(format!("openrigcue-bridge-export-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("Local_SimRacingStudioMotionRigPose");
        let mut windows = FakeWindows::default();
        let mut exports = Exports::new(&root, &crate::catalog::EXPORTS[..1]);

        exports.tick(&mut windows);
        assert!(windows.sections.is_empty());

        let pose: Vec<u8> = [1.0f64, 2.0, 3.0, 4.0, 5.0, 6.0].iter().flat_map(|v| v.to_le_bytes()).collect();
        fs::write(&file, &pose).unwrap();
        exports.tick(&mut windows);
        assert_eq!(windows.sections["Local\\SimRacingStudioMotionRigPose"], pose);

        fs::remove_file(&file).unwrap();
        exports.tick(&mut windows);
        assert!(windows.sections.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    /// `YawVRGEFile` (17 bytes, byte-packed) is published under its unflattened name, zeroed
    /// first, then with each report; a file of another length still gives the 17-byte section.
    #[test]
    fn yaw_vr_block_is_published_with_its_17_bytes() {
        let root = std::env::temp_dir().join(format!("openrigcue-bridge-export-yaw-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let file = root.join(mirror_file_name("YawVRGEFile"));
        assert_eq!(file.file_name().unwrap(), "YawVRGEFile");
        let mut windows = FakeWindows::default();
        let mut exports = Exports::new(&root, crate::catalog::EXPORTS);

        fs::write(&file, [0u8; 17]).unwrap();
        exports.tick(&mut windows);
        assert_eq!(windows.sections["YawVRGEFile"], [0u8; 17]);

        let mut block: Vec<u8> = [12.5f32, -3.0, 0.25, 87.0].iter().flat_map(|v| v.to_le_bytes()).collect();
        block.push(1);
        fs::write(&file, &block).unwrap();
        exports.tick(&mut windows);
        assert_eq!(windows.sections["YawVRGEFile"], block);
        assert!(!windows.sections.contains_key("Local\\SimRacingStudioMotionRigPose"));

        fs::write(&file, [7u8; 4]).unwrap();
        exports.tick(&mut windows);
        assert_eq!(windows.sections["YawVRGEFile"], [7, 7, 7, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        fs::write(&file, [9u8; 20]).unwrap();
        exports.tick(&mut windows);
        assert_eq!(windows.sections["YawVRGEFile"], [9u8; 17]);
        fs::remove_dir_all(root).unwrap();
    }
}
