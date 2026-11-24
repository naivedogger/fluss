# C++ consumer group acceptance test

Each phase starts a new OS process and uses the public C++ SDK. One run creates one
single-bucket log table and two unique groups. Use a dedicated database with create/drop
table permission and group read/write/drop permission. No changes to cluster settings.

| Phases | Expected result |
|---|---|
| seed, no-commit, partial | Write IDs 0–5 and flush. Read without committing; a new process reads the same six IDs. Commit next offset 2 after processing only 0 and 1. |
| resume, append, tail | Read exactly 2–5, asynchronously commit 6 and verify via Admin. Append 6 and 7; a new consumer reads exactly those IDs and commits 8. |
| isolated-auto | A second group starts at 0. After delivery, another poll triggers auto-commit; Admin must observe 8 without an explicit commit. |
| admin, cleanup | Reset the first group to 3 and actually read back 3–7. Delete only its offset; the second group stays at 8. Delete the two test groups, verify missing state and drop the test table. |

Run from the extracted Linux kit on the ECS after updating the **private** config copy:

    cd /root/cpp-test/consumer-group
    python3 kit/run.py --binary kit/consumer_group_e2e --config config.json --output results

The runner reads endpoint, database, security_protocol, sasl_mechanism, username,
password, and optional replication_factor from the existing JSON format. Set the
endpoint and credentials for the intended cluster before starting. Each phase has
a 120 s deadline. A failure exits nonzero and keeps the table, groups and phase
logs for diagnosis; the generated names and outcomes are in results/<run>/result.json.

To build from source on a build host with the SDK prerequisites (Rust, CMake,
C++17, protobuf and system Arrow), from the repository root:

    suite=fluss-rust/bindings/cpp/test/consumer_group_ecs
    cmake -S "$suite" -B build/group -DCMAKE_BUILD_TYPE=Release
    cmake --build build/group --target consumer_group_e2e -j4
    python3 "$suite/run.py" --binary build/group/consumer_group_e2e --config /path/to/config.json

This suite tests manual assignment, persisted offsets and Admin management. Owner
failover/rolling upgrade, cross-language recovery, ACL denial, and partitioned or
multi-table subscriptions need separate scenarios. It does not test rebalancing.
