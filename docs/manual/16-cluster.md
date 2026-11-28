# 16 Clusters and high availability

> After this chapter you can:
> - Set up a three-node cluster and confirm that data written to one node can be read from the others
> - Understand leader, followers, replication and snapshots, and know what the server does when it loses the leader
> - Handle a node failure: restart a node, or rebuild its data from the other nodes
> - Add a new node to the cluster and remove a node safely
> - Deploy a three-node cluster with Docker Compose or Kubernetes

Prerequisites: you have worked through [chapter 03](03-quick-start.md) and know how to start the server and test logins with `rradius-bench`; you have read [chapter 04](04-configuration.md) and know `check` and reload. The exercises in this chapter run three to four nodes on one machine. You need `rradiusd` and `rradius-bench` built from the source, and `rradius-server/examples/scenarios/ha-cluster/` from the source.

In this chapter:

- [Concepts](#concepts)
- [cluster settings](#cluster-settings)
- [Three nodes on one machine](#three-nodes-on-one-machine)
- [Check that the cluster has formed](#check-that-the-cluster-has-formed)
- [Check that writes are replicated](#check-that-writes-are-replicated)
- [Stop the leader](#stop-the-leader)
- [Lose the majority](#lose-the-majority)
- [Restarting nodes and rebuilding data](#restarting-nodes-and-rebuilding-data)
- [Add a node](#add-a-node)
- [Remove a node](#remove-a-node)
- [Check the settings of each node](#check-the-settings-of-each-node)
- [See the state of the whole cluster](#see-the-state-of-the-whole-cluster)
- [Several hosts: the ha-cluster scenario](#several-hosts-the-ha-cluster-scenario)
- [Deploy with Docker Compose](#deploy-with-docker-compose)
- [Deploy on Kubernetes](#deploy-on-kubernetes)
- [Across data centers](#across-data-centers)
- [Cluster metrics](#cluster-metrics)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

## Concepts

### A single node is also a cluster

rradius keeps its accounts, reply attributes and accounting data in an SQLite database embedded in each node (hiqlite). The nodes replicate this database with the Raft protocol, and every node holds a complete copy.

So the configuration always needs `[cluster]`, even on a single machine. When you run only one machine, it is a cluster with one node:

- Without `[[cluster.nodes]]`, the cluster has only node 1, the Raft port is `127.0.0.1:8100` and the API port is `127.0.0.1:8200`. Both ports listen on `127.0.0.1` only by default.
- To grow to three nodes later, the tables and scripts stay as they are. You only change `[cluster]`.

### Leader, followers and writes

At any moment the cluster has at most one leader. The other nodes are followers.

- **Read:** the script's `sql_query` and the `sql` handler's password lookup read the local copy. They do not use the network.
- **Write:** the script's `sql_execute` and commands such as `user add` go to the leader. A write succeeds after the leader has the confirmation of a majority of the nodes. A write that a follower receives is forwarded to the leader.

Because a write needs a majority, the cluster can write only while a majority of the nodes is alive:

| Nodes | Majority | How many can be down at the same time |
| --- | --- | --- |
| 1 | 1 | 0 |
| 2 | 2 | 0 |
| 3 | 2 | 1 |
| 4 | 3 | 1 |
| 5 | 3 | 2 |

A cluster of two nodes is more fragile than one node: when either stops, writes stop. So when only one or two voters would remain, `cluster remove` removes a node only with `--force` (see [Remove a node](#remove-a-node)).

When the leader stops, the remaining nodes elect a new leader by themselves. Nobody has to step in.

### Stopping one node and stopping two

What happens to authentication and accounting when you stop nodes of a three-node cluster (for the details of each kind of request when there is no leader, see "When there is no leader" below):

| Situation | Authentication | Accounting |
| --- | --- | --- |
| All three up | Normal | Normal |
| One stopped | Normal; the NAS asks another node. When the stopped node is the leader, logins that need a write get no answer during the election (for the time, see [Across data centers](#across-data-centers)) | Normal. When the stopped node is the leader, writes time out during the election; with an accounting spool they go to the spool first and are replayed later, possibly once more than needed |
| Two stopped, one left | Logins that only read data (`sql`, `ldap`) read the local copy and are answered as usual; logins that write (verification codes, IP allocation) get no answer, so the NAS retransmits or asks another node. When `stop_without_leader_ms` is set, even read-only logins get no answer after that time | Cannot write to the database; with an accounting spool the request is written locally and answered, and replayed when the cluster recovers |

Accounting can be replayed, and can also be recorded twice because of a retransmit after a timeout, so the accounting script has to be safe to run more than once (for example keyed on `Acct-Session-Id` with `INSERT OR IGNORE`); see [chapter 13](13-accounting.md).

**When the database cannot be used, the server does not answer instead of rejecting.** When a login cannot be decided because the cluster database timed out or found no leader, RADIUS sends no reply and TACACS+ answers ERROR. The NAS or device then asks another node, and the user does not see "wrong password". Such requests are counted in `rradius_radius_discarded_total{reason="database"}`, and the log says `not answered: the cluster database could not be used`. The server rejects only when it is sure the account does not exist or the password is wrong, and all database calls of the script succeeded.

**A node cut off from the others reads old data.** While it is separated from the majority, an account disabled on the other side, a NAS deleted there, or a key revoked there is still valid on this node until the network recovers. If you cannot accept that, set `stop_without_leader_ms`.

### A node serves only after it has caught up

When a node starts, it does not answer requests at once. It does these things in order:

1. Start the local cluster node. A node that starts for the first time has to join the cluster first, and a node 1 without cluster data may have to wait for the other nodes (see [When node 1 waits](#when-node-1-waits)). Then ask the other nodes for the current member list (when this gives no answer either way, ask again after step 2); a node that has been removed ends here and does none of the later steps (see [When a removed node starts again](#when-a-removed-node-starts-again)).
2. Wait for a leader that can accept writes and has answered a read, for at most `migrations_wait_ms` (60000 ms by default). What the node remembers from before it stopped is not enough. When no leader answers in time, the node does not go on to steps 3 and 4: it exits, or serves from its local copy, as [Restart the whole cluster](#restart-the-whole-cluster) describes.
3. If there is a `migrations_dir`, apply the migrations that are not yet applied ([chapter 18](18-operations.md)).
4. Write a marker of its own through the leader, and wait until the local copy reads this marker back. Only then has it caught up. A write the cluster cannot take at that moment, as during an election, is made again until `migrations_wait_ms`. The log says `cluster node N has caught up with the cluster`.
5. Open the RADIUS, TACACS+ and metrics ports. The log says `Servers started`.

Step 4 guarantees that a node that was down for two hours answers its first request with the accounts added during those two hours already visible. The metrics port also opens only after this, so during startup even `/livez` cannot be reached.

### Snapshots

Raft records every write in a log. The log keeps growing, so after every `snapshot_every` applied entries (100000 by default) the node saves a snapshot of the database, and older log entries can then be discarded. A node that is new or has fallen too far behind takes a snapshot from the leader and then catches up with the log from there.

A snapshot is for replication, not a backup. The core edition has no backup feature; for how to protect the data, see [chapter 18](18-operations.md).

### When there is no leader

A node that has lost contact with the other nodes and cannot reach a majority has no leader that it can write to. What it does then depends on `stop_without_leader_ms`:

| Situation | `stop_without_leader_ms` not set (default) | Set, and there has been no leader for longer than this time |
| --- | --- | --- |
| Logins that only read data (for example the `sql` handler looking up a password) | Answered as usual from the local copy; the data may be old | No answer; counted in `rradius_radius_discarded_total{reason="no_leader"}` |
| Status-Server | Answered as usual | No answer |
| Logins whose script writes | No answer (the write fails) | No answer |
| Accounting | For the first few seconds the write waits for a leader and there is no answer; once there has been no leader for more than 5 seconds the write fails at once, and with an accounting spool the request is written to the spool and answered, without one there is no answer | Same as on the left |
| TACACS+ authentication and authorization | Answered as usual from the local copy | ERROR |
| TACACS+ accounting | Handed to the script as usual | Not affected by `stop_without_leader_ms` |
| `/readyz` | 200 | 503 `no leader` |

Two more rules about time:

- Once there has been no leader for more than 5 seconds, new writes fail at once and do not wait any more. An accounting request whose write failed goes to the accounting spool when there is one ([chapter 13](13-accounting.md)).
- The exception is the first write after the majority is lost: it was handed to the cluster before the 5 seconds had passed, so it waits until the cluster's 30 second timeout before it fails, and `query_timeout_ms` does not limit it. The message is `the cluster took the write and said nothing of it within 30 s; it may have been committed` (the write may have taken effect by then, so do not assume it was not written). When only the leader itself is left in the cluster, the message is `local client write timed out` and the login gets no answer. Measured: with two of three nodes stopped, the first write on the remaining node failed after 30 seconds, later writes failed within 12 milliseconds, and reads kept succeeding at once. This was measured with authentication writes; accounting writes and overlapping writes were not measured.
- A leader that has not had the confirmation of a majority for more than 5 seconds also counts as having no leader. An old leader that has been cut off does not think it can still write.

When should you set `stop_without_leader_ms`? When the NAS has several RADIUS servers configured. With it set, a node that is cut off stops answering, so the NAS asks the other nodes that are still in the cluster, and `/readyz` lets a load balancer take the node out. The Kubernetes example sets 10000. For a NAS that has only one server to ask, it is better not to set it: at least the read-only logins can still pass.

### Cluster ports and security

Every node opens two ports for the cluster:

- `raft`: used between nodes for replication.
- `api`: used between nodes to forward writes and for membership management; commands such as `user`, `session` and `cluster` also talk to the cluster through it.

These two ports are protected only by shared secrets (`secret_raft`, `secret_api`), and the traffic itself is **not encrypted**. So:

- Set `listen` to the node's internal address or WireGuard address, and do not let the outside network reach it.
- When the nodes are in different data centers or connected over a public network, the traffic between nodes has to go through an encrypted tunnel, for example WireGuard. The ha-cluster scenario is designed this way: the node addresses are in the WireGuard network `10.99.0.0/24`, and `listen` listens on the WireGuard interface only. How to set up WireGuard itself is outside the scope of this book.

> **Note:** When there is `[[cluster.nodes]]` and no `listen`, the two ports listen on `0.0.0.0`, and both `check` and startup warn `cluster.listen is not set, so the cluster's Raft and API ports are on all interfaces, …`. If you really want to listen on all addresses (for example in a container), write `listen = "0.0.0.0"` and the warning goes away.

## cluster settings

Every key of `[cluster]` needs a restart to take effect; on reload the log lists `[cluster]` after `these need a restart to apply:`. For the full types and defaults, see [Appendix A](90-config-reference.md).

| Key | Default | Meaning |
| --- | --- | --- |
| `node_id` | `1` | The id of this node, at least 1; it has to appear in `nodes` |
| `data_dir` | required | Directory for the database and the Raft log, relative to the configuration file |
| `listen` | `127.0.0.1` without `nodes`, `0.0.0.0` with `nodes` | The address the `raft` and `api` ports listen on; the port numbers come from this node's entry in `nodes` |
| `nodes` | only node 1, `127.0.0.1:8100`, `127.0.0.1:8200` | The list of nodes of the cluster; each entry has `id`, `raft` and `api`, all written as `host:port` |
| `secret_raft`, `secret_raft_file` | one of the two, required | Shared secret of the Raft port, at least 16 characters, the same on every node |
| `secret_api`, `secret_api_file` | one of the two, required | Shared secret of the API port, at least 16 characters, the same on every node |
| `migrations_dir` | none | Directory of SQL files applied at startup ([chapter 18](18-operations.md)) |
| `migrations_wait_ms` | `60000` | Time limit for each of the three startup steps (wait for a leader, apply migrations, catch up), in milliseconds |
| `snapshot_every` | `100000` | Number of Raft log entries between two snapshots |
| `read_connections` | twice the number of worker threads, at most 32 | Number of connections that read the local database, 1 to 128 ([chapter 19](19-performance.md)) |
| `stop_without_leader_ms` | none (always answer) | After there has been no leader for longer than this time, do not answer logins and Status-Server, in milliseconds |

Rules for `nodes` (`check` fails when they are broken):

- There has to be a node with id 1: `cluster.nodes: node 1 is required`. Node 1 is the one that creates the cluster on the first start, once every other node in the list is running and has answered that it has no cluster (see [When node 1 waits](#when-node-1-waits)).
- Ids cannot repeat, and cannot be 0.
- `node_id` has to be in the list: `cluster.node_id 4 is not in cluster.nodes`.
- Addresses have to be `host:port`: `cluster.nodes: node 1 raft '10.0.0.1' must be host:port`. `host` can be a name, for example `rradius-0.rradius-cluster` in Kubernetes.

In the configuration of every node, `nodes` lists all the nodes, including itself. Apart from the local values such as `node_id` and `listen`, the configurations of the nodes should preferably be identical. For how to check this with `config diff`, see [Check the settings of each node](#check-the-settings-of-each-node).

When `nodes` is set and has more than one node, the default of `ready_delay_ms` in `[shutdown]` becomes 5000 (0 for a single node), to give the NAS and load balancers time to ask other nodes. `[shutdown]` is described in [chapter 04](04-configuration.md), and the order of shutdown in [chapter 18](18-operations.md).

### What belongs to each node

The tables are replicated automatically, but the things below are not. Check them one by one when you plan several nodes:

| Item | Per node | Note |
| --- | --- | --- |
| Configuration file, scripts, dictionary, `migrations_dir` | One copy each | The content has to be the same. After a change, update every machine and reload or restart each one (check with `config diff`) |
| `secret_raft_file`, `secret_api_file`, the NAS shared secret files | One copy each | The content has to be the same |
| Certificates, private keys, revocation lists | One copy each | Replace them on every machine when you renew them, and send SIGHUP to each |
| Accounting spool | Each node replays only its own files | Keep it on a persistent disk. When a node breaks, what it has not replayed yet is lost. `rradius_acct_spool_bytes` above 0 means accounting that is not yet in the database; you can alert on it |
| EAP conversations in progress | Only on the node that received them | One login has to stay on one node ([chapter 11](11-eap.md)) |
| Cluster state | Each node sees its own | See [Cluster metrics](#cluster-metrics) |
| Metrics and the timing of `[log.trace]` | Each node's own | Add up the cluster total in the monitoring system; for a user you want to trace, set it on every machine |

### Tuning

- **`read_connections`:** logins are mostly reads, and when there are not enough connections, requests queue. When memory is tight you can lower it; every connection has its own cache.
- **`snapshot_every`:** when you lower it, a restart after an abnormal exit (power loss, SIGKILL) is faster, but with many writes, snapshots that are too frequent slow the service down.
- **More nodes do not raise the write limit.** Every write goes through the same leader and waits for the confirmation of a majority, and between separate hosts it also waits for the round trips between the nodes (see [Across data centers](#across-data-centers)). Logins (reads) are handled by each node on its own, so the more nodes, the higher the total.

The defaults and ranges of the three are in the table above. All need a restart, and for how to measure see [chapter 19](19-performance.md).

## Three nodes on one machine

Goal: run three nodes on one machine, and use them for the practice in the following sections.

> **Note:** This is a way to practice, and it differs from a multi-host deployment in a few ways:
> - All three nodes are on `127.0.0.1`, so every node needs different ports and a different `data_dir`. In production every host uses the same ports (for example 8100, 8200, 1812, 1813, 9812), and only the addresses differ.
> - Stopping a node means stopping a process. You do not meet network partitions, packet loss or clock skew, which only happen with several hosts. The seconds measured in this chapter apply to this machine only.
> - `status --cluster` cannot see the other nodes this way; see [See the state of the whole cluster](#see-the-state-of-the-whole-cluster).

### Prepare the directory

Run these commands from the root of the source repository:

```console
$ export PATH="$PWD/target/release:$PATH"
$ mkdir -p ~/rradius-lab/cluster
$ cp -R rradius-server/examples/scenarios/ha-cluster/accounting.lua \
        rradius-server/examples/scenarios/ha-cluster/migrations ~/rradius-lab/cluster/
$ cd ~/rradius-lab/cluster
$ for f in raft api nas; do openssl rand -hex 32 > $f.secret; done
$ chmod 600 *.secret
```

- `accounting.lua` and `migrations/` are the same as in the quick-start scenario: the three tables `users`, `user_reply` and `accounting`, and the demo accounts.
- `raft.secret` and `api.secret` are the cluster secrets, and `nas.secret` is the NAS shared secret. The three nodes share these three files; on several hosts, you copy the same files to every host.

### Settings for node 1

The exercises in this chapter use ports in the 27000 range, to avoid clashing with the default ports of other programs on the system:

| Node | `node_id` | `data_dir` | `raft` | `api` | RADIUS auth/acct | TACACS+ | Metrics |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 1 | `data1` | 27001 | 27011 | 27021/27031 | 27051 | 27041 |
| 2 | 2 | `data2` | 27002 | 27012 | 27022/27032 | 27052 | 27042 |
| 3 | 3 | `data3` | 27003 | 27013 | 27023/27033 | 27053 | 27043 |
| 4 (used in [Add a node](#add-a-node)) | 4 | `data4` | 27004 | 27014 | 27024/27034 | 27054 | 27044 |

```toml
# node1.toml
dictionary = "/path/to/share/dictionary"   # change to the absolute path of the dictionary

[lua]
script = "accounting.lua"
query_timeout_ms = 2000

[cluster]
node_id = 1
listen = "127.0.0.1"
data_dir = "data1"
secret_raft_file = "raft.secret"
secret_api_file = "api.secret"
migrations_dir = "migrations"
migrations_wait_ms = 120000

[[cluster.nodes]]
id = 1
raft = "127.0.0.1:27001"
api = "127.0.0.1:27011"

[[cluster.nodes]]
id = 2
raft = "127.0.0.1:27002"
api = "127.0.0.1:27012"

[[cluster.nodes]]
id = 3
raft = "127.0.0.1:27003"
api = "127.0.0.1:27013"

[auth]
policy = "first_match"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ? AND enabled = 1"
reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"

[accounts]
add = "INSERT INTO users (username, password) VALUES (?1, ?2)"
show = "SELECT username, enabled FROM users WHERE username = ?1"

[sessions]
list = "SELECT username, nas, session_id, started, updated FROM accounting WHERE stopped IS NULL ORDER BY started, session_id"

[radius]
bind = "127.0.0.1"
auth_port = 27021
acct_port = 27031
status_server = true

[[radius.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "nas.secret"

[radius.accounting_spool]
dir = "spool1"

[tacacs]
bind = "127.0.0.1"
port = 27051

[[tacacs.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "nas.secret"

[metrics]
bind = "127.0.0.1"
port = 27041
```

A few choices that matter for the cluster:

- `query_timeout_ms = 2000`: the total time one request waits for the database. After the cluster loses the majority, the first write that was already handed to the cluster still waits 30 seconds before it fails (see the rule above), and this value does not shorten that; it limits the script's reads and the writes that have not been handed over yet. The ha-cluster scenario uses the same value.
- `migrations_wait_ms = 120000`: when all three nodes restart together, the node that comes up first has to wait for the others, so give it a longer time.
- `[radius.accounting_spool]`: when the database cannot be written, accounting is written to the local disk first and sent again later ([chapter 13](13-accounting.md)). Every node uses its own directory.
- `[accounts]`, `[sessions]`: let this chapter confirm replication with `user add` and `session list` ([chapter 07](07-users-and-chains.md), [chapter 13](13-accounting.md)).

Copy `node1.toml` to `node2.toml` and `node3.toml`, and change `node_id`, `data_dir`, the RADIUS, TACACS+ and metrics ports as in the table above, and the `dir` of `[radius.accounting_spool]` (`spool2`, `spool3`). The `[[cluster.nodes]]` entries of the three files are exactly the same.

```console
$ for n in 1 2 3; do rradiusd check --config node$n.toml; done
configuration ok
configuration ok
configuration ok
```

### Start them together

Start the three nodes together. On the first start node 1 creates the cluster, and only once nodes 2 and 3 are both running and have answered that they have no cluster; nodes 2 and 3 then join it. No node serves before that, so one missing node holds back all of them (see [When node 1 waits](#when-node-1-waits)). Open a terminal for each node, or put them in the background like this and write the logs to files:

```console
$ for n in 1 2 3; do rradiusd serve --config node$n.toml > node$n.log 2>&1 & done
```

On this machine, it takes about 9 seconds from start until `/readyz` of all three nodes answers `ok`. The log of node 1 (timestamps removed):

```text
WARN hiqlite::store::state_machine::sqlite::writer: No metadata exists inside the DB yet
WARN hiqlite::store::state_machine::sqlite::writer: No metadata exists inside the DB yet
…
INFO rradius_core::cli: cluster node 1 started
INFO rradius_core::cli: cluster node 1 has caught up with the cluster
INFO rradius_core::cli: Servers started. Press Ctrl+C to stop.
INFO rradius_core::radius: RADIUS Server listening on 127.0.0.1:27021 and 127.0.0.1:27031
INFO rradius_core::tacacs: TACACS+ Server listening on 127.0.0.1:27051
INFO rradius_core::metrics: Metrics listening on 127.0.0.1:27041
```

Until the cluster has formed, the logs of nodes 2 and 3 have several lines of `ERROR hiqlite::init: … Raft is not initialized` or `Connection refused`: they are waiting for node 1 to create the cluster. As long as `has caught up with the cluster` follows, you can ignore these ERROR lines. When nodes 2 or 3 come up later than node 1, its log also has `Waiting for the other nodes before creating a new cluster` warnings until they answer.

### When node 1 waits

When node 1 starts without cluster data (the first start, or an empty `data_dir`), it asks the other nodes in `[[cluster.nodes]]` over their API ports before it does anything:

- When any node answers with the members of a cluster, node 1 joins that cluster. It does not create one.
- When every other node answers, in the same round, that it has no cluster, node 1 creates the cluster.
- Otherwise it asks again after 1 second. A node that is not running or cannot be reached, has another `secret_api`, takes more than 3 seconds to answer, is starting or stopping with cluster data, or answers with any other error may hold the cluster, so node 1 waits for it.

While it waits, node 1 opens no RADIUS, TACACS+ or metrics port, and not yet its own Raft and API ports either, so the logs of the other nodes report connection errors for it. Every few seconds (6 to 8 in the tests below) it logs one warning that lists the nodes it still waits for, each with the reason:

```text
WARN hiqlite::init: Waiting for the other nodes before creating a new cluster: every one of them has to answer that it has none. Still waiting for node 2 (127.0.0.1:27212): Connection refused (os error 61); node 3 (127.0.0.1:27213): Connection refused (os error 61)
```

| Reason at the end of the entry | Meaning | What to do |
| --- | --- | --- |
| `Connection refused (os error 61)` (the number depends on the operating system) | Nothing listens on that node's API address | Start the node; check its `api` address and the firewall |
| `no answer within 3 s` | The node did not answer in time | Check the node and the network between the two |
| `Token: Invalid API Secret` | The two nodes have different `secret_api` | Use the same API secret on every node |

How long the wait lasts:

- With `migrations_dir`, node 1 exits with exit code 1 after `migrations_wait_ms`, and the last line is `cluster was not ready to apply migrations: node 1 did not join within …` (it says "join" also on a node 1 that is waiting to create the cluster). Nodes 2 and 3, which are waiting to join, end the same way. A process manager starts them again and they wait again.
- Without `migrations_dir`, node 1 waits for as long as it takes. `stop_without_leader_ms` does not limit this wait.
- SIGTERM ends a waiting node 1 with exit code 0 after about 25 seconds, with `stopped during startup` in the log. Nothing has been created.

When a node is broken for good before the cluster exists, either repair it, or remove its entry from `[[cluster.nodes]]` on **every** node and start again; node 1 then creates the cluster with the nodes that are left, and you can add the third one later ([Add a node](#add-a-node)). A list that names only node 1 has nobody to ask, and node 1 creates the cluster at once.

> **Warning:** Do not shorten the list to get past the wait when any node may still hold cluster data, for example after node 1 lost its disk. A node 1 that does not list the nodes that hold the data creates a second, empty cluster.

Tested on 2026-11-28 with three nodes on one machine: with node 3 not running, nodes 1 and 2 served nothing and, with `migrations_wait_ms = 20000`, both exited with code 1 after 20 seconds; a node 1 without `migrations_dir` was still waiting after 35 seconds and exited 25.0 seconds after SIGTERM; with node 3 removed from the lists of nodes 1 and 2, those two formed a cluster with two voters. The three reasons in the table were produced by a stopped node, a port that accepts connections and never answers, and a different `secret_api`. The other cases in the list above, such as a node with cluster data that is starting or stopping, were not produced; they are described from the code. It was not tested on several physical hosts.

## Check that the cluster has formed

### cluster members

`cluster members` asks the nodes in the configuration who the leader is, and then lists the members as the leader sees them:

```console
$ rradiusd cluster members --config node1.toml
node 1  voter    leader  raft 127.0.0.1:27001  api 127.0.0.1:27011
node 2  voter            raft 127.0.0.1:27002  api 127.0.0.1:27012
node 3  voter            raft 127.0.0.1:27003  api 127.0.0.1:27013
```

- `voter`: a node that votes and counts toward the majority. A node that has just joined and is still catching up is a `learner` for a while.
- `leaving`: a node that votes before a change of the members that is in progress, and not after it. While there is such a change, the last line is `a change of the members is in progress`. An ordinary change can show both for a moment; when they stay, see [Remove a node](#remove-a-node).
- `leader`: the current leader. Which node is the leader is not fixed.
- You can use the configuration file of any node; the command finds the leader by itself.
- Exit codes: 0 success; 2 a bad argument or configuration, or a wrong API secret; 3 no node reported a leader within 30 seconds.

### The Node section of status

`status` reads the metrics port of one node. The `Node` section is the cluster as this node sees it:

```console
$ rradiusd status --config node2.toml
rradius status of 127.0.0.1:27042 over 1.0 s

Node
  Ready                     ok
  Leader                    0
  Has a leader              1
  Caught up                 1
  Applied index             16
  Seconds without a leader  0
  Version                   0.1.0
  Seconds running           13
…
```

| Line | Meaning |
| --- | --- |
| `Ready` | The content of `/readyz`: `ok`, `no leader`, `stopping` |
| `Leader` | Whether this node is the leader (1 is yes) |
| `Has a leader` | Whether this node has a leader that it can write to |
| `Caught up` | Whether the node has caught up with the cluster since startup |
| `Applied index` | Up to which Raft log entry the local copy has applied; the numbers of the nodes should be close |
| `Seconds without a leader` | How long there has been no leader; 0 when there is a leader |

The other sections of `status` are described in [chapter 17](17-monitoring.md).

## Check that writes are replicated

Goal: write to one node and read from another.

Add an account through the configuration of node 1, then log in once on each of the three nodes:

```console
$ printf 'erin-password\n' | rradiusd user add --config node1.toml erin
added erin
$ rradiusd user show --config node3.toml erin
username=erin
enabled=1
$ for n in 1 2 3; do rradius-bench run --server 127.0.0.1 --auth-port 2702$n --secret "$(cat nas.secret)" \
    --user erin --password erin-password --load pap,concurrency=1 --requests 1 | grep 'pap:'; done
  pap: 26/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
  pap: 26/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
  pap: 26/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
```

Now send one accounting request to node 2 and look at it from node 3:

```console
$ rradius-bench run --server 127.0.0.1 --acct-port 27032 --secret "$(cat nas.secret)" \
    --user erin --load acct-start,concurrency=1 --requests 1 | grep 'acct-start:'
  acct-start: 729/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
$ rradiusd session list --config node3.toml
username=erin
nas=192.0.2.1
session_id=4220000000000000
started=1795384786
updated=1795384786
```

The script write on node 2 goes to the leader. The leader replies only after a majority has confirmed, and only then does node 2 send the Accounting-Response. When `accounting 1` appears, the data is already in the cluster.

## Stop the leader

Goal: stop the leader, and watch the cluster elect a new leader by itself and writes resume.

First find the leader with `cluster members` (node 1 above) and send it SIGTERM. At the same time send an accounting request to node 2 every 0.5 seconds and see whether writes are interrupted. The course measured on this machine:

| Time since SIGTERM | Old leader (node 1) | Accounting to node 2 |
| --- | --- | --- |
| 0 s | `/readyz` changes to 503 `stopping`, but requests are still answered | As usual |
| 5 s | Stops accepting new requests (`ready_delay_ms` defaults to 5000) and waits for requests in progress | As usual |
| about 14.5 s | Closes the database, the process exits | Starts to slow down, one request times out |
| about 19 s | — | Node 3 is elected the new leader |
| about 25 s | — | Back to normal |

- During the 14.5 seconds of shutdown the old leader is still the leader, so writes continue as usual. The time with no leader is from when it closes the database until the new leader can accept writes, about 10 seconds on this machine.
- During this time, logins that only read data are not affected: followers answer from the local copy.
- `rradius_cluster_leader` on node 2 stays at 1: the metric is updated once per second, and during the election a follower still knows some leader. So do not judge from this metric alone whether writes are interrupted; look at the responses to accounting and at the accounting spool as well.

After the new leader is elected:

```console
$ rradiusd cluster members --config node2.toml
node 1  voter            raft 127.0.0.1:27001  api 127.0.0.1:27011
node 2  voter            raft 127.0.0.1:27002  api 127.0.0.1:27012
node 3  voter    leader  raft 127.0.0.1:27003  api 127.0.0.1:27013
```

Node 1 has stopped, but it is still a voter: stopping is not the same as leaving the cluster. With one of three voters stopped, the remaining two are still a majority, so writes still work.

Start node 1 again. It comes back as a follower and starts serving only after it has caught up:

```text
INFO rradius_core::cli: cluster node 1 started
INFO rradius_core::cli: cluster node 1 has caught up with the cluster
INFO rradius_core::cli: Servers started. Press Ctrl+C to stop.
```

> **Note:** Do not stop a node with `kill -9`. A node that was killed has to rebuild its local database at the next start (see [Restarting nodes and rebuilding data](#restarting-nodes-and-rebuilding-data)). In a multi-node cluster, the process exited about 15 seconds after SIGTERM on this machine, so the wait time of systemd, Docker and Kubernetes has to be longer than that; see [chapter 18](18-operations.md).

## Lose the majority

Goal: see what a node does when it cannot reach a majority, and what `stop_without_leader_ms` does.

Add this to `[cluster]` of node 1:

```toml
# node1.toml (add to [cluster])
stop_without_leader_ms = 10000
```

Restart node 1 so that it takes effect, then send SIGTERM to nodes 2 and 3 and wait until both processes have exited. After that, every ten or so seconds, send node 1 a PAP login, a Status-Server, an accounting request and a TACACS+ login. Measured results:

| After the two nodes stopped | `Seconds without a leader` | `/readyz` | PAP login | Status-Server | Accounting | TACACS+ login |
| --- | --- | --- | --- | --- | --- | --- |
| 0 s | 0 | 200 `ok` | accept | answered | no answer | accept |
| 8 s | 2 | 200 `ok` | accept | answered | no answer | accept |
| 16 s | 10 | 503 `no leader` | no answer | no answer | answered (written to the spool) | ERROR |
| 69 s | 63 | 503 `no leader` | no answer | no answer | answered (written to the spool) | ERROR |

- Node 1 noticed after about 6 seconds that there was no leader.
- Accounting in the first few seconds got no answer: the script's write was waiting for a leader. The first write after the majority is lost waits for the cluster's 30 second timeout (not `query_timeout_ms`), and the NAS retransmits.
- Once there has been no leader for more than 5 seconds, writes fail at once, and accounting is written to the spool and answered to the NAS.
- Once there has been no leader for more than 10 seconds (`stop_without_leader_ms`), logins and Status-Server get no answer and are counted in `rradius_radius_discarded_total{reason="no_leader"}`; TACACS+ authentication answers ERROR (`rradius-bench` prints `status: 0x07`). `/livez` stays 200: the process is alive, only not ready.

What `status` looks like:

```console
$ rradiusd status --config node1.toml
rradius status of 127.0.0.1:27041 over 1.0 s

Node
  Ready                     no leader
  Leader                    0
  Has a leader              0
  Caught up                 1
  Applied index             96
  Seconds without a leader  84
…
Accounting spool
  Bytes waiting  545
  Replayed       0.0/s (0)
…
```

The log keeps showing `Find DB leader error: Connect: Leader vote is in progress`, and a failed script write is logged as `Lua script error (radius/accounting): runtime error: no leader for 83 s`.

Now start nodes 2 and 3. A few seconds after the cluster has a majority again, `/readyz` of node 1 returns to `ok`, and the 5 accounting requests in the spool are sent into the database automatically:

```text
rradius_cluster_leader 1
rradius_cluster_seconds_without_leader 0
rradius_acct_spool_written_total 5
rradius_acct_spool_replayed_total 5
rradius_acct_spool_bytes 0
```

## Restarting nodes and rebuilding data

### Normal restart

Stop a node (SIGTERM) and start it again. It comes back with the data in its own `data_dir`, takes the writes made during its downtime from the leader, and serves only after it has caught up. This is the standard way for upgrades, for changing the `[cluster]` settings and for replacing host hardware.

> **Note:** A starting node first waits for the leader to answer one read from it, and only then reads "which migrations the cluster has applied". When you start it in the few seconds right after the cluster got its majority back and is still electing, it waits at this step; when no leader has answered within `migrations_wait_ms`, it follows the table in [Restart the whole cluster](#restart-the-whole-cluster). When the leader changes again while the list is being read after the leader answered, the node reads again every 200 ms until `migrations_wait_ms`, counted from the start, is used up; only then does it exit with exit code 1, and the last line is `cluster was not ready to apply migrations: cannot read applied migrations: …`, carrying the last error. This retry is covered by a unit test; the gap during an election itself has not been reproduced on multiple real machines. The later step, writing the catch-up marker, is retried too.

### After a forced stop

Kill node 3 with `kill -9` and start it again. The log:

```text
WARN hiqlite_wal::log_store: LockFile data3/logs exists already - this is not a clean start!
WARN hiqlite::store::state_machine::sqlite::state_machine: Lock file already exists: data3/state_machine/lock\nNode did not shut down gracefully - auto-rebuilding State Machine
…
INFO rradius_core::cli: cluster node 3 started
INFO rradius_core::cli: cluster node 3 has caught up with the cluster
```

When a node was not shut down cleanly, the next start rebuilds the local database from the Raft log. With little data this is fast; with a lot of data it takes longer.

The server also exits on its own in some situations, with exit code 1, so that the process manager restarts it:

- The Raft core of the node stopped because of an error, for example a follower fell so far behind that the leader had already discarded the log entries it needs. The log says `cluster node N: its Raft core stopped (…); exiting so that a restart recovers it`.
- A panic occurred in the cluster library: `cluster node: a panic in its database (…); exiting so that a restart recovers it`.

### Rebuild from the other nodes

When the disk of a node has failed, its data directory is gone, or you suspect its data is wrong: stop it, empty its `data_dir`, and start it again with the same configuration (the same `node_id` and addresses). It copies the data back from the other nodes.

```console
$ rm -rf data3
$ rradiusd serve --config node3.toml > node3.log 2>&1 &
```

```text
WARN hiqlite::init: Found remote metrics and we (3) are a Raft member
…
INFO rradius_core::cli: cluster node 3 started
INFO rradius_core::cli: cluster node 3 has caught up with the cluster
```

On this machine it takes about 2 seconds from start to ready, after which `erin` logs in on node 3 with accept. This way needs a majority of the other nodes to be still running.

Node 1 is rebuilt the same way. When none of the other nodes answers with the cluster, node 1 does not take its empty `data_dir` for a first start: it waits for them ([When node 1 waits](#when-node-1-waits)), and joins their cluster once they are back. Tested on 2026-11-28 with all three stopped and node 1's `data_dir` removed: started alone, node 1 answered nothing, and after nodes 2 and 3 were started it joined them and had the accounting record written before the stop.

> **Warning:** Do not empty the `data_dir` of a majority of the nodes at the same time. The data exists only on the nodes of the cluster, and when the copies on a majority are gone, the data is gone.

### A cluster that node 1 split

Earlier builds, without the wait described in [When node 1 waits](#when-node-1-waits), let a node 1 with an empty `data_dir` create a cluster of its own while the other nodes could not be reached, and answer logins and accounting from it. After the other nodes came back, one of two things was seen:

- All three nodes ran and `cluster members` looked normal, but records written while node 1 was alone were on node 1 only, and restarting did not make the copies agree.
- Node 1 panicked (`panicked at …/openraft-0.9.25/src/raft_state/log_state_reader.rs:23`) at every later start, although the log says `exiting so that a restart recovers it`. Nodes 2 and 3 could also exit with `did not catch up with the cluster within …`.

To recover:

1. Install the current build on all three nodes. Installing it does not repair a split that has already happened.
2. Start nodes 2 and 3 and check that they have a leader: `rradiusd cluster members --config node2.toml`.
3. Stop node 1. The records it took while it was alone are not on the other nodes, and the NAS, which got an `Accounting-Response` for them, does not send them again. If you may need them, copy its `data_dir` before the next step (the database is the SQLite file `state_machine/db/hiqlite.db` in it).
4. Empty the `data_dir` of node 1 and start it.

Tested on 2026-11-28 with three nodes on one machine, producing each of the two states with an earlier build and recovering with the current one: afterwards the three copies agreed and node 1 served, and what node 1 had taken while alone was gone. Reading those records out of the copied directory was not tried.

### Replace a failed host

When a host is broken and its data cannot be recovered, replace it with a new host. You do the same as above, but the configuration has to match:

1. The new host uses the **same `node_id`** and the same addresses (its `raft` and `api` in `[[cluster.nodes]]`); the configuration file, secret files and `migrations_dir` are the same as on the other nodes. If you have added or removed nodes before, `[[cluster.nodes]]` has to list all the current members (look with `cluster members` first).
2. `data_dir` is empty. When the old host's data still exists but cannot be trusted, delete it completely before you start.
3. Start. It gets the whole data from the other nodes, and it starts answering only after the log shows `cluster node N has caught up with the cluster`.
4. Confirm: `rradius_cluster_applied_index` is close to the other nodes and `rradius_cluster_leader` is 1; log in once on it with an account that was added while it was down.

- **A majority of the other nodes has to be online.** The new node gets its data from them.
- **It is the same when the broken node is the leader.** The remaining nodes elect a new leader by themselves; logins that need a write get no answer during the election, and accounting goes to the spool and is sent later (for the seconds see [Across data centers](#across-data-centers)).
- **It does not matter if the leader has made a snapshot and discarded old log entries,** because the new node receives the snapshot directly.
- **Management commands wait a little longer.** In the time right after the leader stopped, commands such as `user` connect to the old leader first and switch to the new one only after the connection times out, so they may take more than ten seconds, with 30 seconds as the limit.
- **The same id is not stopped by `node id N is in use`:** when the old address does not answer, the check passes as usual (see [When the id is already in use](#when-the-id-is-already-in-use)).
- **What was tested:** on 2026-11-13, with the three nodes of `compose.cluster.yaml`, in Docker on one host, a broken host was simulated by force-stopping and deleting the container and its data volume. A replaced follower caught up in about 2 seconds; after setting `snapshot_every = 50` and writing about 200 entries so that the leader had made a snapshot, a replaced leader caught up in about 5 seconds, the applied index was the same on all three, and the account added in the meantime could log in on every node. It was not tested on several physical hosts or through WireGuard.

### Restart the whole cluster

When all three nodes are stopped and started together, each node waits for a leader and, once there is one, catches up before it serves. It waits for the leader for at most `migrations_wait_ms`, so that value has to be longer than the time you expect "the last node comes up" to take. A node that still has no leader then does what the table below shows for a node restarted alone.

A write of the catch-up marker that meets an election is made again until `migrations_wait_ms`. Tested on 2026-11-27 with three nodes on one machine and `migrations_wait_ms = 15000`: before each of 10 restarts of all three nodes, one former follower ran alone, without a majority, for 20 seconds. No node exited, and all three had caught up about 6.6 seconds after the start. Before this write was retried, a node exited in 4 of 10 such restarts (`cannot write the marker: … has to forward request to: None, None`).

When only one node is restarted and the other two have not come back: a node that has joined the cluster before does not wait without limit for a leader, whatever its role was when it stopped. When no leader has answered it within `migrations_wait_ms` (60 seconds by default), this happens:

| Settings and local copy | What the node does |
| --- | --- |
| No `stop_without_leader_ms`, no `migrations_dir` | Prints a warning and serves from its local copy |
| No `stop_without_leader_ms`, every file of `migrations_dir` applied in the local copy | Prints the same warning and serves from its local copy |
| No `stop_without_leader_ms`, a file not applied in the local copy | Exits with code 1, `no leader to apply migration file '…', which this node has not applied` |
| No `stop_without_leader_ms`, a file recorded as applied under another name or with other content | Exits with code 2 |
| `stop_without_leader_ms` set | Exits with code 1, `cluster was not ready: no usable leader within …, and stop_without_leader_ms is set`, without answering any request |

The warning reads `cluster node 1: no usable leader within …; serving without a leader before catching up, so answers come from this node's replica, which may be behind`, and `rradius_cluster_caught_up` is 0. After the other nodes come back, the log says `cluster node 1 has caught up with the cluster` and the metric becomes 1.

Tested on 2026-11-27 with three nodes on one machine and `migrations_wait_ms = 10000`, restarting a follower alone, once stopped while the leader was still running and once stopped after the leader: with and without `migrations_dir` it served from its local copy, and with `migrations_dir` and `stop_without_leader_ms = 10000` it exited with code 1. A former leader (node 1 in the test) restarted alone, stopped while the others were still running, served from its local copy in the same way, with and without `migrations_dir`. Stopping a node that serves without a leader (SIGTERM, default `[shutdown]` times) ended it with exit code 0, after about 20 seconds for a former follower and about 15 seconds for a former leader.

A node that has been removed and can ask the current leader does not follow this table: it does not start serving, shuts its cluster node down and exits with exit code 1; see [When a removed node starts again](#when-a-removed-node-starts-again).

A node restarted alone answers read-only logins from a copy that may be old, and writes fail until a majority is back (see [When there is no leader](#when-there-is-no-leader)).

## Add a node

Goal: let a fourth node join the running cluster, without restarting any other node.

A new node joins by itself; there is no "join" command. The conditions:

- The new node's `data_dir` is empty (or does not exist).
- The `[[cluster.nodes]]` of its configuration lists itself and the current leader. The simplest way is to list all the nodes.
- `node_id` is not used by any other running node.

Make `node4.toml` following the table in [Settings for node 1](#settings-for-node-1), with `[[cluster.nodes]]` listing nodes 1 to 4, and then start it:

```console
$ rradiusd serve --config node4.toml > node4.log 2>&1 &
```

On this machine it is ready after about 4 seconds, and the log shows `cluster node 4 started`, `has caught up with the cluster` and `Servers started` in turn. An account created before it joined can log in on node 4 at once:

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 27024 --secret "$(cat nas.secret)" \
    --user erin --password erin-password --load pap,concurrency=1 --requests 1 | grep 'pap:'
  pap: 22/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
$ rradiusd cluster members --config node1.toml
node 1  voter            raft 127.0.0.1:27001  api 127.0.0.1:27011
node 2  voter            raft 127.0.0.1:27002  api 127.0.0.1:27012
node 3  voter    leader  raft 127.0.0.1:27003  api 127.0.0.1:27013
node 4  voter            raft 127.0.0.1:27004  api 127.0.0.1:27014  (not in this configuration)
```

`(not in this configuration)` means that the list in `node1.toml` does not have node 4. This does not affect operation: nodes 1 to 3 do not have to be restarted because of node 4, and they come back as usual when they are restarted. Looking with `node4.toml` shows no such mark. Find time later to add node 4 to the configuration of the other nodes, so that the lists of all nodes agree and `cluster members` stops reminding you.

> **Note:** Keep the total number of nodes odd. Going from 3 to 4 nodes changes the majority from 2 to 3, and the number of nodes that can be down at the same time is still only 1.

A few details when a new node joins:

- **A node that joins with empty data has to list the current leader.** It asks to join only the nodes listed in its own configuration, and the other nodes only tell it to go to the leader; if the leader is not in its list, it cannot join. When the configuration has `migrations_dir`, it waits until `migrations_wait_ms` and exits with exit code 1 with the message `node N did not join within …`, and after the process manager restarts it, it starts over. So listing all current members is the safest. Measured on 2026-11-13: after the leader had changed to node 4, a node 1 with empty data whose configuration listed only 1 to 3 could not join; after node 4 was added, it joined within 8 seconds.
- **A member that restarts with its data is not affected.** It knows the members from its own data, and can return to the cluster even when the configuration does not list the leader (about 14 seconds in the same test).
- **A node with a different secret cannot join,** and the log of the new node gives the reason: a different `secret_raft` gives `Invalid ChallengeResponse`, a different `secret_api` gives `Invalid API Secret`; the members of the cluster do not change (measured on 2026-11-18).
- **When the new node's `migrations_dir` has one file more than the others,** on 2026-11-18 only joining and restarts of the other nodes were looked at; both worked as usual, and the logs did not mention the difference. The `migrations_dir` of every node has to stay the same; this combination was not examined in depth.

### When the id is already in use

When the `node_id` of the new node is the id of a node that is still running, but with a different address, it refuses to start, with exit code 2:

```text
cannot start cluster node 2: node id 2 is in use: the cluster has it at api 127.0.0.1:27012 (raft 127.0.0.1:27002), and that node answers, but this node's configuration gives it api 127.0.0.1:27015 (raft 127.0.0.1:27005); use an id no node has (see `rradiusd cluster members`), or stop the running node first
```

Joining with the same id would rewrite the address of that node in the cluster, so the program does not allow it. When you replace a host, either use a new id or stop the old node first.

## Remove a node

Goal: take node 4 out of the cluster.

`cluster remove` checks these in order, and sends nothing when any check fails:

1. The id has to be a positive integer: `the node id must be a positive integer, not 'abc'`, exit code 2.
2. The id has to be a member: `no node 9 in the cluster`, exit code 1.
3. The node has to be stopped already: a node that is running cannot be removed.
4. The number of voters left after the removal cannot be less than three unless you add `--force`; leaving zero is always refused.

First try to remove it while node 4 is still running:

```console
$ rradiusd cluster remove --config node1.toml 4
node 4 still answers at 127.0.0.1:27014: stop it first, then remove it
```

Exit code 2. A node that is removed while running keeps answering with the old data it holds until it finds that out, so you must stop it first (see [Why you must stop the node first](#why-you-must-stop-the-node-first)).

Stop node 4 (SIGTERM; it exits after 15 seconds on this machine), and then remove it:

```console
$ rradiusd cluster remove --config node1.toml 4
removed node 4
$ rradiusd cluster members --config node1.toml
node 1  voter            raft 127.0.0.1:27001  api 127.0.0.1:27011
node 2  voter            raft 127.0.0.1:27002  api 127.0.0.1:27012
node 3  voter    leader  raft 127.0.0.1:27003  api 127.0.0.1:27013
```

Now take out node 3 as well. Stop it first, in the same way. This removal would leave the cluster with only two voters, so:

```console
$ rradiusd cluster remove --config node1.toml 3
removing node 3 leaves two voters, and the cluster stops writing when either stops; give --force to remove it anyway
$ rradiusd cluster remove --force --config node1.toml 3
removed node 3
$ rradiusd cluster members --config node1.toml
node 1  voter    leader  raft 127.0.0.1:27001  api 127.0.0.1:27011
node 2  voter            raft 127.0.0.1:27002  api 127.0.0.1:27012
node 3  not in the cluster (listed in this configuration)
```

`not in the cluster (listed in this configuration)` means it is in the configuration and not in the cluster.

To add a removed node back, follow the conditions of [Add a node](#add-a-node): empty its `data_dir` and start it. Node 3 above came back into the cluster as a voter within 2 seconds after `data3` was emptied and it was started. Starting it with the data it had does not bring it back: the node ends by itself (see [When a removed node starts again](#when-a-removed-node-starts-again)).

**When only one voter would remain** (two voters, one of them stopped), `remove --force` does not finish on the first try, apparently because the change needs a majority of the current voters and the node being removed is one of them. The command says what to do. Run on 2026-11-27 with two nodes on one machine, node 2, a follower, stopped:

```console
$ rradiusd cluster remove --force --config node1.toml 2
the removal of node 2 may have been made, but the cluster did not answer within 30 s; check with cluster members
the change needs node 2 to commit: start it again with its data, wait about 20 s, stop it, and run this command again
$ rradiusd cluster members --config node1.toml
node 1  voter    leader  raft 127.0.0.1:27001  api 127.0.0.1:27011
node 2  leaving          raft 127.0.0.1:27002  api 127.0.0.1:27012
a change of the members is in progress
$ rradiusd cluster remove --force --config node1.toml 2
127.0.0.1:27011: HTTP 500 Internal Server Error: {"ClientWriteError":{"APIError":{"ChangeMembershipError":{"InProgress":{"committed":{"leader_id":{"term":1,"node_id":1},"index":4},"membership_log_id":{"leader_id":{"term":1,"node_id":1},"index":9}}}}}}
the change needs node 2 to commit: start it again with its data, wait about 20 s, stop it, and run this command again
```

The first command ends with exit code 3 after 30 seconds, the second with exit code 1 at once; the second line of each says how to finish:

1. Start the stopped node again with its data, wait about 20 seconds, and stop it.
2. Run `cluster remove --force <id>` once more. It prints `removed node <id>`:

```console
$ rradiusd cluster remove --force --config node1.toml 2
removed node 2
$ rradiusd cluster members --config node1.toml
node 1  voter    leader  raft 127.0.0.1:27001  api 127.0.0.1:27011
node 2  not in the cluster (listed in this configuration)
```

The node started in step 1 is still in the member list at that time (shown as `leaving`), so it starts as usual and serves, and does not end by itself; in the integration test the cluster accepted writes again about 4 seconds after it started (measured once). Starting that node again after the removal has finished ends it with exit code 1 (see [When a removed node starts again](#when-a-removed-node-starts-again)).

**When the stopped node was the leader,** the node that is left cannot become the leader alone, so `cluster members` and `cluster remove` can only wait and, after 30 seconds, print `the cluster did not answer within 30 s` (exit code 3); no removal has been sent. Start the stopped node again, and stop it only when `cluster members` shows the other node as `leader`; if it leads again, stop it and start it once more (in one run on 2026-11-27 the other node led after the second start). Then remove it as above.

With an empty `data_dir` instead, a test on 2026-11-27 did not complete the change (watched for about 20 seconds), so do not count on removing a node whose data is lost. Think twice before running a cluster with only two voters.

Exit codes of `cluster remove`: 0 success; 1 there is no such member, or the cluster refused the removal; 2 a bad argument or configuration, or a check did not pass; 3 no answer within 30 seconds. With exit code 3 the removal may already have been done, and the message tells you to confirm with `cluster members` and not to simply run it again.

### When a removed node starts again

A removed node receives no later writes, so its data stays as it was when it was removed. A starting node asks the other nodes for the member list of the current leader, and ends with exit code 1 when it is not on that list: it does not start serving, and it writes nothing to the cluster. The log:

```text
… ERROR rradius_core::cli: cluster node 3 has been removed from the cluster: node 1, its leader, does not list it (its members: node 1 at 127.0.0.1:27011, node 2 at 127.0.0.1:27012). To retire this host, stop and disable the service. To add it back, first confirm that this is the cluster you expect, then empty data_dir '…/data3' and start it again
```

There are two ways out:

- **The host is being retired:** stop and disable the service (for example `systemctl disable --now rradius`), and take it off the RADIUS server list of the NAS. If it stays enabled, `Restart=on-failure` of systemd or the restart policy of the container keeps restarting it, and it ends every time.
- **Add it back to the cluster:** first run `cluster members` with a configuration that points at the cluster you expect, and check that those are the members the log lists; then empty the `data_dir` of this node and start it.

What counts as removed:

- A node is removed only when the member list of the current leader (a node that says it is the leader and was acknowledged by a majority within the last 5 seconds) omits it, and that list is newer than the one the local data remembers (or the local list itself already omits the node). It does not count as removed when any such leader lists it. A node still on the list starts as usual, including a `learner`, and the stopped node halfway through [removing one of two voters](#remove-a-node) (`leaving`).
- The question goes over the API ports to the other nodes of `[[cluster.nodes]]` and those the local data remembers.
- A running node that has had no leader for more than 5 seconds, or whose own member list does not have it as a voter (it is not there, or it is a `learner`), asks every 5 seconds; once it finds it has been removed, it stops answering and ends with exit code 1.

> **Warning:** A removed node that cannot reach the cluster has no way to know that it was removed. When no current member answers while it starts (a network partition, or all other nodes stopped), it follows the table in [Restart the whole cluster](#restart-the-whole-cluster): without `stop_without_leader_ms` it answers logins from its old local data, and it ends only once it can find the leader. To keep it from answering in that case, set `stop_without_leader_ms`.

Tested on 2026-11-27 with three nodes on one machine (debug build): a stopped follower was removed and then started three times with its old data, one round with and one without `migrations_dir`. Every start ended with exit code 1 (about 16 to 21 seconds after starting, most of it spent shutting the node down) without listening on the RADIUS ports. Started again with an empty `data_dir`, it was a voter again after about 2 seconds. The case with `stop_without_leader_ms = 10000`, and answering from local data while all other nodes are stopped, are covered by integration tests. It was not tested on several physical hosts.

### Why you must stop the node first

- **A node that was removed but is still running answers with old data for a while.** It no longer receives new writes, so an account added after the removal is rejected on it, and an account disabled after the removal can still log in (measured on 2026-11-13 in Docker). When the NAS still lists it as a RADIUS server, it gets wrong answers. On 2026-11-27 a running follower was removed directly through the API of the leader, which passes the checks of `cluster remove` by: with three nodes on one machine, in one run it went on answering for about 12 seconds after the removal and gave no answers from 14 seconds on; in all three runs it ended with exit code 1 about 28 to 30 seconds after the removal. It does not end while it cannot reach the cluster.
- **Restarting with the data emptied and the configuration still listing itself joins the cluster again automatically.** So for a host you retire, disable the service, and do not only delete the data.
- **Management commands can be slow.** If the first node in the configuration is removed and in the middle of ending, commands such as `user` wait for it: on 2026-11-27 one `user add` run 8 seconds after the removal succeeded after 19 seconds; once the node had ended they were back to normal. Stopping the node first avoids this.

### Other notes

- **Keep an odd number of nodes (3 or 5).** 3 can lose 1, 5 can lose 2; 4 can lose only 1, the same as 3.
- **It is the same when the node to remove is the leader:** stop it first, the remaining nodes elect a new leader (writes during the election time out, see [Across data centers](#across-data-centers)), and then remove it.
- **When a stopped host is first in the configuration,** `cluster members` and `cluster remove` first wait for it to time out before they try the next host. Each host is waited for at most 2 seconds in the program (`PING_WAIT`); the measurement on 2026-11-18 once saw about 5 seconds more, which was a version from before this value was changed. Delete the stopped node from the configuration, or move it to the end, and it does not happen.
- **When the majority is lost, the leader that `members` sees may be out of date.** With three nodes, when only the former leader is left, it still reports itself as the leader (exit code 0), but it has no majority and writes do not succeed; when the remaining node is a follower, `members` exits with exit code 3 after about 30 seconds. To judge whether you can write, look at `Has a leader` in `rradiusd status`, or just try one write (measured on 2026-11-18).
- **Stopping one node does not require removing it.** After an upgrade, a reboot or a hardware change, it comes back with the same `node_id`; see [Restarting nodes and rebuilding data](#restarting-nodes-and-rebuilding-data). Removal is for nodes that will not come back.
- **What was tested:** 2026-11-13 and 2026-11-18, Docker on one host, three to five nodes: removing the leader finished in about 5 seconds, during which logins to the remaining three every 0.05 seconds, about 1,480 of them, all succeeded, with a longest gap of 0.1 seconds (logins read the local copy and do not need a leader); adding a fourth node made it a voter in 1 to 3 seconds and it answered logins after about 4 seconds; adding and removing during writes, 394 writes did not fail. It was not tested on several physical hosts or through WireGuard, and the behavior of a real NAS during an election was not tested.

## Check the settings of each node

The settings of the nodes should differ only in the local values. `config diff` compares two configurations (including defaults) and lists the differences; the two keys that are meant to differ, `cluster.node_id` and `cluster.listen`, are listed at the end under `node-specific:`:

```console
$ rradiusd config diff node1.toml node2.toml
cluster.data_dir: "data1" | "data2"
cluster.stop_without_leader_ms: 10000 | (absent)
metrics.port: 27041 | 27042
radius.accounting_spool.dir: "spool1" | "spool2"
radius.acct_port: 27031 | 27032
radius.auth_port: 27021 | 27022
tacacs.port: 27051 | 27052
node-specific:
  cluster.node_id: 1 | 2
```

This is the result of the one-machine practice, where ports and directories all differ. With several hosts there are far fewer differences, for example two nodes of the ha-cluster scenario:

```console
$ rradiusd config diff node1.toml node2.toml
metrics.bind: "10.99.0.1" | "10.99.0.2"
node-specific:
  cluster.listen: "10.99.0.1" | "10.99.0.2"
  cluster.node_id: 1 | 2
```

The exit code is 1 when there are differences, and 0 when they are exactly the same (differing only in `node-specific`). After you change the configuration and before you push it to the nodes, use it to confirm that you did not change only one machine. Secrets are shown only as `differs (hidden)`, and their content is never printed.

## See the state of the whole cluster

`status --cluster` asks the cluster for the member list and reads the metrics port of every member at the same time, one line per node:

```console
$ rradiusd status --cluster --config node1.toml
rradius cluster status over 1.0 s, members from the cluster

node   role    ready  auth/s  acct/s  discarded/s  in progress  address
1      leader  ok     0.0     0.0     0.0          0            127.0.0.1:27041
2      leader  ok     0.0     0.0     0.0          0            127.0.0.1:27041
3      leader  ok     0.0     0.0     0.0          0            127.0.0.1:27041
total          3/3    0.0     0.0     0.0          0
```

This output is wrong, and wrong as expected: all three lines are node 1. The reason is that `status --cluster` reads the other members with the host name of each member's `api` address, plus **this node's own metrics port**. All three nodes are on `127.0.0.1` and their metrics ports differ, so all three lines read `127.0.0.1:27041`.

For it to be correct in production, every node has to:

- use the same `[metrics] port`;
- set `[metrics] bind` to an address the other nodes can reach, which is the host of the `api` address.

A member that cannot be read is shown as `unreachable`, with the reason and the hint `every node's [metrics] bind has to be an address this host can reach, on the same port as here` below it.

> **Not verified on real hardware:** the correct output of `status --cluster` on several hosts. This chapter ran it on a single machine only, with the result above.

`--cluster` also works with `--watch` and `--json`. The fields of `status` are described in [chapter 17](17-monitoring.md).

## Several hosts: the ha-cluster scenario

`rradius-server/examples/scenarios/ha-cluster/` is an example for three hosts: `node1.toml`, `node2.toml` and `node3.toml`, which differ only in `node_id`, `listen` and `[metrics] bind`.

| Setting | Value | Why |
| --- | --- | --- |
| `cluster.listen` | each node's WireGuard address `10.99.0.N` | The Raft and API ports listen on the WireGuard interface only |
| `cluster.nodes` | `10.99.0.1` to `10.99.0.3`, ports 8100, 8200 | Every host uses the same ports |
| `cluster.migrations_wait_ms` | `120000` | The time to wait for the other nodes when all three restart together |
| `lua.query_timeout_ms` | `2000` | The total time a request waits for the database; the first write after the majority is lost still waits 30 seconds, and later ones fail at once and are written to the spool |
| `radius.bind` | `0.0.0.0` | The NAS connects in from the physical network |
| `radius.clients` | `10.80.0.0/16`, the shared secret the same on all three | Every NAS configures all three nodes as RADIUS servers |
| `radius.status_server` | `true` | NAS and load balancers use Status-Server to tell that a node is alive |
| `[radius.accounting_spool]` | `spool`, at most 1 GiB | When the majority is lost, accounting is written locally first |
| `metrics.bind` | each node's WireGuard address, port 9812 | Monitoring scrapes from the internal network; the three use the same port so that `status --cluster` can read them |
| `log.format` | `json` | Easy to collect centrally |

Copy the scenario to a working directory as described in [chapter 03](03-quick-start.md) and change `dictionary` to the absolute path of the dictionary, and all three pass `check` (with warnings about the example's public secrets and file permissions):

```console
$ for n in 1 2 3; do rradiusd check --config node$n.toml; done 2>&1 | grep -v WARN
configuration ok
configuration ok
configuration ok
```

`check` does not open any port, so it can also run on a machine that has no `10.99.0.x` address. To really run it, you need three hosts and WireGuard.

> **Not verified on real hardware:** how the ha-cluster scenario actually runs on three hosts connected through WireGuard. The behavior in this chapter was all verified on one machine with configurations whose ports were changed.

When you deploy, remember:

- `raft.secret`, `api.secret` and `nas.secret` have to be the same on the three hosts. Replace them with values you generate yourself with `openssl rand -hex 32`, with permissions 0600.
- `migrations/2_demo_users.sql` holds the demo accounts, and in production you delete it.
- The NAS configures all three nodes as servers. When `stop_without_leader_ms` is set, a node that is cut off stops answering, and the NAS asks another one.
- RADIUS uses the node's physical address, not the WireGuard address; requests also have to come from a network in `radius.clients`, or there is no answer. When the host you manage from is not in that network, temporarily add a `[[radius.clients]]` with only its address, test after a reload, and remove it when you are done.
- With EAP, one login has to finish on the same node: configure the NAS for primary and backup, or let the load balancer pin the node by source ([chapter 11](11-eap.md)).
- When the nodes do not start at the same time: on the first start no node serves until all three are running, because node 1 creates the cluster only once the other two answer ([When node 1 waits](#when-node-1-waits)), and nodes 2 and 3 then join it; with `migrations_dir`, as in this example, a node that has not joined within `migrations_wait_ms` exits with code 1 (`did not join within …`), and without it the node waits until it joins; after a restart, a node waits for a leader for at most `migrations_wait_ms` and then does what [Restart the whole cluster](#restart-the-whole-cluster) describes. The wait is not a failure; a node can serve only after its log shows `RADIUS Server listening`.
- Set every node to restart automatically (`Restart=on-failure` of systemd, `restart: unless-stopped` of Docker), because in some situations the server exits on its own with exit code 1 so that the process manager restarts it (see [After a forced stop](#after-a-forced-stop)).

## Deploy with Docker Compose

`compose.cluster.yaml` in the root of the source starts three nodes on one Docker host, and the configuration files are `docker/config/cluster/node1.toml` to `node3.toml`:

| Item | Content |
| --- | --- |
| Services | `node1`, `node2`, `node3`, image `rradius-server`, built from the `Dockerfile` of the source |
| RADIUS ports | Host ports 1812/1813, 2812/2813, 3812/3813 map to 1812/1813 (UDP) of each container |
| Configuration | `./docker/config` mounted read-only at `/etc/rradius` |
| Data | One volume per node: `data1`, `data2`, `data3`, mounted at `/var/lib/rradius` |
| Secrets | Compose secrets: `docker/secrets/raft.secret`, `api.secret`, `radius.secret`, mounted at `/run/secrets/` |
| Security | Read-only root file system, `cap_drop: ALL`, `no-new-privileges` |
| Stop | `stop_grace_period: 30s`, `restart: unless-stopped` |

The main points of the node configuration:

- The three files differ only in `node_id`.
- `listen = "0.0.0.0"`: the container has only one address, and the other containers have to be able to reach it.
- `nodes` uses the service names: `node1:8100`, `node1:8200`, and so on.
- `migrations_dir = "../migrations"`, that is `docker/config/migrations/`.
- `[metrics]` listens on `127.0.0.1:9812` inside the container, for `status` inside the container.

The steps to start (the comment at the top of `compose.cluster.yaml`). If the single node of [chapter 02](02-install.md) is running from the same directory, stop it first with `docker compose down`: the two use the same project name and host ports. `up` runs in the foreground, so run the later commands in another terminal, or add `-d`.

```console
$ sh docker/init-secrets.sh
$ docker compose -f compose.cluster.yaml up --build
```

`init-secrets.sh` generates three random secret files, and does not overwrite ones that exist. To look at the state inside a container:

```console
$ docker compose -f compose.cluster.yaml exec node1 rradiusd status --config /etc/rradius/cluster/node1.toml
$ docker compose -f compose.cluster.yaml exec node1 rradiusd cluster members --config /etc/rradius/cluster/node1.toml
```

For the exercise of stopping the leader, find it with `cluster members`, then run `docker compose -f compose.cluster.yaml stop <that node>` (it sends SIGTERM and waits at most 30 seconds), and look from another node, with its service name and configuration file.

What this book verified, on 2026-11-27 on one Docker host (OrbStack, arm64, with the ports changed to listen on `127.0.0.1`): the image builds, the three containers start, both `exec` commands work, and stopping the leader took about 15 seconds; another node took over, and accounting sent to node 2 went unanswered for about 6 seconds.

> **Not verified on real hardware:** a cluster whose nodes are on several hosts. An x86-64 image was not built in this run.

> **Note:** The three containers are on the same Docker host, and when this host stops, the whole cluster stops. It suits tests and small setups; to survive a host failure, spread the nodes over different hosts, as the ha-cluster scenario does. `[metrics]` listens only on the address inside the container, so `status --cluster` inside a container cannot read the other nodes.

## Deploy on Kubernetes

`deploy/kubernetes/` runs three nodes with a StatefulSet, applied with `kubectl apply -k deploy/kubernetes`. The files:

| File | Content |
| --- | --- |
| `kustomization.yaml` | Applies `statefulset.yaml`, `services.yaml` and `pdb.yaml`; generates `config/` into the ConfigMap `rradius-config` |
| `statefulset.yaml` | 3 pods `rradius-0` to `rradius-2`, `podManagementPolicy: Parallel`, one 1Gi PVC per pod |
| `services.yaml` | The headless Service `rradius-cluster` (nodes find each other); the LoadBalancer Service `rradius` (for the NAS) |
| `pdb.yaml` | PodDisruptionBudget `maxUnavailable: 1` |
| `config/rradius-0.toml` to `rradius-2.toml` | One per pod, differing only in `node_id`: pod `rradius-N` is node N+1 |
| `config/auth.lua`, `config/1_schema.sql` | The script and the migration |
| `secret.example.yaml` | What the Secret `rradius-secrets` looks like; it is not applied |
| `networkpolicy.yaml` | Not applied by default: the RADIUS ports are open to the outside, the cluster ports only to the other nodes, the metrics port only to the `monitoring` namespace |
| `hostnetwork-patch.yaml` | Not applied by default: when there is no load balancer that keeps the source address, lets the pod use the node's network |

### Build and load the image

`statefulset.yaml` uses `rradius-server:latest` with `imagePullPolicy: IfNotPresent`. Build it in the root of the source:

```console
$ docker build -t rradius-server:latest .
```

- **The Kubernetes built into Docker Desktop and OrbStack:** uses the local image directly, and you need not do anything else.
- **kind:** `kind load docker-image rradius-server:latest` loads the image into the kind nodes.
- **Production:** push it to your own registry, then enable the commented-out `images` in `kustomization.yaml` and change it to that name and version:

  ```yaml
  images:
    - name: rradius-server
      newName: registry.example.com/rradius-server
      newTag: "1.0"
  ```

### Create the Secret first

```console
$ kubectl create secret generic rradius-secrets \
    --from-literal=raft="$(openssl rand -hex 32)" \
    --from-literal=api="$(openssl rand -hex 32)" \
    --from-literal=radius="<your NAS shared secret>"
```

The Secret is mounted at `/run/secrets/rradius`, and the configuration reads it with paths such as `secret_raft_file = "/run/secrets/rradius/raft"`. The three pods use the same Secret, so the cluster secrets are the same by construction. Replace `radius` with the shared secret your NAS actually uses. The example directory holds no real secrets, and `secret.example.yaml` only explains the format.

### Apply and wait

First change `[[radius.clients]] address` in `config/rradius-0.toml` to `rradius-2.toml` from the documentation network `192.0.2.0/24` to the network of your NAS (the same in all three; for the reason see [Change the NAS addresses](#change-the-nas-addresses)), then:

```console
$ kubectl apply -k deploy/kubernetes
$ kubectl rollout status statefulset/rradius
```

The three pods start together and begin to serve after a leader is elected; `rollout status` ends when all three are ready. Then add accounts as described in [Run commands in a pod](#run-commands-in-a-pod).

### Why the example is designed this way

Every point corresponds to a behavior described earlier in this chapter:

- **`podManagementPolicy: Parallel`:** the three pods start together. With `stop_without_leader_ms` set, a restarted node that finds no leader within `migrations_wait_ms` exits with code 1 and is started again, so if the pods of a stopped cluster started one by one, the first would never become ready and the other two would not be started. A first start needs them together too: node 1 creates the cluster only once the other two pods answer.
- **`publishNotReadyAddresses: true` (headless Service):** pods that are not ready yet keep their DNS names. Otherwise nodes that are waiting for a leader cannot find each other, and no leader can be elected.
- **`nodes` uses DNS names:** in the form `rradius-0.rradius-cluster:8100`; `listen = "0.0.0.0"`, because the other pods connect through the pod address.
- **Probes:** the metrics port opens only after the node has caught up with the cluster, so only the `startupProbe` runs first (`/livez`, every 5 seconds, at most 60 times). After that the `livenessProbe` looks at `/livez` and the `readinessProbe` at `/readyz`.
- **`stop_without_leader_ms = 10000`:** a pod that is cut off stops answering and `/readyz` becomes 503, so the Service does not send the NAS traffic to it.
- **`ready_delay_ms` defaults to 5000:** after SIGTERM `/readyz` returns 503 first, the Service moves the traffic away first, and only then does the pod stop accepting requests. `terminationGracePeriodSeconds: 30` covers the whole shutdown.
- **`externalTrafficPolicy: Local`:** rradius tells NAS apart by source address, and the source address has to reach the pod unchanged.
- **PodDisruptionBudget `maxUnavailable: 1`:** for voluntary disruptions such as draining a Kubernetes node, only one pod is moved at a time and two of three stay, which keeps the majority.
- **`[metrics] bind = "0.0.0.0"`:** the kubelet probes from the pod address. The three pods use the same port 9812, so by the program's rules `status --cluster` should be able to read the other members; after `networkpolicy.yaml` is applied, 9812 is open only to the `monitoring` namespace, and the pods can no longer read each other.

### Change the NAS addresses

`[[radius.clients]]` in `config/rradius-*.toml` is the documentation network `192.0.2.0/24`, and you have to change it to the actual addresses of your NAS. Do not write a network that is too large: if the network also covers the addresses of the Kubernetes nodes, packets whose source address was changed by the load balancer are also accepted, and all NAS are treated as the same client.

### Verify that the source address is not rewritten

When packets pass through a Kubernetes Service, the source address is often changed to the address of a node or a gateway, with one of two results:

- The rewritten address is not in the network of any client: rradius does not recognize it and **does not answer**, counts it in `rradius_radius_discarded_total{reason="unknown_client"}`, and the log (rate limited) says `RADIUS packet from unknown client <address> discarded`.
- The rewritten address happens to fall in the network of some client, for example when a client uses a wide network such as `10.0.0.0/8`: the request is accepted anyway and **everything looks normal**, but all NAS are treated as the same client. In this case no counter increases, and you can only see it in the authentication records.

So always verify after applying: send one request from a NAS and look in the authentication record (`log.auth`, on by default) to see whether `source=` is the address of that NAS. The request may go to any pod, so look at all three:

```console
$ kubectl logs rradius-0 | grep rradius_auth | tail -3
… INFO rradius_auth: authentication protocol="radius" client="192.0.2.0/24" source=192.0.2.10 user="alice" …
```

If `source=` is the address of a node, a gateway or the pod network, it has been rewritten. When there are no records at all, look at whether `unknown_client` increases:

```console
$ kubectl port-forward pod/rradius-0 9812:9812 &
$ curl -s localhost:9812/metrics | grep 'reason="unknown_client"'
```

- Most cloud load balancers support `externalTrafficPolicy: Local`, but some UDP load balancers rewrite the source address themselves, so read the documentation of the cloud.
- For packets sent from a Mac to a LoadBalancer in the Kubernetes built into OrbStack, `source=` is the gateway of the pod network, so you cannot verify on OrbStack whether the source address is kept. For trying it out locally, you can temporarily change the client to that address.

> **Not verified on real hardware:** the behavior of the two kinds of load balancers above is written from the comments in `deploy/kubernetes/` of the source and the general behavior of Kubernetes. This book did not measure it on a real cluster.

### Without a suitable load balancer: hostNetwork

`hostnetwork-patch.yaml` lets the pods use the node's network directly. The NAS packets go to the node's address, and the source address is not rewritten. Enable `patches` in `kustomization.yaml`:

```yaml
patches:
  - path: hostnetwork-patch.yaml
```

- The patch sets `podAntiAffinity`, so one node can hold only one rradius pod, and you need at least three Kubernetes nodes.
- The NAS is configured with the addresses of the three nodes, as primary and backup or in rotation, either way.
- When a pod moves to another node its address changes, and the NAS side has to be changed to follow, so it suits fixed nodes, for example pinned with `nodeSelector`.
- **The metrics port and the cluster ports open on every address of the node, including the public ones.** The example has `[metrics] bind = "0.0.0.0"`, so 9812 is then open to the outside without authentication and without TLS; the cluster ports 8100 and 8200 are the same. NetworkPolicy has no control over pods that use hostNetwork, so `networkpolicy.yaml` cannot stop this. Use the node's firewall to let only the internal network reach 9812, 8100 and 8200, or change `[metrics] bind` to the node's internal address (the addresses of the three nodes differ, so the configuration files no longer differ only in `node_id`).

### TACACS+

When the configuration adds `[tacacs]`, add a TCP container port to `statefulset.yaml`, and enable the commented-out `tacacs` part of `services.yaml`. TCP through `externalTrafficPolicy: Local` also keeps the source address, and the verification method is the same as above.

> **Note:** The container runs as uid 10001 with all capabilities dropped, and whether it can bind a low port such as 49 depends on how the container runtime handles low ports; this book did not verify it. The safe way is to set the `port` of `[tacacs]` to 1024 or above (for example `49490`), keep the `port` of the Service at 49, and let `targetPort` point at that port.

### Probes and latency

The metrics port (`[metrics] bind = "0.0.0.0"`, because the kubelet probes the pod address from the node) has two paths; for the details see [chapter 17](17-monitoring.md):

| Path | 200 | 503 | Use in the example |
| --- | --- | --- | --- |
| `/livez` | The process responds | Never | `startupProbe`, `livenessProbe` |
| `/readyz` | Can answer authentication | Stopping (SIGTERM received); `stop_without_leader_ms` is set and there has been no leader for longer than that; or an external database with `required_for_ready` cannot be reached (content `database <name> unreachable`, [chapter 09](09-external-databases.md)) | `readinessProbe` |

- **A pod that is not ready is taken out of the `rradius` Service,** and the NAS requests go to the other pods. The example sets `stop_without_leader_ms = 10000`: when one of three is left, the remaining one stops answering after about 10 seconds and becomes not ready; `/livez` stays 200 and the container is not restarted.
- **When a pod is deleted or updated,** the kubelet sends SIGTERM and `/readyz` becomes 503 at once. The pod keeps answering as usual for 5 seconds (the default `ready_delay_ms` of a three-node cluster), and the `readinessProbe` runs every 5 seconds and takes it out after 1 failure, so the Service stops sending requests first, and only then does the pod stop accepting, wait for the requests in progress and exit. The whole process takes about 15 seconds, within `terminationGracePeriodSeconds: 30`.
- **Without `stop_without_leader_ms`, `/readyz` is always 200 except while stopping:** a node without a leader still answers read-only logins from the local copy, and taking it out of the Service would make all logins fail instead.
- **The `startupProbe` waits at most 5 minutes** (every 5 seconds, at most 60 times): the metrics port opens only after the node has caught up with the cluster, and that needs a leader first. Without the `startupProbe`, the `livenessProbe` restarts the container before a leader is elected.

### Run commands in a pod

```console
$ kubectl exec -i rradius-0 -- rradiusd user add --config /etc/rradius/rradius-0.toml alice
$ kubectl exec rradius-0 -- rradiusd cluster members --config /etc/rradius/rradius-0.toml
$ kubectl exec rradius-0 -- rradiusd status --cluster --config /etc/rradius/rradius-0.toml
```

`user add` reads the password from standard input, so it needs `-i`. For how to apply a changed configuration (the ConfigMap changes its name, pods restart one by one), and what to change when you add a migration, see [chapter 18](18-operations.md).

What this book verified: after making a copy in a working directory, `kubectl kustomize` can generate the ConfigMap, 2 Services, the PodDisruptionBudget and the StatefulSet; with the dictionary and secret paths of the three `rradius-*.toml` replaced by local paths, `check` passes for all.

> **Not verified on real hardware:** actually applying to a Kubernetes cluster, the start of the pods, deleting the leader pod, and the effect of `hostnetwork-patch.yaml` and `networkpolicy.yaml`. The `kubectl exec` commands above were not actually run.

## Across data centers

What happens to the cluster when the three nodes are in different data centers, with a few to a few dozen milliseconds between the nodes? The numbers in this section were simulated on one host, and **they are for judging "is it worth designing in this direction", not a promise for a real wide-area network.** Measurements without a date were not measured again for this manual; those with a date were measured on that day. The raw numbers and the measurement scripts are not in this repository.

### Test conditions

- **Environment:** one host with an Apple M4 Max (14 cores), Docker on OrbStack, one container per node; the image is the core `rradius-server` plus `iproute2`. The node configuration is `docker/config/auth.lua` and the `sql` handler (logins read the local copy, and every accounting packet makes one `INSERT`).
- **How the delay was added:** `tc netem` adds a one-way delay (half of the round-trip time, RTT) on the egress of every node toward the other two nodes, and the path from the RADIUS client to the nodes gets none. The RTT was calibrated with `ping` between the nodes, and the tables below always use the measured value.
- **Limits of the simulation:** only a fixed delay, with no jitter, no packet loss, and the same in both directions; the nodes are on the same host, and CPU and memory are not the bottleneck.
- **Load:** `rradius-bench`, fixed concurrency 64, 2 seconds of warm-up and 10 seconds of measurement, 3 runs for each RTT point. The read load is `pap` logins (reading the local copy); the write load is `acct-start` (every packet makes one `INSERT INTO accounting`, which has to go through Raft). The client hits the leader of that time.

### Steady state: reads are not affected, writes are roughly inversely proportional to RTT

| RTT between nodes (measured) | Login (read) throughput | Login p50/p99 | Accounting Start (write) throughput | Write p50/p99 |
| --- | --- | --- | --- | --- |
| 0.2 ms | 86,100/s | 0.7/1.4 ms | 42,700/s | 1.3/3.3 ms |
| 5.1 ms | 85,800/s | 0.7/1.5 ms | 3,940/s | 14.6/25.6 ms |
| 19.2 ms | 85,200/s | 0.7/1.5 ms | 826/s | 78/119 ms |
| 50.5 ms | 85,300/s | 0.7/1.6 ms | 318/s | 198/309 ms |

- **Reads do not change with RTT:** they read the local copy and do not use the network.
- **At fixed concurrency, write throughput is roughly inversely proportional to RTT:** throughput times RTT is about 20,000, 15,900 and 16,100 at 5, 19 and 50 ms; the latency of one write is about 2.9, 4.1 and 3.9 times the RTT. That is, at concurrency 64, the write limit is about `64 ÷ (4 × RTT)`. For more you have to add concurrency, which was not tested.
- These write throughputs were measured against the leader; the throughput of sending writes to a follower (which has to forward them to the leader first) was not measured.

### Kill the leader

The leader was killed with `docker kill` (SIGKILL), and the client sent a writing accounting Start to a surviving node every 50 ms. The election time is from the kill until `rradius_cluster_is_leader` of one of the surviving nodes becomes 1; the write unavailable time is from the kill until the first write sent after it gets a response.

The table below is the measurement from **before the reconnect interval was patched**, 5 runs per point:

| RTT between nodes (measured) | Election time | Write unavailable time |
| --- | --- | --- |
| 0.15 ms | 5.7–6.9 s | 6.7–15.2 s (about 15 s in 4 of 5 runs) |
| 5.2 ms | 5.0–6.4 s | 10.1–15.2 s |
| 20.0 ms | 6.0–6.9 s | 7.2–17.6 s |
| 50.1 ms | 6.4–6.8 s | 7.4–17.8 s |

- **The election time is about 5 to 7 seconds, and within the measured range it does not depend on RTT.**
- **Writes recover a little later than the election.** The reason: the client of the cluster database (hiqlite 0.16) caches "who the current leader is" and updates it only in the reconnect loop; the original reconnect interval was 1 second for the first 5 attempts and 5 seconds after that, so when the election finished after the 5th second, it waited another 5 seconds. Now rradius loads a modified hiqlite (`vendor/hiqlite`) that changes the 5 seconds to 1 second (`RECONNECT_SLOW_INTERVAL`; the change is recorded in `MODIFICATIONS` in that directory), so it reconnects every second; during this time the node logs one line per second, `Could not connect Client API WebSocket to node1` (the old leader). This interval is an internal constant of hiqlite and there is no setting to adjust it.
- **Measured on 2026-11-23 (three nodes, Docker on one host, RTT 0, leader stopped):** the election took 6.3 to 6.7 seconds, and the first successful write came at 7.2 to 7.5 seconds, that is about 0.5 to 1 second after the election; under the same conditions the version from before the change took 15.1 to 15.5 seconds (3 runs). So **the write interruption measured on one machine is about 7 to 8 seconds** (the election about 6 to 7 seconds, plus the client reconnecting). Each was measured only 3 times, only on single-host Docker with RTT about 0; wide-area networks and longer elections were not measured.
- **When you plan for several data centers, you can take about 20 seconds as the margin for the write interruption.** This number comes from the 15 to 18 seconds before the change in the table above, was not measured on the version after the change or on a wide-area network, and is a conservative estimate, not an expected value.
- Logins (reads) are not affected and still read the local copy.

### Partition

`netem` dropped all packets between the nodes (in both directions), and the path of the RADIUS client was not affected. Each case was run once, with samples about 5, 12, 22 and 32 seconds after the partition; each time one `pap` and one accounting Start (timeout 2 seconds) were sent to every node. The results for RTT 0.2 and 20 ms were the same unless noted.

| Case | The isolated node | The majority side | Recovery (after the network is reconnected) |
| --- | --- | --- | --- |
| One follower isolated | Logins are answered as usual (reading the old copy); accounting gets no answer; within about 10 seconds `rradius_cluster_leader` becomes 0 and `seconds_without_leader` keeps rising; `applied_index` stops | Only the leader answers accounting (at 0.2 ms both nodes answered; at 20 ms the other node did not answer in 4 samples); logins are normal | Caught up in 24.8 s (0.2 ms) and 29.3 s (20 ms) |
| Leader isolated | Logins are answered as usual; accounting gets no answer; `rradius_cluster_is_leader` is still 1, but `rradius_cluster_leader` becomes 0 after about 10 seconds; `applied_index` stops | A new leader is elected (visible in the sample at the 12th second at 0.2 ms and the 22nd second at 20 ms); **accounting still got no answer in the sample at 32 seconds** (including from the new leader); logins are normal | Caught up in 1.3 s (0.2 ms) and 1.6 s (20 ms); the old leader becomes a follower |

- The isolated node's copy stays at the state of the moment of the partition and still answers logins. If you cannot accept that, set `stop_without_leader_ms` (it was not set this time, and the behavior with it set was not tested).
- Accounting writes on the majority side took very long to recover after the leader changed; this time they had not recovered 32 seconds after the partition. It may have the same cause as killing the leader above, but in a partition the old leader was still alive, and this was not verified.

**Recovery time after isolating a follower (Linux, no need to tune `tcp_retries2`):** both the connection between Raft nodes (port 8100) and the API connection have `TCP_USER_TIMEOUT` set to 10 seconds: when data was sent and no acknowledgment came for 10 seconds, the kernel closes the connection, and then it reconnects. Without this setting, a connection that got no response during the isolation doubles its retransmit interval each time, and after reconnection it has to wait for the next retransmit before it can send, so the recovery time grows with how long the isolation lasted. On 2026-11-25 the same three-node Docker environment (RTT 0) was used to measure "from reconnection until this node knows the leader, has caught up with the data and a write through it gets a response", the median of 3 runs each:

| Isolation time | Without this setting (2026-11-24, 1 run each) | With this setting (3 runs each) |
| --- | --- | --- |
| 12 seconds | 15.3 s | 6.4 s (5.1 to 6.4) |
| 30 seconds | 29.4 s | 6.4 s (5.1 to 8.2) |
| 60 seconds | 47.3 s | 4.0 s (3.7 to 4.5) |

However long the isolation, recovery is within 10 seconds. The data to catch up was only 3 to 12 entries, so the synchronization is not slow. This setting takes effect only on Linux, and other systems keep the previous behavior.

With 100 ms RTT between the nodes and 5% packet loss on every packet, without isolating, writing continuously for 5 minutes (accounting Start, concurrency 16), the difference with and without this setting: throughput 30.2/s against 29.7/s, the leader did not change, and there was no `Raft WebSocket stream ended` log, so this value did not cause a connection to be wrongly judged as down. **Worse networks (loss lasting more than 10 seconds, a long total outage) were not measured**; in those cases it is expected that the connection is closed, and it reconnects after recovery.

*History (2026-11-20, the version without `TCP_USER_TIMEOUT`, 1 run each):* with isolation of 4, 12, 30 and 60 seconds, recovery took 4.9, 15.2, 25.5 and 49.8 seconds; after lowering `net.ipv4.tcp_retries2` of the nodes from 15 to 5, recovery after 12, 30 and 60 seconds of isolation became 5.1, 7.6 and 5.1 seconds. The current version does not need this kernel parameter to be tuned.

### Rolling upgrade

First measure the highest write throughput at that RTT, take half of it, and divide it equally among the three nodes, with one `rradius-bench` per node (`acct-start`, fixed rate, timeout 1 second, no retransmit); in addition every node has a write probe every 50 ms. One node at a time (the two followers first, the leader last), run `docker stop -t 30` and `docker start`, and wait until the log shows `cluster node N has caught up with the cluster`.

| RTT between nodes (measured) | Load (total) | Stop to caught up, per node | The stop itself | Requests that timed out | Longest gap on the other nodes during the upgrade |
| --- | --- | --- | --- | --- | --- |
| 0.2 ms | 21,600/s | 15.9, 17.6, 21.3 s (leader last) | 14.6 s | 5,500–6,500 per node, about 0.2–0.3% | 0.1–1.4 s |
| 19.8 ms | 350/s | 17.1, 17.1, 21.5 s (leader last) | 14.8 s | 1,500–2,600 per node, about 3.6–6.2% | 0.1–6.9 s (6.9 when the leader was stopped) |

- **Upgrading one node at a time, the other two keep answering.** Most of the requests that timed out were sent to the node being stopped (the client did not switch to another node).
- **The executables of the core edition and the enterprise edition have the same name (`rradiusd`),** so when you change versions you only replace that file, and `rradiusd --version` shows which edition and version number each node runs. A cluster with nodes of both editions at the same time was not tested for this manual and is not guaranteed to work; if you need to do it, try it first on a test cluster. For two builds of the core edition in one cluster, see [chapter 18, "Upgrading the program"](18-operations.md#upgrading-the-program).
- **Upgrading one node takes about 16 to 21 seconds** (about 15 seconds to stop, plus the start and catching up), and the leader takes the longest; when the leader is stopped, the write gap on the other nodes is the longest.
- Not tested in this measurement: old and new versions mixed (the same image was restarted; two builds were measured separately, see [chapter 18](18-operations.md#upgrading-the-program)), and the catch-up time with a large amount of data.

### Three topologies

| | A single cluster across three data centers | A main cluster plus a cold standby | An independent cluster in each data center |
| --- | --- | --- | --- |
| How | The three nodes are spread over three data centers, in one cluster | One data center runs the cluster; the other normally has no nodes, and uses `[cluster.backup]` of the enterprise edition ([chapter 01](01-overview.md)) to save a copy regularly, and when something happens restores the copy as a new cluster | Each data center has a three-node cluster, unrelated to the others |
| Login (read) | Not affected (0.7 ms measured) | Same as the main cluster | Same as a single data center |
| Write (IP allocation, verification codes, accounting) | About 3 to 4 RTT each; at concurrency 64, about 830/s at 20 ms and about 320/s at 50 ms | Same as a single data center | Same as a single data center |
| One data center loses contact entirely | The other two data centers have a majority; the election takes about 5 to 7 seconds, and you have to allow a margin for the write interruption (see above) | When the main data center is lost you depend on a restore, and writes after the last copy are lost; the switch needs a person | The other data centers are not affected; the users of the lost data center have to be served elsewhere |
| Cost | Writes are slow and the gaps during upgrades grow; you have to check yourself how much worse the real network is than the simulation | The standby data center has no hot data normally; how long a restore takes was not measured | Accounts, settings and migrations have to be done once in each cluster, and you manage the consistency yourself |

**A hot standby cannot be done at present.** Keeping a node in the standby data center that only replicates and does not vote (a learner) needs a new node to stay a learner; now a new node is a learner first and is promoted to voter automatically when it has finished copying, and there is no setting that makes it stay a learner.

### What was not tested

Jitter and packet loss, different delays in the two directions, physical hosts and environments through WireGuard, the retransmit behavior of a real NAS during an election, the throughput of sending writes to a follower, higher concurrency, five nodes, a partition with `stop_without_leader_ms` set, the behavior of the accounting spool under high latency, partitions longer than 30 seconds, and the time a backup restore takes.

## Cluster metrics

The `/metrics` of every node has these, and the values are this node's own view:

| Metric | Type | Meaning |
| --- | --- | --- |
| `rradius_cluster_leader` | gauge | 1 means there is a leader that can be written to |
| `rradius_cluster_is_leader` | gauge | 1 means this node is the leader |
| `rradius_cluster_applied_index` | gauge | The number of the Raft log entry the local copy has applied |
| `rradius_cluster_seconds_without_leader` | gauge | How many seconds there has been no leader; 0 when there is a leader |
| `rradius_cluster_caught_up` | gauge | 1 means it has caught up with the cluster since startup; 0 means it is serving from a copy that may be behind |
| `rradius_db_batched_writes_total` | counter | Script writes committed in one transaction together with other writes |
| `rradius_db_split_batches_total` | counter | How many times a combined transaction failed and was split to find the statement that failed |
| `rradius_radius_discarded_total{reason="no_leader"}` | counter | Requests not answered because of `stop_without_leader_ms` |
| `rradius_radius_discarded_total{reason="database"}` | counter | Requests not answered because the database could not be used (for example a write failed) |

Suggested directions for alerts:

- Across the whole cluster, the sum of `rradius_cluster_is_leader` should be 1. 0 means there is no leader.
- `rradius_cluster_seconds_without_leader` stays above 0.
- The `rradius_cluster_applied_index` of a node stays behind the other nodes.
- `rradius_cluster_caught_up` is 0 for too long.
- The bytes of the accounting spool (`rradius_acct_spool_bytes`) keep growing, which means writes to the database are failing.

The `deploy/monitoring/` directory of the source has ready-made rules; see [chapter 17](17-monitoring.md). The full description of every metric is in [Appendix C](92-metrics-reference.md).

## Common mistakes

**Symptom:** `check` prints `cluster.nodes: node 1 is required`.
**Cause:** There is no node with id 1 in `[[cluster.nodes]]`.
**Fix:** Number the node ids from 1.

**Symptom:** `check` prints `cluster.node_id 4 is not in cluster.nodes`.
**Cause:** The `node_id` of this node is not listed in its own configuration.
**Fix:** Add its own entry to `[[cluster.nodes]]`.

**Symptom:** `check` prints `cluster.secret_raft must be at least 16 characters`, or `exactly one of cluster.secret_api or cluster.secret_api_file must be set`.
**Cause:** The cluster secret is too short, or both ways of writing it were given, or neither.
**Fix:** Generate it with `openssl rand -hex 32 > raft.secret` and write only `secret_raft_file` in the configuration.

**Symptom:** `check` or startup warns `cluster.listen is not set, so the cluster's Raft and API ports are on all interfaces, …`.
**Cause:** There is `[[cluster.nodes]]` but no `listen`.
**Fix:** Set it to the node's internal or WireGuard address; if you really want to listen on all addresses, write `listen = "0.0.0.0"`.

**Symptom:** The logs of the second and third node have many `ERROR hiqlite::init: … Raft is not initialized` or `Connection refused` at the beginning.
**Cause:** Node 1 has not created the cluster yet, which it does once every other node answers, or the other nodes have not started.
**Fix:** Wait. When `has caught up with the cluster` appears, it is fine. If it never appears, confirm that all three nodes have started and that the `raft` and `api` addresses can reach each other; the log of node 1 names the nodes it is waiting for.

**Symptom:** Node 1 serves nothing and keeps logging `Waiting for the other nodes before creating a new cluster: …`.
**Cause:** It has no cluster data, and at least one node in its list has answered neither with a cluster nor that it has none.
**Fix:** Deal with the nodes the warning names, as in [When node 1 waits](#when-node-1-waits).

**Symptom:** A node that starts for the first time exits and prints `not ready to apply migrations: node 2 did not join within 10s (the limit is migrations_wait_ms; raise it if a migration file takes longer)`, exit code 1.
**Cause:** It could not join the cluster within `migrations_wait_ms`: the other nodes are not running, or cannot be reached. On the first start, node 1 creates the cluster only once every node answers, so one missing node makes the others, node 1 included, end this way.
**Fix:** Start every node; the `Waiting for the other nodes` warning in the log of node 1 says which one is missing. When you start them together, set `migrations_wait_ms` longer.

**Symptom:** At startup it prints `cannot start cluster node 2: node id 2 is in use: …`, exit code 2.
**Cause:** A node with a different address in the cluster already uses this id and is still running.
**Fix:** Use an id nobody uses (look at `cluster members` first), or stop that node first.

**Symptom:** `cluster members` or `status --cluster` prints `127.0.0.1:27011 refused the api secret: [cluster] secret_api differs from the cluster's`, exit code 2.
**Cause:** The API secret of this configuration differs from the cluster's.
**Fix:** Use the same `api.secret` as the cluster.

**Symptom:** `cluster members` hangs for 30 seconds and prints `the cluster did not answer within 30 s`, exit code 3.
**Cause:** None of the nodes in the configuration answers, or none has a leader.
**Fix:** Confirm that the nodes are running and the API ports can be reached; when the majority is lost, bring the nodes back first. With two voters and the leader stopped, see [Remove a node](#remove-a-node).

**Symptom:** `cluster remove` ends with `the change needs node 2 to commit: start it again with its data, wait about 20 s, stop it, and run this command again`, or, with one `voter` left, `cluster members` keeps showing a `leaving` node and `a change of the members is in progress`.
**Cause:** A removal that would leave one voter waits for the removed node's own vote.
**Fix:** Follow the steps of [Remove a node](#remove-a-node).

**Symptom:** `cluster remove` is refused with `InProgress` and ends with `run this command again once cluster members shows no change in progress`.
**Cause:** An earlier change of the members is not committed yet.
**Fix:** Wait until `cluster members` no longer prints `a change of the members is in progress`, and run the command again. If the line stays, finish that change first.

**Symptom:** `cluster remove` prints `node 4 still answers at 127.0.0.1:27014: stop it first, then remove it`.
**Cause:** The node to remove is still running.
**Fix:** Stop it first, then remove it.

**Symptom:** `cluster remove` prints `removing node 3 leaves two voters, and the cluster stops writing when either stops; give --force to remove it anyway`.
**Cause:** Only two voters would remain after the removal.
**Fix:** Add a new node first and then remove the old one; add `--force` only when you are sure you want to shrink to two nodes.

**Symptom:** A node ends after starting, or suddenly while running, with exit code 1, and the log has `cluster node 3 has been removed from the cluster: …`.
**Cause:** The member list of the current leader of the cluster does not include this node.
**Fix:** See [When a removed node starts again](#when-a-removed-node-starts-again): disable the service of a retired host; to add it back, confirm the cluster first, then empty `data_dir` and start it.

**Symptom:** `/readyz` of a node returns 503 `no leader`, logins get no answer, and `Discarded: no leader` increases.
**Cause:** `stop_without_leader_ms` is set, and this node has had no leader for longer than that.
**Fix:** Let it reconnect to the other nodes, or bring the stopped nodes back. `/livez` is still 200, so do not restart it because of this.

**Symptom:** Every line of `status --cluster` has the same address, or other nodes show `unreachable`.
**Cause:** The metrics ports of the nodes differ, or `[metrics] bind` is an address other hosts cannot reach.
**Fix:** Give every node the same metrics port, and set `bind` to the host of the `api` address.

**Symptom:** A node that was killed warns `Node did not shut down gracefully - auto-rebuilding State Machine` when it starts.
**Cause:** It was not shut down cleanly last time.
**Fix:** Nothing to do; it rebuilds itself. After that, stop it with SIGTERM and give it enough time.

**Symptom:** On Kubernetes a pod never becomes ready and the `startupProbe` fails.
**Cause:** No leader can be elected: the other pods did not start, the `raft` or `api` secrets differ, or `rradius-cluster` has no `publishNotReadyAddresses`.
**Fix:** Look at the reason with `kubectl logs`; keep the secrets in the same Secret.

**Symptom:** On Kubernetes requests from the NAS get no answer, and `unknown_client` increases.
**Cause:** The source address was rewritten by the load balancer.
**Fix:** See [Verify that the source address is not rewritten](#verify-that-the-source-address-is-not-rewritten).

**Symptom:** The `client=` in the authentication records of different NAS is always the same, and `source=` is the address of a node or a gateway.
**Cause:** The source address was rewritten and happens to fall in the network of some client, so all NAS are treated as the same one.
**Fix:** The same as above; shrink `[[radius.clients]]` to the actual network of the NAS.

**Symptom:** All three pods are ready, but after one is stopped all requests get no answer for a while.
**Cause:** The stopped one was the leader, and the remaining two are still electing.
**Fix:** It recovers within seconds; if it lasts, look at `rradius_cluster_leader`.

**Symptom:** A newly added migration is not applied (Kubernetes).
**Cause:** The `items` of `statefulset.yaml` is missing that line, so the file does not appear in `migrations/`.
**Fix:** See [chapter 18](18-operations.md).

## Related chapters

- [03 First start and first login](03-quick-start.md)
- [04 Configuration file basics](04-configuration.md)
- [07 User accounts and auth chains](07-users-and-chains.md)
- [13 Accounting and sessions](13-accounting.md)
- [17 Monitoring, status and logs](17-monitoring.md)
- [18 Day-to-day operations](18-operations.md)
- [19 Performance tuning and load testing](19-performance.md)
- [20 Security hardening](20-security.md)
- [Appendix A: Configuration reference](90-config-reference.md)
- [Appendix C: Metrics reference](92-metrics-reference.md)
