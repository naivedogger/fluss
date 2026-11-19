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

use crate::AbiError;
use fluss::client::ReadMemoryBudget;
use fluss::error::{Error, Result};
use std::ffi::c_void;

/// Moved from the engine at entry, including on open failure.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct MemoryBudget {
    pub data: *mut c_void,
    pub reserve: Option<unsafe extern "C" fn(*mut c_void, usize) -> bool>,
    pub release: Option<unsafe extern "C" fn(*mut c_void, usize)>,
    pub destroy: Option<unsafe extern "C" fn(*mut c_void)>,
}

// The C contract requires all callbacks and their state to be thread-safe.
unsafe impl Send for MemoryBudget {}
unsafe impl Sync for MemoryBudget {}

impl MemoryBudget {
    pub unsafe fn take(raw: *mut Self) -> std::result::Result<Option<Self>, AbiError> {
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return Ok(None);
        };
        let owned = std::mem::replace(
            raw,
            Self {
                data: std::ptr::null_mut(),
                reserve: None,
                release: None,
                destroy: None,
            },
        );
        if owned.reserve.is_none() || owned.release.is_none() || owned.destroy.is_none() {
            return Err(AbiError::invalid(
                "memory budget requires reserve, release and destroy callbacks",
            ));
        }
        Ok(Some(owned))
    }
}

impl ReadMemoryBudget for MemoryBudget {
    fn try_reserve(&self, bytes: usize) -> Result<()> {
        if unsafe { (self.reserve.unwrap())(self.data, bytes) } {
            Ok(())
        } else {
            Err(Error::BufferExhausted {
                message: format!("engine read memory budget rejected {bytes} bytes"),
            })
        }
    }
    fn release(&self, bytes: usize) {
        unsafe { (self.release.unwrap())(self.data, bytes) };
    }
}

impl Drop for MemoryBudget {
    fn drop(&mut self) {
        if let Some(destroy) = self.destroy {
            unsafe { destroy(self.data) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    #[derive(Default)]
    struct Account {
        used: AtomicUsize,
        destroyed: AtomicBool,
    }

    #[test]
    fn callback_budget_owns_state_and_preserves_failed_reservations() {
        let account = Arc::new(Account::default());
        let mut raw = MemoryBudget {
            data: Arc::into_raw(account.clone()).cast_mut().cast(),
            reserve: Some(reserve),
            release: Some(release),
            destroy: Some(destroy),
        };
        unsafe extern "C" fn reserve(data: *mut c_void, bytes: usize) -> bool {
            let account = unsafe { &*data.cast::<Account>() };
            account
                .used
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                    used.checked_add(bytes).filter(|sum| *sum <= 100)
                })
                .is_ok()
        }
        unsafe extern "C" fn release(data: *mut c_void, bytes: usize) {
            unsafe { &*data.cast::<Account>() }
                .used
                .fetch_sub(bytes, Ordering::SeqCst);
        }
        unsafe extern "C" fn destroy(data: *mut c_void) {
            let account = unsafe { Arc::from_raw(data.cast::<Account>()) };
            assert_eq!(account.used.load(Ordering::SeqCst), 0);
            assert!(!account.destroyed.swap(true, Ordering::SeqCst));
        }
        let budget = unsafe { MemoryBudget::take(&mut raw).unwrap().unwrap() };
        assert!(raw.destroy.is_none());
        budget.try_reserve(60).unwrap();
        assert!(matches!(
            budget.try_reserve(50),
            Err(Error::BufferExhausted { .. })
        ));
        assert_eq!(account.used.load(Ordering::SeqCst), 60);
        // The owner is independent of both the caller's stack and the scan thread.
        std::thread::spawn(move || {
            budget.release(60);
            drop(budget);
        })
        .join()
        .unwrap();
        assert!(account.destroyed.load(Ordering::SeqCst));
        drop(raw);
    }
}
