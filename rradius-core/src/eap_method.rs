//! EAP methods that a program's extension offers beside EAP-TLS, PEAP and TTLS. The extension
//! owns what the method says and what it looks up; the server keeps the session, the State,
//! repeats and retries, the chain's word, the record, the keys in the Access-Accept and the
//! counting.

use crate::lua_engine::Db;
use crate::metrics::Metrics;
use futures::future::BoxFuture;
use std::any::Any;
use std::sync::Arc;
use zeroize::Zeroizing;

/// What a conversation hands the server to keep for its method: what to look up, what was found,
/// or the proof to act on before the chain is asked. Only the method reads it.
pub type MethodData = Box<dyn Any + Send + Sync>;

/// The names of a method, which the configuration and the records know it by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EapMethodName {
    /// Its EAP type, such as 50.
    pub kind: u8,
    /// Its name in `radius.eap.methods`, a client's `eap_methods`, the chain's `eap_method` and
    /// the `method` label of `rradius_eap_sessions_total`, such as `aka-prime`.
    pub name: &'static str,
    /// The handler of the auth record, such as `eap-aka-prime`.
    pub handler: &'static str,
    /// What its log lines begin with, such as `EAP-AKA'`.
    pub title: &'static str,
}

/// An EAP method a program's extension offers, made at the start and at each reload. A
/// conversation keeps the method of the load it began under.
pub trait EapMethod: Send + Sync {
    fn names(&self) -> EapMethodName;

    /// A new conversation.
    fn start(&self) -> Box<dyn MethodConversation>;

    /// What a conversation asked to have looked up, run by the server outside the session's lock.
    fn lookup<'a>(&'a self, context: &'a MethodContext<'a>, request: &'a MethodData) -> BoxFuture<'a, Result<MethodData, LookupError>>;

    /// Run before the chain is asked about a peer that proved itself, with the proof its
    /// conversation gave.
    fn before_authorize<'a>(&'a self, context: &'a MethodContext<'a>, name: &'a str, proof: &'a MethodData) -> BoxFuture<'a, Result<(), NotReady>> {
        let _ = (context, name, proof);
        Box::pin(async { Ok(()) })
    }

    /// Runs on the server's timer, with the sweep of the conversations.
    fn sweep(&self) {}

    /// For the extension that made it, to find its own kind of method among those in use.
    fn as_any(&self) -> &dyn Any;
}

impl std::fmt::Debug for dyn EapMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.names().name)
    }
}

/// The state of one conversation of a method.
pub trait MethodConversation: Send + Sync {
    /// What the Request opening the method holds, after its Type.
    fn first_message(&mut self) -> Vec<u8>;

    /// The whole encoded Request that opened the method.
    fn note_request(&mut self, packet: &[u8]) {
        let _ = packet;
    }

    /// Whether the peer has said anything of the method yet: until then it can still ask for
    /// another one.
    fn started(&self) -> bool;

    /// A chain refused, and that is on record.
    fn refused(&self) -> bool;

    /// Who the record says this is about.
    fn account(&self) -> String;

    /// The chain refused `account`, and that is on record now. Returns why the conversation fails.
    fn chain_refused(&mut self, account: String) -> String;

    /// One Response: `data` is what follows the EAP Type, and `packet` the whole of it.
    fn receive(&mut self, data: &[u8], packet: &[u8]) -> MethodStep;

    /// What follows the Type in the next Request, whose Identifier is `sent`, from what the
    /// method's `lookup` found.
    fn answer(&mut self, sent: u8, found: MethodData) -> Result<Vec<u8>, String>;

    /// The keys, once the peer proved itself.
    fn msk(&self) -> Option<Zeroizing<[u8; 64]>>;
}

/// What one Response led to.
pub enum MethodStep {
    /// The method's `lookup` has to run before the conversation can go on.
    Lookup(MethodData),
    /// The peer proved it is `name`, and the chain decides; `proof` goes to `before_authorize`.
    Authorize { name: String, proof: Option<MethodData> },
    Failed(String),
}

/// Why a lookup found nothing to go on with.
#[derive(Debug)]
pub enum LookupError {
    /// An answer about the peer: the conversation fails for this.
    Failed(String),
    /// The cluster database could not be used: not answered, and the conversation waits for the
    /// same Response again.
    Unavailable(String),
    /// Over a budget of the peer's: fails as an overload, logged a line a second on a line of its
    /// own.
    OverBudget(&'static str),
    /// No place to look up in now: fails as an overload, on the line of the other overloads.
    Busy(&'static str),
}

impl From<String> for LookupError {
    fn from(reason: String) -> Self {
        Self::Failed(reason)
    }
}

impl From<&str> for LookupError {
    fn from(reason: &str) -> Self {
        Self::Failed(reason.to_string())
    }
}

/// Why `before_authorize` did not let the chain be asked.
#[derive(Debug)]
pub struct NotReady {
    /// Logged at WARN after the method's title and the client.
    pub log: String,
    /// Why the conversation fails, when it does.
    pub refusal: &'static str,
    /// For want of the cluster database: the conversation waits for the same Response again.
    pub unavailable: bool,
}

/// What a method's lookups run with.
pub struct MethodContext<'a> {
    pub db: &'a Db,
    pub metrics: &'a Metrics,
}

/// The method of `kind` among `methods`.
pub fn find(methods: &[Arc<dyn EapMethod>], kind: u8) -> Option<&Arc<dyn EapMethod>> {
    methods.iter().find(|method| method.names().kind == kind)
}

/// The names of the methods this process has met, by the extensions it installed or the methods
/// it made, for what knows a conversation only by its EAP type.
static REGISTERED: parking_lot::RwLock<Vec<EapMethodName>> = parking_lot::RwLock::new(Vec::new());

/// Notes `names`, once.
pub fn register(names: EapMethodName) {
    let mut registered = REGISTERED.write();
    if !registered.iter().any(|known| known.kind == names.kind) {
        registered.push(names);
    }
}

/// The names of the method of `kind`, if this process has met it.
pub fn registered(kind: u8) -> Option<EapMethodName> {
    REGISTERED.read().iter().find(|names| names.kind == kind).copied()
}
