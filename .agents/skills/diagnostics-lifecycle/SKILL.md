---
name: diagnostics-lifecycle
description: Check diagnostics capture admission, stop draining, span isolation, and resource limits when changing Recorder, Span, Writer, or trace serialization. Exhausts bounded Loom schedules and real-source input sequences, with scratch source mutation controls.
---

# Diagnostics lifecycle

Stopping a capture must handle every admitted event before returning success.
An operation begun outside the current capture must not append its end to that
capture. Capture limits must bound retained data and finished trace size.
Violations lose timing evidence, attribute old operations to new captures, or
let diagnostics consume more resources than promised.

## Run and interpret

From repository root, with Python 3.10+ and stable Rust installed:

```sh
python3 .agents/skills/diagnostics-lifecycle/check.py
python3 .agents/skills/diagnostics-lifecycle/check.py --mutations
```

Cargo downloads locked dependencies on first run. Loom is pinned to 0.7.2.
No Tauri build, GPU, microphone, or speech model is needed. Real-writer checks
use temporary files and successful host filesystem operations.

1. Read the covered methods below and compare changes since the sync commit.
   Confirm the channel boundary still represents production sender lifetime.
2. Run the normal check. Exit 0 means every bounded property passed. Exit 1
   means at least one property failed. Exit 2 means compilation, tool failure,
   timeout, missing test, or incomplete schedule exploration; it proves nothing.
3. After covered source or harness changes, run `--mutations`. Require three
   `CAUGHT` lines and the restored baseline verdict. Existing property failures
   still return 1 after controls succeed. Controls never edit working source.
4. Read `COUNTEREXAMPLE` lines as paths through actual methods. Preserve failures
   as findings. Update a promise only for an intended behavior change; change
   product code only when authorized. Use `/maintain-formal-verification-skill`
   to resynchronize verification after unverified source changes.

Logs and each mutated production source are written to
`target/diagnostics-lifecycle/`. `baseline.log` and `restored.log` preserve full
output. A compilation error alone never counts as a caught mutation.

## Current findings

The ordinary checker currently exits 1 with two failing properties.

**Old operation ends enter a newer capture.** `Recorder::span` stores recorder,
run, and name; `Span::end` invokes `Recorder::end`, which timestamps the end at
completion. No capture identity is retained. Both exhaustive handle cases fail:

1. Begin an `Output` span while inactive, start capture B, end the span.
2. Begin an `Output` span in capture A, stop A, start B, end the old span.

Both paths append an unmatched `output` end to B. Source correspondence is
[`Span` and `Span::end`](../../../src-tauri/src/diagnostics.rs#L304),
[`Recorder::span`](../../../src-tauri/src/diagnostics.rs#L344), and
[`Recorder::end`](../../../src-tauri/src/diagnostics.rs#L370). The existing
product test `operation_end_observes_current_capture` explicitly expects this
behavior. That expectation does not satisfy the requested isolation promise.

**The finished file is not bounded by `byte_cap`.** `Writer::run` applies its
budget only to accepted input-event lines plus header bytes. Synthetic incomplete
ends, process/thread metadata, and `otherData` bypass it. A cap of 128 bytes with
one admitted `Output` begin produces one synthetic end and a 737-byte finished
file on the recorded environment. Even an empty capture with cap 0 writes 494
bytes. In the 320 bounded cases, 295 exceed the requested cap. Exact file sizes
depend on app/platform metadata and timestamp width; the inequality is checked
on the real finished file each run.

The product constant describes a hard 32 MiB trace ceiling. These checks use
the same real writer with its existing tiny-cap parameter, not a claim that a
32 MiB production trace was generated. The accepted-event byte accounting
passes separately and is not substituted for the failing full-file promise.
Source correspondence is [`BYTE_CAP`](../../../src-tauri/src/diagnostics.rs#L28)
and [`Writer::run`](../../../src-tauri/src/diagnostics.rs#L598). `threads` also
records distinct producer tids before the event-budget check, so metadata and
its in-memory map can grow even when every event is truncated. No fixed total
memory bound follows from the bounded channel or accepted-event byte budget.
The bounded real-writer witness uses cap 0 and 0 through 3 distinct producer
tids. No event is accepted, but retained thread entries increase from 0 through 3. The harness observes these entries through the thread metadata that the
actual writer emits directly from its `threads` map. The resource property also
requires thread state to stop growing after the event budget is exhausted.
General growth across arbitrary producer counts is an inference from the
unconditional map insertion, not an unbounded memory proof.

## Coverage and source correspondence

Last source correspondence checked at
`af0f7dbe419feb81bd03fa301a70ad20474c827a`. The covered diagnostics source is
unchanged between that commit and the implementation baseline `61f27f4f`.
Move the sync commit forward only after comparing source, reviewing every
boundary below, and rerunning both commands.

`loom/build.rs` parses `src-tauri/src/diagnostics.rs` with `syn` each relevant
rebuild and emits actual item/method bodies with `quote`. No algorithm is cloned
or repaired in the harness.

| Harness   | Extracted production source                                                                                                                                                                     | Boundary                                                                                                                                                                                                                                     |
| --------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `real.rs` | Entire module except its existing test module and test vocabulary arrays. Includes all `Recorder`, `Span`, `Writer`, serialization, start/stop, locks, channel, and file operations.            | No product boundary stubs. Test config uses fixed allowlisted values; `CARGO_PKG_VERSION` is the standalone crate's 0.1.0. The direct writer fixture preloads a real bounded queue and drops its sender before calling actual `Writer::run`. |
| `race.rs` | Exact structs/enums `Capture`, `Event`, `Recorder`, `Status`, `DiagError`, `RunId`, `Phase`, `ThreadTag`, `Args`, `Name`, `Code`; `STOP_TIMEOUT`; exact `Recorder::new`, `emit`, `stop` bodies. | Unqualified atomics, `Arc`, `Mutex`, `RwLock` resolve to Loom. Event time equals capture epoch. Files and writer are replaced only at channel completion.                                                                                    |

The Loom sender models nonblocking successful enqueue or a full queue. Its
single sender closes on drop; no clone exists in production `Capture`. The
modeled writer waits until closure, drains the FIFO, and reports success through
the `done_rx.recv_timeout` boundary. It returns timeout if that boundary is
called while the sender remains live. There is no concurrent consumer. This
covers the legal real schedule where the writer first consumes after detach;
it does not exhaust other writer-consumption schedules. The real-writer input
enumeration independently checks draining, truncation, finalization, and JSON.

`emit` keeps its actual read guard across timestamp admission, `try_send`, and
admitted-count increment. `stop` retains its actual control mutex, active-store,
capture write-lock/drop, and completion wait. Send/count and stop are not merged
into invented atomic steps. The real lock lifetime decides their ordering.
The generated single-line token output needs one local Clippy whitespace-lint
allowance on the real-source module; product code is unchanged.

## Exhaustive bounds and controls

| Property                        | Exhausted scope                                                                                                                                                                                                                                                                    |
| ------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Admission versus stop           | One producer emits two events; one concurrent stop; queue capacity 2; joining thread. Loom exhausts all modeled schedules, currently 576, and requires witnesses admitting 0, 1, and 2 events. No preemption/permutation/duration/checkpoint pruning.                              |
| Real stop accounting            | Queue capacities 1 through 3 crossed with input counts 0 through 4, 15 inputs. Real background writer timing is host-selected, not exhaustively scheduled. Every input is admitted or dropped; every admitted event appears before successful stop returns with an ample byte cap. |
| Actual writer drain             | All 40 sequences of length 0 through 3 over instant, begin, end; queue capacity 4; ample byte budget. Unique run and thread IDs per event.                                                                                                                                         |
| Epoch admission                 | One event timestamp before, equal to, or after capture epoch. Only pre-epoch input must be rejected.                                                                                                                                                                               |
| Old span isolation              | One handle begun inactive or in prior capture, then ended in the next capture, two cases.                                                                                                                                                                                          |
| Accepted and total byte budgets | All 40 phase sequences crossed with byte caps 0, 1, 16, 64, 128, 256, 512, 1024; 320 cases each. Accepted bytes are measured from actual serialized output; total bytes from the finished file.                                                                                    |

Every command has a 180-second timeout. Loom has a 1,000-branch guard per
execution; hitting either guard is incomplete verification, not a pass.

Controls mutate scratch copies of the actual source:

- `stop_early` takes only the first queued event in `Writer::run`. The exhaustive
  real-writer drain assertion fails with `writer lost admitted event`.
- `accept_pre_epoch` removes `emit`'s timestamp gate. The admission-isolation
  assertion fails with `pre-epoch event admitted into new capture`. This control
  demonstrates the existing epoch gate's sensitivity; it does not establish
  generation binding for `Span`, whose separate stronger property still fails.
- `ignore_event_budget` removes `Writer::run`'s event-byte gate. Measured accepted
  output exceeds the event budget and fails with `event byte budget bypassed`.
  The stronger total-file property remains a baseline finding.

These bounds do not prove all capture lengths, the default 32 MiB boundary,
arbitrary producer counts, concurrent start/stop beyond the one stop,
writer scheduling, process crashes, disk errors, rename failures, timeout
accuracy, integer overflow, run/thread ID wraparound, privacy of future event
vocabulary, or a fixed bound on total memory. A passing component never turns
the two failed stronger promises into passing ones.
