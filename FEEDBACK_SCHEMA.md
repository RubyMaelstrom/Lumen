# Lumen feedback schema

Status: Phase 1 design record, schema version 1.

## Purpose

Lumen's existing inline caches are execution machinery. They contain current `Props` shape
numbers, entry offsets, raw callable identities, and JIT pointers. Those details are useful to the
baseline tier but are not a durable profile contract. The optimizing tiers need a separate,
per-function feedback vector whose site identities survive recompilation and whose observations
can migrate from today's shapes to future heap Maps.

This record defines that boundary. It is informed by the current ECMA-262 algorithms rather than
by the layout of Lumen's Rust enums:

- ECMA-262 §6.1 defines the ECMAScript language types: Undefined, Null, Boolean, String, Symbol,
  Number, BigInt, and Object.
- ECMA-262 §6.2.5 defines Reference Records. Property and call observations must retain the
  distinction between a value, a property Reference, and its `this` value.
- ECMA-262 §10.1.8.1, OrdinaryGet, walks the prototype chain and distinguishes absent, data, and
  accessor outcomes. A receiver layout alone is therefore not a complete property observation.
- ECMA-262 §13.3.6.2, EvaluateCall, derives `this`, evaluates arguments, checks Object/callability,
  performs the tail-call preparation when required, and then calls the target. Feedback may
  describe the completed path but must never reorder or replace those semantics.
- ECMA-262 §7.1.3, ToNumeric, preserves BigInt and otherwise performs ToNumber after ToPrimitive.
- ECMA-262 §13.15.3, ApplyStringOrNumericBinaryOperator, gives `+` its string-concatenation path,
  applies ToNumeric left-to-right for numeric operations, and rejects mixed Number/BigInt inputs.

Authoritative specification: <https://tc39.es/ecma262/>.

## Stable identity

Each baseline-compiled function owns a `FeedbackLayout`. Eligible semantic bytecode operations
are visited in canonical baseline bytecode order and assigned dense `SiteId` values starting at
zero. The recorded bytecode PC is the canonical baseline PC; it is diagnostic metadata, not the
identity itself.

A second-stage or future optimizing compile reuses the baseline layout. It must not generate a
new numbering from inlined or transformed instructions. Feedback originating in an inlined
callee remains in that callee's logical vector rather than acquiring caller-local identities.

Source edits and parser/lowering changes produce a different compatible-layout hash. A change to
the meaning or numeric encoding of a schema type requires a schema-version change. Neither check
substitutes for the other when a serialized profile is consumed.

## Abstract slots

A site has a semantic operation plus one or more `(ObservationKind, ObservationRole)` slots.
Kinds describe what was learned; roles distinguish operands/results without baking a particular
instruction format into the profile:

- `ValueClass`: an abstract ECMAScript value class. Version 1 assigns stable bits to Undefined,
  Null, Boolean, int32-safe Number, other binary64 Number, String, BigInt, Symbol, and Object.
  The int32 split excludes `-0`, non-finite values, fractions, and out-of-range integers; it is an
  optimizer refinement rather than a second ECMAScript numeric type or a Rust `Value` tag.
- `ReceiverLayout` and `HolderLayout`: abstract layout identities for the receiver and the object
  that supplied the property. The active adapter may currently resolve them through shapes and
  prototype guards; future adapters resolve them through Maps and validity dependencies.
- `PropertyAccess`: the named-property outcome and its operation-specific detail. Version 1 uses
  stable outcome classes for data, absence, creation, accessor, exotic, and rejected paths. A
  data result stores `field slot + 1` (zero is reserved for outcomes without a field), while the
  low flag nibble stores bounded prototype depth and the current adapter's array-key guard. These
  meanings describe the completed ECMAScript operation rather than an IC implementation detail.
- `ElementAccess`: indexed/named key class, bounds/hole result, and prototype fallback outcome.
- `CallTarget`: abstract call/construct target identity and callable category, never a raw address.
- `BranchCount` and `Allocation`: bounded counter/allocation summaries reserved for later Phase 1
  slices.

The schema uses fixed numeric encodings and compact descriptors. Observation payload words are
allocated lazily, so merely compiling a function does not allocate a detailed runtime profile.
Saturation and megamorphic/unknown states will be explicit; counters must never wrap into a more
specialized state.

## Adapter boundary

Version 1 binds canonical named-property sites to their existing four-way ICs at runtime. During
an opt-in diagnostic traversal, the current-shape adapter translates monomorphic receiver/holder
shapes into profile-local dense layout tokens, preserves ordinary data field location and
prototype depth plus absence and creation outcomes, and widens distinct ways to an explicit
polymorphic state. Raw shape numbers remain in an internal adapter table and are never written to
observation words. Transformed/inlined chunks reuse the baseline schema without guessing new
bindings; the retained baseline chunk remains authoritative.

The IC adapter intentionally does not infer accessor, proxy, typed-array, namespace, or other
exotic outcomes from an unfilled cache. Those outcomes must be recorded at the canonical runtime
helper endpoint after the normative operation has identified them, without repeating a lookup,
trap, getter, setter, or other observable action. Until that collector lands, their stable outcome
codes are reserved and the property-observation milestone remains incomplete.

Adapter bindings (cache family/index, current shape tokens, raw pins) are runtime-only and must
never be serialized as observations. A future Map adapter replaces only the token interner and
adds validity dependencies; `SiteId`, slot kind/role, observation states, and diagnostic schema do
not change. Element and call adapters are intentionally separate following slices.

The first arithmetic adapter covers the canonical bytecodes for binary `+`, `-`, `*`, `/`, `%`,
bitwise operations, shifts, and `**`, plus unary `+`, `-`, and `~`. It records original operand
classes before any observable coercion and records a result class only after successful completion.
Thus an Object-to-String `+` path describes Object/String inputs and a String result, while a
Number/BigInt TypeError still describes both inputs and leaves the result uninitialized. Category
bits widen from monomorphic through a four-class polymorphic set to generic; old samples never
overwrite or narrow newer evidence.

Update expressions use the same abstract operand/result roles across local-slot, captured/free
environment, named property, element, private-field, immutable-target, and `super` lowerings.
Following ECMA-262 §7.1.3 and §13.4.2-5, the operand is the raw result of GetValue before ToNumeric;
the result is the arithmetic `newValue`, recorded only after PutValue succeeds. Prefix/postfix
selection affects the expression value returned to bytecode but not this arithmetic observation.
An object that coerces to BigInt is therefore recorded as an Object operand with a BigInt result,
and a throwing coercion or setter never publishes a successful result.

Detailed arithmetic collection is opt-in through `LUMEN_FEEDBACK_PROFILE`. Disabled chunks retain
one predictable boolean check at each arithmetic helper and never allocate observation words.
Profile-enabled AArch64 chunks route numeric inline templates, update fusions, and register chains
through exact-PC observation paths so optimized executions are not omitted; the normal
configuration keeps those fast paths unchanged. A second-stage transformed/inlined chunk has no
exact transformed-PC to canonical-site map yet, so it is explicitly unbound and cannot write
coincidentally matching PCs; its retained baseline chunk remains the feedback authority.

Every optimizing consumer must treat missing, unknown, mixed, or invalidated feedback as a reason
to use a guard plus exact semantic fallback. A guard failure resumes at the exact semantic point;
the tree-walker remains the differential oracle.

## Serialized compatibility envelope

The version-1 binary envelope is deliberately smaller in scope than the later machine-readable
diagnostic format. It establishes the compatibility gate around observation words; it does not
claim that current shape-derived layout tokens are portable. Every integer is little-endian:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | Magic `LUMENFB\0` |
| 8 | 2 | Envelope format version (`1`) |
| 10 | 2 | Feedback schema version (`1`) |
| 12 | 1 | Scope (`1` = exact live vector only) |
| 13 | 3 | Reserved, required to be zero |
| 16 | 8 | Opaque process-session identity |
| 24 | 8 | Lazily assigned feedback-vector identity |
| 32 | 8 | Stable layout hash |
| 40 | 4 | Site count |
| 44 | 4 | Observation-slot count |
| 48 | 8 × slots | Observation words |

The layout hash covers the schema version and every specified numeric field of every site and slot.
It never hashes Rust enum discriminants, struct bytes, padding, addresses, runtime IC bindings, or
the adapter's raw shape table. Site and slot counts are checked independently rather than relying
on the hash.

Version 1 accepts no implicit upgrades. Unknown envelope versions, schema versions, scopes,
reserved bits, lengths, observation states, process sessions, vector identities, or layouts each
produce a distinct `ProfileDropReason`. A matching layout alone is insufficient: two vectors can
intern the same dense token to different current shapes. Consequently, version-1 observations may
only be merged back into the exact live vector that emitted them. A process restart, function
recompile, cloned layout, or incompatible engine build drops them. Portable ingestion remains
disabled until canonical Map identities and their validity dependencies can replace current
profile-local tokens.

Ingestion validates the complete envelope and every word before changing the vector. Valid words
are merged monotonically: uninitialized state may gain information, but an existing polymorphic or
generic state is never narrowed by an older snapshot. Conflicting specialized observations widen
to polymorphic. Profile dump/ingestion code must report a drop reason and continue with empty or
current feedback; it must never guess a conversion or make execution depend on profile presence.
