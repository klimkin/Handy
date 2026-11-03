import subprocess
import unittest

from formal_verdict import verify


def completed(tests, returncode=None):
    chunks = [f"running {len(tests)} tests"]
    failed = 0
    for name, panic_messages in tests:
        chunks.append(f"test {name} ... ")
        for message in panic_messages:
            chunks.append(f"\nthread '{name}' (123) panicked at src/lib.rs:10:5:\n{message}\n")
        chunks.append("FAILED\n" if panic_messages else "ok\n")
        failed += bool(panic_messages)
    chunks.append(
        f"\ntest result: {'FAILED' if failed else 'ok'}. {len(tests) - failed} passed; "
        f"{failed} failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n"
    )
    return subprocess.CompletedProcess([], (101 if failed else 0) if returncode is None else returncode, "\n".join(chunks))


class FormalVerdictTests(unittest.TestCase):
    expected = {"property": ("PROPERTY checked invariant",)}

    def test_complete_pass(self):
        self.assertEqual(verify(completed([("property", [])]), self.expected), 0)

    def test_genuine_assertion_is_finding(self):
        for assertion in ["PROPERTY checked invariant", "assertion `left == right` failed: PROPERTY checked invariant; input=1"]:
            with self.subTest(assertion=assertion):
                self.assertEqual(verify(completed([("property", [assertion])]), self.expected), 1)

    def test_old_parser_branch_exhaustion_probe_is_incomplete(self):
        result = completed([("property", ["Model exceeded maximum number of branches. This is often caused by an algorithm requiring the processor to make progress."])])
        self.assertIn("test result: FAILED", result.stdout)
        with self.assertRaises(RuntimeError):
            verify(result, self.expected)

    def test_known_finding_cannot_mask_tool_panic(self):
        for panic in ["deadlock; threads = []", "Model exceeded maximum number of branches", "harness setup failed"]:
            with self.subTest(panic=panic), self.assertRaises(RuntimeError):
                verify(completed([("property", ["PROPERTY checked invariant", panic])]), self.expected)

    def test_each_failed_test_needs_its_own_assertion(self):
        expected = self.expected | {"other": ("other invariant",)}
        result = completed([("property", ["PROPERTY checked invariant"]), ("other", ["deadlock"])])
        with self.assertRaises(RuntimeError):
            verify(result, expected)

    def test_printed_marker_cannot_replace_panic_evidence(self):
        result = completed([("property", ["deadlock"])])
        result.stdout = result.stdout.replace("test property ... ", "test property ... PROPERTY checked invariant")
        with self.assertRaises(RuntimeError):
            verify(result, self.expected)

    def test_missing_extra_or_wrong_test_is_incomplete(self):
        for tests in [[], [("other", [])], [("property", []), ("extra", [])], [("property", []), ("property", [])]]:
            with self.subTest(tests=tests), self.assertRaises(RuntimeError):
                verify(completed(tests), self.expected)

    def test_wrong_process_status_is_incomplete(self):
        for status in [0, 1, 2, -6]:
            with self.subTest(status=status), self.assertRaises(RuntimeError):
                verify(completed([("property", ["PROPERTY checked invariant"])], status), self.expected)

    def test_panic_in_wrong_thread_is_incomplete(self):
        result = completed([("property", ["PROPERTY checked invariant"])])
        result.stdout = result.stdout.replace("thread 'property'", "thread 'harness'")
        with self.assertRaises(RuntimeError):
            verify(result, self.expected)

    def test_missing_completion_is_incomplete(self):
        result = completed([("property", ["PROPERTY checked invariant"])])
        result.stdout = result.stdout.replace("\nFAILED\n", "\n")
        with self.assertRaises(RuntimeError):
            verify(result, self.expected)

    def test_compile_failure_is_incomplete(self):
        with self.assertRaises(RuntimeError):
            verify(subprocess.CompletedProcess([], 101, "error: could not compile"), self.expected)


if __name__ == "__main__":
    unittest.main()
