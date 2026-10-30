# Dictation telemetry validation

Validated on Linux on 2026-10-30 with a debug build, CPAL/PulseAudio through PipeWire, Earshot VAD, and an isolated application profile. No personal recordings or settings were used.

## Real dictation

[The example JSONL record](examples/dictation-events.jsonl) came from the running Handy application. A virtual microphone received repeatable, locally synthesized speech. Unix signals supplied activation and stop. Whisper Tiny English Q8 ran on CPU; output was clipboard-only, with postprocessing disabled.

The recorder delivered 114,859 valid raw frames and reported zero dropped frames. Clipboard output completed and contained text. Output gap was 235,578 microseconds, exactly `output.settled_us - stop_input.t_us`. SQLite held the same value, a null Capture gap, and an acknowledged export. The example contains only emitted metadata. Its times are observed values, not benchmark targets.

Capture gap is unavailable because handy-recorder 0.1.0 supplies no sample timestamps. First delivery at 134,260 microseconds does not establish when those samples were captured. Zero reported drops does not establish device-clock continuity. The source is virtual, so this run provides no hardware startup measurement.

A separate run cancelled during a configured 500 ms recording tail. Stop arrived at 1,600,952 microseconds, cancellation at 1,699,164, and capture drained at 1,732,458. Its finalized trace preserved 26,453 valid raw frames and first-VAD sample provenance, with `outcome="cancelled"` and unavailable Output gap. This exercised cancellation after ownership had moved to the stop task.

Completed JSONL records and SQLite statistics survived application restart. The isolated application was terminated after checks. Orderly shutdown drain was covered by Rust tests, not established by this application replay.

## Capture and overhead probe

Ten recordings alternated telemetry disabled/enabled, five each, for two seconds after first delivery. Input was a repeating 16 kHz mono synthetic speech file through an isolated PipeWire loopback. The 32-core host reported load averages 1.23, 1.67, and 1.74 immediately after the run. No build ran during capture.

The probe timed `CaptureSink::process_chunk`, including VAD, on the recorder delivery worker. Both modes used the same raw-copy and timing instrumentation; raw copying, WAV writes, and persistence were outside the timed interval. Enabled mode collected capture metadata and finalized a trace afterward. This does not measure whole-process overhead or the native real-time callback.

| Observation                              |           Disabled |            Enabled |
| ---------------------------------------- | -----------------: | -----------------: |
| Median of five per-run median sink times |         221.617 µs |         219.224 µs |
| Range of per-run medians                 | 216.374–227.308 µs | 218.198–223.436 µs |
| Largest observed block time              |         849.604 µs |       1,055.449 µs |
| Recorder-reported dropped frames         |                  0 |                  0 |

Speech phases differed between openings; runs were not CPU-pinned. The overlapping ranges and VAD/input variation do not isolate telemetry overhead. These debug measurements support no negligible-overhead or performance-improvement claim. Release profiling and hardware loopback remain necessary for those claims.

Each run delivered 32,939–33,280 valid frames. Sample-by-sample comparison matched every retained VAD block to an ordered raw-input block. Partial final blocks matched raw samples followed only by recorder padding. Every enabled first-VAD source range matched the corresponding raw prefix. Raw coverage counts excluded that padding; VAD output retained existing padded-block behavior.

Five enabled runs produced five JSONL lines totaling 8,104 bytes. The production optional-event budget is 192, the exporter wake queue capacity is one, and file rotation retains an 8 MiB active file plus three archives. Overflow, rotation, queued clear, truncated-tail recovery, restart deduplication, and export retry have deterministic tests. Test-only raw/VAD WAV files and timing reports are outside production telemetry.

## Automated checks and remaining coverage

- Rust library suite passed 361 tests, with 20 ignored. The real-recorder probe also passed when explicitly enabled.
- Ten Statistics Playwright tests passed against regenerated bindings, using locally cached Chromium. Screenshots cover the two tabs, unavailable samples, cancellations, configuration differences, and zoom.
- Statistics helper tests, translation checks, ESLint, frontend production build, native Linux build, Clippy, rustfmt, and changed-file Prettier checks passed. Clippy retained existing unrelated warnings.
- Focused tests cover sample-clock derivation, signed pre-roll, overlapping phases and sessions, actual HTTP attempts and failures, output settlement, migration, durability, cancellation evidence, and concurrent clear generations.

Native Windows reliable-paste settlement and macOS receipt/Apple Intelligence paths were source-reviewed but not compiled or exercised on their target systems. Hardware loopback was unavailable. The shipped recorder cannot produce a measured Capture gap on any platform until trustworthy sample timing is integrated.

## Repeating the capture probe

Provide a repeatable audio signal to a named virtual input, then run:

```sh
HANDY_TELEMETRY_SMOKE_DEVICE='your virtual input name' \
HANDY_TELEMETRY_SMOKE_DIR=/tmp/handy-telemetry-probe \
HANDY_TELEMETRY_SMOKE_RUNS=10 \
HANDY_TELEMETRY_SMOKE_MS=2000 \
cargo test --manifest-path src-tauri/Cargo.toml --lib \
  managers::stats::tests::real_recorder_telemetry_smoke -- --ignored --exact --nocapture
```

The source must contain nonzero signal. Inspect raw and VAD recordings before interpreting timings. Transcription text alone cannot prove capture coverage.
