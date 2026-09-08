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

import org.apache.fluss.client.admin.OffsetSpec;
import org.apache.fluss.client.metadata.LakeSnapshot;
import org.apache.fluss.client.table.Table;
import org.apache.fluss.client.table.writer.UpsertWriter;
import org.apache.fluss.config.ConfigOptions;
import org.apache.fluss.exception.LakeTableSnapshotNotExistException;
import org.apache.fluss.lake.paimon.testutils.FlinkPaimonTieringTestBase;
import org.apache.fluss.metadata.Schema;
import org.apache.fluss.metadata.TableBucket;
import org.apache.fluss.metadata.TableDescriptor;
import org.apache.fluss.metadata.TablePath;
import org.apache.fluss.server.testutils.FlussClusterExtension;
import org.apache.fluss.types.DataTypes;
import org.apache.fluss.utils.FileUtils;

import org.apache.flink.core.execution.JobClient;
import org.apache.flink.runtime.testutils.MiniClusterResourceConfiguration;
import org.apache.flink.streaming.util.TestStreamEnvironment;
import org.apache.flink.test.util.MiniClusterWithClientResource;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.condition.EnabledIfSystemProperty;
import org.junit.jupiter.api.extension.RegisterExtension;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.ValueSource;

import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.time.Duration;
import java.util.Arrays;
import java.util.Collections;
import java.util.Map;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;

import static org.apache.fluss.testutils.DataTestUtils.row;
import static org.apache.fluss.testutils.common.CommonTestUtils.waitUntil;
import static org.assertj.core.api.Assertions.assertThat;

/** Real tiering followed by a precompiled Rust UnionRead test on the same local warehouse. */
@EnabledIfSystemProperty(named = "fluss.rust.union-read.enabled", matches = "true")
class RustUnionReadITCase extends FlinkPaimonTieringTestBase {

    @RegisterExtension
    public static final FlussClusterExtension FLUSS_CLUSTER_EXTENSION =
            FlussClusterExtension.builder()
                    .setClusterConf(initConfig())
                    .setNumOfTabletServers(1)
                    .build();

    @BeforeAll
    protected static void beforeAll() {
        assertThat(System.getenv("FLUSS_RUST_UNION_READ_TEST_BIN"))
                .as("precompile the Rust test before starting this integration suite")
                .isNotBlank();
        assertThat(new File(System.getenv("FLUSS_RUST_UNION_READ_TEST_BIN")).canExecute())
                .as("Rust test executable must exist and be executable")
                .isTrue();
        FlinkPaimonTieringTestBase.beforeAll(FLUSS_CLUSTER_EXTENSION.getClientConfig());
    }

    @AfterAll
    static void closeWarehouse() throws Exception {
        try {
            if (paimonCatalog != null) {
                paimonCatalog.close();
            }
        } finally {
            FileUtils.deleteDirectory(new File(warehousePath).getParentFile());
        }
    }

    @Override
    protected FlussClusterExtension getFlussClusterExtension() {
        return FLUSS_CLUSTER_EXTENSION;
    }

    @ParameterizedTest
    @ValueSource(strings = {"append", "pk"})
    void testRustUnionRead(String scenario) throws Exception {
        boolean primaryKey = scenario.equals("pk");
        TablePath tablePath = TablePath.of(DEFAULT_DB, "rust_union_read_" + scenario);
        Schema.Builder schema =
                Schema.newBuilder()
                        .column("id", DataTypes.INT())
                        .column("name", DataTypes.STRING());
        if (primaryKey) {
            schema.primaryKey("id");
        }
        long tableId =
                createTable(
                        tablePath,
                        TableDescriptor.builder()
                                .schema(schema.build())
                                .distributedBy(1, "id")
                                .property(ConfigOptions.TABLE_DATALAKE_ENABLED, true)
                                .property(
                                        ConfigOptions.TABLE_DATALAKE_FRESHNESS,
                                        Duration.ofSeconds(1))
                                .customProperty("paimon.file.format", "parquet")
                                .build());
        try {
            writeRows(
                    tablePath,
                    Arrays.asList(row(1, "lake-old"), row(2, "lake-delete"), row(3, "lake-keep")),
                    !primaryKey);
            long seam = latestOffset(tablePath);
            assertThat(seam).isGreaterThan(0);
            if (primaryKey) {
                triggerAndWaitSnapshot(tableId, 1);
            }
            tierUntilReadable(tablePath, new TableBucket(tableId, 0), seam);

            // No tiering job remains: these rows must be read from the Fluss log.
            if (primaryKey) {
                try (Table table = conn.getTable(tablePath)) {
                    UpsertWriter writer = table.newUpsert().createWriter();
                    writer.upsert(row(1, "tail-new")).get();
                    writer.delete(row(2, "lake-delete")).get();
                    writer.upsert(row(4, "tail-insert")).get();
                    writer.flush();
                }
            } else {
                writeRows(tablePath, Arrays.asList(row(4, "tail-4"), row(5, "tail-5")), true);
            }
            assertThat(latestOffset(tablePath)).isGreaterThan(seam);
            runRustVerifier(tablePath, scenario);
        } finally {
            admin.dropTable(tablePath, false).get();
        }
    }

    private void tierUntilReadable(TablePath path, TableBucket bucket, long seam) throws Exception {
        MiniClusterWithClientResource miniCluster =
                new MiniClusterWithClientResource(
                        new MiniClusterResourceConfiguration.Builder()
                                .setNumberTaskManagers(1)
                                .setNumberSlotsPerTaskManager(2)
                                .build());
        miniCluster.before();
        TestStreamEnvironment.setAsContext(miniCluster.getMiniCluster(), 2);
        try {
            // Recreate the inherited execution environment in the MiniCluster context.
            super.beforeEach();
            JobClient job = buildTieringJob(execEnv);
            try {
                waitUntil(
                        () -> {
                            try {
                                LakeSnapshot snapshot =
                                        admin.getReadableLakeSnapshot(path)
                                                .get(10, TimeUnit.SECONDS);
                                return snapshot.getSnapshotId() >= 0
                                        && Long.valueOf(seam)
                                                .equals(
                                                        snapshot.getTableBucketsOffset()
                                                                .get(bucket));
                            } catch (ExecutionException e) {
                                if (e.getCause() instanceof LakeTableSnapshotNotExistException) {
                                    return false;
                                }
                                throw e;
                            }
                        },
                        Duration.ofMinutes(2),
                        Duration.ofMillis(200),
                        "readable Paimon snapshot did not reach baseline offset " + seam);
            } finally {
                job.cancel().get(30, TimeUnit.SECONDS);
            }
        } finally {
            TestStreamEnvironment.unsetAsContext();
            // Await cluster shutdown before writing the tail, not only cancellation
            // acknowledgement.
            miniCluster.after();
        }
    }

    private long latestOffset(TablePath path) throws Exception {
        return admin.listOffsets(path, Collections.singletonList(0), new OffsetSpec.LatestSpec())
                .bucketResult(0)
                .get(10, TimeUnit.SECONDS);
    }

    private void runRustVerifier(TablePath path, String scenario) throws Exception {
        ProcessBuilder builder =
                new ProcessBuilder(
                        System.getenv("FLUSS_RUST_UNION_READ_TEST_BIN"),
                        "--ignored",
                        "--exact",
                        "verify_tiered_union_read",
                        "--nocapture");
        Map<String, String> environment = builder.environment();
        environment.put(
                "FLUSS_RUST_UNION_READ_BOOTSTRAP_SERVERS",
                String.join(",", clientConf.get(ConfigOptions.BOOTSTRAP_SERVERS)));
        environment.put("FLUSS_RUST_UNION_READ_DATABASE", path.getDatabaseName());
        environment.put("FLUSS_RUST_UNION_READ_TABLE", path.getTableName());
        environment.put("FLUSS_RUST_UNION_READ_WAREHOUSE", warehousePath);
        environment.put("FLUSS_RUST_UNION_READ_SCENARIO", scenario);
        File log = new File("target/surefire-reports/rust-union-read-" + scenario + ".log");
        Files.createDirectories(log.toPath().getParent());
        Process process = builder.redirectErrorStream(true).redirectOutput(log).start();
        try {
            assertThat(process.waitFor(2, TimeUnit.MINUTES))
                    .as("Rust UnionRead verifier timed out for %s", scenario)
                    .isTrue();
            assertThat(process.exitValue())
                    .as("Rust UnionRead verifier failed; see %s", log)
                    .isZero();
            // libtest returns success for an unmatched filter too. Never accept zero tests.
            assertThat(new String(Files.readAllBytes(log.toPath()), StandardCharsets.UTF_8))
                    .as("the expected Rust test must actually run; see %s", log)
                    .contains("test result: ok. 1 passed; 0 failed; 0 ignored;");
        } finally {
            if (process.isAlive()) {
                process.destroyForcibly();
                assertThat(process.waitFor(10, TimeUnit.SECONDS))
                        .as("Rust verifier did not terminate")
                        .isTrue();
            }
        }
    }
}
