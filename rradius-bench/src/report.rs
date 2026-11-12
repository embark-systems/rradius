//! What a run reports: the document `--json` prints, the text form, warnings and exit code.
use crate::server_metrics::MetricsChange;
use crate::worker::StepStats;
use hdrhistogram::Histogram;
use serde::Serialize;
use std::fmt::Write as _;

/// A worker this busy has no time left, so responses wait on it. An estimate; see `calibrate`.
const SATURATED_BUSY: f64 = 0.90;
/// Below this share of its schedule, a fixed-rate load did not deliver what was asked.
const RATE_MET_SHARE: f64 = 0.99;
/// The resolution of waiting for a timer; a send later than this is not timer resolution.
const MAX_SEND_LAG_US: f64 = 1000.0;

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema: u32,
    pub warmup_s: f64,
    pub duration_s: f64,
    pub trustworthy: bool,
    pub loads: Vec<LoadReport>,
    pub server_metrics: Option<MetricsChange>,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoadReport {
    pub kind: &'static str,
    pub mode: &'static str,
    /// `udp`, `tls` for RadSec, or `tcp` for TACACS+.
    pub transport: &'static str,
    pub concurrency: u32,
    pub rate: Option<f64>,
    pub threads: u32,
    pub sockets: u32,
    /// RadSec connections opened again during the run.
    pub reconnects: u64,
    pub send_lag_us: Option<SendLag>,
    pub worker_busy_max: Option<f64>,
    pub steps: Vec<StepReport>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct SendLag {
    pub p99: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepReport {
    pub step: &'static str,
    pub sent: u64,
    pub responses: Responses,
    pub timeouts: u64,
    pub invalid: u64,
    pub late: u64,
    pub socket_errors: u64,
    pub retransmits: u64,
    pub throughput_per_s: f64,
    /// Packets sent per finished conversation or login: 1 but for EAP and TACACS+ ASCII.
    pub rounds: f64,
    pub latency_us: Latency,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Responses {
    pub accept: u64,
    pub reject: u64,
    pub challenge: u64,
    pub accounting: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Latency {
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub p999: f64,
    pub max: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Warning {
    pub code: String,
    pub message: String,
}

pub fn microseconds(nanoseconds: u64) -> f64 {
    nanoseconds as f64 / 1000.0
}

impl Latency {
    pub fn from_histogram(histogram: &Histogram<u64>) -> Latency {
        Latency {
            p50: microseconds(histogram.value_at_quantile(0.50)),
            p90: microseconds(histogram.value_at_quantile(0.90)),
            p99: microseconds(histogram.value_at_quantile(0.99)),
            p999: microseconds(histogram.value_at_quantile(0.999)),
            max: microseconds(histogram.max()),
        }
    }
}

impl StepReport {
    pub fn new(step: crate::packet::Step, stats: &StepStats, window_s: f64) -> StepReport {
        let finished = stats.accept + stats.reject;
        let rounds = match step.has_rounds() {
            false => 1.0,
            true if finished == 0 => 0.0,
            true => stats.rounds as f64 / finished as f64,
        };
        StepReport {
            step: step.name(),
            rounds,
            sent: stats.sent,
            responses: Responses { accept: stats.accept, reject: stats.reject, challenge: stats.challenge, accounting: stats.accounting },
            timeouts: stats.timeouts,
            invalid: stats.invalid,
            late: stats.late,
            socket_errors: stats.socket_errors,
            retransmits: stats.retransmits,
            throughput_per_s: if window_s > 0.0 { stats.responses() as f64 / window_s } else { 0.0 },
            latency_us: Latency::from_histogram(&stats.latency),
        }
    }
}

/// Requests of a fixed-rate schedule, `n / rate` seconds from the start, inside the window.
pub fn scheduled_in_window(rate: f64, warmup_s: f64, duration_s: f64) -> u64 {
    let first = (warmup_s * rate).ceil() as u64;
    let end = ((warmup_s + duration_s) * rate).ceil() as u64;
    end - first
}

pub fn saturated(busy: f64) -> bool {
    busy >= SATURATED_BUSY
}

pub fn rate_not_met(sent: u64, scheduled: u64, send_lag_p99_us: f64) -> bool {
    (sent as f64) < scheduled as f64 * RATE_MET_SHARE || send_lag_p99_us > MAX_SEND_LAG_US
}

const UNTRUSTWORTHY: [&str; 3] = ["tool_saturated", "rate_not_met", "duplicate_path"];

/// Warnings about the server's counters.
pub fn metrics_warnings(change: &MetricsChange, retries: u32) -> Vec<Warning> {
    let mut warnings = Vec::new();
    if change.duplicates > 0 && retries == 0 {
        warnings.push(Warning {
            code: "duplicate_path".to_string(),
            message: format!("the server saw {} duplicates with no retransmissions sent: part of this measured its duplicate cache", change.duplicates),
        });
    }
    for (reason, count) in [("overload", change.discarded.overload), ("unknown_client", change.discarded.unknown_client), ("invalid", change.discarded.invalid)] {
        if count > 0 {
            warnings.push(Warning { code: format!("server_discarded_{reason}"), message: format!("the server discarded {count} packets for {reason}") });
        }
    }
    warnings
}

impl Report {
    /// True unless a warning says that the numbers do not describe the server.
    pub fn is_trustworthy(warnings: &[Warning]) -> bool {
        !warnings.iter().any(|warning| UNTRUSTWORTHY.contains(&warning.code.as_str()))
    }

    /// 2 for any timeout, invalid response or socket error; 1 for any rejection or challenge.
    pub fn exit_code(&self) -> i32 {
        let steps = || self.loads.iter().flat_map(|load| &load.steps);
        if steps().any(|step| step.timeouts > 0 || step.invalid > 0 || step.socket_errors > 0) {
            2
        } else if steps().any(|step| step.responses.reject > 0 || step.responses.challenge > 0) {
            1
        } else {
            0
        }
    }

    pub fn to_text(&self) -> String {
        let mut text = String::new();
        let _ = writeln!(text, "measured {:.3} s after {:.3} s of warmup", self.duration_s, self.warmup_s);
        for load in &self.loads {
            let _ = write!(text, "\n{} ({}): concurrency {}", load.kind, load.mode, load.concurrency);
            if let Some(rate) = load.rate {
                let _ = write!(text, ", rate {rate}/s");
            }
            let _ = write!(text, ", {} threads, {} sockets", load.threads, load.sockets);
            if load.transport != "udp" {
                let _ = write!(text, " over {}", load.transport);
            }
            if load.reconnects > 0 {
                let _ = write!(text, ", {} reconnects", load.reconnects);
            }
            if let Some(busy) = load.worker_busy_max {
                let _ = write!(text, ", busiest worker {:.0}%", busy * 100.0);
            }
            if let Some(lag) = load.send_lag_us {
                let _ = write!(text, ", send lag p99 {:.1} us", lag.p99);
            }
            let _ = writeln!(text);
            for step in &load.steps {
                let _ = writeln!(
                    text,
                    "  {}: {:.0}/s, sent {}, accept {}, reject {}, challenge {}, accounting {}, timeouts {}, invalid {}, late {}, socket errors {}, retransmits {}",
                    step.step,
                    step.throughput_per_s,
                    step.sent,
                    step.responses.accept,
                    step.responses.reject,
                    step.responses.challenge,
                    step.responses.accounting,
                    step.timeouts,
                    step.invalid,
                    step.late,
                    step.socket_errors,
                    step.retransmits
                );
                if step.rounds != 1.0 {
                    let _ = writeln!(text, "    rounds per finished conversation: {:.1}", step.rounds);
                }
                let latency = step.latency_us;
                let _ = writeln!(
                    text,
                    "    latency us: p50 {:.1}, p90 {:.1}, p99 {:.1}, p99.9 {:.1}, max {:.1}",
                    latency.p50, latency.p90, latency.p99, latency.p999, latency.max
                );
            }
        }
        if let Some(change) = &self.server_metrics {
            let _ = writeln!(
                text,
                "\nserver counters: auth {}, acct {}, duplicates {}, discarded overload {}, unknown_client {}, invalid {}",
                change.packets.auth, change.packets.acct, change.duplicates, change.discarded.overload, change.discarded.unknown_client, change.discarded.invalid
            );
        }
        if !self.warnings.is_empty() {
            let _ = writeln!(text, "\nwarnings:");
            for warning in &self.warnings {
                let _ = writeln!(text, "  {}: {}", warning.code, warning.message);
            }
        }
        if !self.trustworthy {
            let _ = writeln!(text, "\nthese numbers do not describe the server alone: see the warnings");
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(timeouts: u64, invalid: u64, socket_errors: u64, reject: u64, challenge: u64, late: u64) -> StepReport {
        StepReport {
            step: "pap",
            sent: 1,
            responses: Responses { accept: 1, reject, challenge, accounting: 0 },
            timeouts,
            invalid,
            late,
            socket_errors,
            retransmits: 0,
            throughput_per_s: 0.0,
            rounds: 1.0,
            latency_us: Latency { p50: 0.0, p90: 0.0, p99: 0.0, p999: 0.0, max: 0.0 },
        }
    }

    #[test]
    fn test_rounds() {
        let mut stats = StepStats::new().unwrap();
        assert_eq!(StepReport::new(crate::packet::Step::Peap, &stats, 1.0).rounds, 0.0, "no login finished");
        assert_eq!(StepReport::new(crate::packet::Step::Pap, &stats, 1.0).rounds, 1.0);
        (stats.accept, stats.reject, stats.rounds) = (3, 1, 26);
        assert_eq!(StepReport::new(crate::packet::Step::Ttls, &stats, 1.0).rounds, 6.5);
    }

    fn report(steps: Vec<StepReport>, warnings: Vec<Warning>) -> Report {
        Report {
            schema: 1,
            warmup_s: 0.0,
            duration_s: 1.0,
            trustworthy: Report::is_trustworthy(&warnings),
            loads: vec![LoadReport {
                kind: "pap",
                mode: "fixed-concurrency",
                transport: "udp",
                concurrency: 1,
                rate: None,
                threads: 1,
                sockets: 1,
                reconnects: 0,
                send_lag_us: None,
                worker_busy_max: Some(0.0),
                steps,
            }],
            server_metrics: None,
            warnings,
        }
    }

    #[test]
    fn test_exit_code_from_results() {
        for (steps, code) in [
            (vec![step(0, 0, 0, 0, 0, 0)], 0),
            (vec![step(0, 0, 0, 0, 0, 3)], 0),
            (vec![step(0, 0, 0, 1, 0, 0)], 1),
            (vec![step(0, 0, 0, 0, 1, 0)], 1),
            (vec![step(1, 0, 0, 1, 0, 0)], 2),
            (vec![step(0, 1, 0, 0, 0, 0)], 2),
            (vec![step(0, 0, 1, 0, 0, 0)], 2),
        ] {
            assert_eq!(report(steps, vec![]).exit_code(), code);
        }
    }

    #[test]
    fn test_warnings_do_not_change_exit_code() {
        let warning = Warning { code: "tool_saturated".to_string(), message: String::new() };
        let result = report(vec![step(0, 0, 0, 0, 0, 0)], vec![warning]);
        assert_eq!(result.exit_code(), 0);
        assert!(!result.trustworthy);
    }

    #[test]
    fn test_thresholds() {
        assert!(!saturated(0.89));
        assert!(saturated(0.90));
        assert!(!rate_not_met(99, 100, 1000.0));
        assert!(rate_not_met(98, 100, 0.0));
        assert!(rate_not_met(100, 100, 1000.1));
    }

    #[test]
    fn test_scheduled_in_window() {
        assert_eq!(scheduled_in_window(100.0, 0.0, 1.0), 100);
        assert_eq!(scheduled_in_window(100.0, 2.0, 10.0), 1000);
        assert_eq!(scheduled_in_window(3.0, 0.5, 1.0), 3);
    }

    #[test]
    fn test_only_some_warnings_mark_untrustworthy() {
        let warning = |code: &str| Warning { code: code.to_string(), message: String::new() };
        assert!(Report::is_trustworthy(&[warning("metrics_unavailable"), warning("server_discarded_overload")]));
        for code in UNTRUSTWORTHY {
            assert!(!Report::is_trustworthy(&[warning(code)]));
        }
    }

    #[test]
    fn test_metrics_warnings() {
        let change = |duplicates, overload| MetricsChange {
            packets: crate::server_metrics::PacketChange { auth: 0, acct: 0 },
            duplicates,
            discarded: crate::server_metrics::DiscardChange { overload, unknown_client: 0, invalid: 0 },
        };
        assert!(metrics_warnings(&change(0, 0), 0).is_empty());
        assert_eq!(metrics_warnings(&change(1, 0), 0)[0].code, "duplicate_path");
        assert!(metrics_warnings(&change(1, 0), 1).is_empty());
        let warnings = metrics_warnings(&change(0, 5), 0);
        assert!(warnings[0].message.contains("overload"));
        assert!(warnings[0].message.contains('5'));
    }
}
