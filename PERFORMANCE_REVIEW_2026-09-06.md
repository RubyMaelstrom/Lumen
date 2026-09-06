# Lumen performance review — 6 September 2026

Target: V8 parity on representative real web applications, as the engine of a standards-compliant, general-purpose browser.

Scope: review and diagnostics, not an implementation or release approval. I reviewed the optimization roadmap, runtime/compiler/heap/frontend code, historical measurements, the recent regression diagnosis, and the relevant TRust integration. I built the current working tree into a separate release target and ran bounded, affinity-controlled diagnostics. Existing source edits and executables were preserved. I did not run a new full Speedometer, live-site, Test262, or WPT campaign.

## Overall assessment

The roadmap identifies most of the right long-term architecture. Its main weaknesses are the order of delivery, the distance between validated scaffolding and production execution, and the workloads used to decide whether progress matters.

Lumen currently has two substantially different performance problems:

1. Important modern JavaScript cannot enter the compiled tiers at all. A newly broadened, standards-preserving tail-call fallback makes this especially consequential for strict framework code. This is an immediate, demonstrable problem, not a hypothetical benefit of a future optimizer.
2. Code that does compile still pays substantial general-purpose execution costs: call setup, boxed values, environment lookup, property representation, allocation, reference counting, and destruction. The effective numeric specializations do not generalize across these costs.

The first problem needs a more complete baseline execution path. The second needs production integration of the value/object/heap architecture and a general optimizing compiler. Both matter; completing a nursery alone will not fix interpreted framework getters, and compiling those getters alone will not deliver parity on allocation-heavy applications.

My recommendation is to make **modern-code compilation coverage the next performance checkpoint**, while developing one complete production object/value/heap slice and the shared execution-state machinery needed by a general optimizer. Move selected DOM bindings earlier. Keep the existing JIT as the baseline tier. Deprioritize additional isolated arithmetic, string, and RegExp specializations unless a product profile gives them a compelling share of the remaining time.

This is not evidence that Rust is the wrong implementation language. It is evidence that much of the expensive execution still uses representations and paths that an optimizing browser engine must avoid in common cases.

## 1. What the measurements establish

### Fresh framework diagnostic

I used the unmodified Vue 3.2.47 reactivity implementation already pinned in the Speedometer 3.1 TodoMVC vendor bundle. A small harness loads its webpack module and exercises reactive object reads/writes, nested proxies, effect dependency tracking, and bounded array push/shift operations. It performs 1,000 warm-up updates and times 10,000 updates in each fresh process.

This is an application-derived engine diagnostic, **not an official Speedometer result**, DOM benchmark, or complete Vue application. All engines produced the same final value and 12,359 effect invocations. Runs were pinned to CPU 5 and interleaved; timing runs had all `LUMEN_*` and `TRUST_*` instrumentation variables cleared.

| Comparison | Median timed update loop | Interpretation |
| --- | ---: | --- |
| Current Lumen release build, 7 samples | 1,587 ms | Range 1,575–1,596 ms |
| Node 24.20.0 / V8, 7 samples | 112 ms | Range 102–116 ms; Lumen takes about 14.2× as long |
| Current Lumen in a separate interleaved executable A/B | 1,591 ms | 7 samples |
| Existing `target/release/lumen` in that A/B | 1,222 ms | Current build takes 30.2% longer |

The existing executable's exact source provenance is not established. The current tree contains multiple changes. The A/B demonstrates a binary-pair regression on this workload; it does **not** attribute the entire regression to one patch or establish that the older executable is safe to ship.

A separate three-sample tier comparison on the current build gave:

| Requested tier | Median timed loop |
| --- | ---: |
| Interpreter | 2,448 ms |
| Bytecode | 1,754 ms |
| JIT | 1,594 ms |

Thus native execution reduces elapsed time by only about 9% relative to bytecode on this particular workload. This does not mean the JIT is generally ineffective: both compiled configurations retain the same hot interpreter fallbacks.

Separate diagnostic runs explain those fallbacks:

| Compilation outcome | Existing executable | Current build |
| --- | ---: | ---: |
| Bytecode attempts | 57 | 57 |
| Bytecode successes | 55 | 48 |
| Bytecode failures | 2 | 9 |
| Native successes | 52 | 46 |
| Inlining attempts | 0 | 0 |

The current exclusions include Vue's proxy getter, effect runner, `toRaw` helper, rest-argument wrapper, and a destructured-parameter helper. The function diagnostic records approximately 136,000 getter calls and 109,000 `toRaw` calls. An infrequent unsupported branch can disqualify an otherwise very hot function: the effect runner's inactive branch returns a call, even when the measured runner is active.

The diagnostic recorded only 4.38 ms of collection, with no allocation-threshold collection, and approximately 2.41 ms of native compilation. Neither explains the timed-loop gap. Allocation and ownership overhead still occur outside collection.

Do not compare the instrumented elapsed times to the clean timing table: enabling the combined diagnostics substantially changes execution cost. These runs establish eligibility and activity, not production time fractions.

Evidence: [Vue timing](./benchmark-results/review-20260906/vue-timing.json), [executable A/B](./benchmark-results/review-20260906/vue-ab.json), [tier comparison](./benchmark-results/review-20260906/vue-tiers.json), [current diagnostic](./benchmark-results/review-20260906/vue-diagnostic.json), [existing-executable diagnostic](./benchmark-results/review-20260906/vue-diagnostic-existing.json), and [harness](./benchmark-results/review-20260906/vue-probe.js).

### Small probes identify structural cliffs

Five interleaved samples per engine, 200,000 operations per sample, gave these current-Lumen medians:

| Diagnostic pair | Lumen medians | What it helps locate |
| --- | ---: | --- |
| Strict `return leaf(x)` / strict call then return local | 148 / 73 ms | Whole-function tail-call exclusion |
| Named / anonymous function expression with the same arithmetic body | 123 / 28 ms | Named-expression exclusion |
| Strict getter returning a call / returning its saved result | 239 / 169 ms | The exclusion also affects accessors |
| Int32Array / ordinary numeric array update | 268 / 94 ms | Typed-view access deserves its own fast-path investigation |

These are deliberately small probes, not predicted application speedups. Their input recurrence can settle into simple patterns; Node completes some cases near timer resolution. I do not use them to claim precise Lumen/V8 ratios. Nor should application authors be asked to rewrite strict tail calls into non-tail calls: that changes the resource behavior the engine is obliged to implement.

Evidence: [probe source](./benchmark-results/review-20260906/probes.js) and [raw results](./benchmark-results/review-20260906/timing.json).

### Historical scores show a highly uneven gap

The latest completed historical matrix I examined, `engine-matrix-20260904-terminal-many-final.json`, reports these median scores:

| Workload | Lumen | Node | Directional Node/Lumen score ratio |
| --- | ---: | ---: | ---: |
| DeltaBlue | 822 | 53,761 | 65.4× |
| EarleyBoyer | 866 | 43,485 | 50.2× |
| Raytrace | 1,853 | 43,363 | 23.4× |
| Richards | 1,042 | 17,263 | 16.6× |
| RegExp | 487 | 6,917 | 14.2× |
| Crypto | 7,201 | 27,373 | 3.8× |
| NavierStokes | 12,686 | 20,062 | 1.6× |

Splay's Node distribution is too unstable in that report for a useful simple ratio. These are historical, instrumented measurements, not a fresh matrix of my build.

The September 2 and September 4 composite medians rose from 1,710.9 to 1,909.5 for Lumen, but Node rose from 18,292.5 to 20,536.8. Lumen's normalized ratio is essentially flat, around 9.3–9.4%. This does not prove that every individual optimization was ineffective; it shows why an approximately 12% absolute score increase is insufficient evidence of relative progress across those runs.

The user's 50× concern is supported on some object-heavy components. The evidence does not establish one universal 50× factor for all web applications. The fresh Vue diagnostic is approximately 14×; the numeric workload is much closer. Browser latency must be decomposed before assigning its entire gap to JavaScript.

Evidence: [September 4 matrix](./benchmark-results/engine-matrix-20260904-terminal-many-final.json), [September 2 baseline](./benchmark-results/engine-matrix-20260902-current-baseline-node-lumen.json).

## 2. First priority: remove common-language compilation exclusions correctly

At [bytecode.rs:6612](/big/Code/Lumen/crates/lumen/src/bytecode.rs:6612), an ordinary strict function containing a recognized tail-call return causes compilation to bail. The current uncommitted change expands this from tagged-template tail calls to ordinary calls and associated expression forms. At [interpreter.rs:9948](/big/Code/Lumen/crates/lumen/src/interpreter.rs:9948), a failed compilation is retained in the function's once-initialized code slot. Subsequent hot calls continue through the tree walker.

That combination makes a common strict wrapper expensive for its entire lifetime. Modules and class bodies make strict-code coverage particularly important for the intended product. TRust's platform prelude is itself strict.

The fallback is protecting a real requirement. ECMA-262 requires tail-position calls to release or reuse the current context's transient resources. Removing the guard and emitting an ordinary recursive native call would restore speed by reintroducing incorrect resource behavior. The recent native-stack runaway makes that especially unacceptable. [ECMA-262 tail calls](https://tc39.es/ecma262/multipage/ecmascript-language-functions-and-classes.html#sec-preparefortailcall).

I would implement this in two steps:

1. Give bytecode an explicit tail-transfer operation and route it through a trampoline that retires the current execution frame before dispatching the target. Evaluate the callee, receiver, and arguments in the required order first; keep their values rooted during transfer. The body can then remain compiled without requiring an immediately perfect native tail-call implementation.
2. Lower eligible native tail transfers to frame reuse or an exit into that same dispatcher. Validate cross-tier, cross-realm, native, proxy, and accessor calls, including exceptions and resource exhaustion.

Use the specification's actual tail-position analysis, including surrounding control context. A syntax-only return-expression check can be more conservative than necessary. Pending cleanup, iterator closing, direct eval, optional calls, and handler state need explicit treatment; a jump that loses these effects is not a solution.

Next, broaden ordinary function entry. [The compiler's entry restrictions](/big/Code/Lumen/crates/lumen/src/bytecode.rs:4215) and [parameter lowering](/big/Code/Lumen/crates/lumen/src/bytecode.rs:4365) exclude important combinations of rest/destructured/default parameters, named function expressions, lexical `this`, `arguments`, and `new.target`.

A practical bridge already exists conceptually in coroutine compilation: those functions enter with the semantically instantiated environment and seed bytecode from it. Reuse a general, correct function-entry path for ordinary functions that cannot use the lean prologue. Compile their bodies first; specialize away unnecessary activation work later. This may retain some setup overhead, but it removes repeated AST execution of the entire body.

Do not conflate mapped sloppy `arguments` with unmapped strict arguments, or eagerly allocate an arguments object for every call. Preserve parameter environments, default-initializer TDZ, self-name binding, captured cells, and lazy materialization where observable behavior permits it.

The analogous native-tier issue appears at [jit.rs:2438](/big/Code/Lumen/crates/lumen/src/jit.rs:2438): suspension and completion-related opcodes exclude a whole chunk. Heap-owned coroutine continuations are already implemented; replacing old coroutine OS threads is not outstanding work. What is missing is broader native execution around the existing correct continuation model.

Longer term, a rare unsupported operation should enter an exact-state slow path, not force every surrounding loop and property access into a lower tier. That requires a real continuation/exception-state contract; arbitrary mid-statement jumps into the tree walker are not safe.

Proposed checkpoint: compile the pinned framework's currently excluded hot functions, prove constant-space proper tail calls and parameter semantics, and measure clean framework timing. Extend this to several independent framework bundles before treating Vue as representative. Count fallback time weighted by execution, not just the percentage of functions compiled.

## 3. The main architectural investment: production values, objects, and allocation

The roadmap is candid that much of Phases 2–3 is scaffolding. The important distinction is that passing its fixture tests does not yet alter the hot runtime:

- [Value](/big/Code/Lumen/crates/lumen/src/value.rs:75) remains a 16-byte execution enum with reference-counted payloads. Eight-byte packed property storage exists, but conversion and ownership work remains at its boundaries.
- [TaggedValue](/big/Code/Lumen/crates/lumen/src/tagged.rs:1) is an isolated engine-owned ABI nucleus. Immediate numeric migration exercises part of the contract; it is not general tagged heap execution.
- [CentralHeap](/big/Code/Lumen/crates/lumen/src/heap.rs:1) is a checked prototype, not the live collector. Its relocation fixtures should not be mistaken for production bump allocation.
- [Object allocation](/big/Code/Lumen/crates/lumen/src/value.rs:1047) still creates `Rc<RefCell<Object>>`, records a weak registry entry, and maintains ownership metadata. Destruction updates that registry and recursively releases owned storage.
- [The live collector](/big/Code/Lumen/crates/lumen/src/interpreter.rs:7062) snapshots objects/scopes and reconstructs graph relationships for cycle collection. The optional heap bridge records additional handles alongside the authoritative Rc graph; enabling it is not an allocation optimization.

The sampled EarleyBoyer profile supports working on this entire execution path. Exclusive sampled CPU includes `jit_new_inner` 9.2%, `run_moved_inner` 6.2%, `jit_call_hit` 4.4%, property-entry destruction 3.6%, object construction 3.4%, and Rc object destruction 3.3%. Allocator work appears in several additional entries. The collector's inclusive sampled share is about 6.9%.

This profile has 24.4% unattributed exclusive samples and imperfect unwinding through generated code. It cannot establish exact full-stack fractions. Nevertheless, it clearly does not support treating all allocation-related expense as time inside GC. [Sampled profile](./benchmark-results/review-20260906/earley-sampled-profile.txt).

The next heap milestone should execute a complete ordinary allocation/property/call workload through engine-owned values and traced fields. It needs real roots from live VM/native frames and host entry, a collectible old space, correct cross-family edges, and an explicit ownership boundary with unmigrated objects. Then add the nursery and barriers for that production family.

Reject a hybrid in which an Rc edge silently pins migrated objects or a handle is invisible to either collector. Mixed-mode safety is part of the implementation, not something the fixture collector proves automatically. Weak collections, pending jobs, suspended frames, realms, external backing stores, and embedder roots must follow the migration plan before they become eligible for movement.

The performance benefit sought is cheap allocation and graph mutation as well as less collection work. Nursery design is valuable when allocation is high and survival is low; it should be evaluated using allocated bytes, survival, promotion, mutator time, and pause distributions. [V8's GC architecture discussion](https://v8.dev/blog/trash-talk).

The current custom allocator is already addressing repeated general allocator calls. Further allocator replacement cannot remove Rc traffic, property payload destruction, registry management, or unnecessarily allocated activation state. Similarly, simply enabling `PACKED_LOCAL_SLOTS` would run into existing widening/repacking paths and disabled specializations, not complete the tagged ABI migration. [Disabled packed slots](/big/Code/Lumen/crates/lumen/src/jit.rs:748).

### Object layout belongs in this production slice

[Props](/big/Code/Lumen/crates/lumen/src/value.rs:2166) retains `Vec<(Rc<str>, Property)>` for named properties. Shape sharing identifies ordered key sequences but does not remove per-instance names and descriptors. A shared key-order ID is a useful cache guard; it is not yet the complete shared layout/type/prototype dependency model the future optimizer needs.

Use shared descriptors with compact instance fields for ordinary objects, plus dictionary transitions for irregular objects. Keep prototype and attribute semantics explicit. Weak layout identities and validity cells should support invalidation without permanently retaining every observed object or invalidating unrelated sites globally. Shared descriptors and in-object storage are established ways to avoid repeated per-instance metadata. [V8 fast properties](https://v8.dev/blog/fast-properties).

Arrays already have packed/dense storage and numeric fast paths. Do not restart from the false premise that every array access is a string-key hash lookup. The remaining design issue is a clean authoritative elements representation, transitions, holes, prototype lookup, and reduced duplicate/mirror maintenance. Typed arrays need direct, guarded view operations while preserving detachment, resizing, shared-buffer, conversion, and bounds semantics. The small typed-array diagnostic is a reason to profile that path, not permission to assume fixed buffers globally.

## 4. Build a general mid-tier, retaining the current baseline JIT

The current JIT performs real useful work: native control flow, property and call caches, constructor fast paths, direct compiled calls, and effective numeric register regions. A helper-oriented baseline compiler is a legitimate tier; V8's Sparkplug explicitly uses that architecture. [Sparkplug](https://v8.dev/blog/sparkplug).

The missing capability is general optimization across ordinary calls, memory operations, and control flow. Existing CFG/SSA-related code and selected region lowerings are useful foundations, but fixed opcode shapes and numeric chains are not a general register allocator, effect model, or deoptimizing compiler.

I would target a compact CFG-based SSA mid-tier with:

1. Common operations represented semantically, with explicit side effects and exceptional control flow.
2. Small persistent type/call/layout feedback and hotness from entries plus backedges.
3. Constant folding, dead-code removal, redundant-check elimination, and guarded load reuse.
4. General liveness, register allocation, and unboxed integer/double values where proven.
5. Bounded monomorphic/polymorphic inlining, including small accessor and builtin paths.
6. Precise safepoints and deoptimization records: bytecode PC, values, environments, handlers, pending completions, and any materialized objects.

This is the class of capability illustrated by Maglev: a comparatively fast optimizer with semantic IR, feedback, register allocation, and recoverable speculation. It is a better next target than a large peak-throughput tier built before ordinary application code can benefit. [Maglev](https://v8.dev/blog/maglev).

Do not require a finished copying nursery before work on SSA, register allocation, execution-state recovery, or call lowering can deliver anything. Use abstract heap/layout identities and conservative runtime operations initially, so early compiler work does not hard-code Rc addresses or today’s shape layout. Moving-GC integration and aggressive object specialization do have genuine rooting/layout dependencies; arithmetic/control optimization and correct baseline tail transfer do not all share those dependencies.

Calls merit a specific workstream. Direct JIT-to-JIT paths and pooled frames already exist. The goal is to make more real call sites use them and reduce the remaining frame/environment setup, argument copying, initialized-slot scans, and result ownership work. [Native frame entry](/big/Code/Lumen/crates/lumen/src/jit.rs:13654).

Vue's sampled profile contains substantial environment lookup, generic call dispatch, property-chain traversal, and value destruction; `VarMap::get` alone has 6.4% exclusive sampled CPU. Its zero recorded inlining attempts also deserves attention: a hot application can miss the existing trigger paths rather than merely produce bad inline plans. Profile all relevant entry paths, not only cached compiled callers. [Vue sampled profile](./benchmark-results/review-20260906/vue-sampled-profile.txt).

Code lifetime needs a corresponding policy. JavaScript and RegExp currently share a [process-wide 16 MiB executable budget](/big/Code/Lumen/crates/lumen/src/jit.rs:945), while [native compilation failures are cached](/big/Code/Lumen/crates/lumen/src/interpreter.rs:8361). A temporary budget failure should not necessarily make a later-hot function permanently uncompiled. Distinguish unsupported code, invalidation, temporary resource pressure, and retryable compilation; add aging/eviction and an explicit Agent/realm accounting policy. The Vue diagnostic used only about 293 KB of generated code, so budget saturation is a risk to test on large/long-lived applications, not the explanation for this measured result.

Maintain target-neutral lowering and equivalent state-recovery tests across AArch64 and x86-64. The current backends do not have equal specialization coverage. An AArch64 result cannot establish general desktop parity.

## 5. Move hot browser bindings ahead of full binding generation

TRust's current platform layer exposes avoidable work directly in the source:

- `nodeType` and `textContent` getters are strict JavaScript forwarding functions.
- `childNodes` obtains a native array of IDs, maps wrappers over it, and returns an array.
- `children` filters that result using additional getter calls.
- `firstElementChild`, `lastElementChild`, and `childElementCount` use `children`, potentially materializing the whole sibling collection to answer a single-node or count query.

See [platform getters](/big/Code/TRust/src/js_platform.js:2668) and [native child enumeration](/big/Code/TRust/src/lumen_backend.rs:6826). These paths combine JS accessor dispatch, compiler eligibility, host calls, wrapper lookup, and allocation. They also expose algorithmic work that no arithmetic optimizer will remove automatically.

Implement a few hot DOM member families through stable internal slots and a guarded native accessor/call ABI now. Direct native first/last-child and element-count queries can avoid intermediate lists. Proper native collection objects can avoid rebuilding snapshots on every read. This can start through the existing embedder interface; it does not require the complete generated Web IDL system to land first.

The standards constraint is productive here: `childNodes`/`children` are not interchangeable with arbitrary array snapshots. Live collection behavior and required identity must be preserved; static results such as `querySelectorAll` have a different contract. Indexed/named access and brand checks belong to platform-object semantics. [DOM collections](https://dom.spec.whatwg.org/#concept-collection), [Web IDL platform objects](https://webidl.spec.whatwg.org/#es-platform-objects).

Audit the platform prelude's own callers during this migration. Several currently use array-only methods or retain `childNodes` as a before-mutation snapshot. Replacing the getter with a live collection without updating those consumers would change internal cleanup and mutation-record behavior. Take explicit snapshots where an algorithm genuinely needs the old membership; do not preserve snapshot allocation on every ordinary public getter read.

Retain per-realm wrapper identity, receiver validation, prototype mutation handling, re-entrancy, custom-element reactions, mutation-observer ordering, and exception conversion. Replacing visible `__id` conventions with internal slots must not invent a new user-observable shortcut.

CSS style handling is another candidate: there is already a parsed-text cache, so it is inaccurate to say every read reparses. But mutations can serialize the property map and cross a Proxy wrapper. Profile style assignment, class changes, and attribute updates separately from layout; optimize the representation and crossing only where the trace supports it. [Style wrapper](/big/Code/TRust/src/js_platform.js:2970).

Keep browser subsystem attribution explicit. JS, binding overhead, DOM algorithms, selector matching, style/layout, paint, tasks, and network delay are different budgets. A full-browser score cannot be translated directly into a Node engine ratio.

WebAssembly also needs its own product lane. TRust currently embeds an interpreter backend, as its [Cargo manifest](/big/Code/TRust/Cargo.toml:131) documents. Native JS optimization will not accelerate Wasm instruction execution automatically. Measure Wasm compute and JS/Wasm crossings separately before selecting a compiled strategy. The recorded fresh-instance retention failures remain a correctness/lifetime gate, not a reason to drop wrapper identity or weaken tracing. [Recent regression diagnosis](./REGRESSION_DIAGNOSIS_2026-09-05.md).

## 6. Frontend, strings, and RegExp: valuable, with workload-specific priority

The frontend widens source into `Vec<char>` and eagerly tokenizes it. [Lexer source representation](/big/Code/Lumen/crates/lumen/src/lexer.rs:79). [Function source extraction](/big/Code/Lumen/crates/lumen/src/parser.rs:442) builds separate retained strings for function ranges. Nested functions can therefore retain repeated copies of source already represented by their enclosing functions.

Start with a shared immutable source buffer and byte-offset spans, preserving exact `Function.prototype.toString` text. Add a byte-oriented ASCII lexer path with correct Unicode handling, compact tokens/atoms, and measured AST retention. Lazy parsing/preparsing must preserve early errors and binding information, not just skip unseen function bodies. [V8 preparsing](https://v8.dev/blog/preparser).

The existing snapshot path serializes AST rather than reusable initialized native execution. TRust's process-local prelude snapshot helps, but it does not amount to a persistent bytecode cache or a fully initialized realm snapshot. Version cache entries by source and relevant engine/options; rebuild realm-sensitive intrinsic identities and caches. Never deserialize cross-realm ICs or raw native pointers as reusable code state.

Native Latin-1/UTF-16 string representations are worthwhile for a browser engine, particularly when code-unit operations repeatedly require representation conversion. Lumen already has a thin string and an improved append path; that work should be preserved. Evaluate ropes, slices, flattening, and retained backing sizes using application traces. A slice that pins a huge source may save copying while harming long-lived memory.

RegExp is genuinely behind in the historical suite, and a general native matcher can help. However, the recently accumulated specializations do not fix strict-code fallback, ordinary allocation, or DOM dispatch. Keep a bounded RegExp lane focused on measured candidate scanning, matcher dispatch, captures, and representation costs. Cold bytecode plus hot native execution is an established approach. [V8 RegExp tier-up](https://v8.dev/blog/regexp-tier-up).

Do not substitute a restricted linear-time matcher for arbitrary ECMAScript patterns; backreferences, lookarounds, captures, Unicode modes, and observable matching behavior still apply. Do not count early resource exhaustion as a faster successful match. The local adversarial corpus must report completed operations separately from interruptions/errors; treating every caught exception as a budget outcome or dividing truncated work by requested repetitions can manufacture apparent throughput. The current distinction between resource exhaustion and ordinary match failure should be preserved.

## 7. Repair the evidence pipeline without turning it into another long detour

The existing matrix is useful but insufficient for the current goal:

1. The examined classic-suite samples have no bytecode compilation failures, while Vue has hot failures. Add pinned modern framework and application-kernel coverage: strict wrappers, modules/classes, effects/proxies, getters, closure cells, rest/destructuring, collections, async continuations, typed views, and realistic exception paths. Keep original distributions and phase timings, not only a geometric mean. V8's retirement of Octane is relevant evidence about benchmark overfitting, not a reason to discard every old regression fixture. [Retiring Octane](https://v8.dev/blog/retiring-octane).
2. The [engine manifest](/big/Code/Lumen/benchmarks/engine-matrix.json:160) enables `LUMEN_PERF_METRICS` for Lumen but not an equivalent profile for Node. Current native-call timing and final diagnostic collection/accounting have nontrivial cost. Publish clean timing runs separately from instrumented attribution runs. Also record the actual allocator: the shell uses ClassAlloc and the browser defaults to mimalloc; the manifest's generic system-allocator label is stale.
3. `LUMEN_FEEDBACK_PROFILE` changes optimization behavior. [JIT feedback mode](/big/Code/Lumen/crates/lumen/src/jit.rs:2484) disables many native fast paths to observe operations through helpers. This mode is useful for semantic inventories, but its time distribution is not the production bottleneck distribution. It also cannot simply become the permanent feedback source for the future optimizer. Build cheap adaptive feedback separately from detailed diagnostics.
4. Function and native-call profiles have attribution limits. Fast call paths can bypass function-profile entry. Native timing nests across `apply`, array builtins, and JS callbacks: Vue's approximately 1.08 seconds in `apply` and 0.915 seconds in `shift` cannot be added as disjoint costs or interpreted as pure builtin implementation time. Use exclusive sampled attribution and publish unknown samples. Add generated-code symbol/PC mapping and usable unwind metadata.
5. The [regression checker](/big/Code/Lumen/scripts/check-engine-regression.py:95) compares current Lumen scores to a fixed historical floor; it is not a paired candidate-versus-parent test, does not normalize using the concurrent control, and permits inconclusive outcomes. Retain a long-term floor, but separately require matched-build interleaved A/B for accepting a change. An inconclusive result should request more evidence, not certify non-regression.
6. Every optimized-path test needs evidence that the path was exercised. Interpreter/bytecode/JIT agreement is weak evidence for an optimization if all three use the same fallback. Add tier-entry/side-exit assertions in diagnostic tests, forced guards/deopts/GC, and adversarial semantic perturbations. Passing a fixture root-map validator does not validate live root publication.

Use two product measurement layers: identical JS workloads in Lumen and V8 for engine attribution, and matching TRust revisions for browser integration. Compare Chromium and TRust on the same full-browser workloads for the product target, while acknowledging their different non-JS subsystems. Separate cold load, first interaction, warmed interactions, and long-lived navigation/retention. Virtual-time replays are useful deterministic work generators, but their simulated delays are not physical responsiveness measurements.

The previous severe native-stack incident also demonstrates why managed-byte accounting is not sufficient for process safety. Preserve hard process-tree memory/time bounds for stress work and measure native segments, executable pages, external storage, and queued work. Resource limits must never silently become ordinary JS results.

Keep the reproducible core of these measurements auditable and retain source/binary/workload hashes. Some current tools/results are local and ignored; do not reintroduce removed automation indiscriminately, but do not make a release conclusion depend on an unavailable `/tmp` script either.

## 8. How I would change the roadmap's execution order

The existing roadmap's standards rules, baseline-JIT preservation, simple-old-space-first policy, and incremental safety gates are sound. I would revise its dependency interpretation and immediate checkpoints:

| Priority | Deliverable | Proof of value |
| --- | --- | --- |
| Immediate | Correct compiled tail transfer and general ordinary-function entry | Hot framework functions leave AST fallback; semantic and constant-space tests pass; clean application-derived A/B improves |
| Immediate, ongoing | Clean timing plus tier/fallback and sampled attribution | A change's production path and benefit are visible; profiles do not silently disable that path |
| Early | Direct hot accessors/calls and selected native DOM member families | Fewer generic crossings and intermediate allocations; relevant browser/WPT behavior preserved |
| Main architectural track | One live tagged ordinary-object/field/frame/root slice, then nursery | A substantial measured fraction of real allocations/loads/stores migrates; no hidden whole-frame conversion or untraced hybrid edges |
| Overlapping compiler track | General CFG/SSA, register allocation, exact state recovery, then guarded specialization/inlining | Multiple independent object/control workloads benefit without opcode-pattern recognition |
| Independent cold-load track | Shared source spans, lazy frontend, reusable bytecode | Reduced cold/warm navigation parse time and retained source/AST memory |
| Profile-driven secondary work | Strings, RegExp, individual builtins, code-cache policy | Demonstrated contribution to selected product workloads, with bounded code/memory growth |
| Later | Advanced optimizer and sophisticated concurrent collection | Mid-tier coverage, live rooting, object layouts, and measured bottlenecks justify the complexity |

“Overlapping” describes engineering dependencies, not permission to introduce unsafe concurrent JavaScript execution within one Agent. It also does not require multiple people to work simultaneously: a single implementer can alternate bounded deliverables without making every benefit wait for all large phases to finish.

Do not add more abstract migration machinery without naming the next real allocation, call, or field access it will replace. Existing scaffolds are useful investments; the next acceptance criterion should be their exercised production coverage, not another count of isolated types or passing fixture tests.

I would not promise a 50× improvement from any one phase or multiply estimated phase gains together. If a portion of execution remains unchanged, Amdahl's law limits the total gain: eliminating a 10% cost entirely yields only about 1.11×; a 50× overall speedup requires the final total time to be 2% of the original. The costs here overlap and will move as tiers improve.

Finally, distinguish the research-engine objective from a browser shipping deadline. Keeping Lumen entirely independent is a legitimate choice, but V8 parity requires sustained engine development and cannot honestly be scheduled from these samples. If competitive browser performance becomes an overriding near-term constraint, embedding a mature engine alongside continued Lumen development is a product strategy worth evaluating separately. It brings substantial binding, lifetime, build, and distribution work; it is not a drop-in optimization or a recommendation to abandon this codebase.

## Reproduction and limitations

The [evidence directory](./benchmark-results/review-20260906/) contains the harnesses, all timing samples, diagnostics, textual sampled profiles, and [provenance inventory](./benchmark-results/review-20260906/provenance.json). It follows the repository's existing ignored benchmark-results convention; it is not automatically included in a commit. Raw gprofng experiments remain in `/tmp/lumen-review-20260906-yAVoMi` and are not required to read the archived textual summaries.

Current-tree executable SHA-256:

`50c21743505e13b27c2010df26456a6554eb1515f5572889e8ef6aac122026ff`

Existing release executable SHA-256:

`0d077a71b6f498c757ceec612e4096d633fe13b2942462457be14931438823df`

Pinned Vue vendor SHA-256:

`5a6341a5c1eef0dc8a8bcdd7e706d0892e16a67293cc201daa933452a98451e0`

The canonical release build used fat LTO and the repository's release profile, not a mixed no-LTO comparison:

```sh
systemd-run --user --wait --pipe --collect \
  -p MemoryMax=8G -p MemorySwapMax=0 -p OOMPolicy=kill \
  -p RuntimeMaxSec=900 -p LimitCORE=0 \
  --working-directory=/big/Code/Lumen \
  /usr/bin/cargo build --offline --locked --release -j 2 \
  -p lumen --bin lumen --target-dir target/review-20260906
```

For a repeat of the Vue diagnostic, copy the small evidence directory to a new result directory first, because the driver writes its mode-specific JSON there. Its absolute binary/fixture paths must point to the intended artifacts. Then run the copied driver under the same bounds:

```sh
systemd-run --user --wait --pipe --collect \
  -p MemoryMax=1G -p MemorySwapMax=0 -p OOMPolicy=kill \
  -p RuntimeMaxSec=240 -p LimitCORE=0 \
  --working-directory=/big/Code/Lumen \
  /usr/bin/node /ABSOLUTE/NEW/RESULT/DIRECTORY/vue-driver.mjs timing
```

Other modes are `ab`, `tiers`, `diagnostic`, and `diagnostic-existing`. Preserve original evidence; do not overwrite it with a different binary under the same label.

The review used a single AArch64 machine and CPU affinity, not controlled-frequency laboratory conditions. Sample counts are deliberately modest diagnostic distributions, not formal locked acceptance intervals. Sampling has unknown generated-code frames and does not precisely partition all execution tiers. No current full-browser ratio, cross-architecture result, new standards-conformance certification, or installed release approval is claimed.
