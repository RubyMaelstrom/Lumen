# Offline browser replay gates

These fixtures exercise TRust's production Lumen page pipeline without a terminal, window, or
network. Every fixture must finish with matching nonempty `data-replay-checksum` and
`data-replay-expected` attributes and without a JavaScript error or panic.

`event-loop.html` is the fast inner-loop gate. `dom-reconcile.html` stresses large DOM replacement,
selectors, mutation delivery, and layout. `speedometer-vue-todomvc.html` runs the active
Speedometer 3.1 Vue TodoMVC workload: Vue 3.2.47 mounts the application, then the replay performs
the official 100-add, 100-complete, and 100-delete interaction shape.

The third-party JavaScript and CSS are not checked into Lumen. Their upstream repository, revision,
paths, and SHA-256 digests remain pinned in `../browser-replay-assets.json` for historical
reference. The local provisioning and replay helpers have been removed.
