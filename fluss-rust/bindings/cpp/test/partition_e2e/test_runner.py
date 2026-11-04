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

"""The process deadline must survive a native call that never returns."""
import os
from pathlib import Path
import sys
import tempfile
import unittest

import run


class DeadlineTest(unittest.TestCase):
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
