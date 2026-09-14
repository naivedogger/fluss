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

#include <cstdio>
#include <exception>

#include "fluss.hpp"
#include "rust/cxx.h"

namespace fluss {
namespace ffi {

/// Owns a callback transferred to Rust. Access is exclusive, never concurrent.
class WriteCallback {
   public:
    explicit WriteCallback(fluss::WriteCallback callback) : callback_(std::move(callback)) {}

    /// Invoke once, containing all C++ exceptions on this side of the FFI boundary.
    void Complete(int32_t error_code, rust::Str error_message) noexcept {
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
            callback(std::move(result));
        } catch (const std::exception& e) {
            std::fprintf(stderr, "Fluss write callback threw an exception: %s\n", e.what());
        } catch (...) {
            std::fprintf(stderr, "Fluss write callback threw an unknown exception\n");
        }
    }

   private:
    fluss::WriteCallback callback_;
};

}  // namespace ffi
}  // namespace fluss
