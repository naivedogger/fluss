# Licensed to the Apache Software Foundation (ASF) under one
# or more contributor license agreements.  See the NOTICE file
# distributed with this work for additional information
# regarding copyright ownership.  The ASF licenses this file
# to you under the Apache License, Version 2.0 (the
# "License"); you may not use this file except in compliance
# with the License.  You may obtain a copy of the License at
#
#   http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing,
# software distributed under the License is distributed on an
# "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
# KIND, either express or implied.  See the License for the
# specific language governing permissions and limitations
# under the License.

import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from types import SimpleNamespace
import run


class RunnerTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_existing_config_via_symlink(self):
        path = self.root / "existing.json"
        original = dict(
            endpoint="test.invalid:9123",
            security_protocol="sasl",
            sasl_mechanism="PLAIN",
            username="test-user",
            password="test-secret",
            database="benchmark",
            threads=40,
            target_rate=999,
        )
        path.write_text(json.dumps(original))
        link = self.root / "config.json"
        link.symlink_to(path)
        result = run.config(link)
        self.assertEqual(result["endpoint"], original["endpoint"])
        self.assertEqual(result["password"], original["password"])
        self.assertEqual(result["database"], "benchmark")
        self.assertEqual(result["payload_bytes"], 1024)
        self.assertEqual(result["target_rows_per_second"], 400000)
        self.assertNotIn("target_rate", result)
        self.assertEqual(run.config(link, "another")["database"], "another")
        self.assertEqual(json.loads(path.read_text()), original)

    def test_configured_workload_reaches_child_environment(self):
        path = self.root / "config.json"
        path.write_text(
            json.dumps(
                dict(endpoint="test.invalid:9123", payload_bytes=100, target_rows_per_second=4000000)
            )
        )
        c = run.config(path)
        env = run.env_for(c, self.root, 60, 240, c["target_rows_per_second"])
        self.assertEqual(env["OOO_PAYLOAD_BYTES"], "100")
        self.assertEqual(env["OOO_RATE"], "4000000")
        self.assertEqual(run.env_for(c, self.root, 2, 5, 1000)["OOO_RATE"], "1000")

    def test_workload_rejects_invalid_numbers(self):
        path = self.root / "config.json"
        for key in ("payload_bytes", "target_rows_per_second"):
            for value in (0, -1, True, 1.5, "100", 2147483648):
                with self.subTest(key=key, value=value):
                    path.write_text(json.dumps(dict(endpoint="test.invalid:9123", **{key: value})))
                    with self.assertRaises(ValueError):
                        run.config(path)

    def test_logger_summary_counts_keywords_and_numeric_code_and_warns_on_retention(self):
        log = self.root / "sdk-logs" / "group"
        log.mkdir(parents=True)
        (log / "client.7.log").write_text(
            "OutOfOrderSequenceException\nWrite failed: x (code 31)\nResetting writer ID\n"
        )
        stats = dict(accepted=3, written=3, dropped=0, truncated=0, write_errors=0)
        (self.root / "producer-0.log").write_text("LOG_STATS " + json.dumps(stats) + "\n")
        result = run.logger_summary(self.root)
        self.assertTrue(result["possible_retention_loss"])
        self.assertEqual(result["keyword_line_counts"], dict(ooo_lines=2, reset_lines=1))
        self.assertEqual(result["stats"], [stats])

    def test_readback_evidence_and_original_files(self):
        original = dict(table="ooo_old", database="fluss", payload_bytes=1024)
        run.write_json(self.root / "manifest.json", original)
        run.write_json(self.root / "result.json", dict(status="completed"))
        (self.root / "setup.log").write_text('META {"tableId": 42}\n')
        for i in range(4):
            (self.root / f"counts-{i}").write_text("1 1 1 1 1\n")
        old_logs = self.root / "sdk-logs"
        old_logs.mkdir()
        (old_logs / "client.log").write_text(
            "Remote log streaming download: chunk #1 (ossj://old/segment)\n"
        )
        before = {p: p.read_bytes() for p in self.root.rglob("*") if p.is_file()}
        c = dict(endpoint="test.invalid:9123", security_protocol="plaintext",
                 sasl_mechanism="PLAIN", username="", password="", database="fluss",
                 replication_factor=2, payload_bytes=100)
        for scheme, dropped, failure in [("ossj", 0, False), ("oss", 0, False),
                                         (None, 0, False), ("ossj", 1, False),
                                         ("ossj", 0, True)]:
            with self.subTest(scheme=scheme, dropped=dropped, failure=failure):
                def verify(mode, table, directory, env, timeout):
                    self.assertEqual((mode, table), ("verify", "ooo_old"))
                    self.assertEqual(env["OOO_COUNTS_DIR"], str(self.root.resolve()))
                    self.assertEqual(env["OOO_TABLE_ID"], "42")
                    self.assertEqual(env["OOO_PAYLOAD_BYTES"], "1024")
                    self.assertIn("fluss::client::table::remote_log=debug", env["OOO_LOG_FILTER"])
                    log = directory / "sdk-logs"
                    log.mkdir()
                    (log / "client.log").write_text(
                        f"Remote log streaming download: chunk #1 ({scheme}://test/segment)\n"
                        if scheme else "creating remote reader\n"
                    )
                    stats = dict(accepted=1, written=1, dropped=dropped, truncated=0, write_errors=0)
                    (directory / "verify.log").write_text(
                        'VERIFY {"success": true, "read": 20, "seconds": 2}\n'
                        + "LOG_STATS " + json.dumps(stats) + "\n"
                    )
                    if failure:
                        raise RuntimeError("poll failed")

                with patch.object(run, "call", side_effect=verify) as child, \
                     patch.object(run.sp, "check_output", return_value="test-sha"), \
                     patch("builtins.print"):
                    args = SimpleNamespace(run_dir=self.root)
                    if failure or dropped:
                        with self.assertRaises(RuntimeError):
                            run.readback(args, c)
                    elif not scheme:
                        with self.assertRaises(SystemExit) as error:
                            run.readback(args, c)
                        self.assertEqual(error.exception.code, 2)
                    else:
                        run.readback(args, c)
                    child.assert_called_once()
                result = json.loads((child.call_args.args[2] / "result.json").read_text())
                expected = "failed" if failure or dropped else (
                    scheme + "_observed" if scheme else "remote_not_observed")
                self.assertEqual(result["status"], expected)
                self.assertEqual(result["download_chunk_log_lines_by_scheme"],
                                 {scheme: 1} if scheme else {})
                for path, content in before.items():
                    self.assertEqual(path.read_bytes(), content)

    def test_aggregate_merges_histograms_and_uses_longest_drain_time(self):
        reports = {}
        for i in range(4):
            reports[str(i)] = []
            for phase in ("warmup", "measurement"):
                reports[str(i)].append(
                    dict(
                        phase=phase,
                        accepted=i + 1,
                        completed=i + 1,
                        errors=0,
                        secondsIncludingDrain=i + 1,
                    )
                )
                (self.root / f"{phase}-hist-{i}").write_text(f"{i+1} 1\n")
        result = run.aggregate(self.root, reports)["measurement"]
        self.assertEqual(result["completed"], 10)
        self.assertEqual(result["ack_rows_per_second_including_drain"], 2.5)
        self.assertEqual(result["sampled_p50_upper_ms"], 2)
        self.assertEqual(result["sampled_p99_upper_ms"], 4)


if __name__ == "__main__":
    unittest.main()
