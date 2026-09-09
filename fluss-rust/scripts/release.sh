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
# Retired entry point from the standalone fluss-rust repository.
# Keep a failing wrapper so old automation cannot silently create a separate release.

set -euo pipefail

cat >&2 <<'EOF'
Fluss clients now release with the main Fluss project, not as a separate source archive.
No archive was created or signed.

Follow website/community/how-to-release/creating-a-fluss-release.mdx in the
repository root. Client preparation is documented in
fluss-rust/website/docs/release/create-release.md.

Before creating the shared RC tag, run from the repository root:
  python3 fluss-rust/scripts/release_version.py check --tag v1.0.0-rc1

After preparing and committing ALL release versions, the release manager creates
the complete source archive using tools/releasing/create_source_release.sh,
invoked from the tools directory with RELEASE_VERSION set.
EOF
exit 1
