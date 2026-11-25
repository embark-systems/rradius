# 17 Monitoring, status and logs

> After this chapter you can:
> - Turn on the metrics port and know what `/metrics`, `/livez` and `/readyz` each answer, and when
> - Have Prometheus scrape the metrics and apply the alert rules shipped in the repo
> - Use `status` to see the live state of one node or of the whole cluster, and get JSON for other programs to read
> - Adjust the log level and format, and read every authentication record
> - Turn on a trace for a specific user or client for a limited time, without a restart

Prerequisites: you have finished [Chapter 03](03-quick-start.md), so you have a working quick-start scenario and the two commands `rradiusd` and `rradius-bench`. The full description of every metric is in [Appendix C](../manual.zh-TW/92-metrics-reference.md).

In this chapter:

- [Concept: three ways to look at the server](#concept-three-ways-to-look-at-the-server)
- [Prepare the lab environment](#prepare-the-lab-environment)
- [Turn on the metrics port](#turn-on-the-metrics-port)
- [Liveness and readiness](#liveness-and-readiness)
- [Reading /metrics](#reading-metrics)
- [Let Prometheus scrape the metrics](#let-prometheus-scrape-the-metrics)
- [Alert rules and Alertmanager](#alert-rules-and-alertmanager)
- [Live state with status](#live-state-with-status)
- [The sections of status](#the-sections-of-status)
- [Looking at the whole cluster](#looking-at-the-whole-cluster)
- [Adjusting the log](#adjusting-the-log)
- [Authentication records](#authentication-records)
- [Tracing a specific user or client](#tracing-a-specific-user-or-client)
- [When log lines are dropped](#when-log-lines-are-dropped)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

## Concept: three ways to look at the server

rradius gives you three ways to observe it, each for a different purpose:

| Channel | Where you look | What it is good for |
| --- | --- | --- |
| HTTP on the metrics port | `/metrics`, `/livez`, `/readyz` | Long-term collection, graphs and alerts in Prometheus; letting a load balancer or Kubernetes decide whether to send traffic |
| The `status` command | Reads the same metrics port and arranges it on one page | You are at a terminal and want to know right now "how many requests per second, and are packets being dropped" |
| The log | The server's standard error output (stderr) | Finding out why one login was rejected or why a reload failed; a trace shows the full request and response |

How they relate: `status` has no data source of its own. It reads `/metrics` and `/readyz`. So without `[metrics]` there is no `status`. The log and the metrics are independent of each other: when the log cannot be written, the server keeps answering requests and only records in the metrics how many lines were dropped.

Metrics are **counted by each node on its own**, accumulated since the process started. A reload does not reset them; a restart does. To see a total across a cluster, add up the numbers from every node; `status --cluster` does the adding for you.

## Prepare the lab environment

This chapter uses the default ports: RADIUS 1812 and 1813, metrics 9812. TACACS+ uses 4949, as in [Chapter 12](12-tacacs.md), because the standard port 49 is below 1024 and a normal account cannot listen on it.

Copy the quick-start scenario and replace the example secrets (the same way as in Chapter 03):

```console
$ cp -R rradius-server/examples/scenarios/quick-start ~/rradius-lab/monitoring
$ cd ~/rradius-lab/monitoring
$ for f in raft api localhost; do openssl rand -hex 32 > $f.secret; done
$ chmod 600 *.secret
```

Change the dictionary path to an absolute path as in Chapter 03. The `[radius]` and `[metrics]` sections of quick-start already use the default ports, so you do not need to change them.

So that every section of `status` has something to show, add TACACS+, login-failure lockout, an accounting log file and one named query at the end of the file. Each is covered in its own chapter; here they are only borrowed to produce metrics:

```toml
# rradius.toml (add at the end of the file)
[auth.lockout]
max_failures = 3

[radius.accounting_log]
file = "acct.jsonl"

[tacacs]
bind = "127.0.0.1"
port = 4949

[[tacacs.clients]]
name = "lab-switch"
address = "127.0.0.1"
secret_file = "localhost.secret"

[queries.accounts_by_state]
sql = "SELECT CASE enabled WHEN 1 THEN 'enabled' ELSE 'disabled' END AS state, COUNT(*) AS value FROM users GROUP BY enabled"
help = "Accounts in the users table, by state."
labels = ["state"]
interval_ms = 5000
```

- `[auth.lockout]` is in [Chapter 07](07-users-and-chains.md), `[radius.accounting_log]` in [Chapter 13](13-accounting.md), `[tacacs]` in [Chapter 12](12-tacacs.md), and `[queries]` in [Chapter 09](09-external-databases.md).
- The TACACS+ client borrows `localhost.secret` only to save creating one more file. In production every client uses its own shared secret.

```console
$ rradiusd check --config rradius.toml
configuration ok
$ rradiusd serve --config rradius.toml
…
… INFO rradius_core::tacacs: TACACS+ Server listening on 127.0.0.1:4949
… INFO rradius_core::radius: RADIUS Server listening on 127.0.0.1:1812 and 127.0.0.1:1813
… INFO rradius_core::metrics: Metrics listening on 127.0.0.1:9812
```

In another terminal, send some traffic: one successful login, four wrong passwords (`bob` is locked after the third), one accounting request, one TACACS+ login, one request with a wrong shared secret, and one Status-Server.

```console
$ S="$(cat localhost.secret)"
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --acct-port 1813 --secret "$S" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
…
$ for i in 1 2 3 4; do rradius-bench run --server 127.0.0.1 --auth-port 1812 --secret "$S" \
    --user bob --password wrong --load pap,concurrency=1 --requests 1; done
…
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --acct-port 1813 --secret "$S" \
    --user alice --load acct-start,concurrency=1 --requests 1
…
$ rradius-bench run --server 127.0.0.1 --tacacs-port 4949 --secret "$S" \
    --user alice --password alice-password --load tacacs-pap,concurrency=1 --requests 1
…
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --secret wrongsecret \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1 --timeout 1
…
  pap: 0/s, sent 1, accept 0, reject 0, challenge 0, accounting 0, timeouts 1, invalid 0, late 0, socket errors 0, retransmits 0
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --secret "$S" --load status-auth,concurrency=1 --requests 1
…
```

The server does not answer the request with the wrong shared secret, so `rradius-bench` sees `timeouts 1`.

## Turn on the metrics port

The server opens this HTTP port only if the configuration file has `[metrics]` (even an empty table):

| Key | Default | Reload | Meaning |
| --- | --- | --- | --- |
| `metrics.bind` | `127.0.0.1` | restart | The address to listen on |
| `metrics.port` | `9812` | restart | The TCP port to listen on |

The full key table is in [Appendix A](../manual.zh-TW/90-config-reference.md).

This port is simple:

- It accepts only `GET`. Other methods get 405, including `HEAD`, so `curl -I` sees 405.
- It has only three paths, `/metrics`, `/livez` and `/readyz`; anything else gets 404. A query string after the URL (`?x=1`) does not change the result.
- It closes the connection after every answer. It serves at most 16 connections at the same time and closes the extra ones immediately.
- If the request headers exceed 8 KiB or are not finished within 5 seconds, the connection is closed; the answer must also be read within 5 seconds.

Measured:

```console
$ curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:9812/
404
$ curl -s -o /dev/null -w "%{http_code}\n" -X POST http://127.0.0.1:9812/metrics
405
$ curl -s -o /dev/null -w "%{http_code}\n" "http://127.0.0.1:9812/metrics?x=1"
200
```

> **Warning:** The metrics port has no user name or password and no TLS. Anyone who can reach it can read things such as client names and the results of named queries. Bind `bind` only to an internal network address, and use a firewall so that only Prometheus and the management hosts can reach it. When you bind `0.0.0.0`, `status` connects to the local loopback instead.

## Liveness and readiness

`/livez` and `/readyz` both answer with plain text, one line plus a newline:

| Path | Answer | When |
| --- | --- | --- |
| `/livez` | 200 `ok` | Whenever the metrics port can answer. It is also 200 while the server is stopping |
| `/readyz` | 503 `stopping` | After SIGTERM or SIGINT is received and the server starts stopping, until the process exits |
| `/readyz` | 503 `no leader` | `cluster.stop_without_leader_ms` is set and the time without a leader has passed it; login requests are not answered during this time |
| `/readyz` | 503 `database <name> unreachable` | An external database with `required_for_ready = true` cannot be reached; with several, the names are sorted alphabetically and separated by `, ` |
| `/readyz` | 200 `ok` | None of the above |

When several conditions hold at once, the order is: `stopping` first, then `no leader`, and the databases last. On a node without `stop_without_leader_ms`, `/readyz` stays 200 when there is no leader, because the node still answers logins with its local data (see [Chapter 16](16-cluster.md)).

**How to use them:** use `/livez` for the liveness probe, and restart the process when it fails. Use `/readyz` for the readiness probe, and when it fails, stop sending traffic to the node but do not restart it. `deploy/kubernetes/statefulset.yaml` in the repo is set up this way: `startupProbe` and `livenessProbe` look at `/livez`, and `readinessProbe` looks at `/readyz`.

**When stopping:** measured with `[shutdown] ready_delay_ms` set to `2000` and then SIGTERM sent:

```console
$ kill -TERM <pid>
$ curl -s -w " HTTP %{http_code}\n" http://127.0.0.1:9812/readyz
stopping
 HTTP 503
$ curl -s -w " HTTP %{http_code}\n" http://127.0.0.1:9812/livez
ok
 HTTP 200
```

The server's log:

```text
… INFO rradius_core::cli: stopping: /readyz answers 503; requests are answered for 2000 ms more, then those being handled are waited for, for at most 5000 ms
… INFO rradius_core::cli: stopping: no more requests taken
… INFO rradius_core::cli: stopping: every request taken was answered
```

During the `ready_delay_ms` period, `/readyz` is already 503 but requests are still answered as usual, which gives the load balancer time to move traffic away. `[shutdown]` is described in [Chapter 04](04-configuration.md), and the shutdown procedure is in [Chapter 18](18-operations.md).

**External databases:** set `required_for_ready = true` in `[databases.<name>]`. The server runs `SELECT 1` against it every 5 seconds on a connection of its own. Two failures in a row count as unreachable, and one success restores it. Measured with a PostgreSQL that does not exist (a port on `127.0.0.1` with nothing listening) set as required:

```console
$ curl -s -w " HTTP %{http_code}\n" http://127.0.0.1:9812/readyz
database billing unreachable
 HTTP 503
```

If the database cannot be reached at startup, the log says so once first:

```text
… WARN rradius_core::external_db: databases.billing: not reachable at startup: error communicating with database: Connection refused (os error 61); /readyz answers 503 until it is
```

The configuration of external databases is in [Chapter 09](09-external-databases.md).

> **Not verified on real hardware:** The `no leader` answer can only be reproduced with a multi-node cluster, and this chapter did not run it. The behavior is based on `rradius-core/src/metrics.rs` and `specs/server-metrics` (Health Endpoints).

## Reading /metrics

`/metrics` answers in the Prometheus text format (`Content-Type: text/plain; version=0.0.4`). Each metric family starts with two lines, `# HELP` and `# TYPE`, followed by the samples:

```console
$ curl -s http://127.0.0.1:9812/metrics | grep -A6 '^# HELP rradius_radius_responses_total'
# HELP rradius_radius_responses_total RADIUS responses sent, excluding replays and Status-Server.
# TYPE rradius_radius_responses_total counter
rradius_radius_responses_total{code="access_accept"} 1
rradius_radius_responses_total{code="access_reject"} 5
rradius_radius_responses_total{code="access_challenge"} 0
rradius_radius_responses_total{code="accounting_response"} 1
```

A few rules to know when reading:

- **Three types.** A counter only goes up, and you read it with `rate()` or `increase()`. A gauge is the current value and can go up and down. A histogram is the distribution of response times.
- **Label combinations exist from the start.** For example `access_challenge` has a line with `0` even if it never happened. So a query does not compute a wrong rate because some value appears "for the first time".
- **Some metric families appear only when the matching setting exists.** For example, without `[auth.lockout]` there is no `rradius_auth_lockout_*`. The conditions for each are listed in [Appendix C](../manual.zh-TW/92-metrics-reference.md).
- **Client names come from the configuration.** The `client` label of `rradius_radius_client_responses_total` is the `name` of a `[[radius.clients]]` entry, so it does not grow with traffic. When a reload removes a client, its samples disappear with it.

The metric families this chapter covers:

| Metric family | What it shows |
| --- | --- |
| `rradius_radius_*` | How many RADIUS requests came in, what was answered, how many were dropped and why, how long it took, and how many are being handled now |
| `rradius_tacacs_*` | TACACS+ connections, each kind of request, requests that could not be decoded, response times |
| `rradius_traffic_bytes_total` | Protocol packet bytes received and written by RADIUS (UDP) and TACACS+ |
| `rradius_eap_*` | EAP conversations in progress, how conversations end |
| `rradius_reload_*` | The number of successful and failed reloads, the time of the last success |
| `rradius_build_info`, `rradius_start_time_seconds` | Version, start time |
| `rradius_log_dropped_lines_total` | Dropped log lines |

The other metric families have their own main chapters: the cluster and `rradius_db_*` are in [Chapter 16](16-cluster.md), the accounting spool and the accounting log file are in [Chapter 13](13-accounting.md), lockout and password comparison are in [Chapter 07](07-users-and-chains.md), Netlogon is in [Chapter 08](08-ldap-and-ad.md), external databases and named queries are in [Chapter 09](09-external-databases.md), and HTTP calls from scripts are in [Chapter 10](10-lua-policies.md).

### Requests without a response

The metric you will look at most when troubleshooting is `rradius_radius_discarded_total`. It counts packets the server received but did not answer, split by `reason`:

| `reason` | Meaning |
| --- | --- |
| `overload` | The requests being handled at the same time have reached `max_auth_inflight` or `max_acct_inflight` (Chapter 19) |
| `unknown_client` | The source address does not belong to any `[[radius.clients]]` |
| `invalid` | Malformed, or the authenticator or Message-Authenticator is wrong; the most common cause is a shared secret mismatch |
| `database` | The cluster database cannot be used, so the login gets no answer |
| `no_leader` | `stop_without_leader_ms` is set and there has been no leader for too long |

The request with the wrong shared secret sent above is counted here:

```text
rradius_radius_discarded_total{reason="invalid"} 1
```

`rradius_radius_duplicates_total` counts retransmits separately: the NAS did not get an answer and sent the same request again, and the server answers it from the retransmit cache or drops it while it is still being handled. It is not counted in `discarded`.

### Response times

`rradius_radius_response_seconds` and `rradius_tacacs_response_seconds` are histograms. RADIUS is split by `kind` (`auth`, `acct`), measured from passing the retransmit check to sending the response; requests with no response are not counted. TACACS+ is split by `type` (`authen`, `author`, `acct`), and each authentication packet is counted once.

The upper bounds of the buckets (in seconds) are fixed:

```text
0.00005 0.0001 0.00025 0.0005 0.001 0.0025 0.005 0.01 0.025 0.05 0.1 0.25 0.5 1 2.5 5 10 +Inf
```

They run from 50 microseconds to 10 seconds. A local login usually falls under 1 millisecond; anything over 10 seconds has long been given up by the NAS. The unit of `_sum` is seconds.

## Let Prometheus scrape the metrics

The repo's `deploy/monitoring/` has the alert rules and an Alertmanager example. The rules assume the scrape job is named `rradius`, so write the Prometheus configuration like this:

```yaml
# prometheus.yml (excerpt)
scrape_configs:
  - job_name: rradius
    static_configs:
      - targets: ["10.0.0.11:9812", "10.0.0.12:9812", "10.0.0.13:9812"]
rule_files:
  - rradius.rules.yml
alerting:
  alertmanagers:
    - static_configs:
        - targets: ["alertmanager:9093"]
```

- List every node of the cluster. The metrics are counted by each node on its own, so leaving one out loses its share.
- The `[metrics] bind` of every node must be an internal address that Prometheus can reach, not `127.0.0.1`.

A few common queries, taken directly from the rules file:

```promql
# Share of Access-Reject in Accept + Reject, per second, per host
sum by (instance) (rate(rradius_radius_responses_total{code="access_reject"}[10m]))
  / sum by (instance) (rate(rradius_radius_responses_total{code=~"access_accept|access_reject"}[10m]))

# p99 of login response time (seconds)
histogram_quantile(0.99, sum by (le, instance) (rate(rradius_radius_response_seconds_bucket{kind="auth"}[5m])))

# Packets dropped because of overload in five minutes
increase(rradius_radius_discarded_total{reason="overload"}[5m])
```

> **Not verified on real hardware:** This chapter was not connected to a real Prometheus and Grafana. The scrape configuration and queries above were only checked against the files in `deploy/monitoring/` and the real output of `/metrics`.

## Alert rules and Alertmanager

`deploy/monitoring/rradius.rules.yml` has 19 rules in five groups. Metrics that exist only with the matching setting (external databases, the accounting log file, named queries) do not appear when the setting is absent, and the related rules never fire.

| Group | Rule | Condition (excerpt) | Severity |
| --- | --- | --- | --- |
| availability | `RradiusDown` | `up{job="rradius"} == 0` for 2 minutes | critical |
| | `RradiusNoLeader` | `rradius_cluster_leader == 0` for 1 minute | critical |
| | `RradiusNotCaughtUp` | `rradius_cluster_caught_up == 0` for 5 minutes | warning |
| traffic | `RradiusOverload` | `overload` drops within 5 minutes | critical |
| | `RradiusDiscardingForTheDatabase` | `database` or `no_leader` drops within 5 minutes | warning |
| | `RradiusManyRejects` | More than half Rejects within 10 minutes, and more than 0.2 per second | warning |
| | `RradiusInvalidPackets` | `invalid` drops continuing within 10 minutes | warning |
| | `RradiusSlowLogins` | Login p99 over 1 second, for 10 minutes | warning |
| accounting | `RradiusAccountingSpoolBacklog` | The accounting spool holds data, for 15 minutes | warning |
| | `RradiusAccountingSpoolLoss` | Records rejected or corrupted in the spool within 1 hour | critical |
| | `RradiusAccountingLogDropping` | The accounting log file drops or fails to write within 10 minutes | warning |
| dependencies | `RradiusExternalDatabaseUnreachable` | `rradius_external_db_reachable == 0` for 2 minutes | critical |
| | `RradiusExternalDatabaseFailing` | Failures of the `unavailable` kind within 5 minutes | warning |
| | `RradiusExternalDatabasePoolFull` | Connections in use equal the limit, for 5 minutes | warning |
| | `RradiusNamedQueryStale` | A named query has not succeeded for more than 120 seconds | warning |
| | `RradiusNamedQueryFailing` | A named query fails within 10 minutes | warning |
| operations | `RradiusReloadFailed` | A reload fails within 10 minutes | warning |
| | `RradiusLogLinesDropped` | Log lines dropped within 10 minutes | warning |
| | `RradiusEapSessionsLost` | EAP conversations pushed out (`evicted`) or refused (`overload`) within 10 minutes | warning |

The full condition and description text of every rule is in the rules file, which is the authority. Which metric each rule uses is also written under each metric in Appendix C.

`rradius.rules.test.yml` holds the test cases for `promtool test rules`: it feeds in fake time series and checks which alerts fire at what time. After you change the rules, run these in `deploy/monitoring/`:

```console
$ promtool check rules rradius.rules.yml
$ promtool test rules rradius.rules.test.yml
```

`alertmanager.yml` groups alerts by `alertname` and `instance`, sends them by webhook to `http://snmp-notifier:9464/alerts`, and also sends them when an alert resolves (`send_resolved: true`). `snmp_notifier` is a separate project that turns alerts into SNMP traps, for management systems that only receive traps. If you already have your own Alertmanager configuration, merge its `route` and `receivers` into yours.

> **Not verified on real hardware:** The machine used to write this chapter has no `promtool`, so the two commands above were not run; forwarding through Alertmanager and SNMP traps was not run either.

### Three ways to connect

rradius outputs Prometheus metrics and has **no built-in SNMP**. Choose one by your monitoring system:

| Your monitoring system | What to do | Verified? |
| --- | --- | --- |
| Prometheus and Alertmanager (or a system that can receive Alertmanager webhooks) | Load `rradius.rules.yml`; Alertmanager sends the alerts | The syntax and logic of the rules were checked with `promtool` (as recorded in the earlier version of the chapter); no real notification channel was connected |
| Only receives SNMP traps | The Alertmanager webhook goes to `snmp_notifier`, which sends SNMP traps (v2c or v3) | Recorded in the earlier version: tested locally with three containers, Alertmanager, `snmp_notifier` and `snmptrapd`; `snmptrapd` received a v2c trap carrying the alert name, node, severity and summary. v3 and a real monitoring system were not tested. It was not rerun this time, **not verified on real hardware** |
| A monitoring system that can read HTTP (for example the HTTP agent item of Zabbix) | Read `GET /metrics` directly, pick the metrics to watch, and set your own thresholds; the thresholds in the rules file can serve as a reference | **Not verified on real hardware** |
| Only knows SNMP polling (`snmpwalk` must see rradius numbers) | **Not supported** | See [Why there is no SNMP polling](#why-there-is-no-snmp-polling) |

### What to do for each alert

The table below is arranged from the description text and the `for` setting of `rradius.rules.yml`. "For" is the `for` of the rule; when it is not written, the alert fires as soon as the condition holds.

| Alert | Suggested handling |
| --- | --- |
| `RradiusDown` | The process died, or the `[metrics]` address cannot be reached from Prometheus; look at the state and logs in systemd or the container |
| `RradiusNoLeader` | The node is cut off, or the cluster lost its majority. The node cannot write: accounting goes to the spool and logins that need a write are rejected; see [Chapter 16](16-cluster.md#when-there-is-no-leader) |
| `RradiusNotCaughtUp` | The local replica has been behind the cluster for more than 5 minutes: the node just joined or just restarted, or there is a lot of data; if there is no progress, look at the log |
| `RradiusOverload` | More requests are coming in than the node can handle: look for a slow database, directory or script; read it together with `RradiusSlowLogins` and [Chapter 21](../manual.zh-TW/21-troubleshooting.md) |
| `RradiusDiscardingForTheDatabase` | The database cannot answer, or there is no leader to write to; read it together with `RradiusNoLeader` |
| `RradiusManyRejects` | A directory failure, a configuration error, or someone trying passwords; look at the reject reasons in the log |
| `RradiusInvalidPackets` | Usually the shared secret of one device is wrong |
| `RradiusSlowLogins` | The database, directory or script got slow; 1 second is only a starting point, adjust it to the device timeouts |
| `RradiusAccountingSpoolBacklog` | The spool has held accounting for more than 15 minutes, so the database or the cluster has a problem; it replays by itself after recovery, see [Chapter 13](13-accounting.md#the-accounting-spool) |
| `RradiusAccountingSpoolLoss` | Some records were rejected or found corrupted during replay and did not reach the database; look at `rejected.log` in the spool directory |
| `RradiusAccountingLogDropping` | Some requests were not written to the accounting log file: the disk is too slow, full or not writable (the metrics exist only when `[radius.accounting_log]` is set) |
| `RradiusExternalDatabaseUnreachable` | An external database with `required_for_ready` cannot be reached; the `/readyz` of this node answers 503 and the load balancer moves it out. Check the database and the network |
| `RradiusExternalDatabaseFailing` | Calls to an external database fail (`reason="unavailable"`): the database is down or slow, or the network is broken |
| `RradiusExternalDatabasePoolFull` | The connection pool has been full for more than 5 minutes and calls are queueing: queries got slow, or `pool_size` is too small |
| `RradiusNamedQueryStale` | A named query has not succeeded for more than 2 minutes, so numbers that depend on it (for example address pool usage) are old; look at `RradiusNamedQueryFailing` and the log |
| `RradiusNamedQueryFailing` | The `reason` label is `timeout`, `error` or `shape` (the columns of the result do not match the labels and value); see [Chapter 09](09-external-databases.md#named-queries-turn-sql-into-metrics) |
| `RradiusReloadFailed` | The node keeps running with the old configuration; the log says why; fix it and reload again |
| `RradiusLogLinesDropped` | The place the log goes to is too slow, see [When log lines are dropped](#when-log-lines-are-dropped) |
| `RradiusEapSessionsLost` | More 802.1X conversations at the same time than the node keeps; some were pushed out or refused; see [Chapter 21](../manual.zh-TW/21-troubleshooting.md#eap-卡住或失敗) |

**Thresholds are a starting point, not the right answer.** The rules were written from the meaning of the metrics and from synthetic data, and were not calibrated on your traffic. After going live, look at a few weeks of real numbers, and then adjust the `for` times and the ratio and time thresholds such as `RradiusManyRejects` and `RradiusSlowLogins`.

### Turning alerts into SNMP traps

`snmp_notifier` (a separate open-source project, <https://github.com/maxwo/snmp_notifier>, not part of rradius) receives the Alertmanager webhook and sends one SNMP trap for each group of alerts. The steps below come from `deploy/monitoring/README.md` and the earlier version of the chapter. The flags and behavior of `snmp_notifier` are **not verified on real hardware**; go by its own `--help`.

1. Put the `route` and `receivers` of `alertmanager.yml` into your Alertmanager configuration. The receiver sends alerts to `http://snmp-notifier:9464/alerts`, and also sends them when resolved (`send_resolved: true`).
2. Start `snmp_notifier`, pointing at the monitoring system that receives traps:

   ```bash
   snmp_notifier --snmp.destination=nms.example.net:162 --snmp.community=<community string> \
     --trap.default-oid=<your enterprise number>.1 \
     --trap.description-template=/etc/snmp_notifier/description-template.tpl
   ```

3. Its default trap OID (`1.3.6.1.4.1.98789.1`) is not yours; replace it with a subtree under your own enterprise number. When an alert has an `oid` label (or you name the label with `--trap.oid-label`), that alert uses this value as the trap OID. `rradius.rules.yml` currently sets no `oid` label; to give important alerts different OIDs, add it to the `labels` of the rule yourself.
4. **The v2c community string is plain text.** For authentication and encryption use v3: `--snmp.version=V3` with the `--snmp.authentication-*` and `--snmp.private-*` options.

A received trap looks like this (output of `snmptrapd`, tidied up, taken from the earlier version of the chapter):

```
iso.3.6.1.6.3.1.1.4.1.0 = OID: iso.3.6.1.4.1.98789.1
iso.3.6.1.4.1.98789.2.1 = STRING: "1.3.6.1.4.1.98789.1[alertname=RradiusDown,instance=a:9812]"
iso.3.6.1.4.1.98789.2.2 = STRING: "critical"
iso.3.6.1.4.1.98789.2.3 = STRING: "1/1 alerts are firing:
  Status: critical
  - Alert: RradiusDown
    Summary: rradius on a:9812 cannot be scraped"
```

To make the monitoring system understand it, set the trap OID and the three fields (name, severity, description) as its trap rule.

### Why there is no SNMP polling

A built-in SNMP agent would have to handle SNMP encoding and (for v3) authentication and encryption inside rradius, which is a new attack surface, and there is no usable dependency (the project rule is to add no dependencies). The standard RADIUS server MIBs (RFC 4669, RFC 4671) also have only a few counters per client, and can answer far fewer questions than the Prometheus metrics. If a system that only does SNMP polling really has to be connected, a workable approach later is to let rradius connect through AgentX (RFC 2741) to net-snmp on the host and hang the metrics under one MIB subtree, leaving authentication and encryption to `snmpd`. This has not been done; it needs a clear requirement before it is evaluated.

### What was verified

The following is recorded from the earlier version of the chapter and was not rerun this time:

- **Rules:** `promtool check rules` passed; every `rradius_*` metric name used by the 19 rules was checked against `/metrics` of a running server or the source code; `promtool test rules` used 9 synthetic time series to verify the alerts that should and should not fire.
- **Notification:** `amtool check-config` passed; the path of Alertmanager sending a webhook to `snmp_notifier` and then a v2c trap to `snmptrapd` was tested locally in containers.
- **Not verified:** the thresholds of the rules under real traffic, SNMP v3 traps, a real monitoring system (Zabbix, a traditional NMS) as the receiver, and reading rradius with `snmpwalk` (not supported).

## Live state with status

`status` reads `[metrics]` from the configuration file, connects to the metrics port of this node, takes two samples one second apart, and computes per-second rates:

```console
$ rradiusd status --config rradius.toml
rradius status of 127.0.0.1:9812 over 1.0 s

Node
  Ready                     ok
  Leader                    1
  Has a leader              1
  Caught up                 1
  Applied index             5
  Seconds without a leader  0
  Version                   0.1.0
  Seconds running           41

Reload
  Successful                1
  Failed                    0
  Last success (Unix time)  1795384693

RADIUS
  Auth requests              0.0/s (8)
  Acct requests              0.0/s (1)
  Access-Accept              0.0/s (2)
  Access-Reject              0.0/s (6)
  Access-Challenge           0.0/s (0)
  Accounting-Response        0.0/s (1)
  Duplicates                 0.0/s (0)
  Discarded: overload        0.0/s (0)
  Discarded: unknown client  0.0/s (0)
  Discarded: invalid         0.0/s (1)
  Discarded: database        0.0/s (0)
  Discarded: no leader       0.0/s (0)
  In bytes                   0.0 B/s (396 B)
  Out bytes                  0.0 B/s (128 B)
  Auth in progress           0
  Auth limit                 4096
  Acct in progress           0
  Acct limit                 4096

Queries
  accounts_by_state last success (seconds ago)  2
  accounts_by_state rows                        2
  accounts_by_state failed runs                 0.0/s (0)

TACACS+
  Open connections  0
  Authentication    0.0/s (1)
  Authorization     0.0/s (0)
  Accounting        0.0/s (0)
  Replies           0.0/s (1)
  Malformed         0.0/s (0)
  In bytes          0.0 B/s (46 B)
  Out bytes         0.0 B/s (28 B)
…
```

- `0.0/s (8)`: the first number is the rate over this second, and the number in parentheses is the total since the start.
- Byte items such as `In bytes` and `Out bytes` are shown as `58.6 KiB/s (1.0 MiB)`: the units are B, KiB, MiB, GiB and TiB, each 1024 times the one before (unlike the 1000-based units common on network interfaces and cloud bills), the rate has one decimal place, and the total is a whole number in B. `--json` and `/metrics` do no conversion: `rate` is bytes per second and `total` is bytes, for programs to read.
- When there is no traffic in this second, the p50 and p99 lines of RADIUS and TACACS+ do not appear.

Send traffic while you look, and the response times appear:

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --secret "$S" \
    --user alice --password alice-password --load pap,rate=200 --warmup 0 --duration 4 > /dev/null &
$ rradiusd status --config rradius.toml
…
RADIUS
  Auth requests              200.5/s (414)
  Acct requests              0.0/s (1)
  Access-Accept              200.5/s (408)
…
  Auth p50 (ms)              0.15
  Auth p99 (ms)              0.25
…
```

p50 and p99 are interpolated from the increase in each histogram bucket during this second, in milliseconds. The buckets have a limited width, so this is an estimate, not the actual order of each request.

### Flags and exit codes

Usage: `rradiusd status [--config <path>] [--cluster] [--watch | --json | --tui]`.

| Flag | Effect |
| --- | --- |
| `--config` | The configuration file, default `/etc/rradius/rradius.toml` |
| `--watch` | Print once a second until you press Ctrl+C (exit code 0). When the output goes to a terminal, it clears the screen first |
| `--json` | Print JSON instead, one object each time |
| `--cluster` | Look at all members of the cluster, see [Looking at the whole cluster](#looking-at-the-whole-cluster) |
| `--tui` | A full-screen interactive view, see [Full-screen view](#full-screen-view); it cannot be combined with `--json` or `--watch` |

| Exit code | Meaning |
| --- | --- |
| 0 | The result was printed |
| 2 | A bad argument, the configuration file cannot be read, or the configuration has no `[metrics]` |
| 3 | The metrics port cannot be reached; with `--cluster`, not one member could be reached |

With `--watch`, failing to connect does not end the command; it retries every second and prints the error. If the server restarts between two samples, the counters get smaller, and one extra line appears below the first line: `counters reset since the previous sample: the server restarted`.

### Full-screen view

`status --tui` draws the sections above on a full screen and updates every second, so you do not have to run the command again and again. It draws exactly the sections that `status` prints, so it has no data source of its own, and sections added by the enterprise edition or later are added as a page automatically.

| Key | Effect |
| --- | --- |
| Left and right arrows, Tab | Change section (wraps around at the end) |
| Up and down arrows | Select an item in the section; an item with a rate is drawn as a line chart below |
| `[`, `]` | When used with `--cluster`, select the previous or next member; the starting selection is "all", which shows the sections summed over all members below |
| `p` | Pause and resume: while paused the screen does not move but sampling keeps accumulating, and after resuming the chart has the points from this time |
| `q`, Esc, Ctrl+C | Quit; the terminal returns to its original screen and cursor |

Each section is drawn as an item table (name, rate per second or value, total), and the selected item is the highlighted row. When an item with a rate is selected, below the table is its line chart for the last 60 seconds: the horizontal axis runs from 60 seconds ago to now, and the vertical axis from 0 to the maximum rate in this period (at least 1). The history lives only in the memory of the running screen, so at first there are only the samples already taken; a sample where a server restart reset the counters is recorded as 0. Items without a rate (version, status text) have only the table and no chart. Sections added by the enterprise edition automatically get the same table and chart, with no extra setup.

When used with `--cluster`, there is a member table at the top. The first row "all" is the sum over all members, followed by one row per member (node, role, `/readyz`, logins and accounting per second, requests being handled). When "all" is selected, each rate and total below is the sum over the members that answered, and the chart is the summed rate too. A member that cannot be reached is marked `unreachable` in the table with the reason, the screen keeps updating, and it is not counted in the sum; when it is selected, the part below says it did not answer.

`--tui` is a feature of every edition and does not need to be compiled in separately. When standard output is not a terminal (redirected to a file or a pipe), it exits with 2 and writes nothing.

### JSON output

```console
$ rradiusd status --config rradius.toml --json
{"address":"127.0.0.1:9812","counter_reset":false,"interval_seconds":1.003252166,"ready":{"status":200,"text":"ok"},"sections":[{"items":[{"key":"ready","label":"Ready","value":"ok"},…
```

(The real output is one single line; it is abridged here. The keys of an object are sorted alphabetically.)

- `sections` is an array, and each element has `key`, `title` and `items`.
- Each item has `key`, `label`, and one of two sets: `value` (the current value) or `rate` plus `total` (rate and total). For example `{"key":"auth_requests","label":"Auth requests","rate":200.65753617774516,"total":618}`.
- When a program reads it, use `key`, not `label`. In this measurement the section keys were, in order, `node`, `reload`, `radius`, `radius_top_clients`, `queries`, `tacacs`, `eap`, `acct_spool`, `acct_log`, `netlogon`, `log`.

## The sections of status

The sections appear in a fixed order, and **a section or line with no matching metric is not printed**. For example, without `[radius.accounting_log]` there is no Accounting log section; with a configuration that runs only TACACS+, the RADIUS section has no in progress and limit lines.

| Section (key) | Content | Main chapter |
| --- | --- | --- |
| Node (`node`) | The `/readyz` answer, whether this node is the leader, whether there is a leader, whether it has caught up, which entry has been applied, seconds without a leader, version, seconds running | 16 |
| Reload (`reload`) | The number of successful and failed reloads, the Unix time of the last success | 18 |
| RADIUS (`radius`) | Rates of requests and responses, retransmits, each kind of drop, in progress and limit, p50/p99 | this chapter, 05, 19 |
| Busiest RADIUS clients (`radius_top_clients`) | The top five clients by responses in this second; those with no response in this second are not listed | 05 |
| External databases (`databases`) | Connections of each external database, each kind of failure, whether it is reachable | 09 |
| Queries (`queries`) | How long ago each named query succeeded, how many rows, how many failures | 09 |
| TACACS+ (`tacacs`) | Open connections, each kind of request, requests that could not be decoded, p50/p99 | 12 |
| EAP (`eap`) | Conversations in progress | 11 |
| Accounting spool (`acct_spool`) | Bytes waiting to be replayed, replays that succeeded, rejected, failures to write to the spool | 13 |
| Accounting log (`acct_log`) | Written, dropped, open failures, write failures | 13 |
| Netlogon (`netlogon`) | Connections to the domain controller and login results | 08 |
| Log (`log`) | Dropped log lines | this chapter |

When an external database cannot be reached, the Ready line of Node shows the text of `/readyz` directly:

```console
$ rradiusd status --config rradius.toml
…
Node
  Ready                     database billing unreachable
…
External databases
  billing connections in use      0
  billing connections idle        0
  billing connections max         8
  billing failures (unavailable)  0.0/s (0)
  billing failures (mistake)      0.0/s (0)
  billing failures (unknown)      0.0/s (0)
  billing reachable               no
```

> **Enterprise edition:** The `status` of the enterprise edition has more sections, for RadSec, proxying and dynamic clients.

## Looking at the whole cluster

`status --cluster` asks the cluster for its member list, then samples every member at the same time, one line per member:

```console
$ rradiusd status --config rradius.toml --cluster
rradius cluster status over 1.0 s, members from the cluster

node   role    ready  auth/s  acct/s  discarded/s  in progress  address
1      leader  ok     0.0     0.0     0.0          0            127.0.0.1:9812
total          1/1    0.0     0.0     0.0          0
```

- The member list is requested from the cluster's API port with `cluster.secret_api`. When no node's API answers, it falls back to `cluster.nodes` in the configuration file, and the first line says `members from this configuration: no node's api answered`.
- The metrics address of each member is the host of its API address plus the `[metrics]` port of **this** node. So the `[metrics]` of every node must use the same port, and `bind` must be an address this host can reach. Members that cannot be reached are listed below the table with the reason, together with the hint `every node's [metrics] bind has to be an address this host can reach, on the same port as here`.
- `role` is `leader`, `follower` or `learner`. The `total` line adds up the members that could be reached; `1/1` is how many could be reached out of how many there are.
- A member reached for the first time has no rate until the next sample; with `--watch`, the list is requested again every 10 seconds.
- With `--json`, the `status` field of each member is the content of the single-node `status --json`, and the outer object also has `members_from`, `nodes` and `total`.

> **Not verified on real hardware:** The output above has only one node. The table with several nodes and the case of unreachable members are based on `rradius-core/src/status.rs` and `specs/server-status` (Status of the Whole Cluster). Setting up a multi-node cluster is in [Chapter 16](16-cluster.md).

## Adjusting the log

The server writes all messages to stderr and writes nothing to standard output (stdout). Under systemd they go to the journal, and in a container they go to the container's log.

| Key | Default | Reload | Meaning |
| --- | --- | --- | --- |
| `log.level` | `info` | restart | `error`, `warn`, `info` or `debug` |
| `log.format` | `text` | restart | `text` or `json` |
| `log.auth` | `true` | restart | Write one authentication record for every authentication decision |

Changing `[log]` itself needs a restart, but `[log.trace]` under it takes effect on reload. Measured, changing `format` at reload:

```text
… WARN rradius_core::reload: configuration reloaded from rradius.toml; these need a restart to apply: [log]
```

**The level governs only rradius's own messages.** Other libraries (for example the cluster database) are fixed to write only `warn` and above, even with `level = "debug"`, so that you are not drowned in internal messages.

**text and json:** in text, each line is time, level, source (target) and message; in json, each line is an object with `timestamp`, `level`, `target` and `message`, and other fields flattened at the same level. The same startup message in the two formats, measured:

```text
2026-11-22T21:57:39.189728Z  INFO rradius_core::cli: Servers started. Press Ctrl+C to stop.
{"timestamp":"2026-11-22T21:59:07.062005Z","level":"INFO","message":"Servers started. Press Ctrl+C to stop.","target":"rradius_core::cli"}
```

To have a log collection system parse the log, use json.

**Outside values cannot break a line.** In values that come from outside, such as a user name, newlines and control characters are turned into escape sequences. A record always takes exactly one line, and nobody can forge a fake record with a user name.

**The same event does not flood the log.** RADIUS packets from unknown clients and TACACS+ connections from unknown clients are logged at most one line per second, and the next line says how many happened in between; the counters count every one. Overload works the same way, measured:

```text
… WARN rradius_core::radius: RADIUS authentication overloaded: discarded 252 packets
```

## Authentication records

With `log.auth = true` (the default), one record is written for every RADIUS or TACACS+ authentication decision, with target `rradius_auth`, not affected by `level`: it is written even with `level = "error"`. Packets for which no decision was made (retransmits, overload drops, failed validation) are not written.

```text
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="bob" account="bob" chain="default" handler="sql" decision="reject"
… INFO rradius_core::auth_chain: auth lockout: client localhost: account "bob" from "" is locked (this origin)
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="bob" account="bob" chain="default" handler="lockout" decision="reject"
… INFO rradius_auth: authentication protocol="tacacs" client="lab-switch" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="accept"
```

The same record in json format:

```text
{"timestamp":"2026-11-22T21:59:25.940382Z","level":"INFO","message":"authentication","protocol":"radius","client":"localhost","source":"127.0.0.1","user":"alice","account":"alice","chain":"default","handler":"sql","decision":"accept","target":"rradius_auth"}
```

| Field | Meaning |
| --- | --- |
| `protocol` | `radius` or `tacacs` |
| `client` | The name of the client that matched |
| `source` | The source address of the packet |
| `user` | The user name in the request; an empty string when there is none |
| `account` | The account the auth chain actually looked up after normalization (after removing the realm and lowercasing) |
| `chain` | The name of the auth chain; the one in `[auth]` is called `default`; `script` when there is no auth chain and a script decides |
| `handler` | The handler that made the decision, for example `sql`, `lua`, `ldap`; `none` when every handler was skipped; `normalization` when the name was rejected by the auth chain; `lockout` when the account is locked |
| `decision` | `accept`, `reject` or `challenge` |

The record has only these fields. It does not contain passwords, CHAP or MS-CHAPv2 responses, or the TACACS+ `data`.

When you do not need authentication records (for example because accounting data is available elsewhere, or the log volume is too large), set `log.auth = false`. Measured: after turning it off, a successful login no longer produces a `rradius_auth` line.

> **Note:** When the log cannot be written, lines are dropped (see [When log lines are dropped](#when-log-lines-are-dropped)), so authentication records cannot be treated as a complete audit record. For a record with nothing missing, use the accounting data or the accounting log file of Chapter 13.

## Tracing a specific user or client

To see the full request and response of every login of one user, for example to confirm that a reply attribute was sent, turn on `[log.trace]`. It always has a time limit and stops by itself when the time is up.

| Key | Default | Reload | Meaning |
| --- | --- | --- | --- |
| `log.trace.users` | `[]` | yes | The `User-Name` values to trace, which must match exactly |
| `log.trace.clients` | `[]` | yes | The names of the RADIUS clients to trace, which must be the `name` in `[[radius.clients]]` |
| `log.trace.minutes` | none, required | yes | How long to trace, 1 to 1440 minutes |

`users` and `clients` cannot both be empty. A match on either one is recorded. The trace covers only RADIUS, and `check` rejects it when the configuration has no `[radius]`.

**Start tracing:** add this to `rradius.toml` (before `[metrics]`, or in a file in `conf.d/`):

```toml
# rradius.toml
[log.trace]
users = ["alice"]
minutes = 10
```

Then `check` and send SIGHUP:

```console
$ rradiusd check --config rradius.toml
configuration ok
$ kill -HUP <pid>
```

The server's log:

```text
… WARN rradius_core::radius: [log.trace] is on for 10 minutes: requests of these users and clients are written out in full, personal data included
… INFO rradius_core::reload: configuration reloaded from rradius.toml
```

**See the result:** let `alice` log in once, then let `carol`, who is not on the list, log in once:

```text
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="accept"
… INFO rradius_trace: request client="localhost" source=127.0.0.1 user="alice" code=1 attributes="Message-Authenticator = <hidden>, User-Name = alice, User-Password = <hidden>, NAS-IP-Address = 192.0.2.1, NAS-Identifier = rradius-bench"
… INFO rradius_trace: response client="localhost" source=127.0.0.1 user="alice" code=2 attributes="Message-Authenticator = <hidden>, Session-Timeout = 3600, Filter-Id = staff"
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="carol" account="carol" chain="default" handler="none" decision="reject"
```

- Each matching request gives two lines, `request` and `response`, with target `rradius_trace`, not affected by `level`. When no response was sent, the second line is `no response`.
- `code` is the RADIUS packet code: 1 is Access-Request and 2 is Access-Accept.
- `alice` received `Session-Timeout = 3600` and `Filter-Id = staff`, which are exactly the reply attributes you could not see with `rradius-bench` in Chapter 03.
- `carol` is not on the list, so she has only the authentication record and no trace.

**Which values are hidden:** these attributes are always shown as `<hidden>`:

- Attributes marked with the `encrypt` or `secret` option in the dictionary.
- Attributes whose name (case-insensitive) contains `pass`, `secret`, `key`, `token`, `chap`, `arap`, `-pw`, `-cpw`, `digest`, `signature`, `psk`, `pmk` or `credential`.
- The standard attributes numbered 2, 3, 24, 60, 69, 70, 79 and 80 (User-Password, CHAP-Password, State, CHAP-Challenge, ARAP-Password, Tunnel-Password, EAP-Message, Message-Authenticator), whatever name the dictionary gives them.

Other values are written as they are, each at most 128 characters (binary values at most 64 bytes, shown in hexadecimal).

> **Warning:** A trace writes personal data such as user names, MAC addresses and IP addresses into the log in full, and that data goes wherever the log goes. Turn it on only when needed, turn it off when done, and keep `minutes` short.

**How the time is counted:** `minutes` starts counting again from when the configuration is loaded or from **every** reload. If you reload for another reason while a trace is on, the countdown also starts over, and the log prints `[log.trace] is on for 10 minutes` again.

**End it early:** delete `[log.trace]` and reload.

## When log lines are dropped

The log is written by a background thread. If stderr is written too slowly (for example the receiver is stuck) and the queue is full, the server prefers to drop log lines rather than make requests wait. The number of dropped lines, and of lines that failed to write, is recorded in `rradius_log_dropped_lines_total`; `RradiusLogLinesDropped` in the rules file alerts when it increases.

If you see it increase, the place the log goes to cannot keep up, for example the journal or the log collector is too slow, or the disk is full. The authentication records and traces of this period may be incomplete.

When SIGTERM or Ctrl+C makes the server prepare to exit, it waits at most 5 seconds for the log in the queue to be written.

> **Not verified on real hardware:** In the measurements for this chapter, `rradius_log_dropped_lines_total` stayed at 0, and no blocked-log situation was produced. The behavior is based on `rradius-core/src/logging.rs` and `specs/server-logging` (Log Output Never Delays Requests).

## Common mistakes

**Symptom:** `status` prints `status needs [metrics]: the command reads this node's /metrics. Add to rradius.toml:`, followed by a suggested configuration, and exit code 2.
**Cause:** The configuration file has no `[metrics]`.
**Fix:** Add `[metrics]` and restart the server (`[metrics]` needs a restart to take effect).

**Symptom:** `status` prints `cannot reach the metrics port at 127.0.0.1:9812: Connection refused (os error 61)`, exit code 3.
**Cause:** The server is not running, or the configuration file `status` reads is not the one the server uses, so the port differs. The error code number differs by operating system.
**Fix:** Make sure the server is running and `--config` points to the same configuration.

**Symptom:** `/readyz` keeps answering 503 `database billing unreachable`, but logins work.
**Cause:** `[databases.billing]` has `required_for_ready = true`, and this database cannot be reached.
**Fix:** Fix the connection to the database; if this database does not affect logins, remove `required_for_ready` so that it does not affect the readiness state (needs a restart).

**Symptom:** The log shows `reload failed, keeping the running configuration: rradius.toml: log.trace.minutes: 0 is not from 1 to 1440`, and `Failed` in the Reload section of `status` goes up by 1.
**Cause:** The new configuration has an error at reload, and the server keeps using the original configuration.
**Fix:** Run `check` before reloading. `minutes` must be between 1 and 1440.

**Symptom:** `check` prints `log.trace.minutes is required: tracing has to end by itself`.
**Cause:** `[log.trace]` has no `minutes`.
**Fix:** Add `minutes`.

**Symptom:** `check` prints `log.trace lists no users and no clients`.
**Cause:** `users` and `clients` are both empty.
**Fix:** Write at least one user or client.

**Symptom:** `check` prints `log.trace.clients: 'nobody' is not the name of a RADIUS client`.
**Cause:** `clients` takes the `name` of a client, not an address.
**Fix:** Change it to a name in `[[radius.clients]]`.

**Symptom:** `check` prints `log.trace needs [radius]: it only covers RADIUS`.
**Cause:** The configuration has only `[tacacs]`. The trace covers only RADIUS.
**Fix:** For TACACS+ problems, look at the authentication records and the `debug` level log.

**Symptom:** `check` prints `log.level: 'loud' is not error, warn, info or debug`.
**Cause:** The level is misspelled.
**Fix:** Change it to one of the four values.

**Symptom:** After changing `[log]` and reloading, the log says `these need a restart to apply: [log]`, and the format did not change.
**Cause:** `[log]` needs a restart to take effect (`[log.trace]` is the exception).
**Fix:** Schedule a restart.

**Symptom:** `curl -I http://…/metrics` gets `405 Method Not Allowed`.
**Cause:** The metrics port accepts only `GET`, and `-I` sends `HEAD`.
**Fix:** Use `curl -s` to fetch the content. Set the health checks of the monitoring system to `GET` too.

## Related chapters

- [03 First start and first login](03-quick-start.md)
- [04 Configuration file basics](04-configuration.md)
- [07 User accounts and auth chains](07-users-and-chains.md)
- [09 External databases and named queries](09-external-databases.md)
- [13 Accounting and sessions](13-accounting.md)
- [16 Clusters and high availability](16-cluster.md)
- [18 Day-to-day operations](18-operations.md)
- [19 Performance tuning and load testing](../manual.zh-TW/19-performance.md)
- [21 Troubleshooting](../manual.zh-TW/21-troubleshooting.md)
- [Appendix A: Configuration reference](../manual.zh-TW/90-config-reference.md)
- [Appendix C: Metrics reference](../manual.zh-TW/92-metrics-reference.md)
