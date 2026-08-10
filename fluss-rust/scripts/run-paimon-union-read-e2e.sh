#!/usr/bin/env bash

# Licensed to the Apache Software Foundation (ASF) under one
# or more contributor license agreements.  See the NOTICE file
# distributed with this work for additional information
# regarding copyright ownership.  The ASF licenses this file
# to you under the Apache License, Version 2.0 (the
# "License"); you may not use this file except in compliance
# with the License.  You may obtain a copy of the License at
#
#   http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing,
# software distributed under the License is distributed on an
# "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
# KIND, either express or implied.  See the License for the
# specific language governing permissions and limitations
# under the License.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPOSITORY_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

cd "${REPOSITORY_ROOT}"

# Install the Java/Flink tiering test runtime and its reactor dependencies.
# The Rust E2E invokes the helper Maven test in offline mode while its Docker
# cluster is alive, so dependency resolution cannot unexpectedly interrupt the
# cross-language hand-off.
./mvnw \
    --no-transfer-progress \
    -pl fluss-lake/fluss-lake-paimon \
    -am \
    -DskipTests \
    clean install

cd "${REPOSITORY_ROOT}/fluss-rust"

cargo test \
    -p fluss-lake \
    --features integration_tests,paimon \
    --test test_union_read \
    -- \
    --nocapture \
    --test-threads=1
