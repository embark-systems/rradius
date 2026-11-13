//! `rradius-server tuning`: which operating system limits to raise for this configuration, how
//! far, and how, on Linux, macOS and FreeBSD. It only reads: the system's values and the
//! configuration file. Nothing is changed, no node is started and no port is opened.
//!
//! The rules (`advise`) do not depend on the platform it is built for, so those of every platform
//! are tested everywhere; only reading the values (`read`) does.

use crate::config::ServerConfig;
use crate::extension::Extensions;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

pub const USAGE: &str = "usage: rradius-server tuning [--config <path>]";

/// What the open files need on top of the configured connections: database files, the log,
/// the cluster's own connections, LDAP and HTTP pools. An estimate, not a measurement.
pub const BASE_OPEN_FILES: usize = 256;
/// The open files suggested at least, whatever the need.
pub const MIN_OPEN_FILES: u64 = 65536;
/// The receive buffer suggested at least for each UDP socket.
pub const MIN_RECEIVE_BUFFER: u64 = 4 * MIB;
/// The size counted for each waiting RADIUS packet: the largest one, as an upper bound of what
/// the kernel charges a socket for it.
pub const PACKET_BYTES: u64 = 4096;
/// The listen queue the TCP listeners ask for.
pub const LISTEN_QUEUE: u64 = 1024;
/// `net.core.netdev_max_backlog` suggested when packets are dropped before the sockets.
pub const NETDEV_BACKLOG: u64 = 16384;

const MIB: u64 = 1024 * 1024;

/// What the reason of a system default receive buffer adds.
const SET_IT_INSTEAD: &str = "Setting [radius] recv_buffer_bytes instead leaves only the limit to raise";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Linux,
    MacOs,
    FreeBsd,
}

impl Platform {
    /// The platform this program runs on, if it has advice for it.
    pub fn current() -> Option<Platform> {
        if cfg!(target_os = "linux") {
            Some(Platform::Linux)
        } else if cfg!(target_os = "macos") {
            Some(Platform::MacOs)
        } else if cfg!(target_os = "freebsd") {
            Some(Platform::FreeBsd)
        } else {
            None
        }
    }

    fn name(self) -> &'static str {
        match self {
            Platform::Linux => "Linux",
            Platform::MacOs => "macOS",
            Platform::FreeBsd => "FreeBSD",
        }
    }
}

/// The figures of the configuration the advice depends on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inputs {
    /// `[radius]`: `max_auth_inflight` and `sockets`; `None` without it.
    pub radius: Option<(usize, usize)>,
    /// `[tacacs] max_connections`; `None` without it.
    pub tacacs_connections: Option<usize>,
    pub read_connections: usize,
    pub cluster_nodes: usize,
    /// What the extensions report: a name and the connections it may hold open.
    pub extension_connections: Vec<(String, usize)>,
    pub pin_threads: bool,
    /// `[radius] recv_buffer_bytes`; `None` when unset.
    pub recv_buffer: Option<u64>,
}

impl Inputs {
    /// Those of a configuration file that has only the defaults, with `[radius]` and `[tacacs]`.
    pub fn defaults() -> Inputs {
        let sockets = if cfg!(target_os = "linux") { crate::runtime::worker_threads() } else { 1 };
        Inputs {
            radius: Some((crate::radius::DEFAULT_MAX_INFLIGHT, sockets)),
            tacacs_connections: Some(crate::tacacs::DEFAULT_MAX_CONNECTIONS),
            read_connections: crate::config::default_read_connections(),
            cluster_nodes: 0,
            extension_connections: Vec::new(),
            pin_threads: false,
            recv_buffer: None,
        }
    }

    pub fn of(config: &ServerConfig, extensions: &Extensions) -> Inputs {
        let mut extension_connections = Vec::new();
        for extension in extensions.iter() {
            for (name, count) in extension.tuning_connections(config) {
                extension_connections.push((name.to_string(), count));
            }
        }
        Inputs {
            radius: config.radius.as_ref().map(|radius| (radius.max_auth_inflight, radius.sockets)),
            tacacs_connections: config.tacacs.as_ref().map(|tacacs| tacacs.max_connections),
            read_connections: config.cluster.read_connections,
            cluster_nodes: config.cluster.nodes.len(),
            extension_connections,
            pin_threads: config.runtime.pin_threads,
            recv_buffer: config.radius.as_ref().and_then(|radius| radius.recv_buffer_bytes).map(|bytes| bytes as u64),
        }
    }
}

/// What the configuration needs of the system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Needs {
    /// The open files the configuration may use at once.
    pub open_files: usize,
    /// The receive buffer suggested for each UDP socket; `None` without `[radius]`.
    pub receive_buffer: Option<u64>,
    /// Whether a TCP port is listened on.
    pub tcp_listen: bool,
    /// `[radius] sockets`; `None` without `[radius]`.
    pub sockets: Option<usize>,
    pub pin_threads: bool,
    /// `[radius] recv_buffer_bytes`; `None` when unset or without `[radius]`.
    pub recv_buffer: Option<u64>,
}

pub fn needs(inputs: &Inputs) -> Needs {
    let extensions: usize = inputs.extension_connections.iter().map(|(_, count)| count).sum();
    let open_files = BASE_OPEN_FILES + inputs.tacacs_connections.unwrap_or(0) + inputs.read_connections + 2 * inputs.cluster_nodes + extensions;
    Needs {
        open_files,
        receive_buffer: inputs.radius.map(|(inflight, sockets)| receive_buffer(inflight, sockets)),
        tcp_listen: inputs.tacacs_connections.is_some() || !inputs.extension_connections.is_empty(),
        sockets: inputs.radius.map(|(_, sockets)| sockets),
        pin_threads: inputs.pin_threads,
        recv_buffer: inputs.radius.and(inputs.recv_buffer),
    }
}

/// The open files suggested for a need: twice it, and at least `MIN_OPEN_FILES`.
pub fn suggested_open_files(need: usize) -> u64 {
    MIN_OPEN_FILES.max(2 * need as u64)
}

/// The receive buffer for each socket: room for its share of the requests in flight, at least
/// `MIN_RECEIVE_BUFFER`, rounded up to a whole MiB.
pub fn receive_buffer(inflight: usize, sockets: usize) -> u64 {
    let share = inflight.div_ceil(sockets.max(1)) as u64;
    let bytes = MIN_RECEIVE_BUFFER.max(share * PACKET_BYTES);
    bytes.div_ceil(MIB) * MIB
}

/// `RcvbufErrors` of the `Udp:` lines of `/proc/net/snmp`.
pub fn parse_snmp_rcvbuf_errors(text: &str) -> Option<u64> {
    let mut lines = text.lines().filter(|line| line.starts_with("Udp: "));
    let names = lines.next()?;
    let values = lines.next()?;
    let index = names.split_whitespace().position(|name| name == "RcvbufErrors")?;
    values.split_whitespace().nth(index)?.parse().ok()
}

/// The total of the second column, hexadecimal, of `/proc/net/softnet_stat`: packets dropped
/// before reaching a socket.
pub fn parse_softnet_drops(text: &str) -> Option<u64> {
    let mut total: u64 = 0;
    let mut lines = 0;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let column = line.split_whitespace().nth(1)?;
        total = total.checked_add(u64::from_str_radix(column, 16).ok()?)?;
        lines += 1;
    }
    (lines > 0).then_some(total)
}

/// The number on the line of `netstat -s -p udp` that counts the datagrams dropped because a
/// socket buffer was full.
pub fn parse_netstat_udp_drops(text: &str) -> Option<u64> {
    let line = text.lines().find(|line| line.contains("dropped due to full socket buffers"))?;
    line.split_whitespace().next()?.parse().ok()
}

/// The values read from the system, by name; one that could not be read has the reason.
#[derive(Clone, Debug, Default)]
pub struct Readings(BTreeMap<&'static str, Result<u64, String>>);

/// The name of the soft open files limit of the process among the readings.
pub const OPEN_FILES: &str = "open files";
/// The UDP datagrams dropped because a socket receive buffer was full.
pub const UDP_DROPS: &str = "udp drops";
/// Linux: the packets dropped before reaching a socket.
pub const SOFTNET_DROPS: &str = "softnet drops";

impl Readings {
    pub fn set(&mut self, name: &'static str, value: Result<u64, String>) {
        self.0.insert(name, value);
    }

    fn get(&self, name: &str) -> Result<u64, String> {
        self.0.get(name).cloned().unwrap_or_else(|| Err("not read".to_string()))
    }
}

/// One line of advice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub title: String,
    pub current: Result<String, String>,
    /// `None` when there is nothing to suggest, only a value to show.
    pub suggested: Option<String>,
    pub ok: bool,
    pub why: String,
    pub now: Option<String>,
    pub persist: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub platform: Platform,
    pub notes: Vec<String>,
    pub udp_drops: Result<u64, String>,
    pub softnet_drops: Option<Result<u64, String>>,
    pub checks: Vec<Check>,
}

/// A check of a numeric system value that should be at least `suggested`.
fn at_least(title: String, current: Result<u64, String>, suggested: u64, why: String, now: impl Fn(u64) -> String, persist: impl Fn(u64) -> String) -> Check {
    let ok = matches!(current, Ok(value) if value >= suggested);
    let readable = current.is_ok();
    Check {
        title,
        current: current.map(|value| value.to_string()),
        suggested: Some(suggested.to_string()),
        ok,
        why,
        now: (readable && !ok).then(|| now(suggested)),
        persist: (readable && !ok).then(|| persist(suggested)),
    }
}

fn sysctl_check(platform: Platform, readings: &Readings, name: &'static str, title: &str, suggested: u64, why: String) -> Check {
    let file = match platform {
        Platform::Linux => "/etc/sysctl.d/90-rradius.conf",
        Platform::MacOs | Platform::FreeBsd => "/etc/sysctl.conf",
    };
    at_least(format!("{title} ({name})"), readings.get(name), suggested, why, |value| format!("sysctl -w {name}={value}"), |value| format!("{file}: {name} = {value}"))
}

pub fn advise(platform: Platform, readings: &Readings, needs: &Needs) -> Report {
    let mut checks = Vec::new();
    match (needs.receive_buffer, needs.recv_buffer) {
        (Some(buffer), None) => match platform {
            Platform::Linux => {
                checks.push(sysctl_check(platform, readings, "net.core.rmem_default", "UDP receive buffer", buffer, format!("rradius does not set SO_RCVBUF without [radius] recv_buffer_bytes, so every RADIUS socket gets this size; it should hold its share of max_auth_inflight requests. {SET_IT_INSTEAD}")));
                checks.push(sysctl_check(platform, readings, "net.core.rmem_max", "Largest receive buffer", buffer, "the most a socket may ask for; kept at least the default so the two agree".to_string()));
            }
            Platform::MacOs | Platform::FreeBsd => {
                // First: the system refuses a recvspace this limit does not allow.
                checks.push(sysctl_check(platform, readings, "kern.ipc.maxsockbuf", "Largest socket buffer", 2 * buffer, "the receive buffer counts its bookkeeping against this limit too, so twice the buffer; raise it before recvspace".to_string()));
                checks.push(sysctl_check(platform, readings, "net.inet.udp.recvspace", "UDP receive buffer", buffer, format!("every RADIUS socket gets this size without [radius] recv_buffer_bytes; it should hold its share of max_auth_inflight requests. {SET_IT_INSTEAD}")));
            }
        },
        (Some(buffer), Some(configured)) => {
            checks.push(match platform {
                Platform::Linux => sysctl_check(platform, readings, "net.core.rmem_max", "Largest receive buffer", configured, "every RADIUS socket asks for [radius] recv_buffer_bytes, and the system cuts it down to this limit".to_string()),
                Platform::MacOs | Platform::FreeBsd => sysctl_check(platform, readings, "kern.ipc.maxsockbuf", "Largest socket buffer", 2 * configured, "every RADIUS socket asks for [radius] recv_buffer_bytes, and the system refuses it above this limit, which counts bookkeeping too, so twice the buffer".to_string()),
            });
            let ok = configured >= buffer;
            checks.push(Check {
                title: "Receive buffer of each RADIUS socket ([radius] recv_buffer_bytes)".to_string(),
                current: Ok(configured.to_string()),
                suggested: Some(buffer.to_string()),
                ok,
                why: "it should hold the socket's share of max_auth_inflight requests".to_string(),
                now: None,
                persist: (!ok).then(|| format!("the configuration file: recv_buffer_bytes = {buffer} in [radius], then restart")),
            });
        }
        (None, _) => {}
    }
    let open_files = suggested_open_files(needs.open_files);
    let why = format!("the configuration may hold about {} files and connections open at once; twice that, at least {MIN_OPEN_FILES}", needs.open_files);
    checks.push(match platform {
        Platform::Linux => at_least(
            "Open files of the process (RLIMIT_NOFILE)".to_string(),
            readings.get(OPEN_FILES),
            open_files,
            why,
            |value| format!("ulimit -n {value} (in the shell that starts rradius-server)"),
            |value| format!("the systemd unit: LimitNOFILE={value}, or without systemd /etc/security/limits.conf: <user> soft nofile {value}"),
        ),
        Platform::MacOs => at_least(
            "Open files of the process (RLIMIT_NOFILE)".to_string(),
            readings.get(OPEN_FILES),
            open_files,
            why,
            |value| format!("ulimit -n {value} (in the shell that starts rradius-server)"),
            |value| format!("the LaunchDaemon plist: SoftResourceLimits and HardResourceLimits NumberOfFiles = {value}"),
        ),
        Platform::FreeBsd => at_least(
            "Open files of the process (RLIMIT_NOFILE)".to_string(),
            readings.get(OPEN_FILES),
            open_files,
            why,
            |value| format!("ulimit -n {value} (in the shell that starts rradius-server)"),
            |value| format!("/etc/login.conf: :openfiles={value}: in the class of the rradius user, then cap_mkdb /etc/login.conf"),
        ),
    });
    if platform != Platform::Linux {
        checks.push(sysctl_check(platform, readings, "kern.maxfilesperproc", "Open files of any process", open_files, "the system's cap on the open files limit of one process".to_string()));
    }
    if needs.tcp_listen {
        let name = match platform {
            Platform::Linux => "net.core.somaxconn",
            Platform::MacOs => "kern.ipc.somaxconn",
            Platform::FreeBsd => "kern.ipc.soacceptqueue",
        };
        checks.push(sysctl_check(platform, readings, name, "TCP listen queue", LISTEN_QUEUE, format!("the TCP listeners ask for {LISTEN_QUEUE} waiting connections, and the system caps it here")));
    }
    let softnet_drops = (platform == Platform::Linux).then(|| readings.get(SOFTNET_DROPS));
    if platform == Platform::Linux {
        let name = "net.core.netdev_max_backlog";
        match &softnet_drops {
            Some(Ok(drops)) if *drops > 0 => checks.push(sysctl_check(platform, readings, name, "Network device backlog", NETDEV_BACKLOG, format!("{drops} packets were dropped before reaching a socket"))),
            other => checks.push(Check {
                title: format!("Network device backlog ({name})"),
                current: readings.get(name).map(|value| value.to_string()),
                suggested: None,
                ok: true,
                why: if matches!(other, Some(Err(_))) {
                    "no change suggested while the packets dropped before reaching a socket cannot be read".to_string()
                } else {
                    "no change while no packet is dropped before reaching a socket".to_string()
                },
                now: None,
                persist: None,
            }),
        }
    }
    if let Some(sockets) = needs.sockets {
        checks.push(sockets_check(platform, sockets));
    }
    if platform == Platform::Linux {
        checks.push(Check {
            title: "Worker threads pinned to CPUs ([runtime] pin_threads)".to_string(),
            current: Ok(needs.pin_threads.to_string()),
            suggested: None,
            ok: true,
            why: "on a machine that runs only rradius, pin_threads = true can raise throughput; see the usage document".to_string(),
            now: None,
            persist: None,
        });
    }
    let mut notes = Vec::new();
    if platform == Platform::FreeBsd {
        notes.push("(this advice follows the FreeBSD documentation and has not been tested on FreeBSD)".to_string());
    }
    Report { platform, notes, udp_drops: readings.get(UDP_DROPS), softnet_drops, checks }
}

fn sockets_check(platform: Platform, sockets: usize) -> Check {
    let title = "RADIUS receive sockets ([radius] sockets)".to_string();
    match platform {
        Platform::Linux => {
            let threads = crate::runtime::worker_threads();
            let ok = sockets == threads;
            Check {
                title,
                current: Ok(sockets.to_string()),
                suggested: Some(format!("{threads} (leave it unset)")),
                ok,
                why: "unset, it is one per worker thread, so every thread receives".to_string(),
                now: None,
                persist: (!ok).then(|| "the configuration file: remove sockets from [radius]".to_string()),
            }
        }
        Platform::MacOs | Platform::FreeBsd => {
            let ok = sockets == 1;
            Check {
                title,
                current: Ok(sockets.to_string()),
                suggested: Some("1".to_string()),
                ok,
                why: "only Linux shares the packets of one port out over several sockets, so it must be 1 here; --check refuses another value".to_string(),
                now: None,
                persist: (!ok).then(|| "the configuration file: remove sockets from [radius]".to_string()),
            }
        }
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "rradius tuning advice for {}", self.platform.name())?;
        for note in &self.notes {
            writeln!(f, "{note}")?;
        }
        writeln!(f)?;
        match &self.udp_drops {
            Ok(0) => {
                writeln!(f, "UDP packets dropped because a receive buffer was full, since the system started: 0")?;
                writeln!(f, "  The receive buffers need no change at present; the suggestions below are for a higher load.")?;
            }
            Ok(drops) => {
                writeln!(f, "UDP packets dropped because a receive buffer was full, since the system started: {drops}")?;
                writeln!(f, "  Raise the receive buffers below, then watch whether this count still grows.")?;
            }
            Err(reason) => {
                writeln!(f, "UDP packets dropped because a receive buffer was full, since the system started: unknown ({reason})")?;
                writeln!(f, "  It cannot tell whether packets are dropped.")?;
            }
        }
        match &self.softnet_drops {
            Some(Ok(drops)) => writeln!(f, "Packets dropped before reaching a socket, since the system started: {drops}")?,
            Some(Err(reason)) => writeln!(f, "Packets dropped before reaching a socket, since the system started: unknown ({reason})")?,
            None => {}
        }
        for check in &self.checks {
            writeln!(f)?;
            writeln!(f, "{}", check.title)?;
            match &check.current {
                Ok(value) if check.ok && check.suggested.is_some() => writeln!(f, "  current:   {value} (ok)")?,
                Ok(value) => writeln!(f, "  current:   {value}")?,
                Err(reason) => writeln!(f, "  current:   unknown ({reason})")?,
            }
            if let Some(suggested) = &check.suggested {
                writeln!(f, "  suggested: {suggested}")?;
            }
            writeln!(f, "  why:       {}", check.why)?;
            if let Some(now) = &check.now {
                writeln!(f, "  now:       {now}")?;
            }
            if let Some(persist) = &check.persist {
                writeln!(f, "  persist:   {persist}")?;
            }
        }
        writeln!(f)?;
        writeln!(f, "These suggestions are unmeasured starting points: apply them, then confirm with the dropped packet counts above.")
    }
}

/// The `tuning` command: the advice for this system and the configuration at `--config`, or at
/// the default path. Returns the exit status.
pub fn run(args: &[String], extensions: &Extensions) -> i32 {
    crate::logging::init_for_command();
    let explicit = match args {
        [] => None,
        [flag, path] if flag == "--config" => Some(path.as_str()),
        _ => {
            eprintln!("{USAGE}");
            return 2;
        }
    };
    let Some(platform) = Platform::current() else {
        println!("rradius tuning: no advice for this platform (Linux, macOS and FreeBSD have it)");
        return 0;
    };
    let inputs = match explicit {
        Some(path) => match crate::config::load(Path::new(path)) {
            Ok(config) => Inputs::of(&config, extensions),
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        },
        None => {
            let path = crate::config::default_config_path();
            match crate::config::load(Path::new(&path)) {
                Ok(config) => Inputs::of(&config, extensions),
                Err(e) => {
                    println!("{path}: {e}; the default settings are used");
                    Inputs::defaults()
                }
            }
        }
    };
    print!("{}", advise(platform, &read(platform), &needs(&inputs)));
    0
}

/// The values of this system, as `advise` takes them.
pub fn read(platform: Platform) -> Readings {
    let mut readings = Readings::default();
    readings.set(OPEN_FILES, open_files_limit());
    match platform {
        Platform::Linux => {
            for name in ["net.core.rmem_default", "net.core.rmem_max", "net.core.somaxconn", "net.core.netdev_max_backlog"] {
                let file = format!("/proc/sys/{}", name.replace('.', "/"));
                readings.set(name, read_number(&file));
            }
            readings.set(UDP_DROPS, read_text("/proc/net/snmp").and_then(|text| parse_snmp_rcvbuf_errors(&text).ok_or_else(|| "no RcvbufErrors in /proc/net/snmp".to_string())));
            readings.set(SOFTNET_DROPS, read_text("/proc/net/softnet_stat").and_then(|text| parse_softnet_drops(&text).ok_or_else(|| "/proc/net/softnet_stat not understood".to_string())));
        }
        Platform::MacOs | Platform::FreeBsd => {
            let queue = if platform == Platform::MacOs { "kern.ipc.somaxconn" } else { "kern.ipc.soacceptqueue" };
            for name in ["net.inet.udp.recvspace", "kern.ipc.maxsockbuf", "kern.maxfilesperproc", queue] {
                readings.set(name, sysctl(name));
            }
            readings.set(UDP_DROPS, output("netstat", &["-s", "-p", "udp"]).and_then(|text| parse_netstat_udp_drops(&text).ok_or_else(|| "no dropped due to full socket buffers line in netstat -s -p udp".to_string())));
        }
    }
    readings
}

fn read_text(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
}

fn read_number(path: &str) -> Result<u64, String> {
    let text = read_text(path)?;
    text.trim().parse().map_err(|_| format!("{path}: not a number"))
}

fn output(program: &str, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new(program).args(args).output().map_err(|e| format!("{program}: {e}"))?;
    if !output.status.success() {
        return Err(format!("{program} {}: {}", args.join(" "), output.status));
    }
    String::from_utf8(output.stdout).map_err(|_| format!("{program}: output not UTF-8"))
}

fn sysctl(name: &str) -> Result<u64, String> {
    let text = output("sysctl", &["-n", name])?;
    text.trim().parse().map_err(|_| format!("sysctl {name}: not a number"))
}

/// The soft limit on the open files of this process.
fn open_files_limit() -> Result<u64, String> {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit writes only the struct it is given.
    let status = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
    if status != 0 {
        return Err(format!("getrlimit: {}", std::io::Error::last_os_error()));
    }
    if limit.rlim_cur == libc::RLIM_INFINITY {
        return Ok(u64::MAX);
    }
    // `rlim_t` is u64 on Linux and macOS but i64 on FreeBSD.
    #[allow(clippy::unnecessary_cast)]
    let current = limit.rlim_cur as u64;
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> Inputs {
        Inputs { radius: Some((4096, 8)), tacacs_connections: Some(1024), read_connections: 16, cluster_nodes: 3, extension_connections: vec![("RadSec".to_string(), 2048)], pin_threads: false, recv_buffer: None }
    }

    #[test]
    fn test_needs_from_the_configuration() {
        let needs = needs(&inputs());
        assert_eq!(needs.open_files, 256 + 1024 + 16 + 6 + 2048);
        assert_eq!(needs.open_files, 3350);
        assert_eq!(suggested_open_files(needs.open_files), 65536);
        assert_eq!(suggested_open_files(40000), 80000);
        assert_eq!(needs.receive_buffer, Some(4194304));
        assert!(needs.tcp_listen);
        assert_eq!(needs.sockets, Some(8));
    }

    #[test]
    fn test_receive_buffer() {
        assert_eq!(receive_buffer(4096, 8), 4194304);
        assert_eq!(receive_buffer(8192, 1), 33554432);
        // ceil(4096 / 3) = 1366 packets, 5595136 bytes, up to 6 MiB.
        assert_eq!(receive_buffer(4096, 3), 6291456);
    }

    #[test]
    fn test_needs_without_radius_or_tacacs() {
        let needs = needs(&Inputs { radius: None, tacacs_connections: None, extension_connections: Vec::new(), ..inputs() });
        assert_eq!(needs.open_files, 256 + 16 + 6, "TACACS+ counts 0");
        assert_eq!((needs.receive_buffer, needs.sockets, needs.tcp_listen), (None, None, false));
        // An extension's connections listen on TCP.
        let with_radsec = super::needs(&Inputs { radius: None, tacacs_connections: None, ..inputs() });
        assert!(with_radsec.tcp_listen);
    }

    #[test]
    fn test_parse_counts() {
        let snmp = "Ip: Forwarding\nIp: 1\nUdp: InDatagrams NoPorts InErrors OutDatagrams RcvbufErrors SndbufErrors\nUdp: 100 0 5 90 5 0\nUdpLite: InDatagrams NoPorts InErrors OutDatagrams RcvbufErrors SndbufErrors\nUdpLite: 0 0 0 0 7 0\n";
        assert_eq!(parse_snmp_rcvbuf_errors(snmp), Some(5));
        assert_eq!(parse_snmp_rcvbuf_errors("Ip: 1\n"), None);
        let netstat = "udp:\n        1000 datagrams received\n        12 dropped due to full socket buffers\n        0 not for hashed pcb\n";
        assert_eq!(parse_netstat_udp_drops(netstat), Some(12));
        assert_eq!(parse_netstat_udp_drops("udp:\n        1000 datagrams received\n"), None);
        let softnet = "0000a000 00000002 00000000 00000000\n0000b000 00000010 00000000 00000000\n";
        assert_eq!(parse_softnet_drops(softnet), Some(18));
        assert_eq!(parse_softnet_drops(""), None);
        assert_eq!(parse_softnet_drops("0000a000 zz\n"), None);
    }

    fn linux_readings() -> Readings {
        let mut readings = Readings::default();
        readings.set("net.core.rmem_default", Ok(212992));
        readings.set("net.core.rmem_max", Ok(212992));
        readings.set("net.core.somaxconn", Ok(4096));
        readings.set("net.core.netdev_max_backlog", Ok(1000));
        readings.set(OPEN_FILES, Ok(1024));
        readings.set(UDP_DROPS, Ok(0));
        readings.set(SOFTNET_DROPS, Ok(0));
        readings
    }

    fn bsd_readings(queue: &'static str) -> Readings {
        let mut readings = Readings::default();
        readings.set("net.inet.udp.recvspace", Ok(42080));
        readings.set("kern.ipc.maxsockbuf", Ok(2097152));
        readings.set("kern.maxfilesperproc", Ok(10000));
        readings.set(queue, Ok(128));
        readings.set(OPEN_FILES, Ok(256));
        readings.set(UDP_DROPS, Ok(12));
        readings
    }

    fn check<'a>(report: &'a Report, title: &str) -> Result<&'a Check, String> {
        report.checks.iter().find(|check| check.title.contains(title)).ok_or_else(|| format!("no check {title}: {report}"))
    }

    #[test]
    fn test_linux_receive_buffer_below_the_suggestion() -> Result<(), String> {
        let report = advise(Platform::Linux, &linux_readings(), &needs(&inputs()));
        let rmem = check(&report, "net.core.rmem_default")?;
        assert_eq!((rmem.current.clone(), rmem.suggested.as_deref(), rmem.ok), (Ok("212992".to_string()), Some("4194304"), false));
        assert_eq!(rmem.now.as_deref(), Some("sysctl -w net.core.rmem_default=4194304"));
        assert_eq!(rmem.persist.as_deref(), Some("/etc/sysctl.d/90-rradius.conf: net.core.rmem_default = 4194304"));
        let text = report.to_string();
        assert!(text.starts_with("rradius tuning advice for Linux\n\n"), "{text}");
        assert!(text.contains("dropped because a receive buffer was full, since the system started: 0\n  The receive buffers need no change at present; the suggestions below are for a higher load.\n"), "{text}");
        assert!(rmem.why.contains("Setting [radius] recv_buffer_bytes instead"), "{rmem:?}");
        assert!(check(&report, "[radius] recv_buffer_bytes").is_err(), "only when it is set");
        assert!(text.contains("Packets dropped before reaching a socket, since the system started: 0"), "{text}");
        assert!(text.contains("  now:       sysctl -w net.core.rmem_default=4194304\n"), "{text}");
        assert!(text.trim_end().ends_with("confirm with the dropped packet counts above."), "{text}");
        // The listen queue is high enough; the backlog only shown; pin_threads shown.
        assert!(check(&report, "net.core.somaxconn")?.ok && check(&report, "net.core.somaxconn")?.now.is_none());
        assert_eq!(check(&report, "netdev_max_backlog")?.suggested, None);
        assert_eq!(check(&report, "pin_threads")?.current, Ok("false".to_string()));
        let files = check(&report, "RLIMIT_NOFILE")?;
        assert!(files.persist.as_deref().is_some_and(|persist| persist.contains("LimitNOFILE=65536")), "{files:?}");
        Ok(())
    }

    #[test]
    fn test_a_value_already_high_enough() -> Result<(), String> {
        let mut readings = linux_readings();
        readings.set("net.core.rmem_default", Ok(8388608));
        let report = advise(Platform::Linux, &readings, &needs(&inputs()));
        let rmem = check(&report, "net.core.rmem_default")?;
        assert!(rmem.ok && rmem.now.is_none() && rmem.persist.is_none());
        let text = report.to_string();
        assert!(text.contains("  current:   8388608 (ok)\n"), "{text}");
        assert!(!text.contains("sysctl -w net.core.rmem_default"), "{text}");
        Ok(())
    }

    #[test]
    fn test_unknown_values() -> Result<(), String> {
        let mut readings = linux_readings();
        readings.set("net.core.rmem_default", Err("/proc/sys/net/core/rmem_default: denied".to_string()));
        readings.set(UDP_DROPS, Err("no RcvbufErrors".to_string()));
        let report = advise(Platform::Linux, &readings, &needs(&inputs()));
        let rmem = check(&report, "net.core.rmem_default")?;
        assert!(rmem.now.is_none() && rmem.persist.is_none() && rmem.current.is_err());
        let text = report.to_string();
        assert!(text.contains("unknown (no RcvbufErrors)\n  It cannot tell whether packets are dropped."), "{text}");
        assert!(text.contains("  current:   unknown (/proc/sys/net/core/rmem_default: denied)"), "{text}");
        Ok(())
    }

    #[test]
    fn test_softnet_drops_suggest_the_backlog() -> Result<(), String> {
        let mut readings = linux_readings();
        readings.set(SOFTNET_DROPS, Ok(18));
        let report = advise(Platform::Linux, &readings, &needs(&inputs()));
        let backlog = check(&report, "netdev_max_backlog")?;
        assert_eq!((backlog.suggested.as_deref(), backlog.now.as_deref()), (Some("16384"), Some("sysctl -w net.core.netdev_max_backlog=16384")));
        Ok(())
    }

    #[test]
    fn test_softnet_drops_unreadable() -> Result<(), String> {
        let mut readings = linux_readings();
        readings.set(SOFTNET_DROPS, Err("/proc/net/softnet_stat: denied".to_string()));
        let report = advise(Platform::Linux, &readings, &needs(&inputs()));
        let backlog = check(&report, "netdev_max_backlog")?;
        assert_eq!((backlog.suggested.as_deref(), backlog.now.as_deref()), (None, None));
        assert!(backlog.why.contains("cannot be read"), "{backlog:?}");
        Ok(())
    }

    #[test]
    fn test_linux_with_recv_buffer_bytes() -> Result<(), String> {
        let report = advise(Platform::Linux, &linux_readings(), &needs(&Inputs { recv_buffer: Some(2097152), ..inputs() }));
        let limit = check(&report, "net.core.rmem_max")?;
        assert_eq!((limit.suggested.as_deref(), limit.now.as_deref()), (Some("2097152"), Some("sysctl -w net.core.rmem_max=2097152")));
        assert!(check(&report, "net.core.rmem_default").is_err(), "the default is not used");
        let configured = check(&report, "[radius] recv_buffer_bytes")?;
        assert_eq!((configured.current.clone(), configured.suggested.as_deref(), configured.ok), (Ok("2097152".to_string()), Some("4194304"), false));
        assert_eq!(configured.now, None);
        assert!(configured.persist.as_deref().is_some_and(|persist| persist.contains("recv_buffer_bytes = 4194304")), "{configured:?}");
        // At or above the suggestion it is ok.
        let enough = advise(Platform::Linux, &linux_readings(), &needs(&Inputs { recv_buffer: Some(8388608), ..inputs() }));
        let configured = check(&enough, "[radius] recv_buffer_bytes")?;
        assert!(configured.ok && configured.persist.is_none());
        // Without [radius] there is no receive buffer to check.
        let none = advise(Platform::Linux, &linux_readings(), &needs(&Inputs { radius: None, recv_buffer: Some(2097152), ..inputs() }));
        assert!(check(&none, "rmem").is_err() && check(&none, "recv_buffer_bytes").is_err());
        Ok(())
    }

    #[test]
    fn test_macos_with_recv_buffer_bytes() -> Result<(), String> {
        let report = advise(Platform::MacOs, &bsd_readings("kern.ipc.somaxconn"), &needs(&Inputs { radius: Some((4096, 1)), recv_buffer: Some(4194304), ..inputs() }));
        let limit = check(&report, "kern.ipc.maxsockbuf")?;
        assert_eq!((limit.suggested.as_deref(), limit.now.as_deref()), (Some("8388608"), Some("sysctl -w kern.ipc.maxsockbuf=8388608")));
        assert!(check(&report, "net.inet.udp.recvspace").is_err(), "the default is not used");
        assert!(check(&report, "[radius] recv_buffer_bytes").is_ok());
        Ok(())
    }

    #[test]
    fn test_sockets_on_macos() -> Result<(), String> {
        let needs = needs(&Inputs { radius: Some((4096, 4)), ..inputs() });
        let report = advise(Platform::MacOs, &bsd_readings("kern.ipc.somaxconn"), &needs);
        let sockets = check(&report, "[radius] sockets")?;
        assert!(!sockets.ok && sockets.why.contains("must be 1"), "{sockets:?}");
        assert_eq!(sockets.suggested.as_deref(), Some("1"));
        let text = report.to_string();
        assert!(text.starts_with("rradius tuning advice for macOS\n\n"), "{text}");
        assert!(text.contains("dropped because a receive buffer was full, since the system started: 12\n  Raise the receive buffers"), "{text}");
        assert!(!text.contains("before reaching a socket"), "Linux only: {text}");
        let recvspace = check(&report, "net.inet.udp.recvspace")?;
        assert_eq!(recvspace.persist.as_deref(), Some("/etc/sysctl.conf: net.inet.udp.recvspace = 4194304"));
        assert_eq!(check(&report, "kern.ipc.maxsockbuf")?.suggested.as_deref(), Some("8388608"));
        let position = |name: &str| report.checks.iter().position(|check| check.title.contains(name));
        assert!(position("kern.ipc.maxsockbuf") < position("net.inet.udp.recvspace"), "the limit first");
        assert!(check(&report, "RLIMIT_NOFILE")?.persist.as_deref().is_some_and(|persist| persist.contains("NumberOfFiles")));
        assert_eq!(check(&report, "kern.maxfilesperproc")?.suggested.as_deref(), Some("65536"));
        assert!(!text.contains("pin_threads"), "Linux only: {text}");
        Ok(())
    }

    #[test]
    fn test_freebsd() -> Result<(), String> {
        let report = advise(Platform::FreeBsd, &bsd_readings("kern.ipc.soacceptqueue"), &needs(&inputs()));
        let queue = check(&report, "kern.ipc.soacceptqueue")?;
        assert_eq!(queue.now.as_deref(), Some("sysctl -w kern.ipc.soacceptqueue=1024"));
        assert_eq!(queue.persist.as_deref(), Some("/etc/sysctl.conf: kern.ipc.soacceptqueue = 1024"));
        assert!(check(&report, "RLIMIT_NOFILE")?.persist.as_deref().is_some_and(|persist| persist.contains("/etc/login.conf") && persist.contains("openfiles=65536")));
        let text = report.to_string();
        assert!(text.starts_with("rradius tuning advice for FreeBSD\n(this advice follows the FreeBSD documentation and has not been tested on FreeBSD)\n"), "{text}");
        // Without TCP listeners there is no listen queue check.
        let quiet = advise(Platform::FreeBsd, &bsd_readings("kern.ipc.soacceptqueue"), &needs(&Inputs { tacacs_connections: None, extension_connections: Vec::new(), ..inputs() }));
        assert!(check(&quiet, "soacceptqueue").is_err());
        Ok(())
    }

    #[test]
    fn test_defaults() {
        let defaults = Inputs::defaults();
        assert_eq!(defaults.tacacs_connections, Some(1024));
        assert!(defaults.radius.is_some_and(|(inflight, sockets)| inflight == 4096 && sockets >= 1));
    }
}
