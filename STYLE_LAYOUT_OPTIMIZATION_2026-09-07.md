# Retained style/layout project — 2026-09-07

## Result

The first foundation round is implemented and enabled in normal TRust builds.
Two clean release runs per build on the OpenAI article average **132.70 →
79.56 process CPU-seconds: 40.05% less CPU** in the same 240 s load-and-scroll
workload. The main chart interval averages **43.51% less CPU**. These are
CPU-work measurements, not page-load-time or V8-parity claims.

All original screenshot checkpoints and the added settled-top checkpoint are
byte-identical to the frozen baseline. The browser suite passed **1,132 tests**
with no failures, 19 ignored and two pre-existing Wasm tests filtered. Final
release acceptance independently verified all 13 charts, with matching
geometry and SVG content. Known external JSON/video errors remain unchanged.

The architectural changes are one shared completed layout transaction,
dependency-aware pure selector retention, safe positive-functional rule
indexing, and attribute-owned long-class membership. Synchronous geometry,
container-query reevaluation and full fallback paths remain intact. General
per-element computed-style retention and incremental flow are following rounds,
not claimed complete here.

YouTube's two-run result is essentially unchanged (mean CPU **+1.33%**, with
the pairwise direction reversing), with byte-identical captures. No YouTube
performance improvement is claimed. All three release binaries built; no
installed executable was replaced. Source changes remain in the working tree.

## Objective and baseline

Reduce repeated browser work while preserving synchronous CSSOM results,
rendering, interactions, accessibility and observer ordering. This is shared
TRust browser infrastructure, not a site-specific Lumen workaround.

The OpenAI article release profile in `OPENAI_PERFORMANCE_2026-09-07.md`
recorded 91 CSSOM geometry-cache rebuilds and 56.619 s summed rebuild wall
time. The diagnostic cascade windows recorded 172,084,297 candidate-rule
tests. Those windows overlap other work; their times are not additive.
Neither number establishes how much work can safely be eliminated.

Baseline release desktop SHA-256:
`d3ab85554466809f61f37b7e00d20491f175d139b387c7e52478f05d1a7b88dc`.
This includes the preceding SVG intrinsic-sizing optimization.

Artifacts and pre-edit source copies are under the ignored directory
`benchmark-results/retained-layout-20260907-83NuER/`. The browser worktree
already contained platform/rendering changes and the intentional Boa removal;
this project preserves those changes. A Git HEAD alone does not identify the
baseline working tree.

## Correctness contract

- Geometry reads complete all required style/layout work before returning.
- Paint, CSSOM and observers consume the same canonical CSS-pixel fragments.
- No terminal-cell or device-pixel quantization enters the layout cache.
- A cache hit is justified by its inputs, never by a timer or an assumed
  settling period. Required observer/event ordering is unchanged.
- Resource dimensions, viewport, activation metadata, scrolling and hit-test
  metadata must have explicit invalidation paths.
- Dependency analysis errs toward extra work. Unknown/nonlocal dependencies
  keep full recomputation until a narrower rule is proven.
- Cache storage is bounded by the resident document and retained presentation
  products. Shared allocations are included in the host memory inventory.
- Cold recomputation is a regression oracle, not by itself a conformance
  oracle: standards tests and reference-browser checks remain necessary.

The current downloadable-font path installs the document's fonts before
spawning its page actor (`http::install_stylesheet_fonts`); the embedding API
also explicitly requires a stable font set while surfaces are live. Future
asynchronous face installation must add its font-catalog revision to layout
freshness and schedule rendering. It must not rely solely on the text shaper's
existing font-epoch refresh: retained fragments can bypass shaping entirely.

## First implementation round

### One layout transaction

`TRust/src/layout2/session.rs` owns the common transaction. Container queries
interleave style and layout as before, but the converged pass is consumed
directly. It is no longer discarded before another box-tree/flow pass.

Completed fragments can be retained in an immutable `Arc` shared between the
page actor and the graphical/terminal presentation. Unresolved borrowed
out-of-flow placeholders retain the complete original fallback, not omitted
content. CSSOM-only calls need not construct paint or a terminal adapter.

The resident actor's geometry cache now supplies rendering as well as CSSOM.
The first hit test after a geometry read builds its paint-order data from the
retained fragments. Activation metadata is an independent geometry input:
it can affect generated fallback content and retained click actions. Scroll
positions and paint markers have a separate revision and refresh hit-test
paint without invalidating flow layout.

### Dependency-aware pure selector cache

`TRust/src/dom/invalidation.rs` compiles attribute dependencies from parsed
selectors. The pure selector cache is independent of the per-DOM-revision
cascade and container-query applicability cache.

- Unreferenced attributes do not invalidate pure selector matches.
- Own-element dependencies invalidate the subject.
- Ancestor and sibling dependencies invalidate a conservative reachable
  subtree/sibling forest, including nested logical selectors.
- Attributes in `:has()` and filtered `:nth-child()` conditions, nonlocal
  HTML state, and relevant cross-shadow changes retain a broad fallback.
- Slot assignment/name changes are implicit cross-tree dependencies even
  without a literal attribute selector, and retain a broad fallback too.
- Structure, stylesheet and unattributed changes retain broad invalidation.
- Container-query conditions still reevaluate after style/layout changes;
  retaining selector matches must never retain an obsolete query result.

Computed values and cascade winners still use the conservative DOM revision.
This first round does not yet implement general per-element computed-style
retention or constraint-keyed partial flow layout.

### Invalidation responsibilities

| Input change | Pure selector matches | Layout/presentation consequence |
| --- | --- | --- |
| Attribute referenced by selectors | Invalidate the conservatively reachable subjects | Cascade and geometry still advance with the DOM revision |
| Attribute not referenced by selectors | Retain | Cascade/geometry still recompute; presentation attributes may matter |
| Structure or unknown/nonlocal dependency | Broad invalidation | Complete current transaction required |
| Container-query size environment | Retain | Reevaluate cascade/conditions and consume the settled layout pass |
| Intrinsic image dimensions, viewport or DPR | Existing explicit invalidation paths | Rebuild geometry; media-query changes also invalidate style |
| Render activation metadata | Retain selector results | Separate presentation revision invalidates retained layout |
| Scroll position or hit-test markers | Retain | Rebuild hit-test paint from existing fragments, without flow |
| Unretainable borrowed fragment | No semantic change | Keep the complete borrowed-layout fallback |

The class-token memo has a still narrower owner: the element's actual class
attribute, regardless of the selector/cascade/layout invalidations above.

## Validation and staged measurements

The first combined transaction/selector implementation passed 1,124 browser
tests, with no failures, 19 ignored and the same two pre-existing Wasm
accumulation tests excluded (63.02 s, 5.2 GiB peak, no swap). Seven expanded
selector-invalidation tests now pass against full rule scans, covering
relational/positional/state changes, slot reassignment, stylesheet/viewport/
structure changes, inherited variables and container-query updates. The six
focused retained-layout tests also pass, including the existing embedded
document test.

The completed first-stage suite passed **1,126 tests, zero failures**, with the same
19 ignored and two filtered tests (62.55 s, 5.0 GiB peak, no swap). New source
modules pass rustfmt; only owned regions were formatted in the already-dirty
large browser files. `git diff --check` passes. The exact project-only source
patch and candidate source hashes are saved alongside the baseline artifacts.
`cargo fmt --all -- --check` is not clean: it reports repository-wide
formatting differences, including pre-existing Lumen and browser changes.
No unrelated whole-file formatting was applied. The saved project-only patch
also passes `git apply --check --reverse` against the candidate tree.

### Clean release comparison

The first uninstrumented candidate consumed **92.07 process CPU-seconds**
versus **131.02** for the fresh frozen baseline: **38.95 CPU-s saved, 29.73%
less work** during the same fixed 240 s load-and-scroll observation. This is
not a measurement of page-load completion or time to interactive.

| Workload phase | Baseline CPU-s | Candidate CPU-s |
| --- | ---: | ---: |
| 0–40 s: initial view | 15.72 | 13.87 |
| 40–75 s: first charts | 13.64 | 10.90 |
| 75–200 s: progressive charts | 88.06 | 60.49 |
| 200–220 s: end/footer | 13.58 | 6.79 |
| 220–240 s: settled tail | 0.02 | 0.02 |
| Whole observation | **131.02** | **92.07** |

Actor CPU fell from 127.86 to 88.99 s. Main chart scrolling used **31.31%
less CPU**. The candidate's peak process RSS was 1,394.07 MiB, **31.36 MiB
higher** than baseline; this is a throughput/retention tradeoff, not a memory
optimization claim.

All three scheduled PNGs are **byte-identical** between the clean baseline
and candidate: top, first charts, and related cards/footer. Their SHA-256s:

- Top: `2b5384b5a6a7ec5721fc5a83d7f64f6e8556426ad6742afc0e30dee62d7cd5c6`
- First charts: `6b59b95ea512556762b1e7ae7008b71f755661362a804c1afa815a6aa75624ce`
- Cards/footer: `84330ebc95a80fc900365b4162c203cf09326cace9a76947fcda543796edfbe6`

These exact captures are a useful regression check, not proof that every
uncaptured state or every page is conformant. Subsequent acceptance and repeat
measurements are recorded below.

### Diagnostic repeat and evidence-led follow-up

The first candidate's instrumented repeat used 92.96 process CPU-s, close to
the clean result. Its CSSOM rebuilds fell from the previous diagnostic's 91
to 61, with summed rebuild wall time falling from 56.619 to 33.619 s. The new
shared path separately logs 34 render transactions (6.226 s); the old trace
did not log those as geometry rebuilds, so total counts are not directly
comparable. Across all new transactions: 100 layout passes, five query
environment updates, no query-limit warnings and no native panics.

Pure selector reuse occurred 15,746 times in the diagnostic drain windows,
but 159,896,767 candidate tests remained. Structural and nonlocal selector
changes still take broad invalidation. Named DOM/cascade functions consumed
38.25 exclusive sampled CPU-s; the class-membership loop and the other
instructions of `matches_compound` alone consumed 15.60 + 8.38 s. The layout
foundation helped, but this is still a strong remaining optimization target.

The follow-up in `TRust/src/dom/rule_index.rs` extracts necessary subject keys
from positive `:is()`/`:where()` arguments. Every alternative must supply a
safe key; uncertain alternatives, negation and unsupported forms retain the
universal bucket. Original selectors, specificity and full matching remain
authoritative. Alternative keys are deduplicated before matching. Key fanout,
recursion depth and analysis visits are bounded, with universal fallback when
limits are reached. This shares the existing bucket storage/memory inventory.

All three new rule-index tests pass. They compare indexed matching to full
scans, test specificity and mutable selector inputs, and exercise bounded
analysis fallbacks. In a 500-rule functional-subject fixture, only the one
possible rule reaches the matcher; an unrelated element receives none.
The combined suite passed **1,129 tests, zero failures**, with 19 ignored and
the same two filtered Wasm tests (62.71 s, 5.1 GiB peak, no swap). The indexed
release built all three executables successfully (7m54s wall, 20m01s service
CPU, 6.1 GiB peak, no swap). The second
exact source checkpoint is `indexed-implementation.patch` /
`indexed-source.sha256`; all hashes remained unchanged through the build.

The indexed clean run used **88.33 process CPU-s**, versus 92.07 for the
first candidate and 131.02 for baseline: **32.58% less CPU in total**. Its
additional 4.06% gain is small enough to warrant a repeat before treating
that component alone as established across live runs. Actor CPU was 85.40 s,
peak RSS 1,399.30 MiB. First-chart and footer screenshots remained
byte-identical to baseline. The 15 s top capture showed later hydration
(consent and chapter navigation), so a later settled capture was added to
the final comparison; a timing difference is not assumed to prove parity.

The independent indexed diagnostic used 88.59 CPU-s and recorded
112,084,706 candidate tests, **29.90% fewer** than the first candidate's
159,896,767. Matching time in the diagnostic drain windows fell from
30.941 to 27.283 s. Geometry recorded 95 transactions, 100 passes and five
query updates: 62 CSSOM transactions / 32.211 s, and 33 render transactions /
5.885 s. No query-limit warning or native panic occurred. Native sampling
still assigned **14.41 exclusive CPU-s (16.31%)** to the repeated class-string
splitting loop: the next measured target, not speculation about flow cost.

The separate indexed acceptance run found **all 13 charts**, each visible,
with positive geometry, one marks group and nonempty SVG paths. The first
chart remained 272 × 383.255 CSS px. Footer links retained 13 px type and the
expected dark-theme foreground. The same three external JSON rejections and
unsupported inline-video playback error remain; this is not a zero-error or
video-support claim. Acceptance probes are excluded from throughput results.

### Attribute-owned class membership

`TRust/src/dom/class_tokens.rs` retains parsed membership for long class
attributes (at least 64 bytes). This memo survives selector/style/layout
invalidations; only actual class writes/removals clear the element's entry.
Short lists retain allocation-free streaming checks. Both paths use the same
exact token identity and ASCII-whitespace semantics; selector parsing,
specificity and the authoritative full matcher are unchanged. This does not
implement the parser's currently unrecorded quirks-mode matching distinction.

Storage is bounded by the document arena, with hash capacity and token bytes
included in the host memory inventory. Tests cover whitespace edge cases,
duplicates, Unicode, escaped class selectors, misses, idempotent writes,
replacement/removal, and retention across unrelated style/structure changes.
The final combined source passed **1,132 tests, zero failures**, with 19
ignored and the same two pre-existing Wasm tests filtered (62.96 s, 5.0 GiB
peak, no swap). Focused class tests passed first. The exact eight-file project
patch and source/Cargo hashes are saved as `final-implementation.patch` and
`final-source.sha256`; the patch passes reverse-application checking against
the tested tree. All three final release executables built successfully in
8m06s (20m25s service CPU, 5.7 GiB peak, no swap), with source hashes unchanged.
The first final clean release run used **77.55 process CPU-s**, **40.81% less
than baseline** (131.02) and 12.20% less than the indexed candidate (88.33).
Actor CPU was 74.31 s. The main chart interval used 48.47 CPU-s versus 88.06
for baseline (**44.96% less CPU**). Initial / first charts / main charts /
footer / tail CPU-s were 14.00 / 9.62 / 48.47 / 5.44 / 0.02. Peak RSS was
1,346.77 MiB in this run; retention is not being presented as a memory
improvement, since allocation/GC timing and prior candidates varied.

All three original screenshot checkpoints remained **byte-identical** to
baseline, including the early top capture. The added 35 s settled top also
matched the indexed acceptance capture exactly. The fresh baseline and final
repeat with that same added checkpoint also match exactly: the earlier indexed
15 s difference was an intermediate hydration/capture-timing difference.

The completed clean repeat used **134.38 baseline / 81.57 final CPU-s**
(39.30% less CPU), confirming the first pair's 40.81% reduction. The full
clean results, without diagnostic/probe runs mixed into the mean:

| Metric | Baseline run 1 | Baseline run 2 | Final run 1 | Final run 2 |
| --- | ---: | ---: | ---: | ---: |
| Process CPU-s | 131.02 | 134.38 | 77.55 | 81.57 |
| Actor CPU-s | 127.86 | 130.49 | 74.31 | 78.29 |
| Main chart interval CPU-s | 88.06 | 89.61 | 48.47 | 51.90 |
| Peak process RSS, MiB | 1,362.71 | 1,386.31 | 1,346.77 | 1,408.74 |

Mean total CPU is **132.70 / 79.56 s (40.05% less)**; mean main-chart CPU is
**88.835 / 50.185 s (43.51% less)**. Mean peak RSS is 1,374.51 / 1,377.76 MiB,
but the per-run variation is much larger than that mean difference. Caches
retain additional owned storage; neither a memory saving nor a precise memory
overhead is established by these RSS samples. All four image checkpoints
match byte-for-byte in the final repeat: early top, settled top, first charts,
and related cards/footer.

The final diagnostic independently used **77.06 process CPU-s** (76.44 s
sampled). With exactly the same **112,084,706 candidates** as the indexed
diagnostic, recorded matching time fell from **27.283 to 15.890 s**. Named
DOM/cascade exclusive samples fell from **33.47 to 20.92 CPU-s**. The former
14.41 s class-splitting loop no longer appears as a standalone hotspot;
remaining checks are inlined into matching and the new membership lookups.
This establishes a real reduction in matching work, not just a changed counter.

Geometry now recorded 94 transactions / 30.432 s, including **61 CSSOM
rebuilds / 24.615 s** and 33 render transactions / 5.817 s. There were 99
passes, five query updates, no query-limit warnings and no native panics.
The earlier pre-project CSSOM diagnostic was 91 rebuilds / 56.619 s: the
CSSOM-triggered rebuild wall-time sum is now **56.53% lower**. The old
diagnostic did not count render-triggered transactions, so those cannot be
included in this comparison. Counters, overlapping wall times and native
samples must not be added together.

The final release acceptance run completed with **470 probe observations**.
All **13 unique charts** were visible, with positive dimensions, exactly one
marks group each, and the same SVG path counts and dimensions as the indexed
acceptance run. The first chart remained 272 × 383.255 CSS px. Related-card
links and footer geometry/styles were retained. The same three external JSON
rejections and unsupported inline-video playback error were recorded. These
known errors are outside this optimization round; no native panic or acceptance
failure occurred. Probe-run CPU is excluded from every clean performance mean.

Final release SHA-256 values:

- Desktop: `ffb2cacf5d9619153f94f4c663a278d5565de9e0d2e54d32218119955278d125`
- Terminal: `4d1da0acdb02219258e9bee69c87455f9a6e7078e169b6470bd7e912e8407538`
- Headless: `3d51310b494a5a54d51b992bc66c8ce76e5e72602159a2a18a6ac153a23b6ad9`

The optimizations are active in normal execution, not behind opt-in diagnostic
flags. Frozen executables are used for measurements; no installed executable
has been replaced or promoted.

Final checks: source hashes still match the tested checkpoint; all three
release binary hashes still match their recorded values; the project-only
patch passes reverse-application checking; `git diff --check` and new-module
rustfmt checks pass. All private benchmark services have exited. The source
changes are uncommitted, preserving the pre-existing dirty tree and Boa
removal. The installed browser is untouched.

Indexed release SHA-256 values:

- Desktop: `ad0428e6d7f5f2da2ae05ec9506725b929067de4a98ebeb8f64695e60ee25555`
- Terminal: `4c3239ada7797cb3027742137123fc3f3f4574d1930633fde0dca01fe9c108fc`
- Headless: `ac8ca91df32fd2241f5ebc2e497f3bd2bca0df9f5cd7f247ffb57e8ac9fd1bee`

### First-stage release artifacts

All three first-stage release executables built successfully with the normal ThinLTO
configuration and symbols retained for native sampling:

```sh
CARGO_PROFILE_RELEASE_STRIP=none /usr/bin/cargo build --offline --locked \
  --release -j 3 --bin trust-desktop --bin trust --bin trust-headless
```

Build: 8m06s wall, 20m25s service CPU, 7.7 GiB peak, no swap. Source hashes
were unchanged from the tested checkpoint through build completion.

- Desktop: `63b8922747ef99125a2806329d78c56e2f32f884e446bd366c7f25a751764aa5`
- Terminal: `6cf76571d92a891dfa25391b28b3ff6b4ae490a5e9fbdbf13d8e79776e3f26bb`
- Headless: `65d7032e562bbc5f06140978d719f2d39cec4d5c08e38a5fd0cb0ebf235e1a62`

The fresh frozen-baseline repeat (`baseline-clean-01`) consumed 131.02 process
CPU-seconds (127.86 on the page actor) during the fixed 240 s observation,
with 1,362.71 MiB peak RSS. Phase CPU: 15.72 / 13.64 / 88.06 / 13.58 / 0.02 s.
The prior equivalent baseline was 127.20 CPU-s: ordinary live-network/host
variation remains, so small differences would not establish an improvement.

## Cross-site check: YouTube

Fresh private profiles, release desktop, 240 s observation, no consent choice
or playback. Captures at 15, 70, 140 and 220 s. The initial 15 s capture is
still a skeleton in both builds; by 70 s the privacy notice and SVG icons are
fully rendered. All four baseline/final PNGs are byte-identical. This observes
the user's stated visual readiness criterion, not a precise readiness time
or post-consent browsing performance.

The first pair was **85.00 baseline versus 88.28 final process CPU-s**, a
**3.86% increase**, with actor CPU 82.96 versus 85.87 s and peak RSS 1,288.91
versus 1,281.27 MiB. This is not a cross-site performance win. Significant
background actor work continues after the privacy notice looks static; visual
completion must not be confused with idle CPU.

The immediate repeat was **87.54 baseline versus 86.56 final CPU-s** (1.12%
less CPU), reversing the direction of the first pair. All screenshots again
matched byte-for-byte. Two-run means are **86.27 baseline / 87.42 final**,
**1.33% more CPU**, with overlapping ranges (85.00–87.54 / 86.56–88.28).
This small live-run sample establishes neither a reliable performance win nor
a consistent regression; it cannot rule out a small overhead. YouTube is
reported as essentially unchanged, not folded into the OpenAI improvement.
The second candidate's actor CPU was 84.11 s and peak RSS 1,245.01 MiB.

## Following rounds

1. Retain computed styles and classify actual style differences into box
   construction, flow, paint and interaction changes.
2. Add explicit layout-input/constraint keys and incremental fragment updates,
   propagating changed size, baseline and overflow to affected ancestors and
   following content. A formatting context alone is not size containment.
3. Narrow container-query invalidation to dependent conditions and units.

The remaining profile still has 20.92 exclusive named DOM/cascade CPU-s;
`matches_compound`, attribute reads, ancestor traversal, computed values and
cascade maps remain significant. Structural mutations still invalidate pure
matches broadly, and every DOM revision still invalidates computed values and
layout. These are the next architectural targets; more selector syntax
special cases are not a substitute for retaining unaffected computed styles.
Native library/allocation costs overlap those callers and require call-stack
attribution rather than assigning the entire library bucket to layout.

## Standards consulted

The web-standards skill supplied local official editor-source snapshots
fetched 2026-09-06. CSSWG commit:
`81c27f68690138345b2b3b6af8ccc42dad3dca1d`.

- [CSSOM View geometry](https://drafts.csswg.org/cssom-view-1/#dom-element-getclientrects),
  local `/big/web-standards/repositories/w3c/csswg-drafts/cssom-view-1/Overview.bs:1298`.
- [CSSOM View hit testing](https://drafts.csswg.org/cssom-view-1/#dom-document-elementsfrompoint),
  same local source at line 1104.
- [Container query style changes](https://drafts.csswg.org/css-conditional-5/#animated-containers),
  local `css-conditional-5/Overview.bs:1223` under the CSSWG checkout.
- [Selector matching](https://drafts.csswg.org/selectors-4/#match-against-element),
  local `selectors-4/Overview.bs:4760`; attribute selectors at 1995,
  descendant/child/sibling combinators at 4280–4403, logical selectors at
  1481, class token identity/whitespace at 2346–2420, relational selectors at
  1768, filtered child positions at 3915.
- [HTML update the rendering](https://html.spec.whatwg.org/multipage/webappapis.html#update-the-rendering),
  local `/big/web-standards/repositories/whatwg/html/source:123331`, commit
  `e5071a20c8569d8a3ec02ed27dd01b948773f850`.
- [Slot assignment](https://dom.spec.whatwg.org/#shadow-tree-slots), local
  `/big/web-standards/repositories/whatwg/dom/dom.bs:2472`, commit
  `a2331a45360129e8645ef7e0a04740241b6e3726`; CSS Shadow 1
  [slotted content](https://drafts.csswg.org/css-shadow-1/#slotted-pseudo),
  local `css-shadow-1/Overview.bs:522` under the CSSWG checkout.

No installed executable has been promoted as part of this project.
