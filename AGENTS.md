# Working on Lumen

This file collects the implementation and testing guidance that used to live in
README.md. Keep README.md a short introduction for people discovering the project.

## Project and workspace

Lumen is a JavaScript engine written in Rust and the JavaScript backend used by
TRust. The workspace also provides a standalone runtime. Keep the engine independent
of a particular host; use the `embed` feature's curated API for host integration.

| Crate | Responsibility |
| --- | --- |
| `lumen` | Lexer, parser, interpreter, bytecode VM, native JIT, builtins, embedding API |
| `lumen-host` | Host state, resource tables, extensions, worker and callback primitives |
| `lumen-timers` | Timers, immediates, microtask integration |
| `lumen-fs` | Synchronous and asynchronous filesystem operations |
| `lumen-web` | Web APIs and their JS glue, HTTP, Streams, WebSocket, WebAssembly support |
| `lumen-node` | Node compatibility, CommonJS resolution, native addons, Bun compatibility APIs |
| `lumen-tls` | TLS transport using dynamically loaded system OpenSSL |
| `lumen-runtime` | Event loop, module loading, console/process, assembly of host extensions |
| `lumen-repl` | Interactive shell with a persistent realm |
| `lumen-cli` | Runtime command-line entrypoint |
| `lumen-typescript` | TypeScript parsing and checking |
| `lumen-wasm` | WebAssembly build of the JavaScript engine |
| `test262-runner` | Test262 conformance harness |
| `lumen-difftest` | Differential testing across execution tiers |
| `wasm-spec-runner` | Official WebAssembly test harness |

The main layering is engine → host substrate → op crates → runtime → REPL → CLI.
Keep this dependency direction acyclic. Consult the manifests for the complete graph.

The workspace is no longer zero-dependency. The native engine uses `stacker`;
web support uses URL/IDNA and encoding crates; Wasm bindings and test tooling have
their own dependencies. Check Cargo.toml and Cargo.lock before making dependency
claims. System libraries and the vendored Streams polyfill are separate from Cargo
dependencies.

## Execution and correctness

The tree-walking interpreter is the reference implementation for differential
testing. All three tiers must agree on observable behavior: completion values,
errors, side effects, and final state. Use the language specification to resolve
semantic questions; agreement between tiers alone does not prove correctness.

Eligible functions compile to bytecode, with inline caches, object shapes, and
dense-array fast paths. The native template JIT lowers bytecode to ARM64 or x86-64
on supported desktop platforms, with checked helpers for slow paths. Other targets
fall back to bytecode. Backend coverage differs; check both `jit.rs` and
`jit_x64.rs` when changing shared operations or layouts.

The default tier is JIT. Useful diagnostic controls:

- `LUMEN_TIER=interp|bytecode|jit` selects the tier.
- `LUMEN_TIER_THRESHOLD=N` changes the call threshold for tiering up; loop-containing
  bodies can tier up immediately.
- `LUMEN_INLINE_AT=N` changes the speculative inline recompile threshold (default:
  100 machine-code runs); zero disables inlining.
- `LUMEN_JIT_NO_DIRECT_CALLS=1` disables ARM64 shared-context direct calls while
  retaining ordinary JIT calls.
- `LUMEN_TIER_LOG=1` helps diagnose compilation bailouts.

The standalone engine shell accepts `--tier=interp|bytecode|jit`. The runtime CLI's
current argument parser only accepts `--tier=interp|bytecode`; use `LUMEN_TIER=jit`
or the default for runtime JIT execution.

Preserve activation and GC boundaries in generated calls: bind thread-local GC
state at execution time and discard callee-owned exception handlers before restoring
the caller. Keep raw object-layout assumptions validated against live Rust types
and retain checked fallbacks. Executable memory must follow platform write/execute
protections and instruction-cache synchronization requirements. Do not describe
the interpreter or VM as entirely safe Rust; they also contain unsafe code.

The default-on `intl` feature supplies ECMA-402 and CLDR data. For the engine,
`--no-default-features` removes `Intl` and gives `toLocale*` methods their
locale-independent behavior. The `embed` feature exposes the host API; `bench`
exposes internal measurement entrypoints. The `heap-bridge` feature is an opt-in
migration facility, not the default production object representation.

## Runtime and integration

One event-loop thread owns the non-Send engine. Blocking work goes through a std
thread pool and returns through completion channels. Preserve microtask, callback,
timer, and I/O ordering when changing scheduling.

The runtime supports CommonJS and ESM. The CLI chooses ESM for `.mjs`, CommonJS for
`.cjs`, and uses the nearest package.json `"type"` for `.js`. Preserve module-graph
linking, top-level await, `node:` imports, CommonJS interop, and
`require.main === module` for CommonJS entrypoints.

Web API code lives in `crates/lumen-web/src/js/` and its Rust host operations.
`crates/lumen-web/build.rs` assembles the JS in order and precompiles an AST snapshot.
Keep the source bundle and snapshot consistent through that build path.

HTTPS client support uses system OpenSSL on Unix through `lumen-tls`, with
certificate and hostname verification. Availability depends on the platform and
installed libraries. `Lumen.serve` is the runtime's HTTP server API; check
`crates/lumen-web/src/server.rs` for current transport and buffering limits.

Streams is implemented. Preserve
[STREAMS_UPSTREAM.md](crates/lumen-web/src/js/STREAMS_UPSTREAM.md) and
[STREAMS_LICENSE](crates/lumen-web/src/js/STREAMS_LICENSE).
The upstream note documents the vendored bundle, its rebuild procedure, and the
private `_readSync` hook used by the buffered Fetch adapter.

Native addons enter through the dynamic library loader and N-API implementation
in `lumen-node`. Keep their ABI and lifecycle behavior covered by focused tests.
Package compatibility examples include Hono, React SSR, Vite/Rollup, and native
addons; their READMEs explain setup. Check implementation and tests before claiming
full Node, N-API, or web API compatibility: some module-header checklists are stale.

## Build and test

Run commands from the repository root. Build the runtime and engine shell separately:

```sh
cargo build --release -p lumen-cli
./target/release/lumen-cli -e 'console.log("Hello from Lumen!")'
./target/release/lumen-cli repl
./target/release/lumen-cli file.js

cargo build --release -p lumen --bin lumen
./target/release/lumen --tier=interp file.js
```

The engine shell lacks the runtime's host APIs. The REPL supports multiline input
and top-level await; line editing is line-buffered, with `rlwrap` usable for history.

Choose checks appropriate to the change. Common commands are:

```sh
cargo check --workspace --all-targets
cargo test -p lumen
cargo test -p lumen-runtime
cargo run --release -p lumen-difftest -- --count 2000
```

Run checks explicitly. Do not add Git hooks or hook installation machinery to this
project.

Differential tests compare completions, errors, side-effect traces, and final global
state, and minimize divergences into a regression corpus. Add focused regressions
for changed semantics and exercise the affected tiers. A native check alone does
not establish cross-platform or wasm32 correctness.

For Test262:

```sh
scripts/test262-clone.sh
scripts/run-test262.sh language/expressions/addition
LUMEN_TIER=jit scripts/run-test262.sh .
```

Without arguments the runner covers only language expressions and statements;
`.` selects the full test directory. Paths are relative to `test262/test`.
The runner writes `test262-report/summary.json`. Check its source for current
worker controls and limits before a broad run.

The old README recorded 53,574 passes, zero failures, and four skips on the default
JIT tier on 2026-08-26, against Test262 revision
`d86b2294eb0a17eaa281ff12c73c473ec864c72f`. This is a historical measurement, not
evidence about the current checkout. Report the tested engine revision, suite
revision, tier, scope, failures, and skips with new conformance results.

## Performance work

Use release builds and record the machine, revision, tier, flags, and workload.
Follow [benchmarks/README.md](benchmarks/README.md) for comparison and regression
policy. Check correctness as well as throughput, memory, startup, and compilation
costs. Investigation tools and reports may exist locally without being tracked;
do not assume a fresh clone includes them.

The tracked benchmark runners use external checkouts:

| Runner | Default checkout | Override | Focused run |
| --- | --- | --- | --- |
| `scripts/run-octane.sh` | `../octane` | `OCTANE` | `scripts/run-octane.sh richards crypto` |
| `scripts/run-web-tooling.sh` | `../web-tooling-benchmark` | `WEB_TOOLING_BENCHMARK_DIR` | `scripts/run-web-tooling.sh --only babel` |
| `scripts/run-ares6.sh` | `../ARES-6` | `ARES6` | `scripts/run-ares6.sh air basic` |

Octane uses the chromium/octane repository; Web Tooling uses v8/web-tooling-benchmark
and requires `npm install` in that checkout to build its CLI bundle. ARES-6 sources
are not vendored. All three runners accept `LUMEN_BIN` to use an existing binary.

Web Tooling's `--only` rebuilds the external bundle using webpack's build-time
selector; it is not a runtime filter. Full suites can be lengthy, so start with
selected workloads. Octane scores are higher-is-better; ARES-6 reports a geomean in
milliseconds, lower-is-better. A selected ARES-6 run is a partial result, not the
official full-suite score. Missing completion markers or failed workloads invalidate
a run.

## Documentation

Keep useful public setup instructions in READMEs, implementation guidance here,
and third-party provenance beside the vendored source. General Markdown workfiles
and the retired changelog are ignored and kept locally. Do not make builds or
required contributor instructions depend on those untracked documents.
