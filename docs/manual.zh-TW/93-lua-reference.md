# 附錄 D：Lua API 參考

> 本章讓你做到：
> - 查到腳本裡每個全域變數與函式的簽名、回傳值、錯誤與限制
> - 知道屬性與資料庫的值轉成 Lua 是什麼型別
> - 知道腳本能用哪些 Lua 內建程式庫、哪些被拿掉
> - 自己比對這份清單和程式碼是否一致

前置條件：讀過[第 10 章](10-lua-policies.md)。本附錄只列 API；怎麼寫政策、為什麼這樣寫，在第 10 章。

本附錄的內容來自 `rradius-core/src/lua_engine.rs` 裡腳本環境的註冊處（`BINDER` 常數與 `make_binder`），以及 `rradius-core/src/script_http.rs`。錯誤訊息都是在 quick-start 上實際跑出來的，照原文抄錄。

本章小節：

- [腳本的執行環境](#腳本的執行環境)
- [全域變數](#全域變數)
- [請求的屬性](#請求的屬性)
- [叢集資料庫](#叢集資料庫)
- [外部資料庫](#外部資料庫)
- [HTTP](#http)
- [日誌與工具](#日誌與工具)
- [CoA 與 Disconnect](#coa-與-disconnect)
- [內建程式庫](#內建程式庫)
- [錯誤的形式](#錯誤的形式)
- [限制](#限制)
- [比對方法](#比對方法)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

## 腳本的執行環境

腳本是 Luau（Lua 5.1 的延伸）。每個請求有一個自己的全域表：本附錄列的名稱綁定在這個請求上，其他名稱查到的是唯讀的內建程式庫。

- 全域變數不會留到下一個請求。實測：腳本寫 `seen = true`，下一個請求讀 `seen` 還是 `nil`。要記住東西，寫進叢集資料庫。
- 內建程式庫是唯讀的。實測 `string.upper = nil` 得到 `attempt to modify a readonly table`。
- 腳本最後 `return` 的值決定結果：

| 回傳 | 意思 |
| --- | --- |
| `'accept'` | 接受 |
| `'reject'` | 拒絕 |
| `'ignore'` | 只在驗證鏈的 `lua` 處理器有意義：交給下一個處理器。最上層腳本回傳它等於拒絕 |
| `'challenge'` | 只在 RADIUS 登入（`protocol == 'radius'` 且 `phase == 'authentication'`）有效：送 Access-Challenge。其他地方等於拒絕 |
| 其他任何值、不回傳、出錯 | 拒絕 |

只有完全相同的小寫字串才算；`'ACCEPT'` 是拒絕。計費請求的「拒絕」是不回應，日誌寫 `Lua script did not accept accounting (radius, client localhost)`。

## 全域變數

全域變數在腳本開始前就設好，整個請求都不變。

| 變數 | 型別 | 什麼時候有值 |
| --- | --- | --- |
| `protocol` | 字串 | 一定有 |
| `phase` | 字串 | 一定有 |
| `client` | 字串 | 一定有 |
| `auth_user` | 字串或 `nil` | 驗證鏈裡的 `lua` 處理器 |
| `replayed` | 布林 | 一定有 |
| `eap_method` | 字串或 `nil` | EAP 登入 |
| `tls_subject` | 字串或 `nil` | EAP-TLS 憑證通過後 |
| `tls_subject_attrs` | 表或 `nil` | 同上 |

### protocol

請求的協定：`'radius'` 或 `'tacacs'`。

### phase

請求的階段（phase）：

| `protocol` | `phase` 的值 |
| --- | --- |
| `'radius'` | `'authentication'`（Access-Request）、`'accounting'`（Accounting-Request） |
| `'tacacs'` | `'authentication'`、`'authorization'`、`'accounting'` |

### client

請求來自哪個用戶端（client）：設定裡 `[[radius.clients]]` 或 `[[tacacs.clients]]` 的 `name`。quick-start 的用戶端叫 `localhost`，所以是 `'localhost'`。

### auth_user

驗證鏈正規化之後的帳號名稱，例如 `[auth.username]` 設了 `lowercase = true` 時，`Alice` 登入看到 `'alice'`，而 `get_attr('User-Name')` 仍是請求原本的 `'Alice'`（實測）。只有驗證鏈裡的 `lua` 處理器拿得到；最上層腳本一律是 `nil`，要用 `get_attr('User-Name')`。

### replayed

`true` 表示這個計費請求是從計費 spool（寫在磁碟上、等待重送的暫存）重送的；其他請求都是 `false`。重送時 `Acct-Delay-Time` 會加上在 spool 裡等的秒數。

### eap_method

驗證這個請求的 EAP 方法：

| 值 | 什麼時候 |
| --- | --- |
| `'tls'` | EAP-TLS 的憑證通過之後，問驗證鏈或最上層腳本這個名稱能不能登入 |
| `'peap'`、`'ttls'` | PEAP、TTLS 隧道裡的內層登入（只會經過驗證鏈） |

不是 EAP 的請求是 `nil`。

### tls_subject

EAP-TLS 憑證的 subject，字串形式，例如 `'CN=laptop-001'`。其他請求是 `nil`。

### tls_subject_attrs

同一張憑證的 subject 拆成表：每個屬性名稱對應一個陣列，因為名稱可以重複，例如 `{ CN = { 'laptop-001' } }`。值已經去掉跳脫字元。其他請求是 `nil`。

```lua
-- policy.lua（片段）
local ou = tls_subject_attrs and tls_subject_attrs.OU and tls_subject_attrs.OU[1]
```

## 請求的屬性

### get_attr

```text
get_attr(name) -> 值或 nil
```

- **參數：** `name`，屬性名稱（字串）。RADIUS 用字典裡的名稱，大小寫要一致；TACACS+ 的名稱見下面的表。
- **回傳：** 請求裡第一個叫這個名稱的屬性的值；沒有就是 `nil`。名稱不存在不會出錯，一樣是 `nil`。
- **階段：** 全部。

### get_attrs

```text
get_attrs(name) -> 陣列
```

- **參數：** 同 `get_attr`。
- **回傳：** 所有叫這個名稱的屬性的值，依封包裡的順序；沒有就是空表 `{}`。
- **階段：** 全部。

### set_attr

```text
set_attr(name, value)
```

- **參數：** `name`，屬性名稱；`value`，字串或數字。列舉可以寫字典裡的值名稱或數字；帶 tag 的屬性寫成 `名稱:tag`。
- **回傳：** 沒有。
- **效果：** 在回覆加一個屬性，依呼叫順序排列，同名呼叫兩次就有兩個。只有接受與 challenge 會送出；Accounting-Response 一律不帶屬性。
- **階段：** 全部；RADIUS 登入時名稱與值在呼叫當下就用字典檢查。TACACS+ 授權時變成回覆的 AV pair（`name=value`），名稱 `server_msg` 是特例，變成給使用者看的訊息。
- **錯誤：**

| 情況 | 錯誤訊息 |
| --- | --- |
| 值是布林、表或 `nil` | `attribute value must be a string or number, got boolean` |
| 字典沒有這個名稱 | `attribute 'No-Such-Attribute' has an unsupported number` |
| 列舉沒有這個值 | `'Nonsense' is not a value of 'Service-Type'` |

Access-Challenge 只能帶 `Reply-Message`、`State`（最多一個）、`Idle-Timeout`、`Session-Timeout` 與廠商屬性。其他屬性在送出前才檢查，不是 `set_attr` 出錯，而是整個 challenge 改成拒絕，日誌寫 `RADIUS Access-Challenge for client localhost not sent: 'Filter-Id' is not allowed in an Access-Challenge`。

TACACS+ 授權時，一個 AV pair（`name=value`）最多 255 個位元組：超過時 `set_attr` 當下就出錯（`AV pair 'name' is longer than 255 octets`），請求被拒絕。一個回覆最多 255 個 AV pair：超過時回 ERROR，日誌寫 `TACACS+ authorization for client … has … AV pairs, more than the 255 a reply holds`。

### RADIUS 屬性值的型別

| 字典型別 | `get_attr` 拿到 | `set_attr` 收 |
| --- | --- | --- |
| `integer`、`date`、`byte`、`short`、`signed`、`integer64` | 數字；列舉也是數字 | 數字，或列舉的值名稱 |
| `ipaddr`、`ipv6addr`、`ipv6prefix`、`ipv4prefix`、`ether`、`ifid` | 文字形式的字串，例如 `'192.0.2.1'` | 文字形式的字串 |
| `string`、`octets` 與其他 | 原始位元組的字串 | 字串 |

實測：`User-Name` 是 `'bob'`，`NAS-IP-Address` 是 `'192.0.2.1'`，`NAS-Port` 是數字 `7`。

`User-Password` 是解密後的明文，只有 PAP 請求有。

### TACACS+ 屬性的名稱

TACACS+ 沒有字典，名稱是封包欄位與 AV pair 的名稱：

| 名稱 | 型別 | 哪個階段 | 內容 |
| --- | --- | --- | --- |
| `user`、`port`、`rem_addr` | 字串 | 全部 | 使用者名稱、設備的埠、遠端位址 |
| `data` | 字串 | authentication | START 封包的 data 欄位 |
| `authen_type`、`priv_lvl`、`authen_service` | 數字 | 全部 | RFC 8907 的欄位值 |
| `authen_method` | 數字 | authorization、accounting | 使用者當初怎麼登入的 |
| AV pair 的名稱，例如 `service`、`cmd`、`cmd-arg`、`task_id` | 字串 | authorization、accounting | 每個 `key=value` 或 `key*value` 參數，以 key 為名稱 |

同名的 AV pair（例如好幾個 `cmd-arg`）用 `get_attrs` 拿全部。

## 叢集資料庫

`sql_query` 與 `sql_execute` 用的是叢集內建的資料庫（hiqlite，每個節點一份、用 Raft 複寫的 SQLite），用法與注意事項見第 10 章。

### sql_query

```text
sql_query(sql, params) -> 列的陣列
```

- **參數：** `sql`，一個 SQL 敘述；`params`，可省略，值的陣列，依序對上 `?`。
- **回傳：** 陣列，一列一個表，欄位名稱當鍵；沒有結果是空表。
- **效果：** 讀本機這份資料庫，唯讀。
- **階段：** 全部。
- **錯誤：** 見下面的[參數](#參數)與這張表：

| 情況 | 錯誤訊息 |
| --- | --- |
| 寫入 | `Sqlite: attempt to write a readonly database` |
| 語法錯誤 | `Sqlite: near "SELEC": syntax error in SELEC 1 at offset 0` |
| 沒有這個表 | `Sqlite: Error code 1: SQL logic error Some("no such table: nosuch")` |
| 這個請求的資料庫時間用完 | `sql_query timed out: the database calls of one request have 5000 ms together` |

### sql_execute

```text
sql_execute(sql, params) -> 數字
```

- **參數：** 同 `sql_query`。
- **回傳：** 影響的列數。實測 `INSERT` 一列回傳 `1`；`CREATE TABLE` 也可以執行。
- **效果：** 送到叢集的 leader，複寫到各節點之後才回來。
- **階段：** 全部。
- **錯誤：** 除了[參數](#參數)的錯誤，SQL 裡有每個節點會算出不同結果的函式或關鍵字時，送出前就拒絕：

| 情況 | 錯誤訊息 |
| --- | --- |
| 用了函式 `date`、`datetime`、`julianday`、`now`、`random`、`randomblob`、`strftime`、`time`、`timediff`、`unixepoch` | `` sql_execute: non-deterministic `strftime` is not allowed in writes; pass time values from now_unix() instead `` |
| 用了關鍵字 `current_timestamp`、`current_date`、`current_time` | 同上，反引號裡是 `` `current_timestamp` `` |
| 這個請求的資料庫時間用完 | `sql_execute timed out: the database calls of one request have 5000 ms together` |

### 參數

`sql_query`、`sql_execute`、`db_query`、`db_execute` 的參數規則相同。函式名稱會出現在訊息開頭：

| 情況 | 錯誤訊息（以 `sql_query` 為例） |
| --- | --- |
| `?` 與值的數目不同 | `sql_query: 1 placeholder, 0 values`、`sql_query: 0 placeholders, 1 value` |
| 用了 `?1`、`:name` 之類的寫法 | `sql_query: only plain ? placeholders are supported, found ?1` |
| 第二個參數不是表 | `sql_query: parameters must be an array, not string` |
| 陣列有洞（中間有 `nil`） | `sql_query: parameters must be an array without holes` |
| 值的型別不支援 | `sql_query: parameter 1 has unsupported type table` |
| 數字是無限大或 NaN | `sql_query: parameter 1 is not a finite number` |

值的對應：

| Lua 值 | 送給資料庫 |
| --- | --- |
| 字串 | TEXT；不是 UTF-8 時是 BLOB |
| 整數值的數字 | INTEGER |
| 其他數字 | REAL |
| `true`／`false` | `1`／`0` |

SQL 字串常值裡的 `?` 不算參數。實測 `sql_query("SELECT '?' AS q")` 不用傳參數，回傳一列，`q` 是 `'?'`。

### 結果的型別

| 資料庫的值 | Lua |
| --- | --- |
| INTEGER | 數字（整數） |
| REAL | 數字 |
| TEXT | 字串 |
| BLOB | 原始位元組的字串 |
| NULL | `nil`，所以那一列的表裡沒有這個鍵 |

## 外部資料庫

`db_query` 與 `db_execute` 用 `[databases.<name>]` 設定的 PostgreSQL 或 MySQL，設定見[第 9 章](09-external-databases.md)。

### db_query

```text
db_query(name, sql, params) -> 列的陣列
```

- **參數：** `name`，`[databases.<name>]` 的名稱；`sql`；`params`，可省略，規則見[參數](#參數)。
- **回傳：** 同 `sql_query`。
- **效果：** 用那個資料庫的連線池送出。SQL 一律寫 `?`；PostgreSQL 由 rradius 改寫成 `$1`、`$2`……，MySQL 原樣送出。
- **階段：** 全部。
- **錯誤：**

| 情況 | 錯誤訊息 |
| --- | --- |
| 沒有這個名稱 | `db_query: there is no database 'billing' in [databases]` |
| 連不上、連線池等不到連線 | `db_query: database billing: pool timed out while waiting for an open connection` 這類訊息 |
| 讀不了的欄位型別 | `db_query: database billing: column x has a type that cannot be read; CAST it to text in the SQL` |
| MySQL 的無號整數超過腳本數字的範圍 | `db_query: database billing: column x holds …, more than a script number can hold; CAST it to text in the SQL` |
| 這個請求的資料庫時間用完 | `db_query timed out: the database calls of one request have 5000 ms together` |

結果的型別：整數與浮點數是數字，布林是 `1`／`0`，文字是字串，二進位是位元組字串，NULL 是 `nil`；PostgreSQL 的 `NUMERIC` 與 MySQL 的 `DECIMAL`（`SUM`、`AVG` 的結果常是這種）是十進位的字串，不會被四捨五入。

實測（PostgreSQL 17、MySQL 8.4）：

| 查詢 | 結果 |
| --- | --- |
| PostgreSQL 的 `WHERE username = ? OR username = ?`，參數 `{'alice', 'bob'}` | 兩列（`?` 依序改寫成 `$1`、`$2`） |
| `SELECT '?' AS mark …` | `mark` 是 `'?'`，字串常值裡的 `?` 不是參數 |
| PostgreSQL `NUMERIC(8,2)` 與它的 `SUM` | `'12.50'`（字串） |
| PostgreSQL `BOOLEAN` 的 `true` | `1` |
| PostgreSQL `BIGINT` 的 `9007199254740993` | `9007199254740992`：超過 2^53 的整數失去精確度，要 `CAST` 成文字 |
| MySQL `DECIMAL(8,2)`、`TINYINT(1)` | `'12.50'`、`1` |
| PostgreSQL 與 MySQL 的 `DATE` | `db_query: database billing: column created has a type that cannot be read; CAST it to text in the SQL` |
| MySQL `BIGINT UNSIGNED` 的 `18446744073709551615` | `db_query: database legacy: column u_big holds 18446744073709551615, more than a script number can hold; CAST it to text in the SQL` |
| 沒有的欄位（PostgreSQL） | `db_query: database billing: error returned from database: column "nosuch" does not exist …` |

資料庫自己的錯誤訊息接在 `error returned from database:` 後面，原樣轉給腳本。

### db_execute

```text
db_execute(name, sql, params) -> 數字
```

- **參數：** 同 `db_query`。
- **回傳：** 影響的列數。
- **效果：** 外部資料庫沒有叢集的複寫問題，所以不檢查非確定性函式，`now()` 這類函式可以用。
- **階段：** 全部。
- **錯誤：** 同 `db_query`，訊息開頭是 `db_execute:`。實測：沒有權限時是 `db_execute: database legacy: error returned from database: 1142 (42000): UPDATE command denied to user …`（MySQL）或 `… permission denied for table radcheck …`（PostgreSQL）。

## HTTP

`http_request` 與 `url_encode` 只在設定有 `[lua.http]` 時存在；沒有時它們是 `nil`，呼叫會得到 `attempt to call a nil value`。設定見[附錄 A](90-config-reference.md)，用法見第 10 章。

### http_request

```text
http_request{ url = …, method = …, headers = …, body = … } -> 回應, nil
                                                           -> nil, 原因
```

- **參數：** 一個表：

| 鍵 | 必填 | 說明 |
| --- | --- | --- |
| `url` | 是 | 完整的網址，主機、埠與 http／https 要在 `lua.http.allowed_hosts` 裡 |
| `method` | | `'GET'`（預設）、`'POST'`、`'PUT'`、`'DELETE'`、`'PATCH'` |
| `headers` | | 表，標頭名稱對應字串值 |
| `body` | | 字串 |

- **回傳：** 成功時是一個表與 `nil`：`status`（數字）、`body`（字串）、`headers`（表，名稱是小寫，例如 `r.headers['content-type']`）。不會自動跟隨轉址：302 回傳的就是 302，`headers.location` 是轉址的目標。失敗時是 `nil` 與原因字串。
- **階段：** 全部。計費腳本等回應時讓出計費位置。
- **兩種失敗：** 選項本身寫錯是腳本的錯，直接出錯；網址不允許、連不上、逾時這類是執行時的狀況，以回傳值告訴腳本，腳本自己決定怎麼辦。

選項寫錯時的錯誤：

| 情況 | 錯誤訊息 |
| --- | --- |
| 參數不是表 | `http_request: takes a table of options` |
| 沒有 `url` | `http_request: url is required` |
| `url`、`method`、`body` 不是字串 | `http_request: url must be a string`（依鍵名） |
| 不支援的方法 | `http_request: method must be GET, POST, PUT, DELETE or PATCH` |
| `headers` 不是表 | `http_request: headers must be a table` |
| 標頭名稱或值不合法 | `http_request: a header name or value is not valid` |
| 設了 `Host`、`Content-Length`、`Transfer-Encoding`、`Connection`、`Upgrade`、`TE` | `http_request: the Host header cannot be set`（依名稱） |

回傳的失敗原因：

| 原因 | 什麼時候 |
| --- | --- |
| `http://127.0.0.1 is not in lua.http.allowed_hosts` | 主機、埠或 http／https 和白名單不完全相同 |
| `not an http or https URL` | 其他 scheme |
| `not a valid URL` | 網址解析不了 |
| `a URL with a user name is not allowed` | 網址帶帳號或密碼 |
| `a URL with . or .. in its path is not allowed` | 路徑有 `.` 或 `..`（包括編碼成 `%2e` 的） |
| `a URL with a tab or a line break is not allowed` | 網址有 tab 或換行 |
| `a URL with a space or a control character at either end is not allowed` | 網址頭尾有空白 |
| `too many requests at once` | 這台主機同時進行的呼叫已達 `lua.http.max_concurrent`；不排隊，直接失敗 |
| `timed out after 2s` | 超過 `lua.http.timeout_ms`（訊息寫實際的時限） |
| `the response is over 1048576 octets` | 回應內容超過 `lua.http.max_response_bytes` |
| `the connection was refused`、`the connection failed`、`the name lookup failed`、`the server's certificate was not accepted`、`the request failed` | 連線與 TLS 的失敗 |

原因裡只有主機，不含網址的其他部分，因為路徑與查詢字串常帶金鑰或使用者名稱。每次失敗都寫一行 WARN，例如 `script http_request refused: …`、`script http_request to 127.0.0.1: the connection was refused`；同時進行數用滿時是 `script http_request to 127.0.0.1: 64 already waiting`。結果記在指標 `rradius_script_http_requests_total` 的 `result` 標籤（`ok`、`timeout`、`error`、`denied`、`too_large`、`busy`）。

登入與計費的呼叫分開計算同時進行數，計費再多也不會占掉登入的名額。

### url_encode

```text
url_encode(text) -> 字串
```

- **參數：** `text`，字串。
- **回傳：** 除了英文字母、數字與 `-`、`.`、`_`、`~` 以外，每個位元組都編碼成 `%XX`，所以結果永遠只是網址的一段。實測 `url_encode("o'brien@corp é")` 是 `o%27brien%40corp%20%C3%A9`，`url_encode('a b/c')` 是 `a%20b%2Fc`。
- **錯誤：** 空字串、`.`、`..` 不管怎麼編碼，網址解析時都會被當成路徑的一步，所以出錯：`url_encode: '..' cannot be one piece of a URL`。

## 日誌與工具

### log

```text
log(...)
```

- **參數：** 任意個任意型別的值。
- **效果：** 寫一行 INFO 日誌：`script <檔名> (<protocol>/<phase>, client <client>): <訊息>`。參數轉成文字後用空格連起來；`nil`、布林、數字照 Lua 的寫法，表寫成 `table: 0x…`；換行變空格；超過 4096 個位元組的部分截掉。
- **階段：** 全部。

```text
… INFO rradius_core::lua_engine: script policy.lua (radius/authentication, client localhost): refused bob
```

### now_unix

```text
now_unix() -> 數字
```

現在的 Unix 時間，秒，整數值。寫進叢集資料庫的時間一律用它算好當參數傳。

### random_token

```text
random_token() -> 字串
```

16 個位元組的亂數，寫成 32 個小寫十六進位字元，例如給 Access-Challenge 當 `State`。

### totp_verify

```text
totp_verify(secret, code) -> 數字或 nil
```

- **參數：** `secret`，Base32 的 TOTP 密鑰（大小寫都可以，結尾的 `=` 可以省略）；`code`，使用者輸入的驗證碼，建議用字串，數字開頭的 `0` 會不見。
- **規則：** RFC 6238，SHA-1，6 位數，30 秒一步，現在的前後各容許一步。
- **回傳：** 對了回傳那一步的編號（Unix 時間除以 30 取整數）；錯了是 `nil`。記下用過的步數，同一個代碼就不能用第二次。
- **錯誤：** 密鑰不是 Base32 或是空的：`totp_verify: the secret is not valid Base32`。

### wpa_pmk

```text
wpa_pmk(passphrase, ssid) -> 字串
```

- **參數：** `passphrase`，8 到 63 個可列印 ASCII 字元的密碼，或 64 個十六進位字元的 PSK；`ssid`，1 到 32 個位元組。
- **回傳：** WPA2 的 PMK，64 個小寫十六進位字元。實測 `wpa_pmk('password', 'IEEE')` 是 `f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e`。
- **注意：** 每次呼叫要算 4096 輪 PBKDF2，會用掉幾毫秒的執行時間。
- **錯誤：**

| 情況 | 錯誤訊息 |
| --- | --- |
| 密碼長度或字元不對 | `wpa_pmk: the passphrase must be 8 to 63 printable ASCII characters, or the PSK 64 hex digits` |
| SSID 長度不對 | `wpa_pmk: the SSID must be 1 to 32 octets` |
| 不是 UTF-8 文字 | `wpa_pmk: the passphrase is not text`、`wpa_pmk: the SSID is not text` |

### password_verify

```text
password_verify(stored, given) -> 布林
```

- **參數：** `stored`，存放的密碼，格式與 sql 處理器的密碼欄位相同（`{crypt}`、`{clear}` 等前綴與不帶前綴的雜湊，見第 7 章）；`given`，使用者輸入的密碼。
- **回傳：** 相符是 `true`。任何一個是 `nil` 時是 `false`；格式不認得也是 `false`，不會出錯。
- **注意：** 不會累計失敗次數，`[auth.lockout]` 管不到。腳本自己比對密碼時，要自己限制猜測次數。

## CoA 與 Disconnect

核心有這兩個名稱，但呼叫一定出錯。

### send_disconnect

```text
send_disconnect(target, attributes)
```

- **參數：** `target`，字串；`attributes`，表，屬性名稱對應值或值的陣列。
- **核心的行為：** 呼叫一定出錯：`send_disconnect: not supported by this program (CoA needs rradius-enterprise)`。第二個參數不是表時，先得到參數轉換的錯誤，例如 `bad argument #2: error converting Lua string to table`。

### send_coa

```text
send_coa(target, attributes)
```

- **參數：** 同 `send_disconnect`。
- **核心的行為：** 呼叫一定出錯：`send_coa: not supported by this program (CoA needs rradius-enterprise)`。

> **企業版：** 這兩個函式送出 Disconnect-Request 與 CoA-Request，回傳 `'ack'`、`'nak'` 或 `'timeout'`。

## 內建程式庫

腳本看得到的 Luau 內建名稱（實測列出 `_G` 的全部鍵）：

```text
_G _VERSION assert bit32 buffer collectgarbage coroutine debug error gcinfo
getmetatable integer ipairs math newproxy next os pairs pcall print rawequal
rawget rawlen rawset select setmetatable string table tonumber tostring type
typeof unpack utf8 vector xpcall
```

拿掉的：

| 名稱 | 狀況 |
| --- | --- |
| `require`、`loadstring`、`getfenv`、`setfenv` | 不存在，是 `nil` |
| `io` | 不存在，腳本不能讀寫檔案 |
| `os.execute` 等 | 不存在；`os.time()`、`os.clock()` 可以用 |

- `print` 存在，但寫到程式的標準輸出，不是日誌，用 systemd 或容器跑時通常看不到。一律用 `log`。
- `_G` 是內建程式庫的表，不是請求的全域表。本附錄的名稱不在 `_G` 裡，`_G.get_attr` 是 `nil`。

## 錯誤的形式

函式出錯時，丟出的錯誤是字串，開頭是 `runtime error: `，後面接訊息，再接 `stack traceback:` 與呼叫的位置。腳本沒有接住時，請求被拒絕，日誌寫一行 ERROR：

```text
… ERROR rradius_core::lua_engine: Lua script error (radius/authentication): runtime error: probe.lua:40: something went wrong
stack traceback:
…
```

腳本可以用 `pcall` 接住錯誤，自己決定結果：

```lua
-- policy.lua（片段）
local ok, err = pcall(send_coa, 'localhost', { ['User-Name'] = 'alice' })
if not ok then
  log('coa failed', err)
end
```

有兩種情況，`pcall` 接住了也改變不了結果：

- 超過執行時間：之後每一步都再出同樣的錯，結果一定是失敗。
- 資料庫連不上或逾時：接住了，登入一樣不回應（日誌寫 `Discarded: database`），TACACS+ 回 ERROR，讓 NAS 改問別台伺服器。

## 限制

| 限制 | 設定 | 預設 | 超過時 |
| --- | --- | --- | --- |
| 腳本自己的執行時間 | `lua.max_execution_ms` | 1000 | `script exceeded the execution limit of 1000 ms` |
| 每個 VM 的記憶體 | `lua.max_memory_mb` | 256 | `memory error: not enough memory` |
| 一個請求所有資料庫呼叫的時間加總 | `lua.query_timeout_ms` | 5000 | `sql_query timed out: the database calls of one request have 5000 ms together` |

- 執行時間不含等資料庫、等 HTTP、等計費位置的時間。
- `lua.query_timeout_ms` 由 `sql_query`、`sql_execute`、`db_query`、`db_execute` 共用；`http_request` 有自己的 `lua.http.timeout_ms`。
- `lua.max_memory_mb` 改了要重啟，其他的 reload 就生效。
- VM 的數量等於程式能用的 CPU 數。計費腳本同時最多跑「VM 數減 2，至少 1」個，等資料庫與 HTTP 時讓出位置。

## 比對方法

本附錄列的名稱要和程式碼註冊的名稱一致。比對分兩步，在核心 repo 的根目錄執行。

第一步，從程式碼機械抽出名稱。`BINDER` 常數裡 `setmetatable({ … }, meta)` 的鍵，加上 `env.<名稱> =` 設定的名稱，就是腳本環境的全部名稱：

```console
$ python3 -I - rradius-core/src/lua_engine.rs <<'EOF' > /tmp/source.txt
import re, sys
src = open(sys.argv[1]).read()
binder = re.search(r'const BINDER: &str = r#"(.*?)"#;', src, re.S).group(1)
table = re.search(r'setmetatable\(\{(.*?)\}, meta\)', binder, re.S).group(1)
names = set(re.findall(r'^\s*([a-z_]+)\s*=', table, re.M))
names |= set(re.findall(r'env\.([a-z_]+)\s*=', binder))
print('\n'.join(sorted(names)))
EOF
$ wc -l < /tmp/source.txt
      27
```

27 個名稱裡，`radsec_subject` 與 `radsec_subject_attrs` 是 RadSec 連線的憑證資訊。核心沒有 RadSec，它們一律是 `nil`，所以本附錄不列，比對時明確排除：

```console
$ grep -v -x -e radsec_subject -e radsec_subject_attrs /tmp/source.txt > /tmp/expected.txt
$ grep -E '^### [a-z_]+$' docs/manual.zh-TW/93-lua-reference.md | cut -c5- | sort > /tmp/book.txt
$ diff /tmp/expected.txt /tmp/book.txt && echo same
same
```

本附錄的小標題只有 API 名稱是全小寫的英文，所以第二行的 `grep` 抓到的就是書中的清單。

第二步，在執行中確認。用書中的清單產生一個腳本，記下每個名稱的型別，放進 quick-start（`[lua]` 的 `script` 改成它，拿掉 `[auth]`，加上 `[lua.http]`），登入一次：

```console
$ awk '{printf "log(\"%s\", type(%s))\n", $1, $1} END {print "return \"accept\""}' /tmp/book.txt > names.lua
```

實測的日誌（節錄格式，共 25 行）：

```text
… script names.lua (radius/authentication, client localhost): auth_user nil
… script names.lua (radius/authentication, client localhost): client string
… script names.lua (radius/authentication, client localhost): db_execute function
…
… script names.lua (radius/authentication, client localhost): wpa_pmk function
```

17 個函式都是 `function`；`protocol`、`phase`、`client` 是 `string`，`replayed` 是 `boolean`；`auth_user`、`eap_method`、`tls_subject`、`tls_subject_attrs` 在最上層腳本的 PAP 登入是 `nil`，有值的情況見[全域變數](#全域變數)。

程式碼新增或拿掉名稱時，第一步的 `diff` 就會不一樣。

## 常見錯誤

**症狀：** 呼叫 `http_request` 或 `url_encode` 得到 `attempt to call a nil value`。
**原因：** 設定沒有 `[lua.http]`。
**解法：** 加上 `[lua.http]` 與 `allowed_hosts`。

**症狀：** 最上層腳本裡 `auth_user` 一直是 `nil`。
**原因：** 只有驗證鏈裡的 `lua` 處理器拿得到它。
**解法：** 最上層腳本用 `get_attr('User-Name')`。

**症狀：** `get_attr('Service-Type') == 'Framed-User'` 永遠不成立。
**原因：** 列舉拿到的是數字。
**解法：** 寫 `== 2`。`set_attr` 才收值名稱。

**症狀：** `totp_verify` 對使用者輸入的 `012345` 回傳 `nil`。
**原因：** 驗證碼被轉成數字，開頭的 `0` 不見了。
**解法：** 直接傳 `get_attr` 拿到的字串，不要 `tonumber`。

**症狀：** 日誌有 `sql_query: parameters must be an array, not string`。
**原因：** 第二個參數直接傳了值，例如 `sql_query(sql, user)`。
**解法：** 包成陣列：`sql_query(sql, {user})`。

**症狀：** 日誌有 `db_query: there is no database 'billing' in [databases]`。
**原因：** 名稱和 `[databases.<name>]` 不一致，或設定裡沒有這一節。
**解法：** 對照設定的名稱；改了 `[databases]` 要重啟。

**症狀：** `_G.get_attr` 是 `nil`。
**原因：** `_G` 是內建程式庫，不是請求的全域表。
**解法：** 直接寫 `get_attr`。

## 相關章節

- [03 第一次啟動與登入](03-quick-start.md)
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)
- [09 外部資料庫與具名查詢](09-external-databases.md)
- [10 用 Lua 寫政策](10-lua-policies.md)
- [12 TACACS+ 設備管理](12-tacacs.md)
- [附錄 A：設定參考](90-config-reference.md)
