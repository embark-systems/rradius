//! The command line: what `run` and `calibrate` accept, and the `--load` syntax.
use crate::packet::{ExtraAttribute, Step};
use clap::{Args, Parser, Subcommand};
use std::net::IpAddr;
use std::path::PathBuf;

/// In flight on one socket unless a load says otherwise.
pub const DEFAULT_WINDOW: u32 = 64;
/// Identifier has 8 bits.
pub const MAX_WINDOW: u32 = 256;
/// Limit of requests in flight for a fixed-rate load that gives none.
pub const DEFAULT_RATE_CONCURRENCY: u32 = 256;

#[derive(Parser, Debug)]
#[command(name = "rradius-bench", version, about = "Load generator for RADIUS servers")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Sends load to a server and reports throughput and latency.
    Run(Box<RunArgs>),
    /// Measures what this tool can do on this machine, against responders of its own.
    Calibrate(CalibrateArgs),
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Server host name or address.
    #[arg(long)]
    pub server: String,
    /// Shared secret. Visible in the process list: use one kept for testing.
    #[arg(long)]
    pub secret: String,
    /// KIND[,concurrency=N][,rate=N][,window=N][,threads=N][,alive=N]; can be repeated.
    #[arg(long = "load", required = true)]
    pub loads: Vec<String>,
    #[arg(long, default_value_t = 1812)]
    pub auth_port: u16,
    #[arg(long, default_value_t = 1813)]
    pub acct_port: u16,
    /// Source address of the sockets.
    #[arg(long)]
    pub bind: Option<IpAddr>,
    #[arg(long, default_value = "bench")]
    pub user: String,
    #[arg(long, default_value = "bench")]
    pub password: String,
    /// Rotates through USER0 to USER(N-1).
    #[arg(long)]
    pub users: Option<u32>,
    #[arg(long, default_value = "192.0.2.1")]
    pub nas_ip: IpAddr,
    #[arg(long, default_value = "rradius-bench")]
    pub nas_identifier: String,
    /// TYPE=int:N, TYPE=text:S, TYPE=hex:H or TYPE=ip:A, added to every request; can be repeated.
    #[arg(long = "attr")]
    pub attrs: Vec<String>,
    /// Leaves Message-Authenticator out of Access-Requests.
    #[arg(long)]
    pub no_message_authenticator: bool,
    /// Seconds to wait for a response before retransmitting or giving up.
    #[arg(long, default_value_t = 5.0)]
    pub timeout: f64,
    #[arg(long, default_value_t = 0)]
    pub retries: u32,
    /// Seconds of load before the measurement starts [default: 2].
    #[arg(long, conflicts_with = "requests")]
    pub warmup: Option<f64>,
    /// Seconds of measurement [default: 10].
    #[arg(long, conflicts_with = "requests")]
    pub duration: Option<f64>,
    /// Stops after this many measured requests per load (flows for session), with no warmup.
    #[arg(long)]
    pub requests: Option<u64>,
    /// http://host:port/metrics of the server, to report how its counters changed.
    #[arg(long)]
    pub metrics_url: Option<String>,
    #[arg(long)]
    pub json: bool,
    /// TLS version of EAP loads: 1.2 or 1.3 (eap-tls only).
    #[arg(long, default_value = "1.2")]
    pub tls_version: String,
    /// CA to check the server's EAP certificate against; without it any certificate is taken.
    #[arg(long)]
    pub ca_file: Option<PathBuf>,
    /// The name the server's certificate must be valid for with --ca-file [default: --server].
    #[arg(long)]
    pub tls_name: Option<String>,
    /// Certificate an eap-tls load logs in with.
    #[arg(long)]
    pub cert_file: Option<PathBuf>,
    #[arg(long)]
    pub key_file: Option<PathBuf>,
    /// Most octets of TLS data in one EAP-Response.
    #[arg(long, default_value_t = crate::eap::DEFAULT_FRAGMENT)]
    pub eap_fragment: usize,
    /// udp, or tls for RadSec (RFC 6614): one TLS connection per socket.
    #[arg(long, default_value = "udp")]
    pub transport: String,
    /// The RadSec port, for authentication and accounting both.
    #[arg(long, default_value_t = 2083)]
    pub tls_port: u16,
    /// The TACACS+ port of the tacacs-* loads.
    #[arg(long, default_value_t = 49)]
    pub tacacs_port: u16,
    /// keep: one TACACS+ connection per socket for the whole run; per-request: a new one per request.
    #[arg(long, default_value = "keep")]
    pub tacacs_connect: String,
}

#[derive(Args, Debug)]
pub struct CalibrateArgs {
    /// Seconds per combination.
    #[arg(long, default_value_t = 3.0)]
    pub duration: f64,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Single(Step),
    Session,
}

impl Kind {
    pub fn parse(name: &str) -> Option<Kind> {
        if name == "session" {
            return Some(Kind::Session);
        }
        Step::ALL.iter().chain(&Step::TACACS).find(|step| step.name() == name).map(|step| Kind::Single(*step))
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Single(step) => step.name(),
            Kind::Session => "session",
        }
    }
}

/// One `--load`, with its defaults filled in.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadSpec {
    pub kind: Kind,
    /// Requests in flight: what a fixed-concurrency load holds, or a limit under `rate`.
    pub concurrency: u32,
    pub rate: Option<f64>,
    pub window: u32,
    /// As asked for; `threads()` is what a load with few sockets gets.
    pub requested_threads: u32,
    pub alive: u32,
}

impl LoadSpec {
    pub fn parse(text: &str) -> Result<LoadSpec, String> {
        let mut parts = text.split(',');
        let name = parts.next().unwrap_or_default();
        let kind = Kind::parse(name).ok_or_else(|| format!("--load {text}: unknown kind \"{name}\""))?;
        let (mut concurrency, mut rate, mut window, mut threads, mut alive) = (None, None, None, None, None);
        for part in parts {
            let (key, value) = part.split_once('=').ok_or_else(|| format!("--load {text}: \"{part}\" is not key=value"))?;
            let bad = || format!("--load {text}: bad value for {key}");
            match key {
                "concurrency" => concurrency = Some(value.parse::<u32>().map_err(|_| bad())?),
                "rate" => rate = Some(value.parse::<f64>().map_err(|_| bad())?),
                "window" => window = Some(value.parse::<u32>().map_err(|_| bad())?),
                "threads" => threads = Some(value.parse::<u32>().map_err(|_| bad())?),
                "alive" => alive = Some(value.parse::<u32>().map_err(|_| bad())?),
                _ => return Err(format!("--load {text}: unknown key \"{key}\"")),
            }
        }
        if concurrency.is_none() && rate.is_none() {
            return Err(format!("--load {text}: needs concurrency or rate"));
        }
        if concurrency == Some(0) {
            return Err(format!("--load {text}: concurrency must be above 0"));
        }
        if rate.is_some_and(|rate| !(rate.is_finite() && rate > 0.0)) {
            return Err(format!("--load {text}: rate must be above 0"));
        }
        let tacacs = matches!(kind, Kind::Single(step) if step.is_tacacs());
        // A TACACS+ server answers one request at a time on a connection.
        if tacacs && window.is_some_and(|window| window != 1) {
            return Err(format!("--load {text}: window must be 1 for a TACACS+ load, which has one request at a time on each connection"));
        }
        let window = window.unwrap_or(if tacacs { 1 } else { DEFAULT_WINDOW });
        if !(1..=MAX_WINDOW).contains(&window) {
            return Err(format!("--load {text}: window must be from 1 to {MAX_WINDOW}"));
        }
        if threads == Some(0) {
            return Err(format!("--load {text}: threads must be above 0"));
        }
        if alive.is_some() && kind != Kind::Session {
            return Err(format!("--load {text}: alive is only for session"));
        }
        Ok(LoadSpec {
            kind,
            concurrency: concurrency.unwrap_or(DEFAULT_RATE_CONCURRENCY),
            rate,
            window,
            requested_threads: threads.unwrap_or(if rate.is_some() { 1 } else { 2 }),
            alive: alive.unwrap_or(0),
        })
    }

    pub fn sockets(&self) -> u32 {
        self.concurrency.div_ceil(self.window)
    }

    /// A thread with no socket would have nothing to do.
    pub fn threads(&self) -> u32 {
        self.requested_threads.min(self.sockets())
    }

    pub fn mode(&self) -> &'static str {
        if self.rate.is_some() { "fixed-rate" } else { "fixed-concurrency" }
    }
}

/// Divides `total` into `parts` shares that differ by at most one, largest first.
pub fn share(total: u64, parts: u32, index: u32) -> u64 {
    let parts = u64::from(parts);
    total / parts + u64::from(u64::from(index) < total % parts)
}

/// Reads one `--attr`.
pub fn parse_attr(text: &str) -> Result<ExtraAttribute, String> {
    let bad = |why: &str| format!("--attr {text}: {why}");
    let (number, rest) = text.split_once('=').ok_or_else(|| bad("expected TYPE=KIND:VALUE"))?;
    let attr_type: u8 = number.parse().map_err(|_| bad("type must be from 1 to 255"))?;
    if attr_type == 0 {
        return Err(bad("type must be from 1 to 255"));
    }
    if attr_type == crate::packet::MESSAGE_AUTHENTICATOR {
        return Err(bad("type 80 is Message-Authenticator, which the tool handles"));
    }
    let (kind, value) = rest.split_once(':').ok_or_else(|| bad("expected TYPE=KIND:VALUE"))?;
    let value = match kind {
        "int" => value.parse::<u32>().map_err(|_| bad("not a 32-bit integer"))?.to_be_bytes().to_vec(),
        "text" => value.as_bytes().to_vec(),
        "hex" => parse_hex(value).ok_or_else(|| bad("not hexadecimal"))?,
        "ip" => match value.parse::<IpAddr>().map_err(|_| bad("not an IP address"))? {
            IpAddr::V4(address) => address.octets().to_vec(),
            IpAddr::V6(address) => address.octets().to_vec(),
        },
        _ => return Err(bad("kind must be int, text, hex or ip")),
    };
    if value.is_empty() || value.len() > 253 {
        return Err(bad("value must be 1 to 253 octets"));
    }
    Ok(ExtraAttribute { attr_type, value })
}

fn parse_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_socket_counts() {
        for (text, sockets, in_flight) in [
            ("pap,concurrency=64,window=1", 64, 1),
            ("pap,concurrency=4096", 64, 64),
            ("pap,concurrency=100,window=64", 2, 64),
            ("pap,concurrency=256,window=256", 1, 256),
            ("tacacs-pap,concurrency=32", 32, 1),
        ] {
            let load = LoadSpec::parse(text).unwrap();
            assert_eq!(load.sockets(), sockets, "{text}");
            assert_eq!(load.window, in_flight, "{text}");
        }
    }

    #[test]
    fn test_defaults() {
        let closed = LoadSpec::parse("pap,concurrency=64").unwrap();
        assert_eq!((closed.window, closed.requested_threads, closed.rate), (64, 2, None));
        let open = LoadSpec::parse("pap,rate=100").unwrap();
        assert_eq!((open.concurrency, open.requested_threads, open.rate), (256, 1, Some(100.0)));
        assert_eq!(LoadSpec::parse("session,rate=5,alive=2").unwrap().alive, 2);
        assert_eq!(LoadSpec::parse("pap,concurrency=1").unwrap().threads(), 1);
    }

    #[test]
    fn test_invalid_loads() {
        for text in [
            "pap",
            "nope,concurrency=1",
            "pap,concurrency=0",
            "pap,rate=0",
            "pap,rate=-1",
            "pap,concurrency=1,window=0",
            "pap,concurrency=1,window=257",
            "pap,concurrency=1,threads=0",
            "pap,concurrency=1,alive=1",
            "pap,concurrency=x",
            "pap,concurrency",
            "pap,other=1",
            "tacacs-pap,concurrency=4,window=2",
        ] {
            assert!(LoadSpec::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn test_every_kind_parses() {
        for name in ["pap", "chap", "chap-authenticator", "mschapv2", "acct-start", "acct-alive", "acct-stop", "acct-on", "acct-off", "status-auth", "status-acct", "session", "eap-tls", "peap", "ttls", "aka-prime", "tacacs-pap", "tacacs-ascii", "tacacs-author", "tacacs-acct"] {
            assert_eq!(Kind::parse(name).unwrap().name(), name);
        }
    }

    #[test]
    fn test_share() {
        assert_eq!([share(5, 2, 0), share(5, 2, 1)], [3, 2]);
        assert_eq!((0..4).map(|i| share(4096, 4, i)).sum::<u64>(), 4096);
    }

    #[test]
    fn test_attrs() {
        assert_eq!(parse_attr("61=int:15").unwrap(), ExtraAttribute { attr_type: 61, value: vec![0, 0, 0, 15] });
        assert_eq!(parse_attr("31=text:abc").unwrap().value, b"abc");
        assert_eq!(parse_attr("26=hex:00ff").unwrap().value, [0, 255]);
        assert_eq!(parse_attr("8=ip:10.0.0.1").unwrap().value, [10, 0, 0, 1]);
        for text in ["0=int:1", "256=int:1", "80=hex:00", "31", "31=abc", "61=int:x", "26=hex:0", "26=hex:zz", "8=ip:nope", "31=text:", "31=other:1"] {
            assert!(parse_attr(text).is_err(), "{text}");
        }
    }
}
