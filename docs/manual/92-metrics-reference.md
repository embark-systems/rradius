# 92 Appendix C: Metrics reference

> After this chapter you can:
> - Look up every metric that `rradiusd` writes to `/metrics`: its type, its labels and what it means
> - Tell which metrics are always present and which appear only with the matching configuration
> - Find the alert rule in `deploy/monitoring/rradius.rules.yml` that uses a metric

Prerequisites: you have read "Reading /metrics" in [Chapter 17](17-monitoring.md#reading-metrics). You know the difference between a counter, a gauge and a histogram, and how to fetch `/metrics` with `curl`.

In this chapter:

- [How to read this reference](#how-to-read-this-reference)
- [Where this list comes from](#where-this-list-comes-from)
- [Overview](#overview)
- [RADIUS](#radius)
- [TACACS+](#tacacs)
- [EAP](#eap)
- [Cluster and cluster database](#cluster-and-cluster-database)
- [Accounting spool and accounting log file](#accounting-spool-and-accounting-log-file)
- [Login lockout and password checks](#login-lockout-and-password-checks)
- [Netlogon](#netlogon)
- [External databases](#external-databases)
- [Named queries](#named-queries)
- [HTTP calls from scripts](#http-calls-from-scripts)
- [Reload, version and log](#reload-version-and-log)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

---

## How to read this reference

Each metric has its own section. The title is the name of the metric family, and the first line below it says:

- **Type:** `counter` (only goes up), `gauge` (the value right now) or `histogram` (a distribution).
- **Labels:** the name of each label and the values it can take. When the values are a fixed set, all of them are listed. When the values come from the configuration (for example client names), the source is given.
- **Present when:** "always" means every configuration writes it. For the others, the setting that is needed is named.
- **Alert:** the rules in `deploy/monitoring/rradius.rules.yml` that use it, or "—" if there are none.

Rules that apply to all metrics:

- Every metric is kept **per node**, counted from the start of the process. A reload does not reset it. Only a restart does.
- Once a metric family appears, all of its label combinations are present from the start with the value 0. A query never under-counts because a combination "appeared for the first time".
- The sample names of a histogram are `<name>_bucket{…,le="…"}`, `<name>_sum` and `<name>_count`. The upper bounds of the buckets (in seconds) are always `0.00005`, `0.0001`, `0.00025`, `0.0005`, `0.001`, `0.0025`, `0.005`, `0.01`, `0.025`, `0.05`, `0.1`, `0.25`, `0.5`, `1`, `2.5`, `5`, `10` and `+Inf`. `_sum` is in seconds, and `_count` equals the `+Inf` bucket.
- Each metric has an English `# HELP` line in `/metrics`. This reference describes it in its own words and does not translate that line word for word.

---

## Where this list comes from

Part of the metric names are built by the program (for example `rradius_query_<name>` for named queries). So this list was not copied from the source code. It was produced from a running `/metrics` and then cross-checked against the source code.

**Real output:** `rradiusd 0.1.0`, built from the core repo commit of 2026-11-23, was run with four configurations. `/metrics` was fetched once for each:

| Configuration | Contents | Metric families |
| --- | --- | --- |
| Minimal | the quick-start scenario (`[radius]`, the sql handler, the accounting script), `log.auth = false` | 39 |
| TACACS+ only | `rradius-server/benchmarks/tacacs.toml` (no `[radius]`) | 37 |
| Full | Minimal, plus `[tacacs]`, `[auth.lockout]`, `[radius.accounting_log]` and one `[queries.*]` | 49 |
| External database | Full, plus one `[databases.*]` with `required_for_ready = true` | 53 |

From each output, the names and types on the `# TYPE` lines were taken, and the label names and values from the sample lines. The union of the four is 53 metric families (the gauge of a named query counts as one).

```console
$ curl -s http://127.0.0.1:9812/metrics | grep '^# TYPE' | wc -l
```

**Source code:** In every file under `rradius-core/src/` (without the `#[cfg(test)]` test modules), the names passed to `write_family` and `write_histograms`, and the `# HELP` lines written directly, were collected. There are 53, and `rradius_query_{name}` among them is the dynamic name of a named query. The extensions built into the core are empty (`rradius-core/src/extensions/mod.rs`), so extensions add no metrics.

**Result:** The two sets are identical, with 0 differences. Every metric family the code writes appeared in the real output, and every metric family in the real output can be found in the code. The section titles of this appendix were compared with the same set as well. The type, the label names and the fixed label values in each section were checked one by one against the real output, again with 0 differences. Labels that come from the configuration (the names of clients, databases and queries, and the label columns of a named query) have no fixed values listed. Only their source is given.

The 39 metric families of the minimal configuration become 43 sample names once the `_bucket`, `_sum` and `_count` of the histograms are expanded.

> **Enterprise edition:** The enterprise edition's `/metrics` has more metrics after these, for its own features.

---

## Overview

| Metric family | Type | Present when | Main chapter |
| --- | --- | --- | --- |
| `rradius_radius_packets_total` | counter | always | 17 |
| `rradius_radius_responses_total` | counter | always | 17 |
| `rradius_radius_client_responses_total` | counter | always | 17 |
| `rradius_radius_status_server_total` | counter | always | 17 |
| `rradius_radius_duplicates_total` | counter | always | 17 |
| `rradius_radius_discarded_total` | counter | always | 17 |
| `rradius_radius_response_seconds` | histogram | always | 17 |
| `rradius_radius_requests_in_progress` | gauge | with `[radius]` | 17, 19 |
| `rradius_radius_requests_limit` | gauge | with `[radius]` | 17, 19 |
| `rradius_tacacs_connections_total` | counter | always | 17 |
| `rradius_tacacs_connections_open` | gauge | always | 17 |
| `rradius_tacacs_requests_total` | counter | always | 17 |
| `rradius_tacacs_responses_total` | counter | always | 17 |
| `rradius_traffic_bytes_total` | counter | always | 17 |
| `rradius_tacacs_malformed_total` | counter | always | 17 |
| `rradius_tacacs_response_seconds` | histogram | always | 17 |
| `rradius_eap_sessions_open` | gauge | always | 17 |
| `rradius_eap_sessions_total` | counter | always | 17 |
| `rradius_cluster_leader` | gauge | always | 16 |
| `rradius_cluster_is_leader` | gauge | always | 16 |
| `rradius_cluster_applied_index` | gauge | always | 16 |
| `rradius_cluster_seconds_without_leader` | gauge | always | 16 |
| `rradius_cluster_caught_up` | gauge | always | 16 |
| `rradius_db_batched_writes_total` | counter | always | 16 |
| `rradius_db_split_batches_total` | counter | always | 16 |
| `rradius_acct_spool_written_total` | counter | always | 13 |
| `rradius_acct_spool_replayed_total` | counter | always | 13 |
| `rradius_acct_spool_rejected_total` | counter | always | 13 |
| `rradius_acct_spool_full_total` | counter | always | 13 |
| `rradius_acct_spool_corrupt_total` | counter | always | 13 |
| `rradius_acct_spool_bytes` | gauge | always | 13 |
| `rradius_accounting_log_written_total` | counter | with `[radius.accounting_log]` | 13 |
| `rradius_accounting_log_dropped_total` | counter | with `[radius.accounting_log]` | 13 |
| `rradius_accounting_log_failures_total` | counter | with `[radius.accounting_log]` | 13 |
| `rradius_auth_lockout_rejected_total` | counter | with `[auth.lockout]` | 07 |
| `rradius_auth_lockouts_total` | counter | with `[auth.lockout]` | 07 |
| `rradius_auth_lockout_untracked_total` | counter | with `[auth.lockout]` | 07 |
| `rradius_password_checks_total` | counter | always | 07 |
| `rradius_netlogon_logons_total` | counter | always | 08 |
| `rradius_netlogon_channel_setups_total` | counter | always | 08 |
| `rradius_netlogon_open_connections` | gauge | always | 08 |
| `rradius_external_db_failures_total` | counter | with `[databases.*]` | 09 |
| `rradius_external_db_connections` | gauge | with `[databases.*]` | 09 |
| `rradius_external_db_connections_max` | gauge | with `[databases.*]` | 09 |
| `rradius_external_db_reachable` | gauge | with a `[databases.*]` that has `required_for_ready = true` | 09 |
| `rradius_query_{name}` | gauge | one for each `[queries.<name>]` | 09 |
| `rradius_query_failures_total` | counter | with `[queries.*]` | 09 |
| `rradius_query_dropped_rows_total` | counter | with `[queries.*]` | 09 |
| `rradius_query_last_success_timestamp_seconds` | gauge | with `[queries.*]` | 09 |
| `rradius_script_http_requests_total` | counter | always | 10 |
| `rradius_reload_total` | counter | always | 17 |
| `rradius_reload_last_success_timestamp_seconds` | gauge | always | 17 |
| `rradius_build_info` | gauge | always | 17 |
| `rradius_start_time_seconds` | gauge | always | 17 |
| `rradius_log_dropped_lines_total` | counter | always | 17 |

"Always" includes configurations that do not use the feature. For example, without `[tacacs]` the TACACS+ metrics are still written, all with the value 0. The same is true of the Netlogon metrics when there is no netlogon handler.

---

## RADIUS

The RADIUS metrics cover both UDP and RadSec. Main chapter: [Chapter 17](17-monitoring.md).

#### `rradius_radius_packets_total`

Type: counter. Labels: `kind` (`auth`, `acct`). Present when: always. Alert: —.

The number of RADIUS requests that passed the checks and the retransmission detection and actually went into processing, split into authentication and accounting. Discarded packets, retransmissions and Status-Server do not count here. In a test that sent 6 Access-Requests, 1 request with a wrong shared secret and 1 Status-Server, `kind="auth"` was 6.

#### `rradius_radius_responses_total`

Type: counter. Labels: `code` (`access_accept`, `access_reject`, `access_challenge`, `accounting_response`). Present when: always. Alert: `RradiusManyRejects`.

The responses that were sent, split by response type. A response that is sent again from the retransmission cache and a response to Status-Server do not count.

#### `rradius_radius_client_responses_total`

Type: counter. Labels: `client` (the `name` of a `[[radius.clients]]`), `code` (`access_accept`, `access_reject`, `access_challenge`, `accounting_response`). Present when: always. Each client in the configuration has four samples. With no clients, this family has only the two lines `# HELP` and `# TYPE`. Alert: —.

The number of responses sent to each client. Client names come only from the configuration and do not grow with traffic. A client added by a reload starts at 0, and the samples of a removed client disappear. The "Busiest RADIUS clients" list of `status` is calculated from this.

#### `rradius_radius_status_server_total`

Type: counter. Labels: none. Present when: always. Alert: —.

The number of Status-Server packets that were answered.

#### `rradius_radius_duplicates_total`

Type: counter. Labels: none. Present when: always. Alert: —.

Requests that the NAS sent again: they were answered directly from the retransmission cache, or dropped because the original was still being processed. If this value grows during a load test with `rradius-bench`, the test requests are being treated as retransmissions, and what you measure is not the normal path.

#### `rradius_radius_discarded_total`

Type: counter. Labels: `reason` (`overload`, `unknown_client`, `invalid`, `database`, `no_leader`). Present when: always. Alert: `RradiusOverload` (`overload`), `RradiusDiscardingForTheDatabase` (`database`, `no_leader`), `RradiusInvalidPackets` (`invalid`).

Packets that were received but not answered:

| `reason` | Meaning |
| --- | --- |
| `overload` | The requests in progress have reached `max_auth_inflight` or `max_acct_inflight` |
| `unknown_client` | The source does not belong to any client |
| `invalid` | Malformed, the authenticator or Message-Authenticator does not verify, or it does not fit the policy. The most common cause is a shared secret that does not match |
| `database` | A backend is unavailable (the cluster database, an external database, LDAP, a domain controller, or a password verification that timed out), so an Access-Request gets no answer |
| `no_leader` | `cluster.stop_without_leader_ms` has taken effect |

#### `rradius_radius_response_seconds`

Type: histogram. Labels: `kind` (`auth`, `acct`). Present when: always. Alert: `RradiusSlowLogins` (p99 of `kind="auth"` above 1 second).

The time a RADIUS request takes from passing the retransmission check to sending the response. Requests that get no response do not count. The p50 and p99 in `status` are interpolated from this.

#### `rradius_radius_requests_in_progress`

Type: gauge. Labels: `kind` (`auth`, `acct`). Present when: the server has `[radius]`. Alert: —.

The number of RADIUS requests being processed right now. When it gets close to `rradius_radius_requests_limit`, further requests are dropped with `overload`.

#### `rradius_radius_requests_limit`

Type: gauge. Labels: `kind` (`auth`, `acct`). Present when: the server has `[radius]`. Alert: —.

The number of requests that can be processed at the same time. This is the value of `radius.max_auth_inflight` and `radius.max_acct_inflight` (both default to 4096). For how to adjust them, see [Chapter 19](19-performance.md).

---

## TACACS+

Main chapter: [Chapter 17](17-monitoring.md). For TACACS+ itself, see [Chapter 12](12-tacacs.md).

#### `rradius_tacacs_connections_total`

Type: counter. Labels: `result` (`accepted`, `refused`). Present when: always. Alert: —.

The outcome of TACACS+ connections. `refused` is a connection that was closed before any data was read: the source does not belong to any `[[tacacs.clients]]`, or it went over `tacacs.max_connections_per_source` or `max_connections`.

#### `rradius_tacacs_connections_open`

Type: gauge. Labels: none. Present when: always. Alert: —.

The number of TACACS+ connections that are open right now.

#### `rradius_tacacs_requests_total`

Type: counter. Labels: `type` (`authen`, `author`, `acct`). Present when: always. Alert: —.

The number of TACACS+ requests, split into authentication, authorization and accounting. One login counts as one request, however many packets an ASCII login uses back and forth.

#### `rradius_tacacs_responses_total`

Type: counter. Labels: `type` (`authen`, `author`, `acct`), `status`. Present when: always (every label combination is 0 at startup). Alert: —.

The number of TACACS+ replies written to clients, split by the status in the reply. For authentication the statuses are `pass`, `fail`, `getdata`, `getuser`, `getpass`, `restart` and `error`. For authorization they are `pass`, `fail` and `error`. For accounting they are `success` and `error`. Only replies that have actually been written are counted. The TACACS+ section of `status` adds them up into the `Replies` line.

#### `rradius_traffic_bytes_total`

Type: counter. Labels: `protocol` (`radius`, `tacacs`), `direction` (`in`, `out`). Present when: always (all four label combinations are 0 at startup). Alert: —.

The bytes of protocol packets that the server received and wrote. For `radius`, this is the length of the RADIUS packets received on the UDP ports and of the responses written back. For `tacacs`, it is the length of the TACACS+ packets read (the 12-byte header plus the body) and of the replies written, both for plain TCP and for TACACS+ over TLS. `in` is added when a complete packet is received, even if it is dropped afterwards. `out` is added only after the write succeeds.

These are the bytes of the protocol packets. They do not include the IP, UDP, TCP and TLS headers or the record overhead, so they are lower than what a network interface sees (for example, 28 bytes less per RADIUS packet over IPv4). RadSec, proxy upstreams, CoA sent by the server, and cluster and database traffic are not included. The RADIUS and TACACS+ sections of `status` show this as `In bytes` and `Out bytes`, switched automatically to KiB, MiB and so on (powers of 1024). `--json` and `/metrics` still give bytes.

#### `rradius_tacacs_malformed_total`

Type: counter. Labels: none. Present when: always. Alert: —.

Requests whose content does not make sense after decryption. The most common cause is a shared secret that does not match.

#### `rradius_tacacs_response_seconds`

Type: histogram. Labels: `type` (`authen`, `author`, `acct`). Present when: always. Alert: —.

The time from reading a packet to writing the response. For authentication, each packet is counted once.

---

## EAP

Main chapter: [Chapter 17](17-monitoring.md). For EAP itself, see [Chapter 11](11-eap.md).

#### `rradius_eap_sessions_open`

Type: gauge. Labels: none. Present when: always. Alert: —.

The number of EAP conversations in progress right now.

#### `rradius_eap_sessions_total`

Type: counter. Labels: `method` (`tls`, `peap`, `ttls`), `result` (`accept`, `reject`, `timeout`, `overload`, `replaced`, `evicted`). Present when: always. Alert: `RradiusEapSessionsLost` (`evicted`, `overload`).

How EAP conversations end:

| `result` | Meaning |
| --- | --- |
| `accept`, `reject` | The conversation finished, accepted or rejected |
| `timeout` | The conversation went too long without a next step and was cleaned up |
| `overload` | Refused because of a limit or load, for example the device already has the maximum number of conversations |
| `replaced` | The same device started again after stopping at the Identity step, and the old conversation was replaced |
| `evicted` | The total number of conversations, or the number for one client, is full, and the one that had been idle longest was pushed out |

---

## Cluster and cluster database

Main chapter: [Chapter 16](16-cluster.md).

#### `rradius_cluster_leader`

Type: gauge. Labels: none. Present when: always. Alert: `RradiusNoLeader` (equal to 0 for 1 minute).

1 when this node has a leader it can write to, otherwise 0. A single node is also 1.

#### `rradius_cluster_is_leader`

Type: gauge. Labels: none. Present when: always. Alert: —.

1 when this node is the leader.

#### `rradius_cluster_applied_index`

Type: gauge. Labels: none. Present when: always. Alert: —.

The number of the last Raft log entry that the local copy applied. If the values on the nodes are close to each other, replication is keeping up.

#### `rradius_cluster_seconds_without_leader`

Type: gauge. Labels: none. Present when: always. Alert: —.

For how many seconds there has been no leader. It is 0 when there is a leader. When it goes over `cluster.stop_without_leader_ms`, `/readyz` returns 503 `no leader`.

#### `rradius_cluster_caught_up`

Type: gauge. Labels: none. Present when: always. Alert: `RradiusNotCaughtUp` (equal to 0 for 5 minutes).

1 when, after the node started, the local copy has caught up with the cluster. Before that it is 0, because the data in use may be behind.

#### `rradius_db_batched_writes_total`

Type: counter. Labels: none. Present when: always. Alert: —.

The number of script writes that were committed in the same transaction together with other writes.

#### `rradius_db_split_batches_total`

Type: counter. Labels: none. Present when: always. Alert: —.

How many times a merged transaction failed and was split up to find out which write failed.

---

## Accounting spool and accounting log file

Main chapter: [Chapter 13](13-accounting.md). The accounting spool holds accounting requests on disk, to be replayed when the database is back.

#### `rradius_acct_spool_written_total`

Type: counter. Labels: none. Present when: always. Alert: —.

The number of accounting requests written to the spool.

#### `rradius_acct_spool_replayed_total`

Type: counter. Labels: none. Present when: always. Alert: —.

The number of accounting requests replayed from the spool and accepted by the script.

#### `rradius_acct_spool_rejected_total`

Type: counter. Labels: none. Present when: always. Alert: `RradiusAccountingSpoolLoss`.

The number of accounting requests that were rejected during replay and moved to `rejected.log`.

#### `rradius_acct_spool_full_total`

Type: counter. Labels: none. Present when: always. Alert: —.

The number of accounting requests that were not written to the spool because the spool was full or the disk had an error.

#### `rradius_acct_spool_corrupt_total`

Type: counter. Labels: none. Present when: always. Alert: `RradiusAccountingSpoolLoss`.

The number of spool files that were damaged and moved aside.

#### `rradius_acct_spool_bytes`

Type: gauge. Labels: none. Present when: always. Alert: `RradiusAccountingSpoolBacklog` (above 0 for 15 minutes).

The bytes in the spool that are waiting to be replayed.

#### `rradius_accounting_log_written_total`

Type: counter. Labels: none. Present when: with `[radius.accounting_log]`. Alert: —.

The number of accounting requests written to the accounting log file.

#### `rradius_accounting_log_dropped_total`

Type: counter. Labels: none. Present when: with `[radius.accounting_log]`. Alert: `RradiusAccountingLogDropping`.

The number of accounting requests that were not written because the queue of the accounting log file was full.

#### `rradius_accounting_log_failures_total`

Type: counter. Labels: `stage` (`open`, `write`). Present when: with `[radius.accounting_log]`. Alert: `RradiusAccountingLogDropping`.

How many times opening the accounting log file failed and how many times writing to it failed.

---

## Login lockout and password checks

Main chapter: [Chapter 07](07-users-and-chains.md).

#### `rradius_auth_lockout_rejected_total`

Type: counter. Labels: none. Present when: with `[auth.lockout]`. Alert: —.

The number of logins that were rejected directly because the account plus source, or the whole account, is locked, or because no slot was free: too many logins are waiting for a decision, counting the failures in the window ([Chapter 7](07-users-and-chains.md)).

#### `rradius_auth_lockouts_total`

Type: counter. Labels: none. Present when: with `[auth.lockout]`. Alert: —.

How many times a lockout started.

#### `rradius_auth_lockout_untracked_total`

Type: counter. Labels: none. Present when: with `[auth.lockout]`. Alert: —.

The number of failures that were not counted because the lockout table was full of entries that are currently locked.

#### `rradius_password_checks_total`

Type: counter. Labels: `scheme` (`clear`, `none`, `crypt`, `md5`, `smd5`, `sha`, `ssha`, `sha224`, `ssha224`, `sha256`, `ssha256`, `sha384`, `ssha384`, `sha512`, `ssha512`, `pbkdf2-sha1`, `pbkdf2-sha256`, `pbkdf2-sha512`, `x-pbkdf2`, `ns-mta-md5`, `nthash`, `argon2`, `unknown`), `result` (`failed`, `ok`). Present when: always, 23 × 2 samples. Alert: —.

How many times a login password was compared with the stored password, split by the format of the stored value and by the result. `none` is a value with no format prefix. `unknown` is a prefix that is not recognized. `pbkdf2-sha1` covers both of its spellings. In a test with the quick-start scenario, `alice` (plain text) was counted under `none` and `bob` (a bcrypt hash) under `crypt`. Use it to find out which old password formats are still in the table.

---

## Netlogon

Main chapter: [Chapter 08](08-ldap-and-ad.md).

#### `rradius_netlogon_logons_total`

Type: counter. Labels: `result` (`accept`, `reject`, `no_such_user`, `unavailable`). Present when: always. Alert: —.

The MS-CHAPv2 logins that the netlogon handler asked a domain controller about, split by result. `unavailable` means no domain controller could answer.

#### `rradius_netlogon_channel_setups_total`

Type: counter. Labels: `result` (`ok`, `failed`). Present when: always. Alert: —.

How many times a Netlogon secure channel was set up with a domain controller, split by result.

#### `rradius_netlogon_open_connections`

Type: gauge. Labels: none. Present when: always. Alert: —.

The number of Netlogon connections that are open and encrypted right now.

---

## External databases

Main chapter: [Chapter 09](09-external-databases.md). The `database` label is the name of a `[databases.<name>]`.

#### `rradius_external_db_failures_total`

Type: counter. Labels: `database` (the name in the configuration), `reason` (`unavailable`, `mistake`, `unknown`). Present when: with `[databases.*]`. Alert: `RradiusExternalDatabaseFailing` (`unavailable`).

How many calls to each external database failed. `unavailable` means it could not be reached or timed out, `mistake` means the SQL or a parameter is wrong, and `unknown` means anything else.

#### `rradius_external_db_connections`

Type: gauge. Labels: `database` (the name in the configuration), `state` (`in_use`, `idle`). Present when: with `[databases.*]`. Alert: `RradiusExternalDatabasePoolFull`.

The number of connections in the connection pool. `in_use` are those handed to a caller or being created, and `idle` are those that are open and idle.

#### `rradius_external_db_connections_max`

Type: gauge. Labels: `database` (the name in the configuration). Present when: with `[databases.*]`. Alert: `RradiusExternalDatabasePoolFull`.

The most connections the pool keeps, which is `pool_size`.

#### `rradius_external_db_reachable`

Type: gauge. Labels: `database` (the name in the configuration). Present when: a database has `required_for_ready = true`. Only those databases are listed. Alert: `RradiusExternalDatabaseUnreachable` (equal to 0 for 2 minutes).

1 when the readiness check can reach this database, otherwise 0. When it is 0, `/readyz` returns 503 `database <name> unreachable`. Measured:

```text
rradius_external_db_reachable{database="billing"} 0
```

---

## Named queries

Main chapter: [Chapter 09](09-external-databases.md). The `query` label is the name of a `[queries.<name>]`.

#### `rradius_query_{name}`

Type: gauge. Labels: the columns listed in `[queries.<name>] labels`. The label name is the column name. Present when: one for each `[queries.<name>]`. The name is `rradius_query_` plus the name of the query. Alert: —.

The result of each row of a named query. The value is taken from the `value` column. `# HELP` is the `help` from the configuration, or a text with the name of the query if none is set. Before the query has ever succeeded, there are only the two lines `# HELP` and `# TYPE`, and no samples. Measured:

```text
# HELP rradius_query_accounts_by_state Accounts in the users table, by state.
# TYPE rradius_query_accounts_by_state gauge
rradius_query_accounts_by_state{state="disabled"} 1
rradius_query_accounts_by_state{state="enabled"} 2
```

#### `rradius_query_failures_total`

Type: counter. Labels: `query` (the name of the query), `reason` (`timeout`, `error`, `shape`). Present when: with `[queries.*]`. Alert: `RradiusNamedQueryFailing`.

How many times each named query failed. `timeout` means there was no result in time. `error` means the database rejected the SQL or could not be reached. `shape` means the result is missing a column or a value has the wrong type.

#### `rradius_query_dropped_rows_total`

Type: counter. Labels: `query` (the name of the query), `reason` (`null_value`, `over_limit`). Present when: with `[queries.*]`. Alert: —.

The number of rows from a successful query that did not become samples. `null_value` means the value column is NULL, and `over_limit` means the rows went over `max_rows`.

#### `rradius_query_last_success_timestamp_seconds`

Type: gauge. Labels: `query` (the name of the query). Present when: with `[queries.*]`. Alert: `RradiusNamedQueryStale` (no success for more than 120 seconds).

The Unix time of the last success of each named query. It is 0 if the query has never succeeded.

---

## HTTP calls from scripts

Main chapter: [Chapter 10](10-lua-policies.md).

#### `rradius_script_http_requests_total`

Type: counter. Labels: `result` (`ok`, `timeout`, `error`, `denied`, `too_large`, `busy`). Present when: always. Alert: —.

The results of script calls to `http_request`:

| `result` | Meaning |
| --- | --- |
| `ok` | A response was received |
| `timeout` | The time limit was exceeded |
| `error` | The connection or the request failed |
| `denied` | The host is not in `lua.http.allowed_hosts` |
| `too_large` | The response went over the size limit |
| `busy` | The waiting requests to the same host have reached the limit |

---

## Reload, version and log

Main chapter: [Chapter 17](17-monitoring.md).

#### `rradius_reload_total`

Type: counter. Labels: `result` (`ok`, `error`). Present when: always. Alert: `RradiusReloadFailed` (`error`).

The number of reloads, split by result. When one fails, the server keeps using its previous configuration and the log says `reload failed, keeping the running configuration: …`.

#### `rradius_reload_last_success_timestamp_seconds`

Type: gauge. Labels: none. Present when: always. Alert: —.

The Unix time of the last successful reload. It is 0 if there has been no reload.

#### `rradius_build_info`

Type: gauge. Labels: `version` (the version of the program, for example `0.1.0`). Present when: always. Alert: —.

The value is always 1, and the version is in the label. Use it to see whether all the nodes in a cluster run the same version.

#### `rradius_start_time_seconds`

Type: gauge. Labels: none. Present when: always. Alert: —.

The Unix time the process started. `time() - rradius_start_time_seconds` is how long it has been running. If this value changes, the process was restarted, and all counters start again from 0.

#### `rradius_log_dropped_lines_total`

Type: counter. Labels: none. Present when: always. Alert: `RradiusLogLinesDropped`.

The number of log lines that were dropped because the output could not keep up, or that failed to be written.

---

## Common mistakes

**Symptom:** A metric has "disappeared" from a Grafana chart.
**Cause:** It appears only with the matching configuration. For example, after you remove `[auth.lockout]` and restart, the `rradius_auth_lockout_*` metrics are no longer written. After a named query is renamed, the gauge with the old name is gone too.
**Fix:** Check the "Present when" column in the [Overview](#overview).

**Symptom:** A counter suddenly falls back to a small number, and `rate()` shows a spike or a gap.
**Cause:** The node was restarted and the counters started from 0. `rradius_start_time_seconds` changes with it.
**Fix:** Always query with `rate()` and `increase()`, which handle the reset. In this case `status` prints `counters reset since the previous sample: the server restarted`.

**Symptom:** `rradius_radius_packets_total` differs a lot between the three nodes of a cluster.
**Cause:** Metrics are counted per node. The numbers show which node the NAS sent its requests to.
**Fix:** For the total, use `sum without (instance) (…)`, or `status --cluster`.

**Symptom:** A client is missing from `rradius_radius_client_responses_total`.
**Cause:** A reload removed it from the configuration, and its samples were removed with it.
**Fix:** Check that the `name` of this client is still in the configuration.

## Related chapters

- [07 User accounts and auth chains](07-users-and-chains.md)
- [08 LDAP and Active Directory](08-ldap-and-ad.md)
- [09 External databases and named queries](09-external-databases.md)
- [10 Writing policies in Lua](10-lua-policies.md)
- [11 802.1X and EAP](11-eap.md)
- [12 TACACS+ device administration](12-tacacs.md)
- [13 Accounting and sessions](13-accounting.md)
- [16 Clusters and high availability](16-cluster.md)
- [17 Monitoring, status and logs](17-monitoring.md)
- [19 Performance tuning and load testing](19-performance.md)
- [Appendix A: Configuration reference](90-config-reference.md)
