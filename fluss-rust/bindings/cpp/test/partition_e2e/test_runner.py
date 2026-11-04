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

"""Published-runtime loading and native process deadline regressions."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

import run


class RunnerTest(unittest.TestCase):
    def test_published_runner_works_without_source_checkout_and_rejects_wrong_binary(self):
        run.WORK.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=run.WORK) as work:
            root = Path(work)
            runtime = root / "runtime"
            (runtime / "bin").mkdir(parents=True)
            (runtime / "partition").mkdir()
            script = runtime / "partition/run.py"
            shutil.copyfile(run.__file__, script)
            binary = runtime / "bin/partition_e2e"
            binary.write_text("#!" + sys.executable + "\nprint('fixture readback')\n")
            binary.chmod(0o700)
            metadata = dict(sdkRevision="a" * 40, harnessRevision="b" * 40, partitionE2e=dict(
                binarySha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                runnerSha256=hashlib.sha256(script.read_bytes()).hexdigest()))
            (runtime / "build-metadata.json").write_text(json.dumps(metadata))
            config = root / "config.json"
            config.write_text(json.dumps(dict(endpoint="fixture")))
            command = [sys.executable, str(script), "run", "--runtime", str(runtime),
                       "--work-dir", str(root / "output"), "--config", str(config),
                       "--server-version", "fixture", "--cases", "log"]
            subprocess.run(command, check=True, capture_output=True, text=True)
            summaries = list((root / "output/results").glob("*/summary.json"))
            summary = json.loads(summaries[0].read_text())
            self.assertEqual(summary["sdk"]["commit"], metadata["sdkRevision"])
            self.assertEqual(summary["results"][0]["status"], "passed")
            metadata_command = command[:-1] + ["metadata-late-response"]
            missing = subprocess.run(metadata_command, capture_output=True, text=True)
            self.assertNotEqual(missing.returncode, 0)
            self.assertIn("require a published runtime with metadataE2e", missing.stderr)
            native = runtime / "bin/metadata_e2e"
            native.write_text("#!" + sys.executable + "\nimport sys\n"
                              "assert sys.argv[1:] == ['--ignored', '--exact', "
                              "'client::metadata::live_tests::metadata_late_response', "
                              "'--nocapture', '--test-threads=1']\n"
                              "print('test result: ok. 1 passed; 0 failed;')\n")
            native.chmod(0o700)
            for count in (1, 0):
                native.write_text(native.read_text().replace("1 passed", str(count) + " passed"))
                metadata["metadataE2e"] = dict(binarySha256=hashlib.sha256(native.read_bytes()).hexdigest())
                (runtime / "build-metadata.json").write_text(json.dumps(metadata))
                result = subprocess.run(metadata_command, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0 if count else 1, result.stderr)
            binary.write_text("wrong artifact")
            rejected = subprocess.run(command, capture_output=True, text=True)
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn("differs from the published runtime", rejected.stderr)

    def test_timeout_is_reported_and_next_case_can_run(self):
        run.WORK.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=run.WORK) as work:
            directory = Path(work)
            fake = directory / "native"
            fake.write_text("#!" + sys.executable + "\n"
                            "import sys, time\n"
                            "if sys.argv[1] == 'hang': time.sleep(60)\n")
            fake.chmod(0o700)
            hung = directory / "hung"
            hung.mkdir()
            env = dict(os.environ, PATH="")  # No debugger needed for this runner regression.
            result = run.execute(fake, "hang", hung, env, 0.1)
            self.assertEqual(result["status"], "timeout")
            pid = int((hung / "pid").read_text())
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)
            next_case = directory / "next"
            next_case.mkdir()
            result = run.execute(fake, "log", next_case, env, 5)
            self.assertEqual(result["status"], "passed")


if __name__ == "__main__":
    unittest.main()
