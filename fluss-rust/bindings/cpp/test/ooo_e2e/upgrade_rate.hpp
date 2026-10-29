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

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cstdint>

// One lane's pacing state. The aggregate rate is published by the process's
// control thread; no filesystem access or catch-up burst occurs in this path.
class WritePacer {
   public:
    using Clock = std::chrono::steady_clock;
    WritePacer(int payload, int lanes) : divisor_(double(payload) * lanes) {}

    bool acquire(const std::atomic<int64_t>& aggregateBps) {
        if (remaining_ > 0) {
            --remaining_;
            return true;
        }
        return acquire(Clock::now(), aggregateBps);
    }

    bool acquire(Clock::time_point now, const std::atomic<int64_t>& aggregateBps) {
        if (remaining_ == 0) {
            auto bps = aggregateBps.load(std::memory_order_relaxed);
            if (bps != lastBps_) {
                lastBps_ = bps;
                next_ = now;
            }
            if (bps == 0 || now < next_) return false;
            double laneRate = bps / divisor_;
            remaining_ = std::max(1, int(laneRate / 1000));
            next_ = now + std::chrono::microseconds(int64_t(remaining_ * 1000000 / laneRate));
        }
        --remaining_;
        return true;
    }

    Clock::time_point wakeup(Clock::time_point now) const {
        return lastBps_ == 0 ? now + std::chrono::milliseconds(10)
                             : std::min(next_, now + std::chrono::milliseconds(10));
    }

   private:
    double divisor_;
    int64_t lastBps_ = -1;
    int remaining_ = 0;
    Clock::time_point next_{};
};
