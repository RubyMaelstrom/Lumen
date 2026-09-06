# Evidence-led engine optimization round — 2026-09-06

This follows [the release real-site baseline](REAL_SITE_PROFILING_2026-09-06.md).
It measures live, logged-out browser workloads, not V8 parity or final desktop/terminal paint.

## Completed checkpoint

Six default-on optimization steps are committed in `70bb494`, `91960a3`, and `bacc912`:
owned string concatenation, dense-index lookup, collection-local edge reuse, string observation/
binding ownership, batched native branch relaxation, and indexed compiler name pools. Conformance
repairs exposed during validation are recorded separately from performance claims.

| Real application | Measured work | Earlier release | Latest compared release | Result |
| --- | --- | ---: | ---: | --- |
| YouTube | Process CPU over the five-minute observation | 155.32 s, fresh baseline | 100.88 s, frontend | 35.1% less CPU |
| Twitch | Process CPU over the five-minute observation | 47.01 s, fresh baseline | 33.95 s, frontend | 27.8% less CPU |
| Photopea editor entry | Main startup task before the same `ImageData` failure | 29.682 s, ownership | 1.911 s, branches | 15.5× shorter task |

Live-site variation remains significant: the last Twitch step is 5.9% slower than the preceding
candidate. Ordinary execution controls are effectively flat. These are not visual-load-time,
active-interaction, working-Photopea, or V8-parity results. The final correctness gate passes
1,045 engine/host/web unit tests and 105,162 selected release Test262 case-runs across three
tiers, with zero failures, one existing ignored network unit test, and two existing Test262
exclusions per tier. Detailed mechanisms, failed gates, exact artifacts, and limitations follow.

## Controlled artifacts

All browser measurements use release builds with opt-level 3, ThinLTO, one codegen unit,
retained symbols, and TRust's default mimalloc allocator. The TRust source tree is unchanged
from the baseline. Its recorded tracked-diff hash is
`7eb8d06f79ea5c556923cfc5dbd95606cc413d42b324824bb2eb0e86cd93cea9`.

Archived executables under `benchmark-results/real-sites-20260906/binaries/`:

| Variant | Browser SHA-256 |
| --- | --- |
| `baseline` (Lumen b675da4) | `d5d054f09105ff5e8024f5c9a63e44177b8f4f616a18369fed17cbe77581b5f7` |
| `strings` | `b4c0a0091e2a88ce359a0e994f5dca7b698aa4b93f4b3c65f1d9ea94cc5f1784` |
| `indexed` (also ASCII trim repair, not later NEL repair) | `3b738504e0f4adcff41813a07d25bd5d662f84dc8c1edbb807baf589838346e4` |
| `gc-edges` (also NEL repair, not subsequent TypedArray search repair) | `e5a1aae48b436d3e947307cbf532bdccd3474f27555a52933187bf941bcfff9c` |
| `ownership` (also TypedArray search repair) | `cb6e14e6af6974e8250dc12d90c7e86bc7dde344c0352cba064b473426afe1c3` |
| `branches` (ownership + batched branch relaxation, no lexer follow-up) | `ec7a8af8ed25ba286b794a860dd4639516b7886842dc4393aa284eb59d885555` |
| `frontend` (branches + indexed compiler names and declaration grammar repairs) | `5131bbf16c0fd943b04730b1719216df568728ea5da855671fb31ecf26e0d88c` |

The observation runner accepts `SITE_PROFILE_BINARY` to choose an archived executable;
the source status captured at run time is not a claim that an older executable was built
from that current working tree. The executable hash and variant's saved source patch identify
the measured implementation. No installed browser is replaced.

## First change: shared owned-string concatenation

The baseline found 42.90 CPU-seconds in repeated left-operand ASCII scans inside `concat2`
on YouTube. The engine already records an ASCII hint in each string header, but the generic
concatenator took raw Rust string slices and lost that metadata.

`LStr::concat_owned` now serves interpreted/bytecode addition, native addition, fused property
appends, and single-argument `String.prototype.concat`. It:

- preserves conservative ASCII hints instead of rescanning accumulated prefixes;
- transfers ownership and reuses capacity only when the left buffer is uniquely owned;
- leaves aliased strings unchanged, including self-concatenation;
- canonicalizes split surrogate pairs consistently, including the previously missing fused
  property-append case;
- preserves caller coercion order, exception timing, length limits, and the native header ABI.

This still uses flat buffers: shared-prefix copies can remain quadratic. No rope representation,
collector scheduling change, tier override, or site-specific shortcut is included.

The initial 903 engine unit tests passed. An additional gate confirms the relevant addition
and append functions actually compile in bytecode and native tiers. Test262 covered 29,480
files in each of interpreter, bytecode, and JIT modes: 29,476 passed and four existing
vertical-tab `String.trim()` failures remained in each tier. The four cases were reproduced
in the previous release shell too. They are recorded as failures, not silently excluded.

### Initial release YouTube result

Each run observes 300 wall-clock seconds; `/proc` totals below extend through approximately
298.3 seconds. No build or test load ran concurrently with these measurements.

| Sampling-only run | Observed CPU seconds | CPU/core during approximately 118–298 s | Peak RSS MiB |
| --- | ---: | ---: | ---: |
| Original baseline | 152.91 | 0.377 | 1104.0 |
| String candidate | 125.10 | 0.294 | 1125.7 |
| Fresh baseline repeat | 155.32 | 0.377 | 1107.7 |

The string candidate used **19.5% less CPU than the fresh baseline repeat**, and 18.2% less
than the original sampling baseline. Late background CPU fell about 22%. Its observed peak
RSS was about 1.6% above the fresh baseline, so this is not reported as a memory win.

The mechanism is visible in the native profiles: the fresh baseline spent 42.25 exclusive
CPU-seconds in `concat2`; named string-concatenation routines in the candidate consumed
0.10 seconds. Copying still consumed 15.11 seconds across all callers, versus 14.12 in the
fresh baseline. Identified collector routines consumed 22.89 seconds versus 20.01.
Removing one operation's cost does not imply that all other costs or timer counts stay fixed.

These are live-site CPU observations, not a controlled fixed-work throughput score or a
19.5% improvement in visual load time. Both variants ended with the populated YouTube
navigation/history-off semantic document. Final painting of the privacy notice and SVGs is
not established by the headless semantic output. Further repetitions and final-frontend
acceptance remain necessary.

Artifacts: `youtube-strings-sample-01` and `youtube-baseline-sample-02`, with metadata,
resource counters, semantic output, raw sampler experiments, function tables, and summaries.

## Second change: indexed-property lookup

The Twitch baseline put 3.02 exclusive CPU-seconds in `Props::get_mut`, 1.63 in `Props::remove`,
and 3.37 in `bcmp` across all callers. Disassembly located hot linear key scans in the mutable
and removal paths. This does not prove that every comparison was an array-index lookup.

Classic dense arrays intentionally avoid a redundant numeric-key hash table. Read lookup used
their direct index-to-entry table, but the common entry lookup used for mutation, replacement,
deletion, and slot caches did not. The follow-up makes this shared entry lookup use the same
table, retaining the existing sparse fallback. A dense miss proves absence only when no far
index has escaped the table. Descriptor checks and mutation bookkeeping are unchanged.

Lookup becomes constant-time for these dense indices; deletion can still move entries and
repair table slots, so this is **not** a claim that every deletion becomes constant-time.

Focused tests cover 4,096-element maps, shifted slots, deletion/reinsertion, holes, sparse
indices, non-canonical numeric-looking keys, symbols, descriptors, prototypes, packed arrays,
ordinary numeric-key objects, and failed array-length truncation.

The indexed release passed all 29,480 selected Test262 files in each of the three tiers
(88,440 case-runs). A deterministic release-shell probe mutating 8,192 existing indexed
descriptors four times improved from 491 to 69 ms in the interpreter, 451 to 38 ms in bytecode,
and 464 to 57 ms in JIT mode (medians of three alternating runs). All 36 size/tier/variant
runs validated their results. These are mechanism tests, not web throughput scores.
Descending deletion improved too, but retains linear slot-repair work per deletion.
Artifact: `dense-probe-indexed-v2.json` and its two fixture/runner scripts.

### Initial release Twitch result

| Sampling-only run | Observed CPU seconds | Peak RSS MiB |
| --- | ---: | ---: |
| Original baseline | 45.76 | 939.6 |
| String candidate | 40.51 | 898.1 |
| String + indexed candidate | 37.85 | 929.8 |

The indexed candidate used 6.6% less observed CPU than the string candidate, or 17.3% less
than the original baseline. Unlike YouTube's string comparison, these Twitch variants do not
yet have an interleaved baseline repeat, so live variation remains a material qualification.
Property removal's exclusive samples fell from 1.62 to 0.71 CPU-seconds and all-caller `bcmp`
from 2.78 to 0.65 seconds; `Props::get_mut` fell from 2.59 to 0.03 seconds. Collector routines
now account for 5.92 of 37.82 sampled CPU-seconds.
Late background activity remains approximately 0.01 CPU/core. This is logged-out homepage
observation, not video playback, interaction latency, or final-frontend paint acceptance.
Artifacts: `twitch-strings-sample-01`, `twitch-indexed-sample-01`.

The indexed YouTube regression scan observed 119.69 CPU-seconds and 1,101.7 MiB peak RSS,
versus 125.10 seconds and 1,125.7 MiB for the string candidate. Late background CPU was
essentially unchanged (0.295 versus 0.294 CPU/core), with the reduction concentrated in the
first minute. This one run does not establish a precise extra 4.3% gain. Its populated semantic
output remains consistent with the earlier scans. Named collector samples were 23.18 seconds;
all-caller copying was 15.06 seconds. Artifact: `youtube-indexed-sample-01`.

### Conformance repairs discovered by the gates

JavaScript's ASCII whitespace includes U+000B, unlike Rust's `trim_ascii` set. The indexed
release fixes that pre-existing defect, removing all four observed Test262 failures per tier.
The new test covering every ASCII byte and Unicode boundaries then exposed the converse
Unicode mismatch: Rust considers U+0085 whitespace, but ECMAScript does not. The subsequent
source repair shares the regex implementation's explicit ECMAScript predicate and passes
the focused regression across all tiers. It is not part of the archived indexed executable.
These are conformance repairs, not the claimed throughput lever.

## Third change: bounded GC edge reuse

The string-optimized YouTube profile spends 22.89 of 125.11 sampled CPU-seconds in named
collector routines. Baseline instruction samples show substantial property-tag decoding and
side-table lookup in `obj_refs_into`. Source inspection confirms each reachable object is
scanned once for internal-reference counting and again for marking. Primitive properties were
also unpacked/cloned merely to ask whether they contain an object or a packed-array hole.

The candidate checks packed tags directly and caches strong object/scope edges only within
one synchronous collection, with a 32 MiB cap on requested backing capacities. Cached handles
are counted as collector-owned references, never roots. Owners exceeding the cache budget
retain the original tracing path. Snapshot indices reuse existing collector scratch storage;
registry slots are restored before releasing the cache or sweeping. Collection triggers,
weak-target clearing, ephemeron conditions, and cleanup-job scheduling are unchanged.
This is not a nursery or an incremental collector.

The corrected differential fixtures pass with no cache, mixed budget fallback, and the normal
budget. They exercise properties, closures, mapped arguments, classes, accessors, bound calls,
maps/sets, proxies, promises, typed-array buffers, weak cycles, ephemeron chains, and deferred
finalization. The broader engine/host/web unit gate passes 1,032 tests, with one existing external
TLS/network test ignored. Formatting, diff whitespace checks, and engine clippy with denied
warnings pass. A first fixture error (placing allegedly dead locals in a live closure's scope)
failed even with the cache disabled and was corrected by separating the scopes.

In a fixed 16,000-node retained-graph release probe, 40 explicit collections took median
322 ms before and 223 ms after (30.7% less collection time; five alternating runs per variant).
All runs checked graph integrity. This isolates collector work; it is not a browser speedup.
Artifact: `gc-probe-v1.json` and `gc-probe.{js,mjs}`.

The initial release YouTube comparison observes **119.69 → 114.55 CPU-seconds** (4.3% less),
with named collector samples **23.18 → 16.05 seconds** (30.8% less). Late CPU falls from
0.295 to 0.278 CPU/core; peak observed RSS is 1,101.7 → 1,084.8 MiB. This remains a single
live-site comparison, not a fixed-work score, precise memory saving, or visual load-time claim.
Copying grows to 16.08 sampled seconds, and bytecode execution retains 12.36 exclusive seconds.
Artifact: `youtube-gc-sample-01`.

The Twitch follow-up observes **37.85 → 33.92 CPU-seconds** (10.4% less), with named collector
samples **5.92 → 4.42 seconds** (25.3% less). Late CPU remains approximately 0.01 CPU/core;
peak observed RSS is 929.8 → 927.0 MiB. Combined changes are about 26% below the original
Twitch CPU observation and the fresh YouTube baseline, subject to live-run variation and
the missing fresh Twitch baseline repeat. Artifact: `twitch-gc-sample-01`.

### Expanded gate discovered a pre-existing TypedArray loop defect

The GC gate expands the selection to 33,393 Test262 files with weak collections, promises,
typed arrays, buffers, and realms. Its initial interpreter run was stopped after repeated
watchdog timeouts in `TypedArray.prototype.indexOf` tests; it is **not** recorded as a pass.
Both archived `indexed` and `gc-edges` release shells fail to terminate even the one-element
case `new Uint8Array([42]).indexOf(42, 2)` within 1.5 seconds. The unchanged loop advanced from
an out-of-range start while waiting to equal a smaller end index.

The subsequent repair clamps the forward starting index to the captured length, as required
by ToClampedIndex. Regression cases cover all numeric/BigInt typed-array families, extreme
finite/infinite indices, negative indices, missing/undefined arguments, coercion effects,
buffer growth/shrink/detach, and the zero-length early return. This repair is not in the
archived `gc-edges` build and is not counted as a collector performance benefit.

## Fourth change: string ownership at local stores and observations

A separate YouTube function-diagnostic run (`youtube-gc-functions-01`) records approximately
18.00 seconds under TextDecoder's JS decode entry, 9.41 seconds under a text-to-byte-string
conversion loop, 4.38 seconds under random-string generation, and 2.96 seconds under base64
encoding/decoding. These are prefix-grouped **wall timings**, not CPU attribution: direct
compiled children are not all separately instrumented, only the top 32 entries per flush are
printed, and distinct functions/realms can share a source prefix. They identify concrete
accumulation loops without proving that every `memcpy` sample belongs to them.

Source inspection found two general barriers to using the existing owned-string path:

- `Add` followed immediately by `StoreLocal` or `StoreCap` still holds the old binding's string owner until
  after concatenation, forcing a copy even when no real JS alias needs the old buffer.
- Generic ASCII length/character observation installs an owning UTF-16 cache entry despite
  needing no derived representation, pinning otherwise unique accumulators.

The candidate retires an overwritten local/captured owner only after both operands are primitive
strings, RHS evaluation has finished, the length guard has passed, and pointer identity proves
which owner is redundant. The original Add/Store instructions and feedback remain unchanged;
packed native frames update only the affected word. All coercing/throwing cases retain their
original binding state and path. ASCII metadata reads bypass the owning cache; Unicode and
explicitly materialized unit vectors retain the existing representation/cache behavior.
Captured cells additionally require initialization, mutability, and no import indirection;
globals, name resolution, immutable bindings and TDZ paths remain unchanged.

This is engine-wide ownership handling, not replacement of browser polyfills or recognition of
website functions. Nine focused string tests pass, including actual adjacent local/captured
Add/Store bytecodes and native compilation, RHS binding changes, aliases, coercions, exceptions,
UTF-16 observations, and cache ownership. The full engine/host/web unit gate passes 1,037 tests,
with one existing network test ignored. Formatting, diff checks, and engine clippy with denied
warnings pass. The expanded release Test262 gate passes 33,391 files with zero failures in
each of interpreter, bytecode, and JIT modes (100,173 passing case-runs). Two existing runner
exclusions per tier remain visible: the Promise `allSettledKeyed` destructive-descriptor test
and the TypedArray species test missing its immutable-buffer exclusion. No new exclusions
were added. Artifacts: `test262-ownership-v1-{interp,bytecode,jit}`.
The release mechanism probe uses three alternating runs per variant/tier/fixture, with all
36 child runs validating their results. At 262,144 output bytes, local byte-to-string
accumulation improves from median **1,548 → 172 ms in bytecode (9.0×)** and
**1,946 → 131 ms in JIT (14.9×)**. At 65,536 bytes the respective medians are 115 → 43 ms
and 107 → 33 ms. The interpreter does not implement this binding handoff and remains quadratic:
its 262,144-byte samples are highly variable (before 1,148/1,532/1,966 ms; after
1,198/1,797/3,093 ms), so no interpreter local-append win is claimed.

For 64,000 property appends with an ASCII character observation before each append, bypassing
the owning unit cache improves interpreter median 337 → 123 ms and bytecode 292 → 24 ms.
The existing JIT character intrinsic already bypasses that cache: its 5 → 6 ms medians are
too short/noisy to establish a material effect. These are mechanism probes, not web scores.
Artifact: `string-probe-v1.json` and both string fixtures.

The 15 numeric/call/property/allocation controls across three tiers, three alternating rounds,
and two variants all validate matching checksums (270 runs). Median-time geomeans change by
-0.25% interpreter, -0.09% bytecode, and -0.25% JIT; individual medians range from -1.82% to
+1.56%. This limited gate finds no broad numeric or call regression, not proof of universal
parity. Artifact: `ownership-micro-v1.json`.

The eight classic components pass their correctness checks in 48 runs (three alternating
rounds per variant). The geomean of component median scores is +0.26%; components range from
-0.24% (EarleyBoyer) to +1.03% (RegExp), effectively flat. Both regression controls pin CPU 5;
the string mechanism probe is unpinned. These controls compare `gc-edges` with `ownership`,
not the entire round against the original browser baseline. Artifact: `ownership-classic-v1.json`.
The first release YouTube follow-up observes **114.55 → 103.47 CPU-seconds** (9.7% less).
All-caller copying samples fall **16.08 → 2.25 seconds**, consistent with removal of repeated
accumulator copies. Late background CPU falls **0.278 → 0.248 CPU/core** (10.9% less);
peak observed RSS is 1,084.8 → 1,064.7 MiB. Named collector work remains 17.09 sampled seconds,
so its cost has not vanished. The candidate still reaches the populated navigation/history-off
semantic page; final privacy-notice/SVG paint is not measured.

Against the fresh original baseline, observed YouTube CPU is **155.32 → 103.47 seconds (33.4%
less)** and late CPU is 0.377 → 0.248 CPU/core (34.2% less). These remain live-site observations,
not fixed-work throughput or visual-load-time gains. Artifact: `youtube-ownership-sample-01`.
The engine implementation is committed as `70bb494`; the archived release was built before
that commit from the recorded matching production source. Subsequent scans retain the same
binary hash and unchanged TRust tree.

Twitch's final ownership run observes **34.44 CPU-seconds**, versus 33.92 in the preceding GC
run (+1.5%, a single live comparison with no incremental win claimed). Copying samples fall
0.70 → 0.41 seconds, but other work varies. The combined result remains **24.7% below the
original 45.76-second observation**, with the important qualification that at this checkpoint
the original Twitch baseline had not been repeated (the later reverse-order repeat is below).
Late CPU is 0.009 CPU/core, and peak observed RSS is
932.0 MiB. The populated homepage remains present. Artifact: `twitch-ownership-sample-01`.

## Broader application selection

The new-site probes selected were [VS Code for the Web](https://code.visualstudio.com/docs/remote/vscode-web)
at `vscode.dev`, a browser-hosted code editor, and [Photopea](https://www.photopea.com/learn/),
an image editor. Their official application descriptions were checked on 2026-09-06. The intended
contrast is editor/workbench and graphics-application work versus logged-out media homepages;
the specific engine bottlenecks and achieved application state must come from the actual scans,
not from assuming that an application's category predicts its profile.

### VS Code startup is blocked, not fast

`vscode-ownership-profile-01` retains a complete 300-second observation. It reports just
2.50 process CPU-seconds (0.31 in the page actor), because the workbench does not start.
The 326,933-byte inline bootstrap fails with `SyntaxError: unexpected token Punct("/")`.
A separately captured copy has SHA-256
`b3a233dcfa9a58ff8ef43c5a7e2018f0eb3912e37ebd94d1e248ef23202a399d` and reproduces the
same error in both archived `gc-edges` and `ownership` release shells. Node's module syntax
check accepts it. `LUMEN_PARSE_TRACE` identifies code-point offset 322420, the regular-expression
statement immediately following two function declarations. This is a pre-existing parser
investigation, not an optimization gain or a website fault.

Separately, the main workbench bundle fetch reports exactly 16,777,216 decoded bytes;
TRust's `inflate_tolerant` truncates decompression at that same 16 MiB bound. The inline
syntax failure precedes completion of that fetch, so these are distinct issues. The browser
transport tree has not been changed. Artifact: `vscode-source-01` plus the live profile.

A later read-only capture verifies the full bundle is **18,917,924 decoded bytes** (4,834,183
gzip bytes), SHA-256 `6e7c08eecb0a857174a8e3c1e6e98cb14ddf367109f53dd4266de9c73fdddeda`.
Node v24.20.0's module syntax-only check accepts the complete source and rejects its exact
16 MiB prefix with `SyntaxError: Unexpected end of input`. No website code was executed by
these checks. This establishes an actual truncated-resource problem, not just a suspicious
round byte count. Artifact: `vscode-bundle-01` with response headers and syntax-check results.

### Entering the actual Photopea editor

The current Photopea root URL is a marketing page with a “Start using Photopea” button,
not the editor workload. Its initial observation was deliberately terminated at 166.8 seconds
after confirming the author-provided start gate; `photopea-ownership-profile-01` is preserved
as an aborted landing-page probe, not a throughput result.

The [official Photopea API](https://www.photopea.com/api/) permits an encoded JSON object
after the URL hash, with all parameters optional. The editor runs use
`https://www.photopea.com/#%7B%7D` (empty configuration), which enters the same application
loader without injecting a script, changing browser code, or loading a user document.
An additional [Excalidraw application](https://github.com/excalidraw/excalidraw) probe ensures
a blocked VS Code startup does not stand in for a running complex application.

### Photopea identifies a large native-compilation bottleneck

`photopea-editor-ownership-profile-01` observes the actual editor entry for 300 seconds.
Its three application scripts (1,412,774, 1,158,108, and 2,635,632 decoded bytes) finish fetching
by 0.805 seconds. The main application task then occupies 29.682 seconds and ends with
`ReferenceError: ImageData is not defined` at approximately 29.98 seconds. No working-editor
or active graphics-throughput claim is made: the missing platform constructor is a separate,
unchanged TRust compatibility issue.

The process consumes 31.47 CPU-seconds (29.40 in the page actor), with 492.25 MiB peak RSS.
Of 31.43 sampled CPU-seconds, **19.12 are exclusive to `lumen::jit::compile` and 8.50 to
`__memmove_sve`**. Instruction-level samples identify repeated native patch-offset scans;
the compiler inserts one long-branch veneer, moves the remaining code, rescans all labels
and patches, and restarts. Large functions repeatedly pay this quadratic cost. This is a
general compiler defect exposed by Photopea, independent of the subsequent platform error.

### Excalidraw reaches the editor and becomes idle

`excalidraw-ownership-profile-01` completes a 300-second observation with the semantic
document titled “Excalidraw Whiteboard” and the selection, shape, drawing, text, image, zoom,
undo/redo, and library controls present. Its main module completes in approximately 0.798 s;
the DOM reaches 371 nodes and 29 SVGs/57 paths by 1.973 s. Those are DOM/semantic observations,
not a measurement of final canvas paint.

Observed process CPU is 3.43 seconds (1.37 in the actor), peak RSS 332.46 MiB, and the actor
adds no measurable CPU in the late 118–298-second interval. Named compiler samples are just
0.03 exclusive seconds, parser/lexer samples 0.38, and collector samples 0.07. This empty
editor's startup does not reproduce Photopea's compiler bottleneck. Active editing and a
substantial drawing remain necessary before making interaction-throughput claims.

## Fifth change: batched ARM64 branch relaxation (`91960a3`)

The replacement determines which conditional branches need veneers in immutable instruction
coordinates, relocates labels and patch offsets with prefix counts, and constructs the final
code buffer once. Ordinary functions that need no widening retain an allocation-free fast
path. Up to eight exact batches find the least fixed point in normal cases. An adversarial
cascade exceeding that limit uses a conservative all-conditional-widening distance bound;
it can add extra veneers but bounds the compiler's work and preserves every destination.
Catch destinations are exported from the same final label layout as ordinary branches.

New tests compare complete emitted code against the previous algorithm for deterministic
mixed forward/backward B.cond, CBZ/CBNZ (32/64-bit), B and BL graphs; exercise both signed
imm19 boundaries, cascaded widening, forced fallback, unused labels, and 16,000 far branches;
and decode every resulting destination. The existing large-function throw/catch integration
test runs in all tiers. The engine unit gate passes **943 tests**, and the expanded release
JIT gate passes **33,391 Test262 files, zero failures, two unchanged exclusions**. Formatting
and clippy with denied warnings pass. The archived variant predates the separate async-lexer
follow-up; it contains only branch-relaxation production changes beyond `70bb494`.

### Fixed-work compilation scaling

`branches-scaling-v1.json` records three alternating runs per binary and size, pinned to CPU 5,
with immediate native tiering and the existing performance-metrics diagnostic enabled. Each
generic generated function reads an object's property repeatedly inside a conditional, then
throws and catches a value; both the taken and skipped paths are checked. No website source or
host-API stubs are used. All 18 runs return the expected checksum and successfully compile.

| Repeated property operations | Previous median native compilation | Batched median native compilation | Ratio | Generated bytes, both |
| --- | ---: | ---: | ---: | ---: |
| 1,000 | 0.05765 s | 0.01064 s | 5.4× | 1,230,652 |
| 4,000 | 5.11353 s | 0.03748 s | 136.4× | 4,938,652 |
| 8,000 | 24.91004 s | 0.07171 s | 347.3× | 9,882,652 |

Whole-process median wall time for the largest case is 24,949 → 110 ms. Generated sizes match
in every pair. The difference isolates previously quadratic compilation work; it is not a
347× improvement in executing JavaScript or a website's end-to-end load time.

The 270 checksum-validated numeric/call/property/allocation control runs, again with three
alternating samples and CPU 5 affinity, give median-time geomean changes of −0.154% interpreted,
−0.044% bytecode, and +0.225% native. Individual changes range from −3.57% to +3.76% in the
native tier; no general execution-throughput gain is claimed. The 48 successful classic-suite
runs give a median-score geomean of −0.158% (components −1.02% to +0.48%). These controls are
effectively flat overall, not benchmark wins. Artifacts: `branches-micro-v1.json` and
`branches-classic-v1.json`, both comparing against the immediately preceding `ownership` build.

### Release Photopea comparison

`photopea-editor-branches-profile-01` repeats the full 300-second editor-entry observation,
using the unchanged browser tree and the same versioned script URLs and decoded byte counts.
Raw fetched script bodies were not archived by these browser runs, so matching URLs/lengths
are not represented as a byte-identity proof.

| Observation | Ownership | Batched branches |
| --- | ---: | ---: |
| Main startup task span | 29.682 s | 1.911 s |
| Time of the same missing-ImageData error | 29.980 s | 2.499 s |
| Process CPU through approximately 298 s | 31.47 s | 3.95 s |
| Page-actor CPU | 29.40 s | 1.80 s |
| Peak observed RSS | 492.25 MiB | 435.29 MiB |

The startup task is **15.5× shorter**, observed actor CPU is **93.9% lower**, and total observed
CPU is **87.4% lower**. Native-compiler exclusive samples fall from 19.12 seconds to none at
the 10 ms sampling interval (0.03 seconds inclusive); the previous 8.50 seconds of `memmove`
samples also disappear. This agrees with the controlled compilation-scaling probe.

Crucially, both variants still fail on the missing `ImageData` platform constructor. The gain
is removal of wasted engine work before that same failure, **not** a working-editor load time,
graphics throughput, or final-paint acceptance.

The YouTube regression observation (`youtube-branches-sample-01`) uses **101.49 CPU-seconds**,
versus 103.47 for ownership (−1.9%, no meaningful incremental win claimed). Late CPU is
0.240 versus 0.248 CPU/core; peak observed RSS is 1,075.77 versus 1,064.73 MiB. Its populated
navigation/history-off semantic document is preserved. Named GC samples remain 16.77 seconds
and VM execution 13.66 exclusive seconds; compiler name lookup remains visible at 1.48 seconds.
The Twitch follow-up (`twitch-branches-sample-01`) observes **32.05 CPU-seconds**, versus
34.44 for ownership (6.9% less), and 29.67 actor CPU-seconds. Native compilation's exclusive
samples fall 1.39 → 0.05 seconds; the previous 0.65 seconds of `memmove` has no samples in
this run. Named GC samples are 4.36 seconds, late CPU 0.0103 CPU/core, and peak observed RSS
925.54 MiB. The populated logged-out homepage and cookie controls remain present; this is
not a playback or interaction-latency result.

A **fresh reverse-order pre-optimization Twitch baseline** (`twitch-baseline-sample-02`) then
uses 47.01 CPU-seconds, close to the original 45.76 (2.7% higher), versus the candidate's 32.05.
That makes the combined observed reduction **31.8% against the fresh baseline** (33.6% actor
CPU reduction). The repeat again exposes mutable-property scanning (4.12 exclusive seconds),
all-caller `bcmp` (4.04 seconds), and the old native compiler (1.48 seconds). It reduces the
earlier concern about relying solely on the original Twitch baseline, without eliminating
live-content variation or replacing a controlled browser load/interaction benchmark.

## Sixth change: compiler name lookup and bootstrap grammar repairs

Committed as `bacc912` after the unit, conformance, fixed-work, and ordinary-workload gates.

- **Large compiler name pools:** successive YouTube profiles repeatedly spend 1.3–1.7 exclusive
  CPU-seconds in `Compiler::name_idx`, whose lookup scans every earlier name. A compile-only
  index now serves pools of at least 32 entries; small pools keep the existing
  allocation-free scan. The ordered vector remains authoritative, including deliberately
  duplicated contiguous literal keys. Failed inlining removes only the index entries first
  introduced by the discarded suffix. No runtime, feedback, or serialized name IDs change.
- **Async declaration lexical goals:** a minimal `async function first(){} function second(){}
  /x/.test('x')` reproduces VS Code's bootstrap syntax error in the archived ownership build.
  The follow-up classifies the start of the unescaped `async function`/`async function*`
  production, preserves expression-position division, and respects intervening line terminators.
  This corrects the bootstrap syntax, not the independent browser transport limitation below.

Both follow-ups were prepared outside the immutable `branches` browser being measured. Their
tests and builds ran after that real-site sequence finished, preserving timing isolation.

The first follow-up validation stops at a negative syntax test: the existing parser accepts
anonymous ordinary/async function declarations outside `export default`. Both minimal forms
are also accepted by the archived ownership release, while the local grammar requires a name.
The repair keeps anonymous expressions/default exports legal, requires names for ordinary
declarations, and applies the async no-LineTerminator rule in default exports too. The original
failed gate is retained as `frontend-validation-v1`; the corrected focused gates pass in `v2`.

### Follow-up validation and fixed-work scaling

The complete follow-up passes **948 engine + 36 host + 61 web unit tests = 1,045**, with one
existing external-network test ignored. Clippy with denied warnings and formatting checks pass.
The release conformance gate adds ASI, comments, identifiers, literals, module code, whitespace,
and line terminators to the prior selection: **35,054 pass, zero fail, two existing exclusions**
in each of interpreter, bytecode, and native tiers (**105,162 passing case-runs**).
Logs and build outcomes are retained under `frontend-validation-v2` and `test262-frontend-v1-*`.

The captured VS Code bootstrap now parses in the frontend release shell and reaches its
expected unresolved-HTTP-import error (the shell's loader is filesystem-only). That proves
the bootstrap syntax is accepted, not that the application executed or loaded its workbench.

`frontend-names-v1.json` records three alternating runs per size and binary, pinned to CPU 5,
with immediate **bytecode** tiering. A generic function references every distinct property name
four times. Its first call skips execution of the property body while compilation is timed;
a subsequent taken-path call verifies getter behavior and result integrity. All 18 runs pass.

| Distinct names | Previous median bytecode compilation | Indexed median bytecode compilation | Ratio |
| --- | ---: | ---: | ---: |
| 2,048 | 0.01919 s | 0.00190 s | 10.1× |
| 8,192 | 0.41435 s | 0.00817 s | 50.7× |
| 32,768 | 5.16073 s | 0.07287 s | 70.8× |

These timings isolate compiler scaling, not browser throughput. The additional index exists
only during compilation; runtime chunks retain the original ordered name vector and no index.
The 270 checksum-validated ordinary-workload runs give median-time geomean changes of +0.116%
interpreted, −0.113% bytecode, and +0.092% native (individual ranges within −2.09% to +1.37%
in the native tier). The 48 successful classic runs give a median-score geomean of +0.482%,
with components −0.10% to +2.07%. These are effectively flat overall and are not a claim of
broad execution-throughput acceleration. Both controls compare the frontend release against
the preceding branches release. Artifacts: `frontend-micro-v1.json`, `frontend-classic-v1.json`.
### Release frontend comparisons

`youtube-frontend-sample-01` observes **100.88 process CPU-seconds** and 98.24 actor CPU-seconds,
versus 101.49 and 98.80 for branches. This is only 0.6% less process CPU: no meaningful incremental
whole-site win is claimed. The compiler-name lookup falls from 1.48 exclusive sampled seconds
to none at the 10 ms sampling interval (0.01 seconds inclusive), as expected from the mechanism.
Named parser/lexer/compiler samples fall to 1.82 seconds, but other work varies. GC remains
17.06 sampled seconds and VM execution 13.54 exclusive seconds. Late CPU is 0.250 CPU/core,
versus 0.240 in the preceding run; peak observed RSS is 1,133.93 versus 1,075.77 MiB. This is
not a memory or background-CPU improvement over the preceding compiler build.

Against the fresh pre-optimization YouTube baseline, the combined changes use **35.1% less
observed CPU** (155.32 → 100.88 seconds). The observation remains a live-site CPU measurement,
not a fixed-work throughput or final privacy-notice/SVG-paint result. Its populated semantic
navigation/history-off document remains present.

`twitch-frontend-sample-01` observes **33.95 process CPU-seconds**, 31.57 actor CPU-seconds,
931.49 MiB peak RSS, and 0.00977 late CPU/core. The populated homepage, live-channel links,
and cookie controls remain present. Total CPU is **5.9% higher than branches** (32.05 seconds),
so the compiler-name follow-up is not a Twitch win. Against the fresh baseline's 47.01 seconds,
the combined reduction is **27.8%**. This variation, and the flat controlled execution probes,
are retained rather than selecting only the fastest candidate observation.

`vscode-frontend-profile-01` completes the 300-second observation. It passes the old bootstrap error,
then reports `SyntaxError: expected binding identifier` after the imported workbench fetch
again returns exactly 16,777,216 decoded bytes. The separately captured complete bundle's
prefix ends at that exact byte boundary with `function DDr(s,o,e,`, midway through a parameter
list. That is consistent with the new failure, though no claim is made that the rest of the
complete bundle has passed Lumen's parser or that fixing transport alone will load the editor.
It observes 4.70 process CPU-seconds, 2.43 actor CPU-seconds, and a 1,102.48 MiB sampled peak
while processing the large source; after startup the actor goes idle. The larger CPU/memory
totals versus the old blocked run represent newly reached parsing work, not a throughput
regression on the same completed application task. No workbench, active editing, or final
application rendering is claimed.

## Remaining engine costs after the follow-up

Instruction-level analysis of the completed `youtube-frontend-sample-01` experiment adds no
instrumentation to the measured binary and runs only after the browser timings finish.
Artifact: `engine-hotspots-v2/`, whose instruction totals reconcile to the function samples.
The initial helper missed gprofng's `##`-marked hottest lines; its incomplete `engine-hotspots/`
summary is preserved but not used for these totals.

- **VM dispatch and value traffic:** `run_vm` has 13.54 exclusive sampled seconds. The contiguous
  instruction-fetch/operand-unpack/dispatch block at `0xb582b8`–`0xb58314` accounts for 3.77
  seconds (27.8% of VM-exclusive samples). It loads a 24-byte opcode's fields before dispatch;
  other hot blocks move and drop VM values. This motivates measuring a compact/lazy operand
  dispatch or register-frame design, but does not prove a particular replacement will win.
  It is not evidence that 28% of the whole browser can be removed.
- **Whole-graph collection:** named collector routines still account for 17.06 seconds;
  object-edge tracing alone is 7.24. Its property traversal block at `0xa05090`–`0xa05204`
  accounts for 2.24 seconds, with additional samples in refcount operations and per-object
  side-table probes. Scope-edge tracing is 1.34 seconds. The evidence supports reducing
  scanning/edge bookkeeping structurally, not merely changing the collection trigger or
  assuming nursery scaffolding has removed production costs.
- **Browser integration gaps:** the TextDecoder JavaScript shim, Photopea's missing `ImageData`,
  and VS Code's truncated resource require truthful host/platform integration, not fake engine
  globals or workload recognition. TRust's pre-existing tracked changes remain untouched.

The default-on engine work is committed in `70bb494`, `91960a3`, and `bacc912`. The latest release
site observations, flat/regressing controls, correctness repairs, and unresolved application
states are all retained. No installed executable was replaced, no changes were pushed, and no
V8-parity or final frontend-paint acceptance gate is claimed.

## Standards

The local web-standards skill was used before implementation. Sources are the ECMAScript 2027
editor's draft at snapshot `e28783d5fc9dc12b3de905961e2c71410b38a202`, fetched 2026-09-06;
this is not described as a published edition or a newly verified upstream revision.

- String code-unit concatenation and coercions:
  [local definition](/big/web-standards/repositories/tc39/ecma262/spec.html:1251),
  [local operator algorithm](/big/web-standards/repositories/tc39/ecma262/spec.html:21341),
  [official operator clause](https://tc39.es/ecma262/#sec-applystringornumericbinaryoperator).
- Assignment and string observation:
  [local assignment algorithm](/big/web-standards/repositories/tc39/ecma262/spec.html:21240),
  [official assignment clause](https://tc39.es/ecma262/#sec-assignment-operators-runtime-semantics-evaluation),
  [local character algorithm](/big/web-standards/repositories/tc39/ecma262/spec.html:36250),
  [official charCodeAt clause](https://tc39.es/ecma262/#sec-string.prototype.charcodeat).
- `String.prototype.concat`:
  [local algorithm](/big/web-standards/repositories/tc39/ecma262/spec.html:36291),
  [official clause](https://tc39.es/ecma262/#sec-string.prototype.concat).
- Descriptor application and property ordering:
  [local descriptor algorithm](/big/web-standards/repositories/tc39/ecma262/spec.html:13286),
  [official descriptor clause](https://tc39.es/ecma262/#sec-validateandapplypropertydescriptor),
  [local key-order algorithm](/big/web-standards/repositories/tc39/ecma262/spec.html:13531),
  [official key-order clause](https://tc39.es/ecma262/#sec-ordinaryownpropertykeys).
- Array index and length invariants:
  [local algorithm](/big/web-standards/repositories/tc39/ecma262/spec.html:14659),
  [official array clause](https://tc39.es/ecma262/#sec-array-exotic-objects-defineownproperty-p-desc).
- Trimming:
  [local algorithm](/big/web-standards/repositories/tc39/ecma262/spec.html:37045),
  [official TrimString clause](https://tc39.es/ecma262/#sec-trimstring),
  [local whitespace table](/big/web-standards/repositories/tc39/ecma262/spec.html:16949),
  [official whitespace clause](https://tc39.es/ecma262/#sec-white-space).
- GC reachability, atomic weak clearing, and synchronous-job boundaries:
  [local processing model](/big/web-standards/repositories/tc39/ecma262/spec.html:12875),
  [official processing model](https://tc39.es/ecma262/#sec-weakref-processing-model),
  [local ClearKeptObjects](/big/web-standards/repositories/tc39/ecma262/spec.html:12997),
  [official ClearKeptObjects](https://tc39.es/ecma262/#sec-clear-kept-objects).
- Typed-array search and starting-index clamping:
  [local search algorithm](/big/web-standards/repositories/tc39/ecma262/spec.html:42775),
  [official indexOf clause](https://tc39.es/ecma262/#sec-%typedarray%.prototype.indexof),
  [local clamping algorithm](/big/web-standards/repositories/tc39/ecma262/spec.html:5841),
  [official ToClampedIndex](https://tc39.es/ecma262/#sec-toclampedindex).
- Catch completion and final native destinations:
  [local TryStatement evaluation](/big/web-standards/repositories/tc39/ecma262/spec.html:23759),
  [official TryStatement clause](https://tc39.es/ecma262/#sec-try-statement).
  The local standards catalog has no Arm ISA source. A focused lookup of
  [Arm's B.cond instruction specification, DDI0602 2024-06](https://developer.arm.com/documentation/ddi0602/2024-06/Base-Instructions/B-cond--Branch-conditionally-)
  confirms the signed PC-relative imm19 field and condition encodings; the algorithm retains
  the existing branch instructions and inversion scheme. This is not a claim to have verified
  a newer Arm manual revision.
- Lexical goals and async prefixes:
  [local lexical grammar](/big/web-standards/repositories/tc39/ecma262/spec.html:16884),
  [official lexical grammar](https://tc39.es/ecma262/#sec-ecmascript-language-lexical-grammar),
  [local async-function grammar](/big/web-standards/repositories/tc39/ecma262/spec.html:25923),
  [official async-function grammar](https://tc39.es/ecma262/#sec-async-function-definitions),
  [local ASI rules](/big/web-standards/repositories/tc39/ecma262/spec.html:18407),
  [official ASI rules](https://tc39.es/ecma262/#sec-rules-of-automatic-semicolon-insertion).
- Declaration names versus default exports:
  [local function grammar](/big/web-standards/repositories/tc39/ecma262/spec.html:24185),
  [official function grammar](https://tc39.es/ecma262/#sec-function-definitions),
  [local export grammar](/big/web-standards/repositories/tc39/ecma262/spec.html:29918),
  [official export grammar](https://tc39.es/ecma262/#sec-exports).
- Compiler name-table preservation:
  [local property-definition evaluation](/big/web-standards/repositories/tc39/ecma262/spec.html:19222),
  [official property-definition evaluation](https://tc39.es/ecma262/#sec-runtime-semantics-propertydefinitionevaluation).
  The interning optimization must leave literal-key ordering, overwrite behavior, and Unicode
  spelling distinctions intact; it changes lookup costs, not the property-definition algorithm.
