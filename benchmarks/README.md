# Benchmark records

This directory retains the checked-in benchmark manifests, fixtures, and historical performance
policy. The local investigation runners and provisioning helpers have been removed.

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
