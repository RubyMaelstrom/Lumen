# Persistent formatting trees and dependency-scoped invalidation

Artifacts: `/big/Code/Lumen/benchmark-results/box-tree-20260908-qGbnyY`.
This round builds on [independent formatting-context reuse](INCREMENTAL_STYLE_LAYOUT_2026-09-08.md).
Its baseline is that round's **final** release, not the much slower earlier
browser. All browser measurements use release optimization and native Hybrid
rendering. The installed browser and the existing unrelated worktree changes
are untouched.

## What changed

The default shared DOM/layout path now retains immutable CSS formatting
subtrees. Unchanged children can be shared by a newly built ancestor instead of
being restyled and recursively reconstructed. This benefits both desktop and
terminal consumers of the canonical CSS-pixel layout.

- `layout2/tree.rs` uses shared immutable box nodes and inline children.
  `tree_cache.rs` stores the result of classifying/building an element. Its key
  includes the builder's incoming list-counter state and table nesting; reuse
  restores the outgoing list state. A changed preceding list item therefore
  cannot leave later marker values stale.
- Parent reconstruction still performs anonymous-box generation, inline/block
  splitting, flex/grid itemization and `display:contents` hoisting. The cache
  does not assume that the DOM tree and CSS box tree are interchangeable.
- Attribute writes invalidate affected selector subjects, inheriting
  descendants and layout ancestors. They no longer unconditionally discard
  every independent flow result. Local computed-property eviction now walks
  the dirty subtree's property keys instead of scanning the document's entire
  computed-value cache.
- Child-list edits invalidate the inserted/removed subtree and affected
  ancestors. Sibling/positional and `:empty` dependencies widen the dirty root.
  Where no such dependency exists, unchanged siblings keep their styles and
  formatting subtrees. Detach, reparent and replace-all still perform their
  original DOM operations and preserve observable node/mutation semantics.
- Detached construction uses the same dependency analysis, not an unconditional
  recursive invalidation of the whole growing fragment. The latter made
  repeated appends quadratic in an intermediate implementation and was removed.
- Rebuilding a box because its counter/context input changed also expires its
  old flow entry, even if that node's own attributes did not change.
- `DIAGGEOM` now reports `tree_reuse` and `tree_build` alongside tree/flow time.
  A subtree hit skips its descendants, so hit counts are not counts of all
  nodes saved.

### Correctness boundary

Relational/filtered positional selectors, state-dependent structural changes,
and shadow-tree dependencies retain broad invalidation. SVG reference changes,
including referenced text and removed resources, clear retained layout trees so
other `<use>` instances see the mutation. Named native-control associations,
metadata/base changes and document adoption are conservative too. `<picture>`
source changes invalidate the dependent sibling image, not just the `<source>`.
Enabled/disabled and link selectors do not by themselves force global
child-list invalidation: their ancestry/first-legend dependencies are covered
by moved-subtree and fieldset/select/optgroup invalidation. A focused test
checks first-legend changes and preservation of unrelated formatting subtrees.

Stylesheet, viewport, font, image-intrinsic, form/control, presentation and
container-query changes expire the relevant environment. The existing
container-query settling loop remains authoritative. CSSOM geometry is current
at each synchronous read; there is no timer throttle or delayed visibility of
mutations. A test-only cold path disables both caches and fully recascades.

This is **not** complete incremental layout: the parent sizing/positioning
algorithms and final geometry/paint assembly still run. Isolated boundary
rebuilds retain their uncached path. Precise reverse-reference and shadow
dependency indexes remain future work, not assumptions hidden in this cache.

### Ownership and bounds

The new document-owned tree cache has a 32 MiB accounted-storage budget, an
8,192-entry ceiling and an 8 MiB per-entry admission limit. It replaces old
versions and evicts old entries; it does not retain an unbounded history of
mutations. It is separate from the preceding stage's 32 MiB flow cache.

Shared subtrees are counted conservatively once per owning entry, including
reachable strings, styles and length-expression allocations, list states,
children, tables and cache slots. Overlapping ownership can therefore be
overcounted. The inventory is not an exact allocator/RSS measurement. Old
ancestor trees become reclaimable after invalidation, while unchanged child
allocations can remain shared. Tests verify this ownership behavior and bounded
storage under thousands of insertions and replacements.

Budget queries use maintained totals; accounting a newly inserted subtree still
walks that subtree, and choosing an eviction scans the bounded entries. These
are explicit remaining cold-build/eviction costs, to be measured rather than
assumed free.

## Controlled release replay

The final release improves both newly targeted mutation classes by about 3.2×
relative to the preceding optimization round, with 34% less process CPU in the
controlled 60-second capture. The already-optimized text case improves modestly.

| Measurement | Previous final release | This final release |
| --- | ---: | ---: |
| Attribute update + synchronous geometry, median of 20 | 223.10 ms | 70.45 ms |
| Child insertion/removal + geometry, median of 20 | 238.82 ms | 73.45 ms |
| Text update + geometry, median of 20 | 76.18 ms | 69.73 ms |
| Whole-capture browser CPU | 19.57 s | 12.91 s |
| Peak sampled RSS | 519.1 MiB | 521.1 MiB |
| Final sampled RSS | 519.1 MiB | 503.6 MiB |

Attribute/structure/text means are 222.01 / 235.93 / 81.17 ms before and
74.20 / 71.28 / 68.62 ms after; the respective 90th percentiles are
229.44 / 252.11 / 102.56 ms before and 92.24 / 76.46 / 72.44 ms after.
This is one controlled final pair with 20 observations per phase, not a
multi-run statistical confidence interval or a whole-web speedup estimate.

All 60 returned document/target rectangles are exactly equal, with maximum
coordinate difference **zero**. The final full-window screenshots are
byte-identical and have zero differing pixels; both SHA-256 values are
`43c220420c69e00c86e5c546aac60453d6bcd6e05e04ede82a7da6cbcfb8e2b9`.
No script abort, rejection or renderer failure was logged in these replay runs.

The structural trace now builds only 31–32 element results and reuses 30
subtree roots instead of rebuilding all 1,387–1,388 element results. Median
tree time falls from 26 ms to 1 ms; flow time falls from 203 ms to 65 ms as
independent flow results survive. Text tree time is below the trace's 1 ms
resolution in most updates, versus 13 ms before. Flow/fragment work remains
the dominant cost—tree reuse does not make the complete transaction free.

Results are in `replay-before-fast`, `replay-final-fast` and the generated
`replay-summary.json`.

The fixed input is derived from the prior frozen ShowBuzz HTML/CSS replay. Site
scripts and external fonts are removed, images are fixed local placeholders,
loading state is settled and animations paused. The additional injected rule
and script perform 60 sequential changes, each followed by synchronous body
and target bounding-rectangle reads:

1. Twenty alternating attribute writes/removals that change a card's padding.
2. Twenty alternating child insertions/removals in that card.
3. Twenty clock-text updates.

Every update is scheduled 250 ms after the preceding update finishes. The
measurement includes the mutation and synchronous geometry reads. It is a
real-page DOM/CSS workload, **not** a live-site load benchmark or a claim about
JavaScript execution speed/V8 parity. The initial captures last 150 seconds;
the affinity-controlled runs last 60 seconds, still completing all 60 updates.
Each uses a private fresh profile and 1,100×850 Wayland output (1,100×824 page
viewport).

### Measurement controls and the intermediate result

The first unpinned release pair produced mixed results: attribute median
207.62→70.47 ms, structural median 226.77→244.51 ms, and text median
72.68→140.35 ms. The final pixels and all 60 geometry reads were identical.
The structural trace showed zero tree hits: a stylesheet's `:disabled` selector
was conservatively forcing full invalidation. That was narrowed using the
first-legend/ancestry proof and regression test described above.

The GB10 host has heterogeneous CPU cores. Repeating the same intermediate
binary with affinity restricted to cores `5-9,15-19` removed the abrupt later
text slowdown. A matching-affinity baseline then measured attribute
223.10→60.48 ms, structural 238.82→226.25 ms, and text 76.18→62.70 ms.
This is evidence of a scheduling-sensitive measurement, not proof of a
specific migration in the original run (its samples did not record core IDs).
Final comparisons therefore use identical affinity, with per-thread core IDs
recorded and no concurrent agent-launched build/browser benchmark. The user
session remains running; CPU frequency and the entire host are not isolated.
The affinity setting is solely in the test harness, not a browser runtime
policy. Intermediate logs and `initial-unpinned-summary.json` /
`intermediate-fast-summary.json` are retained rather than discarded.

Base fixture SHA-256:
`6277d2bf60676a328f4989ecc66b4720ce5961795f579097d2cb4c0f813c8be6`.
New replay SHA-256:
`756da191c3b5d9960136a5230898f2328d14ae5fde1705dd8cd4ea80cc886cfa`.
`make-replay.mjs`, `serve-replay.mjs`, `desktop.mjs` and
`summarize-replay.mjs` preserve the reproduction and analysis.

## Verification

The final-source optimized test gate passed **1,183 library tests and 29 desktop
tests** (`final-tests-3.log`). Twenty existing ignores remain; the two previously
identified unrelated Wasm retention tests are explicitly filtered:
`fresh_async_wasm_instances_do_not_accumulate` and
`fresh_sync_wasm_instances_do_not_accumulate`. Scoped rustfmt and `git diff
--check` pass. Clippy completed with the same 14 pre-existing warnings and no new
warnings in the optimization code.

Nine new tests cover immutable subtree identity and old-version release,
warm/cold equivalence after attributes and child edits, structural selectors,
anonymous boxes, `display:contents`/`none`, reparenting, list counter state,
referenced SVG resources, picture source selection, shadow distribution,
detached shadow inheritance, adoption and storage limits. Differential layout
checks compare border-box geometry, grid tracks, scrolling extents, graphical
paint/hit-test payloads and terminal rows against a full uncached recascade and
layout. One old test's policy assertion was updated: an unrelated sibling
insertion should now preserve cached selector matches when the stylesheet has
no structural dependency; preceding-sibling positional changes still invalidate.

### Live rendering acceptance

Fresh private profiles used the same 1,100×850 output and native NVIDIA Hybrid
renderer. These runs overlap other correctness checks (and the reference runs
overlap compilation), so their CPU totals are **not controlled throughput
comparisons**. No accounts were accessed or messages/submissions sent.

- **OpenAI article:** both four-minute scroll runs initialize all 13 chart
  wrappers with SVG marks and nonzero dimensions. Final document dimensions
  agree exactly: 1,107.3334×18,033.1641 CSS px. The chapter navigation, rounded
  header controls, related-card destinations and five-column footer remain
  intact. The 60-second chart and 220-second footer screenshots have zero
  differing pixels. Existing JSON rejections, unsupported video on the middle
  card and baseline rendering limitations remain; equivalence is to the
  previous TRust release, not a claim of complete browser conformance.
- **ChatGPT:** both signed-out runs hydrate the same 640×52 composer at
  (360, 346.08), with sidebar, controls and cookie panel. Both the 60-second
  and 150-second screenshots have zero differing pixels. The probe records no
  errors; the separately logged, pre-existing `no idle gap found` rejection
  remains. No interaction with the composer or account controls was performed.
- **ShowBuzz covered-window soak:** completed ten minutes, covered from 60 to
  585 seconds and then uncovered. The page redrew correctly with its clock
  advancing, all 39 current images complete, no loading placeholders, and
  eight events populated. No script abort/rejection, renderer panic or renderer
  fallback was logged. Sampled RSS was 639.4 MiB at one minute, 620.8 MiB at
  five minutes and 635.1 MiB at completion; sampled peak was 703.0 MiB during
  startup. Six-/seven-/eight-/nine-minute samples were 623.3 / 627.1 / 629.9 /
  632.9 MiB. There is modest late growth, not a claim of perfectly flat memory.
  No runaway occurred in this interval; it does not prove absence of every
  hour-long leak. Live content differs from prior rounds, so their image counts
  and RSS are not directly interchangeable.

Observed live process RSS is a tradeoff worth retaining: ChatGPT finishes at
1,462.8 MiB before versus 1,517.7 MiB after; OpenAI finishes at 1,535.6 MiB
before versus 1,514.3 MiB after. These are single live runs with allocator and
runtime variability, not exact attribution to cache payload. In particular,
the tree cache's 32 MiB accounting limit does not cap whole-process RSS or all
document caches combined. These checks establish rendering preservation, not
a material OpenAI/ChatGPT startup speedup.

`acceptance-summary.mjs` regenerates `acceptance-summary.json` from the raw
logs/probes and process samples.

### Detached construction regression probe

`detached-replay.html` is a separate synthetic guard, not the real-page speedup
claim. It creates an unattached fragment, primes computed style, then appends
250, 500 and 1,000 text-bearing spans with class changes. Each final fragment
must contain `n+1` children. The SHA-256 is
`7479bc9b07823026257fcd3fae89002b3d65cae3b3ff5b3af8ae58c5a2808e6b`.

The intermediate implementation took 103.68 / 437.03 / 1,875.33 ms, compared
with 12.93 / 21.11 / 42.37 ms in the original baseline. This exposed an
introduced quadratic invalidation cost before acceptance. The final path uses
the selector dependency proof for detached nodes too, retaining broader
invalidation when that proof is unavailable. The final release takes
14.22 / 23.61 / 47.73 ms, with the expected 251 / 501 / 1,001 children. This
restores approximately linear scaling; it is still about 13% slower than the
old baseline at 1,000 children in this single probe. The added mutation
bookkeeping is not free, and this is not presented as a construction speedup.
Both controls completed their timed loops before any subsequent compile
started; the later idle tail is not part of these loop measurements. The final
probe's timed loops also finished before live-site acceptance began.

## Remaining measured work

The separate `replay-profile-final` diagnostic uses 20 ms CPU sampling and the
same final release, overlapping live-site acceptance. It is not another speed
comparison. Approximately 11.28 sampled CPU seconds cover the active startup
and mutation interval; the collector warns that the experiment was not closed
cleanly when the private browser was terminated. Rust inlining and incomplete
caller stacks limit attribution. Raw function/caller reports and collector
metadata are retained.

Prominent exclusive samples include `Dom::computed_value` (0.56 s), string-key
map lookup (0.44 s), `prop_index` (0.40 s), fragment clone (0.36 s), allocation
(0.48 s in one allocator path) and memcpy (0.46 s). Raster strip generation,
clipping and tile sorting account for 0.78, 0.58 and 0.56 s respectively in
the whole active interval, which also includes initialization. These are not
all independently attributable to a single layout flush, and inclusive times
must not be added as though disjoint.

The next architectural target is to stop repeatedly resolving/string-copying
unchanged style inputs and deep-copying retained fragment payloads, then carry
dirty-subtree identity through presentation assembly. The current tree cache
is a prerequisite, not the end of that work. Dedicated active-phase profiles
and cold-vs-warm correctness comparisons should establish the next gain;
this report does not promise that any one micro-hotspot yields a browser-wide
breakthrough or addresses YouTube's separate JavaScript execution bottleneck.

## Release artifacts

`release-build-final-3.log` records the successful final build with ordinary
release optimization (opt-level 3, ThinLTO, one codegen unit), retaining symbols
for diagnostics. No debug browser was used for timing.

- `/big/Code/TRust/target/release/trust`, SHA-256:
  `1abe00d29c05fa786454da662a0c9546bf2cce5597eba6699d5402c822fee3a5`.
- `/big/Code/TRust/target/release/trust-desktop`, SHA-256:
  `a8e5cb9b98decab9edac46024688aabb9dde993b55013816ca2803cb25c4a903`.
  The frozen `final-trust-desktop` is byte-identical.
- `final-source.tar` contains the corresponding browser sources and Cargo
  files. `before-source.tar`, `before-trust-desktop`, and the intermediate
  snapshots/results retain the comparison history.

No installation, commit or push was performed. Existing worktree changes,
including Boa retirement and prior Lumen/browser changes, were preserved.
The final source archive was compared back against the worktree, the frozen
desktop binary matches the release target, and scoped formatting/diff checks
passed. All private test browsers, compositors and loopback fixture servers
were stopped; the diagnostic artifacts are retained.

## Standards basis

The web-standards skill was used offline, with the official sources fetched on
2026-09-06. These are recorded living-standard/editor's-draft snapshots, not a
claim to have freshly verified upstream. The skill informed the invalidation
boundaries and counter, shadow, SVG, picture and CSSOM differential tests.

CSSWG sources use revision
`81c27f68690138345b2b3b6af8ccc42dad3dca1d`:

- [CSS Display: box tree](https://drafts.csswg.org/css-display-3/#box-tree),
  [anonymous boxes](https://drafts.csswg.org/css-display-3/#anonymous) and
  [box generation](https://drafts.csswg.org/css-display-3/#box-generation):
  local [source](/big/web-standards/repositories/w3c/csswg-drafts/css-display-3/Overview.bs:95),
  `display:contents` clause at line 963. Reuse must preserve generated-box
  structure, not substitute DOM nodes for layout boxes.
- [Cascade: inheritance](https://drafts.csswg.org/css-cascade-5/#inheriting):
  local [source](/big/web-standards/repositories/w3c/csswg-drafts/css-cascade-5/Overview.bs:1795).
  Descendants and flattened shadow-tree inheritance remain dependencies.
- [Selectors: relational pseudo-class](https://drafts.csswg.org/selectors-4/#has-pseudo),
  [structural pseudo-classes](https://drafts.csswg.org/selectors-4/#structural-pseudos)
  and [child indexes](https://drafts.csswg.org/selectors-4/#child-index):
  local [source](/big/web-standards/repositories/w3c/csswg-drafts/selectors-4/Overview.bs:1768),
  structural clauses at lines 3724 and 3896. The round preserves the engine's
  existing `:empty` semantics; it does not independently change specification
  level or whitespace behavior.
- [Lists: counter inheritance](https://drafts.csswg.org/css-lists-3/#inheriting-counters):
  local [source](/big/web-standards/repositories/w3c/csswg-drafts/css-lists-3/Overview.bs:1000).
  Tree-order state is an explicit cache input/output.
- [CSSOM View: client rectangles](https://drafts.csswg.org/cssom-view-1/#dom-element-getclientrects):
  local [source](/big/web-standards/repositories/w3c/csswg-drafts/cssom-view-1/Overview.bs:1298).
  Reads use the current CSS fragments, including after each mutation.
- [Conditional Rules: animated containers](https://drafts.csswg.org/css-conditional-5/#animated-containers):
  local [source](/big/web-standards/repositories/w3c/csswg-drafts/css-conditional-5/Overview.bs:1221).
  Query changes remain part of the style/layout settling transaction.

Other official snapshots:

- DOM revision `a2331a45360129e8645ef7e0a04740241b6e3726`,
  [mutation algorithms](https://dom.spec.whatwg.org/#concept-node-replace-all)
  and [adoption](https://dom.spec.whatwg.org/#concept-node-adopt):
  local [mutation source](/big/web-standards/repositories/whatwg/dom/dom.bs:3060)
  and [adoption source](/big/web-standards/repositories/whatwg/dom/dom.bs:6125).
  Invalidation cannot alter node identity, parentage, document ownership or
  shadow-including adoption semantics.
- HTML revision `e5071a20c8569d8a3ec02ed27dd01b948773f850`,
  [ordered lists](https://html.spec.whatwg.org/multipage/grouping-content.html#the-ol-element)
  and [image source selection](https://html.spec.whatwg.org/multipage/images.html#select-an-image-source):
  local [list source](/big/web-standards/repositories/whatwg/html/source:21798)
  and [image source](/big/web-standards/repositories/whatwg/html/source:33629).
  Reversed/start/value marker state and preceding picture sources can affect
  otherwise unchanged siblings.
  [Disabled controls](https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#concept-fe-disabled)
  and [disabled elements](https://html.spec.whatwg.org/multipage/semantics-other.html#concept-element-disabled),
  local [control source](/big/web-standards/repositories/whatwg/html/source:61057)
  and [selector source](/big/web-standards/repositories/whatwg/html/source:80507),
  establish the first-legend and ancestry boundary used for structural edits.
- SVG revision `c403ca46ad045ebdaeda148bae69f814fb744db7`,
  [use-element shadow trees](https://w3c.github.io/svgwg/svg2-draft/struct.html#UseShadowTree):
  local [source](/big/web-standards/repositories/w3c/svgwg/master/struct.html:830).
  Referenced-subtree mutations must propagate to all instances.
