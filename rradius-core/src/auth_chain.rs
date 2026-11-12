//! Ordered authentication handlers configured in `[auth]`: built-in SQL user
//! lookup with password verification, LDAP directories, and Lua scripts for
//! site-specific rules.
use crate::clients::Client;
use crate::config::{AuthHandlerSettings, AuthPolicy, AuthSettings, ChainSettings, StripRealm, UsernameSettings};
use crate::ldap_handler::LdapHandler;
use crate::lua_engine::{log_line, Db, DbValue, LuaAuthEngine, LuaRequest, LuaVms};
use crate::common::{AttrValue, Outcome};
use crate::{mschap, passwords};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// The secret a request proves the user's identity with.
#[derive(Clone, PartialEq)]
pub enum Secret {
    Pap(zeroize::Zeroizing<Vec<u8>>),
    Chap { id: u8, response: Vec<u8>, challenge: Vec<u8> },
    MsChapV2 { ident: u8, peer_challenge: [u8; 16], response: [u8; 24], challenge: [u8; 16] },
    /// EAP-TLS has verified a certificate for this name. Nothing but finishing that handshake
    /// makes one of these: no attribute of a request can.
    Certificate,
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Credentials {
    pub user: Option<String>,
    pub secret: Secret,
}

/// Overwrites the session keys among reply attributes, when they have gone out or never will.
pub(crate) fn wipe_keys(attributes: &mut [(String, AttrValue)]) {
    for (name, value) in attributes {
        // DPSK replies carry the PMK in these.
        let key_holder = name.starts_with("MS-MPPE-") || name.eq_ignore_ascii_case("Tunnel-Password") || name.eq_ignore_ascii_case("Ruckus-DPSK");
        if let (true, AttrValue::Bytes(key)) = (key_holder, value) {
            zeroize::Zeroize::zeroize(key);
        }
    }
}

/// What the handlers of `all_accept` said so far, which may hold keys and may never be sent:
/// a later handler can still refuse, and there are several ways out.
struct Collected(Vec<(String, AttrValue)>);

impl Drop for Collected {
    fn drop(&mut self) {
        wipe_keys(&mut self.0);
    }
}

impl Secret {
    /// The password of a PAP request.
    pub fn pap(password: impl Into<Vec<u8>>) -> Self {
        Self::Pap(zeroize::Zeroizing::new(password.into()))
    }
}

/// Without what it holds: this may reach a log, and a password must not.
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Pap(_) => "Pap",
            Self::Chap { .. } => "Chap",
            Self::MsChapV2 { .. } => "MsChapV2",
            Self::Certificate => "Certificate",
            Self::None => "None",
        })
    }
}

/// The chain's verdict and, when accepted, the reply attributes of every
/// accepting handler in run order.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthResult {
    /// `Accept`, `Reject` or `Challenge`: handlers that ignored leave no trace here.
    pub decision: Outcome,
    pub attributes: Vec<(String, AttrValue)>,
    /// What decided: a handler's kind, `none` when every handler ignored the request,
    /// `no_password_check` when `all_accept` found nobody had compared a password, or
    /// `normalization` when the chain refused the name.
    pub handler: &'static str,
    /// The name the handlers looked up.
    pub account: Option<String>,
    /// Whether a handler that accepted is one that compares passwords. An accept from a script
    /// that only says whether an account is active does not mean a password was right.
    pub verified: bool,
    /// Not accepted because the cluster database could not be used: no answer is right, so that
    /// the request comes again or goes to another server, not a refusal.
    pub unavailable: bool,
}

impl AuthResult {
    pub fn accepted(&self) -> bool {
        self.decision == Outcome::Accept
    }
}

// Built once at startup and never moved around, so size does not matter here.
#[allow(clippy::large_enum_variant)]
pub enum Handler {
    Sql {
        db: Db,
        user_query: String,
        /// What a verified certificate is looked up with instead.
        certificate_query: Option<String>,
        reply_query: Option<String>,
        default_user: Option<String>,
        timeout: Duration,
    },
    Lua(LuaAuthEngine),
    /// A script that compares passwords, said so in its settings: see `verifies_password`.
    LuaVerifying(LuaAuthEngine),
    Ldap(LdapHandler),
}

pub struct AuthChain {
    policy: AuthPolicy,
    handlers: Vec<Handler>,
    username: Option<UsernameSettings>,
}

impl AuthChain {
    pub fn new(policy: AuthPolicy, handlers: Vec<Handler>) -> Self {
        Self { policy, handlers, username: None }
    }

    /// Whether a handler compares passwords, so that a secret a script cannot check, such as a
    /// CHAP response, can be decided here.
    pub fn compares_passwords(&self) -> bool {
        self.handlers.iter().any(Handler::verifies_password)
    }

    /// Derives account names from received names as `username` says.
    pub fn with_username(mut self, username: Option<UsernameSettings>) -> Self {
        self.username = username;
        self
    }

    /// Builds one chain from `[auth]`, reading and compiling each Lua handler's
    /// script onto the shared `vms`, and reading each LDAP handler's CA file.
    /// `lua` holds the database and execution limits for Lua handlers; SQL and
    /// LDAP handlers carry their own limits.
    pub fn from_settings(settings: &ChainSettings, db: &Db, vms: &Arc<LuaVms>, lua: &LuaLimits) -> Result<Self, String> {
        let mut handlers = Vec::with_capacity(settings.handlers.len());
        for handler in &settings.handlers {
            handlers.push(match handler {
                AuthHandlerSettings::Sql { user_query, certificate_query, reply_query, default_user, timeout } => Handler::Sql {
                    db: db.clone(),
                    user_query: user_query.clone(),
                    certificate_query: certificate_query.clone(),
                    reply_query: reply_query.clone(),
                    default_user: default_user.clone(),
                    timeout: *timeout,
                },
                AuthHandlerSettings::Lua { script, verifies_password } => {
                    let source = std::fs::read_to_string(script)
                        .map_err(|e| format!("cannot read auth handler script '{}': {}", script.display(), e))?;
                    let engine = LuaAuthEngine::new(vms.clone(), db.clone(), source)
                        .map_err(|e| format!("cannot compile auth handler script '{}': {}", script.display(), e))?;
                    let engine = engine.with_script_name(script.display().to_string()).with_query_timeout(lua.query_timeout).with_max_execution(lua.max_execution).with_dynamic_auth(lua.dynamic_auth.clone()).with_script_http(lua.http.clone());
                    if *verifies_password {
                        Handler::LuaVerifying(engine)
                    } else {
                        Handler::Lua(engine)
                    }
                }
                AuthHandlerSettings::Ldap(ldap) => Handler::Ldap(LdapHandler::new(ldap.clone())?),
            });
        }
        Ok(Self::new(settings.policy, handlers).with_username(settings.username.clone()))
    }

    /// Runs the handlers according to the policy. `request` is what Lua
    /// handlers see (phase `authentication`).
    pub async fn authenticate(&self, credentials: &Credentials, request: &LuaRequest) -> AuthResult {
        let received = credentials.user.as_deref();
        let normalized = match (&self.username, received) {
            (Some(settings), Some(name)) => match account_name(settings, name).and_then(|account| {
                // The certificate proves the whole name, and handlers ask for no other proof.
                // ASCII only: Unicode lowercase makes the Kelvin sign a K, and so another account.
                match credentials.secret == Secret::Certificate && !account.eq_ignore_ascii_case(name) {
                    true => Err("normalization changes a certificate's name in more than case"),
                    false => Ok(account),
                }
            }) {
                Ok(account) => Some(account),
                Err(reason) => {
                    // Not the name itself: it comes from outside and can hold control characters.
                    log_line(tracing::Level::WARN, format!("auth chain: rejecting a request from client {}: {}", request.client, reason));
                    return AuthResult { decision: Outcome::Reject, attributes: Vec::new(), handler: "normalization", account: None, verified: false, unavailable: false };
                }
            },
            _ => None,
        };
        let account = normalized.as_deref().or(received);
        let result = |decision, attributes, handler| AuthResult { decision, attributes, handler, account: account.map(str::to_string), verified: false, unavailable: false };
        let mut attributes = Collected(Vec::new());
        let mut last_accept = None;
        let mut verified = false;
        let mut stood_in = None;
        // A script that only checks policy says nothing about who this is: if a handler that compares
        // passwords has not accepted, nobody has checked, and a proved certificate is no account yet.
        let compares = self.handlers.iter().any(Handler::verifies_password);
        let no_password_check = || {
            log_line(tracing::Level::WARN, format!("auth chain: rejecting a request from client {}: all_accept without checking a password", request.client));
            result(Outcome::Reject, Vec::new(), "no_password_check")
        };
        for (position, handler) in self.handlers.iter().enumerate() {
            // Nobody will check any more, so a script's writes would stay for a request that is rejected.
            let too_late = !verified && !handler.verifies_password() && !self.handlers[position + 1..].iter().any(Handler::verifies_password);
            if self.policy == AuthPolicy::AllAccept && compares && too_late {
                return no_password_check();
            }
            let kind = handler.kind();
            let (outcome, handler_attributes, used_account, unavailable) = handler.run(credentials, account, request).await;
            match (self.policy, outcome) {
                (_, Outcome::Reject) => return AuthResult { unavailable, ..result(Outcome::Reject, Vec::new(), kind) },
                // The question would tell names apart, and run what is meant to come after a password.
                (AuthPolicy::AllAccept, Outcome::Challenge) if compares && !verified && !handler.verifies_password() => return no_password_check(),
                // Earlier handlers' attributes are for an accept, and RFC 2865 section 4.4
                // does not allow most of them in an Access-Challenge.
                (_, Outcome::Challenge) => return result(Outcome::Challenge, handler_attributes, kind),
                (_, Outcome::Ignore) => continue,
                (AuthPolicy::FirstMatch, Outcome::Accept) => {
                    let accepted = result(Outcome::Accept, handler_attributes, kind);
                    return AuthResult { verified: handler.verifies_password(), account: used_account.or(accepted.account.clone()), ..accepted };
                }
                (AuthPolicy::AllAccept, Outcome::Accept) => {
                    last_accept = Some(kind);
                    verified |= handler.verifies_password();
                    stood_in = used_account.or(stood_in);
                    attributes.0.extend(handler_attributes);
                }
            }
        }
        match last_accept {
            Some(_) if compares && !verified => no_password_check(),
            Some(kind) => {
                let accepted = result(Outcome::Accept, std::mem::take(&mut attributes.0), kind);
                AuthResult { verified, account: stood_in.or(accepted.account.clone()), ..accepted }
            }
            None => result(Outcome::Reject, Vec::new(), "none"),
        }
    }
}

/// Limits for Lua handlers, from `[lua]`.
#[derive(Clone)]
pub struct LuaLimits {
    pub query_timeout: Duration,
    pub max_execution: Duration,
    /// What `send_disconnect` and `send_coa` send through.
    pub dynamic_auth: Option<Arc<dyn crate::dynamic_authorization::DynamicAuthorization>>,
    /// What `http_request` sends through.
    pub http: Option<Arc<crate::script_http::ScriptHttp>>,
}

impl LuaLimits {
    pub fn from_settings(lua: &crate::config::LuaSettings) -> Self {
        Self { query_timeout: lua.query_timeout, max_execution: lua.max_execution, dynamic_auth: None, http: None }
    }
}

#[cfg(test)]
impl Default for LuaLimits {
    fn default() -> Self {
        Self { query_timeout: Duration::from_secs(5), max_execution: Duration::from_secs(1), dynamic_auth: None, http: None }
    }
}

/// Every chain configured in `[auth]`, built once and shared by all clients
/// and both protocols.
pub struct AuthChains {
    default: Option<AuthChain>,
    named: HashMap<String, AuthChain>,
}

impl AuthChains {
    pub fn new(default: Option<AuthChain>, named: impl IntoIterator<Item = (String, AuthChain)>) -> Self {
        Self { default, named: named.into_iter().collect() }
    }

    /// Builds the default chain and every named chain from `[auth]`.
    pub fn from_settings(settings: &AuthSettings, db: &Db, vms: &Arc<LuaVms>, lua: &LuaLimits) -> Result<Self, String> {
        let default = match &settings.default {
            Some(chain) => Some(AuthChain::from_settings(chain, db, vms, lua).map_err(|e| format!("auth.handlers: {}", e))?),
            None => None,
        };
        let mut named = HashMap::with_capacity(settings.chains.len());
        for (name, chain) in &settings.chains {
            let chain = AuthChain::from_settings(chain, db, vms, lua).map_err(|e| format!("auth.chains.{}: {}", name, e))?;
            named.insert(name.clone(), chain);
        }
        Ok(Self { default, named })
    }

    /// What `from_settings` would find wrong, without a database or building anything that
    /// serves. It does not contact any server.
    pub fn check_settings(settings: &AuthSettings, dictionary: &crate::dictionary::Dictionary) -> Result<(), String> {
        let chains = settings.default.iter().map(|chain| ("auth.handlers".to_string(), chain));
        let named = settings.chains.iter().map(|(name, chain)| (format!("auth.chains.{name}"), chain));
        for (prefix, chain) in chains.chain(named) {
            for handler in &chain.handlers {
                match handler {
                    AuthHandlerSettings::Sql { .. } => {}
                    AuthHandlerSettings::Lua { script, .. } => {
                        let source = std::fs::read_to_string(script)
                            .map_err(|e| format!("{prefix}: cannot read auth handler script '{}': {}", script.display(), e))?;
                        crate::lua_engine::compile_script(&source)
                            .map_err(|e| format!("{prefix}: cannot compile auth handler script '{}': {}", script.display(), e))?;
                    }
                    AuthHandlerSettings::Ldap(ldap) => {
                        LdapHandler::new(ldap.clone()).map_err(|e| format!("{prefix}: {e}"))?;
                        // Fixed in the file, so a mistake here would reject each member at login.
                        for (name, value) in ldap.groups.reply.iter().flat_map(|(_, attributes)| attributes) {
                            let value = AttrValue::Bytes(value.clone().into_bytes());
                            crate::radius::encode_response_attributes(dictionary, [(name.as_str(), &value)], b"", &[0; 16], &mut Vec::new())
                                .map_err(|e| format!("{prefix}: group_reply: {e}"))?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The chain named by the client's `auth`, or the default chain. `None`
    /// leaves the decision to the top-level script.
    pub fn for_client(&self, client: &Client) -> Option<&AuthChain> {
        match &client.auth {
            Some(name) => {
                let chain = self.named.get(name);
                if chain.is_none() {
                    // Configuration loading rejects unknown names, so this is not expected.
                    tracing::error!("auth chain '{}' for client '{}' is not configured; using the script", name, client.name);
                }
                chain
            }
            None => self.default.as_ref(),
        }
    }
}

impl Handler {
    /// Whether its accept means a password was compared.
    fn verifies_password(&self) -> bool {
        !matches!(self, Handler::Lua(_))
    }

    fn kind(&self) -> &'static str {
        match self {
            Handler::Sql { .. } => "sql",
            Handler::Lua(_) | Handler::LuaVerifying(_) => "lua",
            Handler::Ldap(_) => "ldap",
        }
    }

    /// `account` is the name to look the user up by; proving the secret uses the name
    /// in `credentials`, which is the one the client computed its response over. The last
    /// value is the account that was accepted in its place, if any.
    /// The last value says a refusal came of the cluster database being unavailable.
    async fn run(&self, credentials: &Credentials, account: Option<&str>, request: &LuaRequest) -> (Outcome, Vec<(String, AttrValue)>, Option<String>, bool) {
        match self {
            Handler::Lua(engine) | Handler::LuaVerifying(engine) => {
                let mut request = request.clone();
                request.auth_user = account.map(str::to_string);
                let eap = request.eap_method.is_some();
                let mut decision = engine.authenticate(request).await;
                // Inside EAP this says a password was checked, and a script cannot have done that.
                if eap {
                    decision.response_attributes.retain(|(attribute, _)| attribute != "MS-CHAP2-Success");
                }
                // A script's refusal that came after the database failed it says nothing of the user.
                // One that ignores still leaves the decision to the next handler.
                let unavailable = decision.database_failed && decision.outcome == Outcome::Reject;
                (decision.outcome, decision.response_attributes, None, unavailable)
            }
            Handler::Ldap(ldap) => {
                let (outcome, attributes) = ldap.authenticate_account(credentials, account).await;
                (outcome, attributes, None, false)
            }
            Handler::Sql { db, user_query, certificate_query, reply_query, default_user, timeout } => {
                // Inside EAP the name is what gets logged and given back to the device, and it would
                // be any name at all for those who know the default account's password.
                let default_user = default_user.as_deref().filter(|_| request.eap_method.is_none());
                match sql_handler(db, SqlQueries { user: user_query, certificate: certificate_query.as_deref(), reply: reply_query.as_deref() }, default_user, *timeout, credentials, account).await {
                    Ok((outcome, attributes, account)) => (outcome, attributes, account, false),
                    Err(e) => {
                        tracing::error!("auth sql handler error: {}", e);
                        (Outcome::Reject, Vec::new(), None, e.unavailable())
                    }
                }
            }
        }
    }
}

async fn timed_query(db: &Db, sql: &str, param: &str, timeout: Duration) -> Result<Vec<Vec<(String, DbValue)>>, crate::lua_engine::DbFailure> {
    tokio::time::timeout(timeout, db.query_classified(sql, &[param]))
        .await
        .map_err(|_| crate::lua_engine::DbFailure::timed_out(timeout))?
}

fn text(value: &DbValue) -> Option<String> {
    match value {
        DbValue::Text(s) => Some(s.clone()),
        DbValue::Integer(i) => Some(i.to_string()),
        DbValue::Real(r) => Some(r.to_string()),
        DbValue::Blob(b) => Some(String::from_utf8_lossy(b).into_owned()),
        DbValue::Null => None,
    }
}

/// The account name for a received name, or why the chain rejects it. The check comes
/// last, so `allowed` only has to describe what is left after the other steps.
pub(crate) fn account_name(settings: &UsernameSettings, received: &str) -> Result<String, &'static str> {
    let mut name = received;
    if matches!(settings.strip_realm, StripRealm::Prefix | StripRealm::Both) {
        name = name.rsplit_once('\\').map_or(name, |(_, rest)| rest);
    }
    if matches!(settings.strip_realm, StripRealm::Suffix | StripRealm::Both) {
        name = name.rsplit_once('@').map_or(name, |(rest, _)| rest);
    }
    let name = if settings.lowercase { name.to_lowercase() } else { name.to_string() };
    if name.is_empty() {
        return Err("the user name is empty after normalization");
    }
    if settings.allowed.as_ref().is_some_and(|allowed| !allowed.is_match(&name)) {
        return Err("the user name does not match the allowed pattern");
    }
    Ok(name)
}

/// Checks a secret against a stored password. On success, returns the attributes that
/// proving it adds to the response: none, except for MS-CHAPv2.
pub(crate) async fn verify_secret(stored: &str, user: &str, secret: &Secret) -> Option<Vec<(String, AttrValue)>> {
    // `{clear}` with nothing after it is no password, not the empty one: it would match an
    // empty PAP password or a CHAP or MS-CHAPv2 response computed over "".
    if passwords::split_scheme(stored).1.is_empty() {
        return None;
    }
    let verified = match secret {
        Secret::Pap(password) => passwords::verify_pap_blocking(stored, password).await,
        Secret::Chap { id, response, challenge } => passwords::verify_chap(stored, *id, response, challenge),
        Secret::MsChapV2 { ident, peer_challenge, response, challenge } => {
            let proof = mschap::verify(stored, user, peer_challenge, challenge, response)?;
            let mut success = vec![*ident];
            success.extend_from_slice(proof.authenticator_response.as_bytes());
            return Some(vec![
                ("MS-CHAP2-Success".to_string(), AttrValue::Bytes(success)),
                ("MS-MPPE-Send-Key".to_string(), AttrValue::Bytes(proof.send_key.to_vec())),
                ("MS-MPPE-Recv-Key".to_string(), AttrValue::Bytes(proof.recv_key.to_vec())),
            ]);
        }
        Secret::Certificate | Secret::None => false,
    };
    verified.then(Vec::new)
}

/// The queries of a `sql` handler.
struct SqlQueries<'a> {
    user: &'a str,
    certificate: Option<&'a str>,
    reply: Option<&'a str>,
}

/// Looks the user up (falling back to `default_user`), verifies the secret and
/// collects reply attributes. Database errors are returned as `Err`.
async fn sql_handler(
    db: &Db,
    queries: SqlQueries<'_>,
    default_user: Option<&str>,
    timeout: Duration,
    credentials: &Credentials,
    account: Option<&str>,
) -> Result<(Outcome, Vec<(String, AttrValue)>, Option<String>), crate::lua_engine::DbFailure> {
    let (Some(user), Some(account)) = (credentials.user.as_deref(), account) else {
        return Ok((Outcome::Ignore, Vec::new(), None));
    };
    let SqlQueries { user: user_query, certificate: certificate_query, reply: reply_query } = queries;
    let by_certificate = credentials.secret == Secret::Certificate;
    let mut account = account.to_string();
    let mut stood_in = false;
    let query = certificate_query.filter(|_| by_certificate).unwrap_or(user_query);
    let mut rows = timed_query(db, query, &account, timeout).await?;
    if rows.is_empty() {
        // A certificate must not log in as a user that does not exist.
        let default_user = default_user.filter(|_| !by_certificate);
        let Some(default_user) = default_user else {
            return Ok((Outcome::Ignore, Vec::new(), None));
        };
        account = default_user.to_string();
        stood_in = true;
        rows = timed_query(db, user_query, &account, timeout).await?;
        if rows.is_empty() {
            return Ok((Outcome::Ignore, Vec::new(), None));
        }
    }
    if rows.len() > 1 {
        tracing::error!(account = account.as_str(), "auth sql handler: user_query returned {} rows; rejecting", rows.len());
        return Ok((Outcome::Reject, Vec::new(), None));
    }
    let mut attributes = if by_certificate {
        // No stored password is how an account is commonly disabled, and it has to mean the same
        // here. A deployment without passwords says what counts instead with certificate_query.
        if certificate_query.is_none() && rows[0].first().and_then(|(_, value)| text(value)).is_none_or(|stored| passwords::split_scheme(&stored).1.is_empty()) {
            tracing::error!(account = account.as_str(), "auth sql handler: no stored password and no certificate_query; rejecting");
            return Ok((Outcome::Reject, Vec::new(), None));
        }
        Vec::new()
    } else {
        // A NULL or empty stored password must never match: an empty PAP password
        // (an all-zero User-Password block) or a CHAP response over "" would.
        let stored = match rows[0].first().and_then(|(_, value)| text(value)) {
            Some(stored) if !stored.is_empty() => stored,
            _ => {
                tracing::error!(account = account.as_str(), "auth sql handler: no stored password; rejecting");
                return Ok((Outcome::Reject, Vec::new(), None));
            }
        };
        // The limit covers waiting for a turn at a slow hash, and the hash.
        let verified = tokio::time::timeout(timeout, verify_secret(&stored, user, &credentials.secret))
            .await
            .map_err(|_| crate::lua_engine::DbFailure::other(format!("verifying the password timed out after {} ms", timeout.as_millis())))?;
        let Some(attributes) = verified else {
            return Ok((Outcome::Reject, Vec::new(), None));
        };
        attributes
    };
    if let Some(reply_query) = reply_query {
        for row in timed_query(db, reply_query, &account, timeout).await? {
            let mut columns = row.iter().map(|(_, value)| text(value));
            if let (Some(Some(name)), Some(Some(value))) = (columns.next(), columns.next()) {
                attributes.push((name, AttrValue::Bytes(value.into_bytes())));
            }
        }
    }
    Ok((Outcome::Accept, attributes, stood_in.then_some(account)))
}

#[cfg(test)]
mod tests {
    use super::*;

    use md5::{Digest, Md5};
    use crate::config::AllowedNames;
    use crate::test_support::test_db;

    #[test]
    fn test_secret_debug_shows_no_password() {
        let credentials = Credentials { user: Some("alice".to_string()), secret: Secret::pap(b"hunter2".to_vec()) };
        let shown = format!("{credentials:?}");
        assert!(!shown.contains("hunter2") && !shown.contains("104"), "{shown}");
        assert!(shown.contains("alice") && shown.contains("Pap"), "{shown}");
    }

    /// Does not compile if the password is held in memory nobody clears.
    #[test]
    fn test_secret_pap_is_wiped() {
        fn cleared<T: zeroize::ZeroizeOnDrop>(_: &T) {}
        let Secret::Pap(password) = Secret::pap(b"x".to_vec()) else { panic!() };
        cleared(&password);
    }

    fn request() -> LuaRequest {
        LuaRequest::new("radius", "authentication", vec![])
    }

    fn lua(script: &str) -> Handler {
        Handler::Lua(LuaAuthEngine::new(crate::test_support::test_vms(), test_db(), script).unwrap())
    }

    /// A Lua handler that records that it ran in `chain_runs` and returns `result`.
    fn recording_lua(tag: &str, result: &str) -> Handler {
        lua(&format!(
            "sql_execute(\"INSERT INTO chain_runs VALUES ('{tag}')\") return '{result}'"
        ))
    }

    async fn ran(tag: &str) -> bool {
        !test_db().query_with_params("SELECT tag FROM chain_runs WHERE tag = ?", &[tag]).await.unwrap().is_empty()
    }

    async fn setup_runs() {
        test_db().execute("CREATE TABLE IF NOT EXISTS chain_runs (tag TEXT)").await.unwrap();
    }

    fn pap(user: &str, password: &str) -> Credentials {
        Credentials { user: Some(user.to_string()), secret: Secret::pap(password.as_bytes().to_vec()) }
    }

    fn username(strip_realm: StripRealm, lowercase: bool, allowed: Option<&str>) -> UsernameSettings {
        UsernameSettings { strip_realm, lowercase, allowed: allowed.map(|pattern| AllowedNames::new(pattern).unwrap()) }
    }

    #[test]
    fn test_account_names() {
        use StripRealm::*;
        for (strip_realm, lowercase, received, expected) in [
            (None, false, "Alice@Corp", "Alice@Corp"),
            (Suffix, false, "Alice@Corp", "Alice"),
            (Suffix, false, "a@b@c", "a@b"),
            (Suffix, false, "CORP\\Alice", "CORP\\Alice"),
            (Prefix, false, "CORP\\Alice", "Alice"),
            (Prefix, false, "A\\B\\c", "c"),
            (Prefix, false, "Alice@Corp", "Alice@Corp"),
            (Both, true, "CORP\\Alice@Corp", "alice"),
        ] {
            assert_eq!(account_name(&username(strip_realm, lowercase, Option::None), received), Ok(expected.to_string()), "{strip_realm:?} {received}");
        }
    }

    #[tokio::test]
    async fn test_empty_account_name_rejects_without_a_handler() {
        setup_runs().await;
        let chain = AuthChain::new(AuthPolicy::FirstMatch, vec![recording_lua("norm-empty", "accept")])
            .with_username(Some(username(StripRealm::Suffix, false, None)));
        assert_eq!(chain.authenticate(&pap("@corp", "x"), &request()).await.decision, Outcome::Reject);
        assert!(!ran("norm-empty").await);
    }

    #[tokio::test]
    async fn test_allowed_matches_the_whole_name() {
        setup_runs().await;
        for policy in [AuthPolicy::FirstMatch, AuthPolicy::AllAccept] {
            let chain = AuthChain::new(policy, vec![recording_lua("norm-allowed", "accept")])
                .with_username(Some(username(StripRealm::None, false, Some("[a-z]+"))));
            assert_eq!(chain.authenticate(&pap("a'; --", "x"), &request()).await.decision, Outcome::Reject, "{policy:?}");
            assert!(!ran("norm-allowed").await, "{policy:?}");
        }
        let chain = AuthChain::new(AuthPolicy::FirstMatch, vec![lua("return 'accept'")])
            .with_username(Some(username(StripRealm::None, false, Some("[a-z]+"))));
        assert!(chain.authenticate(&pap("alice", "x"), &request()).await.accepted());
    }

    #[tokio::test]
    async fn test_allowed_sees_the_normalized_name() {
        let chain = AuthChain::new(AuthPolicy::FirstMatch, vec![lua("return 'accept'")])
            .with_username(Some(username(StripRealm::Suffix, true, Some("[a-z]+"))));
        assert!(chain.authenticate(&pap("Alice@Corp", "x"), &request()).await.accepted());
    }

    #[tokio::test]
    async fn test_rejection_is_logged_without_the_name() {
        let chain = AuthChain::new(AuthPolicy::FirstMatch, vec![lua("return 'accept'")])
            .with_username(Some(username(StripRealm::None, false, Some("[a-z]+"))));
        let mut for_client = request();
        for_client.client = "norm-log-nas".to_string();
        chain.authenticate(&pap("bad\nname", "x"), &for_client).await;
        let lines = crate::test_support::logged_errors().lock().unwrap().clone();
        let lines: Vec<_> = lines.iter().filter(|line| line.contains("norm-log-nas")).collect();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("allowed"), "{lines:?}");
        assert!(!lines[0].contains("bad") && !lines[0].contains('\n'), "{lines:?}");
    }

    #[tokio::test]
    async fn test_request_without_a_name_reaches_the_handlers() {
        setup_runs().await;
        let chain = AuthChain::new(AuthPolicy::FirstMatch, vec![recording_lua("norm-noname", "accept")])
            .with_username(Some(username(StripRealm::Both, true, Some("[a-z]+"))));
        let credentials = Credentials { user: Option::None, secret: Secret::None };
        assert!(chain.authenticate(&credentials, &request()).await.accepted());
        assert!(ran("norm-noname").await);
    }

    /// A table holding `alice` only, with the NT hash of `secret`, and her reply attribute.
    async fn normalized_sql_chain() -> AuthChain {
        use md4::Md4;
        static SEEDED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
        SEEDED
            .get_or_init(|| async {
                let nt: Vec<u8> = "secret".encode_utf16().flat_map(u16::to_le_bytes).collect();
                let hex: String = Md4::digest(&nt).iter().map(|b| format!("{b:02x}")).collect();
                let db = test_db();
                db.execute("CREATE TABLE norm_users (username TEXT, password TEXT, nt TEXT)").await.unwrap();
                db.execute(&format!("INSERT INTO norm_users VALUES ('alice', 'secret', '{{nthash}}{hex}')")).await.unwrap();
                db.execute("CREATE TABLE norm_reply (username TEXT, attribute TEXT, value TEXT)").await.unwrap();
                db.execute("INSERT INTO norm_reply VALUES ('alice', 'Filter-Id', 'norm.std')").await.unwrap();
            })
            .await;
        let handler = Handler::Sql {
            db: test_db(),
            user_query: "SELECT password FROM norm_users WHERE username = ?".to_string(),
            reply_query: Some("SELECT attribute, value FROM norm_reply WHERE username = ?".to_string()),
            certificate_query: None, default_user: None,
            timeout: Duration::from_secs(5),
        };
        AuthChain::new(AuthPolicy::FirstMatch, vec![handler]).with_username(Some(username(StripRealm::Suffix, true, None)))
    }

    #[tokio::test]
    async fn test_sql_lookup_uses_the_account_name() {
        let chain = normalized_sql_chain().await;
        let result = chain.authenticate(&pap("Alice@Corp", "secret"), &request()).await;
        assert!(result.accepted());
        assert_eq!(result.attributes, vec![attr("Filter-Id", "norm.std")]);
        assert!(!chain.authenticate(&pap("Alice@Corp", "wrong"), &request()).await.accepted());
    }

    #[tokio::test]
    async fn test_mschapv2_uses_the_received_name() {
        let chain = normalized_sql_chain().await;
        let handler = Handler::Sql {
            db: test_db(),
            user_query: "SELECT nt FROM norm_users WHERE username = ?".to_string(),
            reply_query: None,
            certificate_query: None, default_user: None,
            timeout: Duration::from_secs(5),
        };
        let chain = AuthChain::new(AuthPolicy::FirstMatch, vec![handler]).with_username(chain.username);
        let (peer, challenge) = ([3u8; 16], [4u8; 16]);
        let response = mschap::response_for("secret", "Alice@Corp", &peer, &challenge);
        let credentials = Credentials {
            user: Some("Alice@Corp".to_string()),
            secret: Secret::MsChapV2 { ident: 9, peer_challenge: peer, response, challenge },
        };
        let result = chain.authenticate(&credentials, &request()).await;
        assert!(result.accepted());
        let proof = mschap::verify("secret", "Alice@Corp", &peer, &challenge, &response).unwrap();
        let mut success = vec![9u8];
        success.extend_from_slice(proof.authenticator_response.as_bytes());
        assert_eq!(result.attributes[0], ("MS-CHAP2-Success".to_string(), AttrValue::Bytes(success)));
        // Computed over the account name instead, the response proves nothing.
        let over_account = mschap::response_for("secret", "alice", &peer, &challenge);
        let credentials = Credentials {
            user: Some("Alice@Corp".to_string()),
            secret: Secret::MsChapV2 { ident: 9, peer_challenge: peer, response: over_account, challenge },
        };
        assert!(!chain.authenticate(&credentials, &request()).await.accepted());
    }

    #[tokio::test]
    async fn test_ldap_lookup_uses_the_account_name() {
        use crate::ldap_handler::tests::{mock_server, settings};
        use crate::ldap_handler::PasswordCheck;
        use ldap3_proto::proto::LdapFilter;
        let (url, log) = mock_server().await;
        let handler = Handler::Ldap(LdapHandler::new(settings(vec![url], PasswordCheck::Bind)).unwrap());
        let chain = AuthChain::new(AuthPolicy::FirstMatch, vec![handler]).with_username(Some(username(StripRealm::Suffix, true, None)));
        assert!(chain.authenticate(&pap("Alice@Corp", "secret"), &request()).await.accepted());
        assert_eq!(log.lock().unwrap().filters, vec![LdapFilter::Equality("uid".to_string(), "alice".to_string())]);
    }

    #[tokio::test]
    async fn test_what_decided() {
        use crate::ldap_handler::tests::{mock_server, settings};
        use crate::ldap_handler::PasswordCheck;
        let (url, _log) = mock_server().await;
        let ldap = Handler::Ldap(LdapHandler::new(settings(vec![url], PasswordCheck::Bind)).unwrap());
        let accepts = || lua("return 'accept'");
        let ignores = || lua("return 'ignore'");
        let cases = [
            (AuthPolicy::FirstMatch, vec![ldap], "alice", "wrong", Outcome::Reject, "ldap"),
            (AuthPolicy::FirstMatch, vec![lua("set_attr('State', 'x') return 'challenge'")], "alice", "x", Outcome::Challenge, "lua"),
            (AuthPolicy::FirstMatch, vec![ignores(), accepts(), lua("return 'reject'")], "alice", "x", Outcome::Accept, "lua"),
            (AuthPolicy::AllAccept, vec![accepts(), accepts()], "alice", "x", Outcome::Accept, "lua"),
            (AuthPolicy::FirstMatch, vec![ignores(), ignores()], "alice", "x", Outcome::Reject, "none"),
            (AuthPolicy::AllAccept, vec![ignores()], "alice", "x", Outcome::Reject, "none"),
        ];
        for (policy, handlers, user, password, decision, handler) in cases {
            let result = AuthChain::new(policy, handlers).authenticate(&pap(user, password), &request()).await;
            assert_eq!((result.decision, result.handler), (decision, handler));
            assert_eq!(result.account.as_deref(), Some(user));
        }

        setup_users().await;
        let sql = Handler::Sql {
            db: test_db(),
            user_query: format!("SELECT password FROM {USERS} WHERE username = ?"),
            reply_query: None,
            certificate_query: None, default_user: None,
            timeout: Duration::from_secs(5),
        };
        let result = AuthChain::new(AuthPolicy::AllAccept, vec![sql, accepts()]).authenticate(&pap("alice", "secret"), &request()).await;
        assert_eq!((result.decision, result.handler), (Outcome::Accept, "lua"), "all_accept names the last to accept");

        let refused = AuthChain::new(AuthPolicy::FirstMatch, vec![accepts()]).with_username(Some(username(StripRealm::None, false, Some("[a-z]+"))));
        let result = refused.authenticate(&pap("A1", "x"), &request()).await;
        assert_eq!((result.decision, result.handler, result.account), (Outcome::Reject, "normalization", Option::None));

        let normalized = AuthChain::new(AuthPolicy::FirstMatch, vec![accepts()]).with_username(Some(username(StripRealm::Suffix, true, None)));
        assert_eq!(normalized.authenticate(&pap("Alice@Corp", "x"), &request()).await.account.as_deref(), Some("alice"));
        let nameless = Credentials { user: Option::None, secret: Secret::None };
        assert_eq!(normalized.authenticate(&nameless, &request()).await.account, Option::None);
    }

    fn named_request(name: &str) -> LuaRequest {
        LuaRequest::new("radius", "authentication", vec![attr("User-Name", name)])
    }

    #[tokio::test]
    async fn test_lua_handler_sees_both_names() {
        let script = "return (auth_user == 'alice' and get_attr('User-Name') == 'Alice@Corp') and 'accept' or 'reject'";
        let chain = AuthChain::new(AuthPolicy::FirstMatch, vec![lua(script)]).with_username(Some(username(StripRealm::Suffix, true, None)));
        assert!(chain.authenticate(&pap("Alice@Corp", "x"), &named_request("Alice@Corp")).await.accepted());
    }

    #[tokio::test]
    async fn test_lua_handler_without_normalization() {
        let chain = AuthChain::new(AuthPolicy::FirstMatch, vec![lua("return auth_user == 'Alice@Corp' and 'accept' or 'reject'")]);
        assert!(chain.authenticate(&pap("Alice@Corp", "x"), &named_request("Alice@Corp")).await.accepted());
    }

    #[tokio::test]
    async fn test_auth_user_is_nil_without_a_name_or_a_chain() {
        let script = "return auth_user == nil and 'accept' or 'reject'";
        let chain = AuthChain::new(AuthPolicy::FirstMatch, vec![lua(script)]).with_username(Some(username(StripRealm::Suffix, true, None)));
        let nameless = Credentials { user: Option::None, secret: Secret::None };
        assert!(chain.authenticate(&nameless, &request()).await.accepted());
        // A top-level script: the same engine with no chain around it.
        let engine = LuaAuthEngine::new(crate::test_support::test_vms(), test_db(), script).unwrap();
        assert!(engine.authenticate(named_request("alice")).await.accepted());
    }

    #[tokio::test]
    async fn test_first_match_policy() {
        setup_runs().await;
        let chain = AuthChain::new(AuthPolicy::FirstMatch, vec![
            recording_lua("fm-1", "ignore"),
            recording_lua("fm-2", "accept"),
            recording_lua("fm-3", "reject"),
        ]);
        assert!(chain.authenticate(&pap("a", "b"), &request()).await.accepted());
        assert!(ran("fm-1").await && ran("fm-2").await);
        assert!(!ran("fm-3").await, "handler after the decision must not run");

        let all_ignore = AuthChain::new(AuthPolicy::FirstMatch, vec![lua("return 'ignore'"), lua("return 'ignore'")]);
        assert!(!all_ignore.authenticate(&pap("a", "b"), &request()).await.accepted());
    }

    /// A query that does not finish in time is no answer about the user: the chain says the
    /// database was unavailable, where a wrong password or no such account says no.
    #[tokio::test]
    async fn test_unavailable_is_not_a_refusal() -> Result<(), String> {
        let db = test_db();
        db.execute("CREATE TABLE unavailable_users (username TEXT, password TEXT)").await?;
        db.execute("INSERT INTO unavailable_users VALUES ('alice', '{clear}secret')").await?;
        let sql = |query: &str, timeout: Duration| Handler::Sql { db: test_db(), user_query: query.to_string(), reply_query: None, certificate_query: None, default_user: None, timeout };
        let slow = "SELECT password FROM unavailable_users, (WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 5000000) SELECT max(x) FROM c) WHERE username = ?";
        let timed_out = AuthChain::new(AuthPolicy::FirstMatch, vec![sql(slow, Duration::from_millis(5))]).authenticate(&pap("alice", "secret"), &request()).await;
        assert!(!timed_out.accepted() && timed_out.unavailable, "{timed_out:?}");
        let quick = "SELECT password FROM unavailable_users WHERE username = ?";
        for (user, password) in [("alice", "wrong"), ("nobody", "secret")] {
            let refused = AuthChain::new(AuthPolicy::FirstMatch, vec![sql(quick, Duration::from_secs(5))]).authenticate(&pap(user, password), &request()).await;
            assert!(!refused.accepted() && !refused.unavailable, "{user}: {refused:?}");
        }
        // A script whose database call timed out, whether it raised the error or caught it and refused.
        let slow_script = |ending: &str| {
            let script = format!("local ok = pcall(sql_query, \"WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 5000000) SELECT max(x) FROM c\") {ending}");
            LuaAuthEngine::new(crate::test_support::test_vms(), test_db(), &script).map(|engine| Handler::Lua(engine.with_query_timeout(Duration::from_millis(5))))
        };
        for ending in ["return 'reject'", "error('gave up')"] {
            let result = AuthChain::new(AuthPolicy::FirstMatch, vec![slow_script(ending)?]).authenticate(&pap("alice", "secret"), &request()).await;
            assert!(!result.accepted() && result.unavailable, "{ending}: {result:?}");
        }
        // One that ignores leaves it to the next handler, which decides.
        let ignoring = slow_script("return 'ignore'")?;
        let result = AuthChain::new(AuthPolicy::FirstMatch, vec![ignoring, lua("return 'accept'")]).authenticate(&pap("alice", "secret"), &request()).await;
        assert!(result.accepted() && !result.unavailable, "{result:?}");
        // A script that accepts anyway decides: its database trouble is its own business.
        let result = AuthChain::new(AuthPolicy::FirstMatch, vec![slow_script("return 'accept'")?]).authenticate(&pap("alice", "secret"), &request()).await;
        assert!(result.accepted() && !result.unavailable);
        // A refusal from a script whose calls all worked is a refusal.
        let result = AuthChain::new(AuthPolicy::FirstMatch, vec![lua("return 'reject'")]).authenticate(&pap("alice", "secret"), &request()).await;
        assert!(!result.unavailable);
        Ok(())
    }

    /// A 256 MiB hash, slower than the limit, with a query well inside it.
    #[tokio::test]
    async fn test_timeout_covers_password_verification() {
        let db = test_db();
        db.execute("CREATE TABLE slow_hash_users (username TEXT, password TEXT)").await.unwrap();
        db.execute("INSERT INTO slow_hash_users VALUES ('alice', '{argon2}$argon2id$v=19$m=262144,t=2,p=1$c29tZXNhbHQ$eP4eyR+zqlZX1y5xCFTkw9m5GYx0L5YWwvCFvtlbLow')").await.unwrap();
        let query = "SELECT password FROM slow_hash_users WHERE username = ?";
        let result = sql_handler(&db, SqlQueries { user: query, certificate: None, reply: None }, None, Duration::from_millis(300), &pap("alice", "password"), Some("alice")).await;
        let error = result.unwrap_err();
        assert!(error.to_string().contains("verifying the password timed out"), "{error}");
    }

    /// A template writing `'{clear}' || password` turns "no password" into this value.
    #[tokio::test]
    async fn test_empty_stored_value_never_matches() {
        let (peer, challenge) = ([1u8; 16], [2u8; 16]);
        let chap = Md5::digest([&[7u8][..], b"", &challenge].concat()).to_vec();
        for secret in [
            Secret::pap(Vec::new()),
            Secret::Chap { id: 7, response: chap, challenge: challenge.to_vec() },
            Secret::MsChapV2 { ident: 1, peer_challenge: peer, response: crate::mschap::response_for("", "alice", &peer, &challenge), challenge },
        ] {
            assert!(verify_secret("{clear}", "alice", &secret).await.is_none(), "{secret:?}");
        }
    }

    #[tokio::test]
    async fn test_challenge_stops_the_chain() {
        setup_runs().await;
        for policy in [AuthPolicy::AllAccept, AuthPolicy::FirstMatch] {
            let tag = format!("challenge-{policy:?}");
            let chain = AuthChain::new(policy, vec![
                lua("return 'ignore'"),
                lua("set_attr('Reply-Message', 'code?') return 'challenge'"),
                recording_lua(&tag, "accept"),
            ]);
            let result = chain.authenticate(&pap("a", "b"), &request()).await;
            assert_eq!(result.decision, Outcome::Challenge);
            assert_eq!(result.attributes, [("Reply-Message".to_string(), AttrValue::Bytes(b"code?".to_vec()))]);
            let ran = test_db().query_with_params("SELECT tag FROM chain_runs WHERE tag = ?", &[&tag]).await.unwrap();
            assert!(ran.is_empty(), "a handler ran after the challenge");
        }
        // Attributes of an earlier accept do not go into an Access-Challenge.
        let chain = AuthChain::new(AuthPolicy::AllAccept, vec![
            lua("set_attr('Filter-Id', 'x') return 'accept'"),
            lua("return 'challenge'"),
        ]);
        let result = chain.authenticate(&pap("a", "b"), &request()).await;
        assert_eq!(result.decision, Outcome::Challenge);
        assert!(result.attributes.is_empty());
    }

    #[tokio::test]
    async fn test_all_accept_needs_a_password_check() {
        setup_users().await;
        let sql = || Handler::Sql {
            db: test_db(),
            user_query: format!("SELECT password FROM {USERS} WHERE username = ?"),
            reply_query: None,
            certificate_query: None,
            default_user: None,
            timeout: Duration::from_secs(5),
        };
        let accepts = || lua("set_attr('Session-Timeout', 3600) return 'accept'");
        let verifying = || Handler::LuaVerifying(LuaAuthEngine::new(crate::test_support::test_vms(), test_db(), "return 'accept'").unwrap());
        let chain = AuthChain::new(AuthPolicy::AllAccept, vec![sql(), accepts()]);
        let unknown = chain.authenticate(&pap("nosuchuser", "anything"), &request()).await;
        assert_eq!(unknown.decision, Outcome::Reject, "nobody compared a password");
        assert!(unknown.attributes.is_empty());
        let nameless = Credentials { user: None, secret: Secret::pap(b"anything".to_vec()) };
        assert_eq!(chain.authenticate(&nameless, &request()).await.decision, Outcome::Reject);
        assert_eq!(chain.authenticate(&pap("alice", "wrong"), &request()).await.decision, Outcome::Reject);
        let right = chain.authenticate(&pap("alice", "secret"), &request()).await;
        assert_eq!(right.decision, Outcome::Accept);
        assert!(right.attributes.iter().any(|(name, _)| name == "Session-Timeout"));
        // A script that says it compares counts, and scripts alone decide among themselves.
        assert_eq!(AuthChain::new(AuthPolicy::AllAccept, vec![sql(), verifying()]).authenticate(&pap("nosuchuser", "x"), &request()).await.decision, Outcome::Accept);
        assert_eq!(AuthChain::new(AuthPolicy::AllAccept, vec![lua("return 'ignore'"), accepts()]).authenticate(&pap("nosuchuser", "x"), &request()).await.decision, Outcome::Accept);
        assert_eq!(unknown.handler, "no_password_check");
        // A certificate proves a name, and not that it is an account that may log in.
        let certificate = |user: &str| Credentials { user: Some(user.to_string()), secret: Secret::Certificate };
        assert_eq!(chain.authenticate(&certificate("nosuchuser"), &request()).await.decision, Outcome::Reject);
        assert_eq!(chain.authenticate(&certificate("alice"), &request()).await.decision, Outcome::Accept);
        let disabled = Handler::Sql {
            db: test_db(),
            user_query: format!("SELECT password FROM {USERS} WHERE username = ?"),
            reply_query: None,
            certificate_query: Some(format!("SELECT 1 FROM {USERS} WHERE username = ? AND 0")),
            default_user: None,
            timeout: Duration::from_secs(5),
        };
        let disabled = AuthChain::new(AuthPolicy::AllAccept, vec![disabled, accepts()]);
        assert_eq!(disabled.authenticate(&certificate("alice"), &request()).await.decision, Outcome::Reject, "a disabled account");
        // Scripts alone go on deciding for a certificate too.
        assert_eq!(AuthChain::new(AuthPolicy::AllAccept, vec![accepts()]).authenticate(&certificate("anyone"), &request()).await.decision, Outcome::Accept);
        // No challenge before anyone checked: it would tell names apart, and run what comes after a password.
        let challenges = AuthChain::new(AuthPolicy::AllAccept, vec![sql(), lua("return 'challenge'")]);
        let before = challenges.authenticate(&pap("nosuchuser", "x"), &request()).await;
        assert_eq!((before.decision, before.handler), (Outcome::Reject, "no_password_check"));
        assert_eq!(challenges.authenticate(&pap("alice", "wrong"), &request()).await.decision, Outcome::Reject);
        assert_eq!(challenges.authenticate(&pap("alice", "secret"), &request()).await.decision, Outcome::Challenge);
        let first = AuthChain::new(AuthPolicy::AllAccept, vec![lua("return 'challenge'"), sql()]);
        assert_eq!(first.authenticate(&pap("alice", "secret"), &request()).await.decision, Outcome::Reject, "before the check it has");
        let saying = AuthChain::new(AuthPolicy::AllAccept, vec![sql(), Handler::LuaVerifying(LuaAuthEngine::new(crate::test_support::test_vms(), test_db(), "return 'challenge'").unwrap())]);
        assert_eq!(saying.authenticate(&pap("nosuchuser", "x"), &request()).await.decision, Outcome::Challenge, "a script that compares may ask");
        // first_match goes on passing it along, as the documentation warns.
        let first_match = AuthChain::new(AuthPolicy::FirstMatch, vec![sql(), accepts()]);
        assert_eq!(first_match.authenticate(&pap("nosuchuser", "x"), &request()).await.decision, Outcome::Accept);
        // The order does not matter.
        let reversed = AuthChain::new(AuthPolicy::AllAccept, vec![accepts(), sql()]);
        assert_eq!(reversed.authenticate(&pap("nosuchuser", "x"), &request()).await.decision, Outcome::Reject);
    }

    #[tokio::test]
    async fn test_all_accept_policy_script_runs_only_after_a_check() {
        setup_users().await;
        let db = test_db();
        db.execute("CREATE TABLE IF NOT EXISTS policy_writes (who TEXT)").await.unwrap();
        let sql = || Handler::Sql {
            db: test_db(),
            user_query: format!("SELECT password FROM {USERS} WHERE username = ?"),
            reply_query: None,
            certificate_query: None,
            default_user: None,
            timeout: Duration::from_secs(5),
        };
        let writes = |who: &str| lua(&format!("sql_execute(\"INSERT INTO policy_writes VALUES ('{who}')\") return 'accept'"));
        let written = async |who: &str| db.query_with_params("SELECT who FROM policy_writes WHERE who = ?", &[who]).await.unwrap().len();
        let unknown = AuthChain::new(AuthPolicy::AllAccept, vec![sql(), writes("unknown")]);
        let result = unknown.authenticate(&pap("nosuchuser", "x"), &request()).await;
        assert_eq!((result.decision, result.handler), (Outcome::Reject, "no_password_check"));
        let certificate = Credentials { user: Some("nosuchuser".to_string()), secret: Secret::Certificate };
        assert_eq!(unknown.authenticate(&certificate, &request()).await.decision, Outcome::Reject);
        let wrong = AuthChain::new(AuthPolicy::AllAccept, vec![sql(), writes("wrong")]);
        assert_eq!(wrong.authenticate(&pap("alice", "wrong"), &request()).await.decision, Outcome::Reject);
        // Before the check it cannot be known yet what the check will say.
        let before = AuthChain::new(AuthPolicy::AllAccept, vec![writes("before"), sql()]);
        assert_eq!(before.authenticate(&pap("nosuchuser", "x"), &request()).await.decision, Outcome::Reject);
        // The last write goes through the same replica after the others, and a script between
        // two handlers that compare still runs while one is left.
        let between = AuthChain::new(AuthPolicy::AllAccept, vec![sql(), writes("between"), sql()]);
        assert_eq!(between.authenticate(&pap("nosuchuser", "x"), &request()).await.decision, Outcome::Reject);
        let right = AuthChain::new(AuthPolicy::AllAccept, vec![sql(), writes("right")]);
        assert_eq!(right.authenticate(&pap("alice", "secret"), &request()).await.decision, Outcome::Accept);
        assert_eq!(written("right").await, 1, "run after a check");
        assert_eq!(written("between").await, 1, "a handler that compares was still to come");
        assert_eq!(written("before").await, 1);
        assert_eq!(written("unknown").await, 0, "not run for a name nobody checked, or a certificate");
        assert_eq!(written("wrong").await, 0);
    }

    #[tokio::test]
    async fn test_all_accept_policy() {
        setup_runs().await;
        let chain = AuthChain::new(AuthPolicy::AllAccept, vec![lua("return 'accept'"), lua("return 'ignore'"), lua("return 'accept'")]);
        assert!(chain.authenticate(&pap("a", "b"), &request()).await.accepted());

        let stops = AuthChain::new(AuthPolicy::AllAccept, vec![
            recording_lua("aa-1", "accept"),
            recording_lua("aa-2", "reject"),
            recording_lua("aa-3", "accept"),
        ]);
        assert!(!stops.authenticate(&pap("a", "b"), &request()).await.accepted());
        assert!(!ran("aa-3").await, "handler after a reject must not run");

        let none = AuthChain::new(AuthPolicy::AllAccept, vec![lua("return 'ignore'")]);
        assert!(!none.authenticate(&pap("a", "b"), &request()).await.accepted());
    }

    const USERS: &str = "chain_users";

    /// Seeds the user tables once per test binary; tests run in parallel and
    /// would otherwise insert the rows several times.
    async fn setup_users() {
        static SEEDED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
        SEEDED
            .get_or_init(|| async {
                let db = test_db();
                db.execute(&format!("CREATE TABLE {USERS} (username TEXT, password TEXT)")).await.unwrap();
                db.execute("CREATE TABLE chain_reply (username TEXT, attribute TEXT, value TEXT)").await.unwrap();
                db.execute(&format!(
                    "INSERT INTO {USERS} VALUES ('alice', 'secret'), ('DEFAULT', 'guest-pass'), ('twin', 'a'), ('twin', 'b'), \
                     ('nullpw', NULL), ('emptypw', ''), ('prefixpw', '{{clear}}'), ('hashprefixpw', '{{nthash}}')"
                ))
                .await
                .unwrap();
                db.execute(
                    "INSERT INTO chain_reply VALUES ('alice', 'Filter-Id', 'std.ppp'), ('alice', 'Service-Type', 'Framed-User'), \
                     ('DEFAULT', 'Filter-Id', 'guest')",
                )
                .await
                .unwrap();
            })
            .await;
    }

    fn sql(default_user: Option<&str>) -> Handler {
        Handler::Sql {
            db: test_db(),
            user_query: format!("SELECT password FROM {USERS} WHERE username = ?"),
            reply_query: Some("SELECT attribute, value FROM chain_reply WHERE username = ?".to_string()),
            certificate_query: None, default_user: default_user.map(str::to_string),
            timeout: Duration::from_secs(5),
        }
    }

    async fn sql_outcome(handler: &Handler, credentials: &Credentials) -> (Outcome, Vec<(String, AttrValue)>) {
        setup_users().await;
        let (outcome, attributes, _, _) = handler.run(credentials, credentials.user.as_deref(), &request()).await;
        (outcome, attributes)
    }

    fn attr(name: &str, value: &str) -> (String, AttrValue) {
        (name.to_string(), AttrValue::Bytes(value.as_bytes().to_vec()))
    }

    fn by_certificate(user: &str) -> Credentials {
        Credentials { user: Some(user.to_string()), secret: Secret::Certificate }
    }

    #[tokio::test]
    async fn test_sql_handler_with_a_certificate() {
        let handler = sql(None);
        let (outcome, attributes) = sql_outcome(&handler, &by_certificate("alice")).await;
        assert_eq!(outcome, Outcome::Accept, "known user, no password asked");
        assert_eq!(attributes, vec![attr("Filter-Id", "std.ppp"), attr("Service-Type", "Framed-User")]);
        assert_eq!(sql_outcome(&handler, &by_certificate("mallory")).await.0, Outcome::Ignore);
        assert_eq!(sql_outcome(&handler, &by_certificate("twin")).await.0, Outcome::Reject, "two rows");
        let (outcome, _) = sql_outcome(&sql(Some("DEFAULT")), &by_certificate("mallory")).await;
        assert_eq!(outcome, Outcome::Ignore, "default_user does not apply");
    }

    #[tokio::test]
    async fn test_certificate_name_normalized_in_more_than_case() {
        setup_users().await;
        let handler = |pattern: UsernameSettings| AuthChain::new(AuthPolicy::FirstMatch, vec![sql(None)]).with_username(Some(pattern));
        let chain = handler(username(StripRealm::Suffix, false, Option::None));
        let refused = chain.authenticate(&by_certificate("alice@partner.example"), &request()).await;
        assert_eq!((refused.decision, refused.handler), (Outcome::Reject, "normalization"));
        assert_eq!(chain.authenticate(&by_certificate("alice"), &request()).await.decision, Outcome::Accept, "nothing to strip");
        let chain = handler(username(StripRealm::Prefix, false, Option::None));
        assert_eq!(chain.authenticate(&by_certificate("PARTNER\\alice"), &request()).await.handler, "normalization");
        let chain = handler(username(StripRealm::None, true, Option::None));
        let accepted = chain.authenticate(&by_certificate("Alice"), &request()).await;
        assert_eq!((accepted.decision, accepted.account.as_deref()), (Outcome::Accept, Some("alice")), "case alone");
    }

    #[tokio::test]
    async fn test_certificate_without_a_stored_password() {
        let handler = sql(None);
        assert_eq!(sql_outcome(&handler, &by_certificate("nullpw")).await.0, Outcome::Reject, "NULL");
        assert_eq!(sql_outcome(&handler, &by_certificate("emptypw")).await.0, Outcome::Reject, "empty");
        assert_eq!(sql_outcome(&handler, &by_certificate("prefixpw")).await.0, Outcome::Reject, "only a prefix");
        assert_eq!(sql_outcome(&handler, &by_certificate("hashprefixpw")).await.0, Outcome::Reject, "only a prefix");
    }

    #[tokio::test]
    async fn test_certificate_query_is_not_read_as_a_password() {
        for column in ["NULL", "''", "'{clear}'"] {
            let Handler::Sql { db, user_query, reply_query, timeout, .. } = sql(None) else { panic!() };
            let handler = Handler::Sql { db, user_query, certificate_query: Some(format!("SELECT {column} FROM {USERS} WHERE username = ?")), reply_query, default_user: None, timeout };
            assert_eq!(sql_outcome(&handler, &by_certificate("alice")).await.0, Outcome::Accept, "{column}");
        }
    }

    #[tokio::test]
    async fn test_certificate_query() {
        let Handler::Sql { db, user_query, reply_query, timeout, .. } = sql(None) else { panic!() };
        let handler = Handler::Sql {
            db,
            user_query,
            certificate_query: Some(format!("SELECT 1 FROM {USERS} WHERE username = ? AND username <> 'alice'")),
            reply_query,
            default_user: None,
            timeout,
        };
        assert_eq!(sql_outcome(&handler, &by_certificate("nullpw")).await.0, Outcome::Accept, "the query decides, not the password");
        assert_eq!(sql_outcome(&handler, &by_certificate("prefixpw")).await.0, Outcome::Accept, "the query decides, not the password");
        assert_eq!(sql_outcome(&handler, &by_certificate("alice")).await.0, Outcome::Ignore, "only user_query finds it");
        assert_eq!(sql_outcome(&handler, &by_certificate("twin")).await.0, Outcome::Reject, "two rows");
        assert_eq!(sql_outcome(&handler, &pap("alice", "secret")).await.0, Outcome::Accept, "a password goes on using user_query");
        assert_eq!(sql_outcome(&handler, &pap("nullpw", "x")).await.0, Outcome::Reject);
    }

    #[tokio::test]
    async fn test_certificate_is_no_password() {
        assert!(verify_secret("secret", "alice", &Secret::Certificate).await.is_none());
    }

    #[tokio::test]
    async fn test_sql_handler_outcomes() {
        let handler = sql(None);
        let (outcome, attributes) = sql_outcome(&handler, &pap("alice", "secret")).await;
        assert_eq!(outcome, Outcome::Accept);
        assert_eq!(attributes, vec![attr("Filter-Id", "std.ppp"), attr("Service-Type", "Framed-User")]);
        assert_eq!(sql_outcome(&handler, &pap("alice", "wrong")).await.0, Outcome::Reject);
        assert_eq!(sql_outcome(&handler, &pap("mallory", "secret")).await.0, Outcome::Ignore);
        assert_eq!(sql_outcome(&handler, &pap("x' OR '1'='1", "secret")).await.0, Outcome::Ignore);
        assert_eq!(sql_outcome(&handler, &pap("twin", "a")).await.0, Outcome::Reject, "two rows must reject");
        let no_secret = Credentials { user: Some("alice".to_string()), secret: Secret::None };
        assert_eq!(sql_outcome(&handler, &no_secret).await.0, Outcome::Reject);
        let no_user = Credentials { user: None, secret: Secret::pap(b"secret".to_vec()) };
        assert_eq!(sql_outcome(&handler, &no_user).await.0, Outcome::Ignore);
    }

    #[tokio::test]
    async fn test_record_names_the_account_that_was_accepted() {
        setup_users().await;
        let first_match = AuthChain::new(AuthPolicy::FirstMatch, vec![sql(Some("DEFAULT"))]);
        let accepted = first_match.authenticate(&pap("mallory", "guest-pass"), &request()).await;
        assert_eq!((accepted.decision, accepted.account.as_deref()), (Outcome::Accept, Some("DEFAULT")));
        let refused = first_match.authenticate(&pap("mallory", "wrong"), &request()).await;
        assert_eq!((refused.decision, refused.account.as_deref()), (Outcome::Reject, Some("mallory")), "nobody logged in as the default");
        let own = first_match.authenticate(&pap("alice", "secret"), &request()).await;
        assert_eq!((own.decision, own.account.as_deref()), (Outcome::Accept, Some("alice")));
        let all_accept = AuthChain::new(AuthPolicy::AllAccept, vec![sql(Some("DEFAULT")), lua("return 'accept'")]);
        let accepted = all_accept.authenticate(&pap("mallory", "guest-pass"), &request()).await;
        assert_eq!((accepted.decision, accepted.account.as_deref()), (Outcome::Accept, Some("DEFAULT")));
        let own = all_accept.authenticate(&pap("alice", "secret"), &request()).await;
        assert_eq!((own.decision, own.account.as_deref()), (Outcome::Accept, Some("alice")));
    }

    #[tokio::test]
    async fn test_certificate_name_changes_only_in_ascii_case() {
        let chain = AuthChain::new(AuthPolicy::FirstMatch, vec![lua("return 'accept'")]).with_username(Some(username(StripRealm::None, true, None)));
        let certificate = |name: &str| Credentials { user: Some(name.to_string()), secret: Secret::Certificate };
        let kelvin = chain.authenticate(&certificate("\u{212A}evin"), &request()).await;
        assert_eq!((kelvin.decision, kelvin.handler), (Outcome::Reject, "normalization"), "the Kelvin sign is not a K");
        let accented = chain.authenticate(&certificate("\u{c9}va"), &request()).await;
        assert_eq!((accented.decision, accented.handler), (Outcome::Reject, "normalization"));
        let plain = chain.authenticate(&certificate("KEVIN"), &request()).await;
        assert_eq!((plain.decision, plain.account.as_deref()), (Outcome::Accept, Some("kevin")));
        // With a password, the account still has to be proved.
        assert_eq!(chain.authenticate(&pap("\u{212A}evin", "x"), &request()).await.account.as_deref(), Some("kevin"));
    }

    #[tokio::test]
    async fn test_sql_handler_default_user() {
        let handler = sql(Some("DEFAULT"));
        let (outcome, attributes) = sql_outcome(&handler, &pap("mallory", "guest-pass")).await;
        assert_eq!(outcome, Outcome::Accept);
        assert_eq!(attributes, vec![attr("Filter-Id", "guest")], "reply_query must use the default account");
        assert_eq!(sql_outcome(&handler, &pap("mallory", "wrong")).await.0, Outcome::Reject);
    }

    #[tokio::test]
    async fn test_reply_attributes_merged_in_order() {
        setup_users().await;
        let chain = AuthChain::new(AuthPolicy::AllAccept, vec![
            Handler::Sql {
                db: test_db(),
                user_query: format!("SELECT password FROM {USERS} WHERE username = ?"),
                reply_query: Some("SELECT attribute, value FROM chain_reply WHERE username = ? AND attribute = 'Filter-Id'".to_string()),
                certificate_query: None, default_user: None,
                timeout: Duration::from_secs(5),
            },
            lua("set_attr('Session-Timeout', 3600) return 'accept'"),
        ]);
        let result = chain.authenticate(&pap("alice", "secret"), &request()).await;
        assert!(result.accepted());
        assert_eq!(result.attributes, vec![attr("Filter-Id", "std.ppp"), ("Session-Timeout".to_string(), AttrValue::Number(3600.0))]);
    }

    #[tokio::test]
    async fn test_sql_handler_rejects_missing_stored_password() {
        let handler = sql(None);
        for user in ["nullpw", "emptypw"] {
            assert_eq!(sql_outcome(&handler, &pap(user, "")).await.0, Outcome::Reject, "{user} with an empty PAP password");
            let challenge = [9u8; 16];
            use md5::{Digest, Md5};
            let mut hasher = Md5::new();
            hasher.update([7u8]);
            hasher.update(challenge);
            let response = hasher.finalize().to_vec();
            let chap = Credentials { user: Some(user.to_string()), secret: Secret::Chap { id: 7, response, challenge: challenge.to_vec() } };
            assert_eq!(sql_outcome(&handler, &chap).await.0, Outcome::Reject, "{user} with CHAP over an empty password");
        }
    }

    fn client(name: &str, auth: Option<&str>) -> Client {
        Client { max_radsec_connections: None, radsec_subject: None, authorize_group: None, coa_sender: false, mfa: None, eap_methods: None, name: name.to_string(), secret: b"k".to_vec(), auth: auth.map(str::to_string), require_message_authenticator: Default::default(), limit_proxy_state: Default::default(), coa_port: 3799, tls_name: None }
    }

    #[tokio::test]
    async fn test_chains_for_client() {
        let chains = AuthChains::new(
            Some(AuthChain::new(AuthPolicy::FirstMatch, vec![lua("return 'accept'")])),
            [("vpn".to_string(), AuthChain::new(AuthPolicy::FirstMatch, vec![lua("return 'reject'")]))],
        );
        let creds = pap("alice", "secret");
        let default = chains.for_client(&client("switch", None)).unwrap();
        assert!(default.authenticate(&creds, &request()).await.accepted());
        let vpn = chains.for_client(&client("vpn-gw", Some("vpn"))).unwrap();
        assert!(!vpn.authenticate(&creds, &request()).await.accepted());
        assert!(chains.for_client(&client("odd", Some("missing"))).is_none());

        // Only named chains: clients without `auth` are left to the top-level script.
        let named_only = AuthChains::new(None, [("vpn".to_string(), AuthChain::new(AuthPolicy::FirstMatch, vec![lua("return 'reject'")]))]);
        assert!(named_only.for_client(&client("switch", None)).is_none());
        assert!(named_only.for_client(&client("vpn-gw", Some("vpn"))).is_some());
    }

    #[tokio::test]
    async fn test_shared_chain_builds_one_ldap_handler() {
        use crate::config::{AuthSettings, ChainSettings};
        use crate::ldap_handler::tests::{mock_server, settings};
        use crate::ldap_handler::PasswordCheck;
        let (url, log) = mock_server().await;
        let vpn = ChainSettings {
            policy: AuthPolicy::FirstMatch,
            handlers: vec![AuthHandlerSettings::Ldap(settings(vec![url], PasswordCheck::Bind))],
            username: None,
        };
        let auth = AuthSettings { default: None, chains: [("vpn".to_string(), vpn)].into_iter().collect() };
        let chains = AuthChains::from_settings(&auth, &test_db(), &crate::test_support::test_vms(), &LuaLimits::default()).unwrap();
        // A RADIUS NAS and a TACACS+ router both select `vpn`.
        for name in ["radius-nas", "tacacs-router"] {
            let chain = chains.for_client(&client(name, Some("vpn"))).unwrap();
            assert!(chain.authenticate(&pap("alice", "secret"), &request()).await.accepted(), "{name}");
        }
        // One handler, so one pool: the second client reused the first client's connection.
        assert_eq!(log.lock().unwrap().connections, 1);
    }

    #[tokio::test]
    async fn test_ldap_handler_in_all_accept_chain() {
        use crate::ldap_handler::tests::{mock_server, settings};
        use crate::ldap_handler::PasswordCheck;
        let (url, _) = mock_server().await;
        let mut ldap = settings(vec![url], PasswordCheck::Bind);
        ldap.reply_attributes = vec![("Filter-Id".to_string(), "radiusFilterId".to_string())];
        let chain = AuthChain::new(AuthPolicy::AllAccept, vec![
            Handler::Ldap(LdapHandler::new(ldap).unwrap()),
            lua("set_attr('Session-Timeout', 3600) return 'accept'"),
        ]);
        let result = chain.authenticate(&pap("alice", "secret"), &request()).await;
        assert!(result.accepted());
        assert_eq!(result.attributes, vec![
            attr("Filter-Id", "std.ppp"),
            attr("Filter-Id", "extra"),
            ("Session-Timeout".to_string(), AttrValue::Number(3600.0)),
        ]);
        assert_eq!(chain.authenticate(&pap("alice", "wrong"), &request()).await.decision, Outcome::Reject);
    }

    #[test]
    fn test_ldap_handler_built_from_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rradius.toml");
        std::fs::write(
            &path,
            "dictionary = \"d\"\n[lua]\nscript = \"s\"\n\
             [cluster]\ndata_dir = \"data\"\nsecret_raft = \"raft-secret-0123456\"\nsecret_api = \"api-secret-01234567\"\n\
             [auth]\n[[auth.handlers]]\ntype = \"ldap\"\nurls = [\"ldaps://dc1.example.com\"]\n\
             bind_dn = \"cn=radius,dc=example,dc=com\"\nbind_password = \"pw\"\nbase_dn = \"dc=example,dc=com\"\nfilter = \"(uid={user})\"\n\
             [radius]\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n",
        )
        .unwrap();
        let auth = crate::config::load(&path).unwrap().auth.unwrap();
        let chain = AuthChain::from_settings(auth.default.as_ref().unwrap(), &test_db(), &crate::test_support::test_vms(), &LuaLimits::default()).unwrap();
        assert!(matches!(chain.handlers.as_slice(), [Handler::Ldap(_)]));
    }
}
