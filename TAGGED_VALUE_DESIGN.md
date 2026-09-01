# Tagged value ABI design record

Status: design proposal, not an implementation contract yet.

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

The pointer model is intentionally a separate decision (Phase 2, item 2).
Before that decision, no `TaggedValue` may encode a native address. The
recommended initial model is a 32-bit index/offset into an Agent-owned heap
arena, with a generation or allocation-cookie check in debug/stress builds.
This gives moving collection a single update point and leaves room for a
pointer-compression cage later. A 48-bit canonical-address payload is an
optimization option only after startup validation proves the platform address
contract and after relocation/root tests cover it.

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

