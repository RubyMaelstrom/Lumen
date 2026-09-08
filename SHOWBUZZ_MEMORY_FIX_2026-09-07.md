# ShowBuzz covered-window memory fix

Follow-up to [the reproduction and diagnosis](SHOWBUZZ_MEMORY_INVESTIGATION_2026-09-07.md).
This round changes TRust's native presentation boundary, not Lumen's JS heap or
the layout engine. The previously identified unbounded handoff is now bounded,
and both native presenters honor compositor frame pacing.

## Implementation

- `TRust/src/core/events.rs`: bounded asynchronous-producer/nonblocking-consumer
  channel with a maximum of 16 queued events. Adjacent complete, quiet page
  snapshots for the same generation and cumulative fetch count supersede one
  another. Quiet paint-only traffic occupies one queue slot, even with the UI
  consumer completely paused. Superseded layouts are dropped outside the lock.
- `TRust/src/core/mod.rs`: all four producer paths use the bounded handoff:
  navigation fetch, POST fetch, declarative refresh, and resident actor output.
  Semantic events retain FIFO order; full queues apply asynchronous backpressure.
  Shutdown wakes pending senders and releases queued layouts. Native wakeups are
  emitted only on the empty-to-nonempty transition, not for every discarded paint.
- `TRust/src/render/vello_hybrid.rs` and `src/bin/trust-desktop.rs`: call
  `Window::pre_present_notify` immediately before real GPU/CPU presentation.
  Wayland frame callbacks now pace redraws while fully obscured. Unchanged or
  skipped frames that do not commit a surface do not arm a callback.

No focus-based suspension, lost JS tasks, dropped navigation/input results,
layout shortcuts, forced collections, site-specific branches, or new page-memory
limits. The queue preserves incremental patches, diagnostics, generation changes,
and static/final events. In particular, the actor reports a **cumulative** fetch
counter: repeated nonzero counts still permit quiet snapshot replacement.

## Build and artifacts

Artifacts, copied executable, logs, five-second resource samples, native stacks,
screenshots, and private-compositor harnesses:
`/big/Code/Lumen/benchmark-results/showbuzz-fix-20260907-iDomRK/`.

```sh
CARGO_PROFILE_RELEASE_STRIP=none cargo build --release --bin trust-desktop --bin trust --bin trust-headless
```

This is optimized ThinLTO release code, not a debug desktop. Symbol preservation
allows useful native stack captures and matches the pre-fix measurement binary.
Build time: 3m 56s. Frozen desktop SHA-256:
`0af934991b9183dc3bc0e680378536e7e6d31a45a2ab8b234e26b3a5cea23e58`.
No installed binary was replaced or promoted. Unrelated pending work, including
intentional Boa removal, was preserved.

## Acceptance results

Both long runs used fresh private profiles and private Labwc compositors. They
loaded ShowBuzz, covered the entire browser at 60 seconds, and uncovered it at
885 seconds. They completed the full 900-second observation; there was no
navigation, forced GC, restart, or memory-limit intervention. GPU logs confirm
`hybrid adapter=NVIDIA GB10`; the Pixman-compositor control confirms Vello CPU.

| Release desktop run | RSS at 60 s | RSS at 900 s | Peak sampled RSS |
| --- | ---: | ---: | ---: |
| Fixed GPU, fully covered | 724.30 MiB | 793.25 MiB | 820.54 MiB |
| Fixed CPU control, fully covered | 547.13 MiB | 626.23 MiB | 633.88 MiB |

For comparison, the pre-fix production GPU run rose from 633.34 MiB at 60 s to
6,199.92 MiB at 510 s, when the safety guard stopped it. The pre-fix current
release rose from 691.90 MiB at 60 s to 2,765.95 MiB at 180 s. Those were the same
private-compositor covered-window experiment on the same hardware. Live-site
content and concurrent machine load vary, so these are retention comparisons,
not isolated CPU-throughput benchmarks.

The fixed GPU's late covered interval, 420–875 seconds, ranged between 772.37
and 820.54 MiB, ending only 11.79 MiB above its beginning. This removes the
observed 12–17 MiB/s multi-gigabyte runaway. It does **not** certify every cache
or JS allocation leak-free: both lanes still show modest drift, and the soak is
15 minutes, not a one-hour test.

Both covered GPU stacks (`covered-early.stack.txt` and
`covered-late.stack.txt`) are in winit/calloop's ordinary event-loop poll,
**not** blocked inside NVIDIA queue presentation. At the final screenshot,
after 13 minutes 45 seconds continuously covered, the page clock reads
12:50:12 PM at a 19:50:13 UTC capture: current page state, not queued old frames.
Images and loaded upcoming-event content remain present.

An independent eight-minute GPU run completed three cover/uncover cycles. Each
exposure returned to a current clock. Final RSS was 779.76 MiB; peak 802.40 MiB.
Its later input portion initially left Labwc's window switcher up, so those
clicks are **not** accepted as browser-input validation. The harness was repaired
and a separate short run (`gpu-input-02`, 160 seconds) passed these checks without
a switcher grab:

- Minimize at 30 seconds: the compositor shows only its empty background.
  Restore at 60 seconds: the page is visible with the current clock by the
  65-second screenshot, without the switcher overlay.
- Resize from 948×1050 to 1100×850 and back: surface redraw and responsive-layout
  changes occur, and returning to the original size restores its normal layout.
- Visible but unfocused: a focused 300×200 terminal obscures only the bottom
  right. The page clock advances from 12:51:03 to 12:51:13 PM across screenshots
  at 110 and 120 seconds. Focus loss does not suspend the visible page.
- After exposure and resize, native pointer hits activate the page's JS filter
  controls: DJs changes the list to **31** matching acts; Artists changes it to
  **3**, with the expected different cards and selected-button appearance.

The wide responsive layout exposes overlapping carousel/sidebar content. The
frozen **pre-fix** release reproduces the same overlap at 1100×850 in
`before-resize-01/at-85.png`, matching the fixed build's
`gpu-input-02/at-85.png`. This is a separate layout defect, not introduced by
this queue/pacing repair; successful redraw is not a claim of perfect page CSS.
That comparison also reproduced old-build memory accumulation during just
30 seconds minimized: 1,281.51 MiB before restore, versus 737.93 MiB for the
fixed build at the same point. The independent longer covered soaks remain the
primary retention measurements.

All long-run resources are recorded every five seconds in `resources.jsonl`.
`analysis.json` and `analyze.mjs` retain the summarized measurements and
calculation. Private services have per-run memory/time guards; none affected
the acceptance measurements.

All diagnostic browsers, compositors, cover windows, and test services were
stopped after completion. Installed production and unrelated user processes
were untouched. No commit, push, or install was performed in this repair round.

## Automated validation and separate findings

- 29 shared-controller tests passed, including seven new queue tests.
- 29 desktop tests passed, including CPU damage pixels and desktop interaction.
- 46 rendering tests passed, one ignored, serially on the real GPU. CPU/Hybrid
  fixture comparison: mean absolute channel error 0.031678, maximum 3, fraction
  over tolerance 0. Includes damage crops, atlas lifetime, and SVG image pixels.
- Final broad library suite, run serially under a PTY: **1,139 passed, 19 ignored**;
  only the two independently failing fresh-instance Wasm-retention tests are
  excluded. The shared-instance async-trap test is included and passes.
  `suite-final.log` records the exact command and 42.66-second result. This is
  not an unqualified whole-suite pass.
- `git diff --check` and focused new-file rustfmt check passed.

Failures are not hidden by the narrower passing runs:

1. A two-thread whole-suite attempt hit SIGSEGV while initializing a headless
   GPU test. Its core shows a null-call PC, with return address in the system
   Vulkan loader's `loader_scanned_icd_add` (`libvulkan.so.1.4.362`, offset
   `0x45bd0`), not in window presentation or the new queue. The retained core is
   truncated, so full attribution is limited. Serial full-suite and rendering
   reruns completed without the crash. This fix does not claim to repair that
   concurrent GPU-initialization failure.
2. A pipe-only serial run passed 1,137 tests but failed a terminal framebuffer
   test at `Terminal::resize` with `WouldBlock`. It passed in isolation and in the
   full serial PTY run. The pipe-only run also failed
   `fresh_sync_wasm_instances_do_not_accumulate`, which independently reproduces
   with live-object count 5,781 to 7,829. That test constructs the JS engine
   directly and does not exercise any changed controller/presenter code.
   Individual rechecks confirmed the same retention for fresh asynchronous
   instances. The shared-instance asynchronous-trap test **passes** at
   5,525 to 5,525; it is not a remaining failure. The fresh synchronous and
   asynchronous instance cases remain separate follow-up work. Their exact
   logs are saved as `wasm-sync-fresh.log`, `wasm-async-fresh.log`, and
   `wasm-async-shared.log`.

## Standards basis

The local web-standards skill constrained this fix to discard only superseded
presentation snapshots, never semantic tasks/events. It also prevents treating
an unfocused but visible window as hidden.

- WHATWG HTML snapshot `e5071a20c8569d8a3ec02ed27dd01b948773f850`, fetched
  2026-09-06: `/big/web-standards/repositories/whatwg/html/source`, task-source
  ordering at lines 122724–122861, event-loop processing at 122934–123160,
  [update the rendering](https://html.spec.whatwg.org/multipage/webappapis.html#update-the-rendering)
  and rendering opportunities at 123167–123475, document abort at 114443–114501.
- Installed `/usr/share/wayland/wayland.xml:1628–1665`, `wl_surface.frame`:
  compositor callbacks pace surface updates and can be withheld for fully
  obscured surfaces.
- Exact linked winit 0.30.13, `src/window.rs:600–643` and Wayland
  `event_loop/mod.rs:470–504`: pre-presentation notification and pending frame
  callback handling. Exact linked Tokio 1.53.1, `src/sync/notify.rs:111–194`:
  enabling waiters before checking queue capacity prevents lost wakeups.
- For the test harness only, [Labwc's action reference](https://labwc.github.io/labwc-actions.5.html#37)
  documents that `NextWindowImmediate` avoids the modal switcher for a key
  without modifiers. `NextWindow` was inappropriate for that automation; no
  browser implementation change was needed to correct the test.
