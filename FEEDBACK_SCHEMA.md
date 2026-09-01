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

Authoritative specification: <https://tc39.es/ecma262/>.

## Stable identity

Each baseline-compiled function owns a `FeedbackLayout`. Eligible semantic bytecode operations
are visited in canonical baseline bytecode order and assigned dense `SiteId` values starting at
zero. The recorded bytecode PC is the canonical baseline PC; it is diagnostic metadata, not the
identity itself.

A second-stage or future optimizing compile reuses the baseline layout. It must not generate a
new numbering from inlined or transformed instructions. Feedback originating in an inlined
callee remains in that callee's logical vector rather than acquiring caller-local identities.

Source edits, parser/lowering changes, or a change to the meaning/numeric encoding of a schema
type require a schema-version change or a separately validated compatible-layout hash before a
serialized profile may be consumed. Version 1 profiles will initially be process-local only;
serialization and upgrade/drop policy are separate Phase 1 gates.

## Abstract slots

A site has a semantic operation plus one or more `(ObservationKind, ObservationRole)` slots.
Kinds describe what was learned; roles distinguish operands/results without baking a particular
instruction format into the profile:

- `ValueClass`: an abstract ECMAScript value class. Numeric refinements such as integer versus
  double are observations, not Rust `Value` discriminants.
- `ReceiverLayout` and `HolderLayout`: abstract layout identities for the receiver and the object
  that supplied the property. The active adapter may currently resolve them through shapes and
  prototype guards; future adapters resolve them through Maps and validity dependencies.
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
shapes into profile-local dense layout tokens, preserves absent-holder and creation outcomes, and
widens distinct ways to an explicit polymorphic state. Raw shape numbers remain in an internal
adapter table and are never written to observation words. Transformed/inlined chunks reuse the
baseline schema without guessing new bindings; the retained baseline chunk remains authoritative.

Adapter bindings (cache family/index, current shape tokens, raw pins) are runtime-only and must
never be serialized as observations. A future Map adapter replaces only the token interner and
adds validity dependencies; `SiteId`, slot kind/role, observation states, and diagnostic schema do
not change. Element and call adapters are intentionally separate following slices.

Every optimizing consumer must treat missing, unknown, mixed, or invalidated feedback as a reason
to use a guard plus exact semantic fallback. A guard failure resumes at the exact semantic point;
the tree-walker remains the differential oracle.
