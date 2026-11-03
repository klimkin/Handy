---
name: recording-lifecycle
description: Check busy transcription input parity, failed recording starts, delayed microphone readiness, and cancellation before paste when changing transcription_coordinator.rs, actions.rs, or audio generation and mute handling.
---

# Recording lifecycle

Use this skill when changing busy-pipeline input handling, coordinator start
rollback, first-sample readiness cues, or the paste thread's cancellation guard.
These checks execute extracted production Rust. Product code remains unchanged.

## Run and interpret

From repository root:

```bash
python3 .agents/skills/recording-lifecycle/check.py
python3 .agents/skills/recording-lifecycle/check.py --mutations
```

Requires stable Rust, Python 3.10+, and Cargo dependencies from committed
`loom/Cargo.lock`. Loom is pinned to 0.7.2. First run downloads dependencies.
The second command runs all checks, nine scratch source mutations, and all
checks again, including when ordinary properties fail. Scratch mutations never
change the checkout. Logs and mutated sources go under
`target/recording-lifecycle/`.

Exit status 0 means all properties pass. Status 1 means at least one completed
property check found a violation. Status 2 means extraction, compilation, timeout,
missing tests, or mutation controls prevented completion. Inspect logs before
classifying a status 2 as a product defect.

Current result is **9 passed, 3 failed**, with all nine mutation controls caught.
Failures remain ordinary failing assertions. They are current readiness races,
not expected-success tests. Healthy checks and mutation controls still run.

## Properties and bounds

| Priority | Promise and user harm                                                                                                    | Executed scope                                                                                                                                                                   |
| -------- | ------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 7        | Busy toggle edges retain parity, preventing a deferred recording nobody will stop.                                       | Every press/release word of lengths 0 through 8 for one binding. Both external input and keyboard input 31ms apart, 1,022 words.                                                 |
| 7        | Another binding cannot replace the remembered press.                                                                     | Remember binding `a`, then every binding `b` press/release word of lengths 0 through 8, 511 words; drain must start `a`.                                                         |
| 7        | Cancel removes any pending start, preventing recording after Escape.                                                     | Every two-binding press/release word of lengths 0 through 6, 5,461 prefixes; cancel then drain cannot start.                                                                     |
| 8        | A failed microphone start returns the coordinator to Idle, allowing retry.                                               | Both bindings and all three activation modes, six cases. Extracted `run_effect` executes a failed external start and real `on_start_result`. No Started phase may emit.          |
| 8        | Readiness from an ended capture cannot change a newer session's UI, play its old start chime, or mute the newer session. | Three independent Loom interleaving checks, each with one ready thread and one stop/new-session thread. All three currently fail.                                                |
| 8        | Readiness already invalidated before cue handling produces no UI, chime, or mute.                                        | Three sequential controls with invalidation before the ready thread starts. These pass and provide mutation sensitivity independent of current races.                            |
| 9        | Cancellation observed by the final paste guard prevents output commitment.                                               | All Loom schedules of one paste thread and one cancellation increment, plus cancellation before paste-thread entry. Both observed-cancel and successful-commit paths must occur. |

**Output commitment** means entry to `utils::paste(final_text, app)` in the paste
thread. The covered guard is `rm_for_paste.was_cancelled_since(cancel_generation)`.
An observed cancellation means that guard's generation load returns a value
different from the session's captured generation. Once it returns true, paste
must never be called. Cancellation arriving after a false guard result has not
been observed by this thread. This check makes no promise that such cancellation
can recall a paste. Clipboard writes, paste keystrokes, hook side effects, and
post-commit cancellation remain outside this property.

Loom explores all schedules in these finite scenarios without a preemption,
duration, or permutation cap. It allows at most three threads and 1,000 branches
per execution. Exceeding the branch limit is an incomplete run, not a pass.
Failing races stop at their first counterexample. Enumeration covers the stated
finite input language, not arbitrary sequence length or every timing interval.

## Current failures, in source order

`actions.rs` checks readiness generation separately from each effect. A check
does not reserve ownership until the effect completes.

UI replay:

1. Old readiness thread completes its first generation check successfully.
2. Stop calls `invalidate_recording_readiness`; a new recording acquires UI.
3. Old thread calls `utils::emit_recording_ready`, changing newer UI.

Chime replay:

1. Old readiness thread passes its second generation check.
2. Stop invalidates old readiness; a new recording starts.
3. Old thread calls `play_feedback_sound_blocking(Start)`.

Mute replay:

1. Old readiness thread passes its third generation check.
2. Stop invalidates old readiness; a new recording has an open microphone and
   a fresh `MuteState` with `did_mute == false`.
3. Old thread enters real `apply_mute`, locks `is_open` and `mute_state`, and
   calls the OS mute boundary. Those locks protect mute bookkeeping, but they
   never revalidate the old readiness generation.

These schedules require no stale atomic reads. Pausing after a successful load
is enough. Adding another standalone load can move the gap; checking and applying
ownership need a shared protocol. Reproduce against the extracted source before
changing that protocol. Product fixes are outside this skill's initial change.

## Source coverage and synchronization

Last confirmed source synchronization commit:
`af0f7dbe419feb81bd03fa301a70ad20474c827a`.

| Source                                           | Extracted production code                                                                                                                     |
| ------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `src-tauri/src/transcription_coordinator.rs:15`  | Debounce/grace constants, event/state types, both classifiers, full `CoordinatorState`, `PendingPress` and `InputEvent` impls.                |
| `src-tauri/src/transcription_coordinator.rs:736` | Full `run_effect`, including the executor's call to `on_start_result`.                                                                        |
| `src-tauri/src/settings.rs:176`                  | `ShortcutActivation` variants, with serialization/type derives omitted.                                                                       |
| `src-tauri/src/actions.rs:666`                   | Ready closure statements from its first generation guard through UI, chime and mute. Entry assumes `readiness.wait()` already succeeded.      |
| `src-tauri/src/actions.rs:967`                   | Paste closure from its initial cancellation guard through the exact `utils::paste` invocation. Match result handling is outside commitment.   |
| `src-tauri/src/managers/audio.rs:798`            | Full `apply_mute` and production `MuteState`.                                                                                                 |
| `src-tauri/src/managers/audio.rs:1285`           | `invalidate_recording_readiness`, `is_recording_readiness_current`, `was_cancelled_since`. Covered field types are checked during extraction. |

`loom/build.rs` parses actual Rust with `syn`. It rejects missing or ambiguous
extraction sites. Coordinator functions are not cloned into the harness. Ready
and paste closures retain actual statements and branches within the stated
entry/exit boundaries. The only expression adapter changes the OS boundary
`set_mute(true)` into `self.app_handle.set_mute(true)` so its stub records which
modeled app received the effect. It preserves mute conditions, locks and ordering.

Harness substitutions and assumptions:

- Standard atomics, mutexes and thread scheduling become Loom equivalents.
  The manager is shared by one Loom Arc; per-field production Arcs add no
  independent synchronization and are flattened.
- UI events and chime calls record observable effect entry. Native event queues,
  audio playback internals, microphone callbacks and device open/close are not
  modeled. Native work can introduce more delays after these boundaries.
- Stop uses extracted readiness invalidation. A mutex-protected test oracle then
  records newer-session ownership. New capture's additional generation increment
  is omitted because one invalidation already distinguishes the old generation.
  The microphone is open and mute enabled, with fresh mute bookkeeping. Actual
  close/reopen operations can finish before a paused old effect resumes.
- Cancellation uses one `fetch_add(AcqRel)` corresponding to `cancel_recording`.
  A wrapper records generation loads that actually observed cancellation; it
  does not mark cancellation merely because another thread incremented it.
  Captured generations are normalized to zero with at most one invalidation;
  counter wraparound and cancellation spanning multiple later sessions are excluded.
- Paste records commitment. Diagnostics, tray updates, hidden-overlay calls and
  `RECORDING_SESSION` UI ownership are explicit no-op/constant stubs. None control
  whether the covered prefix reaches paste. Post-commit effects are omitted.
- External executor start reports microphone denial; action lookup, Tauri state,
  hardware failure detection, stop drain and async transcription are outside scope.
- Busy tests use Toggle. Other activation modes receive rollback coverage only;
  release grace and hold thresholds need separate properties.

## Maintain and prove sensitivity

After covered source changes, inspect extraction and each stub assumption against
current code. Run both commands. A current failure remains a finding until either
product code fixes it or evidence shows the scenario cannot happen. Keep generation
loads and effects as separate scheduled operations. Never merge them into one step
to make readiness green.

Nine controls mutate real source in temporary trees. They break busy parity,
binding protection, cancel cleanup, failed-start rollback, three readiness guards,
observed-cancellation branching, and generation cancellation detection. Each must
compile, execute its selected test, and fail its named property assertion.
Compilation/extraction errors do not count as caught mutations. The three readiness
controls target healthy already-ended checks so existing races cannot supply
false sensitivity evidence.

Confirm normal source results after mutations. Move the synchronization commit
forward only after confirming extraction and assumptions describe that source.
Use `/maintain-formal-verification-skill` after merges that changed covered code
without rerunning these checks.
