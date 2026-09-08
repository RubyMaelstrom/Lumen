# TRust click-away focus

Scope: clicking outside a page text editor releases keyboard focus without losing entered text, in desktop and terminal TRust. No site-specific behavior.

## Causes and correction

Desktop returned early for any page hit carrying an actor or link, before ending the active native edit. Ordinary DOM content therefore kept the old editor active. Clicking inside the same editor also recreated its native editor unnecessarily.

The terminal's form prompt used `Mode::Search`, but page-click activation only ran in `Mode::Session`. The prompt never saw click-away. It now commits the draft and closes, retaining the original clicked target before closing the prompt changes viewport geometry. Clicking another field or link still performs that action. Clicking within the same field or native prompt retains the draft. Non-form protocol queries are not submitted on blur, and scrollbar handling retains its existing early capture.

Both frontends now send a distinct page-focus notification to the resident actor. It resolves editor/button descendants to their focusable ancestor or falls back to the document viewport. The existing trusted focus-event implementation supplies blur/focusout and focus/focusin, with related targets. Programmatic `.click()` remains non-focusing. `contenteditable=false` is no longer treated as an editing host merely because the attribute exists.

The value update precedes the focus notification. Focus notifications checkpoint handlers and present their mutations, but do not emit an unrelated settle acknowledgment that could prematurely acknowledge a pending edit or click. Three existing controller tests caught that sequencing error during development and pass unchanged with the correction.

## Standards consulted locally

Sources are the local 2026-09-06 snapshots, not a fresh network verification.

- HTML focusing/unfocusing and focus-update steps: [local source](/big/web-standards/repositories/whatwg/html/source:86812), [official clause](https://html.spec.whatwg.org/multipage/interaction.html#focusing-steps). Commit `e5071a20c8569d8a3ec02ed27dd01b948773f850`.
- HTML click focusability and synthetic activation distinction: [local source](/big/web-standards/repositories/whatwg/html/source:86281), [official clause](https://html.spec.whatwg.org/multipage/interaction.html#click-focusable). Editing-host definition: [local source](/big/web-standards/repositories/whatwg/html/source:88235), [official clause](https://html.spec.whatwg.org/multipage/interaction.html#editing-host).
- UI Events focus event order: [local source](/big/web-standards/repositories/w3c/uievents/sections/event-focusevent.txt:69), [official clause](https://w3c.github.io/uievents/#events-focusevent-event-order). Commit `8c1b80982c16b10f28a210e4541ac799dbe39a51`.
- Selection API User Interactions: [local source](/big/web-standards/repositories/w3c/selection-api/index.html:1004), [official specification](https://w3c.github.io/selection-api/). Commit `b475d4ba72cbe08f5015040dcd9fb4cf9133dd00`. Blur must not blindly empty the document Selection; this change never calls the Selection API. It does not implement the engine's remaining Selection API functionality or a new pointer-event dispatch model.

## Verification

Artifacts: [click-away-20260908-02M7YY](/big/Code/Lumen/benchmark-results/click-away-20260908-02M7YY). Contains the before-source archive, isolated this-turn patch, local HTTP fixture, private-compositor native-input harness, release hashes, event ledgers, and screenshots. All browser profiles and input were private; no user browser was controlled.

The old desktop release demonstrably changed the field from `ab` to `abz` after clicking outside and then typing. The old terminal release retained its edit prompt and uncommitted draft. The new behavior preserves `ab`, releases the editor, and exposes the expected focus transition to page code. The fixture additionally exercises textareas, button transfer, same-editor clicks, rich text, and password fields.

Seven new regression tests cover native focus transitions, ancestor resolution, synthetic-click behavior, untouched Selection access, event-loop checkpoints, draft-before-blur ordering, all editor kinds, field/link switching, and non-form prompts.

Final source verification: 1,200 library tests passed, 20 ignored; the two existing fresh-Wasm memory soak tests were excluded, as in the preceding optimization runs. All 29 desktop tests passed. Scoped rustfmt, JavaScript syntax checking, and `git diff --check` passed. Clippy completed with the same 14 pre-existing warnings outside this change.

Release acceptance uses `cargo build --release --bin trust --bin trust-desktop` with `CARGO_PROFILE_RELEASE_STRIP=none`, retaining production optimization and LTO. No installed executable is replaced, and no commit or push is performed by this task.

Final release SHA-256:

- `trust`: `7cd96ca1a3b8b786c0454f46108e721cd15006fca3ae8d2137f2ba796d4a03c5`
- `trust-desktop`: `9c20d448bd56d5fb972f1e3edaa2c17da322c80d869c3b92d6c2bda17dec67ca`

Both final binaries passed the native-input replay in `terminal-final` and `desktop-final`. `node verify.mjs final` passed every value/focus/event-order assertion and confirmed the replayed hashes match the binaries still in `target/release`. The private test browsers, HTTP fixture servers, and compositors were closed afterward.
