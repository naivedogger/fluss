#!/usr/bin/env python3
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

"""Build and run one checkout against a test cluster; no automatic table deletion."""
import argparse
import datetime as dt
import json
import os
from pathlib import Path
import re
import signal
import subprocess as sp
import time
import uuid

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[4]


def config(path, database=None):
    old = json.loads(path.read_text())
    # Ignore unrelated fields in the existing benchmark configuration.
    c = dict(
        endpoint=old["endpoint"],
        security_protocol=old.get("security_protocol", "plaintext"),
        sasl_mechanism=old.get("sasl_mechanism", "PLAIN"),
        username=old.get("username", ""),
        password=old.get("password", ""),
        database=database or old.get("database", "fluss"),
        replication_factor=old.get("replication_factor", 2),
        payload_bytes=old.get("payload_bytes", 1024),
        target_rows_per_second=old.get("target_rows_per_second", 400000),
    )
    for key in ("payload_bytes", "target_rows_per_second"):
        if type(c[key]) is not int or not 0 < c[key] <= 2147483647:
            raise ValueError(key + " must be a positive 32-bit integer")
    return c


def build(args):
    env = dict(os.environ, CARGO_BUILD_JOBS=str(args.jobs))
    env.pop("CARGO_TARGET_DIR", None)
    sp.run(
        [
            "cmake",
            "-S",
            str(HERE),
            "-B",
            str(HERE / "build"),
            "-DCMAKE_BUILD_TYPE=Release",
            "-DFLUSS_CPP_DEP_MODE=build",
            "-DFLUSS_CPP_ARROW_SOURCE_URL=" + str(args.arrow.resolve()),
            "-DCARGO_BUILD_FLAGS=--features=file-logging;--locked",
        ],
        env=env,
        check=True,
    )
    sp.run(
        ["cmake", "--build", str(HERE / "build"), "--target", "ooo_bench", "-j" + str(args.jobs)],
        env=env,
        check=True,
    )


def call(mode, table, directory, env, timeout):
    with (directory / (mode + ".log")).open("w") as f:
        p = sp.Popen(
            [str(HERE / "build/ooo_bench"), mode, table, str(directory), "0"],
            env=env,
            stdout=f,
            stderr=sp.STDOUT,
            start_new_session=True,
        )
        try:
            if p.wait(timeout=timeout):
                raise RuntimeError(mode + " failed; see " + str(directory / (mode + ".log")))
        finally:
            terminate(p)


def run_writers(table, directory, env, warm, measure):
    processes, handles = [], []
    last_sample = 0

    def health():
        nonlocal last_sample
        for i, p in enumerate(processes):
            if p.poll() not in (None, 0):
                raise RuntimeError("producer failed; see producer-" + str(i) + ".log")
        if time.monotonic() - last_sample >= 5:
            last_sample = time.monotonic()
            rss = 0
            for p in processes:
                try:
                    status = Path(f"/proc/{p.pid}/status").read_text()
                    m = re.search(r"^VmRSS:\s+(\d+)", status, re.M)
                    if m:
                        rss += int(m[1])
                except FileNotFoundError:
                    pass
            with (directory / "resources.jsonl").open("a") as f:
                f.write(
                    json.dumps(dict(unix_ms=int(time.time() * 1000), producer_rss_kib=rss)) + "\n"
                )

    def wait_until(predicate, timeout):
        deadline = time.monotonic() + timeout
        while not predicate():
            health()
            if all(p.poll() is not None for p in processes):
                raise RuntimeError("producers exited before completing the stage")
            if time.monotonic() > deadline:
                raise TimeoutError("producer stage timed out; logs and table retained")
            time.sleep(0.25)
        health()

    try:
        for i in range(4):
            f = (directory / f"producer-{i}.log").open("w")
            handles.append(f)
            processes.append(
                sp.Popen(
                    [str(HERE / "build/ooo_bench"), "producer", table, str(directory), str(i)],
                    env=env,
                    stdout=f,
                    stderr=sp.STDOUT,
                    start_new_session=True,
                )
            )
        wait_until(lambda: all((directory / f"ready-{i}").exists() for i in range(4)), 240)
        for phase, duration in [("warmup", warm), ("measurement", measure)]:
            marker = directory / phase
            marker.with_suffix(".tmp").write_text(str(int(time.time() * 1000) + 1500))
            marker.with_suffix(".tmp").replace(marker)
            print("STAGE", phase, "seconds", duration, flush=True)
            wait_until(
                lambda: all((directory / f"done-{phase}-{i}").exists() for i in range(4)),
                duration + 302,
            )
        wait_until(lambda: all(p.poll() is not None for p in processes), 30)
    finally:
        for p in processes:
            terminate(p)
        for f in handles:
            f.close()


def write_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def env_for(c, directory, warm, measure, rate):
    e = dict(os.environ)
    for k in (
        "endpoint",
        "security_protocol",
        "sasl_mechanism",
        "username",
        "password",
        "database",
    ):
        e["OOO_" + k.upper()] = c[k]
    e.update(
        OOO_RF=str(c["replication_factor"]),
        OOO_LOG_DIR=str(directory / "sdk-logs"),
        OOO_WARMUP_SECONDS=str(warm),
        OOO_MEASURE_SECONDS=str(measure),
        OOO_RATE=str(rate),
        OOO_PAYLOAD_BYTES=str(c["payload_bytes"]),
        OOO_VERIFY_SECONDS="3600",
    )
    return e


def records(path, prefix):
    if not path.exists():
        return []
    return [
        json.loads(line[len(prefix) :])
        for line in path.read_text(errors="replace").splitlines()
        if line.startswith(prefix)
    ]


def terminate(p):
    if p.poll() is not None:
        return
    try:
        os.killpg(p.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        p.wait(timeout=5)
    except sp.TimeoutExpired:
        try:
            os.killpg(p.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        p.wait()


def logger_summary(directory):
    stats = [row for p in directory.glob("*.log") for row in records(p, "LOG_STATS ")]
    files = list((directory / "sdk-logs").rglob("client*.log"))
    patterns = {
        "ooo_lines": re.compile(
            r"OutOfOrderSequence|Out of order batch sequence|OUT_OF_ORDER_SEQUENCE|\(code 31\)",
            re.I,
        ),
        "reset_lines": re.compile(r"reset.*writer|writer.*reset", re.I),
    }
    counts = {k: 0 for k in patterns}
    for path in files:
        with path.open(errors="replace") as f:
            for line in f:
                for key, pattern in patterns.items():
                    counts[key] += bool(pattern.search(line))
    # Conservatively flag a full archive window: some earlier events may have been expired.
    full_window = any(p.name == "client.7.log" for p in files)
    return dict(
        stats=stats,
        retained_files=len(files),
        possible_retention_loss=full_window,
        keyword_line_counts=counts,
        note="Keyword lines are not unique server rejects; inspect numeric error codes too.",
    )


def aggregate(directory, reports):
    summary = {}
    for phase in ("warmup", "measurement"):
        rows = [next(row for row in reports[str(i)] if row["phase"] == phase) for i in range(4)]
        seconds = max(row["secondsIncludingDrain"] for row in rows)
        hist = {}
        for i in range(4):
            for line in (directory / f"{phase}-hist-{i}").read_text().splitlines():
                bucket, count = map(int, line.split())
                hist[bucket] = hist.get(bucket, 0) + count
        samples = sum(hist.values())

        def percentile(q):
            if not samples:
                return None
            cumulative = 0
            import math

            target = math.ceil(samples * q)
            for bucket, count in sorted(hist.items()):
                cumulative += count
                if cumulative >= target:
                    return bucket

        completed = sum(row["completed"] for row in rows)
        summary[phase] = dict(
            accepted=sum(row["accepted"] for row in rows),
            completed=completed,
            errors=sum(row["errors"] for row in rows),
            seconds_including_drain=seconds,
            ack_rows_per_second_including_drain=completed / seconds,
            sampled_p50_upper_ms=percentile(0.5),
            sampled_p99_upper_ms=percentile(0.99),
            samples=samples,
        )
    return summary


def remote_evidence(directory):
    # Count chunk observations, not bytes or completed downloads. Only use this readback's logs.
    schemes = {}
    for path in (directory / "sdk-logs").rglob("client*.log"):
        with path.open(errors="replace") as f:
            for line in f:
                match = re.search(
                    r"Remote log streaming download: chunk #\d+ \(([A-Za-z][A-Za-z0-9+.-]*)://",
                    line,
                )
                if match:
                    scheme = match[1].lower()
                    schemes[scheme] = schemes.get(scheme, 0) + 1
    return schemes


def readback(args, c):
    if args.run_dir is None:
        raise ValueError("readback requires --run-dir pointing to the original run")
    source = args.run_dir.resolve()
    original = json.loads((source / "manifest.json").read_text())
    previous = json.loads((source / "result.json").read_text())
    if previous.get("status") != "completed":
        raise ValueError("use a completed original run with intact counts files")
    if c["database"] != original["database"]:
        raise ValueError("database differs from original run; use --database")
    setup = records(source / "setup.log", "META ")
    if len(setup) != 1:
        raise ValueError("original setup.log with table ID is required")
    for i in range(4):
        counts = [int(n) for n in (source / f"counts-{i}").read_text().split()]
        if len(counts) != 5 or any(n < 0 or n > 100000000 for n in counts):
            raise ValueError("invalid original counts file")
    directory = source / ("readback_" + uuid.uuid4().hex[:12])
    directory.mkdir()
    revision = sp.check_output(["git", "-C", str(REPO), "rev-parse", "HEAD"], text=True).strip()
    write_json(
        directory / "manifest.json",
        dict(
            mode="readback",
            source_run=str(source),
            table=original["table"],
            database=original["database"],
            table_id=setup[0]["tableId"],
            payload_bytes=original["payload_bytes"],
            checkout_sha=revision,
            source_checkout_sha=original.get("checkout_sha"),
            start_utc=dt.datetime.now(dt.timezone.utc).isoformat(),
        ),
    )
    # The current config's workload may have changed since the table was written.
    c = dict(c, payload_bytes=original["payload_bytes"])
    env = env_for(c, directory, 0, 0, 1)
    env.update(
        OOO_COUNTS_DIR=str(source),
        OOO_TABLE_ID=str(setup[0]["tableId"]),
        OOO_LOG_FILTER="warn,fluss::client::write::idempotence=debug,"
        "fluss::client::table::remote_log=debug",
    )
    print("READBACK_DIR", directory, flush=True)
    result = dict(status="failed")
    try:
        call("verify", original["table"], directory, env, 3610)
        verify = records(directory / "verify.log", "VERIFY ")
        if len(verify) != 1 or not verify[0]["success"]:
            raise RuntimeError("readback failed; inspect verify.log")
        result["verify"] = verify[0]
        seconds = verify[0]["seconds"]
        result["scan_rows_per_second"] = verify[0]["read"] / seconds if seconds else None
        result["scan_payload_mib_per_second"] = (
            verify[0]["read"] * original["payload_bytes"] / seconds / 1024**2 if seconds else None
        )
        logs = logger_summary(directory)
        if len(logs["stats"]) != 1 or logs["possible_retention_loss"]:
            raise RuntimeError("readback log evidence incomplete")
        stats = logs["stats"][0]
        if (
            any(stats[k] for k in ("dropped", "truncated", "write_errors"))
            or stats["accepted"] != stats["written"]
        ):
            raise RuntimeError("readback logger lost records")
        schemes = remote_evidence(directory)
        if schemes.get("ossj"):
            result["status"] = "ossj_observed"
        elif schemes.get("oss"):
            result["status"] = "oss_observed"
        else:
            result["status"] = "other_remote_observed" if schemes else "remote_not_observed"
    finally:
        result["verify_records"] = records(directory / "verify.log", "VERIFY ")
        result["logs"] = logger_summary(directory)
        result["download_chunk_log_lines_by_scheme"] = remote_evidence(directory)
        result["note"] = (
            "Chunk logs prove download activity, not exact remote bytes or consumed offsets. "
            "Scan rates include mixed local/remote readback and ID verification; not pure OSS bandwidth."
        )
        write_json(directory / "result.json", result)
    print(json.dumps(result, indent=2), flush=True)
    if result["status"] == "remote_not_observed":
        print("INCONCLUSIVE: ID readback passed, but no remote download was observed.")
        raise SystemExit(2)
    print("DONE: ID readback passed; observed scheme:", result["status"])


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["build", "check", "smoke", "run", "readback"])
    parser.add_argument("--config", type=Path, default=HERE / "config.json")
    parser.add_argument("--run-dir", type=Path, help="original completed run for readback")
    parser.add_argument(
        "--database", help="existing database; defaults to config.database or fluss"
    )
    parser.add_argument(
        "--arrow",
        type=Path,
        default=Path("/root/cpp-test/deps/arrow/19.0.1/apache-arrow-19.0.1.tar.gz"),
    )
    parser.add_argument("-j", "--jobs", type=int, default=4)
    args = parser.parse_args()
    if args.command == "build":
        build(args)
        return
    c = config(args.config, args.database)
    if args.command == "readback":
        readback(args, c)
        return
    warm, measure, rate = (
        (2, 5, 1000) if args.command == "smoke" else (60, 240, c["target_rows_per_second"])
    )
    name = "ooo_" + uuid.uuid4().hex[:12]
    directory = HERE / "runs" / name
    directory.mkdir(parents=True)
    revision = sp.check_output(["git", "-C", str(REPO), "rev-parse", "HEAD"], text=True).strip()
    write_json(
        directory / "manifest.json",
        dict(
            table=name,
            database=c["database"],
            checkout_sha=revision,
            mode=args.command,
            start_utc=dt.datetime.now(dt.timezone.utc).isoformat(),
            processes=4,
            writers_per_process=5,
            payload_bytes=c["payload_bytes"],
            buckets=32,
            replication_factor=c["replication_factor"],
            warmup_seconds=warm,
            measurement_seconds=measure,
            target_rate=rate,
        ),
    )
    print("RUN_DIR", directory, flush=True)
    env = env_for(c, directory, warm, measure, rate)
    result = {}
    try:
        call("check", name, directory, env, 120)
        if args.command == "check":
            result["status"] = "completed"
            print("CHECK PASSED: connection/authentication and database exist; no table created.")
            return
        call("setup", name, directory, env, 240)
        run_writers(name, directory, env, warm, measure)
        reports = {str(i): records(directory / f"producer-{i}.log", "RESULT ") for i in range(4)}
        result["summary"] = aggregate(directory, reports)
        result["reports"] = reports
        print(json.dumps(result["summary"], indent=2), flush=True)
        call("verify", name, directory, env, 3610)
        result["verify"] = records(directory / "verify.log", "VERIFY ")
        if len(result["verify"]) != 1 or not result["verify"][0]["success"]:
            raise RuntimeError("readback failed; see verify.log")
        result["status"] = "completed"
    except BaseException:
        result["status"] = "failed"
        raise
    finally:
        result["logs"] = logger_summary(directory)
        write_json(directory / "result.json", result)
        print("Client log counts:", result["logs"]["keyword_line_counts"], flush=True)
        if args.command != "check":
            print("Table retained:", c["database"] + "." + name, flush=True)
    logs = result["logs"]
    if len(logs["stats"]) != 7 or logs["possible_retention_loss"]:
        result["status"] = "incomplete_logs"
        write_json(directory / "result.json", result)
        raise RuntimeError("log evidence incomplete; inspect result.json")
    print(
        "DONE: performance and readback in result.json; OOO counts are client log lines, not server counts."
    )


if __name__ == "__main__":
    try:
        main()
    except (Exception, KeyboardInterrupt) as error:
        # SDK errors stay in local log files; do not print config or environment.
        raise SystemExit(str(error))
