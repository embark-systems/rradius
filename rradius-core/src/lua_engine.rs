use mlua::{AnyUserData, FromLuaMulti, Function, IntoLuaMulti, Lua, MultiValue, Result as LuaResult, Table, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Instant;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use crate::dynamic_authorization::{DynamicAuthorization, Outcome as DynamicAuthOutcome, RequestType};
use crate::script_http::{url_encode, HttpOptions, Purpose, ScriptHttp};
use crate::config::ClusterSettings;
use hiqlite::rusqlite::types::ValueRef;
use rust_embed::{EmbeddedFile, RustEmbed};
use std::path::Path;
use std::time::Duration;

use crate::common::{AttrValidator, AttrValue, Outcome};

/// Everything one script run sees: `protocol`/`phase` globals and the request attributes.
#[derive(Clone)]
pub struct LuaRequest {
    pub protocol: &'static str,
    pub phase: &'static str,
    pub attributes: Vec<(String, AttrValue)>,
    pub validator: Option<AttrValidator>,
    /// Name of the matched client, exposed to Lua as the `client` global.
    pub client: String,
    /// The chain's account name, exposed to Lua as the `auth_user` global; only a chain sets it.
    pub auth_user: Option<String>,
    /// Exposed to Lua as the `replayed` global: this request comes from the accounting spool.
    pub replayed: bool,
    /// Exposed to Lua as `eap_method`: the EAP method that verified this request.
    pub eap_method: Option<&'static str>,
    /// Exposed to Lua as `tls_subject`: the subject of the certificate EAP-TLS verified.
    pub tls_subject: Option<String>,
    /// Exposed to Lua as the table `tls_subject_attrs`: its attributes, unescaped.
    pub tls_subject_attrs: Option<Vec<(String, String)>>,
    /// Exposed to Lua as `radsec_subject` and the table `radsec_subject_attrs`: the certificate of
    /// the RadSec connection this came over, which says who the NAS is, not who the user is.
    pub radsec_subject: Option<Arc<crate::eap::CertificateSubject>>,
}

impl LuaRequest {
    pub fn new(protocol: &'static str, phase: &'static str, attributes: Vec<(String, AttrValue)>) -> Self {
        Self { protocol, phase, attributes, validator: None, client: String::new(), auth_user: None, replayed: false, eap_method: None, tls_subject: None, tls_subject_attrs: None, radsec_subject: None }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LuaDecision {
    pub outcome: Outcome,
    /// Attributes added with `set_attr`, in call order.
    pub response_attributes: Vec<(String, AttrValue)>,
    /// A database call failed because the database could not be reached or timed out, even
    /// if the script caught the error. Mistakes in the SQL itself do not count.
    pub database_failed: bool,
    /// Running the script again would end the same way: it finished and said no, or its SQL
    /// is wrong. Not set while a failure could have been caused by something passing.
    pub final_result: bool,
}

impl LuaDecision {
    /// True only for `"accept"`; `"ignore"` outside an auth chain counts as reject.
    pub fn accepted(&self) -> bool {
        self.outcome == Outcome::Accept
    }
}

/// One column value as returned by the database.
#[derive(Debug, Clone, PartialEq)]
pub enum DbValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// One result row: (column name, value) in column order.
pub type DbRow = Vec<(String, DbValue)>;

/// One row read generically from the underlying rusqlite row. Local reads hand
/// `Row::Borrowed` to the mapper; `Row::Owned` (remote reads) hides column names.
struct GenericRow(DbRow);

impl<'a, 'r> TryFrom<&'a mut hiqlite::Row<'r>> for GenericRow {
    type Error = hiqlite::Error;

    fn try_from(row: &'a mut hiqlite::Row<'r>) -> Result<Self, Self::Error> {
        let hiqlite::Row::Borrowed(row) = row else {
            return Err(hiqlite::Error::Sqlite("remote rows do not expose column names".into()));
        };
        let stmt = row.as_ref();
        let mut columns = Vec::with_capacity(stmt.column_count());
        for i in 0..stmt.column_count() {
            let name = stmt.column_name(i).map_err(hiqlite::Error::from)?.to_string();
            let value = match row.get_ref(i).map_err(hiqlite::Error::from)? {
                ValueRef::Null => DbValue::Null,
                ValueRef::Integer(v) => DbValue::Integer(v),
                ValueRef::Real(v) => DbValue::Real(v),
                ValueRef::Text(v) => DbValue::Text(String::from_utf8_lossy(v).into_owned()),
                ValueRef::Blob(v) => DbValue::Blob(v.to_vec()),
            };
            columns.push((name, value));
        }
        Ok(GenericRow(columns))
    }
}

/// A certificate's subject attributes as a Lua table: each name holds a list, as names can repeat.
fn subject_attrs_table(lua: &mlua::Lua, attrs: &[(String, String)]) -> mlua::Result<mlua::Table> {
    let table = lua.create_table()?;
    for (name, value) in attrs {
        let values = match table.get::<Option<mlua::Table>>(name.as_str())? {
            Some(values) => values,
            None => {
                let values = lua.create_table()?;
                table.set(name.as_str(), values.clone())?;
                values
            }
        };
        values.push(value.as_str())?;
    }
    Ok(table)
}

/// The listen host as hiqlite expects it: hiqlite appends `:port`, so IPv6
/// addresses need brackets (`[::]:8100`, not `:::8100`).
fn listen_host(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V4(v4) => v4.to_string(),
        std::net::IpAddr::V6(v6) => format!("[{}]", v6),
    }
}

/// The database used by Lua scripts: an embedded hiqlite node whose writes are
/// replicated through Raft. All database access goes through here.
#[derive(Clone)]
pub struct Db {
    client: hiqlite::Client,
    /// Writes go to `write_batches` to be committed with the others waiting at the same time.
    writes: tokio::sync::mpsc::Sender<PendingWrite>,
    /// What `track_cluster` last read of this node's Raft state.
    cluster: Arc<ClusterState>,
    /// The node's data directory, under which hiqlite keeps its backups.
    #[cfg(feature = "backup")]
    data_dir: std::path::PathBuf,
}

/// Writes that may wait for the cluster at once; one more fails as the cluster being unavailable.
/// Without a bound a follower cut off from the leader kept every write in memory until it healed.
const WAITING_WRITES: usize = 16384;

/// How long a transaction handed to the cluster may take before its place is given back. hiqlite
/// waits for a leader without a limit, and the places would stay taken for good.
const BATCH_DEADLINE: Duration = Duration::from_secs(30);

/// How long a node may go without a usable leader before writes fail at once instead of waiting.
const LEADERLESS_WRITES_FAIL_AFTER: Duration = Duration::from_secs(5);

/// A leader that no quorum has acknowledged for longer than this is taken as no leader.
const LEADER_QUORUM_ACK_WITHIN_MS: u64 = 5000;

/// The node's view of the cluster, from its own Raft state: see `track_cluster`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ClusterStatus {
    /// A leader this node can write through.
    pub leader: bool,
    pub is_leader: bool,
    /// The last log entry applied to the local replica.
    pub applied: u64,
    /// How long there has been no usable leader; `None` while there is one.
    pub without_leader: Option<Duration>,
    /// Whether the node has read back its own startup marker: false while it serves from a
    /// replica that may be behind the cluster.
    pub caught_up: bool,
}

/// What `track_cluster` read of one node's Raft state.
#[derive(Default)]
struct ClusterState {
    leader: AtomicBool,
    is_leader: AtomicBool,
    applied: AtomicU64,
    /// Milliseconds after `cluster_epoch()` since when there has been no usable leader, plus one;
    /// zero while there is one.
    leaderless_since: AtomicU64,
    caught_up: AtomicBool,
    /// Why this node's Raft core stopped, once it has: it does not start again by itself.
    failed: parking_lot::Mutex<Option<String>>,
}

impl ClusterState {
    fn status(&self) -> ClusterStatus {
        let since = self.leaderless_since.load(Ordering::Relaxed);
        let without_leader = (since != 0).then(|| cluster_epoch().elapsed().saturating_sub(Duration::from_millis(since - 1)));
        ClusterStatus { leader: self.leader.load(Ordering::Relaxed), is_leader: self.is_leader.load(Ordering::Relaxed), applied: self.applied.load(Ordering::Relaxed), without_leader, caught_up: self.caught_up.load(Ordering::Relaxed) }
    }

    fn note(&self, leader: bool, is_leader: bool, applied: Option<u64>) {
        self.leader.store(leader, Ordering::Relaxed);
        self.is_leader.store(is_leader, Ordering::Relaxed);
        if let Some(applied) = applied {
            self.applied.store(applied, Ordering::Relaxed);
        }
        if leader {
            self.leaderless_since.store(0, Ordering::Relaxed);
        } else if self.leaderless_since.load(Ordering::Relaxed) == 0 {
            self.leaderless_since.store(cluster_epoch().elapsed().as_millis() as u64 + 1, Ordering::Relaxed);
        }
    }
}

fn cluster_epoch() -> Instant {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// The state of the node this process serves for, for `/metrics` and `stopped_without_leader`:
/// see `Db::serve_for`. Tests start nodes of their own, which stay out of it.
static SERVING_NODE: std::sync::OnceLock<Arc<ClusterState>> = std::sync::OnceLock::new();

/// The view of the cluster of the node this process serves for, at most a second old; that of
/// a node with a leader before one is set.
pub fn cluster_status() -> ClusterStatus {
    SERVING_NODE.get().map_or(ClusterStatus { leader: true, caught_up: true, ..Default::default() }, |state| state.status())
}

/// Whether Raft metrics show a leader to write through: a follower that knows one, or a leader
/// that a quorum acknowledged recently, since a leader cut off from the others stays leader.
fn has_usable_leader(is_leader: bool, follows: bool, current_leader: Option<u64>, millis_since_quorum_ack: Option<u64>) -> bool {
    if is_leader {
        millis_since_quorum_ack.is_some_and(|millis| millis <= LEADER_QUORUM_ACK_WITHIN_MS)
    } else {
        follows && current_leader.is_some()
    }
}

/// Reads this node's Raft state every second into `state`.
async fn track_cluster(client: hiqlite::Client, state: Arc<ClusterState>) {
    let mut every = tokio::time::interval(Duration::from_secs(1));
    loop {
        every.tick().await;
        let Some(reading) = read_raft(&client).await else {
            state.note(false, false, None);
            continue;
        };
        // A core that stopped on an error leaves the node answering from a replica that no longer
        // follows the cluster. Seen when a follower that fell behind the leader's snapshot came
        // back: its log could not be read.
        if let Some(fatal) = reading.fatal {
            let mut failed = state.failed.lock();
            if failed.is_none() {
                *failed = Some(fatal);
            }
        }
        state.note(reading.usable_leader, reading.is_leader, reading.applied);
    }
}

/// One reading of a node's Raft state.
struct RaftReading {
    /// A leader to write through, as `has_usable_leader` takes it.
    usable_leader: bool,
    is_leader: bool,
    applied: Option<u64>,
    /// Why the Raft core stopped, if it has.
    fatal: Option<String>,
}

/// Reads `client`'s Raft state: what both `track_cluster` and `Db::wait_usable_leader` go by, so
/// that they agree on what a usable leader is.
async fn read_raft(client: &hiqlite::Client) -> Option<RaftReading> {
    let metrics = client.metrics_db().await.ok()?;
    let (is_leader, follows) = (metrics.state.is_leader(), metrics.state.is_follower() || metrics.state.is_learner());
    Some(RaftReading {
        usable_leader: metrics.running_state.is_ok() && has_usable_leader(is_leader, follows, metrics.current_leader, metrics.millis_since_quorum_ack),
        is_leader,
        applied: metrics.last_applied.map(|log| log.index),
        fatal: metrics.running_state.as_ref().err().map(|fatal| fatal.to_string()),
    })
}

/// `[cluster] stop_without_leader_ms` in milliseconds, or 0 when it is not set. Set once at start,
/// since `[cluster]` changes take a restart.
static STOP_WITHOUT_LEADER_MS: AtomicU64 = AtomicU64::new(0);

/// Sets `[cluster] stop_without_leader_ms` for `stopped_without_leader`.
pub fn set_stop_without_leader(limit: Option<Duration>) {
    STOP_WITHOUT_LEADER_MS.store(limit.map_or(0, |limit| limit.as_millis().max(1) as u64), Ordering::Relaxed);
}

/// Whether authentication should go unanswered: `[cluster] stop_without_leader_ms` is set and
/// the node has gone longer than that without a leader.
pub fn stopped_without_leader() -> bool {
    stops_without_leader(STOP_WITHOUT_LEADER_MS.load(Ordering::Relaxed), &cluster_status())
}

fn stops_without_leader(limit_ms: u64, status: &ClusterStatus) -> bool {
    limit_ms != 0 && status.without_leader.is_some_and(|lasted| lasted > Duration::from_millis(limit_ms))
}

/// The outcome of a queued write: given up after `left` if it has not been handed to the cluster
/// by then, and otherwise waited for, which the batch deadline bounds.
async fn wait_for_write(mut answer: tokio::sync::oneshot::Receiver<Result<usize, DbFailure>>, state: &std::sync::atomic::AtomicU8, left: Duration) -> Result<Result<usize, DbFailure>, TimedOut> {
    let outcome = match tokio::time::timeout(left, &mut answer).await {
        Ok(outcome) => outcome,
        Err(_) => match state.compare_exchange(WAITING, ABANDONED, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return Err(TimedOut),
            // Handed over meanwhile: what the cluster says.
            Err(_) => answer.await,
        },
    };
    Ok(answered(outcome, state))
}

/// What became of a write from its reply: a reply dropped after it was handed over means the
/// cluster gave no outcome by the batch deadline, and it may have committed it.
fn answered(outcome: Result<Result<usize, DbFailure>, tokio::sync::oneshot::error::RecvError>, state: &std::sync::atomic::AtomicU8) -> Result<usize, DbFailure> {
    match outcome {
        Ok(result) => result,
        Err(_) if state.load(Ordering::SeqCst) == HANDED => Err(DbFailure::other(format!("the cluster took the write and said nothing of it within {} s; it may have been committed", BATCH_DEADLINE.as_secs()))),
        Err(_) => Err(DbFailure { message: "the cluster did not take the write in time".to_string(), kind: DbFailureKind::Unavailable }),
    }
}

/// A write refused before it waits, when `status` shows no leader for a while.
fn refused_without_leader(status: &ClusterStatus) -> Option<DbFailure> {
    status.without_leader.filter(|lasted| *lasted > LEADERLESS_WRITES_FAIL_AFTER).map(|lasted| DbFailure {
        message: format!("no leader for {} s", lasted.as_secs()),
        kind: DbFailureKind::Unavailable,
    })
}

/// Queues `write` for `write_batches`, or fails as unavailable when as many are waiting as may.
fn queue_write(writes: &tokio::sync::mpsc::Sender<PendingWrite>, write: PendingWrite) -> Result<(), DbFailure> {
    writes.try_send(write).map_err(|e| match e {
        tokio::sync::mpsc::error::TrySendError::Full(_) => DbFailure { message: format!("{WAITING_WRITES} writes are waiting for the cluster"), kind: DbFailureKind::Unavailable },
        tokio::sync::mpsc::error::TrySendError::Closed(_) => DbFailure { message: "the database writer has stopped".to_string(), kind: DbFailureKind::Unavailable },
    })
}

/// A statement waiting to be committed.
struct PendingWrite {
    sql: String,
    params: Vec<hiqlite::Param>,
    reply: tokio::sync::oneshot::Sender<Result<usize, DbFailure>>,
    /// `WAITING`, then `HANDED` once given to the cluster or `ABANDONED` once its caller gave up:
    /// one value both sides change, so that a write is never both.
    state: Arc<std::sync::atomic::AtomicU8>,
}

/// The states of a `PendingWrite`.
const WAITING: u8 = 0;
const HANDED: u8 = 1;
const ABANDONED: u8 = 2;

impl PendingWrite {
    fn new(sql: String, params: Vec<hiqlite::Param>, reply: tokio::sync::oneshot::Sender<Result<usize, DbFailure>>) -> Self {
        Self { sql, params, reply, state: Arc::new(std::sync::atomic::AtomicU8::new(WAITING)) }
    }

    /// Whether it goes to the cluster now: handed over, unless its caller gave up first or
    /// stopped waiting. One handed over before stays so, as when a batch is split.
    fn hand_over(&self) -> bool {
        if self.reply.is_closed() {
            let _ = self.state.compare_exchange(WAITING, ABANDONED, Ordering::SeqCst, Ordering::SeqCst);
        }
        match self.state.compare_exchange(WAITING, HANDED, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => true,
            Err(state) => state == HANDED,
        }
    }
}

/// A write given up on before it was handed to the cluster: it never will be.
#[derive(Debug)]
pub struct TimedOut;

/// Statements in one transaction at most, and transactions on their way at once. While these are
/// on their way the next writes queue up, so the batches grow with the load.
const MAX_BATCH: usize = 256;
const BATCHES_IN_FLIGHT: usize = 4;

/// Statements committed in a transaction with others, and transactions that failed and were split.
static BATCHED_WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SPLIT_BATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// For `/metrics`: statements committed together with others, and batches that failed and were
/// split to find the statement that failed.
pub fn write_batch_counts() -> (u64, u64) {
    (BATCHED_WRITES.load(Ordering::Relaxed), SPLIT_BATCHES.load(Ordering::Relaxed))
}

/// Whether a statement can share a transaction with others: a plain INSERT, UPDATE, DELETE or
/// REPLACE that returns no rows. In a transaction hiqlite runs a statement that returns rows as a
/// query and counts them, where alone it is an error; and a statement that ends or undoes the
/// transaction would commit or undo the others' work. Those, and anything else, go alone, as
/// every statement did before.
fn can_share_a_transaction(sql: &str) -> bool {
    let tokens = sql_tokens(sql);
    let starts_plainly = matches!(tokens.first(), Some(SqlToken::Name { text, quoted: false }) if ["insert", "update", "delete", "replace"].contains(&text.as_str()));
    let returns_rows = tokens.iter().any(|token| matches!(token, SqlToken::Name { text, quoted: false } if text == "returning"));
    starts_plainly && !returns_rows
}

/// What commits the writes: hiqlite's client, or a stand-in in tests.
trait Committer: Clone + Send + Sync + 'static {
    fn commit_one(&self, sql: String, params: Vec<hiqlite::Param>) -> impl Future<Output = Result<usize, hiqlite::Error>> + Send;
    fn commit_together(&self, statements: Vec<(String, Vec<hiqlite::Param>)>) -> impl Future<Output = Result<Vec<Result<usize, hiqlite::Error>>, hiqlite::Error>> + Send;
}

impl Committer for hiqlite::Client {
    fn commit_one(&self, sql: String, params: Vec<hiqlite::Param>) -> impl Future<Output = Result<usize, hiqlite::Error>> + Send {
        let client = self.clone();
        async move { client.execute(sql, params).await }
    }

    fn commit_together(&self, statements: Vec<(String, Vec<hiqlite::Param>)>) -> impl Future<Output = Result<Vec<Result<usize, hiqlite::Error>>, hiqlite::Error>> + Send {
        let client = self.clone();
        async move { client.txn(statements).await }
    }
}

/// Commits the statements waiting at the same time in one transaction: one Raft entry and one
/// SQLite commit for all of them, where each on its own costs both. hiqlite applies the entries
/// on one thread, so committing one statement at a time limited writes to that thread's speed.
async fn write_batches<C: Committer>(committer: C, mut pending: tokio::sync::mpsc::Receiver<PendingWrite>, deadline: Duration) {
    let in_flight = Arc::new(Semaphore::new(BATCHES_IN_FLIGHT));
    while let Some(first) = pending.recv().await {
        // Waiting here, before taking the rest, lets the batch grow while the others commit.
        let Ok(place) = in_flight.clone().acquire_owned().await else {
            return;
        };
        let mut batch = vec![first];
        while batch.len() < MAX_BATCH {
            match pending.try_recv() {
                Ok(next) => batch.push(next),
                Err(_) => break,
            }
        }
        let committer = committer.clone();
        tokio::spawn(async move {
            // Past the deadline the batch is dropped, and with it the replies: its callers hear
            // that the cluster did not take their writes. hiqlite may still commit what it was
            // handed, as a timed-out call's write may be.
            let _ = tokio::time::timeout(deadline, commit_batch(&committer, batch)).await;
            drop(place);
        });
    }
}

async fn commit_alone<C: Committer>(committer: &C, write: PendingWrite) {
    let result = committer.commit_one(write.sql, write.params).await.map_err(DbFailure::from);
    let _ = write.reply.send(result);
}

fn commit_batch<'a, C: Committer>(committer: &'a C, mut batch: Vec<PendingWrite>) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        // Nobody is waiting for these: their calls timed out and were reported as failed, so
        // writing them now would record what the caller was told did not happen, and the
        // request may well come again. Those kept are handed over now, and their callers wait
        // for what the cluster says. Checked again after each split.
        batch.retain(PendingWrite::hand_over);
        if batch.len() <= 1 {
            if let Some(write) = batch.pop() {
                commit_alone(committer, write).await;
            }
            return;
        }
        let statements: Vec<(String, Vec<hiqlite::Param>)> = batch.iter().map(|write| (write.sql.clone(), write.params.clone())).collect();
        match committer.commit_together(statements).await {
            Ok(results) if results.len() == batch.len() => {
                BATCHED_WRITES.fetch_add(batch.len() as u64, Ordering::Relaxed);
                for (write, result) in batch.into_iter().zip(results) {
                    let _ = write.reply.send(result.map_err(DbFailure::from));
                }
            }
            // Committed, but which result is whose is not known. Never again: that would write twice.
            Ok(results) => {
                let message = format!("a transaction of {} statements answered with {} results", batch.len(), results.len());
                for write in batch {
                    let _ = write.reply.send(Err(DbFailure { message: message.clone(), kind: DbFailureKind::Unknown }));
                }
            }
            Err(error) if DbFailure::from_ref(&error).kind == DbFailureKind::Unavailable => {
                // Smaller batches would wait for the same cluster again.
                let failure = DbFailure::from(error);
                for write in batch {
                    let _ = write.reply.send(Err(DbFailure { message: failure.message.clone(), kind: failure.kind }));
                }
            }
            // A statement failed and the transaction took the others with it. Halves again, so
            // that one failing statement among many costs a few transactions, not one each.
            Err(_) => {
                SPLIT_BATCHES.fetch_add(1, Ordering::Relaxed);
                let second = batch.split_off(batch.len() / 2);
                futures::future::join(commit_batch(committer, batch), commit_batch(committer, second)).await;
            }
        }
    })
}

impl Db {
    /// Starts this process's hiqlite node. On a node's first start this returns only
    /// once it has joined the cluster, however long that takes; use `wait_healthy`
    /// to wait until it can serve requests.
    pub async fn start(settings: &ClusterSettings) -> Result<Self, String> {
        Self::start_with(settings, &crate::extension::ExtensionSettings::default()).await
    }

    /// Like `start`, with what extensions made of their sections: with the `backup` feature,
    /// the `ClusterBackup` of `[cluster.backup]`, if there is one.
    pub async fn start_with(settings: &ClusterSettings, extensions: &crate::extension::ExtensionSettings) -> Result<Self, String> {
        #[cfg_attr(not(feature = "backup"), allow(unused_mut))]
        let mut config = hiqlite::NodeConfig {
            node_id: settings.node_id,
            nodes: settings
                .nodes
                .iter()
                .map(|n| hiqlite::Node { id: n.id, addr_raft: n.raft.clone(), addr_api: n.api.clone() })
                .collect(),
            listen_addr_api: listen_host(settings.listen).into(),
            listen_addr_raft: listen_host(settings.listen).into(),
            data_dir: settings.data_dir.to_string_lossy().into_owned().into(),
            secret_raft: settings.secret_raft.clone(),
            secret_api: settings.secret_api.clone(),
            raft_config: hiqlite::NodeConfig::default_raft_config(settings.snapshot_every),
            read_pool_size: settings.read_connections,
            ..Default::default()
        };
        #[cfg(feature = "backup")]
        crate::backup::configure(&mut config, extensions.get::<crate::backup::ClusterBackup>()).await?;
        #[cfg(not(feature = "backup"))]
        let _ = extensions;
        let client = hiqlite::start_node(config).await.map_err(|e| e.to_string())?;
        let (writes, pending) = tokio::sync::mpsc::channel(WAITING_WRITES);
        tokio::spawn(write_batches(client.clone(), pending, BATCH_DEADLINE));
        // Taken as having a leader until the first reading, as a node that has just started
        // and found one has.
        let cluster = Arc::new(ClusterState::default());
        cluster.note(true, false, None);
        tokio::spawn(track_cluster(client.clone(), cluster.clone()));
        Ok(Self {
            client,
            writes,
            cluster,
            #[cfg(feature = "backup")]
            data_dir: settings.data_dir.clone(),
        })
    }

    /// Asks the cluster for a backup: every node writes a copy of the database, at the same point
    /// of the log, into its `backup_dir`. Returns before the copies are made.
    #[cfg(feature = "backup")]
    pub async fn backup(&self) -> Result<(), String> {
        self.client.backup().await.map_err(|e| e.to_string())
    }

    /// Where hiqlite keeps this node's backups, named `backup_node_<id>_<unix time>.sqlite`.
    #[cfg(feature = "backup")]
    pub fn backup_dir(&self) -> std::path::PathBuf {
        self.data_dir.join("state_machine").join("backups")
    }

    /// Makes this node the one `cluster_status` and `stopped_without_leader` speak of.
    pub fn serve_for(&self) {
        let _ = SERVING_NODE.set(self.cluster.clone());
    }

    /// Why this node's Raft core stopped, if it has. Only starting the node again recovers it.
    pub fn cluster_failure(&self) -> Option<String> {
        self.cluster.failed.lock().clone()
    }

    /// This node's view of the cluster, at most a second old.
    pub fn cluster_status(&self) -> ClusterStatus {
        self.cluster.status()
    }

    /// Waits up to `limit` for a leader this node can write through, as `has_usable_leader` takes
    /// it: hiqlite's own health is not enough, since a node restarted alone takes itself for the
    /// leader. Read from Raft's state each time, not from `track_cluster`'s last reading.
    pub async fn wait_usable_leader(&self, limit: Duration) -> bool {
        let usable = async {
            loop {
                if read_raft(&self.client).await.is_some_and(|reading| reading.usable_leader) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        };
        tokio::time::timeout(limit, usable).await.is_ok()
    }

    /// Notes that this node has caught up with the cluster, for `rradius_cluster_caught_up`.
    pub fn note_caught_up(&self) {
        self.cluster.caught_up.store(true, Ordering::Relaxed);
    }

    /// For a node that serves before catching up: keeps waiting for a leader and writing its
    /// marker until the replica has it, then notes and logs that it has caught up. After a
    /// failed try it waits `limit` before the next, so that a leader that cannot reach a quorum
    /// does not get a write every second.
    pub fn catch_up_in_background(&self, node_id: u64, limit: Duration) {
        let db = self.clone();
        tokio::spawn(async move {
            let mut warned = false;
            loop {
                if !db.wait_usable_leader(limit).await {
                    tracing::debug!("cluster node {node_id} has no usable leader yet to catch up with");
                    continue;
                }
                match db.catch_up(node_id, limit).await {
                    Ok(()) => {
                        db.note_caught_up();
                        tracing::info!("cluster node {node_id} has caught up with the cluster");
                        return;
                    }
                    Err(e) if !warned => {
                        warned = true;
                        tracing::warn!("cluster node {node_id} has a leader but has not caught up yet, trying again: {e}");
                    }
                    Err(e) => tracing::debug!("cluster node {node_id} has not caught up yet: {e}"),
                }
                tokio::time::sleep(limit).await;
            }
        });
    }

    pub async fn wait_healthy(&self, timeout: Duration) -> Result<(), String> {
        self.client.wait_until_healthy_db_timeout(timeout).await.map_err(|e| e.to_string())
    }

    /// Reads the local replica. Writes fail: hiqlite's read connections are read-only.
    pub async fn query(&self, sql: &str) -> Result<Vec<DbRow>, String> {
        self.query_with_params(sql, &[]).await
    }

    /// Like `query`, binding each `?` to the next text parameter instead of
    /// splicing values into the SQL.
    pub async fn query_with_params(&self, sql: &str, params: &[&str]) -> Result<Vec<DbRow>, String> {
        self.query_bound(sql, params.iter().map(|p| hiqlite::Param::Text(p.to_string())).collect()).await.map_err(|failure| failure.message)
    }

    /// Like `query`, with values of any type. hiqlite treats a `?` that has no value as NULL.
    async fn query_bound(&self, sql: &str, params: Vec<hiqlite::Param>) -> Result<Vec<DbRow>, DbFailure> {
        let rows = self.client.query_try_map::<GenericRow, _>(sql.to_string(), params).await.map_err(DbFailure::from)?;
        rows.into_iter().map(|row| row.map(|r| r.0).map_err(DbFailure::from)).collect()
    }

    /// Commits a statement through the cluster leader and returns the number of affected rows.
    pub async fn execute(&self, sql: &str) -> Result<u64, String> {
        self.execute_bound(sql, Vec::new()).await.map_err(|failure| failure.message)
    }

    /// Like `execute`, binding each `?` to the next value.
    async fn execute_bound(&self, sql: &str, params: Vec<hiqlite::Param>) -> Result<u64, DbFailure> {
        if let Some(refused) = refused_without_leader(&self.cluster_status()) {
            return Err(refused);
        }
        // Lua scripts are deployed by administrators and trusted to issue their own SQL.
        if !can_share_a_transaction(sql) {
            let affected = self.client.execute(sql.to_string(), params).await.map_err(DbFailure::from)?;
            return Ok(affected as u64);
        }
        let (reply, answer) = tokio::sync::oneshot::channel();
        let write = PendingWrite::new(sql.to_string(), params, reply);
        let state = write.state.clone();
        queue_write(&self.writes, write)?;
        Ok(answered(answer.await, &state)? as u64)
    }

    /// Like `execute_bound`, giving up after `left` only while the write has not been handed to
    /// the cluster. Handed over, it waits for the cluster to say, for as long as a transaction is
    /// given: given up then, the cluster could still commit what its caller was told failed.
    pub async fn execute_within(&self, sql: &str, params: Vec<hiqlite::Param>, left: Duration) -> Result<Result<u64, DbFailure>, TimedOut> {
        if left.is_zero() {
            return Err(TimedOut);
        }
        if let Some(refused) = refused_without_leader(&self.cluster_status()) {
            return Ok(Err(refused));
        }
        if !can_share_a_transaction(sql) {
            let executed = tokio::time::timeout(left, self.client.execute(sql.to_string(), params)).await.map_err(|_| TimedOut)?;
            return Ok(executed.map(|affected| affected as u64).map_err(DbFailure::from));
        }
        let (reply, answer) = tokio::sync::oneshot::channel();
        let write = PendingWrite::new(sql.to_string(), params, reply);
        let state = write.state.clone();
        if let Err(full) = queue_write(&self.writes, write) {
            return Ok(Err(full));
        }
        Ok(wait_for_write(answer, &state, left).await?.map(|affected| affected as u64))
    }

    /// Commits a statement ending in `RETURNING <column>` through the cluster leader, and returns
    /// that integer: `None` when no row changed. The statement runs once, on the leader, so two
    /// nodes calling this at once see the effect of each other.
    pub async fn execute_returning_integer(&self, sql: &str, column: &str, params: Vec<hiqlite::Param>) -> Result<Option<i64>, DbFailure> {
        if let Some(refused) = refused_without_leader(&self.cluster_status()) {
            return Err(refused);
        }
        let rows = self.client.execute_returning(sql.to_string(), params).await.map_err(DbFailure::from)?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let mut row = row.map_err(DbFailure::from)?;
        row.try_get::<i64>(column).map(Some).map_err(DbFailure::from)
    }

    /// Waits until this node's replica has everything the cluster committed before now: writes a
    /// marker of its own through the leader, then reads the local replica until it is there.
    /// Entries apply in order, so the marker comes after all that was committed before it.
    pub async fn catch_up(&self, node_id: u64, limit: Duration) -> Result<(), String> {
        let mut token = [0u8; 16];
        getrandom::fill(&mut token).map_err(|e| format!("no random marker: {e}"))?;
        let token: String = token.iter().map(|byte| format!("{byte:02x}")).collect();
        let node = node_id.to_string();
        let caught_up = async {
            self.client.execute("CREATE TABLE IF NOT EXISTS rradius_nodes (node INTEGER PRIMARY KEY, token TEXT NOT NULL)".to_string(), Vec::new()).await.map_err(|e| format!("cannot write the marker: {e}"))?;
            self.client
                .execute("INSERT OR REPLACE INTO rradius_nodes (node, token) VALUES (?, ?)".to_string(), vec![hiqlite::Param::Integer(node_id as i64), hiqlite::Param::Text(token.clone())])
                .await
                .map_err(|e| format!("cannot write the marker: {e}"))?;
            loop {
                // An error means the replica does not even have the table yet.
                let rows = self.query_with_params("SELECT token FROM rradius_nodes WHERE node = ?", &[node.as_str()]).await.unwrap_or_default();
                if rows.first().and_then(|row| row.first()).is_some_and(|(_, value)| *value == DbValue::Text(token.clone())) {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::time::timeout(limit, caught_up).await.map_err(|_| format!("this node's replica did not catch up with the cluster within {limit:?}"))?
    }

    /// Like `query_with_params`, saying whether a failure was the cluster's.
    pub async fn query_classified(&self, sql: &str, params: &[&str]) -> Result<Vec<DbRow>, DbFailure> {
        self.query_bound(sql, params.iter().map(|p| hiqlite::Param::Text(p.to_string())).collect()).await
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        self.client.shutdown().await.map_err(|e| e.to_string())
    }

    /// Applies the files that are not applied yet and waits for this node's replica
    /// to have them. See `MigrationError`.
    ///
    /// `wait` limits reaching the cluster; `catch_up` limits each later step (running
    /// the files, reading the record again, this node's replica catching up), starting
    /// when that step does.
    pub async fn migrate(&self, migrations: Migrations, wait: Duration, catch_up: Duration) -> Result<(), MigrationError> {
        let Migrations { dir, files } = migrations;
        let applied = self.applied_migrations(wait).await?;
        // Before hiqlite sees them: it panics on these rather than returning an error.
        check_applied(&files, &applied)?;
        let last_applied = applied.last().map_or(0, |record| record.id);
        if files.last().is_none_or(|file| file.id <= last_applied) {
            if files.len() < applied.len() {
                log_line(tracing::Level::WARN, format!("migrations directory '{}' has {} files, but {} are applied: this node has not been given the newer files", dir.display(), files.len(), applied.len()));
            }
            return self.wait_for_replica(last_applied, catch_up).await;
        }
        let names: Vec<(u32, String)> = files.iter().map(|f| (f.id, f.file_name.clone())).collect();
        let newest = names.last().map_or(last_applied, |(id, _)| *id);
        let Ok(result) = tokio::time::timeout(catch_up, self.migrate_files(files)).await else {
            return Err(MigrationError::Cluster(format!("the migration files did not finish within {catch_up:?}")));
        };
        let Err(error) = result else { return self.wait_for_replica(newest, catch_up).await };
        if !is_sql_error(&error) {
            return Err(MigrationError::Cluster(format!("cannot apply migrations: {error}")));
        }
        // A fresh limit: the time left of `wait` may be gone, and this must not hide the SQL error.
        let recorded = self.applied_migrations(catch_up).await.map(|applied| applied.last().map_or(0, |record| record.id));
        Err(sql_failure(&names, recorded, &error))
    }

    /// Waits until this node's replica has the migration numbered `target`. The
    /// leader having it is not enough: scripts and handlers read the replica.
    async fn wait_for_replica(&self, target: u32, limit: Duration) -> Result<(), MigrationError> {
        if target == 0 {
            return Ok(());
        }
        let reached = async {
            loop {
                // An error means the replica does not even have the table yet.
                let rows = self.query("SELECT MAX(id) AS id FROM _migrations").await.unwrap_or_default();
                if let Some((_, DbValue::Integer(id))) = rows.first().and_then(|row| row.first()) {
                    if *id >= i64::from(target) {
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::time::timeout(limit, reached)
            .await
            .map_err(|_| MigrationError::Cluster(format!("this node's replica did not catch up to migration {target} within {limit:?}")))
    }

    /// Whether this node's replica alone has every file of `migrations` applied, for a node that
    /// has no leader to ask: `Invalid` when a file is recorded under another name or content, as
    /// with a leader, and `Cluster` when a file is not recorded here, since applying it takes one.
    pub async fn check_local_migrations(&self, migrations: &Migrations) -> Result<(), MigrationError> {
        let Some(newest) = migrations.files.last() else { return Ok(()) };
        let read = self.client.query_map::<hiqlite::AppliedMigration, _>("SELECT id, name, ts, hash FROM _migrations ORDER BY id", vec![]).await;
        let applied = match read {
            Ok(applied) => applied,
            // hiqlite creates the table with the first migration.
            Err(e) if e.to_string().contains("no such table") => Vec::new(),
            Err(e) => return Err(MigrationError::Cluster(format!("cannot read the migrations applied on this node: {e}"))),
        };
        check_applied(&migrations.files, &applied)?;
        match migrations.files.iter().find(|file| !applied.iter().any(|record| record.id == file.id)) {
            Some(missing) => Err(MigrationError::Cluster(format!("no leader to apply migration file '{}', which this node has not applied (newest file '{}')", missing.file_name, newest.file_name))),
            None => Ok(()),
        }
    }

    /// What the cluster has applied, in order, asked of the leader: this node's
    /// replica can lag behind.
    async fn applied_migrations(&self, wait: Duration) -> Result<Vec<hiqlite::AppliedMigration>, MigrationError> {
        let sql = "SELECT id, name, ts, hash FROM _migrations ORDER BY id";
        let read = self.client.query_consistent_map::<hiqlite::AppliedMigration, _>(sql, vec![]);
        match tokio::time::timeout(wait, read).await {
            Ok(Ok(applied)) => Ok(applied),
            // hiqlite creates the table with the first migration.
            Ok(Err(e)) if e.to_string().contains("no such table") => Ok(Vec::new()),
            Ok(Err(e)) => Err(MigrationError::Cluster(format!("cannot read applied migrations: {e}"))),
            Err(_) => Err(MigrationError::Cluster(format!("cannot read applied migrations: no answer within {wait:?}"))),
        }
    }

    /// Hands every file to hiqlite, which skips those already applied and runs
    /// each other one in its own transaction, through the cluster.
    ///
    /// Always the whole list: hiqlite panics in its state machine when it gets
    /// fewer files than are applied, which a node with a replica that lags behind
    /// would otherwise send while another node is applying them.
    async fn migrate_files(&self, files: Vec<MigrationFile>) -> Result<(), hiqlite::Error> {
        // `RustEmbed` has only static functions, so the list has to be in a static.
        // hiqlite reads it again when it retries after the leader changed.
        let _only_one_list = MIGRATION_LOCK.lock().await;
        *lock_migration_list() = files.into_iter().map(|f| (f.file_name, f.file)).collect();
        // Also when the caller stops waiting and drops this call.
        struct ClearList;
        impl Drop for ClearList {
            fn drop(&mut self) {
                lock_migration_list().clear();
            }
        }
        let _clear = ClearList;
        self.client.try_migrate::<MigrationFiles>().await
    }
}

/// Why the files in `[cluster].migrations_dir` could not be applied.
#[derive(Debug, PartialEq)]
pub enum MigrationError {
    /// The directory or a file in it is not acceptable; nothing was sent to the cluster.
    Invalid(String),
    /// A file's SQL failed to run; the message names the file.
    Failed(String),
    /// The cluster could not be reached, or an error from it interrupted applying the files.
    Cluster(String),
    /// The node has no leader, or could not catch up through one, and must not serve without:
    /// no migration is involved.
    Unavailable(String),
}

impl std::fmt::Display for MigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) | Self::Failed(message) => f.write_str(message),
            Self::Cluster(message) => write!(f, "cluster was not ready to apply migrations: {message} (the limit is migrations_wait_ms; raise it if a migration file takes longer)"),
            Self::Unavailable(message) => write!(f, "cluster was not ready: {message}"),
        }
    }
}

impl MigrationError {
    /// The server's exit status for this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Invalid(_) | Self::Failed(_) => 2,
            Self::Cluster(_) | Self::Unavailable(_) => 1,
        }
    }
}

/// Refuses files that are recorded as applied under another name or with other content.
fn check_applied(files: &[MigrationFile], applied: &[hiqlite::AppliedMigration]) -> Result<(), MigrationError> {
    for record in applied {
        let Some(file) = files.iter().find(|f| f.id == record.id) else { continue };
        if file.name != record.name {
            return Err(MigrationError::Invalid(format!("migration file '{}' was applied under the name '{}'", file.file_name, record.name)));
        }
        if file.hash() != record.hash {
            return Err(MigrationError::Invalid(format!("migration file '{}' changed after it was applied", file.file_name)));
        }
    }
    Ok(())
}

/// A file in `[cluster].migrations_dir`, as read at startup.
struct MigrationFile {
    id: u32,
    /// The whole file name, such as `1_users.sql`.
    file_name: String,
    /// Without the number and the suffix: what hiqlite records.
    name: String,
    file: EmbeddedFile,
}

impl MigrationFile {
    /// The hash as hiqlite records it.
    fn hash(&self) -> String {
        self.file.metadata.sha256_hash().iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

/// The error when `error`, from a file's SQL, stopped the migrations. Each file is its
/// own transaction, so the first one not recorded is the one that failed; `recorded` is
/// the last one recorded, if that could be read.
fn sql_failure(names: &[(u32, String)], recorded: Result<u32, MigrationError>, error: &impl std::fmt::Display) -> MigrationError {
    MigrationError::Failed(match recorded {
        Ok(last) => match names.iter().find(|(id, _)| *id > last) {
            Some((_, name)) => format!("migration file '{name}' could not be applied: {error}"),
            None => format!("a migration file could not be applied, though every file is recorded as applied: {error}"),
        },
        Err(read) => format!("a migration file could not be applied, and which one could not be identified ({read}): {error}"),
    })
}

/// Whether a migration failed because of its SQL rather than the cluster.
fn is_sql_error(error: &hiqlite::Error) -> bool {
    use hiqlite::Error::{ConstraintViolation, PrepareStatement, QueryParams, Sqlite};
    matches!(error, ConstraintViolation(_) | PrepareStatement(_) | QueryParams(_) | Sqlite(_))
}

/// Splits `<number>_<name>.sql`.
fn parse_migration_name(file_name: &str) -> Option<(u32, &str)> {
    let (id, name) = file_name.strip_suffix(".sql")?.split_once('_')?;
    if name.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((id.parse().ok()?, name))
}

/// The files of a migrations directory, read and checked.
pub struct Migrations {
    dir: std::path::PathBuf,
    files: Vec<MigrationFile>,
}

/// Reads and checks `dir`. Needs no cluster, so a mistake here shows at once.
pub fn read_migrations(dir: &Path) -> Result<Migrations, MigrationError> {
    Ok(Migrations { dir: dir.to_path_buf(), files: read_migration_dir(dir)? })
}

/// Reads every file in `dir`, in order, checking the names and the numbering.
fn read_migration_dir(dir: &Path) -> Result<Vec<MigrationFile>, MigrationError> {
    let invalid = |message: String| MigrationError::Invalid(message);
    let entries = std::fs::read_dir(dir).map_err(|e| invalid(format!("cannot read migrations directory '{}': {e}", dir.display())))?;
    let mut files = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| invalid(format!("cannot read migrations directory '{}': {e}", dir.display())))?.path();
        let file_name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let Some((id, name)) = parse_migration_name(&file_name) else {
            return Err(invalid(format!("migration file '{file_name}' is not named <number>_<name>.sql")));
        };
        let name = name.to_string();
        let file = rust_embed_utils::read_file_from_fs(&path).map_err(|e| invalid(format!("cannot read migration file '{file_name}': {e}")))?;
        files.push(MigrationFile { id, file_name, name, file });
    }
    files.sort_by_key(|f| f.id);
    for (index, file) in files.iter().enumerate() {
        let expected = index as u64 + 1;
        if u64::from(file.id) != expected {
            return Err(invalid(format!(
                "migration numbering must start at 1 without gaps or repeats: expected number {expected}, found '{}'",
                file.file_name
            )));
        }
    }
    Ok(files)
}

static MIGRATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// (file name, file) of the list being applied.
static MIGRATION_LIST: Mutex<Vec<(String, EmbeddedFile)>> = Mutex::new(Vec::new());

fn lock_migration_list() -> std::sync::MutexGuard<'static, Vec<(String, EmbeddedFile)>> {
    MIGRATION_LIST.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What hiqlite reads the migrations through: the current `MIGRATION_LIST`,
/// where it would normally get files embedded at compile time.
struct MigrationFiles;

impl RustEmbed for MigrationFiles {
    fn get(file_path: &str) -> Option<EmbeddedFile> {
        lock_migration_list().iter().find(|(name, _)| name == file_path).map(|(_, file)| file.clone())
    }

    fn iter() -> impl Iterator<Item = std::borrow::Cow<'static, str>> + 'static {
        let names: Vec<_> = lock_migration_list().iter().map(|(name, _)| std::borrow::Cow::Owned(name.clone())).collect();
        names.into_iter()
    }
}

#[cfg(test)]
impl Db {
    pub(crate) async fn migrate_dir(&self, dir: &Path, wait: Duration) -> Result<(), MigrationError> {
        self.migrate(read_migrations(dir)?, wait, wait).await
    }

    /// The single TEXT value of the first row; panics otherwise.
    pub(crate) async fn query_text(&self, sql: &str) -> String {
        match &self.query(sql).await.unwrap()[0][0].1 {
            DbValue::Text(text) => text.clone(),
            other => panic!("expected TEXT, got {:?}", other),
        }
    }
}

/// Converts a database value to Lua: INTEGER and REAL -> number, TEXT -> string,
/// BLOB -> string of raw bytes, NULL -> nil.
fn db_value_to_lua(lua: &Lua, value: &DbValue) -> LuaResult<Value> {
    Ok(match value {
        DbValue::Null => Value::Nil,
        DbValue::Integer(i) => Value::Integer(*i),
        DbValue::Real(f) => Value::Number(*f),
        DbValue::Text(s) => Value::String(lua.create_string(s)?),
        DbValue::Blob(b) => Value::String(lua.create_string(b)?),
    })
}

/// Functions that the replicated write path rejects whatever their arguments
/// (hiqlite's `FORBIDDEN_NON_DET_FNS`).
const NON_DETERMINISTIC_FUNCTIONS: &[&str] = &[
    "date", "datetime", "julianday", "now", "random", "randomblob", "strftime", "time", "timediff", "unixepoch",
];

/// Keywords that hiqlite does not reject but that evaluate differently on every node.
const NON_DETERMINISTIC_KEYWORDS: &[&str] = &["current_timestamp", "current_date", "current_time"];

/// Default limit for one `sql_query` / `sql_execute` call.
pub const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_millis(5000);

/// Default limit for a script's own execution in one request.
pub const DEFAULT_MAX_EXECUTION: Duration = Duration::from_millis(1000);

/// How long a script runs before it yields so other requests on its VM can run.
const YIELD_SLICE: Duration = Duration::from_millis(10);

/// A VM's interrupt reads the clock on one call in this many: a script's loop calls
/// it on every iteration, where the clock and the lock would take most of the time.
const INTERRUPT_CHECK_EVERY: usize = 64;

/// Words right before the name of a table, which a list of its columns may follow.
const BEFORE_A_TABLE_NAME: &[&str] = &["into", "table", "exists", "references", "with", "recursive"];

#[derive(Debug, PartialEq)]
enum SqlToken {
    /// A word, or a name in `"x"`, `` `x` `` or `[x]`; lowercase, with what quotes it taken off.
    Name { text: String, quoted: bool },
    OpenParen,
    Other,
}

/// The tokens of `sql` as far as SQLite's tokenizer goes for names: comments and whitespace are
/// skipped, a byte order mark is whitespace before a token and part of a name inside one, and
/// every other non-ASCII character is part of a name.
fn sql_tokens(sql: &str) -> Vec<SqlToken> {
    let is_word_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii();
    let name = |text: String, quoted| SqlToken::Name { text: text.to_ascii_lowercase(), quoted };
    let mut tokens = Vec::new();
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' | '\x0c' | '\r' | '\u{FEFF}' => {}
            '-' if chars.peek() == Some(&'-') => {
                chars.by_ref().find(|c| *c == '\n');
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = ' ';
                for c in chars.by_ref() {
                    if previous == '*' && c == '/' {
                        break;
                    }
                    previous = c;
                }
            }
            '\'' | '"' | '`' | '[' => {
                let close = if c == '[' { ']' } else { c };
                let mut text = String::new();
                while let Some(c) = chars.next() {
                    if c == close && (close == ']' || chars.next_if_eq(&close).is_none()) {
                        break;
                    }
                    text.push(c);
                }
                tokens.push(if c == '\'' { SqlToken::Other } else { name(text, true) });
            }
            '(' => tokens.push(SqlToken::OpenParen),
            c if is_word_char(c) => {
                let mut text = c.to_string();
                while let Some(c) = chars.next_if(|c| is_word_char(*c)) {
                    text.push(c);
                }
                tokens.push(name(text, false));
            }
            _ => tokens.push(SqlToken::Other),
        }
    }
    tokens
}

/// Returns the first non-deterministic function call or keyword in `sql`, however it is written:
/// hiqlite panics on these when it applies a write, in the cluster's state machine. A name counts
/// when a parenthesis follows it, quoted or not, unless it is a table with its columns; a bare
/// keyword counts by itself. A list of columns anywhere else, as in `CREATE INDEX i ON "date"(a)`,
/// reads as a call: being wrong that way is an error for the script, and the other way a panic.
pub fn find_non_deterministic(sql: &str) -> Option<&'static str> {
    let tokens = sql_tokens(sql);
    let names_a_table = |position: usize| {
        position > 0 && matches!(&tokens[position - 1], SqlToken::Name { text, quoted: false } if BEFORE_A_TABLE_NAME.contains(&text.as_str()))
    };
    tokens.iter().enumerate().find_map(|(position, token)| {
        let SqlToken::Name { text, quoted } = token else {
            return None;
        };
        let keyword = NON_DETERMINISTIC_KEYWORDS.iter().find(|keyword| **keyword == text);
        let item = keyword.or_else(|| NON_DETERMINISTIC_FUNCTIONS.iter().find(|function| **function == text))?;
        let called = tokens.get(position + 1) == Some(&SqlToken::OpenParen) && !names_a_table(position);
        (called || (keyword.is_some() && !quoted)).then_some(*item)
    })
}

/// Counts the `?` placeholders of `sql`, leaving out text, quoted names and comments.
/// hiqlite does not check the number of values, so a `?` left without one would read as NULL.
fn count_placeholders(sql: &str) -> Result<usize, String> {
    let placeholders = sql_placeholders(sql)?;
    match placeholders.iter().find(|placeholder| placeholder.number.is_some()) {
        Some(numbered) => Err(unsupported_placeholder(sql, numbered.at)),
        None => Ok(placeholders.len()),
    }
}

/// A `?` or `?NNN` placeholder in a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placeholder {
    /// Where it starts in the statement.
    pub at: usize,
    /// `NNN` of `?NNN`; `None` for a plain `?`.
    pub number: Option<u32>,
}

/// The `?` and `?NNN` placeholders of `sql`, in order, leaving out text, quoted names and
/// comments. Named parameters (`:name`, `@name`, `$name`) are refused.
pub fn sql_placeholders(sql: &str) -> Result<Vec<Placeholder>, String> {
    let bytes = sql.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80;
    // The position after the first `end` at or after `from`, or the end of the statement.
    let after = |from: usize, end: &[u8]| {
        bytes[from..].windows(end.len()).position(|w| w == end).map_or(bytes.len(), |at| from + at + end.len())
    };
    let (mut found, mut i) = (Vec::new(), 0);
    while i < bytes.len() {
        let next = bytes.get(i + 1).copied();
        match bytes[i] {
            // A doubled quote inside reads as two strings in a row, which counts the same.
            quote @ (b'\'' | b'"' | b'`') => i = after(i + 1, &[quote]),
            b'[' => i = after(i + 1, b"]"),
            b'-' if next == Some(b'-') => i = after(i + 2, b"\n"),
            b'/' if next == Some(b'*') => i = after(i + 2, b"*/"),
            b'?' => {
                let digits = bytes[i + 1..].iter().take_while(|b| b.is_ascii_digit()).count();
                let number = match digits {
                    0 => None,
                    _ => Some(sql[i + 1..i + 1 + digits].parse::<u32>().map_err(|_| unsupported_placeholder(sql, i))?),
                };
                found.push(Placeholder { at: i, number });
                i += 1 + digits;
            }
            // A name, with any `$` in it, as in `a$$b`. What follows is not part of it: `LIKE:pat`
            // is a parameter.
            b if is_word(b) => {
                while bytes.get(i).is_some_and(|b| is_word(*b) || *b == b'$') {
                    i += 1;
                }
            }
            // SQLite reads `$` as part of a parameter's name too, as in `$$name` and `:$name`.
            b':' | b'@' | b'$' if next.is_some_and(|b| is_word(b) || b == b'$') => return Err(unsupported_placeholder(sql, i)),
            _ => i += 1,
        }
    }
    Ok(found)
}

fn unsupported_placeholder(sql: &str, at: usize) -> String {
    let form: String = sql[at..].chars().take(16).take_while(|c| *c == '?' || *c == ':' || *c == '@' || *c == '$' || c.is_alphanumeric() || *c == '_').collect();
    format!("only plain ? placeholders are supported, found {form}")
}

fn sql_param(position: usize, value: &Value) -> Result<hiqlite::Param, String> {
    use hiqlite::Param;
    match value {
        Value::String(text) => {
            let bytes = text.as_bytes();
            Ok(match std::str::from_utf8(&bytes) {
                Ok(text) => Param::Text(text.to_string()),
                Err(_) => Param::Blob(bytes.to_vec()),
            })
        }
        // mlua hands over a number with an integer value that fits as an integer.
        Value::Integer(number) => Ok(Param::Integer(*number)),
        Value::Number(number) if !number.is_finite() => Err(format!("parameter {position} is not a finite number")),
        Value::Number(number) => Ok(Param::Real(*number)),
        Value::Boolean(flag) => Ok(Param::Integer(i64::from(*flag))),
        other => Err(format!("parameter {position} has unsupported type {}", other.type_name())),
    }
}

/// The values of an array of parameters. `pairs` skips `nil`, so a hole shows up as a key
/// beyond the number of values; a `nil` at the end is left to the placeholder count.
fn sql_params(params: &Table) -> Result<Vec<hiqlite::Param>, String> {
    let not_array = || "parameters must be an array without holes".to_string();
    let mut entries = Vec::new();
    for pair in params.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|e| e.to_string())?;
        let position = match key {
            Value::Integer(key) => usize::try_from(key).ok(),
            _ => None,
        };
        entries.push((position.filter(|p| *p >= 1).ok_or_else(not_array)?, value));
    }
    let count = entries.len();
    entries.sort_by_key(|(position, _)| *position);
    if entries.last().is_some_and(|(position, _)| *position != count) {
        return Err(not_array());
    }
    entries.iter().map(|(position, value)| sql_param(*position, value)).collect()
}

/// What to bind for the optional second argument of `sql_query` and `sql_execute`.
fn bound_params(function: &str, sql: &str, params: &Value) -> LuaResult<Vec<hiqlite::Param>> {
    let values = match params {
        // Or a `?` would quietly be NULL, and a query with a forgotten table would find nothing.
        Value::Nil => Ok(Vec::new()),
        Value::Table(params) => sql_params(params),
        other => Err(format!("parameters must be an array, not {}", other.type_name())),
    };
    let checked = values.and_then(|values| {
        let placeholders = count_placeholders(sql)?;
        if placeholders == values.len() {
            Ok(values)
        } else {
            let counted = |count: usize, what: &str| format!("{count} {what}{}", if count == 1 { "" } else { "s" });
            Err(format!("{}, {}", counted(placeholders, "placeholder"), counted(values.len(), "value")))
        }
    });
    checked.map_err(|e| mlua::Error::RuntimeError(format!("{function}: {e}")))
}

pub fn now_unix() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as f64).unwrap_or(0.0)
}

fn attr_to_lua(lua: &Lua, value: &AttrValue) -> LuaResult<Value> {
    Ok(match value {
        AttrValue::Bytes(bytes) => Value::String(lua.create_string(bytes)?),
        AttrValue::Number(n) => Value::Number(*n),
    })
}

fn lua_to_attr(value: Value) -> LuaResult<AttrValue> {
    match value {
        Value::String(s) => Ok(AttrValue::Bytes(s.as_bytes().to_vec())),
        Value::Integer(i) => Ok(AttrValue::Number(i as f64)),
        Value::Number(n) => Ok(AttrValue::Number(n)),
        other => Err(mlua::Error::RuntimeError(format!("attribute value must be a string or number, got {}", other.type_name()))),
    }
}

/// The most `log` writes for one message.
const MAX_LOG_MESSAGE: usize = 4096;

/// The arguments of `log` on one line, cut to the limit. A value that cannot be turned
/// into text, through a metamethod that fails, must not change the script's decision.
fn log_message(values: &[Value]) -> String {
    let text = |value: &Value| match value {
        Value::String(text) => String::from_utf8_lossy(&text.as_bytes()).into_owned(),
        value => value.to_string().unwrap_or_else(|_| format!("<{}>", value.type_name())),
    };
    let mut message = values.iter().map(text).collect::<Vec<_>>().join(" ").replace(['\n', '\r'], " ");
    if message.len() > MAX_LOG_MESSAGE {
        let end = message.floor_char_boundary(MAX_LOG_MESSAGE);
        message.truncate(end);
    }
    message
}

/// `call` within `left`; with nothing left, a timeout at once, without starting it.
async fn run_within<T>(left: Duration, call: impl Future<Output = T>) -> Result<T, tokio::time::error::Elapsed> {
    if left.is_zero() {
        // An elapsed timeout, the same kind of error as one that ran out.
        return tokio::time::timeout(Duration::ZERO, std::future::pending::<T>()).await;
    }
    tokio::time::timeout(left, call).await
}

/// Notes how a database call failed.
fn note_database_failure<T, E>(result: &Result<Result<T, DbFailure>, E>, failures: &DbFailures) {
    let flag = match result {
        Err(_) => &failures.unavailable,
        Ok(Err(DbFailure { kind: DbFailureKind::Unavailable, .. })) => &failures.unavailable,
        Ok(Err(DbFailure { kind: DbFailureKind::Mistake, message })) => {
            failures.note_mistake(message);
            return;
        }
        Ok(Err(DbFailure { kind: DbFailureKind::Unknown, .. })) => &failures.unknown,
        Ok(Ok(_)) => return,
    };
    flag.store(true, Ordering::SeqCst);
}

/// A database call that failed, and whether trying later could help. Only errors known to
/// mean the cluster could not take the call count: for the rest the NAS retransmits as it
/// does without a spool, so a mistake here cannot make a bad request look recorded.
#[derive(Debug)]
pub struct DbFailure {
    message: String,
    kind: DbFailureKind,
}

impl DbFailure {
    /// The cluster could not take the call, or did not answer in time: another try may work.
    pub fn unavailable(&self) -> bool {
        self.kind == DbFailureKind::Unavailable
    }

    /// A failure that is neither the cluster's nor known to be the statement's.
    pub fn other(message: String) -> Self {
        Self { message, kind: DbFailureKind::Unknown }
    }

    /// For a call that took too long.
    pub fn timed_out(after: Duration) -> Self {
        Self { message: format!("timed out after {} ms", after.as_millis()), kind: DbFailureKind::Unavailable }
    }
}

impl From<DbFailure> for String {
    fn from(failure: DbFailure) -> Self {
        failure.message
    }
}

impl std::fmt::Display for DbFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum DbFailureKind {
    /// The database could not be reached or did not answer in time.
    Unavailable,
    /// The script's SQL or its parameters: it would fail the same way again.
    Mistake,
    /// Neither of those for certain.
    Unknown,
}

/// How the database calls of one request failed, even if the script caught the errors.
/// How many errors of one request are remembered, since a script can make them in a loop.
const REMEMBERED_MISTAKES: usize = 16;

#[derive(Default)]
struct DbFailures {
    /// The time the request's database calls have taken: together they have `query_timeout`.
    spent: parking_lot::Mutex<Duration>,
    unavailable: AtomicBool,
    /// The messages of the errors that running it again would not change, newest last.
    mistakes: parking_lot::Mutex<Vec<String>>,
    unknown: AtomicBool,
}

impl DbFailures {
    /// What is left of `limit` for the next database call of the request.
    fn left(&self, limit: Duration) -> Duration {
        limit.saturating_sub(*self.spent.lock())
    }

    fn spend(&self, taken: Duration) {
        *self.spent.lock() += taken;
    }

    fn note_mistake(&self, message: &str) {
        let mut mistakes = self.mistakes.lock();
        if mistakes.len() == REMEMBERED_MISTAKES {
            mistakes.remove(0);
        }
        mistakes.push(message.to_string());
    }

    fn note_mistake_raised(&self, error: &mlua::Error) {
        match error {
            mlua::Error::RuntimeError(message) => self.note_mistake(message),
            error => self.note_mistake(&error.to_string()),
        }
    }

    /// Whether `error`, which ended the script, is one of those, passed along or not. One the
    /// script caught says nothing about what came after it.
    fn ended_by_mistake(&self, error: &mlua::Error) -> bool {
        let error = error.to_string();
        self.mistakes.lock().iter().any(|message| error.contains(message.as_str()))
    }
}

/// Whether `error` says the cluster could not take the statement, rather than refusing it.
pub(crate) fn cluster_unavailable(error: &hiqlite::Error) -> bool {
    use hiqlite::Error as E;
    // `Error` is what hiqlite says when a channel to the node closed or could not be sent on, and
    // `SnapshotError` a snapshot failing: the cluster's trouble, not the statement's.
    matches!(error, E::RaftErrorFatal(_) | E::CheckIsLeaderError(_) | E::ClientWriteError(_) | E::Connect(_) | E::LeaderChange(_) | E::Timeout(_) | E::Channel(_) | E::RaftError(_) | E::Request(_) | E::WebSocket(_) | E::Error(_) | E::SnapshotError(_))
}

impl From<hiqlite::Error> for DbFailure {
    fn from(error: hiqlite::Error) -> Self {
        Self::from_ref(&error)
    }
}

impl DbFailure {
    fn from_ref(error: &hiqlite::Error) -> Self {
        use hiqlite::Error as E;
        let unavailable = cluster_unavailable(error);
        // Reads report wrong SQL as `Sqlite`, along with a locked database or a failing disk, so
        // this one variant goes by its text. What it does not recognize is tried again.
        let wrong_sql = |text: &str| ["syntax error", "no such ", "has no column", "ambiguous column"].iter().any(|sign| text.contains(sign));
        let mistake = match error {
            E::BadRequest(_) | E::ConstraintViolation(_) | E::PrepareStatement(_) | E::QueryParams(_) | E::QueryReturnedNoRows(_) => true,
            E::Sqlite(text) => wrong_sql(text),
            _ => false,
        };
        let kind = if unavailable {
            DbFailureKind::Unavailable
        } else if mistake {
            DbFailureKind::Mistake
        } else {
            DbFailureKind::Unknown
        };
        Self { message: error.to_string(), kind }
    }
}

/// The options passed to `http_request`. A mistake here is the script's, so it is an error.
fn http_options(options: &Value) -> LuaResult<HttpOptions> {
    let failed = |message: &str| mlua::Error::RuntimeError(format!("http_request: {message}"));
    let Value::Table(options) = options else {
        return Err(failed("takes a table of options"));
    };
    let text = |value: Value, what: &str| match value {
        Value::String(text) => Ok(text.as_bytes().to_vec()),
        _ => Err(failed(&format!("{what} must be a string"))),
    };
    let url = match options.raw_get::<Value>("url")? {
        Value::Nil => return Err(failed("url is required")),
        value => String::from_utf8_lossy(&text(value, "url")?).into_owned(),
    };
    let method = match options.raw_get::<Value>("method")? {
        Value::Nil => reqwest::Method::GET,
        value => match text(value, "method")?.as_slice() {
            b"GET" => reqwest::Method::GET,
            b"POST" => reqwest::Method::POST,
            b"PUT" => reqwest::Method::PUT,
            b"DELETE" => reqwest::Method::DELETE,
            b"PATCH" => reqwest::Method::PATCH,
            _ => return Err(failed("method must be GET, POST, PUT, DELETE or PATCH")),
        },
    };
    let mut headers = Vec::new();
    match options.raw_get::<Value>("headers")? {
        Value::Nil => {}
        Value::Table(table) => {
            for pair in table.pairs::<Value, Value>() {
                let (name, value) = pair?;
                let name = String::from_utf8_lossy(&text(name, "a header name")?).into_owned();
                let value = text(value, "a header value")?;
                if reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err() || reqwest::header::HeaderValue::from_bytes(&value).is_err() {
                    return Err(failed("a header name or value is not valid"));
                }
                // These decide where a shared proxy sends the request, or how it is framed.
                const RESERVED: [&str; 6] = ["host", "content-length", "transfer-encoding", "connection", "upgrade", "te"];
                if RESERVED.iter().any(|reserved| name.eq_ignore_ascii_case(reserved)) {
                    return Err(failed(&format!("the {name} header cannot be set")));
                }
                headers.push((name, value));
            }
        }
        _ => return Err(failed("headers must be a table")),
    }
    let body = match options.raw_get::<Value>("body")? {
        Value::Nil => None,
        value => Some(text(value, "body")?),
    };
    Ok(HttpOptions { url, method, headers, body })
}

/// The attributes passed to `send_disconnect` or `send_coa`: name to value, or to an
/// array of values for an attribute that repeats. Sorted, as a table has no order.
fn dynamic_auth_attributes(table: &Table) -> LuaResult<Vec<(String, AttrValue)>> {
    let mut attributes = Vec::new();
    for pair in table.pairs::<String, Value>() {
        let (name, value) = pair?;
        match value {
            Value::Table(values) => {
                if values.raw_len() == 0 || values.raw_len() != values.pairs::<Value, Value>().count() {
                    return Err(mlua::Error::RuntimeError(format!("attribute '{name}' needs a value or a non-empty array of values")));
                }
                for value in values.sequence_values::<Value>() {
                    attributes.push((name.clone(), lua_to_attr(value?)?));
                }
            }
            value => attributes.push((name, lua_to_attr(value)?)),
        }
    }
    attributes.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(attributes)
}

/// Attributes added with `set_attr` during one script run, in call order.
type ResponseAttributes = Arc<Mutex<Vec<(String, AttrValue)>>>;

/// The request whose script the VM is executing right now, for the interrupt.
#[derive(Clone, Copy)]
struct Running {
    /// The request's own coroutine; only it yields, so coroutines the script
    /// creates itself keep their semantics.
    thread: usize,
    poll_start: Instant,
    /// Execution time used by earlier polls of this request.
    used: Duration,
    limit: Duration,
}

/// One sandboxed Luau VM and the number of requests currently running on it.
struct Vm {
    lua: Lua,
    /// Its place in `LuaVms::vms`.
    index: usize,
    /// What it may hold, in bytes; 0 for no limit.
    max_memory: AtomicUsize,
    /// What it held after the last collection here.
    held_after_collecting: AtomicUsize,
    #[cfg(test)]
    collections: AtomicUsize,
    active: AtomicUsize,
    /// Serializes polls of scripts on this VM, so `running` always names the
    /// request being executed. Released fairly, so a script that keeps running
    /// cannot take it back ahead of requests waiting for the same VM.
    poll_lock: parking_lot::Mutex<()>,
    running: Arc<Mutex<Option<Running>>>,
    /// Whether the script being polled has used up its time. Only `poll_lock`'s holder touches it.
    over_limit: Arc<AtomicBool>,
}

impl Vm {
    /// Luau collects in steps as scripts allocate, with no last try when an allocation fails,
    /// so what a finished script left must not be the next one's failure. Live data of scripts
    /// that wait is not worth going over each time one ends, so past half of the limit this
    /// waits for growth, until so close to the limit that it pays anyway.
    fn collect_near_the_limit(&self) {
        let limit = self.max_memory.load(Ordering::Relaxed);
        if limit == 0 {
            return;
        }
        let collect_above = (self.held_after_collecting.load(Ordering::Relaxed) + limit / 4).clamp(limit / 2, limit / 8 * 7);
        if self.lua.used_memory() > collect_above {
            let _poll = self.poll_lock.lock();
            let _ = self.lua.gc_collect();
            self.held_after_collecting.store(self.lua.used_memory(), Ordering::Relaxed);
            #[cfg(test)]
            self.collections.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// What a script past its time limit gets, however many times it catches it.
fn limit_error(limit: Duration) -> mlua::Error {
    mlua::Error::RuntimeError(format!("script exceeded the execution limit of {} ms", limit.as_millis()))
}

/// A fixed set of sandboxed Luau VMs shared by every script: the top-level
/// script and every `lua` auth handler. Requests go to the least busy VM and
/// run in a global table of their own, so they never see each other's globals.
pub struct LuaVms {
    vms: Vec<Vm>,
    /// Accounting scripts running at once. Without a limit a burst of them fills the
    /// runtime's queue, and every other request waits for its whole length each time
    /// it is scheduled.
    accounting_slots: Arc<Semaphore>,
}

/// An accounting request's place among the scripts allowed to run. It is given
/// up while the script waits for the database, so a slow database does not limit
/// accounting throughput to the number of places.
struct AccountingSlot {
    slots: Arc<Semaphore>,
    state: Mutex<SlotState>,
}

#[derive(Default)]
struct SlotState {
    held: Option<OwnedSemaphorePermit>,
    /// The task queueing to get the place back.
    queueing: Option<tokio::task::AbortHandle>,
    ended: bool,
}

/// Ends the slot when a script ends, however it ends. Dropping the slot is not
/// enough: the request's Lua functions hold it until the VM's next garbage collection.
struct PlaceGuard(Arc<AccountingSlot>);

impl Drop for PlaceGuard {
    fn drop(&mut self) {
        self.0.end();
    }
}

impl AccountingSlot {
    /// Waits in arrival order for a place. Not in a coroutine yet, so if the request is
    /// abandoned here, this wait goes with it.
    async fn acquire(slots: Arc<Semaphore>) -> LuaResult<Self> {
        let permit = slots.clone().acquire_owned().await.map_err(mlua::Error::external)?;
        Ok(Self { slots, state: Mutex::new(SlotState { held: Some(permit), ..SlotState::default() }) })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits for the place back, on behalf of a script between database calls. The queueing
    /// is a task of its own because this future lives in the script's coroutine. If the
    /// request were abandoned, the semaphore would go on to assign a place to a waiter that
    /// nothing polls and that only garbage collection removes, which never comes if every
    /// script on the VM is waiting for that place.
    async fn take_place(self: &Arc<Self>) -> LuaResult<()> {
        if let Ok(permit) = self.slots.clone().try_acquire_owned() {
            self.state().held = Some(permit);
            return Ok(());
        }
        let slot = self.clone();
        let queue = tokio::spawn(async move {
            let permit = slot.slots.clone().acquire_owned().await?;
            let mut state = slot.state();
            if !state.ended {
                state.held = Some(permit);
            }
            Ok::<_, tokio::sync::AcquireError>(())
        });
        {
            let mut state = self.state();
            if state.ended {
                queue.abort();
            } else {
                state.queueing = Some(queue.abort_handle());
            }
        }
        queue.await.map_err(mlua::Error::external)?.map_err(mlua::Error::external)
    }

    fn give_up_place(&self) {
        self.state().held = None;
    }

    /// Gives up the place, and the place being queued for.
    fn end(&self) {
        let mut state = self.state();
        state.ended = true;
        state.held = None;
        if let Some(queueing) = state.queueing.take() {
            queueing.abort();
        }
    }
}

/// Marks a request as running on a VM until dropped.
struct VmGuard<'a>(&'a Vm);

impl Drop for VmGuard<'_> {
    fn drop(&mut self) {
        // Before it looks idle, or new requests would come here and wait.
        self.0.collect_near_the_limit();
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Accounting scripts allowed to run at once on `vms` VMs. There is one VM and one
/// runtime worker per CPU by default, and a script runs on its worker, so this leaves
/// workers free for logins while accounting scripts use every place.
fn accounting_places(vms: usize) -> usize {
    vms.saturating_sub(2).max(1)
}

impl LuaVms {
    /// One VM per available CPU, each holding at most `max_memory` bytes.
    pub fn new(max_memory: usize) -> LuaResult<Self> {
        let vms = Self::with_count(crate::runtime::worker_threads())?;
        vms.set_memory_limit(max_memory)?;
        Ok(vms)
    }

    /// What a script past it gets is an error like any other, and not the end of the process.
    fn set_memory_limit(&self, bytes: usize) -> LuaResult<()> {
        for vm in &self.vms {
            vm.lua.set_memory_limit(bytes)?;
            vm.max_memory.store(bytes, Ordering::Relaxed);
        }
        Ok(())
    }

    pub fn with_count(count: usize) -> LuaResult<Self> {
        let mut vms = Vec::with_capacity(count.max(1));
        for _ in 0..count.max(1) {
            let lua = Lua::new();
            // These reach files on disk or a global table that outlives the request.
            // Before sandbox(), which makes the global table read-only.
            let globals = lua.globals();
            for name in ["require", "loadstring", "getfenv", "setfenv"] {
                globals.raw_remove(name)?;
            }
            // Built-in libraries become read-only.
            lua.sandbox(true)?;
            let running: Arc<Mutex<Option<Running>>> = Arc::default();
            let current = running.clone();
            // Relaxed: `poll_lock` lets one thread at a time run scripts on this VM.
            let interrupts = AtomicUsize::new(0);
            let over_limit: Arc<AtomicBool> = Arc::default();
            let over = over_limit.clone();
            lua.set_interrupt(move |lua| {
                // Past the limit, every check raises the error and not one in so many: a script can
                // catch it with `pcall`, and only here does it reach the code outside the `pcall`.
                let sampled = interrupts.fetch_add(1, Ordering::Relaxed).is_multiple_of(INTERRUPT_CHECK_EVERY);
                if !sampled && !over.load(Ordering::Relaxed) {
                    return Ok(mlua::VmState::Continue);
                }
                let Some(run) = *current.lock().unwrap_or_else(PoisonError::into_inner) else {
                    return Ok(mlua::VmState::Continue);
                };
                let in_poll = run.poll_start.elapsed();
                if run.used + in_poll > run.limit {
                    over.store(true, Ordering::Relaxed);
                    return Err(limit_error(run.limit));
                }
                // Yielding right after a resume would make no progress, hence the slice.
                if in_poll >= YIELD_SLICE && lua.current_thread().to_pointer() as usize == run.thread {
                    Ok(mlua::VmState::Yield)
                } else {
                    Ok(mlua::VmState::Continue)
                }
            });
            vms.push(Vm { lua, index: vms.len(), max_memory: AtomicUsize::new(0), held_after_collecting: AtomicUsize::new(0), #[cfg(test)] collections: AtomicUsize::new(0), active: AtomicUsize::new(0), poll_lock: parking_lot::Mutex::new(()), running, over_limit });
        }
        let accounting_slots = Arc::new(Semaphore::new(accounting_places(vms.len())));
        Ok(Self { vms, accounting_slots })
    }

    fn pick(&self) -> VmGuard<'_> {
        let vm = self.vms.iter().min_by_key(|vm| vm.active.load(Ordering::SeqCst)).unwrap_or(&self.vms[0]);
        vm.active.fetch_add(1, Ordering::SeqCst);
        VmGuard(vm)
    }
}

/// A script run on a VM, counting only the time spent executing it: while it
/// waits for the database it is not polled, so that time is not counted.
struct Timed<'a, F> {
    inner: Pin<Box<F>>,
    vm: &'a Vm,
    thread: usize,
    used: Duration,
    limit: Duration,
}

impl<F: Future> Future for Timed<'_, F> {
    /// What the script came to, and whether it took longer than it may have.
    type Output = (F::Output, bool);

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let vm = self.vm;
        let serial = vm.poll_lock.lock();
        let poll_start = Instant::now();
        *vm.running.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(Running { thread: self.thread, poll_start, used: self.used, limit: self.limit });
        vm.over_limit.store(self.used > self.limit, Ordering::Relaxed);
        let result = self.inner.as_mut().poll(cx);
        vm.over_limit.store(false, Ordering::Relaxed);
        *vm.running.lock().unwrap_or_else(PoisonError::into_inner) = None;
        parking_lot::MutexGuard::unlock_fair(serial);
        self.used += poll_start.elapsed();
        let over = self.used > self.limit;
        result.map(|output| (output, over))
    }
}

/// Logs a script failure; the request it belongs to is rejected.
fn log_script_error(request: &LuaRequest, error: &mlua::Error) {
    log_line(tracing::Level::ERROR, format!("Lua script error ({}/{}): {}", request.protocol, request.phase, error));
}

pub fn log_line(level: tracing::Level, line: String) {
    #[cfg(any(test, feature = "test-support"))]
    if crate::test_support::keeping() {
        crate::test_support::logged_errors().lock().unwrap_or_else(PoisonError::into_inner).push(line.clone());
    }
    match level {
        tracing::Level::ERROR => tracing::error!("{line}"),
        tracing::Level::WARN => tracing::warn!("{line}"),
        tracing::Level::INFO => tracing::info!("{line}"),
        _ => tracing::debug!("{line}"),
    }
}

pub struct LuaAuthEngine {
    pub db: Db,
    vms: Arc<LuaVms>,
    /// The script compiled once to Luau bytecode.
    bytecode: Vec<u8>,
    query_timeout: Duration,
    max_execution: Duration,
    /// Where the script's errors say they happened.
    chunk_name: String,
    /// What `http_request` sends through; `None` without `[lua.http]`.
    http: Option<Arc<ScriptHttp>>,
    /// What `send_disconnect` and `send_coa` send through; `None` in a program without CoA.
    dynamic_auth: Option<Arc<dyn DynamicAuthorization>>,
    /// For each VM, the function that makes a request's global table, made on first use.
    binders: Vec<std::sync::OnceLock<Function>>,
    /// How many times a binder was made, for tests.
    #[cfg(test)]
    binders_made: AtomicUsize,
}

/// What one request's functions work on. The functions are made once per VM and
/// script, and get this as their first argument from the closures in the request's
/// global table, so making a request's table costs no new Rust function.
struct RequestData {
    attributes: Vec<(String, AttrValue)>,
    validator: Option<AttrValidator>,
    responses: ResponseAttributes,
    slot: Option<Arc<AccountingSlot>>,
    failures: Arc<DbFailures>,
    protocol: &'static str,
    phase: &'static str,
    client: String,
}

impl mlua::UserData for RequestData {}

/// Makes a request's global table: called once per VM and script with the shared
/// metatable and functions, it returns the function called for each request.
/// `...` keeps the script's arguments where the script put them, so a wrong one is
/// reported at the same position as before.
const BINDER: &str = r#"
local meta, get_attr, get_attrs, set_attr, sql_query, sql_execute, log, send_disconnect, send_coa, http_request, url_encode, now_unix, totp_verify, random_token, wpa_pmk = ...
local setmetatable = setmetatable
return function(request, protocol, phase, client, auth_user, replayed, eap_method, tls_subject, tls_subject_attrs, radsec_subject, radsec_subject_attrs)
	local env = setmetatable({
		protocol = protocol,
		phase = phase,
		client = client,
		auth_user = auth_user,
		replayed = replayed,
		eap_method = eap_method,
		tls_subject = tls_subject,
		tls_subject_attrs = tls_subject_attrs,
		radsec_subject = radsec_subject,
		radsec_subject_attrs = radsec_subject_attrs,
		get_attr = function(...) return get_attr(request, ...) end,
		get_attrs = function(...) return get_attrs(request, ...) end,
		set_attr = function(...) return set_attr(request, ...) end,
		sql_query = function(...) return sql_query(request, ...) end,
		sql_execute = function(...) return sql_execute(request, ...) end,
		log = function(...) return log(request, ...) end,
		send_disconnect = function(...) return send_disconnect(request, ...) end,
		send_coa = function(...) return send_coa(request, ...) end,
		now_unix = now_unix,
		totp_verify = totp_verify,
		random_token = random_token,
		wpa_pmk = wpa_pmk,
	}, meta)
	if http_request then
		env.http_request = function(...) return http_request(request, ...) end
		env.url_encode = url_encode
	end
	return env
end
"#;

/// A function whose first argument is the request it works for; the rest are
/// converted and reported as the script's arguments 1, 2, and so on.
fn request_function<A, R, F>(lua: &Lua, f: F) -> LuaResult<Function>
where
    A: FromLuaMulti,
    R: IntoLuaMulti,
    F: Fn(&Lua, &RequestData, A) -> LuaResult<R> + Send + 'static,
{
    lua.create_function(move |lua, (request, args): (AnyUserData, MultiValue)| {
        let request = request.borrow::<RequestData>()?;
        f(lua, &request, A::from_lua_args(args, 1, None, lua)?)
    })
}

/// Like `request_function`, for a function that waits: `f` takes what it needs from
/// the request and returns the future.
fn request_async_function<A, R, F, Fut>(lua: &Lua, f: F) -> LuaResult<Function>
where
    A: FromLuaMulti,
    R: IntoLuaMulti,
    F: Fn(Lua, &RequestData, A) -> Fut + Send + 'static,
    Fut: Future<Output = LuaResult<R>> + Send + 'static,
{
    lua.create_async_function(move |lua, (request, args): (AnyUserData, MultiValue)| {
        let started = request.borrow::<RequestData>().and_then(|request| {
            let args = A::from_lua_args(args, 1, None, &lua)?;
            Ok(f(lua.clone(), &request, args))
        });
        async move { started?.await }
    })
}

/// Compiles a script to Luau bytecode; a syntax error is returned as the error message.
pub fn compile_script(script: &str) -> Result<Vec<u8>, String> {
    mlua::chunk::Compiler::new().compile(script).map_err(|e| e.to_string())
}

impl LuaAuthEngine {
    /// Compiles `script`; a syntax error is returned as the error message.
    pub fn new(vms: Arc<LuaVms>, db: Db, script: impl AsRef<str>) -> Result<Self, String> {
        let bytecode = compile_script(script.as_ref())?;
        let binders = vms.vms.iter().map(|_| std::sync::OnceLock::new()).collect();
        Ok(Self {
            http: None,
            db,
            vms,
            bytecode,
            query_timeout: DEFAULT_QUERY_TIMEOUT,
            max_execution: DEFAULT_MAX_EXECUTION,
            chunk_name: "=script".to_string(),
            dynamic_auth: None,
            binders,
            #[cfg(test)]
            binders_made: AtomicUsize::new(0),
        })
    }

    /// Sets the limit for the script's own execution in one request; database
    /// waits do not count.
    pub fn with_max_execution(mut self, limit: Duration) -> Self {
        self.max_execution = limit;
        self
    }

    /// Sets the name the script's errors give as their location, usually its
    /// path. Without this it is `script`.
    pub fn with_script_name(mut self, name: impl AsRef<str>) -> Self {
        // `=` makes Luau show the name as it is.
        self.chunk_name = format!("={}", name.as_ref());
        self
    }

    /// Sets the limit for one `sql_query` / `sql_execute` call.
    pub fn with_query_timeout(mut self, timeout: Duration) -> Self {
        self.query_timeout = timeout;
        self
    }

    /// Gives scripts `http_request` and `url_encode`; `None` leaves them out.
    pub fn with_script_http(mut self, http: Option<Arc<ScriptHttp>>) -> Self {
        self.http = http;
        self
    }

    /// Sets what `send_disconnect` and `send_coa` send through.
    pub fn with_dynamic_auth(mut self, dynamic_auth: Option<Arc<dyn DynamicAuthorization>>) -> Self {
        self.dynamic_auth = dynamic_auth;
        self
    }

    /// Creates the global table for one request on `vm`: `protocol`, `phase`,
    /// `client`, and `get_attr`, `set_attr`, `sql_query`, `sql_execute`
    /// bound to this request, and `now_unix` and the other functions that
    /// need no request, shared by every request of this script on the VM.
    /// Other names resolve to the VM's read-only built-in libraries.
    /// `set_attr` appends to `responses`. With a `slot`, the database
    /// functions give up its place while they wait.
    fn request_env(&self, vm: &Vm, request: &LuaRequest, responses: ResponseAttributes, slot: Option<Arc<AccountingSlot>>, failures: Arc<DbFailures>) -> LuaResult<Table> {
        let lua = &vm.lua;
        let data = RequestData {
            attributes: request.attributes.clone(),
            validator: request.validator.clone(),
            responses,
            slot,
            failures,
            protocol: request.protocol,
            phase: request.phase,
            client: request.client.clone(),
        };
        let tls_subject_attrs = request.tls_subject_attrs.as_deref().map(|attrs| subject_attrs_table(lua, attrs)).transpose()?;
        let radsec_subject_attrs = request.radsec_subject.as_deref().map(|subject| subject_attrs_table(lua, &subject.attrs)).transpose()?;
        // This runs Lua code, which the VM's interrupt checks against the script being polled:
        // only under the poll lock is nothing being polled, so another script's overrun is not
        // blamed on this request. The lock also makes requests arriving together make one binder.
        let _serial = vm.poll_lock.lock();
        let cell = &self.binders[vm.index];
        let binder = match cell.get() {
            Some(binder) => binder,
            // A failure is not kept: running out of memory now need not last.
            None => {
                let made = self.make_binder(lua)?;
                cell.get_or_init(|| made)
            }
        };
        binder.call((
            lua.create_userdata(data)?,
            request.protocol,
            request.phase,
            request.client.as_str(),
            request.auth_user.as_deref(),
            request.replayed,
            request.eap_method,
            request.tls_subject.as_deref(),
            tls_subject_attrs,
            request.radsec_subject.as_deref().map(|subject| subject.text.as_str()),
            radsec_subject_attrs,
        ))
    }

    /// Makes this script's request functions on `lua`, and the function that
    /// binds them to a request.
    fn make_binder(&self, lua: &Lua) -> LuaResult<Function> {
        #[cfg(test)]
        self.binders_made.fetch_add(1, Ordering::SeqCst);
        let meta = lua.create_table()?;
        meta.set("__index", lua.globals())?;

        let get_attr = request_function(lua, |lua, request, name: String| match request.attributes.iter().find(|(n, _)| *n == name) {
            Some((_, value)) => attr_to_lua(lua, value),
            None => Ok(Value::Nil),
        })?;

        let get_attrs = request_function(lua, |lua, request, name: String| {
            let values = request.attributes.iter().filter(|(n, _)| *n == name).map(|(_, value)| attr_to_lua(lua, value)).collect::<LuaResult<Vec<_>>>()?;
            lua.create_sequence_from(values)
        })?;

        let set_attr = request_function(lua, |_, request, (name, value): (String, Value)| {
            let value = lua_to_attr(value)?;
            if let Some(validate) = &request.validator {
                validate(&name, &value).map_err(mlua::Error::RuntimeError)?;
            }
            request.responses.lock().unwrap_or_else(PoisonError::into_inner).push((name, value));
            Ok(())
        })?;

        let timeout = self.query_timeout;
        let timed_out = move |what: &str| mlua::Error::RuntimeError(format!("{} timed out: the database calls of one request have {} ms together", what, timeout.as_millis()));

        let db = self.db.clone();
        let sql_query = request_async_function(lua, move |lua, request, (sql, params): (String, Value)| {
            let (db, slot, failed) = (db.clone(), request.slot.clone(), request.failures.clone());
            async move {
                let params = bound_params("sql_query", &sql, &params).inspect_err(|e| failed.note_mistake_raised(e))?;
                if let Some(slot) = &slot {
                    slot.give_up_place();
                }
                // The request's calls share the limit, so that one request cannot wait on the
                // database for many times what a NAS waits for its answer.
                let started = Instant::now();
                let rows = run_within(failed.left(timeout), db.query_bound(&sql, params)).await;
                failed.spend(started.elapsed());
                if let Some(slot) = &slot {
                    slot.take_place().await?;
                }
                note_database_failure(&rows, &failed);
                let rows = rows.map_err(|_| timed_out("sql_query"))?.map_err(|failure| mlua::Error::RuntimeError(failure.message))?;
                let result = lua.create_table()?;
                for (i, row) in rows.iter().enumerate() {
                    let lua_row = lua.create_table()?;
                    for (name, value) in row {
                        lua_row.set(name.as_str(), db_value_to_lua(&lua, value)?)?;
                    }
                    result.set(i + 1, lua_row)?;
                }
                Ok(result)
            }
        })?;

        let script = self.chunk_name.trim_start_matches('=').to_string();
        let log = request_function(lua, move |_, request, values: mlua::Variadic<Value>| {
            log_line(tracing::Level::INFO, format!("script {} ({}/{}, client {}): {}", script, request.protocol, request.phase, request.client, log_message(&values)));
            Ok(())
        })?;

        let (http_request, url_encode) = match self.http.clone() {
            Some(http) => {
                let http_request = request_async_function(lua, move |lua, request, options: Value| {
                    let (http, slot) = (http.clone(), request.slot.clone());
                    async move {
                        let options = http_options(&options)?;
                        let purpose = if slot.is_some() { Purpose::Accounting } else { Purpose::Authentication };
                        if let Some(slot) = &slot {
                            slot.give_up_place();
                        }
                        let result = http.request(options, purpose).await;
                        if let Some(slot) = &slot {
                            slot.take_place().await?;
                        }
                        match result {
                            Ok(response) => {
                                let table = lua.create_table()?;
                                table.set("status", response.status)?;
                                table.set("body", lua.create_string(&response.body)?)?;
                                let headers = lua.create_table()?;
                                for (name, value) in response.headers {
                                    headers.set(name, lua.create_string(&value)?)?;
                                }
                                table.set("headers", headers)?;
                                Ok((Value::Table(table), Value::Nil))
                            }
                            Err(message) => Ok((Value::Nil, Value::String(lua.create_string(&message)?))),
                        }
                    }
                })?;
                let url_encode = lua.create_function(|_, text: mlua::LuaString| url_encode(&text.as_bytes()).map_err(mlua::Error::RuntimeError))?;
                (Some(http_request), Some(url_encode))
            }
            None => (None, None),
        };

        let sender = |name: &'static str, kind: RequestType| {
            let dynamic_auth = self.dynamic_auth.clone();
            request_async_function(lua, move |_, request, (target, attributes): (String, Table)| {
                let (dynamic_auth, slot) = (dynamic_auth.clone(), request.slot.clone());
                async move {
                    let failed = |e: String| mlua::Error::RuntimeError(format!("{name}: {e}"));
                    let dynamic_auth = dynamic_auth.ok_or_else(|| failed("not supported by this program (CoA needs rradius-enterprise)".to_string()))?;
                    let attributes = dynamic_auth_attributes(&attributes)?;
                    let prepared = dynamic_auth.prepare(kind, &target, &attributes).map_err(failed)?;
                    if let Some(slot) = &slot {
                        slot.give_up_place();
                    }
                    let outcome = prepared.send().await;
                    if let Some(slot) = &slot {
                        slot.take_place().await?;
                    }
                    Ok(match outcome {
                        DynamicAuthOutcome::Ack => ("ack", None),
                        DynamicAuthOutcome::Nak(cause) => ("nak", cause),
                        DynamicAuthOutcome::Timeout => ("timeout", None),
                    })
                }
            })
        };
        let send_disconnect = sender("send_disconnect", RequestType::Disconnect)?;
        let send_coa = sender("send_coa", RequestType::Coa)?;

        let db = self.db.clone();
        let sql_execute = request_async_function(lua, move |_, request, (sql, params): (String, Value)| {
            let (db, slot, failed) = (db.clone(), request.slot.clone(), request.failures.clone());
            async move {
                if let Some(item) = find_non_deterministic(&sql) {
                    let error = mlua::Error::RuntimeError(format!(
                        "sql_execute: non-deterministic `{}` is not allowed in writes; pass time values from now_unix() instead",
                        item
                    ));
                    failed.note_mistake_raised(&error);
                    return Err(error);
                }
                let params = bound_params("sql_execute", &sql, &params).inspect_err(|e| failed.note_mistake_raised(e))?;
                if let Some(slot) = &slot {
                    slot.give_up_place();
                }
                let started = Instant::now();
                let affected = db.execute_within(&sql, params, failed.left(timeout)).await;
                failed.spend(started.elapsed());
                if let Some(slot) = &slot {
                    slot.take_place().await?;
                }
                note_database_failure(&affected, &failed);
                let affected = affected.map_err(|_| timed_out("sql_execute"))?.map_err(|failure| mlua::Error::RuntimeError(failure.message))?;
                Ok(affected as f64)
            }
        })?;
        let clock = lua.create_function(|_, ()| Ok(now_unix()))?;
        let totp_verify = lua.create_function(|_, (secret, code): (String, mlua::LuaString)| {
            crate::totp::verify(&secret, &code.as_bytes(), now_unix() as u64).map_err(mlua::Error::RuntimeError)
        })?;
        let random_token = lua.create_function(|_, ()| {
            let mut bytes = [0u8; 16];
            getrandom::fill(&mut bytes).map_err(|e| mlua::Error::RuntimeError(format!("random_token: {e}")))?;
            Ok(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
        })?;

        // PBKDF2 of 4096 rounds: a few milliseconds of this VM's time for each call.
        let wpa_pmk = lua.create_function(|_, (passphrase, ssid): (mlua::LuaString, mlua::LuaString)| {
            let (passphrase, ssid) = (passphrase.to_str().map_err(|_| mlua::Error::RuntimeError("wpa_pmk: the passphrase is not text".to_string()))?, ssid.to_str().map_err(|_| mlua::Error::RuntimeError("wpa_pmk: the SSID is not text".to_string()))?);
            let pmk = crate::common::wpa::pmk_from_passphrase(&passphrase, &ssid).map_err(|e| mlua::Error::RuntimeError(format!("wpa_pmk: {e}")))?;
            Ok(crate::common::hex::encode_hex(&pmk[..]))
        })?;

        lua.load(BINDER).set_name("=rradius").call((
            meta,
            get_attr,
            get_attrs,
            set_attr,
            sql_query,
            sql_execute,
            log,
            send_disconnect,
            send_coa,
            http_request,
            url_encode,
            clock,
            totp_verify,
            random_token,
            wpa_pmk,
        ))
    }

    /// Runs the configured script for one request: `"accept"` accepts, `"ignore"`
    /// ignores, and any other value or a Lua error rejects (errors are logged).
    pub async fn authenticate(&self, request: LuaRequest) -> LuaDecision {
        let failures = Arc::new(DbFailures::default());
        let result = self.run(&request, failures.clone()).await;
        let database_failed = failures.unavailable.load(Ordering::SeqCst);
        let settled = !database_failed && !failures.unknown.load(Ordering::SeqCst);
        let final_result = settled && result.as_ref().map_or_else(|error| failures.ended_by_mistake(error), |_| true);
        match result {
            Ok((outcome, response_attributes)) => {
                // The client gets no acknowledgement and retries; say why.
                if request.phase == "accounting" && outcome != Outcome::Accept {
                    log_line(tracing::Level::WARN, format!("Lua script did not accept accounting ({}, client {})", request.protocol, request.client));
                }
                LuaDecision { outcome, response_attributes, database_failed, final_result }
            }
            Err(e) => {
                log_script_error(&request, &e);
                LuaDecision { outcome: Outcome::Reject, response_attributes: Vec::new(), database_failed, final_result }
            }
        }
    }

    /// Runs the script on the least busy VM in a coroutine of its own, within
    /// the execution limit.
    async fn run(&self, request: &LuaRequest, failures: Arc<DbFailures>) -> LuaResult<(Outcome, Vec<(String, AttrValue)>)> {
        // Before picking a VM: a script waiting here is not running on any of them.
        let slot = match request.phase {
            "accounting" => Some(Arc::new(AccountingSlot::acquire(self.vms.accounting_slots.clone()).await?)),
            _ => None,
        };
        let _place = slot.clone().map(PlaceGuard);
        let vm = self.vms.pick();
        let lua = &vm.0.lua;
        let responses = ResponseAttributes::default();
        let env = self.request_env(vm.0, request, responses.clone(), slot, failures)?;
        let function = lua.load(&self.bytecode[..]).set_name(self.chunk_name.as_str()).set_environment(env).into_function()?;
        let thread = lua.create_thread(function)?;
        let key = thread.to_pointer() as usize;
        let inner = Box::pin(thread.into_async::<Value>(())?);
        let (value, over) = Timed { inner, vm: vm.0, thread: key, used: Duration::ZERO, limit: self.max_execution }.await;
        // Whatever it returned: a script that caught the error may well have returned 'accept'.
        if over {
            return Err(limit_error(self.max_execution));
        }
        let value = value?;
        let outcome = match &value {
            Value::String(s) if s.as_bytes() == &b"accept"[..] => Outcome::Accept,
            Value::String(s) if s.as_bytes() == &b"ignore"[..] => Outcome::Ignore,
            // Only a RADIUS authentication has a way to send one.
            Value::String(s) if s.as_bytes() == &b"challenge"[..] && request.protocol == "radius" && request.phase == "authentication" => Outcome::Challenge,
            _ => Outcome::Reject,
        };
        let attributes = std::mem::take(&mut *responses.lock().unwrap_or_else(PoisonError::into_inner));
        Ok((outcome, attributes))
    }

    /// A request's global table on one of the VMs, for running code snippets
    /// in tests as a script would see them.
    #[cfg(test)]
    pub(crate) fn new_state(&self, request: &LuaRequest) -> LuaResult<RequestState> {
        let vm = self.vms.pick();
        let lua = vm.0.lua.clone();
        let env = self.request_env(vm.0, request, ResponseAttributes::default(), None, Arc::default())?;
        Ok(RequestState { lua, env })
    }
}

/// A request's global table, for tests: `load` runs code with it as globals.
#[cfg(test)]
pub(crate) struct RequestState {
    lua: Lua,
    env: Table,
}

#[cfg(test)]
impl RequestState {
    pub(crate) fn load<'a>(&'a self, chunk: impl mlua::chunk::AsChunk + 'a) -> mlua::chunk::Chunk<'a> {
        self.lua.load(chunk).set_environment(self.env.clone())
    }

    pub(crate) fn globals(&self) -> Table {
        self.env.clone()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #[test]
    fn test_mistakes_of_one_request_are_all_remembered_up_to_a_limit() {
        let failures = super::DbFailures::default();
        let ended_by = |message: &str| failures.ended_by_mistake(&mlua::Error::RuntimeError(message.to_string()));
        failures.note_mistake("first");
        failures.note_mistake("second");
        assert!(ended_by("first") && ended_by("second") && !ended_by("third"));
        for number in 0..super::REMEMBERED_MISTAKES {
            failures.note_mistake(&format!("mistake {number}"));
        }
        assert!(!ended_by("first"), "the oldest goes, so a script cannot fill the memory");
        assert!(ended_by(&format!("mistake {}", super::REMEMBERED_MISTAKES - 1)));
    }

    use super::*;
    use crate::test_support::{logged_errors, test_db, test_vms};

    async fn engine_with_test_db() -> LuaAuthEngine {
        let db = test_db();
        LuaAuthEngine::new(test_vms(), db, "return 'accept'").unwrap()
    }

    fn test_request() -> LuaRequest {
        LuaRequest::new("radius", "authentication", vec![])
    }

    /// The bundled SQLite is built with the query planner stability guarantee, or SQLite 3.54
    /// plans some statements again on every read (see `.cargo/config.toml`).
    #[tokio::test]
    async fn test_sqlite_built_to_keep_query_plans() {
        let rows = test_db().query_bound("PRAGMA compile_options", vec![]).await.unwrap_or_else(|failure| panic!("{}", failure.message));
        let options: Vec<&DbValue> = rows.iter().flat_map(|row| row.iter().map(|(_, value)| value)).collect();
        assert!(options.contains(&&DbValue::Text("ENABLE_QPSG".to_string())), "{options:?}");
    }

    #[tokio::test]
    async fn test_sql_query_returns_rows() {
        let engine = engine_with_test_db().await;
        let lua = engine.new_state(&test_request()).unwrap();
        lua.load(
            "sql_execute('CREATE TABLE rows_users (id INTEGER, name TEXT, score REAL, note TEXT)')
             sql_execute(\"INSERT INTO rows_users VALUES (1, 'alice', 1.5, 'admin'), (2, 'bob', 2.5, NULL)\")",
        )
        .exec_async()
        .await
        .unwrap();

        let (count, id1, name1, score2, note2_is_nil): (i64, i64, String, f64, bool) = lua
            .load(
                "local rows = sql_query('SELECT id, name, score, note FROM rows_users ORDER BY id')
                 return #rows, rows[1].id, rows[1].name, rows[2].score, rows[2].note == nil",
            )
            .eval_async()
            .await
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(id1, 1);
        assert_eq!(name1, "alice");
        assert_eq!(score2, 2.5);
        assert!(note2_is_nil);
    }

    #[tokio::test]
    async fn test_sql_query_no_rows_returns_empty_array() {
        let engine = engine_with_test_db().await;
        let lua = engine.new_state(&test_request()).unwrap();
        let count: i64 = lua
            .load(
                "sql_execute('CREATE TABLE empty_users (id INTEGER)')
                 return #sql_query('SELECT id FROM empty_users')",
            )
            .eval_async()
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_sql_query_blob_and_integer_types() {
        let engine = engine_with_test_db().await;
        let lua = engine.new_state(&test_request()).unwrap();
        // Luau numbers are doubles, so INTEGER arrives as a Lua number.
        let (n, blob): (i64, mlua::LuaString) = lua
            .load(
                "local row = sql_query(\"SELECT 42 AS n, x'0001ff' AS b\")[1]
                 return row.n, row.b",
            )
            .eval_async()
            .await
            .unwrap();
        assert_eq!(n, 42);
        assert_eq!(blob.as_bytes().to_vec(), vec![0x00, 0x01, 0xff]);
    }

    #[tokio::test]
    async fn test_sql_query_invalid_sql_raises_lua_error() {
        let engine = engine_with_test_db().await;
        let lua = engine.new_state(&test_request()).unwrap();
        let err = lua
            .load("return sql_query('SELEC broken')")
            .eval_async::<mlua::Value>()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("syntax error"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn test_totp_verify_from_a_script() {
        const SECRET: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        let now = now_unix() as u64;
        let code = crate::totp::code_at(SECRET, now);
        let lua = engine_with_test_db().await.new_state(&test_request()).unwrap();
        let step: Option<u64> = lua.load(format!("return totp_verify('{SECRET}', '{code}')")).eval_async().await.unwrap();
        // The step can have moved on since `now`, which is inside the window.
        assert!(step.is_some_and(|step| step == now / 30), "{step:?}");
        let wrong: Option<u64> = lua.load(format!("return totp_verify('{SECRET}', 'abcdef')")).eval_async().await.unwrap();
        assert_eq!(wrong, None);
        assert!(lua.load("return totp_verify('not base32!', '123456')").exec_async().await.is_err());
    }

    #[tokio::test]
    async fn test_wpa_pmk() -> Result<(), String> {
        let lua = engine_with_test_db().await.new_state(&test_request()).map_err(|e| e.to_string())?;
        let pmk: String = lua.load("return wpa_pmk('password', 'IEEE')").eval_async().await.map_err(|e| e.to_string())?;
        assert_eq!(pmk, "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e");
        for call in ["wpa_pmk('short', 'IEEE')", "wpa_pmk('password', '')"] {
            let error = lua.load(format!("return {call}")).exec_async().await.err().ok_or_else(|| format!("{call} did not fail"))?;
            assert!(!error.to_string().contains("short") || call.contains("''"), "{error}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_random_token() {
        let lua = engine_with_test_db().await.new_state(&test_request()).unwrap();
        let (a, b): (String, String) = lua.load("return random_token(), random_token()").eval_async().await.unwrap();
        assert_eq!((a.len(), b.len()), (32, 32));
        assert!(a.bytes().chain(b.bytes()).all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn test_challenge_outcome() {
        assert_eq!(decide("return 'challenge'").await.outcome, Outcome::Challenge);
        for (protocol, phase) in [("tacacs", "authentication"), ("radius", "authorization"), ("radius", "accounting")] {
            let engine = LuaAuthEngine::new(test_vms(), test_db(), "return 'challenge'").unwrap();
            let decision = engine.authenticate(LuaRequest::new(protocol, phase, vec![])).await;
            assert_eq!(decision.outcome, Outcome::Reject, "{protocol}/{phase}");
        }
    }

    async fn decide(script: &str) -> LuaDecision {
        let db = test_db();
        LuaAuthEngine::new(test_vms(), db, script).unwrap().authenticate(test_request()).await
    }

    #[tokio::test]
    async fn test_only_accept_string_is_accepted() {
        assert!(decide("return 'accept'").await.accepted());
        assert!(!decide("return 'reject'").await.accepted());
        assert!(!decide("return nil").await.accepted());
        assert!(!decide("return 'ACCEPT'").await.accepted());
        assert!(!decide("local x = 1").await.accepted());
    }

    #[tokio::test]
    async fn test_script_error_rejects() {
        let decision = decide("set_attr('Filter-Id', 'x') error('boom-logged-7f3a')").await;
        assert!(!decision.accepted());
        assert!(decision.response_attributes.is_empty());
        let logged = logged_errors().lock().unwrap();
        assert!(
            logged.iter().any(|line| line.starts_with("Lua script error (radius/authentication)") && line.contains("boom-logged-7f3a")),
            "the error was not logged: {logged:?}"
        );
    }

    #[tokio::test]
    async fn test_get_attr_set_attr_and_globals() {
        let db = test_db();
        let engine = LuaAuthEngine::new(test_vms(), db, "
            if protocol ~= 'tacacs' or phase ~= 'authorization' then return 'reject' end
            if get_attr('missing') ~= nil then return 'reject' end
            set_attr('priv-lvl', get_attr('level') + 1)
            set_attr('reply', get_attr('user') .. '!')
            return 'accept'").unwrap();
        let request = LuaRequest::new("tacacs", "authorization", vec![
            ("user".to_string(), AttrValue::Bytes(b"alice".to_vec())),
            ("level".to_string(), AttrValue::Number(14.0)),
        ]);
        let decision = engine.authenticate(request).await;
        assert!(decision.accepted());
        assert_eq!(decision.response_attributes, vec![
            ("priv-lvl".to_string(), AttrValue::Number(15.0)),
            ("reply".to_string(), AttrValue::Bytes(b"alice!".to_vec())),
        ]);
    }

    #[tokio::test]
    async fn test_validator_rejects_unknown_attribute() {
        let db = test_db();
        let engine = LuaAuthEngine::new(test_vms(), db, "set_attr('No-Such-Attr', 'x') return 'accept'").unwrap();
        let mut request = test_request();
        request.validator = Some(Arc::new(|name: &str, _: &AttrValue| Err(format!("unknown attribute '{}'", name))));
        assert!(!engine.authenticate(request).await.accepted());
    }

    #[tokio::test]
    async fn test_requests_do_not_share_state() {
        let db = test_db();
        let engine = LuaAuthEngine::new(test_vms(), db, "if leaked then return 'reject' end leaked = true return 'accept'").unwrap();
        assert!(engine.authenticate(test_request()).await.accepted());
        assert!(engine.authenticate(test_request()).await.accepted());
    }

    // A long Lua loop (2 s, under a raised execution limit) must not hold up
    // other requests.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_waiting_request_does_not_block_others() {
        let db = test_db();
        let engine = Arc::new(LuaAuthEngine::new(test_vms(), db, "
            if get_attr('slow') then SLOW_LOOP end
            return 'accept'".replace("SLOW_LOOP", "local t = now_unix() while now_unix() - t < 2 do end")).unwrap()
            .with_max_execution(Duration::from_secs(10)));

        let slow_engine = engine.clone();
        let slow = tokio::spawn(async move {
            let request = LuaRequest::new("radius", "authentication", vec![("slow".to_string(), AttrValue::Number(1.0))]);
            slow_engine.authenticate(request).await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!slow.is_finished(), "the slow request should still be running");

        let other = tokio::time::timeout(Duration::from_secs(1), engine.authenticate(test_request())).await;
        assert!(other.expect("the fast request was blocked").accepted());
        assert!(slow.await.unwrap().accepted());
    }

    #[tokio::test]
    async fn test_client_global() {
        let db = test_db();
        for name in ["nas7", "10.0.0.0/8"] {
            let engine = LuaAuthEngine::new(test_vms(), db.clone(), format!("return client == '{}' and 'accept' or 'reject'", name)).unwrap();
            let mut request = test_request();
            request.client = name.to_string();
            assert!(engine.authenticate(request).await.accepted(), "client global was not '{}'", name);
        }
        // Unset client is an empty string, so it does not match a name.
        let engine = LuaAuthEngine::new(test_vms(), db, "return client == '' and 'accept' or 'reject'").unwrap();
        assert!(engine.authenticate(test_request()).await.accepted());
    }

    #[tokio::test]
    async fn test_sql_execute_writes_and_returns_count() {
        let engine = engine_with_test_db().await;
        let lua = engine.new_state(&test_request()).unwrap();
        let (affected, count): (i64, i64) = lua
            .load(
                "sql_execute('CREATE TABLE exec_sessions (id TEXT)')
                 local n = sql_execute(\"INSERT INTO exec_sessions VALUES ('s-1'), ('s-2')\")
                 return n, #sql_query('SELECT id FROM exec_sessions')",
            )
            .eval_async()
            .await
            .unwrap();
        assert_eq!((affected, count), (2, 2));
    }

    #[tokio::test]
    async fn test_sql_execute_rejects_non_deterministic_sql() {
        let engine = engine_with_test_db().await;
        let lua = engine.new_state(&test_request()).unwrap();
        lua.load("sql_execute('CREATE TABLE nondet_t (v)')").exec_async().await.unwrap();
        let cases = [
            ("INSERT INTO nondet_t VALUES (date('2024-01-01'))", "date"),
            ("INSERT INTO nondet_t VALUES (DateTime ('now'))", "datetime"),
            ("INSERT INTO nondet_t VALUES (julianday('2024-01-01'))", "julianday"),
            ("INSERT INTO nondet_t VALUES (now())", "now"),
            ("INSERT INTO nondet_t VALUES (random())", "random"),
            ("INSERT INTO nondet_t VALUES (RANDOMBLOB(4))", "randomblob"),
            ("INSERT INTO nondet_t VALUES (strftime('%s', 1700000000))", "strftime"),
            ("INSERT INTO nondet_t VALUES (time('12:00'))", "time"),
            ("INSERT INTO nondet_t VALUES (timediff('a', 'b'))", "timediff"),
            ("INSERT INTO nondet_t VALUES (unixepoch())", "unixepoch"),
            ("INSERT INTO nondet_t VALUES (current_timestamp)", "current_timestamp"),
            ("INSERT INTO nondet_t VALUES (Current_Date)", "current_date"),
            ("INSERT INTO nondet_t VALUES (CURRENT_TIME)", "current_time"),
        ];
        for (sql, item) in cases {
            lua.globals().set("stmt", sql).unwrap();
            let err = lua.load("return sql_execute(stmt)").eval_async::<mlua::Value>().await.unwrap_err().to_string();
            assert!(err.contains(&format!("`{}`", item)), "{sql}: unexpected error: {err}");
        }
        let count: i64 = lua.load("return #sql_query('SELECT v FROM nondet_t')").eval_async().await.unwrap();
        assert_eq!(count, 0, "rejected statements must not be sent");
    }

    /// Runs `script` in a request's environment; its result, or the error text.
    async fn script_result(script: &str) -> Result<String, String> {
        let lua = engine_with_test_db().await.new_state(&test_request()).unwrap();
        lua.load(script).eval_async::<String>().await.map_err(|e| e.to_string())
    }

    #[tokio::test]
    async fn test_sql_parameter_is_not_interpreted_as_sql() {
        let result = script_result(
            "sql_execute('CREATE TABLE IF NOT EXISTS param_injection (username TEXT)')
             sql_execute(\"INSERT INTO param_injection VALUES ('alice'), ('bob')\")
             return tostring(#sql_query('SELECT username FROM param_injection WHERE username = ?', {\"x' OR '1'='1\"}))",
        )
        .await;
        assert_eq!(result.as_deref(), Ok("0"));
    }

    #[tokio::test]
    async fn test_sql_parameter_with_a_quote() {
        let result = script_result(
            "sql_execute('CREATE TABLE IF NOT EXISTS param_quote (username TEXT)')
             local written = sql_execute('INSERT INTO param_quote VALUES (?)', {\"o'brien\"})
             local rows = sql_query('SELECT username FROM param_quote WHERE username = ?', {\"o'brien\"})
             return written .. ' ' .. #rows .. ' ' .. rows[1].username",
        )
        .await;
        assert_eq!(result.as_deref(), Ok("1 1 o'brien"));
    }

    #[tokio::test]
    async fn test_sql_calls_without_parameters() {
        let result = script_result(
            "return sql_query('SELECT 1 AS one')[1].one .. ' ' .. sql_query('SELECT 1 AS one', nil)[1].one",
        )
        .await;
        assert_eq!(result.as_deref(), Ok("1 1"));
    }

    #[tokio::test]
    async fn test_sql_parameter_types() {
        for (value, expected) in [
            ("'text'", "text"),
            ("'\\xff\\xfe'", "blob"),
            ("42", "integer"),
            ("1.5", "real"),
            ("2^60", "integer"),
            ("-0", "real"),
            ("1e300", "real"),
            ("true", "integer"),
        ] {
            let script = format!("return sql_query('SELECT typeof(?) AS t', {{{value}}})[1].t");
            assert_eq!(script_result(&script).await.as_deref(), Ok(expected), "{value}");
        }
        let script = "return sql_query('SELECT ? AS a, ? AS b', {true, false})[1].a .. sql_query('SELECT ? AS b', {false})[1].b";
        assert_eq!(script_result(script).await.as_deref(), Ok("10"));
    }

    #[tokio::test]
    async fn test_sql_unsupported_parameters() {
        for value in ["{}", "print", "0/0", "1/0", "-1/0"] {
            let error = script_result(&format!("return sql_query('SELECT ?', {{{value}}})")).await.unwrap_err();
            assert!(error.contains("sql_query:") && error.contains("parameter 1"), "{value}: {error}");
        }
        let error = script_result("return sql_execute('SELECT ?, ?', {1, {}})").await.unwrap_err();
        assert!(error.contains("sql_execute:") && error.contains("parameter 2"), "{error}");
        for call in ["sql_query('SELECT ?')", "sql_query('SELECT ?', nil)", "sql_execute('DELETE FROM no_such_table WHERE id = ?')", "sql_execute('DELETE FROM no_such_table WHERE id = ?', nil)"] {
            let error = script_result(&format!("return {call}")).await.unwrap_err();
            assert!(error.contains("1 placeholder, 0 values"), "{call}: {error}");
        }
        assert_eq!(script_result("return sql_query(\"SELECT '?' AS mark\")[1].mark").await.unwrap(), "?");
        for argument in ["'x'", "1", "true"] {
            let error = script_result(&format!("return sql_query('SELECT ?', {argument})")).await.unwrap_err();
            assert!(error.contains("sql_query:") && error.contains("array"), "{argument}: {error}");
        }
    }

    #[tokio::test]
    async fn test_sql_parameter_counts() {
        let setup = script_result(
            "sql_execute('CREATE TABLE IF NOT EXISTS param_named (\"a?\" TEXT, x INTEGER)')
             sql_execute('DELETE FROM param_named')
             sql_execute('INSERT INTO param_named VALUES (?, ?)', {'v', 1})
             return 'done'",
        )
        .await;
        assert_eq!(setup.as_deref(), Ok("done"));
        for (sql, values, expected) in [
            ("SELECT ? , ?", "{1}", Err("2 placeholders, 1 value")),
            ("SELECT ?", "{1, 2}", Err("1 placeholder, 2 values")),
            ("SELECT ?, ?", "{1, nil}", Err("2 placeholders, 1 value")),
            ("SELECT ''''", "{1}", Err("0 placeholders, 1 value")),
            ("SELECT \"a?\" FROM param_named WHERE x = ?", "{1}", Ok(())),
            ("SELECT '?' , ?", "{1}", Ok(())),
            ("SELECT 'it''s ?' , ?", "{1}", Ok(())),
            ("SELECT 1 -- ?", "{}", Ok(())),
            ("SELECT ? /* ? */", "{1}", Ok(())),
            ("SELECT 1 AS \"a?\", ?", "{1}", Ok(())),
            ("SELECT 1 AS `a?`, ?", "{1}", Ok(())),
            ("SELECT 1 AS [a?], ?", "{1}", Ok(())),
            ("SELECT ?", "{1, nil}", Ok(())),
        ] {
            let result = script_result(&format!("return tostring(#sql_query([=[{sql}]=], {values}))")).await;
            match expected {
                Ok(()) => assert_eq!(result.as_deref(), Ok("1"), "{sql}"),
                Err(counts) => {
                    let error = result.unwrap_err();
                    assert!(error.contains(counts), "{sql}: {error}");
                }
            }
        }
    }

    #[tokio::test]
    async fn test_sql_parameter_count_error_sends_no_statement() {
        let result = script_result(
            "sql_execute('CREATE TABLE IF NOT EXISTS param_counted (a TEXT, b TEXT)')
             pcall(sql_execute, 'INSERT INTO param_counted VALUES (?, ?)', {'only one'})
             pcall(sql_execute, 'INSERT INTO param_counted VALUES (?, ?)', {'a table next', {}})
             return tostring(#sql_query('SELECT a FROM param_counted'))",
        )
        .await;
        assert_eq!(result.as_deref(), Ok("0"));
    }

    #[tokio::test]
    async fn test_sql_parameter_holes() {
        for values in ["{1, nil, 3}", "{1, x = 2}", "{[2] = 1}", "{[1.5] = 1}"] {
            let error = script_result(&format!("return sql_query('SELECT ?, ?, ?', {values})")).await.unwrap_err();
            assert!(error.contains("sql_query:") && error.contains("array"), "{values}: {error}");
        }
    }

    #[tokio::test]
    async fn test_sql_unsupported_placeholder_forms() {
        for sql in ["SELECT ?1", "SELECT :name", "SELECT @name", "SELECT $name"] {
            let error = script_result(&format!("return sql_query('{sql}', {{1}})")).await.unwrap_err();
            assert!(error.contains("sql_query:") && error.contains("plain ?"), "{sql}: {error}");
        }
        // Right after a word, which SQLite still reads as a parameter, and would then be NULL.
        // And `$` after the first character, which SQLite reads as part of the parameter's name.
        for sql in ["SELECT 1 WHERE 1 LIKE:pat", "SELECT 1 WHERE 1 LIKE@pat", "SELECT $$name", "SELECT :$name", "SELECT @$name", "SELECT $$"] {
            let error = script_result(&format!("return sql_query('{sql}', {{}})")).await.unwrap_err();
            assert!(error.contains("sql_query:") && error.contains("plain ?"), "{sql}: {error}");
        }
        // Not placeholders: inside text, or part of a name.
        for sql in ["SELECT ':name' AS a, ?", "SELECT 1 AS a$b, ?", "SELECT 1 AS a$$b, ?", "SELECT 1 AS a$, ?"] {
            let result = script_result(&format!("sql_query(\"{sql}\", {{1}}) return 'ran'")).await;
            assert_eq!(result.as_deref(), Ok("ran"), "{sql}");
        }
    }

    #[tokio::test]
    async fn test_sql_non_determinism_check_covers_only_the_statement() {
        let result = script_result(
            "sql_execute('CREATE TABLE IF NOT EXISTS param_nondet (v)')
             sql_execute('INSERT INTO param_nondet VALUES (?)', {'random()'})
             return sql_query('SELECT v FROM param_nondet')[1].v",
        )
        .await;
        assert_eq!(result.as_deref(), Ok("random()"));
        let error = script_result("return sql_execute('INSERT INTO param_nondet VALUES (random() + ?)', {1})").await.unwrap_err();
        assert!(error.contains("non-deterministic"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_sql_parameter_error_keeps_the_accounting_place() {
        for function in ["sql_query", "sql_execute"] {
            let vm = GatedVm::new();
            let failing = vm.engine(&format!("pcall({function}, 'SELECT ?', {{{{}}}}) note('failed') while not gate_open() do end return 'accept'"));
            let failing = tokio::spawn(async move { failing.authenticate(accounting_request()).await });
            vm.wait_for_note("failed").await;
            let second = vm.engine("return 'accept'");
            let second = tokio::spawn(async move { second.authenticate(accounting_request()).await });
            tokio::time::sleep(NOT_HELD_BACK).await;
            assert!(!second.is_finished(), "the failed {function} gave up the place");
            vm.open_gate();
            assert!(second.await.unwrap().accepted());
            assert!(failing.await.unwrap().accepted());
        }
    }

    #[tokio::test]
    async fn test_sql_execute_allows_similar_text() {
        let engine = engine_with_test_db().await;
        let lua = engine.new_state(&test_request()).unwrap();
        let affected: i64 = lua
            .load(
                "sql_execute('CREATE TABLE similar_notes (a TEXT, b TEXT)')
                 sql_execute('CREATE TABLE similar_sessions (last_time INTEGER, update_date INTEGER)')
                 sql_execute('INSERT INTO similar_sessions VALUES (1, 2)')
                 return sql_execute(\"INSERT INTO similar_notes VALUES ('random( text', 'now')\")
                      + sql_execute('UPDATE similar_sessions SET last_time = 5, update_date = 6')",
            )
            .eval_async()
            .await
            .unwrap();
        assert_eq!(affected, 2);
    }

    #[tokio::test]
    async fn test_now_unix_timestamp() {
        let engine = engine_with_test_db().await;
        let lua = engine.new_state(&test_request()).unwrap();
        let stored: i64 = lua
            .load(
                "sql_execute('CREATE TABLE now_sessions (id TEXT, ts INTEGER)')
                 sql_execute(\"INSERT INTO now_sessions VALUES ('s-1', \" .. now_unix() .. \")\")
                 return sql_query('SELECT ts FROM now_sessions')[1].ts",
            )
            .eval_async()
            .await
            .unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        assert!((now - stored).abs() < 5, "stored {stored}, now {now}");
    }

    /// Inserting this many generated rows keeps hiqlite's single writer busy for a while.
    const BUSY_INSERT: &str = "INSERT INTO timeout_busy SELECT x FROM \
        (WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 3000000) SELECT x FROM c)";

    /// Writes waiting at the same time go in one transaction, and each still gets its own count.
    #[tokio::test]
    async fn test_writes_at_the_same_time() {
        let db = test_db();
        db.execute("CREATE TABLE batch_rows (n INTEGER PRIMARY KEY)").await.unwrap();
        let writes = (0..200).map(|n| {
            let db = db.clone();
            tokio::spawn(async move { db.execute_bound("INSERT INTO batch_rows VALUES (?)", vec![hiqlite::Param::Integer(n)]).await.map_err(|f| f.message) })
        });
        for write in futures::future::join_all(writes).await {
            assert_eq!(write.unwrap(), Ok(1));
        }
        let rows = db.query("SELECT count(*) AS n FROM batch_rows").await.unwrap();
        assert!(matches!(rows[0][0].1, DbValue::Integer(200)), "{rows:?}");
    }

    /// A statement that fails takes only its own call with it.
    #[tokio::test]
    async fn test_one_failing_statement_among_others() {
        let db = test_db();
        db.execute("CREATE TABLE batch_keys (k TEXT PRIMARY KEY)").await.unwrap();
        db.execute("INSERT INTO batch_keys VALUES ('taken')").await.unwrap();
        for round in 0..20 {
            let writes = (0..20).map(|n| {
                let db = db.clone();
                let key = if n == 7 { "taken".to_string() } else { format!("r{round}-{n}") };
                tokio::spawn(async move { db.execute_bound("INSERT INTO batch_keys VALUES (?)", vec![hiqlite::Param::Text(key)]).await })
            });
            for (n, write) in futures::future::join_all(writes).await.into_iter().enumerate() {
                match (n, write.unwrap()) {
                    (7, Err(failure)) => assert_eq!(failure.kind, super::DbFailureKind::Mistake, "{}", failure.message),
                    (7, Ok(_)) => panic!("the taken key went in"),
                    (_, result) => assert_eq!(result.map_err(|f| f.message), Ok(1)),
                }
            }
        }
        let rows = db.query("SELECT count(*) AS n FROM batch_keys").await.unwrap();
        assert!(matches!(rows[0][0].1, DbValue::Integer(381)), "{rows:?}");
    }

    /// Statements that return rows, or end or undo a transaction, are not batched: alone they fail
    /// as they always did, and they cannot commit or undo the work of others.
    #[tokio::test]
    async fn test_statements_that_go_alone() {
        let db = test_db();
        db.execute("CREATE TABLE alone_counter (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)").await.unwrap();
        db.execute("INSERT INTO alone_counter VALUES (1, 0)").await.unwrap();
        for _ in 0..10 {
            let writes = (0..12).map(|n| {
                let db = db.clone();
                let sql = match n {
                    3 => "UPDATE alone_counter SET n = n + 100 WHERE id = 1 RETURNING n",
                    5 => "COMMIT",
                    7 => "ROLLBACK",
                    _ => "UPDATE alone_counter SET n = n + 1 WHERE id = 1",
                };
                tokio::spawn(async move { (n, db.execute_bound(sql, Vec::new()).await) })
            });
            for write in futures::future::join_all(writes).await {
                match write.unwrap() {
                    (3 | 5 | 7, result) => assert!(result.is_err(), "fails as it does alone"),
                    (_, result) => assert_eq!(result.map_err(|f| f.message), Ok(1)),
                }
            }
        }
        let rows = db.query("SELECT n FROM alone_counter").await.unwrap();
        // Nine plain updates in each of ten rounds, once each, and the RETURNING one ten times: alone,
        // hiqlite applies it and then reports the row it returned as an error, as it always has.
        assert!(matches!(rows[0][0].1, DbValue::Integer(1090)), "{rows:?}");
    }

    #[test]
    fn test_which_statements_share_a_transaction() {
        for sql in ["INSERT INTO t VALUES (1)", "  -- note\n update t SET a = 1", "DELETE FROM t", "REPLACE INTO t VALUES (1)", "insert or ignore into t values (?)"] {
            assert!(super::can_share_a_transaction(sql), "{sql}");
        }
        for sql in ["SELECT 1", "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x", "UPDATE t SET a = 1 RETURNING a", "INSERT INTO t VALUES (1) returning id", "COMMIT", "END", "ROLLBACK", "BEGIN", "SAVEPOINT s", "RELEASE s", "CREATE TABLE t (a)", "PRAGMA x", "\"insert\" INTO t VALUES (1)", ""] {
            assert!(!super::can_share_a_transaction(sql), "{sql}");
        }
    }

    /// Stands in for hiqlite: a statement with "bad" in it fails, alone or in a transaction, and
    /// `mode` decides what a transaction answers otherwise. Keeps what it was asked.
    #[derive(Clone, Default)]
    struct FakeCommitter {
        calls: Arc<Mutex<Vec<String>>>,
        mode: Arc<Mutex<&'static str>>,
    }

    impl super::Committer for FakeCommitter {
        fn commit_one(&self, sql: String, _: Vec<hiqlite::Param>) -> impl Future<Output = Result<usize, hiqlite::Error>> + Send {
            self.calls.lock().unwrap().push(format!("one {sql}"));
            let mode = self.mode.lock().map_or("ok", |mode| *mode);
            async move {
                match mode {
                    "hang" => std::future::pending().await,
                    "slow" => {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        Ok(1)
                    }
                    _ if sql.contains("bad") => Err(hiqlite::Error::ConstraintViolation("UNIQUE".into())),
                    _ => Ok(1),
                }
            }
        }

        fn commit_together(&self, statements: Vec<(String, Vec<hiqlite::Param>)>) -> impl Future<Output = Result<Vec<Result<usize, hiqlite::Error>>, hiqlite::Error>> + Send {
            self.calls.lock().unwrap().push(format!("together {}", statements.len()));
            let mode = *self.mode.lock().unwrap();
            async move {
                match mode {
                    "unavailable" => Err(hiqlite::Error::Connect("no leader".into())),
                    "hang" => std::future::pending().await,
                    "slow" => {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        Ok(statements.iter().map(|_| Ok(1)).collect())
                    }
                    "short" => Ok(vec![Ok(1)]),
                    _ if statements.iter().any(|(sql, _)| sql.contains("bad")) => Err(hiqlite::Error::Transaction("UNIQUE".into())),
                    _ => Ok(statements.iter().map(|_| Ok(1)).collect()),
                }
            }
        }
    }

    type Answer = tokio::sync::oneshot::Receiver<Result<usize, super::DbFailure>>;

    fn pending(sqls: &[&str]) -> (Vec<super::PendingWrite>, Vec<Answer>) {
        sqls.iter()
            .map(|sql| {
                let (reply, answer) = tokio::sync::oneshot::channel();
                (super::PendingWrite::new(sql.to_string(), Vec::new(), reply), answer)
            })
            .unzip()
    }

    async fn answers(answers: Vec<Answer>) -> Vec<Result<usize, super::DbFailureKind>> {
        let mut out = Vec::new();
        for answer in answers {
            out.push(answer.await.unwrap().map_err(|failure| failure.kind));
        }
        out
    }

    #[tokio::test]
    async fn test_batch_committed_together() {
        let fake = FakeCommitter::default();
        let (batch, waiting) = pending(&["a", "b", "c"]);
        super::commit_batch(&fake, batch).await;
        assert_eq!(answers(waiting).await, [Ok(1), Ok(1), Ok(1)]);
        assert_eq!(*fake.calls.lock().unwrap(), ["together 3"]);
    }

    /// One failing statement among eight: halves until it is alone, not eight single commits.
    #[tokio::test]
    async fn test_failed_batch_is_split() {
        let fake = FakeCommitter::default();
        let (batch, waiting) = pending(&["a", "b", "c", "d", "e", "bad", "g", "h"]);
        super::commit_batch(&fake, batch).await;
        let kinds = answers(waiting).await;
        assert_eq!(kinds[5], Err(super::DbFailureKind::Mistake));
        assert!(kinds.iter().enumerate().all(|(n, kind)| n == 5 || *kind == Ok(1)), "{kinds:?}");
        let calls = fake.calls.lock().unwrap().clone();
        assert_eq!(calls.iter().filter(|call| call.starts_with("one")).count(), 2, "{calls:?}");
    }

    #[tokio::test]
    async fn test_unavailable_cluster_fails_the_whole_batch_once() {
        let fake = FakeCommitter::default();
        *fake.mode.lock().unwrap() = "unavailable";
        let (batch, waiting) = pending(&["a", "b", "c"]);
        super::commit_batch(&fake, batch).await;
        assert_eq!(answers(waiting).await, vec![Err(super::DbFailureKind::Unavailable); 3]);
        assert_eq!(*fake.calls.lock().unwrap(), ["together 3"], "nothing tried again");
    }

    #[tokio::test]
    async fn test_results_that_do_not_match_are_not_committed_again() {
        let fake = FakeCommitter::default();
        *fake.mode.lock().unwrap() = "short";
        let (batch, waiting) = pending(&["a", "b", "c"]);
        super::commit_batch(&fake, batch).await;
        assert_eq!(answers(waiting).await, vec![Err(super::DbFailureKind::Unknown); 3]);
        assert_eq!(*fake.calls.lock().unwrap(), ["together 3"]);
    }

    /// Writes queued while the writer was busy go out together, in arrival order.
    #[tokio::test]
    async fn test_waiting_writes_go_in_one_batch() {
        let fake = FakeCommitter::default();
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let (batch, waiting) = pending(&["a", "b", "c"]);
        for write in batch {
            assert!(sender.try_send(write).is_ok());
        }
        drop(sender);
        super::write_batches(fake.clone(), receiver, Duration::from_secs(30)).await;
        assert_eq!(answers(waiting).await, [Ok(1), Ok(1), Ok(1)]);
        assert_eq!(*fake.calls.lock().unwrap(), ["together 3"]);
    }

    /// A transaction the cluster never answers gives its place back at the deadline, and its
    /// callers hear that the cluster did not take their writes; the next ones still go out.
    #[tokio::test]
    async fn test_batch_past_its_deadline() -> Result<(), String> {
        let fake = FakeCommitter::default();
        *fake.mode.lock().map_err(|e| e.to_string())? = "hang";
        let (sender, receiver) = tokio::sync::mpsc::channel(super::WAITING_WRITES);
        let writer = tokio::spawn(super::write_batches(fake.clone(), receiver, Duration::from_millis(50)));
        let mut stuck = Vec::new();
        for round in 0..super::BATCHES_IN_FLIGHT + 1 {
            let (batch, waiting) = pending(&["a", "b"]);
            for write in batch {
                super::queue_write(&sender, write).map_err(|e| e.to_string())?;
            }
            stuck.extend(waiting);
            tokio::time::sleep(Duration::from_millis(5)).await;
            assert!(round <= super::BATCHES_IN_FLIGHT);
        }
        for answer in stuck {
            let Err(_) = answer.await else { return Err("a stuck write was answered".to_string()) };
        }
        *fake.mode.lock().map_err(|e| e.to_string())? = "ok";
        let (batch, waiting) = pending(&["c"]);
        for write in batch {
            super::queue_write(&sender, write).map_err(|e| e.to_string())?;
        }
        assert_eq!(answers(waiting).await, [Ok(1)], "places were given back");
        drop(sender);
        writer.await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// One write and the receiving end of its reply, with its state, as `execute_within` makes them.
    fn queued(sql: &str) -> (super::PendingWrite, tokio::sync::oneshot::Receiver<Result<usize, super::DbFailure>>, Arc<std::sync::atomic::AtomicU8>) {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let write = super::PendingWrite::new(sql.to_string(), Vec::new(), reply);
        let state = write.state.clone();
        (write, answer, state)
    }

    /// Handed over and committed after its caller's time ran out: the caller hears it was, so
    /// that nothing it was told failed is in the database.
    #[tokio::test]
    async fn test_handed_over_write_waits_for_its_outcome() -> Result<(), String> {
        let fake = FakeCommitter::default();
        *fake.mode.lock().map_err(|e| e.to_string())? = "slow";
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let writer = tokio::spawn(super::write_batches(fake.clone(), receiver, Duration::from_secs(30)));
        let (write, answer, state) = queued("a");
        super::queue_write(&sender, write).map_err(|e| e.to_string())?;
        let started = std::time::Instant::now();
        let outcome = super::wait_for_write(answer, &state, Duration::from_millis(50)).await;
        assert!(matches!(outcome, Ok(Ok(1))), "{outcome:?}");
        assert!(started.elapsed() >= Duration::from_millis(250), "it waited for the commit");
        drop(sender);
        writer.await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Given up while it waits behind other transactions: a timeout, and never handed over.
    #[tokio::test]
    async fn test_write_given_up_before_it_is_handed_over() -> Result<(), String> {
        let (write, answer, state) = queued("a");
        // Nothing takes it from the queue yet.
        let outcome = super::wait_for_write(answer, &state, Duration::from_millis(20)).await;
        assert!(matches!(outcome, Err(super::TimedOut)), "{outcome:?}");
        assert!(!write.hand_over(), "a write given up on is not handed over");
        let fake = FakeCommitter::default();
        super::commit_batch(&fake, vec![write]).await;
        assert!(fake.calls.lock().map_err(|e| e.to_string())?.is_empty(), "nothing committed");
        Ok(())
    }

    /// Handed over, and no outcome by the batch deadline: a failure of unknown outcome, not the
    /// database being unavailable, since the cluster may have committed it.
    #[tokio::test]
    async fn test_handed_over_write_without_an_outcome() -> Result<(), String> {
        let fake = FakeCommitter::default();
        *fake.mode.lock().map_err(|e| e.to_string())? = "hang";
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let writer = tokio::spawn(super::write_batches(fake.clone(), receiver, Duration::from_millis(100)));
        let (write, answer, state) = queued("a");
        super::queue_write(&sender, write).map_err(|e| e.to_string())?;
        let outcome = super::wait_for_write(answer, &state, Duration::from_millis(10)).await;
        let Ok(Err(failure)) = outcome else { return Err(format!("{outcome:?}")) };
        assert_eq!(failure.kind, super::DbFailureKind::Unknown, "{failure}");
        assert!(failure.to_string().contains("may have been committed"), "{failure}");
        drop(sender);
        writer.await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Handing over and giving up change one value: whichever comes first, the other fails.
    #[test]
    fn test_handed_over_or_given_up_not_both() {
        let (write, _answer, state) = queued("a");
        assert!(write.hand_over());
        assert!(write.hand_over(), "still handed over, as when a batch is split");
        assert!(state.compare_exchange(super::WAITING, super::ABANDONED, Ordering::SeqCst, Ordering::SeqCst).is_err(), "too late to give up");
        let (write, answer, state) = queued("b");
        assert!(state.compare_exchange(super::WAITING, super::ABANDONED, Ordering::SeqCst, Ordering::SeqCst).is_ok());
        assert!(!write.hand_over(), "too late to hand over");
        drop(answer);
        let (write, answer, _state) = queued("c");
        drop(answer);
        assert!(!write.hand_over(), "nobody waits for it");
    }

    /// A full queue refuses at once, as the cluster being unavailable, and keeps nothing more.
    #[test]
    fn test_full_queue_refuses() -> Result<(), String> {
        let (sender, _receiver) = tokio::sync::mpsc::channel(2);
        let (batch, _waiting) = pending(&["a", "b", "c"]);
        let results: Vec<bool> = batch.into_iter().map(|write| super::queue_write(&sender, write).is_ok()).collect();
        assert_eq!(results, [true, true, false]);
        let (batch, _waiting) = pending(&["d"]);
        let Some(write) = batch.into_iter().next() else { return Err("no write".to_string()) };
        let Err(refused) = super::queue_write(&sender, write) else { return Err("taken".to_string()) };
        assert!(refused.unavailable() && refused.to_string().contains("waiting"), "{refused}");
        Ok(())
    }

    /// Without a usable leader for more than a few seconds, writes fail before they wait.
    #[test]
    fn test_writes_refused_without_a_leader() {
        let status = |without: Option<u64>| super::ClusterStatus { without_leader: without.map(Duration::from_secs), ..Default::default() };
        assert!(super::refused_without_leader(&status(None)).is_none());
        assert!(super::refused_without_leader(&status(Some(2))).is_none(), "an election takes a few seconds");
        assert!(super::refused_without_leader(&status(Some(6))).is_some_and(|failure| failure.unavailable()));
        // A follower that knows a leader; a leader that a quorum acknowledged lately, or not.
        assert!(super::has_usable_leader(false, true, Some(2), None));
        assert!(!super::has_usable_leader(false, true, None, None));
        assert!(!super::has_usable_leader(false, false, None, None), "a candidate");
        assert!(super::has_usable_leader(true, false, Some(1), Some(10)));
        assert!(!super::has_usable_leader(true, false, Some(1), Some(60_000)), "cut off from the others");
        assert!(!super::has_usable_leader(true, false, Some(1), None));
    }

    #[test]
    fn test_stopping_without_a_leader() {
        let status = |without: Option<u64>| super::ClusterStatus { without_leader: without.map(Duration::from_millis), ..Default::default() };
        assert!(!super::stops_without_leader(0, &status(Some(3_600_000))), "not set");
        assert!(!super::stops_without_leader(10_000, &status(None)));
        assert!(!super::stops_without_leader(10_000, &status(Some(9_000))));
        assert!(super::stops_without_leader(10_000, &status(Some(15_000))));
    }

    /// The test node is a cluster of one, its own leader.
    #[tokio::test]
    async fn test_cluster_status_of_the_test_node() -> Result<(), String> {
        let db = test_db();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !db.cluster_status().is_leader {
            if std::time::Instant::now() > deadline {
                return Err(format!("{:?}", db.cluster_status()));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let status = db.cluster_status();
        assert!(status.leader && status.without_leader.is_none(), "{status:?}");
        Ok(())
    }

    /// A write whose caller stopped waiting (its call timed out) is not written later.
    #[tokio::test]
    async fn test_abandoned_writes_are_not_committed() {
        let fake = FakeCommitter::default();
        let (batch, mut waiting) = pending(&["a", "gone", "c"]);
        drop(waiting.remove(1));
        super::commit_batch(&fake, batch).await;
        assert_eq!(answers(waiting).await, [Ok(1), Ok(1)]);
        assert_eq!(*fake.calls.lock().unwrap(), ["together 2"]);
        let (alone, waiting) = pending(&["gone"]);
        drop(waiting);
        super::commit_batch(&fake, alone).await;
        assert_eq!(fake.calls.lock().unwrap().len(), 1, "not even alone");
    }

    /// Counting this many generated rows keeps one read connection busy for a while.
    const SLOW_READ: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 30000000) SELECT count(*) AS n FROM c";

    #[tokio::test]
    async fn test_sql_execute_timeout() {
        let db = test_db();
        db.execute("CREATE TABLE IF NOT EXISTS timeout_busy (x INTEGER)").await.unwrap();
        let busy_db = db.clone();
        let busy = tokio::spawn(async move { busy_db.execute(BUSY_INSERT).await });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let engine = LuaAuthEngine::new(test_vms(), db, "return 'accept'").unwrap().with_query_timeout(Duration::from_millis(50));
        let lua = engine.new_state(&test_request()).unwrap();
        let started = std::time::Instant::now();
        let err = lua
            .load("return sql_execute('CREATE TABLE IF NOT EXISTS timeout_probe (x INTEGER)')")
            .eval_async::<mlua::Value>()
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("sql_execute timed out"), "unexpected error: {err}");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!busy.is_finished(), "the busy write finished too early to prove the timeout");
        busy.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_sql_query_timeout() {
        let engine = LuaAuthEngine::new(test_vms(), test_db(), "return 'accept'").unwrap().with_query_timeout(Duration::from_millis(50));
        let lua = engine.new_state(&test_request()).unwrap();
        lua.globals().set("slow_read", SLOW_READ).unwrap();
        let started = std::time::Instant::now();
        let err = lua.load("return sql_query(slow_read)").eval_async::<mlua::Value>().await.unwrap_err().to_string();
        assert!(err.contains("sql_query timed out"), "unexpected error: {err}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// The calls of one request share the limit: three slow reads end in about one limit, not
    /// three, and many quick ones are not held up by it.
    #[tokio::test]
    async fn test_calls_of_a_request_share_the_limit() -> Result<(), String> {
        let limit = Duration::from_millis(300);
        let script = format!("local slow = [[{SLOW_READ}]] for i = 1, 3 do pcall(sql_query, slow) end return 'reject'");
        let engine = LuaAuthEngine::new(test_vms(), test_db(), &script)?.with_query_timeout(limit);
        let started = std::time::Instant::now();
        let decision = engine.authenticate(test_request()).await;
        let took = started.elapsed();
        assert!(decision.database_failed, "the timeouts count as the database failing");
        assert!(took < limit * 2, "three calls took {took:?}, more than the limit they share");
        let quick = LuaAuthEngine::new(test_vms(), test_db(), "for i = 1, 100 do sql_query('SELECT 1 AS n') end return 'accept'")?.with_query_timeout(Duration::from_millis(1000));
        let decision = quick.authenticate(test_request()).await;
        assert!(decision.accepted() && !decision.database_failed);
        Ok(())
    }

    #[test]
    fn test_time_left_for_a_request() {
        let failures = DbFailures::default();
        let limit = Duration::from_millis(1000);
        assert_eq!(failures.left(limit), limit);
        failures.spend(Duration::from_millis(600));
        assert_eq!(failures.left(limit), Duration::from_millis(400));
        failures.spend(Duration::from_millis(600));
        assert_eq!(failures.left(limit), Duration::ZERO, "nothing left, not less than nothing");
    }

    #[tokio::test]
    async fn test_nothing_left_times_out_at_once() {
        let started = std::time::Instant::now();
        assert!(super::run_within(Duration::ZERO, std::future::pending::<()>()).await.is_err());
        assert!(super::run_within(Duration::from_secs(5), async { 7 }).await.is_ok_and(|n| n == 7));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn test_sql_query_rejects_writes() {
        let engine = engine_with_test_db().await;
        let lua = engine.new_state(&test_request()).unwrap();
        lua.load("sql_execute('CREATE TABLE ro_sessions (id TEXT)')").exec_async().await.unwrap();
        let err = lua
            .load("return sql_query(\"INSERT INTO ro_sessions VALUES ('s-1')\")")
            .eval_async::<mlua::Value>()
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("readonly"), "unexpected error: {err}");
        let count: i64 = lua.load("return #sql_query('SELECT id FROM ro_sessions')").eval_async().await.unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_listen_host_brackets_ipv6() {
        assert_eq!(listen_host("0.0.0.0".parse().unwrap()), "0.0.0.0");
        assert_eq!(listen_host("::".parse().unwrap()), "[::]");
        assert_eq!(listen_host("fe80::1".parse().unwrap()), "[fe80::1]");
    }

    #[tokio::test]
    async fn test_script_outcomes() {
        assert_eq!(decide("return 'accept'").await.outcome, Outcome::Accept);
        assert_eq!(decide("return 'ignore'").await.outcome, Outcome::Ignore);
        assert_eq!(decide("return 'reject'").await.outcome, Outcome::Reject);
        assert_eq!(decide("return nil").await.outcome, Outcome::Reject);
        assert_eq!(decide("error('boom')").await.outcome, Outcome::Reject);
        assert!(!decide("return 'ignore'").await.accepted());
    }

    #[tokio::test]
    async fn test_query_with_params_binds_values() {
        let db = test_db();
        db.execute("CREATE TABLE IF NOT EXISTS param_users (name TEXT)").await.unwrap();
        db.execute("INSERT INTO param_users VALUES ('alice')").await.unwrap();
        let sql = "SELECT name FROM param_users WHERE name = ?";
        assert!(db.query_with_params(sql, &["x' OR '1'='1"]).await.unwrap().is_empty());
        let rows = db.query_with_params(sql, &["alice"]).await.unwrap();
        assert_eq!(rows, vec![vec![("name".to_string(), DbValue::Text("alice".to_string()))]]);
    }

    /// The nodes of a new cluster in this process, each with its own directory
    /// and ports, on a runtime that lives as long as the process.
    fn start_cluster(count: u64) -> Vec<Db> {
        let free_port = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
        let nodes: Vec<_> = (1..=count).map(|id| crate::config::ClusterNode { id, raft: free_port(), api: free_port() }).collect();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            runtime.block_on(async move {
                let mut started = Vec::new();
                for id in 1..=count {
                    let settings = ClusterSettings {
                        node_id: id,
                        data_dir: tempfile::tempdir().unwrap().keep(),
                        secret_raft: "test-raft-secret-0123".to_string(),
                        secret_api: "test-api-secret-01234".to_string(),
                        listen: "127.0.0.1".parse().unwrap(),
                        nodes: nodes.clone(),
                        migrations_dir: None,
                        migrations_wait: Duration::from_secs(60),
                        snapshot_every: crate::config::DEFAULT_SNAPSHOT_EVERY,
                        read_connections: 4,
                        stop_without_leader: None,
                    };
                    started.push(Db::start(&settings).await.unwrap());
                }
                for db in &started {
                    db.wait_healthy(Duration::from_secs(60)).await.unwrap();
                }
                tx.send(started).unwrap();
                std::future::pending::<()>().await;
            });
        });
        rx.recv().unwrap()
    }

    fn migration_file(file_name: &str, sql: &str) -> MigrationFile {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(file_name);
        std::fs::write(&path, sql).unwrap();
        let (id, name) = parse_migration_name(file_name).unwrap();
        MigrationFile { id, file_name: file_name.to_string(), name: name.to_string(), file: rust_embed_utils::read_file_from_fs(&path).unwrap() }
    }

    // Calls take turns on MIGRATION_LOCK within a process, so these do not overlap in
    // hiqlite; tests/cluster_test.rs starts nodes as separate processes for that.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_nodes_apply_one_list_once() {
        let nodes = start_cluster(3);
        let apply = nodes.iter().map(|db| {
            let db = db.clone();
            tokio::spawn(async move { db.migrate_files(vec![migration_file("1_users.sql", "CREATE TABLE users (id INTEGER);")]).await.map_err(|e| e.to_string()) })
        });
        let applied: Vec<_> = join_all(apply).await;
        assert!(applied.iter().all(Result::is_ok), "{applied:?}");
        for db in &nodes {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let rows = db.query("SELECT id FROM _migrations").await.unwrap_or_default();
                if rows.len() == 1 && db.query("SELECT id FROM users").await.is_ok() {
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "the table did not reach a node: {rows:?}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    fn write_migrations(dir: &Path, files: &[(&str, &str)]) {
        for (name, sql) in files {
            std::fs::write(dir.join(name), sql).unwrap();
        }
    }

    async fn applied_rows(db: &Db) -> Vec<DbRow> {
        db.query("SELECT id, ts, hash FROM _migrations ORDER BY id").await.unwrap_or_default()
    }

    const WAIT: Duration = Duration::from_secs(10);
    const USERS: &str = "CREATE TABLE users (id INTEGER);";
    const REPLY: &str = "CREATE TABLE user_reply (id INTEGER);";

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_migrate_dir_applies_then_skips() {
        let db = start_cluster(1).remove(0);
        let dir = tempfile::tempdir().unwrap();
        write_migrations(dir.path(), &[("1_users.sql", USERS), ("2_reply.sql", REPLY)]);
        db.migrate_dir(dir.path(), WAIT).await.unwrap();
        db.query("SELECT id FROM users").await.unwrap();
        db.query("SELECT id FROM user_reply").await.unwrap();
        let after_first = applied_rows(&db).await;
        assert_eq!(after_first.len(), 2);

        db.migrate_dir(dir.path(), WAIT).await.unwrap();
        assert_eq!(applied_rows(&db).await, after_first, "applied files ran again");

        write_migrations(dir.path(), &[("3_acct.sql", "CREATE TABLE acct (id INTEGER);")]);
        db.migrate_dir(dir.path(), WAIT).await.unwrap();
        db.query("SELECT id FROM acct").await.unwrap();
        let after_third = applied_rows(&db).await;
        assert_eq!(after_third.len(), 3);
        assert_eq!(after_third[..2], after_first[..], "applied files ran again");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_migrate_dir_refuses_changed_applied_files() {
        let db = start_cluster(1).remove(0);
        let dir = tempfile::tempdir().unwrap();
        write_migrations(dir.path(), &[("1_users.sql", USERS)]);
        db.migrate_dir(dir.path(), WAIT).await.unwrap();

        write_migrations(dir.path(), &[("1_users.sql", "CREATE TABLE users (id INTEGER, name TEXT);")]);
        let error = db.migrate_dir(dir.path(), WAIT).await.unwrap_err();
        assert!(matches!(&error, MigrationError::Invalid(m) if m.contains("1_users.sql")), "{error:?}");

        write_migrations(dir.path(), &[("1_users.sql", USERS)]);
        std::fs::rename(dir.path().join("1_users.sql"), dir.path().join("1_accounts.sql")).unwrap();
        let error = db.migrate_dir(dir.path(), WAIT).await.unwrap_err();
        assert!(matches!(&error, MigrationError::Invalid(m) if m.contains("1_accounts.sql")), "{error:?}");

        // Neither reached hiqlite, which would have panicked in its writer.
        db.execute("CREATE TABLE still_writable (id INTEGER)").await.unwrap();
    }

    #[test]
    fn test_migration_names_and_numbering() {
        for (files, expected) in [
            (vec!["users.sql"], "users.sql"),
            (vec!["1_users.txt"], "1_users.txt"),
            (vec!["1_.sql"], "1_.sql"),
            (vec!["+1_users.sql"], "+1_users.sql"),
            (vec!["99999999999_users.sql"], "99999999999_users.sql"),
            (vec!["1_users.sql", "3_acct.sql"], "3_acct.sql"),
            (vec!["2_reply.sql"], "2_reply.sql"),
            (vec!["1_users.sql", "01_again.sql"], "_"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            for file in &files {
                std::fs::write(dir.path().join(file), "SELECT 1;").unwrap();
            }
            let Err(MigrationError::Invalid(message)) = read_migration_dir(dir.path()) else { panic!("{files:?} was accepted") };
            assert!(message.contains(expected), "{files:?}: {message}");
        }

        let missing = tempfile::tempdir().unwrap().path().join("gone");
        let Err(MigrationError::Invalid(message)) = read_migration_dir(&missing) else { panic!("a missing directory was accepted") };
        assert!(message.contains("gone"), "{message}");

        let dir = tempfile::tempdir().unwrap();
        for file in ["2_reply.sql", "10_ten.sql", "1_users.sql", "3_c.sql", "4_d.sql", "5_e.sql", "6_f.sql", "7_g.sql", "8_h.sql", "9_i.sql"] {
            std::fs::write(dir.path().join(file), "SELECT 1;").unwrap();
        }
        let ids: Vec<u32> = read_migration_dir(dir.path()).unwrap().iter().map(|f| f.id).collect();
        assert_eq!(ids, (1..=10).collect::<Vec<u32>>(), "numbers sort as numbers");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_migrate_dir_names_failing_file() {
        let db = start_cluster(1).remove(0);
        let dir = tempfile::tempdir().unwrap();
        write_migrations(dir.path(), &[("1_users.sql", "CREATE TABLE users (")]);
        let error = db.migrate_dir(dir.path(), WAIT).await.unwrap_err();
        assert!(matches!(&error, MigrationError::Failed(m) if m.contains("1_users.sql")), "{error:?}");
        assert_eq!(applied_rows(&db).await.len(), 0);

        write_migrations(dir.path(), &[("1_users.sql", USERS), ("2_reply.sql", "CREATE TABLE reply (")]);
        let error = db.migrate_dir(dir.path(), WAIT).await.unwrap_err();
        assert!(matches!(&error, MigrationError::Failed(m) if m.contains("2_reply.sql")), "{error:?}");
        assert_eq!(applied_rows(&db).await.len(), 1);
        db.query("SELECT id FROM users").await.unwrap();

        // Fixing the file is enough: nothing is left half done.
        write_migrations(dir.path(), &[("2_reply.sql", REPLY)]);
        db.migrate_dir(dir.path(), WAIT).await.unwrap();
        assert_eq!(applied_rows(&db).await.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_migrate_dir_with_fewer_files_than_applied() {
        let db = start_cluster(1).remove(0);
        let dir = tempfile::tempdir().unwrap();
        write_migrations(dir.path(), &[("1_users.sql", USERS), ("2_reply.sql", REPLY)]);
        db.migrate_dir(dir.path(), WAIT).await.unwrap();

        std::fs::remove_file(dir.path().join("2_reply.sql")).unwrap();
        db.migrate_dir(dir.path(), WAIT).await.unwrap();
        assert!(logged_errors().lock().unwrap().iter().any(|line| line.contains("has 1 files, but 2 are applied")));
        assert_eq!(applied_rows(&db).await.len(), 2);
        db.execute("CREATE TABLE still_writable (id INTEGER)").await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_migrate_dir_empty_directory() {
        let db = start_cluster(1).remove(0);
        let dir = tempfile::tempdir().unwrap();
        db.migrate_dir(dir.path(), WAIT).await.unwrap();
        assert_eq!(applied_rows(&db).await.len(), 0);
        db.execute("CREATE TABLE still_writable (id INTEGER)").await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_migrate_dir_without_majority() {
        let nodes = start_cluster(3);
        let dir = tempfile::tempdir().unwrap();
        write_migrations(dir.path(), &[("1_users.sql", USERS)]);
        nodes[1].shutdown().await.unwrap();
        nodes[2].shutdown().await.unwrap();
        let started = std::time::Instant::now();
        let error = tokio::time::timeout(Duration::from_secs(30), nodes[0].migrate_dir(dir.path(), Duration::from_secs(2))).await;
        let error = error.expect("migrate_dir did not return").unwrap_err();
        assert!(matches!(&error, MigrationError::Cluster(m) if !m.contains("1_users.sql")), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_check_local_migrations() {
        let db = start_cluster(1).remove(0);
        let dir = tempfile::tempdir().unwrap();
        // Nothing applied: an empty directory needs nothing, a file needs a leader.
        assert_eq!(db.check_local_migrations(&read_migrations(dir.path()).unwrap()).await, Ok(()));
        write_migrations(dir.path(), &[("1_users.sql", USERS)]);
        let error = db.check_local_migrations(&read_migrations(dir.path()).unwrap()).await.unwrap_err();
        assert!(matches!(&error, MigrationError::Cluster(m) if m.contains("1_users.sql")), "{error:?}");

        write_migrations(dir.path(), &[("2_reply.sql", REPLY)]);
        db.migrate_dir(dir.path(), WAIT).await.unwrap();
        assert_eq!(db.check_local_migrations(&read_migrations(dir.path()).unwrap()).await, Ok(()));

        // Fewer files than applied is fine, as with a leader.
        std::fs::remove_file(dir.path().join("2_reply.sql")).unwrap();
        assert_eq!(db.check_local_migrations(&read_migrations(dir.path()).unwrap()).await, Ok(()));

        write_migrations(dir.path(), &[("2_reply.sql", REPLY), ("3_acct.sql", "CREATE TABLE acct (id INTEGER);")]);
        let error = db.check_local_migrations(&read_migrations(dir.path()).unwrap()).await.unwrap_err();
        assert!(matches!(&error, MigrationError::Cluster(m) if m.contains("3_acct.sql")), "{error:?}");
        std::fs::remove_file(dir.path().join("3_acct.sql")).unwrap();

        write_migrations(dir.path(), &[("1_users.sql", "CREATE TABLE users (id INTEGER, name TEXT);")]);
        let error = db.check_local_migrations(&read_migrations(dir.path()).unwrap()).await.unwrap_err();
        assert!(matches!(&error, MigrationError::Invalid(m) if m.contains("1_users.sql")), "{error:?}");

        write_migrations(dir.path(), &[("1_users.sql", USERS)]);
        std::fs::rename(dir.path().join("1_users.sql"), dir.path().join("1_accounts.sql")).unwrap();
        let error = db.check_local_migrations(&read_migrations(dir.path()).unwrap()).await.unwrap_err();
        assert!(matches!(&error, MigrationError::Invalid(m) if m.contains("1_accounts.sql")), "{error:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_wait_for_replica_returns_when_reached() {
        let db = start_cluster(1).remove(0);
        let dir = tempfile::tempdir().unwrap();
        write_migrations(dir.path(), &[("1_users.sql", USERS)]);
        db.migrate_dir(dir.path(), WAIT).await.unwrap();
        db.wait_for_replica(1, Duration::from_secs(5)).await.unwrap();
        db.wait_for_replica(0, Duration::from_secs(5)).await.unwrap();

        let waiting = {
            let db = db.clone();
            tokio::spawn(async move { db.wait_for_replica(2, Duration::from_secs(30)).await })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!waiting.is_finished(), "returned before migration 2 was applied");
        write_migrations(dir.path(), &[("2_reply.sql", REPLY)]);
        db.migrate_dir(dir.path(), WAIT).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), waiting).await.expect("still waiting after migration 2").unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_wait_for_replica_times_out() {
        let db = start_cluster(1).remove(0);
        // No table yet, as on a replica that has not received the first migration.
        let started = std::time::Instant::now();
        let error = db.wait_for_replica(1, Duration::from_secs(1)).await.unwrap_err();
        assert!(matches!(&error, MigrationError::Cluster(m) if m.contains("catch up")), "{error:?}");
        assert!(started.elapsed() >= Duration::from_secs(1), "gave up after {:?}", started.elapsed());
        assert!(started.elapsed() < Duration::from_secs(5), "gave up after {:?}", started.elapsed());
    }

    #[test]
    fn test_migration_exit_codes() {
        assert_eq!(MigrationError::Invalid(String::new()).exit_code(), 2);
        assert_eq!(MigrationError::Failed(String::new()).exit_code(), 2);
        assert_eq!(MigrationError::Cluster(String::new()).exit_code(), 1);
        assert_eq!(MigrationError::Unavailable(String::new()).exit_code(), 1);
        let unavailable = MigrationError::Unavailable("no usable leader".to_string()).to_string();
        assert!(unavailable.contains("not ready") && !unavailable.contains("migration"), "{unavailable}");
    }

    /// A file's SQL failed: that stays the error when the record cannot be read either,
    /// so the exit status does not turn into the one for a cluster problem.
    #[test]
    fn test_failing_file_when_record_cannot_be_read() {
        let names = vec![(1, "1_users.sql".to_string())];
        let unreadable = Err(MigrationError::Cluster("no answer".to_string()));
        let error = sql_failure(&names, unreadable, &"syntax error");
        assert_eq!(error.exit_code(), 2);
        let MigrationError::Failed(message) = &error else { panic!("{error:?}") };
        assert!(message.contains("syntax error") && message.contains("could not be identified"), "{message}");
    }

    #[test]
    fn test_failed_migration_message() {
        let names = vec![(1, "1_users.sql".to_string()), (2, "2_reply.sql".to_string())];
        let message = |recorded| sql_failure(&names, recorded, &"syntax error").to_string();
        assert_eq!(message(Ok(1)), "migration file '2_reply.sql' could not be applied: syntax error");
        assert_eq!(message(Ok(0)), "migration file '1_users.sql' could not be applied: syntax error");

        let all_recorded = message(Ok(2));
        assert!(all_recorded.contains("syntax error") && !all_recorded.contains(".sql"), "{all_recorded}");

        let unreadable = message(Err(MigrationError::Cluster("no answer".to_string())));
        assert!(unreadable.contains("syntax error") && unreadable.contains("no answer") && unreadable.contains("could not be identified"), "{unreadable}");
    }

    async fn join_all<T>(tasks: impl Iterator<Item = tokio::task::JoinHandle<T>>) -> Vec<T> {
        let tasks: Vec<_> = tasks.collect();
        let mut results = Vec::with_capacity(tasks.len());
        for task in tasks {
            results.push(task.await.unwrap());
        }
        results
    }

    /// An engine for `script` on a pool of a single VM, so requests share it.
    fn single_vm_engine(script: &str) -> LuaAuthEngine {
        LuaAuthEngine::new(Arc::new(LuaVms::with_count(1).unwrap()), test_db(), script).unwrap()
    }

    const SIXTEEN_MIB: usize = 16 << 20;

    fn memory_limited_vms() -> Arc<LuaVms> {
        Arc::new(LuaVms::new(SIXTEEN_MIB).unwrap())
    }

    async fn outcome_within_memory(vms: &Arc<LuaVms>, script: &str) -> bool {
        let engine = LuaAuthEngine::new(vms.clone(), test_db(), script).unwrap().with_max_execution(Duration::from_secs(30));
        engine.authenticate(LuaRequest::new("radius", "authenticate", Vec::new())).await.accepted()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_memory_limit_stops_a_script_and_not_the_vm() {
        for script in [
            "local s = string.rep('x', 64 * 1024 * 1024) return 'accept'",
            "local kept = {} for i = 1, 10000000 do kept[i] = string.rep('x', 1024) .. i end return 'accept'",
            // The limit holds however often it is caught.
            "local kept = {} for round = 1, 3 do pcall(function() for i = 1, 10000000 do kept[#kept + 1] = string.rep('x', 1024) .. i end end) end local s = string.rep('x', 64 * 1024 * 1024) return 'accept'",
        ] {
            let vms = memory_limited_vms();
            assert!(!outcome_within_memory(&vms, script).await, "{script}");
            assert!(outcome_within_memory(&vms, "local s = string.rep('x', 1024 * 1024) return 'accept'").await, "the VM goes on after {script}");
        }
    }

    #[test]
    fn test_vm_being_collected_does_not_look_idle() {
        let vms = LuaVms::with_count(1).unwrap();
        vms.set_memory_limit(SIXTEEN_MIB).unwrap();
        let vm = &vms.vms[0];
        let _kept = vm.lua.create_string(vec![0u8; 9 << 20]).unwrap();
        let poll = vm.poll_lock.lock();
        std::thread::scope(|scope| {
            let guard = vms.pick();
            scope.spawn(move || drop(guard));
            std::thread::sleep(Duration::from_millis(50));
            assert_eq!(vm.active.load(Ordering::SeqCst), 1, "new requests would come here and wait");
            drop(poll);
        });
        assert_eq!(vm.active.load(Ordering::SeqCst), 0);
        assert_eq!(vm.collections.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_live_data_under_half_of_the_limit_is_left_alone() {
        let vms = LuaVms::with_count(1).unwrap();
        vms.set_memory_limit(SIXTEEN_MIB).unwrap();
        let vm = &vms.vms[0];
        let _six = vm.lua.create_string(vec![0u8; 6 << 20]).unwrap();
        drop(vms.pick());
        assert_eq!(vm.collections.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_live_data_is_not_collected_again_and_again() {
        let vms = LuaVms::with_count(1).unwrap();
        vms.set_memory_limit(SIXTEEN_MIB).unwrap();
        let vm = &vms.vms[0];
        let finished = || drop(vms.pick());
        let collections = || vm.collections.load(Ordering::SeqCst);
        finished();
        assert_eq!(collections(), 0, "nothing to speak of");
        let _nine = vm.lua.create_string(vec![0u8; 9 << 20]).unwrap();
        finished();
        assert_eq!(collections(), 1, "past half");
        for _ in 0..10 {
            finished();
        }
        assert_eq!(collections(), 1, "it has not grown");
        let _five = vm.lua.create_string(vec![0u8; 5 << 20]).unwrap();
        finished();
        assert_eq!(collections(), 2, "grown by more than a quarter of the limit");
        // Close to the limit it is worth it each time, or the next script may fail for garbage.
        finished();
        assert_eq!(collections(), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_memory_limit_leaves_garbage_alone() {
        let vms = memory_limited_vms();
        assert!(outcome_within_memory(&vms, "for i = 1, 2000 do local s = string.rep('x', 64 * 1024) .. i end return 'accept'").await);
        let vms = Arc::new(LuaVms::with_count(1).unwrap());
        vms.set_memory_limit(256 << 20).unwrap();
        assert!(outcome_within_memory(&vms, "for i = 1, 2000 do local s = string.rep('x', 1024 * 1024) .. i end return 'accept'").await, "a response of the most `max_response_bytes` allows by default, again and again");
    }

    #[tokio::test]
    async fn test_requests_on_one_vm_do_not_share_globals() {
        let engine = single_vm_engine("local first = seen == nil seen = true return first and 'accept' or 'reject'");
        assert!(engine.authenticate(test_request()).await.accepted());
        assert!(engine.authenticate(test_request()).await.accepted(), "the second request saw the first one's global");
    }

    #[tokio::test]
    async fn test_builtin_libraries_are_read_only() {
        let vms = Arc::new(LuaVms::with_count(1).unwrap());
        let tamper = LuaAuthEngine::new(vms.clone(), test_db(), "string.upper = nil return 'accept'").unwrap();
        assert!(!tamper.authenticate(test_request()).await.accepted());
        let check = LuaAuthEngine::new(vms, test_db(), "return string.upper('a') == 'A' and 'accept' or 'reject'").unwrap();
        assert!(check.authenticate(test_request()).await.accepted());
    }

    #[tokio::test]
    async fn test_request_functions_made_once_per_vm() {
        let engine = single_vm_engine("set_attr('Filter-Id', get_attr('User-Name')) log('made once') return 'accept'");
        for name in ["a", "b", "c", "d"] {
            let request = LuaRequest::new("radius", "authentication", vec![("User-Name".to_string(), AttrValue::Bytes(name.as_bytes().to_vec()))]);
            let decision = engine.authenticate(request).await;
            assert_eq!(decision.response_attributes, vec![("Filter-Id".to_string(), AttrValue::Bytes(name.as_bytes().to_vec()))]);
        }
        assert_eq!(engine.binders_made.load(Ordering::SeqCst), 1);
    }

    /// Two requests that wait in the middle of their scripts on the same VM: each
    /// reads its own attributes and adds to its own reply, even from a coroutine.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_requests_at_the_same_time_keep_their_own_data() {
        let engine = Arc::new(single_vm_engine("
            local name = get_attr('User-Name')
            sql_query('SELECT 1')
            set_attr('Filter-Id', name)
            coroutine.wrap(function() set_attr('Reply-Message', get_attr('User-Name')) end)()
            return 'accept'"));
        let runs = (0..20).map(|i| {
            let engine = engine.clone();
            tokio::spawn(async move {
                let name = format!("user-{i}").into_bytes();
                let decision = engine.authenticate(LuaRequest::new("radius", "authentication", vec![("User-Name".to_string(), AttrValue::Bytes(name.clone()))])).await;
                (name, decision)
            })
        });
        for run in futures::future::join_all(runs).await {
            let (name, decision) = run.unwrap();
            assert!(decision.accepted());
            assert_eq!(decision.response_attributes, vec![("Filter-Id".to_string(), AttrValue::Bytes(name.clone())), ("Reply-Message".to_string(), AttrValue::Bytes(name))]);
        }
        assert_eq!(engine.binders_made.load(Ordering::SeqCst), 1);
    }

    /// The request the functions get first is not counted among the script's arguments.
    #[tokio::test]
    async fn test_argument_errors_count_from_the_scripts_first() {
        let engine = engine_with_test_db().await;
        let state = engine.new_state(&test_request()).unwrap();
        for (code, position) in [
            ("return get_attr()", "bad argument #1"),
            ("return get_attrs({})", "bad argument #1"),
            ("return set_attr({}, 'x')", "bad argument #1"),
            ("return sql_query({})", "bad argument #1"),
            ("return sql_execute({})", "bad argument #1"),
            ("return send_coa('nas')", "bad argument #2"),
        ] {
            let error = state.load(code).eval_async::<Value>().await.unwrap_err().to_string();
            assert!(error.contains(position), "{code}: {error}");
        }
        let error = state.load("return get_attr()").eval_async::<Value>().await.unwrap_err().to_string();
        assert!(!error.contains("#2"), "{error}");
    }

    /// Another script on the VM has just overrun its limit and its poll has not
    /// finished: a request arriving now waits for it, and is not blamed for it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_another_scripts_overrun_is_not_blamed_on_a_new_request() {
        let engine = Arc::new(single_vm_engine("return 'accept'"));
        assert!(engine.authenticate(test_request()).await.accepted());
        let vm = &engine.vms.vms[0];
        let serial = vm.poll_lock.lock();
        *vm.running.lock().unwrap() = Some(Running { thread: 0, poll_start: Instant::now(), used: Duration::from_secs(10), limit: Duration::from_secs(1) });
        vm.over_limit.store(true, Ordering::Relaxed);
        let next = tokio::spawn({
            let engine = engine.clone();
            async move { engine.authenticate(test_request()).await }
        });
        // Not an await: the lock is what keeps the request waiting, and this thread is not a worker.
        std::thread::sleep(Duration::from_millis(100));
        vm.over_limit.store(false, Ordering::Relaxed);
        *vm.running.lock().unwrap() = None;
        drop(serial);
        assert!(next.await.unwrap().accepted());
    }

    #[tokio::test]
    async fn test_sandboxed_libraries() {
        for script in [
            "io.open('/etc/passwd') return 'accept'",
            "os.execute('true') return 'accept'",
            "require('socket') return 'accept'",
            "require('./mod') return 'accept'",
            "loadstring('return 1') return 'accept'",
            "getfenv(0) return 'accept'",
            "setfenv(0, {}) return 'accept'",
        ] {
            assert!(!single_vm_engine(script).authenticate(test_request()).await.accepted(), "{script}");
        }
    }

    fn logged_line_with(text: &str) -> Option<String> {
        logged_errors().lock().unwrap().iter().find(|line| line.contains(text)).cloned()
    }

    #[tokio::test]
    async fn test_script_error_names_script() {
        let engine = single_vm_engine("local x = nil\n\nreturn x.names_script").with_script_name("/etc/rradius/auth.lua");
        assert!(!engine.authenticate(test_request()).await.accepted());
        let line = logged_line_with("names_script").expect("nothing logged");
        assert!(line.contains("/etc/rradius/auth.lua:3:"), "{line}");
        assert!(!line.contains("lua_engine.rs"), "{line}");
    }

    #[tokio::test]
    async fn test_script_error_default_name() {
        let engine = single_vm_engine("error('default name')");
        assert!(!engine.authenticate(test_request()).await.accepted());
        let line = logged_line_with("default name").expect("nothing logged");
        assert!(line.contains("script:1: default name"), "{line}");
        assert!(!line.contains("lua_engine.rs"), "{line}");
    }

    #[tokio::test]
    async fn test_removed_globals_are_nil() {
        for name in ["require", "loadstring", "getfenv", "setfenv"] {
            let script = format!("return {name} == nil and 'accept' or 'reject'");
            assert!(single_vm_engine(&script).authenticate(test_request()).await.accepted(), "{name} is still there");
        }
    }

    #[tokio::test]
    async fn test_no_route_to_shared_globals() {
        let vms = Arc::new(LuaVms::with_count(1).unwrap());
        let first = "pcall(function() getfenv(0).leak = 1 end)
                     pcall(function() loadstring('leak = 1')() end)
                     pcall(coroutine.wrap(function() leak = 1 end))
                     return 'accept'";
        let first = LuaAuthEngine::new(vms.clone(), test_db(), first).unwrap();
        assert!(first.authenticate(test_request()).await.accepted());
        let second = LuaAuthEngine::new(vms, test_db(), "return leak == nil and 'accept' or 'reject'").unwrap();
        assert!(second.authenticate(test_request()).await.accepted(), "the second request saw the first one's global");
    }

    /// The line a script logged, identified by a word only that test uses.
    fn logged_line(word: &str) -> Option<String> {
        logged_errors().lock().unwrap().iter().find(|line| line.contains(word)).cloned()
    }

    #[tokio::test]
    async fn test_log_line_written() {
        let engine = single_vm_engine("log('log-written', 42) return 'accept'").with_script_name("/etc/rradius/auth.lua");
        let mut request = LuaRequest::new("radius", "accounting", vec![]);
        request.client = "nas1".to_string();
        assert!(engine.authenticate(request).await.accepted(), "logging does not change the decision");
        assert_eq!(logged_line("log-written").unwrap(), "script /etc/rradius/auth.lua (radius/accounting, client nas1): log-written 42");
    }

    #[tokio::test]
    async fn test_log_line_breaks() {
        single_vm_engine("log('log-breaks a\\nb\\r\\nc') return 'accept'").authenticate(test_request()).await;
        let line = logged_line("log-breaks").unwrap();
        assert!(line.ends_with("log-breaks a b  c"), "{line}");
        assert_eq!(line.lines().count(), 1);
    }

    #[tokio::test]
    async fn test_log_argument_types() {
        single_vm_engine("log('log-types', nil, true, 1.5) return 'accept'").authenticate(test_request()).await;
        assert!(logged_line("log-types").unwrap().ends_with("log-types nil true 1.5"));
    }

    #[tokio::test]
    async fn test_log_long_message() {
        let engine = single_vm_engine("log('log-long', string.rep('é', 3000)) return 'accept'");
        assert!(engine.authenticate(test_request()).await.accepted());
        let line = logged_line("log-long").unwrap();
        let message = line.split_once("): ").unwrap().1;
        assert!(message.len() <= 4096 && message.len() > 4000, "{} octets", message.len());
        assert!(message.ends_with('é'), "cut inside a character");
    }

    #[tokio::test]
    async fn test_log_in_handler_names_its_script() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("handler.lua");
        std::fs::write(&script, "log('log-handler') return 'accept'").unwrap();
        let settings = crate::config::ChainSettings { policy: crate::config::AuthPolicy::FirstMatch, handlers: vec![crate::config::AuthHandlerSettings::Lua { script: script.clone(), verifies_password: false }], username: None };
        let chain = crate::auth_chain::AuthChain::from_settings(&settings, &test_db(), &test_vms(), &crate::auth_chain::LuaLimits::default()).unwrap();
        let credentials = crate::auth_chain::Credentials { user: None, secret: crate::auth_chain::Secret::None };
        chain.authenticate(&credentials, &test_request()).await;
        assert!(logged_line("log-handler").unwrap().starts_with(&format!("script {} (", script.display())));
    }

    /// Whether `script` ends with `database_failed` set, with queries timing out quickly.
    async fn database_failed_within(script: &str, limit: Duration) -> bool {
        let engine = LuaAuthEngine::new(test_vms(), test_db(), script).unwrap().with_query_timeout(limit);
        engine.authenticate(accounting_request()).await.database_failed
    }

    async fn database_failed(script: &str) -> bool {
        database_failed_within(script, Duration::from_millis(1)).await
    }

    #[test]
    fn test_database_failure_kinds() {
        use hiqlite::Error as E;
        let kind = |error: E| DbFailure::from(error).kind;
        assert_eq!(kind(E::Connect("refused".to_string())), DbFailureKind::Unavailable);
        assert_eq!(kind(E::ConstraintViolation("UNIQUE".to_string())), DbFailureKind::Mistake);
        assert_eq!(kind(E::Sqlite("near \"SELEKT\": syntax error".into())), DbFailureKind::Mistake);
        assert_eq!(kind(E::Sqlite("database is locked".into())), DbFailureKind::Unknown, "may pass");
        assert_eq!(kind(E::Error("response channel closed".into())), DbFailureKind::Unavailable, "the node's channel");
    }

    #[tokio::test]
    async fn test_database_failure_is_noted() {
        // Slow for the 1 ms limit, yet short: abandoned queries go on using the shared database.
        let slow = "local slow = [[WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 200000) SELECT count(*) AS n FROM c]] ";
        assert!(database_failed(&format!("{slow} sql_query(slow) return 'accept'")).await, "a timeout");
        assert!(database_failed(&format!("{slow} pcall(sql_query, slow) return 'reject'")).await, "caught by the script");
        assert!(database_failed(&format!("{slow} pcall(sql_query, slow) return 'accept'")).await);
        assert!(!database_failed("return 'reject'").await);
        assert!(!database_failed("error('a bug')").await, "not the database");
        assert!(!database_failed_within("sql_query('SELEKT 1') return 'accept'", Duration::from_secs(5)).await, "the script's SQL");
        assert!(!database_failed_within("pcall(sql_execute, 'INSERT INTO no_such_table VALUES (1)') return 'reject'", Duration::from_secs(5)).await, "the script's SQL");
        assert!(!database_failed_within("sql_query('SELECT ?', {1, 2})", Duration::from_secs(5)).await, "the script's parameters");
    }

    #[tokio::test]
    async fn test_get_attrs() {
        let request = LuaRequest::new("radius", "authentication", vec![
            ("Class".to_string(), AttrValue::Bytes(b"a".to_vec())),
            ("NAS-Port".to_string(), AttrValue::Number(3.0)),
            ("Class".to_string(), AttrValue::Bytes(b"b".to_vec())),
        ]);
        let script = "local c, p, n = get_attrs('Class'), get_attrs('NAS-Port'), get_attrs('Missing')
            local ok = #c == 2 and c[1] == 'a' and c[2] == 'b' and get_attr('Class') == 'a'
                and #p == 1 and p[1] == 3 and type(n) == 'table' and #n == 0
            return ok and 'accept' or 'reject'";
        let engine = LuaAuthEngine::new(test_vms(), test_db(), script).unwrap();
        assert!(engine.authenticate(request).await.accepted());
    }

    #[tokio::test]
    async fn test_visible_globals() {
        let lua = single_vm_engine("return 'accept'").new_state(&test_request()).unwrap();
        let names: Vec<String> = lua
            .load("local names = {} for name in pairs(_G) do names[#names + 1] = name end table.sort(names) return names")
            .eval_async()
            .await
            .unwrap();
        let expected = "_G _VERSION assert bit32 buffer collectgarbage coroutine debug error gcinfo getmetatable integer ipairs math newproxy next os pairs pcall print rawequal rawget rawlen rawset select setmetatable string table tonumber tostring type typeof unpack utf8 vector xpcall";
        assert_eq!(names.join(" "), expected);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_parallel_requests_on_one_vm() {
        let engine = Arc::new(single_vm_engine("set_attr('Filter-Id', client) return client == 'b' and 'reject' or 'accept'"));
        let run = |client: &str| {
            let (engine, mut request) = (engine.clone(), test_request());
            request.client = client.to_string();
            tokio::spawn(async move { engine.authenticate(request).await })
        };
        let (a, b) = (run("a"), run("b"));
        let (a, b) = (a.await.unwrap(), b.await.unwrap());
        assert!(a.accepted() && !b.accepted());
        assert_eq!(a.response_attributes, vec![("Filter-Id".to_string(), AttrValue::Bytes(b"a".to_vec()))]);
    }

    #[tokio::test]
    async fn test_endless_loop_stopped() {
        let engine = single_vm_engine("while true do end").with_max_execution(Duration::from_millis(100));
        let started = std::time::Instant::now();
        let err = engine.run(&test_request(), Arc::default()).await.unwrap_err().to_string();
        assert!(started.elapsed() < Duration::from_secs(1), "took {:?}", started.elapsed());
        assert!(err.contains("execution limit of 100 ms"), "unexpected error: {err}");

        // The logged error names the limit too.
        let limit_lines = || logged_errors().lock().unwrap().iter().filter(|line| line.contains("execution limit of 100 ms")).count();
        let before = limit_lines();
        assert!(!engine.authenticate(test_request()).await.accepted());
        assert_eq!(limit_lines(), before + 1, "the execution limit error was not logged");
    }

    #[tokio::test]
    async fn test_database_wait_not_counted() {
        // A query of well over 300 ms, three times the limit (the spec's example).
        const QUERY: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 6000000) SELECT count(*) AS n FROM c";
        // Far past what the query takes on a slow machine: only the execution limit is under test.
        let engine = single_vm_engine(&format!("sql_query([[{QUERY}]]) return 'accept'"))
            .with_max_execution(Duration::from_millis(100))
            .with_query_timeout(Duration::from_secs(120));
        let started = std::time::Instant::now();
        let decision = engine.authenticate(test_request()).await;
        assert!(started.elapsed() > Duration::from_millis(300), "the query was too quick to prove anything: {:?}", started.elapsed());
        assert!(decision.accepted());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_requests_timed_separately() {
        let script = "local s = 0 for i = 1, 3000000 do s += i end return 'accept'";
        // Calibrate how long one run takes on this machine, then set the limit to
        // one and a half runs: each run alone is about two thirds of the limit,
        // the two together take longer than it (the spec's proportions).
        let vms = Arc::new(LuaVms::with_count(1).unwrap());
        let engine = LuaAuthEngine::new(vms.clone(), test_db(), script).unwrap().with_max_execution(Duration::from_secs(60));
        let started = std::time::Instant::now();
        assert!(engine.authenticate(test_request()).await.accepted());
        let one_run = started.elapsed();

        // Two runs at once on the one VM each stay under 1.5 runs, though together they take two.
        let engine = Arc::new(LuaAuthEngine::new(vms.clone(), test_db(), script).unwrap().with_max_execution(one_run * 3 / 2));
        let (a, b) = (engine.clone(), engine.clone());
        let (a, b) = tokio::join!(
            tokio::spawn(async move { a.authenticate(test_request()).await }),
            tokio::spawn(async move { b.authenticate(test_request()).await })
        );
        assert!(a.unwrap().accepted() && b.unwrap().accepted(), "one run took {:?}", one_run);
        // The limit is effective at this scale.
        let tight = LuaAuthEngine::new(vms, test_db(), script).unwrap().with_max_execution(one_run / 2);
        assert!(!tight.authenticate(test_request()).await.accepted());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_long_loop_does_not_block_its_vm() {
        let vms = Arc::new(LuaVms::with_count(1).unwrap());
        let busy = Arc::new(LuaAuthEngine::new(vms.clone(), test_db(), "while true do end").unwrap());
        let quick = LuaAuthEngine::new(vms, test_db(), "return 'accept'").unwrap();
        let busy_task = tokio::spawn(async move { busy.authenticate(test_request()).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let decision = tokio::time::timeout(Duration::from_secs(1), quick.authenticate(test_request())).await;
        assert!(decision.expect("blocked by the loop on the same VM").accepted());
        // The loop itself ends at the default 1000 ms limit.
        assert!(!busy_task.await.unwrap().accepted());
    }

    fn accounting_request() -> LuaRequest {
        LuaRequest::new("radius", "accounting", vec![])
    }

    /// One VM, so one accounting place, whose scripts can talk to the test without
    /// the database (which would give up the place): `gate_open()` says whether the
    /// test opened the gate, and `note(name)` records that a script got that far.
    pub(crate) struct GatedVm {
        pub(crate) vms: Arc<LuaVms>,
        gate: Arc<std::sync::atomic::AtomicBool>,
        notes: Arc<Mutex<Vec<String>>>,
    }

    impl GatedVm {
        pub(crate) fn new() -> Self {
            let vms = Arc::new(LuaVms::with_count(1).unwrap());
            let (gate, notes) = (Arc::new(std::sync::atomic::AtomicBool::new(false)), Arc::new(Mutex::new(Vec::new())));
            let lua = &vms.vms[0].lua;
            let open = gate.clone();
            lua.globals().set("gate_open", lua.create_function(move |_, ()| Ok(open.load(Ordering::SeqCst))).unwrap()).unwrap();
            let seen: Arc<Mutex<Vec<String>>> = notes.clone();
            lua.globals().set("note", lua.create_function(move |_, name: String| { seen.lock().unwrap().push(name); Ok(()) }).unwrap()).unwrap();
            Self { vms, gate, notes }
        }

        pub(crate) fn engine(&self, script: &str) -> Arc<LuaAuthEngine> {
            let engine = LuaAuthEngine::new(self.vms.clone(), test_db(), script).unwrap();
            Arc::new(engine.with_max_execution(Duration::from_secs(120)).with_query_timeout(Duration::from_secs(120)))
        }

        pub(crate) fn open_gate(&self) {
            self.gate.store(true, Ordering::SeqCst);
        }

        fn notes(&self) -> Vec<String> {
            self.notes.lock().unwrap().clone()
        }

        pub(crate) async fn wait_for_note(&self, name: &str) {
            tokio::time::timeout(Duration::from_secs(10), async {
                while !self.notes().iter().any(|n| n == name) {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("the script never reached note('{name}')"));
        }

        /// An accounting script that holds the place in a CPU loop until the gate opens.
        pub(crate) async fn hold_place(&self) -> tokio::task::JoinHandle<LuaDecision> {
            let holder = self.engine("note('holder') while not gate_open() do end return 'accept'");
            let task = tokio::spawn(async move { holder.authenticate(accounting_request()).await });
            self.wait_for_note("holder").await;
            task
        }
    }

    /// Long enough for a script that is not being held back to have finished.
    const NOT_HELD_BACK: Duration = Duration::from_millis(100);

    #[test]
    fn test_accounting_places() {
        for (vms, places) in [(1, 1), (2, 1), (3, 1), (4, 2), (14, 12)] {
            assert_eq!(accounting_places(vms), places, "with {vms} VMs");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_auth_served_while_accounting_is_full() {
        let vms = Arc::new(LuaVms::with_count(4).unwrap());
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (running, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        for vm in &vms.vms {
            let lua = &vm.lua;
            let open = gate.clone();
            lua.globals().set("gate_open", lua.create_function(move |_, ()| Ok(open.load(Ordering::SeqCst))).unwrap()).unwrap();
            let (now, most) = (running.clone(), peak.clone());
            lua.globals().set("enter", lua.create_function(move |_, ()| {
                most.fetch_max(now.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                Ok(())
            }).unwrap()).unwrap();
            let now = running.clone();
            lua.globals().set("leave", lua.create_function(move |_, ()| { now.fetch_sub(1, Ordering::SeqCst); Ok(()) }).unwrap()).unwrap();
        }
        let script = "enter() while not gate_open() do end leave() return 'accept'";
        let accounting = Arc::new(LuaAuthEngine::new(vms.clone(), test_db(), script).unwrap().with_max_execution(Duration::from_secs(120)));
        let tasks: Vec<_> = (0..8).map(|_| {
            let engine = accounting.clone();
            tokio::spawn(async move { engine.authenticate(accounting_request()).await })
        }).collect();
        tokio::time::timeout(Duration::from_secs(10), async {
            while running.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }).await.expect("no accounting script started");
        // Time for scripts beyond the limit to start, if anything lets them.
        tokio::time::sleep(NOT_HELD_BACK).await;

        let login = LuaAuthEngine::new(vms, test_db(), "return 'accept'").unwrap();
        // On a worker, like a real request. How quickly it runs is left to the release
        // measurements: scripts yield often enough that even a full set of workers
        // would serve it well within any bound that is safe to assert here.
        let login = tokio::spawn(async move { login.authenticate(test_request()).await });
        let decision = tokio::time::timeout(Duration::from_secs(5), login).await;
        assert!(decision.expect("authentication was not served while accounting scripts ran").unwrap().accepted());
        assert!(tasks.iter().all(|task| !task.is_finished()), "an accounting script finished before the login");

        gate.store(true, Ordering::SeqCst);
        for task in tasks {
            assert!(task.await.unwrap().accepted());
        }
        assert_eq!(peak.load(Ordering::SeqCst), 2, "accounting scripts running at once on 4 VMs");
    }

    #[tokio::test]
    async fn test_slow_loop_body_is_stopped() {
        let engine = single_vm_engine("while true do local s = string.rep('x', 1000000) end").with_max_execution(Duration::from_millis(100));
        let started = std::time::Instant::now();
        let err = engine.run(&test_request(), Arc::default()).await.unwrap_err().to_string();
        assert!(started.elapsed() < Duration::from_secs(1), "took {:?}", started.elapsed());
        assert!(err.contains("execution limit of 100 ms"), "unexpected error: {err}");
    }

    /// What comes of `script` with a limit of 100 ms, from a thread of its own, or `None` if it
    /// is still running after several seconds: it would hold whichever thread ran it.
    fn outcome_within_a_while(script: &'static str) -> Option<Result<Outcome, String>> {
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let result = runtime.block_on(async {
                let engine = single_vm_engine(script).with_max_execution(Duration::from_millis(100));
                engine.run(&test_request(), Arc::default()).await.map(|(outcome, _)| outcome).map_err(|e| e.to_string())
            });
            let _ = send.send(result);
        });
        receive.recv_timeout(Duration::from_secs(8)).ok()
    }

    #[test]
    fn test_limit_cannot_be_caught() {
        for script in [
            "while true do pcall(function() while true do end end) end return 'accept'",
            "repeat local ok = pcall(function() local i = 0 while i < 1e9 do i = i + 1 end end) until ok return 'accept'",
            "while true do xpcall(function() while true do end end, function(e) return e end) end return 'accept'",
            "while true do local co = coroutine.create(function() while true do end end) coroutine.resume(co) end return 'accept'",
            "pcall(function() pcall(function() while true do end end) while true do end end) return 'accept'",
            "pcall(function() while true do end end) return 'accept'",
            "local ok = pcall(function() while true do end end) if not ok then return 'accept' end return 'reject'",
        ] {
            let result = outcome_within_a_while(script).unwrap_or_else(|| panic!("still running: {script}"));
            let error = result.expect_err(script);
            assert!(error.contains("execution limit"), "{script}: {error}");
        }
    }

    #[tokio::test]
    async fn test_limit_passed_in_a_call_the_script_never_comes_back_to_a_check_from() {
        // Nothing checks during the long call, and the script may end before anything checks after it.
        let engine = single_vm_engine("local s = string.rep('x', 50000000) return 'accept'").with_max_execution(Duration::from_millis(1));
        let error = engine.run(&test_request(), Arc::default()).await.unwrap_err().to_string();
        assert!(error.contains("execution limit"), "{error}");
    }

    #[tokio::test]
    async fn test_errors_within_the_limit_can_be_caught() {
        let engine = single_vm_engine("local ok = pcall(function() error('mine') end) return ok and 'reject' or 'accept'").with_max_execution(Duration::from_secs(5));
        assert_eq!(engine.run(&test_request(), Arc::default()).await.unwrap().0, Outcome::Accept);
    }

    #[test]
    fn test_find_non_deterministic_however_written() {
        for sql in [
            "INSERT INTO t VALUES (random/**/())",
            "INSERT INTO t VALUES (RANDOM /* x */ ())",
            "INSERT INTO t VALUES (random --x\n ())",
            "INSERT INTO t VALUES (\"random\"())",
            "INSERT INTO t VALUES (`random`())",
            "INSERT INTO t VALUES ([random]())",
            "INSERT INTO t VALUES (\"RANDOM\" /* x */ ())",
            // Right after a keyword, with nothing between them.
            "INSERT INTO t SELECT\"random\"()",
            "SELECT`random`()",
            "SELECT[random]()",
            "SELECT 1 WHERE\"random\"()",
            "SELECT CASE WHEN 1 THEN\"random\"() END",
            // A byte order mark counts as a space before a token.
            "SELECT random \u{FEFF}()",
            "SELECT \"random\"\u{FEFF}()",
            "SELECT random/*x*/\u{FEFF}/*y*/()",
            "SELECT \u{FEFF}random()",
            // Not a table, whatever comes before the list.
            "SELECT \"into\" random()",
            "SELECT * FROM a JOIN b ON \"random\"()",
            "SELECT * FROM a JOIN b ON random()",
        ] {
            assert_eq!(find_non_deterministic(sql), Some("random"), "{sql}");
        }
        for (sql, keyword) in [
            ("INSERT INTO t VALUES (CURRENT_TIME/**/)", "current_time"),
            ("INSERT INTO t VALUES (/* x */current_date)", "current_date"),
            ("UPDATE t SET a = \"current_timestamp\"()", "current_timestamp"),
            ("UPDATE t SET a = `current_date`()", "current_date"),
            ("UPDATE t SET a = [current_time]()", "current_time"),
            ("UPDATE t SET a = current_time()", "current_time"),
        ] {
            assert_eq!(find_non_deterministic(sql), Some(keyword), "{sql}");
        }
        for sql in [
            "INSERT INTO t VALUES (1) /* random() */",
            "INSERT INTO t VALUES (1) -- random()",
            "UPDATE t SET \"date\" = 1, \"current_date\" = 2, `time` = 3, [now] = 4",
            "INSERT INTO t VALUES ('random()', '/* x', '-- y')",
            "SELECT a /* unterminated random()",
            "INSERT INTO t VALUES (ran/**/dom())",
            // Names of tables, with their columns.
            "INSERT INTO \"date\" (a) VALUES (1)",
            "INSERT INTO \"time\"(a) VALUES (1)",
            "CREATE TABLE \"date\"(a)",
            "CREATE TABLE [time] (id)",
            "CREATE TABLE IF NOT EXISTS \"date\" (a)",
            "CREATE TABLE t (a REFERENCES \"date\" (id))",
            "WITH \"date\"(a) AS (SELECT 1) SELECT a FROM \"date\"",
            "WITH RECURSIVE \"date\"(a) AS (SELECT 1) SELECT a FROM \"date\"",
            // Names that only hold such a word.
            "UPDATE t SET \"date (utc)\" = 1",
            "INSERT INTO \"my random\"(a) VALUES (1)",
            "SELECT \"random()\" FROM t",
            "SELECT random_thing() FROM t",
            "SELECT my_random() FROM t",
            // One name with a quote in it, and a string, which cannot be called.
            "SELECT \"x\"\"random\"() FROM t",
            "SELECT 'random'() FROM t",
            // A byte order mark inside a name makes it another name, for SQLite too.
            "SELECT random\u{FEFF}() FROM t",
        ] {
            assert_eq!(find_non_deterministic(sql), None, "{sql}");
        }
        // A string or a name holding a comment opener does not hide what follows it.
        assert_eq!(find_non_deterministic("INSERT INTO t VALUES ('/*', random())"), Some("random"));
        assert_eq!(find_non_deterministic("INSERT INTO t VALUES (\"/*\", random())"), Some("random"));
        assert_eq!(find_non_deterministic("INSERT INTO t VALUES ('it''s', random())"), Some("random"));
        assert_eq!(find_non_deterministic("INSERT INTO t VALUES (\"a\"\"b\", random())"), Some("random"));
    }

    #[tokio::test]
    async fn test_sql_execute_non_deterministic_however_written() {
        let engine = engine_with_test_db().await;
        let lua = engine.new_state(&test_request()).unwrap();
        lua.load("sql_execute('CREATE TABLE IF NOT EXISTS nondet_w (v)')").exec_async().await.unwrap();
        for (sql, item) in [
            ("INSERT INTO nondet_w VALUES (random/**/())", "random"),
            ("INSERT INTO nondet_w SELECT\"random\"()", "random"),
            ("INSERT INTO nondet_w VALUES (random \u{FEFF}())", "random"),
            ("INSERT INTO nondet_w VALUES (\"current_date\"())", "current_date"),
        ] {
            lua.globals().set("stmt", sql).unwrap();
            let error = lua.load("return sql_execute(stmt)").eval_async::<mlua::Value>().await.unwrap_err().to_string();
            assert!(error.contains(&format!("`{item}`")), "{sql}: {error}");
        }
        let count: i64 = lua.load("return #sql_query('SELECT v FROM nondet_w')").eval_async().await.unwrap();
        assert_eq!(count, 0, "not sent");
        // And what only looks like one goes through.
        for sql in ["CREATE TABLE IF NOT EXISTS \"date\" (\"time\", \"date (utc)\")", "INSERT INTO \"date\" (\"time\") VALUES (1) /* random() */"] {
            lua.globals().set("stmt", sql).unwrap();
            lua.load("return sql_execute(stmt)").eval_async::<mlua::Value>().await.unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_accounting_script_beyond_the_limit_waits() {
        let vm = GatedVm::new();
        let holder = vm.hold_place().await;
        let second = vm.engine("return 'accept'");
        let second = tokio::spawn(async move { second.authenticate(accounting_request()).await });
        tokio::time::sleep(NOT_HELD_BACK).await;
        assert!(!second.is_finished(), "the second script ran while the first held the only place");
        vm.open_gate();
        assert!(second.await.unwrap().accepted());
        assert!(holder.await.unwrap().accepted());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_accounting_limit_applies_to_both_protocols() {
        let vm = GatedVm::new();
        let holder = vm.hold_place().await;
        let second = vm.engine("return 'accept'");
        let second = tokio::spawn(async move { second.authenticate(LuaRequest::new("tacacs", "accounting", vec![])).await });
        tokio::time::sleep(NOT_HELD_BACK).await;
        assert!(!second.is_finished(), "a TACACS+ accounting script ran past the limit");
        vm.open_gate();
        assert!(second.await.unwrap().accepted());
        holder.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_accounting_scripts_wait_in_arrival_order() {
        let vm = GatedVm::new();
        let holder = vm.hold_place().await;
        let mut waiting = Vec::new();
        for name in ["first", "second", "third"] {
            let script = vm.engine(&format!("note('{name}') return 'accept'"));
            waiting.push(tokio::spawn(async move { script.authenticate(accounting_request()).await }));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        vm.open_gate();
        for task in waiting {
            assert!(task.await.unwrap().accepted());
        }
        holder.await.unwrap();
        assert_eq!(vm.notes(), ["holder", "first", "second", "third"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_other_phases_are_not_limited_by_accounting() {
        let vm = GatedVm::new();
        let holder = vm.hold_place().await;
        let quick = vm.engine("return 'accept'");
        for request in [test_request(), LuaRequest::new("tacacs", "authorization", vec![])] {
            let decision = tokio::time::timeout(Duration::from_secs(1), quick.authenticate(request)).await;
            assert!(decision.expect("waited for accounting").accepted());
        }
        assert!(!holder.is_finished());
        vm.open_gate();
        assert!(holder.await.unwrap().accepted());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_waiting_for_a_place_does_not_use_up_the_execution_limit() {
        let vm = GatedVm::new();
        let holder = vm.hold_place().await;
        let limited = Arc::new(LuaAuthEngine::new(vm.vms.clone(), test_db(), "return 'accept'").unwrap().with_max_execution(Duration::from_millis(100)));
        let waiting = tokio::spawn(async move { limited.authenticate(accounting_request()).await });
        tokio::time::sleep(Duration::from_millis(500)).await;
        vm.open_gate();
        assert!(waiting.await.unwrap().accepted());
        holder.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_waiting_for_a_place_does_not_use_up_the_query_timeout() {
        let vm = GatedVm::new();
        // Queries in a loop, so the place is given up and taken back again and again.
        let script = "note('querying') repeat sql_query('SELECT 1 AS n') until gate_open() return 'accept'";
        let querying = Arc::new(LuaAuthEngine::new(vm.vms.clone(), test_db(), script).unwrap().with_query_timeout(Duration::from_millis(200)).with_max_execution(Duration::from_secs(120)));
        let querying = tokio::spawn(async move { querying.authenticate(accounting_request()).await });
        vm.wait_for_note("querying").await;

        // This script takes the place during a query and keeps it, leaving the
        // first one waiting well past its query timeout.
        let holder = vm.hold_place().await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        vm.open_gate();
        assert!(querying.await.unwrap().accepted(), "the wait for a place counted as a query timeout");
        holder.await.unwrap();
    }

    #[tokio::test]
    async fn test_accounting_place_returned_after_a_failure() {
        let vm = GatedVm::new();
        let next = vm.engine("return 'accept'");
        let wait = Duration::from_secs(5);

        let failing = vm.engine("error('boom')");
        assert!(!failing.authenticate(accounting_request()).await.accepted());
        assert!(tokio::time::timeout(wait, next.authenticate(accounting_request())).await.expect("place not returned after an error").accepted());

        let endless = LuaAuthEngine::new(vm.vms.clone(), test_db(), "while true do end").unwrap().with_max_execution(Duration::from_millis(50));
        assert!(!endless.authenticate(accounting_request()).await.accepted());
        assert!(tokio::time::timeout(wait, next.authenticate(accounting_request())).await.expect("place not returned after the limit").accepted());

        let endless = vm.engine("while true do end");
        assert!(tokio::time::timeout(Duration::from_millis(50), endless.authenticate(accounting_request())).await.is_err());
        assert!(tokio::time::timeout(wait, next.authenticate(accounting_request())).await.expect("place not returned after cancellation").accepted());
    }

    #[tokio::test]
    async fn test_unaccepted_accounting_is_logged() {
        let lines_for = |client: &'static str| logged_errors().lock().unwrap().iter().filter(|line| line.contains(client)).count();
        let run = |script: &'static str, phase: &'static str, client: &'static str| async move {
            let mut request = LuaRequest::new("radius", phase, vec![]);
            request.client = client.to_string();
            LuaAuthEngine::new(test_vms(), test_db(), script).unwrap().authenticate(request).await
        };
        run("return 'reject'", "accounting", "log-acct-reject").await;
        assert_eq!(lines_for("log-acct-reject"), 1, "a rejected accounting request is not recorded, so it must be logged");
        run("return 'accept'", "accounting", "log-acct-accept").await;
        assert_eq!(lines_for("log-acct-accept"), 0);
        run("return 'reject'", "authentication", "log-auth-reject").await;
        assert_eq!(lines_for("log-auth-reject"), 0, "a rejected login is a normal result");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_failing_to_get_a_place_rejects() {
        // The server never closes the semaphore; this only forces the failure path.
        let vm = GatedVm::new();
        let querying = vm.engine("note('querying') repeat sql_query('SELECT 1 AS n') until gate_open() return 'accept'");
        let querying = tokio::spawn(async move { querying.authenticate(accounting_request()).await });
        vm.wait_for_note("querying").await;
        vm.vms.accounting_slots.close();
        assert!(!querying.await.unwrap().accepted(), "taking the place back after a query failed");

        let next = vm.engine("return 'accept'");
        assert!(!next.authenticate(accounting_request()).await.accepted(), "getting a first place failed");
    }

    /// A request abandoned while it waits to get its place back must not keep the place
    /// it is next in line for: its coroutine only goes away at the VM's next garbage
    /// collection, and with no script able to run on that VM, that never comes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_abandoned_wait_for_a_place_does_not_keep_it() {
        let vm = GatedVm::new();
        let waiting = vm.engine("note('querying') repeat sql_query('SELECT 1 AS n') until gate_open() return 'accept'");
        let abandoned = tokio::spawn(async move { waiting.authenticate(accounting_request()).await });
        vm.wait_for_note("querying").await;

        // Taken while the script queries, so the script has to queue to get it back.
        let holder = vm.vms.accounting_slots.clone().acquire_owned().await.unwrap();
        tokio::time::sleep(NOT_HELD_BACK).await;
        abandoned.abort();
        assert!(abandoned.await.unwrap_err().is_cancelled());
        drop(holder);

        let next = vm.engine("return 'accept'");
        let decision = tokio::time::timeout(Duration::from_secs(5), next.authenticate(accounting_request())).await;
        assert!(decision.expect("the abandoned request kept the place").accepted());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_accounting_place_given_up_during_query() {
        let vm = GatedVm::new();
        let waiting = vm.engine("note('querying') repeat sql_query('SELECT 1 AS n') until gate_open() return 'accept'");
        let first = tokio::spawn(async move { waiting.authenticate(accounting_request()).await });
        vm.wait_for_note("querying").await;

        let second = vm.engine("return 'accept'");
        let decision = tokio::time::timeout(Duration::from_secs(5), second.authenticate(accounting_request())).await;
        assert!(decision.expect("the script querying the database kept its place").accepted());
        assert!(!first.is_finished());

        vm.open_gate();
        assert!(first.await.unwrap().accepted());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_accounting_place_given_up_during_write() {
        let db = test_db();
        db.execute("CREATE TABLE slot_bulk (x INTEGER)").await.unwrap();
        let vm = GatedVm::new();
        // A write that takes far longer than the second script needs.
        let write = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 3000000) INSERT INTO slot_bulk SELECT x FROM c";
        let writing = LuaAuthEngine::new(vm.vms.clone(), db, format!("note('writing') sql_execute('{write}') return 'accept'")).unwrap().with_query_timeout(Duration::from_secs(120));
        let first = tokio::spawn(async move { writing.authenticate(accounting_request()).await });
        vm.wait_for_note("writing").await;

        let second = vm.engine("return 'accept'");
        let decision = tokio::time::timeout(Duration::from_secs(60), second.authenticate(accounting_request())).await;
        assert!(decision.unwrap().accepted());
        assert!(!first.is_finished(), "the second script waited for the write to finish");
        assert!(first.await.unwrap().accepted());
    }

    /// Scripts come back from the database and must hold a place again before
    /// running on, or one database call would free them from the limit for good.
    async fn assert_place_taken_back_after(call: &str) {
        let vm = GatedVm::new();
        let script = format!("{call} note('back') while not gate_open() do end return 'accept'");
        let first = vm.engine(&script);
        let first = tokio::spawn(async move { first.authenticate(accounting_request()).await });
        vm.wait_for_note("back").await;

        let second = vm.engine("return 'accept'");
        let second = tokio::spawn(async move { second.authenticate(accounting_request()).await });
        tokio::time::sleep(NOT_HELD_BACK).await;
        assert!(!second.is_finished(), "the script back from `{call}` was running without a place");
        vm.open_gate();
        assert!(first.await.unwrap().accepted());
        assert!(second.await.unwrap().accepted());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_accounting_place_taken_back_after_query() {
        assert_place_taken_back_after("sql_query('SELECT 1 AS n')").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_accounting_place_taken_back_after_write() {
        test_db().execute("CREATE TABLE IF NOT EXISTS slot_small (x INTEGER)").await.unwrap();
        assert_place_taken_back_after("sql_execute('INSERT INTO slot_small VALUES (1)')").await;
    }

    // Luau behaviour the shared-VM design relies on.

    /// An interrupt result that yields at most once per millisecond: yielding on
    /// every interrupt makes no progress, since resuming triggers it again.
    fn yield_every_ms(last: &std::sync::Mutex<std::time::Instant>) -> mlua::VmState {
        let mut last = last.lock().unwrap();
        if last.elapsed() >= Duration::from_millis(1) {
            *last = std::time::Instant::now();
            mlua::VmState::Yield
        } else {
            mlua::VmState::Continue
        }
    }

    #[tokio::test]
    async fn test_luau_environment_isolation() {
        let lua = Lua::new();
        lua.sandbox(true).unwrap();
        let env = lua.create_table().unwrap();
        let meta = lua.create_table().unwrap();
        meta.set("__index", lua.globals()).unwrap();
        env.set_metatable(Some(meta)).unwrap();
        let upper: String = lua
            .load("seen = true return string.upper('a')")
            .set_environment(env.clone())
            .eval_async()
            .await
            .unwrap();
        assert_eq!(upper, "A", "built-ins are reachable through __index");
        assert!(env.get::<bool>("seen").unwrap());
        assert!(lua.globals().get::<Option<bool>>("seen").unwrap().is_none());
        // Sandboxed built-in libraries are read-only.
        let err = lua.load("string.upper = nil").set_environment(env).exec_async().await.unwrap_err().to_string();
        assert!(err.contains("readonly"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn test_luau_interrupt_yield_returns_to_runtime() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let lua = Lua::new();
        let other_ran = Arc::new(AtomicBool::new(false));
        let seen_mid_loop = Arc::new(AtomicBool::new(false));
        let (flag, seen) = (other_ran.clone(), seen_mid_loop.clone());
        let last = std::sync::Mutex::new(std::time::Instant::now());
        lua.set_interrupt(move |_| {
            if flag.load(Ordering::SeqCst) {
                seen.store(true, Ordering::SeqCst);
            }
            Ok(yield_every_ms(&last))
        });
        let flag = other_ran.clone();
        // Current-thread runtime: this task only runs if the script yields to it.
        tokio::spawn(async move { flag.store(true, Ordering::SeqCst) });
        let sum: i64 = lua.load("local s = 0 for i = 1, 20000000 do s += i end return s").eval_async().await.unwrap();
        assert_eq!(sum, 20000000_i64 * 20000001 / 2);
        assert!(seen_mid_loop.load(Ordering::SeqCst), "the loop never yielded to the runtime");
    }

    #[tokio::test]
    async fn test_luau_interrupt_sees_each_coroutine() {
        use std::collections::HashSet;
        use std::sync::Mutex;
        let lua = Lua::new();
        let threads = Arc::new(Mutex::new(HashSet::new()));
        let seen = threads.clone();
        let last = std::sync::Mutex::new(std::time::Instant::now());
        lua.set_interrupt(move |lua| {
            seen.lock().unwrap().insert(lua.current_thread().to_pointer() as usize);
            Ok(yield_every_ms(&last))
        });
        let run = |n: i64| {
            let f = lua.load(format!("local s = 0 for i = 1, {n} do s += i end return s")).into_function().unwrap();
            lua.create_thread(f).unwrap().into_async::<i64>(()).unwrap()
        };
        let (a, b) = tokio::join!(run(2000000), run(2000001));
        assert_eq!((a.unwrap(), b.unwrap()), (2000000_i64 * 2000001 / 2, 2000001_i64 * 2000002 / 2));
        assert!(threads.lock().unwrap().len() >= 2, "interrupts did not tell the coroutines apart");
    }

    #[test]
    fn test_luau_compiler_reports_syntax_errors() {
        let compiler = mlua::chunk::Compiler::new();
        assert!(compiler.compile("return 'accept'").is_ok());
        assert!(matches!(compiler.compile("return 'accept"), Err(mlua::Error::SyntaxError { .. })));
    }

    /// A sender for tests that notes what it was asked to send and answers a NAK with 503.
    type NotedRequests = Arc<parking_lot::Mutex<Vec<(crate::dynamic_authorization::RequestType, String, Vec<(String, AttrValue)>)>>>;
    struct NotingSender(NotedRequests);

    struct Noted;

    impl crate::dynamic_authorization::PreparedRequest for Noted {
        fn send(self: Box<Self>) -> futures::future::BoxFuture<'static, crate::dynamic_authorization::Outcome> {
            Box::pin(async { crate::dynamic_authorization::Outcome::Nak(Some(503)) })
        }
    }

    impl DynamicAuthorization for NotingSender {
        fn prepare(&self, kind: crate::dynamic_authorization::RequestType, target: &str, attributes: &[(String, AttrValue)]) -> Result<Box<dyn crate::dynamic_authorization::PreparedRequest>, String> {
            if target == "nobody" {
                return Err("no client nobody".to_string());
            }
            self.0.lock().push((kind, target.to_string(), attributes.to_vec()));
            Ok(Box::new(Noted))
        }
    }

    /// `send_disconnect` and `send_coa` send through what the program gives: the target and the
    /// attributes go to it, and its answer comes back to the script; its errors are the script's.
    #[tokio::test]
    async fn test_dynamic_authorization_sends_through_the_sender() -> Result<(), String> {
        let noted = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let script = "local result, cause = send_coa('nas1', { ['User-Name'] = 'bob' })\n\
            local disconnected = send_disconnect('nas2', { ['User-Name'] = 'carol' })\n\
            local ok, err = pcall(send_coa, 'nobody', { ['User-Name'] = 'bob' })\n\
            return (result == 'nak' and cause == 503 and disconnected == 'nak' and not ok and string.find(tostring(err), 'send_coa: no client nobody', 1, true)) and 'accept' or 'reject'";
        let engine = LuaAuthEngine::new(test_vms(), test_db(), script).map_err(|e| e.to_string())?.with_dynamic_auth(Some(Arc::new(NotingSender(noted.clone()))));
        assert!(engine.authenticate(LuaRequest::new("radius", "authentication", vec![])).await.accepted());
        let noted = noted.lock().clone();
        assert_eq!(noted.len(), 2);
        assert_eq!((noted[0].0, noted[0].1.as_str(), noted[0].2.first().map(|(name, _)| name.as_str())), (crate::dynamic_authorization::RequestType::Coa, "nas1", Some("User-Name")));
        assert_eq!((noted[1].0, noted[1].1.as_str()), (crate::dynamic_authorization::RequestType::Disconnect, "nas2"));
        Ok(())
    }

    /// In a program without CoA, both functions raise an error that says what it needs; called
    /// without `pcall`, it fails the script as any error does, and nothing is sent.
    #[tokio::test]
    async fn test_dynamic_authorization_needs_the_enterprise_program() -> Result<(), String> {
        let engine = LuaAuthEngine::new(test_vms(), test_db(), "send_coa('nas1', { ['Session-Timeout'] = 60 }) return 'accept'").map_err(|e| e.to_string())?;
        assert!(!engine.authenticate(LuaRequest::new("radius", "authentication", vec![])).await.accepted(), "the script failed");
        let logged = crate::test_support::logged_errors().lock().map_err(|e| e.to_string())?.clone();
        assert!(logged.iter().any(|line| line.contains("send_coa: not supported by this program (CoA needs rradius-enterprise)")), "{logged:?}");
        for function in ["send_coa", "send_disconnect"] {
            let script = format!("local ok, err = pcall({function}, 'nas1', {{ ['User-Name'] = 'bob' }})\n\
                return (not ok and string.find(tostring(err), '{function}: not supported by this program (CoA needs rradius-enterprise)', 1, true)) and 'accept' or 'reject'");
            let engine = LuaAuthEngine::new(test_vms(), test_db(), &script).map_err(|e| e.to_string())?;
            assert!(engine.authenticate(LuaRequest::new("radius", "authentication", vec![])).await.accepted(), "{function}");
        }
        Ok(())
    }
}
