# StarRocks / Fluss UnionRead on ECS

Run on an x86_64 Linux test host with Git, Docker Buildx and Compose v2. One StarRocks Java FE plans external queries; two C++ BEs run either the native reader or the community JNI reader. No StarRocks data table is created.

```sh
git clone --depth 1 -b codex/sr-ecs-bounded https://github.com/naivedogger/fluss.git
cd fluss/tools/sr-unionread
umask 077
cp config.example.json config.json
vi config.json
./run.sh build
./run.sh up
./run.sh prepare
./run.sh bench
./run.sh peak 64 1024
```

Fill in the Fluss bootstrap endpoint (host:port), username and password. Use the SASL/PLAIN listener on a trusted network, or leave both credentials empty for PLAINTEXT. Both BEs and the FE must reach the advertised tablet servers and the lake. The Rust reader is built from the checkout; Buildx fetches the pinned StarRocks commit from the personal fork. Build time and disk usage are dominated by StarRocks and its dev image. The first build needs internet access for GitHub, container registries, Cargo and Maven. Set JOBS to adjust build parallelism (default 4).

**Lake access.** Three settings suffice if Paimon catalog configuration and file access are already available to the clients. Fluss login does not grant DLF or OSS access. When separate credentials or a warehouse override are necessary, add a `catalog_properties` object to config.json. Paimon/DLF uses `table.datalake.paimon.{metastore,uri,warehouse,token.provider,dlf.region,dlf.access-key-id,dlf.access-key-secret}`; Paimon OSS uses `table.datalake.paimon.fs.oss.{endpoint,accessKeyId,accessKeySecret}`; StarRocks raw Parquet uses `aliyun.oss.{endpoint,access_key,secret_key}`. Set actual individual keys, without braces. For a filesystem catalog specify its warehouse instead of DLF settings. config.json is ignored by Git and the Dockerfile build context; runtime catalog credentials may be retained in the FE journal. This prototype does not implement SASL_SSL or native scans of tables requiring Paimon query-auth.

**Prepare data.** The default Fluss database is `fluss` (override with `database`). The account must be able to create and write tables there and the tiering job must pick up new tables. prepare creates two uniquely named, nonpartitioned Paimon/Parquet tables with four buckets and 500,000 baseline rows each. It waits for a readable lake snapshot covering the baseline in each written bucket, then appends 10% more rows on the log table and mixes updates, inserts and deletes on the deduplicate PK table. It checks the visible counts and remembers the table names under results/. Run prepare once per dataset; adjust `benchmark_rows` for larger runs. To use existing tables instead, skip prepare and set `benchmark_tables` to an object with `append` and `pk` table names (numeric id column). `./run.sh smoke` is a separate small correctness check.

**Compare readers.** bench compares latency; peak runs concurrent SQL queries without a rate limit. Both compare the same tables and aggregates. For the prepared tables, the query reads id and the name value column. Both validate results and reject a changed lake snapshot or bucket boundary. For a specifically hybrid workload, keep the baseline and tail stable during measurement and set `benchmark_require_log_tail` to true; if tiering catches up, an otherwise valid run is lake-only.

- bench warms both paths and alternates order for six timed pairs. Reports include P50 and paired JNI/native latency ratio.
- peak defaults to 32 concurrent queries and a ceiling of 512. `peak 64 1024` starts at 64 and doubles up to 1024, probes lower and intermediate levels, and retests the two best zero-error levels. Default measurement/confirmation windows are 30/60 seconds (override `peak_seconds` / `peak_confirm_seconds`). Results include successful QPS, P95, errors, concurrency and the reason expansion stopped. Reaching the ceiling is not proof of saturation.
- Results are in `results/bench-*.json` and `results/peak-*.json`. Compare on an otherwise idle test deployment; monitor FE connection limits, BE CPU/memory, tablet servers and OSS during peak runs.

`./run.sh down` stops the local StarRocks cluster and retains its volumes. The scripts never delete Fluss tables. Remove the generated tables by the exact names printed by prepare or smoke when finished.
