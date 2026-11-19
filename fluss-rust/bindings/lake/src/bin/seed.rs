// SPDX-License-Identifier: Apache-2.0
use fluss::{
    client::FlussConnection,
    config::Config,
    error::FlussError,
    metadata::{DataTypes, Schema, TableDescriptor, TablePath},
    row::GenericRow,
    rpc::message::OffsetSpec,
};
use std::{env, time::Duration};
use tokio::time::{Instant, sleep};

fn row(i: i32, name: &str) -> GenericRow<'static> {
    let mut r = GenericRow::new(2);
    r.set_field(0, i);
    r.set_field(1, name.to_string());
    r
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = env::var("FLUSS_DB")?;
    let mut config = Config {
        bootstrap_servers: env::var("FLUSS_BOOTSTRAP")?,
        security_protocol: env::var("FLUSS_PROTOCOL")?,
        ..Default::default()
    };
    if config.is_sasl_enabled() {
        config.security_sasl_username = env::var("FLUSS_USERNAME")?;
        config.security_sasl_password = env::var("FLUSS_PASSWORD")?;
    } else if !config.security_protocol.eq_ignore_ascii_case("PLAINTEXT") {
        return Err("unsupported protocol".into());
    }
    let conn = FlussConnection::new(config).await?;
    if let Some(cmd) = env::args().nth(1) {
        if cmd != "bounds" {
            return Err("expected bounds or no arguments".into());
        }
        let path = TablePath::new(&db, &env::var("BENCH_TABLE")?);
        let table = fluss_lake::FlussLakeTable::new(std::sync::Arc::new(conn), &path);
        let context = table.capture_read_context().await?;
        if !context.partition_keys().is_empty() {
            return Err("benchmark requires unpartitioned tables".into());
        }
        let snapshot = context
            .lake_snapshot_id()
            .ok_or("no readable lake snapshot")?;
        println!(
            "{} {} {}",
            context.table_id(),
            context.schema_id(),
            snapshot
        );
        let mut ranges: Vec<_> = context
            .log_ranges()
            .iter()
            .map(|r| {
                (
                    r.table_bucket().bucket_id(),
                    r.start_offset(),
                    r.stop_offset(),
                    r.earliest_offset(),
                )
            })
            .collect();
        ranges.sort_unstable();
        for (bucket, start, stop, earliest) in ranges {
            println!("{bucket} {start} {stop} {earliest}");
        }
        return Ok(());
    }
    let prefix = env::var("FIXTURE_PREFIX")?;
    if !prefix.starts_with("sr_probe_")
        || !prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err("invalid fixture prefix".into());
    }
    let bench_rows = env::var("BENCH_ROWS")
        .ok()
        .map(|n| n.parse::<i32>())
        .transpose()?;
    if let Some(n) = bench_rows {
        if !(100..=100_000_000).contains(&n) {
            return Err("BENCH_ROWS must be between 100 and 100000000 per table".into());
        }
    }
    let admin = conn.get_admin()?;
    let mut tables = Vec::new();
    for kind in ["append", "pk"] {
        let path = TablePath::new(&db, &format!("{prefix}_{kind}"));
        let mut schema = Schema::builder()
            .column("id", DataTypes::int())
            .column("name", DataTypes::string());
        if kind == "pk" {
            schema = schema.primary_key(vec!["id"])?;
        }
        let desc = TableDescriptor::builder()
            .schema(schema.build()?)
            .distributed_by(Some(4), vec!["id".into()])
            .property("table.datalake.enabled", "true")
            .custom_property("paimon.file.format", "parquet")
            .build()?;
        admin.create_table(&path, &desc, false).await?;
        println!("created {path}");
        let t = conn.get_table(&path).await?;
        let append = if kind == "append" {
            Some(t.new_append()?.create_writer()?)
        } else {
            None
        };
        let upsert = if kind == "pk" {
            Some(t.new_upsert()?.create_writer()?)
        } else {
            None
        };
        let mut pending = Vec::new();
        for i in 1..=bench_rows.unwrap_or(3) {
            let r = if bench_rows.is_some() {
                row(
                    i,
                    &format!(
                        "baseline-{i:08x}-{:016x}",
                        (i as u64).wrapping_mul(0x9e3779b97f4a7c15)
                    ),
                )
            } else {
                row(i, ["old", "remove", "keep"][(i - 1) as usize])
            };
            pending.push(match (&append, &upsert) {
                (Some(w), _) => w.append(&r)?,
                (_, Some(w)) => w.upsert(&r)?,
                _ => unreachable!(),
            });
            if pending.len() == 1024 {
                futures::future::try_join_all(pending.drain(..)).await?;
            }
        }
        futures::future::try_join_all(pending).await?;
        let stops = admin
            .list_offsets(&path, &[0, 1, 2, 3], OffsetSpec::Latest)
            .await?;
        tables.push((kind, path, stops, t.get_table_info().table_id));
    }
    let deadline = Instant::now()
        + Duration::from_secs(
            env::var("TIER_TIMEOUT")
                .unwrap_or_else(|_| "1800".into())
                .parse()?,
        );
    for (_, path, stops, id) in &tables {
        loop {
            match admin.get_lake_snapshot(path, None, Some(true)).await {
                Ok(s) if s.table_id != *id => return Err("fixture table replaced".into()),
                Ok(s)
                    if stops.iter().all(|(bucket, stop)| {
                        *stop == 0
                            || s.bucket_snapshots.iter().any(|r| {
                                r.bucket_id == *bucket && r.log_offset.is_some_and(|v| v >= *stop)
                            })
                    }) =>
                {
                    println!("{path}: readable snapshot {}", s.snapshot_id);
                    break;
                }
                Err(e) if e.api_error() != Some(FlussError::LakeSnapshotNotExist) => {
                    return Err(e.into());
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "tiering did not cover baseline for {path}; verify the tiering job"
                )
                .into());
            }
            sleep(Duration::from_secs(3)).await;
        }
    }
    for (kind, path, _, id) in tables {
        let t = conn.get_table(&path).await?;
        if t.get_table_info().table_id != id {
            return Err("fixture table replaced".into());
        }
        if let Some(n) = bench_rows {
            let append = if kind == "append" {
                Some(t.new_append()?.create_writer()?)
            } else {
                None
            };
            let upsert = if kind == "pk" {
                Some(t.new_upsert()?.create_writer()?)
            } else {
                None
            };
            let mut pending = Vec::new();
            for i in 1..=n / 10 {
                pending.push(match (&append, &upsert) {
                    (Some(w), _) => w.append(&row(n + i, "tail"))?,
                    (_, Some(w)) if i % 3 == 0 => w.delete(&row(i, ""))?,
                    (_, Some(w)) => {
                        w.upsert(&row(if i % 3 == 1 { i } else { n + i }, "updated"))?
                    }
                    _ => unreachable!(),
                });
                if pending.len() == 1024 {
                    futures::future::try_join_all(pending.drain(..)).await?;
                }
            }
            futures::future::try_join_all(pending).await?;
        } else if kind == "pk" {
            let w = t.new_upsert()?.create_writer()?;
            w.upsert(&row(1, "new"))?.await?;
            w.delete(&row(2, "remove"))?.await?;
            w.upsert(&row(4, "tail"))?.await?;
        } else {
            let w = t.new_append()?.create_writer()?;
            w.append(&row(4, "tail"))?.await?;
        }
    }
    Ok(())
}
