# Offline browser replay gates

These fixtures exercise TRust's production Lumen page pipeline without a terminal, window, or
network. Every fixture must finish with matching nonempty `data-replay-checksum` and
`data-replay-expected` attributes and without a JavaScript error or panic.

`event-loop.html` is the fast inner-loop gate. `dom-reconcile.html` stresses large DOM replacement,
selectors, mutation delivery, and layout. `speedometer-vue-todomvc.html` runs the active
Speedometer 3.1 Vue TodoMVC workload: Vue 3.2.47 mounts the application, then the replay performs
the official 100-add, 100-complete, and 100-delete interaction shape.

The third-party JavaScript and CSS are not checked into Lumen. Their upstream repository, revision,
paths, and SHA-256 digests are pinned in `../browser-replay-assets.json`; the runner verifies the
local cache before starting and cannot provision it. Provision once, separately:

```sh
scripts/fetch-browser-replay-assets.py
# Or copy and verify from an audited checkout of the pinned revision:
scripts/fetch-browser-replay-assets.py --source-checkout /path/to/Speedometer
```

Run the gates from the Lumen root:

```sh
scripts/run-browser-replays.sh quick      # one small debug replay, normally about one second
scripts/run-browser-replays.sh check      # every replay once in debug
scripts/run-browser-replays.sh benchmark  # release, one warmup and five measured rounds
```

The runner builds the sibling `../TRust` checkout offline and writes a hash-bearing JSON report to
the ignored `benchmark-results/` directory. `TRUST_REPLAY_ROOT` selects another TRust checkout,
`LUMEN_REPLAY_CPU=none` disables affinity, and another integer selects a different CPU.
