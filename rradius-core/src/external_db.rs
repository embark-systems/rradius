//! External MySQL and PostgreSQL databases: `[databases.<name>]`, which the `sql` handler's
//! `database` and the scripts' `db_query` and `db_execute` use.

use crate::config::{DatabaseKind, DatabaseSettings, DatabaseTls};
use crate::lua_engine::{DbFailure, DbRow, DbValue};
use sqlx::{Column, ConnectOptions, Row, ValueRef};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The connections of one `[databases.<name>]`.
pub struct ExternalDatabase {
    name: String,
    kind: DatabaseKind,
    pool: Pool,
    pool_size: usize,
    failures: FailureCounts,
    /// How the readiness check connects, outside the pool.
    options: Options,
    connect_timeout: std::time::Duration,
    /// `required_for_ready`.
    required: bool,
    /// What the readiness check found last; false until it first succeeds.
    reachable: std::sync::atomic::AtomicBool,
}

enum Options {
    Mysql(sqlx::mysql::MySqlConnectOptions),
    Postgres(sqlx::postgres::PgConnectOptions),
}

/// The readiness check's own connection.
enum CheckConnection {
    Mysql(sqlx::MySqlConnection),
    Postgres(sqlx::PgConnection),
}

/// How often the readiness check runs, from the start of one to the start of the next.
pub const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Failed checks in a row that make a reachable database unreachable.
const FAILURES_TO_UNREACHABLE: u32 = 2;

/// Where the readiness checks of a database stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Reachability {
    pub(crate) reachable: bool,
    failed_in_a_row: u32,
}

impl Reachability {
    /// After a check that succeeded or not: one success makes the database reachable, and
    /// `FAILURES_TO_UNREACHABLE` failures in a row unreachable.
    pub(crate) fn after(self, ok: bool) -> Self {
        if ok {
            return Self { reachable: true, failed_in_a_row: 0 };
        }
        let failed_in_a_row = self.failed_in_a_row.saturating_add(1);
        Self { reachable: self.reachable && failed_in_a_row < FAILURES_TO_UNREACHABLE, failed_in_a_row }
    }
}

/// The failed calls of one database since the process started, by class.
#[derive(Default)]
struct FailureCounts {
    unavailable: AtomicU64,
    mistake: AtomicU64,
    unknown: AtomicU64,
}

/// What `ExternalDatabase::failures` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FailureTotals {
    pub unavailable: u64,
    pub mistake: u64,
    pub unknown: u64,
}

/// What `ExternalDatabase::connections` reports: connections of the pool handed to a call, open
/// and free, and the most it keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Connections {
    pub in_use: usize,
    pub idle: usize,
    pub max: usize,
}

enum Pool {
    Mysql(sqlx::MySqlPool),
    Postgres(sqlx::PgPool),
}

/// Every `[databases.<name>]`, by name. Made at startup; a reload keeps them.
#[derive(Clone, Default)]
pub struct ExternalDatabases(Arc<BTreeMap<String, Arc<ExternalDatabase>>>);

impl ExternalDatabases {
    /// A pool for each database, without connecting: the first call that needs a connection
    /// opens one, so a database that is down does not keep the server from starting. Needs a
    /// Tokio runtime.
    pub fn open(settings: &BTreeMap<String, DatabaseSettings>) -> Self {
        warn_about_plain(settings);
        let databases = settings
            .iter()
            .map(|(name, settings)| {
                (name.clone(), Arc::new(ExternalDatabase::open(name, settings)))
            })
            .collect();
        Self(Arc::new(databases))
    }

    pub fn get(&self, name: &str) -> Option<&Arc<ExternalDatabase>> {
        self.0.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// Every database, in name order.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<ExternalDatabase>> {
        self.0.values()
    }
}

impl ExternalDatabases {
    /// Starts the readiness check of each database with `required_for_ready`. A check stops
    /// when its database is no longer held anywhere. Needs a Tokio runtime.
    pub fn start_checks(&self) {
        for database in self.iter().filter(|database| database.required) {
            tokio::spawn(check_forever(Arc::downgrade(database)));
        }
    }
}

/// Checks a database every `CHECK_INTERVAL`, each check bounded by its connect timeout, and
/// notes whether it can be reached.
async fn check_forever(database: std::sync::Weak<ExternalDatabase>) {
    let mut connection = None;
    let mut state = Reachability::default();
    let mut ticks = tokio::time::interval(CHECK_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let Some(database) = database.upgrade() else {
            return;
        };
        let result = match tokio::time::timeout(database.connect_timeout, database.check(&mut connection)).await {
            Ok(result) => result.map_err(|e| e.to_string()),
            Err(_) => {
                // Whatever the connection was doing, it cannot be trusted now.
                connection = None;
                Err(format!("no answer within {} ms", database.connect_timeout.as_millis()))
            }
        };
        let next = state.after(result.is_ok());
        let why = result.as_ref().err().map_or("", String::as_str);
        let at_startup = state == Reachability::default() && next.failed_in_a_row == 1;
        match (state.reachable, next.reachable) {
            (true, false) => tracing::warn!("databases.{}: unreachable after {FAILURES_TO_UNREACHABLE} failed checks, the last: {why}; /readyz answers 503", database.name),
            (false, true) => tracing::info!("databases.{}: reachable", database.name),
            _ if at_startup => tracing::warn!("databases.{}: not reachable at startup: {why}; /readyz answers 503 until it is", database.name),
            _ if result.is_err() => tracing::debug!("databases.{}: readiness check failed: {why}", database.name),
            _ => {}
        }
        database.reachable.store(next.reachable, Ordering::Relaxed);
        state = next;
    }
}

impl std::fmt::Debug for ExternalDatabases {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

/// A warning for each database with `tls = "off"` whose host is not this machine: its password
/// and data cross the network in the clear. Said at startup and by the configuration check.
pub fn warn_about_plain(settings: &BTreeMap<String, DatabaseSettings>) {
    for (name, settings) in settings.iter().filter(|(_, settings)| settings.tls == DatabaseTls::Off && !is_loopback(&settings.host)) {
        tracing::warn!("databases.{name}: tls = \"off\" sends the password and the data to {} unencrypted", settings.host);
    }
}

/// Whether `host` is this machine, so that a connection to it without TLS stays on it.
fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

impl ExternalDatabase {
    fn open(name: &str, settings: &DatabaseSettings) -> Self {
        let (pool, options) = match settings.kind {
            DatabaseKind::Postgresql => {
                // Not `new`: it reads `~/.pgpass`. The variables `new_without_pgpass` reads are
                // overridden below wherever the configuration says something.
                let mut options = sqlx::postgres::PgConnectOptions::new_without_pgpass().host(&settings.host).port(settings.port).database(&settings.database).username(&settings.user);
                if let Some(password) = &settings.password {
                    options = options.password(password);
                }
                options = match settings.tls {
                    DatabaseTls::Verify => options.ssl_mode(sqlx::postgres::PgSslMode::VerifyFull),
                    DatabaseTls::Off => options.ssl_mode(sqlx::postgres::PgSslMode::Disable),
                };
                if let Some(ca_file) = &settings.ca_file {
                    options = options.ssl_root_cert(ca_file);
                }
                let options = options.disable_statement_logging();
                (Pool::Postgres(pool_options(settings).connect_lazy_with(options.clone())), Options::Postgres(options))
            }
            DatabaseKind::Mysql => {
                let mut options = sqlx::mysql::MySqlConnectOptions::new().host(&settings.host).port(settings.port).database(&settings.database).username(&settings.user);
                if let Some(password) = &settings.password {
                    options = options.password(password);
                }
                options = match settings.tls {
                    DatabaseTls::Verify => options.ssl_mode(sqlx::mysql::MySqlSslMode::VerifyIdentity),
                    DatabaseTls::Off => options.ssl_mode(sqlx::mysql::MySqlSslMode::Disabled),
                };
                if let Some(ca_file) = &settings.ca_file {
                    options = options.ssl_ca(ca_file);
                }
                let options = options.disable_statement_logging();
                (Pool::Mysql(pool_options(settings).connect_lazy_with(options.clone())), Options::Mysql(options))
            }
        };
        Self {
            name: name.to_string(),
            kind: settings.kind,
            pool,
            pool_size: settings.pool_size,
            failures: FailureCounts::default(),
            options,
            connect_timeout: settings.connect_timeout,
            required: settings.required_for_ready,
            reachable: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// For a database with `required_for_ready`, whether its readiness check last found it
    /// reachable; `None` for the others, which are not checked.
    pub fn reachable(&self) -> Option<bool> {
        self.required.then(|| self.reachable.load(Ordering::Relaxed))
    }

    /// Runs `SELECT 1` on `connection`, connecting first when there is none. A failure leaves
    /// none, so that the next check connects again.
    async fn check(&self, connection: &mut Option<CheckConnection>) -> Result<(), sqlx::Error> {
        use sqlx::ConnectOptions;
        let result = async {
            if connection.is_none() {
                *connection = Some(match &self.options {
                    Options::Mysql(options) => CheckConnection::Mysql(options.connect().await?),
                    Options::Postgres(options) => CheckConnection::Postgres(options.connect().await?),
                });
            }
            match connection {
                Some(CheckConnection::Mysql(open)) => sqlx::query("SELECT 1").execute(&mut *open).await.map(drop),
                Some(CheckConnection::Postgres(open)) => sqlx::query("SELECT 1").execute(&mut *open).await.map(drop),
                None => Ok(()),
            }
        }
        .await;
        if result.is_err() {
            *connection = None;
        }
        result
    }

    /// The failed calls since the process started, by class.
    pub fn failures(&self) -> FailureTotals {
        FailureTotals {
            unavailable: self.failures.unavailable.load(Ordering::Relaxed),
            mistake: self.failures.mistake.load(Ordering::Relaxed),
            unknown: self.failures.unknown.load(Ordering::Relaxed),
        }
    }

    /// Counts a call its caller gave up on for want of time, which counts as the database being
    /// unavailable.
    pub fn note_timeout(&self) {
        self.failures.unavailable.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a call refused before it was sent, as the caller's mistake.
    pub fn note_refused(&self) {
        self.failures.mistake.fetch_add(1, Ordering::Relaxed);
    }

    /// The pool's connections now. The two numbers sqlx gives are read one after the other, so a
    /// connection handed out between them is not counted as in use rather than counted twice.
    pub fn connections(&self) -> Connections {
        let (size, idle) = match &self.pool {
            Pool::Mysql(pool) => (pool.size() as usize, pool.num_idle()),
            Pool::Postgres(pool) => (pool.size() as usize, pool.num_idle()),
        };
        Connections { in_use: size.saturating_sub(idle), idle, max: self.pool_size }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> DatabaseKind {
        self.kind
    }

    /// The rows `sql` returns, with `params` bound to its `?` placeholders in order. The caller
    /// checks the number of values and bounds the time.
    pub async fn query(&self, sql: &str, params: &[hiqlite::Param]) -> Result<Vec<DbRow>, DbFailure> {
        // The SQL is the configuration's or the script's, as with `sql_query`; values are bound.
        let statement = statement_for(self.kind, sql).map_err(|e| {
            self.note_refused();
            DbFailure::mistake(e)
        })?;
        match &self.pool {
            Pool::Postgres(pool) => {
                let mut connection = pool.acquire().await.map_err(|e| self.failure(&e, true))?;
                let rows = bind_postgres(sqlx::query(sqlx::AssertSqlSafe(statement.as_str())), params).fetch_all(&mut *connection).await.map_err(|e| self.failure(&e, false))?;
                rows.iter().map(|row| self.row(row, POSTGRES)).collect()
            }
            Pool::Mysql(pool) => {
                let mut connection = pool.acquire().await.map_err(|e| self.failure(&e, true))?;
                let rows = bind_mysql(sqlx::query(sqlx::AssertSqlSafe(statement.as_str())), params).fetch_all(&mut *connection).await.map_err(|e| self.failure(&e, false))?;
                rows.iter().map(|row| self.row(row, MYSQL)).collect()
            }
        }
    }

    /// How many rows `sql` changed, with `params` bound as for `query`.
    pub async fn execute(&self, sql: &str, params: &[hiqlite::Param]) -> Result<u64, DbFailure> {
        // The SQL is the configuration's or the script's, as with `sql_query`; values are bound.
        let statement = statement_for(self.kind, sql).map_err(|e| {
            self.note_refused();
            DbFailure::mistake(e)
        })?;
        match &self.pool {
            Pool::Postgres(pool) => {
                let mut connection = pool.acquire().await.map_err(|e| self.failure(&e, true))?;
                let done = bind_postgres(sqlx::query(sqlx::AssertSqlSafe(statement.as_str())), params).execute(&mut *connection).await.map_err(|e| self.failure(&e, false))?;
                Ok(done.rows_affected())
            }
            Pool::Mysql(pool) => {
                let mut connection = pool.acquire().await.map_err(|e| self.failure(&e, true))?;
                let done = bind_mysql(sqlx::query(sqlx::AssertSqlSafe(statement.as_str())), params).execute(&mut *connection).await.map_err(|e| self.failure(&e, false))?;
                Ok(done.rows_affected())
            }
        }
    }

    /// `error` as a failure of this database; `connecting` when it came while getting a
    /// connection, which is never the statement's fault.
    fn failure(&self, error: &sqlx::Error, connecting: bool) -> DbFailure {
        let message = format!("database {}: {error}", self.name);
        let class = classify(error, connecting);
        let count = match class {
            Class::Unavailable => &self.failures.unavailable,
            Class::Mistake => &self.failures.mistake,
            Class::Unknown => &self.failures.unknown,
        };
        count.fetch_add(1, Ordering::Relaxed);
        match class {
            Class::Unavailable => DbFailure::database_unavailable(message),
            Class::Mistake => DbFailure::mistake(message),
            Class::Unknown => DbFailure::other(message),
        }
    }

    fn row<R: Row>(&self, row: &R, driver: Driver<R>) -> Result<DbRow, DbFailure>
    where
        usize: sqlx::ColumnIndex<R>,
        for<'r> i8: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
        for<'r> i16: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
        for<'r> i32: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
        for<'r> i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
        for<'r> f32: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
        for<'r> f64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
        for<'r> bool: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
        for<'r> String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
        for<'r> Vec<u8>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    {
        let mut values = Vec::with_capacity(row.columns().len());
        for (index, column) in row.columns().iter().enumerate() {
            let name = column.name();
            let raw = row.try_get_raw(index).map_err(|e| self.failure(&e, false))?;
            let value = if raw.is_null() { DbValue::Null } else { self.value(row, index, name, &raw.type_info(), driver)? };
            values.push((name.to_string(), value));
        }
        Ok(values)
    }

    /// The value of a column that is not NULL: the first type it decodes as, in the order of the
    /// spec's list. Each type is tried only when it fits the column's type, so a column does not
    /// build an error for every type it is not.
    fn value<R: Row>(&self, row: &R, index: usize, name: &str, column: &<R::Database as sqlx::Database>::TypeInfo, driver: Driver<R>) -> Result<DbValue, DbFailure>
    where
        usize: sqlx::ColumnIndex<R>,
        for<'r> i8: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
        for<'r> i16: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
        for<'r> i32: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
        for<'r> i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
        for<'r> f32: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
        for<'r> f64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
        for<'r> bool: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
        for<'r> String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
        for<'r> Vec<u8>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    {
        if let Some(value) = decoded::<i64, R>(row, index, column) {
            return Ok(DbValue::Integer(value));
        }
        if let Some(value) = decoded::<i32, R>(row, index, column) {
            return Ok(DbValue::Integer(i64::from(value)));
        }
        if let Some(value) = decoded::<i16, R>(row, index, column) {
            return Ok(DbValue::Integer(i64::from(value)));
        }
        if let Some(value) = decoded::<i8, R>(row, index, column) {
            return Ok(DbValue::Integer(i64::from(value)));
        }
        // Before bool, which takes a MySQL column of any integer type and gives 1 for 5.
        if let Some(value) = (driver.unsigned)(row, index) {
            let value = value.map_err(|too_big| DbFailure::mistake(format!("database {}: column {name} holds {too_big}, more than a script number can hold; CAST it to text in the SQL", self.name)))?;
            return Ok(DbValue::Integer(value));
        }
        if let Some(value) = decoded::<f64, R>(row, index, column) {
            return Ok(DbValue::Real(value));
        }
        if let Some(value) = decoded::<f32, R>(row, index, column) {
            return Ok(DbValue::Real(f64::from(value)));
        }
        if let Some(value) = decoded::<bool, R>(row, index, column) {
            return Ok(DbValue::Integer(i64::from(value)));
        }
        if let Some(value) = decoded::<String, R>(row, index, column) {
            return Ok(DbValue::Text(value));
        }
        if let Some(value) = decoded::<Vec<u8>, R>(row, index, column) {
            return Ok(DbValue::Blob(value));
        }
        // What SUM and AVG give: exact, so as text rather than as a number that would round it.
        if let Some(value) = (driver.decimal)(row, index) {
            return value.map(DbValue::Text).map_err(|reason| DbFailure::mistake(format!("database {}: column {name}: {reason}", self.name)));
        }
        // The same statement would find the same type again: the SQL's mistake.
        Err(DbFailure::mistake(format!("database {}: column {name} has a type that cannot be read; CAST it to text in the SQL", self.name)))
    }
}

/// The column's value as a `T`, when `T` fits the column's type and the value decodes.
fn decoded<T, R: Row>(row: &R, index: usize, column: &<R::Database as sqlx::Database>::TypeInfo) -> Option<T>
where
    usize: sqlx::ColumnIndex<R>,
    for<'r> T: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    if !<T as sqlx::Type<R::Database>>::compatible(column) {
        return None;
    }
    // The check `try_get` makes, made above without building an error when it fails.
    row.try_get_unchecked::<T, _>(index).ok()
}

/// What one driver reads beyond the types both have.
struct Driver<R> {
    /// A column of an unsigned integer type, or none: `Err` holds a value too big for an `i64`.
    unsigned: fn(&R, usize) -> Option<Result<i64, u64>>,
    /// A column of an exact decimal type as its text, or none: `Err` says why it cannot be read.
    decimal: fn(&R, usize) -> Option<Result<String, String>>,
}

impl<R> Clone for Driver<R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<R> Copy for Driver<R> {}

const POSTGRES: Driver<sqlx::postgres::PgRow> = Driver { unsigned: |_, _| None, decimal: postgres_numeric };
const MYSQL: Driver<sqlx::mysql::MySqlRow> = Driver { unsigned: mysql_unsigned, decimal: mysql_decimal };

/// MySQL's `UNSIGNED` columns, which its signed types refuse.
fn mysql_unsigned(row: &sqlx::mysql::MySqlRow, index: usize) -> Option<Result<i64, u64>> {
    let column = row.try_get_raw(index).ok()?.type_info().into_owned();
    if !<u64 as sqlx::Type<sqlx::MySql>>::compatible(&column) {
        return None;
    }
    let value = row.try_get_unchecked::<u64, _>(index).ok()?;
    Some(i64::try_from(value).map_err(|_| value))
}

/// MySQL's `DECIMAL`, which it sends as text.
fn mysql_decimal(row: &sqlx::mysql::MySqlRow, index: usize) -> Option<Result<String, String>> {
    let column = row.try_get_raw(index).ok()?.type_info().into_owned();
    if sqlx::TypeInfo::name(&column) != "DECIMAL" {
        return None;
    }
    // `String` refuses the type but reads the text it comes as.
    Some(row.try_get_unchecked::<String, _>(index).map_err(|e| e.to_string()))
}

/// PostgreSQL's `NUMERIC`, which it sends in its binary form.
fn postgres_numeric(row: &sqlx::postgres::PgRow, index: usize) -> Option<Result<String, String>> {
    let raw = row.try_get_raw(index).ok()?;
    if sqlx::TypeInfo::name(&*raw.type_info()) != "NUMERIC" {
        return None;
    }
    let text = match raw.format() {
        sqlx::postgres::PgValueFormat::Text => raw.as_str().map(str::to_string).map_err(|e| e.to_string()),
        sqlx::postgres::PgValueFormat::Binary => raw.as_bytes().map_err(|e| e.to_string()).and_then(numeric_text),
    };
    Some(text)
}

/// The text of a `NUMERIC` in PostgreSQL's binary form: four 16-bit fields (the number of
/// digits, the weight of the first, the sign, the digits after the point to show), then the
/// digits, each 0 to 9999 in base 10000, the first one weighing 10000^weight.
fn numeric_text(bytes: &[u8]) -> Result<String, String> {
    let field = |at: usize| -> Result<u16, String> {
        let pair = bytes.get(at..at + 2).ok_or("NUMERIC value is cut short")?;
        Ok(u16::from_be_bytes([pair[0], pair[1]]))
    };
    let count = usize::from(field(0)?);
    let weight = i32::from(field(2)? as i16);
    let sign = field(4)?;
    let scale = usize::from(field(6)?);
    match sign {
        0x0000 | 0x4000 => {}
        0xC000 => return Ok("NaN".to_string()),
        0xD000 => return Ok("Infinity".to_string()),
        0xF000 => return Ok("-Infinity".to_string()),
        _ => return Err(format!("NUMERIC value has unknown sign {sign:#06x}")),
    }
    if bytes.len() != 8 + count * 2 {
        return Err("NUMERIC value has the wrong length".to_string());
    }
    let digits = (0..count).map(|i| field(8 + i * 2)).collect::<Result<Vec<u16>, String>>()?;
    if digits.iter().any(|digit| *digit > 9999) {
        return Err("NUMERIC value has a digit over 9999".to_string());
    }
    // Base-10000 digit at position `p` (0 = units, 1 = ten-thousands, -1 = the first four places
    // after the point) is `digits[weight - p]`.
    let digit = |p: i32| -> u16 { usize::try_from(weight - p).ok().and_then(|i| digits.get(i).copied()).unwrap_or(0) };
    let mut text = String::new();
    if sign == 0x4000 {
        text.push('-');
    }
    if weight < 0 {
        text.push('0');
    } else {
        text.push_str(&digit(weight).to_string());
        for p in (0..weight).rev() {
            text.push_str(&format!("{:04}", digit(p)));
        }
    }
    if scale > 0 {
        text.push('.');
        let mut fraction = String::new();
        let mut p = -1;
        while fraction.len() < scale {
            fraction.push_str(&format!("{:04}", digit(p)));
            p -= 1;
        }
        fraction.truncate(scale);
        text.push_str(&fraction);
    }
    Ok(text)
}

fn pool_options<DB: sqlx::Database>(settings: &DatabaseSettings) -> sqlx::pool::PoolOptions<DB> {
    // `pool_size` is at least 1 and a configuration would not hold four billion.
    let size = u32::try_from(settings.pool_size).unwrap_or(u32::MAX);
    // Waiting for a connection, which takes opening one when none is free, is bounded by the
    // connect timeout as well as by the caller's.
    // No ping before each use: it would double the round trips of every query, and a connection
    // that died while idle fails its one call with an I/O error, which counts as unavailable.
    sqlx::pool::PoolOptions::new().max_connections(size).min_connections(0).acquire_timeout(settings.connect_timeout).test_before_acquire(false)
}

fn bind_postgres<'q>(mut query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>, params: &'q [hiqlite::Param]) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    for param in params {
        query = match param {
            hiqlite::Param::Null => query.bind(None::<String>),
            hiqlite::Param::Integer(value) => query.bind(*value),
            hiqlite::Param::Real(value) => query.bind(*value),
            hiqlite::Param::Text(value) => query.bind(value.as_str()),
            hiqlite::Param::Blob(value) => query.bind(value.as_slice()),
            // What hiqlite passes between the statements of one transaction; scripts never make it.
            hiqlite::Param::StmtOutputIndexed(..) | hiqlite::Param::StmtOutputNamed(..) => query.bind(None::<String>),
        };
    }
    query
}

fn bind_mysql<'q>(mut query: sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>, params: &'q [hiqlite::Param]) -> sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments> {
    for param in params {
        query = match param {
            hiqlite::Param::Null => query.bind(None::<String>),
            hiqlite::Param::Integer(value) => query.bind(*value),
            hiqlite::Param::Real(value) => query.bind(*value),
            hiqlite::Param::Text(value) => query.bind(value.as_str()),
            hiqlite::Param::Blob(value) => query.bind(value.as_slice()),
            // What hiqlite passes between the statements of one transaction; scripts never make it.
            hiqlite::Param::StmtOutputIndexed(..) | hiqlite::Param::StmtOutputNamed(..) => query.bind(None::<String>),
        };
    }
    query
}

/// The statement as sent to a database of `kind`: for PostgreSQL the n-th `?` becomes `$n`, for
/// MySQL nothing changes. A `?` in text, a quoted name or a comment is left alone, and numbered
/// or named placeholders are refused, as `sql_query` refuses them.
pub fn statement_for(kind: DatabaseKind, sql: &str) -> Result<String, String> {
    let placeholders = crate::lua_engine::sql_placeholders(sql)?;
    if let Some(numbered) = placeholders.iter().find(|placeholder| placeholder.number.is_some()) {
        let form: String = sql[numbered.at..].chars().take_while(|c| *c == '?' || c.is_ascii_digit()).collect();
        return Err(format!("only plain ? placeholders are supported, found {form}"));
    }
    if kind == DatabaseKind::Mysql {
        return Ok(sql.to_string());
    }
    let mut statement = String::with_capacity(sql.len() + placeholders.len() * 2);
    let mut from = 0;
    for (index, placeholder) in placeholders.iter().enumerate() {
        statement.push_str(&sql[from..placeholder.at]);
        statement.push('$');
        statement.push_str(&(index + 1).to_string());
        from = placeholder.at + 1;
    }
    statement.push_str(&sql[from..]);
    Ok(statement)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Unavailable,
    Mistake,
    Unknown,
}

/// Whether trying later could help. Getting a connection fails only for reasons of the
/// connection: the server is down, refuses the account or presents a certificate that does not
/// verify. An error the server reports for the statement is the statement's, except those that
/// say the server cannot take it now.
fn classify(error: &sqlx::Error, connecting: bool) -> Class {
    use sqlx::Error as E;
    if connecting {
        return Class::Unavailable;
    }
    match error {
        E::Io(_) | E::Tls(_) | E::Protocol(_) | E::PoolTimedOut | E::PoolClosed | E::WorkerCrashed | E::BeginFailed => Class::Unavailable,
        E::Database(database) if busy(database.as_ref()) => Class::Unavailable,
        E::Database(_) | E::Encode(_) | E::InvalidArgument(_) => Class::Mistake,
        _ => Class::Unknown,
    }
}

/// MySQL errors that mean the server cannot take the statement now: too many connections, a
/// shutdown, a lock wait timeout and a deadlock.
const MYSQL_BUSY: [u16; 5] = [1040, 1053, 1205, 1213, 1317];

/// Whether the server reported that it cannot take the statement now, rather than refusing it:
/// PostgreSQL's SQLSTATE classes 08 (connection), 53 (resources) and 57 (operator intervention),
/// serialization failures and deadlocks, and the MySQL errors of `MYSQL_BUSY`.
fn busy(error: &dyn sqlx::error::DatabaseError) -> bool {
    if let Some(mysql) = error.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>() {
        return MYSQL_BUSY.contains(&mysql.number());
    }
    let Some(code) = error.code() else {
        return false;
    };
    ["08", "53", "57"].iter().any(|class| code.starts_with(class)) || code == "40001" || code == "40P01"
}

/// A database's settings and a port nothing listens on, for tests elsewhere too.
#[cfg(test)]
pub(crate) mod test_support {
    use crate::config::{DatabaseKind, DatabaseSettings, DatabaseTls};
    use std::time::Duration;

    pub(crate) fn settings(kind: DatabaseKind, host: &str, port: u16, tls: DatabaseTls) -> DatabaseSettings {
        DatabaseSettings { user: "rradius".to_string(), pool_size: 2, connect_timeout: Duration::from_millis(300), ..crate::test_support::database_settings(kind, host, port, "rradius", "pw", tls, None) }
    }

    /// A port that accepts connections and never answers on them.
    pub(crate) async fn silent_port() -> Result<u16, String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        Ok(port)
    }

    /// A port nothing listens on.
    pub(crate) fn closed_port() -> Result<u16, String> {
        // One that no test of this or another process is handed, so nothing listens on it.
        Ok(crate::test_support::server::free_port())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::test_support::{closed_port, settings};

    /// spec Database not reachable at startup: opening does not connect, and a call through it
    /// fails as the database being unavailable.
    #[tokio::test]
    async fn test_open_without_the_database() -> Result<(), String> {
        let port = closed_port()?;
        let mut all = BTreeMap::new();
        all.insert("pg".to_string(), settings(DatabaseKind::Postgresql, "127.0.0.1", port, DatabaseTls::Off));
        all.insert("my".to_string(), settings(DatabaseKind::Mysql, "127.0.0.1", port, DatabaseTls::Off));
        let databases = ExternalDatabases::open(&all);
        assert_eq!(databases.names().collect::<Vec<_>>(), ["my", "pg"]);
        for name in ["pg", "my"] {
            let database = databases.get(name).ok_or("missing")?;
            let failure = database.query("SELECT 1", &[]).await.err().ok_or("it answered")?;
            assert!(failure.unavailable(), "{name}: {failure}");
            assert!(failure.to_string().contains(name), "{failure}");
            let failure = database.execute("DELETE FROM t", &[]).await.err().ok_or("it answered")?;
            assert!(failure.unavailable(), "{name}: {failure}");
            assert_eq!(database.failures(), FailureTotals { unavailable: 2, mistake: 0, unknown: 0 }, "{name}");
        }
        Ok(())
    }

    /// spec Failures by class: each failed call counts once under its class, a refused statement
    /// and a call its caller gave up on included.
    #[tokio::test]
    async fn test_failure_counts() -> Result<(), String> {
        let mut all = BTreeMap::new();
        all.insert("billing".to_string(), settings(DatabaseKind::Postgresql, "127.0.0.1", closed_port()?, DatabaseTls::Off));
        let databases = ExternalDatabases::open(&all);
        let database = databases.get("billing").ok_or("missing")?;
        assert_eq!(database.failures(), FailureTotals::default());
        let refused = database.query("SELECT $1", &[]).await.err().ok_or("it was sent")?;
        assert!(!refused.unavailable(), "{refused}");
        assert_eq!(database.failures(), FailureTotals { unavailable: 0, mistake: 1, unknown: 0 });
        drop(database.failure(&sqlx::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)), false));
        drop(database.failure(&sqlx::Error::InvalidArgument("x".to_string()), false));
        drop(database.failure(&sqlx::Error::RowNotFound, false));
        database.note_timeout();
        database.note_refused();
        assert_eq!(database.failures(), FailureTotals { unavailable: 2, mistake: 3, unknown: 1 });
        Ok(())
    }

    /// spec reachability: the example's outcomes in order.
    #[test]
    fn test_reachability() {
        let after = |checks: &[bool]| checks.iter().fold(Reachability::default(), |state, ok| state.after(*ok)).reachable;
        for (checks, reachable) in [
            (&[][..], false),
            (&[false][..], false),
            (&[true][..], true),
            (&[true, false][..], true),
            (&[true, false, false][..], false),
            (&[true, false, false, true][..], true),
        ] {
            assert_eq!(after(checks), reachable, "{checks:?}");
        }
    }

    /// spec Not required and Check outcomes: only a required database is checked, a database
    /// that cannot be reached stays unreachable, and the checks are not counted as failures.
    #[tokio::test]
    async fn test_checks() -> Result<(), String> {
        let port = closed_port()?;
        let mut required = settings(DatabaseKind::Postgresql, "127.0.0.1", port, DatabaseTls::Off);
        required.required_for_ready = true;
        let mut all = BTreeMap::new();
        all.insert("billing".to_string(), required);
        all.insert("legacy".to_string(), settings(DatabaseKind::Mysql, "127.0.0.1", port, DatabaseTls::Off));
        let databases = ExternalDatabases::open(&all);
        let (billing, legacy) = (databases.get("billing").ok_or("missing")?, databases.get("legacy").ok_or("missing")?);
        assert_eq!((billing.reachable(), legacy.reachable()), (Some(false), None), "unreachable until a check succeeds");
        let mut connection = None;
        assert!(billing.check(&mut connection).await.is_err());
        assert!(connection.is_none(), "the next check connects again");
        let logged = crate::test_support::radius::logged_during(async {
            databases.start_checks();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        })
        .await;
        assert_eq!((billing.reachable(), legacy.reachable()), (Some(false), None));
        assert!(logged.contains("WARN") && logged.contains("databases.billing") && !logged.contains("databases.legacy"), "{logged}");
        assert_eq!((billing.failures(), legacy.failures()), (FailureTotals::default(), FailureTotals::default()), "checks are not calls");
        Ok(())
    }

    /// spec Before any call: nothing open or in use, and the most the pool keeps.
    #[tokio::test]
    async fn test_connections_before_any_call() -> Result<(), String> {
        let mut all = BTreeMap::new();
        all.insert("billing".to_string(), settings(DatabaseKind::Mysql, "127.0.0.1", closed_port()?, DatabaseTls::Off));
        let databases = ExternalDatabases::open(&all);
        let database = databases.get("billing").ok_or("missing")?;
        assert_eq!(database.connections(), Connections { in_use: 0, idle: 0, max: 2 });
        Ok(())
    }

    /// spec Plain connection to a remote host: a warning for it, none for this machine.
    #[tokio::test]
    async fn test_warning_without_tls() {
        let open = |host: &str| {
            let mut all = BTreeMap::new();
            all.insert("billing".to_string(), settings(DatabaseKind::Postgresql, host, 5432, DatabaseTls::Off));
            ExternalDatabases::open(&all)
        };
        let remote = crate::test_support::radius::logged_during(async { drop(open("db.example.com")) }).await;
        assert!(remote.contains("WARN") && remote.contains("databases.billing") && remote.contains("db.example.com"), "{remote}");
        for host in ["127.0.0.1", "localhost", "::1", "[::1]"] {
            let local = crate::test_support::radius::logged_during(async { drop(open(host)) }).await;
            assert!(!local.contains("WARN"), "{host}: {local}");
        }
        let verified = crate::test_support::radius::logged_during(async {
            let mut all = BTreeMap::new();
            all.insert("billing".to_string(), settings(DatabaseKind::Postgresql, "db.example.com", 5432, DatabaseTls::Verify));
            drop(ExternalDatabases::open(&all))
        })
        .await;
        assert!(!verified.contains("WARN"), "{verified}");
    }

    /// spec rewriting by kind, and a `?` in text, a quoted name or a comment left alone.
    #[test]
    fn test_statement_for() -> Result<(), String> {
        use DatabaseKind::{Mysql, Postgresql};
        for (kind, sql, sent) in [
            (Postgresql, "SELECT a FROM t WHERE x = ? AND y = ?", "SELECT a FROM t WHERE x = $1 AND y = $2"),
            (Postgresql, "SELECT a FROM t -- why?\nWHERE x = ?", "SELECT a FROM t -- why?\nWHERE x = $1"),
            (Mysql, "SELECT a FROM t WHERE x = ? AND y = ?", "SELECT a FROM t WHERE x = ? AND y = ?"),
            (Postgresql, "SELECT '?' AS mark, plan FROM users WHERE username = ? AND realm = ?", "SELECT '?' AS mark, plan FROM users WHERE username = $1 AND realm = $2"),
            (Postgresql, "SELECT \"a?\" /* ? */ FROM t WHERE x = ?", "SELECT \"a?\" /* ? */ FROM t WHERE x = $1"),
            (Postgresql, "SELECT 'it''s?' FROM t", "SELECT 'it''s?' FROM t"),
            (Postgresql, "SELECT a::text FROM t WHERE x = ?::inet", "SELECT a::text FROM t WHERE x = $1::inet"),
        ] {
            assert_eq!(statement_for(kind, sql)?, sent, "{sql}");
        }
        for refused in ["SELECT ?1", "SELECT :name", "SELECT $name", "SELECT $1"] {
            for kind in [Mysql, Postgresql] {
                let error = statement_for(kind, refused).err().ok_or(refused)?;
                assert!(error.contains("only plain ? placeholders"), "{refused}: {error}");
            }
        }
        Ok(())
    }

    /// One error of each class.
    #[test]
    fn test_classify() {
        use sqlx::Error as E;
        let io = || E::Io(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"));
        assert_eq!(classify(&io(), false), Class::Unavailable);
        assert_eq!(classify(&E::PoolTimedOut, false), Class::Unavailable);
        assert_eq!(classify(&E::PoolClosed, false), Class::Unavailable);
        assert_eq!(classify(&E::Tls("bad certificate".into()), false), Class::Unavailable);
        assert_eq!(classify(&E::Protocol("unexpected".to_string()), false), Class::Unavailable);
        assert_eq!(classify(&E::Encode("too long".into()), false), Class::Mistake);
        assert_eq!(classify(&E::Decode("bad".into()), false), Class::Unknown);
        assert_eq!(classify(&E::RowNotFound, false), Class::Unknown);
        // While connecting, everything is the connection's.
        assert_eq!(classify(&E::Encode("too long".into()), true), Class::Unavailable);
    }

    /// PostgreSQL's binary NUMERIC: count, weight, sign, scale, then base-10000 digits.
    #[test]
    fn test_numeric_text() -> Result<(), String> {
        let numeric = |weight: i16, sign: u16, scale: u16, digits: &[u16]| -> Vec<u8> {
            let count = u16::try_from(digits.len()).unwrap_or(u16::MAX);
            let mut bytes = Vec::new();
            for field in [count, weight as u16, sign, scale].iter().chain(digits) {
                bytes.extend_from_slice(&field.to_be_bytes());
            }
            bytes
        };
        assert_eq!(numeric_text(&numeric(1, 0, 3, &[1, 2345, 6780]))?, "12345.678");
        assert_eq!(numeric_text(&numeric(-1, 0, 4, &[12]))?, "0.0012");
        assert_eq!(numeric_text(&numeric(0, 0x4000, 0, &[5]))?, "-5");
        assert_eq!(numeric_text(&numeric(0, 0, 2, &[]))?, "0.00");
        assert_eq!(numeric_text(&numeric(1, 0, 0, &[100]))?, "1000000");
        assert_eq!(numeric_text(&numeric(0, 0xC000, 0, &[]))?, "NaN");
        assert!(numeric_text(&numeric(0, 0, 0, &[10000])).is_err());
        assert!(numeric_text(&[0, 1, 0, 0]).is_err());
        Ok(())
    }

    #[test]
    fn test_loopback() {
        for host in ["localhost", "LOCALHOST", "127.0.0.1", "127.1.2.3", "::1", "[::1]"] {
            assert!(is_loopback(host), "{host}");
        }
        for host in ["db.example.com", "10.0.0.1", "localhost.example.com", "::2"] {
            assert!(!is_loopback(host), "{host}");
        }
    }
}
