"""Find the highest observed StarRocks query throughput for each Fluss reader."""
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack
from math import ceil
import sys
from threading import Barrier, Event
import time
import uuid

import pymysql
from probe import bounds, catalog, client_env, connect, ident, load_config, observation, profile, query, save, workloads


def timed_load(sessions, sql, expected, seconds):
    """One outstanding request per session. Include the final drain in the denominator."""
    start = []
    gate = Barrier(len(sessions), action=lambda: start.append(time.monotonic()), timeout=30)
    stopped = Event()

    def worker(c):
        latencies, errors = [], Counter()
        consecutive = 0
        gate.wait()
        last = start[0]
        while not stopped.is_set():
            now = time.monotonic()
            if now >= start[0] + seconds:
                break
            try:
                rows = query(c, sql)
                last = time.monotonic()
                if rows != expected:
                    raise ValueError("aggregate mismatch during load; discard benchmark")
                latencies.append((last - now) * 1000)
                consecutive = 0
            except pymysql.MySQLError as e:
                last = time.monotonic()
                errors[str(e.args[0]) if e.args and isinstance(e.args[0], int) else "driver"] += 1
                consecutive += 1
                if not c.open or (e.args and e.args[0] in (2006,2013,2055)):
                    stopped.set()
                    raise RuntimeError("SQL connection lost under load; check outstanding queries before rerunning") from None
                if consecutive >= 3:
                    stopped.set()
            except BaseException:
                stopped.set()
                raise
        return last, latencies, errors

    with ThreadPoolExecutor(max_workers=len(sessions)) as pool:
        results = list(pool.map(worker, sessions))
    elapsed = max(r[0] for r in results) - start[0]
    latencies = sorted(ms for _, samples, _ in results for ms in samples)
    errors = sum((r[2] for r in results), Counter())
    attempts = len(latencies) + sum(errors.values())
    return dict(completed=len(latencies), errors=dict(errors),
                error_rate=sum(errors.values()) / attempts if attempts else None,
                seconds=elapsed, qps=len(latencies) / elapsed if elapsed > 0 else 0,
                p95_ms=latencies[ceil(.95*len(latencies))-1] if latencies else None,
                early_stop=stopped.is_set())


def stage(n, seconds, timeout, sql, expected):
    with ExitStack() as opened:
        sessions = []
        for _ in range(n):
            try:
                c = opened.enter_context(connect(timeout+30))
                query(c, "SET enable_query_cache=false, enable_profile=false, query_timeout=%s", (timeout,))
                sessions.append(c)
            except pymysql.MySQLError as e:
                # FE's user/global connection limit is a search boundary, not zero measured query throughput.
                if e.args and e.args[0] in (1040, 1203, 1226):
                    return dict(admission_error=e.args[0], sessions_open=len(sessions))
                raise
        return timed_load(sessions, sql, expected, seconds)


def search(run, initial, ceiling, seconds, confirm_seconds):
    trials = []
    def measure(n, confirm=False):
        r = dict(concurrency=n, confirmation=confirm, **run(n, confirm_seconds if confirm else seconds))
        trials.append(r)
        if "admission_error" in r:
            print(f"  {n} clients: FE refused admission ({r['admission_error']})", flush=True)
        else:
            print(f"  {n} clients: {r['qps']:.2f} successful QPS; P95 {r['p95_ms']} ms; "
                  f"errors {sum(r['errors'].values())}" + (" [retest]" if confirm else ""), flush=True)
        return r

    def clean(r):
        return r.get("completed", 0) > 0 and not r["errors"] and not r["early_stop"]

    def score(r):
        return r.get("qps", 0)

    n, anchor, flat = initial, 0, 0
    while True:
        r = measure(n)
        if r.get("admission_error") or r.get("early_stop"):
            reason = "admission or repeated errors"
            break
        if score(r) > anchor*1.05:
            anchor, flat = score(r), 0
        else:
            flat += 1
        if n == ceiling or flat >= 2:
            reason = "configured ceiling" if n == ceiling else "two levels without >5% gain"
            break
        n = min(2*n, ceiling)

    # A high starting point can already be past saturation.
    n = initial
    while n > 1 and (not any(clean(r) for r in trials) or
                      max((r for r in trials if clean(r)), key=score)["concurrency"] <= n):
        n = max(1, n//2)
        measure(n)

    clean_trials = [r for r in trials if clean(r)]
    if clean_trials:
        best = max(clean_trials,key=score)["concurrency"]
        # Refine on both sides of the best coarse level, including the first overloaded neighbor.
        levels = sorted(r["concurrency"] for r in trials)
        i = levels.index(best)
        for neighbor in levels[max(0,i-1):i]+levels[i+1:i+2]:
            middle = (best+neighbor)//2
            if middle not in levels:
                measure(middle)

    candidates = sorted([r for r in trials if clean(r)],key=score,reverse=True)[:2]
    confirmed = []
    for first in candidates:
        again = measure(first["concurrency"], True)
        if clean(again):
            confirmed.append(dict(concurrency=first["concurrency"],
                                  qps=min(first["qps"],again["qps"]),
                                  worst_p95_ms=max(first["p95_ms"],again["p95_ms"])))
    observed = max((r for r in trials if r.get("completed") and not r["early_stop"]),key=score,default=None)
    return dict(stop_reason=reason, observed=observed,
                confirmed=max(confirmed,key=score,default=None), trials=trials)


def main(cfg, initial, ceiling):
    seconds = cfg.get("peak_seconds", 30)
    confirm = cfg.get("peak_confirm_seconds", 60)
    if any(type(i) is not int for i in (initial,ceiling,seconds,confirm)) or not (
            1 <= initial <= ceiling and 1 <= seconds <= confirm):
        raise ValueError("expected 1 <= start <= max clients and 1 <= trial <= confirmation seconds")
    timeout = int(cfg.get("benchmark_timeout_seconds",600))
    props = cfg["catalog_properties"]
    env = client_env(cfg,props)
    report = dict(start_clients=initial,max_clients=ceiling,trial_seconds=seconds,
                  confirmation_seconds=confirm,comparisons=[])
    name = "peak-" + uuid.uuid4().hex[:8]
    with connect(timeout+30) as c, catalog(c,props) as native, catalog(c,props,False) as jni:
        query(c,"SET enable_query_cache=false, enable_profile=true, query_timeout=%s",(timeout,))
        report["server_version"] = query(c,"SELECT current_version()")[0][0]
        for kind in ("append","pk"):
            table = cfg["benchmark_tables"][kind]
            suffix = "."+ident(cfg["database"])+"."+ident(table)
            for work,select,where in workloads(cfg,kind):
                before = bounds(env,table)
                tail = any(b[2]>b[1] for b in before["ranges"])
                if cfg.get("benchmark_require_log_tail") and not tail:
                    raise RuntimeError(f"{table}: no log tail")
                pair = dict(table=table,kind=kind,workload=work,boundary=before,
                            coverage="lake+log" if tail else "lake-only",paths={})
                expected = None
                paths = [("jni",jni),("native",native)]
                for mode,cat in paths if len(report["comparisons"])%2==0 else reversed(paths):
                    sql = select+cat+suffix+where
                    rows = query(c,sql)
                    qid,p = profile(c)
                    obs = observation(p,qid,mode,kind)
                    if not rows[0][0] or (expected is not None and rows!=expected):
                        raise RuntimeError(f"{qid}: empty or inconsistent aggregate")
                    if work=="full" and mode=="native" and tail and not obs["nonempty_log_range"]:
                        raise RuntimeError(f"{qid}: missing expected log tail")
                    expected = rows
                    print(f"{kind}/{work} {mode} ({pair['coverage']})",flush=True)
                    result = search(lambda n,s:stage(n,s,timeout,sql,expected),
                                    initial,ceiling,seconds,confirm)
                    if bounds(env,table)!=before:
                        raise RuntimeError(f"{table}: boundary changed; discard comparison")
                    pair["paths"][mode] = dict(preflight=dict(query_id=qid,**obs),**result)
                    print("  Stopped expansion:",result["stop_reason"],flush=True)
                j,n = (pair["paths"][m]["confirmed"] for m in ("jni","native"))
                pair["native_over_jni"] = n["qps"]/j["qps"] if j and n else None
                report["comparisons"].append(pair)
                save(name,report)
                print("Confirmed error-free peaks:", {m:pair["paths"][m]["confirmed"] for m in ("jni","native")},
                      "native/JNI:", pair["native_over_jni"], flush=True)
    print(f"Results: {name}.json")


if __name__=="__main__":
    cfg = load_config("peak")
    if len(sys.argv)>3:
        sys.exit("usage: peak [start_clients [max_clients]]")
    main(cfg,int(sys.argv[1]) if len(sys.argv)>1 else cfg.get("peak_start_clients",32),
         int(sys.argv[2]) if len(sys.argv)>2 else cfg.get("peak_max_clients",512))
