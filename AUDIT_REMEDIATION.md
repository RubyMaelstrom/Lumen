# Lumen audit remediation checklist

This checklist tracks the repository-wide audit begun on 2026-08-28. Work is
ordered by correctness and containment first, standards coverage second, and
measured performance improvements third. An item is complete only after focused
conformance-style tests and the broader affected suite pass.

## Correctness and containment

- [x] Implement normative WebAssembly binary/module/instruction validation,
  checked decoding, and bounded module/store allocations.
- [x] Move object and scope GC registries from thread-local state to explicit
  Agent/interpreter ownership across coroutine handoffs.
- [x] Replace recycled thread-local shape identifiers with collision-free,
  Agent-owned shape identities.
- [x] Implement the HTML timer initialization/repeat algorithm, including
  zero-delay intervals, nesting clamps, and no bulk missed-tick catch-up.
- [x] Split WebSocket read/write ownership and enforce RFC 6455 masking,
  framing, entropy, cancellation, and idle behavior.
- [x] Centralize Fetch header validation and RFC 9112 client/server message
  framing, including redirect header mutation and size limits.
- [x] Replace or harden the blocking-I/O pool with bounded queues, cancellation,
  panic containment, shutdown guarantees, and long-lived-resource isolation.
- [x] Sweep every pointer-keyed GC side table and implement sound WeakRef and
  FinalizationRegistry retention semantics.
  - [x] Count every `RealmState` intrinsic handle—including the realm-specific `%Array%`
    constructor—as an internal collector edge so unreachable secondary realms are reclaimable.
- [x] Give the global symbol registry explicit ECMAScript Agent lifetime and
  remove permanent retention of ordinary Symbols.
- [x] Report RegExp resource exhaustion separately from no-match and integrate
  engine interruption/deadline checks.
  - [x] Compile UnicodeSets string properties into compact forward/backward tries and execute
    repeated string atoms with bounded heap backtracking, preserving longest-first, singleton,
    empty-string, greedy/lazy, and lookbehind semantics without native-stack growth.
- [x] Prevent the native JIT exception unwinder from restoring handler stack depth past the
  current live operand depth, which resurrected moved values and double-freed thrown objects when
  abrupt `for…of` bodies performed IteratorClose.
- [x] Add streaming, byte-bounded decompression and bounded snapshot decoding.
  - [x] Enforce shared regenerated-byte limits and strict RFC/Compression Standard framing
    across DEFLATE, zlib, gzip, Brotli, and Zstandard.
  - [x] Bound snapshot input, allocation, integer conversion, and nesting before allocation.
  - [x] Give raw DEFLATE, zlib, and gzip CompressionStream/DecompressionStream real incremental
    native contexts, 32 KiB decode histories, chunked output, checksums, and GC-safe cleanup.
  - [x] Replace flush-only Brotli buffering with incremental native encoder/decoder contexts,
    advertised-window history retention, chunked output, and early compressed output.

## Web-platform conformance

- [x] Complete URL parsing/serialization and IDNA behavior against the URL
  Standard.
- [x] Implement stateful Encoding Standard decoders and decoder streams.
- [x] Port the Streams Standard controller, backpressure, BYOB, tee, and piping
  state machines with focused WPT coverage.
- [x] Implement DOM event dispatch state, redispatch guards, phases, and reset
  behavior.
- [x] Implement structured serialization transfer lists, detachment, graph
  identity, holes, views, and worker transfer behavior.
- [x] Expand ECMA-402/CLDR locale data and algorithms beyond the current
  deliberate subsets.
  - [x] Replace approximate `Intl.Segmenter` boundaries with Unicode 17 UAX #29
    grapheme, word, and sentence algorithms, generated property tables, and the
    official Unicode conformance corpora.
  - [x] Replace hand-written plural-rule subsets with generated CLDR cardinal,
    ordinal, and range rules and complete plural operands.
  - [x] Audit the remaining number/date/list/relative-time/display-name and
    collation data fallbacks against current ECMA-402 and CLDR.
    - [x] Generate all conjunction/disjunction/unit list patterns for advertised
      locales from CLDR 48 and implement full/context-sensitive pattern assembly.
    - [x] Replace RelativeTimeFormat's English/Polish-only phrases and unit forms.
    - [x] Replace DisplayNames' English-only subset.
    - [x] Expand number/date patterns and collation weights beyond current subsets.
      - [x] Generate CLDR number symbols, grouping, affix, currency, unit, and compact patterns.
      - [x] Generate CLDR date/time skeleton, style, connector, and range patterns.
      - [x] Implement Unicode Collation Algorithm weights plus CLDR locale tailorings.

## Efficiency and lifecycle

- [ ] Replace one-native-thread-per-live-coroutine with explicit VM
  continuations.
  - [x] Run compiler-supported sync and async `yield`/`await` bodies as
    heap-owned VM continuations with no native stack or channel handoff.
  - [x] Model synchronous, async, and async-from-sync `yield*` delegation
    completion records in the VM and remove delegation's stackful compatibility
    path, including iterator-close and abrupt-resumption conformance cases.
  - [x] Lower `for (var ... of ...)` identifier and destructuring heads into the hoisted
    VariableEnvironment homes instead of creating incorrect per-loop slots or falling back.
  - [x] Use compiler-supported heap continuations even when the ordinary execution tier is
    explicitly `interp`; tier selection no longer reintroduces native async workers.
  - [x] Seed coroutine VM frames from the already-instantiated formal parameter bindings so
    generator/async default initializers run exactly once, while preserving the original call
    list for `arguments` objects.
  - [x] Seed flattened parameter BoundNames for coroutine chunks, allowing already-instantiated
    rest and destructuring parameters (including captured/defaulted leaves) to use heap VM
    continuations without replaying binding semantics.
  - [x] Resolve coroutine `arguments` reads through the already-instantiated Function Environment
    Record, preserving object identity for nested arrows and supporting parameterless, strict,
    and otherwise-unmapped parameter lists.
  - [x] Reuse the already-instantiated Function Environment Record for nonempty sloppy simple
    parameter lists, so mapped `arguments` indices and parameter bindings share storage in both
    directions across suspension without replaying parameter initialization.
  - [x] Lower identifier and member logical assignments with one Reference evaluation, normative
    short-circuiting, NamedEvaluation, and hidden base/key homes that survive `yield`/`await`.
  - [x] Preserve normal, throw, source-return, and externally resumed-return completions across
    suspending `try`/`finally` regions for sync generators, ordinary async functions, and async
    generators; a finalizer's abrupt completion correctly replaces the saved completion, while
    AsyncGeneratorUnwrapYieldResumption is not awaited a second time after finalization.
  - [x] Carry break/continue Completion Records through nested suspending finalizers, retaining
    the destination's exact handler depth. Abandoned nested `for…of` iterators close inside-out
    between inner and outer finalizers, and close errors retain their normative catch/replace
    behavior.
  - [x] Make coroutine `for…of` handlers completion-aware so source returns and externally
    injected generator returns always perform IteratorClose. Preserve the distinct no-Await
    semantics of bare async-generator `return;`, and Await `return expression` before its Return
    Completion enters finalizers or iterator closing.
  - [x] Lower labelled non-loop statements as break-only control contexts, including stacked
    labels and labelled breaks crossing suspending finalizers, without stealing unlabelled
    break/continue from nested loops or switches.
  - [ ] Remove the bounded native fallback for generator/async bodies containing
    other constructs the bytecode compiler still cannot lower.
    - [x] Lower `for await…of` with async iterator acquisition, awaited stepping/closing, and
      completion-aware disposal. Native and async-from-sync paths retain their distinct Call/Await
      timing, live value rejections close in the adapter reaction, normal/throw Completion
      precedence survives awaited close, external async-generator returns dispose correctly, and
      nested abandoned iterators close inside-out without native worker threads.
    - [x] Lower `for…in` candidate enumeration with one standards-ordered key walk, per-step
      deletion checks, lexical/var/destructuring declaration heads, and suspending bodies.
    - [x] Lower member-expression assignment heads for `for…in`/`for…of`, evaluating their
      base/key Reference once per iteration after the step value is obtained; abrupt Reference or
      PutValue completion remains inside the loop's IteratorClose region.
    - [x] Lower the remaining destructuring-assignment loop heads.
      - [x] Run non-suspending array/object assignment heads in heap VM continuations through one
        shared normative AssignmentPattern operation, including local-slot projection, partial
        writes on later failure, nested IteratorClose, defaults, rest, computed keys, and members.
      - [x] Lower direct `yield`/`await` suspension inside assignment-pattern targets/defaults,
        preserving Reference-before-step/GetV ordering, nested iterator `[[Done]]`, normal/throw/
        injected-return IteratorClose, computed keys, array/object rest, partial writes, and
        immutable-target errors in heap-owned VM frames.
    - [x] Complete destructuring defaults/rest/computed keys and literal spread/method/accessor
      forms without changing observable evaluation order.
    - [x] Keep statically known immutable writes in heap continuations for simple, compound,
      logical, update, and destructuring forms while preserving RHS, ToNumeric, and TDZ error
      ordering.
    - [x] Lower catch-parameter BindingInitialization through the suspension-aware declaration
      pattern machinery, including defaults, rest, IteratorClose, and catch/finally interaction.
    - [x] Lower classic `for` declaration-head binding patterns for `var`, `let`, and `const`,
      retaining TDZ-before-initializer ordering and suspension-aware defaults, computed keys,
      rest, and IteratorClose.
    - [x] Split function-body lexical binding patterns between activation-homed captured leaves
      and fast uncaptured slots, preserving declaration-wide TDZ and suspension-aware
      BindingInitialization without forcing a native worker.
    - [x] Instantiate uncaptured strict block and switch function declarations at scope entry in
      heap continuations, preserving pre-declaration visibility, fresh identity on repeated block
      entry, and switch-wide initialization before case tests.
    - [ ] Lower classes (`super`, private names, fields/static blocks), dynamic import metadata,
      tagged templates, and other remaining expression forms.
      - [x] Lower tagged templates, `import.meta`, dynamic/import-source calls, private references,
        and `new.target` through suspension, preserving tag receiver/early-callability ordering,
        template-object identity, import option order, and private/super Reference state.
      - [x] Evaluate ordinary class definitions atomically and stage suspending heritage and
        computed ClassElementName evaluation in continuation-owned class/private environments.
        Preserve strict mode, class-name TDZ, superclass validation and prototype access,
        ToPropertyKey, private-name captures, inferred names, abrupt cleanup, and computed static
        `prototype` error ordering.
      - [ ] Model suspending proposal-decorator expressions and application with explicit
        continuation records. Decorated coroutine classes remain on the bounded native
        compatibility path.
      - [x] Lower arbitrary/interleaved spread argument lists for calls, optional calls, and
        constructors while preserving receiver binding, short-circuiting, and evaluation order.
      - [x] Lower delete references, including optional-chain short-circuiting, environment
        bindings, and the distinct computed/uncomputed `super` evaluation order.
      - [x] Preserve the actual method receiver as a Super Reference's `[[ThisValue]]` in
        compiled frames, including named/computed logical assignment and lean calls that do not
        materialize an activation environment.
      - [x] Read `new.target` from the retained Function Environment Record in generator and async
        VM continuations, including async arrows that inherit it after their defining constructor
        has returned, without opening the unsafe lean-frame case.
      - [x] Resolve named generator/async function-expression self-bindings through their retained
        immutable declarative environment, including recursive calls, body-var shadowing, and
        strict/sloppy assignment behavior. Restore the source function's strictness on every heap
        continuation slice rather than inheriting the resuming host job's mode.
      - [x] Resolve async-arrow `this` lexically through the retained defining environment and seed
        the heap continuation with that value, ignoring later call-site receivers and preserving it
        for nested arrows after suspension.
      - [x] Run atomic direct eval through the normative evaluator against a fully observable
        coroutine activation, homing function-scope parameters/vars/lexicals so sloppy eval-created
        bindings and closures persist while strict eval remains isolated. Preserve a free
        assignment's pre-RHS Environment Reference when eval creates a nearer `var`; keep
        dynamically visible per-block lexical environments, assignments spanning suspension, and
        destructuring defaults whose direct eval can change a previously resolved free-name target
        by retaining opaque Environment References directly in the heap continuation.
    - [ ] Move Source Text Module top-level-await evaluation from the bounded native compatibility
      path into continuation-owned module execution state.
    - [x] Replace the native `Array.fromAsync` iterator worker with an explicit heap continuation
      that preserves normative Await boundaries, async-from-sync wrapping, iterator closing, and
      promise rejection ordering. The focused Test262 `built-ins/Array/fromAsync` suite passes
      95/95.
    - [x] Model `using`/`await using`, `with`, and switch lexical environments in resumable frames.
      - [x] Keep synchronous and asynchronous DisposableResource stacks on each heap continuation
        and lower function-body/block, classic-`for`, and per-iteration `for…of` declarations
        through completion-aware disposal pads. Preserve registration-before-initialization,
        reverse disposal, exact `DisposeResources` await markers and sync fallbacks, suppressed-error
        precedence, abrupt generator resumption, disposal-before-iterator-close ordering, and
        captured once-per-call block resources without flattening their immutable semantics.
      - [x] Instantiate one shared switch lexical scope after discriminant evaluation and before
        case matching, preserving TDZ, fall-through, class/function declarations, and captured
        bindings across suspension. Allocate a fresh retained record whenever a containing loop
        re-enters the switch.
      - [x] Carry the active lexical-environment cursor in heap VM continuations and lower
        suspending `with` object/body evaluation, including nested environments, primitive
        ToObject wrappers, `Symbol.unscopables`, implicit call receivers, and restoration before
        normal, throw, return, break, or continue completions. Retain resolved Environment
        References across suspension so simple/compound/logical/destructuring writes cannot be
        rerouted by later object or unscopables mutations.
      - [x] Allocate resumable Declarative Environment Records only for closure-captured bindings
        in re-entered blocks and lexical classic-`for`, `for…in`, `for…of`, and `for await…of`
        heads. Preserve the separate uninitialized RHS environment, classic-for copy points,
        destructuring, `using`/`await using`, suspension, and restoration before disposal,
        IteratorClose, and outer finalizers; uncaptured bindings remain allocation-free slots.
      - [x] Give closure-captured catch parameters a fresh resumable declarative environment per
        CatchClauseEvaluation, distinct from the nested catch Block environment. Preserve
        destructuring/IteratorClose order, suspension, and restoration before outer finalizers.
      - [x] Promote every supported inner declaration scope when multiple captured bindings reuse
        the same spelling. Keep sibling, nested, re-entered block, loop-head, and catch records
        distinct while rejecting only captures that also resolve through an unsupported or
        function-wide declaration identity.
  - [x] Until the continuation conversion is complete, cap live native workers
    and make stack reservations bounded and configurable.
- [x] Share WebAssembly memory backing directly with JS and reclaim store
  modules/entities when their handles die.
  - [x] Identify `Memory.buffer` with the store's linear-memory Data Block and
    synchronously detach/refresh fixed buffers after API and instruction growth.
  - [x] Use weak JS address caches plus finalizer-owned graph roots so modules,
    callbacks, buffers, instances, and entity allocations are traced and reclaimed.
- [x] Add origin-keyed HTTP keep-alive and cache OpenSSL API/client/server
  contexts.
  - [x] Partition the agent-local bounded idle pool and TLS session scopes by
    effective origin and Fetch credentials, and retry stale transports only for
    idempotent methods.
  - [x] Reuse only completely consumed, self-delimited RFC 9112 responses and
    preserve each connection's read-ahead buffer.
  - [x] Load the OpenSSL API once, reuse client contexts, and retain one server
    context per listener instead of reparsing certificates for every accept.
- [x] Convert retained-string, RegExp, and related caches to byte-accounted LRU
  budgets.
  - [x] Bound UTF-16 string views and prepared RegExp subjects by retained source,
    materialized element/offset storage, metadata count, and true recency.
  - [x] Account compiled matcher programs (including nested lookarounds, character
    classes, names, and lookup tables) across all source/flag variants while
    preserving fresh ECMAScript RegExp object identity.
- [x] Remove duplicate parser source indexing/allocation.
  - [x] Share the lexer's single Unicode code-point index with script, module,
    and template-substitution parsers while preserving exact ECMA-262
    `[[SourceText]]` slices. Release benchmark after the change: 67.93 µs
    parse for the kitchen-sink fixture and 10.60 ms for the 500-function fixture.
- [x] Measure TRust workloads and move benchmark-specific JIT regions behind
  general IR passes; retire specializations without production value.
  - [x] Profile TRust's DOM boundary, mutation-observer, and serialization
    workloads with region telemetry. None selected the Richards scheduler
    family; disabling that family produced 12.230/1.808/0.602 s versus
    12.225/1.775/0.564 s event-loop samples (noise-level differences).
  - [x] Remove the Richards-only scheduler planners, graph epochs, native
    emitters, environment switches, cache-discovery helpers, and fixture-only
    regression matrix. This reduced `jit.rs` from 22,674 to 12,673 lines.
  - [x] Retain the production-relevant loop lowerings only after general
    CFG/SSA `RegionIr` validation. Post-removal TRust samples were
    12.265/1.839/0.578 s, and the Richards 100-run probe remained within the
    observed pre-change range (431 ms post-change; 417--458 ms pre-change).
- [x] Add guarded dense-array construction paths for intrinsic `Array.from`, TypedArray iterable
  initialization, and full-range fills of fresh holey arrays. Preserve Realm-specific constructors
  and fall back for holes, accessors, proxies, indexed prototype setters, patched iterator methods,
  or any other observable iteration. The three 10,000-element detached-buffer Test262 stress cases
  fell from watchdog-scale execution to 7.8 seconds together.

## Verification gates

- [x] Run the current full Test262 checkout and record its exact revision.
  - [x] Revision `d86b2294eb0a17eaa281ff12c73c473ec864c72f` (2026-08-25):
    53,574 passed, 0 failed, and 4 documented upstream skips.
- [ ] Add/run focused WPT subsets for every implemented web API.
- [ ] Run the WebAssembly core specification tests for the supported feature
  set and add malformed-binary/validation fuzzing.
- [ ] Run Autobahn WebSocket tests and adversarial RFC 9112 framing tests.
- [x] Resolve the HTTP/2 interoperability regressions.
  - [x] Queue all client frames created before transport connection so the wire order is the
    RFC 9113 client preface, SETTINGS, HEADERS, then DATA.
  - [x] Prevent servers from advertising the forbidden `SETTINGS_ENABLE_PUSH = 1` value and make
    the Node server-interoperability fixtures readiness-driven. Cleartext/TLS client, server,
    frame, and HPACK tests all pass.
- [x] Run formatting, Clippy, workspace tests, release Lumen, and release TRust
  before acceptance handoff.
  - [x] Lumen formatting and warnings-denied all-target Clippy are clean; the full workspace suite
    passes, including the runtime, HTTP/TLS, Node/Bun compatibility, WebAssembly, and doc-test
    targets (two explicitly documented tests remain ignored).
  - [x] The complete optimized Lumen workspace builds successfully.
  - [x] TRust formatting and warnings-denied all-target Clippy are clean; 944 library tests and 28
    desktop tests pass (16 explicitly documented manual/diagnostic tests remain ignored), and the
    optimized `trust` and `trust-desktop` binaries build successfully against this Lumen tree.
