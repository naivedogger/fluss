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

//! Experimental FIP-53 manual-assignment offset lifecycle.
//!
//! This is NOT a wire protocol implementation or a consumer group coordinator.
//! A future RPC adapter implements [`GroupService`]; no API keys or protobuf fields
//! are reserved here. The application supplies *processed* next offsets explicitly.
//! No poll position, timer, close, or destructor implicitly commits anything.
//! APIs in this module are prototype-only and have no compatibility guarantee.

use crate::metadata::TableBucket;
use futures::future::BoxFuture;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Explicit response for every requested bucket; `None` means no saved offset.
pub type FetchedOffsets = HashMap<TableBucket, Option<i64>>;
/// Per-bucket outcome. A transport error is separate from these results.
pub type CommitOutcomes = HashMap<TableBucket, Result<(), ServiceError>>;
/// Next record to process, indexed by full table/partition/bucket identity.
pub type NextOffsets = HashMap<TableBucket, i64>;

/// Internal adapter outcomes, deliberately NOT numeric Fluss RPC error codes.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ServiceError {
    /// No live adapter is available in this prototype.
    #[error("FIP-53 live RPC adapter is not implemented")]
    Unsupported,
    /// The adapter knows this request was rejected without applying it.
    #[error("coordinator changed; request was not applied")]
    NotCoordinator,
    /// A definite per-request or per-bucket rejection, not an ambiguous timeout.
    #[error("request rejected: {0}")]
    Rejected(String),
    /// The write may have happened. The client must not blindly retry.
    #[error("commit outcome is unknown: {0}")]
    UnknownOutcome(String),
    /// The caller no longer owns its assignment.
    #[error("consumer ownership was fenced")]
    Fenced,
}

/// Client lifecycle/validation failures, distinct from transport failures.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum OffsetClientError {
    /// Invalid arguments were rejected before contacting the service.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// The response omitted buckets, added buckets, or contained invalid offsets.
    #[error("malformed group-service response")]
    MalformedResponse,
    /// Missing saved offsets require an explicit fallback.
    #[error("no committed offset for {0:?}")]
    MissingOffset(TableBucket),
    /// Assignment and initial positions must first be restored successfully.
    #[error("client has not restored its assignment")]
    NotRestored,
    /// Reinitialization/reassignment is intentionally outside this prototype.
    #[error("client already has an assignment; use a new client for a new assignment")]
    AlreadyRestored,
    /// An ambiguous/fenced/cancelled commit prohibits further commits.
    #[error("client requires external recovery; do not blindly retry or resume")]
    NeedsRecovery,
    /// Close is terminal and never commits.
    #[error("client is closed")]
    Closed,
    /// Failure of the service request, not an absent committed offset.
    #[error(transparent)]
    Service(#[from] ServiceError),
}

/// Missing-offset policy; errors must NEVER be mistaken for missing offsets.
#[derive(Clone, Copy, Debug)]
pub enum MissingOffsetPolicy {
    /// Fail if any assigned bucket has no committed position.
    Fail,
    /// Return the existing scanner's earliest-position sentinel for that bucket.
    Earliest,
}

/// Transport boundary for a future native FIP-53 service adapter.
///
/// Implementations must return exactly one entry per requested bucket, scope
/// requests to the group, and distinguish definite rejection from ambiguous
/// delivery. Manual-assignment ownership is an external precondition.
pub trait GroupService: Send + Sync {
    /// Fetch saved next offsets. Missing records are explicit `None` values.
    fn fetch_offsets<'a>(
        &'a self,
        group_id: &'a str,
        buckets: &'a [TableBucket],
    ) -> BoxFuture<'a, Result<FetchedOffsets, ServiceError>>;

    /// Submit explicitly completed offsets; no atomicity across buckets is assumed.
    fn commit_offsets<'a>(
        &'a self,
        group_id: &'a str,
        offsets: &'a NextOffsets,
    ) -> BoxFuture<'a, Result<CommitOutcomes, ServiceError>>;
}

/// Fail-closed placeholder: constructing a client never enables fake production IO.
pub struct UnavailableGroupService;

impl GroupService for UnavailableGroupService {
    fn fetch_offsets<'a>(
        &'a self,
        _group_id: &'a str,
        _buckets: &'a [TableBucket],
    ) -> BoxFuture<'a, Result<FetchedOffsets, ServiceError>> {
        Box::pin(async { Err(ServiceError::Unsupported) })
    }

    fn commit_offsets<'a>(
        &'a self,
        _group_id: &'a str,
        _offsets: &'a NextOffsets,
    ) -> BoxFuture<'a, Result<CommitOutcomes, ServiceError>> {
        Box::pin(async { Err(ServiceError::Unsupported) })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    New,
    Ready,
    NeedsRecovery,
    Closed,
}

/// Serial, explicit offset client for one fixed, externally assigned bucket set.
///
/// `&mut self` serializes commits on one instance. This does NOT fence another
/// process or prevent late writes from an earlier client lifetime. Recovery after
/// ambiguous writes requires a server-side ordering/fencing contract.
pub struct GroupOffsetClient {
    group_id: String,
    service: Arc<dyn GroupService>,
    state: State,
    assigned: HashSet<TableBucket>,
    lower_bounds: NextOffsets,
    confirmed: NextOffsets,
}

impl GroupOffsetClient {
    /// Construct a prototype client. The service owns persistence, not this client.
    pub fn new(
        group_id: String,
        service: Arc<dyn GroupService>,
    ) -> Result<Self, OffsetClientError> {
        if group_id.trim().is_empty() {
            return Err(OffsetClientError::InvalidArgument(
                "group_id must not be empty".into(),
            ));
        }
        Ok(Self {
            group_id,
            service,
            state: State::New,
            assigned: HashSet::new(),
            lower_bounds: HashMap::new(),
            confirmed: HashMap::new(),
        })
    }

    /// Resolve starts for a fixed manual assignment, then pass them to `subscribe`.
    ///
    /// On any fetch/validation failure, no assignment is installed. The returned
    /// `-2` sentinel is used ONLY for explicitly missing offsets with Earliest.
    /// Installing these positions into a real scanner is the caller's next step.
    pub async fn restore(
        &mut self,
        buckets: Vec<TableBucket>,
        policy: MissingOffsetPolicy,
    ) -> Result<NextOffsets, OffsetClientError> {
        match self.state {
            State::New => {}
            State::Ready => return Err(OffsetClientError::AlreadyRestored),
            State::NeedsRecovery => return Err(OffsetClientError::NeedsRecovery),
            State::Closed => return Err(OffsetClientError::Closed),
        }
        if buckets.is_empty() {
            return Err(OffsetClientError::InvalidArgument(
                "assignment must not be empty".into(),
            ));
        }
        let mut assigned = HashSet::new();
        for bucket in &buckets {
            validate_bucket(bucket)?;
            if !assigned.insert(bucket.clone()) {
                return Err(OffsetClientError::InvalidArgument(
                    "duplicate assigned bucket".into(),
                ));
            }
        }
        let fetched = self.service.fetch_offsets(&self.group_id, &buckets).await?;
        if fetched.len() != assigned.len() || fetched.keys().any(|b| !assigned.contains(b)) {
            return Err(OffsetClientError::MalformedResponse);
        }
        let mut starts = HashMap::new();
        let mut confirmed = HashMap::new();
        for (bucket, saved) in fetched {
            let start = match saved {
                Some(offset) if offset >= 0 => {
                    confirmed.insert(bucket.clone(), offset);
                    offset
                }
                Some(_) => return Err(OffsetClientError::MalformedResponse),
                None => match policy {
                    MissingOffsetPolicy::Fail => {
                        return Err(OffsetClientError::MissingOffset(bucket));
                    }
                    MissingOffsetPolicy::Earliest => -2,
                },
            };
            starts.insert(bucket, start);
        }
        self.assigned = assigned;
        self.lower_bounds = starts.clone();
        self.confirmed = confirmed;
        self.state = State::Ready;
        Ok(starts)
    }

    /// Commit caller-confirmed next offsets; partial failures remain visible.
    ///
    /// No automatic retry is performed. Invalid batches are rejected before IO.
    /// Cancellation or an ambiguous result latches NeedsRecovery: a caller cannot
    /// accidentally send a later commit while the earlier one may still arrive.
    /// This method cannot verify that the application's side effects completed.
    pub async fn commit(
        &mut self,
        offsets: NextOffsets,
    ) -> Result<CommitOutcomes, OffsetClientError> {
        match self.state {
            State::New => return Err(OffsetClientError::NotRestored),
            State::Ready => {}
            State::NeedsRecovery => return Err(OffsetClientError::NeedsRecovery),
            State::Closed => return Err(OffsetClientError::Closed),
        }
        if offsets.is_empty() {
            return Err(OffsetClientError::InvalidArgument(
                "commit must not be empty".into(),
            ));
        }
        for (bucket, offset) in &offsets {
            if !self.assigned.contains(bucket) {
                return Err(OffsetClientError::InvalidArgument(
                    "cannot commit an unassigned bucket".into(),
                ));
            }
            if *offset < 0 || *offset < self.lower_bounds[bucket] {
                return Err(OffsetClientError::InvalidArgument(
                    "negative or regressing next offset; reset is a separate admin operation"
                        .into(),
                ));
            }
        }

        // Do this before awaiting: dropping/cancelling the future is ambiguous too.
        self.state = State::NeedsRecovery;
        let outcomes = match self.service.commit_offsets(&self.group_id, &offsets).await {
            Ok(outcomes) => outcomes,
            Err(error) => {
                if !needs_recovery(&error) {
                    self.state = State::Ready;
                }
                return Err(error.into());
            }
        };
        if outcomes.len() != offsets.len() || outcomes.keys().any(|b| !offsets.contains_key(b)) {
            return Err(OffsetClientError::MalformedResponse);
        }
        let mut uncertain = false;
        for (bucket, outcome) in &outcomes {
            match outcome {
                Ok(()) => {
                    self.lower_bounds.insert(bucket.clone(), offsets[bucket]);
                    self.confirmed.insert(bucket.clone(), offsets[bucket]);
                }
                Err(error) => uncertain |= needs_recovery(error),
            }
        }
        if !uncertain {
            self.state = State::Ready;
        }
        Ok(outcomes)
    }

    /// Locally observed successful commits/fetched offsets, not poll positions.
    pub fn confirmed_offsets(&self) -> &NextOffsets {
        &self.confirmed
    }

    /// Terminal, idempotent close. Never implicitly commits or flushes work.
    pub fn close(&mut self) {
        self.state = State::Closed;
    }
}

fn needs_recovery(error: &ServiceError) -> bool {
    matches!(
        error,
        ServiceError::UnknownOutcome(_) | ServiceError::Fenced
    )
}

fn validate_bucket(bucket: &TableBucket) -> Result<(), OffsetClientError> {
    if bucket.table_id() < 0
        || bucket.bucket_id() < 0
        || bucket.partition_id().is_some_and(|id| id < 0)
    {
        return Err(OffsetClientError::InvalidArgument(
            "table, partition and bucket IDs must be non-negative".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockService {
        saved: Mutex<HashMap<(String, TableBucket), i64>>,
        fetch_error: Mutex<Option<ServiceError>>,
        commit_error: Mutex<Option<ServiceError>>,
        reject_bucket: Mutex<Option<TableBucket>>,
        rejection_error: Mutex<Option<ServiceError>>,
        malformed_fetch: bool,
        malformed_commit: bool,
        pending_commit: bool,
    }

    impl GroupService for MockService {
        fn fetch_offsets<'a>(
            &'a self,
            group: &'a str,
            buckets: &'a [TableBucket],
        ) -> BoxFuture<'a, Result<FetchedOffsets, ServiceError>> {
            Box::pin(async move {
                if let Some(error) = self.fetch_error.lock().unwrap().take() {
                    return Err(error);
                }
                if self.malformed_fetch {
                    return Ok(HashMap::new());
                }
                let saved = self.saved.lock().unwrap();
                Ok(buckets
                    .iter()
                    .map(|b| {
                        (
                            b.clone(),
                            saved.get(&(group.to_owned(), b.clone())).copied(),
                        )
                    })
                    .collect())
            })
        }

        fn commit_offsets<'a>(
            &'a self,
            group: &'a str,
            offsets: &'a NextOffsets,
        ) -> BoxFuture<'a, Result<CommitOutcomes, ServiceError>> {
            Box::pin(async move {
                if self.pending_commit {
                    return futures::future::pending().await;
                }
                if let Some(error) = self.commit_error.lock().unwrap().take() {
                    return Err(error);
                }
                let reject = self.reject_bucket.lock().unwrap().take();
                let rejection_error = self
                    .rejection_error
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap_or_else(|| ServiceError::Rejected("injected rejection".into()));
                let mut saved = self.saved.lock().unwrap();
                let outcomes = offsets
                    .iter()
                    .map(|(b, offset)| {
                        let outcome = if reject.as_ref() == Some(b) {
                            Err(rejection_error.clone())
                        } else {
                            saved.insert((group.to_owned(), b.clone()), *offset);
                            Ok(())
                        };
                        (b.clone(), outcome)
                    })
                    .collect();
                if self.malformed_commit {
                    Ok(HashMap::new())
                } else {
                    Ok(outcomes)
                }
            })
        }
    }

    fn bucket(id: i32) -> TableBucket {
        TableBucket::new(1, id)
    }

    fn offsets(id: i32, offset: i64) -> NextOffsets {
        HashMap::from([(bucket(id), offset)])
    }

    fn client(service: Arc<MockService>) -> GroupOffsetClient {
        GroupOffsetClient::new("orders".into(), service).unwrap()
    }

    #[tokio::test]
    async fn restart_uses_service_offsets_not_client_memory() {
        let service = Arc::new(MockService::default());
        let mut first = client(service.clone());
        assert_eq!(
            first
                .restore(vec![bucket(0)], MissingOffsetPolicy::Earliest)
                .await
                .unwrap(),
            offsets(0, -2)
        );
        assert!(first.confirmed_offsets().is_empty());
        first.commit(offsets(0, 100)).await.unwrap();
        drop(first);
        let mut second = client(service);
        assert_eq!(
            second
                .restore(vec![bucket(0)], MissingOffsetPolicy::Fail)
                .await
                .unwrap(),
            offsets(0, 100)
        );
    }

    #[tokio::test]
    async fn groups_tables_and_partitions_are_isolated() {
        let service = Arc::new(MockService::default());
        let b1 = TableBucket::new_with_partition(1, Some(10), 0);
        let b2 = TableBucket::new_with_partition(1, Some(11), 0);
        let b3 = TableBucket::new(2, 0);
        let mut first = client(service.clone());
        first
            .restore(
                vec![b1.clone(), b2.clone(), b3.clone()],
                MissingOffsetPolicy::Earliest,
            )
            .await
            .unwrap();
        let expected = HashMap::from([(b1.clone(), 10), (b2.clone(), 20), (b3.clone(), 30)]);
        first.commit(expected.clone()).await.unwrap();
        let mut restarted = client(service.clone());
        assert_eq!(
            restarted
                .restore(
                    vec![b1.clone(), b2.clone(), b3.clone()],
                    MissingOffsetPolicy::Fail
                )
                .await
                .unwrap(),
            expected
        );
        let mut other = GroupOffsetClient::new("other".into(), service).unwrap();
        assert_eq!(
            other
                .restore(vec![b1.clone()], MissingOffsetPolicy::Fail)
                .await,
            Err(OffsetClientError::MissingOffset(b1))
        );
    }

    #[tokio::test]
    async fn fetch_error_does_not_fall_back_or_install_assignment() {
        let service = Arc::new(MockService::default());
        *service.fetch_error.lock().unwrap() = Some(ServiceError::NotCoordinator);
        let mut client = client(service);
        assert_eq!(
            client
                .restore(vec![bucket(0)], MissingOffsetPolicy::Earliest)
                .await,
            Err(OffsetClientError::Service(ServiceError::NotCoordinator))
        );
        assert_eq!(
            client.commit(offsets(0, 1)).await,
            Err(OffsetClientError::NotRestored)
        );
        client
            .restore(vec![bucket(0)], MissingOffsetPolicy::Earliest)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn missing_or_invalid_saved_offset_is_not_installed() {
        let service = Arc::new(MockService::default());
        let mut client = client(service.clone());
        assert_eq!(
            client
                .restore(vec![bucket(0)], MissingOffsetPolicy::Fail)
                .await,
            Err(OffsetClientError::MissingOffset(bucket(0)))
        );
        service
            .saved
            .lock()
            .unwrap()
            .insert(("orders".into(), bucket(0)), -1);
        assert_eq!(
            client
                .restore(vec![bucket(0)], MissingOffsetPolicy::Earliest)
                .await,
            Err(OffsetClientError::MalformedResponse)
        );
    }

    #[tokio::test]
    async fn partial_commit_confirms_only_successful_buckets() {
        let service = Arc::new(MockService::default());
        let mut client = client(service.clone());
        client
            .restore(vec![bucket(0), bucket(1)], MissingOffsetPolicy::Earliest)
            .await
            .unwrap();
        *service.reject_bucket.lock().unwrap() = Some(bucket(1));
        let result = client
            .commit(HashMap::from([(bucket(0), 10), (bucket(1), 20)]))
            .await
            .unwrap();
        assert!(result[&bucket(0)].is_ok());
        assert!(result[&bucket(1)].is_err());
        assert_eq!(client.confirmed_offsets(), &offsets(0, 10));
        client.commit(offsets(1, 20)).await.unwrap();
        assert_eq!(client.confirmed_offsets()[&bucket(1)], 20);
    }

    #[tokio::test]
    async fn rejects_invalid_or_regressing_commits_before_io() {
        let service = Arc::new(MockService::default());
        let mut client = client(service.clone());
        client
            .restore(vec![bucket(0)], MissingOffsetPolicy::Earliest)
            .await
            .unwrap();
        for invalid in [HashMap::new(), offsets(0, -1), offsets(1, 10)] {
            assert!(matches!(
                client.commit(invalid).await,
                Err(OffsetClientError::InvalidArgument(_))
            ));
        }
        assert!(service.saved.lock().unwrap().is_empty());
        client.commit(offsets(0, 10)).await.unwrap();
        assert!(matches!(
            client.commit(offsets(0, 9)).await,
            Err(OffsetClientError::InvalidArgument(_))
        ));
        client.commit(offsets(0, 10)).await.unwrap();
    }

    #[tokio::test]
    async fn definite_coordinator_rejection_can_be_submitted_again() {
        let service = Arc::new(MockService::default());
        let mut client = client(service.clone());
        client
            .restore(vec![bucket(0)], MissingOffsetPolicy::Earliest)
            .await
            .unwrap();
        *service.commit_error.lock().unwrap() = Some(ServiceError::NotCoordinator);
        assert_eq!(
            client.commit(offsets(0, 10)).await,
            Err(OffsetClientError::Service(ServiceError::NotCoordinator))
        );
        assert!(client.confirmed_offsets().is_empty());
        assert!(service.saved.lock().unwrap().is_empty());
        client.commit(offsets(0, 10)).await.unwrap();
        assert_eq!(client.confirmed_offsets(), &offsets(0, 10));
    }

    #[tokio::test]
    async fn partial_ambiguous_failure_keeps_successes_but_blocks_further_commits() {
        let service = Arc::new(MockService::default());
        let mut client = client(service.clone());
        client
            .restore(vec![bucket(0), bucket(1)], MissingOffsetPolicy::Earliest)
            .await
            .unwrap();
        *service.reject_bucket.lock().unwrap() = Some(bucket(1));
        *service.rejection_error.lock().unwrap() =
            Some(ServiceError::UnknownOutcome("timeout".into()));
        let outcomes = client
            .commit(HashMap::from([(bucket(0), 10), (bucket(1), 20)]))
            .await
            .unwrap();
        assert!(outcomes[&bucket(0)].is_ok());
        assert!(matches!(
            outcomes[&bucket(1)],
            Err(ServiceError::UnknownOutcome(_))
        ));
        assert_eq!(client.confirmed_offsets(), &offsets(0, 10));
        assert_eq!(
            client.commit(offsets(0, 30)).await,
            Err(OffsetClientError::NeedsRecovery)
        );
    }

    #[tokio::test]
    async fn ambiguous_or_fenced_commit_blocks_later_commits() {
        for error in [
            ServiceError::UnknownOutcome("timeout".into()),
            ServiceError::Fenced,
        ] {
            let service = Arc::new(MockService::default());
            let mut client = client(service.clone());
            client
                .restore(vec![bucket(0)], MissingOffsetPolicy::Earliest)
                .await
                .unwrap();
            *service.commit_error.lock().unwrap() = Some(error.clone());
            assert_eq!(
                client.commit(offsets(0, 10)).await,
                Err(OffsetClientError::Service(error))
            );
            assert_eq!(
                client.commit(offsets(0, 20)).await,
                Err(OffsetClientError::NeedsRecovery)
            );
            assert!(client.confirmed_offsets().is_empty());
        }
    }

    #[tokio::test]
    async fn cancelled_commit_is_also_ambiguous() {
        let service = Arc::new(MockService {
            pending_commit: true,
            ..Default::default()
        });
        let mut client = client(service);
        client
            .restore(vec![bucket(0)], MissingOffsetPolicy::Earliest)
            .await
            .unwrap();
        {
            let pending = client.commit(offsets(0, 10));
            futures::pin_mut!(pending);
            assert!(futures::poll!(pending).is_pending());
        }
        assert_eq!(
            client.commit(offsets(0, 20)).await,
            Err(OffsetClientError::NeedsRecovery)
        );
    }

    #[tokio::test]
    async fn malformed_responses_fail_closed() {
        let mut fetch_client = client(Arc::new(MockService {
            malformed_fetch: true,
            ..Default::default()
        }));
        assert_eq!(
            fetch_client
                .restore(vec![bucket(0)], MissingOffsetPolicy::Earliest)
                .await,
            Err(OffsetClientError::MalformedResponse)
        );
        let mut commit_client = client(Arc::new(MockService {
            malformed_commit: true,
            ..Default::default()
        }));
        commit_client
            .restore(vec![bucket(0)], MissingOffsetPolicy::Earliest)
            .await
            .unwrap();
        assert_eq!(
            commit_client.commit(offsets(0, 10)).await,
            Err(OffsetClientError::MalformedResponse)
        );
        assert_eq!(
            commit_client.commit(offsets(0, 20)).await,
            Err(OffsetClientError::NeedsRecovery)
        );
    }

    #[tokio::test]
    async fn close_and_drop_never_commit() {
        let service = Arc::new(MockService::default());
        let mut client = client(service.clone());
        client
            .restore(vec![bucket(0)], MissingOffsetPolicy::Earliest)
            .await
            .unwrap();
        client.close();
        client.close();
        assert_eq!(
            client.commit(offsets(0, 10)).await,
            Err(OffsetClientError::Closed)
        );
        assert_eq!(
            client
                .restore(vec![bucket(0)], MissingOffsetPolicy::Earliest)
                .await,
            Err(OffsetClientError::Closed)
        );
        drop(client);
        assert!(service.saved.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn validates_group_and_assignment() {
        let service = Arc::new(MockService::default());
        assert!(GroupOffsetClient::new(" ".into(), service.clone()).is_err());
        let mut client = client(service);
        for invalid in [
            vec![],
            vec![bucket(0), bucket(0)],
            vec![bucket(-1)],
            vec![TableBucket::new(-1, 0)],
            vec![TableBucket::new_with_partition(1, Some(-1), 0)],
        ] {
            assert!(matches!(
                client.restore(invalid, MissingOffsetPolicy::Earliest).await,
                Err(OffsetClientError::InvalidArgument(_))
            ));
        }
        client
            .restore(vec![bucket(0)], MissingOffsetPolicy::Earliest)
            .await
            .unwrap();
        assert_eq!(
            client
                .restore(vec![bucket(1)], MissingOffsetPolicy::Earliest)
                .await,
            Err(OffsetClientError::AlreadyRestored)
        );
    }

    #[tokio::test]
    async fn live_service_is_explicitly_unsupported() {
        let mut client =
            GroupOffsetClient::new("orders".into(), Arc::new(UnavailableGroupService)).unwrap();
        assert_eq!(
            client
                .restore(vec![bucket(0)], MissingOffsetPolicy::Earliest)
                .await,
            Err(OffsetClientError::Service(ServiceError::Unsupported))
        );
    }
}
