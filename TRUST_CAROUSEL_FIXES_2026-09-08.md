# Carousel rendering and navigation fixes — 2026-09-08

## Scope and diagnosis

Public release-browser reproductions, using fresh disposable profiles and private
Wayland compositors. No user browser/profile was controlled. Site interaction was
limited to declining optional cookies and exercising carousel controls/hover.

Artifacts: [carousels-20260908-2ATKGU](benchmark-results/carousels-20260908-2ATKGU/).
`baseline-src.tar` preserves the pre-task source; existing pending changes were
not reverted. The saved release executables reproduce the user's failures.

### ShowBuzz and Second Life Marketplace: misplaced clipping

The carousel's `scrollLeft` really changed and the content remained in the DOM.
Card backgrounds and positioned badges painted, while ordinary text and images
vanished. On ShowBuzz, `.card.carousel-card:hover { z-index:2 }` established a
stacking context and temporarily restored the hovered card's content.

Anonymous line fragments have no DOM node. Their text/image pieces establish
their own ancestor scroll scopes. The graphical painter was pushing the line's
inherited hard clip **before** those scroll transforms, leaving the clip at the
card's unscrolled position. A stacking context happened to establish the scroll
scope earlier, explaining the hover-dependent difference.

The fix in `TRust/src/layout2/graphics.rs` scopes an anonymous line's clip around
each complete piece **after** establishing its scroll ancestry. This includes
the piece's hit region, not just its pixels. Named fragment and pseudo-box
clipping remains unchanged. Scrollports stay fixed; the descendant card's clip
moves with its contents. Scrolling still updates retained offsets without
rebuilding layout.

Baseline evidence:

- `showbuzz-before/`: both carousels advanced through approximately 216.8,
  433.6, and 650.4 CSS pixels. The `right-*` and `mouseout-*` screenshots show
  blank cards; `hover-*` restores only the hovered card.
- `marketplace-before-clear/`: Featured Items advanced through 627, 1254, and
  1881 CSS pixels. Its cards were blank after each advance. The hero carousel
  continued displaying its images normally.

### Steam: transparent slides incorrectly removed from layout

Steam's `CGenericCarousel.GetNextValidIndex` checks jQuery `:visible`, which uses
layout dimensions. The dots pass a slide index directly and bypass that scan.
The inactive slides use `opacity:0; pointer-events:none`, not `display:none`.

TRust's out-of-flow layout pass discarded transparent subtrees unless a narrow
hyperlink exception applied. Inactive hero slides therefore reported zero
rectangles despite `display:grid`. Native release clicks reached the correct
arrow elements, but the hero remained at index 0. A dot selected index 2.

The fix removes this box-generation shortcut in `layout2/flow.rs`. Transparent
positioned boxes now retain CSSOM geometry regardless of pointer eligibility.
The obsolete hyperlink-exception helper and its unused paint-model field were
also removed; paint-model construction no longer performs that recursive scan.

Terminal painting suppresses the complete zero-opacity group's visual
operations after layout while retaining non-painting hit operations in order.
This prevents newly retained transparent backgrounds or borders from erasing
visible terminal content. Desktop already represents group opacity explicitly.

No site-specific selectors or JavaScript workarounds were added to the browser.

## Standards consulted locally

CSSWG editor's-draft snapshot, commit
`81c27f68690138345b2b3b6af8ccc42dad3dca1d`, fetched 2026-09-06.
This is the recorded local snapshot, not a claim of a fresh upstream check.

- [CSS Overflow 3, scrolling](https://drafts.csswg.org/css-overflow-3/#scrolling):
  `/big/web-standards/repositories/w3c/csswg-drafts/css-overflow-3/Overview.bs:228`.
  The scrollport is the container's padding box; scrolling brings clipped
  content into view. Overflow values and programmatic scrolling were also
  checked at line 380 onward.
- [CSS Transforms 1, rendering model](https://drafts.csswg.org/css-transforms-1/#transform-rendering):
  `/big/web-standards/repositories/w3c/csswg-drafts/css-transforms-1/Overview.bs:180`.
  Coordinate mappings accumulate; ancestor clipping and descendant transforms
  must remain in their respective coordinate systems.
- [CSS Color 4, transparency](https://drafts.csswg.org/css-color-4/#transparency):
  `/big/web-standards/repositories/w3c/csswg-drafts/css-color-4/Overview.bs:668`.
  Opacity is a group compositing operation, not permission to omit layout.
- [CSSOM View, offsetWidth](https://drafts.csswg.org/cssom-view-1/#dom-htmlelement-offsetwidth)
  and [offsetHeight](https://drafts.csswg.org/cssom-view-1/#dom-htmlelement-offsetheight):
  `/big/web-standards/repositories/w3c/csswg-drafts/cssom-view-1/Overview.bs:1938`.
  These APIs measure generated border boxes. Zero opacity and pointer-events
  do not remove those boxes. The element-scroll algorithm was checked at
  line 1820 onward.

## Regression coverage

- `carousel_inline_paint_and_hits_survive_retained_scroll_and_hover`: actual
  headless production pixels, data-SVG image, text, a positioned badge, image
  link targeting, clipping outside the viewport, forward/backward offsets,
  hover/mouseout and an explicit assertion of no new layout passes on scroll.
- `carousel_inline_clips_follow_nested_scroll_coordinates`: horizontal and
  vertical nested scrolling through a transformed outer container, rectangular
  and rounded overflow clips, and one-axis clipping. All retained-scroll
  comparisons are pixel-identical to the reference.
- `transparent_positioned_carousel_slides_keep_cssom_geometry`: three slides
  retain 240×80 boxes and the correct offset parent; transparent fixed and
  ancestor-transparent positioned boxes retain their sizes; display:none still
  yields zero. A dimension-based arrow algorithm returns `1,2,0,2`.
- `opacity_zero_abspos_group_does_not_erase_terminal_content`: a transparent
  positioned layer and its opaque-background child leave underlying text intact.

Final full library run: **1,203 passed**, 20 ignored, two existing fresh-instance
WebAssembly memory-soak tests excluded. **29 desktop tests passed.**
Clippy completed with the same 14 existing warnings outside this change.
Scoped rustfmt and `git diff --check` passed. No browser JavaScript source was
modified.

## Release acceptance

Both release binaries built successfully (3m 50s). Build command:

```sh
env CARGO_PROFILE_RELEASE_STRIP=none cargo build --release --bin trust --bin trust-desktop
```

The final desktop runs selected the production Hybrid GPU renderer (NVIDIA
GB10). Actual pointer clicks, not scripted `.click()`, exercised the controls.

- `showbuzz-after/`: both carousels advanced three cards, retained their text
  and images through every mouseout, and moved backward correctly. Compare
  `mouseout-3.png` with the corresponding baseline ghost cards.
- `marketplace-after/`: three Featured Items advances and a backward move all
  painted the new cards, with no hover dependency. The hero remained functional.
- `steam-after/`: hero index **1 → 2 → 1** for right/left, then **2** via a dot.
  Offers index **0 → 1 → 0** for right/left. All 12 hero slides retained width
  860px, and all four offers slides retained width 774px, including inactive
  transparent slides. Screenshots confirm the selected images/content changed.
- `steam-terminal-after/`: native left/right navigation changed the hero and
  offers selection; the offers returned **0 → 1 → 0**. This is navigation
  validation, not a claim of complete terminal media/pixel parity. The first
  attempted terminal baseline did not navigate to HTTP and is excluded from
  comparisons.

`verify.mjs` checks the saved scroll/selection/geometry evidence. Screenshot
inspection and the exact-pixel headless regressions additionally check painting.
The live-site traces contain no uncaught JS throws, parse errors, or panics in
these bounded runs. This does not assert absence of every unrelated site issue.
Steam's bottom cookie notice covered its offers dots during the test; the
offers **arrows** and the hero dots were the controls verified there.

Final SHA-256:

- `target/release/trust`:
  `5dccf0d9492df0b515feac97329922867300e02210548dcf651c222b4a22812b`
- `target/release/trust-desktop`:
  `8dc67ccf61d0435b4014276955a90e73cf0453a68e989f93eeb4d617bc16e62a`

At completion of the fix, no installed executable had been replaced and no
commit or push had been performed.

## User-approved production promotion (2026-09-08)

After the user's own testing and explicit approval, the release build command
above completed again in 0.40s without rebuilding. Both approved executables
were installed to `/home/ruby/.local/bin/`; `cmp` verified byte identity with
the release artifacts, whose SHA-256 values are recorded above.

Previous production executables were backed up, preserving permissions, to
`/home/ruby/.local/state/trust/release-backups/approved-20260908-QushrW`.
Already-running browser processes were not restarted.

Source revisions:

- Lumen engine: `8b456e1c3d544111af3dae058becef11f8ce6bb9` (nested
  `for await` parser fix; all six focused `for_await` tests passed again).
- TRust approved foundation: `f66c4bbb437e0f34fa60a17b6d81632066462d2f`.
- TRust carousel fixes and tested-engine revision documentation:
  `036764c349637dad62c5f2926767fc745f0cad27`.

Promotion destinations are Lumen's `private/main` and TRust's `origin/main`;
these are the configured upstreams, not Lumen's public `origin` remote.
