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

#include "group_offsets.hpp"
#include "fluss.hpp"

#include <iostream>
#include <map>
#include <stdexcept>

namespace proto = fluss::experimental::fip53;

// Deliberately not assert(): checks also run with -DNDEBUG.
void Require(bool condition, const std::string& message) {
    if (!condition) {
        throw std::runtime_error(message);
    }
}

template <class F>
void ExpectError(F&& operation, const std::string& expected) {
    try {
        operation();
    } catch (const std::exception& e) {
        Require(std::string(e.what()).find(expected) != std::string::npos,
                "unexpected error: " + std::string(e.what()));
        return;
    }
    throw std::runtime_error("expected error: " + expected);
}

// Local scanner double: exercises the same subscription signatures as the SDK.
// This does NOT read from Fluss or validate the real fetcher.
struct ScriptedScanner {
    std::map<std::pair<std::optional<int64_t>, int32_t>, int64_t> starts;
    bool reject_bucket_one = false;

    fluss::Result Subscribe(int32_t bucket, int64_t offset) {
        if (reject_bucket_one && bucket == 1) {
            return {-2, "injected scanner error"};
        }
        starts[{std::nullopt, bucket}] = offset;
        return {};
    }

    fluss::Result SubscribePartitionBuckets(int64_t partition, int32_t bucket, int64_t offset) {
        starts[{partition, bucket}] = offset;
        return {};
    }
};

int main() {
    try {
        const proto::Bucket b0{42, std::nullopt, 0};
        const proto::Bucket b1{42, std::nullopt, 1};
        proto::MockGroupServer server;

        // No implicit fallback to this mock when a real adapter is absent.
        auto live = proto::GroupOffsetClient::Create("orders");
        ExpectError([&] { live.Restore({b0}, proto::MissingOffsetPolicy::Earliest); },
                    "not implemented");
        ExpectError([&] { server.NewClient(""); }, "group_id");

        {
            auto first = server.NewClient("orders");
            ExpectError([&] { first.CommitSync({{b0, 1}}); }, "not restored");
            ExpectError([&] { first.Restore({b0, b0}); }, "duplicate");
            ScriptedScanner scanner;
            ExpectError([&] { first.RestoreInto(scanner, 99, {b0}); }, "scanner table");
            const auto starts = first.RestoreInto(
                scanner, 42, {b0, b1}, proto::MissingOffsetPolicy::Earliest);
            Require(starts.size() == 2 && starts[0].next_offset == -2,
                    "missing positions should explicitly start at earliest");
            Require(scanner.starts.size() == 2 &&
                        scanner.starts.at({std::nullopt, 0}) == -2,
                    "restored starts not installed in scanner");

            // This is an offset lifecycle demo, NOT a live data read. A real app
            // installs starts in its scanner and polls/processes actual records.
            // Imagine 100 records were returned, but only 0..39 completed.
            // Commit 40, not the scanner's fetched position of 100.
            const auto result = first.CommitSync({{b0, 40}, {b1, 20}});
            Require(result.size() == 2 && result[0].status == proto::CommitStatus::Success &&
                        result[1].status == proto::CommitStatus::Success,
                    "initial explicit commit failed");
            ExpectError([&] { first.CommitSync({{b0, 40}, {b0, 50}}); }, "duplicate");
            ExpectError([&] { first.CommitSync({{b0, 39}}); }, "regressing");
            ExpectError([&] { first.CommitSync({{{42, std::nullopt, 2}, 10}}); }, "unassigned");
            // Drop without Close: deliberately no final/automatic commit.
        }

        auto restarted = server.NewClient("orders");
        ScriptedScanner resumed_scanner;
        auto restored = restarted.RestoreInto(resumed_scanner, 42, {b0, b1});
        Require(restored[0].next_offset == 40 && restored[1].next_offset == 20,
                "new client must restore only explicitly committed progress");
        Require(resumed_scanner.starts.at({std::nullopt, 0}) == 40,
                "new scanner must actually receive restored next offset");
        std::cout << "SIMULATED restart: bucket 0 resumes at 40, bucket 1 at 20\n";

        auto other = server.NewClient("other-group");
        ExpectError([&] { other.Restore({b0}); }, "no committed offset");
        auto fallback = other.Restore({b0}, proto::MissingOffsetPolicy::Earliest);
        Require(fallback[0].next_offset == -2, "different groups must be isolated");

        server.RejectNextBucket(b1);
        auto partial = restarted.CommitSync({{b0, 50}, {b1, 30}});
        Require(partial[0].status == proto::CommitStatus::Success &&
                    partial[1].status == proto::CommitStatus::Rejected,
                "per-bucket failure must remain visible");
        auto confirmed = restarted.ConfirmedOffsets();
        Require(confirmed[0].next_offset == 50 && confirmed[1].next_offset == 20,
                "failed bucket must not advance local confirmed progress");
        const auto retried = restarted.CommitSync({{b1, 30}});
        Require(retried[0].status == proto::CommitStatus::Success,
                "a definite rejected bucket can be submitted again");

        const proto::Bucket p0{42, 100, 0};
        const proto::Bucket p1{42, 101, 0};
        const proto::Bucket t0{43, std::nullopt, 0};
        auto partitioned = server.NewClient("orders");
        const auto partition_starts = partitioned.Restore(
            {p0, p1, t0}, proto::MissingOffsetPolicy::Earliest);
        for (const auto& start : partition_starts) {
            Require(start.next_offset == -2, "table/partition identities must not collide");
        }
        const auto partition_commits = partitioned.CommitSync({{p0, 5}, {p1, 6}, {t0, 7}});
        for (const auto& outcome : partition_commits) {
            Require(outcome.status == proto::CommitStatus::Success, "partition commit failed");
        }
        auto partition_restart = server.NewClient("orders");
        const auto partition_restored = partition_restart.Restore({p0, p1, t0});
        Require(partition_restored[0].next_offset == 5 &&
                    partition_restored[1].next_offset == 6 &&
                    partition_restored[2].next_offset == 7,
                "partition/table offsets not preserved through C++/Rust conversion");

        auto partition_reader = server.NewClient("orders");
        ScriptedScanner partition_scanner;
        partition_reader.RestoreInto(partition_scanner, 42, {p0, p1});
        Require(partition_scanner.starts.at({100, 0}) == 5 &&
                    partition_scanner.starts.at({101, 0}) == 6,
                "partition-aware subscriptions did not receive saved starts");

        auto broken_reader = server.NewClient("orders");
        ScriptedScanner broken_scanner;
        broken_scanner.reject_bucket_one = true;
        ExpectError([&] { broken_reader.RestoreInto(broken_scanner, 42, {b0, b1}); },
                    "scanner subscription failed");
        ExpectError([&] { broken_reader.CommitSync({{b0, 999}}); }, "closed");
        // The failed, partially initialized scanner must be discarded without Poll.

        server.LoseNextCommitAcknowledgement();
        ExpectError([&] { restarted.CommitSync({{b0, 60}}); }, "unknown");
        ExpectError([&] { restarted.CommitSync({{b0, 70}}); }, "requires external recovery");
        confirmed = restarted.ConfirmedOffsets();
        Require(confirmed[0].next_offset == 50, "lost acknowledgement is not confirmed success");
        restarted.Close();
        restarted.Close();
        ExpectError([&] { restarted.CommitSync({{b0, 80}}); }, "closed");

        std::cout << "PASS: C++ -> cxx -> Rust core -> explicit simulated service\n"
                     "PASS: restore, isolation, partial failure, validation, lost ack, close\n"
                     "No real Fluss RPC, live data scan, process-crash persistence or rebalance tested.\n";
        return 0;
    } catch (const std::exception& e) {
        std::cerr << "FAIL: " << e.what() << '\n';
        return 1;
    }
}
