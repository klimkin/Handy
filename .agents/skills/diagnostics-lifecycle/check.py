#!/usr/bin/env python3
"""Check real diagnostics lifecycle code; 0 pass, 1 property failure, 2 incomplete."""

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

SOURCE = Path("src-tauri/src/diagnostics.rs")
TARGET = ROOT / "target" / "diagnostics-lifecycle"
PROPERTIES = {
    "race::stop_admission_interleavings": (
        "stop did not finish after detaching capture",
        "stop lost admitted event at detach boundary",
    ),
    "real::stop_drains_admitted": (
        "emission accounting lost an input",
        "stop lost admitted event in real writer",
    ),
    "real::stale_timestamp_rejected": ("pre-epoch event admitted into new capture",),
    "real::old_span_isolation": ("old span entered newer capture",),
    "real::writer_drain_inputs": ("writer lost admitted event", "event byte budget bypassed"),
    "real::accepted_event_budget": ("event byte budget bypassed",),
    "real::total_capture_resource_limit": (
        "finished capture exceeds resource limits",
        "event byte budget bypassed",
    ),
}


def run(root: Path, test: str | None = None) -> subprocess.CompletedProcess[str]:
    command = [
        "cargo", "test", "--manifest-path", str(SKILL / "loom/Cargo.toml"),
        "--target-dir", str(TARGET), "--locked", "--release", "--lib",
    ]
    if test:
        command.append(test)
    command += ["--", "--nocapture", "--test-threads=1"]
    if test:
        command.append("--exact")
    return subprocess.run(
        command, cwd=ROOT,
        env=os.environ | {"HANDY_FORMAL_SOURCE_ROOT": str(root)},
        text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=180,
    )


def baseline(label: str) -> int:
    result = run(ROOT)
    TARGET.mkdir(parents=True, exist_ok=True)
    (TARGET / f"{label}.log").write_text(result.stdout)
    print(result.stdout, end="", flush=True)
    status = verify(result, PROPERTIES)
    if "EXHAUSTED stop admission schedules:" not in result.stdout:
        raise RuntimeError("Loom schedule exhaustion was not reported")
    return status


def mutations() -> None:
    source = (ROOT / SOURCE).read_text()
    cases = [
        ("stop_early", "real::writer_drain_inputs", "for ev in self.rx {", "for ev in self.rx.into_iter().take(1) {", "writer lost admitted event"),
        ("accept_pre_epoch", "real::stale_timestamp_rejected", "if ev.at < cap.epoch {", "if false {", "pre-epoch event admitted into new capture"),
        ("ignore_event_budget", "real::accepted_event_budget", "if bytes + line.len() as u64 + sep_len + 1 > self.byte_cap {", "if false {", "event byte budget bypassed"),
    ]
    for name, test, old, new, message in cases:
        if source.count(old) != 1:
            raise RuntimeError(f"{name}: production mutation site changed")
        changed = source.replace(old, new, 1)
        with tempfile.TemporaryDirectory(prefix=f"handy-diagnostics-{name}-") as directory:
            root = Path(directory)
            path = root / SOURCE
            path.parent.mkdir(parents=True)
            path.write_text(changed)
            result = run(root, test)
        (TARGET / f"{name}.rs").write_text(changed)
        (TARGET / f"{name}.log").write_text(result.stdout)
        if verify(result, {test: (message,)}) != 1:
            print(result.stdout, end="", flush=True)
            raise RuntimeError(f"{name}: real-source mutation did not trigger its assertion")
        print(f"CAUGHT {name}: {message}", flush=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mutations", action="store_true")
    args = parser.parse_args()
    try:
        status = baseline("baseline")
        if args.mutations:
            mutations()
            restored = baseline("restored")
            if restored != status:
                raise RuntimeError("restored source changed baseline verdict")
        print("PROPERTY FAILURE" if status else "PASS", flush=True)
        print(f"Logs: {TARGET}", flush=True)
        return status
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"INCOMPLETE: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
