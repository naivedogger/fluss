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

#include "ffi_converter.hpp"
#include "fluss.hpp"
#include "lib.rs.h"

namespace fluss {

Logger::~Logger() noexcept { ffi::delete_log_handle(handle_); }
Logger::Logger(Logger&& other) noexcept : handle_(other.handle_) { other.handle_ = nullptr; }
Logger& Logger::operator=(Logger&& other) noexcept {
    if (this != &other) {
        ffi::delete_log_handle(handle_);
        handle_ = other.handle_;
        other.handle_ = nullptr;
    }
    return *this;
}

Result Logger::InitFile(const FileLoggingOptions& options, Logger& out) {
    if (out.handle_) {
        return utils::make_client_error("Logger output handle is already initialized");
    }
    ffi::FfiLogOptions config;
    config.directory = options.directory;
    config.filter = options.filter;
    config.max_file_size = options.max_file_size;
    config.max_files = options.max_files;
    config.queue_capacity = options.queue_capacity;
    auto result = ffi::init_file_logging(config);
    if (result.result.error_code == 0) {
        out.handle_ = utils::ptr_from_ffi<ffi::LogHandle>(result);
    }
    return utils::from_ffi_result(result.result);
}

std::string Logger::Path() const {
    return handle_ ? std::string(ffi::logging_path(*handle_)) : std::string();
}
LoggingStats Logger::Stats() const {
    if (!handle_) {
        return {};
    }
    auto stats = ffi::logging_stats(*handle_);
    return {stats.accepted, stats.written, stats.dropped, stats.truncated, stats.write_errors};
}
Result Logger::Flush(uint64_t timeout_ms) const {
    return handle_ ? utils::from_ffi_result(ffi::logging_flush(*handle_, timeout_ms))
                   : utils::make_client_error("Logger is not initialized");
}
Result Logger::Shutdown(uint64_t timeout_ms) const {
    return handle_ ? utils::from_ffi_result(ffi::logging_shutdown(*handle_, timeout_ms))
                   : utils::make_client_error("Logger is not initialized");
}

}  // namespace fluss
