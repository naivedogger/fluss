"""Run bounded Fluss external scans via StarRocks FE. No SR data tables."""
import contextlib
import hashlib
import json
import os
from pathlib import Path
import re
import socket
from statistics import median
import subprocess
import sys
import time
import uuid

import pymysql

def ident(value):
    if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", value):
        raise ValueError("invalid database/table identifier")
    return "`" + value + "`"

def connect(read_timeout=180):
    return pymysql.connect(host=os.getenv("SR_HOST", "fe"), port=int(os.getenv("SR_PORT","9030")),
        user=os.getenv("SR_USER","root"), password=os.getenv("SR_PASSWORD",""),
        charset="utf8mb4", connect_timeout=5, read_timeout=read_timeout, autocommit=True)

def query(c, sql, args=()):
    with c.cursor() as cur:
        cur.execute(sql,args)
        return cur.fetchall()

def register():
    deadline=time.monotonic()+180
    while time.monotonic()<deadline:
        try:
            with connect() as c:
                with c.cursor(pymysql.cursors.DictCursor) as q:
                    q.execute("SHOW BACKENDS"); rows=q.fetchall()
                ready=True
                for host in ("be1","be2"):
                    ip=socket.gethostbyname(host)
                    row=next((r for r in rows if r["IP"]==ip),None)
                    if row is None: query(c,"ALTER SYSTEM ADD BACKEND %s",(ip+":9050",))
                    if not row or str(row["Alive"]).lower()!="true":ready=False
                if ready:return
        except pymysql.err.OperationalError as e:
            if e.args[0] not in (2002,2003,2013):raise
        time.sleep(2)
    raise RuntimeError("two BEs did not register; check docker compose logs")

@contextlib.contextmanager
def catalog(c, props, native=True):
    name="ecs_probe_"+uuid.uuid4().hex[:12]
    opts=dict(props)
    opts.update({"type":"fluss","fluss.option.native.append.enabled":str(native).lower(),
                 "fluss.option.native.pk.enabled":str(native).lower()})
    pairs=",".join(["%s=%s"]*len(opts))
    try:
        query(c,"CREATE EXTERNAL CATALOG "+ident(name)+" PROPERTIES ("+pairs+")",
              tuple(x for kv in opts.items() for x in kv))
    except pymysql.MySQLError as e:
        # Some SQL syntax errors include the whole CREATE statement with credentials.
        raise RuntimeError(f"CREATE EXTERNAL CATALOG failed (MySQL code {e.args[0]})") from None
    try:yield ident(name)
    finally:query(c,"DROP CATALOG "+ident(name))

def profile(c):
    qid=query(c,"select last_query_id()")[0][0]
    deadline=time.monotonic()+20
    while time.monotonic()<deadline:
        rows=query(c,"select get_query_profile(%s)",(qid,))
        if rows and rows[0][0] and "BackendNum:" in rows[0][0]:return qid,rows[0][0]
        time.sleep(.2)
    raise RuntimeError("no completed profile for "+qid)

def positive(p,key):
    return any(float(x)>0 for x in re.findall(r"\b"+key+r":\s*(\d+(?:\.\d+)?)",p))

def check(c,ref,kind,expected,out,min_be=2):
    rows=query(c,f"SELECT id,name FROM {ref} ORDER BY id")
    qid,p=profile(c)
    if list(rows)!=expected:raise RuntimeError(f"{kind}: result mismatch, query {qid}")
    marker="rust-pk-bucket" if kind=="pk" else "rust-append"
    if ("FlussReader: "+marker) not in p:raise RuntimeError(f"{kind}: missing native BE scanner, query {qid}")
    if not positive(p,"FlussNonemptyTails"):raise RuntimeError(f"{kind}: no non-empty log range; tiering may have caught up; repeat with a fresh fixture")
    if kind=="append" and not (positive(p,"rawFileRangeNum") or positive(p,"FlussLakeSnapshots")):raise RuntimeError("Append: missing lake range")
    be=max(map(int,re.findall(r"BackendNum:\s*(\d+)",p)),default=0)
    if be<min_be:raise RuntimeError(f"{kind}: only {be} BE(s) used; check scheduler/profile")
    # Store identifiers and non-sensitive observations; raw profiles may include properties.
    out.append(dict(table=kind,rows=len(rows),reader=marker,backends=be,query_id=qid))
    print(kind,marker,len(rows),"rows;",be,"BE(s);",qid,flush=True)

def client_env(cfg,props):
    db=cfg["database"]
    protocol=props.get("fluss.option.client.security.protocol","PLAINTEXT")
    if protocol.upper() not in ("SASL","PLAINTEXT"):raise ValueError("unsupported Fluss protocol")
    env=dict(os.environ,FLUSS_DB=db,FLUSS_BOOTSTRAP=props["bootstrap.servers"],FLUSS_PROTOCOL=protocol,
             TIER_TIMEOUT=str(cfg.get("tier_timeout_seconds",1800)))
    if protocol.upper()=="SASL":
        if props.get("fluss.option.client.security.sasl.mechanism","PLAIN").upper()!="PLAIN":raise ValueError("expected SASL/PLAIN")
        env["FLUSS_USERNAME"]=props["fluss.option.client.security.sasl.username"]
        env["FLUSS_PASSWORD"]=props["fluss.option.client.security.sasl.password"]
    return env

def smoke(c,cfg,props):
    prefix="sr_probe_"+uuid.uuid4().hex[:12];db=cfg["database"];ident(db)
    env=dict(client_env(cfg,props),FIXTURE_PREFIX=prefix)
    with catalog(c,props) as cat:
        # Resolve catalog before creating fresh tables. No change to existing Fluss data.
        query(c,f"SHOW DATABASES FROM {cat}")
        print("Fluss test tables:",db,prefix+"_append",prefix+"_pk",flush=True)
        subprocess.run([os.getenv("SEED_BIN","/opt/seed")],env=env,check=True,timeout=int(env["TIER_TIMEOUT"])+240)
        result=[]
        for kind,rows in [("append",[(1,"old"),(2,"remove"),(3,"keep"),(4,"tail")]),("pk",[(1,"new"),(3,"keep"),(4,"tail")])]:
            check(c,cat+"."+ident(db)+"."+ident(prefix+"_"+kind),kind,rows,result)
        save(prefix,result)

def save(name,data):
    p=Path(os.getenv("RESULT_DIR","/results"));p.mkdir(parents=True,exist_ok=True)
    (p/(name+".json")).write_text(json.dumps(data,indent=2)+"\n")

def bounds(env, table):
    output = subprocess.check_output(
        [os.getenv("SEED_BIN", "/opt/seed"), "bounds"],
        env=dict(env, BENCH_TABLE=table), text=True, timeout=90)
    lines = [list(map(int, line.split())) for line in output.strip().splitlines()]
    table_id, schema, snapshot = lines[0]
    ranges = lines[1:]  # bucket, start inclusive, stop exclusive, earliest retained
    if not ranges or any(len(r) != 4 or r[3] > r[1] or r[1] > r[2] for r in ranges):
        raise RuntimeError(f"{table}: incomplete or unavailable boundary")
    return dict(table_id=table_id, schema=schema, snapshot=snapshot, ranges=ranges)


def workloads(cfg, kind):
    columns = ["count(*)", "sum(id)"]
    payload = cfg.get("benchmark_value_columns", {}).get(kind)
    if payload:
        columns.append(f"sum(crc32(cast({ident(payload)} as string)))")
    select = "SELECT " + ",".join(columns) + " FROM "
    yield "full", select, ""
    interval = cfg.get("benchmark_id_ranges", {}).get(kind)
    if interval is not None:
        lo, hi = interval
        if type(lo) is not int or type(hi) is not int or lo > hi:
            raise ValueError(f"{kind}: invalid inclusive id range")
        yield "range", select, f" WHERE id BETWEEN {lo} AND {hi}"


def observation(p, qid, mode, kind):
    reader = "jni" if mode == "jni" else {"pk": "rust-pk-bucket", "append": "rust-append"}[kind]
    markers = set(re.findall(r"\bFlussReader:\s*([\w-]+)", p))
    if mode == "native" and kind == "append" and not markers and positive(p,"rawFileRangeNum"):
        reader = "sr-parquet"
    elif markers != {reader}:
        raise RuntimeError(f"{qid}: expected {reader}, observed {sorted(markers)}")
    fields = dict(reader=reader, backends=max(map(int,re.findall(r"BackendNum:\s*(\d+)",p)),default=0))
    for key in ("QueryCumulativeCpuTime","QueryPeakMemoryUsagePerNode","QueryCumulativeScanTime"):
        match = re.search(r"\b" + key + r":\s*([^\r\n]+)", p)
        if match:
            fields[key] = match.group(1).strip()
    if mode == "native":
        fields.update(nonempty_log_range=positive(p,"FlussNonemptyTails"),
                      raw_lake_range=positive(p,"rawFileRangeNum") or "FlussBaselineReader: sr-parquet" in p)
    return fields


def fixture_path(cfg):
    cluster = [cfg["catalog_properties"]["bootstrap.servers"], cfg["database"]]
    key = hashlib.sha256(json.dumps(cluster).encode()).hexdigest()[:16]
    return Path(os.getenv("RESULT_DIR", "/results")) / ("tables-" + key + ".json")


def load_config(cmd):
    cfg = json.loads(Path(os.getenv("PROBE_CONFIG", "/run/secrets/probe.json")).read_text())
    if not cfg.get("endpoint"):
        raise ValueError("set endpoint in config.json")
    props = dict(cfg.get("catalog_properties", {}))
    props["bootstrap.servers"] = cfg["endpoint"]
    if cfg.get("username") or cfg.get("password"):
        if not cfg.get("username") or not cfg.get("password"):
            raise ValueError("set both username and password for SASL/PLAIN")
        props.update({"fluss.option.client.security.protocol":"SASL",
                      "fluss.option.client.security.sasl.mechanism":"PLAIN",
                      "fluss.option.client.security.sasl.username":cfg["username"],
                      "fluss.option.client.security.sasl.password":cfg["password"]})
    cfg["catalog_properties"] = props
    cfg.setdefault("database", "fluss")
    if cmd in ("bench", "peak") and not cfg.get("benchmark_tables"):
        path = fixture_path(cfg)
        if not path.exists():
            raise ValueError("run ./run.sh prepare first, or set benchmark_tables")
        cfg["benchmark_tables"] = json.loads(path.read_text())
        cfg.setdefault("benchmark_value_columns", {"append":"name", "pk":"name"})
    return cfg


def prepare(c, cfg, props):
    prefix = "sr_probe_" + uuid.uuid4().hex[:12]
    n = cfg.get("benchmark_rows", 500000)
    if type(n) is not int or not 100 <= n <= 100000000:
        raise ValueError("benchmark_rows must be 100..100000000 per table")
    env = dict(client_env(cfg,props), FIXTURE_PREFIX=prefix, BENCH_ROWS=str(n))
    names = {k:prefix+"_"+k for k in ("append", "pk")}
    print("Creating Fluss benchmark tables:", cfg["database"], names, flush=True)
    with catalog(c, props) as cat:
        query(c, f"SHOW DATABASES FROM {cat}")
        subprocess.run([os.getenv("SEED_BIN", "/opt/seed")], env=env, check=True)
        # With two out of three tail mutations updating or deleting existing PKs,
        # reading only the baseline or blindly appending changes the aggregate.
        tail = n//10
        triangular = lambda x: x*(x+1)//2
        deleted, inserted = tail//3, (tail+1)//3
        pk_sum = triangular(n) - 3*triangular(deleted) + inserted*(n+2) + 3*triangular(inserted-1)
        for kind, expected in (("append", (n+tail, triangular(n+tail))),
                               ("pk", (n+inserted-deleted, pk_sum))):
            ref = cat+"."+ident(cfg["database"])+"."+ident(names[kind])
            actual = query(c, "SELECT count(*),sum(id) FROM "+ref)
            if actual[0] != expected:
                raise RuntimeError(f"{kind}: expected (count, sum) {expected}, found {actual[0]}")
    path = fixture_path(cfg)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix("."+prefix+".part")
    tmp.write_text(json.dumps(names))
    tmp.replace(path)
    print("Ready:", names, flush=True)


def bench(c, cfg, props):
    reps = cfg.get("repetitions", 6)
    if type(reps) is not int or reps < 2:
        raise ValueError("repetitions must be >= 2")
    query(c,"SET enable_query_cache=false")
    query(c,"SET query_timeout=%s",(int(cfg.get("benchmark_timeout_seconds",600)),))
    env = client_env(cfg,props)
    db = ident(cfg["database"])
    report = {"repetitions":reps, "server_version":query(c,"SELECT current_version()")[0][0],
              "summary":[], "samples":[], "boundaries":{}}
    name = "bench-" + uuid.uuid4().hex[:8]
    with catalog(c,props) as native, catalog(c,props,False) as jni:
        for kind in ("append","pk"):
            table = cfg["benchmark_tables"][kind]
            suffix = "." + db + "." + ident(table)
            for workload, select, where in workloads(cfg,kind):
                before = bounds(env,table)
                has_tail = any(stop > start for _,start,stop,_ in before["ranges"])
                coverage = "lake+log" if has_tail else "lake-only"
                if cfg.get("benchmark_require_log_tail", False) and not has_tail:
                    raise RuntimeError(f"{table}: log tail is empty; prepare a hybrid benchmark table")
                expected = None
                times = {"jni":[], "native":[]}
                for i in range(reps+1):
                    paths = [("jni",jni),("native",native)]
                    for mode, cat in paths if i % 2 == 0 else reversed(paths):
                        start = time.perf_counter()
                        rows = query(c,select+cat+suffix+where)
                        ms = (time.perf_counter()-start)*1000
                        qid,p = profile(c)
                        fields = observation(p,qid,mode,kind)
                        if expected is None:
                            expected = rows
                        if expected != rows or not rows[0][0]:
                            raise RuntimeError(f"{table}/{workload}: results differ or selection empty; {qid}")
                        if workload == "full" and has_tail and mode == "native" and not fields["nonempty_log_range"]:
                            raise RuntimeError(f"{qid}: full scan did not open a nonempty log range")
                        if i:
                            times[mode].append(ms)
                            report["samples"].append(dict(kind=kind, table=table, workload=workload,
                                pair=i, path=mode, ms=ms, query_id=qid, **fields))
                if bounds(env,table) != before:
                    raise RuntimeError(f"{table}/{workload}: lake snapshot or bucket offsets changed; discard run")
                report["boundaries"][kind+"/"+workload] = before
                j,n = times["jni"],times["native"]
                ratio = median(a/b for a,b in zip(j,n))
                report["summary"].append(dict(kind=kind,table=table,workload=workload,coverage=coverage,
                    matched_rows=int(expected[0][0]), jni_p50_ms=median(j),native_p50_ms=median(n),
                    paired_speedup=ratio))
                print(f"{kind}/{workload} ({coverage} boundary): {expected[0][0]} rows, JNI {median(j):.1f} ms, "
                      f"native {median(n):.1f} ms (P50); paired JNI/native {ratio:.2f}x",flush=True)
    save(name, report)
    print(f"Results: {name}.json")

if __name__=="__main__":
    cmd=sys.argv[1]
    if cmd=="register":register()
    else:
        cfg=load_config(cmd);props=cfg["catalog_properties"]
        try:
            with connect(int(cfg.get("benchmark_timeout_seconds",600))+30 if cmd=="bench" else 180) as c:
                query(c,"SET enable_profile=true")
                if cmd=="smoke":smoke(c,cfg,props)
                elif cmd=="bench":bench(c,cfg,props)
                elif cmd=="prepare":prepare(c,cfg,props)
                else:raise ValueError("unknown command")
        except pymysql.MySQLError as e:
            print("SR query failed; MySQL error code:",e.args[0],file=sys.stderr)
            sys.exit(1)
