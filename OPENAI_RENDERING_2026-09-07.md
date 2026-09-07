# OpenAI article rendering — 2026-09-07

Affected page: <https://openai.com/index/research-acceleration-view-inside-openai/>.
This round addresses rendering, not Lumen throughput or further Photopea work.

## Findings and corrections

1. **Oversized header SVGs:** SVG `width`/`height` presentation attributes were
   missing from the canonical cascade. In a 40×40 flex button an explicitly
   16×16 SVG consequently used the SVG auto/100% fallback and became 40×40.
   The cascade now includes supported SVG geometry presentation attributes below
   author stylesheets, with namespace and element eligibility checks. Layered
   author CSS and explicit `auto` still override the attributes.
2. **Navigation over the article:** the live closed
   `#header-mobile-drawer-panel` uses `clip-path: inset(0 0 100% 0)`. TRust did
   not track or paint that property. The resulting full-size fixed drawer
   obscured the article. `inset()` clipping now survives cascade serialization,
   creates its required stacking context, and clips the complete desktop
   context, including backgrounds and interaction regions. It does not create
   a containing block or change canonical layout geometry. A fixed-descendant
   test also caught and corrected an escape through the viewport-underlay path.
3. **Blurry external-link arrows:** the live drawer arrows have XML `width="9"`
   but winning CSS `width:18px;height:auto`. The decoder used the old 9-pixel
   width, so the resulting bitmap was enlarged. The square reduced test exposes
   the same failure as 9×9 instead of 18×18; the live arrows retain their
   non-square `viewBox` ratio.
   Isolated SVG resources now carry resolved definite CSS dimensions. Math and
   font/viewport units are resolved; overridden XML dimensions cannot remain
   intrinsic dimensions when CSS instead supplies `auto` or percentages.

The terminal adapter also suppresses the closed drawer. Rounded desktop clips
have ellipse-aware hit testing rather than bounding-box-only targeting.
There are no hostname-specific rendering rules or page-script overrides.

## Verification

- The three original reduced tests failed before implementation: 40×40 instead
  of 16×16, hidden drawer click interception, and a 9×9 instead of 18×18 raster.
- Ten focused regressions pass: sizing/cascade/namespace, resource sizing and
  overrides, inset math/validation, rounded paint/hits, serialization/reopening,
  closed terminal drawer, and clipping fixed descendants without changing their
  containing block.
- Final non-backend library run: **965 passed, 0 failed, 17 ignored** (125
  backend tests filtered out). This includes the existing absolute-SVG ratio
  regression, which caught the one-definite-dimension interaction and now passes.
- Initial complete library run: 1,087 passed, 17 ignored, three failures: the
  subsequently corrected SVG ratio interaction and the two previously known
  fresh-instance Wasm retention failures (+2,048 objects each).
- Final backend check: **123 passed, 0 failed**, explicitly excluding the two
  known fresh-instance Wasm retention failures. Across the two final test runs,
  1,088 tests pass, 17 are ignored, and those two known failures remain excluded.
- `cargo clippy --offline --locked -j2 --lib` succeeds with ten warnings in
  unchanged code; targeted whitespace checks and formatting of the new modules
  are clean.
- Release build succeeds for **trust, trust-headless, and trust-desktop**:
  `CARGO_PROFILE_RELEASE_STRIP=none cargo build --offline --locked --release -j2 --bin trust --bin trust-headless --bin trust-desktop`.
  All three targets took 10m51s together; peak build memory was 4.4 GiB, no swap.
- **Live release desktop:** the 15-, 60-, and 75-second screenshots show the
  article with no closed-navigation overlap and correctly sized header icons.
  The click trace reports the panel icon's actual 18×18 box (not its 40×40
  parent). The drawer's CSS clip survives into the captured DOM.
- **Live interaction limitation:** both attempted drawer clicks reached the
  intended button, but the drawer did not open. Both baseline and candidate
  logs contain the same pre-existing script parse error in
  `/_next/static/immutable/chunks/1pdr3tr8u1jm4.js`, line 132: `expected ':'`.
  This is evidence of a separate scripting failure, not proof of its complete
  causal chain. Live drawer open/close acceptance and visual inspection of its
  newly sized arrows are therefore **not claimed**. The reduced reopening,
  clipping, and SVG raster-resolution tests pass. That scripting issue was not
  changed in this rendering round.

Baseline: actual release `trust-desktop`, SHA-256
`4b2c5212b63f0821909c1d6660fcc46071ffa92534d3bb09296d5cfbfde2611d`.
The private headless Wayland compositor used a 1280×720 output and the production
renderer selection (CPU fallback in this environment). After 180 seconds the
closed drawer still covered the article; this was not a slow SVG load.

Candidate release SHA-256:

| Target | SHA-256 |
| --- | --- |
| trust | `5e67e3137e1f821fd579bba68448728d81cf7507b0e8dd9058c84e02cb6c5a03` |
| trust-headless | `7b7e4f9a65d29925f2bf961ec701ba1ea31b30582b000aabde63cfac690e5c0e` |
| trust-desktop | `8a07cbbb26fdadc7a2424156d4442a19afd0a3fb593374f27296fee9e041a46f` |

Artifacts:

- [Baseline screenshot](benchmark-results/openai-desktop-baseline-20260907/desktop-180s.png)
- [Baseline metadata](benchmark-results/openai-desktop-baseline-20260907/metadata.json)
- [Corrected release screenshot](benchmark-results/openai-desktop-fixed-20260907/desktop-75s.png)
- [Corrected release metadata](benchmark-results/openai-desktop-fixed-20260907/metadata.json)
- [Live click and script diagnostics](benchmark-results/openai-desktop-fixed-20260907/desktop.log)
- [Captured DOM](benchmark-results/openai-desktop-baseline-20260907/renders/render_000001.html)
- [Full library test log](benchmark-results/openai-layout-20260907-9hirtd/lib-tests.log)
- [Final rendering/library tests](benchmark-results/openai-layout-20260907-9hirtd/rendering-tests-final.log)
- [Final backend tests](benchmark-results/openai-layout-20260907-9hirtd/backend-tests-final.log)
- [Clippy log](benchmark-results/openai-layout-20260907-9hirtd/clippy.log)
- [Release build log](benchmark-results/openai-layout-20260907-9hirtd/release-build-final.log)

## Standards used

The web-standards skill routed this work through the local official-source
library. No standards-server refresh or crawl was needed. Sources are upstream
editor-source snapshots, not a claim of freshly verified published editions.

- SVGWG snapshot `c403ca46ad045ebdaeda148bae69f814fb744db7`, fetched 2026-09-06:
  [presentation attributes](https://www.w3.org/TR/SVG2/styling.html#PresentationAttributes),
  local [styling.html](/big/web-standards/repositories/w3c/svgwg/master/styling.html:303);
  [sizing properties](https://www.w3.org/TR/SVG2/geometry.html#Sizing),
  local [geometry.html](/big/web-standards/repositories/w3c/svgwg/master/geometry.html:461);
  [SVG intrinsic sizing](https://www.w3.org/TR/SVG2/coords.html#SizingSVGInCSS),
  local [coords.html](/big/web-standards/repositories/w3c/svgwg/master/coords.html:1273).
- CSSWG snapshot `81c27f68690138345b2b3b6af8ccc42dad3dca1d`, fetched 2026-09-06:
  [clipping model](https://drafts.csswg.org/css-masking-1/#clipping-paths) and
  [clip-path](https://drafts.csswg.org/css-masking-1/#the-clip-path), local
  [CSS Masking 1](/big/web-standards/repositories/w3c/csswg-drafts/css-masking-1/Overview.bs:234)
  (paint/geometry), line 328 (pointer targeting), and line 530 (reference boxes
  and stacking contexts);
  [basic shapes](https://drafts.csswg.org/css-shapes-1/#supported-basic-shapes),
  local [CSS Shapes 1](/big/web-standards/repositories/w3c/csswg-drafts/css-shapes-1/Overview.bs:333);
  [corner overlap](https://drafts.csswg.org/css-backgrounds-3/#corner-overlap),
  local [CSS Backgrounds 3](/big/web-standards/repositories/w3c/csswg-drafts/css-backgrounds-3/Overview.bs:2602).

## Scope and acceptance limits

This implements the `inset()` basic shape, including rounded corners, for CSS
border boxes and their `stroke-box`/`view-box` aliases. It is not complete CSS
Masking: other basic shapes, URL clip sources, additional reference boxes, and
clip-path animation/interpolation remain follow-up work. Stylesheet feature queries do not
advertise those unimplemented shapes. Terminal rounded clips currently use
their bounding rectangle; the user's closed rectangular drawer is covered.
The SVG correction is not a general device-scale/transform-aware vector cache.

The user's GPU renderer and exact 960×1050 window have not been verified in the
private compositor. No installed executable was overwritten, and no source was
staged, committed, or pushed in this round. Existing unrelated worktree changes
are preserved.
