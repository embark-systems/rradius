# 11. 日常維運

這一章整理上線之後常做的事。每一項的完整規則在參考文件，這裡只講做法和要注意的地方。出了問題要從現象找原因時，見[第 12 章](12-troubleshooting.md)。

## 上線前的檢查清單

- [ ] 所有 `*.secret` 都換成新產生的值（見下方「更換密碼」），檔案權限只有執行 rradius 的帳號讀得到。
- [ ] 憑證不是示範用的；伺服器憑證、NAS 和設備憑證的到期日都納入監控。
- [ ] 刪掉 `migrations/` 裡的示範資料檔案，或在第一次啟動前換成正式資料。
- [ ] `[[radius.clients]]` 只列真的會來問的設備，不要用 `0.0.0.0/0`。
- [ ] `[metrics]` 只綁內部位址；多節點時 `[cluster] listen` 綁 WireGuard 或內部網路的位址。
- [ ] 防火牆只開用到的埠，並限制來源：1812、1813（UDP）和 49（TACACS+）。
- [ ] `rradius-server --check --config ...` 通過。
- [ ] 跑一次 `rradius-server tuning --config ...`，照建議調好接收緩衝和檔案描述元上限（接收緩衝建議用 `[radius] recv_buffer_bytes` 設定，只需調高系統上限，使用文件 2.3 節）；它只讀取不修改，建議值是起點，上線後看報告開頭的丟包數確認（使用文件 8.1 節）。
- [ ] 用 systemd 之類的服務管理器啟動，停止時送 SIGTERM（不要 SIGKILL）並給 30 秒以上，資料目錄和計費暫存的目錄放在持久的本機磁碟。
- [ ] 設好自動重新啟動（systemd `Restart=on-failure`、Docker `restart: unless-stopped`）：節點的資料庫壞掉時，rradius 會以狀態碼 1 結束，等重新啟動來恢復（[第 7 章](07-ha-cluster.md)）。
- [ ] 啟動後看記錄，每個要開的埠都有一行 `listening`。TACACS+、指標的埠綁不了時只記一行錯誤，程序不會結束。
- [ ] 用實際的設備（不只是 `radtest`）各試一次登入和計費。
- [ ] 監控至少接上「節點活著」（Status-Server）和下面列的幾個指標。

## 帳號和資料

帳號存在 rradius 自己的資料表時，用 `rradius-server user` 指令管理；帳號在別的地方時，讓 rradius 去問那裡：

| 做法 | 適合 | 怎麼做 |
| --- | --- | --- |
| **`user` 指令** | 帳號常變動（寬頻用戶、員工、訪客） | `rradius-server user add\|passwd\|enable\|disable\|delete\|show`，下一次登入就看得到 |
| **migration 檔** | 很少變動、想留下變更紀錄的資料（方案、設備、資料表結構） | 新增下一個編號的 SQL 檔，放到每個節點，逐台重啟 |
| **由現有的目錄驗證** | 公司已經有 AD／LDAP | `ldap` 處理器直接查目錄，rradius 不存帳號（第 5 章） |
| **由現有的系統驗證** | 帳號在帳務、會員系統裡 | 在 `lua` 處理器用 `http_request` 問那個系統的 API（第 10 章） |
| **從舊系統搬過來** | 要汰換舊的認證系統 | 把舊的密碼雜湊原樣匯入，使用者不必重設密碼（第 9 章） |

### 用 `user` 指令

先在設定檔加上 `[accounts]`，寫出每個動作對應的 SQL（`?1` 是帳號，`?2` 是密碼）。以快速開始的 `users` 表為例：

```toml
[accounts]
add = "INSERT INTO users (username, password) VALUES (?1, ?2)"
passwd = "UPDATE users SET password = ?2 WHERE username = ?1"
enable = "UPDATE users SET enabled = 1 WHERE username = ?1"
disable = "UPDATE users SET enabled = 0 WHERE username = ?1"
delete = "DELETE FROM users WHERE username = ?1"
show = "SELECT username, enabled FROM users WHERE username = ?1"
```

不用重啟伺服器，指令每次都讀設定檔。然後：

```bash
rradius-server user add --config rradius.toml dave        # 問兩次密碼，不顯示輸入
rradius-server user disable --config rradius.toml carol   # 停用；user_query 要只查 enabled = 1
rradius-server user show --config rradius.toml dave
```

- **改的是整個叢集，** 不用逐台處理，也不用重新載入。叢集要在執行，執行指令的主機要連得到節點的 api 埠。
- **密碼從標準輸入讀，** 不會留在 shell 歷史。批次匯入時一行一個帳號，從管線送進去。
- **密碼怎麼存由 `scheme` 決定：** 預設 bcrypt；登入量大改 `sha512-crypt`；用 CHAP（例如 PPPoE）要 `clear`；用 MS-CHAPv2 或 PEAP 要 `nthash`。寬頻情境的範例設定用 `clear`。
- **名稱和登入時一樣處理：** 有 `[auth.username]` 時，`Dave@Corp` 會依設定存成 `dave`。
- 完整的設定規則、輸出和狀態碼見 [usage 2.14 節](../usage.md#214-帳號管理-accounts)。
- **設定檔要保護好：** 這些指令用 `secret_api` 連到叢集，它等於整個資料庫的鑰匙。只讓管理者讀得到設定檔和密碼檔，並在節點上或經過 WireGuard 執行。

### 用 migration 檔

```sql
-- 3_plans.sql
INSERT INTO plans (name, max_sessions) VALUES ('family-500m', 3);
UPDATE users SET enabled = 0 WHERE username = 'carol';
```

- **每個檔案只會執行一次，** 而且套用後不能修改。所以它適合當「變更紀錄」，不適合頻繁改密碼。
- 不要拿其他工具直接連叢集的 api 埠改資料。
- **`user` 指令只管帳號。** 其他資料表用 migration 檔。

## 清除舊資料

`accounting`、`tacacs_log` 這類表會一直變大，rradius 不會自動清除。可以在計費腳本裡定期刪除：每小時最多一次，整個叢集只有一個請求會真的去做。

先用 migration 建一張記錄時間的表：

```sql
-- 3_housekeeping.sql
CREATE TABLE housekeeping (name TEXT PRIMARY KEY, last_run INTEGER NOT NULL);
INSERT INTO housekeeping VALUES ('purge-accounting', 0);
```

在計費腳本 `return 'accept'` 之前加上：

```lua
-- 每小時最多一次：先讀本機副本，不必每個請求都寫入叢集。
local now = math.floor(now_unix())
local due = sql_query("SELECT 1 FROM housekeeping WHERE name = 'purge-accounting' AND last_run < ?", {now - 3600})
-- 寫入經過叢集，只有一個請求能把 last_run 改成功，它負責刪除。
if #due > 0 and sql_execute("UPDATE housekeeping SET last_run = ? WHERE name = 'purge-accounting' AND last_run < ?",
    {now, now - 3600}) == 1 then
  -- 一次最多刪 10000 列，太多就留到下一個小時，避免一次寫入太久。
  -- ORDER BY 讓每個節點刪到同樣的列。
  sql_execute([[DELETE FROM accounting WHERE rowid IN
    (SELECT rowid FROM accounting WHERE stopped IS NOT NULL AND stopped < ? ORDER BY rowid LIMIT 10000)]], {now - 180 * 86400})
end
```

- 保存多久（範例 180 天）依法規和公司政策決定。業者可能有最短保存期限，**刪除前請先確認**。
- 刪除在處理一個計費請求時執行，那個請求會晚一點回覆。刪除的量要能在 `[lua] query_timeout_ms` 內完成。
- 要保留歷史資料時，先由外部的匯出流程把資料搬走，再刪除。

## 重新載入（SIGHUP）

**改設定檔之前：** 用 `rradius-server schema` 產生 schema 給編輯器用，寫錯的欄位在編輯時就會標出來（使用文件第 2 節）；改完仍要跑 `--check`。

大部分的設定改完送 SIGHUP 就生效，不必重啟：

```bash
kill -HUP $(pidof rradius-server)
systemctl reload rradius        # 服務檔有 ExecReload=/bin/kill -HUP $MAINPID 時
```

記錄會寫 `configuration reloaded from ...`，或 `reload failed, keeping the running configuration: ...`（這時繼續用舊設定，修正後再送一次）。

| 改了什麼 | 生效方式 |
| --- | --- |
| 腳本、認證串接和名稱正規化、`[lua]` 的時限、`[lua.http]` | SIGHUP |
| RADIUS 和 TACACS+ 的 client 名單、共用密碼檔、TACACS+ 的授權群組和 `enable` | SIGHUP |
| EAP 的憑證、私鑰、CA 和撤銷清單；`[radius.eap]` 的其他設定（只影響新的對話） | SIGHUP |
| `[log.trace]` | SIGHUP |
| 字典、`[cluster]`（含新的 migration）、`[log]` 的其他設定、`[metrics]` | 重啟 |
| 所有監聽位址和埠；新增或移除 `[radius]`、`[tacacs]` | 重啟 |
| 同時處理的上限（`max_auth_inflight`、`max_acct_inflight`、`max_connections`）、`idle_timeout_ms`、`[lua] max_memory_mb` | 重啟 |
| 計費暫存（`[radius.accounting_spool]`） | 重啟 |

- **要重啟的設定改了時，** SIGHUP 會在記錄列出它們（`these need a restart to apply: ...`），其他的照常套用。
- **全部正確才會套用。** 有一項錯，整份維持原狀，服務不中斷。
- **進行中的請求用舊設定做完。** 已開啟的 TACACS+ 連線不會斷，但下一個封包依新名單處理：client 被移除時連線關閉。

完整的清單見 [usage 2.9 節](../usage.md#29-重新載入sighup)。**每個節點各自重新載入，** 叢集要對每一台更新檔案並送信號。

## 更換密碼

| 密碼 | 怎麼換 |
| --- | --- |
| NAS 的共用密碼（`secret_file`） | 一台一台換：先改 NAS 那邊，再改 rradius 的檔案並送 SIGHUP；或反過來，看哪邊可以先改。中間那一段這台 NAS 的請求會失敗。每台 NAS 用不同的密碼，就能一次只影響一台 |
| `ldap.secret` | 先在目錄改服務帳號的密碼，再改檔案並送 SIGHUP |
| `raft.secret`、`api.secret` | 所有節點要相同，**要整個叢集一起停下來換**，安排維護時間 |
| TACACS+ 的共用密碼 | 和 NAS 的相同。SIGHUP 後，已開啟的連線下一個封包就要用新密碼 |
| 憑證和私鑰 | 換掉檔案後送 SIGHUP。新連線、新對話用新的，已開啟的不受影響 |

產生新密碼：

```bash
(umask 077; openssl rand -base64 24 > nas.secret)
```

共用密碼少於 16 個字元時，啟動、重新載入和 `--check` 都會有警告（`has a shared secret shorter than 16 octets`），但不會拒絕啟動。有些舊設備限制長度或字元，產生後先確認設備接受。

## 監控

`[metrics]` 開啟後，`GET /metrics` 是 Prometheus 格式。建議先看這幾個：

| 指標 | 代表 | 建議 |
| --- | --- | --- |
| `rradius_radius_responses_total{code="access_reject"}` 的增加速度 | 登入失敗 | 突然增加：密碼外洩嘗試、目錄故障、設定錯誤 |
| `rradius_radius_discarded_total{reason="unknown_client"}` | 不認得的來源 | 增加：新設備沒加進名單，或有人在掃描 |
| `rradius_radius_discarded_total{reason="invalid"}` | 簽章、格式不對 | 增加：共用密碼設錯 |
| `rradius_radius_discarded_total{reason="overload"}` | 太忙丟棄 | 大於 0 就要查：資料庫慢、腳本慢 |
| `rradius_acct_spool_bytes` | 還沒寫進資料庫的計費 | 持續大於 0：資料庫或叢集有問題 |
| `rradius_cluster_leader` | 這台有沒有可用的 leader | 0 超過幾秒：這台被隔開或叢集沒有多數 |
| `rradius_radius_duplicates_total` 的增加速度 | NAS 在重送 | 持續增加：處理比 NAS 的重送間隔慢 |
| `rradius_acct_spool_rejected_total` | 重放時被放棄的計費 | 大於 0 就去看暫存目錄的 `rejected.log` |
| `rradius_eap_sessions_total{result="timeout"}`、`{result="evicted"}` | 沒走完的、被擠掉的 802.1X 對話 | 持續增加，見[第 12 章](12-troubleshooting.md#wi-fi8021x) |
| `rradius_script_http_requests_total{result="timeout"}` | 腳本呼叫的 API 逾時 | 增加：API 有問題 |
| `rradius_reload_total{result="error"}` | 重新載入失敗 | 大於 0 就要看記錄 |
| `rradius_log_dropped_lines_total` | 記錄來不及寫 | 增加：記錄的去處太慢 |

活著與否用 Status-Server 檢查最準確，它走和認證相同的路徑：

```bash
echo 'Message-Authenticator=0x00' | radclient -t 2 -r 1 10.99.0.1:1812 status "$(cat nas.secret)"
```

全部指標見 [usage 2.8 節](../usage.md#28-統計-metrics)。

- **數值從啟動開始累計，** 每個節點各算各的，重新載入不會歸零。告警要看增加的速度，不是絕對值。
- **`/metrics` 沒有認證也沒有 TLS，** 只綁內部位址。

## 太忙的時候會發生什麼

rradius 太忙時不會回錯誤，而是**不回應**，讓 NAS 重送或改用備援：

- **同時處理的請求有上限：** 登入 `max_auth_inflight`、計費 `max_acct_inflight`，預設各 4096，分開計算，所以計費暴增不會占掉登入的名額。超過的封包直接丟棄，記錄每秒最多一行 `RADIUS ... overloaded: discarded N packets`，`rradius_radius_discarded_total{reason="overload"}` 增加。
- **NAS 的重送不會重做一次：** 同一個請求還在處理時，重送的封包丟棄；處理完 10 秒內的重送直接回上次的答案，不再查資料庫或目錄。
- **調高上限通常沒有用。** 合適的值大約是「每秒處理量 × NAS 的重送逾時」，再大也只是讓更多請求排到 NAS 已經放棄。出現過載時，先找出是什麼讓每個請求變慢（資料庫、目錄、上游、API、密碼雜湊），見[第 12 章](12-troubleshooting.md#登入很慢或時好時壞)。
- **TACACS+** 用連線數當上限（`max_connections`，預設 1024），超過時新連線立刻關閉。
- **Linux 上** 每個 UDP 埠預設開和 CPU 數一樣多的接收 socket；專門跑 rradius 的實體機還可以開 `[runtime] pin_threads = true`。其他平台只有一個 socket。見 [usage 2.4 節](../usage.md#24-過載與重送)。

## 計費暫存的日常

設了 `[radius.accounting_spool]`（[usage 2.11 節](../usage.md#211-計費暫存-radiusaccounting_spool)）之後：

| 要做的事 | 做法 |
| --- | --- |
| 知道現在有沒有落後 | `rradius_acct_spool_bytes` 大於 0 就是有。持續不降就告警 |
| 估計能撐多久 | 每筆約一百多個位元組加上 NAS 送的屬性。預設上限 1 GiB，滿了之後新的計費不回覆，靠 NAS 重送 |
| 查看被放棄的 | 暫存目錄的 `rejected.log`：重放時再跑一次結果也一樣的請求（腳本拒絕、SQL 錯誤、client 已不存在）。**不會自動清理，也不會再重放** |
| 查看損壞的 | `.corrupt` 結尾的檔案和 `truncated.log`：斷電或磁碟問題留下的 |
| 停機維護 | SIGTERM 會停止重放，資料留在磁碟，下次啟動繼續 |
| 換掉節點 | 先等這台的暫存清空（指標為 0）再關機，否則它還沒重放的就留在舊磁碟上 |
| 改設定 | 暫存的設定要重啟才套用 |

暫存檔是原始封包，有使用者名稱和位址。目錄權限是 0700，備份和報廢磁碟時一併考慮。

## 排查某個使用者的問題

**1. 先看認證紀錄。** 每次登入一行：

```
authentication protocol="radius" client="wlc" source=10.30.0.10 user="Staff1@corp.example.com" account="staff1" chain="default" handler="sql" decision="reject"
```

`user` 是設備送來的名稱，`account` 是查帳號用的名稱，`chain`、`handler` 是誰做的決定。

**2. 需要看完整封包時，暫時開追蹤：**

```toml
[log.trace]
users = ["staff1"]
minutes = 30
```

送 SIGHUP 後，這個使用者的請求和回應會完整寫進記錄（密碼顯示為 `<hidden>`），30 分鐘後自動停止。也可以用 `clients = ["wlc"]` 追蹤一台設備。**記錄裡會有個人資料，** 用完就移除這個區段。

**3. 腳本的問題** 看 `Lua script error (<protocol>/<phase>): <檔案>:<行號>: <訊息>`。腳本裡可以用 `log(...)` 印出自己的訊息。

常見訊息的意思見 [usage 第 8 節](../usage.md#8-疑難排解)。

## 升級

1. 看新版本的變更說明，特別是標了「升級注意」的項目。
2. 用新版本對現有設定跑 `--check`。
3. 單機：停止 → 換執行檔 → 啟動。叢集：一次一台，見[第 7 章](07-ha-cluster.md#升級或重開機)。

## 備份

rradius 沒有內建的備份指令。叢集本身有三份副本，可以撐過一台主機壞掉，但**誤刪的資料會複寫到每一台，** 所以還是要另外保存：

- **帳號的來源要能重建：** migration 檔放進版本控制；帳號來自目錄或其他系統時，那邊才是正本。
- **計費這類只存在 rradius 的資料，** 由外部流程定期匯出到帳務系統或資料倉儲。
- **設定檔、腳本、憑證** 放進版本控制或設定管理工具；私鑰和 `*.secret` 用你們管理機密的方式另外保存。

定期把整個資料庫存成副本（每個節點、一個目錄或 S3）並從副本還原整個叢集，需要 rradius-enterprise 的 `[cluster.backup]`，見企業版手冊。
