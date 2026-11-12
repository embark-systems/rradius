use tokio::net::TcpListener;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use std::io;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use md5::{Digest, Md5};
use crate::auth_chain::{AuthChain, AuthChains, Credentials, Secret};
use crate::radius::{accept_next, DiscardLog};
use tokio::sync::Semaphore;
use crate::clients::{Client, ClientTable};
use crate::metrics::{Metrics, Open};
use crate::reload::Live;
use crate::lua_engine::{LuaAuthEngine, LuaRequest};
use crate::common::AttrValue;
use std::sync::Arc;
use crate::tacacs_transport::{PeerCheck, TacacsTlsServed};

pub const HEADER_LEN: usize = 12;
pub const MAJOR_VERSION: u8 = 0xc;
pub const UNENCRYPTED_FLAG: u8 = 0x01;
/// Asks to keep the connection for several sessions; set back on the first reply to agree
/// (RFC 8907 section 4.3).
pub const SINGLE_CONNECT_FLAG: u8 = 0x04;
pub const TYPE_AUTHEN: u8 = 0x01;
pub const TYPE_AUTHOR: u8 = 0x02;
pub const TYPE_ACCT: u8 = 0x03;
pub const AUTHEN_STATUS_PASS: u8 = 0x01;
pub const AUTHEN_STATUS_FAIL: u8 = 0x02;
/// Asks for more, such as a one-time code after the password.
pub const AUTHEN_STATUS_GETDATA: u8 = 0x03;
pub const AUTHEN_STATUS_GETUSER: u8 = 0x04;
pub const AUTHEN_STATUS_GETPASS: u8 = 0x05;
/// The authentication type is not accepted; the client may start over with another.
pub const AUTHEN_STATUS_RESTART: u8 = 0x06;
pub const AUTHEN_STATUS_ERROR: u8 = 0x07;
/// The client must not show what is typed in answer to this reply.
pub const REPLY_FLAG_NOECHO: u8 = 0x01;
pub const CONTINUE_FLAG_ABORT: u8 = 0x01;
pub const ACTION_LOGIN: u8 = 0x01;
/// `authen_service` of a request to enter a higher privilege level (RFC 8907 section 5.1).
pub const AUTHEN_SVC_ENABLE: u8 = 0x02;
pub const AUTHOR_STATUS_PASS_ADD: u8 = 0x01;
pub const AUTHOR_STATUS_FAIL: u8 = 0x10;
pub const AUTHOR_STATUS_ERROR: u8 = 0x11;
pub const ACCT_STATUS_SUCCESS: u8 = 0x01;
pub const ACCT_STATUS_ERROR: u8 = 0x02;
/// Default upper bound on body size (estimate; configurable per server).
pub const MAX_BODY_LEN: usize = 64 * 1024;

/// Default limit of open connections.
pub const DEFAULT_MAX_CONNECTIONS: usize = 1024;

pub struct TacacsPacket {
    pub version: u8,
    pub type_: u8,
    pub seq_num: u8,
    pub flags: u8,
    pub session_id: u32,
    pub body: Vec<u8>,
}

impl TacacsPacket {
    /// Parses a complete packet (header plus `length` body octets) without decrypting it.
    pub fn parse(data: &[u8]) -> Result<Self, String> {
        if data.len() < HEADER_LEN {
            return Err("Packet too short".to_string());
        }
        let length = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
        if data.len() != HEADER_LEN + length {
            return Err(format!("Header length {} does not match body size {}", length, data.len() - HEADER_LEN));
        }
        Ok(TacacsPacket {
            version: data[0],
            type_: data[1],
            seq_num: data[2],
            flags: data[3],
            session_id: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
            body: data[HEADER_LEN..].to_vec(),
        })
    }

    /// Serializes the packet; the body is written as-is (encrypt it first if needed).
    pub fn encode(&self) -> Vec<u8> {
        let mut packet = vec![self.version, self.type_, self.seq_num, self.flags];
        packet.extend_from_slice(&self.session_id.to_be_bytes());
        packet.extend_from_slice(&(self.body.len() as u32).to_be_bytes());
        packet.extend_from_slice(&self.body);
        packet
    }

    /// RFC 8907 section 4.5 pseudo-pad: MD5_1 = MD5(session_id, key, version, seq_no),
    /// MD5_n = MD5(session_id, key, version, seq_no, MD5_n-1), truncated to `len`.
    pub fn pseudo_pad(secret: &[u8], session_id: u32, version: u8, seq_num: u8, len: usize) -> Vec<u8> {
        let mut pad = Vec::with_capacity(len + 16);
        let mut previous: Option<[u8; 16]> = None;
        while pad.len() < len {
            let mut hasher = Md5::new();
            hasher.update(session_id.to_be_bytes());
            hasher.update(secret);
            hasher.update([version, seq_num]);
            if let Some(prev) = previous {
                hasher.update(prev);
            }
            let segment: [u8; 16] = hasher.finalize().into();
            pad.extend_from_slice(&segment);
            previous = Some(segment);
        }
        pad.truncate(len);
        pad
    }

    /// Obfuscation is an XOR with the pseudo-pad, so the same call encrypts and decrypts.
    pub fn encrypt_decrypt(data: &[u8], secret: &[u8], session_id: u32, version: u8, seq_num: u8) -> Vec<u8> {
        let pad = Self::pseudo_pad(secret, session_id, version, seq_num, data.len());
        data.iter().zip(pad).map(|(byte, key)| byte ^ key).collect()
    }
}

/// Reads one packet framed by its header Length. Returns `None` on a clean EOF
/// before a new header, and an `InvalidData` error for packets that must close the connection.
pub async fn read_packet<R: AsyncRead + Unpin>(reader: &mut R, max_body_len: usize) -> io::Result<Option<TacacsPacket>> {
    let mut header = [0u8; HEADER_LEN];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    if header[0] >> 4 != MAJOR_VERSION {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unsupported TACACS+ version {:#04x}", header[0])));
    }
    let length = u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;
    if length > max_body_len {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("TACACS+ body length {} exceeds limit", length)));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    Ok(Some(TacacsPacket {
        version: header[0],
        type_: header[1],
        seq_num: header[2],
        flags: header[3],
        session_id: u32::from_be_bytes([header[4], header[5], header[6], header[7]]),
        body,
    }))
}

/// Splits `data` into consecutive fields of the given lengths; `None` unless the
/// lengths add up to exactly the remaining data.
fn split_fields<'a>(data: &'a [u8], lengths: &[usize]) -> Option<Vec<&'a [u8]>> {
    if lengths.iter().sum::<usize>() != data.len() {
        return None;
    }
    let mut fields = Vec::with_capacity(lengths.len());
    let mut rest = data;
    for len in lengths {
        let (field, tail) = rest.split_at(*len);
        fields.push(field);
        rest = tail;
    }
    Some(fields)
}

fn bytes(field: &[u8]) -> AttrValue {
    AttrValue::Bytes(field.to_vec())
}

/// `user`, `port`, `rem_addr` plus each `key=value` / `key*value` argument by key.
fn common_attributes(user: &[u8], port: &[u8], rem_addr: &[u8], args: &[&[u8]]) -> Vec<(String, AttrValue)> {
    let mut attributes = vec![
        ("user".to_string(), bytes(user)),
        ("port".to_string(), bytes(port)),
        ("rem_addr".to_string(), bytes(rem_addr)),
    ];
    for arg in args {
        if let Some(pos) = arg.iter().position(|b| *b == b'=' || *b == b'*') {
            attributes.push((String::from_utf8_lossy(&arg[..pos]).into_owned(), bytes(&arg[pos + 1..])));
        }
    }
    attributes
}

/// Whether a privilege level, authentication type and service are ones RFC 8907 defines. With the
/// wrong secret a body decrypts to noise, and these are what lengths that happen to add up do not
/// check. The authentication method is left out: it is the field a device is likeliest to extend.
fn known_fields(priv_lvl: u8, authen_type: u8, authen_service: u8) -> bool {
    priv_lvl <= 15 && authen_type <= 6 && authen_service <= 9
}

/// Parses an Authentication START body (RFC 8907 section 5.1).
pub fn parse_authen_start(body: &[u8]) -> Option<Vec<(String, AttrValue)>> {
    if body.len() < 8 {
        return None;
    }
    if !matches!(body[0], 1..=4) || body[2] == 0 || !known_fields(body[1], body[2], body[3]) {
        return None;
    }
    let fields = split_fields(&body[8..], &[body[4] as usize, body[5] as usize, body[6] as usize, body[7] as usize])?;
    let mut attributes = common_attributes(fields[0], fields[1], fields[2], &[]);
    attributes.push(("data".to_string(), bytes(fields[3])));
    attributes.push(("authen_type".to_string(), AttrValue::Number(body[2] as f64)));
    attributes.push(("priv_lvl".to_string(), AttrValue::Number(body[1] as f64)));
    attributes.push(("authen_service".to_string(), AttrValue::Number(body[3] as f64)));
    Some(attributes)
}

/// The reply an Authentication START gets before anything is asked or run: FAIL for an
/// action other than LOGIN, which this server does not do (RFC 8907 section 5.1:
/// unsupported options fail), for an enable request while `enable` denies them, or for CHAP
/// or MS-CHAPv2 data of the wrong size; RESTART for an authentication type this server does
/// not check, so that the client can try another. CHAP and MS-CHAPv2 are checked only by an
/// authentication chain with a handler that compares passwords (`chained`); a script has
/// nothing to compare them with. `None` for a body that does not parse, which is
/// answered as malformed.
fn refused_start(body: &[u8], enable: EnableMode, chained: bool, client: &Client) -> Option<Vec<u8>> {
    let attributes = parse_authen_start(body)?;
    let user = || match attributes.iter().find(|(name, _)| name == "user") {
        Some((_, AttrValue::Bytes(user))) => String::from_utf8_lossy(user).into_owned(),
        _ => String::new(),
    };
    if body[0] != ACTION_LOGIN {
        tracing::info!(client = client.name.as_str(), user = user().as_str(), action = body[0], "TACACS+ authentication refused: only LOGIN is supported");
        return Some(authen_reply(AUTHEN_STATUS_FAIL, 0, b""));
    }
    if body[3] == AUTHEN_SVC_ENABLE && enable == EnableMode::Deny {
        tracing::info!(client = client.name.as_str(), user = user().as_str(), priv_lvl = body[1], "TACACS+ enable request refused: [tacacs].enable is \"deny\"");
        return Some(authen_reply(AUTHEN_STATUS_FAIL, 0, b""));
    }
    let challenge_response = matches!(body[2], AUTHEN_TYPE_CHAP | AUTHEN_TYPE_MSCHAPV2) && chained && body[3] != AUTHEN_SVC_ENABLE;
    if !matches!(body[2], AUTHEN_TYPE_ASCII | AUTHEN_TYPE_PAP) && !challenge_response {
        tracing::info!(client = client.name.as_str(), user = user().as_str(), authen_type = body[2], "TACACS+ authentication type not supported here: asking the client to restart with another");
        return Some(authen_reply(AUTHEN_STATUS_RESTART, 0, b""));
    }
    if challenge_response && challenge_secret(body[2], data_field(&attributes)).is_none() {
        tracing::info!(client = client.name.as_str(), user = user().as_str(), authen_type = body[2], data_len = data_field(&attributes).len(), "TACACS+ authentication refused: CHAP needs a challenge of at least 8 octets, MS-CHAPv2 one of 16");
        return Some(authen_reply(AUTHEN_STATUS_FAIL, 0, b""));
    }
    None
}

fn data_field(attributes: &[(String, AttrValue)]) -> &[u8] {
    match attributes.iter().find(|(name, _)| name == "data") {
        Some((_, AttrValue::Bytes(data))) => data,
        _ => &[],
    }
}

/// CHAP response size (RFC 8907 section 5.4.2.3).
const CHAP_RESPONSE_LEN: usize = 16;
/// RFC 8907 section 5.4.2.3 recommends refusing shorter CHAP challenges.
const MIN_CHAP_CHALLENGE_LEN: usize = 8;
/// MS-CHAPv2 response size (RFC 8907 section 5.4.2.5): peer challenge (16), reserved (8),
/// NT response (24), flags (1), as in RFC 2759.
const MSCHAPV2_RESPONSE_LEN: usize = 49;

/// The secret in the data of a CHAP or MS-CHAPv2 START: the PPP id, the challenge, then the
/// response. `None` for another type, or for data whose challenge is shorter than
/// [`MIN_CHAP_CHALLENGE_LEN`] (CHAP) or other than 16 octets (MS-CHAPv2, as RFC 8907 requires).
fn challenge_secret(authen_type: u8, data: &[u8]) -> Option<Secret> {
    let (&id, rest) = data.split_first()?;
    match authen_type {
        AUTHEN_TYPE_CHAP => {
            let split = rest.len().checked_sub(CHAP_RESPONSE_LEN).filter(|&len| len >= MIN_CHAP_CHALLENGE_LEN)?;
            let (challenge, response) = rest.split_at(split);
            Some(Secret::Chap { id, response: response.to_vec(), challenge: challenge.to_vec() })
        }
        AUTHEN_TYPE_MSCHAPV2 => {
            if rest.len() != 16 + MSCHAPV2_RESPONSE_LEN {
                return None;
            }
            let (challenge, response) = rest.split_at(16);
            Some(Secret::MsChapV2 {
                ident: id,
                challenge: challenge.try_into().ok()?,
                peer_challenge: response[..16].try_into().ok()?,
                response: response[24..48].try_into().ok()?,
            })
        }
        _ => None,
    }
}

/// Parses the common layout of an Authorization REQUEST (offset 0) or Accounting
/// REQUEST (offset 1, after its flags byte): 4 method bytes, user/port/rem_addr
/// lengths, arg_cnt, the argument lengths, then the fields.
fn parse_args_body(body: &[u8], offset: usize) -> Option<Vec<(String, AttrValue)>> {
    args_attributes(body, offset, &args_body_fields(body, offset)?)
}

/// What a script sees of an authorization or accounting body already split into `fields`.
fn args_attributes(body: &[u8], offset: usize, fields: &[&[u8]]) -> Option<Vec<(String, AttrValue)>> {
    let mut attributes = common_attributes(fields.first()?, fields.get(1)?, fields.get(2)?, fields.get(3..)?);
    push_header_fields(&mut attributes, body.get(offset..offset + 4)?);
    Some(attributes)
}

/// The four method fields of an authorization or accounting body, by their RFC 8907 names:
/// how the user logged in, and the privilege level they have now.
fn push_header_fields(attributes: &mut Vec<(String, AttrValue)>, header: &[u8]) {
    for (name, value) in ["authen_method", "priv_lvl", "authen_type", "authen_service"].into_iter().zip(header) {
        attributes.push((name.to_string(), AttrValue::Number(f64::from(*value))));
    }
}

/// The user, port and rem_addr fields, then each argument as sent, of the layout
/// `parse_args_body` reads.
fn args_body_fields(body: &[u8], offset: usize) -> Option<Vec<&[u8]>> {
    let header = offset + 8;
    if body.len() < header {
        return None;
    }
    if !known_fields(body[offset + 1], body[offset + 2], body[offset + 3]) {
        return None;
    }
    let arg_cnt = body[offset + 7] as usize;
    if body.len() < header + arg_cnt {
        return None;
    }
    let mut lengths = vec![body[offset + 4] as usize, body[offset + 5] as usize, body[offset + 6] as usize];
    lengths.extend(body[header..header + arg_cnt].iter().map(|l| *l as usize));
    split_fields(&body[header + arg_cnt..], &lengths)
}

/// Parses an Authorization REQUEST body (RFC 8907 section 6.1).
pub fn parse_author_request(body: &[u8]) -> Option<Vec<(String, AttrValue)>> {
    parse_args_body(body, 0)
}

/// Parses an Accounting REQUEST body (RFC 8907 section 7.1).
pub fn parse_acct_request(body: &[u8]) -> Option<Vec<(String, AttrValue)>> {
    // START, STOP or WATCHDOG. The lowest bit was MORE in the draft RFC 8907 came from.
    let flags = *body.first()?;
    if flags & 0xf0 != 0 || flags & 0x0e == 0 {
        return None;
    }
    parse_args_body(body, 1)
}

/// Formats a `set_attr` pair as a `name=value` AV pair.
fn av_pair(name: &str, value: &AttrValue) -> Vec<u8> {
    let mut pair = format!("{}=", name).into_bytes();
    match value {
        AttrValue::Bytes(b) => pair.extend_from_slice(b),
        AttrValue::Number(n) => pair.extend_from_slice(n.to_string().as_bytes()),
    }
    pair
}

/// Builds an Authorization RESPONSE body (RFC 8907 section 6.2) without server_msg or data.
fn author_response(status: u8, args: &[Vec<u8>]) -> Vec<u8> {
    author_response_with(status, args, b"")
}

/// An Authorization RESPONSE body with `server_msg` and no data.
fn author_response_with(status: u8, args: &[Vec<u8>], server_msg: &[u8]) -> Vec<u8> {
    let mut body = vec![status, args.len() as u8];
    body.extend_from_slice(&(server_msg.len() as u16).to_be_bytes());
    body.extend_from_slice(&[0, 0]);
    body.extend(args.iter().map(|a| a.len() as u8));
    body.extend_from_slice(server_msg);
    for arg in args {
        body.extend_from_slice(arg);
    }
    body
}

/// The attribute a script sets with `set_attr` to put a message for the user in a reply.
const SERVER_MSG: &str = "server_msg";
/// The longest `server_msg` sent: a line or two on a terminal. Longer ones are left out and logged.
const MAX_SERVER_MSG: usize = 1024;

/// The last `server_msg` among a decision's attributes, empty when there is none or it is too long.
fn server_msg(attributes: &[(String, AttrValue)], client: &Client) -> Vec<u8> {
    let Some((_, value)) = attributes.iter().rev().find(|(name, _)| name == SERVER_MSG) else {
        return Vec::new();
    };
    let text = match value {
        AttrValue::Bytes(bytes) => bytes.clone(),
        AttrValue::Number(number) => number.to_string().into_bytes(),
    };
    if text.len() > MAX_SERVER_MSG {
        crate::lua_engine::log_line(tracing::Level::ERROR, format!("TACACS+ reply to client {}: server_msg of {} octets left out, more than {MAX_SERVER_MSG}", client.name, text.len()));
        return Vec::new();
    }
    text
}

/// Authentication type ASCII in an Authentication START (RFC 8907 section 5.1).
pub const AUTHEN_TYPE_ASCII: u8 = 0x01;
/// Authentication type PAP in an Authentication START (RFC 8907 section 5.1).
pub const AUTHEN_TYPE_PAP: u8 = 0x02;
/// Authentication type CHAP in an Authentication START (RFC 8907 section 5.1).
pub const AUTHEN_TYPE_CHAP: u8 = 0x03;
/// Authentication type MS-CHAPv2 in an Authentication START (RFC 8907 section 5.1).
pub const AUTHEN_TYPE_MSCHAPV2: u8 = 0x06;

/// The user and secret of a parsed Authentication START: `data` is the
/// password for PAP and the PPP id, challenge and response for CHAP and MS-CHAPv2;
/// ASCII carries no secret in its START.
pub fn start_credentials(attributes: &[(String, AttrValue)]) -> Credentials {
    let field = |name: &str| attributes.iter().find(|(n, _)| n == name).map(|(_, v)| v);
    let user = match field("user") {
        Some(AttrValue::Bytes(user)) if !user.is_empty() => Some(String::from_utf8_lossy(user).into_owned()),
        _ => None,
    };
    let secret = match (field("authen_type"), field("data")) {
        (Some(AttrValue::Number(t)), Some(AttrValue::Bytes(data))) if *t == AUTHEN_TYPE_PAP as f64 => Secret::pap(data.clone()),
        (Some(AttrValue::Number(t)), Some(AttrValue::Bytes(data))) => challenge_secret(*t as u8, data).unwrap_or(Secret::None),
        _ => Secret::None,
    };
    Credentials { user, secret }
}

/// Unfinished ASCII logins one connection may hold.
const MAX_ASCII_LOGINS: usize = 8;
/// How long an ASCII login may take.
const ASCII_LOGIN_LIMIT: Duration = Duration::from_secs(60);
/// RFC 8907 section 5.4.2.1 recommends three attempts at getting a username.
const MAX_USERNAME_ATTEMPTS: u8 = 3;

/// An ASCII login still asking for its username or password.
struct AsciiLogin {
    attributes: Vec<(String, AttrValue)>,
    user: Option<Vec<u8>>,
    /// The sequence number of the CONTINUE expected next.
    next_seq: u8,
    empty_usernames: u8,
    started: Instant,
    /// After the password, waiting for the code the second factor asked for.
    code_for: Option<CodeFor>,
}

/// Whose code an ASCII login waits for: the password is not kept.
struct CodeFor {
    user: String,
    account: String,
}

/// What to do with an authentication packet.
enum AuthenStep {
    /// Not part of an ASCII login: decided as it stands.
    Decide,
    Reply(Vec<u8>),
    /// An ERROR for a body that did not decode, after which the connection closes.
    Undecodable(Vec<u8>),
    /// The client gave up; RFC 8907 asks for no reply.
    NoReply,
    /// An ASCII login has what it needs.
    Finished { attributes: Vec<(String, AttrValue)>, credentials: Credentials },
    /// The code an ASCII login was asked for after its password.
    Code { code_for: CodeFor, code: zeroize::Zeroizing<Vec<u8>> },
}

/// The unfinished ASCII logins of one connection (RFC 8907 section 5.4.2.1). They stay
/// here rather than in the database: a connection belongs to one node anyway, and a
/// password should not be stored even in passing.
#[derive(Default)]
struct AsciiLogins(HashMap<u32, AsciiLogin>);

impl AsciiLogins {
    fn forget(&mut self, session_id: u32) {
        self.0.remove(&session_id);
    }

    fn step(&mut self, packet: &TacacsPacket, body: &[u8], now: Instant) -> AuthenStep {
        self.0.retain(|_, login| now.duration_since(login.started) < ASCII_LOGIN_LIMIT);
        if packet.seq_num == 1 {
            return self.start(packet.session_id, body, now);
        }
        let Some(login) = self.0.get_mut(&packet.session_id).filter(|login| login.next_seq == packet.seq_num) else {
            self.0.remove(&packet.session_id);
            return AuthenStep::Reply(authen_reply(AUTHEN_STATUS_ERROR, 0, b""));
        };
        let Some((user_msg, flags)) = parse_continue(body) else {
            self.0.remove(&packet.session_id);
            return AuthenStep::Undecodable(authen_reply(AUTHEN_STATUS_ERROR, 0, b""));
        };
        if flags & CONTINUE_FLAG_ABORT != 0 {
            self.0.remove(&packet.session_id);
            return AuthenStep::NoReply;
        }
        login.next_seq = login.next_seq.wrapping_add(2);
        if login.code_for.is_some() {
            let code = zeroize::Zeroizing::new(user_msg.to_vec());
            return match self.0.remove(&packet.session_id).and_then(|login| login.code_for) {
                Some(code_for) => AuthenStep::Code { code_for, code },
                None => AuthenStep::Reply(authen_reply(AUTHEN_STATUS_ERROR, 0, b"")),
            };
        }
        if login.user.is_none() {
            if !user_msg.is_empty() {
                login.user = Some(user_msg.to_vec());
                return AuthenStep::Reply(ask_password());
            }
            login.empty_usernames += 1;
            if login.empty_usernames < MAX_USERNAME_ATTEMPTS {
                return AuthenStep::Reply(ask_username());
            }
            self.0.remove(&packet.session_id);
            return AuthenStep::Reply(authen_reply(AUTHEN_STATUS_FAIL, 0, b""));
        }
        let password = user_msg.to_vec();
        let Some(login) = self.0.remove(&packet.session_id) else {
            return AuthenStep::Reply(authen_reply(AUTHEN_STATUS_ERROR, 0, b""));
        };
        let user = login.user.unwrap_or_default();
        let mut attributes = login.attributes;
        for (name, value) in &mut attributes {
            match name.as_str() {
                "user" => *value = bytes(&user),
                "data" => *value = bytes(&password),
                _ => {}
            }
        }
        let credentials = Credentials { user: Some(String::from_utf8_lossy(&user).into_owned()), secret: Secret::pap(password) };
        AuthenStep::Finished { attributes, credentials }
    }

    fn start(&mut self, session_id: u32, body: &[u8], now: Instant) -> AuthenStep {
        let is_ascii_login = body.len() > 2 && body[0] == ACTION_LOGIN && body[2] == AUTHEN_TYPE_ASCII;
        let attributes = is_ascii_login.then(|| parse_authen_start(body)).flatten();
        let Some(attributes) = attributes else {
            return AuthenStep::Decide;
        };
        if self.0.len() >= MAX_ASCII_LOGINS && !self.0.contains_key(&session_id) {
            return AuthenStep::Reply(authen_reply(AUTHEN_STATUS_ERROR, 0, b""));
        }
        let user = attributes.iter().find_map(|(name, value)| match value {
            AttrValue::Bytes(user) if name == "user" && !user.is_empty() => Some(user.clone()),
            _ => None,
        });
        let reply = if user.is_some() { ask_password() } else { ask_username() };
        self.0.insert(session_id, AsciiLogin { attributes, user, next_seq: 3, empty_usernames: 0, started: now, code_for: None });
        AuthenStep::Reply(reply)
    }

    /// After the password of an ASCII login, waits for the code its reply asked for, from the
    /// CONTINUE numbered `next_seq`, with a new time limit.
    fn wait_for_code(&mut self, session_id: u32, next_seq: u8, code_for: CodeFor, now: Instant) {
        self.0.insert(session_id, AsciiLogin { attributes: Vec::new(), user: None, next_seq, empty_usernames: 0, started: now, code_for: Some(code_for) });
    }
}

/// How a login is decided.
enum Login {
    Reply(Vec<u8>),
    /// The password was accepted and the second factor asks for a code: `reply` asks for it.
    AskCode { code_for: CodeFor, reply: Vec<u8> },
}

/// The second factor of a client with `mfa`, for one login.
struct Factor<'a> {
    mode: crate::clients::SecondFactorMode,
    factor: &'a dyn crate::second_factor::SecondFactor,
    /// Only an ASCII login can be asked for more.
    can_ask: bool,
}

fn ask_username() -> Vec<u8> {
    authen_reply(AUTHEN_STATUS_GETUSER, 0, b"Username: ")
}

fn ask_password() -> Vec<u8> {
    authen_reply(AUTHEN_STATUS_GETPASS, REPLY_FLAG_NOECHO, b"Password: ")
}

/// An Authentication REPLY body (RFC 8907 section 5.2): status, flags, server_msg_len(2),
/// data_len(2), server_msg.
fn authen_reply(status: u8, flags: u8, server_msg: &[u8]) -> Vec<u8> {
    let mut body = vec![status, flags];
    body.extend_from_slice(&(server_msg.len() as u16).to_be_bytes());
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(server_msg);
    body
}

/// The user_msg and flags of an Authentication CONTINUE (RFC 8907 section 5.3):
/// user_msg_len(2), data_len(2), flags, user_msg, data.
pub fn parse_continue(body: &[u8]) -> Option<(&[u8], u8)> {
    if body.len() < 5 {
        return None;
    }
    let user_msg_len = u16::from_be_bytes([body[0], body[1]]) as usize;
    let data_len = u16::from_be_bytes([body[2], body[3]]) as usize;
    if body.len() != 5 + user_msg_len + data_len {
        return None;
    }
    Some((&body[5..5 + user_msg_len], body[4]))
}

/// The authentication record of a TACACS+ login.
fn record_login(client: &Client, source: IpAddr, user: &str, account: &str, chain: &str, handler: &str, decision: &str) {
    crate::logging::auth_record(&crate::logging::AuthRecord { protocol: "tacacs", client: &client.name, source, user, account, chain, handler, decision });
}

/// What a reload replaces. A request reads this once and keeps it until it is done.
pub struct TacacsState {
    pub clients: Arc<ClientTable>,
    pub engine: Arc<LuaAuthEngine>,
    /// Decides authentication for each client with the chain it selects;
    /// without a chain the top-level script does.
    pub chains: Option<Arc<AuthChains>>,
    pub enable: EnableMode,
    /// `[tacacs.tls] tls_only`, from the extension that serves TACACS+ over TLS: obfuscated sessions get nothing, also on a plain port a reload
    /// turned it on for, which stays open until a restart.
    pub tls_only: bool,
    /// The second factor of the clients with `mfa`, such as `[mfa]`.
    pub second_factor: Option<Arc<dyn crate::second_factor::SecondFactor>>,
}

/// How packets travel on a connection: obfuscated with the client's secret (RFC 8907), or in
/// the clear inside a connection that checks its peer, as TLS 1.3 does (RFC 9887).
enum Transport {
    Obfuscated,
    Clear(Box<dyn PeerCheck>),
}

/// A connection the server took on: from a configured client and within `max_connections`,
/// counted as open until it is dropped.
pub struct Admitted {
    source: SocketAddr,
    _slot: tokio::sync::OwnedSemaphorePermit,
    _open: Open,
}

impl Admitted {
    /// Where the connection comes from.
    pub fn source(&self) -> SocketAddr {
        self.source
    }
}

/// What an enable request (an Authentication START for the ENABLE service) gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EnableMode {
    /// FAIL, without asking for a password: otherwise anyone who can log in could enable
    /// with their own password.
    #[default]
    Deny,
    /// Decided as a login, by the chain or the script, which see `authen_service`.
    Login,
}

impl EnableMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "deny" => Some(Self::Deny),
            "login" => Some(Self::Login),
            _ => None,
        }
    }
}

/// How long a connection may go without a complete packet, and take to write a response.
#[derive(Clone, Copy)]
pub struct Timeouts {
    pub idle: Duration,
    pub write: Duration,
    /// How often a peer an extension checks is checked again.
    pub recheck: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self { idle: Duration::from_millis(DEFAULT_IDLE_TIMEOUT_MS), write: Duration::from_secs(10), recheck: DEFAULT_RECHECK }
    }
}

/// How often a peer that an extension checks, such as a TACACS+ over TLS client, has its
/// certificate checked again, when it sends a packet: one revoked by a reload or expired since
/// does not stay connected.
pub const DEFAULT_RECHECK: Duration = Duration::from_secs(60);

/// Default of `[tacacs].idle_timeout_ms`.
pub const DEFAULT_IDLE_TIMEOUT_MS: u64 = 300_000;

pub struct TacacsServer {
    pub addr: SocketAddr,
    pub max_body_len: usize,
    state: Arc<Live<TacacsState>>,
    metrics: Arc<Metrics>,
    /// Open connections from configured clients.
    connection_slots: Arc<Semaphore>,
    timeouts: Timeouts,
    /// TACACS+ over TLS, when an extension serves it, and whether only it does.
    tls_served: Option<TacacsTlsServed>,
    /// What `admit` logs of connections it refuses, at most once a second each.
    refusals: parking_lot::Mutex<(DiscardLog, DiscardLog)>,
}

impl TacacsServer {
    pub fn new(addr: SocketAddr, clients: Arc<ClientTable>, engine: Arc<LuaAuthEngine>, max_body_len: usize) -> Self {
        Self::from_state(addr, TacacsState { clients, engine, chains: None, enable: EnableMode::default(), tls_only: false, second_factor: None }, max_body_len)
    }

    pub fn from_state(addr: SocketAddr, state: TacacsState, max_body_len: usize) -> Self {
        Self { addr, max_body_len, state: Arc::new(Live::new(state)), metrics: Arc::default(), connection_slots: Arc::new(Semaphore::new(DEFAULT_MAX_CONNECTIONS)), timeouts: Timeouts::default(), tls_served: None, refusals: parking_lot::Mutex::default() }
    }

    /// How long connections may go idle and take to write; an extension that makes connections
    /// gives a handshake as long as a write.
    pub fn timeouts(&self) -> Timeouts {
        self.timeouts
    }

    /// Takes on a connection from `source` that an extension accepted, before it reads anything
    /// from it: `None`, after counting and logging the refusal, for a source no client covers or
    /// when `max_connections` are open.
    pub fn admit(&self, source: SocketAddr) -> Option<Admitted> {
        let mut refusals = self.refusals.lock();
        let (strangers, discards) = &mut *refusals;
        self.admit_with(source, strangers, discards)
    }

    fn admit_with(&self, source: SocketAddr, strangers: &mut DiscardLog, discards: &mut DiscardLog) -> Option<Admitted> {
        // Unknown sources are closed before any data is read.
        let Some(client) = self.state.get().clients.lookup(source.ip()).cloned() else {
            self.metrics.tacacs_refused.inc();
            if let Some(line) = strangers.line(std::time::Instant::now(), || format!("TACACS+ connection from unknown client {} closed", source.ip())) {
                tracing::warn!("{line}");
            }
            return None;
        };
        let Ok(slot) = self.connection_slots.clone().try_acquire_owned() else {
            self.metrics.tacacs_refused.inc();
            if let Some(count) = discards.record(std::time::Instant::now()) {
                tracing::warn!("TACACS+ overloaded: closed {} connections", count);
            }
            return None;
        };
        tracing::debug!("New TACACS+ connection from {} ({})", source, client.name);
        self.metrics.tacacs_accepted.inc();
        Some(Admitted { source, _slot: slot, _open: Open::new(self.metrics.clone(), |metrics| &metrics.tacacs_open) })
    }

    /// Serves packets in the clear on `stream`, a connection `admit` took on and the extension
    /// made, as RFC 9887 has them inside TLS; `check` is asked about the peer now and then. An
    /// error ends the connection, for the extension to log.
    pub async fn serve_clear<S: AsyncRead + AsyncWrite + Unpin>(&self, admitted: Admitted, stream: S, check: Box<dyn PeerCheck>) -> io::Result<()> {
        let served = Self::handle_clear_connection(stream, admitted.source.ip(), &self.state, &self.metrics, self.max_body_len, self.timeouts, check).await;
        drop(admitted);
        served
    }

    /// The clients as they are now, for an extension to check a peer against before it serves.
    pub fn clients(&self) -> Arc<ClientTable> {
        self.state.get().clients.clone()
    }

    /// Sets where this server counts what it handles.
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = metrics;
        self
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> Arc<TacacsState> {
        self.state.get()
    }

    /// What requests read from now on; those being handled finish with what they read.
    pub fn replace_state(&self, mut state: TacacsState) {
        // As at startup: only with a TLS port listening, which a reload does not open.
        state.tls_only &= self.tls_served.is_some();
        self.state.set(state);
    }

    /// Sets how long a connection may go without a complete packet.
    pub fn with_idle_timeout(mut self, idle: Duration) -> Self {
        self.timeouts.idle = idle;
        self
    }

    /// Sets how many connections from configured clients stay open at once.
    pub fn with_max_connections(mut self, max_connections: usize) -> Self {
        self.connection_slots = Arc::new(Semaphore::new(max_connections));
        self
    }

    /// Sets how often a peer an extension checks is checked again.
    pub fn with_recheck(mut self, recheck: Duration) -> Self {
        self.timeouts.recheck = recheck;
        self
    }

    /// Tells the server that an extension serves TACACS+ over TLS (RFC 9887) as `served` says:
    /// with `tls_only`, the port without TLS is not opened.
    pub fn with_tls_served(mut self, served: Option<TacacsTlsServed>) -> Self {
        self.tls_served = served;
        self
    }

    pub fn with_auth_chains(self, chains: Arc<AuthChains>) -> Self {
        let state = self.state.get();
        self.state.set(TacacsState { clients: state.clients.clone(), engine: state.engine.clone(), chains: Some(chains), enable: state.enable, tls_only: state.tls_only, second_factor: state.second_factor.clone() });
        self
    }

    /// Listens on the port without TLS and serves it, unless the extension that serves TACACS+
    /// over TLS has `tls_only`: then there is nothing to do here, and it logs where it listens.
    pub async fn run(&self) -> io::Result<()> {
        if self.tls_served.is_some_and(|served| served.tls_only) {
            return Ok(());
        }
        let listener = TcpListener::bind(self.addr).await?;
        tracing::info!("TACACS+ Server listening on {}:{}", self.addr.ip(), self.addr.port());
        self.serve(listener).await
    }

    /// Accepts connections on `listener`.
    async fn serve(&self, listener: TcpListener) -> io::Result<()> {
        let label = "TACACS+";
        let mut discards = DiscardLog::default();
        let mut failures = DiscardLog::default();
        let mut strangers = DiscardLog::default();

        loop {
            let (socket, addr) = accept_next(&listener, label, &mut failures).await;
            let Some(admitted) = self.admit_with(addr, &mut strangers, &mut discards) else {
                drop(socket);
                continue;
            };
            let live = self.state.clone();
            let max_body_len = self.max_body_len;
            let timeouts = self.timeouts;
            let metrics = self.metrics.clone();
            tokio::spawn(async move {
                let _admitted = admitted;
                // Yield first: a freshly spawned task sits in this worker's LIFO slot, which other
                // workers cannot steal, so a script that never yields would also stall the receive loop.
                tokio::task::yield_now().await;
                if let Err(e) = Self::handle_connection(socket, addr.ip(), &live, &metrics, max_body_len, timeouts).await {
                    tracing::warn!("{} connection error from {}: {}", label, addr, e);
                }
            });
        }
    }

    /// Serves packets until EOF; returns an error (closing the connection) on packets
    /// with an unsupported version, oversized body, or the unencrypted flag (every
    /// configured client has a non-empty secret).
    pub async fn handle_connection<S: AsyncRead + AsyncWrite + Unpin>(socket: S, source: IpAddr, live: &Live<TacacsState>, metrics: &Metrics, max_body_len: usize, timeouts: Timeouts) -> io::Result<()> {
        Self::serve_packets(socket, source, live, metrics, max_body_len, timeouts, Transport::Obfuscated).await
    }

    /// Serves packets in the clear, as RFC 9887 has them inside TLS, on a connection whose peer
    /// `check` checks again now and then.
    pub async fn handle_clear_connection<S: AsyncRead + AsyncWrite + Unpin>(socket: S, source: IpAddr, live: &Live<TacacsState>, metrics: &Metrics, max_body_len: usize, timeouts: Timeouts, check: Box<dyn PeerCheck>) -> io::Result<()> {
        Self::serve_packets(socket, source, live, metrics, max_body_len, timeouts, Transport::Clear(check)).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn serve_packets<S: AsyncRead + AsyncWrite + Unpin>(mut socket: S, source: IpAddr, live: &Live<TacacsState>, metrics: &Metrics, max_body_len: usize, timeouts: Timeouts, transport: Transport) -> io::Result<()> {
        let mut logins = AsciiLogins::default();
        let mut first_packet = true;
        let mut checked = Instant::now();
        loop {
            let Ok(read) = tokio::time::timeout(timeouts.idle, read_packet(&mut socket, max_body_len)).await else {
                tracing::warn!("TACACS+ connection from {source} closed: idle for {:?}", timeouts.idle);
                return Ok(());
            };
            let Some(packet) = read? else { return Ok(()) };
            // Only the first packet can ask for single-connection mode, and only its reply agrees.
            let reply_flags = if std::mem::replace(&mut first_packet, false) { packet.flags & SINGLE_CONNECT_FLAG } else { 0 };
            // Every packet, so that a reload reaches connections already open.
            let state = live.get();
            if matches!(transport, Transport::Obfuscated) && state.tls_only {
                tracing::warn!("TACACS+ connection from {source} closed: tls_only takes TACACS+ over TLS only");
                return Ok(());
            }
            let Some((network, client)) = state.clients.lookup_entry(source) else {
                tracing::warn!("TACACS+ connection from {source} closed: no longer a configured client");
                return Ok(());
            };

            match &transport {
                Transport::Clear(check) if checked.elapsed() >= timeouts.recheck => {
                    if let Err(reason) = check.recheck(network, client) {
                        tracing::warn!("TACACS+ over TLS connection from {source} closed: {reason}");
                        return Ok(());
                    }
                    checked = Instant::now();
                }
                Transport::Clear(_) => {}
                // A client known by its certificate has no business on the port without TLS.
                Transport::Obfuscated if client.tls_name.is_some() => {
                    return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("client '{}' has a tls_name, so it has to use TLS", client.name)));
                }
                Transport::Obfuscated => {}
            }
            let (engine, secret) = (&state.engine, client.secret.as_slice());
            let mfa = client.mfa.zip(state.second_factor.as_deref());
            let chain = state.chains.as_deref().and_then(|chains| chains.for_client(client));
            // Clients send odd numbers. Answering an even one would use the pad of the client's next
            // packet on a body the sender can predict, and 255 has no number left for the answer.
            if packet.seq_num % 2 == 0 || packet.seq_num == u8::MAX {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("TACACS+ sequence number {} from a client", packet.seq_num)));
            }
            let tls = matches!(transport, Transport::Clear(_));
            if !tls && packet.flags & UNENCRYPTED_FLAG != 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "unencrypted TACACS+ packet while a secret is configured"));
            }
            // RFC 9887 section 4: no obfuscation inside TLS. A packet without the flag gets an
            // ERROR with it, and the connection ends.
            if tls && packet.flags & UNENCRYPTED_FLAG == 0 {
                let body = match packet.type_ {
                    TYPE_AUTHOR => author_response(AUTHOR_STATUS_ERROR, &[]),
                    TYPE_ACCT => vec![0, 0, 0, 0, ACCT_STATUS_ERROR],
                    _ => vec![AUTHEN_STATUS_ERROR, 0, 0, 0, 0, 0],
                };
                let error = TacacsPacket { version: packet.version, type_: packet.type_, seq_num: packet.seq_num.wrapping_add(1), flags: UNENCRYPTED_FLAG, session_id: packet.session_id, body };
                tokio::time::timeout(timeouts.write, socket.write_all(&error.encode())).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "an ERROR could not be written"))??;
                return Err(io::Error::new(io::ErrorKind::InvalidData, "an obfuscated TACACS+ packet over TLS"));
            }
            let body = match tls {
                true => packet.body.clone(),
                false => TacacsPacket::encrypt_decrypt(&packet.body, secret, packet.session_id, packet.version, packet.seq_num),
            };
            match packet.type_ {
                // A login is one request, however many packets asking for its parts take.
                TYPE_AUTHEN if packet.seq_num == 1 => metrics.tacacs_authen.inc(),
                TYPE_AUTHEN => {}
                TYPE_AUTHOR => metrics.tacacs_author.inc(),
                TYPE_ACCT => metrics.tacacs_acct.inc(),
                _ => {}
            }

            let refused = (packet.type_ == TYPE_AUTHEN && packet.seq_num == 1).then(|| refused_start(&body, state.enable, chain.is_some_and(AuthChain::compares_passwords), client)).flatten();
            let step = match refused {
                Some(reply) => {
                    // A new START ends whatever this session was doing, as it does for a login.
                    logins.forget(packet.session_id);
                    AuthenStep::Reply(reply)
                }
                None if packet.type_ == TYPE_AUTHEN => logins.step(&packet, &body, Instant::now()),
                None => AuthenStep::Decide,
            };
            let (response_body, undecodable) = match step {
                // A PAP login gives its code after the password; CHAP and MS-CHAPv2 cannot carry one.
                AuthenStep::Decide => Self::reply_body(packet.type_, &body, engine, chain, client, source, metrics, mfa.map(|(_, factor)| Factor { mode: crate::clients::SecondFactorMode::Append, factor, can_ask: false })).await,
                AuthenStep::Reply(reply) => (reply, false),
                AuthenStep::Undecodable(reply) => (reply, true),
                AuthenStep::NoReply => continue,
                AuthenStep::Finished { attributes, credentials } => match Self::decide_login(credentials, attributes, engine, chain, client, source, mfa.map(|(mode, factor)| Factor { mode, factor, can_ask: true })).await {
                    Login::Reply(reply) => (reply, false),
                    Login::AskCode { code_for, reply } => {
                        logins.wait_for_code(packet.session_id, packet.seq_num.wrapping_add(2), code_for, Instant::now());
                        (reply, false)
                    }
                },
                AuthenStep::Code { code_for, code } => match mfa {
                    Some((mode, factor)) => (Self::check_code(&code_for, &code, client, source, chain.is_some(), mode, factor).await, false),
                    // A reload took the second factor away while the login waited.
                    None => (authen_reply(AUTHEN_STATUS_FAIL, 0, &[]), false),
                },
            };
            let seq_num = packet.seq_num.wrapping_add(1);
            let response = TacacsPacket {
                version: packet.version,
                type_: packet.type_,
                seq_num,
                flags: if tls { reply_flags | UNENCRYPTED_FLAG } else { reply_flags },
                session_id: packet.session_id,
                body: match tls {
                    true => response_body,
                    false => TacacsPacket::encrypt_decrypt(&response_body, secret, packet.session_id, packet.version, seq_num),
                },
            };
            tokio::time::timeout(timeouts.write, socket.write_all(&response.encode()))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, format!("a response could not be written within {:?}", timeouts.write)))??;
            // RFC 8907 section 10.5.2: no more sessions once the secret looks wrong.
            if undecodable {
                match tls {
                    true => tracing::warn!("TACACS+ over TLS connection from {source} closed: a body did not decode"),
                    false => tracing::warn!("TACACS+ connection from {source} closed: a body did not decode, so the shared secret may be wrong"),
                }
                return Ok(());
            }
        }
    }

    /// Decides an authentication and builds its reply body; with `factor`, the second factor
    /// sees the login before the chain and after it accepts.
    #[allow(clippy::too_many_arguments)]
    async fn decide_login(mut credentials: Credentials, mut attributes: Vec<(String, AttrValue)>, engine: &Arc<LuaAuthEngine>, chain: Option<&AuthChain>, client: &Client, source: IpAddr, factor: Option<Factor<'_>>) -> Login {
        let factor_user = credentials.user.clone().unwrap_or_default();
        let factor_chain = if chain.is_some() { client.auth.as_deref().unwrap_or("default") } else { "script" };
        let factor_password = match (&factor, &credentials.secret) {
            (Some(_), Secret::Pap(password)) => Some(password.clone()),
            _ => None,
        };
        if let Some(Factor { mode, factor, .. }) = &factor {
            let request = crate::second_factor::SecondFactorRequest { protocol: "tacacs", client, mode: *mode, user: &factor_user, password: factor_password.as_deref().map(Vec::as_slice), state: None };
            let decision = match factor.before_chain(&request).await {
                crate::second_factor::Before::Continue { password: Some(password) } => {
                    for (name, value) in &mut attributes {
                        if name == "data" {
                            *value = bytes(&password);
                        }
                    }
                    credentials.secret = Secret::pap(password.to_vec());
                    None
                }
                crate::second_factor::Before::Continue { password: None } => None,
                crate::second_factor::Before::Answer(decision) => Some(decision),
                crate::second_factor::Before::Reject { log } => Some(crate::second_factor::Decision::Reject { log }),
                crate::second_factor::Before::Unavailable { log } => Some(crate::second_factor::Decision::Unavailable { log }),
            };
            if let Some(decision) = decision {
                return Login::Reply(Self::second_factor_reply(client, source, &factor_user, "", factor_chain, decision));
            }
        }
        let mut request = LuaRequest::new("tacacs", "authentication", attributes);
        request.client = client.name.clone();
        let credentials = &credentials;
        let user = credentials.user.as_deref().unwrap_or("");
        let (accepted, chain_name, handler, account, mut replies, unavailable) = match chain {
            Some(chain) => {
                let result = chain.authenticate(credentials, &request).await;
                let unavailable = result.unavailable;
                // A CHAP or MS-CHAPv2 response proves nothing unless a handler compared it: an accept
                // from a policy script, after one that did not know the name, is no such proof, and
                // MS-CHAPv2 is only proved by MS-CHAP2-Success, as for PEAP's inner method.
                let proved = match &credentials.secret {
                    Secret::Chap { .. } => result.verified,
                    Secret::MsChapV2 { .. } => result.verified && result.attributes.iter().any(|(name, _)| name == "MS-CHAP2-Success"),
                    _ => true,
                };
                if result.accepted() && !proved {
                    crate::lua_engine::log_line(tracing::Level::WARN, format!("TACACS+ client {}: a CHAP or MS-CHAPv2 login accepted without a handler checking the response, refused", client.name));
                }
                (result.accepted() && proved, client.auth.as_deref().unwrap_or("default"), if proved { result.handler } else { "no_password_check" }, result.account, if proved { result.attributes } else { Vec::new() }, unavailable)
            }
            None => {
                let decision = engine.authenticate(request).await;
                let unavailable = decision.database_failed && !decision.accepted();
                (decision.accepted(), "script", "script", credentials.user.clone(), decision.response_attributes, unavailable)
            }
        };
        // Not a refusal: ERROR sends the device to its next server (RFC 8907 section 5.2).
        if unavailable {
            crate::lua_engine::log_line(tracing::Level::WARN, format!("TACACS+ authentication for client {} answered ERROR: the cluster database could not be used", client.name));
            return Login::Reply(authen_reply(AUTHEN_STATUS_ERROR, 0, &[]));
        }
        // The password was accepted: the second factor asks for the code, or checks one.
        if let (true, Some(Factor { mode, factor, can_ask })) = (accepted, &factor) {
            let request = crate::second_factor::SecondFactorRequest { protocol: "tacacs", client, mode: *mode, user: &factor_user, password: factor_password.as_deref().map(Vec::as_slice), state: None };
            let accepted = crate::second_factor::Accepted { attributes: std::mem::take(&mut replies), account: account.clone() };
            let account = account.as_deref().unwrap_or("");
            match factor.after_accept(&request, accepted).await {
                crate::second_factor::Decision::Accept(attributes) => replies = attributes,
                crate::second_factor::Decision::Challenge { reply_message, .. } if *can_ask => {
                    record_login(client, source, user, account, chain_name, "mfa", "challenge");
                    let code_for = CodeFor { user: factor_user.clone(), account: account.to_string() };
                    return Login::AskCode { code_for, reply: authen_reply(AUTHEN_STATUS_GETDATA, REPLY_FLAG_NOECHO, reply_message.as_bytes()) };
                }
                // A PAP login cannot be asked for more.
                crate::second_factor::Decision::Challenge { .. } => {
                    let log = format!("TACACS+ client {}: a second factor asked {user} for a code a PAP login cannot be asked for, refused", client.name);
                    return Login::Reply(Self::second_factor_reply(client, source, user, account, chain_name, crate::second_factor::Decision::Reject { log: Some(log) }));
                }
                other => return Login::Reply(Self::second_factor_reply(client, source, user, account, chain_name, other)),
            }
        }
        crate::logging::auth_record(&crate::logging::AuthRecord {
            protocol: "tacacs",
            client: &client.name,
            source,
            user,
            account: account.as_deref().unwrap_or(""),
            chain: chain_name,
            handler,
            decision: if accepted { "accept" } else { "reject" },
        });
        Login::Reply(authen_reply(if accepted { AUTHEN_STATUS_PASS } else { AUTHEN_STATUS_FAIL }, 0, &server_msg(&replies, client)))
    }

    /// Checks the code an ASCII login was asked for after its password.
    async fn check_code(code_for: &CodeFor, code: &[u8], client: &Client, source: IpAddr, chained: bool, mode: crate::clients::SecondFactorMode, factor: &dyn crate::second_factor::SecondFactor) -> Vec<u8> {
        let request = crate::second_factor::SecondFactorRequest { protocol: "tacacs", client, mode, user: &code_for.user, password: None, state: None };
        let decision = factor.verify_code(&request, code).await;
        let chain = if chained { client.auth.as_deref().unwrap_or("default") } else { "script" };
        Self::second_factor_reply(client, source, &code_for.user, &code_for.account, chain, decision)
    }

    /// The reply for what the second factor decided, recorded with the handler `mfa`.
    fn second_factor_reply(client: &Client, source: IpAddr, user: &str, account: &str, chain: &str, decision: crate::second_factor::Decision) -> Vec<u8> {
        use crate::second_factor::Decision;
        match decision {
            Decision::Accept(attributes) => {
                record_login(client, source, user, account, chain, "mfa", "accept");
                authen_reply(AUTHEN_STATUS_PASS, 0, &server_msg(&attributes, client))
            }
            // Nothing more can be asked here.
            Decision::Challenge { .. } => {
                record_login(client, source, user, account, chain, "mfa", "reject");
                authen_reply(AUTHEN_STATUS_FAIL, 0, &[])
            }
            Decision::Reject { log } => {
                if let Some(log) = log {
                    crate::lua_engine::log_line(tracing::Level::WARN, log);
                }
                record_login(client, source, user, account, chain, "mfa", "reject");
                authen_reply(AUTHEN_STATUS_FAIL, 0, &[])
            }
            // ERROR sends the device to its next server, as when the database is unavailable.
            Decision::Unavailable { log } => {
                crate::lua_engine::log_line(tracing::Level::WARN, log);
                authen_reply(AUTHEN_STATUS_ERROR, 0, &[])
            }
        }
    }

    /// Builds the plaintext reply body for the packet type (RFC 8907 sections 5.2, 6.2, 7.2),
    /// and whether it is the ERROR for a body that did not decode.
    #[allow(clippy::too_many_arguments)]
    async fn reply_body(type_: u8, body: &[u8], engine: &Arc<LuaAuthEngine>, chain: Option<&AuthChain>, client: &Client, source: IpAddr, metrics: &Metrics, factor: Option<Factor<'_>>) -> (Vec<u8>, bool) {
        let request = |phase: &'static str, attributes| {
            let mut request = LuaRequest::new("tacacs", phase, attributes);
            request.client = client.name.clone();
            request
        };
        // Its device is better off asking a node that can reach the others.
        if type_ != TYPE_ACCT && crate::lua_engine::stopped_without_leader() {
            return match type_ {
                TYPE_AUTHEN => (authen_reply(AUTHEN_STATUS_ERROR, 0, &[]), false),
                _ => (author_response(AUTHOR_STATUS_ERROR, &[]), false),
            };
        }
        match type_ {
            TYPE_AUTHEN => {
                // status, flags, server_msg_len(2), data_len(2)
                let Some(attributes) = parse_authen_start(body) else {
                    metrics.tacacs_malformed.inc();
                    return (vec![AUTHEN_STATUS_ERROR, 0, 0, 0, 0, 0], true);
                };
                let credentials = start_credentials(&attributes);
                match Self::decide_login(credentials, attributes, engine, chain, client, source, factor).await {
                    Login::Reply(reply) => (reply, false),
                    // Only an ASCII login is asked for more, and it does not come here.
                    Login::AskCode { .. } => (authen_reply(AUTHEN_STATUS_FAIL, 0, &[]), false),
                }
            }
            TYPE_AUTHOR => {
                let Some(fields) = args_body_fields(body, 0) else {
                    metrics.tacacs_malformed.inc();
                    return (author_response(AUTHOR_STATUS_ERROR, &[]), true);
                };
                // The client's rules decide instead of the script.
                if let Some(group) = &client.authorize_group {
                    let reply = match group.decide(&fields[3..]) {
                        Some(reply) => author_response(AUTHOR_STATUS_PASS_ADD, reply),
                        None => author_response(AUTHOR_STATUS_FAIL, &[]),
                    };
                    return (reply, false);
                }
                let Some(attributes) = args_attributes(body, 0, &fields) else {
                    metrics.tacacs_malformed.inc();
                    return (author_response(AUTHOR_STATUS_ERROR, &[]), true);
                };
                let mut request = request("authorization", attributes);
                // Each AV pair travels with a 1-octet length.
                request.validator = Some(Arc::new(|name: &str, value: &AttrValue| {
                    if name != SERVER_MSG && av_pair(name, value).len() > 255 { Err(format!("AV pair '{}' is longer than 255 octets", name)) } else { Ok(()) }
                }));
                let decision = engine.authenticate(request).await;
                if decision.database_failed && !decision.accepted() {
                    crate::lua_engine::log_line(tracing::Level::WARN, format!("TACACS+ authorization for client {} answered ERROR: the cluster database could not be used", client.name));
                    return (author_response(AUTHOR_STATUS_ERROR, &[]), false);
                }
                let message = server_msg(&decision.response_attributes, client);
                if decision.accepted() {
                    let args: Vec<Vec<u8>> = decision.response_attributes.iter().filter(|(n, _)| n != SERVER_MSG).map(|(n, v)| av_pair(n, v)).collect();
                    // The count travels in one octet, and a device would misread what follows.
                    if args.len() > 255 {
                        crate::lua_engine::log_line(tracing::Level::ERROR, format!("TACACS+ authorization for client {} has {} AV pairs, more than the 255 a reply holds", client.name, args.len()));
                        return (author_response(AUTHOR_STATUS_ERROR, &[]), false);
                    }
                    (author_response_with(AUTHOR_STATUS_PASS_ADD, &args, &message), false)
                } else {
                    (author_response_with(AUTHOR_STATUS_FAIL, &[], &message), false)
                }
            }
            TYPE_ACCT => {
                // server_msg_len(2), data_len(2), status
                let Some(attributes) = parse_acct_request(body) else {
                    metrics.tacacs_malformed.inc();
                    return (vec![0, 0, 0, 0, ACCT_STATUS_ERROR], true);
                };
                // SUCCESS only if the script accepted, so the client knows nothing is recorded otherwise.
                let decision = engine.authenticate(request("accounting", attributes)).await;
                let message = server_msg(&decision.response_attributes, client);
                let mut reply = (message.len() as u16).to_be_bytes().to_vec();
                reply.extend_from_slice(&[0, 0, if decision.accepted() { ACCT_STATUS_SUCCESS } else { ACCT_STATUS_ERROR }]);
                reply.extend_from_slice(&message);
                (reply, false)
            }
            _ => {
                tracing::warn!("Unknown TACACS+ type: {}", type_);
                // Unknown types get an authentication-style FAIL.
                (vec![AUTHEN_STATUS_FAIL, 0, 0, 0, 0, 0], false)
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::auth_chain::Handler;
    use crate::clients::ClientOptions;
    use tokio::io::duplex;

    /// Serves a connection from a single configured client.
    pub(crate) async fn serve_as<S: AsyncRead + AsyncWrite + Unpin>(socket: S, client: &Client, engine: &Arc<LuaAuthEngine>, chains: Option<&Arc<AuthChains>>, max_body_len: usize) -> io::Result<()> {
        let mut clients = ClientTable::default();
        let options = ClientOptions { auth: client.auth.clone(), authorize_group: client.authorize_group.clone(), ..ClientOptions::default() };
        clients.add("127.0.0.1", Some(client.name.clone()), client.secret.clone(), options).unwrap();
        let live = Live::new(TacacsState { clients: Arc::new(clients), engine: engine.clone(), chains: chains.cloned(), enable: EnableMode::default(), tls_only: false, second_factor: None });
        TacacsServer::handle_connection(socket, "127.0.0.1".parse().unwrap(), &live, &Metrics::default(), max_body_len, Timeouts::default()).await
    }

    const SECRET: &[u8] = b"tac_secret";
    const VERSION: u8 = 0xc0;

    pub(crate) fn test_client() -> Client {
        Client { max_radsec_connections: None, radsec_subject: None, authorize_group: None, coa_sender: false, mfa: None, eap_methods: None, name: "local".to_string(), secret: SECRET.to_vec(), auth: None, require_message_authenticator: Default::default(), limit_proxy_state: Default::default(), coa_port: 3799, tls_name: None }
    }

    fn md5(parts: &[&[u8]]) -> [u8; 16] {
        let mut hasher = Md5::new();
        for part in parts {
            hasher.update(part);
        }
        hasher.finalize().into()
    }

    async fn test_engine() -> Arc<LuaAuthEngine> {
        script_engine("return 'accept'").await
    }

    pub(crate) async fn script_engine(script: &str) -> Arc<LuaAuthEngine> {
        let db = crate::test_support::test_db();
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), db, script).unwrap());
        engine
    }

    fn encrypted_request(type_: u8, seq_num: u8, flags: u8, body: &[u8]) -> Vec<u8> {
        TacacsPacket {
            version: VERSION,
            type_,
            seq_num,
            flags,
            session_id: 0x1234_5678,
            body: TacacsPacket::encrypt_decrypt(body, SECRET, 0x1234_5678, VERSION, seq_num),
        }
        .encode()
    }

    /// Sends one request through handle_connection and returns the decrypted reply.
    async fn exchange(request: Vec<u8>) -> TacacsPacket {
        exchange_with(test_engine().await, request).await
    }

    async fn exchange_with(engine: Arc<LuaAuthEngine>, request: Vec<u8>) -> TacacsPacket {
        let (mut client, server) = duplex(4096);
        let server_task = tokio::spawn(async move { serve_as(server, &test_client(), &engine, None, MAX_BODY_LEN).await });
        client.write_all(&request).await.unwrap();
        let mut reply = read_packet(&mut client, MAX_BODY_LEN).await.unwrap().unwrap();
        drop(client);
        server_task.await.unwrap().unwrap();
        reply.body = TacacsPacket::encrypt_decrypt(&reply.body, SECRET, reply.session_id, reply.version, reply.seq_num);
        reply
    }

    /// Sends a request and reports whether the server closed the connection without replying.
    async fn closes_without_reply(request: Vec<u8>) -> bool {
        let engine = test_engine().await;
        let (mut client, server) = duplex(4096);
        let server_task = tokio::spawn(async move { serve_as(server, &test_client(), &engine, None, MAX_BODY_LEN).await });
        client.write_all(&request).await.unwrap();
        let mut buf = Vec::new();
        let n = client.read_to_end(&mut buf).await.unwrap();
        n == 0 && server_task.await.unwrap().is_err()
    }

    fn state_with_secret(secret: Option<&[u8]>, engine: &Arc<LuaAuthEngine>) -> TacacsState {
        let mut clients = ClientTable::default();
        if let Some(secret) = secret {
            clients.add("127.0.0.1", None, secret.to_vec(), ClientOptions::default()).unwrap();
        } else {
            clients.add("10.0.0.1", None, b"k".to_vec(), ClientOptions::default()).unwrap();
        }
        TacacsState { clients: Arc::new(clients), engine: engine.clone(), chains: None, enable: EnableMode::default(), tls_only: false, second_factor: None }
    }

    /// A connection served from 127.0.0.1 under a state a test can replace.
    fn open_connection(engine: &Arc<LuaAuthEngine>) -> (tokio::io::DuplexStream, Arc<Live<TacacsState>>, tokio::task::JoinHandle<io::Result<()>>) {
        let live = Arc::new(Live::new(state_with_secret(Some(SECRET), engine)));
        let (stream, server) = duplex(4096);
        let serving = live.clone();
        let task = tokio::spawn(async move { TacacsServer::handle_connection(server, "127.0.0.1".parse().unwrap(), &serving, &Metrics::default(), MAX_BODY_LEN, Timeouts::default()).await });
        (stream, live, task)
    }

    #[tokio::test]
    async fn test_client_removed_closes_connection() {
        let engine = script_engine("return 'accept'").await;
        let (mut stream, live, task) = open_connection(&engine);
        let request = request_with_key(SECRET, &start_body(b"alice", b"secret"));
        stream.write_all(&request).await.unwrap();
        read_packet(&mut stream, MAX_BODY_LEN).await.unwrap().unwrap();

        live.set(state_with_secret(None, &engine));
        stream.write_all(&request).await.unwrap();
        let mut rest = Vec::new();
        assert_eq!(stream.read_to_end(&mut rest).await.unwrap(), 0, "no reply is sent");
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_secret_change_applies_to_open_connection() {
        let engine = script_engine("return 'accept'").await;
        let (mut stream, live, task) = open_connection(&engine);
        stream.write_all(&request_with_key(SECRET, &start_body(b"alice", b"secret"))).await.unwrap();
        read_packet(&mut stream, MAX_BODY_LEN).await.unwrap().unwrap();

        const NEW_SECRET: &[u8] = b"a new secret";
        live.set(state_with_secret(Some(NEW_SECRET), &engine));
        stream.write_all(&request_with_key(NEW_SECRET, &start_body(b"alice", b"secret"))).await.unwrap();
        let reply = read_packet(&mut stream, MAX_BODY_LEN).await.unwrap().unwrap();
        let body = TacacsPacket::encrypt_decrypt(&reply.body, NEW_SECRET, reply.session_id, reply.version, reply.seq_num);
        assert_eq!(body[0], AUTHEN_STATUS_PASS);
        drop(stream);
        task.await.unwrap().unwrap();
    }

    const ASCII_SESSION: u32 = 0x0a5c_1100;

    fn ascii_start_body(user: &[u8]) -> Vec<u8> {
        let port = b"tty0";
        let mut body = vec![ACTION_LOGIN, 0x01, AUTHEN_TYPE_ASCII, 0x01, user.len() as u8, port.len() as u8, 0, 0];
        body.extend_from_slice(user);
        body.extend_from_slice(port);
        body
    }

    fn continue_body(user_msg: &[u8], flags: u8) -> Vec<u8> {
        let mut body = (user_msg.len() as u16).to_be_bytes().to_vec();
        body.extend_from_slice(&[0, 0, flags]);
        body.extend_from_slice(user_msg);
        body
    }

    fn authen_packet(session_id: u32, seq_num: u8, body: &[u8]) -> Vec<u8> {
        TacacsPacket { version: VERSION, type_: TYPE_AUTHEN, seq_num, flags: 0, session_id, body: TacacsPacket::encrypt_decrypt(body, SECRET, session_id, VERSION, seq_num) }.encode()
    }

    /// The sequence number, status, flags and message of the next reply.
    async fn next_reply<S: AsyncRead + Unpin>(stream: &mut S) -> (u8, u8, u8, Vec<u8>) {
        let reply = tokio::time::timeout(std::time::Duration::from_secs(10), read_packet(stream, MAX_BODY_LEN)).await.expect("no reply").unwrap().expect("closed");
        let body = TacacsPacket::encrypt_decrypt(&reply.body, SECRET, reply.session_id, reply.version, reply.seq_num);
        let length = u16::from_be_bytes([body[2], body[3]]) as usize;
        assert_eq!(body.len(), 6 + length, "a reply without data");
        (reply.seq_num, body[0], body[1], body[6..].to_vec())
    }

    /// Whether nothing arrives on `stream` for a moment.
    async fn stays_silent<S: AsyncRead + Unpin>(stream: &mut S) -> bool {
        tokio::time::timeout(std::time::Duration::from_millis(100), read_packet(stream, MAX_BODY_LEN)).await.is_err()
    }

    struct AsciiConnection {
        stream: tokio::io::DuplexStream,
        metrics: Arc<Metrics>,
    }

    impl AsciiConnection {
        /// A connection from a client whose script accepts the password `secret`.
        async fn open() -> Self {
            Self::with(script_engine("return get_attr('data') == 'secret' and 'accept' or 'reject'").await, None).await
        }

        async fn with(engine: Arc<LuaAuthEngine>, chains: Option<Arc<AuthChains>>) -> Self {
            Self::with_enable(engine, chains, EnableMode::default()).await
        }

        async fn with_enable(engine: Arc<LuaAuthEngine>, chains: Option<Arc<AuthChains>>, enable: EnableMode) -> Self {
            let mut state = state_with_secret(Some(SECRET), &engine);
            state.chains = chains;
            state.enable = enable;
            let live = Live::new(state);
            let metrics = Arc::new(Metrics::default());
            let (stream, server) = duplex(4096);
            let counting = metrics.clone();
            tokio::spawn(async move { TacacsServer::handle_connection(server, "127.0.0.1".parse().unwrap(), &live, &counting, MAX_BODY_LEN, Timeouts::default()).await });
            Self { stream, metrics }
        }

        /// A connection from a client with `mfa = mode` and `FakeTacacsFactor`, whose script
        /// accepts the password `secret`.
        async fn with_factor(mode: crate::clients::SecondFactorMode) -> Result<Self, String> {
            let engine = script_engine("return get_attr('data') == 'secret' and 'accept' or 'reject'").await;
            let mut clients = ClientTable::default();
            clients.add("127.0.0.1", Some("nas-mfa".to_string()), SECRET.to_vec(), ClientOptions { mfa: Some(mode), ..ClientOptions::default() }).map_err(|e| format!("{e:?}"))?;
            let state = TacacsState { clients: Arc::new(clients), engine, chains: None, enable: EnableMode::default(), tls_only: false, second_factor: Some(Arc::new(FakeTacacsFactor)) };
            let live = Live::new(state);
            let metrics = Arc::new(Metrics::default());
            let (stream, server) = duplex(4096);
            let counting = metrics.clone();
            tokio::spawn(async move { TacacsServer::handle_connection(server, IpAddr::from([127, 0, 0, 1]), &live, &counting, MAX_BODY_LEN, Timeouts::default()).await });
            Ok(Self { stream, metrics })
        }

        async fn send(&mut self, session_id: u32, seq_num: u8, body: &[u8]) {
            self.stream.write_all(&authen_packet(session_id, seq_num, body)).await.unwrap();
        }

        async fn ask(&mut self, session_id: u32, seq_num: u8, body: &[u8]) -> (u8, u8, u8, Vec<u8>) {
            self.send(session_id, seq_num, body).await;
            next_reply(&mut self.stream).await
        }

        /// A login's status after its START (with `user`) and each answer in turn.
        async fn login(&mut self, user: &[u8], answers: &[&[u8]]) -> u8 {
            let (mut seq, mut status, ..) = self.ask(ASCII_SESSION, 1, &ascii_start_body(user)).await;
            for answer in answers {
                (seq, status, ..) = self.ask(ASCII_SESSION, seq + 1, &continue_body(answer, 0)).await;
            }
            status
        }
    }

    /// A second factor for tests: `append` logins give the code 123456 after the password,
    /// `challenge` logins are asked for it; the code 999999 finds the database unavailable and the
    /// User-Name `tf-refused` is refused before the chain.
    struct FakeTacacsFactor;

    impl crate::second_factor::SecondFactor for FakeTacacsFactor {
        fn before_chain<'a>(&'a self, request: &'a crate::second_factor::SecondFactorRequest<'a>) -> futures::future::BoxFuture<'a, crate::second_factor::Before> {
            use crate::second_factor::Before;
            Box::pin(async move {
                let password = request.password.unwrap_or_default();
                match request.user {
                    "tf-refused" => Before::Reject { log: Some("tf refused the login".to_string()) },
                    _ if request.mode == crate::clients::SecondFactorMode::Append && password.len() > 6 => Before::Continue { password: Some(zeroize::Zeroizing::new(password[..password.len() - 6].to_vec())) },
                    _ => Before::Continue { password: None },
                }
            })
        }

        fn after_accept<'a>(&'a self, request: &'a crate::second_factor::SecondFactorRequest<'a>, accepted: crate::second_factor::Accepted) -> futures::future::BoxFuture<'a, crate::second_factor::Decision> {
            use crate::second_factor::Decision;
            Box::pin(async move {
                match request.mode {
                    crate::clients::SecondFactorMode::Challenge => Decision::Challenge { state: Vec::new(), reply_message: "Code: ".to_string() },
                    crate::clients::SecondFactorMode::Append if request.password.unwrap_or_default().ends_with(b"123456") => Decision::Accept(accepted.attributes),
                    crate::clients::SecondFactorMode::Append => Decision::Reject { log: None },
                }
            })
        }

        fn verify_code<'a>(&'a self, request: &'a crate::second_factor::SecondFactorRequest<'a>, code: &'a [u8]) -> futures::future::BoxFuture<'a, crate::second_factor::Decision> {
            use crate::second_factor::Decision;
            Box::pin(async move {
                match code {
                    b"123456" => Decision::Accept(Vec::new()),
                    b"999999" => Decision::Unavailable { log: "tf could not read its table".to_string() },
                    _ => Decision::Reject { log: Some(format!("wrong code for {}", request.user)) },
                }
            })
        }
    }

    /// An ASCII login of a `challenge` client: GETPASS, then GETDATA for the code, then PASS;
    /// a wrong code fails, and an unavailable database answers ERROR.
    #[tokio::test]
    async fn test_second_factor_ascii_challenge() -> Result<(), String> {
        let mut connection = AsciiConnection::with_factor(crate::clients::SecondFactorMode::Challenge).await?;
        let (seq, status, flags, _) = connection.ask(ASCII_SESSION, 1, &ascii_start_body(b"tf-ascii")).await;
        assert_eq!((seq, status, flags), (2, AUTHEN_STATUS_GETPASS, REPLY_FLAG_NOECHO));
        let (seq, status, flags, prompt) = connection.ask(ASCII_SESSION, 3, &continue_body(b"secret", 0)).await;
        assert_eq!((seq, status, flags, prompt.as_slice()), (4, AUTHEN_STATUS_GETDATA, REPLY_FLAG_NOECHO, b"Code: ".as_slice()));
        let (seq, status, ..) = connection.ask(ASCII_SESSION, 5, &continue_body(b"123456", 0)).await;
        assert_eq!((seq, status), (6, AUTHEN_STATUS_PASS));
        let records = crate::logging::records_for("tf-ascii");
        let decisions: Vec<(&str, &str)> = records.iter().map(|record| (record.handler.as_str(), record.decision.as_str())).collect();
        assert_eq!(decisions, [("mfa", "challenge"), ("mfa", "accept")]);

        let log = crate::test_support::radius::logged_during(async {
            assert_eq!(connection.login(b"tf-wrong", &[b"secret", b"000000"]).await, AUTHEN_STATUS_FAIL);
            assert_eq!(connection.login(b"tf-unavailable", &[b"secret", b"999999"]).await, AUTHEN_STATUS_ERROR);
        })
        .await;
        assert!(log.contains("wrong code for tf-wrong") && log.contains("tf could not read its table"), "{log}");
        // A wrong password is refused before any code is asked for.
        assert_eq!(connection.login(b"tf-bad-password", &[b"wrong"]).await, AUTHEN_STATUS_FAIL);
        assert_eq!(crate::logging::records_for("tf-bad-password")[0].handler, "script");
        Ok(())
    }

    /// The code after the password: an ASCII login of an `append` client, and a PAP login, which
    /// gives it that way whatever the client's `mfa`.
    #[tokio::test]
    async fn test_second_factor_appended() -> Result<(), String> {
        let mut connection = AsciiConnection::with_factor(crate::clients::SecondFactorMode::Append).await?;
        assert_eq!(connection.login(b"tf-append", &[b"secret123456"]).await, AUTHEN_STATUS_PASS);
        assert_eq!(connection.login(b"tf-append-wrong", &[b"secret000000"]).await, AUTHEN_STATUS_FAIL);
        let mut connection = AsciiConnection::with_factor(crate::clients::SecondFactorMode::Challenge).await?;
        assert_eq!(connection.ask(ASCII_SESSION, 1, &start_body(b"tf-pap", b"secret123456")).await.1, AUTHEN_STATUS_PASS);
        assert_eq!(connection.ask(ASCII_SESSION, 1, &start_body(b"tf-pap-no-code", b"secret")).await.1, AUTHEN_STATUS_FAIL);
        assert_eq!(connection.ask(ASCII_SESSION, 1, &start_body(b"tf-refused", b"secret123456")).await.1, AUTHEN_STATUS_FAIL);
        assert_eq!(crate::logging::records_for("tf-pap-no-code")[0].handler, "mfa");
        Ok(())
    }

    #[tokio::test]
    async fn test_ascii_full_exchange() {
        let mut connection = AsciiConnection::open().await;
        let (seq, status, flags, prompt) = connection.ask(ASCII_SESSION, 1, &ascii_start_body(b"")).await;
        assert_eq!((seq, status, flags), (2, AUTHEN_STATUS_GETUSER, 0));
        assert!(!prompt.is_empty());
        let (seq, status, flags, prompt) = connection.ask(ASCII_SESSION, 3, &continue_body(b"alice", 0)).await;
        assert_eq!((seq, status, flags), (4, AUTHEN_STATUS_GETPASS, REPLY_FLAG_NOECHO));
        assert!(!prompt.is_empty());
        let (seq, status, ..) = connection.ask(ASCII_SESSION, 5, &continue_body(b"secret", 0)).await;
        assert_eq!((seq, status), (6, AUTHEN_STATUS_PASS));
    }

    #[tokio::test]
    async fn test_ascii_login_record() {
        let mut connection = AsciiConnection::open().await;
        connection.login(b"rec-tacacs-ascii", &[b"secret"]).await;
        let records = crate::logging::records_for("rec-tacacs-ascii");
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!((records[0].protocol.as_str(), records[0].source), ("tacacs", "127.0.0.1".parse().unwrap()));
    }

    #[tokio::test]
    async fn test_ascii_username_in_start() {
        let mut connection = AsciiConnection::open().await;
        let (seq, status, flags, _) = connection.ask(ASCII_SESSION, 1, &ascii_start_body(b"alice")).await;
        assert_eq!((seq, status, flags), (2, AUTHEN_STATUS_GETPASS, REPLY_FLAG_NOECHO));
    }

    #[tokio::test]
    async fn test_ascii_wrong_password() {
        let mut connection = AsciiConnection::open().await;
        assert_eq!(connection.login(b"alice", &[b"wrong"]).await, AUTHEN_STATUS_FAIL);
    }

    #[tokio::test]
    async fn test_ascii_through_a_chain() {
        let db = crate::test_support::test_db();
        db.execute("CREATE TABLE IF NOT EXISTS tacacs_ascii_users (username TEXT, password TEXT)").await.unwrap();
        db.execute("INSERT INTO tacacs_ascii_users VALUES ('alice', 'secret')").await.unwrap();
        let handler = Handler::Sql {
            db,
            user_query: "SELECT password FROM tacacs_ascii_users WHERE username = ?".to_string(),
            reply_query: None,
            certificate_query: None, default_user: None,
            timeout: std::time::Duration::from_secs(5),
        };
        let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![handler]);
        let chains = Arc::new(AuthChains::new(Some(chain), Vec::new()));
        // The top-level script rejects everything, so a PASS proves the chain decided.
        let engine = script_engine("return 'reject'").await;
        let mut connection = AsciiConnection::with(engine.clone(), Some(chains.clone())).await;
        assert_eq!(connection.login(b"alice", &[b"secret"]).await, AUTHEN_STATUS_PASS);
        let mut connection = AsciiConnection::with(engine, Some(chains)).await;
        assert_eq!(connection.login(b"alice", &[b"wrong"]).await, AUTHEN_STATUS_FAIL);
    }

    #[tokio::test]
    async fn test_ascii_script_sees_the_result() {
        let script = "log('ascii-script-ran') return (get_attr('user') == 'alice' and get_attr('data') == 'secret' and get_attr('authen_type') == 1) and 'accept' or 'reject'";
        let mut connection = AsciiConnection::with(script_engine(script).await, None).await;
        assert_eq!(connection.login(b"", &[b"alice", b"secret"]).await, AUTHEN_STATUS_PASS);
        let runs = crate::test_support::logged_errors().lock().unwrap().iter().filter(|line| line.contains("ascii-script-ran")).count();
        assert_eq!(runs, 1);
    }

    #[tokio::test]
    async fn test_ascii_counted_once() {
        let mut connection = AsciiConnection::open().await;
        connection.login(b"", &[b"alice", b"secret"]).await;
        assert_eq!(connection.metrics.tacacs_authen.get(), 1);
    }

    #[tokio::test]
    async fn test_pap_unchanged() {
        let mut connection = AsciiConnection::open().await;
        let (seq, status, ..) = connection.ask(ASCII_SESSION, 1, &start_body(b"alice", b"secret")).await;
        assert_eq!((seq, status), (2, AUTHEN_STATUS_PASS));
    }

    /// Changing a password, and the actions RFC 8907 advises against, are not done here, so
    /// they fail before anything is asked, even with a script that accepts everything.
    #[tokio::test]
    async fn test_actions_other_than_login_fail() {
        for (action, authen_type) in [(0x02, AUTHEN_TYPE_ASCII), (0x03, AUTHEN_TYPE_PAP), (0x04, AUTHEN_TYPE_PAP)] {
            let mut connection = AsciiConnection::with(script_engine("return 'accept'").await, None).await;
            let mut body = ascii_start_body(b"alice");
            body[0] = action;
            body[2] = authen_type;
            let (seq, status, ..) = connection.ask(ASCII_SESSION, 1, &body).await;
            assert_eq!((seq, status), (2, AUTHEN_STATUS_FAIL), "action {action}");
        }
    }

    #[tokio::test]
    async fn test_unsupported_authentication_types_restart() {
        const CHAP: u8 = 3;
        const MSCHAPV2: u8 = 6;
        let cases = [
            (ACTION_LOGIN, 1, CHAP, EnableMode::Deny, AUTHEN_STATUS_RESTART),
            (ACTION_LOGIN, 1, MSCHAPV2, EnableMode::Deny, AUTHEN_STATUS_RESTART),
            (ACTION_LOGIN, AUTHEN_SVC_ENABLE, MSCHAPV2, EnableMode::Deny, AUTHEN_STATUS_FAIL),
            (ACTION_LOGIN, AUTHEN_SVC_ENABLE, CHAP, EnableMode::Login, AUTHEN_STATUS_RESTART),
            (0x02, 1, CHAP, EnableMode::Deny, AUTHEN_STATUS_FAIL),
            (ACTION_LOGIN, 1, 4, EnableMode::Deny, AUTHEN_STATUS_RESTART),
            (ACTION_LOGIN, 1, 5, EnableMode::Deny, AUTHEN_STATUS_RESTART),
        ];
        for (action, service, authen_type, mode, expected) in cases {
            // The script would accept anything: these never reach it.
            let mut connection = AsciiConnection::with_enable(script_engine("return 'accept'").await, None, mode).await;
            let mut body = ascii_start_body(b"alice");
            (body[0], body[2], body[3]) = (action, authen_type, service);
            let (seq, status, ..) = connection.ask(ASCII_SESSION, 1, &body).await;
            assert_eq!((seq, status), (2, expected), "action {action} service {service} type {authen_type} {mode:?}");
        }
        // PAP is decided as before.
        let mut connection = AsciiConnection::with(script_engine("return 'accept'").await, None).await;
        let (_, status, ..) = connection.ask(ASCII_SESSION, 1, &start_body(b"alice", b"secret")).await;
        assert_eq!(status, AUTHEN_STATUS_PASS);
    }

    /// A chain whose only handler compares the password `secret` stored for alice in plain text.
    async fn plain_password_chain() -> Result<Arc<AuthChains>, Box<dyn std::error::Error>> {
        let db = crate::test_support::test_db();
        db.execute("CREATE TABLE IF NOT EXISTS tacacs_chap_users (username TEXT, password TEXT)").await?;
        db.execute("DELETE FROM tacacs_chap_users").await?;
        db.execute("INSERT INTO tacacs_chap_users VALUES ('alice', 'secret')").await?;
        let handler = Handler::Sql {
            db,
            user_query: "SELECT password FROM tacacs_chap_users WHERE username = ?".to_string(),
            reply_query: None,
            certificate_query: None, default_user: None,
            timeout: std::time::Duration::from_secs(5),
        };
        let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![handler]);
        Ok(Arc::new(AuthChains::new(Some(chain), Vec::new())))
    }

    fn typed_start_body(authen_type: u8, service: u8, user: &[u8], data: &[u8]) -> Vec<u8> {
        let mut body = start_body(user, data);
        (body[2], body[3]) = (authen_type, service);
        body
    }

    fn chap_data(id: u8, challenge: &[u8], password: &[u8]) -> Vec<u8> {
        let mut data = vec![id];
        data.extend_from_slice(challenge);
        data.extend_from_slice(&md5(&[&[id], password, challenge]));
        data
    }

    fn mschapv2_data(id: u8, challenge: &[u8; 16], password: &str) -> Vec<u8> {
        let peer = [0x5a; 16];
        let mut data = vec![id];
        data.extend_from_slice(challenge);
        data.extend_from_slice(&peer);
        data.extend_from_slice(&[0; 8]);
        data.extend_from_slice(&crate::mschap::response_for(password, "alice", &peer, challenge));
        data.push(0);
        data
    }

    #[tokio::test]
    async fn test_chap_and_mschapv2_through_a_chain() -> Result<(), Box<dyn std::error::Error>> {
        let chains = plain_password_chain().await?;
        let challenge = [0x11; 16];
        let cases: Vec<(&str, Vec<u8>, u8)> = vec![
            ("CHAP, right password", typed_start_body(AUTHEN_TYPE_CHAP, 1, b"alice", &chap_data(7, &challenge, b"secret")), AUTHEN_STATUS_PASS),
            ("CHAP, wrong password", typed_start_body(AUTHEN_TYPE_CHAP, 1, b"alice", &chap_data(7, &challenge, b"wrong")), AUTHEN_STATUS_FAIL),
            ("CHAP, unknown user", typed_start_body(AUTHEN_TYPE_CHAP, 1, b"bob", &chap_data(7, &challenge, b"secret")), AUTHEN_STATUS_FAIL),
            ("CHAP, 8-octet challenge", typed_start_body(AUTHEN_TYPE_CHAP, 1, b"alice", &chap_data(7, &challenge[..8], b"secret")), AUTHEN_STATUS_PASS),
            ("CHAP, 7-octet challenge", typed_start_body(AUTHEN_TYPE_CHAP, 1, b"alice", &chap_data(7, &challenge[..7], b"secret")), AUTHEN_STATUS_FAIL),
            ("CHAP, no data", typed_start_body(AUTHEN_TYPE_CHAP, 1, b"alice", b""), AUTHEN_STATUS_FAIL),
            ("MS-CHAPv2, right password", typed_start_body(AUTHEN_TYPE_MSCHAPV2, 1, b"alice", &mschapv2_data(9, &challenge, "secret")), AUTHEN_STATUS_PASS),
            ("MS-CHAPv2, wrong password", typed_start_body(AUTHEN_TYPE_MSCHAPV2, 1, b"alice", &mschapv2_data(9, &challenge, "wrong")), AUTHEN_STATUS_FAIL),
            ("MS-CHAPv2, one octet short", typed_start_body(AUTHEN_TYPE_MSCHAPV2, 1, b"alice", &mschapv2_data(9, &challenge, "secret")[..65]), AUTHEN_STATUS_FAIL),
            // Enable stays ASCII or PAP, and MS-CHAPv1 is not checked.
            ("CHAP enable", typed_start_body(AUTHEN_TYPE_CHAP, AUTHEN_SVC_ENABLE, b"alice", &chap_data(7, &challenge, b"secret")), AUTHEN_STATUS_RESTART),
            ("MS-CHAPv1", typed_start_body(5, 1, b"alice", &[0; 66]), AUTHEN_STATUS_RESTART),
        ];
        // A chain of scripts that do not compare passwords cannot check a response either.
        let scripts_only = Arc::new(AuthChains::new(Some(AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![
            Handler::Lua(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'accept'")?),
        ])), Vec::new()));
        let mut connection = AsciiConnection::with(script_engine("return 'accept'").await, Some(scripts_only)).await;
        let chap = typed_start_body(AUTHEN_TYPE_CHAP, 1, b"alice", &chap_data(7, &challenge, b"secret"));
        assert_eq!(connection.ask(ASCII_SESSION, 1, &chap).await.1, AUTHEN_STATUS_RESTART, "chain of scripts only");
        for (label, body, expected) in cases {
            // The top-level script rejects everything, so a PASS proves the chain decided.
            let mut connection = AsciiConnection::with_enable(script_engine("return 'reject'").await, Some(chains.clone()), EnableMode::Login).await;
            let (seq, status, ..) = connection.ask(ASCII_SESSION, 1, &body).await;
            assert_eq!((seq, status), (2, expected), "{label}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_chap_needs_a_handler_that_checked_it() -> Result<(), Box<dyn std::error::Error>> {
        // A policy script that accepts everyone, first, and a handler that compares passwords after:
        // the script's accept is no proof of a CHAP response.
        let script = Handler::Lua(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "set_attr('server_msg', 'Welcome, admin') return 'accept'")?);
        let db = crate::test_support::test_db();
        db.execute("CREATE TABLE IF NOT EXISTS tacacs_chap_proof (username TEXT, password TEXT)").await?;
        let sql = Handler::Sql { db, user_query: "SELECT password FROM tacacs_chap_proof WHERE username = ?".to_string(), reply_query: None, certificate_query: None, default_user: None, timeout: std::time::Duration::from_secs(5) };
        let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![script, sql]);
        let chains = Arc::new(AuthChains::new(Some(chain), Vec::new()));
        let challenge = [0x22; 16];
        for (label, body) in [
            ("CHAP", typed_start_body(AUTHEN_TYPE_CHAP, 1, b"nobody", &chap_data(7, &challenge, b"anything"))),
            ("MS-CHAPv2", typed_start_body(AUTHEN_TYPE_MSCHAPV2, 1, b"nobody", &mschapv2_data(9, &challenge, "anything"))),
        ] {
            let mut connection = AsciiConnection::with(script_engine("return 'accept'").await, Some(chains.clone())).await;
            let (_, status, _, rest) = connection.ask(ASCII_SESSION, 1, &body).await;
            assert_eq!(status, AUTHEN_STATUS_FAIL, "{label}");
            assert!(rest.is_empty(), "{label}: no server_msg of the refused accept");
        }
        Ok(())
    }

    #[test]
    fn test_challenge_secret() {
        let challenge = [3u8; 8];
        let data = chap_data(7, &challenge, b"secret");
        assert_eq!(challenge_secret(AUTHEN_TYPE_CHAP, &data), Some(Secret::Chap { id: 7, response: data[9..].to_vec(), challenge: challenge.to_vec() }));
        let data = mschapv2_data(9, &[1; 16], "secret");
        let expected = Secret::MsChapV2 { ident: 9, challenge: [1; 16], peer_challenge: [0x5a; 16], response: crate::mschap::response_for("secret", "alice", &[0x5a; 16], &[1; 16]) };
        assert_eq!(challenge_secret(AUTHEN_TYPE_MSCHAPV2, &data), Some(expected));
        assert_eq!(challenge_secret(AUTHEN_TYPE_MSCHAPV2, &[data.as_slice(), &[0]].concat()), None);
        assert_eq!(challenge_secret(AUTHEN_TYPE_PAP, &data), None);
        assert_eq!(challenge_secret(AUTHEN_TYPE_CHAP, &[]), None);
    }

    #[tokio::test]
    async fn test_enable_requests() {
        // The script accepts an enable to level 15 only, and a login never: what it sees decides.
        let script = "if get_attr('authen_service') == 2 and get_attr('priv_lvl') == 15 and get_attr('data') == 'secret' then return 'accept' end return 'reject'";
        let enable_start = |authen_type: u8| {
            let mut body = ascii_start_body(b"alice");
            (body[1], body[2], body[3]) = (15, authen_type, AUTHEN_SVC_ENABLE);
            if authen_type == AUTHEN_TYPE_PAP {
                body[7] = 6;
                body.extend_from_slice(b"secret");
            }
            body
        };
        let cases = [
            (EnableMode::Deny, enable_start(AUTHEN_TYPE_ASCII), AUTHEN_STATUS_FAIL),
            (EnableMode::Deny, enable_start(AUTHEN_TYPE_PAP), AUTHEN_STATUS_FAIL),
            (EnableMode::Login, enable_start(AUTHEN_TYPE_ASCII), AUTHEN_STATUS_GETPASS),
            (EnableMode::Login, enable_start(AUTHEN_TYPE_PAP), AUTHEN_STATUS_PASS),
            (EnableMode::Deny, ascii_start_body(b"alice"), AUTHEN_STATUS_GETPASS),
        ];
        for (index, (mode, body, expected)) in cases.into_iter().enumerate() {
            // The first case leaves the setting at its default.
            let mut connection = if index == 0 { AsciiConnection::with(script_engine(script).await, None).await } else { AsciiConnection::with_enable(script_engine(script).await, None, mode).await };
            let (seq, status, ..) = connection.ask(ASCII_SESSION, 1, &body).await;
            assert_eq!((seq, status), (2, expected), "{mode:?} {body:?}");
            if expected == AUTHEN_STATUS_GETPASS {
                // The rest of an ASCII exchange: an enable passes, a login does not.
                let (_, status, ..) = connection.ask(ASCII_SESSION, 3, &continue_body(b"secret", 0)).await;
                assert_eq!(status, if body[3] == AUTHEN_SVC_ENABLE { AUTHEN_STATUS_PASS } else { AUTHEN_STATUS_FAIL }, "{mode:?}");
            }
        }
    }

    #[tokio::test]
    async fn test_ascii_empty_username() {
        for (empty_answers, last) in [(1usize, AUTHEN_STATUS_GETUSER), (2, AUTHEN_STATUS_GETUSER), (3, AUTHEN_STATUS_FAIL)] {
            let mut connection = AsciiConnection::open().await;
            let answers = vec![&b""[..]; empty_answers];
            assert_eq!(connection.login(b"", &answers).await, last, "{empty_answers} empty answers");
        }
    }

    #[tokio::test]
    async fn test_ascii_abort() {
        let mut connection = AsciiConnection::open().await;
        connection.ask(ASCII_SESSION, 1, &ascii_start_body(b"")).await;
        connection.send(ASCII_SESSION, 3, &continue_body(b"", CONTINUE_FLAG_ABORT)).await;
        assert!(stays_silent(&mut connection.stream).await, "an abort got a reply");
        assert_eq!(connection.login(b"alice", &[b"secret"]).await, AUTHEN_STATUS_PASS, "the connection is still usable");
    }

    #[tokio::test]
    async fn test_ascii_continue_without_a_session() {
        let mut connection = AsciiConnection::open().await;
        let (seq, status, ..) = connection.ask(ASCII_SESSION, 3, &continue_body(b"alice", 0)).await;
        assert_eq!((seq, status), (4, AUTHEN_STATUS_ERROR));
    }

    #[tokio::test]
    async fn test_ascii_wrong_sequence_number() {
        let mut connection = AsciiConnection::open().await;
        connection.ask(ASCII_SESSION, 1, &ascii_start_body(b"")).await;
        assert_eq!(connection.ask(ASCII_SESSION, 5, &continue_body(b"alice", 0)).await.1, AUTHEN_STATUS_ERROR);
        assert_eq!(connection.ask(ASCII_SESSION, 3, &continue_body(b"alice", 0)).await.1, AUTHEN_STATUS_ERROR, "the session ended");
    }

    #[tokio::test]
    async fn test_ascii_too_many_sessions() {
        let mut connection = AsciiConnection::open().await;
        for session in 1..=8 {
            assert_eq!(connection.ask(session, 1, &ascii_start_body(b"alice")).await.1, AUTHEN_STATUS_GETPASS);
        }
        assert_eq!(connection.ask(9, 1, &ascii_start_body(b"alice")).await.1, AUTHEN_STATUS_ERROR);
        for session in 1..=8 {
            assert_eq!(connection.ask(session, 3, &continue_body(b"secret", 0)).await.1, AUTHEN_STATUS_PASS, "session {session}");
        }
        assert_eq!(connection.ask(9, 1, &ascii_start_body(b"alice")).await.1, AUTHEN_STATUS_GETPASS, "room again");
    }

    #[test]
    fn test_ascii_expired_session() {
        let start = TacacsPacket { version: VERSION, type_: TYPE_AUTHEN, seq_num: 1, flags: 0, session_id: 7, body: vec![] };
        let answer = TacacsPacket { seq_num: 3, ..TacacsPacket { version: VERSION, type_: TYPE_AUTHEN, seq_num: 1, flags: 0, session_id: 7, body: vec![] } };
        let is_error = |step: AuthenStep| matches!(step, AuthenStep::Reply(reply) if reply[0] == AUTHEN_STATUS_ERROR);
        let started = Instant::now();

        let mut logins = AsciiLogins::default();
        logins.step(&start, &ascii_start_body(b"alice"), started);
        let in_time = logins.step(&answer, &continue_body(b"secret", 0), started + ASCII_LOGIN_LIMIT - Duration::from_secs(1));
        assert!(matches!(in_time, AuthenStep::Finished { .. }));

        let mut logins = AsciiLogins::default();
        logins.step(&start, &ascii_start_body(b"alice"), started);
        assert!(is_error(logins.step(&answer, &continue_body(b"secret", 0), started + ASCII_LOGIN_LIMIT)));
    }

    #[tokio::test]
    async fn test_ascii_malformed_continue() {
        for body in [vec![0u8, 1], vec![0u8, 9, 0, 0, 0, b'a'], vec![0u8, 0, 0, 9, 0]] {
            let mut connection = AsciiConnection::open().await;
            connection.ask(ASCII_SESSION, 1, &ascii_start_body(b"")).await;
            assert_eq!(connection.ask(ASCII_SESSION, 3, &body).await.1, AUTHEN_STATUS_ERROR, "{body:?}");
        }
    }

    #[tokio::test]
    async fn test_requests_counted() {
        let engine = script_engine("return 'accept'").await;
        let live = Live::new(state_with_secret(Some(SECRET), &engine));
        let metrics = Metrics::default();
        let (mut stream, server) = duplex(4096);
        stream.write_all(&request_with_key(SECRET, &start_body(b"alice", b"secret"))).await.unwrap();
        drop(stream);
        TacacsServer::handle_connection(server, "127.0.0.1".parse().unwrap(), &live, &metrics, MAX_BODY_LEN, Timeouts::default()).await.unwrap_or(());
        assert_eq!((metrics.tacacs_authen.get(), metrics.tacacs_author.get(), metrics.tacacs_acct.get()), (1, 0, 0));
    }

    #[tokio::test]
    async fn test_connections_counted() {
        let engine = script_engine("return 'accept'").await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let metrics = Arc::new(Metrics::default());
        let server = Arc::new(TacacsServer::from_state(addr, state_with_secret(Some(SECRET), &engine), MAX_BODY_LEN).with_max_connections(1).with_metrics(metrics.clone()));
        tokio::spawn(async move { server.run().await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let first = tokio::net::TcpStream::connect(addr).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!((metrics.tacacs_accepted.get(), metrics.tacacs_open.get()), (1, 1));
        let mut second = tokio::net::TcpStream::connect(addr).await.unwrap();
        assert_eq!(second.read(&mut [0u8; 16]).await.unwrap(), 0);
        assert_eq!(metrics.tacacs_refused.get(), 1);
        drop(first);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(metrics.tacacs_open.get(), 0);
    }

    #[tokio::test]
    async fn test_packets_without_reload_answered() {
        let engine = script_engine("return 'accept'").await;
        let (mut stream, _live, task) = open_connection(&engine);
        for _ in 0..3 {
            stream.write_all(&request_with_key(SECRET, &start_body(b"alice", b"secret"))).await.unwrap();
            let reply = read_packet(&mut stream, MAX_BODY_LEN).await.unwrap().unwrap();
            let body = TacacsPacket::encrypt_decrypt(&reply.body, SECRET, reply.session_id, reply.version, reply.seq_num);
            assert_eq!(body[0], AUTHEN_STATUS_PASS);
        }
        drop(stream);
        task.await.unwrap().unwrap();
    }

    #[test]
    fn test_pseudo_pad_matches_rfc8907() {
        let session_id = 0x1234_5678u32;
        let pad = TacacsPacket::pseudo_pad(SECRET, session_id, VERSION, 1, 40);
        let first = md5(&[&session_id.to_be_bytes(), SECRET, &[VERSION, 1]]);
        let second = md5(&[&session_id.to_be_bytes(), SECRET, &[VERSION, 1], &first]);
        assert_eq!(&pad[..16], &first);
        assert_eq!(&pad[16..32], &second);
        assert_eq!(pad.len(), 40);
    }

    #[test]
    fn test_encrypt_then_decrypt_round_trip() {
        let body: Vec<u8> = (0..40u8).collect();
        let encrypted = TacacsPacket::encrypt_decrypt(&body, SECRET, 42, VERSION, 3);
        assert_ne!(encrypted, body);
        assert_eq!(TacacsPacket::encrypt_decrypt(&encrypted, SECRET, 42, VERSION, 3), body);
    }

    #[tokio::test]
    async fn test_packet_split_across_reads() {
        let packet = encrypted_request(TYPE_AUTHEN, 1, 0, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let (mut writer, mut reader) = duplex(4096);
        let read_task = tokio::spawn(async move { read_packet(&mut reader, MAX_BODY_LEN).await });

        writer.write_all(&packet[..HEADER_LEN]).await.unwrap();
        tokio::task::yield_now().await;
        writer.write_all(&packet[HEADER_LEN..]).await.unwrap();

        let read = read_task.await.unwrap().unwrap().unwrap();
        assert_eq!(read.body, packet[HEADER_LEN..].to_vec());
        assert_eq!(read.seq_num, 1);
    }

    #[tokio::test]
    async fn test_unsupported_major_version_closes_connection() {
        let mut packet = encrypted_request(TYPE_AUTHEN, 1, 0, &[0; 8]);
        packet[0] = 0x10;
        assert!(closes_without_reply(packet).await);
    }

    #[tokio::test]
    async fn test_unencrypted_flag_closes_connection() {
        let packet = encrypted_request(TYPE_AUTHEN, 1, UNENCRYPTED_FLAG, &[0; 8]);
        assert!(closes_without_reply(packet).await);
    }

    #[tokio::test]
    async fn test_sequence_number_a_client_cannot_send_closes_connection() {
        for seq_num in [0, 2, 4, 254, 255] {
            assert!(closes_without_reply(encrypted_request(TYPE_AUTHEN, seq_num, 0, &[0x01, 0x01, 0x02, 0x01, 0, 0, 0, 0])).await, "{seq_num}");
            assert!(closes_without_reply(encrypted_request(TYPE_AUTHOR, seq_num, 0, &[0x06, 0x01, 0x01, 0x01, 0, 0, 0, 0])).await, "{seq_num}");
        }
        assert_eq!(exchange(encrypted_request(TYPE_AUTHEN, 253, 0, &[0x01, 0x01, 0x02, 0x01, 0, 0, 0, 0])).await.seq_num, 254);
    }

    #[test]
    fn test_fields_rfc_8907_does_not_define() {
        let start = |action, priv_lvl, authen_type, service| parse_authen_start(&[action, priv_lvl, authen_type, service, 0, 0, 0, 0]);
        assert!(start(1, 15, 6, 9).is_some() && start(4, 0, 1, 0).is_some());
        for (action, priv_lvl, authen_type, service) in [(0, 1, 2, 1), (5, 1, 2, 1), (1, 16, 2, 1), (1, 1, 0, 1), (1, 1, 7, 1), (1, 1, 2, 10)] {
            assert!(start(action, priv_lvl, authen_type, service).is_none(), "{action} {priv_lvl} {authen_type} {service}");
        }
        let author = |priv_lvl, authen_type, service| parse_author_request(&[0x06, priv_lvl, authen_type, service, 0, 0, 0, 0]);
        assert!(author(15, 0, 0).is_some() && author(0, 6, 9).is_some());
        assert!(author(16, 1, 1).is_none() && author(1, 7, 1).is_none() && author(1, 1, 10).is_none());
        let acct = |flags, priv_lvl| parse_acct_request(&[flags, 0x06, priv_lvl, 1, 1, 0, 0, 0, 0]);
        for flags in [0x02, 0x04, 0x08, 0x03] {
            assert!(acct(flags, 1).is_some(), "{flags}");
        }
        for flags in [0x00, 0x01, 0x10, 0x82] {
            assert!(acct(flags, 1).is_none(), "{flags}");
        }
        assert!(acct(0x02, 16).is_none(), "its other fields too");
    }

    #[tokio::test]
    async fn test_wrong_secret_is_counted() -> io::Result<()> {
        // Lengths that add up, and fields that do not make sense: each gets its ERROR, and
        // then the connection closes (RFC 8907 section 10.5.2).
        let metrics = Arc::new(Metrics::default());
        let cases = [
            (TYPE_AUTHEN, vec![0xee, 0x01, 0x02, 0x01, 0, 0, 0, 0], AUTHEN_STATUS_ERROR),
            (TYPE_AUTHOR, vec![0x06, 0xee, 0x01, 0x01, 0, 0, 0, 0], AUTHOR_STATUS_ERROR),
            (TYPE_ACCT, vec![0x00, 0x06, 0x01, 0x01, 0x01, 0, 0, 0, 0], ACCT_STATUS_ERROR),
        ];
        for (type_, body, status) in cases {
            let engine = test_engine().await;
            let (mut client, server) = duplex(4096);
            let counting = metrics.clone();
            let server_task = tokio::spawn(async move {
                let live = Live::new(state_with_secret(Some(SECRET), &engine));
                TacacsServer::handle_connection(server, "127.0.0.1".parse().map_err(io::Error::other)?, &live, &counting, MAX_BODY_LEN, Timeouts::default()).await
            });
            client.write_all(&encrypted_request(type_, 1, 0, &body)).await?;
            let reply = read_packet(&mut client, MAX_BODY_LEN).await?.ok_or_else(|| io::Error::other("no reply"))?;
            let reply = TacacsPacket::encrypt_decrypt(&reply.body, SECRET, reply.session_id, reply.version, reply.seq_num);
            let at = if type_ == TYPE_ACCT { 4 } else { 0 };
            assert_eq!(reply.get(at).copied(), Some(status), "type {type_}");
            // The server ends the connection on its own, without waiting for more.
            tokio::time::timeout(Duration::from_secs(5), server_task).await.map_err(io::Error::other)?.map_err(io::Error::other)??;
            assert!(read_packet(&mut client, MAX_BODY_LEN).await?.is_none(), "type {type_}: still open");
        }
        assert_eq!(metrics.tacacs_malformed.get(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn test_refused_start_ends_an_unfinished_login() {
        let mut connection = AsciiConnection::open().await;
        let (_, status, ..) = connection.ask(ASCII_SESSION, 1, &ascii_start_body(b"alice")).await;
        assert_eq!(status, AUTHEN_STATUS_GETPASS);
        // The same session starts over as an enable request, which is refused...
        let mut enable = ascii_start_body(b"alice");
        enable[3] = AUTHEN_SVC_ENABLE;
        let (_, status, ..) = connection.ask(ASCII_SESSION, 1, &enable).await;
        assert_eq!(status, AUTHEN_STATUS_FAIL);
        // ...and the login it replaced can no longer be finished.
        let (_, status, ..) = connection.ask(ASCII_SESSION, 3, &continue_body(b"secret", 0)).await;
        assert_eq!(status, AUTHEN_STATUS_ERROR);
    }

    #[tokio::test]
    async fn test_undecodable_continue_closes_the_connection() -> io::Result<()> {
        let mut connection = AsciiConnection::open().await;
        let (seq, status, ..) = connection.ask(ASCII_SESSION, 1, &ascii_start_body(b"alice")).await;
        assert_eq!((seq, status), (2, AUTHEN_STATUS_GETPASS));
        // A CONTINUE whose lengths do not add up.
        let (_, status, ..) = connection.ask(ASCII_SESSION, 3, &[0, 9, 0, 0, 0, b'x']).await;
        assert_eq!(status, AUTHEN_STATUS_ERROR);
        assert!(read_packet(&mut connection.stream, MAX_BODY_LEN).await?.is_none(), "still open");
        Ok(())
    }

    #[tokio::test]
    async fn test_unexpected_sequence_number_keeps_the_connection() {
        let mut connection = AsciiConnection::open().await;
        // A CONTINUE for a login that never started: an ERROR, and the connection stays.
        let (_, status, ..) = connection.ask(ASCII_SESSION, 3, &continue_body(b"secret", 0)).await;
        assert_eq!(status, AUTHEN_STATUS_ERROR);
        assert_eq!(connection.login(b"alice", &[b"secret"]).await, AUTHEN_STATUS_PASS);
    }

    #[tokio::test]
    async fn test_authentication_start_reply() {
        // START: action LOGIN, priv_lvl 1, authen_type PAP, service LOGIN, empty fields.
        let reply = exchange(encrypted_request(TYPE_AUTHEN, 1, 0, &[0x01, 0x01, 0x02, 0x01, 0, 0, 0, 0])).await;
        assert_eq!(reply.type_, TYPE_AUTHEN);
        assert_eq!(reply.seq_num, 2);
        assert_eq!(reply.session_id, 0x1234_5678);
        assert_eq!(reply.body, vec![AUTHEN_STATUS_PASS, 0, 0, 0, 0, 0]);
    }

    #[tokio::test]
    async fn test_authorization_reply() {
        let reply = exchange(encrypted_request(TYPE_AUTHOR, 1, 0, &[0x06, 0x01, 0x01, 0x01, 0, 0, 0, 0])).await;
        assert_eq!(reply.seq_num, 2);
        assert_eq!(reply.body[0], AUTHOR_STATUS_PASS_ADD);
    }

    #[tokio::test]
    async fn test_accounting_reply() {
        let reply = exchange(encrypted_request(TYPE_ACCT, 1, 0, &[0x02, 0x06, 0x01, 0x01, 0x01, 0, 0, 0, 0])).await;
        assert_eq!(reply.seq_num, 2);
        assert_eq!(reply.body, vec![0, 0, 0, 0, ACCT_STATUS_SUCCESS]);
    }

    const POLICY: &str = "
        if phase == 'authentication' then
            if get_attr('user') == 'alice' and get_attr('data') == 'secret' and get_attr('authen_type') == 2 then return 'accept' end
            return 'reject'
        end
        if phase == 'authorization' then
            if get_attr('user') == 'alice' and get_attr('service') == 'shell' then
                set_attr('priv-lvl', '15')
                return 'accept'
            end
            return 'reject'
        end
        return 'accept'";

    /// Authentication START: action LOGIN, priv_lvl 1, authen_type PAP, service LOGIN.
    fn start_body(user: &[u8], data: &[u8]) -> Vec<u8> {
        let port = b"tty0";
        let mut body = vec![0x01, 0x01, 0x02, 0x01, user.len() as u8, port.len() as u8, 0, data.len() as u8];
        body.extend_from_slice(user);
        body.extend_from_slice(port);
        body.extend_from_slice(data);
        body
    }

    /// Sends one PAP authentication START for `user` from `client` and returns
    /// the reply status.
    pub(crate) async fn authen_status(client: Client, chains: Arc<AuthChains>, engine: Arc<LuaAuthEngine>, user: &[u8], password: &[u8]) -> u8 {
        let (mut stream, server) = duplex(4096);
        let task = tokio::spawn(async move { serve_as(server, &client, &engine, Some(&chains), MAX_BODY_LEN).await });
        stream.write_all(&encrypted_request(TYPE_AUTHEN, 1, 0, &start_body(user, password))).await.unwrap();
        let mut reply = read_packet(&mut stream, MAX_BODY_LEN).await.unwrap().unwrap();
        drop(stream);
        task.await.unwrap().unwrap();
        reply.body = TacacsPacket::encrypt_decrypt(&reply.body, SECRET, reply.session_id, reply.version, reply.seq_num);
        reply.body[0]
    }

    /// A chain that could not reach the database answers ERROR, so the device asks its next
    /// server; one that says no answers FAIL.
    #[tokio::test]
    async fn test_unavailable_database_answers_error() -> Result<(), String> {
        use crate::auth_chain::Handler;
        let slow = "local ok = pcall(sql_query, [[WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 5000000) SELECT max(x) FROM c]]) return 'reject'";
        let engine = |script: &str| LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), script).map(|engine| engine.with_query_timeout(Duration::from_millis(5)));
        let chain = |script: &str| -> Result<Arc<AuthChains>, String> { Ok(Arc::new(AuthChains::new(Some(AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![Handler::Lua(engine(script)?)])), []))) };
        let top = Arc::new(engine("return 'reject'")?);
        assert_eq!(authen_status(test_client(), chain(slow)?, top.clone(), b"alice", b"pw").await, AUTHEN_STATUS_ERROR);
        assert_eq!(authen_status(test_client(), chain("return 'reject'")?, top, b"alice", b"pw").await, AUTHEN_STATUS_FAIL);
        Ok(())
    }

    #[tokio::test]
    async fn test_chain_per_client() {
        use crate::auth_chain::Handler;
        let db = crate::test_support::test_db();
        let lua = |script: &str| Handler::Lua(LuaAuthEngine::new(crate::test_support::test_vms(), db.clone(), script).unwrap());
        let chains = Arc::new(AuthChains::new(
            Some(AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![lua("return 'accept'")])),
            [("vpn".to_string(), AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![lua("return 'reject'")]))],
        ));
        let engine = script_engine("return 'reject'").await;
        for (auth, status) in [(None, AUTHEN_STATUS_PASS), (Some("vpn"), AUTHEN_STATUS_FAIL)] {
            let client = Client { auth: auth.map(str::to_string), ..test_client() };
            assert_eq!(authen_status(client, chains.clone(), engine.clone(), b"alice", b"secret").await, status, "auth {:?}", auth);
        }
    }

    /// Authorization REQUEST with authen_method TACACSPLUS, priv_lvl 1, PAP, service LOGIN.
    fn author_body(user: &[u8], args: &[&[u8]]) -> Vec<u8> {
        let mut body = vec![0x06, 0x01, 0x02, 0x01, user.len() as u8, 0, 0, args.len() as u8];
        body.extend(args.iter().map(|a| a.len() as u8));
        body.extend_from_slice(user);
        for arg in args {
            body.extend_from_slice(arg);
        }
        body
    }

    #[tokio::test]
    async fn test_authentication_uses_start_fields() {
        let engine = script_engine(POLICY).await;
        let pass = exchange_with(engine.clone(), encrypted_request(TYPE_AUTHEN, 1, 0, &start_body(b"alice", b"secret"))).await;
        assert_eq!(pass.body[0], AUTHEN_STATUS_PASS);
        let fail = exchange_with(engine, encrypted_request(TYPE_AUTHEN, 1, 0, &start_body(b"alice", b"wrong"))).await;
        assert_eq!(fail.seq_num, 2);
        assert_eq!(fail.body[0], AUTHEN_STATUS_FAIL);
    }

    #[tokio::test]
    async fn test_challenge_is_a_failure_over_tacacs() {
        let engine = script_engine("return 'challenge'").await;
        let reply = exchange_with(engine, encrypted_request(TYPE_AUTHEN, 1, 0, &start_body(b"alice", b"x"))).await;
        assert_eq!(reply.body[0], AUTHEN_STATUS_FAIL);
    }

    #[tokio::test]
    async fn test_get_attrs_reads_repeated_av_pairs() {
        let script = "local args = get_attrs('cmd-arg') return (#args == 2 and args[1] == 'a' and args[2] == 'b') and 'accept' or 'reject'";
        let engine = script_engine(script).await;
        let reply = exchange_with(engine, encrypted_request(TYPE_AUTHOR, 1, 0, &author_body(b"alice", &[b"cmd-arg=a", b"cmd-arg=b"]))).await;
        assert_eq!(reply.body[0], AUTHOR_STATUS_PASS_ADD);
    }

    #[tokio::test]
    async fn test_authorization_with_more_av_pairs_than_fit() {
        let script = |count: usize| format!("for i = 1, {count} do set_attr('a' .. i, 'x') end return 'accept'");
        let request = || encrypted_request(TYPE_AUTHOR, 1, 0, &author_body(b"alice", &[]));
        let reply = exchange_with(script_engine(&script(256)).await, request()).await;
        assert_eq!(reply.body, vec![AUTHOR_STATUS_ERROR, 0, 0, 0, 0, 0]);
        let logged = crate::test_support::logged_errors().lock().unwrap().clone();
        assert!(logged.iter().any(|line| line.contains(&test_client().name) && line.contains("256 AV pairs")), "{logged:?}");
        let reply = exchange_with(script_engine(&script(255)).await, request()).await;
        assert_eq!((reply.body[0], reply.body[1]), (AUTHOR_STATUS_PASS_ADD, 255));
        let (lengths, mut rest) = reply.body[6..].split_at(255);
        for (index, length) in lengths.iter().enumerate() {
            let (pair, after) = rest.split_at(usize::from(*length));
            assert_eq!(pair, format!("a{}=x", index + 1).as_bytes());
            rest = after;
        }
        assert!(rest.is_empty());
    }

    #[tokio::test]
    async fn test_script_sees_the_port_and_address_a_device_sent() {
        let script = "return (get_attr('port') == 'tty7' and get_attr('rem_addr') == '10.1.2.3') and 'accept' or 'reject'";
        let (user, port, address, data) = (&b"alice"[..], &b"tty7"[..], &b"10.1.2.3"[..], &b"secret"[..]);
        let mut start = vec![0x01, 0x01, 0x02, 0x01, user.len() as u8, port.len() as u8, address.len() as u8, data.len() as u8];
        start.extend_from_slice(&[user, port, address, data].concat());
        let reply = exchange_with(script_engine(script).await, encrypted_request(TYPE_AUTHEN, 1, 0, &start)).await;
        assert_eq!(reply.body[0], AUTHEN_STATUS_PASS, "authentication");
        let mut request = vec![0x06, 0x01, 0x02, 0x01, user.len() as u8, port.len() as u8, address.len() as u8, 0];
        request.extend_from_slice(&[user, port, address].concat());
        let reply = exchange_with(script_engine(script).await, encrypted_request(TYPE_AUTHOR, 1, 0, &request)).await;
        assert_eq!(reply.body[0], AUTHOR_STATUS_PASS_ADD, "authorization");
        let other = String::from_utf8(script.as_bytes().to_vec()).unwrap().replace("tty7", "tty8");
        let reply = exchange_with(script_engine(&other).await, encrypted_request(TYPE_AUTHEN, 1, 0, &start)).await;
        assert_ne!(reply.body[0], AUTHEN_STATUS_PASS, "the script did look");
    }

    #[tokio::test]
    async fn test_authorization_returns_av_pairs() {
        let engine = script_engine(POLICY).await;
        let pass = exchange_with(engine.clone(), encrypted_request(TYPE_AUTHOR, 1, 0, &author_body(b"alice", &[b"service=shell", b"cmd*"]))).await;
        let expected_arg = b"priv-lvl=15";
        let mut expected = vec![AUTHOR_STATUS_PASS_ADD, 1, 0, 0, 0, 0, expected_arg.len() as u8];
        expected.extend_from_slice(expected_arg);
        assert_eq!(pass.body, expected);

        let fail = exchange_with(engine, encrypted_request(TYPE_AUTHOR, 1, 0, &author_body(b"alice", &[b"service=ppp"]))).await;
        assert_eq!(fail.body, vec![AUTHOR_STATUS_FAIL, 0, 0, 0, 0, 0]);
    }

    /// A peer check that passes `passes` times, then refuses.
    struct PassesFor(parking_lot::Mutex<usize>);

    impl PeerCheck for PassesFor {
        fn recheck(&self, _network: &ipnet::IpNet, _client: &Client) -> Result<(), String> {
            let mut left = self.0.lock();
            *left = left.checked_sub(1).ok_or("revoked since")?;
            Ok(())
        }
    }

    /// RFC 9887's rules on a connection an extension made, here without TLS: a login in the
    /// clear passes and its reply has the flag; a peer the check refuses is closed without an
    /// answer; a packet without the flag gets an ERROR with it, and the connection ends.
    #[tokio::test]
    async fn test_clear_connection_rules() -> io::Result<()> {
        let engine = script_engine("return 'accept'").await;
        let live = Live::new(state_with_secret(Some(SECRET), &engine));
        let timeouts = Timeouts { recheck: Duration::ZERO, ..Timeouts::default() };
        let source: IpAddr = "127.0.0.1".parse().map_err(io::Error::other)?;

        let (mut near, far) = duplex(65536);
        near.write_all(&tls_pap(7, b"pw")).await?;
        near.write_all(&tls_pap(8, b"pw")).await?;
        let check = Box::new(PassesFor(parking_lot::Mutex::new(1)));
        let logged = crate::test_support::radius::logged_during(async {
            let served = TacacsServer::handle_clear_connection(far, source, &live, &Metrics::default(), MAX_BODY_LEN, timeouts, check).await;
            assert!(served.is_ok(), "{served:?}");
        })
        .await;
        let reply = read_packet(&mut near, MAX_BODY_LEN).await?.ok_or_else(|| io::Error::other("no reply"))?;
        assert_eq!((reply.flags & UNENCRYPTED_FLAG, reply.body.first().copied()), (UNENCRYPTED_FLAG, Some(AUTHEN_STATUS_PASS)), "in the clear, with the flag");
        assert!(read_packet(&mut near, MAX_BODY_LEN).await?.is_none(), "no answer once the check refuses");
        assert!(logged.contains("TACACS+ over TLS connection from 127.0.0.1 closed: revoked since"), "{logged}");

        let (mut near, far) = duplex(65536);
        near.write_all(&TacacsPacket { version: 0xc1, type_: TYPE_AUTHEN, seq_num: 1, flags: 0, session_id: 9, body: start_body(b"alice", b"pw") }.encode()).await?;
        let served = TacacsServer::handle_clear_connection(far, source, &live, &Metrics::default(), MAX_BODY_LEN, timeouts, Box::new(PassesFor(parking_lot::Mutex::new(9)))).await;
        assert!(served.is_err(), "an obfuscated packet ends the connection");
        let reply = read_packet(&mut near, MAX_BODY_LEN).await?.ok_or_else(|| io::Error::other("no ERROR"))?;
        assert_eq!((reply.flags & UNENCRYPTED_FLAG, reply.body.first().copied()), (UNENCRYPTED_FLAG, Some(AUTHEN_STATUS_ERROR)));
        Ok(())
    }

    /// A connection an extension accepted from a source no client covers is refused before
    /// anything is read, and counted.
    #[tokio::test]
    async fn test_clear_connection_from_unknown_source() -> io::Result<()> {
        let engine = script_engine("return 'accept'").await;
        let metrics = Arc::new(Metrics::default());
        let server = TacacsServer::new("127.0.0.1:0".parse().map_err(io::Error::other)?, Arc::new(ClientTable::default()), engine, MAX_BODY_LEN).with_metrics(metrics.clone());
        assert!(server.admit("192.0.2.1:4000".parse().map_err(io::Error::other)?).is_none());
        assert_eq!(metrics.tacacs_refused.get(), 1);
        assert_eq!(metrics.tacacs_accepted.get(), 0);
        Ok(())
    }

    fn tls_pap(session_id: u32, password: &[u8]) -> Vec<u8> {
        TacacsPacket { version: 0xc1, type_: TYPE_AUTHEN, seq_num: 1, flags: UNENCRYPTED_FLAG, session_id, body: start_body(b"alice", password) }.encode()
    }

    #[tokio::test]
    async fn test_tls_only_needs_a_tls_port_already_open() -> io::Result<()> {
        // A reload that adds [tacacs.tls] with tls_only opens no port: the plain one keeps serving.
        let engine = script_engine("return 'accept'").await;
        let server = TacacsServer::from_state(SocketAddr::from(([127, 0, 0, 1], 0)), state_with_secret(Some(SECRET), &engine), MAX_BODY_LEN);
        server.replace_state(TacacsState { tls_only: true, ..state_with_secret(Some(SECRET), &engine) });
        assert!(!server.state.get().tls_only);
        let server = server.with_tls_served(Some(TacacsTlsServed { port: 300, tls_only: false }));
        server.replace_state(TacacsState { tls_only: true, ..state_with_secret(Some(SECRET), &engine) });
        assert!(server.state.get().tls_only, "with a TLS port it applies");
        Ok(())
    }

    #[tokio::test]
    async fn test_tls_only_turned_on_closes_plain_sessions() -> io::Result<()> {
        let engine = script_engine("return 'accept'").await;
        let (mut stream, live, task) = open_connection(&engine);
        stream.write_all(&request_with_key(SECRET, &start_body(b"alice", b"secret"))).await?;
        assert!(read_packet(&mut stream, MAX_BODY_LEN).await?.is_some(), "served before");
        let old = live.get();
        live.set(TacacsState { clients: old.clients.clone(), engine: old.engine.clone(), chains: None, enable: old.enable, tls_only: true, second_factor: None });
        stream.write_all(&request_with_key(SECRET, &start_body(b"alice", b"secret"))).await?;
        assert!(!matches!(read_packet(&mut stream, MAX_BODY_LEN).await, Ok(Some(_))), "no reply once tls_only is on");
        task.await.map_err(io::Error::other)??;
        Ok(())
    }

    /// A port nothing listens on just now.
    fn unused_port() -> io::Result<u16> {
        Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
    }

    /// With an extension serving TACACS+ over TLS only, the server opens no port of its own.
    #[tokio::test]
    async fn test_tls_only_leaves_the_plain_port_closed() -> io::Result<()> {
        let plain = SocketAddr::from(([127, 0, 0, 1], unused_port()?));
        let engine = script_engine("return 'accept'").await;
        let server = TacacsServer::from_state(plain, state_with_secret(Some(SECRET), &engine), MAX_BODY_LEN).with_tls_served(Some(TacacsTlsServed { port: 300, tls_only: true }));
        tokio::time::timeout(Duration::from_secs(5), server.run()).await.map_err(io::Error::other)??;
        assert!(tokio::net::TcpStream::connect(plain).await.is_err(), "nothing listens without TLS");
        Ok(())
    }

    #[tokio::test]
    async fn test_tls_name_client_refused_without_tls() -> io::Result<()> {
        let engine = script_engine("return 'accept'").await;
        let mut clients = ClientTable::default();
        clients.add("127.0.0.1", None, SECRET.to_vec(), ClientOptions { tls_name: Some("device.example.com".to_string()), ..ClientOptions::default() }).map_err(|e| io::Error::other(e.to_string()))?;
        let live = Live::new(TacacsState { clients: Arc::new(clients), engine, chains: None, enable: EnableMode::default(), tls_only: false, second_factor: None });
        let (mut near, far) = duplex(65536);
        let task = tokio::spawn(async move { TacacsServer::handle_connection(far, "127.0.0.1".parse().map_err(io::Error::other)?, &live, &Metrics::default(), MAX_BODY_LEN, Timeouts::default()).await });
        near.write_all(&encrypted_request(TYPE_AUTHEN, 1, 0, &start_body(b"alice", b"secret"))).await?;
        let error = task.await.map_err(io::Error::other)?.err().ok_or_else(|| io::Error::other("served without TLS"))?;
        assert!(error.to_string().contains("has to use TLS"), "{error}");
        Ok(())
    }

    #[tokio::test]
    async fn test_server_msg_in_replies() -> io::Result<()> {
        let exchange = |script: &'static str, request: Vec<u8>| async move { exchange_as(test_client(), script_engine(script).await, request).await };
        // Authentication: PASS and FAIL carry the message.
        let pap = encrypted_request(TYPE_AUTHEN, 1, 0, &start_body(b"alice", b"secret"));
        let reply = exchange("set_attr('server_msg', 'Welcome') return 'accept'", pap.clone()).await?;
        assert_eq!(reply.body, [&[AUTHEN_STATUS_PASS, 0, 0, 7, 0, 0][..], b"Welcome"].concat());
        let reply = exchange("set_attr('server_msg', 'Account locked') return 'reject'", pap.clone()).await?;
        assert_eq!(reply.body, [&[AUTHEN_STATUS_FAIL, 0, 0, 14, 0, 0][..], b"Account locked"].concat());
        // Authorization: the message comes after the argument lengths and is not an argument.
        let author = encrypted_request(TYPE_AUTHOR, 1, 0, &author_body(b"alice", &[b"service=shell"]));
        let reply = exchange("set_attr('priv-lvl', 15) set_attr('server_msg', 'Hi') return 'accept'", author.clone()).await?;
        assert_eq!(reply.body, [&[AUTHOR_STATUS_PASS_ADD, 1, 0, 2, 0, 0, 11][..], b"Hi", b"priv-lvl=15"].concat());
        let reply = exchange("set_attr('server_msg', 'No') return 'reject'", author).await?;
        assert_eq!(reply.body, [&[AUTHOR_STATUS_FAIL, 0, 0, 2, 0, 0][..], b"No"].concat());
        // Accounting: server_msg_len, data_len, status, then the message.
        let acct = encrypted_request(TYPE_ACCT, 1, 0, &acct_body(b"alice", &[b"task_id=1"]));
        let reply = exchange("set_attr('server_msg', 'Logged') return 'accept'", acct.clone()).await?;
        assert_eq!(reply.body, [&[0, 6, 0, 0, ACCT_STATUS_SUCCESS][..], b"Logged"].concat());
        // Too long: left out, and the decision stands.
        let reply = exchange("set_attr('server_msg', string.rep('x', 1025)) return 'accept'", pap).await?;
        assert_eq!(reply.body, vec![AUTHEN_STATUS_PASS, 0, 0, 0, 0, 0]);
        let reply = exchange("return 'accept'", acct).await?;
        assert_eq!(reply.body, vec![0, 0, 0, 0, ACCT_STATUS_SUCCESS]);
        Ok(())
    }

    /// One exchange as `client`, which may carry an authorization group.
    async fn exchange_as(client: Client, engine: Arc<LuaAuthEngine>, request: Vec<u8>) -> io::Result<TacacsPacket> {
        let (mut stream, server) = duplex(4096);
        let server_task = tokio::spawn(async move { serve_as(server, &client, &engine, None, MAX_BODY_LEN).await });
        stream.write_all(&request).await?;
        let mut reply = read_packet(&mut stream, MAX_BODY_LEN).await?.ok_or_else(|| io::Error::other("no reply"))?;
        drop(stream);
        server_task.await.map_err(io::Error::other)??;
        reply.body = TacacsPacket::encrypt_decrypt(&reply.body, SECRET, reply.session_id, reply.version, reply.seq_num);
        Ok(reply)
    }

    fn grouped_client(rules: &[&str]) -> Result<Client, String> {
        let group = crate::tacacs_rules::AuthorizationRules::parse(&rules.iter().map(|rule| rule.to_string()).collect::<Vec<_>>())?;
        Ok(Client { authorize_group: Some(Arc::new(group)), ..test_client() })
    }

    #[tokio::test]
    async fn test_authorization_by_group() -> Result<(), Box<dyn std::error::Error>> {
        let rules = ["deny cmd=show cmd-arg=running-config", r"permit service=shell cmd\* {priv-lvl=15}", "permit service=shell cmd=show"];
        // The script would refuse everything: the group decides without it.
        let engine = script_engine("return 'reject'").await;
        let ask = |args: &'static [&'static [u8]]| {
            let engine = engine.clone();
            async move { exchange_as(grouped_client(&rules)?, engine, encrypted_request(TYPE_AUTHOR, 1, 0, &author_body(b"alice", args))).await.map_err(|e| e.to_string()) }
        };
        let mut shell = vec![AUTHOR_STATUS_PASS_ADD, 1, 0, 0, 0, 0, 11];
        shell.extend_from_slice(b"priv-lvl=15");
        assert_eq!(ask(&[b"service=shell", b"cmd*"]).await?.body, shell);
        assert_eq!(ask(&[b"service=shell", b"cmd=show", b"cmd-arg=version"]).await?.body, vec![AUTHOR_STATUS_PASS_ADD, 0, 0, 0, 0, 0]);
        assert_eq!(ask(&[b"service=shell", b"cmd=show", b"cmd-arg=running-config"]).await?.body, vec![AUTHOR_STATUS_FAIL, 0, 0, 0, 0, 0]);
        assert_eq!(ask(&[b"service=shell", b"cmd=configure"]).await?.body, vec![AUTHOR_STATUS_FAIL, 0, 0, 0, 0, 0]);

        // A body that does not add up is still an error.
        let mut author = author_body(b"alice", &[b"service=shell", b"cmd*"]);
        author.push(b'x');
        let reply = exchange_as(grouped_client(&rules)?, engine.clone(), encrypted_request(TYPE_AUTHOR, 1, 0, &author)).await?;
        assert_eq!(reply.body[0], AUTHOR_STATUS_ERROR);

        // Without a group the script decides, and authentication is not touched by the group.
        let reply = exchange_as(test_client(), engine.clone(), encrypted_request(TYPE_AUTHOR, 1, 0, &author_body(b"alice", &[b"service=shell", b"cmd*"]))).await?;
        assert_eq!(reply.body[0], AUTHOR_STATUS_FAIL);
        let accepting = script_engine("return 'accept'").await;
        let reply = exchange_as(test_client(), accepting.clone(), encrypted_request(TYPE_AUTHOR, 1, 0, &author_body(b"alice", &[b"service=shell"]))).await?;
        assert_eq!(reply.body[0], AUTHOR_STATUS_PASS_ADD);
        let reply = exchange_as(grouped_client(&rules)?, engine, encrypted_request(TYPE_AUTHEN, 1, 0, &start_body(b"alice", b"secret"))).await?;
        assert_eq!(reply.body[0], AUTHEN_STATUS_FAIL, "the script still decides authentication");
        Ok(())
    }

    #[tokio::test]
    async fn test_scripts_see_the_header_fields() -> io::Result<()> {
        let fields = "get_attr('priv_lvl') == 15 and get_attr('authen_method') == 6 and get_attr('authen_type') == 2 and get_attr('authen_service') == 1";
        let engine = script_engine(&format!("return ({fields}) and 'accept' or 'reject'")).await;
        // author_body sends authen_method 6, priv_lvl 1, authen_type 2, authen_service 1.
        let mut author = author_body(b"alice", &[b"service=shell"]);
        author[1] = 15;
        let reply = exchange_as(test_client(), engine.clone(), encrypted_request(TYPE_AUTHOR, 1, 0, &author)).await?;
        assert_eq!(reply.body.first().copied(), Some(AUTHOR_STATUS_PASS_ADD));
        author[1] = 14;
        let reply = exchange_as(test_client(), engine.clone(), encrypted_request(TYPE_AUTHOR, 1, 0, &author)).await?;
        assert_eq!(reply.body.first().copied(), Some(AUTHOR_STATUS_FAIL), "the script did look");
        // Accounting: a STOP with the same header.
        let mut acct = vec![0x04];
        acct.extend(author_body(b"alice", &[b"task_id=1"]));
        acct[2] = 15;
        let reply = exchange_as(test_client(), engine, encrypted_request(TYPE_ACCT, 1, 0, &acct)).await?;
        assert_eq!(reply.body.get(4).copied(), Some(ACCT_STATUS_SUCCESS));
        Ok(())
    }

    /// Writes one packet and reads the reply header and decrypted body.
    async fn ask_on(stream: &mut tokio::io::DuplexStream, packet: TacacsPacket) -> io::Result<TacacsPacket> {
        stream.write_all(&packet.encode()).await?;
        let mut reply = read_packet(stream, MAX_BODY_LEN).await?.ok_or_else(|| io::Error::other("no reply"))?;
        reply.body = TacacsPacket::encrypt_decrypt(&reply.body, SECRET, reply.session_id, reply.version, reply.seq_num);
        Ok(reply)
    }

    #[tokio::test]
    async fn test_single_connection_flag() -> io::Result<()> {
        let request = |type_: u8, session_id: u32, flags: u8, body: &[u8]| TacacsPacket {
            version: VERSION,
            type_,
            seq_num: 1,
            flags,
            session_id,
            body: TacacsPacket::encrypt_decrypt(body, SECRET, session_id, VERSION, 1),
        };
        for first_flags in [SINGLE_CONNECT_FLAG, 0] {
            let engine = test_engine().await;
            let (mut stream, server) = duplex(4096);
            tokio::spawn(async move { serve_as(server, &test_client(), &engine, None, MAX_BODY_LEN).await });
            let first = ask_on(&mut stream, request(TYPE_AUTHEN, 1, first_flags, &start_body(b"alice", b"secret"))).await?;
            assert_eq!(first.flags, first_flags, "the first reply agrees only when asked");
            // Another session on the same connection; its flag no longer matters.
            let second = ask_on(&mut stream, request(TYPE_AUTHOR, 2, SINGLE_CONNECT_FLAG, &author_body(b"alice", &[b"service=shell"]))).await?;
            assert_eq!((second.flags, second.session_id, second.body.first().copied()), (0, 2, Some(AUTHOR_STATUS_PASS_ADD)));
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_malformed_bodies_get_error_status() {
        let mut start = start_body(b"alice", b"secret");
        start[4] = 20; // user_len larger than the body
        let reply = exchange(encrypted_request(TYPE_AUTHEN, 1, 0, &start)).await;
        assert_eq!(reply.body[0], AUTHEN_STATUS_ERROR);

        let mut author = author_body(b"alice", &[b"service=shell"]);
        author.push(b'x'); // trailing octet not covered by any length
        let reply = exchange(encrypted_request(TYPE_AUTHOR, 1, 0, &author)).await;
        assert_eq!(reply.body[0], AUTHOR_STATUS_ERROR);

        let reply = exchange(encrypted_request(TYPE_ACCT, 1, 0, &[0x02, 0x06])).await;
        assert_eq!(reply.body, vec![0, 0, 0, 0, ACCT_STATUS_ERROR]);
    }

    #[test]
    fn test_parse_bodies() {
        let attrs = parse_authen_start(&start_body(b"alice", b"secret")).unwrap();
        assert!(attrs.contains(&("port".to_string(), AttrValue::Bytes(b"tty0".to_vec()))));
        assert!(attrs.contains(&("data".to_string(), AttrValue::Bytes(b"secret".to_vec()))));
        let attrs = parse_author_request(&author_body(b"bob", &[b"service=shell", b"cmd*show"])).unwrap();
        assert!(attrs.contains(&("user".to_string(), AttrValue::Bytes(b"bob".to_vec()))));
        assert!(attrs.contains(&("cmd".to_string(), AttrValue::Bytes(b"show".to_vec()))));
        assert!(parse_author_request(&[0x06, 0x01]).is_none());
    }

    /// Accounting REQUEST: flags START, then the same layout as an Authorization REQUEST.
    fn acct_body(user: &[u8], args: &[&[u8]]) -> Vec<u8> {
        let mut body = vec![0x02];
        body.extend(author_body(user, args));
        body
    }

    async fn accounting_engine(script: &str) -> (Arc<LuaAuthEngine>, crate::lua_engine::Db) {
        let db = crate::test_support::test_db();
        db.execute("CREATE TABLE IF NOT EXISTS tacacs_acct_sessions (id TEXT)").await.unwrap();
        (Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), db.clone(), script).unwrap()), db)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_accounting_requests_share_the_script_limit() {
        let vm = crate::lua_engine::tests::GatedVm::new();
        let holder = vm.hold_place().await;
        let engine = vm.engine("return 'accept'");
        let request = encrypted_request(TYPE_ACCT, 1, 0, &acct_body(b"alice", &[b"task_id=77"]));
        let reply = tokio::spawn(async move { exchange_with(engine, request).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!reply.is_finished(), "TACACS+ accounting ran past the limit");
        vm.open_gate();
        assert_eq!(reply.await.unwrap().body, vec![0, 0, 0, 0, ACCT_STATUS_SUCCESS]);
        holder.await.unwrap();
    }

    #[tokio::test]
    async fn test_accounting_script_records_task_id() {
        let (engine, db) = accounting_engine("
            if phase == 'accounting' then
                sql_execute(\"INSERT INTO tacacs_acct_sessions VALUES ('\" .. get_attr('task_id') .. \"')\")
            end
            return 'accept'").await;
        let reply = exchange_with(engine, encrypted_request(TYPE_ACCT, 1, 0, &acct_body(b"alice", &[b"task_id=77", b"start_time=1700000000"]))).await;
        assert_eq!(reply.body, vec![0, 0, 0, 0, ACCT_STATUS_SUCCESS]);
        let id = db.query_text("SELECT id FROM tacacs_acct_sessions WHERE id = '77'").await;
        assert_eq!(id, "77");
    }

    #[tokio::test]
    async fn test_accounting_status_follows_the_script() {
        for (script, status) in [
            ("error('boom')", ACCT_STATUS_ERROR),
            ("return 'reject'", ACCT_STATUS_ERROR),
            ("local x = 1", ACCT_STATUS_ERROR),
            ("return 'accept'", ACCT_STATUS_SUCCESS),
        ] {
            let (engine, _db) = accounting_engine(script).await;
            let reply = exchange_with(engine, encrypted_request(TYPE_ACCT, 1, 0, &acct_body(b"alice", &[b"task_id=77"]))).await;
            assert_eq!(reply.body, vec![0, 0, 0, 0, status], "script: {script}");
        }
    }

    #[tokio::test]
    async fn test_accounting_error_keeps_the_connection_open() {
        let (engine, _db) = accounting_engine("return get_attr('task_id') == 'good' and 'accept' or 'reject'").await;
        let (mut stream, server) = duplex(4096);
        let task = tokio::spawn(async move { serve_as(server, &test_client(), &engine, None, MAX_BODY_LEN).await });
        for (task_id, status) in [(&b"task_id=bad"[..], ACCT_STATUS_ERROR), (&b"task_id=good"[..], ACCT_STATUS_SUCCESS)] {
            stream.write_all(&encrypted_request(TYPE_ACCT, 1, 0, &acct_body(b"alice", &[task_id]))).await.unwrap();
            let mut reply = read_packet(&mut stream, MAX_BODY_LEN).await.unwrap().unwrap();
            reply.body = TacacsPacket::encrypt_decrypt(&reply.body, SECRET, reply.session_id, reply.version, reply.seq_num);
            assert_eq!(reply.body, vec![0, 0, 0, 0, status]);
        }
        drop(stream);
        task.await.unwrap().unwrap();
    }

    fn request_with_key(key: &[u8], body: &[u8]) -> Vec<u8> {
        TacacsPacket {
            version: VERSION,
            type_: TYPE_AUTHEN,
            seq_num: 1,
            flags: 0,
            session_id: 7,
            body: TacacsPacket::encrypt_decrypt(body, key, 7, VERSION, 1),
        }
        .encode()
    }

    #[tokio::test]
    async fn test_per_client_key() {
        let engine = test_engine().await;
        for (name, key) in [("router-5", &b"k1"[..]), ("router-6", &b"k2"[..])] {
            let client = Client { max_radsec_connections: None, radsec_subject: None, authorize_group: None, coa_sender: false, mfa: None, eap_methods: None, name: name.to_string(), secret: key.to_vec(), auth: None, require_message_authenticator: Default::default(), limit_proxy_state: Default::default(), coa_port: 3799, tls_name: None };
            let (mut stream, server) = duplex(4096);
            let engine = engine.clone();
            let task = tokio::spawn(async move { serve_as(server, &client, &engine, None, MAX_BODY_LEN).await });
            stream.write_all(&request_with_key(key, &start_body(b"alice", b"secret"))).await.unwrap();
            let reply = read_packet(&mut stream, MAX_BODY_LEN).await.unwrap().unwrap();
            drop(stream);
            task.await.unwrap().unwrap();
            let plain = TacacsPacket::encrypt_decrypt(&reply.body, key, reply.session_id, reply.version, reply.seq_num);
            assert_eq!(plain[0], AUTHEN_STATUS_PASS, "reply for {} not encrypted with its key", name);
        }
    }

    #[tokio::test]
    async fn test_body_over_configured_limit_closes() {
        let engine = test_engine().await;
        let (mut stream, server) = duplex(4096);
        let task = tokio::spawn(async move { serve_as(server, &test_client(), &engine, None, 1024).await });
        let mut header = encrypted_request(TYPE_AUTHEN, 1, 0, &[])[..HEADER_LEN].to_vec();
        header[8..12].copy_from_slice(&1025u32.to_be_bytes());
        stream.write_all(&header).await.unwrap();
        let mut buf = Vec::new();
        assert_eq!(stream.read_to_end(&mut buf).await.unwrap(), 0);
        assert!(task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn test_connection_limit() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let clients = ClientTable::new(vec![("127.0.0.1", None, SECRET.to_vec(), None)]).unwrap();
        let server = TacacsServer::new(port, Arc::new(clients), test_engine().await, MAX_BODY_LEN).with_max_connections(1);
        tokio::spawn(async move { server.run().await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        async fn pass_status(stream: &mut tokio::net::TcpStream) -> u8 {
            stream.write_all(&encrypted_request(TYPE_AUTHEN, 1, 0, &start_body(b"alice", b"secret"))).await.unwrap();
            let mut reply = read_packet(stream, MAX_BODY_LEN).await.unwrap().unwrap();
            reply.body = TacacsPacket::encrypt_decrypt(&reply.body, SECRET, reply.session_id, reply.version, reply.seq_num);
            reply.body[0]
        }
        let wait = std::time::Duration::from_secs(1);

        let mut first = tokio::net::TcpStream::connect(port).await.unwrap();
        assert_eq!(pass_status(&mut first).await, AUTHEN_STATUS_PASS);

        // The accept loop closes the second connection without reading from it.
        let mut second = tokio::net::TcpStream::connect(port).await.unwrap();
        let mut buf = Vec::new();
        let read = tokio::time::timeout(wait, second.read_to_end(&mut buf)).await.expect("the connection should be closed");
        assert_eq!(read.unwrap_or(0), 0);
        assert_eq!(pass_status(&mut first).await, AUTHEN_STATUS_PASS, "the first connection keeps working");

        drop(first);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut third = tokio::net::TcpStream::connect(port).await.unwrap();
        assert_eq!(tokio::time::timeout(wait, pass_status(&mut third)).await.expect("served after capacity returns"), AUTHEN_STATUS_PASS);
    }

    #[tokio::test]
    async fn test_lines_for_unknown_clients_are_limited() {
        let clients = ClientTable::new(vec![("192.0.2.5", None, b"k".to_vec(), None)]).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let metrics = Arc::new(Metrics::default());
        let server = TacacsServer::new(addr, Arc::new(clients), test_engine().await, MAX_BODY_LEN).with_metrics(metrics.clone());
        tokio::spawn(async move { server.run().await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let log = crate::radius::tests::logged_during(async {
            for _ in 0..20 {
                let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
                assert_eq!(socket.read(&mut [0u8; 16]).await.unwrap_or(0), 0);
            }
        })
        .await;
        assert_eq!(metrics.tacacs_refused.get(), 20);
        let lines = log.matches("unknown client 127.0.0.1").count();
        assert!((1..=2).contains(&lines), "{log}");
    }

    fn timeouts(idle_ms: u64, write_ms: u64) -> Timeouts {
        Timeouts { idle: std::time::Duration::from_millis(idle_ms), write: std::time::Duration::from_millis(write_ms), ..Timeouts::default() }
    }

    #[tokio::test]
    async fn test_connection_that_sends_nothing_is_closed() {
        let engine = script_engine("return 'accept'").await;
        let live = Live::new(state_with_secret(Some(SECRET), &engine));
        let (mut client, server) = tokio::io::duplex(4096);
        let metrics = Metrics::default();
        let serving = TacacsServer::handle_connection(server, "127.0.0.1".parse().unwrap(), &live, &metrics, MAX_BODY_LEN, timeouts(100, 10_000));
        tokio::time::timeout(std::time::Duration::from_secs(5), serving).await.expect("still open").unwrap();
        assert_eq!(client.read(&mut [0u8; 16]).await.unwrap(), 0);
        // Half a header is no packet.
        let (mut client, server) = tokio::io::duplex(4096);
        client.write_all(&[0xc0, 1, 1, 0, 0]).await.unwrap();
        let metrics = Metrics::default();
        let serving = TacacsServer::handle_connection(server, "127.0.0.1".parse().unwrap(), &live, &metrics, MAX_BODY_LEN, timeouts(100, 10_000));
        tokio::time::timeout(std::time::Duration::from_secs(5), serving).await.expect("still open").unwrap();
    }

    #[tokio::test]
    async fn test_connection_with_packets_arriving_stays_open() {
        let engine = script_engine("return 'accept'").await;
        let live = Live::new(state_with_secret(Some(SECRET), &engine));
        let (mut client, server) = tokio::io::duplex(4096);
        let serving = tokio::spawn(async move { TacacsServer::handle_connection(server, "127.0.0.1".parse().unwrap(), &live, &Metrics::default(), MAX_BODY_LEN, timeouts(200, 10_000)).await });
        for session in 1..=4u32 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            client.write_all(&encrypted_request(TYPE_AUTHEN, 1, 0, &start_body(b"alice", b"secret"))).await.unwrap();
            assert!(tokio::time::timeout(std::time::Duration::from_secs(5), read_packet(&mut client, MAX_BODY_LEN)).await.unwrap().unwrap().is_some(), "session {session}");
        }
        drop(client);
        serving.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_time_in_a_script_is_not_idle() {
        let engine = script_engine("sql_query('WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 3000000) SELECT count(*) AS n FROM c') return 'accept'").await;
        let live = Live::new(state_with_secret(Some(SECRET), &engine));
        let (mut client, server) = tokio::io::duplex(4096);
        let serving = tokio::spawn(async move { TacacsServer::handle_connection(server, "127.0.0.1".parse().unwrap(), &live, &Metrics::default(), MAX_BODY_LEN, timeouts(50, 10_000)).await });
        client.write_all(&encrypted_request(TYPE_AUTHEN, 1, 0, &start_body(b"alice", b"secret"))).await.unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(30), read_packet(&mut client, MAX_BODY_LEN)).await.unwrap().unwrap();
        assert!(reply.is_some(), "closed while the script ran");
        drop(client);
        serving.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_client_that_does_not_read_is_closed() {
        let engine = script_engine("return 'accept'").await;
        let live = Live::new(state_with_secret(Some(SECRET), &engine));
        // Too little room for a reply.
        let (mut client, server) = tokio::io::duplex(8);
        let serving = tokio::spawn(async move { TacacsServer::handle_connection(server, "127.0.0.1".parse().unwrap(), &live, &Metrics::default(), MAX_BODY_LEN, timeouts(60_000, 100)).await });
        let request = encrypted_request(TYPE_AUTHEN, 1, 0, &start_body(b"alice", b"secret"));
        client.write_all(&request).await.unwrap();
        let ended = tokio::time::timeout(std::time::Duration::from_secs(5), serving).await.expect("still open").unwrap();
        assert!(ended.is_err(), "{ended:?}");
    }

    #[tokio::test]
    async fn test_idle_connection_gives_its_place_back() {
        let engine = script_engine("return 'accept'").await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let metrics = Arc::new(Metrics::default());
        let server = Arc::new(TacacsServer::from_state(addr, state_with_secret(Some(SECRET), &engine), MAX_BODY_LEN).with_max_connections(1).with_idle_timeout(std::time::Duration::from_millis(100)).with_metrics(metrics.clone()));
        tokio::spawn(async move { server.run().await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut first = tokio::net::TcpStream::connect(addr).await.unwrap();
        assert_eq!(tokio::time::timeout(std::time::Duration::from_secs(5), first.read(&mut [0u8; 16])).await.expect("still open").unwrap(), 0);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(metrics.tacacs_open.get(), 0);
        let mut second = tokio::net::TcpStream::connect(addr).await.unwrap();
        second.write_all(&encrypted_request(TYPE_AUTHEN, 1, 0, &start_body(b"alice", b"secret"))).await.unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), read_packet(&mut second, MAX_BODY_LEN)).await.expect("no reply").unwrap();
        assert!(reply.is_some(), "its place was not free again");
        assert_eq!(metrics.tacacs_refused.get(), 0);
    }

    #[tokio::test]
    async fn test_unknown_client_connection_closed() {
        let clients = ClientTable::new(vec![("10.0.0.5", None, b"k".to_vec(), None)]).unwrap();
        let server = TacacsServer::new(([127, 0, 0, 1], 14949).into(), Arc::new(clients), test_engine().await, MAX_BODY_LEN);
        tokio::spawn(async move { server.run().await });

        let mut stream = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                match tokio::net::TcpStream::connect("127.0.0.1:14949").await {
                    Ok(stream) => return stream,
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("server not listening");
        let mut buf = Vec::new();
        let read = tokio::time::timeout(std::time::Duration::from_secs(1), stream.read_to_end(&mut buf))
            .await
            .expect("connection from unknown client was not closed");
        assert_eq!(read.unwrap_or(0), 0);
    }

    #[tokio::test]
    async fn test_run_selects_key_by_source() {
        // 127.0.0.1 matches both; the /32 entry is the longer prefix and must win.
        let clients = ClientTable::new(vec![
            ("127.0.0.0/8", Some("loopback-net".to_string()), b"k-net".to_vec(), None),
            ("127.0.0.1", Some("loopback-host".to_string()), b"k-host".to_vec(), None),
        ])
        .unwrap();
        let server = TacacsServer::new(([127, 0, 0, 1], 14950).into(), Arc::new(clients), test_engine().await, MAX_BODY_LEN);
        tokio::spawn(async move { server.run().await });

        let mut stream = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                match tokio::net::TcpStream::connect("127.0.0.1:14950").await {
                    Ok(stream) => return stream,
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("server not listening");
        stream.write_all(&request_with_key(b"k-host", &start_body(b"alice", b"secret"))).await.unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(1), read_packet(&mut stream, MAX_BODY_LEN))
            .await
            .expect("no reply")
            .unwrap()
            .expect("connection closed without reply");
        let plain = TacacsPacket::encrypt_decrypt(&reply.body, b"k-host", reply.session_id, reply.version, reply.seq_num);
        assert_eq!(plain[0], AUTHEN_STATUS_PASS, "reply was not encrypted with the /32 client's key");
    }

    #[tokio::test]
    async fn test_chain_looks_up_the_account_name() {
        let db = crate::test_support::test_db();
        db.execute("CREATE TABLE tacacs_norm_users (username TEXT, password TEXT)").await.unwrap();
        db.execute("INSERT INTO tacacs_norm_users VALUES ('alice', 'secret')").await.unwrap();
        let handler = crate::auth_chain::Handler::Sql {
            db,
            user_query: "SELECT password FROM tacacs_norm_users WHERE username = ?".to_string(),
            reply_query: None,
            certificate_query: None, default_user: None,
            timeout: std::time::Duration::from_secs(5),
        };
        let username = crate::config::UsernameSettings { lowercase: true, ..Default::default() };
        let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![handler]).with_username(Some(username));
        let chains = Arc::new(AuthChains::new(Some(chain), Vec::new()));
        let engine = script_engine("return 'reject'").await;
        assert_eq!(authen_status(test_client(), chains, engine, b"ALICE", b"secret").await, AUTHEN_STATUS_PASS);
    }

    #[tokio::test]
    async fn test_all_accept_over_tacacs() {
        let db = crate::test_support::test_db();
        db.execute("CREATE TABLE IF NOT EXISTS tacacs_all_users (username TEXT, password TEXT)").await.unwrap();
        db.execute("INSERT INTO tacacs_all_users VALUES ('alice', 'secret')").await.unwrap();
        let handler = crate::auth_chain::Handler::Sql {
            db: db.clone(),
            user_query: "SELECT password FROM tacacs_all_users WHERE username = ?".to_string(),
            reply_query: None,
            certificate_query: None, default_user: None,
            timeout: std::time::Duration::from_secs(5),
        };
        let policy = crate::auth_chain::Handler::Lua(LuaAuthEngine::new(crate::test_support::test_vms(), db, "return 'accept'").unwrap());
        let chain = AuthChain::new(crate::config::AuthPolicy::AllAccept, vec![handler, policy]);
        let chains = Arc::new(AuthChains::new(Some(chain), Vec::new()));
        let engine = script_engine("return 'accept'").await;
        assert_eq!(authen_status(test_client(), chains.clone(), engine.clone(), b"nosuchuser", b"anything").await, AUTHEN_STATUS_FAIL);
        assert_eq!(authen_status(test_client(), chains.clone(), engine.clone(), b"alice", b"wrong").await, AUTHEN_STATUS_FAIL);
        assert_eq!(authen_status(test_client(), chains, engine, b"alice", b"secret").await, AUTHEN_STATUS_PASS);
    }

    #[tokio::test]
    async fn test_login_records() {
        let chains = Arc::new(AuthChains::new(None, Vec::new()));
        let engine = script_engine("return 'accept'").await;
        assert_eq!(authen_status(test_client(), chains, engine.clone(), b"rec-tacacs-pap", b"secret").await, AUTHEN_STATUS_PASS);
        let records = crate::logging::records_for("rec-tacacs-pap");
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0], crate::logging::OwnedRecord {
            protocol: "tacacs".to_string(),
            client: "local".to_string(),
            source: "127.0.0.1".parse().unwrap(),
            user: "rec-tacacs-pap".to_string(),
            account: "rec-tacacs-pap".to_string(),
            chain: "script".to_string(),
            handler: "script".to_string(),
            decision: "accept".to_string(),
        });
    }

    #[tokio::test]
    async fn test_chain_pap() {
        static SEEDED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
        SEEDED
            .get_or_init(|| async {
                let db = crate::test_support::test_db();
                db.execute("CREATE TABLE tacacs_chain_users (username TEXT, password TEXT)").await.unwrap();
                db.execute("INSERT INTO tacacs_chain_users VALUES ('alice', 'secret')").await.unwrap();
            })
            .await;
        let handler = crate::auth_chain::Handler::Sql {
            db: crate::test_support::test_db(),
            user_query: "SELECT password FROM tacacs_chain_users WHERE username = ?".to_string(),
            reply_query: None,
            certificate_query: None, default_user: None,
            timeout: std::time::Duration::from_secs(5),
        };
        let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![handler]);
        let chains = Arc::new(AuthChains::new(Some(chain), Vec::new()));
        // The top-level script rejects everything, so a PASS proves the chain decided.
        let engine = script_engine("return 'reject'").await;
        for (password, status) in [(&b"secret"[..], AUTHEN_STATUS_PASS), (&b"wrong"[..], AUTHEN_STATUS_FAIL)] {
            let (mut stream, server) = duplex(4096);
            let (engine, chains) = (engine.clone(), chains.clone());
            let task = tokio::spawn(async move {
                serve_as(server, &test_client(), &engine, Some(&chains), MAX_BODY_LEN).await
            });
            stream.write_all(&encrypted_request(TYPE_AUTHEN, 1, 0, &start_body(b"alice", password))).await.unwrap();
            let mut reply = read_packet(&mut stream, MAX_BODY_LEN).await.unwrap().unwrap();
            drop(stream);
            task.await.unwrap().unwrap();
            reply.body = TacacsPacket::encrypt_decrypt(&reply.body, SECRET, reply.session_id, reply.version, reply.seq_num);
            assert_eq!(reply.body[0], status, "password {:?}", String::from_utf8_lossy(password));
        }
    }

    #[test]
    fn test_start_credentials() {
        let attrs = parse_authen_start(&start_body(b"alice", b"secret")).unwrap();
        assert_eq!(start_credentials(&attrs), Credentials { user: Some("alice".to_string()), secret: Secret::pap(b"secret".to_vec()) });
        let mut ascii = start_body(b"alice", b"secret");
        ascii[2] = 0x01; // authen_type ASCII
        let attrs = parse_authen_start(&ascii).unwrap();
        assert_eq!(start_credentials(&attrs).secret, Secret::None);
    }
}
