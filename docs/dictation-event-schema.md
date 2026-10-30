# Dictation event schema, version 1

Handy appends one UTF-8 JSON object and newline per finalized dictation. Statistics and external records derive from the same immutable trace. Schema version changes independently of the application version.

## File location and retention

**Statistics** shows the resolved file path and provides **Copy path**. The directory uses `portable::app_data_dir`.

| Installation    | Active file                                                               |
| --------------- | ------------------------------------------------------------------------- |
| Windows         | `%APPDATA%\com.pais.handy\telemetry\dictation-events.jsonl`               |
| Portable        | `Data\telemetry\dictation-events.jsonl` beside the executable             |
| Other platforms | `telemetry/dictation-events.jsonl` beneath the application data directory |

External readers can discover all retained files with `dictation-events*.jsonl`. The writer rotates before an append would exceed 8 MiB. It retains three archives and the active file, approximately 32 MiB total. Archive names use a durable increasing sequence, `dictation-events-00000000000000000001.jsonl`. Wall-clock changes do not reorder archives.

Each record has at most 192 optional events plus reserved activation, stop, capture, output, and terminal events. Configuration has a fixed field allowlist. Identifier values and language identifiers have a 128-byte limit. Longer identifiers retain a prefix and SHA-256 fingerprint, with `metadata_truncated=true`. These bounds keep serialized records below 64 KiB. Audio samples, transcript text, prompts, credentials, endpoint URLs, microphone names, and recording paths are excluded.

## Record fields

| Field                | Meaning                                                                                     |
| -------------------- | ------------------------------------------------------------------------------------------- |
| `schema_version`     | Integer `1`                                                                                 |
| `run_id`             | UUID v4 created at the original input boundary and adopted when input becomes an activation |
| `app_version`        | Handy package version                                                                       |
| `started_at_utc`     | UTC correlation timestamp recorded once at activation input                                 |
| `origin`             | `shortcut_callback` or `external_command`                                                   |
| `config`             | Allowlisted configuration identifiers and settings                                          |
| `language`           | Resolved language, selected language, or `null`                                             |
| `outcome`            | Terminal outcome listed below                                                               |
| `capture`            | Raw delivery counts, sample-clock validity, and first VAD output lineage                    |
| `post_process`       | Requested state, actual attempts, skip reason, and execution outcome                        |
| `output`             | Selected output kind, actual settlement time and result, or `null`                          |
| `events`             | Events sorted by signed `t_us`, then `seq`                                                  |
| `metrics`            | Capture gap and Output gap values, validity reasons, and uncertainty                        |
| `events_complete`    | False when optional events dropped or a reported phase remains unfinished                   |
| `dropped_events`     | Optional events lost to contention or the event budget                                      |
| `metadata_truncated` | An unknown metadata key was excluded or an identifier exceeded its limit                    |

`config.post_process` reflects the action's request, independently of the saved enable flag. Requested postprocessing includes provider, model, prompt ID, and SHA-256 of the unexpanded prompt template. Disabled configurations omit those irrelevant saved values. LLM and output settings are snapshotted when selected for execution, then the operation uses that snapshot. Actual model identity replaces the requested model in `config.model` when known. `requested_model` retains the initial request. `shortcut_activation` records the input's actual mode, including external commands' Toggle mode.

Resource-state events distinguish ready, cold, and already-loading model, VAD, or microphone state. An already-ready resource has no fabricated load span.

## Time and metric validity

All public event times and gaps use signed integer microseconds relative to the original activation monotonic timestamp. Activation is zero. Pre-activation evidence remains negative. UTC never participates in duration arithmetic. Origin is the earliest timestamp Handy observes, not the physical key switch or microphone acoustics.

Each metric has `us`, `reason`, and `uncertainty_us`. Valid values have a numeric `us` and `reason=null`. Unavailable values have `us=null`, an explicit reason, and no invented zero.

`metrics.capture_gap.us` equals `max(0, first_retained_raw_sample_time - activation)`. Validity requires trustworthy sample-clock timestamps, valid-frame coverage that reaches activation, and verified continuous capture. Delivery time, VAD onset, model readiness, and ASR start do not establish Capture gap.

The installed `handy-recorder` 0.1.0 exposes no capture timestamps or sample-clock mapping. Production records therefore report `capture.timing_source="unavailable"`, `capture.reason="sample_timestamps_unavailable"`, and unavailable Capture gap. `capture.discontinuities=null` means continuity is unknown. Recorder-reported dropped frames remain separate evidence. Detected drops invalidate measured coverage as `capture_discontinuous`. Synthetic tests exercise validated sample-clock evidence without claiming current device support.

`capture.first_delivery_us` measures delivery latency only. `capture.valid_frames` counts valid raw 16 kHz mono frames delivered to the sink, excluding final padding. `first_vad_output_us` timestamps the first accepted output. `vad_source_range` is `[first_raw_frame_ordinal, valid_frame_count]` for that output, before VAD. Smoothed VAD propagates buffered frame ordinals. Detectors without proven lineage return `null`. These ordinals do not establish device capture time.

`metrics.output_gap.us` equals `output.settled_us - original_stop_input_time`. It includes release grace, configured capture tail, ASR, concurrent-file waits, history writes before output, all LLM attempts, and output delays. It uses endpoints and never sums overlapping phase durations or subtracts LLM time. A valid sample requires completed output and an original stop timestamp.

Unavailable Capture gap reasons include `sample_timestamps_unavailable`, `capture_continuity_unknown`, `capture_discontinuous`, and `coverage_does_not_reach_activation`. Output reasons include `stop_input_unavailable`, `stop_before_activation`, `output_before_stop`, and `output_not_completed`.

## Events and terminal outcomes

Every event has `seq`, `t_us`, and `name`. Sequence identifiers are unsigned 64-bit integers; readers must preserve integer precision when comparing them. Events with repeated work use `phase_id`. `phase_started` also has `kind`; `phase_completed` has `outcome`.

Event names include `activation`, `stop_input`, `recording_requested`, `capture_stopped`, `first_raw_block_observed`, `first_vad_output`, `resource_state`, `phase_started`, `phase_completed`, `asr_fallback`, `post_process_fallback`, `post_process_skipped`, `paste_injected`, `output_started`, `output_done`, `cancel_requested`, and `terminal`.

Phase kinds are `model_load`, `vad_prepare`, `microphone_open`, `live_asr`, `batch_asr`, `live_finalize`, `wav_write`, `wav_verify`, `history_write`, `post_process`, `post_process_attempt`, and `output`. Actual Live ASR start occurs during recording. Shared work still in flight when a run terminates leaves an incomplete phase instead of a fabricated completion.

Terminal outcomes are `output_completed`, `output_unconfirmed`, `output_failed`, `no_output_requested`, `empty`, `cancelled`, `capture_failed`, `processing_failed`, and `unfinished_owner`. The final outcome belongs to one run owner. An earlier paste can settle after another run starts. A forgotten terminal path records `unfinished_owner` during ordinary unwinding. Process abort cannot run that fallback.

`post_process_attempt` spans each actual HTTP request through response parsing. Reasoning-parameter retries and structured-output fallbacks have separate phase IDs. Dropped or cancelled requests close with an explicit outcome.

`post_process.attempted` is true after an observed actual attempt. It is false after a known skip or disabled action. Missing execution evidence remains `null`. A failed attempt followed by original-text output has `failed_original_text_fallback`; its entire wait remains in Output gap. Skip reasons distinguish empty input, missing provider, missing model, missing prompt, and no request.

## Output settlement

Output kinds are `clipboard_paste`, `clipboard_only`, `direct_typing`, `external_script`, and `none`. `output.result` has a `status` and, where applicable, `reason`. Confirmed output also has an explicit `completion_basis`.

| Completion basis              | Endpoint                                                                                                      |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------- |
| `clipboard_write`             | Clipboard-only write returns                                                                                  |
| `synchronous_output_return`   | Legacy paste delays, clipboard restoration, enabled auto-submit, and final selected clipboard handling return |
| `direct_typing_return`        | Typing and selected follow-up work return                                                                     |
| `script_process_exit`         | External script process exits successfully                                                                    |
| `receipt_transaction_settled` | Reliable transaction finishes receipt-supported submit and guarded clipboard settlement                       |

Reliable paste returns a pending receipt at scheduling. Its output worker waits after releasing the paste lock. Both normal settlement and supersession resolve the same completion sender once. Timeout, ownership loss, and supersession remain unconfirmed. Injection, typing, script, restoration, auto-submit, and disconnected completion failures remain distinct. Clipboard receipt proves a read, not target rendering. Script process exit makes no claim about detached child work.

No output operation produces `not_requested`, with unavailable Output gap. Empty results, cancellation, and failures have no successful `output_done` event. Cancellation after side effects does not erase actual settlement evidence.

## Persistence and clear

Finalization freezes the trace before persistence. SQLite migration 3 preserves both shipped migrations and legacy rows. New rows have unique `run_id`, explicit nullable `capture_gap_us` and `output_gap_us`, configuration, validity reasons, and canonical JSON. The database uses WAL and `synchronous=FULL`. Failed SQL commits retry the same frozen record twice, 50 ms apart, then report degraded persistence. A failed commit can lose that trace.

One file worker exports committed pending rows. Its wake channel is bounded and carries no trace payload. Wake coalescing loses no committed rows. The writer flushes and synchronizes each completed line before acknowledging export in SQLite. Exported rows retain statistics and discard their JSON payload. Pending payloads remain durable in SQLite until export succeeds. Prolonged file failure can grow that pending disk usage; the implementation does not silently expire pending runs. Errors and pending counts appear in Statistics. Retry delay grows to 60 seconds.

Startup, or recovery after file failure, scans retained files once. It truncates an unterminated active-file suffix and retains earlier complete lines. It acknowledges pending run IDs already present, covering a crash between file synchronization and SQLite acknowledgment. Interior corruption produces an error. Completed acknowledged rows from pruned archives do not re-export. SQLite and JSONL are separate stores, without a cross-file atomic transaction.

Clear executes through the same writer as append and rotation. Its SQLite transaction increments the generation, deletes legacy and new statistics, and sets a pending-clear marker. The generation publishes immediately after commit. Input stamps capture the generation before queueing, so old active or queued runs cannot restore cleared rows. The writer removes all recognized active and archive files before acknowledging clear. Deletion failure retains the marker and blocks further export. Restart completes deletion before appending new-generation records.

Orderly quit keeps the main event loop available while a background worker cancels active work, waits up to 12 seconds for owned traces, then drains committed exports. Unsettled active traces or disk errors are logged. Committed pending rows retry after restart. A hard kill can lose active or uncommitted traces and filesystem writes not yet durable.

Legacy Entry, exclusive Exit, and postprocessing statistics remain in their original table. New distributions exclude those values because their timestamp semantics differ. Cancellations retain trace evidence and are excluded from default distributions and missing counts. Empty results and later failures retain Capture gap evidence.

## Validation commands

The ignored `regenerate_bindings` test regenerates TypeScript through the same tauri-specta builder used at startup.

```sh
cargo test --manifest-path src-tauri/Cargo.toml --lib regenerate_bindings -- --ignored --exact
cargo test --manifest-path src-tauri/Cargo.toml --lib
```

The ignored `managers::stats::tests::real_recorder_telemetry_smoke` test opens the actual recorder and production CaptureSink. `HANDY_TELEMETRY_SMOKE_DIR` selects an isolated output directory. `HANDY_TELEMETRY_SMOKE_RUNS` defaults to ten alternating runs, and `HANDY_TELEMETRY_SMOKE_MS` defaults to 2000 ms per run. A repeatable input must already be active. Set `HANDY_TELEMETRY_SMOKE_DEVICE` to its input-device name; otherwise the test opens the system default. Linux CPAL can select PulseAudio, which does not honor the ALSA `PIPEWIRE_NODE` override.

The test writes test-only raw and VAD WAV files, per-block sink durations, valid and retained counts, drops, and real finalized capture traces. This validates recording, VAD, event normalization, SQLite, and JSONL. It does not run ASR or native output and does not supply the missing device clock. Test recordings and timing reports stay outside the production JSONL file. The enabled and disabled runs measure sink telemetry work; the recorder's real-time callback is unchanged.

See [validation results](dictation-event-validation.md) and the [real dictation example](examples/dictation-events.jsonl).
