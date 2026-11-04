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

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

struct DiagnosticLogger;

static LOGGER: DiagnosticLogger = DiagnosticLogger;

impl log::Log for DiagnosticLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
            || (metadata.level() <= log::Level::Debug
                && (metadata.target().starts_with("fluss::client::write::")
                    || metadata.target() == "fluss::client::write"
                    || metadata.target().starts_with("fluss::client::metadata::")
                    || metadata.target() == "fluss::client::metadata"))
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        // stderr is captured by the native harness; stdout carries its JSON protocol.
        // A failed diagnostic write must not panic on an SDK worker thread.
        let _ = writeln!(
            std::io::stderr().lock(),
            "{timestamp} {:?} [{}] {}: {}",
            std::thread::current().id(),
            record.level(),
            record.target(),
            record.args()
        );
    }

    fn flush(&self) {}
}

pub(crate) fn init() {
    // Enabled by default only in this diagnostic branch. Respect an existing
    // process-wide logger and its filter if the application installed one first.
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
        log::warn!("C++ diagnostic Rust logger enabled; write/metadata=debug, other=warn");
    }
}
