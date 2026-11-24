# 10 用 Lua 寫政策

> 本章讓你做到：
> - 知道腳本什麼時候執行、回傳值代表什麼
> - 讀取請求的屬性、在回覆裡加屬性
> - 用 `sql_query`、`sql_execute` 讀寫叢集資料庫，而且寫入在每個節點都一樣
> - 寫出可以重送的計費腳本
> - 從腳本呼叫白名單上的 HTTP API，並守住執行時間、記憶體與資料庫等待的限制

前置條件：照[第 03 章](03-quick-start.md)跑過 quick-start；讀過[第 07 章](07-users-and-chains.md)，知道驗證鏈與處理器。本章只寫用法，每個函式完整的參數、回傳值與錯誤訊息在[附錄 D](93-lua-reference.md)。

本章小節：

- [腳本在哪裡執行](#腳本在哪裡執行)
- [準備練習環境](#準備練習環境)
- [第一個腳本](#第一個腳本)
- [回傳值](#回傳值)
- [讀取請求的屬性](#讀取請求的屬性)
- [加回覆屬性](#加回覆屬性)
- [讀寫叢集資料庫](#讀寫叢集資料庫)
- [用 migration 建立資料表](#用-migration-建立資料表)
- [在驗證鏈裡當 lua 處理器](#在驗證鏈裡當-lua-處理器)
- [兩步登入：回 Access-Challenge](#兩步登入回-access-challenge)
- [寫計費腳本](#寫計費腳本)
- [呼叫 HTTP API](#呼叫-http-api)
- [其他工具函式](#其他工具函式)
- [EAP 與憑證的變數](#eap-與憑證的變數)
- [執行時間、記憶體與資料庫等待的限制](#執行時間記憶體與資料庫等待的限制)
- [改了腳本之後](#改了腳本之後)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

## 腳本在哪裡執行

rradius 有兩種腳本：

| 種類 | 設定 | 什麼時候執行 |
| --- | --- | --- |
| 最上層腳本 | `[lua]` 的 `script` | 每個計費請求（RADIUS 與 TACACS+）；沒有驗證鏈的登入；TACACS+ 授權（用戶端沒有 `authorize_group` 時） |
| `lua` 處理器 | `[[auth.handlers]]` 的 `type = "lua"` | 驗證鏈輪到它的時候，只在登入時執行 |

所以只要設定裡有 `[auth]`，登入就交給驗證鏈，最上層腳本只會看到計費與 TACACS+ 授權。quick-start 就是這樣：`accounting.lua` 一開頭就寫了「不是計費就拒絕」。

> **注意：** 驗證鏈是「每個用戶端選一條」。`[auth]` 底下沒有預設鏈（沒有 `[[auth.handlers]]`），而用戶端也沒有用 `auth` 選一條具名的鏈時，這個用戶端的登入一樣回到最上層腳本。

每個請求執行腳本時，腳本看到的是一個為這個請求新建的全域環境：

- 腳本自己設的全域變數，下一個請求看不到。要記住東西，就寫進資料庫。
- Lua 內建的 `string`、`table`、`math` 等函式庫可以用，但是唯讀的，改它會出錯。
- 沒有 `io`、`require`、`loadstring`，`os` 也只剩時間相關的函式。腳本不能讀寫檔案、不能執行程式。
- rradius 另外放進來的變數與函式，例如 `get_attr`、`sql_query`、`phase`，完整清單在附錄 D。

腳本用的是 Luau（Lua 5.1 的延伸），伺服器啟動時編譯一次，之後每個請求直接執行。

## 準備練習環境

本章用一份從 quick-start 改出來的設定，把登入交給最上層腳本，比較容易看出腳本做了什麼。

```console
$ cp -R ~/rradius-lab/quick-start ~/rradius-lab/lua
$ cd ~/rradius-lab/lua
$ rm -rf data
```

用編輯器打開 `rradius.toml`，做兩件事：

1. 刪掉 `[auth]` 這一節和它底下的 `[[auth.handlers]]`，也就是從 `[auth]` 那一行刪到 `[radius]` 的前一行。
2. 把 `[lua]` 改成指向新的腳本：

```toml
# rradius.toml（只列出改過的部分）
[lua]
script = "policy.lua"
# …（其他設定略）
```

`[lua]` 各鍵的預設值與能否 reload，見[附錄 A](90-config-reference.md)：

| 鍵 | 預設值 | reload | 意思 |
| --- | --- | --- | --- |
| `lua.script` | 必填 | 可以 | 最上層腳本，相對於設定檔 |
| `lua.max_execution_ms` | `1000` | 可以 | 腳本在一個請求裡可以執行的時間（毫秒） |
| `lua.query_timeout_ms` | `5000` | 可以 | 腳本在一個請求裡等資料庫的總時間（毫秒） |
| `lua.max_memory_mb` | `256` | 要重啟 | 每個 Lua VM 的記憶體上限（MiB） |

## 第一個腳本

目標：用腳本做 sql 處理器做的事，查 `users` 表比對密碼，接受時帶兩個回覆屬性。

```lua
-- policy.lua
-- 計費：練習環境不記錄，直接回應，NAS 才不會一直重送。
if phase == 'accounting' then
  return 'accept'
end
-- 這份腳本只處理 RADIUS 登入。
if protocol ~= 'radius' then
  return 'reject'
end

local user = get_attr('User-Name')
if not user then
  return 'reject'
end
local rows = sql_query('SELECT password FROM users WHERE username = ? AND enabled = 1', {user})
local account = rows[1]
if not account or not password_verify(account.password, get_attr('User-Password')) then
  log('refused', user)
  return 'reject'
end

set_attr('Filter-Id', 'staff')
set_attr('Session-Timeout', 3600)
return 'accept'
```

存成 `policy.lua`，檢查設定，再啟動伺服器（同第 03 章）：

```console
$ rradiusd check --config rradius.toml
configuration ok
$ rradiusd serve --config rradius.toml
```

`check` 會編譯腳本，語法錯誤在這裡就會看到。在另一個終端機登入：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user bob --password bob-password --load pap,concurrency=1 --requests 1
…
  pap: 24/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user bob --password wrong --load pap,concurrency=1 --requests 1
…
  pap: 25/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

伺服器的日誌：

```text
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="bob" account="bob" chain="script" handler="script" decision="accept"
… INFO rradius_core::lua_engine: script policy.lua (radius/authentication, client localhost): refused bob
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="bob" account="bob" chain="script" handler="script" decision="reject"
```

- `chain="script" handler="script"`：這次是最上層腳本做的決定，不是驗證鏈。
- `log()` 寫的那一行前面會標出腳本、協定與階段（phase）、用戶端，方便你知道是哪個請求寫的。
- `bob` 的密碼存的是 bcrypt，`password_verify` 跟 sql 處理器用同一套比對方式，所以兩種存法都認得。

這段過程用到的東西：

| 名稱 | 種類 | 這裡的用途 |
| --- | --- | --- |
| `phase` | 變數 | `"authentication"`、`"authorization"` 或 `"accounting"` |
| `protocol` | 變數 | `"radius"` 或 `"tacacs"` |
| `client` | 變數 | 比對到的用戶端名稱；沒寫 `name` 時是它的 `address` |
| `get_attr(name)` | 函式 | 讀請求裡的一個屬性 |
| `sql_query(sql, params)` | 函式 | 查叢集資料庫 |
| `password_verify(stored, given)` | 函式 | 比對存放的密碼 |
| `set_attr(name, value)` | 函式 | 在回覆裡加一個屬性 |
| `log(...)` | 函式 | 寫一行日誌 |

## 回傳值

腳本最後 `return` 的字串決定結果：

| 回傳 | 最上層腳本 | `lua` 處理器 |
| --- | --- | --- |
| `'accept'` | 接受：Access-Accept、TACACS+ PASS、Accounting-Response | 接受（`first_match` 就到此為止） |
| `'reject'` | 拒絕 | 拒絕，整條鏈到此為止 |
| `'ignore'` | 當成拒絕 | 不表示意見，交給下一個處理器 |
| `'challenge'` | 只在 RADIUS 登入有效：Access-Challenge | 同左 |
| 其他值、什麼都不回、出錯 | 拒絕 | 拒絕 |

- 只有完全相同的小寫字串才算。實測回傳 `'ACCEPT'` 是拒絕。
- 計費請求的「拒絕」是不回應：NAS 收不到 Accounting-Response，會重送。日誌寫 `Lua script did not accept accounting (radius, client localhost)`。
- 腳本出錯時，日誌寫一行 ERROR，例如 `Lua script error (radius/authentication): runtime error: policy.lua:12: …`，冒號後面是腳本檔名與行號。
- 有一個例外：資料庫連不上或逾時造成的失敗，登入不會收到拒絕，而是不回應，見[資料庫連不上時](#資料庫連不上時)。

## 讀取請求的屬性

`get_attr(name)` 回傳請求裡第一個叫這個名字的屬性；沒有就是 `nil`。名稱就是字典裡的名稱（第 06 章），大小寫要一致。

值的型別依字典裡的型別而定：

| 字典型別 | Lua 拿到的 | 例子 |
| --- | --- | --- |
| `string`、`octets` | 字串（原始的位元組） | `get_attr('User-Name')` → `"alice"` |
| `integer`、`date`、`byte`、`short`、`signed`、`integer64` | 數字 | `get_attr('NAS-Port')` → `7` |
| `ipaddr`、`ipv6addr`、`ipv6prefix` 等位址 | 文字形式的字串 | `get_attr('NAS-IP-Address')` → `"192.0.2.1"` |

列舉的值拿到的是數字，不是字典裡的名稱。例如 `Acct-Status-Type` 的 Start 是 `1`，所以要寫 `get_attr('Acct-Status-Type') == 1`，不是 `== 'Start'`。

`User-Password` 拿到的是解密後的明文，只有 PAP 才有。CHAP、MS-CHAP 的請求沒有明文密碼，腳本無法比對，要交給驗證鏈的 sql 或 ldap 處理器。

一個屬性可能出現好幾次，例如 `Class`。`get_attrs(name)` 回傳全部的值，依封包裡的順序：

```lua
-- policy.lua（片段）
local classes = get_attrs('Class')   -- 沒有時是空表 {}
for i, value in ipairs(classes) do
  log('Class', i, value)
end
```

實測：請求帶兩個 `Class`（`a`、`b`）時，`get_attrs('Class')` 是 `{"a", "b"}`，`get_attr('Class')` 是 `"a"`。

TACACS+ 請求沒有字典，屬性名稱是封包的欄位與 AV pair 的名稱，例如 `user`、`rem_addr`、`service`、`cmd`。清單在附錄 D，用法在[第 12 章](12-tacacs.md)。

> **警告：** 屬性的值是 NAS 或使用者送來的，不要直接拼進 SQL 或網址。SQL 一律用參數（下一節），網址用 `url_encode`。

## 加回覆屬性

`set_attr(name, value)` 在回覆加一個屬性，依呼叫的順序排列；同一個名稱呼叫兩次就有兩個。值可以是字串或數字：

```lua
-- policy.lua（片段）
set_attr('Filter-Id', 'staff')
set_attr('Service-Type', 'Framed-User')   -- 列舉可以寫字典裡的名稱
set_attr('Session-Timeout', 3600)
```

實測的回覆（用 `[log.trace]` 記下，第 17 章）：`Filter-Id = staff, Service-Type = 2, Session-Timeout = 3600`。

- 名稱與值在呼叫的當下就檢查。字典裡沒有的名稱、列舉裡沒有的值、型別不對的值，`set_attr` 直接出錯，請求被拒絕。例如 `set_attr('Service-Type', 'Nonsense')` 得到 `'Nonsense' is not a value of 'Service-Type'`。
- 帶 tag 的屬性寫成 `名稱:tag`，例如 `Tunnel-Type:1`。
- 只有「接受」與「challenge」會送出這些屬性。拒絕時不送。
- 計費請求的 Accounting-Response 一律不帶屬性，腳本設了也不會送出。
- TACACS+ 授權時，`set_attr` 加的是回覆的 AV pair，例如 `set_attr('priv-lvl', 15)`；名稱 `server_msg` 是特例，它變成給使用者看的訊息，不是 AV pair。

## 讀寫叢集資料庫

腳本用兩個函式使用叢集資料庫（內建的 hiqlite，也就是每個節點一份、用 Raft 複寫的 SQLite）：

| 函式 | 做什麼 | 回傳 |
| --- | --- | --- |
| `sql_query(sql, params)` | 讀本機這份資料庫 | 一列一個表的陣列，欄位名稱當鍵 |
| `sql_execute(sql, params)` | 寫入：送到叢集的 leader，複寫到每個節點之後才回來 | 影響的列數 |

### 用參數，不要拼字串

SQL 裡的值一律寫 `?`，值放在第二個參數的陣列裡，依順序對上：

```lua
-- policy.lua（片段）
local rows = sql_query('SELECT password FROM users WHERE username = ? AND enabled = 1', {user})
```

- 參數永遠不會被當成 SQL。實測查 `x' OR '1'='1` 得到空陣列。
- `?` 的數量要和值的數量一樣，不一樣時在送出前就出錯：`sql_query: 1 placeholder, 0 values`。
- 只能寫 `?`。`?1`、`:name` 這類寫法會出錯：`only plain ? placeholders are supported, found ?1`。
- 值可以是字串、數字或布林（`true` 存成 1）。`nil` 不行：陣列中間有 `nil` 會得到 `parameters must be an array without holes`。要存 NULL，直接在 SQL 裡寫 `NULL`。

查詢結果：

```lua
local rows = sql_query('SELECT username, enabled FROM users WHERE username = ?', {'alice'})
-- #rows 是 1；rows[1].username 是 "alice"；rows[1].enabled 是 1
```

- 查不到時是空陣列，`rows[1]` 是 `nil`。
- INTEGER、REAL 變成數字，TEXT 變成字串，NULL 變成 `nil`（所以那個鍵不存在）。
- `sql_query` 只能讀。用它寫入會出錯：`attempt to write a readonly database`。

### 寫入：在每個節點都要算出同樣的結果

`sql_execute` 的 SQL 會原封不動地在每個節點執行一次。所以寫入裡不能有每個節點算出來不一樣的東西，例如現在的時間或亂數。rradius 在送出前就擋下這些：

- 函式：`date`、`datetime`、`julianday`、`now`、`random`、`randomblob`、`strftime`、`time`、`timediff`、`unixepoch`
- 關鍵字：`CURRENT_TIMESTAMP`、`CURRENT_DATE`、`CURRENT_TIME`

時間改從 Lua 算好，當參數傳進去：

```lua
-- policy.lua（片段）
sql_execute('INSERT INTO last_login (username, at, client) VALUES (?, ?, ?) ' ..
  'ON CONFLICT (username) DO UPDATE SET at = excluded.at, client = excluded.client',
  {user, math.floor(now_unix()), client})
```

寫成 `strftime('%s', 'now')` 時，實測得到 `` sql_execute: non-deterministic `strftime` is not allowed in writes; pass time values from now_unix() instead ``。寫在字串值裡、註解裡，或欄位名稱剛好有這些字（例如 `update_date`）都沒關係。

`sql_query` 只在本機讀，不受這個限制。`[sessions]` 的查詢用 `strftime` 就是這個原因。

> **注意：** `sql_query` 讀的是本機這一份。叢集有好幾個節點時，剛在另一個節點寫入的資料，這裡可能要稍後才讀得到。需要「讀到自己剛寫的」時，在同一個請求裡用 `sql_execute` 的回傳值判斷，例如 `DELETE … WHERE …` 回傳 1 才算搶到。

### 資料庫連不上時

寫入要等 leader 確認。叢集沒有 leader 超過 5 秒後，新的寫入立刻出錯；失去多數後的第一次寫入已交給叢集，要等 30 秒才失敗（見[第 16 章](16-cluster.md)）。一個請求等資料庫的總時間超過 `lua.query_timeout_ms` 時，呼叫也出錯，例如 `sql_query timed out: the database calls of one request have 5000 ms together`。

這類失敗表示「現在問不到」，不表示這個人不能登入，所以伺服器的處理不同於一般的錯誤：

- 登入：不回應，讓 NAS 重送或改問別台伺服器。日誌寫 `RADIUS Access-Request from client localhost not answered: the authentication backend could not be used`，`status` 的 `Discarded: database` 加一。即使腳本用 `pcall` 接住錯誤再回傳 `'reject'`，結果也一樣。
- 計費：有計費 spool 時先寫進 spool，晚點重送（replay，第 13 章）；沒有時不回應。

實測：`query_timeout_ms = 200` 時，一段要跑很久的查詢讓 `rradius-bench` 得到 `timeouts 1`，`Discarded: database` 從 0 變 1。

一個請求裡的所有資料庫呼叫（包括第 09 章的 `db_query`、`db_execute`）共用同一個 `query_timeout_ms`：第二個呼叫只能用第一個剩下的時間。外部資料庫的用法在第 09 章，每個函式的參數與錯誤在附錄 D。

## 用 migration 建立資料表

腳本用到的資料表，用 `[cluster]` 的 `migrations_dir` 建立。伺服器啟動時、開始服務之前，依編號套用目錄裡的 SQL 檔，每個檔只套用一次。

規則：

- 檔名是 `<編號>_<名稱>.sql`，從 `1` 開始連號，例如 `1_schema.sql`、`2_demo_users.sql`。
- 每個檔是一個 transaction，透過叢集寫入，所以每個節點的資料表都一樣。
- 套用過的檔會記下名稱與內容的 SHA-256。之後改了已套用的檔，或編號有缺號，伺服器啟動失敗，結束碼 2。
- 要改資料表，就加一個新檔，例如 `3_last_login.sql`，不要改舊檔。

練習環境加一個表給上一節的範例用：

```sql
-- migrations/3_last_login.sql
CREATE TABLE last_login (
  username TEXT PRIMARY KEY,
  at INTEGER NOT NULL,
  client TEXT
);
```

migration 只在啟動時套用，加了新檔要重新啟動伺服器。叢集有好幾個節點時的行為（等 leader、追上進度）在[第 16 章](16-cluster.md)。

## 在驗證鏈裡當 lua 處理器

驗證鏈裡的 `lua` 處理器適合做「密碼以外的規則」：密碼交給 sql 或 ldap 處理器，腳本只決定這個帳號現在能不能上線。

回到 quick-start 的設定（有 `[auth]` 的那份），在 sql 處理器前面加一個 `lua` 處理器：

```toml
# rradius.toml（取代原本的 [auth] 那一段）
[auth]
policy = "first_match"

[auth.username]
lowercase = true

[[auth.handlers]]
type = "lua"
script = "policy.lua"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ? AND enabled = 1"
reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"
```

```lua
-- policy.lua（lua 處理器）
-- auth_user 是驗證鏈整理過的帳號名稱；User-Name 是請求原本帶的。
log('policy', get_attr('User-Name'), auth_user)
if auth_user == 'carol' then
  return 'reject'      -- 擋下：整條鏈到此為止
end
return 'ignore'        -- 不表示意見：交給下一個處理器（sql）比對密碼
```

實測結果：

| 登入 | 腳本看到的 `User-Name`、`auth_user` | 結果 | 認證紀錄的 `handler` |
| --- | --- | --- | --- |
| `Alice` 正確密碼 | `Alice`、`alice` | 接受 | `sql` |
| `CAROL` | `CAROL`、`carol` | 拒絕 | `lua` |
| `nobody` | `nobody`、`nobody` | 拒絕 | `none` |

- `auth_user` 只有在驗證鏈裡才有值。它是 `[auth.username]` 整理過的名稱，查資料表時用它。
- `lua` 處理器只在登入時執行。計費一律交給最上層腳本。

> **警告：** `first_match` 的鏈裡，`lua` 處理器回傳 `'accept'` 就直接接受，後面的處理器不再比對密碼。實測：腳本對 `guest` 回傳 `'accept'`，任何密碼都能登入。只做規則的腳本，該放行時回傳 `'ignore'`；要兩關都過，改用 `policy = "all_accept"`（第 07 章）。

腳本真的自己比對了密碼（例如用 `password_verify` 或呼叫 API）時，在處理器加上 `verifies_password = true`，驗證鏈才把它的接受當成「密碼已比對」。PEAP、TTLS 的內層登入需要這一點，細節在第 07 章與[第 11 章](11-eap.md)。

## 兩步登入：回 Access-Challenge

腳本回傳 `'challenge'` 時，伺服器送 Access-Challenge，NAS 會再問使用者一次（例如一次性密碼），第二個 Access-Request 帶回同一個 `State`。

```lua
-- policy.lua（片段）
set_attr('State', random_token())          -- 32 個十六進位字元
set_attr('Reply-Message', 'Enter your code')
return 'challenge'
```

- 只有 RADIUS 的登入能回 challenge。TACACS+、計費、授權回傳 `'challenge'` 都當成拒絕。
- Access-Challenge 只能帶 `Reply-Message`、`State`、`Idle-Timeout`、`Session-Timeout` 與廠商屬性，`State` 最多一個。帶了別的屬性，伺服器改送拒絕，日誌寫 `RADIUS Access-Challenge for client localhost not sent: 'Filter-Id' is not allowed in an Access-Challenge`。
- 腳本要自己記住 challenge 給了誰：把 `State` 寫進資料表，第二次請求再查。

完整的做法（密碼、TOTP、錯誤次數上限、一個代碼只能用一次）在範例 `rradius-server/examples/two-step-totp.lua`，[第 15 章](15-access-recipes.md)逐段說明。

## 寫計費腳本

計費請求一律交給最上層腳本。quick-start 的 `accounting.lua` 是一個完整的例子，它示範了寫計費腳本的幾個要點：

1. **回傳 `'accept'` 才會回應。** 沒有回應，NAS 會重送同一個封包。
2. **每個寫入都要能重複執行。** NAS 重送時腳本會再跑一次，所以用 `INSERT … ON CONFLICT … DO UPDATE`，同一個封包寫兩次和寫一次結果相同。
3. **無法記錄、重送也不會比較好時，還是回傳 `'accept'`**，例如沒有 `Acct-Session-Id` 的封包。先用 `log()` 留下一行。
4. **時間用 `now_unix()` 減掉 `Acct-Delay-Time`**，就是事件發生的時間。從計費 spool 重送的請求，伺服器會把它在 spool 裡等的時間加進 `Acct-Delay-Time`。
5. **`replayed` 是 `true` 時，這個請求是從計費 spool 重送的**，可能是很久以前的事。不要拿它做即時的動作。

```lua
-- accounting.lua（節錄 quick-start）
local status = get_attr('Acct-Status-Type')
if status == 7 or status == 8 then    -- Accounting-On／Off：這台 NAS 重開機了
  sql_execute("UPDATE accounting SET stopped = updated, terminate_cause = 11 WHERE client = ? AND nas = ? AND stopped IS NULL",
    {client, nas})
  return 'accept'
end
```

計費腳本同時執行的數量有上限：Lua VM 數減 2，至少 1（VM 數等於程式能用的 CPU 數）。超過的請求排隊等，不會被拒絕；腳本在等資料庫或 HTTP 回應時會讓出位置。這是為了讓大量計費湧入時，登入還有 CPU 可用。登入與授權的腳本沒有這個上限。

計費的資料表怎麼設計、spool、`session` 指令，都在[第 13 章](13-accounting.md)。

## 呼叫 HTTP API

帳號在別的系統（會員系統、帳務系統）時，腳本可以呼叫它的 HTTP API。這要先在設定裡列出允許的主機：

```toml
# rradius.toml（加在 [lua] 後面）
[lua.http]
allowed_hosts = ["members.example.com"]   # 只寫主機：只允許 HTTPS，埠 443
timeout_ms = 1500
max_concurrent = 64
```

沒有 `[lua.http]` 時，腳本裡根本沒有 `http_request` 與 `url_encode`，是 `nil`。各鍵的預設值見[附錄 A](90-config-reference.md)：

| 鍵 | 預設值 | 意思 |
| --- | --- | --- |
| `lua.http.allowed_hosts` | 必填 | `host`、`host:port`，或 `http://host:port`（這台主機用明文 HTTP） |
| `lua.http.timeout_ms` | `2000` | 一次呼叫從連線到讀完回應的時限 |
| `lua.http.max_response_bytes` | `1048576` | 回應內容的上限 |
| `lua.http.max_concurrent` | `64` | 每台主機同時進行的呼叫數；登入與計費分開算 |
| `lua.http.ca_file` | 無 | 系統根憑證之外另外信任的 CA |

這些鍵都可以 reload。`allowed_hosts` 寫 `https://…` 或帶路徑都不行，`check` 會指出：`lua.http.allowed_hosts: 'https://api.example.com' is not host, host:port or http://host:port`。

範例情境 `external-api` 的 `member-api.lua` 是完整的例子，它的核心是：

```lua
-- member-api.lua（節錄 external-api 情境）
local response, problem = http_request{
  url = API,
  method = 'POST',
  headers = {
    Authorization = 'Bearer ' .. API_KEY,
    ['Content-Type'] = 'application/x-www-form-urlencoded',
  },
  -- 名稱和密碼來自使用者，一律先編碼。
  body = 'username=' .. url_encode(user) .. '&password=' .. url_encode(password),
}
if not response then
  log('member API gave no answer:', problem)
  return 'reject'
end
if response.status ~= 200 then
  return 'reject'
end
local plan = response.headers['x-plan']
if plan and plan:match('^[%w_-]+$') then
  set_attr('Filter-Id', plan)
end
```

`http_request` 的結果：

- 有收到 HTTP 回應，不論狀態碼，回傳一個表：`status`（數字）、`body`（字串）、`headers`（標頭名稱一律小寫）。500、302 也是回應，不會自動跟著轉址。
- 沒有回應時，回傳 `nil` 與一段原因。例如逾時 `timed out after 1500ms`、回應太大 `the response is over 1048576 octets`、同時太多 `too many requests at once`、不在白名單 `https://evil.example is not in lua.http.allowed_hosts`。
- 這些情況腳本會繼續執行，由腳本決定怎麼辦。用錯參數（不是表、沒有 `url`、方法不是 GET／POST／PUT／DELETE／PATCH、設定 `Host` 這類標頭）才是 Lua 錯誤。

這個例子用 `verifies_password = true` 的 `lua` 處理器呼叫 API，因為 API 真的比對了密碼。

實測（用一台在本機的測試 HTTP 伺服器，`allowed_hosts = ["http://127.0.0.1:<埠>"]`）：密碼正確時 API 回 200 與 `X-Plan: gold`，登入接受，回覆帶 `Filter-Id = gold`；密碼錯誤時 API 回 401，登入拒絕。

每次呼叫都記在指標 `rradius_script_http_requests_total` 的 `result` 標籤：`ok`、`timeout`、`error`、`denied`（不在白名單，或網址被拒）、`too_large`、`busy`（同時太多）。失敗時日誌另有一行 WARN，例如 `script http_request to members.example.com: timed out after 1500ms`，只寫主機，不寫網址的其他部分。

> **注意：** `timeout_ms` 要比 NAS 的重送間隔短。API 沒回應時，腳本最好很快拒絕，而不是讓 NAS 等到放棄。

> **警告：** 不要把 API 金鑰寫在別人讀得到的腳本裡。腳本檔的權限設成只有執行 rradius 的帳號讀得到。

## 其他工具函式

| 函式 | 用途 |
| --- | --- |
| `now_unix()` | 現在的 Unix 時間（秒，整數值）。寫進資料庫的時間一律用它 |
| `random_token()` | 32 個十六進位字元的亂數，例如當 `State` |
| `totp_verify(secret, code)` | 比對 TOTP 驗證碼（Base32 密鑰、6 位數、30 秒一步，前後各容許一步）。對了回傳步數，錯了回傳 `nil` |
| `password_verify(stored, given)` | 用驗證鏈同一套規則比對存放的密碼，回傳 `true` 或 `false` |
| `wpa_pmk(passphrase, ssid)` | 算出 WPA2 的 PMK（64 個十六進位字元） |
| `url_encode(text)` | 把一段文字編碼成網址的一段（有 `[lua.http]` 時才有） |
| `log(...)` | 寫一行 INFO 日誌，參數用空格連起來，換行變空格，最長 4096 位元組 |

`totp_verify` 回傳步數，是為了讓腳本記下用過的步數，同一個代碼不能用第二次（`two-step-totp.lua` 的做法）。

`password_verify` 不會累計失敗次數，也不受 `[auth.lockout]` 保護。腳本自己比對密碼時，猜密碼的次數要自己限制；或者把密碼交給驗證鏈的處理器，讓 `[auth.lockout]` 去數。

> **注意：** `print()` 存在，但它寫到程式的標準輸出，不是日誌，用 systemd 或容器跑時通常看不到。一律用 `log()`。

> **企業版：** `send_disconnect` 與 `send_coa` 可以從腳本送 Disconnect 與 CoA。核心也有這兩個名稱，但呼叫時一定出錯：`send_coa: not supported by this program (CoA needs rradius-enterprise)`。

## EAP 與憑證的變數

EAP 登入時，驗證鏈或最上層腳本會多看到幾個變數：

| 變數 | 什麼時候有值 | 例子 |
| --- | --- | --- |
| `eap_method` | EAP-TLS 憑證通過後，以及 PEAP、TTLS 的內層登入 | `"tls"`、`"peap"`、`"ttls"` |
| `tls_subject` | EAP-TLS 憑證通過後 | `"CN=laptop-001"` |
| `tls_subject_attrs` | 同上 | `{ CN = { "laptop-001" } }`，每個名稱對應一個陣列 |

實測（wifi-certs 情境，在 sql 處理器前加一個 `lua` 處理器記錄這些變數）：用 `laptop-001` 的憑證登入時，腳本看到 `eap_method = "tls"`、`tls_subject = "CN=laptop-001"`、`tls_subject_attrs.CN[1] = "laptop-001"`。

不是 EAP 的請求，這幾個變數都是 `nil`。用法（例如依憑證的 OU 給不同 VLAN）在[第 11 章](11-eap.md)。

## 執行時間、記憶體與資料庫等待的限制

腳本跑在伺服器裡，一個寫壞的腳本不能拖垮伺服器。三個限制：

| 限制 | 設定 | 超過時 |
| --- | --- | --- |
| 執行時間 | `lua.max_execution_ms`（預設 1000） | `script exceeded the execution limit of 1000 ms`，請求失敗 |
| 記憶體 | `lua.max_memory_mb`（預設 256，每個 VM） | `memory error: not enough memory`，請求失敗 |
| 資料庫等待 | `lua.query_timeout_ms`（預設 5000，一個請求的所有呼叫加總） | 呼叫出錯；登入不回應 |

執行時間只算腳本自己在跑的時間。等資料庫、等 HTTP 回應、排隊等計費位置的時間都不算。每個請求分開計時。

實測（`max_execution_ms = 100`）：

```lua
-- policy.lua
while true do end
```

```text
… ERROR rradius_core::lua_engine: Lua script error (radius/authentication): runtime error: script exceeded the execution limit of 100 ms
```

- 超過時間後，腳本用 `pcall` 接住也沒用：之後每一步都再出同樣的錯，最後就算回傳 `'accept'` 也算失敗。實測 `pcall(function() while true do end end) return 'accept'` 的結果是拒絕。
- 一個跑很久的迴圈不會卡住其他請求：腳本每跑大約 10 毫秒就讓別的請求先跑。

記憶體上限是每個 Lua VM 的上限。VM 的數量等於程式能用的 CPU 數，所有腳本共用。一個腳本要太多記憶體時，出錯的是它自己；剛好同時在同一個 VM 上的其他腳本也可能拿到同樣的錯誤。實測 `max_memory_mb = 16` 時，`string.rep('x', 64 * 1024 * 1024)` 讓那個請求失敗，下一個請求照常接受。

## 改了腳本之後

腳本檔和設定檔一樣，送 `SIGHUP` 重新載入（reload）就換成新的，不用重啟：

```console
$ kill -HUP <伺服器的 PID>
```

```text
… INFO rradius_core::reload: configuration reloaded from rradius.toml
```

- 新腳本有語法錯誤時，reload 失敗，伺服器繼續用舊的：`reload failed, keeping the running configuration: cannot compile script 'policy.lua': syntax error: 1: Malformed string; did you forget to finish it?`
- 改 `lua.max_memory_mb` 要重啟，reload 時日誌寫 `these need a restart to apply: lua.max_memory_mb`。
- 先跑 `rradiusd check`，它編譯全部腳本（最上層與每個 `lua` 處理器），語法錯誤在這裡就看得到。

## 常見錯誤

**症狀：** 改了 `accounting.lua` 想處理登入，但腳本完全沒被叫到登入。
**原因：** 設定裡有 `[auth]`，登入交給驗證鏈，最上層腳本只收到計費。
**解法：** 把規則寫成 `lua` 處理器放進驗證鏈，或拿掉 `[auth]`。

**症狀：** 腳本判斷 `get_attr('Acct-Status-Type') == 'Start'` 永遠不成立。
**原因：** 列舉的值拿到的是數字。
**解法：** 寫 `== 1`（Start）、`== 2`（Stop）、`== 3`（Interim-Update）。

**症狀：** 日誌有 `` sql_execute: non-deterministic `strftime` is not allowed in writes; pass time values from now_unix() instead ``。
**原因：** 寫入的 SQL 用了時間或亂數函式。
**解法：** 在 Lua 用 `now_unix()` 算好，當參數傳進去。

**症狀：** 日誌有 `sql_query: 1 placeholder, 0 values`。
**原因：** SQL 有 `?`，但沒有傳參數陣列，或陣列的長度不對。
**解法：** 補上第二個參數，例如 `sql_query(sql, {user})`。

**症狀：** 日誌有 `attribute value must be a string or number, got boolean`。
**原因：** `set_attr` 的值是布林、表或 `nil`。
**解法：** 傳字串或數字。

**症狀：** 腳本回傳 `'challenge'`，NAS 卻收到 Access-Reject，日誌有 `Access-Challenge for client … not sent: '…' is not allowed in an Access-Challenge`。
**原因：** challenge 時設了 `Reply-Message`、`State`、`Idle-Timeout`、`Session-Timeout` 以外的標準屬性。
**解法：** challenge 時只設這幾個；其他屬性等接受時再設。

**症狀：** 登入沒有回應，`rradius-bench` 是 `timeouts 1`，日誌有 `not answered: the authentication backend could not be used`。
**原因：** 腳本的資料庫呼叫失敗或逾時（叢集沒有 leader、查詢太慢、外部資料庫連不上）。
**解法：** 看同一時間前面那行 ERROR 找出是哪個呼叫；查詢太慢就加索引，或調大 `lua.query_timeout_ms`。

**症狀：** 日誌有 `script exceeded the execution limit of 1000 ms`。
**原因：** 腳本自己的計算太久，常見的是沒有結束條件的迴圈，或在 Lua 裡處理大量資料。
**解法：** 把資料處理交給 SQL；真的需要時才調大 `lua.max_execution_ms`。

**症狀：** 腳本呼叫 `http_request` 得到 `attempt to call a nil value`。
**原因：** 設定沒有 `[lua.http]`，腳本裡沒有這個函式。
**解法：** 加上 `[lua.http]` 與 `allowed_hosts`。

**症狀：** `http_request` 回傳 `nil` 與 `http://… is not in lua.http.allowed_hosts`。
**原因：** 網址的主機、埠或 http／https 和白名單不完全相同。只寫主機時只允許 HTTPS 的預設埠。
**解法：** 白名單寫成和網址一致，例如 `api.example.com:8443`，或 `http://10.0.0.5:8080`。

**症狀：** 照 `disconnect-disabled.lua` 寫的腳本，日誌有 `send_disconnect: not supported by this program (CoA needs rradius-enterprise)`。
**原因：** 核心不送 CoA 與 Disconnect。
**解法：** 拿掉這段；計費請求照常回傳 `'accept'`。

## 相關章節

- [03 第一次啟動與登入](03-quick-start.md)
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)
- [09 外部資料庫與具名查詢](09-external-databases.md)
- [11 802.1X 與 EAP](11-eap.md)
- [12 TACACS+ 設備管理](12-tacacs.md)
- [13 計費與 session](13-accounting.md)
- [15 情境實作：組織網路存取](15-access-recipes.md)
- [16 叢集與高可用](16-cluster.md)
- [附錄 A：設定參考](90-config-reference.md)
- [附錄 D：Lua API 參考](93-lua-reference.md)
