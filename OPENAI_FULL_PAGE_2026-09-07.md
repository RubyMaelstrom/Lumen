# OpenAI article: full-page rendering investigation (2026-09-07)

Target: <https://openai.com/index/research-acceleration-view-inside-openai/>.
This extends `OPENAI_RENDERING_2026-09-07.md`; it does not replace that earlier
round's measurements. No site-specific rendering or JavaScript overrides were
added. Both repositories had substantial pre-existing changes, which were
preserved. At the end of rendering acceptance, nothing had been installed,
promoted, committed, or pushed. The subsequent user-authorized checkpoint
records the Lumen parser/JIT fixes in `d1b7c6e` and the preceding Photopea engine
work in `6905670`. The browser working-tree checkpoint is separate; no installed
executable has been replaced.

## Confirmed causes and general fixes

| Symptom | Cause | Fix |
| --- | --- | --- |
| A large JavaScript bundle fails to parse | A postfix update followed by division was lexed as a regular expression | Track the update operator's lexical context, including line terminators and template interpolation |
| Blue links, missing button fills, incorrect fonts | Escaped quotes in Tailwind selectors were treated as CSS string delimiters; a layer swallowed following stylesheet rules | Honor escapes outside quoted strings when splitting CSS blocks and declarations |
| Missing chapter navigation, full-width article, stacked footer | Size-container query rules were not being applied to the relevant ancestor container | Retain query conditions on rules and evaluate size queries from content-box layout geometry, with size containment and bounded style/layout stabilization |
| Related still-image cards are not clickable | Their links use an absolutely positioned empty `::after` box covering the card; pseudo style variables and hit regions were incomplete | Resolve pseudo-element variables in their own cascade and include the generated box in the originating link's hit testing |
| Button corners and sticky offsets differ | Geometry paths used bare numeric parsing for CSS lengths | Resolve radii and insets using typed CSS length units; normalize overlapping radii |
| Chapter list remains over the footer when scrolling | Sticky translations were not limited to the containing block | Constrain the sticky position box to its containing block, including both inset edges and oversized sticky view rectangles; share paint and hit-test transforms |
| Consent dismissal leaves React transitions suspended | Next's router requested same-URL `location.replace`; TRust incorrectly treated a fragmentless identical URL as a fragment no-op | Require a non-null target fragment for fragment navigation; emit replacement navigation for an identical fragmentless URL |
| Scrolling toward charts crashes the page actor | A function-keyed JIT call-cache retry used the current closure's environment but a previous closure's identity in `FnFrame`; stack capture dereferenced freed object data | Put the live callee identity in the local cache-entry copy used for the active call, without disabling code sharing or changing the cache's weak address pins |
| Vega initialization throws while inspecting its first child | `element.textContent = ''` inserted an empty Text node; Vega then read that node's nonexistent `tagName` | Implement DOM string-replace-all: no child for the empty string, a fresh Text node for nonempty strings, correct combined mutation records, and no Document/DocumentType setter effect |
| Inherited theme tokens read as empty through CSSOM | Untracked property getters did not inherit custom properties; internal substitution also rebound inherited references against descendant variables | Expose inherited computed custom-property values, retaining epoch-scoped memoization, case-sensitive names, invalid-value fallbacks, and CSS-wide keyword handling |
| Chapter/article gutters remain missing | `:not(.container .container)` was rejected because only compound arguments were accepted | Parse and match strict complex selector lists, with argument specificity and nested dependency/hover tracking |
| Still images have square corners | Backgrounds and borders were rounded, but replaced pixels and overflowing descendants were not clipped to the corresponding curves; the actual cards put the radius on an overflow-hidden ancestor | Clip replaced content to the curved content edge and overflow descendants to the curved padding edge. Establish ancestor clips outside descendant transforms, share clips with hit testing, and follow positioned containing-block ancestry |
| Chart headings split ordinary words before an em dash | The line composer found a legal punctuation break but did not check whether that normal segment fit before enabling emergency breaks | Prefer the preceding space when the next normal segment exceeds the remaining width; retain emergency wrapping for genuinely overlong words |

The chart areas are not missing downloaded PNGs: they are lazy JavaScript-rendered
charts. Intersection observers did deliver visibility changes. Two independent
failures were hiding one another: consent dismissal could suspend React before
chart execution; leaving consent untouched allowed the chart bundle to execute
and exposed the native crash. Fixing the crash then exposed the empty-Text-node
bug in Vega's SVG renderer initialization.

Release 6 (`trust-desktop` SHA-256
`f4f7ffbb6c61826697e28a3a44a681e7174932cd180b1c980c61e34af6ea73bd`)
creates and visibly paints the chart axes, labels, data lines, and controls
without any page behavior shim. The private `vega-native6` run captures error
formatting for diagnosis but does not alter DOM text-clearing or chart styles.
An earlier empty-text diagnostic override was used only to discover downstream
behavior while the native fix compiled; it is not shipped or used as acceptance.

The initial blank plots after SVG creation were still in the site's temporary
`visibility:hidden` initialization state. That state subsequently cleared.
Although the CSSOM theme getter defect is real, the native SVG serializer was
already resolving inherited paint variables correctly; it was not the cause of
those temporarily invisible pixels. Offline serialized HTML drops some paint
context and must not be confused with the canonical live DOM rendering path.

The middle related-media card is an MP4 with no poster. Its lack of a decoded
video frame is separate from the hit-testing defect affecting the two still
images. This round does not implement video decoding.

## Engine crash evidence

An isolated release process was stopped at its first native fault using a local
diagnostic signal handler. Its saved binary has SHA-256
`68a0a80b90848e67714f6fbf7dc72b2bcabcda21e8263324ddc7a5aee8cec1c7`.
The faulting instruction was in `Props::get`, called from
`Interp::capture_stack` / `Interp::make_error`, while a native ArrayBuffer
operation was reporting a TypeError through Reflect. The fault stack, register
record, exact executable, and release logs are retained under the artifact
directory below. The signal handler and source probes are diagnostics only.

`fresh_closure_call_cache_uses_the_live_function_identity` first reproduced the
wrong identity deterministically while keeping the old closure alive:
interpreter and bytecode passed; JIT failed. The identity correction made all
three tiers pass. Expanded coverage drops the cached closure, creates fresh
closures and allocation churn, and catches native ArrayBuffer TypeErrors while
checking that stacks name the live closure. It also asserts that the tested
caller actually populated a needs-environment JIT cache entry.

## Verification

- Lumen library suite: **930 passed, 0 failed**, 57.32 seconds (test execution).
- TRust library suite including the final rounded ancestor-overflow and
  punctuation-wrapping changes: **1110 passed, 0 failed, 19 ignored**,
  82.29 seconds (test execution).
  Two existing allocation-accumulation tests were explicitly excluded:
  `fresh_sync_wasm_instances_do_not_accumulate` and
  `fresh_async_wasm_instances_do_not_accumulate`.
- Focused checks cover lexical goals, CSS escaping, pseudo custom-property
  inheritance, generated link hit regions and exclusions, container ancestry /
  dimensions / resize invalidation, rounded geometry, sticky movement and hit
  testing, Location fragment versus document navigation, and actor replacement
  events. New DOM coverage checks child-node identity and MutationObserver
  records for equal and empty text, Element/Fragment/SVG children, CharacterData,
  and Document no-op behavior. Custom-property and complex-selector regressions
  include inheritance, invalid values, mutation invalidation, combinators,
  specificity, and invalid argument rejection. Rounded overflow tests cover
  hidden/clip/auto/scroll, a visible axis, transformed descendants, pixel and
  hit clipping, and absolute/fixed containing-block escape cases.
- `git diff --check` passes for Lumen and the affected browser source paths.
- All live browser checks use **release** artifacts, in an isolated fixed-size
  headless Wayland compositor. LibreWolf uses its own isolated profile and a
  WebDriver BiDi connection, not the user's running browser.
- Both still-image cards have been clicked successfully in isolated release
  runs (`candidate/image-click-navigation.png` and
  `combined-acceptance/third-image-navigation.png`). The middle video is not
  responsible for either still-image link failure.
- Release 8 additionally verified the first image's navigation to
  `/index/an-alien-mind/`. In release 10, clicking **(780, 300)** inside the
  right-hand image hit the generated link box and dispatched navigation to
  `/index/safety-overview-gpt-6-astra/`; the destination returned a Cloudflare
  challenge. This verifies the image click/navigation, not completion of that
  separate destination's challenge (`third-image-navigation.png`).
- Release 6 produced all **13 chart SVGs** after scrolling; screenshots show
  data plots in multiple article sections, without Vega initialization errors.
- Release 8 also produced all **13 chart SVGs**. The first chart's x position
  and width are now **182px / 272px**, matching the captured LibreWolf values;
  the chapter gutter is **32px**. Inherited theme reads on SVG descendants
  return the actual color instead of an empty value.
- Release 10's first chart measures **272 × 383.255px**, versus the captured
  reference's **272 × 383.267px**, at the same **x=182px**. Its heading now
  wraps at word boundaries into the same four-line structure as the reference;
  the approximately 28px height discrepancy in release 8 is gone.
- The header panel now responds to both open and close clicks in release 6
  (`native-panel-interaction.png`, `native-panel-closed.png`), resolving the
  script-blocked interaction noted in the earlier rendering report. This is an
  interaction check, not a claim of pixel-identical open-drawer layout.
- Release 10 completed the article scroll-through with all **13 populated
  chart SVGs**, none hidden, and no native crash or Vega initialization error.
  `final-acceptance10/first-charts.png`, `charts-section-two.png`, and
  `charts-section-five-settled.png` show plots in multiple sections. The last
  screenshot was taken after the site's temporary hidden initialization state
  cleared; the preceding `charts-section-five.png` records that transient state.
- `final-acceptance10/cards-footer.png` verifies rounded still-image corners,
  the five-column footer, 13px footer links, and no overlapping chapter list.
  Pixel samples at image corners are the page background while adjacent
  interior pixels retain image color. Release 9 was stopped before completion
  after inspecting the actual card's overflow-hidden ancestor; release 10
  includes both ancestor clipping and punctuation wrapping.
- Final browser Clippy completes with 10 existing warnings outside the changes in
  this round; Lumen Clippy is clean. A pipe-only test invocation could not
  initialize a terminal-dependent framebuffer fixture; the full suite above
  was rerun with a private PTY, without changing that test.

Release builds are resource bounded (14 GiB, no swap); tests and private TRust
instances have separate 8 GiB, no-swap limits. Debug binaries were used only for
unit tests, not live page acceptance or timing.

Final combined build (release 10): 8m19s wall time, 7.7 GiB peak memory,
zero swap. Command: `/usr/bin/cargo build --offline --locked --release -j3
--bin trust --bin trust-headless --bin trust-desktop`, with
`CARGO_PROFILE_RELEASE_STRIP=none` to retain diagnostic symbols.

| Target | SHA-256 |
| --- | --- |
| trust | `967659b4c94d9098ce234ea9eafe51e957b9d3e76ec997a20de83cdcbad65404` |
| trust-headless | `8dc2fef724ec34da23fab3325e4d9f2825ae2506cd04a5c6c06c69b943fdde52` |
| trust-desktop | `b2d8503b3e63ab48094f3c5a93dc805417f691f20f678217e1b7eeb9a58675e7` |

## Standards basis

The `web-standards-skill` directed this work to the local official sources; no
bulk refresh or standards-host crawl was needed. The following are the recorded
2026-09-06 snapshots, not claims about a newly fetched upstream revision.

- ECMA-262 `e28783d5fc9dc12b3de905961e2c71410b38a202`:
  `spec.html:16880` (lexical goals), `20405` (update expressions), and
  `13860` ([PrepareForOrdinaryCall](https://tc39.es/ecma262/#sec-prepareforordinarycall)).
  Each call's Function component identifies the actual function object.
  `Error.stack` is an implementation extension using that frame information.
- CSSWG drafts `81c27f68690138345b2b3b6af8ccc42dad3dca1d`:
  `css-syntax-3/Overview.bs:1399` (escapes), `3110` (simple blocks);
  `css-pseudo-4/Overview.bs:1385` (generated boxes);
  `css-variables-1/Overview.bs:87,220,669` (custom properties and substitution);
  `css-cascade-5/Overview.bs:1801` (computed-value inheritance);
  `cssom-1/Overview.bs:3380` (getComputedStyle custom properties);
  `selectors-4/Overview.bs:1559,4442`
  ([complex negation](https://drafts.csswg.org/selectors-4/#negation));
  `css-text-3/Overview.bs:4643`
  ([overflow wrapping](https://drafts.csswg.org/css-text-3/#overflow-wrap-property));
  `css-conditional-5/Overview.bs:467,591,1023,1290` (container selection,
  types, queries, size features);
  `css-contain-2/Overview.bs:574,837` (size containment);
  `css-backgrounds-3/Overview.bs:2267,2357,2423,2525,2602`
  ([radii and corner clipping](https://drafts.csswg.org/css-backgrounds-3/#corner-clipping));
  `css-overflow-3/Overview.bs:150,232,354,506,580`
  ([overflow corner clipping](https://drafts.csswg.org/css-overflow-3/#corner-clipping));
  `css-position-3/Overview.bs:233,447,617`
  ([sticky positioning](https://drafts.csswg.org/css-position-3/#stickypos-insets));
  `css-ui-4/Overview.bs:2444` (pointer events), and
  `cssom-view-1/Overview.bs:1065` (hit testing).
- WHATWG HTML `e5071a20c8569d8a3ec02ed27dd01b948773f850`:
  `source:99209` (Location hash), `99309` (Location replace),
  `108238` ([navigation history handling](https://html.spec.whatwg.org/multipage/browsing-the-web.html#navigate-convert-to-replace)),
  and `108300` ([fragment navigation gate](https://html.spec.whatwg.org/multipage/browsing-the-web.html#navigate-fragid-step)).
- WHATWG DOM `a2331a45360129e8645ef7e0a04740241b6e3726`:
  `dom.bs:3240` (replace all), `4760–4855`
  ([textContent and string replace all](https://dom.spec.whatwg.org/#dom-node-textcontent)),
  and `8342` (CharacterData replacement).

Local repositories are under `/big/web-standards/repositories/`:
`tc39/ecma262`, `w3c/csswg-drafts`, `whatwg/html`, and `whatwg/dom` respectively.

## Scope and comparison cautions

This is not a claim of complete CSS Conditional 5 or CSS Position 3 conformance.
Container style/scroll-state queries and more complex vertical-writing/grid
containing-block cases require their own implementation and coverage. The
Location change is scoped to navigation dispatch; it does not finish every
Location/history edge case.

TRust currently advertises a dark color-scheme preference; isolated LibreWolf
advertises light. With the stylesheet now parsed correctly, the site honors
that difference. Reference `innerWidth` is 960 but its content area is 948 pixels
because of its scrollbar; later private TRust runs use 948 pixels to match
that usable width. Earlier 960-pixel runs also straddled a 60rem container
breakpoint. Theme and usable-width differences
must be accounted for before interpreting screenshot differences as defects.

The reference browser also logs external-response/analytics errors, including
JSON parse failures. A blanket "zero console errors" claim would not be an
accurate acceptance criterion. Engine syntax failures, crashes, missing content,
and incorrect hit testing remain actual browser defects and are treated as such.

Artifacts (ignored, including fetched site assets and diagnostic programs):
`benchmark-results/openai-complete-20260907-mLQnbA/`.
