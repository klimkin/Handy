"""Classify serial, uncaptured Rust property tests from complete Cargo output."""

from collections.abc import Mapping, Sequence
import re
import subprocess


SUMMARY = re.compile(
    r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; "
    r"(\d+) ignored; (\d+) measured; (\d+) filtered out;.*$", re.MULTILINE,
)
TEST = re.compile(r"^test ([\w:]+) \.\.\. ", re.MULTILINE)
PANIC = re.compile(
    r"^thread '([^'\n]*)'(?: \([^\n]*\))? panicked at [^\n]+:\n([^\n]*)",
    re.MULTILINE,
)


def verify(
    result: subprocess.CompletedProcess[str],
    expected: Mapping[str, Sequence[str]],
) -> int:
    """Return 0 or 1 only for complete tests with recognized property failures.

    Each expected identity maps to its allowed assertion messages. Only the
    first line of that test's own panic may provide assertion evidence. Unknown
    panics, missing tests, and inconsistent process results are incomplete.
    """
    output = result.stdout
    summaries = list(SUMMARY.finditer(output))
    runs = re.findall(r"^running (\d+) tests?$", output, re.MULTILINE)
    if len(summaries) != 1 or runs != [str(len(expected))] or not expected:
        raise RuntimeError("requested property tests did not complete exactly once")
    summary = summaries[0]
    status, passed, failed, ignored, measured, _filtered = summary.groups()
    passed, failed = int(passed), int(failed)
    if passed + failed != len(expected) or int(ignored) or int(measured):
        raise RuntimeError("requested property tests did not all finish")
    if status != ("FAILED" if failed else "ok") or result.returncode != (101 if failed else 0):
        raise RuntimeError(f"unexpected cargo result {result.returncode}")

    starts = list(TEST.finditer(output))
    names = [start[1] for start in starts]
    if len(names) != len(expected) or set(names) != set(expected):
        raise RuntimeError("completed test identities differ from requested properties")
    outcomes = {}
    associated_panics = 0
    for index, start in enumerate(starts):
        end = starts[index + 1].start() if index + 1 < len(starts) else summary.start()
        body = re.split(r"^failures:\s*$", output[start.end():end], maxsplit=1, flags=re.MULTILINE)[0]
        completions = re.findall(r"^(ok|FAILED)$", body, re.MULTILINE)
        if len(completions) != 1:
            raise RuntimeError(f"{start[1]}: missing or ambiguous completion")
        outcome = completions[0]
        outcomes[start[1]] = outcome
        panics = list(PANIC.finditer(body))
        associated_panics += len(panics)
        if outcome == "FAILED" and not panics:
            raise RuntimeError(f"{start[1]}: failure has no named property assertion")
        for panic in panics:
            name, assertion = panic.groups()
            assertion = re.sub(r"^assertion `[^`]+` failed: ", "", assertion)
            if outcome != "FAILED" or name != start[1] or not any(
                assertion == message or assertion.startswith(message + ";")
                for message in expected[name]
            ):
                raise RuntimeError(f"{start[1]}: unrecognized or tool panic: {panic[2]}")
    if output.count(" panicked at ") != associated_panics:
        raise RuntimeError("panic outside a completed property test")
    if sum(outcome == "FAILED" for outcome in outcomes.values()) != failed:
        raise RuntimeError("completed test outcomes differ from summary")
    return int(bool(failed))
