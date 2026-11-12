//! TACACS+ (RFC 8907) for the load: the packets the four `tacacs-*` kinds send, how a reply is
//! judged, and one TCP connection that carries one request at a time.
use crate::packet::Step;
use crate::radsec::Received;
use md5::{Digest, Md5};
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

pub const HEADER: usize = 12;
/// The longest body a reply may say it has.
pub const MAX_BODY: usize = 64 * 1024;
const MAJOR_VERSION: u8 = 0xc;
/// Minor version 0, for ASCII logins, authorization and accounting.
const VERSION_DEFAULT: u8 = 0xc0;
/// Minor version 1, which PAP authentication uses (RFC 8907 section 5.4.2).
const VERSION_ONE: u8 = 0xc1;
const UNENCRYPTED_FLAG: u8 = 0x01;

const TYPE_AUTHEN: u8 = 0x01;
const TYPE_AUTHOR: u8 = 0x02;
const TYPE_ACCT: u8 = 0x03;

const ACTION_LOGIN: u8 = 0x01;
const AUTHEN_TYPE_ASCII: u8 = 0x01;
const AUTHEN_TYPE_PAP: u8 = 0x02;
const SERVICE_LOGIN: u8 = 0x01;
const METHOD_TACACSPLUS: u8 = 0x06;
const ACCT_FLAG_START: u8 = 0x02;

const AUTHEN_PASS: u8 = 0x01;
const AUTHEN_FAIL: u8 = 0x02;
const AUTHEN_GETUSER: u8 = 0x04;
const AUTHEN_GETPASS: u8 = 0x05;
const AUTHOR_PASS_ADD: u8 = 0x01;
const AUTHOR_PASS_REPL: u8 = 0x02;
const AUTHOR_FAIL: u8 = 0x10;
const ACCT_SUCCESS: u8 = 0x01;
const ACCT_ERROR: u8 = 0x02;

/// The port field of every request.
const PORT: &[u8] = b"rradius-bench";

#[cfg(target_os = "linux")]
const EINPROGRESS: i32 = 115;
#[cfg(not(target_os = "linux"))]
const EINPROGRESS: i32 = 36;

/// What a request says beyond its kind.
pub struct Request<'a> {
    pub user: &'a str,
    pub password: &'a str,
    /// Accounting `task_id`.
    pub task: u64,
    /// Accounting `start_time`, in Unix seconds.
    pub start_time: u64,
}

/// What a reply means for its request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Accept,
    Reject,
    Accounting,
    /// An ASCII login asks for the name.
    GetUser,
    /// An ASCII login asks for the password.
    GetPass,
}

/// What reply a request is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expect {
    pub step: Step,
    pub session: u32,
    /// The sequence number of the request.
    pub seq: u8,
    /// Of an ASCII login: 0 before the name was sent, 1 before the password, 2 after.
    pub stage: u8,
}

/// XORs `data` with the pseudo-pad of RFC 8907 section 4.5, which both hides and reveals a body.
pub fn crypt(data: &mut [u8], secret: &[u8], session: u32, version: u8, seq: u8) {
    let mut previous: Option<[u8; 16]> = None;
    for chunk in data.chunks_mut(16) {
        let mut hasher = Md5::new();
        hasher.update(session.to_be_bytes());
        hasher.update(secret);
        hasher.update([version, seq]);
        if let Some(previous) = previous {
            hasher.update(previous);
        }
        let pad: [u8; 16] = hasher.finalize().into();
        for (byte, mask) in chunk.iter_mut().zip(pad) {
            *byte ^= mask;
        }
        previous = Some(pad);
    }
}

fn version_of(step: Step) -> u8 {
    if step == Step::TacacsPap { VERSION_ONE } else { VERSION_DEFAULT }
}

fn type_of(step: Step) -> u8 {
    match step {
        Step::TacacsAuthor => TYPE_AUTHOR,
        Step::TacacsAcct => TYPE_ACCT,
        _ => TYPE_AUTHEN,
    }
}

/// A whole packet: the header and `body`, hidden with `secret`.
fn packet(step: Step, session: u32, seq: u8, mut body: Vec<u8>, secret: &[u8]) -> Result<Vec<u8>, String> {
    let version = version_of(step);
    let length = u32::try_from(body.len()).map_err(|_| "a request body too long".to_string())?;
    crypt(&mut body, secret, session, version, seq);
    let mut out = Vec::with_capacity(HEADER + body.len());
    out.extend_from_slice(&[version, type_of(step), seq, 0]);
    out.extend_from_slice(&session.to_be_bytes());
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

fn short(field: &[u8], name: &str) -> Result<u8, String> {
    u8::try_from(field.len()).map_err(|_| format!("{name} of {} octets does not fit in a TACACS+ request", field.len()))
}

/// The user, port and rem_addr lengths, then the args: what authorization and accounting share.
fn args_body(mut body: Vec<u8>, user: &[u8], args: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    body.extend_from_slice(&[short(user, "the name")?, short(PORT, "the port")?, 0, short_count(args)?]);
    for arg in args {
        body.push(short(arg, "an argument")?);
    }
    body.extend_from_slice(user);
    body.extend_from_slice(PORT);
    for arg in args {
        body.extend_from_slice(arg);
    }
    Ok(body)
}

fn short_count(args: &[Vec<u8>]) -> Result<u8, String> {
    u8::try_from(args.len()).map_err(|_| "too many arguments".to_string())
}

/// The first packet of a request: sequence number 1.
pub fn start(step: Step, session: u32, request: &Request<'_>, secret: &[u8]) -> Result<Vec<u8>, String> {
    let user = request.user.as_bytes();
    let body = match step {
        Step::TacacsPap => {
            let password = request.password.as_bytes();
            let mut body = vec![ACTION_LOGIN, 1, AUTHEN_TYPE_PAP, SERVICE_LOGIN, short(user, "the name")?, short(PORT, "the port")?, 0, short(password, "the password")?];
            body.extend_from_slice(user);
            body.extend_from_slice(PORT);
            body.extend_from_slice(password);
            body
        }
        Step::TacacsAscii => {
            let mut body = vec![ACTION_LOGIN, 1, AUTHEN_TYPE_ASCII, SERVICE_LOGIN, 0, short(PORT, "the port")?, 0, 0];
            body.extend_from_slice(PORT);
            body
        }
        Step::TacacsAuthor => {
            let args = [b"service=shell".to_vec(), b"cmd=show".to_vec(), b"cmd-arg=version".to_vec()];
            args_body(vec![METHOD_TACACSPLUS, 15, AUTHEN_TYPE_ASCII, SERVICE_LOGIN], user, &args)?
        }
        Step::TacacsAcct => {
            let args = [format!("task_id={}", request.task).into_bytes(), format!("start_time={}", request.start_time).into_bytes(), b"service=shell".to_vec()];
            args_body(vec![ACCT_FLAG_START, METHOD_TACACSPLUS, 15, AUTHEN_TYPE_ASCII, SERVICE_LOGIN], user, &args)?
        }
        other => return Err(format!("{} is not a TACACS+ kind", other.name())),
    };
    packet(step, session, 1, body, secret)
}

/// An Authentication CONTINUE carrying `message` (RFC 8907 section 5.3).
pub fn continue_with(session: u32, seq: u8, message: &[u8], secret: &[u8]) -> Result<Vec<u8>, String> {
    let length = u16::try_from(message.len()).map_err(|_| "a CONTINUE message too long".to_string())?;
    let mut body = length.to_be_bytes().to_vec();
    body.extend_from_slice(&[0, 0, 0]);
    body.extend_from_slice(message);
    packet(Step::TacacsAscii, session, seq, body, secret)
}

/// The body length a header says, or None for a header too short.
pub fn body_length(header: &[u8]) -> Option<usize> {
    let bytes: [u8; 4] = header.get(8..HEADER)?.try_into().ok()?;
    usize::try_from(u32::from_be_bytes(bytes)).ok()
}

fn length16(body: &[u8], at: usize) -> usize {
    match body.get(at..at + 2) {
        Some(&[high, low]) => usize::from(u16::from_be_bytes([high, low])),
        _ => 0,
    }
}

/// Judges a whole reply against what its request waits for. An error reads "kind: detail".
pub fn judge(reply: &[u8], expect: Expect, secret: &[u8]) -> Result<Verdict, String> {
    let Some(header) = reply.get(..HEADER) else { return Err("malformed: a reply shorter than its header".to_string()) };
    if header[0] >> 4 != MAJOR_VERSION {
        return Err(format!("version: major version {:#x}", header[0] >> 4));
    }
    if header[1] != type_of(expect.step) {
        return Err(format!("type: a reply of type {} to a request of type {}", header[1], type_of(expect.step)));
    }
    let wanted = expect.seq.wrapping_add(1);
    if header[2] != wanted {
        return Err(format!("sequence number: {}, expected {wanted}", header[2]));
    }
    if header[3] & UNENCRYPTED_FLAG != 0 {
        return Err("unencrypted: a reply with the unencrypted flag".to_string());
    }
    let session = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    if session != expect.session {
        return Err(format!("session id: {session}, expected {}", expect.session));
    }
    let mut body = reply[HEADER..].to_vec();
    if body_length(header) != Some(body.len()) {
        return Err("malformed: the header's length differs from the body".to_string());
    }
    crypt(&mut body, secret, session, header[0], header[2]);
    let (status, length) = match expect.step {
        Step::TacacsAuthor => {
            let count = usize::from(body.get(1).copied().unwrap_or(0));
            let args: usize = body.get(6..6 + count).map_or(0, |lengths| lengths.iter().map(|length| usize::from(*length)).sum());
            (body.first().copied(), 6 + count + length16(&body, 2) + length16(&body, 4) + args)
        }
        Step::TacacsAcct => (body.get(4).copied(), 5 + length16(&body, 0) + length16(&body, 2)),
        _ => (body.first().copied(), 6 + length16(&body, 2) + length16(&body, 4)),
    };
    let Some(status) = status.filter(|_| length == body.len()) else {
        return Err("lengths: the fields of the reply do not add up to its length, so the shared secret may be wrong".to_string());
    };
    let verdict = match (expect.step, status) {
        (Step::TacacsPap | Step::TacacsAscii, AUTHEN_PASS) => Verdict::Accept,
        (Step::TacacsPap | Step::TacacsAscii, AUTHEN_FAIL) => Verdict::Reject,
        (Step::TacacsAscii, AUTHEN_GETUSER) if expect.stage == 0 => Verdict::GetUser,
        (Step::TacacsAscii, AUTHEN_GETPASS) if expect.stage == 1 => Verdict::GetPass,
        (Step::TacacsAuthor, AUTHOR_PASS_ADD | AUTHOR_PASS_REPL) => Verdict::Accept,
        (Step::TacacsAuthor, AUTHOR_FAIL) => Verdict::Reject,
        (Step::TacacsAcct, ACCT_SUCCESS) => Verdict::Accounting,
        (Step::TacacsAcct, ACCT_ERROR) => Verdict::Reject,
        (_, status) => return Err(format!("status: {status:#04x} at this point of a {}", expect.step.name())),
    };
    Ok(verdict)
}

/// `--tacacs-connect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connect {
    /// One connection per socket for the whole run.
    Keep,
    /// A new connection per request, closed after its last reply.
    PerRequest,
}

impl Connect {
    pub fn parse(text: &str) -> Result<Connect, String> {
        match text {
            "keep" => Ok(Connect::Keep),
            "per-request" => Ok(Connect::PerRequest),
            other => Err(format!("--tacacs-connect {other}: keep or per-request")),
        }
    }
}

/// Where TACACS+ connections go, and how.
pub struct TacacsTarget {
    pub address: SocketAddr,
    /// `--bind`: the source address.
    pub bind: Option<IpAddr>,
    pub timeout: Duration,
    pub connect: Connect,
    pub secret: Vec<u8>,
}

/// One socket of a TACACS+ load: a connection, or none between the requests of `per-request`.
pub struct TacacsLink {
    stream: Option<mio::net::TcpStream>,
    /// A connection still being made: nothing can be written yet.
    connecting: bool,
    outgoing: Vec<u8>,
    received: Vec<u8>,
    /// Whether a whole reply came on this connection.
    answered: bool,
}

impl TacacsLink {
    /// A socket with no connection yet.
    pub fn idle() -> TacacsLink {
        TacacsLink { stream: None, connecting: false, outgoing: Vec::new(), received: Vec::new(), answered: false }
    }

    /// Connects within the target's timeout, waiting for it: for `keep`.
    pub fn connect(target: &TacacsTarget) -> Result<TacacsLink, String> {
        let socket = new_socket(target).map_err(|e| format!("connecting to {}: {e}", target.address))?;
        socket.connect_timeout(&target.address.into(), target.timeout).map_err(|e| format!("connecting to {}: {e}", target.address))?;
        socket.set_nonblocking(true).map_err(|e| format!("connecting to {}: {e}", target.address))?;
        let stream: std::net::TcpStream = socket.into();
        Ok(TacacsLink { stream: Some(mio::net::TcpStream::from_std(stream)), connecting: false, outgoing: Vec::new(), received: Vec::new(), answered: false })
    }

    /// Starts a connection without waiting for it: for `per-request`.
    pub fn open(&mut self, target: &TacacsTarget) -> io::Result<()> {
        let socket = new_socket(target)?;
        socket.set_nonblocking(true)?;
        match socket.connect(&target.address.into()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock || error.raw_os_error() == Some(EINPROGRESS) => {}
            Err(error) => return Err(error),
        }
        let stream: std::net::TcpStream = socket.into();
        self.stream = Some(mio::net::TcpStream::from_std(stream));
        self.connecting = true;
        self.outgoing.clear();
        self.received.clear();
        self.answered = false;
        Ok(())
    }

    /// Whether the server answered anything on this connection: one it closed before that
    /// may have refused it, as a server at its connection limit does.
    pub fn answered(&self) -> bool {
        self.answered
    }

    pub fn is_open(&self) -> bool {
        self.stream.is_some()
    }

    pub fn source(&mut self) -> Option<&mut mio::net::TcpStream> {
        self.stream.as_mut()
    }

    /// Ends the connection, if there is one, and forgets what it held.
    pub fn close(&mut self) {
        self.stream = None;
        self.connecting = false;
        self.outgoing.clear();
        self.received.clear();
    }

    /// Queues `packet` and writes what it can. True when everything went out.
    pub fn send(&mut self, packet: &[u8]) -> io::Result<bool> {
        self.outgoing.extend_from_slice(packet);
        self.flush()
    }

    /// Writes what waits, once the connection is made. True when nothing is left.
    pub fn flush(&mut self) -> io::Result<bool> {
        let Some(stream) = self.stream.as_mut() else { return Err(io::Error::new(io::ErrorKind::NotConnected, "no connection")) };
        if self.connecting {
            if let Some(error) = stream.take_error()? {
                return Err(error);
            }
            match stream.peer_addr() {
                Ok(_) => self.connecting = false,
                Err(error) if error.kind() == io::ErrorKind::NotConnected => return Ok(false),
                Err(error) => return Err(error),
            }
        }
        while !self.outgoing.is_empty() {
            match stream.write(&self.outgoing) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "the connection takes no more")),
                Ok(written) => {
                    self.outgoing.drain(..written);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }

    /// Reads everything there is, and the whole packets in it.
    pub fn receive(&mut self) -> Received {
        let mut received = Received { packets: Vec::new(), closed: None };
        let Some(stream) = self.stream.as_mut() else { return received };
        if self.connecting {
            // Readable before writable: the connection failed, or it is made and the reply
            // waits for the next writable event to send the request.
            match stream.take_error() {
                Ok(None) => {}
                Ok(Some(error)) | Err(error) => received.closed = Some(format!("connecting: {error}")),
            }
            return received;
        }
        let mut buffer = [0u8; 4096];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => {
                    received.closed = Some("closed: the server closed the connection".to_string());
                    break;
                }
                Ok(read) => self.received.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    received.closed = Some(format!("reading: {error}"));
                    break;
                }
            }
        }
        while let Some(length) = body_length(&self.received) {
            if length > MAX_BODY {
                received.closed = Some(format!("malformed: a reply that says it has {length} octets"));
                break;
            }
            if self.received.len() < HEADER + length {
                break;
            }
            received.packets.push(self.received.drain(..HEADER + length).collect());
            self.answered = true;
        }
        received
    }
}

fn new_socket(target: &TacacsTarget) -> io::Result<socket2::Socket> {
    let socket = socket2::Socket::new(socket2::Domain::for_address(target.address), socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    // Requests are small and each waits for its reply.
    socket.set_tcp_nodelay(true)?;
    if let Some(bind) = target.bind {
        socket.bind(&SocketAddr::new(bind, 0).into())?;
    }
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"tacacs-secret";

    /// A reply to `expect` with `body`, from a server that knows `secret`.
    fn reply(expect: Expect, seq: u8, session: u32, body: &[u8], secret: &[u8]) -> Result<Vec<u8>, String> {
        let mut out = packet(expect.step, session, seq, body.to_vec(), secret)?;
        // A server answers with the version of the request.
        out[0] = version_of(expect.step);
        Ok(out)
    }

    fn expect(step: Step, stage: u8) -> Expect {
        Expect { step, session: 7, seq: 1, stage }
    }

    /// A request's body, revealed again.
    fn reveal(packet: &[u8]) -> Result<Vec<u8>, String> {
        let header = packet.get(..HEADER).ok_or("short")?;
        let mut body = packet[HEADER..].to_vec();
        crypt(&mut body, SECRET, u32::from_be_bytes([header[4], header[5], header[6], header[7]]), header[0], header[2]);
        Ok(body)
    }

    #[test]
    fn test_crypt_round_trip() {
        let original: Vec<u8> = (0..=40).collect();
        let mut data = original.clone();
        crypt(&mut data, SECRET, 9, VERSION_DEFAULT, 1);
        assert_ne!(data, original);
        crypt(&mut data, SECRET, 9, VERSION_DEFAULT, 1);
        assert_eq!(data, original);
    }

    #[test]
    fn test_pseudo_pad_follows_rfc() {
        // MD5_1 = MD5(session_id, key, version, seq_no); MD5_2 adds MD5_1.
        let mut data = [0u8; 20];
        crypt(&mut data, SECRET, 0x0102_0304, VERSION_DEFAULT, 1);
        let mut first = Md5::new();
        first.update([1, 2, 3, 4]);
        first.update(SECRET);
        first.update([VERSION_DEFAULT, 1]);
        let first: [u8; 16] = first.finalize().into();
        let mut second = Md5::new();
        second.update([1, 2, 3, 4]);
        second.update(SECRET);
        second.update([VERSION_DEFAULT, 1]);
        second.update(first);
        let second: [u8; 16] = second.finalize().into();
        assert_eq!(data[..16], first);
        assert_eq!(data[16..], second[..4]);
    }

    #[test]
    fn test_requests() -> Result<(), String> {
        let request = Request { user: "alice", password: "pw", task: 5, start_time: 100 };
        let pap = start(Step::TacacsPap, 7, &request, SECRET)?;
        assert_eq!(&pap[..4], &[VERSION_ONE, TYPE_AUTHEN, 1, 0]);
        assert_eq!(body_length(&pap), Some(pap.len() - HEADER));
        let body = reveal(&pap)?;
        assert_eq!(&body[..8], &[ACTION_LOGIN, 1, AUTHEN_TYPE_PAP, SERVICE_LOGIN, 5, 13, 0, 2]);
        assert_eq!(&body[8..], [b"alice".as_slice(), PORT, b"pw"].concat().as_slice());

        let ascii = reveal(&start(Step::TacacsAscii, 7, &request, SECRET)?)?;
        assert_eq!(&ascii[..8], &[ACTION_LOGIN, 1, AUTHEN_TYPE_ASCII, SERVICE_LOGIN, 0, 13, 0, 0]);

        let author = reveal(&start(Step::TacacsAuthor, 7, &request, SECRET)?)?;
        assert_eq!(&author[..8], &[METHOD_TACACSPLUS, 15, AUTHEN_TYPE_ASCII, SERVICE_LOGIN, 5, 13, 0, 3]);
        assert_eq!(&author[8..11], &[13, 8, 15]);
        assert!(author.ends_with(b"service=shellcmd=showcmd-arg=version"));

        let acct = reveal(&start(Step::TacacsAcct, 7, &request, SECRET)?)?;
        assert_eq!(&acct[..9], &[ACCT_FLAG_START, METHOD_TACACSPLUS, 15, AUTHEN_TYPE_ASCII, SERVICE_LOGIN, 5, 13, 0, 3]);
        assert!(acct.ends_with(b"task_id=5start_time=100service=shell"));

        let more = continue_with(7, 3, b"alice", SECRET)?;
        assert_eq!(&more[..3], &[VERSION_DEFAULT, TYPE_AUTHEN, 3]);
        assert_eq!(reveal(&more)?, [&[0, 5, 0, 0, 0][..], b"alice"].concat());
        Ok(())
    }

    /// The rows of the spec's "replies judged" example.
    #[test]
    fn test_replies_judged() -> Result<(), String> {
        let pap = expect(Step::TacacsPap, 0);
        let pass = [AUTHEN_PASS, 0, 0, 0, 0, 0];
        assert_eq!(judge(&reply(pap, 2, 7, &pass, SECRET)?, pap, SECRET), Ok(Verdict::Accept));
        assert!(judge(&reply(pap, 2, 8, &pass, SECRET)?, pap, SECRET).is_err_and(|e| e.starts_with("session id:")));
        assert!(judge(&reply(pap, 4, 7, &pass, SECRET)?, pap, SECRET).is_err_and(|e| e.starts_with("sequence number:")));
        assert!(judge(&reply(pap, 2, 7, &[AUTHEN_PASS, 0, 0, 5, 0, 0], SECRET)?, pap, SECRET).is_err_and(|e| e.starts_with("lengths:")));
        let acct = expect(Step::TacacsAcct, 0);
        assert_eq!(judge(&reply(acct, 2, 7, &[0, 0, 0, 0, ACCT_ERROR], SECRET)?, acct, SECRET), Ok(Verdict::Reject));
        let author = expect(Step::TacacsAuthor, 0);
        assert!(judge(&reply(author, 2, 7, &[0x11, 0, 0, 0, 0, 0], SECRET)?, author, SECRET).is_err_and(|e| e.starts_with("status:")));
        Ok(())
    }

    #[test]
    fn test_statuses() -> Result<(), String> {
        let cases: [(Step, u8, &[u8], Verdict); 8] = [
            (Step::TacacsPap, 0, &[AUTHEN_FAIL, 0, 0, 0, 0, 0], Verdict::Reject),
            (Step::TacacsAscii, 0, &[AUTHEN_GETUSER, 0, 0, 1, 0, 0, b':'], Verdict::GetUser),
            (Step::TacacsAscii, 1, &[AUTHEN_GETPASS, 1, 0, 1, 0, 0, b':'], Verdict::GetPass),
            (Step::TacacsAscii, 2, &[AUTHEN_PASS, 0, 0, 0, 0, 0], Verdict::Accept),
            (Step::TacacsAuthor, 0, &[AUTHOR_PASS_ADD, 1, 0, 0, 0, 0, 3, b'a', b'=', b'b'], Verdict::Accept),
            (Step::TacacsAuthor, 0, &[AUTHOR_PASS_REPL, 0, 0, 0, 0, 0], Verdict::Accept),
            (Step::TacacsAuthor, 0, &[AUTHOR_FAIL, 0, 0, 0, 0, 0], Verdict::Reject),
            (Step::TacacsAcct, 0, &[0, 0, 0, 0, ACCT_SUCCESS], Verdict::Accounting),
        ];
        for (step, stage, body, verdict) in cases {
            let wanted = expect(step, stage);
            assert_eq!(judge(&reply(wanted, 2, 7, body, SECRET)?, wanted, SECRET), Ok(verdict), "{step:?} {body:?}");
        }
        Ok(())
    }

    #[test]
    fn test_ascii_prompts_only_in_turn() -> Result<(), String> {
        let at_password = expect(Step::TacacsAscii, 0);
        let getpass = [AUTHEN_GETPASS, 1, 0, 0, 0, 0];
        assert!(judge(&reply(at_password, 2, 7, &getpass, SECRET)?, at_password, SECRET).is_err_and(|e| e.starts_with("status:")));
        let done = expect(Step::TacacsAscii, 2);
        let getuser = [AUTHEN_GETUSER, 0, 0, 0, 0, 0];
        assert!(judge(&reply(done, 2, 7, &getuser, SECRET)?, done, SECRET).is_err_and(|e| e.starts_with("status:")));
        Ok(())
    }

    #[test]
    fn test_lengths_that_disagree() -> Result<(), String> {
        let author = expect(Step::TacacsAuthor, 0);
        // One argument of 3 octets said, none there.
        assert!(judge(&reply(author, 2, 7, &[AUTHOR_PASS_ADD, 1, 0, 0, 0, 0, 3], SECRET)?, author, SECRET).is_err_and(|e| e.starts_with("lengths:")));
        let acct = expect(Step::TacacsAcct, 0);
        assert!(judge(&reply(acct, 2, 7, &[0, 2, 0, 0, ACCT_SUCCESS], SECRET)?, acct, SECRET).is_err_and(|e| e.starts_with("lengths:")));
        let pap = expect(Step::TacacsPap, 0);
        assert!(judge(&reply(pap, 2, 7, &[AUTHEN_PASS, 0, 0, 0], SECRET)?, pap, SECRET).is_err_and(|e| e.starts_with("lengths:")));
        Ok(())
    }

    #[test]
    fn test_wrong_secret_is_caught() -> Result<(), String> {
        let pap = expect(Step::TacacsPap, 0);
        let mut caught = 0;
        // Garbage nearly never adds up; try many sessions to be sure it is not luck.
        for session in 0..200u32 {
            let wanted = Expect { session, ..pap };
            if judge(&reply(wanted, 2, session, &[AUTHEN_PASS, 0, 0, 0, 0, 0], b"other")?, wanted, SECRET).is_err() {
                caught += 1;
            }
        }
        assert!(caught >= 199, "{caught} of 200");
        Ok(())
    }

    #[test]
    fn test_header_checks() -> Result<(), String> {
        let pap = expect(Step::TacacsPap, 0);
        let good = reply(pap, 2, 7, &[AUTHEN_PASS, 0, 0, 0, 0, 0], SECRET)?;
        let mut wrong_type = good.clone();
        wrong_type[1] = TYPE_ACCT;
        assert!(judge(&wrong_type, pap, SECRET).is_err_and(|e| e.starts_with("type:")));
        let mut unencrypted = good.clone();
        unencrypted[3] = UNENCRYPTED_FLAG;
        assert!(judge(&unencrypted, pap, SECRET).is_err_and(|e| e.starts_with("unencrypted:")));
        let mut version = good.clone();
        version[0] = 0xb0;
        assert!(judge(&version, pap, SECRET).is_err_and(|e| e.starts_with("version:")));
        assert!(judge(&good[..HEADER - 1], pap, SECRET).is_err_and(|e| e.starts_with("malformed:")));
        Ok(())
    }

    #[test]
    fn test_connect_parse() {
        assert_eq!(Connect::parse("keep"), Ok(Connect::Keep));
        assert_eq!(Connect::parse("per-request"), Ok(Connect::PerRequest));
        assert!(Connect::parse("sometimes").is_err());
    }
}
