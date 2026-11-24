//! Named queries: the `[queries.<name>]` of the configuration. Each node runs every query in the
//! background against its own copy of the database and keeps the rows of the last successful run;
//! `/metrics` and `status` read what is kept and never query. `rradiusd query` runs the same
//! statements once, through the cluster's API port, and does not come here.

use crate::config::QuerySettings;
use crate::lua_engine::{Db, DbRow, DbValue};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Why a run failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// No answer within `timeout_ms`.
    Timeout,
    /// The database refused the statement or could not be reached.
    Error,
    /// The rows are not what the settings expect: a column that is not there, or of a type that
    /// cannot be a label or a value.
    Shape,
}

impl Reason {
    pub const ALL: [Reason; 3] = [Reason::Timeout, Reason::Error, Reason::Shape];

    /// The value of the `reason` label.
    pub fn name(self) -> &'static str {
        match self {
            Reason::Timeout => "timeout",
            Reason::Error => "error",
            Reason::Shape => "shape",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// A row that became a sample of the gauge.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    /// The values of the label columns, in the order of the settings' `labels`.
    pub labels: Vec<String>,
    pub value: f64,
}

/// What a query has reported so far.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub samples: Vec<Sample>,
    /// Failed runs, by `Reason::ALL`.
    pub failures: [u64; 3],
    /// Rows left out because the value was NULL.
    pub null_rows: u64,
    /// Rows left out because they were beyond `max_rows`.
    pub over_limit_rows: u64,
    /// Unix time in seconds of the last successful run; 0 before one.
    pub last_success: u64,
    /// Runs started, successful or not.
    pub runs: u64,
}

/// One query and what its runs have given.
#[derive(Debug)]
pub struct QueryState {
    name: String,
    settings: QuerySettings,
    samples: Mutex<Vec<Sample>>,
    failures: [AtomicU64; 3],
    null_rows: AtomicU64,
    over_limit_rows: AtomicU64,
    last_success: AtomicU64,
    runs: AtomicU64,
    /// When a failure of each reason was last logged: one line a minute is enough.
    last_logged: Mutex<[Option<Instant>; 3]>,
    /// Runs under way now and the most there have been at once, for the test of overlap.
    #[cfg(test)]
    active: AtomicU64,
    #[cfg(test)]
    most_active: AtomicU64,
}

/// How often a failure of one kind of one query is logged.
const LOG_EVERY: Duration = Duration::from_secs(60);

impl QueryState {
    pub fn new(name: &str, settings: QuerySettings) -> Self {
        Self {
            name: name.to_string(),
            settings,
            samples: Mutex::new(Vec::new()),
            failures: Default::default(),
            null_rows: AtomicU64::new(0),
            over_limit_rows: AtomicU64::new(0),
            last_success: AtomicU64::new(0),
            runs: AtomicU64::new(0),
            last_logged: Mutex::new([None; 3]),
            #[cfg(test)]
            active: AtomicU64::new(0),
            #[cfg(test)]
            most_active: AtomicU64::new(0),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn settings(&self) -> &QuerySettings {
        &self.settings
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            samples: self.samples.lock().unwrap_or_else(PoisonError::into_inner).clone(),
            failures: [0, 1, 2].map(|i| self.failures[i].load(Ordering::Relaxed)),
            null_rows: self.null_rows.load(Ordering::Relaxed),
            over_limit_rows: self.over_limit_rows.load(Ordering::Relaxed),
            last_success: self.last_success.load(Ordering::Relaxed),
            runs: self.runs.load(Ordering::Relaxed),
        }
    }

    /// Runs the statement once. A failed run leaves the samples of the last successful one.
    pub async fn run_once(&self, db: &Db) -> Result<(), Reason> {
        self.runs.fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        self.most_active.fetch_max(self.active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
        let outcome = match tokio::time::timeout(self.settings.timeout, db.query(&self.settings.sql)).await {
            Err(_) => Err((Reason::Timeout, format!("no answer within {} ms", self.settings.timeout.as_millis()))),
            Ok(Err(message)) => Err((Reason::Error, message)),
            Ok(Ok(rows)) => self.samples_of(&rows),
        };
        #[cfg(test)]
        self.active.fetch_sub(1, Ordering::SeqCst);
        match outcome {
            Ok((samples, null_rows, over_limit_rows)) => {
                *self.samples.lock().unwrap_or_else(PoisonError::into_inner) = samples;
                self.null_rows.fetch_add(null_rows, Ordering::Relaxed);
                self.over_limit_rows.fetch_add(over_limit_rows, Ordering::Relaxed);
                self.last_success.store(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs()), Ordering::Relaxed);
                Ok(())
            }
            Err((reason, message)) => {
                self.failures[reason.index()].fetch_add(1, Ordering::Relaxed);
                self.log_failure(reason, &message);
                Err(reason)
            }
        }
    }

    fn log_failure(&self, reason: Reason, message: &str) {
        let mut logged = self.last_logged.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = &mut logged[reason.index()];
        if slot.is_some_and(|at| at.elapsed() < LOG_EVERY) {
            return;
        }
        *slot = Some(Instant::now());
        tracing::warn!("query {}: the run failed ({}): {message}", self.name, reason.name());
    }

    /// The samples of `rows`, how many rows had no value and how many were beyond the limit.
    fn samples_of(&self, rows: &[DbRow]) -> Result<(Vec<Sample>, u64, u64), (Reason, String)> {
        let shape = |message: String| (Reason::Shape, message);
        let settings = &self.settings;
        let mut samples = Vec::with_capacity(rows.len().min(settings.max_rows));
        let mut seen: HashSet<Vec<String>> = HashSet::new();
        let (mut null_rows, mut over_limit_rows) = (0, 0);
        for (index, row) in rows.iter().enumerate() {
            if index >= settings.max_rows {
                over_limit_rows = (rows.len() - settings.max_rows) as u64;
                break;
            }
            let column = |name: &str| row.iter().find(|(found, _)| found == name).map(|(_, value)| value).ok_or_else(|| shape(format!("the result has no column \"{name}\"")));
            let value = match column(&settings.value)? {
                DbValue::Null => {
                    null_rows += 1;
                    continue;
                }
                DbValue::Integer(value) => *value as f64,
                DbValue::Real(value) if value.is_finite() => *value,
                other => return Err(shape(format!("column \"{}\" is not an integer or a real number: {other:?}", settings.value))),
            };
            let mut labels = Vec::with_capacity(settings.labels.len());
            for name in &settings.labels {
                labels.push(match column(name)? {
                    DbValue::Text(text) => text.clone(),
                    DbValue::Integer(number) => number.to_string(),
                    other => return Err(shape(format!("label column \"{name}\" is not text or an integer: {other:?}"))),
                });
            }
            if !seen.insert(labels.clone()) {
                return Err(shape(format!("two rows have the labels {labels:?}")));
            }
            samples.push(Sample { labels, value });
        }
        Ok((samples, null_rows, over_limit_rows))
    }
}

/// Runs the query again and again: once now, then every interval counted from the start of a
/// run. A run that takes longer than the interval is followed by the next at once, never by
/// one that overlaps it. Ends when the last reference to the state is gone.
pub async fn run_forever(state: Weak<QueryState>, db: Db) {
    loop {
        let Some(query) = state.upgrade() else { return };
        let started = Instant::now();
        let interval = query.settings.interval;
        let _ = query.run_once(&db).await;
        drop(query);
        tokio::time::sleep_until((started + interval).into()).await;
    }
}

/// Starts the runs of every query of `states` and returns them.
pub fn start(states: &[Arc<QueryState>], db: &Db) {
    for state in states {
        tokio::spawn(run_forever(Arc::downgrade(state), db.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_db;

    // A timeout far longer than any query here takes: the test database is shared by the whole
    // process, and on a busy machine a query that takes milliseconds can wait seconds for its turn.
    // The test of the timeout itself sets its own.
    fn settings(sql: &str, labels: &[&str], value: &str) -> QuerySettings {
        QuerySettings { sql: sql.to_string(), help: String::new(), labels: labels.iter().map(|label| label.to_string()).collect(), value: value.to_string(), interval: Duration::from_secs(30), timeout: Duration::from_secs(30), max_rows: 200 }
    }

    async fn table(name: &str, rows: &str) -> Result<(), String> {
        let db = test_db();
        db.execute(&format!("CREATE TABLE {name} (pool TEXT, state TEXT, n INTEGER, x REAL, num INTEGER)")).await?;
        if !rows.is_empty() {
            db.execute(&format!("INSERT INTO {name} (pool, state, n, x, num) VALUES {rows}")).await?;
        }
        Ok(())
    }

    fn pairs(samples: &[Sample]) -> Vec<(Vec<&str>, f64)> {
        samples.iter().map(|sample| (sample.labels.iter().map(String::as_str).collect(), sample.value)).collect()
    }

    /// The pool usage of the spec: one sample for each row, the labels as text, integers and reals as values.
    #[tokio::test]
    async fn test_named_query_samples() -> Result<(), String> {
        table("nq_samples", "('residential', 'free', 8, 0.5, 1), ('residential', 'reserved', 2, 1.5, 2)").await?;
        let state = QueryState::new("pool_usage", settings("SELECT pool, state, n FROM nq_samples ORDER BY state", &["pool", "state"], "n"));
        assert_eq!(state.snapshot().last_success, 0);
        state.run_once(&test_db()).await.map_err(|reason| reason.name())?;
        let snapshot = state.snapshot();
        assert_eq!(pairs(&snapshot.samples), [(vec!["residential", "free"], 8.0), (vec!["residential", "reserved"], 2.0)]);
        assert!(snapshot.last_success > 0);
        assert_eq!((snapshot.failures, snapshot.null_rows, snapshot.over_limit_rows), ([0; 3], 0, 0));
        // A real number as the value, an integer as a label.
        let state = QueryState::new("reals", settings("SELECT num, x FROM nq_samples ORDER BY num", &["num"], "x"));
        state.run_once(&test_db()).await.map_err(|reason| reason.name())?;
        assert_eq!(pairs(&state.snapshot().samples), [(vec!["1"], 0.5), (vec!["2"], 1.5)]);
        Ok(())
    }

    /// The spec's 205 rows with a limit of 200 and three that have no value: 197 samples, 3 and 5.
    #[tokio::test]
    async fn test_named_query_null_values_and_the_row_limit() -> Result<(), String> {
        table("nq_limit", "").await?;
        test_db().execute("INSERT INTO nq_limit (pool, n) WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM r WHERE i < 205) SELECT 'p' || i, CASE WHEN i IN (3, 50, 199) THEN NULL ELSE i END FROM r").await?;
        let state = QueryState::new("limited", settings("SELECT pool, n FROM nq_limit ORDER BY rowid", &["pool"], "n"));
        state.run_once(&test_db()).await.map_err(|reason| reason.name())?;
        let snapshot = state.snapshot();
        assert_eq!((snapshot.samples.len(), snapshot.null_rows, snapshot.over_limit_rows), (197, 3, 5));
        // Another run adds to the counters; the samples are those of the run.
        state.run_once(&test_db()).await.map_err(|reason| reason.name())?;
        let snapshot = state.snapshot();
        assert_eq!((snapshot.samples.len(), snapshot.null_rows, snapshot.over_limit_rows), (197, 6, 10));
        Ok(())
    }

    /// A failed run keeps the last samples, counts one failure of its kind and leaves the time of the last success.
    #[tokio::test]
    async fn test_named_query_failures() -> Result<(), String> {
        table("nq_fail", "('a', 'x', 1, 0.0, 1)").await?;
        let db = test_db();
        // A column that is not there, a value that is text, a label that is NULL, the same labels twice.
        for (sql, labels, value) in [
            ("SELECT pool, n AS other FROM nq_fail", vec!["pool"], "n"),
            ("SELECT pool, state AS n FROM nq_fail", vec!["pool"], "n"),
            ("SELECT NULL AS pool, n FROM nq_fail", vec!["pool"], "n"),
            ("SELECT 'a' AS pool, n FROM nq_fail UNION ALL SELECT 'a', 2", vec!["pool"], "n"),
            ("SELECT pool, n FROM nq_fail", vec!["missing"], "n"),
        ] {
            let state = QueryState::new("shape", settings("SELECT pool, n FROM nq_fail", &["pool"], "n"));
            state.run_once(&db).await.map_err(|reason| reason.name())?;
            let kept = state.snapshot();
            let broken = QueryState { samples: Mutex::new(kept.samples.clone()), ..QueryState::new("shape", settings(sql, &labels, value)) };
            let last = broken.snapshot().last_success;
            assert_eq!(broken.run_once(&db).await, Err(Reason::Shape), "{sql}");
            let snapshot = broken.snapshot();
            assert_eq!((snapshot.failures, snapshot.samples, snapshot.last_success), ([0, 0, 1], kept.samples, last), "{sql}");
        }
        // The database refuses it.
        let state = QueryState::new("gone", settings("SELECT n FROM nq_no_such_table", &[], "n"));
        assert_eq!(state.run_once(&db).await, Err(Reason::Error));
        assert_eq!((state.snapshot().failures, state.snapshot().last_success), ([0, 1, 0], 0));
        // It does not answer in time.
        let mut slow = settings("WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 10000000) SELECT count(*) AS n FROM c", &[], "n");
        slow.timeout = Duration::from_millis(20);
        let state = QueryState::new("slow", slow);
        assert_eq!(state.run_once(&db).await, Err(Reason::Timeout));
        assert_eq!(state.snapshot().failures, [1, 0, 0]);
        Ok(())
    }

    /// A run that is slower than the interval is not overlapped: the runs follow one another.
    #[tokio::test]
    async fn test_named_query_runs_do_not_overlap() -> Result<(), String> {
        let mut slow = settings("WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 300000) SELECT count(*) AS n FROM c", &[], "n");
        // An interval far shorter than a run: a run that did not wait for the one before would
        // overlap it. Wait for several runs and ask how many were under way at once.
        slow.interval = Duration::from_millis(1);
        let state = Arc::new(QueryState::new("overlap", slow));
        let handle = tokio::spawn(run_forever(Arc::downgrade(&state), test_db()));
        let deadline = Instant::now() + Duration::from_secs(60);
        while state.snapshot().runs < 5 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        handle.abort();
        assert!(state.snapshot().runs >= 5, "it ran again and again: {}", state.snapshot().runs);
        assert_eq!(state.most_active.load(Ordering::SeqCst), 1, "never two runs at once");
        assert_eq!(state.snapshot().failures, [0; 3]);
        Ok(())
    }
}
