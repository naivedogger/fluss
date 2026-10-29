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

#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <memory>
#include <numeric>
#include <thread>
#include <vector>

#include "fluss.hpp"
#include "upgrade_rate.hpp"

namespace fs = std::filesystem;
using Clock = std::chrono::steady_clock;
constexpr int PROCESSES = 4, THREADS = 5, LANES = PROCESSES * THREADS, BINS = 60002;
std::string env(const char* key, const char* fallback = "") {
    const char* value = std::getenv(key);
    return value ? value : fallback;
}
int64_t number(const char* key, int64_t fallback) {
    return std::stoll(env(key, std::to_string(fallback).c_str()));
}
int64_t wall() {
    return std::chrono::duration_cast<std::chrono::milliseconds>(
               std::chrono::system_clock::now().time_since_epoch())
        .count();
}
void ck(const char* action, const fluss::Result& result) {
    if (!result.Ok()) throw std::runtime_error(std::string(action) + ": " + result.error_message);
}
void save(const fs::path& path, const std::string& value) {
    fs::path temp = path.string() + ".tmp";
    std::ofstream out(temp);
    out << value << '\n';
    out.close();
    if (!out) throw std::runtime_error("cannot save " + path.string());
    fs::rename(temp, path);
}
struct Histogram {
    std::array<std::atomic<uint64_t>, BINS> bins{};
    void add(int64_t ms) { bins[std::min<int64_t>(std::max<int64_t>(ms, 0), BINS - 1)]++; }
    std::string take() {
        std::string value = "[";
        bool first = true;
        for (int i = 0; i < BINS; i++) {
            auto n = bins[i].exchange(0);
            if (n) {
                if (!first) value += ",";
                value += "[" + std::to_string(i) + "," + std::to_string(n) + "]";
                first = false;
            }
        }
        return value + "]";
    }
};
fluss::Configuration configuration() {
    fluss::Configuration c;
    c.bootstrap_servers = env("OOO_ENDPOINT");
    c.security_protocol = env("OOO_SECURITY_PROTOCOL");
    c.security_sasl_mechanism = env("OOO_SASL_MECHANISM");
    c.security_sasl_username = env("OOO_USERNAME");
    c.security_sasl_password = env("OOO_PASSWORD");
    c.writer_dynamic_batch_size_enabled = true;
    c.writer_dynamic_batch_size_min = 256 * 1024;
    c.writer_batch_size = 2 * 1024 * 1024;
    c.writer_batch_timeout_ms = 100;
    c.writer_buffer_memory_size = 256 * 1024 * 1024;
    c.writer_buffer_wait_timeout_ms = 1000;
    c.writer_max_inflight_requests_per_bucket = 5;
    c.writer_enable_idempotence = true;
    c.writer_acks = "all";
    c.scanner_log_max_poll_records = 20000;
    return c;
}

int execute(const std::string& mode, const std::string& name, const fs::path& dir, int proc) {
    fluss::Connection connection;
    ck("connection", fluss::Connection::Create(configuration(), connection));
    fluss::Admin admin;
    ck("admin", connection.GetAdmin(admin));
    const int buckets = number("UP_BUCKETS", 64);
    const int payload = number("UP_PAYLOAD", 1024);
    fluss::TablePath path(env("OOO_DATABASE"), name);
    std::vector<int32_t> ids(buckets);
    std::iota(ids.begin(), ids.end(), 0);
    if (mode == "setup") {
        auto create = [&](const std::string& tableName, int n) {
            auto descriptor = fluss::TableDescriptor::NewBuilder()
                                  .SetSchema(fluss::Schema::NewBuilder()
                                                 .AddColumn("id", fluss::DataType::BigInt())
                                                 .AddColumn("sent_ms", fluss::DataType::BigInt())
                                                 .AddColumn("value", fluss::DataType::String())
                                                 .Build())
                                  .SetBucketCount(n)
                                  .SetBucketKeys({"id"})
                                  .SetProperty("table.replication.factor", "3")
                                  .Build();
            fluss::TablePath target(env("OOO_DATABASE"), tableName);
            ck("create", admin.CreateTable(target, descriptor, false));
            std::vector<int32_t> bs(n);
            std::iota(bs.begin(), bs.end(), 0);
            auto deadline = Clock::now() + std::chrono::seconds(300);
            bool ready = false;
            while (Clock::now() < deadline) {
                std::unordered_map<int32_t, int64_t> offsets;
                auto r = admin.ListOffsets(target, bs, fluss::OffsetSpec::Latest(), offsets);
                if (r.Ok() && offsets.size() == bs.size()) {
                    ready = true;
                    break;
                }
                std::this_thread::sleep_for(std::chrono::seconds(1));
            }
            if (!ready) throw std::runtime_error("readiness timeout for " + tableName);
            fluss::Table table;
            ck("table", connection.GetTable(target, table));
            std::cout << "TABLE {\"name\":\"" << tableName
                      << "\",\"id\":" << table.GetTableInfo().table_id << ",\"buckets\":" << n
                      << "}" << std::endl;
        };
        create(name, buckets);
        for (int i = 0; i < number("UP_AUX_TABLES", 30); i++)
            create(name + "_meta_" + std::to_string(i), number("UP_AUX_BUCKETS", 20));
        return 0;
    }
    fluss::Table table;
    ck("table", connection.GetTable(path, table));
    if (table.GetTableInfo().table_id != number("UP_TABLE_ID", -1))
        throw std::runtime_error("table ID mismatch");

    if (mode == "write") {
        struct Stats {
            std::atomic<uint64_t> accepted{0}, completed{0}, failed{0}, rejected{0};
            Histogram latency;
        };
        auto stats = std::make_shared<Stats>();
        std::atomic<bool> stop{false}, fatal{false};
        std::array<uint64_t, THREADS> counts{};
        std::vector<fluss::AppendWriter> writers(THREADS);
        std::vector<std::vector<fluss::GenericRow>> pools(THREADS);
        for (int t = 0; t < THREADS; t++) {
            ck("writer", table.NewAppend().CreateWriter(writers[t]));
            uint64_t seed = 17 + proc * THREADS + t;
            pools[t].resize(2048);
            for (auto& row : pools[t]) {
                std::string text(payload, ' ');
                for (char& ch : text) {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    ch = '!' + seed % 64;
                }
                row.SetInt64(0, 0);
                row.SetInt64(1, 0);
                row.SetString(2, text);
            }
        }
        save(dir / ("ready-w-" + std::to_string(proc)), "ready rate-control-v1");
        while (!fs::exists(dir / "start"))
            std::this_thread::sleep_for(std::chrono::milliseconds(100));
        std::atomic<int64_t> writeBps{number("UP_WRITE_BPS", 524288000)};
        int64_t rateRequest = 0;
        std::vector<std::thread> workers;
        for (int t = 0; t < THREADS; t++)
            workers.emplace_back([&, t, stats] {
                try {
                    WritePacer pacer(payload, LANES);
                    while (!stop && !fatal) {
                        if (!pacer.acquire(writeBps)) {
                            std::this_thread::sleep_until(pacer.wakeup(Clock::now()));
                            continue;
                        }
                        uint64_t seq = counts[t];
                        auto& row = pools[t][seq % pools[t].size()];
                        row.SetInt64(0, seq * LANES + proc * THREADS + t);
                        row.SetInt64(1, wall());
                        bool sample = seq % 256 == 0;
                        auto begin = sample ? Clock::now() : Clock::time_point{};
                        auto result = writers[t].Append(
                            row, [stats, sample, begin](const fluss::WriteCompletion& r) {
                                if (sample)
                                    stats->latency.add(
                                        std::chrono::duration_cast<std::chrono::milliseconds>(
                                            Clock::now() - begin)
                                            .count());
                                if (r.result.Ok())
                                    stats->completed++;
                                else
                                    stats->failed++;
                            });
                        if (result.Ok()) {
                            counts[t]++;
                            stats->accepted++;
                        } else {
                            stats->rejected++;
                            // These capacity errors occur before submission, so the ID can be
                            // reused.
                            const auto& message = result.error_message;
                            if (message.find("buffer exhausted") == std::string::npos &&
                                message != "Write callback capacity is full" &&
                                message != "Timed out waiting for write callback capacity") {
                                ck("append", result);
                            }
                            std::this_thread::sleep_for(std::chrono::milliseconds(100));
                        }
                    }
                    ck("flush", writers[t].Flush());
                } catch (const std::exception& e) {
                    fatal = true;
                    std::cerr << "writer " << t << ": " << e.what() << std::endl;
                }
            });
        uint64_t previous = 0;
        int64_t lastAck = wall();
        auto sample = [&] {
            auto a = stats->accepted.load(), c = stats->completed.load(), f = stats->failed.load();
            const auto now = wall();
            if (c != previous || (writeBps.load(std::memory_order_relaxed) == 0 && a == c + f)) {
                lastAck = now;
                previous = c;
            }
            std::string value =
                "{\"unix_ms\":" + std::to_string(now) + ",\"accepted\":" + std::to_string(a) +
                ",\"completed\":" + std::to_string(c) + ",\"errors\":" + std::to_string(f) +
                ",\"rejected_appends\":" + std::to_string(stats->rejected.load()) +
                ",\"pending\":" + std::to_string(a > c + f ? a - c - f : 0) +
                ",\"target_write_mib\":" + std::to_string(writeBps.load() / 1024 / 1024) +
                ",\"rate_request_id\":" + std::to_string(rateRequest) +
                ",\"no_ack_ms\":" + std::to_string(now - lastAck) +
                ",\"latency_hist_ms\":" + stats->latency.take() + "}";
            save(dir / ("writer-" + std::to_string(proc) + ".json"), value);
            std::cout << "SAMPLE " << value << std::endl;
        };
        while (!fs::exists(dir / "stop") && !fatal) {
            std::this_thread::sleep_for(std::chrono::seconds(1));
            // Atomic file replacement by set-rate prevents observing a partial command.
            std::ifstream control(dir / "write-rate");
            int64_t request, mib;
            std::string extra;
            if (control >> request >> mib) {
                if (!(control >> extra) && request > 0 && request != rateRequest && mib >= 0 &&
                    mib <= 1024 * 1024) {
                    writeBps.store(mib * 1024 * 1024, std::memory_order_relaxed);
                    rateRequest = request;
                    std::cout << "RATE_APPLIED {\"unix_ms\":" << wall()
                              << ",\"request_id\":" << request << ",\"write_mib\":" << mib << "}"
                              << std::endl;
                }
            }
            sample();
        }
        stop = true;
        for (auto& worker : workers) worker.join();
        sample();
        std::string saved;
        for (auto n : counts) saved += std::to_string(n) + " ";
        save(dir / ("counts-" + std::to_string(proc)), saved);
        return fatal || stats->failed || stats->accepted != stats->completed ? 2 : 0;
    }

    if (mode == "read" || mode == "verify") {
        const bool verify = mode == "verify";
        const int parts = number("UP_READERS", 4);
        std::vector<std::vector<bool>> seen(LANES);
        uint64_t expected = 0;
        if (verify) {
            for (int p = 0; p < PROCESSES; p++) {
                std::ifstream counts(dir / ("counts-" + std::to_string(p)));
                for (int t = 0; t < THREADS; t++) {
                    uint64_t n;
                    if (!(counts >> n)) throw std::runtime_error("missing final counts");
                    seen[p * THREADS + t].resize(n);
                    expected += n;
                }
            }
        }
        fluss::LogScanner scanner;
        ck("scanner", table.NewScan().CreateLogScanner(scanner));
        for (int b : ids)
            if (verify || b % parts == proc) ck("subscribe", scanner.Subscribe(b, 0));
        uint64_t rows = 0, bytes = 0, invalid = 0, duplicates = 0;
        Histogram pollLatency, age;
        auto sample = [&] {
            std::string value =
                "{\"unix_ms\":" + std::to_string(wall()) + ",\"rows\":" + std::to_string(rows) +
                ",\"bytes\":" + std::to_string(bytes) + ",\"poll_hist_ms\":" + pollLatency.take() +
                ",\"data_age_hist_ms\":" + age.take() + "}";
            save(dir / ("reader-" + std::to_string(proc) + ".json"), value);
            std::cout << "SAMPLE " << value << std::endl;
        };
        const double rate = double(number("UP_READ_BPS", 314572800)) / parts;
        auto tick = Clock::now(), lastSample = tick;
        const auto deadline = tick + std::chrono::seconds(number("UP_VERIFY_SECONDS", 14400));
        if (!verify) save(dir / ("ready-r-" + std::to_string(proc)), "ready");
        while (verify ? rows < expected && Clock::now() < deadline
                      : !fs::exists(dir / "reader.stop")) {
            fluss::ScanRecords records;
            auto begin = Clock::now();
            ck("poll", scanner.Poll(1000, records));
            pollLatency.add(
                std::chrono::duration_cast<std::chrono::milliseconds>(Clock::now() - begin)
                    .count());
            uint64_t batchBytes = 0;
            for (const auto& r : records) {
                if (verify) {
                    int64_t id = r.row.GetInt64(0);
                    int lane = id % LANES;
                    uint64_t seq = id / LANES;
                    if (id < 0 || seq >= seen[lane].size() ||
                        r.row.GetString(2).size() != size_t(payload))
                        invalid++;
                    else if (seen[lane][seq])
                        duplicates++;
                    else
                        seen[lane][seq] = true;
                } else if (rows % 256 == 0)
                    age.add(wall() - r.row.GetInt64(1));
                rows++;
                batchBytes += payload;
            }
            bytes += batchBytes;
            if (!verify) {
                tick = std::max(tick, Clock::now()) +
                       std::chrono::microseconds(int64_t(batchBytes * 1000000 / rate));
                std::this_thread::sleep_until(tick);
                if (Clock::now() - lastSample >= std::chrono::seconds(1)) {
                    sample();
                    lastSample = Clock::now();
                }
            }
        }
        if (!verify) {
            sample();
            return 0;
        }
        uint64_t missing = 0;
        for (const auto& lane : seen) missing += std::count(lane.begin(), lane.end(), false);
        std::unordered_map<int32_t, int64_t> ends;
        ck("end offsets", admin.ListOffsets(path, ids, fluss::OffsetSpec::Latest(), ends));
        int64_t total = 0;
        for (const auto& end : ends) total += end.second;
        bool success = ends.size() == ids.size() && total == int64_t(expected) &&
                       rows == expected && !invalid && !duplicates && !missing;
        std::cout << "VERIFY {\"expected\":" << expected << ",\"read\":" << rows
                  << ",\"endOffsets\":" << total << ",\"missing\":" << missing
                  << ",\"duplicates\":" << duplicates << ",\"invalid\":" << invalid
                  << ",\"success\":" << (success ? "true" : "false") << "}" << std::endl;
        return success ? 0 : 2;
    }
    throw std::runtime_error("unknown mode");
}

int main(int argc, char** argv) {
    try {
        if (argc != 5) throw std::runtime_error("mode table run-dir process-id");
        fluss::FileLoggingOptions options;
        options.directory = env("OOO_LOG_DIR");
        options.filter = "warn,fluss::client::write::idempotence=debug";
        options.queue_capacity = 65536;
        options.max_files = 8;
        fluss::Logger logger;
        ck("init logging", fluss::Logger::InitFile(options, logger));
        int result;
        try {
            result = execute(argv[1], argv[2], argv[3], std::stoi(argv[4]));
        } catch (const std::exception& e) {
            std::cerr << "FAIL " << e.what() << std::endl;
            result = 1;
        }
        ck("shutdown logging", logger.Shutdown(10000));
        auto s = logger.Stats();
        std::cout << "LOG_STATS {\"accepted\":" << s.accepted << ",\"written\":" << s.written
                  << ",\"dropped\":" << s.dropped << ",\"truncated\":" << s.truncated
                  << ",\"write_errors\":" << s.write_errors << "}" << std::endl;
        return result;
    } catch (const std::exception& e) {
        std::cerr << "FAIL " << e.what() << std::endl;
        return 1;
    }
}
