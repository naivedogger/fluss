# Apache Fluss™ C++ Bindings

C++ bindings for Fluss, built on top of the [fluss-rust](../../crates/fluss) client. The API is exposed via a C++ header ([include/fluss.hpp](include/fluss.hpp)) and implemented with Rust FFI.

## Requirements

- Rust (see [rust-toolchain.toml](../../rust-toolchain.toml) at repo root)
- C++17-capable compiler
- CMake 3.18+ and/or Bazel
- Apache Arrow (for Arrow-based APIs)

## Build

From the repository root or from `bindings/cpp`:

**With CMake:**

```bash
cd bindings/cpp
mkdir build && cd build
cmake ..
cmake --build .
```

By default, CMake now uses `Release` when `CMAKE_BUILD_TYPE` is not specified.

**With Bazel:**

```bash
cd bindings/cpp
bazel build //...
```
`ci.sh` defaults to optimized builds via `-c opt` (override with `BAZEL_BUILD_FLAGS` if needed).
See [ci.sh](ci.sh) for the CI build sequence.

## Examples and Documentation

- [examples/example.cpp](examples/example.cpp) demonstrates log-table writes, continuous scans,
  bounded Arrow record-batch scans, projections, and offset queries.
- [examples/admin_example.cpp](examples/admin_example.cpp) demonstrates database, table,
  partition, and cluster administration.
- [examples/kv_example.cpp](examples/kv_example.cpp) and
  [examples/kv_changelog_example.cpp](examples/kv_changelog_example.cpp) demonstrate
  primary-key table access.
- The website documentation includes the
  [C++ API reference](../../website/docs/user-guide/cpp/api-reference.md) and
  [log-table examples](../../website/docs/user-guide/cpp/example/log-tables.md).

For a bounded log scan, first subscribe the record-batch scanner at the desired starting
offsets, then create a `RecordBatchLogReader` with either explicit stopping offsets or the
latest offsets observed at reader creation:

```cpp
auto info = table.GetTableInfo();
std::vector<int32_t> bucket_ids;
for (int32_t bucket_id = 0; bucket_id < info.num_buckets; ++bucket_id) {
    bucket_ids.push_back(bucket_id);
}

std::unordered_map<int32_t, int64_t> latest_offsets;
admin.ListOffsets(table_path, bucket_ids, fluss::OffsetSpec::Latest(), latest_offsets);

fluss::LogScanner scanner;
table.NewScan().CreateRecordBatchLogScanner(scanner);

std::vector<fluss::ReaderStopOffset> stopping_offsets;
for (int32_t bucket_id : bucket_ids) {
    scanner.Subscribe(bucket_id, 0);
    stopping_offsets.push_back(
        {fluss::TableBucket{info.table_id, bucket_id}, latest_offsets.at(bucket_id)});
}

fluss::RecordBatchLogReader reader;
scanner.CreateRecordBatchLogReaderUntilOffsets(stopping_offsets, reader);

while (true) {
    fluss::ArrowRecordBatches batches;
    fluss::BoundedReadStatus status;
    reader.NextBatch(1000, batches, status);
    if (status == fluss::BoundedReadStatus::TimedOut) {
        continue;  // Check query cancellation before retrying.
    }
    if (status == fluss::BoundedReadStatus::Finished) {
        break;
    }
    for (const auto& batch : batches) {
        process(batch->GetArrowRecordBatch());
    }
}
```

`CollectAllBatches()` is available when materializing the complete bounded result is
preferred. `NextBatch()` reports timeout separately from completion so engines can periodically
check cancellation. Use only one active reader or polling operation for a scanner at a time.

## TODO

- [ ] How to introduce fluss-cpp in your own project, https://github.com/apache/opendal/blob/main/bindings/cpp/README.md is a good reference
- [ ] Add CMake/Bazel install and packaging instructions.
- [ ] Add more C++ examples (upsert, partitioned bounded scans, etc.).
