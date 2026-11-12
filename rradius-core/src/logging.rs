//! Where the server's messages go: timestamped, levelled lines on stderr, written from a
//! background thread so that output nobody reads cannot hold up a request. Output that
//! cannot keep up is dropped and counted, so these lines are not a reliable audit trail.

use crate::config::{LogFormat, LogLevel, LogSettings};
use std::io::{self, Write};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};
use tracing_appender::non_blocking::{ErrorCounter, NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::field::MakeExt;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt::format::{debug_fn, FormatFields};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The target of authentication records; `[log].auth` decides about it, not `level`.
const AUTH_TARGET: &str = "rradius_auth";
/// The target of `[log.trace]` lines, which asked for them whatever `level` says.
pub(crate) const TRACE_TARGET: &str = "rradius_trace";

static DROPPED: OnceLock<ErrorCounter> = OnceLock::new();
static WORKER: Mutex<Option<WorkerGuard>> = Mutex::new(None);
/// Lines handed to the background thread, whether its queue took them or not.
static OFFERED: AtomicU64 = AtomicU64::new(0);
/// Lines the background thread is done with, whether writing them worked or not.
static FINISHED: AtomicU64 = AtomicU64::new(0);
/// Lines the background thread failed to write.
static FAILED: AtomicU64 = AtomicU64::new(0);

/// How long the end of the process waits for lines still waiting to be written.
const FLUSH_WAIT: Duration = Duration::from_secs(5);

/// Stderr for the background thread, which ignores errors: they would otherwise go
/// unnoticed, since its queue goes on emptying. It writes each line with one call.
struct CountingStderr;

impl Write for CountingStderr {
    fn write(&mut self, line: &[u8]) -> io::Result<usize> {
        if io::stderr().write_all(line).is_err() {
            FAILED.fetch_add(1, Ordering::Relaxed);
        }
        FINISHED.fetch_add(1, Ordering::Relaxed);
        Ok(line.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The way into the background thread's queue, counting what goes in.
struct CountingQueue(NonBlocking);

impl Write for CountingQueue {
    fn write(&mut self, line: &[u8]) -> io::Result<usize> {
        OFFERED.fetch_add(1, Ordering::Relaxed);
        self.0.write(line)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<'a> MakeWriter<'a> for CountingQueue {
    type Writer = CountingQueue;

    fn make_writer(&'a self) -> Self::Writer {
        CountingQueue(self.0.clone())
    }
}

/// The crates whose messages `[log] level` applies to: this library and the programs built on it.
const OWN_TARGETS: [&str; 3] = ["rradius_core", "rradius_server", "rradius_enterprise"];

/// What gets through. Other libraries stay at `warn`, whatever `level` says: at `debug`
/// the cluster's consensus code would drown everything else.
fn filter(settings: &LogSettings) -> Targets {
    let level = match settings.level {
        LogLevel::Error => LevelFilter::ERROR,
        LogLevel::Warn => LevelFilter::WARN,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Debug => LevelFilter::DEBUG,
    };
    let auth = if settings.auth { LevelFilter::INFO } else { LevelFilter::OFF };
    let mut targets = Targets::new().with_default(LevelFilter::WARN);
    for program in OWN_TARGETS {
        targets = targets.with_target(program, level);
    }
    targets.with_target(AUTH_TARGET, auth).with_target(TRACE_TARGET, LevelFilter::INFO)
}

/// Replaces control characters and line separators with escapes, so that a value from
/// outside cannot end the line it is on.
fn escape_line(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
            escaped.extend(c.escape_default());
        } else {
            escaped.push(c);
        }
    }
    escaped
}

/// Writes a text line's message and fields, one line whatever they hold.
fn text_fields() -> impl for<'writer> FormatFields<'writer> + 'static {
    debug_fn(|writer, field, value| {
        let value = escape_line(&format!("{value:?}"));
        if field.name() == "message" {
            write!(writer, "{value}")
        } else {
            write!(writer, "{}={value}", field.name())
        }
    })
    .delimited(" ")
}

/// Starts logging as `settings` says. Until `flush`, lines go through a background thread.
pub fn init(settings: &LogSettings) {
    #[cfg(any(test, feature = "test-support"))]
    crate::test_support::stop_keeping();
    let (writer, worker) = NonBlockingBuilder::default().lossy(true).finish(CountingStderr);
    let _ = DROPPED.set(writer.error_counter());
    *WORKER.lock().unwrap_or_else(PoisonError::into_inner) = Some(worker);
    let layer = tracing_subscriber::fmt::layer().with_ansi(false).with_writer(CountingQueue(writer));
    let registry = tracing_subscriber::registry().with(filter(settings));
    let _ = match settings.format {
        LogFormat::Text => registry.with(layer.fmt_fields(text_fields())).try_init(),
        LogFormat::Json => registry.with(layer.json().flatten_event(true).with_current_span(false).with_span_list(false)).try_init(),
    };
}

/// Logging for a command an operator runs by hand: warnings and errors, written at once.
pub fn init_for_command() {
    let settings = LogSettings { level: LogLevel::Warn, format: LogFormat::Text, auth: false };
    let layer = tracing_subscriber::fmt::layer().with_ansi(false).with_writer(std::io::stderr).fmt_fields(text_fields());
    let _ = tracing_subscriber::registry().with(filter(&settings)).with(layer).try_init();
}

/// Waits for what is waiting in the background to be written, for up to `FLUSH_WAIT`; for
/// just before the process exits, which does not wait for that thread. Output that is stuck
/// loses what is left, uncounted, since nothing can read the count anymore.
pub fn flush() {
    let Some(worker) = WORKER.lock().unwrap_or_else(PoisonError::into_inner).take() else {
        return;
    };
    let deadline = Instant::now() + FLUSH_WAIT;
    let drained = loop {
        let done = FINISHED.load(Ordering::Relaxed) + DROPPED.get().map_or(0, |counter| counter.dropped_lines() as u64);
        if done >= OFFERED.load(Ordering::Relaxed) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    if drained {
        drop(worker);
    } else {
        // Dropping it waits a little longer, and with its queue full, says so on stdout.
        std::mem::forget(worker);
    }
}

/// One authentication decision. These are all the fields a record has, so a secret cannot
/// end up in one unless it is in the user name.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthRecord<'a> {
    pub protocol: &'a str,
    pub client: &'a str,
    pub source: IpAddr,
    /// The name received; empty without one.
    pub user: &'a str,
    /// The name looked up; empty without one.
    pub account: &'a str,
    /// The chain's name, `default`, or `script` when no chain decided.
    pub chain: &'a str,
    /// `sql`, `lua`, `ldap`, `script`, `none` when every handler ignored the request, or
    /// `normalization` when the chain refused the name.
    pub handler: &'a str,
    /// `accept`, `reject` or `challenge`.
    pub decision: &'a str,
}

/// Writes a record, if `[log].auth` asks for them.
pub fn auth_record(record: &AuthRecord) {
    #[cfg(any(test, feature = "test-support"))]
    if crate::test_support::keeping() {
        records().lock().unwrap_or_else(PoisonError::into_inner).push(OwnedRecord::from(record));
    }
    tracing::info!(
        target: AUTH_TARGET,
        protocol = record.protocol,
        client = record.client,
        source = %record.source,
        user = record.user,
        account = record.account,
        chain = record.chain,
        handler = record.handler,
        decision = record.decision,
        "authentication"
    );
}

/// A record kept for tests to look at.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone, PartialEq)]
pub struct OwnedRecord {
    pub protocol: String,
    pub client: String,
    pub source: IpAddr,
    pub user: String,
    pub account: String,
    pub chain: String,
    pub handler: String,
    pub decision: String,
}

#[cfg(any(test, feature = "test-support"))]
impl From<&AuthRecord<'_>> for OwnedRecord {
    fn from(r: &AuthRecord) -> Self {
        Self {
            protocol: r.protocol.to_string(),
            client: r.client.to_string(),
            source: r.source,
            user: r.user.to_string(),
            account: r.account.to_string(),
            chain: r.chain.to_string(),
            handler: r.handler.to_string(),
            decision: r.decision.to_string(),
        }
    }
}

/// Every record written in this test binary.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn records() -> &'static Mutex<Vec<OwnedRecord>> {
    static RECORDS: Mutex<Vec<OwnedRecord>> = Mutex::new(Vec::new());
    &RECORDS
}

/// The records for one user name; tests use names of their own.
#[cfg(test)]
pub(crate) fn records_for(user: &str) -> Vec<OwnedRecord> {
    records().lock().unwrap_or_else(PoisonError::into_inner).iter().filter(|record| record.user == user).cloned().collect()
}

/// The records for one client; tests use client names of their own.
#[cfg(any(test, feature = "test-support"))]
pub fn records_for_client(client: &str) -> Vec<OwnedRecord> {
    records().lock().unwrap_or_else(PoisonError::into_inner).iter().filter(|record| record.client == client).cloned().collect()
}

/// Lines lost: output could not keep up, or writing them failed.
pub fn dropped_lines() -> u64 {
    DROPPED.get().map_or(0, |counter| counter.dropped_lines() as u64) + FAILED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_line() {
        assert_eq!(escape_line("alice"), "alice");
        assert_eq!(escape_line("alice\nINFO forged\r\u{1b}[31m\t\u{2028}"), "alice\\nINFO forged\\r\\u{1b}[31m\\t\\u{2028}");
        assert_eq!(escape_line("許\u{e9}"), "許\u{e9}", "only what can break a line");
    }

    #[test]
    fn test_filter() {
        use tracing::Level;
        let enabled = |settings: LogSettings, target: &str, level: Level| filter(&settings).would_enable(target, &level);
        let settings = |level, auth| LogSettings { level, format: LogFormat::Text, auth };
        assert!(enabled(settings(LogLevel::Info, true), "rradius_core::radius", Level::INFO));
        assert!(!enabled(settings(LogLevel::Info, true), "rradius_core::radius", Level::DEBUG));
        assert!(!enabled(settings(LogLevel::Warn, true), "rradius_core::lua_engine", Level::INFO));
        assert!(enabled(settings(LogLevel::Debug, true), "rradius_core::tacacs", Level::DEBUG));
        for program in ["rradius_server", "rradius_enterprise"] {
            assert!(enabled(settings(LogLevel::Info, true), program, Level::INFO), "{program}");
            assert!(!enabled(settings(LogLevel::Info, true), program, Level::DEBUG), "{program}");
        }
        for level in [LogLevel::Error, LogLevel::Warn, LogLevel::Info, LogLevel::Debug] {
            assert!(enabled(settings(level, true), AUTH_TARGET, Level::INFO), "{level:?}");
            assert!(!enabled(settings(level, false), AUTH_TARGET, Level::INFO), "{level:?}");
            assert!(enabled(settings(level, false), TRACE_TARGET, Level::INFO), "{level:?}");
        }
        assert!(!enabled(settings(LogLevel::Debug, true), "openraft::core", Level::INFO));
        assert!(enabled(settings(LogLevel::Debug, true), "hiqlite", Level::WARN));
    }
}
