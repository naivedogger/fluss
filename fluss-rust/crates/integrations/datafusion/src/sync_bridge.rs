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

//! Sync<->async bridge for DataFusion's synchronous catalog callbacks.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::{Handle, Runtime};

pub(crate) const ACCESS_PANIC: &str = "fluss catalog access thread panicked";

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

fn global_runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        Runtime::new()
            .expect("failed to build global tokio runtime for fluss datafusion integration")
    })
}

pub(crate) fn block_on_with_runtime<F>(future: F, panic_error: &'static str) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    if Handle::try_current().is_ok() {
        let (tx, rx) = std::sync::mpsc::channel();
        global_runtime().spawn(async move {
            let out = future.await;
            let _ = tx.send(out);
        });
        rx.recv().expect(panic_error)
    } else {
        global_runtime().block_on(future)
    }
}
