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

import upgrade


class UpgradeTest(unittest.TestCase):
    def test_live_rate_pause_resume_and_request_history(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            upgrade.write_json(root / "manifest.json", dict(rate_control_version=1))
            upgrade.write_json(root / "state.json", dict(stage="upgrading"))
            for rate in (100, 0, 500):
                upgrade.set_rate(root, rate)
                request, actual = map(int, (root / "write-rate").read_text().split())
                history = [
                    json.loads(line)
                    for line in (root / "rate-changes.jsonl").read_text().splitlines()
                ]
                self.assertEqual(actual, rate)
                self.assertEqual(history[-1]["request_id"], request)
                self.assertEqual(history[-1]["write_mib"], rate)
            self.assertEqual([r["write_mib"] for r in history], [100, 0, 500])
            self.assertFalse((root / "write-rate.tmp").exists())

    def test_rate_command_rejects_old_runs_invalid_rates_and_inactive_stages(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            upgrade.write_json(root / "state.json", dict(stage="upgrading"))
            upgrade.write_json(root / "manifest.json", {})
            with self.assertRaisesRegex(ValueError, "does not support"):
                upgrade.set_rate(root, 100)
            upgrade.write_json(root / "manifest.json", dict(rate_control_version=1))
            for rate in (None, -1, 1048577, 1.5):
                with self.subTest(rate=rate), self.assertRaises(ValueError):
                    upgrade.set_rate(root, rate)
            for stage in ("setup", "draining", "verifying", "completed", "failed"):
                upgrade.write_json(root / "state.json", dict(stage=stage))
                with self.subTest(stage=stage), self.assertRaises(ValueError):
                    upgrade.set_rate(root, 100)
            upgrade.write_json(root / "state.json", dict(stage="upgrading"))
            (root / "stop").touch()
            with self.assertRaises(ValueError):
                upgrade.set_rate(root, 100)
            self.assertFalse((root / "write-rate").exists())

    def test_phase_rates_histograms_and_stall_are_windowed(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            events = dict(
                started=0,
                prefill_done=1000,
                baseline_done=5000,
                upgrade_started=6000,
                upgrade_done=10000,
                load_stopped=14000,
            )
            for i in range(4):
                rows = [
                    dict(
                        unix_ms=t,
                        completed=t + i,
                        no_ack_ms=t % 3000,
                        latency_hist_ms=[[10 if t < 6000 else 900, 2]],
                    )
                    for t in range(0, 15000, 1000)
                ]
                (root / f"writer-{i}.log").write_text(
                    "".join("SAMPLE " + json.dumps(r) + "\n" for r in rows)
                )
            result = upgrade.summarize(root, events, 1024)
            self.assertEqual(result["baseline"]["writer"]["contributing_processes"], 4)
            self.assertEqual(result["baseline"]["writer"]["p99_ms"], 10)
            self.assertEqual(result["upgrade"]["writer"]["p99_ms"], 900)
            self.assertEqual(result["upgrade"]["writer"]["max_no_ack_ms"], 2000)
            self.assertEqual(result["upgrade"]["writer"]["mib_per_second"], 4000 / 1024)
            self.assertIsNone(result["prefill"]["reader"]["mib_per_second"])

    def test_markers_are_ordered_and_cannot_overwrite_times(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            upgrade.write_json(root / "state.json", dict(stage="baseline"))
            with self.assertRaises(ValueError):
                upgrade.mark(root, "upgrade.started")
            upgrade.write_json(root / "state.json", dict(stage="ready_for_upgrade"))
            upgrade.mark(root, "upgrade.started")
            stamp = (root / "upgrade.started").read_text()
            with self.assertRaises(FileExistsError):
                upgrade.mark(root, "upgrade.started")
            self.assertEqual((root / "upgrade.started").read_text(), stamp)
            with self.assertRaises(ValueError):
                upgrade.mark(root, "upgrade.done")
            upgrade.write_json(root / "state.json", dict(stage="upgrading"))
            upgrade.mark(root, "upgrade.done")
            self.assertGreaterEqual(int((root / "upgrade.done").read_text()), int(stamp))


if __name__ == "__main__":
    unittest.main()
