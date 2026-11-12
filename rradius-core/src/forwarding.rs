//! Where a program's extension takes over a RADIUS request to send it to another server, before
//! EAP, chains and scripts would run here.

use crate::clients::Client;
use crate::config::ServerConfig;
use crate::dictionary::Dictionary;
use crate::metrics::Metrics;
use crate::radius::{PacketKind, RadiusPacket};
use futures::future::BoxFuture;
use std::any::Any;
use std::net::IpAddr;
use std::sync::Arc;

/// A request as the server received it, checked and from a known client.
pub struct ForwardRequest<'a> {
    pub kind: PacketKind,
    pub client: &'a Client,
    pub packet: &'a RadiusPacket,
    /// The packet as it came.
    pub data: &'a [u8],
    pub source: IpAddr,
}

/// What became of a request offered to the forwarder.
pub enum Route {
    /// Not one to forward: EAP, chains and scripts run here.
    Local,
    /// Dropped without an answer, as where it goes cannot be told now; the server logs the
    /// reason, a line a second at most.
    Discard(String),
    /// Another server decided: this is the answer for the client, with the account that server
    /// was asked about and the realm the request went to, for the record of the decision.
    Answered { response: Vec<u8>, account: String, realm: String },
    /// Answered without another server's decision, as when accounting was written down to send
    /// on later.
    Replied(Vec<u8>),
    /// Nothing goes back to the client.
    Unanswered,
}

/// What a program's extension forwards requests with, made again at each reload.
pub trait Forwarder: Send + Sync {
    /// Decides whether `request` goes elsewhere, and sends it there if so.
    fn route<'a>(&'a self, request: &'a ForwardRequest<'a>) -> BoxFuture<'a, Route>;

    /// Called once this forwarder is the one requests use, as at the start or after a reload.
    fn in_use(&self) {}

    /// For the extension that made it, to reach what it holds.
    fn as_any(&self) -> &dyn Any;
}

/// What a forwarder is made from, at the start and at each reload.
pub struct ForwardingContext<'a> {
    pub config: &'a ServerConfig,
    pub dictionary: &'a Arc<Dictionary>,
    pub metrics: &'a Arc<Metrics>,
}
