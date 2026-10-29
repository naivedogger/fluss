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

#include <cassert>

#include "upgrade_rate.hpp"

int main() {
    using namespace std::chrono;
    auto now = WritePacer::Clock::time_point{};
    std::atomic<int64_t> rate{20 * 1024 * 1000};
    WritePacer pacer(1024, 20);
    assert(pacer.acquire(now, rate));
    assert(!pacer.acquire(now, rate));
    assert(!pacer.acquire(now + microseconds(999), rate));
    now += milliseconds(1);
    assert(pacer.acquire(now, rate));

    rate = 0;
    assert(!pacer.acquire(now, rate));
    now += hours(1);
    assert(!pacer.acquire(now, rate));
    assert(pacer.wakeup(now) == now + milliseconds(10));

    rate = 20 * 1024 * 4000;
    for (int i = 0; i < 4; i++) assert(pacer.acquire(now, rate));
    assert(!pacer.acquire(now, rate));
    now += milliseconds(1);
    for (int i = 0; i < 4; i++) assert(pacer.acquire(now, rate));

    rate = 20 * 1024 * 100;
    assert(pacer.acquire(now, rate));
    assert(!pacer.acquire(now + milliseconds(9), rate));
    assert(pacer.acquire(now + milliseconds(10), rate));

    // A long backpressure stall does not accumulate a catch-up burst.
    now += hours(1);
    assert(pacer.acquire(now, rate));
    assert(!pacer.acquire(now, rate));

    rate = 1;
    assert(pacer.acquire(now, rate));
    assert(!pacer.acquire(now + seconds(1), rate));
    assert(pacer.wakeup(now) == now + milliseconds(10));
    rate = 20 * 1024 * 1000;
    assert(pacer.acquire(now, rate));
}
