# ChatGPT submission and pricing layout — 2026-09-08

## Findings and changes

### Submission stops before the conversation request

The original release reproducibly failed in the site's upload-capability check:
`TypeError: cannot read property 'split' of undefined`, with the stack beginning
`RXe → GXe → vQt`. The relevant public asset was
`/cdn/assets/4813494d-o593jrji51wy4azk.js`, offset 936705: it reads the `accept`
property of the `#upload-photos` file input while preparing a message.

TRust had the content attribute but no `HTMLInputElement.accept` IDL reflector.
Consequently, even a plain text message failed before its conversation POST.
The fix implements raw DOMString reflection, including the empty default,
attribute synchronization, prototype descriptor, receiver checks, and Web IDL
string conversion. It does not change the site's code or bypass its checks.

Implementation: [js_platform.js](/big/Code/TRust/src/js_platform.js:4612).
Regression: [lumen_backend.rs](/big/Code/TRust/src/lumen_backend.rs:11924).

### Pricing cards overlap because Grid sizing is incomplete

The pricing grid uses `auto auto 1fr` rows and cards that span three rows with
`grid-template-rows: subgrid`. There were two independent omissions:

- Flexible tracks in an indefinite-height grid omitted spanning items'
  max-content contributions. In this case the first card got zero height.
- Row subgrids were treated as independent grids, so their descendants did not
  contribute to parent row sizing or inherit final parent track positions.

The shared layout engine now accounts for those max-content contributions and
supports row-subgrid contribution propagation and final positioning. This
includes nested row subgrids, padding/border/margin edges, gutter differences,
placement clamping, containment fallback, and resolved CSSOM serialization.

Subgrids cannot be independent incremental-layout boundaries. Fragment reuse is
also disabled inside an active inherited-row constraint context, whose track
positions are not part of the old independent-fragment cache key. A regression
changes the row distribution without changing the outer box size and compares
warm/cold geometry, track sizes, and paint.

Implementation/tests: [grid.rs](/big/Code/TRust/src/layout2/grid.rs:639),
[dom.rs](/big/Code/TRust/src/dom.rs:1596),
[flow.rs](/big/Code/TRust/src/layout2/flow.rs:2828).
Stylesheet `contain` is now retained in the tracked property registry; otherwise
the required `contain:layout` fallback was silently lost.

Live diagnostic geometry, in CSS pixels:

| Viewport | Original release | Fixed release |
| --- | --- | --- |
| 960 px | Free height 0; Free and Go both start at y=529.98 | Free height 577.71; Go begins at Free's bottom, y=1107.69 |
| 1280 px | — | All four cards share y=538.63 and height 802.31; corresponding rows align |

These are observed live-page measurements, not a pixel-identical golden image:
server experiments, copy, fonts, themes, and scrollbar widths can differ.

### Additional terminal editing issue

The terminal check exposed a separate interaction omission: preserving a rich
editor's authored block descendants left their text carrying only a page-click
action, or no action, instead of the terminal editing binding.

Terminal paint metadata now consumes the already-extracted control map. Editing
hosts retain full-box activation surfaces; editable descendant text retains its
editing action without changing CSS geometry or authored paint. Non-editable
islands stop inheritance, and existing native control/link actions remain intact.
Pages without editing hosts take an immediate fast-path return.

Implementation: [paint.rs](/big/Code/TRust/src/layout2/paint.rs:96).
The [native-interaction regression](/big/Code/TRust/src/app.rs:11762) covers both
placeholder text and blank box space, in fixed and ordinary positioned content.

## Validation

- Final optimized library suite: **1,193 passed**, 20 existing ignores, two known
  Wasm accumulation tests explicitly excluded. Tests ran under a PTY.
- Desktop tests: **29 passed**.
- Clippy succeeds with 14 existing warnings; no new warnings in these changes.
- Scoped Rust formatting, JavaScript syntax checking, and `git diff --check` pass.
- Live testing uses normal optimized release binaries, fresh signed-out profiles,
  private headless compositors, and benign `hello` messages. No personal profile
  or signed-in account is used. Network probes record request metadata, not
  credentials or request/response bodies.
- Release desktop testing confirmed a successful conversation POST (HTTP 200,
  `text/event-stream`) and a rendered assistant reply, not just a spinning icon.
- Final terminal release acceptance succeeded: native click opened its editor;
  typing `hello` and Enter committed the text, then activating Send produced a
  conversation POST (HTTP 200, `text/event-stream`) and a visible assistant reply.
  Terminal Enter retains its existing edit-commit convention; this was not a
  change to its keyboard model.
- The final desktop release also displayed a reply in a **clean run without any
  injected probe or prelude override**.
- In the final release, resizing the same pricing page from **960 to 1280 px**
  preserved non-overlapping stacked cards and correctly aligned shared rows.

Release build: normal optimized profile, 3m58s. Artifacts ready for user testing:

| Binary | SHA-256 |
| --- | --- |
| `/big/Code/TRust/target/release/trust` | `499f8fdfffc90302dd89c82ac952ce2f8d7b65c53005ab2a0e7d31cf6faa318d` |
| `/big/Code/TRust/target/release/trust-desktop` | `0a6cbc37e97654f2a954e3fa553c94bc6afb55a97d329f5e564bbc9b31287288` |

Final screenshots: [terminal reply](/big/Code/Lumen/benchmark-results/chatgpt-submit-pricing-20260908-d8X8Vu/terminal-release/at-90.png),
[clean desktop reply](/big/Code/Lumen/benchmark-results/chatgpt-submit-pricing-20260908-d8X8Vu/submit-release-clean/at-90.png),
[narrow pricing](/big/Code/Lumen/benchmark-results/chatgpt-submit-pricing-20260908-d8X8Vu/pricing-release-resize/at-45.png),
[pricing after resize](/big/Code/Lumen/benchmark-results/chatgpt-submit-pricing-20260908-d8X8Vu/pricing-release-resize/at-90.png).

Artifacts, screenshots, probes, source snapshots, and logs:
[diagnostic directory](/big/Code/Lumen/benchmark-results/chatgpt-submit-pricing-20260908-d8X8Vu).
The directory also includes an isolated `this-turn.patch` against the source
snapshot taken at turn start, excluding earlier unrelated pending work.
Early `terminal-prelude`, `terminal-final`, and `terminal-accepted` directories
are unsuccessful input-harness/diagnostic attempts, **not** passing acceptance
runs. `terminal-release` is the final verified run.

## Standards used

The web-standards skill guided these general fixes and their edge-case tests.
Only the locally downloaded official sources were consulted for standards;
their snapshot date is **2026-09-06**, not a claim of current upstream freshness.

- HTML, commit `e5071a20c8569d8a3ec02ed27dd01b948773f850`:
  [input IDL](/big/web-standards/repositories/whatwg/html/source:49696),
  [reflection algorithms](/big/web-standards/repositories/whatwg/html/source:8544),
  [click focus](/big/web-standards/repositories/whatwg/html/source:86313), and
  [editing hosts](/big/web-standards/repositories/whatwg/html/source:88235).
  Official clauses: [accept](https://html.spec.whatwg.org/multipage/input.html#dom-input-accept),
  [reflection](https://html.spec.whatwg.org/multipage/common-dom-interfaces.html#reflect),
  [click focus](https://html.spec.whatwg.org/multipage/interaction.html#click-focusable).
- Web IDL, commit `8f182624f632a0ce485e236edbc1df18ca385b1d`:
  [DOMString conversion](/big/web-standards/repositories/whatwg/webidl/index.bs:7821),
  [prototype attributes](/big/web-standards/repositories/whatwg/webidl/index.bs:12288).
  Official: [DOMString](https://webidl.spec.whatwg.org/#js-DOMString),
  [attributes](https://webidl.spec.whatwg.org/#js-attributes).
- CSSWG editor's drafts, commit `81c27f68690138345b2b3b6af8ccc42dad3dca1d`:
  [subgrids](/big/web-standards/repositories/w3c/csswg-drafts/css-grid-2/Overview.bs:3555),
  [flexible tracks](/big/web-standards/repositories/w3c/csswg-drafts/css-grid-2/Overview.bs:5211),
  [layout containment](/big/web-standards/repositories/w3c/csswg-drafts/css-contain-2/Overview.bs:993),
  [appearance](/big/web-standards/repositories/w3c/csswg-drafts/css-ui-4/Overview.bs:2845).
  Official: [subgrid sizing](https://drafts.csswg.org/css-grid-2/#subgrids),
  [flex-track algorithm](https://drafts.csswg.org/css-grid-2/#algo-flex-tracks),
  [containment](https://drafts.csswg.org/css-contain-2/#containment-layout),
  [appearance](https://drafts.csswg.org/css-ui-4/#appearance-switching).

## Boundaries and remaining work

This is not a claim of complete ChatGPT or CSS Grid conformance. Column subgrids,
named-line placement, baseline shims, and automatic subgrid spans inferred from
local line-name counts remain separate work. Existing independent console
warnings (`no idle gap found`, KaTeX quirks-mode warning) still appear in the
successful desktop run. The conversation header also has a remaining Share/login
overlap. Those did not prevent the observed reply.

Network responses are still buffered before exposure to script: this change
unblocks submission, not true incremental network streaming. These diagnostic
runs are not throughput benchmarks. Existing unrelated worktree changes were
preserved; no installed executable, commit, or remote branch was changed.
