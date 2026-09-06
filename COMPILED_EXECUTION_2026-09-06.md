# Compiled execution coverage checkpoint — 2026-09-06

This implements the first priority from [the performance review](PERFORMANCE_REVIEW_2026-09-06.md):
stop excluding common application functions from compiled execution. These are general compiler
and runtime changes, enabled automatically by the existing tier policy. They do not recognize
frameworks, benchmark names, or particular source strings.

This is an engine implementation checkpoint, not a release promotion or a claim of V8/browser
parity. The installed browser and executables were not replaced. Existing unrelated work was
preserved.

## What changed

1. **Proper tail transfers in compiled functions.** Strict ordinary functions containing a
   tail-position call can now compile. The compiler evaluates the callee, receiver, and arguments
   in the original order, checks callability, and stages owned values for the existing trampoline.
   The VM/native frame and its handlers are retired before the callee is entered. Tail positions
   propagate through parentheses, conditional arms, the final sequence expression, logical RHSs,
   and optional calls. Calls requiring catch/finally, iterator closing, or resource disposal retain
   their required continuation. A tail-calling body is not spliced into an unrelated caller frame.
2. **General compiled body entry.** If the lean compiler cannot model a function's activation,
   it can compile its body against the actual FunctionDeclarationInstantiation environment.
   Rest/destructuring/default parameters, mapped and unmapped arguments, direct eval, hoisted
   closures, and lexical bindings are instantiated once by the existing normative prologue; only
   body execution changes tier. Parameter-created closures and body vars retain distinct bindings
   where required. Entry shortcuts cannot mistake this prepared environment for a closure's
   definition environment.
3. **Lean named functions and lexical arrows.** A named expression now creates its immutable
   self-name environment once, at closure creation, instead of allocating one on every call.
   Ordinary arrows can read lexical `this` and `arguments` using guarded, live binding reads in
   compiled code. `this` is read at the expression, not eagerly at entry, preserving the derived
   constructor TDZ and untaken-branch behavior. Named functions and arrows do not need the more
   expensive general prologue just for these lexical references.
4. **Optional-super call completion.** Forced-tier Test262 exposed two pre-existing compiler
   defects, reproduced on the exact baseline in both bytecode and JIT modes. Optional calls now
   preserve a super property's Reference receiver, including computed properties and spreads.
   `super()?.property` uses the constructor continuation instead of treating `super` as an
   ordinary function-valued expression.

The first general-entry prototype made tiny named-function and lexical-arrow probes slower.
Those results were investigated before acceptance; the lean closure paths above address the
per-call setup cost directly. Initial measurements remain archived separately from final runs.

## Standards basis

The web-standards skill was used with direct local file searches because its catalog lookup was
not functional during this work. No requests were made to standards-bearing servers. Normative
source: `/big/web-standards/repositories/tc39/ecma262/spec.html`, checkout
`e28783d5fc9dc12b3de905961e2c71410b38a202`.

- [EvaluateCall](https://tc39.es/ecma262/multipage/ecmascript-language-expressions.html#sec-evaluatecall),
  local lines 19890–19960: Reference/argument order, callability, and tail preparation.
- [Tail position and PrepareForTailCall](https://tc39.es/ecma262/multipage/ecmascript-language-functions-and-classes.html#sec-tail-position-calls),
  local lines 26245–26638: strictness, syntactic tail positions, cleanup exclusions, and removal
  of the caller's execution context.
- [FunctionDeclarationInstantiation](https://tc39.es/ecma262/multipage/ordinary-and-exotic-objects-behaviours.html#sec-functiondeclarationinstantiation),
  local lines 14247–14435: parameter/default environments, arguments aliasing, hoisting, and TDZ.
- [InstantiateOrdinaryFunctionExpression](https://tc39.es/ecma262/multipage/ecmascript-language-functions-and-classes.html#sec-runtime-semantics-instantiateordinaryfunctionexpression),
  local lines 24322–24363: creation-time immutable self-name environment.
- [InstantiateArrowFunctionExpression](https://tc39.es/ecma262/multipage/ecmascript-language-functions-and-classes.html#sec-runtime-semantics-instantiatearrowfunctionexpression),
  local lines 24479–24502: lexical `this`, `arguments`, `super`, and `new.target`.
- [Optional-chain evaluation](https://tc39.es/ecma262/multipage/ecmascript-language-expressions.html#sec-optional-chaining-chain-evaluation),
  local lines 20099–20215: preserve the base Reference for EvaluateCall; skip arguments after a
  nullish optional callee.

## Verification and performance evidence

Evidence lives in `benchmark-results/optimization-20260906/`; the exact parent binary and its
source-hash inventory remain in `target/review-20260906/release/lumen` and
`benchmark-results/review-20260906/provenance.json`.

### Correctness

- `cargo test --offline --locked -j2 -p lumen -p lumen-web -p lumen-host --lib --quiet -- --test-threads=2`:
  897 Lumen tests, 36 host tests, and 61 web tests passed; one web test ignored. Combined package
  feature unification enables additional embedder tests. This includes 19 new engine tests for
  compiled entry and tail transfer, with native-entry checks where applicable.
- Test262 checkout `d86b2294eb0a17eaa281ff12c73c473ec864c72f`: **22,524 / 22,524 files passed in each
  of interpreter, bytecode, and JIT modes**, zero failures/skips. The compiled modes used threshold
  zero, not a warm-up policy that could leave these short tests interpreted. This is a selected
  language/call/Function/Reflect/Proxy slice, not the full Test262 suite. Exact targets, binary
  hashes and raw logs are in `test262-{interp,bytecode,jit}-final/`.
- The two optional-super failures on the earlier candidate also failed on the preserved parent
  in both compiled modes and both source strictness variants. `recheck.json` records that
  baseline comparison. Neither remains in the final conformance run.
- Tail tests exercise 20,000 mutual calls with bounded interpreter depth/function-frame count,
  prepared-entry recursion, direct/overridden eval, exceptions, optional receivers, multiple
  spreads, templates, bound/proxy callees, constructor return mapping, iterator closing, and
  resource/finally completion ordering. Closure tests also prove the creation-time self binding
  is collectible rather than a permanent cyclic root.

### Reproducibility

Canonical fat-LTO release build, Rust 1.98.0, AArch64, two build jobs, separate target directory:

```text
cargo build --offline --locked --release -j2 -p lumen --bin lumen \
  -p test262-runner --bin test262-runner --target-dir target/optimization-20260906
```

| Artifact | SHA-256 |
| --- | --- |
| Exact pre-change Lumen | `50c21743505e13b27c2010df26456a6554eb1515f5572889e8ef6aac122026ff` |
| Final candidate Lumen | `c4c3b3c8dcdb331a4def3718c0df2f53cb47c68949d3dad3538b967bdd1a524a` |
| Final Test262 runner | `b14f39d48b11c6029343bdf31425f332d3f38522fa81ef8a9e308da49d420967` |

The source inventory differs from the preserved review inventory only in `bytecode.rs`,
`interpreter.rs`, `lib.rs`, and the two new test modules. The baseline therefore includes the
user's pre-existing RegExp and other changes; this is not a comparison against an unknown older
release. Builds/tests/runs were bounded by systemd memory and runtime limits with swap/core dumps
disabled. No standards downloads, executable promotion, or accepted-production record changes
were performed.

### Clean performance comparisons

Seven interleaved, fresh-process rounds for the preserved parent, final candidate and Node
24.20.0, all pinned to CPU 5. All `LUMEN_`/`TRUST_` environment flags were cleared; diagnostics
were run separately, after timing. Workloads were not rewritten. Engine-specific correctness
results matched across every Vue/micro sample. Times below are measured workload medians, not
process startup times. Diagnostic probes are not substitutes for application benchmarks.

| Workload | Parent, ms | Candidate, ms | Parent / candidate |
| --- | ---: | ---: | ---: |
| Unchanged Vue reactivity, 10,000 updates | 1,584 | 1,197 | 1.32× |
| Strict tail-forwarding probe | 147 | 91 | 1.62× |
| Named-function-expression probe | 122 | 27 | 4.52× |
| Lexical-`this` arrow probe | 107 | 47 | 2.28× |
| Strict getter with tail call | 239 | 186 | 1.28× |
| Mapped `arguments` probe | 551 | 563 | 0.98× |

Vue's measured update time fell **24.4%**. It still took approximately **10.9×** Node's 110 ms
median on this probe. This is the unchanged Vue 3.2.47 reactivity module from the pinned
Speedometer 3.1 vendor bundle, not a complete rendered Speedometer iteration. Every run produced
the expected final observation (30,016), effect count (12,359), and bounded 16-element array.
The complete-process medians were 1,773→1,351 ms; all samples are retained in `vue-final.json`.
The paired-round bootstrap 95% interval for the workload speedup is 1.316–1.331× (10,000
resamples, ratio of medians). The corresponding intervals are 4.36–4.56× for the named-function
probe and 2.23–2.30× for the lexical-arrow probe. Seven rounds establish a local comparison,
not a hardware-independent performance guarantee.

The tiny mapped-arguments body is **2.2% slower**, not a win: it now executes a compiled body
after the same expensive arguments/parameter setup. That cost is below the existing provisional
3% material-regression threshold, but is recorded and should not be hidden in an aggregate.
The other existing diagnostic medians are effectively flat: sloppy forwarding 48→48 ms,
anonymous expression 28→28, captured counter 28→28, strict local-return getter 169→169,
numeric array 94→94, typed array 266→266, plain field 91→91, proxy field 491→491, allocation
76→76; strict local-return forwarding is 73→74 ms. V8-relative ratios are deliberately omitted
for microcases where Node finishes close to the clock's resolution.

The classic workloads were also run as seven sequential interleaved rounds using the unchanged
V8 v7 harness and suite sources. Higher scores are better:

| Workload | Parent score | Candidate score | Candidate / parent | 95% paired bootstrap interval |
| --- | ---: | ---: | ---: | ---: |
| DeltaBlue | 846 | 844 | 0.998× | 0.993–1.000× |
| EarleyBoyer | 821 | 818 | 0.996× | 0.976–1.001× |
| NavierStokes | 12,674 | 12,674 | 1.000× | 1.000–1.002× |

These are essentially flat, not additional optimization wins. In particular, the object-heavy
classic gaps remain roughly 63× and 56× behind the same-run Node scores. None of the measured
components crosses the existing 3% material-regression criterion. The distributions and method
are retained in `classic-final.json` and `summary.json`; the initial exploratory run that
overlapped a diagnostic is excluded.

The separate Vue diagnostic confirms that this is an execution-coverage change:

| Compilation observation | Parent | Candidate |
| --- | ---: | ---: |
| Bytecode attempts | 57 | 57 |
| Bytecode successes | 48 | 57 |
| Native successes | 46 | 54 |
| Generated native code bytes | 292,940 | 367,836 |

The previously reported hot interpreted bodies disappear from `LUMEN_AST_HOT` output. Three
compiled bodies still use bytecode rather than native code. The extra native code is a measured
cost of covering more functions, not an unreported memory win. Diagnostic runtimes and inclusive
nested helper timings are not used as performance results. Final records are in
`diagnostic-final.json`; the earlier baseline diagnostic remains in the review evidence folder.

## Boundaries and next architectural work

- General prepared-body constructor entry and compiled tail-transfer constructors remain outside
  the lean entry shortcuts. Their existing constructor paths preserve return-object mapping,
  derived `this` initialization, and completion semantics. Ordinary calls to those same functions
  can still compile. A constructor continuation must be modeled before removing these guards.
- A bytecode-compiled function is not necessarily native-compiled. Direct eval, cleanup and some
  other operations still use the heap VM; unsupported bodies and `with` closure contexts retain
  their safe interpreter path. Native execution was tested on AArch64, not on x86-64 hardware.
- The general entry still pays for its normative activation/parameter prologue. This closes a
  whole-body compilation gap; it is not yet a fully compiled parameter-initialization system.
- The production `Value`/object representation is still reference-counted. The experimental
  packed-frame and heap bridge switches are not a completed production migration and were not
  enabled. Making their roots, host references and cross-family ownership correct is necessary
  before switching real objects to the new heap.
- The next major engine checkpoint should put a complete production object/value family through
  traced fields and precise live-frame/host roots, with a measured allocation/property/call
  workload. General object access, allocation/ownership, and broader optimizing IR remain the
  scale of work required for V8 parity; more isolated arithmetic cases will not substitute for it.
- Full Speedometer, browser replay/live-site, WPT, and multi-architecture release gates remain
  necessary before executable promotion. This checkpoint does not claim those gates ran.
