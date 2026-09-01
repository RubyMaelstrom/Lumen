# Heap reference model decision

Status: selected for the Phase 2 ABI; implementation begins with the central
heap work in Phase 3A.

## Decision

On native desktop targets, a `TaggedValue` heap reference is a 32-bit offset
into a per-Agent heap cage. The cage is a reserved, contiguous virtual-address
range no larger than 4 GiB; offset zero is invalid. The tagged word carries the
offset in its payload, while the Agent supplies the cage base when resolving a
reference. Generated code may use base-plus-offset arithmetic only after the
cage has been installed and validated for that Agent.

On targets that cannot reserve and validate a cage (including constrained
embedders and wasm), the same 32-bit payload is a checked handle-table index.
Handle entries carry a generation/cookie in debug and stress builds so stale
references fail verification instead of resolving to a recycled object. The
fallback is an explicit ABI mode, not a reinterpretation of native addresses.

This gives the common desktop path V8-like compressed-reference density while
keeping the representation portable and making relocation an owned heap
operation. It avoids the current `PackedValue` prototype's unsafe assumption
that an `Rc` address always fits the NaN-box payload.

## Allocation and relocation contract

- The cage reservation and alignment are established once per Agent before the
  first tagged heap allocation. Failure selects handle mode for that Agent;
  execution never silently truncates an address.
- Every managed object has an offset-addressable header containing size, type/
  layout identity, generation/mark state, and (during a move) forwarding data.
  Header validation is mandatory at debug/stress safepoints.
- A moving collector updates tagged roots and heap fields using generated root
  maps and object descriptors. No Rust pointer is embedded in a tagged word or
  retained in generated code across allocation, host/native calls, interrupt
  polls, or back-edge safepoints.
- Large objects, executable code, external backing stores, and shared data
  blocks remain separate allocation families. They are referenced by dedicated
  handles/identities and are not forced into the movable cage.
- Cross-realm objects within one ECMAScript Agent share that Agent's cage. A
  separate Agent never receives a raw tagged reference; host-visible transfers
  use structured cloning, shared-data identities, or explicit handles according
  to the relevant Web/ECMAScript API.

## ABI and generated-code consequences

`HeapRef` is an engine-owned newtype with checked constructors and accessors.
The native ABI passes the 64-bit tagged word by value; a helper resolves its
`HeapRef` against the active Agent. Optimized templates may inline the
base-plus-offset address calculation only for cage mode and must deopt/fall back
to the checked helper for handle mode.

The cage base is per Agent, never process-global. A generated frame therefore
records its Agent identity (or obtains it from the active execution context)
before dereferencing a reference. Snapshot blobs, feedback profiles, and cache
keys never persist offsets or handle indices; they store semantic data and
versioned shape/profile identities only.

## Verification gates

1. Unit-test cage reservation bounds, alignment, offset zero, maximum offset,
   overflow, and explicit fallback selection.
2. In a stress build, poison freed/recycled slots and validate every tagged
   reference before and after collection; stale or wrong-kind references must
   fail loudly.
3. Run forced-relocation and root-poisoning tests across interpreter, bytecode,
   baseline, and optimizing tiers, including suspended coroutines, jobs,
   ShadowRealms, WeakMap/WeakSet ephemerons, and host handles.
4. Keep a deterministic handle-mode test run even on cage-capable machines so
   the fallback remains behaviorally equivalent.
5. Benchmark both modes independently. Cage mode is an optimization, never a
   semantic precondition; a cage reservation or compression miss must not alter
   ECMAScript results.

