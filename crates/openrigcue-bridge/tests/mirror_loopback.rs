//! Loopback of the mirror format: what the bridge writes into /dev/shm/openrigcue is what
//! `openrigcue_sources::shared_memory::mapping::MappedView` reads. Each test mirrors into a directory of its own
//! under the system temporary directory, never into the real /dev/shm/openrigcue.
#![cfg(unix)]

use std::path::{Path, PathBuf};

use openrigcue_bridge::event::{EventRelay, EventState, EventWait, EVENT_FILE_LEN};
use openrigcue_bridge::mirror::{Mirror, MirrorDir, SectionReader};
use openrigcue_bridge::paths::{mirror_file_name, MIRROR_DIR};
use openrigcue_bridge::plan::MirrorTarget;
use openrigcue_sources::shared_memory::mapping::{self, MappedView};

/// One fake section whose content the test changes between ticks, possibly held for OpenRigCue.
struct OneSection {
    bytes: Option<Vec<u8>>,
    held: bool,
}

impl SectionReader for OneSection {
    fn read(&mut self, _target: &MirrorTarget, buf: &mut Vec<u8>) -> bool {
        match &self.bytes {
            Some(bytes) => {
                buf.clone_from(bytes);
                true
            }
            None => false,
        }
    }

    fn is_held(&self, _name: &str) -> bool {
        self.held
    }
}

fn unique(tag: &str) -> &'static str {
    Box::leak(format!("Local\\openrigcue_bridge_{tag}_{}", std::process::id()).into_boxed_str())
}

/// The mirror directory of one test (standing in for /dev/shm/openrigcue), removed when dropped.
struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> TestDir {
        let dir = std::env::temp_dir().join(format!("openrigcue-bridge-loopback-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TestDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// Whether the mirror of `name` says the section is absent (missing or length 0: a listener's open fails).
    fn absent(&self, name: &str) -> bool {
        std::fs::metadata(mapping::mirror_path_in(self.path(), name)).map_or(true, |m| m.len() == 0)
    }

    fn open(&self, name: &str, size: usize) -> std::io::Result<MappedView> {
        MappedView::open_in(self.path(), name, size)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The Warplanes structure: six little-endian floats, 24 bytes.
fn ww1_frame(values: [f32; 6]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

#[test]
fn bridge_and_sources_agree_on_directory_and_names() {
    assert_eq!(MIRROR_DIR, mapping::MIRROR_DIR);
    for name in ["Local\\acpmf_physics", "Local\\IRSDKMemMapFileName", "$rFactor2SMMP_Scoring$", "a/b"] {
        assert_eq!(Path::new(MIRROR_DIR).join(mirror_file_name(name)), mapping::mirror_path(name));
        let elsewhere = Path::new("/tmp/mirrors");
        assert_eq!(MirrorDir::new(elsewhere).path_of(name), mapping::mirror_path_in(elsewhere, name));
    }
}

/// A section nothing holds (opened afresh on every
/// poll) mirrored by the bridge reads back byte for byte. A reader that keeps its view sees each new copy; once the game
/// has gone the mirror is absent (length 0: the view's read fails with `NotFound`), and
/// when the game is back the same view sees the new frames.
#[test]
fn per_poll_section_reads_back_disappears_and_returns() {
    let name = unique("per_poll");
    let tmp = TestDir::new("per_poll");
    let target = MirrorTarget::sized(name, 24);
    let mut dir = MirrorDir::new(tmp.path());
    let mut mirror = Mirror::new(vec![target]);
    assert!(tmp.absent(name));

    let first = ww1_frame([1.5, -2.0, 3.25, 0.0, 9.0, -0.5]);
    let mut section = OneSection { bytes: Some(first.clone()), held: false };
    mirror.tick(&mut section, &mut dir);
    assert!(!tmp.absent(name));

    let mut view = tmp.open(name, 24).expect("mirror exists");
    assert_eq!(view.read().unwrap(), &first[..]);

    let second: Vec<u8> = (0..24).collect();
    section.bytes = Some(second.clone());
    mirror.tick(&mut section, &mut dir);
    assert_eq!(view.read().unwrap(), &second[..]);

    // A reader asking for more than the section holds gets the rest as zeros.
    let mut wide = tmp.open(name, 32).unwrap();
    assert_eq!(&wide.read().unwrap()[24..], &[0; 8]);

    // The game exits.
    section.bytes = None;
    mirror.tick(&mut section, &mut dir);
    assert!(tmp.absent(name));
    assert_eq!(view.read().unwrap_err().kind(), std::io::ErrorKind::NotFound);

    // The game restarts within the same bridge run.
    let third = ww1_frame([7.0; 6]);
    section.bytes = Some(third.clone());
    mirror.tick(&mut section, &mut dir);
    assert_eq!(view.read().unwrap(), &third[..]);
    mirror.finish(&section, &mut dir);
    assert!(tmp.absent(name));
    assert_eq!(view.read().unwrap_err().kind(), std::io::ErrorKind::NotFound);
}

/// A bridge run that was killed leaves a mirror with data: the next run (serving another game) marks it
/// absent at start (the held view's read fails with `NotFound`), and that view sees the frames once a run mirrors the
/// section again.
#[test]
fn stale_per_poll_mirror_is_absent_in_the_next_run() {
    let name = unique("stale");
    let tmp = TestDir::new("stale");
    let target = MirrorTarget::sized(name, 24);
    let mut killed_run = Mirror::new(vec![target.clone()]);
    let frame = ww1_frame([2.0; 6]);
    let mut section = OneSection { bytes: Some(frame.clone()), held: false };
    killed_run.tick(&mut section, &mut MirrorDir::new(tmp.path()));
    let mut view = tmp.open(name, 24).unwrap();
    assert_eq!(view.read().unwrap(), &frame[..]);

    Mirror::new(Vec::new()).prepare(&mut MirrorDir::new(tmp.path()), &[name]);
    assert!(tmp.absent(name));
    assert_eq!(view.read().unwrap_err().kind(), std::io::ErrorKind::NotFound);

    let mut later_run = Mirror::new(vec![target]);
    let mut dir = MirrorDir::new(tmp.path());
    later_run.prepare(&mut dir, &[name]);
    let fresh = ww1_frame([3.0; 6]);
    section.bytes = Some(fresh.clone());
    later_run.tick(&mut section, &mut dir);
    assert_eq!(view.read().unwrap(), &fresh[..]);
}

/// A section the bridge holds for OpenRigCue (the Warplanes listener's mapping) keeps the game's last
/// frame for readers after the game has exited and after the run.
#[test]
fn held_section_stays_frozen_after_the_run() {
    let name = unique("held");
    let tmp = TestDir::new("held");
    let mut dir = MirrorDir::new(tmp.path());
    let mut mirror = Mirror::new(Vec::new());
    mirror.ensure(name, 24);
    let last = ww1_frame([4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
    let mut section = OneSection { bytes: Some(last.clone()), held: true };
    mirror.tick(&mut section, &mut dir);
    section.bytes = None;
    mirror.tick(&mut section, &mut dir);
    mirror.finish(&section, &mut dir);

    let mut view = tmp.open(name, 24).expect("held mirror kept");
    assert_eq!(view.read().unwrap(), &last[..]);
}

/// The iRacing event file reads back through MappedView and decodes to the bridge's state.
#[test]
fn event_file_reads_back_through_mapped_view() {
    let tmp = TestDir::new("irsdk_event");
    let event = unique("irsdk_event");
    let mut dir = MirrorDir::new(tmp.path());
    let mut relay = EventRelay::start(event, &mut dir);
    relay.observe(EventWait::Signalled, &mut dir);
    relay.observe(EventWait::Signalled, &mut dir);

    let mut view = tmp.open(event, EVENT_FILE_LEN).expect("event file exists");
    assert_eq!(EventState::decode(view.read().unwrap()), Some(EventState { open: true, signals: 2 }));
}
