# 12. 依症狀排查

這一章從「看到什麼現象」出發，告訴你先看哪裡。各章結尾的「常見問題」講的是那個情境特有的問題；這裡是跨情境的。訊息的完整對照表在 [usage 第 8 節](../usage.md#8-疑難排解)。

## 先準備好這四樣

| 工具 | 看什麼 | 怎麼用 |
| --- | --- | --- |
| **認證紀錄** | 每次登入的結果，和是誰做的決定 | 記錄裡 `authentication protocol=...` 開頭的行，預設就有 |
| **指標** | 沒有留下紀錄的事：被丟棄的封包、過載、上游狀態 | `rradius-server status --config ... --watch` 看每秒的量；完整內容 `curl -s http://127.0.0.1:9812/metrics`。都要有 `[metrics]` |
| **追蹤** | 一個使用者或一台 NAS 的完整請求和回應 | `[log.trace]` 加 SIGHUP，見[第 11 章](11-operations.md#排查某個使用者的問題) |
| **`check`** | 設定檔、憑證、腳本語法有沒有問題 | `rradius-server check --config ...`，不影響執行中的服務 |

**認證紀錄只記「做了決定」的請求。** 被丟棄的封包（來源不認得、簽章不對、過載、重送）不會有紀錄，要看指標。這是最常見的誤會：記錄裡什麼都沒有，不代表封包沒到。

## NAS 送了請求，完全沒有回應

rradius 對很多情況刻意不回應（回了反而讓攻擊者得到資訊，或讓 NAS 以為是密碼錯誤）。先看這個指標哪一個標籤在增加：

```bash
curl -s http://127.0.0.1:9812/metrics | grep -E 'rradius_radius_(discarded|duplicates|packets)_total'
```

| 哪個在增加 | 意思 | 怎麼辦 |
| --- | --- | --- |
| 都沒有動 | 封包沒有到 rradius | 防火牆、路由、NAS 設定的位址和埠；`[radius] bind` 是不是只聽了 `127.0.0.1`；用 `tcpdump -ni any udp port 1812` 確認 |
| `discarded{reason="unknown_client"}` | 來源位址不在 `[[radius.clients]]` | 記錄有 `unknown client` 和來源位址。NAS 經過 NAT 時，rradius 看到的是轉換後的位址。 |
| `discarded{reason="invalid"}` | 共用密碼不對、格式錯誤，或沒帶 Message-Authenticator | 記錄有 `lacking a Message-Authenticator` 時：舊設備不送這個屬性，在那個 client 設 `require_message_authenticator = "no"` 或 `"auto"`（[第 1 章](01-concepts.md#client誰可以來問)）。否則核對共用密碼，注意檔案結尾有沒有多的空白 |
| `discarded{reason="overload"}` | 同時處理的請求到上限 | 見下面「登入很慢或時好時壞」 |
| `duplicates` | 是重送：原本的請求還在處理 | 處理太慢，NAS 等不及。見下面「登入很慢或時好時壞」 |
| `packets` 有增加，但沒有回應 | 請求有處理，但決定不回應 | 計費腳本沒有回傳 `'accept'`；EAP 的 `max_sessions_per_device` 滿了 |

**用 Status-Server 分開「網路和密碼」和「帳號」：**

```bash
echo 'Message-Authenticator=0x00' | radclient -t 2 -r 1 <位址>:1812 status <共用密碼>
```

有回應，代表網路通、來源被認得、共用密碼正確，問題在請求本身。沒有回應，問題在前面三樣之一。這個指令要從 client 名單內的主機送。

## 登入被拒絕

找到那次登入的認證紀錄，看 `handler` 和 `account`：

```
authentication protocol="radius" client="wlc" source=10.30.0.10 user="Staff1@corp.example.com" account="staff1" chain="default" handler="sql" decision="reject"
```

| `handler` | 意思 | 常見原因 |
| --- | --- | --- |
| `sql` | `sql` 處理器拒絕 | 密碼錯誤；密碼欄是 `NULL` 或空的；查到多筆；密碼的格式不能用於這種登入方式（CHAP 要明文，MS-CHAPv2 要明文或 `{nthash}`） |
| `ldap` | `ldap` 處理器拒絕 | 密碼錯誤、不在 `required_groups`；目錄連不上或服務帳號登入失敗時，記錄另有 `auth ldap handler:` 開頭的訊息 |
| `lua`、`script` | 腳本拒絕 | 腳本的邏輯，或腳本出錯（記錄有 `Lua script error`，附檔案和行號） |
| `none` | 所有處理器都略過 | 帳號不存在（`sql` 查不到、`ldap` 找不到）。先看 `account` 是不是你預期的名稱 |
| `no_password_check` | `all_accept` 串接裡沒有人比對過密碼 | 帳號不在 `sql`、`ldap` 裡，只有後面的腳本說可以 |
| `normalization` | 名稱被擋在串接之前 | 名稱處理後是空的，或不符合 `[auth.username] allowed` |
| `eap-tls`、`eap-peap`、`eap-ttls` | EAP 對話以拒絕結束 | 前一行通常有 `EAP conversation with client ... failed` 或 `refused` 和原因，見下面「Wi-Fi（802.1X）」 |

**密碼明明正確：**

- `account` 和資料表裡的名稱一樣嗎？大小寫、有沒有帶 realm。見 `[auth.username]`（[usage 3.6 節](../usage.md#36-使用者名稱正規化-username)）。
- 記錄有 `RADIUS response attribute error` 嗎？那是回覆屬性的名稱或值不對（`reply_query`、`group_reply`、腳本的 `set_attr`），這時就算密碼正確也會拒絕。
- 有沒有紀錄根本不是這台 rradius 寫的？多節點時到每一台找。

**完全找不到這次登入的紀錄：** 回到上一節，它被丟棄了。或 `[log] auth = false` 把紀錄關了。

## 登入很慢或時好時壞

| 看什麼 | 代表 |
| --- | --- |
| 記錄有 `RADIUS authentication overloaded: discarded N packets` | 同時處理的請求到了 `max_auth_inflight`（預設 4096）。這是結果，不是原因：有東西讓每個請求變慢了 |
| `rradius_radius_duplicates_total` 增加 | NAS 在重送，處理時間超過它的重送間隔 |
| 記錄有 `sql_query timed out`、`sql_execute timed out` | 資料庫慢，或叢集沒有多數節點。這樣的登入不回應（`rradius_radius_discarded_total{reason="database"}`），NAS 會重送或改問別台 |
| 記錄有 `auth ldap handler:` 的逾時 | 目錄慢或連不上 |
| `rradius_script_http_requests_total{result="timeout"}` 增加 | 腳本呼叫的 API 慢（[第 10 章](10-external-api.md#api-變慢或停擺時)） |

找出慢的那一段：

- **密碼雜湊：** bcrypt 和 argon2 刻意很慢，同時驗證的數量不超過 CPU 核心數，其餘排隊。尖峰時每秒幾百次登入就會排隊。登入量大時改用 `sha512-crypt`（[第 11 章](11-operations.md#用-user-指令)）。
- **寫入：** 每次登入都寫資料庫的腳本（配額、IP 配發、兩步驟驗證）要經過叢集，比只讀的慢。叢集剛失去多數節點時，寫入要等 `[lua] query_timeout_ms` 才失敗；沒有 leader 超過 5 秒後立刻失敗。這些登入都不回應，不是拒絕。
- **調高 `max_auth_inflight` 通常沒有用。** NAS 等幾秒就放棄了，排更長的隊只是讓更多請求白做。先解決變慢的原因。
- **計費拖慢登入：** 同時執行的計費腳本有上限（CPU 核心數減 2），留了執行緒給登入；但兩者共用資料庫，計費腳本很重時登入還是會受影響。

## 計費一直重送，或計費不見了

**NAS 一直重送同一筆計費：** 計費腳本沒有回傳 `'accept'`，所以 rradius 沒有回覆。

- 記錄有 `Lua script error (radius/accounting)`：腳本出錯，看檔案和行號。
- 記錄有 `Lua script did not accept accounting`：腳本正常結束但回傳了別的值。不想記錄的封包也要回 `'accept'`。
- 記錄有 `sql_execute timed out`：資料庫寫不進去。設定計費暫存後，這種情況會先寫到本機再回覆（[usage 2.11 節](../usage.md#211-計費暫存-radiusaccounting_spool)）。

**計費少了：**

| 檢查 | 代表 |
| --- | --- |
| `rradius_acct_spool_bytes` 大於 0 | 還在暫存裡等重放，還沒進資料庫。資料庫恢復、腳本正常後會自己清空 |
| 暫存目錄有 `rejected.log` | 重放時被放棄的請求：腳本拒絕、SQL 語法或約束錯誤、client 已經不存在。檔案不會自動清理，也不會再重放 |
| 暫存目錄有 `.corrupt` 結尾的檔案或 `truncated.log` | 斷電或磁碟問題留下的，裡面的請求沒有重放 |
| `rradius_acct_spool_full_total` 增加 | 暫存滿了或寫入失敗，這些請求沒有回覆，靠 NAS 重送 |
| 記錄有 `RADIUS accounting overloaded` | 到了 `max_acct_inflight`，封包被丟棄 |

**暫存一直不降（`rradius_acct_spool_bytes` 持續大於 0）：** 重放卡在最前面那一筆。資料庫還沒恢復，或腳本每次都出錯。看記錄裡 `Lua script` 和 `accounting spool` 開頭的訊息；修好腳本並送 SIGHUP 後會自動繼續。

**同一筆計費記了兩次：** 腳本不能重複執行。NAS 更新 `Acct-Delay-Time` 重送時會換新的 Identifier，rradius 認不出是重送；暫存重放也可能重跑。寫入要用 `INSERT ... ON CONFLICT` 或 `INSERT OR IGNORE`（[第 1 章](01-concepts.md#計費要能重複執行)）。

## Wi-Fi（802.1X）

| 現象 | 原因 |
| --- | --- |
| 記錄有 `EAP conversation with client ... failed`，原因提到 `State` | 對話的後半段到了另一個節點。同一次登入要留在同一個節點：NAS 用主備，或負載平衡器依來源固定（[第 7 章](07-ha-cluster.md)） |
| `... failed`，原因是 TLS 錯誤 | 用戶端不信任伺服器憑證而中斷（裝置上沒有 CA、伺服器名稱不符），或 TLS 版本、加密套件不合 |
| `... refused` | 憑證（或卡片）通過，但名稱不符，或串接沒有接受：`certificate_query` 查不到、`ldap` 沒設 `certificate_filter` |
| `the chain did not accept the inner name` | PEAP、TTLS 的密碼錯誤或查無帳號 |
| `the chain accepted the inner name without checking a password` | 接受的處理器是沒設 `verifies_password` 的 `lua` |
| `this client has no chain to check a password with` | PEAP、TTLS 需要認證串接，這個 client 沒有 |
| PEAP 一直失敗，PAP 測試卻可以 | 密碼不是明文或 `{nthash}`；或 `ldap` 用了 `bind` 模式 |
| 用戶端選了你不想給的方法 | 那個 client 的 `eap_methods` 列了它。只想收憑證就只列 `["tls"]` |
| `rradius_eap_sessions_total{result="timeout"}` 高 | 用戶端沒有走完：使用者看到憑證警告後取消、訊號不好，或 NAS 的 EAP 逾時太短 |
| `result="evicted"` 持續增加 | 同時進行的對話超過 `max_sessions` 或 `max_sessions_per_client`。所有使用者都經過同一台控制器時，調高後者 |
| `result="overload"` | 同一台設備同時開太多對話（`max_sessions_per_device`） |
| 撤銷的問題 | [第 8 章](08-wifi-certs.md#常見問題) |

**看不到內層的名稱：** PEAP、TTLS 的認證紀錄裡，`user` 是外層的匿名名稱，`account` 才是真正的帳號。用 `[log.trace]` 追蹤時比對的是外層名稱，所以要用 `clients = [...]` 追蹤整台 NAS，或列出外層實際用的名稱。

## TACACS+

| 現象 | 原因 |
| --- | --- |
| 連線馬上被關閉，記錄有 `TACACS+ connection from unknown client` | 來源不在 `[[tacacs.clients]]` |
| 認證回 ERROR，記錄有 `a body did not decode, so the shared secret may be wrong` | 共用密碼不同。`rradius_tacacs_malformed_total` 也會增加 |
| 能登入，但每個指令都被拒絕 | 授權腳本拒絕，或授權群組沒有任何一條規則成立（[第 6 章](06-device-admin.md#常見問題)） |
| `enable` 一律失敗，記錄有 `TACACS+ enable request refused` | 預設拒絕 enable。設備端不要把 rradius 放進 enable 的方法，或設 `enable = "login"` |
| CHAP、MS-CHAPv2 登入收到 RESTART | 這個 client 沒有認證串接，或串接裡只有沒設 `verifies_password` 的腳本 |
| 登入一直回 ERROR | 一條連線上未完成的登入超過 8 個，或一次登入超過 60 秒 |
| `TACACS+ overloaded: closed N connections` | 到了 `max_connections`。設備開了連線不關時，調低 `idle_timeout_ms` |

## 啟動失敗

看結束的狀態碼：

| 狀態碼 | 意思 | 常見原因 |
| --- | --- | --- |
| 2 | 設定有錯，什麼都還沒開始 | 訊息會指出檔案和欄位：欄位拼錯、檔案不存在、腳本語法錯誤、憑證和私鑰不配對、金鑰檔權限太寬、migration 檔名或內容有問題 |
| 1 | 設定沒問題，但環境不允許 | RADIUS 的 UDP 埠綁不了（被占用、沒有權限，訊息有位址和埠）；叢集在 `migrations_wait_ms` 內沒有就緒（其他節點沒起來、連不到） |

- **TACACS+、指標的埠綁不了時不會結束，** 只記一行錯誤，其他服務照常。所以 TACACS+ 設了 49 埠卻沒有權限時，程序看起來正常，但設備連不上；啟動後確認記錄裡有每個埠的 `listening`。
- **`migration file '...' changed after it was applied`：** 改了套用過的檔案。改回原本的內容，要變更就新增下一個編號的檔案。開發環境可以停掉伺服器、刪掉 `data/` 重來。
- **節點反覆以狀態碼 1 結束，記錄有 `cluster was not ready to apply migrations`：** 多數節點還沒上線；或 `migrations_wait_ms` 比最慢的 SQL 檔執行時間短。
- **一直出現 `Raft node has not been initialized`：** 節點還沒加入叢集。其他節點啟動了嗎？彼此連得到 raft 和 api 的埠嗎？`raft.secret` 相同嗎？
- **啟動前先用 `check`** 可以抓到狀態碼 2 的大部分問題，但它不連資料庫和目錄，SQL 寫錯、LDAP 密碼錯誤要啟動後才知道。

## 重新載入沒有生效

- **記錄有 `reload failed, keeping the running configuration: ...`：** 新設定有錯，服務繼續用舊的。修正後再送一次 SIGHUP。`rradius_reload_total{result="error"}` 會增加。
- **記錄有 `these need a restart to apply: ...`：** 列出的設定改了但要重啟才會套用；其他的已經套用。清單見[第 11 章](11-operations.md#重新載入sighup)。
- **只有一台生效：** 每個節點各自重新載入，檔案要放到每一台、信號要送給每一台。
- **改了字典檔：** 字典不會重讀，要重啟。
- **改了 migration 目錄：** 新的檔案只在啟動時套用。

## 叢集

| 現象 | 原因 |
| --- | --- |
| 某台的登入結果和其他台不同 | 那台的 `migrations/`、腳本或設定檔和別台不同；或它被隔開了（`rradius_cluster_leader` 是 0），讀的是舊資料 |
| 某台有些登入不回應 | `rradius_radius_discarded_total{reason="database"}`：它連不到叢集資料庫；`reason="no_leader"`：設了 `stop_without_leader_ms`，它沒有 leader 太久（[第 7 章](07-ha-cluster.md)） |
| 重啟的節點過一陣子才開始回應 | 正常：它等本機副本追上叢集才回應，記錄有 `has caught up with the cluster`。停機很久時追得比較久，上限是 `migrations_wait_ms` |
| 剛寫入的資料讀不到 | `sql_query` 讀本機副本，可能比寫入慢一點。判斷寫入有沒有成功用 `sql_execute` 的回傳值，不要再查一次 |
| 寫入都逾時 | 失去多數節點（三台剩一台）。只讀的登入照常，要寫入的登入不回應、TACACS+ 回 ERROR，計費有暫存時進暫存，等節點回來 |
| 節點自己結束，狀態碼 1，記錄有 `exiting so that a restart recovers it` | 它的 Raft 核心停了（常見原因：斷線期間叢集寫入超過 `snapshot_every`），或 hiqlite 內部 panic。重新啟動它就會追上；有設自動重啟時不必處理（[第 7 章](07-ha-cluster.md)） |
| 停止一個節點要等 15 秒左右 | 正常：多節點時先照常回應 5 秒讓負載平衡器移出它（`[shutdown] ready_delay_ms`），hiqlite 關閉前再等 9.5 秒 |
| 停止時記錄 `stopping: after N ms, ... they are cut off` | 等了 `[shutdown] drain_ms` 還有請求沒做完（腳本、LDAP 或資料庫太慢），那些請求沒有回應，設備會重送到別台（[usage 2.16 節](../usage.md#216-停止-shutdown)） |
| 剩一台時重啟，它一直起不來（有 migration 時以狀態碼 1 結束） | 重新啟動的節點要等到有 leader。先讓其他節點回來（[第 7 章](07-ha-cluster.md)） |
| 異常結束後啟動很慢 | 要重播上次快照以來的寫入。見 [usage 2.2 節](../usage.md#22-cluster)的 `snapshot_every` |
| 每個節點行為不同的功能 | 計費暫存、EAP 對話、追蹤的計時，都是每個節點自己的 |

## 記錄本身

- **`rradius_log_dropped_lines_total` 增加：** 記錄寫得比輸出的去處快，或寫入失敗（磁碟滿、接收的程序結束）。被丟掉的行就沒有了，服務不受影響。檢查 stderr 接到哪裡；流量很大時關掉 `[log] auth`，或不要對整台 NAS 開追蹤。
- **結尾是 `(N since the last line)` 的訊息：** 任何人都能觸發的訊息（陌生來源、握手失敗）每種每秒最多一行，N 是這段時間的總數。
- **`hiqlite` 的 WARN、ERROR：** 叢集啟動和關閉時會有幾行，例如 `No metadata exists inside the DB yet`，是正常的。
- **`script ...: ...` 開頭的行** 是腳本用 `log()` 寫的，不是錯誤。
- **所有訊息都在 stderr。** 管理指令 `user` 的結果在 stdout。
