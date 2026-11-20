//! Dictionary lookups and assignments by path, without long object locks.
//! Nested views retain a path, not a captured child: every statement re-reads
//! its receiver, including when an earlier aliased write rebound that path.
use super::super::value::{Dict, Key, LegacyValue, NativeKey, NativeObject};

enum Root<'a> {
    Borrowed(&'a mut Dict),
    Shared(NativeObject),
}
pub(crate) struct DictView<'a> {
    root: Root<'a>,
    path: Vec<Key>,
}
fn missing(key: &Key) -> String {
    format!("KeyError: {}", openrigcue_model::python_repr::repr(&key.to_value()))
}
fn keys(dict: &Dict) -> Vec<Key> {
    dict.iter_native()
        .map(|(key, _)| match key {
            LegacyValue::Object(o) => Key::Extra(NativeKey::Object(o)),
            key => Key::from_value(&key).expect("existing hashable key"),
        })
        .collect()
}
fn read_dict(dict: &Dict, path: &[Key], field: &Key) -> Result<LegacyValue, String> {
    if let Some((key, tail)) = path.split_first() {
        read_value(dict.get_key(key).ok_or_else(|| missing(key))?, tail, field)
    } else {
        dict.get_key(field).cloned().ok_or_else(|| missing(field))
    }
}
fn read_value(value: &LegacyValue, path: &[Key], field: &Key) -> Result<LegacyValue, String> {
    if let LegacyValue::Dict(dict) = value {
        return read_dict(dict, path, field);
    }
    if let Some((key, tail)) = path.split_first() {
        read_value(&value.lookup_owned(&key.to_value())?, tail, field)
    } else {
        value.lookup_owned(&field.to_value())
    }
}
fn write_dict(dict: &mut Dict, path: &[Key], field: Key, value: LegacyValue) -> Result<(), String> {
    if let Some((key, tail)) = path.split_first() {
        write_value(dict.get_key_mut(key).ok_or_else(|| missing(key))?, tail, field, value)
    } else {
        dict.insert(field, value);
        Ok(())
    }
}
fn write_value(target: &mut LegacyValue, path: &[Key], field: Key, value: LegacyValue) -> Result<(), String> {
    if let LegacyValue::Dict(dict) = target {
        return write_dict(dict, path, field, value);
    }
    if let Some((key, tail)) = path.split_first() {
        write_value(&mut target.lookup_owned(&key.to_value())?, tail, field, value)
    } else {
        target.assign_key(field.to_value(), value)
    }
}
fn delete_dict(dict: &mut Dict, path: &[Key], field: &Key) -> Result<Option<LegacyValue>, String> {
    if let Some((key, tail)) = path.split_first() {
        delete_value(dict.get_key_mut(key).ok_or_else(|| missing(key))?, tail, field)
    } else {
        Ok(dict.remove_key(field))
    }
}
fn delete_value(target: &mut LegacyValue, path: &[Key], field: &Key) -> Result<Option<LegacyValue>, String> {
    if let LegacyValue::Dict(dict) = target {
        return delete_dict(dict, path, field);
    }
    if let Some((key, tail)) = path.split_first() {
        return delete_value(&mut target.lookup_owned(&key.to_value())?, tail, field);
    }
    match target.remove_key(&field.to_value()) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.starts_with("KeyError:") => Ok(None),
        Err(e) => Err(e),
    }
}
fn keys_dict(dict: &Dict, path: &[Key]) -> Result<Vec<Key>, String> {
    if let Some((key, tail)) = path.split_first() {
        keys_value(dict.get_key(key).ok_or_else(|| missing(key))?, tail)
    } else {
        Ok(keys(dict))
    }
}
fn keys_value(value: &LegacyValue, path: &[Key]) -> Result<Vec<Key>, String> {
    if let LegacyValue::Dict(dict) = value {
        return keys_dict(dict, path);
    }
    if let Some((key, tail)) = path.split_first() {
        keys_value(&value.lookup_owned(&key.to_value())?, tail)
    } else {
        value.dict_keys_owned()
    }
}
fn object_dict(dict: &Dict, path: &[Key]) -> Result<Option<NativeObject>, String> {
    if let Some((key, tail)) = path.split_first() {
        object_value(dict.get_key(key).ok_or_else(|| missing(key))?, tail)
    } else {
        Ok(None)
    }
}
fn object_value(value: &LegacyValue, path: &[Key]) -> Result<Option<NativeObject>, String> {
    if let LegacyValue::Dict(dict) = value {
        return object_dict(dict, path);
    }
    if let Some((key, tail)) = path.split_first() {
        object_value(&value.lookup_owned(&key.to_value())?, tail)
    } else {
        Ok(match value {
            LegacyValue::Object(o) => Some(o.clone()),
            _ => None,
        })
    }
}
pub(crate) fn as_dict<'a>(value: &'a mut LegacyValue, what: &str) -> Result<DictView<'a>, String> {
    let root = match value {
        LegacyValue::Dict(dict) => Root::Borrowed(dict),
        LegacyValue::Object(o) if LegacyValue::Object(o.clone()).type_name() == "dict" => Root::Shared(o.clone()),
        _ => return Err(format!("{what} is not a dict")),
    };
    Ok(DictView { root, path: Vec::new() })
}
impl DictView<'_> {
    pub fn get_result(&self, key: &str) -> Result<LegacyValue, String> {
        self.lookup_result(&Key::from(key))
    }
    pub fn get(&self, key: &str) -> Option<LegacyValue> {
        self.get_result(key).ok()
    }
    pub fn lookup_result(&self, key: &Key) -> Result<LegacyValue, String> {
        match &self.root {
            Root::Borrowed(dict) => read_dict(dict, &self.path, key),
            Root::Shared(o) => read_value(&LegacyValue::Object(o.clone()), &self.path, key),
        }
    }
    pub fn lookup(&self, key: &Key) -> Option<LegacyValue> {
        self.lookup_result(key).ok()
    }
    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }
    pub fn insert(&mut self, key: impl Into<Key>, value: LegacyValue) -> Result<(), String> {
        let key = key.into();
        match &mut self.root {
            Root::Borrowed(dict) => write_dict(dict, &self.path, key, value),
            Root::Shared(o) => write_value(&mut LegacyValue::Object(o.clone()), &self.path, key, value),
        }
    }
    pub fn remove(&mut self, key: &str) -> Result<Option<LegacyValue>, String> {
        self.remove_key(&Key::from(key))
    }
    pub fn remove_key(&mut self, key: &Key) -> Result<Option<LegacyValue>, String> {
        match &mut self.root {
            Root::Borrowed(dict) => delete_dict(dict, &self.path, key),
            Root::Shared(o) => delete_value(&mut LegacyValue::Object(o.clone()), &self.path, key),
        }
    }
    pub fn set_default(&mut self, key: &str, value: LegacyValue) -> Result<(), String> {
        let _operation = openrigcue_model::native_object::native_operation();
        if !self.contains(key) {
            self.insert(key, value)?;
        }
        Ok(())
    }
    pub fn keys(&self) -> Result<Vec<Key>, String> {
        match &self.root {
            Root::Borrowed(dict) => keys_dict(dict, &self.path),
            Root::Shared(o) => keys_value(&LegacyValue::Object(o.clone()), &self.path),
        }
    }

    pub fn nested(&mut self, key: &str) -> Result<DictView<'_>, String> {
        self.child(&Key::from(key))
    }
    pub fn child(&mut self, key: &Key) -> Result<DictView<'_>, String> {
        let root = match &mut self.root {
            Root::Borrowed(dict) => Root::Borrowed(dict),
            Root::Shared(o) => Root::Shared(o.clone()),
        };
        let mut path = self.path.clone();
        path.push(key.clone());
        let view = DictView { root, path };
        Ok(view)
    }
    pub fn cursor(&self) -> Result<DictCursor, String> {
        let _operation = openrigcue_model::native_object::native_operation();
        let receiver = match &self.root {
            Root::Borrowed(dict) => object_dict(dict, &self.path)?,
            Root::Shared(o) => object_value(&LegacyValue::Object(o.clone()), &self.path)?,
        };
        let length = self.keys()?.len();
        Ok(DictCursor { receiver, seen: Vec::new(), length, remaining: length })
    }
}
/// Item/key iteration captures its dictionary once. Each step reads live entries
/// and fails before advancing if a child mutation changed that dictionary.
pub(crate) struct DictCursor {
    receiver: Option<NativeObject>,
    seen: Vec<Key>,
    length: usize,
    remaining: usize,
}
impl DictCursor {
    pub fn next(&mut self, view: &DictView<'_>) -> Result<Option<Key>, String> {
        let _operation = openrigcue_model::native_object::native_operation();
        let keys = if let Some(o) = &self.receiver { o.dict_keys_owned()? } else { view.keys()? };
        if keys.len() != self.length {
            return Err("RuntimeError: dictionary changed size during iteration".into());
        }
        if let Some(key) = keys.into_iter().find(|key| !self.seen.contains(key)) {
            if self.remaining == 0 {
                return Err("RuntimeError: dictionary keys changed during iteration".into());
            }
            self.remaining -= 1;
            self.seen.push(key.clone());
            return Ok(Some(key));
        }
        Ok(None)
    }
}
pub(crate) fn at_path<'a>(value: &'a mut LegacyValue, path: &[&str]) -> Result<DictView<'a>, String> {
    let mut view = as_dict(value, "mapping")?;
    view.path = path.iter().map(|k| Key::from(*k)).collect();
    Ok(view)
}
