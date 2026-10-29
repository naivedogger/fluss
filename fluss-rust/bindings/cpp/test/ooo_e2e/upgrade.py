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

"""High-load upgrade test. The operator starts/finishes the cloud upgrade manually."""
import argparse
import fcntl
import json
import math
import os
from pathlib import Path
import signal
import subprocess as sp
import time
import uuid

import run

HERE = Path(__file__).resolve().parent
BINARY = HERE / "build" / "upgrade_bench"


def write_json(path, value):
    temp = path.with_name(path.name + ".tmp")
    run.write_json(temp, value)
    temp.replace(path)


def quantile(hist, q):
    target = math.ceil(sum(hist.values()) * q)
    if not target:
        return None
    for bucket, count in sorted(hist.items()):
        target -= count
        if target <= 0:
            return bucket


def samples(path):
    return run.records(path, "SAMPLE ")


def summarize(directory, events, payload):
    result = {}
    for phase, start, end in zip(
        ("prefill", "baseline", "waiting_for_upgrade", "upgrade", "recovery"),
        (
            events.get(k)
            for k in ("started", "prefill_done", "baseline_done", "upgrade_started", "upgrade_done")
        ),
        (
            events.get(k)
            for k in (
                "prefill_done",
                "baseline_done",
                "upgrade_started",
                "upgrade_done",
                "load_stopped",
            )
        ),
    ):
        if start is None or end is None or end <= start:
            continue
        entry = {}
        for kind, count_key in (("writer", "completed"), ("reader", "bytes")):
            total = 0
            covered = []
            histogram = {}
            no_progress = 0
            for path in directory.glob(kind + "-*.log"):
                all_rows = samples(path)
                rows = [r for r in all_rows if start <= r["unix_ms"] <= end]
                if len(rows) < 2:
                    continue
                # Only count records inside the observed window; do not invent a boundary sample.
                total += rows[-1][count_key] - rows[0][count_key]
                covered.append((rows[-1]["unix_ms"] - rows[0]["unix_ms"]) / 1000)
                for row in rows[1:]:
                    for bucket, n in row.get(
                        "latency_hist_ms" if kind == "writer" else "poll_hist_ms", []
                    ):
                        histogram[bucket] = histogram.get(bucket, 0) + n
                    no_progress = max(no_progress, row.get("no_ack_ms", 0))
            rate = total / max(covered) if covered and max(covered) else None
            entry[kind] = dict(
                mib_per_second=(
                    (rate * (payload if kind == "writer" else 1) / 1024**2)
                    if rate is not None
                    else None
                ),
                p50_ms=quantile(histogram, 0.5),
                p99_ms=quantile(histogram, 0.99),
                max_no_ack_ms=no_progress if kind == "writer" else None,
                contributing_processes=len(covered),
                observed_seconds=max(covered) if covered else 0,
            )
        result[phase] = entry
    return result


def mark(directory, name):
    directory = directory.resolve()
    state = json.loads((directory / "state.json").read_text())
    permitted = {"upgrade.started": "ready_for_upgrade", "upgrade.done": "upgrading"}
    if state["stage"] != permitted[name]:
        raise ValueError("current stage is " + state["stage"] + "; marker is out of order")
    path = directory / name
    temp = directory / (name + "." + uuid.uuid4().hex + ".tmp")
    try:
        temp.write_text(str(int(time.time() * 1000)) + "\n")
        # Publish the complete timestamp atomically, without replacing an earlier marker.
        os.link(temp, path)
    finally:
        temp.unlink()
    print("Marked", name, flush=True)


RATE_STAGES = {"prefill", "baseline", "ready_for_upgrade", "upgrading", "recovery"}
MAX_WRITE_MIB = 1024 * 1024


def set_rate(directory, mib):
    if not isinstance(mib, int) or not 0 <= mib <= MAX_WRITE_MIB:
        raise ValueError("--write-mib must be an integer between 0 and 1048576")
    directory = directory.resolve()
    with (directory / "rate.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        manifest = json.loads((directory / "manifest.json").read_text())
        if manifest.get("rate_control_version") != 1:
            raise ValueError(
                "this run does not support live rate control; rebuild and start a new run"
            )
        state = json.loads((directory / "state.json").read_text())
        if state["stage"] not in RATE_STAGES or (directory / "stop").exists():
            raise ValueError("rate changes are not allowed in stage " + state["stage"])
        request_id = time.time_ns()
        temp = directory / "write-rate.tmp"
        temp.write_text(f"{request_id} {mib}\n")
        with (directory / "rate-changes.jsonl").open("a") as log:
            log.write(
                json.dumps(
                    dict(request_id=request_id, unix_ms=int(time.time() * 1000), write_mib=mib)
                )
                + "\n"
            )
        temp.replace(directory / "write-rate")
    print(
        f"Requested aggregate write rate {mib} MiB/s; request_id={request_id}. "
        "Writers poll once per second; pending writes still drain.",
        flush=True,
    )


def execute(args):
    if args.write_mib is None:
        args.write_mib = 500
    if args.write_mib > MAX_WRITE_MIB:
        raise ValueError("--write-mib must not exceed 1048576")
    if (
        min(
            args.prefill_gib,
            args.write_mib,
            args.read_mib,
            args.baseline_seconds,
            args.recovery_seconds,
            args.max_seconds,
            args.max_gib,
            args.payload_bytes,
            args.buckets,
            args.aux_buckets,
        )
        <= 0
        or args.aux_tables < 0
    ):
        raise ValueError("sizes, rates and durations must be positive")
    if args.readers < 1 or args.readers > args.buckets:
        raise ValueError("readers must be between 1 and buckets")
    if args.max_gib <= args.prefill_gib:
        raise ValueError("max-gib must leave room beyond prefill-gib")
    c = run.config(args.config, args.database)
    c.update(payload_bytes=args.payload_bytes, replication_factor=3)
    name = "upgrade_" + uuid.uuid4().hex[:12]
    directory = HERE / "runs" / name
    directory.mkdir(parents=True)
    write_json(HERE / "runs" / "latest-upgrade.json", dict(run_dir=str(directory)))
    write_json(
        directory / "manifest.json",
        dict(
            table=name,
            database=c["database"],
            mode="upgrade",
            rate_control_version=1,
            payload_bytes=args.payload_bytes,
            checkout_sha=sp.check_output(
                ["git", "-C", str(run.REPO), "rev-parse", "HEAD"], text=True
            ).strip(),
            replication_factor=3,
            required_server_min_isr=2,
            buckets=args.buckets,
            auxiliary_tables=args.aux_tables,
            auxiliary_buckets=args.aux_buckets,
            replicas=(args.aux_tables * args.aux_buckets + args.buckets) * 3,
            prefill_gib=args.prefill_gib,
            write_mib=args.write_mib,
            read_mib=args.read_mib,
            baseline_seconds=args.baseline_seconds,
            recovery_seconds=args.recovery_seconds,
            max_seconds=args.max_seconds,
            max_gib=args.max_gib,
            readers=args.readers,
            source_version=args.source_version,
            target_version=args.target_version,
        ),
    )
    env = run.env_for(c, directory, 0, 0, 1)
    env.update(
        UP_BUCKETS=str(args.buckets),
        UP_PAYLOAD=str(args.payload_bytes),
        UP_AUX_TABLES=str(args.aux_tables),
        UP_AUX_BUCKETS=str(args.aux_buckets),
        UP_WRITE_BPS=str(args.write_mib * 1024**2),
        UP_READ_BPS=str(args.read_mib * 1024**2),
        UP_READERS=str(args.readers),
    )
    children, handles = [], []
    events = {}
    result = dict(status="failed")
    stage = "setup"

    def state(value):
        nonlocal stage
        stage = value
        write_json(directory / "state.json", dict(stage=value, unix_ms=int(time.time() * 1000)))
        print("STAGE", value, flush=True)

    def spawn(mode, index):
        path = directory / (
            f"{mode}-{index}.log" if mode in ("writer", "reader") else mode + ".log"
        )
        handle = path.open("w")
        handles.append(handle)
        cmd_mode = {"writer": "write", "reader": "read"}.get(mode, mode)
        p = sp.Popen(
            [str(BINARY), cmd_mode, name, str(directory), str(index)],
            env=env,
            stdout=handle,
            stderr=sp.STDOUT,
            start_new_session=True,
        )
        children.append((mode, index, p))
        return p

    def health():
        for mode, index, p in children:
            if mode in ("writer", "reader") and p.poll() is not None:
                raise RuntimeError(f"{mode}-{index} exited ({p.returncode}); inspect its log")

    def wait_ready(kind, count):
        deadline = time.monotonic() + 300
        while not all((directory / f"ready-{kind}-{i}").exists() for i in range(count)):
            health()
            if time.monotonic() > deadline:
                raise TimeoutError("client readiness timed out")
            time.sleep(0.5)

    print("UPGRADE_RUN_DIR", directory, flush=True)
    print(
        "Server min ISR must already be 2. This script does not change cloud configuration.",
        flush=True,
    )
    try:
        state("setup")
        spawn("setup", 0).wait(timeout=(args.aux_tables + 1) * 330)
        if children[-1][2].returncode:
            raise RuntimeError("setup failed; inspect setup.log; partially created tables retained")
        tables = run.records(directory / "setup.log", "TABLE ")
        write_json(directory / "tables.json", tables)
        if len(tables) != args.aux_tables + 1:
            raise RuntimeError("setup table list incomplete")
        env["UP_TABLE_ID"] = str(tables[0]["id"])
        for i in range(4):
            spawn("writer", i)
        wait_ready("w", 4)
        for i in range(4):
            if (directory / f"ready-w-{i}").read_text().strip() != "ready rate-control-v1":
                raise RuntimeError("outdated upgrade_bench; run upgrade.py build first")
        events["started"] = int(time.time() * 1000)
        (directory / "start").touch()
        state("prefill")
        last_print = 0
        previous_time, previous_completed, previous_read = time.monotonic(), 0, 0
        print_time, print_completed, print_read = previous_time, 0, 0
        recovery_until = None
        while True:
            health()
            now = int(time.time() * 1000)
            writes = [json.loads(p.read_text()) for p in sorted(directory.glob("writer-*.json"))]
            accepted = sum(s["accepted"] for s in writes)
            completed = sum(s["completed"] for s in writes)
            if now - events["started"] > args.max_seconds * 1000:
                raise TimeoutError("maximum load duration reached")
            if accepted * args.payload_bytes >= args.max_gib * 1024**3:
                raise RuntimeError("maximum logical byte budget reached")
            if stage == "prefill" and completed * args.payload_bytes >= args.prefill_gib * 1024**3:
                events["prefill_done"] = now
                write_json(
                    directory / "prefill.json",
                    dict(ack_rows=completed, ack_bytes=completed * args.payload_bytes),
                )
                for i in range(args.readers):
                    spawn("reader", i)
                wait_ready("r", args.readers)
                events["prefill_done"] = int(time.time() * 1000)
                state("baseline")
            elif (
                stage == "baseline" and now - events["prefill_done"] >= args.baseline_seconds * 1000
            ):
                events["baseline_done"] = now
                state("ready_for_upgrade")
                print(
                    "READY: start the upgrade in the cloud console, then mark-start using this RUN_DIR.",
                    flush=True,
                )
            elif stage == "ready_for_upgrade" and (directory / "upgrade.started").exists():
                events["upgrade_started"] = int((directory / "upgrade.started").read_text())
                state("upgrading")
                print(
                    "After the cloud upgrade completes, run mark-done using this RUN_DIR.",
                    flush=True,
                )
            elif stage == "upgrading" and (directory / "upgrade.done").exists():
                events["upgrade_done"] = int((directory / "upgrade.done").read_text())
                recovery_until = now + args.recovery_seconds * 1000
                state("recovery")
            elif stage == "recovery" and now >= recovery_until:
                break
            reads = [json.loads(p.read_text()) for p in directory.glob("reader-*.json")]
            read_bytes = sum(s["bytes"] for s in reads)
            interval = time.monotonic() - previous_time
            metrics = dict(
                unix_ms=now,
                stage=stage,
                write_mib_per_second=(completed - previous_completed)
                * args.payload_bytes
                / 1024**2
                / interval,
                read_mib_per_second=(read_bytes - previous_read) / 1024**2 / interval,
                applied_write_mib_by_process=[s.get("target_write_mib") for s in writes],
                rate_request_ids=[s.get("rate_request_id") for s in writes],
                pending=sum(s["pending"] for s in writes),
                write_errors=sum(s["errors"] for s in writes),
                rejected_appends=sum(s["rejected_appends"] for s in writes),
                max_no_ack_ms=max((s["no_ack_ms"] for s in writes), default=0),
                ack_gib=completed * args.payload_bytes / 1024**3,
            )
            for label, snapshots, key in (
                ("write", writes, "latency_hist_ms"),
                ("read_poll", reads, "poll_hist_ms"),
            ):
                hist = {}
                for snapshot in snapshots:
                    for bucket, n in snapshot[key]:
                        hist[bucket] = hist.get(bucket, 0) + n
                metrics["recent_" + label + "_p99_ms"] = quantile(hist, 0.99)
            with (directory / "metrics.jsonl").open("a") as output:
                output.write(json.dumps(metrics) + "\n")
            if now - last_print >= 10000:
                elapsed = time.monotonic() - print_time
                print(
                    "PROGRESS",
                    stage,
                    "write_MiB/s",
                    round(
                        (completed - print_completed) * args.payload_bytes / 1024**2 / elapsed, 1
                    ),
                    "read_MiB/s",
                    round((read_bytes - print_read) / 1024**2 / elapsed, 1),
                    "ACK_GiB",
                    round(metrics["ack_gib"], 2),
                    "targets_MiB/s",
                    metrics["applied_write_mib_by_process"],
                    "pending",
                    metrics["pending"],
                    "errors",
                    metrics["write_errors"],
                    "recent_write_p99_ms",
                    metrics["recent_write_p99_ms"],
                    "recent_read_poll_p99_ms",
                    metrics["recent_read_poll_p99_ms"],
                    "max_no_ack_ms",
                    metrics["max_no_ack_ms"],
                    flush=True,
                )
                last_print = now
                print_time, print_completed, print_read = time.monotonic(), completed, read_bytes
            previous_time, previous_completed, previous_read = (
                time.monotonic(),
                completed,
                read_bytes,
            )
            write_json(directory / "events.json", events)
            time.sleep(1)
        events["load_stopped"] = int(time.time() * 1000)
        state("draining")
        (directory / "stop").touch()
        (directory / "reader.stop").touch()
        drain_deadline = time.monotonic() + 300
        for mode, index, p in children:
            p.wait(timeout=max(0.01, drain_deadline - time.monotonic()))
            if p.returncode:
                raise RuntimeError(f"{mode}-{index} failed ({p.returncode}); inspect log")
        final_writes = [json.loads((directory / f"writer-{i}.json").read_text()) for i in range(4)]
        result["writers"] = final_writes
        if any(s["errors"] or s["accepted"] != s["completed"] for s in final_writes):
            raise RuntimeError("writer drain or callbacks failed")
        state("verifying")
        verify = spawn("verify", 0)
        verify.wait(timeout=14410)
        result["verify"] = run.records(directory / "verify.log", "VERIFY ")
        if verify.returncode or len(result["verify"]) != 1 or not result["verify"][0]["success"]:
            raise RuntimeError("full ID readback failed; inspect verify.log")
        result["status"] = "completed"
        state("completed")
    except BaseException as error:
        result["failure_stage"] = stage
        result["error"] = str(error)
        state("failed")
        raise
    finally:
        (directory / "stop").touch()
        (directory / "reader.stop").touch()
        for _, _, p in children:
            run.terminate(p)
        for h in handles:
            h.close()
        write_json(directory / "events.json", events)
        result["phases"] = summarize(directory, events, args.payload_bytes)
        result["processes"] = [
            {"mode": m, "index": i, "exit_code": p.returncode} for m, i, p in children
        ]
        result["logs"] = run.logger_summary(directory)
        result["note"] = (
            "MiB/s counts uncompressed payload. Callback latency is sampled every 256 rows; "
            "read latency is Poll wall time. 60001ms histogram bin is overflow. Data age includes backlog. "
            "Cloud version, ISR and disk/OSS health require server-side confirmation. "
            "Log drops affect diagnostic completeness, not the ID verification outcome. "
            "Phases may contain multiple target rates; use rate-changes.jsonl and per-process "
            "RATE_APPLIED/SAMPLE records to identify effective rate windows."
        )
        write_json(directory / "result.json", result)
        print("RESULT", directory / "result.json", flush=True)
        print("Tables retained; names and IDs are in setup.log/tables.json.", flush=True)


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "command", choices=["build", "run", "mark-start", "mark-done", "status", "set-rate"]
    )
    parser.add_argument("--config", type=Path, default=Path("/root/cpp-test/config.json"))
    parser.add_argument("--database")
    parser.add_argument("--run-dir", type=Path)
    parser.add_argument(
        "--arrow",
        type=Path,
        default=Path("/root/cpp-test/deps/arrow/19.0.1/apache-arrow-19.0.1.tar.gz"),
    )
    parser.add_argument("-j", "--jobs", type=int, default=4)
    parser.add_argument("--payload-bytes", type=int, default=1024)
    parser.add_argument("--prefill-gib", type=float, default=1024)
    parser.add_argument("--aux-tables", type=int, default=30)
    parser.add_argument("--aux-buckets", type=int, default=20)
    parser.add_argument("--buckets", type=int, default=64)
    parser.add_argument("--readers", type=int, default=4)
    parser.add_argument("--write-mib", type=int, help="aggregate MiB/s; run defaults to 500")
    parser.add_argument("--read-mib", type=int, default=300)
    parser.add_argument("--baseline-seconds", type=int, default=600)
    parser.add_argument("--recovery-seconds", type=int, default=900)
    parser.add_argument("--max-seconds", type=int, default=14400)
    parser.add_argument("--max-gib", type=int, default=4096)
    parser.add_argument("--source-version", default="operator-unrecorded")
    parser.add_argument("--target-version", default="operator-unrecorded")
    args = parser.parse_args()
    if args.command == "build":
        run.build(args, target="upgrade_bench")
    elif args.command == "run":

        def interrupted(signum, _frame):
            raise KeyboardInterrupt("signal " + str(signum))

        signal.signal(signal.SIGTERM, interrupted)
        signal.signal(signal.SIGINT, interrupted)
        execute(args)
    else:
        directory = args.run_dir
        if directory is None:
            directory = Path(json.loads((HERE / "runs/latest-upgrade.json").read_text())["run_dir"])
        if args.command == "set-rate":
            set_rate(directory, args.write_mib)
        elif args.command == "status":
            print("RUN_DIR", directory)
            print((directory / "state.json").read_text())
            for path in sorted(directory.glob("writer-*.json")):
                snapshot = json.loads(path.read_text())
                print(
                    path.stem,
                    "target_MiB/s",
                    snapshot.get("target_write_mib"),
                    "request_id",
                    snapshot.get("rate_request_id"),
                )
            if (directory / "result.json").exists():
                print((directory / "result.json").read_text())
        else:
            mark(directory, "upgrade.started" if args.command == "mark-start" else "upgrade.done")


if __name__ == "__main__":
    try:
        main()
    except (Exception, KeyboardInterrupt) as error:
        raise SystemExit(str(error))
