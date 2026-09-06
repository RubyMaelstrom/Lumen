# Callee-local call and constructor guards — 2026-09-06

This follow-on to [compiled execution](COMPILED_EXECUTION_2026-09-06.md) removes an Agent-wide
performance cliff: creating any Proxy previously prevented new fast-call and constructor cache
entries even for completely unrelated ordinary functions. Existing entries could still work,
making performance depend on whether a function warmed up before or after Proxy creation.

The change is enabled by default, with no workload recognition or experimental flags. It restores
roughly 2–3× throughput on the affected dispatch probes. The unchanged Vue reactivity workload
improves by 4.7%, not by 2–3×. This is a useful execution-path correction, not the production
heap/value migration, a general optimizing compiler, V8 parity, or browser release approval.

## Implementation and semantic boundary

- `call_jit_fast` checks the actual callee's `ic_plain` marker and immutable `Callable::User`
  identity instead of requiring the entire proxy table to be empty. Prepared function entry,
  lexical-arrow, `with`, class-constructor, and realm restrictions remain in force.
- Native call-cache filling similarly requires an ordinary `Callable::Native` callee. A callable
  Proxy contains a native **sentinel**, so matching only the Native variant would incorrectly
  bypass its `apply` trap or revocation. The callee-local marker is required.
- Constructor misses validate the actual callee before native construction or user-constructor
  cache fill. Existing identity/epoch/realm-protected hits remain valid; live `.prototype` reads,
  argument ownership, `new.target`, exceptions, and constructor return mapping are unchanged.
  Constructors closing over `with` also retain their full reference-resolution path.
- Call-cache hits continue to use the existing weak identity pins, which prevent address reuse
  from becoming a false hit. No new raw-pointer ownership or root representation was introduced.
- The intrinsic audit found that `Object.hasOwn` bypassed special-object behavior through a raw
  property-map read. Its general builtin also incorrectly rejected non-nullish primitives.
  Both now share the existing trap-aware own-property machinery, extended to handle typed-array
  indices, live/deferred namespaces, and host indices. Ordinary objects retain an object-local
  map fast path. `Object.prototype.hasOwnProperty` shares that dispatch but preserves its distinct
  coercion order. The old test expecting `Object.hasOwn(1, 'x')` to throw was corrected; new tests
  require primitive boxing and test nullish errors separately.

The remaining global `inline_ic_safe` guard for raw prototype-walking operations was **not**
removed. This patch does not make arbitrary prototype chains or exotic receivers ordinary.

### Local normative sources

The local web-standards skill supplied the specification routing and semantic checks. All reads
were offline from ECMA-262 checkout `e28783d5fc9dc12b3de905961e2c71410b38a202`, the recorded
editor's-draft snapshot, not a claim of current upstream verification. No standards-host requests
or library refreshes were made.

- [ECMAScript function [[Call]]](https://tc39.es/ecma262/multipage/ordinary-and-exotic-objects-behaviours.html#sec-ecmascript-function-objects-call-thisargument-argumentslist),
  [local source:13831](/big/web-standards/repositories/tc39/ecma262/spec.html:13831):
  callee realm, class-call rejection, this binding, evaluation, and context restoration.
- [Proxy [[Call]]](https://tc39.es/ecma262/multipage/ordinary-and-exotic-objects-behaviours.html#sec-proxy-object-internal-methods-and-internal-slots-call-thisargument-argumentslist)
  and [Proxy [[Construct]]](https://tc39.es/ecma262/multipage/ordinary-and-exotic-objects-behaviours.html#sec-proxy-object-internal-methods-and-internal-slots-construct-argumentslist-newtarget),
  [local source:16541](/big/web-standards/repositories/tc39/ecma262/spec.html:16541):
  revocation, trap lookup, receiver/argument forwarding, `new.target`, and object-result checks.
- [Object.hasOwn](https://tc39.es/ecma262/multipage/fundamental-objects.html#sec-object.hasown),
  [local source:31428](/big/web-standards/repositories/tc39/ecma262/spec.html:31428), and
  [HasOwnProperty](https://tc39.es/ecma262/multipage/abstract-operations.html#sec-hasownproperty),
  [local source:6428](/big/web-standards/repositories/tc39/ecma262/spec.html:6428):
  ToObject before ToPropertyKey, then the receiver's actual [[GetOwnProperty]].
- [Object.prototype.hasOwnProperty](https://tc39.es/ecma262/multipage/fundamental-objects.html#sec-object.prototype.hasownproperty),
  [local source:31550](/big/web-standards/repositories/tc39/ecma262/spec.html:31550):
  ToPropertyKey before ToObject; that order remains distinct from the static builtin.

## Verification

The combined library run passes **1,001 tests**, with one existing ignored test:

```sh
cargo test --offline --locked -j2 -p lumen -p lumen-web -p lumen-host --lib --quiet -- --test-threads=2
```

That is 904 engine tests, 36 host tests, and 61 web tests. The seven new tests in
[call_guard_tests.rs](crates/lumen/src/call_guard_tests.rs) include actual native-code and cache-fill
assertions while a Proxy is live, plus warmed trap/revocation, live prototype, native constructor,
`new.target`, `call`/`apply`, exotic own-property, coercion-order, and cross-realm cases. Semantic
tests run in all three tiers, check frame/tail cleanup, collect garbage, and execute again.

An expanded **22,993-file Test262 selection passes with zero failures and zero skips separately
in interpreter, bytecode, and JIT modes**. Compiled modes use threshold zero. The selection retains
the previous checkpoint's language and Function/Reflect coverage and adds the whole Proxy subtree,
Object.hasOwn, Object.prototype.hasOwnProperty, and Array push/pop. Test262 revision:
`d86b2294eb0a17eaa281ff12c73c473ec864c72f`.

This is not a fresh full Test262, WPT, real-site, or browser acceptance campaign. Native execution
was tested on this AArch64 host, not an x86-64 execution host. No installed binary or
`accepted-production.json` was changed.

## Matched performance results

Canonical fat-LTO, one-codegen-unit release builds; rustc 1.98.0; Node 24.20.0/V8 control.
Seven interleaved fresh-process rounds per engine on CPU 5, all `LUMEN_*`/`TRUST_*` variables
cleared. Builds, conformance, profiling, and timing did not overlap. Vue is the same unmodified
Speedometer-pinned Vue 3.2.47 reactivity code and identical harness as the preceding checkpoint,
not an official Speedometer or DOM result. Every sample has the expected observation/effect
count; diagnostic probes compare checksums across all three engines.

| Workload | Parent median, ms | Candidate median, ms | Speedup |
| --- | ---: | ---: | ---: |
| Vue reactive updates | 1,201 | 1,145 | 1.049× |
| Ordinary calls, unrelated Proxy present | 83 | 28 | 2.96× |
| Construction, unrelated Proxy present | 125 | 63 | 1.98× |
| Plain native calls, unrelated Proxy present | 64 | 30 | 2.13× |
| Proxy field-update probe | 492 | 465 | 1.058× |

For Vue, the paired-round bootstrap 95% speedup interval is **1.041–1.054×**. Full-process median
wall time falls from 1,357 to 1,296 ms. Node's update-loop median is 111 ms: Lumen remains **10.3×
slower** on this application-derived workload.

The unrelated-Proxy probes have no Proxy operation inside their timed loop. Their no-Proxy
controls are effectively unchanged: calls 28→28 ms, construction 63→62 ms, natives 30→30 ms.
The result identifies removal of a dispatch-eligibility cliff, not acceleration of arbitrary
Proxy trap bodies. Node completes some tiny probes near timer resolution; no precise Lumen/V8
ratio is inferred from those samples.

Classic original-harness component scores, higher is better:

| Component | Parent | Candidate | Speedup, paired 95% interval |
| --- | ---: | ---: | --- |
| DeltaBlue | 843 | 845 | 1.002×, 0.995–1.004 |
| EarleyBoyer | 813 | 824 | 1.014×, 0.983–1.029 |
| NavierStokes | 12,674 | 12,661 | 0.999×, 0.999–1.009 |

These are effectively flat, not evidence of solving the classic object-heavy performance gap.
No measured stable component meets the existing material-regression rule (>3% median loss
and an interval excluding parity). The initial named-function probe reads 27→28 ms with an
interval including parity. A separate 15-round recheck using two million rather than 200,000
operations gives **275→275 ms**; anonymous controls give 276→274 ms. The initial readings remain
archived rather than being replaced. Mapped arguments measure 563→559 ms in the original probe;
their earlier compiled-entry overhead has not been architecturally removed.

### Memory, compilation, and attribution

Three additional interleaved Vue runs measured child peak RSS with `getrusage`: median
34,824→34,832 KiB, essentially unchanged. These observations are not a long-lived retention gate.
Separate instrumented runs show the same post-GC object count (1,896). Reported post-GC managed
requested storage increases from 6,216,310 to 6,252,604 bytes (0.6%; both are lower bounds).

Inlining is active on the framework for the first time in these checkpoints: seven attempts,
one successful site, versus zero attempts previously. This adds a second-stage chunk: bytecode
successes 57→58, native successes 54→55, generated bytes 367,836→395,852 (+7.6%). The additional
code/metadata is an explicit cost of enabling the existing optimization, not free performance.
No hot tree-walker function body was reported in either checkpoint's diagnostics.

The refreshed sampled profile still puts `VarMap::get` at 7.8% exclusive sampled CPU, with
general function entry, compiled-entry setup, dispatch, property lookup, and value destruction
also prominent. Unknown exclusive samples are 9.0%; generated-code unwinding remains imperfect.
Do not sum inclusive native `apply`/`shift` costs or interpret instrumented cache-gate counters
as production time fractions.

Next work should tackle the remaining general call/activation and binding-lookup costs, including
safe cached lexical-arrow entry and stateful native closures, alongside the production shared
object/value layout and general optimizer described in the review. The mapped-arguments prologue,
three remaining VM chunks in this workload, and roughly 56–63× classic object-heavy V8 gaps are
still open. Removing another unrelated global flag is not a substitute for those architectural
changes.

## Evidence and exact artifacts

- Parent source is committed in `51fbed4`; documentation checkpoint `50aec51`.
- Parent binary: `target/optimization-20260906/release/lumen`, SHA-256
  `c4c3b3c8dcdb331a4def3718c0df2f53cb47c68949d3dad3538b967bdd1a524a`.
- Candidate: `target/guarded-calls-20260906/release/lumen`, SHA-256
  `7162f63c97b22a947ac8d6a638cee833297eebf60435a65bafd7dbd31a4b5e88`.
- Runner SHA-256: `8c554230ac6fb56a151cff7cf7ce46475b8587903af04d7133ba00f77f5673dc`.
- [Source/build manifest](benchmark-results/guarded-calls-20260906/provenance.json),
  [paired summary](benchmark-results/guarded-calls-20260906/summary.json),
  [longer probe recheck](benchmark-results/guarded-calls-20260906/long-micro-final.json),
  [RSS observations](benchmark-results/guarded-calls-20260906/memory-verified.json), and
  [sampled profile](benchmark-results/guarded-calls-20260906/vue-sampled-profile.txt).

Raw timing, diagnostics, per-tier Test262 summaries/logs, and local runners remain under the same
gitignored evidence directory. Every stress/build process tree had a cgroup memory limit,
zero swap allowance, a deadline, and disabled core dumps. The release build took 3m15s with a
2.8 GiB peak; no installed artifact was overwritten.

Two diagnostic-wrapper failures are retained but excluded from successful-run claims:
`memory-final.json` could not launch a missing `/usr/bin/time` (the replacement is
`memory-verified.json`); the profile wrapper initially tried to parse gprofng's banner as JSON.
The profiling child exited successfully with the expected checksum, its trailing result was
validated separately, and the parser was corrected. Neither failed wrapper supplied clean timing
samples or changed engine code.
