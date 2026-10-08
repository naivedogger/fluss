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

//! Explicit simulation bridge; never selected by a normal SDK connection.

use crate::{RUNTIME, ffi};
use fluss::client::group_offsets_prototype::{
    CommitOutcomes, FetchedOffsets, GroupOffsetClient, GroupService, MissingOffsetPolicy,
    NextOffsets, ServiceError, UnavailableGroupService,
};
use fluss::metadata::TableBucket;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// C++ ownership handle for the Rust prototype.
pub struct PrototypeGroupClient {
    inner: GroupOffsetClient,
}

/// Explicit, in-process simulated service. All state dies with this process.
pub struct PrototypeMockGroupServer {
    service: Arc<MockService>,
}

#[derive(Default)]
struct MockState {
    saved: HashMap<(String, TableBucket), i64>,
    reject_next: Option<TableBucket>,
    lose_ack: bool,
}

#[derive(Default)]
struct MockService {
    state: Mutex<MockState>,
}

impl GroupService for MockService {
    fn fetch_offsets<'a>(
        &'a self,
        group_id: &'a str,
        buckets: &'a [TableBucket],
    ) -> Pin<Box<dyn Future<Output = Result<FetchedOffsets, ServiceError>> + Send + 'a>> {
        Box::pin(async move {
            let state = self
                .state
                .lock()
                .map_err(|_| ServiceError::Rejected("mock state lock poisoned".into()))?;
            Ok(buckets
                .iter()
                .map(|b| {
                    (
                        b.clone(),
                        state.saved.get(&(group_id.to_owned(), b.clone())).copied(),
                    )
                })
                .collect())
        })
    }

    fn commit_offsets<'a>(
        &'a self,
        group_id: &'a str,
        offsets: &'a NextOffsets,
    ) -> Pin<Box<dyn Future<Output = Result<CommitOutcomes, ServiceError>> + Send + 'a>> {
        Box::pin(async move {
            let mut state = self
                .state
                .lock()
                .map_err(|_| ServiceError::Rejected("mock state lock poisoned".into()))?;
            let reject = state.reject_next.take();
            let outcomes = offsets
                .iter()
                .map(|(b, offset)| {
                    let outcome = if reject.as_ref() == Some(b) {
                        Err(ServiceError::Rejected("injected bucket rejection".into()))
                    } else {
                        state
                            .saved
                            .insert((group_id.to_owned(), b.clone()), *offset);
                        Ok(())
                    };
                    (b.clone(), outcome)
                })
                .collect();
            if std::mem::take(&mut state.lose_ack) {
                Err(ServiceError::UnknownOutcome(
                    "simulated lost acknowledgement".into(),
                ))
            } else {
                Ok(outcomes)
            }
        })
    }
}

/// Construct the unavailable live path; no mock or local persistence fallback.
pub fn new_prototype_group_client(group_id: &str) -> Result<Box<PrototypeGroupClient>, String> {
    make_client(group_id, Arc::new(UnavailableGroupService))
}

/// Construct a simulated server for explicit prototype demos/tests only.
pub fn new_prototype_mock_group_server() -> Box<PrototypeMockGroupServer> {
    Box::new(PrototypeMockGroupServer {
        service: Arc::new(MockService::default()),
    })
}

fn make_client(
    group_id: &str,
    service: Arc<dyn GroupService>,
) -> Result<Box<PrototypeGroupClient>, String> {
    GroupOffsetClient::new(group_id.to_owned(), service)
        .map(|inner| Box::new(PrototypeGroupClient { inner }))
        .map_err(|e| e.to_string())
}

impl PrototypeMockGroupServer {
    /// New client lifetime sharing this simulated server's state.
    pub fn mock_client(&self, group_id: &str) -> Result<Box<PrototypeGroupClient>, String> {
        make_client(group_id, self.service.clone())
    }

    /// Inject a definite one-bucket rejection on the next commit request.
    pub fn mock_reject_next_bucket(&self, bucket: ffi::PrototypeBucket) -> Result<(), String> {
        let bucket = from_bucket(bucket)?;
        self.service
            .state
            .lock()
            .map_err(|e| e.to_string())?
            .reject_next = Some(bucket);
        Ok(())
    }

    /// Simulate applying the next commit, but losing its acknowledgement.
    pub fn mock_lose_next_commit_ack(&self) -> Result<(), String> {
        self.service
            .state
            .lock()
            .map_err(|e| e.to_string())?
            .lose_ack = true;
        Ok(())
    }
}

impl PrototypeGroupClient {
    /// Resolve starting positions; the caller installs them in its fresh scanner.
    pub fn prototype_restore(
        &mut self,
        buckets: Vec<ffi::PrototypeBucket>,
        earliest_if_missing: bool,
    ) -> Result<Vec<ffi::PrototypeOffset>, String> {
        let buckets = buckets
            .into_iter()
            .map(from_bucket)
            .collect::<Result<_, _>>()?;
        let policy = if earliest_if_missing {
            MissingOffsetPolicy::Earliest
        } else {
            MissingOffsetPolicy::Fail
        };
        RUNTIME
            .block_on(self.inner.restore(buckets, policy))
            .map(to_offsets)
            .map_err(|e| e.to_string())
    }

    /// Submit explicit completed offsets. Duplicate buckets fail before IO.
    pub fn prototype_commit_sync(
        &mut self,
        offsets: Vec<ffi::PrototypeOffset>,
    ) -> Result<Vec<ffi::PrototypeCommitOutcome>, String> {
        let mut request = HashMap::new();
        for offset in offsets {
            if request
                .insert(from_bucket(offset.bucket)?, offset.next_offset)
                .is_some()
            {
                return Err("duplicate bucket in commit".into());
            }
        }
        let outcomes = RUNTIME
            .block_on(self.inner.commit(request))
            .map_err(|e| e.to_string())?;
        let mut outcomes: Vec<_> = outcomes
            .into_iter()
            .map(|(bucket, outcome)| {
                let (status, message) = match outcome {
                    Ok(()) => (ffi::PrototypeCommitStatus::Success, String::new()),
                    Err(error) => {
                        let status = match &error {
                            ServiceError::Unsupported => ffi::PrototypeCommitStatus::Unsupported,
                            ServiceError::NotCoordinator => {
                                ffi::PrototypeCommitStatus::NotCoordinator
                            }
                            ServiceError::Rejected(_) => ffi::PrototypeCommitStatus::Rejected,
                            ServiceError::UnknownOutcome(_) => {
                                ffi::PrototypeCommitStatus::UnknownOutcome
                            }
                            ServiceError::Fenced => ffi::PrototypeCommitStatus::Fenced,
                        };
                        (status, error.to_string())
                    }
                };
                ffi::PrototypeCommitOutcome {
                    bucket: to_bucket(bucket),
                    status,
                    message,
                }
            })
            .collect();
        outcomes.sort_by_key(|o| bucket_key(&o.bucket));
        Ok(outcomes)
    }

    /// Only observed successful service positions, never a poll cursor.
    pub fn prototype_confirmed(&self) -> Vec<ffi::PrototypeOffset> {
        to_offsets(self.inner.confirmed_offsets().clone())
    }

    /// Idempotent close with no implicit commit.
    pub fn prototype_close(&mut self) {
        self.inner.close();
    }
}

fn from_bucket(bucket: ffi::PrototypeBucket) -> Result<TableBucket, String> {
    if bucket.table_id < 0
        || bucket.bucket_id < 0
        || (bucket.has_partition && bucket.partition_id < 0)
        || (!bucket.has_partition && bucket.partition_id != 0)
    {
        return Err("invalid bucket identity (absent partition must have partition_id=0)".into());
    }
    Ok(TableBucket::new_with_partition(
        bucket.table_id,
        bucket.has_partition.then_some(bucket.partition_id),
        bucket.bucket_id,
    ))
}

fn to_bucket(bucket: TableBucket) -> ffi::PrototypeBucket {
    ffi::PrototypeBucket {
        table_id: bucket.table_id(),
        has_partition: bucket.partition_id().is_some(),
        partition_id: bucket.partition_id().unwrap_or(0),
        bucket_id: bucket.bucket_id(),
    }
}

fn bucket_key(bucket: &ffi::PrototypeBucket) -> (i64, bool, i64, i32) {
    (
        bucket.table_id,
        bucket.has_partition,
        bucket.partition_id,
        bucket.bucket_id,
    )
}

fn to_offsets(offsets: NextOffsets) -> Vec<ffi::PrototypeOffset> {
    let mut offsets: Vec<_> = offsets
        .into_iter()
        .map(|(bucket, next_offset)| ffi::PrototypeOffset {
            bucket: to_bucket(bucket),
            next_offset,
        })
        .collect();
    offsets.sort_by_key(|o| bucket_key(&o.bucket));
    offsets
}
