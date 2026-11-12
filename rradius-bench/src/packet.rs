//! Building requests and checking responses. This is a client's view of RFC 2865, 2866,
//! 2548, 2759, 3579 and 5997, written apart from the server's code so that the tests,
//! which check one with the other, can catch a misreading.
//!
//! A `Template` holds a whole packet for one step and one user, made once. Sending copies
//! it into a buffer and changes only what differs from request to request.
use des::cipher::BlockCipherEncrypt;
use des::Des;
use hmac::{Hmac, KeyInit, Mac};
use md4::Md4;
use md5::{Digest, Md5};
use sha1::Sha1;
use std::net::IpAddr;

pub const MAX_PACKET: usize = 4096;
const HEADER: usize = 20;

pub const ACCESS_REQUEST: u8 = 1;
pub const ACCESS_ACCEPT: u8 = 2;
pub const ACCESS_REJECT: u8 = 3;
pub const ACCOUNTING_REQUEST: u8 = 4;
pub const ACCOUNTING_RESPONSE: u8 = 5;
pub const ACCESS_CHALLENGE: u8 = 11;
pub const STATUS_SERVER: u8 = 12;

const USER_NAME: u8 = 1;
const USER_PASSWORD: u8 = 2;
const CHAP_PASSWORD: u8 = 3;
const NAS_IP_ADDRESS: u8 = 4;
pub const FRAMED_IP_ADDRESS: u8 = 8;
pub const CLASS: u8 = 25;
const VENDOR_SPECIFIC: u8 = 26;
const NAS_IDENTIFIER: u8 = 32;
const ACCT_STATUS_TYPE: u8 = 40;
const ACCT_INPUT_OCTETS: u8 = 42;
const ACCT_OUTPUT_OCTETS: u8 = 43;
const ACCT_SESSION_ID: u8 = 44;
const ACCT_SESSION_TIME: u8 = 46;
const CHAP_CHALLENGE: u8 = 60;
pub const MESSAGE_AUTHENTICATOR: u8 = 80;
const NAS_IPV6_ADDRESS: u8 = 95;

const MICROSOFT: u32 = 311;
const MS_CHAP_CHALLENGE: u8 = 11;
const MS_CHAP2_RESPONSE: u8 = 25;

/// Fixed, as the server remembers nothing of a challenge between requests.
const CHAP_ID: u8 = 5;
const CHAP_FIXED_CHALLENGE: [u8; 16] = *b"rradius-bench-ch";
const MSCHAP_AUTHENTICATOR_CHALLENGE: [u8; 16] = *b"rradius-bench-au";
const MSCHAP_PEER_CHALLENGE: [u8; 16] = *b"rradius-bench-pe";

/// Width of Acct-Session-Id: a `u64` in hexadecimal, so a template can be changed in place.
const SESSION_ID_LEN: usize = 16;

/// One kind of request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Pap,
    Chap,
    ChapAuthenticator,
    MsChapV2,
    AcctStart,
    AcctAlive,
    AcctStop,
    AcctOn,
    AcctOff,
    StatusAuth,
    StatusAcct,
    /// A whole EAP conversation, see `crate::eap`.
    EapTls,
    Peap,
    Ttls,
    AkaPrime,
    /// TACACS+ (RFC 8907), see `crate::tacacs`: not RADIUS, so not in `ALL`.
    TacacsPap,
    TacacsAscii,
    TacacsAuthor,
    TacacsAcct,
}

impl Step {
    pub const ALL: [Step; 15] = [
        Step::Pap,
        Step::Chap,
        Step::ChapAuthenticator,
        Step::MsChapV2,
        Step::AcctStart,
        Step::AcctAlive,
        Step::AcctStop,
        Step::AcctOn,
        Step::AcctOff,
        Step::StatusAuth,
        Step::StatusAcct,
        Step::EapTls,
        Step::Peap,
        Step::Ttls,
        Step::AkaPrime,
    ];

    pub const TACACS: [Step; 4] = [Step::TacacsPap, Step::TacacsAscii, Step::TacacsAuthor, Step::TacacsAcct];

    pub fn name(self) -> &'static str {
        match self {
            Step::Pap => "pap",
            Step::Chap => "chap",
            Step::ChapAuthenticator => "chap-authenticator",
            Step::MsChapV2 => "mschapv2",
            Step::AcctStart => "acct-start",
            Step::AcctAlive => "acct-alive",
            Step::AcctStop => "acct-stop",
            Step::AcctOn => "acct-on",
            Step::AcctOff => "acct-off",
            Step::StatusAuth => "status-auth",
            Step::StatusAcct => "status-acct",
            Step::EapTls => "eap-tls",
            Step::Peap => "peap",
            Step::Ttls => "ttls",
            Step::AkaPrime => "aka-prime",
            Step::TacacsPap => "tacacs-pap",
            Step::TacacsAscii => "tacacs-ascii",
            Step::TacacsAuthor => "tacacs-author",
            Step::TacacsAcct => "tacacs-acct",
        }
    }

    /// Whether this goes to the accounting port.
    pub fn uses_acct_port(self) -> bool {
        self.is_accounting() || self == Step::StatusAcct
    }

    fn is_accounting(self) -> bool {
        matches!(self, Step::AcctStart | Step::AcctAlive | Step::AcctStop | Step::AcctOn | Step::AcctOff)
    }

    fn is_access(self) -> bool {
        matches!(self, Step::Pap | Step::Chap | Step::ChapAuthenticator | Step::MsChapV2) || self.is_eap()
    }

    /// Whether this is an EAP conversation of several Access-Requests.
    pub fn is_eap(self) -> bool {
        matches!(self, Step::EapTls | Step::Peap | Step::Ttls | Step::AkaPrime)
    }

    pub fn is_tacacs(self) -> bool {
        Step::TACACS.contains(&self)
    }

    /// Whether one counted request takes several packets, and `rounds` says how many.
    pub fn has_rounds(self) -> bool {
        self.is_eap() || self == Step::TacacsAscii
    }

    fn code(self) -> u8 {
        match self {
            _ if self.is_access() => ACCESS_REQUEST,
            _ if self.is_accounting() => ACCOUNTING_REQUEST,
            _ => STATUS_SERVER,
        }
    }

    fn carries_user(self) -> bool {
        !matches!(self, Step::AcctOn | Step::AcctOff | Step::StatusAuth | Step::StatusAcct)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraAttribute {
    pub attr_type: u8,
    pub value: Vec<u8>,
}

/// What every request of a run has in common.
#[derive(Debug, Clone)]
pub struct RequestOptions {
    pub nas_ip: IpAddr,
    pub nas_identifier: String,
    pub extra: Vec<ExtraAttribute>,
    pub message_authenticator: bool,
}

/// The secret with its hashing state prepared, so a request does not set it up again.
#[derive(Clone)]
pub struct Signer {
    secret: Vec<u8>,
    hmac: Hmac<Md5>,
}

impl Signer {
    pub fn new(secret: &[u8]) -> Result<Signer, String> {
        let hmac = <Hmac<Md5> as KeyInit>::new_from_slice(secret).map_err(|e| format!("secret: {e}"))?;
        Ok(Signer { secret: secret.to_vec(), hmac })
    }

    fn with_secret(&self, packet: &[u8]) -> [u8; 16] {
        let mut hasher = Md5::new();
        hasher.update(packet);
        hasher.update(&self.secret);
        hasher.finalize().into()
    }
}

/// xorshift64*. Good enough for Request Authenticators in a test tool; not for anything else.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        // A splitmix64 step, so that nearby seeds start far apart. Never zero.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        Rng((z ^ (z >> 31)) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn fill16(&mut self, out: &mut [u8]) {
        out[..8].copy_from_slice(&self.next().to_ne_bytes());
        out[8..16].copy_from_slice(&self.next().to_ne_bytes());
    }
}

/// A whole packet for one step and one user, with where to change it.
#[derive(Debug, Clone)]
pub struct Template {
    step: Step,
    bytes: Vec<u8>,
    message_authenticator: Option<usize>,
    /// Where the hidden password goes, and the padded plaintext.
    password: Option<(usize, Vec<u8>)>,
    /// Where CHAP-Password's hash goes, and the password, when the challenge is the authenticator.
    chap_authenticator: Option<(usize, Vec<u8>)>,
    session_id: Option<usize>,
    session_time: Option<usize>,
}

fn push_attr(bytes: &mut Vec<u8>, attr_type: u8, value: &[u8]) -> Result<usize, String> {
    let length = value.len() + 2;
    if length > 255 {
        return Err(format!("attribute {attr_type}: value of {} octets does not fit", value.len()));
    }
    bytes.push(attr_type);
    bytes.push(length as u8);
    bytes.extend_from_slice(value);
    Ok(bytes.len() - value.len())
}

fn push_microsoft(bytes: &mut Vec<u8>, vendor_type: u8, value: &[u8]) -> Result<(), String> {
    let mut vsa = MICROSOFT.to_be_bytes().to_vec();
    vsa.push(vendor_type);
    vsa.push(value.len() as u8 + 2);
    vsa.extend_from_slice(value);
    push_attr(bytes, VENDOR_SPECIFIC, &vsa).map(|_| ())
}

impl Template {
    pub fn new(step: Step, user: &str, password: &str, options: &RequestOptions) -> Result<Template, String> {
        let mut bytes = vec![step.code(), 0, 0, 0];
        bytes.extend_from_slice(&[0u8; 16]);
        let mut template = Template { step, bytes: Vec::new(), message_authenticator: None, password: None, chap_authenticator: None, session_id: None, session_time: None };

        let signed = step == Step::StatusAuth || step == Step::StatusAcct || (step.is_access() && options.message_authenticator);
        if signed {
            // First: it then covers everything after it (the BlastRADIUS mitigation).
            template.message_authenticator = Some(push_attr(&mut bytes, MESSAGE_AUTHENTICATOR, &[0u8; 16])?);
        }
        if step.carries_user() {
            if user.is_empty() {
                return Err("the user name must not be empty".to_string());
            }
            push_attr(&mut bytes, USER_NAME, user.as_bytes())?;
        }
        match step {
            Step::Pap => {
                if password.is_empty() || password.len() > 128 {
                    return Err("a PAP password must be 1 to 128 octets".to_string());
                }
                let mut plain = password.as_bytes().to_vec();
                plain.resize(password.len().div_ceil(16) * 16, 0);
                let at = push_attr(&mut bytes, USER_PASSWORD, &vec![0u8; plain.len()])?;
                template.password = Some((at, plain));
            }
            Step::Chap => {
                let mut value = vec![CHAP_ID];
                value.extend_from_slice(&chap_hash(password.as_bytes(), &CHAP_FIXED_CHALLENGE));
                push_attr(&mut bytes, CHAP_PASSWORD, &value)?;
                push_attr(&mut bytes, CHAP_CHALLENGE, &CHAP_FIXED_CHALLENGE)?;
            }
            Step::ChapAuthenticator => {
                let at = push_attr(&mut bytes, CHAP_PASSWORD, &[CHAP_ID; 17])?;
                template.chap_authenticator = Some((at + 1, password.as_bytes().to_vec()));
            }
            Step::MsChapV2 => {
                push_microsoft(&mut bytes, MS_CHAP_CHALLENGE, &MSCHAP_AUTHENTICATOR_CHALLENGE)?;
                let response = mschapv2_response(password, user, &MSCHAP_PEER_CHALLENGE, &MSCHAP_AUTHENTICATOR_CHALLENGE)?;
                let mut value = vec![1, 0];
                value.extend_from_slice(&MSCHAP_PEER_CHALLENGE);
                value.extend_from_slice(&[0u8; 8]);
                value.extend_from_slice(&response);
                push_microsoft(&mut bytes, MS_CHAP2_RESPONSE, &value)?;
            }
            _ => {}
        }
        match options.nas_ip {
            IpAddr::V4(address) => push_attr(&mut bytes, NAS_IP_ADDRESS, &address.octets())?,
            IpAddr::V6(address) => push_attr(&mut bytes, NAS_IPV6_ADDRESS, &address.octets())?,
        };
        push_attr(&mut bytes, NAS_IDENTIFIER, options.nas_identifier.as_bytes())?;
        if step.is_accounting() {
            let status: u32 = match step {
                Step::AcctStart => 1,
                Step::AcctStop => 2,
                Step::AcctAlive => 3,
                Step::AcctOn => 7,
                _ => 8,
            };
            push_attr(&mut bytes, ACCT_STATUS_TYPE, &status.to_be_bytes())?;
            template.session_id = Some(push_attr(&mut bytes, ACCT_SESSION_ID, &[b'0'; SESSION_ID_LEN])?);
            if matches!(step, Step::AcctAlive | Step::AcctStop) {
                template.session_time = Some(push_attr(&mut bytes, ACCT_SESSION_TIME, &0u32.to_be_bytes())?);
                push_attr(&mut bytes, ACCT_INPUT_OCTETS, &20000u32.to_be_bytes())?;
                push_attr(&mut bytes, ACCT_OUTPUT_OCTETS, &30000u32.to_be_bytes())?;
            }
        }
        for attribute in &options.extra {
            push_attr(&mut bytes, attribute.attr_type, &attribute.value)?;
        }
        // Room for what a session copies from its Access-Accept.
        if bytes.len() + MAX_COPIED > MAX_PACKET {
            return Err("the attributes do not fit in a packet".to_string());
        }
        template.bytes = bytes;
        Ok(template)
    }

    pub fn step(&self) -> Step {
        self.step
    }

    /// Writes a request into `out` and returns its length and Request Authenticator.
    /// `copied` holds attributes in wire form, to add at the end.
    /// `session_time` tells the Alive requests of one session apart: once the Identifier
    /// comes round, nothing else differs and a server would take one for a retransmission.
    #[allow(clippy::too_many_arguments)]
    pub fn render(&self, out: &mut [u8; MAX_PACKET], identifier: u8, session_id: u64, session_time: u32, copied: &[u8], rng: &mut Rng, signer: &Signer) -> (usize, [u8; 16]) {
        let length = self.bytes.len() + copied.len();
        out[..self.bytes.len()].copy_from_slice(&self.bytes);
        out[self.bytes.len()..length].copy_from_slice(copied);
        out[1] = identifier;
        out[2..4].copy_from_slice(&(length as u16).to_be_bytes());
        if !self.step.is_accounting() {
            rng.fill16(&mut out[4..HEADER]);
        }
        if let Some(at) = self.session_id {
            write_hex(&mut out[at..at + SESSION_ID_LEN], session_id);
        }
        if let Some(at) = self.session_time {
            out[at..at + 4].copy_from_slice(&session_time.to_be_bytes());
        }
        if let Some((at, plain)) = &self.password {
            hide_password(&mut out[..], *at, plain, signer);
        }
        if let Some((at, password)) = &self.chap_authenticator {
            let challenge: [u8; 16] = header_authenticator(out);
            out[*at..*at + 16].copy_from_slice(&chap_hash(password, &challenge));
        }
        if let Some(at) = self.message_authenticator {
            let mut mac = signer.hmac.clone();
            mac.update(&out[..length]);
            out[at..at + 16].copy_from_slice(&mac.finalize().into_bytes());
        }
        if self.step.is_accounting() {
            let digest = signer.with_secret(&out[..length]);
            out[4..HEADER].copy_from_slice(&digest);
        }
        (length, header_authenticator(out))
    }
}

const STATE: u8 = 24;
const EAP_MESSAGE: u8 = 79;

impl Template {
    /// Writes one Access-Request of an EAP conversation: the template, `state` from the last
    /// challenge, and `eap` cut into EAP-Message attributes, then signed. `None` when it does
    /// not fit in a packet.
    pub fn render_eap(&self, out: &mut [u8; MAX_PACKET], identifier: u8, eap: &[u8], state: Option<&[u8]>, rng: &mut Rng, signer: &Signer) -> Option<(usize, [u8; 16])> {
        let mut length = self.bytes.len();
        out[..length].copy_from_slice(&self.bytes);
        let mut push = |attr_type: u8, value: &[u8]| -> Option<()> {
            let end = length + 2 + value.len();
            if value.len() > 253 || end > MAX_PACKET {
                return None;
            }
            out[length] = attr_type;
            out[length + 1] = (value.len() + 2) as u8;
            out[length + 2..end].copy_from_slice(value);
            length = end;
            Some(())
        };
        if let Some(state) = state {
            push(STATE, state)?;
        }
        for chunk in eap.chunks(253) {
            push(EAP_MESSAGE, chunk)?;
        }
        out[1] = identifier;
        out[2..4].copy_from_slice(&(length as u16).to_be_bytes());
        rng.fill16(&mut out[4..HEADER]);
        let at = self.message_authenticator?;
        out[at..at + 16].fill(0);
        let mut mac = signer.hmac.clone();
        mac.update(&out[..length]);
        out[at..at + 16].copy_from_slice(&mac.finalize().into_bytes());
        Some((length, header_authenticator(out)))
    }
}

/// The EAP-Message attributes of a packet joined, and its State, if any.
pub fn eap_parts(packet: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
    let (mut eap, mut state) = (Vec::new(), None);
    for (attr_type, value) in crate::eap::attributes(packet) {
        match attr_type {
            EAP_MESSAGE => eap.extend_from_slice(value),
            STATE if state.is_none() => state = Some(value.to_vec()),
            _ => {}
        }
    }
    (eap, state)
}

/// The secret, for what has to decrypt with it.
impl Signer {
    pub fn secret(&self) -> &[u8] {
        &self.secret
    }
}

/// The most a session copies from an Access-Accept into its accounting: a Class that fills
/// its attribute, and a Framed-IP-Address.
pub const MAX_COPIED: usize = 255 + 6;

fn header_authenticator(packet: &[u8]) -> [u8; 16] {
    let mut authenticator = [0u8; 16];
    authenticator.copy_from_slice(&packet[4..HEADER]);
    authenticator
}

fn write_hex(out: &mut [u8], value: u64) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = DIGITS[((value >> (60 - i * 4)) & 0xf) as usize];
    }
}

/// RFC 2865 section 5.2: c1 = p1 XOR MD5(secret + Request Authenticator), ci = pi XOR MD5(secret + c(i-1)).
fn hide_password(packet: &mut [u8], at: usize, plain: &[u8], signer: &Signer) {
    let mut previous = header_authenticator(packet);
    for (i, block) in plain.chunks(16).enumerate() {
        let mut hasher = Md5::new();
        hasher.update(&signer.secret);
        hasher.update(previous);
        let pad = hasher.finalize();
        let start = at + i * 16;
        for (j, (p, b)) in block.iter().zip(pad.iter()).enumerate() {
            packet[start + j] = p ^ b;
            previous[j] = p ^ b;
        }
    }
}

fn chap_hash(password: &[u8], challenge: &[u8]) -> [u8; 16] {
    let mut hasher = Md5::new();
    hasher.update([CHAP_ID]);
    hasher.update(password);
    hasher.update(challenge);
    hasher.finalize().into()
}

/// GenerateNTResponse, RFC 2759 section 8.1.
pub(crate) fn mschapv2_response(password: &str, user: &str, peer: &[u8; 16], authenticator: &[u8; 16]) -> Result<[u8; 24], String> {
    let name = user.rsplit('\\').next().unwrap_or(user);
    let mut hasher = Sha1::new();
    hasher.update(peer);
    hasher.update(authenticator);
    hasher.update(name.as_bytes());
    let digest = hasher.finalize();
    let unicode: Vec<u8> = password.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let hash = Md4::digest(&unicode);
    let mut padded = [0u8; 21];
    padded[..16].copy_from_slice(&hash);
    let mut response = [0u8; 24];
    for (key, out) in padded.chunks(7).zip(response.chunks_mut(8)) {
        let spread = [
            key[0],
            (key[0] << 7) | (key[1] >> 1),
            (key[1] << 6) | (key[2] >> 2),
            (key[2] << 5) | (key[3] >> 3),
            (key[3] << 4) | (key[4] >> 4),
            (key[4] << 3) | (key[5] >> 5),
            (key[5] << 2) | (key[6] >> 6),
            key[6] << 1,
        ];
        let cipher = Des::new_from_slice(&spread).map_err(|e| format!("DES key: {e}"))?;
        out.copy_from_slice(&digest[..8]);
        cipher.encrypt_block(out.try_into().map_err(|_| "DES block".to_string())?);
    }
    Ok(response)
}

/// What a verified response says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Accept,
    Reject,
    Challenge,
    Accounting,
}

/// Why a response was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    Malformed,
    ResponseAuthenticator,
    MessageAuthenticator,
    UnexpectedCode,
}

/// Checks a response against the Request Authenticator of its request.
pub fn verify_response(reply: &[u8], step: Step, request_authenticator: &[u8; 16], signer: &Signer) -> Result<Answer, Invalid> {
    if reply.len() < HEADER || usize::from(u16::from_be_bytes([reply[2], reply[3]])) != reply.len() {
        return Err(Invalid::Malformed);
    }
    let mut message_authenticator = None;
    let mut at = HEADER;
    while at < reply.len() {
        let Some(&length) = reply.get(at + 1) else { return Err(Invalid::Malformed) };
        let length = usize::from(length);
        if length < 2 || at + length > reply.len() {
            return Err(Invalid::Malformed);
        }
        if reply[at] == MESSAGE_AUTHENTICATOR {
            if length != 18 {
                return Err(Invalid::Malformed);
            }
            message_authenticator = Some(at + 2);
        }
        at += length;
    }
    let mut hasher = Md5::new();
    hasher.update(&reply[..4]);
    hasher.update(request_authenticator);
    hasher.update(&reply[HEADER..]);
    hasher.update(&signer.secret);
    if hasher.finalize().as_slice() != &reply[4..HEADER] {
        return Err(Invalid::ResponseAuthenticator);
    }
    if let Some(at) = message_authenticator {
        // RFC 3579: over the response with the Request Authenticator in place of its own.
        let mut mac = signer.hmac.clone();
        mac.update(&reply[..4]);
        mac.update(request_authenticator);
        mac.update(&reply[HEADER..at]);
        mac.update(&[0u8; 16]);
        mac.update(&reply[at + 16..]);
        if mac.finalize().into_bytes().as_slice() != &reply[at..at + 16] {
            return Err(Invalid::MessageAuthenticator);
        }
    }
    let answer = match reply[0] {
        ACCESS_ACCEPT => Answer::Accept,
        ACCESS_REJECT => Answer::Reject,
        ACCESS_CHALLENGE => Answer::Challenge,
        ACCOUNTING_RESPONSE => Answer::Accounting,
        _ => return Err(Invalid::UnexpectedCode),
    };
    let allowed = match step {
        Step::Pap | Step::Chap | Step::ChapAuthenticator | Step::MsChapV2 | Step::EapTls | Step::Peap | Step::Ttls | Step::AkaPrime => answer != Answer::Accounting,
        Step::StatusAuth => matches!(answer, Answer::Accept | Answer::Reject),
        _ => answer == Answer::Accounting,
    };
    if allowed { Ok(answer) } else { Err(Invalid::UnexpectedCode) }
}

/// Appends the Class and Framed-IP-Address of a verified response, in wire form, to `copied`.
pub fn copy_for_accounting(reply: &[u8], copied: &mut Vec<u8>) {
    let mut at = HEADER;
    let mut class = false;
    let mut framed = false;
    while at + 2 <= reply.len() {
        let length = usize::from(reply[at + 1]);
        if length < 2 || at + length > reply.len() {
            return;
        }
        let first = match reply[at] {
            CLASS => !std::mem::replace(&mut class, true),
            FRAMED_IP_ADDRESS => !std::mem::replace(&mut framed, true),
            _ => false,
        };
        if first {
            copied.extend_from_slice(&reply[at..at + length]);
        }
        at += length;
    }
}

/// Writes a correctly signed response to `request` into `out`, as a responder would, and
/// returns its length. A Message-Authenticator comes first when asked for.
pub fn write_response(out: &mut [u8; MAX_PACKET], request: &[u8], code: u8, attributes: &[(u8, &[u8])], with_message_authenticator: bool, signer: &Signer) -> usize {
    out[0] = code;
    out[1] = request[1];
    let mut at = HEADER;
    let signature_at = with_message_authenticator.then(|| {
        out[at] = MESSAGE_AUTHENTICATOR;
        out[at + 1] = 18;
        out[at + 2..at + 18].fill(0);
        at += 18;
        at - 16
    });
    for (attr_type, value) in attributes {
        out[at] = *attr_type;
        out[at + 1] = value.len() as u8 + 2;
        out[at + 2..at + 2 + value.len()].copy_from_slice(value);
        at += value.len() + 2;
    }
    out[2..4].copy_from_slice(&(at as u16).to_be_bytes());
    out[4..HEADER].copy_from_slice(&request[4..HEADER]);
    if let Some(signature_at) = signature_at {
        let mut mac = signer.hmac.clone();
        mac.update(&out[..at]);
        out[signature_at..signature_at + 16].copy_from_slice(&mac.finalize().into_bytes());
    }
    let digest = signer.with_secret(&out[..at]);
    out[4..HEADER].copy_from_slice(&digest);
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"s3cret";

    /// A request as these tests read it, to check it against the RFCs rather than against
    /// this tool's own encoding. Whether a server accepts it is checked by the tests that
    /// run the tool against the server's executable.
    struct Request {
        code: u8,
        identifier: u8,
        authenticator: [u8; 16],
        /// Type and value of each attribute, in order.
        attributes: Vec<(u8, Vec<u8>)>,
        /// The packet up to its Length.
        bytes: Vec<u8>,
    }

    /// Reads a RADIUS packet: the header, then attributes up to its Length. The tests in
    /// `tests/common/mod.rs` have the same reader, as they cannot use this module; keep the
    /// two alike.
    fn parse(bytes: &[u8]) -> Result<Request, String> {
        let header = bytes.get(..HEADER).ok_or("shorter than a RADIUS header")?;
        let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
        let body = bytes.get(HEADER..length).ok_or_else(|| format!("Length {length} is not within the {} octets", bytes.len()))?;
        let mut authenticator = [0u8; 16];
        authenticator.copy_from_slice(&header[4..HEADER]);
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

    impl Request {
        /// The value of the first attribute of `kind`.
        fn attribute(&self, kind: u8) -> Option<&[u8]> {
            self.attributes.iter().find(|(found, _)| *found == kind).map(|(_, value)| value.as_slice())
        }

        /// The value of the first Microsoft vendor attribute of `kind` (RFC 2548).
        fn microsoft(&self, kind: u8) -> Option<&[u8]> {
            self.attributes.iter().filter(|(found, _)| *found == VENDOR_SPECIFIC).find_map(|(_, value)| {
                let (vendor, rest) = value.split_first_chunk::<4>()?;
                (u32::from_be_bytes(*vendor) == MICROSOFT && rest.first() == Some(&kind)).then(|| rest.get(2..)).flatten()
            })
        }

        /// Whether the Message-Authenticator is HMAC-MD5 of the packet with its own value
        /// as 16 zero octets (RFC 3579 section 3.2); None without one.
        fn verify_message_authenticator(&self, secret: &[u8]) -> Option<bool> {
            let mut packet = self.bytes.clone();
            let (mut at, mut found) = (HEADER, None);
            while at + 2 <= packet.len() {
                let size = usize::from(packet[at + 1]);
                if packet[at] == MESSAGE_AUTHENTICATOR && size == 18 {
                    found = Some(packet[at + 2..at + 18].to_vec());
                    packet[at + 2..at + 18].fill(0);
                    break;
                }
                at += size.max(2);
            }
            let expected = found?;
            let mut mac = <Hmac<Md5> as KeyInit>::new_from_slice(secret).ok()?;
            mac.update(&packet);
            Some(mac.finalize().into_bytes()[..] == expected[..])
        }

        /// Whether the Request Authenticator is MD5 of the packet, with 16 zero octets in
        /// its place, followed by `secret` (RFC 2866 section 3).
        fn verify_accounting_request(&self, secret: &[u8]) -> bool {
            let mut packet = self.bytes.clone();
            packet[4..HEADER].fill(0);
            Md5::new().chain_update(&packet).chain_update(secret).finalize()[..] == self.authenticator
        }

        /// The User-Password, decrypted as RFC 2865 section 5.2 describes, without the
        /// zero octets that pad it.
        fn pap_password(&self, secret: &[u8]) -> Result<Vec<u8>, String> {
            let hidden = self.attribute(USER_PASSWORD).ok_or("no User-Password")?;
            if hidden.is_empty() || hidden.len() % 16 != 0 {
                return Err(format!("User-Password of {} octets", hidden.len()));
            }
            let (mut plain, mut previous) = (Vec::with_capacity(hidden.len()), self.authenticator.to_vec());
            for block in hidden.chunks(16) {
                let key = Md5::new().chain_update(secret).chain_update(&previous).finalize();
                plain.extend(block.iter().zip(key.iter()).map(|(c, b)| c ^ b));
                previous = block.to_vec();
            }
            while plain.last() == Some(&0) {
                plain.pop();
            }
            Ok(plain)
        }
    }

    fn options() -> RequestOptions {
        RequestOptions {
            nas_ip: "192.0.2.1".parse().unwrap(),
            nas_identifier: "nas".to_string(),
            extra: vec![ExtraAttribute { attr_type: 31, value: b"caller".to_vec() }],
            message_authenticator: true,
        }
    }

    fn render(step: Step, password: &str, options: &RequestOptions) -> (Vec<u8>, [u8; 16]) {
        let signer = Signer::new(SECRET).unwrap();
        let template = Template::new(step, "alice", password, options).unwrap();
        let mut out = [0u8; MAX_PACKET];
        let (length, authenticator) = template.render(&mut out, 7, 0xabc, 60, &[], &mut Rng::new(1), &signer);
        (out[..length].to_vec(), authenticator)
    }

    /// The request `render` makes for `step`, after the checks every Access-Request passes.
    fn checked(step: Step, password: &str) -> Result<Request, String> {
        let (bytes, _) = render(step, password, &options());
        let packet = parse(&bytes)?;
        assert_eq!(packet.identifier, 7);
        assert_eq!(packet.attributes[0].0, MESSAGE_AUTHENTICATOR, "{step:?}");
        assert_eq!(packet.verify_message_authenticator(SECRET), Some(true), "{step:?}");
        assert_eq!(packet.attribute(31), Some(&b"caller"[..]));
        assert_eq!(packet.attribute(USER_NAME), Some(&b"alice"[..]), "{step:?}");
        Ok(packet)
    }

    #[test]
    fn test_requests_pass_server_checks() -> Result<(), String> {
        for password in ["pw", "exactly16octets!", "a password longer than one block of sixteen"] {
            let found = checked(Step::Pap, password)?;
            assert_eq!(found.pap_password(SECRET)?, password.as_bytes());
        }

        // CHAP-Password is the ID and MD5(ID + password + challenge), the challenge being
        // CHAP-Challenge or else the Request Authenticator (RFC 2865 section 2.2).
        for step in [Step::Chap, Step::ChapAuthenticator] {
            let packet = checked(step, "pw")?;
            let (id, response) = packet.attribute(CHAP_PASSWORD).and_then(|value| value.split_first()).ok_or("no CHAP-Password")?;
            let challenge = packet.attribute(CHAP_CHALLENGE).unwrap_or(&packet.authenticator);
            assert_eq!(*id, CHAP_ID);
            assert_eq!(response, Md5::new().chain_update([*id]).chain_update(b"pw").chain_update(challenge).finalize().as_slice(), "{step:?}");
        }
        let (with_challenge, _) = render(Step::Chap, "pw", &options());
        assert!(parse(&with_challenge)?.attribute(CHAP_CHALLENGE).is_some());
        let (with_authenticator, authenticator) = render(Step::ChapAuthenticator, "pw", &options());
        let packet = parse(&with_authenticator)?;
        assert!(packet.attribute(CHAP_CHALLENGE).is_none());
        assert_eq!(packet.authenticator, authenticator);

        // MS-CHAP2-Response: ident, flags, peer challenge, 8 reserved, then the NT-Response
        // (RFC 2548 section 2.3.2), which the RFC 2759 test vector checks the reckoning of.
        let packet = checked(Step::MsChapV2, "pw")?;
        let challenge: [u8; 16] = packet.microsoft(MS_CHAP_CHALLENGE).and_then(|value| value.try_into().ok()).ok_or("no MS-CHAP-Challenge")?;
        let response = packet.microsoft(MS_CHAP2_RESPONSE).filter(|value| value.len() == 50).ok_or("no MS-CHAP2-Response")?;
        let peer: [u8; 16] = response[2..18].try_into().map_err(|_| "peer challenge")?;
        assert_eq!(response[26..], mschapv2_response("pw", "alice", &peer, &challenge)?);
        assert_ne!(response[26..], mschapv2_response("other", "alice", &peer, &challenge)?);

        for step in [Step::StatusAuth, Step::StatusAcct] {
            let (bytes, _) = render(step, "pw", &options());
            let packet = parse(&bytes)?;
            assert_eq!(packet.code, STATUS_SERVER);
            assert_eq!(packet.attributes[0].0, MESSAGE_AUTHENTICATOR);
            assert_eq!(packet.verify_message_authenticator(SECRET), Some(true));
        }

        for (step, status, has_counters, has_user) in [
            (Step::AcctStart, 1u32, false, true),
            (Step::AcctAlive, 3, true, true),
            (Step::AcctStop, 2, true, true),
            (Step::AcctOn, 7, false, false),
            (Step::AcctOff, 8, false, false),
        ] {
            let (bytes, authenticator) = render(step, "pw", &options());
            let packet = parse(&bytes)?;
            assert_eq!(packet.code, ACCOUNTING_REQUEST);
            assert!(packet.verify_accounting_request(SECRET), "{step:?}");
            assert_eq!(packet.authenticator, authenticator);
            assert!(packet.attribute(MESSAGE_AUTHENTICATOR).is_none(), "{step:?}");
            assert_eq!(packet.attribute(ACCT_STATUS_TYPE), Some(&status.to_be_bytes()[..]));
            assert_eq!(packet.attribute(ACCT_SESSION_ID), Some(&b"0000000000000abc"[..]));
            assert_eq!(packet.attribute(ACCT_SESSION_TIME).is_some(), has_counters, "{step:?}");
            assert_eq!(packet.attribute(ACCT_INPUT_OCTETS).is_some(), has_counters, "{step:?}");
            assert_eq!(packet.attribute(ACCT_OUTPUT_OCTETS).is_some(), has_counters, "{step:?}");
            assert_eq!(packet.attribute(USER_NAME).is_some(), has_user, "{step:?}");
        }
        Ok(())
    }

    #[test]
    fn test_every_request_names_its_nas() {
        for step in Step::ALL {
            let (bytes, _) = render(step, "pw", &options());
            let packet = parse(&bytes).unwrap();
            assert_eq!(packet.attribute(NAS_IP_ADDRESS), Some(&[192, 0, 2, 1][..]), "{step:?}");
            assert_eq!(packet.attribute(NAS_IDENTIFIER), Some(&b"nas"[..]), "{step:?}");
        }
    }

    #[test]
    fn test_no_message_authenticator_only_for_access_requests() {
        let mut without = options();
        without.message_authenticator = false;
        let (pap, _) = render(Step::Pap, "pw", &without);
        assert!(parse(&pap).unwrap().attribute(MESSAGE_AUTHENTICATOR).is_none());
        let (status, _) = render(Step::StatusAuth, "pw", &without);
        assert_eq!(parse(&status).unwrap().verify_message_authenticator(SECRET), Some(true));
    }

    #[test]
    fn test_authenticators_differ_between_requests() {
        let signer = Signer::new(SECRET).unwrap();
        let template = Template::new(Step::Pap, "alice", "pw", &options()).unwrap();
        let mut rng = Rng::new(9);
        let mut out = [0u8; MAX_PACKET];
        let (_, first) = template.render(&mut out, 1, 0, 60, &[], &mut rng, &signer);
        let (_, second) = template.render(&mut out, 1, 0, 60, &[], &mut rng, &signer);
        assert_ne!(first, second);
    }

    #[test]
    fn test_copied_attributes_are_covered() {
        let signer = Signer::new(SECRET).unwrap();
        let template = Template::new(Step::AcctStop, "alice", "pw", &options()).unwrap();
        let mut out = [0u8; MAX_PACKET];
        let copied = [CLASS, 5, b'a', b'b', b'c'];
        let (length, _) = template.render(&mut out, 1, 1, 60, &copied, &mut Rng::new(1), &signer);
        let packet = parse(&out[..length]).unwrap();
        assert!(packet.verify_accounting_request(SECRET));
        assert_eq!(packet.attribute(CLASS), Some(&b"abc"[..]));
    }

    #[test]
    fn test_rfc2759_nt_response() {
        let authenticator = [0x5B, 0x5D, 0x7C, 0x7D, 0x7B, 0x3F, 0x2F, 0x3E, 0x3C, 0x2C, 0x60, 0x21, 0x32, 0x26, 0x26, 0x28];
        let peer = [0x21, 0x40, 0x23, 0x24, 0x25, 0x5E, 0x26, 0x2A, 0x28, 0x29, 0x5F, 0x2B, 0x3A, 0x33, 0x7C, 0x7E];
        let expected = [
            0x82, 0x30, 0x9E, 0xCD, 0x8D, 0x70, 0x8B, 0x5E, 0xA0, 0x8F, 0xAA, 0x39, 0x81, 0xCD, 0x83, 0x54, 0x42, 0x33, 0x11, 0x4A, 0x3D, 0x85, 0xD6, 0xDF,
        ];
        assert_eq!(mschapv2_response("clientPass", "User", &peer, &authenticator).unwrap(), expected);
    }

    #[test]
    fn test_template_limits() {
        assert!(Template::new(Step::Pap, "", "pw", &options()).is_err());
        assert!(Template::new(Step::Pap, "alice", "", &options()).is_err());
        assert!(Template::new(Step::Pap, "alice", &"x".repeat(129), &options()).is_err());
        assert!(Template::new(Step::Pap, &"x".repeat(254), "pw", &options()).is_err());
        let mut many = options();
        many.extra = vec![ExtraAttribute { attr_type: 31, value: vec![b'x'; 253] }; 16];
        assert!(Template::new(Step::Pap, "alice", "pw", &many).is_err());
    }

    /// A response to the request `render` made, signed as a server signs it: a
    /// Message-Authenticator first in an Access-Accept, Access-Reject or Access-Challenge.
    fn server_response(code: u8, step: Step, signer: &Signer) -> (Vec<u8>, [u8; 16]) {
        let (bytes, authenticator) = render(step, "pw", &options());
        let with_message_authenticator = matches!(code, ACCESS_ACCEPT | ACCESS_REJECT | ACCESS_CHALLENGE);
        let mut out = [0u8; MAX_PACKET];
        let length = write_response(&mut out, &bytes, code, &[(18, b"hello")], with_message_authenticator, signer);
        (out[..length].to_vec(), authenticator)
    }

    #[test]
    fn test_verify_accepts_server_responses() {
        let signer = Signer::new(SECRET).unwrap();
        for (code, step, answer) in [
            (ACCESS_ACCEPT, Step::Pap, Answer::Accept),
            (ACCESS_REJECT, Step::Chap, Answer::Reject),
            (ACCESS_CHALLENGE, Step::MsChapV2, Answer::Challenge),
            (ACCOUNTING_RESPONSE, Step::AcctStart, Answer::Accounting),
            (ACCOUNTING_RESPONSE, Step::StatusAcct, Answer::Accounting),
            (ACCESS_ACCEPT, Step::StatusAuth, Answer::Accept),
        ] {
            let (reply, authenticator) = server_response(code, step, &signer);
            assert_eq!(verify_response(&reply, step, &authenticator, &signer), Ok(answer), "{step:?}");
        }
    }

    #[test]
    fn test_verify_refuses_tampering() {
        let signer = Signer::new(SECRET).unwrap();
        let (reply, authenticator) = server_response(ACCESS_ACCEPT, Step::Pap, &signer);
        let mut wrong_request = authenticator;
        wrong_request[0] ^= 1;
        assert_eq!(verify_response(&reply, Step::Pap, &wrong_request, &signer), Err(Invalid::ResponseAuthenticator));

        let mut forged = reply.clone();
        forged[4] ^= 1;
        assert_eq!(verify_response(&forged, Step::Pap, &authenticator, &signer), Err(Invalid::ResponseAuthenticator));

        let other = Signer::new(b"other").unwrap();
        assert_eq!(verify_response(&reply, Step::Pap, &authenticator, &other), Err(Invalid::ResponseAuthenticator));

        // A wrong Message-Authenticator under a correct Response Authenticator.
        let mut signed = reply.clone();
        let at = 22;
        assert_eq!(signed[20], MESSAGE_AUTHENTICATOR);
        signed[at] ^= 1;
        let digest = {
            let mut scratch = signed.clone();
            scratch[4..20].copy_from_slice(&authenticator);
            signer.with_secret(&scratch)
        };
        signed[4..20].copy_from_slice(&digest);
        assert_eq!(verify_response(&signed, Step::Pap, &authenticator, &signer), Err(Invalid::MessageAuthenticator));

        assert_eq!(verify_response(&reply[..10], Step::Pap, &authenticator, &signer), Err(Invalid::Malformed));
        let mut short = reply.clone();
        short[3] -= 1;
        assert_eq!(verify_response(&short, Step::Pap, &authenticator, &signer), Err(Invalid::Malformed));
    }

    #[test]
    fn test_verify_refuses_wrong_codes() {
        let signer = Signer::new(SECRET).unwrap();
        for (code, step) in [
            (ACCOUNTING_RESPONSE, Step::Pap),
            (ACCESS_ACCEPT, Step::AcctStart),
            (ACCESS_ACCEPT, Step::StatusAcct),
            (ACCESS_CHALLENGE, Step::StatusAuth),
            (ACCOUNTING_RESPONSE, Step::StatusAuth),
        ] {
            let (reply, authenticator) = server_response(code, step, &signer);
            assert_eq!(verify_response(&reply, step, &authenticator, &signer), Err(Invalid::UnexpectedCode), "{step:?}");
        }
        let (bytes, authenticator) = render(Step::Pap, "pw", &options());
        let mut unknown = [0u8; MAX_PACKET];
        let length = write_response(&mut unknown, &bytes, 40, &[], false, &signer);
        assert_eq!(verify_response(&unknown[..length], Step::Pap, &authenticator, &signer), Err(Invalid::UnexpectedCode));
    }

    #[test]
    fn test_write_response_passes_verification() {
        let signer = Signer::new(SECRET).unwrap();
        let (request, authenticator) = render(Step::Pap, "pw", &options());
        let mut out = [0u8; MAX_PACKET];
        let length = write_response(&mut out, &request, ACCESS_ACCEPT, &[(CLASS, b"gold"), (FRAMED_IP_ADDRESS, &[10, 0, 0, 1])], true, &signer);
        assert_eq!(verify_response(&out[..length], Step::Pap, &authenticator, &signer), Ok(Answer::Accept));
        let mut copied = Vec::new();
        copy_for_accounting(&out[..length], &mut copied);
        assert_eq!(copied, [CLASS, 6, b'g', b'o', b'l', b'd', FRAMED_IP_ADDRESS, 6, 10, 0, 0, 1]);
    }
}
