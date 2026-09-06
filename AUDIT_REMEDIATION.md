# Lumen audit remediation checklist

This checklist tracks the repository-wide audit begun on 2026-08-28. Work is
ordered by correctness and containment first, standards coverage second, and
measured performance improvements third. An item is complete only after focused
conformance-style tests and the broader affected suite pass.

The checked audit entries below record work completed during the original audit;
they are not an acceptance statement for the resulting engine. Production-
differential browser testing on 2026-08-29 exposed regressions that the original
verification gates did not detect. The following acceptance blockers therefore
take precedence over every historical checkmark in this document.

## Post-audit regression remediation

### Incremental browser-conformance acceptance

- [x] Reproduce and correct the YouTube consent rejection regression through
  general Window/Document lifecycle behavior. A fresh optimized gate now
  activates the real accessible "Reject" control, observes it disappear, and
  completes with zero script errors; no site-specific selector or workaround
  is involved.
- [x] Give each nested Window settings context independent author global
  properties, writable platform descriptors, global lexical declarations,
  module maps, event handlers/listeners, timers, animation frames, observers,
  and queued tasks. Add focused navigation and cross-Window isolation tests.
- [x] Retire a destroyed nested Document's engine jobs, module records,
  namespace roots, and embedder settings state, while keeping the top-level
  settings context alive for the page realm. Move detached-node listeners to a
  weak registry without changing observable listener identity when retained
  author nodes are reinserted.
- [x] Exercise Speedometer 3.1 through its actual accessible Start control and
  require the official summary with no script errors. All default suites pass
  one iteration (58/58, valid summary, score 0.1687), every suite has passed
  individually, the Web Components pair passes two iterations, and the first
  six default suites pass two iterations after the lifecycle corrections.
- [x] Preserve Fetch's initiating global as the networking task destination and
  construct response/body objects in that Window Realm. A focused child-Window
  test now consumes exact text and ArrayBuffer bytes; Perf-Dashboard completes
  all three tests instead of parsing an empty manifest body and failing its
  router initialization.
- [x] Remove the non-standard cumulative 256-request failure cliff from author
  Fetch/XHR and required HTML/module/SVG resource loads. Keep the threshold only
  as an optional module-prefetch cutoff: a focused test prepares 320 valid
  requests, and an optimized resident-page probe completes 320 sequential Fetch
  calls with zero errors. This restores the Fetch Standard §5.6 rule that every
  valid call invokes Fetch and fails only for a specified request/network reason.
- [x] Make the ECMA-262 GetTemplateObject cache Realm-owned for GC purposes.
  Key entries by the active Realm and template site, count the cache as an
  internal edge, activate cached arrays only with their live Realm, and evict
  entries when that Realm is collected. The side-table regression test now
  creates a tagged template in a temporary `$262.createRealm()` and verifies
  that its Realm and template cache are reclaimed.
- [x] Preserve tagged-template Parse Node identity across AST lifetime and
  bytecode compilation. Process-unique site IDs replace allocator-address
  keys, are carried through compiled chunks and snapshots, and a regression
  test verifies that separately parsed identical source gets a distinct frozen
  template object (ECMA-262 GetTemplateObject/[[TemplateMap]]).
- [x] Re-run real-site semantic gates after the lifecycle pass. YouTube consent
  rejection, Twitch search/carousel/channel cards, and Steam search/featured/
  offers/catalog all pass after both Fetch corrections with responsive resident
  actors and zero script errors.
- [x] Repeat the YouTube/Twitch/Steam semantic matrix after document-scoped
  geometry caching and real-Realm task normalization. YouTube consent rejection
  and search (954 nodes), Twitch search/carousel/channel cards (1,888 nodes),
  and Steam search/featured/offers/catalog (2,462 nodes) all retain responsive
  actors and report zero script errors.
- [x] Cancel queued iframe attribute-processing work when its container is
  removed. The DOM removal steps now destroy the child navigable, and a stale
  task cannot resurrect a detached Window Realm or run its srcdoc script;
  focused HTTP regression coverage exercises the detached-navigation case.
- [ ] Complete the remaining Speedometer 3.1 expansion without batching
  unrelated implementation changes.
  - [x] Re-run all default suites for one iteration on the lifecycle-cleanup
    tree: actual Start activation, valid official summary, score 0.1687, 121
    live updates, and zero script errors in 461.162 seconds. After the nested
    frame lifecycle/viewport fixes, the same gate again reached the summary
    with 122 live updates, zero errors, and 503.382 seconds elapsed.
  - [x] Run all default suites for two iterations to expose repeat-navigation
    and accumulated-state defects. The first run exposed the cumulative request
    cliff at 112/116. After its general removal, the optimized gate completed
    116/116, reached a valid official summary (score 0.152 ± 0.32), produced
    240 live updates, and reported zero script errors in 938.058 seconds.
  - [x] Run the official default ten-iteration benchmark to its summary with
    zero script errors, then repeat the real-site regression matrix.
    - [x] Diagnose the 2026-08-31 apparent ten-iteration stall. It made real
      progress through 151/580 subtests with no exception; a three-iteration
      TodoMVC reproduction completed and showed flat per-iteration callback
      cost rather than a lost continuation or accumulating lifecycle state.
      The two-hour gate expired because synchronous geometry was too slow.
    - [x] Profile the real accessible-control workload rather than a synthetic
      engine loop. A function/self-time profile and `TRUST_DIAG_FRAME` localized
      47.3 of 59 seconds to 214 arena-wide CSSOM View measure passes triggered
      while an iframe benchmark mutated its distinct child Document.
    - [x] Give CSSOM View geometry invalidation document scope. Cached top-level
      iframe boxes are reused only when every intervening mutation is confined
      to a child Document, iframe child content, or a still-disconnected tree;
      iframe attributes, connected container mutations, viewport/image changes,
      and hit-test activation retain the full measure fallback. The focused Lit
      run fell from 214 to 7 measure passes, 58.6 to 12.6 seconds, and score
      0.01799 to 0.1023 with a valid summary and zero errors.
    - [x] Establish an independent LibreWolf/WebDriver-BiDi reference using the
      actual Start button. The same isolated suite completed with a valid score
      of 12.00 in 1.242 seconds and no console errors; this remains a performance
      reference, while the official specifications remain the behavior authority.
    - [x] Stop treating a real child Realm's null internal task marker and its
      root iframe marker as different Window globals. HTML creates one Window/
      GlobalEnvironment for that Realm; normalizing the markers before legacy
      scoped-Window dispatch removed the former 810/800 author-global descriptor
      scan batches and exposed a remaining 212-scan Promise-job batch. The
      focused Lit run remained valid and error-free and fell from 12.596 to
      6.166 seconds; all 34 iframe-focused TRust tests passed.
    - [x] Create the iframe child navigable's populated initial `about:blank`
      Document and real Lumen Realm during post-connection processing. Detached
      containers expose null content accessors; connected blank frames have
      independent intrinsics and fire their initial load event; the first
      same-origin navigation reuses that Window while replacing its Document.
      Focused conformance coverage and all 35 iframe tests pass, and the Lit
      workload remains valid and error-free at 6.107 seconds. A post-change
      matrix also passes YouTube consent/search (954 nodes), Twitch search/
      carousel/channel cards (1,858 nodes), and Steam search/featured/offers/
      catalog (2,458 nodes), with responsive actors and zero script errors.
    - [x] Carry the ECMA-262 §9.5 Promise-job Realm explicitly and select it
      before HTML §8.1.6.6.4 prepares that Realm's environment settings. A
      focused cross-Realm test proves the host preparation callback never runs
      against the caller global, while the legacy same-Realm settings-token
      test remains green; the Test262 `built-ins/Promise` slice passes 731/731
      (one upstream skip). This removed the last 212 top-Realm descriptor scans;
      the isolated Lit workload reports a valid 0.4132 score with zero errors in
      5.072 seconds. Lumen passes 691 tests, optimized TRust passes 1,004 tests
      (17 ignored), and all 35 iframe tests pass. Post-change Twitch (1,867
      nodes) and Steam (2,470 nodes) retain every semantic milestone with zero
      errors; YouTube reached its search input with zero errors in a session
      where the consent dialog was not presented, so dismissal was not exercised
      by this pass.
    - [x] Batch the DOM Standard `replace all` wrapper-retention transition for
      `innerHTML` and perform one descendant-frame teardown query per replacement
      target. Focused tests preserve detached wrapper identity, listeners, shadow
      descendants, reinsertion, and weak collection; the isolated TodoMVC-jQuery
      run fell from 106.323 to 82.860 seconds before engine collection work.
    - [x] Give ordinary Map and Set a collision-safe SameValueZero hash index while
      retaining the ordered tombstone list required by live iterators. ECMA-262
      §§24.1/24.2 explicitly require average sublinear access. All 587 Map/Set
      Test262 cases pass, including clear/delete/reinsert iterator behavior, and a
      20,000-key stress case passes. The final optimized TodoMVC-jQuery gate is
      valid and error-free at 8.992 seconds (score 0.1162), about 11.8 times faster
      than the pre-batching baseline.
    - [x] Move segmented-stack headroom checks ahead of large accessor/host-native
      execution frames. ECMA-262 §9.4 execution contexts remain language-visible
      only through their specified state; the native representation must not abort
      before Lumen's heap-backed stack can engage. A focused 2 MiB host-stack test,
      the repeated bytecode iframe-navigation stress case, all 36 iframe tests,
      and all 693 Lumen tests pass without adding a recursion limit.
    - [x] Repeat the current optimized real-site matrix after the DOM/collection/
      stack work. YouTube search (430 nodes), Ars Technica's populated article grid
      (2,911), Instagram login/cookie UI (809), Twitch search/carousel/channel cards
      (1,901), and Steam search/featured/offers/catalog (2,457) all pass with
      responsive actors and zero script errors. YouTube did not serve a consent
      dialog in this pass, so its reject action could not be re-exercised.
    - [x] Re-establish a current-tree full Speedometer 3.1 one-iteration summary
      after the collection and stack changes. All 58 workloads complete through
      the real Start control in 127.416 seconds with 120 live updates, a valid
      0.3385 score, and zero script errors—about 3.6–4 times faster than the
      earlier 461–503-second full runs.
    - [x] Re-run two iterations on the current tree to check repeat navigation
      and accumulated state. All 116 workload executions complete in 282.190
      seconds with 239 live updates, a valid 0.296 ± 0.53 score, and zero script
      errors.
    - [x] Complete the current-tree official ten-iteration gate. All 580 workload
      executions reach a valid summary in 3,388.022 seconds with 1,188 live
      updates, a 0.147 ± 0.066 score, and zero script errors. Resident memory
      reached roughly 6.7 GiB and throughput declined relative to the one- and
      two-iteration gates, so accumulated-state/lifetime cost remains a measured
      optimization target rather than a conformance blocker.
    - [x] Repeat the real-site matrix after the ten-iteration pass. YouTube
      (1,306 nodes), Twitch (1,869), Steam (2,468), Ars Technica (3,122), and
      Instagram (809) retain their semantic milestones with responsive actors
      and zero errors. A second YouTube run presented the real consent dialog;
      canonical activation of its accessible Reject control removed the dialog,
      retained search and 954 useful nodes, and completed the 420-second gate
      with zero errors and no choice-saving failure.

### Post-checkpoint performance recovery

The 2026-08-31 release checkpoint is functionally accepted by the existing
conformance gates but is not approved for installation. Direct execution of
Crystal's unchanged v7/v8 fixture through the installed and checkpoint browser
binaries found general hot-path and memory regressions that must be recovered
without undoing the standards fixes above. Five interleaved terminal samples
measured a 2,195 -> 1,618 aggregate score, 36.318 -> 38.904 seconds of page
time, and 396.9 -> 456.2 MiB peak RSS. Richards accounts for most of the
aggregate score loss; excluding it, the paired component geometric mean is
2.4% lower. RegExp remains 19.7% lower, and a bytecode-only control is 15.0%
lower across the seven non-Richards components. A focused browser page also
measured Map/Set 55% faster but repeated innerHTML replacement plus scoped
selector queries 9.0% slower. User-visible DOM churn is an optimization target,
not an acceptable cost of conformance.

- [x] Remove redundant per-call Agent activation while preserving the ECMA-262
  surrounding-Agent, Realm, job, coroutine-resume, and independent-engine
  boundaries. Prove cross-Agent allocation/symbol ownership with focused tests.
  Ordinary calls now pay no Agent-selection cost; host entries activate once and
  nested ShadowRealm heap transitions restore by RAII. All 696 embed-enabled
  Lumen tests pass. Three optimized browser-harness samples improved median
  benchmark time from 41.39s with the per-call fast check to 37.47s, and a
  120-second YouTube gate completed with 1,306 nodes, 19 updates, and zero
  script errors.
- [x] Amortize native segmented-stack headroom checks without introducing any
  ECMAScript recursion limit. Retain the 2 MiB host-stack/accessor stress and
  deep interpreter/bytecode/JIT/constructor/coroutine coverage. Native calls
  now query stack headroom at entry and every eight execution contexts; the
  existing 1 MiB red zone and segmented growth remain. The large-frame test,
  4,096-context tests on all three tiers, recursive construction, mixed JIT
  boundaries, and generator continuation test pass. A 5.12-million-call
  bytecode A/B improved 2,641ms -> 2,579ms, broad browser-harness wall time
  improved 39.10-39.74s -> 38.32s, and a populated 90-second YouTube gate
  completed with 1,306 nodes, 20 updates, and zero errors.
- [x] Amortize RegExp host-interruption polling while keeping ECMA-262 matcher
  failure distinct from interruption and resource exhaustion. Run the official
  RegExp Test262 slice plus adversarial interruption tests. Direct engine calls
  retain an immediate poll, long scans/backtracking retain internal checkpoints,
  and native multi-match loops share the execution cadence instead of issuing
  an atomic host-state read per tiny match. A 2-million-match A/B improved
  3,961ms -> 3,921ms; all 1,879 official `built-ins/RegExp` Test262 files pass,
  as do cancellation/resource-exhaustion tests. A 90-second Twitch gate retained
  1,862 nodes, processed 18 updates, and reported zero script errors.
- [x] Keep trivial ASCII RegExp subject views out of the byte-accounted prepared-
  text LRU while retaining one hot entry for consecutive reuse and preserving
  the LRU for non-ASCII code-unit/code-point materialization. This is internal
  caching beneath ECMA-262 §§22.2.6.2 and 22.2.7.2; `lastIndex`, captures,
  matcher ordering, and abrupt completions are unchanged. Five interleaved
  browser samples improved the unchanged RegExp score 361 -> 437 (21.1%), and a
  complete fixture pass improved 1,737 -> 1,786 while peak RSS fell 463.2 ->
  453.1 MiB. All 1,879 official `built-ins/RegExp` Test262 files pass; rebuilt
  YouTube consent/search and Twitch semantic gates complete with zero errors.
- [ ] Profile and accelerate HTML innerHTML fragment replacement and DOM scoped
  selector matching. Preserve ordered removal/insertion side effects, one
  replace-all mutation record, detached wrapper identity/listeners/shadow trees,
  iframe teardown, static querySelectorAll results, scoping, and syntax errors.
  - [x] Batch standards-ordered fragment replacement and keep the html5ever
    parser's private arena surgery free of live-page invalidation work. Focused
    replace-all, foster-parenting, and adoption-agency tests pass; 1,000 isolated
    fragment replacements improved 288.98 -> 260.69 ms (9.79%).
  - [x] Return a new static, indexed, non-constructible `NodeList` from
    `querySelectorAll`, defer wrapper creation until consumption, preserve
    delayed connectedness/identity, and implement the Web IDL indexed-property
    define/delete/extensibility rules. Invalid selector strings now throw a
    `SyntaxError` DOMException from query and match APIs. The replacement plus
    length-only selector microbenchmark improved 392.32 -> 108.99 ms (72.2%),
    total workload wall time improved 2.685 -> 2.406 seconds, and peak RSS fell
    168.5 -> 156.5 MiB.
  - [x] Replace the JavaScript Proxy-backed indexed collection with a native
    Lumen/Web IDL exotic object fast path. The implementation follows Web IDL
    legacy platform-object `[[GetOwnProperty]]`, `[[Set]]`,
    `[[DefineOwnProperty]]`, `[[Delete]]`, `[[PreventExtensions]]`, and
    `[[OwnPropertyKeys]]` behavior, including proxy invariants and key snapshots
    across getter side effects. Against the Proxy checkpoint, the length-only
    browser workload improved 108.99 -> 88.05 ms (19.2%); against the former
    eager Array baseline, fully consuming every node improved 391.94 -> 374.19
    ms (4.5%), eliminating the prior approximately 7% regression.
  - [x] Repeat the optimized semantic gates after both DOM passes. YouTube's
    real consent Reject action closes the dialog and retains search (954 nodes,
    24 updates), Twitch retains search/carousel/channel cards (1,924 nodes), and
    Steam retains search/featured/offers/catalog (2,376 nodes); all three actors
    remain responsive and report zero script errors.
- [ ] After every accepted optimization, re-run its focused conformance tests,
  the comparative browser-binary microbenchmark, and at least one actual-site
  semantic gate. Repeat the full site matrix and a Speedometer slice before a
  new release checkpoint.

### Real-site capability expansion

- [x] Implement the Service Workers §5 `CacheStorage`/`Cache` Window surface
  with origin-scoped named caches, request/Vary matching, body cloning, batch
  replacement, quotas, and deleted-name/cache-object lifetime. Focused tests
  cover the normative matching and lifetime algorithms; Discord no longer
  aborts in its Wasm cache bridge.
- [ ] Finish Cache API worker exposure and cross-agent serialization/ordering;
  the current first slice is exposed in the resident Window realm.
- [ ] Implement Indexed Database 3 incrementally rather than accepting a
  feature-detection stub.
  - [x] Implement the first functional Window slice: queued open/upgrade and
    request events, ordered transaction requests, private read/write snapshots
    with atomic publication, structured-cloned values, object-store CRUD,
    key comparison/ranges, and reusable cursor continuation requests.
  - [x] Run Telegram's actual downloaded `encryptedStorageLayer` module graph
    against the implementation. Its production wrapper creates/upgrades the
    database, stores and retrieves a nested value plus `Uint8Array`, and walks
    the store cursor to completion with no errors.
  - [x] Complete index retrieval/cursors and unique/multiEntry enforcement,
    including secondary-key ordering, unique cursor directions, and per-request
    rollback when a unique write fails.
  - [x] Implement the Indexed Database 3/WHATWG DOM request event path
    (`request -> transaction -> connection`), trusted capture/target/bubble
    dispatch, transaction reactivation, cancelable error recovery, and the
    listener-exception `AbortError` branch. Focused tests exercise every event
    phase, non-bubbling success capture, continued requests after cancellation,
    and abort propagation to the database.
  - [x] Implement the per-name connection queue and event-driven upgrade/delete
    blocking algorithms. A close-pending connection now continues to block while
    its live transaction drains, actual closure wakes the waiter without polling,
    and queued delete observes the upgraded version in specification order.
  - [ ] Complete remaining abort and upgrade rollback edge cases, key
    generators/key-path edge cases, and Worker exposure with focused WPT-style
    coverage.
- [x] Implement WHATWG HTML §3.2.6.6 `DOMStringMap` supported named properties:
  dynamic attribute-order enumeration, own property descriptors, prototype
  fallback, named set/delete conversion, and exceptions. This fixes GitLab's
  enumerated `dataset` boot-data path; its JSON exception is gone.
- [x] Expand the public-site matrix with Spotify and CodePen. Their optimized
  live actors reached 1,121/786 nodes respectively, fetched 105/60 resources,
  remained responsive, and reported zero script errors.
- [x] Recheck the previously failing Ars Technica and Instagram front pages
  after the Realm/lifecycle work. Ars Technica reaches its search control and
  populated article feed (3,122 nodes, 120 resources); Instagram reaches its
  interactive username/password login surface and cookie controls (810 nodes,
  12 resources). Both resident actors remain responsive for two minutes with
  zero script errors and neither snapshot is an error/unsupported-browser page.
- [ ] Teach the browser-workload diagnostic to follow actor `Navigate`,
  `Replace`, and `SubmitForm` events across documents. Reddit currently serves
  a standards-based async `requestSubmit()` JavaScript challenge; the main
  browser consumes its serialized hidden-control submission, but the single-
  document gate discards that navigation event and therefore records only the
  challenge splash. Keep the exact challenge flow covered by a Lumen actor
  regression test while this diagnostic limitation remains.
- [ ] Review TRust's HTTP and Navigator `User-Agent` policy against RFC 9110 and
  WHATWG HTML. Telegram and WhatsApp currently return explicit unsupported-
  browser documents to the valid but brand-specific `TRust/0.1` token before
  their application code runs; do not disguise this as a JavaScript failure or
  add a site-specific exception.

- [ ] Complete joint JS/WASM lifetime handling before browser acceptance. The September 5
  follow-up fixes duplicate externref allocation, callback dispatch, multi-value iteration,
  and constructible exported wrappers; all focused tier tests pass. Fresh-instance retention
  falls from three JS objects per instance to one but both enabled retention gates still fail,
  and native Store/unique externref reclamation remains open. Do not weaken wrapper identity
  or failed-start escape handling. See the [verification record](REGRESSION_DIAGNOSIS_2026-09-05.md).
- [ ] Replace arbitrary execution-depth rejection with an implementation that
  supports ECMAScript execution contexts up to actual resource exhaustion.
  - **2026-09-05 correction:** unrestricted native segment allocation was not a safe
    replacement for a depth guard. Neocities exposes recursive calls growing the
    native stacks until host OOM. Bound live additional segment storage, return a
    catchable stack-overflow error through owning call boundaries, and reclaim the
    budget on unwind. Preserve deep finite calls and proper tail-call reuse. See
    [the investigation and verification record](REGRESSION_DIAGNOSIS_2026-09-05.md).
    Existing finite-depth tests alone did not test exhaustion or recovery.
  - [x] Remove TRust's embedder-selected depth budget and Lumen's native
    browser-visible depth guard.
  - [x] Cover interpreter, bytecode, JIT, captured/native calls, and construction
    with focused deep-recursion tests using native segmented-stack checkpoints.
  - [ ] Move wasm32 ordinary calls to heap-owned VM frames so that target does
    not retain a temporary fixed execution-depth guard.
  - [x] Pass the broader Lumen suite and optimized real-browser gate. The full
    workspace suite passes, and Steam, Twitch, and YouTube pass the semantic,
    scheduler-responsive browser matrix plus real terminal loading checks.
- [x] Establish a fast, optimized, non-LTO browser-workload gate for development
  iterations, while reserving the full release profile for acceptance milestones.
  - [x] Make fatal JavaScript errors and minimum DOM progress test failures rather
    than diagnostic output that always passes.
  - [x] Replace raw node-count acceptance with site-semantic landmarks, require a
    real frontend-command acknowledgement within ten seconds, and force the
    ignored gate through the release-equivalent complete typed-render path.
  - [x] Expose terminal loading components (navigation, image fetch/decode,
    image encode, and command acknowledgement) in the frame diagnostic so a
    perpetual standards-valid animation cannot be mistaken for an unfinished
    page.
  - [x] Record a passing production-differential baseline for Twitch, YouTube,
    and Steam. Paired 200x50 pseudo-terminal runs against the installed release
    and current optimized browser-check artifact retain the same YouTube Home/
    Subscriptions/search surface, Twitch login/signup/live-channel surface (the
    current build additionally exposes Browse/Search), and Steam featured
    storefront surface (the current build additionally exposes its search).
- [x] Restore Twitch loading and verify that no JavaScript stack failure or
  framework abort prevents production-equivalent DOM progress. The optimized
  gate reaches search, carousel, and live-channel landmarks; a real terminal
  run reaches a populated 165-row live region with every loading component
  false.
- [x] Restore YouTube's search control and production-equivalent DOM progress.
  The optimized gate reaches the visible `search_query` control after the real
  viewport correction, with the resident actor still responsive to browser
  commands; a real terminal run also reaches an idle loading state.
- [x] Restore Steam's finite initial loading state without reinstating the
  non-standard Promise failure that happened to abort its animation path. The
  gate reaches search, featured-carousel, offers, and catalog landmarks; a real
  terminal run clears every loading component about eight seconds after first
  content while the valid marquee animation continues independently.
- [x] Preserve HTML lazy-image classification through the terminal presentation
  adapter and resume deferred resources only near their retained CSS-pixel
  viewport boxes. Cover eager dynamic mounts, off-screen deferral, scroll
  resumption, and fixed-position descendants with focused frontend tests.
- [ ] Audit every audit-introduced numeric cap, fallback, unsupported branch,
  and watchdog against its governing standard and real-browser workloads.
  - [x] Make iframe navigation handle local `data:` and `blob:` HTML
    resources through their scheme fetch algorithms, including a focused
    regression test. Unsupported document types still complete navigation and
    fire the element `load` event.
  - [x] Remove the no-JS frame depth/load ceilings. The breadth-first
    prefetch/install queues now follow every finite document tree, retain the
    HTML circular-navigation guard, and decode local `data:` HTML without
    treating an implementation count as a navigation failure.
  - [x] Stop truncating applicable external stylesheets and SVG external-use
    resources at a declaration-count cap. Fetch concurrency and per-response
    storage limits remain resource safeguards; CSS source-order participation
    is no longer silently dropped.
  - [x] Remove the stylesheet `@import` nesting and web-font declaration
    count cutoffs. Imports are cycle-checked in source order and every
    applicable `@font-face` is offered to the font loader; failed fetch or
    decode remains the per-resource fallback.
  - [x] Remove the non-standard 16 MiB ceilings from CompressionStream input/
    output and Fetch ReadableStream body consumption. Stream and typed-array
    allocation now proceed until ordinary host allocation/resource failure;
    the standard's BufferSource/type checks remain intact.
  - [ ] Wire TRust's page prelude to Lumen's incremental compression contexts
    and expose the current Compression Standard format set (including Brotli
    and DecompressionStream); the present terminal adapter still has a
    one-shot DEFLATE-family encoder only.
  - [ ] Replace the script-platform fallback for unsupported frame schemes
    with standards-preserving resource/accounting behavior rather than
    silently dropping a child document.
- [x] Run the full Lumen and TRust suites, optimized release builds, browser
  acceptance matrix, and system `js-engine-benchmark` before promotion. The
  Lumen workspace is green; the current Lumen-enabled TRust suite passes 987
  active library tests plus all 28 desktop tests; release `trust` and
  `trust-desktop` build; the YouTube and Speedometer gates pass after the
  lifecycle fixes; prior Twitch/Steam matrix and system benchmark baselines
  remain recorded above. The built artifacts remain uninstalled pending
  explicit approval.

## Correctness and containment

- [x] Implement normative WebAssembly binary/module/instruction validation,
  checked decoding, and bounded module/store allocations.
- [x] Move object and scope GC registries from thread-local state to explicit
  Agent/interpreter ownership across coroutine handoffs.
- [x] Replace recycled thread-local shape identifiers with collision-free,
  Agent-owned shape identities.
- [x] Implement the HTML timer initialization/repeat algorithm, including
  zero-delay intervals, nesting clamps, and no bulk missed-tick catch-up.
- [x] Split WebSocket read/write ownership and enforce RFC 6455 masking,
  framing, entropy, cancellation, and idle behavior.
- [x] Centralize Fetch header validation and RFC 9112 client/server message
  framing, including redirect header mutation and size limits.
- [x] Replace or harden the blocking-I/O pool with bounded queues, cancellation,
  panic containment, shutdown guarantees, and long-lived-resource isolation.
- [x] Sweep every pointer-keyed GC side table and implement sound WeakRef and
  FinalizationRegistry retention semantics.
  - [x] Count every `RealmState` intrinsic handle—including the realm-specific `%Array%`
    constructor—as an internal collector edge so unreachable secondary realms are reclaimable.
- [x] Give the global symbol registry explicit ECMAScript Agent lifetime and
  remove permanent retention of ordinary Symbols.
- [x] Report RegExp resource exhaustion separately from no-match and integrate
  engine interruption/deadline checks.
  - [x] Compile UnicodeSets string properties into compact forward/backward tries and execute
    repeated string atoms with bounded heap backtracking, preserving longest-first, singleton,
    empty-string, greedy/lazy, and lookbehind semantics without native-stack growth.
- [x] Prevent the native JIT exception unwinder from restoring handler stack depth past the
  current live operand depth, which resurrected moved values and double-freed thrown objects when
  abrupt `for…of` bodies performed IteratorClose.
- [x] Add streaming, byte-bounded decompression and bounded snapshot decoding.
  - [x] Enforce shared regenerated-byte limits and strict RFC/Compression Standard framing
    across DEFLATE, zlib, gzip, Brotli, and Zstandard.
  - [x] Bound snapshot input, allocation, integer conversion, and nesting before allocation.
  - [x] Give raw DEFLATE, zlib, and gzip CompressionStream/DecompressionStream real incremental
    native contexts, 32 KiB decode histories, chunked output, checksums, and GC-safe cleanup.
  - [x] Replace flush-only Brotli buffering with incremental native encoder/decoder contexts,
    advertised-window history retention, chunked output, and early compressed output.

## Web-platform conformance

- [x] Complete URL parsing/serialization, setter state overrides, and IDNA behavior against the
  current URL Standard and its full data-driven WPT corpus.
  - [x] Implement URLSearchParams' Web IDL union/record conversion order, USVString collision
    behavior, optional-undefined semantics, and live list operations.
  - [x] Enforce the host-state empty-buffer failure before Servo `url` 2.5.8 can construct an
    invalid component record for a non-special URL with a port or credentials.
  - [x] Adapt the remaining version-lagging Servo parser behavior for file URLs, non-special
    paths, opaque-path spaces, ASCII-domain compatibility, origins, and component state overrides;
    use Unicode 17 ICU4X data for current UTS #46 processing. The 23 runner-compatible URL WPT
    files pass 4,756/4,756 subtests at pinned revision
    `54078e9ec9d5c73f8815ff38b42b8fcbf1f3200b`.
- [x] Implement stateful Encoding Standard decoders and decoder streams.
- [x] Port the Streams Standard controller, backpressure, BYOB, tee, and piping
  state machines with focused WPT coverage.
- [x] Implement DOM event dispatch state, redispatch guards, phases, and reset
  behavior.
- [x] Implement structured serialization transfer lists, detachment, graph
  identity, holes, views, and worker transfer behavior.
- [x] Expand ECMA-402/CLDR locale data and algorithms beyond the current
  deliberate subsets.
  - [x] Replace approximate `Intl.Segmenter` boundaries with Unicode 17 UAX #29
    grapheme, word, and sentence algorithms, generated property tables, and the
    official Unicode conformance corpora.
  - [x] Replace hand-written plural-rule subsets with generated CLDR cardinal,
    ordinal, and range rules and complete plural operands.
  - [x] Audit the remaining number/date/list/relative-time/display-name and
    collation data fallbacks against current ECMA-402 and CLDR.
    - [x] Generate all conjunction/disjunction/unit list patterns for advertised
      locales from CLDR 48 and implement full/context-sensitive pattern assembly.
    - [x] Replace RelativeTimeFormat's English/Polish-only phrases and unit forms.
    - [x] Replace DisplayNames' English-only subset.
    - [x] Expand number/date patterns and collation weights beyond current subsets.
      - [x] Generate CLDR number symbols, grouping, affix, currency, unit, and compact patterns.
      - [x] Generate CLDR date/time skeleton, style, connector, and range patterns.
      - [x] Implement Unicode Collation Algorithm weights plus CLDR locale tailorings.

## Efficiency and lifecycle

- [x] Expand compiled-tier coverage for standards-sensitive control flow and
  constructors before optimizing around benchmark behavior.
  - [x] Compile base class constructors after their normative pre-body instance
    initialization and derived constructors through a real uninitialized-`this`
    Function Environment Record. Focused bytecode/JIT tests cover `super()`,
    `new.target`, field ordering, return validation, arrow capture, and double
    `super()`; the forced-JIT Test262 class directory passes 4,367/4,367.
  - [x] Compile ordinary synchronous `try`/`finally` through the existing
    completion-aware VM handler while retaining native-JIT fallback. Focused
    tests cover normal/throw/expression-return/bare-return/break/continue,
    nesting, catch environment restoration, and abrupt finalizer replacement;
    forced-tier Test262 `language/statements/try` passes 201/201.
  - [x] Preserve optional-chain short circuiting across private field/method
    tails and retain the receiver for a live private method Reference. The exact
    previously failing Test262 case and the full forced-JIT class directory pass.

- [x] Replace one-native-thread-per-live-coroutine with explicit VM
  continuations.
  - [x] Run compiler-supported sync and async `yield`/`await` bodies as
    heap-owned VM continuations with no native stack or channel handoff.
  - [x] Model synchronous, async, and async-from-sync `yield*` delegation
    completion records in the VM and remove delegation's stackful compatibility
    path, including iterator-close and abrupt-resumption conformance cases.
  - [x] Lower `for (var ... of ...)` identifier and destructuring heads into the hoisted
    VariableEnvironment homes instead of creating incorrect per-loop slots or falling back.
  - [x] Use compiler-supported heap continuations even when the ordinary execution tier is
    explicitly `interp`; tier selection no longer reintroduces native async workers.
  - [x] Seed coroutine VM frames from the already-instantiated formal parameter bindings so
    generator/async default initializers run exactly once, while preserving the original call
    list for `arguments` objects.
  - [x] Seed flattened parameter BoundNames for coroutine chunks, allowing already-instantiated
    rest and destructuring parameters (including captured/defaulted leaves) to use heap VM
    continuations without replaying binding semantics.
  - [x] Resolve coroutine `arguments` reads through the already-instantiated Function Environment
    Record, preserving object identity for nested arrows and supporting parameterless, strict,
    and otherwise-unmapped parameter lists.
  - [x] Reuse the already-instantiated Function Environment Record for nonempty sloppy simple
    parameter lists, so mapped `arguments` indices and parameter bindings share storage in both
    directions across suspension without replaying parameter initialization.
  - [x] Lower identifier and member logical assignments with one Reference evaluation, normative
    short-circuiting, NamedEvaluation, and hidden base/key homes that survive `yield`/`await`.
  - [x] Preserve normal, throw, source-return, and externally resumed-return completions across
    suspending `try`/`finally` regions for sync generators, ordinary async functions, and async
    generators; a finalizer's abrupt completion correctly replaces the saved completion, while
    AsyncGeneratorUnwrapYieldResumption is not awaited a second time after finalization.
  - [x] Carry break/continue Completion Records through nested suspending finalizers, retaining
    the destination's exact handler depth. Abandoned nested `for…of` iterators close inside-out
    between inner and outer finalizers, and close errors retain their normative catch/replace
    behavior.
  - [x] Make coroutine `for…of` handlers completion-aware so source returns and externally
    injected generator returns always perform IteratorClose. Preserve the distinct no-Await
    semantics of bare async-generator `return;`, and Await `return expression` before its Return
    Completion enters finalizers or iterator closing.
  - [x] Lower labelled non-loop statements as break-only control contexts, including stacked
    labels and labelled breaks crossing suspending finalizers, without stealing unlabelled
    break/continue from nested loops or switches.
  - [x] Remove the bounded native fallback for generator/async bodies containing
    other constructs the bytecode compiler still cannot lower.
    - [x] Lower `for await…of` with async iterator acquisition, awaited stepping/closing, and
      completion-aware disposal. Native and async-from-sync paths retain their distinct Call/Await
      timing, live value rejections close in the adapter reaction, normal/throw Completion
      precedence survives awaited close, external async-generator returns dispose correctly, and
      nested abandoned iterators close inside-out without native worker threads.
    - [x] Lower `for…in` candidate enumeration with one standards-ordered key walk, per-step
      deletion checks, lexical/var/destructuring declaration heads, and suspending bodies.
    - [x] Lower member-expression assignment heads for `for…in`/`for…of`, evaluating their
      base/key Reference once per iteration after the step value is obtained; abrupt Reference or
      PutValue completion remains inside the loop's IteratorClose region.
    - [x] Lower the remaining destructuring-assignment loop heads.
      - [x] Run non-suspending array/object assignment heads in heap VM continuations through one
        shared normative AssignmentPattern operation, including local-slot projection, partial
        writes on later failure, nested IteratorClose, defaults, rest, computed keys, and members.
      - [x] Lower direct `yield`/`await` suspension inside assignment-pattern targets/defaults,
        preserving Reference-before-step/GetV ordering, nested iterator `[[Done]]`, normal/throw/
        injected-return IteratorClose, computed keys, array/object rest, partial writes, and
        immutable-target errors in heap-owned VM frames.
    - [x] Complete destructuring defaults/rest/computed keys and literal spread/method/accessor
      forms without changing observable evaluation order.
    - [x] Keep statically known immutable writes in heap continuations for simple, compound,
      logical, update, and destructuring forms while preserving RHS, ToNumeric, and TDZ error
      ordering.
    - [x] Lower catch-parameter BindingInitialization through the suspension-aware declaration
      pattern machinery, including defaults, rest, IteratorClose, and catch/finally interaction.
    - [x] Lower classic `for` declaration-head binding patterns for `var`, `let`, and `const`,
      retaining TDZ-before-initializer ordering and suspension-aware defaults, computed keys,
      rest, and IteratorClose.
    - [x] Split function-body lexical binding patterns between activation-homed captured leaves
      and fast uncaptured slots, preserving declaration-wide TDZ and suspension-aware
      BindingInitialization without forcing a native worker.
    - [x] Instantiate uncaptured strict block and switch function declarations at scope entry in
      heap continuations, preserving pre-declaration visibility, fresh identity on repeated block
      entry, and switch-wide initialization before case tests.
    - [x] Lower sloppy Annex B.3.2 block functions with distinct lexical and promoted-var homes,
      declaration-time synchronization, synthetic if-clause blocks, parameter/`arguments` and
      early-error blockers, and simultaneous closure captures of both bindings. The focused
      official Test262 function-code/if slice passes 160/160 in forced bytecode mode.
    - [x] Lower classes (`super`, private names, fields/static blocks), dynamic import metadata,
      tagged templates, and other remaining expression forms.
      - [x] Lower tagged templates, `import.meta`, dynamic/import-source calls, private references,
        and `new.target` through suspension, preserving tag receiver/early-callability ordering,
        template-object identity, import option order, and private/super Reference state.
      - [x] Evaluate ordinary class definitions atomically and stage suspending heritage and
        computed ClassElementName evaluation in continuation-owned class/private environments.
        Preserve strict mode, class-name TDZ, superclass validation and prototype access,
        ToPropertyKey, private-name captures, inferred names, abrupt cleanup, and computed static
        `prototype` error ordering.
      - [x] Model proposal-decorator evaluation with continuation-owned `(this, callback)` records:
        class expressions run before heritage, member expressions interleave with computed names,
        reverse application retains natural receivers, and every stage may suspend without a
        native worker. Focused official decorator syntax/staging coverage passes 23/23.
      - [x] Keep named-class self bindings in the distinct ClassDefinitionEvaluation `classEnv`
        during capture analysis, so methods and initializers can close over their own class without
        misclassifying that engine-owned environment as a coroutine activation scope.
      - [x] Lower arbitrary/interleaved spread argument lists for calls, optional calls, and
        constructors while preserving receiver binding, short-circuiting, and evaluation order.
      - [x] Lower delete references, including optional-chain short-circuiting, environment
        bindings, and the distinct computed/uncomputed `super` evaluation order.
      - [x] Preserve the actual method receiver as a Super Reference's `[[ThisValue]]` in
        compiled frames, including named/computed logical assignment and lean calls that do not
        materialize an activation environment.
      - [x] Read `new.target` from the retained Function Environment Record in generator and async
        VM continuations, including async arrows that inherit it after their defining constructor
        has returned, without opening the unsafe lean-frame case.
      - [x] Resolve named generator/async function-expression self-bindings through their retained
        immutable declarative environment, including recursive calls, body-var shadowing, and
        strict/sloppy assignment behavior. Restore the source function's strictness on every heap
        continuation slice rather than inheriting the resuming host job's mode.
      - [x] Resolve async-arrow `this` lexically through the retained defining environment and seed
        the heap continuation with that value, ignoring later call-site receivers and preserving it
        for nested arrows after suspension.
      - [x] Lower `super(...)` in async arrows nested within derived constructors. Retain
        GetNewTarget/GetSuperConstructor before suspending argument evaluation, resolve lexical
        `this` at the actual access point, construct before BindThisValue, initialize instance
        elements after binding, and shield the capability at intervening ordinary-function
        environments. The forced-bytecode Test262 super/arrow/direct-eval gate passes 723/723.
      - [x] Run atomic direct eval through the normative evaluator against a fully observable
        coroutine activation, homing function-scope parameters/vars/lexicals so sloppy eval-created
        bindings and closures persist while strict eval remains isolated. Preserve a free
        assignment's pre-RHS Environment Reference when eval creates a nearer `var`, and preserve
        assignments spanning suspension plus destructuring defaults whose eval can change a
        previously resolved free-name target by retaining opaque Environment References directly
        in the heap continuation.
      - [x] Retain exact block, loop, switch, and catch lexical environments for direct eval;
        distinguish a `with` property Reference from a direct call; and lower arbitrary suspended
        ArgumentListEvaluation (including spreads) before choosing direct PerformEval versus an
        ordinary shadowed call. The complete forced-bytecode Test262 `language/eval-code/direct`
        slice passes 286/286.
    - [x] Move compiler-supported Source Text Module top-level-await evaluation into a strict,
      continuation-owned AsyncBlock over the already-instantiated module environment. Preserve
      live imports/exports, TDZ and immutable cells, default-export naming, module completion
      cascades, top-level await in proposal-decorator expressions, and fresh lexical `for…in`,
      `for…of`, and `for await…of` heads that shadow same-spelled module bindings. The focused
      Test262 `language/module-code/top-level-await` suite passes 251/251.
    - [x] Replace the native `Array.fromAsync` iterator worker with an explicit heap continuation
      that preserves normative Await boundaries, async-from-sync wrapping, iterator closing, and
      promise rejection ordering. The focused Test262 `built-ins/Array/fromAsync` suite passes
      95/95.
    - [x] Model `using`/`await using`, `with`, and switch lexical environments in resumable frames.
      - [x] Keep synchronous and asynchronous DisposableResource stacks on each heap continuation
        and lower function-body/block, classic-`for`, and per-iteration `for…of` declarations
        through completion-aware disposal pads. Preserve registration-before-initialization,
        reverse disposal, exact `DisposeResources` await markers and sync fallbacks, suppressed-error
        precedence, abrupt generator resumption, disposal-before-iterator-close ordering, and
        captured once-per-call block resources without flattening their immutable semantics.
      - [x] Instantiate one shared switch lexical scope after discriminant evaluation and before
        case matching, preserving TDZ, fall-through, class/function declarations, and captured
        bindings across suspension. Allocate a fresh retained record whenever a containing loop
        re-enters the switch.
      - [x] Carry the active lexical-environment cursor in heap VM continuations and lower
        suspending `with` object/body evaluation, including nested environments, primitive
        ToObject wrappers, `Symbol.unscopables`, implicit call receivers, and restoration before
        normal, throw, return, break, or continue completions. Retain resolved Environment
        References across suspension so simple/compound/logical/destructuring writes cannot be
        rerouted by later object or unscopables mutations.
      - [x] Allocate resumable Declarative Environment Records only for closure-captured bindings
        in re-entered blocks and lexical classic-`for`, `for…in`, `for…of`, and `for await…of`
        heads. Preserve the separate uninitialized RHS environment, classic-for copy points,
        destructuring, `using`/`await using`, suspension, and restoration before disposal,
        IteratorClose, and outer finalizers; uncaptured bindings remain allocation-free slots.
      - [x] Give closure-captured catch parameters a fresh resumable declarative environment per
        CatchClauseEvaluation, distinct from the nested catch Block environment. Preserve
        destructuring/IteratorClose order, suspension, and restoration before outer finalizers.
      - [x] Promote every supported inner declaration scope when multiple captured bindings reuse
        the same spelling. Keep sibling, nested, re-entered block, loop-head, and catch records
        distinct while rejecting only captures that also resolve through an unsupported or
        function-wide declaration identity.
    - [x] Materialize a parameterless synchronous function's `arguments` object in its activation
      only when an inner arrow captures it, preserving object identity without taxing uncaptured
      variadic helpers. The official forced-bytecode lexical-arguments regression now passes.
  - [x] Delete the legacy worker pool, channel handoff, unsafe cross-thread interpreter transfer,
    lazy native generator bodies, stack-size/live-worker configuration, and module fallback.
    Future compiler coverage regressions produce a contained JavaScript rejection from a
    bodyless suspended-start sentinel instead of executing source on a native stack. The combined
    forced-bytecode generator/async/top-level-await Test262 gate passes 1,958/1,958 with zero
    sentinel audit events.
- [x] Share WebAssembly memory backing directly with JS and reclaim store
  modules/entities when their handles die.
  - [x] Identify `Memory.buffer` with the store's linear-memory Data Block and
    synchronously detach/refresh fixed buffers after API and instruction growth.
  - [x] Use weak JS address caches plus finalizer-owned graph roots so modules,
    callbacks, buffers, instances, and entity allocations are traced and reclaimed.
- [x] Add origin-keyed HTTP keep-alive and cache OpenSSL API/client/server
  contexts.
  - [x] Partition the agent-local bounded idle pool and TLS session scopes by
    effective origin and Fetch credentials, and retry stale transports only for
    idempotent methods.
  - [x] Reuse only completely consumed, self-delimited RFC 9112 responses and
    preserve each connection's read-ahead buffer.
  - [x] Load the OpenSSL API once, reuse client contexts, and retain one server
    context per listener instead of reparsing certificates for every accept.
- [x] Convert retained-string, RegExp, and related caches to byte-accounted LRU
  budgets.
  - [x] Bound UTF-16 string views and prepared RegExp subjects by retained source,
    materialized element/offset storage, metadata count, and true recency.
  - [x] Account compiled matcher programs (including nested lookarounds, character
    classes, names, and lookup tables) across all source/flag variants while
    preserving fresh ECMAScript RegExp object identity.
- [x] Remove duplicate parser source indexing/allocation.
  - [x] Share the lexer's single Unicode code-point index with script, module,
    and template-substitution parsers while preserving exact ECMA-262
    `[[SourceText]]` slices. Release benchmark after the change: 67.93 µs
    parse for the kitchen-sink fixture and 10.60 ms for the 500-function fixture.
- [x] Measure TRust workloads and move benchmark-specific JIT regions behind
  general IR passes; retire specializations without production value.
  - [x] Profile TRust's DOM boundary, mutation-observer, and serialization
    workloads with region telemetry. None selected the Richards scheduler
    family; disabling that family produced 12.230/1.808/0.602 s versus
    12.225/1.775/0.564 s event-loop samples (noise-level differences).
  - [x] Remove the Richards-only scheduler planners, graph epochs, native
    emitters, environment switches, cache-discovery helpers, and fixture-only
    regression matrix. This reduced `jit.rs` from 22,674 to 12,673 lines.
  - [x] Retain the production-relevant loop lowerings only after general
    CFG/SSA `RegionIr` validation. Post-removal TRust samples were
    12.265/1.839/0.578 s, and the Richards 100-run probe remained within the
    observed pre-change range (431 ms post-change; 417--458 ms pre-change).
- [x] Add guarded dense-array construction paths for intrinsic `Array.from`, TypedArray iterable
  initialization, and full-range fills of fresh holey arrays. Preserve Realm-specific constructors
  and fall back for holes, accessors, proxies, indexed prototype setters, patched iterator methods,
  or any other observable iteration. The three 10,000-element detached-buffer Test262 stress cases
  fell from watchdog-scale execution to 7.8 seconds together.

## Verification gates

- [x] Run the current full Test262 checkout and record its exact revision.
  - [x] Revision `d86b2294eb0a17eaa281ff12c73c473ec864c72f` (2026-08-25):
    53,574 passed, 0 failed, and 4 documented upstream skips.
- [x] Add/run focused WPT subsets for every implemented web API.
  - [x] Add a pinned, sparse-checkout WPT runner with JavaScript-shell harness support and bounded
    same-origin fixture loading.
  - [x] Pass the focused DOM Events, Encoding/base64, URLSearchParams, High Resolution Time,
    WebCrypto, Fetch value/body, and Streams slices.
  - [x] Bring the URL parser, component setters, URLSearchParams, URL-encoded parser, and Unicode
    17 IDNA processing through every runner-compatible URL WPT file. The combined 64-file manifest
    passes 6,306/6,306 subtests at revision
    `54078e9ec9d5c73f8815ff38b42b8fcbf1f3200b`.
- [x] Run the WebAssembly core specification tests for the supported feature
  set and add malformed-binary/validation fuzzing.
  - [x] Pin the official WebAssembly 2.0 core suite at revision
    `05ca4182176763112561ae20153975c12bd689e4`; the 75-suite scalar,
    control-flow, linking, single-memory, funcref-table, multi-value, sign-extension,
    non-trapping-conversion, and bulk-memory manifest passes 24,197/24,197 directives.
  - [x] Correct trapping float-to-integer conversions, Wasm NaN/signed-zero `min`/`max` and
    nearest behavior, start functions, cross-instance function identity, multiple table imports,
    ordered instantiation side effects, typed select/reference-null instructions, stack-exhaustion
    containment, and passive data-segment initialization/drop semantics found by the suite.
  - [x] Add a deterministic malformed-binary decoder/validator corpus; 100,000 mutations pass
    without a panic, including strict custom-section name and LEB128 decoding.
- [x] Run Autobahn WebSocket tests and adversarial RFC 9112 framing tests.
  - [x] Run Autobahn Testsuite 25.10.1/AutobahnPython 0.10.9's 247 applicable client cases
    (excluding the mass-performance and unimplemented compression-extension categories): 244
    normative cases pass strictly, 3 close-race/code cases are informational, and none fail or
    report non-strict behavior. Add a reproducible in-tree testee and gate.
  - [x] Validate RFC 6455 text incrementally across transport and continuation-frame boundaries,
    failing a provably invalid prefix with close code 1007 before buffering the remainder. This
    changed Autobahn cases 6.4.1--6.4.4 from non-strict to strict passes.
  - [x] Add adversarial RFC 9110/9112 list, TE/CL precedence, line syntax, chunk extension,
    trailer, overflow, incomplete-body, and resource-limit coverage. Accept recipient-side empty
    list elements and chunk-extension BWS, and consume a declared 205 wire body before discarding
    its Fetch-null body so a persistent connection cannot desynchronize.
- [x] Resolve the HTTP/2 interoperability regressions.
  - [x] Queue all client frames created before transport connection so the wire order is the
    RFC 9113 client preface, SETTINGS, HEADERS, then DATA.
  - [x] Prevent servers from advertising the forbidden `SETTINGS_ENABLE_PUSH = 1` value and make
    the Node server-interoperability fixtures readiness-driven. Cleartext/TLS client, server,
    frame, and HPACK tests all pass.
- [x] Run formatting, Clippy, workspace tests, release Lumen, and release TRust
  before acceptance handoff.
  - [x] Lumen formatting and warnings-denied all-target Clippy are clean; the full workspace suite
    passes, including the runtime, HTTP/TLS, Node/Bun compatibility, WebAssembly, and doc-test
    targets (two explicitly documented tests remain ignored).
  - [x] The complete optimized Lumen workspace builds successfully.
  - [x] TRust formatting and warnings-denied all-target Clippy are clean; 944 library tests and 28
    desktop tests pass (16 explicitly documented manual/diagnostic tests remain ignored), and the
    optimized `trust` and `trust-desktop` binaries build successfully against this Lumen tree.
