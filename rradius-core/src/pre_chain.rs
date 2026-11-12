//! Authenticators that run before the chain: what a program's extension can offer the RADIUS
//! server for requests it proves the identity of by itself, such as per-user pre-shared keys. The
//! identity then goes to the chain to be authorized; the auth record, the chain and the encoding of
//! the reply stay with the server.

use crate::clients::Client;
use crate::common::AttrValue;
use crate::config::ServerConfig;
use crate::lua_engine::Db;
use futures::future::BoxFuture;

/// What a pre-chain authenticator made of a request it took.
pub enum Identified {
    /// The identity it proved, for the chain to authorize; on an accept, `reply` is added to the
    /// attributes the chain gave.
    Found { identity: String, reply: Vec<(String, AttrValue)> },
    /// Refused without running a chain; `log` is written at WARN when there is one.
    Refused { log: Option<String> },
    /// Not answered, as the database could not be used; counted as discarded and logged at WARN.
    Unavailable { log: String },
    /// Not answered, without a log line: the access point asks again.
    Busy,
}

/// Proves the identity of the requests it takes, before the chain runs.
pub trait PreChainAuthenticator: Send + Sync {
    /// The name of the handler in the auth record and the method the chain is told of, such as
    /// `dpsk`.
    fn name(&self) -> &'static str;

    /// `None` when the request is not one this authenticator takes.
    fn take<'a>(&'a self, request: &'a PreChainRequest<'a>) -> Option<BoxFuture<'a, Identified>>;
}

/// An Access-Request, as a pre-chain authenticator sees it.
pub struct PreChainRequest<'a> {
    pub client: &'a Client,
    pub attributes: &'a [(String, AttrValue)],
    /// The request's User-Name, empty without one.
    pub user: &'a str,
}

/// What a pre-chain authenticator is made from, at the start and at each reload, which makes a
/// new one.
pub struct PreChainContext<'a> {
    pub config: &'a ServerConfig,
    pub db: &'a Db,
}
