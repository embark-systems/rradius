//! A second factor after the password: what a program's extension can offer for the clients with
//! `mfa`, such as a one-time code from an authenticator app. The password is still checked by the
//! chain; the second factor is asked before the chain runs, to take an answer to a challenge or a
//! code appended to the password, and after the chain accepts, to ask for the code or check it.
//! Clients without `mfa`, and programs without such an extension, are authenticated as before.

use crate::clients::{Client, SecondFactorMode};
use crate::common::AttrValue;
use crate::config::ServerConfig;
use crate::lua_engine::Db;
use futures::future::BoxFuture;
use zeroize::Zeroizing;

/// A request from a client with `mfa`, as the second factor sees it.
pub struct SecondFactorRequest<'a> {
    /// `radius` or `tacacs`.
    pub protocol: &'static str,
    pub client: &'a Client,
    /// How the client's users give the code. A TACACS+ PAP login is given as `Append`.
    pub mode: SecondFactorMode,
    /// The User-Name, empty without one.
    pub user: &'a str,
    /// The request's cleartext password, with any code still on it; `None` for CHAP, MS-CHAP or
    /// no password.
    pub password: Option<&'a [u8]>,
    /// RADIUS: the request's `State`, which an answer to a challenge carries.
    pub state: Option<&'a [u8]>,
}

/// What to do before the chain runs.
pub enum Before {
    /// Run the chain; with `password`, give it that instead of the request's, such as the
    /// password without the code appended to it.
    Continue { password: Option<Zeroizing<Vec<u8>>> },
    /// The request answers a challenge: answer it without running the chain.
    Answer(Decision),
    /// Refused without running the chain; `log` is written at WARN when there is one.
    Reject { log: Option<String> },
    /// Not answered, as the database could not be used: the client asks again.
    Unavailable { log: String },
}

/// What the chain accepted.
pub struct Accepted {
    /// The reply attributes the chain gave.
    pub attributes: Vec<(String, AttrValue)>,
    /// The account the chain found, if it named one.
    pub account: Option<String>,
}

/// The answer to give.
pub enum Decision {
    /// Accept with these reply attributes.
    Accept(Vec<(String, AttrValue)>),
    /// Ask for the code: RADIUS sends an Access-Challenge with `state` and `reply_message`;
    /// TACACS+ asks once more in the same session, showing `reply_message`.
    Challenge { state: Vec<u8>, reply_message: String },
    /// Refused; `log` is written at WARN when there is one.
    Reject { log: Option<String> },
    /// Not answered, as the database could not be used: the client asks again.
    Unavailable { log: String },
}

/// The second factor of the clients with `mfa`.
pub trait SecondFactor: Send + Sync {
    /// Called before the chain runs, for every request from such a client.
    fn before_chain<'a>(&'a self, request: &'a SecondFactorRequest<'a>) -> BoxFuture<'a, Before>;

    /// Called when the chain accepts such a request.
    fn after_accept<'a>(&'a self, request: &'a SecondFactorRequest<'a>, accepted: Accepted) -> BoxFuture<'a, Decision>;

    /// TACACS+: checks `code`, the answer to the question asked after a `Challenge`, which the
    /// session itself remembers with the chain's replies; those of an `Accept` are added to them.
    /// Accept, Reject or Unavailable.
    fn verify_code<'a>(&'a self, request: &'a SecondFactorRequest<'a>, code: &'a [u8]) -> BoxFuture<'a, Decision>;
}

/// What a second factor is made from, at the start and at each reload, which makes a new one.
pub struct SecondFactorContext<'a> {
    pub config: &'a ServerConfig,
    pub db: &'a Db,
}
