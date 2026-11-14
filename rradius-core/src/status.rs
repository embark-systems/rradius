//! The `status` command: what this node is doing now, read from its own `/readyz` and `/metrics`,
//! as rates over the time between two samples and current values, as text or JSON.

use crate::extension::{Extensions, Show, StatusItem, StatusSection};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{IsTerminal, Write as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::signal::unix::{signal, SignalKind};

pub const USAGE: &str = "usage: rradius-server status [--config <path>] [--watch] [--json]";

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

/// Makes a section of what two samples have for its items; `None` when it would be empty.
fn show_section(section: &StatusSection, before: &Scrape, after: &Scrape, seconds: f64, reset: &mut bool) -> Option<ShownSection> {
    let items: Vec<ShownItem> = section
        .items
        .iter()
        .filter_map(|item| {
            let now = after.value(item.metric, item.labels)?;
            let reading = match item.show {
                Show::Value | Show::Total => Reading::Value(Value::Number(now)),
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

fn now_unix() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |since| since.as_secs_f64())
}

/// How the command prints.
#[derive(Debug, Clone, Copy)]
struct Options {
    watch: bool,
    json: bool,
}

fn print_view(view: &View, options: Options, clear: bool) {
    let mut out = std::io::stdout().lock();
    let written = if options.json {
        writeln!(out, "{}", json(view))
    } else if clear {
        write!(out, "\x1b[H\x1b[2J{}", text(view))
    } else if options.watch {
        writeln!(out, "{}", text(view))
    } else {
        write!(out, "{}", text(view))
    };
    let _ = written.and_then(|()| out.flush());
}

/// The `status` command; returns the exit status.
pub async fn run(args: &[String], extensions: &Extensions) -> i32 {
    crate::logging::init_for_command();
    let mut options = Options { watch: false, json: false };
    let mut config_path = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--watch" if !options.watch => options.watch = true,
            "--json" if !options.json => options.json = true,
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
    let address = target(bind);
    let extra = extensions.status_sections();
    let watching = async {
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
                    print_view(&view(address, &before, &after, &extra, now_unix()), options, clear);
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
    };
    let (Ok(mut terminate), Ok(mut interrupt)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
        return watching.await;
    };
    tokio::select! {
        status = watching => status,
        _ = terminate.recv() => 0,
        _ = interrupt.recv() => 0,
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
