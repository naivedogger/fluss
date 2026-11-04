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

"""Published-package compatibility and validation before execution."""
import io
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import download
import run


class DownloadTest(unittest.TestCase):
    def setUp(self):
        run.WORK.mkdir(parents=True, exist_ok=True)
        temporary = tempfile.TemporaryDirectory(dir=run.WORK)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.ref = "a" * 40
        self.archive = self.root / "published.tar.gz"
        self.files = {
            "partition/run.py": Path(run.__file__).read_bytes(),
            "bin/partition_e2e": ("#!" + sys.executable + "\nprint('fixture readback')\n").encode(),
        }
        self.metadata = dict(artifactSchemaVersion=1, sdkRevision=self.ref, harnessRevision="b" * 40,
                             sdkRequestedRef="codex/cpp-partition-e2e", platform="linux-x86_64",
                             glibcVersion="2.35", partitionE2e=dict(
            binarySha256=download.hashlib.sha256(self.files["bin/partition_e2e"]).hexdigest(),
            runnerSha256=download.hashlib.sha256(self.files["partition/run.py"]).hexdigest()))
        self.write_archive()
        self.addCleanup(patch.stopall)
        patch.object(download, "host", return_value="linux-x86_64").start()
        patch.object(download, "glibc", return_value="2.35").start()

    def write_archive(self):
        self.files["build-metadata.json"] = json.dumps(self.metadata).encode()
        with tarfile.open(self.archive, "w:gz") as archive:
            for name, content in self.files.items():
                entry = tarfile.TarInfo(name)
                entry.size = len(content)
                archive.addfile(entry, io.BytesIO(content))
        self.index = dict(schemaVersion=1, sdkRef=self.ref, sha256=download.sha256(self.archive),
                          size=self.archive.stat().st_size,
                          expandedBytes=sum(map(len, self.files.values())))

    def fetch(self, argv, log, timeout):
        self.assertEqual(argv[:3], ["ossutil", "-c", str(self.root / "oss-config")])
        target = Path(argv[-1])
        if target.name == "artifact-index.json":
            target.write_text(json.dumps(self.index))
        else:
            shutil.copyfile(self.archive, target)

    def prepare(self, name):
        output = self.root / name
        output.mkdir()
        download.prepare(self.ref, output, self.root / "oss-config", self.fetch)
        return output

    def test_download_runs_verified_bundled_runner_with_existing_config(self):
        output = self.prepare("success")
        config = self.root / "config.json"
        config.write_text(json.dumps(dict(endpoint="fixture")))
        runtime = output / "runtime"
        subprocess.run([sys.executable, str(runtime / "partition/run.py"), "run",
                        "--runtime", str(runtime), "--work-dir", str(output),
                        "--config", str(config), "--server-version", "fixture",
                        "--cases", "log"], check=True, capture_output=True, text=True)
        summary = json.loads(next((output / "results").glob("*/summary.json")).read_text())
        self.assertEqual(summary["sdk"]["commit"], self.ref)
        self.assertEqual(summary["results"][0]["status"], "passed")

    def test_rejects_bad_digest_runner_and_archive_path(self):
        self.index["sha256"] = "0" * 64
        with self.assertRaisesRegex(ValueError, "checksum"):
            self.prepare("digest")
        self.metadata["partitionE2e"]["runnerSha256"] = "0" * 64
        self.write_archive()
        with self.assertRaisesRegex(ValueError, "partition runtime checksum"):
            self.prepare("runner")
        self.files["../escape"] = b"bad path"
        self.write_archive()
        with self.assertRaisesRegex(ValueError, "invalid runtime artifact entry"):
            self.prepare("path")


if __name__ == "__main__":
    unittest.main()
