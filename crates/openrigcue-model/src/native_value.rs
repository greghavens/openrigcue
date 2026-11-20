//! Dynamically typed values read from imported Sim Racing Studio files.
//!
//! The imported files hold nested dictionaries: `config.ini` is an INI file of nested string
//! sections, `profile.db`/`metadata.db` are SQLite tables of pickled dicts, and `.srs` profiles are
//! JSON. [`LegacyValue`] models exactly the value kinds those files can contain, keeping the
//! distinctions (int vs float, insertion-ordered dicts) that the migrations depend on.

pub use crate::native_object::NativeObject;
pub use crate::python_string::PythonString;
pub use num_bigint::BigInt;
use num_traits::{FromPrimitive, ToPrimitive, Zero};
use std::fmt;

/// A restricted global is data, never an executable Rust or user-code pointer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NativeGlobal {
    pub module: String,
    pub name: String,
}

/// Normalized duration fields (days, seconds, microseconds) produced by date arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TimeDeltaParts {
    pub days: i32,
    pub seconds: u32,
    pub microseconds: u32,
}

/// A read-only buffer view, as stored in a pickle file, over an in-band bytearray.
/// Its buffer object remains live; no buffer callback or user code is run.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeBufferView {
    pub buffer: LegacyValue,
    pub readonly: bool,
}
impl NativeBufferView {
    pub fn new(buffer: LegacyValue, readonly: bool) -> std::sync::Arc<Self> {
        let buffer = if matches!(buffer, LegacyValue::Object(_)) {
            buffer
        } else {
            LegacyValue::Object(NativeObject::new(buffer))
        };
        let view = std::sync::Arc::new(Self { buffer, readonly });
        if let LegacyValue::Object(object) = &view.buffer {
            let mut views = buffer_views().lock().unwrap_or_else(|e| e.into_inner());
            views.retain(|(_, view)| view.strong_count() != 0);
            views.push((object.id(), std::sync::Arc::downgrade(&view)));
        }
        view
    }
}
type BufferViews = std::sync::Mutex<Vec<(usize, std::sync::Weak<NativeBufferView>)>>;
fn buffer_views() -> &'static BufferViews {
    static VIEWS: std::sync::OnceLock<BufferViews> = std::sync::OnceLock::new();
    VIEWS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NativeKey {
    BigInt(BigInt),
    Unicode(PythonString),
    Bytes(Vec<u8>),
    Tuple(Vec<Key>),
    FrozenSet(Vec<Key>),
    DateTime(DateTimeParts),
    Global(NativeGlobal),
    Object(NativeObject),
    TimeDelta(TimeDeltaParts),
}

/// A non-NaN float key with its stored bits intact. NaNs use object keys
/// so dictionary membership retains their distinct identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FloatKey(u64);

impl FloatKey {
    pub fn new(value: f64) -> Option<Self> {
        (!value.is_nan()).then_some(Self(value.to_bits()))
    }

    pub fn value(self) -> f64 {
        f64::from_bits(self.0)
    }
}

/// A represented dictionary key, retaining its original kind independently from text names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Key {
    Str(String),
    Int(i64),
    Bool(bool),
    Float(FloatKey),
    None,
    Extra(NativeKey),
}

impl Key {
    /// Hashable builtin keys, keyed by value except NaN floats and tuple/frozenset objects, which keep their identity. No
    /// user-defined hash, equality or other callable is executed.
    pub fn from_value(value: &LegacyValue) -> Result<Self, String> {
        match value {
            LegacyValue::Object(object) => {
                let resolved = object.resolved();
                let key = Self::from_value(&resolved)?;
                if matches!(resolved, LegacyValue::Tuple(_) | LegacyValue::FrozenSet(_))
                    || matches!(resolved,LegacyValue::Float(v)if v.is_nan())
                {
                    Ok(Self::Extra(NativeKey::Object(object.clone())))
                } else {
                    Ok(key)
                }
            }
            LegacyValue::None => Ok(Self::None),
            LegacyValue::Bool(v) => Ok(Self::Bool(*v)),
            LegacyValue::Int(v) => Ok(Self::Int(*v)),
            LegacyValue::BigInt(v) => Ok(Self::Extra(NativeKey::BigInt(v.clone()))),
            LegacyValue::Str(v) => Ok(Self::Str(v.clone())),
            LegacyValue::Unicode(v) => Ok(Self::Extra(NativeKey::Unicode(v.clone()))),
            LegacyValue::Bytes(v) => Ok(Self::Extra(NativeKey::Bytes(v.clone()))),
            LegacyValue::Float(v) => Self::float(*v)
                .or_else(|| Some(Self::Extra(NativeKey::Object(NativeObject::new(LegacyValue::Float(*v))))))
                .ok_or_else(|| "invalid float key".into()),
            LegacyValue::Tuple(v) => {
                Ok(Self::Extra(NativeKey::Tuple(v.iter().map(Self::from_value).collect::<Result<_, _>>()?)))
            }
            LegacyValue::FrozenSet(v) => {
                Ok(Self::Extra(NativeKey::FrozenSet(v.iter().map(Self::from_value).collect::<Result<_, _>>()?)))
            }
            LegacyValue::DateTime(v) => Ok(Self::Extra(NativeKey::DateTime(*v))),
            LegacyValue::Global(v) => Ok(Self::Extra(NativeKey::Global(v.clone()))),
            LegacyValue::TimeDelta(v) => Ok(Self::Extra(NativeKey::TimeDelta(*v))),
            LegacyValue::MemoryView(view) => {
                if !view.readonly {
                    return Err("cannot hash writable memoryview object".into());
                }
                if matches!(view.buffer.resolved(), LegacyValue::ByteArray(_)) {
                    return Err("unhashable type: 'bytearray'".into());
                }
                Ok(Self::Extra(NativeKey::Object(NativeObject::new(value.clone()))))
            }
            value => Err(format!("unhashable type: '{}'", value.type_name())),
        }
    }
    pub fn float(value: f64) -> Option<Self> {
        FloatKey::new(value).map(Self::Float)
    }

    /// The key rendered as display text.
    pub fn to_text(&self) -> String {
        match self {
            Key::Str(s) => s.clone(),
            Key::Int(i) => i.to_string(),
            Key::Bool(value) => if *value { "True" } else { "False" }.into(),
            Key::Float(value) => py_float_repr(value.value()),
            Key::None => "None".into(),
            Key::Extra(value) => crate::python_repr::str_value(&value.to_value()),
        }
    }

    pub fn to_value(&self) -> LegacyValue {
        match self {
            Key::Str(value) => LegacyValue::Str(value.clone()),
            Key::Int(value) => LegacyValue::Int(*value),
            Key::Bool(value) => LegacyValue::Bool(*value),
            Key::Float(value) => LegacyValue::Float(value.value()),
            Key::None => LegacyValue::None,
            Key::Extra(value) => value.to_value(),
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Key::Str(_) => "str",
            Key::Int(_) => "int",
            Key::Bool(_) => "bool",
            Key::Float(_) => "float",
            Key::None => "NoneType",
            Key::Extra(value) => value.type_name(),
        }
    }

    /// The qualified type name used in builtin arithmetic/conversion
    /// errors. JSON's `Object of type ...` messages use `type_name` instead.
    pub fn qualified_type_name(&self) -> &'static str {
        self.to_value().qualified_type_name()
    }

    pub fn json_text(&self) -> String {
        match self {
            Key::None => "null".into(),
            Key::Bool(value) => value.to_string(),
            Key::Float(value) if value.value().is_infinite() => {
                if value.value().is_sign_negative() { "-Infinity" } else { "Infinity" }.into()
            }
            key => key.to_text(),
        }
    }

    /// Builtin key ordering. None is only sortable when it is the sole dictionary key.
    pub fn native_cmp(&self, other: &Self) -> Result<std::cmp::Ordering, String> {
        Ok(crate::python_compare::numeric_cmp(&self.to_value(), &other.to_value())?
            .unwrap_or(std::cmp::Ordering::Equal))
    }

    /// Dictionary lookup equality retains numeric aliases while structural equality keeps key kinds.
    fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Str(left), Self::Str(right)) => return left == right,
            (Self::Int(left), Self::Int(right)) => return left == right,
            (Self::Bool(left), Self::Bool(right)) => return left == right,
            (Self::None, Self::None) => return true,
            (Self::Float(left), Self::Float(right)) => return left.value() == right.value(),
            _ => {}
        }
        let left = self.to_value();
        let right = other.to_value();
        left.same_object(&right) || crate::python_compare::native_equal(&left, &right)
    }
}

impl NativeKey {
    pub fn to_value(&self) -> LegacyValue {
        match self {
            Self::BigInt(v) => LegacyValue::BigInt(v.clone()),
            Self::Unicode(v) => LegacyValue::Unicode(v.clone()),
            Self::Bytes(v) => LegacyValue::Bytes(v.clone()),
            Self::Tuple(v) => LegacyValue::Tuple(v.iter().map(Key::to_value).collect()),
            Self::FrozenSet(v) => LegacyValue::FrozenSet(v.iter().map(Key::to_value).collect()),
            Self::DateTime(v) => LegacyValue::DateTime(*v),
            Self::Global(v) => LegacyValue::Global(v.clone()),
            Self::Object(v) => LegacyValue::Object(v.clone()),
            Self::TimeDelta(v) => LegacyValue::TimeDelta(*v),
        }
    }
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::BigInt(_) => "int",
            Self::Unicode(_) => "str",
            Self::Bytes(_) => "bytes",
            Self::Tuple(_) => "tuple",
            Self::FrozenSet(_) => "frozenset",
            Self::DateTime(_) => "datetime",
            Self::Global(_) => "type",
            Self::Object(v) => v.read(LegacyValue::type_name),
            Self::TimeDelta(_) => "timedelta",
        }
    }
}

impl From<&str> for Key {
    fn from(s: &str) -> Self {
        Key::Str(s.to_owned())
    }
}

impl From<String> for Key {
    fn from(s: String) -> Self {
        Key::Str(s)
    }
}

/// Calendar fields of a pickled `datetime.datetime` (naive, without a time zone).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DateTimeParts {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub microsecond: u32,
    pub fold: bool,
}

/// An insertion-ordered dictionary: assigning an existing key keeps
/// its position, a new key is appended, removal closes the gap.
#[derive(Debug, Clone, Default)]
pub struct Dict {
    entries: Vec<(Key, LegacyValue)>,
    // Key is the compatibility lookup shape; the first-assigned key object is kept
    // separately so keys()/pickle can retain aliases without changing Key::Str.
    native_keys: Vec<Option<LegacyValue>>,
}

impl PartialEq for Dict {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl Dict {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Key, &LegacyValue)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&Key, &mut LegacyValue)> {
        self.entries.iter_mut().map(|(k, v)| (&*k, v))
    }

    /// The actual key objects in insertion order. Assigning an equal
    /// key changes its value while retaining the first key's identity.
    pub fn iter_native(&self) -> impl Iterator<Item = (LegacyValue, &LegacyValue)> {
        self.entries
            .iter()
            .zip(&self.native_keys)
            .map(|((key, value), native)| (native.clone().unwrap_or_else(|| key.to_value()), value))
    }

    pub(crate) fn native_keys_mut(&mut self) -> impl Iterator<Item = &mut LegacyValue> {
        self.native_keys.iter_mut().filter_map(Option::as_mut)
    }
    pub(crate) fn native_keys(&self) -> impl Iterator<Item = &LegacyValue> {
        self.native_keys.iter().filter_map(Option::as_ref)
    }

    pub fn insert_native(&mut self, key: LegacyValue, value: LegacyValue) -> Result<(), String> {
        let key = if matches!(key,LegacyValue::Float(v)if v.is_nan()) {
            LegacyValue::from_float(if let LegacyValue::Float(value) = key { value } else { unreachable!() })
        } else {
            key
        };
        let lookup = Key::from_value(&key)?;
        match self.position(&lookup) {
            Some(index) => self.entries[index].1 = value,
            None => {
                self.entries.push((lookup, value));
                self.native_keys.push(Some(key));
            }
        }
        Ok(())
    }

    pub fn keys(&self) -> impl Iterator<Item = &Key> {
        self.entries.iter().map(|(k, _)| k)
    }

    /// String keys in order (non-string keys are skipped).
    pub fn str_keys(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter_map(|(k, _)| match k {
                Key::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    fn position(&self, key: &Key) -> Option<usize> {
        self.entries.iter().position(|(k, _)| k.matches(key))
    }

    pub fn get(&self, key: &str) -> Option<&LegacyValue> {
        self.get_key(&Key::from(key))
    }

    pub fn get_key(&self, key: &Key) -> Option<&LegacyValue> {
        self.position(key).map(|i| &self.entries[i].1)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut LegacyValue> {
        let key = Key::from(key);
        self.position(&key).map(move |i| &mut self.entries[i].1)
    }

    pub fn get_key_mut(&mut self, key: &Key) -> Option<&mut LegacyValue> {
        self.position(key).map(move |index| &mut self.entries[index].1)
    }

    pub fn remove_key(&mut self, key: &Key) -> Option<LegacyValue> {
        self.position(key).map(|index| {
            self.native_keys.remove(index);
            self.entries.remove(index).1
        })
    }

    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Assigns `value` to `key`: an existing key keeps its position, a new key is appended.
    pub fn insert(&mut self, key: impl Into<Key>, value: LegacyValue) {
        let key = key.into();
        if let Key::Extra(NativeKey::Object(object)) = &key {
            if object.read(LegacyValue::type_name) == "str" {
                self.insert_native(LegacyValue::Object(object.clone()), value).expect("native string key");
                return;
            }
        }
        match self.position(&key) {
            Some(i) => self.entries[i].1 = value,
            None => {
                self.entries.push((key, value));
                self.native_keys.push(None);
            }
        }
    }

    /// Removes `key` and returns its value, or `None` when it is missing.
    pub fn remove(&mut self, key: &str) -> Option<LegacyValue> {
        let key = Key::from(key);
        self.remove_key(&key)
    }

    /// Inserts `value` only when `key` is missing.
    pub fn set_default(&mut self, key: &str, value: LegacyValue) {
        if !self.contains(key) {
            self.insert(key, value);
        }
    }

    /// The entry for `key`, created as an empty dict when missing.
    pub fn dict_entry(&mut self, key: &str) -> &mut LegacyValue {
        if !self.contains(key) {
            self.insert(key, LegacyValue::Dict(Dict::new()));
        }
        self.get_mut(key).expect("entry inserted above")
    }
}

impl FromIterator<(Key, LegacyValue)> for Dict {
    fn from_iter<T: IntoIterator<Item = (Key, LegacyValue)>>(iter: T) -> Self {
        let mut dict = Dict::new();
        for (k, v) in iter {
            dict.insert(k, v);
        }
        dict
    }
}

/// A value of one of the kinds the imported settings files can hold.
#[derive(Debug, Clone)]
pub enum LegacyValue {
    None,
    Bool(bool),
    Int(i64),
    BigInt(BigInt),
    Float(f64),
    Str(String),
    Unicode(PythonString),
    Bytes(Vec<u8>),
    ByteArray(Vec<u8>),
    MemoryView(std::sync::Arc<NativeBufferView>),
    List(Vec<LegacyValue>),
    Tuple(Vec<LegacyValue>),
    Set(Vec<LegacyValue>),
    FrozenSet(Vec<LegacyValue>),
    Dict(Dict),
    DateTime(DateTimeParts),
    Global(NativeGlobal),
    TimeDelta(TimeDeltaParts),
    Object(NativeObject),
}

thread_local! {static STRUCTURAL_COMPARING:std::cell::RefCell<std::collections::BTreeSet<(usize,usize)>>=const{std::cell::RefCell::new(std::collections::BTreeSet::new())};}
/// Representation equality keeps value kinds and order, while arena wrappers are
/// transparent. Value-model `==` (including recursive comparison errors) belongs
/// to python_compare::native_equal_checked; identity belongs to same_object.
impl PartialEq for LegacyValue {
    fn eq(&self, other: &Self) -> bool {
        if matches!(self, Self::Object(_)) || matches!(other, Self::Object(_)) {
            let _operation = crate::native_object::native_operation();
            if self.same_object(other) {
                return true;
            }
            let id = |value: &Self| {
                if let Self::Object(object) = value {
                    object.id() << 1
                } else {
                    (value as *const Self as usize) | 1
                }
            };
            let pair = (id(self), id(other));
            if !STRUCTURAL_COMPARING.with(|pairs| pairs.borrow_mut().insert(pair)) {
                return true;
            }
            let equal = self.resolved().eq(&other.resolved());
            STRUCTURAL_COMPARING.with(|pairs| {
                pairs.borrow_mut().remove(&pair);
            });
            return equal;
        }
        match (self, other) {
            (Self::None, Self::None) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::BigInt(a), Self::BigInt(b)) => a == b,
            (Self::Int(a), Self::BigInt(b)) | (Self::BigInt(b), Self::Int(a)) => BigInt::from(*a) == *b,
            (Self::Float(a), Self::Float(b)) => a == b,
            (Self::Str(a), Self::Str(b)) => a == b,
            (Self::Unicode(a), Self::Unicode(b)) => a == b,
            (Self::Str(a), Self::Unicode(b)) | (Self::Unicode(b), Self::Str(a)) => b == a.as_str(),
            (Self::Bytes(a), Self::Bytes(b)) | (Self::ByteArray(a), Self::ByteArray(b)) => a == b,
            (Self::MemoryView(a), Self::MemoryView(b)) => a == b,
            (Self::List(a), Self::List(b))
            | (Self::Tuple(a), Self::Tuple(b))
            | (Self::Set(a), Self::Set(b))
            | (Self::FrozenSet(a), Self::FrozenSet(b)) => a == b,
            (Self::Dict(a), Self::Dict(b)) => a == b,
            (Self::DateTime(a), Self::DateTime(b)) => a == b,
            (Self::Global(a), Self::Global(b)) => a == b,
            (Self::TimeDelta(a), Self::TimeDelta(b)) => a == b,
            _ => false,
        }
    }
}

impl From<&str> for LegacyValue {
    fn from(s: &str) -> Self {
        LegacyValue::Str(s.to_owned())
    }
}

impl From<String> for LegacyValue {
    fn from(s: String) -> Self {
        LegacyValue::Str(s)
    }
}

impl From<i64> for LegacyValue {
    fn from(i: i64) -> Self {
        LegacyValue::Int(i)
    }
}

impl From<f64> for LegacyValue {
    fn from(f: f64) -> Self {
        LegacyValue::from_float(f)
    }
}

impl From<bool> for LegacyValue {
    fn from(b: bool) -> Self {
        LegacyValue::Bool(b)
    }
}

impl From<Dict> for LegacyValue {
    fn from(d: Dict) -> Self {
        LegacyValue::Dict(d)
    }
}

impl LegacyValue {
    /// Current bytes of a buffer value, retaining a live mutable-buffer view.
    pub fn buffer_bytes(&self) -> Option<Vec<u8>> {
        match self {
            Self::Bytes(v) | Self::ByteArray(v) => Some(v.clone()),
            Self::MemoryView(v) => v.buffer.buffer_bytes(),
            Self::Object(v) => v.read(Self::buffer_bytes),
            _ => None,
        }
    }
    /// A bytearray cannot change length while any live view exports it.
    pub fn has_buffer_exports(&self) -> bool {
        if let Self::Object(object) = self {
            let mut views = buffer_views().lock().unwrap_or_else(|e| e.into_inner());
            views.retain(|(_, view)| view.strong_count() != 0);
            views.iter().any(|(id, _)| *id == object.id())
        } else {
            false
        }
    }
    /// Construct a new float object. NaN identity is observable through
    /// container membership even though a NaN's direct equality stays false.
    pub fn from_float(value: f64) -> Self {
        if value.is_nan() {
            Self::Object(NativeObject::new(Self::Float(value)))
        } else {
            Self::Float(value)
        }
    }
    /// Resolve one arena object. Child objects remain rooted handles, so a
    /// cloned snapshot retains live aliases instead of recursively detaching.
    /// Borrowed `as_dict`/`as_str` inspect ordinary variants only; typed
    /// consumers use this method or `item_owned` at each actual read boundary.
    pub fn resolved(&self) -> Self {
        match self {
            Self::Object(object) => object.resolved(),
            value => value.clone(),
        }
    }
    /// Object identity for container membership and identity fast paths.
    /// This does not make direct NaN equality true.
    pub fn same_object(&self, other: &Self) -> bool {
        matches!((self,other),(Self::Object(a),Self::Object(b))if a==b)
    }
    /// Required string-key lookup preserving child identity. Errors use the
    /// existing `item` message contract; callers retain their missing-key/wrong-type
    /// boundary. No lock is held after this operation returns.
    pub fn item_owned(&self, key: &str) -> Result<Self, String> {
        if let Self::Object(object) = self {
            return object.item_owned(key);
        }
        self.item(key).cloned()
    }

    /// Insertion-ordered mapping keys without cloning sibling values. Key
    /// handles retain identity; use lookup_owned for each subsequent read.
    pub fn dict_keys_owned(&self) -> Result<Vec<Key>, String> {
        match self {
            Self::Object(object) => object.dict_keys_owned(),
            Self::Dict(dict) => dict
                .iter_native()
                .map(|(key, _)| {
                    if let Self::Object(object) = key {
                        Ok(Key::Extra(NativeKey::Object(object)))
                    } else {
                        Key::from_value(&key)
                    }
                })
                .collect(),
            value => Err(format!("AttributeError: '{}' object has no attribute 'keys'", value.qualified_type_name())),
        }
    }
    /// Full subscription for dictionary and sequence keys, including
    /// negative/arbitrary-width indexes and unpaired-surrogate string cells.
    /// Errors include the error class name.
    pub fn lookup_owned(&self, key: &Self) -> Result<Self, String> {
        let _operation = crate::native_object::native_operation();
        if let Self::Object(object) = self {
            return object.lookup_owned(key);
        }
        if let Self::Dict(dict) = self {
            let key = Key::from_value(key).map_err(|e| format!("TypeError: {e}"))?;
            return dict
                .get_key(&key)
                .cloned()
                .ok_or_else(|| format!("KeyError: {}", crate::python_repr::repr(&key.to_value())));
        }
        if !matches!(
            self,
            Self::List(_)
                | Self::Tuple(_)
                | Self::Bytes(_)
                | Self::ByteArray(_)
                | Self::MemoryView(_)
                | Self::Str(_)
                | Self::Unicode(_)
        ) {
            return Err(format!("TypeError: '{}' object is not subscriptable", self.qualified_type_name()));
        }
        let length = self.py_len().expect("sequence length");
        let mut index = key.integer().ok_or_else(|| match self {
            Self::Str(_) | Self::Unicode(_) => {
                format!("TypeError: string indices must be integers, not '{}'", key.qualified_type_name())
            }
            Self::Bytes(_) => {
                format!("TypeError: byte indices must be integers or slices, not {}", key.qualified_type_name())
            }
            Self::MemoryView(_) => "TypeError: memoryview: invalid slice key".into(),
            _ => format!(
                "TypeError: {} indices must be integers or slices, not {}",
                self.type_name(),
                key.qualified_type_name()
            ),
        })?;
        if index.to_i64().is_none() {
            return Err(format!("IndexError: cannot fit '{}' into an index-sized integer", key.qualified_type_name()));
        }
        if index < BigInt::from(0) {
            index += length;
        }
        let index = index.to_usize().filter(|&i| i < length).ok_or_else(|| match self {
            Self::Str(_) | Self::Unicode(_) => "IndexError: string index out of range".into(),
            Self::Bytes(_) => "IndexError: index out of range".into(),
            Self::MemoryView(_) => "IndexError: index out of bounds on dimension 1".into(),
            _ => format!("IndexError: {} index out of range", self.type_name()),
        })?;
        match self {
            Self::List(v) | Self::Tuple(v) => Ok(v[index].clone()),
            Self::Bytes(v) | Self::ByteArray(v) => Ok(Self::Int(i64::from(v[index]))),
            Self::MemoryView(v) => Ok(Self::Int(i64::from(v.buffer.buffer_bytes().expect("live buffer")[index]))),
            Self::Str(_) | Self::Unicode(_) => Ok(Self::from_python_string(
                PythonString::from_codepoints(vec![self.python_string().expect("string").codepoints()[index]])
                    .expect("codepoint"),
            )),
            _ => Err(format!("TypeError: '{}' object is not subscriptable", self.type_name())),
        }
    }
    /// Read a list/tuple cell at a nonnegative index without detaching
    /// the returned child object. Used by live device list consumers.
    pub fn get_index(&self, index: usize) -> Result<Self, String> {
        if let Self::Object(object) = self {
            return object.get_index(index);
        }
        self.lookup_owned(&Self::from_integer(BigInt::from(index)))
    }
    /// The builtin `.copy()` methods used by profile loading/export. This is
    /// a shallow copy: nested object handles keep their live identities.
    pub fn copy_builtin(&self) -> Result<Self, String> {
        let value = self.resolved();
        match value {
            Self::Dict(_) | Self::List(_) | Self::Set(_) | Self::ByteArray(_) => Ok(value),
            Self::FrozenSet(_) => Ok(self.clone()),
            Self::Global(ref global) => {
                let class = if global.module == "builtins" {
                    global.name.clone()
                } else {
                    format!("{}.{}", global.module, global.name)
                };
                if ["dict", "list", "set", "frozenset"].contains(&class.as_str()) {
                    Err(format!("TypeError: unbound method {class}.copy() needs an argument"))
                } else {
                    Err(format!("AttributeError: type object '{class}' has no attribute 'copy'"))
                }
            }
            value => Err(format!("AttributeError: '{}' object has no attribute 'copy'", value.type_name())),
        }
    }
    /// Replace one cell in the same live list. Per-slot publication is
    /// visible immediately to aliases; it is not deferred to a detached copy.
    pub fn assign_index(&mut self, index: usize, value: Self) -> Result<(), String> {
        let _operation = crate::native_object::native_operation();
        if let Self::Object(object) = self {
            // Convert before taking the cell's write lock: an invalid value
            // may itself alias this bytearray.
            let value = if matches!(object.resolved(), Self::ByteArray(_)) {
                Self::Int(i64::from(
                    value
                        .integer()
                        .ok_or_else(|| {
                            format!(
                                "TypeError: '{}' object cannot be interpreted as an integer",
                                value.qualified_type_name()
                            )
                        })?
                        .to_u8()
                        .ok_or_else(|| "ValueError: byte must be in range(0, 256)".to_owned())?,
                ))
            } else {
                value
            };
            return object.edit(|target| target.assign_index(index, value));
        }
        match self {
            Self::Object(object) => object.edit(|target| target.assign_index(index, value)),
            Self::List(values) => {
                let target =
                    values.get_mut(index).ok_or_else(|| "IndexError: list assignment index out of range".to_owned())?;
                *target = value;
                Ok(())
            }
            Self::ByteArray(values) => {
                let byte = value
                    .integer()
                    .ok_or_else(|| {
                        format!(
                            "TypeError: '{}' object cannot be interpreted as an integer",
                            value.qualified_type_name()
                        )
                    })?
                    .to_u8()
                    .ok_or_else(|| "ValueError: byte must be in range(0, 256)".to_owned())?;
                *values.get_mut(index).ok_or_else(|| "IndexError: bytearray index out of range".to_owned())? = byte;
                Ok(())
            }
            Self::MemoryView(view) => {
                if view.readonly {
                    Err("TypeError: cannot modify read-only memory".into())
                } else {
                    view.buffer.clone().assign_index(index, value)
                }
            }
            v => Err(format!("TypeError: '{}' object does not support item assignment", v.qualified_type_name())),
        }
    }

    /// `container[key] = value` with a key of any value kind. Profile import must keep
    /// numeric, surrogate and NaN keys, rather than converting them to names.
    pub fn assign_key(&mut self, key: Self, value: Self) -> Result<(), String> {
        let _operation = crate::native_object::native_operation();
        if let Self::Object(object) = self {
            // Hash/convert outside the target's cell lock: key/value may alias
            // that very container, and failed conversions must not deadlock.
            let kind = object.read(Self::type_name);
            let key = if kind == "dict" {
                // Validate before taking the cell lock, but keep the actual
                // key allocation: a memoized string may also be a row value.
                Key::from_value(&key).map_err(|detail| {
                    format!(
                        "{}: {detail}",
                        if detail == "cannot hash writable memoryview object" { "ValueError" } else { "TypeError" }
                    )
                })?;
                key
            } else {
                if kind == "memoryview" && object.read(|value| matches!(value,Self::MemoryView(view)if view.readonly)) {
                    return Err("TypeError: cannot modify read-only memory".into());
                }
                if matches!(kind, "list" | "bytearray" | "memoryview") {
                    let index = key.integer().ok_or_else(|| {
                        if kind == "memoryview" {
                            "TypeError: memoryview: invalid slice key".into()
                        } else {
                            format!(
                                "TypeError: {kind} indices must be integers or slices, not {}",
                                key.qualified_type_name()
                            )
                        }
                    })?;
                    Self::Int(index.to_i64().ok_or_else(|| {
                        format!("IndexError: cannot fit '{}' into an index-sized integer", key.qualified_type_name())
                    })?)
                } else {
                    key.resolved()
                }
            };
            let value = if kind == "bytearray" {
                Self::Int(i64::from(
                    value
                        .integer()
                        .ok_or_else(|| {
                            format!(
                                "TypeError: '{}' object cannot be interpreted as an integer",
                                value.qualified_type_name()
                            )
                        })?
                        .to_u8()
                        .ok_or_else(|| "ValueError: byte must be in range(0, 256)".to_owned())?,
                ))
            } else {
                value
            };
            return object.edit(|target| target.assign_key(key, value));
        }
        if let Self::Dict(dict) = self {
            return dict.insert_native(key, value).map_err(|detail| {
                format!(
                    "{}: {detail}",
                    if detail == "cannot hash writable memoryview object" { "ValueError" } else { "TypeError" }
                )
            });
        }
        if matches!(self,Self::MemoryView(view)if view.readonly) {
            return Err("TypeError: cannot modify read-only memory".into());
        }
        if !matches!(self, Self::List(_) | Self::ByteArray(_) | Self::MemoryView(_)) {
            return Err(format!("TypeError: '{}' object does not support item assignment", self.qualified_type_name()));
        }
        let index = key.integer().ok_or_else(|| {
            format!(
                "TypeError: {} indices must be integers or slices, not {}",
                self.type_name(),
                key.qualified_type_name()
            )
        })?;
        let mut index = index.to_i64().ok_or_else(|| {
            format!("IndexError: cannot fit '{}' into an index-sized integer", key.qualified_type_name())
        })?;
        let value = if matches!(self, Self::ByteArray(_)) {
            Self::Int(i64::from(
                value
                    .integer()
                    .ok_or_else(|| {
                        format!(
                            "TypeError: '{}' object cannot be interpreted as an integer",
                            value.qualified_type_name()
                        )
                    })?
                    .to_u8()
                    .ok_or_else(|| "ValueError: byte must be in range(0, 256)".to_owned())?,
            ))
        } else {
            value
        };
        if index < 0 {
            index = index.saturating_add(self.py_len().expect("sequence") as i64);
        }
        let index = usize::try_from(index).map_err(|_| {
            if matches!(self, Self::ByteArray(_)) {
                "IndexError: bytearray index out of range".into()
            } else {
                format!("IndexError: {} assignment index out of range", self.type_name())
            }
        })?;
        self.assign_index(index, value)
    }
    /// Bytearray.extend over the decoder's concrete slice. Conversion happens
    /// before resizing, and exported views prohibit the actual resize.
    pub fn extend_bytearray(&mut self, items: &[Self]) -> Result<(), String> {
        let bytes = items
            .iter()
            .map(|value| {
                value
                    .integer()
                    .ok_or_else(|| {
                        format!(
                            "TypeError: '{}' object cannot be interpreted as an integer",
                            value.qualified_type_name()
                        )
                    })?
                    .to_u8()
                    .ok_or_else(|| "ValueError: byte must be in range(0, 256)".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !bytes.is_empty() && self.has_buffer_exports() {
            return Err("BufferError: Existing exports of data: object cannot be re-sized".into());
        }
        match self {
            Self::Object(object) => object.edit(|value| {
                if let Self::ByteArray(values) = value {
                    values.extend(bytes);
                    Ok(())
                } else {
                    Err(format!("AttributeError: '{}' object has no attribute 'extend'", value.qualified_type_name()))
                }
            }),
            Self::ByteArray(values) => {
                values.extend(bytes);
                Ok(())
            }
            v => Err(format!("AttributeError: '{}' object has no attribute 'extend'", v.qualified_type_name())),
        }
    }
    /// Assign a dictionary path without detaching an aliased parent.
    /// Each object read releases its lock before following another edge, so
    /// recursive aliases are legal and retain their original mutations.
    pub fn assign_path(&mut self, path: &[&str], value: Self) -> Result<(), String> {
        let Some((key, tail)) = path.split_first() else {
            *self = value;
            return Ok(());
        };
        if tail.is_empty() {
            return self.assign_item(key, value);
        }
        if matches!(self, Self::Object(_)) {
            return self.item_owned(key)?.assign_path(tail, value);
        }
        match self {
            Self::Dict(fields) => {
                fields.get_mut(key).ok_or_else(|| crate::python_repr::repr_str(key))?.assign_path(tail, value)
            }
            v => Err(format!("'{}' object is not subscriptable", v.type_name())),
        }
    }
    /// Delete a dictionary member through live aliases, retaining the
    /// missing-key and unsupported-parent failures.
    pub fn remove_path(&mut self, path: &[&str]) -> Result<Self, String> {
        let Some((key, tail)) = path.split_first() else {
            return Err("TypeError: empty deletion path".into());
        };
        if tail.is_empty() {
            return match self {
                Self::Object(object) => object.edit(|value| value.remove_path(path)),
                Self::Dict(dict) => {
                    dict.remove(key).ok_or_else(|| format!("KeyError: {}", crate::python_repr::repr_str(key)))
                }
                value => Err(format!("TypeError: '{}' object does not support item deletion", value.type_name())),
            };
        }
        if matches!(self, Self::Object(_)) {
            return self.item_owned(key)?.remove_path(tail);
        }
        match self {
            Self::Dict(fields) => {
                fields.get_mut(key).ok_or_else(|| crate::python_repr::repr_str(key))?.remove_path(tail)
            }
            v => Err(format!("'{}' object is not subscriptable", v.type_name())),
        }
    }

    /// Remove a key of any value kind while retaining the removed child's actual root.
    /// Hash/index conversion precedes locking the target, since a key can
    /// itself alias that target. Used by source-ordered migration rekeying.
    pub fn remove_key(&mut self, key: &Self) -> Result<Self, String> {
        let _operation = crate::native_object::native_operation();
        if let Self::Object(object) = self {
            let key = if object.read(Self::type_name) == "dict" {
                Key::from_value(key).map_err(|detail| format!("TypeError: {detail}"))?.to_value()
            } else {
                key.resolved()
            };
            if object.read(Self::type_name) == "bytearray" && Self::Object(object.clone()).has_buffer_exports() {
                if let Some(index) = key.integer().and_then(|index| index.to_i64()) {
                    let length = object.read(|value| value.py_len().expect("bytearray"));
                    let index = if index < 0 { i128::from(index) + length as i128 } else { i128::from(index) };
                    if index >= 0 && index < length as i128 {
                        return Err("BufferError: Existing exports of data: object cannot be re-sized".into());
                    }
                }
            }
            return object.edit(|value| value.remove_key(&key));
        }
        if let Self::Dict(dict) = self {
            let lookup = Key::from_value(key).map_err(|detail| format!("TypeError: {detail}"))?;
            return dict.remove_key(&lookup).ok_or_else(|| format!("KeyError: {}", crate::python_repr::repr(key)));
        }
        if matches!(self, Self::MemoryView(_)) {
            return Err("TypeError: cannot delete memory".into());
        }
        if !matches!(self, Self::List(_) | Self::ByteArray(_)) {
            return Err(format!("TypeError: '{}' object does not support item deletion", self.qualified_type_name()));
        }
        let integer = key.integer().ok_or_else(|| {
            format!(
                "TypeError: {} indices must be integers or slices, not {}",
                self.type_name(),
                key.qualified_type_name()
            )
        })?;
        let index = integer.to_i64().ok_or_else(|| {
            format!("IndexError: cannot fit '{}' into an index-sized integer", key.qualified_type_name())
        })?;
        let length = self.py_len().expect("sequence");
        let index = if index < 0 { i128::from(index) + length as i128 } else { i128::from(index) };
        let index = usize::try_from(index).ok().filter(|&index| index < length).ok_or_else(|| {
            format!(
                "IndexError: {} {}index out of range",
                self.type_name(),
                if matches!(self, Self::List(_)) { "assignment " } else { "" }
            )
        })?;
        match self {
            Self::List(values) => Ok(values.remove(index)),
            Self::ByteArray(values) => Ok(Self::Int(i64::from(values.remove(index)))),
            _ => unreachable!("sequence type"),
        }
    }
    pub fn from_integer(value: BigInt) -> Self {
        match value.to_i64() {
            Some(value) => Self::Int(value),
            None => Self::BigInt(value),
        }
    }
    pub fn integer(&self) -> Option<BigInt> {
        match self {
            Self::Int(v) => Some((*v).into()),
            Self::BigInt(v) => Some(v.clone()),
            Self::Bool(v) => Some(u8::from(*v).into()),
            Self::Object(v) => v.read(Self::integer),
            _ => None,
        }
    }
    pub fn py_integer(&self) -> Result<BigInt, String> {
        if let Self::Object(object) = self {
            return object.read(Self::py_integer);
        }
        if let Some(value) = self.integer() {
            return Ok(value);
        }
        match self {
            Self::Float(value) if value.is_nan() => Err("ValueError: cannot convert float NaN to integer".into()),
            Self::Float(value) if value.is_infinite() => {
                Err("OverflowError: cannot convert float infinity to integer".into())
            }
            Self::Float(value) => {
                BigInt::from_f64(value.trunc()).ok_or_else(|| "OverflowError: cannot convert float to integer".into())
            }
            Self::Str(value) => integer_text_native(value, self),
            Self::Unicode(value) => integer_text_native(&value.scalar_string().unwrap_or_default(), self),
            Self::Bytes(value) | Self::ByteArray(value) => integer_text_native(
                if value.is_ascii() { std::str::from_utf8(value).expect("ASCII") } else { "" },
                self,
            ),
            Self::MemoryView(value) => {
                let bytes = value.buffer.buffer_bytes().expect("native buffer");
                integer_text_native(
                    if bytes.is_ascii() { std::str::from_utf8(&bytes).expect("ASCII") } else { "" },
                    self,
                )
            }
            _ => Err(format!(
                "TypeError: int() argument must be a string, a bytes-like object or a real number, not '{}'",
                self.qualified_type_name()
            )),
        }
    }
    pub fn py_float_native(&self) -> Result<f64, String> {
        if let Self::Object(object) = self {
            return object.read(Self::py_float_native);
        }
        if let Self::BigInt(value) = self {
            return value
                .to_f64()
                .filter(|v| v.is_finite())
                .ok_or_else(|| "OverflowError: int too large to convert to float".into());
        }
        if !matches!(
            self,
            Self::Str(_)
                | Self::Unicode(_)
                | Self::Bytes(_)
                | Self::ByteArray(_)
                | Self::MemoryView(_)
                | Self::Int(_)
                | Self::Bool(_)
                | Self::Float(_)
        ) {
            return Err(format!(
                "TypeError: float() argument must be a string or a real number, not '{}'",
                self.qualified_type_name()
            ));
        }
        self.py_float()
            .ok_or_else(|| format!("ValueError: could not convert string to float: {}", crate::python_repr::repr(self)))
    }
    pub fn from_python_string(value: PythonString) -> Self {
        value.scalar_string().map_or_else(|| Self::Unicode(value), Self::Str)
    }
    pub fn python_string(&self) -> Option<PythonString> {
        match self {
            Self::Str(value) => Some(PythonString::from(value.as_str())),
            Self::Unicode(value) => Some(value.clone()),
            Self::Object(value) => value.read(Self::python_string),
            _ => None,
        }
    }
    /// Numeric scalar without any text-to-number conversion. Arithmetic and rounding
    /// accept these kinds; a text value remains text until a consumer explicitly converts it.
    pub fn number(&self) -> Option<f64> {
        match self {
            Self::Bool(value) => Some(f64::from(*value)),
            Self::Int(value) => Some(*value as f64),
            Self::BigInt(value) => value.to_f64(),
            Self::Object(value) => value.read(Self::number),
            Self::Float(value) => Some(*value),
            _ => None,
        }
    }

    /// The value kind's type name, used in error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            LegacyValue::None => "NoneType",
            LegacyValue::Bool(_) => "bool",
            LegacyValue::Int(_) => "int",
            LegacyValue::BigInt(_) => "int",
            LegacyValue::Float(_) => "float",
            LegacyValue::Str(_) => "str",
            LegacyValue::Unicode(_) => "str",
            LegacyValue::Bytes(_) => "bytes",
            LegacyValue::ByteArray(_) => "bytearray",
            LegacyValue::MemoryView(_) => "memoryview",
            LegacyValue::List(_) => "list",
            LegacyValue::Tuple(_) => "tuple",
            LegacyValue::Set(_) => "set",
            LegacyValue::FrozenSet(_) => "frozenset",
            LegacyValue::Dict(_) => "dict",
            LegacyValue::DateTime(_) => "datetime",
            LegacyValue::Global(_) => "type",
            LegacyValue::TimeDelta(_) => "timedelta",
            LegacyValue::Object(value) => value.read(Self::type_name),
        }
    }

    /// Qualified type name used in conversion and arithmetic
    /// error texts; `type_name` remains the unqualified JSON view.
    pub fn qualified_type_name(&self) -> &'static str {
        match self {
            Self::Object(object) => object.read(Self::qualified_type_name),
            Self::DateTime(_) => "datetime.datetime",
            Self::TimeDelta(_) => "datetime.timedelta",
            _ => self.type_name(),
        }
    }

    pub fn as_dict(&self) -> Option<&Dict> {
        match self {
            LegacyValue::Dict(d) => Some(d),
            _ => None,
        }
    }

    pub fn as_dict_mut(&mut self) -> Option<&mut Dict> {
        match self {
            LegacyValue::Dict(d) => Some(d),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            LegacyValue::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Truthiness: false for none, zero and empty text or containers; true otherwise.
    pub fn is_truthy(&self) -> bool {
        match self {
            LegacyValue::None => false,
            LegacyValue::Bool(b) => *b,
            LegacyValue::Int(i) => *i != 0,
            LegacyValue::BigInt(i) => !i.is_zero(),
            LegacyValue::Float(f) => *f != 0.0,
            LegacyValue::Str(s) => !s.is_empty(),
            LegacyValue::Unicode(s) => !s.is_empty(),
            LegacyValue::Bytes(b) | LegacyValue::ByteArray(b) => !b.is_empty(),
            LegacyValue::MemoryView(v) => v.buffer.py_len() != Some(0),
            LegacyValue::List(v) | LegacyValue::Tuple(v) | LegacyValue::Set(v) | LegacyValue::FrozenSet(v) => {
                !v.is_empty()
            }
            LegacyValue::Dict(d) => !d.is_empty(),
            LegacyValue::DateTime(_) | LegacyValue::Global(_) => true,
            LegacyValue::TimeDelta(v) => v.days != 0 || v.seconds != 0 || v.microseconds != 0,
            LegacyValue::Object(value) => value.read(Self::is_truthy),
        }
    }

    /// The length of a sized value.
    pub fn py_len(&self) -> Option<usize> {
        match self {
            LegacyValue::Str(s) => Some(s.chars().count()),
            LegacyValue::Unicode(s) => Some(s.len()),
            LegacyValue::Bytes(b) | LegacyValue::ByteArray(b) => Some(b.len()),
            LegacyValue::MemoryView(v) => v.buffer.py_len(),
            LegacyValue::List(v) | LegacyValue::Tuple(v) | LegacyValue::Set(v) | LegacyValue::FrozenSet(v) => {
                Some(v.len())
            }
            LegacyValue::Dict(d) => Some(d.len()),
            LegacyValue::Object(value) => value.read(Self::py_len),
            _ => None,
        }
    }

    /// Integer conversion for the value kinds the migrations convert (str, int, float, bool).
    pub fn py_int(&self) -> Option<i64> {
        self.py_integer().ok()?.to_i64()
    }

    /// Float conversion for str, int, float and bool.
    pub fn py_float(&self) -> Option<f64> {
        match self {
            LegacyValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            LegacyValue::Int(i) => Some(*i as f64),
            LegacyValue::BigInt(i) => i.to_f64().filter(|value| value.is_finite()),
            LegacyValue::Float(f) => Some(*f),
            LegacyValue::Str(s) => parse_py_float(s),
            LegacyValue::Unicode(s) => s.scalar_string().as_deref().and_then(parse_py_float),
            LegacyValue::Bytes(v) | LegacyValue::ByteArray(v) if v.is_ascii() => {
                parse_py_float(std::str::from_utf8(v).expect("ASCII"))
            }
            LegacyValue::MemoryView(v) => v.buffer.py_float(),
            LegacyValue::Object(value) => value.read(Self::py_float),
            _ => None,
        }
    }

    /// Indexing by a string key, with the required-lookup errors.
    pub fn item(&self, key: &str) -> Result<&LegacyValue, String> {
        match self {
            Self::Dict(values) => values.get(key).ok_or_else(|| crate::python_repr::repr_str(key)),
            Self::Str(_) | Self::Unicode(_) => Err("string indices must be integers, not 'str'".into()),
            Self::List(_) => Err("list indices must be integers or slices, not str".into()),
            Self::Tuple(_) => Err("tuple indices must be integers or slices, not str".into()),
            Self::Bytes(_) => Err("byte indices must be integers or slices, not str".into()),
            Self::ByteArray(_) => Err("bytearray indices must be integers or slices, not str".into()),
            Self::MemoryView(_) => Err("memoryview: invalid slice key".into()),
            value => Err(format!("'{}' object is not subscriptable", value.qualified_type_name())),
        }
    }

    /// Assignment by a string key, with an error message for unsupported parents.
    pub fn assign_item(&mut self, key: &str, value: LegacyValue) -> Result<(), String> {
        match self {
            Self::Object(object) => object.edit(|target| target.assign_item(key, value)),
            Self::Dict(fields) => {
                fields.insert(key, value);
                Ok(())
            }
            Self::List(_) => Err("list indices must be integers or slices, not str".into()),
            Self::ByteArray(_) => Err("bytearray indices must be integers or slices, not str".into()),
            Self::MemoryView(_) => Err("cannot modify read-only memory".into()),
            parent => Err(format!("'{}' object does not support item assignment", parent.type_name())),
        }
    }

    /// The display text of a scalar; `None` for containers.
    pub fn py_str(&self) -> Option<String> {
        match self {
            LegacyValue::None => Some("None".to_owned()),
            LegacyValue::Bool(true) => Some("True".to_owned()),
            LegacyValue::Bool(false) => Some("False".to_owned()),
            LegacyValue::Int(i) => Some(i.to_string()),
            LegacyValue::BigInt(i) => Some(i.to_string()),
            LegacyValue::Float(f) => Some(py_float_repr(*f)),
            LegacyValue::Str(s) => Some(s.clone()),
            LegacyValue::Object(value) => value.read(Self::py_str),
            _ => None,
        }
    }
}

impl fmt::Display for LegacyValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.py_str() {
            Some(s) => f.write_str(&s),
            None => write!(f, "<{}>", self.type_name()),
        }
    }
}

fn integer_text_native(text: &str, original: &LegacyValue) -> Result<BigInt, String> {
    let value = crate::python_numeric::integer_text(text).ok_or_else(|| {
        format!("ValueError: invalid literal for int() with base 10: {}", crate::python_repr::repr(original))
    })?;
    let digits = value.trim_start_matches(['+', '-']).len();
    if digits > crate::python_numeric::DEFAULT_INT_MAX_STR_DIGITS {
        return Err(format!("ValueError: Exceeds the limit (4300 digits) for integer string conversion: value has {digits} digits; use sys.set_int_max_str_digits() to increase the limit"));
    }
    Ok(value.parse().expect("validated decimal integer"))
}

impl serde::Serialize for LegacyValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error, SerializeMap};
        match self {
            Self::None => serializer.serialize_unit(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Int(value) => serializer.serialize_i64(*value),
            Self::BigInt(value) => {
                let encoded = serde_json::value::RawValue::from_string(value.to_string()).map_err(S::Error::custom)?;
                encoded.serialize(serializer)
            }
            Self::Float(value) => serializer.serialize_f64(*value),
            Self::Str(value) => serializer.serialize_str(value),
            Self::Unicode(value) => {
                let encoded =
                    serde_json::value::RawValue::from_string(value.json_escaped()).map_err(S::Error::custom)?;
                encoded.serialize(serializer)
            }
            Self::Object(value) => {
                crate::native_object::serializing(value.id(), || value.resolved().serialize(serializer))
            }
            Self::List(items) | Self::Tuple(items) => items.serialize(serializer),
            Self::Dict(dict) => {
                let mut map = serializer.serialize_map(Some(dict.len()))?;
                for (key, value) in dict.iter() {
                    map.serialize_entry(&key.json_text(), value)?;
                }
                map.end()
            }
            _ => Err(S::Error::custom(format!("{} is not JSON serializable", self.type_name()))),
        }
    }
}

pub use crate::python_numeric::{parse_py_float, parse_py_int, parse_py_int128};

/// Boolean text: `yes/true/t/y/1` and
/// `no/false/f/n/0`, case-insensitive. Anything else is not a boolean.
pub fn str2bool(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "yes" | "true" | "t" | "y" | "1" => Some(true),
        "no" | "false" | "f" | "n" | "0" => Some(false),
        _ => None,
    }
}

/// Float text: shortest round-trip digits, positional for exponents in [-4, 16),
/// scientific (`1e+16`, `1e-05`) otherwise, and always a fractional part when positional.
pub fn py_float_repr(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0.0" } else { "0.0" }.to_owned();
    }
    let sci = format!("{value:e}");
    let (mantissa, exponent) = sci.split_once('e').expect("`{:e}` always has an exponent");
    let exponent: i32 = exponent.parse().expect("`{:e}` exponent is an integer");
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let sign = if negative { "-" } else { "" };
    if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        let body = if point <= 0 {
            format!("0.{}{}", "0".repeat((-point) as usize), digits)
        } else if point as usize >= digits.len() {
            format!("{}{}.0", digits, "0".repeat(point as usize - digits.len()))
        } else {
            let (int_part, frac_part) = digits.split_at(point as usize);
            format!("{int_part}.{frac_part}")
        };
        format!("{sign}{body}")
    } else {
        let (first, rest) = digits.split_at(1);
        let mantissa = if rest.is_empty() { first.to_owned() } else { format!("{first}.{rest}") };
        let exp_sign = if exponent < 0 { '-' } else { '+' };
        format!("{sign}{mantissa}e{exp_sign}{:02}", exponent.abs())
    }
}

impl From<serde_json::Value> for LegacyValue {
    fn from(v: serde_json::Value) -> Self {
        match v {
            serde_json::Value::Null => LegacyValue::None,
            serde_json::Value::Bool(b) => LegacyValue::Bool(b),
            serde_json::Value::Number(n) => match n.as_i64() {
                Some(i) => LegacyValue::Int(i),
                None if n.is_u64() => LegacyValue::from_integer(BigInt::from(n.as_u64().expect("u64"))),
                None => LegacyValue::Float(n.as_f64().unwrap_or(f64::NAN)),
            },
            serde_json::Value::String(s) => LegacyValue::Str(s),
            serde_json::Value::Array(items) => LegacyValue::List(items.into_iter().map(Into::into).collect()),
            serde_json::Value::Object(map) => {
                LegacyValue::Dict(map.into_iter().map(|(k, v)| (Key::Str(k), LegacyValue::from(v))).collect())
            }
        }
    }
}
