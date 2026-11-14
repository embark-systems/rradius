//! The `netlogon` authentication handler: MS-CHAPv2 responses checked by a domain controller.
use super::logon::Sid;
use super::channel::{self, ChannelError, Machine, Sealed, Session};
use super::logon::{self, status, Answer, Request};
use super::{epm, rpc};
use crate::auth_chain::{Credentials, Secret};
use crate::common::{AttrValue, Outcome};
use crate::metrics::Counter;
use std::sync::{Arc, LazyLock};
use tokio::net::TcpStream;
use std::time::Duration;
use zeroize::Zeroizing;

/// The settings of a `netlogon` handler, checked when the configuration loads.
#[derive(Clone, PartialEq)]
pub struct NetlogonSettings {
    /// The NetBIOS domain name.
    pub domain: String,
    /// Domain controllers, in the order they are tried.
    pub servers: Vec<String>,
    /// The Netlogon port; `None` asks the endpoint mapper.
    pub port: Option<u16>,
    /// This node's computer account, without `$`.
    pub computer_name: String,
    /// The NT hash of the computer account's password; the password itself is not kept.
    pub machine_nt_hash: Zeroizing<[u8; 16]>,
    /// The limit of one call.
    pub timeout: Duration,
    /// The most connections kept open to one domain controller.
    pub connections: usize,
    /// The user has to be in one of these; empty asks for none.
    pub required_groups: Vec<Sid>,
    /// `(group, reply attributes)` in configuration order.
    pub group_reply: Vec<(Sid, Vec<(String, String)>)>,
}

impl std::fmt::Debug for NetlogonSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetlogonSettings")
            .field("domain", &self.domain)
            .field("servers", &self.servers)
            .field("port", &self.port)
            .field("computer_name", &self.computer_name)
            .field("machine_nt_hash", &"<secret>")
            .field("timeout", &self.timeout)
            .field("connections", &self.connections)
            .field("required_groups", &self.required_groups)
            .field("group_reply", &self.group_reply)
            .finish()
    }
}

/// What the `netlogon` handlers did, for `/metrics`. Every handler counts here, across reloads.
#[derive(Debug, Default)]
pub struct NetlogonCounters {
    pub accept: Counter,
    pub reject: Counter,
    pub no_such_user: Counter,
    pub unavailable: Counter,
    pub setups_ok: Counter,
    pub setups_failed: Counter,
    pub open: Counter,
}

static COUNTERS: LazyLock<Arc<NetlogonCounters>> = LazyLock::new(Arc::default);

/// The counters every handler reports to.
pub fn counters() -> &'static NetlogonCounters {
    &COUNTERS
}

/// A connection bound with the secure channel, counted while it is open.
struct Connection {
    sealed: Sealed<TcpStream>,
    session: Arc<Session>,
    counters: Arc<NetlogonCounters>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.counters.open.dec();
    }
}

/// One domain controller and what is kept of it between requests.
struct DomainController {
    name: String,
    /// The Netlogon port, once known.
    port: parking_lot::Mutex<Option<u16>>,
    /// Where the endpoint mapper listens.
    mapper_port: u16,
    session: parking_lot::Mutex<Option<Arc<Session>>>,
    /// Held while a secure channel is set up, so that requests waiting for one share it.
    setup: tokio::sync::Mutex<()>,
    idle: parking_lot::Mutex<Vec<Connection>>,
    /// One permit for each connection that may be open.
    slots: tokio::sync::Semaphore,
}

/// Why a domain controller gave no answer to go by.
enum Failure {
    /// It may answer on a fresh connection, after the channel is set up again.
    Retry(String),
    /// Try the next one.
    Give(String),
}

/// Whether an error says the secure channel itself is no longer good.
fn channel_gone(error: &ChannelError) -> bool {
    matches!(error, ChannelError::Seal(_) | ChannelError::Rpc(rpc::RpcError::BindRefused(_)) | ChannelError::Rpc(rpc::RpcError::Fault(_)))
}

/// Checks MS-CHAPv2 responses with the domain controllers of one domain.
pub struct NetlogonHandler {
    settings: NetlogonSettings,
    machine: Machine,
    controllers: Vec<DomainController>,
    counters: Arc<NetlogonCounters>,
}

impl NetlogonHandler {
    /// Contacts nothing: channels are set up by the first requests.
    pub fn new(settings: NetlogonSettings) -> Self {
        let ports = vec![settings.port; settings.servers.len()];
        Self::with_ports(settings, ports, epm::PORT, COUNTERS.clone())
    }

    fn with_ports(settings: NetlogonSettings, ports: Vec<Option<u16>>, mapper_port: u16, counters: Arc<NetlogonCounters>) -> Self {
        let machine = Machine { domain: settings.domain.clone(), computer: settings.computer_name.clone(), nt_hash: settings.machine_nt_hash.clone() };
        let controllers = settings
            .servers
            .iter()
            .zip(ports)
            .map(|(name, port)| DomainController {
                name: name.clone(),
                port: parking_lot::Mutex::new(port),
                mapper_port,
                session: parking_lot::Mutex::new(None),
                setup: tokio::sync::Mutex::new(()),
                idle: parking_lot::Mutex::new(Vec::new()),
                slots: tokio::sync::Semaphore::new(settings.connections),
            })
            .collect();
        Self { settings, machine, controllers, counters }
    }

    /// Decides an MS-CHAPv2 request; anything else is ignored. `account` is the name to log on
    /// as, and the name in `credentials` the one the response was computed over. The last value
    /// says no domain controller could answer.
    pub async fn authenticate(&self, credentials: &Credentials, account: Option<&str>) -> (Outcome, Vec<(String, AttrValue)>, bool) {
        let Secret::MsChapV2 { ident, peer_challenge, response, challenge } = &credentials.secret else {
            return (Outcome::Ignore, Vec::new(), false);
        };
        let (Some(user), Some(account)) = (credentials.user.as_deref(), account) else {
            return (Outcome::Ignore, Vec::new(), false);
        };
        // The domain is the configured one, whatever the name says.
        let account = account.rsplit_once('\\').map_or(account, |(_, name)| name);
        let challenge_hash = crate::mschap::challenge_hash(peer_challenge, challenge, user);
        let Some((server, answer)) = self.ask(account, &challenge_hash, response).await else {
            crate::lua_engine::log_line(tracing::Level::ERROR, format!("auth netlogon handler: no domain controller of {} could answer; rejecting", self.settings.domain));
            self.counters.unavailable.inc();
            return (Outcome::Reject, Vec::new(), true);
        };
        let reject = |reason: String| {
            tracing::info!(account, "auth netlogon handler: {server}: {reason}; rejecting");
            self.counters.reject.inc();
            (Outcome::Reject, Vec::new(), false)
        };
        match answer.status {
            status::SUCCESS => {}
            status::NO_SUCH_USER => {
                self.counters.no_such_user.inc();
                return (Outcome::Ignore, Vec::new(), false);
            }
            other => return reject(status::name(other).map_or_else(|| format!("status 0x{other:08X}"), str::to_string)),
        }
        let Some(validation) = answer.validation else {
            return reject("success without validation information".to_string());
        };
        if validation.is_guest() {
            return reject("logged on as the guest account".to_string());
        }
        let Some(proof) = crate::mschap::proof_from_session_key(&validation.user_session_key, response, &challenge_hash) else {
            return reject("an empty user session key".to_string());
        };
        let groups = validation.group_sids();
        if !self.settings.required_groups.is_empty() && !self.settings.required_groups.iter().any(|group| groups.contains(group)) {
            return reject("not in any of required_groups".to_string());
        }
        let mut attributes = crate::auth_chain::mschapv2_success(*ident, &proof);
        for (group, reply) in &self.settings.group_reply {
            if groups.contains(group) {
                attributes.extend(reply.iter().map(|(name, value)| (name.clone(), AttrValue::Bytes(value.clone().into_bytes()))));
            }
        }
        self.counters.accept.inc();
        (Outcome::Accept, attributes, false)
    }

    /// The first authoritative answer, in the order of `servers`, and who gave it.
    async fn ask(&self, account: &str, challenge: &[u8; 8], response: &[u8; 24]) -> Option<(&str, Answer)> {
        for controller in &self.controllers {
            let request = Request {
                server: &controller.name,
                computer: &self.settings.computer_name,
                domain: &self.settings.domain,
                account,
                workstation: "",
                challenge: *challenge,
                nt_response: *response,
            };
            match self.ask_one(controller, &request).await {
                Ok(answer) if answer.authoritative => return Some((&controller.name, answer)),
                Ok(_) => tracing::warn!("auth netlogon handler: {}: the answer is not authoritative; trying the next domain controller", controller.name),
                Err(reason) => tracing::warn!("auth netlogon handler: {}: {reason}", controller.name),
            }
        }
        None
    }

    /// One logon with `controller`: on an idle connection, or a fresh one; once more on a fresh
    /// connection, after setting the channel up again if it was the trouble.
    async fn ask_one(&self, controller: &DomainController, request: &Request<'_>) -> Result<Answer, String> {
        let timeout = self.settings.timeout;
        let _slot = tokio::time::timeout(timeout, controller.slots.acquire())
            .await
            .map_err(|_| format!("no connection free within {} ms", timeout.as_millis()))?
            .map_err(|e| e.to_string())?;
        let mut reason = String::new();
        for fresh in [false, true] {
            let idle = if fresh { None } else { controller.idle.lock().pop() };
            let mut connection = match idle {
                Some(connection) => connection,
                None => match tokio::time::timeout(timeout, self.connect(controller)).await {
                    Ok(Ok(connection)) => connection,
                    Ok(Err(Failure::Retry(why))) if !fresh => {
                        reason = why;
                        continue;
                    }
                    Ok(Err(Failure::Retry(why) | Failure::Give(why))) => return Err(why),
                    Err(_) => return Err(format!("no connection within {} ms", timeout.as_millis())),
                },
            };
            match tokio::time::timeout(timeout, logon::logon(&mut connection.sealed, request)).await {
                Err(_) => return Err(format!("no answer within {} ms", timeout.as_millis())),
                Ok(Ok(answer)) if answer.status == status::ACCESS_DENIED => {
                    forget_session(controller, &connection.session);
                    reason = "the domain controller refused the secure channel (STATUS_ACCESS_DENIED)".to_string();
                }
                Ok(Ok(answer)) => {
                    controller.idle.lock().push(connection);
                    return Ok(answer);
                }
                Ok(Err(e)) => {
                    if channel_gone(&e) {
                        forget_session(controller, &connection.session);
                    }
                    reason = e.to_string();
                }
            }
        }
        Err(reason)
    }

    /// A new connection bound with the secure channel, setting the channel up if there is none.
    async fn connect(&self, controller: &DomainController) -> Result<Connection, Failure> {
        let session = self.session(controller).await?;
        let port = self.port(controller).await.map_err(Failure::Give)?;
        let stream = TcpStream::connect((controller.name.as_str(), port)).await.map_err(|e| Failure::Give(format!("cannot connect to port {port}: {e}")))?;
        let _ = stream.set_nodelay(true);
        match Sealed::bind(stream, session.clone(), &self.machine).await {
            Ok(sealed) => {
                self.counters.open.inc();
                Ok(Connection { sealed, session, counters: self.counters.clone() })
            }
            Err(e) => {
                if channel_gone(&e) {
                    forget_session(controller, &session);
                }
                Err(Failure::Retry(e.to_string()))
            }
        }
    }

    /// The controller's secure channel, set up now if it has none.
    async fn session(&self, controller: &DomainController) -> Result<Arc<Session>, Failure> {
        if let Some(session) = controller.session.lock().clone() {
            return Ok(session);
        }
        let _setup = controller.setup.lock().await;
        if let Some(session) = controller.session.lock().clone() {
            return Ok(session);
        }
        let port = self.port(controller).await.map_err(Failure::Give)?;
        let result = async {
            let mut stream = TcpStream::connect((controller.name.as_str(), port)).await.map_err(|e| ChannelError::Rpc(e.into()))?;
            channel::authenticate(&mut stream, &controller.name, &self.machine).await
        }
        .await;
        match result {
            Ok(session) => {
                self.counters.setups_ok.inc();
                let session = Arc::new(session);
                *controller.session.lock() = Some(session.clone());
                Ok(session)
            }
            Err(e) => {
                self.counters.setups_failed.inc();
                let reason = format!("cannot set up the secure channel as {}$: {e}", self.machine.computer);
                if matches!(e, ChannelError::Refused(_) | ChannelError::Unsupported(_) | ChannelError::ServerCredential) {
                    // Nothing that a retry mends: the computer account or the controller needs fixing.
                    crate::lua_engine::log_line(tracing::Level::ERROR, format!("auth netlogon handler: {}: {reason}", controller.name));
                }
                Err(Failure::Give(reason))
            }
        }
    }

    /// The Netlogon port: configured, or asked of the endpoint mapper once.
    async fn port(&self, controller: &DomainController) -> Result<u16, String> {
        if let Some(port) = *controller.port.lock() {
            return Ok(port);
        }
        let mut stream = TcpStream::connect((controller.name.as_str(), controller.mapper_port))
            .await
            .map_err(|e| format!("cannot reach the endpoint mapper on port {}: {e}", controller.mapper_port))?;
        let port = epm::map(&mut stream, rpc::NETLOGON).await.map_err(|e| format!("the endpoint mapper: {e}"))?;
        *controller.port.lock() = Some(port);
        Ok(port)
    }
}

/// Drops the controller's secure channel if it is still `session`, along with the idle
/// connections bound with it; a newer one set up meanwhile stays.
fn forget_session(controller: &DomainController, session: &Arc<Session>) {
    let mut current = controller.session.lock();
    if current.as_ref().is_some_and(|current| Arc::ptr_eq(current, session)) {
        *current = None;
        drop(current);
        controller.idle.lock().retain(|connection| !Arc::ptr_eq(&connection.session, session));
    }
}

/// A handler with its own test domain controller, for tests here and elsewhere.
#[cfg(test)]
pub mod test_support {
    use super::*;
    use crate::netlogon::channel::test_dc::{self, Behaviour, State};
    use crate::netlogon::logon::test_dc::{directory, Quirks};

    pub struct Rig {
        pub handler: NetlogonHandler,
        pub state: Arc<parking_lot::Mutex<State>>,
        pub quirks: Arc<Quirks>,
        pub counters: Arc<NetlogonCounters>,
    }

    /// One controller on 127.0.0.1, port given, computer account RRADIUS1 with `machine-pw`.
    pub fn settings(servers: usize) -> NetlogonSettings {
        NetlogonSettings {
            domain: "CORP".to_string(),
            servers: vec!["127.0.0.1".to_string(); servers],
            port: None,
            computer_name: "RRADIUS1".to_string(),
            machine_nt_hash: crate::mschap::nt_password_hash("machine-pw"),
            timeout: Duration::from_secs(2),
            connections: 4,
            required_groups: Vec::new(),
            group_reply: Vec::new(),
        }
    }

    /// A test domain controller where alice has `password`.
    pub async fn domain_controller(password: &'static str) -> Result<(u16, Arc<parking_lot::Mutex<State>>, Arc<Quirks>), String> {
        let quirks = Arc::new(Quirks::default());
        let (port, state) = test_dc::start(Behaviour::new("machine-pw"), directory(password, quirks.clone())).await.map_err(|e| e.to_string())?;
        Ok((port, state, quirks))
    }

    /// A handler whose one controller knows alice with `password`.
    pub async fn rig(password: &'static str, adjust: impl FnOnce(&mut NetlogonSettings)) -> Result<Rig, String> {
        let (port, state, quirks) = domain_controller(password).await?;
        let mut settings = settings(1);
        adjust(&mut settings);
        let counters = Arc::new(NetlogonCounters::default());
        let handler = NetlogonHandler::with_ports(settings, vec![Some(port)], epm::PORT, counters.clone());
        Ok(Rig { handler, state, quirks, counters })
    }

    /// The handler in a first-match chain of its own.
    pub async fn chain(password: &'static str) -> Result<crate::auth_chain::AuthChain, String> {
        let rig = rig(password, |_| {}).await?;
        Ok(crate::auth_chain::AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![crate::auth_chain::Handler::Netlogon(rig.handler)]))
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{domain_controller, rig, settings};
    use super::*;
    use crate::netlogon::logon::test_dc::domain_sid;
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    const PEER: [u8; 16] = [0x21; 16];
    const AUTHENTICATOR: [u8; 16] = [0x5B; 16];

    fn mschapv2(user: &str, password: &str) -> Credentials {
        let response = crate::mschap::response_for(password, user.rsplit('\\').next().unwrap_or(user), &PEER, &AUTHENTICATOR);
        Credentials { user: Some(user.to_string()), secret: Secret::MsChapV2 { ident: 7, peer_challenge: PEER, response, challenge: AUTHENTICATOR } }
    }

    async fn decide(handler: &NetlogonHandler, user: &str, password: &str) -> (Outcome, Vec<(String, AttrValue)>, bool) {
        handler.authenticate(&mschapv2(user, password), Some(user)).await
    }

    fn names(attributes: &[(String, AttrValue)]) -> Vec<&str> {
        attributes.iter().map(|(name, _)| name.as_str()).collect()
    }

    /// A port nothing listens on.
    async fn closed_port() -> Result<u16, String> {
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
        listener.local_addr().map(|a| a.port()).map_err(|e| e.to_string())
    }

    /// A port that accepts connections and never says anything.
    async fn silent_port() -> Result<u16, String> {
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        Ok(port)
    }

    fn make(ports: Vec<Option<u16>>, adjust: impl FnOnce(&mut NetlogonSettings)) -> (NetlogonHandler, Arc<NetlogonCounters>) {
        let mut settings = settings(ports.len());
        adjust(&mut settings);
        let counters = Arc::new(NetlogonCounters::default());
        (NetlogonHandler::with_ports(settings, ports, epm::PORT, counters.clone()), counters)
    }

    /// Scenario: Correct password, with the same proof a stored password gives; the
    /// connection is reused.
    #[tokio::test]
    async fn test_correct_password() -> Result<(), String> {
        let rig = rig("clientPass", |_| {}).await?;
        let credentials = mschapv2("alice", "clientPass");
        let (outcome, attributes, unavailable) = rig.handler.authenticate(&credentials, Some("alice")).await;
        assert_eq!((outcome, unavailable), (Outcome::Accept, false));
        let local = crate::auth_chain::verify_secret("clientPass", "alice", &credentials.secret).await.ok_or("local verify failed")?;
        assert_eq!(attributes, local);
        assert_eq!(names(&attributes), ["MS-CHAP2-Success", "MS-MPPE-Send-Key", "MS-MPPE-Recv-Key"]);
        // A second logon goes over the same channel and connection.
        let (outcome, ..) = decide(&rig.handler, "CORP\\alice", "clientPass").await;
        assert_eq!(outcome, Outcome::Accept);
        assert_eq!(rig.state.lock().calls.iter().filter(|(opnum, _)| *opnum == logon::OPNUM_SAM_LOGON_EX).count(), 2);
        let counters = &rig.counters;
        assert_eq!((counters.accept.get(), counters.setups_ok.get(), counters.open.get()), (2, 1, 1));
        drop(rig.handler);
        assert_eq!(counters.open.get(), 0);
        Ok(())
    }

    /// Scenarios: Wrong password, Disabled account, Guest logon, Unknown user, PAP request.
    #[tokio::test]
    async fn test_account_status() -> Result<(), String> {
        let rig = rig("Secret1", |_| {}).await?;
        for (user, password, expected) in [
            ("alice", "Secret2", Outcome::Reject),
            ("carol", "Secret1", Outcome::Reject),
            ("guest", "Secret1", Outcome::Reject),
            ("bob", "Secret1", Outcome::Ignore),
        ] {
            let (outcome, attributes, unavailable) = decide(&rig.handler, user, password).await;
            assert_eq!((outcome, attributes.len(), unavailable), (expected, 0, false), "{user}");
        }
        assert_eq!((rig.counters.reject.get(), rig.counters.no_such_user.get(), rig.counters.accept.get()), (3, 1, 0));
        let calls = rig.state.lock().calls.len();
        let pap = Credentials { user: Some("alice".to_string()), secret: Secret::pap("Secret1") };
        assert_eq!(rig.handler.authenticate(&pap, Some("alice")).await.0, Outcome::Ignore);
        assert_eq!(rig.state.lock().calls.len(), calls, "a PAP request reached the domain controller");
        assert_eq!(status::name(status::WRONG_PASSWORD), Some("STATUS_WRONG_PASSWORD"));
        Ok(())
    }

    /// Scenarios: Not in a required group, Group reply.
    #[tokio::test]
    async fn test_groups() -> Result<(), String> {
        let staff = domain_sid().with_rid(1110);
        let other: Sid = "S-1-5-21-1-2-3-1105".parse()?;
        let refused = rig("Secret1", |settings| settings.required_groups = vec![other]).await?;
        assert_eq!(decide(&refused.handler, "alice", "Secret1").await.0, Outcome::Reject);
        let reply = vec![
            ("S-1-5-21-1-2-3-1106".parse::<Sid>()?, vec![("Filter-Id".to_string(), "other".to_string())]),
            (staff.clone(), vec![("Filter-Id".to_string(), "staff".to_string())]),
            ("S-1-5-21-1-2-3-1200".parse::<Sid>()?, vec![("Class".to_string(), "extra".to_string())]),
        ];
        let member = rig("Secret1", |settings: &mut NetlogonSettings| {
            settings.required_groups = vec![staff.clone()];
            settings.group_reply = reply;
        })
        .await?;
        let (outcome, attributes, _) = decide(&member.handler, "alice", "Secret1").await;
        assert_eq!(outcome, Outcome::Accept);
        assert_eq!(&attributes[3..], &[("Filter-Id".to_string(), AttrValue::Bytes(b"staff".to_vec())), ("Class".to_string(), AttrValue::Bytes(b"extra".to_vec()))]);
        Ok(())
    }

    /// Scenario: First domain controller down; then one that does not answer, and one that is not
    /// authoritative.
    #[tokio::test]
    async fn test_next_domain_controller() -> Result<(), String> {
        let (port, _, quirks) = domain_controller("Secret1").await?;
        let (handler, counters) = make(vec![Some(closed_port().await?), Some(port)], |_| {});
        assert_eq!(decide(&handler, "alice", "Secret1").await.0, Outcome::Accept);
        assert_eq!((counters.setups_failed.get(), counters.setups_ok.get()), (1, 1));

        let (handler, _) = handler_with_timeout(vec![Some(silent_port().await?), Some(port)]);
        let started = std::time::Instant::now();
        assert_eq!(decide(&handler, "alice", "Secret1").await.0, Outcome::Accept);
        assert!(started.elapsed() < Duration::from_secs(2));

        let (other, _, _) = domain_controller("Secret1").await?;
        quirks.not_authoritative.store(true, Ordering::Relaxed);
        let (handler, counters) = make(vec![Some(port), Some(other)], |_| {});
        assert_eq!(decide(&handler, "alice", "Secret1").await.0, Outcome::Accept);
        assert_eq!(counters.setups_ok.get(), 2);
        Ok(())
    }

    fn handler_with_timeout(ports: Vec<Option<u16>>) -> (NetlogonHandler, Arc<NetlogonCounters>) {
        make(ports, |settings| settings.timeout = Duration::from_millis(200))
    }

    /// Scenario: Channel reset by the domain controller.
    #[tokio::test]
    async fn test_channel_set_up_again() -> Result<(), String> {
        let rig = rig("Secret1", |_| {}).await?;
        assert_eq!(decide(&rig.handler, "alice", "Secret1").await.0, Outcome::Accept);
        rig.quirks.deny_once.store(true, Ordering::Relaxed);
        assert_eq!(decide(&rig.handler, "alice", "Secret1").await.0, Outcome::Accept);
        assert_eq!((rig.counters.setups_ok.get(), rig.counters.open.get()), (2, 1));
        Ok(())
    }

    /// Scenario: All down, and a call that gets no answer in time.
    #[tokio::test]
    async fn test_all_down() -> Result<(), String> {
        let (handler, counters) = handler_with_timeout(vec![Some(closed_port().await?), Some(silent_port().await?)]);
        let (outcome, attributes, unavailable) = decide(&handler, "alice", "Secret1").await;
        assert_eq!((outcome, attributes.len(), unavailable), (Outcome::Reject, 0, true));
        assert_eq!((counters.unavailable.get(), counters.accept.get(), counters.open.get()), (1, 0, 0));
        Ok(())
    }

    /// Without a port, the endpoint mapper is asked once.
    #[tokio::test]
    async fn test_endpoint_mapper() -> Result<(), String> {
        let (port, _, _) = domain_controller("Secret1").await?;
        let mapper = TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
        let mapper_port = mapper.local_addr().map_err(|e| e.to_string())?.port();
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = asked.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = mapper.accept().await {
                counted.fetch_add(1, Ordering::Relaxed);
                let _ = async {
                    let bind = rpc::parse(&rpc::read_pdu(&mut stream).await?)?;
                    stream.write_all(&rpc::server::bind_ack(bind.call_id, None)).await?;
                    let request = rpc::parse(&rpc::read_pdu(&mut stream).await?)?;
                    let flags = rpc::PFC_FIRST_FRAG | rpc::PFC_LAST_FRAG;
                    stream.write_all(&rpc::server::response(request.call_id, flags, &epm::map_response(port), None, |_| Vec::new())).await?;
                    Ok::<_, rpc::RpcError>(())
                }
                .await;
            }
        });
        let counters = Arc::new(NetlogonCounters::default());
        let handler = NetlogonHandler::with_ports(settings(1), vec![None], mapper_port, counters);
        for _ in 0..2 {
            assert_eq!(decide(&handler, "alice", "Secret1").await.0, Outcome::Accept);
        }
        assert_eq!(asked.load(Ordering::Relaxed), 1);
        Ok(())
    }

    struct LogWriter(Arc<parking_lot::Mutex<Vec<u8>>>);

    impl std::io::Write for LogWriter {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn hex(octets: &[u8]) -> String {
        octets.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// Scenario: Trace logging; and Wrong password names its status.
    #[tokio::test]
    async fn test_secrets_stay_out_of_logs() -> Result<(), String> {
        let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let written = log.clone();
        let subscriber = tracing_subscriber::fmt().with_max_level(tracing::Level::TRACE).with_ansi(false).with_writer(move || LogWriter(written.clone())).finish();
        let _default = tracing::subscriber::set_default(subscriber);
        // As in test_support::radius::logged_during: a second dispatcher keeps callsites that
        // other tests reach first from being cached as never enabled.
        let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());

        let rig = rig("Secret1", |_| {}).await?;
        let credentials = mschapv2("alice", "Secret1");
        let (outcome, attributes, _) = rig.handler.authenticate(&credentials, Some("alice")).await;
        assert_eq!(outcome, Outcome::Accept);
        let mut secrets: Vec<Vec<u8>> = attributes.iter().filter(|(name, _)| name.starts_with("MS-MPPE-")).filter_map(|(_, value)| match value {
            AttrValue::Bytes(key) => Some(key.clone()),
            _ => None,
        }).collect();
        assert_eq!(secrets.len(), 2);
        let session = rig.state.lock().session.clone().ok_or("no session")?;
        secrets.push(session.key.to_vec());
        rig.quirks.deny_once.store(true, Ordering::Relaxed);
        assert_eq!(decide(&rig.handler, "alice", "Secret2").await.0, Outcome::Reject);
        let session = rig.state.lock().session.clone().ok_or("no session")?;
        secrets.push(session.key.to_vec());
        let Secret::MsChapV2 { response, .. } = &credentials.secret else {
            return Err("not MS-CHAPv2".to_string());
        };
        secrets.push(response.to_vec());
        secrets.push(crate::mschap::challenge_hash(&PEER, &AUTHENTICATOR, "alice").to_vec());
        secrets.push(crate::mschap::hash_nt_password_hash(&crate::mschap::nt_password_hash("Secret1")).to_vec());
        secrets.push(crate::mschap::nt_password_hash("machine-pw").to_vec());
        // A computer account the controller refuses, and a controller that is down.
        let (port, _, _) = domain_controller("Secret1").await?;
        let (refused, _) = make(vec![Some(port), Some(closed_port().await?)], |settings| settings.machine_nt_hash = crate::mschap::nt_password_hash("other-pw"));
        assert_eq!(decide(&refused, "alice", "Secret1").await, (Outcome::Reject, Vec::new(), true));

        let text = String::from_utf8_lossy(&log.lock()).into_owned();
        assert!(text.contains("STATUS_WRONG_PASSWORD"), "{text}");
        assert!(text.contains("STATUS_ACCESS_DENIED") && text.contains("RRADIUS1$"), "{text}");
        for word in ["machine-pw", "other-pw", "Secret1", "Secret2"] {
            assert!(!text.contains(word), "{word} in {text}");
        }
        for secret in &secrets {
            for form in [hex(secret), hex(secret).to_uppercase(), format!("{secret:?}")] {
                assert!(!text.contains(&form), "{form} in {text}");
            }
        }
        Ok(())
    }

    #[test]
    fn test_metrics_families() {
        let text = crate::metrics::Metrics::default().render();
        for line in [
            "rradius_netlogon_logons_total{result=\"unavailable\"} ",
            "rradius_netlogon_channel_setups_total{result=\"failed\"} ",
            "rradius_netlogon_open_connections ",
        ] {
            assert!(text.contains(line), "{line}");
        }
    }

    /// At most `connections` connections, however many requests come at once.
    #[tokio::test]
    async fn test_connection_limit() -> Result<(), String> {
        let rig = rig("Secret1", |settings| settings.connections = 2).await?;
        let handler = Arc::new(rig.handler);
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let handler = handler.clone();
                tokio::spawn(async move { decide(&handler, "alice", "Secret1").await.0 })
            })
            .collect();
        for task in tasks {
            assert_eq!(task.await.map_err(|e| e.to_string())?, Outcome::Accept);
        }
        assert!(rig.counters.open.get() <= 2, "{} open", rig.counters.open.get());
        assert_eq!(rig.counters.setups_ok.get(), 1);
        Ok(())
    }
}
