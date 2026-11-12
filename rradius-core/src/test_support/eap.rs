//! What the tests of EAP over RADIUS need: a TLS client holding a test certificate, a peer that
//! speaks EAP-TLS, PEAP or TTLS the way a supplicant does, and EAP settings for a test PKI.

use crate::config::EapSettings;
use crate::eap::{encode_tls_fragment, parse_tls_fragment, tls_config, EapPacket, CODE_REQUEST, CODE_RESPONSE, FLAG_START, TYPE_IDENTITY, TYPE_TLS};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// A client for `tls_config`, holding the certificate `name` from `dir`.
pub fn client_connection(dir: &Path, name: Option<&str>, version: &'static rustls::SupportedProtocolVersion) -> ClientConnection {
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from_pem_file(dir.join("ca.pem")).unwrap()).unwrap();
    let builder = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider())).with_protocol_versions(&[version]).unwrap().with_root_certificates(roots);
    let config = match name {
        Some(name) => builder
            .with_client_auth_cert(vec![CertificateDer::from_pem_file(dir.join(format!("{name}.pem"))).unwrap()], PrivateKeyDer::from_pem_file(dir.join(format!("{name}.key"))).unwrap())
            .unwrap(),
        None => builder.with_no_client_auth(),
    };
    ClientConnection::new(Arc::new(config), ServerName::try_from("localhost").unwrap()).unwrap()
}

/// A peer speaking EAP-TLS with rustls, the way a supplicant does.
pub struct Peer {
    pub client: ClientConnection,
    incoming: Vec<u8>,
    outgoing: Vec<u8>,
    sent: usize,
    pub fragment_size: usize,
    pub refused: bool,
    pub method: u8,
    /// For PEAP and TTLS: what to answer each message inside the tunnel with.
    pub inner: Option<Inner>,
    /// For TTLS: what to say first once the handshake is over.
    pub opening: Option<Opening>,
}

pub type Inner = Box<dyn FnMut(&[u8]) -> Vec<u8> + Send>;
/// What a TTLS peer says first, once the handshake is over.
pub type Opening = Box<dyn FnOnce(&ClientConnection) -> Vec<u8> + Send>;

impl Peer {
    pub fn new(client: ClientConnection) -> Self {
        Self { client, incoming: Vec::new(), outgoing: Vec::new(), sent: 0, fragment_size: 1000, refused: false, method: TYPE_TLS, inner: None, opening: None }
    }

    fn next_fragment(&mut self) -> Vec<u8> {
        let end = (self.sent + self.fragment_size).min(self.outgoing.len());
        let more = end < self.outgoing.len();
        let total = (self.sent == 0 && more).then_some(self.outgoing.len() as u32);
        let fragment = encode_tls_fragment(&self.outgoing[self.sent..end], total, more);
        self.sent = end;
        if !more {
            self.outgoing.clear();
            self.sent = 0;
        }
        fragment
    }

    /// The Response to an EAP-Request/TLS.
    pub fn answer(&mut self, request: &[u8]) -> Vec<u8> {
        let request = EapPacket::parse(request).unwrap();
        assert_eq!(request.code, CODE_REQUEST);
        let (kind, data) = request.body.split_first().unwrap();
        assert_eq!(*kind, self.method);
        let mut data = data.to_vec();
        let body = if data == [FLAG_START] {
            self.client.write_tls(&mut self.outgoing).unwrap();
            self.next_fragment()
        } else {
            let fragment = parse_tls_fragment(&data).unwrap();
            let (more, payload) = (fragment.more, fragment.data.to_vec());
            data.clear();
            if payload.is_empty() {
                self.next_fragment()
            } else {
                self.incoming.extend_from_slice(&payload);
                if more {
                    encode_tls_fragment(&[], None, false)
                } else {
                    let flight = std::mem::take(&mut self.incoming);
                    let mut reader = flight.as_slice();
                    while !reader.is_empty() {
                        self.client.read_tls(&mut reader).unwrap();
                        if self.client.process_new_packets().is_err() {
                            self.refused = true;
                            break;
                        }
                    }
                    if !self.client.is_handshaking() {
                        if let Some(opening) = self.opening.take() {
                            let message = opening(&self.client);
                            self.client.writer().write_all(&message).unwrap();
                        }
                    }
                    if let Some(inner) = &mut self.inner {
                        let mut heard = Vec::new();
                        let _ = std::io::Read::read_to_end(&mut self.client.reader(), &mut heard);
                        if !heard.is_empty() {
                            let reply = inner(&heard);
                            self.client.writer().write_all(&reply).unwrap();
                        }
                    }
                    while self.client.wants_write() {
                        self.client.write_tls(&mut self.outgoing).unwrap();
                    }
                    self.next_fragment()
                }
            }
        };
        let mut response = vec![self.method];
        response.extend_from_slice(&body);
        EapPacket { code: CODE_RESPONSE, id: request.id, body: response }.encode()
    }
}

pub fn identity_response(id: u8, name: &[u8]) -> Vec<u8> {
    let mut body = vec![TYPE_IDENTITY];
    body.extend_from_slice(name);
    EapPacket { code: CODE_RESPONSE, id, body }.encode()
}

pub fn eap_settings(dir: &Path, crl: Option<&str>) -> EapSettings {
    EapSettings {
        methods: vec![TYPE_TLS],
        tls: Some(tls_config(&dir.join("server.pem"), &dir.join("server.key"), &dir.join("ca.pem"), crl.map(|crl| dir.join(crl)).as_deref()).unwrap()),
        tls_verifier: None,
        peap_tls: None,
        resumption: None,
        revocation: None,
        fragment_size: 1000,
        session_timeout: Duration::from_secs(30),
        tunnel_tls: None,
        extra: Vec::new(),
        max_sessions: 16,
        max_sessions_per_client: 16,
        max_sessions_per_device: 4,
        match_identity: true,
    }
}
