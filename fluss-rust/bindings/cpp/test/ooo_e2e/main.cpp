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
#include <cmath>
#include <cstdlib>
#include <fstream>
#include <iostream>
#include <memory>
#include <numeric>
#include <thread>
#include <vector>

#include "fluss.hpp"
using Clock = std::chrono::steady_clock;
constexpr int STRIDE = 100000000, POOL = 1024, SAMPLE = 256, BINS = 60002;
void ck(const char* s, const fluss::Result& r) {
    if (!r.Ok()) throw std::runtime_error(std::string(s) + ": " + r.error_message);
}
long long wall() {
    return std::chrono::duration_cast<std::chrono::milliseconds>(
               std::chrono::system_clock::now().time_since_epoch())
        .count();
}
struct Stats {
    alignas(64) std::atomic<uint64_t> accepted{0};
    alignas(64) std::atomic<uint64_t> completed{0}, failed{0}, samples{0};
    std::array<std::atomic<uint64_t>, BINS> hist{};
};
uint64_t sum(const std::vector<std::shared_ptr<Stats>>& ss, int which) {
    uint64_t x = 0;
    for (auto& s : ss)
        x += which == 0 ? s->accepted.load() : which == 1 ? s->completed.load() : s->failed.load();
    return x;
}
void report(const char* phase, const std::vector<std::shared_ptr<Stats>>& ss, double seconds,
            uint64_t atCut, double cutoff) {
    std::array<uint64_t, BINS> hist{};
    uint64_t samples = 0;
    for (auto& s : ss) {
        samples += s->samples.load();
        for (int i = 0; i < BINS; i++) hist[i] += s->hist[i].load();
    }
    auto quantile = [&](double q) {
        uint64_t n = 0, target = std::max<uint64_t>(1, uint64_t(std::ceil(samples * q)));
        for (int i = 0; i < BINS; i++) {
            n += hist[i];
            if (n >= target) return i;
        }
        return BINS - 1;
    };
    std::cout << "RESULT {\"phase\":\"" << phase << "\",\"accepted\":" << sum(ss, 0)
              << ",\"completed\":" << sum(ss, 1) << ",\"errors\":" << sum(ss, 2)
              << ",\"secondsIncludingDrain\":" << seconds
              << ",\"ackRateIncludingDrain\":" << sum(ss, 1) / seconds
              << ",\"completedAtCutoff\":" << atCut << ",\"cutoffSeconds\":" << cutoff
              << ",\"samples\":" << samples << ",\"p50UpperMs\":" << quantile(.5)
              << ",\"p95UpperMs\":" << quantile(.95) << ",\"p99UpperMs\":" << quantile(.99) << "}"
              << std::endl;
}

void touch(const std::string& p) { std::ofstream(p) << "ready\n"; }
long long barrier(const std::string& p) {
    for (int i = 0; i < 24000; i++) {
        std::ifstream f(p);
        long long n;
        if (f >> n) return n;
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    throw std::runtime_error("barrier timeout " + p);
}
void saveStats(const std::string& p, const std::vector<std::shared_ptr<Stats>>& ss) {
    std::ofstream f(p);
    for (int i = 0; i < BINS; i++) {
        uint64_t n = 0;
        for (auto& s : ss) n += s->hist[i].load();
        if (n) f << i << " " << n << "\n";
    }
}
std::string env(const char* key, const char* fallback = "") {
    const char* value = std::getenv(key);
    return value ? value : fallback;
}
int number(const char* key, int fallback) {
    return std::stoi(env(key, std::to_string(fallback).c_str()));
}
int run(int argc, char** argv) {
    try {
        if (argc != 5) throw std::runtime_error("mode table control-dir process-id");
        std::string mode = argv[1], name = argv[2], dir = argv[3];
        int proc = std::stoi(argv[4]);
        constexpr int buckets = 32, threads = 5, bytes = 1024;
        fluss::Configuration cfg;
        cfg.bootstrap_servers = env("OOO_ENDPOINT");
        cfg.security_protocol = env("OOO_SECURITY_PROTOCOL");
        cfg.security_sasl_mechanism = env("OOO_SASL_MECHANISM");
        cfg.security_sasl_username = env("OOO_USERNAME");
        cfg.security_sasl_password = env("OOO_PASSWORD");
        cfg.writer_dynamic_batch_size_enabled = true;
        cfg.writer_dynamic_batch_size_min = 256 * 1024;
        cfg.writer_batch_size = 2 * 1024 * 1024;
        cfg.writer_batch_timeout_ms = 100;
        cfg.writer_buffer_memory_size = 256 * 1024 * 1024;
        cfg.writer_buffer_wait_timeout_ms = 10000;
        cfg.writer_max_inflight_requests_per_bucket = 5;
        cfg.writer_enable_idempotence = true;
        cfg.writer_acks = "all";
        cfg.scanner_log_max_poll_records = 20000;
        fluss::TablePath path(env("OOO_DATABASE"), name);
        std::vector<int32_t> bucketIds(buckets);
        std::iota(bucketIds.begin(), bucketIds.end(), 0);
        fluss::Connection c;
        ck("connection", fluss::Connection::Create(cfg, c));
        fluss::Admin admin;
        ck("admin", c.GetAdmin(admin));
        if (mode == "check") {
            bool exists = false;
            ck("database", admin.DatabaseExists(env("OOO_DATABASE"), exists));
            if (!exists)
                throw std::runtime_error(
                    "database does not exist; select an existing database with --database");
            std::vector<fluss::ServerNode> nodes;
            ck("get server nodes", admin.GetServerNodes(nodes));
            std::cout << "CHECK nodes=" << nodes.size() << std::endl;
            return 0;
        }
        if (mode == "setup") {
            auto d = fluss::TableDescriptor::NewBuilder()
                         .SetSchema(fluss::Schema::NewBuilder()
                                        .AddColumn("id", fluss::DataType::Int())
                                        .AddColumn("value", fluss::DataType::String())
                                        .Build())
                         .SetBucketCount(buckets)
                         .SetBucketKeys({"id"})
                         .SetProperty("table.replication.factor", env("OOO_RF"))
                         .Build();
            ck("create", admin.CreateTable(path, d, false));
            std::unordered_map<int32_t, int64_t> offsets;
            auto until = Clock::now() + std::chrono::seconds(180);
            bool ready = false;
            while (Clock::now() < until) {
                auto r = admin.ListOffsets(path, bucketIds, fluss::OffsetSpec::Latest(), offsets);
                if (r.Ok() && offsets.size() == bucketIds.size()) {
                    ready = true;
                    break;
                }
                std::this_thread::sleep_for(std::chrono::milliseconds(100));
            }
            if (!ready) throw std::runtime_error("readiness timeout");
            fluss::Table table;
            ck("table", c.GetTable(path, table));
            std::cout << "META {\"tableId\":" << table.GetTableInfo().table_id << "}" << std::endl;
            return 0;
        }

        fluss::Table table;
        ck("table", c.GetTable(path, table));

        if (mode == "verify") {
            std::vector<int> next(20);
            for (int p = 0; p < 4; p++) {
                std::ifstream f(dir + "/counts-" + std::to_string(p));
                for (int t = 0; t < 5; t++)
                    if (!(f >> next[p * 5 + t])) throw std::runtime_error("missing counts");
            }
            uint64_t expected = std::accumulate(next.begin(), next.end(), uint64_t{0});
            std::unordered_map<int32_t, int64_t> ends;
            ck("end offsets",
               admin.ListOffsets(path, bucketIds, fluss::OffsetSpec::Latest(), ends));
            int64_t total = 0;
            for (auto& it : ends) total += it.second;
            fluss::LogScanner scanner;
            ck("scanner", table.NewScan().CreateLogScanner(scanner));
            for (int b : bucketIds) ck("subscribe", scanner.Subscribe(b, 0));
            std::vector<std::vector<bool>> seen(20);
            for (int t = 0; t < 20; t++) seen[t].resize(next[t]);
            int64_t read = 0, duplicate = 0, invalid = 0;
            auto readDeadline =
                Clock::now() + std::chrono::seconds(number("OOO_VERIFY_SECONDS", 3600));
            auto begin = Clock::now();
            if (ends.size() != bucketIds.size()) throw std::runtime_error("incomplete end offsets");
            while (read < total && Clock::now() < readDeadline) {
                fluss::ScanRecords records;
                ck("poll", scanner.Poll(1000, records));
                for (const auto& r : records) {
                    int id = r.row.GetInt32(0), lane = id / STRIDE, seq = id % STRIDE;
                    if (id < 0 || lane >= 20 || seq >= next[lane])
                        ++invalid;
                    else if (seen[lane][seq])
                        ++duplicate;
                    else
                        seen[lane][seq] = true;
                    ++read;
                }
            }
            int64_t missing = 0;
            for (auto& lane : seen) missing += std::count(lane.begin(), lane.end(), false);
            bool ok =
                total == int64_t(expected) && read == total && !missing && !duplicate && !invalid;
            std::cout << "VERIFY {\"expected\":" << expected << ",\"endOffsets\":" << total
                      << ",\"read\":" << read << ",\"duplicate\":" << duplicate
                      << ",\"invalid\":" << invalid << ",\"missing\":" << missing << ",\"seconds\":"
                      << std::chrono::duration<double>(Clock::now() - begin).count()
                      << ",\"success\":" << (ok ? "true" : "false") << "}" << std::endl;
            return ok ? 0 : 2;
        }
        if (mode != "producer" || proc < 0 || proc >= 4)
            throw std::runtime_error("invalid mode/process");
        std::vector<fluss::AppendWriter> writers(threads);
        std::vector<std::vector<fluss::GenericRow>> pools(threads);
        std::vector<std::shared_ptr<Stats>> warm, measured;
        std::vector<int> next(threads);
        for (int t = 0; t < threads; t++) {
            ck("writer", table.NewAppend().CreateWriter(writers[t]));
            warm.push_back(std::make_shared<Stats>());
            measured.push_back(std::make_shared<Stats>());
            pools[t].resize(POOL);
            uint64_t seed = 17 + proc * 5 + t;
            for (auto& row : pools[t]) {
                std::string value(bytes, ' ');
                for (char& ch : value) {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    ch = '!' + seed % 90;
                }
                row.SetInt32(0, 0);
                row.SetString(1, value);
            }
        }
        touch(dir + "/ready-" + std::to_string(proc));
        std::atomic<bool> failed{false};
        for (int phase = 0; phase < 2; phase++) {
            std::string label = phase ? "measurement" : "warmup";
            auto& ss = phase ? measured : warm;
            int secs =
                phase ? number("OOO_MEASURE_SECONDS", 600) : number("OOO_WARMUP_SECONDS", 120);
            long long startWall = barrier(dir + "/" + label);
            auto start = Clock::now() + std::chrono::milliseconds(startWall - wall()),
                 stop = start + std::chrono::seconds(secs);
            std::vector<std::thread> workers;
            for (int t = 0; t < threads; t++)
                workers.emplace_back([&, t] {
                    auto s = ss[t];
                    std::this_thread::sleep_until(start);
                    auto tick = start;
                    int quota = 0;
                    const int rate = number("OOO_RATE", 400000);
                    try {
                        while (!failed && Clock::now() < stop) {
                            if (quota == 0) {
                                std::this_thread::sleep_until(tick);
                                if (Clock::now() >= stop) break;
                                quota = std::max(1, rate / 4000);
                                tick =
                                    std::max(tick, Clock::now()) +
                                    std::chrono::microseconds(int64_t(quota) * 20 * 1000000 / rate);
                            }
                            --quota;
                            int seq = next[t];
                            if (seq >= STRIDE) throw std::runtime_error("lane budget exceeded");
                            auto& row = pools[t][seq % POOL];
                            row.SetInt32(0, (proc * threads + t) * STRIDE + seq);
                            bool sample = seq % SAMPLE == 0;
                            auto before = sample ? Clock::now() : Clock::time_point{};
                            auto r = writers[t].Append(
                                row, [s, sample, before](const fluss::WriteCompletion& r) {
                                    if (sample) {
                                        auto us =
                                            std::chrono::duration_cast<std::chrono::microseconds>(
                                                Clock::now() - before)
                                                .count();
                                        size_t bin = std::min<int64_t>((us + 999) / 1000, BINS - 1);
                                        s->hist[bin].fetch_add(1, std::memory_order_relaxed);
                                        s->samples.fetch_add(1, std::memory_order_relaxed);
                                    }
                                    if (r.result.Ok())
                                        s->completed.fetch_add(1, std::memory_order_relaxed);
                                    else
                                        s->failed.fetch_add(1, std::memory_order_relaxed);
                                });
                            ck("append", r);
                            s->accepted.fetch_add(1, std::memory_order_relaxed);
                            ++next[t];
                        }
                        ck("flush", writers[t].Flush());
                    } catch (const std::exception& e) {
                        failed = true;
                        std::cerr << "worker " << t << ": " << e.what() << std::endl;
                    }
                });
            std::this_thread::sleep_until(start);
            std::cout << "PHASE " << label << " " << wall() << std::endl;
            while (Clock::now() < stop) {
                std::this_thread::sleep_until(
                    std::min(stop, Clock::now() + std::chrono::seconds(1)));
                std::cout << "TICK " << label << " " << wall() << " " << sum(ss, 0) << " "
                          << sum(ss, 1) << " " << sum(ss, 2) << std::endl;
            }
            uint64_t cutoff = sum(ss, 1);
            double cutoffSecs = std::chrono::duration<double>(Clock::now() - start).count();
            for (auto& t : workers) t.join();
            double elapsed = std::chrono::duration<double>(Clock::now() - start).count();
            report(label.c_str(), ss, elapsed, cutoff, cutoffSecs);
            saveStats(dir + "/" + label + "-hist-" + std::to_string(proc), ss);
            if (failed || sum(ss, 2) || sum(ss, 0) != sum(ss, 1))
                throw std::runtime_error("callback/flush failure");
            touch(dir + "/done-" + label + "-" + std::to_string(proc));
        }
        std::ofstream f(dir + "/counts-" + std::to_string(proc));
        for (int n : next) f << n << "\n";
        f.close();
        return 0;
    } catch (const std::exception& e) {
        std::cerr << "FAIL " << e.what() << std::endl;
        return 1;
    }
}

int main(int argc, char** argv) {
    try {
        fluss::FileLoggingOptions options;
        options.directory = env("OOO_LOG_DIR");
        if (options.directory.empty()) throw std::runtime_error("OOO_LOG_DIR is required");
        options.filter = "warn,fluss::client::write::idempotence=debug";
        options.queue_capacity = 8192;
        options.max_files = 8;
        fluss::Logger logger;
        ck("init logging", fluss::Logger::InitFile(options, logger));
        std::cout << "LOG_PATH " << logger.Path() << std::endl;
        // run owns all SDK connections; destroy them before draining the logger.
        int result = run(argc, argv);
        auto shutdown = logger.Shutdown(10000);
        auto stats = logger.Stats();
        std::cout << "LOG_STATS {\"accepted\":" << stats.accepted
                  << ",\"written\":" << stats.written << ",\"dropped\":" << stats.dropped
                  << ",\"truncated\":" << stats.truncated
                  << ",\"write_errors\":" << stats.write_errors << "}" << std::endl;
        ck("shutdown logging", shutdown);
        if (stats.dropped || stats.truncated || stats.write_errors ||
            stats.accepted != stats.written)
            return 3;
        return result;
    } catch (const std::exception& e) {
        std::cerr << "FAIL " << e.what() << std::endl;
        return 1;
    }
}
