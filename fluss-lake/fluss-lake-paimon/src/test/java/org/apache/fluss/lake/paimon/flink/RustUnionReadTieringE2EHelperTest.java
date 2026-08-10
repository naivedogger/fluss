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

package org.apache.fluss.lake.paimon.flink;

import org.apache.fluss.client.Connection;
import org.apache.fluss.client.ConnectionFactory;
import org.apache.fluss.client.admin.Admin;
import org.apache.fluss.client.metadata.LakeSnapshot;
import org.apache.fluss.config.ConfigOptions;
import org.apache.fluss.config.Configuration;
import org.apache.fluss.flink.tiering.LakeTieringJobBuilder;
import org.apache.fluss.metadata.DataLakeFormat;
import org.apache.fluss.metadata.TableBucket;
import org.apache.fluss.metadata.TablePath;

import org.apache.flink.api.common.JobStatus;
import org.apache.flink.api.common.RuntimeExecutionMode;
import org.apache.flink.core.execution.JobClient;
import org.apache.flink.streaming.api.environment.StreamExecutionEnvironment;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.condition.EnabledIfEnvironmentVariable;

import java.time.Duration;
import java.util.Collections;
import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.TimeUnit;

import static org.apache.fluss.flink.tiering.source.TieringSourceOptions.POLL_TIERING_TABLE_INTERVAL;
import static org.assertj.core.api.Assertions.assertThat;

/**
 * Runs the production Flink-to-Paimon tiering pipeline for the Rust UnionRead cross-language E2E.
 */
@EnabledIfEnvironmentVariable(named = "FLUSS_RUST_UNION_READ_TIERING_E2E", matches = "true")
class RustUnionReadTieringE2EHelperTest {

    private static final Duration DEFAULT_TIMEOUT = Duration.ofMinutes(2);
    private static final Duration POLL_INTERVAL = Duration.ofMillis(200);

    @Test
    void tierUntilTargetOffsetBecomesReadable() throws Exception {
        String bootstrapServers =
                requiredEnvironmentVariable("FLUSS_RUST_UNION_READ_BOOTSTRAP_SERVERS");
        String warehouse = requiredEnvironmentVariable("FLUSS_RUST_UNION_READ_PAIMON_WAREHOUSE");
        String database = requiredEnvironmentVariable("FLUSS_RUST_UNION_READ_TABLE_DATABASE");
        String table = requiredEnvironmentVariable("FLUSS_RUST_UNION_READ_TABLE_NAME");
        long tableId =
                Long.parseLong(requiredEnvironmentVariable("FLUSS_RUST_UNION_READ_TABLE_ID"));
        long targetOffset =
                Long.parseLong(
                        requiredEnvironmentVariable("FLUSS_RUST_UNION_READ_TARGET_LOG_END_OFFSET"));
        Long targetPartitionId =
                optionalLongEnvironmentVariable("FLUSS_RUST_UNION_READ_TARGET_PARTITION_ID");
        Duration timeout =
                Duration.ofSeconds(
                        Long.parseLong(
                                environmentVariableOrDefault(
                                        "FLUSS_RUST_UNION_READ_TIERING_TIMEOUT_SECONDS",
                                        String.valueOf(DEFAULT_TIMEOUT.getSeconds()))));

        Configuration flussConfig = new Configuration();
        flussConfig.set(
                ConfigOptions.BOOTSTRAP_SERVERS, Collections.singletonList(bootstrapServers));
        flussConfig.set(POLL_TIERING_TABLE_INTERVAL, POLL_INTERVAL);

        Map<String, String> paimonOptions = new HashMap<>();
        paimonOptions.put("metastore", "filesystem");
        paimonOptions.put("warehouse", warehouse);

        StreamExecutionEnvironment execEnv = StreamExecutionEnvironment.getExecutionEnvironment();
        execEnv.setRuntimeMode(RuntimeExecutionMode.STREAMING);
        execEnv.setParallelism(1);

        TablePath tablePath = TablePath.of(database, table);
        TableBucket tableBucket = new TableBucket(tableId, targetPartitionId, 0);
        JobClient jobClient = null;
        try (Connection connection =
                        createConnectionWithRetry(flussConfig, Duration.ofSeconds(30));
                Admin admin = connection.getAdmin()) {
            jobClient =
                    LakeTieringJobBuilder.newBuilder(
                                    execEnv,
                                    flussConfig,
                                    Configuration.fromMap(paimonOptions),
                                    new Configuration(),
                                    DataLakeFormat.PAIMON.toString())
                            .build();

            LakeSnapshot readableSnapshot =
                    waitForReadableSnapshot(
                            admin, jobClient, tablePath, tableBucket, targetOffset, timeout);
            assertThat(readableSnapshot.getSnapshotId()).isGreaterThanOrEqualTo(0);
            assertThat(readableSnapshot.getTableBucketsOffset())
                    .containsEntry(tableBucket, targetOffset);
        } finally {
            if (jobClient != null) {
                jobClient.cancel().get(30, TimeUnit.SECONDS);
            }
        }
    }

    private static Connection createConnectionWithRetry(Configuration flussConfig, Duration timeout)
            throws InterruptedException {
        long deadlineNanos = System.nanoTime() + timeout.toNanos();
        RuntimeException lastError = null;
        while (System.nanoTime() < deadlineNanos) {
            try {
                return ConnectionFactory.createConnection(flussConfig);
            } catch (RuntimeException e) {
                lastError = e;
                Thread.sleep(POLL_INTERVAL.toMillis());
            }
        }
        throw new IllegalStateException(
                "Timed out connecting the Java tiering helper to "
                        + flussConfig.get(ConfigOptions.BOOTSTRAP_SERVERS),
                lastError);
    }

    private static LakeSnapshot waitForReadableSnapshot(
            Admin admin,
            JobClient jobClient,
            TablePath tablePath,
            TableBucket tableBucket,
            long targetOffset,
            Duration timeout)
            throws Exception {
        long deadlineNanos = System.nanoTime() + timeout.toNanos();
        Exception lastSnapshotError = null;
        LakeSnapshot lastSnapshot = null;
        while (System.nanoTime() < deadlineNanos) {
            JobStatus jobStatus = jobClient.getJobStatus().get(10, TimeUnit.SECONDS);
            assertThat(jobStatus)
                    .as("production tiering job must remain alive while waiting for %s", tablePath)
                    .isNotIn(JobStatus.FAILED, JobStatus.CANCELED, JobStatus.FINISHED);

            try {
                lastSnapshot = admin.getReadableLakeSnapshot(tablePath).get(10, TimeUnit.SECONDS);
                Long readableOffset = lastSnapshot.getTableBucketsOffset().get(tableBucket);
                if (readableOffset != null && readableOffset >= targetOffset) {
                    return lastSnapshot;
                }
            } catch (Exception e) {
                lastSnapshotError = e;
            }
            Thread.sleep(POLL_INTERVAL.toMillis());
        }

        throw new AssertionError(
                String.format(
                        "Timed out after %s waiting for readable lake snapshot of %s bucket %s "
                                + "to cover target offset %d; last snapshot was %s",
                        timeout, tablePath, tableBucket, targetOffset, lastSnapshot),
                lastSnapshotError);
    }

    private static String requiredEnvironmentVariable(String name) {
        String value = System.getenv(name);
        assertThat(value).as("environment variable %s", name).isNotBlank();
        return value;
    }

    private static String environmentVariableOrDefault(String name, String defaultValue) {
        String value = System.getenv(name);
        return value == null || value.trim().isEmpty() ? defaultValue : value;
    }

    private static Long optionalLongEnvironmentVariable(String name) {
        String value = System.getenv(name);
        return value == null || value.trim().isEmpty() ? null : Long.valueOf(value);
    }
}
