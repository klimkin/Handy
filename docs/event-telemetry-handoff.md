# Implement event traces and two gap metrics

Implement this change on `feat/local-latency-statistics`.
The starting commit is `6ed3b752cf5b897a648f96dc58898c1dba1b4d1e`.
This document is an implementation handoff, not a description of shipped behavior.

## Deliver the requested behavior

Capture lightweight events during each dictation. After the dictation finishes, normalize event times to the original activation event.
Save the trace in a file that a program outside Handy can read.
Use those events to derive the two statistics distributions:

- **Capture gap** measures how much audio immediately after activation was not captured.
- **Output gap** measures the time from the original stop event to completion of output.

Include LLM postprocessing in Output gap. Treat LLM postprocessing as a configuration difference, not a third metric.
Keep the existing box plots, shared zoom, language filter, row selection, and configuration comparison.

Here, "after exit" means after the dictation reaches its terminal outcome.
Do not wait for the Handy process to shut down before normalizing or saving traces.

Do not implement automatic always-on recording, a new pre-roll policy, or ASR optimizations in this change.
Those are separate ways to reduce a measured gap. Do not silently change microphone privacy behavior to improve a statistic.

## Define the metrics before adding events

### Measure Capture gap from sample coverage

Use the original activation timestamp as `t0`.
Find the capture timestamp of the first raw sample retained for this dictation, before VAD.
Derive the metric as follows:

```text
capture_gap = max(0, first_retained_raw_sample_time - t0)
```

If a retained pre-roll block starts before `t0` and covers `t0`, Capture gap is zero.
Require continuous sample coverage afterward. Record discontinuities separately, and do not present a recording with gaps as loss-free.

Do not use the first callback's execution time as the sample's capture time.
A block can arrive later while containing audio captured at activation.
Preserve the device timestamp or sample-position clock through resampling and delivery.
Use the recorder's valid sample count, not padded frames, to establish coverage.

The current `CaptureSink::process_chunk` records `Instant::now()` when the first delivered chunk reaches the sink.
That is delivery timing, not proof of when the device captured its first sample.
Inspect `handy-recorder`'s timing API. Extend the recorder integration or dependency if needed to carry sample timing.

Record the timing source and its uncertainty.
If the backend cannot supply trustworthy sample timing, return an unknown Capture gap.
An estimate may remain in the trace, but do not mix estimates with measured Capture gap samples without a visible distinction.
Never subtract a block duration from callback time and label the result an exact measurement.

Keep first-block delivery latency as a diagnostic event-derived value, not a Statistics tab.
Do not substitute VAD's first speech detection or the first ASR result for Capture gap.

Define `t0` at the earliest available input boundary. The current shortcut handler uses `Instant::now()` before settings lookup.
Preserve that timestamp through the coordinator, including deferred or queued actions.
Record whether the origin is a shortcut callback, a mapped native input timestamp, or an external command.
A zero gap is relative to that origin. It does not prove zero delay from the physical key switch or microphone acoustics.

### Measure Output gap through actual output completion

Use the original stop input timestamp as `t_stop`.
For Push-to-talk, use key release. For Toggle, use the next activation press.
Keep the original event timestamp through release grace, debounce, and coordinator queues.

```text
output_gap = output_done_time - t_stop
```

Include stop-event handling, the configured recording tail, ASR waits, WAV waits, history writes before output, LLM calls, and output delays.
Do not subtract LLM time.
Do not add parallel phase durations together. Derive the gap directly from the two timestamps.

Emit `output_done` when Handy finishes the selected output operation.
For the normal clipboard path, include the configured delays, clipboard restoration, and enabled auto-submit work.
For direct typing, wait until the typing operation returns.
For an external script, record process completion and identify that completion basis.
An external script that detaches other work cannot prove that the detached work finished.

The `reliable_paste` path currently returns after scheduling asynchronous completion.
Do not treat that return as `output_done`. Carry a completion notification from transaction settlement.
Keep timeout, ownership-loss, and failure outcomes distinct from confirmed completion.
Clipboard-read evidence is not proof that the target application rendered the text.

Exclude telemetry persistence, UI cleanup after output, and resource cleanup from Output gap.
Keep terminal failure time as an event. Do not invent a successful `output_done` event for an empty result, cancellation, or failed output.
If the configured result is clipboard-only, identify that output kind and use the completed clipboard write.
If no output operation is requested, report Output gap as unavailable.

## Capture events without blocking audio

Create one trace with a unique `run_id` at the activation boundary.
Pass its identity through the recording, ASR worker, postprocessing, and output task.
Keep each run's state with that run. Do not introduce global "last recording" timestamps.
An earlier paste can finish after a later recording has started.

Use a typed event enum and a monotonic clock.
Record timestamps at each event's source, not when the telemetry worker receives them.
Keep wall-clock UTC once per run for external correlation. Do not use wall-clock time for interval arithmetic.

Capture these events where the work exists:

| Area     | Events or evidence                                                                                        |
| -------- | --------------------------------------------------------------------------------------------------------- |
| Input    | Original activation and stop events, action mode, cancellation                                            |
| Startup  | Recording request, model-load start and end, VAD preparation start and end, microphone-open start and end |
| Capture  | First raw block observed, its retained sample coverage, discontinuity counters, capture stop              |
| VAD      | First accepted output and the original sample range it contains                                           |
| ASR      | Live or batch work start and end, live finalization, fallback, actual model identity                      |
| Files    | WAV write start and end, WAV verification, history write start and end                                    |
| LLM      | Actual attempt start and end, fallback, success, failure, or skip reason                                  |
| Output   | Output start, paste injection where applicable, actual completion or failure, completion basis            |
| Terminal | Success, empty result, cancellation, capture failure, processing failure, or output failure               |

Use phase identifiers to pair starts and ends when work repeats.
Record an already-loaded model as configuration or state. Do not fabricate a zero-duration load.
For Live ASR, retain the actual start during recording and its later finalization.

Keep event capture bounded and allocation-free on the real-time audio callback.
Do not serialize JSON, write files, take a contended telemetry lock, or wait for a worker there.
Reuse existing worker boundaries where possible.
Do not emit an event per audio sample.
Preserve first-block timing and aggregate continuity counters instead of keeping an unbounded per-block log.

Reserve storage for metric-critical timestamps and terminal state.
If optional diagnostic events overflow, increment `dropped_events` and mark the trace incomplete.
Never silently compute a valid-looking gap from missing critical evidence.
Any background queue must be bounded and must not block capture.

After the run ends, derive metrics and serialize the trace outside the capture and output paths.
Subtract `t0` using signed durations. Pre-roll sample timestamps can be negative.
Sort events by normalized time and keep a sequence number as the tie-breaker.
Do not clamp pre-activation events to zero.
Do not persist Rust `Instant` values or arbitrary process uptime as the public time format.

## Write an externally readable JSONL file

Use UTF-8 JSON Lines. Append one compact JSON object per finalized run.
This keeps completed lines readable while another run is appended.
Do not rewrite one ever-growing JSON array after every dictation.

Resolve the directory with `crate::portable::app_data_dir`.
Use `telemetry\dictation-events.jsonl` beneath that directory.
For a normal Windows installation, the expected path is:

```text
%APPDATA%\com.pais.handy\telemetry\dictation-events.jsonl
```

For portable mode, use `Data\telemetry\dictation-events.jsonl` beside the executable.
Expose the resolved path in Statistics with a copy-path or open-folder control.
Do not require a developer to query Tauri internals or export the file manually.

Version the record format independently of the application version.
Use signed integer microseconds for normalized timestamps and derived gaps.
Use milliseconds only for UI display.
Include the run ID, application version, UTC start time, timing source, configuration, terminal outcome, events, metric validity, and drop counters.

The following abbreviated record illustrates the format.
The times are invented, not measurements.
Write the actual record on one line:

```json
{
  "schema_version": 1,
  "run_id": "example-run",
  "app_version": "0.9.8",
  "started_at_utc": "2026-10-30T06:00:00Z",
  "origin": "shortcut_callback",
  "config": {
    "model": "example-asr",
    "microphone_mode": "always_on",
    "post_process_requested": true,
    "post_process_provider": "example-provider",
    "post_process_model": "example-llm"
  },
  "outcome": "output_completed",
  "capture": {
    "status": "measured",
    "timing_source": "device_timestamp",
    "timing_uncertainty_us": 1000,
    "first_retained_sample_us": -8000,
    "discontinuities": 0
  },
  "post_process": {
    "attempted": true,
    "outcome": "success"
  },
  "output": {
    "kind": "clipboard_paste",
    "completion_basis": "synchronous_output_return"
  },
  "events": [
    { "seq": 0, "name": "activation", "t_us": 0 },
    { "seq": 1, "name": "recording_requested", "t_us": 500 },
    { "seq": 2, "name": "first_raw_block_observed", "t_us": 16000 },
    { "seq": 3, "name": "asr_started", "t_us": 40000 },
    { "seq": 4, "name": "stop_input", "t_us": 1200000 },
    { "seq": 5, "name": "capture_stopped", "t_us": 1220000 },
    { "seq": 6, "name": "asr_completed", "t_us": 1400000 },
    { "seq": 7, "name": "post_process_started", "t_us": 1410000 },
    { "seq": 8, "name": "post_process_completed", "t_us": 1710000 },
    { "seq": 9, "name": "output_started", "t_us": 1720000 },
    { "seq": 10, "name": "output_done", "t_us": 1800000 }
  ],
  "metrics": {
    "capture_gap_us": 0,
    "output_gap_us": 600000
  },
  "events_complete": true,
  "dropped_events": 0
}
```

The example's first block arrives 16 ms after activation, but covers activation through retained pre-roll.
Capture gap is therefore zero. Output gap is 600 ms, including the 300 ms LLM attempt.
For missing evidence, use `null` and a reason. Do not use zero as a placeholder.

Use one file writer. Flush completed records and use a durable write outside the real-time path.
Drain the writer on orderly shutdown.
Recover from a truncated final line without discarding earlier records.
A hard kill can lose an active or not-yet-durable trace. Do not promise crash safety for records that have not reached durable storage.
Preserve the existing durability of completed SQLite statistics.

If SQLite remains the internal statistics store, derive its values and the JSONL record from the same finalized trace.
Define how failed writes retry and how restart repairs an incomplete export.
Use `run_id` to prevent duplicate statistics. Do not claim an atomic transaction across SQLite and a separate file.
Do not add a second, independent timing implementation for the UI.

Bound file growth with documented size-based rotation.
Keep active and rotated filenames discoverable for external readers.
Serialize rotation, appends, and Clear statistics through the same writer.
Clearing statistics must not leave the same traces behind in the external files or let queued old writes restore them.
Do not add a network collector.

Allowlist trace fields. Exclude audio samples, transcript text, prompts, credentials, endpoint URLs, and local recording paths.
Keep device identity to a non-sensitive identifier or category.
The event file is metadata, not a raw audio archive.

## Replace the Statistics metrics and separate LLM configurations

Replace the Entry, Exit, and Post-processing tabs with Capture gap and Output gap.
Do not keep a third LLM duration tab in this scope. LLM events remain available in the file.
Update hints, accessible summaries, sample counts, sorting, and translations.

Group LLM-enabled and LLM-disabled dictations separately.
Record whether the chosen action requests postprocessing.
When requested, include provider, LLM model, and prompt identity in the configuration.
Use a prompt ID with a revision or a content fingerprint, not prompt text.
Do not distinguish disabled configurations by irrelevant saved provider settings.

Record actual attempts and skip or failure reasons separately from settings.
Do not infer an attempt from the persisted `post_process_enabled` flag.
The existing action flag and runtime provider checks determine whether a call actually happens.
A failed attempt still contributes its wait to a successful Output gap if Handy then outputs the original text.

Retain model, language, and relevant configuration comparisons.
Capture gap must come from the capture path, even when groups are displayed by ASR model.
Record microphone mode, VAD backend, and relevant buffering settings so capture comparisons have useful context.
Include a cold or warm device state when the recorder can determine it.

Retain traces for every activation and terminal outcome.
Include valid Capture gap samples even when later ASR or output fails.
Do not apply the current "empty result without detected speech" filter to capture diagnostics.
Keep cancellations visible in traces and separate from failure counts.
Exclude deliberate cancellations from the default distributions.
Show missing or invalid sample counts rather than inventing measurements.
Include Output gap samples only for completed output with a valid stop timestamp.

## Preserve old data without relabeling it

Do not rename old `entry_ms` samples to Capture gap.
Those samples measure first-block delivery.
Do not rename `exit_exclusive_ms` samples to Output gap.
Those samples exclude LLM time.

The old raw `exit_ms` column includes LLM time, but its endpoint follows existing completion behavior.
In particular, asynchronous reliable paste can finish later.
Do not merge old samples into the new distributions without proof of equivalent timestamp and completion semantics.
The safe default is to preserve old records and show that the new metrics require new dictations.

Append migrations instead of modifying shipped migrations.
Give the new metrics explicit names such as `capture_gap_us` and `output_gap_us`.
Preserve existing recordings, history, and raw statistics.
Migrate affected callers and generated types together.

## Change the existing owners

Use these files as the implementation entry points:

| File                                                      | Work                                                                             |
| --------------------------------------------------------- | -------------------------------------------------------------------------------- |
| `src-tauri\src\shortcut\handler.rs`                       | Preserve original input timestamps and create trace identity early               |
| `src-tauri\src\transcription_coordinator.rs`              | Carry trace identity and original times through mode handling and queues         |
| `src-tauri\src\managers\audio.rs`                         | Carry raw sample timing and continuity evidence through recording start and stop |
| `src-tauri\src\audio_toolkit\audio\sink.rs`               | Keep pre-VAD evidence separate from delivery time and VAD output                 |
| `src-tauri\src\actions.rs`                                | Attach events to each run and finalize on all terminal paths                     |
| `src-tauri\src\managers\transcription.rs`                 | Record actual model work, including overlapping Live work                        |
| `src-tauri\src\clipboard.rs` and `src-tauri\src\paste_tx` | Supply actual output completion, including asynchronous settlement               |
| `src-tauri\src\managers\stats.rs`                         | Own trace persistence, migration, derivation, aggregation, and clear behavior    |
| `src-tauri\src\commands\stats.rs`                         | Expose the two metrics and the resolved external trace location                  |
| `src\components\settings\statistics`                      | Replace metric selection and preserve shared chart interactions                  |
| `src\bindings.ts`                                         | Regenerate through tauri-specta after Rust API changes                           |
| `src\i18n\locales`                                        | Add localized metric definitions and external-file controls                      |

Keep metric derivation pure and test it with synthetic event sequences.
Choose event timestamps once. Reuse those events for both Statistics and external analysis.

## Verify the exact requirements

Add focused coverage to the existing stats, sink, action, helper, and browser tests.
Add output-transaction tests where completion changes.

- A block delivered after activation contains samples that cover activation. Capture gap is zero and delivery latency is positive.
- A recording starts after activation. Capture gap equals the missing initial interval.
- Sample timestamps are unavailable or discontinuous. The trace reports the reason and does not fabricate zero.
- Negative pre-roll timestamps survive normalization. Wall-clock changes do not change either metric.
- ASR model load overlaps capture. Capture gap does not use model-ready or ASR-start timestamps.
- Live ASR and WAV writes overlap other phases. Derivation uses event endpoints, not summed phase durations.
- LLM work delays output. Output gap includes the full delay and the row belongs to the LLM-enabled configuration.
- LLM work is skipped or fails. The trace records the execution outcome without pretending no time elapsed.
- Reliable paste returns before settlement. Output gap ends at the correct terminal notification, not at scheduling.
- Output fails, produces no text, or is cancelled. Capture evidence remains available and Output gap is not invented.
- Two sessions overlap through a slow paste. Each trace keeps its own activation, stop, configuration, and outcome.
- A trace exceeds its diagnostic-event budget. Drop counters and metric validity remain honest.
- A process restarts after completed writes. External records remain readable and statistics do not duplicate runs.
- The last JSONL write is truncated. Earlier records still parse and recovery permits later appends.
- Clear statistics runs with queued writes. Neither active nor rotated files restore cleared records.
- The UI shows exactly two metric tabs, separates LLM settings, and retains zoom, selection, and configuration comparison.

Use the existing project commands:

```powershell
bun run test:playwright statistics.spec.ts --reporter=line --workers=1
bun src\components\settings\statistics\statsView.test.ts
bun run lint
bun run build
cargo test --manifest-path src-tauri\Cargo.toml --lib -- managers::stats audio_toolkit::audio::sink actions::tests paste_tx
cargo clippy --manifest-path src-tauri\Cargo.toml --lib --tests
cargo fmt --manifest-path src-tauri\Cargo.toml -- --check
git --no-pager diff --check
```

Run native commands in the MSVC and Vulkan environment described in `BUILD.md`.
Run Prettier on changed frontend and documentation files.
Do not normalize unrelated checkout files.

Validate a real capture with a timed, repeatable audio input.
Use a virtual input to test the software path and a hardware loopback to test device startup.
Compare raw input coverage with the audio retained after VAD.
Do not use transcription text as proof that the microphone captured the beginning.
Keep diagnostic raw recordings test-only and outside the production event file.

Measure telemetry overhead with tracing enabled and disabled under the same capture setup.
Check capture continuity, callback work, queue bounds, and file growth.
Report the conditions and results. Do not claim negligible overhead without measuring it.

Deliver the two-metric UI, the external trace file and its documented schema, migration tests, and an example trace from a real run.
State the timing accuracy and crash-loss window that the implementation actually supports.
