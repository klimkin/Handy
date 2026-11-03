#!/usr/bin/env python3
"""Check source-extracted recording lifecycle properties and mutation controls."""

import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile


SKILL = Path(__file__).resolve().parent
ROOT = SKILL.parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
from formal_verdict import verify

TARGET = ROOT / "target" / "recording-lifecycle"
PROPERTIES = {
    "busy_cancel_removes_pending_start": ("cancel retained pending start",),
    "busy_other_binding_preserves_remembered_press": ("another binding replaced remembered press",),
    "busy_toggle_parity_exhaustive": ("busy toggle parity changed",),
    "cancellation_before_paste_thread_prevents_commitment": ("cancelled generation reached paste commitment",),
    "ended_readiness_rejects_chime": ("ended readiness played start chime",),
    "ended_readiness_rejects_mute": ("ended readiness muted newer session",),
    "ended_readiness_rejects_ui": ("ended readiness changed newer UI",),
    "failed_start_rolls_coordinator_back": (
        "failed start left coordinator recording",
        "failed start retained hold",
        "failed start prevented retry",
        "failed start emitted started phase",
    ),
    "observed_cancel_prevents_paste_commitment": ("observed cancellation reached paste commitment",),
    "readiness_chime_never_follows_ended_session": ("ended readiness played start chime",),
    "readiness_mute_never_changes_newer_session": ("ended readiness muted newer session",),
    "readiness_ui_never_changes_newer_session": ("ended readiness changed newer UI",),
}
COORDINATOR = Path("src-tauri/src/transcription_coordinator.rs")
AUDIO = Path("src-tauri/src/managers/audio.rs")
ACTIONS = Path("src-tauri/src/actions.rs")
SOURCES = [COORDINATOR, AUDIO, ACTIONS, Path("src-tauri/src/settings.rs")]


def run(root: Path, test: str | None = None) -> subprocess.CompletedProcess[str]:
    command = [
        "cargo", "test", "--manifest-path", str(SKILL / "loom/Cargo.toml"),
        "--target-dir", str(TARGET), "--locked", "--release", "--lib",
    ]
    if test:
        command.append(test)
    command.extend(["--", "--nocapture", "--test-threads=1"])
    if test:
        command.append("--exact")
    return subprocess.run(
        command, cwd=ROOT,
        env=os.environ | {"HANDY_FORMAL_SOURCE_ROOT": str(root)},
        text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=180,
    )


def baseline() -> int:
    result = run(ROOT)
    print(result.stdout, end="", flush=True)
    TARGET.mkdir(parents=True, exist_ok=True)
    (TARGET / "baseline.log").write_text(result.stdout)
    return verify(result, PROPERTIES)


def mutations() -> None:
    cases = [
        ("busy_parity", COORDINATOR, "busy_toggle_parity_exhaustive",
         "(Toggle, true, Some(_)) => BusyAction::Forget,",
         "(Toggle, true, Some(_)) => BusyAction::Remember,", "busy toggle parity changed"),
        ("pending_binding", COORDINATOR, "busy_other_binding_preserves_remembered_press",
         "if pending.binding_id != input.binding_id {",
         "if false && pending.binding_id != input.binding_id {",
         "another binding replaced remembered press"),
        ("cancel_pending", COORDINATOR, "busy_cancel_removes_pending_start",
         "// asked for silence, not a deferred recording.\n        self.pending_press = None;",
         "// asked for silence, not a deferred recording.", "cancel retained pending start"),
        ("start_rollback", COORDINATOR, "failed_start_rolls_coordinator_back",
         "if !started && matches!(&self.stage, Stage::Recording(id) if id == binding_id) {",
         "if started && matches!(&self.stage, Stage::Recording(id) if id == binding_id) {",
         "failed start left coordinator recording"),
        *[(f"readiness_{effect}", AUDIO, f"ended_readiness_rejects_{effect}",
           "self.capture_generation.load(Ordering::Acquire) == generation", "true", expected)
          for effect, expected in [
              ("ui", "ended readiness changed newer UI"),
              ("chime", "ended readiness played start chime"),
              ("mute", "ended readiness muted newer session"),
          ]],
        ("observed_cancel", ACTIONS, "observed_cancel_prevents_paste_commitment",
         "if rm_for_paste.was_cancelled_since(cancel_generation) {",
         "if !rm_for_paste.was_cancelled_since(cancel_generation) {",
         "observed cancellation reached paste commitment"),
        ("cancel_before_paste", AUDIO, "cancellation_before_paste_thread_prevents_commitment",
         "self.cancel_generation.load(Ordering::Acquire) != generation", "false",
         "cancelled generation reached paste commitment"),
    ]
    errors = []
    for name, path, test, old, new, expected in cases:
        original = (ROOT / path).read_text()
        if original.count(old) != 1:
            errors.append(f"{name}: source mutation anchor changed")
            continue
        changed = original.replace(old, new, 1)
        with tempfile.TemporaryDirectory(prefix=f"handy-{name}-") as directory:
            scratch = Path(directory)
            for source in SOURCES:
                destination = scratch / source
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_text(changed if source == path else (ROOT / source).read_text())
            result = run(scratch, test)
        (TARGET / f"{name}.log").write_text(result.stdout)
        (TARGET / f"{name}.rs").write_text(changed)
        try:
            failed = verify(result, {test: (expected,)})
            if not failed:
                raise RuntimeError("expected property assertion did not fail")
        except RuntimeError as error:
            print(result.stdout, end="", flush=True)
            errors.append(f"{name}: {error}")
        else:
            print(f"CAUGHT {name}: {expected}", flush=True)
    if errors:
        raise RuntimeError("; ".join(errors))
    print(f"Mutation controls: {len(cases)} caught", flush=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mutations", action="store_true", help="also run nine scratch source mutation controls")
    arguments = parser.parse_args()
    status = 0
    try:
        status = baseline()
        if arguments.mutations:
            mutations()
            status |= baseline()
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"INCOMPLETE: {error}", file=sys.stderr)
        return 2
    return status


if __name__ == "__main__":
    sys.exit(main())
