//! Resuming the TLS sessions of EAP-TLS and PEAP: an extension reads the settings of
//! `[radius.eap]` that ask for it and makes the TLS settings that resume; the core alone resumes
//! nothing, and every handshake is a full one.

use crate::lua_engine::Db;
use futures::future::BoxFuture;
use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The settings of `[radius.eap]` about resumption, as the configuration has them: `None` for
/// one it does not set.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RequestedResumption {
    pub resumption_ms: Option<u64>,
    pub shared_tickets: Option<bool>,
    pub ticket_seal_file: Option<PathBuf>,
    pub previous_ticket_seal_file: Option<PathBuf>,
}

impl RequestedResumption {
    /// The first setting that is set, in the order the refusal names them.
    pub fn first_set(&self) -> Option<&'static str> {
        [
            ("resumption_ms", self.resumption_ms.is_some()),
            ("shared_tickets", self.shared_tickets.is_some()),
            ("ticket_seal_file", self.ticket_seal_file.is_some()),
            ("previous_ticket_seal_file", self.previous_ticket_seal_file.is_some()),
        ]
        .into_iter()
        .find_map(|(name, set)| set.then_some(name))
    }
}

/// What else the settings of resumption depend on.
pub struct ResumptionContext<'a> {
    /// What relative file names are relative to.
    pub base: &'a Path,
    /// The EAP types of `radius.eap.methods`.
    pub methods: &'a [u8],
    /// `radius.eap.max_sessions`.
    pub max_sessions: usize,
}

pub trait SessionResumption: Send + Sync {
    /// EAP-TLS's TLS settings, made to resume.
    fn eap_tls(&self, config: &Arc<rustls::ServerConfig>) -> Arc<rustls::ServerConfig>;

    /// PEAP's TLS settings, made to resume.
    fn peap(&self, config: &Arc<rustls::ServerConfig>) -> Arc<rustls::ServerConfig>;

    /// Run before each EAP request is handled, for what resuming needs at hand then.
    fn prepare<'a>(&'a self, db: &'a Db) -> BoxFuture<'a, ()>;

    /// For the extension that made it, to find its own kind among the settings in use.
    fn as_any(&self) -> &dyn Any;
}

/// Session resumption, as the EAP settings hold it.
pub type Resumption = Arc<dyn SessionResumption>;

impl std::fmt::Debug for dyn SessionResumption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionResumption")
    }
}
