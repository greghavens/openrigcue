//! The control connection end to end: OpenRigCue's registry of its mapping objects
//! (`openrigcue_sources::shared_memory::held::Registry`) talking to the bridge's request handling
//! (`openrigcue_bridge::control::Holds`) over a real localhost TCP connection, with a fake prefix whose sections
//! the fake holder mirrors into a temporary directory.
#![cfg(unix)]

use std::collections::HashMap;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use openrigcue_bridge::control::{read_message, write_message, Holds, Request, SectionHolder};
use openrigcue_bridge::mirror::MirrorDir;
use openrigcue_sources::shared_memory::block_reader::{BlockOpener, MappingSpec, RegistryMirrors};
use openrigcue_sources::shared_memory::held::Registry;

/// The sections of a fake prefix, mirrored into `dir` the way the Windows side mirrors held sections.
struct FakePrefix {
    sections: HashMap<String, Vec<u8>>,
    held: Vec<String>,
    dir: MirrorDir,
}

impl SectionHolder for FakePrefix {
    fn hold(&mut self, name: &str, size: usize, create: bool, seed: Option<&[u8]>) -> bool {
        if !self.sections.contains_key(name) {
            if !create {
                return false;
            }
            let mut bytes = seed.map(<[u8]>::to_vec).unwrap_or_default();
            bytes.resize(size, 0);
            self.sections.insert(name.to_owned(), bytes);
        }
        self.held.push(name.to_owned());
        self.dir.publish(name, &self.sections[name]).unwrap();
        true
    }

    fn release(&mut self, name: &str) {
        self.held.retain(|held| held != name);
    }

    fn create_once(&mut self, _name: &str, _size: usize) {}
}

fn temp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("openrigcue-bridge-control-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const SPEC: MappingSpec = MappingSpec { name: "Local\\GPBProxyObject", size: 8 };

/// A listener's held mapping while the game is running: the bridge opens the game's section,
/// holds it and mirrors it before the listener's first read; when the bridge (the prefix) goes away the object
/// keeps the last frame; a later bridge creates the section again with that frame.
#[test]
fn registry_and_bridge_hold_sections_together() {
    let root = temp("hold");
    let registry = Registry::new(&root);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let bridge_end = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let prefix_ends = bridge_end.try_clone().unwrap();
    registry.attach(listener.accept().unwrap().0);

    let prefix = Arc::new(Mutex::new(FakePrefix {
        sections: HashMap::from([(SPEC.name.to_owned(), vec![1, 2, 3, 4, 5, 6, 7, 8])]),
        held: Vec::new(),
        dir: MirrorDir::new(&root),
    }));
    let bridge = {
        let prefix = Arc::clone(&prefix);
        thread::spawn(move || {
            let mut reader = bridge_end.try_clone().unwrap();
            let mut writer = bridge_end;
            let mut holds = Holds::default();
            while let Ok(Some(message)) = read_message(&mut reader) {
                let request = Request::decode(&message).unwrap();
                let mut prefix = prefix.lock().unwrap();
                if let Some(answer) = holds.apply(request, &mut *prefix) {
                    if write_message(&mut writer, &answer).is_err() {
                        return;
                    }
                }
            }
        })
    };

    let mut opener = RegistryMirrors(Arc::clone(&registry));
    let mut held = opener.create_held(SPEC).unwrap();
    assert_eq!(held.read().unwrap(), &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(prefix.lock().unwrap().held, [SPEC.name]);

    // A plain open reads the mirror while the bridge is connected.
    assert_eq!(opener.open(SPEC).unwrap().read().unwrap(), &[1, 2, 3, 4, 5, 6, 7, 8]);

    // The game writes once more; then the prefix (and with it the bridge) goes away.
    prefix.lock().unwrap().dir.publish(SPEC.name, &[9; 8]).unwrap();
    assert_eq!(held.read().unwrap(), &[9; 8]);
    prefix_ends.shutdown(std::net::Shutdown::Both).unwrap();
    bridge.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while registry.bridge_connected() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!registry.bridge_connected());
    MirrorDir::new(&root).publish(SPEC.name, &[0; 8]).unwrap();
    assert_eq!(held.read().unwrap(), &[9; 8], "the original's object keeps the last frame");
    assert!(opener.open(SPEC).is_err(), "no prefix, no section to open");

    // A later bridge gets the object with that frame, to create the section with it.
    let mut later = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    registry.attach(listener.accept().unwrap().0);
    let message = read_message(&mut later).unwrap().unwrap();
    match Request::decode(&message).unwrap() {
        Request::Hold { create, size, name, seed, .. } => {
            assert_eq!((create, size, name.as_str(), seed), (true, 8, SPEC.name, Some(vec![9; 8])));
        }
        other => panic!("unexpected {other:?}"),
    }
    std::fs::remove_dir_all(root).unwrap();
}
