//! The `status` command: what this node is doing now, read from its own `/readyz` and `/metrics`,
//! as rates over the time between two samples and current values, as text or JSON.

use crate::cluster_commands::{current_members, Api, Members, Source};
use crate::config::ClusterSettings;
use crate::extension::{Extensions, Show, StatusItem, StatusSection};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::future::Future;
use std::io::{IsTerminal, Write as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::pin::Pin;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::signal::unix::{signal, SignalKind};

pub const USAGE: &str = "usage: rradius-server status [--config <path>] [--watch] [--json] [--cluster]";

/// How long one exchange with the metrics port may take.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(2);
/// The largest answer read.
const MAX_ANSWER: usize = 8 * 1024 * 1024;
/// The time between samples.
const INTERVAL: Duration = Duration::from_secs(1);
/// The clients the busiest-clients section lists at most.
const TOP_CLIENTS: usize = 5;

/// Why a sample could not be had.
#[derive(Debug, Clone, PartialEq)]
pub enum FetchError {
    /// Nothing answered, or not in time.
    Unreachable(String),
    /// Something answered, but not as the server does.
    Format(String),
}

impl FetchError {
    fn message(&self, address: SocketAddr) -> String {
        match self {
            FetchError::Unreachable(e) => format!("cannot reach the metrics port at {address}: {e}"),
            FetchError::Format(e) => format!("{address}/metrics did not answer as rradius does: {e}"),
        }
    }
}

/// Sends `GET path` to `address` and returns the status code and the body.
pub async fn get(address: SocketAddr, path: &str) -> Result<(u16, String), FetchError> {
    let exchange = async {
        let mut socket = TcpStream::connect(address).await.map_err(|e| FetchError::Unreachable(e.to_string()))?;
        let request = format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
        socket.write_all(request.as_bytes()).await.map_err(|e| FetchError::Unreachable(e.to_string()))?;
        let mut answer = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            let read = socket.read(&mut chunk).await.map_err(|e| FetchError::Unreachable(e.to_string()))?;
            if read == 0 {
                break;
            }
            if answer.len() + read > MAX_ANSWER {
                return Err(FetchError::Format(format!("the answer is longer than {} MiB", MAX_ANSWER / 1024 / 1024)));
            }
            answer.extend_from_slice(&chunk[..read]);
        }
        Ok(answer)
    };
    let answer = tokio::time::timeout(EXCHANGE_TIMEOUT, exchange)
        .await
        .map_err(|_| FetchError::Unreachable(format!("no answer within {} s", EXCHANGE_TIMEOUT.as_secs())))??;
    let answer = String::from_utf8(answer).map_err(|_| FetchError::Format("the answer is not UTF-8".to_string()))?;
    let (head, body) = answer.split_once("\r\n\r\n").ok_or_else(|| FetchError::Format("no end of the head".to_string()))?;
    let status_line = head.lines().next().unwrap_or_default();
    let status = status_line
        .strip_prefix("HTTP/1.1 ")
        .or_else(|| status_line.strip_prefix("HTTP/1.0 "))
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| FetchError::Format(format!("not an HTTP status line: {status_line:?}")))?;
    Ok((status, body.to_string()))
}

/// The labels of a sample, by name, so that their order does not matter.
pub type Labels = BTreeMap<String, String>;

/// The samples of one scrape: by metric name, each sample's labels and value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scrape(BTreeMap<String, Vec<(Labels, f64)>>);

impl Scrape {
    /// The value of `metric` with exactly these labels; with none, the sum of all its samples,
    /// such as those of each server or stage.
    pub fn value(&self, metric: &str, labels: &[(&str, &str)]) -> Option<f64> {
        let samples = self.0.get(metric)?;
        if labels.is_empty() {
            return (!samples.is_empty()).then(|| samples.iter().map(|(_, value)| value).sum());
        }
        samples.iter().find(|(have, _)| have.len() == labels.len() && labels.iter().all(|(name, value)| have.get(*name).map(String::as_str) == Some(*value))).map(|(_, value)| *value)
    }

    /// Every sample of `metric`.
    pub fn samples(&self, metric: &str) -> &[(Labels, f64)] {
        self.0.get(metric).map_or(&[], Vec::as_slice)
    }
}

/// Reads the Prometheus text format as the server writes it. A line that is not a comment and not
/// a sample makes it an error.
pub fn parse(text: &str) -> Result<Scrape, String> {
    let mut scrape = Scrape::default();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, labels, value) = parse_sample(line).map_err(|e| format!("line {}: {e}: {line:?}", index + 1))?;
        scrape.0.entry(name).or_default().push((labels, value));
    }
    Ok(scrape)
}

fn parse_sample(line: &str) -> Result<(String, Labels, f64), &'static str> {
    let name_end = line.find(['{', ' ']).ok_or("no value")?;
    let name = &line[..name_end];
    let valid_name = name.chars().enumerate().all(|(i, c)| c.is_ascii_alphabetic() || c == '_' || c == ':' || (i > 0 && c.is_ascii_digit()));
    if name.is_empty() || !valid_name {
        return Err("not a metric name");
    }
    let mut rest = &line[name_end..];
    let mut labels = Labels::new();
    if let Some(after) = rest.strip_prefix('{') {
        rest = after;
        loop {
            if let Some(after) = rest.strip_prefix('}') {
                rest = after;
                break;
            }
            let (label, after) = rest.split_once("=\"").ok_or("a label without a quoted value")?;
            let mut value = String::new();
            let mut chars = after.char_indices();
            let end = loop {
                match chars.next() {
                    Some((at, '"')) => break at,
                    Some((_, '\\')) => match chars.next() {
                        Some((_, 'n')) => value.push('\n'),
                        Some((_, c @ ('\\' | '"'))) => value.push(c),
                        _ => return Err("an unknown escape in a label value"),
                    },
                    Some((_, c)) => value.push(c),
                    None => return Err("a label value without its end"),
                }
            };
            labels.insert(label.to_string(), value);
            rest = &after[end + 1..];
            rest = rest.strip_prefix(',').unwrap_or(rest);
        }
    }
    let value = rest.strip_prefix(' ').ok_or("no space before the value")?;
    let value: f64 = value.trim().parse().map_err(|_| "a value that is not a number")?;
    Ok((name.to_string(), labels, value))
}

/// What `/readyz` said.
#[derive(Debug, Clone, PartialEq)]
pub struct Ready {
    pub status: u16,
    pub text: String,
}

/// One look at the node: when it was had, `/readyz`, `/metrics`.
#[derive(Debug, Clone)]
pub struct Sample {
    pub at: Instant,
    pub ready: Ready,
    pub scrape: Scrape,
}

/// Takes one sample of the node at `address`.
pub async fn sample(address: SocketAddr) -> Result<Sample, FetchError> {
    let (status, text) = get(address, "/readyz").await?;
    if status != 200 && status != 503 {
        return Err(FetchError::Format(format!("/readyz answered {status}")));
    }
    let ready = Ready { status, text: text.trim().to_string() };
    let (status, body) = get(address, "/metrics").await?;
    if status != 200 {
        return Err(FetchError::Format(format!("/metrics answered {status}")));
    }
    let at = Instant::now();
    let scrape = parse(&body).map_err(FetchError::Format)?;
    Ok(Sample { at, ready, scrape })
}

/// A value shown as it is.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Number(f64),
    Text(String),
}

/// What an item shows.
#[derive(Debug, Clone, PartialEq)]
pub enum Reading {
    Value(Value),
    Rate { rate: f64, total: f64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShownItem {
    pub key: String,
    pub label: String,
    pub reading: Reading,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShownSection {
    pub key: String,
    pub title: String,
    pub items: Vec<ShownItem>,
}

/// What one print shows: made from two samples, the second the newer.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub address: SocketAddr,
    pub interval: f64,
    pub ready: Ready,
    pub counter_reset: bool,
    pub sections: Vec<ShownSection>,
}

/// The rate of a counter from `before` to `after` over `seconds`, and whether it went down.
pub fn rate(before: f64, after: f64, seconds: f64) -> (f64, bool) {
    if after < before {
        return (0.0, true);
    }
    if seconds <= 0.0 {
        return (0.0, false);
    }
    ((after - before) / seconds, false)
}

const fn item(label: &'static str, key: &'static str, metric: &'static str, labels: &'static [(&'static str, &'static str)], show: Show) -> StatusItem {
    StatusItem { label, key, metric, labels, show }
}

/// The server's own sections after the node and before the busiest clients: reload and RADIUS.
fn core_sections_first() -> Vec<StatusSection> {
    vec![
        StatusSection {
            title: "Reload",
            key: "reload",
            items: vec![
                item("Successful", "ok", "rradius_reload_total", &[("result", "ok")], Show::Total),
                item("Failed", "error", "rradius_reload_total", &[("result", "error")], Show::Total),
                item("Last success (Unix time)", "last_success", "rradius_reload_last_success_timestamp_seconds", &[], Show::Value),
            ],
        },
        StatusSection {
            title: "RADIUS",
            key: "radius",
            items: vec![
                item("Auth requests", "auth_requests", "rradius_radius_packets_total", &[("kind", "auth")], Show::Rate),
                item("Acct requests", "acct_requests", "rradius_radius_packets_total", &[("kind", "acct")], Show::Rate),
                item("Access-Accept", "access_accept", "rradius_radius_responses_total", &[("code", "access_accept")], Show::Rate),
                item("Access-Reject", "access_reject", "rradius_radius_responses_total", &[("code", "access_reject")], Show::Rate),
                item("Access-Challenge", "access_challenge", "rradius_radius_responses_total", &[("code", "access_challenge")], Show::Rate),
                item("Accounting-Response", "accounting_response", "rradius_radius_responses_total", &[("code", "accounting_response")], Show::Rate),
                item("Duplicates", "duplicates", "rradius_radius_duplicates_total", &[], Show::Rate),
                item("Discarded: overload", "discarded_overload", "rradius_radius_discarded_total", &[("reason", "overload")], Show::Rate),
                item("Discarded: unknown client", "discarded_unknown_client", "rradius_radius_discarded_total", &[("reason", "unknown_client")], Show::Rate),
                item("Discarded: invalid", "discarded_invalid", "rradius_radius_discarded_total", &[("reason", "invalid")], Show::Rate),
                item("Discarded: database", "discarded_database", "rradius_radius_discarded_total", &[("reason", "database")], Show::Rate),
                item("Discarded: no leader", "discarded_no_leader", "rradius_radius_discarded_total", &[("reason", "no_leader")], Show::Rate),
                item("Auth in progress", "auth_in_progress", "rradius_radius_requests_in_progress", &[("kind", "auth")], Show::Value),
                item("Auth limit", "auth_limit", "rradius_radius_requests_limit", &[("kind", "auth")], Show::Value),
                item("Acct in progress", "acct_in_progress", "rradius_radius_requests_in_progress", &[("kind", "acct")], Show::Value),
                item("Acct limit", "acct_limit", "rradius_radius_requests_limit", &[("kind", "acct")], Show::Value),
                item("Auth p50 (ms)", "auth_p50_ms", "rradius_radius_response_seconds", &[("kind", "auth")], Show::Quantile(0.5)),
                item("Auth p99 (ms)", "auth_p99_ms", "rradius_radius_response_seconds", &[("kind", "auth")], Show::Quantile(0.99)),
                item("Acct p50 (ms)", "acct_p50_ms", "rradius_radius_response_seconds", &[("kind", "acct")], Show::Quantile(0.5)),
                item("Acct p99 (ms)", "acct_p99_ms", "rradius_radius_response_seconds", &[("kind", "acct")], Show::Quantile(0.99)),
            ],
        },
    ]
}

/// The server's own sections after the busiest clients.
fn core_sections_last() -> Vec<StatusSection> {
    vec![
        StatusSection {
            title: "TACACS+",
            key: "tacacs",
            items: vec![
                item("Open connections", "connections_open", "rradius_tacacs_connections_open", &[], Show::Value),
                item("Authentication", "authen", "rradius_tacacs_requests_total", &[("type", "authen")], Show::Rate),
                item("Authorization", "author", "rradius_tacacs_requests_total", &[("type", "author")], Show::Rate),
                item("Accounting", "acct", "rradius_tacacs_requests_total", &[("type", "acct")], Show::Rate),
                item("Malformed", "malformed", "rradius_tacacs_malformed_total", &[], Show::Rate),
                item("Authentication p50 (ms)", "authen_p50_ms", "rradius_tacacs_response_seconds", &[("type", "authen")], Show::Quantile(0.5)),
                item("Authentication p99 (ms)", "authen_p99_ms", "rradius_tacacs_response_seconds", &[("type", "authen")], Show::Quantile(0.99)),
                item("Authorization p50 (ms)", "author_p50_ms", "rradius_tacacs_response_seconds", &[("type", "author")], Show::Quantile(0.5)),
                item("Authorization p99 (ms)", "author_p99_ms", "rradius_tacacs_response_seconds", &[("type", "author")], Show::Quantile(0.99)),
                item("Accounting p50 (ms)", "acct_p50_ms", "rradius_tacacs_response_seconds", &[("type", "acct")], Show::Quantile(0.5)),
                item("Accounting p99 (ms)", "acct_p99_ms", "rradius_tacacs_response_seconds", &[("type", "acct")], Show::Quantile(0.99)),
            ],
        },
        StatusSection { title: "EAP", key: "eap", items: vec![item("Sessions under way", "sessions_open", "rradius_eap_sessions_open", &[], Show::Value)] },
        StatusSection {
            title: "Accounting spool",
            key: "acct_spool",
            items: vec![
                item("Bytes waiting", "bytes", "rradius_acct_spool_bytes", &[], Show::Value),
                item("Replayed", "replayed", "rradius_acct_spool_replayed_total", &[], Show::Rate),
                item("Rejected", "rejected", "rradius_acct_spool_rejected_total", &[], Show::Rate),
                item("Not spooled", "full", "rradius_acct_spool_full_total", &[], Show::Rate),
            ],
        },
        StatusSection {
            title: "Netlogon",
            key: "netlogon",
            items: vec![
                item("Open connections", "open_connections", "rradius_netlogon_open_connections", &[], Show::Value),
                item("Logons accepted", "accept", "rradius_netlogon_logons_total", &[("result", "accept")], Show::Rate),
                item("Logons rejected", "reject", "rradius_netlogon_logons_total", &[("result", "reject")], Show::Rate),
                item("Logons of unknown users", "no_such_user", "rradius_netlogon_logons_total", &[("result", "no_such_user")], Show::Rate),
                item("Logons without a DC", "unavailable", "rradius_netlogon_logons_total", &[("result", "unavailable")], Show::Rate),
            ],
        },
        StatusSection { title: "Log", key: "log", items: vec![item("Dropped lines", "dropped_lines", "rradius_log_dropped_lines_total", &[], Show::Rate)] },
    ]
}

/// The cumulative buckets of the histogram `metric` with exactly `labels` besides `le`, by upper
/// bound, the last `+Inf`; `None` when there is no `+Inf` bucket. A bucket whose `le` is not a
/// number is passed over.
pub fn histogram_buckets(scrape: &Scrape, metric: &str, labels: &[(&str, &str)]) -> Option<Vec<(f64, f64)>> {
    let mut buckets: Vec<(f64, f64)> = scrape
        .samples(&format!("{metric}_bucket"))
        .iter()
        .filter(|(have, _)| have.len() == labels.len() + 1 && labels.iter().all(|(name, value)| have.get(*name).map(String::as_str) == Some(*value)))
        .filter_map(|(have, count)| {
            let bound = match have.get("le")?.as_str() {
                "+Inf" => f64::INFINITY,
                text => text.parse::<f64>().ok().filter(|bound| bound.is_finite())?,
            };
            Some((bound, *count))
        })
        .collect();
    buckets.sort_by(|a, b| a.0.total_cmp(&b.0));
    (buckets.last()?.0 == f64::INFINITY).then_some(buckets)
}

/// What a quantile of the requests counted between two looks at a histogram's buckets came to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Quantile {
    /// In seconds; the last finite bound when it falls past it.
    Seconds(f64),
    /// Nothing was counted, or the buckets of the two looks differ.
    None,
    /// A count went down: the server restarted.
    Reset,
}

/// The quantile `q` of what was counted between the cumulative buckets `before` and `after`: in
/// the bucket where the count first reaches `q` times the total, interpolated from its lower bound
/// (0 for the first) to its upper bound.
pub fn quantile(before: &[(f64, f64)], after: &[(f64, f64)], q: f64) -> Quantile {
    if before.len() != after.len() || before.iter().zip(after).any(|(then, now)| then.0 != now.0) {
        return Quantile::None;
    }
    let counted: Vec<(f64, f64)> = before.iter().zip(after).map(|(then, now)| (now.0, now.1 - then.1)).collect();
    if counted.iter().any(|(_, count)| *count < 0.0) {
        return Quantile::Reset;
    }
    let total = counted.last().map_or(0.0, |(_, count)| *count);
    if total <= 0.0 {
        return Quantile::None;
    }
    let wanted = q * total;
    let (mut lower, mut below) = (0.0, 0.0);
    for (bound, count) in counted {
        if count >= wanted {
            if bound.is_infinite() {
                return Quantile::Seconds(lower);
            }
            let inside = count - below;
            if inside <= 0.0 {
                return Quantile::Seconds(lower);
            }
            return Quantile::Seconds(lower + (bound - lower) * (wanted - below) / inside);
        }
        (lower, below) = (bound, count);
    }
    Quantile::Seconds(lower)
}

/// Makes a section of what two samples have for its items; `None` when it would be empty.
fn show_section(section: &StatusSection, before: &Scrape, after: &Scrape, seconds: f64, reset: &mut bool) -> Option<ShownSection> {
    let items: Vec<ShownItem> = section
        .items
        .iter()
        .filter_map(|item| {
            if let Show::Quantile(q) = item.show {
                let buckets = |scrape: &Scrape| histogram_buckets(scrape, item.metric, item.labels);
                let value = match quantile(&buckets(before)?, &buckets(after)?, q) {
                    Quantile::Seconds(seconds) => (seconds * 1_000_000.0).round() / 1000.0,
                    Quantile::None => return None,
                    Quantile::Reset => {
                        *reset = true;
                        return None;
                    }
                };
                return Some(ShownItem { key: item.key.to_string(), label: item.label.to_string(), reading: Reading::Value(Value::Number(value)) });
            }
            let now = after.value(item.metric, item.labels)?;
            let reading = match item.show {
                Show::Value | Show::Total | Show::Quantile(_) => Reading::Value(Value::Number(now)),
                Show::Rate => {
                    let (rate, went_down) = before.value(item.metric, item.labels).map_or((0.0, false), |then| rate(then, now, seconds));
                    *reset |= went_down;
                    Reading::Rate { rate, total: now }
                }
            };
            Some(ShownItem { key: item.key.to_string(), label: item.label.to_string(), reading })
        })
        .collect();
    (!items.is_empty()).then(|| ShownSection { key: section.key.to_string(), title: section.title.to_string(), items })
}

/// The node section: readiness, the cluster, the version and how long it has run.
fn node_section(ready: &Ready, after: &Scrape, now_unix: f64) -> ShownSection {
    let mut items = vec![ShownItem { key: "ready".to_string(), label: "Ready".to_string(), reading: Reading::Value(Value::Text(ready.text.clone())) }];
    for (key, label, metric) in [
        ("is_leader", "Leader", "rradius_cluster_is_leader"),
        ("has_leader", "Has a leader", "rradius_cluster_leader"),
        ("caught_up", "Caught up", "rradius_cluster_caught_up"),
        ("applied_index", "Applied index", "rradius_cluster_applied_index"),
        ("seconds_without_leader", "Seconds without a leader", "rradius_cluster_seconds_without_leader"),
    ] {
        if let Some(value) = after.value(metric, &[]) {
            items.push(ShownItem { key: key.to_string(), label: label.to_string(), reading: Reading::Value(Value::Number(value)) });
        }
    }
    if let Some((labels, _)) = after.samples("rradius_build_info").first() {
        let version = labels.get("version").cloned().unwrap_or_default();
        items.push(ShownItem { key: "version".to_string(), label: "Version".to_string(), reading: Reading::Value(Value::Text(version)) });
    }
    if let Some(started) = after.value("rradius_start_time_seconds", &[]).filter(|started| *started > 0.0) {
        let running = (now_unix - started).max(0.0).floor();
        items.push(ShownItem { key: "seconds_running".to_string(), label: "Seconds running".to_string(), reading: Reading::Value(Value::Number(running)) });
    }
    ShownSection { key: "node".to_string(), title: "Node".to_string(), items }
}

/// The clients with the most responses per second, largest first and by name among equals,
/// leaving out those with none: from each client's rate and total.
pub fn top_clients(mut clients: Vec<(String, f64, f64)>) -> Vec<(String, f64, f64)> {
    clients.retain(|(_, rate, _)| *rate > 0.0);
    clients.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    clients.truncate(TOP_CLIENTS);
    clients
}

/// The busiest clients section, from the responses each client was sent.
fn clients_section(before: &Scrape, after: &Scrape, seconds: f64, reset: &mut bool) -> Option<ShownSection> {
    let by_client = |scrape: &Scrape| {
        let mut sums: BTreeMap<String, f64> = BTreeMap::new();
        for (labels, value) in scrape.samples("rradius_radius_client_responses_total") {
            if let Some(client) = labels.get("client") {
                *sums.entry(client.clone()).or_default() += value;
            }
        }
        sums
    };
    let (then, now) = (by_client(before), by_client(after));
    let clients = now
        .into_iter()
        .map(|(client, total)| {
            let (rate, went_down) = then.get(&client).map_or((0.0, false), |then| rate(*then, total, seconds));
            *reset |= went_down;
            (client, rate, total)
        })
        .collect();
    let items: Vec<ShownItem> = top_clients(clients)
        .into_iter()
        .map(|(client, rate, total)| ShownItem { key: client.clone(), label: client, reading: Reading::Rate { rate, total } })
        .collect();
    (!items.is_empty()).then(|| ShownSection { key: "radius_top_clients".to_string(), title: "Busiest RADIUS clients".to_string(), items })
}

/// What to print for two samples of the node at `address`; `extra` are the extensions' sections.
pub fn view(address: SocketAddr, before: &Sample, after: &Sample, extra: &[StatusSection], now_unix: f64) -> View {
    let seconds = after.at.saturating_duration_since(before.at).as_secs_f64();
    let mut reset = false;
    let mut sections = vec![node_section(&after.ready, &after.scrape, now_unix)];
    for section in core_sections_first() {
        sections.extend(show_section(&section, &before.scrape, &after.scrape, seconds, &mut reset));
    }
    sections.extend(clients_section(&before.scrape, &after.scrape, seconds, &mut reset));
    for section in core_sections_last().iter().chain(extra) {
        sections.extend(show_section(section, &before.scrape, &after.scrape, seconds, &mut reset));
    }
    View { address, interval: seconds, ready: after.ready.clone(), counter_reset: reset, sections }
}

/// A number as people read it: whole numbers without a fraction.
fn number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// The view as text for people.
pub fn text(view: &View) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "rradius status of {} over {:.1} s", view.address, view.interval);
    if view.counter_reset {
        let _ = writeln!(out, "counters reset since the previous sample: the server restarted");
    }
    for section in &view.sections {
        let _ = writeln!(out, "\n{}", section.title);
        let width = section.items.iter().map(|item| item.label.chars().count()).max().unwrap_or(0);
        for item in &section.items {
            let shown = match &item.reading {
                Reading::Value(Value::Number(value)) => number(*value),
                Reading::Value(Value::Text(text)) => text.clone(),
                Reading::Rate { rate, total } => format!("{rate:.1}/s ({})", number(*total)),
            };
            let _ = writeln!(out, "  {:width$}  {shown}", item.label);
        }
    }
    out
}

/// A number in JSON: an integer when it is whole.
fn json_number(value: f64) -> serde_json::Value {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        serde_json::Value::from(value as i64)
    } else {
        serde_json::Value::from(value)
    }
}

/// The view as one JSON object.
pub fn json(view: &View) -> serde_json::Value {
    let sections: Vec<serde_json::Value> = view
        .sections
        .iter()
        .map(|section| {
            let items: Vec<serde_json::Value> = section
                .items
                .iter()
                .map(|item| {
                    let mut object = serde_json::Map::new();
                    object.insert("key".to_string(), item.key.clone().into());
                    object.insert("label".to_string(), item.label.clone().into());
                    match &item.reading {
                        Reading::Value(Value::Number(value)) => {
                            object.insert("value".to_string(), json_number(*value));
                        }
                        Reading::Value(Value::Text(text)) => {
                            object.insert("value".to_string(), text.clone().into());
                        }
                        Reading::Rate { rate, total } => {
                            object.insert("rate".to_string(), serde_json::Value::from(*rate));
                            object.insert("total".to_string(), json_number(*total));
                        }
                    }
                    serde_json::Value::Object(object)
                })
                .collect();
            serde_json::json!({"key": section.key, "title": section.title, "items": items})
        })
        .collect();
    serde_json::json!({
        "address": view.address.to_string(),
        "interval_seconds": view.interval,
        "ready": {"status": view.ready.status, "text": view.ready.text},
        "counter_reset": view.counter_reset,
        "sections": sections,
    })
}

/// Where to connect for a `[metrics]` bound to `bind`: its loopback when it is unspecified.
pub fn target(bind: SocketAddr) -> SocketAddr {
    match bind.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), bind.port()),
        IpAddr::V6(ip) if ip.is_unspecified() => SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), bind.port()),
        _ => bind,
    }
}

/// The metrics address of a cluster member: the host of its api address `member_api` with this
/// node's metrics `port`, as `host:port` to be resolved.
pub fn metrics_address(member_api: &str, port: u16) -> Result<String, String> {
    let host = member_api.rsplit_once(':').map(|(host, _)| host).filter(|host| !host.is_empty()).ok_or_else(|| format!("no host and port in the api address {member_api:?}"))?;
    if host.contains(':') && !host.starts_with('[') {
        return Ok(format!("[{host}]:{port}"));
    }
    Ok(format!("{host}:{port}"))
}

/// The first address `address` resolves to.
async fn resolve(address: &str) -> Result<SocketAddr, String> {
    let found = tokio::time::timeout(EXCHANGE_TIMEOUT, tokio::net::lookup_host(address))
        .await
        .map_err(|_| format!("cannot resolve {address} within {} s", EXCHANGE_TIMEOUT.as_secs()))?
        .map_err(|e| format!("cannot resolve {address}: {e}"))?;
    found.into_iter().next().ok_or_else(|| format!("{address} resolves to no address"))
}

/// One member in a look at the whole cluster: its view, or why there is none.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MemberView {
    pub(crate) id: u64,
    pub(crate) role: &'static str,
    pub(crate) address: String,
    pub(crate) shown: Result<View, String>,
}

/// What one print of `status --cluster` shows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ClusterView {
    pub(crate) from: Source,
    pub(crate) interval: f64,
    pub(crate) members: Vec<MemberView>,
}

/// The numbers of one member that the cluster lines show and add up.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Figures {
    pub(crate) auth: f64,
    pub(crate) acct: f64,
    pub(crate) discarded: f64,
    pub(crate) in_progress: f64,
}

/// The cluster's total: the figures of the members that could be sampled, added up.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Total {
    pub(crate) figures: Figures,
    pub(crate) reachable: usize,
    pub(crate) members: usize,
}

/// What a reading of the RADIUS section counts: a rate per second, or a value.
fn radius_readings(view: &View) -> impl Iterator<Item = (&str, f64)> {
    view.sections.iter().filter(|section| section.key == "radius").flat_map(|section| section.items.iter()).filter_map(|item| match &item.reading {
        Reading::Rate { rate, .. } => Some((item.key.as_str(), *rate)),
        Reading::Value(Value::Number(value)) => Some((item.key.as_str(), *value)),
        Reading::Value(Value::Text(_)) => None,
    })
}

pub(crate) fn figures(view: &View) -> Figures {
    let mut figures = Figures::default();
    for (key, number) in radius_readings(view) {
        match key {
            "auth_requests" => figures.auth = number,
            "acct_requests" => figures.acct = number,
            "auth_in_progress" | "acct_in_progress" => figures.in_progress += number,
            _ if key.starts_with("discarded_") => figures.discarded += number,
            _ => {}
        }
    }
    figures
}

pub(crate) fn total(cluster: &ClusterView) -> Total {
    let mut total = Total { members: cluster.members.len(), ..Total::default() };
    for shown in cluster.members.iter().filter_map(|member| member.shown.as_ref().ok()) {
        let figures = figures(shown);
        total.reachable += 1;
        total.figures.auth += figures.auth;
        total.figures.acct += figures.acct;
        total.figures.discarded += figures.discarded;
        total.figures.in_progress += figures.in_progress;
    }
    total
}

/// A member's role: what its own metrics say about leading when it could be sampled, else the
/// leader the cluster sees; the others are followers when they vote and learners when not.
fn role(id: u64, members: &Members, leader: Option<u64>, shown: Option<&View>) -> &'static str {
    let leads = match shown.and_then(|view| view.sections.first()).and_then(|node| node.items.iter().find(|item| item.key == "is_leader")) {
        Some(item) => item.reading == Reading::Value(Value::Number(1.0)),
        None => leader == Some(id),
    };
    if leads {
        "leader"
    } else if members.voters.contains(&id) {
        "follower"
    } else {
        "learner"
    }
}

const NOT_REACHABLE_HINT: &str = "every node's [metrics] bind has to be an address this host can reach, on the same port as here";

/// The cluster view as text: one line per member, then the total and why members were left out.
pub(crate) fn cluster_text(cluster: &ClusterView) -> String {
    let from = match cluster.from {
        Source::Cluster => "the cluster",
        Source::Configuration => "this configuration: no node's api answered",
    };
    let mut out = String::new();
    let _ = writeln!(out, "rradius cluster status over {:.1} s, members from {from}\n", cluster.interval);
    let mut rows: Vec<Vec<String>> = vec![["node", "role", "ready", "auth/s", "acct/s", "discarded/s", "in progress", "address"].map(str::to_string).to_vec()];
    for member in &cluster.members {
        let (ready, numbers) = match &member.shown {
            Ok(shown) => {
                let figures = figures(shown);
                (shown.ready.text.clone(), [format!("{:.1}", figures.auth), format!("{:.1}", figures.acct), format!("{:.1}", figures.discarded), number(figures.in_progress)])
            }
            Err(_) => ("unreachable".to_string(), ["-", "-", "-", "-"].map(str::to_string)),
        };
        let mut row = vec![member.id.to_string(), member.role.to_string(), ready];
        row.extend(numbers);
        row.push(member.address.clone());
        rows.push(row);
    }
    let total = total(cluster);
    rows.push(vec![
        "total".to_string(),
        String::new(),
        format!("{}/{}", total.reachable, total.members),
        format!("{:.1}", total.figures.auth),
        format!("{:.1}", total.figures.acct),
        format!("{:.1}", total.figures.discarded),
        number(total.figures.in_progress),
        String::new(),
    ]);
    let widths: Vec<usize> = (0..rows[0].len()).map(|column| rows.iter().map(|row| row[column].chars().count()).max().unwrap_or(0)).collect();
    for row in &rows {
        let line: Vec<String> = row.iter().zip(&widths).map(|(cell, width)| format!("{cell:width$}")).collect();
        let _ = writeln!(out, "{}", line.join("  ").trim_end());
    }
    let mut unreachable = cluster.members.iter().filter_map(|member| member.shown.as_ref().err().map(|e| (member.id, e))).peekable();
    if unreachable.peek().is_some() {
        let _ = writeln!(out);
        for (id, e) in unreachable {
            let _ = writeln!(out, "node {id}: {e}");
        }
        let _ = writeln!(out, "({NOT_REACHABLE_HINT})");
    }
    out
}

/// The cluster view as one JSON object; each member's `status` is what `status --json` prints.
pub(crate) fn cluster_json(cluster: &ClusterView) -> serde_json::Value {
    let members: Vec<serde_json::Value> = cluster
        .members
        .iter()
        .map(|member| {
            let mut object = serde_json::json!({"id": member.id, "role": member.role, "address": member.address, "reachable": member.shown.is_ok()});
            match &member.shown {
                Ok(shown) => object["status"] = json(shown),
                Err(e) => object["error"] = e.clone().into(),
            }
            object
        })
        .collect();
    let total = total(cluster);
    serde_json::json!({
        "cluster": true,
        "members_from": match cluster.from { Source::Cluster => "cluster", Source::Configuration => "configuration" },
        "interval_seconds": cluster.interval,
        "nodes": members,
        "total": {
            "auth_per_second": total.figures.auth,
            "acct_per_second": total.figures.acct,
            "discarded_per_second": total.figures.discarded,
            "in_progress": json_number(total.figures.in_progress),
            "reachable": total.reachable,
            "nodes": total.members,
        },
    })
}

/// The members as the cluster has them, and where to sample each.
struct Known {
    members: Members,
    leader: Option<u64>,
    from: Source,
    /// Each member's id, metrics address as shown, and the socket address it resolved to.
    targets: Vec<(u64, String, Result<SocketAddr, String>)>,
    at: Instant,
}

/// Finds the members and resolves their metrics addresses. Fails only when the cluster refuses the
/// api secret.
async fn know_members(cluster: &ClusterSettings, api: &Api, local: SocketAddr) -> Result<Known, String> {
    let (members, leader, from) = current_members(api, &cluster.nodes).await?;
    let targets = futures::future::join_all(members.nodes.iter().map(|(id, member)| async move {
        if *id == cluster.node_id {
            return (*id, local.to_string(), Ok(local));
        }
        match metrics_address(&member.api, local.port()) {
            Ok(address) => {
                let resolved = resolve(&address).await;
                (*id, address, resolved)
            }
            Err(e) => (*id, member.api.clone(), Err(e)),
        }
    }))
    .await;
    Ok(Known { members, leader, from, targets, at: Instant::now() })
}

/// One sample of every member, together, in the order of `targets`.
async fn sample_members(targets: &[(u64, String, Result<SocketAddr, String>)]) -> Vec<Result<Sample, String>> {
    futures::future::join_all(targets.iter().map(|(_, _, resolved)| async move {
        match resolved {
            Ok(address) => sample(*address).await.map_err(|e| e.message(*address)),
            Err(e) => Err(e.clone()),
        }
    }))
    .await
}

/// What a member answering for the first time shows until it has been sampled twice.
const FIRST_SAMPLE: &str = "answered for the first time; its rates follow at the next sample";

/// The cluster view of each member's previous sample in `before` and its new one in `after`.
fn cluster_view(known: &Known, before: &BTreeMap<u64, Sample>, after: &[Result<Sample, String>], extra: &[StatusSection], now_unix: f64) -> ClusterView {
    let members: Vec<MemberView> = known
        .targets
        .iter()
        .zip(after)
        .map(|((id, address, resolved), after)| {
            let shown = match (resolved, after, before.get(id)) {
                (Err(e), _, _) | (Ok(_), Err(e), _) => Err(e.clone()),
                (Ok(socket), Ok(after), Some(before)) => Ok(view(*socket, before, after, extra, now_unix)),
                (Ok(_), Ok(_), None) => Err(FIRST_SAMPLE.to_string()),
            };
            MemberView { id: *id, role: role(*id, &known.members, known.leader, shown.as_ref().ok()), address: address.clone(), shown }
        })
        .collect();
    let interval = members.iter().find_map(|member| member.shown.as_ref().ok().map(|view| view.interval)).unwrap_or(INTERVAL.as_secs_f64());
    ClusterView { from: known.from, interval, members }
}

/// Keeps each member's newest sample; a member that failed keeps its older one, so that its next
/// rates are over the whole time since then.
fn keep_samples(known: &Known, before: &mut BTreeMap<u64, Sample>, after: Vec<Result<Sample, String>>) {
    for ((id, _, _), after) in known.targets.iter().zip(after) {
        if let Ok(after) = after {
            before.insert(*id, after);
        }
    }
}

fn now_unix() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |since| since.as_secs_f64())
}

/// How the command prints.
#[derive(Debug, Clone, Copy)]
struct Options {
    watch: bool,
    json: bool,
    cluster: bool,
}

/// Prints one look, as JSON or as text, clearing the terminal before it when `clear`.
fn print_out(text: impl FnOnce() -> String, json: impl FnOnce() -> serde_json::Value, options: Options, clear: bool) {
    let mut out = std::io::stdout().lock();
    let written = if options.json {
        writeln!(out, "{}", json())
    } else if clear {
        write!(out, "\x1b[H\x1b[2J{}", text())
    } else if options.watch {
        writeln!(out, "{}", text())
    } else {
        write!(out, "{}", text())
    };
    let _ = written.and_then(|()| out.flush());
}

/// The `status` command; returns the exit status.
pub async fn run(args: &[String], extensions: &Extensions) -> i32 {
    crate::logging::init_for_command();
    let mut options = Options { watch: false, json: false, cluster: false };
    let mut config_path = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--watch" if !options.watch => options.watch = true,
            "--json" if !options.json => options.json = true,
            "--cluster" if !options.cluster => options.cluster = true,
            "--config" if config_path.is_none() => match rest.next() {
                Some(path) => config_path = Some(path.clone()),
                None => return usage(),
            },
            _ => return usage(),
        }
    }
    let explicit = config_path.is_some();
    let path = config_path.unwrap_or_else(|| crate::config::default_config_path().to_string());
    let config = match crate::config::load(Path::new(&path)) {
        Ok(config) => config,
        Err(e) if explicit => {
            eprintln!("{e}");
            return 2;
        }
        Err(e) => {
            eprintln!("{e}\n(the default configuration; give --config <path> for another)");
            return 2;
        }
    };
    let Some(bind) = config.metrics else {
        eprintln!("status needs [metrics]: the command reads this node's /metrics. Add to {path}:\n[metrics]\nbind = \"127.0.0.1\"\nport = 9812");
        return 2;
    };
    let extra = extensions.status_sections();
    let watching: Pin<Box<dyn Future<Output = i32> + '_>> = if options.cluster { Box::pin(watch_cluster(&config.cluster, target(bind), &extra, options)) } else { Box::pin(watch_one(target(bind), &extra, options)) };
    let (Ok(mut terminate), Ok(mut interrupt)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
        return watching.await;
    };
    tokio::select! {
        status = watching => status,
        _ = terminate.recv() => 0,
        _ = interrupt.recv() => 0,
    }
}

/// How long `--cluster --watch` uses the members it found before asking the cluster again.
const MEMBERS_EVERY: Duration = Duration::from_secs(10);

/// `status --cluster`: two samples of every member a second apart; with `--watch`, one more each
/// second, asking the cluster for its members again every `MEMBERS_EVERY`.
async fn watch_cluster(cluster: &ClusterSettings, local: SocketAddr, extra: &[StatusSection], options: Options) -> i32 {
    let api = match Api::new(cluster.secret_api.clone()) {
        Ok(api) => api,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let clear = options.watch && !options.json && std::io::stdout().is_terminal();
    let mut known: Option<Known> = None;
    let mut before: BTreeMap<u64, Sample> = BTreeMap::new();
    let mut next = Instant::now();
    loop {
        let stale = known.as_ref().is_none_or(|known| known.at.elapsed() >= MEMBERS_EVERY);
        if stale {
            match know_members(cluster, &api, local).await {
                Ok(found) => known = Some(found),
                Err(e) => {
                    eprintln!("{e}");
                    return 2;
                }
            }
        }
        let Some(known) = known.as_ref() else { return 2 };
        // Nothing to compare with yet: a first sample of everyone, a second later the next.
        if before.is_empty() {
            let first = sample_members(&known.targets).await;
            next = Instant::now() + INTERVAL;
            keep_samples(known, &mut before, first);
        }
        tokio::time::sleep_until(next.into()).await;
        next = Instant::now() + INTERVAL;
        let after = sample_members(&known.targets).await;
        let shown = cluster_view(known, &before, &after, extra, now_unix());
        print_out(|| cluster_text(&shown), || cluster_json(&shown), options, clear);
        if !options.watch {
            return if shown.members.iter().any(|member| member.shown.is_ok()) { 0 } else { 3 };
        }
        keep_samples(known, &mut before, after);
    }
}

/// `status` of this node: two samples a second apart, then one more each second with `--watch`.
async fn watch_one(address: SocketAddr, extra: &[StatusSection], options: Options) -> i32 {
    {
        let mut before = match sample(address).await {
            Ok(sample) => sample,
            Err(e) if !options.watch => {
                eprintln!("{}", e.message(address));
                return 3;
            }
            Err(e) => {
                eprintln!("{}", e.message(address));
                loop {
                    tokio::time::sleep(INTERVAL).await;
                    match sample(address).await {
                        Ok(sample) => break sample,
                        Err(e) => eprintln!("{}", e.message(address)),
                    }
                }
            }
        };
        let clear = options.watch && !options.json && std::io::stdout().is_terminal();
        // A failed sample leaves `before` as it is, so that the next rates are over the whole time
        // since it was had; only the time of the next try moves on.
        let mut next = before.at + INTERVAL;
        loop {
            tokio::time::sleep_until(next.into()).await;
            match sample(address).await {
                Ok(after) => {
                    next = after.at + INTERVAL;
                    let shown = view(address, &before, &after, extra, now_unix());
                    print_out(|| text(&shown), || json(&shown), options, clear);
                    if !options.watch {
                        return 0;
                    }
                    before = after;
                }
                Err(e) if !options.watch => {
                    eprintln!("{}", e.message(address));
                    return 3;
                }
                Err(e) => {
                    eprintln!("{}", e.message(address));
                    next = Instant::now() + INTERVAL;
                }
            }
        }
    }
}

fn usage() -> i32 {
    eprintln!("{}", crate::cli::named(USAGE));
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scrape(text: &str) -> Result<Scrape, String> {
        parse(text)
    }

    fn sample_of(text: &str, at: Instant) -> Result<Sample, String> {
        Ok(Sample { at, ready: Ready { status: 200, text: "ok".to_string() }, scrape: scrape(text)? })
    }

    fn address() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9100)
    }

    #[test]
    fn test_parse() -> Result<(), String> {
        let parsed = scrape("# HELP a b\n# TYPE a counter\na 3\nb{kind=\"auth\"} 1.5\nc{client=\"x\\\"y\\\\z\\nw\",code=\"ok\"} 7\n\n")?;
        assert_eq!(parsed.value("a", &[]), Some(3.0));
        assert_eq!(parsed.value("b", &[("kind", "auth")]), Some(1.5));
        assert_eq!(parsed.value("b", &[("kind", "acct")]), None, "the labels have to match");
        assert_eq!(parsed.value("b", &[]), Some(1.5), "without labels, the sum");
        assert_eq!(scrape("d{peer=\"a\"} 1\nd{peer=\"b\"} 2\n")?.value("d", &[]), Some(3.0));
        assert_eq!(parsed.value("missing", &[]), None);
        assert_eq!(parsed.value("c", &[("code", "ok"), ("client", "x\"y\\z\nw")]), Some(7.0));
        for bad in ["a", "a b", "a{kind=auth} 1", "a{kind=\"auth} 1", "1a 2", "a{kind=\"\\q\"} 1", "a{kind=\"x\"}1"] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        Ok(())
    }

    #[test]
    fn test_metrics_of_the_server_parse() -> Result<(), String> {
        let metrics = crate::metrics::Metrics::default().with_version("1.0");
        metrics.set_radius_clients(["a \"quoted\" name"]);
        let parsed = scrape(&metrics.render())?;
        assert_eq!(parsed.value("rradius_build_info", &[("version", "1.0")]), Some(1.0));
        assert_eq!(parsed.value("rradius_radius_client_responses_total", &[("client", "a \"quoted\" name"), ("code", "access_accept")]), Some(0.0));
        Ok(())
    }

    /// The rates table of the spec.
    #[test]
    fn test_rates() {
        for (first, second, seconds, shown, reset) in [(1000.0, 1812.0, 1.0, 812.0, false), (1000.0, 1000.0, 1.0, 0.0, false), (1000.0, 40.0, 1.0, 0.0, true), (1000.0, 1500.0, 2.0, 250.0, false)] {
            assert_eq!(rate(first, second, seconds), (shown, reset), "{first} {second} {seconds}");
        }
    }

    #[test]
    fn test_rate_and_reset_in_the_view() -> Result<(), String> {
        let start = Instant::now();
        let before = sample_of("rradius_radius_packets_total{kind=\"auth\"} 1000\n", start)?;
        let after = sample_of("rradius_radius_packets_total{kind=\"auth\"} 1812\n", start + Duration::from_secs(1))?;
        let shown = view(address(), &before, &after, &[], 0.0);
        let radius = shown.sections.iter().find(|section| section.key == "radius").ok_or("no RADIUS section")?;
        assert_eq!(radius.items, vec![ShownItem { key: "auth_requests".to_string(), label: "Auth requests".to_string(), reading: Reading::Rate { rate: 812.0, total: 1812.0 } }]);
        assert!(!shown.counter_reset);
        let restarted = sample_of("rradius_radius_packets_total{kind=\"auth\"} 40\n", start + Duration::from_secs(2))?;
        let shown = view(address(), &after, &restarted, &[], 0.0);
        assert!(shown.counter_reset);
        assert!(text(&shown).contains("counters reset"));
        Ok(())
    }

    /// The top clients example of the spec.
    #[test]
    fn test_busiest_clients() {
        let mut clients = vec![("wlc".to_string(), 400.0, 0.0), ("vpn".to_string(), 0.0, 0.0), ("ap-1".to_string(), 120.0, 0.0), ("ap-2".to_string(), 120.0, 0.0)];
        for name in ["c5", "c3", "c1", "c4", "c2"] {
            clients.push((name.to_string(), 10.0, 0.0));
        }
        let names: Vec<String> = top_clients(clients).into_iter().map(|(name, _, _)| name).collect();
        assert_eq!(names, ["wlc", "ap-1", "ap-2", "c1", "c2"]);
    }

    #[test]
    fn test_clients_section_from_samples() -> Result<(), String> {
        let start = Instant::now();
        let line = |client: &str, code: &str, value: u64| format!("rradius_radius_client_responses_total{{client=\"{client}\",code=\"{code}\"}} {value}\n");
        let before = sample_of(&(line("wlc", "access_accept", 0) + &line("wlc", "access_reject", 0) + &line("idle", "access_accept", 5)), start)?;
        let after = sample_of(&(line("wlc", "access_accept", 300) + &line("wlc", "access_reject", 100) + &line("idle", "access_accept", 5)), start + Duration::from_secs(1))?;
        let shown = view(address(), &before, &after, &[], 0.0);
        let clients = shown.sections.iter().find(|section| section.key == "radius_top_clients").ok_or("no clients section")?;
        assert_eq!(clients.items, vec![ShownItem { key: "wlc".to_string(), label: "wlc".to_string(), reading: Reading::Rate { rate: 400.0, total: 400.0 } }]);
        Ok(())
    }

    #[test]
    fn test_missing_metrics_left_out() -> Result<(), String> {
        let start = Instant::now();
        let text_of = "rradius_tacacs_connections_open 2\nrradius_netlogon_open_connections 1\n";
        let before = sample_of(text_of, start)?;
        let after = sample_of(text_of, start + Duration::from_secs(1))?;
        let keys: Vec<String> = view(address(), &before, &after, &[], 0.0).sections.into_iter().map(|section| section.key).collect();
        assert_eq!(keys, ["node", "tacacs", "netlogon"]);
        let without_netlogon = sample_of("rradius_tacacs_connections_open 2\n", start)?;
        let keys: Vec<String> = view(address(), &without_netlogon, &without_netlogon, &[], 0.0).sections.into_iter().map(|section| section.key).collect();
        assert_eq!(keys, ["node", "tacacs"], "no Netlogon section without its metrics");
        Ok(())
    }

    /// The one-section example of the spec, and that the text and the JSON have the same sections
    /// and items in the same order.
    #[test]
    fn test_same_content() -> Result<(), String> {
        let start = Instant::now();
        let before = sample_of("rradius_tacacs_connections_open 2\nrradius_tacacs_requests_total{type=\"authen\"} 10\n", start)?;
        let after = sample_of("rradius_tacacs_connections_open 2\nrradius_tacacs_requests_total{type=\"authen\"} 15\nrradius_radius_packets_total{kind=\"auth\"} 3\n", start + Duration::from_secs(1))?;
        let extra = [StatusSection { title: "Extra", key: "extra", items: vec![item("Things", "things", "rradius_radius_packets_total", &[("kind", "auth")], Show::Total)] }];
        let shown = view(address(), &before, &after, &extra, 0.0);
        let written = text(&shown);
        let lines: Vec<String> = written.lines().map(|line| line.split_whitespace().collect::<Vec<_>>().join(" ")).collect();
        assert!(lines.contains(&"Open connections 2".to_string()), "{written}");
        assert!(lines.contains(&"Authentication 5.0/s (15)".to_string()), "{written}");
        let json = json(&shown);
        let sections = json["sections"].as_array().ok_or("no sections")?;
        let tacacs = sections.iter().find(|section| section["key"] == "tacacs").ok_or("no tacacs")?;
        assert_eq!(tacacs["title"], "TACACS+");
        assert_eq!(tacacs["items"][0], serde_json::json!({"key": "connections_open", "label": "Open connections", "value": 2}));
        assert_eq!(tacacs["items"][1], serde_json::json!({"key": "authen", "label": "Authentication", "rate": 5.0, "total": 15}));
        // Every title and label of the text, in order, and nothing else.
        let mut from_json = Vec::new();
        for section in sections {
            from_json.push(section["title"].as_str().ok_or("title")?.to_string());
            for item in section["items"].as_array().ok_or("items")? {
                from_json.push(item["label"].as_str().ok_or("label")?.to_string());
            }
        }
        let from_text: Vec<String> = written.lines().skip(1).filter(|line| !line.is_empty()).map(|line| match line.strip_prefix("  ") {
            Some(item) => item.split("  ").next().unwrap_or_default().trim().to_string(),
            None => line.to_string(),
        }).collect();
        assert_eq!(from_text, from_json);
        assert_eq!(sections.last().map(|section| section["key"].clone()), Some(serde_json::json!("extra")), "the extensions' sections come last");
        Ok(())
    }

    #[test]
    fn test_node_section() -> Result<(), String> {
        let start = Instant::now();
        let text_of = "rradius_cluster_is_leader 1\nrradius_cluster_leader 1\nrradius_build_info{version=\"0.1.0\"} 1\nrradius_start_time_seconds 1000\n";
        let mut after = sample_of(text_of, start)?;
        after.ready = Ready { status: 503, text: "stopping".to_string() };
        let shown = view(address(), &after, &after, &[], 1060.5);
        let node = &shown.sections[0];
        let reading = |key: &str| node.items.iter().find(|item| item.key == key).map(|item| item.reading.clone());
        assert_eq!(reading("ready"), Some(Reading::Value(Value::Text("stopping".to_string()))));
        assert_eq!(reading("is_leader"), Some(Reading::Value(Value::Number(1.0))));
        assert_eq!(reading("version"), Some(Reading::Value(Value::Text("0.1.0".to_string()))));
        assert_eq!(reading("seconds_running"), Some(Reading::Value(Value::Number(60.0))));
        assert_eq!(json(&shown)["ready"], serde_json::json!({"status": 503, "text": "stopping"}));
        Ok(())
    }

    /// After failed samples in `--watch`, the rates are over the whole time since the last one had.
    #[test]
    fn test_rates_over_a_gap() -> Result<(), String> {
        let then = Instant::now();
        let before = sample_of("rradius_radius_packets_total{kind=\"auth\"} 100\n", then)?;
        let after = sample_of("rradius_radius_packets_total{kind=\"auth\"} 160\n", then + Duration::from_secs(6))?;
        let shown = json(&view(address(), &before, &after, &[], 0.0));
        assert_eq!(shown["interval_seconds"], serde_json::json!(6.0));
        let radius = shown["sections"].as_array().and_then(|sections| sections.iter().find(|section| section["key"] == "radius")).ok_or("no RADIUS section")?;
        assert_eq!(radius["items"][0], serde_json::json!({"key": "auth_requests", "label": "Auth requests", "rate": 10.0, "total": 160}));
        Ok(())
    }

    #[test]
    fn test_target() {
        let port = 9100;
        assert_eq!(target(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)), SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
        assert_eq!(target(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port)), SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port));
        let given = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), port);
        assert_eq!(target(given), given);
    }

    /// Cumulative buckets of the response times as `/metrics` has them, for `counts` per bucket
    /// from the one up to 0.5 ms on; the three below it are empty.
    fn bucket_lines(metric: &str, labels: &str, counts: &[u64; 15]) -> String {
        let mut text = String::new();
        let mut cumulative = 0;
        for (index, count) in [0, 0, 0].iter().chain(counts).enumerate() {
            cumulative += count;
            let bound = crate::metrics::RESPONSE_BUCKETS.get(index).map_or_else(|| "+Inf".to_string(), f64::to_string);
            text.push_str(&format!("{metric}_bucket{{{labels},le=\"{bound}\"}} {cumulative}\n"));
        }
        text
    }

    fn radius_times(counts: &[u64; 15]) -> String {
        bucket_lines("rradius_radius_response_seconds", "kind=\"auth\"", counts)
    }

    fn quantile_of(before: &str, after: &str) -> Result<(Option<Reading>, Option<Reading>, bool), String> {
        let start = Instant::now();
        let section = StatusSection {
            title: "T",
            key: "t",
            items: vec![
                item("p50", "auth_p50_ms", "rradius_radius_response_seconds", &[("kind", "auth")], Show::Quantile(0.5)),
                item("p99", "auth_p99_ms", "rradius_radius_response_seconds", &[("kind", "auth")], Show::Quantile(0.99)),
            ],
        };
        let shown = view(address(), &sample_of(before, start)?, &sample_of(after, start + Duration::from_secs(1))?, &[section], 0.0);
        let found = |key: &str| shown.sections.iter().find(|section| section.key == "t").and_then(|section| section.items.iter().find(|item| item.key == key)).map(|item| item.reading.clone());
        Ok((found("auth_p50_ms"), found("auth_p99_ms"), shown.counter_reset))
    }

    /// The two percentile examples of the spec.
    #[test]
    fn test_quantiles() -> Result<(), String> {
        let before = radius_times(&[5, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0]);
        let after = radius_times(&[65, 31, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0]);
        let number = |value: f64| Some(Reading::Value(Value::Number(value)));
        assert_eq!(quantile_of(&before, &after)?, (number(0.458), number(2.5), false));
        let past = radius_times(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(quantile_of(&radius_times(&[0; 15]), &past)?, (number(10000.0), number(10000.0), false));
        Ok(())
    }

    #[test]
    fn test_quantile_without_requests_left_out() -> Result<(), String> {
        let same = radius_times(&[3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(quantile_of(&same, &same)?, (None, None, false));
        assert_eq!(quantile_of("", "")?, (None, None, false), "no histogram");
        // A count that went down: the server restarted.
        assert_eq!(quantile_of(&same, &radius_times(&[0; 15]))?, (None, None, true));
        Ok(())
    }

    #[test]
    fn test_quantile_incomplete_buckets_left_out() -> Result<(), String> {
        let before = radius_times(&[0; 15]);
        let full = radius_times(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let without_inf: String = full.lines().filter(|line| !line.contains("+Inf")).map(|line| format!("{line}\n")).collect();
        assert_eq!(quantile_of(&before, &without_inf)?, (None, None, false));
        // An `le` that is not a number is passed over; the rest still makes the quantile.
        let odd = format!("{full}rradius_radius_response_seconds_bucket{{kind=\"auth\",le=\"soon\"}} 7\n");
        let odd_before = format!("{before}rradius_radius_response_seconds_bucket{{kind=\"auth\",le=\"soon\"}} 0\n");
        assert_eq!(quantile_of(&odd_before, &odd)?.0, Some(Reading::Value(Value::Number(0.375))));
        Ok(())
    }

    #[test]
    fn test_quantile_interpolation() {
        // Everything counted in the first bucket: the median halfway inside it, the maximum at its bound.
        let before = [(0.0005, 0.0), (0.001, 0.0), (f64::INFINITY, 0.0)];
        let after = [(0.0005, 4.0), (0.001, 4.0), (f64::INFINITY, 4.0)];
        assert_eq!(quantile(&before, &after, 0.5), Quantile::Seconds(0.00025));
        assert_eq!(quantile(&before, &after, 1.0), Quantile::Seconds(0.0005));
        assert_eq!(quantile(&before[..2], &after, 0.5), Quantile::None, "different buckets");
    }

    /// The member addresses table of the spec.
    #[test]
    fn test_metrics_address() -> Result<(), String> {
        assert_eq!(metrics_address("10.0.0.2:8200", 9812)?, "10.0.0.2:9812");
        assert_eq!(metrics_address("[fd00::2]:8200", 9812)?, "[fd00::2]:9812");
        assert_eq!(metrics_address("node2.internal:8200", 9100)?, "node2.internal:9100");
        assert!(metrics_address("node2", 9812).is_err());
        assert!(metrics_address(":8200", 9812).is_err());
        Ok(())
    }

    /// A view with the RADIUS readings the cluster lines show, and the node's leading.
    fn radius_view(auth: f64, acct: f64, in_progress: (f64, f64), leads: bool) -> View {
        let rate = |key: &str, rate: f64| ShownItem { key: key.to_string(), label: key.to_string(), reading: Reading::Rate { rate, total: rate } };
        let value = |key: &str, value: f64| ShownItem { key: key.to_string(), label: key.to_string(), reading: Reading::Value(Value::Number(value)) };
        let node = ShownSection { key: "node".to_string(), title: "Node".to_string(), items: vec![value("is_leader", if leads { 1.0 } else { 0.0 })] };
        let radius = ShownSection {
            key: "radius".to_string(),
            title: "RADIUS".to_string(),
            items: vec![rate("auth_requests", auth), rate("acct_requests", acct), rate("discarded_overload", 0.0), value("auth_in_progress", in_progress.0), value("acct_in_progress", in_progress.1), value("auth_limit", 4096.0)],
        };
        View { address: address(), interval: 1.0, ready: Ready { status: 200, text: "ok".to_string() }, counter_reset: false, sections: vec![node, radius] }
    }

    /// The total example of the spec: two of three members reachable.
    fn two_of_three() -> ClusterView {
        let members = vec![
            MemberView { id: 1, role: "leader", address: "10.0.0.1:9812".to_string(), shown: Ok(radius_view(100.0, 10.0, (2.0, 1.0), true)) },
            MemberView { id: 2, role: "follower", address: "10.0.0.2:9812".to_string(), shown: Ok(radius_view(50.0, 5.0, (1.0, 0.0), false)) },
            MemberView { id: 3, role: "follower", address: "10.0.0.3:9812".to_string(), shown: Err("cannot reach the metrics port at 10.0.0.3:9812: refused".to_string()) },
        ];
        ClusterView { from: Source::Cluster, interval: 1.0, members }
    }

    #[test]
    fn test_cluster_total() {
        let total = total(&two_of_three());
        assert_eq!(total, Total { figures: Figures { auth: 150.0, acct: 15.0, discarded: 0.0, in_progress: 4.0 }, reachable: 2, members: 3 });
    }

    #[test]
    fn test_role() {
        let members = Members { voters: [1, 2].into_iter().collect(), nodes: BTreeMap::new() };
        let leading = radius_view(0.0, 0.0, (0.0, 0.0), true);
        let following = radius_view(0.0, 0.0, (0.0, 0.0), false);
        assert_eq!(role(2, &members, Some(1), Some(&leading)), "leader", "its own metrics count first");
        assert_eq!(role(1, &members, Some(1), Some(&following)), "follower");
        assert_eq!(role(1, &members, Some(1), None), "leader", "unreachable: the leader the cluster sees");
        assert_eq!(role(2, &members, Some(1), None), "follower");
        assert_eq!(role(5, &members, Some(1), None), "learner");
    }

    /// A member's rates are over the time since its last sample that worked; one with none yet
    /// waits for the next.
    #[test]
    fn test_cluster_view_keeps_samples() -> Result<(), String> {
        let start = Instant::now();
        let socket = |last: u8| Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, last)), 9812));
        let known = Known {
            members: Members { voters: [1, 2].into_iter().collect(), nodes: BTreeMap::new() },
            leader: Some(1),
            from: Source::Cluster,
            targets: vec![(1, "10.0.0.1:9812".to_string(), socket(1)), (2, "10.0.0.2:9812".to_string(), socket(2))],
            at: start,
        };
        let packets = |count: u64, at: Instant| sample_of(&format!("rradius_radius_packets_total{{kind=\"auth\"}} {count}\n"), at);
        let mut before = BTreeMap::new();
        keep_samples(&known, &mut before, vec![packets(100, start), Err("down".to_string())]);
        // Node 1 failed once in between: its rates are over three seconds.
        keep_samples(&known, &mut before, vec![Err("down".to_string()), Err("down".to_string())]);
        let after = vec![packets(130, start + Duration::from_secs(3)), packets(7, start + Duration::from_secs(3))];
        let shown = cluster_view(&known, &before, &after, &[], 0.0);
        let first = shown.members[0].shown.as_ref().map_err(String::clone)?;
        assert_eq!(figures(first).auth, 10.0);
        assert_eq!(shown.members[1].shown, Err(FIRST_SAMPLE.to_string()));
        keep_samples(&known, &mut before, after);
        let after = vec![packets(140, start + Duration::from_secs(4)), packets(12, start + Duration::from_secs(4))];
        let shown = cluster_view(&known, &before, &after, &[], 0.0);
        let second = shown.members[1].shown.as_ref().map_err(String::clone)?;
        assert_eq!(figures(second).auth, 5.0);
        Ok(())
    }

    /// The text and the JSON of a cluster view show the same members and numbers.
    #[test]
    fn test_cluster_text_and_json_agree() -> Result<(), String> {
        let cluster = two_of_three();
        let written = cluster_text(&cluster);
        let shown = cluster_json(&cluster);
        let rows: Vec<Vec<&str>> = written.lines().map(|line| line.split_whitespace().collect()).collect();
        let row = |first: &str| rows.iter().find(|row| row.first() == Some(&first)).cloned().ok_or(format!("no {first} row: {written}"));
        assert_eq!(row("1")?, ["1", "leader", "ok", "100.0", "10.0", "0.0", "3", "10.0.0.1:9812"]);
        assert_eq!(row("2")?, ["2", "follower", "ok", "50.0", "5.0", "0.0", "1", "10.0.0.2:9812"]);
        assert_eq!(row("3")?, ["3", "follower", "unreachable", "-", "-", "-", "-", "10.0.0.3:9812"]);
        assert_eq!(row("total")?, ["total", "2/3", "150.0", "15.0", "0.0", "4"]);
        assert!(written.contains("node 3: cannot reach") && written.contains("[metrics] bind"), "{written}");
        assert_eq!(shown["cluster"], true);
        assert_eq!(shown["members_from"], "cluster");
        assert_eq!(shown["total"], serde_json::json!({"auth_per_second": 150.0, "acct_per_second": 15.0, "discarded_per_second": 0.0, "in_progress": 4, "reachable": 2, "nodes": 3}));
        let nodes = shown["nodes"].as_array().ok_or("no nodes")?;
        for (node, id) in nodes.iter().zip(["1", "2", "3"]) {
            let row = row(id)?;
            assert_eq!(node["id"].to_string(), row[0]);
            assert_eq!(node["role"], row[1]);
            assert_eq!(node["address"], row[7]);
        }
        assert_eq!(nodes[0]["status"], json(&radius_view(100.0, 10.0, (2.0, 1.0), true)), "a member's status is what status --json prints");
        assert_eq!(nodes[2]["reachable"], false);
        assert!(nodes[2]["error"].as_str().is_some_and(|e| e.contains("cannot reach")) && nodes[2].get("status").is_none());
        let from_configuration = cluster_json(&ClusterView { from: Source::Configuration, ..cluster });
        assert_eq!(from_configuration["members_from"], "configuration");
        Ok(())
    }

    /// Serves `answer` once to whoever connects, after reading the request head.
    async fn answer_once(answer: Vec<u8>) -> Result<SocketAddr, String> {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.map_err(|e| e.to_string())?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut head = [0u8; 1024];
                let _ = socket.read(&mut head).await;
                let _ = socket.write_all(&answer).await;
                let _ = socket.shutdown().await;
            }
        });
        Ok(address)
    }

    #[tokio::test]
    async fn test_get() -> Result<(), String> {
        let address = answer_once(b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\nstopping\n".to_vec()).await?;
        assert_eq!(get(address, "/readyz").await, Ok((503, "stopping\n".to_string())));
        let address = answer_once(b"SSH-2.0-OpenSSH\r\n".to_vec()).await?;
        assert!(matches!(get(address, "/metrics").await, Err(FetchError::Format(_))));
        let mut long = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        long.resize(MAX_ANSWER + 10, b'a');
        let address = answer_once(long).await?;
        assert!(matches!(get(address, "/metrics").await, Err(FetchError::Format(e)) if e.contains("longer")));
        Ok(())
    }

    #[tokio::test]
    async fn test_get_with_nothing_there() -> Result<(), String> {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.map_err(|e| e.to_string())?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;
        drop(listener);
        assert!(matches!(get(address, "/metrics").await, Err(FetchError::Unreachable(_))));
        // Something that takes the connection and never answers.
        let silent = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.map_err(|e| e.to_string())?;
        let address = silent.local_addr().map_err(|e| e.to_string())?;
        let started = Instant::now();
        assert!(matches!(get(address, "/metrics").await, Err(FetchError::Unreachable(e)) if e.contains("no answer")));
        assert!(started.elapsed() < Duration::from_secs(4));
        drop(silent);
        Ok(())
    }
}
