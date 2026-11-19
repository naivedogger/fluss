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

//! Experimental, process-local C ABI for the default bounded UnionRead executor.
//! The C header defines ownership and threading requirements. This is not a task wire protocol.

mod append;
#[cfg(feature = "paimon")]
mod baseline;
mod bucket;
mod error;
mod memory;
mod stream;

use error::{AbiError, ErrorCode};

use arrow::ffi_stream::FFI_ArrowArrayStream;
use fluss::{client::FlussConnection, config::Config, metadata::TablePath};
use fluss_lake::{FlussLakeReadPlan, FlussLakeReader, FlussLakeTable};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

struct Client {
    connection: Arc<FlussConnection>,
    runtime: Arc<Runtime>,
}

struct Plan {
    plan: FlussLakeReadPlan,
    reader: FlussLakeReader,
    runtime: Arc<Runtime>,
}

#[repr(C)]
struct Property {
    key: *const c_char,
    value: *const c_char,
}

fn guarded<T>(f: impl FnOnce() -> Result<T, AbiError>) -> Result<T, AbiError> {
    catch_unwind(AssertUnwindSafe(f))
        .unwrap_or_else(|_| Err(AbiError::internal("UnionRead panicked")))
}

fn c_string(message: String) -> CString {
    CString::new(message.replace('\0', "\\0")).expect("NUL bytes escaped")
}

// All strings are borrowed for the duration of the ABI call only.
unsafe fn text(value: *const c_char) -> Result<String, AbiError> {
    if value.is_null() {
        return Err(AbiError::invalid("required string is null"));
    }
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .map(str::to_owned)
        .map_err(AbiError::invalid)
}

unsafe fn status(error: *mut *mut c_char, f: impl FnOnce() -> Result<(), AbiError>) -> c_int {
    if !error.is_null() {
        unsafe { error.write(ptr::null_mut()) };
    }
    match guarded(f) {
        Ok(()) => 0,
        Err(failure) => {
            if !error.is_null() {
                unsafe { error.write(c_string(failure.message).into_raw()) };
            }
            failure.code
        }
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_client_create(
    bootstrap: *const c_char,
    out: *mut *mut Client,
    error: *mut *mut c_char,
) -> c_int {
    unsafe { fluss_union_client_create_cancellable(bootstrap, ptr::null(), out, error) }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_client_create_cancellable(
    bootstrap: *const c_char,
    operation_cancel: *const CancellationToken,
    out: *mut *mut Client,
    error: *mut *mut c_char,
) -> c_int {
    unsafe { create_client(bootstrap, None, operation_cancel, out, error) }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_client_create_sasl(
    bootstrap: *const c_char,
    username: *const c_char,
    password: *const c_char,
    operation_cancel: *const CancellationToken,
    out: *mut *mut Client,
    error: *mut *mut c_char,
) -> c_int {
    unsafe {
        create_client(
            bootstrap,
            Some((username, password)),
            operation_cancel,
            out,
            error,
        )
    }
}

unsafe fn create_client(
    bootstrap: *const c_char,
    credentials: Option<(*const c_char, *const c_char)>,
    operation_cancel: *const CancellationToken,
    out: *mut *mut Client,
    error: *mut *mut c_char,
) -> c_int {
    unsafe {
        status(error, || {
            if out.is_null() {
                return Err(AbiError::invalid("client output is null"));
            }
            out.write(ptr::null_mut());
            let mut config = Config {
                bootstrap_servers: text(bootstrap)?,
                ..Default::default()
            };
            if let Some((username, password)) = credentials {
                config.security_protocol = "SASL".into();
                config.security_sasl_username = text(username)?;
                config.security_sasl_password = text(password)?;
            }
            let runtime = Arc::new(
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(AbiError::internal)?,
            );
            let token = child_cancel(operation_cancel);
            let connection = Arc::new(wait_for(&runtime, &token, async {
                FlussConnection::new(config).await.map_err(AbiError::from)
            })?);
            out.write(Box::into_raw(Box::new(Client {
                connection,
                runtime,
            })));
            Ok(())
        })
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_plan_create(
    client: *const Client,
    database: *const c_char,
    table: *const c_char,
    properties: *const Property,
    property_count: usize,
    out: *mut *mut Plan,
    error: *mut *mut c_char,
) -> c_int {
    unsafe {
        fluss_union_plan_create_cancellable(
            client,
            database,
            table,
            properties,
            property_count,
            ptr::null(),
            out,
            error,
        )
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_plan_create_cancellable(
    client: *const Client,
    database: *const c_char,
    table: *const c_char,
    properties: *const Property,
    property_count: usize,
    operation_cancel: *const CancellationToken,
    out: *mut *mut Plan,
    error: *mut *mut c_char,
) -> c_int {
    unsafe {
        status(error, || {
            if out.is_null() {
                return Err(AbiError::invalid("plan output is null"));
            }
            out.write(ptr::null_mut());
            let client = client
                .as_ref()
                .ok_or_else(|| AbiError::invalid("client is null"))?;
            let path = TablePath::new(text(database)?, text(table)?);
            let mut overrides = HashMap::new();
            if property_count != 0 {
                if properties.is_null() {
                    return Err(AbiError::invalid("properties are null"));
                }
                for property in std::slice::from_raw_parts(properties, property_count) {
                    overrides.insert(text(property.key)?, text(property.value)?);
                }
            }
            let table = FlussLakeTable::new(client.connection.clone(), &path)
                .with_catalog_properties(overrides);
            let scan = table.new_scan();
            let token = child_cancel(operation_cancel);
            let plan = wait_for(&client.runtime, &token, async {
                scan.plan().await.map_err(AbiError::from)
            })?;
            out.write(Box::into_raw(Box::new(Plan {
                plan,
                reader: scan.new_reader(),
                runtime: client.runtime.clone(),
            })));
            Ok(())
        })
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_plan_split_count(plan: *const Plan) -> usize {
    unsafe { (*plan).plan.split_count() }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_plan_open(
    plan: *const Plan,
    first: usize,
    count: usize,
    out: *mut FFI_ArrowArrayStream,
    cancel: *mut *mut CancellationToken,
    error: *mut *mut c_char,
) -> c_int {
    unsafe {
        status(error, || {
            if out.is_null() || cancel.is_null() {
                return Err(AbiError::invalid("stream output is null"));
            }
            out.write(FFI_ArrowArrayStream::empty());
            cancel.write(ptr::null_mut());
            let plan = plan
                .as_ref()
                .ok_or_else(|| AbiError::invalid("plan is null"))?;
            let end = first
                .checked_add(count)
                .ok_or_else(|| AbiError::invalid("split range overflow"))?;
            let splits = plan
                .plan
                .splits()
                .get(first..end)
                .ok_or_else(|| AbiError::invalid("split range out of bounds"))?;
            let input = plan.reader.read_splits(splits).map_err(AbiError::from)?;
            let token = CancellationToken::new();
            let output = stream::export(
                input,
                plan.plan.schema(),
                plan.runtime.clone(),
                token.clone(),
            );
            cancel.write(Box::into_raw(Box::new(token)));
            out.write(output);
            Ok(())
        })
    }
}

// A returned stream gets its own child: cancelling one stream must not cancel
// other opens sharing the engine's query token.
unsafe fn child_cancel(parent: *const CancellationToken) -> CancellationToken {
    unsafe { parent.as_ref() }
        .map(CancellationToken::child_token)
        .unwrap_or_default()
}

fn wait_for<T>(
    runtime: &Runtime,
    cancel: &CancellationToken,
    operation: impl std::future::Future<Output = Result<T, AbiError>>,
) -> Result<T, AbiError> {
    runtime.block_on(async {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(AbiError::new(ErrorCode::Cancelled, "UnionRead initialization cancelled")),
            result = operation => result,
        }
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_cancel_create(
    out: *mut *mut CancellationToken,
    error: *mut *mut c_char,
) -> c_int {
    unsafe {
        status(error, || {
            if out.is_null() {
                return Err(AbiError::invalid("cancel output is null"));
            }
            out.write(ptr::null_mut());
            out.write(Box::into_raw(Box::new(CancellationToken::new())));
            Ok(())
        })
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_cancel(cancel: *const CancellationToken) {
    if let Some(cancel) = unsafe { cancel.as_ref() } {
        cancel.cancel();
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_cancel_release(cancel: *mut CancellationToken) {
    if !cancel.is_null() {
        drop(unsafe { Box::from_raw(cancel) });
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_plan_release(plan: *mut Plan) {
    if !plan.is_null() {
        let _ = guarded(|| {
            drop(unsafe { Box::from_raw(plan) });
            Ok(())
        });
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_client_release(client: *mut Client) {
    if !client.is_null() {
        let _ = guarded(|| {
            drop(unsafe { Box::from_raw(client) });
            Ok(())
        });
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_error_release(error: *mut c_char) {
    if !error.is_null() {
        drop(unsafe { CString::from_raw(error) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluss::error::{ApiError, Error, FlussError};
    use fluss_lake::FlussLakeError;

    #[test]
    fn cancel_interrupts_client_bootstrap_before_a_handle_exists() {
        use std::sync::mpsc;
        use std::time::Duration;
        use tokio::io::AsyncReadExt;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let bootstrap = CString::new(listener.local_addr().unwrap().to_string()).unwrap();
        let mut cancel = ptr::null_mut();
        assert_eq!(
            unsafe { fluss_union_cancel_create(&mut cancel, ptr::null_mut()) },
            0
        );
        // The main thread retains the token until the borrowing call has returned.
        let address = cancel as usize;
        let (done, result) = mpsc::channel();
        let worker = std::thread::spawn(move || unsafe {
            let mut client = ptr::null_mut();
            let mut error = ptr::null_mut();
            let code = fluss_union_client_create_cancellable(
                bootstrap.as_ptr(),
                address as *const CancellationToken,
                &mut client,
                &mut error,
            );
            let output_cleared = client.is_null();
            let message =
                (!error.is_null()).then(|| CStr::from_ptr(error).to_string_lossy().into_owned());
            fluss_union_error_release(error);
            fluss_union_client_release(client);
            done.send((code, output_cleared, message)).unwrap();
        });
        // Keep the socket open without replying: EOF must not cause the return.
        let _socket = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                let (mut socket, _) = listener.accept().await.unwrap();
                let size = socket.read_u32().await.unwrap() as usize;
                assert!((8..4096).contains(&size));
                socket.read_exact(&mut vec![0; size]).await.unwrap();
                socket
            })
            .await
            .unwrap()
        });
        unsafe { fluss_union_cancel(cancel) };
        let (code, cleared, message) = result.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        unsafe { fluss_union_cancel_release(cancel) };
        assert_eq!(code, ErrorCode::Cancelled as c_int);
        assert!(cleared);
        assert!(message.unwrap().contains("cancelled"));
    }

    #[test]
    fn initialization_cancel_drops_pending_work_without_cancelling_siblings() {
        use std::sync::mpsc;
        use std::time::Duration;

        let parent = CancellationToken::new();
        let child = unsafe { child_cancel(&parent) };
        let sibling = unsafe { child_cancel(&parent) };
        let resource = Arc::new(());
        let released = Arc::downgrade(&resource);
        let (started, waiting) = mpsc::channel();
        let (done, result) = mpsc::channel();
        let token = child.clone();
        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let outcome = wait_for(&runtime, &token, async move {
                let _resource = resource;
                started.send(()).unwrap();
                futures::future::pending::<Result<(), AbiError>>().await
            });
            done.send(outcome.unwrap_err().code).unwrap();
        });
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        unsafe { fluss_union_cancel(&child) };
        assert_eq!(
            result.recv_timeout(Duration::from_secs(5)).unwrap(),
            ErrorCode::Cancelled as c_int
        );
        worker.join().unwrap();
        assert!(released.upgrade().is_none());
        assert!(!parent.is_cancelled());
        assert!(!sibling.is_cancelled());
        unsafe { fluss_union_cancel(&parent) };
        assert!(sibling.is_cancelled());
    }

    #[test]
    fn abi_status_preserves_codes_and_owned_messages() {
        let api = FlussError::TableNotExist.code();
        for (failure, code) in [
            (
                AbiError::from(Error::from(ApiError {
                    code: api,
                    message: "missing table".into(),
                })),
                api,
            ),
            (
                AbiError::from(Error::IllegalArgument {
                    message: "invalid core option".into(),
                }),
                ErrorCode::Client as c_int,
            ),
            (
                AbiError::from(FlussLakeError::SchemaIncompatible("changed schema".into())),
                ErrorCode::SchemaIncompatible as c_int,
            ),
            (
                AbiError::from(FlussLakeError::MemoryLimitExceeded("overlay budget".into())),
                ErrorCode::MemoryLimitExceeded as c_int,
            ),
        ] {
            let expected = failure.message.clone();
            let mut message = ptr::null_mut();
            unsafe {
                assert_eq!(status(&mut message, || Err(failure)), code);
                assert_eq!(CStr::from_ptr(message).to_str().unwrap(), expected);
                fluss_union_error_release(message);
            }
        }
        unsafe {
            let mut client = ptr::null_mut();
            let mut message = ptr::null_mut();
            assert_eq!(
                fluss_union_client_create(ptr::null(), &mut client, &mut message),
                ErrorCode::InvalidArgument as c_int
            );
            assert!(client.is_null());
            fluss_union_error_release(message);
            assert_eq!(status(ptr::null_mut(), || Ok(())), 0);
        }
    }
}
