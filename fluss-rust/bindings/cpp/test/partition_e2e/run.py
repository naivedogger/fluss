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

"""Run small partition correctness cases against an existing ECS test cluster."""
import argparse
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time
import uuid

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[4]
WORK = REPO / "target" / "cpp-partition-e2e"
CASES = ("log", "arrow", "pk", "lifecycle", "missing-log", "missing-pk", "float-name")
FAULTS = ("scanner-drop", "scanner-control", "lookup-drop", "lookup-control")


def execute(binary, case, directory, env, timeout):
    with (directory / "stdout.log").open("w") as stdout, (directory / "stderr.log").open("w") as stderr:
        process = subprocess.Popen(
            [str(binary), case], cwd=directory, env=env, stdout=stdout, stderr=stderr,
            start_new_session=True,
        )
        (directory / "pid").write_text(str(process.pid) + "\n")
        started = time.monotonic()
        timed_out = False
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            # Capture native and Rust worker stacks before terminating the case.
            gdb = shutil.which("gdb") if sys.platform.startswith("linux") else None
            if gdb:
                with (directory / "threads.log").open("w") as trace:
                    try:
                        subprocess.run(
                            [gdb, "-batch", "-ex", "set pagination off",
                             "-ex", "thread apply all bt", "-p", str(process.pid)],
                            stdout=trace, stderr=subprocess.STDOUT, timeout=15, check=False,
                        )
                    except (subprocess.TimeoutExpired, OSError) as error:
                        trace.write("\ngdb unavailable: " + str(error) + "\n")
            code = -1
        finally:
            # Also clean up on Ctrl-C; do not leave a writer running after the runner exits.
            if process.poll() is None:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait()
        status = "timeout" if timed_out else ("passed" if code == 0 else "failed")
        if code == 0 and case in FAULTS:
            status = "needs-evidence"
        return dict(case=case, status=status, exit_code=code,
                    seconds=round(time.monotonic() - started, 3))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    build = sub.add_parser("build")
    build.add_argument("--arrow", type=Path,
                       default=Path("/root/cpp-test/deps/arrow/19.0.1/apache-arrow-19.0.1.tar.gz"))
    build.add_argument("--arrow-dir", type=Path, help="Use an installed Arrow CMake config instead")
    build.add_argument("-j", "--jobs", type=int, default=4)
    run = sub.add_parser("run")
    run.add_argument("--config", type=Path, required=True)
    run.add_argument("--cases", nargs="+", choices=CASES + FAULTS, default=list(CASES))
    run.add_argument("--rows", type=int, default=1000, help="Rows per partition for basic cases")
    run.add_argument("--timeout", type=int, default=120, help="Whole-process deadline per automatic case")
    run.add_argument("--fault-timeout", type=int, default=600, help="Includes operator intervention")
    run.add_argument("--buckets", type=int, default=4)
    run.add_argument("--lookup-retries", type=int, default=2147483647)
    run.add_argument("--keep", action="store_true", help="Keep successful automatic case tables too")
    run.add_argument("--database", help="Override the existing config database")
    run.add_argument("--server-version", required=True, help="Record the deployed server version/build")
    args = parser.parse_args()
    WORK.mkdir(parents=True, exist_ok=True)
    tmp = WORK / "tmp"
    tmp.mkdir(exist_ok=True)
    env = dict(os.environ, TMPDIR=str(tmp))
    if args.command == "build":
        if args.jobs <= 0:
            parser.error("--jobs must be positive")
        command = ["cmake", "-S", str(HERE), "-B", str(WORK / "build"),
                   "-DCMAKE_BUILD_TYPE=Release", "-DCARGO_BUILD_FLAGS=--locked"]
        if args.arrow_dir:
            command += ["-DFLUSS_CPP_DEP_MODE=system", "-DArrow_DIR=" + str(args.arrow_dir.resolve())]
        else:
            if not args.arrow.is_file():
                parser.error("Arrow archive missing; pass --arrow or --arrow-dir")
            command += ["-DFLUSS_CPP_DEP_MODE=build",
                        "-DFLUSS_CPP_ARROW_SOURCE_URL=" + str(args.arrow.resolve())]
        subprocess.run(command, env=env, check=True)
        subprocess.run(["cmake", "--build", str(WORK / "build"), "--target",
                        "partition_e2e", "-j", str(args.jobs)], env=env, check=True)
        revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=REPO, text=True).strip()
        dirty = bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=REPO, text=True))
        fingerprint = hashlib.sha256((WORK / "build" / "partition_e2e").read_bytes()).hexdigest()
        (WORK / "build" / "revision.json").write_text(
            json.dumps(dict(commit=revision, dirty=dirty, binary_sha256=fingerprint)))
        return 0

    if not 0 < args.rows <= 100000 or not 0 < args.buckets <= 128:
        parser.error("rows must be 1..100000 and buckets 1..128")
    if args.timeout <= 0 or args.fault_timeout <= 0:
        parser.error("timeouts must be positive")
    if len(set(args.cases)) != len(args.cases):
        parser.error("case names must not repeat")
    if not 0 <= args.lookup_retries <= 2147483647:
        parser.error("lookup retries must be 0..2147483647")
    binary = WORK / "build" / "partition_e2e"
    if not binary.is_file():
        parser.error("build first")
    config = json.loads(args.config.read_text())
    replication = config.get("replication_factor", 3)
    if type(replication) is not int or replication < 1:
        parser.error("replication_factor must be a positive integer")
    run_id = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "_" + uuid.uuid4().hex[:8]
    result_dir = WORK / "results" / run_id
    result_dir.mkdir(parents=True)
    env.update(
        PART_ENDPOINT=config["endpoint"],
        PART_DATABASE=args.database or config.get("database", "fluss"),
        PART_SECURITY=config.get("security_protocol", "plaintext"),
        PART_MECHANISM=config.get("sasl_mechanism", "PLAIN"),
        PART_USERNAME=config.get("username", ""),
        PART_PASSWORD=config.get("password", ""),
        PART_REPLICATION=str(replication), PART_ROWS=str(args.rows),
        PART_BUCKETS=str(args.buckets), PART_KEEP="1" if args.keep else "0",
        PART_LOOKUP_RETRIES=str(args.lookup_retries),
    )
    manifest = dict(run_id=run_id, server_version=args.server_version,
                    sdk=json.loads((WORK / "build" / "revision.json").read_text()),
                    rows_per_partition=args.rows, buckets=args.buckets,
                    replication_factor=replication, lookup_retries=args.lookup_retries, results=[])
    print("RESULTS " + str(result_dir), flush=True)
    (result_dir / "summary.json").write_text(json.dumps(manifest, indent=2) + "\n")
    for case in args.cases:
        directory = result_dir / case
        directory.mkdir()  # Duplicate case names are rejected before launching another process.
        case_env = dict(env, PART_DIR=str(directory),
                        PART_TABLE="pe_" + run_id.lower() + "_" + case.replace("-", "_"))
        print("START " + case + " logs=" + str(directory), flush=True)
        if case in FAULTS:
            print("Wait for fault.ready, perform leader migration/restart on the test cluster, "
                  "then create recovery.ready only AFTER the cluster has recovered. "
                  "See README for evidence requirements.", flush=True)
        result = execute(binary, case, directory, case_env,
                         args.fault_timeout if case in FAULTS else args.timeout)
        manifest["results"].append(result)
        (result_dir / "summary.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(json.dumps(result), flush=True)
    if any(r["status"] in ("failed", "timeout") for r in manifest["results"]):
        return 1
    return 2 if any(r["status"] == "needs-evidence" for r in manifest["results"]) else 0


if __name__ == "__main__":
    sys.exit(main())
