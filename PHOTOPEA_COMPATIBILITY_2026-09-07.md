# Photopea compatibility and pixel-buffer work

Status: **headless release import, rotation, painted-document and pixel-exact PNG export
acceptance passed** for the small probe and a 1024×768 input. The larger run's
five-minute observation completed without a reported JS panic or delayed failure.
The user's subsequent desktop blank-page report was reproduced with the stale
desktop binary: it still threw `ReferenceError: ImageData is not defined`.
Rebuilding `trust-desktop` separately restores the editor start screen and the
interactive New Project dialog in an isolated desktop CPU-renderer check.

## Reproduced failures and implemented changes

1. `ImageData` was missing. TRust now implements its constructors, private brand,
   readonly attributes, Uint8ClampedArray/Float16Array storage, settings conversion,
   and structured serialization in Window and Worker realms. Same-Agent foreign
   realm getters and garbage collection are covered by tests.
2. The next failure was Photopea calling missing `console.clear()` from a network
   error handler. Window and Worker consoles now provide the operation; the
   append-only host diagnostic log is not erased.
3. A real 64×48 PNG import exposed `Not enough RAM! (need 2 MB)` / `low_ram`.
   Lumen incorrectly applied its ordinary-array 1,048,576-element work bound to
   numeric typed-array allocations. Numeric and array-like typed-array construction
   and ArrayBuffer transfer now use the existing 256 MiB **byte** bound, with checked
   element-size arithmetic. This is not an unlimited-allocation change.
4. TypedArray-from-TypedArray construction no longer consumes the source iterator
   and boxes every element. Same-element-type copies copy the actual view bytes
   directly, preserving offsets, NaN payloads and negative zero. Other element
   types perform the required per-element conversion; Number/BigInt mixing is
   rejected. Constructor/prototype lookup and array-like Get/conversion order
   were corrected at the same boundary.
5. Structured cloning previously expanded ArrayBuffers into JSON arrays of boxed
   numbers, hitting the ordinary-array limit for a normal document bitmap. The
   internal wire codec now uses native base64 (`AB64`) and retains backwards reads
   of `AB`. Resizable ArrayBuffer dimensions are preserved. Transfer-list support
   and other existing structured-clone gaps are **not** claimed fixed.
6. Investigation then found Canvas2D was a no-op object and layout skipped canvas
   elements. The current development implementation has DOM-owned native bitmaps,
   pixel read/write, clipping, affine transforms, solid fill/stroke, compositing,
   paths and Path2D, image/canvas drawing, PNG encoding, and shared replaced-element
   layout/image discovery. Cached PNG presentation is an initial transport, not
   a claim of a low-copy animation pipeline.
7. HTTP iframe navigation now carries its actual final request referrer and URL
   into child-Document creation. Initial about:blank Documents retain the creator
   URL; replaced Documents retain their own metadata snapshots. The iframe's
   explicit referrer-policy attribute and redirect policy headers are honored.
8. Native image drawing keeps cross-origin canvas taint across every recorded
   redirect, even a return to the original origin. Unknown legacy cache provenance
   is not treated as proof of same-origin. `<base>` cannot change the Canvas
   Realm's captured origin. Copying and clearing do not remove taint; resizing does.
9. The embedded export probe revealed that Window.postMessage reported the
   receiver's origin as the sender's. A native operation now captures the executing
   script/function Realm before entering the receiver's self-hosted implementation.
   Script, module and eval entries retain their execution context across native
   calls. Private same-Agent Window slots carry origins and receiver queues;
   messages are serialized at send time, filtered by target origin at delivery,
   and deserialized in the destination Realm. This is **not** a complete HTML
   incumbent-settings/WindowProxy implementation; see the limitations below.

## Evidence, not a shell-only success criterion

Artifacts are under the ignored directory
`benchmark-results/photopea-20260907-start-X2ZhE8/`; real browser observations are
under `benchmark-results/real-sites-20260906/`.

- `photopea-edit-override-profile-01`: old release engine plus the ImageData/console
  prelude; loading a disposable red/green PNG triggered the false 2 MB memory error.
- `photopea-edit-buffers-profile-01`: rebuilt release engine/browser; memory error
  gone, document tab `image.png`, History entries `Open` and `Rotate`, and a full
  836×666 bitmap submitted to `putImageData`. Canvas was still stubbed in this
  binary: **these facts establish document processing, not painted usability**.
- `photopea-iframe-buffers-profile-01`: the documented embed/API test encountered
  `document.referrer is empty`. TRust's referrer getter is an existing empty-string
  stub. No PNG export message was captured. Top-level `saveToOE` does not send a
  message, because Photopea requires an enclosing Window for that API.
- `photopea-edit-canvas-profile-01`: rebuilt native-Canvas release, **no prelude
  override**. The canonical-display-list PNG shows the actual 48×64 rotated
  document, red above green, and the corresponding layer thumbnail. Pixel probes
  read exact RGBA `(255,0,0,255)` and `(0,255,0,255)` inside the document. Its first
  serialized render containing the document tab and Rotate history arrived at
  5.80 seconds; this is a DOM milestone, not a desktop time-to-interactive claim.
  The complete 300-second observation used 28.25 service CPU-seconds and peaked
  at 805.6 MiB. This is functional acceptance, not a controlled speedup comparison.
- Every observation above used a release binary and a 300-second observation
  bound. That bound is **not a load-time measurement**. Diagnostic prelude
  overrides and exact binary hashes are recorded with each run.
- `photopea-iframe-origin-profile-01`: a separate 30-second diagnostic with the
  security/referrer release confirmed that Photopea generated an ArrayBuffer
  export at 5.90 seconds, but the event carried the loopback parent's origin,
  not `https://www.photopea.com`. The parent deliberately kept its origin filter
  and rejected it. This is the failure addressed by item 9, not an accepted export.
- `photopea-iframe-export-final-01`: final rebuilt release, no prelude override;
  the parent required `https://www.photopea.com` **and** the actual iframe's
  WindowProxy source identity. The 127-byte PNG arrived at 5.9347 seconds and all
  3,072 decoded pixels matched the 48×64 expected rotation. The canonical
  display-list screenshot also shows the painted document and layer thumbnail.
  The 60.2663-second observation used 11.806 service CPU-seconds, peak 816.3 MiB.
- `photopea-iframe-large-final-01`: the same unmodified release imported a
  1024×768 PNG (3,145,728 uncompressed RGBA bytes, exceeding the old list cap),
  rotated it, and returned a 768×1024 PNG at 20.2126 seconds. The parent origin
  and source checks passed; **all 786,432 decoded pixels matched exactly**.
  This remains slow for a simple image: it establishes functionality, not V8
  parity or acceptable sustained editing performance. The larger verifier first
  hit Node's default stdout buffer limit; raising the diagnostic pipe bound to
  the expected RGBA size allowed the full check without changing browser code.
  The completed 300.2187-second observation used 47.939 service CPU-seconds,
  peak 836.6 MiB. Its final canonical display-list screenshot shows the large
  rotated red/green document; pixel probes at (500,200) and (500,600) read exact
  opaque red and green. The final JS outcome reports `panicked=false`.
  Exit code 3 is the runner's configured observation timeout, not a successful
  browser shutdown code; acceptance is established by the recorded export and
  paint checks, not by interpreting that timeout as a load milestone.

Final release artifacts, archived under `candidate-photopea-messages/`:

- `trust`: SHA-256 `39d08111d62405a89db214958bfbd7961ec882d808ad737fa879251382379d7d`.
- `trust-headless`: SHA-256 `dad13dabdb11fa3be810c9c9e84369208d619771205965b48538fcaf66bdce37`.
- Build: `CARGO_PROFILE_RELEASE_STRIP=none cargo build --offline --locked --release -j2 --bin trust --bin trust-headless`;
  7m31s, 5.1 GiB service peak memory. Source diffs, untracked-source copies and
  hashes are archived alongside the binaries. No application builds or unit
  suites ran concurrently with these final browser observations; these are not
  controlled whole-site speedup comparisons on an otherwise idle workstation.

The disposable input is 64×48, red on the left and green on the right. The
documented Photopea API opens it, rotates it 90°, and requests a PNG export. It
does not access an account, existing document, or upload destination.

### Desktop handoff correction — 2026-09-07

The initial build command above selected only `trust` and `trust-headless`.
It did **not** update `trust-desktop`. This was a handoff omission, not evidence
that a successful API export established acceptance of the desktop executable.
The user's running desktop executable (PID 2325553, started 10:26:20 CEST) matched
the old on-disk desktop artifact, built at 07:21:32 CEST, before these fixes.

- Old desktop SHA-256:
  `35113368354d6c266ee4eb3509a3a349f544e64718b6a7e9f5a283861619ed6b`.
- Corrected desktop SHA-256:
  `4b2c5212b63f0821909c1d6660fcc46071ffa92534d3bb09296d5cfbfde2611d`.
- Build: `CARGO_PROFILE_RELEASE_STRIP=none cargo build --offline --locked --release -j2 --bin trust-desktop`;
  succeeded in 4m36s, service peak 4.7 GiB. No production source changes were
  needed in this follow-up. The current release target matches the archived
  corrected executable byte-for-byte. The user's running process was untouched
  and still mapped the old executable after the rebuild; relaunch is necessary.
- Functional checks run the actual desktop binary under a private labwc
  headless/pixman Wayland compositor, with default `--renderer=auto`, no page
  prelude overrides, and isolated configuration/storage. Auto selected the Vello
  **CPU** fallback because that compositor has no compatible Hybrid surface.
  This verifies desktop presentation/input, not the user's GPU renderer, and is
  not a controlled performance comparison. Private virtual-pointer input targets
  only the disposable compositor, never the user's window.
- Both `photopea.com` and `www.photopea.com` initially show the landing page in
  clean storage. The apex redirects to www; the title retaining the supplied
  URL is not proof of a different editor. The site's `addPP()` handler hides the
  landing page and inserts the editor resources. The earlier API fragment also
  invokes that handler, so initial no-click observations were not editor tests.
- `benchmark-results/photopea-desktop-old-click-20260907/`: launching the
  archived old binary at the user's exact apex URL and clicking Start at the
  15-second checkpoint reproduces the white page and pale purple gradient.
  `desktop.log:111` records Photopea's main script throwing
  `ReferenceError: ImageData is not defined`. `desktop-after-start.png` captures
  both the blank document and the error in TRust's command overlay.
- `benchmark-results/photopea-desktop-editor-20260907/`: the corrected release
  at www, followed by a normal Start click, paints the editor menus, sidebar,
  logo and action buttons. A subsequent New Project click opens the actual
  interactive dialog (`desktop-new-project.png`). This is stronger than a DOM
  shell check but does not yet verify completing a manually created document.
  The completed 300.091-second desktop observation retained that dialog and
  reported no script exception or panic; service CPU time was 17.402 seconds,
  peak memory 882.4 MiB. Start and New Project were clicked during the observation,
  so neither the observation duration nor its CPU total is an editor load time.
  The Start screenshot briefly shows Photopea's source-change warning: its
  captured code checks a roughly 320-pixel difference between initial/current
  viewport or screen width. This compositor maximizes 960 to 1280 pixels during
  startup, matching that condition; the warning is not proof of source rewriting.

### Isolated engine operation

With no browser run/build competing for CPU, the archived old and new release
CLIs ran the same `buffer-copy.js`: one warmup followed by three measured batches
of eight 128 KiB Uint8ClampedArray copies, checking independent buffers and data.

| Release | Batch times (ms, Date.now) | Checksum |
| --- | --- | --- |
| Prior frontend optimization checkpoint | 956, 959, 959 | 7168 |
| Pixel-buffer candidate | 0, 1, 0 | 7168 |

The candidate is below the timer resolution for this batch size. Do **not** derive
an exact speedup ratio or a whole-site speedup from these numbers.

### Remaining throughput target exposed by the working editor

The larger final run provides a useful next engine workload. Its first-60-second
CPU profile has 23.09 sampled CPU-seconds, including 4.67 exclusive seconds in
`lumen::bytecode::run_vm`, 1.21 in integer formatting, 0.78 in `num_to_str`, 0.66 in
`canonical_numeric_index`, 0.63 in `to_property_key`, and 0.44 in float parsing.
The formatting/index-conversion attribution is a strong lead for investigating
numeric typed-array element access and VM/JIT indexed-access paths; these samples
alone do not establish how much a particular replacement will save.

The entire run's conservatively named GC functions total only 0.46 exclusive
seconds, versus 26.15 in named Lumen functions. Eight logged full layout passes
sum to 35.56 ms. Neither figure covers all possible GC/layout costs, but this
profile does **not** point to nursery integration or full layout as the dominant
first lever for this image-editing case. About 7.43% of full-run exclusive samples
are unresolved, so this is not a complete attribution. Profile reports are saved
beside the final run rather than discarded after compatibility acceptance.

## Verification so far

- Lumen `--features embed --lib`: **952 passed**.
- The archived buffer-candidate release's Test262 TypedArray, TypedArrayConstructors, Uint8Array, ArrayBuffer,
  SharedArrayBuffer and DataView groups: interpreter and bytecode each
  **3,139 passed, zero failed, one existing skip**; JIT **3,137 passed, two failed,
  one existing skip**.
- Both JIT failures are reproduced with the archived old release runner: strict
  indexed assignment to a non-extensible receiver with a TypedArray prototype,
  in Number and BigInt variants. They are not new buffer-constructor failures.
- ImageData constructor/clone tests pass in all three tiers, plus Worker and
  cross-realm/GC cases. The standalone Node 24 experimental base64 reference
  rejects the final resizable-buffer case; it is not a passing reference for
  that case.
- Canvas pixel tests pass in all three tiers, including negative dirty rectangles,
  detached data, float16/color conversion, overlapping self-copy, saved state,
  SVG path valid-prefix recovery, Path2D copies, and clipping with `copy`/clear.
- A canonical-renderer integration test verifies actual red/green RGBA pixels and
  terminal image discovery. The `canvas_` filtered suite: **6 passed**.
- Window messaging regression tests cover direct, bound and Reflect.apply calls,
  child-to-parent and parent-to-child delivery, opaque origins, default/explicit
  target filtering, malformed target URLs, cycles, Date/Map/buffer clones, and
  destination Realm prototypes in all three tiers. The native script-caller test
  also covers nested eval and module execution. These do not cover every form of
  native Web IDL callback with no author function on the stack.
- The current serial TRust backend suite: **106 passed, two failed**. The two
  failures are Wasm instance-retention tests
  growing by 2,048 objects. Both reproduce with the old Lumen checkpoint and
  pre-edit browser prelude in an isolated baseline build. They remain unfixed.
  A parallel run also failed the iframe URL-resolution test with an empty
  Document. It passed both in isolation and in the serial suite; its loopback
  server has a two-second accept timeout. Load-sensitive timing is a plausible
  explanation, not a separately proven root cause; the test was not weakened.
- HTTP suite: **103 passed, zero failed, nine manual tests skipped**. Focused
  referrer/policy tests: **seven passed**. The redirect test checks the headers
  actually received by loopback HTTP servers, including a cross-origin hop back
  to the original origin and a redirect that sets `no-referrer`.
- Clippy for the library and headless binary completes with ten warnings: eight
  new style/iterator suggestions in Canvas/referrer code and two outside this
  change. It is not a warning-free result. New Rust functions were formatted
  without rewriting unrelated dirty-worktree code. New JS regression fixtures
  use `.mjs`, because TRust's local ignore rules hide untracked `*.js` files.

The current release build targets `TRust/target/release/trust`,
`trust-headless` and (after the handoff correction) `trust-desktop`;
**nothing has been installed or promoted**.

## Standards basis

The local web-standards skill was used to consult recorded official snapshots,
without refreshing or crawling the standards servers. Constructor/coercion order,
private brands, pixel replacement independent of drawing state, and resizing
semantics were derived from the algorithms, not Photopea-specific exceptions.

- [HTML ImageData](https://html.spec.whatwg.org/multipage/imagebitmap-and-animations.html#imagedata):
  local `/big/web-standards/repositories/whatwg/html/source:131555`, snapshot
  `e5071a20c8569d8a3ec02ed27dd01b948773f850`, fetched 2026-09-06.
- HTML canvas state and pixel algorithms: same source at lines 71017, 71274,
  73060, 73482, 74826, 75249 and 75405; [Canvas](https://html.spec.whatwg.org/multipage/canvas.html).
- [ECMA-262 TypedArray constructors](https://tc39.es/ecma262/#sec-typedarray):
  local `/big/web-standards/repositories/tc39/ecma262/spec.html:43443`, snapshot
  `e28783d5fc9dc12b3de905961e2c71410b38a202`.
- Web IDL: local `/big/web-standards/repositories/whatwg/webidl/index.bs:11523`
  (overloads), 9138 (buffer sources), 7992 (dictionaries), 12310 (attributes),
  snapshot `8f182624f632a0ce485e236edbc1df18ca385b1d`.
- [Console clear](https://console.spec.whatwg.org/#clear): local
  `/big/web-standards/repositories/whatwg/console/index.bs`, snapshot
  `a82403e842252f34975f84091ee694aef86dfd37`.
- [CSS Color conversion](https://drafts.csswg.org/css-color-4/#color-conversion),
  [compositing](https://drafts.csswg.org/compositing-1/), and
  [matrix dictionaries](https://drafts.csswg.org/geometry-1/#dommatrixinit-dictionary):
  local CSSWG editor's-draft snapshot `81c27f68690138345b2b3b6af8ccc42dad3dca1d`.
- [SVG path error handling](https://www.w3.org/TR/SVG2/paths.html#PathDataErrorHandling):
  local `/big/web-standards/repositories/w3c/svgwg/master/paths.html:1118`,
  snapshot `c403ca46ad045ebdaeda148bae69f814fb744db7`.
- [Fetch main algorithm](https://fetch.spec.whatwg.org/#main-fetch) and
  [HTTP redirects](https://fetch.spec.whatwg.org/#http-redirect-fetch): local
  `/big/web-standards/repositories/whatwg/fetch/fetch.bs:4980` and 5980,
  snapshot `394d20d144ed1401c2c0e02c35bc3608cb2a2269`.
- [Referrer determination](https://w3c.github.io/webappsec-referrer-policy/#determine-requests-referrer):
  local `/big/web-standards/repositories/w3c/webappsec-referrer-policy/index.src.html:733`
  through 1000, snapshot `cc435b05ca4a94f7f1a139be5074b168d20014db`; HTML Document
  creation at `source:106496` and `source:113436`.
- [HTML posting messages](https://html.spec.whatwg.org/multipage/web-messaging.html#posting-messages):
  local HTML `source:133766`–133934; incumbent settings at 117752–118020, same
  HTML snapshot above. ECMA-262 `spec.html:13860`, 14408, 26800 and 29294 describe
  ordinary/native calls and script/module evaluation contexts, same ECMA snapshot.

## Open acceptance work / limitations

Do not claim full Canvas2D compliance: gradient/pattern/text/hit-testing entry
points still include old stubs; wide-gamut/float backing-context requests return
null; OffscreenCanvas, complete image request/CORS state, foreign-realm Canvas
brands, and bitmap lifetime/aggregate-budget hardening need follow-up. JS numeric
drawing attributes also need a precision audit. PNG presentation must be measured
for repeated-frame cost. The HTTP iframe referrer implementation is not a claim
of complete policy-container/meta-referrer/top-level-navigation integration.
Window messaging still needs the full backup-incumbent stack for bound native
callbacks invoked by timers, listeners and promise jobs, with self-hosted platform
frames excluded from author attribution. Transfer-list detachment/MessagePort
ownership and the broader cross-origin WindowProxy security surface are not
claimed complete. Structured cloning retains other existing foreign-Realm brand
gaps. The newly tested direct script/function messaging path does not establish
conformance for those cases.

The documented Photopea API's import/rotate/export path is verified. The rebuilt
release `target/release/trust-desktop` additionally passes the isolated CPU
desktop editor-start and New Project dialog checks described above and is ready
for the user's acceptance test. Arbitrary documents, manual file-picker/download
workflows, broader interactive tool coverage, the user's GPU renderer and
sustained editing performance have not yet been verified. Nothing was installed,
promoted, staged, committed or pushed during this development round.
