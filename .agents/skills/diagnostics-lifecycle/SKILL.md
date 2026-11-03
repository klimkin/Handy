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
3. After covered source or harness changes, run `--mutations`. Require six
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

The ordinary checker exits 1 with one failing property, old-span isolation.
The resource limit property passes after the writer budget fix.

**Old operation ends enter a newer capture.** `Recorder::span` stores recorder,
run, and name; `Span::end` invokes `Recorder::end`, which timestamps the end at
completion. No capture identity is retained. Both exhaustive handle cases fail:

1. Begin an `Output` span while inactive, start capture B, end the span.
2. Begin an `Output` span in capture A, stop A, start B, end the old span.

Both paths append an unmatched `output` end to B. Source correspondence is
[`Span` and `Span::end`](../../../src-tauri/src/diagnostics.rs#L305),
[`Recorder::span`](../../../src-tauri/src/diagnostics.rs#L345), and
[`Recorder::end`](../../../src-tauri/src/diagnostics.rs#L371). The existing
product test `operation_end_observes_current_capture` explicitly expects this
behavior. That expectation does not satisfy the requested isolation promise.

## Resource limits

`Writer::write_trace` reserves the complete final output before accepting each event.
The invariant is `written bytes + reserved bytes <= byte_cap`. Reservations
cover process/thread metadata, the JSON footer, and synthetic ends for accepted
open spans. One borrowed footer serializer sizes maximum-width counters and
writes actual counters at stop. The event serializer sizes closures with a
maximum-width `u64` timestamp. Accepted ends release their matching reservation;
duplicate begins replace it. Rejected events change neither retained map.

`Recorder::start_inner` rejects a cap that cannot fit the mandatory envelope,
including maximum counters, before directory/file creation or active-state
publication. A bounded counting writer counts escaped config strings without
cloning or buffering the footer. The direct-writer fixture requires the same
rejection and no remaining capture state or file for impossible caps.

Every retained thread has budgeted metadata. Every retained open span has a
budgeted closure and an accepted begin. Their counts are bounded by the output
cap and cannot grow through rejected producers. This is a proportional bound,
not a claim that allocator bytes equal the file cap. Tree overhead, caller-owned
config, and the fixed-capacity event channel are separate resources.

The original source produced 295 total-file violations in 320 tiny-cap cases;
a zero-byte cap even produced a 494-byte empty trace. The updated property
measures every successful finished file, parses JSON, verifies exact thread
metadata membership, checks input/truncation accounting, and verifies synthetic
closure identities. Unsupported caps must fail explicitly. No full 32 MiB
production trace or native app stress run is claimed.

## Coverage and source correspondence

Last source correspondence checked at
`f04c6159295ae7c313e8e868336eb6c8d859e705`. The resource fix at
`427bc79b5a94140ff40e0c4d6540f4810132a2ad` changed budgeting and serialization.
The later refactor moves fallible writes into `Writer::write_trace`, shares
record framing, and streams footer fields through a map serializer. Admission
decisions and recorder stop concurrency boundaries stayed intact.
Both run commands were rerun with six mutation controls. Old-span isolation
remains a source bug, not verification drift.

`loom/build.rs` parses `src-tauri/src/diagnostics.rs` with `syn` each relevant
rebuild and emits actual item/method bodies with `quote`. No algorithm is cloned
or repaired in the harness.

| Harness   | Extracted production source                                                                                                                                                                     | Boundary                                                                                                                                                                                                                                                            |
| --------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `real.rs` | Entire module except its existing test module and test vocabulary arrays. Includes all `Recorder`, `Span`, `Writer`, serialization, start/stop, locks, channel, and file operations.            | No product boundary stubs. Test config uses fixed values plus one JSON-escaping stress case; `CARGO_PKG_VERSION` is the standalone crate's 0.1.0. The direct writer fixture preloads a real bounded queue and drops its sender before calling actual `Writer::run`. |
| `race.rs` | Exact structs/enums `Capture`, `Event`, `Recorder`, `Status`, `DiagError`, `RunId`, `Phase`, `ThreadTag`, `Args`, `Name`, `Code`; `STOP_TIMEOUT`; exact `Recorder::new`, `emit`, `stop` bodies. | Unqualified atomics, `Arc`, `Mutex`, `RwLock` resolve to Loom. Event time equals capture epoch. Files and writer are replaced only at channel completion.                                                                                                           |

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
allowance on the real-source module; extracted production bodies are unchanged.

## Exhaustive bounds and controls

| Property                        | Exhausted scope                                                                                                                                                                                                                                                                    |
| ------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Admission versus stop           | One producer emits two events; one concurrent stop; queue capacity 2; joining thread. Loom exhausts all modeled schedules, currently 576, and requires witnesses admitting 0, 1, and 2 events. No preemption/permutation/duration/checkpoint pruning.                              |
| Real stop accounting            | Queue capacities 1 through 3 crossed with input counts 0 through 4, 15 inputs. Real background writer timing is host-selected, not exhaustively scheduled. Every input is admitted or dropped; every admitted event appears before successful stop returns with an ample byte cap. |
| Actual writer drain             | All 40 sequences of length 0 through 3 over instant, begin, end; queue capacity equal to input length, minimum 1; ample byte budget. Unique run and thread IDs per event.                                                                                                          |
| Epoch admission                 | One event timestamp before, equal to, or after capture epoch. Only pre-epoch input must be rejected.                                                                                                                                                                               |
| Old span isolation              | One handle begun inactive or in prior capture, then ended in the next capture, two cases.                                                                                                                                                                                          |
| Accepted and total byte budgets | All 40 phase sequences crossed with byte caps 0, 1, 16, 64, 128, 256, 512, 1024; 320 cases each. Every successful file must fit; unsupported caps must reject without state or files.                                                                                              |

Every command has a 180-second timeout. Loom has a 1,000-branch guard per
execution; hitting either guard is incomplete verification, not a pass.

Controls mutate scratch copies of the actual source:

- `stop_early` takes only the first queued event in `Writer::write_trace`. The exhaustive
  real-writer drain assertion fails with `writer lost admitted event`.
- `accept_pre_epoch` removes `emit`'s timestamp gate. The admission-isolation
  assertion fails with `pre-epoch event admitted into new capture`. This control
  demonstrates the existing epoch gate's sensitivity; it does not establish
  generation binding for `Span`, whose separate stronger property still fails.
- `ignore_total_budget` removes the combined written/reserved-byte gate. The
  finished-file assertion detects oversized output.
- `omit_thread_budget` drops new-thread metadata reservations. The finished-file
  assertion detects metadata that exceeds the cap.
- `omit_span_budget` drops synthetic end reservations. The finished-file
  assertion detects unbudgeted incomplete closures.
- `undersize_footer` sizes footer counters as zero. Maximum-width dropped
  counters expose the undersized footer reservation.

The four resource controls fail with `finished capture exceeds resource limits`.
All six controls require completed named assertion failures; compile errors or
unrecognized panics are incomplete verification.

Additional resource inputs use the actual writer and serialization:

- 12 cases cross 1, 2, 64, and 512 distinct unfinished-span producers with caps
  2048, 4096, and 8192, maximum run/thread IDs, payload counts, and drop counters.
- 422 cases sweep caps 512 through 4096 in steps of 17 for a begin alone and a
  begin/end pair, checking both incomplete closure and accepted completion.
- 880 cases sweep caps 512 through 2048 in steps of 7 for duplicate begins with
  different thread-ID widths, cross-thread ends, rejected rich ends, and a
  smaller event after rejection. Coverage witnesses must show replacement,
  rejected-end closure, and later acceptance.
- Four caps exercise a config string with extensive JSON escaping. Rejection
  must precede capture creation; ample caps preserve the config exactly.

These bounds do not prove all capture lengths, the default 32 MiB boundary,
arbitrary producer counts, concurrent start/stop beyond the one stop,
writer scheduling, process crashes, disk errors, rename failures, timeout
accuracy, integer overflow, run/thread ID wraparound, privacy of future event
vocabulary, or an exact allocator-byte ceiling. Retained writer indexes have the
proportional bound above. Passing resource checks never turn the remaining
old-span isolation finding into a pass.
