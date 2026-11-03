#!/usr/bin/env python3
"""Verify model worker lifetime, publication, and bounded crash recovery."""

import argparse
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

SKILL = Path(__file__).resolve().parent
ROOT = SKILL.parents[2]
SOURCE = Path("src-tauri/src/engine_supervisor/supervisor.rs")
TARGET = ROOT / "target/engine-lifecycle"


def run(root: Path, test: str | None = None) -> subprocess.CompletedProcess[str]:
    command = ["cargo", "test", "--manifest-path", str(SKILL / "loom/Cargo.toml"),
               "--target-dir", str(TARGET), "--locked", "--release", "--lib"]
    if test:
        command.append(test)
    command.extend(["--", "--nocapture", "--test-threads=1"])
    if test:
        command.append("--exact")
    return subprocess.run(command, cwd=ROOT,
                          env=os.environ | {"HANDY_FORMAL_SOURCE_ROOT": str(root)},
                          text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=180)


def verdict(result: subprocess.CompletedProcess[str], count: int) -> int:
    match = re.search(r"test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored", result.stdout)
    if not match or int(match[1]) + int(match[2]) != count or int(match[3]):
        raise RuntimeError("requested property checks did not all finish")
    failed = bool(int(match[2]))
    if result.returncode != (101 if failed else 0):
        raise RuntimeError(f"unexpected cargo status {result.returncode}")
    return int(failed)


def baseline() -> int:
    result = run(ROOT)
    print(result.stdout, end="", flush=True)
    TARGET.mkdir(parents=True, exist_ok=True)
    (TARGET / "baseline.log").write_text(result.stdout)
    return verdict(result, 7)


def mutations() -> None:
    cases = [
        ("overlap_replacement", "previous_worker_exits_before_replacement",
         "    fn load(&mut self, spec: LoadSpec) -> Result<LoadedInfo, EngineError> {\n        self.unload();",
         "    fn load(&mut self, spec: LoadSpec) -> Result<LoadedInfo, EngineError> {\n        let _old_worker = self.worker.take();",
         "replacement started before previous worker exited and was reaped"),
        ("omit_reap", "unload_acknowledgement_follows_reaping",
         "            let _ = child.wait();", "            let _ = ();",
         "unload acknowledged before worker was reaped"),
        ("early_unload_ack", "unload_acknowledgement_follows_reaping",
         "                    self.unload();\n                    self.unloads_done += 1;\n                    let _ = reply.send(());",
         "                    let _ = reply.send(());\n                    self.unload();\n                    self.unloads_done += 1;",
         "unload acknowledged before worker was reaped"),
        ("late_publication", "pending_unload_blocks_late_publication",
         "if self.shared.unloads_requested.load(Ordering::SeqCst) == self.unloads_done {", "if true {",
         "pending unload resurrected loaded model"),
        ("unbounded_retry", "transcription_retry_is_bounded",
         "if retried || pinned || hung_on_cpu {", "if pinned || hung_on_cpu {",
         "transcription retry exceeded two attempts"),
        ("pinned_load_fallback", "load_recovery_is_bounded_and_respects_selection",
         "&& !wanted.pinned()", "",
         "explicit selection silently fell back"),
        ("pinned_run_retry", "transcription_retry_is_bounded",
         "if retried || pinned || hung_on_cpu {", "if retried || hung_on_cpu {",
         "failure bypassed no-retry policy"),
        ("gpu_fallback_again", "load_recovery_is_bounded_and_respects_selection",
         "self.start_worker(&wanted.cpu())?", "self.start_worker(&wanted)?",
         "GPU recovery did not use CPU-only worker"),
        ("recover_cancel", "transcription_retry_is_bounded",
         "matches!(self, Failure::Crashed(_) | Failure::Hung(_))",
         "matches!(self, Failure::Crashed(_) | Failure::Hung(_) | Failure::Cancelled)",
         "cancellation entered GPU recovery"),
        ("run_cancelled_queue", "cancelled_queued_work_never_starts",
         "            if self.shared.cancelled_since(epoch) {\n                return Err(EngineError::Cancelled);\n            }",
         "", "old queued work started after cancel"),
    ]
    source = (ROOT / SOURCE).read_text()
    for name, test, old, new, expected in cases:
        if source.count(old) != 1:
            raise RuntimeError(f"{name}: source mutation site changed")
        changed = source.replace(old, new, 1)
        with tempfile.TemporaryDirectory(prefix=f"handy-{name}-") as directory:
            root = Path(directory)
            path = root / SOURCE
            path.parent.mkdir(parents=True)
            path.write_text(changed)
            result = run(root, test)
        (TARGET / f"{name}.log").write_text(result.stdout)
        (TARGET / f"{name}.rs").write_text(changed)
        if verdict(result, 1) != 1 or expected not in result.stdout:
            print(result.stdout, end="", flush=True)
            raise RuntimeError(f"{name}: expected property assertion did not fail")
        print(f"CAUGHT {name}: {expected}", flush=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mutations", action="store_true")
    args = parser.parse_args()
    try:
        status = baseline()
        if args.mutations:
            mutations()
            if baseline() != status:
                raise RuntimeError("restored source changed property verdict")
        return status
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"INCOMPLETE: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
