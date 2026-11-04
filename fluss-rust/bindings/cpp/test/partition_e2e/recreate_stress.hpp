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

// Adapted from isr-storm-cpp on test/rust-isr-callback-bp: per-writer callback
// accounting and rate-limited submission. Reuse the published runner/watchdog.
#pragma once

#include <atomic>
#include <memory>
#include <mutex>

namespace recreate_stress {
int64_t millis() {
    return std::chrono::duration_cast<std::chrono::milliseconds>(Clock::now().time_since_epoch())
        .count();
}
struct Counters {
    std::atomic<int64_t> submitted{0}, accepted{0}, ok{0}, failed{0}, sync_failed{0}, pending{0};
    std::atomic<int64_t> last_ack{millis()}, max_latency_ms{0};
    std::atomic<int64_t> max_ack_gap_ms{0}, table_gone_errors{0};
    std::mutex mutex;
    std::map<int, int64_t> errors;
    void error(int code) {
        if (code == fluss::ErrorCode::TABLE_NOT_EXIST ||
            code == fluss::ErrorCode::INVALID_TABLE_EXCEPTION) {
            ++table_gone_errors;
        }
        std::lock_guard<std::mutex> lock(mutex);
        ++errors[code];
    }
    void latency(int64_t value) {
        auto old = max_latency_ms.load();
        while (old < value && !max_latency_ms.compare_exchange_weak(old, value)) {
        }
    }
};

// The worker owns its writer. Callback captures outlive worker teardown even on
// failure; no synchronous SDK call is made from the callback thread.
struct Workload {
    fluss::Table& table;
    std::string name;
    int writers, rate, payload_bytes;
    std::shared_ptr<Counters> stats = std::make_shared<Counters>();
    std::atomic<bool> stop{false};
    std::vector<std::thread> workers;
    std::vector<fluss::AppendWriter> handles;
    std::atomic<int> worker_errors{0};

    Workload(fluss::Table& table, std::string name, int writers, int rate, int payload_bytes)
        : table(table),
          name(std::move(name)),
          writers(writers),
          rate(rate),
          payload_bytes(payload_bytes) {
        handles.resize(writers);
        fluss::WriteCallbackOptions options;
        options.max_pending_operations = 32768;
        for (auto& writer : handles)
            check("create stress writer", table.NewAppend().CreateWriter(writer, options));
    }
    void start() {
        for (int w = 0; w < writers; ++w) {
            workers.emplace_back([this, w, counts = stats] {
                try {
                    uint64_t sequence = 0;
                    uint64_t random = 0x9e3779b97f4a7c15ULL + w;
                    std::string value(payload_bytes, 'x');
                    auto next = Clock::now();
                    auto interval = std::chrono::nanoseconds(1000000000LL * writers / rate);
                    while (!stop.load()) {
                        // Vary the payload to avoid compressible constant-string load.
                        for (auto& ch : value) {
                            random ^= random << 13;
                            random ^= random >> 7;
                            random ^= random << 17;
                            ch = static_cast<char>('!' + random % 90);
                        }
                        fluss::GenericRow row(2);
                        row.SetString(
                            0, name + ":" + std::to_string(w) + ":" + std::to_string(sequence++));
                        row.SetString(1, value);
                        auto begin = millis();
                        ++counts->submitted;
                        ++counts->pending;
                        auto result = handles[w].Append(row, [counts, begin](
                                                                 const fluss::WriteCompletion& c) {
                            counts->latency(millis() - begin);
                            if (c.result.Ok()) {
                                ++counts->ok;
                                auto now = millis();
                                auto gap = now - counts->last_ack.exchange(now);
                                auto old = counts->max_ack_gap_ms.load();
                                while (old < gap &&
                                       !counts->max_ack_gap_ms.compare_exchange_weak(old, gap)) {
                                }
                            } else {
                                counts->error(c.result.error_code);
                                ++counts->failed;
                            }
                            --counts->pending;
                        });
                        if (result.Ok()) {
                            ++counts->accepted;
                        } else {
                            ++counts->sync_failed;
                            counts->error(result.error_code);
                            --counts->pending;
                        }
                        // Do not create a catch-up burst after buffer admission blocks.
                        next = std::max(next + interval, Clock::now());
                        std::this_thread::sleep_until(next);
                    }
                } catch (const std::exception& e) {
                    ++worker_errors;
                    std::cerr << "WORKER_FAILED " << name << " " << e.what() << std::endl;
                }
            });
        }
    }
    void join() {
        stop = true;
        for (auto& worker : workers)
            if (worker.joinable()) worker.join();
    }
    ~Workload() { join(); }
    void report(const std::string& phase, int64_t start) {
        auto& c = *stats;
        std::cout << "{\"type\":\"sample\",\"phase\":\"" << phase << "\",\"role\":\"" << name
                  << "\",\"elapsed_ms\":" << millis() - start << ",\"submitted\":" << c.submitted
                  << ",\"accepted\":" << c.accepted << ",\"ack_ok\":" << c.ok
                  << ",\"ack_failed\":" << c.failed << ",\"sync_failed\":" << c.sync_failed
                  << ",\"pending\":" << c.pending << ",\"ack_idle_ms\":" << millis() - c.last_ack
                  << ",\"max_latency_ms\":" << c.max_latency_ms
                  << ",\"max_ack_gap_ms\":" << c.max_ack_gap_ms << ",\"error_codes\":{";
        std::lock_guard<std::mutex> lock(c.mutex);
        bool first = true;
        for (auto [code, count] : c.errors) {
            if (!first) std::cout << ',';
            first = false;
            std::cout << '"' << code << "\":" << count;
        }
        std::cout << "}}" << std::endl;
    }
    void flush(bool healthy) {
        for (auto& writer : handles) {
            auto result = writer.Flush();
            std::cout << "FLUSH " << name << " code=" << result.error_code << std::endl;
            if (healthy) check("healthy flush " + name, result);
        }
    }
};

fluss::TableDescriptor descriptor(int buckets) {
    auto schema = fluss::Schema::NewBuilder()
                      .AddColumn("id", fluss::DataType::String())
                      .AddColumn("payload", fluss::DataType::String())
                      .Build();
    return fluss::TableDescriptor::NewBuilder()
        .SetSchema(schema)
        .SetBucketCount(buckets)
        .SetProperty("table.replication.factor", env("PART_REPLICATION", "3"))
        .SetComment("owned drop-recreate isolation test")
        .Build();
}

void acquire(fluss::Connection& connection, const fluss::TablePath& path, fluss::Table& table) {
    const auto deadline = Clock::now() + 30s;
    while (true) {
        auto result = connection.GetTable(path, table);
        if (result.Ok()) return;
        if (Clock::now() >= deadline ||
            (!result.IsRetriable() && result.error_code != fluss::ErrorCode::TABLE_NOT_EXIST))
            check("acquire test table", result);
        std::this_thread::sleep_for(200ms);
    }
}

void run(bool shared) {
    const int rate = std::stoi(env("PART_STRESS_RATE", "20000"));
    const int writers = std::stoi(env("PART_STRESS_WRITERS", "2"));
    const int bytes = std::stoi(env("PART_STRESS_PAYLOAD", "1024"));
    const int baseline = std::stoi(env("PART_STRESS_BASELINE", "30"));
    const int observation = std::stoi(env("PART_STRESS_OBSERVE", "60"));
    const int drain = std::stoi(env("PART_STRESS_DRAIN", "30"));
    const int stall = std::stoi(env("PART_STRESS_STALL", "10"));
    require(rate >= writers && writers > 0 && rate <= 500000 && writers <= 16 && bytes > 0 &&
                bytes <= 4096,
            "invalid stress load");
    require(baseline >= 5 && observation >= 5 && drain > 0 && stall > 0,
            "invalid stress durations");
    auto config = configuration(false);
    config.writer_batch_timeout_ms = 100;
    config.writer_buffer_memory_size = 64 * 1024 * 1024;
    config.writer_buffer_wait_timeout_ms = 1000;
    config.writer_retry_backoff_ms = 100;
    config.writer_retry_max_backoff_ms = 1000;
    fluss::Connection a_connection, b_connection, admin_connection;
    check("A connection", fluss::Connection::Create(config, a_connection));
    if (!shared) check("B connection", fluss::Connection::Create(config, b_connection));
    auto& b_conn = shared ? a_connection : b_connection;
    // DDL must not refresh the writers' shared cache and hide the recovery path.
    check("admin connection", fluss::Connection::Create(config, admin_connection));
    fluss::Admin admin;
    check("admin", admin_connection.GetAdmin(admin));
    const auto base = env("PART_TABLE");
    fluss::TablePath a_path(env("PART_DATABASE"), base + "_a");
    fluss::TablePath b_path(env("PART_DATABASE"), base + "_b");
    auto desc = descriptor(std::stoi(env("PART_BUCKETS", "4")));
    check("create A", admin.CreateTable(a_path, desc, false));
    std::cout << "OWNED_TABLE " << a_path.ToString() << std::endl;
    check("create B", admin.CreateTable(b_path, desc, false));
    std::cout << "OWNED_TABLE " << b_path.ToString() << std::endl;
    fluss::Table a_table, b_table;
    acquire(a_connection, a_path, a_table);
    acquire(b_conn, b_path, b_table);
    const auto a_id = a_table.GetTableInfo().table_id;
    const auto old_id = b_table.GetTableInfo().table_id;
    const auto start = millis();
    std::cout << "STRESS_CONFIG shared=" << shared << " rate_per_table=" << rate
              << " writers_per_table=" << writers << " payload=" << bytes
              << " backoff_ms=100 max_backoff_ms=1000 A=" << a_id << " old_B=" << old_id
              << std::endl;
    Workload a(a_table, "A", writers, rate, bytes);
    Workload b(b_table, "B_old", writers, rate, bytes);
    a.start();
    b.start();
    auto report = [&](const std::string& phase) {
        a.report(phase, start);
        b.report(phase, start);
    };
    for (int i = 0; i < baseline; ++i) {
        std::this_thread::sleep_for(1s);
        report("baseline");
    }
    require(a.stats->ok > 0 && b.stats->ok > 0, "baseline has no ACK; DDL not attempted");
    require(a.worker_errors == 0 && b.worker_errors == 0, "baseline worker failed");
    require(millis() - a.stats->last_ack < 2000 && millis() - b.stats->last_ack < 2000,
            "baseline has stalled; DDL not attempted");
    const auto a_errors_before = a.stats->failed + a.stats->sync_failed;
    const auto b_errors_before = b.stats->failed + b.stats->sync_failed;
    const auto b_gone_before = b.stats->table_gone_errors.load();
    const auto a_ok_before = a.stats->ok.load();
    const auto b_submitted_before = b.stats->submitted.load();
    a.stats->max_ack_gap_ms = 0;
    std::cout << "EVENT drop_start elapsed_ms=" << millis() - start << " old_B=" << old_id
              << std::endl;
    check("drop owned B", admin.DropTable(b_path, false));
    check("recreate owned B", admin.CreateTable(b_path, desc, false));
    fluss::TableInfo new_info;
    check("new B identity", admin.GetTableInfo(b_path, new_info));
    require(new_info.table_id != old_id, "recreation did not change table identity");
    std::cout << "EVENT recreated elapsed_ms=" << millis() - start << " old_B=" << old_id
              << " new_B=" << new_info.table_id << std::endl;
    int64_t max_a_idle = 0;
    auto observe_a = [&] {
        max_a_idle =
            std::max({max_a_idle, millis() - a.stats->last_ack, a.stats->max_ack_gap_ms.load()});
    };
    for (int i = 0; i < observation; ++i) {
        std::this_thread::sleep_for(1s);
        observe_a();
        report("after_recreate");
    }
    // Stop only B first: A must progress while the failed table drains.
    b.join();
    auto deadline = Clock::now() + std::chrono::seconds(drain);
    while (b.stats->pending > 0 && Clock::now() < deadline) {
        std::this_thread::sleep_for(1s);
        observe_a();
        report("drain_old_B");
    }
    const bool old_drained = b.stats->pending == 0;
    if (old_drained) b.flush(false);
    // Do not acquire B through the original connection until after observation:
    // GetTable refreshes metadata and could otherwise mask a stuck old writer.
    bool new_writable = false;
    if (old_drained) {
        fluss::Table new_table;
        acquire(b_conn, b_path, new_table);
        require(new_table.GetTableInfo().table_id == new_info.table_id,
                "new writer got stale table identity");
        Workload fresh(new_table, "B_new", 1, std::min(rate, 1000), bytes);
        fresh.start();
        std::this_thread::sleep_for(2s);
        fresh.join();
        deadline = Clock::now() + std::chrono::seconds(drain);
        while (fresh.stats->pending > 0 && Clock::now() < deadline) {
            std::this_thread::sleep_for(1s);
            observe_a();
            fresh.report("new_writer", start);
        }
        fresh.report("new_writer_final", start);
        new_writable = fresh.stats->pending == 0 && fresh.stats->ok > 0 &&
                       fresh.stats->failed == 0 && fresh.stats->sync_failed == 0 &&
                       fresh.worker_errors == 0;
        if (fresh.stats->pending == 0) fresh.flush(true);
    }
    observe_a();
    a.join();
    deadline = Clock::now() + std::chrono::seconds(drain);
    while (a.stats->pending > 0 && Clock::now() < deadline) {
        std::this_thread::sleep_for(1s);
        report("drain_A");
    }
    report("final");
    const bool a_healthy = a.stats->pending == 0 && a.stats->ok > a_ok_before &&
                           a.stats->failed + a.stats->sync_failed == a_errors_before &&
                           max_a_idle <= stall * 1000LL && a.worker_errors == 0;
    const bool b_failed = b.stats->submitted > b_submitted_before &&
                          b.stats->failed + b.stats->sync_failed > b_errors_before &&
                          b.stats->table_gone_errors > b_gone_before && b.worker_errors == 0;
    std::cout << "{\"type\":\"verdict\",\"old_B_drained\":" << old_drained
              << ",\"old_B_failed\":" << b_failed << ",\"A_healthy\":" << a_healthy
              << ",\"new_B_writable\":" << new_writable << ",\"max_A_idle_ms\":" << max_a_idle
              << ",\"A_id\":" << a_id << ",\"old_B_id\":" << old_id
              << ",\"new_B_id\":" << new_info.table_id << "}" << std::endl;
    if (a.stats->pending == 0) a.flush(true);
    require(old_drained && b_failed && a_healthy && new_writable,
            "drop/recreate isolation contract failed");
    if (env("PART_KEEP") != "1") {
        for (auto [path, expected] : std::vector<std::pair<fluss::TablePath, int64_t>>{
                 {a_path, a_id}, {b_path, new_info.table_id}}) {
            fluss::TableInfo current;
            check("cleanup identity", admin.GetTableInfo(path, current));
            require(current.table_id == expected, "cleanup identity changed");
            check("cleanup owned table", admin.DropTable(path, false));
        }
    }
}
}  // namespace recreate_stress
