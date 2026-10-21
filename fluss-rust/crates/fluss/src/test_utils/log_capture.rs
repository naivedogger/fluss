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

use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Once;

use log::{Level, LevelFilter, Log, Metadata, Record};

struct Capture {
    level: LevelFilter,
    records: Vec<(Level, String)>,
}

thread_local! {
    static CAPTURE: RefCell<Option<Capture>> = const { RefCell::new(None) };
}

struct TestLogger;

impl Log for TestLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        CAPTURE.with(|capture| {
            capture
                .borrow()
                .as_ref()
                .is_some_and(|c| metadata.level() <= c.level)
        })
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            let message = record.args().to_string();
            CAPTURE.with(|capture| {
                capture
                    .borrow_mut()
                    .as_mut()
                    .unwrap()
                    .records
                    .push((record.level(), message));
            });
        }
    }

    fn flush(&self) {}
}

/// Captures logs on one test thread, including tasks on a current-thread Tokio runtime.
/// The guard cannot move between threads; other parallel tests remain isolated.
pub(crate) struct LogCapture(PhantomData<Rc<()>>);

impl LogCapture {
    pub(crate) fn new(level: LevelFilter) -> Self {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            log::set_logger(&TestLogger).expect("install test logger");
            log::set_max_level(LevelFilter::Trace);
        });
        CAPTURE.with(|capture| {
            let mut capture = capture.borrow_mut();
            assert!(capture.is_none(), "nested log capture");
            *capture = Some(Capture {
                level,
                records: Vec::new(),
            });
        });
        Self(PhantomData)
    }

    pub(crate) fn records(&self) -> Vec<(Level, String)> {
        CAPTURE.with(|capture| capture.borrow().as_ref().unwrap().records.clone())
    }

    pub(crate) fn assert_contains(&self, level: Level, fragments: &[&str]) {
        let records = self.records();
        assert!(
            records.iter().any(|(l, message)| {
                *l == level && fragments.iter().all(|fragment| message.contains(fragment))
            }),
            "missing {level} log containing {fragments:?}; captured {records:?}"
        );
    }
}

impl Drop for LogCapture {
    fn drop(&mut self) {
        CAPTURE.with(|capture| *capture.borrow_mut() = None);
    }
}
