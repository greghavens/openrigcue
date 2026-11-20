//! Copying sections into Linux mirror files.
//!
//! Mirror format (read by `openrigcue_sources::shared_memory::mapping::MappedView`): one regular file per section in
//! [`crate::paths::MIRROR_DIR`], named by [`crate::paths::mirror_file_name`], holding the section's bytes from
//! offset 0. Every copy rewrites the whole file in place with one positional write, and a mirror file, once
//! created, is never removed or replaced, so a reader that keeps the file open always sees the latest copy, also
//! across game restarts and bridge runs.
//!
//! **A missing or empty (length 0) mirror means the section does not exist**: a listener's open of it fails.
//! A section disappears with the game unless something still holds it: its mirror is then truncated to length 0
//! and refilled in place when the section appears again. A section the bridge holds for OpenRigCue (the
//! application's own mapping objects, see [`crate::control`]) stays alive with the game's last frame, and so does
//! its mirror.
//!
//! A target with a data-ready event also gets an event file (see [`crate::event`]).

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::PathBuf;

use crate::paths::mirror_file_name;
use crate::plan::MirrorTarget;

/// Access to the live sections (Win32 inside Wine; fakes in tests).
pub trait SectionReader {
    /// Copy the current bytes of `target` into `buf` (resized to the mapped view), or return `false` when the
    /// section does not exist or none of the target's sizes can be mapped. This is the snapshot a
    /// listener takes per poll: open the named mapping, map a view and copy it.
    /// A section OpenRigCue holds is copied from the handle the bridge keeps for it.
    fn read(&mut self, target: &MirrorTarget, buf: &mut Vec<u8>) -> bool;

    /// Whether the bridge holds `name` for OpenRigCue (its mirror is never marked absent).
    fn is_held(&self, name: &str) -> bool;
}

/// The directory of mirror files, with the files currently being written kept open.
pub struct MirrorDir {
    dir: PathBuf,
    open: HashMap<String, File>,
}

impl MirrorDir {
    pub fn new(dir: impl Into<PathBuf>) -> MirrorDir {
        MirrorDir { dir: dir.into(), open: HashMap::new() }
    }

    pub fn path_of(&self, windows_name: &str) -> PathBuf {
        self.dir.join(mirror_file_name(windows_name))
    }

    /// Write `bytes` as the whole content of the mirror of `windows_name`, creating it if needed.
    pub fn publish(&mut self, windows_name: &str, bytes: &[u8]) -> io::Result<()> {
        let file = match self.open.entry(windows_name.to_owned()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                fs::create_dir_all(&self.dir)?;
                let path = self.dir.join(mirror_file_name(windows_name));
                entry.insert(OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?)
            }
        };
        let len = bytes.len() as u64;
        if file.metadata()?.len() > len {
            file.set_len(len)?;
        }
        write_all_at(file, bytes)
    }

    /// The current content of the mirror of `windows_name`, or `None` when there is none.
    pub fn last_copy(&self, windows_name: &str) -> io::Result<Option<Vec<u8>>> {
        let mut bytes = Vec::new();
        match File::open(self.path_of(windows_name)) {
            Ok(mut file) => file.read_to_end(&mut bytes).map(|_| Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Mark the section of `windows_name` absent: truncate its mirror to length 0, keeping the file (and so the
    /// inode readers hold open) for the next [`MirrorDir::publish`]. A missing mirror is left missing.
    pub fn withdraw(&mut self, windows_name: &str) -> io::Result<()> {
        if let Some(file) = self.open.get(windows_name) {
            return file.set_len(0);
        }
        match OpenOptions::new().write(true).open(self.path_of(windows_name)) {
            Ok(file) => file.set_len(0),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(unix)]
fn write_all_at(file: &File, bytes: &[u8]) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, bytes, 0)
}

#[cfg(windows)]
fn write_all_at(file: &File, bytes: &[u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < bytes.len() {
        match file.seek_write(&bytes[done..], done as u64)? {
            0 => return Err(io::ErrorKind::WriteZero.into()),
            n => done += n,
        }
    }
    Ok(())
}

struct TargetState {
    target: MirrorTarget,
    present: bool,
}

/// The mirroring loop body: every target is read on every tick.
pub struct Mirror {
    targets: Vec<TargetState>,
    buf: Vec<u8>,
}

impl Mirror {
    pub fn new(targets: Vec<MirrorTarget>) -> Mirror {
        Mirror {
            targets: targets.into_iter().map(|target| TargetState { target, present: false }).collect(),
            buf: Vec::new(),
        }
    }

    /// Mirror `name` from now on (a section OpenRigCue holds or creates, which may lie outside the plan), with
    /// `size` among its view sizes.
    pub fn ensure(&mut self, name: &str, size: usize) {
        match self.targets.iter_mut().find(|state| state.target.name == name) {
            Some(state) => state.target.add_size(size),
            None => self.targets.push(TargetState { target: MirrorTarget::sized(name, size), present: false }),
        }
    }

    /// Start a run over what earlier runs left in `dir`: every mirror of `names` (every section any listener
    /// reads, also those outside this run's plan: a run that was killed may have mirrored another game's) is
    /// marked absent. The sections OpenRigCue's listeners hold are created again from the content OpenRigCue
    /// kept, and mirrored as soon as they are held.
    pub fn prepare(&mut self, dir: &mut MirrorDir, names: &[&str]) {
        for &name in names {
            log_failure("clear stale mirror", name, dir.withdraw(name));
        }
    }

    /// One mirroring pass.
    pub fn tick(&mut self, reader: &mut impl SectionReader, dir: &mut MirrorDir) {
        for index in 0..self.targets.len() {
            self.copy(index, reader, dir);
        }
    }

    /// Copy `name` now (after a hold, or after its data-ready event was signalled).
    pub fn refresh(&mut self, name: &str, reader: &mut impl SectionReader, dir: &mut MirrorDir) {
        if let Some(index) = self.targets.iter().position(|state| state.target.name == name) {
            self.copy(index, reader, dir);
        }
    }

    /// Whether `name` was found by its last copy.
    pub fn is_present(&self, name: &str) -> bool {
        self.targets.iter().any(|state| state.target.name == name && state.present)
    }

    fn copy(&mut self, index: usize, reader: &mut impl SectionReader, dir: &mut MirrorDir) {
        let state = &mut self.targets[index];
        let name = state.target.name.as_str();
        if reader.read(&state.target, &mut self.buf) {
            if !state.present {
                tracing::debug!(section = name, bytes = self.buf.len(), "section found, mirroring");
            }
            state.present = true;
            log_failure("write mirror", name, dir.publish(name, &self.buf));
        } else if state.present && !reader.is_held(name) {
            state.present = false;
            tracing::debug!(section = name, "section gone, marking its mirror absent");
            log_failure("withdraw mirror", name, dir.withdraw(name));
        }
    }

    /// End the run: the sections the prefix had disappear with it, so their mirrors are marked absent. The
    /// mirrors of sections OpenRigCue holds keep their last copy, which OpenRigCue takes as the content of its
    /// object once the control connection ends.
    pub fn finish(&mut self, reader: &impl SectionReader, dir: &mut MirrorDir) {
        for state in self.targets.iter_mut().filter(|s| s.present) {
            if reader.is_held(&state.target.name) {
                continue;
            }
            state.present = false;
            log_failure("withdraw mirror", &state.target.name, dir.withdraw(&state.target.name));
        }
    }
}

fn log_failure(action: &str, section: &str, result: io::Result<()>) {
    if let Err(error) = result {
        tracing::warn!(section, %error, "{action} failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    /// Fake sections: name → current bytes; reads per name are counted; `held` names are the bridge's holds.
    #[derive(Default)]
    struct FakeSections {
        live: HashMap<String, Vec<u8>>,
        reads: HashMap<String, usize>,
        held: HashSet<String>,
    }

    impl SectionReader for FakeSections {
        fn read(&mut self, target: &MirrorTarget, buf: &mut Vec<u8>) -> bool {
            *self.reads.entry(target.name.clone()).or_default() += 1;
            match self.live.get(&target.name) {
                Some(bytes) => {
                    buf.clear();
                    buf.extend_from_slice(bytes);
                    true
                }
                None => false,
            }
        }

        fn is_held(&self, name: &str) -> bool {
            self.held.contains(name)
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("openrigcue-bridge-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// Every tick copies the section.
    #[test]
    fn present_section_is_copied_every_tick() {
        let root = temp_dir("present");
        let mut dir = MirrorDir::new(&root);
        let mut fake = FakeSections::default();
        fake.live.insert("Local\\ww1".into(), vec![1; 24]);
        let mut mirror = Mirror::new(vec![MirrorTarget::sized("Local\\ww1", 24)]);
        mirror.tick(&mut fake, &mut dir);
        assert_eq!(fs::read(root.join("Local_ww1")).unwrap(), vec![1; 24]);
        fake.live.insert("Local\\ww1".into(), vec![2; 24]);
        mirror.tick(&mut fake, &mut dir);
        assert_eq!(fs::read(root.join("Local_ww1")).unwrap(), vec![2; 24]);
        assert_eq!(fake.reads["Local\\ww1"], 2);
        fs::remove_dir_all(root).unwrap();
    }

    /// A missing section is probed every tick, and nothing is created for it.
    #[test]
    fn absent_section_is_probed_every_tick() {
        let root = temp_dir("absent");
        let mut dir = MirrorDir::new(&root);
        let mut fake = FakeSections::default();
        let mut mirror = Mirror::new(vec![MirrorTarget::sized("$R3E", 43_996)]);
        for _ in 0..3 {
            mirror.tick(&mut fake, &mut dir);
        }
        assert_eq!(fake.reads["$R3E"], 3);
        assert!(!root.join("$R3E").exists());
    }

    /// A section nothing holds goes with the game and the next open fails: the mirror becomes empty (length 0),
    /// and the same file (the inode a reader holds) receives the next frame once the game is back.
    #[cfg(unix)]
    #[test]
    fn vanished_section_is_emptied_then_refilled_in_place() {
        use std::os::unix::fs::MetadataExt;
        let root = temp_dir("vanish");
        let mut dir = MirrorDir::new(&root);
        let mut fake = FakeSections::default();
        fake.live.insert("CLODDeviceLink".into(), vec![7; 64]);
        let mut mirror = Mirror::new(vec![MirrorTarget::sized("CLODDeviceLink", 64)]);
        mirror.tick(&mut fake, &mut dir);
        let held_open = File::open(root.join("CLODDeviceLink")).unwrap();
        let inode = held_open.metadata().unwrap().ino();

        fake.live.clear();
        mirror.tick(&mut fake, &mut dir);
        assert_eq!(held_open.metadata().unwrap().len(), 0);
        assert_eq!(fs::metadata(root.join("CLODDeviceLink")).unwrap().ino(), inode);

        fake.live.insert("CLODDeviceLink".into(), vec![9; 64]);
        mirror.tick(&mut fake, &mut dir);
        let mut seen = vec![0; 64];
        std::os::unix::fs::FileExt::read_exact_at(&held_open, &mut seen, 0).unwrap();
        assert_eq!(seen, vec![9; 64]);
        fs::remove_dir_all(root).unwrap();
    }

    /// A held section: while the bridge holds the section for OpenRigCue its mirror keeps the
    /// last frame, also when a copy fails and after the run.
    #[test]
    fn held_section_keeps_its_last_frame() {
        let root = temp_dir("held");
        let mut dir = MirrorDir::new(&root);
        let mut fake = FakeSections::default();
        fake.live.insert("SimBin".into(), vec![7; 180]);
        fake.held.insert("SimBin".into());
        let mut mirror = Mirror::new(Vec::new());
        mirror.ensure("SimBin", 180);
        mirror.tick(&mut fake, &mut dir);
        fake.live.clear();
        mirror.tick(&mut fake, &mut dir);
        mirror.finish(&fake, &mut dir);
        assert_eq!(fs::read(root.join("SimBin")).unwrap(), vec![7; 180]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn smaller_copy_shrinks_the_file() {
        let root = temp_dir("shrink");
        let mut dir = MirrorDir::new(&root);
        dir.publish("$pcars2$", &[1; 11708]).unwrap();
        dir.publish("$pcars2$", &[2; 11624]).unwrap();
        assert_eq!(fs::read(root.join("$pcars2$")).unwrap(), vec![2; 11624]);
        assert_eq!(dir.last_copy("$pcars2$").unwrap(), Some(vec![2; 11624]));
        assert_eq!(dir.last_copy("$pcars$").unwrap(), None);
        fs::remove_dir_all(root).unwrap();
    }

    /// A run starts by emptying every mirror an earlier run may have left (any game's), and ends by emptying the
    /// mirrors of the sections that go with the prefix; the held ones keep their last copy. Nothing missing is
    /// created.
    #[test]
    fn prepare_and_finish_empty_what_the_prefix_owned() {
        let root = temp_dir("finish");
        let mut earlier = MirrorDir::new(&root);
        earlier.publish("acpmf_static", &[3; 452]).unwrap();
        earlier.publish("CLODDeviceLink", &[3; 16]).unwrap();
        let len = |name: &str| fs::metadata(root.join(name)).map(|m| m.len()).ok();

        let mut dir = MirrorDir::new(&root);
        let mut mirror = Mirror::new(vec![MirrorTarget::sized("acevo_pmf_physics", 800)]);
        mirror.prepare(&mut dir, &["acpmf_static", "acevo_pmf_physics", "CLODDeviceLink"]);
        assert_eq!((len("acpmf_static"), len("CLODDeviceLink"), len("acevo_pmf_physics")), (Some(0), Some(0), None));

        let mut fake = FakeSections::default();
        fake.live.insert("acevo_pmf_physics".into(), vec![4; 800]);
        fake.live.insert("acpmf_static".into(), vec![5; 452]);
        fake.held.insert("acpmf_static".into());
        mirror.ensure("acpmf_static", 452);
        mirror.tick(&mut fake, &mut dir);
        assert_eq!((len("acevo_pmf_physics"), len("acpmf_static")), (Some(800), Some(452)));
        mirror.finish(&fake, &mut dir);
        assert_eq!((len("acevo_pmf_physics"), len("acpmf_static")), (Some(0), Some(452)));
        fs::remove_dir_all(root).unwrap();
    }

    /// A refresh copies one target at once (after a hold or a data-ready signal).
    #[test]
    fn refresh_copies_one_target() {
        let root = temp_dir("refresh");
        let mut dir = MirrorDir::new(&root);
        let mut fake = FakeSections::default();
        fake.live.insert("a".into(), vec![1]);
        fake.live.insert("b".into(), vec![2]);
        let mut mirror = Mirror::new(vec![MirrorTarget::sized("a", 1), MirrorTarget::sized("b", 1)]);
        mirror.refresh("b", &mut fake, &mut dir);
        assert!(mirror.is_present("b") && !mirror.is_present("a"));
        assert_eq!(fake.reads.get("a"), None);
        fs::remove_dir_all(root).unwrap();
    }
}
