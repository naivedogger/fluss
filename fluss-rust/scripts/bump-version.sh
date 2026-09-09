#!/usr/bin/env bash
# Licensed to the Apache Software Foundation (ASF) under one or more
# contributor license agreements.  See the NOTICE file distributed with
# this work for additional information regarding copyright ownership.
# The ASF licenses this file to you under the Apache License, Version 2.0
# (the "License"); you may not use this file except in compliance with
# the License.  You may obtain a copy of the License at
#
#   http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#
# Update the Rust workspace, its self-dependency and the Elixir application version.
# Requires Python 3.11+. Can be invoked from any working directory.
#
# Usage: ./scripts/bump-version.sh <current_version> <next_version>
#   e.g. ./scripts/bump-version.sh 1.0.0 1.1.0-SNAPSHOT
#   Or with env vars: ./scripts/bump-version.sh $RELEASE_VERSION $NEXT_VERSION

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "${PYTHON:-python3}" "${SCRIPT_DIR}/release_version.py" bump "$@"
