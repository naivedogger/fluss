// ISR retry-storm writer built on the C++ SDK, contrasting the two append
// functions of fluss::AppendWriter:
//   --mode wait      Append(row, WriteResult&) + a background Wait() queue.
//                    This is the customer's current model (see the reference
//                    FlussAckTracker/FlussWriteCore). No admission backpressure
//                    beyond buffer memory, so in-flight can grow unbounded.
//   --mode callback  Append(row, WriteCallback) on a writer created with
//                    WriteCallbackOptions. Submission blocks on callback
//                    capacity + buffer, i.e. the SDK's native backpressure.
//                    No manual semaphore. Latency is submit->ack incl. the
//                    backpressure wait, observed inside the callback.
//   --mode forget    Append(row) fire-and-forget. Used for the A stressor.
//
// One process = one role. Orchestrate the A/B storm with the run script:
// start the observed B writers in one mode, fire the A stressor, watch B.
//
// Telemetry: one line per second to stdout, plus a final SUMMARY line, in the
// same shape as the Rust example so the existing analysis scripts still parse.

#include "fluss.hpp"

#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <csignal>
#include <cstdint>
#include <deque>
#include <functional>
#include <iostream>
#include <map>
#include <memory>
#include <mutex>
#include <condition_variable>
#include <cstdlib>
#include <random>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

using Clock = std::chrono::steady_clock;
using namespace std::chrono_literals;

static std::atomic<bool> g_stop{false};
static void onSignal(int) { g_stop.store(true); }

// ---------------------------------------------------------------------------
// Arg parsing: --key value (and --flag for booleans that default false).
// ---------------------------------------------------------------------------
struct Args {
    std::map<std::string, std::string> m;
    void parse(int argc, char** argv) {
        for (int i = 1; i < argc; ++i) {
            std::string a = argv[i];
            if (a.rfind("--", 0) != 0) continue;
            std::string key = a.substr(2);
            if (i + 1 < argc && std::string(argv[i + 1]).rfind("--", 0) != 0) {
                m[key] = argv[++i];
            } else {
                m[key] = "true";
            }
        }
    }
    std::string s(const std::string& k, const std::string& d = "") const {
        auto it = m.find(k);
        return it == m.end() ? d : it->second;
    }
    uint64_t n(const std::string& k, uint64_t d) const {
        auto v = s(k);
        return v.empty() ? d : std::stoull(v);
    }
    bool b(const std::string& k, bool d) const {
        auto v = s(k);
        return v.empty() ? d : (v == "true" || v == "1");
    }
};

// ---------------------------------------------------------------------------
// Latency + throughput accounting. Clock starts before Append so the callback
// path includes backpressure wait, matching what the upper layer perceives.
// ---------------------------------------------------------------------------
static const std::array<uint64_t, 16> kBoundsMs = {
    1, 5, 10, 25, 50, 100, 250, 500, 1000, 2000, 5000, 10000, 20000, 30000, 60000, 120000};

struct Stats {
    std::atomic<uint64_t> enq{0};       // rows submitted (Append returned Ok)
    std::atomic<uint64_t> submitErr{0}; // Append returned non-Ok (no completion)
    std::atomic<uint64_t> ok{0};
    std::atomic<uint64_t> fail{0};
    std::atomic<uint64_t> over{0};       // completions >= threshold
    std::atomic<int64_t> inflt{0};       // submitted but not yet completed
    std::atomic<uint64_t> infltPeak{0};
    std::atomic<uint64_t> maxMs{0};
    std::array<std::atomic<uint64_t>, 17> hist{}; // 16 bounds + catch-all

    uint64_t thresholdMs{30000};

    void submitted() {
        enq.fetch_add(1, std::memory_order_relaxed);
        int64_t cur = inflt.fetch_add(1, std::memory_order_relaxed) + 1;
        uint64_t peak = infltPeak.load(std::memory_order_relaxed);
        while (static_cast<uint64_t>(cur) > peak &&
               !infltPeak.compare_exchange_weak(peak, static_cast<uint64_t>(cur))) {
        }
    }
    void completed(bool okResult, uint64_t elapsedMs) {
        inflt.fetch_sub(1, std::memory_order_relaxed);
        if (okResult) ok.fetch_add(1, std::memory_order_relaxed);
        else fail.fetch_add(1, std::memory_order_relaxed);
        if (elapsedMs >= thresholdMs) over.fetch_add(1, std::memory_order_relaxed);
        size_t bin = kBoundsMs.size();
        for (size_t i = 0; i < kBoundsMs.size(); ++i) {
            if (elapsedMs <= kBoundsMs[i]) { bin = i; break; }
        }
        hist[bin].fetch_add(1, std::memory_order_relaxed);
        uint64_t mx = maxMs.load(std::memory_order_relaxed);
        while (elapsedMs > mx && !maxMs.compare_exchange_weak(mx, elapsedMs)) {
        }
    }
    // Approximate quantile from the cumulative histogram; returns a bound in ms.
    uint64_t quantileMs(double q) const {
        uint64_t total = 0;
        for (size_t i = 0; i < hist.size(); ++i) total += hist[i].load(std::memory_order_relaxed);
        if (total == 0) return 0;
        uint64_t target = static_cast<uint64_t>(q * static_cast<double>(total));
        uint64_t cum = 0;
        for (size_t i = 0; i < hist.size(); ++i) {
            cum += hist[i].load(std::memory_order_relaxed);
            if (cum >= target) {
                return i < kBoundsMs.size() ? kBoundsMs[i] : (kBoundsMs.back() + 1);
            }
        }
        return kBoundsMs.back() + 1;
    }
};

// ---------------------------------------------------------------------------
// ErrorLog: thread-safe aggregation of failure reasons (Result code + message).
// Every submit/ack failure is recorded with a site tag and the raw Result text,
// deduplicated with a count. Dumped periodically by the reporter so the
// breakdown survives even when the callback arm is killed mid-Flush.
// ---------------------------------------------------------------------------
struct ErrorLog {
    std::mutex mu;
    std::map<std::string, uint64_t> counts; // "<site> code=<c> <msg>" -> count

    void record(const char* site, int32_t code, const std::string& msg) {
        std::ostringstream k;
        k << site << " code=" << code << " " << msg;
        std::string key = k.str();
        std::lock_guard<std::mutex> g(mu);
        if (counts.size() >= 128 && counts.find(key) == counts.end()) {
            counts["(capped) other distinct errors"]++;
            return;
        }
        counts[key]++;
    }
    void dump(const std::string& tag) {
        std::vector<std::pair<std::string, uint64_t>> v;
        {
            std::lock_guard<std::mutex> g(mu);
            v.assign(counts.begin(), counts.end());
        }
        if (v.empty()) {
            std::cout << tag << " none" << std::endl;
            return;
        }
        std::sort(v.begin(), v.end(),
                  [](const std::pair<std::string, uint64_t>& a,
                     const std::pair<std::string, uint64_t>& b) { return a.second > b.second; });
        for (const auto& e : v) {
            std::cout << tag << " count=" << e.second << " " << e.first << std::endl;
        }
    }
};

// ---------------------------------------------------------------------------
// Wait-mode queue: submit threads hand off WriteResult; wait workers drain it.
// Mirrors the customer's FlussAckTracker (queue + background Wait), so nothing
// throttles submission and in-flight can grow unbounded.
// ---------------------------------------------------------------------------
struct Pending {
    fluss::WriteResult result;
    Clock::time_point submittedAt;
};

class WaitQueue {
   public:
    void push(std::unique_ptr<Pending> p) {
        {
            std::lock_guard<std::mutex> lock(mu_);
            q_.push_back(std::move(p));
        }
        cv_.notify_one();
    }
    void close() {
        {
            std::lock_guard<std::mutex> lock(mu_);
            closing_ = true;
        }
        cv_.notify_all();
    }
    // Returns false when drained and closed.
    bool pop(std::unique_ptr<Pending>& out) {
        std::unique_lock<std::mutex> lock(mu_);
        cv_.wait(lock, [&] { return !q_.empty() || closing_; });
        if (q_.empty()) return false;
        out = std::move(q_.front());
        q_.pop_front();
        return true;
    }

   private:
    std::mutex mu_;
    std::condition_variable cv_;
    std::deque<std::unique_ptr<Pending>> q_;
    bool closing_{false};
};

// ---------------------------------------------------------------------------
static fluss::Configuration buildConfig(const Args& a) {
    fluss::Configuration c;
    c.bootstrap_servers = a.s("bootstrap", "127.0.0.1:9123");
    c.security_protocol = a.s("security-protocol", "PLAINTEXT");
    c.security_sasl_mechanism = a.s("sasl-mechanism", "PLAIN");
    c.security_sasl_username = a.s("sasl-username");
    c.security_sasl_password = a.s("sasl-password");
    c.writer_acks = a.s("acks", "all");
    c.writer_enable_idempotence = a.b("idempotence", true);
    c.writer_buffer_memory_size = a.n("buffer-memory", c.writer_buffer_memory_size);
    c.writer_buffer_wait_timeout_ms = a.n("buffer-wait-timeout-ms", c.writer_buffer_wait_timeout_ms);
    c.writer_retry_backoff_ms = a.n("retry-backoff-ms", c.writer_retry_backoff_ms);
    c.writer_retry_max_backoff_ms = a.n("retry-max-backoff-ms", c.writer_retry_max_backoff_ms);
    c.writer_batch_size = static_cast<int32_t>(a.n("batch-size", c.writer_batch_size));
    c.writer_batch_timeout_ms = static_cast<int64_t>(a.n("batch-timeout-ms", c.writer_batch_timeout_ms));
    c.writer_max_inflight_requests_per_bucket =
        a.n("max-inflight-per-bucket", c.writer_max_inflight_requests_per_bucket);
    c.connect_timeout_ms = a.n("connect-timeout-ms", c.connect_timeout_ms);
    return c;
}

static void die(const char* step, const fluss::Result& r) {
    if (!r.Ok()) {
        std::cerr << step << " failed: code=" << r.error_code << " " << r.error_message << "\n";
        std::exit(1);
    }
}

// Ensure the observed table exists as a nonpartitioned, unkeyed log table.
static void ensureTable(fluss::Admin& admin, const std::string& db, const std::string& table,
                        int32_t buckets, int32_t rf, bool recreate) {
    die("CreateDatabase", admin.CreateDatabase(db, fluss::DatabaseDescriptor{}, true));
    if (recreate) {
        // Drop then recreate: the DDL kick that shrinks ISR for the stressor.
        die("DropTable", admin.DropTable(fluss::TablePath(db, table), true));
    }
    bool exists = false;
    die("TableExists", admin.TableExists(fluss::TablePath(db, table), exists));
    if (exists && !recreate) return;
    auto schema = fluss::Schema::NewBuilder()
                      .AddColumn("id", fluss::DataType::String())
                      .AddColumn("payload", fluss::DataType::String())
                      .Build();
    auto descriptor = fluss::TableDescriptor::NewBuilder()
                          .SetSchema(schema)
                          .SetBucketCount(buckets)
                          .SetProperty("table.replication.factor", std::to_string(rf))
                          .SetComment("ISR storm C++ writer table; not production data")
                          .Build();
    die("CreateTable", admin.CreateTable(fluss::TablePath(db, table), descriptor, true));
}

static fluss::GenericRow makeRow(const std::string& id, const std::string& payload) {
    fluss::GenericRow row(2);
    row.SetString(0, id);
    row.SetString(1, payload);
    return row;
}

int main(int argc, char** argv) {
    Args a;
    a.parse(argc, argv);
    std::signal(SIGINT, onSignal);
    std::signal(SIGTERM, onSignal);

    const std::string mode = a.s("mode", "callback");
    const std::string db = a.s("database", "fluss");
    const std::string table = a.s("table", "bench_b");
    const int32_t buckets = static_cast<int32_t>(a.n("buckets", 32));
    const int32_t rf = static_cast<int32_t>(a.n("replication-factor", 3));
    const size_t threads = a.n("concurrency", 5);
    const size_t waitWorkers = a.n("wait-workers", 4);
    const size_t payloadBytes = a.n("payload-bytes", 256);
    const double targetRate = static_cast<double>(a.n("target-rate", 0)); // rows/s for this proc
    const uint64_t runSeconds = a.n("run-seconds", 3600);
    const uint64_t thresholdMs = a.n("latency-threshold-ms", 30000);
    const size_t cbMaxPending = a.n("callback-max-pending", 262144);
    const uint64_t cbEnqueueTimeoutMs = a.n("callback-enqueue-timeout-ms", 30000);
    const std::string runId = a.s("run-id", "run");
    const std::string procId = a.s("process-id", "0");
    const bool doRecreate = a.b("recreate", false);

    fluss::Configuration cfg = buildConfig(a);
    fluss::Connection conn;
    die("Connection::Create", fluss::Connection::Create(cfg, conn));
    fluss::Admin admin;
    die("GetAdmin", conn.GetAdmin(admin));

    // --recreate performs the DDL kick and exits (used against the A table).
    if (doRecreate) {
        ensureTable(admin, db, table, buckets, rf, true);
        std::cout << "recreated " << db << "." << table << "\n";
        return 0;
    }
    ensureTable(admin, db, table, buckets, rf, false);

    fluss::Table tbl;
    die("GetTable", conn.GetTable(fluss::TablePath(db, table), tbl));

    Stats stats;
    ErrorLog errs;
    stats.thresholdMs = thresholdMs;

    const double rowsPerThread = targetRate > 0 ? targetRate / static_cast<double>(threads) : 0.0;

    // First line: parameters, so the analyzer can identify the arm.
    std::cout << "mode=" << mode << " table=" << db << "." << table << " buckets=" << buckets
              << " rf=" << rf << " threads=" << threads << " target_rate=" << targetRate
              << " acks=" << cfg.writer_acks << " idempotence=" << cfg.writer_enable_idempotence
              << " retry_backoff_ms=" << cfg.writer_retry_backoff_ms
              << " retry_max_backoff_ms=" << cfg.writer_retry_max_backoff_ms
              << " buffer_memory=" << cfg.writer_buffer_memory_size
              << " buffer_wait_timeout_ms=" << cfg.writer_buffer_wait_timeout_ms;
    if (mode == "callback")
        std::cout << " callback_max_pending=" << cbMaxPending
                  << " callback_enqueue_timeout_ms=" << cbEnqueueTimeoutMs;
    std::cout << " latency_threshold_ms=" << thresholdMs << std::endl;

    const auto start = Clock::now();
    const auto deadline = start + std::chrono::seconds(runSeconds);

    WaitQueue waitQueue;
    std::vector<std::thread> waiters;
    if (mode == "wait") {
        for (size_t w = 0; w < waitWorkers; ++w) {
            waiters.emplace_back([&] {
                std::unique_ptr<Pending> p;
                while (waitQueue.pop(p)) {
                    auto r = p->result.Wait();
                    uint64_t ms = std::chrono::duration_cast<std::chrono::milliseconds>(
                                      Clock::now() - p->submittedAt)
                                      .count();
                    if (!r.Ok()) errs.record("ack-wait", r.error_code, r.error_message);
                    stats.completed(r.Ok(), ms);
                    p.reset();
                }
            });
        }
    }

    // Reporter: one line per second.
    std::atomic<bool> done{false};
    std::thread reporter([&] {
        uint64_t lastEnq = 0, lastDone = 0, sec = 0;
        while (!done.load()) {
            std::this_thread::sleep_for(1s);
            ++sec;
            uint64_t enq = stats.enq.load();
            uint64_t doneCount = stats.ok.load() + stats.fail.load();
            uint64_t over = stats.over.load();
            double pct = doneCount ? 100.0 * static_cast<double>(over) / static_cast<double>(doneCount) : 0.0;
            std::ostringstream line;
            line << "t=" << sec << "s"
                 << " enq/s=" << (enq - lastEnq) << " done/s=" << (doneCount - lastDone)
                 << " inflt=" << stats.inflt.load() << " >" << thresholdMs << "ms=" << over << "("
                 << pct << "%)"
                 << " p50~" << stats.quantileMs(0.50) << "ms"
                 << " p99~" << stats.quantileMs(0.99) << "ms"
                 << " max=" << stats.maxMs.load() << "ms"
                 << " enq_total=" << enq << " ok=" << stats.ok.load()
                 << " fail=" << stats.fail.load() << " submit_err=" << stats.submitErr.load();
            std::cout << line.str() << std::endl;
            if (sec % 30 == 0) errs.dump("ERRLOG t=" + std::to_string(sec) + "s");
            lastEnq = enq;
            lastDone = doneCount;
        }
    });

    // Writer workers: one AppendWriter per thread (callback writers are not
    // safe for concurrent Append, so never share one across threads).
    std::vector<std::thread> workers;
    for (size_t tid = 0; tid < threads; ++tid) {
        workers.emplace_back([&, tid] {
            fluss::AppendWriter writer;
            if (mode == "callback") {
                fluss::WriteCallbackOptions opts;
                opts.max_pending_operations = cbMaxPending;
                opts.enqueue_timeout = std::chrono::milliseconds(cbEnqueueTimeoutMs);
                die("CreateWriter(callback)", tbl.NewAppend().CreateWriter(writer, opts));
            } else {
                die("CreateWriter", tbl.NewAppend().CreateWriter(writer));
            }
            uint64_t seq = 0;
            auto next = start;
            // Per-thread high-entropy payload buffer, refilled every row so batch
            // compression cannot collapse identical rows and the on-wire size
            // stays close to payloadBytes. 64-char alphabet -> 6 bits/char, so
            // one 64-bit draw yields 10 chars (cheap enough at these rates).
            static const char kAlphabet[] =
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            std::mt19937_64 rng(
                static_cast<uint64_t>(Clock::now().time_since_epoch().count())
                ^ (static_cast<uint64_t>(std::hash<std::string>{}(procId)) << 17)
                ^ (static_cast<uint64_t>(tid + 1) * 0x9E3779B97F4A7C15ULL));
            std::string payload(payloadBytes, 'x');
            while (!g_stop.load() && Clock::now() < deadline) {
                if (rowsPerThread > 0) {
                    while (!g_stop.load() && Clock::now() < next && Clock::now() < deadline) {
                        auto wait = std::min<Clock::duration>(
                            5ms, std::chrono::duration_cast<Clock::duration>(next - Clock::now()));
                        std::this_thread::sleep_for(wait);
                    }
                    if (g_stop.load() || Clock::now() >= deadline) break;
                }
                for (size_t k = 0; k < payloadBytes;) {
                    uint64_t r = rng();
                    for (int b = 0; b < 10 && k < payloadBytes; ++b) {
                        payload[k++] = kAlphabet[r & 63];
                        r >>= 6;
                    }
                }
                std::string id = runId + "_" + procId + "_" + std::to_string(tid) + "_" + std::to_string(seq++);
                auto row = makeRow(id, payload);

                if (mode == "callback") {
                    auto submitAt = Clock::now();
                    fluss::WriteCallback cb = [&stats, &errs, submitAt](fluss::Result r) {
                        uint64_t ms = std::chrono::duration_cast<std::chrono::milliseconds>(
                                          Clock::now() - submitAt)
                                          .count();
                        if (!r.Ok()) errs.record("ack-cb", r.error_code, r.error_message);
                        stats.completed(r.Ok(), ms);
                    };
                    // Count in-flight before submit; the clock already started.
                    stats.submitted();
                    auto submitted = writer.Append(row, std::move(cb));
                    if (!submitted.Ok()) {
                        // No callback runs on submission failure: undo the counters.
                        stats.inflt.fetch_sub(1, std::memory_order_relaxed);
                        stats.enq.fetch_sub(1, std::memory_order_relaxed);
                        stats.submitErr.fetch_add(1, std::memory_order_relaxed);
                        errs.record("submit-cb", submitted.error_code, submitted.error_message);
                    }
                } else if (mode == "wait") {
                    auto pending = std::make_unique<Pending>();
                    pending->submittedAt = Clock::now();
                    auto submitted = writer.Append(row, pending->result);
                    if (!submitted.Ok()) {
                        stats.submitErr.fetch_add(1, std::memory_order_relaxed);
                        errs.record("submit-wait", submitted.error_code, submitted.error_message);
                    } else {
                        stats.submitted();
                        waitQueue.push(std::move(pending));
                    }
                } else { // forget
                    auto submitted = writer.Append(row);
                    if (submitted.Ok()) {
                        stats.enq.fetch_add(1, std::memory_order_relaxed);
                    } else {
                        stats.submitErr.fetch_add(1, std::memory_order_relaxed);
                        errs.record("submit-forget", submitted.error_code, submitted.error_message);
                    }
                }

                if (rowsPerThread > 0) {
                    next = std::max(Clock::now(),
                                    next + std::chrono::duration_cast<Clock::duration>(
                                               std::chrono::duration<double>(1.0 / rowsPerThread)));
                }
            }
            // Flush drains buffered writes and, for callback writers, pending callbacks.
            writer.Flush();
        });
    }

    for (auto& w : workers) w.join();
    if (mode == "wait") {
        waitQueue.close();
        for (auto& w : waiters) w.join();
    }
    done.store(true);
    reporter.join();

    uint64_t doneCount = stats.ok.load() + stats.fail.load();
    double pct = doneCount ? 100.0 * static_cast<double>(stats.over.load()) / static_cast<double>(doneCount) : 0.0;
    std::cout << "SUMMARY mode=" << mode << " completed=" << doneCount << " ok=" << stats.ok.load()
              << " fail=" << stats.fail.load() << " submit_err=" << stats.submitErr.load()
              << " over_" << thresholdMs << "ms=" << stats.over.load() << "(" << pct << "%)"
              << " peak_inflt=" << stats.infltPeak.load() << " enq_total=" << stats.enq.load()
              << " p50~" << stats.quantileMs(0.50) << "ms p99~" << stats.quantileMs(0.99)
              << "ms max=" << stats.maxMs.load() << "ms" << std::endl;
    errs.dump("ERRLOG final");
    return 0;
}
