<!-- SPDX-License-Identifier: Apache-2.0
     https://www.apache.org/licenses/LICENSE-2.0 -->

# Client logging

The Rust core emits diagnostic events through the `log` facade. Python, C++, and
Elixir operations that call the core share these events. The embedding application
chooses the logger, output destination, and level filter. The core does not install
a logger, set a process-wide filter, or create log files. Without a logger, these
events produce no output.

## Events and levels

Use the Java client's corresponding operation as the reference for event levels.
The table covers writing, log fetching, and connection setup. It does not imply
that every Java diagnostic event has a Rust equivalent.

| Event | Level | Java reference |
| --- | --- | --- |
| Sender wakeup, flush start/end, batch completion | TRACE | `WriterClient.send/flush`, `WriteBatch.done` |
| Writer ID and batch sequence assignment; sender start, drain, stop | DEBUG | `RecordAccumulator.drain`, `Sender.run` |
| Writer close start/end | INFO | `WriterClient.close` |
| Write retry and final batch failure; graceful close timeout | WARN | `Sender.handleWriteBatchError/prepareWriteRetry`, `WriterClient.close` |
| Unexpected sender loop failures; aborting batches after a fatal error | ERROR | `Sender.run/maybeAbortBatches` |
| Fetch request failure and bucket fetch errors, including retriable errors | ERROR | `LogFetcher.handleFetchLogException/handleFetchLogExceptionForBucket` |
| Connection creation, authentication start, connection ready | DEBUG | `ServerConnection` connection and authentication lifecycle |
| Connection setup, API version handshake, or authentication failure | ERROR | `ServerConnection` initialization failure paths |

Rust also logs the append retry after a new batch requires bucket reassignment at
TRACE. This identifies a routing step before the sender wakeup.

Java sources: [WriterClient][java-writer], [RecordAccumulator][java-accumulator],
[Sender][java-sender], [WriteBatch][java-batch], [LogFetcher][java-fetcher], and
[ServerConnection][java-connection].

Retriability alone does not determine the level: Java logs write retries at WARN
and fetch failures at ERROR. Preserve the retry decision and the error returned
to the caller when adding a log. An ERROR event can therefore describe a failed
fetch attempt that later recovers. API validation errors returned directly to the
caller do not need another log at every layer.

Keep each retry visible; do not add sampling or rate limiting to these events.
If an application selects INFO, it sees INFO, WARN, and ERROR events. TRACE and
DEBUG remain available for detailed diagnosis. INFO is an example application
filter, not an SDK-installed default. Repeated failures can produce substantial
output, so the application's backend must control storage retention.

## Context and cost

- Include existing identifiers relevant to the event: physical table path,
  `TableBucket` (table, partition when present, bucket), batch ID, sequence,
  attempts, fetch offset, or server node. Preserve the error kind and message.
- Use batch ID with table/bucket context. Batch IDs belong to an accumulator and
  are not global request IDs; these events do not form a complete batch-to-RPC
  trace. Batch completion logs contain the table and batch ID, while sender
  retry/failure logs also contain the bucket and sequence.
- Record terminal batch completion once, after winning the completion transition.
  Emit close-complete only after the close path finishes.
- Do not log rows, keys, request/response payloads, credentials, tokens, or full
  configuration objects. Error text can originate at the server; avoid adding
  sensitive values to errors and inspect collected logs before sharing them.
- Pass values directly to `log` macros instead of building strings first. Guard
  expensive diagnostic-only computation with `log::log_enabled!`. Routine write
  diagnostics belong at batch/lifecycle boundaries, not on every record.
- Use the shared core for shared operations. A binding-specific event should
  describe behavior owned by that binding and avoid duplicating a core error.

Enabled logs incur formatting and backend costs. TRACE can be frequent, and a
synchronous application logger may block the calling thread. File output,
rotation, and asynchronous buffering are backend responsibilities.

## Testing changes

Use the test-only `LogCapture` helper on a current-thread Tokio runtime or a
synchronous test. Assert the level and useful context together with the operation's
result, retry behavior, or metadata invalidation. Check that detailed events are
filtered at INFO and that repeated completion/close does not create duplicate
terminal events. Connection tests should exercise success and failure without
logging authentication values.

[java-writer]: https://github.com/apache/fluss/blob/main/fluss-client/src/main/java/org/apache/fluss/client/write/WriterClient.java
[java-accumulator]: https://github.com/apache/fluss/blob/main/fluss-client/src/main/java/org/apache/fluss/client/write/RecordAccumulator.java
[java-sender]: https://github.com/apache/fluss/blob/main/fluss-client/src/main/java/org/apache/fluss/client/write/Sender.java
[java-batch]: https://github.com/apache/fluss/blob/main/fluss-client/src/main/java/org/apache/fluss/client/write/WriteBatch.java
[java-fetcher]: https://github.com/apache/fluss/blob/main/fluss-client/src/main/java/org/apache/fluss/client/table/scanner/log/LogFetcher.java
[java-connection]: https://github.com/apache/fluss/blob/main/fluss-rpc/src/main/java/org/apache/fluss/rpc/netty/client/ServerConnection.java
