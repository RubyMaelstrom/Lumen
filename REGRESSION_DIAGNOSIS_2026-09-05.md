# September 5 regression investigation and safe-test checkpoint

Status: **native-stack runaway fixed and verified in bounded release-headless observations;
not approved for promotion.** Separate Wasm-instance retention and visual/site gates remain open.

Source checkpoints at the start of this continuation: Lumen `9114121`, TRust `03fb97c`,
both with substantial uncommitted work. Do not attribute all working-tree changes to a single
optimization. No installed production executable has been replaced in this continuation.

## Host safety evidence

The previous boot's kernel journal records a global OOM on September 4 at 23:27:49:

```
task=trust-desktop,pid=2637789
total-vm:110466204kB, anon-rss:60476788kB, file-rss:87084kB
```

That is about 57.7 GiB of anonymous resident memory. It confirms host memory exhaustion, but
does not establish why the computer later powered off or which exact desktop artifact was running.
Recover the evidence with:

```sh
journalctl -b -1 -k --no-pager | rg 'Killed process|global_oom'
```

Polling RSS is insufficient: an earlier reproducer jumped from roughly 339 MiB to 3.6 GiB between
two-second samples. New `scripts/run-bounded.sh` uses a transient user-systemd service and checks
`memory.max`, `memory.swap.max=0`, and `memory.oom.group=1` **inside the service before executing**
the target. OOMPolicy=kill and the service deadline cover descendants; core dumps are disabled.
There is no unbounded fallback when systemd/cgroup support or configuration is unavailable.

`bash scripts/run-bounded.test.sh` passed normal success, nonzero exit propagation, timeout,
a deliberately contained 64 MiB cgroup OOM, and rejection of invalid limits/relative commands.
Browser diagnostics use 1 GiB; serial integration tests use 1–3 GiB. Compilation uses a separate
8 GiB limit and two Cargo jobs. These are test containment limits, not Web platform semantics.

## Findings from new local tests

The partially implemented WASM finalization experiment was still unverified. Three additional
tests failed before corrections (3 failed / 3 passed in the WASM conformance group):

1. A function retained only through a WASM table lost its custom JS property and parameter count
   after GC: `original wrapper:1` became `undefined:0`.
2. A module whose start function trapped after populating an imported table lost the JS imports
   needed by its escaped function: `WebAssembly import token 1 is missing`.
3. Writes before a trap were invisible in the existing JS buffer; a nested trap had the same
   problem, and `memory.grow` before a trap did not detach the old buffer until a later access.

Corrections:

- Withdraw the partial weak-wrapper/FinalizationRegistry/handle-release/store-reset experiment,
  including its host hook. Restore conservative strong identity caches and stable native handles.
  This prevents premature collection; it does **not** solve existing page-lifetime retention.
- Publish WASM memory changes before propagating an export-call failure, in both ordinary and
  re-entrant calls. Test writes, subsequent calls, imported exceptions, growth, and detachment
  across interpreter, bytecode, and JIT.
- Diagnostic logging must not invoke a thrown object's `toString` or `stack` getter. The test
  verifies zero such calls with tracing both off and on. Make trace formatting lazy and avoid
  incrementing call counters when tracing is disabled.
- Remove temporary `LUMEN_DISABLE_FINALIZATION`, `LUMEN_HIGH_INDEX_TRACE`, and
  `TRUST_BYPASS_FETCH_POLICY` investigation switches. Do not ship altered weak-target cleanup
  or bypass browser Fetch policy as a supposed leak fix.

These changes follow [WebAssembly JS API](https://webassembly.github.io/spec/js-api/) §§4.1–4.2,
5.6 and 7 (store/data-block identity, exported-function caches, state publication before a trap,
exception propagation), and [ECMA-262 liveness](https://tc39.es/ecma262/multipage/executable-code-and-execution-contexts.html#sec-liveness).
The unfinished finalization experiment was not in the earlier release artifacts and cannot
explain the user's original image-regression screenshots.

## Retention: distinguish instances from completed async jobs

The previous `completed_async_wasm_traps_do_not_accumulate` diagnostic created a fresh instance
on every call. Its name/comment incorrectly localized the failure to settled async work.
Keep that fresh-instance gate, add a synchronous control, and test reused-instance async work
separately. All counts below follow matched microtask/GC/finalizer boundaries, after warm-up
and 2,048 further operations in eight batches:

| Operation | Baseline live JS objects | Final | Result |
| --- | ---: | ---: | --- |
| Reuse one instance; async trap/catch | 5,538 | 5,538 | Pass |
| Fresh instance; async trap/catch | 6,306 | 12,450 | **Fail** |
| Fresh instance; synchronous trap/catch | 6,306 | 12,450 | **Fail** |

The fresh-instance tests retain exactly three JS objects per instance in this fixture. Strong
export-wrapper caches and import registrations remain page-lifetime roots; Wasmi's store arena
also retains native entities. These two tests remain enabled and failing, not skipped or
relaxed. The control rules out cumulative *live JS object* growth in this specific reused-instance
async fixture; it does not rule out every async/native allocation leak, nor explain the live
Neocities 58 GiB incident. A correct future cleanup implementation must trace cross-instance,
table/global/function/import and externref reachability, not just attach an Instance owner to
an export wrapper.

## Earlier verification record (before the native-stack fix)

- Full Lumen unit suite: **851 passed**, plus binary/doc targets; 38.54 seconds including build,
  3.7 GiB cgroup peak, no swap.
- After the corrections, WASM conformance group: **6 passed**, 5.40 seconds, 203 MiB peak.
- Memory/exception test repeated with `TRUST_WASM_TRACE=1`: **passed** in all three tiers,
  1.89 seconds, 102.8 MiB peak.
- Wider TRust Lumen-backend suite: **89 passed, 2 failed, 0 ignored** in 92.14 seconds under a
  3 GiB cap, 2.1 GiB peak, no swap. Only the fresh-instance retention tests fail. The initial
  combined run hit its 2 GiB cgroup cap near the last (worker) test; that worker test passes alone
  at 106.3 MiB. Aggregate memory remains a separate measurement concern, not evidence that
  worker execution alone accounts for the runaway.
- `cargo fmt --check` and `git diff --check` pass in both repositories.
- Release **headless only** rebuilt in 4m21s under an 8 GiB build cap; peak 4.5 GiB, no swap.
  Earlier artifact at `/big/Code/TRust/target/release/trust-headless` (replaced by the build below), SHA-256
  `1032c2f7cb656f9479a60e9f950ef213ea4904687a2497d752ae1082f2f4ba61`.
  `trust` and `trust-desktop` release targets were not rebuilt in this continuation; they do
  not contain this continuation's corrections. Nothing has been installed/promoted.
- Bounded release-headless Neocities diagnostic: **FAIL, delayed WASM phase exercised**.
  Ran with `--timeout 75 --settle 0 --js-diagnostics`, a 90-second outer deadline,
  `TRUST_WASM_TRACE=1`, and proxy environment variables removed. The trace reached over
  11,000 WASM export calls, then logged:

  ```
  wasm: import error token=2 index=22 params=[I32(1042600), I32(1050)]
  Finished with result: oom-kill
  Service runtime: 44.059s
  Memory peak: 1G (swap: 0B)
  ```

  The kernel terminated the entire constrained service with SIGKILL. This confirms that the
  runaway persists after the trap-memory corrections and withdrawal of the finalization
  experiment. It is not a successful site gate. The numeric import index differs from older
  captures (87); do not hard-code that index into diagnostics or infer an array index from the
  first numeric argument. The exact provenance and two raw i32 arguments are evidence;
  their meaning needs confirmation from the matching module/glue. The host remained protected.
  The trace contains only two instantiated modules: an empty 8-byte probe and a 639,875-byte
  module with 168 imports and 20 exports. Its registered linear memory grew from 18 to 19 pages
  (1,245,184 bytes), with no later growth recorded before termination. Repeated fresh instances
  therefore do not explain this particular captured burst, and linear-memory growth is not
  supported as its cause by the trace. Profile allocations/queued work at the callback failure.
  Raw local trace: `/tmp/neocities-cgroup-20260905.log`, SHA-256
  `154e4c77c018bfd53e20be3cbe16ee88859aa1fb7875f5ff2ba7b6e0dfdfa0cd`.

## Why the earlier process missed the reported regressions

The earlier engine benchmark matrix and JS unit tests did not validate decoded images or
desktop hover transitions. Semantic landmarks can exist while their imagery is missing. Likewise,
an initial-render or quiet-period gate can exit before delayed iframe/WASM work. Earlier bounded
Neocities samples that fetched the iframe/logo without reaching its WASM phase are not evidence
that the reported failure was fixed. Production also exhibiting the leak does not make it acceptable.

Before further optimization/promotion:

- Preserve and close the enabled fresh-instance retention gates with sound lifetime handling.
- Reproduce/profile the actual delayed Neocities allocation burst within the hard memory cap;
  distinguish JS objects, backing buffers, native store allocations, and queued work.
- Compare Archive banner/SVGs and hover, and Steam carousel images, using matching release
  desktop builds and viewports. Assert decode/render outcomes, not text alone.
- Record the precise binary/source provenance, intended phase, observed phase, time/memory
  limits, peak memory, errors, and outcome for every gate. Use **not exercised** when a required
  phase was absent, **failed** for a timeout/OOM, and never infer a visual pass from a text dump.
- Only after correctness gates pass, rerun performance comparisons and the full benchmark matrix.
  Build acceptance artifacts without installing; promotion still requires explicit user approval.

## Follow-up: allocation owner identified and native stack growth bounded

The runaway is **unbounded native execution-stack growth**, not an ever-growing Wasm linear
memory. External `eu-stack` sampling of the page actor during rising RSS shows the same JIT
call site recursively returning through `jit_call_inner → Interp::call → call_dispatch →
call_user → run_compiled_chunk`, repeatedly crossing `with_execution_stack`. Native segments
are anonymous `mmap` storage, outside Rust allocator/object/GC accounting. The relevant
source change is `c2069f6` (August 29, `fix(runtime): remove native execution-depth budget`),
which introduced unrestricted 8 MiB `stacker` segments. This establishes source provenance,
not which precise build the user had installed or why the site first exposed it recently.

The release allocation probe also reproduces the 1 GiB cgroup OOM after 42.237 seconds,
with only about 25 MiB net Rust allocation growth recorded at its last sample. The external
debugger capture is `/tmp/neocities-external-stack.log`; the release allocation log is
`/tmp/neocities-release-alloc.log`. Earlier in-process **debug** backtrace captures loaded
large DWARF symbol tables and polluted their own memory measurements; they are not valid
allocation-size attribution. All temporary allocator instrumentation has been removed.

The engine correction limits *simultaneously live additional native segments*, not the
number of JS calls made over an Agent's lifetime. The current host resource policy allows
64 × 8 MiB = 512 MiB beyond the embedder-owned thread stack. That accommodates the existing
4,096-context stress tests even with large unoptimized VM frames (381.8 MiB test peak).
Call/construct/native/intrinsic/JIT entry paths detect exhausted storage before moving owned
arguments and propagate a catchable stack-overflow RangeError through existing cleanup paths.
A thread-local segment count cannot be reset by re-entrant Agents/Realms, and an RAII guard
returns the budget on both normal completion and Rust unwinding. Stack headroom checks remain
amortized. This is native-resource containment; ECMAScript §9.4 does not prescribe these bytes
or an exact call-depth limit.

The new constant-stack tail-call check also exposed ordinary compiled tail calls missing
the existing tagged-tail-call fallback. Extend that fallback to ordinary/optional calls in
tail expressions; strict ordinary functions use the interpreter's trampoline until bytecode
implements equivalent transfer. ECMA-262 §15.10.3 requires resource reuse, so treating those
calls as ordinary growing frames would be incorrect. This fallback may affect performance of
strict tail-calling functions and needs measured native-tail-call work later, not a hidden
exception to the resource limit.

Local verification so far:

- Before the fix, the new offline recursive-call test is killed by its **512 MiB** cgroup in
  **432 ms**, no swap. The actual site likewise fails under its 1 GiB cap.
- After the fix, all four stack-resource tests pass: ordinary/captured/native call/apply,
  accessor, proxy and constructor recursion in interpreter/bytecode/JIT; repeated exhaustion,
  `finally` cleanup, subsequent calls, 20,000 proper tail calls, native-segment cleanup on Rust
  unwinding, and amortized headroom checks. Peak **758.3 MiB** under a 1 GiB cap, 9.82 seconds.
- Existing deep-execution-context tests: **4 passed**, unchanged assertions, 381.8 MiB peak.
- Full Lumen suite: **853 passed, 0 failed**, two test threads, 71.14 seconds, 1.8 GiB cgroup
  peak, no swap.
- Browser integration `webassembly_trap_then_recursive_microtask_recovers`: **passed** in all
  three tiers, including Wasm memory preservation, stack-error catch, later DOM update and
  following microtask. 3.16 seconds, 828.9 MiB peak under a 1 GiB cap. The initial fixture used
  an empty document and failed its DOM step; it now explicitly parses an HTML/body document
  and reports unexpected promise rejections. No production behavior was changed for that.
- Wider Lumen-backend suite: **90 passed, 2 failed**, zero ignored, 96.41 seconds, 2.8 GiB peak
  under a 3 GiB cap. Only the pre-existing fresh synchronous/asynchronous Wasm-instance
  retention gates fail, with unchanged 6,306 → 12,450 live-object counts.
- Both repositories pass `cargo fmt --all --check` and `git diff --check`.

### Release observation results

Both runs use the actual release headless binary, a verified 1 GiB process-tree cap, no swap,
1024×768 CSS viewport, `--settle 0`, `--js-diagnostics`, and proxy variables removed. The
120/180-second **observation windows** deliberately extend past the prior delayed OOM. They
are not a relaxation of the full-site completion criterion: both headless runs exit **3**,
reporting that the page did not become final. Neither reaches its outer 150/210-second cgroup
deadline or gets OOM-killed.

| Observation | Kernel memory peak | Subsequent sampled RSS | CPU | Result |
| --- | ---: | ---: | ---: | --- |
| 120 seconds, Wasm tracing on | 902.8 MiB | about 406–425 MiB after the burst | 17.214s | Survives; page text rendered; non-final |
| 180 seconds, tracing off | 929.6 MiB | about 412–434 MiB; final 30 seconds flat in samples | 17.308s | Survives; page text rendered; non-final |

The first trace explicitly reaches over 11,000 Wasm calls and the matching delayed import
failure (`token=2 index=62 params=[I32(1042600), I32(1050)]`), then continues for the rest of
the window. Import numbering varies; no index/site exception was added. The second run has
no Wasm/allocation tracing and shows the same transient memory burst followed by recovery.
Both final diagnostic snapshots report `panicked=false`, zero module skips, five fetches,
and the Neocities HTTP-200 page text. Final snapshots are not a complete history of script
errors and cannot certify that the user's earlier visible Wasm error is gone. The internal
Wasm import failure is still present in the traced run. Longer-term retention and full
visual/interaction acceptance remain separate gates.

Logs and RSS samples:

- `/tmp/neocities-stack-fixed-release-120s.log`
- `/tmp/neocities-stack-fixed-release-rss.tsv`
- `/tmp/neocities-stack-fixed-untraced-180s.log`
- `/tmp/neocities-stack-fixed-untraced-rss.tsv`
- `/tmp/trust-stack-fix-backend-suite-final.log`
- `/tmp/lumen-stack-fix-suite.log`

### Acceptance artifacts (not installed or promoted)

`cargo build --release --offline --locked --bin trust --bin trust-desktop --bin trust-headless
-j 2` succeeded in 11m00s under the 8 GiB build cap (4.6 GiB peak, no swap). These builds
contain the stack-storage fix and strict-tail-call fallback; no allocator probe is compiled in.
The new browser test is test-only. Real-site observations above exercise headless, not the
desktop visual renderer. Installed production executables were not modified.
`trust-desktop --help` also passes under a 1 GiB cap. The terminal `trust --help` attempt
enters terminal initialization and fails without a controlling TTY in the pipe-based runner;
it is not a successful terminal-frontend smoke test and did not exercise page JavaScript.

| `/big/Code/TRust/target/release/` artifact | SHA-256 |
| --- | --- |
| `trust` | `e40e16896deb9a4a062123dbaed5dd5a17f8f559c49e9df62c6ead658858d7ba` |
| `trust-desktop` | `96e6d60c6eba6ad45715da0c830d43b29a1eaad1501d21c3d4b39a822efe393a` |
| `trust-headless` | `ed022aecfa78a704491e20a8600890198dd184310ebfdd5eb0f88864c6a3057c` |

This closes the rapid native-stack runaway reproducer, **not** the optimization milestone or
the full browser-regression checklist. No benchmark-speedup claim follows from this fix.

## Further WASM boundary corrections

The next investigation found four independently reproducible defects. All four new tests failed
on the pre-fix code, before implementation; each now passes in interpreter, bytecode, and JIT:

- Imported JS callbacks were dispatched through `callback.apply`. An own getter was invoked
  and its exception replaced the intended call. Capture intrinsic `Reflect.apply` for the
  normative `Call(func, undefined, args)` operation, without reading callback properties.
- Multi-value imports used author-visible `Array.from`, incorrectly accepting non-iterable
  array-like values. Implement GetMethod(@@iterator), GetIteratorFromMethod, and IteratorToList
  in the shared WASM prelude; capture `next` once, consume the iterator before checking arity
  or coercing results, and propagate iterator exceptions without extra IteratorClose work.
- Every conversion of an existing `externref` allocated another host-cache entry and another
  native Wasmi ExternRef. The red test allocated **8,960 extra native entries** for the same
  ten values in 128 batches. Intern the JS host value and native address, keeping the native
  cache in Store data so an active Caller uses it during nested imports/export calls. Preserve
  object/symbol identity, undefined, BigInt, NaN, and the distinction between +0 and -0.
  The post-warmup native-allocation delta is now **zero** in all three tiers, including nested
  callbacks, Table.set/grow(0), Global writes, and exported-function arguments/results.
- Exported functions were ordinary constructible JS functions with a prototype object.
  Use a nonconstructible rest-argument closure and a configurable length descriptor; calling
  with `new` now throws TypeError. This also avoids author `Array.prototype.slice/call` when
  forwarding export arguments and removes the unnecessary retained prototype object.

Normative sources: [WebAssembly JS API §5.6](https://webassembly.github.io/spec/js-api/#exported-functions)
(Call, multi-value results, host-value address reuse, and nonconstructible Exported Functions),
and [ECMA-262 IteratorToList](https://tc39.es/ecma262/multipage/abstract-operations.html#sec-iteratortolist).
The shared prelude changes also reach worker scopes; the native externref cache change is in
the default Lumen adapter, not the optional legacy Boa adapter.

Also stop registering an empty import-function array when an instance has no JS callbacks.
No callback can observe that token in this case. Nonempty registrations remain rooted, including
when a failed start has escaped through an imported table. This is safe allocation avoidance,
not an attempted substitute for cross-heap collection.

### Retention remains an acceptance blocker

After matched warm-up/GC and 2,048 operations, the reused-instance async control remains flat
(5,542 live JS objects). Both fresh-instance cases improve from **three retained JS objects per
instance to one**, but still fail unchanged assertions (5,798 → 7,846). Native Store arenas and
distinct externref host values remain retained for the page lifetime. Deduplicating existing
values does not collect unreachable distinct values or instances, and no byte/RSS reduction
can be inferred from these JS object counts alone.

The remaining implementation needs joint JS/WASM reachability, including table/global/ref.func,
import closures, element segments, cross-instance references, and failed-start escapes. Wasmi's
Arena explicitly does not support individual deallocation. Clearing wrapper maps, resetting a
still-live Store, or releasing an Instance just because its JS wrapper died is unsound. Preserve
the two failing gates and add native-allocation bounds when introducing actual reclamation.

### Verification

- New-test red run: **0 passed / 4 failed**, 4.40s, 128.1 MiB peak under a 2 GiB cap.
- Focused WASM group: **11 passed / 0 failed**, 18.23s, 1.1 GiB peak under 2 GiB.
- Full Lumen suite with embed feature: **876 passed / 0 failed**, 72.77s test time,
  1.8 GiB peak under 4 GiB; binary/doc targets also pass.
- Broader default browser-backend suite: **94 passed / 2 failed**, 104.65s,
  2.9 GiB peak under 3 GiB; only the two known fresh-instance retention gates fail.
- Trap-memory and multi-value tests with `TRUST_WASM_TRACE=1`: **2 passed**, 3.77s,
  168.5 MiB peak under 1 GiB. This includes all three execution tiers.
- Formatting and diff whitespace checks pass in both repositories.

Add `Ctx::error_diagnostic` for bounded inspection of an actual Error's own string data message
and engine-captured stack. It never performs [[Get]], follows prototypes, or coerces objects;
proxies and non-Errors return None. Its unit test verifies no author getters/toString/proxy traps
execute, including accessor/non-string messages, and bounds long Unicode diagnostics. WASM import
tracing uses it lazily to identify the delayed failure without perturbing exception semantics.

Local logs: `/tmp/trust-wasm-followup-red.log`, `/tmp/trust-wasm-followup-green.log`,
`/tmp/trust-wasm-followup-backend-suite.log`, `/tmp/trust-wasm-followup-tracing.log`,
`/tmp/lumen-error-diagnostic-test.log`, and `/tmp/lumen-wasm-followup-suite.log`.

### Delayed Neocities failure localized to missing nested-document API

The first follow-up release-headless build (SHA-256
`adc2ea9734eb93e1b2ae9c27db4ea09f18ec3722760a96702c1bc61d5dbff3ad`) survives a
120-second observation under the 1 GiB cap (908.8 MiB peak, 17.154s CPU, final sampled
RSS 419,192 KiB). The application exits 3 at its observation deadline: non-final, not a
functional pass. Crucially, enabling both WASM and Lumen diagnostic history reveals:

```
import error token=2 index=62 params=[I32(1042600), I32(1050)]
Error: cannot read property of null or undefined
    at o
    at ia
    at f
    at vV
    at aeN
microtask: cannot read property of null or undefined
microtask: wasm `unreachable` instruction executed
```

A second capped 120-second run (921.2 MiB peak, 17.249s CPU) confirms the same pair of
errors. Existing nullish-property diagnostics identify a `.length` read in the string
marshalling helper. Fetch diagnostics identify the exact public hsw asset:
`https://newassets.hcaptcha.com/c/08e1f1fd2699e6185a3d6281dd519b788e1d186944f48f248faf1d600bbc78f0/hsw.js`.
Offline decoding of its constant string table (without executing the script) maps import
`a.ia` to a **referrer getter**: it reads a host object's `referrer` and passes that value
to a wasm-bindgen-style string encoder. The two i32 parameters are an output pointer and
a glue object-handle index, not an allocation size or a JS array length.

TRust's top-level Document implements `referrer`, but its separate FrameDocument facade
does not. Consequently nested document.referrer is undefined and its encoder fails on
`.length`. This is a browser Document-interface omission exposed through a WASM import,
not a reason to swallow the TypeError or return a fabricated WASM result. Referrer metadata
initialization for navigations is a separate existing gap (the top-level getter currently
returns the specification's uninitialized empty-string default); do not claim that adding
the missing interface member implements navigation/referrer-policy semantics.

Logs: `/tmp/neocities-wasm-followup-release-120s.log`,
`/tmp/neocities-wasm-followup-release-rss.tsv`, `/tmp/neocities-wasm-followup-nullish.log`.
The decoded source used `/tmp/neocities-hsw-08e1f1fd.js`; no source/domain fingerprint or
page-specific behavior was added to the browser.

Add the missing readonly `FrameDocument.referrer` getter with the same uninitialized string
default as the existing top-level getter. The local iframe/WASM import test fails before this
change and passes after it in all three tiers, including readonly assignment. Navigation
referrer initialization remains explicitly unimplemented; no parent URL is fabricated or
exposed without policy processing. After this correction, the focused WASM group passes
**12/12** (21.58s, 1.2 GiB peak under 2 GiB), and the browser-backend suite is **95 passed /
2 failed** (106.85s, 2.9 GiB peak under 3 GiB). Only the unchanged fresh-instance retention
gates fail. Logs: `/tmp/trust-wasm-referrer-red.log`, `/tmp/trust-wasm-referrer-green.log`,
`/tmp/trust-wasm-referrer-backend-suite.log`.

The rebuilt release headless (SHA-256
`97fdd889cf4f9251a2b5bc5edf153051128dda8789201d1876635b06b0f9291c`) gets past the
referrer import, reaching roughly **44,000 WASM export calls**, but then **segfaults** after
43.789s (383.3 MiB peak, 16.462s CPU). This is a **failed real-site gate**, not acceptance of
the referrer correction or the release. The later crash is under investigation; no conclusion
about its provenance follows merely from reaching it after the getter fix. The binary was not
installed. Log: `/tmp/neocities-wasm-referrer-fixed-release-120s.log`.

An unoptimized debug observation stops at its 120-second deadline before WASM is exercised
(four fetches, 396 MiB peak, about 120s CPU). It cannot rule out the optimized crash. A temporary
external fault-capture preload lives only in `/tmp/lumen-wasm-segv-bIlfgj/`; it stops on SIGSEGV
or SIGBUS for external unwinding and is never linked into release artifacts. Its test process
still has a verified 1.5 GiB cgroup cap and outer deadline; no unbounded core dump is enabled.

### Follow-on crash: ARM64 exception targets were not relocated with branches

Optimized fault capture reproduces an invalid read at address `0x3d` in anonymous JIT code,
not inside Wasmi. The faulting JS function has about 1.03 MiB of generated instructions.
Its runtime catch table still contains pre-relaxation offsets: `Asm::finish` inserts words
to extend out-of-range conditional branches, adjusts its internal labels, but the separately
recorded `pc_insn` table does not move. `jit_unwind` branches through that stale table and
can enter the middle of an unrelated template. The old diagnostic PC map is stale for the
same reason, so its apparent `Return` attribution is not a reliable instruction attribution.

Resolve exported bytecode-PC offsets from the assembler's **final label layout** instead.
Both exception dispatch and diagnostic PC maps now use those offsets. The x86-64 emitter
uses fixed-size rel32 patches without inserting bytes, so it does not share this relocation
defect. This repairs general try/catch behavior (ECMA-262 §14.15.3); there is no site, source
fingerprint, WASM module, or disabled-JIT exception in the implementation.

The standalone 4,000-statement JS reproducer **segfaults before the fix** under a cgroup
cap and passes afterward in all three tiers. Reduce the permanent fixture to 1,000 statements:
it still emits `0x1287f0` bytes on this ARM64 build and passes in 0.93s, avoiding a 100-second
unit test. A separate assembler regression verifies exported labels and aliases after both
forward and backward long-branch insertions. The full Lumen/embed suite now passes **878/878**
(22.21s test time, 2.7 GiB peak under a 4 GiB cap; binary/doc targets also pass).

Logs: `/tmp/neocities-wasm-referrer-jit-map.log`,
`/tmp/neocities-wasm-referrer-symbols-stack-2.log`, `/tmp/lumen-large-catch-red.log`,
`/tmp/lumen-large-catch-green.log`, `/tmp/lumen-large-catch-small-green.log`,
`/tmp/lumen-branch-offsets-green.log`, and `/tmp/lumen-wasm-catch-full-suite.log`.
The new browser-boundary regression passes in all three tiers (2.85s). It checks both early
and late catches around a large generated body, preserves an imported exception's object
identity, recognizes native traps as RuntimeErrors, repeats both paths, and delivers a later
microtask. The complete focused WASM group passes **13/13** (24.90s, 1.4 GiB peak under 2 GiB).
The broader backend suite before that final test addition passes **95** with the same **2**
fresh-instance retention failures (108.69s test time); those assertions remain enabled.
Logs: `/tmp/trust-wasm-large-catch-green.log`, `/tmp/trust-wasm-catch-group.log`, and
`/tmp/trust-wasm-catch-backend-suite.log`.

The first branch-fixed release-headless binary (SHA-256
`d9f578774955615e3ee0e0ae217b827f2c81df1665d1d2a2b332d6a3eec6b46e`) reaches over
**62,000 WASM calls**, beyond the former ~44,000-call crash, and survives the full **120s**
observation without a segfault, traced import error, or microtask `unreachable`. It uses
17.398s CPU, peaks at **932.6 MiB** under a verified 1 GiB cgroup cap with no swap, and its
post-burst sampled RSS settles around 410 MiB. The application exits 3 (non-final) at the
observation deadline, **not** a completed load or desktop visual acceptance. Log:
`/tmp/neocities-wasm-catch-fixed-release-120s.log`; post-burst RSS samples:
`/tmp/neocities-wasm-catch-fixed-release-rss.log`.
The live WASM payload is 643,226 bytes (168 imports/20 exports), versus 646,682 bytes in the
fault capture. Live assets vary, so call counts are phase evidence, not a matched-payload
benchmark or proof of identical site code; the standalone crashing regression is deterministic.

All three release targets are rebuilt from the cleaned final source (the first headless build
still contained an unused assembler helper). The build takes **10m56s**, peaks at **4.7 GiB**
under an 8 GiB cap, and uses two build jobs. Nothing is installed or promoted.

| `/big/Code/TRust/target/release/` artifact | SHA-256 |
| --- | --- |
| `trust` | `76f86471926f150bb0bc9606900c6f3d858c48227e0bcb17f14bd804171dd196` |
| `trust-desktop` | `58ff354bba11e11b4fb37e3b7a4fefc3384e75505f434f963ebd082a8d943c31` |
| `trust-headless` | `50920f6a789acb90937ae18c6259a6c6773f8e3b87d7fc16605f9ef016e167ea` |

The final headless artifact completes a **180s observation**, with a 1 GiB memory cap, no swap,
and a 210s outer deadline. Import/microtask/fetch history and JIT range diagnostics are enabled
(no preload, source-word dump, debugger, or core dump). It reaches over **62,000 WASM calls**
without a segfault, traced import error, or microtask `unreachable`; CPU time is **18.023s** and
cgroup peak is **946.2 MiB**. It exits **3/non-final** at the application deadline, not a
completed-load or visual pass. The native JS chunk reaches **1,077,004 bytes**, confirming
that this observation still crosses ARM64's conditional-branch range.

This run fetches a newer public asset:
`https://newassets.hcaptcha.com/c/c768b39702b45053ff903b439b716f5e5abc676ffe0ca4dc83955a3fe5aa7c48/hsw.js`
(1,041,818 JS bytes; 643,226 WASM bytes, 168 imports/20 exports). The late 99 RSS samples
rise from **443,348 to 461,600 KiB** (about 433–451 MiB), so the memory trajectory is **not
flat** and this finite observation does not prove leak freedom. The rapid 58 GiB runaway does
not recur in this window. Fresh-instance/native-arena reclamation remains an acceptance blocker.
Logs: `/tmp/neocities-wasm-followup-final-release-180s.log` and
`/tmp/neocities-wasm-followup-final-release-rss.log`. No live reproducer or build remains running.

The final 98-test backend run reaches the recursive-microtask test but hits its **3 GiB**
cgroup ceiling; only that test service is OOM-killed. This is an incomplete/failed run, not a
suite pass. Repeat the combined suite under **4 GiB**; live-site observations stay at 1 GiB.
Log: `/tmp/trust-wasm-followup-final-backend-suite.log`.
The 4 GiB rerun completes: **96 passed / 2 failed**, 111.02s test time, **3.3 GiB peak**.
Only the unchanged fresh synchronous/asynchronous instance-retention assertions fail.
Log: `/tmp/trust-wasm-followup-final-backend-4g.log`.

`cargo clippy -p lumen --features embed --all-targets -- -D warnings` also fails: eight lints
in the existing executable-buffer/RegExp changes (needless returns, question-mark simplification,
missing transmute annotations, identical conditional bodies). The lint gate is not clean;
do not conflate successful tests/builds with a Clippy pass. Log:
`/tmp/lumen-wasm-followup-clippy.log`. Formatting and diff whitespace checks pass in both repos.

## YouTube SVG regression: browser referrer misclassified as an author header

Later September 5 investigation, after the legacy backend removal:

- Installed desktop production renders the icons; the pre-fix single-engine release renders
  black navigation blocks and omits the consent-dialog icons/logo. Matched live captures show
  production inserting 21 SVG elements, while the new release inserts CSS-mask fallback DIVs
  with no SVG elements. YouTube's icon loader falls back when fetching/parsing the SVG fails;
  TRust's missing CSS-mask support then exposes the solid background. This is not a failure
  of the SVG rasterizer to decode the same markup.
- A two-origin loopback page reproduces the failure with `fetch(url,
  {credentials:'same-origin'}).then(r => r.clone().text())`. Production sends GET; the release
  sends OPTIONS with `Access-Control-Request-Headers: referer`, then rejects the request.
  The same probe fails for the real fonts.gstatic.com menu SVG, whose GET response provides
  `Access-Control-Allow-Origin: *`.
- Cause: the new CORS preflight classifier in TRust `src/http.rs` omitted the browser-owned
  Referer from its generated-header exclusions. The page actor attaches that header before
  calling the policy path, so even ordinary cross-origin GETs were incorrectly preflighted.
  This predates the backend removal and is unrelated to the Intl preference or memory caps.
- Fix: exclude browser Referer from the author-header preflight decision, preserving it on
  the wire. [Fetch §4.6](https://fetch.spec.whatwg.org/#http-network-or-cache-fetch) appends
  Referer at the network stage, after the §4.4/§4.8 preflight decision. CORS permissions,
  author custom-header preflights, and opaque no-cors filtering remain in place.
- Added two permanent tests in TRust `http::tests`: generated/mixed-case referrer versus
  author custom headers, and real cross-origin HTTP SVG fetch/decode with positive CORS,
  opaque no-cors, and missing-CORS-permission negative cases. Both failed before the fix
  and pass afterward. Earlier policy-only fixtures omitted the browser-attached referrer;
  that integration gap explains why they missed this regression.

The initial 1 GiB diagnostic wrapper killed YouTube at 48.252s, before full load. It was
inconclusive, not a rendering failure or acceptance result. At the user's explicit request,
subsequent YouTube observations have no RAM cap, sample RSS, and allow 210s. The unfixed
release reproduces the icons failure at about 67s/1.15 GiB RSS and remains around 1.18 GiB
at 190s. Those finite samples do not establish leak freedom. Normally launched user browsers
were outside the diagnostic cgroups. Do not apply the earlier 1 GiB testing recommendation
to YouTube or interpret a cap-induced exit as a page failure.

Evidence directory: `/tmp/trust-youtube-svg.pE2OI3/` (`current-full/`, `production/`,
`fetch-probe.log`, `tests-red.log`, `tests-green.log`). Final verification:

- HTTP module: 100 passed, 9 existing ignored; SVG filter: 27 passed (overlaps the HTTP
  module, so do not sum these as unique tests). Clippy all targets/all features with
  `-D warnings`, formatting and diff checks pass.
- All three release binaries rebuilt successfully in 10m14s. The exact original
  fetch/clone/text/DOM probe now passes for both the loopback SVG and the live CDN SVG,
  with ordinary GET requests and correct `cors` response types. English-preference,
  worker, iframe, task and tiny-WASM release smoke checks also pass; the pre-existing
  headless image-scheduler limitation is not claimed fixed.
- Fixed release desktop visually renders the consent logo and icons, confirmed by the
  user and a capture containing 21 SVGs / 45 paths / zero mask fallbacks. The user then
  clicked Accept All at approximately 127s. Extend the observation beyond its original
  210s timer for this second load: at 271s, the post-consent navigation still correctly
  renders its 11 SVGs / 27 paths / zero mask fallbacks. Captures include
  `fixed-early.png` and the window-only `fixed-post-consent-final.png`.
- RSS samples: 72s 1,210,248 KiB; 104s 1,191,224 KiB; after consent/new load,
  200s 1,738,868 KiB and 271s 1,751,956 KiB. No RAM cap applied. These observations
  validate the SVG fix, not long-term leak freedom or a performance benchmark.
- Separate follow-up: the user-triggered `consent.youtube.com/save` request logs
  `body framing is invalid for this response status`. Icons recover after navigation,
  but consent-save success/persistence is **not verified**. The existing HTTP parser's
  informational/204 framing rejection needs a separate wire/spec investigation; the
  failing response status/headers were not captured. Do not conflate this with SVG loading.

Rebuilt artifact SHA-256 values (not installed or promoted):

| Binary | SHA-256 |
| --- | --- |
| `trust` | `80cbf12209d7aab2d2edb064af2dadbcc067136cd1045952fb219133255e0cc7` |
| `trust-desktop` | `40b39925ced8a14436f2d89691afd8ad65dc8e5f06043a2e29b43635a127bab5` |
| `trust-headless` | `93b2000ec339b218ded6471260d41bc0121a079d66cccd54cbed2b4e74674e5c` |

Installed desktop remains `e7fdb23b91888aeaf1ebfecbadfcf25557ca6aab310b7a89ff11efb70fb9d778`.

## Consent-response follow-up (explicitly requested by the user)

Replaying the user-triggered consent URL with POST captures `HTTP/1.1 204 No Content`
and `Content-Length: 0`. A GET control returns 405 and is not the same request. Raw
headers are in the temporary evidence directory; do not copy consent tokens/cookies into
project notes. The new strict framing code wrongly rejected the POST response before
its status and Set-Cookie fields could be processed.

[RFC 9112 §6.3](https://www.rfc-editor.org/rfc/rfc9112.html#section-6.3) gives HEAD,
1xx, 204 and 304 responses first precedence: they end at the header boundary regardless
of framing headers. Sender prohibitions on those fields do not imply that a browser must
discard the completed bodyless response. RFC 9110 §2.4 allows recovery; connection reuse
must remain safe.

Correction in TRust `read_response_with_policy`:

- Resolve bodyless statuses before TE/CL framing validation and body decoding.
- Preserve successful status, headers and cookies without consuming purported body bytes.
- Retire connections carrying contradictory bodyless framing (including interim responses)
  and never pool a 101 protocol switch. Valid HEAD/304 representation metadata remains usable.
- Keep rejection of malformed/truncated framing on actual body-bearing responses unchanged.

Two additional tests fail before this fix and pass after it: bodyless status/header precedence
across 204/304/101 plus informational responses, and a real POST/204/Set-Cookie exchange
followed by a cookie-bearing GET on a fresh connection. The latter also verifies that the
client closes the contradictory keep-alive socket instead of hanging or reusing it.
An older test incorrectly requiring rejection of 204 plus Content-Length was replaced by
the precedence and connection-retirement tests, not simply removed without coverage.

HTTP module: 102 passed / 9 existing ignored; SVG filter: 27 passed; Clippy all targets/all
features with `-D warnings` passes. All three releases rebuilt successfully in 10m09s.

The rebuilt headless release passes a seven-case page-JS framing gate: empty 204, 204 with
purported body bytes, 304 representation metadata, rejection of TE+CL on a 200 response,
rejection of conflicting lengths and truncated bodies, and POST-save/cookie-follow-up.
The actual `fetch`/Body methods are exercised, not only Rust parser fixtures. The original
local/live-CDN SVG fetch probe and English/task/WASM/worker/iframe smoke also pass again.

Live desktop verification used the latest artifact with no RAM cap. A diagnostic-only probe
clicks the same Accept All choice authorized by the user after allowing 90s for page load,
and observes the original fetch promise without changing its arguments or result. Evidence:

```
CONSENT_GATE_CLICK Accept All
net: @98435ms +272ms 204 0B https://consent.youtube.com/save?[redacted]
CONSENT_GATE_RESPONSE status=204 type=cors ok=true
```

At 229s (over two minutes after the save), the consent dialog remains absent and the new
page's captured DOM has 11 SVGs / 27 paths / zero mask fallbacks. No image-load failures,
network errors, TypeError, RangeError or unreachable errors were found in this run's trace.
The logo/search icons were visually confirmed during the post-save load; the final screen
capture was obscured by the user changing workspace, so do not present it as a final browser
screenshot. RSS sampled 1,204,132 KiB at 89s and 1,740,088 KiB at 229s after navigation;
this is not a long-duration leak-freedom claim. Long-term consent persistence across browser
restarts is outside this check; real HTTP save success and same-session cookie handling pass.

Logs: `consent-fixed.log`, `consent-http-tests.log`, `consent-svg-tests.log`,
`consent-clippy.log`, `consent-release-build.log`, `framing-release-gate.log`,
`fetch-probe-consent-fixed.log`, `language-smoke-consent-fixed.log` in the evidence directory.

Latest release SHA-256 values, superseding the SVG-only checkpoint above:

| Binary | SHA-256 |
| --- | --- |
| `trust` | `30fa0290045f55cbac612628df04d4bd585c657b173646c8681d27d0aeaffe55` |
| `trust-desktop` | `c1a9d35b8d05b0c76537c45bd06c8ca7d36a9bdb9a86dc03ad4d8f980783b016` |
| `trust-headless` | `1991a6696faa7209d3a425ccc7627294bea3745ffe4bb1a0fcb1f997808549c7` |

No installed binary was replaced, and no commit or push was made.

## Archive SVG disappearance: Hybrid atlas command ordering

Evidence directory: `/tmp/trust-archive-regression.TYoece/`.

The user again reports the missing Wayback logo and top-bar book icon in the
latest release (`c1a9d35...` desktop), while installed production (`e7fdb23...`)
normally paints both. Matched live runs establish:

- Release, normal Hybrid, diagnostics unset: both missing (user-confirmed).
- Installed production, diagnostics unset: both visible (user-confirmed).
- Installed production, only raw HTML dump or Lumen trace: both visible.
- Installed production, desktop trace enabled: the same disappearance occurs.
  The first run with all diagnostics enabled therefore was not a clean visual
  reference; the user correctly caught this. Trace overhead exposes the defect
  in production as well. Do not conclude a trace flag semantically changes SVG.
- Release with `--renderer=cpu`: both visible in the screenshot. This is a
  diagnostic control, not an acceptable default-renderer workaround.
- Temporary frontend capture at the decoder boundary contains the complete
  Wayback paths/fills and book geometry with `fill:#999`. Both builds' live
  DOM dumps contain the SVGs too. No image fetch/decode error accounts for the
  disappearance. Temporary capture instrumentation has been removed.

The deterministic cause is GPU atlas operation ordering, not a removed SVG
implementation or page memory cap. Hybrid deallocates unused images and records
their texture clear in a command encoder. A later allocation can reuse that slot
in the same frame. Pixmap upload previously called `Queue::write_texture`, whose
work executes before the explicitly submitted encoder; the older encoded clear
then wipes the replacement upload. Similarly an encoded atlas-growth copy can
overwrite an update uploaded to the new texture. This explains sensitivity to
intermediate frames, image-key/style changes, and tracing overhead. No bisect of
which recent change altered frame timing has been performed; do not attribute
that trigger to the memory work without evidence.

The stale-image eviction loop dates to `8b8eb40b` (2026-08-10). The prior fix
`1c142ae8` (2026-08-12) already recognized the clear/upload problem, but handled
same-sized replacement under one stable image handle by updating in place. Its
pixel test did not cover changing handles or atlas growth. That coverage gap,
plus treating decode/DOM success as visual success, allowed this path to escape.

Correction is confined to the owned Hybrid fork's Pixmap atlas writer: stage
the bytes in a COPY_SRC buffer and encode copy_buffer_to_texture in the same
command stream as clears and atlas-growth copies. Rows are padded to 256 bytes
when required; already aligned rows avoid that extra CPU padding copy. The
encoder retains the upload buffer until submission; no persistent cache or page
memory limit is added. Texture writers already use encoder-ordered copies.
The existing same-handle in-place update remains. Sources read: WebGPU §§3.4.1,
13.2.1, 19.2 (writeTexture/submit/command execution), downloaded to `webgpu.html`,
and wgpu 30 Queue documentation/source. SVG 2 styling/rendering sections were
also read during initial classification.

New tests (all exercised on this machine's actual GPU, not skipped):

1. `hybrid_recycles_image_slots_without_erasing_replacement_pixels`: red before
   fix, second frame expected green but read back `[0,0,0,0]`; green after.
2. `archive_svg_pixels_survive_hover_and_resource_key_changes`: uses the actual
   two SVG geometries in `src/render/fixtures/archive-svg-repaint.html`, six
   hover/resource-key phases and two repaints per phase, checks both decoded
   ink and GPU/CPU pixel parity including each icon's own rectangle. Old writer
   fails on phase 1 with 22.6% of whole-frame channels over tolerance; fixed
   writer passes. The smaller book cannot hide in a whole-page tolerance.
3. `hybrid_atlas_growth_preserves_upload_order_and_row_padding`: uses a small
   atlas to force growth and an existing-image update in one frame, both aligned
   and non-aligned rows. Old writer reads stale red instead of new blue; fixed
   writer passes. Initial test setup accidentally fit two images in one atlas;
   its explicit growth assertion caught that and the fixture was corrected.

Initial validation: renderer module 45 pass / 1 existing ignored; image module
24 pass / 1 existing ignored; SVG filter 28 pass; HTTP module 102 pass / 9 existing
ignored (groups overlap). All-target/all-feature Clippy clean; format/diff checks
clean. GPU tests explicitly report unavailable adapters as not exercised; a
machine without an adapter cannot establish the visual gate. Gate commands and
real-site release acceptance requirements are recorded in TRust DIAGNOSTICS.md.

Final validation and acceptance:

- Five fresh processes running the GPU-related filter: 8 pass per run, no adapter
  skips. Final rebuilt test binary: all four dedicated pixel gates pass again,
  including the pre-existing stable-handle gate. Logs `repeated-gpu-tests.log`
  and `final-pixel-gates.log`.
- Root all-target/all-feature Clippy and separately selected `vello_hybrid` library
  Clippy both clean with `-D warnings`; explicit vendored-file rustfmt check clean.
  Logs `clippy.log`, `fork-clippy.log`.
- Full release build of trust, trust-desktop, trust-headless succeeded in 7m56s,
  `-j 3`, `release-build.log`. Only desktop's bytes change because the corrected
  Hybrid upload path is not linked into the terminal/headless outputs:

  | Binary | SHA-256 |
  | --- | --- |
  | `trust` | `30fa0290045f55cbac612628df04d4bd585c657b173646c8681d27d0aeaffe55` |
  | `trust-desktop` | `00304269537b2b70fd634120b8dd3cf3e65b7a1d4f3282095211a992fb443ae9` |
  | `trust-headless` | `1991a6696faa7209d3a425ccc7627294bea3745ffe4bb1a0fcb1f997808549c7` |

- Exact rebuilt release, normal default Hybrid, no diagnostic flags: live
  Archive logo and full top bar visible at 23s, `fixed-plain.png`, PID232777,
  RSS507144KiB at 23s and 58s. Observed run continued to its 150s deadline.
- Exact rebuilt release, all original trace/dump flags: live Archive logo and
  book/top-bar icons visible, `fixed-trace.png`, PID233709, continued to120s.
  No image error, network ERR, panic or unreachable in `fixed-trace.log`.
  These are real visible-window captures, not only DOM evidence. Pointer hover
  is covered deterministically by the offline pixel gate; do not claim a
  separately observed manual live hover interaction.
- Follow-up release YouTube run: actual logo/consent and navigation icons visible
  after over90s, `youtube-fixed.png`; matching DOM21SVG/45paths/zero CSS-mask
  fallback. No image/network errors or panic/unreachable in `youtube.log`.
  PID233928 observed until240s, uncapped; RSS1223240KiB at90s and1238312KiB
  at165s. No new claim about long-term leak freedom or consent-save interaction
  (the earlier consent-save gate remains the evidence for that behavior).
- Additional release Steam observation: hero and featured carousel main artwork
  plus thumbnails are visible in `steam-fixed.png` at about47s. PID234308,
  RSS805456KiB at47s, observed until90s. No image decode failure. **Not a whole-site
  pass:** API `IStoreQueryService/Query/v1?origin=undefined&...` repeatedly fails
  CORS in `steam.log`. That separate unresolved issue was reported to the user;
  do not silently weaken CORS or label all Steam JS functional.

All diagnostic GUI processes exited at their planned observation deadlines
(timeout wrapper124 is expected for these continuously resident pages, not a
claimed quiet-period completion). No memory caps were applied. Installed
desktop remains `e7fdb23b91888aeaf1ebfecbadfcf25557ca6aab310b7a89ff11efb70fb9d778`.
No binary installation, commit, push, JS-engine change or memory-limit change
was performed for this Archive fix. Other existing dirty changes were preserved.
