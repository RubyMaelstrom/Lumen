# Shared ordered keys and compact instance fields — 2026-09-06

This round starts the production ordinary-object storage migration. Named properties now use
contiguous **16-byte `Property` fields**, replacing 32-byte `(Rc<str>, Property)` entries, with
ordered keys held in shared, copy-on-write layouts. This is enabled by default in the production
interpreter, bytecode VM and native JIT; no benchmark recognition or experimental switch is used.
The field-stride reduction is not a claim that total object size, heap use, or execution time halves.

Final outcome: dynamic/literal creation takes 12.9%/7.8% less time and a retained repeated-record
graph reports 26.2% fewer managed requested bytes. Broad execution is not faster: the classic
eight-component score geomean is 0.6% lower, Proxy reads take 3.2% longer and the Vue kernel 2.0%
longer. Keep this as a storage foundation, with those costs explicit; it is not V8 parity or
browser/Phase-4 acceptance.

## Production integration

- Compiled literal templates share their key layouts and move values into fresh field vectors.
  Small-map instance creation/destruction no longer clones/drops an owning key reference for
  every field. Larger maps still retain their existing per-instance lookup indexes and index
  keys; this slice does not claim to eliminate those sidecars.
- Constructor bytecode supplies predicted ordered keys alongside the existing capacity hint.
  Dynamic/forwarding constructors can learn small layouts through the existing weak-pinned
  constructor cache. Predictions reserve storage only: the live field count determines which
  keys exist. Divergent writes detach the layout before changing keys.
- Ordinary insertion shares small key layouts through the Agent's existing shape transition
  table, covering dynamic objects and JSON parsing across different creation sites. Eager cached
  layouts are capped at 4,096, with at most 16 keys each. Creation caches retrieve layouts by
  already-proved shape ID, without repeating a string-keyed transition lookup. The sparse
  shape-ID vector is separately capped at 65,536 entries (512 KiB on this host).
  Large/irregular maps retain the general
  copy-on-write path; arrays keep independent indexed storage and do not use named-only shape
  identities to infer element slot positions.
- Cold transition learning extends matching small insertion prefixes with the observed remaining
  keys. Subsequent dynamic objects reserve the learned field capacity once, and native stores
  can use the predicted keys without repeated layout-table lookups. This is bounded by the same
  16-key/cache limits; incompatible branches detach, and future fields remain unobservable.
- Guarded forwarding constructors retain layouts on the selected initializer chunk, not the
  shared wrapper body. After the existing live initializer/plan guards pass, a fresh empty
  receiver adopts those keys. This preserves the existing no-identity-hash hot path even when
  distinct constructors share a wrapper function template but use different initializers.
  Once a guarded immutable initializer plan has established its layout, it moves values into
  the field vector directly, without per-field key cloning or repeated runtime key comparisons.
- Native property reads, writes, numeric updates and element operations use the measured compact
  field stride. Array-holder key checks follow the shared key layout. Native creation uses an
  equal predicted next key or adopts the owning Agent's cached destination-shape layout before
  publishing a field; an uncached transition uses the checked append path. Adoption requires
  spare field capacity and an absent or shared old layout: last-owner destruction stays in Rust.
  The owning heap and vector offsets are measured and validated before native code is enabled.
  The creation guard accepts separately allocated equal short strings through a bounded inline
  content check after its pointer-identity fast path. Long non-identical strings require a cached
  destination layout or use the helper.
  Existing extensibility, prototype, epoch, accessor, writable, receiver and ownership guards
  remain active. The x86-64 numeric property path consumes the same measured field offsets.
- Shape-validated and already-resolved VM slot reads access the field vector directly, without
  loading shared keys that the caller would discard. Array-holder caches retain their required
  key check, and mutable slot access retains numeric-mirror invalidation. Both APIs are bounded
  by the live field count, not the length of a constructor prediction.
- GC traces the live field vector, including accessors; layout metadata contains no JS values,
  prototypes, or SymbolData ownership. Existing symbol-key retention stays with the property map.
  Memory diagnostics count each shared key buffer once, including buffers retained by shape
  transitions, constructor hints and compiled templates, rather than charging it to every object.

Descriptor flags and accessor payloads are deliberately **per instance** in this slice. Sharing
keys does not imply that two instances have equal attributes or prototypes. Redefinition, freezing,
sealing, deletion and prototype mutation continue to use their existing semantic paths. This is
not yet a full hidden-class descriptor/prototype dependency model, eight-byte execution values,
an Agent-owned production heap, a nursery, or a general optimizing tier.

## Local standards contract

The local web-standards skill directed offline consultation of ECMA-262 checkout
`e28783d5fc9dc12b3de905961e2c71410b38a202`, the recorded editor's-draft snapshot. No standards
servers were contacted and no claim of current-upstream verification is made.

- [OrdinaryGetOwnProperty](https://tc39.es/ecma262/multipage/ordinary-and-exotic-objects-behaviours.html#sec-ordinarygetownproperty),
  [local source:13212](/big/web-standards/repositories/tc39/ecma262/spec.html:13212):
  only created properties exist; constructor layout predictions must remain unobservable.
- [ValidateAndApplyPropertyDescriptor](https://tc39.es/ecma262/multipage/ordinary-and-exotic-objects-behaviours.html#sec-validateandapplypropertydescriptor),
  [local source:13286](/big/web-standards/repositories/tc39/ecma262/spec.html:13286):
  attributes, accessor/data conversion and non-configurability remain instance-local.
- [OrdinaryGet](https://tc39.es/ecma262/multipage/ordinary-and-exotic-objects-behaviours.html#sec-ordinaryget)
  and [OrdinarySetWithOwnDescriptor](https://tc39.es/ecma262/multipage/ordinary-and-exotic-objects-behaviours.html#sec-ordinarysetwithowndescriptor),
  [local source:13389](/big/web-standards/repositories/tc39/ecma262/spec.html:13389):
  prototype lookup, live descriptors and accessor receivers cannot be replaced by a key-layout match.
- [OrdinaryDelete](https://tc39.es/ecma262/multipage/ordinary-and-exotic-objects-behaviours.html#sec-ordinarydelete)
  and [OrdinaryOwnPropertyKeys](https://tc39.es/ecma262/multipage/ordinary-and-exotic-objects-behaviours.html#sec-ordinaryownpropertykeys),
  [local source:13501](/big/web-standards/repositories/tc39/ecma262/spec.html:13501):
  deleting/re-adding detaches and preserves chronological string/symbol order, with numeric
  indices sorted independently.

## Measurement discipline and initial rejected slice

The parent is `36e7e70`, measured through the exact previously verified release binary
`target/guarded-calls-20260906/release/lumen` (SHA-256
`7162f63c97b22a947ac8d6a638cee833297eebf60435a65bafd7dbd31a4b5e88`).

The first implementation shared literal/constructor layouts but left generic insertion building
private key buffers. Three diagnostic interleaved rounds showed literals improving 318→286 ms
and constructors 492→468 ms, but dynamic records regressing 839→888 ms and JSON state processing
307→318 ms. That incomplete version was not accepted: ordinary insertion sharing was added before
the final verification. Its hashes, patch and raw results remain in the local evidence directory.

The next iteration exposed another cost: creation IC hits repeated string-keyed transition
lookups to fetch layouts despite already knowing the destination shape. Its dynamic-record probe
remained slower (837→904 ms). Those timings are retained in `objects-final.json` despite that
early filename; they are **not** the final accepted build. That timing batch was stopped after
the completed object/micro/Vue lanes and part of the classic lane, before replacing the lookup
with direct bounded shape-ID access. Final-build labels and hashes below identify accepted data.

A fixed-work DeltaBlue diagnostic then exposed a native integration regression: learned/shared
keys could be equal strings with different `Rc` identities, so the new prediction guard sent
every field of a warmed constructor to the helper. Content equality restored native creation;
the offending stores disappear from the top-24 helper sites, which now match the parent's list.
A test-only counter verifies that warmed stores with deliberately distinct key allocations use
zero property-store helpers. The three-round DeltaBlue recheck improves from the rejected
roughly 4.4% regression to a remaining 1.8% slowdown. These preliminary observations are separate
from the seven-round matrix below.

The ensuing `accepted-*` timing batch was also stopped during the classic lane: despite its early
label, it is not final acceptance. Dynamic records still regressed 837→871 ms (seven rounds), and
RayTrace showed roughly 10% lower throughput. The dynamic path needed complete insertion-sequence
prediction and reservation, not only shared per-field prefixes. The forwarding path had acquired
per-instance constructor-identity hint lookups; it now adopts the selected initializer's layout
after the already-existing plan guards. The chain-only probe records dynamic objects at
842→729 ms (three rounds). The final-build matrix below is the authority for the combined result.

Removing the forwarding hint-table work alone did **not** resolve RayTrace's regression. A
20-scene fixed-work trace identified roughly 396,000 extra property-store helper entries at its
three leading affected sites. Predictions alone were insufficient for ordinary post-construction
field additions: the old native path could append to spare capacity, whereas the first shared-key
version required a predicted key. The native path now also adopts cached destination layouts,
retaining the capacity, prototype/epoch and ownership guards. Native transition/last-owner tests
cover this integration explicitly; successful compilation alone is not used as a fast-path test.

Restoring the native transitions removed those leading extra helper entries but did not by itself
remove the whole timing loss (the next three-round probe was still about 7.5% slower). Removing
the remaining per-field key work from established initializer plans brought the RayTrace probe
to roughly 2.5% below the parent. The repeated final matrix reports that remaining cost rather
than presenting restored fast-path coverage as a universal throughput win.

Evidence lives under `benchmark-results/shared-layouts-20260906/` (gitignored). Runners refuse to
overwrite existing results, record binary/program hashes and compare checksums with Node. Clean
timing, instrumented diagnostics, memory sampling, unit tests and conformance run in separate
lanes; timing uses fresh interleaved processes on CPU 5 with `LUMEN_*`/`TRUST_*` variables cleared.
The final timing matrix has 588 successful samples across 28 workloads and three engines, seven
rounds each. The comparison control is Node v24.20.0 / V8 13.6.233.17-node.53.
All build/test/benchmark process trees have cgroup memory limits, zero swap, deadlines and disabled
core dumps. Installed binaries and browser acceptance records are unchanged.
The host is shared with unrelated user work; CPU affinity is not exclusive CPU reservation.
Bootstrap intervals describe these samples, not uncertainty from every possible thermal or
background workload. Degenerate intervals on millisecond-quantized probes do not imply perfect
measurement precision. Tiny component shifts are not promoted into architectural success claims.

## Correctness checks and pre-existing conformance failures

The final combined unit run passes **1,016 tests**, with one existing ignored test: 919 engine,
36 host, and 61 web. The fifteen new tests cover layout sharing across templates, constructors,
dynamic creation sites and JSON; descriptor independence; invisible predictions; divergent writes;
deletion/key ordering; prototype setters and non-extensible receivers; proxies; array slot shifts
and failed truncation; GC edges/cycles; bounded cache growth/Agent lifetime; and deduplicated memory
accounting including unused predicted keys. `cargo clippy --offline --locked -j2 -p lumen --lib
--tests -- -D warnings`, formatting, and diff-whitespace checks pass.

An expanded 30,894-file native-tier Test262 sweep during development passed 30,889 files, with
four timeouts and one runner-reported upstream exclusion. All four timeouts are pre-existing:
the exact parent runner reproduces the same failures in its 43-file TypedArray `indexOf` subtree
(39 pass, four timeout). The affected tests are the Number and BigInt versions of
`fromIndex-equal-or-greater-length-returns-minus-one.js` and `fromIndex-infinity.js`.
The unchanged typed-array implementation uses `while k != end` even when the positive starting
index is already past `end`, so it advances away from its termination condition. This issue is
recorded rather than silently skipped or described as a storage regression. No typed-array search
implementation was changed in this round. The initial full sweep used a 60-second watchdog;
the focused parent reproduction used five seconds per test.

The first parent search run was deliberately stopped after reproducing a timeout and replaced
with the bounded focused reproduction. One attempted overlapping conformance invocation was
refused by the runner's existing global lock; that attempt supplied no test results. Those logs
are preserved separately from completed verification.

The final exact build passes **29,448/29,448 Test262 files, zero failures and zero skips,
separately in interpreter, bytecode and JIT modes**, with compiled thresholds set to zero.
This selection includes the whole Object, Array, Function, Proxy and Reflect subtrees plus the
language/Annex-B selection recorded in each run's metadata. Test262 revision:
`d86b2294eb0a17eaa281ff12c73c473ec864c72f`. This is not full Test262/WPT/browser acceptance.
Completed final runs are `test262-field-only-{interp,bytecode,jit}`. A first attempt reused the
old `verified` label and was refused before execution by the no-overwrite guard; it supplied no
new results and did not replace any existing evidence.

Final release artifacts (canonical fat LTO, one codegen unit, rustc 1.98.0):

- `target/shared-layouts-20260906/release/lumen`, SHA-256
  `9d45aab91bd5f9f9c5ff2160ac837d9db6e8d4a94445c774cbd177f3fdb14a67`.
- `target/shared-layouts-20260906/release/test262-runner`, SHA-256
  `b832fb7741a652c2f886f3070bda13f55bd22e656bb0371597673d973835b6b2`.

`provenance-field-only.json` records the complete source/build hash set. Execution is verified on
AArch64; no native x86-64 execution result or full-browser performance claim is made.

## Final clean timing: object workloads

Seven interleaved fresh-process rounds against the unchanged parent, exact final binary,
`field-only-objects.json`. Times are workload-reported medians in milliseconds; process startup
is outside that interval. The speedup interval is a paired-round percentile bootstrap (10,000
resamples); values above one favor the candidate. These are diagnostic allocation/state-processing
workloads, not browser scores. The same checksum must match in both engines and Node.

| Workload | Parent ms | Candidate ms | Node ms | Candidate elapsed change | Speedup, 95% interval |
|---|---:|---:|---:|---:|---:|
| Literal records | 319 | 294 | 27 | −7.8% | 1.085 [1.078, 1.105] |
| Constructor records | 496 | 483 | 32 | −2.6% | 1.027 [1.010, 1.029] |
| Dynamic records | 836 | 728 | 118 | −12.9% | 1.148 [1.132, 1.158] |
| JSON state processing | 306 | 309 | 59 | +1.0% | 0.990 [0.981, 0.997] |

The dynamic-record result improves through general insertion-chain sharing/reservation, not
recognition of these fixture keys. Literal records benefit from one shared key-layout reference
per instance instead of one owning reference per field. Constructor gains are smaller because the
parent already reserves capacity and has initializer fast paths. JSON's extra parsing, array and
large-map work is not removed by this change; its small regression remains visible here.

## Final clean timing: unchanged application and guard probes

Seven rounds, same method and binary, `field-only-micro.json` and `field-only-vue.json`.
Intervals include parity for most tiny probes; whole-millisecond quantization is substantial for
the shortest cases. Some Node control samples round to zero and must not be used to claim huge
engine ratios. No universal throughput improvement is inferred from this table.

| Workload | Parent ms | Candidate ms | Speedup, 95% interval |
|---|---:|---:|---:|
| Sloppy forwarding | 48 | 48 | 1.000 [1.000, 1.043] |
| Strict forwarding | 90 | 90 | 1.000 [0.989, 1.011] |
| Strict local return | 73 | 73 | 1.000 [1.000, 1.014] |
| Anonymous expression | 28 | 28 | 1.000 [0.964, 1.037] |
| Named expression | 28 | 28 | 1.000 [0.964, 1.037] |
| Arguments | 578 | 560 | 1.032 [1.023, 1.056] |
| Captured counter | 28 | 28 | 1.000 [1.000, 1.000] |
| Lexical arrow | 48 | 48 | 1.000 [1.000, 1.000] |
| Strict getter | 188 | 187 | 1.005 [0.989, 1.011] |
| Strict getter/local | 173 | 172 | 1.006 [0.966, 1.024] |
| Numeric array | 94 | 94 | 1.000 [0.989, 1.011] |
| Typed array | 265 | 264 | 1.004 [0.996, 1.015] |
| Plain field | 91 | 91 | 1.000 [1.000, 1.022] |
| Proxy field | 465 | 480 | 0.969 [0.958, 0.983] |
| Allocation | 76 | 74 | 1.027 [1.014, 1.041] |
| Vue reactivity kernel | 1,140 | 1,163 | 0.980 [0.973, 0.988] |

The Proxy probe remains **3.2% slower**, and the unchanged Vue kernel **2.0% slower**. The final
field-only read integration avoids unnecessary key-layout loads but does not remove those
regressions; its existence is not evidence of a measured win. The broad `checkpoint-*` run of the
preceding build independently showed similar small losses. They are not dismissed as noise.
Vue's checked output remains `[30016,12359]`; Node's final median is 107 ms, leaving Lumen about
10.9× slower on this kernel. This is neither a full Vue application nor Speedometer/browser parity.

## Final clean timing: classic engine components

Seven rounds per engine/component, `field-only-classic.json`, 168 successful samples. Scores are
higher-is-better; the interval has the same paired-bootstrap definition as above. These are the
unchanged legacy V8-v7 component workloads, not current web applications or browser scores.

| Component | Parent score | Candidate score | Node score | Candidate score change | Speedup, 95% interval |
|---|---:|---:|---:|---:|---:|
| DeltaBlue | 837 | 822 | 52,544 | −1.8% | 0.982 [0.980, 0.988] |
| EarleyBoyer | 777 | 788 | 44,705 | +1.4% | 1.014 [1.008, 1.026] |
| NavierStokes | 12,614 | 12,649 | 20,034 | +0.3% | 1.003 [1.001, 1.005] |
| Richards | 1,033 | 1,020 | 17,189 | −1.3% | 0.987 [0.977, 0.993] |
| RayTrace | 1,910 | 1,881 | 50,911 | −1.5% | 0.985 [0.982, 0.991] |
| Splay | 2,795 | 2,800 | 4,213 | +0.2% | 1.002 [0.981, 1.011] |
| Crypto | 7,358 | 7,296 | 28,602 | −0.8% | 0.992 [0.989, 0.994] |
| RegExp | 479 | 474 | 6,721 | −1.0% | 0.990 [0.983, 0.994] |

The unweighted geometric mean of the eight candidate/parent median-score ratios is **0.9942**,
or 0.6% lower. It is a descriptive engine diagnostic, not a product-weighted aggregate or an
acceptance pass. The large early RayTrace loss was reduced, not converted into a win. Node's
Splay controls range from 3,693 to 15,060, and other Node components also vary; no single overall
V8-parity ratio is inferred from them. DeltaBlue and EarleyBoyer still expose very large gaps.

## Final memory and collection diagnostics

Three interleaved fresh-process rounds, exact final binary, `field-only-memory-{heap,vue}.json`.
Managed figures are complete post-GC snapshots of **requested payload bytes, quality
`lower_bound`**, not allocator-usable bytes or peak live heap. Shared layout headers, key buffers,
unused predictions and retained caches are counted once. RSS is process peak RSS in KiB, and
includes allocations outside the managed census. These instrumented runs are not clean timings.

| Median metric | Retained graph: parent → candidate | Vue: parent → candidate |
|---|---:|---:|
| Reported managed requested bytes | 13,381,321 → 9,873,557 (−26.2%) | 6,246,835 → 6,430,272 (+2.9%) |
| Detached property storage bytes | 9,001,056 → 5,248,152 (−41.7%) | 267,944 → 414,288 (+54.6%) |
| Object body bytes | 3,720,840 → 3,968,896 | 227,520 → 242,688 |
| Peak RSS, KiB | 39,672 → 34,688 (−12.6%) | 35,704 → 35,512 (−0.5%) |
| GC total, ms | 14.286 → 13.580 | 3.835 → 3.902 |
| Maximum GC pause, ms | 7.393 → 6.913 | 3.346 → 3.372 |
| Collection count | 2 → 2 | 2 → 2 |
| Generated native code bytes | 3,104 → 3,104 | 392,640 → 396,752 (+1.0%) |
| Native compilation, ms | 0.068 → 0.094 | 2.992 → 2.982 |

The object body grows from 120 to 128 bytes because it holds a layout pointer. This is a favorable
trade for many live repeated-field records, but metadata and reserved slack dominate some smaller
live graphs, including this Vue snapshot. Three samples do not establish a reliable RSS, compiler
latency, or GC-pause improvement; in particular, a tiny one-chunk compilation is not a meaningful
percentage comparison. The collector and its collection frequency are unchanged.

## Keep / rework decision and next architectural target

**Keep the default-on storage migration as an engine-development checkpoint; do not accept the
full performance phase or promote a browser build.** The retained repeated-record graph has a
material storage reduction, and general dynamic/literal creation improves without a fixture flag.
The broader throughput regressions above remain real costs, not waived correctness checks or
unreported losses. No conformance failure introduced by this change was found in the tested slice.

This does not meet Phase 4's 25% object-heavy score-geomean target or 50% generic-helper reduction.
Nor does a 26.2% lower post-GC requested-byte census establish its 20% **peak** managed-heap target
on locked browser/object-heavy workloads. Descriptor/prototype Map identities, authoritative
elements, moving roots and the production nursery are still missing. The existing collector still
scans reference-counted objects, and execution values still use the existing 16-byte ABI.

The next major allocation round should complete a production Agent-owned object/value family
through precise VM, native and host roots, then use that ownership model for nursery allocation.
Its acceptance needs native fast-path coverage, mutation/GC/root stress, reduced collector work,
and warmed real-browser replay—not merely a separate tagged-heap prototype. General optimizing
code must then consume stable field/Map identities; shared ordered keys alone are not descriptor
or prototype validity proofs. The JSON, Proxy, Vue and classic regression lanes in this report
remain part of that acceptance matrix.

## Reproducing the object diagnostics

The new workload sources are checked in as `scripts/fixtures/object-layouts.js` and
`scripts/fixtures/retained-object-graph.js`. From the repository root, a single literal sample is:

```sh
node -e 'process.stdout.write(require("node:fs").readFileSync("scripts/fixtures/object-layouts.js","utf8").replace("__CASE__","literal"))' | target/shared-layouts-20260906/release/lumen
```

Replace `literal` with `construct`, `dynamic`, or `json-state` for the other cases. A retained-graph
memory diagnostic, deliberately separate from clean timing, is:

```sh
LUMEN_PERF_METRICS=1 target/shared-layouts-20260906/release/lumen < scripts/fixtures/retained-object-graph.js
```

The local `run.mjs`, `memory.mjs`, `test262.mjs`, and `finish.mjs` evidence runners additionally
enforce interleaving, exact comparison binaries, affinity, process timeouts, result validation,
and no-overwrite labels. They are development evidence tooling, not installed engine components.
