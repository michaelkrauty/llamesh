#!/usr/bin/env python3
import os
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/log-tools/errors.sh"
DATE = "2026-01-02"
LOG = f"proxy.log.{DATE}"


class ErrorsScriptTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.local = self.root / "local"
        self.remote = self.root / "remote"
        self.bin = self.root / "bin"
        for directory in (self.local, self.remote, self.bin):
            directory.mkdir()
        ssh = self.bin / "ssh"
        ssh.write_text(
            "#!/bin/sh\n"
            'case "${FAKE_SSH_RESULT:-ok}" in\n'
            '  ok) cat "${FAKE_REMOTE_LOG:?}" ;;\n'
            "  missing) exit 1 ;;\n"
            "  fail) printf '%s\\n' 'simulated ssh failure' >&2; exit 255 ;;\n"
            "esac\n"
        )
        ssh.chmod(0o755)

    def write(self, directory, text):
        (directory / LOG).write_text(text)

    def invoke(
        self,
        node="local",
        *,
        result="ok",
        host="test-host",
        local_text=None,
        remote_text=None,
    ):
        if local_text is not None:
            self.write(self.local, local_text)
        if remote_text is not None:
            self.write(self.remote, remote_text)
        env = os.environ | {
            "LLAMESH_LOCAL_LOG_DIR": str(self.local),
            "LLAMESH_REMOTE_LOG_DIR": str(self.remote),
            "LLAMESH_REMOTE_HOST": host,
            "FAKE_REMOTE_LOG": str(self.remote / LOG),
            "FAKE_SSH_RESULT": result,
            "PATH": f"{self.bin}:{os.environ['PATH']}",
        }
        return subprocess.run(
            ["bash", str(SCRIPT), node, DATE, "2"],
            env=env,
            text=True,
            capture_output=True,
            check=False,
            timeout=5,
        )

    def test_filters_warn_and_error_and_honors_limit(self):
        lines = [
            '{"timestamp":"2026-01-02T01:02:03.123Z","level":"INFO","target":"app","fields":{"message":"ignore"}}',
            '{"timestamp":"2026-01-02T01:02:04.123Z","level":"WARN","target":"app","fields":{"event":"warning event"}}',
            '{"timestamp":"2026-01-02T01:02:05.123Z","level":"ERROR","target":"app","fields":{"message":"discarded error"}}',
            '{"timestamp":"2026-01-02T01:02:06.123Z","level":"WARN","target":"app","fields":{"event":"warning event"}}',
            '{"timestamp":"2026-01-02T01:02:07.123Z","level":"ERROR","target":"app","fields":{"message":"error two"}}',
        ]
        completed = self.invoke(local_text="\n".join(lines))
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(
            completed.stdout,
            "2026-01-02T01:02:06 [WARN] app: warning event\n"
            "2026-01-02T01:02:07 [ERROR] app: error two\n",
        )

    def test_empty_and_info_only_logs_succeed(self):
        for node, local_text, remote_text in (
            ("local", "", None),
            ("remote", None, ""),
            ("both", "", ""),
            ("local", '{"level":"INFO","fields":{"message":"quiet"}}\n', None),
            ("remote", None, '{"level":"INFO","fields":{"message":"quiet"}}\n'),
            (
                "both",
                '{"level":"INFO","fields":{"message":"quiet"}}\n',
                '{"level":"INFO","fields":{"message":"quiet"}}\n',
            ),
        ):
            with self.subTest(node=node, empty=not local_text and not remote_text):
                completed = self.invoke(
                    node, local_text=local_text, remote_text=remote_text
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)
                self.assertNotIn("[ERROR]", completed.stdout)
                self.assertNotIn("[WARN]", completed.stdout)

    def test_failures_are_visible_and_not_reported_as_empty(self):
        cases = (
            ("missing local log", "local", {}, "Log file not found"),
            ("remote host absent", "remote", {"host": ""}, "Set LLAMESH_REMOTE_HOST"),
            ("ssh failure", "remote", {"result": "fail"}, "simulated ssh failure"),
            (
                "missing remote log",
                "remote",
                {"result": "missing"},
                "Log file not found on",
            ),
            ("malformed JSON", "local", {"local_text": "not json"}, "parse error"),
        )
        for name, node, options, expected in cases:
            with self.subTest(name=name):
                completed = self.invoke(node, **options)
                self.assertNotEqual(completed.returncode, 0)
                self.assertIn(expected, completed.stderr)
                self.assertNotIn("(no errors)", completed.stdout)

    def test_both_attempts_remote_after_local_failure(self):
        completed = self.invoke(
            "both",
            remote_text='{"timestamp":"2026-01-02T01:02:03Z","level":"WARN","target":"app","fields":{"message":"remote warning"}}\n',
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("Log file not found", completed.stderr)
        self.assertIn("remote warning", completed.stdout)
        self.assertNotIn("(no errors)", completed.stdout)

    def test_both_fails_after_remote_failure_but_keeps_local_output(self):
        completed = self.invoke(
            "both",
            result="fail",
            local_text='{"timestamp":"2026-01-02T01:02:03Z","level":"ERROR","target":"app","fields":{"message":"local error"}}\n',
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("local error", completed.stdout)
        self.assertIn("simulated ssh failure", completed.stderr)
        self.assertNotIn("(no errors)", completed.stdout)

    def test_both_fails_for_malformed_local_log_after_remote_success(self):
        completed = self.invoke(
            "both",
            local_text="not json",
            remote_text='{"timestamp":"2026-01-02T01:02:03Z","level":"WARN","target":"app","fields":{"message":"remote warning"}}\n',
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("parse error", completed.stderr)
        self.assertIn("remote warning", completed.stdout)
        self.assertNotIn("(no errors)", completed.stdout)


if __name__ == "__main__":
    unittest.main()
