# 92 附錄 C：指標參考

> 本章讓你做到：
> - 查到 `rradius-server` 在 `/metrics` 輸出的每一個指標：型別、標籤、意思
> - 知道哪些指標一直都有，哪些要有對應的設定才會出現
> - 從指標找到 `deploy/monitoring/rradius.rules.yml` 裡對應的告警規則

前置條件：讀過[第 17 章](17-monitoring.md)的「讀懂 /metrics」，知道 counter、gauge、histogram 的差別，以及怎麼用 `curl` 抓 `/metrics`。

本章小節：

- [怎麼讀這份參考](#怎麼讀這份參考)
- [這份清單怎麼來的](#這份清單怎麼來的)
- [總覽](#總覽)
- [RADIUS](#radius)
- [TACACS+](#tacacs)
- [EAP](#eap)
- [叢集與叢集資料庫](#叢集與叢集資料庫)
- [計費 spool 與計費記錄檔](#計費-spool-與計費記錄檔)
- [登入失敗鎖定與密碼比對](#登入失敗鎖定與密碼比對)
- [Netlogon](#netlogon)
- [外部資料庫](#外部資料庫)
- [具名查詢](#具名查詢)
- [腳本的 HTTP 呼叫](#腳本的-http-呼叫)
- [reload、版本與日誌](#reload版本與日誌)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

---

## 怎麼讀這份參考

每個指標一個小節，標題是指標族的名稱，下面第一行寫：

- **型別：** `counter`（只增不減）、`gauge`（當下的值）或 `histogram`（分布）。
- **標籤：** 每個標籤的名稱與它可能的值。值是固定的一組時全部列出；值來自設定（例如用戶端名稱）時寫出來源。
- **出現條件：** 「一直都有」表示任何設定都會輸出；其他的寫出需要哪個設定。
- **告警：** `deploy/monitoring/rradius.rules.yml` 裡用到它的規則；沒有就寫「—」。

共同的規則：

- 所有指標都是**每個節點各自**、從行程啟動起算。reload 不歸零，重新啟動才歸零。
- 一個指標族只要出現，它所有標籤組合從一開始就在，值是 0。查詢不會因為某個組合「第一次出現」而少算。
- histogram 的樣本名稱是 `<名稱>_bucket{…,le="…"}`、`<名稱>_sum`、`<名稱>_count`。區間的上界（秒）固定是 `0.00005`、`0.0001`、`0.00025`、`0.0005`、`0.001`、`0.0025`、`0.005`、`0.01`、`0.025`、`0.05`、`0.1`、`0.25`、`0.5`、`1`、`2.5`、`5`、`10`、`+Inf`；`_sum` 的單位是秒，`_count` 等於 `+Inf` 那一格。
- 每個指標的英文 `# HELP` 說明，就是 `/metrics` 裡那一行，這裡用中文重寫，不逐字翻譯。

---

## 這份清單怎麼來的

指標名稱有一部分是程式組出來的（例如具名查詢的 `rradius_query_<名稱>`），所以這份清單不是從原始碼抄的，而是用實際跑起來的 `/metrics` 產生，再和原始碼交叉比對。

**實際輸出：** 用 2026-11-23 核心 repo 的 commit 建出的 `rradius-server 0.1.0`，跑四種設定，每種抓一次 `/metrics`：

| 設定 | 內容 | 指標族數 |
| --- | --- | --- |
| 最小 | quick-start 情境（`[radius]`、sql 處理器、計費腳本），`log.auth = false` | 39 |
| 只有 TACACS+ | `rradius-server/benchmarks/tacacs.toml`（沒有 `[radius]`） | 37 |
| 完整 | 最小，加上 `[tacacs]`、`[auth.lockout]`、`[radius.accounting_log]`、一個 `[queries.*]` | 49 |
| 外部資料庫 | 完整，加上一個 `required_for_ready = true` 的 `[databases.*]` | 53 |

每份輸出取 `# TYPE` 行的名稱與型別，再從樣本行取出標籤名稱與值。四份的聯集是 53 個指標族（具名查詢的 gauge 算一個）。

```console
$ curl -s http://127.0.0.1:9812/metrics | grep '^# TYPE' | wc -l
```

**原始碼：** 在 `rradius-core/src/` 的每個檔案（不含 `#[cfg(test)]` 的測試模組），找出傳給 `write_family`、`write_histograms` 的名稱與直接寫出的 `# HELP` 行，共 53 個，其中 `rradius_query_{name}` 是具名查詢的動態名稱。核心內建的擴充是空的（`rradius-core/src/extensions/mod.rs`），所以擴充不會再加指標。

**結果：** 兩邊的集合完全相同，差異是 0：程式碼寫出的每個指標族都在實際輸出裡出現過，實際輸出的每個指標族也都能在程式碼找到。再拿本附錄的小節標題比對同一個集合，每一節寫的型別、標籤名稱與固定的標籤值也逐一對過實際輸出，差異同樣是 0。來自設定的標籤（用戶端、資料庫、查詢的名稱，以及具名查詢自己的標籤欄位）不列固定值，只寫出來源。

最小設定的 39 個指標族，展開 histogram 的 `_bucket`、`_sum`、`_count` 之後，是 43 個樣本名稱。

> **企業版：** 企業版的 `/metrics` 在這些之後還有它自己功能的指標。

---

## 總覽

| 指標族 | 型別 | 出現條件 | 主章 |
| --- | --- | --- | --- |
| `rradius_radius_packets_total` | counter | 一直都有 | 17 |
| `rradius_radius_responses_total` | counter | 一直都有 | 17 |
| `rradius_radius_client_responses_total` | counter | 一直都有 | 17 |
| `rradius_radius_status_server_total` | counter | 一直都有 | 17 |
| `rradius_radius_duplicates_total` | counter | 一直都有 | 17 |
| `rradius_radius_discarded_total` | counter | 一直都有 | 17 |
| `rradius_radius_response_seconds` | histogram | 一直都有 | 17 |
| `rradius_radius_requests_in_progress` | gauge | 有 `[radius]` | 17、19 |
| `rradius_radius_requests_limit` | gauge | 有 `[radius]` | 17、19 |
| `rradius_tacacs_connections_total` | counter | 一直都有 | 17 |
| `rradius_tacacs_connections_open` | gauge | 一直都有 | 17 |
| `rradius_tacacs_requests_total` | counter | 一直都有 | 17 |
| `rradius_tacacs_malformed_total` | counter | 一直都有 | 17 |
| `rradius_tacacs_response_seconds` | histogram | 一直都有 | 17 |
| `rradius_eap_sessions_open` | gauge | 一直都有 | 17 |
| `rradius_eap_sessions_total` | counter | 一直都有 | 17 |
| `rradius_cluster_leader` | gauge | 一直都有 | 16 |
| `rradius_cluster_is_leader` | gauge | 一直都有 | 16 |
| `rradius_cluster_applied_index` | gauge | 一直都有 | 16 |
| `rradius_cluster_seconds_without_leader` | gauge | 一直都有 | 16 |
| `rradius_cluster_caught_up` | gauge | 一直都有 | 16 |
| `rradius_db_batched_writes_total` | counter | 一直都有 | 16 |
| `rradius_db_split_batches_total` | counter | 一直都有 | 16 |
| `rradius_acct_spool_written_total` | counter | 一直都有 | 13 |
| `rradius_acct_spool_replayed_total` | counter | 一直都有 | 13 |
| `rradius_acct_spool_rejected_total` | counter | 一直都有 | 13 |
| `rradius_acct_spool_full_total` | counter | 一直都有 | 13 |
| `rradius_acct_spool_corrupt_total` | counter | 一直都有 | 13 |
| `rradius_acct_spool_bytes` | gauge | 一直都有 | 13 |
| `rradius_accounting_log_written_total` | counter | 有 `[radius.accounting_log]` | 13 |
| `rradius_accounting_log_dropped_total` | counter | 有 `[radius.accounting_log]` | 13 |
| `rradius_accounting_log_failures_total` | counter | 有 `[radius.accounting_log]` | 13 |
| `rradius_auth_lockout_rejected_total` | counter | 有 `[auth.lockout]` | 07 |
| `rradius_auth_lockouts_total` | counter | 有 `[auth.lockout]` | 07 |
| `rradius_auth_lockout_untracked_total` | counter | 有 `[auth.lockout]` | 07 |
| `rradius_password_checks_total` | counter | 一直都有 | 07 |
| `rradius_netlogon_logons_total` | counter | 一直都有 | 08 |
| `rradius_netlogon_channel_setups_total` | counter | 一直都有 | 08 |
| `rradius_netlogon_open_connections` | gauge | 一直都有 | 08 |
| `rradius_external_db_failures_total` | counter | 有 `[databases.*]` | 09 |
| `rradius_external_db_connections` | gauge | 有 `[databases.*]` | 09 |
| `rradius_external_db_connections_max` | gauge | 有 `[databases.*]` | 09 |
| `rradius_external_db_reachable` | gauge | 有 `required_for_ready = true` 的 `[databases.*]` | 09 |
| `rradius_query_{name}` | gauge | 每個 `[queries.<name>]` 一個 | 09 |
| `rradius_query_failures_total` | counter | 有 `[queries.*]` | 09 |
| `rradius_query_dropped_rows_total` | counter | 有 `[queries.*]` | 09 |
| `rradius_query_last_success_timestamp_seconds` | gauge | 有 `[queries.*]` | 09 |
| `rradius_script_http_requests_total` | counter | 一直都有 | 10 |
| `rradius_reload_total` | counter | 一直都有 | 17 |
| `rradius_reload_last_success_timestamp_seconds` | gauge | 一直都有 | 17 |
| `rradius_build_info` | gauge | 一直都有 | 17 |
| `rradius_start_time_seconds` | gauge | 一直都有 | 17 |
| `rradius_log_dropped_lines_total` | counter | 一直都有 | 17 |

「一直都有」包括沒有用到那個功能的設定：例如沒有 `[tacacs]` 時 TACACS+ 的指標仍然輸出，值都是 0；沒有 netlogon 處理器時 Netlogon 的指標也一樣。

---

## RADIUS

RADIUS 的指標同時涵蓋 UDP 與 RadSec。主章：[第 17 章](17-monitoring.md)。

#### `rradius_radius_packets_total`

型別：counter。標籤：`kind`（`auth`、`acct`）。出現條件：一直都有。告警：—。

通過檢查與重送偵測、實際進入處理的 RADIUS 請求數，分認證與計費。被丟棄的封包、重送與 Status-Server 都不算在這裡；實測送了 6 筆 Access-Request、1 筆共用密碼錯誤的請求與 1 個 Status-Server，`kind="auth"` 是 6。

#### `rradius_radius_responses_total`

型別：counter。標籤：`code`（`access_accept`、`access_reject`、`access_challenge`、`accounting_response`）。出現條件：一直都有。告警：`RradiusManyRejects`。

送出的回應，依回應種類分。從重送快取再送一次的回應、回給 Status-Server 的回應不算。

#### `rradius_radius_client_responses_total`

型別：counter。標籤：`client`（`[[radius.clients]]` 的 `name`）、`code`（`access_accept`、`access_reject`、`access_challenge`、`accounting_response`）。出現條件：一直都有；每個設定裡的用戶端各有四個樣本，沒有用戶端時這個指標族只有 `# HELP`、`# TYPE` 兩行。告警：—。

送給每個用戶端的回應數。用戶端名稱只來自設定，不會隨流量增加；reload 新增的用戶端從 0 開始，被移除的用戶端樣本消失。`status` 的「Busiest RADIUS clients」就是從這裡算的。

#### `rradius_radius_status_server_total`

型別：counter。標籤：無。出現條件：一直都有。告警：—。

回答過的 Status-Server 封包數。

#### `rradius_radius_duplicates_total`

型別：counter。標籤：無。出現條件：一直都有。告警：—。

NAS 重送的請求：從重送快取直接回答，或因為原本那筆還在處理而丟掉的。用 `rradius-bench` 壓測時這個值增加，表示測試的請求被當成重送，量到的不是正常路徑。

#### `rradius_radius_discarded_total`

型別：counter。標籤：`reason`（`overload`、`unknown_client`、`invalid`、`database`、`no_leader`）。出現條件：一直都有。告警：`RradiusOverload`（`overload`）、`RradiusDiscardingForTheDatabase`（`database`、`no_leader`）、`RradiusInvalidPackets`（`invalid`）。

收到了、但沒有回應的封包：

| `reason` | 意思 |
| --- | --- |
| `overload` | 處理中的請求已達 `max_auth_inflight` 或 `max_acct_inflight` |
| `unknown_client` | 來源不屬於任何用戶端 |
| `invalid` | 格式錯誤、驗證碼或 Message-Authenticator 驗不過、不符合政策；最常見的是共用密碼不一致 |
| `database` | 叢集資料庫無法使用，Access-Request 不回應 |
| `no_leader` | `cluster.stop_without_leader_ms` 已經生效 |

#### `rradius_radius_response_seconds`

型別：histogram。標籤：`kind`（`auth`、`acct`）。出現條件：一直都有。告警：`RradiusSlowLogins`（`kind="auth"` 的 p99 超過 1 秒）。

RADIUS 請求從通過重送檢查到送出回應花的時間。沒有回應的請求不算。`status` 的 p50、p99 從這裡內插。

#### `rradius_radius_requests_in_progress`

型別：gauge。標籤：`kind`（`auth`、`acct`）。出現條件：伺服器有 `[radius]`。告警：—。

這一刻正在處理的 RADIUS 請求數。接近 `rradius_radius_requests_limit` 時，再多的請求就會以 `overload` 丟棄。

#### `rradius_radius_requests_limit`

型別：gauge。標籤：`kind`（`auth`、`acct`）。出現條件：伺服器有 `[radius]`。告警：—。

可以同時處理的請求數，也就是 `radius.max_auth_inflight` 與 `radius.max_acct_inflight` 的值（預設都是 4096）。調整方式見[第 19 章](19-performance.md)。

---

## TACACS+

主章：[第 17 章](17-monitoring.md)，TACACS+ 本身見[第 12 章](12-tacacs.md)。

#### `rradius_tacacs_connections_total`

型別：counter。標籤：`result`（`accepted`、`refused`）。出現條件：一直都有。告警：—。

TACACS+ 連線的結果。`refused` 是讀任何資料之前就關掉的連線：來源不屬於任何 `[[tacacs.clients]]`，或超過 `tacacs.max_connections_per_source`、`max_connections`。

#### `rradius_tacacs_connections_open`

型別：gauge。標籤：無。出現條件：一直都有。告警：—。

這一刻開著的 TACACS+ 連線數。

#### `rradius_tacacs_requests_total`

型別：counter。標籤：`type`（`authen`、`author`、`acct`）。出現條件：一直都有。告警：—。

TACACS+ 請求數，依認證、授權、計費分。一次登入算一筆，不管 ASCII 登入來回用了幾個封包。

#### `rradius_tacacs_malformed_total`

型別：counter。標籤：無。出現條件：一直都有。告警：—。

解密後內容不合理的請求，最常見的原因是共用密碼不一致。

#### `rradius_tacacs_response_seconds`

型別：histogram。標籤：`type`（`authen`、`author`、`acct`）。出現條件：一直都有。告警：—。

從讀到封包到寫出回應的時間；認證的每個封包各算一次。

---

## EAP

主章：[第 17 章](17-monitoring.md)，EAP 本身見[第 11 章](11-eap.md)。

#### `rradius_eap_sessions_open`

型別：gauge。標籤：無。出現條件：一直都有。告警：—。

這一刻進行中的 EAP 對話數。

#### `rradius_eap_sessions_total`

型別：counter。標籤：`method`（`tls`、`peap`、`ttls`）、`result`（`accept`、`reject`、`timeout`、`overload`、`replaced`、`evicted`）。出現條件：一直都有。告警：`RradiusEapSessionsLost`（`evicted`、`overload`）。

EAP 對話怎麼結束：

| `result` | 意思 |
| --- | --- |
| `accept`、`reject` | 對話完成，接受或拒絕 |
| `timeout` | 對話太久沒有下一步，過期清掉 |
| `overload` | 因為上限或負載被拒，例如同一台設備的對話數已達上限 |
| `replaced` | 同一台設備停在 Identity 之後又重新開始，舊的那個被取代 |
| `evicted` | 對話總數或單一用戶端的對話數已滿，最久沒動靜的被擠掉 |

---

## 叢集與叢集資料庫

主章：[第 16 章](16-cluster.md)。

#### `rradius_cluster_leader`

型別：gauge。標籤：無。出現條件：一直都有。告警：`RradiusNoLeader`（等於 0 持續 1 分鐘）。

這個節點有可以寫入的 leader 時是 1，否則 0。單機時也是 1。

#### `rradius_cluster_is_leader`

型別：gauge。標籤：無。出現條件：一直都有。告警：—。

這個節點就是 leader 時是 1。

#### `rradius_cluster_applied_index`

型別：gauge。標籤：無。出現條件：一直都有。告警：—。

本機副本最後套用的 Raft 記錄編號。各節點的值相近表示複寫跟得上。

#### `rradius_cluster_seconds_without_leader`

型別：gauge。標籤：無。出現條件：一直都有。告警：—。

沒有 leader 已經幾秒；有 leader 時是 0。超過 `cluster.stop_without_leader_ms` 時，`/readyz` 回 503 `no leader`。

#### `rradius_cluster_caught_up`

型別：gauge。標籤：無。出現條件：一直都有。告警：`RradiusNotCaughtUp`（等於 0 持續 5 分鐘）。

節點啟動之後，本機副本已經追上叢集時是 1；在那之前用的可能是落後的資料，是 0。

#### `rradius_db_batched_writes_total`

型別：counter。標籤：無。出現條件：一直都有。告警：—。

腳本的寫入和其他寫入合併在同一個交易裡提交的筆數。

#### `rradius_db_split_batches_total`

型別：counter。標籤：無。出現條件：一直都有。告警：—。

合併的交易失敗後，拆開來找出是哪一筆失敗的次數。

---

## 計費 spool 與計費記錄檔

主章：[第 13 章](13-accounting.md)。計費 spool 是寫在磁碟上、等資料庫恢復後重送的計費請求。

#### `rradius_acct_spool_written_total`

型別：counter。標籤：無。出現條件：一直都有。告警：—。

寫進 spool 的計費請求數。

#### `rradius_acct_spool_replayed_total`

型別：counter。標籤：無。出現條件：一直都有。告警：—。

從 spool 重送、被腳本接受的計費請求數。

#### `rradius_acct_spool_rejected_total`

型別：counter。標籤：無。出現條件：一直都有。告警：`RradiusAccountingSpoolLoss`。

重送時被拒、移到 `rejected.log` 的計費請求數。

#### `rradius_acct_spool_full_total`

型別：counter。標籤：無。出現條件：一直都有。告警：—。

因為 spool 滿了或磁碟出錯而沒寫進 spool 的計費請求數。

#### `rradius_acct_spool_corrupt_total`

型別：counter。標籤：無。出現條件：一直都有。告警：`RradiusAccountingSpoolLoss`。

損毀、被移到一旁的 spool 檔案數。

#### `rradius_acct_spool_bytes`

型別：gauge。標籤：無。出現條件：一直都有。告警：`RradiusAccountingSpoolBacklog`（大於 0 持續 15 分鐘）。

spool 裡等著重送的位元組數。

#### `rradius_accounting_log_written_total`

型別：counter。標籤：無。出現條件：有 `[radius.accounting_log]`。告警：—。

寫進計費記錄檔的計費請求數。

#### `rradius_accounting_log_dropped_total`

型別：counter。標籤：無。出現條件：有 `[radius.accounting_log]`。告警：`RradiusAccountingLogDropping`。

計費記錄檔的佇列滿了、沒有寫入的計費請求數。

#### `rradius_accounting_log_failures_total`

型別：counter。標籤：`stage`（`open`、`write`）。出現條件：有 `[radius.accounting_log]`。告警：`RradiusAccountingLogDropping`。

計費記錄檔開檔失敗與寫入失敗的次數。

---

## 登入失敗鎖定與密碼比對

主章：[第 07 章](07-users-and-chains.md)。

#### `rradius_auth_lockout_rejected_total`

型別：counter。標籤：無。出現條件：有 `[auth.lockout]`。告警：—。

因為帳號加來源、或整個帳號被鎖住，或同時等待判定的登入太多，而直接拒絕的登入數。

#### `rradius_auth_lockouts_total`

型別：counter。標籤：無。出現條件：有 `[auth.lockout]`。告警：—。

開始鎖定的次數。

#### `rradius_auth_lockout_untracked_total`

型別：counter。標籤：無。出現條件：有 `[auth.lockout]`。告警：—。

鎖定表被鎖定中的項目占滿，因而沒有被計入的失敗次數。

#### `rradius_password_checks_total`

型別：counter。標籤：`scheme`（`clear`、`none`、`crypt`、`md5`、`smd5`、`sha`、`ssha`、`sha224`、`ssha224`、`sha256`、`ssha256`、`sha384`、`ssha384`、`sha512`、`ssha512`、`pbkdf2-sha1`、`pbkdf2-sha256`、`pbkdf2-sha512`、`x-pbkdf2`、`ns-mta-md5`、`nthash`、`argon2`、`unknown`）、`result`（`failed`、`ok`）。出現條件：一直都有，23 × 2 個樣本。告警：—。

登入時拿來和存放的密碼比對的次數，依存放值的格式與結果分。`none` 是沒有格式前綴的值；`unknown` 是認不得的前綴；`pbkdf2-sha1` 涵蓋它的兩種寫法。實測 quick-start 的 `alice`（明文）記在 `none`，`bob`（bcrypt 雜湊）記在 `crypt`。用來找出資料表裡還有哪些舊格式的密碼。

---

## Netlogon

主章：[第 08 章](08-ldap-and-ad.md)。

#### `rradius_netlogon_logons_total`

型別：counter。標籤：`result`（`accept`、`reject`、`no_such_user`、`unavailable`）。出現條件：一直都有。告警：—。

netlogon 處理器向網域控制站詢問的 MS-CHAPv2 登入，依結果分。`unavailable` 是沒有網域控制站可以回答。

#### `rradius_netlogon_channel_setups_total`

型別：counter。標籤：`result`（`ok`、`failed`）。出現條件：一直都有。告警：—。

和網域控制站建立 Netlogon 安全通道的次數，依結果分。

#### `rradius_netlogon_open_connections`

型別：gauge。標籤：無。出現條件：一直都有。告警：—。

這一刻開著、已加密的 Netlogon 連線數。

---

## 外部資料庫

主章：[第 09 章](09-external-databases.md)。`database` 標籤是 `[databases.<名稱>]` 的名稱。

#### `rradius_external_db_failures_total`

型別：counter。標籤：`database`（設定裡的名稱）、`reason`（`unavailable`、`mistake`、`unknown`）。出現條件：有 `[databases.*]`。告警：`RradiusExternalDatabaseFailing`（`unavailable`）。

對每個外部資料庫的呼叫失敗次數。`unavailable` 是連不上或逾時，`mistake` 是 SQL 或參數有錯，`unknown` 是其他。

#### `rradius_external_db_connections`

型別：gauge。標籤：`database`（設定裡的名稱）、`state`（`in_use`、`idle`）。出現條件：有 `[databases.*]`。告警：`RradiusExternalDatabasePoolFull`。

連線池的連線數：`in_use` 是交給呼叫者或正在建立的，`idle` 是開著、閒置的。

#### `rradius_external_db_connections_max`

型別：gauge。標籤：`database`（設定裡的名稱）。出現條件：有 `[databases.*]`。告警：`RradiusExternalDatabasePoolFull`。

連線池最多保持的連線數，也就是 `pool_size`。

#### `rradius_external_db_reachable`

型別：gauge。標籤：`database`（設定裡的名稱）。出現條件：有 `required_for_ready = true` 的資料庫，只列這些。告警：`RradiusExternalDatabaseUnreachable`（等於 0 持續 2 分鐘）。

就緒檢查連得到這個資料庫時是 1，否則 0。是 0 時 `/readyz` 回 503 `database <名稱> unreachable`。實測：

```text
rradius_external_db_reachable{database="billing"} 0
```

---

## 具名查詢

主章：[第 09 章](09-external-databases.md)。`query` 標籤是 `[queries.<名稱>]` 的名稱。

#### `rradius_query_{name}`

型別：gauge。標籤：`[queries.<name>] labels` 列出的欄位，標籤名稱就是欄位名稱。出現條件：每個 `[queries.<name>]` 一個，名稱是 `rradius_query_` 加上查詢的名稱。告警：—。

具名查詢每一列的結果，值取自 `value` 欄位。`# HELP` 是設定的 `help`，沒設時是一段寫著查詢名稱的文字。查詢還沒成功過時，只有 `# HELP`、`# TYPE` 兩行，沒有樣本。實測：

```text
# HELP rradius_query_accounts_by_state Accounts in the users table, by state.
# TYPE rradius_query_accounts_by_state gauge
rradius_query_accounts_by_state{state="disabled"} 1
rradius_query_accounts_by_state{state="enabled"} 2
```

#### `rradius_query_failures_total`

型別：counter。標籤：`query`（查詢名稱）、`reason`（`timeout`、`error`、`shape`）。出現條件：有 `[queries.*]`。告警：`RradiusNamedQueryFailing`。

每個具名查詢失敗的次數。`timeout` 是時間內沒有結果，`error` 是資料庫拒絕這段 SQL 或連不上，`shape` 是結果少了欄位或值的型別不對。

#### `rradius_query_dropped_rows_total`

型別：counter。標籤：`query`（查詢名稱）、`reason`（`null_value`、`over_limit`）。出現條件：有 `[queries.*]`。告警：—。

查詢成功、但沒有變成樣本的列數。`null_value` 是值欄位為 NULL，`over_limit` 是超過 `max_rows`。

#### `rradius_query_last_success_timestamp_seconds`

型別：gauge。標籤：`query`（查詢名稱）。出現條件：有 `[queries.*]`。告警：`RradiusNamedQueryStale`（超過 120 秒沒成功）。

每個具名查詢最後一次成功的 Unix 時間；還沒成功過是 0。

---

## 腳本的 HTTP 呼叫

主章：[第 10 章](10-lua-policies.md)。

#### `rradius_script_http_requests_total`

型別：counter。標籤：`result`（`ok`、`timeout`、`error`、`denied`、`too_large`、`busy`）。出現條件：一直都有。告警：—。

腳本呼叫 `http_request` 的結果：

| `result` | 意思 |
| --- | --- |
| `ok` | 拿到回應 |
| `timeout` | 超過時限 |
| `error` | 連線或請求失敗 |
| `denied` | 主機不在 `lua.http.allowed_hosts` 裡 |
| `too_large` | 回應超過大小上限 |
| `busy` | 對同一個主機等待中的請求已達上限 |

---

## reload、版本與日誌

主章：[第 17 章](17-monitoring.md)。

#### `rradius_reload_total`

型別：counter。標籤：`result`（`ok`、`error`）。出現條件：一直都有。告警：`RradiusReloadFailed`（`error`）。

reload 的次數，依結果分。失敗時伺服器繼續用原本的設定，日誌寫 `reload failed, keeping the running configuration: …`。

#### `rradius_reload_last_success_timestamp_seconds`

型別：gauge。標籤：無。出現條件：一直都有。告警：—。

最後一次 reload 成功的 Unix 時間；還沒有 reload 過是 0。

#### `rradius_build_info`

型別：gauge。標籤：`version`（程式的版本，例如 `0.1.0`）。出現條件：一直都有。告警：—。

值永遠是 1，版本放在標籤裡。用來看叢集裡各節點的版本是否一致。

#### `rradius_start_time_seconds`

型別：gauge。標籤：無。出現條件：一直都有。告警：—。

行程啟動的 Unix 時間。`time() - rradius_start_time_seconds` 是跑了多久；這個值變了，表示重新啟動過，所有 counter 也從 0 重來。

#### `rradius_log_dropped_lines_total`

型別：counter。標籤：無。出現條件：一直都有。告警：`RradiusLogLinesDropped`。

輸出跟不上而丟掉、或寫入失敗的日誌行數。

---

## 常見錯誤

**症狀：** Grafana 圖上某個指標「不見了」。
**原因：** 它只在有對應設定時才出現，例如拿掉 `[auth.lockout]` 之後重啟，`rradius_auth_lockout_*` 就不再輸出；具名查詢改名後，舊名稱的 gauge 也消失。
**解法：** 對照[總覽](#總覽)的出現條件。

**症狀：** counter 突然掉回很小的數字，`rate()` 出現一個尖峰或缺口。
**原因：** 節點重新啟動了，counter 從 0 開始。`rradius_start_time_seconds` 會跟著改變。
**解法：** 查詢一律用 `rate()`、`increase()`，它們會處理歸零。`status` 遇到這種情況會印 `counters reset since the previous sample: the server restarted`。

**症狀：** 叢集三個節點的 `rradius_radius_packets_total` 差很多。
**原因：** 指標是每個節點各自算的，數字反映的是 NAS 把請求送到了哪一台。
**解法：** 要看總量用 `sum without (instance) (…)`，或 `status --cluster`。

**症狀：** `rradius_radius_client_responses_total` 裡找不到某個用戶端。
**原因：** reload 把它從設定拿掉了，它的樣本也跟著拿掉。
**解法：** 確認設定裡還有這個用戶端的 `name`。

## 相關章節

- [07 使用者帳號與驗證鏈](07-users-and-chains.md)
- [08 接上 LDAP 與 Active Directory](08-ldap-and-ad.md)
- [09 外部資料庫與具名查詢](09-external-databases.md)
- [10 用 Lua 寫政策](10-lua-policies.md)
- [11 802.1X 與 EAP](11-eap.md)
- [12 TACACS+ 設備管理](12-tacacs.md)
- [13 計費與 session](13-accounting.md)
- [16 叢集與高可用](16-cluster.md)
- [17 監控、狀態與日誌](17-monitoring.md)
- [19 效能調校與壓測](19-performance.md)
- [附錄 A：設定參考](90-config-reference.md)
