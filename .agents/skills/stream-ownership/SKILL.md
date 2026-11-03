---
name: stream-ownership
description: Check streaming worker cleanup, frame routing, and cancelled finalize when changing StreamWorkerGuard, start_stream, StreamHandle, or Owner feed/finalize.
---

# Check stream ownership and cancellation

An exiting worker must preserve a newer worker's active flag. A queued frame must target its original engine stream. An observed cancellation must return `Cancelled`, since `Ok(None)` starts batch transcription.

Current code violates manager router frame ownership when an old sink resumes after recorder stop and delivery shutdown time out. Default command exits `1` for this finding. Guard ownership, engine frame IDs, and cancellation propagation pass their bounded checks. Preserve the failing router invariant.

Run from repository root:

```bash
python3 .agents/skills/stream-ownership/check.py
python3 .agents/skills/stream-ownership/check.py --mutations
```

Exit `0` means all seven checks passed. Exit `1` reports a current production property violation. Exit `2` means extraction, compilation, timeout, or another infrastructure check failed. `--mutations` continues mutation controls after a baseline property violation and retains exit `1`. Compilation failure never counts as a caught mutation.

Use the installed stable Rust toolchain and Python 3.10 or newer. Cargo locks Loom to `0.7.2` and the extraction dependencies in `loom/Cargo.lock`. No app build, native model, or worker process is required. First run needs Cargo dependencies cached or network access. Recorder lifecycle source must be cached as `handy-recorder-0.1.0` under Cargo's registry. If missing, run `cargo fetch --manifest-path src-tauri/Cargo.toml`. `HANDY_RECORDER_SOURCE` can select its source directory. Checker rejects a product lockfile with another recorder version.

## Review source changes before trusting a pass

1. Compare covered code against sync commit `af0f7dbe419feb81bd03fa301a70ad20474c827a`.
2. Inspect every write to `active_stream_worker`. The proof depends on `start_stream` reserving from zero and `StreamWorkerGuard::drop` alone retiring its token. Review any new writer before running the checker.
3. Run both commands. Current complete run prints six passing tests, one router failure, and seven `CAUGHT` lines. Baseline takes about 23 seconds on the development machine after compilation. Mutation mode runs baseline twice.
4. Inspect `target/stream-ownership/baseline.log` for a failure. Preserve the invariant and report the production function, assertion, and replayed action sequence. Model or extraction drift needs a separate explanation from a product finding.
5. Update fixtures and bounds when intended code changes invalidate assumptions. Keep production checks, locks, loads, stores, and CAS operations separate. Move the sync commit only after matching each extracted operation and boundary to current source.

For later maintenance, use `/maintain-formal-verification-skill`. No TLA+ model exists here, so `/statechart` is not needed.

## Coverage record

Source sync is `af0f7dbe419feb81bd03fa301a70ad20474c827a`. Covered files are unchanged between that commit and verification worktree baseline `61f27f4f`.

`loom/build.rs` parses production Rust with `syn` and emits complete method bodies with `quote`. It fails if a covered method disappears or becomes ambiguous. Methods are never rewritten into a second algorithm.

| Production source | Extracted code | Check |
| --- | --- | --- |
| `src-tauri/src/managers/transcription.rs` | `StreamWorkerGuard` and its `Drop` impl; `StreamRouter`, `Route`, `StreamCmd`; `StreamRouter::{new,open,take,clear,feed,is_open}`; `TranscriptionManager::start_stream` | Old cleanup races one real new-start attempt. Successful replacement keeps token 2 and `stream_active=true`. |
| `src-tauri/src/engine_supervisor/supervisor.rs` | `StreamHandle` fields and `StreamHandle::{feed,finalize}`; `Shared::{epoch,cancelled_since}`; `Owner::{feed,finalize,end_stream}` | Handle preserves frame ID and epoch. Owner rejects frames for absent, replaced, closed, cancelled, or workerless streams. Cancelled finalize reports cancellation even when stream is absent or replaced. |
| `src-tauri/src/managers/transcription.rs` | `TranscriptionManager::finalize_route` | Worker cancellation reply stays an error through manager handshake. |
| `src-tauri/src/audio_toolkit/audio/sink.rs` | `CaptureSink::emit` | Old recording's frame must not reach replacement route after stalled callback resumes. Current source fails. |
| `src-tauri/src/managers/audio.rs`, `src-tauri/src/actions.rs`, cached `handy-recorder-0.1.0/src/capture/engine.rs` | Source correspondence gates over lifecycle methods listed below. No lifecycle method is replaced by an invented implementation. | Callback-overlap witness requires shared router, bounded stop, possible delivery detach, and recorder reopen. |

## Bounds and boundaries

Loom exhausts the guard/start scenario with one old worker, one replacement attempt, and replacement activation. Initial old token is 1, next token is 2, and old route has already been taken for finalization. Actual `start_stream` spawns the replacement fixture asynchronously. All production loads, stores, CAS operations, and router locks retain their order. No preemption or execution-count cutoff applies. A 1,000-branch per-execution cap and runner's 240-second process timeout fail incomplete exploration rather than passing. Builder permits four threads, scenario uses at most three including main.

Replacement worker fixture performs the activation store used by `StreamSession::begin` in `src-tauri/src/managers/transcription/live.rs`, then signals fixture completion. It omits model loading, decoding, callbacks, and the replacement's own eventual guard drop. This isolates old cleanup while the new worker remains active. Fixture signal happens after activation and never orders activation against old cleanup. Manager's channel uses standard `mpsc`; channel contents do not affect this ownership check.

Engine owner runs on one production thread. Finite checks enumerate all 24 combinations of current stream absent/ID 1/ID 2, worker absent/present, epoch unchanged/changed, and stream open/closed. Each frame uses actual `StreamHandle::feed`, then dispatches the emitted command to actual `Owner::feed`. Owner dispatch is a fixture matching `Owner::run`'s `Command::Feed` branch. This checks engine stream IDs. Router separately runs the stalled-callback witness below. Router has no frame generation argument; a callback routes using the route held when its mutex is acquired.

Router witness bounds one old frame, one old worker, one replacement, and one fresh frame. Two Loom actors replay a delivery callback held before `CaptureSink::emit` while stop and close time out. After real router `take`, old worker guard drop, and real router `open`, old callback resumes through extracted `CaptureSink::emit` and router `feed`. Actual guard retirement leaves token zero before replacement. Router open represents successful next start, whose CAS protocol has separate exhaustive coverage. The test also requires delivery of a fresh replacement frame, so dropping every frame cannot satisfy the property.

Timeouts and microphone reopen are explicit environment boundaries. Source gates parse these exact production methods and require the relevant operations in order. `TranscribeAction::start` opens live stream before recorder start. `AudioRecordingManager::new` captures shared router. `collect_recording` calls recorder stop and marks failure on error. `stop_recording` clears recording state after collect. `start_microphone_stream` closes and reopens a failed microphone. `close_microphone` continues after recorder close. Cached `Engine::collect` returns on stop timeout without a delivery join, `Engine::stop` then sets its slot idle, and `Engine::shutdown` permits delivery timeout without a join. These gates establish source correspondence for the callback-overlap assumption; they do not exhaust recorder hardware, clocks, state, or all callers. Altering those operations requires reviewing the witness, even if router code is unchanged.

Finalize enumerates six already-cancelled states across absent/matching/replaced stream and absent/present worker. One additional state injects `Failure::Cancelled` from `call_work`. Another 24 states cover uncancelled success, remote failure, worker loss, and unexpected reply. Actual handle finalize additionally sends its ID and original epoch through Loom channels to actual owner finalize. Manager's standard-channel check injects the resulting worker cancellation reply.

`Owner::call_work` is an explicit scripted boundary. Worker outcomes, deadline arithmetic, pending audio size, callback text, reset I/O, and worker-loss I/O are fixtures. This skill checks cancellation before owner finalize and cancellation reported by `call_work`. Concurrent publication, kill, reply, and epoch races inside `call_work` belong to `engine-cancellation`. Cancellation after successful worker reply linearization, ID or epoch wraparound, supervisor panic, channel disconnects, shutdown, chunked decoding, and whole manager live-result processing remain outside this proof. `StreamHandle::Drop` is outside extraction; closed-state inputs model queued frames processed after handle closure.

## Mutation controls and findings

Controls modify production source only in temporary directories. Logs and mutated `.rs` files remain under `target/stream-ownership/`. Baseline is rerun against untouched checkout after controls.

| Control | Required assertion failure |
| --- | --- |
| `retire_token_before_flag` | Old guard releases token, replacement reserves token and activates, old guard clears replacement's active flag. |
| `remove_feed_id_fence` | Queued ID 1 frame dispatches while current engine stream is ID 2. |
| `remove_early_cancel` | Cancelled finalize with no stream returns `Ok(None)`. |
| `swallow_worker_cancel` | `Failure::Cancelled` becomes `Ok(None)` at owner boundary. |
| `swallow_manager_cancel` | Worker cancellation reply becomes `Ok(None)` in manager. |
| `swallow_handle_cancel` | Owner cancellation reply becomes `Ok(None)` in handle. |
| `drop_router_frames` | Removing real router send prevents stale delivery but also drops replacement's fresh frame. |

Current finding is a stale frame entering a replacement manager route. Source-linked replay follows:

1. Old delivery thread has an old recording's frame and pauses before `CaptureSink::emit` at `src-tauri/src/audio_toolkit/audio/sink.rs:176`.
2. Recorder stop times out at cached `handy-recorder-0.1.0/src/capture/engine.rs:448`. Sink remains on delivery thread. Shutdown can detach that thread at line 515 after its own timeout.
3. `AudioRecordingManager::collect_recording` at `src-tauri/src/managers/audio.rs:994` marks microphone failed. `stop_recording` sets app state idle. On-demand close can return while old delivery remains detached.
4. Empty stopped recording reaches `TranscribeAction::stop`'s `samples.is_empty()` branch and calls `cancel_stream` at `src-tauri/src/actions.rs:816`. Router takes old route and old worker's guard retires token 1.
5. Next start opens new route at `src-tauri/src/managers/transcription.rs:156`. New recording uses same shared router closure from `src-tauri/src/managers/audio.rs:571`.
6. Old sink resumes. Real `emit` invokes shared callback. Real `StreamRouter::feed` at `src-tauri/src/managers/transcription.rs:183` locks current route and sends old frame to new worker.

The witness prints replacement frames `[[1.0], [2.0]]` where `1.0` belongs to old recording and `2.0` belongs to replacement. The invariant rejects `1.0`. This is a replayed conditional counterexample under the source-supported stalled-callback boundary, not an exhaustive app lifecycle proof or a hardware reproduction. Product code stays untouched.

Initial complete guard run explored 1,029,456 executions, including 1,029,416 successful replacement starts. All seven executable mutation controls fail their named property assertions. Existing router bug remains a baseline failure before and after controls. These counts describe finite scenarios, not arbitrary app behavior.
