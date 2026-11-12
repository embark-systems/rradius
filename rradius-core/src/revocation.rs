//! Checking whether a client's certificate has been revoked, beyond the CRL the core reads
//! itself: an extension supplies the check, and the core's TLS servers call it.

use futures::future::BoxFuture;
use rustls::pki_types::CertificateDer;
use std::sync::Arc;

pub trait RevocationCheck: Send + Sync {
    /// After the handshake, where waiting is fine: `chain` is the client's, end-entity first.
    /// The error is why the client is refused.
    fn check<'a>(&'a self, chain: &'a [CertificateDer<'static>]) -> BoxFuture<'a, Result<(), String>>;

    /// For an open connection, checked again: from what is at hand only, never waiting on
    /// anything outside.
    fn recheck(self: Arc<Self>, chain: &[CertificateDer<'static>]) -> Result<(), String>;
}

/// A revocation check, as the TLS servers hold it.
pub type Revocation = Arc<dyn RevocationCheck>;

impl std::fmt::Debug for dyn RevocationCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RevocationCheck")
    }
}
