//! A supplicant's side of EAP-TLS (RFC 5216, RFC 9190), PEAPv0 with inner EAP-MSCHAPv2 and
//! EAP-TTLS with inner PAP (RFC 5281), enough to run whole logins against a server. Written
//! apart from the server's code, as the rest of this tool is, so that the tests can catch a
//! misreading on either side.
//!
//! TLS runs on buffers: what the server sends goes into the TLS connection, and what it
//! writes goes out in EAP-Responses, cut into fragments.
use crate::packet::Step;
use hmac::digest::Digest;
use md4::Md4;
use md5::Md5;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection};
use sha1::Sha1;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

/// Octets of TLS data in one EAP-Response unless `--eap-fragment` says otherwise.
pub const DEFAULT_FRAGMENT: usize = 1000;
/// What `--eap-fragment` may be: a fragment, its headers and the rest of the request fit in a packet.
const FRAGMENT_RANGE: std::ops::RangeInclusive<usize> = 64..=2000;
/// The most one TLS message may be, in all its fragments.
const MAX_MESSAGE: usize = 64 * 1024;

const CODE_REQUEST: u8 = 1;
const CODE_RESPONSE: u8 = 2;
pub const CODE_SUCCESS: u8 = 3;
const TYPE_IDENTITY: u8 = 1;
const TYPE_NAK: u8 = 3;
const TYPE_TLS: u8 = 13;
const TYPE_TTLS: u8 = 21;
const TYPE_PEAP: u8 = 25;
const TYPE_MSCHAPV2: u8 = 26;
const TYPE_TLV: u8 = 33;
const FLAG_LENGTH: u8 = 0x80;
const FLAG_MORE: u8 = 0x40;
const FLAG_START: u8 = 0x20;

const MSCHAP_CHALLENGE: u8 = 1;
const MSCHAP_RESPONSE: u8 = 2;
const MSCHAP_SUCCESS: u8 = 3;
const MSCHAP_FAILURE: u8 = 4;
/// Fixed: a test tool has no use for a fresh one, and the server's challenge is fresh anyway.
const PEER_CHALLENGE: [u8; 16] = *b"rradius-bench-pc";

const AVP_USER_NAME: u32 = 1;
const AVP_USER_PASSWORD: u32 = 2;
const AVP_MANDATORY: u8 = 0x40;

const VENDOR_SPECIFIC: u8 = 26;
const MICROSOFT: u32 = 311;
const MS_MPPE_SEND_KEY: u8 = 16;
const MS_MPPE_RECV_KEY: u8 = 17;

/// Which EAP method a load runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Tls,
    Peap,
    Ttls,
    /// No TLS: see `crate::aka`.
    AkaPrime,
}

impl Method {
    pub fn of(step: Step) -> Option<Method> {
        match step {
            Step::EapTls => Some(Method::Tls),
            Step::Peap => Some(Method::Peap),
            Step::Ttls => Some(Method::Ttls),
            Step::AkaPrime => Some(Method::AkaPrime),
            _ => None,
        }
    }

    fn eap_type(self) -> u8 {
        match self {
            Method::Tls => TYPE_TLS,
            Method::Peap => TYPE_PEAP,
            Method::Ttls => TYPE_TTLS,
            Method::AkaPrime => crate::aka::TYPE_AKA_PRIME,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Method::Tls => "eap-tls",
            Method::Peap => "peap",
            Method::Ttls => "ttls",
            Method::AkaPrime => "aka-prime",
        }
    }
}

/// What the command line says about EAP.
#[derive(Debug, Clone)]
pub struct EapOptions {
    pub tls_version: String,
    pub ca_file: Option<std::path::PathBuf>,
    pub tls_name: String,
    pub cert_file: Option<std::path::PathBuf>,
    pub key_file: Option<std::path::PathBuf>,
    pub fragment: usize,
}

impl Default for EapOptions {
    fn default() -> Self {
        EapOptions { tls_version: "1.2".to_string(), ca_file: None, tls_name: "localhost".to_string(), cert_file: None, key_file: None, fragment: DEFAULT_FRAGMENT }
    }
}

/// What every conversation of one load shares.
pub struct EapSetup {
    method: Method,
    config: Arc<ClientConfig>,
    name: ServerName<'static>,
    fragment: usize,
    /// AKA': the Ki and OPc every card of the load has.
    card: Option<crate::aka::CardKeys>,
}

impl EapSetup {
    /// Checks the options for `method` and reads the files they name. Errors name the option.
    pub fn new(method: Method, options: &EapOptions) -> Result<Arc<EapSetup>, String> {
        let version = match options.tls_version.as_str() {
            "1.2" => &rustls::version::TLS12,
            "1.3" if method == Method::Tls => &rustls::version::TLS13,
            "1.3" => return Err(format!("--tls-version 1.3: only for eap-tls, not {}", method.name())),
            other => return Err(format!("--tls-version {other}: 1.2 or 1.3")),
        };
        if !FRAGMENT_RANGE.contains(&options.fragment) {
            return Err(format!("--eap-fragment {}: from {} to {}", options.fragment, FRAGMENT_RANGE.start(), FRAGMENT_RANGE.end()));
        }
        let name = ServerName::try_from(options.tls_name.to_string()).map_err(|_| format!("--tls-name {}: not a DNS name or an IP address", options.tls_name))?;
        let client = match (method, options.cert_file.as_deref(), options.key_file.as_deref()) {
            (Method::Tls, Some(cert_file), Some(key_file)) => Some((cert_file, key_file)),
            (Method::Tls, None, _) => return Err("eap-tls needs --cert-file".to_string()),
            (Method::Tls, _, None) => return Err("eap-tls needs --key-file".to_string()),
            _ => None,
        };
        let config = crate::tls::client_config(&[version], options.ca_file.as_deref(), client)?;
        Ok(Arc::new(EapSetup { method, config, name, fragment: options.fragment, card: None }))
    }

    /// AKA' needs no TLS: `password` is the cards' `KI:OPC`.
    pub fn aka(password: &str) -> Result<Arc<EapSetup>, String> {
        let card = crate::aka::CardKeys::parse(password)?;
        let config = crate::tls::client_config(&[&rustls::version::TLS12], None, None)?;
        let name = ServerName::try_from("localhost").map_err(|_| "localhost is not a DNS name".to_string())?;
        Ok(Arc::new(EapSetup { method: Method::AkaPrime, config, name, fragment: DEFAULT_FRAGMENT, card: Some(card) }))
    }

    pub fn method(&self) -> Method {
        self.method
    }
}

/// Where the inner method of PEAP stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeapState {
    /// Waiting for the server's inner Identity request.
    Opening,
    /// Answered the Identity; waiting for the MS-CHAPv2 Challenge.
    Identified,
    /// Answered the Challenge; waiting for Success or Failure.
    Responded,
    /// Acknowledged the result; waiting for the EAP-TLV Result.
    Told,
    Done,
}

/// One login in progress.
pub struct EapClient {
    setup: Arc<EapSetup>,
    identity: Vec<u8>,
    password: String,
    tls: Option<ClientConnection>,
    /// A TLS message from the server, while its fragments come in.
    incoming: Vec<u8>,
    /// What to send, and how much of it went out already.
    outgoing: Vec<u8>,
    sent: usize,
    peap: PeapState,
    /// MS-CHAPv2: the server's challenge and our response, for checking its Success.
    mschap: Option<([u8; 16], [u8; 24])>,
    avps_sent: bool,
    /// Asked once for this method instead of the one the server offered.
    nak_sent: bool,
    aka: Option<crate::aka::AkaPeer>,
}

impl EapClient {
    pub fn new(setup: Arc<EapSetup>, identity: &str, password: &str) -> EapClient {
        let aka = setup.card.clone().map(|card| crate::aka::AkaPeer::new(card, identity.as_bytes()));
        EapClient {
            setup,
            identity: identity.as_bytes().to_vec(),
            password: password.to_string(),
            tls: None,
            incoming: Vec::new(),
            outgoing: Vec::new(),
            sent: 0,
            peap: PeapState::Opening,
            mschap: None,
            avps_sent: false,
            nak_sent: false,
            aka,
        }
    }

    /// The EAP-Response/Identity that opens a conversation.
    pub fn start(&self) -> Vec<u8> {
        eap_packet(CODE_RESPONSE, 0, TYPE_IDENTITY, &self.identity)
    }

    /// The EAP-Response to the server's EAP-Request.
    pub fn next(&mut self, request: &[u8]) -> Result<Vec<u8>, String> {
        let (code, id, length) = match request {
            [code, id, high, low, ..] => (*code, *id, usize::from(u16::from_be_bytes([*high, *low]))),
            _ => return Err("an EAP packet shorter than its header".to_string()),
        };
        if length != request.len() || length < 5 {
            return Err("an EAP packet whose Length does not fit".to_string());
        }
        if code != CODE_REQUEST {
            return Err(format!("EAP code {code} in an Access-Challenge"));
        }
        let eap_type = request[4];
        if eap_type == TYPE_IDENTITY {
            return Ok(eap_packet(CODE_RESPONSE, id, TYPE_IDENTITY, &self.identity));
        }
        let method = self.setup.method;
        if eap_type != method.eap_type() {
            // A server offering several methods starts with its first: a legacy Nak (RFC 3748
            // section 5.3.1) asks for this one instead, once, before anything has begun.
            if self.tls.is_none() && !self.aka.as_ref().is_some_and(crate::aka::AkaPeer::started) && !self.nak_sent {
                self.nak_sent = true;
                return Ok(eap_packet(CODE_RESPONSE, id, TYPE_NAK, &[method.eap_type()]));
            }
            return Err(format!("EAP type {eap_type} offered instead of {}", method.name()));
        }
        if let Some(peer) = self.aka.as_mut() {
            let data = peer.answer(request, |data| eap_packet(CODE_RESPONSE, id, crate::aka::TYPE_AKA_PRIME, data))?;
            return Ok(eap_packet(CODE_RESPONSE, id, crate::aka::TYPE_AKA_PRIME, &data));
        }
        let Some((&flags, mut data)) = request[5..].split_first() else {
            return Err("an EAP-TLS request without flags".to_string());
        };
        if flags & FLAG_START != 0 {
            if self.tls.is_some() {
                return Err("a second TLS start".to_string());
            }
            let connection = ClientConnection::new(self.setup.config.clone(), self.setup.name.clone()).map_err(|e| format!("TLS: {e}"))?;
            self.tls = Some(connection);
        } else if data.is_empty() {
            // The server acknowledged a fragment of ours.
            if self.sent < self.outgoing.len() {
                return Ok(self.fragment(id));
            }
            return Err("an empty EAP-TLS request with nothing left to send".to_string());
        } else {
            if flags & FLAG_LENGTH != 0 {
                let (total, rest) = data.split_first_chunk::<4>().ok_or("an L flag without a length")?;
                if u32::from_be_bytes(*total) as usize > MAX_MESSAGE {
                    return Err(format!("a TLS message of more than {MAX_MESSAGE} octets"));
                }
                data = rest;
            }
            if self.incoming.len() + data.len() > MAX_MESSAGE {
                return Err(format!("a TLS message of more than {MAX_MESSAGE} octets"));
            }
            self.incoming.extend_from_slice(data);
            if flags & FLAG_MORE != 0 {
                return Ok(self.acknowledgement(id));
            }
            self.take_incoming()?;
        }
        self.collect_outgoing()?;
        if self.outgoing.is_empty() {
            return Ok(self.acknowledgement(id));
        }
        Ok(self.fragment(id))
    }

    /// Hands a whole TLS message from the server to the connection, and answers what comes
    /// out of the tunnel.
    fn take_incoming(&mut self) -> Result<(), String> {
        let tls = self.tls.as_mut().ok_or("TLS data before the TLS start")?;
        let mut data = &self.incoming[..];
        while !data.is_empty() {
            tls.read_tls(&mut data).map_err(|e| format!("TLS: {e}"))?;
            tls.process_new_packets().map_err(|e| format!("TLS: {e}"))?;
        }
        self.incoming.clear();
        let mut plain = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            match tls.reader().read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => plain.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(format!("TLS: {error}")),
            }
        }
        let handshaking = tls.is_handshaking();
        let reply = match self.setup.method {
            // RFC 9190's one-octet commitment message, which needs no answer.
            Method::Tls => None,
            Method::Peap if plain.is_empty() => None,
            Method::Peap => Some(self.peap_receive(&plain)?),
            Method::Ttls if !handshaking && !self.avps_sent => {
                self.avps_sent = true;
                Some(self.avps())
            }
            Method::Ttls if plain.is_empty() => None,
            Method::Ttls => return Err("inner data from the server, where TTLS with PAP expects none".to_string()),
            Method::AkaPrime => return Err("TLS data in an AKA' conversation".to_string()),
        };
        if let Some(reply) = reply {
            let tls = self.tls.as_mut().ok_or("TLS data before the TLS start")?;
            tls.writer().write_all(&reply).map_err(|e| format!("TLS: {e}"))?;
        }
        Ok(())
    }

    /// Takes what the connection has to send, once the last of it has gone.
    fn collect_outgoing(&mut self) -> Result<(), String> {
        if self.sent < self.outgoing.len() {
            return Err("a new TLS message from the server before ours was acknowledged".to_string());
        }
        self.outgoing.clear();
        self.sent = 0;
        let Some(tls) = self.tls.as_mut() else { return Ok(()) };
        while tls.wants_write() {
            tls.write_tls(&mut self.outgoing).map_err(|e| format!("TLS: {e}"))?;
        }
        Ok(())
    }

    /// The next fragment of what there is to send: the first of several says how long the
    /// whole is, and all but the last say more follows.
    fn fragment(&mut self, id: u8) -> Vec<u8> {
        let total = self.outgoing.len();
        let end = (self.sent + self.setup.fragment).min(total);
        let fragmented = total > self.setup.fragment;
        let mut flags = 0;
        let mut data = Vec::with_capacity(end - self.sent + 5);
        data.push(0);
        if fragmented && self.sent == 0 {
            flags |= FLAG_LENGTH;
            data.extend_from_slice(&(total as u32).to_be_bytes());
        }
        if end < total {
            flags |= FLAG_MORE;
        }
        data[0] = flags;
        data.extend_from_slice(&self.outgoing[self.sent..end]);
        self.sent = end;
        eap_packet(CODE_RESPONSE, id, self.setup.method.eap_type(), &data)
    }

    fn acknowledgement(&self, id: u8) -> Vec<u8> {
        eap_packet(CODE_RESPONSE, id, self.setup.method.eap_type(), &[0])
    }

    /// PEAPv0's inner EAP goes without its four-octet header, except EAP-TLV, which keeps it.
    fn peap_receive(&mut self, message: &[u8]) -> Result<Vec<u8>, String> {
        if let [CODE_REQUEST, id, high, low, TYPE_TLV, tlv @ ..] = message {
            if usize::from(u16::from_be_bytes([*high, *low])) != message.len() || self.peap != PeapState::Told {
                return Err("an EAP-TLV out of turn".to_string());
            }
            let [kind_high, kind_low, 0, 2, 0, status] = *tlv else { return Err("an EAP-TLV that is no Result".to_string()) };
            if u16::from_be_bytes([kind_high, kind_low]) & 0x3fff != 3 {
                return Err("an EAP-TLV that is no Result".to_string());
            }
            self.peap = PeapState::Done;
            return Ok(eap_packet(CODE_RESPONSE, *id, TYPE_TLV, &[kind_high, kind_low, 0, 2, 0, status]));
        }
        match (self.peap, message) {
            (PeapState::Opening, [TYPE_IDENTITY, ..]) => {
                self.peap = PeapState::Identified;
                let mut reply = vec![TYPE_IDENTITY];
                reply.extend_from_slice(&self.identity);
                Ok(reply)
            }
            (PeapState::Identified, [TYPE_MSCHAPV2, MSCHAP_CHALLENGE, id, _, _, 16, rest @ ..]) if rest.len() >= 16 => {
                let mut challenge = [0u8; 16];
                challenge.copy_from_slice(&rest[..16]);
                let name = std::str::from_utf8(&self.identity).map_err(|_| "the user name is not text".to_string())?;
                let response = crate::packet::mschapv2_response(&self.password, name, &PEER_CHALLENGE, &challenge)?;
                self.mschap = Some((challenge, response));
                self.peap = PeapState::Responded;
                let length = 4 + 1 + 49 + self.identity.len();
                let mut reply = vec![TYPE_MSCHAPV2, MSCHAP_RESPONSE, *id, (length >> 8) as u8, length as u8, 49];
                reply.extend_from_slice(&PEER_CHALLENGE);
                reply.extend_from_slice(&[0u8; 8]);
                reply.extend_from_slice(&response);
                reply.push(0);
                reply.extend_from_slice(&self.identity);
                Ok(reply)
            }
            (PeapState::Responded, [TYPE_MSCHAPV2, MSCHAP_SUCCESS, _, _, _, text @ ..]) => {
                let (challenge, response) = self.mschap.ok_or("an MS-CHAPv2 Success before a Response")?;
                let expected = authenticator_response(&self.password, &response, &PEER_CHALLENGE, &challenge, &self.identity);
                if !text.starts_with(expected.as_bytes()) {
                    return Err("an MS-CHAPv2 Success that does not prove the server knows the password".to_string());
                }
                self.peap = PeapState::Told;
                Ok(vec![TYPE_MSCHAPV2, MSCHAP_SUCCESS])
            }
            (PeapState::Responded, [TYPE_MSCHAPV2, MSCHAP_FAILURE, ..]) => {
                self.peap = PeapState::Told;
                Ok(vec![TYPE_MSCHAPV2, MSCHAP_FAILURE])
            }
            (state, _) => Err(format!("an inner PEAP message out of turn ({state:?})")),
        }
    }

    /// TTLS's inner PAP: User-Name and User-Password AVPs, the password padded to 16 octets.
    fn avps(&self) -> Vec<u8> {
        let mut password = self.password.as_bytes().to_vec();
        password.resize(password.len().div_ceil(16).max(1) * 16, 0);
        let mut avps = avp(AVP_USER_NAME, &self.identity);
        avps.extend(avp(AVP_USER_PASSWORD, &password));
        avps
    }

    /// Whether the MS-MPPE keys in an Access-Accept are the ones this login's TLS session or
    /// AKA' Challenge gives, decrypted with `secret` and the Request Authenticator of the request it answers.
    pub fn check_keys(&self, accept: &[u8], request_authenticator: &[u8; 16], secret: &[u8]) -> Result<(), String> {
        let msk = match &self.aka {
            Some(peer) => *peer.msk().ok_or("an Access-Accept before the AKA' Challenge was answered")?,
            None => self.tls_msk()?,
        };
        let (mut recv, mut send) = (None, None);
        for (attr_type, value) in attributes(accept) {
            if attr_type != VENDOR_SPECIFIC {
                continue;
            }
            if let [a, b, c, d, vendor_type, length, rest @ ..] = value {
                if u32::from_be_bytes([*a, *b, *c, *d]) == MICROSOFT && usize::from(*length) == rest.len() + 2 {
                    match *vendor_type {
                        MS_MPPE_RECV_KEY => recv = Some(rest),
                        MS_MPPE_SEND_KEY => send = Some(rest),
                        _ => {}
                    }
                }
            }
        }
        let (Some(recv), Some(send)) = (recv, send) else { return Err("an Access-Accept without MS-MPPE keys".to_string()) };
        let recv = decrypt_key(recv, secret, request_authenticator).ok_or("an MS-MPPE-Recv-Key that does not decrypt")?;
        let send = decrypt_key(send, secret, request_authenticator).ok_or("an MS-MPPE-Send-Key that does not decrypt")?;
        if recv != msk[..32] || send != msk[32..] {
            return Err("MS-MPPE keys that differ from the TLS session's".to_string());
        }
        Ok(())
    }

    /// The MSK of a TLS method, once its inner method is over.
    fn tls_msk(&self) -> Result<[u8; 64], String> {
        let tls = self.tls.as_ref().ok_or("an Access-Accept with no TLS session")?;
        if tls.is_handshaking() {
            return Err("an Access-Accept before the TLS handshake finished".to_string());
        }
        // Keys alone do not show the password was checked: the inner method has to be over.
        let inner_done = match self.setup.method {
            Method::Tls => true,
            Method::Peap => self.peap == PeapState::Done,
            Method::Ttls => self.avps_sent,
            Method::AkaPrime => false,
        };
        if !inner_done {
            return Err("an Access-Accept before the inner method finished".to_string());
        }
        // RFC 9190 exports 128 octets, of which the MSK is the first 64, and a TLS 1.3 exporter
        // gives other octets for another length. The TLS 1.2 PRF gives the same first octets.
        let mut material = [0u8; 128];
        let tls13 = tls.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_3);
        let (label, context): (&[u8], Option<&[u8]>) = match self.setup.method {
            Method::Ttls => (b"ttls keying material", None),
            Method::Tls if tls13 => (b"EXPORTER_EAP_TLS_Key_Material", Some(&[TYPE_TLS])),
            _ => (b"client EAP encryption", None),
        };
        tls.export_keying_material(&mut material[..], label, context).map_err(|e| format!("TLS: {e}"))?;
        Ok(std::array::from_fn(|i| material[i]))
    }
}

fn eap_packet(code: u8, id: u8, eap_type: u8, data: &[u8]) -> Vec<u8> {
    let length = 5 + data.len();
    let mut packet = vec![code, id, (length >> 8) as u8, length as u8, eap_type];
    packet.extend_from_slice(data);
    packet
}

/// One Diameter AVP (RFC 5281 section 10.1), padded to four octets.
fn avp(code: u32, data: &[u8]) -> Vec<u8> {
    let length = 8 + data.len();
    let mut avp = code.to_be_bytes().to_vec();
    avp.push(AVP_MANDATORY);
    avp.extend_from_slice(&(length as u32).to_be_bytes()[1..]);
    avp.extend_from_slice(data);
    avp.resize(length.div_ceil(4) * 4, 0);
    avp
}

/// The attributes of a RADIUS packet, stopping at the first that does not fit.
pub fn attributes(packet: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut at = 20;
    std::iter::from_fn(move || {
        let (&attr_type, &length) = (packet.get(at)?, packet.get(at + 1)?);
        let length = usize::from(length);
        if length < 2 || at + length > packet.len() {
            return None;
        }
        let value = &packet[at + 2..at + length];
        at += length;
        Some((attr_type, value))
    })
}

/// RFC 2548 section 2.4.2: a salt, then the key length and the key, padded and encrypted
/// with MD5 of the secret, the Request Authenticator and the salt, block after block.
fn decrypt_key(value: &[u8], secret: &[u8], request_authenticator: &[u8; 16]) -> Option<Vec<u8>> {
    let (salt, cipher) = value.split_first_chunk::<2>()?;
    if cipher.is_empty() || !cipher.len().is_multiple_of(16) {
        return None;
    }
    let mut plain = Vec::with_capacity(cipher.len());
    let mut previous: Vec<u8> = [&request_authenticator[..], &salt[..]].concat();
    for block in cipher.chunks(16) {
        let pad = Md5::new().chain_update(secret).chain_update(&previous).finalize();
        plain.extend(block.iter().zip(pad.iter()).map(|(c, p)| c ^ p));
        previous = block.to_vec();
    }
    let length = usize::from(*plain.first()?);
    plain.get(1..1 + length).map(<[u8]>::to_vec)
}

/// GenerateAuthenticatorResponse, RFC 2759 section 8.7: "S=" and 40 hexadecimal digits.
fn authenticator_response(password: &str, nt_response: &[u8; 24], peer: &[u8; 16], authenticator: &[u8; 16], user: &[u8]) -> String {
    const MAGIC1: &[u8] = b"Magic server to client signing constant";
    const MAGIC2: &[u8] = b"Pad to make it do more than one iteration";
    let unicode: Vec<u8> = password.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let hash_hash = Md4::digest(Md4::digest(&unicode));
    let digest = Sha1::new().chain_update(hash_hash).chain_update(nt_response).chain_update(MAGIC1).finalize();
    let name = user.rsplit(|byte| *byte == b'\\').next().unwrap_or(user);
    let challenge = Sha1::new().chain_update(peer).chain_update(authenticator).chain_update(name).finalize();
    let digest = Sha1::new().chain_update(digest).chain_update(&challenge[..8]).chain_update(MAGIC2).finalize();
    let hex: String = digest.iter().map(|byte| format!("{byte:02X}")).collect();
    format!("S={hex}")
}

/// Prints `line` to standard error the first time a conversation fails for the reason `kind`
/// stands for, so a reader learns each reason without a line per request.
pub fn report_once(kind: &str, line: &str) {
    static SEEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let Ok(mut seen) = SEEN.lock() else { return };
    if seen.get_or_insert_with(HashSet::new).insert(kind.to_string()) {
        eprintln!("rradius-bench: {line}");
    }
}

/// What a failure is about, without its detail: what comes before the first colon, so that
/// `TLS: ...` errors are one kind and each other reason is its own.
pub fn failure_kind(reason: &str) -> &str {
    reason.split(':').next().unwrap_or(reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::PrivateKeyDer;
    use rustls::{ServerConfig, ServerConnection};
    use std::path::Path;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
    }

    fn setup(method: Method, dir: &Path) -> Arc<EapSetup> {
        setup_for(method, dir, "1.2")
    }

    fn setup_for(method: Method, dir: &Path, tls_version: &str) -> Arc<EapSetup> {
        let options = EapOptions { cert_file: Some(dir.join("client.pem")), key_file: Some(dir.join("client.key")), tls_version: tls_version.to_string(), ..EapOptions::default() };
        EapSetup::new(method, &options).unwrap()
    }

    /// A CA-less pair of certificates: the server's and a client's, as PEM files.
    fn certificates(dir: &Path) -> Arc<ServerConfig> {
        certificates_for(dir, &rustls::version::TLS12)
    }

    fn certificates_for(dir: &Path, version: &'static rustls::SupportedProtocolVersion) -> Arc<ServerConfig> {
        let mut server_certificate = None;
        for name in ["server", "client"] {
            let key = rcgen::KeyPair::generate().unwrap();
            let certificate = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap().self_signed(&key).unwrap();
            std::fs::write(dir.join(format!("{name}.pem")), certificate.pem()).unwrap();
            std::fs::write(dir.join(format!("{name}.key")), key.serialize_pem()).unwrap();
            if name == "server" {
                server_certificate = Some((certificate.der().clone(), PrivateKeyDer::try_from(key.serialize_der()).unwrap()));
            }
        }
        let (certificate, key) = server_certificate.unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[version])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certificate], key)
            .unwrap();
        Arc::new(config)
    }

    fn request(id: u8, eap_type: u8, data: &[u8]) -> Vec<u8> {
        eap_packet(CODE_REQUEST, id, eap_type, data)
    }

    /// The TLS data in an EAP-TLS response, and its flags.
    fn tls_data(response: &[u8]) -> (u8, Option<u32>, Vec<u8>) {
        assert_eq!(response[0], CODE_RESPONSE);
        assert_eq!(usize::from(u16::from_be_bytes([response[2], response[3]])), response.len());
        let flags = response[5];
        let (total, data) = if flags & FLAG_LENGTH != 0 { (Some(u32::from_be_bytes(response[6..10].try_into().unwrap())), &response[10..]) } else { (None, &response[6..]) };
        (flags, total, data.to_vec())
    }

    #[test]
    fn test_fragments_as_the_spec_example() {
        let dir = tempfile::tempdir().unwrap();
        certificates(dir.path());
        let mut client = EapClient::new(setup(Method::Peap, dir.path()), "alice", "secret");
        client.outgoing = vec![7; 2500];
        let first = client.fragment(5);
        let second = client.next(&request(6, TYPE_PEAP, &[0])).unwrap();
        let third = client.next(&request(7, TYPE_PEAP, &[0])).unwrap();
        let fragments: Vec<_> = [first, second, third].iter().map(|response| tls_data(response)).collect();
        assert_eq!(fragments.iter().map(|(_, _, data)| data.len()).collect::<Vec<_>>(), [1000, 1000, 500]);
        assert_eq!(fragments.iter().map(|(flags, total, _)| (flags & FLAG_LENGTH != 0, *total, flags & FLAG_MORE != 0)).collect::<Vec<_>>(), [(true, Some(2500), true), (false, None, true), (false, None, false)]);
        assert!(client.next(&request(8, TYPE_PEAP, &[0])).is_err(), "nothing left to send");
    }

    #[test]
    fn test_short_message_is_not_fragmented() {
        let dir = tempfile::tempdir().unwrap();
        certificates(dir.path());
        let mut client = EapClient::new(setup(Method::Ttls, dir.path()), "alice", "secret");
        client.outgoing = vec![7; 300];
        let (flags, total, data) = tls_data(&client.fragment(1));
        assert_eq!((flags, total, data.len()), (0, None, 300));
    }

    #[test]
    fn test_fragments_from_the_server_are_acknowledged_and_reassembled() {
        let dir = tempfile::tempdir().unwrap();
        certificates(dir.path());
        let mut client = EapClient::new(setup(Method::Peap, dir.path()), "alice", "secret");
        assert!(client.next(&request(1, TYPE_PEAP, &[FLAG_START])).is_ok());
        let mut first = vec![FLAG_LENGTH | FLAG_MORE];
        first.extend_from_slice(&20u32.to_be_bytes());
        first.extend_from_slice(&[1; 10]);
        let acknowledgement = client.next(&request(2, TYPE_PEAP, &first)).unwrap();
        assert_eq!(acknowledgement, eap_packet(CODE_RESPONSE, 2, TYPE_PEAP, &[0]));
        assert_eq!(client.incoming.len(), 10, "kept until the last fragment");
        let mut too_long = vec![FLAG_LENGTH | FLAG_MORE];
        too_long.extend_from_slice(&(MAX_MESSAGE as u32 + 1).to_be_bytes());
        assert!(client.next(&request(3, TYPE_PEAP, &too_long)).is_err());
    }

    #[test]
    fn test_other_requests_refused() {
        let dir = tempfile::tempdir().unwrap();
        certificates(dir.path());
        let mut client = EapClient::new(setup(Method::Peap, dir.path()), "alice", "secret");
        assert_eq!(client.next(&request(1, TYPE_IDENTITY, &[])).unwrap(), eap_packet(CODE_RESPONSE, 1, TYPE_IDENTITY, b"alice"));
        assert_eq!(client.next(&request(2, TYPE_TTLS, &[FLAG_START])).unwrap(), eap_packet(CODE_RESPONSE, 2, TYPE_NAK, &[TYPE_PEAP]), "a Nak for the method wanted");
        assert!(client.next(&request(3, TYPE_TTLS, &[FLAG_START])).is_err(), "only one Nak");
        assert!(client.next(&eap_packet(CODE_RESPONSE, 3, TYPE_PEAP, &[FLAG_START])).is_err(), "not a request");
        assert!(client.next(&[1, 2, 0, 9, 25]).is_err(), "a Length that does not fit");
    }

    /// The server's TLS data as EAP-Requests, fragmented as a server would.
    fn server_requests(server: &mut ServerConnection, id: &mut u8, eap_type: u8) -> Vec<Vec<u8>> {
        let mut data = Vec::new();
        while server.wants_write() {
            server.write_tls(&mut data).unwrap();
        }
        let chunks: Vec<&[u8]> = data.chunks(1000).collect();
        chunks
            .iter()
            .enumerate()
            .map(|(index, chunk)| {
                let mut body = vec![if index + 1 < chunks.len() { FLAG_MORE } else { 0 }];
                if index == 0 && chunks.len() > 1 {
                    body[0] |= FLAG_LENGTH;
                    body.extend_from_slice(&(data.len() as u32).to_be_bytes());
                }
                body.extend_from_slice(chunk);
                *id = id.wrapping_add(1);
                request(*id, eap_type, &body)
            })
            .collect()
    }

    /// Runs the TLS handshake of `method` between a client and a rustls server over EAP.
    fn handshake(method: Method, dir: &Path) -> (EapClient, ServerConnection) {
        handshake_over(method, dir, &rustls::version::TLS12)
    }

    fn handshake_over(method: Method, dir: &Path, version: &'static rustls::SupportedProtocolVersion) -> (EapClient, ServerConnection) {
        let config = certificates_for(dir, version);
        let mut server = ServerConnection::new(config).unwrap();
        let name = if version == &rustls::version::TLS13 { "1.3" } else { "1.2" };
        let mut client = EapClient::new(setup_for(method, dir, name), "alice", "secret");
        let eap_type = method.eap_type();
        let mut id = 1;
        let mut response = client.next(&request(id, eap_type, &[FLAG_START])).unwrap();
        for _ in 0..20 {
            let (flags, _, data) = tls_data(&response);
            let mut reader = &data[..];
            while !reader.is_empty() {
                server.read_tls(&mut reader).unwrap();
            }
            if flags & FLAG_MORE != 0 {
                id += 1;
                response = client.next(&request(id, eap_type, &[0])).unwrap();
                continue;
            }
            server.process_new_packets().unwrap();
            if !server.is_handshaking() && !server.wants_write() && client.tls.as_ref().is_some_and(|tls| !tls.is_handshaking()) {
                return (client, server);
            }
            let requests = server_requests(&mut server, &mut id, eap_type);
            if requests.is_empty() {
                return (client, server);
            }
            for request in requests {
                response = client.next(&request).unwrap();
            }
        }
        panic!("no end to the handshake");
    }

    /// RFC 2548 section 2.4.2, written out: what a server sends for `key`.
    fn encrypted_key(key: &[u8], secret: &[u8], request_authenticator: &[u8; 16], salt: [u8; 2]) -> Vec<u8> {
        let mut plain = vec![key.len() as u8];
        plain.extend_from_slice(key);
        plain.resize(plain.len().div_ceil(16) * 16, 0);
        let mut out = salt.to_vec();
        let mut previous: Vec<u8> = [&request_authenticator[..], &salt[..]].concat();
        for block in plain.chunks(16) {
            let pad = Md5::new().chain_update(secret).chain_update(&previous).finalize();
            let cipher: Vec<u8> = block.iter().zip(pad.iter()).map(|(p, k)| p ^ k).collect();
            previous = cipher.clone();
            out.extend(cipher);
        }
        out
    }

    /// An Access-Accept with these keys, as attributes after a header.
    fn accept_with(recv: &[u8], send: &[u8], secret: &[u8], request_authenticator: &[u8; 16]) -> Vec<u8> {
        let mut packet = vec![2, 1, 0, 0];
        packet.extend_from_slice(&[0; 16]);
        for (vendor_type, key, salt) in [(MS_MPPE_RECV_KEY, recv, [0x80, 1]), (MS_MPPE_SEND_KEY, send, [0x80, 2])] {
            let value = encrypted_key(key, secret, request_authenticator, salt);
            let mut vsa = MICROSOFT.to_be_bytes().to_vec();
            vsa.extend([vendor_type, (value.len() + 2) as u8]);
            vsa.extend(value);
            packet.extend([VENDOR_SPECIFIC, (vsa.len() + 2) as u8]);
            packet.extend(vsa);
        }
        packet
    }

    #[test]
    fn test_handshake_over_eap_and_keys_checked() {
        for (method, label) in [(Method::Peap, &b"client EAP encryption"[..]), (Method::Ttls, &b"ttls keying material"[..])] {
            let dir = tempfile::tempdir().unwrap();
            let (mut client, server) = handshake(method, dir.path());
            assert!(!server.is_handshaking(), "{method:?}");
            let mut msk = [0u8; 64];
            server.export_keying_material(&mut msk[..], label, None).unwrap();
            let (secret, authenticator) = (b"s".as_slice(), [9u8; 16]);
            if method == Method::Peap {
                let error = client.check_keys(&accept_with(&msk[..32], &msk[32..], secret, &authenticator), &authenticator, secret).unwrap_err();
                assert!(error.contains("inner method"), "the right keys, but MS-CHAPv2 never ran: {error}");
                client.peap = PeapState::Done;
            }
            assert_eq!(client.check_keys(&accept_with(&msk[..32], &msk[32..], secret, &authenticator), &authenticator, secret), Ok(()), "{method:?}");
            let mut wrong = msk;
            wrong[0] ^= 1;
            assert!(client.check_keys(&accept_with(&wrong[..32], &msk[32..], secret, &authenticator), &authenticator, secret).is_err(), "{method:?}: another Recv-Key");
            assert!(client.check_keys(&accept_with(&msk[..32], &msk[32..], b"other", &authenticator), &authenticator, secret).is_err(), "{method:?}: another secret");
            let mut header_only = vec![2, 1, 0, 20];
            header_only.extend_from_slice(&[0; 16]);
            assert!(client.check_keys(&header_only, &authenticator, secret).is_err(), "{method:?}: no keys");
        }
    }

    /// EAP-TLS keys: RFC 5216 for TLS 1.2, and RFC 9190 for TLS 1.3, whose 128 octets of
    /// key material give an MSK other than 64 octets would.
    #[test]
    fn test_eap_tls_keys_for_both_versions() {
        for (version, label, context) in [(&rustls::version::TLS12, &b"client EAP encryption"[..], None), (&rustls::version::TLS13, &b"EXPORTER_EAP_TLS_Key_Material"[..], Some(&[TYPE_TLS][..]))] {
            let dir = tempfile::tempdir().unwrap();
            let (client, server) = handshake_over(Method::Tls, dir.path(), version);
            assert_eq!(server.protocol_version(), Some(version.version));
            let mut material = [0u8; 128];
            server.export_keying_material(&mut material[..], label, context).unwrap();
            let (secret, authenticator) = (b"s".as_slice(), [4u8; 16]);
            assert_eq!(client.check_keys(&accept_with(&material[..32], &material[32..64], secret, &authenticator), &authenticator, secret), Ok(()), "{version:?}");
        }
    }

    #[test]
    fn test_ttls_sends_its_avps_once_the_handshake_is_done() {
        let dir = tempfile::tempdir().unwrap();
        let (client, mut server) = handshake(Method::Ttls, dir.path());
        assert!(client.avps_sent);
        let mut plain = Vec::new();
        let _ = server.reader().read_to_end(&mut plain);
        assert_eq!(plain, client.avps(), "the AVPs came through the tunnel");
    }

    #[test]
    fn test_failure_kinds() {
        assert_eq!(failure_kind("TLS: invalid peer certificate: UnknownIssuer"), "TLS");
        assert_eq!(failure_kind("TLS: received fatal alert"), "TLS");
        assert_eq!(failure_kind("MS-MPPE keys that differ from the TLS session's"), "MS-MPPE keys that differ from the TLS session's");
    }

    #[test]
    fn test_avps() {
        let dir = tempfile::tempdir().unwrap();
        certificates(dir.path());
        let client = EapClient::new(setup(Method::Ttls, dir.path()), "alice", "secret");
        let mut expected = hex("00000001400000_0d".replace('_', "").as_str());
        expected.extend(b"alice");
        expected.extend([0, 0, 0]);
        expected.extend(hex("00000002400000_18".replace('_', "").as_str()));
        expected.extend(b"secret");
        expected.extend([0; 10]);
        assert_eq!(client.avps(), expected);
    }

    /// RFC 2759 section 9.2.
    #[test]
    fn test_mschapv2_against_the_rfc_example() {
        let authenticator: [u8; 16] = hex("5B5D7C7D7B3F2F3E3C2C602132262628").try_into().unwrap();
        let peer: [u8; 16] = hex("21402324255E262A28295F2B3A337C7E").try_into().unwrap();
        let response = crate::packet::mschapv2_response("clientPass", "User", &peer, &authenticator).unwrap();
        assert_eq!(response.to_vec(), hex("82309ECD8D708B5EA08FAA3981CD83544233114A3D85D6DF"));
        assert_eq!(authenticator_response("clientPass", &response, &peer, &authenticator, b"User"), "S=407A5589115FD0D6209F510FE9C04566932CDA56");
    }

    #[test]
    fn test_peap_inner_conversation() {
        let dir = tempfile::tempdir().unwrap();
        certificates(dir.path());
        let mut client = EapClient::new(setup(Method::Peap, dir.path()), "alice", "secret");
        assert_eq!(client.peap_receive(&[TYPE_IDENTITY]).unwrap(), [&[TYPE_IDENTITY][..], b"alice"].concat());
        let challenge = [3u8; 16];
        let mut message = vec![TYPE_MSCHAPV2, MSCHAP_CHALLENGE, 42, 0, 4 + 1 + 16 + 7, 16];
        message.extend_from_slice(&challenge);
        message.extend_from_slice(b"rradius");
        let response = client.peap_receive(&message).unwrap();
        assert_eq!(&response[..3], [TYPE_MSCHAPV2, MSCHAP_RESPONSE, 42]);
        assert_eq!(usize::from(u16::from_be_bytes([response[3], response[4]])), response.len() - 1, "the length counts from the opcode");
        assert_eq!(response[5], 49);
        assert_eq!(&response[response.len() - 5..], b"alice");
        let nt: [u8; 24] = response[30..54].try_into().unwrap();
        assert!(client.peap_receive(&[TYPE_MSCHAPV2, MSCHAP_SUCCESS, 42, 0, 46, b'S']).is_err(), "a Success that proves nothing");
        let proof = authenticator_response("secret", &nt, &PEER_CHALLENGE, &challenge, b"alice");
        let mut success = vec![TYPE_MSCHAPV2, MSCHAP_SUCCESS, 42, 0, (4 + proof.len()) as u8];
        success.extend_from_slice(proof.as_bytes());
        assert_eq!(client.peap_receive(&success).unwrap(), [TYPE_MSCHAPV2, MSCHAP_SUCCESS]);
        let result = [CODE_REQUEST, 43, 0, 11, TYPE_TLV, 0x80, 0x03, 0, 2, 0, 1];
        assert_eq!(client.peap_receive(&result).unwrap(), [CODE_RESPONSE, 43, 0, 11, TYPE_TLV, 0x80, 0x03, 0, 2, 0, 1]);
        assert!(client.peap_receive(&[TYPE_IDENTITY]).is_err(), "nothing after the result");
    }
}
