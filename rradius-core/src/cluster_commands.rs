//! `rradiusd cluster ...`: lists the cluster's members and removes a node, through the
//! management API every node serves on its api port. A node joins on its own (a configuration
//! listing it, an empty data directory), so there is nothing to add.
//!
//! The leader is found by asking the nodes of `[[cluster.nodes]]` in order, trusting only a node
//! that sees a leader: a node removed from the cluster but still running sees none, and answers
//! from stale data.
//!
//! A serving node asks the same API whether the cluster still has it, so that a removed node ends
//! instead of answering from that data: see `standing_in_cluster`.

use crate::accounts::{CLUSTER_WAIT, STATUS_MISTAKE, STATUS_NO_MATCH, STATUS_UNREACHABLE};
use crate::config::{self, ClusterNode};
use crate::lua_engine::LEADER_QUORUM_ACK_WITHIN_MS;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub const USAGE: &str = "usage: rradiusd cluster members|remove [--config <path>] [--force] [<node_id>]\n  remove takes a node id, and --force to leave one or two voters";

/// How long one node has to answer one request. The cluster's own clients give up connecting
/// after the same time.
const REQUEST_WAIT: Duration = Duration::from_secs(5);
/// How long a node has to answer before it counts as stopped.
const PING_WAIT: Duration = Duration::from_secs(2);
/// Between two rounds of asking every node.
const ROUND_PAUSE: Duration = Duration::from_secs(1);
/// What the database is called in the management API's paths.
const RAFT: &str = "sqlite";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Members,
    Remove { node: u64, force: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Parsed {
    action: Action,
    config_path: Option<String>,
}

/// One node as the cluster has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Member {
    pub(crate) raft: String,
    pub(crate) api: String,
}

/// The cluster's members: voters and every node with its addresses.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Members {
    pub(crate) voters: BTreeSet<u64>,
    pub(crate) nodes: BTreeMap<u64, Member>,
}

/// Where a list of members came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// A node of the cluster said so.
    Cluster,
    /// No node answered: `[[cluster.nodes]]` of the configuration.
    Configuration,
}

/// What a node said about the cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Metrics {
    id: u64,
    leader: Option<u64>,
    members: Members,
    /// The log index of the membership in `members`.
    membership_index: Option<u64>,
    /// The last log index the node has applied.
    applied_index: Option<u64>,
    /// How long ago a quorum acknowledged the node, which only a leader says.
    quorum_ack_millis: Option<u64>,
}

impl Metrics {
    /// Whether this is a leader speaking for its cluster: it takes itself for the leader, a
    /// quorum acknowledged it recently, and it has applied the membership it gives, so that one
    /// is committed. An answer missing any of these confirms nothing.
    fn is_confirmed_leader(&self) -> bool {
        self.leader == Some(self.id)
            && self.quorum_ack_millis.is_some_and(|millis| millis <= LEADER_QUORUM_ACK_WITHIN_MS)
            && self.membership_index.zip(self.applied_index).is_some_and(|(membership, applied)| applied >= membership)
    }
}

/// This node's own copy of the membership, as its replica has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnMembership {
    /// Its log index, 0 for the one a cluster starts with.
    pub(crate) index: u64,
    /// Every member's id and api address.
    pub(crate) nodes: BTreeMap<u64, String>,
}

/// Whether the cluster still has this node, as the other nodes' answers show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Standing {
    Member,
    Removed(Removal),
    /// No answer settles it: the node goes on as if it had not asked.
    Unknown,
}

/// The leader that no longer lists a node, and what it lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Removal {
    node: u64,
    leader: u64,
    members: BTreeMap<u64, Member>,
}

impl Removal {
    /// What the node logs as it ends. A process manager starts it again and again, and this line
    /// is what the operator sees, so it says both ways out, and to look before emptying anything.
    pub(crate) fn message(&self, data_dir: &Path) -> String {
        let members: Vec<String> = self.members.iter().map(|(id, member)| format!("node {id} at {}", member.api)).collect();
        format!(
            "node {} has been removed from the cluster: node {}, its leader, does not list it (its members: {}). \
             To retire this host, stop and disable the service. To add it back, first confirm that this is the cluster you expect, \
             then empty data_dir '{}' and start it again",
            self.node,
            self.leader,
            members.join(", "),
            data_dir.display()
        )
    }
}

/// Decides from the other nodes' `answers` whether the cluster still has `node`. Only a confirmed
/// leader other than the node counts. One that lists it, voter or not, settles that it is a
/// member. Otherwise it is removed when such a leader's membership is later in the log than the
/// node's own, or the node's own already omits it: a cluster that another node made from empty
/// data starts its log again, so its membership is not later than that of a cluster that formed.
fn standing(node: u64, own: &OwnMembership, answers: &[Metrics]) -> Standing {
    let leaders = || answers.iter().filter(|answer| answer.id != node && answer.is_confirmed_leader());
    if leaders().any(|leader| leader.members.nodes.contains_key(&node)) {
        return Standing::Member;
    }
    let omitted_already = !own.nodes.contains_key(&node);
    match leaders().find(|leader| omitted_already || leader.membership_index.is_some_and(|index| index > own.index)) {
        Some(leader) => Standing::Removed(Removal { node, leader: leader.id, members: leader.members.nodes.clone() }),
        None => Standing::Unknown,
    }
}

/// Asks the other nodes whether the cluster still has `node`: those `configured` lists and those
/// of the node's own membership, all at once, each for up to `PING_WAIT`, then any leader they
/// name that has not been asked. It waits for every answer, so that one cluster that omits the
/// node cannot outrun another that lists it. A node that does not answer, refuses the secret or
/// answers something unreadable says nothing.
pub(crate) async fn standing_in_cluster(secret_api: &str, node: u64, own: &OwnMembership, configured: &[ClusterNode]) -> Standing {
    let api = match Api::new(secret_api.to_string()) {
        Ok(api) => api,
        Err(e) => {
            tracing::debug!("cannot ask whether node {node} is still a member: {e}");
            return Standing::Unknown;
        }
    };
    let asked: BTreeSet<&str> = configured
        .iter()
        .filter(|other| other.id != node)
        .map(|other| other.api.as_str())
        .chain(own.nodes.iter().filter(|(id, _)| **id != node).map(|(_, api)| api.as_str()))
        .collect();
    let mut answers = ask_all(&api, asked.iter().copied()).await;
    let named: BTreeSet<String> = answers
        .iter()
        .filter_map(|answer| {
            let leader = answer.leader.filter(|leader| *leader != node && !answers.iter().any(|other| other.id == *leader))?;
            answer.members.nodes.get(&leader).map(|member| member.api.clone())
        })
        .filter(|address| !asked.contains(address.as_str()))
        .collect();
    answers.extend(ask_all(&api, named.iter().map(String::as_str)).await);
    standing(node, own, &answers)
}

/// The answers of the nodes at `addresses` that gave a readable one within `PING_WAIT`.
async fn ask_all<'a>(api: &Api, addresses: impl Iterator<Item = &'a str>) -> Vec<Metrics> {
    // Once: a node without a leader asks every few seconds.
    static REFUSED: std::sync::Once = std::sync::Once::new();
    let answers = futures::future::join_all(addresses.map(|address| api.metrics_within(address, PING_WAIT))).await;
    answers
        .into_iter()
        .filter_map(|answer| match answer {
            Ok(metrics) => Some(metrics),
            Err(Failure::Secret(e)) => {
                REFUSED.call_once(|| tracing::warn!("cannot ask whether this node is still a member: {e}"));
                None
            }
            Err(Failure::Unusable(e) | Failure::Silent(e)) => {
                tracing::debug!("{e}");
                None
            }
        })
        .collect()
}

/// What the command prints, and its exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Outcome {
    status: i32,
    stdout: String,
    stderr: String,
}

impl Outcome {
    fn done(stdout: String) -> Self {
        Self { status: 0, stdout, stderr: String::new() }
    }

    fn failed(status: i32, stderr: String) -> Self {
        Self { status, stdout: String::new(), stderr }
    }
}

/// Why a request got no usable answer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Failure {
    /// The node refused the api secret: a mistake in the configuration, which waiting does not mend.
    Secret(String),
    /// An answer the command cannot use from this node; another node may do.
    Unusable(String),
    /// No answer at all: nothing connected, or nothing came back in time.
    Silent(String),
}

/// How far a removal got, for what to say when time runs out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Progress {
    /// Nothing sent yet.
    Checking = 0,
    /// The request is on its way; the leader may have made the change.
    Sending = 1,
    /// The leader accepted it; the members have yet to show it.
    Sent = 2,
}

impl Progress {
    fn load(from: &AtomicU8) -> Self {
        match from.load(Ordering::SeqCst) {
            0 => Self::Checking,
            1 => Self::Sending,
            _ => Self::Sent,
        }
    }
}

/// Runs `cluster` with its arguments, the word `cluster` not included. Prints the result and
/// returns the exit status.
pub async fn run(args: &[String]) -> i32 {
    crate::logging::init_for_command();
    let outcome = match prepare(args) {
        Ok((parsed, cluster, api)) => {
            let progress = AtomicU8::new(Progress::Checking as u8);
            match tokio::time::timeout(CLUSTER_WAIT, act(&parsed.action, &cluster.nodes, &api, &progress)).await {
                Ok(outcome) => outcome,
                Err(_) => timed_out(&parsed.action, Progress::load(&progress)),
            }
        }
        Err(e) => Outcome::failed(STATUS_MISTAKE, e),
    };
    if !outcome.stdout.is_empty() {
        println!("{}", outcome.stdout);
    }
    if !outcome.stderr.is_empty() {
        eprintln!("{}", outcome.stderr);
    }
    outcome.status
}

/// Reads the arguments and the configuration. Nothing is sent yet.
fn prepare(args: &[String]) -> Result<(Parsed, config::ClusterSettings, Api), String> {
    let parsed = parse(args)?;
    let config_path = parsed.config_path.as_deref().unwrap_or(config::default_config_path());
    let config = config::load(Path::new(config_path)).map_err(|e| e.to_string())?;
    let api = Api::new(config.cluster.secret_api.clone())?;
    Ok((parsed, config.cluster, api))
}

fn parse(args: &[String]) -> Result<Parsed, String> {
    let usage = || crate::cli::named(USAGE);
    let mut args = args.iter();
    let remove = match args.next().map(String::as_str) {
        Some("members") => false,
        Some("remove") => true,
        _ => return Err(usage()),
    };
    let (mut config_path, mut force, mut node) = (None, false, None);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" if config_path.is_none() => config_path = Some(args.next().ok_or_else(|| format!("--config needs a value\n{}", usage()))?.clone()),
            "--force" if remove && !force => force = true,
            _ if remove && node.is_none() && !arg.starts_with("--") => node = Some(arg.as_str()),
            _ => return Err(format!("unexpected argument '{arg}'\n{}", usage())),
        }
    }
    let action = match (remove, node) {
        (false, _) => Action::Members,
        (true, None) => return Err(format!("a node id is required\n{}", usage())),
        (true, Some(text)) => match text.parse::<u64>() {
            Ok(node) if node > 0 => Action::Remove { node, force },
            _ => return Err(format!("the node id must be a positive integer, not '{text}'")),
        },
    };
    Ok(Parsed { action, config_path })
}

/// What to say when the time limit ends the command, by how far a removal got: once the request
/// may have reached the leader, saying only that the cluster did not answer would send the
/// operator to run it again, and find the node gone.
fn timed_out(action: &Action, progress: Progress) -> Outcome {
    let limit = CLUSTER_WAIT.as_secs();
    let message = match (action, progress) {
        (Action::Remove { node, .. }, Progress::Sending) => format!("the removal of node {node} may have been made, but the cluster did not answer within {limit} s; check with cluster members"),
        (Action::Remove { node, .. }, Progress::Sent) => format!("the leader accepted the removal of node {node}, but its members did not show it within {limit} s; check with cluster members"),
        _ => format!("the cluster did not answer within {limit} s"),
    };
    Outcome::failed(STATUS_UNREACHABLE, message)
}

async fn act(action: &Action, configured: &[ClusterNode], api: &Api, progress: &AtomicU8) -> Outcome {
    match action {
        Action::Members => match find_leader(api, configured).await {
            Ok((leader, metrics)) => Outcome::done(member_lines(&metrics.members, leader.id, configured)),
            Err(e) => Outcome::failed(STATUS_MISTAKE, e),
        },
        Action::Remove { node, force } => remove(api, configured, *node, *force, progress).await,
    }
}

/// The leader and its own view of the cluster. Asks the configured nodes in order, round after
/// round, until one sees a leader and the leader confirms it. A node that gave no answer at all
/// is asked last in the next round, so that a stopped host, whose packets may be dropped, does
/// not cost its full wait every round. Ends only on a refused secret, returned as the message;
/// the caller's time limit ends the rest.
async fn find_leader(api: &Api, configured: &[ClusterNode]) -> Result<(LeaderAddress, Metrics), String> {
    let mut order: Vec<&ClusterNode> = configured.iter().collect();
    loop {
        let mut silent = BTreeSet::new();
        for node in &order {
            let seen = match api.metrics(&node.api).await {
                Ok(metrics) => metrics,
                Err(Failure::Secret(e)) => return Err(e),
                Err(Failure::Unusable(e)) => {
                    tracing::debug!("{e}");
                    continue;
                }
                Err(Failure::Silent(e)) => {
                    tracing::debug!("{e}");
                    silent.insert(node.id);
                    continue;
                }
            };
            let Some(address) = seen.leader.and_then(|id| seen.members.nodes.get(&id).map(|member| LeaderAddress { id, api: member.api.clone() })) else {
                tracing::debug!("node {} sees no leader", node.id);
                continue;
            };
            match api.metrics(&address.api).await {
                Ok(metrics) if metrics.id == address.id && metrics.leader == Some(address.id) => return Ok((address, metrics)),
                Ok(_) => tracing::debug!("node {} is no longer the leader", address.id),
                Err(Failure::Secret(e)) => return Err(e),
                Err(Failure::Unusable(e) | Failure::Silent(e)) => tracing::debug!("{e}"),
            }
        }
        quiet_last(&mut order, &silent);
        tokio::time::sleep(ROUND_PAUSE).await;
    }
}

/// The members as the first node of `configured`, in order, that answers within `PING_WAIT` has
/// them, and the leader it sees; without waiting for a leader, which is what `status` needs when
/// there is none. When none answers, the configured nodes, all as voters, and no leader. Fails only
/// on a refused secret.
pub(crate) async fn current_members(api: &Api, configured: &[ClusterNode]) -> Result<(Members, Option<u64>, Source), String> {
    for node in configured {
        match api.metrics_within(&node.api, PING_WAIT).await {
            Ok(metrics) => return Ok((metrics.members, metrics.leader, Source::Cluster)),
            Err(Failure::Secret(e)) => return Err(e),
            Err(Failure::Unusable(e) | Failure::Silent(e)) => tracing::debug!("{e}"),
        }
    }
    Ok((configured_members(configured), None, Source::Configuration))
}

/// The message of the first node of `nodes` that refuses `secret_api`, when one does. Asks every
/// node at once, each for up to `PING_WAIT`, and stops at the first that settles it: a 401 is a
/// refusal, an answer that the secret was taken says it is not one, so a node that is down or
/// drops packets delays nothing while another answers. A node that does not answer, or answers
/// anything else, settles nothing: the command goes on, and ends its own way, as it did before.
pub(crate) async fn refused_secret(secret_api: &str, nodes: &[ClusterNode]) -> Option<String> {
    use futures::StreamExt;
    let api = Api::new(secret_api.to_string()).ok()?;
    let mut answers: futures::stream::FuturesUnordered<_> = nodes.iter().map(|node| api.metrics_within(&node.api, PING_WAIT)).collect();
    while let Some(answer) = answers.next().await {
        match answer {
            Err(Failure::Secret(message)) => return Some(message),
            Ok(_) => return None,
            Err(Failure::Unusable(_) | Failure::Silent(_)) => {}
        }
    }
    None
}

/// Refuses to start a node whose `node_id` another, running node of the cluster already has.
/// Joining under a member's id rewrites that member's addresses to this node's, so the cluster
/// would take the running node for another host. Asks the other configured nodes, each for up
/// to `PING_WAIT`; the first that answers speaks for the cluster. Passes when none answers (a
/// first start, or a cluster that is down), when the cluster has the id at this node's own
/// address (a restart), and when the address it has does not answer (a replaced host). A refused
/// secret passes too: the node's own start reports that.
pub(crate) async fn check_node_id_free(own: &ClusterNode, configured: &[ClusterNode], secret_api: &str) -> Result<(), String> {
    let api = Api::new(secret_api.to_string())?;
    for node in configured.iter().filter(|node| node.id != own.id) {
        match api.metrics_within(&node.api, PING_WAIT).await {
            Ok(metrics) => {
                let Some(taken) = taken_by_another(own, &metrics.members) else {
                    return Ok(());
                };
                if !api.answers(&taken.api).await {
                    return Ok(());
                }
                return Err(format!(
                    "node id {} is in use: the cluster has it at api {} (raft {}), and that node answers, but this node's configuration gives it api {} (raft {}); \
                     use an id no node has (see `rradiusd cluster members`), or stop the running node first",
                    own.id, taken.api, taken.raft, own.api, own.raft
                ));
            }
            Err(Failure::Secret(_)) => return Ok(()),
            Err(Failure::Unusable(e) | Failure::Silent(e)) => tracing::debug!("{e}"),
        }
    }
    Ok(())
}

/// The member the cluster has under this node's id, when its api address is not this node's.
fn taken_by_another<'a>(own: &ClusterNode, members: &'a Members) -> Option<&'a Member> {
    members.nodes.get(&own.id).filter(|member| member.api != own.api)
}

/// The configured nodes as members, all of them voters.
fn configured_members(configured: &[ClusterNode]) -> Members {
    let nodes = configured.iter().map(|node| (node.id, Member { raft: node.raft.clone(), api: node.api.clone() })).collect();
    Members { voters: configured.iter().map(|node| node.id).collect(), nodes }
}

/// Moves the nodes in `silent` to the end, keeping the order within both groups.
fn quiet_last(order: &mut [&ClusterNode], silent: &BTreeSet<u64>) {
    order.sort_by_key(|node| silent.contains(&node.id));
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LeaderAddress {
    id: u64,
    api: String,
}

async fn remove(api: &Api, configured: &[ClusterNode], node: u64, force: bool, progress: &AtomicU8) -> Outcome {
    let (leader, metrics) = match find_leader(api, configured).await {
        Ok(found) => found,
        Err(e) => return Outcome::failed(STATUS_MISTAKE, e),
    };
    let Some(member) = metrics.members.nodes.get(&node) else {
        return Outcome { status: STATUS_NO_MATCH, stdout: format!("no node {node} in the cluster"), stderr: String::new() };
    };
    if api.answers(&member.api).await {
        return Outcome::failed(STATUS_MISTAKE, format!("node {node} still answers at {}: stop it first, then remove it", member.api));
    }
    if let Err(e) = removal_allowed(&metrics.members, node, force) {
        return Outcome::failed(STATUS_MISTAKE, e);
    }
    let mut target = leader.api;
    let mut redirected = false;
    progress.store(Progress::Sending as u8, Ordering::SeqCst);
    loop {
        match api.leave(&target, node).await {
            Ok(()) => break,
            Err(Answer::Leader(address)) if !redirected => {
                redirected = true;
                target = address;
            }
            Err(answer) => return failed_removal(node, answer),
        }
    }
    progress.store(Progress::Sent as u8, Ordering::SeqCst);
    // Done once the leader's members no longer include the node. Ask the node that took the
    // request first; only when it no longer leads, look for the leader again.
    loop {
        let members = match api.metrics(&target).await {
            Ok(metrics) if metrics.leader == Some(metrics.id) => metrics.members,
            Err(Failure::Secret(e)) => return Outcome::failed(STATUS_MISTAKE, e),
            _ => match find_leader(api, configured).await {
                Ok((leader, metrics)) => {
                    target = leader.api;
                    metrics.members
                }
                Err(e) => return Outcome::failed(STATUS_MISTAKE, e),
            },
        };
        if !members.nodes.contains_key(&node) {
            return Outcome::done(format!("removed node {node}"));
        }
        tokio::time::sleep(ROUND_PAUSE).await;
    }
}

/// What a removal request that did not succeed ends with. Only a refusal is status 1: a request
/// with no answer may still have been made, which is what status 3 says.
fn failed_removal(node: u64, answer: Answer) -> Outcome {
    match answer {
        Answer::Secret(e) => Outcome::failed(STATUS_MISTAKE, e),
        Answer::Leader(address) => Outcome::failed(STATUS_NO_MATCH, format!("the leader moved twice; last to {address}; run the command again")),
        Answer::Refused(e) => Outcome::failed(STATUS_NO_MATCH, e),
        Answer::Unanswered(e) => Outcome::failed(STATUS_UNREACHABLE, format!("{e}; the removal of node {node} may have been made; check with cluster members")),
    }
}

/// Whether removing `node` leaves enough voters: none left is refused, one or two only with `force`.
fn removal_allowed(members: &Members, node: u64, force: bool) -> Result<(), String> {
    if !members.voters.contains(&node) {
        return Ok(());
    }
    match members.voters.len() - 1 {
        0 => Err(format!("node {node} is the last voter; the cluster cannot remove it")),
        1 if !force => Err(format!("removing node {node} leaves one voter, with nothing to take over when it stops; give --force to remove it anyway")),
        2 if !force => Err(format!("removing node {node} leaves two voters, and the cluster stops writing when either stops; give --force to remove it anyway")),
        _ => Ok(()),
    }
}

/// One line per member by node id, then the configured ids the cluster does not have.
fn member_lines(members: &Members, leader: u64, configured: &[ClusterNode]) -> String {
    let mut lines: Vec<String> = members
        .nodes
        .iter()
        .map(|(id, member)| {
            let role = if members.voters.contains(id) { "voter" } else { "learner" };
            let lead = if *id == leader { "leader" } else { "" };
            let unlisted = if configured.iter().any(|node| node.id == *id) { "" } else { "  (not in this configuration)" };
            format!("node {id}  {role:<7}  {lead:<6}  raft {}  api {}{unlisted}", member.raft, member.api)
        })
        .collect();
    let mut missing: Vec<u64> = configured.iter().map(|node| node.id).filter(|id| !members.nodes.contains_key(id)).collect();
    missing.sort_unstable();
    lines.extend(missing.into_iter().map(|id| format!("node {id}  not in the cluster (listed in this configuration)")));
    lines.join("\n")
}

/// Reads `GET /cluster/metrics/sqlite`.
fn parse_metrics(body: &[u8]) -> Result<Metrics, String> {
    let value: Value = serde_json::from_slice(body).map_err(|e| format!("unreadable metrics: {e}"))?;
    let id = value.get("id").and_then(Value::as_u64).ok_or("metrics without the node's id")?;
    let leader = value.get("current_leader").and_then(Value::as_u64);
    let membership = value.get("membership_config").and_then(|config| config.get("membership")).ok_or("metrics without the membership")?;
    let index = |log: Option<&Value>| log.and_then(|log| log.get("index")).and_then(Value::as_u64);
    Ok(Metrics {
        id,
        leader,
        members: parse_membership(membership)?,
        membership_index: index(value.get("membership_config").and_then(|config| config.get("log_id"))),
        applied_index: index(value.get("last_applied")),
        quorum_ack_millis: value.get("millis_since_quorum_ack").and_then(Value::as_u64),
    })
}

fn parse_membership(membership: &Value) -> Result<Members, String> {
    // During a change there are two voter sets; the last is the one the cluster moves to.
    let voters = membership
        .get("configs")
        .and_then(Value::as_array)
        .and_then(|configs| configs.last())
        .and_then(Value::as_array)
        .ok_or("membership without voters")?
        .iter()
        .map(|id| id.as_u64().ok_or("a voter id that is not a number"))
        .collect::<Result<BTreeSet<u64>, _>>()?;
    let mut nodes = BTreeMap::new();
    for node in membership.get("nodes").and_then(Value::as_object).ok_or("membership without nodes")?.values() {
        let field = |name: &str| node.get(name).and_then(Value::as_str).map(str::to_string);
        match (node.get("id").and_then(Value::as_u64), field("addr_raft"), field("addr_api")) {
            (Some(id), Some(raft), Some(api)) => nodes.insert(id, Member { raft, api }),
            _ => return Err("a member without its id or addresses".to_string()),
        };
    }
    Ok(Members { voters, nodes })
}

/// The leader's api address in a 409 answer that names it.
fn forwarded_leader(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let forward = value.get("CheckIsLeaderError")?.get("APIError")?.get("ForwardToLeader")?;
    forward.get("leader_node")?.get("addr_api")?.as_str().map(str::to_string)
}

/// What a removal request got instead of success.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Answer {
    Secret(String),
    /// Sent to a node that is not the leader; the leader's api address.
    Leader(String),
    Refused(String),
    /// No answer: the request may or may not have reached the leader.
    Unanswered(String),
}

/// Requests to the nodes' management API.
pub(crate) struct Api {
    client: reqwest::Client,
    secret: String,
}

impl Api {
    pub(crate) fn new(secret: String) -> Result<Self, String> {
        // Plain HTTP only, but the client wants TLS set up all the same; both rustls crypto
        // providers are compiled in, so name one. No proxy, no redirects: requests go only to
        // the cluster's own addresses.
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("cannot set up the client: {e}"))?
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| format!("cannot set up the client: {e}"))?;
        Ok(Self { client, secret })
    }

    async fn metrics(&self, address: &str) -> Result<Metrics, Failure> {
        self.metrics_within(address, REQUEST_WAIT).await
    }

    /// `metrics` with its own time to answer.
    async fn metrics_within(&self, address: &str, wait: Duration) -> Result<Metrics, Failure> {
        let response = self
            .client
            .get(format!("http://{address}/cluster/metrics/{RAFT}"))
            .header("X-API-SECRET", &self.secret)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(wait)
            .send()
            .await
            .map_err(|e| Failure::Silent(format!("{address}: {e}")))?;
        let status = response.status();
        let body = response.bytes().await.map_err(|e| Failure::Silent(format!("{address}: {e}")))?;
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Failure::Secret(secret_refused(address)));
        }
        if !status.is_success() {
            return Err(Failure::Unusable(format!("{address}: HTTP {status}: {}", String::from_utf8_lossy(&body))));
        }
        parse_metrics(&body).map_err(|e| Failure::Unusable(format!("{address}: {e}")))
    }

    /// Whether anything answers HTTP at `address` in time.
    async fn answers(&self, address: &str) -> bool {
        self.client.get(format!("http://{address}/ping")).timeout(PING_WAIT).send().await.is_ok()
    }

    async fn leave(&self, address: &str, node: u64) -> Result<(), Answer> {
        let body = serde_json::json!({ "node_id": node, "stay_as_learner": false }).to_string();
        let response = self
            .client
            .delete(format!("http://{address}/cluster/membership/{RAFT}"))
            .header("X-API-SECRET", &self.secret)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body)
            .timeout(CLUSTER_WAIT)
            .send()
            .await
            .map_err(|e| Answer::Unanswered(format!("{address}: {e}")))?;
        let status = response.status();
        let body = response.bytes().await.map_err(|e| Answer::Unanswered(format!("{address}: {e}")))?;
        if status.is_success() {
            return Ok(());
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Answer::Secret(secret_refused(address)));
        }
        match forwarded_leader(&body) {
            Some(leader) if status == reqwest::StatusCode::CONFLICT => Err(Answer::Leader(leader)),
            _ => Err(Answer::Refused(format!("{address}: HTTP {status}: {}", String::from_utf8_lossy(&body)))),
        }
    }
}

fn secret_refused(address: &str) -> String {
    format!("{address} refused the api secret: [cluster] secret_api differs from the cluster's")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    /// Answers each connection with `reply` (a whole HTTP response) and counts them.
    async fn serve(reply: String) -> Result<String, std::io::Error> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer).await;
                let _ = stream.write_all(reply.as_bytes()).await;
            }
        });
        Ok(address)
    }

    fn accepted() -> String {
        format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{METRICS}", METRICS.len())
    }

    fn unauthorized() -> String {
        "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string()
    }

    fn node_at(id: u64, api: String) -> ClusterNode {
        ClusterNode { id, raft: format!("127.0.0.1:{}", 9000 + id), api }
    }

    #[tokio::test]
    async fn test_refused_secret_is_named_with_the_node() -> Result<(), std::io::Error> {
        let refusing = serve(unauthorized()).await?;
        let answer = refused_secret("wrong", &[node_at(1, refusing.clone())]).await;
        assert_eq!(answer, Some(secret_refused(&refusing)));
        Ok(())
    }

    #[tokio::test]
    async fn test_refused_secret_ignores_what_it_cannot_tell() -> Result<(), std::io::Error> {
        let fine = serve(accepted()).await?;
        // A port nothing listens on: bound, then dropped.
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            listener.local_addr()?.to_string()
        };
        assert_eq!(refused_secret("right", &[node_at(1, fine), node_at(2, closed)]).await, None);
        assert_eq!(refused_secret("right", &[]).await, None);
        Ok(())
    }

    #[tokio::test]
    async fn test_a_refusal_is_found_past_a_node_that_is_down() -> Result<(), std::io::Error> {
        let refusing = serve(unauthorized()).await?;
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            listener.local_addr()?.to_string()
        };
        let answer = refused_secret("wrong", &[node_at(1, closed), node_at(2, refusing.clone())]).await;
        assert_eq!(answer, Some(secret_refused(&refusing)));
        Ok(())
    }

    /// A node that never answers does not hold up the others: the probe ends with the first
    /// answer that settles it, well before `PING_WAIT`.
    #[tokio::test]
    async fn test_a_silent_node_does_not_delay_an_answer() -> Result<(), std::io::Error> {
        let fine = serve(accepted()).await?;
        let silent = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?.to_string();
            tokio::spawn(async move {
                let mut held = Vec::new();
                while let Ok((stream, _)) = listener.accept().await {
                    held.push(stream);
                }
            });
            address
        };
        let started = std::time::Instant::now();
        assert_eq!(refused_secret("right", &[node_at(1, silent), node_at(2, fine)]).await, None);
        assert!(started.elapsed() < PING_WAIT, "{:?}", started.elapsed());
        Ok(())
    }

    fn configured(ids: &[u64]) -> Vec<ClusterNode> {
        ids.iter().map(|id| ClusterNode { id: *id, raft: format!("node{id}:8100"), api: format!("node{id}:8200") }).collect()
    }

    fn members(voters: &[u64], learners: &[u64]) -> Members {
        let nodes = voters.iter().chain(learners).map(|id| (*id, Member { raft: format!("node{id}:8100"), api: format!("node{id}:8200") })).collect();
        Members { voters: voters.iter().copied().collect(), nodes }
    }

    /// A metrics answer as a follower gave it in a three-node cluster on 2026-11-13, shortened.
    const METRICS: &str = r#"{"id":2,"state":"Follower","current_term":1,"current_leader":1,
        "membership_config":{"log_id":{"leader_id":{"term":1,"node_id":1},"index":7},
        "membership":{"configs":[[1,2,3]],"nodes":{
            "1":{"id":1,"addr_raft":"node1:8100","addr_api":"node1:8200"},
            "2":{"id":2,"addr_raft":"node2:8100","addr_api":"node2:8200"},
            "3":{"id":3,"addr_raft":"node3:8100","addr_api":"node3:8200"}}}}}"#;

    #[test]
    fn test_parse() -> Result<(), String> {
        assert_eq!(parse(&args("members"))?, Parsed { action: Action::Members, config_path: None });
        assert_eq!(parse(&args("members --config c.toml"))?, Parsed { action: Action::Members, config_path: Some("c.toml".to_string()) });
        assert_eq!(parse(&args("remove 4"))?.action, Action::Remove { node: 4, force: false });
        assert_eq!(parse(&args("remove --force 3 --config c.toml"))?, Parsed { action: Action::Remove { node: 3, force: true }, config_path: Some("c.toml".to_string()) });
        for wrong in ["", "join", "members 4", "members --force", "remove", "remove 3 4", "remove --force --force 3", "remove --config a --config b 3", "remove --config", "remove --dry-run 3"] {
            assert!(parse(&args(wrong)).is_err(), "'{wrong}' was accepted");
        }
        for id in ["abc", "0", "-1", "1.5"] {
            let error = parse(&args(&format!("remove {id}"))).err().ok_or(format!("'{id}' was accepted"))?;
            assert!(error.contains("positive integer") || error.contains("unexpected argument"), "{error}");
        }
        Ok(())
    }

    #[test]
    fn test_parse_metrics() -> Result<(), String> {
        let metrics = parse_metrics(METRICS.as_bytes())?;
        assert_eq!(metrics.id, 2);
        assert_eq!(metrics.leader, Some(1));
        assert_eq!(metrics.members, members(&[1, 2, 3], &[]));
        // A node that sees no leader, as a removed one does.
        let leaderless = METRICS.replace(r#""current_leader":1"#, r#""current_leader":null"#);
        assert_eq!(parse_metrics(leaderless.as_bytes())?.leader, None);
        // A learner, and two voter sets during a change: the last one counts.
        let changing = METRICS.replace("[[1,2,3]]", "[[1,2,3],[1,2]]");
        assert_eq!(parse_metrics(changing.as_bytes())?.members.voters, [1, 2].into_iter().collect());
        for broken in ["", "{}", r#"{"id":1}"#, &METRICS.replace(r#""addr_api":"node3:8200""#, r#""addr":"x""#)] {
            assert!(parse_metrics(broken.as_bytes()).is_err(), "{broken} was read");
        }
        Ok(())
    }

    #[test]
    fn test_forwarded_leader() {
        let body = br#"{"CheckIsLeaderError":{"APIError":{"ForwardToLeader":{"leader_id":1,"leader_node":{"id":1,"addr_raft":"node1:8100","addr_api":"node1:8200"}}}}}"#;
        assert_eq!(forwarded_leader(body), Some("node1:8200".to_string()));
        assert_eq!(forwarded_leader(br#"{"Token":"Invalid API Secret"}"#), None);
        assert_eq!(forwarded_leader(b"not json"), None);
    }

    #[test]
    fn test_member_lines() {
        // The spec's example: voters 1, 2, 4 with leader 1, the configuration listing 1, 2, 3.
        let shown = member_lines(&members(&[1, 2, 4], &[]), 1, &configured(&[1, 2, 3]));
        assert_eq!(
            shown,
            "node 1  voter    leader  raft node1:8100  api node1:8200\n\
             node 2  voter            raft node2:8100  api node2:8200\n\
             node 4  voter            raft node4:8100  api node4:8200  (not in this configuration)\n\
             node 3  not in the cluster (listed in this configuration)"
        );
        let shown = member_lines(&members(&[1, 2, 3], &[5]), 2, &configured(&[3, 2, 1, 5]));
        assert_eq!(
            shown,
            "node 1  voter            raft node1:8100  api node1:8200\n\
             node 2  voter    leader  raft node2:8100  api node2:8200\n\
             node 3  voter            raft node3:8100  api node3:8200\n\
             node 5  learner          raft node5:8100  api node5:8200"
        );
    }

    #[test]
    fn test_failed_removal() {
        let status = |answer| failed_removal(4, answer).status;
        assert_eq!(status(Answer::Refused("node1:8200: HTTP 500".to_string())), STATUS_NO_MATCH);
        assert_eq!(status(Answer::Leader("node2:8200".to_string())), STATUS_NO_MATCH);
        assert_eq!(status(Answer::Secret("refused".to_string())), STATUS_MISTAKE);
        // No answer is not a refusal: the change may have been made.
        let unanswered = failed_removal(4, Answer::Unanswered("node1:8200: connection refused".to_string()));
        assert_eq!(unanswered.status, STATUS_UNREACHABLE);
        assert!(unanswered.stderr.contains("may have been made") && unanswered.stderr.contains("cluster members"), "{}", unanswered.stderr);
    }

    #[test]
    fn test_timed_out() {
        let remove = Action::Remove { node: 4, force: false };
        for (action, progress, says) in [
            (&Action::Members, Progress::Checking, "the cluster did not answer within 30 s"),
            (&remove, Progress::Checking, "the cluster did not answer within 30 s"),
            (&remove, Progress::Sending, "the removal of node 4 may have been made"),
            (&remove, Progress::Sent, "the leader accepted the removal of node 4"),
        ] {
            let outcome = timed_out(action, progress);
            assert_eq!(outcome.status, STATUS_UNREACHABLE);
            assert!(outcome.stderr.contains(says), "{progress:?}: {}", outcome.stderr);
        }
        assert_eq!(Progress::load(&AtomicU8::new(Progress::Sent as u8)), Progress::Sent);
    }

    /// With no node answering, the members are those of the configuration.
    #[tokio::test]
    async fn test_current_members_from_the_configuration() -> Result<(), String> {
        let mut nodes = Vec::new();
        for id in [1, 2] {
            // A port that no test of this or another process is handed, so nothing answers on it.
            let address = format!("127.0.0.1:{}", crate::test_support::server::free_port());
            nodes.push(ClusterNode { id, raft: format!("node{id}:8100"), api: address });
        }
        let api = Api::new("secret".to_string())?;
        let (members, leader, source) = current_members(&api, &nodes).await?;
        assert_eq!(source, Source::Configuration);
        assert_eq!(leader, None);
        assert_eq!(members.voters, [1, 2].into_iter().collect());
        assert_eq!(members.nodes.get(&2).map(|member| member.api.clone()), Some(nodes[1].api.clone()));
        Ok(())
    }

    #[test]
    fn test_quiet_last() {
        let nodes = configured(&[1, 2, 3, 4]);
        let mut order: Vec<&ClusterNode> = nodes.iter().collect();
        quiet_last(&mut order, &[1, 3].into_iter().collect());
        assert_eq!(order.iter().map(|node| node.id).collect::<Vec<_>>(), [2, 4, 1, 3]);
        quiet_last(&mut order, &BTreeSet::new());
        assert_eq!(order.iter().map(|node| node.id).collect::<Vec<_>>(), [2, 4, 1, 3]);
    }

    #[test]
    fn test_removal_allowed() {
        let four = members(&[1, 2, 3, 4], &[]);
        assert!(removal_allowed(&four, 4, false).is_ok());
        let three = members(&[1, 2, 3], &[5]);
        assert!(removal_allowed(&three, 3, false).is_err_and(|e| e.contains("two voters") && e.contains("--force")));
        assert!(removal_allowed(&three, 3, true).is_ok());
        // A learner leaves the voters as they are.
        assert!(removal_allowed(&three, 5, false).is_ok());
        let two = members(&[1, 2], &[]);
        assert!(removal_allowed(&two, 2, false).is_err_and(|e| e.contains("one voter")));
        assert!(removal_allowed(&two, 2, true).is_ok());
        let one = members(&[1], &[]);
        assert!(removal_allowed(&one, 1, true).is_err_and(|e| e.contains("last voter")));
    }

    #[test]
    fn test_taken_by_another() {
        let own = |api: &str| ClusterNode { id: 2, raft: "x:8100".to_string(), api: api.to_string() };
        let cluster = members(&[1, 2, 3], &[]);
        assert_eq!(taken_by_another(&own("node2:8200"), &cluster), None);
        assert_eq!(taken_by_another(&own("other:8200"), &cluster).map(|m| m.api.as_str()), Some("node2:8200"));
        // An id the cluster does not have is free.
        let new = ClusterNode { id: 4, raft: "x:8100".to_string(), api: "other:8200".to_string() };
        assert_eq!(taken_by_another(&new, &cluster), None);
    }

    /// Serves `/cluster/metrics/sqlite` with `body`, and `/ping`, on a local port.
    async fn serve_metrics(body: String) -> Result<String, String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
        let address = listener.local_addr().map_err(|e| e.to_string())?.to_string();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 2048];
                    let Ok(read) = stream.read(&mut buffer).await else { return };
                    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let answer = if request.starts_with("GET /ping") { "ok".to_string() } else { body };
                    let reply = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}", answer.len(), answer);
                    let _ = stream.write_all(reply.as_bytes()).await;
                });
            }
        });
        Ok(address)
    }

    /// Metrics of a cluster whose node 2 is at `api`, answered by the node at `api`.
    fn metrics_with_node_2_at(api: &str) -> String {
        METRICS.replace("node2:8200", api)
    }

    #[tokio::test]
    async fn test_check_node_id_free() -> Result<(), String> {
        // A running node 2, and node 1 as the cluster's voice.
        let running = serve_metrics(String::new()).await?;
        let voice = serve_metrics(metrics_with_node_2_at(&running)).await?;
        let nodes = vec![
            ClusterNode { id: 1, raft: "node1:8100".to_string(), api: voice.clone() },
            ClusterNode { id: 2, raft: "new:8100".to_string(), api: "127.0.0.1:1".to_string() },
        ];
        // A new host with node 2's id while node 2 answers: refused.
        let error = check_node_id_free(&nodes[1], &nodes, "secret").await.err().ok_or("accepted")?;
        assert!(error.contains("node id 2 is in use") && error.contains(&running), "{error}");
        // The running node itself, restarting: its own address is the one the cluster has.
        let restarting = ClusterNode { id: 2, raft: "node2:8100".to_string(), api: running.clone() };
        check_node_id_free(&restarting, &nodes, "secret").await?;
        // The same id on a host that replaces a stopped one: nothing answers at the old address.
        let gone = format!("127.0.0.1:{}", crate::test_support::server::free_port());
        let voice_of_gone = serve_metrics(metrics_with_node_2_at(&gone)).await?;
        let nodes = vec![ClusterNode { id: 1, raft: "node1:8100".to_string(), api: voice_of_gone }, nodes[1].clone()];
        check_node_id_free(&nodes[1], &nodes, "secret").await?;
        // No node answers: a first start.
        let silent = vec![ClusterNode { id: 1, raft: "node1:8100".to_string(), api: gone }, nodes[1].clone()];
        check_node_id_free(&silent[1], &silent, "secret").await?;
        Ok(())
    }

    /// A leader's answer: members `ids` as of log index `index`, applied up to `applied`, last
    /// acknowledged by a quorum `ack` ms ago.
    fn answer(id: u64, leader: u64, ids: &[u64], index: u64, applied: u64, ack: Option<u64>) -> Metrics {
        Metrics { id, leader: Some(leader), members: members(ids, &[]), membership_index: Some(index), applied_index: Some(applied), quorum_ack_millis: ack }
    }

    /// Node 3's own membership: nodes `ids` as of log index `index`.
    fn own(ids: &[u64], index: u64) -> OwnMembership {
        OwnMembership { index, nodes: members(ids, &[]).nodes.into_iter().map(|(id, member)| (id, member.api)).collect() }
    }

    fn removed_by(answer: &Metrics) -> Standing {
        Standing::Removed(Removal { node: 3, leader: answer.id, members: answer.members.nodes.clone() })
    }

    /// Scenario: Which answers count; Example: decisions.
    #[test]
    fn test_standing() {
        // The spec's table: node 3, whose own membership lists nodes 1 to 3 at index 7.
        let formed = own(&[1, 2, 3], 7);
        let omitting = answer(1, 1, &[1, 2], 16, 16, Some(100));
        assert_eq!(standing(3, &formed, std::slice::from_ref(&omitting)), removed_by(&omitting));
        for (answers, expected) in [
            // A cluster another node made from empty data.
            (vec![answer(1, 1, &[1], 0, 3, Some(100))], Standing::Unknown),
            (vec![answer(1, 1, &[1, 2, 3], 16, 16, Some(100))], Standing::Member),
            // No recent quorum acknowledgement.
            (vec![answer(1, 1, &[1, 2], 16, 16, Some(LEADER_QUORUM_ACK_WITHIN_MS + 1))], Standing::Unknown),
            (vec![answer(1, 1, &[1, 2], 16, 16, None)], Standing::Unknown),
            // The membership is not applied yet.
            (vec![answer(1, 1, &[1, 2], 16, 15, Some(100))], Standing::Unknown),
            // A follower's word for its leader.
            (vec![answer(2, 1, &[1, 2], 16, 16, None)], Standing::Unknown),
            // Not the leader, whatever else it says.
            (vec![answer(2, 1, &[1, 2], 16, 16, Some(100))], Standing::Unknown),
            // One leader omits it and another lists it.
            (vec![omitting.clone(), answer(2, 2, &[2, 3], 9, 9, Some(100))], Standing::Member),
            (vec![answer(2, 2, &[2, 3], 9, 9, Some(100)), omitting.clone()], Standing::Member),
            (vec![], Standing::Unknown),
            // The node's own answer, which a node alone gives about itself.
            (vec![answer(3, 3, &[1, 2], 16, 16, Some(0))], Standing::Unknown),
            // The same index and the node's own membership lists it: not the same log.
            (vec![answer(1, 1, &[1, 2], 7, 16, Some(100))], Standing::Unknown),
        ] {
            assert_eq!(standing(3, &formed, &answers), expected, "{answers:?}");
        }
        // Fields a node did not give confirm nothing.
        for missing in [Metrics { membership_index: None, ..omitting.clone() }, Metrics { applied_index: None, ..omitting.clone() }] {
            assert_eq!(standing(3, &formed, &[missing]), Standing::Unknown);
        }
        // A learner is a member.
        let learner = Metrics { members: members(&[1, 2], &[3]), ..omitting.clone() };
        assert_eq!(standing(3, &formed, &[learner]), Standing::Member);
        // Its own membership omits it already, at the leader's index.
        let told = answer(1, 1, &[1, 2], 16, 16, Some(100));
        assert_eq!(standing(3, &own(&[1, 2], 16), std::slice::from_ref(&told)), removed_by(&told));
    }

    #[test]
    fn test_removal_message() {
        let removal = Removal { node: 3, leader: 1, members: members(&[1, 2], &[]).nodes };
        let message = removal.message(Path::new("/var/lib/rradius/cluster"));
        for part in [
            "node 3 has been removed from the cluster",
            "node 1, its leader",
            "node 1 at node1:8200, node 2 at node2:8200",
            "stop and disable the service",
            "confirm that this is the cluster you expect, then empty data_dir '/var/lib/rradius/cluster'",
        ] {
            assert!(message.contains(part), "{message}");
        }
    }

    /// Metrics as the management API gives them: node `id` saying `leader` leads nodes at
    /// `addresses`, as of index 16.
    fn metrics_json(id: u64, leader: u64, addresses: &[(u64, &str)]) -> String {
        let nodes: serde_json::Map<String, Value> = addresses.iter().map(|(id, api)| (id.to_string(), serde_json::json!({"id": id, "addr_raft": format!("node{id}:8100"), "addr_api": api}))).collect();
        let voters: Vec<u64> = addresses.iter().map(|(id, _)| *id).collect();
        let log = serde_json::json!({"leader_id": {"term": 1, "node_id": leader}, "index": 16});
        serde_json::json!({
            "id": id, "current_leader": leader, "last_applied": log,
            "millis_since_quorum_ack": (id == leader).then_some(100),
            "membership_config": {"log_id": log, "membership": {"configs": [voters], "nodes": nodes}},
        })
        .to_string()
    }

    /// The node itself is not asked, and a leader that neither list has is followed to.
    #[tokio::test]
    async fn test_standing_in_cluster_follows_the_leader_and_skips_itself() -> Result<(), String> {
        let asked = Arc::new(AtomicU8::new(0));
        let itself = {
            use tokio::io::AsyncWriteExt;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
            let address = listener.local_addr().map_err(|e| e.to_string())?.to_string();
            let asked = asked.clone();
            tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    asked.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.shutdown().await;
                }
            });
            address
        };
        // The leader's address is known only from what node 1 says; bound before its answer, which
        // names it, is made.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
        let leader = listener.local_addr().map_err(|e| e.to_string())?.to_string();
        let leading = metrics_json(4, 4, &[(1, "node1:8200"), (4, &leader)]);
        {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let mut buffer = [0u8; 2048];
                    let _ = stream.read(&mut buffer).await;
                    let reply = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{leading}", leading.len());
                    let _ = stream.write_all(reply.as_bytes()).await;
                }
            });
        }
        let voice = serve_metrics(metrics_json(1, 4, &[(1, "node1:8200"), (4, &leader)])).await?;
        let configured = vec![node_at(1, voice), node_at(3, itself.clone())];
        let own = OwnMembership { index: 7, nodes: [(3, itself)].into_iter().collect() };
        let standing = standing_in_cluster("secret", 3, &own, &configured).await;
        assert!(matches!(&standing, Standing::Removed(removal) if removal.leader == 4), "{standing:?}");
        assert_eq!(asked.load(Ordering::SeqCst), 0, "the node asked itself");
        Ok(())
    }

    /// A refused secret and an unreadable answer are no answer.
    #[tokio::test]
    async fn test_standing_in_cluster_with_unusable_answers() -> Result<(), String> {
        let refusing = serve(unauthorized()).await.map_err(|e| e.to_string())?;
        let garbled = serve("HTTP/1.1 200 OK\r\ncontent-length: 8\r\nconnection: close\r\n\r\nnot json".to_string()).await.map_err(|e| e.to_string())?;
        let configured = vec![node_at(1, refusing), node_at(2, garbled)];
        let own = OwnMembership { index: 7, nodes: BTreeMap::from([(3, "node3:8200".to_string())]) };
        assert_eq!(standing_in_cluster("secret", 3, &own, &configured).await, Standing::Unknown);
        Ok(())
    }

    /// A node that never answers costs its `PING_WAIT`, not more, and the leader still counts.
    #[tokio::test]
    async fn test_standing_in_cluster_with_a_silent_node() -> Result<(), String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
        let silent = listener.local_addr().map_err(|e| e.to_string())?.to_string();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        let leader = serve_metrics(metrics_json(1, 1, &[(1, "node1:8200"), (2, &silent)])).await?;
        let configured = vec![node_at(1, leader), node_at(2, silent)];
        let own = OwnMembership { index: 7, nodes: BTreeMap::from([(3, "node3:8200".to_string())]) };
        let started = std::time::Instant::now();
        let standing = standing_in_cluster("secret", 3, &own, &configured).await;
        assert!(matches!(standing, Standing::Removed(_)), "{standing:?}");
        assert!(started.elapsed() < 2 * PING_WAIT + Duration::from_secs(1), "{:?}", started.elapsed());
        Ok(())
    }
}
