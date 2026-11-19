// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

#ifndef FLUSS_UNION_H
#define FLUSS_UNION_H

#include <stddef.h>
#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

struct ArrowArrayStream;
struct ArrowArray;
typedef struct FlussUnionClient FlussUnionClient;
typedef struct FlussUnionPlan FlussUnionPlan;
typedef struct FlussUnionCancel FlussUnionCancel;
typedef struct FlussUnionProperty {
    const char* key;
    const char* value;
} FlussUnionProperty;

/* Core failures follow the existing C++ binding: server API codes are retained,
 * transport failures use NetworkException, other core failures use CLIENT_ERROR.
 * UnionRead-specific codes occupy a separate negative range. Codes describe
 * failures, not retry policy; unknown nonzero codes must be treated as errors.
 */
typedef enum FlussUnionErrorCode {
    FLUSS_UNION_OK = 0,
    FLUSS_UNION_CLIENT_ERROR = -2,
    FLUSS_UNION_INVALID_ARGUMENT = -1000,
    FLUSS_UNION_CANCELLED = -1001,
    FLUSS_UNION_NOT_LAKE_READABLE = -1002,
    FLUSS_UNION_PLANNING_FAILED = -1003,
    FLUSS_UNION_DATA_UNAVAILABLE = -1004,
    FLUSS_UNION_SCHEMA_INCOMPATIBLE = -1005,
    FLUSS_UNION_CONNECTION_ERROR = -1006,
    FLUSS_UNION_UNSUPPORTED_MERGE_ENGINE = -1007,
    FLUSS_UNION_INCOMPATIBLE_SPLIT_VERSION = -1008,
    FLUSS_UNION_INVALID_READ_CONTEXT = -1009,
    FLUSS_UNION_INTERNAL = -1010,
    FLUSS_UNION_MEMORY_LIMIT_EXCEEDED = -1011
} FlussUnionErrorCode;

/* Experimental process-local ABI. No compatibility promise yet.
 * All strings are UTF-8. Inputs are borrowed only during the call.
 * Status 0 means success; otherwise it is a Fluss error code.
 * On error, outputs are cleared and *error (if supplied)
 * is owned by the caller; free it with fluss_union_error_release().
 * Output slots must not already own resources. Invalid/dangling pointers are UB.
 * Call from ordinary native threads, outside a Tokio runtime. Do not unload the
 * library while any handle, stream, schema or batch is still alive.
 */

/* Creates a connection and runtime shared by its plans and streams.
 * The SASL constructor authenticates with PLAIN. This transport does not use TLS.
 * The constructor without credentials uses PLAINTEXT.
 */
int fluss_union_client_create_sasl(const char* bootstrap, const char* username, const char* password,
                                   const FlussUnionCancel* cancel, FlussUnionClient** out, char** error);
int fluss_union_client_create(const char* bootstrap, FlussUnionClient** out, char** error);

/* Captures boundaries and plans a full bounded table scan. Blocking metadata I/O.
 * Lake properties (including credentials) remain local to the plan and streams.
 * Multiple plans may share a client. Queries apply their own filters/projections.
 */
int fluss_union_plan_create(const FlussUnionClient* client, const char* database,
                           const char* table, const FlussUnionProperty* properties,
                           size_t property_count, FlussUnionPlan** out, char** error);

/* Requires a live, non-null plan. This is task count, not execution concurrency.
 * Append lake tasks and log tasks are independent. PK tasks retain a complete
 * bucket's baseline and tail so default reconciliation stays within one task.
 */
size_t fluss_union_plan_split_count(const FlussUnionPlan* plan);

/* Opens [first, first + count) tasks, lazily and sequentially, without I/O.
 * For parallel execution, open independent ranges on engine-selected threads.
 * A task must be assigned once per query attempt; reopening replays its rows.
 * count=0 gives an empty stream with the output schema.
 * The stream owns everything needed to outlive both the plan and client.
 * get_next blocks for one batch. Do not call callbacks concurrently on the same
 * stream. Different streams may be consumed concurrently. Scheduling/yielding
 * occurs between batches; this ABI is not an asynchronous pipeline poll API.
 * An error is terminal: discard partial results before retrying the query.
 * Follow the Arrow C Stream/Data interfaces for schema/batch/stream ownership.
 * Returned batches can outlive their stream. Release the stream early to stop
 * reading; never release it concurrently with a callback.
 */
int fluss_union_plan_open(const FlussUnionPlan* plan, size_t first, size_t count,
                         struct ArrowArrayStream* out, FlussUnionCancel** cancel, char** error);

/* Arrow callbacks return errno-style status, not Fluss error codes.
 * This accessor returns the last Fluss error code (0 before any error); obtain
 * its message through get_last_error. Both remain valid until another callback
 * failure or stream release. Requires the original live stream returned by
 * fluss_union_plan_open or fluss_union_append_open, not a wrapper or a
 * third-party stream. Serialize with
 * callbacks/release on that stream. This accessor performs no I/O.
 */
int fluss_union_stream_error_code(const struct ArrowArrayStream* stream);

/* Timeout-aware alternative to get_next for engine scan threads. A successful
 * call with *timed_out=true has no batch and is not EOF; poll again or cancel.
 * On success with *timed_out=false, follow Arrow's normal batch/EOF convention.
 * Timeout preserves the pending source stream. Callback threading and Fluss
 * error-code access rules are identical to get_next. All pointers are non-null.
 * The timeout bounds async waiting, not a synchronous foreign callback. Imported
 * readers must use the engine's cancellation/deadline mechanism for blocking I/O.
 * Cancellation observed when a callback returns discards its batch. It does not
 * make it safe to release the stream while that callback is still running.
 */
int fluss_union_stream_poll(struct ArrowArrayStream* stream, struct ArrowArray* out,
                           uint32_t timeout_ms, bool* timed_out);

/* Engine-owned append task. snapshot_id >= 0 reads the entire pinned lake
 * snapshot (currently unpartitioned tables only); -1 reads one Fluss log range.
 * Log tasks use partition_id=-1 for an unpartitioned table, otherwise the
 * concrete partition id. Offsets are concrete and half-open, never sentinels.
 * table_id/schema_id/modified_time are captured by the coordinator; metadata
 * changes fail the attempt. PK tables are rejected. This struct is a local ABI,
 * not a wire format: the engine owns transport and task assignment.
 */
typedef struct FlussUnionAppendTask {
    int64_t table_id;
    int32_t schema_id;
    int64_t modified_time;
    int64_t snapshot_id;
    int64_t partition_id;
    int32_t bucket_id;
    int64_t start_offset;
    int64_t stop_offset;
} FlussUnionAppendTask;

/* Imports a frozen task; never recaptures snapshot ids or log offsets.
 * Initialization performs metadata I/O. Subsequent batch reads, cancellation,
 * ownership and errors follow fluss_union_plan_open. Output contains all columns;
 * the engine applies its projection, filters and limit.
 */
int fluss_union_append_open(const FlussUnionClient* client, const char* database,
                           const char* table, const FlussUnionAppendTask* task,
                           const FlussUnionProperty* properties, size_t property_count,
                           struct ArrowArrayStream* out, FlussUnionCancel** cancel, char** error);

/* Complete frozen source boundary for a PK bucket task. Experimental support is
 * currently unpartitioned. Supply every bucket exactly once, including empty
 * ranges. The selected bucket owns its entire baseline and changelog tail.
 * Lake file planning may happen locally against the pinned snapshot; task
 * assignment uses bucket identity, never independently generated file ordinals.
 */
typedef struct FlussUnionBoundary {
    int64_t table_id;
    int32_t schema_id;
    int64_t modified_time;
    int64_t snapshot_id; /* -1: no lake snapshot */
} FlussUnionBoundary;
typedef struct FlussUnionLogRange {
    int32_t bucket_id;
    int64_t start_offset;
    int64_t stop_offset;
    bool has_lake_baseline;
} FlussUnionLogRange;

/* Output/lifetime rules match append_open. Metadata identity is checked during
 * open; lake planning and tail reads are lazy and cancellable by stream token.
 * memory_limit=0 is unlimited; otherwise it caps this task's PK retained state,
 * not total query memory. Engines still account for file readers and batches.
 */
int fluss_union_bucket_open(const FlussUnionClient* client, const char* database,
                           const char* table, const FlussUnionBoundary* boundary,
                           const FlussUnionLogRange* ranges, size_t range_count,
                           int32_t bucket_id, size_t memory_limit,
                           const FlussUnionProperty* properties, size_t property_count,
                           struct ArrowArrayStream* out, FlussUnionCancel** cancel, char** error);

/* Engine accounting for retained PK state, not an allocator or a total scan
 * budget. Callbacks must be thread-safe, nonblocking and must not unwind.
 * reserve returns false without charging on failure; release returns reserved
 * bytes. destroy releases the callback state after all reservations are returned.
 * Passing this object transfers ownership at entry, even on open failure, and
 * clears all its fields. The caller may immediately discard the cleared struct.
 * Callback state must retain the query tracker independently of the scanner.
 */
typedef struct FlussUnionMemoryBudget {
    void* data;
    bool (*reserve)(void* data, size_t bytes);
    void (*release)(void* data, size_t bytes);
    void (*destroy)(void* data);
} FlussUnionMemoryBudget;

/* Same as bucket_open; a non-null memory budget takes precedence over the local
 * memory_limit. A null budget preserves the original per-task limit behavior.
 */
int fluss_union_bucket_open_with_memory(const FlussUnionClient* client, const char* database,
                           const char* table, const FlussUnionBoundary* boundary,
                           const FlussUnionLogRange* ranges, size_t range_count,
                           int32_t bucket_id, size_t memory_limit, FlussUnionMemoryBudget* memory,
                           const FlussUnionProperty* properties, size_t property_count,
                           struct ArrowArrayStream* out, FlussUnionCancel** cancel, char** error);

/* Optional engine baseline for the selected PK bucket. It must represent the
 * pinned snapshot's complete current view (one live row/key), with lake-specific
 * merges and deletion vectors already applied. Return all Fluss columns in order;
 * do not apply mutable-column predicates or limits before reconciliation.
 * A non-null baseline is moved and cleared at entry, including on failure.
 * Callbacks execute synchronously during poll on the consuming scan thread.
 * They must not unwind and must check engine cancellation during blocking I/O;
 * poll's timeout cannot interrupt a blocking foreign callback. The engine must
 * retain callback dependencies until release. Errors are terminal, never fallback.
 * A null baseline uses the default lake reader. Memory follows with_memory.
 */
int fluss_union_bucket_open_with_baseline(const FlussUnionClient* client, const char* database,
                           const char* table, const FlussUnionBoundary* boundary,
                           const FlussUnionLogRange* ranges, size_t range_count,
                           int32_t bucket_id, size_t memory_limit, FlussUnionMemoryBudget* memory,
                           struct ArrowArrayStream* baseline,
                           const FlussUnionProperty* properties, size_t property_count,
                           struct ArrowArrayStream* out, FlussUnionCancel** cancel, char** error);

/* Create a query/operation token before starting blocking initialization.
 * The cancellable entry points below borrow it during the call; NULL preserves
 * the existing non-cancellable initialization. Keep it alive until the call and
 * any concurrent fluss_union_cancel() calls have returned. Returned streams own
 * child tokens: parent cancellation reaches them, while cancelling a stream does
 * not cancel the parent or siblings. Tokens are one-shot; never reset/reuse a
 * cancelled token for a retry. Releasing a token does not cancel its children.
 * Client/plan creation observes this token only until it returns. Later plan_open
 * streams use their own returned tokens; they do not inherit the creation token.
 * Cancellation returns FLUSS_UNION_CANCELLED and clears outputs, with the same
 * ownership rules as the original entry point. It drops pending async work;
 * already sent RPCs may still finish remotely. It cannot interrupt synchronous
 * foreign callbacks or filesystem calls which do not yield.
 */
int fluss_union_cancel_create(FlussUnionCancel** out, char** error);
int fluss_union_client_create_cancellable(const char* bootstrap, const FlussUnionCancel* operation_cancel,
                           FlussUnionClient** out, char** error);

int fluss_union_plan_create_cancellable(const FlussUnionClient* client, const char* database,
                           const char* table, const FlussUnionProperty* properties,
                           size_t property_count, const FlussUnionCancel* operation_cancel,
                           FlussUnionPlan** out, char** error);

int fluss_union_append_open_cancellable(const FlussUnionClient* client, const char* database,
                           const char* table, const FlussUnionAppendTask* task,
                           const FlussUnionProperty* properties, size_t property_count,
                           const FlussUnionCancel* operation_cancel,
                           struct ArrowArrayStream* out, FlussUnionCancel** cancel, char** error);

int fluss_union_bucket_open_with_baseline_cancellable(const FlussUnionClient* client, const char* database,
                           const char* table, const FlussUnionBoundary* boundary,
                           const FlussUnionLogRange* ranges, size_t range_count,
                           int32_t bucket_id, size_t memory_limit, FlussUnionMemoryBudget* memory,
                           struct ArrowArrayStream* baseline,
                           const FlussUnionProperty* properties, size_t property_count,
                           const FlussUnionCancel* operation_cancel,
                           struct ArrowArrayStream* out, FlussUnionCancel** cancel, char** error);

/* May run on another thread to interrupt async initialization or get_next.
 * Idempotent.
 * Cancellation returns an error, not successful end-of-stream. The token owns
 * no stream; releasing the token does not cancel it. Keep the token alive until
 * concurrent cancel calls finish, then release it exactly once.
 */
void fluss_union_cancel(const FlussUnionCancel* cancel);
void fluss_union_cancel_release(FlussUnionCancel* cancel);

/* Release functions accept NULL; other handles must be released exactly once.
 * Do not release a client/plan concurrently with an operation borrowing it.
 */
void fluss_union_plan_release(FlussUnionPlan* plan);
void fluss_union_client_release(FlussUnionClient* client);
void fluss_union_error_release(char* error);

#ifdef __cplusplus
}
#endif
#endif
