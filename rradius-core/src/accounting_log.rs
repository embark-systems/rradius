//! `[radius.accounting_log]`: one line of JSON for each accounting request answered, appended to
//! a local file. The line is the one the enterprise accounting packet log writes, without the node.

use crate::extension::{AccountingObserver, AccountingRecord};
use crate::metrics::AccountingLogCounters;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long stopping waits for the lines still queued.
const STOP_WAIT: Duration = Duration::from_secs(5);
/// How long the writer sleeps after each batch. While it sleeps, answering a request only puts
/// the record in the queue, with no wake-up of the writer and no write of its own: a wake-up and a
/// write for each request cost about 15% more CPU per accounting request (measured). A line
/// reaches the file at most about this long after the answer; stop and reload still write all
/// that is queued.
const BATCH_INTERVAL: Duration = Duration::from_millis(10);
/// The same failure is logged at most this often.
const WARN_EVERY: Duration = Duration::from_secs(60);

const DAY: i64 = 24 * 3600;

/// `time` as RFC 3339 in UTC, to the millisecond.
pub fn timestamp(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = i64::try_from(since.as_secs()).unwrap_or(i64::MAX / 2);
    let (day, second) = (seconds.div_euclid(DAY), seconds.rem_euclid(DAY));
    let (year, month, date) = civil(day);
    format!("{year:04}-{month:02}-{date:02}T{:02}:{:02}:{:02}.{:03}Z", second / 3600, second / 60 % 60, second % 60, since.subsec_millis())
}

/// The year, month and day of `day`, counted from 1970-01-01, in the proleptic Gregorian calendar.
fn civil(day: i64) -> (i64, i64, i64) {
    let z = day + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let date = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, date)
}

/// Appends `text` to `out` as a JSON string.
pub fn json_string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The attributes of a request as one JSON object: an attribute that comes more than once is a
/// list of its values, in order.
pub fn attributes_json(attributes: &[(String, String)]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for (name, _) in attributes {
        if !names.contains(&name.as_str()) {
            names.push(name);
        }
    }
    let mut out = String::from("{");
    for (at, name) in names.iter().enumerate() {
        if at > 0 {
            out.push(',');
        }
        json_string(name, &mut out);
        out.push(':');
        let values: Vec<&str> = attributes.iter().filter(|(other, _)| other == name).map(|(_, value)| value.as_str()).collect();
        match values.as_slice() {
            [one] => json_string(one, &mut out),
            many => {
                out.push('[');
                for (at, value) in many.iter().enumerate() {
                    if at > 0 {
                        out.push(',');
                    }
                    json_string(value, &mut out);
                }
                out.push(']');
            }
        }
    }
    out.push('}');
    out
}

impl AccountingRecord {
    /// The request as one line of JSON, without the newline: `time`, then `node` when given,
    /// `client`, `source`, `transport`, `handling` and `attributes`.
    pub fn json_line(&self, node: Option<u64>) -> String {
        let mut line = String::from("{\"time\":");
        json_string(&timestamp(self.time), &mut line);
        if let Some(node) = node {
            line.push_str(&format!(",\"node\":{node}"));
        }
        line.push_str(",\"client\":");
        json_string(&self.client, &mut line);
        line.push_str(",\"source\":");
        json_string(&self.source.to_string(), &mut line);
        line.push_str(",\"transport\":");
        json_string(self.transport, &mut line);
        line.push_str(",\"handling\":");
        json_string(self.handling, &mut line);
        line.push_str(",\"attributes\":");
        line.push_str(&attributes_json(&self.attributes));
        line.push('}');
        line
    }
}

/// What the writer is sent: a request to write, or what to do next.
enum Message {
    Record(Box<AccountingRecord>),
    /// Open this file and write there from now on.
    Reopen(PathBuf),
    /// Write what is queued and end, then say so.
    Stop(std::sync::mpsc::Sender<()>),
}

/// Where the lines go: the file, or in tests anything that writes.
type Sink = Box<dyn Write + Send>;

/// `[radius.accounting_log]`: a queue of bounded length in front of a thread that appends the
/// lines. Telling it of a request never waits.
pub struct AccountingLog {
    sender: SyncSender<Message>,
    counters: Arc<AccountingLogCounters>,
    /// Set once stopping: what is answered after is neither written nor counted.
    stopped: AtomicBool,
}

impl AccountingLog {
    /// Opens `file` for appending, creating it with mode 0640 when it is not there, and starts
    /// the thread that writes to it.
    pub fn start(file: &Path, queue: usize, counters: Arc<AccountingLogCounters>) -> Result<Arc<Self>, String> {
        let sink = open(file).map_err(|e| format!("radius.accounting_log.file: cannot open {}: {e}", file.display()))?;
        Self::start_with(sink, queue, counters, open)
    }

    fn start_with(sink: Sink, queue: usize, counters: Arc<AccountingLogCounters>, opener: fn(&Path) -> std::io::Result<Sink>) -> Result<Arc<Self>, String> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(queue);
        let writer = Writer { sink, opener, counters: counters.clone(), warned: None };
        std::thread::Builder::new().name("accounting-log".to_string()).spawn(move || writer.run(receiver)).map_err(|e| format!("cannot start the accounting log: {e}"))?;
        Ok(Arc::new(Self { sender, counters, stopped: AtomicBool::new(false) }))
    }

    /// After what is queued now, write to `file` instead: what a reload does, so that a file
    /// moved away by rotation is followed by a new one. Waits only for room in the queue.
    pub fn reopen(&self, file: &Path) {
        let _ = self.sender.send(Message::Reopen(file.to_path_buf()));
    }

    /// Writes what is queued and ends the writer, waiting at most `STOP_WAIT`. Requests told of
    /// afterwards are ignored.
    pub fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let (done, ended) = std::sync::mpsc::channel();
        let deadline = Instant::now() + STOP_WAIT;
        let mut message = Message::Stop(done);
        // The queue may be full; the writer empties it.
        loop {
            match self.sender.try_send(message) {
                Ok(()) => break,
                Err(TrySendError::Disconnected(_)) => return,
                Err(TrySendError::Full(back)) if Instant::now() < deadline => {
                    message = back;
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(TrySendError::Full(_)) => {
                    tracing::warn!("accounting log: lines still queued after {} s are not written", STOP_WAIT.as_secs());
                    return;
                }
            }
        }
        if ended.recv_timeout(deadline.saturating_duration_since(Instant::now())).is_err() {
            tracing::warn!("accounting log: lines still queued after {} s are not written", STOP_WAIT.as_secs());
        }
    }
}

impl AccountingObserver for AccountingLog {
    fn observe(&self, record: &AccountingRecord) {
        if self.stopped.load(Ordering::Relaxed) {
            return;
        }
        if let Err(TrySendError::Full(_)) = self.sender.try_send(Message::Record(Box::new(record.clone()))) {
            self.counters.dropped.inc();
        }
    }
}

/// Opens `file` for appending, made with mode 0640 when it is not there.
fn open(file: &Path) -> std::io::Result<Sink> {
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o640);
    Ok(Box::new(options.open(file)?))
}

struct Writer {
    sink: Sink,
    opener: fn(&Path) -> std::io::Result<Sink>,
    counters: Arc<AccountingLogCounters>,
    /// The last failure logged and when, so that the same one is logged once a minute.
    warned: Option<(String, Instant)>,
}

impl Writer {
    /// Takes everything queued at once, writes it in one go, and flushes when nothing waits.
    fn run(mut self, receiver: Receiver<Message>) {
        let (mut lines, mut count) = (String::new(), 0u64);
        while let Ok(first) = receiver.recv() {
            let mut next = Some(first);
            while let Some(message) = next.take().or_else(|| receiver.try_recv().ok()) {
                match message {
                    Message::Record(record) => {
                        lines.push_str(&record.json_line(None));
                        lines.push('\n');
                        count += 1;
                    }
                    Message::Reopen(file) => {
                        self.write(&mut lines, &mut count);
                        match (self.opener)(&file) {
                            Ok(sink) => self.sink = sink,
                            Err(e) => {
                                self.counters.failed_open.inc();
                                self.warn(format!("accounting log: cannot open {}, still writing to the file open before: {e}", file.display()));
                            }
                        }
                    }
                    Message::Stop(done) => {
                        self.write(&mut lines, &mut count);
                        let _ = done.send(());
                        return;
                    }
                }
            }
            self.write(&mut lines, &mut count);
            std::thread::sleep(BATCH_INTERVAL);
        }
    }

    /// Writes the `count` requests of `lines` and flushes; on a failure they are dropped.
    fn write(&mut self, lines: &mut String, count: &mut u64) {
        if *count == 0 {
            return;
        }
        match self.sink.write_all(lines.as_bytes()).and_then(|()| self.sink.flush()) {
            Ok(()) => self.counters.written.add(*count),
            Err(e) => {
                self.counters.failed_write.inc();
                self.warn(format!("accounting log: {count} requests not written: {e}"));
            }
        }
        lines.clear();
        *count = 0;
    }

    fn warn(&mut self, message: String) {
        // The same failure once a minute; the number of requests is not part of it.
        let kind: String = message.chars().filter(|c| !c.is_ascii_digit()).collect();
        if self.warned.as_ref().is_some_and(|(last, at)| *last == kind && at.elapsed() < WARN_EVERY) {
            return;
        }
        tracing::warn!("{message}");
        self.warned = Some((kind, Instant::now()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn record() -> AccountingRecord {
        AccountingRecord {
            // 2026-11-15T08:00:00.123Z
            time: UNIX_EPOCH + Duration::from_millis(1_794_729_600_123),
            client: "nas1".to_string(),
            source: std::net::IpAddr::from([127, 0, 0, 1]),
            transport: "udp",
            handling: "script",
            attributes: [("Acct-Status-Type", "Start"), ("Acct-Session-Id", "s-1"), ("Class", "a"), ("Class", "b")].map(|(name, value)| (name.to_string(), value.to_string())).to_vec(),
        }
    }

    /// spec Accounting Log Lines, Example: a line.
    #[test]
    fn test_json_line() {
        let expected = r#"{"time":"2026-11-15T08:00:00.123Z","client":"nas1","source":"127.0.0.1","transport":"udp","handling":"script","attributes":{"Acct-Status-Type":"Start","Acct-Session-Id":"s-1","Class":["a","b"]}}"#;
        assert_eq!(record().json_line(None), expected);
        let with_node = expected.replace(r#"Z","client""#, r#"Z","node":1,"client""#);
        assert_eq!(record().json_line(Some(1)), with_node);
    }

    #[test]
    fn test_json_escapes() {
        let mut out = String::new();
        json_string("a\"b\\c\nd\u{1}é", &mut out);
        assert_eq!(out, r#""a\"b\\c\nd\u0001é""#);
        assert_eq!(attributes_json(&[]), "{}");
    }

    #[test]
    fn test_timestamp() {
        assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        // A leap day.
        assert_eq!(timestamp(UNIX_EPOCH + Duration::from_secs(951_782_400)), "2000-02-29T00:00:00.000Z");
        assert_eq!(timestamp(UNIX_EPOCH + Duration::from_millis(1_794_729_600_123)), "2026-11-15T08:00:00.123Z");
    }

    fn counters() -> Arc<AccountingLogCounters> {
        Arc::new(AccountingLogCounters::default())
    }

    /// spec Accounting Log Lines: one line per request, appended to what is there, the file
    /// made with mode 0640; spec Accounting Log Through Shutdown: stop writes what is queued.
    #[test]
    fn test_writes_lines() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let file = dir.path().join("acct.jsonl");
        std::fs::write(&file, "earlier\n").map_err(|e| e.to_string())?;
        let counts = counters();
        let log = AccountingLog::start(&file, 65536, counts.clone())?;
        for _ in 0..1000 {
            log.observe(&record());
        }
        log.stop();
        log.observe(&record());
        let text = std::fs::read_to_string(&file).map_err(|e| e.to_string())?;
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1001, "what was there, then each request once, nothing after stop");
        assert_eq!(lines[0], "earlier");
        assert_eq!(lines[1], record().json_line(None));
        assert_eq!((counts.written.get(), counts.dropped.get()), (1000, 0), "a request after stop is not counted");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let made = dir.path().join("new.jsonl");
            let log = AccountingLog::start(&made, 8, counters())?;
            log.stop();
            let mode = std::fs::metadata(&made).map_err(|e| e.to_string())?.permissions().mode() & 0o777;
            assert_eq!(mode & !0o640, 0, "{mode:o}");
        }
        Ok(())
    }

    /// Writes slowly, as a full disk might.
    struct Slow;

    impl Write for Slow {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            std::thread::sleep(Duration::from_millis(50));
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Refuses every write.
    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// spec Accounting Log Lines, Queue full: telling never waits, what does not fit is counted.
    #[test]
    fn test_queue_full() -> Result<(), String> {
        let counts = counters();
        let log = AccountingLog::start_with(Box::new(Slow), 1, counts.clone(), open)?;
        let started = Instant::now();
        for _ in 0..100 {
            log.observe(&record());
        }
        assert!(started.elapsed() < Duration::from_millis(500), "telling waited: {:?}", started.elapsed());
        log.stop();
        assert!(counts.dropped.get() > 0, "nothing dropped");
        assert_eq!(counts.dropped.get() + counts.written.get(), 100);
        Ok(())
    }

    /// spec Accounting Log Metrics: a failed write is counted and the writer goes on; spec
    /// Accounting Log Reopened on Reload: a file that cannot be opened keeps the one open.
    #[test]
    fn test_failures() -> Result<(), String> {
        let counts = counters();
        let log = AccountingLog::start_with(Box::new(Broken), 16, counts.clone(), open)?;
        log.observe(&record());
        log.stop();
        assert_eq!((counts.failed_write.get(), counts.written.get()), (1, 0));

        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let file = dir.path().join("acct.jsonl");
        let counts = counters();
        let log = AccountingLog::start(&file, 16, counts.clone())?;
        log.observe(&record());
        // A directory cannot be opened as the file.
        log.reopen(dir.path());
        log.observe(&record());
        let moved = dir.path().join("acct.jsonl.1");
        log.reopen(&moved);
        log.observe(&record());
        log.stop();
        assert_eq!(counts.failed_open.get(), 1);
        let lines = |path: &Path| std::fs::read_to_string(path).map(|text| text.lines().count()).map_err(|e| e.to_string());
        assert_eq!(lines(&file)?, 2, "kept the file open before");
        assert_eq!(lines(&moved)?, 1, "then the new one");
        assert_eq!(counts.written.get(), 3);
        Ok(())
    }
}
