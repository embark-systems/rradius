//! Turns the command line into workers, and their results into a report.
use crate::cli::{parse_attr, share, Kind, LoadSpec, RunArgs};
use crate::eap::{EapOptions, EapSetup, Method};
use crate::packet::{RequestOptions, Signer, Step, Template};
use crate::report::{self, LoadReport, Report, SendLag, StepReport, Warning};
use crate::server_metrics::MetricsUrl;
use crate::radsec::{TlsLink, TlsTarget};
use crate::tacacs::{Connect, TacacsLink, TacacsTarget};
use crate::worker::{open_socket, prepare_worker, EapPlan, Link, LoadPlan, PreparedWorker, StepPlan, StepStats, TacacsPlan, Targets, WorkerInput, WorkerOutput};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Barrier};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_WARMUP: f64 = 2.0;
const DEFAULT_DURATION: f64 = 10.0;

/// What every load of a run shares.
pub struct RunParams {
    pub signer: Signer,
    pub options: RequestOptions,
    pub user: String,
    pub password: String,
    pub users: Option<u32>,
    pub bind: Option<IpAddr>,
    pub warmup: Duration,
    pub duration: Option<Duration>,
    pub requests: Option<u64>,
    pub timeout: Duration,
    pub retries: u32,
    pub eap: EapOptions,
    /// With `--transport tls`: where the RadSec connections go.
    pub radsec: Option<Arc<TlsTarget>>,
    /// With TACACS+ loads: where their connections go.
    pub tacacs: Option<Arc<TacacsTarget>>,
}

/// The merged results of one load.
pub struct LoadOutcome {
    pub spec: LoadSpec,
    pub steps: Vec<(Step, StepStats)>,
    pub send_lag: hdrhistogram::Histogram<u64>,
    pub busy_max: f64,
    pub window: Duration,
    pub reconnects: u64,
}

fn seconds(name: &str, value: f64) -> Result<Duration, String> {
    Duration::try_from_secs_f64(value).map_err(|_| format!("--{name} {value}: not a time in seconds"))
}

/// The steps of a load, in order.
fn steps_of(spec: &LoadSpec) -> Vec<Step> {
    match spec.kind {
        Kind::Single(step) => vec![step],
        Kind::Session if spec.alive > 0 => vec![Step::Pap, Step::AcctStart, Step::AcctAlive, Step::AcctStop],
        Kind::Session => vec![Step::Pap, Step::AcctStart, Step::AcctStop],
    }
}

fn user_names(user: &str, users: Option<u32>) -> Vec<String> {
    match users {
        Some(count) => (0..count).map(|index| format!("{user}{index}")).collect(),
        None => vec![user.to_string()],
    }
}

fn plan(spec: &LoadSpec, params: &RunParams) -> Result<Arc<LoadPlan>, String> {
    let names = user_names(&params.user, params.users);
    if let Kind::Single(step) = spec.kind {
        if step.is_tacacs() {
            let tacacs = Some(TacacsPlan { users: names, password: params.password.clone() });
            return Ok(Arc::new(LoadPlan { spec: spec.clone(), steps: vec![StepPlan { step, templates: Vec::new() }], eap: None, tacacs }));
        }
    }
    let steps = steps_of(spec)
        .into_iter()
        .map(|step| {
            let templates = names.iter().map(|name| Template::new(step, name, &params.password, &params.options)).collect::<Result<Vec<_>, _>>()?;
            Ok(StepPlan { step, templates })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let eap = match steps.first().and_then(|step| Method::of(step.step)) {
        Some(Method::AkaPrime) => Some(EapPlan { setup: EapSetup::aka(&params.password)?, users: names, password: params.password.clone() }),
        Some(method) => Some(EapPlan { setup: EapSetup::new(method, &params.eap)?, users: names, password: params.password.clone() }),
        None => None,
    };
    Ok(Arc::new(LoadPlan { spec: spec.clone(), steps, eap, tacacs: None }))
}

/// Runs loads at once. `targets_of` gives where a load's thread sends. `at_measure_start`
/// runs on this thread as the measured window opens.
pub fn run_loads(specs: &[LoadSpec], params: &RunParams, targets_of: impl Fn(usize, u32) -> Targets, at_measure_start: impl FnOnce()) -> Result<Vec<LoadOutcome>, String> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos() as u64);
    // Everything that can fail to set up does so before the first thread starts.
    let mut prepared: Vec<Vec<PreparedWorker>> = Vec::new();
    for (load_index, spec) in specs.iter().enumerate() {
        let load_plan = plan(spec, params)?;
        let (threads, sockets) = (spec.threads(), spec.sockets());
        // RadSec carries both kinds on one connection.
        let connected = spec.kind != Kind::Session || params.radsec.is_some();
        let mut workers = Vec::new();
        let mut socket_index = 0;
        for thread in 0..threads {
            let targets = match &params.radsec {
                Some(target) => Targets { auth: target.address, acct: target.address },
                None => targets_of(load_index, thread),
            };
            let port_target = if load_plan.steps[0].step.uses_acct_port() { targets.acct } else { targets.auth };
            let mut owned = Vec::new();
            for _ in 0..share(u64::from(sockets), threads, thread) {
                let socket = match (&params.radsec, &load_plan.tacacs, &params.tacacs) {
                    (_, Some(_), None) => return Err("TACACS+ load without a TACACS+ target".to_string()),
                    // Under `keep`, every connection is open before anything is sent.
                    (_, Some(_), Some(target)) => Link::Tacacs(Box::new(match target.connect {
                        Connect::Keep => TacacsLink::connect(target).map_err(|e| format!("TACACS+ connection {} of load {}: {e}", socket_index + 1, spec.kind.name()))?,
                        Connect::PerRequest => TacacsLink::idle(),
                    })),
                    // Every connection is through its handshake before anything is sent.
                    (Some(target), None, _) => Link::Tls(Box::new(TlsLink::connect(target).map_err(|e| format!("RadSec connection {} of load {}: {e}", socket_index + 1, spec.kind.name()))?)),
                    (None, None, _) => Link::Udp(open_socket(params.bind, connected.then_some(port_target), port_target).map_err(|e| format!("socket: {e}"))?),
                };
                owned.push((socket, share(u64::from(spec.concurrency), sockets, socket_index) as u16));
                socket_index += 1;
            }
            workers.push(prepare_worker(WorkerInput {
                plan: load_plan.clone(),
                targets,
                signer: params.signer.clone(),
                sockets: owned,
                tls: params.radsec.clone(),
                tacacs: load_plan.tacacs.as_ref().and(params.tacacs.clone()),
                connected,
                rate: spec.rate.map(|rate| rate / f64::from(threads)),
                budget: params.requests.map(|requests| share(requests, threads, thread)),
                warmup: params.warmup,
                duration: params.duration,
                timeout: params.timeout,
                // Nothing is sent again over TCP.
                retries: if load_plan.tacacs.is_some() { 0 } else { params.retries },
                thread_index: thread,
                session_prefix: ((nonce & 0xffff) << 48) | ((load_index as u64 & 0xff) << 40) | (u64::from(thread) & 0xff) << 32,
                rng_seed: nonce ^ ((load_index as u64) << 32) ^ u64::from(thread),
            })?);
        }
        prepared.push(workers);
    }

    let count: usize = prepared.iter().map(Vec::len).sum();
    let barrier = Arc::new(Barrier::new(count + 1));
    let handles: Vec<Vec<_>> = prepared
        .into_iter()
        .map(|workers| {
            workers
                .into_iter()
                .map(|worker| {
                    let barrier = barrier.clone();
                    std::thread::spawn(move || worker.run(barrier))
                })
                .collect()
        })
        .collect();
    if params.warmup.is_zero() {
        at_measure_start();
        barrier.wait();
    } else {
        barrier.wait();
        std::thread::sleep(params.warmup);
        at_measure_start();
    }

    let mut outcomes = Vec::new();
    for (spec, handles) in specs.iter().zip(handles) {
        let mut outputs: Vec<WorkerOutput> = Vec::new();
        for handle in handles {
            outputs.push(handle.join().map_err(|_| "a worker failed".to_string())??);
        }
        let steps = steps_of(spec);
        let mut merged: Vec<(Step, StepStats)> = steps.iter().map(|step| Ok((*step, StepStats::new()?))).collect::<Result<_, String>>()?;
        let mut send_lag = hdrhistogram::Histogram::new_with_bounds(1, 3_600_000_000_000, 3).map_err(|e| format!("histogram: {e}"))?;
        let (mut busy_max, mut window, mut reconnects) = (0.0f64, Duration::ZERO, 0);
        for output in &outputs {
            for ((_, into), from) in merged.iter_mut().zip(&output.steps) {
                into.merge(from)?;
            }
            send_lag.add(&output.send_lag).map_err(|e| format!("histogram: {e}"))?;
            busy_max = busy_max.max(output.busy);
            window = window.max(output.window);
            reconnects += output.reconnects;
        }
        outcomes.push(LoadOutcome { spec: spec.clone(), steps: merged, send_lag, busy_max, window, reconnects });
    }
    Ok(outcomes)
}

/// `run`: the arguments are checked first, so a bad one sends nothing.
pub fn execute(args: &RunArgs) -> Result<Report, String> {
    let specs = args.loads.iter().map(|text| LoadSpec::parse(text)).collect::<Result<Vec<_>, _>>()?;
    let extra = args.attrs.iter().map(|text| parse_attr(text)).collect::<Result<Vec<_>, _>>()?;
    if args.users == Some(0) {
        return Err("--users must be above 0".to_string());
    }
    if args.requests == Some(0) {
        return Err("--requests must be above 0".to_string());
    }
    let timeout = seconds("timeout", args.timeout)?;
    if timeout.is_zero() {
        return Err("--timeout must be above 0".to_string());
    }
    let warmup = match args.requests {
        Some(_) => Duration::ZERO,
        None => seconds("warmup", args.warmup.unwrap_or(DEFAULT_WARMUP))?,
    };
    let duration = match args.requests {
        Some(_) => None,
        None => {
            let duration = seconds("duration", args.duration.unwrap_or(DEFAULT_DURATION))?;
            if duration.is_zero() {
                return Err("--duration must be above 0".to_string());
            }
            Some(duration)
        }
    };
    let radsec = match args.transport.as_str() {
        "udp" => false,
        "tls" => true,
        other => return Err(format!("--transport {other}: udp or tls")),
    };
    if radsec {
        if args.cert_file.is_none() || args.key_file.is_none() {
            return Err("--transport tls needs --cert-file and --key-file: RadSec servers ask for a client certificate".to_string());
        }
        if args.retries > 0 {
            return Err("--retries: nothing is sent again over TLS (RFC 6614), so --transport tls takes 0".to_string());
        }
        if args.tls_port == 0 {
            return Err("--tls-port 0: not a port".to_string());
        }
    }
    let tacacs_connect = Connect::parse(&args.tacacs_connect)?;
    if args.tacacs_port == 0 {
        return Err("--tacacs-port 0: not a port".to_string());
    }
    let tacacs_loads = specs.iter().any(|spec| matches!(spec.kind, Kind::Single(step) if step.is_tacacs()));
    if tacacs_loads && radsec {
        return Err("--transport tls: TACACS+ loads go over TCP, not RadSec".to_string());
    }
    let eap_loads = specs.iter().any(|spec| matches!(spec.kind, Kind::Single(step) if step.is_eap()));
    if eap_loads && args.no_message_authenticator {
        return Err("--no-message-authenticator: EAP loads always carry Message-Authenticator (RFC 3579)".to_string());
    }
    let metrics_url = args.metrics_url.as_deref().map(MetricsUrl::parse).transpose()?;
    let address = resolve(&args.server, args.auth_port)?;
    let tls_name = args.tls_name.clone().unwrap_or_else(|| args.server.clone());
    let radsec = match (radsec, args.cert_file.as_deref(), args.key_file.as_deref()) {
        (true, Some(cert_file), Some(key_file)) => {
            let config = crate::tls::client_config(rustls::DEFAULT_VERSIONS, args.ca_file.as_deref(), Some((cert_file, key_file)))?;
            let name = rustls::pki_types::ServerName::try_from(tls_name.clone()).map_err(|_| format!("--tls-name {tls_name}: not a DNS name or an IP address"))?;
            Some(Arc::new(TlsTarget { address: resolve(&args.server, args.tls_port)?, bind: args.bind, config, name, timeout }))
        }
        _ => None,
    };
    let tacacs = match tacacs_loads {
        true => Some(Arc::new(TacacsTarget { address: resolve(&args.server, args.tacacs_port)?, bind: args.bind, timeout, connect: tacacs_connect, secret: args.secret.as_bytes().to_vec() })),
        false => None,
    };
    let targets = Targets { auth: address, acct: SocketAddr::new(address.ip(), args.acct_port) };
    let params = RunParams {
        signer: Signer::new(args.secret.as_bytes())?,
        options: RequestOptions {
            nas_ip: args.nas_ip,
            nas_identifier: args.nas_identifier.clone(),
            extra,
            message_authenticator: !args.no_message_authenticator,
        },
        user: args.user.clone(),
        password: args.password.clone(),
        users: args.users,
        bind: args.bind,
        warmup,
        duration,
        requests: args.requests,
        timeout,
        retries: args.retries,
        eap: EapOptions {
            tls_version: args.tls_version.clone(),
            ca_file: args.ca_file.clone(),
            tls_name,
            cert_file: args.cert_file.clone(),
            key_file: args.key_file.clone(),
            fragment: args.eap_fragment,
        },
        radsec,
        tacacs,
    };

    let mut warnings = Vec::new();
    let mut before = None;
    let outcomes = run_loads(&specs, &params, |_, _| targets, || {
        before = metrics_url.as_ref().map(MetricsUrl::read);
    })?;
    let mut server_metrics = None;
    if let (Some(url), Some(before)) = (&metrics_url, before) {
        match before.and_then(|before| url.read().map(|after| after.since(&before))) {
            Ok(change) => {
                warnings.extend(report::metrics_warnings(&change, args.retries));
                server_metrics = Some(change);
            }
            Err(error) => warnings.push(Warning { code: "metrics_unavailable".to_string(), message: format!("could not read the server's metrics: {error}") }),
        }
    }
    let mut report = build_report(&outcomes, &params, warnings);
    report.server_metrics = server_metrics;
    Ok(report)
}

fn resolve(server: &str, port: u16) -> Result<SocketAddr, String> {
    let bracketed = if server.contains(':') && !server.starts_with('[') { format!("[{server}]") } else { server.to_string() };
    format!("{bracketed}:{port}")
        .to_socket_addrs()
        .map_err(|e| format!("--server {server}: {e}"))?
        .next()
        .ok_or_else(|| format!("--server {server}: no address"))
}

/// One load's section of the report, and what it warns about.
pub fn load_report(outcome: &LoadOutcome, params: &RunParams, warnings: &mut Vec<Warning>) -> LoadReport {
    let spec = &outcome.spec;
    let window_s = outcome.window.as_secs_f64();
    let steps: Vec<StepReport> = outcome.steps.iter().map(|(step, stats)| StepReport::new(*step, stats, window_s)).collect();
    let (send_lag_us, worker_busy_max) = match spec.rate {
        Some(rate) => {
            let p99 = report::microseconds(outcome.send_lag.value_at_quantile(0.99));
            let scheduled = params.requests.unwrap_or_else(|| report::scheduled_in_window(rate, params.warmup.as_secs_f64(), window_s));
            let sent = outcome.steps.first().map_or(0, |(_, stats)| stats.sent);
            if report::rate_not_met(sent, scheduled, p99) {
                warnings.push(Warning {
                    code: "rate_not_met".to_string(),
                    message: format!("{}: sent {sent} of {scheduled} scheduled requests, send lag p99 {p99:.1} us", spec.kind.name()),
                });
            }
            (Some(SendLag { p99 }), None)
        }
        None => {
            if report::saturated(outcome.busy_max) {
                warnings.push(Warning {
                    code: "tool_saturated".to_string(),
                    message: format!("{}: a worker had no time to spare ({:.0}% busy), so the tool may have been the limit", spec.kind.name(), outcome.busy_max * 100.0),
                });
            }
            (None, Some(outcome.busy_max))
        }
    };
    let transport = match spec.kind {
        Kind::Single(step) if step.is_tacacs() => "tcp",
        _ if params.radsec.is_some() => "tls",
        _ => "udp",
    };
    LoadReport { kind: spec.kind.name(), mode: spec.mode(), transport, concurrency: spec.concurrency, rate: spec.rate, threads: spec.threads(), sockets: spec.sockets(), reconnects: outcome.reconnects, send_lag_us, worker_busy_max, steps }
}

fn build_report(outcomes: &[LoadOutcome], params: &RunParams, mut warnings: Vec<Warning>) -> Report {
    let loads: Vec<LoadReport> = outcomes.iter().map(|outcome| load_report(outcome, params, &mut warnings)).collect();
    let duration_s = outcomes.iter().map(|outcome| outcome.window.as_secs_f64()).fold(0.0, f64::max);
    Report {
        schema: 1,
        warmup_s: params.warmup.as_secs_f64(),
        duration_s,
        trustworthy: Report::is_trustworthy(&warnings),
        loads,
        server_metrics: None,
        warnings,
    }
}
