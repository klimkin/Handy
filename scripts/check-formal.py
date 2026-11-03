#!/usr/bin/env python3
"""Run every bounded transcription lifecycle check without hiding findings."""

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import re
import subprocess
import sys


ROOT = Path(__file__).resolve().parent.parent
SUITES = {
    "engine-cancellation": [1],
    "engine-lifecycle": [2, 3, 4],
    "stream-ownership": [5, 6],
    "recording-lifecycle": [7, 8, 9],
    "diagnostics-lifecycle": [10],
}


def check(name: str, mutations: bool, output: Path) -> dict:
    script = ROOT / ".agents/skills" / name / "check.py"
    command = [sys.executable, str(script)]
    if mutations:
        command.append("--mutations")
    print(f"Checking {name}, priorities {SUITES[name]}...", flush=True)
    try:
        result = subprocess.run(command, cwd=ROOT, text=True, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, timeout=900)
        text = result.stdout
        status = result.returncode
        if status not in (0, 1) or "INCOMPLETE:" in text:
            status = 2
    except (OSError, subprocess.TimeoutExpired) as error:
        text = f"INCOMPLETE: {error}\n"
        status = 2
    log = output / f"{name}.log"
    log.write_text(text)
    summaries = re.findall(
        r"test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored", text)
    counts = None
    if summaries:
        passed, failed, ignored = map(int, summaries[-1])
        counts = {"passed": passed, "failed": failed, "ignored": ignored}
    if counts is None or counts["ignored"]:
        status = 2
    label = {0: "VERIFIED", 1: "FINDING", 2: "INCOMPLETE"}[status]
    controls = len(re.findall(r"^CAUGHT ", text, flags=re.MULTILINE))
    print(f"{label} {name}: {counts}; {controls} mutation controls. Log: {log.relative_to(ROOT)}", flush=True)
    return {"suite": name, "priorities": SUITES[name], "status": label,
            "exit_code": status, "tests": counts, "mutation_controls": controls,
            "log": str(log.relative_to(ROOT))}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mutations", action="store_true", help="also check deliberate source breaks")
    parser.add_argument("--suite", choices=SUITES, action="append", help="run only selected suites")
    args = parser.parse_args()
    output = ROOT / "target/formal-lifecycle"
    output.mkdir(parents=True, exist_ok=True)
    results = [check(name, args.mutations, output) for name in args.suite or SUITES]
    revision = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True,
                              stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    report = {"checked_at": datetime.now(timezone.utc).isoformat(),
              "revision": revision.stdout.strip() if revision.returncode == 0 else None,
              "mutations": args.mutations, "results": results}
    (output / "results.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"Report: {output.relative_to(ROOT)}/results.json", flush=True)
    if any(item["exit_code"] == 2 for item in results):
        return 2
    return int(any(item["exit_code"] == 1 for item in results))


if __name__ == "__main__":
    sys.exit(main())
