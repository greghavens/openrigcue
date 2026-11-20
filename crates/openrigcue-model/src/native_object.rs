//! Identity-preserving shared objects. Arena edges are unrooted handles; external
//! handles are roots. A mark/sweep on root release collects recursive containers
//! without unsafe references or Arc cycles retaining their payloads forever.
use super::native_value::{Dict, LegacyValue};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};

thread_local! {static OPERATION_DEPTH:std::cell::Cell<usize>=const{std::cell::Cell::new(0)};}
/// Reentrant same-thread boundary for builtin operations over shared values.
/// It is one process-wide lock serializing concrete builtin operations;
/// never hold it across service callbacks, blocking I/O or a user callable.
pub struct NativeOperationGuard {
    lock: Option<std::sync::MutexGuard<'static, ()>>,
}
/// Keep multi-object snapshots, builtin arithmetic and recursive comparisons
/// atomic relative to shared object mutation. Nested same-thread calls are safe.
pub fn native_operation() -> NativeOperationGuard {
    static LOCK: Mutex<()> = Mutex::new(());
    let lock = if OPERATION_DEPTH.with(|d| d.get() == 0) {
        Some(LOCK.lock().unwrap_or_else(|e| e.into_inner()))
    } else {
        None
    };
    OPERATION_DEPTH.with(|d| d.set(d.get() + 1));
    NativeOperationGuard { lock }
}
impl Drop for NativeOperationGuard {
    fn drop(&mut self) {
        OPERATION_DEPTH.with(|d| d.set(d.get() - 1));
        drop(self.lock.take());
    }
}

type Cell = Arc<Mutex<LegacyValue>>;
#[derive(Default)]
struct State {
    next: usize,
    revision: u64,
    gc_debt: usize,
    nodes: BTreeMap<usize, Cell>,
    roots: BTreeMap<usize, usize>,
    #[cfg(test)]
    collections: usize,
}
#[derive(Default)]
struct Store(Mutex<State>);
fn store() -> Arc<Store> {
    static STORE: OnceLock<Arc<Store>> = OnceLock::new();
    STORE.get_or_init(|| Arc::new(Store::default())).clone()
}

pub struct NativeObject {
    store: Arc<Store>,
    id: usize,
    rooted: bool,
}
impl std::fmt::Debug for NativeObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeObject").field("id", &self.id).finish()
    }
}
impl PartialEq for NativeObject {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && Arc::ptr_eq(&self.store, &other.store)
    }
}
impl Eq for NativeObject {}
impl std::hash::Hash for NativeObject {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&self.id, state);
    }
}
impl PartialOrd for NativeObject {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for NativeObject {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}
impl Clone for NativeObject {
    fn clone(&self) -> Self {
        let _operation = native_operation();
        if self.rooted {
            self.store.root(self.id);
        }
        Self { store: self.store.clone(), id: self.id, rooted: self.rooted }
    }
}
impl Drop for NativeObject {
    fn drop(&mut self) {
        let _operation = native_operation();
        if self.rooted {
            self.store.unroot(self.id);
            self.store.collect();
        }
    }
}
thread_local! {static EDIT_DEPTH:std::cell::Cell<usize>=const{std::cell::Cell::new(0)};}

impl Store {
    fn root(&self, id: usize) {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *s.roots.entry(id).or_default() += 1;
        s.revision = s.revision.wrapping_add(1);
    }
    fn unroot(&self, id: usize) {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = s.roots.get_mut(&id) {
            *n -= 1;
            if *n == 0 {
                s.roots.remove(&id);
            }
        }
        s.revision = s.revision.wrapping_add(1);
    }
    fn cell(&self, id: usize) -> Cell {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).nodes.get(&id).expect("rooted object exists").clone()
    }
    fn collect(&self) {
        if EDIT_DEPTH.with(|depth| depth.get() != 0) {
            return;
        }
        let (revision, nodes, mut pending) = {
            let s = self.0.lock().unwrap_or_else(|e| e.into_inner());
            // A full-heap mark must be amortized against the retained heap,
            // not triggered after a fixed tiny number of new row objects.
            // Otherwise loading N profiles beside a large shipped root graph
            // repeatedly scans that graph and becomes quadratic.
            if s.gc_debt < (s.nodes.len() / 4).max(256) && !s.roots.is_empty() {
                return;
            }
            (s.revision, s.nodes.clone(), s.roots.keys().copied().collect::<Vec<_>>())
        };
        let mut seen = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            if let Some(cell) = nodes.get(&id) {
                references(&cell.lock().unwrap_or_else(|e| e.into_inner()), &mut pending);
            }
        }
        let removed = {
            let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if s.revision != revision {
                return;
            }
            let dead = s.nodes.keys().copied().filter(|id| !seen.contains(id)).collect::<Vec<_>>();
            #[cfg(test)]
            {
                s.collections += 1;
            }
            s.gc_debt = 0;
            dead.into_iter().filter_map(|id| s.nodes.remove(&id)).collect::<Vec<_>>()
        };
        drop(removed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_graph_reads_do_not_trigger_full_heap_marks() {
        let _operation = native_operation();
        let root = NativeObject::new(LegacyValue::Dict(
            (0..2000)
                .map(|index| (format!("field{index}").into(), LegacyValue::List(vec![LegacyValue::Int(index)])))
                .collect(),
        ));
        let state = root.store.clone();
        let before = state.0.lock().unwrap().collections;
        for _ in 0..4000 {
            assert_eq!(root.item_owned("field1999").unwrap().get_index(0).unwrap().integer(), Some(1999.into()));
        }
        assert_eq!(state.0.lock().unwrap().collections, before, "a read creates no native object allocation debt");
    }
    #[test]
    fn released_recursive_objects_are_collectable_without_arc_cycles() {
        let _operation = native_operation();
        let object = NativeObject::new(LegacyValue::List(Vec::new()));
        let id = object.id();
        let state = object.store.clone();
        let own = object.clone();
        object.edit(|value| {
            if let LegacyValue::List(values) = value {
                values.push(LegacyValue::Object(own));
            }
        });
        drop(object);
        state.0.lock().unwrap().gc_debt = usize::MAX;
        state.collect();
        assert!(!state.0.lock().unwrap().nodes.contains_key(&id));
    }
}

thread_local! {static SERIALIZING:std::cell::RefCell<BTreeSet<usize>>=const{std::cell::RefCell::new(BTreeSet::new())};}
pub(crate) fn serializing<T, E: serde::ser::Error>(id: usize, run: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    if !SERIALIZING.with(|stack| stack.borrow_mut().insert(id)) {
        return Err(E::custom("Circular reference detected"));
    }
    let result = run();
    SERIALIZING.with(|stack| {
        stack.borrow_mut().remove(&id);
    });
    result
}
fn children_mut(value: &mut LegacyValue, mut change: impl FnMut(&mut LegacyValue)) {
    match value {
        LegacyValue::List(v) | LegacyValue::Tuple(v) | LegacyValue::Set(v) | LegacyValue::FrozenSet(v) => {
            for v in v {
                change(v);
            }
        }
        LegacyValue::Dict(v) => {
            for key in v.native_keys_mut() {
                change(key);
            }
            for (_, v) in v.iter_mut() {
                change(v);
            }
        }
        _ => {}
    }
}
fn internalize(value: &mut LegacyValue) {
    if let LegacyValue::Object(object) = value {
        if object.rooted {
            object.store.unroot(object.id);
            object.rooted = false;
        }
    } else {
        children_mut(value, |child| {
            if matches!(
                child,
                LegacyValue::List(_)
                    | LegacyValue::Tuple(_)
                    | LegacyValue::Set(_)
                    | LegacyValue::FrozenSet(_)
                    | LegacyValue::Dict(_)
                    | LegacyValue::ByteArray(_)
            ) || matches!(child,LegacyValue::Float(v)if v.is_nan())
            {
                *child = LegacyValue::Object(NativeObject::new(std::mem::replace(child, LegacyValue::None)));
            }
            internalize(child);
        });
    }
}
fn externalize(value: &mut LegacyValue) {
    if let LegacyValue::Object(object) = value {
        if !object.rooted {
            object.store.root(object.id);
            object.rooted = true;
        }
    } else {
        children_mut(value, externalize);
    }
}
fn references(value: &LegacyValue, out: &mut Vec<usize>) {
    match value {
        LegacyValue::Object(object) => out.push(object.id),
        LegacyValue::List(v) | LegacyValue::Tuple(v) | LegacyValue::Set(v) | LegacyValue::FrozenSet(v) => {
            for v in v {
                references(v, out);
            }
        }
        LegacyValue::Dict(v) => {
            for key in v.native_keys() {
                references(key, out);
            }
            for (_, v) in v.iter() {
                references(v, out);
            }
        }
        _ => {}
    }
}
impl NativeObject {
    pub fn new(value: LegacyValue) -> Self {
        let _operation = native_operation();
        let store = store();
        // Like cyclic GC's allocation counter, the collection budget tracks
        // allocated nodes, not temporary references to a retained graph.
        // Reading a live settings tree must not retrace the entire profile DB
        // every few hundred alias clones/drops.
        let id = {
            let mut s = store.0.lock().unwrap_or_else(|e| e.into_inner());
            let id = s.next;
            s.next += 1;
            s.gc_debt = s.gc_debt.saturating_add(1);
            s.nodes.insert(id, Arc::new(Mutex::new(LegacyValue::None)));
            s.roots.insert(id, 1);
            s.revision = s.revision.wrapping_add(1);
            id
        };
        let object = Self { store, id, rooted: true };
        object.replace(value);
        object
    }
    pub fn id(&self) -> usize {
        self.id
    }
    /// Borrow one builtin payload for scalar inspection. Keep this internal:
    /// a callback must not edit or recursively resolve this same cell.
    pub(crate) fn read<R>(&self, read: impl FnOnce(&LegacyValue) -> R) -> R {
        let _operation = native_operation();
        let cell = self.store.cell(self.id);
        let value = cell.lock().unwrap_or_else(|e| e.into_inner());
        read(&value)
    }

    /// Required dictionary subscription clones only the selected value. A
    /// field lookup is not a shallow copy of the entire source dictionary.
    pub fn item_owned(&self, key: &str) -> Result<LegacyValue, String> {
        let _operation = native_operation();
        let mut selected = self.read(|value| value.item(key).cloned())?;
        externalize(&mut selected);
        Ok(selected)
    }

    /// Snapshot only insertion-ordered mapping keys. Live migration loops use
    /// these keys and then fetch each selected value at its own statement.
    pub fn dict_keys_owned(&self) -> Result<Vec<super::native_value::Key>, String> {
        let _operation = native_operation();
        let mut keys = self.read(|value| match value {
            LegacyValue::Dict(dict) => Ok(dict.iter_native().map(|(key, _)| key).collect::<Vec<_>>()),
            value => Err(format!("AttributeError: '{}' object has no attribute 'keys'", value.qualified_type_name())),
        })?;
        for key in &mut keys {
            externalize(key);
        }
        keys.into_iter()
            .map(|key| {
                if let LegacyValue::Object(object) = key {
                    Ok(super::native_value::Key::Extra(super::native_value::NativeKey::Object(object)))
                } else {
                    super::native_value::Key::from_value(&key)
                }
            })
            .collect()
    }

    pub fn lookup_owned(&self, key: &LegacyValue) -> Result<LegacyValue, String> {
        let _operation = native_operation();
        let mut selected = if self.read(LegacyValue::type_name) == "dict" {
            let key = super::native_value::Key::from_value(key).map_err(|detail| format!("TypeError: {detail}"))?;
            self.read(|value| {
                let LegacyValue::Dict(values) = value else { unreachable!("native operation holds type stable") };
                values.get_key(&key).cloned()
            })
            .ok_or_else(|| format!("KeyError: {}", crate::python_repr::repr(&key.to_value())))?
        } else {
            let key = key.resolved();
            self.read(|value| value.lookup_owned(&key))?
        };
        externalize(&mut selected);
        Ok(selected)
    }

    /// Nonnegative sequence read, retaining just the chosen child's
    /// root. Conversion/error callers can still use full subscription.
    pub fn get_index(&self, index: usize) -> Result<LegacyValue, String> {
        let _operation = native_operation();
        let mut selected = self.read(|value| value.get_index(index))?;
        externalize(&mut selected);
        Ok(selected)
    }
    pub fn resolved(&self) -> LegacyValue {
        let _operation = native_operation();
        let mut value = self.store.cell(self.id).lock().unwrap_or_else(|e| e.into_inner()).clone();
        externalize(&mut value);
        value
    }
    pub fn replace(&self, value: LegacyValue) {
        self.edit(|target| *target = value);
    }
    pub fn edit<R>(&self, change: impl FnOnce(&mut LegacyValue) -> R) -> R {
        let _operation = native_operation();
        EDIT_DEPTH.with(|depth| depth.set(depth.get() + 1));
        let cell = self.store.cell(self.id);
        let result = {
            let mut value = cell.lock().unwrap_or_else(|e| e.into_inner());
            externalize(&mut value);
            let result = change(&mut value);
            internalize(&mut value);
            result
        };
        {
            let mut s = self.store.0.lock().unwrap_or_else(|e| e.into_inner());
            s.revision = s.revision.wrapping_add(1);
        }
        EDIT_DEPTH.with(|depth| depth.set(depth.get() - 1));
        self.store.collect();
        result
    }
    /// Strip memo-only wrappers while retaining every repeated or recursive
    /// object. Ordinary tree profiles therefore keep their existing borrowed
    /// dictionary representation; actual aliases stay live object handles.
    pub fn materialize(value: LegacyValue) -> LegacyValue {
        fn count(v: &LegacyValue, counts: &mut BTreeMap<usize, usize>, walked: &mut BTreeSet<usize>) {
            if let LegacyValue::Object(o) = v {
                *counts.entry(o.id).or_default() += 1;
                if !walked.insert(o.id) {
                    return;
                }
                count(&o.resolved(), counts, walked);
            } else {
                match v {
                    LegacyValue::List(v) | LegacyValue::Tuple(v) | LegacyValue::Set(v) | LegacyValue::FrozenSet(v) => {
                        for v in v {
                            count(v, counts, walked)
                        }
                    }
                    LegacyValue::Dict(d) => {
                        for (k, v) in d.iter_native() {
                            count(&k, counts, walked);
                            count(v, counts, walked)
                        }
                    }
                    LegacyValue::MemoryView(v) => count(&v.buffer, counts, walked),
                    _ => {}
                }
            }
        }
        fn flatten(v: LegacyValue, counts: &BTreeMap<usize, usize>) -> LegacyValue {
            match v {
                LegacyValue::Object(ref o)
                    if counts.get(&o.id) == Some(&1) && !matches!(o.resolved(),LegacyValue::Float(v)if v.is_nan()) =>
                {
                    flatten(o.resolved(), counts)
                }
                LegacyValue::List(v) => LegacyValue::List(v.into_iter().map(|v| flatten(v, counts)).collect()),
                LegacyValue::Tuple(v) => LegacyValue::Tuple(v.into_iter().map(|v| flatten(v, counts)).collect()),
                LegacyValue::Set(v) => LegacyValue::Set(v.into_iter().map(|v| flatten(v, counts)).collect()),
                LegacyValue::FrozenSet(v) => {
                    LegacyValue::FrozenSet(v.into_iter().map(|v| flatten(v, counts)).collect())
                }
                LegacyValue::Dict(d) => {
                    let mut out = Dict::new();
                    for (k, v) in d.iter_native() {
                        out.insert_native(flatten(k, counts), flatten(v.clone(), counts))
                            .expect("existing hashable key");
                    }
                    LegacyValue::Dict(out)
                }
                v => v,
            }
        }
        let mut counts = BTreeMap::new();
        count(&value, &mut counts, &mut BTreeSet::new());
        flatten(value, &counts)
    }
}
