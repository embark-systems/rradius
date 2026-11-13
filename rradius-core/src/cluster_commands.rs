//! `rradius-server cluster ...`: lists the cluster's members and removes a node, through the
//! management API every node serves on its api port. A node joins on its own (a configuration
//! listing it, an empty data directory), so there is nothing to add.
//!
//! The leader is found by asking the nodes of `[[cluster.nodes]]` in order, trusting only a node
//! that sees a leader: a node removed from the cluster but still running sees none, and answers
//! from stale data.

use crate::accounts::{CLUSTER_WAIT, STATUS_MISTAKE, STATUS_NO_MATCH, STATUS_UNREACHABLE};
use crate::config::{self, ClusterNode};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub const USAGE: &str = "usage: rradius-server cluster members|remove [--config <path>] [--force] [<node_id>]\n  remove takes a node id, and --force to leave one or two voters";

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
struct Member {
    raft: String,
    api: String,
}

/// The cluster's members: voters and every node with its addresses.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Members {
    voters: BTreeSet<u64>,
    nodes: BTreeMap<u64, Member>,
}

/// What a node said about the cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Metrics {
    id: u64,
    leader: Option<u64>,
    members: Members,
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
    Ok(Metrics { id, leader, members: parse_membership(membership)? })
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
struct Api {
    client: reqwest::Client,
    secret: String,
}

impl Api {
    fn new(secret: String) -> Result<Self, String> {
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
        let response = self
            .client
            .get(format!("http://{address}/cluster/metrics/{RAFT}"))
            .header("X-API-SECRET", &self.secret)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(REQUEST_WAIT)
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
}
