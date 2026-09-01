# Tagged value ABI design record

Status: reviewed design checkpoint. The representation and execution-boundary rules below are the
implementation contract for the first migration slice; they do not change the current `Value`
execution path until the executable gates in this document and `HEAP_SAFETY_MODEL.md` pass.

This record describes the representation that the execution tiers may share after
the heap/root API is ready. It does not change the current `Value` enum or its
observable behavior. The semantic source of truth remains ECMA-262: language
values are defined in §6.1, completion records in §6.2.4, and the numeric
comparison distinctions are specified by `SameValue`/`SameValueZero` in §7.2.

## Goals and constraints

- Keep the execution representation exactly one 64-bit word on supported native
  targets (`TaggedValue` is `repr(transparent)` over `u64`).
- Preserve every observable distinction: `undefined`, `null`, Boolean values,
  Numbers including `-0`, infinities and NaN, strings, Symbols, BigInts, and
  objects. The internal `Empty` completion marker is not an ECMAScript value and
  must never escape an engine boundary.
- Make tags and payload interpretation engine-owned. Generated code must not
  depend on Rust enum, `Rc`, `Vec`, or `RefCell` layout.
- Keep the representation valid for moving collection and deoptimization. A
  tagged word containing a heap reference is a handle/offset, never an
  unregistered native pointer.
- Permit a compatibility period in which the public API and semantic tree
  walker continue to use `Value`; conversion shims must be explicit and audited.

## Proposed word format

The initial format reserves a quiet-NaN prefix and uses the remaining payload
bits for immediates or a checked heap reference. The exact prefix and payload
width are constants owned by the engine, not inferred from host ABI layouts.

```
  63                 48 47                              0
  +--------------------+--------------------------------+
  |   16-bit tag      |       48-bit payload            |
  +--------------------+--------------------------------+
```

Non-tagged IEEE-754 binary64 words represent Numbers directly. Tagged words use
the reserved quiet-NaN range; a canonical NaN word is used for all NaN inputs
that are stored as a Number. The current `PackedValue` constants are a staging
prototype of this scheme, not permission for generated code to assume a 48-bit
native address.

The initial tag set is:

| Tag | Payload | Meaning |
| --- | --- | --- |
| `Undefined` | zero | ECMAScript `undefined` |
| `Empty` | zero | internal statement completion marker only |
| `Null` | zero | ECMAScript `null` |
| `Boolean` | `0` or `1` | `false` or `true`; other payloads are invalid |
| `HeapRef` | checked reference | object, string, Symbol, or BigInt handle with a subtype bit/table |
| `Reserved` | — | rejected by constructors and verifier |

Using one heap-reference tag plus a reference-kind table avoids baking separate
raw pointer interpretations into every generated template. If profiling shows
that subtype tests are hot, a later version may split the tag while retaining
the same semantic constructors.

## Number rules

`TaggedValue::from_number` must:

1. Preserve the exact binary64 bits for all non-NaN Numbers, including `-0`,
   positive/negative infinity, and subnormals.
2. Map every NaN payload to one canonical quiet-NaN word. NaN payload bits are
   not observable through ECMAScript Number operations, while `-0` is observable
   through `Object.is`, division, and the specified `SameValue` rules, so `-0`
   must never be normalized to `+0`.
3. Reject a bit pattern that collides with the reserved tagged range unless it
   is the canonical NaN encoding.

All equality, ordering, and conversion helpers continue to implement the
ECMA-262 algorithms; raw-word equality is never a substitute for
`SameValue`, `SameValueZero`, or numeric comparison.

## Heap reference boundary

The pointer model is recorded separately in `HEAP_POINTER_MODEL.md`: desktop
Agents use a validated 32-bit cage offset, with a checked handle-table index as
the portable fallback. Before the central heap exists, no `TaggedValue` may
encode a native address. A 48-bit canonical-address payload is not an accepted
ABI option; it would reintroduce the relocation and platform-address hazards
that the cage/handle split avoids.

References are always resolved through an Agent/heap context. A stale, zero, or
wrong-kind reference is a verifier failure, never an implicit `undefined`.
External resources, executable code, and shared backing stores remain separate
handle families with their own ownership/accounting rules.

## Completion and frame invariants

- `Empty` is accepted only in internal completion slots. Public evaluation,
  native return, promise settlement, and host callbacks convert it to
  `undefined` according to the existing completion plumbing.
- Every interpreter/bytecode/JIT frame records the number and kind of tagged
  slots. Safepoint metadata identifies which slots may contain heap references;
  a Number or immediate tag is never traced as a pointer.
- Deoptimization materializes the exact bytecode locals, operand stack,
  environment links, handler state, completion state, and resume PC. A tagged
  value may be duplicated in multiple logical slots; each slot is described,
  while heap identity is not duplicated.
- Native helpers receive handles or copied immediates under one calling
  convention. Borrowed raw pointers are allowed only inside a documented
  `NoGc` region and cannot cross allocation, host-call, interrupt, or back-edge
  safepoints.

## Migration and verification gates

1. Implement constructors/accessors and a bit-level verifier alongside the
   existing `PackedValue` prototype. Add tests for every value kind, all Number
   edge cases, invalid tags, stale references, and `Empty` boundary conversion.
2. Define the pointer model and heap cage/offset limits before placing handles
   in generated code.
3. Add a tagged shadow representation for selected bytecode operands; compare
   interpreter, bytecode, and native results after every operation in a stress
   mode.
4. Migrate frames and native helper arguments only after forced safepoint,
   root-poisoning, and (when available) forced-relocation tests pass.
5. Remove layout probing from generated code only after the generated-code and
   differential suites cover every migrated opcode and deoptimization point.

No performance result can waive these correctness gates. A failed or ambiguous
representation check falls back to the existing `Value`/bytecode path rather
than guessing at a tag or pointer.

## Tier calling convention and frame record

The tiers share one semantic call boundary even while their machine-level prologues differ. A
compiled entry receives an `ExecutionContext` owned by the active Agent plus a contiguous argument
slice of tagged words. The context carries the current Realm/Agent, callee and `this` handles,
the caller's frame link, an exact bytecode resume point, and the pending completion/exception
state. The entry returns either a tagged normal value or a tagged abrupt completion routed through
the same boundary; it never exposes `Empty` as a public result.

Every activation has this logical record, independent of whether it is stored in a Rust vector, a
native stack segment, or a JIT spill area:

```text
Frame {
    function_identity: FunctionHandle,
    caller: FrameLink,
    resume_pc: BytecodePc,
    locals: [TaggedValue],
    operands: [TaggedValue],
    environments: [EnvironmentHandle],
    handlers: HandlerState,
    completion: CompletionState,
    safepoint: SafepointId,
}
```

`locals` and `operands` have stable logical indices; an optimizing tier may keep a value in a
register or coalesce duplicate logical values only when its frame map describes both locations.
The baseline bytecode PC remains the canonical identity for feedback and deoptimization. A tier
may use native PCs internally, but every native PC that can allocate, call out, poll, throw, or
exit has a checked mapping to one canonical bytecode PC and `SafepointId`.

The ABI has one ownership rule: an argument or result is either an immediate word, an Agent-owned
handle, or a copied scalar. A helper never receives an unregistered native pointer as a value. A
helper that can run JavaScript, allocate, suspend, or invoke the host returns through the context
boundary so the caller's frame map is installed before that operation begins.

## Exact safepoints and root maps

The first migration declares safepoints at allocation slow paths, runtime and host calls,
interrupt polls, unconditional loop back-edges, explicit collection requests, and every deopt or
exception exit. Straight-line arithmetic and direct branches remain safepoint-free until one of
those events is reached. A safepoint record is immutable after code publication:

```text
Safepoint {
    id: SafepointId,
    bytecode_pc: BytecodePc,
    tagged_registers: RegisterMask,
    tagged_frame_slots: SlotBitmap,
    handle_slots: SlotBitmap,
    environment_slots: SlotBitmap,
    live_handler_state: HandlerMap,
    deopt_id: Option<DeoptId>,
}
```

The root walker uses the record and the active `ExecutionContext` together. It visits global and
Realm roots, the current and suspended frame chains, environment handles, queued jobs/promises,
coroutine continuations, modules, ShadowRealms, WebAssembly/host handles, and weak tables using
the policies in `HEAP_SAFETY_MODEL.md`. A register or slot not named by the active map is dead;
the collector must not guess from its bit pattern. A poisoned-map stress build fills omitted
locations with an invalid word and verifies that collection never reads them.

Safepoint publication is ordered before the operation that may collect. The current frame's PC,
operand depth, handler state, completion state, and all live roots are visible as one record before
the Agent can be stopped. Relocation updates roots and descriptor fields while JavaScript and host
callbacks are excluded; the resumed tier resolves handles again rather than retaining a raw
address across the safepoint.

## Deoptimization and exact resumption

Deoptimization is a state reconstruction operation, not a retry of the optimized instruction. A
`DeoptRecord` identifies the canonical bytecode PC, the target baseline frame shape, and recipes
for every logical local, operand, environment link, handler, and completion slot:

```text
DeoptRecord {
    id: DeoptId,
    bytecode_pc: BytecodePc,
    caller_chain: [InlineFrame],
    values: [MaterializationRecipe],
    operand_depth: u16,
    handler_state: HandlerState,
    completion: CompletionRecipe,
}
```

`InlineFrame` entries identify the retained baseline function and its canonical call-site PC. The
outermost frame resumes at the exact semantic point required by the bytecode operation: before a
pending write, after a completed `GetValue`, or at the next statement as specified by the
operation's abrupt-completion path. A guard failure must not re-run a getter, proxy trap,
coercion, iterator step, allocation, or host callback that already completed.

Recipes are deliberately finite and auditable:

- `CopySlot` copies an existing tagged register or frame slot.
- `Constant` materializes an immediate tagged word (including canonical NaN and `Empty` only in
  internal completion slots).
- `BoxNumber` requests an Agent allocation and roots its source until initialization commits.
- `Handle` retains or resolves an existing rooted handle through the Agent table.
- `VirtualObject` allocates and initializes a descriptor-listed object in field order, or falls
  back to the generic path if any field can invoke user code.
- `Duplicate` names another recipe's result when two logical slots hold the same value; it does
  not duplicate heap identity or charge the object twice.

The verifier rejects a record with an unmapped live value, an invalid recipe dependency, a
non-canonical immediate, a native pointer payload, or a resume PC that is not a baseline boundary.
If verification fails, execution takes the existing tree-walker/bytecode fallback and reports a
diagnostic failure; it never guesses a frame state.

## Rooted handles and no-safepoint regions

Rust builtins and embedders use an Agent-scoped `Handle<T>` for managed values. A strong handle is
registered before a possible allocation and released explicitly or by a lexical guard after the
operation. A borrowed handle is valid only inside a `NoGc` region whose type-level/documented
contract forbids allocation, host calls, interrupt polls, suspension, and back-edges. The region
is short and auditable; it is not a way to suppress collection around arbitrary user code.

The migration bridge keeps old `Rc` values alive through an explicitly scanned compatibility cell.
It may not derive a root from `Rc::strong_count`, a native address, or a frame's untyped bytes. A
handle crossing an Agent boundary becomes a structured clone, shared backing identity, or explicit
host resource according to the relevant ECMAScript/WebAssembly rules.

## ABI acceptance gates

Before the first execution slice is enabled, the implementation must provide:

1. constructor/accessor and verifier tests for every immediate and invalid-word case;
2. frame-map tests that collect at every declared safepoint and poison every omitted root;
3. forced relocation tests that update registers, frame slots, environments, handles, and weak
   structures without stale references;
4. deoptimization tests for normal and abrupt completion, re-entrant accessors/proxies, calls,
   generators/async suspension, and exact resume PCs; and
5. differential results matching the tree-walker for each migrated opcode across the baseline VM
   and JIT paths.

Until all five gates pass, the canonical `Value` path remains the only execution path and the
tagged record is documentation plus isolated bit-level tests.
