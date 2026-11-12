//! What EAP and RadSec share on the client's side of TLS: the CA a server's certificate is
//! checked against, or none, and the certificate the client presents.
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme, SupportedProtocolVersion};
use std::path::Path;
use std::sync::Arc;

/// A client configuration for these TLS versions. With `ca_file` the server's certificate
/// must chain to it; without, any certificate is taken. `client` is the certificate and key
/// to present. Errors name the option.
pub fn client_config(versions: &[&'static SupportedProtocolVersion], ca_file: Option<&Path>, client: Option<(&Path, &Path)>) -> Result<Arc<ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone()).with_protocol_versions(versions).map_err(|e| format!("TLS: {e}"))?;
    let builder = match ca_file {
        Some(ca_file) => {
            let mut roots = rustls::RootCertStore::empty();
            for certificate in read_certificates("--ca-file", ca_file)? {
                roots.add(certificate).map_err(|e| format!("--ca-file {}: {e}", ca_file.display()))?;
            }
            builder.with_root_certificates(roots)
        }
        None => builder.dangerous().with_custom_certificate_verifier(Arc::new(AnyCertificate(provider))),
    };
    let config = match client {
        Some((cert_file, key_file)) => {
            let certificates = read_certificates("--cert-file", cert_file)?;
            let key = PrivateKeyDer::from_pem_file(key_file).map_err(|e| format!("--key-file {}: {e}", key_file.display()))?;
            builder.with_client_auth_cert(certificates, key).map_err(|e| format!("--key-file {}: does not fit --cert-file: {e}", key_file.display()))?
        }
        None => builder.with_no_client_auth(),
    };
    Ok(Arc::new(config))
}

fn read_certificates(option: &str, path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let certificates = CertificateDer::pem_file_iter(path)
        .and_then(|certificates| certificates.collect::<Result<Vec<_>, _>>())
        .map_err(|e| format!("{option} {}: {e}", path.display()))?;
    if certificates.is_empty() {
        return Err(format!("{option} {}: no certificate in it", path.display()));
    }
    Ok(certificates)
}

/// Takes any certificate, still checking that the handshake was signed with its key: a load
/// generator talks to servers with certificates of their own making.
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, message: &[u8], certificate: &CertificateDer<'_>, signature: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, certificate, signature, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], certificate: &CertificateDer<'_>, signature: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, certificate, signature, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
