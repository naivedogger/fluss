# C++ consumer group acceptance test

Each phase starts a new OS process and uses the public C++ SDK. One run creates two temporary tables (1 and 2 buckets) and three unique groups. Use a dedicated database with create/drop
table permission and group read/write/drop permission. The default run does not change cluster settings.

| Phases | Expected result |
|---|---|
| seed, no-commit, partial | Write IDs 0–5 and flush. Read without committing; a new process reads the same six IDs. Commit next offset 2 after processing only 0 and 1. |
| resume, append, tail | Read exactly 2–5, asynchronously commit 6 and verify via Admin. Append 6 and 7; a new consumer reads exactly those IDs and commits 8. |
| isolated-auto | A second group starts at 0. After delivery, another poll triggers auto-commit; Admin must observe 8 without an explicit commit. |
| split-commit, split-resume | Write 64 distinct IDs to a 2-bucket keyed table and record the actual ID sequence per bucket. Persist 1 for both buckets, then submit 3 for both, with oversized metadata only on B. Require request success, A success and B error 80. A new process must see saved positions (3,1) and read exactly the corresponding suffixes. |
| cross-arrow | Use the multi-table scanner to persist 3 and 2 for bucket 0 of two different tables. A fresh Arrow scanner must return the correct table ID, bucket, batch start offset and every remaining business ID for both tables. |
| admin, cleanup | Reset the first group to 3 and actually read back 3–7. Delete only its offset; the second group stays at 8. Delete all three test groups, verify missing state and drop both tables. |

Run from the extracted Linux kit on the ECS after updating the **private** config copy:

    cd /root/cpp-test/consumer-group
    python3 kit/run.py --binary kit/consumer_group_e2e --config config.json --output results

The runner reads endpoint, database, security_protocol, sasl_mechanism, username,
password, and optional replication_factor from the existing JSON format. Set the
endpoint and credentials for the intended cluster before starting. Each phase has
a 120 s deadline. A failure exits nonzero and keeps the table, groups and phase
logs for diagnosis; the generated names and outcomes are in results/<run>/result.json.

For the partial rejection, the default invalid metadata is 8 KiB. If the cluster
accepts metadata that large, set `--reject-metadata-bytes` above its configured
limit (at most 1 MiB for this test). An unexpectedly accepted bucket fails the test.

### Owner migration and ACL denial

To include both operator-assisted cases, add `--owner-move --acl-config subject.json`
to the run command. The second config is a separate ordinary SASL user's credentials
for the same cluster. Keep it private. Omitted cases are recorded as `not_run` in
`result.json`. A requested case with missing evidence times out and fails.

Use a second terminal and the exact output directory and generated group names printed
by this run. Marker files acknowledge changes made via the **cluster control plane**;
the runner does not move live replicas or administer ACLs. Use only a dedicated
test principal with no inherited group access.

1. When `owner-moved.ready` appears, the original scanner has cached the owner and
   committed 2. Discover the owner of the **printed primary group**; move leadership
   of its offsets-table bucket using the cluster's approved procedure. Re-query the
   same group until its new owner is confirmed. Atomically publish the observed
   group name and numeric old/new node IDs (space-separated) in `owner-moved`
   (write a temporary file in the run directory, then rename it). A restart with
   the same owner is not a migration. The original scanner must now commit 6;
   a new process must restore 6, read IDs 6–7 and commit 8.
2. At `acl-granted.ready`, grant the restricted user READ on the generated
   **single-bucket source table** and DESCRIBE on the printed **-multi group**.
   Once effective, create `acl-granted`. The user must restore the already
   committed position 3 and read IDs 3–7. At `acl-revoked.ready`, remove this
   user's group access only. Once effective, create `acl-revoked`. Direct reads
   must still return IDs 0–7, while a new committed subscription with earliest
   fallback must return authorization error 48. The privileged client verifies
   that the stored position is still 3. Remove the temporary table grant after
   the run; on failure also remove any remaining temporary group grant.

Each manual gate allows ten minutes. Use results from the real management operation
for the marker, not a guessed ID or the leader of an arbitrary source bucket.

To build from source on a build host with the SDK prerequisites (Rust, CMake,
C++17, protobuf and system Arrow), from the repository root:

    suite=fluss-rust/bindings/cpp/test/consumer_group_ecs
    cmake -S "$suite" -B build/group -DCMAKE_BUILD_TYPE=Release
    cmake --build build/group --target consumer_group_e2e -j4
    python3 "$suite/run.py" --binary build/group/consumer_group_e2e --config /path/to/config.json

The migration case exercises a live owner change, not a cross-version rolling upgrade.
Partitioned tables, cross-language recovery, and automatic rebalancing are outside this suite.
