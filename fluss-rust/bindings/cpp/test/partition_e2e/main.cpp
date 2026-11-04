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

#include <arrow/api.h>

#include <algorithm>
#include <chrono>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <map>
#include <set>
#include <stdexcept>
#include <string>
#include <thread>
#include <unordered_map>
#include <vector>

#include "fluss.hpp"

using namespace std::chrono_literals;
namespace fs = std::filesystem;
using Clock = std::chrono::steady_clock;

std::string env(const char* key, const char* fallback = "") {
    const char* value = std::getenv(key);
    return value ? value : fallback;
}
void require(bool value, const std::string& message) {
    if (!value) throw std::runtime_error(message);
}
void check(const std::string& stage, const fluss::Result& result) {
    if (!result.Ok())
        throw std::runtime_error(stage + ": code=" + std::to_string(result.error_code) + " " +
                                 result.error_message);
}
void stage(const std::string& name) { std::cout << "STAGE " << name << std::endl; }
void arrow_check(const arrow::Status& status) { require(status.ok(), status.ToString()); }
fluss::Configuration configuration(bool dynamic = true) {
    fluss::Configuration c;
    c.bootstrap_servers = env("PART_ENDPOINT");
    c.security_protocol = env("PART_SECURITY", "plaintext");
    c.security_sasl_mechanism = env("PART_MECHANISM", "PLAIN");
    c.security_sasl_username = env("PART_USERNAME");
    c.security_sasl_password = env("PART_PASSWORD");
    c.writer_dynamic_create_partition_enabled = dynamic;
    c.writer_batch_timeout_ms = 5;
    c.lookup_batch_timeout_ms = 5;
    c.lookup_max_retries = std::stoi(env("PART_LOOKUP_RETRIES", "2147483647"));
    c.scanner_log_max_poll_records = 64;
    c.scanner_io_tmp_dir = env("TMPDIR");
    return c;
}

struct Context {
    fluss::Connection connection;
    fluss::Admin admin;
    fluss::Table table;
    fluss::TablePath path{env("PART_DATABASE"), env("PART_TABLE")};
    int buckets = std::stoi(env("PART_BUCKETS", "4"));
    int rows = std::stoi(env("PART_ROWS", "1000"));
    bool created = false;

    explicit Context(bool dynamic = true) {
        stage("connect");
        check("connect", fluss::Connection::Create(configuration(dynamic), connection));
        check("admin", connection.GetAdmin(admin));
    }
    void create(bool pk = false, bool multi = false, bool floating = false) {
        auto schema = fluss::Schema::NewBuilder()
                          .AddColumn("region", floating ? fluss::DataType::Double()
                                                        : fluss::DataType::String())
                          .AddColumn("id", fluss::DataType::Int())
                          .AddColumn("seq", fluss::DataType::Int())
                          .AddColumn("value", fluss::DataType::String());
        if (pk) schema.SetPrimaryKeys({"region", "id", "seq"});
        auto descriptor = fluss::TableDescriptor::NewBuilder()
                              .SetSchema(schema.Build())
                              .SetBucketCount(buckets)
                              .SetBucketKeys({"id"})
                              .SetPartitionKeys(multi ? std::vector<std::string>{"region", "seq"}
                                                      : std::vector<std::string>{"region"})
                              .SetProperty("table.replication.factor", env("PART_REPLICATION", "3"))
                              .Build();
        stage("create " + path.ToString());
        check("create table", admin.CreateTable(path, descriptor, false));
        created = true;
        // Creation can precede metadata visibility while tablets are registering.
        auto deadline = Clock::now() + 30s;
        while (true) {
            auto result = connection.GetTable(path, table);
            if (result.Ok()) break;
            if (Clock::now() >= deadline ||
                (!result.IsRetriable() && result.error_code != fluss::ErrorCode::TABLE_NOT_EXIST)) {
                check("get newly created table", result);
            }
            std::this_thread::sleep_for(200ms);
        }
    }
    void partition(const std::string& p, bool multi = false) {
        std::unordered_map<std::string, std::string> spec{{"region", p}};
        if (multi) spec.emplace("seq", "0");
        check("create partition " + p, admin.CreatePartition(path, spec, false));
    }
    std::vector<fluss::PartitionInfo> partitions() {
        std::vector<fluss::PartitionInfo> result;
        check("list partitions", admin.ListPartitionInfos(path, result));
        for (auto& p : result) {
            std::cout << "PARTITION " << p.partition_name << " id=" << p.partition_id
                      << " buckets=" << p.bucket_count << std::endl;
        }
        return result;
    }
    void cleanup() {
        if (created && env("PART_KEEP") != "1") {
            stage("cleanup");
            check("drop test table", admin.DropTable(path, false));
        }
    }
};

std::string payload(const std::string& region, int index, int version = 0) {
    return region + ":" + std::to_string(index) + ":v" + std::to_string(version);
}
fluss::GenericRow row(const std::string& region, int index, int version = 0) {
    fluss::GenericRow result(4);
    result.SetString(0, region);
    result.SetInt32(1, index / 2);
    result.SetInt32(2, index % 2);
    result.SetString(3, payload(region, index, version));
    return result;
}
fluss::GenericRow key(const std::string& region, int index) {
    fluss::GenericRow result(3);
    result.SetString(0, region);
    result.SetInt32(1, index / 2);
    result.SetInt32(2, index % 2);
    return result;
}
using Values = std::set<std::pair<std::string, int>>;
using Offsets = std::map<std::pair<int64_t, int32_t>, int64_t>;

template <typename Scanner>
void subscribe(Scanner& scanner, const std::vector<fluss::PartitionInfo>& partitions,
               const Offsets& offsets = {}) {
    for (const auto& p : partitions) {
        require(p.bucket_count > 0, "server did not report partition bucket count");
        for (int b = 0; b < p.bucket_count; ++b) {
            auto it = offsets.find({p.partition_id, b});
            check("subscribe", scanner.SubscribePartitionBuckets(
                                   p.partition_id, b, it == offsets.end() ? 0 : it->second));
        }
    }
}
std::map<int64_t, std::string> names(const std::vector<fluss::PartitionInfo>& partitions) {
    std::map<int64_t, std::string> result;
    for (const auto& p : partitions) result.emplace(p.partition_id, p.partition_name);
    return result;
}
void consume(fluss::LogScanner& scanner, const std::map<int64_t, std::string>& partitions,
             Values& seen, Offsets& offsets, size_t wanted, bool one_poll = false) {
    auto deadline = Clock::now() + 30s;
    while (seen.size() < wanted && Clock::now() < deadline) {
        fluss::ScanRecords records;
        check("poll", scanner.Poll(200, records));
        for (size_t b = 0; b < records.BucketCount(); ++b) {
            auto bucket = records.BucketAt(b);
            auto id = bucket.Bucket().partition_id.value();
            for (const auto& record : bucket) {
                auto region = std::string(record.row.GetString(0));
                int index = record.row.GetInt32(1) * 2 + record.row.GetInt32(2);
                require(partitions.at(id) == region, "row routed to the wrong partition");
                require(record.row.GetString(3) == payload(region, index), "payload mismatch");
                require(seen.emplace(region, index).second, "duplicate row");
                offsets[{id, bucket.Bucket().bucket_id}] = record.offset + 1;
            }
        }
        if (one_poll && !records.IsEmpty()) return;
    }
    require(seen.size() == wanted, "scan did not reach the expected record count");
}

void log_case(Context& c, bool arrow_mode) {
    c.create();
    for (auto p : {"A", "B", "C"}) c.partition(p);
    fluss::AppendWriter writer;
    check("writer", c.table.NewAppend().CreateWriter(writer));
    std::vector<fluss::WriteResult> acknowledgments;
    stage("append");
    if (!arrow_mode) {
        for (int i = 0; i < c.rows; ++i) {
            for (auto p : {"A", "B", "C"}) {
                acknowledgments.emplace_back();
                check("append", writer.Append(row(p, i), acknowledgments.back()));
            }
        }
    } else {
        std::shared_ptr<arrow::Schema> schema;
        check("arrow schema", c.table.GetArrowSchema(schema));
        for (auto p : {"A", "B", "C"}) {
            for (int start = 0; start < c.rows; start += 127) {
                arrow::StringBuilder regions, values;
                arrow::Int32Builder ids, sequences;
                for (int i = start; i < std::min(c.rows, start + 127); ++i) {
                    arrow_check(regions.Append(p));
                    arrow_check(ids.Append(i / 2));
                    arrow_check(sequences.Append(i % 2));
                    arrow_check(values.Append(payload(p, i)));
                }
                std::vector<std::shared_ptr<arrow::Array>> columns(4);
                arrow_check(regions.Finish(&columns[0]));
                arrow_check(ids.Finish(&columns[1]));
                arrow_check(sequences.Finish(&columns[2]));
                arrow_check(values.Finish(&columns[3]));
                acknowledgments.emplace_back();
                check("append Arrow",
                      writer.AppendArrowBatch(
                          arrow::RecordBatch::Make(schema, columns[0]->length(), columns),
                          acknowledgments.back()));
            }
        }
    }
    stage("flush");
    check("flush", writer.Flush());
    for (auto& acknowledgment : acknowledgments)
        check("write acknowledgment", acknowledgment.Wait());
    auto parts = c.partitions();
    require(parts.size() == 3, "unexpected partitions");
    for (const auto& p : parts) {
        std::vector<int32_t> buckets;
        for (int b = 0; b < p.bucket_count; ++b) buckets.push_back(b);
        std::unordered_map<int32_t, int64_t> ends;
        check("latest offsets", c.admin.ListPartitionOffsets(c.path, p.partition_name, buckets,
                                                             fluss::OffsetSpec::Latest(), ends));
        int64_t total = 0;
        for (const auto& [bucket, end] : ends) total += end;
        require(ends.size() == buckets.size() && total == c.rows,
                "partition log length differs from submitted rows");
    }
    Values expected;
    for (auto p : {"A", "B", "C"})
        for (int i = 0; i < c.rows; ++i) expected.emplace(p, i);
    Values seen;
    stage("readback");
    if (arrow_mode) {
        fluss::RecordBatchLogScanner scanner;
        check("Arrow scanner", c.table.NewScan().CreateRecordBatchLogScanner(scanner));
        subscribe(scanner, parts);
        fluss::RecordBatchLogReader reader;
        check("bounded reader",
              std::move(scanner).CreateRecordBatchLogReaderUntilLatest(c.admin, reader));
        fluss::ArrowRecordBatches batches;
        check("bounded collect", reader.CollectAllBatches(30000, batches));
        auto mapping = names(parts);
        for (const auto& batch : batches) {
            auto rb = batch->GetArrowRecordBatch();
            auto regions = std::static_pointer_cast<arrow::StringArray>(rb->column(0));
            auto ids = std::static_pointer_cast<arrow::Int32Array>(rb->column(1));
            auto sequences = std::static_pointer_cast<arrow::Int32Array>(rb->column(2));
            auto values = std::static_pointer_cast<arrow::StringArray>(rb->column(3));
            for (int64_t i = 0; i < rb->num_rows(); ++i) {
                auto region = regions->GetString(i);
                int index = ids->Value(i) * 2 + sequences->Value(i);
                require(mapping.at(batch->GetPartitionId()) == region, "Arrow partition mismatch");
                require(values->GetString(i) == payload(region, index), "Arrow payload mismatch");
                require(seen.emplace(region, index).second, "duplicate Arrow row");
            }
        }
    } else {
        Offsets offsets;
        {
            fluss::LogScanner scanner;
            check("scanner", c.table.NewScan().CreateLogScanner(scanner));
            subscribe(scanner, parts);
            consume(scanner, names(parts), seen, offsets, expected.size(), true);
        }
        stage("resume-from-offsets");
        fluss::LogScanner scanner;
        check("resume scanner", c.table.NewScan().CreateLogScanner(scanner));
        subscribe(scanner, parts, offsets);
        consume(scanner, names(parts), seen, offsets, expected.size());
    }
    require(seen == expected, "readback differs from the input set");
}

void pk_case(Context& c) {
    c.create(true);
    for (auto p : {"A", "B", "C"}) c.partition(p);
    fluss::UpsertWriter writer;
    check("writer", c.table.NewUpsert().CreateWriter(writer));
    for (int i = 0; i < c.rows; ++i)
        for (auto p : {"A", "B", "C"}) check("upsert", writer.Upsert(row(p, i)));
    check("insert flush", writer.Flush());
    fluss::Lookuper lookup;
    check("lookuper", c.table.NewLookup().CreateLookuper(lookup));
    auto verify = [&](bool changed) {
        for (int start = 0; start < c.rows; start += 128) {
            std::vector<fluss::PendingLookup> pending;
            std::vector<std::pair<std::string, int>> requested;
            for (int i = start; i < std::min(c.rows, start + 128); ++i) {
                for (auto p : {"A", "B", "C"}) {
                    pending.emplace_back();
                    check("submit lookup", lookup.Lookup(key(p, i), pending.back()));
                    requested.emplace_back(p, i);
                }
            }
            for (size_t n = 0; n < pending.size(); ++n) {
                const auto& [p, i] = requested[n];
                fluss::LookupResult result;
                check("lookup", pending[n].Wait(result, 30000));
                bool deleted = changed && p == "A" && i % 3 == 0;
                require(result.Found() != deleted, "lookup presence mismatch");
                if (!deleted) {
                    require(result.GetString(0) == p && result.GetInt32(1) == i / 2 &&
                                result.GetInt32(2) == i % 2,
                            "lookup returned wrong key");
                    require(result.GetString(3) == payload(p, i, changed && p == "B" ? 1 : 0),
                            "lookup payload mismatch");
                }
            }
        }
    };
    stage("lookup-after-insert");
    verify(false);
    fluss::PrefixLookuper prefix;
    check("prefix lookuper", c.table.NewPrefixLookup({"region", "id"}, prefix));
    fluss::GenericRow prefix_key(2);
    prefix_key.SetString(0, "B");
    prefix_key.SetInt32(1, 0);
    fluss::PrefixLookupResult prefixed;
    check("prefix lookup", prefix.PrefixLookup(prefix_key, prefixed));
    Values prefix_seen;
    for (size_t i = 0; i < prefixed.Size(); ++i) {
        auto r = prefixed.GetRow(i);
        require(r.GetInt32(1) == 0, "wrong prefix key");
        prefix_seen.emplace(std::string(r.GetString(0)), r.GetInt32(2));
    }
    Values prefix_expected{{"B", 0}};
    if (c.rows > 1) prefix_expected.emplace("B", 1);
    require(prefix_seen == prefix_expected && prefixed.Size() == prefix_expected.size(),
            "prefix result mismatch");
    stage("update-delete");
    for (int i = 0; i < c.rows; ++i) {
        check("update", writer.Upsert(row("B", i, 1)));
        if (i % 3 == 0) check("delete", writer.Delete(key("A", i)));
    }
    check("mutation flush", writer.Flush());
    verify(true);
    fluss::LookupResult absent;
    check("absent partition", lookup.Lookup(key("never_created", 0), absent));
    require(!absent.Found(), "missing partition should return empty");
}

void missing_case(Context& c, bool pk) {
    c.create(pk);
    c.partition("B");
    fluss::Connection disabled;
    check("disabled connection", fluss::Connection::Create(configuration(false), disabled));
    fluss::Table table;
    check("disabled table", disabled.GetTable(c.path, table));
    fluss::WriteResult missing, healthy;
    stage("disabled-create-with-healthy-sibling");
    if (pk) {
        fluss::UpsertWriter writer;
        check("disabled writer", table.NewUpsert().CreateWriter(writer));
        check("missing submission", writer.Upsert(row("A", 0), missing));
        check("healthy submission", writer.Upsert(row("B", 0), healthy));
        check("healthy acknowledgment", healthy.Wait());
        auto error = missing.Wait();
        require(!error.Ok() && error.error_code == fluss::ErrorCode::PARTITION_NOT_EXISTS,
                "missing partition must fail with PARTITION_NOT_EXISTS");
    } else {
        fluss::AppendWriter writer;
        check("disabled writer", table.NewAppend().CreateWriter(writer));
        check("missing submission", writer.Append(row("A", 0), missing));
        check("healthy submission", writer.Append(row("B", 0), healthy));
        check("healthy acknowledgment", healthy.Wait());
        auto error = missing.Wait();
        require(!error.Ok() && error.error_code == fluss::ErrorCode::PARTITION_NOT_EXISTS,
                "missing partition must fail with PARTITION_NOT_EXISTS");
    }
    stage("enabled-create");
    if (pk) {
        fluss::UpsertWriter writer;
        check("enabled writer", c.table.NewUpsert().CreateWriter(writer));
        check("auto-create upsert", writer.Upsert(row("A", 0)));
        check("auto-create flush", writer.Flush());
        fluss::Lookuper lookup;
        check("lookuper", c.table.NewLookup().CreateLookuper(lookup));
        for (auto p : {"A", "B"}) {
            fluss::LookupResult result;
            check("readback", lookup.Lookup(key(p, 0), result));
            require(result.Found() && result.GetString(3) == payload(p, 0), "missing PK readback");
        }
    } else {
        fluss::AppendWriter writer;
        check("enabled writer", c.table.NewAppend().CreateWriter(writer));
        check("auto-create append", writer.Append(row("A", 0)));
        check("auto-create flush", writer.Flush());
        auto parts = c.partitions();
        fluss::LogScanner scanner;
        check("scanner", c.table.NewScan().CreateLogScanner(scanner));
        subscribe(scanner, parts);
        Values seen;
        Offsets offsets;
        consume(scanner, names(parts), seen, offsets, 2);
        require(seen == Values{{"A", 0}, {"B", 0}}, "missing Log readback");
    }
}

void lifecycle_case(Context& c) {
    c.create(false, true);
    c.partition("A", true);
    auto before = c.partitions();
    require(before.size() == 1, "expected one partition");
    fluss::AppendWriter writer;
    check("writer", c.table.NewAppend().CreateWriter(writer));
    check("first append", writer.Append(row("A", 0)));
    check("first flush", writer.Flush());
    fluss::LogScanner scanner;
    check("scanner", c.table.NewScan().CreateLogScanner(scanner));
    subscribe(scanner, before);
    check("drop partition", c.admin.DropPartition(c.path, {{"region", "A"}, {"seq", "0"}}, false));
    for (const auto& p : before)
        for (int b = 0; b < p.bucket_count; ++b)
            check("unsubscribe old", scanner.UnsubscribePartition(p.partition_id, b));
    c.partition("A", true);
    c.partition("B", true);
    auto after = c.partitions();
    require(after.size() == 2, "expected recreated and new partitions");
    for (const auto& p : after)
        require(p.partition_id != before[0].partition_id, "recreated partition reused ID");
    std::vector<fluss::PartitionInfo> filtered;
    check("partial spec", c.admin.ListPartitionInfos(c.path, {{"region", "A"}}, filtered));
    require(filtered.size() == 1 && filtered[0].partition_name == "A$0", "partial spec mismatch");
    stage("same-writer-after-recreate");
    fluss::WriteResult recreated, added;
    check("recreated append", writer.Append(row("A", 2), recreated));
    check("new partition append", writer.Append(row("B", 2), added));
    check("recreated flush", writer.Flush());
    check("new partition acknowledgment", added.Wait());
    auto acknowledgment = recreated.Wait();
    if (!acknowledgment.Ok()) {
        require(acknowledgment.error_code == fluss::ErrorCode::PARTITION_NOT_EXISTS,
                "unexpected error after partition recreation");
        stage("old-partition-rejected: reopen connection before resubmitting");
        fluss::Connection fresh;
        check("fresh connection", fluss::Connection::Create(configuration(), fresh));
        fluss::Table table;
        check("fresh table", fresh.GetTable(c.path, table));
        fluss::AppendWriter fresh_writer;
        check("fresh writer", table.NewAppend().CreateWriter(fresh_writer));
        fluss::WriteResult replacement;
        check("replacement append", fresh_writer.Append(row("A", 2), replacement));
        check("replacement acknowledgment", replacement.Wait());
        check("replacement flush", fresh_writer.Flush());
    }
    subscribe(scanner, after);
    auto mapping = names(after);
    for (auto& [id, name] : mapping) name = name.substr(0, name.find('$'));
    Values seen;
    Offsets offsets;
    consume(scanner, mapping, seen, offsets, 2);
    require(seen == Values{{"A", 2}, {"B", 2}}, "recreated partition returned old or missing rows");
}

void float_case(Context& c) {
    c.create(false, false, true);
    // This is a Java-format compatibility probe, not a substitute for Java/C++ E2E.
    c.partition("1_0");
    fluss::GenericRow value(4);
    value.SetFloat64(0, 1.0);
    value.SetInt32(1, 0);
    value.SetInt32(2, 0);
    value.SetString(3, "one");
    fluss::AppendWriter writer;
    check("writer", c.table.NewAppend().CreateWriter(writer));
    check("append float", writer.Append(value));
    check("flush float", writer.Flush());
    auto parts = c.partitions();
    require(parts.size() == 1 && parts[0].partition_name == "1_0",
            "C++ created a different partition for the Java-formatted DOUBLE 1.0");
    std::vector<int32_t> buckets;
    for (int b = 0; b < parts[0].bucket_count; ++b) buckets.push_back(b);
    std::unordered_map<int32_t, int64_t> offsets;
    check("float offsets", c.admin.ListPartitionOffsets(c.path, "1_0", buckets,
                                                        fluss::OffsetSpec::Latest(), offsets));
    int64_t total = 0;
    for (auto [bucket, offset] : offsets) total += offset;
    require(total == 1, "float write absent from Java-named partition");
}

void fault_case(Context& c, bool lookup_mode, bool drop) {
    c.create(lookup_mode);
    for (auto p : {"A", "B"}) c.partition(p);
    auto parts = c.partitions();
    fluss::AppendWriter appender;
    fluss::UpsertWriter upserter;
    fluss::Lookuper lookup;
    fluss::LogScanner scanner;
    if (lookup_mode) {
        check("writer", c.table.NewUpsert().CreateWriter(upserter));
        for (auto p : {"A", "B"}) check("warm upsert", upserter.Upsert(row(p, 0)));
        check("warm flush", upserter.Flush());
        check("lookuper", c.table.NewLookup().CreateLookuper(lookup));
        for (auto p : {"A", "B"}) {
            fluss::LookupResult result;
            check("warm lookup", lookup.Lookup(key(p, 0), result));
            require(result.Found(), "warm lookup missing");
        }
    } else {
        check("writer", c.table.NewAppend().CreateWriter(appender));
        for (auto p : {"A", "B"}) check("warm append", appender.Append(row(p, 0)));
        check("warm flush", appender.Flush());
        check("scanner", c.table.NewScan().CreateLogScanner(scanner));
        subscribe(scanner, parts);
        Values seen;
        Offsets offsets;
        consume(scanner, names(parts), seen, offsets, 2);
    }
    if (drop) check("drop A", c.admin.DropPartition(c.path, {{"region", "A"}}, false));
    const fs::path directory = env("PART_DIR");
    std::ofstream(directory / "fault.ready") << c.path.ToString() << "\n";
    stage("WAIT: migrate B leader or restart its node, then touch recovery.ready after recovery");
    auto deadline = Clock::now() + 8min;
    while (!fs::exists(directory / "recovery.ready")) {
        require(Clock::now() < deadline, "operator recovery marker timeout");
        if (!lookup_mode) {
            fluss::ScanRecords records;
            auto result = scanner.Poll(200, records);
            if (!result.Ok())
                std::cout << "DURING_FAULT " << result.error_code << " " << result.error_message
                          << std::endl;
        } else {
            std::this_thread::sleep_for(100ms);
        }
    }
    stage("verify-recovery-on-original-reader");
    if (lookup_mode) {
        fluss::PendingLookup a, b;
        check("submit A", lookup.Lookup(key("A", 0), a));
        check("submit B", lookup.Lookup(key("B", 0), b));
        fluss::LookupResult result_b;
        check("B must recover", b.Wait(result_b, 60000));
        require(result_b.Found() && result_b.GetString(3) == payload("B", 0), "B lookup mismatch");
        fluss::LookupResult result_a;
        auto result = a.Wait(result_a, 10000);
        require(!a.Available(), "A lookup remains pending after its partition was dropped");
        if (!drop) {
            check("control A", result);
            require(result_a.Found(), "control A missing");
        } else {
            std::cout << "DELETED_LOOKUP_COMPLETED code=" << result.error_code << std::endl;
            require(!result.Ok() || !result_a.Found(), "deleted partition returned a stale row");
        }
    } else {
        // A separate connection avoids refreshing the scanner's shared metadata via the writer.
        fluss::Connection producer;
        check("new producer", fluss::Connection::Create(configuration(), producer));
        fluss::Table table;
        check("producer table", producer.GetTable(c.path, table));
        fluss::AppendWriter writer;
        check("producer writer", table.NewAppend().CreateWriter(writer));
        check("post-recovery append", writer.Append(row("B", 2)));
        check("post-recovery flush", writer.Flush());
        Values seen;
        Offsets offsets;
        consume(scanner, names(parts), seen, offsets, 1);
        require(seen == Values{{"B", 2}}, "B did not resume from its previous position");
    }
    stage("READBACK_OK: verify leader-change and unknown-leader logs before declaring coverage");
}

int main(int argc, char** argv) {
    try {
        require(argc == 2, "usage: partition_e2e CASE");
        std::string mode = argv[1];
        Context c;
        if (mode == "log" || mode == "arrow")
            log_case(c, mode == "arrow");
        else if (mode == "pk")
            pk_case(c);
        else if (mode == "missing-log" || mode == "missing-pk")
            missing_case(c, mode == "missing-pk");
        else if (mode == "lifecycle")
            lifecycle_case(c);
        else if (mode == "float-name")
            float_case(c);
        else if (mode == "scanner-drop" || mode == "scanner-control" || mode == "lookup-drop" ||
                 mode == "lookup-control") {
            fault_case(c, mode.find("lookup") == 0, mode.find("-drop") != std::string::npos);
            std::cout << "RETAINED " << c.path.ToString() << std::endl;
            return 0;
        } else
            throw std::runtime_error("unknown case " + mode);
        c.cleanup();
        stage("PASS");
        return 0;
    } catch (const std::exception& error) {
        std::cerr << "FAIL " << error.what() << "\nTest table retained: " << env("PART_DATABASE")
                  << "." << env("PART_TABLE") << std::endl;
        return 1;
    }
}
