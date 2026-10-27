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

//! Opt-in, process-wide logging for applications without an existing Rust logger.
//!
//! Events are formatted on the caller and written to rotating files by a dedicated thread.
//! A full or contended queue drops events instead of delaying SDK requests. Inspect
//! [`LogHandle::stats`] before interpreting an absence of errors. No environment
//! variables are read, and nothing is installed automatically.

//! # Example
//!
//! ```no_run
//! use fluss::logging::{init_file_logging, FileLogOptions};
//! use std::time::Duration;
//!
//! # fn main() -> std::io::Result<()> {
//! let logger = init_file_logging(FileLogOptions::default())?;
//! // Create connections and run the workload; close connections before shutdown.
//! logger.shutdown(Duration::from_secs(5))?;
//! let stats = logger.stats();
//! assert_eq!((stats.dropped, stats.truncated, stats.write_errors), (0, 0, 0));
//! # Ok(())
//! # }
//! ```

use log::{LevelFilter, Log, Metadata, Record};
use std::fmt::{self, Write as _};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_RECORD_BYTES: usize = 8192;

/// Configuration for the optional rotating file logger.
#[derive(Clone, Debug)]
pub struct FileLogOptions {
    /// Parent directory. Each initialization creates a unique process/run subdirectory.
    /// Retention applies within that subdirectory; older runs are application-managed.
    pub directory: PathBuf,
    /// Rotate after this size is exceeded (at least 8192 bytes). A file may exceed
    /// the limit by one event, at most 8192 bytes.
    pub max_file_size: u64,
    /// Number of archived files to retain, excluding the active file (1..=1000).
    pub max_files: u32,
    /// Comma-separated levels and module overrides, for example
    /// `warn,fluss::client::write::idempotence=debug`. Longest module prefix wins.
    /// Only targets named `fluss` or starting with `fluss::` are collected.
    pub filter: String,
    /// Maximum queued messages (1..=65536). Each event is capped at 8192 bytes.
    pub queue_capacity: usize,
}

impl Default for FileLogOptions {
    fn default() -> Self {
        Self {
            directory: PathBuf::from("logs"),
            max_file_size: 64 * 1024 * 1024,
            max_files: 4,
            filter: "warn".into(),
            queue_capacity: 1024,
        }
    }
}

/// Cumulative counters; concurrent snapshots may span more than one event.
#[derive(Clone, Copy, Debug, Default)]
pub struct LogStats {
    /// Events accepted by the output queue.
    pub accepted: u64,
    /// Events whose write to the file appender succeeded (not a durability guarantee).
    pub written: u64,
    /// Events rejected because the queue was full, contended, or disconnected.
    pub dropped: u64,
    /// Events shortened to the per-event byte limit before enqueueing.
    pub truncated: u64,
    /// Failed output writes or flushes.
    pub write_errors: u64,
}

#[derive(Default)]
struct Counters {
    accepted: AtomicU64,
    written: AtomicU64,
    dropped: AtomicU64,
    truncated: AtomicU64,
    write_errors: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> LogStats {
        LogStats {
            accepted: self.accepted.load(Ordering::Relaxed),
            written: self.written.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            truncated: self.truncated.load(Ordering::Relaxed),
            write_errors: self.write_errors.load(Ordering::Relaxed),
        }
    }
}

struct Filter(Vec<(String, LevelFilter)>);

impl Filter {
    fn parse(input: &str) -> io::Result<Self> {
        if input.len() > 4096 {
            return Err(invalid("logging filter exceeds 4096 bytes"));
        }
        let mut rules = vec![(String::new(), LevelFilter::Warn)];
        for directive in input.split(',') {
            let directive = directive.trim();
            if directive.is_empty() {
                return Err(invalid("empty logging directive"));
            }
            let (target, level) = directive.split_once('=').unwrap_or(("", directive));
            let target = target.trim();
            if !target.is_empty()
                && (!is_fluss(target)
                    || target
                        .chars()
                        .any(|c| !c.is_ascii_alphanumeric() && c != '_' && c != ':'))
            {
                return Err(invalid("logging overrides must name a fluss module"));
            }
            let level = level
                .trim()
                .parse()
                .map_err(|_| invalid("invalid logging level"))?;
            rules.retain(|(prefix, _)| prefix != target);
            rules.push((target.to_owned(), level));
        }
        Ok(Self(rules))
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        let target = metadata.target();
        if !is_fluss(target) {
            return false;
        }
        let mut selected = LevelFilter::Warn;
        let mut length = 0;
        for (prefix, level) in &self.0 {
            if prefix.len() >= length
                && (prefix.is_empty()
                    || target == prefix
                    || target
                        .strip_prefix(prefix)
                        .is_some_and(|suffix| suffix.starts_with("::")))
            {
                selected = *level;
                length = prefix.len();
            }
        }
        metadata.level() <= selected
    }

    fn max_level(&self) -> LevelFilter {
        self.0
            .iter()
            .map(|(_, level)| *level)
            .max()
            .unwrap_or(LevelFilter::Off)
    }
}

fn is_fluss(target: &str) -> bool {
    target == "fluss" || target.starts_with("fluss::")
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

struct Line {
    text: String,
    truncated: bool,
}
impl fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let remaining = MAX_RECORD_BYTES - 1 - self.text.len();
        let mut end = text.len().min(remaining);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&text[..end]);
        if end < text.len() {
            self.truncated = true;
            return Err(fmt::Error);
        }
        Ok(())
    }
}

enum Message {
    Event(String),
    Flush(mpsc::SyncSender<()>),
}
type Completion = Arc<(Mutex<bool>, Condvar)>;

struct Backend {
    filter: Filter,
    sender: Mutex<Option<mpsc::SyncSender<Message>>>,
    active: AtomicBool,
    counters: Arc<Counters>,
    done: Completion,
}

impl Backend {
    fn stop(&self) {
        self.active.store(false, Ordering::Release);
        // Taking the sole sender orders closure after all accepted events. The worker
        // drains the receiver before flushing; no stop message needs a spare queue slot.
        self.sender.lock().unwrap().take();
    }

    fn flush(&self, timeout: Duration) -> io::Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        let sender = self
            .sender
            .try_lock()
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "logger queue is contended"))?;
        let tx_queue = sender
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "logger is shut down"))?;
        tx_queue.try_send(Message::Flush(tx)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "logger queue is full or disconnected",
            )
        })?;
        drop(sender);
        rx.recv_timeout(timeout).map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "logger flush did not complete")
        })?;
        self.check_output()
    }

    fn check_output(&self) -> io::Result<()> {
        if self.counters.write_errors.load(Ordering::Relaxed) > 0 {
            Err(io::Error::other("logger output failed; inspect LogStats"))
        } else {
            Ok(())
        }
    }
}

struct InstalledLogger(Arc<Backend>);
impl Log for InstalledLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.0.active.load(Ordering::Acquire) && self.0.filter.enabled(metadata)
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let mut line = Line {
            text: String::with_capacity(256),
            truncated: false,
        };
        let _ = write!(
            line,
            "{time} {} pid={} thread={:?} {}: {}",
            record.level(),
            std::process::id(),
            std::thread::current().id(),
            record.target(),
            record.args()
        );
        if line.truncated {
            self.0.counters.truncated.fetch_add(1, Ordering::Relaxed);
        }
        line.text.push('\n');
        // Do not wait behind another producer or a flush/shutdown caller.
        let accepted = self
            .0
            .sender
            .try_lock()
            .ok()
            .and_then(|sender| {
                sender
                    .as_ref()
                    .map(|tx| tx.try_send(Message::Event(line.text)).is_ok())
            })
            .unwrap_or(false);
        if accepted {
            self.0.counters.accepted.fetch_add(1, Ordering::Relaxed);
        } else {
            self.0.counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn flush(&self) {
        let _ = self.0.flush(Duration::from_secs(1));
    }
}

/// Owns an installed logger's output lifetime; thread-safe but not cloneable.
///
/// Drop stops accepting events without waiting for I/O. Call [`Self::shutdown`]
/// after closing SDK connections to wait for drain/flush and inspect the result.
/// A blocked OS write cannot be cancelled: a timed-out worker may remain alive.
/// The Rust global logger cannot be uninstalled or replaced, even after shutdown.
pub struct LogHandle {
    path: PathBuf,
    backend: Arc<Backend>,
}

impl LogHandle {
    /// Active file path; archives are alongside it. A just-rotated active file may
    /// not exist until the next event. Only this run's archives are expired.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns cumulative counters, including queue drops and output errors.
    pub fn stats(&self) -> LogStats {
        self.backend.counters.snapshot()
    }

    /// Flushes events accepted before this call. Concurrent producers may continue.
    /// Returns WouldBlock if the control message cannot enter the queue, TimedOut
    /// if output stalls, or an output error. This does not stop the logger.
    pub fn flush(&self, timeout: Duration) -> io::Result<()> {
        self.backend.flush(timeout)
    }

    /// Stops accepting events and waits up to `timeout` for drain and flush.
    /// Safe to repeat, including after a timeout. Returns output failures; callers
    /// must separately check drops/truncation before treating logs as complete.
    pub fn shutdown(&self, timeout: Duration) -> io::Result<()> {
        self.backend.stop();
        let (lock, cv) = &*self.backend.done;
        let (finished, _) = cv
            .wait_timeout_while(lock.lock().unwrap(), timeout, |done| !*done)
            .unwrap();
        if !*finished {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "logger shutdown did not complete",
            ));
        }
        self.backend.check_output()
    }
}

impl Drop for LogHandle {
    fn drop(&mut self) {
        self.backend.stop();
    }
}

fn start(
    options: FileLogOptions,
    mut output: impl Write + Send + 'static,
) -> io::Result<LogHandle> {
    if options.queue_capacity == 0 || options.queue_capacity > 65536 {
        return Err(invalid("logger queue capacity must be 1..=65536"));
    }
    let filter = Filter::parse(&options.filter)?;
    let (tx, rx) = mpsc::sync_channel(options.queue_capacity);
    let counters = Arc::new(Counters::default());
    let done = Arc::new((Mutex::new(false), Condvar::new()));
    let backend = Arc::new(Backend {
        filter,
        sender: Mutex::new(Some(tx)),
        active: AtomicBool::new(true),
        counters: counters.clone(),
        done: done.clone(),
    });
    std::thread::Builder::new()
        .name("fluss-log-output".into())
        .spawn(move || {
            for message in rx {
                let (result, ack) = match message {
                    Message::Event(line) => {
                        let result = output.write_all(line.as_bytes());
                        if result.is_ok() {
                            counters.written.fetch_add(1, Ordering::Relaxed);
                        }
                        (result, None)
                    }
                    Message::Flush(ack) => (output.flush(), Some(ack)),
                };
                if result.is_err() {
                    counters.write_errors.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
            }
            if output.flush().is_err() {
                counters.write_errors.fetch_add(1, Ordering::Relaxed);
            }
            drop(output);
            let (lock, cv) = &*done;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        })?;
    Ok(LogHandle {
        path: PathBuf::new(),
        backend,
    })
}

#[cfg(feature = "file-logging")]
fn file_output(options: &FileLogOptions) -> io::Result<(Box<dyn Write + Send>, PathBuf)> {
    use log4rs::append::Append;
    use log4rs::append::rolling_file::RollingFileAppender;
    use log4rs::append::rolling_file::policy::compound::{
        CompoundPolicy, roll::fixed_window::FixedWindowRoller, trigger::size::SizeTrigger,
    };
    use log4rs::encode::pattern::PatternEncoder;

    struct Output(RollingFileAppender);
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let text = std::str::from_utf8(bytes).map_err(io::Error::other)?;
            self.0
                .append(
                    &Record::builder()
                        .args(format_args!("{text}"))
                        .level(log::Level::Info)
                        .target("fluss")
                        .build(),
                )
                .map_err(io::Error::other)?;
            Ok(bytes.len())
        }
        // RollingFileAppender flushes its BufWriter during every append and
        // propagates that error. Its flush() is a no-op, not an fsync guarantee.
        fn flush(&mut self) -> io::Result<()> {
            self.0.flush();
            Ok(())
        }
    }

    if options.directory.as_os_str().is_empty()
        || options.max_file_size < MAX_RECORD_BYTES as u64
        || !(1..=1000).contains(&options.max_files)
    {
        return Err(invalid(
            "nonempty directory, max_file_size >= 8192 and max_files in 1..=1000 required",
        ));
    }
    std::fs::create_dir_all(&options.directory)?;
    let parent = options.directory.canonicalize()?;
    let parent_text = parent
        .to_str()
        .ok_or_else(|| invalid("log directory must be UTF-8"))?;
    if parent_text.contains("{}") || parent_text.contains("$ENV{") {
        return Err(invalid("log directory contains log4rs pattern syntax"));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let directory = parent.join(format!("fluss-{}-{now}", std::process::id()));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    // create_dir rather than create_dir_all reserves the file group exclusively.
    builder.create(&directory)?;
    let path = directory.join("client.log");
    let pattern = directory.join("client.{}.log");
    let roller = FixedWindowRoller::builder()
        .build(pattern.to_str().unwrap(), options.max_files)
        .map_err(io::Error::other)?;
    let policy = CompoundPolicy::new(
        Box::new(SizeTrigger::new(options.max_file_size)),
        Box::new(roller),
    );
    let appender = RollingFileAppender::builder()
        .encoder(Box::new(PatternEncoder::new("{m}")))
        .build(&path, Box::new(policy))?;
    Ok((Box::new(Output(appender)), path))
}

#[cfg(not(feature = "file-logging"))]
fn file_output(_: &FileLogOptions) -> io::Result<(Box<dyn Write + Send>, PathBuf)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "enable the file-logging Cargo feature",
    ))
}

/// Installs an asynchronous rotating file logger explicitly, once per process.
///
/// Requires the optional `file-logging` feature; otherwise returns Unsupported.
/// Call before creating SDK connections and retain the handle until they close.
/// Existing Rust loggers are never replaced: AlreadyExists is returned instead.
/// Initialization may create an empty file group before detecting a foreign logger.
/// Filters/capacities return InvalidInput; file/worker setup can return I/O errors.
/// Does not read RUST_LOG or change SDK configuration defaults. Lines contain
/// event-time Unix milliseconds, level, PID, thread ID and target. File writes,
/// rotation and expiration all run on the output worker, not SDK request threads.
/// Retention is per run, not a limit on the parent directory or older runs.
pub fn init_file_logging(options: FileLogOptions) -> io::Result<LogHandle> {
    // Serialize our own initialization attempts without replacing a foreign logger.
    static INSTALLED: Mutex<bool> = Mutex::new(false);
    let mut installed = INSTALLED.lock().unwrap();
    if *installed {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a process-wide Rust logger is already installed",
        ));
    }
    Filter::parse(&options.filter)?;
    if options.queue_capacity == 0 || options.queue_capacity > 65536 {
        return Err(invalid("logger queue capacity must be 1..=65536"));
    }
    let (output, path) = file_output(&options)?;
    let mut handle = start(options, output)?;
    handle.path = path;
    let level = handle.backend.filter.max_level();
    log::set_boxed_logger(Box::new(InstalledLogger(handle.backend.clone()))).map_err(|_| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a process-wide Rust logger is already installed",
        )
    })?;
    log::set_max_level(level);
    *installed = true;
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Level;

    fn emit(logger: &InstalledLogger, target: &str, level: Level, message: &str) {
        logger.log(
            &Record::builder()
                .target(target)
                .level(level)
                .args(format_args!("{message}"))
                .build(),
        );
    }

    #[test]
    fn filter_matches_module_boundaries_and_last_override() {
        let filter =
            Filter::parse("off,fluss::client=debug,fluss::client::write=error,fluss::client=info")
                .unwrap();
        let enabled = |target, level| {
            filter.enabled(&Metadata::builder().target(target).level(level).build())
        };
        assert!(enabled("fluss::client::read", Level::Info));
        assert!(!enabled("fluss::client::read", Level::Debug));
        assert!(!enabled("fluss::client::write", Level::Warn));
        assert!(enabled("fluss::client::write", Level::Error));
        assert!(!enabled("fluss::client_extra", Level::Error));
        assert!(!enabled("another_crate", Level::Error));
        assert_eq!(Filter::parse("off").unwrap().max_level(), LevelFilter::Off);
        for value in ["", "warn,", "verbose", "other=info", "fluss x=debug"] {
            assert!(Filter::parse(value).is_err());
        }
    }

    #[derive(Clone, Default)]
    struct Bytes(Arc<Mutex<Vec<u8>>>);
    impl Write for Bytes {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writes_filtered_events_and_caps_utf8_before_shutdown() {
        let bytes = Bytes::default();
        let handle = start(FileLogOptions::default(), bytes.clone()).unwrap();
        let logger = InstalledLogger(handle.backend.clone());
        emit(&logger, "fluss::client", Level::Info, "filtered");
        emit(
            &logger,
            "fluss::client",
            Level::Warn,
            "OutOfOrderSequenceException sequence=7",
        );
        emit(&logger, "fluss::client", Level::Error, &"汉".repeat(10000));
        handle.flush(Duration::from_secs(2)).unwrap();
        handle.shutdown(Duration::from_secs(2)).unwrap();
        handle.shutdown(Duration::ZERO).unwrap();
        emit(&logger, "fluss::client", Level::Error, "after shutdown");
        let output = String::from_utf8(bytes.0.lock().unwrap().clone()).unwrap();
        assert!(output.contains("OutOfOrderSequenceException sequence=7"));
        assert!(!output.contains("filtered") && !output.contains("after shutdown"));
        assert!(output.lines().all(|line| line.len() < MAX_RECORD_BYTES));
        let stats = handle.stats();
        assert_eq!(
            (
                stats.accepted,
                stats.written,
                stats.dropped,
                stats.truncated
            ),
            (2, 2, 0, 1)
        );
    }

    #[test]
    fn full_queue_and_shutdown_do_not_wait_for_blocked_output() {
        struct Blocked {
            started: mpsc::SyncSender<()>,
            release: Option<mpsc::Receiver<()>>,
        }
        impl Write for Blocked {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if let Some(release) = self.release.take() {
                    self.started.send(()).unwrap();
                    release.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (started, ready) = mpsc::sync_channel(1);
        let (release, rx) = mpsc::channel();
        let handle = start(
            FileLogOptions {
                queue_capacity: 1,
                ..Default::default()
            },
            Blocked {
                started,
                release: Some(rx),
            },
        )
        .unwrap();
        let logger = InstalledLogger(handle.backend.clone());
        emit(&logger, "fluss", Level::Warn, "first");
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        emit(&logger, "fluss", Level::Warn, "queued");
        emit(&logger, "fluss", Level::Warn, "dropped");
        assert_eq!(handle.stats().dropped, 1);
        assert_eq!(
            handle.flush(Duration::ZERO).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            handle.shutdown(Duration::ZERO).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        release.send(()).unwrap();
        handle.shutdown(Duration::from_secs(2)).unwrap();
        assert_eq!(handle.stats().written, 2);
    }

    #[test]
    fn output_failures_and_invalid_capacity_are_observable() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("disk failure"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("flush failure"))
            }
        }
        assert!(
            start(
                FileLogOptions {
                    queue_capacity: 0,
                    ..Default::default()
                },
                Broken
            )
            .is_err()
        );
        let handle = start(FileLogOptions::default(), Broken).unwrap();
        emit(
            &InstalledLogger(handle.backend.clone()),
            "fluss",
            Level::Warn,
            "error",
        );
        assert!(handle.shutdown(Duration::from_secs(2)).is_err());
        assert_eq!(handle.stats().write_errors, 2);
        assert_eq!(handle.stats().written, 0);
    }

    #[cfg(feature = "file-logging")]
    #[test]
    fn global_logger_and_rotation_in_isolated_processes() {
        if let Ok(mode) = std::env::var("FLUSS_LOG_TEST_MODE") {
            let directory = PathBuf::from(std::env::var_os("FLUSS_LOG_TEST_DIRECTORY").unwrap());
            let options = FileLogOptions {
                directory,
                max_file_size: 8192,
                max_files: 2,
                queue_capacity: 128,
                ..Default::default()
            };
            if mode == "foreign" {
                struct Foreign;
                impl Log for Foreign {
                    fn enabled(&self, _: &Metadata<'_>) -> bool {
                        true
                    }
                    fn log(&self, _: &Record<'_>) {}
                    fn flush(&self) {}
                }
                static FOREIGN: Foreign = Foreign;
                log::set_logger(&FOREIGN).unwrap();
                log::set_max_level(LevelFilter::Trace);
                assert_eq!(
                    init_file_logging(options).err().unwrap().kind(),
                    io::ErrorKind::AlreadyExists
                );
                assert_eq!(log::max_level(), LevelFilter::Trace);
                return;
            }
            let handle = init_file_logging(options.clone()).unwrap();
            assert!(init_file_logging(options).is_err());
            let message = "x".repeat(1000);
            for _ in 0..100 {
                log::warn!(target: "fluss::client", "{message}");
            }
            handle.shutdown(Duration::from_secs(3)).unwrap();
            let stats = handle.stats();
            assert_eq!(
                (
                    stats.accepted,
                    stats.written,
                    stats.dropped,
                    stats.write_errors
                ),
                (100, 100, 0, 0)
            );
            let group = handle.path().parent().unwrap();
            let files: Vec<_> = std::fs::read_dir(group)
                .unwrap()
                .map(|file| file.unwrap().path())
                .collect();
            assert!(files.len() <= 3);
            assert!(group.join("client.0.log").exists() && group.join("client.1.log").exists());
            for file in files {
                assert!(std::fs::metadata(file).unwrap().len() <= 16384);
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(group).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
            return;
        }
        let directory = std::env::temp_dir().join(format!(
            "fluss-logging-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for mode in ["normal", "foreign"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "logging::tests::global_logger_and_rotation_in_isolated_processes",
                    "--nocapture",
                ])
                .env("FLUSS_LOG_TEST_MODE", mode)
                .env("FLUSS_LOG_TEST_DIRECTORY", &directory)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(not(feature = "file-logging"))]
    #[test]
    fn disabled_feature_returns_explicit_error() {
        assert_eq!(
            init_file_logging(FileLogOptions::default())
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::Unsupported
        );
    }
}
