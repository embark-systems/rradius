//! EAP conversations carried in RADIUS: joining and splitting `EAP-Message`, sessions across
//! rounds, and the methods. RFC 3579 for the carriage, RFC 3748 for EAP, RFC 5216 and RFC 9190
//! for EAP-TLS. TLS runs on buffers here, through rustls, and never touches a socket.

use zeroize::Zeroizing;
use crate::config::EapSettings;
use crate::eap_method::{EapMethod, LookupError, MethodConversation, MethodData, MethodStep};
use crate::common::AttrValue;
use crate::metrics::Metrics;
use crate::reload::Live;
use parking_lot::Mutex;
use rustls::pki_types::CertificateDer;
use rustls::ServerConnection;
use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::{FromDer, X509Certificate};

/// RFC 3579 section 3.1.
pub const EAP_MESSAGE: u8 = 79;
/// Requests one conversation may receive. A login takes under ten.
pub const MAX_ROUNDS: u32 = 256;
/// How many times `session_timeout_ms` a conversation may last however busy it is, so that
/// settings it began with, its CRL among them, are not good for as long as a peer keeps talking.
const LIFETIME_TIMEOUTS: u32 = 4;
/// What fits in one RADIUS attribute.
const MAX_ATTRIBUTE_VALUE: usize = 253;

pub(crate) const CODE_REQUEST: u8 = 1;
pub(crate) const CODE_RESPONSE: u8 = 2;
const CODE_SUCCESS: u8 = 3;
const CODE_FAILURE: u8 = 4;

pub const TYPE_IDENTITY: u8 = 1;
pub const TYPE_NAK: u8 = 3;
pub const TYPE_TLS: u8 = 13;
pub const TYPE_PEAP: u8 = 25;
pub const TYPE_TTLS: u8 = 21;

/// TLS Message Length follows the flags.
const FLAG_LENGTH: u8 = 0x80;
const FLAG_MORE: u8 = 0x40;
pub(crate) const FLAG_START: u8 = 0x20;

/// PEAP's version, in the flags.
const FLAG_VERSION: u8 = 0x07;

const TYPE_MSCHAPV2: u8 = 26;
const TYPE_TLV: u8 = 33;
const MSCHAP_CHALLENGE: u8 = 1;
const MSCHAP_RESPONSE: u8 = 2;
const MSCHAP_SUCCESS: u8 = 3;
const MSCHAP_FAILURE: u8 = 4;
/// A mandatory Result TLV, and what it can say.
const TLV_RESULT: [u8; 2] = [0x80, 0x03];
const TLV_SUCCESS: u8 = 1;
const TLV_FAILURE: u8 = 2;
/// The Name in an MS-CHAPv2 Challenge.
const SERVER_NAME: &[u8] = b"rradius";
/// A login takes at most 4.
const MAX_INNER_ROUNDS: u8 = 8;
/// What a RADIUS attribute holds.
const MAX_INNER_NAME: usize = 253;
/// What a RADIUS `User-Password` holds.
const MAX_INNER_PASSWORD: usize = 128;
const AVP_VENDOR: u8 = 0x80;
const AVP_MANDATORY: u8 = 0x40;
const AVP_USER_NAME: u32 = 1;
const AVP_USER_PASSWORD: u32 = 2;
const VENDOR_MICROSOFT: u32 = 311;
const AVP_MS_CHAP_CHALLENGE: u32 = 11;
const AVP_MS_CHAP2_RESPONSE: u32 = 25;
const AVP_MS_CHAP2_SUCCESS: u32 = 26;

/// One EAP packet. `body` starts with the Type on a Request or Response, and is empty on a
/// Success or Failure.
#[derive(Debug, Clone, PartialEq)]
pub struct EapPacket {
    pub code: u8,
    pub id: u8,
    pub body: Vec<u8>,
}

impl EapPacket {
    pub fn request(id: u8, kind: u8, data: &[u8]) -> Self {
        let mut body = Vec::with_capacity(data.len() + 1);
        body.push(kind);
        body.extend_from_slice(data);
        Self { code: CODE_REQUEST, id, body }
    }

    pub fn success(id: u8) -> Self {
        Self { code: CODE_SUCCESS, id, body: Vec::new() }
    }

    pub fn failure(id: u8) -> Self {
        Self { code: CODE_FAILURE, id, body: Vec::new() }
    }

    /// `None` when the bytes are not one well-formed EAP packet.
    pub fn parse(data: &[u8]) -> Option<Self> {
        let [code, id, length_high, length_low, body @ ..] = data else { return None };
        if usize::from(u16::from_be_bytes([*length_high, *length_low])) != data.len() {
            return None;
        }
        let fits = match *code {
            CODE_REQUEST | CODE_RESPONSE => !body.is_empty(),
            CODE_SUCCESS | CODE_FAILURE => body.is_empty(),
            _ => false,
        };
        fits.then(|| Self { code: *code, id: *id, body: body.to_vec() })
    }

    pub fn encode(&self) -> Vec<u8> {
        let length = (self.body.len() + 4) as u16;
        let mut packet = vec![self.code, self.id];
        packet.extend_from_slice(&length.to_be_bytes());
        packet.extend_from_slice(&self.body);
        packet
    }

    /// The Type of a Response, and what follows it.
    pub fn response(&self) -> Option<(u8, &[u8])> {
        match (self.code, self.body.split_first()) {
            (CODE_RESPONSE, Some((kind, data))) => Some((*kind, data)),
            _ => None,
        }
    }
}

/// The `EAP-Message` attributes of a request, in order, as one packet.
pub fn join_message<'a>(attributes: impl Iterator<Item = &'a [u8]>) -> Vec<u8> {
    attributes.flatten().copied().collect()
}

/// An EAP packet as RADIUS attributes.
pub fn split_message(packet: &[u8]) -> Vec<(u8, Vec<u8>)> {
    packet.chunks(MAX_ATTRIBUTE_VALUE).map(|chunk| (EAP_MESSAGE, chunk.to_vec())).collect()
}

/// One EAP-TLS message from the peer.
#[derive(Debug, PartialEq)]
pub struct TlsFragment<'a> {
    pub more: bool,
    pub total: Option<u32>,
    pub data: &'a [u8],
}

/// `None` when the flags promise a length that is not there.
pub fn parse_tls_fragment(data: &[u8]) -> Option<TlsFragment<'_>> {
    let (flags, rest) = data.split_first()?;
    let (total, data) = if flags & FLAG_LENGTH != 0 {
        let (total, rest) = rest.split_first_chunk::<4>()?;
        (Some(u32::from_be_bytes(*total)), rest)
    } else {
        (None, rest)
    };
    Some(TlsFragment { more: flags & FLAG_MORE != 0, total, data })
}

/// An EAP-TLS message to the peer, with the length on the first of several.
pub fn encode_tls_fragment(data: &[u8], total: Option<u32>, more: bool) -> Vec<u8> {
    let mut flags = if more { FLAG_MORE } else { 0 };
    let mut fragment = vec![0];
    if let Some(total) = total {
        flags |= FLAG_LENGTH;
        fragment.extend_from_slice(&total.to_be_bytes());
    }
    fragment[0] = flags;
    fragment.extend_from_slice(data);
    fragment
}

/// The EAP-TLS Start.
pub fn tls_start() -> Vec<u8> {
    vec![FLAG_START]
}

/// The TLS settings of EAP-TLS: a client certificate from `client_ca_file` is required, and
/// nothing is resumed unless an extension makes it so.
pub fn tls_config(cert_file: &Path, key_file: &Path, client_ca_file: &Path, crl_file: Option<&Path>) -> Result<Arc<rustls::ServerConfig>, String> {
    Ok(tls_config_and_verifier(cert_file, key_file, client_ca_file, crl_file)?.0)
}

/// `tls_config`, and what checks client certificates, for checking again on resumption.
pub fn tls_config_and_verifier(cert_file: &Path, key_file: &Path, client_ca_file: &Path, crl_file: Option<&Path>) -> Result<(Arc<rustls::ServerConfig>, Arc<dyn rustls::server::danger::ClientCertVerifier>), String> {
    let tls = crate::common::tls::server_tls_config(cert_file, key_file, client_ca_file, crl_file)?;
    let mut config = Arc::unwrap_or_clone(tls.config);
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    Ok((Arc::new(config), tls.verifier))
}

/// A resumed EAP-TLS session skipped checking the certificate: check the chain it was made with
/// again, as it would be checked now, so that a revoked or expired certificate does not come back in.
fn check_resumed(settings: &EapSettings, connection: &ServerConnection) -> Result<(), String> {
    if connection.handshake_kind() != Some(rustls::HandshakeKind::Resumed) {
        return Ok(());
    }
    let verifier = settings.tls_verifier.as_ref().ok_or("a resumed session and nothing to check its certificate with")?;
    let (end_entity, intermediates) = connection.peer_certificates().and_then(|chain| chain.split_first()).ok_or("a resumed session without a certificate")?;
    verifier.verify_client_cert(end_entity, intermediates, rustls::pki_types::UnixTime::now()).map_err(|e| format!("the certificate of a resumed session no longer passes: {e}"))?;
    Ok(())
}

/// The TLS settings of PEAP and TTLS: TLS 1.2 only, since what clients do with them over TLS 1.3
/// is not known here, and no client certificate, since the password inside is what proves who it is.
/// Nothing can be resumed. An error starts with the setting it is about.
pub fn tunnel_tls_config(cert_file: &Path, key_file: &Path) -> Result<Arc<rustls::ServerConfig>, String> {
    use rustls::pki_types::pem::PemObject;
    let certificates = CertificateDer::pem_file_iter(cert_file).and_then(|certificates| certificates.collect::<Result<Vec<_>, _>>()).map_err(|e| format!("cert_file '{}': {e}", cert_file.display()))?;
    let key = rustls::pki_types::PrivateKeyDer::from_pem_file(key_file).map_err(|e| format!("key_file '{}': {e}", key_file.display()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS12])
        .map_err(|e| format!("cert_file: cannot set up TLS: {e}"))?
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|e| format!("key_file '{}' does not fit cert_file '{}': {e}", key_file.display(), cert_file.display()))?;
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    Ok(Arc::new(config))
}

/// The most a TLS record holds, plus a little.
const INNER_READ_ROOM: usize = 16 * 1024 + 256;

/// The keys EAP-TLS derives from a finished handshake: RFC 5216 section 2.3 for TLS 1.2, where
/// the exporter's seed without a context is the one the RFC asks for, and RFC 9190 section 2.3
/// for TLS 1.3. The MSK is the first 64 octets.
pub fn eap_tls_msk(connection: &ServerConnection) -> Result<Zeroizing<[u8; 64]>, rustls::Error> {
    let mut material = Zeroizing::new([0u8; 128]);
    if connection.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_3) {
        connection.export_keying_material(&mut material[..], b"EXPORTER_EAP_TLS_Key_Material", Some(&[TYPE_TLS]))?;
    } else {
        connection.export_keying_material(&mut material[..], b"client EAP encryption", None)?;
    }
    let mut msk = Zeroizing::new([0u8; 64]);
    msk.copy_from_slice(&material[..64]);
    Ok(msk)
}

/// The keys of a method with a tunnel: `client EAP encryption` for PEAP as for EAP-TLS over TLS 1.2,
/// and `ttls keying material` for TTLS (RFC 5281 section 8). The MSK is the first 64 octets.
pub fn tunnel_msk(connection: &ServerConnection, method: u8) -> Result<Zeroizing<[u8; 64]>, rustls::Error> {
    if method != TYPE_TTLS {
        return eap_tls_msk(connection);
    }
    let mut msk = Zeroizing::new([0u8; 64]);
    connection.export_keying_material(&mut msk[..], b"ttls keying material", None)?;
    Ok(msk)
}

/// The largest TLS message either side may send, in all its fragments.
const MAX_TLS_MESSAGE: usize = 64 * 1024;

/// What one EAP-TLS message from the peer led to.
enum TlsStep {
    /// What to put in the next EAP-Request.
    Send(Vec<u8>),
    /// The handshake is over and the peer has everything.
    Done,
    Fail(String),
}

/// A TLS connection carried in EAP messages. EAP has one message each way at a time, so a flight
/// goes out one fragment for each acknowledgement, and the peer's comes in the same way.
struct TlsConversation {
    connection: ServerConnection,
    fragment_size: usize,
    incoming: Vec<u8>,
    outgoing: Vec<u8>,
    sent: usize,
    /// RFC 9190's last word from the server has been queued.
    committed: bool,
    /// The handshake failed, and what is left to send is the alert saying so.
    failed: Option<String>,
}

impl TlsConversation {
    fn new(config: Arc<rustls::ServerConfig>, fragment_size: usize) -> Result<Self, rustls::Error> {
        Ok(Self { connection: ServerConnection::new(config)?, fragment_size, incoming: Vec::new(), outgoing: Vec::new(), sent: 0, committed: false, failed: None })
    }

    /// Whether the peer has sent any TLS yet: until then it can still ask for another method.
    fn started(&self) -> bool {
        // Not only what it has sent: after the handshake, an empty Request leaves neither.
        !self.incoming.is_empty() || self.sent > 0 || !self.connection.is_handshaking()
    }

    fn receive(&mut self, data: &[u8]) -> TlsStep {
        let Some(fragment) = parse_tls_fragment(data) else {
            return TlsStep::Fail("a TLS message without its flags".to_string());
        };
        if fragment.total.is_some_and(|total| total as usize > MAX_TLS_MESSAGE) || self.incoming.len() + fragment.data.len() > MAX_TLS_MESSAGE {
            return TlsStep::Fail("a TLS message over the limit".to_string());
        }
        if fragment.data.is_empty() {
            // Announcing more with nothing in it would only buy the peer another round.
            return match fragment.more {
                true => TlsStep::Fail("an empty fragment that announces more".to_string()),
                false => self.acknowledged(),
            };
        }
        // Data answers a flight in place of the last acknowledgement, which is how a flight ends
        // when the peer has something to say. It cannot answer one that is not all out yet.
        if self.sent < self.outgoing.len() {
            return TlsStep::Fail("TLS data in the middle of a flight".to_string());
        }
        self.outgoing.clear();
        self.sent = 0;
        if let Some(reason) = self.failed.take() {
            return TlsStep::Fail(reason);
        }
        self.incoming.extend_from_slice(fragment.data);
        if fragment.more {
            return TlsStep::Send(encode_tls_fragment(&[], None, false));
        }
        let flight = std::mem::take(&mut self.incoming);
        if let Err(e) = self.take_flight(&flight) {
            // Tell the peer why, if rustls has an alert to send. Its acknowledgement ends this.
            self.failed = Some(e.to_string());
            self.collect_outgoing();
            return match self.outgoing.is_empty() {
                true => TlsStep::Fail(e.to_string()),
                false => TlsStep::Send(self.next_fragment()),
            };
        }
        self.collect_outgoing();
        if !self.connection.is_handshaking() && self.outgoing.is_empty() && !self.committed {
            self.commit();
        }
        match (self.outgoing.is_empty(), self.connection.is_handshaking()) {
            (false, _) => TlsStep::Send(self.next_fragment()),
            // Part of a record, say. Not left for the keys to fail on: that would be all that
            // stood between an unfinished handshake and a login.
            (true, true) => TlsStep::Fail("the peer stopped and the handshake did not finish".to_string()),
            (true, false) => TlsStep::Done,
        }
    }

    fn acknowledged(&mut self) -> TlsStep {
        if self.sent < self.outgoing.len() {
            return TlsStep::Send(self.next_fragment());
        }
        self.outgoing.clear();
        self.sent = 0;
        if let Some(reason) = self.failed.take() {
            return TlsStep::Fail(reason);
        }
        if self.connection.is_handshaking() {
            return TlsStep::Fail("an acknowledgement in the middle of the handshake".to_string());
        }
        if !self.committed {
            self.commit();
            if !self.outgoing.is_empty() {
                return TlsStep::Send(self.next_fragment());
            }
        }
        TlsStep::Done
    }

    /// RFC 9190 section 2.5: TLS 1.3 has no last flight of its own, so the server says it is done
    /// with one octet of application data. TLS 1.2 ended with the server's Finished.
    fn commit(&mut self) {
        self.committed = true;
        if self.connection.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_3) && self.connection.writer().write_all(&[0]).is_ok() {
            self.collect_outgoing();
        }
    }

    /// Puts `message` in the tunnel, and returns the first fragment of it.
    fn send(&mut self, message: &[u8]) -> std::io::Result<Vec<u8>> {
        self.connection.writer().write_all(message)?;
        self.collect_outgoing();
        Ok(self.next_fragment())
    }

    fn take_flight(&mut self, flight: &[u8]) -> Result<(), rustls::Error> {
        let mut reader = flight;
        while !reader.is_empty() {
            match self.connection.read_tls(&mut reader) {
                Ok(0) | Err(_) => return Err(rustls::Error::General("could not read the TLS data".to_string())),
                Ok(_) => {}
            }
            self.connection.process_new_packets()?;
        }
        Ok(())
    }

    fn collect_outgoing(&mut self) {
        while self.connection.wants_write() {
            if self.connection.write_tls(&mut self.outgoing).is_err() {
                break;
            }
        }
    }

    fn next_fragment(&mut self) -> Vec<u8> {
        let end = (self.sent + self.fragment_size).min(self.outgoing.len());
        let more = end < self.outgoing.len();
        // RFC 5216 section 2.1.5: the length goes on the first of several.
        let total = (self.sent == 0 && more).then_some(self.outgoing.len() as u32);
        let fragment = encode_tls_fragment(&self.outgoing[self.sent..end], total, more);
        self.sent = end;
        fragment
    }
}

/// Which of `offered` to use after a Legacy Nak asking for `asked`, in the peer's order.
fn choose_method(offered: &[u8], asked: &[u8]) -> Option<u8> {
    asked.iter().copied().find(|kind| offered.contains(kind))
}

/// What a chain said about the name inside a tunnel, and what goes with it.
pub struct InnerAccepted {
    pub account: String,
    pub attributes: Vec<(String, AttrValue)>,
}

/// A conversation that finished its method. Holds the keys, so it has no `Debug`.
pub struct Finished {
    pub method: u8,
    /// The Identifier for the EAP-Success or EAP-Failure.
    pub id: u8,
    pub identity: Vec<u8>,
    pub certificate: Option<CertificateDer<'static>>,
    /// The whole chain the peer sent, end-entity first, and the revocation check to ask about it.
    pub chain: Vec<CertificateDer<'static>>,
    pub revocation: Option<crate::revocation::Revocation>,
    pub msk: Zeroizing<[u8; 64]>,
    /// As when the conversation began.
    pub match_identity: bool,
    /// For PEAP and TTLS, where the decision was made inside the tunnel.
    pub inner: Option<InnerAccepted>,
    /// For EAP-TLS, whose decision is yet to come: a repeat of the last response waits on
    /// this, as with the other methods, and `settled` ends the conversation.
    holding: Option<(tokio::sync::OwnedMutexGuard<Session>, [u8; 16])>,
    /// Already counted when it was dropped for room or timed out while it waited: its result is
    /// recorded and answered, and not counted again.
    pub counted: bool,
}

/// Which conversation a failure ended, for the record and the counters.
pub struct Ended {
    pub method: u8,
    pub account: String,
    /// Counted as `overload` rather than `reject`.
    pub overload: bool,
    /// An overload for a peer over its budget, logged on a line of its own.
    pub over_budget: bool,
}

/// The name in a record of `method`.
pub fn handler_name(method: u8) -> &'static str {
    match method {
        TYPE_PEAP => "eap-peap",
        TYPE_TTLS => "eap-ttls",
        TYPE_TLS => "eap-tls",
        other => crate::eap_method::registered(other).map_or("eap-tls", |names| names.handler),
    }
}

/// The name scripts see as `eap_method` for a method whose peer proved itself without a password,
/// as EAP-AKA' does.
pub fn method_name(method: u8) -> &'static str {
    match method {
        TYPE_PEAP => "peap",
        TYPE_TTLS => "ttls",
        TYPE_TLS => "tls",
        other => crate::eap_method::registered(other).map_or("tls", |names| names.name),
    }
}

/// A password from inside a tunnel, for a chain to check. Holds the conversation, so that
/// a request repeated meanwhile waits for the answer rather than disturbing the tunnel.
pub struct PendingAuth {
    session: Option<tokio::sync::OwnedMutexGuard<Session>>,
    pub state: [u8; 16],
    id: u8,
    pub method: u8,
    pub name: String,
    pub secret: crate::auth_chain::Secret,
    /// What a method of an extension hands its `before_authorize`, before the chain is asked.
    pub proof: Option<MethodData>,
}

/// A method of an extension has to look something up before the conversation can go on. Holds
/// the conversation as `PendingAuth` does.
pub struct PendingLookup {
    session: Option<tokio::sync::OwnedMutexGuard<Session>>,
    state: [u8; 16],
    id: u8,
    pub method: Arc<dyn EapMethod>,
    pub request: MethodData,
}

/// What came of a `PendingAuth`.
pub enum InnerResult {
    /// `success` is `S=...` from the chain for MS-CHAPv2, which only comes of checking the password.
    Proved { success: Option<Vec<u8>>, account: String, attributes: Vec<(String, AttrValue)> },
    /// Written to the log and the record by the caller, there and then: the peer knows now, and
    /// need not say anything more for a failed guess to be on record.
    Refused { account: String },
}

/// What to do about one request.
pub enum Step {
    Challenge { packet: Vec<u8>, state: Option<[u8; 16]> },
    Finished(Box<Finished>),
    /// Needs a chain's word before the conversation can go on: see `EapService::authorized`.
    Authorize(Box<PendingAuth>),
    /// A method of an extension needs its lookup first: see `EapService::looked_up`.
    Lookup(Box<PendingLookup>),
    /// `ended`: the conversation this ended, if it belonged to one.
    Failed { packet: Vec<u8>, reason: String, ended: Option<Ended> },
    /// The end of a conversation already on record as refused.
    Closed { packet: Vec<u8> },
    /// No answer: its device has as many conversations as it may, and the NAS sends it again, or
    /// it repeats a request that ended the conversation while it waited.
    Discard,
}

/// Which device a conversation belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Owner {
    pub client: String,
    pub calling_station: Option<Vec<u8>>,
}

struct Session {
    identity: Vec<u8>,
    method: u8,
    /// `method`, for those who cannot wait for the lock on this.
    shared_method: Arc<AtomicU8>,
    /// Whether a refusal is on record: `Entry` reads it without the lock on this.
    shared_refused: Arc<AtomicBool>,
    /// What the method keeps from round to round.
    conversation: Conversation,
    /// What this client may use, which a Legacy Nak chooses from.
    allowed: Vec<u8>,
    /// The Identifier of the last Request sent, and of the Response before it.
    sent_id: u8,
    answered_id: u8,
    last_request: Vec<u8>,
    settings: Arc<EapSettings>,
    /// What waits for the cluster database, to be done again when the same Response comes again.
    retry: Option<Retry>,
}

/// A step that could not finish for want of the cluster database, and the EAP Identifier of the
/// Response it answers. Done again from here, the message is not read twice: a TLS record or an
/// AKA' check code would not survive that.
enum Retry {
    Authorize { id: u8, name: String, secret: crate::auth_chain::Secret, proof: Option<MethodData> },
    Lookup { id: u8, request: MethodData },
    Finished(Box<Finished>),
}

impl Retry {
    fn id(&self) -> u8 {
        match self {
            Retry::Authorize { id, .. } | Retry::Lookup { id, .. } => *id,
            Retry::Finished(finished) => finished.id,
        }
    }
}

/// The state of one method.
enum Conversation {
    /// EAP-TLS, or with what goes on inside the tunnel, PEAP and TTLS.
    Tls { tls: Box<TlsConversation>, inner: Option<Inner> },
    /// A method of the program's extensions.
    Method { method: Arc<dyn EapMethod>, conversation: Box<dyn MethodConversation> },
}

impl Conversation {
    /// A new one of `method`, or `None` when these settings do not have it.
    fn new(settings: &EapSettings, method: u8) -> Option<Self> {
        if let Some(method) = crate::eap_method::find(&settings.extra, method) {
            return Some(Conversation::Method { method: method.clone(), conversation: method.start() });
        }
        let config = match method {
            TYPE_TLS => settings.tls.clone()?,
            TYPE_PEAP => settings.peap_tls.clone().or_else(|| settings.tunnel_tls.clone())?,
            TYPE_TTLS => settings.tunnel_tls.clone()?,
            _ => return None,
        };
        let tls = Box::new(TlsConversation::new(config, settings.fragment_size).ok()?);
        let inner = match method {
            TYPE_PEAP => Some(Inner::Peap(Peap::new()?)),
            TYPE_TTLS => Some(Inner::Ttls(Ttls::new())),
            _ => None,
        };
        Some(Conversation::Tls { tls, inner })
    }

    /// Whether the peer has said anything of the method yet: until then it can still ask for
    /// another one.
    fn started(&self) -> bool {
        match self {
            Conversation::Tls { tls, .. } => tls.started(),
            Conversation::Method { conversation, .. } => conversation.started(),
        }
    }

    /// A chain refused, and that is on record.
    fn refused(&self) -> bool {
        match self {
            Conversation::Tls { inner, .. } => inner.as_ref().is_some_and(Inner::refused),
            Conversation::Method { conversation, .. } => conversation.refused(),
        }
    }

    /// Who the record says this is about.
    fn account(&self) -> String {
        match self {
            Conversation::Tls { inner, .. } => inner.as_ref().map(|inner| inner.account().to_string()).unwrap_or_default(),
            Conversation::Method { conversation, .. } => conversation.account(),
        }
    }

    /// What a Request opening this method holds.
    fn first_message(&mut self) -> Vec<u8> {
        match self {
            Conversation::Tls { .. } => tls_start(),
            Conversation::Method { conversation, .. } => conversation.first_message(),
        }
    }
}

impl Session {
    fn request(&mut self, answered: u8, data: &[u8]) -> Vec<u8> {
        self.answered_id = answered;
        self.sent_id = answered.wrapping_add(1);
        self.last_request = EapPacket::request(self.sent_id, self.method, data).encode();
        self.last_request.clone()
    }

    fn failed(&self, id: u8, reason: String) -> Step {
        if self.conversation.refused() {
            return Step::Closed { packet: EapPacket::failure(id).encode() };
        }
        let account = self.conversation.account();
        Step::Failed { packet: EapPacket::failure(id).encode(), reason, ended: Some(Ended { method: self.method, account, overload: false, over_budget: false }) }
    }

    /// Runs one Response through the conversation. May take a while: TLS signs here.
    fn step(&mut self, packet: &EapPacket, state: [u8; 16]) -> Step {
        // The NAS sent again what waited for the database: that step again, and nothing more.
        // Only for that Response: a stray one in between leaves it waiting.
        if let Some(retry) = self.retry.take_if(|retry| retry.id() == packet.id) {
            return match retry {
                Retry::Authorize { id, name, secret, proof } => Step::Authorize(Box::new(PendingAuth { session: None, state, id, method: self.method, name, secret, proof })),
                Retry::Lookup { id, request } => match &self.conversation {
                    Conversation::Method { method, .. } => Step::Lookup(Box::new(PendingLookup { session: None, state, id, method: method.clone(), request })),
                    _ => self.failed(id, "not this method".to_string()),
                },
                Retry::Finished(finished) => Step::Finished(finished),
            };
        }
        // The peer did not get the last Request, and is saying so with its last Response.
        if packet.id == self.answered_id {
            return Step::Challenge { packet: self.last_request.clone(), state: Some(state) };
        }
        // RFC 3748 section 4.1: discarded in silence. It can be an old copy that arrived late, and
        // the conversation goes on; what a peer holding the State can do with this is use up rounds.
        if packet.id != self.sent_id {
            tracing::debug!("EAP Identifier {} when {} was expected", packet.id, self.sent_id);
            return Step::Discard;
        }
        let Some((kind, data)) = packet.response() else {
            return self.failed(packet.id, "not an EAP Response".to_string());
        };
        if kind == TYPE_NAK && !self.conversation.started() {
            let Some(method) = choose_method(&self.allowed, data) else {
                return self.failed(packet.id, "the peer asked only for methods this server does not offer".to_string());
            };
            let Some(conversation) = Conversation::new(&self.settings, method) else {
                return self.failed(packet.id, "cannot start the method the peer asked for".to_string());
            };
            self.method = method;
            self.shared_method.store(method, Ordering::Relaxed);
            self.conversation = conversation;
            let packet = self.open_method(packet.id);
            return Step::Challenge { packet, state: Some(state) };
        }
        if kind != self.method {
            return self.failed(packet.id, format!("EAP type {kind} in a conversation of type {}", self.method));
        }
        match self.conversation {
            Conversation::Tls { .. } => self.tls_step(packet, data, state),
            Conversation::Method { .. } => self.method_step(packet, data, state),
        }
    }

    /// The Request that opens the method.
    fn open_method(&mut self, answered: u8) -> Vec<u8> {
        let message = self.conversation.first_message();
        let packet = self.request(answered, &message);
        match &mut self.conversation {
            Conversation::Method { conversation, .. } => conversation.note_request(&packet),
            Conversation::Tls { .. } => {}
        }
        packet
    }

    fn method_step(&mut self, packet: &EapPacket, data: &[u8], state: [u8; 16]) -> Step {
        let Conversation::Method { method, conversation } = &mut self.conversation else {
            return self.failed(packet.id, "not this method".to_string());
        };
        let encoded = packet.encode();
        match conversation.receive(data, &encoded) {
            MethodStep::Lookup(request) => Step::Lookup(Box::new(PendingLookup { session: None, state, id: packet.id, method: method.clone(), request })),
            MethodStep::Authorize { name, proof } => Step::Authorize(Box::new(PendingAuth { session: None, state, id: packet.id, method: self.method, name, secret: crate::auth_chain::Secret::Certificate, proof })),
            MethodStep::Failed(reason) => self.failed(packet.id, reason),
        }
    }

    /// The next Request of a method of an extension, from what its lookup found.
    fn method_answer(&mut self, id: u8, state: [u8; 16], found: Result<MethodData, LookupError>) -> Step {
        let found = match found {
            Ok(found) => found,
            Err(LookupError::Failed(reason) | LookupError::Unavailable(reason)) => return self.failed(id, reason),
            Err(LookupError::OverBudget(reason)) => return self.overloaded(id, reason, true),
            Err(LookupError::Busy(reason)) => return self.overloaded(id, reason, false),
        };
        let Conversation::Method { conversation, .. } = &mut self.conversation else {
            return self.failed(id, "not this method".to_string());
        };
        match conversation.answer(id.wrapping_add(1), found) {
            Ok(message) => {
                let packet = self.request(id, &message);
                Step::Challenge { packet, state: Some(state) }
            }
            Err(reason) => self.failed(id, reason),
        }
    }

    /// Fails as an overload, for a peer `over_budget` or for want of room.
    fn overloaded(&self, id: u8, reason: &str, over_budget: bool) -> Step {
        let mut step = self.failed(id, reason.to_string());
        if let Step::Failed { ended: Some(ended), .. } = &mut step {
            ended.overload = true;
            ended.over_budget = over_budget;
        }
        step
    }

    fn tls_step(&mut self, packet: &EapPacket, data: &[u8], state: [u8; 16]) -> Step {
        let Conversation::Tls { tls, inner } = &mut self.conversation else {
            return self.failed(packet.id, "not this method".to_string());
        };
        let tunnelled = inner.is_some();
        // These keep their version in the low bits of the flags, and only version 0 is offered.
        if tunnelled && data.first().is_some_and(|flags| flags & FLAG_VERSION != 0) {
            return self.failed(packet.id, "a version of the method other than 0".to_string());
        }
        match tls.receive(data) {
            TlsStep::Send(fragment) => {
                let packet = self.request(packet.id, &fragment);
                Step::Challenge { packet, state: Some(state) }
            }
            TlsStep::Fail(reason) => self.failed(packet.id, reason),
            TlsStep::Done if tunnelled => self.tunnel(packet.id, state),
            TlsStep::Done => match check_resumed(&self.settings, &tls.connection).and_then(|()| eap_tls_msk(&tls.connection).map_err(|e| format!("no keys from the handshake: {e}"))) {
                Ok(msk) => {
                    let chain: Vec<CertificateDer<'static>> = tls.connection.peer_certificates().unwrap_or_default().iter().map(|certificate| certificate.clone().into_owned()).collect();
                    Step::Finished(Box::new(Finished {
                        method: self.method,
                        id: packet.id,
                        identity: self.identity.clone(),
                        certificate: chain.first().cloned(),
                        chain,
                        revocation: self.settings.revocation.clone(),
                        msk,
                        match_identity: self.settings.match_identity,
                        inner: None,
                        holding: None,
                        counted: false,
                    }))
                }
                Err(reason) => self.failed(packet.id, reason),
            },
        }
    }

    /// The handshake is over and everything is across: the next move inside the tunnel.
    fn tunnel(&mut self, id: u8, state: [u8; 16]) -> Step {
        let Conversation::Tls { tls, inner } = &mut self.conversation else {
            return self.failed(id, "not this method".to_string());
        };
        // An inner password is in here. Room for a whole record first: with little room the reader
        // goes through a buffer of its own, and growing would leave part of it behind.
        let mut heard = Zeroizing::new(Vec::with_capacity(INNER_READ_ROOM));
        match tls.connection.reader().read_to_end(&mut heard) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return self.failed(id, format!("cannot read inside the tunnel: {e}")),
        }
        let step = match inner.as_mut() {
            Some(Inner::Peap(peap)) => peap.receive(&heard),
            Some(Inner::Ttls(ttls)) => ttls.receive(&heard, &tls.connection),
            None => InnerStep::Refused("no inner conversation".to_string()),
        };
        self.after_inner(id, state, step)
    }

    fn after_inner(&mut self, id: u8, state: [u8; 16], step: InnerStep) -> Step {
        let method = self.method;
        match step {
            InnerStep::Send(message) => self.send_inner(id, &message, state),
            InnerStep::Authorize { name, secret } => Step::Authorize(Box::new(PendingAuth { session: None, state, id, method, name, secret, proof: None })),
            InnerStep::Refused(reason) => self.failed(id, reason),
            InnerStep::Accepted(accepted) => {
                let msk = match &self.conversation {
                    Conversation::Tls { tls, .. } => tunnel_msk(&tls.connection, method).map_err(|e| e.to_string()),
                    Conversation::Method { conversation, .. } => conversation.msk().ok_or_else(|| "none yet".to_string()),
                };
                match msk {
                    Ok(msk) => Step::Finished(Box::new(Finished {
                        method,
                        id,
                        identity: self.identity.clone(),
                        certificate: None,
                        chain: Vec::new(),
                        revocation: None,
                        msk,
                        match_identity: self.settings.match_identity,
                        inner: Some(accepted),
                        holding: None,
                        counted: false,
                    })),
                    Err(e) => self.failed(id, format!("no keys from the tunnel: {e}")),
                }
            }
        }
    }

    fn send_inner(&mut self, id: u8, message: &[u8], state: [u8; 16]) -> Step {
        let Conversation::Tls { tls, .. } = &mut self.conversation else {
            return self.failed(id, "not this method".to_string());
        };
        match tls.send(message) {
            Ok(fragment) => {
                let packet = self.request(id, &fragment);
                Step::Challenge { packet, state: Some(state) }
            }
            Err(e) => self.failed(id, format!("cannot write inside the tunnel: {e}")),
        }
    }

    fn authorized(&mut self, id: u8, state: [u8; 16], result: InnerResult) -> Step {
        let step = match &mut self.conversation {
            Conversation::Tls { inner: Some(Inner::Peap(peap)), .. } => InnerStep::Send(peap.authorized(result)),
            Conversation::Tls { inner: Some(Inner::Ttls(ttls)), .. } => ttls.authorized(result),
            Conversation::Tls { inner: None, .. } => InnerStep::Refused("no inner conversation".to_string()),
            Conversation::Method { conversation, .. } => match result {
                InnerResult::Proved { account, attributes, .. } => InnerStep::Accepted(InnerAccepted { account, attributes }),
                InnerResult::Refused { account } => InnerStep::Refused(conversation.chain_refused(account)),
            },
        };
        self.shared_refused.store(self.conversation.refused(), Ordering::Relaxed);
        self.after_inner(id, state, step)
    }
}

/// What goes on inside a tunnel.
enum Inner {
    Peap(Peap),
    Ttls(Ttls),
}

impl Inner {
    /// A chain refused the password, and that is on record.
    fn refused(&self) -> bool {
        match self {
            Inner::Peap(peap) => peap.refused,
            Inner::Ttls(ttls) => ttls.refused,
        }
    }

    fn account(&self) -> &str {
        match self {
            Inner::Peap(peap) => &peap.account,
            Inner::Ttls(ttls) => &ttls.account,
        }
    }
}

/// What one message inside a tunnel led to.
enum InnerStep {
    Send(Vec<u8>),
    Authorize { name: String, secret: crate::auth_chain::Secret },
    Accepted(InnerAccepted),
    Refused(String),
}

enum InnerState {
    /// The tunnel just opened, and the peer has nothing to say yet.
    Opening,
    AskedIdentity,
    Challenged,
    /// The peer's MS-CHAPv2 Success or Failure acknowledgement.
    Told { accepted: bool },
    /// The peer's answer to the EAP-TLV Result.
    Resulted { accepted: bool },
}

/// PEAPv0 with inner EAP-MSCHAPv2. Inside the tunnel, EAP goes without its four-octet header,
/// except for EAP-TLV, which keeps it.
struct Peap {
    state: InnerState,
    rounds: u8,
    identifier: u8,
    challenge: [u8; 16],
    /// Who the record says this is about. Empty until the peer gives a name.
    account: String,
    attributes: Vec<(String, AttrValue)>,
    /// A chain refused the password, and that is on record.
    refused: bool,
}

impl Peap {
    fn new() -> Option<Self> {
        let mut challenge = [0u8; 16];
        getrandom::fill(&mut challenge).ok()?;
        Some(Self { state: InnerState::Opening, rounds: 0, identifier: challenge[0], challenge, account: String::new(), attributes: Vec::new(), refused: false })
    }

    fn receive(&mut self, message: &[u8]) -> InnerStep {
        if matches!(self.state, InnerState::Opening) {
            if !message.is_empty() {
                return InnerStep::Refused("inner data before the server's first message".to_string());
            }
            self.state = InnerState::AskedIdentity;
            return InnerStep::Send(vec![TYPE_IDENTITY]);
        }
        if message.is_empty() {
            return InnerStep::Refused("nothing inside the tunnel".to_string());
        }
        self.rounds += 1;
        if self.rounds > MAX_INNER_ROUNDS {
            return InnerStep::Refused("too many inner rounds".to_string());
        }
        match self.state {
            InnerState::Opening => InnerStep::Refused("inner data out of turn".to_string()),
            InnerState::AskedIdentity if message[0] == TYPE_IDENTITY => {
                self.state = InnerState::Challenged;
                InnerStep::Send(self.mschap_challenge())
            }
            InnerState::AskedIdentity => InnerStep::Refused("no inner Identity".to_string()),
            // It did not get the challenge, as far as it can say.
            InnerState::Challenged if message[0] == TYPE_IDENTITY => InnerStep::Send(self.mschap_challenge()),
            InnerState::Challenged => self.mschap_response(message),
            InnerState::Told { accepted } => {
                if *message != [TYPE_MSCHAPV2, if accepted { MSCHAP_SUCCESS } else { MSCHAP_FAILURE }] {
                    return InnerStep::Refused("no acknowledgement of the MS-CHAPv2 result".to_string());
                }
                self.state = InnerState::Resulted { accepted };
                let status = if accepted { TLV_SUCCESS } else { TLV_FAILURE };
                let data = [TLV_RESULT[0], TLV_RESULT[1], 0, 2, 0, status];
                InnerStep::Send(EapPacket::request(self.identifier.wrapping_add(1), TYPE_TLV, &data).encode())
            }
            InnerState::Resulted { accepted } => match (parse_tlv_result(message, self.identifier.wrapping_add(1)), accepted) {
                (Some(TLV_SUCCESS), true) => InnerStep::Accepted(InnerAccepted { account: self.account.clone(), attributes: std::mem::take(&mut self.attributes) }),
                (Some(_), false) => InnerStep::Refused("the inner authentication failed".to_string()),
                _ => InnerStep::Refused("no agreement on the EAP-TLV Result".to_string()),
            },
        }
    }

    fn mschap_challenge(&self) -> Vec<u8> {
        let length = 4 + 1 + self.challenge.len() + SERVER_NAME.len();
        let mut message = vec![TYPE_MSCHAPV2, MSCHAP_CHALLENGE, self.identifier, (length >> 8) as u8, length as u8, self.challenge.len() as u8];
        message.extend_from_slice(&self.challenge);
        message.extend_from_slice(SERVER_NAME);
        message
    }

    fn mschap_response(&mut self, message: &[u8]) -> InnerStep {
        let Some(response) = parse_mschapv2_response(message).filter(|response| response.id == self.identifier) else {
            return InnerStep::Refused("no MS-CHAPv2 Response for this challenge".to_string());
        };
        let Some(name) = std::str::from_utf8(response.name).ok().filter(|name| !name.is_empty()) else {
            return InnerStep::Refused("the inner name is not text".to_string());
        };
        // As in TTLS: RADIUS could not carry more, and the name goes in the record.
        if name.len() > MAX_INNER_NAME {
            return InnerStep::Refused(format!("an inner name over {MAX_INNER_NAME} octets"));
        }
        self.account = name.to_string();
        // `authorized` says which it is, and the lock on the session keeps anything else out until then.
        self.state = InnerState::Told { accepted: false };
        InnerStep::Authorize {
            name: name.to_string(),
            secret: crate::auth_chain::Secret::MsChapV2 { ident: self.identifier, peer_challenge: response.peer_challenge, response: response.response, challenge: self.challenge },
        }
    }

    /// The MS-CHAPv2 Success or Failure to send for what the chain said.
    fn authorized(&mut self, result: InnerResult) -> Vec<u8> {
        let (code, text) = match result {
            InnerResult::Proved { success, account, attributes } => {
                self.state = InnerState::Told { accepted: true };
                self.account = account;
                self.attributes = attributes;
                (MSCHAP_SUCCESS, success.unwrap_or_default())
            }
            InnerResult::Refused { account } => {
                self.state = InnerState::Told { accepted: false };
                self.refused = true;
                self.account = account;
                let challenge: String = self.challenge.iter().map(|byte| format!("{byte:02X}")).collect();
                (MSCHAP_FAILURE, format!("E=691 R=0 C={challenge} V=3 M=Authentication failed").into_bytes())
            }
        };
        let length = 4 + text.len();
        let mut message = vec![TYPE_MSCHAPV2, code, self.identifier, (length >> 8) as u8, length as u8];
        message.extend_from_slice(&text);
        message
    }
}

/// One AVP inside a TTLS tunnel (RFC 5281 section 10.1).
#[derive(Debug, PartialEq)]
pub struct Avp<'a> {
    pub code: u32,
    pub vendor: Option<u32>,
    pub mandatory: bool,
    pub data: &'a [u8],
}

/// The AVPs in `data`, or `None` when a length does not fit.
pub fn parse_avps(mut data: &[u8]) -> Option<Vec<Avp<'_>>> {
    let mut avps = Vec::new();
    while !data.is_empty() {
        let (header, _) = data.split_first_chunk::<8>()?;
        let code = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let flags = header[4];
        let length = u32::from_be_bytes([0, header[5], header[6], header[7]]) as usize;
        let vendor = flags & AVP_VENDOR != 0;
        let fixed = if vendor { 12 } else { 8 };
        if length < fixed || length > data.len() {
            return None;
        }
        let vendor = vendor.then(|| data[8..12].try_into().map(u32::from_be_bytes)).transpose().ok()?;
        avps.push(Avp { code, vendor, mandatory: flags & AVP_MANDATORY != 0, data: &data[fixed..length] });
        // The last one's padding has nothing after it to keep apart from.
        data = data.get(length.next_multiple_of(4)..).unwrap_or_default();
    }
    Some(avps)
}

/// EAP-TTLSv0's tunnel, with inner PAP or MS-CHAPv2. The peer speaks first, and PAP needs one
/// message. MS-CHAPv2 also needs the peer's acknowledgement of the server's proof.
struct Ttls {
    state: TtlsState,
    rounds: u8,
    ident: u8,
    /// Who the record says this is about. Empty until the peer gives a name.
    account: String,
    attributes: Vec<(String, AttrValue)>,
    /// A chain refused the password, and that is on record.
    refused: bool,
}

enum TtlsState {
    /// The peer's AVPs are due.
    Opening,
    /// With a chain.
    Deciding,
    /// The peer has `MS-CHAP2-Success`, and its acknowledgement is due.
    Proved,
}

impl Ttls {
    fn new() -> Self {
        Self { state: TtlsState::Opening, rounds: 0, ident: 0, account: String::new(), attributes: Vec::new(), refused: false }
    }

    fn receive(&mut self, message: &[u8], connection: &ServerConnection) -> InnerStep {
        self.rounds += 1;
        if self.rounds > MAX_INNER_ROUNDS {
            return InnerStep::Refused("too many inner rounds".to_string());
        }
        match self.state {
            // It may only acknowledge the handshake: its turn again, then.
            TtlsState::Opening if message.is_empty() => InnerStep::Send(Vec::new()),
            TtlsState::Opening => self.opening(message, connection),
            TtlsState::Proved if message.is_empty() => InnerStep::Accepted(InnerAccepted { account: self.account.clone(), attributes: std::mem::take(&mut self.attributes) }),
            TtlsState::Proved => InnerStep::Refused("something other than an acknowledgement of MS-CHAP2-Success".to_string()),
            TtlsState::Deciding => InnerStep::Refused("inner data out of turn".to_string()),
        }
    }

    fn opening(&mut self, message: &[u8], connection: &ServerConnection) -> InnerStep {
        let Some(avps) = parse_avps(message) else {
            return InnerStep::Refused("an inner AVP whose length does not fit".to_string());
        };
        let (mut name, mut password, mut challenge, mut response) = (None, None, None, None);
        for avp in &avps {
            match (avp.vendor, avp.code) {
                (None, AVP_USER_NAME) => name = Some(avp.data),
                (None, AVP_USER_PASSWORD) => password = Some(avp.data),
                (Some(VENDOR_MICROSOFT), AVP_MS_CHAP_CHALLENGE) => challenge = Some(avp.data),
                (Some(VENDOR_MICROSOFT), AVP_MS_CHAP2_RESPONSE) => response = Some(avp.data),
                _ if avp.mandatory => return InnerStep::Refused("an unrecognized mandatory inner AVP".to_string()),
                _ => {}
            }
        }
        let Some(name) = name.and_then(|name| std::str::from_utf8(name).ok()).filter(|name| !name.is_empty()) else {
            return InnerStep::Refused("no inner name".to_string());
        };
        // RADIUS could not carry more, and the tunnel could: the work of some password hashes grows
        // with the length, and the name goes in the record.
        if name.len() > MAX_INNER_NAME {
            return InnerStep::Refused(format!("an inner name over {MAX_INNER_NAME} octets"));
        }
        self.account = name.to_string();
        let secret = match (password, challenge, response) {
            (Some(password), None, None) => {
                // Padded with NULs up to a multiple of 16 octets.
                let end = password.iter().rposition(|byte| *byte != 0).map_or(0, |last| last + 1);
                if end > MAX_INNER_PASSWORD {
                    return InnerStep::Refused(format!("an inner password over {MAX_INNER_PASSWORD} octets"));
                }
                crate::auth_chain::Secret::pap(password[..end].to_vec())
            }
            (None, Some(challenge), Some(response)) => match self.mschap_secret(challenge, response, connection) {
                Ok(secret) => secret,
                Err(reason) => return InnerStep::Refused(reason.to_string()),
            },
            _ => return InnerStep::Refused("neither PAP nor MS-CHAPv2 inside the tunnel".to_string()),
        };
        self.state = TtlsState::Deciding;
        InnerStep::Authorize { name: name.to_string(), secret }
    }

    /// RFC 5281 section 11.2.4: the challenge is what both ends export, or a response to it could
    /// have been relayed from somewhere else. It is not for the peer to choose.
    fn mschap_secret(&mut self, challenge: &[u8], response: &[u8], connection: &ServerConnection) -> Result<crate::auth_chain::Secret, &'static str> {
        let mut exported = [0u8; 17];
        connection.export_keying_material(&mut exported[..], b"ttls challenge", None).map_err(|_| "no challenge from the tunnel")?;
        let [ident, _flags, rest @ ..] = response else {
            return Err("an MS-CHAP2-Response that is too short");
        };
        if challenge != &exported[..16] || *ident != exported[16] {
            return Err("an MS-CHAP challenge other than the one the tunnel exports");
        }
        let (Ok(peer_challenge), Ok(nt_response)) = (<[u8; 16]>::try_from(rest.get(..16).unwrap_or_default()), <[u8; 24]>::try_from(rest.get(24..48).unwrap_or_default())) else {
            return Err("an MS-CHAP2-Response that is too short");
        };
        self.ident = *ident;
        Ok(crate::auth_chain::Secret::MsChapV2 { ident: *ident, peer_challenge, response: nt_response, challenge: exported[..16].try_into().map_err(|_| "no challenge from the tunnel")? })
    }

    fn authorized(&mut self, result: InnerResult) -> InnerStep {
        match result {
            InnerResult::Proved { success: Some(success), account, attributes } => {
                self.state = TtlsState::Proved;
                self.account = account;
                self.attributes = attributes;
                InnerStep::Send(encode_avp(AVP_MS_CHAP2_SUCCESS, Some(VENDOR_MICROSOFT), &[&[self.ident], success.as_slice()].concat()))
            }
            InnerResult::Proved { success: None, account, attributes } => InnerStep::Accepted(InnerAccepted { account, attributes }),
            InnerResult::Refused { account } => {
                self.refused = true;
                self.account = account;
                InnerStep::Refused("the inner authentication failed".to_string())
            }
        }
    }
}

/// A mandatory AVP, padded.
fn encode_avp(code: u32, vendor: Option<u32>, data: &[u8]) -> Vec<u8> {
    let length = 8 + vendor.map_or(0, |_| 4) + data.len();
    let flags = AVP_MANDATORY | if vendor.is_some() { AVP_VENDOR } else { 0 };
    let mut message = code.to_be_bytes().to_vec();
    message.push(flags);
    message.extend_from_slice(&(length as u32).to_be_bytes()[1..]);
    if let Some(vendor) = vendor {
        message.extend_from_slice(&vendor.to_be_bytes());
    }
    message.extend_from_slice(data);
    message.resize(length.next_multiple_of(4), 0);
    message
}

/// An inner EAP-MSCHAPv2 Response.
#[derive(Debug, PartialEq)]
pub struct MschapResponse<'a> {
    pub id: u8,
    pub peer_challenge: [u8; 16],
    pub response: [u8; 24],
    pub name: &'a [u8],
}

/// RFC 2759 section 4 inside EAP, with EAP's header left out as PEAPv0 does. The fixed part is
/// 16 octets of peer challenge, 8 reserved, 24 of response, and one of flags.
pub fn parse_mschapv2_response(message: &[u8]) -> Option<MschapResponse<'_>> {
    let (header, rest) = message.split_first_chunk::<6>()?;
    let [kind, op, id, high, low, size] = *header;
    if kind != TYPE_MSCHAPV2 || op != MSCHAP_RESPONSE || size != 49 || usize::from(u16::from_be_bytes([high, low])) != message.len() - 1 {
        return None;
    }
    let (value, name) = rest.split_first_chunk::<49>()?;
    Some(MschapResponse { id, peer_challenge: value[..16].try_into().ok()?, response: value[24..48].try_into().ok()?, name })
}

/// The status of an EAP-TLV Response holding a Result for `id`, in a whole EAP packet.
pub fn parse_tlv_result(message: &[u8], id: u8) -> Option<u8> {
    let packet = EapPacket::parse(message)?;
    if packet.code != CODE_RESPONSE || packet.id != id {
        return None;
    }
    let (TYPE_TLV, data) = packet.response()? else { return None };
    // The top bits say mandatory or not.
    let [high, low, 0, 2, 0, status] = *data else { return None };
    (u16::from_be_bytes([high, low]) & 0x3fff == 3).then_some(status)
}

struct Entry {
    owner: Owner,
    started: Instant,
    last_seen: Instant,
    /// `session_timeout_ms` when it began.
    timeout: Duration,
    /// Where it stands in `Sessions`' order of who goes first.
    heard: (bool, u64),
    /// Requests so far, the one that opened it included.
    rounds: u32,
    method: Arc<AtomicU8>,
    refused: Arc<AtomicBool>,
    session: Arc<tokio::sync::Mutex<Session>>,
}

impl Entry {
    fn method(&self) -> u8 {
        self.method.load(Ordering::Relaxed)
    }

    /// Whether this conversation's refusal is on record, so that its end adds nothing.
    fn refused(&self) -> bool {
        self.refused.load(Ordering::Relaxed)
    }

    fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.last_seen) >= self.timeout || now.duration_since(self.started) >= self.timeout * LIFETIME_TIMEOUTS
    }
}

/// A conversation's place in the order of who goes first when there is no room: those with nothing
/// since their Identity, which cost a sender one packet each, then whoever was last heard from
/// longest ago. A counter rather than a time, so that no two are the same.
type LastHeard = (bool, u64, [u8; 16]);

/// The conversations under way, and for the whole table and each client, which goes first.
/// Opening one and each round take no scan of the table.
#[derive(Default)]
struct Sessions {
    entries: HashMap<[u8; 16], Entry>,
    clock: u64,
    quiet_first: BTreeSet<LastHeard>,
    per_client: HashMap<String, BTreeSet<LastHeard>>,
    /// Only those with a `Calling-Station-Id`: without one, requests cannot be told apart.
    per_device: HashMap<Owner, Vec<[u8; 16]>>,
}

impl Sessions {
    fn insert(&mut self, state: [u8; 16], mut entry: Entry) {
        self.clock += 1;
        entry.heard = (false, self.clock);
        self.quiet_first.insert((false, self.clock, state));
        self.per_client.entry(entry.owner.client.clone()).or_default().insert((false, self.clock, state));
        if entry.owner.calling_station.is_some() {
            self.per_device.entry(entry.owner.clone()).or_default().push(state);
        }
        self.entries.insert(state, entry);
    }

    /// Removes a conversation, and says its method only if it has not been counted yet: one
    /// already on record as refused has its result.
    fn remove_uncounted(&mut self, state: &[u8; 16]) -> Option<u8> {
        let refused = self.entries.get(state).is_some_and(|entry| entry.refused());
        self.remove(state).filter(|_| !refused)
    }

    fn remove(&mut self, state: &[u8; 16]) -> Option<u8> {
        let entry = self.entries.remove(state)?;
        let (went_on, clock) = entry.heard;
        self.quiet_first.remove(&(went_on, clock, *state));
        if let Some(sessions) = self.per_client.get_mut(&entry.owner.client) {
            sessions.remove(&(went_on, clock, *state));
            if sessions.is_empty() {
                self.per_client.remove(&entry.owner.client);
            }
        }
        if let Some(states) = self.per_device.get_mut(&entry.owner) {
            states.retain(|other| other != state);
            if states.is_empty() {
                self.per_device.remove(&entry.owner);
            }
        }
        Some(entry.method())
    }

    /// Notes a request for `state`, which then goes to the back of the line for being dropped.
    fn heard(&mut self, state: &[u8; 16]) {
        let Some(entry) = self.entries.get_mut(state) else {
            return;
        };
        self.clock += 1;
        let (went_on, clock) = std::mem::replace(&mut entry.heard, (true, self.clock));
        self.quiet_first.remove(&(went_on, clock, *state));
        self.quiet_first.insert((true, self.clock, *state));
        if let Some(sessions) = self.per_client.get_mut(&entry.owner.client) {
            sessions.remove(&(went_on, clock, *state));
            sessions.insert((true, self.clock, *state));
        }
    }

    /// Makes room for a conversation of `owner`, or says there is none. None of those here has
    /// proved anything yet, so refusing new ones would let anyone keep everyone out. Those with
    /// nothing since their Identity go first, and of those or the rest, the quietest: the peer
    /// gave up on those, and those under way are heard from every round.
    fn admit(&mut self, owner: &Owner, settings: &EapSettings, method: u8, metrics: &Metrics) -> bool {
        // Its own device left these at the EAP Identity: it has started over.
        let abandoned: Vec<[u8; 16]> = self.per_device.get(owner).into_iter().flatten().filter(|state| self.entries.get(*state).is_some_and(|entry| entry.rounds == 1)).copied().collect();
        for state in &abandoned {
            if let Some(method) = self.remove(state) {
                metrics.eap(method).replaced.inc();
            }
        }
        // Only that device is affected, so that device waits.
        let admitted = self.per_device.get(owner).map_or(0, Vec::len) < settings.max_sessions_per_device;
        if admitted {
            while let Some(&(_, _, quietest)) = self.per_client.get(&owner.client).filter(|sessions| sessions.len() >= settings.max_sessions_per_client).and_then(BTreeSet::first) {
                if let Some(method) = self.remove_uncounted(&quietest) {
                    metrics.eap(method).evicted.inc();
                }
            }
            while self.entries.len() >= settings.max_sessions {
                let Some(&(_, _, quietest)) = self.quiet_first.first() else { break };
                if let Some(method) = self.remove_uncounted(&quietest) {
                    metrics.eap(method).evicted.inc();
                }
            }
        } else {
            metrics.eap(method).overload.inc();
        }
        metrics.eap_open.set(self.entries.len() as u64);
        admitted
    }
}

/// The conversations under way on this node, and the settings new ones start with.
pub struct EapService {
    settings: Live<Option<Arc<EapSettings>>>,
    sessions: Mutex<Sessions>,
}

impl Default for EapService {
    fn default() -> Self {
        Self { settings: Live::new(None), sessions: Mutex::default() }
    }
}

impl EapService {
    /// Applies to new conversations. Those under way go on with what they began with.
    pub fn set_settings(&self, settings: Option<Arc<EapSettings>>) {
        for method in settings.iter().flat_map(|settings| settings.extra.iter()) {
            crate::eap_method::register(method.names());
        }
        self.settings.set(settings);
    }

    pub fn settings(&self) -> Option<Arc<EapSettings>> {
        self.settings.get().as_ref().clone()
    }

    /// Whether `state` is a conversation under way, which goes on even if EAP is no longer
    /// configured: what it would get as an ordinary request is an answer with no proof behind it.
    pub fn has_conversation(&self, state: &[u8]) -> bool {
        <[u8; 16]>::try_from(state).is_ok_and(|state| self.sessions.lock().entries.contains_key(&state))
    }

    /// Drops the conversations that went quiet or have lasted too long. Runs on a timer, and
    /// each request looks after its own, so that a full table is not scanned per request.
    pub fn sweep(&self, metrics: &Metrics) {
        let now = Instant::now();
        for method in self.settings().iter().flat_map(|settings| settings.extra.iter()) {
            method.sweep();
        }
        let mut sessions = self.sessions.lock();
        let expired: Vec<[u8; 16]> = sessions.entries.iter().filter(|(_, entry)| entry.expired(now)).map(|(state, _)| *state).collect();
        for state in &expired {
            if let Some(method) = sessions.remove_uncounted(state) {
                metrics.eap(method).timeout.inc();
            }
        }
        metrics.eap_open.set(sessions.entries.len() as u64);
    }

    /// Removes the conversation: whether it was still there, and not already counted when it was
    /// dropped for room or timed out.
    fn forget(&self, state: &[u8; 16], metrics: &Metrics) -> bool {
        let mut sessions = self.sessions.lock();
        let open = sessions.remove(state).is_some();
        metrics.eap_open.set(sessions.entries.len() as u64);
        open
    }

    /// Counts a refusal of the inner password, unless the conversation was counted when it was
    /// dropped for room or timed out while the chain decided. Under one lock with that, and marked
    /// as refused, so that neither counts it again.
    pub fn count_refusal(&self, state: &[u8; 16], method: u8, metrics: &Metrics) {
        let sessions = self.sessions.lock();
        if let Some(entry) = sessions.entries.get(state).filter(|entry| !entry.refused()) {
            entry.refused.store(true, Ordering::Relaxed);
            metrics.eap(method).reject.inc();
        }
    }

    /// Handles the joined `EAP-Message` of one request.
    pub async fn handle(&self, settings: Option<Arc<EapSettings>>, owner: Owner, allowed: Option<&[u8]>, message: &[u8], state: Option<&[u8]>, metrics: &Metrics) -> Step {
        // RFC 3579 section 2.6.1: an empty one asks the server to begin.
        if message.is_empty() && state.is_none() {
            return Step::Challenge { packet: EapPacket::request(0, TYPE_IDENTITY, &[]).encode(), state: None };
        }
        let state: Option<Result<[u8; 16], ()>> = state.map(|state| state.try_into().map_err(|_| ()));
        let Some(packet) = EapPacket::parse(message) else {
            let mut ended = None;
            if let Some(Ok(state)) = &state {
                let mut sessions = self.sessions.lock();
                // Under one lock: it could be over by the time a second one is held.
                if let Some(entry) = sessions.entries.get(state).filter(|entry| entry.owner == owner) {
                    let (method, refused) = (entry.method(), entry.refused());
                    sessions.remove(state);
                    metrics.eap_open.set(sessions.entries.len() as u64);
                    if !refused {
                        metrics.eap(method).reject.inc();
                        ended = Some(Ended { method, account: String::new(), overload: false, over_budget: false });
                    }
                }
            }
            return Step::Failed { packet: EapPacket::failure(message.get(1).copied().unwrap_or(0)).encode(), reason: "a malformed EAP packet".to_string(), ended };
        };
        match state {
            None => match settings {
                Some(settings) => self.start(settings, owner, allowed, &packet, metrics),
                None => Step::Failed { packet: EapPacket::failure(packet.id).encode(), reason: "EAP is not configured".to_string(), ended: None },
            },
            Some(Err(())) => Step::Failed { packet: EapPacket::failure(packet.id).encode(), reason: "a State this server did not send".to_string(), ended: None },
            Some(Ok(state)) => self.go_on(owner, packet, state, metrics).await,
        }
    }

    fn start(&self, settings: Arc<EapSettings>, owner: Owner, allowed: Option<&[u8]>, packet: &EapPacket, metrics: &Metrics) -> Step {
        let failed = |reason: &str| Step::Failed { packet: EapPacket::failure(packet.id).encode(), reason: reason.to_string(), ended: None };
        let Some((TYPE_IDENTITY, identity)) = packet.response() else {
            return failed("a conversation has to begin with an Identity");
        };
        let allowed = allowed.unwrap_or(&settings.methods).to_vec();
        let Some(&method) = allowed.first() else {
            return failed("this client has no EAP methods");
        };
        if !self.sessions.lock().admit(&owner, &settings, method, metrics) {
            return Step::Discard;
        }
        let Some(conversation) = Conversation::new(&settings, method) else {
            return failed("cannot start the method");
        };
        let mut state = [0u8; 16];
        if getrandom::fill(&mut state).is_err() {
            return failed("no random State");
        }
        let shared_method = Arc::new(AtomicU8::new(method));
        let shared_refused = Arc::new(AtomicBool::new(false));
        let mut session = Session { identity: identity.to_vec(), method, shared_method: shared_method.clone(), shared_refused: shared_refused.clone(), conversation, allowed, sent_id: 0, answered_id: packet.id, last_request: Vec::new(), settings, retry: None };
        let request = session.open_method(packet.id);
        let timeout = session.settings.session_timeout;
        let settings = session.settings.clone();
        let mut sessions = self.sessions.lock();
        // Others may have opened theirs since the check above, which saved building this one.
        if !sessions.admit(&owner, &settings, method, metrics) {
            return Step::Discard;
        }
        let now = Instant::now();
        sessions.insert(state, Entry { owner, started: now, last_seen: now, timeout, heard: (false, 0), rounds: 1, method: shared_method, refused: shared_refused, session: Arc::new(tokio::sync::Mutex::new(session)) });
        metrics.eap_open.set(sessions.entries.len() as u64);
        Step::Challenge { packet: request, state: Some(state) }
    }

    async fn go_on(&self, owner: Owner, packet: EapPacket, state: [u8; 16], metrics: &Metrics) -> Step {
        let failed = |reason: &str| Step::Failed { packet: EapPacket::failure(packet.id).encode(), reason: reason.to_string(), ended: None };
        let session = {
            let mut sessions = self.sessions.lock();
            let session = match sessions.entries.get_mut(&state) {
                None => return failed("a State no conversation on this node has"),
                // Not the owner's: it stays usable by them, and this one learns nothing.
                Some(entry) if entry.owner != owner => return failed("a State no conversation on this node has"),
                Some(entry) if entry.expired(Instant::now()) => {
                    let counted = sessions.remove_uncounted(&state);
                    metrics.eap_open.set(sessions.entries.len() as u64);
                    if let Some(method) = counted {
                        metrics.eap(method).timeout.inc();
                    }
                    return failed("a State no conversation on this node has");
                }
                Some(entry) if entry.rounds >= MAX_ROUNDS => {
                    let method = entry.method();
                    let refused = entry.refused();
                    sessions.remove(&state);
                    metrics.eap_open.set(sessions.entries.len() as u64);
                    let ended = (!refused).then(|| {
                        metrics.eap(method).reject.inc();
                        Ended { method, account: String::new(), overload: false, over_budget: false }
                    });
                    return Step::Failed { packet: EapPacket::failure(packet.id).encode(), reason: "too many rounds".to_string(), ended };
                }
                Some(entry) => {
                    entry.rounds += 1;
                    entry.last_seen = Instant::now();
                    entry.session.clone()
                }
            };
            sessions.heard(&state);
            session
        };
        let mut session = session.lock_owned().await;
        // It waited behind another request, and that one ended the conversation. Going on would
        // feed the tunnel a record it has already had; the answer to it has gone out already.
        if !self.sessions.lock().entries.contains_key(&state) {
            return Step::Discard;
        }
        let response = packet.clone();
        let method = session.method;
        let stepped = tokio::task::spawn_blocking(move || {
            let step = session.step(&response, state);
            (session, step)
        })
        .await;
        let Ok((session, step)) = stepped else {
            self.forget(&state, metrics);
            metrics.eap(method).reject.inc();
            return Step::Failed { packet: EapPacket::failure(packet.id).encode(), reason: "the conversation failed".to_string(), ended: Some(Ended { method, account: String::new(), overload: false, over_budget: false }) };
        };
        match step {
            Step::Authorize(mut pending) => {
                pending.session = Some(session);
                Step::Authorize(pending)
            }
            Step::Lookup(mut pending) => {
                pending.session = Some(session);
                Step::Lookup(pending)
            }
            Step::Finished(mut finished) if finished.inner.is_none() => {
                finished.holding = Some((session, state));
                Step::Finished(finished)
            }
            step => self.after_step(step, state, metrics),
        }
    }

    /// Whether the conversation of `state` is still in the table: one dropped for room or timed
    /// out while it waited on the chain or the database was counted then.
    pub fn is_open(&self, state: &[u8; 16]) -> bool {
        self.sessions.lock().entries.contains_key(state)
    }

    /// Forgets the conversation if `step` ended it, and counts how, unless it was counted when
    /// it left the table.
    fn after_step(&self, mut step: Step, state: [u8; 16], metrics: &Metrics) -> Step {
        match &mut step {
            Step::Finished(finished) => {
                finished.counted |= !self.forget(&state, metrics);
            }
            Step::Closed { .. } => {
                self.forget(&state, metrics);
            }
            Step::Failed { ended, .. } => {
                let open = self.forget(&state, metrics);
                if let Some(ended) = ended.as_ref().filter(|_| open) {
                    let counters = metrics.eap(ended.method);
                    if ended.overload { counters.overload.inc() } else { counters.reject.inc() }
                }
            }
            Step::Challenge { .. } | Step::Authorize(_) | Step::Lookup(_) | Step::Discard => {}
        }
        step
    }

    /// Ends a conversation whose next request cannot be sent. Unless it is already on record as
    /// refused, counts it as a rejection and says what to record.
    pub async fn abandon(&self, state: &[u8; 16], metrics: &Metrics) -> Option<Ended> {
        let (method, session) = {
            let mut sessions = self.sessions.lock();
            // Under one lock: it could be over by the time a second one is held.
            let entry = sessions.entries.get(state)?;
            let found = (!entry.refused()).then(|| (entry.method(), entry.session.clone()));
            sessions.remove(state);
            metrics.eap_open.set(sessions.entries.len() as u64);
            found?
        };
        metrics.eap(method).reject.inc();
        let account = session.lock().await.conversation.account();
        Some(Ended { method, account, overload: false, over_budget: false })
    }

    /// Ends a conversation that was held while the chain decided on its certificate, noting in
    /// `counted` whether it was counted when it left the table meanwhile.
    pub fn settled(&self, finished: &mut Finished, metrics: &Metrics) {
        if let Some((session, state)) = finished.holding.take() {
            finished.counted |= !self.forget(&state, metrics);
            drop(session);
        }
    }

    /// Goes on with the conversation behind `pending`, with what its method's lookup found.
    pub fn looked_up(&self, pending: PendingLookup, found: Result<MethodData, LookupError>, metrics: &Metrics) -> Step {
        let PendingLookup { session, state, id, .. } = pending;
        let Some(mut session) = session else {
            return Step::Failed { packet: EapPacket::failure(id).encode(), reason: "the conversation is gone".to_string(), ended: None };
        };
        let step = session.method_answer(id, state, found);
        self.after_step(step, state, metrics)
    }

    /// Leaves the conversation behind `pending` open, without an answer: the cluster database
    /// could not be used for its lookup, and the same Response coming again tries again.
    pub fn lookup_waits(&self, pending: PendingLookup) -> Step {
        let PendingLookup { session, id, request, .. } = pending;
        if let Some(mut session) = session {
            session.retry = Some(Retry::Lookup { id, request });
        }
        Step::Discard
    }

    /// As `lookup_waits`, for a chain that could not decide for want of the cluster database.
    pub fn authorization_waits(&self, pending: PendingAuth) -> Step {
        let PendingAuth { session, id, name, secret, proof, .. } = pending;
        if let Some(mut session) = session {
            session.retry = Some(Retry::Authorize { id, name, secret, proof });
        }
        Step::Discard
    }

    /// As `lookup_waits`, for an EAP-TLS certificate the chain could not decide about.
    pub fn finish_waits(&self, mut finished: Box<Finished>) {
        if let Some((mut session, _)) = finished.holding.take() {
            session.retry = Some(Retry::Finished(finished));
        }
    }

    /// Goes on with the conversation behind `pending`, now that a chain has said.
    pub fn authorized(&self, pending: PendingAuth, result: InnerResult, metrics: &Metrics) -> Step {
        let PendingAuth { session, state, id, .. } = pending;
        let Some(mut session) = session else {
            return Step::Failed { packet: EapPacket::failure(id).encode(), reason: "the conversation is gone".to_string(), ended: None };
        };
        let step = session.authorized(id, state, result);
        self.after_step(step, state, metrics)
    }
}

/// A certificate's subject, for scripts.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CertificateSubject {
    /// As text, with each value escaped: see `subject_text`.
    pub text: String,
    /// Each attribute's name and value, unescaped, in the certificate's order.
    pub attrs: Vec<(String, String)>,
}

/// The text of a value of a subject, or `None` if it holds none. All the string types of X.509,
/// not only those `as_str` takes: a certificate may well have its names in any of them.
fn attribute_text(value: &x509_parser::der_parser::asn1_rs::Any<'_>) -> Option<String> {
    use x509_parser::der_parser::asn1_rs::Tag;
    match value.tag() {
        Tag::NumericString | Tag::VisibleString | Tag::PrintableString | Tag::GeneralString | Tag::ObjectDescriptor | Tag::GraphicString | Tag::T61String | Tag::VideotexString | Tag::Utf8String | Tag::Ia5String => {
            std::str::from_utf8(value.data).ok().map(str::to_string)
        }
        Tag::BmpString => {
            let (units, rest) = value.data.as_chunks::<2>();
            if !rest.is_empty() {
                return None;
            }
            char::decode_utf16(units.iter().map(|unit| u16::from_be_bytes(*unit))).collect::<Result<String, _>>().ok()
        }
        _ => None,
    }
}

/// RFC 4514 section 2.4: a value that is not text, as `#` and the hex of all its DER.
fn attribute_der_hex(value: &x509_parser::der_parser::asn1_rs::Any<'_>) -> String {
    use x509_parser::der_parser::asn1_rs::ToDer;
    let der = value.to_der_vec().unwrap_or_else(|_| value.data.to_vec());
    let hex: String = der.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("#{hex}")
}

/// RFC 4514 section 2.4.
fn escape_rdn_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    let last = value.chars().count().saturating_sub(1);
    for (position, c) in value.chars().enumerate() {
        match c {
            '"' | '+' | ',' | ';' | '<' | '>' | '\\' => {
                escaped.push('\\');
                escaped.push(c);
            }
            '#' if position == 0 => escaped.push_str("\\#"),
            ' ' if position == 0 || position == last => escaped.push_str("\\ "),
            c if c.is_ascii_control() => escaped.push_str(&format!("\\{:02X}", c as u8)),
            c => escaped.push(c),
        }
    }
    escaped
}

/// The subject of a certificate in DER, if it can be read.
pub fn certificate_subject(der: &[u8]) -> Option<CertificateSubject> {
    let (_, parsed) = x509_parser::parse_x509_certificate(der).ok()?;
    Some(read_subject(parsed.subject()))
}

/// The subject of a parsed certificate. A value that is not text comes as `#` and its hex, as RFC
/// 4514 has it.
fn read_subject(subject: &x509_parser::x509::X509Name<'_>) -> CertificateSubject {
    let mut rdns: Vec<Rdn> = Vec::new();
    let mut attrs = Vec::new();
    for rdn in subject.iter() {
        let mut escaped = Vec::new();
        for attr in rdn.iter() {
            let name = x509_parser::objects::oid2abbrev(attr.attr_type(), x509_parser::objects::oid_registry()).map_or_else(|_| attr.attr_type().to_id_string(), str::to_string);
            let (text, shown) = match attribute_text(attr.attr_value()) {
                Some(value) => {
                    let shown = escape_rdn_value(&value);
                    (value, shown)
                }
                None => {
                    let hex = attribute_der_hex(attr.attr_value());
                    (hex.clone(), hex)
                }
            };
            escaped.push((name.clone(), shown));
            attrs.push((name, text));
        }
        rdns.push(escaped);
    }
    CertificateSubject { text: subject_text(&rdns), attrs }
}

/// One RDN of a subject: its attributes, usually one.
type Rdn = Vec<(String, String)>;

/// A subject as text, in the certificate's order, with `, ` between RDNs as before.
fn subject_text(rdns: &[Rdn]) -> String {
    rdns.iter().map(|rdn| rdn.iter().map(|(name, value)| format!("{name}={value}")).collect::<Vec<_>>().join(" + ")).collect::<Vec<_>>().join(", ")
}

/// The name a certificate logs in as, spelled as in the certificate, and its subject for scripts.
/// Its names are its DNS and email names, or its common name when it has neither: a CA that checks
/// alternative names may let the requester pick the common name. With `match_identity` it is the
/// one the EAP Identity fits, without regard to case, and without it the first of them.
pub fn certificate_name(certificate: &CertificateDer<'_>, identity: &[u8], match_identity: bool) -> Result<(String, CertificateSubject), String> {
    let (_, parsed) = X509Certificate::from_der(certificate.as_ref()).map_err(|e| format!("cannot read the certificate: {e}"))?;
    let mut names: Vec<String> = Vec::new();
    // Names that cannot be read are not no names: the common name may say anything.
    let alternatives = parsed.subject_alternative_name().map_err(|e| format!("cannot read the certificate's alternative names: {e}"))?;
    if let Some(alternatives) = alternatives {
        for name in &alternatives.value.general_names {
            match name {
                GeneralName::DNSName(name) | GeneralName::RFC822Name(name) => names.push(name.to_string()),
                // What the parser could not read could be a name, and the TLS library lets it through.
                GeneralName::Invalid(..) => return Err("cannot read the certificate's alternative names: one is not text".to_string()),
                _ => {}
            }
        }
    }
    if names.is_empty() {
        names.extend(parsed.subject().iter_common_name().filter_map(|name| name.as_str().ok()).map(str::to_string));
    }
    let subject = read_subject(parsed.subject());
    if !match_identity {
        let name = names.into_iter().next().ok_or("the certificate has no name to log in as")?;
        return Ok((name, subject));
    }
    let identity = std::str::from_utf8(identity).map_err(|_| "the EAP Identity is not text")?;
    match names.into_iter().find(|name| name.eq_ignore_ascii_case(identity)) {
        Some(name) => Ok((name, subject)),
        // Not the Identity itself: it comes from outside and can hold control characters.
        None => Err("the EAP Identity does not fit the certificate".to_string()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) use crate::test_support::eap::{client_connection, eap_settings, identity_response, Inner, Opening, Peer};
    use crate::test_support::certs;
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{PrivateKeyDer, ServerName};
    use rustls::{ClientConfig, ClientConnection, Connection, ProtocolVersion, RootCertStore};
    use std::time::Duration;

    fn certificates(dir: &Path) -> Arc<rustls::ServerConfig> {
        let issuer = certs::ca(dir, "ca");
        certs::leaf(dir, "server", &issuer);
        certs::named_leaf(dir, "alice", "alice.example.com", 20, &issuer);
        tls_config(&dir.join("server.pem"), &dir.join("server.key"), &dir.join("ca.pem"), None).unwrap()
    }

    /// Moves whatever `from` wants to send to `to`, through a buffer, as EAP would carry it.
    fn deliver(from: &mut Connection, to: &mut Connection) -> usize {
        let mut flight = Vec::new();
        while from.wants_write() {
            from.write_tls(&mut flight).unwrap();
        }
        let mut reader = flight.as_slice();
        while !reader.is_empty() {
            to.read_tls(&mut reader).unwrap();
            to.process_new_packets().unwrap();
        }
        flight.len()
    }

    #[test]
    fn test_handshake_on_buffers_and_keys_agree() {
        let dir = tempfile::tempdir().unwrap();
        let config = certificates(dir.path());
        for (version, label, context) in [
            (&rustls::version::TLS12, &b"client EAP encryption"[..], None),
            (&rustls::version::TLS13, &b"EXPORTER_EAP_TLS_Key_Material"[..], Some(&[TYPE_TLS][..])),
        ] {
            let mut client = Connection::Client(client_connection(dir.path(), Some("alice"), version));
            let mut server = Connection::Server(ServerConnection::new(config.clone()).unwrap());
            while deliver(&mut client, &mut server) + deliver(&mut server, &mut client) > 0 {}
            let Connection::Server(server) = server else { unreachable!() };
            assert!(!server.is_handshaking());
            assert!(server.peer_certificates().is_some());
            assert_eq!(server.protocol_version(), Some(version.version));
            let msk = eap_tls_msk(&server).unwrap();
            let mut theirs = [0u8; 128];
            let Connection::Client(client) = client else { unreachable!() };
            client.export_keying_material(&mut theirs[..], label, context).unwrap();
            assert_eq!(msk[..], theirs[..64], "{:?}", version.version);
            assert_ne!(*msk, [0u8; 64]);
        }
    }

    /// Two handshakes of one client against `server`: the second resumes the first's session.
    fn resume_twice(dir: &Path, server: &Arc<rustls::ServerConfig>) -> Result<ServerConnection, String> {
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from_pem_file(dir.join("ca.pem")).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        let client = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS12])
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_client_auth_cert(vec![CertificateDer::from_pem_file(dir.join("alice.pem")).map_err(|e| e.to_string())?], PrivateKeyDer::from_pem_file(dir.join("alice.key")).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        let client = Arc::new(client);
        let mut last = None;
        for _ in 0..2 {
            let name = ServerName::try_from("localhost").map_err(|e| e.to_string())?;
            let mut ours = Connection::Client(ClientConnection::new(client.clone(), name).map_err(|e| e.to_string())?);
            let mut theirs = Connection::Server(ServerConnection::new(server.clone()).map_err(|e| e.to_string())?);
            while deliver(&mut ours, &mut theirs) + deliver(&mut theirs, &mut ours) > 0 {}
            last = Some(theirs);
        }
        match last {
            Some(Connection::Server(server)) => Ok(server),
            _ => Err("no server connection".to_string()),
        }
    }

    #[test]
    fn test_resumed_session_checks_the_certificate_again() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let issuer = certs::ca(dir.path(), "ca");
        certs::leaf(dir.path(), "server", &issuer);
        certs::named_leaf(dir.path(), "alice", "alice.example.com", 20, &issuer);
        let (config, verifier) = tls_config_and_verifier(&dir.path().join("server.pem"), &dir.path().join("server.key"), &dir.path().join("ca.pem"), None)?;
        assert!(!config.session_storage.can_cache(), "nothing resumed unless asked");
        let mut resuming = rustls::ServerConfig::clone(&config);
        resuming.session_storage = rustls::server::ServerSessionMemoryCache::new(16);
        let resuming = Arc::new(resuming);
        let server = resume_twice(dir.path(), &resuming)?;
        assert_eq!(server.handshake_kind(), Some(rustls::HandshakeKind::Resumed));
        assert!(server.peer_certificates().is_some(), "the session keeps the certificate");
        let settings = EapSettings { tls_verifier: Some(verifier), ..eap_settings(dir.path(), None) };
        check_resumed(&settings, &server)?;
        // Revoked since: the resumed session is refused.
        certs::crl(dir.path(), "ca.crl", &[20], &issuer);
        let (_, revoking) = tls_config_and_verifier(&dir.path().join("server.pem"), &dir.path().join("server.key"), &dir.path().join("ca.pem"), Some(&dir.path().join("ca.crl")))?;
        let settings = EapSettings { tls_verifier: Some(revoking), ..eap_settings(dir.path(), None) };
        let error = check_resumed(&settings, &server).err().ok_or("a revoked certificate resumed")?;
        assert!(error.contains("no longer passes"), "{error}");
        // A full handshake needs no second check, and a resumed one needs a verifier.
        let full = resume_twice(dir.path(), &config)?;
        assert_eq!(full.handshake_kind(), Some(rustls::HandshakeKind::Full));
        check_resumed(&EapSettings { tls_verifier: None, ..eap_settings(dir.path(), None) }, &full)?;
        assert!(check_resumed(&EapSettings { tls_verifier: None, ..eap_settings(dir.path(), None) }, &server).is_err());
        Ok(())
    }

    #[test]
    fn test_nothing_to_resume() {
        let dir = tempfile::tempdir().unwrap();
        let config = certificates(dir.path());
        assert_eq!(config.send_tls13_tickets, 0);
        assert!(!config.session_storage.can_cache());
        // TLS 1.3 hands out no ticket: the last flight from the server is empty.
        let mut client = Connection::Client(client_connection(dir.path(), Some("alice"), &rustls::version::TLS13));
        let mut server = Connection::Server(ServerConnection::new(config).unwrap());
        deliver(&mut client, &mut server);
        deliver(&mut server, &mut client);
        deliver(&mut client, &mut server);
        assert_eq!(deliver(&mut server, &mut client), 0);
        let Connection::Server(server) = &server else { unreachable!() };
        assert_eq!(server.protocol_version(), Some(ProtocolVersion::TLSv1_3));
    }

    #[test]
    fn test_packet_round_trip() {
        let packet = EapPacket::request(7, TYPE_TLS, &[FLAG_START]);
        let bytes = packet.encode();
        assert_eq!(bytes, [CODE_REQUEST, 7, 0, 6, TYPE_TLS, FLAG_START]);
        assert_eq!(EapPacket::parse(&bytes), Some(packet));
        assert_eq!(EapPacket::success(9).encode(), [CODE_SUCCESS, 9, 0, 4]);
        assert_eq!(EapPacket::failure(9).encode(), [CODE_FAILURE, 9, 0, 4]);
        assert_eq!(EapPacket::parse(&EapPacket::failure(9).encode()), Some(EapPacket::failure(9)));
    }

    #[test]
    fn test_response_type_and_data() {
        let response = EapPacket::parse(&[CODE_RESPONSE, 3, 0, 7, TYPE_IDENTITY, b'a', b'b']).unwrap();
        assert_eq!(response.response(), Some((TYPE_IDENTITY, &b"ab"[..])));
        assert_eq!(EapPacket::request(1, TYPE_TLS, &[]).response(), None, "not a response");
    }

    #[test]
    fn test_malformed_packets() {
        for bad in [
            &[][..],
            &[CODE_RESPONSE, 1, 0][..],                  // no room for the length
            &[CODE_RESPONSE, 1, 0, 9, TYPE_TLS][..],     // says longer than it is
            &[CODE_RESPONSE, 1, 0, 5, TYPE_TLS, 0][..],  // says shorter than it is
            &[CODE_RESPONSE, 1, 0, 4][..],               // a response needs a type
            &[CODE_SUCCESS, 1, 0, 5, 0][..],             // a success has nothing after the length
            &[9, 1, 0, 5, 0][..],                        // no such code
        ] {
            assert_eq!(EapPacket::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn test_split_and_join() {
        let packet: Vec<u8> = (0..600u32).map(|n| n as u8).collect();
        let attributes = split_message(&packet);
        assert_eq!(attributes.iter().map(|(_, value)| value.len()).collect::<Vec<_>>(), [253, 253, 94]);
        assert!(attributes.iter().all(|(kind, _)| *kind == EAP_MESSAGE));
        assert_eq!(join_message(attributes.iter().map(|(_, value)| value.as_slice())), packet);
        assert_eq!(split_message(&packet[..253]).len(), 1, "exactly one attribute's worth");
    }

    #[test]
    fn test_tls_fragments() {
        assert_eq!(encode_tls_fragment(b"ab", None, false), [0, b'a', b'b']);
        let first = encode_tls_fragment(b"ab", Some(5), true);
        assert_eq!(first, [FLAG_LENGTH | FLAG_MORE, 0, 0, 0, 5, b'a', b'b']);
        assert_eq!(parse_tls_fragment(&first), Some(TlsFragment { more: true, total: Some(5), data: b"ab" }));
        assert_eq!(parse_tls_fragment(&[0]), Some(TlsFragment { more: false, total: None, data: b"" }), "an acknowledgement");
        assert_eq!(parse_tls_fragment(&[]), None, "no flags");
        assert_eq!(parse_tls_fragment(&[FLAG_LENGTH, 0, 0]), None, "the length is cut off");
        assert_eq!(tls_start(), [FLAG_START]);
    }

    /// A peer's side of PEAP's inner EAP-MSCHAPv2, noting what it hears in `log`. It checks that
    /// the server's Success proves the server knows the password, as a real peer would.
    pub(crate) fn mschap_inner(name: &str, password: &str, log: Arc<Mutex<Vec<String>>>) -> Inner {
        let (name, password) = (name.to_string(), password.to_string());
        let peer_challenge = [7u8; 16];
        let mut response = [0u8; 24];
        let mut challenge = [0u8; 16];
        Box::new(move |message: &[u8]| {
            if message.len() >= 5 && message[4] == TYPE_TLV && usize::from(u16::from_be_bytes([message[2], message[3]])) == message.len() {
                let status = message[message.len() - 1];
                log.lock().push(format!("tlv {status}"));
                return EapPacket { code: CODE_RESPONSE, id: message[1], body: message[4..].to_vec() }.encode();
            }
            match (message[0], message.get(1).copied()) {
                (TYPE_IDENTITY, _) => [&[TYPE_IDENTITY], name.as_bytes()].concat(),
                (TYPE_MSCHAPV2, Some(1)) => {
                    challenge.copy_from_slice(&message[6..22]);
                    response = crate::mschap::response_for(&password, &name, &peer_challenge, &challenge);
                    let length = 4 + 1 + 49 + name.len();
                    let mut reply = vec![TYPE_MSCHAPV2, 2, message[2], (length >> 8) as u8, length as u8, 49];
                    reply.extend_from_slice(&peer_challenge);
                    reply.extend_from_slice(&[0; 8]);
                    reply.extend_from_slice(&response);
                    reply.push(0);
                    reply.extend_from_slice(name.as_bytes());
                    reply
                }
                (TYPE_MSCHAPV2, Some(3)) => {
                    let text = String::from_utf8_lossy(&message[5..]).into_owned();
                    let proof = crate::mschap::verify(&password, &name, &peer_challenge, &challenge, &response).unwrap();
                    assert!(text.starts_with(&proof.authenticator_response), "{text}");
                    log.lock().push("success".to_string());
                    vec![TYPE_MSCHAPV2, 3]
                }
                (TYPE_MSCHAPV2, Some(4)) => {
                    log.lock().push(String::from_utf8_lossy(&message[5..]).into_owned());
                    vec![TYPE_MSCHAPV2, 4]
                }
                other => panic!("no answer to {other:?}"),
            }
        })
    }

    /// A peer of PEAP, with no certificate and TLS 1.2.
    pub(crate) fn peap_peer(dir: &Path, inner: Inner) -> Peer {
        let mut peer = Peer::new(client_connection(dir, None, &rustls::version::TLS12));
        peer.method = TYPE_PEAP;
        peer.inner = Some(inner);
        peer
    }

    /// One AVP with its padding.
    pub(crate) fn avp(code: u32, vendor: Option<u32>, mandatory: bool, data: &[u8]) -> Vec<u8> {
        let length = 8 + vendor.map_or(0, |_| 4) + data.len();
        let flags = if vendor.is_some() { 0x80u8 } else { 0 } | if mandatory { 0x40 } else { 0 };
        let mut message = code.to_be_bytes().to_vec();
        message.push(flags);
        message.extend_from_slice(&(length as u32).to_be_bytes()[1..]);
        if let Some(vendor) = vendor {
            message.extend_from_slice(&vendor.to_be_bytes());
        }
        message.extend_from_slice(data);
        message.resize(message.len() + (4 - length % 4) % 4, 0);
        message
    }

    /// A peer of TTLS, with no certificate and TLS 1.2, and what it says in the tunnel.
    pub(crate) fn ttls_peer(dir: &Path, opening: Opening, inner: Inner) -> Peer {
        let mut peer = Peer::new(client_connection(dir, None, &rustls::version::TLS12));
        peer.method = TYPE_TTLS;
        peer.opening = Some(opening);
        peer.inner = Some(inner);
        peer
    }

    /// What a peer says in TTLS-PAP, with the password padded as peers do.
    pub(crate) fn pap_avps(name: Option<&str>, password: &str) -> Vec<u8> {
        let mut padded = password.as_bytes().to_vec();
        padded.resize(padded.len().div_ceil(16).max(1) * 16, 0);
        let mut message = name.map_or(Vec::new(), |name| avp(1, None, true, name.as_bytes()));
        message.extend(avp(2, None, true, &padded));
        message
    }

    /// What a peer says in TTLS-MSCHAPv2, over `challenge` and its ident.
    pub(crate) fn mschap_avps(name: &str, password: &str, challenge: &[u8; 16], ident: u8) -> Vec<u8> {
        let peer_challenge = [7u8; 16];
        let response = crate::mschap::response_for(password, name, &peer_challenge, challenge);
        let mut value = vec![ident, 0];
        value.extend_from_slice(&peer_challenge);
        value.extend_from_slice(&[0; 8]);
        value.extend_from_slice(&response);
        let mut message = avp(1, None, true, name.as_bytes());
        message.extend(avp(11, Some(311), true, challenge));
        message.extend(avp(25, Some(311), true, &value));
        message
    }

    /// A TTLS peer of MS-CHAPv2 over the exported challenge, noting what it hears in `log`.
    pub(crate) fn ttls_mschap_peer(dir: &Path, name: &str, password: &str, log: Arc<Mutex<Vec<Vec<u8>>>>) -> Peer {
        let (name, password) = (name.to_string(), password.to_string());
        let opening: Opening = Box::new(move |client| {
            let mut exported = [0u8; 17];
            client.export_keying_material(&mut exported[..], b"ttls challenge", None).unwrap();
            mschap_avps(&name, &password, exported[..16].try_into().unwrap(), exported[16])
        });
        let inner: Inner = Box::new(move |message: &[u8]| {
            log.lock().push(message.to_vec());
            Vec::new()
        });
        ttls_peer(dir, opening, inner)
    }

    pub(crate) fn ttls_settings(dir: &Path) -> EapSettings {
        EapSettings { methods: vec![TYPE_TTLS], ..peap_settings(dir) }
    }

    pub(crate) fn peap_settings(dir: &Path) -> EapSettings {
        EapSettings { methods: vec![TYPE_PEAP], tls: None, tunnel_tls: Some(tunnel_tls_config(&dir.join("server.pem"), &dir.join("server.key")).unwrap()), ..eap_settings(dir, None) }
    }

    fn device(client: &str, station: Option<&[u8]>) -> Owner {
        Owner { client: client.to_string(), calling_station: station.map(<[u8]>::to_vec) }
    }

    fn owner() -> Owner {
        Owner { client: "nas".to_string(), calling_station: Some(b"aa-bb".to_vec()) }
    }

    struct Rig {
        dir: tempfile::TempDir,
        service: EapService,
        settings: Arc<EapSettings>,
        metrics: Metrics,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let issuer = certs::ca(dir.path(), "ca");
        certs::leaf(dir.path(), "server", &issuer);
        certs::named_leaf(dir.path(), "alice", "alice.example.com", 20, &issuer);
        certs::crl(dir.path(), "alice-revoked.crl", &[20], &issuer);
        let other = certs::ca(dir.path(), "other-ca");
        certs::named_leaf(dir.path(), "stranger", "alice.example.com", 21, &other);
        let settings = Arc::new(eap_settings(dir.path(), None));
        Rig { dir, service: EapService::default(), settings, metrics: Metrics::default() }
    }

    impl Rig {
        async fn send_as(&self, owner: Owner, message: &[u8], state: Option<&[u8]>) -> Step {
            self.service.handle(Some(self.settings.clone()), owner, None, message, state, &self.metrics).await
        }

        /// Starts a conversation for `owner` and takes it past the EAP Identity.
        async fn past_identity(&self, owner: &Owner) -> [u8; 16] {
            let Step::Challenge { packet, state } = self.send_as(owner.clone(), &identity_response(1, b"a"), None).await else { panic!("not opened") };
            let state = state.unwrap();
            let hello = self.peer(Some("alice"), &rustls::version::TLS12).answer(&packet);
            assert!(matches!(self.send_as(owner.clone(), &hello, Some(&state)).await, Step::Challenge { .. }));
            state
        }

        async fn send(&self, message: &[u8], state: Option<&[u8]>) -> Step {
            self.service.handle(Some(self.settings.clone()), owner(), None, message, state, &self.metrics).await
        }

        /// Runs a whole conversation with `peer`, up to the end, and returns how it ended.
        async fn converse(&self, peer: &mut Peer) -> Step {
            let mut step = self.send(&identity_response(1, b"alice.example.com"), None).await;
            for _ in 0..64 {
                let Step::Challenge { packet, state } = &step else { return step };
                let response = peer.answer(packet);
                step = self.send(&response, state.as_ref().map(|state| state.as_slice())).await;
            }
            panic!("the conversation did not end");
        }

        fn peer(&self, certificate: Option<&str>, version: &'static rustls::SupportedProtocolVersion) -> Peer {
            Peer::new(client_connection(self.dir.path(), certificate, version))
        }
    }

    fn failure_of(step: &Step) -> (u8, String) {
        match step {
            Step::Failed { packet, reason, .. } => (EapPacket::parse(packet).unwrap().code, reason.clone()),
            Step::Closed { packet } => (EapPacket::parse(packet).unwrap().code, String::new()),
            Step::Authorize(_) => panic!("waiting for a chain"),
            Step::Lookup(_) => panic!("waiting for the database"),
            Step::Challenge { .. } => panic!("still going"),
            Step::Finished(_) => panic!("it finished"),
            Step::Discard => panic!("discarded"),
        }
    }

    #[test]
    fn test_alternative_names_that_cannot_be_read_refuse_the_certificate() {
        let certificate = |names: Vec<String>, extra: Option<Vec<u8>>| {
            let key = rcgen::KeyPair::generate().unwrap();
            let mut params = rcgen::CertificateParams::new(names).unwrap();
            params.distinguished_name.push(rcgen::DnType::CommonName, "alice");
            params.custom_extensions.extend(extra.map(|content| rcgen::CustomExtension::from_oid_content(&[2, 5, 29, 17], content)));
            params.self_signed(&key).unwrap().der().clone()
        };
        // A sequence of one DNS name, bob.example.com.
        let bob = [&[0x30, 17, 0x82, 15][..], b"bob.example.com"].concat();
        // And a sequence of one DNS name that is not text.
        let not_text = vec![0x30, 3, 0x82, 1, 0xff];
        for bad in [certificate(vec!["alice.example.com".to_string()], Some(bob)), certificate(Vec::new(), Some(vec![0xff, 0xff])), certificate(Vec::new(), Some(not_text))] {
            for match_identity in [true, false] {
                assert!(certificate_name(&bad, b"alice", match_identity).is_err(), "the common name must not stand in, match_identity = {match_identity}");
            }
        }
        let (name, _) = certificate_name(&certificate(Vec::new(), None), b"alice", true).unwrap();
        assert_eq!(name, "alice", "with none at all it does");
        let (name, _) = certificate_name(&certificate(vec!["alice.example.com".to_string()], None), b"alice.example.com", true).unwrap();
        assert_eq!(name, "alice.example.com");
    }

    /// Does not compile if the keys are held in memory nobody clears.
    #[tokio::test]
    async fn test_the_keys_are_wiped() {
        fn cleared<T: zeroize::ZeroizeOnDrop>(_: &T) {}
        let rig = rig();
        let mut peer = rig.peer(Some("alice"), &rustls::version::TLS12);
        let Step::Finished(mut finished) = rig.converse(&mut peer).await else { panic!("it did not finish") };
        cleared(&finished.msk);
        rig.service.settled(&mut finished, &rig.metrics);
    }

    #[tokio::test]
    async fn test_full_conversation_gives_both_sides_the_keys() {
        let rig = rig();
        for (version, label, context) in [
            (&rustls::version::TLS12, &b"client EAP encryption"[..], None),
            (&rustls::version::TLS13, &b"EXPORTER_EAP_TLS_Key_Material"[..], Some(&[TYPE_TLS][..])),
        ] {
            let mut peer = rig.peer(Some("alice"), version);
            let Step::Finished(mut finished) = rig.converse(&mut peer).await else { panic!("{:?} did not finish", version.version) };
            assert_eq!(rig.metrics.eap_open.get(), 1, "open until the chain has decided");
            rig.service.settled(&mut finished, &rig.metrics);
            assert_eq!(finished.method, TYPE_TLS);
            assert_eq!(finished.identity, b"alice.example.com");
            assert!(finished.certificate.is_some());
            let mut theirs = [0u8; 128];
            peer.client.export_keying_material(&mut theirs[..], label, context).unwrap();
            assert_eq!(finished.msk[..], theirs[..64]);
            assert_eq!(rig.metrics.eap_open.get(), 0, "a finished conversation is gone");
            if version.version == rustls::ProtocolVersion::TLSv1_3 {
                // The commitment message arrived as application data.
                let mut commitment = [0u8; 8];
                let read = std::io::Read::read(&mut peer.client.reader(), &mut commitment).unwrap();
                assert_eq!(commitment[..read], [0]);
            }
        }
    }

    #[tokio::test]
    async fn test_no_certificate() {
        let rig = rig();
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            let mut peer = rig.peer(None, version);
            let (code, reason) = failure_of(&rig.converse(&mut peer).await);
            assert_eq!(code, CODE_FAILURE, "{reason}");
            assert!(peer.refused, "the peer heard why");
        }
        assert_eq!(rig.metrics.eap_open.get(), 0);
    }

    #[tokio::test]
    async fn test_certificate_from_another_ca() {
        let rig = rig();
        let mut peer = rig.peer(Some("stranger"), &rustls::version::TLS12);
        let (code, _) = failure_of(&rig.converse(&mut peer).await);
        assert_eq!(code, CODE_FAILURE);
    }

    #[tokio::test]
    async fn test_revoked_certificate() {
        let mut rig = rig();
        rig.settings = Arc::new(eap_settings(rig.dir.path(), Some("alice-revoked.crl")));
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            let mut peer = rig.peer(Some("alice"), version);
            let (code, _) = failure_of(&rig.converse(&mut peer).await);
            assert_eq!(code, CODE_FAILURE);
        }
    }

    #[tokio::test]
    async fn test_fragmented_flights_both_ways() {
        let mut rig = rig();
        rig.settings = Arc::new(EapSettings { fragment_size: 256, ..eap_settings(rig.dir.path(), None) });
        let mut peer = rig.peer(Some("alice"), &rustls::version::TLS12);
        peer.fragment_size = 200;
        let mut step = rig.send(&identity_response(1, b"alice.example.com"), None).await;
        let (mut with_length, mut with_more) = (0, 0);
        for _ in 0..64 {
            let Step::Challenge { packet, state } = &step else { break };
            let request = EapPacket::parse(packet).unwrap();
            let fragment = parse_tls_fragment(&request.body[1..]).unwrap();
            assert!(fragment.data.len() <= 256, "over fragment_size");
            with_length += usize::from(fragment.total.is_some());
            with_more += usize::from(fragment.more);
            assert!(!fragment.more || fragment.data.len() == 256 || request.body[1] == FLAG_START);
            let response = peer.answer(packet);
            step = rig.send(&response, state.as_ref().map(|state| state.as_slice())).await;
        }
        assert!(with_more > 0, "the flight never needed fragments");
        assert!(with_length > 0);
        assert!(matches!(step, Step::Finished(_)));
    }

    #[tokio::test]
    async fn test_tls_message_over_the_limit() {
        let rig = rig();
        let Step::Challenge { state, packet } = rig.send(&identity_response(1, b"a"), None).await else { panic!() };
        let state = state.unwrap();
        let id = EapPacket::parse(&packet).unwrap().id;
        let mut body = vec![TYPE_TLS];
        body.extend_from_slice(&encode_tls_fragment(b"x", Some(MAX_TLS_MESSAGE as u32 + 1), true));
        let response = EapPacket { code: CODE_RESPONSE, id, body }.encode();
        let (code, reason) = failure_of(&rig.send(&response, Some(&state)).await);
        assert_eq!(code, CODE_FAILURE);
        assert!(reason.contains("limit"), "{reason}");
    }

    #[tokio::test]
    async fn test_tls_fragments_without_a_length_are_limited_too() {
        let rig = rig();
        let Step::Challenge { state, packet } = rig.send(&identity_response(1, b"a"), None).await else { panic!() };
        let state = state.unwrap();
        let mut id = EapPacket::parse(&packet).unwrap().id;
        let chunk = vec![0u8; 1000];
        let mut sent = 0;
        let step = loop {
            let mut body = vec![TYPE_TLS];
            body.extend_from_slice(&encode_tls_fragment(&chunk, None, true));
            let response = EapPacket { code: CODE_RESPONSE, id, body }.encode();
            sent += chunk.len();
            assert!(sent < 2 * MAX_TLS_MESSAGE, "no limit after {sent} octets");
            match rig.send(&response, Some(&state)).await {
                Step::Challenge { packet, .. } => id = EapPacket::parse(&packet).unwrap().id,
                step => break step,
            }
        };
        let (code, reason) = failure_of(&step);
        assert_eq!(code, CODE_FAILURE);
        assert!(reason.contains("limit"), "{reason}");
        assert!(sent > MAX_TLS_MESSAGE - chunk.len() && sent <= MAX_TLS_MESSAGE + chunk.len(), "stopped at {sent}");
    }

    /// `rcgen` cannot make a subject with one attribute twice, so this one came from `openssl req`.
    const TWO_OU: &str = "-----BEGIN CERTIFICATE-----\n\
         MIIBdzCCAR4CCQC75RCgEMHyYjAKBggqhkjOPQQDAjBDMRAwDgYDVQQKDAdFeGFt\n\
         cGxlMQ4wDAYDVQQLDAVmaXJzdDEPMA0GA1UECwwGc2Vjb25kMQ4wDAYDVQQDDAVh\n\
         bGljZTAgFw0yNjExMDcwNDI1MzVaGA8yMTI2MTAxNDA0MjUzNVowQzEQMA4GA1UE\n\
         CgwHRXhhbXBsZTEOMAwGA1UECwwFZmlyc3QxDzANBgNVBAsMBnNlY29uZDEOMAwG\n\
         A1UEAwwFYWxpY2UwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAQBjx5mO0tZnQa+\n\
         4bA8ntTbSZ0xBJcb/uPF0Yiu0W4A0JrR/TIrx2TlDe1pq7m+jh0IlsOrXZQIlsY/\n\
         7GlccRFvMAoGCCqGSM49BAMCA0cAMEQCIE+UQFJFGTjsHE+VvQ/bBDcy/v2HOebU\n\
         h2yD5WW8OC1iAiB36a17BR+Dper9Mfp64QA/7TMDz3RtB+g1xqYQqWHQaA==\n\
         -----END CERTIFICATE-----";

    #[test]
    fn test_two_values_of_one_attribute_in_a_real_certificate() {
        let certificate = CertificateDer::from_pem_slice(TWO_OU.as_bytes()).unwrap();
        let (name, subject) = certificate_name(&certificate, b"alice", true).unwrap();
        assert_eq!(name, "alice");
        let ou: Vec<&str> = subject.attrs.iter().filter(|(attribute, _)| attribute == "OU").map(|(_, value)| value.as_str()).collect();
        assert_eq!(ou, ["first", "second"], "both, in the certificate's order");
        assert_eq!(subject.text, "O=Example, OU=first, OU=second, CN=alice");
    }

    #[tokio::test]
    async fn test_unknown_state() {
        let rig = rig();
        let (code, _) = failure_of(&rig.send(&identity_response(2, b"a"), Some(&[9; 16])).await);
        assert_eq!(code, CODE_FAILURE);
        let (code, _) = failure_of(&rig.send(&identity_response(2, b"a"), Some(&[9; 3])).await);
        assert_eq!(code, CODE_FAILURE, "a State of another size");
    }

    #[tokio::test]
    async fn test_state_from_another_client() {
        let rig = rig();
        let Step::Challenge { packet, state } = rig.send(&identity_response(1, b"alice.example.com"), None).await else { panic!() };
        let state = state.unwrap();
        let mut peer = rig.peer(Some("alice"), &rustls::version::TLS12);
        let response = peer.answer(&packet);
        for other in [Owner { client: "other".to_string(), ..owner() }, Owner { calling_station: Some(b"cc-dd".to_vec()), ..owner() }] {
            let step = rig.service.handle(Some(rig.settings.clone()), other, None, &response, Some(&state), &rig.metrics).await;
            assert_eq!(failure_of(&step).0, CODE_FAILURE);
        }
        assert!(matches!(rig.send(&response, Some(&state)).await, Step::Challenge { .. }), "the owner goes on");
    }

    #[tokio::test]
    async fn test_eap_retransmission() {
        let rig = rig();
        let Step::Challenge { packet, state } = rig.send(&identity_response(1, b"alice.example.com"), None).await else { panic!() };
        let state = state.unwrap();
        let mut peer = rig.peer(Some("alice"), &rustls::version::TLS12);
        let hello = peer.answer(&packet);
        let Step::Challenge { packet: first, .. } = rig.send(&hello, Some(&state)).await else { panic!() };
        // The same Response again, in a new RADIUS packet: the same Request, and TLS has not moved.
        let Step::Challenge { packet: again, .. } = rig.send(&hello, Some(&state)).await else { panic!() };
        assert_eq!(first, again);
        let mut step = Step::Challenge { packet: first, state: Some(state) };
        for _ in 0..64 {
            let Step::Challenge { packet, state } = &step else { break };
            let response = peer.answer(packet);
            step = rig.send(&response, state.as_ref().map(|state| state.as_slice())).await;
        }
        assert!(matches!(step, Step::Finished(_)));
    }

    #[tokio::test]
    async fn test_unexpected_identifier() {
        let rig = rig();
        let Step::Challenge { state, .. } = rig.send(&identity_response(1, b"a"), None).await else { panic!() };
        let state = state.as_ref().map(|s| s.as_slice());
        let late = EapPacket { code: CODE_RESPONSE, id: 77, body: vec![TYPE_TLS, 0] }.encode();
        assert!(matches!(rig.send(&late, state).await, Step::Discard), "discarded in silence");
        assert_eq!((rig.metrics.eap_open.get(), rig.metrics.eap_tls.reject.get()), (1, 0));
        // The conversation is where it was: a repeat gets the last Request, and the answer is taken.
        let Step::Challenge { packet, .. } = rig.send(&identity_response(1, b"a"), state).await else { panic!("the same Request again") };
        let hello = rig.peer(Some("alice"), &rustls::version::TLS12).answer(&packet);
        assert!(matches!(rig.send(&hello, state).await, Step::Challenge { .. }));
    }

    #[tokio::test]
    async fn test_those_with_only_an_identity_go_first() {
        for per_client in [true, false] {
            let mut rig = rig();
            let limit = |base: EapSettings| if per_client { EapSettings { max_sessions_per_client: 2, ..base } } else { EapSettings { max_sessions: 2, ..base } };
            rig.settings = Arc::new(limit(eap_settings(rig.dir.path(), None)));
            let client = |name: &'static str| if per_client { "a" } else { name };
            // The oldest, and the quietest: it has been waiting for a slow directory, say.
            let under_way = rig.past_identity(&device(client("a"), Some(b"1"))).await;
            let Step::Challenge { state: only_identity, .. } = rig.send_as(device(client("b"), Some(b"2")), &identity_response(1, b"a"), None).await else { panic!() };
            let Step::Challenge { state: newest, .. } = rig.send_as(device(client("c"), Some(b"3")), &identity_response(1, b"a"), None).await else { panic!() };
            let sessions = rig.service.sessions.lock();
            assert!(sessions.entries.contains_key(&under_way), "per client: {per_client}");
            assert!(!sessions.entries.contains_key(&only_identity.unwrap()), "per client: {per_client}");
            assert!(sessions.entries.contains_key(&newest.unwrap()), "per client: {per_client}");
            drop(sessions);
            assert_eq!(rig.metrics.eap_tls.evicted.get(), 1);
        }
    }

    #[tokio::test]
    async fn test_idle_session() {
        let mut rig = rig();
        rig.settings = Arc::new(EapSettings { session_timeout: Duration::from_millis(10), ..eap_settings(rig.dir.path(), None) });
        let Step::Challenge { state, packet } = rig.send(&identity_response(1, b"a"), None).await else { panic!() };
        assert_eq!(rig.metrics.eap_open.get(), 1);
        tokio::time::sleep(Duration::from_millis(30)).await;
        let id = EapPacket::parse(&packet).unwrap().id;
        let response = EapPacket { code: CODE_RESPONSE, id, body: vec![TYPE_TLS, 0] }.encode();
        assert_eq!(failure_of(&rig.send(&response, state.as_ref().map(|s| s.as_slice())).await).0, CODE_FAILURE);
        assert_eq!(rig.metrics.eap_tls.timeout.get(), 1);
        assert_eq!(rig.metrics.eap_open.get(), 0);
    }

    #[test]
    fn test_inner_messages() {
        let mut message = vec![TYPE_MSCHAPV2, MSCHAP_RESPONSE, 9, 0, 59, 49];
        message.extend_from_slice(&[1; 16]);
        message.extend_from_slice(&[0; 8]);
        message.extend_from_slice(&[2; 24]);
        message.push(0);
        message.extend_from_slice(b"alice");
        assert_eq!(parse_mschapv2_response(&message), Some(MschapResponse { id: 9, peer_challenge: [1; 16], response: [2; 24], name: b"alice" }));
        let with = |at: usize, value: u8| {
            let mut wrong = message.clone();
            wrong[at] = value;
            wrong
        };
        assert_eq!(parse_mschapv2_response(&with(0, TYPE_TLS)), None, "another type");
        assert_eq!(parse_mschapv2_response(&with(1, MSCHAP_SUCCESS)), None, "another operation");
        assert_eq!(parse_mschapv2_response(&with(4, 58)), None, "a length that does not fit");
        assert_eq!(parse_mschapv2_response(&with(5, 48)), None, "a value of another size");
        for length in 0..54 {
            assert_eq!(parse_mschapv2_response(&message[..length]), None, "cut at {length}");
        }
        let tlv = |body: &[u8]| EapPacket { code: CODE_RESPONSE, id: 5, body: [&[TYPE_TLV], body].concat() }.encode();
        assert_eq!(parse_tlv_result(&tlv(&[0x80, 3, 0, 2, 0, 1]), 5), Some(TLV_SUCCESS));
        assert_eq!(parse_tlv_result(&tlv(&[0, 3, 0, 2, 0, 2]), 5), Some(TLV_FAILURE), "not marked mandatory");
        assert_eq!(parse_tlv_result(&tlv(&[0x80, 3, 0, 2, 0, 1]), 6), None, "another Identifier");
        assert_eq!(parse_tlv_result(&tlv(&[0x80, 4, 0, 2, 0, 1]), 5), None, "another TLV");
        assert_eq!(parse_tlv_result(&tlv(&[0x80, 3, 0, 2, 0]), 5), None, "cut short");
        assert_eq!(parse_tlv_result(&EapPacket { code: CODE_REQUEST, id: 5, body: vec![TYPE_TLV, 0x80, 3, 0, 2, 0, 1] }.encode(), 5), None, "a Request");
    }

    #[tokio::test]
    async fn test_peap_cannot_finish_without_a_chain_saying() {
        // A peer that skips to the end: an EAP-TLV Success where the challenge's answer belongs.
        let mut rig = rig();
        rig.settings = Arc::new(peap_settings(rig.dir.path()));
        let inner: Inner = Box::new(|message: &[u8]| match message[0] {
            TYPE_IDENTITY => vec![TYPE_IDENTITY, b'a'],
            _ => EapPacket { code: CODE_RESPONSE, id: 1, body: vec![TYPE_TLV, 0x80, 3, 0, 2, 0, 1] }.encode(),
        });
        let (step, _) = talk(&rig, peap_peer(rig.dir.path(), inner), b"anonymous").await;
        assert_eq!(failure_of(&step).0, CODE_FAILURE);
    }

    /// Runs a whole conversation from the first request, and how it ended.
    async fn talk(rig: &Rig, mut peer: Peer, identity: &[u8]) -> (Step, Peer) {
        let mut step = rig.send(&identity_response(1, identity), None).await;
        for _ in 0..64 {
            let Step::Challenge { packet, state } = &step else { break };
            let response = peer.answer(packet);
            step = rig.send(&response, state.as_ref().map(|state| state.as_slice())).await;
        }
        (step, peer)
    }

    #[test]
    fn test_peap_tls_settings() {
        let dir = tempfile::tempdir().unwrap();
        certificates(dir.path());
        let config = tunnel_tls_config(&dir.path().join("server.pem"), &dir.path().join("server.key")).unwrap();
        assert_eq!(config.send_tls13_tickets, 0);
        let mut client = Connection::Client(client_connection(dir.path(), None, &rustls::version::TLS12));
        let mut server = Connection::Server(ServerConnection::new(config.clone()).unwrap());
        while deliver(&mut client, &mut server) + deliver(&mut server, &mut client) > 0 {}
        let Connection::Server(server) = server else { unreachable!() };
        assert!(!server.is_handshaking(), "no certificate asked for");
        assert_eq!(server.protocol_version(), Some(ProtocolVersion::TLSv1_2));
        let msk = eap_tls_msk(&server).unwrap();
        let mut theirs = [0u8; 64];
        let Connection::Client(client) = client else { unreachable!() };
        client.export_keying_material(&mut theirs[..], b"client EAP encryption", None).unwrap();
        // Asking for 128 and for 64 give the same start under TLS 1.2's exporter.
        assert_eq!(*msk, theirs);
    }

    #[test]
    fn test_avps() {
        let name = avp(1, None, true, b"alice");
        assert_eq!(name.len(), 16, "padded to four octets");
        let vendor = avp(11, Some(311), true, &[9; 16]);
        let optional = avp(99, None, false, b"x");
        let message = [name.clone(), vendor.clone(), optional.clone()].concat();
        assert_eq!(parse_avps(&message).unwrap(), vec![
            Avp { code: 1, vendor: None, mandatory: true, data: b"alice" },
            Avp { code: 11, vendor: Some(311), mandatory: true, data: &[9; 16] },
            Avp { code: 99, vendor: None, mandatory: false, data: b"x" },
        ]);
        assert_eq!(parse_avps(&[]).unwrap(), Vec::new());
        // The last one's padding may be left off.
        assert_eq!(parse_avps(&name[..13]).unwrap()[0].data, b"alice");
        let with_length = |length: u32| {
            let mut wrong = name.clone();
            wrong[5..8].copy_from_slice(&length.to_be_bytes()[1..]);
            wrong
        };
        assert_eq!(parse_avps(&with_length(17)), None, "past the end");
        assert_eq!(parse_avps(&with_length(7)), None, "shorter than its own header");
        assert_eq!(parse_avps(&with_length(0xffffff)), None, "far past the end");
        let mut flagged = avp(11, None, true, &[1; 4]);
        flagged[4] |= 0x80;
        assert_eq!(parse_avps(&flagged), Some(vec![Avp { code: 11, vendor: Some(0x01010101), mandatory: true, data: &[] }]));
        flagged[5..8].copy_from_slice(&[0, 0, 8]);
        assert_eq!(parse_avps(&flagged), None, "no room for its vendor");
        for length in 1..8 {
            assert_eq!(parse_avps(&name[..length]), None, "cut at {length}");
        }
    }

    #[test]
    fn test_ttls_exports() {
        let dir = tempfile::tempdir().unwrap();
        certificates(dir.path());
        let config = tunnel_tls_config(&dir.path().join("server.pem"), &dir.path().join("server.key")).unwrap();
        let mut client = Connection::Client(client_connection(dir.path(), None, &rustls::version::TLS12));
        let mut server = Connection::Server(ServerConnection::new(config).unwrap());
        while deliver(&mut client, &mut server) + deliver(&mut server, &mut client) > 0 {}
        let (Connection::Server(server), Connection::Client(client)) = (server, client) else { unreachable!() };
        let msk = tunnel_msk(&server, TYPE_TTLS).unwrap();
        let mut theirs = [0u8; 64];
        client.export_keying_material(&mut theirs[..], b"ttls keying material", None).unwrap();
        assert_eq!(*msk, theirs);
        let mut ours = [0u8; 17];
        server.export_keying_material(&mut ours[..], b"ttls challenge", None).unwrap();
        let mut challenge = [0u8; 17];
        client.export_keying_material(&mut challenge[..], b"ttls challenge", None).unwrap();
        assert_eq!(ours, challenge);
    }

    #[tokio::test]
    async fn test_ttls_other_version() {
        let mut rig = rig();
        rig.settings = Arc::new(ttls_settings(rig.dir.path()));
        let Step::Challenge { packet, state } = rig.send(&identity_response(1, b"anonymous"), None).await else { panic!() };
        let start = EapPacket::parse(&packet).unwrap();
        assert_eq!(start.body, vec![TYPE_TTLS, FLAG_START], "version 0");
        let response = EapPacket { code: CODE_RESPONSE, id: start.id, body: vec![TYPE_TTLS, 1] }.encode();
        let (code, reason) = failure_of(&rig.send(&response, state.as_ref().map(|s| s.as_slice())).await);
        assert_eq!(code, CODE_FAILURE);
        assert!(reason.contains("version"), "{reason}");
    }

    #[tokio::test]
    async fn test_ttls_tls13_only() {
        let mut rig = rig();
        rig.settings = Arc::new(ttls_settings(rig.dir.path()));
        let mut peer = ttls_peer(rig.dir.path(), Box::new(|_| Vec::new()), Box::new(|_| Vec::new()));
        peer.client = client_connection(rig.dir.path(), None, &rustls::version::TLS13);
        let (step, peer) = talk(&rig, peer, b"anonymous").await;
        assert_eq!(failure_of(&step).0, CODE_FAILURE);
        assert!(peer.client.is_handshaking());
        assert_eq!(rig.metrics.eap_ttls.reject.get(), 1);
    }

    /// How a conversation goes when the peer opens the tunnel with `opening`.
    async fn ttls_says(opening: Vec<u8>) -> Step {
        let mut rig = rig();
        rig.settings = Arc::new(ttls_settings(rig.dir.path()));
        let peer = ttls_peer(rig.dir.path(), Box::new(move |_| opening), Box::new(|_| Vec::new()));
        talk(&rig, peer, b"anonymous").await.0
    }

    /// A conversation that timed out while the chain decided is counted then, as `timeout`, and
    /// not again when the chain's answer ends it.
    #[tokio::test]
    async fn test_timed_out_while_waiting_is_counted_once() -> Result<(), String> {
        for (result, accepted) in [(InnerResult::Proved { success: None, account: "alice".to_string(), attributes: Vec::new() }, true), (InnerResult::Refused { account: "alice".to_string() }, false)] {
            let mut rig = rig();
            rig.settings = Arc::new(EapSettings { session_timeout: Duration::from_millis(1500), ..ttls_settings(rig.dir.path()) });
            let peer = ttls_peer(rig.dir.path(), Box::new(|_| pap_avps(Some("alice"), "secret")), Box::new(|_| Vec::new()));
            let Step::Authorize(pending) = talk(&rig, peer, b"anonymous").await.0 else { return Err("no question for the chain".to_string()) };
            assert!(rig.service.is_open(&pending.state));
            tokio::time::sleep(Duration::from_millis(1600)).await;
            rig.service.sweep(&rig.metrics);
            assert_eq!(rig.metrics.eap_ttls.timeout.get(), 1);
            assert!(!rig.service.is_open(&pending.state));
            match rig.service.authorized(*pending, result, &rig.metrics) {
                Step::Finished(finished) => assert!(accepted && finished.counted, "counted as it timed out"),
                // The tunnel says so to the peer first; the conversation is gone, so that answer
                // gets no further.
                Step::Challenge { .. } => assert!(accepted),
                Step::Failed { .. } => assert!(!accepted),
                // A refusal inside the tunnel ends it; the caller counts that only while it is open.
                Step::Closed { .. } => assert!(!accepted),
                Step::Discard => return Err(format!("discard, accepted {accepted}")),
                Step::Authorize(_) | Step::Lookup(_) => return Err(format!("asks again, accepted {accepted}")),
            }
            assert_eq!(rig.metrics.eap_ttls.reject.get(), 0, "not counted again");
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_ttls_unknown_mandatory_avp() {
        let opening = [pap_avps(Some("alice"), "secret"), avp(99, None, true, b"x")].concat();
        let (code, reason) = failure_of(&ttls_says(opening).await);
        assert_eq!(code, CODE_FAILURE);
        assert!(reason.contains("mandatory"), "{reason}");
    }

    #[tokio::test]
    async fn test_ttls_unknown_optional_avp_is_skipped() {
        let opening = [avp(99, None, false, b"x"), pap_avps(Some("alice"), "secret")].concat();
        assert!(matches!(ttls_says(opening).await, Step::Authorize(_)));
    }

    #[tokio::test]
    async fn test_ttls_bad_avp_length() {
        let mut opening = pap_avps(Some("alice"), "secret");
        opening[7] = 200;
        let (code, reason) = failure_of(&ttls_says(opening).await);
        assert_eq!(code, CODE_FAILURE);
        assert!(reason.contains("AVP"), "{reason}");
    }

    #[tokio::test]
    async fn test_ttls_no_inner_name() {
        let (code, reason) = failure_of(&ttls_says(pap_avps(None, "secret")).await);
        assert_eq!(code, CODE_FAILURE);
        assert!(reason.contains("name"), "{reason}");
    }

    #[tokio::test]
    async fn test_ttls_challenge_the_peer_chose() {
        // Right for the challenge it names, which is not the exported one: no chain is asked.
        let (code, reason) = failure_of(&ttls_says(mschap_avps("alice", "secret", &[5; 16], 5)).await);
        assert_eq!(code, CODE_FAILURE);
        assert!(reason.contains("challenge"), "{reason}");
    }

    #[tokio::test]
    async fn test_ttls_oversized_inner_values() {
        let (code, reason) = failure_of(&ttls_says(pap_avps(Some(&"n".repeat(254)), "secret")).await);
        assert_eq!(code, CODE_FAILURE);
        assert!(reason.contains("name"), "{reason}");
        assert!(matches!(ttls_says(pap_avps(Some(&"n".repeat(253)), "secret")).await, Step::Authorize(_)), "the longest name");
        let (code, reason) = failure_of(&ttls_says(pap_avps(Some("alice"), &"p".repeat(129))).await);
        assert_eq!(code, CODE_FAILURE);
        assert!(reason.contains("password"), "{reason}");
        assert!(matches!(ttls_says(pap_avps(Some("alice"), &"p".repeat(128))).await, Step::Authorize(_)), "the longest password");
    }

    #[tokio::test]
    async fn test_ttls_ident_the_peer_chose() {
        let mut rig = rig();
        rig.settings = Arc::new(ttls_settings(rig.dir.path()));
        let peer = ttls_peer(rig.dir.path(), Box::new(|client| {
            let mut exported = [0u8; 17];
            client.export_keying_material(&mut exported[..], b"ttls challenge", None).unwrap();
            mschap_avps("alice", "secret", exported[..16].try_into().unwrap(), exported[16].wrapping_add(1))
        }), Box::new(|_| Vec::new()));
        let (code, reason) = failure_of(&talk(&rig, peer, b"anonymous").await.0);
        assert_eq!(code, CODE_FAILURE);
        assert!(reason.contains("challenge"), "{reason}");
    }

    #[tokio::test]
    async fn test_ttls_proved_needs_an_acknowledgement() {
        let mut ttls = Ttls::new();
        ttls.state = TtlsState::Proved;
        let dir = tempfile::tempdir().unwrap();
        let config = certificates(dir.path());
        let connection = ServerConnection::new(config).unwrap();
        assert!(matches!(ttls.receive(b"more", &connection), InnerStep::Refused(_)));
        assert!(matches!(ttls.receive(&[], &connection), InnerStep::Accepted(_)));
    }

    #[tokio::test]
    async fn test_ttls_nak_after_the_handshake() {
        let mut rig = rig();
        rig.settings = Arc::new(EapSettings { methods: vec![TYPE_TTLS, TYPE_PEAP], ..ttls_settings(rig.dir.path()) });
        let mut peer = ttls_peer(rig.dir.path(), Box::new(|_| Vec::new()), Box::new(|_| Vec::new()));
        peer.fragment_size = 16000;
        let mut step = rig.send(&identity_response(1, b"anonymous"), None).await;
        for _ in 0..64 {
            let Step::Challenge { packet, state } = &step else { panic!("it ended") };
            if is_empty_request(packet) && peer.client.wants_read() && !peer.client.is_handshaking() {
                let id = EapPacket::parse(packet).unwrap().id;
                let nak = EapPacket { code: CODE_RESPONSE, id, body: vec![TYPE_NAK, TYPE_PEAP] }.encode();
                let (code, reason) = failure_of(&rig.send(&nak, state.as_ref().map(|state| state.as_slice())).await);
                assert_eq!(code, CODE_FAILURE);
                assert!(reason.contains("EAP type 3 in a conversation of type 21"), "{reason}");
                return;
            }
            let response = peer.answer(packet);
            step = rig.send(&response, state.as_ref().map(|state| state.as_slice())).await;
        }
        panic!("no empty request after the handshake");
    }

    #[tokio::test]
    async fn test_acknowledgement_cannot_skip_the_handshake() {
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            // After the Start.
            let rig = rig();
            let Step::Challenge { packet, state } = rig.send(&identity_response(1, b"alice.example.com"), None).await else { panic!() };
            let ack = |packet: &[u8]| EapPacket { code: CODE_RESPONSE, id: EapPacket::parse(packet).unwrap().id, body: vec![TYPE_TLS, 0] }.encode();
            let (code, reason) = failure_of(&rig.send(&ack(&packet), state.as_ref().map(|state| state.as_slice())).await);
            assert_eq!(code, CODE_FAILURE);
            assert!(reason.contains("middle of the handshake"), "{reason}");
            // After everything of the server's first flight.
            let rig = self::rig();
            let mut peer = rig.peer(Some("alice"), version);
            let mut step = rig.send(&identity_response(1, b"alice.example.com"), None).await;
            let mut answered_start = false;
            loop {
                let Step::Challenge { packet, state } = &step else { panic!("it ended") };
                let flags = EapPacket::parse(packet).unwrap().body[1];
                if answered_start && flags & FLAG_MORE == 0 {
                    let (code, reason) = failure_of(&rig.send(&ack(packet), state.as_ref().map(|state| state.as_slice())).await);
                    assert_eq!(code, CODE_FAILURE);
                    assert!(reason.contains("middle of the handshake"), "{reason}");
                    break;
                }
                answered_start = true;
                let response = peer.answer(packet);
                step = rig.send(&response, state.as_ref().map(|state| state.as_slice())).await;
            }
        }
    }

    /// A PEAP conversation up to the refusal of its password, with the Response the refusal
    /// answered.
    async fn peap_refused(rig: &Rig) -> (Vec<u8>, [u8; 16]) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut peer = peap_peer(rig.dir.path(), mschap_inner("alice", "wrong", log.clone()));
        let mut step = rig.send(&identity_response(1, b"anonymous"), None).await;
        let mut last = Vec::new();
        for _ in 0..64 {
            match step {
                Step::Challenge { ref packet, ref state } => {
                    let response = peer.answer(packet);
                    if !log.lock().is_empty() {
                        return (last, state.unwrap());
                    }
                    step = rig.send(&response, state.as_ref().map(|state| state.as_slice())).await;
                    last = response;
                }
                Step::Authorize(pending) => {
                    step = rig.service.authorized(*pending, InnerResult::Refused { account: "alice".to_string() }, &rig.metrics);
                }
                _ => panic!("it ended"),
            }
        }
        panic!("no refusal");
    }

    #[tokio::test]
    async fn test_malformed_packet_from_another_device() {
        let rig = rig();
        let Step::Challenge { packet, state } = rig.send(&identity_response(1, b"alice.example.com"), None).await else { panic!() };
        let state = state.unwrap();
        let other = Owner { calling_station: Some(b"cc-dd".to_vec()), ..owner() };
        let step = rig.service.handle(Some(rig.settings.clone()), other, None, &[2, 1], Some(&state), &rig.metrics).await;
        assert!(matches!(step, Step::Failed { ended: None, .. }), "nothing of theirs ended");
        assert_eq!(rig.metrics.eap_tls.reject.get(), 0);
        let hello = rig.peer(Some("alice"), &rustls::version::TLS12).answer(&packet);
        assert!(matches!(rig.send(&hello, Some(&state)).await, Step::Challenge { .. }), "the owner goes on");
    }

    #[tokio::test]
    async fn test_malformed_packet_after_a_refusal() {
        let mut rig = rig();
        rig.settings = Arc::new(peap_settings(rig.dir.path()));
        let (_, state) = peap_refused(&rig).await;
        let step = rig.send(&[2, 1], Some(&state)).await;
        assert!(matches!(step, Step::Failed { ended: None, .. }), "on record already");
        assert_eq!(rig.metrics.eap_peap.reject.get(), 0, "counted when it was refused");
        assert_eq!(rig.metrics.eap_open.get(), 0, "and it is over");
    }

    #[tokio::test]
    async fn test_abandoning_after_a_refusal_counts_nothing() {
        let mut rig = rig();
        rig.settings = Arc::new(peap_settings(rig.dir.path()));
        let (_, state) = peap_refused(&rig).await;
        assert!(rig.service.abandon(&state, &rig.metrics).await.is_none());
        assert_eq!(rig.metrics.eap_peap.reject.get(), 0, "counted when it was refused");
        assert_eq!(rig.metrics.eap_open.get(), 0, "and it is over");
        assert!(rig.service.abandon(&state, &rig.metrics).await.is_none(), "nothing left to abandon");
    }

    #[tokio::test]
    async fn test_abandoning_names_the_account() {
        let mut rig = rig();
        rig.settings = Arc::new(peap_settings(rig.dir.path()));
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut peer = peap_peer(rig.dir.path(), mschap_inner("alice", "secret", log));
        let mut step = rig.send(&identity_response(1, b"anonymous"), None).await;
        let mut authorized = false;
        let state = loop {
            match step {
                Step::Challenge { state, .. } if authorized => break state.unwrap(),
                Step::Challenge { ref packet, ref state } => {
                    let response = peer.answer(packet);
                    step = rig.send(&response, state.as_ref().map(|state| state.as_slice())).await;
                }
                Step::Authorize(pending) => {
                    authorized = true;
                    step = rig.service.authorized(*pending, InnerResult::Proved { success: None, account: "alice".to_string(), attributes: Vec::new() }, &rig.metrics);
                }
                _ => panic!("it ended"),
            }
        };
        let ended = rig.service.abandon(&state, &rig.metrics).await.expect("not on record yet");
        assert_eq!((ended.method, ended.account.as_str()), (TYPE_PEAP, "alice"));
        assert_eq!(rig.metrics.eap_peap.reject.get(), 1);
        assert_eq!(rig.metrics.eap_open.get(), 0);
    }

    #[tokio::test]
    async fn test_refused_conversation_is_not_counted_again_when_it_expires() {
        let mut rig = rig();
        rig.settings = Arc::new(EapSettings { session_timeout: Duration::from_millis(1500), ..peap_settings(rig.dir.path()) });
        let (_, state) = peap_refused(&rig).await;
        tokio::time::sleep(Duration::from_millis(1600)).await;
        rig.service.sweep(&rig.metrics);
        assert_eq!(rig.metrics.eap_peap.timeout.get(), 0, "counted when it was refused");
        assert_eq!(rig.metrics.eap_open.get(), 0);
        // The same when a request is the first to notice.
        let (answered, state2) = peap_refused(&rig).await;
        tokio::time::sleep(Duration::from_millis(1600)).await;
        let _ = rig.send(&answered, Some(&state2)).await;
        assert_eq!(rig.metrics.eap_peap.timeout.get(), 0, "counted when it was refused");
        let _ = state;
    }

    #[tokio::test]
    async fn test_refused_conversation_is_not_counted_again_when_dropped_for_room() {
        let mut rig = rig();
        rig.settings = Arc::new(EapSettings { max_sessions: 1, ..peap_settings(rig.dir.path()) });
        peap_refused(&rig).await;
        assert!(matches!(rig.send_as(device("other", Some(b"2")), &identity_response(1, b"a"), None).await, Step::Challenge { .. }));
        assert_eq!(rig.metrics.eap_peap.evicted.get(), 0, "counted when it was refused");
        // One that was not refused does count.
        assert!(matches!(rig.send_as(device("third", Some(b"3")), &identity_response(1, b"a"), None).await, Step::Challenge { .. }));
        assert_eq!(rig.metrics.eap_peap.evicted.get(), 1);
    }

    #[tokio::test]
    async fn test_refused_conversation_is_not_counted_again_when_dropped_for_its_client() {
        let mut rig = rig();
        rig.settings = Arc::new(EapSettings { max_sessions_per_client: 1, ..peap_settings(rig.dir.path()) });
        peap_refused(&rig).await;
        let same_client = |station: &[u8]| Owner { calling_station: Some(station.to_vec()), ..owner() };
        assert!(matches!(rig.send_as(same_client(b"2"), &identity_response(1, b"a"), None).await, Step::Challenge { .. }));
        assert_eq!(rig.metrics.eap_peap.evicted.get(), 0, "counted when it was refused");
        assert!(matches!(rig.send_as(same_client(b"3"), &identity_response(1, b"a"), None).await, Step::Challenge { .. }));
        assert_eq!(rig.metrics.eap_peap.evicted.get(), 1, "one that was not refused does count");
    }

    #[tokio::test]
    async fn test_too_many_rounds_after_a_refusal() {
        let mut rig = rig();
        rig.settings = Arc::new(peap_settings(rig.dir.path()));
        let (answered, state) = peap_refused(&rig).await;
        let mut step = rig.send(&answered, Some(&state)).await;
        for _ in 0..512 {
            if !matches!(step, Step::Challenge { .. }) {
                break;
            }
            // It says it did not hear, again and again.
            step = rig.send(&answered, Some(&state)).await;
        }
        let Step::Failed { reason, ended, .. } = step else { panic!("it did not reach the limit") };
        assert_eq!(reason, "too many rounds");
        assert!(ended.is_none(), "on record already");
        assert_eq!(rig.metrics.eap_peap.reject.get(), 0, "counted when it was refused");
    }

    #[tokio::test]
    async fn test_peap_inner_name_limit() {
        let mut rig = rig();
        rig.settings = Arc::new(peap_settings(rig.dir.path()));
        let log = Arc::new(Mutex::new(Vec::new()));
        let (step, _) = talk(&rig, peap_peer(rig.dir.path(), mschap_inner(&"a".repeat(MAX_INNER_NAME + 1), "x", log.clone())), b"anonymous").await;
        let (code, reason) = failure_of(&step);
        assert_eq!(code, CODE_FAILURE);
        assert!(reason.contains("inner name over"), "{reason}");
        let (step, _) = talk(&rig, peap_peer(rig.dir.path(), mschap_inner(&"a".repeat(MAX_INNER_NAME), "x", log)), b"anonymous").await;
        assert!(matches!(step, Step::Authorize(_)), "up to the chain");
    }

    #[tokio::test]
    async fn test_ttls_acknowledges_and_says_nothing() {
        let mut rig = rig();
        rig.settings = Arc::new(ttls_settings(rig.dir.path()));
        let mut peer = ttls_peer(rig.dir.path(), Box::new(|_| Vec::new()), Box::new(|_| Vec::new()));
        peer.fragment_size = 16000;
        let mut step = rig.send(&identity_response(1, b"anonymous"), None).await;
        let mut asked = 0;
        for _ in 0..64 {
            let Step::Challenge { packet, state } = &step else { break };
            asked += usize::from(is_empty_request(packet));
            let response = peer.answer(packet);
            step = rig.send(&response, state.as_ref().map(|state| state.as_slice())).await;
        }
        let (code, reason) = failure_of(&step);
        assert_eq!(code, CODE_FAILURE);
        assert_eq!(reason, "too many inner rounds");
        assert_eq!(asked, 8, "its turn again, eight times");
    }

    #[tokio::test]
    async fn test_request_repeated_during_a_slow_check() {
        let mut rig = rig();
        rig.settings = Arc::new(ttls_settings(rig.dir.path()));
        let mut peer = ttls_peer(rig.dir.path(), Box::new(|_| pap_avps(Some("alice"), "secret")), Box::new(|_| Vec::new()));
        let mut step = rig.send(&identity_response(1, b"anonymous"), None).await;
        let mut last = (Vec::new(), None);
        for _ in 0..64 {
            let Step::Challenge { packet, state } = &step else { break };
            let response = peer.answer(packet);
            last = (response.clone(), *state);
            step = rig.send(&response, state.as_ref().map(|state| state.as_slice())).await;
        }
        let Step::Authorize(pending) = step else { panic!("no chain to ask") };
        let (response, state) = last;
        // The chain is thinking. The same response comes again, and waits.
        let repeated = rig.send(&response, state.as_ref().map(|state| state.as_slice()));
        let answer = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            rig.service.authorized(*pending, InnerResult::Proved { success: None, account: "alice".to_string(), attributes: Vec::new() }, &rig.metrics)
        };
        let (repeated, answered) = tokio::join!(repeated, answer);
        assert!(matches!(answered, Step::Finished(_)));
        assert!(matches!(repeated, Step::Discard), "the conversation is over");
    }

    #[tokio::test]
    async fn test_peap_tls13_only() {
        let mut rig = rig();
        rig.settings = Arc::new(peap_settings(rig.dir.path()));
        let mut peer = peap_peer(rig.dir.path(), mschap_inner("alice", "secret", Default::default()));
        peer.client = client_connection(rig.dir.path(), None, &rustls::version::TLS13);
        let (step, peer) = talk(&rig, peer, b"anonymous").await;
        assert_eq!(failure_of(&step).0, CODE_FAILURE);
        assert!(peer.client.is_handshaking());
        assert_eq!(rig.metrics.eap_peap.reject.get(), 1);
    }

    #[tokio::test]
    async fn test_peap_other_version() {
        let mut rig = rig();
        rig.settings = Arc::new(peap_settings(rig.dir.path()));
        let Step::Challenge { packet, state } = rig.send(&identity_response(1, b"anonymous"), None).await else { panic!() };
        let start = EapPacket::parse(&packet).unwrap();
        assert_eq!(start.body, vec![TYPE_PEAP, FLAG_START], "version 0");
        let response = EapPacket { code: CODE_RESPONSE, id: start.id, body: vec![TYPE_PEAP, 1] }.encode();
        let (code, reason) = failure_of(&rig.send(&response, state.as_ref().map(|s| s.as_slice())).await);
        assert_eq!(code, CODE_FAILURE);
        assert!(reason.contains("version"), "{reason}");
    }

    #[tokio::test]
    async fn test_peap_too_many_inner_rounds() {
        let mut rig = rig();
        rig.settings = Arc::new(peap_settings(rig.dir.path()));
        // Says who it is again each time it is asked anything.
        let heard = Arc::new(Mutex::new(0));
        let count = heard.clone();
        let inner: Inner = Box::new(move |_: &[u8]| {
            *count.lock() += 1;
            vec![TYPE_IDENTITY, b'a']
        });
        let (step, _) = talk(&rig, peap_peer(rig.dir.path(), inner), b"anonymous").await;
        let (code, reason) = failure_of(&step);
        assert_eq!(code, CODE_FAILURE);
        assert!(reason.contains("inner"), "{reason}");
        assert_eq!(*heard.lock(), 9, "eight rounds answered, and refused at the ninth message");
    }

    fn both_methods(dir: &Path) -> EapSettings {
        EapSettings { methods: vec![TYPE_TLS, TYPE_PEAP], tunnel_tls: peap_settings(dir).tunnel_tls, ..eap_settings(dir, None) }
    }

    #[tokio::test]
    async fn test_overload_counts_for_the_client_method() {
        let mut rig = rig();
        rig.settings = Arc::new(EapSettings { max_sessions_per_device: 1, ..both_methods(rig.dir.path()) });
        let allowed = [TYPE_PEAP];
        let this = || device("a", Some(b"1"));
        let identity = identity_response(1, b"a");
        let Step::Challenge { packet, state } = rig.service.handle(Some(rig.settings.clone()), this(), Some(&allowed), &identity, None, &rig.metrics).await else { panic!() };
        let hello = peap_peer(rig.dir.path(), mschap_inner("a", "b", Default::default())).answer(&packet);
        assert!(matches!(rig.service.handle(Some(rig.settings.clone()), this(), Some(&allowed), &hello, state.as_ref().map(|s| s.as_slice()), &rig.metrics).await, Step::Challenge { .. }));
        assert!(matches!(rig.service.handle(Some(rig.settings.clone()), this(), Some(&allowed), &identity, None, &rig.metrics).await, Step::Discard));
        assert_eq!(rig.metrics.eap_peap.overload.get(), 1);
        assert_eq!(rig.metrics.eap_tls.overload.get(), 0, "though the server lists tls first");
    }

    #[tokio::test]
    async fn test_client_allows_only_certificates() {
        let mut rig = rig();
        rig.settings = Arc::new(both_methods(rig.dir.path()));
        let allowed = [TYPE_TLS];
        let open = rig.service.handle(Some(rig.settings.clone()), owner(), Some(&allowed), &identity_response(1, b"a"), None, &rig.metrics).await;
        let Step::Challenge { packet, state } = open else { panic!() };
        let start = EapPacket::parse(&packet).unwrap();
        assert_eq!(start.body[0], TYPE_TLS);
        let nak = EapPacket { code: CODE_RESPONSE, id: start.id, body: vec![TYPE_NAK, TYPE_PEAP] }.encode();
        let (code, reason) = failure_of(&rig.send(&nak, state.as_ref().map(|s| s.as_slice())).await);
        assert_eq!(code, CODE_FAILURE, "the server has PEAP, and this client may not use it");
        assert!(reason.contains("does not offer"), "{reason}");
    }

    #[tokio::test]
    async fn test_client_allows_another_method_first() {
        let mut rig = rig();
        rig.settings = Arc::new(both_methods(rig.dir.path()));
        let allowed = [TYPE_PEAP];
        let open = rig.service.handle(Some(rig.settings.clone()), owner(), Some(&allowed), &identity_response(1, b"a"), None, &rig.metrics).await;
        let Step::Challenge { packet, .. } = open else { panic!() };
        assert_eq!(EapPacket::parse(&packet).unwrap().body[0], TYPE_PEAP);
    }

    #[tokio::test]
    async fn test_client_allows_no_method() {
        let mut rig = rig();
        rig.settings = Arc::new(both_methods(rig.dir.path()));
        let open = rig.service.handle(Some(rig.settings.clone()), owner(), Some(&[]), &identity_response(1, b"a"), None, &rig.metrics).await;
        assert_eq!(failure_of(&open).0, CODE_FAILURE);
        assert_eq!(rig.metrics.eap_open.get(), 0);
    }

    #[tokio::test]
    async fn test_methods_by_legacy_nak() {
        let mut rig = rig();
        rig.settings = Arc::new(EapSettings { methods: vec![TYPE_PEAP, TYPE_TLS], tunnel_tls: peap_settings(rig.dir.path()).tunnel_tls, ..eap_settings(rig.dir.path(), None) });
        let Step::Challenge { packet, state } = rig.send(&identity_response(1, b"alice.example.com"), None).await else { panic!() };
        let start = EapPacket::parse(&packet).unwrap();
        assert_eq!(start.body[0], TYPE_PEAP, "the first listed");
        let nak = EapPacket { code: CODE_RESPONSE, id: start.id, body: vec![TYPE_NAK, TYPE_TLS] }.encode();
        let Step::Challenge { packet, state: again } = rig.send(&nak, state.as_ref().map(|s| s.as_slice())).await else { panic!() };
        let mut peer = rig.peer(Some("alice"), &rustls::version::TLS13);
        let mut step = Step::Challenge { packet, state: again };
        for _ in 0..64 {
            let Step::Challenge { packet, state } = &step else { break };
            let response = peer.answer(packet);
            step = rig.send(&response, state.as_ref().map(|state| state.as_slice())).await;
        }
        assert!(matches!(step, Step::Finished(_)));
    }

    #[tokio::test]
    async fn test_too_many_sessions_from_one_client() {
        let mut rig = rig();
        rig.settings = Arc::new(EapSettings { max_sessions_per_client: 2, ..eap_settings(rig.dir.path(), None) });
        let identity = identity_response(1, b"a");
        let open = |client: &'static str, station: &'static [u8]| {
            let (rig, identity) = (&rig, &identity);
            async move {
                let Step::Challenge { state, .. } = rig.send_as(device(client, Some(station)), identity, None).await else { panic!("not opened") };
                state.unwrap()
            }
        };
        let other_client = open("b", b"1").await;
        let oldest = open("a", b"1").await;
        let newer = open("a", b"2").await;
        // Full for client a, whose oldest goes rather than the older one of client b.
        open("a", b"3").await;
        assert_eq!(rig.metrics.eap_tls.evicted.get(), 1);
        assert_eq!(rig.metrics.eap_tls.overload.get(), 0);
        assert_eq!(rig.metrics.eap_open.get(), 3);
        assert!(!rig.service.has_conversation(&oldest));
        assert!(rig.service.has_conversation(&newer));
        assert!(rig.service.has_conversation(&other_client));
        // Without a Calling-Station-Id it is still this client.
        assert!(matches!(rig.send_as(device("a", None), &identity, None).await, Step::Challenge { .. }));
        assert!(!rig.service.has_conversation(&newer));
    }

    #[tokio::test]
    async fn test_active_conversation_during_a_flood() {
        let mut rig = rig();
        rig.settings = Arc::new(EapSettings { max_sessions: 2, ..eap_settings(rig.dir.path(), None) });
        let this = device("a", Some(b"this"));
        let identity = identity_response(1, b"alice.example.com");
        let Step::Challenge { packet, state } = rig.send_as(this.clone(), &identity, None).await else { panic!() };
        let state = state.unwrap();
        let mut peer = rig.peer(Some("alice"), &rustls::version::TLS13);
        let mut request = packet;
        let mut flooded = 0u32;
        // Between every two rounds, new conversations turn the rest of the table over.
        let finished = loop {
            flooded += 1;
            let station = flooded.to_be_bytes();
            assert!(matches!(rig.send_as(device("a", Some(&station)), &identity, None).await, Step::Challenge { .. }));
            let response = peer.answer(&request);
            match rig.send_as(this.clone(), &response, Some(&state)).await {
                Step::Challenge { packet, .. } => request = packet,
                other => break other,
            }
        };
        assert!(matches!(finished, Step::Finished(_)), "evicted though active");
        assert!(rig.metrics.eap_tls.evicted.get() >= 1);
    }

    #[tokio::test]
    async fn test_too_many_sessions_from_one_device() {
        let mut rig = rig();
        rig.settings = Arc::new(EapSettings { max_sessions_per_device: 2, ..eap_settings(rig.dir.path(), None) });
        let this = device("a", Some(b"1"));
        rig.past_identity(&this).await;
        rig.past_identity(&this).await;
        assert!(matches!(rig.send_as(this, &identity_response(1, b"a"), None).await, Step::Discard));
        assert_eq!(rig.metrics.eap_tls.overload.get(), 1);
        assert!(matches!(rig.send_as(device("a", Some(b"2")), &identity_response(1, b"a"), None).await, Step::Challenge { .. }), "another device");
        assert!(matches!(rig.send_as(device("b", Some(b"1")), &identity_response(1, b"a"), None).await, Step::Challenge { .. }), "the same station at another client");
    }

    #[tokio::test]
    async fn test_device_starts_over() {
        let mut rig = rig();
        // A limit of one on each: it only works if the old one is gone first.
        rig.settings = Arc::new(EapSettings { max_sessions: 1, max_sessions_per_client: 1, max_sessions_per_device: 1, ..eap_settings(rig.dir.path(), None) });
        let this = device("a", Some(b"1"));
        let identity = identity_response(1, b"a");
        let Step::Challenge { packet, state: old } = rig.send_as(this.clone(), &identity, None).await else { panic!() };
        let hello = rig.peer(Some("alice"), &rustls::version::TLS12).answer(&packet);
        let Step::Challenge { packet, state: new } = rig.send_as(this.clone(), &identity, None).await else { panic!("not replaced") };
        assert_eq!(rig.metrics.eap_tls.replaced.get(), 1);
        assert_eq!(rig.metrics.eap_tls.overload.get(), 0);
        assert_eq!(rig.metrics.eap_open.get(), 1);
        assert_eq!(failure_of(&rig.send_as(this.clone(), &hello, old.as_ref().map(|s| s.as_slice())).await).0, CODE_FAILURE, "the old State");
        let hello = rig.peer(Some("alice"), &rustls::version::TLS12).answer(&packet);
        assert!(matches!(rig.send_as(this, &hello, new.as_ref().map(|s| s.as_slice())).await, Step::Challenge { .. }), "the new State");
    }

    #[tokio::test]
    async fn test_device_starts_over_during_a_handshake() {
        let rig = rig();
        let this = device("a", Some(b"1"));
        rig.past_identity(&this).await;
        assert!(matches!(rig.send_as(this, &identity_response(1, b"a"), None).await, Step::Challenge { .. }));
        assert_eq!(rig.metrics.eap_tls.replaced.get(), 0);
        assert_eq!(rig.metrics.eap_open.get(), 2);
    }

    #[tokio::test]
    async fn test_no_calling_station_replaces_nothing() {
        let mut rig = rig();
        rig.settings = Arc::new(EapSettings { max_sessions_per_device: 1, ..eap_settings(rig.dir.path(), None) });
        for _ in 0..3 {
            assert!(matches!(rig.send_as(device("a", None), &identity_response(1, b"a"), None).await, Step::Challenge { .. }));
        }
        assert_eq!(rig.metrics.eap_tls.replaced.get(), 0);
        assert_eq!(rig.metrics.eap_open.get(), 3);
    }

    #[tokio::test]
    async fn test_sweep_releases_the_limits() {
        let mut rig = rig();
        rig.settings = Arc::new(EapSettings { session_timeout: Duration::from_millis(10), max_sessions_per_client: 1, ..eap_settings(rig.dir.path(), None) });
        assert!(matches!(rig.send_as(device("a", Some(b"1")), &identity_response(1, b"a"), None).await, Step::Challenge { .. }));
        tokio::time::sleep(Duration::from_millis(30)).await;
        rig.service.sweep(&rig.metrics);
        assert!(matches!(rig.send_as(device("a", Some(b"2")), &identity_response(1, b"a"), None).await, Step::Challenge { .. }));
    }

    #[tokio::test]
    async fn test_too_many_sessions() {
        let mut rig = rig();
        rig.settings = Arc::new(EapSettings { max_sessions: 2, ..eap_settings(rig.dir.path(), None) });
        let identity = identity_response(1, b"a");
        let open = |client: &'static str| {
            let (rig, identity) = (&rig, &identity);
            async move {
                let Step::Challenge { state, .. } = rig.send_as(device(client, Some(b"1")), identity, None).await else { panic!("not opened") };
                state.unwrap()
            }
        };
        let oldest = open("a").await;
        let newer = open("b").await;
        open("c").await;
        assert_eq!(rig.metrics.eap_tls.evicted.get(), 1);
        assert_eq!(rig.metrics.eap_tls.overload.get(), 0);
        assert_eq!(rig.metrics.eap_open.get(), 2);
        assert!(!rig.service.has_conversation(&oldest), "whichever client it belongs to");
        assert!(rig.service.has_conversation(&newer));
        // Dropped: the device hears of it on its next round.
        let response = EapPacket { code: CODE_RESPONSE, id: 2, body: vec![TYPE_TLS, 0] }.encode();
        assert_eq!(failure_of(&rig.send_as(device("a", Some(b"1")), &response, Some(&oldest)).await).0, CODE_FAILURE);
    }

    #[tokio::test]
    async fn test_eap_start() {
        let rig = rig();
        let Step::Challenge { packet, state } = rig.send(&[], None).await else { panic!() };
        assert_eq!(state, None);
        let request = EapPacket::parse(&packet).unwrap();
        assert_eq!((request.code, request.body.as_slice()), (CODE_REQUEST, &[TYPE_IDENTITY][..]));
        assert_eq!(rig.metrics.eap_open.get(), 0, "nothing to remember yet");
    }

    #[tokio::test]
    async fn test_malformed_packet_ends_the_conversation() {
        let rig = rig();
        let Step::Challenge { state, .. } = rig.send(&identity_response(1, b"a"), None).await else { panic!() };
        assert_eq!(failure_of(&rig.send(&[CODE_RESPONSE, 3, 0, 99, TYPE_TLS], state.as_ref().map(|s| s.as_slice())).await).0, CODE_FAILURE);
        assert_eq!(rig.metrics.eap_open.get(), 0);
        assert_eq!(failure_of(&rig.send(&[CODE_RESPONSE, 3, 0, 99, TYPE_TLS], None).await).0, CODE_FAILURE, "without a State too");
    }

    #[tokio::test]
    async fn test_a_conversation_begins_with_an_identity() {
        let rig = rig();
        let response = EapPacket { code: CODE_RESPONSE, id: 1, body: vec![TYPE_TLS, 0] }.encode();
        assert_eq!(failure_of(&rig.send(&response, None).await).0, CODE_FAILURE);
        assert_eq!(rig.metrics.eap_open.get(), 0);
    }

    #[test]
    fn test_method_choice() {
        assert_eq!(choose_method(&[TYPE_TLS, 25], &[25]), Some(25), "a listed one the peer asks for");
        assert_eq!(choose_method(&[TYPE_TLS, 25], &[4, 25, TYPE_TLS]), Some(25), "the peer's order");
        assert_eq!(choose_method(&[TYPE_TLS], &[4]), None, "only EAP-MD5");
        assert_eq!(choose_method(&[TYPE_TLS], &[0]), None, "nothing");
    }

    #[tokio::test]
    async fn test_peer_asks_only_for_md5() {
        let rig = rig();
        let Step::Challenge { packet, state } = rig.send(&identity_response(1, b"a"), None).await else { panic!() };
        let id = EapPacket::parse(&packet).unwrap().id;
        let nak = EapPacket { code: CODE_RESPONSE, id, body: vec![TYPE_NAK, 4] }.encode();
        assert_eq!(failure_of(&rig.send(&nak, state.as_ref().map(|s| s.as_slice())).await).0, CODE_FAILURE);
    }

    /// EAP-AKA' is rradius-enterprise's: without it, a peer asking for type 50 asks for a method
    /// this server does not offer.
    #[tokio::test]
    async fn test_peer_asks_for_aka_prime_without_it() {
        let mut rig = rig();
        rig.settings = Arc::new(peap_settings(rig.dir.path()));
        let Step::Challenge { packet, state } = rig.send(&identity_response(1, b"a"), None).await else { panic!() };
        let start = EapPacket::parse(&packet).unwrap();
        assert_eq!(start.body[0], TYPE_PEAP);
        let nak = EapPacket { code: CODE_RESPONSE, id: start.id, body: vec![TYPE_NAK, 50] }.encode();
        assert_eq!(failure_of(&rig.send(&nak, state.as_ref().map(|s| s.as_slice())).await).0, CODE_FAILURE);
    }

    #[test]
    fn test_certificate_names() {
        let dir = tempfile::tempdir().unwrap();
        let issuer = certs::ca(dir.path(), "ca");
        certs::named_leaf(dir.path(), "alice", "alice.example.com", 20, &issuer);
        let certificate = CertificateDer::from_pem_file(dir.path().join("alice.pem")).unwrap();
        let name = |identity: &[u8], matching| certificate_name(&certificate, identity, matching).map(|(name, _)| name);
        assert_eq!(name(b"ALICE.example.com", true).unwrap(), "alice.example.com", "without regard to case, spelled as in the certificate");
        let refusal = name(b"bob.example.com", true).unwrap_err();
        assert!(refusal.contains("does not fit"));
        assert!(!refusal.contains("bob"), "what the peer sent stays out of the log");
        assert_eq!(name(b"anonymous", false).unwrap(), "alice.example.com", "the first name in it");
        assert!(name(&[0xff], true).is_err(), "not text");
    }

    fn certificate_with(alternative: Option<&str>, common_name: &str) -> CertificateDer<'static> {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(alternative.map(str::to_string).into_iter().collect::<Vec<_>>()).unwrap();
        params.distinguished_name.push(rcgen::DnType::CommonName, common_name);
        params.self_signed(&key).unwrap().der().clone()
    }

    #[test]
    fn test_subject_text_keeps_a_plain_subject() {
        let certificate = certificate_with(Some("a.example.com"), "alice");
        let (_, subject) = certificate_name(&certificate, b"a.example.com", true).unwrap();
        let (_, parsed) = X509Certificate::from_der(certificate.as_ref()).unwrap();
        assert_eq!(subject.text, parsed.subject().to_string(), "as it was");
        assert!(subject.text.contains("CN=alice"), "{}", subject.text);
        assert!(subject.attrs.contains(&("CN".to_string(), "alice".to_string())), "{:?}", subject.attrs);
    }

    /// A certificate with this `O` beside a common name.
    fn certificate_with_organization(value: rcgen::DnValue) -> CertificateDer<'static> {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["a.example.com".to_string()]).unwrap();
        params.distinguished_name.push(rcgen::DnType::OrganizationName, value);
        params.self_signed(&key).unwrap().der().clone()
    }

    #[test]
    fn test_subject_text_in_other_string_types() {
        for value in [
            rcgen::DnValue::BmpString("Example".try_into().unwrap()),
            rcgen::DnValue::TeletexString("Example".try_into().unwrap()),
            rcgen::DnValue::PrintableString("Example".try_into().unwrap()),
            rcgen::DnValue::Utf8String("Example".to_string()),
        ] {
            let certificate = certificate_with_organization(value.clone());
            let (_, subject) = certificate_name(&certificate, b"a.example.com", true).unwrap();
            assert!(subject.text.contains("O=Example"), "{value:?}: {}", subject.text);
            assert!(subject.attrs.contains(&("O".to_string(), "Example".to_string())), "{value:?}: {:?}", subject.attrs);
            let (_, parsed) = X509Certificate::from_der(certificate.as_ref()).unwrap();
            assert_eq!(subject.text, parsed.subject().to_string(), "as it was");
        }
        // Outside ASCII, and still escaped.
        let certificate = certificate_with_organization(rcgen::DnValue::BmpString("雲, OU=x".try_into().unwrap()));
        let (_, subject) = certificate_name(&certificate, b"a.example.com", true).unwrap();
        assert!(subject.text.contains(r"O=雲\, OU=x"), "{}", subject.text);
    }

    #[test]
    fn test_subject_value_that_is_not_text() {
        let certificate = certificate_with_organization(rcgen::DnValue::UniversalString("a".try_into().unwrap()));
        let (_, subject) = certificate_name(&certificate, b"a.example.com", true).unwrap();
        assert!(subject.text.contains("O=#1c0400000061"), "the tag and the length too: {}", subject.text);
        assert!(subject.attrs.contains(&("O".to_string(), "#1c0400000061".to_string())), "{:?}", subject.attrs);
    }

    #[test]
    fn test_subject_text_of_broken_values() {
        use x509_parser::der_parser::asn1_rs::{Any, Header, Tag};
        let any = |tag: Tag, data: &'static [u8]| Any::new(Header::new_simple(tag), data);
        assert_eq!(attribute_text(&any(Tag::BmpString, &[0, 0x61, 0])), None, "half a character");
        assert_eq!(attribute_text(&any(Tag::BmpString, &[0xd8, 0, 0, 0x61])), None, "half a surrogate pair");
        assert_eq!(attribute_text(&any(Tag::T61String, &[0xff])), None, "not UTF-8");
        assert_eq!(attribute_text(&any(Tag::Integer, &[5])), None);
        assert_eq!(attribute_text(&any(Tag::BmpString, &[0, 0x61])).as_deref(), Some("a"));
        for tag in [Tag::NumericString, Tag::VisibleString, Tag::PrintableString, Tag::GeneralString, Tag::ObjectDescriptor, Tag::GraphicString, Tag::T61String, Tag::VideotexString, Tag::Utf8String, Tag::Ia5String] {
            assert_eq!(attribute_text(&any(tag, b"Example")).as_deref(), Some("Example"), "{tag:?}");
        }
        assert_eq!(attribute_der_hex(&any(Tag::Integer, &[5])), "#020105");
        assert_eq!(attribute_der_hex(&any(Tag::OctetString, &[5])), "#040105", "another value than the one before");
    }

    #[test]
    fn test_subject_value_that_looks_like_another_attribute() {
        let certificate = certificate_with(Some("a.example.com"), "x, OU=admins");
        let (_, subject) = certificate_name(&certificate, b"a.example.com", true).unwrap();
        assert!(subject.text.contains(r"CN=x\, OU=admins"), "{}", subject.text);
        assert!(subject.attrs.contains(&("CN".to_string(), "x, OU=admins".to_string())), "unescaped: {:?}", subject.attrs);
        assert!(!subject.attrs.iter().any(|(name, _)| name == "OU"), "{:?}", subject.attrs);
    }

    #[test]
    fn test_subject_values_are_escaped() {
        for (value, escaped) in [
            ("a+b", r"a\+b"),
            ("a,b", r"a\,b"),
            ("a;b", r"a\;b"),
            ("a<b>", r"a\<b\>"),
            ("a\"b", r#"a\"b"#),
            (r"a\b", r"a\\b"),
            ("#a", r"\#a"),
            ("a#", "a#"),
            (" a ", r"\ a\ "),
            ("a b", "a b"),
            ("a\n", r"a\0A"),
            ("a\0b", r"a\00b"),
            ("a=b", "a=b"),
            ("雲", "雲"),
        ] {
            assert_eq!(escape_rdn_value(value), escaped, "{value:?}");
        }
    }

    #[test]
    fn test_subject_text_of_several() {
        let rdns = vec![vec![("OU".to_string(), "a".to_string()), ("CN".to_string(), "b".to_string())], vec![("OU".to_string(), "c".to_string())]];
        assert_eq!(subject_text(&rdns), "OU=a + CN=b, OU=c");
    }

    #[test]
    fn test_common_name_only_without_alternative_names() {
        let both = certificate_with(Some("alice.example.com"), "ceo@example.com");
        assert!(certificate_name(&both, b"ceo@example.com", true).is_err(), "the common name beside an alternative name");
        assert_eq!(certificate_name(&both, b"alice.example.com", true).unwrap().0, "alice.example.com");
        assert_eq!(certificate_name(&both, b"anonymous", false).unwrap().0, "alice.example.com");
        let only = certificate_with(None, "carol");
        assert_eq!(certificate_name(&only, b"CAROL", true).unwrap().0, "carol");
        assert_eq!(certificate_name(&only, b"anonymous", false).unwrap().0, "carol");
    }

    /// Whether a request is an EAP-TLS one with no TLS data, flags, or length.
    fn is_empty_request(request: &[u8]) -> bool {
        let request = EapPacket::parse(request).unwrap();
        parse_tls_fragment(&request.body[1..]).is_some_and(|fragment| fragment.data.is_empty() && request.body[1] & FLAG_START == 0)
    }

    #[tokio::test]
    async fn test_no_empty_request_when_the_peer_answers_with_data() {
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            let rig = rig();
            let mut peer = rig.peer(Some("alice"), version);
            peer.fragment_size = 16000;
            let mut step = rig.send(&identity_response(1, b"alice.example.com"), None).await;
            let mut empty = 0;
            for _ in 0..64 {
                let Step::Challenge { packet, state } = &step else { break };
                empty += usize::from(is_empty_request(packet));
                let response = peer.answer(packet);
                step = rig.send(&response, state.as_ref().map(|state| state.as_slice())).await;
            }
            assert!(matches!(step, Step::Finished(_)));
            assert_eq!(empty, 0, "{version:?}");
        }
    }

    /// A server that makes a client lead with X25519 ask again, so that it has three flights.
    fn with_retry(dir: &Path) -> Arc<rustls::ServerConfig> {
        let mut provider = rustls::crypto::ring::default_provider();
        provider.kx_groups = vec![rustls::crypto::ring::kx_group::SECP256R1];
        let provider = Arc::new(provider);
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from_pem_file(dir.join("ca.pem")).unwrap()).unwrap();
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone()).build().unwrap();
        let certificates = vec![CertificateDer::from_pem_file(dir.join("server.pem")).unwrap()];
        let key = PrivateKeyDer::from_pem_file(dir.join("server.key")).unwrap();
        let mut config = rustls::ServerConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap().with_client_cert_verifier(verifier).with_single_cert(certificates, key).unwrap();
        config.send_tls13_tickets = 0;
        Arc::new(config)
    }

    #[tokio::test]
    async fn test_every_flight_starts_with_its_length() {
        let mut rig = rig();
        rig.settings = Arc::new(EapSettings { fragment_size: 256, tls: Some(with_retry(rig.dir.path())), ..eap_settings(rig.dir.path(), None) });
        let mut peer = rig.peer(Some("alice"), &rustls::version::TLS13);
        peer.fragment_size = 16000;
        let mut step = rig.send(&identity_response(1, b"alice.example.com"), None).await;
        let (mut flight_start, mut flights) = (true, 0);
        for _ in 0..64 {
            let Step::Challenge { packet, state } = &step else { break };
            let request = EapPacket::parse(packet).unwrap();
            let fragment = parse_tls_fragment(&request.body[1..]).unwrap();
            if !fragment.data.is_empty() {
                if flight_start {
                    flights += 1;
                    assert_eq!(fragment.total.is_some(), fragment.more, "flight {flights}");
                }
                flight_start = !fragment.more;
            }
            let response = peer.answer(packet);
            step = rig.send(&response, state.as_ref().map(|state| state.as_slice())).await;
        }
        assert!(matches!(step, Step::Finished(_)));
        assert!(flights >= 3, "the retry, the certificate, the commitment: {flights}");
    }

    #[tokio::test]
    async fn test_data_in_the_middle_of_a_flight() {
        let mut rig = rig();
        rig.settings = Arc::new(EapSettings { fragment_size: 256, ..eap_settings(rig.dir.path(), None) });
        let Step::Challenge { packet, state } = rig.send(&identity_response(1, b"a"), None).await else { panic!() };
        let state = state.unwrap();
        let mut peer = rig.peer(Some("alice"), &rustls::version::TLS12);
        let hello = peer.answer(&packet);
        let Step::Challenge { packet, .. } = rig.send(&hello, Some(&state)).await else { panic!() };
        assert!(parse_tls_fragment(&EapPacket::parse(&packet).unwrap().body[1..]).unwrap().more);
        let id = EapPacket::parse(&packet).unwrap().id;
        let data = EapPacket { code: CODE_RESPONSE, id, body: vec![TYPE_TLS, 0, 22, 3, 3] }.encode();
        assert_eq!(failure_of(&rig.send(&data, Some(&state)).await).0, CODE_FAILURE);
        assert_eq!(rig.metrics.eap_open.get(), 0);
    }

    #[tokio::test]
    async fn test_timeout_stays_with_the_conversation() {
        let mut rig = rig();
        let long = rig.settings.clone();
        let short = Arc::new(EapSettings { session_timeout: Duration::from_millis(20), ..eap_settings(rig.dir.path(), None) });
        // Begun with a long timeout, a shorter one later does not end it.
        let Step::Challenge { packet, state } = rig.send(&identity_response(1, b"a"), None).await else { panic!() };
        let hello = rig.peer(Some("alice"), &rustls::version::TLS12).answer(&packet);
        rig.settings = short.clone();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(matches!(rig.send(&hello, state.as_ref().map(|s| s.as_slice())).await, Step::Challenge { .. }), "ended early");
        // Begun with a short one, a longer one later does not extend it.
        let Step::Challenge { packet, state } = rig.send(&identity_response(1, b"b"), None).await else { panic!() };
        let hello = rig.peer(Some("alice"), &rustls::version::TLS12).answer(&packet);
        rig.settings = long;
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(failure_of(&rig.send(&hello, state.as_ref().map(|s| s.as_slice())).await).0, CODE_FAILURE);
    }

    #[tokio::test]
    async fn test_sweep_needs_no_request() {
        let mut rig = rig();
        rig.settings = Arc::new(EapSettings { session_timeout: Duration::from_millis(10), ..eap_settings(rig.dir.path(), None) });
        assert!(matches!(rig.send(&identity_response(1, b"a"), None).await, Step::Challenge { .. }));
        tokio::time::sleep(Duration::from_millis(30)).await;
        rig.service.sweep(&rig.metrics);
        assert_eq!(rig.metrics.eap_open.get(), 0);
        assert_eq!(rig.metrics.eap_tls.timeout.get(), 1);
    }

    #[tokio::test]
    async fn test_incomplete_tls_message() {
        let rig = rig();
        let Step::Challenge { packet, state } = rig.send(&identity_response(1, b"a"), None).await else { panic!() };
        let id = EapPacket::parse(&packet).unwrap().id;
        // One octet of a TLS record, with nothing saying more is coming.
        let response = EapPacket { code: CODE_RESPONSE, id, body: vec![TYPE_TLS, 0, 0x16] }.encode();
        let (code, reason) = failure_of(&rig.send(&response, state.as_ref().map(|s| s.as_slice())).await);
        assert_eq!(code, CODE_FAILURE);
        assert!(reason.contains("did not finish"), "{reason}");
        assert_eq!(rig.metrics.eap_open.get(), 0);
    }

    #[tokio::test]
    async fn test_peer_asks_for_a_listed_method() {
        let rig = rig();
        let Step::Challenge { packet, state } = rig.send(&identity_response(1, b"alice.example.com"), None).await else { panic!() };
        let start = EapPacket::parse(&packet).unwrap();
        let nak = EapPacket { code: CODE_RESPONSE, id: start.id, body: vec![TYPE_NAK, 4, TYPE_TLS] }.encode();
        let Step::Challenge { packet, state: again } = rig.send(&nak, state.as_ref().map(|s| s.as_slice())).await else { panic!("no new Request") };
        let restarted = EapPacket::parse(&packet).unwrap();
        assert_ne!(restarted.id, start.id, "a new Identifier");
        assert_eq!(restarted.body, vec![TYPE_TLS, FLAG_START], "the method starts over");
        // The conversation goes on from there to the end.
        let mut peer = rig.peer(Some("alice"), &rustls::version::TLS13);
        let mut step = Step::Challenge { packet, state: again };
        for _ in 0..64 {
            let Step::Challenge { packet, state } = &step else { break };
            let response = peer.answer(packet);
            step = rig.send(&response, state.as_ref().map(|state| state.as_slice())).await;
        }
        assert!(matches!(step, Step::Finished(_)));
    }

    #[tokio::test]
    async fn test_empty_fragment_announcing_more() {
        let rig = rig();
        let Step::Challenge { packet, state } = rig.send(&identity_response(1, b"a"), None).await else { panic!() };
        let id = EapPacket::parse(&packet).unwrap().id;
        let response = EapPacket { code: CODE_RESPONSE, id, body: vec![TYPE_TLS, 0x40] }.encode();
        assert_eq!(failure_of(&rig.send(&response, state.as_ref().map(|s| s.as_slice())).await).0, CODE_FAILURE);
        assert_eq!(rig.metrics.eap_open.get(), 0);
        assert_eq!(rig.metrics.eap_tls.reject.get(), 1);
    }

    #[tokio::test]
    async fn test_too_many_rounds() {
        let rig = rig();
        let Step::Challenge { packet, state } = rig.send(&identity_response(1, b"a"), None).await else { panic!() };
        let state = state.unwrap();
        let mut peer = rig.peer(Some("alice"), &rustls::version::TLS12);
        let hello = peer.answer(&packet);
        // A repeated Response is answered without moving TLS, and still counts.
        for round in 1..MAX_ROUNDS {
            assert!(matches!(rig.send(&hello, Some(&state)).await, Step::Challenge { .. }), "round {round}");
        }
        assert_eq!(failure_of(&rig.send(&hello, Some(&state)).await).0, CODE_FAILURE);
        assert_eq!(rig.metrics.eap_open.get(), 0);
        assert_eq!(rig.metrics.eap_tls.reject.get(), 1);
    }

    #[tokio::test]
    async fn test_session_kept_busy() {
        let mut rig = rig();
        rig.settings = Arc::new(EapSettings { session_timeout: Duration::from_millis(400), ..eap_settings(rig.dir.path(), None) });
        let Step::Challenge { packet, state } = rig.send(&identity_response(1, b"a"), None).await else { panic!() };
        let state = state.unwrap();
        let mut peer = rig.peer(Some("alice"), &rustls::version::TLS12);
        let hello = peer.answer(&packet);
        let began = Instant::now();
        let ended = loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(began.elapsed() < Duration::from_secs(4), "a busy conversation went on");
            if !matches!(rig.send(&hello, Some(&state)).await, Step::Challenge { .. }) {
                break began.elapsed();
            }
        };
        assert!(ended >= Duration::from_millis(1400), "it went on while busy, until {ended:?}");
        assert_eq!(rig.metrics.eap_tls.timeout.get(), 1);
        assert_eq!(rig.metrics.eap_open.get(), 0);
    }
}
