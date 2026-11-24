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

#include <arrow/api.h>

#include <algorithm>
#include <array>
#include <chrono>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <numeric>
#include <stdexcept>
#include <string>
#include <thread>

#include "fluss.hpp"

using namespace fluss;
using Clock = std::chrono::steady_clock;

namespace {
std::string env(const char* name) {
    const char* v = std::getenv(name);
    if (!v) throw std::runtime_error(std::string("Missing setting: ") + name);
    return v;
}
void expect(bool ok, const std::string& message) {
    if (!ok) throw std::runtime_error(message);
}
void check(Result r) { expect(r.Ok(), r.error_message); }
void committed(const OffsetCommitResult& r) {
    check(r.result);
    expect(!r.buckets.empty(), "commit returned no bucket results");
    for (const auto& b : r.buckets) check(b.result);
    expect(r.Ok(), "commit outcome unknown");
}
void saved(Admin& admin, const std::string& group, const TableBucket& bucket, int next) {
    std::vector<CommittedOffset> found;
    check(admin.GetConsumerOffsets(group, {bucket}, found));
    expect(found.size() == 1 && found[0].bucket == bucket, "missing bucket response");
    const auto& offset = found[0].value;
    expect(next < 0 ? !offset : (offset && offset->offset == next),
           "expected saved next offset " + std::to_string(next) + ", got " +
               (offset ? std::to_string(offset->offset) : "absent"));
}
void write(Table& table, int first, int end) {
    arrow::Int32Builder ids;
    for (int i = first; i < end; ++i) {
        auto status = ids.Append(i);
        expect(status.ok(), status.ToString());
    }
    auto array = ids.Finish();
    expect(array.ok(), array.status().ToString());
    AppendWriter writer;
    check(table.NewAppend().CreateWriter(writer));
    check(writer.AppendArrowBatch(arrow::RecordBatch::Make(
        arrow::schema({arrow::field("id", arrow::int32())}), end - first, {*array})));
    check(writer.Flush());
}
void read(LogScanner& scanner, const TableBucket& bucket, int first, int end) {
    auto stop = Clock::now() + std::chrono::seconds(30);
    while (first < end && Clock::now() < stop) {
        ScanRecords batch;
        check(scanner.Poll(1000, batch));
        for (const auto& b : batch.Buckets()) {
            expect(b == bucket, "received another bucket");
            for (const auto& record : batch.Records(b)) {
                expect(first < end && record.offset == first && record.row.GetInt32(0) == first,
                       "unexpected record or offset; expected id " + std::to_string(first));
                ++first;
            }
        }
    }
    expect(first == end, "timed out before id " + std::to_string(first));
}
using Ids = std::array<std::vector<int>, 2>;

void create(Admin& admin, const TablePath& path, int buckets) {
    auto builder = TableDescriptor::NewBuilder();
    builder.SetSchema(Schema::NewBuilder().AddColumn("id", DataType::Int()).Build())
        .SetBucketCount(buckets);
    if (buckets > 1) builder.SetBucketKeys({"id"});
    if (!env("CG_RF").empty()) builder.SetProperty("table.replication.factor", env("CG_RF"));
    check(admin.CreateTable(path, builder.Build(), false));
}

std::filesystem::path file(const std::string& name) {
    return std::filesystem::path(env("CG_OUT")) / name;
}

// The caller publishes a marker only after the cluster reports the requested change.
std::ifstream gate(const std::string& name) {
    std::ofstream(file(name + ".ready")).close();
    auto stop = Clock::now() + std::chrono::minutes(10);
    while (Clock::now() < stop) {
        std::ifstream input(file(name));
        if (input) return input;
        std::this_thread::sleep_for(std::chrono::milliseconds(200));
    }
    throw std::runtime_error("no external evidence for " + name);
}

Ids poll_ids(LogScanner& scanner, int64_t tid, std::array<int, 2> next, int count) {
    Ids ids;
    auto stop = Clock::now() + std::chrono::seconds(30);
    while (count > 0 && Clock::now() < stop) {
        ScanRecords batch;
        check(scanner.Poll(1000, batch));
        for (const auto& b : batch.Buckets()) {
            int i = b.bucket_id;
            expect(i >= 0 && i < 2 && b == TableBucket{tid, i}, "unexpected bucket");
            for (const auto& row : batch.Records(b)) {
                expect(row.offset == next[i]++, "gap or duplicate in bucket");
                ids[i].push_back(row.row.GetInt32(0));
                --count;
            }
        }
    }
    expect(count == 0, "incorrect number of rows");
    return ids;
}

void extended(const std::string& phase, const std::string& group, const TablePath& primary,
              const TableBucket& p, const TablePath& path, Connection& conn, Admin& admin) {
    if (phase == "split-commit") create(admin, path, 2);
    Table table;
    check(conn.GetTable(path, table));
    auto tid = table.GetTableInfo().table_id;
    TableBucket a{tid, 0}, b{tid, 1};
    if (phase == "split-commit") {
        write(table, 100, 164);
        LogScanner scanner;
        check(table.NewScan().WithConsumerGroup({group}).CreateLogScanner(scanner));
        check(scanner.Subscribe({a, b}, OffsetsInitializer::Committed(group)));
        auto ids = poll_ids(scanner, tid, {0, 0}, 64);
        auto all = ids[0];
        all.insert(all.end(), ids[1].begin(), ids[1].end());
        std::sort(all.begin(), all.end());
        for (int i = 0; i < 64; ++i) expect(all[i] == 100 + i, "unexpected source IDs");
        for (int i = 0; i < 2; ++i) {
            expect(ids[i].size() > 3, "need records in both buckets");
            std::ofstream out(file("bucket" + std::to_string(i) + ".ids"));
            out.exceptions(std::ios::badbit | std::ios::failbit);
            for (int v : ids[i]) out << v << '\n';
        }
        committed(scanner.CommitSync({{a, {1, {}}}, {b, {1, {}}}}));
        auto r = scanner.CommitSync(
            {{a, {3, {}}}, {b, {3, std::string(std::stoul(env("CG_BAD_BYTES")), 'x')}}});
        check(r.result);
        expect(!r.Ok() && !r.outcome_unknown && r.buckets.size() == 2,
               "expected per-bucket rejection");
        int successes = 0, rejections = 0;
        for (const auto& entry : r.buckets) {
            expect(!entry.outcome_unknown, "unknown bucket outcome");
            if (entry.bucket == a && entry.result.Ok()) ++successes;
            if (entry.bucket == b &&
                entry.result.error_code == ErrorCode::CONSUMER_OFFSET_METADATA_TOO_LARGE)
                ++rejections;
        }
        expect(successes == 1 && rejections == 1, "expected A success and B metadata-too-large");
        return;
    }
    Ids ids;
    for (int i = 0; i < 2; ++i) {
        std::ifstream in(file("bucket" + std::to_string(i) + ".ids"));
        int v;
        while (in >> v) ids[i].push_back(v);
        expect(ids[i].size() > 3, "missing bucket reference IDs");
    }
    if (phase == "split-resume") {
        saved(admin, group, a, 3);
        saved(admin, group, b, 1);
        LogScanner scanner;
        check(table.NewScan().WithConsumerGroup({group}).CreateLogScanner(scanner));
        check(scanner.Subscribe({a, b},
                                OffsetsInitializer::Committed(group, OffsetsInitializer::Fail())));
        auto actual = poll_ids(scanner, tid, {3, 1}, 60);
        for (int i = 0; i < 2; ++i)
            expect(actual[i] == std::vector<int>(ids[i].begin() + (i == 0 ? 3 : 1), ids[i].end()),
                   "wrong resumed IDs");
        LogScanner multi;
        check(conn.CreateMultiTableLogScanner({primary, path}, {group}, multi));
        check(multi.Subscribe({p, a}, OffsetsInitializer::Earliest()));
        committed(multi.CommitSync({{p, {3, {}}}, {a, {2, {}}}}));
        return;
    }
    RecordBatchLogScanner scanner;
    check(conn.CreateMultiTableRecordBatchLogScanner({primary, path}, {group}, scanner));
    check(scanner.Subscribe({p, a},
                            OffsetsInitializer::Committed(group, OffsetsInitializer::Fail())));
    std::vector<int> primary_ids(8);
    std::iota(primary_ids.begin(), primary_ids.end(), 0);
    std::array<size_t, 2> next{3, 2};
    auto stop = Clock::now() + std::chrono::seconds(30);
    while ((next[0] < 8 || next[1] < ids[0].size()) && Clock::now() < stop) {
        ArrowRecordBatches batches;
        check(scanner.Poll(1000, batches));
        for (const auto& batch : batches) {
            auto t = batch->GetTableId();
            int i = t == p.table_id ? 0 : 1;
            expect((i == 0 || t == tid) && batch->GetBucketId() == 0 &&
                       batch->GetBaseOffset() == static_cast<int64_t>(next[i]),
                   "wrong Arrow source or start");
            auto values = std::dynamic_pointer_cast<arrow::Int32Array>(
                batch->GetArrowRecordBatch()->column(0));
            expect(values != nullptr, "unexpected Arrow schema");
            const auto& reference = i == 0 ? primary_ids : ids[0];
            for (int64_t j = 0; j < values->length(); ++j) {
                expect(next[i] < reference.size() && !values->IsNull(j) &&
                           values->Value(j) == reference[next[i]],
                       "wrong Arrow ID");
                ++next[i];
            }
        }
    }
    expect(next[0] == 8 && next[1] == ids[0].size(), "incomplete cross-table restore");
}

void run(const std::string& phase, const std::string& id) {
    Configuration conf;
    conf.bootstrap_servers = env("CG_ENDPOINT");
    conf.security_protocol = env("CG_SECURITY");
    conf.security_sasl_mechanism = env("CG_MECHANISM");
    conf.security_sasl_username = env("CG_USER");
    conf.security_sasl_password = env("CG_PASSWORD");
    Connection conn;
    check(Connection::Create(conf, conn));
    Admin admin;
    check(conn.GetAdmin(admin));
    const TablePath path(env("CG_DB"), "cg_test_" + id);
    const std::string primary = "cpp-cg-" + id, other = primary + "-auto";
    const std::string extra = primary + "-multi";
    const TablePath split(env("CG_DB"), "cg_split_" + id);
    if (phase == "seed") create(admin, path, 1);
    if (phase == "acl-denial") gate("acl-granted");
    Table table;
    check(conn.GetTable(path, table));
    TableBucket bucket{table.GetTableInfo().table_id, 0};
    if (phase == "seed") {
        write(table, 0, 6);
        return;
    }
    if (phase == "split-commit" || phase == "split-resume" || phase == "cross-arrow") {
        extended(phase, extra, path, bucket, split, conn, admin);
        return;
    }
    if (phase == "owner-move" || phase == "owner-resume") {
        LogScanner scanner;
        check(table.NewScan().WithConsumerGroup({primary}).CreateLogScanner(scanner));
        if (phase == "owner-move") {
            check(scanner.Subscribe(
                {bucket}, OffsetsInitializer::Committed(primary, OffsetsInitializer::Fail())));
            committed(scanner.CommitSync({{bucket, {2, {}}}}));
            auto evidence = gate("owner-moved");
            std::string observed;
            int before = -1, after = -1;
            expect((evidence >> observed >> before >> after) && observed == primary &&
                       before >= 0 && after >= 0 && before != after,
                   "expected group, old owner ID and distinct new owner ID");
            committed(scanner.CommitSync({{bucket, {6, {}}}}));
        } else {
            saved(admin, primary, bucket, 6);
            check(scanner.Subscribe(
                {bucket}, OffsetsInitializer::Committed(primary, OffsetsInitializer::Fail())));
            read(scanner, bucket, 6, 8);
            committed(scanner.CommitSync());
        }
        return;
    }
    if (phase == "acl-denial") {
        saved(admin, extra, bucket, 3);
        LogScanner scanner;
        check(table.NewScan().WithConsumerGroup({extra}).CreateLogScanner(scanner));
        check(scanner.Subscribe({bucket}, OffsetsInitializer::Committed(extra)));
        read(scanner, bucket, 3, 8);
        gate("acl-revoked");
        LogScanner direct;
        check(table.NewScan().CreateLogScanner(direct));
        check(direct.Subscribe(0, 0));
        read(direct, bucket, 0, 8);
        LogScanner denied;
        check(table.NewScan().WithConsumerGroup({extra}).CreateLogScanner(denied));
        auto result = denied.Subscribe(
            {bucket}, OffsetsInitializer::Committed(extra, OffsetsInitializer::Earliest()));
        expect(result.error_code == ErrorCode::AUTHORIZATION_EXCEPTION,
               "expected GROUP authorization error 48; got " + std::to_string(result.error_code) +
                   " " + result.error_message);
        return;
    }
    if (phase == "append") {
        saved(admin, primary, bucket, 6);
        write(table, 6, 8);
        return;
    }
    if (phase == "admin") {
        saved(admin, extra, bucket, 3);
        saved(admin, primary, bucket, 8);
        saved(admin, other, bucket, 8);
        std::vector<std::string> groups;
        check(admin.ListConsumerGroups(groups));
        expect(std::find(groups.begin(), groups.end(), primary) != groups.end(),
               "group not listed");
        committed(admin.AlterConsumerOffsets(primary, {{bucket, {3, "reset"}}}));
        LogScanner restored;
        check(table.NewScan().WithConsumerGroup({primary}).CreateLogScanner(restored));
        check(restored.Subscribe({bucket}, OffsetsInitializer::Committed(primary)));
        read(restored, bucket, 3, 8);
        committed(admin.DeleteConsumerOffsets(primary, {bucket}));
        saved(admin, primary, bucket, -1);
        saved(admin, other, bucket, 8);
        return;
    }
    if (phase == "cleanup") {
        std::vector<ConsumerGroupDeleteResult> removed;
        check(admin.DeleteConsumerGroups({primary, other, extra}, removed));
        expect(removed.size() == 3, "missing group deletion result");
        for (const auto& r : removed) {
            check(r.result);
            expect(!r.outcome_unknown, "unknown group deletion");
        }
        saved(admin, other, bucket, -1);
        check(admin.DropTable(split, false));
        check(admin.DropTable(path, false));
        return;
    }
    bool automatic = phase == "isolated-auto";
    const std::string& group = automatic ? other : primary;
    LogScanner scanner;
    check(table.NewScan()
              .WithConsumerGroup({group, 30000, automatic ? 5u : 0u})
              .CreateLogScanner(scanner));
    check(scanner.Subscribe({bucket}, OffsetsInitializer::Committed(group)));
    if (phase == "no-commit" || phase == "partial") {
        saved(admin, group, bucket, -1);
        read(scanner, bucket, 0, 6);
        if (phase == "partial") {
            committed(scanner.CommitSync({{bucket, {2, "processed ids 0 and 1"}}}));
            saved(admin, group, bucket, 2);
        }
    } else if (phase == "resume" || phase == "tail") {
        bool tail = phase == "tail";
        saved(admin, group, bucket, tail ? 6 : 2);
        read(scanner, bucket, tail ? 6 : 2, tail ? 8 : 6);
        committed(tail ? scanner.CommitSync() : scanner.CommitAsync().get());
        saved(admin, group, bucket, tail ? 8 : 6);
    } else if (automatic) {
        saved(admin, group, bucket, -1);
        read(scanner, bucket, 0, 8);
        // Auto-commit is poll-driven. No explicit commit is issued by this group.
        auto stop = Clock::now() + std::chrono::seconds(20);
        do {
            ScanRecords empty;
            check(scanner.Poll(200, empty));
            expect(empty.Count() == 0, "unexpected additional data");
            std::vector<CommittedOffset> found;
            check(admin.GetConsumerOffsets(group, {bucket}, found));
            if (found.size() == 1 && found[0].value && found[0].value->offset == 8) return;
        } while (Clock::now() < stop);
        throw std::runtime_error("auto-commit did not persist 8");
    } else {
        throw std::runtime_error("unknown phase");
    }
}
}  // namespace

int main(int argc, char** argv) {
    if (argc != 3) return 2;
    try {
        run(argv[1], argv[2]);
    } catch (const std::exception& e) {
        std::cerr << argv[1] << ": " << e.what() << '\n';
        return 1;
    }
    std::cout << "PASS " << argv[1] << '\n';
}
