# Managed-memory accounting design

Status: implementation in vertical slices. The first post-collection visitor reports collector
object/scope payloads, property/binding capacity, reachable shared string/Symbol/BigInt and
callable lower bounds, and deduplicated ordinary `ArrayBuffer` capacity. Its versioned record
marks the remaining side-table, cache, AST/bytecode, shared/Wasm, and host-resource owners as
unavailable; the Phase 0 total is therefore explicitly a lower bound and the roadmap item remains
open.

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
2. `managed_external_bytes`: backing storage whose lifetime is controlled by a managed wrapper but
   whose allocation is external to ordinary object/string/container storage. Buffers, WebAssembly
   memories, and embedder resources belong here, with a category breakdown.
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
- Adding the remaining owners is still required before `complete` can become true. A test-only
  inventory macro expands one checked-in classification list into an exhaustive `Interp` struct
  pattern with no `..`, so a newly added field fails test compilation until classified. Its
  temporary `unaccounted` class is review-visible and must reach zero before completion.

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
- Per-Agent records carry both Agent and collector-heap identity. Shared/Wasm memory will need a
  stable allocation identity and an externally-shared marker so a process aggregator can dedupe
  it across Agents without changing the useful per-Agent retained view.

The Function/Chunk vertical slice now follows user callables through both compiled generations,
deduplicates shared Functions, Chunks, hoist plans, and JIT sidecars, and accounts the principal
bytecode, constant, name, feedback/IC, captured-binding, template, and plan-vector capacities.
The recursive AST visitor exhaustively matches every `Stmt`, `Expr`, `Pattern`, class, import, and
property variant; vector capacities and `Box` targets are local storage, while shared Functions,
Classes, strings, and BigInts use allocation-family identity sets. Several uncommon Chunk plans
remain a documented lower bound. JIT `pc_offsets` and its Rust payload are reported as heap
metadata; executable mappings remain exclusively in generated-code metrics.

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
