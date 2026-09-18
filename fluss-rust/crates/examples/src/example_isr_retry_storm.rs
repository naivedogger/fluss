// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! High-throughput A/B driver for the writer retry-backoff fix.
//!
//! It hammers an `acks=all` log table with many concurrent writers. When the
//! cluster's ISR shrinks below `min-in-sync-replicas-number` (for example while
//! a big table is deleted/created and the leader's request queue hits the netty
//! backpressure threshold, pausing follower replication), the leader keeps
//! rejecting writes with a retriable `NotEnoughReplicas` error. The client then
//! re-enqueues the batches; with `--retry-backoff-ms 0` that re-enqueue loop is
//! unbounded (the pre-fix storm), with `--retry-backoff-ms 100` it is paced.
//!
//! Run the exact same command twice, once with 0 and once with 100, and compare
//! `retry/s` (client-side storm signal) plus the server's `NotEnoughReplicas`
//! ERROR rate and how long the ISR stays shrunk.
//!
//! Stressor vs observed workload. Backpressure protects the process that has it,
//! so a single backpressured job may no longer reproduce the storm on its own.
//! To keep the anomaly reproducible while measuring the fix, split the roles:
//!   - Stressor: a fire-and-forget process (`--await-completions` off,
//!     `--retry-backoff-ms 0`) that keeps inducing the ISR shrink. Same in both
//!     arms; it is the constant environmental insult.
//!   - Observed: a healthy-rate process that carries the config under test and
//!     reports end-to-end latency. Baseline = `--await-completions` with
//!     `--max-in-flight-appends 0` (unbounded, measure-only) and
//!     `--retry-backoff-ms 0`; fixed = `--await-completions` with a bounded
//!     `--max-in-flight-appends` and `--retry-backoff-ms 100`.
//!
//! Compare the observed process's `over_...ms` ratio and p99/max between arms.
//! The latency clock starts before the backpressure permit, so a backpressure
//! stall still counts as user-visible latency and cannot hide a timeout.
//!
//! Connection: pass a config file with the endpoint and credentials, or use
//! flags/env vars. Precedence for each value is CLI flag > config file > env.
//!   --config isr-storm.conf   (copy isr-storm.example.conf and fill it in)
//!   --bootstrap host:port     (or config `bootstrap`, or $FLUSS_BOOTSTRAP)
//!   SASL is enabled when a username is present (config `sasl.username` or
//!   $FLUSS_SASL_USERNAME). Keep real config files out of git; the checked-in
//!   file is a placeholder template only. Never bake credentials into source.
//!
//! Example (pre-fix storm):
//!   FLUSS_SASL_USERNAME=... FLUSS_SASL_PASSWORD=... \
//!   cargo run --release -p fluss-examples --example example-isr-retry-storm -- \
//!     --bootstrap 10.0.0.1:9123 --buckets 32 --replication-factor 3 \
//!     --concurrency 8 --target-rate 600000 --retry-backoff-ms 0 --metrics-port 9000

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use clap::Parser;
use fluss::client::{FlussConnection, WriteCallbackBatch};
use fluss::config::Config;
use fluss::error::{Error, Result};
use fluss::metadata::{DataTypes, Schema, TableDescriptor, TablePath};
use fluss::row::GenericRow;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Rows appended per inner loop before the task paces or yields.
const BURST: usize = 500;

/// End-to-end append latency bucket upper bounds in ms (exclusive). The extra
/// catch-all bucket holds everything at or above the last bound.
const LAT_BOUNDS_MS: [u64; 12] = [1, 5, 10, 25, 50, 100, 250, 500, 1000, 2000, 5000, 30000];

/// Lock-free latency + outcome stats, updated from completion callbacks. The
/// callbacks run off the writer task on the sender's completion path, so every
/// field is atomic to keep the callback cheap and non-blocking.
struct LatencyStats {
    ok: AtomicU64,
    failed: AtomicU64,
    sum_ms: AtomicU64,
    max_ms: AtomicU64,
    over_threshold: AtomicU64,
    buckets: [AtomicU64; LAT_BOUNDS_MS.len() + 1],
    /// Time spent blocked on the backpressure permit before the append was even
    /// submitted. Reported separately so we can attribute user-visible latency
    /// between "cluster was slow to ack" and "we throttled ourselves".
    wait_sum_ms: AtomicU64,
    wait_max_ms: AtomicU64,
    /// Appends submitted but not yet acked. Tracked directly (not via the
    /// semaphore) so it is meaningful in unbounded/measure-only mode too.
    in_flight: AtomicU64,
}

impl LatencyStats {
    fn new() -> Self {
        Self {
            ok: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            sum_ms: AtomicU64::new(0),
            max_ms: AtomicU64::new(0),
            over_threshold: AtomicU64::new(0),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            wait_sum_ms: AtomicU64::new(0),
            wait_max_ms: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
        }
    }

    /// Records one completed append: its outcome, end-to-end latency, whether it
    /// blew past the user-visible threshold, and which distribution bucket it hit.
    fn record(&self, result: &Result<()>, elapsed_ms: u64, threshold_ms: u64) {
        if result.is_ok() {
            self.ok.fetch_add(1, Ordering::Relaxed);
        } else {
            self.failed.fetch_add(1, Ordering::Relaxed);
        }
        self.sum_ms.fetch_add(elapsed_ms, Ordering::Relaxed);
        self.max_ms.fetch_max(elapsed_ms, Ordering::Relaxed);
        if elapsed_ms >= threshold_ms {
            self.over_threshold.fetch_add(1, Ordering::Relaxed);
        }
        let idx = LAT_BOUNDS_MS
            .iter()
            .position(|&b| elapsed_ms < b)
            .unwrap_or(LAT_BOUNDS_MS.len());
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }

    /// Marks an append as submitted (callback registered, not yet acked).
    fn on_submit(&self) {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
    }

    /// Records how long an append blocked on the backpressure permit.
    fn record_wait(&self, wait_ms: u64) {
        self.wait_sum_ms.fetch_add(wait_ms, Ordering::Relaxed);
        self.wait_max_ms.fetch_max(wait_ms, Ordering::Relaxed);
    }

    fn snapshot_buckets(&self) -> [u64; LAT_BOUNDS_MS.len() + 1] {
        std::array::from_fn(|i| self.buckets[i].load(Ordering::Relaxed))
    }
}

/// Approximate percentile from cumulative latency buckets. Returns the bucket's
/// upper bound in ms; the catch-all bucket reports `u64::MAX`.
fn approx_percentile_ms(buckets: &[u64], q: f64) -> u64 {
    let total: u64 = buckets.iter().sum();
    if total == 0 {
        return 0;
    }
    let target = ((total as f64) * q).ceil() as u64;
    let mut cum = 0u64;
    for (i, &c) in buckets.iter().enumerate() {
        cum += c;
        if cum >= target {
            return LAT_BOUNDS_MS.get(i).copied().unwrap_or(u64::MAX);
        }
    }
    u64::MAX
}

/// Renders a bucket bound, showing the catch-all sentinel as `>last`.
fn fmt_ms(v: u64) -> String {
    if v == u64::MAX {
        format!(">{}ms", LAT_BOUNDS_MS[LAT_BOUNDS_MS.len() - 1])
    } else {
        format!("{v}ms")
    }
}

/// Prints the full latency distribution, tail percentiles, and the count of
/// appends that exceeded the user-visible threshold. Called once at shutdown.
fn print_latency_summary(stats: &LatencyStats, threshold_ms: u64) {
    let ok = stats.ok.load(Ordering::Relaxed);
    let failed = stats.failed.load(Ordering::Relaxed);
    let over = stats.over_threshold.load(Ordering::Relaxed);
    let max_ms = stats.max_ms.load(Ordering::Relaxed);
    let sum_ms = stats.sum_ms.load(Ordering::Relaxed);
    let wait_sum_ms = stats.wait_sum_ms.load(Ordering::Relaxed);
    let wait_max_ms = stats.wait_max_ms.load(Ordering::Relaxed);
    let buckets = stats.snapshot_buckets();
    let total = ok + failed;
    let mean = if total > 0 {
        sum_ms as f64 / total as f64
    } else {
        0.0
    };
    let over_pct = if total > 0 {
        over as f64 * 100.0 / total as f64
    } else {
        0.0
    };
    let wait_mean = if total > 0 {
        wait_sum_ms as f64 / total as f64
    } else {
        0.0
    };
    println!(
        "Latency (submit->ack, incl. backpressure wait): completed={total} ok={ok} failed={failed} over_{}ms={over} ({over_pct:.2}%)",
        threshold_ms
    );
    println!(
        "  p50~{} p90~{} p99~{} max={max_ms}ms mean={mean:.1}ms | backpressure wait mean={wait_mean:.1}ms max={wait_max_ms}ms",
        fmt_ms(approx_percentile_ms(&buckets, 0.50)),
        fmt_ms(approx_percentile_ms(&buckets, 0.90)),
        fmt_ms(approx_percentile_ms(&buckets, 0.99)),
    );
    let mut lo = 0u64;
    for (i, &c) in buckets.iter().enumerate() {
        if i < LAT_BOUNDS_MS.len() {
            println!("    [{lo:>6}, {:>6}) ms: {c}", LAT_BOUNDS_MS[i]);
            lo = LAT_BOUNDS_MS[i];
        } else {
            println!("    [{lo:>6},    inf) ms: {c}");
        }
    }
}

#[derive(Parser, Clone)]
#[command(about = "A/B load driver for the writer retry-backoff fix (ISR-shrink storm).")]
struct Args {
    /// Config file with `key = value` lines (endpoint + credentials). Keys:
    /// bootstrap, sasl.username, sasl.password, sasl.mechanism. See
    /// isr-storm.example.conf. Values here override env vars but not CLI flags.
    #[arg(long)]
    config: Option<String>,

    /// Fluss bootstrap servers (host:port). Falls back to config file, then
    /// $FLUSS_BOOTSTRAP.
    #[arg(long)]
    bootstrap: Option<String>,

    #[arg(long, default_value = "fluss")]
    database: String,
    #[arg(long, default_value = "rust_isr_retry_storm")]
    table: String,

    /// Number of buckets for the test table. More buckets spread load and make
    /// the storm touch more leaders.
    #[arg(long, default_value_t = 32)]
    buckets: i32,
    /// Replication factor. Use 3 so losing one in-sync follower drops the ISR
    /// below a `min-in-sync-replicas-number` of 2 or 3.
    #[arg(long, default_value_t = 3)]
    replication_factor: i32,

    /// Producer acks. Must be "all" to observe NotEnoughReplicas.
    #[arg(long, default_value = "all")]
    acks: String,

    /// THE A/B knob. 0 = pre-fix tight retry storm; 100 = fixed exponential backoff.
    #[arg(long, default_value_t = 100)]
    retry_backoff_ms: u64,
    /// Upper bound for the exponential backoff (ignored when retry-backoff-ms is 0).
    #[arg(long, default_value_t = 1000)]
    retry_max_backoff_ms: u64,

    /// In-flight requests per bucket. Raise to amplify the storm. Values above 5
    /// require --idempotence false.
    #[arg(long, default_value_t = 5)]
    max_inflight: usize,
    /// Idempotent writes. Set to false to allow --max-inflight above 5.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    idempotence: bool,

    /// Concurrent writer tasks in this process. Run several processes for more.
    #[arg(long, default_value_t = 8)]
    concurrency: usize,
    /// Payload string length in bytes.
    #[arg(long, default_value_t = 256)]
    payload_bytes: usize,
    /// Per-process target rows/sec (0 = push as fast as possible).
    #[arg(long, default_value_t = 0)]
    target_rate: u64,

    /// Writer buffer memory in bytes. Larger absorbs longer stalls.
    #[arg(long, default_value_t = 256 * 1024 * 1024)]
    buffer_memory: usize,
    /// Max time (ms) an append blocks on buffer memory before returning a
    /// backpressure error. Kept short so a saturated producer cycles quickly and
    /// reports throttling; it does NOT mean data was lost.
    #[arg(long, default_value_t = 1000)]
    buffer_wait_timeout_ms: u64,

    /// Register a completion callback per append and bound outstanding un-acked
    /// appends, instead of fire-and-forget. This is the client-backpressure mode:
    /// the producer cannot outrun the cluster, and end-to-end latency
    /// (submit -> ack) is recorded so you can see how long writes actually take,
    /// including the share that blows past --latency-threshold-ms.
    #[arg(long, default_value_t = false)]
    await_completions: bool,
    /// Backpressure bound: max outstanding un-acked appends across the whole
    /// process when --await-completions is set. Once reached, new appends wait
    /// for an ack before enqueuing more. Set to 0 for unbounded: callbacks still
    /// measure end-to-end latency but no backpressure is applied, which is the
    /// pre-fix baseline we can still observe (register-to-measure only).
    #[arg(long, default_value_t = 10_000)]
    max_in_flight_appends: usize,
    /// User-visible latency threshold in ms. Completions at or above this count
    /// toward `over_...ms`, the "writes that stalled long enough to hurt the
    /// caller" signal (default 30s, a typical request timeout).
    #[arg(long, default_value_t = 30_000)]
    latency_threshold_ms: u64,

    /// Seconds to run.
    #[arg(long, default_value_t = 600)]
    run_seconds: u64,
    /// Prometheus scrape port for client metrics (0 = disabled). When set, the
    /// monitor also prints client-side retry/s parsed from the recorder.
    #[arg(long, default_value_t = 0)]
    metrics_port: u16,

    /// Trigger mode: drop the table, recreate it with the same schema/buckets/RF,
    /// wait until it accepts writes again, then exit. Does NOT run load. Use this
    /// as the delete+recreate step while the load processes keep running.
    #[arg(long, default_value_t = false)]
    recreate: bool,
    /// Max seconds to wait for a freshly created table to become writable before
    /// giving up. Applies both on load startup and in --recreate mode.
    #[arg(long, default_value_t = 120)]
    ready_timeout_secs: u64,
}

/// Connection settings parsed from the `--config` file. Everything is optional;
/// unset values fall back to env vars (and, for bootstrap, CLI flag).
#[derive(Default)]
struct FileConfig {
    bootstrap: Option<String>,
    sasl_username: Option<String>,
    sasl_password: Option<String>,
    sasl_mechanism: Option<String>,
}

/// Parse a minimal `key = value` config file (dependency-free). Blank lines and
/// lines starting with `#` are ignored; surrounding quotes on values are
/// stripped. Unknown keys and malformed lines are hard errors so a typo in the
/// endpoint or credentials fails fast instead of silently connecting nowhere.
fn load_config_file(path: &str) -> FileConfig {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read config file {path}: {e}"));
    let mut cfg = FileConfig::default();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("{path}:{}: expected `key = value`, got: {raw}", i + 1));
        let value = value.trim().trim_matches('"').to_string();
        match key.trim() {
            "bootstrap" | "endpoint" => cfg.bootstrap = Some(value),
            "sasl.username" => cfg.sasl_username = Some(value),
            "sasl.password" => cfg.sasl_password = Some(value),
            "sasl.mechanism" => cfg.sasl_mechanism = Some(value),
            other => panic!("{path}:{}: unknown key `{other}`", i + 1),
        }
    }
    cfg
}

fn build_config(args: &Args, bootstrap: String, file_cfg: &FileConfig) -> Config {
    let mut config = Config {
        bootstrap_servers: bootstrap,
        writer_acks: args.acks.clone(),
        writer_retry_backoff_ms: args.retry_backoff_ms,
        writer_retry_max_backoff_ms: args.retry_max_backoff_ms,
        writer_max_inflight_requests_per_bucket: args.max_inflight,
        writer_enable_idempotence: args.idempotence,
        writer_buffer_memory_size: args.buffer_memory,
        writer_buffer_wait_timeout_ms: args.buffer_wait_timeout_ms,
        ..Default::default()
    };
    // SASL is enabled when a username is present: config file first, then env.
    let user = file_cfg
        .sasl_username
        .clone()
        .or_else(|| std::env::var("FLUSS_SASL_USERNAME").ok())
        .filter(|s| !s.is_empty());
    if let Some(user) = user {
        config.security_protocol = "sasl".to_string();
        config.security_sasl_mechanism = file_cfg
            .sasl_mechanism
            .clone()
            .or_else(|| std::env::var("FLUSS_SASL_MECHANISM").ok())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "PLAIN".to_string());
        config.security_sasl_username = user;
        config.security_sasl_password = file_cfg
            .sasl_password
            .clone()
            .or_else(|| std::env::var("FLUSS_SASL_PASSWORD").ok())
            .unwrap_or_default();
    }
    config
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let file_cfg = args
        .config
        .as_deref()
        .map(load_config_file)
        .unwrap_or_default();

    let bootstrap = args
        .bootstrap
        .clone()
        .or_else(|| file_cfg.bootstrap.clone())
        .or_else(|| std::env::var("FLUSS_BOOTSTRAP").ok())
        .filter(|s| !s.is_empty())
        .expect("set --bootstrap, config file `bootstrap`, or $FLUSS_BOOTSTRAP");

    if args.max_inflight > 5 && args.idempotence {
        panic!(
            "--max-inflight {} requires --idempotence false (idempotent writes cap it at 5)",
            args.max_inflight
        );
    }

    // Install a Prometheus recorder before any client object is created so the client
    // binds its metric handles to it. We always install the in-process recorder (except
    // in --recreate trigger mode) so the monitor can print client-side sent/s and
    // retry/s. The HTTP listener is only bound when --metrics-port > 0; skip it when
    // running many processes on one host to avoid port collisions.
    let metrics_handle: Option<PrometheusHandle> = if args.recreate {
        None
    } else if args.metrics_port > 0 {
        let (recorder, exporter) = PrometheusBuilder::new()
            .with_http_listener(([0, 0, 0, 0], args.metrics_port))
            .build()
            .expect("failed to build Prometheus recorder");
        let handle = recorder.handle();
        metrics::set_global_recorder(recorder).expect("failed to install global recorder");
        tokio::spawn(exporter);
        println!(
            "Client metrics on http://0.0.0.0:{}/metrics",
            args.metrics_port
        );
        Some(handle)
    } else {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::set_global_recorder(recorder).expect("failed to install global recorder");
        Some(handle)
    };

    let config = build_config(&args, bootstrap.clone(), &file_cfg);
    let mode = if args.await_completions {
        if args.max_in_flight_appends > 0 {
            format!(
                "callback+backpressure(max_in_flight={})",
                args.max_in_flight_appends
            )
        } else {
            "callback+measure-only(unbounded)".to_string()
        }
    } else {
        "fire-and-forget".to_string()
    };
    println!(
        "Connecting to {bootstrap} | acks={} idempotence={} max_inflight={} retry_backoff_ms={} concurrency={} target_rate={} mode={}",
        args.acks,
        args.idempotence,
        args.max_inflight,
        args.retry_backoff_ms,
        args.concurrency,
        args.target_rate,
        mode
    );

    let conn = FlussConnection::new(config).await?;
    let admin = conn.get_admin()?;

    let table_path = TablePath::new(&args.database, &args.table);
    let table_descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("c1", DataTypes::int())
                .column("c2", DataTypes::string())
                .build()?,
        )
        .distributed_by(Some(args.buckets), vec!["c1".to_string()])
        .property(
            "table.replication.factor",
            args.replication_factor.to_string(),
        )
        .build()?;

    let ready_timeout = Duration::from_secs(args.ready_timeout_secs);

    // Trigger mode: drop + recreate the same-named table, wait until it accepts
    // writes, then exit. This is the DDL event whose blast radius we observe.
    if args.recreate {
        println!("Dropping {}.{} ...", args.database, args.table);
        admin.drop_table(&table_path, true).await?;
        println!(
            "Recreating {}.{} ({} buckets, RF={}) ...",
            args.database, args.table, args.buckets, args.replication_factor
        );
        admin
            .create_table(&table_path, &table_descriptor, false)
            .await?;
        let elapsed = wait_until_ready(&conn, &table_path, ready_timeout).await?;
        println!(
            "Recreate done: table writable after {:.1}s.",
            elapsed.as_secs_f64()
        );
        return Ok(());
    }

    match admin
        .create_table(&table_path, &table_descriptor, true)
        .await
    {
        Ok(_) => println!(
            "Table {}.{} ready ({} buckets, RF={}).",
            args.database, args.table, args.buckets, args.replication_factor
        ),
        Err(e) => println!("create_table returned an error (continuing, may already exist): {e}"),
    }

    // Wait until the table actually accepts an acks=all write before hammering it,
    // so a load process restarted right after a recreate does not spew errors
    // against a table whose leaders are not elected yet.
    let elapsed = wait_until_ready(&conn, &table_path, ready_timeout).await?;
    println!(
        "Table writable after {:.1}s; starting load.",
        elapsed.as_secs_f64()
    );

    let table = conn.get_table(&table_path).await?;

    let enqueued = Arc::new(AtomicU64::new(0));
    let throttled = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));

    // Callback / backpressure mode state. In fire-and-forget mode these are
    // created but unused, keeping the task and monitor wiring uniform.
    // `max_in_flight_appends == 0` means unbounded: callbacks still measure
    // latency but the permit gate is skipped (measure-only baseline).
    let await_completions = args.await_completions;
    let threshold_ms = args.latency_threshold_ms;
    let bounded = args.max_in_flight_appends > 0;
    let max_in_flight = args.max_in_flight_appends.max(1);
    let latency = Arc::new(LatencyStats::new());
    let semaphore = Arc::new(Semaphore::new(max_in_flight));

    let per_task_rate = if args.target_rate > 0 {
        (args.target_rate / args.concurrency.max(1) as u64).max(1)
    } else {
        0
    };

    // Capture Copy locals so each task closure does not move `args`.
    let concurrency = args.concurrency;
    let payload_bytes = args.payload_bytes;
    let run = Duration::from_secs(args.run_seconds);
    let mut handles = Vec::with_capacity(concurrency);
    for task_id in 0..concurrency {
        // A dedicated writer per task avoids contending on the shared bucket-key
        // encoder mutex, so the drivers actually reach a high enqueue rate.
        let writer = table.new_append()?.create_writer()?;
        let enqueued = Arc::clone(&enqueued);
        let throttled = Arc::clone(&throttled);
        let errors = Arc::clone(&errors);
        let done = Arc::clone(&done);
        let latency = Arc::clone(&latency);
        let semaphore = Arc::clone(&semaphore);
        handles.push(tokio::spawn(async move {
            let start = Instant::now();
            // Stride the counter per task so bucket keys spread out.
            let mut counter = task_id as i32;
            // Per-task RNG + reusable buffer for incompressible, row-unique payloads,
            // so the table actually grows on disk instead of compressing to nothing.
            let mut rng = (0x9E37_79B9_7F4A_7C15u64
                ^ (task_id as u64).wrapping_mul(0xD1B5_4A32_D192_ED03))
                | 1;
            let mut buf: Vec<u8> = Vec::with_capacity(payload_bytes);
            let burst_target = per_task_rate > 0;
            let burst_interval = if burst_target {
                Duration::from_secs_f64(BURST as f64 / per_task_rate as f64)
            } else {
                Duration::ZERO
            };
            while !done.load(Ordering::Relaxed) && start.elapsed() < run {
                let burst_start = Instant::now();
                for _ in 0..BURST {
                    fill_random(&mut buf, payload_bytes, &mut rng);
                    // Safe: fill_random only emits printable ASCII (0x30..=0x6F).
                    let payload = std::str::from_utf8(&buf).unwrap();
                    let mut row = GenericRow::new(2);
                    row.set_field(0, counter);
                    row.set_field(1, payload);
                    if await_completions {
                        // Observed-workload mode. Register a completion callback
                        // to measure end-to-end latency, and (when bounded) hold
                        // a backpressure permit for the write's whole lifetime so
                        // the producer cannot outrun the cluster.
                        //
                        // The latency clock starts HERE, before acquiring the
                        // permit, so a backpressure stall counts as user-visible
                        // latency and cannot hide a timeout by converting it into
                        // upstream blocking. When unbounded (max-in-flight 0) the
                        // permit gate is skipped: callbacks still measure latency
                        // but no backpressure is applied (the pre-fix baseline).
                        let want = Instant::now();
                        let permit = if bounded {
                            match Arc::clone(&semaphore).acquire_owned().await {
                                Ok(permit) => Some(permit),
                                Err(_) => break,
                            }
                        } else {
                            None
                        };
                        latency.record_wait(want.elapsed().as_millis() as u64);
                        match writer.append(&row) {
                            Ok(fut) => {
                                enqueued.fetch_add(1, Ordering::Relaxed);
                                latency.on_submit();
                                let stats = Arc::clone(&latency);
                                // The callback runs on the sender's completion
                                // path; it only touches atomics and drops the
                                // permit, so it never blocks the I/O thread.
                                let cb = move |res: Result<()>| {
                                    let elapsed_ms = want.elapsed().as_millis() as u64;
                                    stats.record(&res, elapsed_ms, threshold_ms);
                                    drop(permit);
                                };
                                if let Err((_fut, cb)) =
                                    fut.try_on_complete(cb, WriteCallbackBatch::run)
                                {
                                    // A fresh future never reaches this; drop the
                                    // callback (releasing its permit and undoing
                                    // the in-flight bump) defensively.
                                    latency.in_flight.fetch_sub(1, Ordering::Relaxed);
                                    drop(cb);
                                }
                            }
                            Err(Error::BufferExhausted { .. }) => {
                                drop(permit);
                                throttled.fetch_add(1, Ordering::Relaxed);
                                tokio::time::sleep(Duration::from_millis(20)).await;
                            }
                            Err(_) => {
                                drop(permit);
                                errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    } else {
                        // Fire-and-forget: drop the returned future. Buffer memory is
                        // freed by the sender when the batch is acked (the batch owns the
                        // MemoryPermit), independent of the future, so dropping it is safe.
                        match writer.append(&row) {
                            Ok(_) => {
                                enqueued.fetch_add(1, Ordering::Relaxed);
                            }
                            // Buffer full: the sender cannot drain to the cluster as fast as
                            // we enqueue. This is backpressure, not a write failure, so count
                            // it separately and back off briefly to let the sender catch up.
                            Err(Error::BufferExhausted { .. }) => {
                                throttled.fetch_add(1, Ordering::Relaxed);
                                tokio::time::sleep(Duration::from_millis(20)).await;
                            }
                            Err(_) => {
                                errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    counter = counter.wrapping_add(concurrency as i32);
                }
                if burst_target {
                    if let Some(rem) = burst_interval.checked_sub(burst_start.elapsed()) {
                        tokio::time::sleep(rem).await;
                    }
                } else {
                    // Yield so the sender/background tasks get scheduled.
                    tokio::task::yield_now().await;
                }
            }
            let _ = writer.flush().await;
        }));
    }

    // Per-second monitor.
    let monitor = {
        let enqueued = Arc::clone(&enqueued);
        let throttled = Arc::clone(&throttled);
        let errors = Arc::clone(&errors);
        let done = Arc::clone(&done);
        let latency = Arc::clone(&latency);
        tokio::spawn(async move {
            let start = Instant::now();
            let mut last_rows = 0u64;
            let mut last_thr = 0u64;
            let mut last_err = 0u64;
            let mut last_retry = 0f64;
            let mut last_sent = 0f64;
            let mut last_done = 0u64;
            while !done.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let total = enqueued.load(Ordering::Relaxed);
                let thr = throttled.load(Ordering::Relaxed);
                let err = errors.load(Ordering::Relaxed);
                let rows_s = total.saturating_sub(last_rows);
                let thr_s = thr.saturating_sub(last_thr);
                let err_s = err.saturating_sub(last_err);
                last_rows = total;
                last_thr = thr;
                last_err = err;

                // When metrics are on, sent/s (records the sender actually pushed to the
                // cluster) is the real throughput. enq/s only tracks appends into the
                // local buffer and flat-lines once the buffer saturates, even while the
                // sender keeps draining to the server.
                let mut metric_line = String::new();
                if let Some(handle) = &metrics_handle {
                    let rendered = handle.render();
                    let sent = counter_value(&rendered, "fluss_client_writer_records_send_total")
                        .unwrap_or(0.0);
                    let retry = counter_value(&rendered, "fluss_client_writer_records_retry_total")
                        .unwrap_or(0.0);
                    let sent_s = (sent - last_sent).max(0.0);
                    let retry_s = (retry - last_retry).max(0.0);
                    last_sent = sent;
                    last_retry = retry;
                    metric_line = format!("  sent/s={sent_s:>9.0}  retry/s={retry_s:>9.0}");
                }

                // In callback mode, add the user-facing view: completions/s,
                // outstanding un-acked appends (backpressure depth), failures,
                // how many blew past the threshold (with running ratio), and
                // cumulative tail latency.
                let mut cb_line = String::new();
                if await_completions {
                    let ok = latency.ok.load(Ordering::Relaxed);
                    let failed = latency.failed.load(Ordering::Relaxed);
                    let done_total = ok + failed;
                    let done_s = done_total.saturating_sub(last_done);
                    last_done = done_total;
                    let inflight = latency.in_flight.load(Ordering::Relaxed);
                    let over = latency.over_threshold.load(Ordering::Relaxed);
                    let over_pct = if done_total > 0 {
                        over as f64 * 100.0 / done_total as f64
                    } else {
                        0.0
                    };
                    let max_ms = latency.max_ms.load(Ordering::Relaxed);
                    let buckets = latency.snapshot_buckets();
                    cb_line = format!(
                        "  done/s={done_s:>8}  inflt={inflight:>6}  fail={failed:>7}  >{}s={over:>7}({over_pct:>5.2}%)  p50~{}  p99~{}  max={max_ms}ms",
                        threshold_ms / 1000,
                        fmt_ms(approx_percentile_ms(&buckets, 0.50)),
                        fmt_ms(approx_percentile_ms(&buckets, 0.99)),
                    );
                }

                println!(
                    "t={:>4}s  enq/s={:>8}  thr/s={:>7}  err/s={:>7}  enq_total={:>12}{}{}",
                    start.elapsed().as_secs(),
                    rows_s,
                    thr_s,
                    err_s,
                    total,
                    metric_line,
                    cb_line
                );
            }
        })
    };

    println!(
        "Running for {}s. Trigger the ISR shrink now (big table delete/create, or scale/rebalance). Ctrl-C to stop.",
        args.run_seconds
    );

    for h in handles {
        let _ = h.await;
    }
    done.store(true, Ordering::Relaxed);
    let _ = monitor.await;

    println!(
        "Done. enqueued={} throttled={} append_errors={}",
        enqueued.load(Ordering::Relaxed),
        throttled.load(Ordering::Relaxed),
        errors.load(Ordering::Relaxed)
    );
    if await_completions {
        print_latency_summary(&latency, threshold_ms);
    }
    Ok(())
}

/// Fills `buf` with `len` bytes of pseudo-random printable ASCII using a cheap
/// per-task xorshift PRNG. Random, row-unique content defeats Fluss log
/// compression so the table grows on disk roughly in line with the byte count.
fn fill_random(buf: &mut Vec<u8>, len: usize, state: &mut u64) {
    buf.clear();
    buf.reserve(len);
    let mut written = 0;
    while written < len {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        let take = (len - written).min(8);
        for b in &x.to_le_bytes()[..take] {
            // Map to printable ASCII 0x30..=0x6F to keep the string valid UTF-8.
            buf.push(0x30 + (b & 0x3f));
        }
        written += take;
    }
}

/// Blocks until the table accepts an `acks=all` write or `timeout` elapses. A
/// freshly created (or recreated) table needs its bucket leaders elected before
/// it can be written; a successful append+flush is the strongest ready signal.
async fn wait_until_ready(
    conn: &FlussConnection,
    table_path: &TablePath,
    timeout: Duration,
) -> Result<Duration> {
    let start = Instant::now();
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match probe_write(conn, table_path).await {
            Ok(()) => return Ok(start.elapsed()),
            Err(e) => {
                if start.elapsed() >= timeout {
                    return Err(e);
                }
                if attempt == 1 || attempt % 10 == 0 {
                    println!("table not writable yet (attempt {attempt}): {e}; retrying...");
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

/// Writes and flushes a single probe row. Returns Ok only once the write is
/// acknowledged, i.e. the table's leaders and ISR are up.
async fn probe_write(conn: &FlussConnection, table_path: &TablePath) -> Result<()> {
    let table = conn.get_table(table_path).await?;
    let writer = table.new_append()?.create_writer()?;
    let mut row = GenericRow::new(2);
    row.set_field(0, 0i32);
    row.set_field(1, "ready-probe");
    writer.append(&row)?;
    writer.flush().await
}

/// Parse an unlabeled counter line (`metric_name <value>`) from rendered
/// Prometheus exposition text.
fn counter_value(rendered: &str, name: &str) -> Option<f64> {
    let prefix = format!("{name} ");
    rendered
        .lines()
        .find(|line| line.starts_with(&prefix))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
}
