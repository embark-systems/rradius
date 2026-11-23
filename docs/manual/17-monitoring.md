# 17 監控、狀態與日誌

> 本章讓你做到：
> - 開啟指標埠，知道 `/metrics`、`/livez`、`/readyz` 各自什麼時候回什麼
> - 讓 Prometheus 抓指標，套用 repo 附的告警規則
> - 用 `status` 看一台或整個叢集的即時狀態，也能拿 JSON 給其他程式讀
> - 調整日誌的等級與格式，讀懂每一筆認證紀錄
> - 在不重啟的情況下，針對特定使用者或用戶端開一段時間的 trace

前置條件：做完[第 03 章](03-quick-start.md)，手上有能跑的 quick-start 情境，以及 `rradius-server`、`rradius-bench` 兩個指令。每個指標的完整說明在[附錄 C](92-metrics-reference.md)。

本章小節：

- [概念：三種看伺服器的方式](#概念三種看伺服器的方式)
- [準備實驗環境](#準備實驗環境)
- [開啟指標埠](#開啟指標埠)
- [存活與就緒](#存活與就緒)
- [讀懂 /metrics](#讀懂-metrics)
- [讓 Prometheus 抓指標](#讓-prometheus-抓指標)
- [告警規則與 Alertmanager](#告警規則與-alertmanager)
- [用 status 看即時狀態](#用-status-看即時狀態)
- [status 的各個區段](#status-的各個區段)
- [看整個叢集](#看整個叢集)
- [調整日誌](#調整日誌)
- [認證紀錄](#認證紀錄)
- [追蹤特定使用者或用戶端](#追蹤特定使用者或用戶端)
- [日誌行被丟棄的時候](#日誌行被丟棄的時候)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

## 概念：三種看伺服器的方式

rradius 給你三種觀察它的管道，用途不一樣：

| 管道 | 從哪裡看 | 適合做什麼 |
| --- | --- | --- |
| 指標埠的 HTTP | `/metrics`、`/livez`、`/readyz` | 給 Prometheus 長期收集、畫圖、告警；給負載平衡器或 Kubernetes 判斷要不要送流量 |
| `status` 指令 | 讀同一個指標埠，整理成一頁 | 人在終端機前，想馬上知道「現在每秒幾筆、有沒有在丟封包」 |
| 日誌 | 伺服器的標準錯誤輸出（stderr） | 查某一次登入為什麼被拒、reload 為什麼失敗；trace 看完整的請求與回應 |

三者的關係：`status` 沒有自己的資料來源，它就是讀 `/metrics` 和 `/readyz`。所以沒有 `[metrics]` 就沒有 `status`。日誌和指標彼此獨立：日誌寫不出去時伺服器照樣回應請求，只是在指標裡記下丟了幾行。

指標都是**每個節點各自計算**、從行程啟動起累計的。reload 不會歸零，重新啟動才會。叢集裡要看總量，就把每個節點的數字加起來；`status --cluster` 幫你加好了。

## 準備實驗環境

本章用預設埠：RADIUS 1812、1813，指標 9812。TACACS+ 跟[第 12 章](12-tacacs.md)一樣用 4949，因為標準埠 49 小於 1024，一般帳號不能監聽。

從 quick-start 複製一份，換掉範例密碼（做法同第 03 章）：

```console
$ cp -R rradius-server/examples/scenarios/quick-start ~/rradius-lab/monitoring
$ cd ~/rradius-lab/monitoring
$ for f in raft api localhost; do openssl rand -hex 32 > $f.secret; done
$ chmod 600 *.secret
```

字典路徑照第 03 章改成絕對路徑。quick-start 的 `[radius]` 與 `[metrics]` 已經是預設埠，不用改。

為了讓 `status` 的每個區段都有東西看，在檔案最後再加上 TACACS+、登入失敗鎖定、計費記錄檔與一個具名查詢。它們各自的用法在別章，這裡只是借來產生指標：

```toml
# rradius.toml（加在檔案最後）
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

- `[auth.lockout]` 見[第 07 章](07-users-and-chains.md)，`[radius.accounting_log]` 見[第 13 章](13-accounting.md)，`[tacacs]` 見[第 12 章](12-tacacs.md)，`[queries]` 見[第 09 章](09-external-databases.md)。
- TACACS+ 用戶端借用 `localhost.secret` 只是為了少建一個檔案，正式環境每個用戶端用自己的共用密碼。

```console
$ rradius-server check --config rradius.toml
configuration ok
$ rradius-server server --config rradius.toml
…
… INFO rradius_core::tacacs: TACACS+ Server listening on 127.0.0.1:4949
… INFO rradius_core::radius: RADIUS Server listening on 127.0.0.1:1812 and 127.0.0.1:1813
… INFO rradius_core::metrics: Metrics listening on 127.0.0.1:9812
```

在另一個終端機送一些流量：一次成功登入、四次錯誤密碼（第三次後 `bob` 被鎖）、一筆計費、一筆 TACACS+ 登入、一筆共用密碼錯誤的請求，以及一個 Status-Server。

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

共用密碼錯誤的那一筆，伺服器不回應，所以 `rradius-bench` 看到的是 `timeouts 1`。

## 開啟指標埠

設定檔裡寫了 `[metrics]`（即使是空的表），伺服器才會開這個 HTTP 埠：

| 鍵 | 預設 | reload | 意思 |
| --- | --- | --- | --- |
| `metrics.bind` | `127.0.0.1` | 要重啟 | 監聽的位址 |
| `metrics.port` | `9812` | 要重啟 | 監聽的 TCP 埠 |

完整的鍵表見[附錄 A](90-config-reference.md)。

這個埠很單純：

- 只接受 `GET`。其他方法回 405，包括 `HEAD`，所以 `curl -I` 會看到 405。
- 只有 `/metrics`、`/livez`、`/readyz` 三個路徑，其他回 404。網址後面的查詢字串（`?x=1`）不影響結果。
- 每次回答完就關閉連線。同時最多服務 16 條連線，多的直接關掉。
- 請求的標頭超過 8 KiB，或 5 秒內沒有送完，連線直接關掉；回答也要在 5 秒內被讀走。

實測：

```console
$ curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:9812/
404
$ curl -s -o /dev/null -w "%{http_code}\n" -X POST http://127.0.0.1:9812/metrics
405
$ curl -s -o /dev/null -w "%{http_code}\n" "http://127.0.0.1:9812/metrics?x=1"
200
```

> **警告：** 指標埠沒有帳號密碼，也沒有 TLS。任何連得到的人都能讀到用戶端名稱、具名查詢的結果這類資訊。`bind` 只綁內部網路的位址，並用防火牆限制只有 Prometheus 與管理主機連得到。綁 `0.0.0.0` 時，`status` 會改連本機的 loopback。

## 存活與就緒

`/livez` 和 `/readyz` 回的都是純文字，一行字加換行：

| 路徑 | 回答 | 什麼時候 |
| --- | --- | --- |
| `/livez` | 200 `ok` | 指標埠還能回答就是這個。停止中也是 200 |
| `/readyz` | 503 `stopping` | 收到 SIGTERM 或 SIGINT、開始停止之後，直到行程結束 |
| `/readyz` | 503 `no leader` | 設了 `cluster.stop_without_leader_ms`，而且沒有 leader 的時間超過它；這時登入請求不會被回應 |
| `/readyz` | 503 `database <名稱> unreachable` | 有 `required_for_ready = true` 的外部資料庫連不上；有好幾個時名稱依字母排序、用 `, ` 隔開 |
| `/readyz` | 200 `ok` | 以上都不是 |

同時有好幾種情況時，順序是：`stopping` 優先，其次 `no leader`，最後才是資料庫。沒有設 `stop_without_leader_ms` 的節點，沒有 leader 時 `/readyz` 仍然是 200，因為它還會用本機的資料回答登入（見[第 16 章](16-cluster.md)）。

**怎麼用：** 存活探測（liveness）用 `/livez`，失敗就重啟行程；就緒探測（readiness）用 `/readyz`，失敗就先別送流量過來，但不要重啟它。repo 裡 `deploy/kubernetes/statefulset.yaml` 就是這樣配的：`startupProbe` 與 `livenessProbe` 看 `/livez`，`readinessProbe` 看 `/readyz`。

**停止時：** 實測把 `[shutdown] ready_delay_ms` 設成 `2000` 再送 SIGTERM：

```console
$ kill -TERM <pid>
$ curl -s -w " HTTP %{http_code}\n" http://127.0.0.1:9812/readyz
stopping
 HTTP 503
$ curl -s -w " HTTP %{http_code}\n" http://127.0.0.1:9812/livez
ok
 HTTP 200
```

伺服器的日誌：

```text
… INFO rradius_core::cli: stopping: /readyz answers 503; requests are answered for 2000 ms more, then those being handled are waited for, for at most 5000 ms
… INFO rradius_core::cli: stopping: no more requests taken
… INFO rradius_core::cli: stopping: every request taken was answered
```

`ready_delay_ms` 這段時間裡，`/readyz` 已經是 503，但請求照常回答，讓負載平衡器有時間把流量移走。`[shutdown]` 的說明在[第 04 章](04-configuration.md)，停機的程序在[第 18 章](18-operations.md)。

**外部資料庫：** 在 `[databases.<名稱>]` 設 `required_for_ready = true`，伺服器每 5 秒用一條自己的連線對它執行 `SELECT 1`；連續兩次失敗就算連不上，一次成功就恢復。實測把一個不存在的 PostgreSQL（`127.0.0.1` 上一個沒有服務在聽的埠）設成必要：

```console
$ curl -s -w " HTTP %{http_code}\n" http://127.0.0.1:9812/readyz
database billing unreachable
 HTTP 503
```

啟動時連不上，日誌會先說一次：

```text
… WARN rradius_core::external_db: databases.billing: not reachable at startup: error communicating with database: Connection refused (os error 61); /readyz answers 503 until it is
```

外部資料庫的設定見[第 09 章](09-external-databases.md)。

> **未實機驗證：** `no leader` 這一種回答需要多節點叢集才能重現，本章沒有實際跑過；行為依據是 `rradius-core/src/metrics.rs` 與 `specs/server-metrics`（Health Endpoints）。

## 讀懂 /metrics

`/metrics` 回的是 Prometheus 的文字格式（`Content-Type: text/plain; version=0.0.4`）。每個指標族先有 `# HELP`、`# TYPE` 兩行，接著是樣本：

```console
$ curl -s http://127.0.0.1:9812/metrics | grep -A6 '^# HELP rradius_radius_responses_total'
# HELP rradius_radius_responses_total RADIUS responses sent, excluding replays and Status-Server.
# TYPE rradius_radius_responses_total counter
rradius_radius_responses_total{code="access_accept"} 1
rradius_radius_responses_total{code="access_reject"} 5
rradius_radius_responses_total{code="access_challenge"} 0
rradius_radius_responses_total{code="accounting_response"} 1
```

幾個讀的時候要知道的規則：

- **三種型別。** counter 只會增加，看它要用 `rate()` 或 `increase()`；gauge 是當下的值，可以上下；histogram 是回應時間的分布。
- **標籤組合從一開始就在。** 例如 `access_challenge` 即使一次都沒發生，也有一行 `0`。所以查詢不會因為某個值「第一次出現」而算錯速率。
- **有些指標族只在有對應設定時才出現。** 例如沒有 `[auth.lockout]` 就沒有 `rradius_auth_lockout_*`。出現的條件逐一列在[附錄 C](92-metrics-reference.md)。
- **用戶端名稱來自設定。** `rradius_radius_client_responses_total` 的 `client` 標籤是 `[[radius.clients]]` 的 `name`，不會因為流量而變多。reload 拿掉的用戶端，它的樣本也跟著消失。

本章負責的指標族：

| 指標族 | 看什麼 |
| --- | --- |
| `rradius_radius_*` | RADIUS 收了幾筆、回了什麼、丟了幾筆、為什麼丟、花多久、現在有幾筆在處理 |
| `rradius_tacacs_*` | TACACS+ 連線、各類請求、解不開的請求、回應時間 |
| `rradius_eap_*` | 進行中的 EAP 對話、對話怎麼結束 |
| `rradius_reload_*` | reload 成功與失敗的次數、最後一次成功的時間 |
| `rradius_build_info`、`rradius_start_time_seconds` | 版本、啟動時間 |
| `rradius_log_dropped_lines_total` | 丟掉的日誌行 |

其他指標族有各自的主章：叢集與 `rradius_db_*` 在[第 16 章](16-cluster.md)，計費 spool 與計費記錄檔在[第 13 章](13-accounting.md)，鎖定與密碼比對在[第 07 章](07-users-and-chains.md)，Netlogon 在[第 08 章](08-ldap-and-ad.md)，外部資料庫與具名查詢在[第 09 章](09-external-databases.md)，腳本的 HTTP 呼叫在[第 10 章](10-lua-policies.md)。

### 沒有回應的請求

排錯時最常看的是 `rradius_radius_discarded_total`，它記的是伺服器收到、但沒有回應的封包，用 `reason` 區分：

| `reason` | 意思 |
| --- | --- |
| `overload` | 同時處理中的請求已經達到 `max_auth_inflight` 或 `max_acct_inflight`（第 19 章） |
| `unknown_client` | 來源位址不屬於任何 `[[radius.clients]]` |
| `invalid` | 格式錯誤、驗證碼或 Message-Authenticator 不對，最常見的原因是共用密碼不一致 |
| `database` | 叢集資料庫無法使用，登入沒有回應 |
| `no_leader` | 設了 `stop_without_leader_ms`，沒有 leader 太久 |

上面送的那一筆錯誤共用密碼，就記在這裡：

```text
rradius_radius_discarded_total{reason="invalid"} 1
```

`rradius_radius_duplicates_total` 另外計算重送：NAS 沒等到回應又送一次同樣的請求，伺服器從重送快取回答或在處理中直接丟掉。它不算在 `discarded` 裡。

### 回應時間

`rradius_radius_response_seconds` 與 `rradius_tacacs_response_seconds` 是 histogram。RADIUS 依 `kind`（`auth`、`acct`）分，從通過重送檢查到送出回應為止；沒有回應的請求不算。TACACS+ 依 `type`（`authen`、`author`、`acct`）分，每個認證封包各算一次。

區間的上界（秒）固定是：

```text
0.00005 0.0001 0.00025 0.0005 0.001 0.0025 0.005 0.01 0.025 0.05 0.1 0.25 0.5 1 2.5 5 10 +Inf
```

從 50 微秒到 10 秒。本機登入通常落在 1 毫秒以下；超過 10 秒的，NAS 早就放棄了。`_sum` 的單位是秒。

## 讓 Prometheus 抓指標

repo 的 `deploy/monitoring/` 有告警規則與 Alertmanager 的範例。規則假設抓取工作（scrape job）叫 `rradius`，所以 Prometheus 的設定要這樣寫：

```yaml
# prometheus.yml（節錄）
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

- 叢集的每個節點都要列進去。指標是每個節點各自算的，漏掉一個就少一份。
- 每個節點的 `[metrics] bind` 要是 Prometheus 連得到的內部位址，不能留 `127.0.0.1`。

幾個常用的查詢，直接取自規則檔：

```promql
# 每台每秒的 Access-Reject 占 Accept＋Reject 的比例
sum by (instance) (rate(rradius_radius_responses_total{code="access_reject"}[10m]))
  / sum by (instance) (rate(rradius_radius_responses_total{code=~"access_accept|access_reject"}[10m]))

# 登入回應時間的 p99（秒）
histogram_quantile(0.99, sum by (le, instance) (rate(rradius_radius_response_seconds_bucket{kind="auth"}[5m])))

# 五分鐘內因過載丟掉的封包
increase(rradius_radius_discarded_total{reason="overload"}[5m])
```

> **未實機驗證：** 本章沒有接上真的 Prometheus 與 Grafana。上面的抓取設定與查詢，只對照了 `deploy/monitoring/` 的檔案與 `/metrics` 的實際輸出。

## 告警規則與 Alertmanager

`deploy/monitoring/rradius.rules.yml` 有 19 條規則，分成五組。只在有對應設定時才有的指標（外部資料庫、計費記錄檔、具名查詢），沒設定就不會出現，相關規則也不會觸發。

| 組 | 規則 | 條件（節錄） | 嚴重度 |
| --- | --- | --- | --- |
| availability | `RradiusDown` | `up{job="rradius"} == 0` 持續 2 分鐘 | critical |
| | `RradiusNoLeader` | `rradius_cluster_leader == 0` 持續 1 分鐘 | critical |
| | `RradiusNotCaughtUp` | `rradius_cluster_caught_up == 0` 持續 5 分鐘 | warning |
| traffic | `RradiusOverload` | 5 分鐘內有 `overload` 丟棄 | critical |
| | `RradiusDiscardingForTheDatabase` | 5 分鐘內有 `database` 或 `no_leader` 丟棄 | warning |
| | `RradiusManyRejects` | 10 分鐘內 Reject 超過一半，且每秒超過 0.2 筆 | warning |
| | `RradiusInvalidPackets` | 10 分鐘內持續有 `invalid` 丟棄 | warning |
| | `RradiusSlowLogins` | 登入 p99 超過 1 秒，持續 10 分鐘 | warning |
| accounting | `RradiusAccountingSpoolBacklog` | 計費 spool 有資料，持續 15 分鐘 | warning |
| | `RradiusAccountingSpoolLoss` | 1 小時內 spool 有被拒或損毀的紀錄 | critical |
| | `RradiusAccountingLogDropping` | 10 分鐘內計費記錄檔丟棄或寫入失敗 | warning |
| dependencies | `RradiusExternalDatabaseUnreachable` | `rradius_external_db_reachable == 0` 持續 2 分鐘 | critical |
| | `RradiusExternalDatabaseFailing` | 5 分鐘內有 `unavailable` 類的失敗 | warning |
| | `RradiusExternalDatabasePoolFull` | 使用中的連線等於上限，持續 5 分鐘 | warning |
| | `RradiusNamedQueryStale` | 具名查詢超過 120 秒沒有成功 | warning |
| | `RradiusNamedQueryFailing` | 10 分鐘內具名查詢失敗 | warning |
| operations | `RradiusReloadFailed` | 10 分鐘內 reload 失敗 | warning |
| | `RradiusLogLinesDropped` | 10 分鐘內丟掉日誌行 | warning |
| | `RradiusEapSessionsLost` | 10 分鐘內有 EAP 對話被擠掉（`evicted`）或拒絕（`overload`） | warning |

每條規則的完整條件與說明文字，以規則檔為準。每條規則對應哪個指標，附錄 C 的各指標也有寫。

`rradius.rules.test.yml` 是 `promtool test rules` 的測試案例：餵入假的時間序列，檢查哪些告警會在什麼時間觸發。改規則之後，在 `deploy/monitoring/` 裡跑：

```console
$ promtool check rules rradius.rules.yml
$ promtool test rules rradius.rules.test.yml
```

`alertmanager.yml` 把告警依 `alertname`、`instance` 分組，以 webhook 送到 `http://snmp-notifier:9464/alerts`，告警解除時也送（`send_resolved: true`）。`snmp_notifier` 是另一個獨立專案，負責把告警轉成 SNMP trap，給只收 trap 的網管系統。你已經有自己的 Alertmanager 設定時，把其中的 `route` 與 `receivers` 併進去即可。

> **未實機驗證：** 撰寫本章的機器沒有 `promtool`，上面兩個指令沒有實際執行；Alertmanager 與 SNMP trap 的轉送也沒有實際跑過。

### 三種接法

rradius 輸出的是 Prometheus 指標，**沒有內建 SNMP**。依你的監控系統選一種：

| 你的監控系統 | 做法 | 驗證過嗎 |
| --- | --- | --- |
| Prometheus 與 Alertmanager（或能收 Alertmanager webhook 的系統） | 載入 `rradius.rules.yml`，告警由 Alertmanager 發送 | 規則的語法與邏輯用 `promtool` 驗證（舊版章節的記載）；沒有接真實的通知管道 |
| 只收 SNMP trap | Alertmanager 的 webhook 送給 `snmp_notifier`，由它發 SNMP trap（v2c 或 v3） | 舊版章節記載：本機以 Alertmanager、`snmp_notifier`、`snmptrapd` 三個容器實測，`snmptrapd` 收到帶告警名稱、節點、嚴重程度與摘要的 v2c trap；v3 與真實監控系統沒有測。本次沒有重跑，**未實機驗證** |
| 能讀 HTTP 的監控系統（例如 Zabbix 的 HTTP agent 項目） | 直接讀 `GET /metrics`，挑要看的指標，自己設門檻；規則檔的門檻可當參考 | **未實機驗證** |
| 只會用 SNMP 輪詢（`snmpwalk` 要看得到 rradius 的數字） | **不支援** | 見[為什麼沒有 SNMP 輪詢](#為什麼沒有-snmp-輪詢) |

### 各告警怎麼處理

下表依 `rradius.rules.yml` 的說明文字與 `for` 設定整理。「持續」是規則的 `for`，沒寫表示條件成立就響。

| 告警 | 處理建議 |
| --- | --- |
| `RradiusDown` | 程序掛了，或 `[metrics]` 的位址從 Prometheus 連不到；看 systemd 或容器的狀態與日誌 |
| `RradiusNoLeader` | 節點被隔開，或叢集失去多數。節點不能寫：計費進 spool，需要寫入的登入被拒絕；見[第 16 章](16-cluster.md#沒有-leader-時) |
| `RradiusNotCaughtUp` | 本機副本落後叢集超過 5 分鐘：節點剛加入或剛重啟、資料多；沒有進展就看日誌 |
| `RradiusOverload` | 進來的請求超過節點處理得了的量：找變慢的資料庫、目錄或腳本；搭配 `RradiusSlowLogins` 與[第 21 章](21-troubleshooting.md) |
| `RradiusDiscardingForTheDatabase` | 資料庫不能回答，或沒有 leader 可寫；和 `RradiusNoLeader` 一起看 |
| `RradiusManyRejects` | 目錄故障、設定錯誤，或有人在試密碼；看日誌的拒絕原因 |
| `RradiusInvalidPackets` | 通常是某台設備的共用密碼設錯 |
| `RradiusSlowLogins` | 資料庫、目錄或腳本變慢；1 秒只是起點，依設備的逾時調整 |
| `RradiusAccountingSpoolBacklog` | spool 已存放計費超過 15 分鐘，資料庫或叢集有問題；恢復後會自己重放，見[第 13 章](13-accounting.md#計費-spool) |
| `RradiusAccountingSpoolLoss` | 有紀錄在重放時被拒或發現損毀，沒有進資料庫；看 spool 目錄的 `rejected.log` |
| `RradiusAccountingLogDropping` | 計費記錄檔有請求沒寫進去：磁碟太慢、滿了或不能寫（只在設了 `[radius.accounting_log]` 時有指標） |
| `RradiusExternalDatabaseUnreachable` | 設了 `required_for_ready` 的外部資料庫連不上；這個節點的 `/readyz` 回 503，負載平衡器會把它移出。查資料庫與網路 |
| `RradiusExternalDatabaseFailing` | 呼叫外部資料庫失敗（`reason="unavailable"`）：資料庫掛了、慢，或網路不通 |
| `RradiusExternalDatabasePoolFull` | 連線池用滿超過 5 分鐘，呼叫在排隊：查詢變慢，或 `pool_size` 太小 |
| `RradiusNamedQueryStale` | 具名查詢超過 2 分鐘沒有成功，依賴它的數字（例如位址池使用率）是舊的；看 `RradiusNamedQueryFailing` 與日誌 |
| `RradiusNamedQueryFailing` | `reason` 標籤是 `timeout`、`error` 或 `shape`（結果的欄位不符合標籤與數值），見[第 09 章](09-external-databases.md#具名查詢把-sql-變成指標) |
| `RradiusReloadFailed` | 節點照舊設定運作；日誌寫了原因，修好再重新載入 |
| `RradiusLogLinesDropped` | 日誌的去處太慢，見[日誌行被丟棄的時候](#日誌行被丟棄的時候) |
| `RradiusEapSessionsLost` | 同時進行的 802.1X 對話超過節點保留的量，有的被擠掉或被拒絕；見[第 21 章](21-troubleshooting.md#eap-卡住或失敗) |

**門檻是起點，不是標準答案。** 規則依指標的意義與合成資料寫成，沒有在你的流量上校正過：上線後看幾週的實際數字，再調 `for` 的時間，以及 `RradiusManyRejects`、`RradiusSlowLogins` 這類比例與時間的門檻。

### 把告警變成 SNMP trap

`snmp_notifier`（獨立的開源專案，<https://github.com/maxwo/snmp_notifier>，rradius 不包含它）接 Alertmanager 的 webhook，對每組告警發一個 SNMP trap。以下步驟取自 `deploy/monitoring/README.md` 與舊版章節，`snmp_notifier` 的旗標與行為**未實機驗證**，以它自己的 `--help` 為準。

1. 把 `alertmanager.yml` 的 `route` 與 `receivers` 放進你的 Alertmanager 設定。接收者把告警送到 `http://snmp-notifier:9464/alerts`，已解除時也送（`send_resolved: true`）。
2. 啟動 `snmp_notifier`，指向接收 trap 的監控系統：

   ```bash
   snmp_notifier --snmp.destination=nms.example.net:162 --snmp.community=<共同體字串> \
     --trap.default-oid=<你的企業編號>.1 \
     --trap.description-template=/etc/snmp_notifier/description-template.tpl
   ```

3. 它預設的 trap OID（`1.3.6.1.4.1.98789.1`）不是你的，要換成自己企業編號下的子樹。告警上有 `oid` 標籤（或用 `--trap.oid-label` 指定標籤名）時，那筆告警改用該值當 trap OID；`rradius.rules.yml` 目前沒有設 `oid` 標籤，要為重要告警指定不同的 OID，得自己在規則的 `labels` 加。
4. **v2c 的共同體字串是明文。** 要認證與加密就用 v3：`--snmp.version=V3` 加上 `--snmp.authentication-*`、`--snmp.private-*` 選項。

收到的 trap 長這樣（`snmptrapd` 的輸出，內容經過整理，取自舊版章節）：

```
iso.3.6.1.6.3.1.1.4.1.0 = OID: iso.3.6.1.4.1.98789.1
iso.3.6.1.4.1.98789.2.1 = STRING: "1.3.6.1.4.1.98789.1[alertname=RradiusDown,instance=a:9812]"
iso.3.6.1.4.1.98789.2.2 = STRING: "critical"
iso.3.6.1.4.1.98789.2.3 = STRING: "1/1 alerts are firing:
  Status: critical
  - Alert: RradiusDown
    Summary: rradius on a:9812 cannot be scraped"
```

要讓監控系統看得懂，把 trap 的 OID 與三個欄位（名稱、嚴重程度、說明）設成它的 trap 規則。

### 為什麼沒有 SNMP 輪詢

內建 SNMP agent 要在 rradius 裡處理 SNMP 的編碼和（v3 的）認證與加密，是新的攻擊面，也沒有可用的相依（專案規則是不新增相依）。標準的 RADIUS 伺服器 MIB（RFC 4669、RFC 4671）也只有每個 client 的幾個計數，能回答的問題比 Prometheus 指標少很多。若真有只會 SNMP 輪詢的系統必須接上，日後可行的做法是讓 rradius 透過 AgentX（RFC 2741）接到主機上的 net-snmp，把指標掛在一棵 MIB 子樹下，認證與加密交給 `snmpd`。這件事還沒做，要有明確需求再評估。

### 驗證過什麼

以下是舊版章節的記載，本次沒有重跑：

- **規則：** `promtool check rules` 通過；19 條規則用到的每個 `rradius_*` 指標名稱，都對照過執行中伺服器的 `/metrics` 或原始碼；`promtool test rules` 以 9 組合成的時間序列驗證該響與不該響的告警。
- **通知：** `amtool check-config` 通過；Alertmanager 送 webhook 給 `snmp_notifier`、再發 v2c trap 到 `snmptrapd` 這條路，在本機容器實測過。
- **沒有驗證：** 規則在真實流量下的門檻、SNMP v3 trap、真實的監控系統（Zabbix、傳統 NMS）當接收端、用 `snmpwalk` 讀 rradius（不支援）。

## 用 status 看即時狀態

`status` 讀設定檔的 `[metrics]`，連到這個節點的指標埠，相隔一秒取兩次樣本，算出每秒的速率：

```console
$ rradius-server status --config rradius.toml
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
  Malformed         0.0/s (0)
…
```

- `0.0/s (8)`：前面是這一秒的速率，括號裡是啟動以來的總數。
- 這一秒沒有流量時，RADIUS 與 TACACS+ 的 p50、p99 兩行不會出現。

一邊送流量一邊看，就會出現回應時間：

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --secret "$S" \
    --user alice --password alice-password --load pap,rate=200 --warmup 0 --duration 4 > /dev/null &
$ rradius-server status --config rradius.toml
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

p50、p99 是從這一秒內 histogram 各區間的增量內插出來的，單位毫秒。區間的寬度有限，所以它是估計值，不是每一筆的實際排序。

### 旗標與結束碼

用法：`rradius-server status [--config <path>] [--cluster] [--watch | --json | --tui]`。

| 旗標 | 作用 |
| --- | --- |
| `--config` | 設定檔，預設 `/etc/rradius/rradius.toml` |
| `--watch` | 每秒印一次，直到按 Ctrl+C（結束碼 0）。輸出到終端機時會先清畫面 |
| `--json` | 改印 JSON，每次一個物件 |
| `--cluster` | 看叢集的所有成員，見[看整個叢集](#看整個叢集) |
| `--tui` | 全螢幕互動畫面，見[全螢幕畫面](#全螢幕畫面)；不能和 `--json`、`--watch` 並用 |

| 結束碼 | 意思 |
| --- | --- |
| 0 | 印出了結果 |
| 2 | 參數錯誤、設定檔讀不到，或設定沒有 `[metrics]` |
| 3 | 連不到指標埠；`--cluster` 時是一個成員都連不到 |

加了 `--watch` 時連不上不會結束，而是每秒重試並印出錯誤。伺服器在兩次取樣之間重啟，計數器會變小，這時第一行下面會多一行 `counters reset since the previous sample: the server restarted`。

### 全螢幕畫面

`status --tui` 把上面各個區段畫在全螢幕裡，每秒更新，不必一直重新執行。它畫的就是 `status` 印出的那些區段，所以沒有自己的資料來源，企業版或之後新增的區段會自動多一頁。

| 按鍵 | 作用 |
| --- | --- |
| 左、右方向鍵、Tab | 換區段（到頭會繞回） |
| 上、下方向鍵 | 和 `--cluster` 一起用時選成員；下方顯示選到那一台的區段 |
| `q`、Esc、Ctrl+C | 離開，終端機回到原來的畫面與游標 |

和 `--cluster` 一起用時，上方有成員表（節點、角色、`/readyz`、每秒登入與計費、處理中的請求）。連不上的成員在表裡標 `unreachable` 並附原因，畫面繼續更新；選到它時，下方說明它沒有回答。

`--tui` 是每個版本都有的功能，不必另外編進。標準輸出不是終端機（導到檔案或管線）時以 2 結束，且不寫任何東西。

### JSON 輸出

```console
$ rradius-server status --config rradius.toml --json
{"address":"127.0.0.1:9812","counter_reset":false,"interval_seconds":1.003252166,"ready":{"status":200,"text":"ok"},"sections":[{"items":[{"key":"ready","label":"Ready","value":"ok"},…
```

（實際輸出是一整行；這裡刪節了。物件的鍵依字母排序。）

- `sections` 是陣列，每一項有 `key`、`title`、`items`。
- 每個 item 有 `key`、`label`，以及 `value`（當下的值）或 `rate`＋`total`（速率與總數）其中一組。例如 `{"key":"auth_requests","label":"Auth requests","rate":200.65753617774516,"total":618}`。
- 給程式讀的時候用 `key`，不要用 `label`。這次實測的區段 key 依序是 `node`、`reload`、`radius`、`radius_top_clients`、`queries`、`tacacs`、`eap`、`acct_spool`、`acct_log`、`netlogon`、`log`。

## status 的各個區段

區段依固定順序出現，**沒有對應指標的區段或行就不印**。例如沒有 `[radius.accounting_log]` 就沒有 Accounting log 區段；只跑 TACACS+ 的設定，RADIUS 區段沒有 in progress 與 limit 兩行。

| 區段（key） | 內容 | 主章 |
| --- | --- | --- |
| Node（`node`） | `/readyz` 的回答、是不是 leader、有沒有 leader、追上進度沒、套用到哪一筆、沒有 leader 幾秒、版本、跑了幾秒 | 16 |
| Reload（`reload`） | reload 成功與失敗的次數、最後一次成功的 Unix 時間 | 18 |
| RADIUS（`radius`） | 請求與回應的速率、重送、各種丟棄、處理中與上限、p50／p99 | 本章、05、19 |
| Busiest RADIUS clients（`radius_top_clients`） | 這一秒回應最多的前五個用戶端；這一秒沒有回應的不列 | 05 |
| External databases（`databases`） | 每個外部資料庫的連線數、各類失敗、連不連得到 | 09 |
| Queries（`queries`） | 每個具名查詢多久前成功、有幾列、失敗幾次 | 09 |
| TACACS+（`tacacs`） | 開著的連線、各類請求、解不開的請求、p50／p99 | 12 |
| EAP（`eap`） | 進行中的對話 | 11 |
| Accounting spool（`acct_spool`） | 等待重送的位元組、重送成功、被拒、寫不進 spool | 13 |
| Accounting log（`acct_log`） | 寫入、丟棄、開檔失敗、寫入失敗 | 13 |
| Netlogon（`netlogon`） | 對網域控制站的連線與登入結果 | 08 |
| Log（`log`） | 丟掉的日誌行 | 本章 |

外部資料庫連不上時，Node 的 Ready 那一行直接顯示 `/readyz` 的文字：

```console
$ rradius-server status --config rradius.toml
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

> **企業版：** 企業版的 `status` 另有 RadSec、轉送、動態用戶端等區段。

## 看整個叢集

`status --cluster` 向叢集要成員名單，然後同時對每個成員取樣，一個成員一行：

```console
$ rradius-server status --config rradius.toml --cluster
rradius cluster status over 1.0 s, members from the cluster

node   role    ready  auth/s  acct/s  discarded/s  in progress  address
1      leader  ok     0.0     0.0     0.0          0            127.0.0.1:9812
total          1/1    0.0     0.0     0.0          0
```

- 成員名單用 `cluster.secret_api` 向叢集的 API 埠要。所有節點的 API 都不回答時，改用設定檔的 `cluster.nodes`，第一行會寫 `members from this configuration: no node's api answered`。
- 每個成員的指標位址，是它 API 位址的主機加上**這台**的 `[metrics]` 埠。所以每個節點的 `[metrics]` 要用同一個埠，`bind` 也要是這台連得到的位址；連不上的成員會在表格下面列出原因，並提示 `every node's [metrics] bind has to be an address this host can reach, on the same port as here`。
- `role` 是 `leader`、`follower` 或 `learner`。`total` 那一行把連得到的成員加總，`1/1` 是連得到幾個、共有幾個。
- 第一次連上的成員要等到下一次取樣才有速率；`--watch` 時，名單每 10 秒重新要一次。
- 加 `--json` 時，每個成員的 `status` 欄位就是單機 `status --json` 的內容，外層另有 `members_from`、`nodes`、`total`。

> **未實機驗證：** 上面的輸出只有一個節點。多節點時的表格、成員連不上的情況，依據的是 `rradius-core/src/status.rs` 與 `specs/server-status`（Status of the Whole Cluster）。多節點叢集的架設見[第 16 章](16-cluster.md)。

## 調整日誌

伺服器把所有訊息寫到 stderr，標準輸出（stdout）不寫任何東西。用 systemd 跑時進 journal，用容器跑時進容器的日誌。

| 鍵 | 預設 | reload | 意思 |
| --- | --- | --- | --- |
| `log.level` | `info` | 要重啟 | `error`、`warn`、`info` 或 `debug` |
| `log.format` | `text` | 要重啟 | `text` 或 `json` |
| `log.auth` | `true` | 要重啟 | 每次認證決定寫一筆認證紀錄 |

`[log]` 本身改了要重啟，但它底下的 `[log.trace]` reload 就生效。實測 reload 時改 `format`：

```text
… WARN rradius_core::reload: configuration reloaded from rradius.toml; these need a restart to apply: [log]
```

**等級只管 rradius 自己的訊息。** 其他函式庫（例如叢集資料庫）固定只寫 `warn` 以上，就算 `level = "debug"` 也一樣，免得被大量的內部訊息淹沒。

**text 與 json：** text 每行是時間、等級、來源（target）和訊息；json 每行是一個物件，有 `timestamp`、`level`、`target`、`message`，其他欄位攤平放在同一層。實測同一個啟動訊息的兩種格式：

```text
2026-11-22T21:57:39.189728Z  INFO rradius_core::cli: Servers started. Press Ctrl+C to stop.
{"timestamp":"2026-11-22T21:59:07.062005Z","level":"INFO","message":"Servers started. Press Ctrl+C to stop.","target":"rradius_core::cli"}
```

要給日誌收集系統解析，用 json。

**外部的值不會打斷一行。** 使用者名稱這類從外面來的值，裡面的換行與控制字元會被轉成跳脫字元，一筆紀錄永遠只占一行，沒有人能用使用者名稱偽造出一行假的紀錄。

**同一件事不會洗版。** 來自未知用戶端的 RADIUS 封包、未知用戶端的 TACACS+ 連線，每秒最多記一行，下一行會寫出這段時間累積了幾次；計數器則每一次都算。過載也一樣，實測：

```text
… WARN rradius_core::radius: RADIUS authentication overloaded: discarded 252 packets
```

## 認證紀錄

`log.auth = true`（預設）時，每一次 RADIUS 或 TACACS+ 的認證決定都寫一筆，target 是 `rradius_auth`，不受 `level` 影響：`level = "error"` 也照寫。沒有做出決定的封包（重送、過載丟棄、驗證失敗）不寫。

```text
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="bob" account="bob" chain="default" handler="sql" decision="reject"
… INFO rradius_core::auth_chain: auth lockout: client localhost: account "bob" from "" is locked (this origin)
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="bob" account="bob" chain="default" handler="lockout" decision="reject"
… INFO rradius_auth: authentication protocol="tacacs" client="lab-switch" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="accept"
```

json 格式的同一種紀錄：

```text
{"timestamp":"2026-11-22T21:59:25.940382Z","level":"INFO","message":"authentication","protocol":"radius","client":"localhost","source":"127.0.0.1","user":"alice","account":"alice","chain":"default","handler":"sql","decision":"accept","target":"rradius_auth"}
```

| 欄位 | 意思 |
| --- | --- |
| `protocol` | `radius` 或 `tacacs` |
| `client` | 比對到的用戶端名稱 |
| `source` | 封包的來源位址 |
| `user` | 請求裡的使用者名稱；沒有時是空字串 |
| `account` | 驗證鏈正規化之後實際查的帳號（去掉 realm、轉小寫之後） |
| `chain` | 驗證鏈的名稱；`[auth]` 那條叫 `default`；沒有驗證鏈、由腳本決定時是 `script` |
| `handler` | 做出決定的處理器，例如 `sql`、`lua`、`ldap`；每個處理器都略過時是 `none`；名稱被驗證鏈拒絕時是 `normalization`；被鎖定時是 `lockout` |
| `decision` | `accept`、`reject` 或 `challenge` |

紀錄裡只有這些欄位，不含密碼、CHAP 或 MS-CHAPv2 的回應、TACACS+ 的 `data`。

不需要認證紀錄時（例如另外有計費資料可查，或日誌量太大），設 `log.auth = false`。實測關掉後，登入成功也不再有 `rradius_auth` 的行。

> **注意：** 日誌寫不出去時會丟行（見[日誌行被丟棄的時候](#日誌行被丟棄的時候)），所以認證紀錄不能當成完整的稽核紀錄。需要不漏的紀錄，用計費資料或第 13 章的計費記錄檔。

## 追蹤特定使用者或用戶端

想看某個使用者每一次登入的完整請求與回應，例如確認回覆屬性有沒有送出，就開 `[log.trace]`。它一定有時間限制，時間到就自己停。

| 鍵 | 預設 | reload | 意思 |
| --- | --- | --- | --- |
| `log.trace.users` | `[]` | 可以 | 要追蹤的 `User-Name`，要完全相同 |
| `log.trace.clients` | `[]` | 可以 | 要追蹤的 RADIUS 用戶端名稱，要是 `[[radius.clients]]` 裡的 `name` |
| `log.trace.minutes` | 無，必填 | 可以 | 追蹤多久，1 到 1440 分鐘 |

`users` 與 `clients` 不能都是空的。符合其中一個就記。trace 只涵蓋 RADIUS，設定裡沒有 `[radius]` 時 `check` 會拒絕。

**開始追蹤：** 在 `rradius.toml` 加上（放在 `[metrics]` 之前，或放進 `conf.d/` 的檔案）：

```toml
# rradius.toml
[log.trace]
users = ["alice"]
minutes = 10
```

然後 `check`、送 SIGHUP：

```console
$ rradius-server check --config rradius.toml
configuration ok
$ kill -HUP <pid>
```

伺服器的日誌：

```text
… WARN rradius_core::radius: [log.trace] is on for 10 minutes: requests of these users and clients are written out in full, personal data included
… INFO rradius_core::reload: configuration reloaded from rradius.toml
```

**看結果：** 讓 `alice` 登入一次，再讓不在名單上的 `carol` 登入一次：

```text
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="accept"
… INFO rradius_trace: request client="localhost" source=127.0.0.1 user="alice" code=1 attributes="Message-Authenticator = <hidden>, User-Name = alice, User-Password = <hidden>, NAS-IP-Address = 192.0.2.1, NAS-Identifier = rradius-bench"
… INFO rradius_trace: response client="localhost" source=127.0.0.1 user="alice" code=2 attributes="Message-Authenticator = <hidden>, Session-Timeout = 3600, Filter-Id = staff"
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="carol" account="carol" chain="default" handler="none" decision="reject"
```

- 每個符合的請求兩行：`request` 與 `response`，target 是 `rradius_trace`，不受 `level` 影響。沒有送出回應時，第二行是 `no response`。
- `code` 是 RADIUS 的封包代碼：1 是 Access-Request，2 是 Access-Accept。
- `alice` 拿到了 `Session-Timeout = 3600`、`Filter-Id = staff`，這正是第 03 章用 `rradius-bench` 看不到的回覆屬性。
- `carol` 不在名單上，只有認證紀錄，沒有 trace。

**哪些值會被遮住：** 下列屬性一律顯示 `<hidden>`：

- 字典裡標了 `encrypt` 或 `secret` 選項的屬性。
- 名稱（不分大小寫）含有 `pass`、`secret`、`key`、`token`、`chap`、`arap`、`-pw`、`-cpw`、`digest`、`signature`、`psk`、`pmk`、`credential` 的屬性。
- 編號 2、3、24、60、69、70、79、80 的標準屬性（User-Password、CHAP-Password、State、CHAP-Challenge、ARAP-Password、Tunnel-Password、EAP-Message、Message-Authenticator），不管字典怎麼命名它們。

其他值照寫，每個值最多 128 個字元（二進位值最多 64 個位元組，以十六進位顯示）。

> **警告：** trace 會把使用者名稱、MAC 位址、IP 位址這類個人資料完整寫進日誌，日誌送到哪裡，這些資料就到哪裡。只在需要時開，用完就關，`minutes` 設短一點。

**時間怎麼算：** `minutes` 從設定載入或**每一次** reload 起重新計算。開著 trace 時為了別的原因 reload，倒數也會重來，日誌會再印一次 `[log.trace] is on for 10 minutes`。

**提前結束：** 刪掉 `[log.trace]` 再 reload。

## 日誌行被丟棄的時候

日誌由背景執行緒寫出。stderr 寫得太慢（例如接收端卡住），佇列滿了，伺服器寧可丟掉日誌行，也不讓請求等待。丟掉的行數，以及寫入失敗的行數，記在 `rradius_log_dropped_lines_total`；規則檔的 `RradiusLogLinesDropped` 會在它增加時告警。

看到它增加，代表日誌的去處跟不上，例如 journal 或日誌收集程式太慢、磁碟滿了。這段時間的認證紀錄與 trace 可能不完整。

收到 SIGTERM 或 Ctrl+C 準備結束時，伺服器最多等 5 秒，讓佇列裡的日誌寫完。

> **未實機驗證：** 本章的實測中，`rradius_log_dropped_lines_total` 一直是 0，沒有製造出日誌塞住的情況。行為依據是 `rradius-core/src/logging.rs` 與 `specs/server-logging`（Log Output Never Delays Requests）。

## 常見錯誤

**症狀：** `status` 印出 `status needs [metrics]: the command reads this node's /metrics. Add to rradius.toml:`，後面是一段建議的設定，結束碼 2。
**原因：** 設定檔沒有 `[metrics]`。
**解法：** 加上 `[metrics]`，重新啟動伺服器（`[metrics]` 要重啟才生效）。

**症狀：** `status` 印出 `cannot reach the metrics port at 127.0.0.1:9812: Connection refused (os error 61)`，結束碼 3。
**原因：** 伺服器沒在跑，或 `status` 讀的設定檔和伺服器用的不是同一份，埠不同。錯誤碼的數字依作業系統而不同。
**解法：** 確認伺服器在跑，`--config` 指向同一份設定。

**症狀：** `/readyz` 一直回 503 `database billing unreachable`，但登入正常。
**原因：** `[databases.billing]` 設了 `required_for_ready = true`，而這個資料庫連不上。
**解法：** 修好資料庫的連線；若這個資料庫不影響登入，把 `required_for_ready` 拿掉，讓它不影響就緒狀態（要重啟）。

**症狀：** 日誌出現 `reload failed, keeping the running configuration: rradius.toml: log.trace.minutes: 0 is not from 1 to 1440`，`status` 的 Reload 區段 `Failed` 加 1。
**原因：** reload 時新的設定有錯，伺服器繼續用原本的設定。
**解法：** reload 前先跑 `check`。`minutes` 要在 1 到 1440 之間。

**症狀：** `check` 印出 `log.trace.minutes is required: tracing has to end by itself`。
**原因：** `[log.trace]` 沒有 `minutes`。
**解法：** 加上 `minutes`。

**症狀：** `check` 印出 `log.trace lists no users and no clients`。
**原因：** `users` 與 `clients` 都是空的。
**解法：** 至少寫一個使用者或用戶端。

**症狀：** `check` 印出 `log.trace.clients: 'nobody' is not the name of a RADIUS client`。
**原因：** `clients` 要寫用戶端的 `name`，不是位址。
**解法：** 改成 `[[radius.clients]]` 裡的名稱。

**症狀：** `check` 印出 `log.trace needs [radius]: it only covers RADIUS`。
**原因：** 設定只有 `[tacacs]`。trace 只涵蓋 RADIUS。
**解法：** TACACS+ 的問題看認證紀錄與 `debug` 等級的日誌。

**症狀：** `check` 印出 `log.level: 'loud' is not error, warn, info or debug`。
**原因：** 等級拼錯。
**解法：** 改成四個值之一。

**症狀：** 改了 `[log]` 之後 reload，日誌寫 `these need a restart to apply: [log]`，格式沒變。
**原因：** `[log]` 要重啟才生效（`[log.trace]` 例外）。
**解法：** 安排重新啟動。

**症狀：** `curl -I http://…/metrics` 得到 `405 Method Not Allowed`。
**原因：** 指標埠只接受 `GET`，`-I` 送的是 `HEAD`。
**解法：** 用 `curl -s` 取內容。監控系統的健康檢查也要設成 `GET`。

## 相關章節

- [03 第一次啟動與登入](03-quick-start.md)
- [04 設定檔的基本功](04-configuration.md)
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)
- [09 外部資料庫與具名查詢](09-external-databases.md)
- [13 計費與 session](13-accounting.md)
- [16 叢集與高可用](16-cluster.md)
- [18 日常維運](18-operations.md)
- [19 效能調校與壓測](19-performance.md)
- [21 排錯](21-troubleshooting.md)
- [附錄 A：設定參考](90-config-reference.md)
- [附錄 C：指標參考](92-metrics-reference.md)
