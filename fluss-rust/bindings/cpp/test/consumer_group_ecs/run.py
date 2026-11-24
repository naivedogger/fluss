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

"""Run each consumer lifecycle in a fresh process against the specified cluster."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import time
import uuid

PHASES = ("seed", "no-commit", "partial", "resume", "append", "tail", "isolated-auto", "admin", "cleanup")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("results"))
    args = parser.parse_args()
    config = json.loads(args.config.read_text())
    endpoint = config["endpoint"]
    if not isinstance(endpoint, str) or not endpoint.strip():
        parser.error("Set endpoint in the cluster config first")
    env = dict(os.environ, CG_ENDPOINT=endpoint,
               CG_DB=config.get("database", "fluss"),
               CG_RF=str(config.get("replication_factor", "")),
               CG_SECURITY=config.get("security_protocol", "plaintext"),
               CG_MECHANISM=config.get("sasl_mechanism", "PLAIN"),
               CG_USER=config.get("username", ""), CG_PASSWORD=config.get("password", ""))
    run_id = uuid.uuid4().hex[:16]
    out = args.output / run_id
    out.mkdir(mode=0o700, parents=True)
    binary = args.binary.resolve(strict=True)
    report = {"table": env["CG_DB"] + ".cg_test_" + run_id, "group": "cpp-cg-" + run_id,
              "phases": {}, "status": "running"}
    print(f"Table {report['table']}; group {report['group']}; report {out}/result.json", flush=True)
    for phase in PHASES:
        start = time.monotonic()
        code = 124
        try:
            with (out / (phase + ".log")).open("w") as log:
                code = subprocess.run([binary, phase, run_id], env=env, stdout=log,
                                      stderr=subprocess.STDOUT, timeout=120).returncode
        except subprocess.TimeoutExpired:
            pass
        report["phases"][phase] = {"exit": code, "seconds": round(time.monotonic()-start, 2)}
        report["status"] = "failed" if code else ("passed" if phase == PHASES[-1] else "running")
        (out / "result.json").write_text(json.dumps(report, indent=2) + "\n")
        print(f"{'FAIL' if code else 'PASS'} {phase}", flush=True)
        if code:
            raise SystemExit(f"See {out}/{phase}.log; test resources retained")


if __name__ == "__main__":
    main()
