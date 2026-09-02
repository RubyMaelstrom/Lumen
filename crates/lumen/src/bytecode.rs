//! Bytecode tier v0: a per-function stack VM behind the tree-walking interpreter.
//!
//! The tree-walker is the reference oracle: compiled tiers are expected to match its observable
//! semantics and are checked against it by the differential test harness. A function is either
//! compiled *whole* (its body contains only
//! constructs this compiler fully understands) or it runs in the tree-walker; there is no partial
//! compilation and no deoptimization. Every operation with observable semantics (property access,
//! calls, coercions, name resolution outside the function) delegates to the interpreter's own
//! helpers, so behavior differences can only come from the local-variable and dispatch layers.
//!
//! Locals normally live in a flat slot vector. Bindings observable through closures, direct eval,
//! mapped arguments, or other dynamic environments are instead homed in a retained activation;
//! uncommon non-suspending syntax can share the normative evaluator through a projected frame.
//! TDZ is represented by `Value::Empty` in a slot — reads check it and throw the same
//! ReferenceError the tree-walker would.
//!
//! Tier selection (see `Interp::tier`): `jit` (default), `bytecode`, or `interp` (this module is
//! never entered — the tree-walker runs). Compiled tiers kick in at `tier_threshold` calls (0 =
//! immediately). Selectable via the `LUMEN_TIER` / `LUMEN_TIER_THRESHOLD` env vars, the CLI's
//! `--tier`, or `Engine::set_tier`.

use std::rc::Rc;

use crate::ast::*;
use crate::interpreter::{Abrupt, Env, Interp};
use crate::value::Value;

/// Execution tier. `Interp` must not touch any codegen path at all; `Jit` compiles eligible
/// chunks to ARM64 machine code (macOS/Apple Silicon), falling back to the bytecode VM.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    Interp,
    Bytecode,
    Jit,
}

/// Per-site property inline-cache state. `depth == IC_EMPTY` means the site has not cached yet.
/// Otherwise the property was last found as an own, non-accessor data property of the object
/// `depth` prototype hops above the receiver, at `entries` slot `slot` — and every hop below the
/// holder had *no* own property of that name. A hit re-validates all of that (each hop plain and
/// missing `name`, the holder's cached slot still keyed `name`), so a stale cache — including a
/// *different* object reaching this shared per-site cache — can only cost time, never correctness.
///
/// `recv_shape` / `holder_shape` are the receiver's and holder's [object shapes] at cache time.
/// For a `depth == 0` or `depth == 1` hit on non-exotic objects they turn validation into shape-
/// id compares (no per-hop key/hash checks): shapes are shared across structurally-identical
/// objects, so matching one recorded from object A on object B guarantees B's `slot` maps `name`
/// too. Deeper hits, exotics (arrays), and shape misses fall back to the key-checked walk.
///
/// [object shapes]: crate::value::Props::shape
///
/// `repr(C)` with this field order gives the JIT's inline templates fixed byte offsets to read
/// the live cache from machine code: recv_shape@0, holder_shape@4, slot@8, depth@12, mid_ok@13,
/// mid_shape@16.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct IcState {
    pub recv_shape: u32,
    pub holder_shape: u32,
    pub slot: u32,
    pub depth: u8,
    /// Bit 0: `mid_shape` was recorded (a `depth ≥ 2` fill whose depth-1 hop was a plain
    /// ordinary object); bit 1: `mid2_shape` too (depth 3). Flags are needed because shape id 0
    /// is a real shape (the empty object).
    pub mid_ok: u8,
    /// The intermediate (depth-1) hop's shape for a `depth == 2` hit: a match proves that hop
    /// still lacks the name, making the two-hop shape fast path sound.
    pub mid_shape: u32,
    /// The depth-2 hop's shape for a `depth == 3` hit (three-level class hierarchies put base
    /// methods three hops from an instance; without this they'd re-walk every access). Recorded
    /// iff `mid_ok & 2`. The JIT templates handle depth ≤ 2 and route deeper hits to the helper.
    pub mid2_shape: u32,
}

/// Byte offsets into an [`IcState`] `Cell`, for the JIT inline templates.
pub const IC_OFF_RECV_SHAPE: u32 = 0;
pub const IC_OFF_HOLDER_SHAPE: u32 = 4;
pub const IC_OFF_SLOT: u32 = 8;
pub const IC_OFF_DEPTH: u32 = 12;
pub const IC_OFF_MID_OK: u32 = 13;
pub const IC_OFF_MID_SHAPE: u32 = 16;
pub const IC_OFF_MID2_SHAPE: u32 = 20;

pub const IC_EMPTY: u8 = u8::MAX;
/// `IcState::depth` marker for a cached ABSENT property: on a receiver of `recv_shape`, `name`
/// is missing along the entire (all `Exotic::None`, all ic-plain) prototype chain. The chain's
/// shapes sit in recv_shape/mid_shape/mid2_shape/holder_shape in walk order with the level
/// count in `slot` (1-4); a hit re-walks the live chain validating each shape and yields
/// `undefined`. Shape-only proof is sound for absence: a shape pins the exact key set of a
/// non-elem-mode map, and every level's exotic/side-table gates are re-checked live.
pub const IC_ABSENT: u8 = 0xFC;
/// Way count of a property IC site: `Compiler::new_cache` allocates this many consecutive
/// cells, the Rust probes walk all of them, and the JIT get/set templates loop over them
/// inline (a 3-4 shape site — one dispatch loop over a class hierarchy — otherwise thrashes
/// 2 ways and helper-calls forever).
pub const PROP_IC_WAYS: usize = 4;
/// Flag bit OR'd into `IcState::depth` when the HOLDER is an `Exotic::Array` (including a
/// depth-0 array receiver — `arr.length`, and `Array.prototype`, itself an Array exotic, as a
/// method holder): array shapes don't pin named slots (element entries occupy slots without
/// transitioning the shape), so a hit must re-check the entry's key. The JIT templates compare
/// `depth` exactly and so route these to the helper automatically. Only meaningful while
/// `depth < 0x80` (`IC_CREATE`/`IC_EMPTY` have the bit set but are filtered by range first).
pub const IC_ARR_KEYCHK: u8 = 0x40;
/// Deepest prototype hop the IC will record; hotter sites deeper than this stay on the slow path.
pub const IC_MAX_DEPTH: u8 = 4;
/// `IcState::depth` marker for a property-*creation* cache (constructor `this.x = v` on a fresh
/// shape): `recv_shape` is the shape BEFORE the insert and `mid_shape` holds the
/// [`crate::value::proto_epoch`] at fill. A hit requires the same shape, the same receiver
/// prototype *identity* (weak-pinned by identity in `Interp::creation_pins` — same shape does NOT
/// imply same proto), an unchanged epoch (no marked prototype mutated, no proto swap, no defineProperty
/// anywhere), a live `extensible` receiver, and a non-index name (guaranteed at fill): together
/// they re-prove the fill-time chain walk ("no hop has an own copy / setter / non-writable shadow
/// of this name"), so the insert can skip the whole `OrdinarySet` walk.
pub const IC_CREATE: u8 = 0xFD;

/// Per-site free-name inline-cache state (`LoadName` / `LoadNameForCall`): the last successful
/// *depth-0* resolution — the name was found directly in the scope the chunk runs under (`env`),
/// as a plain initialized binding (no `with` object on that scope, no live module import).
///
/// A hit revalidates: (1) the current env is *the same allocation* — `env` compares raw pointers,
/// which is ABA-safe because `Chunk::name_pins` holds a `Weak` to the cached scope, pinning its
/// allocation for the cache's lifetime; (2) the scope's [`crate::interpreter::VarMap`] generation
/// is unchanged — every structural map mutation bumps it, so `binding` still points at the live
/// entry *and* no insert/remove could have changed what the name resolves to. Depth-0-only is
/// what makes the generation check complete: with no intermediate scopes between start and
/// holder, there is nothing else whose mutation could re-route the name (a sloppy direct `eval`
/// hoisting into this scope, or a `delete`, is an insert/remove here and bumps the generation).
///
/// In-place binding writes don't bump the generation, so a hit reads the *live* value and the
/// live `initialized` flag through the pointer — both exactly what the slow path would see.
///
/// A second mode covers *globals* (`Math`, a script-level `var`, a top-level function): when the
/// chunk's env IS the global scope (no intermediate scopes to guard), a resolution that missed
/// the scope and landed on an own data property of the ordinary global object caches
/// `(env|1, shape<<32|slot, gen)` — the low bit of `env` tags the mode (scope pointers are
/// ≥8-aligned). A hit revalidates the scope generation (no shadowing binding appeared) and the
/// global object's shape (same ordered key layout ⇒ the slot still maps this name), then
/// re-checks `accessor` at the slot (attributes are not part of the shape).
///
/// `repr(C)` with this field order gives the JIT template fixed offsets: env@0, binding@8, gen@16.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct NameIc {
    /// `Rc::as_ptr` of the scope at cache time (0 = empty). Low tag bits (scope allocations
    /// are ≥8-aligned): bit 0 = global-object mode; bit 1 = depth-1 mode, where the pointer
    /// is the current env's PARENT — the current env is this chunk's activation, whose fresh
    /// per-call pointer could never hit an exact compare (see `Chunk::name_ic_fill`).
    pub env: usize,
    /// Scope mode: the resolved `&Binding` within that scope's map. Global mode: shape<<32|slot.
    /// `u64` (not `usize`) so the packing is well-defined on 32-bit targets (wasm).
    pub binding: u64,
    /// Generation of the map holding `binding` at fill time (structural changes invalidate).
    pub gen: u32,
    /// Depth-1 mode: the activation's post-construction generation — chunk-determined (the
    /// cap_inits insert count), so it validates EVERY fresh activation of this chunk while
    /// catching a sloppy inner eval's var hoisted into a live one. 0 otherwise.
    pub act_gen: u32,
}

impl NameIc {
    pub const EMPTY: NameIc = NameIc {
        env: 0,
        binding: 0,
        gen: 0,
        act_gen: 0,
    };
}

/// Byte offsets into a [`NameIc`] `Cell`, for the JIT inline template.
pub const NAME_IC_OFF_ENV: u32 = 0;
pub const NAME_IC_OFF_BINDING: u32 = 8;
pub const NAME_IC_OFF_GEN: u32 = 16;
pub const NAME_IC_OFF_ACT_GEN: u32 = 20;

impl IcState {
    pub const EMPTY: IcState = IcState {
        recv_shape: 0,
        holder_shape: 0,
        slot: 0,
        depth: IC_EMPTY,
        mid_ok: 0,
        mid_shape: 0,
        mid2_shape: 0,
    };
}

/// Per-site call cache (`Op::Call` / `Op::CallWithThis`): the last callee that took the JIT→JIT
/// fast call at this site, keyed by object identity. `callee` is the callee object's stored `Rc`
/// pointer; a `Weak` pin in [`Chunk::call_pins`] keeps that ADDRESS from ever being recycled, so
/// a live `Value::Obj` whose payload equals `callee` proves it is the *same, still-alive* object
/// — and therefore that everything recorded at fill time still holds: its `call` field is the
/// same `Callable::User` (a live function's `call` is never reassigned; the one upgrade site
/// only converts `Callable::None`), which pins the `Function`, its env, its compiled chunk
/// (`Function::code` is set once) and machine code (`Chunk::jit` is set once). A hit therefore
/// skips the borrow + dispatch checks and reads everything through raw pointers with a single
/// refcount bump (the env handle the frame needs).
///
/// The fill happens only after [`crate::interpreter::Interp::call_jit_fast`] passed its full
/// guard set (plain same-realm user fn, not an arrow / class ctor / proxy, compiled, machine
/// code, no activation env), so a hit replays exactly that committed path. The same-realm proof
/// is carried by `global_env`: the fill's env-root walk was relative to the then-active global
/// scope, whose address is compared raw on every hit (and weak-allocation-pinned in
/// `Interp::global_env_pins` so it cannot be recycled); a realm switch changes the active
/// global and makes every cached site miss and revalidate. The blanket proxy/realm gates
/// `call_jit_fast` re-checks per call are safe to skip on a hit: they exist to keep EXOTIC
/// callees off the fast path, and identity proves this callee is the same plain user function.
#[derive(Clone, Copy)]
/// `repr(C)` with this field order gives the JIT call template fixed byte offsets for its
/// inline way-1 probe: callee@0, env@8, chunk@16, code@24, global_env@32, strict@40,
/// uses_this@41, n_params@42, n_slots@44, func@48, epoch@56 — 64-byte stride inside
/// [`CallSite::entries`] (compile-asserted in jit.rs).
#[repr(C)]
pub struct CallIc {
    /// Stored `Rc` pointer of the callee function object; 0 = empty.
    pub callee: usize,
    /// `Rc::as_ptr` of the callee's closure env (a hit reconstructs one owned handle from it).
    pub env: *const std::cell::RefCell<crate::interpreter::Scope>,
    /// Address of the `Rc<Chunk>` handle inside the callee's `Function::code` (set-once cell).
    pub chunk: *const Rc<Chunk>,
    /// `Rc::as_ptr` of the chunk's machine code.
    pub code: *const crate::jit::JitCode,
    /// `Rc::as_ptr` of the active realm's global scope at fill time: the fill's same-realm proof
    /// (the callee's env-chain root) is relative to it, so a hit requires the active global to be
    /// unchanged — a realm switch makes every site miss and revalidate through the full path.
    pub global_env: usize,
    /// The callee's strictness (feeds the frame record and the `this` binding).
    pub strict: bool,
    /// `Chunk::uses_this()` at fill time (set-once state): skips the `this` binding entirely.
    pub uses_this: bool,
    /// `Chunk::jit_frame()` at fill time: the callee frame shape without touching the chunk.
    pub n_params: u16,
    pub n_slots: u16,
    /// Direct-call gates computed at fill: bit 0 = a compiled user-code entry; bit 1 = the chunk
    /// needs the realm's global body pointer (the sequence requires the caller's
    /// `ctx.global_body` to be live).
    pub direct: u8,
    /// `Rc::as_ptr` of the callee's `ast::Function` (alive while the callee object is: `call`
    /// is never reassigned) — the inline-recompile trigger needs the AST.
    pub func: *const crate::ast::Function,
    /// [`CALL_IC_EPOCH`] at fill time; a mismatch forces a re-fill (so a fresh second-stage
    /// compile of the callee replaces the cached chunk/code pointers).
    pub epoch: u32,
    /// `Rc::as_ptr` of the callee's chunk — what `ctx.chunk` holds (the direct-call sequence
    /// swaps it in without the RcBox-offset arithmetic a `*const Rc<Chunk>` deref would need).
    /// NULL for a NATIVE entry (see `native`) — every machine-code consumer that dereferences
    /// chunk state must route null to the helper first.
    pub chunk_raw: *const Chunk,
    /// The callee machine code's entry address (`JitCode::mem`).
    pub code_mem: *const u8,
    /// The callee's pc→code-offset table data pointer (`JitCode::pc_offsets.as_ptr()`).
    pub pc_offs_ptr: *const u32,
    /// Native-callee entry: the builtin's fn pointer (0 = a user-function entry). A hit
    /// invokes it straight from the IC — no receiver borrow, no `Callable` dispatch, no
    /// re-probe. Identity/epoch/realm guards are exactly the user-entry ones; the callee
    /// object is pinned in `call_pins` like any cached callee, and a builtin's `call` field
    /// is never reassigned while the object lives.
    pub native: usize,
    /// Intrinsic id for a native entry the call template can inline entirely (0 = none;
    /// see `INTRINSIC_CHAR_CODE_AT`). Filled from the fn pointer at record time.
    pub intrinsic: u8,
}

/// [`CallIc::direct`] bit 4: the callee needs an activation environment (captured locals or
/// lexical `this`) — the committed path enters through `jit::run` (which builds it) instead of
/// `run_moved`. Bits 0-3 stay clear on such entries, so the machine-code direct sequence's
/// first gate routes them to the helper.
pub const CALL_IC_NEEDS_ENV: u8 = 16;

/// `String.prototype.charCodeAt`: the call template inlines the all-ASCII receiver + exact-u32
/// in-bounds index case to a byte load (see `crate::lstr::ASCII_HINT`).
pub const INTRINSIC_CHAR_CODE_AT: u8 = 1;
pub const INTRINSIC_STRING_SLICE: u8 = 2;
pub const INTRINSIC_OBJECT_HAS_OWN: u8 = 3;
pub const INTRINSIC_FUNCTION_APPLY: u8 = 4;
pub const INTRINSIC_MATH_SQRT: u8 = 5;
pub const INTRINSIC_ARRAY_PUSH: u8 = 6;
pub const INTRINSIC_ARRAY_POP: u8 = 7;
pub const INTRINSIC_FUNCTION_CALL: u8 = 8;
pub const INTRINSIC_REGEXP_EXEC_DISCARD: u8 = 9;
pub const INTRINSIC_STRING_REPLACE_DISCARD: u8 = 10;
pub const INTRINSIC_STRING_SPLIT_DISCARD: u8 = 11;
pub const INTRINSIC_CHAR_AT: u8 = 12;

impl CallIc {
    pub const EMPTY: CallIc = CallIc {
        callee: 0,
        env: std::ptr::null(),
        chunk: std::ptr::null(),
        code: std::ptr::null(),
        global_env: 0,
        strict: false,
        uses_this: false,
        n_params: 0,
        n_slots: 0,
        direct: 0,
        func: std::ptr::null(),
        epoch: 0,
        chunk_raw: std::ptr::null(),
        code_mem: std::ptr::null(),
        pc_offs_ptr: std::ptr::null(),
        native: 0,
        intrinsic: 0,
    };
}

/// A speculative-inline site's guard data: the pinned expected callee plus how the call site's
/// stack is shaped when the guard runs (`argc` arguments above the callee).
pub struct InlineTarget {
    /// Stored `Rc` pointer of the expected callee function object.
    pub expected: usize,
    /// Keeps `expected` from ever being recycled (same ABA argument as [`CallIc`]): a live
    /// `Value::Obj` whose payload equals it therefore IS the same, still-alive function.
    pub pin: std::rc::Weak<std::cell::RefCell<crate::value::Object>>,
    /// Exact shared closure environment required by a non-global free-name inline. Zero means
    /// the callee has no such dependency.
    pub expected_env: usize,
    pub argc: u16,
    /// Sloppy callee that reads `this`: the receiver must already be an object (the generic
    /// path would box a primitive or substitute the global object).
    pub check_this: bool,
}

/// Bumped whenever any function gains a second-stage (inlined) compile: every [`CallIc`] fills
/// with the current value and misses on mismatch, so cached callers re-resolve through
/// [`crate::interpreter::Interp::call_jit_fast`] and pick up `Function::code2`.
pub static CALL_IC_EPOCH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// A call site's cache: 4-way set-associative over callee identity. Method-dispatch sites are
/// routinely polymorphic (DeltaBlue rotates a handful of `execute` implementations through one
/// loop), so a single entry thrashes; four entries filled round-robin stabilize any site with up
/// to four distinct callees.
/// Way count of [`CallSite::entries`] — the JIT call template's inline probe walks all of them.
pub const CALL_IC_WAYS: usize = 4;

pub struct CallSite {
    pub entries: [std::cell::Cell<CallIc>; CALL_IC_WAYS],
    /// Round-robin fill cursor.
    pub next: std::cell::Cell<u8>,
}

impl CallSite {
    pub fn empty() -> CallSite {
        CallSite {
            entries: [
                std::cell::Cell::new(CallIc::EMPTY),
                std::cell::Cell::new(CallIc::EMPTY),
                std::cell::Cell::new(CallIc::EMPTY),
                std::cell::Cell::new(CallIc::EMPTY),
            ],
            next: std::cell::Cell::new(0),
        }
    }
    /// Record `ic`, replacing an existing way for the same callee (epoch or realm refills must
    /// not fan one callee across ways — the inline planner reads way-count as polymorphism),
    /// else the next way round-robin.
    pub fn fill(&self, ic: CallIc) {
        for e in &self.entries {
            if e.get().callee == ic.callee {
                e.set(ic);
                return;
            }
        }
        let k = self.next.get() as usize & 3;
        self.entries[k].set(ic);
        self.next.set((k as u8 + 1) & 3);
    }
}

/// Monomorphic `new`-site cache. Constructors are overwhelmingly fixed per source site, so this
/// avoids the interpreter-wide constructor hash table after the first execution while retaining
/// the same epoch/realm/prototype guards.
#[derive(Clone, Copy)]
pub(crate) struct ConstructSite {
    pub call: CallIc,
    pub prototype_shape: u32,
    pub prototype_slot: u32,
    pub arguments_apply_forwarder: bool,
}

impl ConstructSite {
    const EMPTY: ConstructSite = ConstructSite {
        call: CallIc::EMPTY,
        prototype_shape: 0,
        prototype_slot: 0,
        arguments_apply_forwarder: false,
    };
}

/// Which update `UpdateLocal` performs, and the value it leaves on the stack: `Pre*` push the
/// updated value, `Post*` push the original (coerced) value, `*Discard` push nothing (the update
/// is a statement or a `for` update — its value is unobservable).
#[derive(Clone, Copy, Debug)]
pub enum UpdKind {
    PreInc,
    PreDec,
    PostInc,
    PostDec,
    IncDiscard,
    DecDiscard,
}

#[derive(Clone, Copy, Debug)]
pub enum Op {
    Const(u32),
    Undef,
    Dup,
    Pop,
    LoadLocal(u16),
    StoreLocal(u16),
    /// Read a captured local from the activation environment (TDZ-checked). The operand indexes
    /// `names`; the activation env holds exactly the captured bindings, so this is one hash hit.
    LoadCap(u32),
    /// Write a captured local (TDZ-checked: assignment before a lexical's initialization throws).
    StoreCap(u32),
    /// Initialize a captured lexical (`let`/`const` declaration): sets the value and clears TDZ.
    StoreCapInit(u32),
    /// `++`/`--` on a captured local, in place (the env-homed `UpdateLocal`).
    UpdateCap(u32, UpdKind),
    /// `++`/`--` on a free binding resolved through the closure/global environment.
    UpdateName(u32, UpdKind),
    /// [`Op::UpdateName`] with a per-site generation-checked name cache. The JIT uses it for a
    /// guarded numeric update; misses retain the complete free-name/ToNumeric semantics.
    UpdateNameCached(u32, u32, UpdKind),
    /// Create a closure over the current environment from `Chunk::funcs[fidx]`. The second
    /// operand names an anonymous function expression per NamedEvaluation (`names` index, or
    /// `u32::MAX` for none).
    MakeClosure(u32, u32),
    /// `++`/`--` on a local slot, done in place (no LoadLocal/Plus/Add dance). Applies ToNumeric
    /// so a BigInt slot stays a BigInt — the `Plus`-based lowering this replaces was ToNumber and
    /// wrongly threw on BigInt. The `UpdKind` says increment vs decrement and which value (old,
    /// new, or none in statement position) to leave on the stack.
    UpdateLocal(u16, UpdKind),
    /// Put the slot into its temporal dead zone (block entry for `let`/`const`).
    Tdz(u16),
    /// Read a free name (resolved through the scope chain / global). Operands: name index,
    /// per-site [`NameIc`] index into `Chunk::name_caches`.
    LoadName(u32, u32),
    StoreName(u32),
    /// [`Op::StoreName`] with a per-site generation-checked name cache.
    StoreNameCached(u32, u32),
    /// Resolve one identifier Reference now and retain its exact Environment Record/object base
    /// in continuation-owned storage. `LoadRef`/`StoreRef` reuse it after arbitrary suspension or
    /// user code instead of repeating ResolveBinding against a changed `with`/eval environment.
    ResolveNameRef(u32, u16),
    LoadRef(u16),
    StoreRef(u16),
    /// Assignment to a statically-known immutable slot lexical. Consume the attempted value,
    /// preserve SetMutableBinding's TDZ-before-immutability check, then throw the runtime
    /// TypeError. Operands are the slot and the diagnostic name index.
    StoreConstLocal(u16, u32),
    /// The activation-environment form of [`Op::StoreConstLocal`].
    StoreConstCap(u32),
    /// Complete ToNumeric and ±1 for an immutable update target, then reject its PutValue. The
    /// old value is consumed from the stack; a preceding LoadLocal/LoadCap performs GetValue and
    /// its TDZ check in the normative order.
    UpdateConst(u32, UpdKind),
    LoadThis,
    /// Resolve the lexical `this` binding at the point of evaluation. Async arrows can outlive
    /// a derived constructor and observe its binding change from uninitialized to initialized
    /// after a suspended `super()` call, so entry-time frame seeding is not equivalent.
    LoadLexicalThis,
    /// RequireObjectCoercible on the top stack value while retaining it as a Reference base.
    RequireObject,
    /// `obj.name`. First operand is the name index; second is the per-site inline-cache index into
    /// `Chunk::caches` (see `Interp::get_prop_ic`).
    GetProp(u32, u32),
    /// `this.name` — GetProp with the receiver read straight from the frame's `this` binding:
    /// no operand-stack traffic and no receiver refcounting (the frame owns the binding).
    GetPropThis(u32, u32),
    /// `local.name` — receiver read straight from a slot (alive for the whole frame). A TDZ
    /// slot throws the same ReferenceError LoadLocal would.
    GetPropLocal(u16, u32, u32),
    /// `obj.name = v`. Operands: name index, inline-cache index.
    SetProp(u32, u32),
    /// `obj.name = v` in statement position: stores without leaving `v` on the stack.
    SetPropDrop(u32, u32),
    /// `this.name = v` statement — SetPropDrop with the receiver from the frame's `this`
    /// binding: pops only the value.
    SetPropThisDrop(u32, u32),
    /// `local.name = v` statement — receiver from a slot; pops only the value. Only emitted
    /// when the RHS provably can't reassign the local (evaluation-order safety).
    SetPropLocalDrop(u16, u32, u32),
    /// Template-literal substitution: ToString (string hint) on the top of the stack.
    ToStr,
    /// `for…of` prologue: pops the iterable, pushes the iterator object then its `next` method
    /// (GetIterator with the sync hint, via the interpreter's own helper).
    GetIter,
    /// `for await…of` prologue: GetIterator(value, async), represented without allocating the
    /// otherwise-unobservable Async-from-Sync wrapper. Pushes iterator, next method, and a bool
    /// identifying the sync fallback so stepping/closing can run its continuation algorithms.
    GetAsyncIter,
    /// `for…in` prologue: pops the RHS and pushes an internal dense array containing the candidate
    /// enumerable string keys in prototype-chain order (deduplicated by first occurrence).
    ForInKeys,
    /// Step an internal `for…in` key snapshot. Operands are the key-array, numeric cursor, and
    /// original RHS slots. Deleted object properties are skipped before pushing key + has-value.
    ForInStepL(u16, u16, u16),
    /// One `for…of` step against the iterator/next stored in the two slots: pushes the yielded
    /// value (or `undefined` at exhaustion) then a has-value bool — a following `JumpIfFalse`
    /// exits the loop, so the branch reuses existing machinery in both tiers. A `next`/`done`/
    /// `value` trap throw propagates as-is (the spec skips IteratorClose for step throws).
    IterStepL(u16, u16),
    /// IteratorClose on the iterator in the slot, normal-completion mode (close errors
    /// propagate): emitted where a `break`/`return` legitimately exits a compiled `for…of`.
    IterCloseL(u16),
    /// The `for…of` body's catch pad: pops the in-flight exception, closes the slot's iterator
    /// in throw mode (trap errors swallowed, per spec), and rethrows. Always abrupt.
    IterAbortL(u16),
    /// One IteratorDestructuringAssignmentEvaluation step. The third slot is the iterator
    /// record's [[Done]] state; it is set before a potentially abrupt IteratorStep so the
    /// surrounding assignment-pattern handler knows not to close after a step failure.
    DestructureStepL(u16, u16, u16),
    /// Drain the remaining destructuring iterator into a fresh Array, with the same [[Done]]
    /// bookkeeping as `DestructureStepL`.
    DestructureRestL(u16, u16, u16),
    /// IteratorClose only while an assignment-pattern iterator is still live.
    IterCloseIfNotDoneL(u16, u16),
    /// Throw-mode conditional IteratorClose: consume/preserve the in-flight exception.
    IterAbortIfNotDoneL(u16, u16),
    /// Start one `for await…of` step and suspend on the result promise. Operands are iterator,
    /// next, async-from-sync flag, and saved done flag slots.
    AsyncIterStepL(u16, u16, u16, u16),
    /// Consume the settled step value and push iteration value + has-value bool.
    AsyncIterResumeL(u16, u16),
    /// Begin AsyncIteratorClose and suspend as needed. The bool selects throw-completion mode,
    /// where close errors are discarded before execution resumes at the following throw pad.
    AsyncIterCloseL(u16, u16, bool),
    /// Object-destructuring guard: throws the oracle's TypeError when the value on top of the
    /// stack (peeked, not popped) is null/undefined — GetProp's own nullish error has a
    /// different message, and the check must run before any property read.
    DestructureGuard,
    /// Array-destructuring prologue: pops the iterable and walks the iterator protocol for `n`
    /// pattern elements (undefined once exhausted; IteratorClose when the pattern didn't drain
    /// it), pushing the `n` element values (last element on top — the stores emit in reverse,
    /// which is unobservable because only uncaptured slot leaves compile to this). Flat
    /// ident/hole elements only: a nested pattern's own reads would interleave with the
    /// iterator steps in the wrong order.
    DestructureArr(u16),
    /// Destructuring-assignment loop head: pop one iteration value and run the retained
    /// AssignmentPattern from `Chunk::assignment_targets`. This deliberately shares the
    /// tree-walker's normative Reference/GetV/default/IteratorClose implementation rather than
    /// maintaining a second subtly different algorithm. The compiler admits only patterns that
    /// cannot suspend this activation; the enclosing VM for-of handler owns any abrupt close.
    AssignTarget(u32),
    /// Object assignment-rest: pop `count` already-normalized excluded property keys and the
    /// source value, then push CopyDataProperties(source, excludedNames).
    ObjectRest(u16),
    /// `delete obj.name` (pops obj, pushes the bool result). Operands: name index and the
    /// *function's* strictness — the oracle's `self.strict` is not maintained across direct
    /// JIT→JIT call sequences, so it travels in the op.
    DeleteProp(u32, bool),
    /// `delete obj[k]` (pops k then obj, pushes the bool result; ToPropertyKey on the key
    /// before the nullish-base check, matching the oracle's order).
    DeleteElem(bool),
    /// Sloppy `delete identifier`: resolve the Environment Reference and perform DeleteBinding.
    DeleteName(u32),
    /// Throw the mandatory ReferenceError after a super-property Reference has been evaluated.
    DeleteSuper,
    /// `f(a, b, ...c)` — a spread argument in the LAST position (the only shape whose
    /// evaluate-everything-then-expand lowering matches the spec's interleaved evaluation
    /// order): pops the iterable and `argc-1` plain arguments, expands via the iterator
    /// protocol, and calls through the generic path (no call IC — spread sites are cold).
    CallSpread(u16),
    /// [`Op::CallSpread`] with a receiver beneath the callee (method calls, with-object hits).
    CallSpreadThis(u16),
    /// Call with an internally-built dense argument array. This is the general
    /// ArgumentListEvaluation path for interleaved/multiple spreads; the array never escapes.
    CallArgsArray,
    /// [`Op::CallArgsArray`] with a receiver beneath the callee.
    CallArgsArrayThis,
    /// A syntactic `eval(...)` call after its Reference and complete argument list have been
    /// evaluated in source order. If the retained receiver proves a non-property Reference and
    /// the callee is this Realm's `%eval%`, run PerformEval against the live VM environment;
    /// otherwise make the ordinary call. The arguments travel in the same private dense array as
    /// [`Op::CallArgsArrayThis`], so arbitrary suspension and spreads remain explicit bytecode.
    EvalCallArgsArray,
    /// Statement-position `obj.name += v` (pops v, the compound-read lval, obj): appends IN
    /// PLACE when the property still holds the exact string the read produced and everything is
    /// plain (see `Interp::append_prop_fast`); otherwise runs the generic Add + IC store —
    /// observably identical to the unfused GetProp/Add/SetPropDrop sequence.
    AppendProp(u32, u32),
    GetElem,
    SetElem,
    /// `obj[k] = v` in statement position: stores without leaving `v` on the stack.
    SetElemDrop,
    /// `x[k]` where `x` is a never-TDZ local slot (param or `var`): fused LoadLocal+GetElem —
    /// the receiver never crosses the operand stack (no clone/drop refcount churn). Reads the
    /// slot at exec time, which is only sound because the emitter proves the key expression
    /// cannot reassign the base local (see `Compiler::fused_elem_slot`) and the slot can never
    /// TDZ-throw.
    GetElemLocal(u16),
    /// `x[k] = v` with `x` a never-TDZ local slot (see [`Op::GetElemLocal`]), keeping `v`.
    SetElemLocal(u16),
    /// `x[k] = v` in statement position with `x` a never-TDZ local slot.
    SetElemLocalDrop(u16),
    /// `obj.name++` / `--obj.name` as one op: pops obj, reads via the site IC, ToNumeric, ±1,
    /// writes back via the IC, pushes old / new / nothing per `UpdKind`.
    UpdateProp(u32, u32, UpdKind),
    /// `obj[k]++` / `--obj[k]`: pops k and obj, coerces the key at most once (matching the
    /// oracle's cached-Reference semantics), read-modify-write, pushes per `UpdKind`.
    UpdateElem(UpdKind),
    /// Compound `obj[k] op= v` support: coerce the top of stack to a property key *now* when the
    /// coercion could be observable (an object's valueOf/toString), so the following GetElem +
    /// SetElem pair can't run it twice. Num/Str keys stay raw — their later coercion is
    /// side-effect-free and deterministic, and keeping numbers numeric preserves the dense-array
    /// fast path. Checks the base (one below top) for null/undefined first, like `ref_prop_key`.
    ToPropKey,
    /// [`Op::ToPropKey`] for the slot-fused compound form: the base is read from the local slot
    /// (never on the stack), keys already Num/Str pass through untouched.
    ToPropKeyLocal(u16),
    /// Duplicate the top two stack values (for compound `obj[k] op= v`).
    Dup2,
    /// `obj.name` as a call target: pops obj, pushes obj then the method (get runs before args).
    /// Operands: name index, inline-cache index (methods live on prototypes — the IC walks hops).
    GetMethod(u32, u32),
    /// `obj[k]` as a call target: pops k and obj, pushes obj then the method.
    GetMethodElem,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    UShr,
    Lt,
    Gt,
    Le,
    Ge,
    EqEq,
    NotEq,
    StrictEq,
    StrictNotEq,
    /// `lhs instanceof rhs`, with one shape cache for the overwhelmingly common ordinary
    /// constructor case. The JIT additionally validates the live constructor/prototype chain;
    /// misses retain the complete `@@hasInstance` semantics through the generic executor.
    InstanceOf(u32),
    /// Any other binary operator (`**`, `in`, `instanceof`) via the interpreter, op in names.
    GenBin(u32),
    Neg,
    Plus,
    Not,
    BitNot,
    Typeof,
    /// `typeof freeName`: absent bindings yield "undefined", while lexical TDZ still throws.
    TypeofName(u32),
    Void,
    Jump(u32),
    /// A `break`/`continue` Completion crossing a `finally`: unwind handlers down to the second
    /// operand, running every intervening finalizer, then continue at the first operand. The
    /// destination is patched with the loop target just like [`Op::Jump`].
    AbruptJump(u32, u32),
    JumpIfFalse(u32),
    /// Peek variants leave the operand on the stack (for `&&` / `||` / `??`).
    JumpIfFalsePeek(u32),
    JumpIfTruePeek(u32),
    JumpIfNotNullishPeek(u32),
    /// Plain call: pops argc args and the callee; `this` is undefined. Second operand = the
    /// per-site [`CallIc`] index.
    Call(u16, u32),
    /// Resolve a free name as a call target *before* the arguments evaluate (spec order):
    /// pushes the `with`-object `this` (or undefined) then the callee, feeding CallWithThis.
    /// Operands: name index, per-site [`NameIc`] index.
    LoadNameForCall(u32, u32),
    /// Method call: pops argc args, the method, and the receiver pushed by GetMethod*. Second
    /// operand = the per-site [`CallIc`] index.
    CallWithThis(u16, u32),
    /// Speculative-inline guard (see [`plan_inlines`]): with the stack holding
    /// `[this, callee, arg0..argN]` for the call site, verify the callee is the pinned expected
    /// function (and, for a sloppy this-using callee, that the receiver is already an object);
    /// on mismatch jump to the operand (the generic call op). Operands: `Chunk::inline_targets`
    /// index, jump target.
    InlineGuard(u32, u32),
    /// Reset `count` slots starting at `start` to undefined (dropping old values): a spliced
    /// callee's hoisted vars start fresh on every pass through the site.
    ResetSlots(u16, u16),
    New(u16, u32),
    /// Construct with an internally-built dense argument array (the general spread path).
    NewArgsArray,
    /// A regexp literal: source and flags indices in `names`. Each execution allocates a fresh
    /// JS RegExp object while `Interp::regexp_programs` shares the immutable compiled matcher.
    MakeRegExp(u32, u32),
    MakeArray(u16),
    /// Incremental ArrayLiteral construction used when holes/spreads or suspending elements make
    /// one batched `MakeArray` impossible. Every op retains the fresh array on the stack.
    NewArray,
    ArrayPush,
    ArrayHole,
    ArraySpread,
    /// Object literal: `count` plain data keys starting at names[start], values on the stack.
    MakeObject(u32, u16, u32),
    /// Incremental ObjectLiteral construction. Data's bool requests runtime NamedEvaluation;
    /// Method's kind is 0/method, 1/getter, or 2/setter.
    NewObject,
    ObjectData(bool),
    ObjectSpread,
    ObjectProto,
    ObjectMethod(u32, u8),
    /// Meta/import/private-name expression forms. DynamicImport's bool says whether an options
    /// operand is present; its phase is the parsed proposal/core import phase.
    ImportMeta,
    NewTarget,
    DynamicImport(ImportPhase, bool),
    PrivateIn(u32),
    GetPrivate(u32),
    GetPrivateKeep(u32),
    GetPrivateMethod(u32),
    SetPrivate(u32),
    UpdatePrivate(u32, UpdKind),
    /// SuperCall steps 1–3: push the lexically inherited new.target and live superclass before
    /// ArgumentListEvaluation. Both values stay on the continuation stack across suspension.
    SuperCallStart,
    /// Complete SuperCall from `[newTarget, superConstructor, privateArgsArray]`: Construct,
    /// BindThisValue on the retained derived-constructor environment, initialize instance
    /// elements, and push the constructed object.
    SuperCallArgsArray,
    /// Split SuperProperty reference construction. The compiler emits SuperThis, then the key,
    /// then SuperBase, matching GetThisBinding → computed key → GetSuperBase ordering.
    SuperThis,
    SuperBase,
    SuperGet,
    SuperGetKeep,
    SuperGetMethod,
    SuperSet,
    SuperUpdate(UpdKind),
    /// Push the realm-cached frozen template object for `Chunk::templates[site]`.
    TemplateObject(u32),
    /// Tagged templates must reject a non-callable tag before evaluating substitutions.
    RequireCallable,
    /// Retained uncommon expression evaluated through the normative tree-walker against a
    /// projected view of this VM frame. Direct yield/await never enters this bridge.
    EvalExpr(u32),
    /// Staged ECMA-262 ClassDefinitionEvaluation for a heritage or computed name which suspends.
    /// The plan index also selects one continuation-local in-progress state slot; the count
    /// consumes proposal class-decorator (receiver, callback) records evaluated before the class
    /// environment opens.
    ClassStart(u32, u16),
    /// Consume the evaluated heritage (the bool says it was syntactically present), validate it,
    /// perform the observable `prototype` read, and activate the retained private environment.
    ClassHeritage(u32, bool),
    /// Consume and ToPropertyKey one computed member name in source order.
    ClassKey(u32, u16),
    /// Consume one proposal member-decorator (receiver, callback) record in source order for later
    /// reverse application. The member index selects its continuation-owned value list.
    ClassDecorator(u32, u16),
    /// Finish the non-suspending class body and push its constructor value.
    ClassFinish(u32),
    /// Abandon a partially evaluated class on any abrupt completion and restore the outer env.
    ClassAbort(u32),
    /// Pop a value, perform ToObject, and install a `with` Object Environment Record above the
    /// continuation's current lexical environment. `PopEnv` restores its parent on every normal
    /// or abrupt exit through the compiler's completion-aware cleanup pads.
    PushWith,
    /// Install a declarative Environment Record for the captured subset of one lexical scope.
    /// Uncaptured bindings in the same source scope remain VM slots; `InitLex` performs the
    /// declaration's InitializeBinding against this record.
    PushLex(u32),
    /// [`Op::PushLex`] for a CatchClause parameter environment. The distinct marker implements
    /// the Annex B.3.4 EvalDeclarationInstantiation exemption for `catch (e)`.
    PushCatchLex(u32),
    /// CreatePerIterationEnvironment for the captured subset of a classic `for (let ...)` head:
    /// replace the current lexical record with a fresh sibling and copy its live binding values.
    CloneLex(u32),
    InitLex(u32),
    PopEnv,
    /// Open a continuation-owned DisposableResource stack. A following `PushFinally` protects
    /// the corresponding statement list; keeping the records on the VM frame avoids sharing the
    /// tree-walker's ambient stack between independently suspended coroutines.
    PushDisposeFrame,
    /// Capture the value currently on top of the operand stack as a sync/async disposable
    /// resource without consuming it (BindingInitialization follows only after this succeeds).
    AddDisposable(bool),
    /// Dispose the innermost frame for a normal statement-list completion.
    DisposeNormal,
    /// Dispose while preserving/replacing the completion represented by the operand/pad kind.
    DisposeThrow,
    DisposeReturn,
    DisposeBareReturn,
    DisposeResumeReturn,
    DisposeJump,
    Throw,
    Return,
    /// An explicit source `return;`. Unlike fallthrough it is an abrupt Completion that unwinds
    /// finalizers/iterators; unlike `return expression` in an async generator, it performs no
    /// Await before the generator completes.
    ReturnBare,
    /// Re-issue an externally injected generator return after a finalizer. Unlike a source
    /// ReturnStatement in an async generator, its value has already been awaited.
    ResumeReturn,
    /// Re-issue a saved `break`/`continue` Completion after a finalizer. Pops the destination and
    /// target handler depth, which the finalizer pads preserve in hidden local slots.
    ResumeJump,
    ReturnUndef,
    /// `await expr`: suspend the async body, handing the popped operand to the driver; on resume the
    /// settled value is pushed back (or a rejection is thrown). Only emitted for async functions.
    Await,
    /// `yield expr`: suspend a synchronous generator and hand the popped operand to its iterator
    /// driver. A later `next(value)` pushes `value` as the yield expression's result.
    Yield,
    /// `yield* expr`: initialize and drive a delegated iterator. The heap-owned [`VmCoro`]
    /// retains the iterator record and the received normal/throw/return completion across every
    /// suspension (ECMA-262 YieldExpression runtime semantics).
    YieldStar,
    /// Enter a `try` region: register a handler that, on a throw anywhere in the region, unwinds the
    /// stack and jumps to the operand (the catch pc, with the exception pushed).
    PushHandler(u32),
    /// Enter a `try`/`finally` region. Its pads preserve throw, expression-return, bare-return,
    /// already-awaited return, and break/continue Completions across finalizer suspension.
    PushFinally(u32, u32, u32, u32, u32),
    /// Enter one `for…of` body iteration. Throw, source return, bare return, and externally
    /// resumed return use separate close pads so IteratorClose observes the exact Completion.
    PushIterator(u32, u32, u32, u32),
    /// Leave a protected try/iteration region normally: drop the innermost handler.
    PopHandler,
}

/// An active `try` region on the VM's handler stack.
struct Handler {
    target: HandlerTarget,
    /// The operand-stack depth to restore before entering a completion pad.
    stack_depth: usize,
}

enum HandlerTarget {
    Catch {
        throw_pc: usize,
    },
    Finally {
        throw_pc: usize,
        return_pc: usize,
        bare_return_pc: usize,
        resume_return_pc: usize,
        jump_pc: usize,
    },
    Iterator {
        throw_pc: usize,
        return_pc: usize,
        bare_return_pc: usize,
        resume_return_pc: usize,
    },
}

enum PendingCompletion {
    Throw(Value),
    ResumeReturn(Value),
    SourceReturn(Value),
    BareReturn,
    Jump { target: usize, handler_depth: usize },
}

/// How one captured binding seeds into the activation environment at entry (in order).
pub(crate) enum CapInit {
    /// A captured parameter: seed from argument `k`.
    Param(u16, Rc<str>),
    /// A captured function-scoped `var`: undefined, unless already bound (a same-named param).
    Var(Rc<str>),
    /// A hoisted function declaration: a closure over the activation itself (self-recursion).
    Fn(u16, Rc<str>),
    /// A captured top-level lexical: inserted uninitialized (TDZ); bool = `const`.
    Lexical(Rc<str>, bool),
}

struct LexicalBinding {
    name: Rc<str>,
    is_const: bool,
}

pub struct Chunk {
    // (fields below; Debug is manual — `consts` holds engine Values)
    ops: Vec<Op>,
    consts: Vec<Value>,
    names: Vec<Rc<str>>,
    n_slots: usize,
    /// Slot names, for TDZ ReferenceError messages.
    slot_names: Vec<Rc<str>>,
    /// Number of opaque prepared-Reference homes needed by this chunk.
    n_refs: usize,
    /// Parameter positions map onto slots [0, n_params).
    n_params: usize,
    /// Slot holding the ordinary function's materialized `arguments` object. Only compiled for
    /// parameterless synchronous functions for now, avoiding mapped-parameter aliasing while
    /// covering common variadic helpers.
    arguments_slot: Option<u16>,
    uses_this: bool,
    /// Arrow functions have lexical `this`; their bytecode reads the defining Environment Record
    /// rather than an OrdinaryCallBindThis frame value.
    lexical_this: bool,
    /// The source function's strictness. Heap continuations resume outside their original call
    /// stack, so operations whose Reference semantics depend on strict mode restore this value
    /// for every VM slice.
    strict: bool,
    /// Distinct `this.name = …` stores before the body's first control-flow split, capped small.
    /// `new` uses this to reserve the instance property vector exactly once; it costs no bytes
    /// per object and avoids retaining geometric-growth slack.
    instance_capacity_hint: u8,
    /// Field count learned from a structurally proven `initialize.apply(this, arguments)`
    /// forwarder. Once set it is as stable as the immutable forwarding/initializer chunks.
    forwarded_capacity_hint: std::cell::Cell<u8>,
    /// Inner function templates for `MakeClosure`.
    funcs: Vec<Rc<Function>>,
    /// Tagged-template sites retained by the chunk. Each entry carries the Parse Node identity
    /// used by the Realm's GetTemplateObject cache.
    templates: Vec<(u64, Vec<(Option<String>, String)>)>,
    eval_exprs: Vec<EvalExprPlan>,
    class_plans: Vec<ClassPlan>,
    /// AssignmentPattern expression trees retained only for generic [`Op::AssignTarget`] sites.
    /// These sites replace whole-function/native-coroutine fallback while uncommon pattern work
    /// stays off the hot scalar bytecode path.
    assignment_targets: Vec<AssignmentTargetPlan>,
    /// Captured subsets of block/loop lexical scopes. These allocate only when closure identity is
    /// observable; ordinary lexical bindings stay in the allocation-free slot representation.
    lexical_scopes: Vec<Vec<LexicalBinding>>,
    /// Captured bindings to seed into a fresh activation env at entry; empty = no activation
    /// needed (closures, if any, capture the definition env directly).
    cap_inits: Vec<CapInit>,
    /// Coroutine FunctionDeclarationInstantiation already made the activation that owns mapped
    /// parameters and their arguments object. Seed additional compiled bindings into that exact
    /// environment so both access paths share storage.
    reuse_activation: bool,
    /// An inner arrow chain reads the outer `this`: the activation carries a `this` binding.
    env_this: bool,
    /// A parameterless synchronous function whose `arguments` is captured by an inner arrow.
    /// The object is materialized once into the activation rather than a frame-only slot.
    env_arguments: bool,
    /// One inline-cache slot per property-access op (`GetProp`/`SetProp`/`SetPropDrop`/`GetMethod`),
    /// holding the (prototype depth, `entries` slot) last seen for that site (see [`IcState`]). The
    /// `Chunk` is shared across calls via `Rc`, so these persist. `Cell` is fine: the VM runs one
    /// thread at a time (coroutine ping-pong), like the rest of the engine's shared-`Rc` state.
    caches: Vec<std::cell::Cell<IcState>>,
    /// Representation-independent semantic site layout. The detailed payload is lazy and does
    /// not mirror raw IC words; a second-stage compile reuses the canonical baseline layout.
    feedback: crate::feedback::FeedbackVector,
    /// Runtime-only current-shape adapter table. Index+1 is the abstract layout token stored in
    /// feedback words; raw Agent-local shape numbers never cross the profile boundary.
    feedback_shapes: std::cell::RefCell<Vec<u32>>,
    /// One pre-shaped `Props` template per plain object-literal site (`Op::MakeObject`'s third
    /// operand indexes this; `u32::MAX` = duplicate keys, take the insert path). Built on first
    /// execution, cloned per instance — key hashing and shape transitions paid once per SITE.
    obj_maps: Vec<std::cell::OnceCell<crate::value::Props>>,
    /// One [`NameIc`] slot per free-name op (`LoadName`/`LoadNameForCall`), persisting across
    /// calls like `caches`.
    name_caches: Vec<std::cell::Cell<NameIc>>,
    /// Weak handles pinning each name cache's scope allocation (parallel to `name_caches`), so
    /// the cached raw `env` pointer can never be recycled into a different scope while cached.
    name_pins: std::cell::RefCell<
        Vec<Option<std::rc::Weak<std::cell::RefCell<crate::interpreter::Scope>>>>,
    >,
    /// Numeric value observed when each name cache was filled. The JIT compares the live value
    /// before using it, so ordinary assignment needs no invalidation. Split validity/bits keeps
    /// this at nine bytes per site rather than padding a feedback struct to sixteen.
    name_num_bits: Vec<std::cell::Cell<u64>>,
    name_num_valid: Vec<std::cell::Cell<bool>>,
    /// Captured-binding cache keyed by the chunk's interned-name index. Unlike a free-name cache,
    /// this always resolves in the current activation; a weak pin keeps its raw scope pointer
    /// ABA-safe until a fresh/recursive activation refills the entry.
    cap_caches: Vec<std::cell::Cell<NameIc>>,
    cap_pins: std::cell::RefCell<
        Vec<Option<std::rc::Weak<std::cell::RefCell<crate::interpreter::Scope>>>>,
    >,
    /// Parsed straight-line initializer body (`this.x = arg` with optional truthy defaults).
    initializer_plan: std::cell::OnceCell<Option<InitializerPlan>>,
    /// Complete creation-shape chain for an exact straight-line constructor, keyed by the live
    /// prototype epoch/identity and fresh receiver shape.
    simple_constructor_shapes: std::cell::Cell<InitializerShapes>,
    /// Parsed exact straight-line constructor fields; immutable bytecode makes this a one-time
    /// structural decision rather than work repeated by every `new`.
    simple_constructor_plan: std::cell::OnceCell<Option<Vec<InitializerField>>>,
    /// Parsed `this.initialize.apply(this, arguments)` forwarding body.
    arguments_forwarder: std::cell::OnceCell<Option<ArgumentsForwarder>>,
    /// Immutable callable metadata behind a warmed forwarding constructor. Observable
    /// properties are still IC-validated on every construction before this cache is used.
    arguments_forwarder_runtime: std::cell::RefCell<Option<ForwarderRuntime>>,
    /// Compiled matcher per RegExp-literal bytecode pc. Literal evaluation still allocates a
    /// fresh JS wrapper and `lastIndex`; only immutable source/flags compilation is shared.
    regexp_literals: Vec<std::cell::OnceCell<Rc<crate::regex::Regex>>>,
    /// One [`CallSite`] per `Call`/`CallWithThis` site (the JIT→JIT fast call's callee cache).
    call_caches: Vec<CallSite>,
    /// One monomorphic identity cache per `New` site.
    construct_caches: Vec<std::cell::Cell<ConstructSite>>,
    /// Weak handles pinning every callee address a call cache has ever recorded (see [`CallIc`]),
    /// keyed by that address — one pin per distinct callee no matter how often sites refill, so a
    /// megamorphic site can't exhaust the budget for the whole chunk.
    call_pins: std::cell::RefCell<
        crate::fasthash::FastMap<usize, std::rc::Weak<std::cell::RefCell<crate::value::Object>>>,
    >,
    /// Guard data for speculatively inlined call sites (`Op::InlineGuard` indexes this).
    inline_targets: Vec<InlineTarget>,
    /// Machine-code runs of this chunk (the [`plan_inlines`] trigger counts these).
    pub(crate) jit_runs: std::cell::Cell<u32>,
    /// Whether the one-shot inline recompile has been attempted for this chunk.
    pub(crate) inline_attempted: std::cell::Cell<bool>,
    /// Machine-code tier state: the compile result once attempted (`None` inside = the chunk
    /// cannot JIT — async, or an unsupported platform — and runs on the bytecode VM forever).
    pub(crate) jit: std::cell::OnceCell<Option<Rc<crate::jit::JitCode>>>,
}

impl Chunk {
    /// Scan the directly-owned bytecode/feedback payload. Shared AST nodes, Functions, strings,
    /// properties, RegExp programs, chunks, and JIT sidecars route back through the one
    /// allocation-family visitor for identity deduplication.
    pub(crate) fn scan_retained_memory(&self, visitor: &mut crate::memory::Visitor) {
        refresh_current_layout_feedback(&self.feedback, &self.feedback_shapes, &self.caches);
        macro_rules! vec_bytes {
            ($field:ident, $ty:ty) => {
                self.$field
                    .capacity()
                    .saturating_mul(std::mem::size_of::<$ty>())
            };
        }
        let mut bytes = std::mem::size_of::<Chunk>()
            .saturating_add(vec_bytes!(ops, Op))
            .saturating_add(vec_bytes!(consts, Value))
            .saturating_add(vec_bytes!(names, Rc<str>))
            .saturating_add(vec_bytes!(slot_names, Rc<str>))
            .saturating_add(vec_bytes!(funcs, Rc<Function>))
            .saturating_add(vec_bytes!(templates, (u64, Vec<(Option<String>, String)>)))
            .saturating_add(vec_bytes!(eval_exprs, EvalExprPlan))
            .saturating_add(vec_bytes!(class_plans, ClassPlan))
            .saturating_add(vec_bytes!(assignment_targets, AssignmentTargetPlan))
            .saturating_add(vec_bytes!(lexical_scopes, Vec<LexicalBinding>))
            .saturating_add(vec_bytes!(cap_inits, CapInit))
            .saturating_add(vec_bytes!(caches, std::cell::Cell<IcState>))
            .saturating_add(vec_bytes!(
                obj_maps,
                std::cell::OnceCell<crate::value::Props>
            ))
            .saturating_add(vec_bytes!(name_caches, std::cell::Cell<NameIc>))
            .saturating_add(vec_bytes!(name_num_bits, std::cell::Cell<u64>))
            .saturating_add(vec_bytes!(name_num_valid, std::cell::Cell<bool>))
            .saturating_add(vec_bytes!(cap_caches, std::cell::Cell<NameIc>))
            .saturating_add(vec_bytes!(
                regexp_literals,
                std::cell::OnceCell<Rc<crate::regex::Regex>>
            ))
            .saturating_add(vec_bytes!(call_caches, CallSite))
            .saturating_add(vec_bytes!(construct_caches, std::cell::Cell<ConstructSite>))
            .saturating_add(vec_bytes!(inline_targets, InlineTarget));
        bytes = bytes.saturating_add(self.feedback.retained_bytes());
        bytes = bytes.saturating_add(
            self.feedback_shapes
                .borrow()
                .capacity()
                .saturating_mul(std::mem::size_of::<u32>()),
        );

        for value in &self.consts {
            visitor.value(value);
        }
        for name in self.names.iter().chain(&self.slot_names) {
            visitor.rc_str(name);
        }
        for function in &self.funcs {
            visitor.function(function);
        }
        for (_, parts) in &self.templates {
            bytes = bytes.saturating_add(
                parts
                    .capacity()
                    .saturating_mul(std::mem::size_of::<(Option<String>, String)>()),
            );
            for (cooked, raw) in parts {
                bytes = bytes.saturating_add(raw.capacity());
                if let Some(cooked) = cooked {
                    bytes = bytes.saturating_add(cooked.capacity());
                }
            }
        }
        for plan in &self.eval_exprs {
            bytes =
                bytes.saturating_add(crate::ast::scan_expr_retained_memory(&plan.expr, visitor));
            bytes = bytes.saturating_add(
                plan.locals
                    .capacity()
                    .saturating_mul(std::mem::size_of::<AssignmentLocal>()),
            );
            bytes = bytes.saturating_add(plan.name.as_ref().map_or(0, String::capacity));
            for local in &plan.locals {
                visitor.rc_str(&local.name);
            }
        }
        for plan in &self.assignment_targets {
            bytes =
                bytes.saturating_add(crate::ast::scan_expr_retained_memory(&plan.target, visitor));
            bytes = bytes.saturating_add(
                plan.locals
                    .capacity()
                    .saturating_mul(std::mem::size_of::<AssignmentLocal>()),
            );
            for local in &plan.locals {
                visitor.rc_str(&local.name);
            }
        }
        for plan in &self.class_plans {
            visitor.class(&plan.class);
            bytes = bytes.saturating_add(plan.inferred_name.as_ref().map_or(0, String::capacity));
        }
        for scope in &self.lexical_scopes {
            bytes = bytes.saturating_add(
                scope
                    .capacity()
                    .saturating_mul(std::mem::size_of::<LexicalBinding>()),
            );
            for binding in scope {
                visitor.rc_str(&binding.name);
            }
        }
        for init in &self.cap_inits {
            let name = match init {
                CapInit::Param(_, name)
                | CapInit::Var(name)
                | CapInit::Fn(_, name)
                | CapInit::Lexical(name, _) => name,
            };
            visitor.rc_str(name);
        }
        for map in &self.obj_maps {
            if let Some(map) = map.get() {
                visitor.props(map);
            }
        }
        bytes =
            bytes
                .saturating_add(self.name_pins.borrow().capacity().saturating_mul(
                    std::mem::size_of::<
                        Option<std::rc::Weak<std::cell::RefCell<crate::interpreter::Scope>>>,
                    >(),
                ))
                .saturating_add(self.cap_pins.borrow().capacity().saturating_mul(
                    std::mem::size_of::<
                        Option<std::rc::Weak<std::cell::RefCell<crate::interpreter::Scope>>>,
                    >(),
                ));
        if let Some(Some(plan)) = self.initializer_plan.get() {
            bytes = bytes.saturating_add(
                plan.fields
                    .capacity()
                    .saturating_mul(std::mem::size_of::<InitializerField>()),
            );
        }
        if let Some(Some(fields)) = self.simple_constructor_plan.get() {
            bytes = bytes.saturating_add(
                fields
                    .capacity()
                    .saturating_mul(std::mem::size_of::<InitializerField>()),
            );
        }
        if let Some(runtime) = self.arguments_forwarder_runtime.borrow().as_ref() {
            visitor.chunk(&runtime.chunk);
        }
        for regex in &self.regexp_literals {
            if let Some(regex) = regex.get() {
                visitor.regex(regex);
            }
        }
        let call_pins = self.call_pins.borrow();
        bytes = bytes.saturating_add(call_pins.len().saturating_mul(std::mem::size_of::<(
            usize,
            std::rc::Weak<std::cell::RefCell<crate::value::Object>>,
        )>()));
        if call_pins.capacity() != 0 {
            visitor.mark_function_bytecode_opaque_storage();
        }
        if let Some(code) = self.jit.get().and_then(Option::as_ref) {
            visitor.jit_code(code);
        }
        visitor.add_function_bytecode_bytes(bytes);
    }
}

/// Borrowed description of a constructor whose complete body is a sequence of unique
/// parameter-to-`this` stores followed by implicit return.
pub(crate) struct SimpleConstructor<'a> {
    chunk: &'a Chunk,
    fields: &'a [InitializerField],
}

/// One visible slot-backed binding projected into the temporary Environment Record used by a
/// generic destructuring-assignment operation. Captured bindings already live in `env` and are
/// intentionally absent; free names continue resolving through the plan environment's parent.
struct AssignmentLocal {
    name: Rc<str>,
    slot: u16,
    mutable: bool,
}

struct AssignmentTargetPlan {
    target: Expr,
    locals: Vec<AssignmentLocal>,
    strict: bool,
}

struct EvalExprPlan {
    expr: Expr,
    locals: Vec<AssignmentLocal>,
    strict: bool,
    name: Option<String>,
}

struct ClassPlan {
    class: Rc<Class>,
    inferred_name: Option<String>,
}

pub(crate) struct InitializerPlan {
    fields: Vec<InitializerField>,
    shapes: std::cell::Cell<InitializerShapes>,
}

struct InitializerField {
    slot: u16,
    default: Option<u32>,
    name: u32,
    cache: u32,
}

#[derive(Clone, Copy)]
struct ArgumentsForwarder {
    initializer: u32,
    initializer_cache: u32,
    apply_cache: u32,
}

struct ForwarderRuntime {
    initializer: usize,
    apply: usize,
    pin: std::rc::Weak<std::cell::RefCell<crate::value::Object>>,
    chunk: Rc<Chunk>,
}

#[derive(Clone, Copy, Default)]
struct InitializerShapes {
    epoch: u32,
    proto: usize,
    start: u32,
    len: u8,
    transitions: [u32; 16],
}

impl InitializerPlan {
    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        self.fields.len()
    }

    pub(crate) fn cached_shapes(&self, epoch: u32, proto: usize, start: u32) -> Option<[u32; 16]> {
        let cached = self.shapes.get();
        (cached.epoch == epoch
            && cached.proto == proto
            && cached.start == start
            && cached.len as usize == self.fields.len())
        .then_some(cached.transitions)
    }

    pub(crate) fn cache_shapes(&self, epoch: u32, proto: usize, start: u32, values: &[u32]) {
        debug_assert!(values.len() <= 16);
        let mut transitions = [0; 16];
        transitions[..values.len()].copy_from_slice(values);
        self.shapes.set(InitializerShapes {
            epoch,
            proto,
            start,
            len: values.len() as u8,
            transitions,
        });
    }
}

impl SimpleConstructor<'_> {
    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        self.fields.len()
    }

    #[inline(always)]
    pub(crate) fn field(&self, index: usize) -> (usize, &Rc<str>, &std::cell::Cell<IcState>) {
        let field = &self.fields[index];
        (
            field.slot as usize,
            &self.chunk.names[field.name as usize],
            &self.chunk.caches[field.cache as usize],
        )
    }

    pub(crate) fn cached_shapes(&self, epoch: u32, proto: usize, start: u32) -> Option<[u32; 16]> {
        let cached = self.chunk.simple_constructor_shapes.get();
        (cached.epoch == epoch
            && cached.proto == proto
            && cached.start == start
            && cached.len as usize == self.fields.len())
        .then_some(cached.transitions)
    }

    pub(crate) fn cache_shapes(&self, epoch: u32, proto: usize, start: u32, values: &[u32]) {
        debug_assert!(values.len() <= 16);
        let mut transitions = [0; 16];
        transitions[..values.len()].copy_from_slice(values);
        self.chunk.simple_constructor_shapes.set(InitializerShapes {
            epoch,
            proto,
            start,
            len: values.len() as u8,
            transitions,
        });
    }
}

impl Chunk {
    pub(crate) fn compiled_regexp_literal(
        &self,
        i: &mut Interp,
        pc: usize,
        body: u32,
        flags: u32,
    ) -> Result<Rc<crate::regex::Regex>, Abrupt> {
        if let Some(re) = self.regexp_literals[pc].get() {
            return Ok(re.clone());
        }
        let re = i.compiled_regexp(&self.names[body as usize], &self.names[flags as usize])?;
        let _ = self.regexp_literals[pc].set(re.clone());
        Ok(re)
    }

    fn make_regexp_literal(
        &self,
        i: &mut Interp,
        pc: usize,
        body: u32,
        flags: u32,
    ) -> Result<Value, Abrupt> {
        let re = self.compiled_regexp_literal(i, pc, body, flags)?;
        i.make_regexp_compiled(re)
    }

    #[inline]
    fn record_name_number(&self, cache: usize, value: &Value) {
        if let Value::Num(n) = value {
            if !n.is_nan() {
                self.name_num_bits[cache].set(n.to_bits());
                self.name_num_valid[cache].set(true);
                return;
            }
        }
        self.name_num_valid[cache].set(false);
    }

    /// Whether the body (or an inner arrow chain) reads `this`, so the caller must bind it.
    pub fn uses_this(&self) -> bool {
        self.uses_this || self.env_this
    }

    /// Whether the caller must bind or eagerly resolve a frame receiver. Arrow functions retain
    /// their defining environment and read its `this` binding only when the expression executes.
    pub(crate) fn needs_frame_this(&self) -> bool {
        self.uses_this() && !self.lexical_this
    }

    pub(crate) fn instance_capacity_hint(&self) -> usize {
        self.instance_capacity_hint
            .max(self.forwarded_capacity_hint.get()) as usize
    }
    pub(crate) fn note_forwarded_capacity(&self, capacity: usize) {
        self.forwarded_capacity_hint
            .set(self.forwarded_capacity_hint.get().max(capacity as u8));
    }
    pub(crate) fn construct_cache(&self, cache: u32) -> ConstructSite {
        self.construct_caches[cache as usize].get()
    }
    pub(crate) fn fill_construct_cache(
        &self,
        cache: u32,
        entry: ConstructSite,
        constructor: &crate::value::Gc,
    ) {
        self.construct_caches[cache as usize].set(entry);
        self.call_pins
            .borrow_mut()
            .entry(entry.call.callee)
            .or_insert_with(|| Rc::downgrade(constructor));
    }
    /// Whether calls need a real activation environment (captured locals / lexical `this`).
    fn makes_env(&self) -> bool {
        !self.cap_inits.is_empty() || (self.env_this && !self.lexical_this) || self.env_arguments
    }

    /// Whether the JIT-to-JIT moved-frame path must stand down. An `arguments` object observes
    /// the call frame even though it does not itself require an activation scope.
    fn needs_env(&self) -> bool {
        self.makes_env() || self.arguments_slot.is_some()
    }

    /// Stable adapter query for call feedback. The optimizing tier must not infer this from the
    /// presence of a Rust `Env`; the bytecode's activation predicate includes arguments objects
    /// and lexical-`this` requirements that are observable at the language level.
    pub(crate) fn requires_activation_environment(&self) -> bool {
        self.needs_env()
    }

    /// Build the activation environment for one call: normally a fresh scope under `env` holding
    /// exactly the captured bindings (and `this` when an inner arrow chain reads it). A coroutine
    /// with mapped arguments instead reuses the FunctionDeclarationInstantiation environment so
    /// parameter homes and the arguments ParameterMap address the same bindings. Free names and
    /// `MakeClosure` environments route through the result. Returns `env` untouched when nothing
    /// needs seeding.
    fn make_run_env(&self, i: &mut Interp, env: &Env, this_val: &Value, args: &[Value]) -> Env {
        if !self.makes_env() {
            return env.clone();
        }
        let act = if self.reuse_activation {
            env.clone()
        } else {
            crate::interpreter::new_var_scope_with_capacity(
                Some(env.clone()),
                self.cap_inits.len()
                    + usize::from(self.env_this && !self.lexical_this)
                    + usize::from(self.env_arguments),
            )
        };
        // Function-declaration closures capture the activation itself, so they are created after
        // the borrow below is released.
        let mut fns: Vec<(u16, Rc<str>)> = Vec::new();
        {
            let mut b = act.borrow_mut();
            for ci in &self.cap_inits {
                match ci {
                    CapInit::Param(k, name) => {
                        b.vars.insert(
                            name.clone(),
                            crate::interpreter::Binding {
                                value: args.get(*k as usize).cloned().unwrap_or(Value::Undefined),
                                mutable: true,
                                strict_immutable: false,
                                initialized: true,
                                import_ref: None,
                                deletable: false,
                            },
                        );
                    }
                    CapInit::Var(name) => {
                        if !b.vars.contains_key(name) {
                            b.vars.insert(
                                name.clone(),
                                crate::interpreter::Binding {
                                    value: Value::Undefined,
                                    mutable: true,
                                    strict_immutable: false,
                                    initialized: true,
                                    import_ref: None,
                                    deletable: false,
                                },
                            );
                        }
                    }
                    CapInit::Fn(fidx, name) => fns.push((*fidx, name.clone())),
                    CapInit::Lexical(name, is_const) => {
                        // Non-strict FunctionDeclarationInstantiation keeps body lexicals visible
                        // to EvalDeclarationInstantiation's var-conflict walk. The compact Scope
                        // stores both binding kinds together, so preserve that distinction here.
                        b.lexical_names.push(name.to_string());
                        b.vars.insert(
                            name.clone(),
                            crate::interpreter::Binding {
                                value: Value::Undefined,
                                mutable: !is_const,
                                strict_immutable: *is_const,
                                initialized: false,
                                import_ref: None,
                                deletable: false,
                            },
                        );
                    }
                }
            }
            if self.env_this && !self.lexical_this {
                b.vars.insert(
                    "this",
                    crate::interpreter::Binding {
                        value: this_val.clone(),
                        mutable: false,
                        strict_immutable: true,
                        initialized: true,
                        import_ref: None,
                        deletable: false,
                    },
                );
            }
        }
        for (fidx, name) in fns {
            let v = i.make_function(self.funcs[fidx as usize].clone(), act.clone());
            act.borrow_mut().vars.insert(
                name.to_string(),
                crate::interpreter::Binding {
                    value: v,
                    mutable: true,
                    strict_immutable: false,
                    initialized: true,
                    import_ref: None,
                    deletable: false,
                },
            );
        }
        if self.env_arguments {
            let arguments = Value::Obj(i.make_compiled_arguments_object(args, &act));
            act.borrow_mut().vars.insert(
                "arguments".to_string(),
                crate::interpreter::Binding::data(arguments, true, true),
            );
        }
        act
    }
}

impl std::fmt::Debug for Chunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Chunk({} ops, {} slots)", self.ops.len(), self.n_slots)
    }
}

// ---------------------------------------------------------------------------------------------
// Capture analysis
// ---------------------------------------------------------------------------------------------

/// Which names the body's *inner functions* can resolve to the outer function's locals — the set
/// that must live in a real activation environment instead of VM slots. Also whether any inner
/// arrow chain reads the outer `this`.
///
/// Soundness rule: a name wrongly treated as local to an inner function would silently resolve
/// past the activation to the wrong binding, so everything not fully understood returns `None`
/// (direct eval, `with`, sloppy block function declarations, module syntax, …) and the caller
/// bails to the tree-walker.
struct CaptureScan {
    /// Declared-name scopes, innermost last, each tagged with the function-nesting depth it
    /// belongs to (0 = the function being compiled) and its push serial.
    scopes: Vec<(std::collections::HashSet<String>, u32, u32)>,
    fn_depth: u32,
    /// Names resolving from depth > 0 to a depth-0 scope.
    captured: std::collections::HashSet<String>,
    /// Names declared by a depth-0 scope that is NOT the function's top scope (block lexicals,
    /// for-head lexicals, catch params). If one is captured, it must either qualify for safe
    /// once-per-call activation homing or have an exact resumable Environment Record candidate.
    depth0_inner_decls: std::collections::HashSet<String>,
    /// Activation-homing candidates: a `let`/`using` declared by a once-per-call depth-0 scope
    /// (a block/switch outside every loop — freshness never matters) with no ENCLOSING
    /// declaration of the same name (an enclosing slot would wrongly shadow the env binding
    /// inside the block), keyed name → declaring scope serial. Same-name declarations nested
    /// INSIDE the candidate's scope are fine (their slots shadow the env binding correctly);
    /// any other same-name declaration poisons the entry. At the end a candidate homes only
    /// if every capture of the name resolved through ITS scope (see `captured_serials`) and
    /// the name was never a free/global reference.
    candidates: std::collections::HashMap<String, (u32, bool)>,
    /// Names that ever entered (or were disqualified from) candidacy — a second same-name
    /// once-per-call `let` cannot home (both would map to ONE activation binding).
    ever_candidates: std::collections::HashSet<String>,
    /// Inner lexical declarations that can be represented by a real resumable Environment
    /// Record. The compiler currently admits statement-list blocks and lexical loop heads; the
    /// serial proves the capture and the one admitted declaration are the same binding.
    runtime_candidates: std::collections::HashMap<String, Vec<u32>>,
    /// For candidate names: the scope serials their captures resolved through.
    captured_serials: std::collections::HashMap<String, std::collections::HashSet<u32>>,
    /// Names referenced somewhere they did NOT resolve to a scope (free/global uses).
    free_refs: std::collections::HashSet<String>,
    /// Innermost loop nesting at the current walk position (for-head scopes and every scope
    /// pushed inside a loop body are never homable).
    loop_depth: u32,
    /// A block lexical declared beneath a `with` Object Environment Record cannot be flattened
    /// into the function activation: doing so would put the with object before that lexical in
    /// the environment chain. Keep such captured declarations on the conservative fallback.
    with_depth: u32,
    /// Scope-push counter (the serial stored per scope for capture attribution).
    next_serial: u32,
    /// Serial of the compiled function's own top scope. A spelling can legitimately need both
    /// this activation home and a distinct runtime block environment (notably Annex B functions).
    top_serial: Option<u32>,
    /// Whether `this` is read from an inner arrow chain rooted at the outer function.
    env_this: bool,
    /// A direct eval can dynamically name every binding in the surrounding function. Coroutine
    /// compilation retains the function activation and every exact block/catch/loop environment
    /// that can be visible at the call site.
    allow_direct_eval: bool,
    saw_direct_eval: bool,
    /// Coroutine bytecode carries a resumable lexical-environment cursor and can therefore
    /// represent `with`; ordinary bytecode functions remain conservative.
    allow_with: bool,
    saw_with: bool,
    with_requires_inner_env: bool,
    top_names: std::collections::HashSet<String>,
    /// Arrow-ness of each enclosing function on the current path (index 0 = the outer function).
    arrow_path: Vec<bool>,
}

/// Collect every binding a `Pattern` introduces.
fn pat_idents(p: &Pattern, out: &mut std::collections::HashSet<String>) {
    match p {
        Pattern::Ident(n) => {
            out.insert(n.clone());
        }
        Pattern::Array(elems) => {
            for e in elems {
                match e {
                    ArrayPatElem::Hole => {}
                    ArrayPatElem::Elem { pattern, .. } => pat_idents(pattern, out),
                    ArrayPatElem::Rest(p) => pat_idents(p, out),
                }
            }
        }
        Pattern::Object(o) => {
            for pr in &o.props {
                pat_idents(&pr.value, out);
            }
            if let Some(r) = &o.rest {
                out.insert(r.clone());
            }
        }
        Pattern::Member(_) => {}
    }
}

/// Collect the function-scoped `var` names (and direct top-level function-declaration names) of a
/// body: recurses through blocks/loops/switch/try but never into nested functions or classes.
/// `top` distinguishes direct body statements (whose FuncDecls hoist) from block-level ones.
/// Annex B promotion names are added from the shared standards-aware hoist plan in `fn_body`.
fn hoisted_vars(
    stmts: &[Stmt],
    top: bool,
    strict: bool,
    out: &mut std::collections::HashSet<String>,
) -> bool {
    for s in stmts {
        if !hoisted_vars_stmt(s, top, strict, out) {
            return false;
        }
    }
    true
}

fn hoisted_vars_stmt(
    s: &Stmt,
    top: bool,
    strict: bool,
    out: &mut std::collections::HashSet<String>,
) -> bool {
    match s {
        Stmt::ExportDecl(inner) | Stmt::ExportDefault(inner) => {
            hoisted_vars_stmt(inner, top, strict, out)
        }
        Stmt::VarDecl {
            kind: DeclKind::Var,
            decls,
        } => {
            for (p, _) in decls {
                pat_idents(p, out);
            }
            true
        }
        Stmt::FuncDecl(f) => {
            if top {
                if let Some(n) = &f.name {
                    out.insert(n.clone());
                }
            }
            // Block-level declarations are lexical in both modes. A qualifying sloppy plain
            // function gets its additional var binding from ECMA-262 Annex B.3.2 below.
            true
        }
        Stmt::Block(b) => hoisted_vars(b, false, strict, out),
        Stmt::If { cons, alt, .. } => {
            hoisted_vars_stmt(cons, false, strict, out)
                && alt
                    .as_deref()
                    .map(|a| hoisted_vars_stmt(a, false, strict, out))
                    .unwrap_or(true)
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } | Stmt::Labeled { body, .. } => {
            hoisted_vars_stmt(body, false, strict, out)
        }
        Stmt::For { init, body, .. } => {
            if let Some(ForInit::VarDecl {
                kind: DeclKind::Var,
                decls,
            }) = init.as_deref()
            {
                for (p, _) in decls {
                    pat_idents(p, out);
                }
            }
            hoisted_vars_stmt(body, false, strict, out)
        }
        Stmt::ForInOf {
            decl, left, body, ..
        } => {
            if matches!(decl, Some(DeclKind::Var)) {
                pat_idents(left, out);
            }
            hoisted_vars_stmt(body, false, strict, out)
        }
        Stmt::Try {
            block,
            handler,
            finalizer,
        } => {
            hoisted_vars(block, false, strict, out)
                && handler
                    .as_ref()
                    .map(|(_, b)| hoisted_vars(b, false, strict, out))
                    .unwrap_or(true)
                && finalizer
                    .as_ref()
                    .map(|b| hoisted_vars(b, false, strict, out))
                    .unwrap_or(true)
        }
        Stmt::Switch { cases, .. } => cases
            .iter()
            .all(|c| hoisted_vars(&c.body, false, strict, out)),
        _ => true,
    }
}

impl CaptureScan {
    /// Analyze `func`, returning function captures, lexical `this`, safely homed once-per-call
    /// bindings, runtime lexical bindings, and direct-eval observability.
    fn run(
        func: &Function,
        allow_direct_eval: bool,
    ) -> Option<(
        std::collections::HashSet<String>,
        bool,
        Vec<(String, bool)>,
        std::collections::HashSet<String>,
        bool,
    )> {
        let mut sc = CaptureScan {
            scopes: Vec::new(),
            fn_depth: 0,
            captured: Default::default(),
            depth0_inner_decls: Default::default(),
            candidates: Default::default(),
            ever_candidates: Default::default(),
            runtime_candidates: Default::default(),
            captured_serials: Default::default(),
            free_refs: Default::default(),
            loop_depth: 0,
            with_depth: 0,
            next_serial: 0,
            top_serial: None,
            env_this: false,
            allow_direct_eval,
            saw_direct_eval: false,
            allow_with: allow_direct_eval,
            saw_with: false,
            with_requires_inner_env: false,
            top_names: Default::default(),
            arrow_path: vec![func.is_arrow],
        };
        sc.fn_body(func)?;
        // ECMA-262 PerformEval starts a direct eval from the running context's LexicalEnvironment
        // and VariableEnvironment. A syntactic eval at any nested function depth may therefore
        // dynamically reference any binding in the outer function. Retain every function-wide
        // binding in the activation and every depth-0 inner binding in its exact runtime scope.
        // This is deliberately function-wide over-approximation: direct eval is rare, while
        // flattening even a once-only block into the activation would make it visible to eval
        // after that block has exited.
        if sc.saw_direct_eval {
            let top_serial = sc.top_serial?;
            for name in &sc.top_names {
                sc.captured.insert(name.clone());
                sc.captured_serials
                    .entry(name.clone())
                    .or_default()
                    .insert(top_serial);
            }
            for name in &sc.depth0_inner_decls {
                let serials = sc.runtime_candidates.get(name)?;
                sc.captured.insert(name.clone());
                sc.captured_serials
                    .entry(name.clone())
                    .or_default()
                    .extend(serials.iter().copied());
                // Direct eval requires source-scope visibility, never activation homing.
                sc.candidates.remove(name);
            }
        }
        // Every function-scope binding visible beneath a with object must have an Environment
        // Record home; direct slots would bypass HasBinding/@@unscopables. Inner block lexicals
        // need their own correctly ordered Environment Records, which are not flattened yet.
        if sc.saw_with {
            if sc.with_requires_inner_env {
                return None;
            }
            sc.captured.extend(sc.top_names.iter().cloned());
        }
        // A captured name declared by an inner depth-0 scope needs per-block freshness —
        // except a candidate whose EVERY capture resolved through its own scope (a same-name
        // capture through any other binding — a for-of head, another block — captured a
        // DIFFERENT binding, which one activation slot can't express), which the compiler
        // homes activation-wide instead.
        let mut homed: Vec<(String, bool)> = Vec::new();
        let mut runtime = std::collections::HashSet::new();
        let mut runtime_and_activation = std::collections::HashSet::new();
        for n in &sc.captured {
            if sc.depth0_inner_decls.contains(n) {
                let candidate = sc.candidates.get(n).copied();
                let ok = candidate.is_some_and(|(serial, _)| {
                    !sc.free_refs.contains(n)
                        && sc
                            .captured_serials
                            .get(n)
                            .is_some_and(|set| set.len() == 1 && set.contains(&serial))
                });
                if ok {
                    homed.push((n.clone(), candidate.expect("checked candidate").1));
                    continue;
                }
                let runtime_serials = sc.runtime_candidates.get(n);
                let captured_serials = sc.captured_serials.get(n)?;
                let mut needs_runtime = false;
                let mut needs_activation = false;
                for serial in captured_serials {
                    if runtime_serials.is_some_and(|supported| supported.contains(serial)) {
                        needs_runtime = true;
                    } else if Some(*serial) == sc.top_serial {
                        needs_activation = true;
                    } else {
                        return None;
                    }
                }
                if needs_runtime {
                    // The emitter selects runtime lexicals by spelling, so promote every supported
                    // declaration of this spelling. Each source scope still gets its own PushLex
                    // record; promoting an uncaptured sibling costs one small record but cannot
                    // merge identities or change resolution.
                    runtime.insert(n.clone());
                }
                if needs_runtime && needs_activation {
                    // Slot/env lookup still respects lexical shadowing, so retaining both homes is
                    // exact. Annex B relies on this when a block function and its promoted var are
                    // both closed over by different functions.
                    runtime_and_activation.insert(n.clone());
                }
            }
        }
        // Homed names leave `captured`: the remaining consumers (param/var/body-lexical
        // homing, the for-of gates) concern OTHER bindings of the name, and any same-name
        // binding that could conflict already poisoned candidacy above.
        for (n, _) in &homed {
            sc.captured.remove(n);
        }
        for n in &runtime {
            if !runtime_and_activation.contains(n) {
                sc.captured.remove(n);
            }
        }
        homed.sort_by(|left, right| left.0.cmp(&right.0)); // deterministic cap_init order
        Some((sc.captured, sc.env_this, homed, runtime, sc.saw_direct_eval))
    }

    fn push_scope(&mut self, names: std::collections::HashSet<String>) {
        self.push_scope_lets(names, Default::default(), false);
    }

    /// Like [`CaptureScan::push_scope`]; `homable` is the subset that can share the activation
    /// when the scope runs at most once per call. `runtime_capable` marks scopes for which the
    /// compiler can instead maintain an exact heap-owned Environment Record.
    fn push_scope_lets(
        &mut self,
        names: std::collections::HashSet<String>,
        homable: std::collections::HashMap<String, bool>,
        runtime_capable: bool,
    ) {
        let serial = self.next_serial;
        self.next_serial += 1;
        if self.fn_depth == 0 && self.scopes.is_empty() {
            self.top_serial = Some(serial);
        }
        if self.fn_depth == 0 && !self.scopes.is_empty() {
            for n in &names {
                self.depth0_inner_decls.insert(n.clone());
                if runtime_capable {
                    self.runtime_candidates
                        .entry(n.clone())
                        .or_default()
                        .push(serial);
                }
                let enclosed = self.scopes.iter().any(|(s, _, _)| s.contains(n));
                if self.loop_depth == 0
                    && self.with_depth == 0
                    && homable.contains_key(n)
                    && !enclosed
                    && self.ever_candidates.insert(n.clone())
                {
                    self.candidates.insert(n.clone(), (serial, homable[n]));
                } else {
                    // A non-qualifying declaration doesn't poison an existing candidate: a
                    // later same-name SLOT declaration (nested or sibling) shadows the env
                    // binding correctly, and a capture through it fails the serial check in
                    // `run`. It does block FUTURE candidacy — the pending-consumption scheme
                    // in the compiler requires the candidate to be the walk-order-FIRST
                    // block-lexical declaration of its name.
                    self.ever_candidates.insert(n.clone());
                }
            }
        } else if self.fn_depth == 0 {
            // The function's top scope: params/vars/body lexicals block all same-name
            // candidacy (they'd be enclosing declarations).
            for n in &names {
                self.ever_candidates.insert(n.clone());
            }
        }
        self.scopes.push((names, self.fn_depth, serial));
    }

    /// Walk a whole function: params + hoisted vars + top-level lexicals in one scope, then body.
    fn fn_body(&mut self, func: &Function) -> Option<()> {
        let mut names = std::collections::HashSet::new();
        for p in &func.params {
            pat_idents(&p.pattern, &mut names);
        }
        if !func.is_arrow {
            names.insert("arguments".to_string());
        }
        if func.is_fn_expr {
            if let Some(n) = &func.name {
                names.insert(n.clone());
            }
        }
        if !hoisted_vars(&func.body, true, func.is_strict, &mut names) {
            return None;
        }
        // ECMA-262 Annex B.3.2 extends FunctionDeclarationInstantiation with a mutable
        // function-scope binding for each qualifying sloppy block FunctionDeclaration. Reuse the
        // interpreter's shared hoist plan so capture analysis observes exactly the same early-error
        // and parameter/`arguments` blockers as execution.
        let mut annexb_blocked = crate::interpreter::param_bound_names(&func.params);
        if !func.is_arrow {
            annexb_blocked.push("arguments".to_string());
        }
        for op in crate::interpreter::collect_hoist_ops(&func.body, func.is_strict, &annexb_blocked)
        {
            if let HoistOp::AnnexB(name, _) = op {
                names.insert(name);
            }
        }
        self.declare_lexicals(&func.body, &mut names);
        if self.fn_depth == 0 {
            self.top_names = names.clone();
        }
        self.push_scope(names);
        // Parameter defaults evaluate in the function scope.
        for p in &func.params {
            if let Some(d) = &p.default {
                self.expr(d)?;
            }
        }
        for s in &func.body {
            self.stmt(s)?;
        }
        self.scopes.pop();
        Some(())
    }

    /// Add a statement list's block-scoped declarations. `homable` additionally collects the
    /// `let`/`using` names whose once-per-call scope can safely share the activation.
    fn declare_lexicals(&self, stmts: &[Stmt], out: &mut std::collections::HashSet<String>) {
        self.declare_lexicals_lets(stmts, out, &mut Default::default());
    }

    fn declare_lexicals_lets(
        &self,
        stmts: &[Stmt],
        out: &mut std::collections::HashSet<String>,
        homable: &mut std::collections::HashMap<String, bool>,
    ) {
        for s in stmts {
            let s = crate::interpreter::unwrap_export(s);
            match s {
                Stmt::VarDecl {
                    kind: DeclKind::Let | DeclKind::Const | DeclKind::Using | DeclKind::AwaitUsing,
                    decls,
                } => {
                    let homed_const = match s {
                        Stmt::VarDecl {
                            kind: DeclKind::Let,
                            ..
                        } => Some(false),
                        Stmt::VarDecl {
                            kind: DeclKind::Using | DeclKind::AwaitUsing,
                            ..
                        } => Some(true),
                        _ => None,
                    };
                    if let Some(is_const) = homed_const {
                        for (p, _) in decls {
                            let mut names = std::collections::HashSet::new();
                            pat_idents(p, &mut names);
                            homable.extend(names.into_iter().map(|name| (name, is_const)));
                        }
                    }
                    for (p, _) in decls {
                        pat_idents(p, out);
                    }
                }
                Stmt::ClassDecl(c) => {
                    if let Some(n) = &c.name {
                        out.insert(n.clone());
                    }
                }
                Stmt::FuncDecl(f) => {
                    // Only reached for *block-level* declarations (top-level ones are in the
                    // hoisted set); strict mode makes them block lexicals. (Sloppy already bailed
                    // in hoisted_vars.)
                    if let Some(n) = &f.name {
                        out.insert(n.clone());
                    }
                }
                _ => {}
            }
        }
    }

    fn block(&mut self, stmts: &[Stmt]) -> Option<()> {
        let mut names = std::collections::HashSet::new();
        let mut homable = std::collections::HashMap::new();
        self.declare_lexicals_lets(stmts, &mut names, &mut homable);
        self.push_scope_lets(names, homable, true);
        for s in stmts {
            self.stmt(s)?;
        }
        self.scopes.pop();
        Some(())
    }

    fn reference(&mut self, name: &str) {
        for (scope, depth, serial) in self.scopes.iter().rev() {
            if scope.contains(name) {
                if *depth == 0 && self.fn_depth > 0 {
                    self.captured.insert(name.to_string());
                    self.captured_serials
                        .entry(name.to_string())
                        .or_default()
                        .insert(*serial);
                }
                return;
            }
        }
        // Unresolved: a global/free name of the whole compilation — nothing to capture, but
        // it poisons activation homing for a like-named block lexical (whose env binding
        // would wrongly shadow the global for this reference).
        self.free_refs.insert(name.to_string());
    }

    /// Walk a pattern in *assignment* position (destructuring assignment): idents are references.
    fn pat_targets(&mut self, p: &Pattern) -> Option<()> {
        match p {
            Pattern::Ident(n) => {
                self.reference(n);
                Some(())
            }
            Pattern::Array(elems) => {
                for e in elems {
                    match e {
                        ArrayPatElem::Hole => {}
                        ArrayPatElem::Elem { pattern, default } => {
                            self.pat_targets(pattern)?;
                            if let Some(d) = default {
                                self.expr(d)?;
                            }
                        }
                        ArrayPatElem::Rest(p) => self.pat_targets(p)?,
                    }
                }
                Some(())
            }
            Pattern::Object(o) => {
                for pr in &o.props {
                    if let PropKey::Computed(k) = &pr.key {
                        self.expr(k)?;
                    }
                    self.pat_targets(&pr.value)?;
                    if let Some(d) = &pr.default {
                        self.expr(d)?;
                    }
                }
                if let Some(r) = &o.rest {
                    self.reference(r);
                }
                Some(())
            }
            Pattern::Member(e) => self.expr(e),
        }
    }

    /// Walk the expressions inside a *declaration* pattern (defaults, computed keys); the idents
    /// themselves were declared by the enclosing scope construction.
    fn pat_decl_exprs(&mut self, p: &Pattern) -> Option<()> {
        match p {
            Pattern::Ident(_) => Some(()),
            Pattern::Array(elems) => {
                for e in elems {
                    match e {
                        ArrayPatElem::Hole => {}
                        ArrayPatElem::Elem { pattern, default } => {
                            self.pat_decl_exprs(pattern)?;
                            if let Some(d) = default {
                                self.expr(d)?;
                            }
                        }
                        ArrayPatElem::Rest(p) => self.pat_decl_exprs(p)?,
                    }
                }
                Some(())
            }
            Pattern::Object(o) => {
                for pr in &o.props {
                    if let PropKey::Computed(k) = &pr.key {
                        self.expr(k)?;
                    }
                    self.pat_decl_exprs(&pr.value)?;
                    if let Some(d) = &pr.default {
                        self.expr(d)?;
                    }
                }
                Some(())
            }
            Pattern::Member(e) => self.expr(e),
        }
    }

    fn stmt(&mut self, s: &Stmt) -> Option<()> {
        match s {
            Stmt::Expr(e) | Stmt::Throw(e) => self.expr(e),
            Stmt::VarDecl { decls, .. } => {
                for (p, init) in decls {
                    self.pat_decl_exprs(p)?;
                    if let Some(e) = init {
                        self.expr(e)?;
                    }
                }
                Some(())
            }
            Stmt::FuncDecl(f) => self.inner_fn(f),
            Stmt::Return(e) => {
                if let Some(e) = e {
                    self.expr(e)?;
                }
                Some(())
            }
            Stmt::If { test, cons, alt } => {
                self.expr(test)?;
                self.stmt(cons)?;
                if let Some(a) = alt {
                    self.stmt(a)?;
                }
                Some(())
            }
            Stmt::Block(b) => self.block(b),
            Stmt::While { test, body } => {
                self.expr(test)?;
                self.loop_depth += 1;
                let r = self.stmt(body);
                self.loop_depth -= 1;
                r
            }
            Stmt::DoWhile { body, test } => {
                self.loop_depth += 1;
                let r = self.stmt(body);
                self.loop_depth -= 1;
                r?;
                self.expr(test)
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                let mut names = std::collections::HashSet::new();
                if let Some(ForInit::VarDecl {
                    kind: DeclKind::Let | DeclKind::Const,
                    decls,
                }) = init.as_deref()
                {
                    for (p, _) in decls {
                        pat_idents(p, &mut names);
                    }
                }
                // The head scope itself counts as in-loop: its lexicals are per-iteration
                // fresh, never once-per-call.
                self.loop_depth += 1;
                self.push_scope_lets(names, Default::default(), true);
                let r = (|| {
                    match init.as_deref() {
                        Some(ForInit::VarDecl { decls, .. }) => {
                            for (p, e) in decls {
                                self.pat_decl_exprs(p)?;
                                if let Some(e) = e {
                                    self.expr(e)?;
                                }
                            }
                        }
                        Some(ForInit::Expr(e)) => self.expr(e)?,
                        None => {}
                    }
                    if let Some(t) = test {
                        self.expr(t)?;
                    }
                    if let Some(u) = update {
                        self.expr(u)?;
                    }
                    self.stmt(body)
                })();
                self.scopes.pop();
                self.loop_depth -= 1;
                r
            }
            Stmt::ForInOf {
                decl,
                left,
                right,
                body,
                ..
            } => {
                self.expr(right)?;
                let mut names = std::collections::HashSet::new();
                match decl {
                    Some(
                        DeclKind::Let | DeclKind::Const | DeclKind::Using | DeclKind::AwaitUsing,
                    ) => {
                        pat_idents(left, &mut names);
                    }
                    Some(DeclKind::Var) => {} // already in the hoisted set
                    None => {}
                }
                self.loop_depth += 1;
                self.push_scope_lets(names, Default::default(), true);
                let r = (|| {
                    if decl.is_none() {
                        self.pat_targets(left)?;
                    } else {
                        self.pat_decl_exprs(left)?;
                    }
                    self.stmt(body)
                })();
                self.scopes.pop();
                self.loop_depth -= 1;
                r
            }
            Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty | Stmt::Debugger => Some(()),
            Stmt::Try {
                block,
                handler,
                finalizer,
            } => {
                self.block(block)?;
                if let Some((param, body)) = handler {
                    if let Some(pattern) = param {
                        // CatchClauseEvaluation creates a parameter environment first, then
                        // evaluates the catch Block in its own nested block environment. Keep
                        // those scope serials distinct so a captured parameter or block lexical
                        // can receive the exact fresh runtime record it denotes.
                        let mut names = std::collections::HashSet::new();
                        pat_idents(pattern, &mut names);
                        self.push_scope_lets(names, Default::default(), true);
                        let result = (|| {
                            self.pat_decl_exprs(pattern)?;
                            self.block(body)
                        })();
                        self.scopes.pop();
                        result?;
                    } else {
                        self.block(body)?;
                    }
                }
                if let Some(f) = finalizer {
                    self.block(f)?;
                }
                Some(())
            }
            Stmt::Switch { disc, cases } => {
                self.expr(disc)?;
                let mut names = std::collections::HashSet::new();
                let mut homable = std::collections::HashMap::new();
                for c in cases {
                    self.declare_lexicals_lets(&c.body, &mut names, &mut homable);
                }
                self.push_scope_lets(names, homable, true);
                let r = (|| {
                    for c in cases {
                        if let Some(t) = &c.test {
                            self.expr(t)?;
                        }
                        for s in &c.body {
                            self.stmt(s)?;
                        }
                    }
                    Some(())
                })();
                self.scopes.pop();
                r
            }
            Stmt::Labeled { body, .. } => self.stmt(body),
            Stmt::With { obj, body } => {
                if !self.allow_with {
                    return None;
                }
                self.saw_with = true;
                self.with_requires_inner_env |= self
                    .scopes
                    .iter()
                    .skip(1)
                    .any(|(names, depth, _)| *depth == 0 && !names.is_empty());
                self.expr(obj)?;
                self.with_depth += 1;
                let result = self.stmt(body);
                self.with_depth -= 1;
                result
            }
            Stmt::ClassDecl(c) => self.class(c),
            // Module declarations carry link-time metadata. Their executable declarations and
            // default expressions are walked in the same lexical context as the module body.
            Stmt::Import(_) | Stmt::ExportNamed { .. } | Stmt::ExportAll { .. } => Some(()),
            Stmt::ExportDecl(inner) | Stmt::ExportDefault(inner) => self.stmt(inner),
        }
    }

    /// Enter an inner function (declaration, expression, method, accessor…).
    fn inner_fn(&mut self, f: &Function) -> Option<()> {
        self.fn_depth += 1;
        self.arrow_path.push(f.is_arrow);
        let r = self.fn_body(f);
        self.arrow_path.pop();
        self.fn_depth -= 1;
        r
    }

    fn class(&mut self, c: &Class) -> Option<()> {
        // Class decorator expressions evaluate outside the class environment. Heritage, member
        // decorators, and computed keys then evaluate in the distinct `classEnv` created by
        // ECMA-262 ClassDefinitionEvaluation; methods and initializers retain that environment.
        // Tag the self-name scope as owned by the latter implicit function-like depth so it is
        // never mistaken for a coroutine activation binding. References can still walk through
        // it to capture genuine locals from the function being compiled.
        for d in &c.decorators {
            self.expr(d)?;
        }
        let mut names = std::collections::HashSet::new();
        if let Some(n) = &c.name {
            names.insert(n.clone());
        }
        self.fn_depth += 1;
        self.push_scope(names);
        self.fn_depth -= 1;
        let r = (|| {
            if let Some(sc) = &c.superclass {
                self.expr(sc)?;
            }
            for m in &c.members {
                for d in &m.decorators {
                    self.expr(d)?;
                }
                if let PropKey::Computed(k) = &m.key {
                    self.expr(k)?;
                }
                if let Some(f) = &m.func {
                    self.inner_fn(f)?;
                }
                if let Some(v) = &m.value {
                    // Field initializers run in an implicit method with its own `this` (the
                    // instance) — inner depth, and NOT part of any outer arrow chain.
                    self.fn_depth += 1;
                    self.arrow_path.push(false);
                    let r = self.expr(v);
                    self.arrow_path.pop();
                    self.fn_depth -= 1;
                    r?;
                }
            }
            Some(())
        })();
        self.scopes.pop();
        r
    }

    fn expr(&mut self, e: &Expr) -> Option<()> {
        match e {
            Expr::Num(_)
            | Expr::BigInt(_)
            | Expr::Str(_)
            | Expr::Bool(_)
            | Expr::Null
            | Expr::Undefined
            | Expr::Regex { .. }
            | Expr::Super
            | Expr::NewTarget
            | Expr::ImportMeta => Some(()),
            Expr::This => {
                // `this` read through an unbroken arrow chain from the outer function observes
                // the outer `this` — the activation must carry it.
                if self.fn_depth > 0 && self.arrow_path[1..].iter().all(|a| *a) {
                    self.env_this = true;
                }
                Some(())
            }
            Expr::Ident(n) => {
                self.reference(n);
                Some(())
            }
            Expr::Paren(i) | Expr::ToStr(i) | Expr::Await(i) | Expr::OptionalChain(i) => {
                self.expr(i)
            }
            Expr::Array(elems) => {
                for el in elems {
                    match el {
                        ArrayElem::Item(e) | ArrayElem::Spread(e) => self.expr(e)?,
                        ArrayElem::Hole => {}
                    }
                }
                Some(())
            }
            Expr::Object(props) => {
                for p in props {
                    match p {
                        PropDef::KeyValue { key, value } | PropDef::Cover { key, value } => {
                            if let PropKey::Computed(k) = key {
                                self.expr(k)?;
                            }
                            self.expr(value)?;
                        }
                        PropDef::Method { key, func }
                        | PropDef::Getter { key, func }
                        | PropDef::Setter { key, func } => {
                            if let PropKey::Computed(k) = key {
                                self.expr(k)?;
                            }
                            self.inner_fn(func)?;
                        }
                        PropDef::Spread(e) | PropDef::Proto(e) => self.expr(e)?,
                    }
                }
                Some(())
            }
            Expr::Func(f) => self.inner_fn(f),
            Expr::Class(c) => self.class(c),
            Expr::Yield { arg, .. } => {
                if let Some(a) = arg {
                    self.expr(a)?;
                }
                Some(())
            }
            Expr::Unary { arg, .. } | Expr::Update { arg, .. } => self.expr(arg),
            Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
                self.expr(left)?;
                self.expr(right)
            }
            Expr::Assign { target, value, .. } => {
                // A destructuring assignment target is a pattern of references.
                match &**target {
                    Expr::Array(_) | Expr::Object(_) => {
                        // Reinterpreting the literal as a pattern is the parser's job; walking it
                        // as an expression visits the same identifiers (Cover handles defaults).
                        self.expr(target)?;
                    }
                    t => self.expr(t)?,
                }
                self.expr(value)
            }
            Expr::Cond { test, cons, alt } => {
                self.expr(test)?;
                self.expr(cons)?;
                self.expr(alt)
            }
            Expr::Call {
                callee,
                args,
                optional,
            } => {
                if !optional && matches!(&**callee, Expr::Ident(n) if n == "eval") {
                    if !self.allow_direct_eval {
                        return None;
                    }
                    self.saw_direct_eval = true;
                    for arg in args {
                        match arg {
                            ArrayElem::Item(expr) | ArrayElem::Spread(expr) => self.expr(expr)?,
                            ArrayElem::Hole => {}
                        }
                    }
                    return Some(());
                }
                self.expr(callee)?;
                for a in args {
                    match a {
                        ArrayElem::Item(e) | ArrayElem::Spread(e) => self.expr(e)?,
                        ArrayElem::Hole => {}
                    }
                }
                Some(())
            }
            Expr::New { callee, args } => {
                self.expr(callee)?;
                for a in args {
                    match a {
                        ArrayElem::Item(e) | ArrayElem::Spread(e) => self.expr(e)?,
                        ArrayElem::Hole => {}
                    }
                }
                Some(())
            }
            Expr::Member { obj, .. } => self.expr(obj),
            Expr::Index { obj, index, .. } => {
                self.expr(obj)?;
                self.expr(index)
            }
            Expr::Seq(es) => {
                for e in es {
                    self.expr(e)?;
                }
                Some(())
            }
            Expr::TaggedTemplate { tag, subs, .. } => {
                self.expr(tag)?;
                for s in subs {
                    self.expr(s)?;
                }
                Some(())
            }
            Expr::PrivateIn { obj, .. } => self.expr(obj),
            Expr::ImportCall { spec, options, .. } => {
                self.expr(spec)?;
                if let Some(o) = options {
                    self.expr(o)?;
                }
                Some(())
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Compiler
// ---------------------------------------------------------------------------------------------

const VALUE_OPERAND_0: crate::feedback::SlotDescriptor = crate::feedback::SlotDescriptor {
    kind: crate::feedback::ObservationKind::ValueClass,
    role: crate::feedback::ObservationRole::Operand0,
};
const VALUE_OPERAND_1: crate::feedback::SlotDescriptor = crate::feedback::SlotDescriptor {
    kind: crate::feedback::ObservationKind::ValueClass,
    role: crate::feedback::ObservationRole::Operand1,
};
const VALUE_RESULT: crate::feedback::SlotDescriptor = crate::feedback::SlotDescriptor {
    kind: crate::feedback::ObservationKind::ValueClass,
    role: crate::feedback::ObservationRole::Result,
};
const RECEIVER_LAYOUT: crate::feedback::SlotDescriptor = crate::feedback::SlotDescriptor {
    kind: crate::feedback::ObservationKind::ReceiverLayout,
    role: crate::feedback::ObservationRole::Receiver,
};
const HOLDER_LAYOUT: crate::feedback::SlotDescriptor = crate::feedback::SlotDescriptor {
    kind: crate::feedback::ObservationKind::HolderLayout,
    role: crate::feedback::ObservationRole::Holder,
};
const PROPERTY_ACCESS: crate::feedback::SlotDescriptor = crate::feedback::SlotDescriptor {
    kind: crate::feedback::ObservationKind::PropertyAccess,
    role: crate::feedback::ObservationRole::Access,
};
const ELEMENT_ACCESS: crate::feedback::SlotDescriptor = crate::feedback::SlotDescriptor {
    kind: crate::feedback::ObservationKind::ElementAccess,
    role: crate::feedback::ObservationRole::Access,
};
const CALL_TARGET: crate::feedback::SlotDescriptor = crate::feedback::SlotDescriptor {
    kind: crate::feedback::ObservationKind::CallTarget,
    role: crate::feedback::ObservationRole::Target,
};
const BRANCH_OUTCOME: crate::feedback::SlotDescriptor = crate::feedback::SlotDescriptor {
    kind: crate::feedback::ObservationKind::BranchCount,
    role: crate::feedback::ObservationRole::Outcome,
};
const ALLOCATION_OUTCOME: crate::feedback::SlotDescriptor = crate::feedback::SlotDescriptor {
    kind: crate::feedback::ObservationKind::Allocation,
    role: crate::feedback::ObservationRole::Outcome,
};

/// Build the immutable semantic layout from the canonical baseline bytecode. Cache indexes and
/// raw shape/callee state are intentionally ignored: a future adapter may read them, but they are
/// not part of the profile schema. ECMA-262 §6.1, §10.1.8.1, and §13.3.6.2 define the semantic
/// distinctions represented by these slots.
fn feedback_layout_for_ops(
    ops: &[Op],
    names: &[Rc<str>],
) -> (
    crate::feedback::FeedbackLayout,
    Box<[crate::feedback::RuntimeBinding]>,
) {
    use crate::feedback::{LayoutBuilder, OperationKind, RuntimeBinding};

    let mut builder = LayoutBuilder::default();
    let mut bindings = Vec::new();
    for (pc, op) in ops.iter().enumerate() {
        let binding = match op {
            Op::GetProp(_, cache)
            | Op::GetPropThis(_, cache)
            | Op::SetProp(_, cache)
            | Op::SetPropDrop(_, cache)
            | Op::SetPropThisDrop(_, cache)
            | Op::AppendProp(_, cache)
            | Op::GetMethod(_, cache) => RuntimeBinding::PropertyIc {
                first_way: *cache,
                way_count: PROP_IC_WAYS as u8,
            },
            Op::GetPropLocal(_, _, cache)
            | Op::SetPropLocalDrop(_, _, cache)
            | Op::UpdateProp(_, cache, _) => RuntimeBinding::PropertyIc {
                first_way: *cache,
                way_count: PROP_IC_WAYS as u8,
            },
            _ => RuntimeBinding::Unbound,
        };
        let (operation, slots): (OperationKind, &[crate::feedback::SlotDescriptor]) = match op {
            Op::GetProp(..) | Op::GetPropThis(..) | Op::GetPropLocal(..) | Op::GetMethod(..) => (
                OperationKind::NamedLoad,
                &[
                    RECEIVER_LAYOUT,
                    HOLDER_LAYOUT,
                    PROPERTY_ACCESS,
                    VALUE_RESULT,
                ],
            ),
            Op::SetProp(..)
            | Op::SetPropDrop(..)
            | Op::SetPropThisDrop(..)
            | Op::SetPropLocalDrop(..)
            | Op::AppendProp(..) => (
                OperationKind::NamedStore,
                &[
                    RECEIVER_LAYOUT,
                    HOLDER_LAYOUT,
                    PROPERTY_ACCESS,
                    VALUE_OPERAND_1,
                ],
            ),
            Op::UpdateProp(..) => (
                OperationKind::NamedStore,
                &[
                    RECEIVER_LAYOUT,
                    HOLDER_LAYOUT,
                    PROPERTY_ACCESS,
                    VALUE_OPERAND_0,
                    VALUE_RESULT,
                ],
            ),
            Op::GetElem | Op::GetElemLocal(..) | Op::GetMethodElem => (
                OperationKind::ElementLoad,
                &[RECEIVER_LAYOUT, ELEMENT_ACCESS, HOLDER_LAYOUT, VALUE_RESULT],
            ),
            Op::SetElem | Op::SetElemDrop | Op::SetElemLocal(..) | Op::SetElemLocalDrop(..) => (
                OperationKind::ElementStore,
                &[RECEIVER_LAYOUT, ELEMENT_ACCESS, VALUE_OPERAND_1],
            ),
            Op::UpdateElem(..) => (
                OperationKind::ElementStore,
                &[
                    RECEIVER_LAYOUT,
                    ELEMENT_ACCESS,
                    VALUE_OPERAND_0,
                    VALUE_RESULT,
                ],
            ),
            Op::Call(..)
            | Op::CallWithThis(..)
            | Op::CallSpread(..)
            | Op::CallSpreadThis(..)
            | Op::CallArgsArray
            | Op::CallArgsArrayThis
            | Op::EvalCallArgsArray => (OperationKind::Call, &[CALL_TARGET, VALUE_RESULT]),
            Op::New(..) | Op::NewArgsArray | Op::SuperCallArgsArray => {
                (OperationKind::Construct, &[CALL_TARGET, VALUE_RESULT])
            }
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Mod
            | Op::BitAnd
            | Op::BitOr
            | Op::BitXor
            | Op::Shl
            | Op::Shr
            | Op::UShr => (
                OperationKind::Arithmetic,
                &[VALUE_OPERAND_0, VALUE_OPERAND_1, VALUE_RESULT],
            ),
            Op::GenBin(name)
                if names
                    .get(*name as usize)
                    .is_some_and(|name| &**name == "**") =>
            {
                (
                    OperationKind::Arithmetic,
                    &[VALUE_OPERAND_0, VALUE_OPERAND_1, VALUE_RESULT],
                )
            }
            Op::Neg | Op::Plus | Op::BitNot => {
                (OperationKind::Arithmetic, &[VALUE_OPERAND_0, VALUE_RESULT])
            }
            Op::UpdateLocal(..)
            | Op::UpdateCap(..)
            | Op::UpdateName(..)
            | Op::UpdateNameCached(..)
            | Op::UpdateConst(..)
            | Op::UpdatePrivate(..)
            | Op::SuperUpdate(..) => (OperationKind::Arithmetic, &[VALUE_OPERAND_0, VALUE_RESULT]),
            Op::JumpIfFalse(..)
            | Op::JumpIfFalsePeek(..)
            | Op::JumpIfTruePeek(..)
            | Op::JumpIfNotNullishPeek(..) => (OperationKind::Branch, &[BRANCH_OUTCOME]),
            Op::Jump(target) if (*target as usize) <= pc => {
                (OperationKind::Loop, &[BRANCH_OUTCOME])
            }
            Op::MakeClosure(..)
            | Op::MakeRegExp(..)
            | Op::MakeArray(..)
            | Op::NewArray
            | Op::MakeObject(..)
            | Op::NewObject => (OperationKind::Allocation, &[ALLOCATION_OUTCOME]),
            _ => continue,
        };
        builder.add_site(pc, operation, slots);
        bindings.push(binding);
    }
    (builder.finish(), bindings.into_boxed_slice())
}

const OBS_FLAG_ARRAY_KEY_CHECK: u8 = 0x80;
const OBS_FLAG_CREATION: u8 = 0x40;

fn intern_current_shape(shapes: &std::cell::RefCell<Vec<u32>>, shape: u32) -> u32 {
    let mut shapes = shapes.borrow_mut();
    if let Some(index) = shapes.iter().position(|candidate| *candidate == shape) {
        return index as u32 + 1;
    }
    let token = u32::try_from(shapes.len())
        .expect("feedback layout identity space exhausted")
        .checked_add(1)
        .expect("feedback layout identity space exhausted");
    shapes.push(shape);
    token
}

/// Current-shape adapter for diagnostic snapshots. It reads warmed property ICs only when the
/// opt-in retained-memory/profile walk runs, interns Agent-local shapes into dense abstract
/// tokens, and writes no raw shape or pointer into the versioned vector.
fn refresh_current_layout_feedback(
    feedback: &crate::feedback::FeedbackVector,
    shapes: &std::cell::RefCell<Vec<u32>>,
    caches: &[std::cell::Cell<IcState>],
) {
    use crate::feedback::{
        property_access_flags, ObservationKind, ObservationRole, ObservationState, ObservationWord,
        PropertyOutcome, RuntimeBinding,
    };

    #[derive(Clone, Copy, Eq, PartialEq)]
    struct CurrentPropertyObservation {
        receiver_shape: u32,
        holder_shape: Option<u32>,
        layout_flags: u8,
        access_state: ObservationState,
        access_payload: u32,
        access_flags: u8,
    }

    for (site, binding) in feedback.sites() {
        let RuntimeBinding::PropertyIc {
            first_way,
            way_count,
        } = binding
        else {
            continue;
        };
        let start = first_way as usize;
        let mut ways = Vec::with_capacity(way_count as usize);
        let mut generic = false;
        for state in caches.iter().skip(start).take(way_count as usize) {
            let state = state.get();
            let observation = match state.depth {
                IC_EMPTY => continue,
                IC_ABSENT => CurrentPropertyObservation {
                    receiver_shape: state.recv_shape,
                    holder_shape: None,
                    layout_flags: 0,
                    access_state: ObservationState::Absent,
                    access_payload: 0,
                    access_flags: property_access_flags(PropertyOutcome::Absent, 0, false),
                },
                IC_CREATE => CurrentPropertyObservation {
                    receiver_shape: state.recv_shape,
                    holder_shape: None,
                    layout_flags: OBS_FLAG_CREATION,
                    access_state: ObservationState::Monomorphic,
                    access_payload: 0,
                    access_flags: property_access_flags(PropertyOutcome::Created, 0, false),
                },
                encoded_depth => {
                    let key_check = encoded_depth & IC_ARR_KEYCHK != 0;
                    let depth = encoded_depth & !IC_ARR_KEYCHK;
                    let Some(slot) = state.slot.checked_add(1) else {
                        generic = true;
                        break;
                    };
                    if depth > IC_MAX_DEPTH {
                        generic = true;
                        break;
                    }
                    CurrentPropertyObservation {
                        receiver_shape: state.recv_shape,
                        holder_shape: Some(state.holder_shape),
                        layout_flags: depth
                            | if key_check {
                                OBS_FLAG_ARRAY_KEY_CHECK
                            } else {
                                0
                            },
                        access_state: ObservationState::Monomorphic,
                        access_payload: slot,
                        access_flags: property_access_flags(
                            PropertyOutcome::Data,
                            depth,
                            key_check,
                        ),
                    }
                }
            };
            if !ways.contains(&observation) {
                ways.push(observation);
            }
        }
        if generic {
            let word = ObservationWord::new(ObservationState::Generic, 0, 0);
            feedback.merge_write(
                site,
                ObservationKind::ReceiverLayout,
                ObservationRole::Receiver,
                word,
            );
            feedback.merge_write(
                site,
                ObservationKind::HolderLayout,
                ObservationRole::Holder,
                word,
            );
            feedback.merge_write(
                site,
                ObservationKind::PropertyAccess,
                ObservationRole::Access,
                word,
            );
            continue;
        }
        match ways.as_slice() {
            [] => {}
            [observation] => {
                let receiver = intern_current_shape(shapes, observation.receiver_shape);
                feedback.merge_write(
                    site,
                    ObservationKind::ReceiverLayout,
                    ObservationRole::Receiver,
                    ObservationWord::new(
                        ObservationState::Monomorphic,
                        receiver,
                        observation.layout_flags,
                    ),
                );
                let holder_word = observation.holder_shape.map_or_else(
                    || ObservationWord::new(ObservationState::Absent, 0, observation.layout_flags),
                    |holder| {
                        ObservationWord::new(
                            ObservationState::Monomorphic,
                            intern_current_shape(shapes, holder),
                            observation.layout_flags,
                        )
                    },
                );
                feedback.merge_write(
                    site,
                    ObservationKind::HolderLayout,
                    ObservationRole::Holder,
                    holder_word,
                );
                feedback.merge_write(
                    site,
                    ObservationKind::PropertyAccess,
                    ObservationRole::Access,
                    ObservationWord::new(
                        observation.access_state,
                        observation.access_payload,
                        observation.access_flags,
                    ),
                );
            }
            polymorphic => {
                let word = ObservationWord::new(
                    ObservationState::Polymorphic,
                    polymorphic.len() as u32,
                    0,
                );
                feedback.merge_write(
                    site,
                    ObservationKind::ReceiverLayout,
                    ObservationRole::Receiver,
                    word,
                );
                feedback.merge_write(
                    site,
                    ObservationKind::HolderLayout,
                    ObservationRole::Holder,
                    word,
                );
                feedback.merge_write(
                    site,
                    ObservationKind::PropertyAccess,
                    ObservationRole::Access,
                    word,
                );
            }
        }
    }
}

/// Runtime adapter from the current `Value` representation to the stable feedback schema.
///
/// ECMA-262 §6.1.6 defines Number as binary64. The int32 subdivision is an optimization fact:
/// negative zero and every value that cannot round-trip through signed int32 stay NumberDouble.
fn arithmetic_value_class(value: &Value) -> Option<crate::feedback::ValueClass> {
    use crate::feedback::ValueClass;

    Some(match value {
        Value::Undefined => ValueClass::Undefined,
        Value::Empty => return None,
        Value::Null => ValueClass::Null,
        Value::Bool(_) => ValueClass::Boolean,
        Value::Num(number)
            if number.is_finite()
                && !(number.to_bits() == (-0.0_f64).to_bits())
                && number.fract() == 0.0
                && *number >= i32::MIN as f64
                && *number <= i32::MAX as f64 =>
        {
            ValueClass::NumberInt32
        }
        Value::Num(_) => ValueClass::NumberDouble,
        Value::BigInt(_) => ValueClass::BigInt,
        Value::Str(_) => ValueClass::String,
        Value::Sym(_) => ValueClass::Symbol,
        Value::Obj(_) => ValueClass::Object,
    })
}

#[inline(always)]
fn observe_arithmetic_value(
    feedback: &crate::feedback::FeedbackVector,
    pc: usize,
    role: crate::feedback::ObservationRole,
    value: &Value,
) {
    if let Some(class) = arithmetic_value_class(value) {
        feedback.observe_value_class(pc, role, class);
    }
}

#[inline(always)]
fn observe_arithmetic_operand(
    feedback: &crate::feedback::FeedbackVector,
    pc: usize,
    value: &Value,
) -> bool {
    let enabled = feedback.detailed_enabled();
    if enabled {
        observe_arithmetic_value(
            feedback,
            pc,
            crate::feedback::ObservationRole::Operand0,
            value,
        );
    }
    enabled
}

#[inline(always)]
fn observe_arithmetic_operands(
    feedback: &crate::feedback::FeedbackVector,
    pc: usize,
    left: &Value,
    right: &Value,
) -> bool {
    let enabled = feedback.detailed_enabled();
    if enabled {
        observe_arithmetic_value(
            feedback,
            pc,
            crate::feedback::ObservationRole::Operand0,
            left,
        );
        observe_arithmetic_value(
            feedback,
            pc,
            crate::feedback::ObservationRole::Operand1,
            right,
        );
    }
    enabled
}

#[inline(always)]
fn observe_arithmetic_result(
    feedback: &crate::feedback::FeedbackVector,
    pc: usize,
    enabled: bool,
    result: &Value,
) {
    if enabled {
        observe_arithmetic_value(
            feedback,
            pc,
            crate::feedback::ObservationRole::Result,
            result,
        );
    }
}

#[inline(always)]
fn observe_allocation(
    feedback: &crate::feedback::FeedbackVector,
    pc: usize,
    object: crate::feedback::AllocationObjectKind,
    requested_units: usize,
) {
    if feedback.detailed_enabled() {
        feedback.observe_allocation(pc, object, requested_units);
    }
}

#[cfg(test)]
mod feedback_layout_tests {
    use super::*;
    use crate::feedback::{
        property_access_flags, CallEnvironmentKind, CallTargetKind, ElementKeyKind, ElementOutcome,
        ElementReceiverKind, FeedbackVector, ObservationKind, ObservationRole, ObservationState,
        OperationKind, PropertyOutcome, ValueClass,
    };

    #[test]
    fn canonical_layout_ignores_raw_cache_and_name_indexes() {
        let (first, first_bindings) =
            feedback_layout_for_ops(&[Op::GetProp(1, 4), Op::Add, Op::CallWithThis(2, 7)], &[]);
        let (second, second_bindings) = feedback_layout_for_ops(
            &[Op::GetProp(99, 400), Op::Add, Op::CallWithThis(2, 700)],
            &[],
        );

        assert_eq!(first, second);
        assert_ne!(first_bindings, second_bindings);
        assert_eq!(first.version(), crate::feedback::SCHEMA_VERSION);
        assert_eq!(first.len(), 3);
        assert_eq!(first.slot_len(), 9);
        let sites = first.site_ids().collect::<Vec<_>>();
        assert_eq!(
            first.site(sites[0]).unwrap().operation,
            OperationKind::NamedLoad
        );
        assert_eq!(
            first.site(sites[1]).unwrap().operation,
            OperationKind::Arithmetic
        );
        assert_eq!(first.site(sites[2]).unwrap().operation, OperationKind::Call);
        assert_eq!(
            first.slots(sites[2]).unwrap(),
            &[
                crate::feedback::SlotDescriptor {
                    kind: ObservationKind::CallTarget,
                    role: ObservationRole::Target,
                },
                VALUE_RESULT,
            ]
        );
    }

    #[test]
    fn layout_records_back_edges_separately_from_conditional_branches() {
        let (layout, _) = feedback_layout_for_ops(
            &[Op::JumpIfFalse(3), Op::Undef, Op::Jump(0), Op::ReturnUndef],
            &[],
        );
        let sites = layout.site_ids().collect::<Vec<_>>();

        assert_eq!(sites.len(), 2);
        assert_eq!(
            layout.site(sites[0]).unwrap().operation,
            OperationKind::Branch
        );
        assert_eq!(
            layout.site(sites[1]).unwrap().operation,
            OperationKind::Loop
        );
    }

    #[test]
    fn layout_records_explicit_allocation_sites() {
        let ops = [
            Op::MakeClosure(0, u32::MAX),
            Op::MakeRegExp(0, 1),
            Op::MakeArray(2),
            Op::NewArray,
            Op::MakeObject(0, 1, u32::MAX),
            Op::NewObject,
        ];
        let names = [Rc::from("pattern"), Rc::from("g")];
        let (layout, _) = feedback_layout_for_ops(&ops, &names);
        assert_eq!(layout.len(), ops.len());
        assert!(layout.site_ids().all(|site| {
            layout.site(site).unwrap().operation == OperationKind::Allocation
                && layout.slots(site).unwrap()
                    == [crate::feedback::SlotDescriptor {
                        kind: ObservationKind::Allocation,
                        role: ObservationRole::Outcome,
                    }]
        }));
    }

    #[test]
    fn layout_stays_empty_for_bytecode_without_feedback_sites() {
        let (layout, bindings) = feedback_layout_for_ops(&[Op::Undef, Op::Return], &[]);
        assert!(layout.is_empty());
        assert_eq!(layout.slot_len(), 0);
        assert!(bindings.is_empty());
    }

    #[test]
    fn exponentiation_uses_the_arithmetic_schema_not_generic_binary_feedback() {
        let names: [Rc<str>; 2] = [Rc::from("in"), Rc::from("**")];
        let (layout, _) = feedback_layout_for_ops(&[Op::GenBin(0), Op::GenBin(1)], &names);
        let sites = layout.site_ids().collect::<Vec<_>>();

        assert_eq!(sites.len(), 1);
        assert_eq!(layout.site(sites[0]).unwrap().bytecode_pc, 1);
        assert_eq!(
            layout.site(sites[0]).unwrap().operation,
            OperationKind::Arithmetic
        );
    }

    #[test]
    fn arithmetic_adapter_preserves_number_refinements_and_language_types() {
        let interp = Interp::new();
        assert_eq!(
            arithmetic_value_class(&Value::Num(0.0)),
            Some(ValueClass::NumberInt32)
        );
        assert_eq!(
            arithmetic_value_class(&Value::Num(-0.0)),
            Some(ValueClass::NumberDouble)
        );
        assert_eq!(
            arithmetic_value_class(&Value::Num(i32::MAX as f64 + 1.0)),
            Some(ValueClass::NumberDouble)
        );
        assert_eq!(
            arithmetic_value_class(&Value::Str(crate::lstr::LStr::from("x"))),
            Some(ValueClass::String)
        );
        assert_eq!(
            arithmetic_value_class(&Value::BigInt(crate::bigint::JsBigInt::from_u64(1))),
            Some(ValueClass::BigInt)
        );
        assert_eq!(
            arithmetic_value_class(&Value::Obj(interp.new_object())),
            Some(ValueClass::Object)
        );
        assert_eq!(arithmetic_value_class(&Value::Empty), None);
    }

    #[test]
    fn tagged_numeric_binary_accepts_only_immediate_numbers() {
        let add = |left: f64, right: f64| left + right;
        let result = try_tagged_numeric_binary(&Value::Num(1.5), &Value::Num(2.0), &add)
            .expect("Number operands use the tagged frame");
        assert!(matches!(result, Value::Num(value) if value == 3.5));

        let interp = Interp::new();
        assert!(try_tagged_numeric_binary(&Value::str("x"), &Value::Num(2.0), &add).is_none());
        assert!(try_tagged_numeric_binary(
            &Value::BigInt(crate::bigint::JsBigInt::from_u64(1)),
            &Value::Num(2.0),
            &add,
        )
        .is_none());
        assert!(try_tagged_numeric_binary(
            &Value::Obj(interp.new_object()),
            &Value::Num(2.0),
            &add
        )
        .is_none());
    }

    #[test]
    fn tagged_numeric_binary_keeps_ieee_edge_results() {
        let add = |left: f64, right: f64| left + right;
        let negative_zero = try_tagged_numeric_binary(&Value::Num(-0.0), &Value::Num(-0.0), &add)
            .expect("numeric fast path");
        assert!(
            matches!(negative_zero, Value::Num(value) if value.to_bits() == (-0.0f64).to_bits())
        );

        let nan = try_tagged_numeric_binary(&Value::Num(f64::NAN), &Value::Num(1.0), &add)
            .expect("numeric fast path");
        assert!(matches!(nan, Value::Num(value) if value.is_nan()));

        let infinity = try_tagged_numeric_binary(
            &Value::Num(f64::INFINITY),
            &Value::Num(f64::NEG_INFINITY),
            &add,
        )
        .expect("numeric fast path");
        assert!(matches!(infinity, Value::Num(value) if value.is_nan()));
    }

    #[test]
    fn profiled_binary_helper_records_original_operands_and_successful_result() {
        let (layout, bindings) = feedback_layout_for_ops(&[Op::Add], &[]);
        let feedback = FeedbackVector::new_with_enabled(layout, bindings, true);
        let mut interp = Interp::new();
        let mut stack = vec![Value::str("left"), Value::Num(1.0)];

        assert!(bin_num(&mut interp, &mut stack, &feedback, 0, "+", |a, b| a + b).is_ok());
        assert!(matches!(stack.as_slice(), [Value::Str(value)] if &**value == "left1"));
        let site = feedback.sites().next().unwrap().0;
        assert_eq!(
            feedback
                .read(site, ObservationKind::ValueClass, ObservationRole::Operand0)
                .payload(),
            ValueClass::String.bit()
        );
        assert_eq!(
            feedback
                .read(site, ObservationKind::ValueClass, ObservationRole::Operand1)
                .payload(),
            ValueClass::NumberInt32.bit()
        );
        assert_eq!(
            feedback
                .read(site, ObservationKind::ValueClass, ObservationRole::Result)
                .payload(),
            ValueClass::String.bit()
        );
    }

    #[test]
    fn disabled_binary_feedback_does_not_allocate_observation_words() {
        let (layout, bindings) = feedback_layout_for_ops(&[Op::Add], &[]);
        let feedback = FeedbackVector::new_with_enabled(layout, bindings, false);
        let retained_before = feedback.retained_bytes();
        let mut interp = Interp::new();
        let mut stack = vec![Value::Num(1.0), Value::Num(2.0)];

        assert!(bin_num(&mut interp, &mut stack, &feedback, 0, "+", |a, b| a + b).is_ok());
        assert_eq!(stack.len(), 1);
        assert!(matches!(stack[0], Value::Num(3.0)));
        assert_eq!(feedback.retained_bytes(), retained_before);
    }

    #[test]
    fn throwing_numeric_mix_records_operands_but_no_result() {
        let (layout, bindings) = feedback_layout_for_ops(&[Op::Add], &[]);
        let feedback = FeedbackVector::new_with_enabled(layout, bindings, true);
        let mut interp = Interp::new();
        let mut stack = vec![
            Value::BigInt(crate::bigint::JsBigInt::from_u64(1)),
            Value::Num(1.0),
        ];

        assert!(bin_num(&mut interp, &mut stack, &feedback, 0, "+", |a, b| a + b).is_err());
        let site = feedback.sites().next().unwrap().0;
        assert_eq!(
            feedback
                .read(site, ObservationKind::ValueClass, ObservationRole::Operand0)
                .payload(),
            ValueClass::BigInt.bit()
        );
        assert_eq!(
            feedback
                .read(site, ObservationKind::ValueClass, ObservationRole::Operand1)
                .payload(),
            ValueClass::NumberInt32.bit()
        );
        assert_eq!(
            feedback
                .read(site, ObservationKind::ValueClass, ObservationRole::Result)
                .state(),
            ObservationState::Uninitialized
        );
    }

    #[test]
    fn update_layout_covers_every_reference_lowering() {
        let ops = [
            Op::UpdateLocal(0, UpdKind::PostInc),
            Op::UpdateCap(0, UpdKind::PreDec),
            Op::UpdateName(0, UpdKind::IncDiscard),
            Op::UpdateNameCached(0, 0, UpdKind::DecDiscard),
            Op::UpdateConst(0, UpdKind::PostInc),
            Op::UpdateProp(0, 0, UpdKind::PreInc),
            Op::UpdateElem(UpdKind::PostDec),
            Op::UpdatePrivate(0, UpdKind::PreDec),
            Op::SuperUpdate(UpdKind::PostInc),
        ];
        let (layout, _) = feedback_layout_for_ops(&ops, &[Rc::from("value")]);
        let sites = layout.site_ids().collect::<Vec<_>>();

        assert_eq!(sites.len(), ops.len());
        for site in sites {
            let slots = layout.slots(site).expect("update site has slots");
            assert!(slots.contains(&VALUE_OPERAND_0));
            assert!(slots.contains(&VALUE_RESULT));
        }
    }

    #[test]
    fn profiled_update_records_raw_operand_and_successful_new_value() {
        let (layout, bindings) = feedback_layout_for_ops(
            &[Op::UpdateLocal(0, UpdKind::PostInc)],
            &[Rc::from("value")],
        );
        let feedback = FeedbackVector::new_with_enabled(layout, bindings, true);
        let mut interp = Interp::new();
        let mut stored = None;

        let expression = match step_value(
            &mut interp,
            &feedback,
            0,
            UpdKind::PostInc,
            Value::str("4"),
            |_, value| {
                stored = Some(value);
                Ok(())
            },
        ) {
            Ok(Some(value)) => value,
            Ok(None) => panic!("postfix update must return a value"),
            Err(_) => panic!("string update must succeed"),
        };

        assert!(matches!(expression, Value::Num(4.0)));
        assert!(matches!(stored, Some(Value::Num(5.0))));
        let site = feedback.sites().next().unwrap().0;
        assert_eq!(
            feedback
                .read(site, ObservationKind::ValueClass, ObservationRole::Operand0)
                .payload(),
            ValueClass::String.bit()
        );
        assert_eq!(
            feedback
                .read(site, ObservationKind::ValueClass, ObservationRole::Result)
                .payload(),
            ValueClass::NumberInt32.bit()
        );
    }

    #[test]
    fn failed_update_write_does_not_publish_a_result_class() {
        let (layout, bindings) = feedback_layout_for_ops(
            &[Op::UpdateProp(0, 0, UpdKind::PreInc)],
            &[Rc::from("value")],
        );
        let feedback = FeedbackVector::new_with_enabled(layout, bindings, true);
        let mut interp = Interp::new();

        assert!(step_value(
            &mut interp,
            &feedback,
            0,
            UpdKind::PreInc,
            Value::Num(2.0),
            |interp, _| Err(interp.throw("TypeError", "setter rejected update")),
        )
        .is_err());

        let site = feedback.sites().next().unwrap().0;
        assert_eq!(
            feedback
                .read(site, ObservationKind::ValueClass, ObservationRole::Operand0)
                .payload(),
            ValueClass::NumberInt32.bit()
        );
        assert_eq!(
            feedback
                .read(site, ObservationKind::ValueClass, ObservationRole::Result)
                .state(),
            ObservationState::Uninitialized
        );
    }

    #[test]
    fn current_shape_adapter_uses_dense_tokens_and_widens_polymorphism() {
        let (layout, bindings) = feedback_layout_for_ops(&[Op::GetProp(0, 0)], &[]);
        let feedback = FeedbackVector::new(layout, bindings);
        let shapes = std::cell::RefCell::new(Vec::new());
        let caches = (0..PROP_IC_WAYS)
            .map(|_| std::cell::Cell::new(IcState::EMPTY))
            .collect::<Vec<_>>();
        caches[0].set(IcState {
            recv_shape: 41,
            holder_shape: 73,
            slot: 2,
            depth: 1,
            mid_ok: 0,
            mid_shape: 0,
            mid2_shape: 0,
        });

        refresh_current_layout_feedback(&feedback, &shapes, &caches);
        let site = feedback.sites().next().unwrap().0;
        let receiver = feedback.read(
            site,
            ObservationKind::ReceiverLayout,
            ObservationRole::Receiver,
        );
        let holder = feedback.read(site, ObservationKind::HolderLayout, ObservationRole::Holder);
        let access = feedback.read(
            site,
            ObservationKind::PropertyAccess,
            ObservationRole::Access,
        );
        assert_eq!(receiver.state(), ObservationState::Monomorphic);
        assert_eq!(holder.state(), ObservationState::Monomorphic);
        assert_eq!(access.state(), ObservationState::Monomorphic);
        assert_eq!((receiver.payload(), holder.payload()), (1, 2));
        assert_eq!(access.payload(), 3);
        assert_eq!(
            access.flags(),
            property_access_flags(PropertyOutcome::Data, 1, false)
        );
        assert_eq!(&*shapes.borrow(), &[41, 73]);

        caches[1].set(IcState {
            recv_shape: 99,
            holder_shape: 73,
            slot: 2,
            depth: 1,
            mid_ok: 0,
            mid_shape: 0,
            mid2_shape: 0,
        });
        refresh_current_layout_feedback(&feedback, &shapes, &caches);
        let receiver = feedback.read(
            site,
            ObservationKind::ReceiverLayout,
            ObservationRole::Receiver,
        );
        let access = feedback.read(
            site,
            ObservationKind::PropertyAccess,
            ObservationRole::Access,
        );
        assert_eq!(receiver.state(), ObservationState::Polymorphic);
        assert_eq!(receiver.payload(), 2);
        assert_eq!(access.state(), ObservationState::Polymorphic);
        assert_eq!(access.payload(), 2);
    }

    #[test]
    fn current_shape_adapter_records_absence_without_a_fake_holder() {
        let (layout, bindings) = feedback_layout_for_ops(&[Op::GetProp(0, 0)], &[]);
        let feedback = FeedbackVector::new(layout, bindings);
        let shapes = std::cell::RefCell::new(Vec::new());
        let caches = (0..PROP_IC_WAYS)
            .map(|_| std::cell::Cell::new(IcState::EMPTY))
            .collect::<Vec<_>>();
        caches[0].set(IcState {
            recv_shape: 12,
            holder_shape: 0,
            slot: 1,
            depth: IC_ABSENT,
            mid_ok: 0,
            mid_shape: 0,
            mid2_shape: 0,
        });

        refresh_current_layout_feedback(&feedback, &shapes, &caches);
        let site = feedback.sites().next().unwrap().0;
        assert_eq!(
            feedback
                .read(site, ObservationKind::HolderLayout, ObservationRole::Holder,)
                .state(),
            ObservationState::Absent
        );
        let access = feedback.read(
            site,
            ObservationKind::PropertyAccess,
            ObservationRole::Access,
        );
        assert_eq!(access.state(), ObservationState::Absent);
        assert_eq!(access.payload(), 0);
        assert_eq!(
            access.flags(),
            property_access_flags(PropertyOutcome::Absent, 0, false)
        );
        assert_eq!(&*shapes.borrow(), &[12]);
    }

    #[test]
    fn current_shape_adapter_distinguishes_property_creation() {
        let (layout, bindings) = feedback_layout_for_ops(&[Op::SetProp(0, 0)], &[]);
        let feedback = FeedbackVector::new(layout, bindings);
        let shapes = std::cell::RefCell::new(Vec::new());
        let caches = (0..PROP_IC_WAYS)
            .map(|_| std::cell::Cell::new(IcState::EMPTY))
            .collect::<Vec<_>>();
        caches[0].set(IcState {
            recv_shape: 23,
            holder_shape: 0,
            slot: 0,
            depth: IC_CREATE,
            mid_ok: 0,
            mid_shape: 0,
            mid2_shape: 0,
        });

        refresh_current_layout_feedback(&feedback, &shapes, &caches);
        let site = feedback.sites().next().unwrap().0;
        let access = feedback.read(
            site,
            ObservationKind::PropertyAccess,
            ObservationRole::Access,
        );
        assert_eq!(access.state(), ObservationState::Monomorphic);
        assert_eq!(access.payload(), 0);
        assert_eq!(
            access.flags(),
            property_access_flags(PropertyOutcome::Created, 0, false)
        );
        assert_eq!(
            feedback
                .read(site, ObservationKind::HolderLayout, ObservationRole::Holder)
                .state(),
            ObservationState::Absent
        );
        assert_eq!(&*shapes.borrow(), &[23]);
    }

    #[test]
    fn profiled_named_get_records_accessor_at_the_canonical_lookup() {
        let mut interp = Interp::new();
        let object = Value::Obj(interp.new_object());
        let getter = interp.new_native_fn("get value", 0, Rc::new(|_, _, _| Ok(Value::Num(17.0))));
        interp.define_accessor_value(&object, "value", Some(getter), None, true);
        let caches = (0..PROP_IC_WAYS)
            .map(|_| std::cell::Cell::new(IcState::EMPTY))
            .collect::<Vec<_>>();
        let mut trace = crate::feedback::CurrentPropertyTrace::default();

        let result = match interp.get_prop_ic_profiled(&object, "value", &caches[0], &mut trace) {
            Ok(value) => value,
            Err(_) => panic!("getter must complete"),
        };

        assert!(matches!(result, Value::Num(17.0)));
        assert_eq!(trace.outcome, Some(PropertyOutcome::Accessor));
        assert_eq!(trace.depth, 0);
        assert_eq!(trace.field_slot, None);
        assert_eq!(
            trace.holder_shape,
            object.as_obj().map(|object| object.borrow().props.shape())
        );
        assert!(caches.iter().all(|cache| cache.get().depth == IC_EMPTY));
    }

    #[test]
    fn profiled_named_get_classifies_primitive_virtual_properties_as_exotic() {
        let mut interp = Interp::new();
        let caches = (0..PROP_IC_WAYS)
            .map(|_| std::cell::Cell::new(IcState::EMPTY))
            .collect::<Vec<_>>();
        let mut trace = crate::feedback::CurrentPropertyTrace::default();

        let result =
            match interp.get_prop_ic_profiled(&Value::str("abc"), "length", &caches[0], &mut trace)
            {
                Ok(value) => value,
                Err(_) => panic!("string length must complete"),
            };

        assert!(matches!(result, Value::Num(3.0)));
        assert_eq!(trace.outcome, Some(PropertyOutcome::Exotic));
        assert_eq!(trace.depth, 0);
    }

    #[test]
    fn profiled_named_access_classifies_proxy_forwarding_as_exotic() {
        let mut interp = Interp::new();
        let global = interp.global_this();
        let proxy = match interp.eval_in_realm(&global, "new Proxy({ value: 3 }, {})") {
            Ok(value) => value,
            Err(_) => panic!("proxy construction must complete"),
        };
        let name: Rc<str> = Rc::from("value");
        let caches = (0..PROP_IC_WAYS)
            .map(|_| std::cell::Cell::new(IcState::EMPTY))
            .collect::<Vec<_>>();
        let mut get_trace = crate::feedback::CurrentPropertyTrace::default();

        let result = match interp.get_prop_ic_profiled(&proxy, &name, &caches[0], &mut get_trace) {
            Ok(value) => value,
            Err(_) => panic!("forwarded proxy get must complete"),
        };
        assert!(matches!(result, Value::Num(3.0)));
        assert_eq!(get_trace.outcome, Some(PropertyOutcome::Exotic));

        let mut set_trace = crate::feedback::CurrentPropertyTrace::default();
        assert!(interp
            .set_prop_ic_profiled(&proxy, &name, Value::Num(4.0), &caches[0], &mut set_trace,)
            .is_ok());
        assert_eq!(set_trace.outcome, Some(PropertyOutcome::Exotic));
        assert!(matches!(
            interp.get_member(&proxy, "value"),
            Ok(Value::Num(4.0))
        ));
    }

    #[test]
    fn profiled_named_set_invokes_a_setter_once_and_records_accessor() {
        let mut interp = Interp::new();
        let object = Value::Obj(interp.new_object());
        let calls = Rc::new(std::cell::Cell::new(0_u32));
        let setter_calls = calls.clone();
        let setter = interp.new_native_fn(
            "set value",
            1,
            Rc::new(move |_, _, _| {
                setter_calls.set(setter_calls.get() + 1);
                Ok(Value::Undefined)
            }),
        );
        interp.define_accessor_value(&object, "value", None, Some(setter), true);
        let name: Rc<str> = Rc::from("value");
        let caches = (0..PROP_IC_WAYS)
            .map(|_| std::cell::Cell::new(IcState::EMPTY))
            .collect::<Vec<_>>();
        let mut trace = crate::feedback::CurrentPropertyTrace::default();

        assert!(interp
            .set_prop_ic_profiled(&object, &name, Value::Num(9.0), &caches[0], &mut trace,)
            .is_ok());

        assert_eq!(calls.get(), 1);
        assert_eq!(trace.outcome, Some(PropertyOutcome::Accessor));
        assert_eq!(trace.depth, 0);
        assert!(caches.iter().all(|cache| cache.get().depth == IC_EMPTY));
    }

    #[test]
    fn profiled_named_set_records_read_only_rejection() {
        let mut interp = Interp::new();
        let object = Value::Obj(interp.new_object());
        object.as_obj().unwrap().borrow_mut().props.insert(
            "value",
            crate::value::Property::data(Value::Num(1.0), false, true, true),
        );
        let name: Rc<str> = Rc::from("value");
        let caches = (0..PROP_IC_WAYS)
            .map(|_| std::cell::Cell::new(IcState::EMPTY))
            .collect::<Vec<_>>();
        let mut trace = crate::feedback::CurrentPropertyTrace::default();

        assert!(interp
            .set_prop_ic_profiled(&object, &name, Value::Num(9.0), &caches[0], &mut trace,)
            .is_ok());

        assert_eq!(trace.outcome, Some(PropertyOutcome::Rejected));
        assert!(matches!(
            interp.get_member(&object, "value"),
            Ok(Value::Num(1.0))
        ));
    }

    #[test]
    fn profiled_element_helpers_preserve_array_holes_and_typedarray_exotics() {
        let mut interp = Interp::new();
        let global = interp.global_this();
        let array = interp
            .eval_in_realm(&global, "[1,,3]")
            .unwrap_or_else(|_| panic!("array construction must complete"));
        assert_eq!(
            interp.element_receiver_kind(&array),
            ElementReceiverKind::Array
        );
        assert_eq!(interp.element_key_kind("1"), ElementKeyKind::Index);
        let mut trace = crate::feedback::CurrentPropertyTrace::default();
        let value = interp
            .get_member_profiled(&array, "1", &mut trace)
            .unwrap_or_else(|_| panic!("array hole read must complete"));
        assert!(matches!(value, Value::Undefined));
        assert_eq!(trace.outcome, Some(PropertyOutcome::Absent));
        assert_eq!(
            match trace.outcome {
                Some(PropertyOutcome::Absent) => ElementOutcome::Hole,
                _ => panic!("expected an absent property trace"),
            },
            ElementOutcome::Hole
        );

        let typed = interp
            .eval_in_realm(&global, "new Uint8Array(2)")
            .unwrap_or_else(|_| panic!("typed array construction must complete"));
        assert_eq!(
            interp.element_receiver_kind(&typed),
            ElementReceiverKind::TypedArray
        );
        assert_eq!(
            interp.element_key_kind("-0"),
            ElementKeyKind::CanonicalNumeric
        );
        let mut typed_trace = crate::feedback::CurrentPropertyTrace::default();
        let value = interp
            .get_member_profiled(&typed, "-0", &mut typed_trace)
            .unwrap_or_else(|_| panic!("canonical numeric typed-array read must complete"));
        assert!(matches!(value, Value::Undefined));
        assert_eq!(typed_trace.outcome, Some(PropertyOutcome::Exotic));
    }

    #[test]
    fn call_adapter_classifies_callable_families_without_object_identity() {
        let mut interp = Interp::new();
        let global = interp.global_this();
        let user = interp
            .eval_in_realm(&global, "(function user() {})")
            .unwrap_or_else(|_| panic!("user function construction must complete"));
        let bound = interp
            .eval_in_realm(&global, "(function target() {}).bind(null)")
            .unwrap_or_else(|_| panic!("bound function construction must complete"));
        let proxy = interp
            .eval_in_realm(&global, "new Proxy(function target() {}, {})")
            .unwrap_or_else(|_| panic!("proxy function construction must complete"));
        let native = interp.new_native_fn("native", 0, Rc::new(|_, _, _| Ok(Value::Undefined)));
        let ordinary = Value::Obj(interp.new_object());

        assert_eq!(interp.call_target_kind(&user), CallTargetKind::UserFunction);
        assert_eq!(
            interp.call_target_kind(&native),
            CallTargetKind::NativeFunction
        );
        assert_eq!(
            interp.call_target_kind(&bound),
            CallTargetKind::BoundFunction
        );
        assert_eq!(interp.call_target_kind(&proxy), CallTargetKind::Proxy);
        assert_eq!(
            interp.call_target_kind(&ordinary),
            CallTargetKind::NonCallable
        );
        assert_eq!(
            interp.call_target_kind(&Value::Num(1.0)),
            CallTargetKind::NonCallable
        );
        assert!(matches!(
            interp.call_environment_kind(&user),
            CallEnvironmentKind::None | CallEnvironmentKind::Unknown
        ));
        assert_eq!(
            interp.call_environment_kind(&bound),
            CallEnvironmentKind::Dynamic
        );
        assert_eq!(
            interp.call_environment_kind(&proxy),
            CallEnvironmentKind::Dynamic
        );
    }

    #[test]
    fn throwing_profiled_setter_is_not_replayed_by_observation() {
        let mut interp = Interp::new();
        let object = Value::Obj(interp.new_object());
        let calls = Rc::new(std::cell::Cell::new(0_u32));
        let setter_calls = calls.clone();
        let setter = interp.new_native_fn(
            "set value",
            1,
            Rc::new(move |_, _, _| {
                setter_calls.set(setter_calls.get() + 1);
                Err(Value::str("setter failed"))
            }),
        );
        interp.define_accessor_value(&object, "value", None, Some(setter), true);
        let name: Rc<str> = Rc::from("value");
        let caches = (0..PROP_IC_WAYS)
            .map(|_| std::cell::Cell::new(IcState::EMPTY))
            .collect::<Vec<_>>();
        let mut trace = crate::feedback::CurrentPropertyTrace::default();

        assert!(interp
            .set_prop_ic_profiled(&object, &name, Value::Num(9.0), &caches[0], &mut trace,)
            .is_err());

        assert_eq!(calls.get(), 1);
        assert_eq!(trace.outcome, Some(PropertyOutcome::Accessor));
    }
}

/// Compile `func` whole, or `None` if it uses anything outside the v0 subset.
pub fn compile(func: &Function) -> Option<Rc<Chunk>> {
    let started = crate::jit::perf_stage_start();
    let result = compile_inner(func, &Default::default(), None, None, false);
    crate::jit::perf_bytecode_compile_end(started, result.is_some());
    result
}

/// Compile a derived class constructor against a retained Function Environment Record.
///
/// Unlike an ordinary lean frame, that environment starts with an uninitialized `this` binding
/// and carries the active constructor and `new.target`; `super()` binds `this` and initializes the
/// derived class's instance elements through the shared ECMA-262 algorithm.
pub(crate) fn compile_derived_constructor(func: &Function) -> Option<Rc<Chunk>> {
    let started = crate::jit::perf_stage_start();
    let result = compile_inner(func, &Default::default(), None, None, true);
    crate::jit::perf_bytecode_compile_end(started, result.is_some());
    result
}

/// Compile a Source Text Module's already-instantiated body as a strict heap continuation.
/// `bindings` are the module environment's own, non-import bindings; unlike an ordinary async
/// function, the chunk must read and initialize those exact cells so exports stay live and TDZ
/// state established during ModuleDeclarationInstantiation remains authoritative.
pub(crate) fn compile_module(body: &[Stmt], bindings: &[(String, bool)]) -> Option<Rc<Chunk>> {
    let function = Function {
        name: None,
        params: Vec::new(),
        body: body.to_vec(),
        is_arrow: false,
        is_strict: true,
        expr_body: false,
        is_generator: false,
        is_async: true,
        is_method: false,
        is_fn_expr: false,
        source: None,
        scan: std::cell::Cell::new(0),
        hoist: std::cell::OnceCell::new(),
        calls: std::cell::Cell::new(0),
        code: std::cell::OnceCell::new(),
        code2: std::cell::OnceCell::new(),
        fn_maps: std::cell::OnceCell::new(),
    };
    let started = crate::jit::perf_stage_start();
    let result = compile_inner(&function, &Default::default(), None, Some(bindings), false);
    crate::jit::perf_bytecode_compile_end(started, result.is_some());
    result
}

/// Second-stage compile: same as [`compile`], with hot monomorphic callees from `plan` spliced
/// inline at their call sites (guarded; see [`plan_inlines`]).
pub(crate) fn compile_with_inlines(
    func: &Function,
    plan: &crate::fasthash::FastMap<u32, InlinePlanEntry>,
    hot: &Chunk,
) -> Option<Rc<Chunk>> {
    let seed = std::env::var_os("LUMEN_JIT_NO_CACHE_SEED")
        .is_none()
        .then_some(hot);
    let started = crate::jit::perf_stage_start();
    let result = compile_inner(func, plan, seed, None, false);
    crate::jit::perf_bytecode_compile_end(started, result.is_some());
    result
}

fn property_cache_seeds(chunk: &Chunk) -> Vec<(Rc<str>, [IcState; PROP_IC_WAYS])> {
    chunk
        .ops
        .iter()
        .filter_map(|op| {
            let (name, cache) = match *op {
                Op::GetProp(n, c)
                | Op::GetPropThis(n, c)
                | Op::SetProp(n, c)
                | Op::SetPropDrop(n, c)
                | Op::SetPropThisDrop(n, c)
                | Op::AppendProp(n, c)
                | Op::GetMethod(n, c) => (n, c),
                Op::GetPropLocal(_, n, c) | Op::SetPropLocalDrop(_, n, c) => (n, c),
                Op::UpdateProp(n, c, _) => (n, c),
                _ => return None,
            };
            let states = std::array::from_fn(|way| chunk.caches[cache as usize + way].get());
            Some((chunk.names[name as usize].clone(), states))
        })
        .collect()
}

type NamePin = Option<std::rc::Weak<std::cell::RefCell<crate::interpreter::Scope>>>;
type NameSeed = (Rc<str>, NameIc, NamePin, Option<u64>);
type CallPin = std::rc::Weak<std::cell::RefCell<crate::value::Object>>;

struct CallSeed {
    entries: [CallIc; CALL_IC_WAYS],
    next: u8,
    pins: Vec<(usize, CallPin)>,
}

fn name_cache_seeds(chunk: &Chunk) -> Vec<NameSeed> {
    let pins = chunk.name_pins.borrow();
    chunk
        .ops
        .iter()
        .filter_map(|op| {
            let (name, cache) = match *op {
                Op::LoadName(n, c)
                | Op::LoadNameForCall(n, c)
                | Op::StoreNameCached(n, c)
                | Op::UpdateNameCached(n, c, _) => (n, c as usize),
                _ => return None,
            };
            let number = chunk.name_num_valid[cache]
                .get()
                .then(|| chunk.name_num_bits[cache].get());
            Some((
                chunk.names[name as usize].clone(),
                chunk.name_caches[cache].get(),
                pins[cache].clone(),
                number,
            ))
        })
        .collect()
}

fn call_cache_seeds(chunk: &Chunk) -> Vec<CallSeed> {
    let pins = chunk.call_pins.borrow();
    chunk
        .ops
        .iter()
        .filter_map(|op| {
            let cache = match *op {
                Op::Call(_, cache) | Op::CallWithThis(_, cache) => cache as usize,
                _ => return None,
            };
            let site = chunk.call_caches.get(cache)?;
            let entries = std::array::from_fn(|way| {
                let entry = site.entries[way].get();
                if entry.callee == 0 || pins.contains_key(&entry.callee) {
                    entry
                } else {
                    CallIc::EMPTY
                }
            });
            let mut seed_pins = Vec::new();
            for entry in entries {
                if entry.callee == 0 || seed_pins.iter().any(|(callee, _)| *callee == entry.callee)
                {
                    continue;
                }
                if let Some(pin) = pins.get(&entry.callee) {
                    seed_pins.push((entry.callee, pin.clone()));
                }
            }
            Some(CallSeed {
                entries,
                next: site.next.get(),
                pins: seed_pins,
            })
        })
        .collect()
}

fn compile_inner(
    func: &Function,
    plan: &crate::fasthash::FastMap<u32, InlinePlanEntry>,
    hot: Option<&Chunk>,
    module_bindings: Option<&[(String, bool)]>,
    derived_constructor: bool,
) -> Option<Rc<Chunk>> {
    // Body facts the scanner already knows: `new.target` is an observation channel into the
    // activation that slots do not provide; `this` / `arguments` in an ordinary arrow are free
    // variables we do not model. Parameterless synchronous ordinary functions can materialize an
    // unmapped arguments object into a dedicated slot (the common variadic-helper shape).
    let scan = func.scan_flags();
    let is_coroutine = func.is_generator || func.is_async;
    // ECMA-262 §9.4.5 GetNewTarget reads the nearest this-binding Function Environment Record.
    // Coroutine calls have already materialized that activation before this chunk is created and
    // retain it for every resumption, including as the lexical parent of async arrows. A compiled
    // derived constructor likewise runs under its required Function Environment Record. Other
    // lean ordinary frames still omit the activation, so keep their conservative exclusion.
    if scan & SCAN_NEW_TARGET != 0 && !is_coroutine && !derived_constructor {
        log_bail("fn", "new.target");
        return None;
    }
    let uses_arguments = scan & SCAN_ARGUMENTS != 0;
    // ECMA-262 §10.2.11 creates a mapped arguments object only for a non-strict, non-arrow
    // function with a simple parameter list. A nonempty mapped list must alias parameter writes;
    // the current coroutine slot projection cannot yet preserve that relationship.
    let has_mapped_parameter_aliases = uses_arguments
        && !func.is_arrow
        && !func.is_strict
        && !func.params.is_empty()
        && func.params.iter().all(|param| {
            !param.rest && param.default.is_none() && matches!(param.pattern, Pattern::Ident(_))
        })
        && !func
            .params
            .iter()
            .any(|param| matches!(&param.pattern, Pattern::Ident(name) if name == "arguments"));
    if uses_arguments && !is_coroutine && (func.is_arrow || !func.params.is_empty()) {
        log_bail("fn", "arguments with arrow/async/parameters");
        return None;
    }
    // Arrow functions do not bind `this`; they resolve it through their captured lexical
    // environment (ECMA-262 §15.3.4). Async-arrow setup retains that chain and seeds the VM
    // frame from its resolved value, including after the defining call has returned. Ordinary
    // lean arrows still skip the observable activation machinery and remain conservative.
    if func.is_arrow && scan & SCAN_THIS != 0 && !is_coroutine {
        log_bail("fn", "arrow reading this");
        return None;
    }
    // `yield`, `yield*`, and `await` lower to explicit VmCoro suspension points. Constructs the
    // flat bytecode compiler cannot preserve still leave the entire function on the tree-walker.
    // ECMA-262 Instantiate{Generator,Async}FunctionExpression creates a declarative environment
    // outside the call activation, initializes its immutable self-name to the closure, and makes
    // the closure capture it. Coroutine setup retains that exact environment chain, so unresolved
    // Name operations correctly reach the self-binding (while a body `var` of the same name gets
    // its own nearer home). Ordinary lean frames still bypass this setup and must stay excluded.
    if func.is_fn_expr && func.name.is_some() && !is_coroutine {
        log_bail("fn", "named function expression");
        return None;
    }
    // Capture analysis: which locals inner functions can name (they live in a real activation
    // env), and whether an inner arrow chain reads `this`. `None` = unanalyzable — bail.
    let Some((captured, env_this, block_lets, runtime_lexicals, direct_eval)) =
        CaptureScan::run(func, is_coroutine)
    else {
        let head: String = func
            .source
            .as_deref()
            .unwrap_or("<no source>")
            .chars()
            .take(90)
            .collect();
        log_bail(
            "capture-scan",
            &format!(
                "unanalyzable body (eval/with/annexB/pattern) in: {}",
                head.replace('\n', " ")
            ),
        );
        return None;
    };

    let mut c = Compiler {
        // A module already has its own `this` binding, initialized to undefined. Reuse that
        // environment for nested arrows instead of synthesizing a function activation.
        // A derived constructor already runs under its mandatory Function Environment Record;
        // arrows must close over that live, initially-uninitialized `this` binding instead of a
        // child activation seeded with the entry-time placeholder.
        env_this: env_this && module_bindings.is_none() && !derived_constructor,
        lexical_this: func.is_arrow,
        derived_constructor,
        strict: func.is_strict,
        is_coroutine,
        module_body: module_bindings.is_some(),
        direct_eval,
        runtime_lexicals,
        reuse_activation: has_mapped_parameter_aliases,
        plan_stack: vec![(plan.clone(), 0)],
        cache_seed_stack: hot
            .map(|chunk| vec![(property_cache_seeds(chunk), 0)])
            .unwrap_or_default(),
        name_seed_stack: hot
            .map(|chunk| vec![(name_cache_seeds(chunk), 0)])
            .unwrap_or_default(),
        call_seed_stack: hot
            .map(|chunk| vec![(call_cache_seeds(chunk), 0)])
            .unwrap_or_default(),
        ..Compiler::default()
    };
    if let Some(bindings) = module_bindings {
        for (name, is_const) in bindings {
            c.env_bind(name, *is_const);
        }
        // Any compiler-homed block captures may safely use the once-only module environment.
        // More importantly, a fresh function activation would put pre-instantiated module cells
        // in the parent while `LoadCap`/`StoreCap` intentionally address the fixed home directly.
        c.reuse_activation = true;
    }
    if uses_arguments {
        if has_mapped_parameter_aliases {
            c.env_bind("arguments", false);
        } else if !is_coroutine {
            if captured.contains("arguments") {
                // ECMA-262 §15.3.4: an arrow has no own arguments binding. Materialize the
                // enclosing function's one arguments object in its activation so both the outer
                // body and every captured arrow resolve the same identity.
                c.env_bind("arguments", false);
                c.env_arguments = true;
            } else {
                let slot = c.fresh_slot("arguments");
                c.scope_bind("arguments", slot, false);
                c.arguments_slot = Some(slot);
            }
        }
        // Other coroutines deliberately leave `arguments` unresolved in the chunk. The normal
        // name operation walks the retained activation installed by FunctionDeclarationInstantiation,
        // or its parents for an arrow, so direct reads and nested arrows observe one object.
    }
    // Captured once-per-call block `let`s home in the activation (TDZ from entry, initialized
    // by the declaring block's own StoreCapInit); CaptureScan proved no enclosing same-name
    // declaration and block-resolved references only, so the function-flat env map is
    // faithful (nested same-name declarations shadow it through their slots).
    for (name, is_const) in &block_lets {
        c.cap_inits
            .push(CapInit::Lexical(Rc::from(name.as_str()), *is_const));
        c.env_bind(name, *is_const);
        c.homed_lets.insert(name.clone());
        c.homed_pending.insert(name.clone());
    }
    // Ordinary compiled calls instantiate parameters in this chunk, so each formal needs its
    // positional slot and only the subset whose default initialization is safely lowerable can
    // compile. Coroutine calls have already completed FunctionDeclarationInstantiation in the
    // tree-walker before their resumable context is created. For them, flatten BoundNames into
    // slots and seed those slots from the live bindings; this admits rest/destructuring/default
    // parameter lists without replaying any binding or initializer operation.
    let mut defaulted: Vec<(u16, &Expr)> = Vec::new();
    if func.is_generator || func.is_async {
        let bound_names = crate::interpreter::param_bound_names(&func.params);
        for (k, name) in bound_names.iter().enumerate() {
            let slot = c.fresh_slot(name);
            if has_mapped_parameter_aliases {
                // The arguments exotic object's ParameterMap already aliases this binding in the
                // retained call activation. Keep all direct and captured accesses there too.
                c.env_bind(name, false);
            } else if captured.contains(name) {
                c.cap_inits
                    .push(CapInit::Param(k as u16, Rc::from(name.as_str())));
                c.env_bind(name, false);
            } else {
                c.scope_bind(name, slot, false);
            }
        }
        c.n_params = bound_names.len();
    } else {
        for (k, p) in func.params.iter().enumerate() {
            if p.rest {
                log_bail("params", "rest parameter");
                return None;
            }
            let Pattern::Ident(name) = &p.pattern else {
                log_bail("params", "destructuring parameter");
                return None;
            };
            if let Some(d) = &p.default {
                // Lowerable defaults: an uncaptured identifier parameter whose default expression
                // can't observe this-or-later parameters (see `default_expr_safe`).
                if captured.contains(name) {
                    log_bail("params", "captured defaulted parameter");
                    return None;
                }
                let banned: std::collections::HashSet<&str> = func.params[k..]
                    .iter()
                    .filter_map(|q| match &q.pattern {
                        Pattern::Ident(n) => Some(n.as_str()),
                        _ => None,
                    })
                    .collect();
                if !default_expr_safe(d, &banned) {
                    log_bail("params", "unsafe default expression");
                    return None;
                }
                defaulted.push((k as u16, d));
            }
            let slot = c.fresh_slot(name);
            if captured.contains(name) {
                c.cap_inits
                    .push(CapInit::Param(k as u16, Rc::from(name.as_str())));
                c.env_bind(name, false);
            } else {
                c.scope_bind(name, slot, false);
            }
        }
        c.n_params = func.params.len();
        for (slot, d) in defaulted {
            c.emit(Op::LoadLocal(slot));
            c.emit(Op::Undef);
            c.emit(Op::StrictEq);
            let jf = c.emit(Op::JumpIfFalse(0));
            c.expr(d).ok()?;
            c.emit(Op::StoreLocal(slot));
            c.patch(jf);
        }
    }
    // Function-scoped `var`s and hoisted function declarations from the shared hoist plan.
    let mut annexb_blocked = crate::interpreter::param_bound_names(&func.params);
    if !func.is_arrow {
        annexb_blocked.push("arguments".to_string());
    }
    for op in crate::interpreter::collect_hoist_ops(&func.body, func.is_strict, &annexb_blocked) {
        match op {
            HoistOp::Var(name) => {
                if c.env_has(&name) {
                    // A `var` sharing a parameter's Function Environment binding is already
                    // instantiated. In particular, do not split mapped parameters into slots.
                } else if captured.contains(&name) {
                    if !c.env_has(&name) {
                        c.cap_inits.push(CapInit::Var(Rc::from(name.as_str())));
                        c.env_bind(&name, false);
                    }
                } else if c.lookup(&name).is_none() {
                    let slot = c.fresh_slot(&name);
                    c.scope_bind(&name, slot, false);
                }
            }
            HoistOp::Fn(name, f) => {
                if c.module_body && c.env_has(&name) {
                    // ModuleDeclarationInstantiation already created the closure in this exact
                    // environment. Replaying FunctionDeclarationInstantiation would replace the
                    // live export cell and, for cycles, expose the wrong function identity.
                    continue;
                }
                let fidx = c.funcs.len() as u16;
                c.funcs.push(f.clone());
                if c.env_has(&name) || captured.contains(&name) {
                    c.cap_inits.push(CapInit::Fn(fidx, Rc::from(name.as_str())));
                    c.env_bind(&name, false);
                } else {
                    let slot = match c.lookup(&name) {
                        Some((s, _)) => s,
                        None => {
                            let s = c.fresh_slot(&name);
                            c.scope_bind(&name, s, false);
                            s
                        }
                    };
                    // Created at entry, in hoist order, closing over the activation.
                    c.emit(Op::MakeClosure(fidx as u32, u32::MAX));
                    c.emit(Op::StoreLocal(slot));
                }
            }
            HoistOp::AnnexB(name, function) => {
                // Annex B.3.2 FunctionDeclarationInstantiation creates/reuses a mutable var home
                // initialized to undefined. The block's distinct lexical binding is instantiated
                // later; evaluating its declaration copies that function object into this home.
                let target = if let Some(home) = c.home(&name) {
                    home
                } else if captured.contains(&name) {
                    c.cap_inits.push(CapInit::Var(Rc::from(name.as_str())));
                    c.env_bind(&name, false);
                    Home::Env(false)
                } else {
                    let slot = c.fresh_slot(&name);
                    c.scope_bind(&name, slot, false);
                    Home::Slot(slot, false)
                };
                c.annexb_targets
                    .insert(Rc::as_ptr(&function) as usize, target);
            }
        }
    }
    // Body-level lexicals: captured ones home in the activation (inserted in TDZ by
    // make_run_env), the rest get TDZ slots.
    if c.declare_body_lexicals(&func.body, &captured).is_err() {
        log_bail("body-lexicals", "unsupported declaration form");
        return None;
    }
    let compile_body = |compiler: &mut Compiler| -> CResult {
        for stmt in &func.body {
            if compiler.stmt(stmt).is_err() {
                log_bail("stmt-in", &format!("{:.80}", format!("{stmt:?}")));
                return Err(Bail);
            }
        }
        Ok(())
    };
    let has_body_using = func.body.iter().any(|statement| {
        matches!(
            statement,
            Stmt::VarDecl {
                kind: DeclKind::Using | DeclKind::AwaitUsing,
                ..
            }
        )
    });
    let body_result = if has_body_using {
        c.disposal_scope(compile_body)
    } else {
        compile_body(&mut c)
    };
    if body_result.is_err() {
        return None;
    }
    c.emit(Op::ReturnUndef);
    // Constructors in OO workloads overwhelmingly initialize a short, straight-line list of
    // fields. Count distinct names only until control flow can make the estimate speculative,
    // and decline large reservations: this is an allocation/memory optimization, not metadata
    // proportional to object count.
    let mut instance_names = [u32::MAX; 16];
    let mut instance_capacity_hint = 0u8;
    for op in &c.ops {
        match op {
            Op::SetPropThisDrop(name, _) => {
                if !instance_names[..instance_capacity_hint as usize].contains(name) {
                    if instance_capacity_hint == instance_names.len() as u8 {
                        instance_capacity_hint = 0;
                        break;
                    }
                    instance_names[instance_capacity_hint as usize] = *name;
                    instance_capacity_hint += 1;
                }
            }
            Op::Jump(..)
            | Op::AbruptJump(..)
            | Op::JumpIfFalse(..)
            | Op::JumpIfFalsePeek(..)
            | Op::JumpIfTruePeek(..)
            | Op::JumpIfNotNullishPeek(..)
            | Op::InlineGuard(..)
            | Op::Throw
            | Op::Return
            | Op::ReturnBare
            | Op::ResumeReturn
            | Op::ResumeJump
            | Op::ReturnUndef
            | Op::Await
            | Op::Yield
            | Op::PushHandler(..)
            | Op::PushFinally(..)
            | Op::PushIterator(..) => break,
            Op::PushDisposeFrame
            | Op::AddDisposable(_)
            | Op::DisposeNormal
            | Op::DisposeThrow
            | Op::DisposeReturn
            | Op::DisposeBareReturn
            | Op::DisposeResumeReturn
            | Op::DisposeJump
            | Op::PushWith
            | Op::PushLex(_)
            | Op::PushCatchLex(_)
            | Op::CloneLex(_)
            | Op::InitLex(_)
            | Op::PopEnv
            | Op::ResolveNameRef(..)
            | Op::LoadRef(_)
            | Op::StoreRef(_) => break,
            _ => {}
        }
    }
    // A speculative inline can allocate child call-cache seeds and then roll its op/cache
    // vectors back. Keep only Weak pins referenced by the surviving fixed-size sites so failed
    // speculation neither retains dead Rc allocation blocks nor consumes the runtime pin budget.
    c.call_pins.retain(|callee, _| {
        c.call_caches.iter().any(|site| {
            site.entries
                .iter()
                .any(|entry| entry.get().callee == *callee)
        })
    });
    let cap_cache_len = c.names.len();
    let op_count = c.ops.len();
    let feedback = if let Some(chunk) = hot {
        // The transformed/inlined bytecode has different PCs. Retain the baseline schema without
        // guessing new runtime bindings; the original chunk remains the current-shape adapter.
        crate::feedback::FeedbackVector::unbound(chunk.feedback.layout().clone())
    } else {
        let (layout, bindings) = feedback_layout_for_ops(&c.ops, &c.names);
        crate::feedback::FeedbackVector::new(layout, bindings)
    };
    Some(Rc::new(Chunk {
        ops: c.ops,
        consts: c.consts,
        names: c.names,
        n_slots: c.slot_names.len(),
        slot_names: c.slot_names,
        n_refs: c.n_refs,
        n_params: c.n_params,
        arguments_slot: c.arguments_slot,
        uses_this: c.uses_this,
        lexical_this: c.lexical_this,
        strict: c.strict,
        instance_capacity_hint,
        forwarded_capacity_hint: std::cell::Cell::new(0),
        funcs: c.funcs,
        templates: c.templates,
        eval_exprs: c.eval_exprs,
        class_plans: c.class_plans,
        assignment_targets: c.assignment_targets,
        lexical_scopes: c.lexical_scopes,
        cap_inits: c.cap_inits,
        reuse_activation: c.reuse_activation,
        env_this: c.env_this,
        env_arguments: c.env_arguments,
        feedback,
        feedback_shapes: std::cell::RefCell::new(Vec::new()),
        obj_maps: (0..c.obj_maps)
            .map(|_| std::cell::OnceCell::new())
            .collect(),
        caches: c.caches,
        name_pins: std::cell::RefCell::new(c.name_pins),
        name_caches: c.name_caches,
        name_num_bits: c.name_num_bits,
        name_num_valid: c.name_num_valid,
        cap_caches: vec![std::cell::Cell::new(NameIc::EMPTY); cap_cache_len],
        cap_pins: std::cell::RefCell::new(vec![None; cap_cache_len]),
        initializer_plan: std::cell::OnceCell::new(),
        simple_constructor_shapes: std::cell::Cell::new(InitializerShapes::default()),
        simple_constructor_plan: std::cell::OnceCell::new(),
        arguments_forwarder: std::cell::OnceCell::new(),
        arguments_forwarder_runtime: std::cell::RefCell::new(None),
        regexp_literals: (0..op_count).map(|_| std::cell::OnceCell::new()).collect(),
        call_caches: c.call_caches,
        construct_caches: c.construct_caches,
        call_pins: std::cell::RefCell::new(c.call_pins),
        inline_targets: c.inline_targets,
        jit_runs: std::cell::Cell::new(0),
        inline_attempted: std::cell::Cell::new(false),
        jit: std::cell::OnceCell::new(),
    }))
}

/// How many machine-code runs of a chunk trigger the one-shot speculative inline recompile.
pub(crate) fn inline_recompile_at() -> u32 {
    static AT: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *AT.get_or_init(|| {
        std::env::var("LUMEN_INLINE_AT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100)
    })
}

/// Whether the ARM64 JIT may enter another compiled function through the shared activation.
/// This is a diagnostic kill switch, not a tier choice: ordinary layered JIT calls remain active
/// when it is disabled.
pub(crate) fn direct_shared_context_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("LUMEN_JIT_NO_DIRECT_CALLS").is_none())
}

/// Build the speculative-inline plan for a hot chunk: for each monomorphic, filled call site,
/// the callee qualifies when it is a plain same-strictness function whose compiled body is
/// small, needs no activation environment, touches no free names, and hides no control-flow
/// the splice can't reproduce (handlers, closures). The plan keys are the sites' `CallIc`
/// indices, which equal the second compile's caller-level site ordinals (same AST, same
/// emission order).
pub(crate) fn plan_inlines(
    chunk: &Chunk,
    caller: &Function,
    global_env: &crate::interpreter::Env,
    caller_env: *const std::cell::RefCell<crate::interpreter::Scope>,
) -> crate::fasthash::FastMap<u32, InlinePlanEntry> {
    // Bound the *whole* optimized body rather than stopping after one arbitrary nesting level.
    // OO hot loops tend to be call chains (dispatcher -> virtual method -> small scheduler
    // helper); a depth-one cap leaves the most valuable dispatch intact.  The shared source-op
    // budget prevents four-way polymorphic sites from growing exponentially.  It is deliberately
    // conservative: an inline property op can expand to substantially more machine code than a
    // simple arithmetic op.
    const INLINE_SOURCE_OP_BUDGET: usize = 320;
    let limit = std::env::var("LUMEN_INLINE_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(INLINE_SOURCE_OP_BUDGET);
    let mut budget = limit.saturating_sub(chunk.ops.len());
    plan_inlines_at(chunk, caller, global_env, caller_env, 0, &mut budget)
}

fn plan_inlines_at(
    chunk: &Chunk,
    caller: &Function,
    global_env: &crate::interpreter::Env,
    caller_env: *const std::cell::RefCell<crate::interpreter::Scope>,
    depth: u32,
    budget: &mut usize,
) -> crate::fasthash::FastMap<u32, InlinePlanEntry> {
    const INLINE_MAX_DEPTH: u32 = 3;
    const INLINE_MAX_WAYS: usize = 4;
    let max_ops = std::env::var("LUMEN_INLINE_MAX_OPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(96);
    let mut plan: crate::fasthash::FastMap<u32, InlinePlanEntry> = Default::default();
    let log = std::env::var_os("LUMEN_TIER_LOG").is_some();
    macro_rules! skip {
        ($idx:expr, $why:expr) => {{
            if log {
                eprintln!("[tier] inline skip site {}: {}", $idx, $why);
            }
            continue;
        }};
    }
    let pins = chunk.call_pins.borrow();
    for (idx, site) in chunk.call_caches.iter().enumerate() {
        let mut filled: Vec<CallIc> = site
            .entries
            .iter()
            .map(|e| e.get())
            .filter(|c| c.callee != 0)
            .collect();
        // Distinct callees (refills may duplicate one across ways).  Call ICs retain four ways,
        // so consume all four when the body budget permits: Richards' central task dispatcher is
        // exactly four-way polymorphic, and leaving half of it as real calls dominates runtime.
        filled.dedup_by_key(|c| c.callee);
        let mut ways: Vec<InlineWay> = Vec::new();
        for ic in filled.iter().take(INLINE_MAX_WAYS) {
            let Some(weak) = pins.get(&ic.callee) else {
                skip!(idx, "no pin")
            };
            let Some(obj) = weak.upgrade() else {
                skip!(idx, "dead callee")
            };
            let b = obj.borrow();
            let crate::value::Callable::User(user) = &b.call else {
                continue;
            };
            // Free names are spliceable when the callee closes directly over the global scope,
            // or when caller and callee close over the exact same activation. The latter is
            // guarded again in generated code because an optimized Chunk is shared by every
            // closure instance created from the same Function AST.
            let global_closure = Rc::ptr_eq(&user.env, global_env);
            let callee_env = Rc::as_ptr(&user.env);
            let shared_closure = !caller_env.is_null() && callee_env == caller_env;
            let f = &user.func;
            if f.is_arrow || f.is_strict != caller.is_strict {
                skip!(idx, "arrow/strictness");
            }
            if f.params.iter().any(|p| {
                p.rest || p.default.is_some() || !matches!(p.pattern, crate::ast::Pattern::Ident(_))
            }) {
                skip!(idx, "param shape");
            }
            let Some(Some(callee_chunk)) = f.code.get() else {
                skip!(idx, "callee not compiled");
            };
            if std::ptr::eq(&**callee_chunk, chunk) {
                skip!(idx, "self-recursion");
            }
            if callee_chunk.ops.len() > max_ops
                || callee_chunk.n_slots > 32
                || !callee_chunk.jit_no_activation()
                || callee_chunk.env_this
                || ic.n_params > 8
            {
                skip!(idx, "callee size/shape");
            }
            // The splice runs under the caller's frame: no handler regions to relocate, no inner
            // closures, no name writes. Free-name READS are allowed for global-closure callees —
            // the compiler re-resolves them at the splice site and refuses shadowed ones.
            if callee_chunk.ops.iter().any(|op| {
                matches!(
                    op,
                    Op::PushHandler(_)
                        | Op::PushFinally(..)
                        | Op::PushIterator(..)
                        | Op::PushDisposeFrame
                        | Op::AddDisposable(_)
                        | Op::DisposeNormal
                        | Op::DisposeThrow
                        | Op::DisposeReturn
                        | Op::DisposeBareReturn
                        | Op::DisposeResumeReturn
                        | Op::DisposeJump
                        | Op::PushWith
                        | Op::PushLex(_)
                        | Op::PushCatchLex(_)
                        | Op::CloneLex(_)
                        | Op::InitLex(_)
                        | Op::PopEnv
                        | Op::ResolveNameRef(..)
                        | Op::LoadRef(_)
                        | Op::StoreRef(_)
                        | Op::MakeClosure(..)
                        | Op::StoreName(_)
                        | Op::StoreNameCached(..)
                        | Op::UpdateName(..)
                        | Op::UpdateNameCached(..)
                )
            }) {
                skip!(idx, "callee ops (handlers/closures/name writes)");
            }
            let mut free_names: Vec<Rc<str>> = Vec::new();
            for op in callee_chunk.ops.iter() {
                if let Op::LoadName(n, _) | Op::LoadNameForCall(n, _) = op {
                    let name = callee_chunk.names[*n as usize].clone();
                    if !free_names.contains(&name) {
                        free_names.push(name);
                    }
                }
            }
            if !free_names.is_empty() && !global_closure && !shared_closure {
                skip!(idx, "free names in a non-global closure");
            }
            let inline_cost = callee_chunk.ops.len();
            if inline_cost > *budget {
                skip!(idx, "optimized-body budget");
            }
            *budget -= inline_cost;
            let uses_this = callee_chunk.uses_this();
            // Follow small hot call chains under the shared body budget.  This reaches through a
            // dispatcher into its virtual target and then into leaf helpers without allowing
            // unbounded recursive expansion.
            let nested = if depth < INLINE_MAX_DEPTH && *budget > 0 {
                plan_inlines_at(callee_chunk, f, global_env, callee_env, depth + 1, budget)
            } else {
                Default::default()
            };
            let f = f.clone();
            drop(b);
            ways.push(InlineWay {
                check_this: uses_this && !f.is_strict,
                uses_this,
                f,
                obj,
                free_names,
                expected_env: if shared_closure {
                    callee_env as usize
                } else {
                    0
                },
                nested,
            });
        }
        if ways.is_empty() {
            continue;
        }
        plan.insert(idx as u32, InlinePlanEntry { ways });
    }
    plan
}

#[derive(Default)]
struct Compiler {
    /// The compiled function's strictness (carried into ops whose runtime behavior forks on it).
    strict: bool,
    /// Generator/async chunks use the VM as their resumable execution context, not merely as an
    /// optimization tier. Some completion-aware lowerings are intentionally coroutine-only.
    is_coroutine: bool,
    /// Source Text Module execution uses bindings instantiated by the link phase rather than a
    /// fresh FunctionDeclarationInstantiation environment.
    module_body: bool,
    /// This coroutine contains direct eval and CaptureScan proved every dynamically visible
    /// depth-0 binding has an exact retained activation or runtime lexical-environment home.
    direct_eval: bool,
    /// Inner lexical names whose closure- or eval-visible bindings must live in the resumable
    /// environment chain. Every admitted source scope receives its own Environment Record even
    /// when several declarations reuse the same spelling.
    runtime_lexicals: std::collections::HashSet<String>,
    /// Captured once-per-call block `let`s homed in the activation (see CaptureScan's
    /// `candidates`). `homed_pending` holds the ones whose declaring block hasn't been reached
    /// yet: the FIRST block-level declaration of the name consumes it (skipping slot creation);
    /// any later same-name declaration is a nested shadow and binds a slot normally.
    homed_lets: std::collections::HashSet<String>,
    homed_pending: std::collections::HashSet<String>,
    ops: Vec<Op>,
    consts: Vec<Value>,
    names: Vec<Rc<str>>,
    /// Lexical scopes for slot resolution: (name, slot, is_const), innermost last.
    scopes: Vec<Vec<(String, u16, bool)>>,
    /// Environment-backed bindings at each compiler scope, parallel to `scopes`. An entry blocks
    /// lookup of same-named outer slots and resolves through the VM's current environment cursor.
    lexical_env_names: Vec<std::collections::HashMap<String, bool>>,
    /// Scope-vector length at entry to each compiled `with` body. Bindings declared in scopes
    /// added after the innermost boundary shadow the with object; all earlier slot/activation
    /// homes must instead use dynamic Environment Record resolution.
    with_scope_floors: Vec<usize>,
    slot_names: Vec<Rc<str>>,
    n_refs: usize,
    n_params: usize,
    arguments_slot: Option<u16>,
    loops: Vec<LoopCtx>,
    /// Labels collected from an enclosing `Stmt::Labeled` chain, waiting to be attached to the next
    /// loop's `LoopCtx` (drained when that loop pushes its context).
    pending_labels: Vec<String>,
    uses_this: bool,
    /// Count of object-literal template sites handed out (see `Chunk::obj_maps`).
    obj_maps: u32,
    caches: Vec<std::cell::Cell<IcState>>,
    /// Property-cache snapshots for the source frame currently being recompiled. The hot
    /// first-stage caller is the root; entering a speculative inline pushes that callee's own
    /// hot first-stage property-site vector. Cursors follow source property-op order, which is
    /// stable for a function's AST even when `this` becomes an inline local; each seed also
    /// carries its property name so a compiler-shape mismatch safely leaves the new site cold.
    cache_seed_stack: Vec<(Vec<(Rc<str>, [IcState; PROP_IC_WAYS])>, usize)>,
    name_caches: Vec<std::cell::Cell<NameIc>>,
    name_pins: Vec<NamePin>,
    name_num_bits: Vec<std::cell::Cell<u64>>,
    name_num_valid: Vec<std::cell::Cell<bool>>,
    name_seed_stack: Vec<(Vec<NameSeed>, usize)>,
    call_caches: Vec<CallSite>,
    construct_caches: Vec<std::cell::Cell<ConstructSite>>,
    call_pins: crate::fasthash::FastMap<usize, CallPin>,
    /// Polymorphic call feedback for the source frame currently being recompiled. Like property
    /// seeds, this is a compile-time stack: speculative child splices consume their own original
    /// call-site order without disturbing the caller's cursor.
    call_seed_stack: Vec<(Vec<CallSeed>, usize)>,
    /// Number of `PushHandler` regions active at the current emission point. `break`/`continue`
    /// jumping out of a `try` block (or a for-of body, which wraps itself in a handler) must
    /// emit a `PopHandler` per region crossed, or the stale handler catches unrelated throws
    /// later in the frame.
    try_depth: u32,
    /// Active `finally` handler depths. A break/continue may discard ordinary catch/iterator
    /// handlers, but it must never jump across a finalizer without executing it.
    finally_depths: Vec<u32>,
    /// Slots that ever enter a temporal dead zone (an `Op::Tdz` was emitted for them). The fused
    /// element ops defer the base-slot read past key/value evaluation, which is only
    /// order-unobservable when the base can never TDZ-throw — params and `var`s qualify.
    tdz_slots: std::collections::HashSet<u16>,
    /// Captured (env-homed) function-scope-wide names → is_const. Slot scopes shadow these.
    env_names: std::collections::HashMap<String, bool>,
    /// Annex B block FunctionDeclaration AST identity → its distinct function-scope var home.
    /// The current lexical home has the same spelling when the declaration statement executes.
    annexb_targets: crate::fasthash::FastMap<usize, Home>,
    funcs: Vec<Rc<Function>>,
    templates: Vec<(u64, Vec<(Option<String>, String)>)>,
    eval_exprs: Vec<EvalExprPlan>,
    class_plans: Vec<ClassPlan>,
    assignment_targets: Vec<AssignmentTargetPlan>,
    lexical_scopes: Vec<Vec<LexicalBinding>>,
    cap_inits: Vec<CapInit>,
    reuse_activation: bool,
    env_this: bool,
    env_arguments: bool,
    lexical_this: bool,
    /// A derived constructor reads the live Function Environment Record: its `this` binding is
    /// uninitialized until `super()` and its `new.target` belongs to that same record.
    derived_constructor: bool,
    /// Speculative-inline plan stack (second-stage only): one frame per active splice, each
    /// mapping that frame's call-site ordinal (== the function's first-compile `CallIc` index —
    /// same AST, same emission order) to the callees to splice. See [`plan_inlines`].
    plan_stack: Vec<(crate::fasthash::FastMap<u32, InlinePlanEntry>, u32)>,
    /// > 0 while compiling a spliced callee body.
    inline_depth: u32,
    /// The slot holding the receiver while inlining a `this`-using callee.
    inline_this: Option<u16>,
    /// `return` jumps inside the current spliced body, patched to the join point.
    inline_returns: Vec<usize>,
    inline_targets: Vec<InlineTarget>,
}

/// Whether a tagged-template call occurs in a tail position of a return operand.
///
/// Strict ordinary functions currently preserve proper tail calls through the tree-walker's
/// trampoline.  Until bytecode returns can transfer tagged calls to that trampoline too, keep
/// such functions on the normative path rather than turning an otherwise constant-stack loop
/// into Rust recursion.  This is the expression part of ECMA-262 HasCallInTailPosition: only the
/// selected conditional arm, the final sequence element, and the right logical operand inherit
/// tail position.
fn has_tail_tagged_template(expr: &Expr) -> bool {
    match expr {
        Expr::TaggedTemplate { .. } => true,
        Expr::Cond { cons, alt, .. } => {
            has_tail_tagged_template(cons) || has_tail_tagged_template(alt)
        }
        Expr::Seq(exprs) => exprs.last().is_some_and(has_tail_tagged_template),
        Expr::Logical { right, .. } | Expr::Paren(right) => has_tail_tagged_template(right),
        _ => false,
    }
}

/// One planned inline: the callee function (AST) and its pinned identity.
#[derive(Clone)]
pub struct InlinePlanEntry {
    /// Guarded ways, tried in order; a polymorphic site (DeltaBlue's constraint hierarchies)
    /// splices each hot callee behind its own identity guard, falling through to the next.
    pub ways: Vec<InlineWay>,
}

#[derive(Clone)]
pub struct InlineWay {
    pub f: Rc<Function>,
    pub obj: crate::value::Gc,
    pub check_this: bool,
    pub uses_this: bool,
    /// Free names the callee reads (global-closure callees only): the splice refuses any that
    /// the caller's scopes shadow, so the inlined LoadNames resolve identically.
    pub free_names: Vec<Rc<str>>,
    /// Exact shared closure environment required by non-global free-name inlines.
    pub expected_env: usize,
    /// The callee's OWN inline plan (depth-capped recursion): call sites inside the spliced
    /// body splice too, keyed by the callee-frame ordinal — its first-compile cache numbering,
    /// which the splice reproduces by walking the same AST in the same order.
    pub nested: crate::fasthash::FastMap<u32, InlinePlanEntry>,
}

/// Where a name resolves inside the compiled body.
#[derive(Clone, Copy)]
enum Home {
    Slot(u16, bool),
    /// Captured: lives in the activation env; bool = is_const.
    Env(bool),
}

/// A destructuring-assignment Reference whose observable base/key evaluation has already run.
/// Hidden slots retain member References across iterator steps and suspending defaults.
enum PreparedAssignmentRef {
    Local {
        slot: u16,
        is_const: bool,
        name: u32,
    },
    Captured {
        name: u32,
        is_const: bool,
    },
    Name(u32),
    Reference(u16),
    Property {
        base: u16,
        name: u32,
        cache: u32,
    },
    Element {
        base: u16,
        key: u16,
    },
}

#[derive(Default)]
struct LoopCtx {
    breaks: Vec<usize>,
    continues: Vec<usize>,
    /// `Compiler::try_depth` when this context was entered — the reference point for how many
    /// handler regions a `break`/`continue` targeting this context crosses.
    entry_try_depth: u32,
    /// A for-of loop's iterator slot: crossing `break`s close it (`IterCloseL`); its own
    /// `continue`s don't (the loop keeps iterating).
    foreach_iter: Option<u16>,
    /// The async-from-sync flag slot for a `for await…of`; present means every abandonment uses
    /// suspending AsyncIteratorClose rather than synchronous IteratorClose.
    foreach_async_from_sync: Option<u16>,
    /// For a for-of context: `try_depth` just after its per-iteration body handler pushed —
    /// exits emitted inside the body pop down to here before touching the handler itself.
    body_try_depth: u32,
    /// Labels naming this loop (usually zero or one; `a: b: for(…)` stacks several). A labelled
    /// `break`/`continue` searches the loop stack for the ctx carrying its target label.
    labels: Vec<String>,
    /// A `switch` context: an unlabelled `break` targets it, but `continue` skips past it to the
    /// innermost enclosing loop.
    is_switch: bool,
    /// A labelled non-breakable statement accepts only a matching labelled `break`; unlabelled
    /// break/continue searches skip it.
    is_label_block: bool,
}

/// Debug (`LUMEN_TIER_LOG=1`): report the AST construct a compile bail came from.
fn log_bail(what: &str, detail: &str) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("LUMEN_TIER_LOG").is_some()) {
        eprintln!("[tier] unsupported {what}: {detail}");
    }
}

/// Compilation bail: the construct is outside the v0 subset.
struct Bail;
type CResult = Result<(), Bail>;

#[derive(Clone, Copy)]
enum CallArgsMode {
    Fixed(u16),
    FinalSpread(u16),
    Array,
}

/// Whether a parameter default is in the compiler's lowerable subset: no reference to any
/// *banned* name (this parameter itself or a later one — the spec's param-scope TDZ would throw
/// where slots would read a seeded `undefined`), and no nested function/class (whose capture
/// analysis of a *parameter expression* scope the slot model doesn't carry). Whitelist
/// recursion: unknown constructs answer false (the function stays on the tree-walker).
fn default_expr_safe(e: &Expr, banned: &std::collections::HashSet<&str>) -> bool {
    match e {
        Expr::Num(_)
        | Expr::BigInt(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Undefined
        | Expr::This
        | Expr::Regex { .. } => true,
        Expr::Ident(n) => !banned.contains(n.as_str()),
        Expr::Paren(x) | Expr::ToStr(x) | Expr::Unary { arg: x, .. } => {
            default_expr_safe(x, banned)
        }
        Expr::Update { arg, .. } => default_expr_safe(arg, banned),
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            default_expr_safe(left, banned) && default_expr_safe(right, banned)
        }
        Expr::Cond { test, cons, alt } => {
            default_expr_safe(test, banned)
                && default_expr_safe(cons, banned)
                && default_expr_safe(alt, banned)
        }
        Expr::Member { obj, .. } => default_expr_safe(obj, banned),
        Expr::Index { obj, index, .. } => {
            default_expr_safe(obj, banned) && default_expr_safe(index, banned)
        }
        Expr::Call { callee, args, .. } | Expr::New { callee, args } => {
            default_expr_safe(callee, banned)
                && args.iter().all(|a| match a {
                    ArrayElem::Item(x) | ArrayElem::Spread(x) => default_expr_safe(x, banned),
                    ArrayElem::Hole => true,
                })
        }
        Expr::Array(elems) => elems.iter().all(|a| match a {
            ArrayElem::Item(x) | ArrayElem::Spread(x) => default_expr_safe(x, banned),
            ArrayElem::Hole => true,
        }),
        Expr::Object(props) => props.iter().all(|p| match p {
            PropDef::KeyValue { key, value } => {
                !matches!(key, PropKey::Computed(_)) && default_expr_safe(value, banned)
            }
            _ => false,
        }),
        _ => false,
    }
}

/// Whether `e` provably cannot reassign the local `name` (for fused element ops, which defer the
/// base-slot read past this expression's evaluation). Whitelist recursion: any variant not
/// explicitly handled answers `false` (don't fuse). Calls and nested functions are safe — a slot
/// local is unobservable outside its function (that is what makes slot storage sound), so only a
/// syntactic assignment/update in this very expression could touch it.
fn no_assign_to(e: &Expr, name: &str) -> bool {
    match e {
        Expr::Num(_)
        | Expr::BigInt(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Undefined
        | Expr::Ident(_)
        | Expr::This
        | Expr::Regex { .. }
        | Expr::Func(_) => true,
        Expr::Paren(x) | Expr::ToStr(x) | Expr::Unary { arg: x, .. } => no_assign_to(x, name),
        Expr::Update { arg, .. } => match &**arg {
            Expr::Ident(n) => n != name,
            Expr::Member { obj, .. } => no_assign_to(obj, name),
            Expr::Index { obj, index, .. } => no_assign_to(obj, name) && no_assign_to(index, name),
            _ => false,
        },
        Expr::Assign { target, value, .. } => {
            let target_ok = match &**target {
                Expr::Ident(n) => n != name,
                Expr::Member { obj, .. } => no_assign_to(obj, name),
                Expr::Index { obj, index, .. } => {
                    no_assign_to(obj, name) && no_assign_to(index, name)
                }
                _ => false, // destructuring pattern — could bind `name`
            };
            target_ok && no_assign_to(value, name)
        }
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            no_assign_to(left, name) && no_assign_to(right, name)
        }
        Expr::Cond { test, cons, alt } => {
            no_assign_to(test, name) && no_assign_to(cons, name) && no_assign_to(alt, name)
        }
        Expr::Member { obj, .. } => no_assign_to(obj, name),
        Expr::Index { obj, index, .. } => no_assign_to(obj, name) && no_assign_to(index, name),
        Expr::Call { callee, args, .. } | Expr::New { callee, args } => {
            no_assign_to(callee, name)
                && args.iter().all(|a| match a {
                    ArrayElem::Item(e) | ArrayElem::Spread(e) => no_assign_to(e, name),
                    ArrayElem::Hole => true,
                })
        }
        Expr::Array(elems) => elems.iter().all(|a| match a {
            ArrayElem::Item(e) | ArrayElem::Spread(e) => no_assign_to(e, name),
            ArrayElem::Hole => true,
        }),
        _ => false,
    }
}

impl Compiler {
    fn emit(&mut self, op: Op) -> usize {
        self.ops.push(op);
        self.ops.len() - 1
    }
    /// Reserve a fresh inline-cache slot (starts empty) for a property-access op.
    fn new_cache(&mut self, name: u32) -> u32 {
        // PROP_IC_WAYS consecutive ways per site: consumers address way 1; probes reach the
        // others at `cache_ptr + k` (see `Interp::ic_way`). Keeps every existing call site
        // untouched.
        let idx = self.caches.len() as u32;
        let seed = self
            .cache_seed_stack
            .last_mut()
            .and_then(|(sites, cursor)| {
                let site = sites.get(*cursor);
                *cursor += 1;
                site.filter(|(hot_name, _)| hot_name.as_ref() == self.names[name as usize].as_ref())
                    .map(|(_, states)| *states)
            });
        for way in 0..PROP_IC_WAYS {
            self.caches.push(std::cell::Cell::new(
                seed.map(|states| states[way]).unwrap_or(IcState::EMPTY),
            ));
        }
        idx
    }
    /// Reserve one cache cell for an op whose generated template has a single stable shape.
    /// Unlike property sites, `instanceof` does not need four polymorphic ways; keeping this
    /// separate avoids paying 96 bytes per source occurrence.
    fn new_single_cache(&mut self) -> u32 {
        let idx = self.caches.len() as u32;
        self.caches.push(std::cell::Cell::new(IcState::EMPTY));
        idx
    }
    /// Reserve a fresh name-cache slot for a free-name op.
    fn new_name_cache(&mut self, name: u32) -> u32 {
        let seed = self.name_seed_stack.last_mut().and_then(|(sites, cursor)| {
            let site = sites.get(*cursor);
            *cursor += 1;
            site.filter(|(hot_name, ..)| hot_name.as_ref() == self.names[name as usize].as_ref())
                .cloned()
        });
        let (ic, pin, number) = match seed {
            Some((_, ic, pin, number)) => (ic, pin, number),
            None => (NameIc::EMPTY, None, None),
        };
        self.name_caches.push(std::cell::Cell::new(ic));
        self.name_pins.push(pin);
        self.name_num_bits
            .push(std::cell::Cell::new(number.unwrap_or(0)));
        self.name_num_valid
            .push(std::cell::Cell::new(number.is_some()));
        (self.name_caches.len() - 1) as u32
    }
    fn emit_store_name(&mut self, name: u32) {
        let cache = self.new_name_cache(name);
        self.emit(Op::StoreNameCached(name, cache));
    }

    /// ECMA-262 §13.15.2 evaluates a statically-resolved immutable identifier like every other
    /// Reference: simple assignment evaluates the RHS before PutValue, while compound assignment
    /// first performs GetValue and the binary operation. The final store distinguishes a slot TDZ
    /// from an initialized immutable binding instead of folding both failures into a TypeError.
    fn immutable_assignment(&mut self, home: Home, name: &str, op: &str, value: &Expr) -> CResult {
        let name_index = self.name_idx(name);
        if op == "=" {
            self.named_expr(value, name)?;
        } else {
            match home {
                Home::Slot(slot, true) => self.emit(Op::LoadLocal(slot)),
                Home::Env(true) => self.emit(Op::LoadCap(name_index)),
                _ => unreachable!("immutable assignment requires an immutable home"),
            };
            self.expr(value)?;
            self.emit_compound(op)?;
        }
        match home {
            Home::Slot(slot, true) => self.emit(Op::StoreConstLocal(slot, name_index)),
            Home::Env(true) => self.emit(Op::StoreConstCap(name_index)),
            _ => unreachable!("immutable assignment requires an immutable home"),
        };
        Ok(())
    }
    /// Reserve a fresh call-cache slot for a `Call`/`CallWithThis` site.
    fn new_call_cache(&mut self) -> u32 {
        // Every frame counts its own call-site ordinals (see `Compiler::plan_stack`).
        if let Some(top) = self.plan_stack.last_mut() {
            top.1 += 1;
        }
        let seed = self.call_seed_stack.last_mut().and_then(|(sites, cursor)| {
            let seed = sites.get(*cursor);
            *cursor += 1;
            seed.map(|seed| (seed.entries, seed.next, seed.pins.clone()))
        });
        let site = if let Some((entries, next, pins)) = seed {
            for (callee, pin) in pins {
                self.call_pins.entry(callee).or_insert(pin);
            }
            CallSite {
                entries: std::array::from_fn(|way| std::cell::Cell::new(entries[way])),
                next: std::cell::Cell::new(next),
            }
        } else {
            CallSite::empty()
        };
        self.call_caches.push(site);
        (self.call_caches.len() - 1) as u32
    }
    fn new_construct_cache(&mut self) -> u32 {
        let cache = self.construct_caches.len() as u32;
        self.construct_caches
            .push(std::cell::Cell::new(ConstructSite::EMPTY));
        cache
    }

    /// The current frame's plan entry for the NEXT call site (call before `new_call_cache`).
    fn plan_hit(&self) -> Option<InlinePlanEntry> {
        let (plan, ord) = self.plan_stack.last()?;
        plan.get(ord).cloned()
    }

    /// Emit a planned speculative inline for a method call site whose stack already holds
    /// `[this, method, args...]`; anything the splice can't express reverts to the plain call.
    fn emit_call_with_inline(
        &mut self,
        entry: InlinePlanEntry,
        argc: u16,
        cc: u32,
        has_this: bool,
    ) {
        let snap = (
            self.ops.len(),
            self.consts.len(),
            self.names.len(),
            self.caches.len(),
            self.name_caches.len(),
            self.call_caches.len(),
            self.inline_targets.len(),
            self.slot_names.len(),
            self.funcs.len(),
            self.templates.len(),
            self.eval_exprs.len(),
            self.class_plans.len(),
            self.assignment_targets.len(),
        );
        if self.try_emit_inline(&entry, argc, cc, has_this).is_err() {
            self.ops.truncate(snap.0);
            self.consts.truncate(snap.1);
            self.names.truncate(snap.2);
            self.caches.truncate(snap.3);
            self.name_caches.truncate(snap.4);
            self.name_pins.truncate(snap.4);
            self.name_num_bits.truncate(snap.4);
            self.name_num_valid.truncate(snap.4);
            self.call_caches.truncate(snap.5);
            self.inline_targets.truncate(snap.6);
            self.slot_names.truncate(snap.7);
            self.funcs.truncate(snap.8);
            self.templates.truncate(snap.9);
            self.eval_exprs.truncate(snap.10);
            self.class_plans.truncate(snap.11);
            self.assignment_targets.truncate(snap.12);
            if has_this {
                self.emit(Op::CallWithThis(argc, cc));
            } else {
                self.emit(Op::Call(argc, cc));
            }
        }
    }

    fn try_emit_inline(
        &mut self,
        entry: &InlinePlanEntry,
        argc: u16,
        cc: u32,
        has_this: bool,
    ) -> CResult {
        if argc > 8 {
            return Err(Bail); // the JIT guard peeks the callee with a ±256-byte unscaled load
        }
        // Per-way gates: a plain `Call` site has no `this` beneath the callee (a this-using
        // callee needs the generic binding, and the guard's receiver peek would read past the
        // operands); a caller binding (slot or captured) would shadow a global free name.
        let ways: Vec<&InlineWay> = entry
            .ways
            .iter()
            .filter(|w| {
                (has_this || !w.uses_this)
                    && (w.expected_env != 0
                        || !w.free_names.iter().any(|name| {
                            self.lookup(name).is_some() || self.env_names.contains_key(&**name)
                        }))
            })
            .collect();
        if ways.is_empty() {
            return Err(Bail);
        }
        // Each way: identity guard → bind → spliced body → jump to the shared join; a guard
        // mismatch falls to the next way, the last one to the generic call.
        let mut end_jumps: Vec<usize> = Vec::new();
        let mut pending_guard: Option<usize> = None;
        for w in &ways {
            if let Some(g) = pending_guard.take() {
                self.patch(g); // previous way's mismatch lands on this way's guard
            }
            let guard = self.emit_inline_way(w, argc, has_this, &mut end_jumps)?;
            pending_guard = Some(guard);
        }
        // ---- join: every way's result jumps here; the last mismatch runs the generic call.
        self.patch(pending_guard.take().expect("at least one way"));
        if has_this {
            self.emit(Op::CallWithThis(argc, cc));
        } else {
            self.emit(Op::Call(argc, cc));
        }
        for j in end_jumps {
            self.patch(j);
        }
        Ok(())
    }

    /// One guarded splice: emits the identity guard (returned unpatched — the caller chains it
    /// to the next way or the generic call), the frame binds, and the body; the result-carrying
    /// exits are appended to `end_jumps`.
    fn emit_inline_way(
        &mut self,
        w: &InlineWay,
        argc: u16,
        has_this: bool,
        end_jumps: &mut Vec<usize>,
    ) -> Result<usize, Bail> {
        let f = &w.f;
        let t = self.inline_targets.len() as u32;
        self.inline_targets.push(InlineTarget {
            expected: Rc::as_ptr(&w.obj) as usize,
            pin: Rc::downgrade(&w.obj),
            expected_env: w.expected_env,
            argc,
            check_this: has_this && w.check_this,
        });
        let guard = self.emit(Op::InlineGuard(t, 0));

        // ---- bind the callee frame into fresh caller slots ----
        let n_params = f.params.len();
        for _ in n_params..argc as usize {
            self.emit(Op::Pop); // surplus arguments (evaluated; excess drops from the top)
        }
        for _ in argc as usize..n_params {
            self.emit(Op::Undef); // missing arguments
        }
        let mut param_slots: Vec<u16> = Vec::with_capacity(n_params);
        for p in &f.params {
            let Pattern::Ident(name) = &p.pattern else {
                return Err(Bail);
            };
            if p.default.is_some() || p.rest {
                return Err(Bail);
            }
            param_slots.push(self.fresh_slot(name));
        }
        for &s in param_slots.iter().rev() {
            self.emit(Op::StoreLocal(s));
        }
        self.emit(Op::Pop); // the method (identity proven; the value itself is dead)
        let this_slot = if !has_this {
            None // a plain Call site: nothing beneath the callee
        } else if w.uses_this {
            let s = self.fresh_slot("(inline this)");
            self.emit(Op::StoreLocal(s));
            Some(s)
        } else {
            self.emit(Op::Pop);
            None
        };

        // ---- compile the body under the callee's (empty) namespace ----
        let saved_scopes = std::mem::take(&mut self.scopes);
        let saved_lexical_env_names = std::mem::take(&mut self.lexical_env_names);
        let saved_env_names = std::mem::take(&mut self.env_names);
        let saved_loops = std::mem::take(&mut self.loops);
        let saved_labels = std::mem::take(&mut self.pending_labels);
        let saved_try = std::mem::replace(&mut self.try_depth, 0);
        let saved_this = std::mem::replace(&mut self.inline_this, this_slot);
        let saved_returns = std::mem::take(&mut self.inline_returns);
        self.inline_depth += 1;
        self.plan_stack.push((w.nested.clone(), 0));
        let hot_chunk = f.code.get().and_then(Option::as_ref);
        self.cache_seed_stack.push((
            hot_chunk
                .map(|chunk| property_cache_seeds(chunk))
                .unwrap_or_default(),
            0,
        ));
        self.name_seed_stack.push((
            hot_chunk
                .map(|chunk| name_cache_seeds(chunk))
                .unwrap_or_default(),
            0,
        ));
        self.call_seed_stack.push((
            hot_chunk
                .map(|chunk| call_cache_seeds(chunk))
                .unwrap_or_default(),
            0,
        ));
        self.push_compile_scope();
        for (k, p) in f.params.iter().enumerate() {
            let Pattern::Ident(name) = &p.pattern else {
                unreachable!()
            };
            self.scope_bind(name, param_slots[k], false);
        }
        let r = self.inline_body(f);
        self.call_seed_stack.pop();
        self.name_seed_stack.pop();
        self.cache_seed_stack.pop();
        self.plan_stack.pop();
        self.inline_depth -= 1;
        let returns = std::mem::replace(&mut self.inline_returns, saved_returns);
        self.inline_this = saved_this;
        self.try_depth = saved_try;
        self.pending_labels = saved_labels;
        self.loops = saved_loops;
        self.env_names = saved_env_names;
        self.lexical_env_names = saved_lexical_env_names;
        self.scopes = saved_scopes;
        r?;

        end_jumps.push(self.emit(Op::Jump(0)));
        end_jumps.extend(returns);
        Ok(guard)
    }

    /// Compile a spliced callee body: mirrors `compile_inner`'s hoist + lexical + statement
    /// sequence, with explicit per-execution resets replacing the fresh frame's zeroed slots.
    fn inline_body(&mut self, f: &Function) -> CResult {
        // Hoisted vars start undefined on EVERY pass through the site; fused-reset runs are
        // emitted per contiguous slot range (fresh slots are consecutive, so usually one op).
        let mut resets: Vec<u16> = Vec::new();
        for op in crate::interpreter::collect_hoist_ops(&f.body, f.is_strict, &[]) {
            match op {
                HoistOp::Var(name) => {
                    if self.lookup(&name).is_none() {
                        let slot = self.fresh_slot(&name);
                        self.scope_bind(&name, slot, false);
                        resets.push(slot);
                    }
                }
                HoistOp::Fn(..) | HoistOp::AnnexB(..) => return Err(Bail),
            }
        }
        resets.sort_unstable();
        let mut k = 0;
        while k < resets.len() {
            let start = resets[k];
            let mut count = 1u16;
            while k + (count as usize) < resets.len() && resets[k + count as usize] == start + count
            {
                count += 1;
            }
            self.emit(Op::ResetSlots(start, count));
            k += count as usize;
        }
        let empty = std::collections::HashSet::new();
        self.declare_body_lexicals(&f.body, &empty)?;
        for stmt in &f.body {
            self.stmt(stmt)?;
        }
        self.emit(Op::Undef); // implicit return value
        Ok(())
    }
    /// Declare every binding a lexical declaration pattern introduces, in source order (slot +
    /// TDZ each, like the plain-identifier path). Defaults and computed keys are evaluations, not
    /// declarations; rest and nested patterns contribute their BoundNames recursively.
    fn declare_lexical_pattern(&mut self, pat: &Pattern, is_const: bool) -> CResult {
        match pat {
            Pattern::Ident(name) => {
                if self.current_lexical_env_has(name) {
                    return Ok(());
                }
                if self.homed_pending.remove(name) {
                    // The homed block `let`'s own declaration (see `Compiler::homed_lets`):
                    // in TDZ since entry, no slot, no per-entry Tdz (the block runs at most
                    // once per call by construction). Consumed so any LATER same-name
                    // declaration (a nested for-of head, a sibling block) slot-shadows.
                    return Ok(());
                }
                let slot = self.fresh_slot(name);
                self.scope_bind(name, slot, is_const);
                self.tdz_slots.insert(slot);
                self.emit(Op::Tdz(slot));
                Ok(())
            }
            Pattern::Object(o) => {
                for prop in &o.props {
                    self.declare_lexical_pattern(&prop.value, is_const)?;
                }
                if let Some(rest) = &o.rest {
                    self.declare_lexical_pattern(&Pattern::Ident(rest.clone()), is_const)?;
                }
                Ok(())
            }
            Pattern::Array(elems) => {
                for e in elems {
                    match e {
                        ArrayPatElem::Hole => {}
                        ArrayPatElem::Elem { pattern, .. } | ArrayPatElem::Rest(pattern) => {
                            self.declare_lexical_pattern(pattern, is_const)?
                        }
                    }
                }
                Ok(())
            }
            _ => Err(Bail),
        }
    }

    /// Lower a declaration destructuring against the value on the stack (consumed): the
    /// KeyedBindingInitialization subset with plain (non-computed) keys, no defaults, no rest —
    /// per property: Dup + GetProp (the oracle's GetV), recursing into nested object patterns.
    /// The nullish guard throws the oracle's exact TypeError before any read.
    fn destructure_store(&mut self, pat: &Pattern, kind: DeclKind) -> CResult {
        match pat {
            Pattern::Ident(name) => {
                if self.current_lexical_env_has(name) {
                    let name = self.name_idx(name);
                    self.emit(if matches!(kind, DeclKind::Var) {
                        Op::StoreName(name)
                    } else {
                        Op::InitLex(name)
                    });
                    return Ok(());
                }
                let home = self.home(name).ok_or(Bail)?;
                match home {
                    Home::Slot(slot, _) => {
                        self.emit(Op::StoreLocal(slot));
                    }
                    Home::Env(_) => {
                        let n = self.name_idx(name);
                        if matches!(kind, DeclKind::Var) {
                            self.emit(Op::StoreCap(n));
                        } else {
                            self.emit(Op::StoreCapInit(n));
                        }
                    }
                }
                Ok(())
            }
            Pattern::Object(o) => {
                if o.rest.is_some()
                    || o.props.iter().any(|prop| {
                        prop.default.is_some()
                            || !matches!(&prop.key, PropKey::Ident(_) | PropKey::Str(_))
                    })
                {
                    return self.binding_object_pattern(o, kind);
                }
                self.emit(Op::DestructureGuard);
                for prop in &o.props {
                    if prop.default.is_some() {
                        return Err(Bail);
                    }
                    let key: String = match &prop.key {
                        PropKey::Ident(k) => k.clone(),
                        PropKey::Str(k) => k.to_string(),
                        _ => return Err(Bail),
                    };
                    let ki = self.name_idx(&key);
                    self.emit(Op::Dup);
                    let c = self.new_cache(ki);
                    self.emit(Op::GetProp(ki, c));
                    self.destructure_store(&prop.value, kind)?;
                }
                self.emit(Op::Pop);
                Ok(())
            }
            Pattern::Array(elems) => {
                // Batched iterator walk (Op::DestructureArr), then stores in reverse. Batching
                // is only order-unobservable when every leaf is an UNCAPTURED slot (an env-homed
                // leaf's initialization is visible to a later iterator step's next() per spec)
                // and elements are flat idents/holes (a nested pattern's own reads would
                // interleave with the steps), with no defaults (their evaluation interleaves).
                let batched = elems.iter().all(|e| {
                    match e {
                        ArrayPatElem::Hole => {}
                        ArrayPatElem::Elem {
                            pattern: Pattern::Ident(n),
                            default: None,
                        } if matches!(self.home(n), Some(Home::Slot(..))) => {}
                        _ => return false,
                    }
                    true
                });
                if !batched {
                    return self.binding_array_pattern(elems, kind);
                }
                self.emit(Op::DestructureArr(elems.len() as u16));
                for e in elems.iter().rev() {
                    match e {
                        ArrayPatElem::Hole => {
                            self.emit(Op::Pop);
                        }
                        ArrayPatElem::Elem { pattern, .. } => {
                            self.destructure_store(pattern, kind)?;
                        }
                        ArrayPatElem::Rest(_) => unreachable!("filtered above"),
                    }
                }
                Ok(())
            }
            _ => Err(Bail),
        }
    }

    fn binding_default(&mut self, pattern: &Pattern, default: Option<&Expr>) -> CResult {
        let Some(default) = default else {
            return Ok(());
        };
        self.emit(Op::Dup);
        self.emit(Op::Undef);
        self.emit(Op::StrictEq);
        let present = self.emit(Op::JumpIfFalse(0));
        self.emit(Op::Pop);
        if let Pattern::Ident(name) = pattern {
            self.named_expr(default, name)?;
        } else {
            self.expr(default)?;
        }
        self.patch(present);
        Ok(())
    }

    fn binding_array_pattern(&mut self, elements: &[ArrayPatElem], kind: DeclKind) -> CResult {
        let iterator = self.fresh_slot("%binding-iterator%");
        let next = self.fresh_slot("%binding-next%");
        let done = self.fresh_slot("%binding-done%");
        self.emit(Op::GetIter);
        self.emit(Op::StoreLocal(next));
        self.emit(Op::StoreLocal(iterator));
        let false_value = self.const_idx(Value::Bool(false));
        self.emit(Op::Const(false_value));
        self.emit(Op::StoreLocal(done));

        let handler = self.emit(Op::PushIterator(0, 0, 0, 0));
        self.try_depth += 1;
        for element in elements {
            match element {
                ArrayPatElem::Hole => {
                    self.emit(Op::DestructureStepL(iterator, next, done));
                    self.emit(Op::Pop);
                }
                ArrayPatElem::Elem { pattern, default } => {
                    self.emit(Op::DestructureStepL(iterator, next, done));
                    self.binding_default(pattern, default.as_ref())?;
                    self.destructure_store(pattern, kind)?;
                }
                ArrayPatElem::Rest(pattern) => {
                    self.emit(Op::DestructureRestL(iterator, next, done));
                    self.destructure_store(pattern, kind)?;
                }
            }
        }
        self.emit(Op::PopHandler);
        self.try_depth -= 1;
        self.emit(Op::IterCloseIfNotDoneL(iterator, done));
        let after_pads = self.emit(Op::Jump(0));

        let throw_pc = self.ops.len() as u32;
        self.emit(Op::IterAbortIfNotDoneL(iterator, done));
        let return_pc = self.ops.len() as u32;
        self.emit(Op::IterCloseIfNotDoneL(iterator, done));
        self.emit(Op::Return);
        let bare_return_pc = self.ops.len() as u32;
        self.emit(Op::IterCloseIfNotDoneL(iterator, done));
        self.emit(Op::ReturnBare);
        let resume_return_pc = self.ops.len() as u32;
        self.emit(Op::IterCloseIfNotDoneL(iterator, done));
        self.emit(Op::ResumeReturn);
        match &mut self.ops[handler] {
            Op::PushIterator(
                throw_target,
                return_target,
                bare_return_target,
                resume_return_target,
            ) => {
                *throw_target = throw_pc;
                *return_target = return_pc;
                *bare_return_target = bare_return_pc;
                *resume_return_target = resume_return_pc;
            }
            _ => unreachable!(),
        }
        self.patch(after_pads);
        Ok(())
    }

    fn binding_object_pattern(
        &mut self,
        object: &crate::ast::ObjectPat,
        kind: DeclKind,
    ) -> CResult {
        self.emit(Op::DestructureGuard);
        let source = self.fresh_slot("%binding-object%");
        self.emit(Op::StoreLocal(source));
        let mut excluded = Vec::new();
        for property in &object.props {
            let key = self.assignment_object_key(source, &property.key)?;
            excluded.push(key);
            self.load_assignment_property(source, key);
            self.binding_default(&property.value, property.default.as_ref())?;
            self.destructure_store(&property.value, kind)?;
        }
        if let Some(rest) = &object.rest {
            self.emit(Op::LoadLocal(source));
            for key in &excluded {
                self.emit(Op::LoadLocal(*key));
            }
            self.emit(Op::ObjectRest(excluded.len() as u16));
            self.destructure_store(&Pattern::Ident(rest.clone()), kind)?;
        }
        Ok(())
    }

    /// Emit ArgumentListEvaluation from ECMA-262 §13.3.8.1. Plain calls retain their compact
    /// fixed-arity op and one final spread retains the allocation-free expansion path. Multiple
    /// or interleaved spreads build a private dense list incrementally, so each iterator is fully
    /// consumed before the following argument expression is evaluated.
    fn call_args(&mut self, args: &[ArrayElem]) -> Result<CallArgsMode, Bail> {
        let spreads: Vec<usize> = args
            .iter()
            .enumerate()
            .filter_map(|(index, arg)| matches!(arg, ArrayElem::Spread(_)).then_some(index))
            .collect();
        if args.iter().any(|arg| matches!(arg, ArrayElem::Hole)) {
            return Err(Bail);
        }
        if spreads.is_empty() {
            for arg in args {
                let ArrayElem::Item(expr) = arg else {
                    unreachable!("argument shape checked above")
                };
                self.expr(expr)?;
            }
            return Ok(CallArgsMode::Fixed(
                u16::try_from(args.len()).map_err(|_| Bail)?,
            ));
        }
        if spreads.as_slice() == [args.len() - 1] {
            for arg in args {
                match arg {
                    ArrayElem::Item(expr) | ArrayElem::Spread(expr) => self.expr(expr)?,
                    ArrayElem::Hole => unreachable!("argument shape checked above"),
                }
            }
            return Ok(CallArgsMode::FinalSpread(
                u16::try_from(args.len()).map_err(|_| Bail)?,
            ));
        }

        self.emit(Op::NewArray);
        for arg in args {
            match arg {
                ArrayElem::Item(expr) => {
                    self.expr(expr)?;
                    self.emit(Op::ArrayPush);
                }
                ArrayElem::Spread(expr) => {
                    self.expr(expr)?;
                    self.emit(Op::ArraySpread);
                }
                ArrayElem::Hole => unreachable!("argument shape checked above"),
            }
        }
        Ok(CallArgsMode::Array)
    }

    fn finish_call(&mut self, args: &[ArrayElem], with_this: bool, allow_inline: bool) -> CResult {
        match self.call_args(args)? {
            CallArgsMode::Fixed(argc) => {
                let plan_hit = allow_inline.then(|| self.plan_hit()).flatten();
                let cache = self.new_call_cache();
                match plan_hit {
                    Some(entry) => self.emit_call_with_inline(entry, argc, cache, with_this),
                    None if with_this => {
                        self.emit(Op::CallWithThis(argc, cache));
                    }
                    None => {
                        self.emit(Op::Call(argc, cache));
                    }
                }
            }
            CallArgsMode::FinalSpread(argc) if with_this => {
                self.emit(Op::CallSpreadThis(argc));
            }
            CallArgsMode::FinalSpread(argc) => {
                self.emit(Op::CallSpread(argc));
            }
            CallArgsMode::Array if with_this => {
                self.emit(Op::CallArgsArrayThis);
            }
            CallArgsMode::Array => {
                self.emit(Op::CallArgsArray);
            }
        }
        Ok(())
    }

    /// `delete obj.p` / `delete obj[k]` on plain (non-optional, non-super, public) references;
    /// a non-reference operand evaluates for its effects and deletes to `true`. Identifier
    /// deletes (env bindings) and optional chains stay in the oracle.
    fn delete_expr(&mut self, arg: &Expr) -> CResult {
        match arg {
            Expr::Paren(inner) => self.delete_expr(inner),
            Expr::OptionalChain(inner) => self.delete_optional_chain(inner),
            Expr::Ident(name) => {
                if self.home(name).is_some() {
                    // Function/lexical declarations represented by slots or activation bindings
                    // are never deletable. Only a free name needs the full environment/global
                    // DeleteBinding lookup.
                    let no = self.const_idx(Value::Bool(false));
                    self.emit(Op::Const(no));
                } else {
                    let name = self.name_idx(name);
                    self.emit(Op::DeleteName(name));
                }
                Ok(())
            }
            Expr::Member {
                obj,
                optional: false,
                ..
            } if matches!(**obj, Expr::Super) => {
                // Evaluation of `super.name` performs GetThisBinding before Delete observes that
                // this is a Super Reference and throws. It does not need GetSuperBase.
                self.emit_super_this();
                self.emit(Op::Pop);
                self.emit(Op::DeleteSuper);
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if matches!(**obj, Expr::Super) => {
                // The computed expression evaluates, but Delete throws before ToPropertyKey.
                self.emit_super_this();
                self.emit(Op::Pop);
                self.expr(index)?;
                self.emit(Op::Pop);
                self.emit(Op::DeleteSuper);
                Ok(())
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.expr(obj)?;
                let n = self.name_idx(prop);
                self.emit(Op::DeleteProp(n, self.strict));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                self.expr(obj)?;
                self.expr(index)?;
                self.emit(Op::DeleteElem(self.strict));
                Ok(())
            }
            other => {
                self.expr(other)?;
                self.emit(Op::Pop);
                let k = self.const_idx(Value::Bool(true));
                self.emit(Op::Const(k));
                Ok(())
            }
        }
    }

    /// OptionalChain evaluation for the delete operator. A short-circuited chain produces true,
    /// while a live final Member/Index remains a Reference and performs [[Delete]].
    fn delete_optional_chain(&mut self, inner: &Expr) -> CResult {
        let mut shorts = Vec::new();
        match inner {
            Expr::Member {
                obj,
                prop,
                optional,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.opt_chain(obj, &mut shorts)?;
                if *optional {
                    self.opt_link(1, &mut shorts);
                }
                let name = self.name_idx(prop);
                self.emit(Op::DeleteProp(name, self.strict));
            }
            Expr::Index {
                obj,
                index,
                optional,
            } if !matches!(**obj, Expr::Super) => {
                self.opt_chain(obj, &mut shorts)?;
                if *optional {
                    self.opt_link(1, &mut shorts);
                }
                self.expr(index)?;
                self.emit(Op::DeleteElem(self.strict));
            }
            other => {
                self.opt_chain(other, &mut shorts)?;
                self.emit(Op::Pop);
                let yes = self.const_idx(Value::Bool(true));
                self.emit(Op::Const(yes));
            }
        }
        if shorts.is_empty() {
            return Ok(());
        }
        let done = self.emit(Op::Jump(0));
        for short in shorts {
            self.patch(short);
        }
        let yes = self.const_idx(Value::Bool(true));
        self.emit(Op::Const(yes));
        self.patch(done);
        Ok(())
    }

    /// Compile an optional chain (`a?.b.c`, `r?.m(args)`): each optional link peeks its base —
    /// nullish pops what the link would have consumed and jumps to a shared pad that pushes the
    /// chain's `undefined` result (skipping every later link, key expression, and argument, per
    /// spec). Non-optional links compile as usual. Public Member/Index method calls and plain
    /// optional callees preserve their distinct receiver rules, including private method
    /// receivers. Optional `delete` and `super` retain their separate paths.
    fn opt_chain(&mut self, e: &Expr, shorts: &mut Vec<usize>) -> CResult {
        match e {
            Expr::Member {
                obj,
                prop,
                optional,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.opt_chain(obj, shorts)?;
                if *optional {
                    self.opt_link(1, shorts);
                }
                let i = self.name_idx(prop);
                let c = self.new_cache(i);
                self.emit(Op::GetProp(i, c));
                Ok(())
            }
            Expr::Member {
                obj,
                prop,
                optional,
            } if !matches!(**obj, Expr::Super) && prop.starts_with('#') => {
                self.opt_chain(obj, shorts)?;
                if *optional {
                    self.opt_link(1, shorts);
                }
                let name = self.name_idx(prop);
                self.emit(Op::GetPrivate(name));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional,
            } if !matches!(**obj, Expr::Super) => {
                self.opt_chain(obj, shorts)?;
                if *optional {
                    self.opt_link(1, shorts);
                }
                self.expr(index)?;
                self.emit(Op::GetElem);
                Ok(())
            }
            Expr::Call {
                callee,
                args,
                optional: call_opt,
            } => match &**callee {
                Expr::Member {
                    obj,
                    prop,
                    optional,
                } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                    self.opt_chain(obj, shorts)?;
                    if *optional {
                        self.opt_link(1, shorts);
                    }
                    let name = self.name_idx(prop);
                    let cache = self.new_cache(name);
                    self.emit(Op::GetMethod(name, cache));
                    if *call_opt {
                        // `a.b?.(args)`: the method value is peeked; nullish drops
                        // [receiver, method].
                        self.opt_link(2, shorts);
                    }
                    self.finish_call(args, true, true)
                }
                Expr::Member {
                    obj,
                    prop,
                    optional,
                } if !matches!(**obj, Expr::Super) && prop.starts_with('#') => {
                    self.opt_chain(obj, shorts)?;
                    if *optional {
                        self.opt_link(1, shorts);
                    }
                    let name = self.name_idx(prop);
                    self.emit(Op::GetPrivateMethod(name));
                    if *call_opt {
                        self.opt_link(2, shorts);
                    }
                    self.finish_call(args, true, true)
                }
                Expr::Index {
                    obj,
                    index,
                    optional,
                } if !matches!(**obj, Expr::Super) => {
                    self.opt_chain(obj, shorts)?;
                    if *optional {
                        self.opt_link(1, shorts);
                    }
                    self.expr(index)?;
                    self.emit(Op::GetMethodElem);
                    if *call_opt {
                        self.opt_link(2, shorts);
                    }
                    self.finish_call(args, true, true)
                }
                plain => {
                    self.opt_chain(plain, shorts)?;
                    if *call_opt {
                        self.opt_link(1, shorts);
                    }
                    // A non-Reference callee and an environment Reference both use undefined as
                    // `this` here; method References were handled by the two arms above.
                    self.finish_call(args, false, true)
                }
            },
            // The chain's base (before any `?.` link): an ordinary expression.
            other => self.expr(other),
        }
    }

    /// One optional link: fall through when the top of stack isn't nullish; otherwise pop the
    /// `depth` values the rest of the link would consume and jump to the chain's undefined pad.
    fn opt_link(&mut self, depth: u16, shorts: &mut Vec<usize>) {
        let cont = self.emit(Op::JumpIfNotNullishPeek(0));
        for _ in 0..depth {
            self.emit(Op::Pop);
        }
        shorts.push(self.emit(Op::Jump(0)));
        self.patch(cont);
    }

    /// The local slot for a fused element access (`x[k]` → `GetElemLocal`), or `None` to use
    /// the generic ops. Fusing defers the base-local read past the key/value evaluation, so it
    /// requires: the base is an Ident homed in a slot that can never be in TDZ (a param or a
    /// `var` — no early throw to reorder; see `tdz_slots`), and no `deps` expression can
    /// reassign that local (calls can't — slot locals are unobservable outside the function;
    /// only an explicit assignment/update in the key/value expressions themselves could, and
    /// `no_assign_to` rejects those).
    fn fused_elem_slot(&self, obj: &Expr, deps: &[&Expr]) -> Option<u16> {
        let Expr::Ident(name) = obj else { return None };
        let Some(Home::Slot(slot, _)) = self.home(name) else {
            return None;
        };
        if self.tdz_slots.contains(&slot) {
            return None;
        }
        if deps.iter().all(|d| no_assign_to(d, name)) {
            Some(slot)
        } else {
            None
        }
    }
    fn fresh_slot(&mut self, name: &str) -> u16 {
        let slot = self.slot_names.len() as u16;
        self.slot_names.push(Rc::from(name));
        slot
    }
    fn fresh_reference(&mut self) -> Result<u16, Bail> {
        let reference = u16::try_from(self.n_refs).map_err(|_| Bail)?;
        self.n_refs += 1;
        Ok(reference)
    }
    fn push_compile_scope(&mut self) {
        self.scopes.push(Vec::new());
        self.lexical_env_names.push(Default::default());
    }
    fn pop_compile_scope(&mut self) {
        self.scopes.pop().expect("compiler scope stack underflow");
        self.lexical_env_names
            .pop()
            .expect("compiler lexical-environment stack underflow");
    }
    fn scope_bind(&mut self, name: &str, slot: u16, is_const: bool) {
        if self.scopes.is_empty() {
            self.push_compile_scope();
        }
        let top = self.scopes.last_mut().unwrap();
        if let Some(e) = top.iter_mut().find(|(n, ..)| n == name) {
            *e = (name.to_string(), slot, is_const);
        } else {
            top.push((name.to_string(), slot, is_const));
        }
    }
    fn lookup(&self, name: &str) -> Option<(u16, bool)> {
        for (scope, environment) in self.scopes.iter().zip(&self.lexical_env_names).rev() {
            if let Some((_, slot, k)) = scope.iter().rev().find(|(n, ..)| n == name) {
                return Some((*slot, *k));
            }
            if environment.contains_key(name) {
                return None;
            }
        }
        None
    }
    fn lexical_env_bind(&mut self, name: &str, is_const: bool) {
        self.lexical_env_names
            .last_mut()
            .expect("lexical binding requires a compiler scope")
            .insert(name.to_string(), is_const);
    }
    fn current_lexical_env_has(&self, name: &str) -> bool {
        self.lexical_env_names
            .last()
            .is_some_and(|scope| scope.contains_key(name))
    }
    fn env_bind(&mut self, name: &str, is_const: bool) {
        self.env_names.insert(name.to_string(), is_const);
    }
    fn env_has(&self, name: &str) -> bool {
        self.env_names.contains_key(name)
    }
    /// Resolve a local: innermost slot scope first, then runtime lexical environments (reported
    /// as `None` so the emitter uses dynamic name resolution), then the function activation.
    fn home(&self, name: &str) -> Option<Home> {
        if let Some(floor) = self.with_scope_floors.last().copied() {
            for (scope, environment) in self.scopes[floor..]
                .iter()
                .zip(&self.lexical_env_names[floor..])
                .rev()
            {
                if let Some((_, slot, is_const)) =
                    scope.iter().rev().find(|(candidate, ..)| candidate == name)
                {
                    return Some(Home::Slot(*slot, *is_const));
                }
                if environment.contains_key(name) {
                    return None;
                }
            }
            return None;
        }
        for (scope, environment) in self.scopes.iter().zip(&self.lexical_env_names).rev() {
            if let Some((_, slot, is_const)) =
                scope.iter().rev().find(|(candidate, ..)| candidate == name)
            {
                return Some(Home::Slot(*slot, *is_const));
            }
            if environment.contains_key(name) {
                return None;
            }
        }
        self.env_names
            .get(name)
            .map(|is_const| Home::Env(*is_const))
    }
    fn name_reference_can_change(&self, name: &str) -> bool {
        self.home(name).is_none() && (self.direct_eval || !self.with_scope_floors.is_empty())
    }
    fn const_idx(&mut self, v: Value) -> u32 {
        self.consts.push(v);
        (self.consts.len() - 1) as u32
    }
    fn name_idx(&mut self, name: &str) -> u32 {
        if let Some(i) = self.names.iter().position(|n| &**n == name) {
            return i as u32;
        }
        self.names.push(Rc::from(name));
        (self.names.len() - 1) as u32
    }
    fn patch(&mut self, at: usize) {
        let target = self.ops.len() as u32;
        self.patch_to(at, target);
    }
    fn patch_to(&mut self, at: usize, target: u32) {
        match &mut self.ops[at] {
            Op::Jump(t)
            | Op::AbruptJump(t, _)
            | Op::JumpIfFalse(t)
            | Op::JumpIfFalsePeek(t)
            | Op::JumpIfTruePeek(t)
            | Op::JumpIfNotNullishPeek(t)
            | Op::InlineGuard(_, t) => *t = target,
            _ => unreachable!("patching a non-jump"),
        }
    }

    /// Declare the function body's top-level lexical bindings: captured ones home in the activation
    /// env (inserted in TDZ by `make_run_env`), the rest get TDZ slots. Function declarations
    /// were already handled by the hoist plan; classes and resource declarations follow the same
    /// lexical initialization split.
    fn declare_body_lexicals(
        &mut self,
        stmts: &[Stmt],
        captured: &std::collections::HashSet<String>,
    ) -> CResult {
        for s in stmts {
            let s = match s {
                Stmt::ExportDecl(inner) => &**inner,
                Stmt::ExportDefault(inner)
                    if matches!(&**inner, Stmt::Expr(_))
                        || matches!(&**inner, Stmt::FuncDecl(function) if function.name.is_none())
                        || matches!(&**inner, Stmt::ClassDecl(class) if class.name.is_none()) =>
                {
                    continue;
                }
                Stmt::ExportDefault(inner) => &**inner,
                other => other,
            };
            match s {
                Stmt::VarDecl {
                    kind:
                        kind
                        @ (DeclKind::Let | DeclKind::Const | DeclKind::Using | DeclKind::AwaitUsing),
                    decls,
                } => {
                    let is_const = matches!(
                        kind,
                        DeclKind::Const | DeclKind::Using | DeclKind::AwaitUsing
                    );
                    for (pat, _) in decls {
                        self.declare_body_lexical_pattern(pat, is_const, captured)?;
                    }
                }
                Stmt::ClassDecl(class) => {
                    let name = class.name.as_ref().ok_or(Bail)?;
                    if self.module_body && self.env_has(name) {
                        // ModuleDeclarationInstantiation created the mutable TDZ binding.
                    } else if captured.contains(name) {
                        self.cap_inits
                            .push(CapInit::Lexical(Rc::from(name.as_str()), false));
                        self.env_bind(name, false);
                    } else {
                        let slot = self.fresh_slot(name);
                        self.scope_bind(name, slot, false);
                        self.tdz_slots.insert(slot);
                        self.emit(Op::Tdz(slot));
                    }
                }
                Stmt::FuncDecl(_) => {} // hoisted — created at entry
                _ => {}
            }
        }
        Ok(())
    }

    /// FunctionDeclarationInstantiation creates every top-level lexical binding before body
    /// evaluation. Split a binding pattern leaf-by-leaf: captured names live in the activation
    /// Environment Record, while unobserved names retain the cheaper slot representation. Both
    /// remain uninitialized until the declaration's BindingInitialization reaches that leaf.
    fn declare_body_lexical_pattern(
        &mut self,
        pattern: &Pattern,
        is_const: bool,
        captured: &std::collections::HashSet<String>,
    ) -> CResult {
        match pattern {
            Pattern::Ident(name) if self.module_body && self.env_has(name) => Ok(()),
            Pattern::Ident(name) if captured.contains(name) => {
                self.cap_inits
                    .push(CapInit::Lexical(Rc::from(name.as_str()), is_const));
                self.env_bind(name, is_const);
                Ok(())
            }
            Pattern::Ident(_) => self.declare_lexical_pattern(pattern, is_const),
            Pattern::Object(object) => {
                for property in &object.props {
                    self.declare_body_lexical_pattern(&property.value, is_const, captured)?;
                }
                if let Some(rest) = &object.rest {
                    self.declare_body_lexical_pattern(
                        &Pattern::Ident(rest.clone()),
                        is_const,
                        captured,
                    )?;
                }
                Ok(())
            }
            Pattern::Array(elements) => {
                for element in elements {
                    match element {
                        ArrayPatElem::Hole => {}
                        ArrayPatElem::Elem { pattern, .. } | ArrayPatElem::Rest(pattern) => {
                            self.declare_body_lexical_pattern(pattern, is_const, captured)?;
                        }
                    }
                }
                Ok(())
            }
            Pattern::Member(_) => Err(Bail),
        }
    }

    /// Instantiate a statement list's block-scoped declarations at block entry. Per ECMA-262
    /// BlockDeclarationInstantiation, every binding exists before statement evaluation and a
    /// block FunctionDeclaration is initialized immediately rather than when its statement is
    /// reached. CaptureScan keeps recursive/closure-captured block functions on the tree-walker;
    /// the slot path here is therefore the exact, allocation-light representation of the
    /// uncaptured subset.
    fn declare_block_lexicals(&mut self, stmts: &[Stmt]) -> CResult {
        for s in stmts {
            match s {
                Stmt::VarDecl {
                    kind: DeclKind::Let | DeclKind::Const | DeclKind::Using | DeclKind::AwaitUsing,
                    decls,
                } => {
                    let is_const = matches!(
                        s,
                        Stmt::VarDecl {
                            kind: DeclKind::Const | DeclKind::Using | DeclKind::AwaitUsing,
                            ..
                        }
                    );
                    for (pat, _) in decls {
                        self.declare_lexical_pattern(pat, is_const)?;
                    }
                }
                Stmt::FuncDecl(function) => {
                    let name = function.name.as_ref().ok_or(Bail)?;
                    self.declare_lexical_pattern(&Pattern::Ident(name.clone()), false)?;
                }
                Stmt::ClassDecl(class) => {
                    let name = class.name.as_ref().ok_or(Bail)?;
                    self.declare_lexical_pattern(&Pattern::Ident(name.clone()), false)?;
                }
                _ => {}
            }
        }
        // InstantiateFunctionObject uses the current lexical environment and initializes the
        // mutable block binding before the first statement executes. Do this after reserving all
        // slots so source-order declarations resolve against the complete block scope.
        for s in stmts {
            if let Stmt::FuncDecl(function) = s {
                let name = function.name.as_ref().ok_or(Bail)?;
                self.emit_closure(function, None);
                if self.current_lexical_env_has(name) {
                    let name = self.name_idx(name);
                    self.emit(Op::InitLex(name));
                } else {
                    let Home::Slot(slot, false) = self.home(name).ok_or(Bail)? else {
                        return Err(Bail);
                    };
                    self.emit(Op::StoreLocal(slot));
                }
            }
        }
        Ok(())
    }

    fn stmt(&mut self, s: &Stmt) -> CResult {
        match s {
            Stmt::Expr(e) => self.expr_stmt(e),
            Stmt::Empty | Stmt::Debugger => Ok(()),
            // Top-level function declarations were hoisted at function entry; block-level ones
            // were initialized by BlockDeclarationInstantiation at block entry. Annex B.3.2 adds
            // one declaration-time step: copy that lexical function object into its separate
            // function-scope var binding.
            Stmt::FuncDecl(function) => {
                let Some(target) = self
                    .annexb_targets
                    .get(&(Rc::as_ptr(function) as usize))
                    .copied()
                else {
                    return Ok(());
                };
                let name = function.name.as_ref().ok_or(Bail)?;
                self.expr(&Expr::Ident(name.clone()))?;
                match target {
                    Home::Slot(slot, false) => self.emit(Op::StoreLocal(slot)),
                    Home::Env(false) => {
                        let name = self.name_idx(name);
                        self.emit(Op::StoreCap(name))
                    }
                    Home::Slot(_, true) | Home::Env(true) => {
                        unreachable!("Annex B promotion target is mutable")
                    }
                };
                Ok(())
            }
            Stmt::ClassDecl(class) => {
                self.expr(&Expr::Class(class.clone()))?;
                let name = class.name.as_ref().ok_or(Bail)?;
                if self.current_lexical_env_has(name) {
                    let name = self.name_idx(name);
                    self.emit(Op::InitLex(name));
                    return Ok(());
                }
                match self.home(name).ok_or(Bail)? {
                    Home::Slot(slot, _) => self.emit(Op::StoreLocal(slot)),
                    Home::Env(_) => {
                        let name = self.name_idx(name);
                        self.emit(Op::StoreCapInit(name))
                    }
                };
                Ok(())
            }
            Stmt::VarDecl { kind, decls } => {
                for (pat, init) in decls {
                    let Pattern::Ident(name) = pat else {
                        if matches!(kind, DeclKind::Using | DeclKind::AwaitUsing) {
                            return Err(Bail);
                        }
                        // Destructuring declaration: evaluate the initializer, then lower the
                        // pattern against it (a pattern without an initializer is a parse error).
                        let Some(e) = init else { return Err(Bail) };
                        self.expr(e)?;
                        self.destructure_store(pat, *kind)?;
                        continue;
                    };
                    let dynamic_lexical = self.current_lexical_env_has(name);
                    let home = if dynamic_lexical {
                        None
                    } else {
                        Some(self.home(name).ok_or(Bail)?)
                    };
                    match init {
                        Some(e) => self.named_expr(e, name)?,
                        // `var x;` leaves an existing binding alone; `let x;` initializes.
                        None => {
                            if matches!(kind, DeclKind::Var) {
                                continue;
                            }
                            self.emit(Op::Undef);
                        }
                    }
                    if matches!(kind, DeclKind::Using | DeclKind::AwaitUsing) {
                        // UsingDeclaration Evaluation calls AddDisposableResource before
                        // InitializeBinding. AddDisposable peeks so the same value then initializes
                        // the immutable lexical home without a clone on the common primitive path.
                        self.emit(Op::AddDisposable(matches!(kind, DeclKind::AwaitUsing)));
                    }
                    if dynamic_lexical {
                        let name = self.name_idx(name);
                        self.emit(if matches!(kind, DeclKind::Var) {
                            Op::StoreName(name)
                        } else {
                            Op::InitLex(name)
                        });
                        continue;
                    }
                    match home.expect("non-dynamic declaration has a static home") {
                        Home::Slot(slot, _) => {
                            self.emit(Op::StoreLocal(slot));
                        }
                        Home::Env(_) => {
                            let n = self.name_idx(name);
                            // A lexical declaration initializes (clearing TDZ); a `var` writes an
                            // already-initialized binding.
                            if matches!(kind, DeclKind::Var) {
                                self.emit(Op::StoreCap(n));
                            } else {
                                self.emit(Op::StoreCapInit(n));
                            }
                        }
                    }
                }
                Ok(())
            }
            Stmt::Return(arg) => {
                if self.strict
                    && !self.is_coroutine
                    && arg.as_ref().is_some_and(has_tail_tagged_template)
                {
                    return Err(Bail);
                }
                // Inside a spliced callee body, `return v` is "leave v on the stack and jump to
                // the join point" (the plan guarantees no handlers/for-of regions to unwind:
                // the callee chunk contains no PushHandler).
                if self.inline_depth > 0 {
                    match arg {
                        Some(e) => self.expr(e)?,
                        None => {
                            self.emit(Op::Undef);
                        }
                    }
                    let j = self.emit(Op::Jump(0));
                    self.inline_returns.push(j);
                    return Ok(());
                }
                let explicit = if let Some(e) = arg {
                    self.expr(e)?;
                    true
                } else {
                    false
                };
                if !self.is_coroutine {
                    // Ordinary/JIT frames do not return through drive_vm. Preserve their direct
                    // cleanup lowering; coroutine frames use completion-aware iterator handlers
                    // so injected returns and suspending finalizers share the normative path.
                    let fors: Vec<(u16, u32)> = self
                        .loops
                        .iter()
                        .filter_map(|ctx| ctx.foreach_iter.map(|it| (it, ctx.body_try_depth)))
                        .collect();
                    if fors.len() > 1 {
                        return Err(Bail);
                    }
                    if let Some(&(iter_s, body_depth)) = fors.first() {
                        for _ in body_depth..self.try_depth {
                            self.emit(Op::PopHandler);
                        }
                        self.emit(Op::PopHandler);
                        self.emit(Op::IterCloseL(iter_s));
                    }
                }
                // ECMA-262 §14.10.1 distinguishes `return;` from `return Expression;` in an
                // async generator: only the expression form performs Await, even when that
                // expression evaluates to undefined. Both remain abrupt completions so active
                // finalizers and for-of IteratorClose handlers see them in normative order.
                if explicit {
                    self.emit(Op::Return);
                } else {
                    self.emit(Op::ReturnBare);
                }
                Ok(())
            }
            Stmt::Throw(e) => {
                self.expr(e)?;
                self.emit(Op::Throw);
                Ok(())
            }
            Stmt::If { test, cons, alt } => {
                self.expr(test)?;
                let jf = self.emit(Op::JumpIfFalse(0));
                self.stmt(cons)?;
                match alt {
                    Some(a) => {
                        let jend = self.emit(Op::Jump(0));
                        self.patch(jf);
                        self.stmt(a)?;
                        self.patch(jend);
                    }
                    None => self.patch(jf),
                }
                Ok(())
            }
            Stmt::Block(body) => {
                self.push_compile_scope();
                let r = self.block_body(body);
                self.pop_compile_scope();
                r
            }
            Stmt::While { test, body } => {
                let labels = std::mem::take(&mut self.pending_labels);
                let start = self.ops.len();
                self.expr(test)?;
                let jf = self.emit(Op::JumpIfFalse(0));
                self.loops.push(LoopCtx {
                    labels,
                    entry_try_depth: self.try_depth,
                    ..LoopCtx::default()
                });
                let r = self.stmt(body);
                let ctx = self.loops.pop().unwrap();
                r?;
                for c in ctx.continues {
                    self.patch_to(c, start as u32);
                }
                self.emit(Op::Jump(start as u32));
                self.patch(jf);
                for b in ctx.breaks {
                    self.patch(b);
                }
                Ok(())
            }
            Stmt::DoWhile { body, test } => {
                let labels = std::mem::take(&mut self.pending_labels);
                let start = self.ops.len();
                self.loops.push(LoopCtx {
                    labels,
                    entry_try_depth: self.try_depth,
                    ..LoopCtx::default()
                });
                let r = self.stmt(body);
                let ctx = self.loops.pop().unwrap();
                r?;
                let cont = self.ops.len();
                for c in ctx.continues {
                    self.patch_to(c, cont as u32);
                }
                self.expr(test)?;
                let jf = self.emit(Op::JumpIfFalse(0));
                self.emit(Op::Jump(start as u32));
                self.patch(jf);
                for b in ctx.breaks {
                    self.patch(b);
                }
                Ok(())
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                self.push_compile_scope();
                let r = self.for_loop(init.as_deref(), test.as_ref(), update.as_ref(), body);
                self.pop_compile_scope();
                r
            }
            Stmt::Break(None) => {
                let idx = self
                    .loops
                    .iter()
                    .rposition(|ctx| !ctx.is_label_block)
                    .ok_or(Bail)?;
                let j = self.emit_exit_jump(idx, false)?;
                self.loops[idx].breaks.push(j);
                Ok(())
            }
            Stmt::Continue(None) => {
                // `continue` skips switch contexts: it targets the innermost enclosing *loop*.
                let idx = self
                    .loops
                    .iter()
                    .rposition(|c| !c.is_switch && !c.is_label_block)
                    .ok_or(Bail)?;
                let j = self.emit_exit_jump(idx, true)?;
                self.loops[idx].continues.push(j);
                Ok(())
            }
            // Labelled break/continue: jump to the control context carrying the target label.
            Stmt::Break(Some(name)) => {
                let idx = self
                    .loops
                    .iter()
                    .rposition(|c| c.labels.iter().any(|l| l == name))
                    .ok_or(Bail)?;
                let j = self.emit_exit_jump(idx, false)?;
                self.loops[idx].breaks.push(j);
                Ok(())
            }
            Stmt::Continue(Some(name)) => {
                // A labelled continue must target a loop — a label on a switch is only a break
                // target (the parser rejects `continue` to it; not-found bails to the oracle).
                let idx = self
                    .loops
                    .iter()
                    .rposition(|c| {
                        !c.is_switch && !c.is_label_block && c.labels.iter().any(|l| l == name)
                    })
                    .ok_or(Bail)?;
                let j = self.emit_exit_jump(idx, true)?;
                self.loops[idx].continues.push(j);
                Ok(())
            }
            // A label naming a loop or switch attaches to that context; stacked labels
            // (`a: b: for`) accumulate through the recursion. Any other labelled statement gets
            // a break-only context, matching ECMA-262 §14.13.4 LabelledEvaluation.
            Stmt::Labeled { label, body } => match &**body {
                Stmt::While { .. }
                | Stmt::DoWhile { .. }
                | Stmt::For { .. }
                | Stmt::Switch { .. }
                | Stmt::Labeled { .. } => {
                    self.pending_labels.push(label.clone());
                    self.stmt(body)
                }
                _ => {
                    let mut labels = std::mem::take(&mut self.pending_labels);
                    labels.push(label.clone());
                    self.loops.push(LoopCtx {
                        labels,
                        entry_try_depth: self.try_depth,
                        is_label_block: true,
                        ..LoopCtx::default()
                    });
                    let result = self.stmt(body);
                    let ctx = self.loops.pop().expect("just pushed labelled context");
                    result?;
                    for jump in ctx.breaks {
                        self.patch(jump);
                    }
                    Ok(())
                }
            },
            // `switch`: evaluate the discriminant in the outer environment first, then perform
            // one BlockDeclarationInstantiation shared by every case before any case test
            // (ECMA-262 §14.12.4). Case tests run in source order and bodies remain contiguous,
            // so fall-through is just falling through.
            Stmt::Switch { disc, cases } => {
                self.expr(disc)?;
                let tmp = self.fresh_slot("%switch%");
                self.emit(Op::StoreLocal(tmp));
                self.push_compile_scope();
                let mut bindings = Vec::new();
                for case in cases {
                    for binding in self.runtime_block_bindings(&case.body) {
                        if !bindings.iter().any(|(name, _)| name == &binding.0) {
                            bindings.push(binding);
                        }
                    }
                }
                let result = if bindings.is_empty() {
                    self.switch_body(tmp, cases)
                } else {
                    let scope = self.push_runtime_lexical_scope(bindings);
                    self.environment_scope(|compiler| compiler.switch_body(tmp, cases), scope)
                };
                self.pop_compile_scope();
                result
            }
            // `try` completion handling follows ECMA-262 §14.15.3. Catch consumes only a throw.
            // A finally handler intercepts throw/return/break/continue, records that Completion
            // in hidden slots, runs the finalizer once, then re-issues the saved Completion unless
            // the finalizer itself completed abruptly. The same representation also survives a
            // coroutine finalizer's suspension.
            Stmt::Try {
                block,
                handler,
                finalizer,
            } => {
                if let Some(finalizer) = finalizer {
                    let completion_slot = self.fresh_slot("%finally-value%");
                    let completion_kind = self.fresh_slot("%finally-kind%");
                    let completion_target_depth = self.fresh_slot("%finally-target-depth%");
                    let push_finally = self.emit(Op::PushFinally(0, 0, 0, 0, 0));
                    self.try_depth += 1;
                    self.finally_depths.push(self.try_depth);

                    if let Some((param, catch_body)) = handler {
                        let push_catch = self.emit(Op::PushHandler(0));
                        self.try_depth += 1;
                        self.push_compile_scope();
                        let try_result = self.block_body(block);
                        self.pop_compile_scope();
                        try_result?;
                        self.emit(Op::PopHandler);
                        self.try_depth -= 1;
                        let after_catch = self.emit(Op::Jump(0));
                        let catch_pc = self.ops.len() as u32;
                        match &mut self.ops[push_catch] {
                            Op::PushHandler(target) => *target = catch_pc,
                            _ => unreachable!(),
                        }
                        self.catch_clause(param.as_ref(), catch_body)?;
                        self.patch(after_catch);
                    } else {
                        self.push_compile_scope();
                        let try_result = self.block_body(block);
                        self.pop_compile_scope();
                        try_result?;
                    }

                    // Normal completion: remove the handler and tag the saved completion as
                    // normal. Throw/return pads are entered only after drive_vm already popped it.
                    self.emit(Op::PopHandler);
                    self.try_depth -= 1;
                    self.finally_depths.pop();
                    let normal = self.const_idx(Value::Num(0.0));
                    self.emit(Op::Const(normal));
                    self.emit(Op::StoreLocal(completion_kind));
                    let normal_to_finally = self.emit(Op::Jump(0));

                    let throw_pc = self.ops.len() as u32;
                    self.emit(Op::StoreLocal(completion_slot));
                    let throwing = self.const_idx(Value::Num(1.0));
                    self.emit(Op::Const(throwing));
                    self.emit(Op::StoreLocal(completion_kind));
                    let throw_to_finally = self.emit(Op::Jump(0));

                    let return_pc = self.ops.len() as u32;
                    self.emit(Op::StoreLocal(completion_slot));
                    let returning = self.const_idx(Value::Num(2.0));
                    self.emit(Op::Const(returning));
                    self.emit(Op::StoreLocal(completion_kind));
                    let return_to_finally = self.emit(Op::Jump(0));

                    let bare_return_pc = self.ops.len() as u32;
                    let bare_returning = self.const_idx(Value::Num(5.0));
                    self.emit(Op::Const(bare_returning));
                    self.emit(Op::StoreLocal(completion_kind));
                    let bare_return_to_finally = self.emit(Op::Jump(0));

                    // AsyncGeneratorUnwrapYieldResumption and async-generator ReturnStatement
                    // have already awaited their values before creating the Return Completion.
                    // Keep those resumptions distinct so finalization cannot await them again.
                    let resume_return_pc = self.ops.len() as u32;
                    self.emit(Op::StoreLocal(completion_slot));
                    let resume_returning = self.const_idx(Value::Num(3.0));
                    self.emit(Op::Const(resume_returning));
                    self.emit(Op::StoreLocal(completion_kind));
                    let resume_return_to_finally = self.emit(Op::Jump(0));

                    // A break/continue pad receives its patched bytecode destination followed by
                    // the handler depth at that destination. Both are exact u32 integers in the
                    // Number representation and survive arbitrary finalizer suspension.
                    let jump_pc = self.ops.len() as u32;
                    self.emit(Op::StoreLocal(completion_target_depth));
                    self.emit(Op::StoreLocal(completion_slot));
                    let jumping = self.const_idx(Value::Num(4.0));
                    self.emit(Op::Const(jumping));
                    self.emit(Op::StoreLocal(completion_kind));

                    match &mut self.ops[push_finally] {
                        Op::PushFinally(
                            throw_target,
                            return_target,
                            bare_return_target,
                            resume_return_target,
                            jump_target,
                        ) => {
                            *throw_target = throw_pc;
                            *return_target = return_pc;
                            *bare_return_target = bare_return_pc;
                            *resume_return_target = resume_return_pc;
                            *jump_target = jump_pc;
                        }
                        _ => unreachable!(),
                    }
                    self.patch(normal_to_finally);
                    self.patch(throw_to_finally);
                    self.patch(return_to_finally);
                    self.patch(bare_return_to_finally);
                    self.patch(resume_return_to_finally);

                    self.push_compile_scope();
                    let finally_result = self.block_body(finalizer);
                    self.pop_compile_scope();
                    finally_result?;

                    self.emit(Op::LoadLocal(completion_kind));
                    self.emit(Op::Const(throwing));
                    self.emit(Op::StrictEq);
                    let not_throw = self.emit(Op::JumpIfFalse(0));
                    self.emit(Op::LoadLocal(completion_slot));
                    self.emit(Op::Throw);
                    self.patch(not_throw);
                    self.emit(Op::LoadLocal(completion_kind));
                    self.emit(Op::Const(returning));
                    self.emit(Op::StrictEq);
                    let not_return = self.emit(Op::JumpIfFalse(0));
                    self.emit(Op::LoadLocal(completion_slot));
                    self.emit(Op::Return);
                    self.patch(not_return);
                    self.emit(Op::LoadLocal(completion_kind));
                    self.emit(Op::Const(bare_returning));
                    self.emit(Op::StrictEq);
                    let not_bare_return = self.emit(Op::JumpIfFalse(0));
                    self.emit(Op::ReturnBare);
                    self.patch(not_bare_return);
                    self.emit(Op::LoadLocal(completion_kind));
                    self.emit(Op::Const(resume_returning));
                    self.emit(Op::StrictEq);
                    let not_resume_return = self.emit(Op::JumpIfFalse(0));
                    self.emit(Op::LoadLocal(completion_slot));
                    self.emit(Op::ResumeReturn);
                    self.patch(not_resume_return);
                    self.emit(Op::LoadLocal(completion_kind));
                    self.emit(Op::Const(jumping));
                    self.emit(Op::StrictEq);
                    let not_jump = self.emit(Op::JumpIfFalse(0));
                    self.emit(Op::LoadLocal(completion_slot));
                    self.emit(Op::LoadLocal(completion_target_depth));
                    self.emit(Op::ResumeJump);
                    self.patch(not_jump);
                    return Ok(());
                }

                let Some((param, catch_body)) = handler else {
                    return Err(Bail);
                };
                let push = self.emit(Op::PushHandler(0));
                self.try_depth += 1;
                self.push_compile_scope();
                let try_result = self.block_body(block);
                self.pop_compile_scope();
                try_result?;
                self.emit(Op::PopHandler);
                self.try_depth -= 1;
                let after_catch = self.emit(Op::Jump(0));
                let catch_pc = self.ops.len() as u32;
                match &mut self.ops[push] {
                    Op::PushHandler(target) => *target = catch_pc,
                    _ => unreachable!(),
                }
                self.catch_clause(param.as_ref(), catch_body)?;
                self.patch(after_catch);
                Ok(())
            }
            // For-in/of iteration keeps its iterator/enumeration record in hidden slots. Sync
            // and async bodies share completion-aware handlers; `for await` adds explicit Await
            // points for stepping and AsyncIteratorClose without reserving a native thread.
            Stmt::ForInOf {
                decl,
                left,
                right,
                of,
                is_await,
                body,
            } => {
                let labels = std::mem::take(&mut self.pending_labels);
                if *is_await && (!*of || !self.is_coroutine) {
                    return Err(Bail);
                }
                // A lexical head is in TDZ while the iterable expression evaluates. Captured
                // leaves use a real head Environment Record and a fresh sibling record per
                // iteration; uncaptured leaves retain the allocation-free slot representation.
                self.push_compile_scope();
                let mut runtime_bindings = Vec::new();
                if let Some(
                    kind @ (DeclKind::Let
                    | DeclKind::Const
                    | DeclKind::Using
                    | DeclKind::AwaitUsing),
                ) = decl
                {
                    let mut names = std::collections::HashSet::new();
                    pat_idents(left, &mut names);
                    let mut names: Vec<_> = names.into_iter().collect();
                    names.sort();
                    runtime_bindings.extend(
                        names
                            .into_iter()
                            .filter(|name| self.runtime_lexicals.contains(name))
                            .map(|name| (name, !matches!(kind, DeclKind::Let))),
                    );
                }
                let runtime_scope = (!runtime_bindings.is_empty())
                    .then(|| self.push_runtime_lexical_scope(runtime_bindings));
                enum Bind {
                    Slot(u16),
                    Cap(u32),
                    Name(u32),
                    Lex(u32),
                    Using {
                        slot: u16,
                        is_async: bool,
                    },
                    UsingLex {
                        name: u32,
                        is_async: bool,
                    },
                    AssignmentMember,
                    /// Destructuring lexical head: bound by `destructure_store` INSIDE the
                    /// body's handler region (a binding throw must IteratorClose in throw mode,
                    /// which is exactly what the body's abort pad does).
                    Pattern(DeclKind),
                }
                let bind = match (left, decl) {
                    (
                        Pattern::Ident(name),
                        Some(kind @ (DeclKind::Using | DeclKind::AwaitUsing)),
                    ) if *of => {
                        if self.current_lexical_env_has(name) {
                            Bind::UsingLex {
                                name: self.name_idx(name),
                                is_async: matches!(kind, DeclKind::AwaitUsing),
                            }
                        } else if self.env_names.contains_key(name) {
                            self.pop_compile_scope();
                            return Err(Bail);
                        } else {
                            let slot = self.fresh_slot(name);
                            self.scope_bind(name, slot, true);
                            self.tdz_slots.insert(slot);
                            self.emit(Op::Tdz(slot));
                            Bind::Using {
                                slot,
                                is_async: matches!(kind, DeclKind::AwaitUsing),
                            }
                        }
                    }
                    (_, Some(DeclKind::Using | DeclKind::AwaitUsing)) => {
                        self.pop_compile_scope();
                        return Err(Bail);
                    }
                    (Pattern::Ident(name), Some(kind @ (DeclKind::Let | DeclKind::Const))) => {
                        if self.current_lexical_env_has(name) {
                            Bind::Lex(self.name_idx(name))
                        } else {
                            // ECMA-262 §14.7.5.5/§14.7.5.8 creates the loop-head lexical in a
                            // fresh Environment Record. An outer activation/module binding with
                            // the same spelling is therefore shadowed, never a reason to reuse or
                            // reject the new slot.
                            let slot = self.fresh_slot(name);
                            self.scope_bind(name, slot, matches!(kind, DeclKind::Const));
                            if matches!(kind, DeclKind::Let | DeclKind::Const) {
                                self.tdz_slots.insert(slot);
                                self.emit(Op::Tdz(slot));
                            }
                            Bind::Slot(slot)
                        }
                    }
                    (pat, Some(kind @ (DeclKind::Let | DeclKind::Const))) => {
                        // A destructuring lexical head: fresh uncaptured slots for every leaf,
                        // declared (in TDZ) before `right` like the ident path. `var` patterns
                        // would have to write hoisted function-scope bindings — those stay in
                        // the oracle.
                        if self
                            .declare_lexical_pattern(pat, matches!(kind, DeclKind::Const))
                            .is_err()
                        {
                            self.pop_compile_scope();
                            return Err(Bail);
                        }
                        Bind::Pattern(*kind)
                    }
                    (Pattern::Ident(name), Some(DeclKind::Var)) => match self.home(name) {
                        // ECMA-262 §14.7.5.7 var-binding writes the already-instantiated
                        // VariableEnvironment binding; it does not create a per-iteration slot.
                        Some(Home::Slot(slot, false)) => Bind::Slot(slot),
                        Some(Home::Env(false)) => Bind::Cap(self.name_idx(name)),
                        None if !self.with_scope_floors.is_empty() => {
                            Bind::Name(self.name_idx(name))
                        }
                        _ => {
                            self.pop_compile_scope();
                            return Err(Bail);
                        }
                    },
                    (pat, Some(DeclKind::Var)) => {
                        // Hoisting created every leaf home at function entry. Pattern binding is
                        // inside the loop-body handler so an abrupt iterator/destructuring step
                        // follows the same IteratorClose path as lexical patterns.
                        let mut leaf_names = std::collections::HashSet::new();
                        pat_idents(pat, &mut leaf_names);
                        if leaf_names.iter().any(|name| self.home(name).is_none()) {
                            self.pop_compile_scope();
                            return Err(Bail);
                        }
                        Bind::Pattern(DeclKind::Var)
                    }
                    (Pattern::Ident(name), None) => match self.home(name) {
                        Some(Home::Slot(slot, is_const)) => {
                            if is_const {
                                self.pop_compile_scope();
                                return Err(Bail);
                            }
                            Bind::Slot(slot)
                        }
                        Some(Home::Env(is_const)) => {
                            if is_const {
                                self.pop_compile_scope();
                                return Err(Bail);
                            }
                            Bind::Cap(self.name_idx(name))
                        }
                        None => Bind::Name(self.name_idx(name)),
                    },
                    (Pattern::Member(_), None) => Bind::AssignmentMember,
                    // Destructuring *assignment* heads are represented by the original covered
                    // Array/ObjectLiteral in Pattern::Member. They lower through AssignTarget
                    // below, retaining exact Reference-before-read and inner IteratorClose order.
                    (_, None) => Bind::AssignmentMember,
                };
                let er = if let Some(scope) = runtime_scope {
                    self.environment_scope(|compiler| compiler.expr(right), scope)
                } else {
                    self.expr(right)
                };
                if er.is_err() {
                    self.pop_compile_scope();
                    return er;
                }
                if !of {
                    // ForIn/OfHeadEvaluation(enumerate): snapshot candidate keys once, then check
                    // whether each object property still exists immediately before visiting it.
                    // Null/undefined and non-string primitives naturally produce no candidates.
                    let source_s = self.fresh_slot("%for-in-source%");
                    let keys_s = self.fresh_slot("%for-in-keys%");
                    let index_s = self.fresh_slot("%for-in-index%");
                    self.emit(Op::Dup);
                    self.emit(Op::StoreLocal(source_s));
                    self.emit(Op::ForInKeys);
                    self.emit(Op::StoreLocal(keys_s));
                    let zero = self.const_idx(Value::Num(0.0));
                    self.emit(Op::Const(zero));
                    self.emit(Op::StoreLocal(index_s));
                    self.loops.push(LoopCtx {
                        labels,
                        entry_try_depth: self.try_depth,
                        ..LoopCtx::default()
                    });
                    let loop_head = self.ops.len();
                    self.emit(Op::ForInStepL(keys_s, index_s, source_s));
                    let jexit = self.emit(Op::JumpIfFalse(0));
                    if let Some(scope) = runtime_scope {
                        self.emit(Op::PushLex(scope));
                    }
                    let compile_iteration = |compiler: &mut Compiler| match bind {
                        Bind::Slot(slot) => {
                            compiler.emit(Op::StoreLocal(slot));
                            compiler.stmt(body)
                        }
                        Bind::Cap(name) => {
                            compiler.emit(Op::StoreCap(name));
                            compiler.stmt(body)
                        }
                        Bind::Name(name) => {
                            compiler.emit_store_name(name);
                            compiler.stmt(body)
                        }
                        Bind::Lex(name) => {
                            compiler.emit(Op::InitLex(name));
                            compiler.stmt(body)
                        }
                        Bind::Using { .. } | Bind::UsingLex { .. } => Err(Bail),
                        Bind::AssignmentMember => compiler
                            .for_head_member_store(left)
                            .and_then(|()| compiler.stmt(body)),
                        Bind::Pattern(kind) => compiler
                            .destructure_store(left, kind)
                            .and_then(|()| compiler.stmt(body)),
                    };
                    let result = if let Some(scope) = runtime_scope {
                        self.environment_scope(compile_iteration, scope)
                    } else {
                        compile_iteration(self)
                    };
                    let ctx = self.loops.pop().expect("just pushed");
                    self.pop_compile_scope();
                    result?;
                    for jump in ctx.continues {
                        self.patch_to(jump, loop_head as u32);
                    }
                    self.emit(Op::Jump(loop_head as u32));
                    self.patch(jexit);
                    self.emit(Op::Pop); // exhausted-step placeholder
                    for jump in ctx.breaks {
                        self.patch(jump);
                    }
                    return Ok(());
                }
                let iter_s = self.fresh_slot("%iter%");
                let next_s = self.fresh_slot("%next%");
                let async_from_sync_s = if *is_await {
                    let slot = self.fresh_slot("%async-from-sync%");
                    self.emit(Op::GetAsyncIter);
                    self.emit(Op::StoreLocal(slot));
                    self.emit(Op::StoreLocal(next_s));
                    self.emit(Op::StoreLocal(iter_s));
                    Some(slot)
                } else {
                    self.emit(Op::GetIter);
                    self.emit(Op::StoreLocal(next_s));
                    self.emit(Op::StoreLocal(iter_s));
                    None
                };
                self.loops.push(LoopCtx {
                    labels,
                    entry_try_depth: self.try_depth,
                    foreach_iter: Some(iter_s),
                    foreach_async_from_sync: async_from_sync_s,
                    ..Default::default()
                });
                let loop_head = self.ops.len();
                if let Some(from_sync_s) = async_from_sync_s {
                    let done_s = self.fresh_slot("%async-step-done%");
                    self.emit(Op::AsyncIterStepL(iter_s, next_s, from_sync_s, done_s));
                    self.emit(Op::AsyncIterResumeL(from_sync_s, done_s));
                } else {
                    self.emit(Op::IterStepL(iter_s, next_s));
                }
                let jexit = self.emit(Op::JumpIfFalse(0));
                // ForIn/OfBodyEvaluation includes every form of BindingInitialization and
                // assignment in the status whose abrupt completion closes the iterator. Push the
                // body handler before even the identifier stores: a strict unresolved assignment
                // or environment operation can be abrupt just like a member/pattern PutValue.
                let push = if self.is_coroutine {
                    self.emit(Op::PushIterator(0, 0, 0, 0))
                } else {
                    self.emit(Op::PushHandler(0))
                };
                self.try_depth += 1;
                self.loops.last_mut().expect("just pushed").body_try_depth = self.try_depth;
                if let Some(scope) = runtime_scope {
                    self.emit(Op::PushLex(scope));
                }
                let compile_iteration = |compiler: &mut Compiler| match bind {
                    Bind::Slot(slot) => {
                        compiler.emit(Op::StoreLocal(slot));
                        compiler.stmt(body)
                    }
                    Bind::Cap(name) => {
                        compiler.emit(Op::StoreCap(name));
                        compiler.stmt(body)
                    }
                    Bind::Name(name) => {
                        compiler.emit_store_name(name);
                        compiler.stmt(body)
                    }
                    Bind::Lex(name) => {
                        compiler.emit(Op::InitLex(name));
                        compiler.stmt(body)
                    }
                    Bind::Using { slot, is_async } => compiler.disposal_scope(|compiler| {
                        compiler.emit(Op::AddDisposable(is_async));
                        compiler.emit(Op::StoreLocal(slot));
                        compiler.stmt(body)
                    }),
                    Bind::UsingLex { name, is_async } => compiler.disposal_scope(|compiler| {
                        compiler.emit(Op::AddDisposable(is_async));
                        compiler.emit(Op::InitLex(name));
                        compiler.stmt(body)
                    }),
                    Bind::Pattern(kind) => compiler
                        .destructure_store(left, kind)
                        .and_then(|()| compiler.stmt(body)),
                    Bind::AssignmentMember => compiler
                        .for_head_member_store(left)
                        .and_then(|()| compiler.stmt(body)),
                };
                let r = if let Some(scope) = runtime_scope {
                    self.environment_scope(compile_iteration, scope)
                } else {
                    compile_iteration(self)
                };
                let ctx = self.loops.pop().expect("just pushed");
                self.pop_compile_scope();
                r?;
                self.emit(Op::PopHandler);
                self.try_depth -= 1;
                // continues re-enter at the step (the loop head re-pushes the body handler —
                // their cleanup already popped it).
                for j in ctx.continues {
                    self.patch_to(j, loop_head as u32);
                }
                self.emit(Op::Jump(0));
                let jback = self.ops.len() - 1;
                match &mut self.ops[jback] {
                    Op::Jump(t) => *t = loop_head as u32,
                    _ => unreachable!(),
                }
                // The body's completion pads implement ForIn/OfBodyEvaluation step 13. Throw
                // closes in throw mode (preserving the original error); every return closes in
                // normal mode, where a close error replaces the saved completion.
                let abort_pc = self.ops.len() as u32;
                if let Some(from_sync_s) = async_from_sync_s {
                    self.emit(Op::AsyncIterCloseL(iter_s, from_sync_s, true));
                    self.emit(Op::Throw);
                } else {
                    self.emit(Op::IterAbortL(iter_s));
                }
                if self.is_coroutine {
                    let return_pc = self.ops.len() as u32;
                    if let Some(from_sync_s) = async_from_sync_s {
                        self.emit(Op::AsyncIterCloseL(iter_s, from_sync_s, false));
                    } else {
                        self.emit(Op::IterCloseL(iter_s));
                    }
                    self.emit(Op::Return);
                    let bare_return_pc = self.ops.len() as u32;
                    if let Some(from_sync_s) = async_from_sync_s {
                        self.emit(Op::AsyncIterCloseL(iter_s, from_sync_s, false));
                    } else {
                        self.emit(Op::IterCloseL(iter_s));
                    }
                    self.emit(Op::ReturnBare);
                    let resume_return_pc = self.ops.len() as u32;
                    if let Some(from_sync_s) = async_from_sync_s {
                        self.emit(Op::AsyncIterCloseL(iter_s, from_sync_s, false));
                    } else {
                        self.emit(Op::IterCloseL(iter_s));
                    }
                    self.emit(Op::ResumeReturn);
                    match &mut self.ops[push] {
                        Op::PushIterator(
                            throw_target,
                            return_target,
                            bare_return_target,
                            resume_return_target,
                        ) => {
                            *throw_target = abort_pc;
                            *return_target = return_pc;
                            *bare_return_target = bare_return_pc;
                            *resume_return_target = resume_return_pc;
                        }
                        _ => unreachable!(),
                    }
                } else {
                    match &mut self.ops[push] {
                        Op::PushHandler(throw_target) => *throw_target = abort_pc,
                        _ => unreachable!(),
                    }
                }
                // Exhaustion lands here (the step's bool was false): drop the undefined
                // placeholder the step pushed; no close on a completed iterator.
                self.patch(jexit);
                self.emit(Op::Pop);
                // Breaks jump here too — their cleanup (pop handler + close) ran at the site.
                let after = self.ops.len() as u32;
                for j in ctx.breaks {
                    self.patch_to(j, after);
                }
                Ok(())
            }
            // Source Text Module declarations are link-time metadata. Exported declarations still
            // execute normally against the module environment; default expressions and anonymous
            // classes initialize the synthetic `*default*` live binding using NamedEvaluation.
            Stmt::Import(_) | Stmt::ExportNamed { .. } | Stmt::ExportAll { .. }
                if self.module_body =>
            {
                Ok(())
            }
            Stmt::ExportDecl(inner) if self.module_body => self.stmt(inner),
            Stmt::ExportDefault(inner) if self.module_body => match &**inner {
                Stmt::Expr(expression) => {
                    self.named_expr(expression, "default")?;
                    let name = self.name_idx("*default*");
                    self.emit(Op::StoreCapInit(name));
                    Ok(())
                }
                Stmt::FuncDecl(function) if function.name.is_none() => Ok(()),
                Stmt::ClassDecl(class) if class.name.is_none() => {
                    self.named_expr(&Expr::Class(class.clone()), "default")?;
                    let name = self.name_idx("*default*");
                    self.emit(Op::StoreCapInit(name));
                    Ok(())
                }
                declaration => self.stmt(declaration),
            },
            Stmt::With { obj, body } if self.is_coroutine => self.with_scope(obj, body),
            other => {
                log_bail("stmt", &format!("{:.60}", format!("{other:?}")));
                Err(Bail)
            }
        }
    }

    /// Store the iterator/enumeration value on top of the stack into a member-expression loop
    /// head. ForIn/OfBodyEvaluation obtains the value first, then evaluates the LHS Reference on
    /// every iteration; the hidden slot keeps that ordering while evaluating base and key once.
    fn for_head_member_store(&mut self, target: &Pattern) -> CResult {
        let Pattern::Member(target) = target else {
            return Err(Bail);
        };
        let value_slot = self.fresh_slot("%for-head-value%");
        self.emit(Op::StoreLocal(value_slot));
        match &**target {
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.expr(obj)?;
                self.emit(Op::LoadLocal(value_slot));
                let name = self.name_idx(prop);
                let cache = self.new_cache(name);
                self.emit(Op::SetPropDrop(name, cache));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                self.expr(obj)?;
                self.expr(index)?;
                self.emit(Op::LoadLocal(value_slot));
                self.emit(Op::SetElemDrop);
                Ok(())
            }
            target @ (Expr::Array(_) | Expr::Object(_)) => {
                // A direct yield/await in a default, computed key, or target expression must be
                // represented as VM suspension points rather than entered through the oracle.
                let suspends = crate::eval::expr_contains(target, |expr| {
                    matches!(expr, Expr::Yield { .. } | Expr::Await(_))
                });
                if self.is_coroutine && suspends {
                    self.emit(Op::LoadLocal(value_slot));
                    return self.assignment_pattern(target);
                }
                let locals = self.projected_locals();
                let index = self.assignment_targets.len() as u32;
                self.assignment_targets.push(AssignmentTargetPlan {
                    target: target.clone(),
                    locals,
                    strict: self.strict,
                });
                self.emit(Op::LoadLocal(value_slot));
                self.emit(Op::AssignTarget(index));
                Ok(())
            }
            _ => Err(Bail),
        }
    }

    fn projected_locals(&self) -> Vec<AssignmentLocal> {
        let mut seen = std::collections::HashSet::new();
        let mut locals = Vec::new();
        for scope in self.scopes.iter().rev() {
            for (name, slot, is_const) in scope.iter().rev() {
                if !name.starts_with('%') && seen.insert(name.clone()) {
                    locals.push(AssignmentLocal {
                        name: Rc::from(name.as_str()),
                        slot: *slot,
                        mutable: !*is_const,
                    });
                }
            }
        }
        locals
    }

    fn retain_eval_expr(&mut self, expr: &Expr) {
        self.retain_named_eval_expr(expr, None);
    }

    fn retain_named_eval_expr(&mut self, expr: &Expr, name: Option<&str>) {
        // The projected evaluator resolves `this` through an Environment Record. Keeping one in
        // every bridged frame is rare-path overhead only and also covers super-property helpers.
        self.env_this = true;
        let plan = self.eval_exprs.len() as u32;
        self.eval_exprs.push(EvalExprPlan {
            expr: expr.clone(),
            locals: self.projected_locals(),
            strict: self.strict,
            name: name.map(str::to_string),
        });
        self.emit(Op::EvalExpr(plan));
    }

    fn prepare_assignment_reference(
        &mut self,
        target: &Expr,
    ) -> Result<PreparedAssignmentRef, Bail> {
        match target {
            Expr::Paren(inner) => self.prepare_assignment_reference(inner),
            Expr::Ident(name) => {
                let name_index = self.name_idx(name);
                Ok(match self.home(name) {
                    Some(Home::Slot(slot, is_const)) => PreparedAssignmentRef::Local {
                        slot,
                        is_const,
                        name: name_index,
                    },
                    Some(Home::Env(is_const)) => PreparedAssignmentRef::Captured {
                        name: name_index,
                        is_const,
                    },
                    None if self.name_reference_can_change(name) => {
                        let reference = self.fresh_reference()?;
                        self.emit(Op::ResolveNameRef(name_index, reference));
                        PreparedAssignmentRef::Reference(reference)
                    }
                    None => PreparedAssignmentRef::Name(name_index),
                })
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.expr(obj)?;
                self.emit(Op::RequireObject);
                let base = self.fresh_slot("%assignment-base%");
                self.emit(Op::StoreLocal(base));
                let name = self.name_idx(prop);
                let cache = self.new_cache(name);
                Ok(PreparedAssignmentRef::Property { base, name, cache })
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                self.expr(obj)?;
                self.expr(index)?;
                // EvaluatePropertyAccessWithExpressionKey performs RequireObjectCoercible and
                // ToPropertyKey while creating the Reference, before any later iterator step.
                self.emit(Op::ToPropKey);
                let key = self.fresh_slot("%assignment-key%");
                let base = self.fresh_slot("%assignment-base%");
                self.emit(Op::StoreLocal(key));
                self.emit(Op::StoreLocal(base));
                Ok(PreparedAssignmentRef::Element { base, key })
            }
            _ => Err(Bail),
        }
    }

    fn put_assignment_reference(&mut self, reference: PreparedAssignmentRef) {
        match reference {
            PreparedAssignmentRef::Local {
                slot,
                is_const,
                name,
            } => {
                self.emit(if is_const {
                    Op::StoreConstLocal(slot, name)
                } else {
                    Op::StoreLocal(slot)
                });
            }
            PreparedAssignmentRef::Captured { name, is_const } => {
                self.emit(if is_const {
                    Op::StoreConstCap(name)
                } else {
                    Op::StoreCap(name)
                });
            }
            PreparedAssignmentRef::Name(name) => {
                self.emit_store_name(name);
            }
            PreparedAssignmentRef::Reference(reference) => {
                self.emit(Op::StoreRef(reference));
            }
            PreparedAssignmentRef::Property { base, name, cache } => {
                let value = self.fresh_slot("%assignment-value%");
                self.emit(Op::StoreLocal(value));
                self.emit(Op::LoadLocal(base));
                self.emit(Op::LoadLocal(value));
                self.emit(Op::SetPropDrop(name, cache));
            }
            PreparedAssignmentRef::Element { base, key } => {
                let value = self.fresh_slot("%assignment-value%");
                self.emit(Op::StoreLocal(value));
                self.emit(Op::LoadLocal(base));
                self.emit(Op::LoadLocal(key));
                self.emit(Op::LoadLocal(value));
                self.emit(Op::SetElemDrop);
            }
        }
    }

    fn assignment_default(&mut self, core: &Expr, default: Option<&Expr>) -> CResult {
        let Some(default) = default else {
            return Ok(());
        };
        self.emit(Op::Dup);
        self.emit(Op::Undef);
        self.emit(Op::StrictEq);
        let present = self.emit(Op::JumpIfFalse(0));
        self.emit(Op::Pop);
        if let Expr::Ident(name) = core {
            self.named_expr(default, name)?;
        } else {
            self.expr(default)?;
        }
        self.patch(present);
        Ok(())
    }

    fn assignment_object_key(&mut self, source: u16, key: &PropKey) -> Result<u16, Bail> {
        self.emit(Op::LoadLocal(source));
        match key {
            PropKey::Ident(key) => {
                let value = self.const_idx(Value::from_string(key.clone()));
                self.emit(Op::Const(value));
            }
            PropKey::Str(key) => {
                let value = self.const_idx(Value::Str(key.clone().into()));
                self.emit(Op::Const(value));
            }
            PropKey::Num(key) => {
                let value = self.const_idx(Value::Num(*key));
                self.emit(Op::Const(value));
            }
            PropKey::Computed(key) => self.expr(key)?,
        }
        self.emit(Op::ToPropKey);
        let key_slot = self.fresh_slot("%assignment-object-key%");
        self.emit(Op::StoreLocal(key_slot));
        self.emit(Op::Pop); // retained source base used only to enforce Reference key ordering
        Ok(key_slot)
    }

    fn load_assignment_property(&mut self, source: u16, key: u16) {
        self.emit(Op::LoadLocal(source));
        self.emit(Op::LoadLocal(key));
        self.emit(Op::GetElem);
    }

    fn assignment_pattern(&mut self, target: &Expr) -> CResult {
        match target {
            Expr::Paren(inner) => self.assignment_pattern(inner),
            Expr::Array(elements) => self.assignment_array_pattern(elements),
            Expr::Object(properties) => self.assignment_object_pattern(properties),
            _ => Err(Bail),
        }
    }

    fn assignment_array_pattern(&mut self, elements: &[ArrayElem]) -> CResult {
        let iterator = self.fresh_slot("%assignment-iterator%");
        let next = self.fresh_slot("%assignment-next%");
        let done = self.fresh_slot("%assignment-done%");
        self.emit(Op::GetIter);
        self.emit(Op::StoreLocal(next));
        self.emit(Op::StoreLocal(iterator));
        let false_value = self.const_idx(Value::Bool(false));
        self.emit(Op::Const(false_value));
        self.emit(Op::StoreLocal(done));

        let handler = self.emit(Op::PushIterator(0, 0, 0, 0));
        self.try_depth += 1;
        for element in elements {
            match element {
                ArrayElem::Hole => {
                    self.emit(Op::DestructureStepL(iterator, next, done));
                    self.emit(Op::Pop);
                }
                ArrayElem::Item(element) => {
                    let (core, default) = match element {
                        Expr::Assign {
                            op: "=",
                            target,
                            value,
                        } => (&**target, Some(&**value)),
                        _ => (element, None),
                    };
                    if matches!(core, Expr::Array(_) | Expr::Object(_)) {
                        self.emit(Op::DestructureStepL(iterator, next, done));
                        self.assignment_default(core, default)?;
                        self.assignment_pattern(core)?;
                    } else {
                        let reference = self.prepare_assignment_reference(core)?;
                        self.emit(Op::DestructureStepL(iterator, next, done));
                        self.assignment_default(core, default)?;
                        self.put_assignment_reference(reference);
                    }
                }
                ArrayElem::Spread(target) => {
                    if matches!(target, Expr::Array(_) | Expr::Object(_)) {
                        self.emit(Op::DestructureRestL(iterator, next, done));
                        self.assignment_pattern(target)?;
                    } else {
                        let reference = self.prepare_assignment_reference(target)?;
                        self.emit(Op::DestructureRestL(iterator, next, done));
                        self.put_assignment_reference(reference);
                    }
                }
            }
        }
        self.emit(Op::PopHandler);
        self.try_depth -= 1;
        self.emit(Op::IterCloseIfNotDoneL(iterator, done));
        let after_pads = self.emit(Op::Jump(0));

        let throw_pc = self.ops.len() as u32;
        self.emit(Op::IterAbortIfNotDoneL(iterator, done));
        let return_pc = self.ops.len() as u32;
        self.emit(Op::IterCloseIfNotDoneL(iterator, done));
        self.emit(Op::Return);
        let bare_return_pc = self.ops.len() as u32;
        self.emit(Op::IterCloseIfNotDoneL(iterator, done));
        self.emit(Op::ReturnBare);
        let resume_return_pc = self.ops.len() as u32;
        self.emit(Op::IterCloseIfNotDoneL(iterator, done));
        self.emit(Op::ResumeReturn);
        match &mut self.ops[handler] {
            Op::PushIterator(
                throw_target,
                return_target,
                bare_return_target,
                resume_return_target,
            ) => {
                *throw_target = throw_pc;
                *return_target = return_pc;
                *bare_return_target = bare_return_pc;
                *resume_return_target = resume_return_pc;
            }
            _ => unreachable!(),
        }
        self.patch(after_pads);
        Ok(())
    }

    fn assignment_object_pattern(&mut self, properties: &[PropDef]) -> CResult {
        self.emit(Op::DestructureGuard);
        let source = self.fresh_slot("%assignment-object%");
        self.emit(Op::StoreLocal(source));
        let mut excluded = Vec::new();
        for property in properties {
            match property {
                PropDef::KeyValue { key, value } | PropDef::Cover { key, value } => {
                    let key = self.assignment_object_key(source, key)?;
                    excluded.push(key);
                    let (core, default) = match value {
                        Expr::Assign {
                            op: "=",
                            target,
                            value,
                        } => (&**target, Some(&**value)),
                        _ => (value, None),
                    };
                    if matches!(core, Expr::Array(_) | Expr::Object(_)) {
                        self.load_assignment_property(source, key);
                        self.assignment_default(core, default)?;
                        self.assignment_pattern(core)?;
                    } else {
                        let reference = self.prepare_assignment_reference(core)?;
                        self.load_assignment_property(source, key);
                        self.assignment_default(core, default)?;
                        self.put_assignment_reference(reference);
                    }
                }
                PropDef::Proto(value) => {
                    let key = self
                        .assignment_object_key(source, &PropKey::Ident("__proto__".to_string()))?;
                    excluded.push(key);
                    if matches!(value, Expr::Array(_) | Expr::Object(_)) {
                        self.load_assignment_property(source, key);
                        self.assignment_pattern(value)?;
                    } else {
                        let reference = self.prepare_assignment_reference(value)?;
                        self.load_assignment_property(source, key);
                        self.put_assignment_reference(reference);
                    }
                }
                PropDef::Spread(target) => {
                    let reference = self.prepare_assignment_reference(target)?;
                    self.emit(Op::LoadLocal(source));
                    for key in &excluded {
                        self.emit(Op::LoadLocal(*key));
                    }
                    self.emit(Op::ObjectRest(excluded.len() as u16));
                    self.put_assignment_reference(reference);
                }
                PropDef::Method { .. } | PropDef::Getter { .. } | PropDef::Setter { .. } => {
                    return Err(Bail);
                }
            }
        }
        Ok(())
    }

    /// Emit the bookkeeping a `break`/`continue` targeting `self.loops[target]` must run before
    /// its jump: pop every `try`/for-of-body handler region opened since the target's entry (a
    /// stale handler would catch unrelated throws later in the frame), and IteratorClose each
    /// for-of iterator being abandoned — the target's own iterator too for a `break`, but not
    /// for a `continue` (the loop keeps iterating). Multiple levels use abrupt-jump cleanup pads
    /// so close errors cascade through the outer loops' existing throw-mode handlers.
    fn emit_exit_jump(&mut self, target: usize, is_continue: bool) -> Result<usize, Bail> {
        let floor = self.loops[target].entry_try_depth;
        let crosses_finally = self
            .finally_depths
            .iter()
            .any(|finally_depth| *finally_depth > floor && *finally_depth <= self.try_depth);
        // For-of levels whose iterator is abandoned by this jump.
        let closes: Vec<(usize, u16, Option<u16>, u32)> = self
            .loops
            .iter()
            .enumerate()
            .skip(if is_continue { target + 1 } else { target })
            .filter_map(|(k, c)| {
                c.foreach_iter
                    .map(|it| (k, it, c.foreach_async_from_sync, c.body_try_depth))
            })
            .collect();
        if crosses_finally || closes.len() > 1 {
            // ECMA-262 §14.15.3 preserves a break/continue Completion when the finalizer is
            // normal. Handler unwinding therefore owns the jump. Abandoned for-of iterators close
            // inside-out: unwind only to each loop's entry, close with the outer body handlers
            // still active, then resume. Consequently a close throw replaces the jump and is
            // fed through every outer iterator's throw-mode close by its existing catch pad.
            for &(_, iter_s, async_from_sync_s, body_depth) in closes.iter().rev() {
                let to_cleanup = self.emit(Op::AbruptJump(0, body_depth - 1));
                let cleanup_pc = self.ops.len() as u32;
                self.patch_to(to_cleanup, cleanup_pc);
                if let Some(from_sync_s) = async_from_sync_s {
                    self.emit(Op::AsyncIterCloseL(iter_s, from_sync_s, false));
                } else {
                    self.emit(Op::IterCloseL(iter_s));
                }
            }
            return Ok(self.emit(Op::AbruptJump(0, floor)));
        }
        let mut depth_now = self.try_depth;
        if let Some(&(_, iter_s, async_from_sync_s, body_depth)) = closes.last() {
            // Pop the regions inside the for-of body, then its own body handler, then close.
            for _ in body_depth..depth_now {
                self.emit(Op::PopHandler);
            }
            self.emit(Op::PopHandler);
            if let Some(from_sync_s) = async_from_sync_s {
                self.emit(Op::AsyncIterCloseL(iter_s, from_sync_s, false));
            } else {
                self.emit(Op::IterCloseL(iter_s));
            }
            depth_now = body_depth - 1;
        }
        // Remaining regions down to the target's entry (plain `try`s between the loops — and for
        // a continue to a for-of, its own body handler, which the loop head re-pushes).
        for _ in floor..depth_now {
            self.emit(Op::PopHandler);
        }
        Ok(self.emit(Op::Jump(0)))
    }

    fn switch_body(&mut self, discriminant: u16, cases: &[SwitchCase]) -> CResult {
        for case in cases {
            for statement in &case.body {
                match statement {
                    Stmt::VarDecl {
                        kind: kind @ (DeclKind::Let | DeclKind::Const),
                        decls,
                    } => {
                        let is_const = matches!(kind, DeclKind::Const);
                        for (pattern, _) in decls {
                            self.declare_lexical_pattern(pattern, is_const)?;
                        }
                    }
                    Stmt::ClassDecl(class) => {
                        let name = class.name.as_ref().ok_or(Bail)?;
                        self.declare_lexical_pattern(&Pattern::Ident(name.clone()), false)?;
                    }
                    Stmt::VarDecl {
                        kind: DeclKind::Using | DeclKind::AwaitUsing,
                        ..
                    } => return Err(Bail),
                    Stmt::FuncDecl(function) => {
                        let name = function.name.as_ref().ok_or(Bail)?;
                        self.declare_lexical_pattern(&Pattern::Ident(name.clone()), false)?;
                    }
                    _ => {}
                }
            }
        }
        // CaseBlockEvaluation uses one shared lexical environment. Its block functions are
        // instantiated after the discriminant but before the first case test, and declarations in
        // later/default clauses are visible throughout the CaseBlock.
        for case in cases {
            for statement in &case.body {
                if let Stmt::FuncDecl(function) = statement {
                    let name = function.name.as_ref().ok_or(Bail)?;
                    self.emit_closure(function, None);
                    if self.current_lexical_env_has(name) {
                        let name = self.name_idx(name);
                        self.emit(Op::InitLex(name));
                    } else {
                        let Home::Slot(slot, false) = self.home(name).ok_or(Bail)? else {
                            return Err(Bail);
                        };
                        self.emit(Op::StoreLocal(slot));
                    }
                }
            }
        }

        // Phase 1: the test chain. Each match jumps to its (not yet emitted) body.
        let mut body_jumps: Vec<(usize, usize)> = Vec::new();
        for (case_index, case) in cases.iter().enumerate() {
            if let Some(test) = &case.test {
                self.emit(Op::LoadLocal(discriminant));
                self.expr(test)?;
                self.emit(Op::StrictEq);
                let no_match = self.emit(Op::JumpIfFalse(0));
                let body = self.emit(Op::Jump(0));
                body_jumps.push((case_index, body));
                self.patch(no_match);
            }
        }
        let default_jump = self.emit(Op::Jump(0));
        self.loops.push(LoopCtx {
            labels: std::mem::take(&mut self.pending_labels),
            entry_try_depth: self.try_depth,
            is_switch: true,
            ..LoopCtx::default()
        });

        // Phase 2: bodies, contiguous and in source order so normal fall-through is implicit.
        let mut body_starts = vec![0usize; cases.len()];
        let mut result = Ok(());
        'bodies: for (case_index, case) in cases.iter().enumerate() {
            body_starts[case_index] = self.ops.len();
            for statement in &case.body {
                result = self.stmt(statement);
                if result.is_err() {
                    break 'bodies;
                }
            }
        }
        let context = self.loops.pop().expect("switch context missing");
        result?;
        for (case_index, jump) in body_jumps {
            match &mut self.ops[jump] {
                Op::Jump(target) => *target = body_starts[case_index] as u32,
                _ => unreachable!(),
            }
        }
        match cases.iter().position(|case| case.test.is_none()) {
            Some(default_index) => match &mut self.ops[default_jump] {
                Op::Jump(target) => *target = body_starts[default_index] as u32,
                _ => unreachable!(),
            },
            None => self.patch(default_jump),
        }
        for jump in context.breaks {
            self.patch(jump);
        }
        Ok(())
    }

    /// Lower CatchClauseEvaluation (ECMA-262 §14.15.2). A parameterized catch creates every
    /// mutable binding before BindingInitialization; the catch Block then has its own nested
    /// lexical scope. Binding-pattern defaults and iterator closing use the same suspension-aware
    /// machinery as declarations, while an omitted parameter simply discards the thrown value.
    fn catch_clause(&mut self, param: Option<&Pattern>, body: &[Stmt]) -> CResult {
        if let Some(pattern) = param {
            self.push_compile_scope();
            let mut names = std::collections::HashSet::new();
            pat_idents(pattern, &mut names);
            let mut bindings: Vec<_> = names
                .into_iter()
                .filter(|name| self.runtime_lexicals.contains(name))
                .map(|name| (name, false))
                .collect();
            bindings.sort_by(|left, right| left.0.cmp(&right.0));

            let result = if bindings.is_empty() {
                self.catch_clause_body(pattern, body)
            } else {
                // NewDeclarativeEnvironment precedes BindingInitialization, and the environment
                // remains current through evaluation of the nested Block. Keep the catch marker:
                // Annex B.3.4 exempts this one environment when sloppy eval checks whether a var
                // declaration may cross intervening lexical environments.
                let scope = self.push_runtime_catch_scope(bindings);
                self.environment_scope(|compiler| compiler.catch_clause_body(pattern, body), scope)
            };
            self.pop_compile_scope();
            return result;
        }

        self.emit(Op::Pop);
        self.push_compile_scope();
        let result = self.block_body(body);
        self.pop_compile_scope();
        result
    }

    fn catch_clause_body(&mut self, pattern: &Pattern, body: &[Stmt]) -> CResult {
        self.declare_lexical_pattern(pattern, false)?;
        self.destructure_store(pattern, DeclKind::Let)?;

        self.push_compile_scope();
        let result = self.block_body(body);
        self.pop_compile_scope();
        result
    }

    fn block_body(&mut self, body: &[Stmt]) -> CResult {
        let bindings = self.runtime_block_bindings(body);
        if !bindings.is_empty() {
            let scope = self.push_runtime_lexical_scope(bindings);
            return self.environment_scope(|compiler| compiler.block_body_slots(body), scope);
        }
        self.block_body_slots(body)
    }

    fn block_body_slots(&mut self, body: &[Stmt]) -> CResult {
        self.declare_block_lexicals(body)?;
        if body.iter().any(|statement| {
            matches!(
                statement,
                Stmt::VarDecl {
                    kind: DeclKind::Using | DeclKind::AwaitUsing,
                    ..
                }
            )
        }) {
            return self.disposal_scope(|compiler| {
                for statement in body {
                    compiler.stmt(statement)?;
                }
                Ok(())
            });
        }
        for statement in body {
            self.stmt(statement)?;
        }
        Ok(())
    }

    /// The closure-visible subset of BlockDeclarationInstantiation. Keeping uncaptured names out
    /// of this list preserves the slot fast path while the selected bindings receive one fresh
    /// Declarative Environment Record per block evaluation.
    fn runtime_block_bindings(&self, body: &[Stmt]) -> Vec<(String, bool)> {
        let mut bindings = Vec::new();
        let mut add_pattern = |pattern: &Pattern, is_const: bool| {
            let mut names = std::collections::HashSet::new();
            pat_idents(pattern, &mut names);
            let mut names: Vec<_> = names.into_iter().collect();
            names.sort();
            for name in names {
                if self.runtime_lexicals.contains(&name)
                    && !bindings.iter().any(|(existing, _)| existing == &name)
                {
                    bindings.push((name, is_const));
                }
            }
        };
        for statement in body {
            match statement {
                Stmt::VarDecl { kind, decls }
                    if matches!(
                        kind,
                        DeclKind::Let | DeclKind::Const | DeclKind::Using | DeclKind::AwaitUsing
                    ) =>
                {
                    let is_const = !matches!(kind, DeclKind::Let);
                    for (pattern, _) in decls {
                        add_pattern(pattern, is_const);
                    }
                }
                Stmt::ClassDecl(class) => {
                    if let Some(name) = &class.name {
                        add_pattern(&Pattern::Ident(name.clone()), false);
                    }
                }
                Stmt::FuncDecl(function) => {
                    if let Some(name) = &function.name {
                        add_pattern(&Pattern::Ident(name.clone()), false);
                    }
                }
                _ => {}
            }
        }
        bindings
    }

    fn push_runtime_lexical_scope(&mut self, bindings: Vec<(String, bool)>) -> u32 {
        self.push_runtime_scope(bindings, false)
    }

    fn push_runtime_catch_scope(&mut self, bindings: Vec<(String, bool)>) -> u32 {
        self.push_runtime_scope(bindings, true)
    }

    fn push_runtime_scope(&mut self, bindings: Vec<(String, bool)>, catch_param: bool) -> u32 {
        for (name, is_const) in &bindings {
            self.lexical_env_bind(name, *is_const);
        }
        let scope = self.lexical_scopes.len() as u32;
        self.lexical_scopes.push(
            bindings
                .into_iter()
                .map(|(name, is_const)| LexicalBinding {
                    name: Rc::from(name),
                    is_const,
                })
                .collect(),
        );
        self.emit(if catch_param {
            Op::PushCatchLex(scope)
        } else {
            Op::PushLex(scope)
        });
        scope
    }

    /// Restore one compiler-created lexical environment on every Completion. The handler is
    /// deliberately outside resource/iterator handlers compiled by `compile_body`, matching the
    /// spec's restoration of LexicalEnvironment after inner cleanup has completed.
    fn environment_scope(
        &mut self,
        compile_body: impl FnOnce(&mut Compiler) -> CResult,
        _scope: u32,
    ) -> CResult {
        let push = self.emit(Op::PushFinally(0, 0, 0, 0, 0));
        self.try_depth += 1;
        self.finally_depths.push(self.try_depth);

        compile_body(self)?;

        self.emit(Op::PopHandler);
        self.try_depth -= 1;
        self.finally_depths.pop();
        self.emit(Op::PopEnv);
        let normal_exit = self.emit(Op::Jump(0));

        let throw_pc = self.ops.len() as u32;
        self.emit(Op::PopEnv);
        self.emit(Op::Throw);
        let return_pc = self.ops.len() as u32;
        self.emit(Op::PopEnv);
        self.emit(Op::Return);
        let bare_return_pc = self.ops.len() as u32;
        self.emit(Op::PopEnv);
        self.emit(Op::ReturnBare);
        let resume_return_pc = self.ops.len() as u32;
        self.emit(Op::PopEnv);
        self.emit(Op::ResumeReturn);
        let jump_pc = self.ops.len() as u32;
        self.emit(Op::PopEnv);
        self.emit(Op::ResumeJump);

        match &mut self.ops[push] {
            Op::PushFinally(
                throw_target,
                return_target,
                bare_return_target,
                resume_return_target,
                jump_target,
            ) => {
                *throw_target = throw_pc;
                *return_target = return_pc;
                *bare_return_target = bare_return_pc;
                *resume_return_target = resume_return_pc;
                *jump_target = jump_pc;
            }
            _ => unreachable!("lexical environment handler changed kind"),
        }
        self.patch(normal_exit);
        Ok(())
    }

    /// Compile WithStatement Evaluation with its Object Environment Record stored directly in
    /// the heap continuation. The `PushFinally` pads restore the previous environment for every
    /// ECMAScript completion, including a throw/rejection or generator return injected while the
    /// body is suspended.
    fn with_scope(&mut self, object: &Expr, body: &Stmt) -> CResult {
        self.expr(object)?;
        self.emit(Op::PushWith);
        let push = self.emit(Op::PushFinally(0, 0, 0, 0, 0));
        self.try_depth += 1;
        self.finally_depths.push(self.try_depth);

        self.with_scope_floors.push(self.scopes.len());
        let result = self.stmt(body);
        self.with_scope_floors.pop();
        result?;

        self.emit(Op::PopHandler);
        self.try_depth -= 1;
        self.finally_depths.pop();
        self.emit(Op::PopEnv);
        let normal_exit = self.emit(Op::Jump(0));

        let throw_pc = self.ops.len() as u32;
        self.emit(Op::PopEnv);
        self.emit(Op::Throw);
        let return_pc = self.ops.len() as u32;
        self.emit(Op::PopEnv);
        self.emit(Op::Return);
        let bare_return_pc = self.ops.len() as u32;
        self.emit(Op::PopEnv);
        self.emit(Op::ReturnBare);
        let resume_return_pc = self.ops.len() as u32;
        self.emit(Op::PopEnv);
        self.emit(Op::ResumeReturn);
        let jump_pc = self.ops.len() as u32;
        self.emit(Op::PopEnv);
        self.emit(Op::ResumeJump);

        match &mut self.ops[push] {
            Op::PushFinally(
                throw_target,
                return_target,
                bare_return_target,
                resume_return_target,
                jump_target,
            ) => {
                *throw_target = throw_pc;
                *return_target = return_pc;
                *bare_return_target = bare_return_pc;
                *resume_return_target = resume_return_pc;
                *jump_target = jump_pc;
            }
            _ => unreachable!("with scope handler changed kind"),
        }
        self.patch(normal_exit);
        Ok(())
    }

    /// Compile one statement-list disposal boundary as a completion-aware `finally` region.
    /// Each pad consumes the exact Completion that caused scope exit, runs DisposeResources, and
    /// reissues the surviving completion so outer finalizers/iterators observe the right order.
    fn disposal_scope(&mut self, compile_body: impl FnOnce(&mut Compiler) -> CResult) -> CResult {
        self.emit(Op::PushDisposeFrame);
        let push = self.emit(Op::PushFinally(0, 0, 0, 0, 0));
        self.try_depth += 1;
        self.finally_depths.push(self.try_depth);

        compile_body(self)?;

        self.emit(Op::PopHandler);
        self.try_depth -= 1;
        self.finally_depths.pop();
        self.emit(Op::DisposeNormal);
        let normal_exit = self.emit(Op::Jump(0));

        let throw_pc = self.ops.len() as u32;
        self.emit(Op::DisposeThrow);
        let return_pc = self.ops.len() as u32;
        self.emit(Op::DisposeReturn);
        let bare_return_pc = self.ops.len() as u32;
        self.emit(Op::DisposeBareReturn);
        let resume_return_pc = self.ops.len() as u32;
        self.emit(Op::DisposeResumeReturn);
        let jump_pc = self.ops.len() as u32;
        self.emit(Op::DisposeJump);

        match &mut self.ops[push] {
            Op::PushFinally(
                throw_target,
                return_target,
                bare_return_target,
                resume_return_target,
                jump_target,
            ) => {
                *throw_target = throw_pc;
                *return_target = return_pc;
                *bare_return_target = bare_return_pc;
                *resume_return_target = resume_return_pc;
                *jump_target = jump_pc;
            }
            _ => unreachable!("disposal scope handler changed kind"),
        }
        self.patch(normal_exit);
        Ok(())
    }

    fn for_loop(
        &mut self,
        init: Option<&ForInit>,
        test: Option<&Expr>,
        update: Option<&Expr>,
        body: &Stmt,
    ) -> CResult {
        // Claim any labels from an enclosing `Stmt::Labeled` before the head runs, so they land on
        // this loop's context (the head itself introduces no labelled break/continue targets).
        let labels = std::mem::take(&mut self.pending_labels);
        let mut runtime_bindings = Vec::new();
        if let Some(ForInit::VarDecl {
            kind: kind @ (DeclKind::Let | DeclKind::Const),
            decls,
        }) = init
        {
            for (pattern, _) in decls {
                let mut names = std::collections::HashSet::new();
                pat_idents(pattern, &mut names);
                let mut names: Vec<_> = names.into_iter().collect();
                names.sort();
                runtime_bindings.extend(
                    names
                        .into_iter()
                        .filter(|name| self.runtime_lexicals.contains(name))
                        .map(|name| (name, matches!(kind, DeclKind::Const))),
                );
            }
        }
        if !runtime_bindings.is_empty() {
            let scope = self.push_runtime_lexical_scope(runtime_bindings);
            let per_iteration = matches!(
                init,
                Some(ForInit::VarDecl {
                    kind: DeclKind::Let,
                    ..
                })
            )
            .then_some(scope);
            return self.environment_scope(
                |compiler| compiler.for_loop_core(labels, init, test, update, body, per_iteration),
                scope,
            );
        }
        if let Some(ForInit::VarDecl {
            kind: kind @ (DeclKind::Using | DeclKind::AwaitUsing),
            decls,
        }) = init
        {
            for (pattern, _) in decls {
                self.declare_lexical_pattern(pattern, true)?;
            }
            return self.disposal_scope(|compiler| {
                for (pattern, initializer) in decls {
                    let Pattern::Ident(name) = pattern else {
                        return Err(Bail);
                    };
                    let initializer = initializer.as_ref().ok_or(Bail)?;
                    compiler.named_expr(initializer, name)?;
                    compiler.emit(Op::AddDisposable(matches!(kind, DeclKind::AwaitUsing)));
                    match compiler.home(name).ok_or(Bail)? {
                        Home::Slot(slot, _) => compiler.emit(Op::StoreLocal(slot)),
                        Home::Env(_) => {
                            let name = compiler.name_idx(name);
                            compiler.emit(Op::StoreCapInit(name))
                        }
                    };
                }
                compiler.for_loop_core(labels, None, test, update, body, None)
            });
        }
        self.for_loop_core(labels, init, test, update, body, None)
    }

    fn for_loop_core(
        &mut self,
        labels: Vec<String>,
        init: Option<&ForInit>,
        test: Option<&Expr>,
        update: Option<&Expr>,
        body: &Stmt,
        per_iteration_scope: Option<u32>,
    ) -> CResult {
        match init {
            Some(ForInit::VarDecl { kind, decls }) => {
                debug_assert!(!matches!(kind, DeclKind::Using | DeclKind::AwaitUsing));
                if matches!(kind, DeclKind::Let | DeclKind::Const) {
                    for (pat, _) in decls {
                        // ForLoopEvaluation creates every loopEnv binding before evaluating the
                        // LexicalDeclaration. Captured leaves are already present in the runtime
                        // record; uncaptured leaves retain TDZ slots and need no environment copy.
                        self.declare_lexical_pattern(pat, matches!(kind, DeclKind::Const))?;
                    }
                }
                for (pat, initv) in decls {
                    let Pattern::Ident(name) = pat else {
                        let Some(initializer) = initv else {
                            return Err(Bail);
                        };
                        self.expr(initializer)?;
                        self.destructure_store(pat, *kind)?;
                        continue;
                    };
                    let dynamic_lexical = self.current_lexical_env_has(name);
                    let home = if dynamic_lexical {
                        None
                    } else {
                        Some(self.home(name).ok_or(Bail)?)
                    };
                    match initv {
                        Some(initializer) => self.named_expr(initializer, name)?,
                        None if matches!(kind, DeclKind::Var) => continue,
                        None => {
                            self.emit(Op::Undef);
                        }
                    }
                    if dynamic_lexical {
                        let name = self.name_idx(name);
                        self.emit(if matches!(kind, DeclKind::Var) {
                            Op::StoreName(name)
                        } else {
                            Op::InitLex(name)
                        });
                        continue;
                    }
                    match home.expect("non-dynamic for declaration has a static home") {
                        Home::Slot(slot, _) => {
                            self.emit(Op::StoreLocal(slot));
                        }
                        Home::Env(_) => {
                            let name = self.name_idx(name);
                            self.emit(if matches!(kind, DeclKind::Var) {
                                Op::StoreCap(name)
                            } else {
                                Op::StoreCapInit(name)
                            });
                        }
                    }
                }
            }
            Some(ForInit::Expr(e)) => {
                self.expr_stmt(e)?;
            }
            None => {}
        }
        if let Some(scope) = per_iteration_scope {
            // ForBodyEvaluation calls CreatePerIterationEnvironment once before the first test.
            self.emit(Op::CloneLex(scope));
        }
        let start = self.ops.len();
        let jf = match test {
            Some(t) => {
                self.expr(t)?;
                Some(self.emit(Op::JumpIfFalse(0)))
            }
            None => None,
        };
        self.loops.push(LoopCtx {
            labels,
            entry_try_depth: self.try_depth,
            ..LoopCtx::default()
        });
        let r = self.stmt(body);
        let ctx = self.loops.pop().unwrap();
        r?;
        let cont = self.ops.len();
        for c in ctx.continues {
            self.patch_to(c, cont as u32);
        }
        if let Some(scope) = per_iteration_scope {
            // The next iteration's record is created before evaluating the increment expression;
            // closures in the body therefore retain the record that the body actually observed.
            self.emit(Op::CloneLex(scope));
        }
        if let Some(u) = update {
            self.expr_stmt(u)?;
        }
        self.emit(Op::Jump(start as u32));
        if let Some(jf) = jf {
            self.patch(jf);
        }
        for b in ctx.breaks {
            self.patch(b);
        }
        Ok(())
    }

    /// Compile an expression whose value is discarded (an expression statement, or a `for`
    /// header's init / update). Assignments and `++`/`--` to a local drop their producing `Dup`
    /// (and the trailing `Pop`); everything else falls back to `expr` + `Pop`. Semantically
    /// identical to `self.expr(e)?; self.emit(Op::Pop)` — the only difference is the unobservable
    /// result value.
    fn expr_stmt(&mut self, e: &Expr) -> CResult {
        match e {
            Expr::Paren(inner) => return self.expr_stmt(inner),
            // A comma expression as a statement: every operand is evaluated for effect only.
            Expr::Seq(exprs) => {
                for ex in exprs {
                    self.expr_stmt(ex)?;
                }
                return Ok(());
            }
            Expr::Update { op, arg, .. } => {
                let kind = match *op {
                    "++" => UpdKind::IncDiscard,
                    "--" => UpdKind::DecDiscard,
                    _ => return Err(Bail),
                };
                return self.update_target(arg, kind);
            }
            Expr::Assign { op, target, value } => {
                return self.assign_discard(op, target, value);
            }
            _ => {}
        }
        self.expr(e)?;
        self.emit(Op::Pop);
        Ok(())
    }

    /// Compile a discarded assignment: the fast `Dup`-free lowering when the target is a plain
    /// local / free name / `obj.x` / `obj[k]`, otherwise the generic value-producing `assign`
    /// followed by `Pop` (identical to `self.expr(assign)?; Pop`).
    fn assign_discard(&mut self, op: &str, target: &Expr, value: &Expr) -> CResult {
        if self.try_assign_discard(op, target, value)? {
            return Ok(());
        }
        self.assign(op, target, value)?;
        self.emit(Op::Pop);
        Ok(())
    }

    /// Fast lowering for a discarded assignment (no `Dup`, no trailing `Pop`). Returns `Ok(true)`
    /// when it emitted the assignment, `Ok(false)` to defer to the generic `assign` + `Pop` path
    /// (which handles — or itself bails on — the forms not covered here). Any `Bail` from a
    /// compiled sub-expression propagates: the generic path would bail identically.
    fn try_assign_discard(&mut self, op: &str, target: &Expr, value: &Expr) -> Result<bool, Bail> {
        // Logical-assignment short-circuits; leave it to the generic path (which bails).
        if matches!(op, "&&=" | "||=" | "??=") {
            return Ok(false);
        }
        match target {
            Expr::Ident(name) => match self.home(name) {
                Some(Home::Slot(slot, is_const)) => {
                    if is_const {
                        self.immutable_assignment(Home::Slot(slot, true), name, op, value)?;
                        return Ok(true);
                    }
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        self.emit(Op::LoadLocal(slot));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::StoreLocal(slot));
                    Ok(true)
                }
                Some(Home::Env(is_const)) => {
                    if is_const {
                        self.immutable_assignment(Home::Env(true), name, op, value)?;
                        return Ok(true);
                    }
                    let n = self.name_idx(name);
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        self.emit(Op::LoadCap(n));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::StoreCap(n));
                    Ok(true)
                }
                None => {
                    let i = self.name_idx(name);
                    let reference = if self.name_reference_can_change(name) {
                        let reference = self.fresh_reference()?;
                        self.emit(Op::ResolveNameRef(i, reference));
                        Some(reference)
                    } else {
                        None
                    };
                    if op == "=" {
                        // StoreName already consumes the value without re-pushing it.
                        self.named_expr(value, name)?;
                    } else {
                        if let Some(reference) = reference {
                            self.emit(Op::LoadRef(reference));
                        } else {
                            let c = self.new_name_cache(i);
                            self.emit(Op::LoadName(i, c));
                        }
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    if let Some(reference) = reference {
                        self.emit(Op::StoreRef(reference));
                    } else {
                        self.emit_store_name(i);
                    }
                    Ok(true)
                }
            },
            // Receiver-direct statement stores: `this.x = v` (always safe — `this` can't be
            // reassigned) and `slotlocal.x = v` when the RHS provably can't reassign the local
            // (the receiver is read at set time, after the RHS — evaluation order must agree).
            Expr::Member {
                obj: mobj,
                prop,
                optional: false,
            } if op == "="
                && !prop.starts_with('#')
                && match &**mobj {
                    Expr::This => !self.lexical_this && !self.derived_constructor,
                    Expr::Ident(name) => {
                        matches!(self.home(name), Some(Home::Slot(..))) && no_assign_to(value, name)
                    }
                    _ => false,
                } =>
            {
                self.expr(value)?;
                let i = self.name_idx(prop);
                let c = self.new_cache(i);
                match &**mobj {
                    Expr::This => {
                        if self.inline_depth > 0 {
                            match self.inline_this {
                                Some(slot) => {
                                    self.emit(Op::SetPropLocalDrop(slot, i, c));
                                }
                                None => return Err(Bail), // splice without a this binding
                            }
                        } else {
                            self.uses_this = true;
                            self.emit(Op::SetPropThisDrop(i, c));
                        }
                    }
                    Expr::Ident(name) => {
                        let Some(Home::Slot(slot, _)) = self.home(name) else {
                            unreachable!()
                        };
                        self.emit(Op::SetPropLocalDrop(slot, i, c));
                    }
                    _ => unreachable!(),
                }
                Ok(true)
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.expr(obj)?;
                let i = self.name_idx(prop);
                if op == "+=" {
                    // Fused append: same evaluation order (read before RHS), and the op itself
                    // falls back to the generic Add + store when anything isn't plain strings.
                    self.emit(Op::Dup);
                    let cg = self.new_cache(i);
                    self.emit(Op::GetProp(i, cg));
                    self.expr(value)?;
                    let c = self.new_cache(i);
                    self.emit(Op::AppendProp(i, c));
                    return Ok(true);
                }
                if op == "=" {
                    self.expr(value)?;
                } else {
                    self.emit(Op::Dup);
                    let cg = self.new_cache(i);
                    self.emit(Op::GetProp(i, cg));
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                let c = self.new_cache(i);
                self.emit(Op::SetPropDrop(i, c));
                Ok(true)
            }

            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                if let Some(slot) = self.fused_elem_slot(obj, &[index.as_ref(), value]) {
                    self.expr(index)?;
                    if op == "=" {
                        self.expr(value)?;
                    } else {
                        // Compound: coerce a side-effecting key once (Num keys pass raw), then
                        // read-modify-write against the slot base — one Dup, no receiver churn.
                        self.emit(Op::ToPropKeyLocal(slot));
                        self.emit(Op::Dup);
                        self.emit(Op::GetElemLocal(slot));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::SetElemLocalDrop(slot));
                    return Ok(true);
                }
                self.expr(obj)?;
                self.expr(index)?;
                if op == "=" {
                    self.expr(value)?;
                } else {
                    self.emit(Op::ToPropKey);
                    self.emit(Op::Dup2);
                    self.emit(Op::GetElem);
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                self.emit(Op::SetElemDrop);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Emit a closure over the current environment; `name` applies NamedEvaluation to an
    /// anonymous function expression (`var f = function(){}` → `f.name === "f"`).
    fn emit_closure(&mut self, f: &Rc<Function>, name: Option<&str>) {
        let fidx = self.funcs.len() as u32;
        self.funcs.push(f.clone());
        let name_idx = match name {
            Some(n) if f.name.is_none() && !f.is_method => self.name_idx(n),
            _ => u32::MAX,
        };
        self.emit(Op::MakeClosure(fidx, name_idx));
    }

    /// Compile a value expression in a naming position (declaration/assignment to `name`).
    fn named_expr(&mut self, e: &Expr, name: &str) -> CResult {
        if let Expr::Func(f) = e {
            self.emit_closure(f, Some(name));
            return Ok(());
        }
        if let Expr::Class(class) = e {
            if class.name.is_none() {
                if crate::eval::expr_contains(e, |expr| {
                    matches!(expr, Expr::Yield { .. } | Expr::Await(_))
                }) {
                    return self.staged_class(class, Some(name));
                }
                self.retain_named_eval_expr(e, Some(name));
                return Ok(());
            }
        }
        self.expr(e)
    }

    /// Lower the suspendable prefix of ClassDefinitionEvaluation while retaining its lexical and
    /// private environments in the VM frame. The existing evaluator finishes the atomic suffix
    /// after heritage and every computed key have completed exactly once.
    fn staged_class(&mut self, class: &Rc<Class>, inferred_name: Option<&str>) -> CResult {
        if !self.is_coroutine
            || class.decorators.len() > u16::MAX as usize
            || class
                .members
                .iter()
                .any(|member| member.decorators.len() > u16::MAX as usize)
        {
            return Err(Bail);
        }
        if class.members.len() > u16::MAX as usize {
            return Err(Bail);
        }
        let plan = self.class_plans.len() as u32;
        self.class_plans.push(ClassPlan {
            class: class.clone(),
            inferred_name: inferred_name.map(str::to_string),
        });

        // TC39 decorators proposal, "Evaluating decorators": class decorator expressions run in
        // the surrounding environment before ClassDefinitionEvaluation opens the class-name TDZ.
        for decorator in &class.decorators {
            self.decorator_expression(decorator)?;
        }
        self.emit(Op::ClassStart(plan, class.decorators.len() as u16));

        let cleanup = self.emit(Op::PushFinally(0, 0, 0, 0, 0));
        self.try_depth += 1;
        self.finally_depths.push(self.try_depth);

        // ClassDefinitionEvaluation is strict code and resolves the class's self-name through the
        // newly created (TDZ) class environment rather than any outer declaration slot.
        let saved_strict = std::mem::replace(&mut self.strict, true);
        self.scopes.push(Vec::new());
        let mut class_names = std::collections::HashMap::new();
        if let Some(name) = &class.name {
            class_names.insert(name.clone(), true);
        }
        self.lexical_env_names.push(class_names);
        let compile_result = (|| {
            if let Some(superclass) = &class.superclass {
                self.expr(superclass)?;
                self.emit(Op::ClassHeritage(plan, true));
            } else {
                self.emit(Op::ClassHeritage(plan, false));
            }
            for (index, member) in class.members.iter().enumerate() {
                for decorator in &member.decorators {
                    self.decorator_expression(decorator)?;
                    self.emit(Op::ClassDecorator(plan, index as u16));
                }
                if member.kind == MemberKind::Constructor {
                    continue;
                }
                if let PropKey::Computed(key) = &member.key {
                    self.expr(key)?;
                    self.emit(Op::ClassKey(plan, index as u16));
                }
            }
            Ok(())
        })();
        self.lexical_env_names.pop();
        self.scopes.pop();
        self.strict = saved_strict;
        compile_result?;

        self.emit(Op::ClassFinish(plan));
        self.emit(Op::PopHandler);
        self.try_depth -= 1;
        self.finally_depths.pop();
        let normal_exit = self.emit(Op::Jump(0));

        let throw_pc = self.ops.len() as u32;
        self.emit(Op::ClassAbort(plan));
        self.emit(Op::Throw);
        let return_pc = self.ops.len() as u32;
        self.emit(Op::ClassAbort(plan));
        self.emit(Op::Return);
        let bare_return_pc = self.ops.len() as u32;
        self.emit(Op::ClassAbort(plan));
        self.emit(Op::ReturnBare);
        let resume_return_pc = self.ops.len() as u32;
        self.emit(Op::ClassAbort(plan));
        self.emit(Op::ResumeReturn);
        let jump_pc = self.ops.len() as u32;
        self.emit(Op::ClassAbort(plan));
        self.emit(Op::ResumeJump);
        match &mut self.ops[cleanup] {
            Op::PushFinally(
                throw_target,
                return_target,
                bare_return_target,
                resume_return_target,
                jump_target,
            ) => {
                *throw_target = throw_pc;
                *return_target = return_pc;
                *bare_return_target = bare_return_pc;
                *resume_return_target = resume_return_pc;
                *jump_target = jump_pc;
            }
            _ => unreachable!("class environment handler changed kind"),
        }
        self.patch(normal_exit);
        Ok(())
    }

    /// Evaluate a proposal decorator to its retained (receiver, callback) pair. Ordinary
    /// expression evaluation performs GetValue and loses the Reference base; `GetMethod` keeps it
    /// so `@holder.decorator` has the proposal's natural `this` when application occurs later.
    fn decorator_expression(&mut self, expression: &Expr) -> CResult {
        match expression {
            Expr::Paren(inner) => self.decorator_expression(inner),
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                self.expr(obj)?;
                let name = self.name_idx(prop);
                if prop.starts_with('#') {
                    self.emit(Op::GetPrivateMethod(name));
                } else {
                    let cache = self.new_cache(name);
                    self.emit(Op::GetMethod(name, cache));
                }
                Ok(())
            }
            _ => {
                self.emit(Op::Undef);
                self.expr(expression)
            }
        }
    }

    /// Evaluate an ObjectLiteral property name while the fresh object remains immediately below
    /// it on the operand stack. `ToPropKey` performs the observable coercion now (before the
    /// property's value or method definition), as required by PropertyDefinitionEvaluation.
    /// Numeric and string keys may remain in their cheap raw representation because their later
    /// re-coercion is side-effect-free; object and symbol keys are normalized immediately.
    fn object_literal_key(&mut self, key: &PropKey) -> CResult {
        match key {
            PropKey::Ident(key) => {
                let value = self.const_idx(Value::from_string(key.clone()));
                self.emit(Op::Const(value));
            }
            PropKey::Str(key) => {
                let value = self.const_idx(Value::Str(key.clone().into()));
                self.emit(Op::Const(value));
            }
            PropKey::Num(key) => {
                let value = self.const_idx(Value::Num(*key));
                self.emit(Op::Const(value));
            }
            PropKey::Computed(key) => self.expr(key)?,
        }
        self.emit(Op::ToPropKey);
        Ok(())
    }

    fn super_named_reference(&mut self, name: &str) {
        self.emit_super_this();
        let key = self.const_idx(Value::from_string(name.to_string()));
        self.emit(Op::Const(key));
        self.emit(Op::SuperBase);
    }

    fn super_computed_reference(&mut self, key: &Expr) -> CResult {
        self.emit_super_this();
        self.expr(key)?;
        self.emit(Op::SuperBase);
        Ok(())
    }

    /// A Super Reference's `[[ThisValue]]` is the current function's actual this binding. Marking
    /// that dependency is as important as emitting the read: lean compiled calls otherwise omit
    /// OrdinaryCallBindThis and `SuperThis` could resolve an enclosing/global `this` instead.
    fn emit_super_this(&mut self) {
        self.uses_this = true;
        self.emit(if self.lexical_this || self.derived_constructor {
            Op::LoadLexicalThis
        } else {
            Op::SuperThis
        });
    }

    fn expr(&mut self, e: &Expr) -> CResult {
        match e {
            Expr::Func(f) => {
                self.emit_closure(f, None);
                Ok(())
            }
            Expr::Class(class)
                if crate::eval::expr_contains(e, |expression| {
                    matches!(expression, Expr::Yield { .. } | Expr::Await(_))
                }) =>
            {
                self.staged_class(class, None)
            }
            Expr::Num(n) => {
                let i = self.const_idx(Value::Num(*n));
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Str(s) => {
                let i = self.const_idx(Value::Str(s.clone().into()));
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Bool(b) => {
                let i = self.const_idx(Value::Bool(*b));
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Null => {
                let i = self.const_idx(Value::Null);
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Undefined => {
                self.emit(Op::Undef);
                Ok(())
            }
            Expr::BigInt(n) => {
                let i = self.const_idx(Value::BigInt(n.clone()));
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Ident(name) => {
                match self.home(name) {
                    Some(Home::Slot(slot, _)) => {
                        self.emit(Op::LoadLocal(slot));
                    }
                    Some(Home::Env(_)) => {
                        let i = self.name_idx(name);
                        self.emit(Op::LoadCap(i));
                    }
                    None => {
                        let i = self.name_idx(name);
                        let c = self.new_name_cache(i);
                        self.emit(Op::LoadName(i, c));
                    }
                };
                Ok(())
            }
            Expr::This => {
                // A spliced callee's `this` is the receiver, parked in a caller slot.
                if let Some(slot) = self.inline_this {
                    if self.inline_depth > 0 {
                        self.emit(Op::LoadLocal(slot));
                        return Ok(());
                    }
                }
                self.uses_this = true;
                self.emit(if self.lexical_this || self.derived_constructor {
                    Op::LoadLexicalThis
                } else {
                    Op::LoadThis
                });
                Ok(())
            }
            Expr::Paren(inner) => self.expr(inner),
            Expr::Seq(exprs) => {
                for (k, ex) in exprs.iter().enumerate() {
                    self.expr(ex)?;
                    if k + 1 < exprs.len() {
                        self.emit(Op::Pop);
                    }
                }
                Ok(())
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                // Receiver-direct forms: `this.x` and `slotlocal.x` skip the operand-stack
                // round trip (push + refcount bump + drop) entirely.
                match &**obj {
                    // Inside a splice `this` is the receiver slot; otherwise the frame binding.
                    Expr::This if self.inline_depth > 0 => {
                        if let Some(slot) = self.inline_this {
                            let i = self.name_idx(prop);
                            let c = self.new_cache(i);
                            self.emit(Op::GetPropLocal(slot, i, c));
                            return Ok(());
                        }
                    }
                    Expr::This if !self.lexical_this && !self.derived_constructor => {
                        self.uses_this = true;
                        let i = self.name_idx(prop);
                        let c = self.new_cache(i);
                        self.emit(Op::GetPropThis(i, c));
                        return Ok(());
                    }
                    Expr::Ident(name) => {
                        if let Some(Home::Slot(slot, _)) = self.home(name) {
                            let i = self.name_idx(prop);
                            let c = self.new_cache(i);
                            self.emit(Op::GetPropLocal(slot, i, c));
                            return Ok(());
                        }
                    }
                    _ => {}
                }
                self.expr(obj)?;
                let i = self.name_idx(prop);
                let c = self.new_cache(i);
                self.emit(Op::GetProp(i, c));
                Ok(())
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if matches!(**obj, Expr::Super) => {
                self.super_named_reference(prop);
                self.emit(Op::SuperGet);
                Ok(())
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if prop.starts_with('#') => {
                self.expr(obj)?;
                let name = self.name_idx(prop);
                self.emit(Op::GetPrivate(name));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                if let Some(slot) = self.fused_elem_slot(obj, &[index.as_ref()]) {
                    self.expr(index)?;
                    self.emit(Op::GetElemLocal(slot));
                } else {
                    self.expr(obj)?;
                    self.expr(index)?;
                    self.emit(Op::GetElem);
                }
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if matches!(**obj, Expr::Super) => {
                self.super_computed_reference(index)?;
                self.emit(Op::SuperGet);
                Ok(())
            }
            Expr::Binary { op, left, right } => {
                self.expr(left)?;
                self.expr(right)?;
                let bop = match *op {
                    "+" => Op::Add,
                    "-" => Op::Sub,
                    "*" => Op::Mul,
                    "/" => Op::Div,
                    "%" => Op::Mod,
                    "&" => Op::BitAnd,
                    "|" => Op::BitOr,
                    "^" => Op::BitXor,
                    "<<" => Op::Shl,
                    ">>" => Op::Shr,
                    ">>>" => Op::UShr,
                    "<" => Op::Lt,
                    ">" => Op::Gt,
                    "<=" => Op::Le,
                    ">=" => Op::Ge,
                    "==" => Op::EqEq,
                    "!=" => Op::NotEq,
                    "===" => Op::StrictEq,
                    "!==" => Op::StrictNotEq,
                    "instanceof" => Op::InstanceOf(self.new_single_cache()),
                    other => {
                        let i = self.name_idx(other);
                        Op::GenBin(i)
                    }
                };
                self.emit(bop);
                Ok(())
            }
            Expr::Logical { op, left, right } => {
                self.expr(left)?;
                let j = match *op {
                    "&&" => self.emit(Op::JumpIfFalsePeek(0)),
                    "||" => self.emit(Op::JumpIfTruePeek(0)),
                    "??" => self.emit(Op::JumpIfNotNullishPeek(0)),
                    _ => return Err(Bail),
                };
                self.emit(Op::Pop);
                self.expr(right)?;
                self.patch(j);
                Ok(())
            }
            Expr::Cond { test, cons, alt } => {
                self.expr(test)?;
                let jf = self.emit(Op::JumpIfFalse(0));
                self.expr(cons)?;
                let jend = self.emit(Op::Jump(0));
                self.patch(jf);
                self.expr(alt)?;
                self.patch(jend);
                Ok(())
            }
            Expr::Unary { op, arg } => {
                match *op {
                    "-" => {
                        self.expr(arg)?;
                        self.emit(Op::Neg);
                    }
                    "+" => {
                        self.expr(arg)?;
                        self.emit(Op::Plus);
                    }
                    "!" => {
                        self.expr(arg)?;
                        self.emit(Op::Not);
                    }
                    "~" => {
                        self.expr(arg)?;
                        self.emit(Op::BitNot);
                    }
                    "void" => {
                        self.expr(arg)?;
                        self.emit(Op::Void);
                    }
                    "typeof" => {
                        if let Expr::Ident(n) = &**arg {
                            if self.home(n).is_none() {
                                let name = self.name_idx(n);
                                self.emit(Op::TypeofName(name));
                                return Ok(());
                            }
                        }
                        self.expr(arg)?;
                        self.emit(Op::Typeof);
                    }
                    "delete" => return self.delete_expr(arg),
                    _ => return Err(Bail),
                }
                Ok(())
            }
            Expr::Await(arg) => {
                self.expr(arg)?;
                self.emit(Op::Await);
                Ok(())
            }
            Expr::Yield { delegate, arg } => {
                if let Some(value) = arg {
                    self.expr(value)?;
                } else {
                    self.emit(Op::Undef);
                }
                self.emit(if *delegate { Op::YieldStar } else { Op::Yield });
                Ok(())
            }
            Expr::Update { op, prefix, arg } => {
                if !self.with_scope_floors.is_empty()
                    && matches!(&**arg, Expr::Ident(name) if self.home(name).is_none())
                {
                    // UpdateExpression retains one Environment Reference across GetValue,
                    // ToNumeric (which may run user code), and PutValue. Re-resolving a name
                    // through a mutable with object after ToNumeric would be observable.
                    self.retain_eval_expr(e);
                    return Ok(());
                }
                let kind = match (*op, *prefix) {
                    ("++", true) => UpdKind::PreInc,
                    ("--", true) => UpdKind::PreDec,
                    ("++", false) => UpdKind::PostInc,
                    ("--", false) => UpdKind::PostDec,
                    _ => return Err(Bail),
                };
                self.update_target(arg, kind)
            }
            Expr::Assign { op, target, value } => self.assign(op, target, value),
            Expr::ToStr(inner) => {
                self.expr(inner)?;
                self.emit(Op::ToStr);
                Ok(())
            }
            Expr::ImportMeta => {
                self.emit(Op::ImportMeta);
                Ok(())
            }
            Expr::NewTarget => {
                self.emit(Op::NewTarget);
                Ok(())
            }
            Expr::ImportCall {
                spec,
                phase,
                options,
            } => {
                self.expr(spec)?;
                if let Some(options) = options {
                    self.expr(options)?;
                }
                self.emit(Op::DynamicImport(*phase, options.is_some()));
                Ok(())
            }
            Expr::PrivateIn { name, obj } => {
                self.expr(obj)?;
                let name = self.name_idx(name);
                self.emit(Op::PrivateIn(name));
                Ok(())
            }
            Expr::TaggedTemplate {
                tag,
                site: site_id,
                quasis,
                subs,
            } => {
                // Evaluation produces a Reference first, so method/with receivers are retained;
                // IsCallable is checked before GetTemplateObject and every substitution.
                match &**tag {
                    Expr::Member {
                        obj,
                        prop,
                        optional: false,
                    } if matches!(**obj, Expr::Super) => {
                        self.super_named_reference(prop);
                        self.emit(Op::SuperGetMethod);
                    }
                    Expr::Index {
                        obj,
                        index,
                        optional: false,
                    } if matches!(**obj, Expr::Super) => {
                        self.super_computed_reference(index)?;
                        self.emit(Op::SuperGetMethod);
                    }
                    Expr::Member {
                        obj,
                        prop,
                        optional: false,
                    } if prop.starts_with('#') => {
                        self.expr(obj)?;
                        let name = self.name_idx(prop);
                        self.emit(Op::GetPrivateMethod(name));
                    }
                    Expr::Member {
                        obj,
                        prop,
                        optional: false,
                    } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                        self.expr(obj)?;
                        let name = self.name_idx(prop);
                        let cache = self.new_cache(name);
                        self.emit(Op::GetMethod(name, cache));
                    }
                    Expr::Index {
                        obj,
                        index,
                        optional: false,
                    } if !matches!(**obj, Expr::Super) => {
                        self.expr(obj)?;
                        self.expr(index)?;
                        self.emit(Op::GetMethodElem);
                    }
                    Expr::Ident(name) if self.home(name).is_none() => {
                        let name = self.name_idx(name);
                        let cache = self.new_name_cache(name);
                        self.emit(Op::LoadNameForCall(name, cache));
                    }
                    other => {
                        self.emit(Op::Undef);
                        self.expr(other)?;
                    }
                }
                self.emit(Op::RequireCallable);
                let site = self.templates.len() as u32;
                self.templates.push((*site_id, quasis.clone()));
                self.emit(Op::TemplateObject(site));
                for substitution in subs {
                    self.expr(substitution)?;
                }
                let argc = u16::try_from(subs.len() + 1).map_err(|_| Bail)?;
                let cache = self.new_call_cache();
                self.emit(Op::CallWithThis(argc, cache));
                Ok(())
            }
            Expr::OptionalChain(inner) => {
                let mut shorts = Vec::new();
                self.opt_chain(inner, &mut shorts)?;
                if shorts.is_empty() {
                    return Ok(()); // no optional link actually taken a short path
                }
                let done = self.emit(Op::Jump(0));
                for j in shorts {
                    self.patch(j);
                }
                self.emit(Op::Undef);
                self.patch(done);
                Ok(())
            }
            Expr::Call {
                callee,
                args,
                optional: false,
            } => {
                if matches!(&**callee, Expr::Ident(n) if n == "eval") {
                    if !self.direct_eval {
                        return Err(Bail);
                    }

                    // ECMA-262 §13.3.6 evaluates the identifier Reference/GetValue before
                    // ArgumentListEvaluation, then recognizes direct eval only when that exact
                    // non-property Reference still names this Realm's %eval%. Retain the
                    // with-object receiver (if any) alongside the callee, stage every argument
                    // and spread in source order, and decide direct-vs-ordinary only after the
                    // complete list exists. PerformEval itself is atomic, but yield/await in its
                    // arguments stays visible to this heap continuation.
                    if self.home("eval").is_none() {
                        let name = self.name_idx("eval");
                        let cache = self.new_name_cache(name);
                        self.emit(Op::LoadNameForCall(name, cache));
                    } else {
                        self.emit(Op::Undef);
                        self.expr(callee)?;
                    }
                    self.emit(Op::NewArray);
                    for arg in args {
                        match arg {
                            ArrayElem::Item(expr) => {
                                self.expr(expr)?;
                                self.emit(Op::ArrayPush);
                            }
                            ArrayElem::Spread(expr) => {
                                self.expr(expr)?;
                                self.emit(Op::ArraySpread);
                            }
                            ArrayElem::Hole => return Err(Bail),
                        }
                    }
                    self.emit(Op::EvalCallArgsArray);
                    return Ok(());
                }
                match &**callee {
                    Expr::Member {
                        obj,
                        prop,
                        optional: false,
                    } if matches!(**obj, Expr::Super) => {
                        self.super_named_reference(prop);
                        self.emit(Op::SuperGetMethod);
                        self.finish_call(args, true, false)?;
                    }
                    Expr::Index {
                        obj,
                        index,
                        optional: false,
                    } if matches!(**obj, Expr::Super) => {
                        self.super_computed_reference(index)?;
                        self.emit(Op::SuperGetMethod);
                        self.finish_call(args, true, false)?;
                    }
                    Expr::Member {
                        obj,
                        prop,
                        optional: false,
                    } if prop.starts_with('#') => {
                        self.expr(obj)?;
                        let name = self.name_idx(prop);
                        self.emit(Op::GetPrivateMethod(name));
                        self.finish_call(args, true, false)?;
                    }
                    Expr::Member {
                        obj,
                        prop,
                        optional: false,
                    } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                        self.expr(obj)?;
                        let i = self.name_idx(prop);
                        let c = self.new_cache(i);
                        self.emit(Op::GetMethod(i, c));
                        self.finish_call(args, true, true)?;
                    }
                    Expr::Index {
                        obj,
                        index,
                        optional: false,
                    } if !matches!(**obj, Expr::Super) => {
                        self.expr(obj)?;
                        self.expr(index)?;
                        self.emit(Op::GetMethodElem);
                        self.finish_call(args, true, true)?;
                    }
                    Expr::Super => {
                        // ECMA-262 §13.3.7.1 obtains new.target and the live superclass before
                        // ArgumentListEvaluation. Keep both on the continuation stack while any
                        // argument or spread suspends, then perform Construct/BindThisValue/field
                        // initialization as one non-suspending completion step.
                        self.emit(Op::SuperCallStart);
                        self.emit(Op::NewArray);
                        for arg in args {
                            match arg {
                                ArrayElem::Item(expression) => {
                                    self.expr(expression)?;
                                    self.emit(Op::ArrayPush);
                                }
                                ArrayElem::Spread(expression) => {
                                    self.expr(expression)?;
                                    self.emit(Op::ArraySpread);
                                }
                                ArrayElem::Hole => return Err(Bail),
                            }
                        }
                        self.emit(Op::SuperCallArgsArray);
                    }
                    Expr::Ident(name) if self.home(name).is_none() => {
                        // Free-name callee: resolved before the arguments (spec order), and a
                        // `with (obj) f()` hit supplies obj as `this`.
                        let i = self.name_idx(name);
                        let c = self.new_name_cache(i);
                        self.emit(Op::LoadNameForCall(i, c));
                        self.finish_call(args, true, true)?;
                    }
                    other => {
                        self.expr(other)?;
                        self.finish_call(args, false, true)?;
                    }
                }
                Ok(())
            }
            Expr::New { callee, args } => {
                self.expr(callee)?;
                if args.iter().all(|arg| matches!(arg, ArrayElem::Item(_))) {
                    let argc = u16::try_from(args.len()).map_err(|_| Bail)?;
                    for arg in args {
                        let ArrayElem::Item(expr) = arg else {
                            unreachable!("plain constructor arguments checked above")
                        };
                        self.expr(expr)?;
                    }
                    let cache = self.new_construct_cache();
                    self.emit(Op::New(argc, cache));
                    return Ok(());
                }

                // Evaluate/expand each argument in source order before IsConstructor, as required
                // by EvaluateNew and ArgumentListEvaluation. The private array does not escape.
                self.emit(Op::NewArray);
                for arg in args {
                    match arg {
                        ArrayElem::Item(expr) => {
                            self.expr(expr)?;
                            self.emit(Op::ArrayPush);
                        }
                        ArrayElem::Spread(expr) => {
                            self.expr(expr)?;
                            self.emit(Op::ArraySpread);
                        }
                        ArrayElem::Hole => return Err(Bail),
                    }
                }
                self.emit(Op::NewArgsArray);
                Ok(())
            }
            Expr::Regex { body, flags } => {
                let body = self.name_idx(body);
                let flags = self.name_idx(flags);
                self.emit(Op::MakeRegExp(body, flags));
                Ok(())
            }
            Expr::Array(elems) => {
                if elems
                    .iter()
                    .all(|element| matches!(element, ArrayElem::Item(_)))
                {
                    let count = u16::try_from(elems.len()).map_err(|_| Bail)?;
                    for element in elems {
                        let ArrayElem::Item(value) = element else {
                            unreachable!("plain array literal fast path checked above")
                        };
                        self.expr(value)?;
                    }
                    self.emit(Op::MakeArray(count));
                    return Ok(());
                }

                self.emit(Op::NewArray);
                for element in elems {
                    match element {
                        ArrayElem::Item(value) => {
                            self.expr(value)?;
                            self.emit(Op::ArrayPush);
                        }
                        ArrayElem::Spread(value) => {
                            self.expr(value)?;
                            self.emit(Op::ArraySpread);
                        }
                        ArrayElem::Hole => {
                            self.emit(Op::ArrayHole);
                        }
                    }
                }
                Ok(())
            }
            Expr::Object(props) => {
                let plain = props.iter().all(|property| {
                    let PropDef::KeyValue { key, .. } = property else {
                        return false;
                    };
                    matches!(key, PropKey::Ident(key) if key != "__proto__" && !key.starts_with('#'))
                        || matches!(key, PropKey::Str(key) if &**key != "__proto__" && !key.starts_with('#'))
                });
                if !plain {
                    self.emit(Op::NewObject);
                    for property in props {
                        match property {
                            PropDef::KeyValue { key, value } | PropDef::Cover { key, value } => {
                                self.object_literal_key(key)?;
                                self.expr(value)?;
                                self.emit(Op::ObjectData(crate::eval::is_anonymous_fn(value)));
                            }
                            PropDef::Method { key, func } => {
                                self.object_literal_key(key)?;
                                let function = self.funcs.len() as u32;
                                self.funcs.push(func.clone());
                                self.emit(Op::ObjectMethod(function, 0));
                            }
                            PropDef::Getter { key, func } => {
                                self.object_literal_key(key)?;
                                let function = self.funcs.len() as u32;
                                self.funcs.push(func.clone());
                                self.emit(Op::ObjectMethod(function, 1));
                            }
                            PropDef::Setter { key, func } => {
                                self.object_literal_key(key)?;
                                let function = self.funcs.len() as u32;
                                self.funcs.push(func.clone());
                                self.emit(Op::ObjectMethod(function, 2));
                            }
                            PropDef::Spread(value) => {
                                self.expr(value)?;
                                self.emit(Op::ObjectSpread);
                            }
                            PropDef::Proto(value) => {
                                self.expr(value)?;
                                self.emit(Op::ObjectProto);
                            }
                        }
                    }
                    return Ok(());
                }

                let count = u16::try_from(props.len()).map_err(|_| Bail)?;
                // Keys must land contiguously in `names`; values go on the stack in order.
                let mut keys: Vec<String> = Vec::new();
                for p in props {
                    let PropDef::KeyValue { key, value } = p else {
                        unreachable!("plain object literal fast path checked above")
                    };
                    let k = match key {
                        PropKey::Ident(k) => k.clone(),
                        PropKey::Str(k) => k.to_string(),
                        _ => unreachable!("plain object literal key checked above"),
                    };
                    // NamedEvaluation: `{ m: function(){} }` names the anonymous function "m".
                    self.named_expr(value, &k)?;
                    keys.push(k);
                }
                // Keys go into `names` only after every value is compiled — value expressions
                // add names of their own, and the key range must stay contiguous.
                let start = self.names.len() as u32;
                for k in &keys {
                    self.names.push(Rc::from(k.as_str()));
                }
                // Distinct keys → a pre-shaped template site ({a:1, a:2} keeps the insert path:
                // the template's slot-indexed value writes assume one slot per key).
                let tidx = {
                    let mut sorted: Vec<&String> = keys.iter().collect();
                    sorted.sort();
                    if count > 0 && sorted.windows(2).all(|w| w[0] != w[1]) {
                        self.obj_maps += 1;
                        self.obj_maps - 1
                    } else {
                        u32::MAX
                    }
                };
                self.emit(Op::MakeObject(start, count, tidx));
                Ok(())
            }
            other => {
                if crate::eval::expr_contains(other, |expr| {
                    matches!(expr, Expr::Yield { .. } | Expr::Await(_))
                }) {
                    log_bail("expr", &format!("{:.60}", format!("{other:?}")));
                    return Err(Bail);
                }
                self.retain_eval_expr(other);
                Ok(())
            }
        }
    }

    /// `++`/`--` on a local slot, `obj.name`, or `obj[k]`; `kind` carries pre/post/discard.
    fn update_target(&mut self, arg: &Expr, kind: UpdKind) -> CResult {
        match arg {
            Expr::Paren(inner) => self.update_target(inner, kind),
            Expr::Ident(name) => match self.home(name) {
                Some(Home::Slot(slot, false)) => {
                    self.emit(Op::UpdateLocal(slot, kind));
                    Ok(())
                }
                Some(Home::Env(false)) => {
                    let n = self.name_idx(name);
                    self.emit(Op::UpdateCap(n, kind));
                    Ok(())
                }
                Some(home @ (Home::Slot(_, true) | Home::Env(true))) => {
                    let name = self.name_idx(name);
                    match home {
                        Home::Slot(slot, true) => self.emit(Op::LoadLocal(slot)),
                        Home::Env(true) => self.emit(Op::LoadCap(name)),
                        _ => unreachable!(),
                    };
                    self.emit(Op::UpdateConst(name, kind));
                    Ok(())
                }
                None => {
                    let n = self.name_idx(name);
                    let c = self.new_name_cache(n);
                    self.emit(Op::UpdateNameCached(n, c, kind));
                    Ok(())
                }
            },
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if matches!(**obj, Expr::Super) => {
                self.super_named_reference(prop);
                self.emit(Op::SuperUpdate(kind));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if matches!(**obj, Expr::Super) => {
                self.super_computed_reference(index)?;
                self.emit(Op::SuperUpdate(kind));
                Ok(())
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if prop.starts_with('#') => {
                self.expr(obj)?;
                let name = self.name_idx(prop);
                self.emit(Op::UpdatePrivate(name, kind));
                Ok(())
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.expr(obj)?;
                let i = self.name_idx(prop);
                let c = self.new_cache(i);
                self.emit(Op::UpdateProp(i, c, kind));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                self.expr(obj)?;
                self.expr(index)?;
                self.emit(Op::UpdateElem(kind));
                Ok(())
            }
            other => {
                log_bail("expr", &format!("{:.60}", format!("{other:?}")));
                Err(Bail)
            }
        }
    }

    fn assign(&mut self, op: &str, target: &Expr, value: &Expr) -> CResult {
        if matches!(op, "&&=" | "||=" | "??=") {
            return self.logical_assign(op, target, value);
        }
        if op == "=" && matches!(target, Expr::Array(_) | Expr::Object(_)) {
            if !self.is_coroutine {
                return Err(Bail);
            }
            // AssignmentExpression returns the unmodified RHS value after running the pattern.
            // Keep one copy beneath the continuation-aware AssignmentPattern operation.
            self.expr(value)?;
            self.emit(Op::Dup);
            self.assignment_pattern(target)?;
            return Ok(());
        }
        match target {
            Expr::Ident(name) => match self.home(name) {
                Some(Home::Slot(slot, is_const)) => {
                    if is_const {
                        return self.immutable_assignment(Home::Slot(slot, true), name, op, value);
                    }
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        self.emit(Op::LoadLocal(slot));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::Dup);
                    self.emit(Op::StoreLocal(slot));
                    Ok(())
                }
                Some(Home::Env(is_const)) => {
                    if is_const {
                        return self.immutable_assignment(Home::Env(true), name, op, value);
                    }
                    let n = self.name_idx(name);
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        self.emit(Op::LoadCap(n));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::Dup);
                    self.emit(Op::StoreCap(n));
                    Ok(())
                }
                None => {
                    let i = self.name_idx(name);
                    let reference = if self.name_reference_can_change(name) {
                        let reference = self.fresh_reference()?;
                        self.emit(Op::ResolveNameRef(i, reference));
                        Some(reference)
                    } else {
                        None
                    };
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        if let Some(reference) = reference {
                            self.emit(Op::LoadRef(reference));
                        } else {
                            let c = self.new_name_cache(i);
                            self.emit(Op::LoadName(i, c));
                        }
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::Dup);
                    if let Some(reference) = reference {
                        self.emit(Op::StoreRef(reference));
                    } else {
                        self.emit_store_name(i);
                    }
                    Ok(())
                }
            },
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if matches!(**obj, Expr::Super) => {
                self.super_named_reference(prop);
                if op == "=" {
                    self.expr(value)?;
                } else {
                    self.emit(Op::SuperGetKeep);
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                self.emit(Op::SuperSet);
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if matches!(**obj, Expr::Super) => {
                self.super_computed_reference(index)?;
                if op == "=" {
                    self.expr(value)?;
                } else {
                    self.emit(Op::SuperGetKeep);
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                self.emit(Op::SuperSet);
                Ok(())
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if prop.starts_with('#') => {
                self.expr(obj)?;
                let name = self.name_idx(prop);
                if op == "=" {
                    self.expr(value)?;
                } else {
                    self.emit(Op::GetPrivateKeep(name));
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                self.emit(Op::SetPrivate(name));
                Ok(())
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.expr(obj)?;
                let i = self.name_idx(prop);
                if op == "=" {
                    self.expr(value)?;
                } else {
                    // Compound: base evaluated once (Dup), get before the RHS — Reference order.
                    self.emit(Op::Dup);
                    let cg = self.new_cache(i);
                    self.emit(Op::GetProp(i, cg));
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                let c = self.new_cache(i);
                self.emit(Op::SetProp(i, c));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                if let Some(slot) = self.fused_elem_slot(obj, &[index.as_ref(), value]) {
                    self.expr(index)?;
                    if op == "=" {
                        self.expr(value)?;
                    } else {
                        self.emit(Op::ToPropKeyLocal(slot));
                        self.emit(Op::Dup);
                        self.emit(Op::GetElemLocal(slot));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::SetElemLocal(slot));
                    return Ok(());
                }
                self.expr(obj)?;
                self.expr(index)?;
                if op == "=" {
                    self.expr(value)?;
                } else {
                    // Compound: coerce a side-effecting key once, then read-modify-write.
                    self.emit(Op::ToPropKey);
                    self.emit(Op::Dup2);
                    self.emit(Op::GetElem);
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                self.emit(Op::SetElem);
                Ok(())
            }
            _ => Err(Bail),
        }
    }

    fn logical_super_assign(&mut self, op: &str, value: &Expr) -> CResult {
        self.emit(Op::SuperGetKeep);
        let current = self.fresh_slot("%logical-super-current%");
        let base = self.fresh_slot("%logical-super-base%");
        let key = self.fresh_slot("%logical-super-key%");
        let receiver = self.fresh_slot("%logical-super-receiver%");
        self.emit(Op::StoreLocal(current));
        self.emit(Op::StoreLocal(base));
        self.emit(Op::StoreLocal(key));
        self.emit(Op::StoreLocal(receiver));
        self.emit(Op::LoadLocal(current));
        let done = match op {
            "&&=" => self.emit(Op::JumpIfFalsePeek(0)),
            "||=" => self.emit(Op::JumpIfTruePeek(0)),
            "??=" => self.emit(Op::JumpIfNotNullishPeek(0)),
            _ => return Err(Bail),
        };
        self.emit(Op::Pop);
        self.emit(Op::LoadLocal(receiver));
        self.emit(Op::LoadLocal(key));
        self.emit(Op::LoadLocal(base));
        self.expr(value)?;
        self.emit(Op::SuperSet);
        self.patch(done);
        Ok(())
    }

    /// ECMA-262 §13.16.2 logical assignment: evaluate the left Reference exactly once, read it,
    /// and only evaluate/write the RHS when the operator's short-circuit test permits it. The
    /// old value remains the expression result on the short path; the stored RHS is the result on
    /// the write path. Hidden slots retain member bases/keys across a suspending RHS.
    fn logical_assign(&mut self, op: &str, target: &Expr, value: &Expr) -> CResult {
        fn short_jump(compiler: &mut Compiler, op: &str) -> Result<usize, Bail> {
            Ok(match op {
                "&&=" => compiler.emit(Op::JumpIfFalsePeek(0)),
                "||=" => compiler.emit(Op::JumpIfTruePeek(0)),
                "??=" => compiler.emit(Op::JumpIfNotNullishPeek(0)),
                _ => return Err(Bail),
            })
        }

        match target {
            Expr::Paren(inner) => self.logical_assign(op, inner, value),
            Expr::Ident(name) => match self.home(name) {
                Some(Home::Slot(slot, false)) => {
                    self.emit(Op::LoadLocal(slot));
                    let done = short_jump(self, op)?;
                    self.emit(Op::Pop);
                    self.named_expr(value, name)?;
                    self.emit(Op::Dup);
                    self.emit(Op::StoreLocal(slot));
                    self.patch(done);
                    Ok(())
                }
                Some(Home::Env(false)) => {
                    let name_index = self.name_idx(name);
                    self.emit(Op::LoadCap(name_index));
                    let done = short_jump(self, op)?;
                    self.emit(Op::Pop);
                    self.named_expr(value, name)?;
                    self.emit(Op::Dup);
                    self.emit(Op::StoreCap(name_index));
                    self.patch(done);
                    Ok(())
                }
                Some(home @ (Home::Slot(_, true) | Home::Env(true))) => {
                    let name_index = self.name_idx(name);
                    match home {
                        Home::Slot(slot, true) => self.emit(Op::LoadLocal(slot)),
                        Home::Env(true) => self.emit(Op::LoadCap(name_index)),
                        _ => unreachable!(),
                    };
                    let done = short_jump(self, op)?;
                    self.emit(Op::Pop);
                    self.named_expr(value, name)?;
                    match home {
                        Home::Slot(slot, true) => {
                            self.emit(Op::StoreConstLocal(slot, name_index));
                        }
                        Home::Env(true) => {
                            self.emit(Op::StoreConstCap(name_index));
                        }
                        _ => unreachable!(),
                    }
                    self.patch(done);
                    Ok(())
                }
                None => {
                    let name_index = self.name_idx(name);
                    let reference = if self.name_reference_can_change(name) {
                        let reference = self.fresh_reference()?;
                        self.emit(Op::ResolveNameRef(name_index, reference));
                        self.emit(Op::LoadRef(reference));
                        Some(reference)
                    } else {
                        let cache = self.new_name_cache(name_index);
                        self.emit(Op::LoadName(name_index, cache));
                        None
                    };
                    let done = short_jump(self, op)?;
                    self.emit(Op::Pop);
                    self.named_expr(value, name)?;
                    self.emit(Op::Dup);
                    if let Some(reference) = reference {
                        self.emit(Op::StoreRef(reference));
                    } else {
                        self.emit_store_name(name_index);
                    }
                    self.patch(done);
                    Ok(())
                }
            },
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if matches!(**obj, Expr::Super) => {
                self.super_named_reference(prop);
                self.logical_super_assign(op, value)
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if matches!(**obj, Expr::Super) => {
                self.super_computed_reference(index)?;
                self.logical_super_assign(op, value)
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if prop.starts_with('#') => {
                let base = self.fresh_slot("%logical-private-base%");
                self.expr(obj)?;
                self.emit(Op::StoreLocal(base));
                let name = self.name_idx(prop);
                self.emit(Op::LoadLocal(base));
                self.emit(Op::GetPrivate(name));
                let done = short_jump(self, op)?;
                self.emit(Op::Pop);
                self.emit(Op::LoadLocal(base));
                self.expr(value)?;
                self.emit(Op::SetPrivate(name));
                self.patch(done);
                Ok(())
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                let base_slot = self.fresh_slot("%logical-base%");
                self.expr(obj)?;
                self.emit(Op::StoreLocal(base_slot));
                let name_index = self.name_idx(prop);
                let get_cache = self.new_cache(name_index);
                self.emit(Op::GetPropLocal(base_slot, name_index, get_cache));
                let done = short_jump(self, op)?;
                self.emit(Op::Pop);
                self.emit(Op::LoadLocal(base_slot));
                self.expr(value)?;
                let set_cache = self.new_cache(name_index);
                self.emit(Op::SetProp(name_index, set_cache));
                self.patch(done);
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                let base_slot = self.fresh_slot("%logical-base%");
                let key_slot = self.fresh_slot("%logical-key%");
                self.expr(obj)?;
                self.expr(index)?;
                self.emit(Op::ToPropKey);
                self.emit(Op::StoreLocal(key_slot));
                self.emit(Op::StoreLocal(base_slot));
                self.emit(Op::LoadLocal(base_slot));
                self.emit(Op::LoadLocal(key_slot));
                self.emit(Op::GetElem);
                let done = short_jump(self, op)?;
                self.emit(Op::Pop);
                self.emit(Op::LoadLocal(base_slot));
                self.emit(Op::LoadLocal(key_slot));
                self.expr(value)?;
                self.emit(Op::SetElem);
                self.patch(done);
                Ok(())
            }
            _ => Err(Bail),
        }
    }

    fn emit_compound(&mut self, op: &str) -> CResult {
        let bop = match op {
            "+=" => Op::Add,
            "-=" => Op::Sub,
            "*=" => Op::Mul,
            "/=" => Op::Div,
            "%=" => Op::Mod,
            "&=" => Op::BitAnd,
            "|=" => Op::BitOr,
            "^=" => Op::BitXor,
            "<<=" => Op::Shl,
            ">>=" => Op::Shr,
            ">>>=" => Op::UShr,
            "**=" => {
                let i = self.name_idx("**");
                Op::GenBin(i)
            }
            _ => return Err(Bail),
        };
        self.emit(bop);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// VM
// ---------------------------------------------------------------------------------------------

/// Completion carried through an asynchronous DisposeResources operation. Distinct return kinds
/// preserve async-generator Await rules and externally injected returns across nested handlers.
pub enum DisposeCompletion {
    Normal,
    Throw(Value),
    SourceReturn(Value),
    BareReturn,
    ResumeReturn(Value),
    Jump { target: usize, handler_depth: usize },
}

/// How one run of the VM ended: the body returned a value, suspended at an `await` (async bodies
/// only — see [`VmCoro`]), or — carried as `Err(Abrupt::Throw)` — threw.
pub enum VmStep {
    Done(Value),
    /// An explicit `return expression`; async generators must Await its value before completion.
    Return(Value),
    /// An explicit source `return;`, kept distinct from both body fallthrough and expression
    /// returns so it unwinds resources without adding an async-generator Await.
    BareReturn,
    /// A generator `.return(value)` completion whose value was already awaited by
    /// AsyncGeneratorUnwrapYieldResumption before it entered a finalizer.
    ResumeReturn(Value),
    /// A `break`/`continue` Completion. `handler_depth` identifies the handler stack at the
    /// destination so only intervening regions are unwound.
    AbruptJump {
        target: usize,
        handler_depth: usize,
    },
    Await(Value),
    AsyncClose {
        iterator: Value,
        from_sync: bool,
        swallow_error: bool,
    },
    Dispose {
        frame: Vec<crate::interpreter::Disposable>,
        completion: DisposeCompletion,
    },
    Yield(Value),
    YieldStar(Value),
}

/// Execute a compiled function body. `env` is the root for free-name resolution — the *definition*
/// environment when called leanly (see `Interp::call_user_inner`), since a compiled body has no
/// observable activation. Parameters seed straight into slots; `this_val` is the already-bound
/// `this` (computed only when the body reads it). Synchronous bodies only — an async body runs
/// through [`VmCoro`], which drives [`run_vm`] and can suspend it.
///
/// The slot and operand-stack buffers come from a per-interpreter pool ([`Interp::vm_pool`]) so a
/// hot call tree does not allocate two `Vec`s per call.
pub fn run(
    i: &mut Interp,
    chunk: &Chunk,
    env: &Env,
    this_val: Value,
    args: &[Value],
) -> Result<Value, Abrupt> {
    // Captured locals (and a lexically-read `this`) live in a per-call activation env; slots
    // hold everything else. No captures → the definition env is used directly.
    let mut env = chunk.make_run_env(i, env, &this_val, args);
    let cap_env = env.clone();
    let (mut slots, mut stack) = i.vm_pool.pop().unwrap_or_default();
    let seed = chunk.n_params.min(args.len());
    slots.extend_from_slice(&args[..seed]);
    slots.resize(chunk.n_slots, Value::Undefined);
    if let Some(s) = chunk.arguments_slot {
        slots[s as usize] = Value::Obj(i.make_compiled_arguments_object(args, &env));
    }
    let mut pc = 0usize;
    let mut handlers: Vec<Handler> = Vec::new();
    let mut disposal_frames: Vec<Vec<crate::interpreter::Disposable>> = Vec::new();
    let mut class_states = (0..chunk.class_plans.len())
        .map(|_| None)
        .collect::<Vec<_>>();
    let mut references = (0..chunk.n_refs).map(|_| None).collect::<Vec<_>>();
    let r = drive_vm(
        i,
        chunk,
        &mut env,
        &cap_env,
        &mut references,
        &mut slots,
        &mut stack,
        &mut pc,
        &this_val,
        &mut handlers,
        &mut disposal_frames,
        &mut class_states,
        None,
        false,
    );
    slots.clear();
    stack.clear();
    if i.vm_pool.len() < 64 {
        i.vm_pool.push((slots, stack));
    }
    match r? {
        VmStep::Done(v) | VmStep::Return(v) | VmStep::ResumeReturn(v) => Ok(v),
        VmStep::BareReturn => Ok(Value::Undefined),
        VmStep::AbruptJump { .. } => unreachable!("drive_vm consumes loop completions"),
        VmStep::Await(_)
        | VmStep::AsyncClose { .. }
        | VmStep::Dispose { .. }
        | VmStep::Yield(_)
        | VmStep::YieldStar(_) => {
            unreachable!("a synchronous bytecode function cannot suspend")
        }
    }
}

/// Drive the VM through abrupt completions. Catch handlers consume only throws; finally handlers
/// consume throws and returns, restore their saved operand depth, and jump to a completion-specific
/// pad with the completion value pushed. `pending` injects a completion before the first step (for
/// a rejected `await`, generator `.throw()`, or generator `.return()`).
#[allow(clippy::too_many_arguments)]
fn drive_vm(
    i: &mut Interp,
    chunk: &Chunk,
    env: &mut Env,
    cap_env: &Env,
    references: &mut [Option<crate::eval::PreparedReference>],
    slots: &mut [Value],
    stack: &mut Vec<Value>,
    pc: &mut usize,
    this_val: &Value,
    handlers: &mut Vec<Handler>,
    disposal_frames: &mut Vec<Vec<crate::interpreter::Disposable>>,
    class_states: &mut [Option<crate::eval::PreparedClassEvaluation>],
    mut pending: Option<PendingCompletion>,
    defer_source_return: bool,
) -> Result<VmStep, Abrupt> {
    loop {
        let outcome = match pending.take() {
            Some(PendingCompletion::Throw(error)) => Err(Abrupt::Throw(error)),
            Some(PendingCompletion::ResumeReturn(value)) => Ok(VmStep::ResumeReturn(value)),
            Some(PendingCompletion::SourceReturn(value)) => Ok(VmStep::Return(value)),
            Some(PendingCompletion::BareReturn) => Ok(VmStep::BareReturn),
            Some(PendingCompletion::Jump {
                target,
                handler_depth,
            }) => Ok(VmStep::AbruptJump {
                target,
                handler_depth,
            }),
            None => run_vm(
                i,
                chunk,
                env,
                cap_env,
                references,
                slots,
                stack,
                pc,
                this_val,
                handlers,
                disposal_frames,
                class_states,
            ),
        };
        match outcome {
            Ok(VmStep::Return(value)) => {
                // In an async generator, ReturnStatement awaits its expression before creating
                // the Return Completion that enters surrounding finalizers/IteratorClose.
                if defer_source_return {
                    return Ok(VmStep::Return(value));
                }
                let mut value = Some(value);
                while let Some(handler) = handlers.pop() {
                    let return_pc = match handler.target {
                        HandlerTarget::Finally { return_pc, .. }
                        | HandlerTarget::Iterator { return_pc, .. } => return_pc,
                        HandlerTarget::Catch { .. } => continue,
                    };
                    stack.truncate(handler.stack_depth);
                    stack.push(value.take().expect("return completion consumed once"));
                    *pc = return_pc;
                    break;
                }
                if let Some(value) = value {
                    return Ok(VmStep::Return(value));
                }
            }
            Ok(VmStep::BareReturn) => {
                let mut handled = false;
                while let Some(handler) = handlers.pop() {
                    let bare_return_pc = match handler.target {
                        HandlerTarget::Finally { bare_return_pc, .. }
                        | HandlerTarget::Iterator { bare_return_pc, .. } => bare_return_pc,
                        HandlerTarget::Catch { .. } => continue,
                    };
                    stack.truncate(handler.stack_depth);
                    *pc = bare_return_pc;
                    handled = true;
                    break;
                }
                if !handled {
                    return Ok(VmStep::BareReturn);
                }
            }
            Ok(VmStep::ResumeReturn(value)) => {
                let mut value = Some(value);
                while let Some(handler) = handlers.pop() {
                    let resume_return_pc = match handler.target {
                        HandlerTarget::Finally {
                            resume_return_pc, ..
                        }
                        | HandlerTarget::Iterator {
                            resume_return_pc, ..
                        } => resume_return_pc,
                        HandlerTarget::Catch { .. } => continue,
                    };
                    stack.truncate(handler.stack_depth);
                    stack.push(
                        value
                            .take()
                            .expect("resumed return completion consumed once"),
                    );
                    *pc = resume_return_pc;
                    break;
                }
                if let Some(value) = value {
                    return Ok(VmStep::ResumeReturn(value));
                }
            }
            Ok(VmStep::AbruptJump {
                target,
                handler_depth,
            }) => {
                let mut completion = Some((target, handler_depth));
                while handlers.len() > handler_depth {
                    let handler = handlers.pop().expect("handler depth checked");
                    if let HandlerTarget::Finally { jump_pc, .. } = handler.target {
                        stack.truncate(handler.stack_depth);
                        stack.push(Value::Num(target as f64));
                        stack.push(Value::Num(handler_depth as f64));
                        *pc = jump_pc;
                        completion = None;
                        break;
                    }
                }
                if completion.is_some() {
                    debug_assert_eq!(handlers.len(), handler_depth);
                    *pc = target;
                }
            }
            Ok(step) => return Ok(step),
            Err(Abrupt::Throw(error)) => {
                let mut error = Some(error);
                if let Some(handler) = handlers.pop() {
                    let throw_pc = match handler.target {
                        HandlerTarget::Catch { throw_pc }
                        | HandlerTarget::Finally { throw_pc, .. }
                        | HandlerTarget::Iterator { throw_pc, .. } => throw_pc,
                    };
                    stack.truncate(handler.stack_depth);
                    stack.push(error.take().expect("throw completion consumed once"));
                    *pc = throw_pc;
                }
                if let Some(error) = error {
                    return Err(Abrupt::Throw(error));
                }
            }
            // Return/Break/Continue never escape a compiled body as an Abrupt; propagate defensively.
            Err(other) => return Err(other),
        }
    }
}

fn for_in_step(
    i: &mut Interp,
    slots: &mut [Value],
    keys_slot: u16,
    index_slot: u16,
    source_slot: u16,
) -> Result<Option<Value>, Abrupt> {
    loop {
        let index = match slots[index_slot as usize] {
            Value::Num(index) => index as u32,
            _ => unreachable!("for-in cursor is an internal integer"),
        };
        let key = match &slots[keys_slot as usize] {
            Value::Obj(keys) => keys
                .borrow()
                .props
                .get_index(index)
                .map(|prop| prop.value()),
            _ => unreachable!("for-in keys are stored in an internal array"),
        };
        let Some(key) = key else {
            return Ok(None);
        };
        slots[index_slot as usize] = Value::Num(index as f64 + 1.0);
        if matches!(slots[source_slot as usize], Value::Obj(_)) {
            let Value::Str(name) = &key else {
                unreachable!("for-in candidates are strings")
            };
            let source = slots[source_slot as usize].clone();
            if !i.js_has_property(&source, name)? {
                continue;
            }
        }
        return Ok(Some(key));
    }
}

/// Run a retained AssignmentPattern against a slot-backed VM frame. The tree-walker operates on
/// Environment Records, so project the lexically visible slot bindings into one child record,
/// execute the normative algorithm, and copy bindings back even after an abrupt completion (an
/// earlier element/property assignment remains observable when a later one throws).
fn assign_target_with_slots(
    i: &mut Interp,
    plan: &AssignmentTargetPlan,
    value: Value,
    env: &Env,
    slots: &mut [Value],
) -> Result<(), Abrupt> {
    let projected = crate::interpreter::new_scope(Some(env.clone()));
    {
        let mut scope = projected.borrow_mut();
        for local in &plan.locals {
            let value = slots[local.slot as usize].clone();
            let initialized = !matches!(value, Value::Empty);
            scope.vars.insert(
                local.name.clone(),
                crate::interpreter::Binding::data(value, local.mutable, initialized),
            );
        }
    }

    // Direct JIT-to-JIT calls do not maintain the interpreter's ambient strict flag. Assignment
    // target semantics therefore carry the compiled function's strictness explicitly.
    let old_strict = std::mem::replace(&mut i.strict, plan.strict);
    let result = i.assign_to_target(&plan.target, value, &projected);
    i.strict = old_strict;

    let scope = projected.borrow();
    for local in &plan.locals {
        let binding = scope
            .vars
            .get(&local.name)
            .expect("projected assignment binding remains present");
        slots[local.slot as usize] = if binding.initialized {
            binding.value.clone()
        } else {
            Value::Empty
        };
    }
    result
}

/// Execute one uncommon, non-suspending expression through the normative evaluator while the
/// surrounding function remains a heap VM continuation. Slot bindings are projected exactly like
/// AssignmentPattern's shared bridge and copied back after both normal and abrupt completion.
fn eval_expr_with_slots(
    i: &mut Interp,
    plan: &EvalExprPlan,
    env: &Env,
    slots: &mut [Value],
) -> Result<Value, Abrupt> {
    let projected = crate::interpreter::new_scope(Some(env.clone()));
    {
        let mut scope = projected.borrow_mut();
        for local in &plan.locals {
            let value = slots[local.slot as usize].clone();
            let initialized = !matches!(value, Value::Empty);
            scope.vars.insert(
                local.name.clone(),
                crate::interpreter::Binding::data(value, local.mutable, initialized),
            );
        }
    }

    let old_strict = std::mem::replace(&mut i.strict, plan.strict);
    let old_name = std::mem::replace(&mut i.pending_fn_name, plan.name.clone());
    let result = i.eval(&plan.expr, &projected);
    i.pending_fn_name = old_name;
    i.strict = old_strict;

    let scope = projected.borrow();
    for local in &plan.locals {
        let binding = scope
            .vars
            .get(&local.name)
            .expect("projected expression binding remains present");
        slots[local.slot as usize] = if binding.initialized {
            binding.value.clone()
        } else {
            Value::Empty
        };
    }
    result
}

fn rejected_intrinsic_promise(i: &mut Interp, reason: Value) -> Value {
    let promise = i.new_promise();
    i.reject_promise(&promise, reason);
    promise
}

/// The rejection handler installed by AsyncFromSyncIteratorContinuation when a live sync
/// iterator value rejects. IteratorClose receives the throw completion, so its own failures are
/// discarded and the original rejection is propagated by throwing it from this reaction.
fn async_from_sync_close_rejection(
    i: &mut Interp,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let iterator = args.first().cloned().unwrap_or(Value::Undefined);
    let error = args.get(1).cloned().unwrap_or(Value::Undefined);
    i.iterator_close(&iterator);
    Err(error)
}

fn async_from_sync_abrupt_promise(i: &mut Interp, completion: Abrupt) -> Result<Value, Abrupt> {
    match completion {
        Abrupt::Throw(error) => Ok(rejected_intrinsic_promise(i, error)),
        other => Err(other),
    }
}

fn array_literal_append(i: &mut Interp, array: &Value, value: Option<Value>) -> Result<(), Abrupt> {
    let Value::Obj(array) = array else {
        unreachable!("array literal builder retains an Array")
    };
    let index = i.array_length(array);
    if index >= u32::MAX as usize {
        return Err(i.throw("RangeError", "invalid array length"));
    }
    let mut object = array.borrow_mut();
    if let Some(value) = value {
        object
            .props
            .insert(index.to_string(), crate::value::Property::plain(value));
    }
    object
        .props
        .get_mut("length")
        .expect("fresh Array has length")
        .set_value(Value::Num(index as f64 + 1.0));
    Ok(())
}

/// Consume the private dense array built by ArgumentListEvaluation lowering. No user code can
/// observe this object between creation and consumption, so every index is a plain own value.
fn argument_array_values(i: &Interp, array: Value) -> Vec<Value> {
    let Value::Obj(array) = array else {
        unreachable!("argument-list builder retains an Array")
    };
    let length = i.array_length(&array);
    let array = array.borrow();
    (0..length)
        .map(|index| {
            array
                .props
                .get_index(index as u32)
                .expect("argument-list builder creates no holes")
                .value()
        })
        .collect()
}

/// Run from `*pc` until the body returns (`Done`), suspends at an `await` (`Await`, async bodies
/// only), or throws (`Err(Abrupt::Throw)`, caught by [`drive_vm`]). Operates on borrowed state so an
/// async [`VmCoro`] can save it at a suspension and restore it on resume.
#[allow(clippy::too_many_arguments)]
fn run_vm(
    i: &mut Interp,
    chunk: &Chunk,
    env: &mut Env,
    cap_env: &Env,
    references: &mut [Option<crate::eval::PreparedReference>],
    slots: &mut [Value],
    stack: &mut Vec<Value>,
    pc: &mut usize,
    this_val: &Value,
    handlers: &mut Vec<Handler>,
    disposal_frames: &mut Vec<Vec<crate::interpreter::Disposable>>,
    class_states: &mut [Option<crate::eval::PreparedClassEvaluation>],
) -> Result<VmStep, Abrupt> {
    macro_rules! pop {
        () => {
            stack.pop().expect("vm stack underflow")
        };
    }
    loop {
        let op_pc = *pc;
        let op = chunk.ops[op_pc];
        *pc += 1;
        match op {
            Op::Const(k) => stack.push(chunk.consts[k as usize].clone()),
            Op::Undef => stack.push(Value::Undefined),
            Op::Dup => {
                let t = stack.last().expect("vm stack underflow").clone();
                stack.push(t);
            }
            Op::Pop => {
                pop!();
            }
            Op::LoadLocal(s) => {
                let v = slots[s as usize].clone();
                if matches!(v, Value::Empty) {
                    return Err(i.throw(
                        "ReferenceError",
                        format!(
                            "cannot access '{}' before initialization",
                            chunk.slot_names[s as usize]
                        ),
                    ));
                }
                stack.push(v);
            }
            Op::StoreLocal(s) => slots[s as usize] = pop!(),
            Op::UpdateLocal(s, kind) => {
                let idx = s as usize;
                match &slots[idx] {
                    // Reading a slot still in its TDZ is the same ReferenceError as LoadLocal.
                    Value::Empty => {
                        return Err(i.throw(
                            "ReferenceError",
                            format!(
                                "cannot access '{}' before initialization",
                                chunk.slot_names[idx]
                            ),
                        ));
                    }
                    // Fast path: a numeric slot updates in place.
                    Value::Num(n) => {
                        let profiling =
                            observe_arithmetic_operand(&chunk.feedback, op_pc, &slots[idx]);
                        let old = *n;
                        let new = match kind {
                            UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard => old + 1.0,
                            UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard => old - 1.0,
                        };
                        slots[idx] = Value::Num(new);
                        observe_arithmetic_result(&chunk.feedback, op_pc, profiling, &slots[idx]);
                        match kind {
                            UpdKind::PreInc | UpdKind::PreDec => stack.push(Value::Num(new)),
                            UpdKind::PostInc | UpdKind::PostDec => stack.push(Value::Num(old)),
                            UpdKind::IncDiscard | UpdKind::DecDiscard => {}
                        }
                    }
                    // BigInt updates stay BigInt (ToNumeric, not ToNumber) — never coerced to a
                    // Number and never thrown on like unary `+` would.
                    Value::BigInt(n) => {
                        let profiling =
                            observe_arithmetic_operand(&chunk.feedback, op_pc, &slots[idx]);
                        let old = n.clone();
                        let one = crate::bigint::JsBigInt::from_u64(1);
                        let new = match kind {
                            UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard => {
                                old.add(&one)
                            }
                            UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard => {
                                old.sub(&one)
                            }
                        };
                        slots[idx] = Value::BigInt(new.clone());
                        observe_arithmetic_result(&chunk.feedback, op_pc, profiling, &slots[idx]);
                        match kind {
                            UpdKind::PreInc | UpdKind::PreDec => stack.push(Value::BigInt(new)),
                            UpdKind::PostInc | UpdKind::PostDec => stack.push(Value::BigInt(old)),
                            UpdKind::IncDiscard | UpdKind::DecDiscard => {}
                        }
                    }
                    // Anything else: the shared ToNumeric path may run user code and may produce
                    // either Number or BigInt. This is cold compared with the two direct tags.
                    _ => {
                        let old = slots[idx].clone();
                        if let Some(value) =
                            step_value(i, &chunk.feedback, op_pc, kind, old, |_, value| {
                                slots[idx] = value;
                                Ok(())
                            })?
                        {
                            stack.push(value);
                        }
                    }
                }
            }
            Op::Tdz(s) => slots[s as usize] = Value::Empty,
            Op::LoadCap(n) => {
                stack.push(chunk.load_cap_ic(i, cap_env, n)?);
            }
            Op::StoreCap(n) => {
                let v = pop!();
                chunk.store_cap_ic(i, cap_env, n, v, false)?;
            }
            Op::StoreCapInit(n) => {
                let v = pop!();
                chunk.store_cap_ic(i, cap_env, n, v, true)?;
            }
            Op::UpdateCap(n, kind) => {
                let name = &chunk.names[n as usize];
                let old = {
                    let b = cap_env.borrow();
                    let bd = b.vars.get(name).expect("captured binding missing");
                    if !bd.initialized {
                        let msg = format!("cannot access '{name}' before initialization");
                        drop(b);
                        return Err(i.throw("ReferenceError", msg));
                    }
                    bd.value.clone()
                };
                step_and_store(i, stack, &chunk.feedback, op_pc, kind, old, |_, v| {
                    if let Some(bd) = cap_env.borrow_mut().vars.get_mut(name) {
                        bd.value = v;
                    }
                    Ok(())
                })?;
            }
            Op::UpdateName(n, kind) => {
                let name = &chunk.names[n as usize];
                let old = i.get_var(name, env)?;
                step_and_store(i, stack, &chunk.feedback, op_pc, kind, old, |i, v| {
                    i.assign_free_name(name, v, env)
                })?;
            }
            Op::UpdateNameCached(n, c, kind) => {
                let name = &chunk.names[n as usize];
                let old = chunk.load_name_ic(i, env, n, c)?;
                step_and_store(i, stack, &chunk.feedback, op_pc, kind, old, |i, v| {
                    i.assign_free_name(name, v, env)
                })?;
            }
            Op::MakeClosure(fidx, name_n) => {
                let v = i.make_function(chunk.funcs[fidx as usize].clone(), env.clone());
                if name_n != u32::MAX {
                    i.set_fn_name(&v, &chunk.names[name_n as usize]);
                }
                observe_allocation(
                    &chunk.feedback,
                    op_pc,
                    crate::feedback::AllocationObjectKind::Function,
                    0,
                );
                stack.push(v);
            }
            Op::LoadName(n, c) => {
                let v = chunk.load_name_ic(i, env, n, c)?;
                stack.push(v);
            }
            Op::StoreName(n) => {
                let v = pop!();
                i.assign_free_name(&chunk.names[n as usize], v, env)?;
            }
            Op::StoreNameCached(n, c) => {
                let v = pop!();
                chunk.store_name_ic(i, env, n, c, v)?;
            }
            Op::ResolveNameRef(name, reference) => {
                references[reference as usize] =
                    Some(i.prepare_name_reference(&chunk.names[name as usize], env)?);
            }
            Op::LoadRef(reference) => {
                let reference = references[reference as usize]
                    .as_mut()
                    .expect("prepared name reference was resolved before use");
                stack.push(i.read_prepared_reference(reference)?);
            }
            Op::StoreRef(reference) => {
                let value = pop!();
                let reference = references[reference as usize]
                    .as_mut()
                    .expect("prepared name reference was resolved before use");
                i.write_prepared_reference(reference, value)?;
            }
            Op::StoreConstLocal(s, n) => {
                pop!();
                if matches!(slots[s as usize], Value::Empty) {
                    return Err(i.throw(
                        "ReferenceError",
                        format!(
                            "cannot access '{}' before initialization",
                            chunk.slot_names[s as usize]
                        ),
                    ));
                }
                return Err(i.throw(
                    "TypeError",
                    format!(
                        "assignment to constant variable '{}'",
                        chunk.names[n as usize]
                    ),
                ));
            }
            Op::StoreConstCap(n) => {
                pop!();
                chunk.reject_const_cap_store(i, env, n)?;
                unreachable!("immutable captured store always completes abruptly");
            }
            Op::UpdateConst(n, kind) => {
                let old = pop!();
                let name = chunk.names[n as usize].clone();
                step_value(i, &chunk.feedback, op_pc, kind, old, |i, _| {
                    Err(i.throw(
                        "TypeError",
                        format!("assignment to constant variable '{name}'"),
                    ))
                })?;
                unreachable!("immutable update always completes abruptly");
            }
            Op::LoadThis => stack.push(this_val.clone()),
            Op::LoadLexicalThis => stack.push(i.get_var("this", env)?),
            Op::RequireObject => {
                if matches!(
                    stack.last().expect("vm stack underflow"),
                    Value::Undefined | Value::Null
                ) {
                    return Err(i.throw("TypeError", "cannot access property of null or undefined"));
                }
            }
            Op::GetProp(n, c) => {
                let obj = pop!();
                let v = get_named_property(
                    i,
                    chunk,
                    op_pc,
                    &obj,
                    &chunk.names[n as usize],
                    &chunk.caches[c as usize],
                )?;
                stack.push(v);
            }
            Op::GetPropThis(n, c) => {
                let v = get_named_property(
                    i,
                    chunk,
                    op_pc,
                    this_val,
                    &chunk.names[n as usize],
                    &chunk.caches[c as usize],
                )?;
                stack.push(v);
            }
            Op::GetPropLocal(s, n, c) => {
                let obj = slots[s as usize].clone();
                if matches!(obj, Value::Empty) {
                    return Err(i.throw(
                        "ReferenceError",
                        format!(
                            "cannot access '{}' before initialization",
                            chunk.slot_names[s as usize]
                        ),
                    ));
                }
                let v = get_named_property(
                    i,
                    chunk,
                    op_pc,
                    &obj,
                    &chunk.names[n as usize],
                    &chunk.caches[c as usize],
                )?;
                stack.push(v);
            }
            Op::SetProp(n, c) => {
                let v = pop!();
                let obj = pop!();
                set_named_property(
                    i,
                    chunk,
                    op_pc,
                    &obj,
                    &chunk.names[n as usize],
                    v.clone(),
                    &chunk.caches[c as usize],
                )?;
                stack.push(v);
            }
            Op::SetPropDrop(n, c) => {
                let v = pop!();
                let obj = pop!();
                set_named_property(
                    i,
                    chunk,
                    op_pc,
                    &obj,
                    &chunk.names[n as usize],
                    v,
                    &chunk.caches[c as usize],
                )?;
            }
            Op::SetPropThisDrop(n, c) => {
                let v = pop!();
                set_named_property(
                    i,
                    chunk,
                    op_pc,
                    this_val,
                    &chunk.names[n as usize],
                    v,
                    &chunk.caches[c as usize],
                )?;
            }
            Op::SetPropLocalDrop(s, n, c) => {
                let v = pop!();
                let obj = slots[s as usize].clone();
                if matches!(obj, Value::Empty) {
                    return Err(i.throw(
                        "ReferenceError",
                        format!(
                            "cannot access '{}' before initialization",
                            chunk.slot_names[s as usize]
                        ),
                    ));
                }
                set_named_property(
                    i,
                    chunk,
                    op_pc,
                    &obj,
                    &chunk.names[n as usize],
                    v,
                    &chunk.caches[c as usize],
                )?;
            }
            Op::DestructureGuard => {
                if matches!(
                    stack.last().expect("vm stack underflow"),
                    Value::Undefined | Value::Null
                ) {
                    return Err(i.throw("TypeError", "cannot destructure null or undefined"));
                }
            }
            Op::DestructureArr(n) => {
                let v = pop!();
                let (it, nx) = i.get_iterator(&v)?;
                let mut done = false;
                for _ in 0..n {
                    if !done {
                        match i.iterator_step(&it, &nx)? {
                            Some(x) => {
                                stack.push(x);
                                continue;
                            }
                            None => done = true,
                        }
                    }
                    stack.push(Value::Undefined);
                }
                if !done {
                    i.iterator_close_normal(&it)?;
                }
            }
            Op::AssignTarget(target) => {
                let value = pop!();
                assign_target_with_slots(
                    i,
                    &chunk.assignment_targets[target as usize],
                    value,
                    env,
                    slots,
                )?;
            }
            Op::EvalExpr(plan) => {
                let value = eval_expr_with_slots(i, &chunk.eval_exprs[plan as usize], env, slots)?;
                stack.push(value);
            }
            Op::ClassStart(plan, decorator_count) => {
                let class_plan = &chunk.class_plans[plan as usize];
                let value_count = (decorator_count as usize)
                    .checked_mul(2)
                    .expect("class decorator count overflow");
                let split = stack
                    .len()
                    .checked_sub(value_count)
                    .expect("compiled class decorator stack underflow");
                let decorator_values = stack.split_off(split);
                let mut state = i.begin_class_evaluation(&class_plan.class, env);
                let mut decorator_values = decorator_values.into_iter();
                while let Some(this_value) = decorator_values.next() {
                    let callback = decorator_values
                        .next()
                        .expect("compiled class decorator record is complete");
                    i.prepare_class_decorator(&mut state, None, callback, this_value);
                }
                *env = state.outer_class_env.clone();
                assert!(
                    class_states[plan as usize].replace(state).is_none(),
                    "class plan re-entered before its previous evaluation completed"
                );
                i.strict = true;
            }
            Op::ClassHeritage(plan, present) => {
                let parent = present.then(|| pop!());
                let state = class_states[plan as usize]
                    .as_mut()
                    .expect("ClassStart precedes heritage evaluation");
                i.prepare_class_heritage(state, parent)?;
                *env = state.class_env.clone();
            }
            Op::ClassKey(plan, member) => {
                let value = pop!();
                let state = class_states[plan as usize]
                    .as_mut()
                    .expect("ClassStart precedes computed-name evaluation");
                i.prepare_class_key(
                    state,
                    &chunk.class_plans[plan as usize].class,
                    member as usize,
                    value,
                )?;
            }
            Op::ClassDecorator(plan, member) => {
                let callback = pop!();
                let this_value = pop!();
                let state = class_states[plan as usize]
                    .as_mut()
                    .expect("ClassStart precedes decorator evaluation");
                i.prepare_class_decorator(state, Some(member as usize), callback, this_value);
            }
            Op::ClassFinish(plan) => {
                let state = class_states[plan as usize]
                    .take()
                    .expect("ClassStart precedes class completion");
                *env = state
                    .outer_class_env
                    .borrow()
                    .parent
                    .clone()
                    .expect("class environment retains its outer environment");
                let class_plan = &chunk.class_plans[plan as usize];
                let saved_name =
                    std::mem::replace(&mut i.pending_fn_name, class_plan.inferred_name.clone());
                let result = i.finish_class_evaluation(&class_plan.class, state);
                i.pending_fn_name = saved_name;
                i.strict = if class_states.iter().any(Option::is_some) {
                    true
                } else {
                    chunk.strict
                };
                stack.push(result?);
            }
            Op::ClassAbort(plan) => {
                if let Some(state) = class_states[plan as usize].take() {
                    *env = state
                        .outer_class_env
                        .borrow()
                        .parent
                        .clone()
                        .expect("class environment retains its outer environment");
                }
                i.strict = if class_states.iter().any(Option::is_some) {
                    true
                } else {
                    chunk.strict
                };
            }
            Op::PushWith => {
                let value = pop!();
                if matches!(value, Value::Undefined | Value::Null) {
                    return Err(i.throw("TypeError", "Cannot convert undefined or null to object"));
                }
                let object = crate::builtins::box_primitive_pub(i, value);
                *env = crate::interpreter::new_with_scope(env.clone(), object);
            }
            Op::PushLex(scope) | Op::PushCatchLex(scope) => {
                let next = if matches!(op, Op::PushCatchLex(_)) {
                    crate::interpreter::new_catch_scope(env.clone())
                } else {
                    crate::interpreter::new_scope(Some(env.clone()))
                };
                {
                    let mut record = next.borrow_mut();
                    for binding in &chunk.lexical_scopes[scope as usize] {
                        record.lexical_names.push(binding.name.to_string());
                        record.vars.insert(
                            binding.name.clone(),
                            crate::interpreter::Binding {
                                value: Value::Undefined,
                                mutable: !binding.is_const,
                                strict_immutable: binding.is_const,
                                initialized: false,
                                import_ref: None,
                                deletable: false,
                            },
                        );
                    }
                }
                *env = next;
            }
            Op::CloneLex(scope) => {
                let parent = env
                    .borrow()
                    .parent
                    .clone()
                    .expect("per-iteration lexical environment has a parent");
                let next = crate::interpreter::new_scope(Some(parent));
                {
                    let current = env.borrow();
                    let mut record = next.borrow_mut();
                    for binding in &chunk.lexical_scopes[scope as usize] {
                        let previous = current
                            .vars
                            .get(&binding.name)
                            .expect("per-iteration binding missing");
                        record.lexical_names.push(binding.name.to_string());
                        record.vars.insert(
                            binding.name.clone(),
                            crate::interpreter::Binding {
                                value: previous.value.clone(),
                                mutable: previous.mutable,
                                strict_immutable: previous.strict_immutable,
                                initialized: previous.initialized,
                                import_ref: None,
                                deletable: false,
                            },
                        );
                    }
                }
                *env = next;
            }
            Op::InitLex(name) => {
                let name = &chunk.names[name as usize];
                let mut record = env.borrow_mut();
                let binding = record
                    .vars
                    .get_mut(name)
                    .expect("lexical initialization requires an own binding");
                binding.value = pop!();
                binding.initialized = true;
            }
            Op::PopEnv => {
                let parent = env
                    .borrow()
                    .parent
                    .clone()
                    .expect("compiled environment scope has a parent");
                *env = parent;
            }
            Op::PushDisposeFrame => disposal_frames.push(Vec::new()),
            Op::AddDisposable(is_async) => {
                let value = stack.last().expect("vm stack underflow");
                if let Some(resource) = i.create_disposable(value, is_async)? {
                    disposal_frames
                        .last_mut()
                        .expect("using declaration outside a disposal boundary")
                        .push(resource);
                }
            }
            Op::DisposeNormal => {
                let frame = disposal_frames
                    .pop()
                    .expect("normal disposal without an active frame");
                if frame.iter().any(|resource| resource.kind_is_async) {
                    return Ok(VmStep::Dispose {
                        frame,
                        completion: DisposeCompletion::Normal,
                    });
                }
                i.dispose_frame(frame, Ok(Value::Undefined))?;
            }
            Op::DisposeThrow => {
                let error = pop!();
                let frame = disposal_frames
                    .pop()
                    .expect("throw disposal without an active frame");
                if frame.iter().any(|resource| resource.kind_is_async) {
                    return Ok(VmStep::Dispose {
                        frame,
                        completion: DisposeCompletion::Throw(error),
                    });
                }
                return match i.dispose_frame(frame, Err(Abrupt::Throw(error))) {
                    Err(completion) => Err(completion),
                    Ok(_) => unreachable!("DisposeResources preserved a throw as normal"),
                };
            }
            Op::DisposeReturn => {
                let value = pop!();
                let frame = disposal_frames
                    .pop()
                    .expect("return disposal without an active frame");
                if frame.iter().any(|resource| resource.kind_is_async) {
                    return Ok(VmStep::Dispose {
                        frame,
                        completion: DisposeCompletion::SourceReturn(value),
                    });
                }
                return match i.dispose_frame(frame, Err(Abrupt::Return(value))) {
                    Err(Abrupt::Return(value)) => Ok(VmStep::Return(value)),
                    Err(completion) => Err(completion),
                    Ok(_) => unreachable!("DisposeResources preserved a return as normal"),
                };
            }
            Op::DisposeBareReturn => {
                let frame = disposal_frames
                    .pop()
                    .expect("bare-return disposal without an active frame");
                if frame.iter().any(|resource| resource.kind_is_async) {
                    return Ok(VmStep::Dispose {
                        frame,
                        completion: DisposeCompletion::BareReturn,
                    });
                }
                return match i.dispose_frame(frame, Err(Abrupt::Return(Value::Undefined))) {
                    Err(Abrupt::Return(_)) => Ok(VmStep::BareReturn),
                    Err(completion) => Err(completion),
                    Ok(_) => unreachable!("DisposeResources preserved a return as normal"),
                };
            }
            Op::DisposeResumeReturn => {
                let value = pop!();
                let frame = disposal_frames
                    .pop()
                    .expect("resumed-return disposal without an active frame");
                if frame.iter().any(|resource| resource.kind_is_async) {
                    return Ok(VmStep::Dispose {
                        frame,
                        completion: DisposeCompletion::ResumeReturn(value),
                    });
                }
                return match i.dispose_frame(frame, Err(Abrupt::Return(value))) {
                    Err(Abrupt::Return(value)) => Ok(VmStep::ResumeReturn(value)),
                    Err(completion) => Err(completion),
                    Ok(_) => unreachable!("DisposeResources preserved a return as normal"),
                };
            }
            Op::DisposeJump => {
                let handler_depth = match pop!() {
                    Value::Num(value) => value as usize,
                    _ => unreachable!("disposal jump depth is an internal integer"),
                };
                let target = match pop!() {
                    Value::Num(value) => value as usize,
                    _ => unreachable!("disposal jump target is an internal integer"),
                };
                let frame = disposal_frames
                    .pop()
                    .expect("jump disposal without an active frame");
                if frame.iter().any(|resource| resource.kind_is_async) {
                    return Ok(VmStep::Dispose {
                        frame,
                        completion: DisposeCompletion::Jump {
                            target,
                            handler_depth,
                        },
                    });
                }
                return match i.dispose_frame(frame, Err(Abrupt::Break(None, Value::Empty))) {
                    Err(Abrupt::Break(..)) | Err(Abrupt::Continue(..)) => Ok(VmStep::AbruptJump {
                        target,
                        handler_depth,
                    }),
                    Err(completion) => Err(completion),
                    Ok(_) => unreachable!("DisposeResources preserved a jump as normal"),
                };
            }
            Op::ObjectRest(count) => {
                let at = stack.len() - count as usize;
                let excluded_values = stack.split_off(at);
                let source = pop!();
                let mut excluded = Vec::with_capacity(excluded_values.len());
                for key in excluded_values {
                    match key {
                        Value::Str(key) => excluded.push(key.to_string()),
                        other => excluded.push(i.to_property_key(&other)?.into_string()),
                    }
                }
                let rest = i.copy_data_properties(&source, &excluded)?;
                stack.push(Value::Obj(rest));
            }
            Op::DeleteProp(n, strict) => {
                let base = pop!();
                let prop = &chunk.names[n as usize];
                if matches!(base, Value::Undefined | Value::Null) {
                    return Err(i.throw(
                        "TypeError",
                        format!("cannot delete property '{prop}' of null or undefined"),
                    ));
                }
                let v = i.delete_prop_with(base, prop, strict)?;
                stack.push(v);
            }
            Op::DeleteElem(strict) => {
                let idx = pop!();
                let base = pop!();
                let key = i.to_property_key(&idx)?;
                if matches!(base, Value::Undefined | Value::Null) {
                    return Err(i.throw(
                        "TypeError",
                        format!("cannot delete property '{key}' of null or undefined"),
                    ));
                }
                let v = i.delete_prop_with(base, &key, strict)?;
                stack.push(v);
            }
            Op::DeleteName(name) => {
                stack.push(i.delete_ident(&chunk.names[name as usize], env)?);
            }
            Op::DeleteSuper => {
                return Err(i.throw("ReferenceError", "cannot delete a super property"));
            }
            Op::CallSpread(argc) | Op::CallSpreadThis(argc) => {
                let spread = pop!();
                let at = stack.len() - (argc as usize - 1);
                let mut args: Vec<Value> = stack.split_off(at);
                let (it, nx) = i.get_iterator(&spread)?;
                while let Some(x) = i.iterator_step(&it, &nx)? {
                    args.push(x);
                }
                let callee = pop!();
                let this = if matches!(op, Op::CallSpreadThis(_)) {
                    pop!()
                } else {
                    Value::Undefined
                };
                let v = if chunk.feedback.detailed_enabled() {
                    call_profiled(i, chunk, op_pc, callee, this, &args)?
                } else {
                    i.call(callee, this, &args)?
                };
                stack.push(v);
            }
            Op::CallArgsArray | Op::CallArgsArrayThis => {
                let args = argument_array_values(i, pop!());
                let callee = pop!();
                let this = if matches!(op, Op::CallArgsArrayThis) {
                    pop!()
                } else {
                    Value::Undefined
                };
                let value = if chunk.feedback.detailed_enabled() {
                    call_profiled(i, chunk, op_pc, callee, this, &args)?
                } else {
                    i.call(callee, this, &args)?
                };
                stack.push(value);
            }
            Op::EvalCallArgsArray => {
                let args = argument_array_values(i, pop!());
                let callee = pop!();
                let receiver = pop!();
                let direct = matches!(receiver, Value::Undefined)
                    && matches!(
                        (&callee, &i.eval_fn),
                        (Value::Obj(function), Some(intrinsic))
                            if Rc::ptr_eq(function, intrinsic)
                    );
                let value = if direct {
                    if chunk.feedback.detailed_enabled() {
                        let target = i.call_target_kind(&callee);
                        let environment = i.call_environment_kind(&callee);
                        chunk.feedback.observe_call(
                            op_pc,
                            target,
                            call_arity_kind(args.len()),
                            environment,
                        );
                    }
                    let value = i.direct_eval(args.first(), env)?;
                    if chunk.feedback.detailed_enabled() {
                        if let Some(class) = arithmetic_value_class(&value) {
                            chunk.feedback.observe_value_class(
                                op_pc,
                                crate::feedback::ObservationRole::Result,
                                class,
                            );
                        }
                    }
                    value
                } else if chunk.feedback.detailed_enabled() {
                    call_profiled(i, chunk, op_pc, callee, receiver, &args)?
                } else {
                    i.call(callee, receiver, &args)?
                };
                stack.push(value);
            }
            Op::AppendProp(n, c) => {
                let v = pop!();
                let lval = pop!();
                let obj = pop!();
                let name = &chunk.names[n as usize];
                let lval = if let (Value::Str(x), Value::Obj(o)) = (&v, &obj) {
                    match i.append_prop_fast(o, name, lval, x) {
                        Ok(()) => continue,
                        Err(l) => l,
                    }
                } else {
                    lval
                };
                let r = i.binary("+", lval, v)?;
                set_named_property(i, chunk, op_pc, &obj, name, r, &chunk.caches[c as usize])?;
            }
            Op::GetElem => {
                let key = pop!();
                let obj = pop!();
                if chunk.feedback.detailed_enabled() {
                    let v = get_element_profiled(i, chunk, op_pc, &obj, &key)?;
                    stack.push(v);
                    continue;
                }
                if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                    if let Some(v) = i.fast_get_elem(o, *n) {
                        stack.push(v);
                        continue;
                    }
                }
                if matches!(obj, Value::Undefined | Value::Null) {
                    return Err(i.throw("TypeError", "cannot read property of null or undefined"));
                }
                let k = i.to_property_key(&key)?;
                let v = i.get_member(&obj, &k)?;
                stack.push(v);
            }
            Op::SetElem => {
                let v = pop!();
                let key = pop!();
                let obj = pop!();
                if chunk.feedback.detailed_enabled() {
                    let ret = v.clone();
                    set_element_profiled(i, chunk, op_pc, &obj, &key, v)?;
                    stack.push(ret);
                    continue;
                }
                if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                    let ret = v.clone();
                    match i.fast_set_elem(o, *n, v) {
                        Ok(()) => {
                            stack.push(ret);
                            continue;
                        }
                        Err(back) => {
                            let k = i.to_property_key(&key)?;
                            i.set_member(&obj, &k, back)?;
                            stack.push(ret);
                            continue;
                        }
                    }
                }
                let k = i.to_property_key(&key)?;
                i.set_member(&obj, &k, v.clone())?;
                stack.push(v);
            }
            Op::SetElemDrop => {
                let v = pop!();
                let key = pop!();
                let obj = pop!();
                if chunk.feedback.detailed_enabled() {
                    set_element_profiled(i, chunk, op_pc, &obj, &key, v)?;
                    continue;
                }
                if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                    match i.fast_set_elem(o, *n, v) {
                        Ok(()) => continue,
                        Err(back) => {
                            let k = i.to_property_key(&key)?;
                            i.set_member(&obj, &k, back)?;
                            continue;
                        }
                    }
                }
                let k = i.to_property_key(&key)?;
                i.set_member(&obj, &k, v)?;
            }
            Op::GetElemLocal(s) => {
                let key = pop!();
                if chunk.feedback.detailed_enabled() {
                    let obj = slots[s as usize].clone();
                    let v = get_element_profiled(i, chunk, op_pc, &obj, &key)?;
                    stack.push(v);
                    continue;
                }
                if let (Value::Obj(o), Value::Num(n)) = (&slots[s as usize], &key) {
                    if let Some(v) = i.fast_get_elem(o, *n) {
                        stack.push(v);
                        continue;
                    }
                }
                let obj = slots[s as usize].clone();
                if matches!(obj, Value::Undefined | Value::Null) {
                    return Err(i.throw("TypeError", "cannot read property of null or undefined"));
                }
                let k = i.to_property_key(&key)?;
                let v = i.get_member(&obj, &k)?;
                stack.push(v);
            }
            Op::SetElemLocal(s) | Op::SetElemLocalDrop(s) => {
                let keep = matches!(op, Op::SetElemLocal(_));
                let v = pop!();
                let key = pop!();
                if chunk.feedback.detailed_enabled() {
                    if keep {
                        stack.push(v.clone());
                    }
                    let obj = slots[s as usize].clone();
                    set_element_profiled(i, chunk, op_pc, &obj, &key, v)?;
                    continue;
                }
                if keep {
                    stack.push(v.clone());
                }
                if let (Value::Obj(o), Value::Num(n)) = (&slots[s as usize], &key) {
                    match i.fast_set_elem(o, *n, v) {
                        Ok(()) => continue,
                        Err(back) => {
                            let obj = slots[s as usize].clone();
                            let k = i.to_property_key(&key)?;
                            i.set_member(&obj, &k, back)?;
                            continue;
                        }
                    }
                }
                let obj = slots[s as usize].clone();
                let k = i.to_property_key(&key)?;
                i.set_member(&obj, &k, v)?;
            }
            Op::UpdateProp(n, c, kind) => {
                let obj = pop!();
                let name = &chunk.names[n as usize];
                let cache = &chunk.caches[c as usize];
                let old = get_named_property(i, chunk, op_pc, &obj, name, cache)?;
                step_and_store(i, stack, &chunk.feedback, op_pc, kind, old, |i, v| {
                    set_named_property(i, chunk, op_pc, &obj, name, v, cache)
                })?;
            }
            Op::UpdateElem(kind) => {
                let key = pop!();
                let obj = pop!();
                // Dense-element fast path: numeric key on a plain array/object.
                // Detailed collection uses the exact-PC shared tail below; ordinary execution
                // retains this allocation-free numeric path.
                if !chunk.feedback.detailed_enabled() {
                    if let (Value::Obj(o), Value::Num(nk)) = (&obj, &key) {
                        if let Some(Value::Num(old)) = i.fast_get_elem(o, *nk) {
                            let new = match kind {
                                UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard => {
                                    old + 1.0
                                }
                                UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard => {
                                    old - 1.0
                                }
                            };
                            if i.fast_set_elem(o, *nk, Value::Num(new)).is_ok() {
                                match kind {
                                    UpdKind::PreInc | UpdKind::PreDec => {
                                        stack.push(Value::Num(new))
                                    }
                                    UpdKind::PostInc | UpdKind::PostDec => {
                                        stack.push(Value::Num(old))
                                    }
                                    UpdKind::IncDiscard | UpdKind::DecDiscard => {}
                                }
                                continue;
                            }
                        }
                    }
                }
                if chunk.feedback.detailed_enabled() {
                    let old = get_element_profiled(i, chunk, op_pc, &obj, &key)?;
                    if let Some(v) = step_value(i, &chunk.feedback, op_pc, kind, old, |i, v| {
                        set_element_profiled(i, chunk, op_pc, &obj, &key, v)
                    })? {
                        stack.push(v);
                    }
                    continue;
                }
                // General path: nullish check, one ToPropertyKey, [[Get]], ToNumeric, [[Set]] —
                // the oracle's Reference order exactly.
                if matches!(obj, Value::Undefined | Value::Null) {
                    return Err(i.throw("TypeError", "cannot read property of null or undefined"));
                }
                let k = i.to_property_key(&key)?;
                let old = i.get_member(&obj, &k)?;
                step_and_store(i, stack, &chunk.feedback, op_pc, kind, old, |i, v| {
                    i.set_member(&obj, &k, v)
                })?;
            }
            Op::ToPropKeyLocal(s) => {
                if matches!(slots[s as usize], Value::Undefined | Value::Null) {
                    return Err(i.throw("TypeError", "cannot access property of null or undefined"));
                }
                match stack.last().expect("vm stack underflow") {
                    Value::Num(_) | Value::Str(_) => {}
                    _ => {
                        let key = pop!();
                        let k = i.to_property_key(&key)?;
                        stack.push(Value::str(k.into_string()));
                    }
                }
            }
            Op::ToPropKey => {
                if matches!(
                    stack.get(stack.len().saturating_sub(2)),
                    Some(Value::Undefined | Value::Null)
                ) {
                    return Err(i.throw("TypeError", "cannot access property of null or undefined"));
                }
                match stack.last().expect("vm stack underflow") {
                    // Side-effect-free and deterministic to coerce later; numbers stay numeric
                    // so GetElem/SetElem keep their dense fast path.
                    Value::Num(_) | Value::Str(_) => {}
                    _ => {
                        let key = pop!();
                        let k = i.to_property_key(&key)?;
                        stack.push(Value::str(k.into_string()));
                    }
                }
            }
            Op::Dup2 => {
                let len = stack.len();
                let a = stack[len - 2].clone();
                let b = stack[len - 1].clone();
                stack.push(a);
                stack.push(b);
            }
            Op::GetMethod(n, c) => {
                let obj = pop!();
                let m = get_named_property(
                    i,
                    chunk,
                    op_pc,
                    &obj,
                    &chunk.names[n as usize],
                    &chunk.caches[c as usize],
                )?;
                stack.push(obj);
                stack.push(m);
            }
            Op::GetMethodElem => {
                let key = pop!();
                let obj = pop!();
                if chunk.feedback.detailed_enabled() {
                    let m = get_element_profiled(i, chunk, op_pc, &obj, &key)?;
                    stack.push(obj);
                    stack.push(m);
                    continue;
                }
                let m = if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                    match i.fast_get_elem(o, *n) {
                        Some(v) => v,
                        None => {
                            let k = i.to_property_key(&key)?;
                            i.get_member(&obj, &k)?
                        }
                    }
                } else {
                    if matches!(obj, Value::Undefined | Value::Null) {
                        return Err(
                            i.throw("TypeError", "cannot read property of null or undefined")
                        );
                    }
                    let k = i.to_property_key(&key)?;
                    i.get_member(&obj, &k)?
                };
                stack.push(obj);
                stack.push(m);
            }
            Op::Add => bin_num(i, stack, &chunk.feedback, op_pc, "+", |a, b| a + b)?,
            Op::Sub => bin_num(i, stack, &chunk.feedback, op_pc, "-", |a, b| a - b)?,
            Op::Mul => bin_num(i, stack, &chunk.feedback, op_pc, "*", |a, b| a * b)?,
            Op::Div => bin_num(i, stack, &chunk.feedback, op_pc, "/", |a, b| a / b)?,
            Op::Mod => bin_num(i, stack, &chunk.feedback, op_pc, "%", crate::eval::js_mod)?,
            Op::BitAnd => bin_i32(i, stack, &chunk.feedback, op_pc, "&", |a, b| a & b)?,
            Op::BitOr => bin_i32(i, stack, &chunk.feedback, op_pc, "|", |a, b| a | b)?,
            Op::BitXor => bin_i32(i, stack, &chunk.feedback, op_pc, "^", |a, b| a ^ b)?,
            Op::Shl => bin_i32(i, stack, &chunk.feedback, op_pc, "<<", |a, b| {
                a.wrapping_shl(b as u32 & 31)
            })?,
            Op::Shr => bin_i32(i, stack, &chunk.feedback, op_pc, ">>", |a, b| {
                a >> (b as u32 & 31)
            })?,
            Op::UShr => bin_num(i, stack, &chunk.feedback, op_pc, ">>>", |a, b| {
                ((crate::eval::to_int32(a) as u32) >> (crate::eval::to_int32(b) as u32 & 31)) as f64
            })?,
            Op::Lt => bin_cmp(i, &mut *stack, "<", |a, b| a < b)?,
            Op::Gt => bin_cmp(i, &mut *stack, ">", |a, b| a > b)?,
            Op::Le => bin_cmp(i, &mut *stack, "<=", |a, b| a <= b)?,
            Op::Ge => bin_cmp(i, &mut *stack, ">=", |a, b| a >= b)?,
            Op::EqEq => bin_cmp(i, &mut *stack, "==", |a, b| a == b)?,
            Op::NotEq => bin_cmp(i, &mut *stack, "!=", |a, b| a != b)?,
            Op::StrictEq => bin_cmp(i, &mut *stack, "===", |a, b| a == b)?,
            Op::StrictNotEq => bin_cmp(i, &mut *stack, "!==", |a, b| a != b)?,
            Op::InstanceOf(c) => {
                let b = pop!();
                let a = pop!();
                let v = i.instanceof_ic(&a, &b, &chunk.caches[c as usize])?;
                stack.push(v);
            }
            Op::GenBin(n) => {
                let b = pop!();
                let a = pop!();
                let profiling = observe_arithmetic_operands(&chunk.feedback, op_pc, &a, &b);
                let v = i.binary(&chunk.names[n as usize], a, b)?;
                observe_arithmetic_result(&chunk.feedback, op_pc, profiling, &v);
                stack.push(v);
            }
            Op::Neg => {
                let a = pop!();
                let profiling = observe_arithmetic_operand(&chunk.feedback, op_pc, &a);
                let v = match a {
                    Value::Num(n) => Value::Num(-n),
                    other => i.eval_unary_vm("-", other)?,
                };
                observe_arithmetic_result(&chunk.feedback, op_pc, profiling, &v);
                stack.push(v);
            }
            Op::Plus => {
                let a = pop!();
                let profiling = observe_arithmetic_operand(&chunk.feedback, op_pc, &a);
                let v = match a {
                    Value::Num(n) => Value::Num(n),
                    other => i.eval_unary_vm("+", other)?,
                };
                observe_arithmetic_result(&chunk.feedback, op_pc, profiling, &v);
                stack.push(v);
            }
            Op::Not => {
                let a = pop!();
                stack.push(Value::Bool(!i.to_boolean(&a)));
            }
            Op::BitNot => {
                let a = pop!();
                let profiling = observe_arithmetic_operand(&chunk.feedback, op_pc, &a);
                let v = match a {
                    Value::Num(n) => Value::Num(!crate::eval::to_int32(n) as f64),
                    other => i.eval_unary_vm("~", other)?,
                };
                observe_arithmetic_result(&chunk.feedback, op_pc, profiling, &v);
                stack.push(v);
            }
            Op::Typeof => {
                let a = pop!();
                let v = i.eval_unary_vm("typeof", a)?;
                stack.push(v);
            }
            Op::TypeofName(n) => {
                stack.push(i.typeof_name_vm(&chunk.names[n as usize], env)?);
            }
            Op::Void => {
                pop!();
                stack.push(Value::Undefined);
            }
            Op::Jump(t) => {
                if t as usize <= *pc {
                    i.interrupt_poll()?;
                    if chunk.feedback.detailed_enabled() {
                        chunk.feedback.observe_loop_backedge(op_pc);
                    }
                }
                *pc = t as usize;
            }
            Op::InlineGuard(t, target) => {
                let it = &chunk.inline_targets[t as usize];
                let d = it.argc as usize + 1;
                let callee_ok = matches!(
                    &stack[stack.len() - d],
                    Value::Obj(o) if Rc::as_ptr(o) as usize == it.expected
                );
                let this_ok =
                    !it.check_this || matches!(&stack[stack.len() - d - 1], Value::Obj(_));
                let env_ok = it.expected_env == 0 || Rc::as_ptr(env) as usize == it.expected_env;
                if !(callee_ok && this_ok && env_ok) {
                    *pc = target as usize;
                }
            }
            Op::ResetSlots(start, count) => {
                for k in start as usize..start as usize + count as usize {
                    slots[k] = Value::Undefined;
                }
            }
            Op::JumpIfFalse(t) => {
                let a = pop!();
                let taken = !i.to_boolean(&a);
                if taken && t as usize <= *pc {
                    i.interrupt_poll()?;
                }
                if chunk.feedback.detailed_enabled() {
                    chunk.feedback.observe_branch(op_pc, taken);
                }
                if taken {
                    *pc = t as usize;
                }
            }
            Op::JumpIfFalsePeek(t) => {
                let taken = !i.to_boolean(stack.last().expect("vm stack underflow"));
                if taken && t as usize <= *pc {
                    i.interrupt_poll()?;
                }
                if chunk.feedback.detailed_enabled() {
                    chunk.feedback.observe_branch(op_pc, taken);
                }
                if taken {
                    *pc = t as usize;
                }
            }
            Op::JumpIfTruePeek(t) => {
                let taken = i.to_boolean(stack.last().expect("vm stack underflow"));
                if taken && t as usize <= *pc {
                    i.interrupt_poll()?;
                }
                if chunk.feedback.detailed_enabled() {
                    chunk.feedback.observe_branch(op_pc, taken);
                }
                if taken {
                    *pc = t as usize;
                }
            }
            Op::JumpIfNotNullishPeek(t) => {
                let taken = !matches!(
                    stack.last().expect("vm stack underflow"),
                    Value::Undefined | Value::Null
                );
                if taken && t as usize <= *pc {
                    i.interrupt_poll()?;
                }
                if chunk.feedback.detailed_enabled() {
                    chunk.feedback.observe_branch(op_pc, taken);
                }
                if taken {
                    *pc = t as usize;
                }
            }
            // Calls pass the argument window as a slice of the operand stack — no per-call `Vec`.
            // The callee/receiver slots below the window are cloned out first, then the whole
            // region is truncated away after the call. On a throw the stack is left long, which is
            // fine: the handler unwind (or function exit) truncates it.
            Op::Call(argc, _) => {
                let at = stack.len() - argc as usize;
                let callee = stack[at - 1].clone();
                let v = if chunk.feedback.detailed_enabled() {
                    call_profiled(i, chunk, op_pc, callee, Value::Undefined, &stack[at..])?
                } else {
                    i.call(callee, Value::Undefined, &stack[at..])?
                };
                stack.truncate(at - 1);
                stack.push(v);
            }
            Op::LoadNameForCall(n, c) => {
                // A depth-0 cache hit/fill can't have come through a `with` object: `this` is
                // undefined. Only the full walk can produce a with-object receiver.
                if let Some(v) = chunk
                    .name_ic_hit(i, env, c)
                    .or_else(|| chunk.name_ic_fill(i, env, n, c))
                {
                    stack.push(Value::Undefined);
                    stack.push(v);
                } else {
                    let (callee, with_this) = i.get_var_with(&chunk.names[n as usize], env)?;
                    stack.push(with_this.unwrap_or(Value::Undefined));
                    stack.push(callee);
                }
            }
            Op::CallWithThis(argc, _) => {
                let at = stack.len() - argc as usize;
                let m = stack[at - 1].clone();
                let this = stack[at - 2].clone();
                let v = if chunk.feedback.detailed_enabled() {
                    call_profiled(i, chunk, op_pc, m, this, &stack[at..])?
                } else {
                    i.call(m, this, &stack[at..])?
                };
                stack.truncate(at - 2);
                stack.push(v);
            }
            Op::New(argc, _) => {
                let at = stack.len() - argc as usize;
                let callee = stack[at - 1].clone();
                let v = if chunk.feedback.detailed_enabled() {
                    construct_profiled(i, chunk, op_pc, callee, &stack[at..])?
                } else {
                    i.construct(callee, &stack[at..])?
                };
                stack.truncate(at - 1);
                stack.push(v);
            }
            Op::NewArgsArray => {
                let args = argument_array_values(i, pop!());
                let callee = pop!();
                let value = if chunk.feedback.detailed_enabled() {
                    construct_profiled(i, chunk, op_pc, callee, &args)?
                } else {
                    i.construct(callee, &args)?
                };
                stack.push(value);
            }
            Op::MakeRegExp(body, flags) => {
                let body = &chunk.names[body as usize];
                let flags = &chunk.names[flags as usize];
                let value = i.make_regexp(body, flags)?;
                observe_allocation(
                    &chunk.feedback,
                    op_pc,
                    crate::feedback::AllocationObjectKind::RegExp,
                    body.len().saturating_add(flags.len()),
                );
                stack.push(value);
            }
            Op::MakeArray(n) => {
                let at = stack.len() - n as usize;
                let items: Vec<Value> = stack.split_off(at);
                let value = i.make_array(items);
                observe_allocation(
                    &chunk.feedback,
                    op_pc,
                    crate::feedback::AllocationObjectKind::Array,
                    n as usize,
                );
                stack.push(value);
            }
            Op::NewArray => {
                let value = i.make_array(Vec::new());
                observe_allocation(
                    &chunk.feedback,
                    op_pc,
                    crate::feedback::AllocationObjectKind::Array,
                    0,
                );
                stack.push(value);
            }
            Op::ArrayPush => {
                let value = pop!();
                let array = stack.last().expect("array builder missing").clone();
                array_literal_append(i, &array, Some(value))?;
            }
            Op::ArrayHole => {
                let array = stack.last().expect("array builder missing").clone();
                array_literal_append(i, &array, None)?;
            }
            Op::ArraySpread => {
                let spread = pop!();
                let array = stack.last().expect("array builder missing").clone();
                let (iterator, next) = i.get_iterator(&spread)?;
                while let Some(value) = i.iterator_step(&iterator, &next)? {
                    array_literal_append(i, &array, Some(value))?;
                }
            }
            Op::MakeObject(start, count, tidx) => {
                let at = stack.len() - count as usize;
                let values: Vec<Value> = stack.split_off(at);
                let keys = &chunk.names[start as usize..start as usize + count as usize];
                let v = if tidx != u32::MAX {
                    i.make_plain_object_templated(&chunk.obj_maps[tidx as usize], keys, values)
                } else {
                    i.make_plain_object_vm(keys, values)
                };
                observe_allocation(
                    &chunk.feedback,
                    op_pc,
                    crate::feedback::AllocationObjectKind::Object,
                    count as usize,
                );
                stack.push(v);
            }
            Op::NewObject => {
                let value = Value::Obj(i.new_object());
                observe_allocation(
                    &chunk.feedback,
                    op_pc,
                    crate::feedback::AllocationObjectKind::Object,
                    0,
                );
                stack.push(value);
            }
            Op::ObjectData(name_anonymous) => {
                let value = pop!();
                let key = pop!();
                let key = i.to_property_key(&key)?.into_string();
                if name_anonymous {
                    let name = i.fn_name_for_key(&key);
                    i.set_fn_name(&value, &name);
                }
                let Value::Obj(object) = stack.last().expect("object builder missing") else {
                    unreachable!("object literal builder retains an Object")
                };
                object
                    .borrow_mut()
                    .props
                    .insert(key, crate::value::Property::plain(value));
            }
            Op::ObjectSpread => {
                let source = pop!();
                let Value::Obj(object) = stack.last().expect("object builder missing") else {
                    unreachable!("object literal builder retains an Object")
                };
                i.copy_data_properties_into(object, &source, &[])?;
            }
            Op::ObjectProto => {
                let prototype = pop!();
                let Value::Obj(object) = stack.last().expect("object builder missing") else {
                    unreachable!("object literal builder retains an Object")
                };
                match prototype {
                    Value::Obj(prototype) => object.borrow_mut().proto = Some(prototype),
                    Value::Null => object.borrow_mut().proto = None,
                    _ => {}
                }
            }
            Op::ObjectMethod(function, kind) => {
                let key = pop!();
                let key = i.to_property_key(&key)?.into_string();
                let Value::Obj(object) = stack.last().expect("object builder missing") else {
                    unreachable!("object literal builder retains an Object")
                };
                let home_env = crate::interpreter::new_scope(Some(env.clone()));
                crate::eval::bind(&home_env, "%homeobject%", Value::Obj(object.clone()));
                let value = i.make_function(chunk.funcs[function as usize].clone(), home_env);
                let name = i.fn_name_for_key(&key);
                match kind {
                    0 => {
                        i.set_fn_name(&value, &name);
                        object
                            .borrow_mut()
                            .props
                            .insert(key, crate::value::Property::plain(value));
                    }
                    1 => {
                        i.set_fn_name(&value, &format!("get {name}"));
                        i.define_accessor(object, &key, Some(value), None);
                    }
                    2 => {
                        i.set_fn_name(&value, &format!("set {name}"));
                        i.define_accessor(object, &key, None, Some(value));
                    }
                    _ => unreachable!("object method kind is compiler-internal"),
                }
            }
            Op::ImportMeta => stack.push(i.import_meta_vm(env)),
            Op::NewTarget => stack.push(i.new_target_vm(env)),
            Op::DynamicImport(phase, has_options) => {
                let options = has_options.then(|| pop!());
                let specifier = pop!();
                stack.push(i.import_call_vm(specifier, options, phase, env)?);
            }
            Op::PrivateIn(name) => {
                let value = pop!();
                stack.push(i.private_in_vm(&chunk.names[name as usize], value, env)?);
            }
            Op::GetPrivate(name) => {
                let base = pop!();
                stack.push(i.private_get_vm(&chunk.names[name as usize], &base, env)?);
            }
            Op::GetPrivateKeep(name) => {
                let base = pop!();
                let value = i.private_get_vm(&chunk.names[name as usize], &base, env)?;
                stack.push(base);
                stack.push(value);
            }
            Op::GetPrivateMethod(name) => {
                let base = pop!();
                let method = i.private_get_vm(&chunk.names[name as usize], &base, env)?;
                stack.push(base);
                stack.push(method);
            }
            Op::SetPrivate(name) => {
                let value = pop!();
                let base = pop!();
                i.private_set_vm(&chunk.names[name as usize], &base, value.clone(), env)?;
                stack.push(value);
            }
            Op::UpdatePrivate(name, kind) => {
                let base = pop!();
                let old = i.private_get_vm(&chunk.names[name as usize], &base, env)?;
                if let Some(value) =
                    step_value(i, &chunk.feedback, op_pc, kind, old, |i, value| {
                        i.private_set_vm(&chunk.names[name as usize], &base, value, env)
                    })?
                {
                    stack.push(value);
                }
            }
            Op::SuperCallStart => {
                let (new_target, super_constructor) = i.prepare_super_call(env)?;
                stack.push(new_target);
                stack.push(super_constructor);
            }
            Op::SuperCallArgsArray => {
                let args = argument_array_values(i, pop!());
                let super_constructor = pop!();
                let new_target = pop!();
                let value = if chunk.feedback.detailed_enabled() {
                    let target = i.call_target_kind(&super_constructor);
                    let environment = i.call_environment_kind(&super_constructor);
                    chunk.feedback.observe_call(
                        op_pc,
                        target,
                        call_arity_kind(args.len()),
                        environment,
                    );
                    let value = i.finish_super_call(new_target, super_constructor, &args, env)?;
                    if let Some(class) = arithmetic_value_class(&value) {
                        chunk.feedback.observe_value_class(
                            op_pc,
                            crate::feedback::ObservationRole::Result,
                            class,
                        );
                    }
                    value
                } else {
                    i.finish_super_call(new_target, super_constructor, &args, env)?
                };
                stack.push(value);
            }
            // ECMA-262 §13.3.7.1 creates a Super Reference with the current function's actual
            // this binding as [[ThisValue]]. Compiled calls keep that binding in the VM frame
            // rather than materializing an activation environment, so use it directly here.
            Op::SuperThis => stack.push(this_val.clone()),
            Op::SuperBase => stack.push(i.super_base_vm(env)?),
            Op::SuperGet | Op::SuperGetMethod => {
                let keep_receiver = matches!(op, Op::SuperGetMethod);
                let base = pop!();
                let key = pop!();
                let receiver = pop!();
                let value = i.super_get_vm(&base, receiver.clone(), key)?;
                if keep_receiver {
                    stack.push(receiver);
                }
                stack.push(value);
            }
            Op::SuperGetKeep => {
                let base = pop!();
                let key = pop!();
                let receiver = pop!();
                if matches!(base, Value::Null | Value::Undefined) {
                    return Err(i.throw("TypeError", "cannot read property of null super base"));
                }
                let key = i.to_property_key(&key)?.into_string();
                let value = i.get_member_recv(&base, &key, receiver.clone())?;
                stack.push(receiver);
                stack.push(Value::from_string(key));
                stack.push(base);
                stack.push(value);
            }
            Op::SuperSet => {
                let value = pop!();
                let base = pop!();
                let key = pop!();
                let receiver = pop!();
                i.super_set_vm(&base, receiver, key, value.clone())?;
                stack.push(value);
            }
            Op::SuperUpdate(kind) => {
                let base = pop!();
                let key = pop!();
                let receiver = pop!();
                if matches!(base, Value::Null | Value::Undefined) {
                    return Err(i.throw("TypeError", "cannot read property of null super base"));
                }
                let key = i.to_property_key(&key)?.into_string();
                let old = i.get_member_recv(&base, &key, receiver.clone())?;
                if let Some(value) =
                    step_value(i, &chunk.feedback, op_pc, kind, old, |i, value| {
                        i.set_member_recv(&base, &key, value, receiver.clone())
                            .map(|_| ())
                    })?
                {
                    stack.push(value);
                }
            }
            Op::TemplateObject(site) => {
                let (site_id, quasis) = &chunk.templates[site as usize];
                stack.push(i.template_object(*site_id, quasis)?);
            }
            Op::RequireCallable => {
                if !stack
                    .last()
                    .expect("tagged-template callee missing")
                    .is_callable()
                {
                    return Err(i.throw("TypeError", "tag is not a function"));
                }
            }
            Op::ToStr => {
                let v = pop!();
                let s = i.to_string(&v)?;
                stack.push(Value::Str(s));
            }
            Op::GetIter => {
                let v = pop!();
                let (it, nx) = i.get_iterator(&v)?;
                stack.push(it);
                stack.push(nx);
            }
            Op::GetAsyncIter => {
                let v = pop!();
                let opened = VmDelegate::open(i, v, true)?;
                stack.push(opened.iterator);
                stack.push(opened.next);
                stack.push(Value::Bool(opened.from_sync));
            }
            Op::ForInKeys => {
                let source = pop!();
                let keys = i
                    .for_in_keys(&source)?
                    .into_iter()
                    .map(Value::from_string)
                    .collect();
                stack.push(i.make_array(keys));
            }
            Op::ForInStepL(keys, index, source) => {
                if let Some(key) = for_in_step(i, slots, keys, index, source)? {
                    stack.push(key);
                    stack.push(Value::Bool(true));
                } else {
                    stack.push(Value::Undefined);
                    stack.push(Value::Bool(false));
                }
            }
            Op::IterStepL(is, ns) => {
                let it = slots[is as usize].clone();
                let nx = slots[ns as usize].clone();
                match i.iterator_step(&it, &nx)? {
                    Some(v) => {
                        stack.push(v);
                        stack.push(Value::Bool(true));
                    }
                    None => {
                        stack.push(Value::Undefined);
                        stack.push(Value::Bool(false));
                    }
                }
            }
            Op::IterCloseL(s) => {
                let it = slots[s as usize].clone();
                i.iterator_close_normal(&it)?;
            }
            Op::IterAbortL(s) => {
                let exc = pop!();
                let it = slots[s as usize].clone();
                i.iterator_close(&it);
                return Err(Abrupt::Throw(exc));
            }
            Op::DestructureStepL(iter_s, next_s, done_s) => {
                if matches!(slots[done_s as usize], Value::Bool(true)) {
                    stack.push(Value::Undefined);
                    continue;
                }
                // IteratorDestructuringAssignmentEvaluation sets [[Done]] before propagating an
                // abrupt IteratorStep. Restore false only after obtaining a live value.
                slots[done_s as usize] = Value::Bool(true);
                let iterator = slots[iter_s as usize].clone();
                let next = slots[next_s as usize].clone();
                match i.iterator_step(&iterator, &next)? {
                    Some(value) => {
                        slots[done_s as usize] = Value::Bool(false);
                        stack.push(value);
                    }
                    None => stack.push(Value::Undefined),
                }
            }
            Op::DestructureRestL(iter_s, next_s, done_s) => {
                let mut values = Vec::new();
                while !matches!(slots[done_s as usize], Value::Bool(true)) {
                    slots[done_s as usize] = Value::Bool(true);
                    let iterator = slots[iter_s as usize].clone();
                    let next = slots[next_s as usize].clone();
                    match i.iterator_step(&iterator, &next)? {
                        Some(value) => {
                            slots[done_s as usize] = Value::Bool(false);
                            values.push(value);
                        }
                        None => break,
                    }
                }
                stack.push(i.make_array(values));
            }
            Op::IterCloseIfNotDoneL(iter_s, done_s) => {
                if !matches!(slots[done_s as usize], Value::Bool(true)) {
                    slots[done_s as usize] = Value::Bool(true);
                    let iterator = slots[iter_s as usize].clone();
                    i.iterator_close_normal(&iterator)?;
                }
            }
            Op::IterAbortIfNotDoneL(iter_s, done_s) => {
                let error = pop!();
                if !matches!(slots[done_s as usize], Value::Bool(true)) {
                    slots[done_s as usize] = Value::Bool(true);
                    let iterator = slots[iter_s as usize].clone();
                    i.iterator_close(&iterator);
                }
                return Err(Abrupt::Throw(error));
            }
            Op::AsyncIterStepL(iter_s, next_s, from_sync_s, done_s) => {
                let iterator = slots[iter_s as usize].clone();
                let next = slots[next_s as usize].clone();
                let from_sync = matches!(slots[from_sync_s as usize], Value::Bool(true));
                let result = match i.call(next, iterator.clone(), &[]) {
                    Ok(result) => result,
                    Err(error) if from_sync => {
                        return Ok(VmStep::Await(async_from_sync_abrupt_promise(i, error)?));
                    }
                    Err(error) => return Err(error),
                };
                if !from_sync {
                    return Ok(VmStep::Await(result));
                }
                if !matches!(result, Value::Obj(_)) {
                    let error = i.make_error("TypeError", "iterator result is not an object");
                    return Ok(VmStep::Await(rejected_intrinsic_promise(i, error)));
                }
                let done = match i.get_member(&result, "done") {
                    Ok(done) => done,
                    Err(error) => {
                        return Ok(VmStep::Await(async_from_sync_abrupt_promise(i, error)?));
                    }
                };
                let done = i.to_boolean(&done);
                let value = match i.get_member(&result, "value") {
                    Ok(value) => value,
                    Err(error) => {
                        return Ok(VmStep::Await(async_from_sync_abrupt_promise(i, error)?));
                    }
                };
                slots[done_s as usize] = Value::Bool(done);
                // AsyncFromSyncIteratorContinuation resolves and chains the value into the
                // adapter promise before the loop's Await. A live value rejection closes the
                // underlying sync iterator in that reaction job, not one Await job later.
                let wrapper = match i.promise_resolve_checked(value) {
                    Ok(promise) => {
                        let on_rejected = if done {
                            Value::Undefined
                        } else {
                            crate::builtins::make_bound_len(
                                i,
                                async_from_sync_close_rejection,
                                vec![iterator],
                                1.0,
                            )
                        };
                        i.promise_then(&promise, Value::Undefined, on_rejected)
                    }
                    Err(error) => {
                        if !done {
                            i.iterator_close(&iterator);
                        }
                        rejected_intrinsic_promise(i, error)
                    }
                };
                return Ok(VmStep::Await(wrapper));
            }
            Op::AsyncIterResumeL(from_sync_s, done_s) => {
                let settled = pop!();
                let from_sync = matches!(slots[from_sync_s as usize], Value::Bool(true));
                let (done, value) = if from_sync {
                    let done = matches!(slots[done_s as usize], Value::Bool(true));
                    (done, if done { Value::Undefined } else { settled })
                } else {
                    if !matches!(settled, Value::Obj(_)) {
                        return Err(i.throw("TypeError", "iterator result is not an object"));
                    }
                    let done = i.get_member(&settled, "done")?;
                    let done = i.to_boolean(&done);
                    let value = if done {
                        Value::Undefined
                    } else {
                        i.get_member(&settled, "value")?
                    };
                    (done, value)
                };
                stack.push(value);
                stack.push(Value::Bool(!done));
            }
            Op::AsyncIterCloseL(iter_s, from_sync_s, swallow_error) => {
                return Ok(VmStep::AsyncClose {
                    iterator: slots[iter_s as usize].clone(),
                    from_sync: matches!(slots[from_sync_s as usize], Value::Bool(true)),
                    swallow_error,
                });
            }
            Op::Throw => {
                let v = pop!();
                return Err(Abrupt::Throw(v));
            }
            Op::AbruptJump(target, handler_depth) => {
                return Ok(VmStep::AbruptJump {
                    target: target as usize,
                    handler_depth: handler_depth as usize,
                });
            }
            Op::Return => return Ok(VmStep::Return(pop!())),
            Op::ReturnBare => return Ok(VmStep::BareReturn),
            Op::ResumeReturn => return Ok(VmStep::ResumeReturn(pop!())),
            Op::ResumeJump => {
                let handler_depth = match pop!() {
                    Value::Num(value) => value as usize,
                    _ => unreachable!("finally jump depth is an internal integer"),
                };
                let target = match pop!() {
                    Value::Num(value) => value as usize,
                    _ => unreachable!("finally jump target is an internal integer"),
                };
                return Ok(VmStep::AbruptJump {
                    target,
                    handler_depth,
                });
            }
            Op::ReturnUndef => return Ok(VmStep::Done(Value::Undefined)),
            Op::Await => return Ok(VmStep::Await(pop!())),
            Op::Yield => return Ok(VmStep::Yield(pop!())),
            Op::YieldStar => return Ok(VmStep::YieldStar(pop!())),
            Op::PushHandler(throw_pc) => handlers.push(Handler {
                target: HandlerTarget::Catch {
                    throw_pc: throw_pc as usize,
                },
                stack_depth: stack.len(),
            }),
            Op::PushFinally(throw_pc, return_pc, bare_return_pc, resume_return_pc, jump_pc) => {
                handlers.push(Handler {
                    target: HandlerTarget::Finally {
                        throw_pc: throw_pc as usize,
                        return_pc: return_pc as usize,
                        bare_return_pc: bare_return_pc as usize,
                        resume_return_pc: resume_return_pc as usize,
                        jump_pc: jump_pc as usize,
                    },
                    stack_depth: stack.len(),
                })
            }
            Op::PushIterator(throw_pc, return_pc, bare_return_pc, resume_return_pc) => handlers
                .push(Handler {
                    target: HandlerTarget::Iterator {
                        throw_pc: throw_pc as usize,
                        return_pc: return_pc as usize,
                        bare_return_pc: bare_return_pc as usize,
                        resume_return_pc: resume_return_pc as usize,
                    },
                    stack_depth: stack.len(),
                }),
            Op::PopHandler => {
                handlers.pop();
            }
        }
    }
}

#[derive(Clone, Copy)]
enum DelegateCompletion {
    Normal,
    Return,
}

enum DelegateStage {
    /// Waiting for the completion produced by the outer generator's next/throw/return method.
    Ready,
    /// An async iterator method result is being awaited.
    AwaitResult(DelegateCompletion),
    /// An async-from-sync iterator result's value is being awaited.
    AwaitValue {
        completion: DelegateCompletion,
        done: bool,
    },
    /// AsyncIteratorClose after a missing delegated `throw` is awaiting `return()`.
    AwaitCloseResult,
    /// The async-from-sync close result's value is being awaited.
    AwaitCloseValue,
    /// A delegated async iterator has no `return`; YieldExpression performs its own Await of the
    /// already-unwrapped outer return value before propagating the return completion.
    AwaitMissingReturn,
}

struct VmDelegate {
    iterator: Value,
    next: Value,
    from_sync: bool,
    async_mode: bool,
    stage: DelegateStage,
}

enum DelegateAction {
    Suspend(crate::coroutine::Suspend),
    Continue(Value),
    Return(Value),
    Throw(Value),
    Terminate,
}

struct VmDispose {
    resources: Vec<crate::interpreter::Disposable>,
    output: DisposeCompletion,
    needs_await: bool,
    has_awaited: bool,
    /// A synchronous resource deferred until the await-only marker immediately above it has
    /// yielded one job turn.
    pending_resource: Option<crate::interpreter::Disposable>,
}

enum DisposeAction {
    Suspend(Value),
    Complete(DisposeCompletion),
    Terminate,
}

impl VmDispose {
    fn new(resources: Vec<crate::interpreter::Disposable>, output: DisposeCompletion) -> VmDispose {
        VmDispose {
            resources,
            output,
            needs_await: false,
            has_awaited: false,
            pending_resource: None,
        }
    }

    fn record_error(&mut self, i: &mut Interp, error: Value) {
        let previous = std::mem::replace(&mut self.output, DisposeCompletion::Normal);
        self.output = match previous {
            DisposeCompletion::Throw(suppressed) => {
                DisposeCompletion::Throw(i.make_suppressed(error, suppressed))
            }
            _ => DisposeCompletion::Throw(error),
        };
    }

    fn call_failure(&mut self, i: &mut Interp, error: Abrupt) -> Option<DisposeAction> {
        match error {
            Abrupt::Throw(error) => {
                self.record_error(i, error);
                None
            }
            Abrupt::Interrupt(_) => Some(DisposeAction::Terminate),
            // Call's function boundary consumes these completion kinds. Keep the defensive path
            // non-panicking if a host callable violates that contract.
            Abrupt::Return(value) | Abrupt::Break(_, value) | Abrupt::Continue(_, value) => {
                self.record_error(i, value);
                None
            }
        }
    }

    fn advance(&mut self, i: &mut Interp) -> DisposeAction {
        loop {
            let resource = match self
                .pending_resource
                .take()
                .or_else(|| self.resources.pop())
            {
                Some(resource) => resource,
                None => {
                    if self.needs_await && !self.has_awaited {
                        self.needs_await = false;
                        return DisposeAction::Suspend(Value::Undefined);
                    }
                    return DisposeAction::Complete(std::mem::replace(
                        &mut self.output,
                        DisposeCompletion::Normal,
                    ));
                }
            };

            if !resource.kind_is_async && self.needs_await && !self.has_awaited {
                self.needs_await = false;
                self.pending_resource = Some(resource);
                return DisposeAction::Suspend(Value::Undefined);
            }
            if !resource.method.is_callable() {
                debug_assert!(resource.kind_is_async);
                self.needs_await = true;
                continue;
            }

            let called = i.call(resource.method, resource.value, &[]);
            if !resource.kind_is_async {
                if let Err(error) = called {
                    if let Some(action) = self.call_failure(i, error) {
                        return action;
                    }
                }
                continue;
            }

            if resource.method_is_async {
                match called {
                    Ok(value) => {
                        self.has_awaited = true;
                        return DisposeAction::Suspend(value);
                    }
                    Err(error) => {
                        if let Some(action) = self.call_failure(i, error) {
                            return action;
                        }
                        continue;
                    }
                }
            }

            // The sync fallback is wrapped into an intrinsic async disposer. Its return value is
            // ignored; a synchronous throw rejects the wrapper promise before Await observes it.
            let promise = i.new_promise();
            match called {
                Ok(_) => i.resolve_promise(&promise, Value::Undefined),
                Err(Abrupt::Throw(error)) => i.reject_promise(&promise, error),
                Err(error) => {
                    if let Some(action) = self.call_failure(i, error) {
                        return action;
                    }
                    continue;
                }
            }
            self.has_awaited = true;
            return DisposeAction::Suspend(promise);
        }
    }

    fn resume(&mut self, i: &mut Interp, signal: crate::coroutine::Resume) -> DisposeAction {
        match signal {
            crate::coroutine::Resume::Next(_) => {}
            crate::coroutine::Resume::Throw(error) => self.record_error(i, error),
            crate::coroutine::Resume::Return(value) => {
                self.output = DisposeCompletion::ResumeReturn(value)
            }
            crate::coroutine::Resume::Terminate => return DisposeAction::Terminate,
        }
        self.advance(i)
    }
}

fn pending_dispose_completion(completion: DisposeCompletion) -> Option<PendingCompletion> {
    match completion {
        DisposeCompletion::Normal => None,
        DisposeCompletion::Throw(error) => Some(PendingCompletion::Throw(error)),
        DisposeCompletion::SourceReturn(value) => Some(PendingCompletion::SourceReturn(value)),
        DisposeCompletion::BareReturn => Some(PendingCompletion::BareReturn),
        DisposeCompletion::ResumeReturn(value) => Some(PendingCompletion::ResumeReturn(value)),
        DisposeCompletion::Jump {
            target,
            handler_depth,
        } => Some(PendingCompletion::Jump {
            target,
            handler_depth,
        }),
    }
}

enum AsyncCloseStage {
    AwaitResult,
    AwaitValue,
}

struct VmAsyncClose {
    /// Keep the iterator alive while an awaited close reaction is pending.
    _iterator: Value,
    swallow_error: bool,
    stage: AsyncCloseStage,
}

enum AsyncCloseAction {
    Suspend(crate::coroutine::Suspend),
    Continue,
    Throw(Value),
    Terminate,
}

impl VmAsyncClose {
    fn failure(swallow_error: bool, error: Abrupt) -> AsyncCloseAction {
        match error {
            Abrupt::Throw(_) if swallow_error => AsyncCloseAction::Continue,
            Abrupt::Throw(error) => AsyncCloseAction::Throw(error),
            Abrupt::Interrupt(_) => AsyncCloseAction::Terminate,
            Abrupt::Return(value) => AsyncCloseAction::Throw(value),
            Abrupt::Break(_, value) | Abrupt::Continue(_, value) => AsyncCloseAction::Throw(value),
        }
    }

    fn start(
        i: &mut Interp,
        iterator: Value,
        from_sync: bool,
        swallow_error: bool,
    ) -> (Option<VmAsyncClose>, AsyncCloseAction) {
        use crate::coroutine::Suspend;

        // CreateAsyncFromSyncIterator's `return` is an intrinsic method which always returns a
        // promise. Even an absent underlying `return`, or a getter/call/protocol error, therefore
        // reaches AsyncIteratorClose through Await. Preserve that mandatory job boundary instead
        // of directly invoking the sync close as though this were IteratorClose.
        if from_sync {
            return Self::start_from_sync(i, iterator, swallow_error);
        }

        let ret = match i.get_member(&iterator, "return") {
            Ok(ret) => ret,
            Err(error) => return (None, Self::failure(swallow_error, error)),
        };
        if matches!(ret, Value::Undefined | Value::Null) {
            return (None, AsyncCloseAction::Continue);
        }
        if !ret.is_callable() {
            let error = i.throw("TypeError", "iterator 'return' is not callable");
            return (None, Self::failure(swallow_error, error));
        }
        let result = match i.call(ret, iterator.clone(), &[]) {
            Ok(result) => result,
            Err(error) => return (None, Self::failure(swallow_error, error)),
        };
        (
            Some(VmAsyncClose {
                _iterator: iterator,
                swallow_error,
                stage: AsyncCloseStage::AwaitResult,
            }),
            AsyncCloseAction::Suspend(Suspend::Await(result)),
        )
    }

    fn suspend_from_sync(
        iterator: Value,
        swallow_error: bool,
        stage: AsyncCloseStage,
        promise: Value,
    ) -> (Option<VmAsyncClose>, AsyncCloseAction) {
        (
            Some(VmAsyncClose {
                _iterator: iterator,
                swallow_error,
                stage,
            }),
            AsyncCloseAction::Suspend(crate::coroutine::Suspend::Await(promise)),
        )
    }

    fn reject_from_sync(
        i: &mut Interp,
        iterator: Value,
        swallow_error: bool,
        error: Abrupt,
    ) -> (Option<VmAsyncClose>, AsyncCloseAction) {
        match error {
            Abrupt::Throw(error) => Self::suspend_from_sync(
                iterator,
                swallow_error,
                AsyncCloseStage::AwaitResult,
                rejected_intrinsic_promise(i, error),
            ),
            other => (None, Self::failure(swallow_error, other)),
        }
    }

    fn start_from_sync(
        i: &mut Interp,
        iterator: Value,
        swallow_error: bool,
    ) -> (Option<VmAsyncClose>, AsyncCloseAction) {
        let ret = match i.get_member(&iterator, "return") {
            Ok(ret) => ret,
            Err(error) => {
                return Self::reject_from_sync(i, iterator, swallow_error, error);
            }
        };
        if matches!(ret, Value::Undefined | Value::Null) {
            let promise = i.new_promise();
            let result = i.iter_result_obj(Value::Undefined, true);
            i.resolve_promise(&promise, result);
            return Self::suspend_from_sync(
                iterator,
                swallow_error,
                AsyncCloseStage::AwaitResult,
                promise,
            );
        }
        if !ret.is_callable() {
            let error =
                Abrupt::Throw(i.make_error("TypeError", "iterator 'return' is not callable"));
            return Self::reject_from_sync(i, iterator, swallow_error, error);
        }
        let result = match i.call(ret, iterator.clone(), &[]) {
            Ok(result) => result,
            Err(error) => {
                return Self::reject_from_sync(i, iterator, swallow_error, error);
            }
        };
        if !matches!(result, Value::Obj(_)) {
            let error =
                Abrupt::Throw(i.make_error("TypeError", "iterator result is not an object"));
            return Self::reject_from_sync(i, iterator, swallow_error, error);
        }
        // AsyncFromSyncIteratorContinuation observes `done` before `value`, even though
        // AsyncIteratorClose only uses the repackaged result's object-ness.
        if let Err(error) = i.get_member(&result, "done") {
            return Self::reject_from_sync(i, iterator, swallow_error, error);
        }
        let value = match i.get_member(&result, "value") {
            Ok(value) => value,
            Err(error) => {
                return Self::reject_from_sync(i, iterator, swallow_error, error);
            }
        };
        let wrapper = match i.promise_resolve_checked(value) {
            Ok(promise) => i.promise_then(&promise, Value::Undefined, Value::Undefined),
            Err(error) => rejected_intrinsic_promise(i, error),
        };
        Self::suspend_from_sync(
            iterator,
            swallow_error,
            AsyncCloseStage::AwaitValue,
            wrapper,
        )
    }

    fn resume(&mut self, i: &mut Interp, signal: crate::coroutine::Resume) -> AsyncCloseAction {
        use crate::coroutine::Resume;
        match signal {
            Resume::Next(result) => match self.stage {
                AsyncCloseStage::AwaitResult => {
                    if matches!(result, Value::Obj(_)) {
                        AsyncCloseAction::Continue
                    } else {
                        Self::failure(
                            self.swallow_error,
                            i.throw("TypeError", "iterator result is not an object"),
                        )
                    }
                }
                AsyncCloseStage::AwaitValue => AsyncCloseAction::Continue,
            },
            Resume::Throw(error) => Self::failure(self.swallow_error, Abrupt::Throw(error)),
            Resume::Return(error) => Self::failure(self.swallow_error, Abrupt::Throw(error)),
            Resume::Terminate => AsyncCloseAction::Terminate,
        }
    }
}

impl VmDelegate {
    /// GetIterator(value, generator kind). Async generators prefer @@asyncIterator and otherwise
    /// retain the sync iterator plus an explicit async-from-sync continuation state.
    fn open(i: &mut Interp, value: Value, async_mode: bool) -> Result<Self, Abrupt> {
        let (iterator, next, from_sync) = if async_mode {
            let key = crate::builtins::async_iterator_key(i);
            let method = match key {
                Some(key) => i.get_member(&value, &key)?,
                None => Value::Undefined,
            };
            if !matches!(method, Value::Undefined | Value::Null) && !method.is_callable() {
                return Err(i.throw("TypeError", "@@asyncIterator is not callable"));
            }
            if method.is_callable() {
                let iterator = i.call(method, value, &[])?;
                if !matches!(iterator, Value::Obj(_)) {
                    return Err(i.throw("TypeError", "@@asyncIterator did not return an object"));
                }
                let next = i.get_member(&iterator, "next")?;
                (iterator, next, false)
            } else {
                let (iterator, next) = i.get_iterator(&value)?;
                (iterator, next, true)
            }
        } else {
            let (iterator, next) = i.get_iterator(&value)?;
            (iterator, next, false)
        };
        Ok(Self {
            iterator,
            next,
            from_sync,
            async_mode,
            stage: DelegateStage::Ready,
        })
    }

    fn abrupt(error: Abrupt) -> DelegateAction {
        match error {
            Abrupt::Throw(value) => DelegateAction::Throw(value),
            Abrupt::Return(value) => DelegateAction::Return(value),
            Abrupt::Interrupt(_) => DelegateAction::Terminate,
            Abrupt::Break(_, _) | Abrupt::Continue(_, _) => DelegateAction::Terminate,
        }
    }

    fn type_error(i: &mut Interp, message: &str) -> DelegateAction {
        DelegateAction::Throw(i.make_error("TypeError", message))
    }

    /// Consume either an outer resumption completion or an internal async-await reaction.
    fn resume(&mut self, i: &mut Interp, signal: crate::coroutine::Resume) -> DelegateAction {
        use crate::coroutine::{Resume, Suspend};
        let stage = std::mem::replace(&mut self.stage, DelegateStage::Ready);
        match stage {
            DelegateStage::Ready => {
                let (method, argument, completion) = match signal {
                    Resume::Next(value) => (self.next.clone(), value, DelegateCompletion::Normal),
                    Resume::Throw(error) => {
                        let method = match i.get_member(&self.iterator, "throw") {
                            Ok(method) => method,
                            Err(error) => return Self::abrupt(error),
                        };
                        if matches!(method, Value::Undefined | Value::Null) {
                            if !self.async_mode {
                                if let Err(error) = i.iterator_close_normal(&self.iterator) {
                                    return Self::abrupt(error);
                                }
                                return Self::type_error(
                                    i,
                                    "the delegated iterator has no 'throw' method",
                                );
                            }
                            // AsyncIteratorClose(iteratorRecord, NormalCompletion(empty)) before
                            // the mandated protocol TypeError.
                            let ret = match i.get_member(&self.iterator, "return") {
                                Ok(ret) => ret,
                                Err(error) => return Self::abrupt(error),
                            };
                            if matches!(ret, Value::Undefined | Value::Null) {
                                return Self::type_error(
                                    i,
                                    "the delegated iterator has no 'throw' method",
                                );
                            }
                            if !ret.is_callable() {
                                return Self::type_error(i, "iterator 'return' is not callable");
                            }
                            let result = match i.call(ret, self.iterator.clone(), &[]) {
                                Ok(result) => result,
                                Err(error) => return Self::abrupt(error),
                            };
                            self.stage = DelegateStage::AwaitCloseResult;
                            return DelegateAction::Suspend(Suspend::Await(result));
                        }
                        if !method.is_callable() {
                            return Self::type_error(i, "iterator 'throw' is not callable");
                        }
                        (method, error, DelegateCompletion::Normal)
                    }
                    Resume::Return(value) => {
                        let method = match i.get_member(&self.iterator, "return") {
                            Ok(method) => method,
                            Err(error) => return Self::abrupt(error),
                        };
                        if matches!(method, Value::Undefined | Value::Null) {
                            if self.async_mode {
                                self.stage = DelegateStage::AwaitMissingReturn;
                                return DelegateAction::Suspend(Suspend::Await(value));
                            }
                            return DelegateAction::Return(value);
                        }
                        if !method.is_callable() {
                            return Self::type_error(i, "iterator 'return' is not callable");
                        }
                        (method, value, DelegateCompletion::Return)
                    }
                    Resume::Terminate => return DelegateAction::Terminate,
                };
                let result = match i.call(method, self.iterator.clone(), &[argument]) {
                    Ok(result) => result,
                    Err(error) => return Self::abrupt(error),
                };
                if self.async_mode {
                    self.stage = DelegateStage::AwaitResult(completion);
                    DelegateAction::Suspend(Suspend::Await(result))
                } else {
                    self.process_result(i, result, completion)
                }
            }
            DelegateStage::AwaitResult(completion) => match signal {
                Resume::Next(result) => self.process_result(i, result, completion),
                Resume::Throw(error) => DelegateAction::Throw(error),
                Resume::Return(value) => DelegateAction::Return(value),
                Resume::Terminate => DelegateAction::Terminate,
            },
            DelegateStage::AwaitValue { completion, done } => match signal {
                Resume::Next(value) => self.process_value(i, value, done, completion, None),
                Resume::Throw(error) => {
                    if !done {
                        i.iterator_close(&self.iterator);
                    }
                    DelegateAction::Throw(error)
                }
                Resume::Return(value) => DelegateAction::Return(value),
                Resume::Terminate => DelegateAction::Terminate,
            },
            DelegateStage::AwaitCloseResult => match signal {
                Resume::Next(result) => {
                    if !matches!(result, Value::Obj(_)) {
                        return Self::type_error(i, "iterator 'return' must return an object");
                    }
                    if self.from_sync {
                        let value = match i.get_member(&result, "value") {
                            Ok(value) => value,
                            Err(error) => return Self::abrupt(error),
                        };
                        self.stage = DelegateStage::AwaitCloseValue;
                        DelegateAction::Suspend(Suspend::Await(value))
                    } else {
                        Self::type_error(i, "the delegated iterator has no 'throw' method")
                    }
                }
                Resume::Throw(error) => DelegateAction::Throw(error),
                Resume::Return(value) => DelegateAction::Return(value),
                Resume::Terminate => DelegateAction::Terminate,
            },
            DelegateStage::AwaitCloseValue => match signal {
                Resume::Next(_) => {
                    Self::type_error(i, "the delegated iterator has no 'throw' method")
                }
                Resume::Throw(error) => DelegateAction::Throw(error),
                Resume::Return(value) => DelegateAction::Return(value),
                Resume::Terminate => DelegateAction::Terminate,
            },
            DelegateStage::AwaitMissingReturn => match signal {
                Resume::Next(value) => DelegateAction::Return(value),
                Resume::Throw(error) => DelegateAction::Throw(error),
                Resume::Return(value) => DelegateAction::Return(value),
                Resume::Terminate => DelegateAction::Terminate,
            },
        }
    }

    fn process_result(
        &mut self,
        i: &mut Interp,
        result: Value,
        completion: DelegateCompletion,
    ) -> DelegateAction {
        use crate::coroutine::Suspend;
        if !matches!(result, Value::Obj(_)) {
            return Self::type_error(i, "iterator result is not an object");
        }
        let done = match i.get_member(&result, "done") {
            Ok(done) => i.to_boolean(&done),
            Err(error) => return Self::abrupt(error),
        };
        if !self.async_mode && !done {
            // ECMA-262 YieldExpression : yield * AssignmentExpression passes the complete iterator
            // result to GeneratorYield here. In particular, it does not perform IteratorValue, so
            // an observable `value` getter must not run until the outer caller reads it.
            self.stage = DelegateStage::Ready;
            i.yield_raw_result = true;
            return DelegateAction::Suspend(Suspend::Yield(result));
        }
        let value = match i.get_member(&result, "value") {
            Ok(value) => value,
            Err(error) => return Self::abrupt(error),
        };
        if self.async_mode && self.from_sync {
            self.stage = DelegateStage::AwaitValue { completion, done };
            return DelegateAction::Suspend(Suspend::Await(value));
        }
        self.process_value(i, value, done, completion, Some(result))
    }

    fn process_value(
        &mut self,
        i: &mut Interp,
        value: Value,
        done: bool,
        completion: DelegateCompletion,
        result: Option<Value>,
    ) -> DelegateAction {
        use crate::coroutine::Suspend;
        if done {
            return match completion {
                DelegateCompletion::Normal => DelegateAction::Continue(value),
                DelegateCompletion::Return => DelegateAction::Return(value),
            };
        }
        self.stage = DelegateStage::Ready;
        if self.async_mode {
            // AsyncGeneratorYield receives IteratorValue(innerResult); unlike an ordinary async
            // `yield`, delegation does not await a value produced by a native async iterator.
            DelegateAction::Suspend(Suspend::Yield(value))
        } else {
            // GeneratorYield(innerResult) forwards the exact result object, including getters and
            // identity, instead of creating a fresh `{ value, done }` wrapper.
            i.yield_raw_result = true;
            DelegateAction::Suspend(Suspend::Yield(
                result.expect("a synchronous delegation retains its iterator result"),
            ))
        }
    }
}

/// An async function or generator body running as a heap-owned bytecode continuation. It presents
/// the shared `resume(&mut Interp, Resume) -> Suspend` interface used by the promise/generator
/// drivers and owns every suspension point explicitly.
pub struct VmCoro {
    chunk: Rc<Chunk>,
    /// Fixed activation containing compiler-homed captures. `env` may temporarily point at a
    /// nested Object Environment Record while a suspending `with` body is active.
    cap_env: Env,
    env: Env,
    references: Vec<Option<crate::eval::PreparedReference>>,
    this_val: Value,
    slots: Vec<Value>,
    stack: Vec<Value>,
    pc: usize,
    /// The `try` handler stack, saved across suspensions so a rejected `await` inside a `try` still
    /// lands in its `catch`.
    handlers: Vec<Handler>,
    /// One DisposableResource list per active `using` statement-list boundary. Unlike the
    /// tree-walker's ambient stack, this storage belongs to the suspended continuation.
    disposal_frames: Vec<Vec<crate::interpreter::Disposable>>,
    /// In-progress class definitions keyed by immutable class-plan site. A state exists only from
    /// ClassStart until ClassFinish or its abrupt-completion cleanup pad.
    class_states: Vec<Option<crate::eval::PreparedClassEvaluation>>,
    is_generator: bool,
    is_async_generator: bool,
    awaiting_yield_value: bool,
    awaiting_return_value: bool,
    awaiting_body_return: bool,
    delegation: Option<VmDelegate>,
    async_close: Option<VmAsyncClose>,
    disposal: Option<VmDispose>,
    pub done: bool,
    pub started: bool,
}

impl VmCoro {
    /// Scan allocations owned below this continuation. The caller credits the fixed `VmCoro`
    /// payload because it may be boxed directly or embedded in a module-coroutine box.
    pub(crate) fn scan_retained_memory(&self, visitor: &mut crate::memory::Visitor) -> usize {
        fn disposable(
            resource: &crate::interpreter::Disposable,
            visitor: &mut crate::memory::Visitor,
        ) {
            visitor.value(&resource.value);
            visitor.value(&resource.method);
        }
        fn completion(completion: &DisposeCompletion, visitor: &mut crate::memory::Visitor) {
            match completion {
                DisposeCompletion::Throw(value)
                | DisposeCompletion::SourceReturn(value)
                | DisposeCompletion::ResumeReturn(value) => visitor.value(value),
                DisposeCompletion::Normal
                | DisposeCompletion::BareReturn
                | DisposeCompletion::Jump { .. } => {}
            }
        }

        visitor.chunk(&self.chunk);
        visitor.value(&self.this_val);
        let mut bytes = self
            .references
            .capacity()
            .saturating_mul(std::mem::size_of::<Option<crate::eval::PreparedReference>>())
            .saturating_add(
                self.slots
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Value>()),
            )
            .saturating_add(
                self.stack
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Value>()),
            )
            .saturating_add(
                self.handlers
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Handler>()),
            )
            .saturating_add(
                self.disposal_frames
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Vec<crate::interpreter::Disposable>>()),
            )
            .saturating_add(
                self.class_states
                    .capacity()
                    .saturating_mul(std::mem::size_of::<
                        Option<crate::eval::PreparedClassEvaluation>,
                    >()),
            );
        for reference in self.references.iter().flatten() {
            bytes = bytes.saturating_add(reference.scan_retained_memory(visitor));
        }
        for value in self.slots.iter().chain(&self.stack) {
            visitor.value(value);
        }
        for frame in &self.disposal_frames {
            bytes = bytes.saturating_add(
                frame
                    .capacity()
                    .saturating_mul(std::mem::size_of::<crate::interpreter::Disposable>()),
            );
            for resource in frame {
                disposable(resource, visitor);
            }
        }
        for state in self.class_states.iter().flatten() {
            bytes = bytes.saturating_add(state.scan_retained_memory(visitor));
        }
        if let Some(delegation) = &self.delegation {
            visitor.value(&delegation.iterator);
            visitor.value(&delegation.next);
        }
        if let Some(close) = &self.async_close {
            visitor.value(&close._iterator);
        }
        if let Some(disposal) = &self.disposal {
            bytes = bytes.saturating_add(
                disposal
                    .resources
                    .capacity()
                    .saturating_mul(std::mem::size_of::<crate::interpreter::Disposable>()),
            );
            for resource in &disposal.resources {
                disposable(resource, visitor);
            }
            if let Some(resource) = &disposal.pending_resource {
                disposable(resource, visitor);
            }
            completion(&disposal.output, visitor);
        }
        // `cap_env` and `env` are registered scope allocations and remain canonical to the
        // collector snapshot even when a suspended continuation is their discoverer.
        bytes
    }

    /// Build an async coroutine for `chunk` with its already-instantiated parameter values, parked
    /// before its first step (run on the first `resume`). `arguments` remains the original call
    /// list, as required for the function's arguments object.
    pub fn new(
        i: &mut Interp,
        chunk: Rc<Chunk>,
        env: Env,
        this_val: Value,
        params: &[Value],
        arguments: &[Value],
    ) -> VmCoro {
        let env = chunk.make_run_env(i, &env, &this_val, params);
        let references = (0..chunk.n_refs).map(|_| None).collect();
        let mut slots = vec![Value::Undefined; chunk.n_slots];
        for (k, a) in params.iter().take(chunk.n_params).enumerate() {
            slots[k] = a.clone();
        }
        if let Some(slot) = chunk.arguments_slot {
            slots[slot as usize] = Value::Obj(i.make_compiled_arguments_object(arguments, &env));
        }
        let class_states = (0..chunk.class_plans.len()).map(|_| None).collect();
        VmCoro {
            chunk,
            cap_env: env.clone(),
            env,
            references,
            this_val,
            slots,
            stack: Vec::with_capacity(16),
            pc: 0,
            handlers: Vec::new(),
            disposal_frames: Vec::new(),
            class_states,
            is_generator: false,
            is_async_generator: false,
            awaiting_yield_value: false,
            awaiting_return_value: false,
            awaiting_body_return: false,
            delegation: None,
            async_close: None,
            disposal: None,
            done: false,
            started: false,
        }
    }

    /// Build a synchronous generator continuation, parked in suspended-start without reserving a
    /// native thread stack.
    pub fn new_generator(
        i: &mut Interp,
        chunk: Rc<Chunk>,
        env: Env,
        this_val: Value,
        params: &[Value],
        arguments: &[Value],
    ) -> VmCoro {
        let mut coroutine = Self::new(i, chunk, env, this_val, params, arguments);
        coroutine.is_generator = true;
        coroutine
    }

    /// Build an async-generator continuation. `yield value` first reports `Await(value)` to the
    /// async-generator driver and publishes the settled value as `Yield` on the reaction resume.
    pub fn new_async_generator(
        i: &mut Interp,
        chunk: Rc<Chunk>,
        env: Env,
        this_val: Value,
        params: &[Value],
        arguments: &[Value],
    ) -> VmCoro {
        let mut coroutine = Self::new_generator(i, chunk, env, this_val, params, arguments);
        coroutine.is_async_generator = true;
        coroutine
    }

    /// Drive one step: run to the next `await`/`yield`, completion, or uncaught throw. A resumed
    /// throw is injected at the suspension point so an enclosing VM `try` can catch it.
    pub fn resume(
        &mut self,
        i: &mut Interp,
        mut signal: crate::coroutine::Resume,
    ) -> crate::coroutine::Suspend {
        use crate::coroutine::{Resume, Suspend};
        if self.done {
            return Suspend::Done(Value::Undefined);
        }
        let mut settled_body_return = None;
        if self.awaiting_body_return {
            self.awaiting_body_return = false;
            let settled = std::mem::replace(&mut signal, Resume::Terminate);
            settled_body_return = Some(match settled {
                Resume::Next(value) | Resume::Return(value) => {
                    PendingCompletion::ResumeReturn(value)
                }
                Resume::Throw(error) => PendingCompletion::Throw(error),
                Resume::Terminate => {
                    self.done = true;
                    return Suspend::Done(Value::Undefined);
                }
            });
        }
        // AsyncGeneratorUnwrapYieldResumption: a return completion received at a suspended yield
        // is awaited before either completing the generator or reaching a `yield*` delegate.
        if settled_body_return.is_none() && self.awaiting_return_value {
            self.awaiting_return_value = false;
            signal = match signal {
                Resume::Next(value) => Resume::Return(value),
                Resume::Throw(error) => Resume::Throw(error),
                Resume::Return(value) => Resume::Return(value),
                Resume::Terminate => Resume::Terminate,
            };
        } else if settled_body_return.is_none() && self.is_async_generator && self.started {
            if let Resume::Return(value) = signal {
                self.awaiting_return_value = true;
                return Suspend::Await(value);
            }
        }
        if self.awaiting_yield_value {
            self.awaiting_yield_value = false;
            signal = match signal {
                Resume::Next(value) => return Suspend::Yield(value),
                other => other,
            };
        }
        let mut resume_directly = false;
        let mut disposal_pending = None;
        loop {
            let pending = if let Some(pending) = settled_body_return.take() {
                Some(pending)
            } else if let Some(pending) = disposal_pending.take() {
                Some(pending)
            } else if self.disposal.is_some() {
                let action = self
                    .disposal
                    .as_mut()
                    .expect("checked above")
                    .resume(i, signal.clone());
                match action {
                    DisposeAction::Suspend(value) => return Suspend::Await(value),
                    DisposeAction::Complete(completion) => {
                        self.disposal = None;
                        let pending = pending_dispose_completion(completion);
                        if pending.is_none() {
                            resume_directly = true;
                        }
                        pending
                    }
                    DisposeAction::Terminate => {
                        self.disposal = None;
                        self.done = true;
                        return Suspend::Done(Value::Undefined);
                    }
                }
            } else if self.async_close.is_some() {
                let action = self
                    .async_close
                    .as_mut()
                    .expect("checked above")
                    .resume(i, signal.clone());
                match action {
                    AsyncCloseAction::Suspend(suspend) => return suspend,
                    AsyncCloseAction::Continue => {
                        self.async_close = None;
                        resume_directly = true;
                        None
                    }
                    AsyncCloseAction::Throw(error) => {
                        self.async_close = None;
                        Some(PendingCompletion::Throw(error))
                    }
                    AsyncCloseAction::Terminate => {
                        self.async_close = None;
                        self.done = true;
                        return Suspend::Done(Value::Undefined);
                    }
                }
            } else if resume_directly {
                resume_directly = false;
                None
            } else if let Some(delegate) = self.delegation.as_mut() {
                match delegate.resume(i, signal.clone()) {
                    DelegateAction::Suspend(suspend) => return suspend,
                    DelegateAction::Continue(value) => {
                        self.delegation = None;
                        self.stack.push(value); // result of the YieldExpression
                        None
                    }
                    DelegateAction::Return(value) => {
                        self.delegation = None;
                        if self.handlers.iter().any(|handler| {
                            matches!(
                                &handler.target,
                                HandlerTarget::Finally { .. } | HandlerTarget::Iterator { .. }
                            )
                        }) {
                            Some(PendingCompletion::ResumeReturn(value))
                        } else {
                            self.done = true;
                            return Suspend::Done(value);
                        }
                    }
                    DelegateAction::Throw(error) => {
                        self.delegation = None;
                        Some(PendingCompletion::Throw(error))
                    }
                    DelegateAction::Terminate => {
                        self.delegation = None;
                        self.done = true;
                        return Suspend::Done(Value::Undefined);
                    }
                }
            } else {
                match signal.clone() {
                    Resume::Next(value) => {
                        if self.started {
                            // Settled await value, or the value of an ordinary `yield` expression.
                            self.stack.push(value);
                        }
                        None
                    }
                    // A rejected await or `throw()` resumption enters the VM at the suspension
                    // point, so its saved try-handler stack gets the first chance to catch it.
                    Resume::Throw(error) if self.started => Some(PendingCompletion::Throw(error)),
                    Resume::Throw(error) => {
                        self.done = true;
                        return Suspend::Throw(error);
                    }
                    Resume::Return(value) if self.started => {
                        if self.handlers.iter().any(|handler| {
                            matches!(
                                &handler.target,
                                HandlerTarget::Finally { .. } | HandlerTarget::Iterator { .. }
                            )
                        }) {
                            Some(PendingCompletion::ResumeReturn(value))
                        } else {
                            self.done = true;
                            return Suspend::Done(value);
                        }
                    }
                    Resume::Return(value) => {
                        self.done = true;
                        return Suspend::Done(value);
                    }
                    Resume::Terminate => {
                        self.done = true;
                        return Suspend::Done(Value::Undefined);
                    }
                }
            };
            self.started = true;
            // GeneratorResume/AsyncFunctionStart reinstates the suspended execution context.
            // In particular, PutValue must use this function code's strictness rather than the
            // host job/script that happened to resume it. Ordinary compiled calls establish this
            // in `run_compiled_chunk`; heap continuations must do the same on every VM slice.
            let effective_strict = if self.class_states.iter().any(Option::is_some) {
                true
            } else {
                self.chunk.strict
            };
            let saved_strict = std::mem::replace(&mut i.strict, effective_strict);
            let step = drive_vm(
                i,
                &self.chunk,
                &mut self.env,
                &self.cap_env,
                &mut self.references,
                &mut self.slots,
                &mut self.stack,
                &mut self.pc,
                &self.this_val,
                &mut self.handlers,
                &mut self.disposal_frames,
                &mut self.class_states,
                pending,
                self.is_async_generator,
            );
            i.strict = saved_strict;
            match step {
                Ok(VmStep::Await(awaited)) => return Suspend::Await(awaited),
                Ok(VmStep::AsyncClose {
                    iterator,
                    from_sync,
                    swallow_error,
                }) => {
                    let (state, action) =
                        VmAsyncClose::start(i, iterator, from_sync, swallow_error);
                    self.async_close = state;
                    match action {
                        AsyncCloseAction::Suspend(suspend) => return suspend,
                        AsyncCloseAction::Continue => {
                            self.async_close = None;
                            resume_directly = true;
                        }
                        AsyncCloseAction::Throw(error) => {
                            self.async_close = None;
                            signal = Resume::Throw(error);
                        }
                        AsyncCloseAction::Terminate => {
                            self.async_close = None;
                            self.done = true;
                            return Suspend::Done(Value::Undefined);
                        }
                    }
                    continue;
                }
                Ok(VmStep::Dispose { frame, completion }) => {
                    let mut disposal = VmDispose::new(frame, completion);
                    match disposal.advance(i) {
                        DisposeAction::Suspend(value) => {
                            self.disposal = Some(disposal);
                            return Suspend::Await(value);
                        }
                        DisposeAction::Complete(completion) => {
                            let pending = pending_dispose_completion(completion);
                            if pending.is_none() {
                                resume_directly = true;
                            }
                            disposal_pending = pending;
                        }
                        DisposeAction::Terminate => {
                            self.done = true;
                            return Suspend::Done(Value::Undefined);
                        }
                    }
                    continue;
                }
                Ok(VmStep::Yield(value)) if self.is_async_generator => {
                    self.awaiting_yield_value = true;
                    return Suspend::Await(value);
                }
                Ok(VmStep::Yield(value)) if self.is_generator => return Suspend::Yield(value),
                Ok(VmStep::Yield(_)) => {
                    self.done = true;
                    return Suspend::Throw(
                        i.make_error("SyntaxError", "yield outside a generator"),
                    );
                }
                Ok(VmStep::YieldStar(value)) if self.is_generator => {
                    match VmDelegate::open(i, value, self.is_async_generator) {
                        Ok(delegate) => {
                            self.delegation = Some(delegate);
                            // YieldExpression starts its loop with NormalCompletion(undefined).
                            signal = Resume::Next(Value::Undefined);
                        }
                        Err(Abrupt::Throw(error)) => signal = Resume::Throw(error),
                        Err(Abrupt::Return(value)) => signal = Resume::Return(value),
                        Err(Abrupt::Interrupt(_))
                        | Err(Abrupt::Break(_, _))
                        | Err(Abrupt::Continue(_, _)) => signal = Resume::Terminate,
                    }
                    continue;
                }
                Ok(VmStep::YieldStar(_)) => {
                    self.done = true;
                    return Suspend::Throw(
                        i.make_error("SyntaxError", "yield outside a generator"),
                    );
                }
                Ok(VmStep::Done(value)) => {
                    self.done = true;
                    return Suspend::Done(value);
                }
                Ok(VmStep::Return(value)) if self.is_async_generator => {
                    // ReturnStatement : return Expression awaits its operand in an async
                    // generator before producing the return completion (ECMA-262 §14.10.1).
                    self.awaiting_body_return = true;
                    return Suspend::Await(value);
                }
                Ok(VmStep::Return(value)) => {
                    self.done = true;
                    return Suspend::Done(value);
                }
                Ok(VmStep::ResumeReturn(value)) => {
                    self.done = true;
                    return Suspend::Done(value);
                }
                Ok(VmStep::BareReturn) => {
                    self.done = true;
                    return Suspend::Done(Value::Undefined);
                }
                Ok(VmStep::AbruptJump { .. }) => {
                    unreachable!("drive_vm consumes loop completions")
                }
                Err(Abrupt::Throw(error)) => {
                    self.done = true;
                    return Suspend::Throw(error);
                }
                // Return/Break/Continue can't escape a function body; treat defensively as completion.
                Err(_) => {
                    self.done = true;
                    return Suspend::Done(Value::Undefined);
                }
            }
        }
    }
}

/// Shared `++`/`--` tail: ToNumeric the old value, write old±1 back through `set`, and return the
/// value to leave on the stack — old / new / nothing per `kind`.
///
/// ECMA-262 §13.4.2-5 requires PutValue to succeed before either a prefix or postfix expression
/// completes. Accordingly the raw GetValue result is observed before potentially abrupt
/// ToNumeric, while the arithmetic `newValue` is published only after `set` succeeds. Postfix
/// returns the coerced old numeric value; an object that coerces to BigInt therefore stays BigInt.
#[inline]
fn step_value(
    i: &mut Interp,
    feedback: &crate::feedback::FeedbackVector,
    pc: usize,
    kind: UpdKind,
    old: Value,
    set: impl FnOnce(&mut Interp, Value) -> Result<(), Abrupt>,
) -> Result<Option<Value>, Abrupt> {
    let profiling = observe_arithmetic_operand(feedback, pc, &old);
    let inc = matches!(
        kind,
        UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard
    );
    let old_numeric = i.to_numeric(old)?;
    let new_value = match &old_numeric {
        Value::BigInt(old) => {
            let one = crate::bigint::JsBigInt::from_u64(1);
            Value::BigInt(if inc { old.add(&one) } else { old.sub(&one) })
        }
        Value::Num(old) => Value::Num(if inc { old + 1.0 } else { old - 1.0 }),
        _ => unreachable!("ToNumeric returns only Number or BigInt"),
    };
    set(i, new_value.clone())?;
    observe_arithmetic_result(feedback, pc, profiling, &new_value);
    Ok(match kind {
        UpdKind::PreInc | UpdKind::PreDec => Some(new_value),
        UpdKind::PostInc | UpdKind::PostDec => Some(old_numeric),
        UpdKind::IncDiscard | UpdKind::DecDiscard => None,
    })
}

/// [`step_value`] pushing its result onto the VM's operand stack.
fn step_and_store(
    i: &mut Interp,
    stack: &mut Vec<Value>,
    feedback: &crate::feedback::FeedbackVector,
    pc: usize,
    kind: UpdKind,
    old: Value,
    set: impl FnOnce(&mut Interp, Value) -> Result<(), Abrupt>,
) -> Result<(), Abrupt> {
    if let Some(v) = step_value(i, feedback, pc, kind, old, set)? {
        stack.push(v);
    }
    Ok(())
}

#[inline]
fn get_named_property(
    i: &mut Interp,
    chunk: &Chunk,
    pc: usize,
    base: &Value,
    name: &str,
    cache: &std::cell::Cell<IcState>,
) -> Result<Value, Abrupt> {
    if !chunk.feedback.detailed_enabled() {
        return i.get_prop_ic(base, name, cache);
    }
    let mut trace = crate::feedback::CurrentPropertyTrace::default();
    let result = i.get_prop_ic_profiled(base, name, cache, &mut trace);
    chunk
        .feedback
        .observe_current_property(pc, &chunk.feedback_shapes, trace);
    result
}

#[inline]
fn set_named_property(
    i: &mut Interp,
    chunk: &Chunk,
    pc: usize,
    base: &Value,
    name: &Rc<str>,
    value: Value,
    cache: &std::cell::Cell<IcState>,
) -> Result<(), Abrupt> {
    if !chunk.feedback.detailed_enabled() {
        return i.set_prop_ic(base, name, value, cache);
    }
    let mut trace = crate::feedback::CurrentPropertyTrace::default();
    let result = i.set_prop_ic_profiled(base, name, value, cache, &mut trace);
    chunk
        .feedback
        .observe_current_property(pc, &chunk.feedback_shapes, trace);
    result
}

#[inline]
fn observe_element_trace(
    i: &Interp,
    chunk: &Chunk,
    pc: usize,
    base: &Value,
    key: &str,
    trace: crate::feedback::CurrentPropertyTrace,
) {
    let Some(property_outcome) = trace.outcome else {
        return;
    };
    let receiver = i.element_receiver_kind(base);
    let key_kind = i.element_key_kind(key);
    let outcome = match property_outcome {
        crate::feedback::PropertyOutcome::Data => {
            if trace.depth == 0 {
                crate::feedback::ElementOutcome::OwnData
            } else {
                crate::feedback::ElementOutcome::Prototype
            }
        }
        crate::feedback::PropertyOutcome::Absent => {
            if receiver == crate::feedback::ElementReceiverKind::Array
                && key_kind == crate::feedback::ElementKeyKind::Index
            {
                crate::feedback::ElementOutcome::Hole
            } else {
                crate::feedback::ElementOutcome::Absent
            }
        }
        crate::feedback::PropertyOutcome::Created => crate::feedback::ElementOutcome::Created,
        crate::feedback::PropertyOutcome::Accessor => crate::feedback::ElementOutcome::Accessor,
        crate::feedback::PropertyOutcome::Exotic => crate::feedback::ElementOutcome::Exotic,
        crate::feedback::PropertyOutcome::Rejected => crate::feedback::ElementOutcome::Rejected,
    };
    chunk
        .feedback
        .observe_element(pc, receiver, key_kind, outcome);
}

/// Profile a computed element read after the normal nullish check and `ToPropertyKey` conversion.
/// Diagnostic mode deliberately bypasses dense/temporary IC shortcuts so the one canonical
/// `[[Get]]` operation supplies the semantic outcome without replaying observable work.
#[inline]
fn get_element_profiled(
    i: &mut Interp,
    chunk: &Chunk,
    pc: usize,
    base: &Value,
    raw_key: &Value,
) -> Result<Value, Abrupt> {
    if matches!(base, Value::Undefined | Value::Null) {
        return Err(i.throw("TypeError", "cannot read property of null or undefined"));
    }
    let key = i.to_property_key(raw_key)?;
    let mut trace = crate::feedback::CurrentPropertyTrace::default();
    let result = i.get_member_profiled(base, key.as_str(), &mut trace);
    observe_element_trace(i, chunk, pc, base, key.as_str(), trace);
    result
}

/// Profile a computed element write after `ToPropertyKey`, retaining the original value/result
/// handling in the caller. The underlying [[Set]] operation is still executed exactly once.
#[inline]
fn set_element_profiled(
    i: &mut Interp,
    chunk: &Chunk,
    pc: usize,
    base: &Value,
    raw_key: &Value,
    value: Value,
) -> Result<(), Abrupt> {
    let key = i.to_property_key(raw_key)?;
    let mut trace = crate::feedback::CurrentPropertyTrace::default();
    let result = i.set_member_profiled(base, key.as_str(), value, &mut trace);
    observe_element_trace(i, chunk, pc, base, key.as_str(), trace);
    result
}

#[inline]
fn call_arity_kind(len: usize) -> crate::feedback::CallArityKind {
    match len {
        0 => crate::feedback::CallArityKind::Zero,
        1 => crate::feedback::CallArityKind::One,
        2..=4 => crate::feedback::CallArityKind::Few,
        _ => crate::feedback::CallArityKind::Many,
    }
}

/// Profile one completed `Call` operation. Target metadata is captured before dispatch, and the
/// return value class is published only after [[Call]] succeeds, matching EvaluateCall's ordering.
#[inline]
fn call_profiled(
    i: &mut Interp,
    chunk: &Chunk,
    pc: usize,
    callee: Value,
    this: Value,
    args: &[Value],
) -> Result<Value, Abrupt> {
    let target = i.call_target_kind(&callee);
    let environment = i.call_environment_kind(&callee);
    chunk
        .feedback
        .observe_call(pc, target, call_arity_kind(args.len()), environment);
    let result = i.call(callee, this, args);
    if let Ok(value) = &result {
        if let Some(class) = arithmetic_value_class(value) {
            chunk
                .feedback
                .observe_value_class(pc, crate::feedback::ObservationRole::Result, class);
        }
    }
    result
}

/// Profile one completed `Construct` operation. The construct result is necessarily an object on
/// success, but it still flows through the normal ValueClass result slot for a uniform call ABI.
#[inline]
fn construct_profiled(
    i: &mut Interp,
    chunk: &Chunk,
    pc: usize,
    callee: Value,
    args: &[Value],
) -> Result<Value, Abrupt> {
    let target = i.call_target_kind(&callee);
    let environment = i.call_environment_kind(&callee);
    chunk
        .feedback
        .observe_call(pc, target, call_arity_kind(args.len()), environment);
    let result = i.construct(callee, args);
    if let Ok(value) = &result {
        if let Some(class) = arithmetic_value_class(value) {
            chunk
                .feedback
                .observe_value_class(pc, crate::feedback::ObservationRole::Result, class);
        }
    }
    result
}

#[inline]
fn bin_num(
    i: &mut Interp,
    stack: &mut Vec<Value>,
    feedback: &crate::feedback::FeedbackVector,
    pc: usize,
    op: &'static str,
    f: impl Fn(f64, f64) -> f64,
) -> Result<(), Abrupt> {
    let b = stack.pop().expect("vm stack underflow");
    let a = stack.pop().expect("vm stack underflow");
    let profiling = observe_arithmetic_operands(feedback, pc, &a, &b);
    let v = match i
        .tagged_arithmetic
        .then(|| try_tagged_numeric_binary(&a, &b, &f))
        .flatten()
    {
        Some(value) => value,
        None => match (&a, &b) {
            (Value::Num(x), Value::Num(y)) => Value::Num(f(*x, *y)),
            _ => i.binary(op, a, b)?,
        },
    };
    observe_arithmetic_result(feedback, pc, profiling, &v);
    stack.push(v);
    Ok(())
}

/// Attempt the immediate-only tagged ABI path. Returning `None` is a deliberate deoptimization:
/// heap values, strings, BigInts, symbols, objects, and the internal Empty marker all continue
/// through the complete interpreter helper, preserving ToPrimitive/ToNumeric ordering and
/// abrupt completion behavior mandated by ECMA-262 §13.15 (Additive Operators).
#[inline(always)]
fn try_tagged_numeric_binary<F>(left: &Value, right: &Value, f: &F) -> Option<Value>
where
    F: Fn(f64, f64) -> f64,
{
    let (Value::Num(left), Value::Num(right)) = (left, right) else {
        return None;
    };
    let frame = crate::tagged::TaggedNumericFrame::new(*left, *right);
    let result = frame.binary(f);
    Some(Value::Num(
        result
            .as_number()
            .expect("tagged numeric result remains a Number"),
    ))
}

#[inline]
fn bin_i32(
    i: &mut Interp,
    stack: &mut Vec<Value>,
    feedback: &crate::feedback::FeedbackVector,
    pc: usize,
    op: &'static str,
    f: impl Fn(i32, i32) -> i32,
) -> Result<(), Abrupt> {
    let b = stack.pop().expect("vm stack underflow");
    let a = stack.pop().expect("vm stack underflow");
    let profiling = observe_arithmetic_operands(feedback, pc, &a, &b);
    let v = if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
        Value::Num(f(crate::eval::to_int32(*x), crate::eval::to_int32(*y)) as f64)
    } else {
        i.binary(op, a, b)?
    };
    observe_arithmetic_result(feedback, pc, profiling, &v);
    stack.push(v);
    Ok(())
}

#[inline]
fn bin_cmp(
    i: &mut Interp,
    stack: &mut Vec<Value>,
    op: &'static str,
    f: impl Fn(f64, f64) -> bool,
) -> Result<(), Abrupt> {
    let b = stack.pop().expect("vm stack underflow");
    let a = stack.pop().expect("vm stack underflow");
    if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
        stack.push(Value::Bool(f(*x, *y)));
        return Ok(());
    }
    let v = i.binary(op, a, b)?;
    stack.push(v);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// JIT support: Chunk accessors and the runtime helpers the machine-code templates call.
// The generic executor `jit_exec` runs exactly ONE op against a raw operand-stack pointer; the
// templates bake the op index in as an immediate and keep the stack top in a register. Control
// flow never reaches here — jumps, returns and try bookkeeping are real branches in the JIT.
// ---------------------------------------------------------------------------------------------

/// Rust helper scopes temporarily expand ARM64's packed local slots in place. The raw-pointer
/// guard deliberately does not borrow `JitCtx`, allowing the helper body to use it normally; on
/// every ordinary return it restores packed ownership before generated code resumes.
struct JitWideSlots {
    ctx: *mut crate::jit::JitCtx,
    repack: bool,
}

impl JitWideSlots {
    unsafe fn enter(ctx: *mut crate::jit::JitCtx) -> JitWideSlots {
        let repack = unsafe { (*ctx).slots_packed };
        if repack {
            unsafe { (*ctx).unpack_slots() };
        }
        JitWideSlots { ctx, repack }
    }
}

impl Drop for JitWideSlots {
    fn drop(&mut self) {
        if self.repack {
            unsafe { (*self.ctx).pack_slots() };
        }
    }
}

impl Chunk {
    pub(crate) fn jit_ops(&self) -> &[Op] {
        &self.ops
    }
    pub(crate) fn jit_detailed_feedback_enabled(&self) -> bool {
        self.feedback.detailed_enabled()
    }
    /// Leading slot names, for debug identification of a chunk (`LUMEN_JIT_DUMP`).
    pub(crate) fn jit_slot_names(&self) -> &[Rc<str>] {
        &self.slot_names
    }
    /// Guard data for a speculative-inline site (`Op::InlineGuard`'s first operand).
    pub(crate) fn jit_inline_target(&self, t: u32) -> &InlineTarget {
        &self.inline_targets[t as usize]
    }

    /// The interned name a property op refers to (the emitter gates array-receiver inlining on
    /// whether it could be an element key).
    pub(crate) fn jit_name(&self, n: u32) -> &str {
        &self.names[n as usize]
    }
    /// The stable address of inline-cache site `idx`'s `Cell<IcState>`. The `caches` `Vec` is
    /// fixed once compilation finishes (never reallocated), and the `Chunk` outlives its own JIT
    /// code, so the JIT bakes this address as an immediate to read the live cache from machine
    /// code. `None` if the emitter cannot use it (the address must be reachable — always is here).
    pub(crate) fn jit_cache_ptr(&self, idx: u32) -> usize {
        self.caches.as_ptr() as usize
            + idx as usize * std::mem::size_of::<std::cell::Cell<IcState>>()
    }
    /// The hottest property-cache way observed during bytecode warmup. A compact JIT site may
    /// bake this state behind full shape/prototype guards and route misses to the checked helper.
    pub(crate) fn jit_cache_preferred(&self, idx: u32) -> Option<IcState> {
        let st = self.caches[idx as usize].get();
        // A compact template bakes one shape. It is a win only for a truly monomorphic site;
        // choosing way 0 at a polymorphic virtual-method call makes every other stable way fall
        // through to Rust. Seeded second-stage chunks preserve all hot ways, so keep their full
        // native probe whenever warmup observed more than one receiver shape.
        let mono =
            (1..PROP_IC_WAYS).all(|way| self.caches[idx as usize + way].get().depth == IC_EMPTY);
        (st.depth != IC_EMPTY && mono).then_some(st)
    }
    /// The stable address of call site `idx`'s way-1 `Cell<CallIc>` (same contract as
    /// [`Chunk::jit_cache_ptr`]: `call_caches` is fixed once compilation finishes and the Chunk
    /// outlives its JIT code).
    pub(crate) fn jit_call_cache_ptr(&self, idx: u32) -> usize {
        self.call_caches[idx as usize].entries.as_ptr() as usize
    }
    /// The stable address of name-cache site `idx`'s `Cell<NameIc>` (same contract as
    /// [`Chunk::jit_cache_ptr`]).
    pub(crate) fn jit_name_cache_ptr(&self, idx: u32) -> usize {
        self.name_caches.as_ptr() as usize
            + idx as usize * std::mem::size_of::<std::cell::Cell<NameIc>>()
    }
    /// Stable address of the captured-binding cache for interned name `idx`.
    pub(crate) fn jit_cap_cache_ptr(&self, idx: u32) -> usize {
        self.cap_caches.as_ptr() as usize
            + idx as usize * std::mem::size_of::<std::cell::Cell<NameIc>>()
    }
    /// Numeric value observed at this name site before second-stage compilation. Generated code
    /// compares the live binding/property bits before taking the specialized decode path.
    pub(crate) fn jit_name_number(&self, idx: u32) -> Option<u64> {
        self.name_num_valid[idx as usize]
            .get()
            .then(|| self.name_num_bits[idx as usize].get())
    }
    /// Name-cache hit check (see [`NameIc`] for the validation story): a pointer compare, a
    /// generation compare, and a value clone. `None` = miss (including TDZ — the slow path
    /// throws the proper error).
    #[inline]
    fn name_ic_hit(&self, i: &Interp, env: &Env, c: u32) -> Option<Value> {
        let ic = self.name_caches[c as usize].get();
        let raw = Rc::as_ptr(env) as usize;
        if ic.env == raw {
            let b = env.borrow();
            if b.vars.generation() != ic.gen {
                return None;
            }
            // The unchanged generation proves the map is structurally untouched since the fill:
            // the pointer is live and the resolution unchanged (see NameIc). The value and TDZ
            // flag are read live — in-place writes flow through.
            let bd = unsafe { &*(ic.binding as usize as *const crate::interpreter::Binding) };
            return if bd.initialized {
                Some(bd.value.clone())
            } else {
                None
            };
        }
        if ic.env == raw | 1 {
            // Global-object mode (see NameIc): scope still empty of this name (generation),
            // global layout unchanged (shape) → the cached slot is still the resolution.
            if env.borrow().vars.generation() != ic.gen {
                return None;
            }
            let g = i.global.borrow();
            if !matches!(g.exotic, crate::value::Exotic::None)
                || g.props.shape() != (ic.binding >> 32) as u32
            {
                return None;
            }
            let (_, p) = g.props.entry_at(ic.binding as u32 as usize)?;
            if p.accessor() {
                return None;
            }
            return Some(p.value());
        }
        if ic.env & 2 != 0 {
            // Depth-1 mode (see NameIc): `env` is this chunk's fresh activation. Its expected
            // generation proves it holds exactly the chunk's cap_inits — which can never
            // include a LoadName'd free name — so the parent resolution still applies; the
            // parent's generation proves the binding pointer live and unmoved.
            let b = env.borrow();
            if b.vars.generation() != ic.act_gen {
                return None;
            }
            let p = b.parent.as_ref()?;
            if Rc::as_ptr(p) as usize | 2 != ic.env {
                return None;
            }
            let pb = p.borrow();
            if pb.vars.generation() != ic.gen {
                return None;
            }
            let bd = unsafe { &*(ic.binding as usize as *const crate::interpreter::Binding) };
            return if bd.initialized {
                Some(bd.value.clone())
            } else {
                None
            };
        }
        None
    }
    /// Depth-0 cache fill: the name resolves directly in `env` as a plain initialized binding —
    /// no `with` object on the scope, no live import redirect. When `env` *is* the global scope
    /// and misses, an own data property of the ordinary global object fills the global mode
    /// instead. Returns the value on success; `None` = not cacheable at this site (the caller
    /// runs the interpreter's full walk, uncached).
    fn name_ic_fill(&self, i: &Interp, env: &Env, n: u32, c: u32) -> Option<Value> {
        {
            let b = env.borrow();
            if b.with_obj.is_some() {
                return None;
            }
            if let Some(bd) = b.vars.get(&self.names[n as usize]) {
                if !bd.initialized || bd.import_ref.is_some() {
                    return None;
                }
                let v = bd.value.clone();
                self.record_name_number(c as usize, &v);
                self.name_caches[c as usize].set(NameIc {
                    env: Rc::as_ptr(env) as usize,
                    binding: bd as *const _ as usize as u64,
                    gen: b.vars.generation(),
                    act_gen: 0,
                });
                drop(b);
                // Pin the scope allocation so the raw `env` compare stays ABA-safe.
                self.name_pins.borrow_mut()[c as usize] = Some(Rc::downgrade(env));
                return Some(v);
            }
            // Depth-1 fill, ONLY for a chunk that runs under an activation: `env` is then that
            // activation — fresh pointer every call (the depth-0 mode above can never hit) but
            // chunk-determined CONTENTS, so its generation alone re-proves "this name still
            // isn't shadowed here" on any later activation. Never valid for no-activation
            // chunks: their run env is a closure-instance-specific scope whose generation says
            // nothing about which names it holds.
            if self.makes_env() {
                if let Some(p) = &b.parent {
                    let pb = p.borrow();
                    if pb.with_obj.is_none() {
                        if let Some(bd) = pb.vars.get(&self.names[n as usize]) {
                            if bd.initialized && bd.import_ref.is_none() {
                                let v = bd.value.clone();
                                self.record_name_number(c as usize, &v);
                                self.name_caches[c as usize].set(NameIc {
                                    env: Rc::as_ptr(p) as usize | 2,
                                    binding: bd as *const _ as usize as u64,
                                    gen: pb.vars.generation(),
                                    act_gen: b.vars.generation(),
                                });
                                let pin = Rc::downgrade(p);
                                drop(pb);
                                drop(b);
                                // Pin the PARENT: that's the raw pointer the hit compares.
                                self.name_pins.borrow_mut()[c as usize] = Some(pin);
                                return Some(v);
                            }
                        }
                    }
                }
            }
        }
        // Global mode: only when there are no intermediate scopes whose later mutation could
        // re-route the name — i.e. the chunk runs directly under the global scope.
        if !Rc::ptr_eq(env, &i.global_env) || !i.ordinary_get_ptr(Rc::as_ptr(&i.global) as usize) {
            return None;
        }
        let g = i.global.borrow();
        if !matches!(g.exotic, crate::value::Exotic::None) {
            return None;
        }
        let slot = g.props.slot_of(&self.names[n as usize])?;
        let (_, p) = g.props.entry_at(slot)?;
        if p.accessor() {
            return None;
        }
        let v = p.value();
        self.record_name_number(c as usize, &v);
        self.name_caches[c as usize].set(NameIc {
            env: Rc::as_ptr(env) as usize | 1,
            binding: ((g.props.shape() as u64) << 32) | slot as u64,
            gen: env.borrow().vars.generation(),
            act_gen: 0,
        });
        drop(g);
        self.name_pins.borrow_mut()[c as usize] = Some(Rc::downgrade(env));
        Some(v)
    }
    /// Cached free-name read: hit, else depth-0 refill, else the interpreter's full walk
    /// (deeper resolutions, `with`, module imports, TDZ, globals — uncached every time).
    pub(crate) fn load_name_ic(
        &self,
        i: &mut Interp,
        env: &Env,
        n: u32,
        c: u32,
    ) -> Result<Value, Abrupt> {
        if let Some(v) = self.name_ic_hit(i, env, c) {
            return Ok(v);
        }
        if let Some(v) = self.name_ic_fill(i, env, n, c) {
            return Ok(v);
        }
        i.get_var(&self.names[n as usize], env)
    }

    /// Resolve a captured binding in the current activation and cache its stable map-entry
    /// address. Structural `VarMap` mutations bump `generation`; the weak scope pin prevents an
    /// activation allocation from being recycled while its raw pointer remains cached.
    fn cap_binding_ptr(&self, env: &Env, n: u32) -> *mut crate::interpreter::Binding {
        let index = n as usize;
        let raw = Rc::as_ptr(env) as usize;
        let ic = self.cap_caches[index].get();
        {
            let b = env.borrow();
            if ic.env == raw && b.vars.generation() == ic.gen {
                return ic.binding as usize as *mut crate::interpreter::Binding;
            }
        }
        let name = &self.names[index];
        let (binding, generation) = {
            let mut b = env.borrow_mut();
            let generation = b.vars.generation();
            let binding = b.vars.get_mut(name).expect("captured binding missing")
                as *mut crate::interpreter::Binding;
            (binding, generation)
        };
        self.cap_caches[index].set(NameIc {
            env: raw,
            binding: binding as usize as u64,
            gen: generation,
            act_gen: 0,
        });
        self.cap_pins.borrow_mut()[index] = Some(Rc::downgrade(env));
        binding
    }

    pub(crate) fn load_cap_ic(&self, i: &mut Interp, env: &Env, n: u32) -> Result<Value, Abrupt> {
        let binding = unsafe { &*self.cap_binding_ptr(env, n) };
        if !binding.initialized {
            return Err(i.throw(
                "ReferenceError",
                format!(
                    "cannot access '{}' before initialization",
                    self.names[n as usize]
                ),
            ));
        }
        Ok(binding.value.clone())
    }

    pub(crate) fn store_cap_ic(
        &self,
        i: &mut Interp,
        env: &Env,
        n: u32,
        value: Value,
        initialize: bool,
    ) -> Result<(), Abrupt> {
        let binding = unsafe { &mut *self.cap_binding_ptr(env, n) };
        if !initialize && !binding.initialized {
            return Err(i.throw(
                "ReferenceError",
                format!(
                    "cannot access '{}' before initialization",
                    self.names[n as usize]
                ),
            ));
        }
        binding.value = value;
        if initialize {
            binding.initialized = true;
        }
        Ok(())
    }

    /// Reject PutValue to a compiler-proven immutable captured binding without cloning its old
    /// value. DeclarativeEnvironmentRecord.SetMutableBinding checks initialization before
    /// immutability (ECMA-262 §9.1.1.1.5), so a store into a TDZ remains a ReferenceError.
    fn reject_const_cap_store(&self, i: &mut Interp, env: &Env, n: u32) -> Result<(), Abrupt> {
        let binding = unsafe { &*self.cap_binding_ptr(env, n) };
        if !binding.initialized {
            return Err(i.throw(
                "ReferenceError",
                format!(
                    "cannot access '{}' before initialization",
                    self.names[n as usize]
                ),
            ));
        }
        Err(i.throw(
            "TypeError",
            format!(
                "assignment to constant variable '{}'",
                self.names[n as usize]
            ),
        ))
    }

    /// Cached free-name write. A cache hit validates the same resolution proof as a read and
    /// updates a live mutable binding/plain writable global property in place. Misses perform the
    /// full PutValue operation, then side-effect-freely seed a cache when the resulting resolution
    /// is cacheable.
    fn store_name_ic(
        &self,
        i: &mut Interp,
        env: &Env,
        n: u32,
        c: u32,
        value: Value,
    ) -> Result<(), Abrupt> {
        let ic = self.name_caches[c as usize].get();
        let raw = Rc::as_ptr(env) as usize;
        let value = if ic.env == raw {
            let b = env.borrow_mut();
            if b.vars.generation() == ic.gen {
                let bd = unsafe { &mut *(ic.binding as usize as *mut crate::interpreter::Binding) };
                if bd.initialized && bd.mutable && bd.import_ref.is_none() {
                    bd.value = value;
                    return Ok(());
                }
            }
            value
        } else if ic.env == raw | 1 {
            if env.borrow().vars.generation() == ic.gen {
                let mut g = i.global.borrow_mut();
                if matches!(g.exotic, crate::value::Exotic::None)
                    && g.props.shape() == (ic.binding >> 32) as u32
                {
                    if let Some((_, p)) = g.props.entry_at_mut(ic.binding as u32 as usize) {
                        if !p.accessor() && p.writable() {
                            p.set_value(value);
                            return Ok(());
                        }
                    }
                }
            }
            value
        } else if ic.env & 2 != 0 {
            let parent = {
                let b = env.borrow();
                (b.vars.generation() == ic.act_gen)
                    .then(|| b.parent.clone())
                    .flatten()
            };
            if let Some(parent) = parent {
                if Rc::as_ptr(&parent) as usize | 2 == ic.env {
                    let pb = parent.borrow_mut();
                    if pb.vars.generation() == ic.gen {
                        let bd = unsafe {
                            &mut *(ic.binding as usize as *mut crate::interpreter::Binding)
                        };
                        if bd.initialized && bd.mutable && bd.import_ref.is_none() {
                            bd.value = value;
                            return Ok(());
                        }
                    }
                }
            }
            value
        } else {
            value
        };
        i.assign_free_name(&self.names[n as usize], value, env)?;
        let _ = self.name_ic_fill(i, env, n, c);
        Ok(())
    }
    pub(crate) fn jit_frame(&self) -> (usize, usize) {
        (self.n_params, self.n_slots)
    }
    pub(crate) fn jit_arguments_slot(&self) -> Option<u16> {
        self.arguments_slot
    }
    /// Recognize the Prototype.js-style forwarding constructor
    /// `this.<initializer>.apply(this, arguments);`.
    ///
    /// The construct fast path can execute this exact, branch-free body without materializing
    /// the otherwise short-lived `arguments` exotic. Property and builtin-identity guards remain
    /// live there; any accessor, proxy, `apply` override, or other body shape runs normally.
    #[inline]
    pub(crate) fn jit_arguments_apply_forwarder(
        &self,
    ) -> Option<(&str, &std::cell::Cell<IcState>, &std::cell::Cell<IcState>)> {
        let plan = self.arguments_forwarder.get_or_init(|| {
            let [Op::GetPropThis(initializer, initializer_cache), Op::GetMethod(apply, apply_cache), Op::LoadThis, Op::LoadLocal(arguments), Op::CallWithThis(2, _), Op::Pop, Op::ReturnUndef] =
                self.ops.as_slice()
            else {
                return None;
            };
            (self.arguments_slot == Some(*arguments) && &*self.names[*apply as usize] == "apply")
                .then_some(ArgumentsForwarder {
                    initializer: *initializer,
                    initializer_cache: *initializer_cache,
                    apply_cache: *apply_cache,
                })
        });
        let plan = plan.as_ref()?;
        Some((
            &self.names[plan.initializer as usize],
            &self.caches[plan.initializer_cache as usize],
            &self.caches[plan.apply_cache as usize],
        ))
    }

    pub(crate) fn jit_forwarder_runtime(
        &self,
        initializer: &crate::value::Gc,
        apply: usize,
    ) -> Option<Rc<Chunk>> {
        let initializer_ptr = Rc::as_ptr(initializer) as usize;
        self.arguments_forwarder_runtime
            .borrow()
            .as_ref()
            .filter(|runtime| {
                runtime.initializer == initializer_ptr
                    && runtime.apply == apply
                    && runtime.pin.as_ptr() == Rc::as_ptr(initializer)
            })
            .map(|runtime| runtime.chunk.clone())
    }

    pub(crate) fn cache_jit_forwarder_runtime(
        &self,
        initializer: &crate::value::Gc,
        apply: usize,
        runtime_chunk: Rc<Chunk>,
    ) {
        let initializer_ptr = Rc::as_ptr(initializer) as usize;
        *self.arguments_forwarder_runtime.borrow_mut() = Some(ForwarderRuntime {
            initializer: initializer_ptr,
            apply,
            pin: Rc::downgrade(initializer),
            chunk: runtime_chunk,
        });
    }

    fn parse_initializer_plan(&self) -> Option<InitializerPlan> {
        if self.arguments_slot.is_some()
            || self.needs_env()
            || self.n_params > 128
            || !matches!(self.ops.last(), Some(Op::ReturnUndef))
        {
            return None;
        }

        let finish = |fields: Vec<InitializerField>| {
            let mut used = 0u128;
            if fields.is_empty()
                || fields.len() > 8
                || fields.iter().any(|field| {
                    if field.slot as usize >= self.n_params {
                        return true;
                    }
                    let bit = 1u128 << field.slot;
                    let duplicate = used & bit != 0;
                    used |= bit;
                    duplicate
                })
            {
                None
            } else {
                Some(InitializerPlan {
                    fields,
                    shapes: std::cell::Cell::new(InitializerShapes::default()),
                })
            }
        };

        // `this.x = arg ? arg : constant`, repeated, then implicit return.
        let mut pc = 0usize;
        let mut fields = Vec::new();
        while let Some(
            [Op::LoadLocal(test), Op::JumpIfFalse(on_false), Op::LoadLocal(value), Op::Jump(done), Op::Const(default), Op::SetPropThisDrop(name, cache)],
        ) = self.ops.get(pc..pc + 6)
        {
            if test != value || *on_false as usize != pc + 4 || *done as usize != pc + 5 {
                break;
            }
            fields.push(InitializerField {
                slot: *test,
                default: Some(*default),
                name: *name,
                cache: *cache,
            });
            pc += 6;
        }
        if pc + 1 == self.ops.len() && matches!(self.ops[pc], Op::ReturnUndef) {
            return finish(fields);
        }

        // `if (!arg) arg = constant` prefixes followed by direct `this.x = arg` stores.
        let mut defaults = [None; 128];
        pc = 0;
        while let Some(
            [Op::LoadLocal(test), Op::Not, Op::JumpIfFalse(done), Op::Const(default), Op::StoreLocal(store)],
        ) = self.ops.get(pc..pc + 5)
        {
            if test != store
                || *test as usize >= self.n_params
                || *done as usize != pc + 5
                || defaults[*test as usize].is_some()
            {
                break;
            }
            defaults[*test as usize] = Some(*default);
            pc += 5;
        }
        fields = Vec::new();
        while let Some([Op::LoadLocal(slot), Op::SetPropThisDrop(name, cache)]) =
            self.ops.get(pc..pc + 2)
        {
            fields.push(InitializerField {
                slot: *slot,
                default: defaults.get(*slot as usize).copied().flatten(),
                name: *name,
                cache: *cache,
            });
            pc += 2;
        }
        if pc + 1 == self.ops.len() && matches!(self.ops[pc], Op::ReturnUndef) {
            return finish(fields);
        }
        None
    }

    pub(crate) fn jit_initializer_plan(&self) -> Option<&InitializerPlan> {
        self.initializer_plan
            .get_or_init(|| self.parse_initializer_plan())
            .as_ref()
    }

    #[inline(always)]
    pub(crate) fn jit_initializer_field(
        &self,
        plan: &InitializerPlan,
        index: usize,
    ) -> (usize, Option<&Value>, &Rc<str>, &std::cell::Cell<IcState>) {
        let field = &plan.fields[index];
        (
            field.slot as usize,
            field
                .default
                .map(|constant| &self.consts[constant as usize]),
            &self.names[field.name as usize],
            &self.caches[field.cache as usize],
        )
    }

    /// Exact straight-line field constructor recognized without function or benchmark identity.
    /// Unique parameter slots let the construct path move owned values directly into the fresh
    /// object. Any computed value, control flow, duplicate parameter use, or materialized
    /// `arguments` object declines to ordinary execution.
    pub(crate) fn jit_simple_constructor(&self) -> Option<SimpleConstructor<'_>> {
        let fields = self
            .simple_constructor_plan
            .get_or_init(|| {
                if self.ops.len() < 3
                    || self.ops.len() > 17
                    || self.ops.len() & 1 == 0
                    || !matches!(self.ops.last(), Some(Op::ReturnUndef))
                    || self.arguments_slot.is_some()
                    || self.n_params > 128
                {
                    return None;
                }
                let count = self.ops.len() / 2;
                let mut fields = Vec::with_capacity(count);
                let mut used = 0u128;
                for pair_index in 0..count {
                    let start = pair_index * 2;
                    let pair = &self.ops[start..start + 2];
                    let [Op::LoadLocal(slot), Op::SetPropThisDrop(name, cache)] = pair else {
                        return None;
                    };
                    if *slot as usize >= self.n_params {
                        return None;
                    }
                    let bit = 1u128 << *slot;
                    if used & bit != 0 {
                        return None;
                    }
                    used |= bit;
                    fields.push(InitializerField {
                        slot: *slot,
                        default: None,
                        name: *name,
                        cache: *cache,
                    });
                }
                Some(fields)
            })
            .as_deref()?;
        Some(SimpleConstructor {
            chunk: self,
            fields,
        })
    }

    /// Whether calls run without an activation environment (nothing captured, no lexical
    /// `this`) — the precondition for the JIT→JIT fast call's moved-argument entry.
    pub(crate) fn jit_no_activation(&self) -> bool {
        !self.needs_env()
    }
    /// Byte offset of `inline_attempted` within `Chunk` (self-probed; every Chunk shares the
    /// monomorphized layout, so the caller's offset is the callee's too).
    pub(crate) fn jit_inline_attempted_off(&self) -> usize {
        &self.inline_attempted as *const _ as usize - self as *const Chunk as usize
    }
    /// [`CallIc::direct`] gates for this chunk (see its docs).
    pub(crate) fn jit_direct_flags(&self, code: &crate::jit::JitCode) -> u8 {
        if !direct_shared_context_enabled() {
            return 0;
        }
        let mut f = 1u8;
        #[cfg(all(
            target_arch = "aarch64",
            any(target_os = "macos", target_os = "linux", target_os = "windows")
        ))]
        {
            if code.needs_global {
                f |= 2;
            }
            // The direct sequence carves [slots|operand stack] from one fixed-size pooled
            // buffer, exactly like run_moved's fast path — a frame that doesn't fit must take
            // the layered path (run_moved falls back to growable Vecs there).
            let (_, n_slots) = self.jit_frame();
            if n_slots + code.max_stack <= crate::jit::FRAME_BUF {
                f |= 8;
            }
        }
        #[cfg(not(all(
            target_arch = "aarch64",
            any(target_os = "macos", target_os = "linux", target_os = "windows")
        )))]
        let _ = code;
        f
    }
    /// Whether const `k` is a trivially-copyable value the JIT may materialize inline.
    pub(crate) fn jit_const_copyable(&self, k: u32) -> bool {
        matches!(
            self.consts[k as usize],
            Value::Undefined | Value::Null | Value::Bool(_) | Value::Num(_)
        )
    }
    /// Stable address of const `k` (the chunk is pinned by any code that runs it): the JIT's
    /// string-const template copies the Value and bumps its refcount inline.
    pub(crate) fn jit_const_ptr(&self, k: u32) -> *const Value {
        &self.consts[k as usize] as *const Value
    }
    /// Whether const `k` is a string (payload strong count at offset 0 — the template's bump).
    pub(crate) fn jit_const_is_str(&self, k: u32) -> bool {
        matches!(self.consts[k as usize], Value::Str(_))
    }
    /// The f64 bits of a Num const (for the JIT's register-chain emitter).
    pub(crate) fn jit_const_num(&self, k: u32) -> Option<u64> {
        match &self.consts[k as usize] {
            Value::Num(n) => Some(n.to_bits()),
            _ => None,
        }
    }
    /// The first 16 bytes of a copyable const as two words, for inline materialization.
    /// repr(u8) puts each payload at its own alignment: Bool's byte sits in word0 at offset 1,
    /// Num's f64 fills word1 (offset 8).
    pub(crate) fn jit_const_bits(&self, k: u32) -> (u64, u64) {
        match &self.consts[k as usize] {
            Value::Undefined => (0, 0),
            Value::Null => (2, 0),
            Value::Bool(b) => (3 | ((*b as u64) << 8), 0),
            Value::Num(n) => (4, n.to_bits()),
            _ => unreachable!("non-copyable const in jit_const_bits"),
        }
    }
    pub(crate) fn jit_make_run_env(
        &self,
        i: &mut Interp,
        env: &Env,
        this_val: &Value,
        args: &[Value],
    ) -> Env {
        self.make_run_env(i, env, this_val, args)
    }
    /// (pops, pushes) of the op at `pc`, for the static stack-depth analysis. `None` = an op the
    /// JIT can't account for (which refuses compilation).
    pub(crate) fn jit_stack_effect(&self, pc: usize) -> Option<(usize, usize)> {
        // AssignTarget enters the normative tree-walker with a projected Environment Record.
        // Keep that uncommon bridge in the bytecode VM: re-entering arbitrary evaluator code
        // from a packed ARM64 frame is both unnecessary for coroutine conversion and unsafe for
        // JIT ownership assumptions. The rest of the function still avoids the tree-walker and,
        // crucially, coroutine bodies remain heap-owned rather than native-thread-backed.
        if matches!(
            self.ops[pc],
            Op::AssignTarget(_)
                | Op::EvalExpr(_)
                | Op::PushWith
                | Op::PushLex(_)
                | Op::PushCatchLex(_)
                | Op::CloneLex(_)
                | Op::InitLex(_)
                | Op::PopEnv
                | Op::ResolveNameRef(..)
                | Op::LoadRef(_)
                | Op::StoreRef(_)
                | Op::PushDisposeFrame
                | Op::AddDisposable(_)
                | Op::DisposeNormal
                | Op::DisposeThrow
                | Op::DisposeReturn
                | Op::DisposeBareReturn
                | Op::DisposeResumeReturn
                | Op::DisposeJump
                | Op::StoreConstLocal(..)
                | Op::StoreConstCap(_)
                | Op::UpdateConst(..)
                | Op::RequireObject
                | Op::DestructureStepL(..)
                | Op::DestructureRestL(..)
                | Op::IterCloseIfNotDoneL(..)
                | Op::IterAbortIfNotDoneL(..)
                | Op::ObjectRest(_)
                | Op::AsyncIterStepL(..)
                | Op::AsyncIterCloseL(..)
                | Op::NewArray
                | Op::ArrayPush
                | Op::ArrayHole
                | Op::ArraySpread
                | Op::CallArgsArray
                | Op::CallArgsArrayThis
                | Op::EvalCallArgsArray
                | Op::NewArgsArray
                | Op::NewObject
                | Op::ObjectData(_)
                | Op::ObjectSpread
                | Op::ObjectProto
                | Op::ObjectMethod(..)
                | Op::ImportMeta
                | Op::NewTarget
                | Op::DynamicImport(..)
                | Op::PrivateIn(_)
                | Op::GetPrivate(_)
                | Op::GetPrivateKeep(_)
                | Op::GetPrivateMethod(_)
                | Op::SetPrivate(_)
                | Op::UpdatePrivate(..)
                | Op::SuperCallStart
                | Op::SuperCallArgsArray
                | Op::LoadLexicalThis
                | Op::SuperThis
                | Op::SuperBase
                | Op::SuperGet
                | Op::SuperGetKeep
                | Op::SuperGetMethod
                | Op::SuperSet
                | Op::SuperUpdate(_)
                | Op::TemplateObject(_)
                | Op::RequireCallable
                | Op::ClassStart(..)
                | Op::ClassHeritage(..)
                | Op::ClassKey(..)
                | Op::ClassDecorator(..)
                | Op::ClassFinish(_)
                | Op::ClassAbort(_)
        ) {
            return None;
        }
        let upd = |k: &UpdKind| match k {
            UpdKind::IncDiscard | UpdKind::DecDiscard => 0,
            _ => 1,
        };
        Some(match &self.ops[pc] {
            Op::Const(_)
            | Op::Undef
            | Op::LoadLocal(_)
            | Op::LoadCap(_)
            | Op::LoadName(..)
            | Op::LoadThis
            | Op::LoadLexicalThis
            | Op::MakeClosure(..) => (0, 1),
            Op::Dup => (1, 2),
            Op::Dup2 => (2, 4),
            Op::Pop
            | Op::StoreLocal(_)
            | Op::StoreCap(_)
            | Op::StoreCapInit(_)
            | Op::StoreName(_)
            | Op::StoreNameCached(..)
            | Op::StoreConstLocal(..)
            | Op::StoreConstCap(_)
            | Op::UpdateConst(..) => (1, 0),
            Op::UpdateLocal(_, k) | Op::UpdateCap(_, k) | Op::UpdateName(_, k) => (0, upd(k)),
            Op::UpdateNameCached(_, _, k) => (0, upd(k)),
            Op::UpdateProp(_, _, k) => (1, upd(k)),
            Op::UpdateElem(k) => (2, upd(k)),
            Op::Tdz(_) => (0, 0),
            Op::GetProp(..) | Op::RequireObject => (1, 1),
            Op::GetPropThis(..) => (0, 1),
            Op::GetPropLocal(..) => (0, 1),
            Op::SetProp(..) => (2, 1),
            Op::SetPropDrop(..) => (2, 0),
            Op::SetPropThisDrop(..) => (1, 0),
            Op::SetPropLocalDrop(..) => (1, 0),
            Op::GetElem => (2, 1),
            Op::SetElem => (3, 1),
            Op::SetElemDrop => (3, 0),
            Op::AppendProp(..) => (3, 0),
            Op::DestructureGuard => (1, 1),
            Op::DestructureArr(n) => (1, *n as usize),
            Op::AssignTarget(_) => (1, 0),
            Op::EvalExpr(_) => (0, 1),
            Op::ClassStart(_, count) => (*count as usize * 2, 0),
            Op::ClassAbort(_) => (0, 0),
            Op::ClassHeritage(_, present) => (usize::from(*present), 0),
            Op::ClassKey(..) => (1, 0),
            Op::ClassDecorator(..) => (2, 0),
            Op::ClassFinish(_) => (0, 1),
            Op::ResolveNameRef(..) => (0, 0),
            Op::LoadRef(_) => (0, 1),
            Op::StoreRef(_) => (1, 0),
            Op::PushWith => (1, 0),
            Op::PushLex(_) | Op::PushCatchLex(_) | Op::CloneLex(_) => (0, 0),
            Op::InitLex(_) => (1, 0),
            Op::PopEnv => (0, 0),
            Op::PushDisposeFrame | Op::DisposeNormal => (0, 0),
            Op::AddDisposable(_) => (1, 1),
            Op::DisposeThrow | Op::DisposeReturn | Op::DisposeResumeReturn => (1, 0),
            Op::DisposeBareReturn => (0, 0),
            Op::DisposeJump => (2, 0),
            Op::ObjectRest(count) => (*count as usize + 1, 1),
            Op::DeleteProp(..) => (1, 1),
            Op::DeleteElem(_) => (2, 1),
            Op::DeleteName(_) => (0, 1),
            Op::DeleteSuper => (0, 0),
            Op::CallSpread(argc) => (*argc as usize + 1, 1),
            Op::CallSpreadThis(argc) => (*argc as usize + 2, 1),
            Op::CallArgsArray => (2, 1),
            Op::CallArgsArrayThis => (3, 1),
            Op::EvalCallArgsArray => (3, 1),
            Op::ToStr => (1, 1),
            Op::GetIter => (1, 2),
            Op::GetAsyncIter => (1, 3),
            Op::ForInKeys => (1, 1),
            Op::ForInStepL(..) => (0, 2),
            Op::IterStepL(..) => (0, 2),
            Op::IterCloseL(_) => (0, 0),
            Op::IterAbortL(_) => (1, 0),
            Op::DestructureStepL(..) | Op::DestructureRestL(..) => (0, 1),
            Op::IterCloseIfNotDoneL(..) => (0, 0),
            Op::IterAbortIfNotDoneL(..) => (1, 0),
            Op::AsyncIterStepL(..) => (0, 1),
            Op::AsyncIterResumeL(..) => (1, 2),
            Op::AsyncIterCloseL(..) => (0, 0),
            Op::GetElemLocal(_) => (1, 1),
            Op::SetElemLocal(_) => (2, 1),
            Op::SetElemLocalDrop(_) => (2, 0),
            Op::ToPropKey => (2, 2),
            Op::ToPropKeyLocal(_) => (1, 1),
            Op::GetMethod(..) => (1, 2),
            Op::GetMethodElem => (2, 2),
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Mod
            | Op::BitAnd
            | Op::BitOr
            | Op::BitXor
            | Op::Shl
            | Op::Shr
            | Op::UShr
            | Op::Lt
            | Op::Gt
            | Op::Le
            | Op::Ge
            | Op::EqEq
            | Op::NotEq
            | Op::StrictEq
            | Op::StrictNotEq
            | Op::InstanceOf(_)
            | Op::GenBin(_) => (2, 1),
            Op::Neg | Op::Plus | Op::Not | Op::BitNot | Op::Typeof | Op::Void => (1, 1),
            Op::TypeofName(_) => (0, 1),
            Op::Jump(_) | Op::AbruptJump(..) => (0, 0),
            Op::InlineGuard(..) => (0, 0),
            Op::ResetSlots(..) => (0, 0),
            Op::JumpIfFalse(_) => (1, 0),
            Op::JumpIfFalsePeek(_) | Op::JumpIfTruePeek(_) | Op::JumpIfNotNullishPeek(_) => (1, 1),
            Op::Call(argc, _) => (*argc as usize + 1, 1),
            Op::LoadNameForCall(..) => (0, 2),
            Op::CallWithThis(argc, _) => (*argc as usize + 2, 1),
            Op::New(argc, _) => (*argc as usize + 1, 1),
            Op::NewArgsArray => (2, 1),
            Op::MakeRegExp(..) => (0, 1),
            Op::MakeArray(n) => (*n as usize, 1),
            Op::NewArray => (0, 1),
            Op::ArrayPush | Op::ArraySpread => (2, 1),
            Op::ArrayHole => (1, 1),
            Op::MakeObject(_, count, _) => (*count as usize, 1),
            Op::NewObject => (0, 1),
            Op::ObjectData(_) => (3, 1),
            Op::ObjectSpread | Op::ObjectProto | Op::ObjectMethod(..) => (2, 1),
            Op::ImportMeta | Op::NewTarget | Op::TemplateObject(_) => (0, 1),
            Op::DynamicImport(_, has_options) => (usize::from(*has_options) + 1, 1),
            Op::PrivateIn(_) => (1, 1),
            Op::GetPrivate(_) => (1, 1),
            Op::GetPrivateKeep(_) | Op::GetPrivateMethod(_) => (1, 2),
            Op::SetPrivate(_) => (2, 1),
            Op::UpdatePrivate(_, kind) => (1, upd(kind)),
            Op::SuperCallStart => (0, 2),
            Op::SuperCallArgsArray => (3, 1),
            Op::SuperThis | Op::SuperBase => (0, 1),
            Op::SuperGet => (3, 1),
            Op::SuperGetKeep => (3, 4),
            Op::SuperGetMethod => (3, 2),
            Op::SuperSet => (4, 1),
            Op::SuperUpdate(kind) => (3, upd(kind)),
            Op::RequireCallable => (0, 0),
            Op::Throw | Op::Return | Op::ResumeReturn => (1, 0),
            Op::ResumeJump => (2, 0),
            Op::ReturnBare | Op::ReturnUndef => (0, 0),
            Op::Await | Op::Yield | Op::YieldStar => (1, 1),
            Op::PushHandler(_) | Op::PushFinally(..) | Op::PushIterator(..) | Op::PopHandler => {
                (0, 0)
            }
        })
    }
}

/// Execute the single (non-control-flow) op at `pc` against the raw operand stack `sp`. Returns
/// the updated stack top (reflecting any operands consumed *even on a throw* — the unwinder's
/// cleanup must never re-drop moved-out slots) plus a flag: 1 = threw (stored in `ctx.error`).
///
/// # Safety
/// Called from JIT code with `ctx` pointing at the live `JitCtx` for this activation and `sp`
/// inside its stack buffer, whose capacity covers the chunk's statically-computed maximum depth.
pub(crate) unsafe extern "C" fn jit_exec(
    ctx: *mut crate::jit::JitCtx,
    pc: u32,
    mut sp: *mut Value,
) -> crate::jit::SpFlag {
    // Packed local misses must remain O(1): widening every slot for a single object overwrite
    // dominates object-heavy kernels. These two ownership operations need no interpreter state;
    // TDZ loads retain the generic path so it can construct the precise ReferenceError.
    if unsafe { (*ctx).slots_packed } {
        let chunk = unsafe { &*(*ctx).chunk };
        let op = &chunk.ops[pc as usize];
        match *op {
            Op::LoadLocal(slot) => {
                let word = unsafe { (*ctx).slots.cast::<u64>().add(slot as usize) };
                let value = unsafe { crate::value::PackedValue::clone_raw(word) };
                if !matches!(value, Value::Empty) {
                    unsafe { sp.write(value) };
                    return crate::jit::SpFlag {
                        sp: unsafe { sp.add(1) },
                        flag: 0,
                    };
                }
            }
            Op::StoreLocal(slot) => {
                sp = unsafe { sp.sub(1) };
                let value = unsafe { sp.read() };
                let word = unsafe { (*ctx).slots.cast::<u64>().add(slot as usize) };
                unsafe { crate::value::PackedValue::replace_raw(word, value) };
                return crate::jit::SpFlag { sp, flag: 0 };
            }
            _ => {}
        }
    }
    let chunk = unsafe { &*(*ctx).chunk };
    let needs_wide_slots = matches!(
        chunk.ops[pc as usize],
        Op::LoadLocal(_)
            | Op::StoreLocal(_)
            | Op::UpdateLocal(..)
            | Op::Tdz(_)
            | Op::GetPropLocal(..)
            | Op::SetPropLocalDrop(..)
            | Op::GetElemLocal(_)
            | Op::SetElemLocal(_)
            | Op::SetElemLocalDrop(_)
            | Op::ToPropKeyLocal(_)
            | Op::ForInStepL(..)
            | Op::IterStepL(..)
            | Op::IterCloseL(_)
            | Op::IterAbortL(_)
            | Op::AssignTarget(_)
            | Op::ResetSlots(..)
    );
    let _wide_slots = if needs_wide_slots {
        Some(unsafe { JitWideSlots::enter(ctx) })
    } else {
        None
    };
    let ctx = &mut *ctx;
    jit_opstat(ctx, pc);
    match jit_exec_inner(ctx, pc, &mut sp) {
        Ok(()) => crate::jit::SpFlag { sp, flag: 0 },
        Err(ab) => {
            ctx.error = Some(ab);
            crate::jit::SpFlag { sp, flag: 1 }
        }
    }
}

/// Drop the single `Value` at `sp` (rare path: the direct-call sequence's callee slot when its
/// refcount hits zero, or any slot the inline decrement can't handle).
pub(crate) unsafe extern "C" fn jit_drop_at(
    ctx: *mut crate::jit::JitCtx,
    _imm: u32,
    sp: *mut Value,
) -> *mut Value {
    let ctx = &mut *ctx;
    let addr = sp as usize;
    let slots = ctx.slots as usize;
    if ctx.slots_packed && addr >= slots && addr < slots + ctx.n_slots * 8 {
        crate::value::PackedValue::drop_raw(sp.cast::<u64>());
    } else {
        std::ptr::drop_in_place(sp);
    }
    sp
}

/// Drop one NaN-boxed property owner at `word`. Generated stores use this only after all
/// observable guards pass; packed destruction cannot execute JavaScript.
pub(crate) unsafe extern "C" fn jit_drop_packed_at(
    _ctx: *mut crate::jit::JitCtx,
    _imm: u32,
    word: *mut Value,
) -> *mut Value {
    unsafe { crate::value::PackedValue::drop_raw(word.cast::<u64>()) };
    word
}

/// Strict equality fallback for cases intentionally left out of the generated fast path:
/// equal-length string content, BigInts, and operands whose last owner must run a destructor.
/// This operation cannot throw, so it can bypass the generic bytecode decoder and stack/slot
/// adaptation while retaining ordinary `Value` ownership semantics.
pub(crate) unsafe extern "C" fn jit_strict_eq(
    ctx: *mut crate::jit::JitCtx,
    pc: u32,
    sp: *mut Value,
) -> crate::jit::SpFlag {
    let ctx = unsafe { &mut *ctx };
    let chunk = unsafe { &*ctx.chunk };
    let op = &chunk.ops[pc as usize];
    debug_assert!(matches!(op, Op::StrictEq | Op::StrictNotEq));
    let base = unsafe { sp.sub(2) };
    let left = unsafe { base.read() };
    let right = unsafe { base.add(1).read() };
    let mut equal = unsafe { (&*ctx.interp).strict_equals(&left, &right) };
    if matches!(op, Op::StrictNotEq) {
        equal = !equal;
    }
    unsafe { base.write(Value::Bool(equal)) };
    crate::jit::SpFlag {
        sp: unsafe { base.add(1) },
        flag: 0,
    }
}

/// Allocate a fresh RegExp literal wrapper from the chunk's per-site compiled matcher.
pub(crate) unsafe extern "C" fn jit_make_regexp(
    ctx: *mut crate::jit::JitCtx,
    pc: u32,
    sp: *mut Value,
) -> crate::jit::SpFlag {
    let ctx = unsafe { &mut *ctx };
    let chunk = unsafe { &*ctx.chunk };
    let Op::MakeRegExp(body, flags) = chunk.ops[pc as usize] else {
        return unsafe { jit_exec(ctx, pc, sp) };
    };
    match chunk.make_regexp_literal(unsafe { &mut *ctx.interp }, pc as usize, body, flags) {
        Ok(value) => {
            observe_allocation(
                &chunk.feedback,
                pc as usize,
                crate::feedback::AllocationObjectKind::RegExp,
                chunk.names[body as usize]
                    .len()
                    .saturating_add(chunk.names[flags as usize].len()),
            );
            unsafe { sp.write(value) };
            crate::jit::SpFlag {
                sp: unsafe { sp.add(1) },
                flag: 0,
            }
        }
        Err(abrupt) => {
            ctx.error = Some(abrupt);
            crate::jit::SpFlag { sp, flag: 1 }
        }
    }
}

/// Fast `String + String` after the generated-code tag guards. Moving the left operand out of
/// the stack lets a temporary concatenation grow in place; otherwise the replacement reserves
/// spare capacity so a following concatenation can do so. This is the ordinary string-add
/// operation, independent of source or benchmark identity.
pub(crate) unsafe extern "C" fn jit_add_strings(
    ctx: *mut crate::jit::JitCtx,
    pc: u32,
    sp: *mut Value,
) -> crate::jit::SpFlag {
    let base = unsafe { sp.sub(2) };
    if !matches!(unsafe { &*base }, Value::Str(_))
        || !matches!(unsafe { &*base.add(1) }, Value::Str(_))
    {
        return unsafe { jit_exec(ctx, pc, sp) };
    }

    let Value::Str(mut left) = (unsafe { base.read() }) else {
        unreachable!()
    };
    let Value::Str(right) = (unsafe { base.add(1).read() }) else {
        unreachable!()
    };
    if left.len().saturating_add(right.len()) > crate::interpreter::MAX_STR_LEN {
        let ctx = unsafe { &mut *ctx };
        let i = unsafe { &mut *ctx.interp };
        ctx.error = Some(i.throw("RangeError", "Invalid string length"));
        return crate::jit::SpFlag { sp: base, flag: 1 };
    }

    if crate::jstr::needs_join_fixup(&left, &right) {
        left = crate::jstr::concat(&left, &right).into();
    } else if !left.append_in_place(&right) {
        left = left.concat_grown(&right);
    }
    unsafe { base.write(Value::Str(left)) };
    crate::jit::SpFlag {
        sp: unsafe { base.add(1) },
        flag: 0,
    }
}

/// Hot native intrinsics after the machine-code call IC has already proved builtin identity.
/// Emitted guards keep string/property intrinsics on non-coercing cases. Function#apply performs
/// its own dense-list guards; Array push/pop transfer ownership directly between the operand
/// stack and dense storage. Every guard miss invokes the exact builtin implementation.
pub(crate) unsafe extern "C" fn jit_intrinsic(
    ctx: *mut crate::jit::JitCtx,
    packed: u32,
    sp: *mut Value,
) -> crate::jit::SpFlag {
    let ctx = &mut *ctx;
    let i = &mut *ctx.interp;
    let intrinsic = (packed >> 16) as u8;
    let call_argc = (packed >> 24) as usize;
    let _pc = (packed & 0xffff) as usize;
    let width = match intrinsic {
        INTRINSIC_CHAR_AT => 3,    // [receiver, callee, index]
        INTRINSIC_ARRAY_PUSH => 3, // [receiver, callee, arg]
        INTRINSIC_ARRAY_POP => 2,  // [receiver, callee]
        INTRINSIC_FUNCTION_CALL => call_argc + 2,
        INTRINSIC_REGEXP_EXEC_DISCARD => 3,
        INTRINSIC_STRING_SPLIT_DISCARD => 3,
        _ => 4, // [receiver, callee, arg0, arg1]
    };
    let base = sp.sub(width);
    let mut this_moved = false;
    let mut push_arg_moved = false;
    let mut call_args_moved = 0usize;
    i.depth += 1;
    #[cfg(target_arch = "wasm32")]
    let exhausted = i.depth > crate::interpreter::WASM_EXECUTION_DEPTH_GUARD;
    #[cfg(not(target_arch = "wasm32"))]
    let exhausted = false;
    let r: Result<Value, Abrupt> = if exhausted {
        Err(i.throw("RangeError", "Maximum call stack size exceeded"))
    } else {
        crate::interpreter::with_execution_stack(i.depth, || {
            i.gc_check_amortized().and_then(|()| {
                // Match `call_native_committed`: native code and any observable fallback it invokes
                // run outside a pending construction. Dense push/pop themselves cannot observe
                // these fields, but a prototype setter or unusual array-like on the miss path can.
                let saved_ctor = std::mem::replace(&mut i.constructing, false);
                let saved_nt = std::mem::replace(&mut i.new_target, Value::Undefined);
                let r = match intrinsic {
                    INTRINSIC_CHAR_AT => {
                        let Value::Str(s) = &*base else {
                            unreachable!("charAt intrinsic receiver guard")
                        };
                        let Value::Num(n) = &*base.add(2) else {
                            unreachable!("charAt intrinsic index guard")
                        };
                        let idx = if n.is_nan() { 0.0 } else { n.trunc() };
                        Ok(if idx < 0.0 || !idx.is_finite() {
                            Value::str("")
                        } else {
                            match i.unit_at(s, idx as usize) {
                                Some(unit) => Value::Str(crate::jstr::unit_lstr(unit)),
                                None => Value::str(""),
                            }
                        })
                    }
                    INTRINSIC_STRING_SLICE => {
                        let Value::Str(s) = &*base else {
                            unreachable!("slice intrinsic receiver guard")
                        };
                        let Value::Num(start) = &*base.add(2) else {
                            unreachable!("slice intrinsic start guard")
                        };
                        let Value::Num(end) = &*base.add(3) else {
                            unreachable!("slice intrinsic end guard")
                        };
                        debug_assert!(s.ascii_hint());
                        let len = s.len() as i64;
                        let norm = |n: f64| {
                            if n.is_nan() {
                                return 0;
                            }
                            let n = if n.is_infinite() {
                                if n > 0.0 {
                                    len
                                } else {
                                    -len - 1
                                }
                            } else {
                                n as i64
                            };
                            if n < 0 {
                                (len + n).max(0)
                            } else {
                                n.min(len)
                            }
                        };
                        let (start, end) = (norm(*start), norm(*end));
                        Ok(if start < end {
                            Value::str(&s[start as usize..end as usize])
                        } else {
                            Value::str("")
                        })
                    }
                    INTRINSIC_OBJECT_HAS_OWN => {
                        let Value::Obj(o) = &*base.add(2) else {
                            unreachable!("hasOwn intrinsic object guard")
                        };
                        let Value::Str(key) = &*base.add(3) else {
                            unreachable!("hasOwn intrinsic key guard")
                        };
                        Ok(Value::Bool(o.borrow().props.contains(key.as_str())))
                    }
                    INTRINSIC_FUNCTION_CALL => {
                        // `target.call(thisArg, arg)`: transfer thisArg + the single forwarded argument
                        // directly into a compiled target frame. A failed applicability probe has no
                        // side effects, so proxies, native targets, and unusual closures invoke the
                        // exact Function.prototype.call builtin.
                        debug_assert!(call_argc >= 1);
                        let forwarded = call_argc - 1;
                        match i.call_jit_fast(&*base, base.add(2), base.add(3), forwarded, None) {
                            Some(r) => {
                                this_moved = true;
                                call_args_moved = forwarded;
                                r
                            }
                            None => crate::builtins::nf_function_call(
                                i,
                                (*base).clone(),
                                std::slice::from_raw_parts(base.add(2), call_argc),
                            )
                            .map_err(Abrupt::Throw),
                        }
                    }
                    INTRINSIC_FUNCTION_APPLY => {
                        // The dominant `initialize.apply(this, arguments)` shape has an unmapped,
                        // own-dense arguments object. Clone its entries once, then move them straight
                        // into an already-JIT-compiled target frame. No observable operation occurs
                        // before every list guard has passed; unusual array-likes and non-JIT targets
                        // execute the named builtin unchanged.
                        let dense = match (&*base, &*base.add(3)) {
                            (Value::Obj(_), Value::Obj(list))
                                if i.ordinary_get_ptr(Rc::as_ptr(list) as usize)
                                    && !i
                                        .mapped_arguments
                                        .contains_key(&(Rc::as_ptr(list) as usize)) =>
                            {
                                let b = list.borrow();
                                let len = match b.props.get("length") {
                                    Some(p) if !p.accessor() => match p.value() {
                                        Value::Num(n)
                                            if n >= 0.0
                                                && n.is_finite()
                                                && n.fract() == 0.0
                                                && n <= crate::interpreter::MAX_ARRAY_OP_LEN
                                                    as f64 =>
                                        {
                                            Some(n as usize)
                                        }
                                        _ => None,
                                    },
                                    _ => None,
                                };
                                len.and_then(|len| {
                                    let mut values = Vec::with_capacity(len);
                                    for k in 0..len {
                                        let value = b
                                            .props
                                            .get_index(k as u32)
                                            .filter(|p| !p.accessor())
                                            .map(|p| p.value())?;
                                        values.push(value);
                                    }
                                    Some(values)
                                })
                            }
                            _ => None,
                        };
                        if let Some(mut values) = dense {
                            match i.call_jit_fast(
                                &*base,
                                base.add(2),
                                values.as_mut_ptr(),
                                values.len(),
                                None,
                            ) {
                                Some(r) => {
                                    // `call_jit_fast` moved every Vec element and the original thisArg.
                                    values.set_len(0);
                                    this_moved = true;
                                    r
                                }
                                None => crate::builtins::nf_function_apply(
                                    i,
                                    (*base).clone(),
                                    std::slice::from_raw_parts(base.add(2), 2),
                                )
                                .map_err(Abrupt::Throw),
                            }
                        } else {
                            crate::builtins::nf_function_apply(
                                i,
                                (*base).clone(),
                                std::slice::from_raw_parts(base.add(2), 2),
                            )
                            .map_err(Abrupt::Throw)
                        }
                    }
                    INTRINSIC_ARRAY_PUSH => {
                        let Value::Obj(o) = &*base else {
                            unreachable!("push intrinsic receiver guard")
                        };
                        let arg = base.add(2).read();
                        push_arg_moved = true;
                        match crate::builtins::jit_array_push_one(i, o, arg) {
                            Ok(v) => Ok(v),
                            Err(arg) => {
                                // The transfer helper promises a guard miss has no side effects and
                                // returns the original owner, so the generic builtin sees the exact
                                // operand stack it would have seen without specialization.
                                base.add(2).write(arg);
                                push_arg_moved = false;
                                crate::builtins::nf_array_push(
                                    i,
                                    (*base).clone(),
                                    std::slice::from_raw_parts(base.add(2), 1),
                                )
                                .map_err(Abrupt::Throw)
                            }
                        }
                    }
                    INTRINSIC_ARRAY_POP => {
                        let Value::Obj(o) = &*base else {
                            unreachable!("pop intrinsic receiver guard")
                        };
                        match crate::builtins::jit_array_pop(i, o) {
                            Some(v) => Ok(v),
                            None => crate::builtins::nf_array_pop(i, (*base).clone(), &[])
                                .map_err(Abrupt::Throw),
                        }
                    }
                    INTRINSIC_REGEXP_EXEC_DISCARD => {
                        let (Value::Obj(_), Value::Str(input)) = (&*base, &*base.add(2)) else {
                            unreachable!("regexp exec intrinsic guards")
                        };
                        match crate::builtins::regexp_exec_discard_fast(i, &*base, input) {
                            Some(r) => {
                                i.interrupt_poll_force()?;
                                r.map(|_| Value::Undefined).map_err(Abrupt::Throw)
                            }
                            None => crate::builtins::regexp_exec(
                                i,
                                (*base).clone(),
                                std::slice::from_raw_parts(base.add(2), 1),
                            )
                            .map_err(Abrupt::Throw),
                        }
                    }
                    INTRINSIC_STRING_REPLACE_DISCARD => {
                        match crate::builtins::string_replace_discard_fast(
                            i,
                            &*base,
                            &*base.add(2),
                            &*base.add(3),
                        ) {
                            Some(r) => {
                                i.interrupt_poll_force()?;
                                r.map_err(Abrupt::Throw)
                            }
                            None => crate::builtins::nf_string_replace(
                                i,
                                (*base).clone(),
                                std::slice::from_raw_parts(base.add(2), 2),
                            )
                            .map_err(Abrupt::Throw),
                        }
                    }
                    INTRINSIC_STRING_SPLIT_DISCARD => {
                        match crate::builtins::string_split_discard_fast(
                            i,
                            &*base,
                            &*base.add(2),
                            &Value::Undefined,
                        ) {
                            Some(r) => r.map_err(Abrupt::Throw),
                            None => crate::builtins::nf_string_split(
                                i,
                                (*base).clone(),
                                std::slice::from_raw_parts(base.add(2), 1),
                            )
                            .map_err(Abrupt::Throw),
                        }
                    }
                    _ => unreachable!("unknown JIT intrinsic"),
                };
                i.constructing = saved_ctor;
                i.new_target = saved_nt;
                r
            })
        })
    };
    i.depth -= 1;
    // Every operand is still owned by the caller stack. Drop after the result has finished
    // borrowing any receiver/key storage, then reuse the receiver slot.
    for k in 0..width {
        if (!this_moved || k != 2)
            && (!push_arg_moved || k != 2)
            && (call_args_moved == 0 || k < 3 || k >= 3 + call_args_moved)
        {
            std::ptr::drop_in_place(base.add(k));
        }
    }
    match r {
        Ok(v) => {
            base.write(v);
            crate::jit::SpFlag {
                sp: base.add(1),
                flag: 0,
            }
        }
        Err(ab) => {
            ctx.error = Some(ab);
            crate::jit::SpFlag { sp: base, flag: 1 }
        }
    }
}

/// Batch the canonical `for (i=...; i<limit; i++) re.exec(strings[i]);` shape. The generated
/// code calls this before the ordinary loop header. A zero result declines without side effects,
/// one means the helper completed the loop and updated the index slot, and two propagates a throw.
pub(crate) unsafe extern "C" fn jit_regexp_exec_loop(
    ctx: *mut crate::jit::JitCtx,
    head: u32,
) -> u64 {
    macro_rules! decline {
        () => {{
            return 0;
        }};
    }
    let _wide_slots = unsafe { JitWideSlots::enter(ctx) };
    let ctx = unsafe { &mut *ctx };
    let i = unsafe { &mut *ctx.interp };
    let chunk = unsafe { &*ctx.chunk };
    let pc = head as usize;
    let Some(
        [Op::LoadLocal(local0), Op::Const(limit_const), Op::Lt, Op::JumpIfFalse(exit), Op::LoadName(re_name, re_cache), Op::GetMethod(exec_name, _), Op::LoadName(array_name, array_cache), Op::LoadLocal(local1), Op::GetElem, Op::CallWithThis(1, _), Op::Pop, Op::UpdateLocal(local2, UpdKind::IncDiscard), Op::Jump(back)],
    ) = chunk.ops.get(pc..pc + 13)
    else {
        decline!();
    };
    if local0 != local1
        || local0 != local2
        || *back != head
        || *exit as usize != pc + 13
        || chunk.names[*exec_name as usize].as_ref() != "exec"
    {
        decline!();
    }
    let Value::Num(limit_num) = chunk.consts[*limit_const as usize] else {
        decline!();
    };
    if !limit_num.is_finite()
        || limit_num < 0.0
        || limit_num.fract() != 0.0
        || limit_num > crate::interpreter::MAX_ARRAY_OP_LEN as f64
    {
        decline!();
    }
    let Value::Num(mut index_num) = *ctx.slots.add(*local0 as usize) else {
        decline!();
    };
    if !index_num.is_finite()
        || index_num < 0.0
        || index_num.fract() != 0.0
        || index_num > limit_num
    {
        decline!();
    }

    let env_ptr = ctx
        .env_raw
        .cast::<std::cell::RefCell<crate::interpreter::Scope>>();
    if env_ptr.is_null() {
        decline!();
    }
    unsafe { Rc::increment_strong_count(env_ptr) };
    let env = unsafe { Rc::from_raw(env_ptr) };
    let regexp = match chunk.load_name_ic(i, &env, *re_name, *re_cache) {
        Ok(v) => v,
        Err(ab) => {
            ctx.error = Some(ab);
            return 2;
        }
    };
    let strings = match chunk.load_name_ic(i, &env, *array_name, *array_cache) {
        Ok(v) => v,
        Err(ab) => {
            ctx.error = Some(ab);
            return 2;
        }
    };
    drop(env);

    // Prove the repeated GetMethod is the realm's untouched built-in RegExp#exec data property.
    let (Value::Obj(re_obj), Value::Obj(array_obj)) = (&regexp, &strings) else {
        decline!();
    };
    let Some(re_proto) = i.extra_protos.get("RegExp") else {
        return 0;
    };
    {
        let b = re_obj.borrow();
        if b.props.contains("exec")
            || b.proto.as_ref().is_none_or(|p| !Rc::ptr_eq(p, re_proto))
            || !i.regexps.contains_key(&(Rc::as_ptr(re_obj) as usize))
        {
            decline!();
        }
    }
    let canonical_exec = {
        let p = re_proto.borrow();
        match p.props.get("exec") {
            Some(prop) if !prop.accessor() => prop.value(),
            _ => decline!(),
        }
    };
    let Value::Obj(exec_obj) = canonical_exec else {
        decline!();
    };
    if !matches!(
        exec_obj.borrow().call,
        crate::value::Callable::Native(function)
            if function as usize == crate::builtins::regexp_exec as *const () as usize
    ) {
        decline!();
    }

    let regexp_matcher = match i.regexps.get(&(Rc::as_ptr(re_obj) as usize)) {
        Some(matcher) => matcher.clone(),
        None => decline!(),
    };
    {
        let b = re_obj.borrow();
        let Some(last_index) = b.props.get("lastIndex") else {
            decline!();
        };
        if last_index.accessor()
            || !last_index.writable()
            || !matches!(
                last_index.value(),
                Value::Num(n) if n.is_finite() && n >= 0.0 && n.fract() == 0.0
            )
        {
            decline!();
        }
    }

    let limit = limit_num as usize;
    let start = index_num as usize;
    let array = array_obj.borrow();
    if !matches!(array.exotic, crate::value::Exotic::Array) {
        decline!();
    }
    for k in start..limit {
        let Some(prop) = array.props.get_index(k as u32) else {
            decline!();
        };
        if prop.accessor() {
            decline!();
        }
        let Value::Str(subject) = prop.value() else {
            decline!();
        };
        match crate::builtins::regexp_exec_discard_direct(i, re_obj, &regexp_matcher, &subject) {
            Ok(_) => {}
            Err(v) => {
                ctx.error = Some(match i.interrupt_poll_force() {
                    Err(interrupt) => interrupt,
                    Ok(()) => Abrupt::Throw(v),
                });
                return 2;
            }
        }
        index_num += 1.0;
    }
    *ctx.slots.add(*local0 as usize) = Value::Num(index_num);
    1
}

/// Fuse a dead-result `/<literal>/.exec(strings[i])` or `/<literal>/.exec("<constant>")`
/// expression. The helper runs at the literal's original evaluation point, validates the live
/// inherited `exec` method before loading the argument, and declines without side effects when
/// any ordinary-array/name guard is unavailable.
/// Return: 0 = decline, 1 = completed expression, 2 = throw in `ctx.error`.
pub(crate) unsafe extern "C" fn jit_regexp_literal_exec_discard(
    ctx: *mut crate::jit::JitCtx,
    literal_pc: u32,
) -> u64 {
    macro_rules! decline {
        () => {{
            return 0;
        }};
    }
    let _wide_slots = unsafe { JitWideSlots::enter(ctx) };
    let ctx = unsafe { &mut *ctx };
    let i = unsafe { &mut *ctx.interp };
    let chunk = unsafe { &*ctx.chunk };
    let pc = literal_pc as usize;
    enum Subject {
        DenseArray(u32, u32, u32),
        Constant(u32),
    }
    let (body, flags, exec, subject) = match chunk.ops.get(pc..) {
        Some(
            [Op::MakeRegExp(body, flags), Op::GetMethod(exec, _), Op::LoadName(array, array_cache), Op::LoadLocal(index), Op::GetElem, Op::CallWithThis(1, _), Op::Pop, ..],
        ) => (
            *body,
            *flags,
            *exec,
            Subject::DenseArray(*array, *array_cache, u32::from(*index)),
        ),
        Some(
            [Op::MakeRegExp(body, flags), Op::GetMethod(exec, _), Op::Const(subject), Op::CallWithThis(1, _), Op::Pop, ..],
        ) => (*body, *flags, *exec, Subject::Constant(*subject)),
        _ => decline!(),
    };
    if chunk.names[exec as usize].as_ref() != "exec"
        || !crate::builtins::regexp_literal_exec_is_canonical(i)
    {
        decline!();
    }

    let subject = match subject {
        Subject::Constant(index) => {
            let Value::Str(subject) = &chunk.consts[index as usize] else {
                decline!();
            };
            subject.clone()
        }
        Subject::DenseArray(array, array_cache, index) => {
            let env_ptr = ctx
                .env_raw
                .cast::<std::cell::RefCell<crate::interpreter::Scope>>();
            if env_ptr.is_null() {
                decline!();
            }
            unsafe { Rc::increment_strong_count(env_ptr) };
            let env = unsafe { Rc::from_raw(env_ptr) };
            let array_value = match chunk.load_name_ic(i, &env, array, array_cache) {
                Ok(value) => value,
                Err(abrupt) => {
                    ctx.error = Some(abrupt);
                    return 2;
                }
            };
            drop(env);
            let Value::Obj(array_obj) = array_value else {
                decline!();
            };
            if !i.ordinary_get_ptr(Rc::as_ptr(&array_obj) as usize) {
                decline!();
            }
            let Value::Num(index) = *ctx.slots.add(index as usize) else {
                decline!();
            };
            if !index.is_finite() || index < 0.0 || index.fract() != 0.0 || index > u32::MAX as f64
            {
                decline!();
            }
            let array = array_obj.borrow();
            if !matches!(array.exotic, crate::value::Exotic::Array) {
                decline!();
            }
            let Some(property) = array.props.get_index(index as u32) else {
                decline!();
            };
            if property.accessor() {
                decline!();
            }
            let Value::Str(subject) = property.value() else {
                decline!();
            };
            subject
        }
    };

    let re = match chunk.compiled_regexp_literal(i, pc, body, flags) {
        Ok(re) => re,
        Err(abrupt) => {
            ctx.error = Some(abrupt);
            return 2;
        }
    };
    match crate::builtins::regexp_literal_exec_discard(i, &re, &subject) {
        Ok(()) => 1,
        Err(abrupt) => {
            ctx.error = Some(abrupt);
            2
        }
    }
}

/// Fuse a dead-result `strings[i].replace(/<literal>/, "<constant>")` while preserving the
/// original dense subject read and every matcher/legacy-static effect. Return convention matches
/// [`jit_regexp_literal_exec_discard`].
pub(crate) unsafe extern "C" fn jit_regexp_literal_replace_discard(
    ctx: *mut crate::jit::JitCtx,
    start_pc: u32,
) -> u64 {
    macro_rules! decline {
        () => {{
            return 0;
        }};
    }
    let _wide_slots = unsafe { JitWideSlots::enter(ctx) };
    let ctx = unsafe { &mut *ctx };
    let i = unsafe { &mut *ctx.interp };
    let chunk = unsafe { &*ctx.chunk };
    let pc = start_pc as usize;
    let Some(
        [Op::LoadName(array, array_cache), Op::LoadLocal(index), Op::GetElem, Op::GetMethod(replace, _), Op::MakeRegExp(body, flags), Op::Const(replacement), Op::CallWithThis(2, _), Op::Pop],
    ) = chunk.ops.get(pc..pc + 8)
    else {
        decline!();
    };
    if chunk.names[*replace as usize].as_ref() != "replace"
        || !matches!(chunk.consts[*replacement as usize], Value::Str(_))
    {
        decline!();
    }

    // LoadName/GetElem precede GetMethod in the source evaluation order.
    let env_ptr = ctx
        .env_raw
        .cast::<std::cell::RefCell<crate::interpreter::Scope>>();
    if env_ptr.is_null() {
        decline!();
    }
    unsafe { Rc::increment_strong_count(env_ptr) };
    let env = unsafe { Rc::from_raw(env_ptr) };
    let array_value = match chunk.load_name_ic(i, &env, *array, *array_cache) {
        Ok(value) => value,
        Err(abrupt) => {
            ctx.error = Some(abrupt);
            return 2;
        }
    };
    drop(env);
    let Value::Obj(array_obj) = array_value else {
        decline!();
    };
    if !i.ordinary_get_ptr(Rc::as_ptr(&array_obj) as usize) {
        decline!();
    }
    let Value::Num(index) = *ctx.slots.add(*index as usize) else {
        decline!();
    };
    if !index.is_finite() || index < 0.0 || index.fract() != 0.0 || index > u32::MAX as f64 {
        decline!();
    }
    let subject = {
        let array = array_obj.borrow();
        if !matches!(array.exotic, crate::value::Exotic::Array) {
            decline!();
        }
        let Some(property) = array.props.get_index(index as u32) else {
            decline!();
        };
        if property.accessor() {
            decline!();
        }
        let Value::Str(subject) = property.value() else {
            decline!();
        };
        subject
    };

    // These raw identity reads occur at the original GetMethod/call boundary. An accessor or
    // override declines, letting the untouched bytecode perform all observable operations.
    if !crate::builtins::regexp_literal_replace_is_canonical(i) {
        decline!();
    }
    let re = match chunk.compiled_regexp_literal(i, pc + 4, *body, *flags) {
        Ok(re) => re,
        Err(abrupt) => {
            ctx.error = Some(abrupt);
            return 2;
        }
    };
    match crate::builtins::regexp_literal_replace_discard(i, &re, &subject) {
        Ok(()) => 1,
        Err(abrupt) => {
            ctx.error = Some(abrupt);
            2
        }
    }
}

/// Fuse a dead-result `strings[i].match(/<literal>/)` while preserving the subject read and every
/// matcher/legacy-static effect. Return convention matches
/// [`jit_regexp_literal_exec_discard`].
pub(crate) unsafe extern "C" fn jit_regexp_literal_match_discard(
    ctx: *mut crate::jit::JitCtx,
    start_pc: u32,
) -> u64 {
    macro_rules! decline {
        () => {{
            return 0;
        }};
    }
    let _wide_slots = unsafe { JitWideSlots::enter(ctx) };
    let ctx = unsafe { &mut *ctx };
    let i = unsafe { &mut *ctx.interp };
    let chunk = unsafe { &*ctx.chunk };
    let pc = start_pc as usize;
    let Some(
        [Op::LoadName(array, array_cache), Op::LoadLocal(index), Op::GetElem, Op::GetMethod(method, _), Op::MakeRegExp(body, flags), Op::CallWithThis(1, _), Op::Pop],
    ) = chunk.ops.get(pc..pc + 7)
    else {
        decline!();
    };
    if chunk.names[*method as usize].as_ref() != "match" {
        decline!();
    }

    // The subject evaluation precedes the method and literal evaluation.
    let env_ptr = ctx
        .env_raw
        .cast::<std::cell::RefCell<crate::interpreter::Scope>>();
    if env_ptr.is_null() {
        decline!();
    }
    unsafe { Rc::increment_strong_count(env_ptr) };
    let env = unsafe { Rc::from_raw(env_ptr) };
    let array_value = match chunk.load_name_ic(i, &env, *array, *array_cache) {
        Ok(value) => value,
        Err(abrupt) => {
            ctx.error = Some(abrupt);
            return 2;
        }
    };
    drop(env);
    let Value::Obj(array_obj) = array_value else {
        decline!();
    };
    if !i.ordinary_get_ptr(Rc::as_ptr(&array_obj) as usize) {
        decline!();
    }
    let Value::Num(index) = *ctx.slots.add(*index as usize) else {
        decline!();
    };
    if !index.is_finite() || index < 0.0 || index.fract() != 0.0 || index > u32::MAX as f64 {
        decline!();
    }
    let subject = {
        let array = array_obj.borrow();
        if !matches!(array.exotic, crate::value::Exotic::Array) {
            decline!();
        }
        let Some(property) = array.props.get_index(index as u32) else {
            decline!();
        };
        if property.accessor() {
            decline!();
        }
        let Value::Str(subject) = property.value() else {
            decline!();
        };
        subject
    };
    if !crate::builtins::regexp_literal_match_is_canonical(i) {
        decline!();
    }
    let re = match chunk.compiled_regexp_literal(i, pc + 4, *body, *flags) {
        Ok(re) => re,
        Err(abrupt) => {
            ctx.error = Some(abrupt);
            return 2;
        }
    };
    match crate::builtins::regexp_literal_match_discard(i, &re, &subject) {
        Ok(()) => 1,
        Err(abrupt) => {
            ctx.error = Some(abrupt);
            2
        }
    }
}

/// Teardown for a direct (shared-ctx) JIT→JIT call — the asm sequence already ran the callee
/// with the ctx fields swapped to the callee's frame; this drops whatever the callee left
/// (operand-stack range + slots, with the shared-reference decrement fast path), returns the
/// frame buffer to the pool, pops the FnFrame (including a materialized `extra`), drains a
/// pending tail call, and decrements the recursion depth. The caller's asm then restores the
/// swapped fields and pushes `ctx.ret` (still owned by ctx until the asm moves it out).
/// Returns 0 = ok (ret valid) / 1 = threw (ctx.error set).
///
/// # Safety
/// `ctx` must still hold the CALLEE's swapped frame fields (slots/stack_base/final_sp/n_slots),
/// with `ctx.slots` being the pooled buffer base.
pub(crate) unsafe extern "C" fn jit_direct_finish(
    ctx: *mut crate::jit::JitCtx,
    threw: u32,
    _sp: *mut Value,
) -> u64 {
    let ctx = &mut *ctx;
    // Returning from inside a try bypasses the lexical PopHandler. Direct calls share their
    // caller's handler allocation, so discard every record above this activation's watermark
    // before the caller resumes (ECMA-262 14.10.1 and 14.15.3).
    ctx.handlers.truncate(ctx.handler_floor);
    let caller_uses_packed_slots = ctx.slots_packed;
    // Expand the packed callee once for the existing destructor loop. Assembly restores the
    // caller's still-packed slot pointer immediately after this helper, so only the flag is
    // restored after the callee buffer has been released.
    ctx.unpack_slots();
    let i = &mut *ctx.interp;
    // Leftover operand stack (only on throw; clean returns leave it empty).
    let mut p = ctx.stack_base;
    while p < ctx.final_sp {
        std::ptr::drop_in_place(p);
        p = p.add(1);
    }
    // Slot drops with the shared-reference fast path (mirrors run_moved's exit loop).
    let rc_dec_ok = i
        .jit_layout
        .get()
        .is_some_and(|l| l.valid && l.rc_strong_off == 0);
    for k in 0..ctx.n_slots {
        let p = ctx.slots.add(k);
        let tag = *(p as *const u8);
        if tag < 5 {
            continue;
        }
        if rc_dec_ok && tag >= 6 {
            let strong = *(p as *const usize).add(1) as *mut usize;
            if *strong > 1 {
                *strong -= 1;
                continue;
            }
        }
        std::ptr::drop_in_place(p);
    }
    // Return the frame buffer (asm popped it from the freelist; base == ctx.slots).
    let buf = std::ptr::NonNull::new_unchecked(ctx.slots);
    if i.frame_pool.len() < 64 {
        i.frame_pool.0.push(buf);
    } else {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
            ctx.slots as *mut std::mem::MaybeUninit<Value>,
            crate::jit::FRAME_BUF,
        )));
    }
    // The callee's `this` binding lives in the shared ctx — drop it before the asm restores
    // the caller's value over it (a plain overwrite would leak a refcounted `this` per call).
    ctx.this_val = Value::Undefined;
    // FnFrame pop (the asm pushed it; a materialized `extra` drops here).
    if let Some(f) = i.fn_frames.pop() {
        drop(f.extra);
    }
    let mut threw = threw != 0;
    // Proper-tail-call trampoline, exactly like the layered paths.
    if !threw {
        while let Some(bx) = i.pending_tail.take() {
            let (f, t, a) = *bx;
            let r = i.gc_check_amortized().and_then(|()| i.call_inner(f, t, &a));
            match r {
                Ok(v) => ctx.ret = v,
                Err(e) => {
                    ctx.error = Some(e);
                    threw = true;
                    break;
                }
            }
        }
    }
    i.depth -= 1;
    ctx.slots_packed = caller_uses_packed_slots;
    threw as u64
}

/// The call template's inline-probe HIT entry: the emitted code already validated one way
/// (callee identity + epoch + realm — the way index rides in bits 16.. of `pc`, the pc itself
/// in the low 16), so this skips the probe loop — it re-reads that entry (nothing ran between
/// the machine-code compare and this call), bumps the recompile counter, and enters the
/// committed path directly. Same contract as [`jit_exec`].
pub(crate) unsafe extern "C" fn jit_call_hit(
    ctx: *mut crate::jit::JitCtx,
    pc: u32,
    mut sp: *mut Value,
) -> crate::jit::SpFlag {
    let ctx = &mut *ctx;
    let way = (pc >> 16) as usize & (CALL_IC_WAYS - 1);
    let pc = pc & 0xFFFF;
    jit_opstat(ctx, pc);
    let i = &mut *ctx.interp;
    let chunk = &*ctx.chunk;
    // Detailed profiling disables the inline probe at compile time, but keep this helper
    // semantically complete if a previously emitted chunk reaches it after instrumentation is
    // enabled. The profiled path must observe before dispatch and must not consume moved slots.
    if chunk.feedback.detailed_enabled() {
        return match jit_call_inner(ctx, pc, &mut sp) {
            Ok(()) => crate::jit::SpFlag { sp, flag: 0 },
            Err(ab) => {
                ctx.error = Some(ab);
                crate::jit::SpFlag { sp, flag: 1 }
            }
        };
    }
    let (argc, c, with_this) = match chunk.ops[pc as usize] {
        Op::CallWithThis(argc, c) => (argc as usize, c, true),
        Op::Call(argc, c) => (argc as usize, c, false),
        _ => unreachable!("jit_call_hit emitted only for call ops"),
    };
    let ic = chunk.call_caches[c as usize].entries[way].get();
    jit_callstat(i, ctx, &ic, argc, with_this, sp);
    if ic.native == 0 {
        let chunk_ref = &*ic.chunk;
        let runs = chunk_ref.jit_runs.get().wrapping_add(1);
        chunk_ref.jit_runs.set(runs);
        if runs == ctx.inline_recompile_at {
            i.try_inline_recompile(ic.func, chunk_ref, ic.env);
        }
    }
    let args_ptr = sp.sub(argc);
    let mut undef = std::mem::ManuallyDrop::new(Value::Undefined);
    let this_slot: *const Value = if with_this {
        sp.sub(argc + 2)
    } else {
        &raw mut *undef as *const Value
    };
    let r = if ic.native != 0 {
        let nf: crate::value::NativeFn = std::mem::transmute(ic.native);
        i.call_native_committed(nf, this_slot, args_ptr, argc)
    } else {
        i.call_jit_committed(ic, this_slot, args_ptr, argc)
    };
    // Arguments and `this` were moved; pop them virtually and drop only the callee slot
    // (same ownership story as jit_call_inner's Some arm).
    sp = args_ptr.sub(1);
    match sp.read() {
        Value::Obj(o) => {
            if Rc::strong_count(&o) > 1 {
                unsafe { Rc::decrement_strong_count(Rc::into_raw(o)) };
            } else {
                drop(o);
            }
        }
        other => drop(other),
    }
    if with_this {
        sp = sp.sub(1); // `this` was consumed (moved) by the callee — skip, don't drop
    }
    match r {
        Ok(v) => {
            sp.write(v);
            crate::jit::SpFlag {
                sp: sp.add(1),
                flag: 0,
            }
        }
        Err(ab) => {
            ctx.error = Some(ab);
            crate::jit::SpFlag { sp, flag: 1 }
        }
    }
}

/// Dedicated `Op::MakeObject` entry (same contract as [`jit_exec`]): clones the site's
/// pre-shaped map and moves the values straight off the operand stack — no generic op
/// dispatch, no intermediate `Vec`.
pub(crate) unsafe extern "C" fn jit_make_object(
    ctx: *mut crate::jit::JitCtx,
    pc: u32,
    mut sp: *mut Value,
) -> crate::jit::SpFlag {
    let ctx = &mut *ctx;
    jit_opstat(ctx, pc);
    let i = &mut *ctx.interp;
    let chunk = &*ctx.chunk;
    let Op::MakeObject(start, count, tidx) = chunk.ops[pc as usize] else {
        unreachable!("jit_make_object emitted only for MakeObject");
    };
    let count = count as usize;
    let base = sp.sub(count);
    let keys = &chunk.names[start as usize..start as usize + count];
    let v = if tidx != u32::MAX {
        i.make_plain_object_templated_from(&chunk.obj_maps[tidx as usize], keys, base, count)
    } else {
        let mut values = Vec::with_capacity(count);
        for k in 0..count {
            values.push(base.add(k).read());
        }
        i.make_plain_object_vm(keys, values)
    };
    observe_allocation(
        &chunk.feedback,
        pc as usize,
        crate::feedback::AllocationObjectKind::Object,
        count,
    );
    sp = base;
    sp.write(v);
    crate::jit::SpFlag {
        sp: sp.add(1),
        flag: 0,
    }
}

/// Dedicated `Op::MakeArray` entry: moves the operand values directly into the fresh array's
/// dense property storage, avoiding both generic opcode dispatch and an intermediate `Vec`.
pub(crate) unsafe extern "C" fn jit_make_array(
    ctx: *mut crate::jit::JitCtx,
    pc: u32,
    mut sp: *mut Value,
) -> crate::jit::SpFlag {
    let ctx = unsafe { &mut *ctx };
    jit_opstat(ctx, pc);
    let chunk = unsafe { &*ctx.chunk };
    let Op::MakeArray(count) = chunk.ops[pc as usize] else {
        unreachable!("jit_make_array emitted only for MakeArray");
    };
    let base = unsafe { sp.sub(count as usize) };
    let value = unsafe { (&*ctx.interp).make_array_from_raw(base, count as usize) };
    observe_allocation(
        &chunk.feedback,
        pc as usize,
        crate::feedback::AllocationObjectKind::Array,
        count as usize,
    );
    sp = base;
    unsafe { sp.write(value) };
    crate::jit::SpFlag {
        sp: unsafe { sp.add(1) },
        flag: 0,
    }
}

/// Dedicated element-store entry (same contract as [`jit_exec`]): handles the four element
/// assignment stack shapes without entering the full opcode dispatcher. Inline dense overwrites
/// never reach this helper; it primarily serves semantically checked array growth and sparse
/// writes after the generated guards decline.
pub(crate) unsafe extern "C" fn jit_set_elem(
    ctx: *mut crate::jit::JitCtx,
    pc: u32,
    mut sp: *mut Value,
) -> crate::jit::SpFlag {
    let _wide_slots = unsafe { JitWideSlots::enter(ctx) };
    let ctx = unsafe { &mut *ctx };
    jit_opstat(ctx, pc);
    let i = unsafe { &mut *ctx.interp };
    let chunk = unsafe { &*ctx.chunk };
    let result: Result<(), Abrupt> = (|| match chunk.ops[pc as usize] {
        Op::SetElem | Op::SetElemDrop => {
            let keep = matches!(chunk.ops[pc as usize], Op::SetElem);
            sp = unsafe { sp.sub(1) };
            let value = unsafe { sp.read() };
            sp = unsafe { sp.sub(1) };
            let key = unsafe { sp.read() };
            sp = unsafe { sp.sub(1) };
            let object = unsafe { sp.read() };
            let retained = keep.then(|| value.clone());
            if chunk.feedback.detailed_enabled() {
                set_element_profiled(i, chunk, pc as usize, &object, &key, value)?;
            } else if let (Value::Obj(o), Value::Num(n)) = (&object, &key) {
                match i.fast_set_elem(o, *n, value) {
                    Ok(()) => {}
                    Err(back) => {
                        let property_key = i.to_property_key(&key)?;
                        i.set_member(&object, &property_key, back)?;
                    }
                }
            } else {
                let property_key = i.to_property_key(&key)?;
                i.set_member(&object, &property_key, value)?;
            }
            if let Some(value) = retained {
                unsafe { sp.write(value) };
                sp = unsafe { sp.add(1) };
            }
            Ok(())
        }
        Op::SetElemLocal(slot) | Op::SetElemLocalDrop(slot) => {
            let keep = matches!(chunk.ops[pc as usize], Op::SetElemLocal(_));
            sp = unsafe { sp.sub(1) };
            let value = unsafe { sp.read() };
            sp = unsafe { sp.sub(1) };
            let key = unsafe { sp.read() };
            if keep {
                unsafe { sp.write(value.clone()) };
                sp = unsafe { sp.add(1) };
            }
            let local = unsafe { &*ctx.slots.add(slot as usize) };
            if chunk.feedback.detailed_enabled() {
                let object = local.clone();
                set_element_profiled(i, chunk, pc as usize, &object, &key, value)?;
            } else if let (Value::Obj(o), Value::Num(n)) = (local, &key) {
                match i.fast_set_elem(o, *n, value) {
                    Ok(()) => {}
                    Err(back) => {
                        let object = local.clone();
                        let property_key = i.to_property_key(&key)?;
                        i.set_member(&object, &property_key, back)?;
                    }
                }
            } else {
                let object = local.clone();
                let property_key = i.to_property_key(&key)?;
                i.set_member(&object, &property_key, value)?;
            }
            Ok(())
        }
        _ => unreachable!("jit_set_elem emitted only for element stores"),
    })();
    match result {
        Ok(()) => crate::jit::SpFlag { sp, flag: 0 },
        Err(abrupt) => {
            ctx.error = Some(abrupt);
            crate::jit::SpFlag { sp, flag: 1 }
        }
    }
}

/// Dedicated `Op::New` entry (same contract as [`jit_exec`]): enters the identity-cached
/// JIT-to-JIT constructor path without decoding the full opcode family.
pub(crate) unsafe extern "C" fn jit_new(
    ctx: *mut crate::jit::JitCtx,
    packed: u32,
    mut sp: *mut Value,
) -> crate::jit::SpFlag {
    let ctx = &mut *ctx;
    let pc = packed & 0xFFFF;
    let argc = packed >> 16;
    jit_opstat(ctx, pc);
    debug_assert!(matches!(
        (&*ctx.chunk).ops[pc as usize],
        Op::New(n, _) if n as u32 == argc
    ));
    let i = unsafe { &mut *ctx.interp };
    let chunk = unsafe { &*ctx.chunk };
    if chunk.feedback.detailed_enabled() {
        let argc = argc as usize;
        let args_ptr = unsafe { sp.sub(argc) };
        let callee = unsafe { (*sp.sub(argc + 1)).clone() };
        let args = unsafe { std::slice::from_raw_parts(args_ptr, argc) };
        let value = match construct_profiled(i, chunk, pc as usize, callee, args) {
            Ok(value) => value,
            Err(abrupt) => {
                ctx.error = Some(abrupt);
                return crate::jit::SpFlag { sp, flag: 1 };
            }
        };
        sp = unsafe { jit_consume(sp, argc + 1) };
        unsafe { sp.write(value) };
        return crate::jit::SpFlag {
            sp: unsafe { sp.add(1) },
            flag: 0,
        };
    }
    let cache = match (&*ctx.chunk).ops[pc as usize] {
        Op::New(_, cache) => cache,
        _ => unreachable!(),
    };
    match unsafe {
        jit_new_inner(
            i,
            Some(ctx as *mut crate::jit::JitCtx),
            Some((&*ctx.chunk, cache)),
            argc as usize,
            &mut sp,
        )
    } {
        Ok(()) => crate::jit::SpFlag { sp, flag: 0 },
        Err(ab) => {
            ctx.error = Some(ab);
            crate::jit::SpFlag { sp, flag: 1 }
        }
    }
}

/// Dedicated checked `instanceof` entry for generated guards that cannot consume their operands.
/// It avoids generic opcode dispatch and benefits from `instanceof_ic`'s live cached RHS proof.
pub(crate) unsafe extern "C" fn jit_instanceof(
    ctx: *mut crate::jit::JitCtx,
    pc: u32,
    sp: *mut Value,
) -> crate::jit::SpFlag {
    let ctx = unsafe { &mut *ctx };
    jit_opstat(ctx, pc);
    let chunk = unsafe { &*ctx.chunk };
    let Op::InstanceOf(cache) = chunk.ops[pc as usize] else {
        unreachable!("jit_instanceof emitted only for InstanceOf");
    };
    let rhs = unsafe { sp.sub(1).read() };
    let lhs = unsafe { sp.sub(2).read() };
    let out = unsafe { &mut *ctx.interp }.instanceof_ic(&lhs, &rhs, &chunk.caches[cache as usize]);
    let base = unsafe { sp.sub(2) };
    drop(lhs);
    drop(rhs);
    match out {
        Ok(value) => {
            unsafe { base.write(value) };
            crate::jit::SpFlag {
                sp: unsafe { base.add(1) },
                flag: 0,
            }
        }
        Err(abrupt) => {
            ctx.error = Some(abrupt);
            crate::jit::SpFlag { sp: base, flag: 1 }
        }
    }
}

unsafe fn jit_new_inner(
    i: &mut Interp,
    caller_ctx: Option<*mut crate::jit::JitCtx>,
    site: Option<(&Chunk, u32)>,
    argc: usize,
    sp: &mut *mut Value,
) -> Result<(), Abrupt> {
    let args_ptr = unsafe { sp.sub(argc) };
    // Identity-cached construct: on Some the arguments were MOVED into the callee's frame — pop
    // them virtually and drop only the callee slot.
    if let Some(r) =
        unsafe { i.construct_jit_fast(&*sp.sub(argc + 1), args_ptr, argc, caller_ctx, site) }
    {
        *sp = unsafe { args_ptr.sub(1) };
        match unsafe { sp.read() } {
            Value::Obj(o) => {
                if Rc::strong_count(&o) > 1 {
                    unsafe { Rc::decrement_strong_count(Rc::into_raw(o)) };
                } else {
                    drop(o);
                }
            }
            other => drop(other),
        }
        let v = r?;
        unsafe { sp.write(v) };
        *sp = unsafe { sp.add(1) };
        return Ok(());
    }
    let args = unsafe { std::slice::from_raw_parts(args_ptr, argc) };
    let callee = unsafe { (*sp.sub(argc + 1)).clone() };
    let v = i.construct(callee, args)?;
    *sp = unsafe { jit_consume(*sp, argc + 1) };
    unsafe { sp.write(v) };
    *sp = unsafe { sp.add(1) };
    Ok(())
}

/// Dedicated property-store entry (same contract as [`jit_exec`]): straight into
/// [`crate::interpreter::Interp::set_prop_ic`] for the four store shapes, skipping the
/// generic op decode — creation-heavy code (`node.value = x` on a shape that lacks the key)
/// funnels every write through here.
pub(crate) unsafe extern "C" fn jit_set_prop(
    ctx: *mut crate::jit::JitCtx,
    pc: u32,
    mut sp: *mut Value,
) -> crate::jit::SpFlag {
    let _wide_slots = unsafe { JitWideSlots::enter(ctx) };
    let ctx = &mut *ctx;
    jit_opstat(ctx, pc);
    let i = &mut *ctx.interp;
    let chunk = &*ctx.chunk;
    let r: Result<(), Abrupt> = (|| match chunk.ops[pc as usize] {
        Op::SetProp(n, c) => {
            sp = sp.sub(1);
            let v = sp.read();
            sp = sp.sub(1);
            let obj = sp.read();
            set_named_property(
                i,
                chunk,
                pc as usize,
                &obj,
                &chunk.names[n as usize],
                v.clone(),
                &chunk.caches[c as usize],
            )?;
            sp.write(v);
            sp = sp.add(1);
            Ok(())
        }
        Op::SetPropDrop(n, c) => {
            sp = sp.sub(1);
            let v = sp.read();
            sp = sp.sub(1);
            let obj = sp.read();
            set_named_property(
                i,
                chunk,
                pc as usize,
                &obj,
                &chunk.names[n as usize],
                v,
                &chunk.caches[c as usize],
            )
        }
        Op::SetPropThisDrop(n, c) => {
            sp = sp.sub(1);
            let v = sp.read();
            let this = (*ctx.this_raw).clone();
            set_named_property(
                i,
                chunk,
                pc as usize,
                &this,
                &chunk.names[n as usize],
                v,
                &chunk.caches[c as usize],
            )
        }
        Op::SetPropLocalDrop(s, n, c) => {
            sp = sp.sub(1);
            let v = sp.read();
            let obj = (*ctx.slots.add(s as usize)).clone();
            if matches!(obj, Value::Empty) {
                return Err(i.throw(
                    "ReferenceError",
                    format!(
                        "cannot access '{}' before initialization",
                        chunk.slot_names[s as usize]
                    ),
                ));
            }
            set_named_property(
                i,
                chunk,
                pc as usize,
                &obj,
                &chunk.names[n as usize],
                v,
                &chunk.caches[c as usize],
            )
        }
        _ => unreachable!("jit_set_prop emitted only for property stores"),
    })();
    match r {
        Ok(()) => crate::jit::SpFlag { sp, flag: 0 },
        Err(ab) => {
            ctx.error = Some(ab);
            crate::jit::SpFlag { sp, flag: 1 }
        }
    }
}

thread_local! {
    /// Scratch site cells for COMPUTED string-key property reads (`o[k]` where `k` is a
    /// string): `get_prop_ic` needs a site, and probes address all `PROP_IC_WAYS` CONSECUTIVE
    /// cells relative to the one passed (`Interp::ic_way`), so the scratch must be a full
    /// way-array. Computed sites have no cells of their own — the global stub cache (keyed by
    /// receiver shape + key data pointer, both stable for the shared LStr instances that flow
    /// through dispatch tables like astring's `this[node.type]`) carries the real caching;
    /// these cells just absorb the fills.
    static ELEM_IC: [std::cell::Cell<IcState>; PROP_IC_WAYS] =
        const { [const { std::cell::Cell::new(IcState::EMPTY) }; PROP_IC_WAYS] };
}

/// Computed string-key read fast path: route through the property-IC machinery instead of the
/// raw `get_member` chain walk. The way cells are CLEARED first — an `IcState` carries no
/// name (a real site cell binds one implicitly), so a stale way entry filled for one key
/// would answer a different key on the same shape. Resolution therefore comes from the
/// name-keyed STUB cache (hit: shape-validated, no scan) or a rederive; the scratch ways
/// only absorb the fills. Digit-leading keys keep the element paths.
#[inline]
fn get_elem_str_ic(
    i: &mut crate::interpreter::Interp,
    obj: &Value,
    key: &crate::lstr::LStr,
) -> Result<Value, Abrupt> {
    ELEM_IC.with(|cells| {
        for c in cells {
            c.set(IcState::EMPTY);
        }
        i.get_prop_keyed(obj, key, &cells[0])
    })
}

/// Dedicated property-read entry (same contract as [`jit_exec`]): straight into
/// [`crate::interpreter::Interp::get_prop_ic`] for the four read shapes, skipping the generic
/// op decode.
pub(crate) unsafe extern "C" fn jit_get_prop(
    ctx: *mut crate::jit::JitCtx,
    pc: u32,
    mut sp: *mut Value,
) -> crate::jit::SpFlag {
    let ctx = &mut *ctx;
    jit_opstat(ctx, pc);
    let i = &mut *ctx.interp;
    let chunk = &*ctx.chunk;
    let r: Result<(), Abrupt> = (|| {
        match chunk.ops[pc as usize] {
            Op::GetProp(n, c) => {
                sp = sp.sub(1);
                let obj = sp.read();
                let v = get_named_property(
                    i,
                    chunk,
                    pc as usize,
                    &obj,
                    &chunk.names[n as usize],
                    &chunk.caches[c as usize],
                )?;
                sp.write(v);
                sp = sp.add(1);
                Ok(())
            }
            Op::GetPropThis(n, c) => {
                let this = &*ctx.this_raw;
                let v = get_named_property(
                    i,
                    chunk,
                    pc as usize,
                    this,
                    &chunk.names[n as usize],
                    &chunk.caches[c as usize],
                )?;
                sp.write(v);
                sp = sp.add(1);
                Ok(())
            }
            Op::GetPropLocal(s, n, c) => {
                let obj = if ctx.slots_packed {
                    crate::value::PackedValue::clone_raw(ctx.slots.cast::<u64>().add(s as usize))
                } else {
                    (*ctx.slots.add(s as usize)).clone()
                };
                if matches!(obj, Value::Empty) {
                    return Err(i.throw(
                        "ReferenceError",
                        format!(
                            "cannot access '{}' before initialization",
                            chunk.slot_names[s as usize]
                        ),
                    ));
                }
                let v = get_named_property(
                    i,
                    chunk,
                    pc as usize,
                    &obj,
                    &chunk.names[n as usize],
                    &chunk.caches[c as usize],
                )?;
                sp.write(v);
                sp = sp.add(1);
                Ok(())
            }
            Op::GetMethod(n, c) => {
                let obj = &*sp.sub(1); // receiver stays on the stack
                let m = get_named_property(
                    i,
                    chunk,
                    pc as usize,
                    obj,
                    &chunk.names[n as usize],
                    &chunk.caches[c as usize],
                )?;
                sp.write(m);
                sp = sp.add(1);
                Ok(())
            }
            _ => unreachable!("jit_get_prop emitted only for property reads"),
        }
    })();
    match r {
        Ok(()) => crate::jit::SpFlag { sp, flag: 0 },
        Err(ab) => {
            ctx.error = Some(ab);
            crate::jit::SpFlag { sp, flag: 1 }
        }
    }
}

/// Dedicated helper for `Op::Call` / `Op::CallWithThis` sites — the same contract as
/// [`jit_exec`], minus the full op dispatch (calls dominate helper traffic in call-heavy code,
/// so the two arms get a two-way decode of their own).
pub(crate) unsafe extern "C" fn jit_call(
    ctx: *mut crate::jit::JitCtx,
    pc: u32,
    mut sp: *mut Value,
) -> crate::jit::SpFlag {
    let ctx = &mut *ctx;
    jit_opstat(ctx, pc);
    match jit_call_inner(ctx, pc, &mut sp) {
        Ok(()) => crate::jit::SpFlag { sp, flag: 0 },
        Err(ab) => {
            ctx.error = Some(ab);
            crate::jit::SpFlag { sp, flag: 1 }
        }
    }
}

unsafe fn jit_call_inner(
    ctx: &mut crate::jit::JitCtx,
    pc: u32,
    sp: &mut *mut Value,
) -> Result<(), Abrupt> {
    let i = &mut *ctx.interp;
    let chunk = &*ctx.chunk;
    macro_rules! push {
        ($v:expr) => {{
            sp.write($v);
            *sp = sp.add(1);
        }};
    }
    let (argc, c, with_this) = match chunk.ops[pc as usize] {
        Op::CallWithThis(argc, c) => (argc as usize, c, true),
        Op::Call(argc, c) => (argc as usize, c, false),
        _ => unreachable!("jit_call emitted only for call ops"),
    };
    let args_ptr = sp.sub(argc);
    if chunk.feedback.detailed_enabled() {
        let args = std::slice::from_raw_parts(args_ptr, argc);
        let callee = (*sp.sub(argc + 1)).clone();
        let this = if with_this {
            (*sp.sub(argc + 2)).clone()
        } else {
            Value::Undefined
        };
        let value = call_profiled(i, chunk, pc as usize, callee, this, args)?;
        *sp = jit_consume(*sp, argc + usize::from(with_this) + 1);
        push!(value);
        return Ok(());
    }
    // See the Op::Call arm of `jit_exec_inner` for the ownership story: on Some the arguments
    // and the `this` slot were MOVED into the callee.
    let mut undef = std::mem::ManuallyDrop::new(Value::Undefined);
    let this_slot: *const Value = if with_this {
        sp.sub(argc + 2)
    } else {
        &raw mut *undef as *const Value
    };
    let mut r = i.call_jit_cached(
        &chunk.call_caches[c as usize],
        &*sp.sub(argc + 1),
        this_slot,
        args_ptr,
        argc,
    );
    if r.is_none() {
        // Plain-native fast call: a bare `fn` callee in a proxy-free single-realm engine skips the
        // call/call_inner/call_dispatch layering. The callee is ALSO recorded as a native IC
        // entry (identity-pinned like a user callee), so subsequent calls take the machine-code
        // probe + `call_native_committed` — no receiver borrow, no `Callable` dispatch.
        if i.proxies.is_empty() && !i.multi_realm() {
            let callee = &*sp.sub(argc + 1);
            if let Value::Obj(o) = callee {
                let nf = match &o.borrow().call {
                    crate::value::Callable::Native(nf) => Some(*nf),
                    _ => None,
                };
                if let Some(nf) = nf {
                    {
                        let key = Rc::as_ptr(o) as usize;
                        let mut p = chunk.call_pins.borrow_mut();
                        if p.len() < 4096 || p.contains_key(&key) {
                            p.entry(key).or_insert_with(|| Rc::downgrade(o));
                            drop(p);
                            if !i
                                .global_env_pins
                                .iter()
                                .any(|g| g.as_ptr() == Rc::as_ptr(&i.global_env))
                            {
                                let g = Rc::downgrade(&i.global_env);
                                i.global_env_pins.push(g);
                            }
                            chunk.call_caches[c as usize].fill(CallIc {
                                callee: key,
                                env: std::ptr::null(),
                                chunk: std::ptr::null(),
                                code: std::ptr::null(),
                                global_env: Rc::as_ptr(&i.global_env) as usize,
                                strict: true,
                                uses_this: true,
                                n_params: 0,
                                n_slots: 0,
                                direct: 0, // bit 0 clear: the direct sequence's first gate bails
                                func: std::ptr::null(),
                                epoch: CALL_IC_EPOCH.load(std::sync::atomic::Ordering::Relaxed),
                                chunk_raw: std::ptr::null(),
                                code_mem: std::ptr::null(),
                                pc_offs_ptr: std::ptr::null(),
                                native: nf as usize,
                                intrinsic: match nf as *const () as usize {
                                    p if p
                                        == crate::builtins::nf_char_code_at as *const ()
                                            as usize =>
                                    {
                                        INTRINSIC_CHAR_CODE_AT
                                    }
                                    p if p == crate::builtins::nf_char_at as *const () as usize => {
                                        INTRINSIC_CHAR_AT
                                    }
                                    p if p
                                        == crate::builtins::nf_string_slice as *const ()
                                            as usize =>
                                    {
                                        INTRINSIC_STRING_SLICE
                                    }
                                    p if p
                                        == crate::builtins::nf_object_has_own as *const ()
                                            as usize =>
                                    {
                                        INTRINSIC_OBJECT_HAS_OWN
                                    }
                                    p if p
                                        == crate::builtins::nf_function_apply as *const ()
                                            as usize =>
                                    {
                                        INTRINSIC_FUNCTION_APPLY
                                    }
                                    p if p
                                        == crate::builtins::nf_math_sqrt as *const () as usize =>
                                    {
                                        INTRINSIC_MATH_SQRT
                                    }
                                    p if p
                                        == crate::builtins::nf_array_push as *const () as usize =>
                                    {
                                        INTRINSIC_ARRAY_PUSH
                                    }
                                    p if p
                                        == crate::builtins::nf_array_pop as *const () as usize =>
                                    {
                                        INTRINSIC_ARRAY_POP
                                    }
                                    p if p
                                        == crate::builtins::nf_function_call as *const ()
                                            as usize =>
                                    {
                                        INTRINSIC_FUNCTION_CALL
                                    }
                                    p if p
                                        == crate::builtins::regexp_exec as *const () as usize =>
                                    {
                                        INTRINSIC_REGEXP_EXEC_DISCARD
                                    }
                                    p if p
                                        == crate::builtins::nf_string_replace as *const ()
                                            as usize =>
                                    {
                                        INTRINSIC_STRING_REPLACE_DISCARD
                                    }
                                    p if p
                                        == crate::builtins::nf_string_split as *const ()
                                            as usize =>
                                    {
                                        INTRINSIC_STRING_SPLIT_DISCARD
                                    }
                                    _ => 0,
                                },
                            });
                        }
                    }
                    let mut undef2 = std::mem::ManuallyDrop::new(Value::Undefined);
                    let this_slot2: *const Value = if with_this {
                        sp.sub(argc + 2)
                    } else {
                        &raw mut *undef2 as *const Value
                    };
                    let r = i.call_native_committed(nf, this_slot2, args_ptr, argc);
                    // Arguments and `this` were consumed; drop only the callee slot.
                    *sp = args_ptr.sub(1);
                    match sp.read() {
                        Value::Obj(o) => {
                            if Rc::strong_count(&o) > 1 {
                                unsafe { Rc::decrement_strong_count(Rc::into_raw(o)) };
                            } else {
                                drop(o);
                            }
                        }
                        other => drop(other),
                    }
                    if with_this {
                        *sp = sp.sub(1);
                    }
                    let v = r?;
                    push!(v);
                    return Ok(());
                }
            }
        }
    }
    if r.is_none() {
        r = i.call_jit_fast(
            &*sp.sub(argc + 1),
            this_slot,
            args_ptr,
            argc,
            Some((&chunk.call_caches[c as usize], &chunk.call_pins)),
        );
    }
    if let Some(r) = r {
        *sp = args_ptr.sub(1);
        // Drop the callee/method slot. It is virtually always a function object with other
        // live references, so peel that case into a bare refcount decrement instead of the
        // outlined generic Value drop.
        match sp.read() {
            Value::Obj(o) => {
                if Rc::strong_count(&o) > 1 {
                    unsafe { Rc::decrement_strong_count(Rc::into_raw(o)) };
                } else {
                    drop(o);
                }
            }
            other => drop(other),
        }
        if with_this {
            *sp = sp.sub(1); // `this` was consumed by the callee
        }
        let v = r?;
        push!(v);
        return Ok(());
    }
    let args = std::slice::from_raw_parts(args_ptr, argc);
    let callee = (*sp.sub(argc + 1)).clone();
    let this = if with_this {
        (*sp.sub(argc + 2)).clone()
    } else {
        Value::Undefined
    };
    let v = i.call(callee, this, args)?;
    *sp = jit_consume(*sp, argc + 1 + with_this as usize);
    push!(v);
    Ok(())
}

/// Debug: `LUMEN_JIT_CALLSTAT=1` tallies, for every call that reaches the way-1 HIT helper,
/// which direct-call gate would have (or did) reject the shared-ctx fast path. Temporary
/// diagnostic mirroring `emit_direct_call`'s runtime checks in emission order.
pub(crate) fn jit_callstat_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("LUMEN_JIT_CALLSTAT").is_some()
            || std::env::var_os("LUMEN_JIT_NATIVESTAT").is_some()
    })
}

#[inline(always)]
unsafe fn jit_callstat(
    i: &crate::interpreter::Interp,
    ctx: &crate::jit::JitCtx,
    ic: &CallIc,
    argc: usize,
    with_this: bool,
    sp: *mut Value,
) {
    if !ctx.callstat_enabled {
        return;
    }
    struct Dump(crate::fasthash::FastMap<&'static str, u64>);
    impl Drop for Dump {
        fn drop(&mut self) {
            let mut v: Vec<_> = self.0.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1));
            for (r, n) in v {
                eprintln!("[jit-callstat] {n:>12}  {r}");
            }
        }
    }
    thread_local! {
        static COUNTS: std::cell::RefCell<Dump> = std::cell::RefCell::new(Dump(Default::default()));
    }
    struct NativeDump(crate::fasthash::FastMap<(usize, usize, bool), (String, u64)>);
    impl Drop for NativeDump {
        fn drop(&mut self) {
            let mut v: Vec<_> = self.0.values().collect();
            v.sort_by_key(|entry| std::cmp::Reverse(entry.1));
            for (name, n) in v {
                eprintln!("[jit-nativestat] {n:>12}  {name}");
            }
        }
    }
    thread_local! {
        static NATIVES: std::cell::RefCell<NativeDump> =
            std::cell::RefCell::new(NativeDump(Default::default()));
    }
    static NATIVE_ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if ic.native != 0
        && *NATIVE_ON.get_or_init(|| std::env::var_os("LUMEN_JIT_NATIVESTAT").is_some())
    {
        NATIVES.with(|counts| {
            let mut counts = counts.borrow_mut();
            let entry = counts
                .0
                .entry((ic.native, argc, with_this))
                .or_insert_with(|| {
                    let name = match &*sp.sub(argc + 1) {
                        Value::Obj(o) => o
                            .borrow()
                            .props
                            .get("name")
                            .and_then(|p| match p.value() {
                                Value::Str(s) => Some(s.to_string()),
                                _ => None,
                            })
                            .unwrap_or_else(|| "<native>".to_string()),
                        _ => "<native>".to_string(),
                    };
                    (format!("{name} argc={argc} this={with_this}"), 0)
                });
            entry.1 += 1;
        });
    }
    let reason: &'static str = 'r: {
        if ic.native != 0 {
            break 'r "native entry";
        }
        if ic.direct & CALL_IC_NEEDS_ENV != 0 {
            break 'r "env entry";
        }
        if argc > 64 {
            break 'r "emit: argc > 64";
        }
        if ic.direct & 1 == 0 {
            break 'r "gate: var force resets";
        }
        if ic.direct & 8 == 0 {
            break 'r "gate: frame > FRAME_BUF";
        }
        if !(*ic.chunk_raw).inline_attempted.get() {
            break 'r "gate: recompile not settled";
        }
        if ic.direct & 2 != 0 && ctx.global_body.is_null() {
            break 'r "gate: needs_global, no live global_body";
        }
        if ic.n_params as usize != argc {
            break 'r "gate: argc != n_params";
        }
        if ic.uses_this && !ic.strict {
            if !with_this {
                break 'r "gate: sloppy this-user, no receiver";
            }
            let tag = *(sp.sub(argc + 2) as *const u8);
            if tag != 8 {
                break 'r "gate: sloppy this-user, non-object receiver";
            }
        }
        if let Value::Obj(o) = &*sp.sub(argc + 1) {
            if Rc::strong_count(o) <= 1 {
                break 'r "gate: callee refcount <= 1";
            }
        }
        if i.depth >= i.direct_call_depth {
            break 'r "gate: depth";
        }
        if (i.gc_tick + 1) & crate::interpreter::GC_CALL_POLL_MASK == 0 {
            break 'r "gate: gc tick due";
        }
        if i.fn_frames.len() == i.fn_frames.capacity() {
            break 'r "gate: fn_frames at capacity";
        }
        if i.frame_pool.is_empty() {
            break 'r "gate: frame pool empty";
        }
        break 'r "all gates pass (direct not emitted at site?)";
    };
    let _ = COUNTS.try_with(|c| *c.borrow_mut().0.entry(reason).or_insert(0) += 1);
}

/// Debug: `LUMEN_JIT_OPSTAT=1` tallies which ops still reach a helper (the JIT's slow path) and
/// prints the top offenders at process exit. Always-inlined so the disabled case is a single
/// predictable branch inside the helper.
pub(crate) fn jit_opstat_enabled() -> bool {
    static OPSTAT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OPSTAT.get_or_init(|| std::env::var_os("LUMEN_JIT_OPSTAT").is_some())
}

#[inline(always)]
unsafe fn jit_opstat(ctx: &mut crate::jit::JitCtx, pc: u32) {
    {
        if ctx.opstat_enabled {
            struct OpstatDump(crate::fasthash::FastMap<String, u64>);
            impl Drop for OpstatDump {
                fn drop(&mut self) {
                    let mut v: Vec<_> = self.0.iter().collect();
                    v.sort_by(|a, b| b.1.cmp(a.1));
                    for (op, n) in v.iter().take(24) {
                        eprintln!("[jit-opstat] {n:>12}  {op}");
                    }
                }
            }
            thread_local! {
                static COUNTS: std::cell::RefCell<OpstatDump> =
                    std::cell::RefCell::new(OpstatDump(Default::default()));
            }
            let chunk = &*ctx.chunk;
            let op = format!("{:?}", chunk.ops[pc as usize]);
            // Collapse operand differences so sites aggregate by opcode; `=2` adds the function
            // (identified by its leading slot names) and pc for pinpointing bail sites.
            let mut name = op.split(['(', ' ']).next().unwrap_or(&op).to_string();
            if std::env::var("LUMEN_JIT_OPSTAT").as_deref() == Ok("2") {
                let params: Vec<&str> = chunk.slot_names.iter().take(3).map(|s| &**s).collect();
                name = format!("{name} @ fn({}) pc{}", params.join(","), pc);
            }
            let _ = COUNTS.try_with(|c| *c.borrow_mut().0.entry(name).or_insert(0) += 1);
        }
    }
}

unsafe fn jit_exec_inner(
    ctx: &mut crate::jit::JitCtx,
    pc: u32,
    sp: &mut *mut Value,
) -> Result<(), Abrupt> {
    let i = &mut *ctx.interp;
    let chunk = &*ctx.chunk;
    // The activation env, reconstructed from the swapped `env_raw` (NOT a dedicated handle
    // pointer: the direct-call sequence swaps env_raw per activation, so this is the only
    // field that's correct for a shared-ctx callee — a separate `env_ref` silently resolved
    // free names in the CALLER's scope). The aliased handle outlives the run: `run` holds a
    // local, `run_moved`'s caller keeps one alive, and a direct call pins the callee object
    // (whose `Callable::User` owns the env) on the caller's operand stack until after finish.
    let env_h = std::mem::ManuallyDrop::new(unsafe {
        Rc::from_raw(ctx.env_raw as *const std::cell::RefCell<crate::interpreter::Scope>)
    });
    let env: &Env = &env_h;
    let slots = std::slice::from_raw_parts_mut(ctx.slots, ctx.n_slots);
    macro_rules! pop {
        () => {{
            *sp = sp.sub(1);
            sp.read()
        }};
    }
    macro_rules! push {
        ($v:expr) => {{
            sp.write($v);
            *sp = sp.add(1);
        }};
    }
    match chunk.ops[pc as usize] {
        Op::Const(k) => push!(chunk.consts[k as usize].clone()),
        Op::Undef => push!(Value::Undefined),
        Op::Dup => {
            let t = (*sp.sub(1)).clone();
            push!(t);
        }
        Op::Pop => {
            pop!();
        }
        Op::Dup2 => {
            let a = (*sp.sub(2)).clone();
            let b = (*sp.sub(1)).clone();
            push!(a);
            push!(b);
        }
        Op::LoadLocal(s) => {
            let v = slots[s as usize].clone();
            if matches!(v, Value::Empty) {
                return Err(i.throw(
                    "ReferenceError",
                    format!(
                        "cannot access '{}' before initialization",
                        chunk.slot_names[s as usize]
                    ),
                ));
            }
            push!(v);
        }
        Op::StoreLocal(s) => slots[s as usize] = pop!(),
        Op::UpdateLocal(s, kind) => {
            let idx = s as usize;
            if matches!(slots[idx], Value::Empty) {
                return Err(i.throw(
                    "ReferenceError",
                    format!(
                        "cannot access '{}' before initialization",
                        chunk.slot_names[idx]
                    ),
                ));
            }
            let old = slots[idx].clone();
            if let Some(v) = step_value(i, &chunk.feedback, pc as usize, kind, old, |_, v| {
                slots[idx] = v;
                Ok(())
            })? {
                push!(v);
            }
        }
        Op::Tdz(s) => slots[s as usize] = Value::Empty,
        Op::LoadCap(n) => {
            push!(chunk.load_cap_ic(i, env, n)?);
        }
        Op::StoreCap(n) => {
            let v = pop!();
            chunk.store_cap_ic(i, env, n, v, false)?;
        }
        Op::StoreCapInit(n) => {
            let v = pop!();
            chunk.store_cap_ic(i, env, n, v, true)?;
        }
        Op::UpdateCap(n, kind) => {
            let name = &chunk.names[n as usize];
            let old = {
                let b = env.borrow();
                let bd = b.vars.get(name).expect("captured binding missing");
                if !bd.initialized {
                    let msg = format!("cannot access '{name}' before initialization");
                    drop(b);
                    return Err(i.throw("ReferenceError", msg));
                }
                bd.value.clone()
            };
            if let Some(v) = step_value(i, &chunk.feedback, pc as usize, kind, old, |_, v| {
                if let Some(bd) = env.borrow_mut().vars.get_mut(name) {
                    bd.value = v;
                }
                Ok(())
            })? {
                push!(v);
            }
        }
        Op::UpdateName(n, kind) => {
            let name = &chunk.names[n as usize];
            let old = i.get_var(name, env)?;
            if let Some(v) = step_value(i, &chunk.feedback, pc as usize, kind, old, |i, v| {
                i.assign_free_name(name, v, env)
            })? {
                push!(v);
            }
        }
        Op::UpdateNameCached(n, c, kind) => {
            let name = &chunk.names[n as usize];
            let old = chunk.load_name_ic(i, env, n, c)?;
            if let Some(v) = step_value(i, &chunk.feedback, pc as usize, kind, old, |i, v| {
                i.assign_free_name(name, v, env)
            })? {
                push!(v);
            }
        }
        Op::MakeClosure(fidx, name_n) => {
            let v = i.make_function(chunk.funcs[fidx as usize].clone(), env.clone());
            if name_n != u32::MAX {
                i.set_fn_name(&v, &chunk.names[name_n as usize]);
            }
            observe_allocation(
                &chunk.feedback,
                pc as usize,
                crate::feedback::AllocationObjectKind::Function,
                0,
            );
            push!(v);
        }
        Op::LoadName(n, c) => {
            let v = chunk.load_name_ic(i, env, n, c)?;
            push!(v);
        }
        Op::StoreName(n) => {
            let v = pop!();
            i.assign_free_name(&chunk.names[n as usize], v, env)?;
        }
        Op::StoreNameCached(n, c) => {
            let v = pop!();
            chunk.store_name_ic(i, env, n, c, v)?;
        }
        Op::LoadThis => push!(ctx.this_val.clone()),
        Op::GetProp(n, c) => {
            let obj = pop!();
            let v = get_named_property(
                i,
                chunk,
                pc as usize,
                &obj,
                &chunk.names[n as usize],
                &chunk.caches[c as usize],
            )?;
            push!(v);
        }
        Op::GetPropThis(n, c) => {
            let this = (*ctx.this_raw).clone();
            let v = get_named_property(
                i,
                chunk,
                pc as usize,
                &this,
                &chunk.names[n as usize],
                &chunk.caches[c as usize],
            )?;
            push!(v);
        }
        Op::GetPropLocal(s, n, c) => {
            let obj = slots[s as usize].clone();
            if matches!(obj, Value::Empty) {
                return Err(i.throw(
                    "ReferenceError",
                    format!(
                        "cannot access '{}' before initialization",
                        chunk.slot_names[s as usize]
                    ),
                ));
            }
            let v = get_named_property(
                i,
                chunk,
                pc as usize,
                &obj,
                &chunk.names[n as usize],
                &chunk.caches[c as usize],
            )?;
            push!(v);
        }
        Op::SetProp(n, c) => {
            let v = pop!();
            let obj = pop!();
            set_named_property(
                i,
                chunk,
                pc as usize,
                &obj,
                &chunk.names[n as usize],
                v.clone(),
                &chunk.caches[c as usize],
            )?;
            push!(v);
        }
        Op::SetPropDrop(n, c) => {
            let v = pop!();
            let obj = pop!();
            set_named_property(
                i,
                chunk,
                pc as usize,
                &obj,
                &chunk.names[n as usize],
                v,
                &chunk.caches[c as usize],
            )?;
        }
        Op::SetPropThisDrop(n, c) => {
            let v = pop!();
            let this = (*ctx.this_raw).clone();
            set_named_property(
                i,
                chunk,
                pc as usize,
                &this,
                &chunk.names[n as usize],
                v,
                &chunk.caches[c as usize],
            )?;
        }
        Op::SetPropLocalDrop(s, n, c) => {
            let v = pop!();
            let obj = slots[s as usize].clone();
            if matches!(obj, Value::Empty) {
                return Err(i.throw(
                    "ReferenceError",
                    format!(
                        "cannot access '{}' before initialization",
                        chunk.slot_names[s as usize]
                    ),
                ));
            }
            set_named_property(
                i,
                chunk,
                pc as usize,
                &obj,
                &chunk.names[n as usize],
                v,
                &chunk.caches[c as usize],
            )?;
        }
        Op::DestructureGuard => {
            if matches!(&*sp.sub(1), Value::Undefined | Value::Null) {
                return Err(i.throw("TypeError", "cannot destructure null or undefined"));
            }
        }
        Op::DestructureArr(n) => {
            let v = pop!();
            let (it, nx) = i.get_iterator(&v)?;
            let mut done = false;
            for _ in 0..n {
                if !done {
                    match i.iterator_step(&it, &nx)? {
                        Some(x) => {
                            push!(x);
                            continue;
                        }
                        None => done = true,
                    }
                }
                push!(Value::Undefined);
            }
            if !done {
                i.iterator_close_normal(&it)?;
            }
        }
        Op::AssignTarget(target) => {
            let value = pop!();
            assign_target_with_slots(
                i,
                &chunk.assignment_targets[target as usize],
                value,
                env,
                slots,
            )?;
        }
        Op::DeleteProp(n, strict) => {
            let base = pop!();
            let prop = &chunk.names[n as usize];
            if matches!(base, Value::Undefined | Value::Null) {
                return Err(i.throw(
                    "TypeError",
                    format!("cannot delete property '{prop}' of null or undefined"),
                ));
            }
            let v = i.delete_prop_with(base, prop, strict)?;
            push!(v);
        }
        Op::DeleteElem(strict) => {
            let idx = pop!();
            let base = pop!();
            let key = i.to_property_key(&idx)?;
            if matches!(base, Value::Undefined | Value::Null) {
                return Err(i.throw(
                    "TypeError",
                    format!("cannot delete property '{key}' of null or undefined"),
                ));
            }
            let v = i.delete_prop_with(base, &key, strict)?;
            push!(v);
        }
        Op::DeleteName(name) => {
            push!(i.delete_ident(&chunk.names[name as usize], env)?);
        }
        Op::DeleteSuper => {
            return Err(i.throw("ReferenceError", "cannot delete a super property"));
        }
        Op::CallSpread(argc) | Op::CallSpreadThis(argc) => {
            let spread = pop!();
            let mut plain: Vec<Value> = (1..argc).map(|_| pop!()).collect();
            plain.reverse();
            let callee = pop!();
            let this = if matches!(chunk.ops[pc as usize], Op::CallSpreadThis(_)) {
                pop!()
            } else {
                Value::Undefined
            };
            let mut args = plain;
            let (it, nx) = i.get_iterator(&spread)?;
            while let Some(x) = i.iterator_step(&it, &nx)? {
                args.push(x);
            }
            let v = if chunk.feedback.detailed_enabled() {
                call_profiled(i, chunk, pc as usize, callee, this, &args)?
            } else {
                i.call(callee, this, &args)?
            };
            push!(v);
        }
        Op::CallArgsArray | Op::CallArgsArrayThis => {
            let args = argument_array_values(i, pop!());
            let callee = pop!();
            let this = if matches!(chunk.ops[pc as usize], Op::CallArgsArrayThis) {
                pop!()
            } else {
                Value::Undefined
            };
            if chunk.feedback.detailed_enabled() {
                push!(call_profiled(i, chunk, pc as usize, callee, this, &args)?);
            } else {
                push!(i.call(callee, this, &args)?);
            }
        }
        Op::AppendProp(n, c) => {
            let v = pop!();
            let lval = pop!();
            let obj = pop!();
            let name = &chunk.names[n as usize];
            let lval = if let (Value::Str(x), Value::Obj(o)) = (&v, &obj) {
                match i.append_prop_fast(o, name, lval, x) {
                    Ok(()) => return Ok(()),
                    Err(l) => l,
                }
            } else {
                lval
            };
            let r = i.binary("+", lval, v)?;
            set_named_property(
                i,
                chunk,
                pc as usize,
                &obj,
                name,
                r,
                &chunk.caches[c as usize],
            )?;
        }
        Op::GetElem => {
            let key = pop!();
            let obj = pop!();
            if chunk.feedback.detailed_enabled() {
                let v = get_element_profiled(i, chunk, pc as usize, &obj, &key)?;
                push!(v);
                return Ok(());
            }
            if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                if let Some(v) = i.fast_get_elem(o, *n) {
                    push!(v);
                    return Ok(());
                }
            }
            if let (Value::Obj(_), Value::Str(s)) = (&obj, &key) {
                if !s.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
                    let v = get_elem_str_ic(i, &obj, s)?;
                    push!(v);
                    return Ok(());
                }
            }
            if matches!(obj, Value::Undefined | Value::Null) {
                return Err(i.throw("TypeError", "cannot read property of null or undefined"));
            }
            let k = i.to_property_key(&key)?;
            let v = i.get_member(&obj, &k)?;
            push!(v);
        }
        Op::SetElem => {
            let v = pop!();
            let key = pop!();
            let obj = pop!();
            if chunk.feedback.detailed_enabled() {
                let ret = v.clone();
                set_element_profiled(i, chunk, pc as usize, &obj, &key, v)?;
                push!(ret);
                return Ok(());
            }
            if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                let ret = v.clone();
                match i.fast_set_elem(o, *n, v) {
                    Ok(()) => {
                        push!(ret);
                        return Ok(());
                    }
                    Err(back) => {
                        let k = i.to_property_key(&key)?;
                        i.set_member(&obj, &k, back)?;
                        push!(ret);
                        return Ok(());
                    }
                }
            }
            let k = i.to_property_key(&key)?;
            i.set_member(&obj, &k, v.clone())?;
            push!(v);
        }
        Op::SetElemDrop => {
            let v = pop!();
            let key = pop!();
            let obj = pop!();
            if chunk.feedback.detailed_enabled() {
                set_element_profiled(i, chunk, pc as usize, &obj, &key, v)?;
                return Ok(());
            }
            if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                match i.fast_set_elem(o, *n, v) {
                    Ok(()) => return Ok(()),
                    Err(back) => {
                        let k = i.to_property_key(&key)?;
                        i.set_member(&obj, &k, back)?;
                        return Ok(());
                    }
                }
            }
            let k = i.to_property_key(&key)?;
            i.set_member(&obj, &k, v)?;
        }
        Op::GetElemLocal(s) => {
            let key = pop!();
            if chunk.feedback.detailed_enabled() {
                let obj = slots[s as usize].clone();
                let v = get_element_profiled(i, chunk, pc as usize, &obj, &key)?;
                push!(v);
                return Ok(());
            }
            if let (Value::Obj(o), Value::Num(n)) = (&slots[s as usize], &key) {
                if let Some(v) = i.fast_get_elem(o, *n) {
                    push!(v);
                    return Ok(());
                }
            }
            let obj = slots[s as usize].clone();
            if matches!(obj, Value::Undefined | Value::Null) {
                return Err(i.throw("TypeError", "cannot read property of null or undefined"));
            }
            let k = i.to_property_key(&key)?;
            let v = i.get_member(&obj, &k)?;
            push!(v);
        }
        Op::SetElemLocal(s) | Op::SetElemLocalDrop(s) => {
            let keep = matches!(chunk.ops[pc as usize], Op::SetElemLocal(_));
            let v = pop!();
            let key = pop!();
            if chunk.feedback.detailed_enabled() {
                if keep {
                    push!(v.clone());
                }
                let obj = slots[s as usize].clone();
                set_element_profiled(i, chunk, pc as usize, &obj, &key, v)?;
                return Ok(());
            }
            if keep {
                push!(v.clone());
            }
            if let (Value::Obj(o), Value::Num(n)) = (&slots[s as usize], &key) {
                match i.fast_set_elem(o, *n, v) {
                    Ok(()) => return Ok(()),
                    Err(back) => {
                        let obj = slots[s as usize].clone();
                        let k = i.to_property_key(&key)?;
                        i.set_member(&obj, &k, back)?;
                        return Ok(());
                    }
                }
            }
            let obj = slots[s as usize].clone();
            let k = i.to_property_key(&key)?;
            i.set_member(&obj, &k, v)?;
        }
        Op::UpdateProp(n, c, kind) => {
            let obj = pop!();
            let name = &chunk.names[n as usize];
            let cache = &chunk.caches[c as usize];
            let old = get_named_property(i, chunk, pc as usize, &obj, name, cache)?;
            if let Some(v) = step_value(i, &chunk.feedback, pc as usize, kind, old, |i, v| {
                set_named_property(i, chunk, pc as usize, &obj, name, v, cache)
            })? {
                push!(v);
            }
        }
        Op::UpdateElem(kind) => {
            let key = pop!();
            let obj = pop!();
            if !chunk.feedback.detailed_enabled() {
                if let (Value::Obj(o), Value::Num(nk)) = (&obj, &key) {
                    if let Some(Value::Num(old)) = i.fast_get_elem(o, *nk) {
                        let new = match kind {
                            UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard => old + 1.0,
                            UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard => old - 1.0,
                        };
                        if i.fast_set_elem(o, *nk, Value::Num(new)).is_ok() {
                            match kind {
                                UpdKind::PreInc | UpdKind::PreDec => push!(Value::Num(new)),
                                UpdKind::PostInc | UpdKind::PostDec => push!(Value::Num(old)),
                                UpdKind::IncDiscard | UpdKind::DecDiscard => {}
                            }
                            return Ok(());
                        }
                    }
                }
            }
            if chunk.feedback.detailed_enabled() {
                let old = get_element_profiled(i, chunk, pc as usize, &obj, &key)?;
                if let Some(v) = step_value(i, &chunk.feedback, pc as usize, kind, old, |i, v| {
                    set_element_profiled(i, chunk, pc as usize, &obj, &key, v)
                })? {
                    push!(v);
                }
                return Ok(());
            }
            if matches!(obj, Value::Undefined | Value::Null) {
                return Err(i.throw("TypeError", "cannot read property of null or undefined"));
            }
            let k = i.to_property_key(&key)?;
            let old = i.get_member(&obj, &k)?;
            if let Some(v) = step_value(i, &chunk.feedback, pc as usize, kind, old, |i, v| {
                i.set_member(&obj, &k, v)
            })? {
                push!(v);
            }
        }
        Op::ToPropKey => {
            if matches!(&*sp.sub(2), Value::Undefined | Value::Null) {
                return Err(i.throw("TypeError", "cannot access property of null or undefined"));
            }
            match &*sp.sub(1) {
                Value::Num(_) | Value::Str(_) => {}
                _ => {
                    let key = pop!();
                    let k = i.to_property_key(&key)?;
                    push!(Value::str(k.into_string()));
                }
            }
        }
        Op::ToPropKeyLocal(s) => {
            if matches!(slots[s as usize], Value::Undefined | Value::Null) {
                return Err(i.throw("TypeError", "cannot access property of null or undefined"));
            }
            match &*sp.sub(1) {
                Value::Num(_) | Value::Str(_) => {}
                _ => {
                    let key = pop!();
                    let k = i.to_property_key(&key)?;
                    push!(Value::str(k.into_string()));
                }
            }
        }
        Op::GetMethod(n, c) => {
            let obj = pop!();
            let m = get_named_property(
                i,
                chunk,
                pc as usize,
                &obj,
                &chunk.names[n as usize],
                &chunk.caches[c as usize],
            )?;
            push!(obj);
            push!(m);
        }
        Op::GetMethodElem => {
            let key = pop!();
            let obj = pop!();
            if chunk.feedback.detailed_enabled() {
                let m = get_element_profiled(i, chunk, pc as usize, &obj, &key)?;
                push!(obj);
                push!(m);
                return Ok(());
            }
            let m = if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                match i.fast_get_elem(o, *n) {
                    Some(v) => v,
                    None => {
                        let k = i.to_property_key(&key)?;
                        i.get_member(&obj, &k)?
                    }
                }
            } else if let (Value::Obj(_), Value::Str(s)) = (&obj, &key) {
                if !s.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
                    get_elem_str_ic(i, &obj, s)?
                } else {
                    let k = i.to_property_key(&key)?;
                    i.get_member(&obj, &k)?
                }
            } else {
                if matches!(obj, Value::Undefined | Value::Null) {
                    return Err(i.throw("TypeError", "cannot read property of null or undefined"));
                }
                let k = i.to_property_key(&key)?;
                i.get_member(&obj, &k)?
            };
            push!(obj);
            push!(m);
        }
        Op::Add => jit_bin_num(i, sp, &chunk.feedback, pc as usize, "+", |a, b| a + b)?,
        Op::Sub => jit_bin_num(i, sp, &chunk.feedback, pc as usize, "-", |a, b| a - b)?,
        Op::Mul => jit_bin_num(i, sp, &chunk.feedback, pc as usize, "*", |a, b| a * b)?,
        Op::Div => jit_bin_num(i, sp, &chunk.feedback, pc as usize, "/", |a, b| a / b)?,
        Op::Mod => jit_bin_num(
            i,
            sp,
            &chunk.feedback,
            pc as usize,
            "%",
            crate::eval::js_mod,
        )?,
        Op::BitAnd => jit_bin_i32(i, sp, &chunk.feedback, pc as usize, "&", |a, b| a & b)?,
        Op::BitOr => jit_bin_i32(i, sp, &chunk.feedback, pc as usize, "|", |a, b| a | b)?,
        Op::BitXor => jit_bin_i32(i, sp, &chunk.feedback, pc as usize, "^", |a, b| a ^ b)?,
        Op::Shl => jit_bin_i32(i, sp, &chunk.feedback, pc as usize, "<<", |a, b| {
            a.wrapping_shl(b as u32 & 31)
        })?,
        Op::Shr => jit_bin_i32(i, sp, &chunk.feedback, pc as usize, ">>", |a, b| {
            a >> (b as u32 & 31)
        })?,
        Op::UShr => jit_bin_num(i, sp, &chunk.feedback, pc as usize, ">>>", |a, b| {
            ((crate::eval::to_int32(a) as u32) >> (crate::eval::to_int32(b) as u32 & 31)) as f64
        })?,
        Op::Lt => jit_bin_cmp(i, sp, "<", |a, b| a < b)?,
        Op::Gt => jit_bin_cmp(i, sp, ">", |a, b| a > b)?,
        Op::Le => jit_bin_cmp(i, sp, "<=", |a, b| a <= b)?,
        Op::Ge => jit_bin_cmp(i, sp, ">=", |a, b| a >= b)?,
        Op::EqEq => jit_bin_cmp(i, sp, "==", |a, b| a == b)?,
        Op::NotEq => jit_bin_cmp(i, sp, "!=", |a, b| a != b)?,
        Op::StrictEq => jit_bin_cmp(i, sp, "===", |a, b| a == b)?,
        Op::StrictNotEq => jit_bin_cmp(i, sp, "!==", |a, b| a != b)?,
        Op::InstanceOf(c) => {
            let b = pop!();
            let a = pop!();
            let v = i.instanceof_ic(&a, &b, &chunk.caches[c as usize])?;
            push!(v);
        }
        Op::GenBin(n) => {
            let b = pop!();
            let a = pop!();
            let profiling = observe_arithmetic_operands(&chunk.feedback, pc as usize, &a, &b);
            let v = i.binary(&chunk.names[n as usize], a, b)?;
            observe_arithmetic_result(&chunk.feedback, pc as usize, profiling, &v);
            push!(v);
        }
        Op::Neg => {
            let a = pop!();
            let profiling = observe_arithmetic_operand(&chunk.feedback, pc as usize, &a);
            let v = match a {
                Value::Num(n) => Value::Num(-n),
                other => i.eval_unary_vm("-", other)?,
            };
            observe_arithmetic_result(&chunk.feedback, pc as usize, profiling, &v);
            push!(v);
        }
        Op::Plus => {
            let a = pop!();
            let profiling = observe_arithmetic_operand(&chunk.feedback, pc as usize, &a);
            let v = match a {
                Value::Num(n) => Value::Num(n),
                other => i.eval_unary_vm("+", other)?,
            };
            observe_arithmetic_result(&chunk.feedback, pc as usize, profiling, &v);
            push!(v);
        }
        Op::Not => {
            let a = pop!();
            let b = !i.to_boolean(&a);
            push!(Value::Bool(b));
        }
        Op::BitNot => {
            let a = pop!();
            let profiling = observe_arithmetic_operand(&chunk.feedback, pc as usize, &a);
            let v = match a {
                Value::Num(n) => Value::Num(!crate::eval::to_int32(n) as f64),
                other => i.eval_unary_vm("~", other)?,
            };
            observe_arithmetic_result(&chunk.feedback, pc as usize, profiling, &v);
            push!(v);
        }
        Op::Typeof => {
            let a = pop!();
            let v = i.eval_unary_vm("typeof", a)?;
            push!(v);
        }
        Op::TypeofName(n) => {
            push!(i.typeof_name_vm(&chunk.names[n as usize], env)?);
        }
        Op::Void => {
            pop!();
            push!(Value::Undefined);
        }
        Op::Call(argc, c) => {
            let argc = argc as usize;
            let args_ptr = sp.sub(argc);
            if chunk.feedback.detailed_enabled() {
                let args = std::slice::from_raw_parts(args_ptr, argc);
                let callee = (*sp.sub(argc + 1)).clone();
                let value = call_profiled(i, chunk, pc as usize, callee, Value::Undefined, args)?;
                *sp = jit_consume(*sp, argc + 1);
                push!(value);
                return Ok(());
            }
            // JIT→JIT fast call: on Some the arguments and the `this` slot were MOVED into the
            // callee — rewind the stack past them without dropping, then drop only the callee
            // slot. The `this` here is a local Undefined in a ManuallyDrop: the callee owns it on
            // Some (no double drop), and leaking it on None is a no-op (no payload).
            // The per-site callee cache short-circuits the dispatch guards on an identity hit;
            // a miss falls into `call_jit_fast`, which refills it.
            let mut undef = std::mem::ManuallyDrop::new(Value::Undefined);
            let mut r = i.call_jit_cached(
                &chunk.call_caches[c as usize],
                &*sp.sub(argc + 1),
                &raw mut *undef as *const Value,
                args_ptr,
                argc,
            );
            if r.is_none() {
                r = i.call_jit_fast(
                    &*sp.sub(argc + 1),
                    &raw mut *undef as *const Value,
                    args_ptr,
                    argc,
                    Some((&chunk.call_caches[c as usize], &chunk.call_pins)),
                );
            }
            if let Some(r) = r {
                *sp = args_ptr.sub(1);
                std::ptr::drop_in_place(*sp); // the callee
                let v = r?;
                push!(v);
                return Ok(());
            }
            let args = std::slice::from_raw_parts(args_ptr, argc);
            let callee = (*sp.sub(argc + 1)).clone();
            let v = i.call(callee, Value::Undefined, args)?;
            *sp = jit_consume(*sp, argc + 1);
            push!(v);
        }
        Op::LoadNameForCall(n, c) => {
            // A depth-0 cache hit/fill can't have come through a `with` object (see the VM arm).
            if let Some(v) = chunk
                .name_ic_hit(i, env, c)
                .or_else(|| chunk.name_ic_fill(i, env, n, c))
            {
                push!(Value::Undefined);
                push!(v);
            } else {
                let (callee, with_this) = i.get_var_with(&chunk.names[n as usize], env)?;
                push!(with_this.unwrap_or(Value::Undefined));
                push!(callee);
            }
        }
        Op::CallWithThis(argc, c) => {
            let argc = argc as usize;
            let args_ptr = sp.sub(argc);
            if chunk.feedback.detailed_enabled() {
                let args = std::slice::from_raw_parts(args_ptr, argc);
                let method = (*sp.sub(argc + 1)).clone();
                let this = (*sp.sub(argc + 2)).clone();
                let value = call_profiled(i, chunk, pc as usize, method, this, args)?;
                *sp = jit_consume(*sp, argc + 2);
                push!(value);
                return Ok(());
            }
            let mut r = i.call_jit_cached(
                &chunk.call_caches[c as usize],
                &*sp.sub(argc + 1),
                sp.sub(argc + 2),
                args_ptr,
                argc,
            );
            if r.is_none() {
                r = i.call_jit_fast(
                    &*sp.sub(argc + 1),
                    sp.sub(argc + 2),
                    args_ptr,
                    argc,
                    Some((&chunk.call_caches[c as usize], &chunk.call_pins)),
                );
            }
            if let Some(r) = r {
                *sp = args_ptr.sub(1);
                std::ptr::drop_in_place(*sp); // the method
                *sp = sp.sub(1); // `this` was consumed by the callee
                let v = r?;
                push!(v);
                return Ok(());
            }
            let args = std::slice::from_raw_parts(args_ptr, argc);
            let m = (*sp.sub(argc + 1)).clone();
            let this = (*sp.sub(argc + 2)).clone();
            let v = i.call(m, this, args)?;
            *sp = jit_consume(*sp, argc + 2);
            push!(v);
        }
        Op::New(argc, cache) => {
            let argc = argc as usize;
            if chunk.feedback.detailed_enabled() {
                let args_ptr = sp.sub(argc);
                let callee = (*sp.sub(argc + 1)).clone();
                let args = std::slice::from_raw_parts(args_ptr, argc);
                let value = construct_profiled(i, chunk, pc as usize, callee, args)?;
                *sp = jit_consume(*sp, argc + 1);
                push!(value);
            } else {
                unsafe { jit_new_inner(i, None, Some((chunk, cache)), argc, sp) }?;
            }
        }
        Op::NewArgsArray => {
            let args = argument_array_values(i, pop!());
            let callee = pop!();
            if chunk.feedback.detailed_enabled() {
                push!(construct_profiled(i, chunk, pc as usize, callee, &args)?);
            } else {
                push!(i.construct(callee, &args)?);
            }
        }
        Op::MakeRegExp(body, flags) => {
            let value = chunk.make_regexp_literal(i, pc as usize, body, flags)?;
            observe_allocation(
                &chunk.feedback,
                pc as usize,
                crate::feedback::AllocationObjectKind::RegExp,
                chunk.names[body as usize]
                    .len()
                    .saturating_add(chunk.names[flags as usize].len()),
            );
            push!(value);
        }
        Op::MakeArray(n) => {
            let n = n as usize;
            let mut items = Vec::with_capacity(n);
            let base = sp.sub(n);
            for k in 0..n {
                items.push(base.add(k).read());
            }
            *sp = base;
            let value = i.make_array(items);
            observe_allocation(
                &chunk.feedback,
                pc as usize,
                crate::feedback::AllocationObjectKind::Array,
                n,
            );
            push!(value);
        }
        Op::MakeObject(start, count, tidx) => {
            let count = count as usize;
            let mut values = Vec::with_capacity(count);
            let base = sp.sub(count);
            for k in 0..count {
                values.push(base.add(k).read());
            }
            *sp = base;
            let keys = &chunk.names[start as usize..start as usize + count];
            let v = if tidx != u32::MAX {
                i.make_plain_object_templated(&chunk.obj_maps[tidx as usize], keys, values)
            } else {
                i.make_plain_object_vm(keys, values)
            };
            observe_allocation(
                &chunk.feedback,
                pc as usize,
                crate::feedback::AllocationObjectKind::Object,
                count,
            );
            push!(v);
        }
        Op::ToStr => {
            let v = pop!();
            let s = i.to_string(&v)?;
            push!(Value::Str(s));
        }
        Op::GetIter => {
            let v = pop!();
            let (it, nx) = i.get_iterator(&v)?;
            push!(it);
            push!(nx);
        }
        Op::GetAsyncIter => {
            let v = pop!();
            let opened = VmDelegate::open(i, v, true)?;
            push!(opened.iterator);
            push!(opened.next);
            push!(Value::Bool(opened.from_sync));
        }
        Op::ForInKeys => {
            let source = pop!();
            let keys = i
                .for_in_keys(&source)?
                .into_iter()
                .map(Value::from_string)
                .collect();
            push!(i.make_array(keys));
        }
        Op::ForInStepL(keys, index, source) => {
            if let Some(key) = for_in_step(i, slots, keys, index, source)? {
                push!(key);
                push!(Value::Bool(true));
            } else {
                push!(Value::Undefined);
                push!(Value::Bool(false));
            }
        }
        Op::IterStepL(is, ns) => {
            let it = slots[is as usize].clone();
            let nx = slots[ns as usize].clone();
            match i.iterator_step(&it, &nx)? {
                Some(v) => {
                    push!(v);
                    push!(Value::Bool(true));
                }
                None => {
                    push!(Value::Undefined);
                    push!(Value::Bool(false));
                }
            }
        }
        Op::IterCloseL(s) => {
            let it = slots[s as usize].clone();
            i.iterator_close_normal(&it)?;
        }
        Op::IterAbortL(s) => {
            let exc = pop!();
            let it = slots[s as usize].clone();
            i.iterator_close(&it);
            return Err(Abrupt::Throw(exc));
        }
        Op::Throw => {
            let v = pop!();
            return Err(Abrupt::Throw(v));
        }
        Op::ResetSlots(start, count) => {
            for k in start as usize..start as usize + count as usize {
                slots[k] = Value::Undefined;
            }
        }
        Op::Jump(_)
        | Op::StoreConstLocal(..)
        | Op::StoreConstCap(_)
        | Op::UpdateConst(..)
        | Op::RequireObject
        | Op::DestructureStepL(..)
        | Op::DestructureRestL(..)
        | Op::IterCloseIfNotDoneL(..)
        | Op::IterAbortIfNotDoneL(..)
        | Op::ObjectRest(_)
        | Op::EvalExpr(_)
        | Op::PushWith
        | Op::PushLex(_)
        | Op::PushCatchLex(_)
        | Op::CloneLex(_)
        | Op::InitLex(_)
        | Op::PopEnv
        | Op::ResolveNameRef(..)
        | Op::LoadRef(_)
        | Op::StoreRef(_)
        | Op::PushDisposeFrame
        | Op::AddDisposable(_)
        | Op::DisposeNormal
        | Op::DisposeThrow
        | Op::DisposeReturn
        | Op::DisposeBareReturn
        | Op::DisposeResumeReturn
        | Op::DisposeJump
        | Op::NewArray
        | Op::ArrayPush
        | Op::ArrayHole
        | Op::ArraySpread
        | Op::EvalCallArgsArray
        | Op::NewObject
        | Op::ObjectData(_)
        | Op::ObjectSpread
        | Op::ObjectProto
        | Op::ObjectMethod(..)
        | Op::ImportMeta
        | Op::NewTarget
        | Op::DynamicImport(..)
        | Op::PrivateIn(_)
        | Op::GetPrivate(_)
        | Op::GetPrivateKeep(_)
        | Op::GetPrivateMethod(_)
        | Op::SetPrivate(_)
        | Op::UpdatePrivate(..)
        | Op::SuperCallStart
        | Op::SuperCallArgsArray
        | Op::LoadLexicalThis
        | Op::SuperThis
        | Op::SuperBase
        | Op::SuperGet
        | Op::SuperGetKeep
        | Op::SuperGetMethod
        | Op::SuperSet
        | Op::SuperUpdate(_)
        | Op::TemplateObject(_)
        | Op::RequireCallable
        | Op::ClassStart(..)
        | Op::ClassHeritage(..)
        | Op::ClassKey(..)
        | Op::ClassDecorator(..)
        | Op::ClassFinish(_)
        | Op::ClassAbort(_)
        | Op::AbruptJump(..)
        | Op::JumpIfFalse(_)
        | Op::JumpIfFalsePeek(_)
        | Op::JumpIfTruePeek(_)
        | Op::JumpIfNotNullishPeek(_)
        | Op::InlineGuard(..)
        | Op::Return
        | Op::ReturnBare
        | Op::ResumeReturn
        | Op::ResumeJump
        | Op::ReturnUndef
        | Op::Await
        | Op::AsyncIterStepL(..)
        | Op::AsyncIterResumeL(..)
        | Op::AsyncIterCloseL(..)
        | Op::Yield
        | Op::YieldStar
        | Op::PushHandler(_)
        | Op::PushFinally(..)
        | Op::PushIterator(..)
        | Op::PopHandler => unreachable!("control-flow op reached jit_exec"),
    }
    Ok(())
}

/// Drop `n` consumed operands below `sp` (post-call cleanup) and return the new top.
unsafe fn jit_consume(sp: *mut Value, n: usize) -> *mut Value {
    let base = sp.sub(n);
    for k in 0..n {
        // Tag peek: trivially-copyable tags (repr(u8) discriminants 0..=4) skip the outlined
        // drop — operands are overwhelmingly numbers.
        let p = base.add(k);
        if *(p as *const u8) >= 5 {
            std::ptr::drop_in_place(p);
        }
    }
    base
}

unsafe fn jit_bin_num(
    i: &mut Interp,
    sp: &mut *mut Value,
    feedback: &crate::feedback::FeedbackVector,
    pc: usize,
    op: &'static str,
    f: impl Fn(f64, f64) -> f64,
) -> Result<(), Abrupt> {
    *sp = sp.sub(1);
    let b = sp.read();
    *sp = sp.sub(1);
    let a = sp.read();
    let profiling = observe_arithmetic_operands(feedback, pc, &a, &b);
    let v = if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
        Value::Num(f(*x, *y))
    } else {
        i.binary(op, a, b)?
    };
    observe_arithmetic_result(feedback, pc, profiling, &v);
    sp.write(v);
    *sp = sp.add(1);
    Ok(())
}

unsafe fn jit_bin_i32(
    i: &mut Interp,
    sp: &mut *mut Value,
    feedback: &crate::feedback::FeedbackVector,
    pc: usize,
    op: &'static str,
    f: impl Fn(i32, i32) -> i32,
) -> Result<(), Abrupt> {
    *sp = sp.sub(1);
    let b = sp.read();
    *sp = sp.sub(1);
    let a = sp.read();
    let profiling = observe_arithmetic_operands(feedback, pc, &a, &b);
    let v = if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
        Value::Num(f(crate::eval::to_int32(*x), crate::eval::to_int32(*y)) as f64)
    } else {
        i.binary(op, a, b)?
    };
    observe_arithmetic_result(feedback, pc, profiling, &v);
    sp.write(v);
    *sp = sp.add(1);
    Ok(())
}

unsafe fn jit_bin_cmp(
    i: &mut Interp,
    sp: &mut *mut Value,
    op: &'static str,
    f: impl Fn(f64, f64) -> bool,
) -> Result<(), Abrupt> {
    *sp = sp.sub(1);
    let b = sp.read();
    *sp = sp.sub(1);
    let a = sp.read();
    let v = if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
        Value::Bool(f(*x, *y))
    } else {
        i.binary(op, a, b)?
    };
    sp.write(v);
    *sp = sp.add(1);
    Ok(())
}

/// Conditional-branch helper: evaluates the branch predicate per `mode` (see `jit::COND_*`),
/// returning the new sp and the flag. `to_boolean` cannot throw, so sp is never null here.
pub(crate) unsafe extern "C" fn jit_cond(
    ctx: *mut crate::jit::JitCtx,
    packed_mode: u32,
    mut sp: *mut Value,
) -> crate::jit::SpFlag {
    let ctx = &mut *ctx;
    let i = &mut *ctx.interp;
    let profiled = packed_mode & (1 << 2) != 0;
    let take_when_true = packed_mode & (1 << 3) != 0;
    let mode = packed_mode & 0x3;
    let flag = match mode {
        crate::jit::COND_POP_TRUTHY => {
            sp = sp.sub(1);
            let v = sp.read();
            i.to_boolean(&v) as u64
        }
        crate::jit::COND_PEEK_TRUTHY => i.to_boolean(&*sp.sub(1)) as u64,
        _ => !matches!(&*sp.sub(1), Value::Undefined | Value::Null) as u64,
    };
    if profiled {
        let taken = (flag != 0) == take_when_true;
        (&*ctx.chunk)
            .feedback
            .observe_branch((packed_mode >> 4) as usize, taken);
    }
    crate::jit::SpFlag { sp, flag }
}

/// Return helper: mode 1 pops the return value into `ctx.ret`; mode 0 returns undefined.
pub(crate) unsafe extern "C" fn jit_return(
    ctx: *mut crate::jit::JitCtx,
    mode: u32,
    mut sp: *mut Value,
) -> *mut Value {
    let ctx = &mut *ctx;
    ctx.ret = if mode == 1 {
        sp = sp.sub(1);
        sp.read()
    } else {
        Value::Undefined
    };
    sp
}

pub(crate) unsafe extern "C" fn jit_push_handler(
    ctx: *mut crate::jit::JitCtx,
    catch_pc: u32,
    sp: *mut Value,
) -> *mut Value {
    let ctx = &mut *ctx;
    let depth = sp.offset_from(ctx.stack_base) as usize;
    ctx.handlers.push((catch_pc, depth));
    sp
}

pub(crate) unsafe extern "C" fn jit_pop_handler(
    ctx: *mut crate::jit::JitCtx,
    _imm: u32,
    sp: *mut Value,
) -> *mut Value {
    (*ctx).handlers.pop();
    sp
}

/// Throw routing: land on the innermost `try` handler (returning its code address and the
/// unwound sp with the exception pushed), or (0, sp) to leave the function throwing.
pub(crate) unsafe extern "C" fn jit_unwind(
    ctx: *mut crate::jit::JitCtx,
    _imm: u32,
    sp: *mut Value,
) -> crate::jit::SpFlag {
    let ctx = &mut *ctx;
    // Only thrown completions are catchable; anything else propagates out.
    if !matches!(ctx.error, Some(Abrupt::Throw(_))) {
        return crate::jit::SpFlag {
            sp: std::ptr::null_mut(),
            flag: sp as u64,
        };
    }
    if ctx.handlers.len() <= ctx.handler_floor {
        // No handler belongs to THIS activation (a shared-ctx direct call must not consume
        // its caller's regions — their depths are relative to a different stack base).
        return crate::jit::SpFlag {
            sp: std::ptr::null_mut(),
            flag: sp as u64,
        };
    }
    match ctx.handlers.pop() {
        None => crate::jit::SpFlag {
            sp: std::ptr::null_mut(),
            flag: sp as u64,
        },
        Some((catch_pc, saved_depth)) => {
            // A handler may be installed while operands needed for BindingInitialization are
            // still live; the protected operation can consume them before throwing. Vec::truncate
            // (used by the bytecode VM) keeps the smaller current depth in that case. Mirroring
            // it here is essential: restoring the older, deeper depth would expose moved-out raw
            // stack entries and later drop them a second time.
            let current_depth = sp.offset_from(ctx.stack_base) as usize;
            let depth = saved_depth.min(current_depth);
            let target = ctx.stack_base.add(depth);
            // Drop operands above the handler's depth.
            let mut p = target;
            while p < sp {
                std::ptr::drop_in_place(p);
                p = p.add(1);
            }
            let Some(Abrupt::Throw(exc)) = ctx.error.take() else {
                unreachable!()
            };
            target.write(exc);
            let addr = ctx.code_base as usize + *ctx.pc_offsets.add(catch_pc as usize) as usize;
            crate::jit::SpFlag {
                sp: addr as *mut Value,
                flag: target.add(1) as u64,
            }
        }
    }
}

/// Full host-control poll reached by the generated tier's cheap loop divider.
pub(crate) unsafe extern "C" fn jit_interrupt(
    ctx: *mut crate::jit::JitCtx,
    _imm: u32,
    sp: *mut Value,
) -> crate::jit::SpFlag {
    let ctx = unsafe { &mut *ctx };
    match unsafe { &mut *ctx.interp }.interrupt_poll_force() {
        Ok(()) => crate::jit::SpFlag { sp, flag: 0 },
        Err(abrupt) => {
            ctx.error = Some(abrupt);
            crate::jit::SpFlag { sp, flag: 1 }
        }
    }
}

/// Record one completed unconditional loop back-edge from a detailed JIT chunk. The generated
/// caller has already performed the branch's normal stack/interrupt bookkeeping.
pub(crate) unsafe extern "C" fn jit_loop_backedge(
    ctx: *mut crate::jit::JitCtx,
    pc: u32,
    sp: *mut Value,
) -> *mut Value {
    let ctx = unsafe { &*ctx };
    unsafe { (&*ctx.chunk).feedback.observe_loop_backedge(pc as usize) };
    sp
}
