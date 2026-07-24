/*
 * Licensed to the Apache Software Foundation (ASF) under one or more
 * contributor license agreements.  See the NOTICE file distributed with
 * this work for additional information regarding copyright ownership.
 * The ASF licenses this file to You under the Apache License, Version 2.0
 * (the "License"); you may not use this file except in compliance with
 * the License.  You may obtain a copy of the License at
 *
 *    http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package org.apache.fluss.flink.utils;

import org.apache.fluss.client.initializer.OffsetsInitializer;
import org.apache.fluss.config.Configuration;
import org.apache.fluss.flink.FlinkConnectorOptions.ScanBoundedMode;

import org.apache.flink.table.api.ValidationException;
import org.junit.jupiter.api.Test;

import java.time.ZoneId;
import java.util.TimeZone;

import static org.apache.flink.configuration.CoreOptions.TMP_DIRS;
import static org.apache.fluss.config.ConfigOptions.CLIENT_SCANNER_IO_TMP_DIR;
import static org.apache.fluss.flink.FlinkConnectorOptions.SCAN_BOUNDED_MODE;
import static org.apache.fluss.flink.FlinkConnectorOptions.SCAN_BOUNDED_TIMESTAMP;
import static org.apache.fluss.flink.FlinkConnectorOptions.SCAN_SPLIT_ASSIGNMENT_BATCH_SIZE;
import static org.apache.fluss.flink.FlinkConnectorOptions.SCAN_STARTUP_TIMESTAMP;
import static org.apache.fluss.flink.utils.FlinkConnectorOptionsUtils.parseTimestamp;
import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;

/** Test for {@link FlinkConnectorOptionsUtils}. */
class FlinkConnectorOptionsUtilTest {
    @Test
    void testParseTimestamp() {
        assertThat(
                        parseTimestamp(
                                "1702134552000",
                                SCAN_STARTUP_TIMESTAMP.key(),
                                ZoneId.systemDefault()))
                .isEqualTo(1702134552000L);

        assertThat(
                        parseTimestamp(
                                "2023-12-09 23:09:12",
                                SCAN_STARTUP_TIMESTAMP.key(),
                                TimeZone.getTimeZone("Asia/Shanghai").toZoneId()))
                .isEqualTo(1702134552000L);

        assertThatThrownBy(
                        () ->
                                parseTimestamp(
                                        "2023-12-09T23:09:12",
                                        SCAN_STARTUP_TIMESTAMP.key(),
                                        ZoneId.systemDefault()))
                .isInstanceOf(ValidationException.class)
                .hasMessage(
                        "Invalid properties 'scan.startup.timestamp' should follow the format 'yyyy-MM-dd HH:mm:ss' "
                                + "or 'timestamp', but is '2023-12-09T23:09:12'. "
                                + "You can config like: '2023-12-09 23:09:12' or '1678883047356'.");
    }

    @Test
    void testValidateSplitAssignmentBatchSize() {
        org.apache.flink.configuration.Configuration tableOptions =
                new org.apache.flink.configuration.Configuration();

        tableOptions.set(SCAN_SPLIT_ASSIGNMENT_BATCH_SIZE, 1);
        FlinkConnectorOptionsUtils.validateTableSourceOptions(tableOptions);

        tableOptions.set(SCAN_SPLIT_ASSIGNMENT_BATCH_SIZE, 0);
        assertThatThrownBy(
                        () -> FlinkConnectorOptionsUtils.validateTableSourceOptions(tableOptions))
                .isInstanceOf(ValidationException.class)
                .hasMessage("'scan.split.assignment.batch-size' must be positive, but was 0.");
    }

    @Test
    void testGetBoundedOptions() {
        org.apache.flink.configuration.Configuration tableOptions =
                new org.apache.flink.configuration.Configuration();
        // default: unbounded
        FlinkConnectorOptionsUtils.BoundedOptions boundedOptions =
                FlinkConnectorOptionsUtils.getBoundedOptions(tableOptions, ZoneId.systemDefault());
        assertThat(boundedOptions.boundedMode).isEqualTo(ScanBoundedMode.UNBOUNDED);

        // latest
        tableOptions.set(SCAN_BOUNDED_MODE, ScanBoundedMode.LATEST);
        boundedOptions =
                FlinkConnectorOptionsUtils.getBoundedOptions(tableOptions, ZoneId.systemDefault());
        assertThat(boundedOptions.boundedMode).isEqualTo(ScanBoundedMode.LATEST);

        // timestamp
        tableOptions.set(SCAN_BOUNDED_MODE, ScanBoundedMode.TIMESTAMP);
        tableOptions.set(SCAN_BOUNDED_TIMESTAMP, "1702134552000");
        boundedOptions =
                FlinkConnectorOptionsUtils.getBoundedOptions(tableOptions, ZoneId.systemDefault());
        assertThat(boundedOptions.boundedMode).isEqualTo(ScanBoundedMode.TIMESTAMP);
        assertThat(boundedOptions.boundedTimestampMs).isEqualTo(1702134552000L);
    }

    @Test
    void testToStoppingOffsetsInitializer() {
        FlinkConnectorOptionsUtils.BoundedOptions unbounded =
                new FlinkConnectorOptionsUtils.BoundedOptions();
        assertThat(FlinkConnectorOptionsUtils.toStoppingOffsetsInitializer(unbounded)).isNull();

        FlinkConnectorOptionsUtils.BoundedOptions latest =
                new FlinkConnectorOptionsUtils.BoundedOptions();
        latest.boundedMode = ScanBoundedMode.LATEST;
        assertThat(FlinkConnectorOptionsUtils.toStoppingOffsetsInitializer(latest))
                .isInstanceOf(OffsetsInitializer.latest().getClass());

        FlinkConnectorOptionsUtils.BoundedOptions timestamp =
                new FlinkConnectorOptionsUtils.BoundedOptions();
        timestamp.boundedMode = ScanBoundedMode.TIMESTAMP;
        timestamp.boundedTimestampMs = 100L;
        assertThat(FlinkConnectorOptionsUtils.toStoppingOffsetsInitializer(timestamp))
                .isInstanceOf(OffsetsInitializer.timestamp(100L).getClass());
    }

    @Test
    void testValidateScanBoundedMode() {
        org.apache.flink.configuration.Configuration tableOptions =
                new org.apache.flink.configuration.Configuration();
        tableOptions.set(SCAN_BOUNDED_MODE, ScanBoundedMode.TIMESTAMP);
        assertThatThrownBy(
                        () -> FlinkConnectorOptionsUtils.validateTableSourceOptions(tableOptions))
                .isInstanceOf(ValidationException.class)
                .hasMessageContaining(SCAN_BOUNDED_TIMESTAMP.key());

        tableOptions.set(SCAN_BOUNDED_TIMESTAMP, "2023-12-09 23:09:12");
        FlinkConnectorOptionsUtils.validateTableSourceOptions(tableOptions);
    }

    @Test
    void testGetClientScannerIoTmpDir() {
        Configuration flussConfig =
                new Configuration().set(CLIENT_SCANNER_IO_TMP_DIR, "/fluss_tmp_dir");
        org.apache.flink.configuration.Configuration flinkConfig =
                new org.apache.flink.configuration.Configuration().set(TMP_DIRS, "/flink_tmp_dir");
        assertThat(
                        FlinkConnectorOptionsUtils.getClientScannerIoTmpDir(
                                flussConfig, new org.apache.flink.configuration.Configuration()))
                .isEqualTo("/fluss_tmp_dir");
        assertThat(FlinkConnectorOptionsUtils.getClientScannerIoTmpDir(flussConfig, flinkConfig))
                .isEqualTo("/fluss_tmp_dir");
        String property = System.getProperty("java.io.tmpdir");
        assertThat(
                        FlinkConnectorOptionsUtils.getClientScannerIoTmpDir(
                                new Configuration(),
                                new org.apache.flink.configuration.Configuration()))
                .isEqualTo(property + "/fluss");

        // only replace when flussConfig not contains CLIENT_SCANNER_IO_TMP_DIR while flinkConfig
        // contains TMP_DIRS.
        assertThat(
                        FlinkConnectorOptionsUtils.getClientScannerIoTmpDir(
                                new Configuration(), flinkConfig))
                .isEqualTo("/flink_tmp_dir/fluss");
    }
}
