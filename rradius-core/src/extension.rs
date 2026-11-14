//! What a program built on this library adds to the server: sections of the configuration it
//! reads itself, commands, work at startup and every minute, and metrics. Each program hands
//! `cli::main` the extensions it has; one process has one list, installed at its start.

use crate::config::ServerConfig;
use crate::lua_engine::Db;
use crate::metrics::Metrics;
use crate::radius::RadiusServer;
use futures::future::{BoxFuture, LocalBoxFuture};
use std::any::Any;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use crate::revocation::Revocation;
use crate::session_resumption::{RequestedResumption, Resumption, ResumptionContext};
use rustls::pki_types::CertificateDer;

/// A command of its own, run as `<program> <name> ...`.
#[derive(Clone, Copy)]
pub struct Command {
    pub name: &'static str,
    /// Its usage, one or more lines, printed with the program's when no command fits.
    pub usage: &'static str,
    /// Runs it with its arguments, the name not included; returns the exit status. On the
    /// thread that starts the program, so it need not be sent to another.
    pub run: fn(Vec<String>) -> LocalBoxFuture<'static, i32>,
}

/// What work at startup sees: the configuration, and the cluster once it can be used.
pub struct StartContext<'a> {
    pub config: &'a ServerConfig,
    pub db: &'a Db,
}

/// What the work every minute sees. The server's settings are the ones the last reload left.
pub struct JobContext {
    pub db: Db,
    pub radius: Arc<RadiusServer>,
}

/// What an extension's server sees when it starts: once the RADIUS and TACACS+ servers are made,
/// before either serves.
pub struct ServeContext<'a> {
    pub config: &'a ServerConfig,
    /// The RADIUS server, when the configuration has `[radius]`.
    pub radius: Option<Arc<RadiusServer>>,
    /// The TACACS+ server, when the configuration has `[tacacs]`.
    pub tacacs: Option<Arc<crate::tacacs::TacacsServer>>,
    pub metrics: Arc<Metrics>,
    /// The cluster database.
    pub db: Db,
    /// Cancelled when the server stops taking requests: an extension's listener closes then, and
    /// its connections end once the request each is handling is answered.
    pub stopping: crate::stopping::Stopping,
}

/// A server an extension started, told of each configuration a reload applies.
pub trait Served: Send + Sync {
    fn reload(&self, config: &ServerConfig);

    /// Stops what must not go on while the program shuts down, such as replaying a spool: what
    /// was written stays on disk for the next start.
    fn stop(&self) {}
}

/// One accounting request that was answered, for an `AccountingObserver`.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountingRecord {
    /// When its Accounting-Response was made.
    pub time: std::time::SystemTime,
    /// The name of the client it came from.
    pub client: String,
    pub source: std::net::IpAddr,
    /// `udp` or `radsec`.
    pub transport: &'static str,
    /// What answered it: `script` when the script accepted it, `spool` when it was written down
    /// to replay or send on later, `proxy` when another server answered it.
    pub handling: &'static str,
    /// Its attributes in the order of the packet, by their names in the dictionary and with their
    /// values as text: a number by its name in the dictionary when it has one, text as it is,
    /// other octets as `0x` and hexadecimal. One the dictionary does not name is `Attr-<type>`,
    /// with its octets in hexadecimal.
    pub attributes: Vec<(String, String)>,
}

/// What is told of each accounting request the RADIUS server answers, once its Accounting-Response
/// is made; a retransmission answered from the duplicate cache is not told again.
pub trait AccountingObserver: Send + Sync {
    /// Returns at once: on the path of the answer, it never waits.
    fn observe(&self, record: &AccountingRecord);
}

/// A section of the configuration that an extension reads, such as `[accounts.aka]`.
#[derive(Clone)]
pub struct Section {
    /// Its dotted path, one of the extension's `sections`.
    pub name: &'static str,
    /// What the file holds there.
    pub value: toml::Value,
    /// The whole file, for errors that say on which line they are.
    text: Arc<str>,
}

impl Section {
    pub(crate) fn new(name: &'static str, value: toml::Value, text: Arc<str>) -> Section {
        Section { name, value, text }
    }

    /// The line, from 1, that a byte offset of the file is on: for a `toml::Spanned` setting that
    /// `parse` read, so that a mistake found after parsing, such as a file that cannot be read, is
    /// named with its line too.
    pub fn line_at(&self, offset: usize) -> usize {
        self.text.as_bytes()[..offset.min(self.text.len())].iter().filter(|byte| **byte == b'\n').count() + 1
    }

    /// Reads it as a `T`. A setting `T` refuses is named with the line it is on, as the core
    /// names its own.
    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> Result<T, String> {
        let path: Vec<&str> = self.name.split('.').collect();
        let read = toml::Deserializer::parse(&self.text).and_then(|deserializer| serde::de::DeserializeSeed::deserialize(At::<T> { path: &path, read: std::marker::PhantomData }, deserializer));
        match read {
            Ok(Some(parsed)) => Ok(parsed),
            // The file always has the section: read what it held, should it not be found.
            Ok(None) => self.value.clone().try_into::<T>().map_err(|e| format!("{}: {}", self.name, e.message().replace('\n', " "))),
            Err(e) => Err(crate::config::syntax_error(&self.text, &e)),
        }
    }
}

/// Reads the `T` at `path` of a document, passing over everything else.
struct At<'p, T> {
    path: &'p [&'p str],
    read: std::marker::PhantomData<T>,
}

impl<'de, T: serde::de::DeserializeOwned> serde::de::DeserializeSeed<'de> for At<'_, T> {
    type Value = Option<T>;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Option<T>, D::Error> {
        match self.path.split_first() {
            None => T::deserialize(deserializer).map(Some),
            Some((first, rest)) => deserializer.deserialize_map(Walk { first, rest, read: self.read }),
        }
    }
}

/// Finds the table `first` in a table, and `rest` of the path in it.
struct Walk<'p, T> {
    first: &'p str,
    rest: &'p [&'p str],
    read: std::marker::PhantomData<T>,
}

impl<'de, T: serde::de::DeserializeOwned> serde::de::Visitor<'de> for Walk<'_, T> {
    type Value = Option<T>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "a table")
    }

    fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<Option<T>, M::Error> {
        let mut found = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == self.first && found.is_none() {
                found = map.next_value_seed(At::<T> { path: self.rest, read: self.read })?;
            } else {
                map.next_value::<serde::de::IgnoredAny>()?;
            }
        }
        Ok(found)
    }
}

/// The parsed settings of an extension's sections, kept in `ServerConfig` and read back by type.
pub type Settings = Box<dyn Any + Send + Sync>;

/// What every extension made of its sections, in `ServerConfig`.
#[derive(Clone, Default)]
pub struct ExtensionSettings(Vec<Arc<dyn Any + Send + Sync>>);

impl ExtensionSettings {
    /// The settings an extension made as a `T`.
    pub fn get<T: Any>(&self) -> Option<&T> {
        self.0.iter().find_map(|settings| settings.downcast_ref::<T>())
    }
}

impl std::fmt::Debug for ExtensionSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ExtensionSettings({} kept)", self.0.len())
    }
}

/// Sections that only `rradius-enterprise` reads, with what the message of a program without
/// them says.
const SECTIONS_NEEDING_ENTERPRISE: [(&str, &str); 14] = [
    ("radius.eap.aka", "EAP-AKA' needs rradius-enterprise"),
    ("accounts.aka", "EAP-AKA' needs rradius-enterprise"),
    ("radius.tls", "RadSec needs rradius-enterprise"),
    ("radius.dpsk", "DPSK needs rradius-enterprise"),
    ("accounts.dpsk", "DPSK needs rradius-enterprise"),
    ("radius.dynamic_clients", "dynamic clients need rradius-enterprise"),
    ("accounts.clients", "dynamic clients need rradius-enterprise"),
    ("radius.coa", "CoA needs rradius-enterprise"),
    ("proxy", "proxying needs rradius-enterprise"),
    ("tacacs.tls", "TACACS+ over TLS needs rradius-enterprise"),
    ("cluster.backup", "backups need rradius-enterprise"),
    ("accounting_export", "accounting export needs rradius-enterprise"),
    ("mfa", "two-factor login needs rradius-enterprise"),
    ("diameter", "Diameter needs rradius-enterprise"),
];

/// Commands that only `rradius-enterprise` has, likewise.
const COMMANDS_NEEDING_ENTERPRISE: [(&str, &str); 8] = [
    ("aka", "EAP-AKA' needs rradius-enterprise"),
    ("aka-seal", "EAP-AKA' needs rradius-enterprise"),
    ("client", "dynamic clients need rradius-enterprise"),
    ("dpsk", "DPSK needs rradius-enterprise"),
    ("dpsk-seal", "DPSK needs rradius-enterprise"),
    ("disconnect", "CoA needs rradius-enterprise"),
    ("coa", "CoA needs rradius-enterprise"),
    ("otp", "two-factor login needs rradius-enterprise"),
];

/// EAP methods that only `rradius-enterprise` offers, likewise.
const EAP_METHODS_NEEDING_ENTERPRISE: [(&str, &str); 1] = [("aka-prime", "EAP-AKA' needs rradius-enterprise")];

/// What a program without the EAP method `name`, which only `rradius-enterprise` offers, says of
/// it.
pub fn eap_method_needs_enterprise(name: &str) -> Option<&'static str> {
    EAP_METHODS_NEEDING_ENTERPRISE.iter().find(|(needed, _)| *needed == name).map(|(_, hint)| *hint)
}

/// What a program without the section `name`, which only `rradius-enterprise` reads, says of it.
pub fn section_needs_enterprise(name: &str) -> Option<&'static str> {
    SECTIONS_NEEDING_ENTERPRISE.iter().find(|(needed, _)| *needed == name).map(|(_, hint)| *hint)
}

/// What a program without the command `name`, which only `rradius-enterprise` has, says of it.
pub fn command_needs_enterprise(name: &str) -> Option<&'static str> {
    COMMANDS_NEEDING_ENTERPRISE.iter().find(|(needed, _)| *needed == name).map(|(_, hint)| *hint)
}

/// Hands each extension the sections of `found` it reads, and keeps what it makes of them. Every
/// extension is asked, with the sections it reads that the file has, which may be none. A section
/// no extension reads is an error.
pub(crate) fn settings(extensions: &Extensions, found: Vec<Section>, base: &Path) -> Result<ExtensionSettings, String> {
    if let Some(section) = found.iter().find(|section| extensions.owner(section.name).is_none()) {
        let hint = section_needs_enterprise(section.name).map_or(String::new(), |hint| format!(" ({hint})"));
        return Err(format!("{}: not supported by this program{hint}", section.name));
    }
    let mut kept = Vec::new();
    for extension in extensions.iter() {
        let its = found.iter().filter(|section| extension.sections().contains(&section.name)).cloned().collect();
        kept.extend(extension.configure(its, base, extensions)?.into_iter().map(Arc::from));
    }
    Ok(ExtensionSettings(kept))
}

/// What `status` shows of an item: the current value, or how fast a counter goes up together
/// with its total.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Show {
    /// The value now, as of a gauge.
    Value,
    /// The change per second of a counter, and its total.
    Rate,
    /// The total of a counter alone.
    Total,
    /// A quantile (0.5 for the median) of what a histogram counted between the two samples, in
    /// milliseconds; the item's metric is the family name, without `_bucket`. Left out when
    /// nothing was counted.
    Quantile(f64),
}

/// One line of a `status` section: a metric, with the values of its labels when it has some.
#[derive(Debug, Clone, Copy)]
pub struct StatusItem {
    /// Shown to people.
    pub label: &'static str,
    /// Stays the same between versions: lowercase and `_`.
    pub key: &'static str,
    pub metric: &'static str,
    /// Every label of the sample, by name and value; none for the sum of all its samples.
    pub labels: &'static [(&'static str, &'static str)],
    pub show: Show,
}

/// A section of `status`. An item whose metric is not reported is left out, and a section left
/// with none is not shown.
#[derive(Debug, Clone)]
pub struct StatusSection {
    pub title: &'static str,
    /// Stays the same between versions: lowercase and `_`.
    pub key: &'static str,
    pub items: Vec<StatusItem>,
}

pub trait Extension: Send + Sync {
    /// Its name, in messages.
    fn name(&self) -> &'static str;

    /// The sections of the configuration it reads, as dotted paths such as "accounts.aka".
    fn sections(&self) -> &'static [&'static str] {
        &[]
    }

    /// The JSON Schema of each section it reads, under the section's dotted path, which the
    /// `schema` command adds to the schema of the configuration file.
    fn schemas(&self) -> Vec<(&'static str, schemars::Schema)> {
        Vec::new()
    }

    /// The descriptions of the fields of its sections, in the TOML form of the core's
    /// `schema_descriptions.toml`, which the `schema` command adds to their properties.
    fn schema_descriptions(&self) -> &'static str {
        ""
    }

    /// The connections its settings in `config` may hold open at once, by name, which the
    /// `tuning` command adds to the open files the configuration needs. A non-empty list also
    /// means a TCP port is listened on.
    fn tuning_connections(&self, _config: &crate::config::ServerConfig) -> Vec<(&'static str, usize)> {
        Vec::new()
    }

    /// Reads its sections: those of `sections` the file has; `base` is the
    /// directory of the file, `extensions` those of the program, for the revocation check of a
    /// TLS section. What it returns goes into `ServerConfig`: settings of its own, and settings of
    /// types the core defines and reads.
    fn configure(&self, found: Vec<Section>, base: &Path, extensions: &Extensions) -> Result<Vec<Settings>, String> {
        let _ = (found, base, extensions);
        Ok(Vec::new())
    }

    fn commands(&self) -> Vec<Command> {
        Vec::new()
    }

    /// Runs once the cluster can be used, before the servers start; an error stops the start.
    fn start<'a>(&'a self, context: &'a StartContext<'a>) -> BoxFuture<'a, Result<(), String>> {
        let _ = context;
        Box::pin(async { Ok(()) })
    }

    /// Runs every minute while the RADIUS server serves.
    fn every_minute<'a>(&'a self, context: &'a JobContext) -> BoxFuture<'a, ()> {
        let _ = context;
        Box::pin(async {})
    }

    /// Starts its server, once the RADIUS and TACACS+ servers are made and before either serves,
    /// also without `[radius]`; the server runs on a task of its own. An error stops the start.
    fn serve(&self, context: &ServeContext<'_>) -> Result<Option<Arc<dyn Served>>, String> {
        let _ = context;
        Ok(None)
    }

    /// The settings of its own that differ between the two configurations and that a reload
    /// does not apply, named as the reload's log names them.
    fn needs_restart(&self, running: &ServerConfig, new: &ServerConfig) -> Vec<&'static str> {
        let _ = (running, new);
        Vec::new()
    }

    /// Clients the configuration does not list, for the RADIUS server to find UDP sources in;
    /// asked at the start and at each reload, whose source replaces the last.
    fn client_source(&self, context: &crate::client_source::ClientSourceContext<'_>) -> Option<Arc<dyn crate::client_source::ClientSource>> {
        let _ = context;
        None
    }

    /// What RADIUS requests for other servers are forwarded with; asked at the start and at each
    /// reload, whose forwarder replaces the last once the reload succeeds. An error refuses the
    /// configuration.
    fn forwarder(&self, context: &crate::forwarding::ForwardingContext<'_>) -> Result<Option<Arc<dyn crate::forwarding::Forwarder>>, String> {
        let _ = context;
        Ok(None)
    }

    /// What is told of the accounting requests the RADIUS server answers; asked once, at the
    /// start. `None`, as for every extension of a program without one, makes no record at all.
    fn accounting_observer(&self, config: &ServerConfig) -> Option<Arc<dyn AccountingObserver>> {
        let _ = config;
        None
    }

    /// What proves the identity of requests before the chain runs, such as per-user pre-shared
    /// keys; asked at the start and at each reload, whose authenticator replaces the last.
    fn pre_chain(&self, context: &crate::pre_chain::PreChainContext<'_>) -> Option<Arc<dyn crate::pre_chain::PreChainAuthenticator>> {
        let _ = context;
        None
    }

    /// The second factor of the clients with `mfa`, such as a one-time code; asked at the start
    /// and at each reload, whose second factor replaces the last.
    fn second_factor(&self, context: &crate::second_factor::SecondFactorContext<'_>) -> Option<Arc<dyn crate::second_factor::SecondFactor>> {
        let _ = context;
        None
    }

    /// The EAP methods it offers beside EAP-TLS, PEAP and TTLS, made from a configuration as it
    /// is loaded; a conversation keeps those of the load it began under.
    fn eap_methods(&self, config: &ServerConfig) -> Vec<Arc<dyn crate::eap_method::EapMethod>> {
        let _ = config;
        Vec::new()
    }

    /// The names of the EAP methods `eap_methods` can make, whether or not the configuration
    /// asks for them: `rradius_eap_sessions_total` counts each from the start.
    fn eap_method_names(&self) -> Vec<crate::eap_method::EapMethodName> {
        Vec::new()
    }

    /// What a script's `send_disconnect` and `send_coa` send through; asked at the start and at
    /// each reload.
    fn dynamic_authorization(&self, context: &crate::dynamic_authorization::DynamicAuthorizationContext<'_>) -> Option<Arc<dyn crate::dynamic_authorization::DynamicAuthorization>> {
        let _ = context;
        None
    }

    /// Checks what its settings need of the rest of the configuration, once it is loaded; an
    /// error refuses the configuration.
    fn check(&self, config: &ServerConfig) -> Result<(), String> {
        let _ = config;
        Ok(())
    }

    /// Checks what `check` checks on disk without changing it, such as whether a directory it
    /// would write to can be used.
    fn check_files(&self, config: &ServerConfig) -> Result<(), String> {
        let _ = config;
        Ok(())
    }

    /// Logs what is weak in its settings, such as short secrets, where the server warns about
    /// its own: at the start, at each reload and in `check`.
    fn warn_about(&self, config: &ServerConfig) {
        let _ = config;
    }

    /// Writes its metrics, in the Prometheus text format, after the server's own.
    fn render_metrics(&self, out: &mut String) {
        let _ = out;
    }

    /// The sections `status` shows of its metrics, after the server's own. Every metric they
    /// name has to be one `render_metrics` writes.
    fn status_sections(&self) -> Vec<StatusSection> {
        Vec::new()
    }

    /// The settings of a TLS section it reads to check revocation, such as "ocsp".
    fn revocation_settings(&self) -> &'static [&'static str] {
        &[]
    }

    /// The revocation check of `section` ("radius.eap" and the like): `found` are those of its
    /// settings the section has, `cas` the CAs of its `client_ca_file`. `None` checks nothing.
    fn revocation(&self, section: &str, found: Vec<(&'static str, toml::Value)>, cas: Vec<CertificateDer<'static>>) -> Result<Option<Revocation>, String> {
        let _ = (section, found, cas);
        Ok(None)
    }

    /// Session resumption from the settings of `[radius.eap]` that ask for it: `None` when this
    /// extension does not resume sessions; `Some(Ok(None))` when the settings resume nothing.
    /// An error is about the settings, and starts with the path of the one it is about.
    fn session_resumption(&self, requested: &RequestedResumption, context: &ResumptionContext) -> Option<Result<Option<Resumption>, String>> {
        let _ = (requested, context);
        None
    }
}

/// The commands of the program itself, which no extension can take.
/// The old `--config` and `--check` stay reserved, so that they always reach the line naming the
/// subcommand to use.
const PROGRAM_COMMANDS: [&str; 12] = ["--version", "--help", "-h", "help", "--config", "--check", "server", "check", "schema", "config", "tuning", "user"];

/// The extensions of one program.
#[derive(Clone, Default)]
pub struct Extensions {
    list: Vec<Arc<dyn Extension>>,
}

impl Extensions {
    /// Refuses two extensions that read the same section or have a command of the same name,
    /// and a command the program has itself.
    pub fn new(list: Vec<Arc<dyn Extension>>) -> Result<Extensions, String> {
        let (mut sections, mut commands, mut revocation) = (HashSet::new(), HashSet::new(), HashSet::new());
        for extension in &list {
            for setting in extension.revocation_settings() {
                if !revocation.insert(*setting) {
                    return Err(format!("extension {}: another extension reads `{setting}` too", extension.name()));
                }
            }
            for section in extension.sections() {
                if !sections.insert(*section) {
                    return Err(format!("extension {}: another extension reads [{section}] too", extension.name()));
                }
            }
            for command in extension.commands() {
                if PROGRAM_COMMANDS.contains(&command.name) {
                    return Err(format!("extension {}: `{}` is a command of the program itself", extension.name(), command.name));
                }
                if !commands.insert(command.name) {
                    return Err(format!("extension {}: another extension has the command `{}` too", extension.name(), command.name));
                }
            }
        }
        Ok(Extensions { list })
    }

    /// These and `extension`, checked as `new` checks.
    pub fn with(self, extension: Arc<dyn Extension>) -> Result<Extensions, String> {
        let mut list = self.list;
        list.push(extension);
        Extensions::new(list)
    }

    /// The extension that reads the revocation setting `setting`.
    pub fn revocation_owner(&self, setting: &str) -> Option<&Arc<dyn Extension>> {
        self.list.iter().find(|extension| extension.revocation_settings().contains(&setting))
    }

    /// Session resumption from the first extension that resumes sessions; `None` when none does.
    pub fn session_resumption(&self, requested: &RequestedResumption, context: &ResumptionContext) -> Option<Result<Option<Resumption>, String>> {
        self.list.iter().find_map(|extension| extension.session_resumption(requested, context))
    }

    /// Without the checks of `new`, for a list known to pass them.
    pub(crate) fn unchecked(list: Vec<Arc<dyn Extension>>) -> Extensions {
        Extensions { list }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Extension>> {
        self.list.iter()
    }

    /// The extension that reads `section`.
    pub fn owner(&self, section: &str) -> Option<&Arc<dyn Extension>> {
        self.list.iter().find(|extension| extension.sections().contains(&section))
    }

    /// The command called `name`.
    pub fn command(&self, name: &str) -> Option<Command> {
        self.list.iter().flat_map(|extension| extension.commands()).find(|command| command.name == name)
    }

    /// The usage of every command, in the order the extensions came.
    pub fn usage(&self) -> Vec<&'static str> {
        let mut usage: Vec<&'static str> = self.list.iter().flat_map(|extension| extension.commands()).map(|command| command.usage).collect();
        // Commands that share their usage, as `disconnect` and `coa` do, print it once.
        usage.dedup();
        usage
    }

    /// Runs each extension's work at startup in turn, stopping at the first error.
    pub async fn start(&self, context: &StartContext<'_>) -> Result<(), String> {
        for extension in &self.list {
            extension.start(context).await?;
        }
        Ok(())
    }

    /// Runs each extension's work of the minute in turn.
    pub async fn every_minute(&self, context: &JobContext) {
        for extension in &self.list {
            extension.every_minute(context).await;
        }
    }

    /// Starts each extension's server in turn, stopping at the first error.
    pub fn serve(&self, context: &ServeContext<'_>) -> Result<Vec<Arc<dyn Served>>, String> {
        let mut served = Vec::new();
        for extension in &self.list {
            served.extend(extension.serve(context)?);
        }
        Ok(served)
    }

    /// What each extension says needs a restart, in the order the extensions came.
    pub fn needs_restart(&self, running: &ServerConfig, new: &ServerConfig) -> Vec<&'static str> {
        self.list.iter().flat_map(|extension| extension.needs_restart(running, new)).collect()
    }

    /// The sender of the first extension that has one.
    pub fn dynamic_authorization(&self, context: &crate::dynamic_authorization::DynamicAuthorizationContext<'_>) -> Option<Arc<dyn crate::dynamic_authorization::DynamicAuthorization>> {
        self.list.iter().find_map(|extension| extension.dynamic_authorization(context))
    }

    /// Has each extension warn about its settings.
    pub fn warn_about(&self, config: &ServerConfig) {
        for extension in &self.list {
            extension.warn_about(config);
        }
    }

    /// Runs each extension's check of a loaded configuration, stopping at the first error.
    pub fn check(&self, config: &ServerConfig) -> Result<(), String> {
        self.list.iter().try_for_each(|extension| extension.check(config))
    }

    /// What `check` checks on disk for each extension, stopping at the first error.
    pub fn check_files(&self, config: &ServerConfig) -> Result<(), String> {
        self.list.iter().try_for_each(|extension| extension.check_files(config))
    }

    /// The forwarder of the first extension that has one.
    pub fn forwarder(&self, context: &crate::forwarding::ForwardingContext<'_>) -> Result<Option<Arc<dyn crate::forwarding::Forwarder>>, String> {
        for extension in &self.list {
            if let Some(forwarder) = extension.forwarder(context)? {
                return Ok(Some(forwarder));
            }
        }
        Ok(None)
    }

    /// The client source of the first extension that has one.
    pub fn client_source(&self, context: &crate::client_source::ClientSourceContext<'_>) -> Option<Arc<dyn crate::client_source::ClientSource>> {
        self.list.iter().find_map(|extension| extension.client_source(context))
    }

    /// The accounting observer of the first extension that has one.
    pub fn accounting_observer(&self, config: &ServerConfig) -> Option<Arc<dyn AccountingObserver>> {
        self.list.iter().find_map(|extension| extension.accounting_observer(config))
    }

    /// The pre-chain authenticator of the first extension that has one.
    pub fn pre_chain(&self, context: &crate::pre_chain::PreChainContext<'_>) -> Option<Arc<dyn crate::pre_chain::PreChainAuthenticator>> {
        self.list.iter().find_map(|extension| extension.pre_chain(context))
    }

    /// The second factor of the first extension that has one.
    pub fn second_factor(&self, context: &crate::second_factor::SecondFactorContext<'_>) -> Option<Arc<dyn crate::second_factor::SecondFactor>> {
        self.list.iter().find_map(|extension| extension.second_factor(context))
    }

    /// The EAP methods of all of them, made from `config`.
    pub fn eap_methods(&self, config: &ServerConfig) -> Vec<Arc<dyn crate::eap_method::EapMethod>> {
        self.list.iter().flat_map(|extension| extension.eap_methods(config)).collect()
    }

    pub fn eap_method_names(&self) -> Vec<crate::eap_method::EapMethodName> {
        self.list.iter().flat_map(|extension| extension.eap_method_names()).collect()
    }

    pub fn render_metrics(&self, out: &mut String) {
        for extension in &self.list {
            extension.render_metrics(out);
        }
    }

    /// The `status` sections of the extensions, in their order.
    pub fn status_sections(&self) -> Vec<StatusSection> {
        self.list.iter().flat_map(|extension| extension.status_sections()).collect()
    }
}

static INSTALLED: OnceLock<Extensions> = OnceLock::new();

/// Makes `extensions` the ones of this process. Only once, before anything reads them.
pub fn install(extensions: Extensions) -> Result<(), String> {
    INSTALLED.set(extensions).map_err(|_| "the extensions of this program were already installed".to_string())
}

/// The extensions of this process: the installed ones, or else those this library has built in,
/// as for tests that use the library directly.
pub fn installed() -> &'static Extensions {
    INSTALLED.get_or_init(crate::extensions::builtin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Metrics;

    /// An extension for these tests: reads `sections`, has `commands`, and fails its start when asked.
    struct Probe {
        sections: &'static [&'static str],
        commands: Vec<Command>,
        fail_start: bool,
    }

    impl Extension for Probe {
        fn name(&self) -> &'static str {
            "probe"
        }
        fn sections(&self) -> &'static [&'static str] {
            self.sections
        }
        fn commands(&self) -> Vec<Command> {
            self.commands.clone()
        }
        fn start<'a>(&'a self, _context: &'a StartContext<'a>) -> BoxFuture<'a, Result<(), String>> {
            let fail = self.fail_start;
            Box::pin(async move { if fail { Err("probe: cannot start".to_string()) } else { Ok(()) } })
        }
        fn render_metrics(&self, out: &mut String) {
            out.push_str("# HELP probe_total A probe.\n# TYPE probe_total counter\nprobe_total 7\n");
        }
    }

    fn command(name: &'static str) -> Command {
        Command { name, usage: "usage: probe", run: |args| Box::pin(async move { args.len() as i32 + 40 }) }
    }

    fn probe(sections: &'static [&'static str], commands: Vec<Command>) -> Arc<dyn Extension> {
        Arc::new(Probe { sections, commands, fail_start: false })
    }

    #[test]
    fn test_refuses_what_two_would_share() {
        let both_read = Extensions::new(vec![probe(&["accounts.x"], vec![]), probe(&["accounts.x"], vec![])]);
        assert!(both_read.err().is_some_and(|e| e.contains("[accounts.x]")));
        let both_have = Extensions::new(vec![probe(&[], vec![command("x")]), probe(&[], vec![command("x")])]);
        assert!(both_have.err().is_some_and(|e| e.contains("`x`")));
        // The subcommands of the program, `server` and `check` among them.
        assert!(PROGRAM_COMMANDS.contains(&"server") && PROGRAM_COMMANDS.contains(&"check"));
        for name in PROGRAM_COMMANDS {
            let taken = Extensions::new(vec![probe(&[], vec![command(name)])]);
            assert!(taken.err().is_some_and(|e| e.contains("of the program itself")), "{name}");
        }
        assert!(Extensions::new(vec![probe(&["accounts.x"], vec![command("x")]), probe(&["accounts.y"], vec![command("y")])]).is_ok());
    }

    /// An extension that resumes sessions, refusing `resumption_ms` above the number it holds.
    struct Resumes(u64);

    impl Extension for Resumes {
        fn name(&self) -> &'static str {
            "resumes"
        }
        fn session_resumption(&self, requested: &RequestedResumption, _context: &ResumptionContext) -> Option<Result<Option<Resumption>, String>> {
            Some(match requested.resumption_ms {
                Some(ms) if ms > self.0 => Err(format!("radius.eap.resumption_ms must be at most {}", self.0)),
                _ => Ok(None),
            })
        }
    }

    #[test]
    fn test_session_resumption_from_the_extension_that_has_it() -> Result<(), String> {
        let context = ResumptionContext { base: Path::new("."), methods: &[13], max_sessions: 16 };
        let requested = RequestedResumption { resumption_ms: Some(10), ..RequestedResumption::default() };
        // None of them resumes sessions: nothing answers.
        let none = Extensions::new(vec![probe(&["accounts.x"], vec![])])?;
        assert!(none.session_resumption(&requested, &context).is_none());
        // The first that does answers, whatever comes after it.
        let some = Extensions::new(vec![probe(&["accounts.x"], vec![]), Arc::new(Resumes(5)), Arc::new(Resumes(50))])?;
        let error = some.session_resumption(&requested, &context).ok_or("no answer")?.err().ok_or("10 accepted")?;
        assert_eq!(error, "radius.eap.resumption_ms must be at most 5");
        let quiet = RequestedResumption { resumption_ms: Some(0), ..RequestedResumption::default() };
        assert!(some.session_resumption(&quiet, &context).ok_or("no answer")??.is_none());
        Ok(())
    }

    #[test]
    fn test_first_resumption_setting() {
        let path = || Some(std::path::PathBuf::from("ticket.seal"));
        assert_eq!(RequestedResumption::default().first_set(), None);
        assert_eq!(RequestedResumption { resumption_ms: Some(0), shared_tickets: Some(true), ..RequestedResumption::default() }.first_set(), Some("resumption_ms"));
        assert_eq!(RequestedResumption { shared_tickets: Some(false), ticket_seal_file: path(), ..RequestedResumption::default() }.first_set(), Some("shared_tickets"));
        assert_eq!(RequestedResumption { ticket_seal_file: path(), previous_ticket_seal_file: path(), ..RequestedResumption::default() }.first_set(), Some("ticket_seal_file"));
        assert_eq!(RequestedResumption { previous_ticket_seal_file: path(), ..RequestedResumption::default() }.first_set(), Some("previous_ticket_seal_file"));
    }

    /// An extension that only reads revocation settings.
    struct Reads(&'static [&'static str]);

    impl Extension for Reads {
        fn name(&self) -> &'static str {
            "reads"
        }
        fn revocation_settings(&self) -> &'static [&'static str] {
            self.0
        }
    }

    #[test]
    fn test_refuses_two_readers_of_a_revocation_setting() -> Result<(), String> {
        let both = Extensions::new(vec![Arc::new(Reads(&["ocsp"])), Arc::new(Reads(&["crl_url", "ocsp"]))]);
        assert!(both.err().is_some_and(|e| e.contains("`ocsp`")));
        let one = Extensions::new(vec![Arc::new(Reads(&["ocsp"]))])?;
        assert!(one.revocation_owner("ocsp").is_some() && one.revocation_owner("ocsp_url").is_none());
        assert!(one.with(Arc::new(Reads(&["ocsp"]))).err().is_some_and(|e| e.contains("`ocsp`")));
        Ok(())
    }

    #[test]
    fn test_builtin_extensions_are_accepted() -> Result<(), String> {
        let builtin = crate::extensions::builtin();
        Extensions::new(builtin.list).map(|_| ())
    }

    #[tokio::test]
    async fn test_finds_commands_and_sections() -> Result<(), String> {
        let extensions = Extensions::new(vec![probe(&["accounts.x"], vec![command("x")])])?;
        let found = extensions.command("x").ok_or("no command x")?;
        assert_eq!((found.run)(vec!["a".to_string(), "b".to_string()]).await, 42);
        assert!(extensions.command("y").is_none());
        assert_eq!(extensions.usage(), ["usage: probe"]);
        assert!(extensions.owner("accounts.x").is_some() && extensions.owner("accounts").is_none());
        Ok(())
    }

    #[tokio::test]
    async fn test_start_stops_at_an_error() -> Result<(), String> {
        let db = crate::test_support::test_db();
        let (_dir, config) = crate::test_support::minimal_config()?;
        let context = StartContext { config: &config, db: &db };
        let failing = Extensions::new(vec![probe(&[], vec![]), Arc::new(Probe { sections: &[], commands: vec![], fail_start: true })])?;
        assert_eq!(failing.start(&context).await, Err("probe: cannot start".to_string()));
        assert_eq!(Extensions::new(vec![probe(&[], vec![])])?.start(&context).await, Ok(()));
        Ok(())
    }

    /// An extension that reads `accounts.two` into two settings of different types.
    struct Two;

    impl Extension for Two {
        fn name(&self) -> &'static str {
            "two"
        }
        fn sections(&self) -> &'static [&'static str] {
            &["accounts.two"]
        }
        fn configure(&self, found: Vec<Section>, _base: &Path, _extensions: &Extensions) -> Result<Vec<Settings>, String> {
            let count = found.len();
            Ok(vec![Box::new(count), Box::new(format!("{count} found"))])
        }
    }

    #[test]
    fn test_configure_keeps_every_setting_it_returns() -> Result<(), String> {
        let extensions = Extensions::new(vec![Arc::new(Two)])?;
        let kept = settings(&extensions, vec![Section::new("accounts.two", toml::Value::Boolean(true), Arc::from(""))], Path::new(""))?;
        assert_eq!((kept.get::<usize>(), kept.get::<String>().map(String::as_str)), (Some(&1), Some("1 found")));
        Ok(())
    }

    #[test]
    fn test_metrics_come_after_the_servers() -> Result<(), String> {
        let extensions = Extensions::new(vec![probe(&[], vec![])])?;
        let rendered = Metrics::default().render_with(&extensions);
        assert!(rendered.ends_with("probe_total 7\n"), "{rendered}");
        assert!(rendered.starts_with(&Metrics::default().render_with(&Extensions::default())));
        Ok(())
    }

    /// Notes what `serve` was given, and keeps its `stopping`.
    struct SeesServers(Arc<std::sync::Mutex<Vec<(bool, bool, bool)>>>);

    static KEPT_STOPPING: std::sync::Mutex<Option<crate::stopping::Stopping>> = std::sync::Mutex::new(None);

    fn kept_stopping() -> Result<crate::stopping::Stopping, String> {
        KEPT_STOPPING.lock().map_err(|e| e.to_string())?.clone().ok_or_else(|| "serve kept no stopping".to_string())
    }

    impl Extension for SeesServers {
        fn name(&self) -> &'static str {
            "sees-servers"
        }

        fn serve(&self, context: &ServeContext<'_>) -> Result<Option<Arc<dyn Served>>, String> {
            self.0.lock().map_err(|e| e.to_string())?.push((context.radius.is_some(), context.tacacs.is_some(), context.stopping.is_cancelled()));
            *KEPT_STOPPING.lock().map_err(|e| e.to_string())? = Some(context.stopping.clone());
            Ok(None)
        }
    }

    /// A configuration with `[tacacs]` and no `[radius]` starts the extensions too, and they get
    /// the TACACS+ server.
    #[tokio::test]
    async fn test_serve_without_radius() -> Result<(), String> {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let extensions = Extensions::new(vec![Arc::new(SeesServers(seen.clone()))])?;
        let (_dir, config) = crate::test_support::minimal_config()?;
        let engine = Arc::new(crate::lua_engine::LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'accept'").map_err(|e| e.to_string())?);
        let tacacs = Arc::new(crate::tacacs::TacacsServer::new("127.0.0.1:0".parse().map_err(|_| "address")?, Arc::new(crate::clients::ClientTable::default()), engine, 4096));
        let stopping = crate::stopping::Stopping::new();
        let context = ServeContext { config: &config, radius: None, tacacs: Some(tacacs), metrics: Arc::new(Metrics::default()), db: crate::test_support::test_db(), stopping: stopping.clone() };
        assert!(extensions.serve(&context)?.is_empty());
        assert_eq!(seen.lock().map_err(|e| e.to_string())?.clone(), vec![(false, true, false)]);
        // What the extension kept of the context is cancelled with the server's.
        stopping.cancel();
        let kept = kept_stopping()?;
        assert!(kept.is_cancelled());
        Ok(())
    }
}
