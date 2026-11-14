//! Per-node counters and the optional endpoint that serves them in the Prometheus text format.

use crate::radius::{accept_next, DiscardLog};
use std::fmt::Write as _;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

/// The longest request head accepted.
const MAX_HEAD_LEN: usize = 8 * 1024;
/// How long a client gets to send its request head.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Scrapes served at once.
const MAX_CONNECTIONS: usize = 16;

/// A value that only goes up, or with `dec` a gauge.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

/// What one accounting spool counts: `[radius.accounting_spool]` and `[proxy.accounting_spool]`
/// each have their own.
#[derive(Debug, Default)]
pub struct SpoolCounters {
    pub written: Counter,
    pub replayed: Counter,
    pub rejected: Counter,
    pub full: Counter,
    pub corrupt: Counter,
    /// A gauge: bytes waiting to be replayed.
    pub bytes: Counter,
}

impl Counter {
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec(&self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn add(&self, value: u64) {
        self.0.fetch_add(value, Ordering::Relaxed);
    }

    pub fn set(&self, value: u64) {
        self.0.store(value, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}


/// The responses sent to one client.
#[derive(Debug, Default)]
pub struct ClientResponses {
    pub access_accept: Counter,
    pub access_reject: Counter,
    pub access_challenge: Counter,
    pub accounting_response: Counter,
}

impl ClientResponses {
    /// Counts a response of this RADIUS code.
    pub fn count(&self, code: u8) {
        let counter = match code {
            crate::radius::ACCESS_ACCEPT => &self.access_accept,
            crate::radius::ACCESS_REJECT => &self.access_reject,
            crate::radius::ACCESS_CHALLENGE => &self.access_challenge,
            crate::radius::ACCOUNTING_RESPONSE => &self.accounting_response,
            _ => return,
        };
        counter.inc();
    }
}

/// Counts up a gauge until dropped.
pub struct Open<T = Metrics>(Arc<T>, fn(&T) -> &Counter);

impl<T> Open<T> {
    pub fn new(metrics: Arc<T>, gauge: fn(&T) -> &Counter) -> Self {
        gauge(&metrics).inc();
        Self(metrics, gauge)
    }
}

impl<T> Drop for Open<T> {
    fn drop(&mut self) {
        (self.1)(&self.0).dec();
    }
}

/// Writes one family in the Prometheus text format: its HELP and TYPE lines, then a line for each
/// of `samples`, its labels (such as `{result="accepted"}`, or empty) and its value.
pub fn write_family(out: &mut String, name: &str, kind: &str, help: &str, samples: &[(&str, u64)]) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
    for (labels, value) in samples {
        let _ = writeln!(out, "{name}{labels} {value}");
    }
}

/// The upper bounds, in seconds, of the response time buckets; `+Inf` comes after them. From
/// well under a millisecond, where local logins answer, to 10 s, past which a NAS has given up.
pub const RESPONSE_BUCKETS: [f64; 17] = [0.00005, 0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

/// The most one observation adds to a histogram's sum.
const MAX_OBSERVED_MICROS: u64 = 1 << 40;

/// How long requests took: a count for each bucket of `RESPONSE_BUCKETS` and one for `+Inf`, not
/// cumulative, their sum in microseconds, and their number.
#[derive(Debug, Default)]
pub struct Histogram {
    buckets: [AtomicU64; RESPONSE_BUCKETS.len() + 1],
    sum_micros: AtomicU64,
}

impl Histogram {
    pub fn observe(&self, elapsed: Duration) {
        let seconds = elapsed.as_secs_f64();
        let bucket = RESPONSE_BUCKETS.iter().position(|bound| seconds <= *bound).unwrap_or(RESPONSE_BUCKETS.len());
        if let Some(counter) = self.buckets.get(bucket) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        // One observation adds at most 2^40 µs (about 12.7 days), so that the sum, one plain
        // atomic add per request, cannot wrap before some 16 million such requests.
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX).min(MAX_OBSERVED_MICROS);
        self.sum_micros.fetch_add(micros, Ordering::Relaxed);
    }

    /// How many requests were observed: the buckets added up, so that recording one costs
    /// two atomic adds rather than three, and `_count` always equals the `+Inf` bucket.
    pub fn count(&self) -> u64 {
        self.buckets.iter().map(|counter| counter.load(Ordering::Relaxed)).sum()
    }

    /// The time of all of them together.
    pub fn sum(&self) -> Duration {
        Duration::from_micros(self.sum_micros.load(Ordering::Relaxed))
    }

    /// The samples of this histogram with `labels` (`kind="auth"`, without braces): the
    /// cumulative buckets, `_sum` in seconds and `_count`.
    pub fn render(&self, out: &mut String, name: &str, labels: &str) {
        let mut cumulative = 0u64;
        for (index, counter) in self.buckets.iter().enumerate() {
            cumulative += counter.load(Ordering::Relaxed);
            let bound = RESPONSE_BUCKETS.get(index).map_or_else(|| "+Inf".to_string(), |bound| bound.to_string());
            let _ = writeln!(out, "{name}_bucket{{{labels},le=\"{bound}\"}} {cumulative}");
        }
        let _ = writeln!(out, "{name}_sum{{{labels}}} {}", self.sum_micros.load(Ordering::Relaxed) as f64 / 1e6);
        let _ = writeln!(out, "{name}_count{{{labels}}} {cumulative}");
    }
}

/// Writes one histogram family: its HELP and TYPE lines, then each histogram with its labels.
pub fn write_histograms(out: &mut String, name: &str, help: &str, histograms: &[(&str, &Histogram)]) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} histogram");
    for (labels, histogram) in histograms {
        histogram.render(out, name, labels);
    }
}

/// How the conversations of one EAP method ended.
#[derive(Debug, Default)]
pub struct EapCounters {
    pub accept: Counter,
    pub reject: Counter,
    pub timeout: Counter,
    pub overload: Counter,
    pub replaced: Counter,
    pub evicted: Counter,
}

/// The counters of the EAP methods the extensions offer, in their order: those
/// `Metrics::for_extensions` was given, none for `Metrics::default()`.
#[derive(Debug, Default)]
pub struct ExtensionEapCounters(Vec<(crate::eap_method::EapMethodName, EapCounters)>);

impl ExtensionEapCounters {
    fn of(extensions: &crate::extension::Extensions) -> Self {
        Self(extensions.eap_method_names().into_iter().map(|names| (names, EapCounters::default())).collect())
    }
}

/// Everything reported, since the process started. Reloads do not touch these, but for which
/// clients are in `radius_clients`.
#[derive(Debug, Default)]
pub struct Metrics {
    /// By client name. Names come from the configuration, so this does not grow with traffic.
    radius_clients: parking_lot::RwLock<std::collections::BTreeMap<String, Arc<ClientResponses>>>,
    pub eap_open: Counter,
    pub eap_tls: EapCounters,
    pub eap_peap: EapCounters,
    pub eap_ttls: EapCounters,
    /// The methods of the extensions, after TTLS.
    pub eap_extra: ExtensionEapCounters,
    /// `[radius.accounting_spool]`.
    pub acct_spool: Arc<SpoolCounters>,
    pub script_http_ok: Counter,
    pub script_http_timeout: Counter,
    pub script_http_error: Counter,
    pub script_http_denied: Counter,
    pub script_http_too_large: Counter,
    pub script_http_busy: Counter,
    pub radius_packets_auth: Counter,
    pub radius_packets_acct: Counter,
    pub radius_access_accept: Counter,
    pub radius_access_reject: Counter,
    pub radius_access_challenge: Counter,
    pub radius_accounting_response: Counter,
    pub radius_status_server: Counter,
    pub radius_duplicates: Counter,
    pub radius_discarded_overload: Counter,
    pub radius_discarded_unknown_client: Counter,
    pub radius_discarded_invalid: Counter,
    /// Access-Requests left unanswered because the cluster database could not be used.
    pub radius_discarded_database: Counter,
    /// Requests left unanswered under `[cluster] stop_without_leader_ms`.
    pub radius_discarded_no_leader: Counter,
    pub tacacs_accepted: Counter,
    pub tacacs_refused: Counter,
    pub tacacs_open: Counter,
    pub tacacs_authen: Counter,
    pub tacacs_author: Counter,
    pub tacacs_acct: Counter,
    pub tacacs_malformed: Counter,
    pub radius_response_auth: Histogram,
    pub radius_response_acct: Histogram,
    pub tacacs_response_authen: Histogram,
    pub tacacs_response_author: Histogram,
    pub tacacs_response_acct: Histogram,
    pub reload_ok: Counter,
    pub reload_error: Counter,
    pub reload_last_success: Counter,
    /// The places of RADIUS requests, once a RADIUS server serves.
    radius_slots: std::sync::OnceLock<RadiusSlots>,
    /// What `with_version` was given: the program's version and when it started.
    started: std::sync::OnceLock<Started>,
}

/// The places RADIUS requests take while handled, over UDP or RadSec, and how many there are.
#[derive(Debug)]
struct RadiusSlots {
    auth: Arc<Semaphore>,
    auth_capacity: usize,
    acct: Arc<Semaphore>,
    acct_capacity: usize,
}

/// The program's version and the Unix time it started.
#[derive(Debug)]
struct Started {
    version: &'static str,
    at: u64,
}

/// A value for a label, as the Prometheus text format has it.
pub fn label_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

impl Metrics {
    /// The responses sent to a client. Only the configuration adds one: a request still under way
    /// when a reload removes its client must not bring it back.
    pub fn radius_client(&self, name: &str) -> Option<Arc<ClientResponses>> {
        self.radius_clients.read().get(name).cloned()
    }

    /// Makes these the clients counted: new ones start at 0, and others are forgotten.
    pub fn set_radius_clients<'a>(&self, names: impl IntoIterator<Item = &'a str>) {
        let names: std::collections::BTreeSet<&str> = names.into_iter().collect();
        let mut clients = self.radius_clients.write();
        clients.retain(|name, _| names.contains(name.as_str()));
        for name in names {
            clients.entry(name.to_string()).or_default();
        }
    }

    /// Metrics that also count the EAP methods of `extensions`.
    pub fn for_extensions(extensions: &crate::extension::Extensions) -> Self {
        Self { eap_extra: ExtensionEapCounters::of(extensions), ..Self::default() }
    }

    /// Notes the program's version, and now as the time it started.
    pub fn with_version(self, version: &'static str) -> Self {
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs());
        let _ = self.started.set(Started { version, at });
        self
    }

    /// Reports the RADIUS requests being handled from now on: those holding a place of `auth` or
    /// `acct`, out of their capacities. Only the first call counts; the places last as long as
    /// the process.
    pub fn watch_radius_slots(&self, auth: Arc<Semaphore>, auth_capacity: usize, acct: Arc<Semaphore>, acct_capacity: usize) {
        let _ = self.radius_slots.set(RadiusSlots { auth, auth_capacity, acct, acct_capacity });
    }

    /// The counters of an EAP method, by its EAP type.
    pub fn eap(&self, method: u8) -> &EapCounters {
        match method {
            crate::eap::TYPE_PEAP => &self.eap_peap,
            crate::eap::TYPE_TTLS => &self.eap_ttls,
            _ => self.eap_extra.0.iter().find(|(names, _)| names.kind == method).map_or(&self.eap_tls, |(_, counters)| counters),
        }
    }

    /// `render_with` the extensions of this program.
    pub fn render(&self) -> String {
        self.render_with(crate::extension::installed())
    }

    /// The Prometheus text format: the server's metrics, then those of `extensions`. Every
    /// label combination is present from the start, so that a query does not see a series appear.
    pub fn render_with(&self, extensions: &crate::extension::Extensions) -> String {
        let mut out = String::new();
        write_family(&mut out, "rradius_radius_packets_total", "counter", "RADIUS requests processed, after the checks and duplicate detection.", &[
            ("{kind=\"auth\"}", self.radius_packets_auth.get()),
            ("{kind=\"acct\"}", self.radius_packets_acct.get()),
        ]);
        write_family(&mut out, "rradius_radius_responses_total", "counter", "RADIUS responses sent, excluding replays and Status-Server.", &[
            ("{code=\"access_accept\"}", self.radius_access_accept.get()),
            ("{code=\"access_reject\"}", self.radius_access_reject.get()),
            ("{code=\"access_challenge\"}", self.radius_access_challenge.get()),
            ("{code=\"accounting_response\"}", self.radius_accounting_response.get()),
        ]);
        let client_samples: Vec<(String, u64)> = self
            .radius_clients
            .read()
            .iter()
            .flat_map(|(name, counters)| {
                let name = label_value(name);
                [
                    ("access_accept", counters.access_accept.get()),
                    ("access_reject", counters.access_reject.get()),
                    ("access_challenge", counters.access_challenge.get()),
                    ("accounting_response", counters.accounting_response.get()),
                ]
                .map(|(code, value)| (format!("{{client=\"{name}\",code=\"{code}\"}}"), value))
            })
            .collect();
        let client_samples: Vec<(&str, u64)> = client_samples.iter().map(|(labels, value)| (labels.as_str(), *value)).collect();
        write_family(&mut out, "rradius_radius_client_responses_total", "counter", "RADIUS responses sent to each client, excluding replays and Status-Server.", &client_samples);
        write_family(&mut out, "rradius_radius_status_server_total", "counter", "Status-Server packets answered.", &[("", self.radius_status_server.get())]);
        write_family(&mut out, "rradius_radius_duplicates_total", "counter", "Retransmissions answered from the duplicate cache or discarded while in progress.", &[("", self.radius_duplicates.get())]);
        write_family(&mut out, "rradius_radius_discarded_total", "counter", "RADIUS packets discarded before processing.", &[
            ("{reason=\"overload\"}", self.radius_discarded_overload.get()),
            ("{reason=\"unknown_client\"}", self.radius_discarded_unknown_client.get()),
            ("{reason=\"invalid\"}", self.radius_discarded_invalid.get()),
            ("{reason=\"database\"}", self.radius_discarded_database.get()),
            ("{reason=\"no_leader\"}", self.radius_discarded_no_leader.get()),
        ]);
        write_histograms(&mut out, "rradius_radius_response_seconds", "How long RADIUS requests took to answer, over UDP or RadSec, from after the duplicate check to the response; requests given no response are not counted.", &[
            ("kind=\"auth\"", &self.radius_response_auth),
            ("kind=\"acct\"", &self.radius_response_acct),
        ]);
        if let Some(slots) = self.radius_slots.get() {
            let busy = |places: &Semaphore, capacity: usize| capacity.saturating_sub(places.available_permits()) as u64;
            write_family(&mut out, "rradius_radius_requests_in_progress", "gauge", "RADIUS requests being handled now, over UDP or RadSec.", &[
                ("{kind=\"auth\"}", busy(&slots.auth, slots.auth_capacity)),
                ("{kind=\"acct\"}", busy(&slots.acct, slots.acct_capacity)),
            ]);
            write_family(&mut out, "rradius_radius_requests_limit", "gauge", "RADIUS requests that can be handled at once: max_auth_inflight and max_acct_inflight.", &[
                ("{kind=\"auth\"}", slots.auth_capacity as u64),
                ("{kind=\"acct\"}", slots.acct_capacity as u64),
            ]);
        }
        write_family(&mut out, "rradius_tacacs_connections_total", "counter", "TACACS+ connections by what became of them.", &[
            ("{result=\"accepted\"}", self.tacacs_accepted.get()),
            ("{result=\"refused\"}", self.tacacs_refused.get()),
        ]);
        write_family(&mut out, "rradius_tacacs_connections_open", "gauge", "TACACS+ connections open now.", &[("", self.tacacs_open.get())]);
        write_family(&mut out, "rradius_tacacs_requests_total", "counter", "TACACS+ requests by type.", &[
            ("{type=\"authen\"}", self.tacacs_authen.get()),
            ("{type=\"author\"}", self.tacacs_author.get()),
            ("{type=\"acct\"}", self.tacacs_acct.get()),
        ]);
        write_family(&mut out, "rradius_tacacs_malformed_total", "counter", "TACACS+ requests whose decrypted body makes no sense, as with the wrong secret.", &[("", self.tacacs_malformed.get())]);
        write_histograms(&mut out, "rradius_tacacs_response_seconds", "How long TACACS+ requests took to answer, from reading the packet to writing the reply; each authentication packet on its own.", &[
            ("type=\"authen\"", &self.tacacs_response_authen),
            ("type=\"author\"", &self.tacacs_response_author),
            ("type=\"acct\"", &self.tacacs_response_acct),
        ]);
        write_family(&mut out, "rradius_log_dropped_lines_total", "counter", "Log lines lost because output could not keep up or writing them failed.", &[("", crate::logging::dropped_lines())]);
        let (batched, split) = crate::lua_engine::write_batch_counts();
        let cluster = crate::lua_engine::cluster_status();
        write_family(&mut out, "rradius_cluster_leader", "gauge", "1 while this node has a leader to write through, else 0.", &[("", u64::from(cluster.leader))]);
        write_family(&mut out, "rradius_cluster_is_leader", "gauge", "1 while this node is the cluster's leader.", &[("", u64::from(cluster.is_leader))]);
        write_family(&mut out, "rradius_cluster_applied_index", "gauge", "The last log entry applied to this node's replica.", &[("", cluster.applied)]);
        write_family(&mut out, "rradius_cluster_seconds_without_leader", "gauge", "How long this node has had no leader to write through; 0 while it has one.", &[("", cluster.without_leader.map_or(0, |lasted| lasted.as_secs()))]);
        write_family(&mut out, "rradius_cluster_caught_up", "gauge", "1 once this node's replica has caught up with the cluster after it started; 0 while it serves from a replica that may be behind.", &[("", u64::from(cluster.caught_up))]);
        write_family(&mut out, "rradius_db_batched_writes_total", "counter", "Script writes committed in one transaction together with others.", &[("", batched)]);
        write_family(&mut out, "rradius_db_split_batches_total", "counter", "Transactions of script writes that failed and were split to find the statement that failed.", &[("", split)]);
        write_family(&mut out, "rradius_script_http_requests_total", "counter", "http_request calls by result.", &[
            ("{result=\"ok\"}", self.script_http_ok.get()),
            ("{result=\"timeout\"}", self.script_http_timeout.get()),
            ("{result=\"error\"}", self.script_http_error.get()),
            ("{result=\"denied\"}", self.script_http_denied.get()),
            ("{result=\"too_large\"}", self.script_http_too_large.get()),
            ("{result=\"busy\"}", self.script_http_busy.get()),
        ]);
        let netlogon = crate::netlogon::handler::counters();
        write_family(&mut out, "rradius_netlogon_logons_total", "counter", "MS-CHAPv2 logons the netlogon handlers asked domain controllers about, by result.", &[
            ("{result=\"accept\"}", netlogon.accept.get()),
            ("{result=\"reject\"}", netlogon.reject.get()),
            ("{result=\"no_such_user\"}", netlogon.no_such_user.get()),
            ("{result=\"unavailable\"}", netlogon.unavailable.get()),
        ]);
        write_family(&mut out, "rradius_netlogon_channel_setups_total", "counter", "Netlogon secure channels set up with domain controllers, by result.", &[
            ("{result=\"ok\"}", netlogon.setups_ok.get()),
            ("{result=\"failed\"}", netlogon.setups_failed.get()),
        ]);
        write_family(&mut out, "rradius_netlogon_open_connections", "gauge", "Sealed Netlogon connections open to domain controllers.", &[("", netlogon.open.get())]);
        write_family(&mut out, "rradius_eap_sessions_open", "gauge", "EAP conversations under way.", &[("", self.eap_open.get())]);
        let mut eap_sessions: Vec<(String, u64)> = vec![
            ("{method=\"tls\",result=\"accept\"}".to_string(), self.eap_tls.accept.get()),
            ("{method=\"tls\",result=\"reject\"}".to_string(), self.eap_tls.reject.get()),
            ("{method=\"tls\",result=\"timeout\"}".to_string(), self.eap_tls.timeout.get()),
            ("{method=\"tls\",result=\"overload\"}".to_string(), self.eap_tls.overload.get()),
            ("{method=\"tls\",result=\"replaced\"}".to_string(), self.eap_tls.replaced.get()),
            ("{method=\"tls\",result=\"evicted\"}".to_string(), self.eap_tls.evicted.get()),
            ("{method=\"peap\",result=\"accept\"}".to_string(), self.eap_peap.accept.get()),
            ("{method=\"peap\",result=\"reject\"}".to_string(), self.eap_peap.reject.get()),
            ("{method=\"peap\",result=\"timeout\"}".to_string(), self.eap_peap.timeout.get()),
            ("{method=\"peap\",result=\"overload\"}".to_string(), self.eap_peap.overload.get()),
            ("{method=\"peap\",result=\"replaced\"}".to_string(), self.eap_peap.replaced.get()),
            ("{method=\"peap\",result=\"evicted\"}".to_string(), self.eap_peap.evicted.get()),
            ("{method=\"ttls\",result=\"accept\"}".to_string(), self.eap_ttls.accept.get()),
            ("{method=\"ttls\",result=\"reject\"}".to_string(), self.eap_ttls.reject.get()),
            ("{method=\"ttls\",result=\"timeout\"}".to_string(), self.eap_ttls.timeout.get()),
            ("{method=\"ttls\",result=\"overload\"}".to_string(), self.eap_ttls.overload.get()),
            ("{method=\"ttls\",result=\"replaced\"}".to_string(), self.eap_ttls.replaced.get()),
            ("{method=\"ttls\",result=\"evicted\"}".to_string(), self.eap_ttls.evicted.get()),
        ];
        for (names, counters) in &self.eap_extra.0 {
            let method = label_value(names.name);
            for (result, counter) in [("accept", &counters.accept), ("reject", &counters.reject), ("timeout", &counters.timeout), ("overload", &counters.overload), ("replaced", &counters.replaced), ("evicted", &counters.evicted)] {
                eap_sessions.push((format!("{{method=\"{method}\",result=\"{result}\"}}"), counter.get()));
            }
        }
        let eap_sessions: Vec<(&str, u64)> = eap_sessions.iter().map(|(labels, value)| (labels.as_str(), *value)).collect();
        write_family(&mut out, "rradius_eap_sessions_total", "counter", "EAP conversations by how they ended.", &eap_sessions);
        write_family(&mut out, "rradius_acct_spool_written_total", "counter", "Accounting requests written to the spool.", &[("", self.acct_spool.written.get())]);
        write_family(&mut out, "rradius_acct_spool_replayed_total", "counter", "Spooled accounting requests the script accepted.", &[("", self.acct_spool.replayed.get())]);
        write_family(&mut out, "rradius_acct_spool_rejected_total", "counter", "Spooled accounting requests moved to rejected.log.", &[("", self.acct_spool.rejected.get())]);
        write_family(&mut out, "rradius_acct_spool_full_total", "counter", "Accounting requests not spooled because the spool is full or the disk failed.", &[("", self.acct_spool.full.get())]);
        write_family(&mut out, "rradius_acct_spool_corrupt_total", "counter", "Damaged spool files set aside.", &[("", self.acct_spool.corrupt.get())]);
        write_family(&mut out, "rradius_acct_spool_bytes", "gauge", "Bytes of accounting requests waiting to be replayed.", &[("", self.acct_spool.bytes.get())]);
        write_family(&mut out, "rradius_reload_total", "counter", "Reloads by result.", &[
            ("{result=\"ok\"}", self.reload_ok.get()),
            ("{result=\"error\"}", self.reload_error.get()),
        ]);
        write_family(&mut out, "rradius_reload_last_success_timestamp_seconds", "gauge", "When the last successful reload happened; 0 before the first.", &[("", self.reload_last_success.get())]);
        let (version, started) = self.started.get().map_or(("unknown", 0), |started| (started.version, started.at));
        write_family(&mut out, "rradius_build_info", "gauge", "Always 1; the version of the program in its label.", &[(&format!("{{version=\"{}\"}}", label_value(version)), 1)]);
        write_family(&mut out, "rradius_start_time_seconds", "gauge", "The Unix time the program started; 0 when unknown.", &[("", started)]);
        extensions.render_metrics(&mut out);
        out
    }
}

/// Serves `GET /metrics` on `addr` until the process ends.
pub async fn serve(addr: SocketAddr, metrics: Arc<Metrics>) -> io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("Metrics listening on {addr}");
    serve_listener(listener, metrics, READ_TIMEOUT).await
}

async fn serve_listener(listener: TcpListener, metrics: Arc<Metrics>, read_timeout: Duration) -> io::Result<()> {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut failures = DiscardLog::default();
    loop {
        let (socket, _) = accept_next(&listener, "Metrics", &mut failures).await;
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            continue;
        };
        let metrics = metrics.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let _ = answer(socket, &metrics, read_timeout).await;
        });
    }
}

/// Set once the server begins to stop on SIGTERM or SIGINT; never cleared, as the process ends.
static STOPPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Makes `/readyz` answer 503 `stopping` from now on, so that a load balancer sends requests to
/// the other nodes while this one still answers them.
pub fn set_stopping() {
    STOPPING.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// What `/readyz` answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Readiness {
    Ready,
    /// Authentication goes unanswered for want of a leader.
    NoLeader,
    /// The server is stopping.
    Stopping,
}

fn readiness() -> Readiness {
    if STOPPING.load(std::sync::atomic::Ordering::Relaxed) {
        Readiness::Stopping
    } else if crate::lua_engine::stopped_without_leader() {
        Readiness::NoLeader
    } else {
        Readiness::Ready
    }
}

/// Answers one request and closes. Nothing is sent to a client that does not finish its head.
async fn answer<S: AsyncRead + AsyncWrite + Unpin>(mut socket: S, metrics: &Metrics, read_timeout: Duration) -> io::Result<()> {
    let Ok(Ok(Some(head))) = tokio::time::timeout(read_timeout, read_head(&mut socket)).await else {
        return Ok(());
    };
    let response = respond(&head, metrics, readiness());
    // A client that does not read must not keep its place, any more than one that does not write.
    let writing = async {
        socket.write_all(response.as_bytes()).await?;
        socket.shutdown().await
    };
    tokio::time::timeout(read_timeout, writing).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the response could not be written"))?
}

/// Reads up to the blank line ending a request head; `None` without one in `MAX_HEAD_LEN` octets.
async fn read_head<S: AsyncRead + Unpin>(socket: &mut S) -> io::Result<Option<Vec<u8>>> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        if head.len() > MAX_HEAD_LEN {
            return Ok(None);
        }
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        head.extend_from_slice(&chunk[..read]);
    }
    Ok((head.len() <= MAX_HEAD_LEN).then_some(head))
}

/// The response to one request head. `/livez` answers while the process serves at all; `/readyz`
/// answers 503 while the node has stopped answering authentication for want of a leader, or is
/// stopping, so that a load balancer sends requests to the other nodes meanwhile.
fn respond(head: &[u8], metrics: &Metrics, readiness: Readiness) -> String {
    let line = head.split(|byte| *byte == b'\r').next().unwrap_or_default();
    let line = String::from_utf8_lossy(line);
    let mut parts = line.split(' ');
    let (method, target) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
    let path = target.split('?').next().unwrap_or_default();
    let reply = |status: &str, content_type: &str, body: &str| {
        format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
    };
    match (method, path) {
        ("GET", "/metrics") => reply("200 OK", "text/plain; version=0.0.4", &metrics.render()),
        ("GET", "/livez") => reply("200 OK", "text/plain", "ok\n"),
        ("GET", "/readyz") => match readiness {
            Readiness::Ready => reply("200 OK", "text/plain", "ok\n"),
            Readiness::NoLeader => reply("503 Service Unavailable", "text/plain", "no leader\n"),
            Readiness::Stopping => reply("503 Service Unavailable", "text/plain", "stopping\n"),
        },
        ("GET", _) => reply("404 Not Found", "text/plain", "not found\n"),
        _ => reply("405 Method Not Allowed", "text/plain", "method not allowed\n"),
    }
}

#[cfg(test)]
mod tests {
    /// The buckets example of the spec.
    #[test]
    fn test_histogram_buckets() {
        let histogram = super::Histogram::default();
        for micros in [300, 700, 3_000_000, 20_000_000] {
            histogram.observe(std::time::Duration::from_micros(micros));
        }
        let mut out = String::new();
        histogram.render(&mut out, "t", "kind=\"auth\"");
        let value = |sample: &str| out.lines().find_map(|line| line.strip_prefix(sample)?.strip_prefix(' ')).unwrap_or("missing").to_string();
        for bound in ["0.00005", "0.0001", "0.00025"] {
            assert_eq!(value(&format!("t_bucket{{kind=\"auth\",le=\"{bound}\"}}")), "0", "{bound}: {out}");
        }
        assert_eq!(value("t_bucket{kind=\"auth\",le=\"0.0005\"}"), "1", "{out}");
        for bound in ["0.001", "0.0025", "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5"] {
            assert_eq!(value(&format!("t_bucket{{kind=\"auth\",le=\"{bound}\"}}")), "2", "{bound}: {out}");
        }
        for bound in ["5", "10"] {
            assert_eq!(value(&format!("t_bucket{{kind=\"auth\",le=\"{bound}\"}}")), "3", "{bound}: {out}");
        }
        assert_eq!(value("t_bucket{kind=\"auth\",le=\"+Inf\"}"), "4", "{out}");
        assert_eq!(value("t_count{kind=\"auth\"}"), "4");
        assert_eq!(value("t_sum{kind=\"auth\"}"), "23.001");
        assert_eq!(out.lines().count(), 20);
        // What status reads back.
        let parsed = crate::status::parse(&out).map_err(|e| e.to_string());
        assert!(parsed.is_ok(), "{parsed:?}");
    }

    use super::*;
    use tokio::net::TcpStream;
    use std::net::Ipv4Addr;

    async fn start(read_timeout: Duration) -> (SocketAddr, Arc<Metrics>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let metrics = Arc::new(Metrics::default());
        tokio::spawn(serve_listener(listener, metrics.clone(), read_timeout));
        (addr, metrics)
    }

    /// Sends `request` and returns everything sent back until the server closes. A server
    /// closing a connection with unread data resets it, which reads as nothing sent back.
    async fn exchange(addr: SocketAddr, request: &[u8]) -> String {
        let mut socket = TcpStream::connect(addr).await.unwrap();
        let _ = socket.write_all(request).await;
        let mut response = Vec::new();
        let _ = socket.read_to_end(&mut response).await;
        String::from_utf8(response).unwrap()
    }

    #[tokio::test]
    async fn test_scrape() {
        let (addr, metrics) = start(READ_TIMEOUT).await;
        metrics.radius_packets_auth.inc();
        let response = exchange(addr, b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n").await;
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        assert!(head.contains("Content-Type: text/plain; version=0.0.4"), "{head}");
        assert!(body.contains("rradius_radius_packets_total{kind=\"auth\"} 1\n"), "{body}");
        for line in body.lines() {
            let sample = line.rsplit_once(' ').is_some_and(|(name, value)| name.starts_with("rradius_") && value.parse::<u64>().is_ok());
            assert!(line.starts_with("# HELP rradius_") || line.starts_with("# TYPE rradius_") || sample, "{line}");
        }
    }

    #[test]
    fn test_requests_in_progress() -> Result<(), String> {
        let metrics = Metrics::default();
        assert!(!metrics.render().contains("rradius_radius_requests_in_progress"), "only while RADIUS is served");
        assert!(!metrics.render().contains("rradius_radius_requests_limit"));
        let (auth, acct) = (Arc::new(Semaphore::new(4096)), Arc::new(Semaphore::new(512)));
        metrics.watch_radius_slots(auth.clone(), 4096, acct.clone(), 512);
        let held = auth.try_acquire().map_err(|e| e.to_string())?;
        let rendered = metrics.render();
        for line in ["rradius_radius_requests_in_progress{kind=\"auth\"} 1\n", "rradius_radius_requests_in_progress{kind=\"acct\"} 0\n", "rradius_radius_requests_limit{kind=\"auth\"} 4096\n", "rradius_radius_requests_limit{kind=\"acct\"} 512\n"] {
            assert!(rendered.contains(line), "{line}: {rendered}");
        }
        drop(held);
        assert!(metrics.render().contains("rradius_radius_requests_in_progress{kind=\"auth\"} 0\n"));
        Ok(())
    }

    #[test]
    fn test_version_and_start_time() -> Result<(), String> {
        let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_secs();
        let rendered = Metrics::default().with_version("1.2.3").render();
        let after = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_secs();
        assert!(rendered.contains("rradius_build_info{version=\"1.2.3\"} 1\n"), "{rendered}");
        let started = rendered.lines().find_map(|line| line.strip_prefix("rradius_start_time_seconds ")).ok_or("no start time")?;
        let started: u64 = started.parse().map_err(|e| format!("{e}"))?;
        assert!((before..=after).contains(&started), "{started} not in {before}..={after}");
        assert!(Metrics::default().render().contains("rradius_build_info{version=\"unknown\"} 1\n"));
        Ok(())
    }

    #[tokio::test]
    async fn test_request_forms() {
        let (addr, _) = start(READ_TIMEOUT).await;
        for (request, status) in [
            ("GET /metrics HTTP/1.1\r\n\r\n", "200"),
            ("GET /metrics?x=1 HTTP/1.1\r\n\r\n", "200"),
            ("GET / HTTP/1.1\r\n\r\n", "404"),
            ("POST /metrics HTTP/1.1\r\n\r\n", "405"),
        ] {
            let response = exchange(addr, request.as_bytes()).await;
            assert!(response.starts_with(&format!("HTTP/1.1 {status}")), "{request:?}: {response}");
        }
    }

    #[tokio::test]
    async fn test_health_over_the_listener() {
        let (addr, _) = start(READ_TIMEOUT).await;
        for path in ["/livez", "/readyz"] {
            let response = exchange(addr, format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).await;
            assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{path}: {response}");
            assert!(response.contains("Content-Type: text/plain\r\n"), "{path}: {response}");
            assert!(response.ends_with("\r\n\r\nok\n"), "{path}: {response}");
        }
    }

    /// The health paths table of the server-metrics spec: the request, `stop_without_leader_ms`
    /// (0 when not set), how long the node has been without a leader, and the expected status
    /// line and body (`None` when only the status is given).
    #[test]
    fn test_health_paths() {
        let metrics = Metrics::default();
        for (request, limit_ms, without_ms, stopping, status, body) in [
            ("GET /livez HTTP/1.1", 3000, Some(10_000), false, "200 OK", Some("ok\n")),
            ("GET /readyz HTTP/1.1", 0, Some(10_000), false, "200 OK", None),
            ("GET /readyz HTTP/1.1", 3000, None, false, "200 OK", None),
            ("GET /readyz HTTP/1.1", 3000, Some(2_000), false, "200 OK", None),
            ("GET /readyz HTTP/1.1", 3000, Some(4_000), false, "503 Service Unavailable", Some("no leader\n")),
            ("GET /readyz?verbose=1 HTTP/1.1", 3000, Some(4_000), false, "503 Service Unavailable", None),
            ("POST /readyz HTTP/1.1", 0, None, false, "405 Method Not Allowed", None),
            ("POST /livez HTTP/1.1", 0, None, false, "405 Method Not Allowed", None),
            ("GET /readyz HTTP/1.1", 0, None, true, "503 Service Unavailable", Some("stopping\n")),
            ("GET /readyz HTTP/1.1", 3000, Some(4_000), true, "503 Service Unavailable", Some("stopping\n")),
            ("GET /livez HTTP/1.1", 0, None, true, "200 OK", Some("ok\n")),
        ] {
            let status_now = crate::lua_engine::ClusterStatus { without_leader: without_ms.map(Duration::from_millis), ..Default::default() };
            let readiness = match (stopping, crate::lua_engine::stops_without_leader(limit_ms, &status_now)) {
                (true, _) => Readiness::Stopping,
                (false, true) => Readiness::NoLeader,
                (false, false) => Readiness::Ready,
            };
            let response = respond(format!("{request}\r\nHost: x\r\n\r\n").as_bytes(), &metrics, readiness);
            assert!(response.starts_with(&format!("HTTP/1.1 {status}\r\n")), "{request} {limit_ms} {without_ms:?} {stopping}: {response}");
            if let Some(body) = body {
                assert!(response.ends_with(&format!("\r\n\r\n{body}")), "{request}: {response}");
            }
        }
        // The other paths are unchanged by readiness.
        for readiness in [Readiness::NoLeader, Readiness::Stopping] {
            assert!(respond(b"GET /metrics HTTP/1.1\r\n\r\n", &metrics, readiness).starts_with("HTTP/1.1 200 OK"));
            assert!(respond(b"GET /livez HTTP/1.1\r\n\r\n", &metrics, readiness).starts_with("HTTP/1.1 200 OK"));
        }
        assert!(respond(b"GET /ready HTTP/1.1\r\n\r\n", &metrics, Readiness::Ready).starts_with("HTTP/1.1 404"));
    }

    #[tokio::test]
    async fn test_head_too_long_closes() {
        let (addr, _) = start(READ_TIMEOUT).await;
        let mut socket = TcpStream::connect(addr).await.unwrap();
        let _ = socket.write_all(&vec![b'a'; 9 * 1024]).await;
        let mut response = Vec::new();
        let _ = socket.read_to_end(&mut response).await;
        assert!(response.is_empty());
    }

    #[tokio::test]
    async fn test_client_that_does_not_read_is_closed() {
        let metrics = Metrics::default();
        // Too little room for the response, and nobody reading.
        let (mut client, server) = tokio::io::duplex(64);
        client.write_all(b"GET /metrics HTTP/1.1\r\n\r\n").await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), answer(server, &metrics, Duration::from_millis(100))).await.expect("still open").unwrap_or(());
    }

    #[tokio::test]
    async fn test_unfinished_head_closes() {
        let (addr, _) = start(Duration::from_millis(100)).await;
        let response = tokio::time::timeout(Duration::from_secs(5), exchange(addr, b"GET /metrics HTTP/1.1\r\n")).await.unwrap();
        assert!(response.is_empty());
    }

    #[tokio::test]
    async fn test_connection_limit() {
        let (addr, _) = start(Duration::from_secs(30)).await;
        let mut waiting = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            let mut socket = TcpStream::connect(addr).await.unwrap();
            socket.write_all(b"G").await.unwrap();
            waiting.push(socket);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let response = tokio::time::timeout(Duration::from_secs(5), exchange(addr, b"GET /metrics HTTP/1.1\r\n\r\n")).await.unwrap();
        assert!(response.is_empty(), "{response}");
        waiting.clear();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(exchange(addr, b"GET /metrics HTTP/1.1\r\n\r\n").await.starts_with("HTTP/1.1 200"));
    }

    /// Every `rradius_eap_sessions_total` line, in order.
    fn eap_session_lines(body: &str) -> Vec<&str> {
        body.lines().filter(|line| line.starts_with("rradius_eap_sessions_total{")).collect()
    }

    /// An extension offering one EAP method, of type 254.
    struct FakeMethods;

    const FAKE_METHOD: crate::eap_method::EapMethodName = crate::eap_method::EapMethodName { kind: 254, name: "fake-eap", handler: "eap-fake", title: "FAKE-EAP" };

    impl crate::extension::Extension for FakeMethods {
        fn name(&self) -> &'static str {
            "fake-methods"
        }

        fn eap_method_names(&self) -> Vec<crate::eap_method::EapMethodName> {
            vec![FAKE_METHOD]
        }
    }

    #[test]
    fn test_eap_sessions_of_extension_methods() -> Result<(), String> {
        let metrics = Metrics::for_extensions(&crate::extension::Extensions::new(vec![Arc::new(FakeMethods)])?);
        metrics.eap(FAKE_METHOD.kind).overload.inc();
        let body = metrics.render();
        let mut expected = Vec::new();
        for method in ["tls", "peap", "ttls", "fake-eap"] {
            for result in ["accept", "reject", "timeout", "overload", "replaced", "evicted"] {
                let value = u64::from((method, result) == ("fake-eap", "overload"));
                expected.push(format!("rradius_eap_sessions_total{{method=\"{method}\",result=\"{result}\"}} {value}"));
            }
        }
        assert_eq!(eap_session_lines(&body), expected, "{body}");
        assert_eq!(metrics.eap_tls.overload.get(), 0);
        Ok(())
    }

    #[test]
    fn test_eap_sessions_without_extension_methods() {
        let metrics = Metrics::default();
        metrics.eap(FAKE_METHOD.kind).accept.inc();
        let body = metrics.render();
        assert!(!body.contains("method=\"fake-eap\""), "{body}");
        assert_eq!(eap_session_lines(&body).len(), 18, "{body}");
        assert_eq!(metrics.eap_tls.accept.get(), 1, "an unknown type counts as TLS, as before");
    }

    #[test]
    fn test_zero_at_start() {
        let body = Metrics::default().render();
        // RadSec, dynamic clients, CoA, DPSK and EAP-AKA', and their metrics, are in
        // rradius-enterprise only.
        for name in ["rradius_radsec_", "rradius_dynamic_client", "rradius_dynauth_", "rradius_coa_relay", "rradius_dpsk_", "method=\"aka-prime\""] {
            assert!(!body.contains(name), "{name}: {body}");
        }
        for sample in [
            "rradius_radius_packets_total{kind=\"auth\"} 0",
            "rradius_radius_packets_total{kind=\"acct\"} 0",
            "rradius_radius_responses_total{code=\"accounting_response\"} 0",
            "rradius_radius_status_server_total 0",
            "rradius_radius_duplicates_total 0",
            "rradius_radius_discarded_total{reason=\"unknown_client\"} 0",
            "rradius_tacacs_connections_total{result=\"refused\"} 0",
            "rradius_tacacs_connections_open 0",
            "rradius_tacacs_requests_total{type=\"author\"} 0",
            "rradius_tacacs_malformed_total 0",
            "rradius_acct_spool_bytes 0",
            "rradius_log_dropped_lines_total 0",
            "rradius_script_http_requests_total{result=\"denied\"} 0",
            "rradius_eap_sessions_total{method=\"tls\",result=\"accept\"} 0",
            "rradius_eap_sessions_total{method=\"peap\",result=\"accept\"} 0",
            "rradius_eap_sessions_total{method=\"ttls\",result=\"accept\"} 0",
            "rradius_reload_total{result=\"error\"} 0",
            "rradius_reload_last_success_timestamp_seconds 0",
        ] {
            assert!(body.lines().any(|line| line == sample), "{sample} missing from:\n{body}");
        }
        // Proxying is rradius-enterprise's: the core reports none of its metrics.
        assert!(!body.contains("rradius_proxy_"), "proxy metrics in:\n{body}");
        // Counted for the whole process, which tests share, so only their presence is certain.
        for name in ["rradius_db_batched_writes_total ", "rradius_db_split_batches_total "] {
            assert!(body.lines().any(|line| line.starts_with(name)), "{name}missing from:\n{body}");
        }
    }
}
