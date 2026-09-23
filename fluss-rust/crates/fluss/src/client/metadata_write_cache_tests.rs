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

use super::*;
use crate::test_utils::build_cluster;
use std::sync::Barrier;
use std::thread;

fn fixture() -> (Metadata, Arc<PhysicalTablePath>) {
    let path = TablePath::new("db", "write_cache");
    (
        Metadata::new_for_test(Arc::new(build_cluster(&path, 1, 4))),
        Arc::new(PhysicalTablePath::of(Arc::new(path))),
    )
}

fn table_id(metadata: &Metadata, path: &Arc<PhysicalTablePath>) -> Result<i64> {
    metadata.with_write_metadata(path, |_, table| Ok(table.table_id))
}

#[test]
fn stable_reads_borrow_snapshot_without_per_row_arc_clone() {
    let (metadata, path) = fixture();
    for _ in 0..100 {
        metadata
            .with_write_metadata(&path, |cluster, table| {
                // One owner in Metadata, one in this thread's cache. A
                // temporary per-call Arc clone would increase this to three.
                assert_eq!(Arc::strong_count(cluster), 2);
                assert_eq!(table.table_id, 1);
                Ok(())
            })
            .unwrap();
    }
    // A stable cache hit does not even attempt to acquire the shared lock.
    let _writer = metadata.cluster.write();
    assert_eq!(table_id(&metadata, &path).unwrap(), 1);
}

#[test]
fn eviction_and_recreation_refresh_cached_table_info() {
    let (metadata, path) = fixture();
    assert_eq!(table_id(&metadata, &path).unwrap(), 1);
    metadata.evict_table_metadata(path.get_table_path());
    assert_eq!(
        table_id(&metadata, &path).unwrap_err().api_error(),
        Some(FlussError::TableNotExist)
    );
    metadata.replace_cluster(build_cluster(path.get_table_path(), 2, 8));
    metadata
        .with_write_metadata(&path, |_, table| {
            assert_eq!((table.table_id, table.num_buckets), (2, 8));
            Ok(())
        })
        .unwrap();
}

#[test]
fn all_local_invalidations_publish_new_snapshots() {
    let (metadata, path) = fixture();
    let old = metadata
        .with_write_metadata(&path, |cluster, _| Ok(cluster.clone()))
        .unwrap();
    metadata.invalidate_server(&1, vec![1]);
    let invalid_server = metadata
        .with_write_metadata(&path, |cluster, _| Ok(cluster.clone()))
        .unwrap();
    assert!(!Arc::ptr_eq(&old, &invalid_server));
    assert!(Arc::ptr_eq(&invalid_server, &metadata.get_cluster()));

    metadata.invalidate_physical_table_meta(&HashSet::from([path.as_ref().clone()]));
    let invalid_path = metadata
        .with_write_metadata(&path, |cluster, _| Ok(cluster.clone()))
        .unwrap();
    assert!(!Arc::ptr_eq(&invalid_server, &invalid_path));
    assert!(Arc::ptr_eq(&invalid_path, &metadata.get_cluster()));
}

#[test]
fn different_metadata_instances_do_not_share_cached_identity() {
    let (first, path) = fixture();
    let second = Metadata::new_for_test(Arc::new(build_cluster(path.get_table_path(), 42, 4)));
    for _ in 0..10 {
        assert_eq!(table_id(&first, &path).unwrap(), 1);
        assert_eq!(table_id(&second, &path).unwrap(), 42);
    }
}

#[test]
fn reentrant_read_uses_current_snapshot_without_replacing_outer_borrow() {
    let (metadata, path) = fixture();
    metadata
        .with_write_metadata(&path, |outer, table| {
            assert_eq!(table.table_id, 1);
            metadata.replace_cluster(build_cluster(path.get_table_path(), 2, 4));
            assert_eq!(table_id(&metadata, &path).unwrap(), 2);
            assert_eq!(outer.get_table(path.get_table_path()).unwrap().table_id, 1);
            Ok(())
        })
        .unwrap();
    assert_eq!(table_id(&metadata, &path).unwrap(), 2);
}

#[test]
fn switching_tables_never_reuses_another_tables_info() {
    let (metadata, path) = fixture();
    let missing = Arc::new(PhysicalTablePath::of_with_names(
        "db",
        "missing",
        None::<String>,
    ));
    assert_eq!(table_id(&metadata, &path).unwrap(), 1);
    assert!(table_id(&metadata, &missing).is_err());
    assert_eq!(table_id(&metadata, &path).unwrap(), 1);
    let equivalent_path = Arc::new(path.as_ref().clone());
    assert_eq!(table_id(&metadata, &equivalent_path).unwrap(), 1);
}

#[test]
fn readers_refresh_after_each_published_generation() {
    let (metadata, path) = fixture();
    let barrier = Barrier::new(9);
    thread::scope(|scope| {
        for _ in 0..8 {
            let (metadata, path, barrier) = (&metadata, &path, &barrier);
            scope.spawn(move || {
                for expected in 1..=100 {
                    barrier.wait();
                    for _ in 0..10 {
                        assert_eq!(table_id(metadata, path).unwrap(), expected);
                    }
                    barrier.wait();
                }
            });
        }
        for id in 1..=100 {
            metadata.replace_cluster(build_cluster(path.get_table_path(), id, 4));
            barrier.wait();
            barrier.wait();
        }
    });
}

#[test]
fn superseded_cached_snapshot_is_released_on_next_read() {
    let (metadata, path) = fixture();
    let old = metadata
        .with_write_metadata(&path, |cluster, _| Ok(Arc::downgrade(cluster)))
        .unwrap();
    metadata.replace_cluster(build_cluster(path.get_table_path(), 2, 4));
    assert!(old.upgrade().is_some());
    assert_eq!(table_id(&metadata, &path).unwrap(), 2);
    assert!(old.upgrade().is_none());
}

#[test]
fn metadata_read_during_thread_local_destruction_does_not_panic() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::mpsc;

    struct ExitRead {
        metadata: Arc<Metadata>,
        path: Arc<PhysicalTablePath>,
        result: mpsc::Sender<bool>,
    }

    impl Drop for ExitRead {
        fn drop(&mut self) {
            // Contain a regression here: an uncaught panic in a TLS destructor
            // would abort the entire test process rather than just this test.
            let result = catch_unwind(AssertUnwindSafe(|| table_id(&self.metadata, &self.path)));
            self.result.send(matches!(result, Ok(Ok(1)))).unwrap();
        }
    }

    thread_local! {
        static EXIT_READ: RefCell<Option<ExitRead>> = const { RefCell::new(None) };
    }

    let (metadata, path) = fixture();
    let metadata = Arc::new(metadata);
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        // Initialize this first so its destructor runs after WRITE_METADATA's.
        EXIT_READ.with(|slot| {
            *slot.borrow_mut() = Some(ExitRead {
                metadata: metadata.clone(),
                path: path.clone(),
                result: tx,
            });
        });
        assert_eq!(table_id(&metadata, &path).unwrap(), 1);
    })
    .join()
    .unwrap();
    assert!(
        rx.recv().unwrap(),
        "metadata read panicked after cache teardown"
    );
}
