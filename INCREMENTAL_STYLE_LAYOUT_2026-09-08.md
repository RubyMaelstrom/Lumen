# Incremental style/layout: independent formatting-context reuse

Artifacts: `/big/Code/Lumen/benchmark-results/incremental-layout-20260908-36pwzB`.
The directory contains the pre-change browser sources and release executable,
release build/test logs, private-browser captures, and a frozen-input replay.

## Implementation

This is a default-on change to TRust's canonical shared DOM/style/layout path,
not a ShowBuzz rule or a JavaScript timer throttle. Synchronous CSSOM reads still
observe every DOM revision. A new geometry transaction can reuse work internally:

1. Computed styles, cascaded declarations, inherited variables, font metrics and
   decorations no longer expire just because an unrelated node changed.
   Attribute dependency analysis invalidates the relevant selector subjects and
   their inheriting descendants. Structural, relational/state and shadow cases
   retain broad fallbacks where the dependency is not proven local.
2. Character-data changes and text-only replace-all operations invalidate the
   changed layout branch and its ancestors. Text replacement still creates a
   **new Text node** and detaches the old children; wrapper identity and mutation
   semantics are not sacrificed to reuse. `:empty` truth-value changes and
   text-dependent selector state remain explicit invalidation inputs.
3. Independent item layout and intrinsic measurements are retained. Reuse checks
   the actual content width, percentage basis, definite height, aspect-ratio
   transfer mode, box style, inherited inline context and marker state. The
   parent's flex/grid/flow algorithm still sizes and positions its children.
   This also removes repeated identical probes within one transaction.
4. Retained item fragments include shaped text, anchor positions and used grid
   tracks. Borrowed out-of-flow placeholders and transaction-local fixed indices
   cannot enter the cache. CSSOM geometry, overflow measurement, hit testing,
   graphical paint and the terminal adapter consume the resulting canonical
   fragments.
5. Viewport/base URL, images, forms/control bindings, font installation and
   activation/presentation revisions are cache inputs. SVG intrinsic metadata
   now has a separate revision: its ratio can change without a change in decoded
   pixel dimensions. Container-query changes invalidate font-relative units too;
   inherited numeric computed lengths also expire on font changes.

### Scope and bounds

The new document-owned cache is limited to 32 MiB of accounted owned payload,
2,048 entries and eight constraint variants per node. It evicts old results and
does not retain an unlimited history of clock ticks or resize inputs. Owned
fragment strings, shaping data, expression trees, grid tracks and environment
snapshots are included in the host memory inventory. Allocator/hash-table
metadata and process-shared font bytes are not claimed as exact accounting.
Budget checks use incrementally maintained totals rather than scanning the cache
and resource environment on every insertion; tests cross-check those totals
against a deep inventory during allocation, eviction and clearing.

Attribute writes currently retain **full flow invalidation**, even when computed
styles can be invalidated locally: counter sequencing, SVG references and native
control associations can escape selector-subtree dependencies. General structural
edits also retain a full fallback. The box tree and final geometry maps are still
rebuilt per transaction. This is not yet a persistent, fully incremental box tree.

## Initial live release evidence

Both five-minute runs used private fresh profiles, a foreground 1,100×850
headless Wayland output, native Hybrid rendering on NVIDIA GB10, and identical
trace/raw-capture settings. No user profile or installed executable was changed.

| Measurement | Before | First optimized release |
| --- | ---: | ---: |
| Browser CPU, five-minute run | 319.44 s | 91.19 s |
| CPU after first minute | 255.08 s | 63.76 s |
| Median geometry transaction, last 100 renders | 1,449 ms | 81 ms |
| Peak sampled RSS | 763.8 MiB | 767.5 MiB |

The clock continued to advance and the visitor count and event sidebar populated.
No script abort or renderer panic/fallback was recorded. These are **live
observations**, not an isolated speedup claim: the site's card/event content
changed between loads (the final arena sizes were 5,536 versus 5,011 slots).

The settled frozen-input replay removes that variability. It keeps one downloaded
ShowBuzz HTML/CSS response, removes the site's scripts/external fonts, substitutes
fixed local images, applies the image-ready/loading-overlay state, pauses CSS
animation, and runs 60 clock writes followed by synchronous geometry
reads. It is specifically a layout replay, not a claim about whole-site load time
or the original site's networking/JavaScript. `freeze-replay.mjs --settled`
records the input hashes, and `replay-server.mjs showbuzz-settled-replay.html`
serves only this fixture over loopback. The earlier `replay-before` smoke retained
the initial loading overlay and is not the settled comparison.

Frozen source SHA-256:
`99b4fa8def8da7386d1e71301db5eafd73234f09da9e8002168a61cb14a9136d`.
Settled replay SHA-256:
`6277d2bf60676a328f4989ecc66b4720ce5961795f579097d2cb4c0f813c8be6`.

### Controlled replay: final release

`settled-before` and `settled-after` each completed all 60 identical updates,
using the same loopback fixture, native Hybrid renderer, viewport and 150-second
capture duration. These runs were separate from the concurrent cross-site
acceptance checks below.

| Measurement | Before | Final optimized release |
| --- | ---: | ---: |
| Median synchronous layout read, all 60 updates | 1,196.55 ms | 75.00 ms |
| Mean synchronous layout read, all 60 updates | 1,201.96 ms | 85.64 ms |
| 90th percentile, all 60 updates | 1,219.86 ms | 132.76 ms |
| Median after the first five updates | 1,195.43 ms | 74.84 ms |
| Browser CPU over the whole 150-second capture | 81.73 s | 11.48 s |
| Peak sampled RSS | 510.4 MiB | 503.4 MiB |
| Final sampled RSS | 510.4 MiB | 489.6 MiB |

That is approximately **16× faster median synchronous layout**, with **86% less
browser CPU** in this controlled replay. It is not a 16× whole-browser or
JavaScript-engine speedup. Some updates still need more work than the median,
as the percentile and mean show.

The final screenshots (`settled-before/at-150.png` and
`settled-after/at-150.png`) are byte-identical, with **zero differing pixels**
under ImageMagick's absolute-error comparison. Both have SHA-256
`05c9d97c31a18652546f9d24b7c40b28a538630641da59971396941f976a5a88`.
The geometry read returned the same 1,100×824 document
rectangle in all 60 updates. Differential tests, rather than this single visual
comparison alone, cover changing layout constraints and invalidation cases.

## Correctness gates

New differential tests compare warm results against an explicitly disabled-cache
full style/layout transaction, including border boxes, grid CSSOM tracks,
scrolling extents, graphical presentation and terminal rows. Cases include:

- text width/height changes, wrapping and empty content;
- inherited custom properties, fonts, decoration and sibling/relational selectors;
- text-node identity, detach/reparent behavior and stylesheet fallback;
- nested flex/grid, percentage padding, aspect ratios, floats and atomic inlines;
- absolute/fixed descendants, links and activation changes;
- container-query changes, image sizing, SVG metadata, forms and font revisions;
- bounded storage under thousands of changing constraints and cache keys.

The final-source gate passed **1,174 library tests and 29 desktop tests**
(`full-tests-final-3.log`, `desktop-tests-final-3.log`). The
library gate preserved 20 existing ignores and filtered the two previously
identified unrelated Wasm retention tests (`fresh_async_wasm_instances_do_not_accumulate`
and `fresh_sync_wasm_instances_do_not_accumulate`). Clippy succeeded with 14
warnings in pre-existing code, not in the new cache/invalidation logic.

## Live rendering acceptance

The final release was checked on fresh private profiles against the pre-change
release, with the same 1,100×850 output and native NVIDIA Hybrid renderer.
These checks overlap each other and the background soak; they are **correctness
checks, not isolated cross-site throughput benchmarks**.

- **OpenAI article:** the four-minute scripted scroll initializes all 13 chart
  wrappers, each with SVG marks and nonzero width. The chapter navigation,
  rounded header controls, related-card link destinations and five-column footer
  are retained. The 60-second chart and 220-second footer captures are
  pixel-identical before/after. Final document dimensions also match.
  The original external JSON rejections remain. The middle related card still
  uses unsupported video; it is not claimed as fixed by layout reuse.
- **ChatGPT:** both signed-out runs hydrate the editor and retain the same
  640×52 composer at the same coordinates. The sidebar, controls and cookie
  panel render. The 60-second screenshot differences are confined to the
  greeting (a 337×23 rectangle): the page chose different greeting text.
  The probe records no errors, while the separately logged, pre-existing
  `no idle gap found` rejection remains. Known font differences from LibreWolf
  described in `SHOWBUZZ_CHATGPT_RENDERING_2026-09-07.md` are not resolved here.
- **ShowBuzz covered-window soak:** completed ten minutes, covered from 60 to
  585 seconds and then uncovered. The page redrew correctly with an advancing
  clock, all 53 images complete and eight events populated. No script error,
  renderer panic or renderer fallback was recorded. Sampled RSS was 661.7 MiB
  at one minute, 725.7 MiB at five minutes and 750.6 MiB at completion; peak
  sampled RSS was 751.9 MiB. The six-, seven-, eight-, nine- and ten-minute
  samples were approximately 743, 745, 744, 751 and 750 MiB. This shows no
  runaway during the tested interval, not proof against every hour-long leak.

The retained caches are not free memory: final sampled ChatGPT process RSS was
1,449.1 MiB before versus 1,495.4 MiB after. That live process-level difference
is not an exact cache inventory, but it is a useful observed memory tradeoff;
the 32 MiB limit applies to accounted cache ownership, not total process RSS or
allocator overhead. These runs do not demonstrate a material OpenAI/ChatGPT
startup speedup. Structural hydration still takes the conservative fallback;
the large confirmed gain is on repeated local text updates and geometry reads.

Raw logs, probes, screenshots and process samples are in `openai-final`,
`openai-before`, `chatgpt-final`, `chatgpt-before`, and
`showbuzz-background-final`. `acceptance-summary.mjs` regenerates the compact
`acceptance-summary.json` from those artifacts. Visual equivalence here is to
the previous TRust release, not a claim of complete web-platform conformance.

## Release artifacts

`release-build-4.log` records the final build: ordinary release optimization,
ThinLTO and one codegen unit, retaining symbols for diagnostic attribution.
No debug browser executable was used for timings.

- `target/release/trust`: SHA-256
  `658f9d3f405fc30ce2a404e8e7c25339964e3b9d89836d165395a6983b1a4c75`.
- `target/release/trust-desktop`: SHA-256
  `76b416e59cfb0738ce320ae184f1fcc4b5c479d4823351fffcc23144ada987b3`.
  The frozen `final-trust-desktop` is identical.
- `final-browser-source.tar` contains the corresponding affected browser sources.

Existing worktree changes, including Boa retirement and earlier Lumen fixes,
were preserved. This round does not install, commit or push an executable/source
promotion; the rebuilt release targets are available for user acceptance.

## Standards consulted

The local web-standards skill supplied official upstream editor-source snapshots
fetched 2026-09-06; no standards servers were refreshed. CSSWG revision:
`81c27f68690138345b2b3b6af8ccc42dad3dca1d`.

- [Inheritance](https://drafts.csswg.org/css-cascade-5/#inheriting),
  `/big/web-standards/repositories/w3c/csswg-drafts/css-cascade-5/Overview.bs:1795`.
- [Flex layout equivalence](https://drafts.csswg.org/css-flexbox-1/#layout-algorithm),
  `css-flexbox-1/Overview.bs:3589`; independent flex-item contents at line 929.
- [Independent grid-item contents](https://drafts.csswg.org/css-grid-2/#grid-item-display),
  `css-grid-2/Overview.bs:1140` (subgrids are explicitly not independent).
- [Current box geometry](https://drafts.csswg.org/cssom-view-1/#dom-element-getclientrects),
  `cssom-view-1/Overview.bs:1298`.
- [Container-query style changes](https://drafts.csswg.org/css-conditional-5/#animated-containers),
  `css-conditional-5/Overview.bs:1223`.
- [Relational selectors](https://drafts.csswg.org/selectors-4/#has-pseudo),
  `selectors-4/Overview.bs:1768`; `:empty` at line 3761 and directionality at 2498.
- [Font-relative units](https://drafts.csswg.org/css-values-3/#font-relative-lengths),
  `css-values-3/Overview.bs:1155`, and
  [computed line heights](https://drafts.csswg.org/css-inline-3/#line-height-property),
  `css-inline-3/Overview.bs:946`.
- [DOM replace-all](https://dom.spec.whatwg.org/#concept-node-replace-all),
  `/big/web-standards/repositories/whatwg/dom/dom.bs:3240`; string replace-all at
  line 4808, revision `a2331a45360129e8645ef7e0a04740241b6e3726`.
- [SVG intrinsic sizing](https://w3c.github.io/svgwg/svg2-draft/coords.html#SizingSVGInCSS),
  `/big/web-standards/repositories/w3c/svgwg/master/coords.html:1273`, revision
  `c403ca46ad045ebdaeda148bae69f814fb744db7`.

This round changes the browser foundation, not Lumen's parser/interpreter/GC.
Source-to-execution startup and JS-heavy steady-state workloads remain separate
optimization work; the layout results must not be represented as V8 parity.

## Next optimization boundary

The remaining layout work is persistent box-tree maintenance and dependency-safe
reuse across attribute/structural updates. That should remove more of the full
tree construction and hydration cost that this stage deliberately leaves intact.
Lumen source-to-execution startup is a separate measured target after this
foundation; YouTube/Twitch execution and allocation throughput need their own
profiles and before/after checks, not extrapolation from the ShowBuzz replay.
