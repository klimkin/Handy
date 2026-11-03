---
name: engine-lifecycle
description: Check model worker replacement, unload acknowledgement, late loaded-model publication, and bounded CPU recovery when changing EngineSupervisor lifecycle or selection policy.
---

# Engine lifecycle

Use this skill for priorities 2–4 of the transcription lifecycle checks.
Previous model workers must exit and be reaped before replacement or unload
acknowledgement. A pending unload must suppress late loaded-model publication.
Recovery must stay bounded, preserve cancellation, and respect explicit devices.

## Run and interpret

From repository root with Python 3.10+ and stable Rust:

```sh
python3 .agents/skills/engine-lifecycle/check.py
python3 .agents/skills/engine-lifecycle/check.py --mutations
```

Loom is pinned to 0.7.2. Cargo.lock pins dependencies. Exit 0 means all ten
properties pass, 1 means property failure, and 2 means incomplete verification.
Infrastructure errors and schedule limits are never passing verdicts.

Current baseline has seven passing checks and three failing properties.
Mutation mode executes twelve scratch-source controls and repeats baseline.
Expect twelve `CAUGHT` lines and the same baseline findings. Preserve logs and
mutated source from `target/engine-lifecycle/`.

## Checks and scope

| Check                                                    | What it verifies                                                                                                                   | Bound                                                                                                          |
| -------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- |
| `previous_worker_exits_before_replacement`               | No replacement allocates while an earlier worker is live or unreaped.                                                              | All 64 six-operation load/unload words, for cooperative and forced shutdown, 128 cases.                        |
| `unload_acknowledgement_follows_reaping`                 | Public `Unloading::wait` cannot return before worker reaping.                                                                      | One unload, owner queue thread and requesting thread, two shutdown outcomes, exhaustive modeled schedules.     |
| `pending_unload_blocks_late_publication`                 | A racing load completion cannot publish over one pending unload.                                                                   | One publisher and one unload request, exhaustive modeled schedules.                                            |
| `load_recovery_is_bounded_and_respects_selection`        | Load attempts stay at most two; retry uses CPU-only worker; CLI device index and CPU-only choices do not fall back.                | 300 combinations of two native outcomes, three backends, indexed/automatic selection, and host GPU permission. |
| `transcription_retry_is_bounded`                         | At most two runs; no retry for cancellation, remote failure, pinned device, or CPU hang; cancellation does not enter GPU recovery. | 150 combinations of two outcomes, three backends, indexed/automatic selection.                                 |
| `cancelled_queued_work_never_starts`                     | An old cancellation mark rejects work before native dispatch.                                                                      | One queued old mark and one completed cancel.                                                                  |
| `persisted_gpu_choice_never_falls_back`                  | Persisted exact GPU choice does not succeed on CPU after a GPU failure.                                                            | A persisted device key, a lost GPU worker, and successful CPU retry. Current violation.                        |
| `publication::manager_loaded_id_respects_pending_unload` | Late manager load cannot restore model ID during pending unload.                                                                   | One completed engine load, its delayed caller and one pending unload. Current violation.                       |
| `publication::manager_loaded_ui_respects_pending_unload` | Late manager load cannot restore loaded UI event during pending unload.                                                            | Same two-actor boundary. Current violation.                                                                    |
| `publication::manager_unload_clears_published_state`     | Serial unload clears published model ID and emits unloaded UI.                                                                     | One publication followed by one unload.                                                                        |

Native outcomes range over success, crash, hang, remote error, and cancellation.
Each model has at most three threads including its driver. No permutation,
preemption, duration, or checkpoint pruning is enabled. The 10,000-branch guard
and 180-second command timeout fail verification if exceeded.

## Current findings

Persisted exact GPU selection can fall back to CPU and return success.

1. `TranscriptionManager::load_model_with_device` builds `DeviceSelector::Key`
   for a GPU selected in settings, at `managers/transcription.rs:611`.
2. `LoadSpec::pinned` recognizes only CLI `DeviceSelector::Index` at
   `engine_supervisor/supervisor.rs:120`.
3. The selected GPU worker crashes or hangs during load.
4. `Owner::start` allows recovery for this key and loads `wanted.cpu()`.
5. Load succeeds on CPU. The caller logs the changed backend but does not
   return a selection error. The strict explicit-device property fails.

This is a mismatch with the requested property. Current source deliberately
treats CLI indexes more strictly than persisted keys. Report that distinction;
keep the invariant until the user changes the promise. Product code stays
unchanged. No GPU-driver reproduction is claimed.

Manager publication has two additional failing properties. Engine load has
already returned, but its caller can pause before publishing `current_model_id`
and `loading_completed`. `begin_unload` can then queue unload, clear manager ID,
and emit `unloaded`. The delayed load caller resumes and restores ID or emits
`loading_completed` while supervisor remains unloaded. A manual unload command
calls `request_unload` directly in `commands/transcription.rs:53`, so caller
ordering permits the overlap. Supervisor publication itself stays protected.

The publication model extracts complete `begin_unload` and the load method's
actual tail from the model-ID assignment through `loading_completed`. It begins
after engine load returns and stops before notice cleanup. Engine unload is an
explicit boundary that marks unload pending and engine unloaded, matching the
separately checked supervisor promise. Native reaping, language-detector unload,
capability updates, and idle timer are excluded. Two Loom actors race publication
against unload. A serial reverse-order witness verifies ordinary unload behavior.

## Source correspondence

Last checked source commit is `af0f7dbe419feb81bd03fa301a70ad20474c827a`.
Supervisor source is `src-tauri/src/engine_supervisor/supervisor.rs`:

- `Owner::load`, `unload`, `start_worker`, `start`, `run`, and `Worker::drop`
  preserve replacement, worker destruction, and acknowledgement ordering.
- `EngineSupervisor::unload`, `Unloading::wait`, and `Owner::publish_loaded`
  preserve publication/queue/acknowledgement locks and counters.
- `Owner::transcribe`, `worker_lost`, `ensure_worker`, `effective_spec`,
  `cpu_only_worker`, and `LoadSpec` methods preserve retry and selection policy.
- `Shared` epoch and GPU status methods, `Failure::worker_lost`, error
  conversions, `Worker::load`, deadlines, and `lock` are extracted too.

Manager publication additionally extracts `begin_unload` and the publication
statements of `load_model_with_device` in `managers/transcription.rs`, preserving
separate ID and event operations. The build refuses moved extraction boundaries.

`loom/build.rs` parses these items from current source and compiles their bodies
unchanged. Missing or ambiguous extraction sites fail the build. Read
`managers/transcription.rs:601` and `engine_supervisor/protocol.rs:59` when
reviewing explicit-selection semantics; their caller/device types are manual
boundary counterparts, not extracted by this crate.

## Boundary assumptions

Worker creation and native responses use scripted fixtures. Creation records a
worker allocation; actual extracted destructor invokes simulated close/wait,
kill, and reap operations. Cooperative exit marks a child exited and reaped;
forced exit marks it dead at kill and reaped only at wait. These OS operations
are assumed successful. This proves Rust ordering, not operating-system kill
or wait reliability, actual process exit, GPU memory reclamation, or deadlines.

`Owner::call_work` is the response boundary here; use `engine-cancellation` to
check its real handshake. Its cancellation outcome advances the epoch while
retaining the worker, modeling a cancel after response collection but before
the final epoch check. Active streams are absent; their uncalled adapter methods
reject execution. Device probes and concurrent model-less workers are excluded.

The FIFO adapter uses Loom locks and a condition variable to represent ordered
send/receive and last-sender disconnection. Loom's provided channel does not
wake a receiver when its last sender drops. Review this adapter against queue
semantics when changing the owner loop; its behavior is an assumption, not a
proof of standard-library channels.

## Prove sensitivity and maintain

The twelve controls retain old worker through replacement, omit wait, send unload
acknowledgement early, remove publication guard, remove retry bound, permit
pinned load fallback, retry pinned runs, repeat GPU instead of CPU recovery,
classify cancellation as worker loss, admit a cancelled queued run, omit manager
ID clearing, and omit its unloaded UI event.
Each changes real production source in a temporary root and must produce its
named assertion failure. Compilation failure alone never counts as detection.

For a new failure, replay the reported order against production code and check
the boundary assumptions before calling it a product bug. Keep findings visible.
Advance sync commit only after reviewing source changes, confirming extraction
and assumptions still fit, and rerunning both commands. Use
`/maintain-formal-verification-skill` after unchecked covered changes.
