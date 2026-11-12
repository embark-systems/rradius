//! TLS for the ports and connections the server makes: server and client configurations, the
//! certificates and revocation lists they read, and which name a certificate is valid for.
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, CertificateRevocationListDer, PrivateKeyDer, ServerName};
use rustls::server::danger::ClientCertVerifier;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What a TLS configuration is made of, for what needs the parts as well.
///
/// `ocsp` names the enterprise module `ocsp`; `ocsp-crate` replaces it with an extension point
/// the core defines.
#[derive(Debug, Clone)]
pub struct ServerTls {
    pub config: Arc<rustls::ServerConfig>,
    pub verifier: Arc<dyn ClientCertVerifier>,
    /// `ocsp`: asked about each client certificate after the handshake.
    pub revocation: Option<crate::revocation::Revocation>,
}

/// A certificate for the clients that ask for this name.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedCertificate {
    pub name: String,
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
}

/// What `[radius.tls]` can ask for beyond a certificate and whom to trust.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TlsOptions {
    pub reject_expired_crl: bool,
    pub tls13_only: bool,
    pub named_certificates: Vec<NamedCertificate>,
}

/// The default certificate, and those for the names clients can ask for.
#[derive(Debug)]
struct CertificatesByName {
    default: Arc<CertifiedKey>,
    named: HashMap<String, Arc<CertifiedKey>>,
}

impl ResolvesServerCert for CertificatesByName {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        // `rustls` gives the name in lowercase.
        let named = hello.server_name().and_then(|name| self.named.get(name));
        Some(named.unwrap_or(&self.default).clone())
    }
}

pub fn certified_key(provider: &rustls::crypto::CryptoProvider, cert_file: &Path, key_file: &Path) -> Result<Arc<CertifiedKey>, String> {
    let certificates = read_certificates("cert_file", cert_file)?;
    let key = PrivateKeyDer::from_pem_file(key_file).map_err(|e| format!("key_file '{}': {}", key_file.display(), e))?;
    let signing_key = provider.key_provider.load_private_key(key).map_err(|e| format!("key_file '{}': {}", key_file.display(), e))?;
    let certified = CertifiedKey::new(certificates, signing_key);
    certified.keys_match().map_err(|e| format!("key_file '{}' does not fit cert_file '{}': {}", key_file.display(), cert_file.display(), e))?;
    Ok(Arc::new(certified))
}

fn certificates_by_name(provider: &rustls::crypto::CryptoProvider, cert_file: &Path, key_file: &Path, named_certificates: &[NamedCertificate]) -> Result<CertificatesByName, String> {
    let mut named = HashMap::new();
    for certificate in named_certificates {
        let certified = certified_key(provider, &certificate.cert_file, &certificate.key_file)?;
        if !valid_for_name(certified.end_entity_cert().ok(), &certificate.name) {
            return Err(format!("cert_file '{}' is not valid for the name '{}'", certificate.cert_file.display(), certificate.name));
        }
        if named.insert(certificate.name.to_ascii_lowercase(), certified).is_some() {
            return Err(format!("two certificates for the name '{}'", certificate.name));
        }
    }
    Ok(CertificatesByName { default: certified_key(provider, cert_file, key_file)?, named })
}

/// `server_tls_config_with` asking for nothing more.
pub fn server_tls_config(cert_file: &Path, key_file: &Path, client_ca_file: &Path, crl_file: Option<&Path>) -> Result<ServerTls, String> {
    server_tls_config_with(cert_file, key_file, client_ca_file, crl_file, &TlsOptions::default())
}

/// Builds the TLS configuration of the RadSec port: `cert_file` and `key_file`
/// identify the server, and a client must present a certificate chaining to
/// `client_ca_file`. System roots are not trusted here, since any public CA could then
/// issue a certificate that connects. Errors name the setting and the file.
pub fn server_tls_config_with(cert_file: &Path, key_file: &Path, client_ca_file: &Path, crl_file: Option<&Path>, options: &TlsOptions) -> Result<ServerTls, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let certificates = read_certificates("cert_file", cert_file)?;
    let key = PrivateKeyDer::from_pem_file(key_file).map_err(|e| format!("key_file '{}': {}", key_file.display(), e))?;
    let mut roots = rustls::RootCertStore::empty();
    for certificate in read_certificates("client_ca_file", client_ca_file)? {
        roots.add(certificate).map_err(|e| format!("client_ca_file '{}': {}", client_ca_file.display(), e))?;
    }
    let mut verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone());
    if let Some(crl_file) = crl_file {
        // An expired list is still used unless asked otherwise: refusing it would stop every
        // client at once, which is worse than missing what the next list would have added.
        verifier = verifier.with_crls(read_crls(crl_file)?);
        if options.reject_expired_crl {
            verifier = verifier.enforce_revocation_expiration();
        }
    }
    let verifier = verifier
        .build()
        .map_err(|e| match (e, crl_file) {
            (e @ rustls::server::VerifierBuilderError::InvalidCrl(_), Some(crl_file)) => format!("crl_file '{}': {}", crl_file.display(), e),
            (e, _) => format!("client_ca_file '{}': {}", client_ca_file.display(), e),
        })?;
    let versions: &[&'static rustls::SupportedProtocolVersion] = if options.tls13_only { &[&rustls::version::TLS13] } else { rustls::DEFAULT_VERSIONS };
    let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(versions)
        .map_err(|e| format!("cannot set up TLS: {e}"))?
        .with_client_cert_verifier(verifier.clone());
    let config = if options.named_certificates.is_empty() {
        builder.with_single_cert(certificates, key).map_err(|e| format!("key_file '{}' does not fit cert_file '{}': {}", key_file.display(), cert_file.display(), e))?
    } else {
        builder.with_cert_resolver(Arc::new(certificates_by_name(&provider, cert_file, key_file, &options.named_certificates)?))
    };
    Ok(ServerTls { config: Arc::new(config), verifier, revocation: None })
}

/// Builds the TLS configuration for connecting to a RadSec server, such as an upstream of
/// the proxy: its certificate must chain to `ca_file` and not be listed in `crl_file`, and
/// `client`, when given, is the certificate and key presented to it. System roots are not
/// trusted, for the same reason as on the RadSec port. The name the certificate must be
/// valid for is given when connecting. Errors name the setting and the file.
pub fn client_tls_config(ca_file: &Path, crl_file: Option<&Path>, client: Option<(&Path, &Path)>) -> Result<Arc<rustls::ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    for certificate in read_certificates("ca_file", ca_file)? {
        roots.add(certificate).map_err(|e| format!("ca_file '{}': {}", ca_file.display(), e))?;
    }
    let mut verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone());
    if let Some(crl_file) = crl_file {
        verifier = verifier.with_crls(read_crls(crl_file)?);
    }
    let verifier = verifier.build().map_err(|e| match (e, crl_file) {
        (e @ rustls::client::VerifierBuilderError::InvalidCrl(_), Some(crl_file)) => format!("crl_file '{}': {}", crl_file.display(), e),
        (e, _) => format!("ca_file '{}': {}", ca_file.display(), e),
    })?;
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(rustls::DEFAULT_VERSIONS)
        .map_err(|e| format!("cannot set up TLS: {e}"))?
        .with_webpki_verifier(verifier);
    let config = match client {
        Some((cert_file, key_file)) => {
            let certified = certified_key(&provider, cert_file, key_file)?;
            let key = PrivateKeyDer::from_pem_file(key_file).map_err(|e| format!("key_file '{}': {}", key_file.display(), e))?;
            builder.with_client_auth_cert(certified.cert.clone(), key).map_err(|e| format!("key_file '{}' does not fit cert_file '{}': {}", key_file.display(), cert_file.display(), e))?
        }
        None => builder.with_no_client_auth(),
    };
    Ok(Arc::new(config))
}

fn read_crls(path: &Path) -> Result<Vec<CertificateRevocationListDer<'static>>, String> {
    let crls = CertificateRevocationListDer::pem_file_iter(path)
        .and_then(|crls| crls.collect::<Result<Vec<_>, _>>())
        .map_err(|e| format!("crl_file '{}': {}", path.display(), e))?;
    if crls.is_empty() {
        return Err(format!("crl_file '{}' holds no certificate revocation list", path.display()));
    }
    Ok(crls)
}

/// Whether `certificate` is valid for `name`, with any wildcard in the certificate.
pub fn valid_for_name(certificate: Option<&CertificateDer<'_>>, name: &str) -> bool {
    let (Some(certificate), Ok(name)) = (certificate, ServerName::try_from(name)) else {
        return false;
    };
    webpki::EndEntityCert::try_from(certificate).is_ok_and(|parsed| parsed.verify_is_valid_for_subject_name(&name).is_ok())
}

pub fn read_certificates(setting: &str, path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let certificates = CertificateDer::pem_file_iter(path)
        .and_then(|certificates| certificates.collect::<Result<Vec<_>, _>>())
        .map_err(|e| format!("{setting} '{}': {}", path.display(), e))?;
    if certificates.is_empty() {
        return Err(format!("{setting} '{}' holds no certificate", path.display()));
    }
    Ok(certificates)
}
