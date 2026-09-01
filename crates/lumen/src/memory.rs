//! Opt-in, post-collection managed-memory accounting.
//!
//! This deliberately reports requested payload/capacity bytes rather than guessing allocator
//! rounding or Rust's private `RcBox`/`HashMap` layouts. Every partial category says so in the
//! emitted record; unavailable owners are never represented by a misleading zero.

use std::cell::RefCell;
use std::collections::HashSet;
use std::mem::size_of;
use std::rc::Rc;
use std::sync::Mutex;

use crate::interpreter::{Env, Interp, Scope};
use crate::value::{Callable, Exotic, Gc, Object, SymbolData, Value};

#[derive(Clone, Copy)]
enum Quality {
    Exact,
    LowerBound,
}

impl Quality {
    fn json(self) -> &'static str {
        match self {
            Quality::Exact => "exact",
            Quality::LowerBound => "lower_bound",
        }
    }
}

#[derive(Clone, Copy)]
struct Category {
    bytes: usize,
    quality: Quality,
}

impl Category {
    fn exact(bytes: usize) -> Self {
        Self {
            bytes,
            quality: Quality::Exact,
        }
    }

    fn lower_bound(bytes: usize) -> Self {
        Self {
            bytes,
            quality: Quality::LowerBound,
        }
    }

    fn add(&mut self, bytes: usize) {
        self.bytes = self.bytes.saturating_add(bytes);
    }

    fn make_lower_bound(&mut self) {
        self.quality = Quality::LowerBound;
    }

    fn json(self) -> String {
        format!(
            "{{\"bytes\":{},\"quality\":\"{}\"}}",
            self.bytes,
            self.quality.json()
        )
    }
}

#[derive(Clone)]
struct Snapshot {
    object_bodies: Category,
    property_storage: Category,
    scope_bodies: Category,
    scope_storage: Category,
    strings_symbols_bigints: Category,
    callable_metadata: Category,
    array_buffer_backing: Category,
}

impl Snapshot {
    fn managed_requested_bytes(&self) -> usize {
        [
            self.object_bodies,
            self.property_storage,
            self.scope_bodies,
            self.scope_storage,
            self.strings_symbols_bigints,
            self.callable_metadata,
        ]
        .into_iter()
        .map(|category| category.bytes)
        .fold(0usize, usize::saturating_add)
    }

    fn json(&self) -> String {
        format!(
            concat!(
                "{{\"schema_version\":1,\"safepoint\":\"post_gc\",\"complete\":false,",
                "\"managed_requested_bytes\":{{\"bytes\":{},\"quality\":\"lower_bound\"}},",
                "\"managed_external_bytes\":{{\"bytes\":{},\"quality\":\"lower_bound\"}},",
                "\"categories\":{{",
                "\"object_bodies\":{},\"property_storage\":{},",
                "\"scope_bodies\":{},\"scope_storage\":{},",
                "\"strings_symbols_bigints\":{},\"callable_metadata\":{},",
                "\"array_buffer_backing\":{},",
                "\"function_ast_bytecode_feedback\":{{\"bytes\":null,\"quality\":\"unavailable\"}},",
                "\"interpreter_side_tables\":{{\"bytes\":null,\"quality\":\"unavailable\"}},",
                "\"engine_caches\":{{\"bytes\":null,\"quality\":\"unavailable\"}},",
                "\"shared_wasm_backing\":{{\"bytes\":null,\"quality\":\"unavailable\"}},",
                "\"host_resources\":{{\"bytes\":null,\"quality\":\"unavailable\"}}",
                "}}}}"
            ),
            self.managed_requested_bytes(),
            self.array_buffer_backing.bytes,
            self.object_bodies.json(),
            self.property_storage.json(),
            self.scope_bodies.json(),
            self.scope_storage.json(),
            self.strings_symbols_bigints.json(),
            self.callable_metadata.json(),
            self.array_buffer_backing.json(),
        )
    }
}

#[derive(Default)]
struct Visitor {
    lstrs: HashSet<usize>,
    rc_strs: HashSet<usize>,
    symbols: HashSet<usize>,
    bigints: HashSet<usize>,
    callable_allocations: HashSet<usize>,
    strings_symbols_bigints: usize,
    callable_metadata: usize,
}

impl Visitor {
    fn rc_str(&mut self, value: &Rc<str>) {
        let identity = Rc::as_ptr(value) as *const () as usize;
        if self.rc_strs.insert(identity) {
            // `RcBox` counters are a private standard-library layout and are excluded.
            self.strings_symbols_bigints = self.strings_symbols_bigints.saturating_add(value.len());
        }
    }

    fn symbol(&mut self, value: &Rc<SymbolData>) {
        let identity = Rc::as_ptr(value) as usize;
        if self.symbols.insert(identity) {
            self.strings_symbols_bigints = self
                .strings_symbols_bigints
                .saturating_add(size_of::<SymbolData>());
            if let Some(description) = &value.description {
                self.rc_str(description);
            }
        }
    }

    fn value(&mut self, value: &Value) {
        match value {
            Value::BigInt(value) => {
                if self.bigints.insert(value.allocation_identity()) {
                    self.strings_symbols_bigints = self
                        .strings_symbols_bigints
                        .saturating_add(value.retained_requested_bytes());
                }
            }
            Value::Str(value) => {
                let identity = value.as_ptr() as usize;
                if self.lstrs.insert(identity) {
                    self.strings_symbols_bigints = self
                        .strings_symbols_bigints
                        .saturating_add(value.retained_requested_bytes());
                }
            }
            Value::Sym(value) => self.symbol(value),
            Value::Undefined
            | Value::Empty
            | Value::Null
            | Value::Bool(_)
            | Value::Num(_)
            | Value::Obj(_) => {}
        }
    }

    fn callable(&mut self, callable: &Callable) {
        match callable {
            Callable::None | Callable::Native(_) => {}
            Callable::NativeData(value) => {
                let identity = Rc::as_ptr(value) as usize;
                if self.callable_allocations.insert(identity) {
                    self.callable_metadata = self
                        .callable_metadata
                        .saturating_add(size_of_val(value.as_ref()));
                }
            }
            Callable::User(value) => {
                let identity = Rc::as_ptr(value) as usize;
                if self.callable_allocations.insert(identity) {
                    self.callable_metadata = self
                        .callable_metadata
                        .saturating_add(size_of_val(value.as_ref()));
                }
            }
            Callable::Bound(value) => {
                self.callable_metadata = self
                    .callable_metadata
                    .saturating_add(size_of_val(value.as_ref()))
                    .saturating_add(value.args.capacity().saturating_mul(size_of::<Value>()));
                self.value(&value.this);
                for argument in &value.args {
                    self.value(argument);
                }
            }
            Callable::WrappedShadow(value) => {
                let identity = Rc::as_ptr(value) as usize;
                if self.callable_allocations.insert(identity) {
                    self.callable_metadata = self
                        .callable_metadata
                        .saturating_add(size_of_val(value.as_ref()))
                        .saturating_add(size_of::<Value>());
                    self.value(&value.target);
                }
            }
            Callable::WrappedCross(value) => {
                self.callable_metadata = self
                    .callable_metadata
                    .saturating_add(size_of_val(value.as_ref()))
                    .saturating_add(size_of::<Value>());
                self.value(&value.target);
            }
            Callable::AccessorGet(name)
            | Callable::AccessorSet(name)
            | Callable::PropGet(name)
            | Callable::PropSet(name) => {
                let outer = Rc::as_ptr(name) as usize;
                if self.callable_allocations.insert(outer) {
                    self.callable_metadata =
                        self.callable_metadata.saturating_add(size_of::<Rc<str>>());
                }
                self.rc_str(name.as_ref());
            }
        }
    }

    fn object(&mut self, object: &Object) -> (usize, bool) {
        for (name, property) in object.props.iter() {
            self.rc_str(name);
            let value = property.value();
            self.value(&value);
            if let Some(getter) = property.getter() {
                self.value(getter);
            }
            if let Some(setter) = property.setter() {
                self.value(setter);
            }
        }
        // Packed array elements have no key entry and must be visited separately.
        for property in object.props.values() {
            let value = property.value();
            self.value(&value);
            if let Some(getter) = property.getter() {
                self.value(getter);
            }
            if let Some(setter) = property.setter() {
                self.value(setter);
            }
        }
        self.callable(&object.call);
        match &object.exotic {
            Exotic::StrWrap(value) => self.value(&Value::Str((**value).clone())),
            Exotic::SymWrap(value) => self.symbol(value),
            Exotic::BigIntWrap(value) => self.value(&Value::BigInt((**value).clone())),
            Exotic::Error(value) => self.rc_str(value),
            Exotic::None
            | Exotic::Array
            | Exotic::BoolWrap(_)
            | Exotic::NumWrap(_)
            | Exotic::Arguments => {}
        }
        object.props.retained_requested_storage_bytes()
    }

    fn scope(&mut self, scope: &Scope) -> (usize, bool) {
        let (mut bytes, exact) = scope.vars.retained_requested_storage_bytes();
        bytes = bytes.saturating_add(scope.lexical_names.retained_requested_storage_bytes());
        for (name, binding) in scope.vars.iter() {
            self.rc_str(name);
            self.value(&binding.value);
            if let Some((_, import_name)) = &binding.import_ref {
                bytes = bytes.saturating_add(import_name.capacity());
            }
        }
        (bytes, exact)
    }
}

static LAST_SNAPSHOT: Mutex<Option<Snapshot>> = Mutex::new(None);

fn unique_array_buffer_capacity<'a>(
    buffers: impl Iterator<Item = &'a crate::interpreter::ArrayBufferBytes>,
) -> usize {
    let mut seen = HashSet::new();
    buffers.fold(0usize, |bytes, buffer| {
        let identity = Rc::as_ptr(buffer) as usize;
        if seen.insert(identity) {
            bytes.saturating_add(buffer.borrow().capacity())
        } else {
            bytes
        }
    })
}

pub(crate) fn record_post_gc(interp: &Interp, objects: &[Gc], scopes: &[Env]) {
    let mut visitor = Visitor::default();
    let mut property_storage = Category::exact(0);
    for object in objects {
        let (bytes, exact) = visitor.object(&object.borrow());
        property_storage.add(bytes);
        if !exact {
            property_storage.make_lower_bound();
        }
    }

    let mut scope_storage = Category::exact(0);
    for scope in scopes {
        let (bytes, exact) = visitor.scope(&scope.borrow());
        scope_storage.add(bytes);
        if !exact {
            scope_storage.make_lower_bound();
        }
    }

    let array_buffer_bytes = unique_array_buffer_capacity(interp.array_buffers.values());

    let snapshot = Snapshot {
        object_bodies: Category::exact(objects.len().saturating_mul(size_of::<RefCell<Object>>())),
        property_storage,
        scope_bodies: Category::exact(scopes.len().saturating_mul(size_of::<RefCell<Scope>>())),
        scope_storage,
        // The visitor deduplicates everything it sees, but AST, bytecode and side-table values are
        // deliberately deferred to later vertical slices.
        strings_symbols_bigints: Category::lower_bound(visitor.strings_symbols_bigints),
        callable_metadata: Category::lower_bound(visitor.callable_metadata),
        // Ordinary backing stores are exact; shared/Wasm/host external stores remain unavailable.
        array_buffer_backing: Category::lower_bound(array_buffer_bytes),
    };
    *LAST_SNAPSHOT.lock().expect("managed-memory snapshot lock") = Some(snapshot);
}

pub(crate) fn json() -> String {
    match LAST_SNAPSHOT
        .lock()
        .expect("managed-memory snapshot lock")
        .as_ref()
    {
        Some(snapshot) => snapshot.json(),
        None => concat!(
            "{\"schema_version\":1,\"safepoint\":null,\"complete\":false,",
            "\"managed_requested_bytes\":{\"bytes\":null,\"quality\":\"unavailable\"},",
            "\"managed_external_bytes\":{\"bytes\":null,\"quality\":\"unavailable\"}}"
        )
        .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lstr::LStr;
    use crate::value::Props;

    #[test]
    fn property_storage_counts_capacity_not_length() {
        let props = Props::with_capacity(17);
        let (bytes, exact) = props.retained_requested_storage_bytes();
        assert!(exact);
        assert!(bytes >= 17 * size_of::<(Rc<str>, crate::value::Property)>());
    }

    #[test]
    fn unavailable_snapshot_is_not_reported_as_zero() {
        let json = Snapshot {
            object_bodies: Category::exact(1),
            property_storage: Category::lower_bound(2),
            scope_bodies: Category::exact(3),
            scope_storage: Category::exact(4),
            strings_symbols_bigints: Category::lower_bound(5),
            callable_metadata: Category::lower_bound(6),
            array_buffer_backing: Category::lower_bound(7),
        }
        .json();
        assert!(json.contains("\"interpreter_side_tables\":{\"bytes\":null"));
        assert!(json.contains("\"managed_requested_bytes\":{\"bytes\":21"));
    }

    #[test]
    fn shared_string_allocations_are_counted_once() {
        let string = LStr::from("shared allocation");
        let expected = string.retained_requested_bytes();
        let mut visitor = Visitor::default();
        visitor.value(&Value::Str(string.clone()));
        visitor.value(&Value::Str(string));
        assert_eq!(visitor.strings_symbols_bigints, expected);
    }

    #[test]
    fn aliased_array_buffer_backing_is_counted_once() {
        let buffer = Rc::new(RefCell::new(Vec::with_capacity(257)));
        let aliases = [buffer.clone(), buffer];
        assert_eq!(
            unique_array_buffer_capacity(aliases.iter()),
            aliases[0].borrow().capacity()
        );
    }
}
