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

use crate::{AbiError, ErrorCode, c_string, guarded};
use arrow::array::{Array, StructArray};
use arrow::datatypes::SchemaRef;
use arrow::ffi::{FFI_ArrowArray, FFI_ArrowSchema};
use arrow::ffi_stream::FFI_ArrowArrayStream;
use arrow::record_batch::RecordBatch;
use std::pin::Pin;

type AbiStream = Pin<Box<dyn futures::Stream<Item = Result<RecordBatch, AbiError>> + Send>>;
use futures::StreamExt;
use std::ffi::{CString, c_char, c_int, c_void};
use std::ptr;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

// Keep the runtime alive until the input stream has been dropped. Separate
// streams share runtime workers, but have independent polling and cancellation.
struct Stream {
    input: Option<AbiStream>,
    schema: SchemaRef,
    runtime: Arc<Runtime>,
    cancel: CancellationToken,
    error: Option<CString>,
    error_code: c_int,
}

pub(crate) fn export<E: Into<AbiError> + 'static>(
    input: impl futures::Stream<Item = Result<RecordBatch, E>> + Send + 'static,
    schema: SchemaRef,
    runtime: Arc<Runtime>,
    cancel: CancellationToken,
) -> FFI_ArrowArrayStream {
    FFI_ArrowArrayStream {
        get_schema: Some(get_schema),
        get_next: Some(get_next),
        get_last_error: Some(get_last_error),
        release: Some(release),
        private_data: Box::into_raw(Box::new(Stream {
            input: Some(Box::pin(input.map(|batch| batch.map_err(Into::into)))),
            schema,
            runtime,
            cancel,
            error: None,
            error_code: 0,
        }))
        .cast::<c_void>(),
    }
}

impl Stream {
    fn status(&mut self, result: Result<(), AbiError>) -> c_int {
        match result {
            Ok(()) => 0,
            Err(error) => {
                self.error_code = error.code;
                self.error = Some(c_string(error.message));
                5 /* EIO */
            }
        }
    }
}

unsafe extern "C" fn get_schema(
    stream: *mut FFI_ArrowArrayStream,
    out: *mut FFI_ArrowSchema,
) -> c_int {
    let state = unsafe { &mut *(*stream).private_data.cast::<Stream>() };
    unsafe { out.write(FFI_ArrowSchema::empty()) };
    let result = guarded(|| {
        let schema =
            FFI_ArrowSchema::try_from(state.schema.as_ref()).map_err(AbiError::internal)?;
        unsafe { out.write(schema) };
        Ok(())
    });
    state.status(result)
}

unsafe extern "C" fn get_next(
    stream: *mut FFI_ArrowArrayStream,
    out: *mut FFI_ArrowArray,
) -> c_int {
    let mut timed_out = false;
    unsafe { poll_next(stream, out, None, &mut timed_out) }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_stream_poll(
    stream: *mut FFI_ArrowArrayStream,
    out: *mut FFI_ArrowArray,
    timeout_ms: u32,
    timed_out: *mut bool,
) -> c_int {
    unsafe {
        poll_next(
            stream,
            out,
            Some(std::time::Duration::from_millis(timeout_ms.into())),
            timed_out,
        )
    }
}

unsafe fn poll_next(
    stream: *mut FFI_ArrowArrayStream,
    out: *mut FFI_ArrowArray,
    timeout: Option<std::time::Duration>,
    timed_out: *mut bool,
) -> c_int {
    let state = unsafe { &mut *(*stream).private_data.cast::<Stream>() };
    unsafe {
        out.write(FFI_ArrowArray::empty());
        timed_out.write(false);
    }
    let result = guarded(|| {
        let Some(input) = state.input.as_mut() else {
            return Ok(());
        };
        let next = state.runtime.block_on(async {
            tokio::select! {
                biased;
                _ = state.cancel.cancelled() => Err(AbiError::new(ErrorCode::Cancelled, "UnionRead cancelled")),
                next = input.next() => next.transpose().map(Some),
                _ = async {
                    match timeout {
                        Some(duration) => tokio::time::sleep(duration).await,
                        None => futures::future::pending().await,
                    }
                } => Ok(None),
            }
        })?;
        // A foreign reader may block inside poll and return a batch after the
        // token was cancelled. Recheck before publishing that batch or EOF.
        if state.cancel.is_cancelled() {
            return Err(AbiError::new(ErrorCode::Cancelled, "UnionRead cancelled"));
        }
        let Some(next) = next else {
            unsafe {
                timed_out.write(true);
            }
            return Ok(());
        };
        match next {
            Some(batch) => {
                let array = StructArray::from(batch);
                unsafe { out.write(FFI_ArrowArray::new(&array.to_data())) };
            }
            None => {
                state.input = None;
            }
        }
        Ok(())
    });
    if result.is_err() {
        // Failure is terminal; release scanner resources before returning it.
        let _ = guarded(|| {
            state.input = None;
            Ok(())
        });
    }
    state.status(result)
}

unsafe extern "C" fn get_last_error(stream: *mut FFI_ArrowArrayStream) -> *const c_char {
    let state = unsafe { &*(*stream).private_data.cast::<Stream>() };
    state.error.as_ref().map_or(ptr::null(), |e| e.as_ptr())
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_stream_error_code(stream: *const FFI_ArrowArrayStream) -> c_int {
    let state = unsafe { &*(*stream).private_data.cast::<Stream>() };
    state.error_code
}

unsafe extern "C" fn release(stream: *mut FFI_ArrowArrayStream) {
    if stream.is_null() {
        return;
    }
    let stream = unsafe { &mut *stream };
    if stream.release.is_none() {
        return;
    }
    stream.release = None;
    let data = std::mem::replace(&mut stream.private_data, ptr::null_mut());
    let _ = guarded(|| {
        drop(unsafe { Box::from_raw(data.cast::<Stream>()) });
        Ok(())
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::datatypes::Schema;
    use fluss_lake::FlussLakeError;
    use std::ffi::CStr;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn runtime() -> Arc<Runtime> {
        Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap(),
        )
    }

    #[test]
    fn cancel_interrupts_blocked_poll_and_releases_input() {
        let (polled, wait) = std::sync::mpsc::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropFlag(dropped.clone());
        let input = Box::pin(futures::stream::once(async move {
            let _guard = guard;
            polled.send(()).unwrap();
            futures::future::pending::<Result<RecordBatch, FlussLakeError>>().await
        }));
        let cancel = CancellationToken::new();
        let mut stream = export(input, Arc::new(Schema::empty()), runtime(), cancel.clone());
        unsafe {
            let mut array = FFI_ArrowArray::empty();
            let mut timed_out = false;
            assert_eq!(
                fluss_union_stream_poll(&mut stream, &mut array, 1, &mut timed_out),
                0
            );
            assert!(timed_out);
            assert!(!dropped.load(Ordering::SeqCst));
        }
        let thread = std::thread::spawn(move || unsafe {
            let mut array = FFI_ArrowArray::empty();
            assert_ne!(stream.get_next.unwrap()(&mut stream, &mut array), 0);
            assert!(
                CStr::from_ptr(stream.get_last_error.unwrap()(&mut stream))
                    .to_str()
                    .unwrap()
                    .contains("cancelled")
            );
            assert_eq!(
                fluss_union_stream_error_code(&stream),
                ErrorCode::Cancelled as c_int
            );
            // The terminal stream must not restart polling.
            assert_eq!(stream.get_next.unwrap()(&mut stream, &mut array), 0);
            assert!(array.is_released());
        });
        wait.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        unsafe { crate::fluss_union_cancel(&cancel) };
        thread.join().unwrap();
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn cancellation_during_foreign_callback_discards_batch_and_releases_reader() {
        use arrow::array::{Int32Array, RecordBatchReader};
        use arrow::error::ArrowError;
        use arrow::ffi_stream::ArrowArrayStreamReader;
        use std::sync::mpsc;
        use std::time::Duration;

        struct BlockingReader {
            batch: RecordBatch,
            entered: mpsc::Sender<()>,
            resume: mpsc::Receiver<()>,
            _guard: DropFlag,
        }
        impl Iterator for BlockingReader {
            type Item = Result<RecordBatch, ArrowError>;
            fn next(&mut self) -> Option<Self::Item> {
                let result = self
                    .entered
                    .send(())
                    .map_err(|error| ArrowError::ComputeError(error.to_string()))
                    .and_then(|()| {
                        self.resume
                            .recv_timeout(Duration::from_secs(5))
                            .map_err(|error| ArrowError::ComputeError(error.to_string()))
                    })
                    .map(|()| self.batch.clone());
                Some(result)
            }
        }
        impl RecordBatchReader for BlockingReader {
            fn schema(&self) -> SchemaRef {
                self.batch.schema()
            }
        }

        let (entered, wait) = mpsc::channel();
        let (resume, gate) = mpsc::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let batch =
            RecordBatch::try_from_iter([("id", Arc::new(Int32Array::from(vec![1])) as _)]).unwrap();
        let schema = batch.schema();
        let foreign = FFI_ArrowArrayStream::new(Box::new(BlockingReader {
            batch,
            entered,
            resume: gate,
            _guard: DropFlag(dropped.clone()),
        }));
        let reader = ArrowArrayStreamReader::try_new(foreign).unwrap();
        let cancel = CancellationToken::new();
        let mut stream = export(
            futures::stream::iter(reader.map(|batch| batch.map_err(AbiError::internal))),
            schema,
            runtime(),
            cancel.clone(),
        );
        let reader_dropped = dropped.clone();
        let worker = std::thread::spawn(move || unsafe {
            let mut array = FFI_ArrowArray::empty();
            let mut timed_out = false;
            let status = fluss_union_stream_poll(&mut stream, &mut array, 1, &mut timed_out);
            (
                status,
                fluss_union_stream_error_code(&stream),
                array.is_released(),
                timed_out,
                reader_dropped.load(Ordering::SeqCst),
            )
        });
        let observed = wait.recv_timeout(Duration::from_secs(5));
        cancel.cancel();
        // The engine owns interruption of its blocking callback. Rust must not
        // release that reader until the callback has returned.
        let released_while_blocked = dropped.load(Ordering::SeqCst);
        resume.send(()).unwrap();
        let (status, code, empty, timed_out, released) = worker.join().unwrap();
        observed.unwrap();
        assert!(!released_while_blocked);
        assert_ne!(status, 0);
        assert_eq!(code, ErrorCode::Cancelled as c_int);
        assert!(empty && !timed_out);
        assert!(
            released,
            "terminal error must release input before stream release"
        );
    }

    #[test]
    fn source_failures_and_panics_do_not_cross_the_c_boundary() {
        for panic in [false, true] {
            let input = Box::pin(futures::stream::once(async move {
                if panic {
                    panic!("backend panic");
                }
                Err(FlussLakeError::DataUnavailable("expired\0snapshot".into()))
            }));
            let mut stream = export(
                input,
                Arc::new(Schema::empty()),
                runtime(),
                CancellationToken::new(),
            );
            unsafe {
                let mut array = FFI_ArrowArray::empty();
                assert_ne!(stream.get_next.unwrap()(&mut stream, &mut array), 0);
                let message = CStr::from_ptr(stream.get_last_error.unwrap()(&mut stream))
                    .to_str()
                    .unwrap();
                assert!(message.contains(if panic {
                    "panicked"
                } else {
                    "expired\\0snapshot"
                }));
                assert_eq!(
                    fluss_union_stream_error_code(&stream),
                    if panic {
                        ErrorCode::Internal as c_int
                    } else {
                        ErrorCode::DataUnavailable as c_int
                    }
                );
                assert_eq!(stream.get_next.unwrap()(&mut stream, &mut array), 0);
                assert!(array.is_released());
            }
        }
    }
}
