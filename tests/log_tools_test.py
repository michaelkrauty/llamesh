#!/usr/bin/env python3
import gzip
import json
import os
import subprocess
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/log-tools/errors.sh"
INSTANCES_SCRIPT = ROOT / "scripts/log-tools/instances.sh"
DATE = "2026-01-02"
LOG = f"proxy.log.{DATE}"


class LogScriptTestCase(unittest.TestCase):
    """Runs a log-tools script against local and fake-SSH remote log files."""

    script = SCRIPT
    # Arguments after the node and date.
    extra_args = ("2",)

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
            '  run) shift; exec sh -c "$1" ;;\n'
            '  simple) shift; exec python3 -c "import os, shlex, sys; argv = [os.path.expanduser(a) for a in shlex.split(sys.argv[1])]; os.execvp(argv[0], argv)" "$1" ;;\n'
            "  fail) printf '%s\\n' 'simulated ssh failure' >&2; exit 255 ;;\n"
            "esac\n"
        )
        ssh.chmod(0o755)

    def write(self, directory, text):
        (directory / LOG).write_text(text)

    def write_compressed(self, directory, text):
        (directory / f"{LOG}.gz").write_bytes(gzip.compress(text.encode()))

    def invoke(
        self,
        node="local",
        *,
        result="ok",
        host="test-host",
        local_text=None,
        remote_text=None,
        date=DATE,
        tz=None,
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
        if tz is not None:
            env["TZ"] = tz
        return subprocess.run(
            ["bash", str(self.script), node, date, *self.extra_args],
            env=env,
            text=True,
            capture_output=True,
            check=False,
            timeout=5,
        )


class ErrorsScriptTest(LogScriptTestCase):
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
        for node in ("local", "remote", "both"):
            for text in ("", '{"level":"INFO","fields":{"message":"quiet"}}\n'):
                with self.subTest(node=node, empty=not text):
                    completed = self.invoke(node, local_text=text, remote_text=text)
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


    def test_reads_compressed_rotated_logs(self):
        # A rotated file exists only in compressed form when compression is
        # enabled. The remote cases run the script's real SSH command as the
        # remote login shell would: `run` as a POSIX shell, `simple` as a shell
        # such as fish that runs a plain command but not sh syntax.
        line = '{"timestamp":"2026-01-02T01:02:03.123Z","level":"ERROR","target":"app","fields":{"message":"rotated error"}}\n'
        self.write_compressed(self.local, line)
        self.write_compressed(self.remote, line)
        for node, result in (("local", "ok"), ("remote", "run"), ("remote", "simple")):
            with self.subTest(node=node, ssh=result):
                completed = self.invoke(node, result=result)
                self.assertEqual(completed.returncode, 0, completed.stderr)
                self.assertEqual(
                    completed.stdout, "2026-01-02T01:02:03 [ERROR] app: rotated error\n"
                )

    def test_prefers_the_uncompressed_file(self):
        stale = '{"timestamp":"2026-01-02T01:02:03.123Z","level":"ERROR","target":"app","fields":{"message":"compressed copy"}}\n'
        current = '{"timestamp":"2026-01-02T01:02:04.123Z","level":"ERROR","target":"app","fields":{"message":"current file"}}\n'
        for directory in (self.local, self.remote):
            self.write_compressed(directory, stale)
        for node, result in (("local", "ok"), ("remote", "run")):
            with self.subTest(node=node):
                completed = self.invoke(
                    node, result=result, local_text=current, remote_text=current
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)
                self.assertIn("current file", completed.stdout)
                self.assertNotIn("compressed copy", completed.stdout)

    def test_missing_compressed_and_uncompressed_remote_log_fails(self):
        completed = self.invoke("remote", result="run")
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("Log file not found on", completed.stderr)

    def test_shows_the_error_field(self):
        completed = self.invoke(
            local_text='{"timestamp":"2026-01-02T01:02:03.123Z","level":"ERROR","target":"app","fields":{"message":"Local streaming body read failed","error":"connection reset"}}\n'
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(
            completed.stdout,
            "2026-01-02T01:02:03 [ERROR] app: Local streaming body read failed (error: connection reset)\n",
        )

    def test_defaults_to_the_current_utc_day(self):
        # Log files are named by UTC day. Run under an offset that puts the
        # local date on a different day, so a default taken from local time
        # would name the wrong file.
        now = datetime.now(timezone.utc)
        tz, hours = ("<+14>-14", 14) if now.hour >= 12 else ("<-12>+12", -12)
        utc_day = now.strftime("%Y-%m-%d")
        self.assertNotEqual(utc_day, (now + timedelta(hours=hours)).strftime("%Y-%m-%d"))
        (self.local / f"proxy.log.{utc_day}").write_text(
            '{"timestamp":"2026-01-02T01:02:03.123Z","level":"WARN","target":"app","fields":{"message":"today"}}\n'
        )
        completed = self.invoke(date="", tz=tz)
        if datetime.now(timezone.utc).strftime("%Y-%m-%d") != utc_day:
            self.skipTest("the UTC day changed during the test")
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("today", completed.stdout)


def log_line(level, fields, span_model=None):
    entry = {
        "timestamp": "2026-01-02T01:02:03.123Z",
        "level": level,
        "target": "llamesh::node_state",
        "fields": fields,
    }
    if span_model is not None:
        entry["span"] = {"model": span_model, "name": "request"}
    return json.dumps(entry)


class InstancesScriptTest(LogScriptTestCase):
    script = INSTANCES_SCRIPT
    extra_args = ()

    LIFECYCLE = "\n".join(
        [
            log_line("INFO", {"message": "Received request"}, "alpha"),
            log_line(
                "INFO",
                {
                    "message": "Started instance",
                    "event": "instance_spawn",
                    "model": "alpha",
                    "profile": "default",
                    "instance_id": "I1",
                    "port": 8200,
                    "reason": "demand",
                },
            ),
            log_line(
                "INFO",
                {
                    "message": "Instance ready",
                    "event": "instance_ready",
                    "model": "alpha",
                    "profile": "default",
                    "instance_id": "I1",
                    "port": 8200,
                    "startup_ms": 1500,
                },
            ),
            log_line(
                "INFO",
                {
                    "message": "Stopping instance",
                    "event": "instance_terminate",
                    "reason": "idle",
                    "model": "alpha",
                    "profile": "default",
                    "instance_id": "I1",
                    "freed_vram_mb": 900,
                },
            ),
            log_line(
                "ERROR",
                {
                    "message": "Failed to start new instance",
                    "event": "instance_spawn_failed",
                    "port": 8201,
                    "error": "Failed to spawn llama-server",
                },
                "Beta:fast",
            ),
            log_line(
                "WARN",
                {
                    "message": "Instance exited unexpectedly",
                    "event": "instance_exited",
                    "model": "beta",
                    "profile": "fast",
                    "instance_id": "I2",
                    "exit_status": "signal: 9 (SIGKILL)",
                },
            ),
        ]
    )

    def invoke_with(self, node="local", model=None, **options):
        self.extra_args = () if model is None else (model,)
        return self.invoke(node, **options)

    def test_lists_every_instance_event_with_its_details(self):
        completed = self.invoke_with(local_text=self.LIFECYCLE)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(
            completed.stdout,
            "2026-01-02T01:02:03 instance_spawn (demand): model=alpha profile=default instance=I1 port=8200\n"
            "2026-01-02T01:02:03 instance_ready: model=alpha profile=default instance=I1 port=8200 startup_ms=1500\n"
            "2026-01-02T01:02:03 instance_terminate (idle): model=alpha profile=default instance=I1\n"
            "2026-01-02T01:02:03 instance_spawn_failed: model=Beta:fast instance=? port=8201 (error: Failed to spawn llama-server)\n"
            "2026-01-02T01:02:03 instance_exited: model=beta profile=fast instance=I2 exit_status=signal: 9 (SIGKILL)\n",
        )

    def test_filters_by_model_ignoring_case(self):
        completed = self.invoke_with(model="BETA", local_text=self.LIFECYCLE)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertNotIn("alpha", completed.stdout)
        self.assertIn("instance_spawn_failed: model=Beta:fast", completed.stdout)
        self.assertIn("instance_exited: model=beta", completed.stdout)
        # The filter is passed to jq as data, not spliced into the program.
        completed = self.invoke_with(model='"', local_text=self.LIFECYCLE)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stdout, "")

    def test_failures_are_visible_and_not_reported_as_empty(self):
        cases = (
            ("missing local log", "local", {}, "Log file not found"),
            ("ssh failure", "remote", {"result": "fail"}, "simulated ssh failure"),
            ("malformed JSON", "local", {"local_text": "not json"}, "parse error"),
        )
        for name, node, options, expected in cases:
            with self.subTest(name=name):
                completed = self.invoke_with(node, **options)
                self.assertNotEqual(completed.returncode, 0)
                self.assertIn(expected, completed.stderr)
                self.assertNotIn("(none)", completed.stdout)

    def test_both_queries_each_node_and_fails_if_either_fails(self):
        completed = self.invoke_with("both", result="fail", local_text=self.LIFECYCLE)
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("instance_ready: model=alpha", completed.stdout)
        self.assertIn("=== remote instances ===", completed.stdout)
        self.assertIn("simulated ssh failure", completed.stderr)

        completed = self.invoke_with("both", local_text=self.LIFECYCLE, remote_text=self.LIFECYCLE)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stdout.count("instance_ready"), 2)


if __name__ == "__main__":
    unittest.main()
