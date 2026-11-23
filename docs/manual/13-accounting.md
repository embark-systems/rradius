# 13 計費與 session

> 本章讓你做到：
> - 收下 NAS 的計費請求，寫進資料表之後才回覆
> - 設計計費表與計費腳本，承受重送與晚到的封包
> - 打開計費 spool，在叢集資料庫暫時寫不進去時不漏帳
> - 把每一筆計費寫成本機的 JSON 記錄檔
> - 用 `session` 指令查詢上線中的 session

前置條件：做過[第 03 章](03-quick-start.md)，手上有 `~/rradius-lab/quick-start`；會寫簡單的 Lua 腳本（[第 10 章](10-lua-policies.md)）。本章的指令都在 `~/rradius-lab/quick-start` 執行，伺服器要在跑。

本章小節：

- [計費請求怎麼處理](#計費請求怎麼處理)
- [什麼時候不回覆](#什麼時候不回覆)
- [看懂計費腳本](#看懂計費腳本)
- [設計計費表](#設計計費表)
- [送計費做測試](#送計費做測試)
- [腳本出錯時會怎樣](#腳本出錯時會怎樣)
- [查詢上線的 session](#查詢上線的-session)
- [計費記錄檔](#計費記錄檔)
- [計費 spool](#計費-spool)
- [腳本怎麼知道是重送（replay）：replayed](#腳本怎麼知道是重送replayreplayed)
- [TACACS+ 的計費](#tacacs-的計費)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

## 計費請求怎麼處理

NAS 在 session 開始、進行中、結束時各送一個 Accounting-Request（RFC 2866），說明誰在線、用了多久、傳了多少資料。rradius 收到之後：

1. 用來源 IP 比對 `[[radius.clients]]`，再用那個用戶端的共用密碼檢查請求的 Authenticator。比對不到或檢查不過，直接丟掉，不回覆（第 05 章）。
2. 重送偵測：同一個請求在處理中又到，丟掉；已經回覆過的，10 秒內再到就重送原來的回覆，不再跑腳本（第 05 章）。
3. 交給最上層腳本（`[lua] script`），`phase` 是 `accounting`。驗證鏈（`[auth]`）不處理計費。
4. 腳本回傳 `accept`，才回 Accounting-Response。

最後一點是整個計費設計的核心：**回覆就等於「我記下來了」**。NAS 沒收到回覆會重送（retransmit），重送幾次、隔多久由 NAS 決定。所以 rradius 只在確定寫好之後才回覆，寫不進去就不回，讓 NAS 再送一次。

計費腳本看得到請求裡的每一個屬性，名稱照字典（第 06 章）。列舉型的屬性用數字比較，例如 `Acct-Status-Type`：

| 值 | 意思 |
| --- | --- |
| `1` | Start |
| `2` | Stop |
| `3` | Interim-Update |
| `7` | Accounting-On（NAS 開機） |
| `8` | Accounting-Off（NAS 關機） |

腳本另外看得到 `client`（用戶端的名稱）、`protocol`（`"radius"`）、`phase`，以及從計費 spool 重送時才是 true 的 `replayed`。

## 什麼時候不回覆

| 情況 | 回覆 | 日誌 |
| --- | --- | --- |
| 腳本回傳 `accept` | Accounting-Response | 無 |
| 腳本回傳 `reject` 或沒有回傳值 | 不回覆 | `Lua script did not accept accounting (radius, client <名稱>)` |
| 腳本出錯、超過 `max_execution_ms` | 不回覆 | `Lua script error (radius/accounting): …` |
| 資料庫呼叫失敗（逾時、沒有 leader），有計費 spool | 寫進 spool 之後回覆 | 腳本的錯誤 |
| 資料庫呼叫失敗，沒有計費 spool | 不回覆（腳本捕捉錯誤後仍回傳 `accept` 的除外） | 腳本的錯誤 |
| 計費 spool 滿了或寫不進磁碟 | 不回覆 | `accounting spool: request from client <名稱> not written: …` |

不回覆的請求不算進任何 `rradius_radius_discarded_total`。要看有多少計費沒有回覆，比較 `rradius_radius_packets_total{kind="acct"}` 與 `rradius_radius_responses_total{code="accounting_response"}` 的差。

## 看懂計費腳本

quick-start 的 `accounting.lua` 是一份可以直接拿去用的計費腳本。它在每個 session 一列的 `accounting` 表裡新增或更新。逐段看：

```lua
-- accounting.lua（節錄）
if phase ~= 'accounting' then
  return 'reject'
end
```

quick-start 有 `[auth]`，認證不會來到這裡。萬一設定改了，也不讓這個腳本放行任何人。

```lua
local nas = get_attr('NAS-IP-Address') or get_attr('NAS-IPv6-Address') or get_attr('NAS-Identifier')
if not nas or nas == '' then
  nas = client
end
```

一個用戶端可以是整個網段，所以用 NAS 自己報的位址或名稱再分開。這些值是 NAS 自己說的，但只在同一個用戶端裡比較：同一個用戶端的 NAS 共用一把密碼，本來就同樣可信。

```lua
local status = get_attr('Acct-Status-Type')
if status == 7 or status == 8 then
  sql_execute("UPDATE accounting SET stopped = updated, terminate_cause = 11 WHERE client = ? AND nas = ? AND stopped IS NULL",
    {client, nas})
  return 'accept'
end
if status ~= 1 and status ~= 2 and status ~= 3 then
  return 'accept'
end
```

NAS 重開機時送 Accounting-On 或 Off，表示它之前的 session 都已經結束，這裡把它們全部標成結束。其他不認得的種類直接 `accept`：不回覆只會讓 NAS 一直重送。

```lua
local session = get_attr('Acct-Session-Id')
if not session or session == '' then
  log('accounting without Acct-Session-Id from', client)
  return 'accept'
end
local seen = math.floor(now_unix()) - math.floor(get_attr('Acct-Delay-Time') or 0)
local seconds = math.floor(get_attr('Acct-Session-Time') or 0)
local function octets(name)
  return math.floor(get_attr(name .. '-Octets') or 0) + math.floor(get_attr(name .. '-Gigawords') or 0) * 4294967296
end
```

- 沒有 Acct-Session-Id 的請求記不下來，重送也不會比較好，所以寫一行日誌然後 `accept`。
- `Acct-Delay-Time` 是 NAS 說「這件事發生在幾秒前」。用現在的時間減掉它，才是事情真正發生的時間。從計費 spool 重送時，rradius 會把待在 spool 的時間加進這個值（見[計費 spool](#計費-spool)），所以同一行程式也處理了重送。
- 超過 4 GiB 的流量，NAS 把進位放在 `Acct-Input-Gigawords`、`Acct-Output-Gigawords`。

```lua
sql_execute([[
  INSERT INTO accounting (client, nas, session_id, username, started, updated, session_time, input_octets, output_octets)
  VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
  ON CONFLICT (client, nas, session_id) DO UPDATE SET
    updated = CASE WHEN stopped IS NULL THEN max(updated, excluded.updated) ELSE updated END,
    session_time = max(session_time, excluded.session_time),
    input_octets = max(input_octets, excluded.input_octets),
    output_octets = max(output_octets, excluded.output_octets)
]], {client, nas, session, get_attr('User-Name') or '', seen - seconds, seen, seconds, octets('Acct-Input'), octets('Acct-Output')})

if status == 2 then
  sql_execute("UPDATE accounting SET stopped = ?, terminate_cause = ? WHERE client = ? AND nas = ? AND session_id = ? AND stopped IS NULL",
    {seen, math.floor(get_attr('Acct-Terminate-Cause') or 0), client, nas, session})
end
return 'accept'
```

這段是整份腳本最重要的設計：**同一個封包寫兩次，結果跟寫一次一樣**。

- 用 `INSERT … ON CONFLICT … DO UPDATE`：Start、Interim-Update、Stop 不管哪一個先到，都能新增或更新同一列。
- 計數用 `max()`：晚到的舊 Interim-Update 不會把流量改小。
- 結束之後才到的封包（晚送的 Interim-Update、重送的 Stop）不再改 `updated`。
- Stop 的 `UPDATE` 有 `AND stopped IS NULL`：重送的 Stop 不會改掉第一次記下的結束時間。

為什麼一定要這樣寫：NAS 沒收到回覆會重送；腳本寫了一半出錯，下一次重送會再跑一次；計費 spool 的重送保證「至少一次」，斷電時可能把同一筆再跑一次。這三種情況都會讓同一個封包進腳本不只一次。

> **注意：** 腳本裡有多個 `sql_execute` 時，前面的寫入成功、後面的失敗，NAS 會重送整個封包，前面那句會再執行一次。每一句都要能重複執行。

## 設計計費表

quick-start 的計費表（`migrations/1_schema.sql`）：

```sql
-- migrations/1_schema.sql（節錄）
CREATE TABLE accounting (
  client TEXT NOT NULL,
  nas TEXT NOT NULL,
  session_id TEXT NOT NULL,
  username TEXT,
  started INTEGER NOT NULL,
  updated INTEGER NOT NULL,
  stopped INTEGER,
  session_time INTEGER NOT NULL DEFAULT 0,
  input_octets INTEGER NOT NULL DEFAULT 0,
  output_octets INTEGER NOT NULL DEFAULT 0,
  terminate_cause INTEGER,
  PRIMARY KEY (client, nas, session_id)
);
CREATE INDEX accounting_user ON accounting (username);
```

設計的要點：

- **主鍵是 `(client, nas, session_id)`。** Acct-Session-Id 只在同一台 NAS 內唯一，兩台 NAS 可能產生一樣的 ID，所以主鍵要包含是哪台 NAS。
- **時間存 Unix 秒。** `started`、`updated`、`stopped` 都是整數，比較與計算都方便。
- **`stopped IS NULL` 就是還在線。** 但 NAS 可能沒送 Stop 就斷線，所以「在線」還要加上「最近有更新」的條件，見下一節的 `[sessions]`。
- **依查詢需要加索引。** quick-start 依 `username` 查；usage-control 情境要加總一個人在一段期間的流量，所以索引是 `(username, started)`。

migration 套用過就不能再改，要改表結構時新增下一個編號的檔案（第 10 章）。

不是每個情境都需要一張計費表。ip-pool 與 usage-control 情境的「誰在線」就記在位址池或連線位的表裡，計費腳本更新的是那張表，`[sessions]` 也直接查它（第 14 章）。

## 送計費做測試

`rradius-bench` 有這幾種計費負載：

| `--load` | 送出的請求 |
| --- | --- |
| `acct-start` | Start |
| `acct-alive` | Interim-Update，帶 `Acct-Session-Time` 與流量 |
| `acct-stop` | Stop，帶 `Acct-Session-Time` 與流量 |
| `acct-on`、`acct-off` | Accounting-On、Accounting-Off |
| `session` | PAP 登入、Start、Stop，三個用同一個 session ID |

每次執行 `rradius-bench run` 都產生新的 session ID，所以分開送的 `acct-start` 與 `acct-stop` 是兩個不同的 session。要測一個完整的 session，用 `session`（第 03 章）。

送兩個 Start，再送 Accounting-On：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --load acct-start,concurrency=1 --requests 1
…
  acct-start: 1997/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user bob --load acct-start,concurrency=1 --requests 1
…
$ rradius-server session list --config rradius.toml
username=bob
nas=192.0.2.1
session_id=1138000000000000
started=1795384997
updated=1795384997

username=alice
nas=192.0.2.1
session_id=dc80000000000000
started=1795384997
updated=1795384997
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --load acct-on,concurrency=1 --requests 1
…
  acct-on: 2954/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-server session list --config rradius.toml
no sessions
```

`rradius-bench` 的 NAS 位址都是 `192.0.2.1`，所以 Accounting-On 結束了這台「NAS」所有的 session。

要測 `Acct-Delay-Time`，用 `--attr` 加上屬性（41 是 `Acct-Delay-Time` 的編號）：

```console
$ date +%s
1795385623
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user grace --attr 41=int:600 --load acct-start,concurrency=1 --requests 1
…
$ rradius-server session show --config rradius.toml grace
username=grace
nas=192.0.2.1
session_id=2868000000000000
started=1795385023
updated=1795385023
```

`started` 比現在早了 600 秒，正是 NAS 說的延遲。

## 腳本出錯時會怎樣

把腳本換成一個會出錯的版本，看看 NAS 那邊看到什麼。先備份，再寫一個計費時一定出錯的腳本：

```console
$ cp accounting.lua accounting.lua.keep
$ printf "if phase == 'accounting' then error('no table today') end\nreturn 'reject'\n" > accounting.lua
$ kill -HUP <伺服器的 PID>
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --load acct-start,concurrency=1 --requests 1 --timeout 2
…
  acct-start: 0/s, sent 1, accept 0, reject 0, challenge 0, accounting 0, timeouts 1, invalid 0, late 0, socket errors 0, retransmits 0
```

`timeouts 1`：沒有回覆。伺服器日誌：

```text
… ERROR rradius_core::lua_engine: Lua script error (radius/accounting): runtime error: accounting.lua:1: no table today\nstack traceback:…
```

腳本改成回傳 `reject` 時，同樣沒有回覆，日誌是：

```text
… WARN rradius_core::lua_engine: Lua script did not accept accounting (radius, client localhost)
```

做完把腳本還原：`mv accounting.lua.keep accounting.lua`，再送一次 `SIGHUP`。腳本可以 reload；改壞的腳本編譯不過時，reload 失敗，伺服器繼續用原來的腳本。

> **注意：** 計費沒有回覆時，NAS 會一直重送到它放棄為止，有的 NAS 會把放棄的紀錄丟掉。腳本出錯要儘快處理；資料庫短暫不能寫入的情況，交給下面的計費 spool。

## 查詢上線的 session

rradius 自己不保存「誰在線」。`session` 指令執行 `[sessions]` 裡你寫的 SQL，把結果原樣印出來，所以「在線」的定義由你的表與你的 SQL 決定。

```toml
# rradius.toml（quick-start 的 [sessions]）
[sessions]
list = "SELECT username, nas, session_id, started, updated FROM accounting WHERE stopped IS NULL AND updated > strftime('%s', 'now') - 900 ORDER BY started, session_id"
show = "SELECT username, nas, session_id, started, updated FROM accounting WHERE username = ?1 AND stopped IS NULL AND updated > strftime('%s', 'now') - 900 ORDER BY started, session_id"
```

| 鍵 | 預設值 | reload | 意思 |
| --- | --- | --- | --- |
| `sessions.list` | 無 | 只有指令讀 | `session list` 執行的 SQL，沒有參數 |
| `sessions.show` | 無 | 只有指令讀 | `session show <名稱>` 執行的 SQL，`?1` 是使用者名稱 |

（完整說明見[附錄 A](90-config-reference.md)。）

- 兩句都只能讀：開頭（空白之後）要是 `SELECT` 或 `WITH`，大小寫不拘。
- `show` 一定要用 `?1`，而且只能用 `?1`。
- 這一節只有指令讀，伺服器執行時不看它，所以改了不必 reload，也不必重啟。不過啟動、reload 與 `check` 都會檢查它，寫錯一樣會失敗。
- quick-start 把「還沒有 Stop，而且 15 分鐘內有更新」算成在線。NAS 的 Interim-Update 間隔比 15 分鐘長時，要放寬這個條件。

指令的用法：

```console
$ rradius-server session show --config rradius.toml --json bob
[{"username":"bob","nas":"192.0.2.1","session_id":"1138000000000000","started":1795384997,"updated":1795384997}]
$ rradius-server session show --config rradius.toml nobody
no sessions for nobody
$ rradius-server session show --config rradius.toml --json nobody
[]
```

- 沒有 `--json` 時，每一列是一段 `欄位=值`，照 SELECT 的欄位順序，NULL 是空值，列與列之間空一行。
- `--json` 印出一個 JSON 陣列：NULL 是 `null`，整數與實數是數字，文字是字串，BLOB 是小寫的十六進位字串。
- `show` 的名稱照預設鏈的 `[auth.username]` 正規化，跟 `user` 指令一樣（第 07 章）。
- 指令透過叢集的 API 埠讀取，不啟動節點，也不建立資料目錄，但伺服器一定要在跑。讀取是一致讀：讀到的是叢集已經確認的資料。

結束碼：

| 結束碼 | 意思 |
| --- | --- |
| 0 | 至少印出一列 |
| 1 | 沒有符合的列：`list` 印 `no sessions`，`show` 印 `no sessions for <名稱>`；有 `--json` 時印 `[]` |
| 2 | 指令或設定有錯，什麼都沒執行；或 SQL 執行失敗（`the statement failed: …`） |
| 3 | 連不到叢集，或 30 秒內沒有回應：`the cluster did not answer within 30 s` |

寫錯 `[sessions]` 時的訊息（結束碼 2）：

| 寫法 | 訊息 |
| --- | --- |
| `list = "DELETE FROM accounting"` | `rradius.toml: sessions.list: must begin with SELECT or WITH: it only reads` |
| `show = "… WHERE username = ?"` | `rradius.toml: sessions.show: use ?1 for the user name, not ?` |
| `show = "… WHERE username = ?2"` | `rradius.toml: sessions.show: only ?1 (the user name) can be used` |
| 沒有 `[sessions]` 或沒有那個鍵 | `sessions.show is not set in rradius.toml` |

`session` 指令只是查詢。要把在線的人數變成 Prometheus 指標，用具名查詢（[第 09 章](09-external-databases.md)）。

## 計費記錄檔

`[radius.accounting_log]` 讓 rradius 每回覆一筆計費請求，就在本機檔案附加一行 JSON。它跟計費表是兩回事：計費表是腳本寫的、給查詢用的資料；記錄檔是伺服器寫的、照收到的樣子留下的原始紀錄，適合交給日誌系統收集或留存備查。

```toml
# rradius.toml（加在檔案最後）
[radius.accounting_log]
file = "log/accounting.log"
```

| 鍵 | 預設值 | reload | 意思 |
| --- | --- | --- | --- |
| `radius.accounting_log.file` | 無，必填 | 可以 | 附加寫入的檔案，相對於設定檔的目錄 |
| `radius.accounting_log.queue` | `65536` | 要重啟 | 等著寫入的筆數上限，1 到 16777216 |

加入或移除整個 `[radius.accounting_log]` 要重啟。（完整說明見[附錄 A](90-config-reference.md)。）

檔案所在的目錄要先建好，不然 `check` 會失敗：

```console
$ rradius-server check --config rradius.toml
rradius.toml: radius.accounting_log.file: directory log does not exist
$ mkdir log
$ rradius-server check --config rradius.toml
configuration ok
```

重啟伺服器之後，送一個 `session`，記錄檔多了兩行（為了好讀，這裡把每行折成兩行）：

```text
{"time":"2026-11-22T22:03:10.581Z","client":"localhost","source":"127.0.0.1","transport":"udp","handling":"script",
 "attributes":{"User-Name":"alice","NAS-IP-Address":"192.0.2.1","NAS-Identifier":"rradius-bench","Acct-Status-Type":"Start","Acct-Session-Id":"98a0000000000000"}}
{"time":"2026-11-22T22:03:10.582Z","client":"localhost","source":"127.0.0.1","transport":"udp","handling":"script",
 "attributes":{"User-Name":"alice","NAS-IP-Address":"192.0.2.1","NAS-Identifier":"rradius-bench","Acct-Status-Type":"Stop","Acct-Session-Id":"98a0000000000000","Acct-Session-Time":"60","Acct-Input-Octets":"20000","Acct-Output-Octets":"30000"}}
```

每一行的欄位，順序固定：

| 欄位 | 內容 |
| --- | --- |
| `time` | 回覆的時間，RFC 3339、UTC、到毫秒 |
| `client` | 用戶端的名稱 |
| `source` | 來源位址 |
| `transport` | 在核心永遠是 `udp` |
| `handling` | `script`：腳本接受；`spool`：寫進了計費 spool |
| `attributes` | 請求的屬性，以字典名稱為鍵。列舉值寫成名稱（`Start`），不是數字。同一個屬性出現多次時是一個陣列 |

哪些會寫、哪些不會：

- 只寫**有回覆**的請求。腳本沒有接受、沒有回覆的請求不寫。
- 重送偵測直接回覆的重送不再寫一次。
- 只有 RADIUS 的計費。TACACS+ 的計費不寫進來。
- 伺服器建立的檔案權限是 0640。實測：`-rw-r-----`。

**寫入不會拖慢回覆。** 回覆之後，紀錄先放進佇列，由另一個執行緒批次寫入。佇列裡已經有 `queue` 筆在等時，新的那筆丟掉並計數。寫入失敗時記一次失敗，同樣的錯誤最多每分鐘警告一次，伺服器照常運作。

**日誌輪替。** 收到 `SIGHUP` 時，伺服器把佇列裡的寫完、關掉檔案，再重新開啟 `file`（不存在就建立）。所以輪替的做法是先改名，再送 `SIGHUP`：

```console
$ mv log/accounting.log log/accounting.log.1
$ kill -HUP <伺服器的 PID>
$ ls -l log
-rw-r-----  1 …   368 … accounting.log
-rw-r-----  1 …  1187 … accounting.log.1
```

`SIGHUP` 之後的紀錄寫進新建的 `accounting.log`；改名之後、`SIGHUP` 之前的紀錄仍然寫進舊檔案（已改名的那個），不會遺失。reload 時重新開檔失敗的話，記一次 `stage="open"` 的失敗並警告，繼續寫原來的檔案；新的 `file` 所在的目錄不存在時，reload 失敗，設定不變。

用 logrotate 時，在 `postrotate` 送 `SIGHUP`，不要用 `copytruncate`（會漏行）。deb、rpm 套件附的 `/etc/logrotate.d/rradius` 就是這樣寫的，它輪替的是 `/var/log/rradius/*.jsonl`，所以套件安裝時把 `file` 設在那個目錄、副檔名用 `.jsonl`，就會一起被輪替（第 02 章）。

**停止時。** 收到 SIGTERM 或 SIGINT 時，伺服器先停止收請求、等處理中的請求做完，再把佇列裡剩下的寫完，最多等 5 秒。

**指標與狀態。** 有 `[radius.accounting_log]` 時才有這些指標，啟動時都是 0：

| 指標 | 意思 |
| --- | --- |
| `rradius_accounting_log_written_total` | 寫進記錄檔的筆數 |
| `rradius_accounting_log_dropped_total` | 佇列滿了而丟掉的筆數 |
| `rradius_accounting_log_failures_total{stage="open"}` | 開檔失敗的次數 |
| `rradius_accounting_log_failures_total{stage="write"}` | 寫入失敗的次數 |

`status` 在計費 spool 那段後面多一段：

```console
$ rradius-server status --config rradius.toml
…
Accounting log
  Written        0.0/s (5)
  Dropped        0.0/s (0)
  Failed opens   0.0/s (0)
  Failed writes  0.0/s (0)
…
```

`--json` 時這一段的鍵是 `acct_log`。`Dropped` 或 `Failed` 一直增加，就是記錄檔漏了紀錄：檢查磁碟空間與檔案權限。

> **注意：** 記錄檔裡有 `User-Name` 等個人資料，留存期限與存取權限要照你的規範處理。

## 計費 spool

叢集資料庫要等多數節點確認才能寫入（第 16 章）。叢集失去多數節點、或資料庫一時太忙而逾時的那段時間，計費腳本寫不進去，NAS 就收不到回覆。NAS 重送幾次之後會放棄，那些紀錄就沒了。

計費 spool 是寫在本機磁碟上、等待重送（replay）的暫存。設了 `[radius.accounting_spool]` 之後，腳本執行時資料庫呼叫因為**連不上或逾時**而失敗（不管腳本有沒有捕捉這個錯誤、最後回傳什麼），rradius 先把整個請求寫進磁碟，確定寫好了才回 Accounting-Response。之後在背景依序重送：把請求再交給腳本跑一次，直到腳本接受為止。

```toml
# rradius.toml（加在檔案最後）
[radius.accounting_spool]
dir = "spool"
```

| 鍵 | 預設值 | reload | 意思 |
| --- | --- | --- | --- |
| `radius.accounting_spool.dir` | 無，必填 | 要重啟 | 暫存目錄，相對於設定檔的目錄 |
| `radius.accounting_spool.max_bytes` | `1073741824`（1 GiB） | 要重啟 | 暫存的大小上限，位元組，要大於 0 |
| `radius.accounting_spool.replay_interval_ms` | `5000` | 要重啟 | 重送失敗後，等多久再試，毫秒，要大於 0 |
| `radius.accounting_spool.replay_window` | `1` | 要重啟 | 同時重送幾筆，1 到 1024 |

整個表改了都要重啟，reload 時日誌會寫 `these need a restart to apply: [radius.accounting_spool]`。（完整說明見[附錄 A](90-config-reference.md)。）

**哪些會寫進 spool、哪些不會：**

- 會：資料庫呼叫逾時、沒有 leader、寫入佇列拒收等「資料庫現在不能用」的失敗。
- 不會：SQL 寫錯（語法錯誤、表不存在、參數數目不對）、腳本本身的 Lua 錯誤、腳本沒有呼叫資料庫就回傳 `reject`。這些寫進去也補不好，所以照樣不回覆。
- spool 滿了（再寫就超過 `max_bytes`）或磁碟寫入失敗時，不回覆，記一次 `rradius_acct_spool_full_total`，並寫日誌。之後的請求照常嘗試寫入。

**重送（replay）的規則：**

- 從最舊的開始，每次最多拿 `replay_window` 筆，同時交給**目前的**腳本，`phase` 是 `accounting`，`replayed` 是 true。
- `Acct-Delay-Time` 會加上這筆待在 spool 的秒數；請求原本沒有這個屬性時，會加上一個。
- 腳本接受、而且過程中沒有資料庫失敗：重送成功，計入 `rradius_acct_spool_replayed_total`。
- 資料庫仍然不能用、Lua 錯誤、超過執行時間限制，或腳本捕捉了資料庫錯誤卻仍回傳 `accept`：等 `replay_interval_ms` 之後再試同一筆，後面的都等著。
- 下面幾種「再試也不會成功」的，搬到 `rejected.log`，計入 `rradius_acct_spool_rejected_total`，接著重送下一筆：用戶端已經不在設定裡、存下來的不是 RADIUS 封包、腳本在沒有資料庫失敗的情況下不接受、腳本因為 SQL 或參數錯誤（包括違反唯一或外鍵限制）而結束。
- 一批之中，結果照寫入的順序結算，停在第一筆要重試的；它後面的即使腳本接受了，也留在 spool 裡、之後再跑一次。

預設的 `replay_window = 1` 嚴格依照收到的順序一筆一筆重送，因為腳本可能預期同一個 session 的 Start 先於 Stop。調大可以補得比較快，但順序可能交錯，而且一批中途停下時，後面的會多跑一次。腳本照[看懂計費腳本](#看懂計費腳本)那樣寫成能重複執行，順序與重複都不會出問題。

重送是「至少一次」：伺服器在重送與記錄進度之間被中止，重啟後會把那幾筆再跑一次。進度至少每 64 筆、以及每換一個檔案時寫到磁碟。重啟之後從記錄的進度繼續重送。

**目錄裡的檔案：**

| 檔案 | 內容 |
| --- | --- |
| `segment-<20 位數字>.log` | 等待重送的請求，依序存放，一個檔案到 16 MiB 換下一個 |
| `position` | 重送到哪裡 |
| `rejected.log` | 被搬出來、不再重送的請求 |
| `truncated.log` | 啟動時發現最後一筆沒寫完，切掉之前先存在這裡 |
| `*.corrupt` | 中間有損壞的檔案，原封不動改名留下，重送跳到下一個檔案 |

伺服器建立的目錄權限是 0700，檔案是 0600。這些檔案是二進位格式，裡面有 `User-Name` 等個人資料，磁碟遺失就跟著遺失。`check` 會檢查 `dir` 能不能用，但不會建立它，也不會改動裡面的檔案。

**實測：叢集失去多數節點時。** 在本機起三個節點（ha-cluster 情境的三份設定，各自用不同的埠、`data_dir` 與 `dir`，設定方式見第 16 章），先送一筆計費到節點 1 確認正常，再停掉另外兩個節點。下面的指令省略了節點 1 的埠：

- 停掉之後的第一筆，腳本的寫入等了約 30 秒才失敗，然後寫進 spool 並回覆；NAS 的逾時比這短，所以 NAS 那邊會先重送，處理中的重送會被丟掉，等寫進 spool 之後的重送由重送偵測回覆。
- 這個節點沒有 leader 超過 5 秒之後，寫入立刻失敗，新的計費幾毫秒內就寫進 spool 並回覆：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat nas.secret)" \
    --user dave --load acct-start,concurrency=1 --requests 1 --timeout 10
…
  acct-start: 166/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-server status --config node1.toml
…
Accounting spool
  Bytes waiting  223
  Replayed       0.0/s (0)
  Rejected       0.0/s (0)
  Not spooled    0.0/s (0)
…
```

日誌同時有腳本的錯誤 `Lua script error (radius/accounting): runtime error: no leader for 48 s`，那是預期的：腳本確實寫不進去，只是請求已經安全地存在磁碟上。

把另外兩個節點啟動之後，十幾秒內重送完成：`Bytes waiting` 回到 0，`rradius_acct_spool_replayed_total` 是 2，計費表裡有這兩筆，`Acct-Delay-Time` 比原本多了在 spool 裡等的秒數。

把腳本改成重送時一律 `reject` 再試一次，那筆被搬到 `rejected.log`：

```text
… WARN rradius_core::lua_engine: Lua script did not accept accounting (radius, client nas-network)
… ERROR rradius_core::lua_engine: accounting spool: request from client nas-network moved to rejected.log: the script did not accept it
```

> **注意：** `Acct-Delay-Time` 增加的秒數，是從請求**寫進 spool** 的時候算起，不是從收到請求算起。像上面停掉節點後的第一筆，腳本等了約 30 秒才失敗，這 30 秒不會算進去，所以那一筆的時間會晚約 30 秒。

> **未實機驗證：** spool 滿了、磁碟寫入失敗、損壞檔案的處理（`truncated.log`、`*.corrupt`）只對照了程式碼與測試，沒有實測。

**指標與狀態。** 計費 spool 的指標不管有沒有設 `[radius.accounting_spool]` 都會出現：

| 指標 | 意思 |
| --- | --- |
| `rradius_acct_spool_written_total` | 寫進 spool 的請求數 |
| `rradius_acct_spool_replayed_total` | 重送成功（腳本接受）的請求數 |
| `rradius_acct_spool_rejected_total` | 搬到 `rejected.log` 的請求數 |
| `rradius_acct_spool_full_total` | spool 滿了或磁碟失敗而沒寫進去的請求數 |
| `rradius_acct_spool_corrupt_total` | 被改名留下的損壞檔案數 |
| `rradius_acct_spool_bytes` | 等待重送的位元組數 |

`status` 的 Accounting spool 那段：`Bytes waiting` 是 `rradius_acct_spool_bytes`，`Not spooled` 是 `rradius_acct_spool_full_total`。值得告警的是：`rradius_acct_spool_bytes` 長時間不是 0（補不進去）、`full_total` 或 `rejected_total` 增加。告警規則的範例見[第 17 章](17-monitoring.md)。

> **警告：** spool 在每個節點各自的磁碟上，不會複寫到其他節點。一個節點的 spool 裡還有資料時，不要刪掉那個節點的 `dir`，也不要把節點移出叢集後就丟掉它的磁碟：等 `Bytes waiting` 回到 0 再處理。

## 腳本怎麼知道是重送（replay）：replayed

從 spool 重送時，全域變數 `replayed` 是 true；平常是 false。多數腳本不需要分辨，照能重複執行的方式寫就好。需要分辨的是「有副作用、而且只該在當下做一次」的事，例如：

- 呼叫外部的 HTTP API 通知某個系統。
- 根據 Interim-Update 判斷要不要讓使用者斷線。重送的 Interim-Update 是幾分鐘前的舊消息。

```lua
-- accounting.lua（節錄）
if replayed then
  log('replayed', get_attr('Acct-Session-Id'), 'delay', get_attr('Acct-Delay-Time'))
end
```

實測重送時，這一行印出：

```text
… INFO rradius_core::lua_engine: script accounting.lua (radius/accounting, client nas-network): replayed 57b0000000000000 delay 14
```

## TACACS+ 的計費

TACACS+ 的計費請求也交給最上層腳本，`phase` 是 `accounting`，`protocol` 是 `"tacacs"`，但跟 RADIUS 有三點不同：

- 腳本沒有接受時，回覆 `ERROR`，而不是不回覆。
- 沒有計費 spool。
- 不寫進 `[radius.accounting_log]`。

屬性、表的設計與範例見[第 12 章](12-tacacs.md)。

## 常見錯誤

**症狀：** NAS 一直重送計費請求，`rradius-bench` 是 `timeouts 1`，伺服器日誌有 `Lua script did not accept accounting` 或 `Lua script error (radius/accounting)`。
**原因：** 計費腳本沒有回傳 `accept`。常見的是腳本開頭的 `if phase ~= 'accounting'` 或 `protocol` 判斷寫反了，或 SQL 寫錯。
**解法：** 照日誌修正腳本，送 `SIGHUP`。

**症狀：** `rradius-bench` 是 `timeouts 1`，伺服器日誌什麼都沒有。
**原因：** 請求在腳本之前就被丟掉了，最常見的是共用密碼不一致（`status` 的 `Discarded: invalid` 增加），或來源不是設定裡的用戶端（`Discarded: unknown client`）。
**解法：** 見[第 05 章](05-radius-clients.md)。

**症狀：** 同一個 session 在計費表裡有兩列，或流量被算了兩次。
**原因：** 腳本不能重複執行：用了單純的 `INSERT`，或把流量用加的。
**解法：** 照[看懂計費腳本](#看懂計費腳本)改成 `INSERT … ON CONFLICT … DO UPDATE` 與 `max()`。

**症狀：** `session list` 一直列出早就下線的人。
**原因：** NAS 沒送 Stop（斷電、斷線），而 `[sessions]` 的 SQL 只看 `stopped IS NULL`。
**解法：** 加上「最近有更新」的條件，例如 quick-start 的 `updated > strftime('%s', 'now') - 900`，並讓 NAS 送 Interim-Update。

**症狀：** `session list` 印出 `sessions.list is not set in rradius.toml`，結束碼 2。
**原因：** 設定檔沒有 `[sessions]`，或沒有 `list`。
**解法：** 加上 `[sessions]`。改了不必 reload。

**症狀：** `session list` 等了 30 秒，印出 `the cluster did not answer within 30 s`，結束碼 3。
**原因：** 伺服器沒在跑，或連不到叢集的 API 埠。
**解法：** 先啟動伺服器。

**症狀：** `check` 印出 `radius.accounting_log.file: directory log does not exist`。
**原因：** 記錄檔所在的目錄不存在。
**解法：** 先建立目錄，並讓執行伺服器的帳號能寫入。

**症狀：** `check` 印出 `radius.accounting_log.queue must be from 1 to 16777216`。
**原因：** `queue` 超出範圍，例如 0。
**解法：** 改成範圍內的值，或刪掉用預設值。

**症狀：** `check` 印出 `radius.accounting_spool.dir: cannot use 'afile': Not a directory (os error 20)`。
**原因：** `dir` 指到一個檔案，或指到不能寫入、不能列出的目錄。
**解法：** 改成可以寫入的目錄。目錄不存在、但上一層可以寫入時，`check` 會通過，伺服器啟動時才建立它。

**症狀：** `check` 印出 `radius.accounting_spool.replay_window must be from 1 to 1024`、`radius.accounting_spool.max_bytes must be greater than 0` 或 `radius.accounting_spool.replay_interval_ms must be greater than 0`。
**原因：** 值超出範圍。
**解法：** 改成範圍內的值。

**症狀：** `check` 印出 ``conf.d/snippet.toml: line 1: missing field `dir` ``，開頭是 `[radius.accounting_spool]` 所在的檔案與行號。
**原因：** `[radius.accounting_spool]` 沒有 `dir`。
**解法：** 加上 `dir`。

**症狀：** 叢集恢復了，`rradius_acct_spool_bytes` 卻一直不是 0，日誌每 5 秒出現一次同樣的腳本錯誤。
**原因：** 重送的那一筆每次都因為「可能下次會好」的原因失敗，例如腳本的 Lua 錯誤或執行逾時。後面的都在等它。
**解法：** 照日誌修正腳本並 reload，重送會從那一筆繼續。

**症狀：** `rradius_acct_spool_rejected_total` 增加，日誌有 `moved to rejected.log`。
**原因：** 重送時腳本拒絕，或 SQL 錯誤、用戶端已經從設定中移除。
**解法：** 照日誌的原因修正。`rejected.log` 裡的請求不會再自動重送。

## 相關章節

- [03 第一次啟動與登入](03-quick-start.md)
- [05 RADIUS 服務與 NAS 用戶端](05-radius-clients.md)：用戶端、重送偵測、過載保護
- [06 字典與屬性](06-dictionary.md)：屬性名稱與列舉值
- [09 外部資料庫與具名查詢](09-external-databases.md)：把計費資料變成指標
- [10 用 Lua 寫政策](10-lua-policies.md)：`sql_execute`、`log`、migrations
- [12 TACACS+ 設備管理](12-tacacs.md)：TACACS+ 的計費
- [14 情境實作：寬頻與 ISP](14-isp-recipes.md)：位址池與流量控制的計費
- [16 叢集與高可用](16-cluster.md)：寫入為什麼要等 leader 確認
- [17 監控、狀態與日誌](17-monitoring.md)
- [附錄 A：設定參考](90-config-reference.md)
- [附錄 C：指標參考](92-metrics-reference.md)
