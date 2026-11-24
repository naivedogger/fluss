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

PHASES = ("seed", "no-commit", "partial", "resume", "append", "tail", "isolated-auto", "split-commit", "split-resume", "cross-arrow")


def credentials(config):
    return dict(CG_SECURITY=config.get("security_protocol", "plaintext"),
                CG_MECHANISM=config.get("sasl_mechanism", "PLAIN"),
                CG_USER=config.get("username", ""), CG_PASSWORD=config.get("password", ""))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("results"))
    parser.add_argument("--owner-move", action="store_true", help="Wait for operator-verified group owner migration")
    parser.add_argument("--acl-config", type=Path, help="Restricted user's connection config for the same cluster")
    parser.add_argument("--reject-metadata-bytes", type=int, default=8192,
                        help="Size above server's configured consumer offset metadata limit")
    args = parser.parse_args()
    if not 0 < args.reject_metadata_bytes <= 1048576:
        parser.error("Choose a metadata size between 1 and 1048576 bytes")
    config = json.loads(args.config.read_text())
    endpoint = config["endpoint"]
    if not isinstance(endpoint, str) or not endpoint.strip():
        parser.error("Set endpoint in the cluster config first")
    env = dict(os.environ, CG_ENDPOINT=endpoint,
               CG_DB=config.get("database", "fluss"),
               CG_RF=str(config.get("replication_factor", "")),
               CG_BAD_BYTES=str(args.reject_metadata_bytes), **credentials(config))
    restricted = {}
    if args.acl_config:
        subject = json.loads(args.acl_config.read_text())
        if not subject.get("username") or subject.get("username") == env["CG_USER"]:
            parser.error("--acl-config must name a distinct restricted user")
        restricted = dict(CG_ENDPOINT=subject.get("endpoint", endpoint), **credentials(subject))
    run_id = uuid.uuid4().hex[:16]
    out = args.output / run_id
    out.mkdir(mode=0o700, parents=True)
    env["CG_OUT"] = str(out.resolve())
    binary = args.binary.resolve(strict=True)
    report = {"table": env["CG_DB"] + ".cg_test_" + run_id, "group": "cpp-cg-" + run_id,
              "split_table": env["CG_DB"] + ".cg_split_" + run_id,
              "optional": {"owner-move": "pending" if args.owner_move else "not_run",
                           "acl-denial": "pending" if restricted else "not_run"},
              "phases": {}, "status": "running"}
    print(f"Table {report['table']}; group {report['group']}; report {out}/result.json", flush=True)
    phases = list(PHASES)
    if args.owner_move:
        phases += ["owner-move", "owner-resume"]
        print(f"After {out}/owner-moved.ready: verify the owner of {report['group']}, move it, "
              f"then publish '<group> <old-node-id> <new-node-id>' in {out}/owner-moved", flush=True)
    if restricted:
        phases += ["acl-denial"]
        print(f"ACL subject: grant table READ on {report['table']} and GROUP DESCRIBE on "
              f"{report['group']}-multi. Publish acl-granted after acl-granted.ready; after "
              f"acl-revoked.ready remove only group access, wait for sync, then publish acl-revoked in {out}.", flush=True)
    phases += ["admin", "cleanup"]
    for phase in phases:
        start = time.monotonic()
        code = 124
        try:
            with (out / (phase + ".log")).open("w") as log:
                code = subprocess.run([binary, phase, run_id], env={**env, **(restricted if phase == "acl-denial" else {})}, stdout=log,
                                      stderr=subprocess.STDOUT, timeout={"acl-denial": 1260, "owner-move": 660}.get(phase, 120)).returncode
        except subprocess.TimeoutExpired:
            pass
        report["phases"][phase] = {"exit": code, "seconds": round(time.monotonic()-start, 2)}
        if phase == "acl-denial" or phase in ("owner-move", "owner-resume"):
            key = "acl-denial" if phase == "acl-denial" else "owner-move"
            report["optional"][key] = "failed" if code else ("pending" if phase == "owner-move" else "passed")
        report["status"] = "failed" if code else ("passed" if phase == phases[-1] else "running")
        (out / "result.json").write_text(json.dumps(report, indent=2) + "\n")
        print(f"{'FAIL' if code else 'PASS'} {phase}", flush=True)
        if code:
            raise SystemExit(f"See {out}/{phase}.log; test resources retained")


if __name__ == "__main__":
    main()
