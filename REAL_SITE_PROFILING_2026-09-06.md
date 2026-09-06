# Release real-site profiling — 2026-09-06

## Scope and measurement discipline

This is a diagnostic baseline, **not an optimization result or a V8 comparison**.
The workloads are live, logged-out YouTube and Twitch homepages in TRust, not
framework replays. No engine/runtime source was changed for these runs.

All runs use the same newly built release executable:

- Binary: `/big/Code/TRust/target/site-profile-20260906/release/trust-headless`.
- SHA-256: `d5d054f09105ff5e8024f5c9a63e44177b8f4f616a18369fed17cbe77581b5f7`.
- Compiler invocation verified: `opt-level=3`, ThinLTO, one codegen unit.
  `CARGO_PROFILE_RELEASE_STRIP=none` retains symbols; this is not a debug or
  `browser-check` build. Default features include TRust's mimalloc allocator.
- Lumen: `b675da4133a32efdafd71a111f60395eb4f67b69`.
- TRust: working tree based on `03fb97c10a251a36cf5c01c0f618dad3b52f6db4`, with
  substantial pre-existing pending work. This is **not** a clean TRust commit
  baseline. The frozen binary hash identifies the measured artifact; each run
  records the observed source status and tracked diff hash.
- AArch64 Crystal workstation, Rust 1.98.1; synthetic 1365×768 CSS-pixel viewport.
- Fresh browser process and RAM-only cookies for each run. No user profile,
  installed executable, pending TRust changes, or tiering defaults were altered.
- Runs are sequential, bounded to 12 GiB RAM, zero swap, and a 420-second outer
  deadline. No compilation runs concurrently with the site measurements.
- Each browser gets `--timeout 300 --settle 0`. Exit 3 means the observation
  window ended while the resident page actor remained live. **It does not mean
  the site failed, took 300 seconds to load, or encountered a bot wall.**

Headless uses the shared controller, resident Lumen actor, DOM, and layout. It
does not measure terminal image presentation or desktop scene/GPU presentation.
The machine remains a working desktop, not an exclusively reserved benchmark
host. These are individual observations of live sites, without confidence
intervals or a controlled before/after comparison.

Three measurement lanes distinguish costs:

1. **Clean:** no Lumen/TRust diagnostic flags and no native sampler; external
   `/proc` CPU/thread/RSS observations every two seconds.
2. **Sampling only:** the same defaults, plus gprofng PC sampling every 10 ms.
3. **Detailed trace:** sampling plus network/script/task/layout/event traces and
   serialized render snapshots. The task trace executes extra JS queue census
   code and the snapshots add serialization work. Use this lane for sequence
   and investigation, not baseline throughput attribution.

`LUMEN_FEEDBACK_PROFILE` and `LUMEN_PERF_METRICS` were not enabled. No profiling
switch that disables normal native fast paths was used. The headless driver
does not currently emit Lumen's optional performance-metrics JSON.

## YouTube: a concrete engine bottleneck

In the sampling-only run, 154.48 sampled CPU-seconds were collected over a
300-second observation. `/proc` observed 152.91 CPU-seconds through 298.33 seconds,
of which 150.25 were on the page actor. The clean run observed 155.30 CPU-seconds
through 298.31 seconds, 152.85 on the actor. Peak observed RSS was approximately
1.10 GiB in the clean run. The matching scale confirms that the main CPU problem
survives removal of instrumentation; it does not establish a precise sampler
overhead percentage.

The clean run consumed 57.70 CPU-seconds in its first observed 58.06 seconds.
Between approximately 118 and 298 seconds, it consumed another 69.31 CPU-seconds
without interaction: **about 0.38 CPU core continuously on average**. Startup and
ongoing background work are separate performance problems.

Sampling-only exclusive costs (non-overlapping instruction samples):

| Area | CPU seconds | Share of sampled CPU |
| --- | ---: | ---: |
| `LStr::concat2` | 43.18 | 27.95% |
| `__memcpy_sve` across all callers | 14.03 | 9.08% |
| Identified collector routines, combined | 19.71 | 12.76% |
| `run_vm` itself, excluding callees | 10.33 | 6.69% |
| Identified parser/lexer/compiler routines | 3.14 | 2.03% |

The collector bucket sums exclusive samples in `gc_collect_with_cause`,
`obj_refs_into`, `obj_scope_refs_into`, and heap/scope snapshot helpers. It is a
conservative identifiable subset, not the complete cost of GC. The memcpy row
is **not** all attributed to concatenation. Inclusive percentages overlap and
must not be added: `run_vm` has 46.70% inclusive CPU, including work already
listed above. Native/JIT stack unwinding is incomplete even though only 0.79%
of exclusive samples have unknown symbols.

### The string finding is more specific than “allocation is expensive”

The sampled disassembly puts **42.90 seconds** in the left-operand ASCII scan
inside `concat2` (addresses `0x923a8c`–`0x923aa0` in this binary). That is almost
all of its 43.18 exclusive seconds. This is a repeatedly executed eight-byte
load/test loop, not the allocator.

Source inspection explains the work:

- [`LStr::concat2`](crates/lumen/src/lstr.rs#L94) allocates a flat result, copies
  both inputs, and calls `is_ascii()` on both inputs. It accepts `&str`, losing
  access to Lumen's existing cached ASCII hints.
- [Generic string addition](crates/lumen/src/eval.rs#L6988) reaches this path.
- The [native string-add helper](crates/lumen/src/bytecode.rs#L15656) already
  supports moving the left operand and growing a uniquely owned buffer.
  Generic/interpreted/VM paths do not all receive that optimization.
- `concat_grown` also deserves inspection: its allocation path can rescan the
  old contents. Merely moving callers onto a different helper is not sufficient.

**First optimization round: make efficient concatenation an engine-wide
primitive.** Preserve cached string metadata through concatenation, remove
redundant prefix scans, share safe ownership-aware growth across execution
tiers, and measure the remaining prefix-copy volume. Where shared/escaping
prefixes defeat buffer reuse, evaluate a rope/concatenation-node representation
with bounded depth and controlled flattening. Do not introduce a site-specific
shortcut or undertake a whole string ABI migration without measuring that need.

Removing a 28% exclusive cost has an idealized ceiling of roughly 1.39× for
this CPU mix if everything else stays constant. That is meaningful, but it is
**not a claim of a 50× breakthrough or equivalent page-load improvement**.

### Loading sequence and Ruby's completion criterion

Ruby's observed finish line is: **the privacy/cookie notification fully renders,
then the site's SVGs render**. Neither a shell nor a quiet interval is accepted
as complete.

The detailed trace observed:

| Event | Seconds from harness launch |
| --- | ---: |
| Main document response received | 0.399 |
| 10,738,700-byte main JS bundle response received | 0.612 |
| Main bundle evaluation begins | 1.958 |
| Main bundle task finishes | 8.173 |
| First saved live render snapshot | 28.721 |
| Snapshot contains privacy heading and accept/reject controls | 39.714 |
| First SVGs in saved render snapshots | 48.213 |
| SVG count reaches 29, with 48 path elements | 57.987 |

These are **serialized rendered-HTML milestones, not verified screen-paint
completion times**. The notice has dialog/layout styling in those snapshots,
but the final semantic dump does not include its text. That itself shows why
the current driver cannot certify Ruby's visual criterion. These numbers do not
invalidate or replace the reported 2–3 minute experience in the actual frontend.

The trace logged a 19.199-second timer-task checkpoint and many later roughly
two-second checkpoints. The approximately 9–28 second sample interval was
dominated by `concat2` (63.27% exclusive) and memcpy (17.53% exclusive).
Checkpoint timing includes microtask draining, the engine's task-boundary
collection, and diagnostic draining; it is not a pure GC or Promise metric.
The 49 logged full layout passes totaled about 1.236 seconds. That excludes
other DOM/style work, incremental paths, and final frontend raster/presentation;
it is not evidence that all rendering costs are negligible.

## Twitch

The sampling-only run collected 45.69 sampled CPU-seconds. `/proc` observed
45.76 CPU-seconds through 298.32 seconds, 43.49 on the page actor, with peak
observed RSS around 940 MiB. The final semantic output contains the cookie
notice and controls, navigation/search, live channels, and content listings.
That is evidence of an application page, not a bot-wall diagnosis. Playback,
visual completeness, and interaction latency were not tested.

The first observed 58.07 seconds consumed 43.40 CPU-seconds. Between approximately
118 and 298 seconds the process used only 1.73 CPU-seconds: **about 0.01 CPU core
on average**, unlike YouTube's continued background load. A five-minute timeout
would conceal this difference if it were misreported as a five-minute load time.

| Area | Exclusive CPU seconds | Share |
| --- | ---: | ---: |
| `bcmp` across all callers | 3.37 | 7.38% |
| `Props::get_mut` | 3.02 | 6.61% |
| `Props::remove` | 1.63 | 3.57% |
| Identified collector routines, combined | 5.76 | 12.61% |
| Native compiler, `lumen::jit::compile` | 1.35 | 2.95% |
| Native string-add helper | 1.22 | 2.67% |
| `LStr::concat2` | 0.14 | 0.31% |

This is a materially different workload. YouTube's dominant concat helper is
almost absent here. The sampled `get_mut` and `remove` disassemblies instead
show hot linear key-search loops. Not every `bcmp` sample can be assigned to
property searches, and incomplete stack unwinding prevents attributing these
costs to specific JavaScript functions from this experiment alone.

**Second optimization round: complete the property/element fast paths across
mutation as well as reads.** A concrete source-level candidate is the mismatch
between [`get_index`](crates/lumen/src/value.rs#L2998) and the generic
[`get_mut`](crates/lumen/src/value.rs#L3410)/[`remove`](crates/lumen/src/value.rs#L3654)
paths: non-packed dense arrays have an index-to-slot sidecar, but the latter
paths can still use `find`'s key scan. Meanwhile `should_build_index` deliberately
does not build a redundant string hash table for dense array elements. Verify
actual key kinds and object sizes before claiming this accounts for all of
Twitch's scans. The measured fact is the scanning cost; this mechanism is a
source-supported candidate for its cause.

The work should cover direct indexed hits and misses, descriptor-bearing
elements, sparse/dictionary transitions, and mutation/deletion costs together.
Do not apply a blanket cache-threshold change or assume every object is plain.
Protect holes, inherited indexed accessors, proxies, property descriptors,
enumeration order, and reference/shape invalidation with focused tests.

## Priority and what this says about the nursery

1. **Engine-wide string concatenation**, starting with the directly sampled
   redundant ASCII scans and tier-dependent buffer reuse. This is the clearest
   demonstrated large avoidable cost, including during a long startup checkpoint.
2. **Property/element mutation and lookup**, with Twitch as an independent real
   workload and key/size histograms to distinguish dense-element misses from
   mutation-heavy dictionaries and small-object scans.
3. **Collector work and pause distribution**, supported by an identifiable
   roughly 13% exclusive CPU subset on both sites. Measure collection causes,
   live objects/edges scanned, reclaimed fraction, and individual pauses before
   choosing the replacement policy. Whole-heap work on a mostly retained graph
   is the relevant architectural concern; merely allocating objects in a nursery
   does not by itself eliminate that work or establish a throughput breakthrough.

Native coverage, call overhead, and specialization remain important, but these
profiles do not justify calling all inclusive VM time “dispatch overhead” or
promising that turning on more compilation will recover it. Existing native
string optimizations missing from other paths are a particularly concrete
example of performance not yet shared across the engine.

No old/new binary A/B was performed, so these measurements do **not** establish
whether the recently enabled shared object layouts improved or regressed either
site. Each proposed round needs repeated release measurements of the same
milestones and actions, plus the existing conformance and replay gates. These
priorities are starting points toward V8 parity, not evidence that three fixes
will achieve it.

## Standards constraints for the string work

The local web-standards skill was used to consult ECMA-262, recorded checkout
`e28783d5fc9dc12b3de905961e2c71410b38a202`, fetched 2026-09-06. This is the
ECMAScript 2027 **Editor's Draft**, not a claim of a published edition.

- [The String Type](https://tc39.es/ecma262/#sec-ecmascript-language-types-string-type),
  [local source](/big/web-standards/repositories/tc39/ecma262/spec.html:1231): preserve
  the ordered UTF-16 code-unit sequence, including lone surrogates and boundaries
  where a leading and trailing surrogate meet. Do not introduce normalization.
- [ApplyStringOrNumericBinaryOperator](https://tc39.es/ecma262/#sec-applystringornumericbinaryoperator),
  [local source](/big/web-standards/repositories/tc39/ecma262/spec.html:21341), and
  its `ToPrimitive`/`ToString` dependencies at 5078 and 5704: preserve left/right
  evaluation and conversion order, observable
  conversion hooks, exceptions, and numeric/BigInt behavior.

A changed physical representation is an implementation choice, not permission
to alter those semantics. Validation must cover aliasing, coercion reentrancy,
Unicode/surrogate boundaries, all execution tiers, and native header assumptions,
followed by Test262 and repeated release real-site observations. No conformance
run is claimed here: this pass changed no engine behavior.

## Artifacts and remaining measurement work

Local raw evidence is under
[`benchmark-results/real-sites-20260906`](benchmark-results/real-sites-20260906).
It includes the external runner and summarizer, binary/source metadata, raw
logs, timestamped events, `/proc` observations, CPU experiments, function and
disassembly reports, summaries, and the YouTube render snapshots. These ignored
artifacts may contain anonymous-session URLs and page content; they should not
be uploaded wholesale as a public benchmark corpus.

Run labels: `youtube-profile-01`, `youtube-sample-01`, `youtube-clean-01`, and
`twitch-sample-01`. The runner refuses to overwrite a run directory. Its browser
path intentionally points at the isolated release artifact. Use the bounded
systemd invocation when repeating long runs, choosing a fresh unit/run label:

```sh
cd /big/Code/Lumen
systemd-run --user --wait --pipe --collect --unit=lumen-youtube-sample-02 \
  -p MemoryMax=12G -p MemorySwapMax=0 -p OOMPolicy=kill \
  -p RuntimeMaxSec=420 -p LimitCORE=0 --working-directory=/big/Code/Lumen \
  /usr/bin/node benchmark-results/real-sites-20260906/run.mjs \
  youtube-sample-02 sample https://www.youtube.com/ 300
node benchmark-results/real-sites-20260906/summarize.mjs youtube-sample-02
```

Use `clean` for the uninstrumented lane and `profile` for the heavier sequence
trace. The original isolated build used `CARGO_PROFILE_RELEASE_STRIP=none cargo
build --offline --locked --release -j2 --bin trust-headless --target-dir
target/site-profile-20260906` from TRust, also in a bounded systemd unit. Its
cold dependency build plus release link took 13m26s; that build time is excluded
from every site measurement.

Next measurement work must add frontend-visible milestone timestamps and actual
interactions (search, scroll, opening a result/channel, playback controls), then
extend the matrix to Instagram and other architectures/workloads. No Instagram,
signed-in flow, input-to-paint latency, video playback, or V8 parity result is
claimed by these homepage observations.
