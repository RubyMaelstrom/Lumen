//! Opt-in, post-collection managed-memory accounting.
//!
//! This deliberately reports requested payload/capacity bytes rather than guessing allocator
//! rounding or Rust's private `RcBox`/`HashMap` layouts. Every partial category says so in the
//! emitted record; unavailable owners are never represented by a misleading zero.

use std::cell::RefCell;
use std::collections::HashSet;
use std::mem::size_of;
use std::rc::Rc;

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
    reason: Option<&'static str>,
}

impl Category {
    fn exact(bytes: usize) -> Self {
        Self {
            bytes,
            quality: Quality::Exact,
            reason: None,
        }
    }

    fn lower_bound(bytes: usize, reason: &'static str) -> Self {
        Self {
            bytes,
            quality: Quality::LowerBound,
            reason: Some(reason),
        }
    }

    fn add(&mut self, bytes: usize) {
        self.bytes = self.bytes.saturating_add(bytes);
    }

    fn make_lower_bound(&mut self, reason: &'static str) {
        self.quality = Quality::LowerBound;
        self.reason = Some(reason);
    }

    fn json(self) -> String {
        match self.reason {
            Some(reason) => format!(
                "{{\"bytes\":{},\"quality\":\"{}\",\"reason\":\"{}\"}}",
                self.bytes,
                self.quality.json(),
                reason
            ),
            None => format!(
                "{{\"bytes\":{},\"quality\":\"{}\"}}",
                self.bytes,
                self.quality.json()
            ),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Snapshot {
    object_bodies: Category,
    property_storage: Category,
    scope_bodies: Category,
    scope_storage: Category,
    strings_symbols_bigints: Category,
    callable_metadata: Category,
    function_bytecode_metadata: Category,
    jit_heap_metadata: Category,
    regexp_metadata: Category,
    engine_caches: Category,
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
            self.function_bytecode_metadata,
            self.regexp_metadata,
            self.engine_caches,
        ]
        .into_iter()
        .map(|category| category.bytes)
        .fold(0usize, usize::saturating_add)
    }

    fn json(&self, agent_id: u64, heap_id: u64) -> String {
        format!(
            concat!(
                "{{\"schema_version\":1,\"agent_id\":{},\"heap_id\":{},",
                "\"safepoint\":\"post_gc\",\"complete\":false,",
                "\"managed_requested_bytes\":{{\"bytes\":{},\"quality\":\"lower_bound\",\"reason\":\"unavailable ownership categories are excluded\"}},",
                "\"managed_external_bytes\":{{\"bytes\":{},\"quality\":\"lower_bound\",\"reason\":\"shared, Wasm, and host backing stores are not yet included\"}},",
                "\"categories\":{{",
                "\"object_bodies\":{},\"property_storage\":{},",
                "\"scope_bodies\":{},\"scope_storage\":{},",
                "\"strings_symbols_bigints\":{},\"callable_metadata\":{},",
                "\"function_bytecode_metadata\":{},\"jit_heap_metadata\":{},",
                "\"regexp_metadata\":{},\"engine_caches\":{},",
                "\"array_buffer_backing\":{},",
                "\"interpreter_side_tables\":{{\"bytes\":null,\"quality\":\"unavailable\",\"reason\":\"Interp ownership inventory still contains unaccounted fields\"}},",
                "\"shared_wasm_backing\":{{\"bytes\":null,\"quality\":\"unavailable\",\"reason\":\"cross-Agent backing-store identity policy has not landed\"}},",
                "\"host_resources\":{{\"bytes\":null,\"quality\":\"unavailable\",\"reason\":\"host retained-size hook has not landed\"}}",
                "}}}}"
            ),
            agent_id,
            heap_id,
            self.managed_requested_bytes(),
            self.array_buffer_backing.bytes,
            self.object_bodies.json(),
            self.property_storage.json(),
            self.scope_bodies.json(),
            self.scope_storage.json(),
            self.strings_symbols_bigints.json(),
            self.callable_metadata.json(),
            self.function_bytecode_metadata.json(),
            self.jit_heap_metadata.json(),
            self.regexp_metadata.json(),
            self.engine_caches.json(),
            self.array_buffer_backing.json(),
        )
    }
}

#[derive(Default)]
pub(crate) struct Visitor {
    lstrs: HashSet<usize>,
    rc_strs: HashSet<usize>,
    symbols: HashSet<usize>,
    bigints: HashSet<usize>,
    callable_allocations: HashSet<usize>,
    functions: HashSet<usize>,
    chunks: HashSet<usize>,
    jit_codes: HashSet<usize>,
    hoist_plans: HashSet<usize>,
    re_texts: HashSet<usize>,
    regexes: HashSet<usize>,
    rc_u16_slices: HashSet<usize>,
    strings_symbols_bigints: usize,
    callable_metadata: usize,
    function_bytecode_metadata: usize,
    jit_heap_metadata: usize,
    regexp_metadata: usize,
    detached_property_storage: usize,
    detached_property_storage_opaque: bool,
}

impl Visitor {
    pub(crate) fn lstr(&mut self, value: &crate::lstr::LStr) {
        let identity = value.as_ptr() as usize;
        if self.lstrs.insert(identity) {
            self.strings_symbols_bigints = self
                .strings_symbols_bigints
                .saturating_add(value.retained_requested_bytes());
        }
    }

    pub(crate) fn rc_str(&mut self, value: &Rc<str>) {
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

    pub(crate) fn value(&mut self, value: &Value) {
        match value {
            Value::BigInt(value) => {
                if self.bigints.insert(value.allocation_identity()) {
                    self.strings_symbols_bigints = self
                        .strings_symbols_bigints
                        .saturating_add(value.retained_requested_bytes());
                }
            }
            Value::Str(value) => {
                self.lstr(value);
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
                self.function(&value.func);
            }
            Callable::Bound(value) => {
                // Box-backed variants have one allocation per containing Object, and the object
                // snapshot is identity-unique, so they need no shared-allocation identity set.
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
                // See Bound above: this Box cannot be shared between object holders.
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

    pub(crate) fn add_function_bytecode_bytes(&mut self, bytes: usize) {
        self.function_bytecode_metadata = self.function_bytecode_metadata.saturating_add(bytes);
    }

    pub(crate) fn add_jit_heap_metadata_bytes(&mut self, bytes: usize) {
        self.jit_heap_metadata = self.jit_heap_metadata.saturating_add(bytes);
    }

    pub(crate) fn str_units(&mut self, units: &crate::interpreter::StrUnits) {
        if let crate::interpreter::StrUnits::Units(units) = units {
            let identity = Rc::as_ptr(units) as *const () as usize;
            if self.rc_u16_slices.insert(identity) {
                self.strings_symbols_bigints = self
                    .strings_symbols_bigints
                    .saturating_add(units.len().saturating_mul(std::mem::size_of::<u16>()));
            }
        }
    }

    pub(crate) fn re_text(&mut self, text: &Rc<crate::regex::ReText>) {
        let identity = Rc::as_ptr(text) as usize;
        if self.re_texts.insert(identity) {
            self.regexp_metadata = self
                .regexp_metadata
                .saturating_add(text.scan_retained_memory(self));
        }
    }

    pub(crate) fn regex(&mut self, regex: &Rc<crate::regex::Regex>) {
        let identity = Rc::as_ptr(regex) as usize;
        if self.regexes.insert(identity) {
            self.regexp_metadata = self
                .regexp_metadata
                .saturating_add(regex.retained_requested_lower_bound_bytes());
        }
    }

    pub(crate) fn function(&mut self, function: &Rc<crate::ast::Function>) {
        let identity = Rc::as_ptr(function) as usize;
        if !self.functions.insert(identity) {
            return;
        }
        let mut bytes = size_of::<crate::ast::Function>()
            .saturating_add(
                function
                    .params
                    .capacity()
                    .saturating_mul(size_of::<crate::ast::Param>()),
            )
            .saturating_add(
                function
                    .body
                    .capacity()
                    .saturating_mul(size_of::<crate::ast::Stmt>()),
            );
        if let Some(name) = &function.name {
            bytes = bytes.saturating_add(name.capacity());
        }
        if let Some(source) = &function.source {
            self.rc_str(source);
        }
        if let Some((_, hoist)) = function.hoist.get() {
            let identity = Rc::as_ptr(hoist) as usize;
            if self.hoist_plans.insert(identity) {
                bytes = bytes
                    .saturating_add(size_of::<Vec<crate::ast::HoistOp>>())
                    .saturating_add(
                        hoist
                            .capacity()
                            .saturating_mul(size_of::<crate::ast::HoistOp>()),
                    );
                for op in hoist.iter() {
                    match op {
                        crate::ast::HoistOp::Var(name) => {
                            bytes = bytes.saturating_add(name.capacity());
                        }
                        crate::ast::HoistOp::Fn(name, nested)
                        | crate::ast::HoistOp::AnnexB(name, nested) => {
                            bytes = bytes.saturating_add(name.capacity());
                            self.function(nested);
                        }
                    }
                }
            }
        }
        if let Some(chunk) = function.code.get().and_then(Option::as_ref) {
            self.chunk(chunk);
        }
        if let Some(chunk) = function.code2.get().and_then(Option::as_ref) {
            self.chunk(chunk);
        }
        if let Some((function_map, prototype_map)) = function.fn_maps.get() {
            self.props(function_map);
            if let Some(prototype_map) = prototype_map {
                self.props(prototype_map);
            }
        }
        self.add_function_bytecode_bytes(bytes);
    }

    pub(crate) fn chunk(&mut self, chunk: &Rc<crate::bytecode::Chunk>) {
        let identity = Rc::as_ptr(chunk) as usize;
        if self.chunks.insert(identity) {
            chunk.scan_retained_memory(self);
        }
    }

    pub(crate) fn jit_code(&mut self, code: &Rc<crate::jit::JitCode>) {
        let identity = Rc::as_ptr(code) as usize;
        if self.jit_codes.insert(identity) {
            self.add_jit_heap_metadata_bytes(code.retained_heap_metadata_bytes());
        }
    }

    pub(crate) fn props(&mut self, props: &crate::value::Props) {
        let (bytes, exact) = props.retained_requested_storage_bytes();
        self.detached_property_storage = self.detached_property_storage.saturating_add(bytes);
        self.detached_property_storage_opaque |= !exact;
        for (name, property) in props.iter() {
            self.rc_str(name);
            self.value(&property.value());
            if let Some(getter) = property.getter() {
                self.value(getter);
            }
            if let Some(setter) = property.setter() {
                self.value(setter);
            }
        }
        for property in props.packed_values() {
            self.value(&property.value());
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
        for property in object.props.packed_values() {
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

fn measure(interp: &Interp, objects: &[Gc], scopes: &[Env]) -> Snapshot {
    let mut visitor = Visitor::default();
    let mut property_storage = Category::exact(0);
    for object in objects {
        let (bytes, exact) = visitor.object(&object.borrow());
        property_storage.add(bytes);
        if !exact {
            property_storage.make_lower_bound("opaque standard-library HashMap bucket storage");
        }
    }
    let mut scope_storage = Category::exact(0);
    for scope in scopes {
        let (bytes, exact) = visitor.scope(&scope.borrow());
        scope_storage.add(bytes);
        if !exact {
            scope_storage.make_lower_bound("opaque standard-library HashMap bucket storage");
        }
    }

    let mut engine_caches = Category::exact(0);
    let (bytes, exact) = interp.str_units.scan_retained_memory(|(string, units)| {
        visitor.lstr(string);
        visitor.str_units(units);
    });
    engine_caches.add(bytes);
    if !exact {
        engine_caches.make_lower_bound("opaque standard-library HashMap bucket storage");
    }
    let (bytes, exact) = interp.re_texts.scan_retained_memory(|(string, text)| {
        visitor.lstr(string);
        visitor.re_text(text);
    });
    engine_caches.add(bytes);
    if !exact {
        engine_caches.make_lower_bound("opaque standard-library HashMap bucket storage");
    }
    if let Some((string, _, text)) = &interp.re_text_ascii_hot {
        visitor.lstr(string);
        visitor.re_text(text);
    }
    let (bytes, exact) = interp.regexp_programs.scan_retained_memory(&mut visitor);
    engine_caches.add(bytes);
    if !exact {
        engine_caches.make_lower_bound("opaque standard-library HashMap bucket storage");
    }

    // Function-owned maps can be reached while scanning either objects or scopes. Merge their
    // storage only after both root families have completed so traversal order cannot omit it.
    property_storage.add(visitor.detached_property_storage);
    if visitor.detached_property_storage_opaque {
        property_storage.make_lower_bound("opaque standard-library HashMap bucket storage");
    }

    let array_buffer_bytes = unique_array_buffer_capacity(interp.array_buffers.values());

    Snapshot {
        object_bodies: Category::exact(objects.len().saturating_mul(size_of::<RefCell<Object>>())),
        property_storage,
        scope_bodies: Category::exact(scopes.len().saturating_mul(size_of::<RefCell<Scope>>())),
        scope_storage,
        // The visitor deduplicates everything it sees, but AST, bytecode and side-table values are
        // deliberately deferred to later vertical slices.
        strings_symbols_bigints: Category::lower_bound(
            visitor.strings_symbols_bigints,
            "AST and remaining side-table owners are not yet traversed",
        ),
        callable_metadata: Category::lower_bound(
            visitor.callable_metadata,
            "Function AST, bytecode, and native closure payloads are not yet traversed",
        ),
        function_bytecode_metadata: Category::lower_bound(
            visitor.function_bytecode_metadata,
            "nested AST allocations and uncommon Chunk plans are not yet fully traversed",
        ),
        jit_heap_metadata: Category::lower_bound(
            visitor.jit_heap_metadata,
            "heap sidecars are covered; executable mappings are reported separately",
        ),
        regexp_metadata: Category::lower_bound(
            visitor.regexp_metadata,
            "shared character classes and nested lookaround programs are not yet traversed",
        ),
        engine_caches: Category::lower_bound(
            engine_caches.bytes,
            "string and RegExp caches are covered; remaining interpreter caches are not",
        ),
        // The ordinary stores reached through this table are exact, but the category remains a
        // lower bound until shared/Wasm and host-created backing stores join the same layer.
        array_buffer_backing: Category::lower_bound(
            array_buffer_bytes,
            "shared, Wasm, and host-created backing stores are not yet traversed",
        ),
    }
}

pub(crate) fn record_post_gc(interp: &Interp, objects: &[Gc], scopes: &[Env]) {
    let snapshot = measure(interp, objects, scopes);
    let heap_id = crate::value::heap_id(&interp.gc_heap);
    interp
        .symbol_agent
        .borrow_mut()
        .memory_snapshots
        .insert(heap_id, snapshot);
}

pub(crate) fn json(interp: &Interp) -> String {
    let heap_id = crate::value::heap_id(&interp.gc_heap);
    let agent = interp.symbol_agent.borrow();
    match agent.memory_snapshots.get(&heap_id) {
        Some(snapshot) => snapshot.json(agent.agent_id, heap_id),
        None => format!(
            concat!(
                "{{\"schema_version\":1,\"agent_id\":{},\"heap_id\":{},",
                "\"safepoint\":null,\"complete\":false,",
                "\"managed_requested_bytes\":{{\"bytes\":null,\"quality\":\"unavailable\",\"reason\":\"no post-collection safepoint has been recorded\"}},",
                "\"managed_external_bytes\":{{\"bytes\":null,\"quality\":\"unavailable\",\"reason\":\"no post-collection safepoint has been recorded\"}}}}"
            ),
            agent.agent_id, heap_id
        ),
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
            property_storage: Category::lower_bound(2, "test lower bound"),
            scope_bodies: Category::exact(3),
            scope_storage: Category::exact(4),
            strings_symbols_bigints: Category::lower_bound(5, "test lower bound"),
            callable_metadata: Category::lower_bound(6, "test lower bound"),
            function_bytecode_metadata: Category::lower_bound(8, "test lower bound"),
            jit_heap_metadata: Category::lower_bound(9, "test lower bound"),
            regexp_metadata: Category::lower_bound(10, "test lower bound"),
            engine_caches: Category::lower_bound(11, "test lower bound"),
            array_buffer_backing: Category::lower_bound(7, "test lower bound"),
        }
        .json(1, 1);
        assert!(json.contains("\"interpreter_side_tables\":{\"bytes\":null"));
        assert!(json.contains("\"managed_requested_bytes\":{\"bytes\":50"));
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
    fn cache_pins_are_scanned_into_canonical_allocation_families() {
        let mut interp = Interp::new();
        let non_ascii = LStr::from("éééé");
        interp.units_full(&non_ascii);
        interp.re_text(true, &non_ascii);
        let ascii = LStr::from("cache-hot-ascii");
        interp.re_text(false, &ascii);
        interp
            .compiled_regexp("(?:cache)+", "gi")
            .unwrap_or_else(|_| panic!("regexp compiles"));

        let objects = crate::value::heap_gc_snapshot(&interp.gc_heap);
        let scopes = crate::value::gc_scope_snapshot(&interp.gc_heap);
        let first = measure(&interp, &objects, &scopes);
        let second = measure(&interp, &objects, &scopes);

        assert!(first.engine_caches.bytes > 0);
        assert!(first.regexp_metadata.bytes > 0);
        assert!(first.strings_symbols_bigints.bytes >= non_ascii.len() + ascii.len());
        assert_eq!(first.json(9, 12), second.json(9, 12));
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

    #[test]
    fn repeated_scan_at_one_safepoint_is_byte_deterministic() {
        let interp = Interp::new();
        let objects = crate::value::heap_gc_snapshot(&interp.gc_heap);
        let scopes = crate::value::gc_scope_snapshot(&interp.gc_heap);
        assert_eq!(
            measure(&interp, &objects, &scopes).json(7, 11),
            measure(&interp, &objects, &scopes).json(7, 11)
        );
    }

    #[test]
    fn independent_agents_cannot_overwrite_each_others_snapshots() {
        let first = Interp::new();
        let first_objects = crate::value::heap_gc_snapshot(&first.gc_heap);
        let first_scopes = crate::value::gc_scope_snapshot(&first.gc_heap);
        record_post_gc(&first, &first_objects, &first_scopes);
        let first_before = json(&first);

        let second = Interp::new();
        let second_objects = crate::value::heap_gc_snapshot(&second.gc_heap);
        let second_scopes = crate::value::gc_scope_snapshot(&second.gc_heap);
        record_post_gc(&second, &second_objects, &second_scopes);

        assert_eq!(json(&first), first_before);
        assert_ne!(
            first.symbol_agent.borrow().agent_id,
            second.symbol_agent.borrow().agent_id
        );
        assert_ne!(
            crate::value::heap_id(&first.gc_heap),
            crate::value::heap_id(&second.gc_heap)
        );
    }

    #[test]
    fn compiled_function_metadata_is_retained_in_its_canonical_categories() {
        let mut engine = crate::Engine::new();
        engine.set_tier(crate::bytecode::Tier::Jit);
        engine.set_tier_threshold(0);
        engine
            .eval(
                "function f(o) { let n = 0; for (let i=0;i<20;i++) n += o.x+i; return n; } globalThis.keep=f; f({x:1});",
                false,
            )
            .expect("metadata fixture parses");
        let objects = crate::value::heap_gc_snapshot(&engine.interp.gc_heap);
        let scopes = crate::value::gc_scope_snapshot(&engine.interp.gc_heap);
        let snapshot = measure(&engine.interp, &objects, &scopes);
        assert!(snapshot.function_bytecode_metadata.bytes > 0);
        assert!(snapshot.callable_metadata.bytes > 0);
    }
}
