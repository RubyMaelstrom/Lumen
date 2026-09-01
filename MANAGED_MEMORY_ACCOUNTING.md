# Managed-memory accounting design

Status: implementation in vertical slices. The post-collection visitor now classifies every
`Interp` field, traverses the engine-owned side tables/caches described below, reports ordinary
`ArrayBuffer` backing by identity, and exposes an opt-in retained-size contract for host state.
SharedArrayBuffer backing is reported by address-independent Shared Data Block identity. Typed
external-allocation observations now cover the built-in Lumen-web Wasm store and TRust's wasmi
store without double-counting aliased `Memory.buffer` Data Blocks. Any host entry that does not
implement the retained-metadata contract still makes the host category unavailable rather than
silently contributing zero. The Phase 0 total therefore remains explicitly incomplete.

## Why object count is not a byte count

Lumen's current cycle collector registers `Rc<RefCell<Object>>` and `Scope` graph nodes, but much
of a live Agent's memory is owned outside those node bodies:

- `Props` owns named-entry capacity plus optional dense-slot, numeric-mirror, packed-element, and
  hash-index storage.
- values share `LStr`, `Rc<str>`, Symbol, BigInt, callable, closure-environment, and AST/bytecode
  allocations.
- `Interp` owns pointer-keyed internal-slot tables for realms, promises, proxies, collections,
  modules, regular expressions, typed arrays, and host state.
- `ArrayBuffer`, `SharedArrayBuffer`, typed-array, WebAssembly, and host resources retain backing
  allocations that can dominate their small JS wrapper objects.
- bounded parser, string-unit, RegExp-program, template, and inline-cache storage is retained for
  reuse but is not necessarily reachable by following ordinary object properties.
- the shell's `ClassAlloc` rounds small requests to size classes and caches freed blocks. RSS also
  includes code, static Unicode/CLDR data, native stacks, executable JIT pages, and allocator
  fragmentation.

Consequently, `live_objects * size_of::<Object>()`, current RSS, and allocator residency answer
three different questions. None may be labelled as Lumen's live managed heap.

## Proposed measurement contract

At an explicit Agent safepoint after a forced collection, report all three layers separately:

1. `managed_requested_bytes`: unique live allocations semantically owned by the Agent, counting
   container capacity rather than length and deduplicating shared allocations by stable identity.
2. `managed_external_bytes`: engine backing storage whose lifetime is controlled by a managed
   wrapper but whose allocation is external to ordinary object/string/container storage. Buffers
   and WebAssembly memories belong here, with a category breakdown. Host resources are a sibling
   diagnostic and are never summed into this value, so shell and TRust records remain explicit
   about their different embedder payloads.
3. `allocator_live_bytes`, `allocator_cached_bytes`, and process current/peak RSS: implementation
   residency. These diagnose fragmentation and cache policy but do not replace layers 1 and 2.

The managed record should include at least these additive categories:

- object/Rc-cell bodies;
- property entries, indexes, dense slots, numeric mirrors, and packed elements;
- scope/Rc-cell bodies, binding maps, lexical-name storage, and binding-owned names;
- unique Lumen strings, `Rc<str>` strings, Symbols, and BigInts;
- callable, function-source/AST, bytecode, feedback, and inline-cache storage;
- Realm/module/promise/collection/RegExp/Temporal and other interpreter side tables;
- bounded engine caches, reported both as retained and as configured limit;
- `ArrayBuffer`/shared buffer/typed-array and WebAssembly backing stores;
- host-owned resources retained on behalf of the Agent, separately labelled so a standalone shell
  and TRust do not appear directly comparable when their host payload differs.

Every report must state whether a category is exact, a documented lower bound, unavailable, or
excluded. An incomplete category must not silently contribute zero.

## Implementation choices

### A. Safepoint retention visitor

Walk the collector snapshots and every `Interp` ownership table after collection. A measurement
context maintains identity sets for each shared allocation family and adds capacities through
type-specific `retained_size` methods.

Advantages: no hot-path cost; directly answers what the live Agent retains; categorical output
makes omissions visible. Disadvantages: broad and maintenance-heavy; every new side table and
shared allocation type must participate; exact `RcBox` allocator overhead is not a stable Rust
layout contract, so the primary number should be requested payload/capacity bytes.

### B. Categorized allocation ledger

Route managed allocations through category-aware wrappers that update live/peak requested-byte
counters at allocation, reallocation, ownership transfer, and free.

Advantages: exact requested allocation history and peak values; naturally sees temporary
allocations between collections. Disadvantages: touches hot paths, is difficult for standard
containers and shared ownership, and risks measuring the instrumentation unless disabled-cost and
overflow behavior are proven.

### C. Global allocator counters

Teach `ClassAlloc` to report requested, size-class, cached, and system-returned bytes.

Advantages: useful and comparatively small. Disadvantages: includes all Rust/process allocations,
cannot assign Agent ownership, and changes benchmark overhead if implemented with per-allocation
atomics. This is supplementary residency telemetry, not a managed-heap implementation.

## Recommendation

Use A as the authoritative post-collection Phase 0 measurement, with a versioned category enum and
an explicit completeness bitmap. Add C later as a separate allocator/fragmentation diagnostic.
Keep allocation-rate counters already present in the collector for now; introduce B only for
specific high-value allocation families or after Phase 3A centralizes allocation.

Build the visitor in vertical slices. First cover objects, properties/elements, scopes, strings,
and buffer backing stores; make the report say that side tables/code/caches are incomplete. Then
cover every `Interp` owner and refuse to call the total complete until an inventory test accounts
for all owning fields. Tests should construct shared strings/buffers, verify identity
deduplication, distinguish `Vec::capacity` from `len`, force a collection, and show that releasing
the final owner lowers the corresponding category exactly.

Current implementation notes:

- `LUMEN_PERF_METRICS=1` records after every diagnostic-mode collection and makes the standalone
  shell force one final collection. Each visitor runs after the collector pause timer stops, so
  the diagnostic walk does not inflate the reported pause.
- Exact categories mean exact requested payload/capacity according to public Rust container
  information. They exclude allocator rounding and Rust's private `RcBox` header.
- A standard-library `HashMap` makes its containing storage category a lower bound: its entry
  payload can be described, but its private bucket/control allocation cannot be measured exactly.
- The test-only inventory macro expands one checked-in classification list into an exhaustive
  `Interp` struct pattern with no `..`, so a newly added field fails test compilation until
  classified. The inventory now rejects every `unaccounted` entry. `complete` remains false until
  live host entries have retained-metadata reporters and every remaining lower-bound reason has
  been audited.

Allocation attribution rules for the remaining slices:

- Use one visitor context and one identity set per allocation family across objects, scopes,
  callables, functions, chunks, caches, side tables, and sub-realms.
- Credit an allocation to its canonical family, not to the edge that discovered it. A cache owns
  only its table/order overhead; a cached string, function, chunk, or RegExp program is credited
  to that payload's canonical category through the shared identity set.
- Do not descend from callables or chunks into registered captured environments: the collector's
  scope snapshot already accounts those graph nodes and their storage.
- Keep executable mappings and their bounded code-memory budget separate from requested managed
  payload. Heap-side JIT metadata is also reported separately, never silently folded into either
  allocator residency or executable bytes.
- Per-Agent records carry both Agent and collector-heap identity. SharedArrayBuffer records use the
  engine's address-independent Shared Data Block id and an `externally_shared` marker so a process
  aggregator can dedupe them across Agents without changing the useful per-Agent retained view.
  Agent-local Wasm backing uses typed embedder allocation identities. A backing identified with
  Lumen's `ArrayBufferBytes` is attributed canonically to Wasm and suppressed from the ordinary
  ArrayBuffer total; separately allocated mirrors retain distinct identities and both allocations
  remain visible.

The Function/Chunk vertical slice now follows user callables through both compiled generations,
deduplicates shared Functions, Chunks, hoist plans, and JIT sidecars, and accounts the principal
bytecode, constant, name, feedback/IC, captured-binding, template, and plan-vector capacities.
The recursive AST visitor exhaustively matches every `Stmt`, `Expr`, `Pattern`, class, import, and
property variant; vector capacities and `Box` targets are local storage, while shared Functions,
Classes, strings, and BigInts use allocation-family identity sets. Several uncommon Chunk plans
are also traversed, including generic eval/assignment expressions, class plans, initialized
RegExp literals, constructor-plan vectors, forwarding chunks, and call-pin entry payloads. The
Chunk category remains a lower bound because Rust's HashMap bucket capacity is opaque. JIT
`pc_offsets` and its Rust payload are reported as heap metadata; executable mappings remain
exclusively in generated-code metrics.

The bounded string/RegExp cache slice scans UTF-16 views, prepared subjects, the ASCII hot entry,
and compiled-program cache entries plus stale recency keys. Cache tables and recency queues credit
only their own storage to `engine_caches`; pinned strings and matcher payloads pass through the
global string/RegExp identity registries. The diagnostic does not reuse the cache eviction byte
estimate as an exact total: that estimate may conservatively double-count shared matcher graphs,
whereas retained-memory reporting now uses a direct-allocation lower bound until shared character
classes and nested lookaround programs gain identity-aware traversal.

Live RegExp object-to-program pins and deferred legacy-match state now use those same registries.
Their pointer table and capture-vector storage is credited to `interpreter_side_tables`; the
program, prepared subject, and input string remain credited to their canonical payload families.
That side-table category stays a documented lower bound until the rest of the exhaustive `Interp`
inventory is covered.

Realm-local well-known-symbol/key caches now contribute their vector storage and route Symbol and
`Rc<str>` payloads through the shared identity sets. Root-plus-ShadowRealm measurement now walks
every collector heap with one visitor, collects each sub-heap before a diagnostic safepoint, and
credits the Agent-wide symbol registry exactly once. Per-realm object/scope/container storage is
summed; shared strings, symbols, Functions, RegExp programs, and ArrayBuffer backing are globally
deduplicated. The diagnostic snapshot map itself is excluded because it is measurement output,
not workload-retained engine state.

Map/Set and WeakMap/WeakSet side tables now include ordered-entry vector capacities, collision
vectors, and both levels of their hash indexes. Strong collection keys/values and live ephemeron
values pass through the canonical Value visitor; weak keys remain weak during diagnostics. Hash
table bucket capacity remains a documented lower bound because `FastMap` is a standard-library
HashMap alias rather than an allocator-introspectable container.

ArrayBuffer ownership now separates byte backing from engine metadata: owner/version/dirty-range
tables, TypedArray/DataView records, immutable and host-detach-key sets, and view-to-buffer Values
credit `interpreter_side_tables`, while the identity-deduplicated byte capacity remains in
`array_buffer_backing`. SharedArrayBuffer backing is credited once per address-independent Shared
Data Block id across the root realm and ShadowRealms; each allocation record is marked externally
shared so a process-level consumer can deduplicate the same block across Agents. Missing registry
identities make the category unavailable.

Reusable execution storage now contributes to `engine_caches`: bytecode slot/operand Vec pools,
the megamorphic stub table and retained names, raw fixed-size JIT frame buffers, and weak
creation/global-environment pin containers. Pooled VM vectors and raw frame buffers are required
to contain no live Values; a diagnostic assertion protects that ownership invariant.

Realm metadata now covers error/extra-prototype registries, captured console String capacity,
eval-realm identities, import base/meta state, constructor capacity hints, and weak HTMLDDA brand
entries. GC object payload remains canonical to the collector snapshot; these tables credit only
their own entry/string storage.

Module ownership now includes module/namespace tables, Rc-owned parsed statement bodies, resolved
dependency/export/import maps, async-evaluation bookkeeping, pending dynamic-import request
strings and promise Values, and namespace binding names. Module environments remain canonical to
the scope snapshot. Host loader closures are classified external because an opaque embedder
closure cannot safely report zero retained bytes.

Promise ownership includes pending reaction vectors, rejection tracking, queued Promise jobs,
forwarding entries, the Agent `[[KeptAlive]]` vector, and host-settings bookkeeping. JavaScript
Values retain their canonical allocation-family attribution, while saved environments remain in
the scope snapshot. Per-settings global-name sets are identity-deduplicated across their shared
`Rc` owners; opaque map/set bucket storage keeps the category explicitly lower-bound.

Generator ownership includes each boxed heap VM continuation, its slots/operand stack, prepared
references and class state, handler/disposal frames, delegation/async-close state, and queued
async-generator requests. Chunks and JavaScript payloads retain canonical family attribution;
suspended environments remain canonical to the scope snapshot. Lumen's current continuations own
no OS thread or native stack, correcting an obsolete interpreter comment from the former design.

Weak-reference ownership includes WeakRef entries, FinalizationRegistry cell buffers, cleanup
callbacks and held values, and the queued cleanup-job vector. Weak targets and unregister tokens
are never upgraded or visited by diagnostics; their inline `Weak` handles are included in table or
cell payload, while private `Rc` allocation headers remain outside requested-byte accounting.

Active execution ownership includes legacy reflection-frame capacity and rare boxed frame state,
proper-tail-call argument buffers, inferred function-name storage, `using` disposal frames,
decorator initializer scratch, and active/pending `new.target` Values. Lazy argument slices are
identity-deduplicated; their function and Value payloads route to canonical families, and their
captured environments remain canonical to the scope snapshot.

Object-side ownership now covers GC pin, Proxy, host-indexed-property, template-object, Annex B
function, deferred-namespace, mapped-arguments, and module-source tables. Table/string/vector
storage is credited to interpreter side tables; Functions and JavaScript Values retain canonical
family attribution, while mapped argument environments remain canonical to the scope snapshot.

Class-construction ownership includes constructor metadata, field/transform/private-member
buffers, property keys and accessor boxes, and fixed construct-IC entries. Initializer expression
trees are credited to the canonical AST/function-bytecode family; field environments remain in
the scope snapshot, and construct-cache weak pins are never upgraded by diagnostics.

Realm ownership includes the additional-realm table, each realm's error/extra-prototype maps, and
identity-deduplicated global-name sets, including a transient constructor-caller realm snapshot.
Globals and intrinsic objects remain canonical to the object snapshot; global environments remain
canonical to the scope snapshot even when several realm/settings records share them.

Temporal ownership includes the internal-slot table, calendar-id table, ZonedDateTime time-zone
identifiers, and calendar identifiers. Shared `Rc<str>` payloads use the visitor's global string
identity set, so repeated canonical zone/calendar names are credited once across all records.

Agent-event ownership includes pending `Atomics.waitAsync` and timer vector capacity, retained
promise/callback Values, the boxed Agent channel bundle, and its broadcast-sender vector. Rust's
standard-library channel backing and queued messages expose no retained-size API, so any live
receiver/sender keeps the category explicitly lower-bound with that reason rather than reporting
the visible handles as an exact total.

Collector infrastructure includes each identity-deduplicated `GcState` payload, object-registry
and free-list capacity, weak scope-registry capacity, and shape-transition entry/key storage.
Object/scope bodies remain in their existing canonical categories. The embedder wall-clock closure
and shared runtime-interrupt handle are classified external: captured closure state and cross-engine
Arc ownership cannot be assigned honestly to one Agent's managed total.

Host ownership uses the public `RetainedBytes` contract. Existing `OpState::put` and
`ResourceTable::add` calls remain source-compatible but deliberately register an unreported entry;
the `host_resources` category then emits `null`/`unavailable`. Embedders that can describe owned
capacity use `put_retained` or `add_retained`; Lumen adds the inline value size and aggregates the
reported payload across the root realm and every ShadowRealm. HashMap bucket storage and private
`Rc` allocation metadata remain a documented lower bound. Host bytes stay a sibling category and
are not added to either engine-managed composite.

External host backing uses the independent `RetainedExternalMemory` contract, so an embedder may
report Wasm linear memory even while its other opaque host metadata remains unavailable. Each
allocation carries a typed identity. The built-in Lumen-web store reports the same
`ArrayBufferBytes` identity used by `Memory.buffer`, causing one canonical Wasm credit. TRust's
wasmi store reports store-plus-slot identities; its keyed ArrayBuffer synchronization mirror is a
second real allocation and therefore remains in `array_buffer_backing`. Duplicate external
identities are credited once, while conflicting sizes keep the category explicitly lower-bound.

## Questions worth outside review

1. Is a safepoint retention visitor the right short-term contract, or should Phase 0 explicitly
   accept object/scope populations plus RSS until Phase 3A centralizes the heap?
2. Should the canonical managed number use requested payload/capacity bytes, allocator-rounded
   retained bytes, or report both while treating the former as cross-allocator comparable?
3. Should parsed AST/bytecode/JIT metadata be part of `managed_requested_bytes` or a sibling
   `engine_code_and_metadata_bytes` total? The roadmap's code-memory budget benefits from keeping
   executable and metadata categories independently visible even if a composite includes both.
4. How much completeness machinery is warranted to prevent a newly added `Interp` owning field
   from being omitted silently?
