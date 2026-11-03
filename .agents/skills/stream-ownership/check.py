#!/usr/bin/env python3
"""Check stream ownership and cancellation against extracted production Rust."""

import argparse
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


SKILL = Path(__file__).resolve().parent
ROOT = SKILL.parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
from formal_verdict import verify

MANAGER = Path("src-tauri/src/managers/transcription.rs")
ENGINE = Path("src-tauri/src/engine_supervisor/supervisor.rs")
AUDIO = Path("src-tauri/src/managers/audio.rs")
SINK = Path("src-tauri/src/audio_toolkit/audio/sink.rs")
ACTIONS = Path("src-tauri/src/actions.rs")
TARGET = ROOT / "target" / "stream-ownership"
PROPERTIES = {
    "engine::cancelled_finalize_never_falls_back": (
        "PROPERTY finalize: observed cancel became batch fallback",
        "PROPERTY finalize: cancelled finalize dispatched work",
        "PROPERTY finalize: old cancelled finalize removed replacement stream",
        "PROPERTY finalize-boundary: worker cancel became batch fallback",
        "PROPERTY finalize: cancelled worker stream was not reset",
    ),
    "engine::frames_cannot_reach_replaced_stream": (
        "PROPERTY feed: frame crossed stream id/closed/cancel/worker fence",
    ),
    "engine::handle_preserves_cancelled_finalize": (
        "PROPERTY handle-finalize: cancellation became batch fallback",
    ),
    "engine::uncancelled_finalize_retains_fallback": (
        "PROPERTY fallback: uncancelled finalize returned wrong usable result",
        "PROPERTY fallback: uncancelled finalize returned wrong batch fallback",
    ),
    "manager::guard_cannot_clear_replacement": (
        "PROPERTY guard: old cleanup cleared replacement worker state",
        "PROPERTY guard: old cleanup must retire token 1",
    ),
    "manager::manager_preserves_cancelled_finalize": (
        "PROPERTY manager-finalize: cancellation became batch fallback",
    ),
    "manager::stalled_old_callback_cannot_feed_replacement": (
        "PROPERTY router: old recording frame reached replacement route",
        "PROPERTY router: replacement dropped its fresh frame",
    ),
}


def recorder_source() -> Path:
    lock = (ROOT / "src-tauri/Cargo.lock").read_text()
    if not re.search(r'\[\[package\]\]\nname = "handy-recorder"\nversion = "0\.1\.0"\n', lock):
        raise RuntimeError("handy-recorder version changed; review timeout/detach overlap boundary")
    explicit = os.environ.get("HANDY_RECORDER_SOURCE")
    if explicit:
        candidate = Path(explicit)
        if (candidate / "src/capture/engine.rs").is_file():
            return candidate
        raise RuntimeError("HANDY_RECORDER_SOURCE lacks src/capture/engine.rs")
    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    candidates = sorted((cargo_home / "registry/src").glob("*/handy-recorder-0.1.0"))
    if not candidates:
        raise RuntimeError("cache handy-recorder 0.1.0 with cargo fetch --manifest-path src-tauri/Cargo.toml, or set HANDY_RECORDER_SOURCE")
    return candidates[0]


def run(source_root: Path, test: str | None = None) -> subprocess.CompletedProcess[str]:
    command = [
        "cargo", "test", "--manifest-path", str(SKILL / "loom" / "Cargo.toml"),
        "--target-dir", str(TARGET), "--locked", "--release", "--lib",
    ]
    if test:
        command.append(test)
    command.extend(["--", "--nocapture", "--test-threads=1"])
    if test:
        command.append("--exact")
    return subprocess.run(
        command, cwd=ROOT, env=os.environ | {
            "HANDY_FORMAL_SOURCE_ROOT": str(source_root),
            "HANDY_RECORDER_SOURCE": str(recorder_source()),
        },
        text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=240,
    )


def baseline() -> int:
    result = run(ROOT)
    print(result.stdout, end="", flush=True)
    TARGET.mkdir(parents=True, exist_ok=True)
    (TARGET / "baseline.log").write_text(result.stdout)
    status = verify(result, PROPERTIES)
    if status:
        print("FINDING: current production code violates stream property", flush=True)
    return status


def mutations() -> None:
    sources = {path: (ROOT / path).read_text() for path in [MANAGER, ENGINE, AUDIO, SINK, ACTIONS]}
    guard = """        if self.active_stream_worker.load(Ordering::Acquire) == self.worker_id {
            self.stream_active.store(false, Ordering::Release);
        }
        let _ = self.active_stream_worker.compare_exchange(
            self.worker_id,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );"""
    reordered_guard = """        if self.active_stream_worker.load(Ordering::Acquire) == self.worker_id {
            let _ = self.active_stream_worker.compare_exchange(
                self.worker_id,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            self.stream_active.store(false, Ordering::Release);
        }"""
    early_cancel = """        if self.shared.cancelled_since(epoch) {
            if self.stream.as_ref().is_some_and(|s| s.id == id) {
                self.end_stream();
            }
            return Err(EngineError::Cancelled);
        }
"""
    boundary_cancel = """            Err(Failure::Cancelled) => {
                // The finalize may never have reached the worker.
                self.reset_worker_stream();
                Err(EngineError::Cancelled)
            }"""
    cases = [
        ("retire_token_before_flag", MANAGER, "manager::guard_cannot_clear_replacement", guard,
         reordered_guard, "PROPERTY guard: old cleanup cleared replacement worker state"),
        ("remove_feed_id_fence", ENGINE, "engine::frames_cannot_reach_replaced_stream",
         "self.stream.as_ref().filter(|s| s.id == id)", "self.stream.as_ref()",
         "PROPERTY feed: frame crossed stream id/closed/cancel/worker fence"),
        ("remove_early_cancel", ENGINE, "engine::cancelled_finalize_never_falls_back",
         early_cancel, "", "PROPERTY finalize: observed cancel became batch fallback"),
        ("swallow_worker_cancel", ENGINE, "engine::cancelled_finalize_never_falls_back",
         boundary_cancel, "            Err(Failure::Cancelled) => Ok(None),",
         "PROPERTY finalize-boundary: worker cancel became batch fallback"),
        ("swallow_manager_cancel", MANAGER, "manager::manager_preserves_cancelled_finalize",
         "            Ok(Err(e)) => Err(e.into()),",
         "            Ok(Err(_)) => Ok(None),",
         "PROPERTY manager-finalize: cancellation became batch fallback"),
        ("swallow_handle_cancel", ENGINE, "engine::handle_preserves_cancelled_finalize",
         "        answer.recv().unwrap_or(Ok(None))",
         "        answer.recv().unwrap_or(Ok(None)).or(Ok(None))",
         "PROPERTY handle-finalize: cancellation became batch fallback"),
        ("drop_router_frames", MANAGER, "manager::stalled_old_callback_cannot_feed_replacement",
         "            let _ = route.tx.send(StreamCmd::Feed(frame.to_vec()));",
         "            let _ = frame;",
         "PROPERTY router: replacement dropped its fresh frame"),
    ]
    errors = []
    for name, path, test, old, new, expected in cases:
        if sources[path].count(old) != 1:
            errors.append(f"{name}: mutation site changed; review production source")
            continue
        mutated = sources[path].replace(old, new, 1)
        with tempfile.TemporaryDirectory(prefix=f"handy-{name}-") as directory:
            scratch = Path(directory)
            for source, content in sources.items():
                destination = scratch / source
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_text(mutated if source == path else content)
            result = run(scratch, test)
        TARGET.mkdir(parents=True, exist_ok=True)
        (TARGET / f"{name}.log").write_text(result.stdout)
        (TARGET / f"{name}.rs").write_text(mutated)
        try:
            if verify(result, {test: (expected,)}) != 1:
                raise RuntimeError("property assertion did not reject executable mutation")
        except RuntimeError as error:
            print(result.stdout, end="", flush=True)
            errors.append(f"{name}: {error}")
        else:
            print(f"CAUGHT {name}: {expected}", flush=True)
    if errors:
        raise RuntimeError("; ".join(errors))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mutations", action="store_true", help="run seven production-source mutation controls")
    arguments = parser.parse_args()
    try:
        status = baseline()
        if arguments.mutations:
            mutations()
            status = max(status, baseline())
        return status
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"INCOMPLETE: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
