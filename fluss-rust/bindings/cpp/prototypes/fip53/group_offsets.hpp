/*
 * Licensed to the Apache Software Foundation (ASF) under one
 * or more contributor license agreements.  See the NOTICE file
 * distributed with this work for additional information
 * regarding copyright ownership.  The ASF licenses this file
 * to you under the Apache License, Version 2.0 (the
 * "License"); you may not use this file except in compliance
 * with the License.  You may obtain a copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing,
 * software distributed under the License is distributed on an
 * "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
 * KIND, either express or implied.  See the License for the
 * specific language governing permissions and limitations
 * under the License.
 */

#pragma once

// Experimental, not installed/exported by the SDK. Uses the existing cxx bridge.
#include "fluss-cpp/src/lib.rs.h"

#include <cstdint>
#include <optional>
#include <stdexcept>
#include <string>
#include <utility>
#include <vector>

namespace fluss::experimental::fip53 {

/// Full identity: buckets from different tables/partitions are never interchangeable.
struct Bucket {
    int64_t table_id;
    std::optional<int64_t> partition_id;
    int32_t bucket_id;
};

/// The next position to process, not the last processed record's offset.
struct Offset {
    Bucket bucket;
    int64_t next_offset;
};

/// Only absent saved offsets use this policy; service failures never trigger fallback.
enum class MissingOffsetPolicy { Fail, Earliest };

/// Prototype-local statuses; these are NOT server numeric error codes.
using CommitStatus = ffi::PrototypeCommitStatus;

/// A successful call can still contain failed buckets; callers MUST check each result.
struct CommitOutcome {
    Bucket bucket;
    CommitStatus status;
    std::string message;
};

namespace detail {
inline ffi::PrototypeBucket ToFfi(const Bucket& bucket) {
    return {bucket.table_id, bucket.partition_id.has_value(),
            bucket.partition_id.value_or(0), bucket.bucket_id};
}

inline Bucket FromFfi(const ffi::PrototypeBucket& bucket) {
    return {bucket.table_id,
            bucket.has_partition ? std::optional<int64_t>(bucket.partition_id) : std::nullopt,
            bucket.bucket_id};
}

inline std::vector<Offset> FromFfi(rust::Vec<ffi::PrototypeOffset> offsets) {
    std::vector<Offset> out;
    out.reserve(offsets.size());
    for (const auto& offset : offsets) {
        out.push_back({FromFfi(offset.bucket), offset.next_offset});
    }
    return out;
}
}  // namespace detail

/// Manual-assignment prototype. No heartbeat, assignment negotiation or implicit commit.
///
/// Single-threaded usage only. Request/lifecycle failures throw rust::Error
/// (std::exception); this exception API is not a stable SDK contract.
class GroupOffsetClient {
 public:
    GroupOffsetClient(const GroupOffsetClient&) = delete;
    GroupOffsetClient& operator=(const GroupOffsetClient&) = delete;
    GroupOffsetClient(GroupOffsetClient&&) noexcept = default;
    GroupOffsetClient& operator=(GroupOffsetClient&&) noexcept = default;

    /// Live RPC is intentionally unavailable: Restore throws instead of using a mock.
    static GroupOffsetClient Create(const std::string& group_id) {
        return GroupOffsetClient(ffi::new_prototype_group_client(group_id));
    }

    /// Restore once, then install ALL returned positions into a fresh LogScanner.
    /// On subscription failure, discard the scanner/client; do not poll a partial setup.
    /// Earliest returns -2 only for a genuinely missing saved offset.
    std::vector<Offset> Restore(const std::vector<Bucket>& buckets,
                                MissingOffsetPolicy policy = MissingOffsetPolicy::Fail) {
        rust::Vec<ffi::PrototypeBucket> request;
        request.reserve(buckets.size());
        for (const auto& bucket : buckets) {
            request.push_back(detail::ToFfi(bucket));
        }
        return detail::FromFfi(
            inner_->prototype_restore(std::move(request), policy == MissingOffsetPolicy::Earliest));
    }

    /// Restore and install starts into a fresh, externally owned single-table scanner.
    ///
    /// Compatible with LogScanner and RecordBatchLogScanner. The caller must
    /// supply the actual scanner table ID. On subscription failure this client
    /// is closed; discard the partially initialized scanner without polling it.
    /// This does not create a scanner or verify the supplied table ID remotely.
    template <class Scanner>
    std::vector<Offset> RestoreInto(Scanner& scanner, int64_t table_id,
                                    const std::vector<Bucket>& buckets,
                                    MissingOffsetPolicy policy = MissingOffsetPolicy::Fail) {
        for (const auto& bucket : buckets) {
            if (bucket.table_id != table_id) {
                throw std::invalid_argument("assignment does not match scanner table");
            }
        }
        auto starts = Restore(buckets, policy);
        try {
            for (const auto& start : starts) {
                const auto& bucket = start.bucket;
                auto result = bucket.partition_id
                    ? scanner.SubscribePartitionBuckets(*bucket.partition_id, bucket.bucket_id,
                                                        start.next_offset)
                    : scanner.Subscribe(bucket.bucket_id, start.next_offset);
                if (!result.Ok()) {
                    throw std::runtime_error("scanner subscription failed: " + result.error_message);
                }
            }
        } catch (...) {
            Close();
            throw;
        }
        return starts;
    }

    /// Explicitly submit contiguous processed next offsets. Never pass prefetched positions.
    /// Errors with unknown delivery latch recovery-required; there is no automatic retry.
    std::vector<CommitOutcome> CommitSync(const std::vector<Offset>& offsets) {
        rust::Vec<ffi::PrototypeOffset> request;
        request.reserve(offsets.size());
        for (const auto& offset : offsets) {
            request.push_back({detail::ToFfi(offset.bucket), offset.next_offset});
        }
        auto response = inner_->prototype_commit_sync(std::move(request));
        std::vector<CommitOutcome> outcomes;
        outcomes.reserve(response.size());
        for (const auto& outcome : response) {
            outcomes.push_back(
                {detail::FromFfi(outcome.bucket), outcome.status, std::string(outcome.message)});
        }
        return outcomes;
    }

    /// Last successful service positions observed by this client, not a new server query.
    std::vector<Offset> ConfirmedOffsets() const {
        return detail::FromFfi(inner_->prototype_confirmed());
    }

    /// Idempotent and never commits. Destruction likewise never commits.
    void Close() { inner_->prototype_close(); }

 private:
    friend class MockGroupServer;
    explicit GroupOffsetClient(rust::Box<ffi::PrototypeGroupClient> inner)
        : inner_(std::move(inner)) {}

    rust::Box<ffi::PrototypeGroupClient> inner_;
};

/// Explicit demo-only in-process server. NOT persistent and NOT a production fallback.
///
/// Recreating a client simulates losing client memory while the server survives.
/// Restarting the executable loses all mock server data.
class MockGroupServer {
 public:
    MockGroupServer() : inner_(ffi::new_prototype_mock_group_server()) {}
    MockGroupServer(const MockGroupServer&) = delete;
    MockGroupServer& operator=(const MockGroupServer&) = delete;

    /// Share simulated server state across different client lifetimes/groups.
    GroupOffsetClient NewClient(const std::string& group_id) const {
        return GroupOffsetClient(inner_->mock_client(group_id));
    }

    /// Fail one bucket on the next request, without applying that bucket's write.
    void RejectNextBucket(const Bucket& bucket) {
        inner_->mock_reject_next_bucket(detail::ToFfi(bucket));
    }

    /// Apply the next request, but return an ambiguous acknowledgement failure.
    void LoseNextCommitAcknowledgement() { inner_->mock_lose_next_commit_ack(); }

 private:
    rust::Box<ffi::PrototypeMockGroupServer> inner_;
};

}  // namespace fluss::experimental::fip53
