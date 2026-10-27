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

#include <gtest/gtest.h>

#include <chrono>
#include <filesystem>
#include <utility>

#include "fluss.hpp"

TEST(FileLoggingTest, Lifecycle) {
    fluss::Logger empty;
    EXPECT_FALSE(empty.Flush(1).Ok());
    EXPECT_FALSE(empty.Shutdown(1).Ok());

    auto nonce = std::chrono::steady_clock::now().time_since_epoch().count();
    auto directory = std::filesystem::temp_directory_path() /
                     ("fluss-cpp-logging-test-" + std::to_string(nonce));
    fluss::FileLoggingOptions options;
    options.directory = directory.string();
    options.filter = "warn,fluss::client::write::idempotence=debug";
    options.max_file_size = 8192;
    options.max_files = 2;
    fluss::Logger logger;
    auto result = fluss::Logger::InitFile(options, logger);
    if (!result.Ok() && result.error_message == "enable the file-logging Cargo feature") {
        GTEST_SKIP() << result.error_message;
    }
    ASSERT_TRUE(result.Ok()) << result.error_message;
    auto path = logger.Path();
    EXPECT_TRUE(std::filesystem::exists(path));
    EXPECT_FALSE(fluss::Logger::InitFile(options, empty).Ok());
    EXPECT_FALSE(fluss::Logger::InitFile(options, logger).Ok());

    fluss::Logger moved(std::move(logger));
    EXPECT_TRUE(logger.Path().empty());
    EXPECT_EQ(moved.Path(), path);
    ASSERT_TRUE(moved.Flush(1000).Ok());
    ASSERT_TRUE(moved.Shutdown(1000).Ok());
    EXPECT_TRUE(moved.Shutdown(0).Ok());
    EXPECT_FALSE(moved.Flush(1).Ok());
    EXPECT_EQ(moved.Stats().dropped, 0);
    EXPECT_EQ(moved.Stats().write_errors, 0);
    EXPECT_FALSE(fluss::Logger::InitFile(options, empty).Ok());
    std::filesystem::remove_all(directory);
}
