#!/usr/bin/env python3
"""Check extracted engine cancellation code and its source mutation controls."""

import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile


SKILL = Path(__file__).resolve().parent
ROOT = SKILL.parents[2]
SOURCE = Path("src-tauri/src/engine_supervisor/supervisor.rs")
TARGET = ROOT / "target" / "engine-cancellation"


def run(source_root: Path, test: str | None = None) -> subprocess.CompletedProcess[str]:
    command = [
        "cargo", "test", "--manifest-path", str(SKILL / "loom" / "Cargo.toml"),
        "--target-dir", str(TARGET), "--locked", "--release", "--lib",
    ]
    if test:
        command.append(test)
    command.extend(["--", "--nocapture", "--test-threads=1"])
    environment = os.environ | {"HANDY_FORMAL_SOURCE_ROOT": str(source_root)}
    return subprocess.run(
        command, cwd=ROOT, env=environment, text=True,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=180,
    )


def baseline() -> None:
    result = run(ROOT)
    print(result.stdout, end="", flush=True)
    if result.returncode or "3 passed; 0 failed" not in result.stdout:
        raise RuntimeError("production cancellation check did not complete all three scenarios")


def mutations() -> None:
    text = (ROOT / SOURCE).read_text()
    cases = [
        (
            "missing_epoch_check", "cancel_stops_hung_marked_work",
            "            if self.shared.cancelled_since(epoch) {\n"
            "                return Err(Failure::Cancelled);\n"
            "            }\n",
            "",
            "cancel returned but marked work was neither rejected nor killed",
        ),
        (
            "publication_outside_lock", "cancel_stops_hung_marked_work",
            "            let sent = worker.control.send(frame);\n"
            "            *in_flight = Some(InFlight {",
            "            drop(in_flight);\n"
            "            let sent = worker.control.send(frame);\n"
            "            let mut in_flight = lock(&self.shared.in_flight);\n"
            "            *in_flight = Some(InFlight {",
            "cancel returned but marked work was neither rejected nor killed",
        ),
        (
            "accept_cancelled_reply", "cancel_rejects_buffered_reply",
            "        if killed || self.shared.cancelled_since(epoch) {\n",
            "        if false {\n",
            "cancelled call returned a successful reply",
        ),
    ]
    for name, test, old, new, expected in cases:
        if text.count(old) != 1:
            raise RuntimeError(f"{name}: mutation site changed; inspect current production code")
        with tempfile.TemporaryDirectory(prefix=f"handy-{name}-") as directory:
            scratch = Path(directory)
            source = scratch / SOURCE
            source.parent.mkdir(parents=True)
            source.write_text(text.replace(old, new, 1))
            result = run(scratch, test)
        TARGET.mkdir(parents=True, exist_ok=True)
        (TARGET / f"{name}.log").write_text(result.stdout)
        (TARGET / f"{name}.rs").write_text(text.replace(old, new, 1))
        if result.returncode == 0 or expected not in result.stdout or "test result: FAILED" not in result.stdout:
            print(result.stdout, end="", flush=True)
            raise RuntimeError(f"{name}: expected property assertion did not fail")
        print(f"CAUGHT {name}: {expected}", flush=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mutations", action="store_true", help="also prove checks reject three source mutations")
    arguments = parser.parse_args()
    try:
        baseline()
        if arguments.mutations:
            mutations()
            baseline()
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"INCOMPLETE: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
