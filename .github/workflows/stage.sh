#!/usr/bin/env bash
################################################################################
# Licensed to the Apache Software Foundation (ASF) under one
# or more contributor license agreements.  See the NOTICE file
# distributed with this work for additional information
# regarding copyright ownership.  The ASF licenses this file
# to you under the Apache License, Version 2.0 (the
# "License"); you may not use this file except in compliance
# with the License.  You may obtain a copy of the License at
#
#      http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
################################################################################

STAGE_CORE="core"
STAGE_FLINK="flink"

MODULES_FLINK="\
fluss-flink,\
fluss-flink/fluss-flink-common,\
fluss-flink/fluss-flink-1.20,\
fluss-flink/fluss-flink-1.19,\
fluss-flink/fluss-flink-1.18,\
fluss-lake,\
fluss-lake/fluss-lake-paimon,\
fluss-lake/fluss-lake-iceberg,\
fluss-lake/fluss-lake-lance
"

function get_test_modules_for_stage() {
    local stage=$1

    local modules_flink=$MODULES_FLINK
    local modules_core=\!${MODULES_FLINK//,/,\!}

    case ${stage} in
        (${STAGE_CORE})
            echo "-pl $modules_core"
        ;;
        (${STAGE_FLINK})
            echo "-pl fluss-test-coverage,$modules_flink"
        ;;
    esac
}

get_test_modules_for_stage $1
