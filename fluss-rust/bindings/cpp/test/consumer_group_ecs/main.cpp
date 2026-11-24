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
#include <chrono>
#include <cstdlib>
#include <iostream>
#include <stdexcept>
#include <string>

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
    if (phase == "seed") {
        auto builder = TableDescriptor::NewBuilder();
        builder.SetSchema(Schema::NewBuilder().AddColumn("id", DataType::Int()).Build())
            .SetBucketCount(1);
        if (!env("CG_RF").empty()) builder.SetProperty("table.replication.factor", env("CG_RF"));
        check(admin.CreateTable(path, builder.Build(), false));
    }
    Table table;
    check(conn.GetTable(path, table));
    TableBucket bucket{table.GetTableInfo().table_id, 0};
    if (phase == "seed") {
        write(table, 0, 6);
        return;
    }
    if (phase == "append") {
        saved(admin, primary, bucket, 6);
        write(table, 6, 8);
        return;
    }
    if (phase == "admin") {
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
        check(admin.DeleteConsumerGroups({primary, other}, removed));
        expect(removed.size() == 2, "missing group deletion result");
        for (const auto& r : removed) {
            check(r.result);
            expect(!r.outcome_unknown, "unknown group deletion");
        }
        saved(admin, other, bucket, -1);
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
