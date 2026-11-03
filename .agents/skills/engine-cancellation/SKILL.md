---
name: engine-cancellation
description: Check engine cancellation races when changing supervisor cancel epochs, in-flight publication, call_work, or worker kill/reply handling. Runs exhaustive Loom schedules against extracted production Rust and checks source mutations.
---

# Engine cancellation

A cancel must reject old marked work or kill its published worker, and a reply
released after that cancel must return `Cancelled`. A missed cancel leaves
unwanted inference running or lets cancelled text continue toward output.

## Run

From repository root, with Python 3.10+ and stable Rust installed:

```sh
python3 .agents/skills/engine-cancellation/check.py
```

Cargo downloads locked dependencies on first use. Loom is pinned to 0.7.2 in
`loom/Cargo.toml`; `loom/Cargo.lock` pins transitive dependencies. No Tauri build,
GPU, audio device, or speech model is needed.

1. Read covered methods and the worker substitutions in `loom/src/lib.rs`.
   Compare them with the sync commit below when product code has changed.
2. Run the command. All three scenarios must complete. `INCOMPLETE`, compilation
   errors, schedule limits, and timeouts provide no passing verdict.
3. For a concurrency change or a harness update, run the source mutation controls:

   ```sh
   python3 .agents/skills/engine-cancellation/check.py --mutations
   ```

   Expect three `CAUGHT` lines between two passing baseline runs. The script
   mutates temporary copies of production `supervisor.rs`, compiles their real
   method bodies, and requires each expected assertion to fail. Logs and mutated
   source copies go to `target/engine-cancellation/` as `.log` and `.rs` files.
   Working product files stay untouched.

## Interpret results

| Scenario                        | Required behavior                                                                         |
| ------------------------------- | ----------------------------------------------------------------------------------------- |
| `cancel_stops_hung_marked_work` | After cancel returns, old work was rejected before send or its worker received kill.      |
| `cancel_rejects_buffered_reply` | A buffered successful reply released after cancel completes returns `Failure::Cancelled`. |
| `fresh_mark_runs_after_cancel`  | Work marked after a completed cancel still sends and succeeds.                            |

Both concurrent scenarios require schedules that reject work and schedules that
publish work. Observation counters use host atomics only after checking each
execution; all synchronization affecting the algorithm uses Loom types.

The mutation controls demonstrate these broken paths:

- Remove pre-send epoch check. Cancel bumps epoch, finds no in-flight call, and
  returns. Old marked work then sends and runs without a kill.
- Release mutex before send/publication. Owner checks old epoch and unlocks.
  Cancel bumps epoch, finds no call, and returns. Owner publishes too late.
- Remove reply cancellation gate. Owner publishes work. Cancel takes it and
  kills worker. Buffered reply arrives and escapes as success.

For an unexpected failure, map the assertion to source operations before
reporting a product bug. Check whether the real worker can exhibit the simulated
behavior and whether caller ordering permits it. Preserve full assertion output
and the failing source revision. Change product code only when requested. For an intended
behavior change, update the stated promise, assertions, and mutation controls
together after confirming that promise with the user.

## Coverage and synchronization

Last source correspondence checked at
`af0f7dbe419feb81bd03fa301a70ad20474c827a`, using Rust 1.99.1.

All covered product code is in
`src-tauri/src/engine_supervisor/supervisor.rs`:

| Extracted item                             | Responsibility                                                             |
| ------------------------------------------ | -------------------------------------------------------------------------- |
| `Shared::epoch`, `Shared::cancelled_since` | Read and compare cancellation marks.                                       |
| `EngineSupervisor::cancel`                 | Increment epoch, take published call, kill worker.                         |
| `Owner::call_work`                         | Check/send/publish under mutex, collect response, reject cancelled result. |
| `lock`, `InFlight`                         | Preserve locking helper and published-call shape.                          |

`loom/build.rs` parses this file on every relevant rebuild and compiles those
items without rewriting their bodies. Unqualified `Arc`, `Mutex`, `AtomicU64`,
and `Ordering` resolve to Loom types. Shared epoch/slot field types are checked
against production. Missing or renamed methods stop the build.

`Control::send` simulates successful nonblocking enqueue. `Control::kill`
simulates a successful kill request. `Worker::wait` either stays hung until
cancel completes, delivers a buffered response after cancel, or responds
immediately for the fresh-mark witness. Encoding and logging are inert.
Review these substitutions when changing `Control`, `Worker::wait`,
`encode_request`, or mutex/atomic imports. A green run alone does not establish
that changed boundary behavior still matches them.

Move the sync commit forward only after reading changes since that commit,
confirming the extraction and substitutions still describe them, and rerunning
both normal checks and mutation controls. Use
`/maintain-formal-verification-skill` after covered code changes without rechecking.

## Bounds and exclusions

Loom exhausts its modeled schedules for one owner call with an old epoch and
one concurrent cancel, plus their joining test thread. There is no preemption,
permutation, duration, or checkpoint-resume pruning. A 1,000-branch per-execution
guard and 180-second per-command timeout fail the run if exceeded.

This checks cancellation admission/publication and post-cancel reply handling.
It does not prove process termination, driver behavior, time bounds, worker
respawn, queue ordering, model-load cancellation, multiple calls/cancels, epoch
wraparound, clipboard effects, or cancellation after reply acceptance. Both
concurrent scenarios release the simulated worker response only after cancel
returns. This is bounded concurrency verification, not proof of the whole app.

Tool semantics and limitations are documented in
[Loom 0.7.2](https://docs.rs/loom/0.7.2/loom/).
