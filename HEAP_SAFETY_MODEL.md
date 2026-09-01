# Phase 3A heap safety and migration model

Status: design checkpoint. This document is a safety contract for the first central-heap slice;
it does not replace the current `Rc<RefCell<_>>` collector and does not change JavaScript behavior.
No copying nursery or moving object family may be enabled until the invariants and verification
gates below have executable tests.

## Normative boundary

ECMAScript does not prescribe a particular garbage collector or object address. The implementation
may move, compact, or defer collection as long as the language algorithms remain equivalent. The
boundaries that constrain this design are:

- ECMA-262 §6.1 defines the language values; a heap handle is an implementation representation of
  an Object, String, Symbol, or BigInt value and must never be observable as a number or pointer.
- ECMA-262 §6.2.4 defines Completion Records. `Empty` is an internal completion value and may not
  escape a frame, host callback, promise settlement, or public evaluation result.
- ECMA-262 §9.3 defines Agents and their execution contexts and job queues. One Agent owns one
  heap and executes JavaScript logically on one thread; ShadowRealms in that Agent share ownership
  policy but have independent language environments and intrinsics.
- ECMA-262 §6.2.9 defines Data Blocks. `ArrayBuffer`, `SharedArrayBuffer`, and WebAssembly
  backing are separate byte-storage identities, not ordinary movable object payloads.
- ECMA-262's WeakRef/FinalizationRegistry and WeakMap/WeakSet clauses constrain weak liveness,
  ephemeron fixed points, cleanup-job timing, and unregister tokens. A collector policy must not
  promote a weak edge to a strong root or expose collection timing as a language result.
- The WebAssembly JavaScript Interface's Memory and Shared Data Block rules require aliases and
  sharing to retain one backing identity across views and Agents. Wasm and shared backing therefore
  use external identities and pressure accounting, not cage relocation.

Authoritative references: <https://tc39.es/ecma262/> and
<https://webassembly.github.io/spec/js-api/>. These references govern observable ordering and
liveness; the layout and algorithms below are engine-owned implementation choices.

## Safety invariants

The central heap must preserve these invariants in every build, including the deterministic stress
collector:

1. **Owned references only.** A managed field is either an immediate `TaggedValue`, a validated
   `HeapRef`, a registered rooted handle, or an explicitly external identity. It may not contain an
   unregistered native pointer, an `Rc` address encoded as a number, or a raw pointer retained over
   an allocation, host call, interrupt poll, or back-edge safepoint.
2. **Agent ownership.** Every movable allocation belongs to exactly one Agent heap. A value crossing
   an Agent boundary is structured-cloned, a shared-data identity, or an explicit host handle; a
   cage offset from one Agent is never interpreted by another.
3. **Validated references.** Offset/handle zero is invalid. Resolution checks the heap identity,
   generation cookie in stress builds, object type/layout, and allocation bounds before access.
   Stale, wrong-kind, and recycled references fail verification rather than becoming `undefined`.
4. **Exact safepoints.** Collection can occur only at declared allocation, runtime/host call,
   interrupt, back-edge, and explicit collection points. Each safepoint has a root map for tagged
   registers, tagged frame slots, interpreter fields, handles, suspended jobs, and continuations.
5. **No hidden hybrid edges.** During migration, an old `Rc` object graph may point to another old
   object or to a registered migration handle. A new heap object may point only to new heap handles,
   immediates, external identities, or an audited compatibility cell. Raw `Rc` values are never
   reachable solely through a new object descriptor.
6. **Mutation is barriered.** Every store records an old-to-young edge when the source is old and
   the destination is young. The barrier is part of the field/element write primitive, not a best
   effort at individual call sites.
7. **Weakness is explicit.** WeakMap/WeakSet keys, WeakRef targets, unregister tokens, and
   FinalizationRegistry targets are represented in weak tables. They do not enter the strong root
   set during ordinary tracing or retained-memory diagnostics.
8. **Relocation is atomic to the Agent.** No JavaScript, host callback, or observer runs while roots
   and all traced fields are being updated. After relocation, every old reference is either updated
   or rejected by the verifier; no forwarding pointer is left as a user-visible value.
9. **External storage is separate.** Executable mappings, host resources, shared data blocks,
   WebAssembly memories, and large/pinned allocations have independent identity and lifetime
   protocols. Their bytes are not added to movable managed payloads or silently folded into RSS.
10. **Semantics do not depend on collection.** Collection frequency, nursery size, promotion, and
    memory pressure may change throughput and pause time, but never the result of an ECMAScript
    operation. Host resource exhaustion remains distinct from a language `false`, `undefined`,
    ordinary no-match, or user interruption.

## Engine-owned records

The first implementation uses a 32-bit payload as selected in `HEAP_POINTER_MODEL.md`. Cage mode
resolves `base + offset`; handle mode resolves an Agent-local table entry. The encoded word does
not identify a Rust allocation.

```text
HeapRef { offset_or_index: u32 }
ObjectHeader {
    size_units: u32,
    layout: LayoutId,
    generation: Young | Old | Large | Pinned,
    mark: White | Grey | Black,
    forwarding: None | HeapRef,
}
```

The exact Rust representation is intentionally left open until the first object family is migrated.
The header must be fixed-width and independently verifiable; it must not borrow `Rc`, `RefCell`, or
standard-library layout details. Header size is included in allocator diagnostics, but the managed
payload contract continues to report requested payload and external bytes separately.

### Root classes

The root enumerator is an explicit Agent operation. Its implementation and stress verifier must
cover every class in this table; adding a root class requires a code and test change.

| Root class | Strong edge policy | Safepoint owner |
| --- | --- | --- |
| Global/Realm state | Trace global objects, intrinsics, environments, prototype links, and active module records | Agent/Realm |
| Interpreter and VM frames | Trace only slots named by the active frame map; immediates are ignored | Execution owner |
| Native/JIT frames | Trace tagged registers/spills and rooted handles described by the code metadata | Execution owner |
| Jobs and promises | Trace queued jobs, reactions, capability records, and forwarding state | Agent job queue |
| Coroutines/generators/async work | Trace suspended frames, environments, requests, and result values | Agent side tables |
| ShadowRealms | Collect child heaps at the same diagnostic safepoint; share only explicitly shared identities | Parent Agent |
| Modules/imports | Trace linked module records, namespace bindings, and pending dynamic-import records | Realm/module tables |
| WebAssembly/host | Trace JS wrappers and explicit host roots; count backing through external identities | Embedder hook |
| Weak structures | Trace tables, held values, callbacks, and cleanup jobs; do not trace weak targets | Weak/ephemeron tables |
| Handles | Strong handles are roots until released; borrowed `NoGc` handles cannot cross a safepoint | Builtin/embedder |

The existing exhaustive `Interp` inventory and post-collection visitor remain the migration oracle.
Every field is classified as traced, external, or non-owning before a new heap path is enabled.

## Allocation and safepoint protocol

All new allocations go through an Agent-owned allocator:

1. Reserve the requested payload and header in the selected space. A failed reservation reports
   ordinary host/resource exhaustion to the caller; it does not manufacture a JavaScript value.
2. Publish a provisional handle in the Agent allocation table. The object is not visible to author
   code until its fields have been initialized with valid immediates, handles, or external IDs.
3. Keep the destination rooted through initialization. A constructor, accessor, proxy trap, or host
   callback may allocate and collect, so no local raw pointer may survive that boundary.
4. Commit the object after its descriptor/layout verifier succeeds. If initialization throws, remove
   the provisional entry and release external resources without exposing a half-built object.

Before any operation that may allocate, the current tier must:

- spill live tagged values and register roots according to its safepoint map;
- publish the active bytecode PC, handler state, operand depth, environment links, and completion
  state needed for exact resumption;
- enter a `NoGc` scope only for a short, audited raw-pointer region that cannot allocate, call host
  code, poll interrupts, or take a back edge; and
- leave the scope before invoking any operation that can run JavaScript or suspend.

The tree walker remains the semantic oracle. Bytecode, baseline JIT, and later optimizing code must
be able to reconstruct the same frame state at every declared safepoint.

## Tracing, weak processing, and relocation

### Old generation

The first collector is deterministic stop-the-world mark/sweep over the central heap. Marking starts
from the root classes, follows strong descriptors, and reaches a fixed point for ephemerons:

1. mark strong edges normally;
2. when a marked WeakMap/WeakSet key becomes live, mark its associated values;
3. repeat until no new key/value pair is marked; and
4. clear unmarked weak targets and schedule FinalizationRegistry cleanup according to the existing
   host/job plumbing, without running cleanup JavaScript inside the collector.

Sweep returns unreachable movable objects to size-class free lists. Large, pinned, executable, and
external spaces use their own release paths. The deterministic collector and future concurrent
collector must produce equivalent weak-state and finalization behavior.

### Copying nursery

The nursery is enabled only after old-generation marking and promotion are correct. A collection
copies reachable eligible young objects to a promotion destination, installs forwarding entries,
updates all roots and traced fields, processes remembered old-to-young edges, and then releases the
from-space. Objects with unsupported layout, host pinning, large payloads, or external backing are
promoted or kept in a non-moving space.

An old-to-young write barrier records the source handle in a remembered set whenever a store writes
a young reference. Stress builds recompute the remembered set by scanning all old fields and fail if
the incremental set differs. A missing barrier is a correctness failure, not a performance hint.

## Migration without untraced hybrids

Migration proceeds by complete object families, not by changing the meaning of one pointer field at
a time:

1. **Nucleus (current).** `TaggedValue` and `HeapRef` validate words but do not encode native
   addresses or participate in execution.
2. **Rooted handle bridge.** Add Agent-owned handles and root maps while the current `Value`/`Rc`
   graph remains authoritative. Bridge cells are explicitly scanned and keep old objects alive.
3. **One leaf family.** Migrate a small, non-exotic allocation family with a complete descriptor,
   verifier, trace function, relocation test, and interpreter/bytecode differential coverage.
4. **References and side tables.** Migrate fields that point to that family, then its caches,
   environments, weak tables, and host handles. No side table may retain a native pointer to an
   moved object.
5. **Execution slice.** Migrate one bytecode/frame/helper slice to tagged values only after its
   root map and exact completion/deoptimization state are proven. Generic helpers remain a checked
   fallback during the transition.
6. **Nursery.** Enable copying for the migrated eligible family, then expand family-by-family after
   forced relocation and allocation-at-every-safepoint runs.
7. **Retirement.** Remove the old weak registry and cycle-breaking collector only when the final
   `Rc` family, root, weak edge, and host handle has migrated and the compatibility bridge is empty.

The bridge is never allowed to infer reachability from `Rc::strong_count`, object population, RSS,
or a raw address. A stale bridge entry is a verifier failure and is removed only at an Agent
safepoint after its owner has been proven unreachable.

## Verification gates

No migration slice is accepted without all of these tests:

- collect at every allocation, runtime/host call, interrupt poll, and back edge;
- force nursery evacuation on every allocation and promotion at each configured age;
- randomize object movement and poison freed/recycled slots in stress builds;
- independently walk every descriptor and validate all tagged references before and after collection;
- poison and verify every root map, including suspended jobs, generators, ShadowRealms, modules,
  WeakMap/WeakSet ephemerons, FinalizationRegistry state, and host handles;
- compare tree-walker, bytecode, baseline JIT, and migrated execution for every migrated opcode,
  including abrupt completions and exact resume PCs;
- run proxy/accessor/reentrant-host tests that allocate, mutate prototypes, throw, suspend, and
  re-enter during each migrated operation;
- run the full relevant Test262/WPT slices and the deterministic browser replay suite; and
- compare managed requested/external bytes and GC pause/throughput diagnostics before and after,
  keeping executable code, shared backing, and host allocations as sibling totals.

The phase exit requires zero differential failures, zero stale-reference verifier failures, complete
root-family coverage, and a measured nursery hit rate and GC reduction against the locked baseline.
Performance targets in `OPTIMIZATION_ROADMAP.md` are decision criteria after these safety gates;
they cannot waive a correctness failure.
