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

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdio>
#include <exception>
#include <limits>
#include <mutex>

#include "fluss.hpp"
#include "rust/cxx.h"

namespace fluss {
namespace ffi {

/// Per-writer admission control; independent of Rust buffer memory and ACK completion.
/// Acquire, Cancel and AwaitAll require the writer's existing external serialization.
/// Release is called by the single SDK callback worker, after capture destruction.
class WriteCallbackCapacity {
   public:
    /// `wait_timeout_ms` is the connection's client.writer.buffer.wait-timeout, used as
    /// the shared budget for capacity and buffer waits. UINT64_MAX means block until a slot frees.
    WriteCallbackCapacity(size_t max_pending_operations, uint64_t wait_timeout_ms)
        : max_pending_(max_pending_operations), wait_timeout_ms_(wait_timeout_ms) {}

    Result Acquire() {
        // Only the submitter consumes capacity. A stale completion count therefore
        // underestimates free slots, never admits more than the configured limit.
        if (issued_ - observed_completed_ == max_pending_ && !HasSlot()) {
            // Fail fast from within a callback to avoid stalling the shared workers on
            // their own capacity; a zero budget also rejects immediately.
            if (in_callback_ || wait_timeout_ms_ == 0) {
                return {ErrorCode::CLIENT_ERROR, "Write callback capacity is full"};
            }
            if (!Wait([&] { return HasSlot(); }, false)) {
                return {ErrorCode::CLIENT_ERROR, "Timed out waiting for write callback capacity"};
            }
        }
        ++issued_;
        return {};
    }

    /// Return a reservation that was never transferred to an accepted callback.
    /// This stays on the serialized submission side, not the completion counter.
    void Cancel() noexcept { --issued_; }

    void Release() noexcept {
        // One completion writer: no shared read-modify-write on the hot path.
        // This is a count, NOT the sequence number of a contiguous completed prefix.
        completed_.store(++worker_completed_, std::memory_order_seq_cst);
        if (waiting_.load(std::memory_order_seq_cst)) {
            // Pair with Wait's registration/recheck under this mutex. Publication
            // itself never needs the mutex; only an actual waiter needs a wakeup.
            std::lock_guard<std::mutex> lock(wait_mutex_);
            available_.notify_all();
        }
    }

    /// True only while this thread executes a user callback or destroys its captures.
    static bool InCallback() { return in_callback_; }

    /// Wait until every reserved callback and its captures have finished.
    /// Flush rejects callback reentry before starting any write flush.
    void AwaitAll() {
        Wait(
            [&] {
                observed_completed_ = completed_.load(std::memory_order_seq_cst);
                return issued_ == observed_completed_;
            },
            true);
    }

    /// Milliseconds left in the client.writer.buffer.wait-timeout budget since `start`, so
    /// the buffer-backpressure wait plus the capacity reservation stay within one timeout
    /// (Kafka max.block.ms style). Floored at 0 (0 = fail fast). Returns -1 when the timeout
    /// is unbounded, letting the buffer wait fall back to the writer's configured timeout.
    int64_t RemainingBudgetMs(std::chrono::steady_clock::time_point start) const {
        if (IsUnbounded()) {
            return -1;
        }
        auto elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - start);
        int64_t remaining = static_cast<int64_t>(wait_timeout_ms_) - elapsed.count();
        return remaining > 0 ? remaining : 0;
    }

   private:
    friend class WriteCallback;
    inline static thread_local bool in_callback_ = false;
    bool IsUnbounded() const { return wait_timeout_ms_ == std::numeric_limits<uint64_t>::max(); }
    bool HasSlot() {
        observed_completed_ = completed_.load(std::memory_order_seq_cst);
        return issued_ - observed_completed_ < max_pending_;
    }

    template <typename Predicate>
    bool Wait(Predicate ready, bool unbounded) {
        std::unique_lock<std::mutex> lock(wait_mutex_);
        // The SC store/load pairs here and in Release forbid both threads from
        // missing each other: either this recheck sees completion or Release
        // observes a registered waiter and takes the mutex before notifying.
        waiting_.store(true, std::memory_order_seq_cst);
        struct Registration {
            std::atomic<bool>& waiting;
            ~Registration() { waiting.store(false, std::memory_order_seq_cst); }
        } registration{waiting_};
        if (unbounded || IsUnbounded()) {
            available_.wait(lock, ready);
            return true;
        }
        return available_.wait_for(lock, std::chrono::milliseconds(wait_timeout_ms_), ready);
    }

    const size_t max_pending_;
    const uint64_t wait_timeout_ms_;
    // Unsigned differences allow wraparound; outstanding reservations remain bounded.
    // Separate submit-side writes from completion publication/cache lines.
    alignas(128) size_t issued_ = 0;
    size_t observed_completed_ = 0;
    alignas(128) size_t worker_completed_ = 0;
    std::atomic<size_t> completed_{0};
    alignas(128) std::atomic<bool> waiting_{false};
    std::mutex wait_mutex_;
    std::condition_variable available_;
};

/// Owns a callback transferred to Rust. Access is exclusive, never concurrent.
class WriteCallback {
   public:
    explicit WriteCallback(fluss::WriteCallback callback) : callback_(std::move(callback)) {}

    WriteCallback(const WriteCallback&) = delete;
    WriteCallback& operator=(const WriteCallback&) = delete;

    /// Reserve before entering Rust. Destruction also returns capacity on submission failure.
    Result Reserve(std::shared_ptr<WriteCallbackCapacity> capacity) {
        if (!capacity) {
            return {ErrorCode::CLIENT_ERROR, "Writer not available"};
        }
        auto result = capacity->Acquire();
        if (result.Ok()) {
            reservation_.capacity = std::move(capacity);
        }
        return result;
    }

    /// Invoke once, containing all C++ exceptions on this side of the FFI boundary.
    void Complete(int32_t error_code, rust::Str error_message) noexcept {
        // Release captures before the reservation, even if this wrapper outlives Complete().
        CompletedReservation reservation{std::move(reservation_.capacity)};
        CallbackScope scope;
        // Moving std::function alone need not empty the source. Swap with an
        // empty function so captures are released even if the callback throws.
        fluss::WriteCallback callback;
        callback.swap(callback_);
        Result result;
        result.error_code = error_code;
        try {
            result.error_message = std::string(error_message);
        } catch (...) {
            // Error text is best-effort; allocation failure must not skip completion.
            std::fprintf(stderr, "Fluss write callback could not copy error text (code %d)\n",
                         error_code);
        }
        try {
            callback(WriteCompletion{std::move(result)});
        } catch (const std::exception& e) {
            std::fprintf(stderr, "Fluss write callback threw an exception: %s\n", e.what());
        } catch (...) {
            std::fprintf(stderr, "Fluss write callback threw an unknown exception\n");
        }
    }

   private:
    struct Reservation {
        std::shared_ptr<WriteCallbackCapacity> capacity;
        ~Reservation() {
            if (capacity) {
                capacity->Cancel();
            }
        }
    };

    struct CompletedReservation {
        std::shared_ptr<WriteCallbackCapacity> capacity;
        ~CompletedReservation() {
            if (capacity) {
                capacity->Release();
            }
        }
    };

    struct CallbackScope {
        bool previous = std::exchange(WriteCallbackCapacity::in_callback_, true);
        ~CallbackScope() { WriteCallbackCapacity::in_callback_ = previous; }
    };

    // Member order keeps captures alive until invocation, but not past capacity release.
    Reservation reservation_;
    fluss::WriteCallback callback_;
};

}  // namespace ffi
}  // namespace fluss
