# Reproducible performance measurements

`engine-matrix.json` is the checked-in contract for Lumen's initial engine comparison. It pins the
fixture source revision and file hashes, engine versions, command-line arguments, allocator
description, environment policy, warmup/sample counts, CPU affinity, timeout, schedule seed, and
confidence method.

The measured runner never accesses the network and never rewrites the fixture checkout:

```sh
scripts/bench-matrix.py
```

On a new checkout, fetch the pinned fixtures once before disconnecting from the network:

```sh
scripts/fetch-v8-v7.py
```

The default run builds Lumen with Cargo's offline mode, verifies every fixture byte, and runs each
workload/engine pair in a balanced interleaved order. It writes a checkpointed JSON report under
`benchmark-results/` containing raw warmup and measured samples, wall/CPU/peak-RSS measurements,
scores, confidence intervals, exact commands and environment policy, executable hashes/sizes,
host information, Git revisions/dirty state, and the accepted production TRust checkpoint. Lumen's
opt-in record additionally reports JIT compilation/code size and exact collector-boundary
object/scope populations, reclaimed nodes, total/max pause, and a bounded pause histogram.

Object and scope populations are graph-node counts, not managed-byte estimates. Current objects
retain separately allocated property/element storage, shared strings, scopes, interpreter side
tables, caches, and external `ArrayBuffer` storage. Phase 0 will not multiply node counts by a
struct size and call the result a heap measurement; byte accounting must cover those ownership
classes explicitly.

Useful focused invocations are:

```sh
scripts/bench-matrix.py --engine lumen-jit --workload navier-stokes --warmups 0 --samples 1
scripts/bench-matrix.py --engine node --engine lumen-jit --workload regexp --samples 7
scripts/bench-matrix.py --no-build --cpu none
```

Version mismatches fail by default because silently comparing different V8/Bun builds defeats the
manifest. Use `--allow-version-mismatch` only to capture an explicitly exploratory result; the
report records the mismatch and is not an accepted baseline. CLI overrides likewise remain in the
report and do not alter the checked-in manifest.

## Regression policy

- Standards, Test262/WPT, tier-differential, and real-site failures are release blockers regardless
  of performance.
- Compare interleaved distributions from the same manifest and machine. Do not promote a single
  sample, best run, or historical directional ratio as a result.
- A stable component is provisionally regressed when its 95% comparison interval excludes parity
  in the slower direction and the median loss exceeds 3%. Noisy components use a threshold fixed
  from their Phase 0 variance before an optimization is measured.
- Investigate component scores, wall/CPU time, peak and post-GC memory, pauses, compilation time,
  and code size independently. An aggregate improvement does not excuse a visible DOM, latency,
  memory, startup, or completion regression.
- A meaningful regression requires a written explanation and explicit user acceptance. Otherwise
  rework or revert the responsible optimization.
- Update `accepted-production.json` only after the user approves and promotes exact release
  artifacts. Preserve the preceding report rather than overwriting history.
