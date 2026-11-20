//! Cached file comparison: stat signatures, shared outcomes and 8 KiB comparisons.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::UNIX_EPOCH;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Signature {
    regular: bool,
    size: u64,
    modified: u64,
}

pub(crate) fn signature(path: &Path) -> io::Result<Signature> {
    let meta = fs::metadata(path)?;
    let modified = match meta.modified()?.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as f64 + f64::from(duration.subsec_nanos()) * 1e-9,
        Err(error) => -(error.duration().as_secs() as f64 + f64::from(error.duration().subsec_nanos()) * 1e-9),
    };
    Ok(Signature { regular: meta.is_file(), size: meta.len(), modified: modified.to_bits() })
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Reference {
    File(PathBuf),
    Embedded(String),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    first: PathBuf,
    second: Reference,
    first_stat: Signature,
    second_stat: Signature,
}

#[derive(Default)]
struct Comparer(Mutex<BTreeMap<Key, bool>>);

impl Comparer {
    fn cache(&self) -> MutexGuard<'_, BTreeMap<Key, bool>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn compare<R: Read>(
        &self,
        first: &Path,
        second: Reference,
        first_stat: Signature,
        second_stat: Signature,
        shallow: bool,
        open_second: impl FnOnce() -> io::Result<R>,
    ) -> io::Result<bool> {
        if !first_stat.regular || !second_stat.regular {
            return Ok(false);
        }
        if shallow && first_stat == second_stat {
            return Ok(true);
        }
        if first_stat.size != second_stat.size {
            return Ok(false);
        }
        let key = Key { first: first.to_owned(), second, first_stat, second_stat };
        if let Some(outcome) = self.cache().get(&key).copied() {
            return Ok(outcome);
        }
        let mut left = File::open(first)?;
        let mut right = open_second()?;
        let outcome = equal_chunks(&mut left, &mut right)?;
        let count = self.cache().len();
        if count > 100 {
            self.cache().clear();
        }
        self.cache().insert(key, outcome);
        Ok(outcome)
    }
}

fn shared() -> &'static Comparer {
    static COMPARER: OnceLock<Comparer> = OnceLock::new();
    COMPARER.get_or_init(Comparer::default)
}

/// Forget every cached comparison outcome.
pub fn clear_cache() {
    shared().cache().clear();
}

/// Compare `first` with `second`; `shallow` accepts identical stat signatures as equal. Every call stats both paths before consulting the shared cache.
pub fn cmp(first: &Path, second: &Path, shallow: bool) -> io::Result<bool> {
    let first_stat = signature(first)?;
    let second_stat = signature(second)?;
    shared().compare(first, Reference::File(second.to_owned()), first_stat, second_stat, shallow, || File::open(second))
}

/// Compare an already-statted game file with its virtual library counterpart.
pub(crate) fn compare_embedded(
    first: &Path,
    first_stat: Signature,
    name: &str,
    contents: &[u8],
    modified: u64,
) -> io::Result<bool> {
    let second_stat = Signature { regular: true, size: contents.len() as u64, modified: (modified as f64).to_bits() };
    shared().compare(first, Reference::Embedded(name.to_owned()), first_stat, second_stat, true, || {
        Ok(Cursor::new(contents))
    })
}

fn read_chunk(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut length = 0;
    while length < buffer.len() {
        match reader.read(&mut buffer[length..]) {
            Ok(0) => break,
            Ok(count) => length += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(length)
}

fn equal_chunks(left: &mut impl Read, right: &mut impl Read) -> io::Result<bool> {
    let mut left_buffer = [0; 8192];
    let mut right_buffer = [0; 8192];
    loop {
        let left_length = read_chunk(left, &mut left_buffer)?;
        let right_length = read_chunk(right, &mut right_buffer)?;
        if left_buffer[..left_length] != right_buffer[..right_length] {
            return Ok(false);
        }
        if left_length == 0 {
            return Ok(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn fixture(tag: &str) -> (PathBuf, Signature, Signature) {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let number = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("openrigcue-{tag}-{}-{number}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("first");
        fs::write(&path, b"abc").unwrap();
        let first = signature(&path).unwrap();
        let second = Signature { modified: (1.0f64).to_bits(), ..first };
        (path, first, second)
    }

    #[test]
    fn caches_true_and_false_until_signatures_change_or_cache_is_cleared() {
        let (path, first, second) = fixture("filecmp-outcomes");
        let comparer = Comparer::default();
        let reference = Reference::Embedded("reference".into());
        assert!(comparer.compare(&path, reference.clone(), first, second, true, || Ok(Cursor::new(b"abc"))).unwrap());
        fs::write(&path, b"xyz").unwrap();
        assert!(comparer
            .compare(&path, reference.clone(), first, second, false, || -> io::Result<Cursor<&[u8]>> {
                panic!("cached result must not reopen")
            })
            .unwrap());
        comparer.cache().clear();
        assert!(!comparer.compare(&path, reference.clone(), first, second, false, || Ok(Cursor::new(b"abc"))).unwrap());
        fs::write(&path, b"abc").unwrap();
        assert!(!comparer
            .compare(&path, reference.clone(), first, second, true, || -> io::Result<Cursor<&[u8]>> {
                panic!("cached false must also be returned")
            })
            .unwrap());
        let changed = Signature { modified: (2.0f64).to_bits(), ..first };
        assert!(comparer.compare(&path, reference, changed, second, true, || Ok(Cursor::new(b"abc"))).unwrap());
    }

    #[test]
    fn clears_only_after_a_successful_cache_miss_with_more_than_one_hundred_entries() {
        let (path, first, second) = fixture("filecmp-cache-limit");
        let comparer = Comparer::default();
        for i in 0..101 {
            assert!(comparer
                .compare(&path, Reference::Embedded(i.to_string()), first, second, true, || Ok(Cursor::new(b"abc")))
                .unwrap());
        }
        assert_eq!(comparer.cache().len(), 101);
        assert!(comparer
            .compare(&path, Reference::Embedded("0".into()), first, second, true, || -> io::Result<Cursor<&[u8]>> {
                panic!("cache hit")
            })
            .unwrap());
        assert_eq!(comparer.cache().len(), 101);
        let failing = Reference::Embedded("failed".into());
        assert!(comparer
            .compare(&path, failing.clone(), first, second, true, || -> io::Result<Cursor<&[u8]>> {
                Err(io::Error::other("fixture open failure"))
            })
            .is_err());
        assert_eq!(comparer.cache().len(), 101);
        assert!(comparer.compare(&path, failing, first, second, true, || Ok(Cursor::new(b"abc"))).unwrap());
        assert_eq!(comparer.cache().len(), 1);
    }

    #[test]
    fn reads_left_then_right_in_eight_kib_blocks_and_stops_at_the_first_mismatch() {
        struct Reader {
            name: &'static str,
            bytes: Cursor<Vec<u8>>,
            calls: Rc<RefCell<Vec<(&'static str, usize)>>>,
        }
        impl Read for Reader {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.calls.borrow_mut().push((self.name, buffer.len()));
                if self.bytes.position() >= 8192 {
                    return Err(io::Error::other("the unread tail must remain unread"));
                }
                self.bytes.read(buffer)
            }
        }
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut left = Reader { name: "left", bytes: Cursor::new(vec![1; 8193]), calls: calls.clone() };
        let mut data = vec![1; 8193];
        data[8191] = 2;
        let mut right = Reader { name: "right", bytes: Cursor::new(data), calls: calls.clone() };
        assert!(!equal_chunks(&mut left, &mut right).unwrap());
        assert_eq!(*calls.borrow(), [("left", 8192), ("right", 8192)]);
        assert!(equal_chunks(&mut Cursor::new(vec![7; 8193]), &mut Cursor::new(vec![7; 8193])).unwrap());
        assert!(!equal_chunks(&mut Cursor::new(vec![7; 8193]), &mut Cursor::new(vec![7; 8192])).unwrap());
    }
}
