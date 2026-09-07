# OpenAI article: release CPU investigation, 2026-09-07

Target: `https://openai.com/index/research-acceleration-view-inside-openai/`.

## Finding

The first complete native profile points to **repeated browser-side style and
SVG preparation work**, not final desktop rasterization, as the first large
optimization opportunity. Do not describe the entire page actor as JavaScript:
it also executes DOM, cascade, layout, CSSOM measurement, and SVG preparation.

In a fixed four-minute load-and-scroll workload the baseline release used
155.43 CPU-seconds without instrumentation. The sampled repeat used 160.13
CPU-seconds; 157.04 of those were on the resident page actor. Native instruction
sampling accounted for 159.88 CPU-seconds. This is **CPU consumption during a
workload**, not a four-minute load time or a time-to-interactive measurement.

### Baseline CPU profile

Exclusive samples are the appropriate numbers for this table; inclusive
stacks overlap and must not be added.

| Hotspot | Exclusive CPU-seconds | Share of sampled CPU |
| --- | ---: | ---: |
| SVG/image data-URL decoding | 25.91 | 16.21% |
| Class-list membership loop inside compound selector matching | 15.22 | 9.52% |
| Other instructions in `Dom::matches_compound` | 8.38 | 5.24% |
| SipHash writes, across callers | 6.20 | 3.88% |
| XML tokenizer element parsing | 4.75 | 2.97% |
| Allocator zeroing/allocation hot function | 4.34 | 2.71% |
| Byte comparison, across callers | 3.89 | 2.43% |
| `Dom::computed_value` | 3.56 | 2.23% |
| Bytecode dispatch function itself | 3.14 | 1.96% |

Named DOM/cascade functions together account for 46.15 CPU-seconds. Named Lumen
functions account for roughly 25.75 CPU-seconds including dispatch, GC, and
parsing/compilation. Neither is an exhaustive subsystem total: shared allocator,
hashing, XML, and string functions have multiple callers, and native sampling
does not completely unwind JIT call stacks. Only 1.00 CPU-second of exclusive
instruction samples was unknown; the much larger unknown *inclusive* total is
not evidence of unknown instructions consuming that much CPU.

The desktop drawing/event thread used only about 1–2 CPU-seconds. The private
compositor made the normal Auto renderer fall back to Vello CPU, so a slow GPU
path cannot explain this particular profile. This does not establish GPU-path
performance on the user's visible desktop.

### Baseline workload phases

These numbers come from timestamped `/proc` counters, not profiler sample
ordinals. Interval endpoints are the last observation at or before each bound.

| Requested wall interval | Activity | Clean process CPU-seconds |
| --- | --- | ---: |
| 0–40 s | Startup and initial viewport | 15.60 |
| 40–75 s | First charts brought into view | 14.36 |
| 75–200 s | Progressive chart scrolling | 108.94 |
| 200–220 s | End/footer | 16.51 |
| 220–240 s | Settled tail | 0.02 |

The bulk of the cost is triggered as lazy chart sections become active, rather
than an engine spinning continuously on the settled page.

## First optimization: avoid irrelevant SVG sizing metadata

`TRust/src/layout2/replaced.rs::size` resolves specified width and height, then
unconditionally calls `img::svg_url_ratio_only`. For a data-URL SVG this decodes
the entire URL and XML-parses the document merely to inspect root dimensions
and `viewBox`. Layout and intrinsic-measurement probes repeat that work on the
large generated chart SVGs.

For scale, the previous final-acceptance render dump contains 13 chart SVGs
totalling 2,236,775 serialized bytes (the largest is 522,903 bytes). That dump
includes serialized presentation styles; it is not a network-transfer count.

When **both resolved dimensions are definite**, neither the initial box-size
branch nor the min/max branch can use that ratio. The candidate skips that URL
metadata lookup in exactly that case. It does not change the auto-axis ratio
chain, introduce a cache, change script scheduling, skip charts, or alter the
natural dimensions passed independently to `object-fit`.

Tests cover CSS lengths, HTML dimension hints, definite percentages, an
indefinite percentage, author `auto` overriding an HTML hint, each auto-axis
combination, min/max constraints, and `object-fit: contain`. A test-only
thread-local counter checks that definite sizing performs no SVG URL metadata
read and auto sizing still performs one. There is no release counter overhead.

### Clean release A/B result

| Requested wall interval | Baseline CPU-s | Candidate CPU-s |
| --- | ---: | ---: |
| 0–40 s: initial view | 15.60 | 15.61 |
| 40–75 s: first charts | 14.36 | 13.97 |
| 75–200 s: progressive charts | 108.94 | 84.28 |
| 200–220 s: end/footer | 16.51 | 13.32 |
| 220–240 s: settled | 0.02 | 0.02 |
| Whole observation | **155.43** | **127.20** |

This first uninstrumented pair shows **28.23 fewer CPU-seconds (18.16%)** in
the whole workload and **22.64% less CPU** in the main chart-scrolling phase.
The initial view is unchanged, so this must not be advertised as an 18% faster
initial page load. Actor CPU fell from 152.61 to 124.41 seconds. Peak process
RSS was 1361.85 MiB for the candidate versus 1373.50 MiB for the baseline;
that small memory difference is not a demonstrated memory optimization.

The first-chart and footer screenshots preserve the expected content and
layout. The median/90th-percentile charts are visible, as are the related-card
images, rounded corners, header controls, and multi-column footer. The middle
card remains an unsupported video without a poster, as before this change.

The detailed candidate repeat (`svg-sized-trace-01`) used 128.46 process
CPU-seconds, close to the clean candidate's 127.20. It includes native sampling,
task/layout tracing and cascade counters, so the **clean** pair remains the
performance comparison; this is not a statistical confidence interval.

The new native profile confirms the intended mechanism:

| Exclusive sampled instructions | Baseline | Candidate diagnostic repeat |
| --- | ---: | ---: |
| Data-URL decoder | 25.91 s | **0.06 s** |
| All named XML parser functions | 9.00 s | **0.85 s** |
| Class membership loop | 15.22 s | **17.01 s** |
| All named DOM/cascade functions | 46.15 s | **49.82 s** |

Decoding and parsing cost disappeared from the sizing path as intended. CSS
work remains large; percentages alone would conceal that it did not improve.
The candidate's added style timing instrumentation also means these rows are
not all equally clean A/B comparisons. No JS probe or DOM dump was enabled in
this diagnostic profile.

Separate release validation (`svg-sized-validate-01`) then confirmed all **13
chart SVGs populated with paths and none hidden**, all with positive geometry.
The first chart remains 272×383.255 CSS pixels at x=182. It used the existing
acceptance probe and is excluded from performance comparisons. It reported
the three previously observed external-response JSON rejections and the
expected unsupported inline-video `play()` rejection at the related card;
there were no probe exceptions or native panics. Video support was not changed.

Focused replaced-sizing tests: 6 passed. Full browser library suite: 1,114
passed, 0 failed, 19 ignored, 2 existing Wasm accumulation tests excluded;
62.09 seconds in the PTY test harness. The initial new object-fit assertion
used an inexact floating-point scale; the final fixture uses an exactly
representable scale and additionally distinguishes fitting's natural ratio
from the URL ratio. Repository-wide formatting check still reports pre-existing
formatting differences; unrelated files were not mechanically reformatted.

All three release executables built successfully with the existing release
ThinLTO/codegen configuration and symbols retained for sampling:

```sh
CARGO_PROFILE_RELEASE_STRIP=none /usr/bin/cargo build --offline --locked \
  --release -j 3 --bin trust-desktop --bin trust --bin trust-headless
```

Build wall time: 7m49s; service CPU: 19m51s; memory peak 6.7 GiB, no swap.
Candidate SHA-256 values:

- `trust-desktop`: `d3ab85554466809f61f37b7e00d20491f175d139b387c7e52478f05d1a7b88dc`
- `trust`: `3cbf06bba252c698acc1bd37a4f23c122cff961ee7e90b7078548debbdac65d8`
- `trust-headless`: `696cbd805d897d3225472beb7b4cd26a25aafa55b28e9a2f93dfd59775e87d0d`

## Next high-value targets

1. **Style matching and invalidation:** reduce the number of candidate rules
   and preserve unaffected matches/cascade results across DOM mutations.
   `matched_rules` uses a whole-DOM epoch, while `RuleBuckets` only indexes
   direct id/class/type keys in the final compound. Investigate conservative
   key extraction from positive `:is()`/`:where()` alternatives and safe
   ancestor rejection filters before full matching. Measure actual bucket
   composition before selecting that implementation; the current trace does
   not identify exactly which rules populate the universal bucket.
2. **Class membership within surviving candidates:** `matches_compound`
   repeatedly splits the same long `class` attribute for rules and ancestor
   matches. Tokenized or interned membership can remove repeated string
   scanning. Any cache must be bounded by the document, account for retained
   memory, and invalidate on class mutation; escaped names and selector
   matching semantics must survive. This can improve the hot loop but is not
   a substitute for eliminating redundant restyles.
3. **SVG preparation where intrinsic metadata really is needed:** reuse parsed
   immutable data-URL metadata, including negative results, with bounded
   storage. Investigate repeated SVG serialization/XML work separately from
   actual raster decoding. Avoid an unbounded global URL cache or hash-only
   identity that can return wrong metadata.
4. **Geometry-cache reuse:** use the existing attributed dirty-node information
   to identify safe reuse boundaries, with conservative fallback for structural
   selectors, `:has()`, inherited changes, shadow/slot dependencies, and
   container queries. Do not bypass required synchronous geometry or observer
   delivery. The count below demonstrates repeated work, not proof that every
   invalidation is unnecessary.

### Geometry amplification measured in the diagnostic repeat

The existing `DIAGGEOM` instrumentation recorded **91 geometry-cache rebuilds**
totalling **56.619 seconds of wall time** inside the rebuild implementation:

| Trigger | Rebuilds | Summed wall time | Slowest single rebuild |
| --- | ---: | ---: | ---: |
| Bounding rectangle | 70 | 34.960 s | 1.265 s |
| Computed style with container queries | 7 | 10.919 s | **2.668 s** |
| Scrolling area | 14 | 10.740 s | 1.481 s |

The document grew to 5,131 arena nodes. Cascade-counter windows logged
222,003 element match-cache builds and **172,084,297 candidate-rule tests**.
Those counters accumulate until a diagnostic drain and include intervening
style reads outside the geometry call; they must not be attributed wholly to
the individual rebuild that prints them. Their times overlap geometry and
other style work, so do not add the 33.196 seconds of logged matching time to
the geometry wall total. No container-query 64-pass non-convergence warning
occurred; there is no evidence that the iteration cap itself is being hit.

This points to repeated work in shared browser infrastructure, including long
synchronous stalls visible to page scripts. The next optimization should
address both the number of restyles/geometry rebuilds and the cost of matching
each surviving candidate rule.

These precede further speculative GC/JIT tuning for this page. That is a
page-specific priority supported by this profile, not a conclusion that Lumen
is already fast enough for the general web.

## LibreWolf comparison

LibreWolf 153.0.4-1 ran headlessly with a fresh private default profile, BiDi
wheel/End inputs on the same schedule, a 960×1024 viewport with 948 CSS pixels
of document width, and no concurrent benchmark or compilation. Its final DOM
snapshot contained all 13 populated, non-hidden chart SVGs. First-chart
geometry was 272×383.27 CSS pixels at x=182, matching the TRust layout width.

The corrected `/proc` sampler follows children of **every browser thread**,
not only children spawned by the main thread, and retains final observed CPU
counters for exited processes. It observed up to 12 concurrent processes.

| Workload phase | TRust candidate CPU-s | LibreWolf process-tree CPU-s |
| --- | ---: | ---: |
| Initial view, 0–40 s | 15.61 | 31.96 |
| First charts, 40–75 s | 13.97 | 2.43 |
| Progressive charts, 75–200 s | **84.28** | **12.80** |
| End/footer, 200–220 s | 13.32 | 18.41 |
| Settled tail, 220–240 s | 0.02 | 16.72 |
| Whole observation | 127.20 | 82.48 |

The main chart-scrolling phase still costs TRust approximately **6.6×** as
much CPU as LibreWolf's whole process tree in this run. The 18% improvement is
useful, but does not put this workload close to browser-performance parity.

Do not present the whole-observation ratio as an engine ratio. LibreWolf's
default WebExtensions process used 14.90 CPU-seconds, much of it at fresh-profile
startup. Its RDD process used 16.61 CPU-seconds, almost all after reaching the
video card; TRust does not decode/play that video. Ordinary web-content
processes together used 19.69 CPU-seconds over the whole observation, versus
124.41 for TRust's page actor, but these are not identical subsystem boundaries
either: Firefox splits networking, decoding, compositing, and other work into
separate processes. Different default blocking, theme preference, rendering
backend, and live-network timing also remain. The browser comparison exposes
where further investigation is warranted; it is not a JavaScript microbenchmark
or a V8 comparison.

`librewolf-clean-02` is the valid process-tree run. Its service-wide CPU total
was 85.00 seconds including the harness and shutdown, independently consistent
with the process observations. `librewolf-clean-01` verified all 13 charts and
had an independent service-wide total of 85.48 seconds, but its original `/proc`
sampler missed child processes; its 31.35-second main-process number is invalid
as a whole-browser total and is explicitly excluded. Aggregate per-process RSS
double-counts shared pages and is not used for a memory-efficiency comparison.

## Standards used

The web-standards skill supplied local official source snapshots fetched
2026-09-06. These are editor-source snapshots, not a claim of a fresh upstream
verification on 2026-09-07.

- CSSWG commit `81c27f68690138345b2b3b6af8ccc42dad3dca1d`:
  [CSS Images default sizing](https://drafts.csswg.org/css-images-3/#default-sizing),
  local `css-images-3/Overview.bs:1328`; definite width and height determine the
  concrete size, while an absent dimension can require the natural ratio.
- Same snapshot:
  [CSS2 min/max width](https://drafts.csswg.org/css2/#min-max-widths), local
  `css2/Overview.bs:8958`, and
  [min/max height](https://drafts.csswg.org/css2/#min-max-heights), local
  `css2/Overview.bs:9459`; the special ratio-preserving table is for auto sizes.
- Same snapshot:
  [CSS Images object-fit](https://drafts.csswg.org/css-images-3/#the-object-fit),
  local `css-images-3/Overview.bs:1493`; fitting content into the established box
  still respects natural dimensions where required.
- SVGWG commit `c403ca46ad045ebdaeda148bae69f814fb744db7`:
  [SVG intrinsic sizing](https://svgwg.org/svg2-draft/coords.html#SizingSVGInCSS),
  local `master/coords.html:1270`; natural dimensions and `viewBox` ratio remain
  distinct from the specified CSS box.
- CSSWG snapshot above:
  [Class selectors](https://drafts.csswg.org/selectors-4/#class-html), local
  `selectors-4/Overview.bs:2345`, consulted for the next matching optimization.

CSSWG paths are under `/big/web-standards/repositories/w3c/csswg-drafts/`;
SVGWG paths are under `/big/web-standards/repositories/w3c/svgwg/`.

## Reproduction and limitations

Artifact directory:
`benchmark-results/openai-performance-20260907-BsHVac/` (ignored; includes
private disposable browser profiles, not suitable for committing wholesale).

- Frozen baseline desktop release SHA-256:
  `b2d8503b3e63ab48094f3c5a93dc805417f691f20f678217e1b7eeb9a58675e7`.
- Original baseline source was the final rendering acceptance tree described
  in `OPENAI_FULL_PAGE_2026-09-07.md`.
- `profile.mjs`: isolated labwc/pixman desktop, 948×1024 content viewport,
  fresh configuration/cache/profile, fixed wheel/End schedule and screenshots.
- `baseline-clean-01`: no tracing, sampler, DOM dump, or injected JS probe.
- `baseline-sample-02`: same workload with gprofng 10 ms native CPU samples.
  Termination leaves an "experiment was not closed" note; aggregate samples
  agree closely with independent process counters. Do not use the historical
  `cpu-startup.txt`/`cpu-chart-scroll.txt` ordinal-filter reports as wall phases.
- `baseline-sample-01` failed before browser launch due to a private-input
  safety check and is excluded.
- No concurrent build or second benchmark browser during measurements. Host is
  the user's ordinary AArch64 desktop, with heterogeneous Cortex-X925/A725
  cores and possible unrelated host activity, not a reserved benchmark machine.
- Own browser units capped at 12 GiB with no swap. Peak baseline process RSS
  was 1373.50 MiB clean and 1388.68 MiB sampled. No user profile was touched.
- Actual scrolling screenshots verify chart rendering. The same input
  schedule is not a promise of identical pixel scroll positions across browser
  implementations, nor a V8/SpiderMonkey-versus-Lumen throughput benchmark.

## Publishing status

Lumen rendering/parser/closure and Photopea engine fixes and reports were
committed and pushed to tracked `private/main` through `e1e1129`.
Previously committed TRust progress was pushed to `origin/main` at `03fb97c`.
The isolated SVG sizing optimization and its tests are committed separately as
TRust `523b3b7` (`perf: skip SVG intrinsic metadata for definite image
dimensions`). Its staged files were also syntax/format checked independently
of the remaining working-tree changes. It is pushed to `origin/main`.
The pending TRust tree also includes earlier platform work and Boa removal;
publishing that mixed working-tree checkpoint awaits the user's scope choice.
The measured release includes that pending rendering tree as well as the
isolated optimization; checking out only the remote TRust commit does not yet
reproduce the full rendering checkpoint. No installed executable has been
replaced. All private test/browser/compositor units have stopped.
