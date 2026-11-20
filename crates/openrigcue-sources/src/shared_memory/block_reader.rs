//! Snapshot access to named shared-memory blocks for the SimBin, SCS, Falcon BMS, RaceRoom and rFactor
//! listeners: opening the bridge mirror, copying it, and decoding little-endian fields from the copy.
//!
//! The listeners take a full copy of the block on every liveness check,
//! so every channel of one tick comes from one consistent snapshot.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use super::held::{HeldSection, Opening, Registry};
use super::mapping::MappedView;

/// Windows name and byte size of one named mapping, as the bridge must mirror it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappingSpec {
    /// Windows mapping name.
    pub name: &'static str,
    /// Bytes the listener copies (the size of the listener's structure).
    pub size: usize,
}

/// An open mapping whose current bytes can be read.
pub trait BlockReader: Send {
    /// Current contents, exactly the size the block was opened with.
    fn read(&mut self) -> io::Result<&[u8]>;

    /// Read one current field or region without copying an entire large mapping.
    fn read_range(&mut self, offset: usize, len: usize) -> io::Result<Vec<u8>> {
        let end = offset.checked_add(len).ok_or(io::ErrorKind::InvalidInput)?;
        self.read()?.get(offset..end).map(<[u8]>::to_vec).ok_or_else(|| io::ErrorKind::UnexpectedEof.into())
    }
}

impl BlockReader for MappedView {
    fn read(&mut self) -> io::Result<&[u8]> {
        MappedView::read(self)
    }

    fn read_range(&mut self, offset: usize, len: usize) -> io::Result<Vec<u8>> {
        MappedView::read_range(self, offset, len)
    }
}

/// Opens named mappings. The default, [`BridgeMirrors`], reads the `openrigcue-bridge` mirrors; tests use fakes.
///
/// The listeners reach a section in three ways, and each has its own call here because each has its
/// own effect in the game's prefix:
/// - [`BlockOpener::open`]: open only, failing while the section does not exist;
/// - [`BlockOpener::create_held`]: create-or-open, which creates a zero-filled
///   section when none exists and keeps the section alive while the returned reader lives;
/// - [`BlockOpener::create_once`]: the same create-or-open, dropped right after one copy.
pub trait BlockOpener: Send {
    /// Open `spec`; fails while the mapping does not exist (game or bridge not running).
    fn open(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>>;

    /// A create-or-open mapping kept by the listener: the section, created zero-filled when missing, held
    /// until the reader is dropped. The default opens the mapping, or reads zeros when there is none.
    fn create_held(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
        match self.open(spec) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Box::new(Zeros(vec![0; spec.size]))),
            opened => opened,
        }
    }

    /// An explicit open of an existing section whose mapping object the caller keeps: fails like
    /// [`BlockOpener::open`]; otherwise the section is held until the reader is dropped.
    fn open_held(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
        self.open(spec)
    }

    /// A read-only create-or-open still creates a missing named section, with
    /// PAGE_READONLY protection and a FILE_MAP_READ view of the requested size.
    fn create_read_only_held(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
        match self.open(spec) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Box::new(Zeros(vec![0; spec.size]))),
            opened => opened,
        }
    }

    fn copy_read_only_once(&mut self, spec: MappingSpec) -> io::Result<Vec<u8>> {
        let mut reader = self.create_read_only_held(spec)?;
        reader.read().map(<[u8]>::to_vec)
    }

    /// One copy through a create-or-open mapping dropped right after it: the
    /// section's bytes, or the zeros of the section the call creates.
    fn create_once(&mut self, spec: MappingSpec) -> io::Result<Vec<u8>> {
        match copy_block(self, spec) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(vec![0; spec.size]),
            copied => copied,
        }
    }
}

/// The zero-filled section a create-or-open makes where nothing holds one.
struct Zeros(Vec<u8>);

impl BlockReader for Zeros {
    fn read(&mut self) -> io::Result<&[u8]> {
        Ok(&self.0)
    }
}

/// Opens the bridge mirrors under [`super::mapping::MIRROR_DIR`], with the application's own mapping objects in
/// [`Registry::global`]. A missing or emptied (length 0) mirror fails to open and to read with `NotFound`
/// ([`MappedView`]) for every listener; so does every mirror while no bridge is
/// connected (no prefix, so no game section).
#[derive(Debug, Default, Clone, Copy)]
pub struct BridgeMirrors;

impl BlockOpener for BridgeMirrors {
    fn create_read_only_held(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
        RegistryMirrors(Registry::global()).create_read_only_held(spec)
    }

    fn copy_read_only_once(&mut self, spec: MappingSpec) -> io::Result<Vec<u8>> {
        RegistryMirrors(Registry::global()).copy_read_only_once(spec)
    }
    fn open(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
        RegistryMirrors(Registry::global()).open(spec)
    }

    fn create_held(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
        RegistryMirrors(Registry::global()).create_held(spec)
    }

    fn open_held(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
        RegistryMirrors(Registry::global()).open_held(spec)
    }

    fn create_once(&mut self, spec: MappingSpec) -> io::Result<Vec<u8>> {
        RegistryMirrors(Registry::global()).create_once(spec)
    }
}

/// [`BridgeMirrors`] on a given registry (its mirror directory and bridges).
#[derive(Clone)]
pub struct RegistryMirrors(pub Arc<Registry>);

impl BlockOpener for RegistryMirrors {
    fn create_read_only_held(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
        let held = self.0.acquire_read_only(spec)?;
        Ok(Box::new(HeldReader { held, buf: Vec::new() }))
    }

    fn copy_read_only_once(&mut self, spec: MappingSpec) -> io::Result<Vec<u8>> {
        self.0.copy_read_only(spec)
    }
    fn open(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
        if !self.0.bridge_connected() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no bridge connected: no game section to open"));
        }
        Ok(Box::new(MappedView::open_in(self.0.mirror_dir(), spec.name, spec.size)?))
    }

    fn create_held(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
        let held = self.0.acquire(spec, Opening::CreateOrOpen, None);
        Ok(Box::new(HeldReader { held, buf: Vec::new() }))
    }

    fn open_held(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
        let current = copy_block(self, spec)?;
        let held = self.0.acquire(spec, Opening::Existing, Some(&current));
        Ok(Box::new(HeldReader { held, buf: Vec::new() }))
    }

    fn create_once(&mut self, spec: MappingSpec) -> io::Result<Vec<u8>> {
        Ok(self.0.create_once(spec))
    }
}

/// Reads one of the application's mapping objects.
struct HeldReader {
    held: HeldSection,
    buf: Vec<u8>,
}

impl BlockReader for HeldReader {
    fn read(&mut self) -> io::Result<&[u8]> {
        self.buf = self.held.read();
        Ok(&self.buf)
    }

    fn read_range(&mut self, offset: usize, len: usize) -> io::Result<Vec<u8>> {
        self.held.read_range(offset, len)
    }
}

/// Opens the bridge mirrors under the mirror directory it holds instead of [`super::mapping::MIRROR_DIR`]
/// (a test's temporary directory), without the bridge-connection rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorsIn(pub PathBuf);

impl BlockOpener for MirrorsIn {
    fn open(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
        Ok(Box::new(MappedView::open_in(&self.0, spec.name, spec.size)?))
    }
}

/// A create-or-open mapping object a listener keeps between polls.
pub(crate) struct KeptMapping {
    spec: MappingSpec,
    handle: Option<Box<dyn BlockReader>>,
}

impl KeptMapping {
    pub(crate) const fn new(spec: MappingSpec) -> Self {
        KeptMapping { spec, handle: None }
    }

    /// The same retained object with PAGE_READONLY/FILE_MAP_READ access.
    pub(crate) fn snapshot_read_only<O: BlockOpener + ?Sized>(&mut self, opener: &mut O) -> io::Result<Vec<u8>> {
        if self.handle.is_none() {
            self.handle = Some(opener.create_read_only_held(self.spec)?);
        }
        self.read()
    }

    /// Replaces the mapping object on every call: the new object is made before the previous one is
    /// released, so the section stays alive in between; then the block is copied. A failed mapping leaves
    /// the previous object in place.
    pub(crate) fn remake<O: BlockOpener + ?Sized>(&mut self, opener: &mut O) -> io::Result<Vec<u8>> {
        let handle = opener.create_held(self.spec)?;
        self.handle = Some(handle);
        self.read()
    }

    fn read(&mut self) -> io::Result<Vec<u8>> {
        match self.handle.as_mut() {
            Some(handle) => handle.read().map(<[u8]>::to_vec),
            None => Err(io::ErrorKind::NotFound.into()),
        }
    }

    /// Drops the mapping object.
    pub(crate) fn release(&mut self) {
        self.handle = None;
    }
}

/// Open `spec` and copy its current contents.
pub(crate) fn copy_block<O: BlockOpener + ?Sized>(opener: &mut O, spec: MappingSpec) -> io::Result<Vec<u8>> {
    let mut reader = opener.open(spec)?;
    Ok(reader.read()?.to_vec())
}

/// Little-endian field decoding over a block copy. Offsets come from the listener's structure layout; an offset
/// past the end of the copy reads as zero bytes (the copy is always the full structure size in practice).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fields<'a>(pub(crate) &'a [u8]);

impl<'a> Fields<'a> {
    fn array<const N: usize>(&self, offset: usize) -> [u8; N] {
        let mut out = [0u8; N];
        if let Some(src) = self.0.get(offset..offset + N) {
            out.copy_from_slice(src);
        }
        out
    }

    /// A 32-bit float, widened to a double.
    pub(crate) fn f32(&self, offset: usize) -> f64 {
        f64::from(f32::from_le_bytes(self.array(offset)))
    }

    /// A 64-bit float.
    pub(crate) fn f64(&self, offset: usize) -> f64 {
        f64::from_le_bytes(self.array(offset))
    }

    /// A signed 32-bit integer.
    pub(crate) fn i32(&self, offset: usize) -> i32 {
        i32::from_le_bytes(self.array(offset))
    }

    /// An unsigned 32-bit integer.
    pub(crate) fn u32(&self, offset: usize) -> u32 {
        u32::from_le_bytes(self.array(offset))
    }

    /// A signed byte.
    pub(crate) fn i8(&self, offset: usize) -> i8 {
        i8::from_le_bytes(self.array(offset))
    }

    /// An unsigned byte.
    pub(crate) fn u8(&self, offset: usize) -> u8 {
        self.array::<1>(offset)[0]
    }

    /// A boolean byte: any non-zero byte is true.
    pub(crate) fn bool(&self, offset: usize) -> bool {
        self.u8(offset) != 0
    }

    /// A character array of `len` bytes: the bytes before the first NUL.
    pub(crate) fn c_chars(&self, offset: usize, len: usize) -> &'a [u8] {
        let field = self.0.get(offset..offset + len).unwrap_or(&[]);
        let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
        &field[..end]
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! In-memory mappings for tests: blocks are set by name, missing names fail like an absent mapping.

    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    pub(crate) enum MapAttempt {
        Bytes(Vec<u8>),
        CreateError,
        ReadError,
    }

    #[derive(Clone, Default)]
    pub(crate) struct ScriptedMaps {
        attempts: Arc<Mutex<std::collections::VecDeque<MapAttempt>>>,
        trace: Arc<Mutex<Vec<String>>>,
        next: Arc<Mutex<usize>>,
    }

    impl ScriptedMaps {
        pub(crate) fn push(&self, attempt: MapAttempt) {
            self.attempts.lock().unwrap().push_back(attempt);
        }

        pub(crate) fn trace(&self) -> Vec<String> {
            std::mem::take(&mut *self.trace.lock().unwrap())
        }
    }

    struct ScriptedMap {
        id: usize,
        trace: Arc<Mutex<Vec<String>>>,
        bytes: Option<Vec<u8>>,
    }

    impl BlockReader for ScriptedMap {
        fn read(&mut self) -> io::Result<&[u8]> {
            self.trace.lock().unwrap().push(format!("read:{}", self.id));
            self.bytes.as_deref().ok_or_else(|| io::Error::other("scripted copy failure"))
        }
    }

    impl Drop for ScriptedMap {
        fn drop(&mut self) {
            self.trace.lock().unwrap().push(format!("close:{}", self.id));
        }
    }

    impl BlockOpener for ScriptedMaps {
        fn open(&mut self, _: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
            panic!("this source uses native mmap construction, not open-only")
        }

        fn create_held(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
            let mut next = self.next.lock().unwrap();
            *next += 1;
            let id = *next;
            self.trace.lock().unwrap().push(format!("create:{id}"));
            let bytes = match self.attempts.lock().unwrap().pop_front().expect("unplanned mmap creation") {
                MapAttempt::CreateError => return Err(io::Error::other("scripted mmap failure")),
                MapAttempt::ReadError => None,
                MapAttempt::Bytes(mut bytes) => {
                    bytes.resize(spec.size, 0);
                    Some(bytes)
                }
            };
            Ok(Box::new(ScriptedMap { id, trace: self.trace.clone(), bytes }))
        }
    }

    /// Shared store of fake mappings; clones see the same blocks.
    #[derive(Clone, Default)]
    pub(crate) struct FakeBlocks {
        blocks: Arc<Mutex<HashMap<&'static str, Vec<u8>>>>,
        opens: Arc<Mutex<usize>>,
        held: Arc<Mutex<HashMap<&'static str, usize>>>,
        created_once: Arc<Mutex<usize>>,
        read_only_holds: Arc<Mutex<usize>>,
        read_only_copies: Arc<Mutex<usize>>,
        failures: Arc<Mutex<HashMap<&'static str, io::ErrorKind>>>,
        read_failures: Arc<Mutex<std::collections::HashSet<&'static str>>>,
    }

    impl FakeBlocks {
        /// Publish `bytes` (resized to the spec size, like a zero-filled mapping) under `spec.name`.
        pub(crate) fn set(&self, spec: MappingSpec, mut bytes: Vec<u8>) {
            bytes.resize(spec.size, 0);
            self.blocks.lock().unwrap().insert(spec.name, bytes);
        }

        /// Remove the mapping, so opening and reading it fail.
        pub(crate) fn remove(&self, name: &str) {
            self.blocks.lock().unwrap().remove(name);
        }

        /// How many times a mapping was opened.
        pub(crate) fn open_count(&self) -> usize {
            *self.opens.lock().unwrap()
        }

        /// How many mapping objects of `name` the listener keeps.
        pub(crate) fn held_count(&self, name: &str) -> usize {
            self.held.lock().unwrap().get(name).copied().unwrap_or(0)
        }

        /// How many per-poll create-or-open copies were made.
        pub(crate) fn created_once_count(&self) -> usize {
            *self.created_once.lock().unwrap()
        }

        pub(crate) fn read_only_holds(&self) -> usize {
            *self.read_only_holds.lock().unwrap()
        }
        pub(crate) fn read_only_copies(&self) -> usize {
            *self.read_only_copies.lock().unwrap()
        }
        pub(crate) fn fail_open(&self, name: &'static str, error: Option<io::ErrorKind>) {
            let mut failures = self.failures.lock().unwrap();
            match error {
                Some(error) => {
                    failures.insert(name, error);
                }
                None => {
                    failures.remove(name);
                }
            }
        }

        pub(crate) fn fail_read(&self, name: &'static str, fail: bool) {
            let mut failures = self.read_failures.lock().unwrap();
            if fail {
                failures.insert(name);
            } else {
                failures.remove(name);
            }
        }
    }

    struct FakeReader {
        store: FakeBlocks,
        name: &'static str,
        size: usize,
        buf: Vec<u8>,
    }

    impl BlockReader for FakeReader {
        fn read_range(&mut self, offset: usize, len: usize) -> io::Result<Vec<u8>> {
            if self.store.read_failures.lock().unwrap().contains(self.name) {
                return Err(io::Error::other("scripted copy failure"));
            }
            let blocks = self.store.blocks.lock().unwrap();
            let bytes = blocks.get(self.name).ok_or(io::ErrorKind::NotFound)?;
            let end = offset.checked_add(len).ok_or(io::ErrorKind::InvalidInput)?;
            if end > self.size {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let mut result = vec![0; len];
            if let Some(available) = bytes.get(offset..end.min(bytes.len())) {
                result[..available.len()].copy_from_slice(available);
            }
            Ok(result)
        }

        fn read(&mut self) -> io::Result<&[u8]> {
            if self.store.read_failures.lock().unwrap().contains(self.name) {
                return Err(io::Error::other("scripted copy failure"));
            }
            let blocks = self.store.blocks.lock().unwrap();
            let bytes = blocks.get(self.name).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            self.buf = bytes.clone();
            self.buf.resize(self.size, 0);
            Ok(&self.buf)
        }
    }

    impl BlockOpener for FakeBlocks {
        fn create_read_only_held(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
            *self.read_only_holds.lock().unwrap() += 1;
            if let Some(error) = self.failures.lock().unwrap().get(spec.name).copied() {
                return Err(error.into());
            }
            self.create_held(spec)
        }
        fn copy_read_only_once(&mut self, spec: MappingSpec) -> io::Result<Vec<u8>> {
            *self.read_only_copies.lock().unwrap() += 1;
            self.create_once(spec)
        }
        fn open(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
            *self.opens.lock().unwrap() += 1;
            if let Some(error) = self.failures.lock().unwrap().get(spec.name).copied() {
                return Err(error.into());
            }
            if !self.blocks.lock().unwrap().contains_key(spec.name) {
                return Err(io::ErrorKind::NotFound.into());
            }
            Ok(Box::new(FakeReader { store: self.clone(), name: spec.name, size: spec.size, buf: Vec::new() }))
        }

        /// Creates the block zero-filled when missing (as the section is created in the prefix) and counts the
        /// object as held until the reader is dropped.
        fn create_held(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
            *self.opens.lock().unwrap() += 1;
            self.blocks.lock().unwrap().entry(spec.name).or_insert_with(|| vec![0; spec.size]);
            *self.held.lock().unwrap().entry(spec.name).or_default() += 1;
            let reader = FakeReader { store: self.clone(), name: spec.name, size: spec.size, buf: Vec::new() };
            Ok(Box::new(Held { reader, store: self.clone(), name: spec.name }))
        }

        fn open_held(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
            let reader = self.open(spec)?;
            *self.held.lock().unwrap().entry(spec.name).or_default() += 1;
            Ok(Box::new(HeldOpened { reader, store: self.clone(), name: spec.name }))
        }

        fn create_once(&mut self, spec: MappingSpec) -> io::Result<Vec<u8>> {
            *self.created_once.lock().unwrap() += 1;
            match copy_block(self, spec) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(vec![0; spec.size]),
                copied => copied,
            }
        }
    }

    /// A fake mapping object kept by a listener.
    struct Held {
        reader: FakeReader,
        store: FakeBlocks,
        name: &'static str,
    }

    impl BlockReader for Held {
        fn read_range(&mut self, offset: usize, len: usize) -> io::Result<Vec<u8>> {
            self.reader.read_range(offset, len)
        }

        fn read(&mut self) -> io::Result<&[u8]> {
            self.reader.read()
        }
    }

    impl Drop for Held {
        fn drop(&mut self) {
            *self.store.held.lock().unwrap().entry(self.name).or_default() -= 1;
        }
    }

    struct HeldOpened {
        reader: Box<dyn BlockReader>,
        store: FakeBlocks,
        name: &'static str,
    }

    impl BlockReader for HeldOpened {
        fn read_range(&mut self, offset: usize, len: usize) -> io::Result<Vec<u8>> {
            self.reader.read_range(offset, len)
        }

        fn read(&mut self) -> io::Result<&[u8]> {
            self.reader.read()
        }
    }

    impl Drop for HeldOpened {
        fn drop(&mut self) {
            *self.store.held.lock().unwrap().entry(self.name).or_default() -= 1;
        }
    }

    /// Builds a block byte by byte at structure offsets.
    pub(crate) struct BlockBuilder(pub(crate) Vec<u8>);

    impl BlockBuilder {
        pub(crate) fn new(size: usize) -> Self {
            BlockBuilder(vec![0; size])
        }
        fn put(&mut self, offset: usize, bytes: &[u8]) -> &mut Self {
            self.0[offset..offset + bytes.len()].copy_from_slice(bytes);
            self
        }
        pub(crate) fn f32(&mut self, offset: usize, v: f32) -> &mut Self {
            self.put(offset, &v.to_le_bytes())
        }
        pub(crate) fn f64(&mut self, offset: usize, v: f64) -> &mut Self {
            self.put(offset, &v.to_le_bytes())
        }
        pub(crate) fn i32(&mut self, offset: usize, v: i32) -> &mut Self {
            self.put(offset, &v.to_le_bytes())
        }
        pub(crate) fn u32(&mut self, offset: usize, v: u32) -> &mut Self {
            self.put(offset, &v.to_le_bytes())
        }
        pub(crate) fn i8(&mut self, offset: usize, v: i8) -> &mut Self {
            self.put(offset, &v.to_le_bytes())
        }
        pub(crate) fn u8(&mut self, offset: usize, v: u8) -> &mut Self {
            self.put(offset, &[v])
        }
        pub(crate) fn bytes(&mut self, offset: usize, v: &[u8]) -> &mut Self {
            self.put(offset, v)
        }
        pub(crate) fn build(&self) -> Vec<u8> {
            self.0.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_decode_little_endian_and_c_strings() {
        let mut b = fake::BlockBuilder::new(32);
        b.f32(0, 1.5).i32(4, -7).u8(8, 0xff).bytes(9, b"abc\0zz").f64(16, -2.25);
        let bytes = b.build();
        let f = Fields(&bytes);
        assert_eq!(f.f32(0), 1.5);
        assert_eq!(f.i32(4), -7);
        assert_eq!(f.i8(8), -1);
        assert_eq!(f.u8(8), 255);
        assert!(f.bool(8));
        assert_eq!(f.c_chars(9, 6), b"abc");
        assert_eq!(f.f64(16), -2.25);
        // Reads past the end decode as zero.
        assert_eq!(f.i32(30), 0);
    }

    #[test]
    fn c_chars_without_nul_use_the_whole_field() {
        let bytes = *b"abcd";
        assert_eq!(Fields(&bytes).c_chars(0, 4), b"abcd");
    }
}
