//! Runs the real executable against responders the tests script.
#![allow(dead_code)]

use rradius_bench::packet::{write_response, Signer, ACCESS_ACCEPT, ACCESS_REJECT, ACCOUNTING_RESPONSE, MAX_PACKET};
use serde_json::Value;
use std::net::{IpAddr, SocketAddr, TcpListener, UdpSocket};
use std::ops::RangeInclusive;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A port nothing listens on, for a test that needs a connection or a datagram to be refused.
///
/// Binding port 0 and letting go hands back a port from the range the system gives out for
/// every other socket the tests open at the same time, so another test can be handed it
/// before this one uses it. These ports are from below that range (above 32767 on Linux, above
/// 49151 on macOS), which nothing here asks for, and a port is taken only when neither a UDP
/// socket nor a TCP listener can be bound to it. A test program counts up from a start of
/// its own, so two of them running side by side do not look at the same ports first.
/// FreeBSD gives out 10000 to 65535, which holds these ports: there only the sockets of
/// [`udp_socket`] are kept off them, and a TCP connection may still be given one.
pub fn closed_port() -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let start = 20_000 + (std::process::id() % 8_000) as u16;
    loop {
        let port = start + NEXT.fetch_add(1, Ordering::Relaxed) % 2_000;
        if UdpSocket::bind(("127.0.0.1", port)).is_ok() && TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

/// The ports of the lanes `rradius_core::test_support::server::free_port` hands out, which
/// hold those of [`closed_port`] too.
const LANE_PORTS: RangeInclusive<u16> = 20_000..=31_999;

/// A UDP socket on `ip`, on a port the system picks outside the lanes: on one of those it
/// could be the target of its own datagrams, or receive what another test sends to the server
/// it expects there. A copy of `udp_socket` in `rradius_core::test_support::server`, as this
/// crate does not depend on that one.
pub fn udp_socket(ip: IpAddr) -> std::io::Result<UdpSocket> {
    for _ in 0..1000 {
        // A socket that is refused is closed at once: holding them could use up the open files.
        let socket = UdpSocket::bind((ip, 0))?;
        if !LANE_PORTS.contains(&socket.local_addr()?.port()) {
            return Ok(socket);
        }
    }
    Err(std::io::Error::other(format!("no UDP port outside {}-{} in 1000 tries", LANE_PORTS.start(), LANE_PORTS.end())))
}

pub const SECRET: &str = "test-secret";

pub fn signer() -> Signer {
    Signer::new(SECRET.as_bytes()).unwrap()
}

/// A correctly signed response. Access-Requests are accepted, accounting is acknowledged.
pub fn answer(request: &[u8]) -> Vec<u8> {
    let code = if request[0] == 4 { ACCOUNTING_RESPONSE } else { ACCESS_ACCEPT };
    respond(request, code, &[])
}

pub fn respond(request: &[u8], code: u8, attributes: &[(u8, &[u8])]) -> Vec<u8> {
    let mut out = [0u8; MAX_PACKET];
    let length = write_response(&mut out, request, code, attributes, code != ACCOUNTING_RESPONSE, &signer());
    out[..length].to_vec()
}

pub fn reject(request: &[u8]) -> Vec<u8> {
    respond(request, ACCESS_REJECT, &[])
}

/// A responder that calls `handle` for each packet, with a socket to answer on.
pub struct Responder {
    pub address: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// Every request received.
    pub received: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Responder {
    /// `handle` gets the request and how many came before it.
    pub fn start(mut handle: impl FnMut(&UdpSocket, SocketAddr, &[u8], usize) + Send + 'static) -> Responder {
        let socket = udp_socket([127, 0, 0, 1].into()).unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let address = socket.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let received = Arc::new(Mutex::new(Vec::new()));
        let (stopping, log) = (stop.clone(), received.clone());
        let thread = std::thread::spawn(move || {
            let mut buffer = [0u8; MAX_PACKET];
            let mut count = 0;
            while !stopping.load(Ordering::Relaxed) {
                if let Ok((length, from)) = socket.recv_from(&mut buffer) {
                    log.lock().unwrap().push(buffer[..length].to_vec());
                    handle(&socket, from, &buffer[..length], count);
                    count += 1;
                }
            }
        });
        Responder { address, stop, handle: Some(thread), received }
    }

    /// Answers everything correctly.
    pub fn answering() -> Responder {
        Responder::start(|socket, from, request, _| {
            socket.send_to(&answer(request), from).unwrap();
        })
    }

    /// Every request received that parses; one that does not is left out, which the
    /// counts the tests check would show.
    pub fn requests(&self) -> Vec<Request> {
        self.received.lock().unwrap().iter().filter_map(|bytes| parse_request(bytes).ok()).collect()
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A RADIUS request as the tests read it.
pub struct Request {
    pub code: u8,
    pub identifier: u8,
    pub authenticator: [u8; 16],
    /// Type and value of each attribute, in order.
    pub attributes: Vec<(u8, Vec<u8>)>,
    /// The packet up to its Length.
    pub bytes: Vec<u8>,
}

impl Request {
    /// The value of the first attribute of `kind`.
    pub fn attribute(&self, kind: u8) -> Option<&[u8]> {
        self.attributes.iter().find(|(found, _)| *found == kind).map(|(_, value)| value.as_slice())
    }

    /// Whether the Request Authenticator is MD5 of the packet, with 16 zero octets in its
    /// place, followed by `secret`, as an Accounting-Request's must be.
    pub fn verify_accounting_request(&self, secret: &[u8]) -> bool {
        use md5::{Digest, Md5};
        let mut packet = self.bytes.clone();
        packet[4..20].fill(0);
        let mut hash = Md5::new();
        hash.update(&packet);
        hash.update(secret);
        hash.finalize()[..] == self.authenticator
    }
}

/// Reads a RADIUS packet: the header, then attributes up to its Length. The unit tests in
/// `src/packet.rs` have the same reader, as they cannot use this module; keep the two alike.
pub fn parse_request(bytes: &[u8]) -> Result<Request, String> {
    let header = bytes.get(..20).ok_or("shorter than a RADIUS header")?;
    let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
    let body = bytes.get(20..length).ok_or_else(|| format!("Length {length} is not within the {} octets received", bytes.len()))?;
    let mut authenticator = [0u8; 16];
    authenticator.copy_from_slice(&header[4..20]);
    let (mut attributes, mut at) = (Vec::new(), 0);
    while at < body.len() {
        let (kind, size) = match body.get(at..at + 2) {
            Some([kind, size]) => (*kind, usize::from(*size)),
            _ => return Err(format!("attribute header cut short at {at}")),
        };
        let value = body.get(at + 2..at + size).filter(|_| size >= 2).ok_or_else(|| format!("attribute {kind} of length {size} at {at} does not fit"))?;
        attributes.push((kind, value.to_vec()));
        at += size;
    }
    Ok(Request { code: header[0], identifier: header[1], authenticator, attributes, bytes: bytes[..length].to_vec() })
}

pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| panic!("{e}: {} / {}", self.stdout, self.stderr))
    }
}

pub fn bench(args: &[String]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_rradius-bench")).args(args).output().unwrap();
    Output {
        code: output.status.code().unwrap(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// `run` against an address on both ports, with `--json` and the test secret.
pub fn run_against(address: SocketAddr, args: &[&str]) -> Output {
    let port = address.port().to_string();
    let mut all: Vec<String> = ["run", "--json", "--server", "127.0.0.1", "--secret", SECRET, "--auth-port", &port, "--acct-port", &port].iter().map(|s| s.to_string()).collect();
    all.extend(args.iter().map(|s| s.to_string()));
    bench(&all)
}

pub fn step(json: &Value, load: usize, step: usize) -> &Value {
    &json["loads"][load]["steps"][step]
}

/// Waits for `check`, for tests where something happens on another thread.
pub fn wait_for(mut check: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}
