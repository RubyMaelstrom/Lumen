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
    interpreter_side_tables: Category,
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
            self.interpreter_side_tables,
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
                "\"interpreter_side_tables\":{},\"array_buffer_backing\":{},",
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
            self.interpreter_side_tables.json(),
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
    classes: HashSet<usize>,
    chunks: HashSet<usize>,
    jit_codes: HashSet<usize>,
    hoist_plans: HashSet<usize>,
    stmt_bodies: HashSet<usize>,
    global_var_name_sets: HashSet<usize>,
    re_texts: HashSet<usize>,
    regexes: HashSet<usize>,
    rc_u16_slices: HashSet<usize>,
    array_buffers: HashSet<usize>,
    strings_symbols_bigints: usize,
    callable_metadata: usize,
    function_bytecode_metadata: usize,
    jit_heap_metadata: usize,
    regexp_metadata: usize,
    array_buffer_bytes: usize,
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

    pub(crate) fn bigint(&mut self, value: &crate::bigint::JsBigInt) {
        if self.bigints.insert(value.allocation_identity()) {
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

    pub(crate) fn symbol(&mut self, value: &Rc<SymbolData>) {
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
                self.bigint(value);
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

    fn array_buffer(&mut self, buffer: &crate::interpreter::ArrayBufferBytes) {
        let identity = Rc::as_ptr(buffer) as usize;
        if self.array_buffers.insert(identity) {
            self.array_buffer_bytes = self
                .array_buffer_bytes
                .saturating_add(buffer.borrow().capacity());
        }
    }

    pub(crate) fn function(&mut self, function: &Rc<crate::ast::Function>) {
        let identity = Rc::as_ptr(function) as usize;
        if !self.functions.insert(identity) {
            return;
        }
        let mut bytes = crate::ast::scan_function_retained_memory(function, self);
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

    pub(crate) fn class(&mut self, class: &Rc<crate::ast::Class>) {
        let identity = Rc::as_ptr(class) as usize;
        if self.classes.insert(identity) {
            let bytes = crate::ast::scan_class_retained_memory(class, self);
            self.add_function_bytecode_bytes(bytes);
        }
    }

    pub(crate) fn stmt_body(&mut self, body: &Rc<Vec<crate::ast::Stmt>>) {
        let identity = Rc::as_ptr(body) as usize;
        if self.stmt_bodies.insert(identity) {
            let bytes = crate::ast::scan_stmt_body_retained_memory(body, self);
            self.add_function_bytecode_bytes(bytes);
        }
    }

    fn global_var_names(
        &mut self,
        names: &Rc<RefCell<std::collections::HashSet<String>>>,
    ) -> (usize, bool) {
        let identity = Rc::as_ptr(names) as usize;
        if !self.global_var_name_sets.insert(identity) {
            return (0, true);
        }
        let names = names.borrow();
        let bytes = size_of::<RefCell<std::collections::HashSet<String>>>()
            .saturating_add(names.len().saturating_mul(size_of::<String>()))
            .saturating_add(
                names
                    .iter()
                    .map(String::capacity)
                    .fold(0usize, usize::saturating_add),
            );
        (bytes, names.is_empty())
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

struct DirectTotals {
    object_count: usize,
    scope_count: usize,
    property_storage: Category,
    scope_storage: Category,
    engine_caches: Category,
    interpreter_side_tables: Category,
}

impl Default for DirectTotals {
    fn default() -> Self {
        Self {
            object_count: 0,
            scope_count: 0,
            property_storage: Category::exact(0),
            scope_storage: Category::exact(0),
            engine_caches: Category::exact(0),
            interpreter_side_tables: Category::exact(0),
        }
    }
}

fn scan_realm(
    interp: &Interp,
    objects: &[Gc],
    scopes: &[Env],
    visitor: &mut Visitor,
    totals: &mut DirectTotals,
) {
    totals.object_count = totals.object_count.saturating_add(objects.len());
    totals.scope_count = totals.scope_count.saturating_add(scopes.len());
    totals.interpreter_side_tables.add(size_of::<Interp>());
    for object in objects {
        let (bytes, exact) = visitor.object(&object.borrow());
        totals.property_storage.add(bytes);
        if !exact {
            totals
                .property_storage
                .make_lower_bound("opaque standard-library HashMap bucket storage");
        }
    }
    for scope in scopes {
        let (bytes, exact) = visitor.scope(&scope.borrow());
        totals.scope_storage.add(bytes);
        if !exact {
            totals
                .scope_storage
                .make_lower_bound("opaque standard-library HashMap bucket storage");
        }
    }

    let (bytes, exact) = interp.str_units.scan_retained_memory(|(string, units)| {
        visitor.lstr(string);
        visitor.str_units(units);
    });
    totals.engine_caches.add(bytes);
    if !exact {
        totals
            .engine_caches
            .make_lower_bound("opaque standard-library HashMap bucket storage");
    }
    let (bytes, exact) = interp.re_texts.scan_retained_memory(|(string, text)| {
        visitor.lstr(string);
        visitor.re_text(text);
    });
    totals.engine_caches.add(bytes);
    if !exact {
        totals
            .engine_caches
            .make_lower_bound("opaque standard-library HashMap bucket storage");
    }
    if let Some((string, _, text)) = &interp.re_text_ascii_hot {
        visitor.lstr(string);
        visitor.re_text(text);
    }
    let (bytes, exact) = interp.regexp_programs.scan_retained_memory(visitor);
    totals.engine_caches.add(bytes);
    if !exact {
        totals
            .engine_caches
            .make_lower_bound("opaque standard-library HashMap bucket storage");
    }
    totals.engine_caches.add(
        interp
            .vm_pool
            .capacity()
            .saturating_mul(size_of::<(Vec<Value>, Vec<Value>)>())
            .saturating_add(
                interp
                    .stub_cache
                    .capacity()
                    .saturating_mul(size_of::<std::cell::Cell<crate::interpreter::StubEntry>>()),
            )
            .saturating_add(
                interp
                    .stub_cache_names
                    .borrow()
                    .capacity()
                    .saturating_mul(size_of::<Option<Rc<str>>>()),
            )
            .saturating_add(
                interp
                    .frame_pool
                    .capacity()
                    .saturating_mul(size_of::<std::ptr::NonNull<Value>>()),
            )
            .saturating_add(interp.frame_pool.len().saturating_mul(
                crate::jit::FRAME_BUF.saturating_mul(size_of::<std::mem::MaybeUninit<Value>>()),
            ))
            .saturating_add(
                interp
                    .creation_pins
                    .len()
                    .saturating_mul(size_of::<(usize, std::rc::Weak<RefCell<Object>>)>()),
            )
            .saturating_add(
                interp
                    .global_env_pins
                    .capacity()
                    .saturating_mul(size_of::<std::rc::Weak<RefCell<Scope>>>()),
            ),
    );
    for (slots, stack) in &interp.vm_pool {
        totals.engine_caches.add(
            slots
                .capacity()
                .saturating_mul(size_of::<Value>())
                .saturating_add(stack.capacity().saturating_mul(size_of::<Value>())),
        );
        debug_assert!(slots.is_empty() && stack.is_empty());
    }
    for name in interp.stub_cache_names.borrow().iter().flatten() {
        visitor.rc_str(name);
    }
    if !interp.creation_pins.is_empty() {
        totals
            .engine_caches
            .make_lower_bound("opaque standard-library HashMap bucket storage");
    }

    totals.interpreter_side_tables.add(
        interp
            .regexps
            .len()
            .saturating_mul(size_of::<(usize, Rc<crate::regex::Regex>)>()),
    );
    if !interp.regexps.is_empty() {
        totals
            .interpreter_side_tables
            .make_lower_bound("opaque standard-library HashMap bucket storage");
    }
    for regex in interp.regexps.values() {
        visitor.regex(regex);
    }
    if let Some(last) = &interp.regexp_last {
        totals.interpreter_side_tables.add(
            last.caps
                .capacity()
                .saturating_mul(size_of::<Option<(usize, usize)>>()),
        );
        visitor.lstr(&last.input);
        visitor.re_text(&last.text);
        if let Some((regex, _)) = &last.lazy_captures {
            visitor.regex(regex);
        }
    }
    if let Some(symbol) = &interp.iterator_sym {
        visitor.symbol(symbol);
    }
    totals
        .interpreter_side_tables
        .add(
            interp
                .wk_syms
                .capacity()
                .saturating_mul(size_of::<(&'static str, Value, Rc<str>)>()),
        );
    for (_, symbol, key) in &interp.wk_syms {
        visitor.value(symbol);
        visitor.rc_str(key);
    }
    totals.interpreter_side_tables.add(
        interp
            .error_protos
            .len()
            .saturating_mul(size_of::<(&'static str, Gc)>())
            .saturating_add(
                interp
                    .extra_protos
                    .len()
                    .saturating_mul(size_of::<(&'static str, Gc)>()),
            )
            .saturating_add(
                interp
                    .console
                    .capacity()
                    .saturating_mul(size_of::<String>()),
            )
            .saturating_add(
                interp
                    .eval_realm_fns
                    .len()
                    .saturating_mul(size_of::<usize>()),
            )
            .saturating_add(interp.import_base.capacity())
            .saturating_add(
                interp
                    .construct_capacity_hints
                    .len()
                    .saturating_mul(size_of::<(usize, (std::rc::Weak<RefCell<Object>>, u8))>()),
            )
            .saturating_add(
                interp
                    .htmldda
                    .len()
                    .saturating_mul(size_of::<(usize, std::rc::Weak<RefCell<Object>>)>()),
            ),
    );
    for line in &interp.console {
        totals.interpreter_side_tables.add(line.capacity());
    }
    if let Some(meta) = &interp.import_meta {
        visitor.value(meta);
    }
    if !interp.error_protos.is_empty()
        || !interp.extra_protos.is_empty()
        || !interp.eval_realm_fns.is_empty()
        || !interp.construct_capacity_hints.is_empty()
        || !interp.htmldda.is_empty()
    {
        totals
            .interpreter_side_tables
            .make_lower_bound("opaque standard-library HashMap/HashSet bucket storage");
    }

    totals.interpreter_side_tables.add(
        interp
            .modules
            .len()
            .saturating_mul(size_of::<(String, Value)>())
            .saturating_add(
                interp
                    .module_recs
                    .len()
                    .saturating_mul(size_of::<(String, crate::modules::ModuleRec)>()),
            )
            .saturating_add(
                interp
                    .pending_dynamic_imports
                    .len()
                    .saturating_mul(size_of::<(u64, crate::modules::PendingDynamicImport)>()),
            )
            .saturating_add(interp.module_ns.len().saturating_mul(size_of::<(
                usize,
                crate::fasthash::FastMap<String, crate::modules::NsBinding>,
            )>())),
    );
    for (key, value) in &interp.modules {
        totals.interpreter_side_tables.add(key.capacity());
        visitor.value(value);
    }
    for (key, record) in &interp.module_recs {
        totals.interpreter_side_tables.add(
            key.capacity()
                .saturating_add(record.scan_retained_memory(visitor)),
        );
    }
    for pending in interp.pending_dynamic_imports.values() {
        totals
            .interpreter_side_tables
            .add(pending.scan_retained_memory(visitor));
    }
    for bindings in interp.module_ns.values() {
        totals.interpreter_side_tables.add(
            bindings
                .len()
                .saturating_mul(size_of::<(String, crate::modules::NsBinding)>()),
        );
        for (name, binding) in bindings {
            totals.interpreter_side_tables.add(
                name.capacity()
                    .saturating_add(binding.scan_retained_memory(visitor)),
            );
        }
    }
    if !interp.modules.is_empty()
        || !interp.module_recs.is_empty()
        || !interp.pending_dynamic_imports.is_empty()
        || !interp.module_ns.is_empty()
    {
        totals
            .interpreter_side_tables
            .make_lower_bound("opaque standard-library HashMap bucket storage");
    }

    totals.interpreter_side_tables.add(
        interp
            .promises
            .len()
            .saturating_mul(size_of::<(usize, crate::interpreter::PromiseState)>())
            .saturating_add(
                interp
                    .unhandled_rejections
                    .len()
                    .saturating_mul(size_of::<(usize, (Value, Value))>()),
            )
            .saturating_add(
                interp
                    .microtasks
                    .capacity()
                    .saturating_mul(size_of::<crate::interpreter::Job>()),
            )
            .saturating_add(interp.host_settings_states.len().saturating_mul(size_of::<(
                (usize, u64),
                crate::interpreter::HostSettingsState,
            )>()))
            .saturating_add(
                interp
                    .retired_host_job_contexts
                    .len()
                    .saturating_mul(size_of::<u64>()),
            )
            .saturating_add(
                interp
                    .kept_alive
                    .capacity()
                    .saturating_mul(size_of::<Value>()),
            )
            .saturating_add(
                interp
                    .promise_forward
                    .len()
                    .saturating_mul(size_of::<(usize, Value)>()),
            ),
    );
    for state in interp.promises.values() {
        visitor.value(&state.value);
        totals
            .interpreter_side_tables
            .add(
                state
                    .reactions
                    .capacity()
                    .saturating_mul(size_of::<(Value, Value, Value, u64)>()),
            );
        for (on_fulfilled, on_rejected, result, _) in &state.reactions {
            visitor.value(on_fulfilled);
            visitor.value(on_rejected);
            visitor.value(result);
        }
    }
    for (promise, reason) in interp.unhandled_rejections.values() {
        visitor.value(promise);
        visitor.value(reason);
    }
    for job in &interp.microtasks {
        visitor.value(&job.handler);
        visitor.value(&job.result);
        visitor.value(&job.value);
    }
    for state in interp.host_settings_states.values() {
        // Environments are already canonical to the collector's scope snapshot. The parallel
        // global-name set is not part of an Env and therefore needs its own identity registry.
        let (bytes, exact) = visitor.global_var_names(&state.global_var_names);
        totals.interpreter_side_tables.add(bytes);
        if !exact {
            totals
                .interpreter_side_tables
                .make_lower_bound("opaque standard-library HashMap/HashSet bucket storage");
        }
    }
    let (bytes, exact) = visitor.global_var_names(&interp.global_var_names);
    totals.interpreter_side_tables.add(bytes);
    if !exact {
        totals
            .interpreter_side_tables
            .make_lower_bound("opaque standard-library HashMap/HashSet bucket storage");
    }
    for value in &interp.kept_alive {
        visitor.value(value);
    }
    for value in interp.promise_forward.values() {
        visitor.value(value);
    }
    if !interp.promises.is_empty()
        || !interp.unhandled_rejections.is_empty()
        || !interp.host_settings_states.is_empty()
        || !interp.retired_host_job_contexts.is_empty()
        || !interp.promise_forward.is_empty()
    {
        totals
            .interpreter_side_tables
            .make_lower_bound("opaque standard-library HashMap/HashSet bucket storage");
    }

    totals.interpreter_side_tables.add(
        interp
            .map_data
            .len()
            .saturating_mul(size_of::<(usize, Vec<(Value, Value)>)>()),
    );
    for entries in interp.map_data.values() {
        totals.interpreter_side_tables.add(
            entries
                .capacity()
                .saturating_mul(size_of::<(Value, Value)>()),
        );
        for (key, value) in entries {
            visitor.value(key);
            visitor.value(value);
        }
    }
    totals
        .interpreter_side_tables
        .add(interp.collection_index.len().saturating_mul(size_of::<(
            usize,
            crate::fasthash::FastMap<u64, crate::interpreter::CollectionBucket>,
        )>()));
    for index in interp.collection_index.values() {
        totals.interpreter_side_tables.add(
            index
                .len()
                .saturating_mul(size_of::<(u64, crate::interpreter::CollectionBucket)>()),
        );
        for bucket in index.values() {
            if let crate::interpreter::CollectionBucket::Many(offsets) = bucket {
                totals
                    .interpreter_side_tables
                    .add(offsets.capacity().saturating_mul(size_of::<usize>()));
            }
        }
    }
    totals
        .interpreter_side_tables
        .add(interp.weak_collection_data.len().saturating_mul(size_of::<(
            usize,
            Vec<(crate::interpreter::WeakTarget, Value)>,
        )>()));
    for entries in interp.weak_collection_data.values() {
        totals.interpreter_side_tables.add(
            entries
                .capacity()
                .saturating_mul(size_of::<(crate::interpreter::WeakTarget, Value)>()),
        );
        // Weak keys must not become strong merely because diagnostics are enabled. WeakMap values
        // are visited because the collector has already resolved ephemeron liveness.
        for (_, value) in entries {
            visitor.value(value);
        }
    }
    totals
        .interpreter_side_tables
        .add(
            interp
                .weak_collection_index
                .len()
                .saturating_mul(size_of::<(
                    usize,
                    crate::fasthash::FastMap<crate::interpreter::WeakKey, usize>,
                )>()),
        );
    for index in interp.weak_collection_index.values() {
        totals.interpreter_side_tables.add(
            index
                .len()
                .saturating_mul(size_of::<(crate::interpreter::WeakKey, usize)>()),
        );
    }
    if !interp.map_data.is_empty()
        || !interp.collection_index.is_empty()
        || !interp.weak_collection_data.is_empty()
        || !interp.weak_collection_index.is_empty()
    {
        totals
            .interpreter_side_tables
            .make_lower_bound("opaque standard-library HashMap bucket storage");
    }

    totals.interpreter_side_tables.add(
        interp
            .array_buffers
            .len()
            .saturating_mul(size_of::<(usize, crate::interpreter::ArrayBufferBytes)>())
            .saturating_add(
                interp
                    .array_buffer_versions
                    .len()
                    .saturating_mul(size_of::<(usize, u64)>()),
            )
            .saturating_add(
                interp
                    .array_buffer_dirty_ranges
                    .len()
                    .saturating_mul(size_of::<(usize, Vec<std::ops::Range<usize>>)>()),
            )
            .saturating_add(
                interp
                    .shared_buffers
                    .len()
                    .saturating_mul(size_of::<(usize, u64)>()),
            )
            .saturating_add(
                interp
                    .immutable_buffers
                    .len()
                    .saturating_mul(size_of::<usize>()),
            )
            .saturating_add(
                interp
                    .host_keyed_buffers
                    .len()
                    .saturating_mul(size_of::<usize>()),
            )
            .saturating_add(
                interp
                    .typed_arrays
                    .len()
                    .saturating_mul(size_of::<(usize, crate::value::TaInfo)>()),
            )
            .saturating_add(
                interp
                    .ta_buffer
                    .len()
                    .saturating_mul(size_of::<(usize, Value)>()),
            )
            .saturating_add(
                interp
                    .data_views
                    .len()
                    .saturating_mul(size_of::<(usize, (usize, usize, usize, bool))>()),
            ),
    );
    for ranges in interp.array_buffer_dirty_ranges.values() {
        totals.interpreter_side_tables.add(
            ranges
                .capacity()
                .saturating_mul(size_of::<std::ops::Range<usize>>()),
        );
    }
    for buffer in interp.ta_buffer.values() {
        visitor.value(buffer);
    }
    if !interp.array_buffers.is_empty()
        || !interp.array_buffer_versions.is_empty()
        || !interp.array_buffer_dirty_ranges.is_empty()
        || !interp.shared_buffers.is_empty()
        || !interp.immutable_buffers.is_empty()
        || !interp.host_keyed_buffers.is_empty()
        || !interp.typed_arrays.is_empty()
        || !interp.ta_buffer.is_empty()
        || !interp.data_views.is_empty()
    {
        totals
            .interpreter_side_tables
            .make_lower_bound("opaque standard-library HashMap/HashSet bucket storage");
    }
    for buffer in interp.array_buffers.values() {
        visitor.array_buffer(buffer);
    }

    totals.interpreter_side_tables.add(
        interp
            .shadow_realms
            .len()
            .saturating_mul(size_of::<(usize, Box<Interp>)>()),
    );
    if !interp.shadow_realms.is_empty() {
        totals
            .interpreter_side_tables
            .make_lower_bound("opaque standard-library HashMap bucket storage");
    }
    for sub in interp.shadow_realms.values() {
        let objects = crate::value::heap_gc_snapshot(&sub.gc_heap);
        let scopes = crate::value::gc_scope_snapshot(&sub.gc_heap);
        scan_realm(sub, &objects, &scopes, visitor, totals);
    }
}

fn scan_symbol_agent(interp: &Interp, visitor: &mut Visitor, totals: &mut DirectTotals) {
    let agent = interp.symbol_agent.borrow();
    totals
        .interpreter_side_tables
        .add(size_of::<crate::value::SymbolAgentState>());
    totals.interpreter_side_tables.add(
        agent
            .symbols
            .len()
            .saturating_mul(size_of::<(u64, std::rc::Weak<SymbolData>)>())
            .saturating_add(
                agent
                    .global_by_key
                    .len()
                    .saturating_mul(size_of::<(Rc<str>, Rc<SymbolData>)>()),
            )
            .saturating_add(
                agent
                    .global_key_by_id
                    .len()
                    .saturating_mul(size_of::<(u64, Rc<str>)>()),
            )
            .saturating_add(
                agent
                    .well_known
                    .len()
                    .saturating_mul(size_of::<(&'static str, Rc<SymbolData>)>()),
            ),
    );
    totals
        .interpreter_side_tables
        .make_lower_bound("opaque standard-library HashMap bucket storage");
    for (key, symbol) in &agent.global_by_key {
        visitor.rc_str(key);
        visitor.symbol(symbol);
    }
    for key in agent.global_key_by_id.values() {
        visitor.rc_str(key);
    }
    for symbol in agent.well_known.values() {
        visitor.symbol(symbol);
    }
    // The weak identity table owns no Symbol payload. `memory_snapshots` is diagnostic output
    // produced by this visitor and is excluded so measurement cannot inflate the workload.
}

fn measure(interp: &Interp, objects: &[Gc], scopes: &[Env]) -> Snapshot {
    let mut visitor = Visitor::default();
    let mut totals = DirectTotals::default();
    scan_realm(interp, objects, scopes, &mut visitor, &mut totals);
    scan_symbol_agent(interp, &mut visitor, &mut totals);

    totals
        .property_storage
        .add(visitor.detached_property_storage);
    if visitor.detached_property_storage_opaque {
        totals
            .property_storage
            .make_lower_bound("opaque standard-library HashMap bucket storage");
    }

    Snapshot {
        object_bodies: Category::exact(
            totals
                .object_count
                .saturating_mul(size_of::<RefCell<Object>>()),
        ),
        property_storage: totals.property_storage,
        scope_bodies: Category::exact(
            totals
                .scope_count
                .saturating_mul(size_of::<RefCell<Scope>>()),
        ),
        scope_storage: totals.scope_storage,
        strings_symbols_bigints: Category::lower_bound(
            visitor.strings_symbols_bigints,
            "remaining side-table owners are not yet traversed",
        ),
        callable_metadata: Category::lower_bound(
            visitor.callable_metadata,
            "native closure payloads are not yet traversed",
        ),
        function_bytecode_metadata: Category::lower_bound(
            visitor.function_bytecode_metadata,
            "Chunk call-pin HashMap bucket capacity is opaque",
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
            totals.engine_caches.bytes,
            "string and RegExp caches are covered; remaining interpreter caches are not",
        ),
        interpreter_side_tables: Category::lower_bound(
            totals.interpreter_side_tables.bytes,
            "Agent/ShadowRealm, RegExp, and symbol owners are covered; remaining Interp side tables are not",
        ),
        array_buffer_backing: Category::lower_bound(
            visitor.array_buffer_bytes,
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
            interpreter_side_tables: Category::lower_bound(12, "test lower bound"),
            array_buffer_backing: Category::lower_bound(7, "test lower bound"),
        }
        .json(1, 1);
        assert!(json.contains("\"interpreter_side_tables\":{\"bytes\":12"));
        assert!(json.contains("\"managed_requested_bytes\":{\"bytes\":62"));
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
    fn regexp_side_tables_scan_match_state_and_program_pins() {
        let mut engine = crate::Engine::new();
        engine
            .eval("/(cache)(?<tail>.*)/giu.exec('cache-state')", false)
            .expect("evaluates");

        let objects = crate::value::heap_gc_snapshot(&engine.interp.gc_heap);
        let scopes = crate::value::gc_scope_snapshot(&engine.interp.gc_heap);
        let snapshot = measure(&engine.interp, &objects, &scopes);

        assert!(snapshot.interpreter_side_tables.bytes > 0);
        assert!(snapshot.regexp_metadata.bytes > 0);
    }

    #[test]
    fn realm_symbol_caches_credit_storage_and_shared_payload_once() {
        let interp = Interp::new();
        let objects = crate::value::heap_gc_snapshot(&interp.gc_heap);
        let scopes = crate::value::gc_scope_snapshot(&interp.gc_heap);
        let snapshot = measure(&interp, &objects, &scopes);
        let wk_storage =
            interp
                .wk_syms
                .capacity()
                .saturating_mul(size_of::<(&'static str, Value, Rc<str>)>());

        assert!(!interp.wk_syms.is_empty());
        assert!(snapshot.interpreter_side_tables.bytes >= wk_storage);
        assert!(snapshot.strings_symbols_bigints.bytes > 0);
    }

    #[test]
    fn agent_snapshot_aggregates_shadow_heaps_with_global_deduplication() {
        let mut root = Interp::new();
        let mut shadow = Interp::new_with_symbol_agent(root.symbol_agent.clone());
        let shared_buffer = Rc::new(RefCell::new(Vec::with_capacity(333)));
        root.array_buffers.insert(1, shared_buffer.clone());
        shadow.array_buffers.insert(2, shared_buffer);

        let root_objects = crate::value::heap_gc_snapshot(&root.gc_heap);
        let root_scopes = crate::value::gc_scope_snapshot(&root.gc_heap);
        let shadow_objects = crate::value::heap_gc_snapshot(&shadow.gc_heap);
        let shadow_scopes = crate::value::gc_scope_snapshot(&shadow.gc_heap);
        let root_only = measure(&root, &root_objects, &root_scopes);
        let shadow_only = measure(&shadow, &shadow_objects, &shadow_scopes);
        let expected_objects = root_objects.len().saturating_add(shadow_objects.len());
        let expected_scopes = root_scopes.len().saturating_add(shadow_scopes.len());

        root.shadow_realms.insert(7, Box::new(shadow));
        let aggregate = measure(&root, &root_objects, &root_scopes);

        assert_eq!(
            aggregate.object_bodies.bytes,
            expected_objects.saturating_mul(size_of::<RefCell<Object>>())
        );
        assert_eq!(
            aggregate.scope_bodies.bytes,
            expected_scopes.saturating_mul(size_of::<RefCell<Scope>>())
        );
        assert_eq!(aggregate.array_buffer_backing.bytes, 333);
        assert!(
            aggregate.strings_symbols_bigints.bytes
                < root_only
                    .strings_symbols_bigints
                    .bytes
                    .saturating_add(shadow_only.strings_symbols_bigints.bytes),
            "Agent-shared symbol/string payload must be credited once"
        );
        assert!(aggregate.interpreter_side_tables.bytes >= 2 * size_of::<Interp>());
    }

    #[test]
    fn collection_side_tables_account_vectors_indexes_and_values() {
        let mut engine = crate::Engine::new();
        let before_objects = crate::value::heap_gc_snapshot(&engine.interp.gc_heap);
        let before_scopes = crate::value::gc_scope_snapshot(&engine.interp.gc_heap);
        let before = measure(&engine.interp, &before_objects, &before_scopes);
        engine
            .eval(
                r#"
                    globalThis.strong = new Map([["a", 1], ["b", 2], ["c", 3]]);
                    globalThis.weakKey = {};
                    globalThis.weak = new WeakMap([[weakKey, { held: "value" }]]);
                "#,
                false,
            )
            .expect("collections evaluate");
        let objects = crate::value::heap_gc_snapshot(&engine.interp.gc_heap);
        let scopes = crate::value::gc_scope_snapshot(&engine.interp.gc_heap);
        let after = measure(&engine.interp, &objects, &scopes);

        assert!(!engine.interp.map_data.is_empty());
        assert!(!engine.interp.collection_index.is_empty());
        assert!(!engine.interp.weak_collection_data.is_empty());
        assert!(!engine.interp.weak_collection_index.is_empty());
        assert!(after.interpreter_side_tables.bytes > before.interpreter_side_tables.bytes);
    }

    #[test]
    fn array_buffer_view_metadata_is_accounted_separately_from_backing() {
        let mut engine = crate::Engine::new();
        engine
            .eval(
                r#"
                    globalThis.buffer = new ArrayBuffer(64, { maxByteLength: 128 });
                    globalThis.typed = new Uint8Array(buffer);
                    globalThis.view = new DataView(buffer, 4, 16);
                    typed[2] = 7;
                "#,
                false,
            )
            .expect("buffer views evaluate");
        let objects = crate::value::heap_gc_snapshot(&engine.interp.gc_heap);
        let scopes = crate::value::gc_scope_snapshot(&engine.interp.gc_heap);
        let snapshot = measure(&engine.interp, &objects, &scopes);

        assert!(!engine.interp.array_buffers.is_empty());
        assert!(!engine.interp.typed_arrays.is_empty());
        assert!(!engine.interp.ta_buffer.is_empty());
        assert!(!engine.interp.data_views.is_empty());
        assert!(snapshot.interpreter_side_tables.bytes > size_of::<Interp>());
        assert!(snapshot.array_buffer_backing.bytes >= 64);
    }

    #[test]
    fn reusable_execution_pools_report_retained_capacity() {
        let mut engine = crate::Engine::new();
        engine.set_tier(crate::bytecode::Tier::Bytecode);
        engine.set_tier_threshold(0);
        engine
            .eval(
                "function pooled(a, b) { return a + b; } pooled(20, 22);",
                false,
            )
            .expect("bytecode function evaluates");
        let objects = crate::value::heap_gc_snapshot(&engine.interp.gc_heap);
        let scopes = crate::value::gc_scope_snapshot(&engine.interp.gc_heap);
        let snapshot = measure(&engine.interp, &objects, &scopes);
        let stub_bytes = engine
            .interp
            .stub_cache
            .capacity()
            .saturating_mul(size_of::<std::cell::Cell<crate::interpreter::StubEntry>>());

        assert!(!engine.interp.vm_pool.is_empty());
        assert!(snapshot.engine_caches.bytes >= stub_bytes);
    }

    #[test]
    fn realm_metadata_accounts_console_and_import_string_capacity() {
        let mut interp = Interp::new();
        let objects = crate::value::heap_gc_snapshot(&interp.gc_heap);
        let scopes = crate::value::gc_scope_snapshot(&interp.gc_heap);
        let before = measure(&interp, &objects, &scopes);
        let mut line = String::with_capacity(257);
        line.push_str("diagnostic console line");
        interp.console.push(line);
        interp.import_base = String::with_capacity(193);
        interp.import_base.push_str("file:///realm/");
        let after = measure(&interp, &objects, &scopes);

        assert!(
            after.interpreter_side_tables.bytes
                >= before
                    .interpreter_side_tables
                    .bytes
                    .saturating_add(257)
                    .saturating_add(193)
        );
    }

    #[test]
    fn module_records_account_ast_exports_and_namespace_indexes() {
        let mut engine = crate::Engine::new();
        let before_objects = crate::value::heap_gc_snapshot(&engine.interp.gc_heap);
        let before_scopes = crate::value::gc_scope_snapshot(&engine.interp.gc_heap);
        let before = measure(&engine.interp, &before_objects, &before_scopes);
        engine
            .eval_module(
                r#"
                    import { value as dependency } from "./dependency.js";
                    export const answer = dependency + 1;
                "#,
                "file:///main.js",
                |specifier, _| {
                    (specifier == "./dependency.js").then(|| {
                        (
                            "file:///dependency.js".to_string(),
                            "export const value = 41;".to_string(),
                        )
                    })
                },
            )
            .expect("module graph evaluates");
        let objects = crate::value::heap_gc_snapshot(&engine.interp.gc_heap);
        let scopes = crate::value::gc_scope_snapshot(&engine.interp.gc_heap);
        let after = measure(&engine.interp, &objects, &scopes);

        assert_eq!(engine.interp.module_recs.len(), 2);
        assert!(!engine.interp.module_ns.is_empty());
        assert!(after.function_bytecode_metadata.bytes > before.function_bytecode_metadata.bytes);
        assert!(after.interpreter_side_tables.bytes > before.interpreter_side_tables.bytes);
    }

    #[test]
    fn promise_reactions_and_queued_jobs_retain_their_storage() {
        let mut engine = crate::Engine::new();
        let before_objects = crate::value::heap_gc_snapshot(&engine.interp.gc_heap);
        let before_scopes = crate::value::gc_scope_snapshot(&engine.interp.gc_heap);
        let before = measure(&engine.interp, &before_objects, &before_scopes);

        let source = engine.interp.new_promise();
        let result = engine.interp.new_promise();
        engine
            .interp
            .promise_then_into(&source, Value::Undefined, Value::Undefined, result);
        let pending_objects = crate::value::heap_gc_snapshot(&engine.interp.gc_heap);
        let pending_scopes = crate::value::gc_scope_snapshot(&engine.interp.gc_heap);
        let pending = measure(&engine.interp, &pending_objects, &pending_scopes);
        assert_eq!(engine.interp.promises.len(), 2);
        assert_eq!(engine.interp.microtasks.len(), 0);
        assert!(engine
            .interp
            .promises
            .values()
            .any(|state| state.reactions.capacity() > 0));
        assert!(pending.interpreter_side_tables.bytes > before.interpreter_side_tables.bytes);

        engine
            .interp
            .resolve_promise(&source, Value::str("settled payload"));
        let queued_objects = crate::value::heap_gc_snapshot(&engine.interp.gc_heap);
        let queued_scopes = crate::value::gc_scope_snapshot(&engine.interp.gc_heap);
        let queued = measure(&engine.interp, &queued_objects, &queued_scopes);
        assert_eq!(engine.interp.microtasks.len(), 1);
        assert!(queued.interpreter_side_tables.bytes > before.interpreter_side_tables.bytes);
        assert!(queued.strings_symbols_bigints.bytes > before.strings_symbols_bigints.bytes);
    }

    #[test]
    fn host_settings_global_names_are_identity_deduplicated() {
        let mut engine = crate::Engine::new();
        engine
            .interp
            .global_var_names
            .borrow_mut()
            .insert("rootGlobalWithRetainedCapacity".to_string());
        engine.interp.switch_host_job_context(41);
        engine
            .interp
            .global_var_names
            .borrow_mut()
            .insert("contextGlobalWithRetainedCapacity".to_string());
        let shared_names = engine.interp.global_var_names.clone();
        let saved_names = &engine
            .interp
            .host_settings_states
            .values()
            .next()
            .expect("switched host context is saved")
            .global_var_names;
        assert!(Rc::ptr_eq(&shared_names, saved_names));
        let mut visitor = Visitor::default();
        let (first_names, _) = visitor.global_var_names(&shared_names);
        let (duplicate_names, duplicate_exact) = visitor.global_var_names(saved_names);
        assert!(first_names > 0);
        assert_eq!(duplicate_names, 0);
        assert!(duplicate_exact);
        let objects = crate::value::heap_gc_snapshot(&engine.interp.gc_heap);
        let scopes = crate::value::gc_scope_snapshot(&engine.interp.gc_heap);

        let first = measure(&engine.interp, &objects, &scopes);
        let second = measure(&engine.interp, &objects, &scopes);

        assert_eq!(engine.interp.host_settings_states.len(), 1);
        assert_eq!(
            first.interpreter_side_tables.bytes,
            second.interpreter_side_tables.bytes
        );
        assert!(first.interpreter_side_tables.bytes >= size_of::<Interp>());
    }

    #[test]
    fn aliased_array_buffer_backing_is_counted_once() {
        let buffer = Rc::new(RefCell::new(Vec::with_capacity(257)));
        let aliases = [buffer.clone(), buffer];
        let mut visitor = Visitor::default();
        for buffer in &aliases {
            visitor.array_buffer(buffer);
        }
        assert_eq!(visitor.array_buffer_bytes, aliases[0].borrow().capacity());
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

    #[test]
    fn recursive_ast_allocations_are_identity_deduplicated() {
        let source = r#"
            function outer({ first: [head = "default", ...tail] }, ...items) {
                class Nested extends Base {
                    ["method"]({ value = 123456789012345678901234567890n }) {
                        return `${value}:${head}`;
                    }
                }
                function inner(arg) { return { [arg]: /x+/gi, ...items }; }
                try {
                    for (let item of items) { if (item) continue; }
                } catch ({ message }) {
                    throw message;
                }
                return [Nested, inner];
            }
        "#;
        let statements = crate::parser::parse_script(source, false)
            .ok()
            .expect("recursive AST parses");
        let function = statements
            .iter()
            .find_map(|statement| match statement {
                crate::ast::Stmt::FuncDecl(function) => Some(function.clone()),
                _ => None,
            })
            .expect("outer function declaration");

        let mut visitor = Visitor::default();
        visitor.function(&function);
        let first_ast_bytes = visitor.function_bytecode_metadata;
        let first_string_bytes = visitor.strings_symbols_bigints;
        visitor.function(&function);

        assert!(first_ast_bytes > size_of::<crate::ast::Function>());
        assert!(first_string_bytes > 0);
        assert_eq!(visitor.function_bytecode_metadata, first_ast_bytes);
        assert_eq!(visitor.strings_symbols_bigints, first_string_bytes);
        assert!(visitor.functions.len() >= 3, "outer, method, and inner");
        assert_eq!(visitor.classes.len(), 1);
        assert_eq!(visitor.bigints.len(), 1);
    }
}
