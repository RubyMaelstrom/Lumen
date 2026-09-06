# RegExp native-subset contract

Status: bounded Phase 6 design record, 2026-09-04.

This document freezes the semantic boundary for Lumen's first specialized RegExp matcher and the
reference semantics of the matcher bytecode it replaces. The existing general matcher remains the
reference implementation, and every unsupported pattern must continue through it.
The relevant language behavior is defined by [ECMA-262 RegExpBuiltinExec and its matcher
algorithms](https://tc39.es/ecma262/multipage/text-processing.html#sec-regexpbuiltinexec).

## Eligible representation

A `OneByteNativeProgram` is created only from the body of the canonical program:

```text
Save(0), op[0], op[1], ..., op[n - 1], Save(1), Match
```

The body must be non-empty, capture-free, and contain only the following instruction mappings:

| Matcher instruction | Specialized operation | Predicate on a one-byte subject element |
| --- | --- | --- |
| `Inst::Char(c)` where `c < 0x100` | `OneByteNativeOp::Char(c)` | `element == c` |
| `Inst::Any` | `OneByteNativeOp::Any` | element is not a line terminator |
| `Inst::Class(class)` with an ASCII/Latin-1 LUT | `OneByteNativeOp::Class(class)` | `class.matches(element, false, false)` |
| `Inst::AssertStart` without multiline | `OneByteNativeOp::AssertStart` | candidate start is input start |
| `Inst::AssertEnd` without multiline | `OneByteNativeOp::AssertEnd` | candidate end is input end |
| terminal `Inst::Many { rep, min, max, greedy }` | `OneByteNativeOp::Many` | a bounded one-byte run of `rep` |

The native operation owns no independent character-class definition: it shares the immutable class
object already referenced by the matcher program. A class LUT is the exact non-case-folded
membership result for elements `0..=255`, including class negation. Any instruction outside this
table rejects the specialization, including word-boundary assertions, multiline anchors, captures,
alternation, jumps, non-terminal repetitions, lookarounds, backreferences, string-valued classes,
and inline flag operations.

## Search and result semantics

The input is a prepared `ReText` whose `ascii_src` is present or whose cached subject shape is
one-byte. Therefore each matcher element is one byte/code unit and its element index is also its
ECMAScript UTF-16 code-unit offset. The operation scans candidate starts in increasing order from
`start` through the subject end. For fixed-width bodies, `n` is the number of consuming operations
and a successful candidate returns `(from, from + n)`. A terminal `Many` is eligible only when it is
the sole body instruction. It consumes the available contiguous run, capped by `max`, and returns
`avail` for greedy mode or `min` for lazy mode after checking the minimum. With sticky behavior,
only `from == start` is attempted.

The candidate sequence is equivalent to the matcher bytecode's left-to-right sequence: consuming
operations advance one element, anchors inspect the candidate boundary without consuming, and a
candidate succeeds only if every operation's predicate succeeds. Fixed-width operations have no
control flow or captures, so no alternative ordering, capture restoration, or backtracking state
exists to reproduce. Terminal `Many` likewise has no continuation and therefore needs no native
backtracking stack. Pure literal bodies remain on the existing eager literal search and are not
allocated as a second program.

`Any` uses the ECMAScript line-terminator predicate. In the one-byte input domain this excludes LF
and CR; the U+2028 and U+2029 cases are unreachable in this representation and remain handled by
the general matcher for two-byte, astral, and Unicode inputs.

## Interrupts, errors, and fallback

The specialized loop polls the same `RuntimeInterrupt` at candidate and operation boundaries. An
interrupt is returned as `MatchError::Interrupted` and is never converted into no-match. This
straight-line subset cannot exhaust the backtracking budget; patterns that require that budget,
or any other unsupported matcher state, use the general matcher unchanged.

Promotion is per compiled pattern after repeated execution and an observed ASCII or one-byte
subject. Legacy case-folding, dotAll, Unicode/UnicodeSets modes, and all unsupported instructions
decline promotion. The `LUMEN_REGEXP_TIER_UP_AT=0` diagnostic switch disables promotion for A/B
runs.
The emitted machine-code path runs for the fixed-width subset on ASCII `ReText` subjects and legacy
non-Unicode one-byte subjects on Unix AArch64 and x86-64. Terminal `Many` currently uses the same
lazy native-program promotion and a bounded-interrupt-polling Rust loop; it deliberately remains
out of the fixed-width machine emitter until that emitter has a repeat counter and poll path. The
machine path has separate byte-load and 32-bit-element-load entries, so the latter can match
Latin-1 code units without allocating a converted temporary buffer. UTF-16 two-byte, astral,
Unicode, and unsupported-flag inputs remain on the verified Rust or general matcher paths. W^X
mappings are charged to the shared 16 MiB live executable-code pool used by ordinary JS JIT chunks
and are released with the owning compiled pattern or chunk; a failed reservation falls back to the
checked matcher tier. Specialized operation metadata remains in retained RegExp accounting.

## Differential gate

The internal diagnostic test
`regex::internal_engine_diagnostics::one_byte_native_subset_differentially_matches_bytecode`
compares every operation predicate against its source `Inst` for all 256 byte values, then
compares complete search results across empty/short subjects, scan starts, sticky mode, repeat
bounds, and ASCII line terminators. The generated companion gate is emitted by
`scripts/gen-regexp-native-harness.py` into `crates/lumen/src/regex_native_generated.rs`; run the
generator with `--check` to reject stale output.

## Matcher bytecode reference contract

The compiled top-level program is a finite vector of instructions. The entry and successful exit
are conventionally `Save(0), body, Save(1), Match`; nested lookarounds carry their own vector and
terminate in `Match`. A program counter is an instruction index. The matcher state is
`(pc, position, capture slots, marks, flag stack, direction)`, plus a bounded step counter and an
optional abort (`ResourceExhausted` or host `Interrupted`). A failed instruction returns to the
most recent ordered backtracking choice; an abort is never converted into an ordinary no-match.

The instruction meanings are:

| Instruction | Reference behavior |
| --- | --- |
| `Char`, `Any`, `Class` | Consume one element when the active flag and Unicode/code-unit predicate succeeds; otherwise fail. `Any` excludes line terminators unless dotAll is active. |
| `StringSet` | Try UnicodeSets string alternatives in normative order: longer trie matches first, then a singleton, then the empty element. |
| `StringSetRepeat` | Repeat `StringSet` between `min` and `max` using explicit DFS state; greedy and lazy modes differ only in whether the sequel is tried before or after expansion. |
| `Save(slot)` | Write the current position to a capture slot. Backtracking restores the prior slot state. |
| `Split(first, second)` | Push `second` as the later alternative and continue at `first`; this ordering is observable for greedy/lazy and alternation semantics. |
| `Jmp(target)` | Continue at `target`. |
| `Match` | Succeed with the current capture slots. |
| `AssertStart`, `AssertEnd` | Require the input start/end, with multiline line-boundary behavior where specified. |
| `WordBoundary(positive)` | Test the transition between word and non-word elements, including the active direction and flags. |
| `Backref`, `BackrefAlt` | Match the captured element sequence under the active case rules; an unset capture fails, and an alternate tries its resolved group order. |
| `ClearCaps(lo, hi)` | Clear the capture slots in the supplied group range at the start of a repetition attempt. |
| `Look`, `LookBehind` | Run the nested program without consuming outer input; negate its success for negative assertions. Lookbehind runs the reversed body toward the left. |
| `Many(rep, min, max, greedy)` | Iteratively consume a single-element `rep`, recording continuation choices in the same greedy/lazy order as `RepeatMatcher`. |
| `PushFlags`, `PopFlags` | Push or restore `(ignoreCase, multiline, dotAll)` for an inline-modifier body. |
| `SetMark`, `CheckProgress` | Record an iteration position and fail an empty iteration, enforcing the RepeatMatcher progress rule. |

`position` is an element index inside the matcher. In legacy mode an element is one UTF-16 code
unit; in Unicode mode it is one code point, while public match indices remain UTF-16 unit offsets.
The native one-byte subset is valid only where these two offsets coincide. Capture slots retain
the complete internal state even when a caller projects only group 0.

The generated gate covers every consuming operation predicate in the current straight-line subset
over all 256 byte values and compares complete search results across starts, sticky mode, short
subjects, and line terminators. The machine-code gate additionally covers non-multiline start/end
anchors. Control-flow, capture, word-boundary, backreference, and Unicode operations remain
reference-only until they receive equivalent generated cases.

The machine-code gate additionally invokes the emitted entry on representative `Char`, `Any`,
class, and anchor programs, compares its group-0 span with the reference matcher, and verifies
cancellation is returned as `MatchError::Interrupted` rather than as no-match.
