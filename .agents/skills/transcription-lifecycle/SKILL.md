---
name: transcription-lifecycle
description: Run bounded formal checks across Handy cancellation, worker lifetime, load publication, recovery, stream ownership, coordinator, readiness, output commitment, and diagnostics capture. Use after changes spanning these lifecycle boundaries.
---

# Check transcription lifecycle

Run all ten requested priorities from repository root:

```sh
python3 scripts/check-formal.py
python3 scripts/check-formal.py --mutations
```

Python 3.10+ and stable Rust are required. Loom 0.7.2 and each crate's dependencies
are locked. Source extraction uses the current checkout; no product code is
rewritten. Initial dependency download needs network or an existing Cargo cache.
The stream checker additionally reads pinned handy-recorder 0.1.0 source from
Cargo registry; its skill documents fetching or selecting that source.

The runner executes every suite even after property failures. Exit 0 means all
selected bounded checks passed, 1 means one or more completed checks found a
property violation, and 2 means verification did not complete. A known finding
remains red. Assertions weakened to match current code provide no valid verdict.

Logs and machine-readable summary are saved under `target/formal-lifecycle/`.
Use `--suite engine-lifecycle` or another suite name from the table for focused
work. When inspecting a failure, read that suite's skill before proposing a fix.

## Coverage and source correspondence

Source sync commit is `af0f7dbe419feb81bd03fa301a70ad20474c827a`.
Engine, stream, coordinator, readiness, and output source still match that
baseline. Diagnostics resource accounting changed on the fix branch; its linked
skill records source correspondence at
`427bc79b5a94140ff40e0c4d6540f4810132a2ad` and the remaining span finding.
Each linked skill records extracted items, simulated boundaries, finite bounds, and mutation
controls. A pass covers those conditions, not the whole app.

| Priority | Promise                                                                                   | Source                                                               | Suite                                                      |
| -------- | ----------------------------------------------------------------------------------------- | -------------------------------------------------------------------- | ---------------------------------------------------------- |
| 1        | Cancel rejects old work or kills its worker; late cancelled replies fail.                 | `engine_supervisor/supervisor.rs`                                    | [engine-cancellation](../engine-cancellation/SKILL.md)     |
| 2        | Replacement starts after prior exit/reaping; unload acknowledgement follows reaping.      | `engine_supervisor/supervisor.rs`                                    | [engine-lifecycle](../engine-lifecycle/SKILL.md)           |
| 3        | Pending unload prevents stale supervisor and manager/UI model publication.                | `engine_supervisor/supervisor.rs`                                    | [engine-lifecycle](../engine-lifecycle/SKILL.md)           |
| 4        | Retries stay bounded; cancel skips recovery; explicit GPU choice is respected.            | `engine_supervisor/supervisor.rs`, `managers/transcription.rs`       | [engine-lifecycle](../engine-lifecycle/SKILL.md)           |
| 5        | Old cleanup preserves new state; stale frames stay out of replacement streams.            | `managers/transcription.rs`, audio manager/sink, recorder dependency | [stream-ownership](../stream-ownership/SKILL.md)           |
| 6        | Cancelled finalize returns cancellation instead of batch fallback.                        | `managers/transcription.rs`, `engine_supervisor/supervisor.rs`       | [stream-ownership](../stream-ownership/SKILL.md)           |
| 7        | Busy toggle parity, pending-binding ownership, and cancellation preserve intended starts. | `transcription_coordinator.rs`                                       | [recording-lifecycle](../recording-lifecycle/SKILL.md)     |
| 8        | Failed start rolls back; ended-session readiness cannot affect newer UI/chime/mute.       | coordinator, `managers/audio.rs`, `actions.rs`                       | [recording-lifecycle](../recording-lifecycle/SKILL.md)     |
| 9        | Cancellation observed by final guard prevents paste commitment.                           | `actions.rs`, audio cancellation generation                          | [recording-lifecycle](../recording-lifecycle/SKILL.md)     |
| 10       | Stop drains admitted events; captures reject stale spans and respect limits.              | `diagnostics.rs`                                                     | [diagnostics-lifecycle](../diagnostics-lifecycle/SKILL.md) |

Paths in this table are relative to `src-tauri/src/`.

## Findings to preserve

- Late manager load completion can restore model ID or loaded UI event after
  unload, while supervisor correctly remains unloaded.

- Settings-selected GPU keys may recover on CPU after worker failure; CLI
  device indexes remain pinned. This violates strict explicit-device promise.
- A detached old recorder callback can feed a replacement manager route after
  stop/close timeouts. The source-linked replay assumes a stalled delivery
  callback; it does not reproduce hardware behavior.
- Readiness generation validation and UI/chime/mute effects are separate steps.
  A newer recording can begin between them, allowing stale effects.
- `Span::end` has no capture identity, so old spans can end in a later capture.

Diagnostics resource checks now pass with total-output reservations and no
metadata insertion for rejected events. The remaining entries are verification
findings. Read each suite's replay and assumptions to distinguish source behavior from a harness error.

## Output commitment

Priority 9 commits at entry to `utils::paste` in the paste thread. If its final
cancellation guard observes a changed generation, that call must not occur.
Cancellation arriving after the guard returned false is outside this promise,
including later clipboard writes, key injection, or script effects inside paste.
No claim is made that OS text insertion is revocable.

## Maintain checks

For a covered change, compare source with the recorded sync commit and inspect
all writes to guarded state, including new or moved callers. Match extraction,
adapters, and atomicity to the new source before interpreting results. Run both
commands. Mutations must fail on named assertions; compilation failures do not
prove sensitivity. Advance sync commit only after confirming correspondence.

Use `/maintain-formal-verification-skill` when covered changes bypassed these
checks. Native GPU execution, OS process/clipboard effects, arbitrary operation
counts, epoch wraparound, and hardware recovery remain outside bounded models.
