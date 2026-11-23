# 09 外部資料庫與具名查詢

> 本章讓你做到：
> - 連上既有的 PostgreSQL 或 MySQL，讓驗證鏈直接用裡面的帳號表
> - 從 Lua 腳本用 `db_query`、`db_execute` 讀寫外部資料庫
> - 用 `/readyz`、指標與 `status` 監看外部資料庫的健康
> - 把一段 SQL 變成 Prometheus 指標，也能用 `query` 指令直接查

前置條件：照[第 03 章](03-quick-start.md)跑過 quick-start；讀過[第 07 章](07-users-and-chains.md)的 sql 處理器。要從腳本使用外部資料庫，先讀[第 10 章](10-lua-policies.md)。

本章小節：

- [叢集資料庫與外部資料庫](#叢集資料庫與外部資料庫)
- [設定一個外部資料庫](#設定一個外部資料庫)
- [讓 sql 處理器用外部資料庫](#讓-sql-處理器用外部資料庫)
- [從腳本讀寫外部資料庫](#從腳本讀寫外部資料庫)
- [資料庫連不上時](#資料庫連不上時)
- [監看健康狀態](#監看健康狀態)
- [具名查詢：把 SQL 變成指標](#具名查詢把-sql-變成指標)
- [改了設定之後](#改了設定之後)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

## 叢集資料庫與外部資料庫

rradius 本身就有一個資料庫：叢集資料庫（內建的 hiqlite，每個節點一份、用 Raft 複寫的 SQLite）。quick-start 的帳號表與計費表都在裡面。外部資料庫是你另外架設、rradius 透過網路連過去的 PostgreSQL 或 MySQL。

| | 叢集資料庫 | 外部資料庫 |
| --- | --- | --- |
| 在哪裡 | 每個節點本機一份，自動複寫 | 另一台伺服器，rradius 用連線池連過去 |
| 設定 | `[cluster]` | `[databases.<name>]`，可以有好幾個 |
| 建表 | `cluster.migrations_dir` 的 migration | 你自己在資料庫上建 |
| sql 處理器 | 不寫 `database` | `database = "<name>"` |
| Lua | `sql_query`、`sql_execute` | `db_query`、`db_execute` |
| 具名查詢 | 可以 | 不行，`[queries]` 只查叢集資料庫 |
| 讀的延遲 | 本機讀，不經網路 | 每次一趟網路 |
| 寫入的限制 | 不能用時間、亂數這類每個節點算出不同結果的函式 | 沒有這個限制 |

什麼時候用外部資料庫：

- 帳號已經在別的系統裡（例如會員系統、舊的 RADIUS 伺服器的帳號表），你不想搬，也不想同步。
- 要把計費資料寫到報表或計費系統直接讀的地方。

帳號完全由 rradius 管理時，用叢集資料庫就好：登入時讀本機，不受網路與另一台伺服器的影響。把舊系統的帳號與密碼雜湊搬進叢集資料庫的做法在第 07 章。

## 設定一個外部資料庫

每個外部資料庫一節 `[databases.<name>]`。`<name>` 由你取，只能用英文字母、數字與 `_`，sql 處理器與腳本用這個名稱指它。

```toml
# rradius.toml
# …（其他設定略）

[databases.billing]
kind = "postgresql"
host = "db.example.com"
database = "radius"
user = "rradius"
password_file = "billing.secret"
```

密碼放在檔案裡，權限設成只有執行 rradius 的帳號讀得到：

```console
$ printf '%s\n' '<資料庫密碼>' > billing.secret
$ chmod 600 billing.secret
```

檔案結尾的換行會去掉。檔案是空的時 `check` 會出錯：`databases.billing.password_file: 'billing.secret' is empty`。

各鍵（完整說明見[附錄 A](90-config-reference.md)；整個 `[databases]` 改了都要重啟）：

| 鍵 | 預設 | 說明 |
| --- | --- | --- |
| `kind` | — | `postgresql` 或 `mysql`，必填 |
| `host` | — | 主機名稱或位址，必填 |
| `port` | PostgreSQL `5432`，MySQL `3306` | |
| `database` | — | 資料庫名稱，必填 |
| `user` | — | 帳號，必填 |
| `password`、`password_file` | — | 最多設一個；都不設就不送密碼。範例一律用 `password_file` |
| `tls` | `verify` | `verify` 加密並驗證憑證與主機名稱；`off` 不加密 |
| `ca_file` | 系統根憑證 | `tls = "verify"` 時另外信任的 CA（PEM） |
| `pool_size` | `8` | 最多保持幾條連線 |
| `connect_timeout_ms` | `5000` | 建立一條連線的時限，毫秒 |
| `required_for_ready` | `false` | 連不上時 `/readyz` 是否回 503，見[監看健康狀態](#監看健康狀態) |

`rradius-server config show` 會印出補上預設值之後的設定，可以用來確認 `pool_size` 這類沒寫的值。

### 連線數要算多少

每個節點有自己的連線池，各自連到外部資料庫。資料庫那邊看到的連線數最多是：

```text
pool_size × 節點數，再加上每個節點 1 條健康檢查的連線（只有 required_for_ready = true 時）
```

例如三個節點、`pool_size = 8`、`required_for_ready = true`，最多 27 條。資料庫的連線上限（PostgreSQL 的 `max_connections`、MySQL 的 `max_connections`）要留這個量，還要留給其他系統。

連線池是開機時建好、第一次用到時才連，不會一啟動就占滿；閒置的連線留著重複使用。`pool_size` 太小時，尖峰時請求要排隊等連線，等太久就和資料庫連不上一樣不回應（見[資料庫連不上時](#資料庫連不上時)）。先用預設的 8，看 `rradius_external_db_connections{state="in_use"}` 在尖峰時有沒有碰到上限再調。

### 給 rradius 的資料庫帳號

只給它需要的權限。只讀帳號表時，PostgreSQL 可以這樣建：

```sql
-- 在 radius 資料庫上，以管理者執行
CREATE ROLE rradius_ro LOGIN PASSWORD '<資料庫密碼>';
GRANT SELECT ON radcheck, radreply TO rradius_ro;
```

腳本要寫入計費副本時，另外建一個帳號，只對那個表有 `INSERT` 與 `UPDATE`，設成另一個 `[databases.<name>]`。一個帳號一種用途，帳號外流時能做的事就少。

實測（PostgreSQL 17）：用這個帳號，sql 處理器照常登入；腳本對 `radcheck` 下 `DELETE` 時得到 `permission denied for table radcheck`，讀沒有授權的表時得到 `permission denied for table acct_copy`。

### TLS

`tls = "verify"` 是預設：連線加密，資料庫的憑證要由信任的 CA 簽發，而且憑證上的名稱要是 `host` 寫的那個。資料庫用自己的 CA 時：

```toml
# rradius.toml（片段）
[databases.billing]
# …（kind、host 等同上）
ca_file = "db-ca.pem"
```

`ca_file` 在 `check` 與啟動時就讀，檔案不存在或裡面沒有憑證，直接出錯，不會拖到第一次登入才發現。

實測（PostgreSQL 17，伺服器憑證由自建的 CA 簽發，憑證上的名稱只有 `localhost`）：

| 設定 | 結果 |
| --- | --- |
| `host = "localhost"`、`ca_file = "db-ca.pem"` | 連上，登入正常 |
| `host = "127.0.0.1"`、`ca_file = "db-ca.pem"` | 連不上：`invalid peer certificate: certificate not valid for name "127.0.0.1"; certificate is only valid for DnsName("localhost")` |
| `host = "localhost"`、沒有 `ca_file` | 連不上：`invalid peer certificate: UnknownIssuer` |

憑證驗不過時，和資料庫連不上一樣處理：`required_for_ready = true` 時啟動就記 `not reachable at startup: …`，登入不回應（見[資料庫連不上時](#資料庫連不上時)）。

`tls = "off"` 只適合資料庫在同一台機器上（`host` 是 `127.0.0.1` 之類的 loopback 位址）。`host` 不是 loopback 時，啟動和 `check` 都會警告：

```text
… WARN rradius_core::external_db: databases.legacy: tls = "off" sends the password and the data to db.example.com unencrypted
```

> **警告：** `tls = "off"` 時，資料庫的密碼與每一筆查詢的內容（包括帳號名稱與密碼雜湊）都以明文在網路上傳送。

MySQL 搭配 `tls = "off"` 時，資料庫帳號要用 `mysql_native_password`。MySQL 8 新建帳號預設的 `caching_sha2_password` 在不加密的連線上需要 rradius 沒有內建的 RSA 交換，連線會失敗。實測（MySQL 8.4，伺服器以 `--mysql-native-password=ON` 啟動）：

| 帳號的驗證方式 | `tls` | 結果 |
| --- | --- | --- |
| `mysql_native_password` | `off` | 連上，登入正常 |
| `caching_sha2_password` | `off` | 連不上，登入不回應：``error with configuration: RSA auth backend disabled; enable feature `mysql-rsa` (or `rsa` if using sqlx-mysql directly) or use TLS.`` |
| `caching_sha2_password` | `verify`（`ca_file`） | 連上，登入正常 |
| `mysql_native_password` | `verify`（`ca_file`） | 連上，登入正常 |

建 `mysql_native_password` 的帳號：`CREATE USER 'rradius'@'%' IDENTIFIED WITH mysql_native_password BY '<資料庫密碼>';`。MySQL 8.4 預設停用這個驗證方式，伺服器要以 `--mysql-native-password=ON` 啟動（本書的測試就是這樣啟動）。能用 TLS 時，用 `verify` 比改帳號的驗證方式好。

## 讓 sql 處理器用外部資料庫

sql 處理器加上 `database`，它的查詢就送到那個外部資料庫，而不是叢集資料庫。以舊系統的 `radcheck`、`radreply` 表為例（`rradius-server/examples/scenarios/migrate-users/external-db.toml`）：

```toml
# rradius.toml
# …（其他設定略）

[databases.legacy]
kind = "postgresql"
host = "db.example.com"
database = "radius"
user = "rradius_ro"
password_file = "legacy-db.secret"

[auth]
policy = "first_match"

[[auth.handlers]]
type = "sql"
database = "legacy"
user_query = "SELECT password FROM radcheck WHERE username = ?"
reply_query = "SELECT attribute, value FROM radreply WHERE username = ?"
```

- SQL 裡的值一律寫 `?`。送到 PostgreSQL 時，rradius 依序改寫成 `$1`、`$2`……；送到 MySQL 時原樣送出。`?1`、`:name` 這類寫法不接受。
- 密碼欄的格式和叢集資料庫的帳號表相同（`{crypt}`、`{clear}` 等前綴，見第 07 章）。
- 只要讀，所以給 rradius 一個唯讀帳號，能讀這兩個表就好。
- `database` 寫了 `[databases]` 裡沒有的名稱，`check` 就會出錯：`auth.handlers[0].database: there is no [databases.nosuch]`。

實測（PostgreSQL 17 與 MySQL 8.4，表與 quick-start 的帳號相同）：`bob`（bcrypt）與 `alice`（明文）用正確密碼都是 `accept 1`，錯的密碼是 `reject 1`。

## 從腳本讀寫外部資料庫

腳本用兩個函式使用外部資料庫，第一個參數是 `[databases.<name>]` 的名稱：

| 函式 | 做什麼 | 回傳 |
| --- | --- | --- |
| `db_query(name, sql, params)` | 讀 | 一列一個表的陣列，欄位名稱當鍵 |
| `db_execute(name, sql, params)` | 寫 | 影響的列數 |

參數規則與 `sql_query` 相同：值一律用 `?` 與參數陣列傳，不要拼進 SQL 字串。每個函式的錯誤訊息在[附錄 D](93-lua-reference.md)。

例：把計費資料另外寫一份到計費系統的 PostgreSQL。在 quick-start 的 `accounting.lua` 最後的 `return 'accept'` 之前加上：

```lua
-- accounting.lua（片段，放在最後的 return 'accept' 之前）
db_execute('billing', [[
  INSERT INTO acct_copy (nas, session_id, username, updated, session_time)
  VALUES (?, ?, ?, ?, ?)
  ON CONFLICT (nas, session_id) DO UPDATE SET
    updated = GREATEST(acct_copy.updated, excluded.updated),
    session_time = GREATEST(acct_copy.session_time, excluded.session_time)
]], {nas, session, get_attr('User-Name') or '', seen, seconds})
```

`nas`、`session`、`seen`、`seconds` 是 quick-start 的腳本前面已經算好的變數。要注意兩件事：

- 這個寫入要能重複執行。外部資料庫連不上時腳本失敗，之後不管是 NAS 重送，還是從計費 spool（寫在磁碟上、等待重送的暫存）重送（replay），腳本都從頭再跑一次，前面寫進叢集資料庫的那筆也會再寫一次。所以兩邊都用「已經有就更新」的寫法。
- 外部資料庫沒有叢集複寫的問題，`now()` 這類函式可以用；但時間最好還是用腳本算好的值，兩邊的資料才一致。

PostgreSQL 這邊要先建好表：

```sql
-- 在 billing 資料庫上執行
CREATE TABLE acct_copy (
  nas TEXT NOT NULL,
  session_id TEXT NOT NULL,
  username TEXT NOT NULL,
  updated BIGINT NOT NULL,
  session_time BIGINT NOT NULL,
  PRIMARY KEY (nas, session_id)
);
```

### 值的型別

| 資料庫的欄位 | Lua 拿到的 |
| --- | --- |
| 整數、浮點數 | 數字 |
| 布林 | `1` 或 `0` |
| 文字 | 字串 |
| 二進位 | 位元組的字串 |
| PostgreSQL 的 `NUMERIC`、MySQL 的 `DECIMAL` | 十進位的字串，不四捨五入（`SUM`、`AVG` 的結果常是這種） |
| NULL | `nil` |

讀不了的型別會出錯，並提示你在 SQL 裡轉成文字。實測 PostgreSQL 與 MySQL 的 `DATE` 欄都讀不了：`db_query: database billing: column created has a type that cannot be read; CAST it to text in the SQL`；改成 `CAST(created AS TEXT) AS created` 拿到 `'2026-11-23'`。MySQL 的 `BIGINT UNSIGNED` 存了超過有號 64 位元的值時也出錯：`column u_big holds 18446744073709551615, more than a script number can hold; CAST it to text in the SQL`。

實測的其他對應：PostgreSQL 的 `NUMERIC(8,2)` 欄 `12.50` 拿到字串 `'12.50'`，`SUM` 也是 `'12.50'`；`BOOLEAN` 的 `true` 拿到 `1`；`BYTEA` 拿到位元組字串；MySQL 的 `DECIMAL(8,2)` 拿到 `'12.50'`，`TINYINT(1)` 拿到 `1`。

> **注意：** 腳本的數字是雙精度浮點數，超過 2^53（9007199254740992）的整數會失去精確度。實測 PostgreSQL `BIGINT` 的 `9007199254740993` 拿到 `9007199254740992`。這麼大的 ID 一律 `CAST` 成文字再讀。

### 時間的限制

一個請求裡所有資料庫呼叫共用 `lua.query_timeout_ms`（預設 5000 毫秒），`sql_query`、`sql_execute`、`db_query`、`db_execute` 加起來算。計費腳本等外部資料庫時讓出計費的位置，不會卡住其他計費請求。

實測（PostgreSQL 17）：`db_query('billing', 'SELECT username FROM radcheck WHERE username = ? OR username = ?', {'alice', 'bob'})` 回傳兩列，表示兩個 `?` 依序改寫成 `$1`、`$2`；SQL 字串常值裡的 `'?'` 不會被當成參數。上面的計費片段寫進 `acct_copy`：同一個 session 先寫 `updated = 100`，再寫比較舊的 `90`，結果還是 `100`；再寫 `200` 就更新成 `200`，重送不會讓資料倒退。

## 資料庫連不上時

外部資料庫連不上時，rradius 照常啟動，連線池在第一次用到時才連。這個情況不需要真的資料庫就能在 quick-start 試出來，上線前值得先看一次。

### 在 quick-start 試一次

目標：讓 sql 處理器改用一個連不上的外部資料庫，看登入、`/readyz` 與日誌的反應。

設定：在 quick-start 的 `rradius.toml` 裡，sql 處理器加一行 `database = "billing"`，最後加上一個指向沒有程式在聽的埠（這裡故意用 15432）的資料庫：

```toml
# rradius.toml（sql 處理器加一行）
[[auth.handlers]]
type = "sql"
database = "billing"
# …（user_query、reply_query 不變）
```

```toml
# rradius.toml（加在最後）
[databases.billing]
kind = "postgresql"
host = "127.0.0.1"
port = 15432
database = "radius"
user = "rradius"
password_file = "billing.secret"
tls = "off"
required_for_ready = true
```

`billing.secret` 隨便放一個值就好（`printf 'unused\n' > billing.secret`）。啟動伺服器，日誌有：

```text
… WARN rradius_core::external_db: databases.billing: not reachable at startup: error communicating with database: Connection refused (os error 61); /readyz answers 503 until it is
```

這行只有 `required_for_ready = true` 的資料庫才有；`host` 是 loopback，所以沒有 `tls = "off"` 的警告。

驗證：用 `bob` 登入一次，再看 `/readyz`：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user bob --password bob-password --load pap,concurrency=1 --requests 1
…
  pap: 0/s, sent 1, accept 0, reject 0, challenge 0, accounting 0, timeouts 1, invalid 0, late 0, socket errors 0, retransmits 0
$ curl -s -w ' %{http_code}\n' http://127.0.0.1:9812/readyz
database billing unreachable 503
```

結果：登入沒有回應（`timeouts 1`），不是拒絕。日誌在大約 5 秒後（`connect_timeout_ms` 的預設；設成 `1000` 時約 1 秒）寫：

```text
… ERROR rradius_core::auth_chain: auth sql handler error: database billing: pool timed out while waiting for an open connection
… WARN rradius_core::lua_engine: RADIUS Access-Request from client localhost not answered: the authentication backend could not be used
```

同一時間送計費 Start 照常有回應，因為 quick-start 的計費腳本寫的是叢集資料庫，不經過 `billing`。試完把這兩段拿掉。

### 誰會受影響

登入時用到連不上的外部資料庫：

| 誰用它 | 結果 |
| --- | --- |
| sql 處理器 | 不回應；日誌寫 `auth sql handler error: database billing: pool timed out while waiting for an open connection` 與 `RADIUS Access-Request from client localhost not answered: the authentication backend could not be used` |
| `lua` 處理器或最上層腳本的 `db_query` | 不回應。腳本用 `pcall` 接住錯誤、回傳 `'reject'` 也一樣不回應 |
| 計費腳本的 `db_execute` | 有計費 spool（`[radius.accounting_spool]`）時，先寫進 spool 並回 Accounting-Response，資料庫恢復後重送；沒有 spool 時不回應，NAS 會重送 |
| TACACS+ | 回 ERROR，讓設備改問下一台伺服器 |

為什麼是不回應而不是拒絕：資料庫連不上不是使用者的錯。拒絕會讓使用者被踢掉或被鎖住；不回應時，NAS 會重送或改問另一台 RADIUS 伺服器。所以 NAS 上最好設兩台以上的 RADIUS 伺服器，而且它們不要依賴同一個外部資料庫。

失敗的呼叫依原因記在指標 `rradius_external_db_failures_total` 的 `reason` 標籤：

| `reason` | 意思 |
| --- | --- |
| `unavailable` | 連不上、連線池等不到連線、逾時、資料庫說它現在忙（連線數滿、死結等）。之後再試可能就好 |
| `mistake` | SQL 或參數錯了，資料庫拒絕這個敘述。再試也一樣 |
| `unknown` | 其他 |

只有 `unavailable` 會讓登入變成不回應；`mistake` 是腳本或設定的錯，登入照常被拒絕，要改 SQL。

## 監看健康狀態

`required_for_ready = true` 表示「這個資料庫連不上，這個節點就不該收流量」：

```toml
# rradius.toml（片段）
[databases.billing]
# …（kind、host 等同上）
required_for_ready = true
```

設了之後，rradius 每 5 秒用一條不在連線池裡的專用連線執行一次 `SELECT 1`：連續兩次失敗算連不上，一次成功就恢復。連不上時 `/readyz` 回 503，負載平衡器或 Kubernetes 就不會把流量送過來；`/livez` 不受影響，程式不會因此被重啟。實測：

```console
$ curl -s -w ' %{http_code}\n' http://127.0.0.1:9812/readyz
database billing unreachable 503
$ curl -s -w ' %{http_code}\n' http://127.0.0.1:9812/livez
ok 200
```

實測（PostgreSQL 17，執行中把資料庫停掉再啟動）：停掉後約 10 秒 `/readyz` 變成 503，日誌寫：

```text
… WARN rradius_core::external_db: databases.legacy: unreachable after 2 failed checks, the last: error communicating with database: Connection refused (os error 61); /readyz answers 503
```

資料庫再啟動後約 5 秒恢復成 200，日誌寫 `databases.legacy: reachable`。

> **注意：** 只有每一個節點都依賴它才能運作的資料庫才設 `required_for_ready`。只用在計費副本這類次要用途的資料庫，設了反而會讓整個叢集因為它而停止收流量。

每個外部資料庫在 `/metrics` 有這些指標（實測的輸出）：

```text
rradius_external_db_failures_total{database="billing",reason="unavailable"} 1
rradius_external_db_failures_total{database="billing",reason="mistake"} 0
rradius_external_db_failures_total{database="billing",reason="unknown"} 0
rradius_external_db_connections{database="billing",state="in_use"} 0
rradius_external_db_connections{database="billing",state="idle"} 0
rradius_external_db_connections_max{database="billing"} 8
rradius_external_db_reachable{database="billing"} 0
```

| 指標 | 意思 |
| --- | --- |
| `rradius_external_db_failures_total` | 失敗的呼叫數，依 `reason` |
| `rradius_external_db_connections` | 連線池目前的連線：`in_use` 用中或正在建立，`idle` 開著沒在用 |
| `rradius_external_db_connections_max` | 就是 `pool_size` |
| `rradius_external_db_reachable` | 健康檢查連得上是 1，否則 0；只有 `required_for_ready = true` 的資料庫才有 |

`in_use` 一直等於 `max` 表示連線不夠用，請求在排隊等連線：先看資料庫那邊的查詢是不是太慢，再考慮調大 `pool_size`。

`rradius-server status` 有一段 `External databases`：

```console
$ rradius-server status
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

告警可以這樣設：`rradius_external_db_reachable == 0` 持續一分鐘，或 `rate(rradius_external_db_failures_total{reason="unavailable"}[5m]) > 0`。指標的完整清單在[附錄 C](92-metrics-reference.md)。

## 具名查詢：把 SQL 變成指標

具名查詢是你寫在設定裡的唯讀 SQL。每個節點定期執行它，結果變成 Prometheus 的 gauge `rradius_query_<name>`。適合「現在有幾個帳號停用」「位址池還剩多少」這類問題，不用另外寫匯出程式。

> **注意：** 具名查詢只查叢集資料庫，不能查外部資料庫。

在 quick-start 的 `rradius.toml` 最後加上：

```toml
# rradius.toml（加在最後）
[queries.users_by_state]
sql = "SELECT CASE enabled WHEN 1 THEN 'enabled' ELSE 'disabled' END AS state, count(*) AS n FROM users GROUP BY state"
help = "Accounts in the users table, by state"
labels = ["state"]
value = "n"
interval_ms = 10000

[queries.open_sessions]
sql = "SELECT count(*) AS value FROM accounting WHERE stopped IS NULL"
help = "Sessions without a Stop"
```

各鍵（完整說明見附錄 A；整個 `[queries]` 改了都要重啟）：

| 鍵 | 預設 | 說明 |
| --- | --- | --- |
| `sql` | — | 必填。開頭要是 `SELECT` 或 `WITH`，不能有參數 |
| `value` | `value` | 樣本值的欄位，要是整數或實數 |
| `labels` | `[]` | 當作標籤的欄位，標籤名稱就是欄位名稱；值要是文字或整數 |
| `help` | 寫著查詢名稱的一段文字 | 指標的 `# HELP` |
| `interval_ms` | `30000` | 兩次執行開始的間隔，毫秒，至少 5000 |
| `timeout_ms` | `5000` | 一次執行的時限，毫秒，1 到 `interval_ms` |
| `max_rows` | `200` | 最多幾列成為樣本，1 到 10000 |

查詢的名稱用小寫英文字母開頭，只能有小寫字母、數字與 `_`。`list`、`failures_total` 這類和指令或其他指標撞名的名稱不能用。

重啟之後，`/metrics` 多了這些（實測）：

```console
$ curl -s http://127.0.0.1:9812/metrics | grep rradius_query_users_by_state
# HELP rradius_query_users_by_state Accounts in the users table, by state
# TYPE rradius_query_users_by_state gauge
rradius_query_users_by_state{state="disabled"} 1
rradius_query_users_by_state{state="enabled"} 2
```

quick-start 有三個示範帳號，其中一個停用，所以是 1 與 2。

- 每個節點各自執行、各自回報，都是本機那份資料庫的結果。Prometheus 從每個節點抓時，用 `max` 或只取一個節點，不要加總。
- 啟動後馬上執行一次，之後每 `interval_ms` 一次。一次跑得比間隔久，下一次等它結束才開始，不會重疊。
- `/metrics` 給的是最後一次執行的結果，不是現在的值。

再一個例子：每台 NAS 現在有幾個 session。結果的列數跟著 NAS 的數目走，所以把 `max_rows` 設得比 NAS 的數目大：

```toml
# rradius.toml（加在最後）
[queries.sessions_by_nas]
sql = "SELECT nas, count(*) AS value FROM accounting WHERE stopped IS NULL GROUP BY nas"
help = "Open sessions by NAS"
labels = ["nas"]
max_rows = 500
```

實測：用 `rradius-bench` 送一筆計費 Start（它帶的 `NAS-IP-Address` 是 `192.0.2.1`），下一次執行之後：

```text
rradius_query_sessions_by_nas{nas="192.0.2.1"} 1
```

標籤的值是有限的幾種時才適合當標籤，例如 NAS、位址池、帳號狀態。不要用帳號名稱或 session ID 當標籤：每個不同的值都是 Prometheus 裡的一條時間序列，數量大時會拖垮 Prometheus。

### 用指令查詢

`query` 指令透過叢集的 API 埠執行，伺服器要在執行中。

列出所有具名查詢：

```console
$ rradius-server query list
name=open_sessions
help=Sessions without a Stop
interval_ms=30000
sql=SELECT count(*) AS value FROM accounting WHERE stopped IS NULL

name=users_by_state
help=Accounts in the users table, by state
interval_ms=10000
sql=SELECT CASE enabled WHEN 1 THEN 'enabled' ELSE 'disabled' END AS state, count(*) AS n FROM users GROUP BY state
```

執行一個：

```console
$ rradius-server query users_by_state
state     n
disabled  1
enabled   2
```

指令是當下重新執行 SQL，不是讀 `/metrics` 的結果，也不受 `max_rows` 限制。實測：送一筆計費 Start 之後馬上查，`query open_sessions` 已經是 `1`，`/metrics` 的 `rradius_query_open_sessions` 還是上一次執行時的 `0`，要等下一次（預設 30 秒）才更新。

加 `--json` 給程式讀：

```console
$ rradius-server query open_sessions --json
[{"value":1}]
```

不在設定裡的名稱，結束碼是 2：

```console
$ rradius-server query nosuch
queries.nosuch is not set in rradius.toml
```

### 查詢失敗與丟掉的列

每個具名查詢還有三個共用的指標：

| 指標 | 意思 |
| --- | --- |
| `rradius_query_failures_total{query,reason}` | 失敗的次數。`timeout` 超過 `timeout_ms`；`error` 資料庫拒絕這個 SQL；`shape` 結果的欄位不對 |
| `rradius_query_dropped_rows_total{query,reason}` | 成功的執行裡沒變成樣本的列。`null_value` 值是 NULL；`over_limit` 超過 `max_rows` |
| `rradius_query_last_success_timestamp_seconds{query}` | 最後一次成功的 Unix 時間；從沒成功過是 0 |

- 失敗時，gauge 保留上一次成功的結果，不會清空。所以要看 `last_success_timestamp_seconds` 有沒有在前進，才知道數字是不是新的。
- 失敗會記一行 WARN，同一個原因一分鐘最多一行，例如 `query broken: the run failed (shape): the result has no column "n"`。
- `shape` 的原因包括：沒有 `value` 或 `labels` 寫的欄位、值不是數字、標籤是 NULL、兩列的標籤完全相同。

例：忘了寫 `value`。下面這個查詢的值欄叫 `n`，但沒有設 `value = "n"`：

```toml
# rradius.toml（加在最後；故意寫錯）
[queries.users_count]
sql = "SELECT count(*) AS n FROM users"
help = "Accounts in the users table"
```

`check` 看不出來，因為要執行才知道結果有哪些欄位。啟動後日誌馬上有：

```text
… WARN rradius_core::named_queries: query users_count: the run failed (shape): the result has no column "value"
```

`/metrics` 裡只有 `# HELP` 與 `# TYPE`，沒有樣本；共用的指標顯示失敗：

```text
rradius_query_failures_total{query="users_count",reason="shape"} 1
rradius_query_last_success_timestamp_seconds{query="users_count"} 0
```

`status` 寫 `users_count last success (seconds ago)     never`。但 `rradius-server query users_count` 照常印出 `n` 與 `3`，因為指令只執行 SQL，不檢查欄位。所以指令查得到、指標卻沒有時，先看日誌的 `shape`。改法：加上 `value = "n"`，或把 SQL 改成 `count(*) AS value`，然後重啟。

`rradius-server status` 的 `Queries` 一段（實測）：

```console
$ rradius-server status
…
Queries
  open_sessions last success (seconds ago)   1
  open_sessions rows                         1
  open_sessions failed runs                  0.0/s (0)
  users_by_state last success (seconds ago)  1
  users_by_state rows                        2
  users_by_state failed runs                 0.0/s (0)
```

告警可以這樣設：`time() - rradius_query_last_success_timestamp_seconds > 3 * <interval 秒數>`。

## 改了設定之後

`[databases]` 與 `[queries]` 改了都要重啟。送 `SIGHUP` 重新載入（reload）時，其他設定照常生效，日誌會列出沒套用的部分：

```text
… WARN rradius_core::reload: configuration reloaded from rradius.toml; these need a restart to apply: [databases]
```

`[queries]` 改了是 `these need a restart to apply: [queries]`。先用 `rradius-server check` 檢查，`[databases]` 與 `[queries]` 的大部分錯誤在這裡就看得到。

## 常見錯誤

**症狀：** `check` 出現 `databases.billing.kind must be "mysql" or "postgresql", not "oracle"`。
**原因：** 只支援 PostgreSQL 與 MySQL。
**解法：** 改成 `postgresql` 或 `mysql`。

**症狀：** `check` 出現 `` `url`: unknown field `url` ``。
**原因：** 外部資料庫不接受連線字串。
**解法：** 拆成 `host`、`port`、`database`、`user`、`password_file`。

**症狀：** `check` 出現 `databases.bill-ing: a database name may only have ASCII letters, digits and _`。
**原因：** 名稱有 `-`。
**解法：** 改成 `bill_ing`，sql 處理器與腳本裡的名稱一起改。

**症狀：** `check` 出現 `databases.billing.password: at most one of password and password_file may be set`。
**原因：** 兩個都設了。
**解法：** 留 `password_file`。

**症狀：** `check` 出現 `databases.billing.tls must be "verify" or "off", not "require"`。
**原因：** `tls` 只有兩個值。
**解法：** 要加密就用 `verify`（預設），並用 `ca_file` 指定資料庫的 CA。

**症狀：** `check` 出現 `databases.billing.ca_file: cannot read 'db-ca.pem': I/O error: No such file or directory (os error 2)` 或 `databases.billing.ca_file: no certificate in 'db-ca.pem'`。
**原因：** `ca_file` 的路徑不對，或檔案不是 PEM 格式的憑證。相對路徑以設定檔所在的目錄為準。
**解法：** 確認路徑；DER 格式的憑證先用 `openssl x509 -inform der -in ca.der -out db-ca.pem` 轉成 PEM。

**症狀：** `check` 出現 `databases.billing.ca_file is only used with tls = "verify"`。
**原因：** `tls = "off"` 不驗證憑證，`ca_file` 沒有用處。
**解法：** 二選一：拿掉 `ca_file`，或拿掉 `tls = "off"`。

**症狀：** 登入沒有回應，日誌有 `pool timed out while waiting for an open connection`。
**原因：** 連不上外部資料庫，或連線池的連線都在用，等超過時限。
**解法：** 看 `rradius_external_db_connections`：`idle` 與 `in_use` 都是 0 表示根本連不上，查網路、帳號與 TLS；`in_use` 一直等於 `max` 表示連線不夠或查詢太慢。

**症狀：** 日誌有 `invalid peer certificate: UnknownIssuer`。
**原因：** 資料庫的憑證不是系統信任的 CA 簽的，例如自建的 CA。
**解法：** 用 `ca_file` 指定那個 CA 的憑證（PEM）。

**症狀：** 日誌有 `invalid peer certificate: certificate not valid for name "127.0.0.1"`。
**原因：** `host` 寫的名稱不在資料庫憑證上，例如憑證只有 `localhost`，`host` 卻寫位址。
**解法：** `host` 改成憑證上的名稱，或重新簽一張包含這個名稱的憑證。

**症狀：** MySQL 連不上，日誌有 `RSA auth backend disabled`。
**原因：** `tls = "off"`，而資料庫帳號用的是 MySQL 8 預設的 `caching_sha2_password`。
**解法：** 改用 `tls = "verify"`；不能用 TLS 時，把帳號改成 `mysql_native_password`。

**症狀：** 腳本出現 `db_query: there is no database 'billing' in [databases]`。
**原因：** 名稱拼錯，或沒有這一節；這個錯誤要到腳本執行時才出現，`check` 抓不到。
**解法：** 對照 `[databases.<name>]` 的名稱。

**症狀：** 腳本出現 `column x has a type that cannot be read; CAST it to text in the SQL`。
**原因：** 欄位的型別腳本沒有對應的值，常見的是日期與時間。
**解法：** 在 SQL 裡轉成文字，例如 `CAST(created AS TEXT) AS created`。

**症狀：** `check` 出現 `queries.ok.sql: must begin with SELECT or WITH: it only reads`。
**原因：** 具名查詢只能讀。
**解法：** 改成 `SELECT` 或 `WITH` 開頭的查詢。

**症狀：** `check` 出現 `queries.ok.interval_ms must be at least 5000, not 1000`。
**原因：** 間隔太短。
**解法：** 至少 5000。`timeout_ms` 也不能大於 `interval_ms`。

**症狀：** `check` 出現 `queries.ok.labels: "value" is also the value column`。
**原因：** 同一個欄位不能又是值又是標籤。
**解法：** 從 `labels` 拿掉它，或另外指定 `value`。

**症狀：** 日誌有 `query broken: the run failed (shape): the result has no column "n"`，指標一直沒變。
**原因：** `value` 寫的欄位不在查詢結果裡。沒寫 `value` 時，結果要有一個叫 `value` 的欄位。
**解法：** 在 SQL 裡用 `AS` 取成同一個名字，例如 `count(*) AS n`。

**症狀：** `rradius_query_dropped_rows_total{reason="over_limit"}` 一直增加。
**原因：** 結果的列數超過 `max_rows`，多的被丟掉。
**解法：** 在 SQL 裡先彙總或加 `LIMIT`；真的需要時調大 `max_rows`（最多 10000）。

## 相關章節

- [03 第一次啟動與登入](03-quick-start.md)
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)
- [10 用 Lua 寫政策](10-lua-policies.md)
- [17 監控、狀態與日誌](17-monitoring.md)
- [附錄 A：設定參考](90-config-reference.md)
- [附錄 C：指標參考](92-metrics-reference.md)
- [附錄 D：Lua API 參考](93-lua-reference.md)
