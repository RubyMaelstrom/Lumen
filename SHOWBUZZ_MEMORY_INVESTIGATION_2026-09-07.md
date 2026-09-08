# ShowBuzz background-window memory runaway

Follow-up: implementation and validation are tracked in
[the memory-fix report](SHOWBUZZ_MEMORY_FIX_2026-09-07.md). The diagnosis below
records the original, pre-fix experiments.

## Verdict

Reproduced on both the installed production desktop and the current optimized
release desktop, using the real NVIDIA GB10 Hybrid renderer. Covering the window
starts multi-gigabyte growth. Uncovering it releases most of that growth within
five seconds, without navigation, restarting the page, or forcing garbage
collection.

The evidence strongly identifies a presentation-stall / unbounded-page-update
backlog, not a multi-gigabyte JavaScript heap or graphics-texture leak. The native
UI thread blocks inside Vulkan/Wayland presentation while the resident page actor
continues producing complete presentation snapshots. The controller bridge
forwards them into an unbounded channel, defeating the actor's bounded output.

This is a diagnosis, not a completed fix. No browser or engine implementation
files were changed for this investigation. Existing optimization work and other
pending source changes were preserved. No installed binary was replaced.

## Reproduction and measurements

Artifacts and executable copies:
`/big/Code/Lumen/benchmark-results/showbuzz-memory-20260907-tUUnOQ/`.

The `soak.mjs` harness creates a private headless Labwc compositor and a fresh
browser profile. It opens `https://showbuzz.org/`, hides the browser command bar,
then makes no page interactions. At 60 seconds a private opaque terminal covers
the entire browser and takes focus. `/proc` memory and CPU samples are recorded
every five seconds. GPU cases use a GLES2 compositor backed by `renderD128`;
browser logs confirm `renderer=hybrid adapter=NVIDIA GB10`. Pixman compositor
cases use the browser's Vello CPU fallback.

These were memory-retention experiments, not isolated throughput comparisons:
some runs overlapped, and the live site can change between requests.

| Run | At 60 s, before covering | Later RSS | Result |
| --- | ---: | ---: | --- |
| Production, CPU | 552.80 MiB | 606.44 MiB at 900 s | Modest drift, no multi-GB runaway |
| Current release, CPU | 563.58 MiB | 593.04 MiB at 900 s | Modest drift, no multi-GB runaway |
| Production, GPU | 633.34 MiB | 6,199.92 MiB at 510 s | Automatic RSS safety stop |
| Current release, GPU | 691.90 MiB | 2,765.95 MiB at 180 s | Uncover intervention below |
| Current release, GPU, profiler | 621.42 MiB | 3,868.90 MiB at 225 s | Independent uncover replication |

The clean production GPU run grew about 12.36 MiB/s between seconds 60 and 480.
The clean release GPU run grew about 17.28 MiB/s between seconds 60 and 180.
These rates are easily sufficient to explain exhausting RAM over a long
background session. They are not a controlled performance-regression comparison.

### Uncovering reverses the runaway

For `release-gpu-background-01`, the cover was manually terminated at
2026-09-07 19:07:45 UTC, approximately 180.4 seconds after startup:

| Elapsed | RSS | Anonymous resident memory |
| --- | ---: | ---: |
| 180 s, covered | 2,765.95 MiB | 2,547.55 MiB |
| 185 s, uncovered | 791.51 MiB | 573.26 MiB |
| 300 s, still uncovered | 807.48 MiB | 589.57 MiB |

The profiler-assisted repeat independently fell from 3,868.90 MiB at 225 seconds
to 825.98 MiB at 230 seconds after its automatic uncover, and 798.89 MiB at
240 seconds. Profiler overhead makes its absolute rate unsuitable for comparison
with clean runs. The reversible growth itself reproduces in both lanes.

The smaller CPU drift and the higher post-uncover plateau have not been proved
leak-free. This investigation isolates the dominant runaway; it does not certify
every cache or every long-lived allocation.

### Safety and cleanup

Runs used private systemd user units with 8 GiB cgroup memory limits and swap
disabled. The harness stops its browser at 6 GiB RSS; later runs also check a
10 GiB minimum host-memory reserve. Production hit the RSS limit at 510 seconds.
The CPU cases completed 15 minutes. The clean release GPU case was stopped after
the foreground recovery had been observed; the profiler case completed four
minutes. All investigation browser/compositor/cover processes were stopped.
No user browser or unrelated process was terminated.

### Exact desktop binaries

- Installed production copy, SHA-256:
  `35113368354d6c266ee4eb3509a3a349f544e64718b6a7e9f5a283861619ed6b`.
- Current release copy, SHA-256:
  `ffb2cacf5d9619153f94f4c663a278d5565de9e0d2e54d32218119955278d125`.

Both are optimized desktop binaries. No debug browser was used.

## Native evidence and ownership chain

1. The covered UI thread's stack was sampled twice, more than two minutes apart,
   at the same wait: `DesktopApp::draw` → `present_scene` →
   `VelloHybridRenderer::present_damage` → wgpu queue presentation →
   `NativeSwapchain::present` → NVIDIA EGL/Vulkan library →
   `wl_display_dispatch_queue` → `ppoll`.
   The later capture is preserved in
   `release-gpu-heap-01/covered.stack.txt`, with capture time and `/proc` snapshots
   alongside it. This is a wait in **present**, not the already-handled
   surface-acquisition Timeout/Occluded branch.
2. Memory maps from the clean release's growing phase put the large resident
   increase in `[anon:mimalloc]` arenas. GPU mappings were not the multi-GB
   increment. See `growing.smaps` and `before-uncover.smaps` in that run's folder.
3. Immediately after the uncover intervention, an additional live stack caught
   the UI thread freeing `DisplayCommand`, `PagePaint`, `GraphicalLayout`, and
   `RenderedPage` objects inside `BrowserController::process_async_events`.
   This initial stack was returned in the tool transcript; the separately saved
   later `uncovered.stack.txt` is a post-recovery sample, not that exact instant.
4. Source inspection identifies the matching ownership path:
   - `src/lumen_backend.rs:1166`: actor output is a Tokio channel with capacity 16.
   - `src/lumen_backend.rs:2976`: visual updates send HTML plus an `Outcome`
     containing a complete `RenderedPage`.
   - `src/core/mod.rs:510`: the controller creates an unbounded standard channel.
   - `src/core/mod.rs:1328`: `attach_live_page` drains the bounded actor channel
     and forwards every event to that unbounded channel.
   - `src/core/mod.rs:967`: only `process_async_events` drains that controller
     queue; desktop invokes it on its native UI thread.
   - `src/http.rs:175`: `RenderedPage` owns presentation metadata and retains its
     `GraphicalLayout` through an Arc. Distinct updates can therefore keep
     distinct full layouts alive until their queued messages are consumed.

Consequently, a stalled native presenter does not slow the forwarding task or
bound its retained snapshots. The actor's output limit provides no end-to-end
backpressure. This also explains why uncovering suddenly frees large numbers of
layout objects.

Exact queued-message counts and per-snapshot retained-byte totals were not
instrumented. The cause is strongly supported by the intervention, native stacks,
memory mappings, and matching source ownership; it is not based on an allocation
census claiming to account for every byte. gprofng's libc heap tracing is not an
inventory of the statically linked Rust mimalloc heap. Its heap report assigned
the reported allocations/leaks to `__collector_get_frame_info` rather than useful
browser allocation sites, so those totals were rejected as retention evidence.
The CPU samples did resolve browser symbols; they are supplementary only.

## Repair direction

Two boundaries need attention; neither requires changing JavaScript semantics or
weakening rendering correctness.

1. **Bound the native presentation handoff.** Keep only the newest replaceable
   complete paint snapshot when the consumer falls behind. Preserve document
   generations and semantic-event barriers. Do not discard navigation, history,
   input-default results, errors, console records, or incremental patches whose
   successors depend on them. A bounded channel that merely parks the whole JS
   actor forever behind the driver is not a complete responsiveness fix.
2. **Integrate compositor frame pacing.** Neither desktop nor the Hybrid adapter
   currently calls `Window::pre_present_notify`. In the linked winit 0.30.13,
   this requests the Wayland frame callback that gates subsequent redraws.
   Wire it immediately before actual presentation, then verify the same covered
   window case. Its absence is an identified integration gap; the notification
   alone has not yet been experimentally proved sufficient to eliminate the
   driver stall. Queue bounds must hold even if presentation still blocks.

If the driver can still block the UI indefinitely after correct frame pacing,
move potentially blocking GPU presentation off the native UI/controller thread,
using a bounded latest-frame handoff. Do not introduce an unbounded render-worker
queue in its place. Do not equate losing keyboard focus with being invisible:
an unfocused window can still be fully visible.

Recommended acceptance tests:

- Slow/paused presentation consumer: thousands of complete updates with bounded
  retained snapshot count, then resume directly to the newest correct page.
- Navigation/generation changes and semantic-event barriers during the stall;
  incremental patch handling and error/console preservation.
- Real release Hybrid Wayland covered-window soak, repeated cover/uncover,
  minimize/restore, resize, and visible-but-unfocused windows. Use a memory guard
  and a long post-fix run; a headless GPU readback test cannot reproduce the
  blocked desktop swapchain path.
- Correct pixels, current clock, hit testing, and input after exposure, without
  replaying obsolete frames or sacrificing the existing partial-damage rules.
- CPU/reference rendering and affected shared-controller tests remain green.

## Standards and primary-source basis

The local web-standards skill was used to constrain the proposed repair, not to
change standards-governed behavior during this diagnostic pass.

- WHATWG HTML local snapshot
  `e5071a20c8569d8a3ec02ed27dd01b948773f850`, fetched 2026-09-06:
  `/big/web-standards/repositories/whatwg/html/source:123167`,
  [update the rendering](https://html.spec.whatwg.org/multipage/webappapis.html#update-the-rendering),
  and `source:123454`,
  [rendering opportunity](https://html.spec.whatwg.org/multipage/webappapis.html#rendering-opportunity).
  Rendering opportunities account for presentation constraints, and redundant
  rendering can be coalesced. This is not permission to discard semantic events.
- The same HTML snapshot, `source:85112`,
  [page visibility](https://html.spec.whatwg.org/multipage/interaction.html#page-visibility):
  actual visibility and its event transitions are distinct from keyboard focus.
- Installed Wayland protocol source `/usr/share/wayland/wayland.xml:1628`,
  `wl_surface.frame`: frame callbacks pace drawing; compositors should avoid
  signaling them for completely obscured surfaces.
- Exact linked winit 0.30.13 source:
  `/home/ruby/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/winit-0.30.13/src/window.rs:600`,
  [pre_present_notify](https://docs.rs/winit/0.30.13/winit/window/struct.Window.html#method.pre_present_notify),
  and its Wayland event loop `src/platform_impl/linux/wayland/event_loop/mod.rs:486`.
  The implementation gates redraw delivery while a requested frame callback is
  outstanding. This is API implementation evidence, not a web standard.
