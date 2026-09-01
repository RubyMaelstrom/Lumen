//! Embedder host state: typed per-subsystem storage plus an integer-keyed table of open
//! handles, reachable from native functions through their `&mut Interp` argument. This is how
//! a runtime layer (event loop, fs, timers) keeps Rust state despite `NativeFn` being a bare
//! `fn` pointer that cannot capture.
//!
//! Modeled on deno_core's `OpState` + `ResourceTable`.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::rc::Rc;

/// Reports heap storage retained by an embedder-owned value.
///
/// The returned byte count is the requested size of allocations owned below `Self`; it excludes
/// the inline `size_of::<Self>()`, which Lumen adds itself. Implementations should use container
/// capacities rather than lengths and must avoid crediting shared storage from more than one
/// live host entry.
pub trait RetainedBytes {
    fn retained_bytes(&self) -> usize;
}

type RetainedReporter = fn(&dyn Any) -> usize;

fn report_retained<T: Any + RetainedBytes>(value: &dyn Any) -> usize {
    let value = value
        .downcast_ref::<T>()
        .expect("retained-size reporter must match its registered host value type");
    std::mem::size_of::<T>().saturating_add(value.retained_bytes())
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct HostRetainedMemory {
    pub(crate) reported_bytes: usize,
    pub(crate) unavailable_entries: usize,
    pub(crate) opaque_storage: bool,
}

impl HostRetainedMemory {
    pub(crate) fn add(&mut self, other: Self) {
        self.reported_bytes = self.reported_bytes.saturating_add(other.reported_bytes);
        self.unavailable_entries = self
            .unavailable_entries
            .saturating_add(other.unavailable_entries);
        self.opaque_storage |= other.opaque_storage;
    }
}

/// Typed host state: at most one value per Rust type, plus the [`ResourceTable`]. Op crates
/// each keep their state (timer heap, fd table, ...) under their own type.
#[derive(Default)]
pub struct OpState {
    map: HashMap<TypeId, Box<dyn Any>>,
    retained_reporters: HashMap<TypeId, RetainedReporter>,
    pub resources: ResourceTable,
}

impl OpState {
    /// Install (or replace) the `T` slot.
    pub fn put<T: Any>(&mut self, value: T) {
        let type_id = TypeId::of::<T>();
        self.map.insert(type_id, Box::new(value));
        self.retained_reporters.remove(&type_id);
    }
    /// Install a slot whose owned heap storage participates in retained-memory diagnostics.
    pub fn put_retained<T: Any + RetainedBytes>(&mut self, value: T) {
        let type_id = TypeId::of::<T>();
        self.map.insert(type_id, Box::new(value));
        self.retained_reporters
            .insert(type_id, report_retained::<T>);
    }
    pub fn get<T: Any>(&self) -> Option<&T> {
        self.map.get(&TypeId::of::<T>())?.downcast_ref()
    }
    pub fn get_mut<T: Any>(&mut self) -> Option<&mut T> {
        self.map.get_mut(&TypeId::of::<T>())?.downcast_mut()
    }
    pub fn take<T: Any>(&mut self) -> Option<T> {
        let type_id = TypeId::of::<T>();
        let value = self.map.remove(&type_id)?;
        self.retained_reporters.remove(&type_id);
        Some(*value.downcast().ok()?)
    }
    pub fn has<T: Any>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<T>())
    }

    pub(crate) fn retained_memory(&self) -> HostRetainedMemory {
        let mut memory = HostRetainedMemory {
            reported_bytes: self
                .map
                .len()
                .saturating_mul(std::mem::size_of::<(TypeId, Box<dyn Any>)>())
                .saturating_add(
                    self.retained_reporters
                        .len()
                        .saturating_mul(std::mem::size_of::<(TypeId, RetainedReporter)>()),
                ),
            unavailable_entries: self.map.len().saturating_sub(self.retained_reporters.len()),
            opaque_storage: !self.map.is_empty() || !self.retained_reporters.is_empty(),
        };
        for (type_id, reporter) in &self.retained_reporters {
            if let Some(value) = self.map.get(type_id) {
                memory.reported_bytes = memory
                    .reported_bytes
                    .saturating_add(reporter(value.as_ref()));
            }
        }
        memory.add(self.resources.retained_memory());
        memory
    }
}

/// A resource id, the JS-visible handle to an entry in the [`ResourceTable`] (an fd number,
/// in effect).
pub type ResourceId = u32;

/// Open handles (files, sockets, streams): `Rc<dyn Any>` keyed by a small integer that JS code
/// holds. Ids are never reused within a table's lifetime, so a stale id after `close` is a
/// lookup miss, not a use-after-free of a recycled slot.
#[derive(Default)]
pub struct ResourceTable {
    next: ResourceId,
    map: HashMap<ResourceId, Rc<dyn Any>>,
    retained_reporters: HashMap<ResourceId, RetainedReporter>,
}

impl ResourceTable {
    pub fn add<T: Any>(&mut self, resource: T) -> ResourceId {
        let rid = self.next;
        self.next += 1;
        self.map.insert(rid, Rc::new(resource));
        rid
    }
    /// Add a resource whose owned heap storage participates in retained-memory diagnostics.
    pub fn add_retained<T: Any + RetainedBytes>(&mut self, resource: T) -> ResourceId {
        let rid = self.add(resource);
        self.retained_reporters.insert(rid, report_retained::<T>);
        rid
    }
    pub fn get<T: Any>(&self, rid: ResourceId) -> Option<Rc<T>> {
        Rc::downcast(self.map.get(&rid)?.clone()).ok()
    }
    pub fn has(&self, rid: ResourceId) -> bool {
        self.map.contains_key(&rid)
    }
    /// Remove and return the handle; the resource drops (and e.g. the file closes) when the
    /// last `Rc` clone does.
    pub fn close(&mut self, rid: ResourceId) -> Option<Rc<dyn Any>> {
        let value = self.map.remove(&rid);
        self.retained_reporters.remove(&rid);
        value
    }
    /// Live handle count — the event loop stays alive while this is non-zero.
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn retained_memory(&self) -> HostRetainedMemory {
        let mut memory = HostRetainedMemory {
            reported_bytes: self
                .map
                .len()
                .saturating_mul(std::mem::size_of::<(ResourceId, Rc<dyn Any>)>())
                .saturating_add(
                    self.retained_reporters
                        .len()
                        .saturating_mul(std::mem::size_of::<(ResourceId, RetainedReporter)>()),
                ),
            unavailable_entries: self.map.len().saturating_sub(self.retained_reporters.len()),
            opaque_storage: !self.map.is_empty() || !self.retained_reporters.is_empty(),
        };
        for (rid, reporter) in &self.retained_reporters {
            if let Some(value) = self.map.get(rid) {
                memory.reported_bytes = memory
                    .reported_bytes
                    .saturating_add(reporter(value.as_ref()));
            }
        }
        memory
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Reported(Vec<u8>);

    impl RetainedBytes for Reported {
        fn retained_bytes(&self) -> usize {
            self.0.capacity()
        }
    }

    #[test]
    fn retained_reporters_follow_state_and_resource_lifetimes() {
        let mut state = OpState::default();
        state.put_retained(Reported(Vec::with_capacity(41)));
        let rid = state
            .resources
            .add_retained(Reported(Vec::with_capacity(73)));

        let reported = state.retained_memory();
        assert_eq!(reported.unavailable_entries, 0);
        assert!(reported.reported_bytes >= 41 + 73 + 2 * std::mem::size_of::<Reported>());
        assert!(reported.opaque_storage);

        state.put(String::from("not reported"));
        assert_eq!(state.retained_memory().unavailable_entries, 1);
        state.take::<String>();
        state.resources.close(rid);
        assert_eq!(state.retained_memory().unavailable_entries, 0);
    }

    #[test]
    fn replacing_reported_state_with_legacy_state_removes_the_reporter() {
        let mut state = OpState::default();
        state.put_retained(Reported(Vec::with_capacity(19)));
        assert_eq!(state.retained_memory().unavailable_entries, 0);

        state.put(Reported(Vec::new()));
        assert_eq!(state.retained_memory().unavailable_entries, 1);
    }
}
