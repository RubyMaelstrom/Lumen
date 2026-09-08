# ShowBuzz and ChatGPT: rendering, compatibility, and measured costs

Investigation begun 2026-09-07; validation continued 2026-09-08. Browser changes are in the sibling TRust checkout;
the JavaScript parser correction is in Lumen. No installed binary was replaced,
and no commit or push was made during this round.

Artifacts: `benchmark-results/showbuzz-layout-20260907-EGKlNb/`.
The directory contains frozen release executables, source-before snapshots,
private-profile test harnesses, screenshots, resource samples, and test logs.
The pre-existing dirty TRust tree and retired Boa removals were preserved.

## ShowBuzz

### Root causes and fixes

- The implicit `auto` grid column used min-content width where Grid specifies
  the item's minimum contribution. A horizontal scrolling strip consequently
  forced an approximately 11,475px track into an approximately 817px panel.
  Track sizing now distinguishes minimum, min-content, and max-content
  contributions, including scrollable overflow, explicit minima, flexible
  spans, and indefinite-height growth. `overflow:clip` still differs from
  `overflow:hidden`; the fix is not a universal `min-width:0` override.
- Column-flex relayout passed an already border-box width back as content
  width, adding padding and borders twice. That relayout now subtracts the
  cross-axis border/padding contribution.
- Box shorthands containing custom properties were expanded prematurely.
  Pending substitution now survives the cascade and expands after variables
  resolve, including per-pseudo-element variables and invalid-value resets.
  Direct font metrics use the same resolved cascade as layout.
- Scroll snapping called `.slice()` on `querySelectorAll()`'s real NodeList.
  This threw in ShowBuzz's initial carousel/filter setup and prevented the
  remainder of that script from running. The internal consumer now uses
  `Array.from`; NodeList itself does not acquire non-standard Array methods.
  A second occurrence in required-radio validity was corrected similarly.
- The font library's PNG bitmap feature was disabled, losing color emoji.
  Enabling it exposed a separate Hybrid GPU limitation: pixmap-backed paints
  panicked and triggered a CPU fallback. Hybrid now uploads these as temporary
  image-atlas resources, preserves their sampling transforms, and releases
  their allocations after the draw in command order. The experimental glyph
  atlas remains disabled.

### Live checks

Release desktop runs used a private headless Wayland compositor, a fresh
isolated profile, and the NVIDIA GB10 Hybrid renderer. No user browser session
or account was accessed. Screenshots are captured at native resolution.

- Wide panels fit their viewport; carousel overflow stays inside its
  scrollport. Narrow 700px and large 1600px layouts were also exercised.
- Native carousel-arrow input advanced the strip by one card (208.8 CSS px).
  Toggling the DJ filter updated the visible cards and reset the scroll offset.
  No votes, messages, or other external writes were made.
- The script now reaches visitor and upcoming-event initialization. This site
  also awaits its large initial image set before those updates; image/network
  completion must not be confused with JavaScript execution time.
- The final 900-second covered-window run (`gpu-bitmap-soak`) stayed on
  Hybrid, with no renderer panic or fallback. RSS peaked at **802.8 MiB** and
  ended at **749.1 MiB**. This is a bounded 15-minute observation, not proof
  against every possible long-duration leak. See the earlier
  `SHOWBUZZ_MEMORY_FIX_2026-09-07.md` for the original runaway and queue fix.
- Two intermediate PNG-enabled runs (`gpu-compat`, `gpu-font-cache`) fell
  back to CPU before the pixmap correction. They must not be presented as
  successful GPU soak results.
- A later 300-second release smoke (`showbuzz-final`) also stayed on Hybrid,
  rendered the populated event/sidebar content and color emoji, and ended at
  798.3 MiB RSS (816.1 MiB peak). There was no renderer panic or script abort.
- The exact final binary also passed a 60-second structural/GPU smoke
  (`showbuzz-module-cache`, 781.2 MiB at the 60-second sample). Its event/header
  data were still awaiting initialization at that point; this shorter run is
  not the site's complete-load acceptance evidence.

### Performance

The release sampling profile (`gpu-layout1/cpu.er`, 250.66 CPU seconds) showed
repeated canonical geometry/layout work as the principal actionable cost.
`InlineStyle::derive` accounts for 16.18% inclusive CPU and `Units::of` for
10.69% in `inclusive-cpu.txt`; these percentages overlap and must not be added.

Font-relative units are now memoized once per node, stamped by the DOM/style
epoch and shared font revision. The bounded cache is included in retained-heap
accounting, rejects synthetic node IDs, and invalidates for DOM/font changes.
Repeated full-layout observations decreased from approximately 2.7 seconds to
2.3 seconds (roughly 14–16%). These are diagnostic runs, not an isolated
end-to-end load benchmark. The page's frequent clock updates still trigger
expensive full geometry work and keep approximately one CPU core busy. This
round does **not** solve that larger incremental-layout problem.

## ChatGPT

### Confirmed root causes and fixes

- Lumen stored `for await` parsing state in a shared mutable parser field.
  A nested loop could change the flag for its containing loop. ChatGPT's
  classic startup module consequently failed with a false `for await` syntax
  error. The flag is now local to each loop production. Tests cover valid
  nested ordinary loops, invalid await heads, nested functions, script/module
  context, and actual asynchronous iteration. This unblocks startup; it is
  a compatibility correction, not a claimed interpreter throughput gain.
- ChatGPT's pills use valid `3.40282e38px` corner radii. Absolute CSS length
  parsing did not recognize scientific notation, and overlapping-radius
  arithmetic could overflow even after parsing. Number-prefix consumption
  now follows CSS Syntax's exponent lookahead without consuming `em` as an
  exponent. Radius reduction uses finite clamping and wider intermediate
  arithmetic. Pixel/paint tests cover both enormous finite dimensions and
  `calc(infinity * 1px)`.
- External SVG sprite import discarded the actual `<use>` element, losing
  its fill/stroke, inherited color, and geometry. The imported definitions
  now remain behind the authored use instances. Raster regressions cover
  differently colored repeated instances, source-specific paint overrides,
  descendant `currentColor`, offsets/sizing, and legacy `xlink:href`.
- Atomic inline blocks/flex/grid boxes ignored minimum and maximum heights.
  Applying the same min/max constraints as other boxes restores the sidebar
  login button from approximately 16px to its authored 44px height.
- Editing hosts were treated as synthetic textarea layout atoms, throwing away
  their rich descendants and generated placeholders. Their real boxes now flow
  and paint normally, while the form binding remains input metadata. The desktop
  focused-editor overlay now adds selection/caret rather than an opaque white
  replacement textbox. Its text metrics and caret color come from the canonical
  layout. Physical typing was tested; no prompt was submitted.
- `getComputedStyle().lineHeight` returned authored `1.625rem` instead of `26px`;
  padding returned `calc(.25rem * 4)` instead of `16px`. ChatGPT's actual composer
  code parses these values to calculate one-line and five-line height thresholds.
  The old values made an empty, one-line editor appear multiline. Absolute box
  edges now resolve without a layout flush. Line-height lengths/percentages
  compute before inheritance; numbers remain numbers internally and resolve to
  pixels at CSSOM. The reduced JS test checks both CSSOM and the measured 42px
  padded editor height.
- Generated text now carries its pseudo-element paint identity, including the
  placeholder's own color. Adjacent runs must not merge across those identities.
  The terminal adapter also preserves adjacency when differently painted runs
  meet, preventing a new whitespace cell at a purely stylistic boundary.
- The remaining startup TypeErrors came from unavailable HMAC `generateKey` and
  `exportKey` operations. HMAC key generation/import/export/sign/verify now use
  native RustCrypto primitives with protected CryptoKey slots, usage/extraction
  checks, JWK validation, and constant-time verification. Random generation now
  uses OS entropy, including bounded integer typed-array views and v4 UUIDs.
  The main page and workers share these host functions.
- The module dependency provider rescanned cached source whenever another
  importing parent requested it. A response-entry-local claim now performs
  speculative descendant discovery only once. It does not skip required
  source reads, parsing, linking, evaluation, or error handling. Inline modules
  remain separate even when they share a base URL; replacing or retiring a
  cached response clears its claim. The regression checks concurrent claims,
  repeated required reads, replacement, cancellation, and cache misses.

### Module-loading performance

The symbol-preserving **release** profile `chatgpt-final/cpu.er` exposed
`speculate_module_imports` as the largest named exclusive CPU cost: **3.70
seconds**. The first fresh-profile run of the corrected binary
(`chatgpt-module-cache/cpu.er`) reduced it to **0.06 seconds**, approximately
98% less CPU in that operation. ChatGPT still hydrated its signed-out composer
and the diagnostic probe recorded no errors.

These executables retain symbols for sampling but still use the normal release
optimization and ThinLTO settings. The earlier stripped executable's unresolved
static-address profile is not used for function attribution. The two initial
runs have different durations and interaction histories, so their whole-run
totals are **not** a controlled page-load comparison. Their startup/composer
asset hashes also differ; the follow-up pair below removes that particular
revision mismatch.

A sequential 120-second-before/120-second-after repetition used the same
viewport, renderer, probe, fresh profiles, no user input, and matching core
and composer assets (`4813494d-o593jrji51wy4azk.js` and
`8b34dbc2-nhot65scqrg20d6p.js`). Module scanning measured **3.54 seconds before
and 0.10 seconds after**, about 97% less CPU in that operation. Process CPU
accumulated by the 60-second sample was **27.93 vs 23.42 seconds**, about 16%
less. Both rendered a 52px composer with no probe errors.

This is a live-site diagnostic pair, not a replay benchmark or load-latency
guarantee: the final DOM had 707 versus 715 nodes, and the latter contained
eight additional same-revision module references. Other server/network/runtime
variation remains possible. Logs: `chatgpt-scan-before-repeat` and
`chatgpt-scan-after-repeat`.

The corrected 300-second session accumulated 25.0 process CPU seconds by
60 seconds and 28.4 by 300 seconds. RSS at 300 seconds was 1,564.4 MiB; this
optimization does not establish a memory reduction. In its 26.82 sampled CPU
seconds, UTF-8 chunk decoding accounts for 2.06 seconds, module parsing for
1.90 seconds inclusive, and GC collection for 2.02 seconds inclusive. These
are follow-up candidates, not additional completed optimizations. Incomplete
stack unwinding leaves some samples unattributed.

### Isolated crypto performance

SHA-1, SHA-384 and SHA-512 previously ran substantial hash loops in interpreted
JavaScript (including BigInt arithmetic for SHA-384/512). These public digest
paths now use native implementations. The obsolete interpreted implementations
were removed. SHA-256 was already native and is not presented as an improvement.

The local HTTP fixture `crypto-cost.html` executes eight 8,192-byte digests per
algorithm in release desktop binaries. One initial before/after pair:

| Algorithm | Before, ms | After, ms |
| --- | ---: | ---: |
| SHA-1 | 118.92 | 0.289 |
| SHA-256 | 0.351 | 0.401 |
| SHA-384 | 797.34 | 0.225 |
| SHA-512 | 792.30 | 0.225 |

All digest bytes match. This demonstrates elimination of those expensive
interpreted loops, **not** a 400–3,500× whole-page or general-JS speedup. The tiny
native timings need repetition before fine comparisons; SHA-256's difference
is not a meaningful end-to-end result. The earlier `file:` fixture did not run
as HTML and is excluded. Logs: `crypto-before-http` and `crypto-after-http`.
A final-release repetition (`crypto-final-http`) measured 0.265, 0.394,
0.231, and 0.222 ms respectively, again with identical digest bytes. A separate
`crypto-before-repeat` ran without console tracing and is not used as a
machine-readable timing sample.

### Comparison controls

ChatGPT initially served different applications to TRust and LibreWolf:
the classic React app versus an `unauth-mweb` app. A fresh **reference-only**
LibreWolf profile with fingerprint resistance disabled and `TRust/0.1` user
agent receives the same classic app and asset revision. TRust's production
user agent was not changed. The reference was finally reloaded using the app's
own saved dark-theme preference; the earlier root-class-only dark screenshot
is not a valid full-theme comparison. Raw rendered HTML contains baked styles and is not an
independent CSS reference; original DOM and reference styles are saved
separately under `chatgpt-reference-aligned`.

The corrected single-line composer is 52px high with a 495px-wide, 42px-high
editor, matching the corresponding reference measurements. Its external SVG
icons, rounded surfaces, placeholder color, cookie controls, and focused
editing surface render. `chatgpt-final/typed.png` records native typing without
submission; `chatgpt-final/narrow.png` records the 700px responsive layout.
The final module-cache binary's `chatgpt-module-cache/at-300.png` records the
unmodified signed-out page, including the cookie panel.

The parser-only baseline reported two `undefined is not a function` console
errors; the HMAC-enabled live run hydrates the editor, accepts native keyboard
input, enables the send arrow, and records neither error in the probe. The
15-minute instrumented session used 41.05 process CPU seconds, of which 27.8
had accumulated by 60 seconds. It included private input/cookie-dismissal
actions and a diagnostic probe, so it is not a pure idle or load benchmark.

Remaining limits are explicit: the app's separate `no idle gap found` rejection
comes from performance-entry analysis, while PerformanceObserver/resource
timing are still incomplete. HMAC coverage is not complete Web Crypto coverage:
non-byte-aligned keys and proper crypto-task-source completion remain follow-up
work. Mixed-style/multi-paragraph rich selection, the full pseudo typography
model, and percentage/auto CSSOM box edges need further conformance work.
Font selection/typography is not pixel-identical to LibreWolf: the sidebar's
14px, weight-600 invitation wraps to two lines in TRust but one in the
reference's 220px box. A reduced shaping diagnostic selects DejaVu Sans with
234.47px advance for that text. No site-specific font or width override was
introduced to hide the difference.
No account, chat submission, microphone, upload, or authenticated workflow was
exercised. Rendering the signed-out page is not proof of complete ChatGPT support.

## Automated verification

- Lumen library: **932 passed**, including six `for await` tests.
- TRust library after the complete corrections: **1,166 passed**,
  20 intentionally ignored, 2 previously known Wasm retention tests filtered.
  Test threads were serialized and a TTY supplied for terminal tests.
- Desktop adapter: **29 passed**. `full-tests-module-cache.log` and
  `desktop-tests-module-cache.log` are the final successful gates. The earlier
  `full-tests-final.log` caught the terminal pseudo-run spacing regression and
  must not be reported as passing; `full-tests-final-2.log` passed after its fix.
- The GPU bitmap regression renders 60 frames across 48px, 128px, and 320px
  emoji, checks colored pixels and retained atlas layer count. The broader
  serial rendering suite passed (47 passed, 1 ignored) before the final
  style/SVG additions; the full library run includes it again.

Both `target/release/trust` and `target/release/trust-desktop` were rebuilt from
the final source (`release-build-9.log`). SHA-256:

- Terminal: `cdef086fce4af3819dbf884538ffbc329facd2a92dea733eaad27a2708ff4ea8`.
- Desktop: `d24075340c69722183c1a8f4596162feb65cfebb31e25ede3c7c73525e0d38ba`.
  The frozen `module-cache-trust-desktop` is byte-identical. It is a release
  binary with symbols retained for profiling, not a debug build.

`git diff --check` passes in both repositories. No installation or promotion
was performed. All investigation-owned browser/compositor processes and the
local crypto-fixture server were stopped; their artifacts were retained.

## Standards basis (offline snapshots, not verified latest upstream)

The web-standards skill directed these changes to shared standards-governed
implementations and required the conformance-style regression cases. Relevant
normative clauses were read from Ruby's existing official-source library;
the library was not refreshed.

CSS sources: `w3c/csswg-drafts` checkout
`81c27f68690138345b2b3b6af8ccc42dad3dca1d`, fetched 2026-09-06.
These are living editor's drafts, not an assertion about a published edition.

- [Grid automatic minimum and intrinsic track sizing](https://www.w3.org/TR/css-grid-2/#min-size-auto):
  `/big/web-standards/repositories/w3c/csswg-drafts/css-grid-2/Overview.bs:1340`
  and `:4840`; growth/maximization at `:5189`.
- [Scrollable overflow](https://www.w3.org/TR/css-overflow-3/#overflow-properties):
  `css-overflow-3/Overview.bs:345`; [box sizing](https://www.w3.org/TR/css-sizing-3/#box-sizing)
  in `css-sizing-3/Overview.bs:820`.
- [Shorthand substitution](https://drafts.csswg.org/css-values-5/#substitution-in-shorthands):
  `css-values-5/Overview.bs:4810`.
- [Font-relative lengths](https://www.w3.org/TR/css-values-3/#font-relative-lengths):
  `css-values-3/Overview.bs:1155`. Cache stamps preserve these dependencies.
- [Consume a number](https://www.w3.org/TR/css-syntax-3/#consume-number):
  `css-syntax-3/Overview.bs:1610`, with start-of-number lookahead at `:1502`.
- [Overlapping corners](https://www.w3.org/TR/css-backgrounds-3/#corner-overlap):
  `css-backgrounds-3/Overview.bs:2602`; numeric constants/clamping in
  `css-values-4/Overview.bs:4345`.
- [NodeList](https://dom.spec.whatwg.org/#interface-nodelist):
  `/big/web-standards/repositories/whatwg/dom/dom.bs:3830`, snapshot
  `a2331a45360129e8645ef7e0a04740241b6e3726`, fetched 2026-09-06.
- [Module script graphs](https://html.spec.whatwg.org/multipage/webappapis.html#fetch-a-module-script-tree):
  `/big/web-standards/repositories/whatwg/html/source:118812`, optional
  modulepreload descendants at `:118855`, required graph linking at `:119085`,
  and single-module caching/fetching at `:119173`. Snapshot
  `e5071a20c8569d8a3ec02ed27dd01b948773f850`, fetched 2026-09-06.
- [For-in/of/await-of syntax and early errors](https://tc39.es/ecma262/#sec-for-in-and-for-of-statements):
  `/big/web-standards/repositories/tc39/ecma262/spec.html:22724`, snapshot
  `e28783d5fc9dc12b3de905961e2c71410b38a202`, fetched 2026-09-06.
- [SVG use style inheritance](https://www.w3.org/TR/SVG2/struct.html#UseStyleInheritance):
  `/big/web-standards/repositories/w3c/svgwg/master/struct.html:995`, also
  use shadow-tree requirements at `:841`, snapshot
  `c403ca46ad045ebdaeda148bae69f814fb744db7`, fetched 2026-09-06.
- [Line-height computation and inheritance](https://drafts.csswg.org/css-inline-3/#line-height-property):
  `css-inline-3/Overview.bs:947`; [CSSOM resolved values](https://drafts.csswg.org/cssom-1/#resolved-values)
  in `cssom-1/Overview.bs:3523`.
- [Non-widget appearance](https://drafts.csswg.org/css-ui-4/#appearance-switching):
  `css-ui-4/Overview.bs:2895`; [caret color](https://drafts.csswg.org/css-ui-4/#caret-color)
  at `:1380`; [generated pseudo content](https://drafts.csswg.org/css-pseudo-4/#generated-content)
  in `css-pseudo-4/Overview.bs:1411`.
- [HMAC operations](https://w3c.github.io/webcrypto/#hmac-operations):
  `/big/web-standards/repositories/w3c/webcrypto/spec/Overview.html:14455`,
  random methods at `:844`, algorithm normalization at `:3245`.
  Snapshot `811c24c69eb22d477af5f1678cb70dbc611c7c40`, fetched 2026-09-06;
  this is an editor's draft, not a claim of complete conformance to a published
  Web Crypto edition. Tests use [RFC 4231](https://www.rfc-editor.org/rfc/rfc4231.html#section-4.2)
  HMAC vectors, plus RFC 7517/7518 JWK constraints (local RFC text collection).
