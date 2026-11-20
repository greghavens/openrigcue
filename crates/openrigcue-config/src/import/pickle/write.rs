//! Pickle protocol-5 writer for the values stored in the database files.
//! Output uses framing, batched container items and a memo keyed by object identity;
//! no reducer or arbitrary user code is executed. Object handles retain live aliases.
#[cfg(test)]
use super::Key;
use super::{LegacyValue, NativeGlobal, PickleError};
use std::collections::{BTreeMap, BTreeSet};

pub fn encode(value: &LegacyValue) -> Vec<u8> {
    encode_checked(value).expect("value is supported by the native pickle writer")
}

/// Serialize before enqueuing a SQLite assignment, retaining the failure
/// and its ordering. Memoryview is not picklable, even inside a container.
pub fn encode_checked(value: &LegacyValue) -> Result<Vec<u8>, PickleError> {
    let _operation = openrigcue_model::native_object::native_operation();
    fn validate(value: &LegacyValue, seen: &mut BTreeSet<usize>) -> Result<(), PickleError> {
        match value {
            LegacyValue::MemoryView(_) => {
                Err(PickleError::Constructor { class: "TypeError", message: "cannot pickle memoryview objects".into() })
            }
            LegacyValue::Object(v) => {
                if seen.insert(v.id()) {
                    validate(&v.resolved(), seen)
                } else {
                    Ok(())
                }
            }
            LegacyValue::List(v) | LegacyValue::Tuple(v) | LegacyValue::Set(v) | LegacyValue::FrozenSet(v) => {
                for v in v {
                    validate(v, seen)?;
                }
                Ok(())
            }
            LegacyValue::Dict(v) => {
                for (k, v) in v.iter_native() {
                    validate(&k, seen)?;
                    validate(v, seen)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    validate(value, &mut BTreeSet::new())?;
    let mut writer = Writer { out: vec![0x80, 5], frame: Vec::new(), memo: BTreeMap::new(), next: 0 };
    writer.value(value, None);
    writer.write(b".");
    writer.commit();
    Ok(writer.out)
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Identity {
    Object(usize),
    Global(NativeGlobal),
    GlobalModule(NativeGlobal),
    GlobalName(NativeGlobal),
    BuiltinsModule,
    Latin1String(u16),
    ByteSingleton(u16),
}
struct Writer {
    out: Vec<u8>,
    frame: Vec<u8>,
    memo: BTreeMap<Identity, usize>,
    next: usize,
}
impl Writer {
    fn write(&mut self, data: &[u8]) {
        self.frame.extend_from_slice(data);
    }
    fn commit(&mut self) {
        if self.frame.len() >= 4 {
            self.out.push(0x95);
            self.out.extend_from_slice(&(self.frame.len() as u64).to_le_bytes());
        }
        self.out.append(&mut self.frame);
    }
    // Frame boundaries are considered once per saved object, rather than at
    // every byte write. An individual opcode and its payload remain together.
    fn boundary(&mut self) {
        if self.frame.len() >= 65536 {
            self.commit();
        }
    }
    fn memoize(&mut self, id: Option<Identity>) -> usize {
        let index = self.next;
        self.next += 1;
        if let Some(id) = id {
            self.memo.insert(id, index);
        }
        self.write(&[0x94]);
        index
    }
    fn get(&mut self, index: usize) {
        if let Ok(i) = u8::try_from(index) {
            self.write(&[b'h', i]);
        } else {
            self.write(b"j");
            self.write(&(index as u32).to_le_bytes());
        }
    }
    fn sized(&mut self, short: u8, long: u8, long8: u8, data: &[u8]) {
        let mut header = Vec::with_capacity(9);
        if let Ok(n) = u8::try_from(data.len()) {
            header.extend([short, n]);
        } else if let Ok(n) = u32::try_from(data.len()) {
            header.push(long);
            header.extend(n.to_le_bytes());
        } else {
            header.push(long8);
            header.extend((data.len() as u64).to_le_bytes());
        }
        if data.len() >= 65536 {
            self.commit();
            self.out.extend(header);
            self.out.extend_from_slice(data);
        } else {
            self.write(&header);
            self.write(data);
        }
    }
    fn string(&mut self, text: &str, id: Option<Identity>) {
        if let Some(index) = id.as_ref().and_then(|id| self.memo.get(id)).copied() {
            self.get(index);
            return;
        }
        self.sized(0x8c, b'X', 0x8d, text.as_bytes());
        self.memoize(id);
    }
    fn global(&mut self, global: &NativeGlobal) -> usize {
        let id = Identity::Global(global.clone());
        if let Some(&index) = self.memo.get(&id) {
            self.get(index);
            return index;
        }
        self.boundary();
        self.string(
            &global.module,
            Some(if global.module == "builtins" {
                Identity::BuiltinsModule
            } else {
                Identity::GlobalModule(global.clone())
            }),
        );
        self.boundary();
        self.string(&global.name, Some(Identity::GlobalName(global.clone())));
        self.write(&[0x93]);
        self.memoize(Some(id))
    }
    fn tuple(&mut self, values: &[LegacyValue], id: Option<Identity>) {
        if values.is_empty() {
            self.write(b")");
            return;
        }
        if values.len() > 3 {
            self.write(b"(");
        }
        for value in values {
            self.value(value, None);
        }
        if let Some(index) = id.as_ref().and_then(|id| self.memo.get(id)).copied() {
            if values.len() > 3 {
                self.write(b"1");
            } else {
                for _ in values {
                    self.write(b"0");
                }
            }
            self.get(index);
        } else {
            self.write(&[match values.len() {
                1 => 0x85,
                2 => 0x86,
                3 => 0x87,
                _ => b't',
            }]);
            self.memoize(id);
        }
    }
    fn value(&mut self, value: &LegacyValue, id: Option<Identity>) {
        self.boundary();
        if let LegacyValue::Object(object) = value {
            self.value(&object.resolved(), Some(Identity::Object(object.id())));
            return;
        }
        // Empty/single-Latin-1-character strings and empty/one-byte bytes are
        // shared singletons, so equal ones are one object even when no PUT was present.
        let id = match value {
            LegacyValue::Str(text) if text.is_empty() => Some(Identity::Latin1String(256)),
            LegacyValue::Str(text) => {
                let mut chars = text.chars();
                match (chars.next(), chars.next()) {
                    (Some(cp), None) if u32::from(cp) <= 255 => Some(Identity::Latin1String(cp as u16)),
                    _ => id,
                }
            }
            LegacyValue::Bytes(bytes) if bytes.is_empty() => Some(Identity::ByteSingleton(256)),
            LegacyValue::Bytes(bytes) if bytes.len() == 1 => Some(Identity::ByteSingleton(u16::from(bytes[0]))),
            _ => id,
        };
        // Atomic scalar types are deliberately not memoized.
        if !matches!(
            value,
            LegacyValue::None
                | LegacyValue::Bool(_)
                | LegacyValue::Int(_)
                | LegacyValue::BigInt(_)
                | LegacyValue::Float(_)
        ) {
            if let Some(index) = id.as_ref().and_then(|id| self.memo.get(id)).copied() {
                self.get(index);
                return;
            }
        }
        match value {
            LegacyValue::None => self.write(b"N"),
            LegacyValue::Bool(v) => self.write(&[if *v { 0x88 } else { 0x89 }]),
            LegacyValue::Int(v) => {
                let mut bytes = Vec::new();
                integer(*v, &mut bytes);
                self.write(&bytes);
            }
            LegacyValue::BigInt(v) => {
                let bytes = v.to_signed_bytes_le();
                if let Ok(n) = u8::try_from(bytes.len()) {
                    self.write(&[0x8a, n]);
                } else {
                    self.write(&[0x8b]);
                    self.write(&(bytes.len() as u32).to_le_bytes());
                }
                self.write(&bytes);
            }
            LegacyValue::Float(v) => {
                self.write(b"G");
                self.write(&v.to_be_bytes());
            }
            LegacyValue::Str(v) => self.string(v, id),
            LegacyValue::Unicode(v) => {
                self.sized(0x8c, b'X', 0x8d, &v.to_wtf8());
                self.memoize(id);
            }
            LegacyValue::Bytes(v) => {
                self.sized(b'C', b'B', 0x8e, v);
                self.memoize(id);
            }
            LegacyValue::ByteArray(v) => {
                let mut header = vec![0x96];
                header.extend_from_slice(&(v.len() as u64).to_le_bytes());
                if v.len() >= 65536 {
                    self.commit();
                    self.out.extend_from_slice(&header);
                    self.out.extend_from_slice(v);
                } else {
                    self.write(&header);
                    self.write(v);
                }
                self.memoize(id);
            }
            LegacyValue::List(v) => {
                self.write(b"]");
                self.memoize(id);
                if v.len() == 1 {
                    self.value(&v[0], None);
                    self.write(b"a");
                } else {
                    for batch in v.chunks(1000) {
                        self.write(b"(");
                        for v in batch {
                            self.value(v, None);
                        }
                        self.write(b"e");
                    }
                }
            }
            LegacyValue::Tuple(v) => self.tuple(v, id),
            LegacyValue::Dict(v) => {
                self.write(b"}");
                self.memoize(id);
                let entries = v.iter_native().collect::<Vec<_>>();
                if entries.len() == 1 {
                    self.value(&entries[0].0, None);
                    self.value(entries[0].1, None);
                    self.write(b"s");
                } else if !entries.is_empty() {
                    for batch in entries.chunks(1000) {
                        self.write(b"(");
                        for (k, v) in batch {
                            self.value(k, None);
                            self.value(v, None);
                        }
                        self.write(b"u");
                    }
                    if entries.len().is_multiple_of(1000) {
                        self.write(b"(u");
                    }
                }
            }
            LegacyValue::Set(v) => {
                self.write(&[0x8f]);
                self.memoize(id);
                for batch in v.chunks(1000) {
                    self.write(b"(");
                    for v in batch {
                        self.value(v, None);
                    }
                    self.write(&[0x90]);
                }
                if !v.is_empty() && v.len().is_multiple_of(1000) {
                    self.write(&[b'(', 0x90]);
                }
            }
            LegacyValue::FrozenSet(v) => {
                self.write(b"(");
                for v in v {
                    self.value(v, None);
                }
                if let Some(index) = id.as_ref().and_then(|id| self.memo.get(id)).copied() {
                    self.write(b"1");
                    self.get(index);
                } else {
                    self.write(&[0x91]);
                    self.memoize(id);
                }
            }
            LegacyValue::Global(v) => {
                let index = self.global(v);
                if let Some(id) = id {
                    self.memo.insert(id, index);
                }
            }
            LegacyValue::DateTime(v) => {
                self.global(&NativeGlobal { module: "datetime".into(), name: "datetime".into() });
                let year = v.year.to_be_bytes();
                let micros = v.microsecond.to_be_bytes();
                self.value(
                    &LegacyValue::Bytes(vec![
                        year[0],
                        year[1],
                        v.month | (u8::from(v.fold) << 7),
                        v.day,
                        v.hour,
                        v.minute,
                        v.second,
                        micros[1],
                        micros[2],
                        micros[3],
                    ]),
                    None,
                );
                self.write(&[0x85]);
                self.memoize(None);
                self.write(b"R");
                self.memoize(id);
            }
            LegacyValue::TimeDelta(v) => {
                self.global(&NativeGlobal { module: "datetime".into(), name: "timedelta".into() });
                self.tuple(
                    &[
                        LegacyValue::Int(i64::from(v.days)),
                        LegacyValue::Int(i64::from(v.seconds)),
                        LegacyValue::Int(i64::from(v.microseconds)),
                    ],
                    None,
                );
                self.write(b"R");
                self.memoize(id);
            }
            LegacyValue::MemoryView(_) => unreachable!("preflight rejects memoryview"),
            LegacyValue::Object(_) => unreachable!("resolved above"),
        }
    }
}
fn integer(value: i64, out: &mut Vec<u8>) {
    if let Ok(value) = u8::try_from(value) {
        out.extend_from_slice(&[b'K', value]);
    } else if let Ok(value) = u16::try_from(value) {
        out.push(b'M');
        out.extend_from_slice(&value.to_le_bytes());
    } else if let Ok(value) = i32::try_from(value) {
        out.push(b'J');
        out.extend_from_slice(&value.to_le_bytes());
    } else {
        // Signed little-endian two's complement, including a sign byte when needed.
        let bytes = value.to_le_bytes();
        let mut length = bytes.len();
        while length > 1
            && ((bytes[length - 1] == 0 && bytes[length - 2] & 0x80 == 0)
                || (bytes[length - 1] == 0xff && bytes[length - 2] & 0x80 != 0))
        {
            length -= 1;
        }
        out.extend_from_slice(&[0x8a, length as u8]);
        out.extend_from_slice(&bytes[..length]);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::pickle::decode;
    use crate::import::value::{DateTimeParts, Dict};

    pub(crate) fn sample() -> LegacyValue {
        let mut values = Dict::new();
        values.insert("null", LegacyValue::None);
        values.insert("bool", LegacyValue::Bool(true));
        values.insert("float", LegacyValue::Float(1.0));
        values.insert("int", LegacyValue::Int(1));
        values.insert("text", LegacyValue::Str("Sound 🎮, #1\nsecond line".to_owned()));
        values.insert("bytes", LegacyValue::Bytes(vec![0, 128, 255]));
        values.insert("list", LegacyValue::List(vec![0.into(), "first".into()]));
        values.insert("tuple", LegacyValue::Tuple(vec![false.into(), (-0.5).into()]));
        values.insert("set", LegacyValue::Set(vec![3.into(), 5.into()]));
        values.insert(
            "datetime",
            LegacyValue::DateTime(DateTimeParts {
                year: 2026,
                month: 11,
                day: 10,
                hour: 14,
                minute: 5,
                second: 12,
                microsecond: 123456,
                fold: false,
            }),
        );
        values.insert(Key::Int(-7), LegacyValue::Tuple(vec![]));
        LegacyValue::Dict(values)
    }

    #[test]
    fn preserves_types_and_order() {
        let sample = sample();
        assert_eq!(decode(&encode(&sample)).unwrap(), sample);
        for value in [
            i64::MIN,
            i32::MIN as i64 - 1,
            -32769,
            -129,
            -1,
            0,
            255,
            256,
            65535,
            65536,
            i32::MAX as i64,
            i32::MAX as i64 + 1,
            i64::MAX,
        ] {
            let value = LegacyValue::Int(value);
            assert_eq!(decode(&encode(&value)).unwrap(), value);
        }
        for value in [
            LegacyValue::Str("x".repeat(1000)),
            LegacyValue::Bytes(vec![255; 1000]),
            LegacyValue::List(vec![]),
            LegacyValue::Set(vec![]),
        ] {
            assert_eq!(decode(&encode(&value)).unwrap(), value);
        }
    }

    /// An independent reader checks the emitted opcodes and value types, rather than this crate's decoder.
    #[test]
    #[ignore = "requires python3; run explicitly for storage interoperability"]
    fn python_restricted_unpickler_reads_output() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let script = r#"
import datetime, io, pickle, sys
class Restricted(pickle.Unpickler):
    def find_class(self, module, name):
        assert (module, name) == ('datetime', 'datetime')
        return datetime.datetime
value = Restricted(io.BytesIO(sys.stdin.buffer.read())).load()
assert list(value) == ['null', 'bool', 'float', 'int', 'text', 'bytes', 'list', 'tuple', 'set', 'datetime', -7]
assert value['null'] is None and value['bool'] is True
assert type(value['float']) is float and type(value['int']) is int
assert value['text'] == 'Sound 🎮, #1\nsecond line'
assert value['bytes'] == bytes([0,128,255])
assert value['list'] == [0,'first'] and value['tuple'] == (False,-0.5)
assert value['set'] == {3,5} and value[-7] == ()
assert value['datetime'] == datetime.datetime(2026,11,10,14,5,12,123456)
"#;
        let mut child = Command::new("python3").args(["-c", script]).stdin(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(&encode(&sample())).unwrap();
        assert!(child.wait().unwrap().success());
    }
}
