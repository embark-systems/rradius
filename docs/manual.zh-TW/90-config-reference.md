# 附錄 A：設定參考

> 本章讓你做到：
> - 查到 `rradiusd` 每一個設定鍵的型別、預設值、是否必填與意思
> - 知道改了哪個鍵之後 reload 就生效，哪些要重新啟動
> - 知道省略整個區段時伺服器會怎麼做
> - 認出核心不支援、或在核心沒有作用的鍵與區段

前置條件：讀過 [04 設定檔的基本功](04-configuration.md)，知道設定檔、`include` 與 `check` 的用法。

本章小節：

- [怎麼讀這份參考](#怎麼讀這份參考)
- [頂層區段總覽](#頂層區段總覽)
- [頂層鍵：dictionary、include](#頂層鍵dictionaryinclude)
- [lua](#lua)
- [cluster](#cluster)
- [metrics](#metrics)
- [log](#log)
- [runtime](#runtime)
- [shutdown](#shutdown)
- [accounts](#accounts)
- [sessions](#sessions)
- [auth](#auth)
- [databases](#databases)
- [queries](#queries)
- [radius](#radius)
- [tacacs](#tacacs)
- [核心不支援或沒有作用的設定](#核心不支援或沒有作用的設定)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

---

## 怎麼讀這份參考

這份附錄的內容來自 `rradiusd schema --lang zh-TW`（JSON Schema）與 `rradiusd config show` 的輸出，再對照 `rradius-core/src/config.rs` 與 `reload.rs` 補上 schema 沒寫出來的預設值與條件。程式更新後，以你手上那一版的這兩個指令為準：

```console
$ rradiusd schema --lang zh-TW > schema.json
$ rradiusd config show --config rradius.toml
# computed on this machine: radius.sockets = 1, cluster.read_connections = 28
…
```

`config show` 會把沒寫的鍵填上預設值後整份印出，祕密顯示成 `<hidden>`。第一行的註解列出依這台機器算出來的值。

### 鍵名的寫法

表格第一欄是鍵的完整路徑：

| 寫法 | 意思 | 例子 |
| --- | --- | --- |
| `a.b` | 表 `[a]` 裡的鍵 `b` | `radius.auth_port` |
| `a[].b` | 表陣列 `[[a]]` 每一項裡的鍵 `b` | `radius.clients[].secret` |
| `a.<name>.b` | 名稱由你取的子表 `[a.<name>]` 裡的鍵 `b` | `databases.<name>.host` |
| `auth.handlers[ldap].b` | `type = "ldap"` 的那種 `[[auth.handlers]]` 裡的鍵 `b` | `auth.handlers[ldap].urls` |

### 欄位

- **型別：** 字串、整數、布林、IP 位址、陣列、表。「路徑」是字串，相對路徑一律以這個設定檔所在的目錄為準。
- **預設：** 不寫這個鍵時的值。「—」表示沒有預設值，不寫就是「沒有這個設定」，效果寫在說明裡。
- **必填：** 「是」表示這個區段出現時一定要寫；「條件」表示在說明寫出的條件下才必填；空白表示可省略。
- **reload：** 「可」表示 `SIGHUP` 重新載入（reload）就套用；「重啟」表示 reload 時不套用，日誌會寫 `these need a restart to apply: …` 列出這些鍵，要重新啟動才生效；「指令」表示只有管理指令會讀，每次執行指令時讀取，與 reload 無關。
- 整數的單位照鍵名：`_ms` 是毫秒、`_seconds` 是秒、`_bytes` 是位元組、`_mb` 是 MiB、`_kib` 是 KiB。

每個區段的標題下方寫出主章，詳細的用法與範例在那一章。

---

## 頂層區段總覽

| 區段 | 必要 | 省略時 | reload | 主章 |
| --- | --- | --- | --- | --- |
| `dictionary` | 是 | 不能省略 | 重啟 | [06](06-dictionary.md) |
| `include` | | 只讀取這一個檔案 | 可 | [04](04-configuration.md) |
| `[lua]` | 是 | `check` 失敗：`the [lua] section is required` | 可（`max_memory_mb` 要重啟） | [10](10-lua-policies.md) |
| `[cluster]` | 是 | `check` 失敗：`the [cluster] section is required` | 重啟 | [16](16-cluster.md) |
| `[metrics]` | | 不開 HTTP 埠：沒有 `/metrics`、`/livez`、`/readyz`，`status` 指令也讀不到資料 | 重啟 | [17](17-monitoring.md) |
| `[log]` | | 等級 `info`、格式 `text`、寫認證紀錄 | 重啟（`log.trace` 可） | [17](17-monitoring.md) |
| `[runtime]` | | 不綁 CPU | 重啟 | [19](19-performance.md) |
| `[shutdown]` | | 單節點不延遲、排空（drain）最多 5000 毫秒 | 可 | [04](04-configuration.md) |
| `[accounts]` | | `user` 指令無法使用 | 指令 | [07](07-users-and-chains.md) |
| `[sessions]` | | `session` 指令無法使用 | 指令 | [13](13-accounting.md) |
| `[auth]` | | 沒有驗證鏈，登入交給 `[lua]` 的腳本決定 | 可 | [07](07-users-and-chains.md) |
| `[databases.<name>]` | | 沒有外部資料庫 | 重啟 | [09](09-external-databases.md) |
| `[queries.<name>]` | | 沒有具名查詢 | 重啟 | [09](09-external-databases.md) |
| `[radius]` | 與 `[tacacs]` 至少一個 | 不提供 RADIUS | 加入或移除整個區段要重啟 | [05](05-radius-clients.md) |
| `[tacacs]` | 與 `[radius]` 至少一個 | 不提供 TACACS+ | 加入或移除整個區段要重啟 | [12](12-tacacs.md) |

`[radius]` 與 `[tacacs]` 都沒有時，`check` 失敗：`at least one of [radius] or [tacacs] must be configured`。

每個表都不接受未知的鍵。打錯字時 `check` 會指出行號與可用的鍵名，見[常見錯誤](#常見錯誤)。

### 最小的設定

只寫必填的鍵，其他全部用預設值，`check` 就會通過：

```toml
# rradius.toml
dictionary = "share/dictionary"      # 字典主檔，相對於這個檔案

[lua]
script = "auth.lua"                  # 檔案要存在，而且能編譯

[cluster]
data_dir = "data"
secret_raft_file = "raft.secret"     # openssl rand -hex 32 > raft.secret
secret_api_file = "api.secret"       # openssl rand -hex 32 > api.secret

[radius]

[[radius.clients]]
address = "127.0.0.1"
secret_file = "localhost.secret"     # openssl rand -hex 32 > localhost.secret
```

這份設定監聽所有位址的 1812 與 1813 埠、沒有 `[metrics]`（所以沒有 `/metrics` 與 `status`）、沒有 `[auth]`（登入完全由 `auth.lua` 決定）。`rradiusd config init --lang zh-TW` 會產生一份註解完整、密碼隨機的底稿，比從這裡開始更方便。

---

## 頂層鍵：dictionary、include

主章：`dictionary` 在 [06](06-dictionary.md)，`include` 在 [04](04-configuration.md)。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `dictionary` | 路徑 | — | 是 | 重啟 | RADIUS 字典的主檔，例如套件安裝的 `/usr/share/rradius/dictionary`。相對路徑以設定檔所在目錄為準。 |
| `include` | 陣列（字串） | — | | 可 | 跟這個檔案一起讀的其他設定檔，例如 `["conf.d/*.toml"]`，內容視同接在主檔後面。 |

`include` 的規則（`config.rs`）：

- 萬用字元 `*`、`?` 只能出現在最後一段檔名，例如 `conf.d/*.toml`；目錄名稱不能有萬用字元。
- 一個名稱展開後依路徑排序；萬用字元不配對以 `.` 開頭的檔名；同一個檔案只讀取一次；主檔自己會被略過。
- 被引入的檔案最多 1000 個，每個最多 1048576 位元組（1 MiB）。
- 被引入的檔案要以表（`[section]`）開頭，不能再寫 `include`。
- 主檔已經宣告的表，被引入的檔案不能再宣告一次，否則得到 `duplicate key`。

---

## lua

主章：[10 用 Lua 寫政策](10-lua-policies.md)。

### `[lua]`

每個請求都會執行的最上層腳本，與腳本可用的資源上限。這些上限也套用在驗證鏈裡的 `lua` 處理器。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `lua.script` | 路徑 | — | 是 | 可 | 每個請求都執行的最上層腳本。`check` 會編譯它。 |
| `lua.query_timeout_ms` | 整數 | `5000` | | 可 | 腳本在一個請求裡等資料庫的總時間，毫秒，要大於 0。 |
| `lua.max_execution_ms` | 整數 | `1000` | | 可 | 腳本在一個請求裡可以執行的時間，毫秒，要大於 0。 |
| `lua.max_memory_mb` | 整數 | `256` | | 重啟 | 每個 Lua VM 的記憶體上限，MiB，要大於 0。 |

### `[lua.http]`

有這個表，腳本才有 `http_request` 可以呼叫白名單上的 HTTP API。沒有這個表就沒有這個函式。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `lua.http.allowed_hosts` | 陣列（字串） | — | 是 | 可 | 腳本可以呼叫的主機，至少一個：`host`、`host:port`，或寫成 `http://host` 表示這台主機用明文 HTTP，其餘都走 HTTPS。 |
| `lua.http.timeout_ms` | 整數 | `2000` | | 可 | 一次呼叫從連線到讀完回應的時限，毫秒，要大於 0。 |
| `lua.http.max_response_bytes` | 整數 | `1048576` | | 可 | 接受的回應內容上限，位元組，要大於 0。 |
| `lua.http.max_concurrent` | 整數 | `64` | | 可 | 每台主機同時進行的呼叫數，要大於 0。 |
| `lua.http.ca_file` | 路徑 | — | | 可 | 系統根憑證之外另外信任的 CA 憑證（PEM）。 |

---

## cluster

主章：[16 叢集與高可用](16-cluster.md)。整個 `[cluster]` 改了都要重啟。

### `[cluster]`

內嵌資料庫（hiqlite，Raft 複寫）的節點設定。單機也一樣要有這一節，單機就是只有一個節點的叢集。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `cluster.data_dir` | 路徑 | — | 是 | 重啟 | 資料庫與 Raft 記錄的資料目錄。 |
| `cluster.node_id` | 整數 | `1` | | 重啟 | 這個節點的 id，至少 1，要出現在 `nodes` 裡。 |
| `cluster.secret_raft` | 字串 | — | 條件 | 重啟 | Raft 埠的共用密碼，至少 16 個字元。和 `secret_raft_file` 剛好設一個。 |
| `cluster.secret_raft_file` | 路徑 | — | 條件 | 重啟 | 存放 Raft 密碼的檔案。和 `secret_raft` 剛好設一個。 |
| `cluster.secret_api` | 字串 | — | 條件 | 重啟 | API 埠的共用密碼，至少 16 個字元。和 `secret_api_file` 剛好設一個。 |
| `cluster.secret_api_file` | 路徑 | — | 條件 | 重啟 | 存放 API 密碼的檔案。和 `secret_api` 剛好設一個。 |
| `cluster.listen` | IP 位址 | 沒有 `nodes` 時 `127.0.0.1`，有 `nodes` 時 `0.0.0.0` | | 重啟 | 叢集兩個埠監聽的位址。有 `nodes` 又沒設時會警告，見[常見錯誤](#常見錯誤)。 |
| `cluster.nodes` | 表陣列 | — | | 重啟 | 叢集的每個節點，包含這一台。欄位見下表。 |
| `cluster.migrations_dir` | 路徑 | — | | 重啟 | 啟動時依序套用的 SQL 檔目錄（migration）。 |
| `cluster.migrations_wait_ms` | 整數 | `60000` | | 重啟 | 啟動時等叢集就緒、套用 migration、追上進度的時限，三段各自計時，毫秒，要大於 0。 |
| `cluster.snapshot_every` | 整數 | `100000` | | 重啟 | 兩次 snapshot 之間的 Raft 記錄筆數，要大於 0。 |
| `cluster.read_connections` | 整數 | 工作執行緒數的兩倍，最多 32（依機器而定） | | 重啟 | 讀本機資料庫的連線數，1 到 128。工作執行緒數等於 CPU 數。 |
| `cluster.stop_without_leader_ms` | 整數 | —（一直回應） | | 重啟 | 沒有 leader 超過這段時間就不回應登入，毫秒，要大於 0。 |

### `[[cluster.nodes]]`

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `cluster.nodes[].id` | 整數 | — | 是 | 重啟 | 節點 id，至少 1。 |
| `cluster.nodes[].raft` | 字串 | — | 是 | 重啟 | 這個節點 Raft 埠的 `host:port`。 |
| `cluster.nodes[].api` | 字串 | — | 是 | 重啟 | 這個節點 API 埠的 `host:port`。 |

`raft` 與 `api` 是節點之間用的埠，不是管理介面。

---

## metrics

主章：[17 監控、狀態與日誌](17-monitoring.md)。

### `[metrics]`

寫了這個表（即使是空的）才會開 HTTP 埠，提供 `/metrics`、`/livez`、`/readyz`；`status` 指令也是讀這個埠。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `metrics.bind` | IP 位址 | `127.0.0.1` | | 重啟 | HTTP 埠監聽的位址。 |
| `metrics.port` | 整數 | `9812` | | 重啟 | HTTP 埠。 |

---

## log

主章：[17 監控、狀態與日誌](17-monitoring.md)。

### `[log]`

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `log.level` | 字串 | `info` | | 重啟 | 寫出的最低等級：`error`、`warn`、`info` 或 `debug`。 |
| `log.format` | 字串 | `text` | | 重啟 | 日誌格式：`text` 或 `json`。 |
| `log.auth` | 布林 | `true` | | 重啟 | 每次認證寫一筆認證紀錄。 |

### `[log.trace]`

在一段時間內把部分使用者或用戶端（client）的請求完整寫進日誌。它是暫時的：一定要寫 `minutes`，時間到就自己停。reload 就套用，所以可以在不重啟的情況下開始或結束追蹤。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `log.trace.users` | 陣列（字串） | `[]` | 條件 | 可 | 要追蹤的 `User-Name`，完全相同才算。`users` 與 `clients` 不能都是空的。 |
| `log.trace.clients` | 陣列（字串） | `[]` | 條件 | 可 | 要追蹤的 RADIUS 用戶端名稱，要是 `[[radius.clients]]` 裡的 `name`。 |
| `log.trace.minutes` | 整數 | — | 是 | 可 | 追蹤持續的時間，1 到 1440 分鐘。 |

---

## runtime

主章：[19 效能調校與壓測](19-performance.md)。

### `[runtime]`

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `runtime.pin_threads` | 布林 | `false` | | 重啟 | 把每個工作執行緒各綁在一顆 CPU 上；只在 Linux 與 FreeBSD 有作用。 |

---

## shutdown

主章：[04 設定檔的基本功](04-configuration.md)。

### `[shutdown]`

收到 `SIGTERM` 或 `SIGINT` 之後怎麼停。停止開始那一刻生效中的設定為準，所以 reload 就套用。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `shutdown.ready_delay_ms` | 整數 | 單節點 `0`，`cluster.nodes` 超過一個時 `5000` | | 可 | 收到停止訊號後，`/readyz` 回報停止中、但仍照常回答請求的時間，毫秒，最多 60000。讓負載平衡器有時間把流量移走。 |
| `shutdown.drain_ms` | 整數 | `5000` | | 可 | 停止接收新請求後，最多等處理中的請求多久，毫秒，最多 60000。 |

---

## accounts

主章：[07 使用者帳號與驗證鏈](07-users-and-chains.md)。

### `[accounts]`

`rradiusd user …` 指令執行的 SQL。`?1` 是帳號名稱，`?2` 是要存放的密碼值。沒寫某一句，對應的子指令就不能用。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `accounts.scheme` | 字串 | `bcrypt` | | 指令 | `user add` 與 `user passwd` 存密碼的方式：`bcrypt`、`sha512-crypt`、`nthash` 或 `clear`。MS-CHAP 與 PEAP 需要 `nthash`，CHAP 需要 `clear`。 |
| `accounts.add` | 字串 | — | | 指令 | `user add` 執行的 SQL；`?1` 帳號名稱，`?2` 密碼值。 |
| `accounts.passwd` | 字串 | — | | 指令 | `user passwd` 執行的 SQL；`?1` 帳號名稱，`?2` 密碼值。 |
| `accounts.enable` | 字串 | — | | 指令 | `user enable` 執行的 SQL；`?1` 帳號名稱。 |
| `accounts.disable` | 字串 | — | | 指令 | `user disable` 執行的 SQL；`?1` 帳號名稱。要搭配會略過停用帳號的 `user_query` 才有效果。 |
| `accounts.delete` | 字串 | — | | 指令 | `user delete` 執行的 SQL；`?1` 帳號名稱。 |
| `accounts.show` | 字串 | — | | 指令 | `user show` 執行並原樣印出的 SQL；`?1` 帳號名稱。不要選出密碼欄。 |

---

## sessions

主章：[13 計費與 session](13-accounting.md)。

### `[sessions]`

`rradiusd session …` 指令執行並原樣印出的 SQL。兩句都只能讀：開頭要是 `SELECT` 或 `WITH`。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `sessions.list` | 字串 | — | | 指令 | `session list` 的 SQL：目前在線的 session。沒有參數。 |
| `sessions.show` | 字串 | — | | 指令 | `session show` 的 SQL：某個使用者的 session；`?1` 是使用者名稱。 |

---

## auth

主章：[07 使用者帳號與驗證鏈](07-users-and-chains.md)；`ldap` 與 `netlogon` 處理器在 [08 接上 LDAP 與 Active Directory](08-ldap-and-ad.md)。整個 `[auth]` 都可以 reload。

有 `[auth]` 時，登入交給驗證鏈：`[[auth.handlers]]` 是預設的鏈，`[auth.chains.<name>]` 是具名的鏈，用戶端用 `auth` 指定。`[auth]` 至少要有一個處理器或一條具名的鏈，否則 `check` 失敗。

### `[auth]`

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `auth.policy` | 字串 | `first_match` | | 可 | 預設鏈怎麼合併處理器的結果：`first_match` 採用第一個有答案的處理器；`all_accept` 每個處理器都執行，任何一個拒絕就拒絕，且至少要有一個接受。 |
| `auth.handlers` | 表陣列 | — | 條件 | 可 | 預設鏈的處理器，依序嘗試。沒有具名的鏈時必填。欄位依 `type` 而不同，見下方各處理器的表。 |
| `auth.allow_mschapv1` | 布林 | `false` | | 可 | 驗證鏈要不要處理 MS-CHAPv1 登入。沒寫或 `false` 時，MS-CHAPv1 在任何處理器之前就被拒絕，也不計入 `auth.lockout`；PAP、CHAP、MS-CHAPv2 與 EAP 不受影響。 |

`auth.policy` 與 `[auth.username]` 只作用在預設鏈。沒有 `[[auth.handlers]]` 卻寫了它們，`check` 失敗，例如 `auth.username is set but the default chain has no [[auth.handlers]] entries`；具名的鏈要寫自己的 `policy` 與 `username`。

### `[auth.username]`

預設鏈把帳號名稱交給處理器之前的正規化。沒有這個表時，收到什麼名稱就用什麼名稱。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `auth.username.strip_realm` | 字串 | `none` | | 可 | 要去掉的 realm：`none`、`suffix`（最後一個 `@` 起的部分）、`prefix`（到最後一個反斜線為止的部分，例如 `REALM\`）或 `both`（先 prefix 再 suffix）。 |
| `auth.username.lowercase` | 布林 | `false` | | 可 | 帳號名稱轉成小寫。 |
| `auth.username.allowed` | 字串 | — | | 可 | 正規化後的名稱要整個符合的正規表示式，不符合就拒絕。 |

### `[auth.lockout]`

限制登入失敗。同一個鍵（帳號加上來源裝置）在 `window_seconds` 內失敗 `max_failures` 次，就在 `lock_seconds` 內直接拒絕、不驗密碼。沒有這個表就不計數。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `auth.lockout.max_failures` | 整數 | `10` | | 可 | 視窗內失敗幾次就鎖定這個鍵，要大於 0。 |
| `auth.lockout.window_seconds` | 整數 | `300` | | 可 | 從第一次失敗起算，計數持續多久，秒，要大於 0。 |
| `auth.lockout.lock_seconds` | 整數 | `300` | | 可 | 鎖定多久，秒，要大於 0。 |
| `auth.lockout.max_entries` | 整數 | `100000` | | 可 | 每個節點最多記住幾個鍵，要大於 0。滿了先丟過期的，再丟最舊的未鎖定的；鎖定中的不丟。 |
| `auth.lockout.account_max_failures` | 整數 | —（沒有帳號總上限） | | 可 | 同一個帳號不分來源的失敗總數，達到就對所有來源鎖定。要大於 `max_failures`。 |
| `auth.lockout.max_in_flight` | 整數 | `max_failures` 的 5 倍 | | 可 | 同一個鍵同時最多有幾個登入在等待判定，多的直接拒絕，要大於 0。窗口內已計的失敗也佔名額，所以鎖定前錯誤密碼最多檢查這麼多次（`max_failures` 較大時用它）。帳號總上限依 `account_max_failures` 按比例放大。 |

> **注意：** 設了 `account_max_failures`，猜密碼的人只要故意打錯，就能把別人的帳號對所有來源鎖住。

### `[auth.password_limits]`

儲存的密碼雜湊在一次比對裡最多可以要求多少計算成本。超過上限的雜湊不計算，直接當作密碼錯誤。每個鍵都有預設值，沒寫這個表時上限照樣生效。每個值都要大於 0。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `auth.password_limits.pbkdf2_iterations` | 整數 | `5000000` | | 可 | PBKDF2、X-PBKDF2 雜湊的迭代次數上限。 |
| `auth.password_limits.crypt_rounds` | 整數 | `1000000` | | 可 | `$5$`、`$6$`、`$sha1$` 與延伸 DES（`_` 開頭）crypt 雜湊的 rounds 上限。 |
| `auth.password_limits.bcrypt_cost` | 整數 | `15` | | 可 | bcrypt 雜湊的 cost 上限；每加 1，工作量加倍。 |
| `auth.password_limits.argon2_memory_kib` | 整數 | `262144`（256 MiB） | | 可 | argon2 雜湊的記憶體上限，KiB。同時進行的比對數等於 CPU 數，最壞情況約是這個值乘以 CPU 數。 |
| `auth.password_limits.argon2_iterations` | 整數 | `10` | | 可 | argon2 雜湊的迭代次數上限。 |
| `auth.password_limits.argon2_lanes` | 整數 | `16` | | 可 | argon2 雜湊的平行度上限。 |

### `[[auth.handlers]]`：sql 處理器

`type = "sql"`：在叢集資料庫或某個 `[databases.<name>]` 查帳號與密碼。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `auth.handlers[sql].type` | 字串 | — | 是 | 可 | 固定寫 `sql`。 |
| `auth.handlers[sql].user_query` | 字串 | — | 是 | 可 | 查使用者的 SQL，剛好一個 `?`（帳號名稱）；第一欄是存放的密碼。 |
| `auth.handlers[sql].reply_query` | 字串 | — | | 可 | 取得回覆屬性的 SQL，剛好一個 `?`，回傳 `(attribute, value)` 兩欄。 |
| `auth.handlers[sql].certificate_query` | 字串 | — | | 可 | EAP-TLS 憑證通過之後改用的查詢，查到就接受。沒設時用 `user_query`。 |
| `auth.handlers[sql].default_user` | 字串 | — | | 可 | 查無使用者時改用這個帳號的密碼驗證，知道這組密碼的人都會通過。 |
| `auth.handlers[sql].database` | 字串 | —（叢集資料庫） | | 可 | 在哪個 `[databases.<name>]` 執行查詢，佔位符一樣用 `?`。 |
| `auth.handlers[sql].timeout_ms` | 整數 | `5000` | | 可 | 一次查詢的時限，毫秒，要大於 0。 |

### `[[auth.handlers]]`：lua 處理器

`type = "lua"`：由另一個 Lua 腳本決定。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `auth.handlers[lua].type` | 字串 | — | 是 | 可 | 固定寫 `lua`。 |
| `auth.handlers[lua].script` | 路徑 | — | 是 | 可 | 這個處理器的腳本檔。 |
| `auth.handlers[lua].verifies_password` | 布林 | `false` | | 可 | 腳本自己驗證密碼。設成 `true` 時，鏈可以只有這個處理器，它的接受在 EAP 內也算證明了密碼。 |

### `[[auth.handlers]]`：ldap 處理器

`type = "ldap"`：在 LDAP 目錄找使用者並驗證密碼。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `auth.handlers[ldap].type` | 字串 | — | 是 | 可 | 固定寫 `ldap`。 |
| `auth.handlers[ldap].urls` | 陣列（字串） | — | 是 | 可 | 伺服器位址（`ldap://` 或 `ldaps://`），至少一個，依序嘗試。 |
| `auth.handlers[ldap].bind_dn` | 字串 | — | 是 | 可 | 搜尋使用者時綁定的 DN。 |
| `auth.handlers[ldap].bind_password` | 字串 | — | 條件 | 可 | `bind_dn` 的密碼。和 `bind_password_file` 剛好設一個。 |
| `auth.handlers[ldap].bind_password_file` | 路徑 | — | 條件 | 可 | 存放 `bind_dn` 密碼的檔案。和 `bind_password` 剛好設一個。 |
| `auth.handlers[ldap].base_dn` | 字串 | — | 是 | 可 | 搜尋使用者的起點。 |
| `auth.handlers[ldap].filter` | 字串 | — | 是 | 可 | 找使用者的搜尋條件；`{user}` 會換成跳脫過的帳號名稱。 |
| `auth.handlers[ldap].certificate_filter` | 字串 | — | | 可 | EAP-TLS 憑證通過之後改用的搜尋條件，找到就接受。 |
| `auth.handlers[ldap].password_check` | 字串 | `bind` | | 可 | `bind` 以使用者身分綁定來驗證密碼；`attribute` 拿 `password_attribute` 的值來比對。 |
| `auth.handlers[ldap].password_attribute` | 字串 | — | 條件 | 可 | `password_check = "attribute"` 時存放密碼的屬性。 |
| `auth.handlers[ldap].reply_attributes` | 表（字串對字串） | — | | 可 | 鍵是回覆屬性名稱，值是提供屬性值的使用者 LDAP 屬性。依寫的順序加入。 |
| `auth.handlers[ldap].ca_file` | 路徑 | — | | 可 | `ldaps://` 與 StartTLS 信任的 CA 憑證。 |
| `auth.handlers[ldap].allow_plaintext` | 布林 | `false` | | 可 | 允許不用 StartTLS 的 `ldap://`。密碼會以明文傳送。 |
| `auth.handlers[ldap].pool_size` | 整數 | `4` | | 可 | 保持開啟的伺服器連線數，要大於 0。 |
| `auth.handlers[ldap].timeout_ms` | 整數 | `5000` | | 可 | 一次 LDAP 查詢的時限，毫秒，要大於 0。 |
| `auth.handlers[ldap].required_groups` | 陣列（字串） | —（不要求群組） | | 可 | 使用者要屬於其中一個群組才接受。寫了就至少要有一個。 |
| `auth.handlers[ldap].group_attribute` | 字串 | — | | 可 | 列出所屬群組的使用者屬性，例如 `memberOf`。 |
| `auth.handlers[ldap].group_base_dn` | 字串 | — | | 可 | 用 `group_filter` 時搜尋群組的起點。 |
| `auth.handlers[ldap].group_filter` | 字串 | — | | 可 | 找使用者所屬群組的搜尋條件；`{dn}` 是使用者的 DN。 |
| `auth.handlers[ldap].group_reply` | 表陣列 | — | | 可 | 屬於某群組時加上的回覆屬性。欄位見下面兩列。 |
| `auth.handlers[ldap].group_reply[].group` | 字串 | — | 是 | 可 | 加上這些屬性的群組。 |
| `auth.handlers[ldap].group_reply[].attributes` | 表（字串對字串） | — | | 可 | 要加上的回覆屬性，以屬性名稱為鍵。至少一個。 |

### `[[auth.handlers]]`：netlogon 處理器

`type = "netlogon"`：透過 Active Directory 網域控制站的 Netlogon 服務驗證登入。支援哪些登入方式見 [08](08-ldap-and-ad.md)。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `auth.handlers[netlogon].type` | 字串 | — | 是 | 可 | 固定寫 `netlogon`。 |
| `auth.handlers[netlogon].domain` | 字串 | — | 是 | 可 | 網域的 NetBIOS 名稱。 |
| `auth.handlers[netlogon].servers` | 陣列（字串） | — | 是 | 可 | 網域控制站的 DNS 或 NetBIOS 名稱，至少一個，依序嘗試。不能用 IP 位址：登入請求要帶 DC 的名稱。 |
| `auth.handlers[netlogon].port` | 整數 | —（先問 135 埠的端點對應器） | | 可 | 網域控制站固定的 Netlogon 埠，要大於 0。 |
| `auth.handlers[netlogon].computer_name` | 字串 | — | 是 | 可 | 本節點在網域中的電腦帳號，不含結尾的 `$`，最多 15 個字元。每個節點用自己的帳號：共用帳號的節點會互相弄斷 secure channel。 |
| `auth.handlers[netlogon].machine_password_file` | 路徑 | — | 是 | 可 | 存放電腦帳號密碼的檔案；結尾的換行會去掉。 |
| `auth.handlers[netlogon].connections` | 整數 | `4` | | 可 | 每台網域控制站最多保持開啟的連線數，要大於 0。 |
| `auth.handlers[netlogon].timeout_ms` | 整數 | `2000` | | 可 | 一次網域控制站呼叫的時限，毫秒，要大於 0。 |
| `auth.handlers[netlogon].required_groups` | 陣列（字串） | —（不要求群組） | | 可 | 使用者要屬於其中一個群組才接受，群組以 SID（`S-1-5-21-…`）表示。寫了就至少要有一個。 |
| `auth.handlers[netlogon].group_reply` | 表陣列 | — | | 可 | 屬於某群組時加上的回覆屬性。 |
| `auth.handlers[netlogon].group_reply[].group` | 字串 | — | 是 | 可 | 群組的 SID。 |
| `auth.handlers[netlogon].group_reply[].attributes` | 表（字串對字串） | — | | 可 | 要加上的回覆屬性，以屬性名稱為鍵。至少一個。 |

### `[auth.chains.<name>]`

具名的驗證鏈。`<name>` 由你取，用戶端以 `auth = "<name>"` 選用。欄位和 `[auth]` 的同名鍵相同；每條鏈至少要有一個處理器。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `auth.chains.<name>.policy` | 字串 | `first_match` | | 可 | 同 `auth.policy`。 |
| `auth.chains.<name>.handlers` | 表陣列 | — | 是 | 可 | 這條鏈的處理器。四種處理器與它們的每個鍵（含預設值）都和 `auth.handlers[sql]`、`auth.handlers[lua]`、`auth.handlers[ldap]`、`auth.handlers[netlogon]` 完全相同。 |
| `auth.chains.<name>.username` | 表 | — | | 可 | 這條鏈自己的名稱正規化，鍵見下面三列。沒有時不正規化，不會沿用 `[auth.username]`。 |
| `auth.chains.<name>.username.strip_realm` | 字串 | `none` | | 可 | 同 `auth.username.strip_realm`。 |
| `auth.chains.<name>.username.lowercase` | 布林 | `false` | | 可 | 同 `auth.username.lowercase`。 |
| `auth.chains.<name>.username.allowed` | 字串 | — | | 可 | 同 `auth.username.allowed`。 |

---

## databases

主章：[09 外部資料庫與具名查詢](09-external-databases.md)。整個 `[databases]` 改了（包括新增一個）都要重啟。

### `[databases.<name>]`

外部的 MySQL 或 PostgreSQL。`<name>` 由你取，sql 處理器的 `database`、Lua 腳本以這個名稱使用它。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `databases.<name>.kind` | 字串 | — | 是 | 重啟 | `mysql` 或 `postgresql`。 |
| `databases.<name>.host` | 字串 | — | 是 | 重啟 | 資料庫伺服器的主機名稱或位址；`tls = "verify"` 時，憑證上必須是這個名稱。 |
| `databases.<name>.port` | 整數 | mysql 為 `3306`，postgresql 為 `5432` | | 重啟 | 資料庫伺服器的埠。 |
| `databases.<name>.database` | 字串 | — | 是 | 重啟 | 要使用的資料庫名稱。 |
| `databases.<name>.user` | 字串 | — | 是 | 重啟 | 登入資料庫用的帳號。 |
| `databases.<name>.password` | 字串 | — | | 重啟 | `user` 的密碼；和 `password_file` 最多設一個。MySQL 搭配 `tls = "off"` 時，帳號要用 `mysql_native_password`。 |
| `databases.<name>.password_file` | 路徑 | — | | 重啟 | 存放密碼的檔案；和 `password` 最多設一個。 |
| `databases.<name>.tls` | 字串 | `verify` | | 重啟 | `verify`：加密並驗證憑證鏈與主機名稱；`off`：不加密，`host` 不是 loopback 位址時會記警告。 |
| `databases.<name>.ca_file` | 路徑 | —（系統根憑證） | | 重啟 | `tls = "verify"` 信任的 CA 憑證（PEM）。 |
| `databases.<name>.pool_size` | 整數 | `8` | | 重啟 | 對這個資料庫最多保持的連線數，至少 1。 |
| `databases.<name>.connect_timeout_ms` | 整數 | `5000` | | 重啟 | 建立一條連線的時限，毫秒，要大於 0。 |
| `databases.<name>.required_for_ready` | 布林 | `false` | | 重啟 | 這個資料庫連不上時 `/readyz` 是否回 503。每 5 秒用一條專用連線執行 `SELECT 1`；連續兩次失敗算連不上，一次成功就恢復。 |

---

## queries

主章：[09 外部資料庫與具名查詢](09-external-databases.md)。整個 `[queries]` 改了都要重啟。

### `[queries.<name>]`

每個節點在背景定期執行的唯讀 SQL，結果成為指標 `rradius_query_<name>`（gauge）的樣本；`rradiusd query <name>` 也執行它。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `queries.<name>.sql` | 字串 | — | 是 | 重啟 | 要執行的 SQL。只能讀（開頭是 `SELECT` 或 `WITH`），沒有參數。 |
| `queries.<name>.value` | 字串 | `value` | | 重啟 | 存放樣本數值的欄位，整數或實數；這個欄位是 NULL 的列會略過。 |
| `queries.<name>.labels` | 陣列（字串） | `[]` | | 重啟 | 值當作樣本標籤的欄位，標籤名稱就是欄位名稱。不能是數值欄位，不能重複。 |
| `queries.<name>.help` | 字串 | 空字串（輸出時改用一段寫著查詢名稱的文字） | | 重啟 | 指標的 HELP 說明。 |
| `queries.<name>.interval_ms` | 整數 | `30000` | | 重啟 | 一次執行開始到下一次開始的間隔，毫秒，至少 5000。 |
| `queries.<name>.timeout_ms` | 整數 | `5000` | | 重啟 | 一次執行最久多久，毫秒，1 到 `interval_ms`。 |
| `queries.<name>.max_rows` | 整數 | `200` | | 重啟 | 一次執行最多有幾列成為樣本，1 到 10000；多的丟掉並計數。 |

---

## radius

主章：[05 RADIUS 服務與 NAS 用戶端](05-radius-clients.md)。`sockets`、`recv_buffer_bytes`、`max_*_inflight` 的調校在 [19](19-performance.md)；計費 spool 與計費記錄檔在 [13](13-accounting.md)；`[radius.eap]` 在 [11](11-eap.md)。

### `[radius]`

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `radius.bind` | IP 位址，或 IP 位址的陣列 | `0.0.0.0` | | 重啟 | RADIUS 埠監聽的位址，可以一個或一串；寫成陣列時至少一個。每個位址都開認證與計費兩個埠。 |
| `radius.auth_port` | 整數 | `1812` | | 重啟 | 認證埠。 |
| `radius.acct_port` | 整數 | `1813` | | 重啟 | 計費埠。 |
| `radius.sockets` | 整數 | Linux 上等於工作執行緒數，其他平台 `1`（依機器而定） | | 重啟 | 每個位址與埠的接收 socket 數，要大於 0。只有 Linux 能把封包分給多個 socket，其他平台只能是 1。 |
| `radius.recv_buffer_bytes` | 整數 | —（系統預設） | | 重啟 | 每個 RADIUS socket 的 `SO_RCVBUF`，位元組，4096 到 1073741824。 |
| `radius.max_auth_inflight` | 整數 | `4096` | | 重啟 | 同時處理的認證請求數，超過的丟棄。 |
| `radius.max_acct_inflight` | 整數 | `4096` | | 重啟 | 同時處理的計費請求數，超過的丟棄。 |
| `radius.status_server` | 布林 | `true` | | 重啟 | 回應 Status-Server 請求（RFC 5997）。 |
| `radius.require_message_authenticator` | 字串 | `yes` | | 可 | Access-Request 是否一定要有 Message-Authenticator：`yes`、`no`，或 `auto`（由啟動後這個用戶端的第一個 Access-Request 決定）。用戶端可以各自覆寫。 |
| `radius.limit_proxy_state` | 字串 | `yes` | | 可 | 丟棄帶 Proxy-State 但沒有 Message-Authenticator 的 Access-Request：`yes`、`no` 或 `auto`。用戶端可以各自覆寫。 |
| `radius.clients` | 表陣列 | — | | 可 | 允許送請求的 RADIUS 用戶端（NAS）。欄位見下一節。 |
| `radius.coa_timeout_ms` | 整數 | `1000` | | 可 | **核心沒有作用**，見[核心不支援或沒有作用的設定](#核心不支援或沒有作用的設定)。 |
| `radius.coa_attempts` | 整數 | `3` | | 可 | **核心沒有作用**，同上。 |

### `[[radius.clients]]`

允許送請求的 NAS。一個項目可以是一個位址或一個網段。改了用戶端（含共用密碼）reload 就套用。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `radius.clients[].address` | 字串 | — | 是 | 可 | 用戶端的 IP 位址或 CIDR 網段，例如 `10.0.0.0/24`。 |
| `radius.clients[].name` | 字串 | `address` 的值 | | 可 | 日誌與指標用的名稱；`log.trace.clients` 也用它。 |
| `radius.clients[].secret` | 字串 | — | 條件 | 可 | 共用密碼。和 `secret_file` 剛好設一個。 |
| `radius.clients[].secret_file` | 路徑 | — | 條件 | 可 | 存放共用密碼的檔案。和 `secret` 剛好設一個。 |
| `radius.clients[].auth` | 字串 | —（預設鏈） | | 可 | 這個用戶端的登入改用哪條 `[auth.chains.<name>]`。 |
| `radius.clients[].require_message_authenticator` | 字串 | `radius.require_message_authenticator` 的值 | | 可 | 覆寫 `[radius]` 的設定：`yes`、`no` 或 `auto`。 |
| `radius.clients[].limit_proxy_state` | 字串 | `radius.limit_proxy_state` 的值 | | 可 | 覆寫 `[radius]` 的設定：`yes`、`no` 或 `auto`。 |
| `radius.clients[].eap_methods` | 陣列（字串） | `radius.eap.methods` | 條件 | 可 | 提供給這個用戶端的 EAP 方法，依序：`tls`、`peap`、`ttls`。需要 `[radius.eap]`。`radius.eap.methods` 有兩個以上的方法時，每個用戶端都要寫，不用 EAP 的用戶端寫 `[]`。 |
| `radius.clients[].authorize_group` | 字串 | — | | 可 | **只用於 TACACS+**，寫在 RADIUS 用戶端時 `check` 失敗。 |
| `radius.clients[].tls_name` | 字串 | — | | 可 | **核心沒有作用**。 |
| `radius.clients[].coa_port` | 整數 | `3799` | | 可 | **核心沒有作用**。 |
| `radius.clients[].coa_sender` | 布林 | `false` | | — | **核心不支援**，寫了 `check` 失敗。 |
| `radius.clients[].max_radsec_connections` | 整數 | — | | — | **核心不支援**，寫了 `check` 失敗。 |
| `radius.clients[].mfa` | 字串 | — | | — | **核心不支援**，寫了 `check` 失敗。 |

最後五個鍵的實際行為與錯誤訊息見[核心不支援或沒有作用的設定](#核心不支援或沒有作用的設定)。

### `[radius.accounting_spool]`

資料庫無法寫入時，先把計費請求寫進磁碟上的計費 spool（等待重送的暫存），之後再依序重送（重送）。沒有這個表時，叢集資料庫短暫無法使用的期間只能靠 NAS 自己重送。整個表改了要重啟。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `radius.accounting_spool.dir` | 路徑 | — | 是 | 重啟 | 暫存目錄。 |
| `radius.accounting_spool.max_bytes` | 整數 | `1073741824`（1 GiB） | | 重啟 | 暫存大小上限，位元組，要大於 0。 |
| `radius.accounting_spool.replay_interval_ms` | 整數 | `5000` | | 重啟 | 重送失敗後，下一次重送前的等待，毫秒，要大於 0。 |
| `radius.accounting_spool.replay_window` | 整數 | `1` | | 重啟 | 同時重送的筆數，1 到 1024。預設 1，嚴格依序重送，因為腳本可能預期同一個 session 的紀錄依序到達。 |

### `[radius.accounting_log]`

每回覆一筆計費請求，就在本機檔案附加一行 JSON。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `radius.accounting_log.file` | 路徑 | — | 是 | 可 | 附加寫入的檔案；所在的目錄要已經存在，檔案已存在時要能附加寫入。收到 `SIGHUP` 時重新開檔，所以可以配合日誌輪替，也可以 reload 改成別的檔名。 |
| `radius.accounting_log.queue` | 整數 | `65536` | | 重啟 | 等著寫入的筆數上限，1 到 16777216；超過的丟棄並計數。 |

加入或移除整個 `[radius.accounting_log]` 要重啟。

### `[radius.eap]`

802.1X 用的 EAP：伺服器憑證、提供的方法與未完成對話的上限。整個表 reload 就套用。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `radius.eap.methods` | 陣列（字串） | `["tls"]` | | 可 | 提供的 EAP 方法，第一個是開頭提議的方法：`tls`、`peap`、`ttls`，至少一個。有 `peap` 或 `ttls` 時要有 `[auth]`。 |
| `radius.eap.cert_file` | 路徑 | — | 是 | 可 | 伺服器憑證鏈（PEM）。 |
| `radius.eap.key_file` | 路徑 | — | 是 | 可 | 伺服器憑證的私鑰（PEM）。 |
| `radius.eap.client_ca_file` | 路徑 | — | 條件 | 可 | 簽發用戶端憑證的 CA。`methods` 有 `tls` 時必填。 |
| `radius.eap.crl_file` | 路徑 | — | | 可 | 用戶端憑證的 CRL（撤銷清單）；只用於 `tls`。 |
| `radius.eap.match_identity` | 布林 | `true` | | 可 | EAP-TLS：憑證要對應 EAP 身分裡的使用者。 |
| `radius.eap.fragment_size` | 整數 | `1000` | | 可 | EAP 分段的上限，256 到 1400 位元組。 |
| `radius.eap.session_timeout_ms` | 整數 | `30000` | | 可 | 未完成的 EAP 對話保留多久，毫秒，要大於 0。 |
| `radius.eap.max_sessions` | 整數 | `4096` | | 可 | 全部保留的未完成 EAP 對話數。 |
| `radius.eap.max_sessions_per_client` | 整數 | `1024` | | 可 | 每個用戶端保留的未完成 EAP 對話數。 |
| `radius.eap.max_sessions_per_device` | 整數 | `4` | | 可 | 每台裝置（`Calling-Station-Id`）保留的未完成 EAP 對話數。 |
| `radius.eap.resumption_ms` | 整數 | — | | — | **核心不支援**，寫了 `check` 失敗。 |
| `radius.eap.shared_tickets` | 布林 | — | | — | **核心不支援**，寫了 `check` 失敗。 |
| `radius.eap.ticket_seal_file` | 路徑 | — | | — | **核心不支援**，寫了 `check` 失敗。 |
| `radius.eap.previous_ticket_seal_file` | 路徑 | — | | — | **核心不支援**，寫了 `check` 失敗。 |

---

## tacacs

主章：[12 TACACS+ 設備管理](12-tacacs.md)。

### `[tacacs]`

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `tacacs.bind` | IP 位址 | `0.0.0.0` | | 重啟 | TACACS+ 埠監聽的位址。 |
| `tacacs.port` | 整數 | `49` | | 重啟 | TACACS+ 埠（TCP）。 |
| `tacacs.max_connections` | 整數 | `1024` | | 重啟 | 同時開啟的連線數，超過的立刻關閉。 |
| `tacacs.max_connections_per_source` | 整數 | `64` | | 重啟 | 同一個來源位址同時開啟的連線數，超過的立刻關閉。 |
| `tacacs.max_body_len` | 整數 | `65536` | | 重啟 | 封包內容上限，位元組；超過就關閉連線。必須大於 0。 |
| `tacacs.idle_timeout_ms` | 整數 | `300000` | | 重啟 | 這段時間內沒收到完整封包就關閉連線，毫秒。 |
| `tacacs.enable` | 字串 | `deny` | | 可 | enable 怎麼驗證：`deny` 一律拒絕、不詢問密碼；`login` 當作一次登入，由驗證鏈或腳本決定。 |
| `tacacs.authorize_groups` | 表 | `{}` | | 可 | 授權群組：名稱對應到一串 `permit`／`deny` 規則，用來授權指令和服務。見下一節。 |
| `tacacs.clients` | 表陣列 | — | | 可 | 允許連線的 TACACS+ 用戶端（設備）。欄位見下面。 |

### `[tacacs.authorize_groups]`

每個鍵是一個授權群組的名稱，值是規則字串的陣列，例如：

```toml
# conf.d/tacacs.toml
[tacacs.authorize_groups]
ops = ["permit service=shell"]

[[tacacs.clients]]
address = "10.0.0.0/24"
secret_file = "tacacs.secret"   # openssl rand -hex 32 > tacacs.secret
authorize_group = "ops"
```

TACACS+ 用戶端用 `authorize_group = "ops"` 選用。有 `[tacacs]` 就至少要有一個 `[[tacacs.clients]]`，否則 `check` 失敗：`[tacacs] is enabled but has no [[tacacs.clients]] entries`。寫了不存在的群組名稱，`check` 失敗。規則的語法在 [12](12-tacacs.md)。

### `[[tacacs.clients]]`

TACACS+ 用戶端的欄位與 `[[radius.clients]]` 共用同一份定義，所以 schema 列出相同的 14 個鍵；但在 TACACS+ 用戶端，只有前六個有用，其他寫了都會讓 `check` 失敗。

| 鍵 | 型別 | 預設 | 必填 | reload | 說明 |
| --- | --- | --- | --- | --- | --- |
| `tacacs.clients[].address` | 字串 | — | 是 | 可 | 設備的 IP 位址或 CIDR 網段。 |
| `tacacs.clients[].name` | 字串 | `address` 的值 | | 可 | 日誌與指標用的名稱。 |
| `tacacs.clients[].secret` | 字串 | — | 條件 | 可 | 共用密碼。和 `secret_file` 剛好設一個。 |
| `tacacs.clients[].secret_file` | 路徑 | — | 條件 | 可 | 存放共用密碼的檔案。和 `secret` 剛好設一個。 |
| `tacacs.clients[].auth` | 字串 | —（預設鏈） | | 可 | 這台設備的登入改用哪條 `[auth.chains.<name>]`。 |
| `tacacs.clients[].authorize_group` | 字串 | — | | 可 | 用 `tacacs.authorize_groups` 裡哪個群組的規則授權這台設備。 |
| `tacacs.clients[].require_message_authenticator` | 字串 | — | | — | **只用於 RADIUS**：`check` 失敗。 |
| `tacacs.clients[].limit_proxy_state` | 字串 | — | | — | **只用於 RADIUS**：`check` 失敗。 |
| `tacacs.clients[].eap_methods` | 陣列（字串） | — | | — | **只用於 RADIUS**：`check` 失敗。 |
| `tacacs.clients[].coa_port` | 整數 | — | | — | **只用於 RADIUS**：`check` 失敗。 |
| `tacacs.clients[].coa_sender` | 布林 | — | | — | **只用於 RADIUS**：`check` 失敗。 |
| `tacacs.clients[].max_radsec_connections` | 整數 | — | | — | **只用於 RADIUS**：`check` 失敗。 |
| `tacacs.clients[].tls_name` | 字串 | — | | — | **核心不支援**：`check` 失敗。 |
| `tacacs.clients[].mfa` | 字串 | — | | — | **核心不支援**：`check` 失敗。 |

---

## 核心不支援或沒有作用的設定

`rradiusd` 與企業版共用同一個設定的讀取程式，所以有些鍵出現在核心的 `schema` 裡，但在核心不是被拒絕，就是沒有作用。下面是以 `rradiusd check` 逐一實測的結果。

### 寫了會被拒絕的鍵

`check` 結束碼 2，訊息如下（`<file>` 是設定檔路徑，`<name>` 是用戶端名稱）：

| 鍵 | `check` 的訊息 |
| --- | --- |
| `radius.clients[].coa_sender` | `<file>: radius client '<name>': coa_sender needs RadSec, which needs rradius-enterprise` |
| `radius.clients[].max_radsec_connections` | `<file>: radius client '<name>': max_radsec_connections applies only to RadSec, which needs rradius-enterprise` |
| `radius.clients[].mfa` | `<file>: radius client '<name>': mfa needs rradius-enterprise` |
| `radius.clients[].authorize_group` | `<file>: radius client '<name>': authorize_group applies only to TACACS+ clients` |
| `radius.eap.resumption_ms` | `<file>: radius.eap.resumption_ms: not supported by this program (session resumption needs rradius-enterprise)` |
| `radius.eap.shared_tickets` | `<file>: radius.eap.shared_tickets: not supported by this program (session resumption needs rradius-enterprise)` |
| `radius.eap.ticket_seal_file` | `<file>: radius.eap.ticket_seal_file: not supported by this program (session resumption needs rradius-enterprise)` |
| `radius.eap.previous_ticket_seal_file` | `<file>: radius.eap.previous_ticket_seal_file: not supported by this program (session resumption needs rradius-enterprise)` |
| `tacacs.clients[].tls_name` | `<file>: tacacs client '<name>': tls_name needs [tacacs.tls]`（而 `[tacacs.tls]` 在核心也被拒絕，見下面） |
| `tacacs.clients[].mfa` | `<file>: tacacs client '<name>': mfa needs rradius-enterprise` |
| `tacacs.clients[].require_message_authenticator`、`limit_proxy_state`、`eap_methods`、`coa_port`、`coa_sender`、`max_radsec_connections` | `<file>: tacacs client '<name>': <鍵> applies only to RADIUS clients` |

EAP 方法也一樣。`radius.eap.methods` 寫 `aka-prime` 時，訊息是 `radius.eap.methods: aka-prime is not supported by this program (EAP-AKA' needs rradius-enterprise)`；用戶端的 `eap_methods` 寫 `aka-prime` 時是 `radius client '<name>': eap_methods: aka-prime is not supported by this program (EAP-AKA' needs rradius-enterprise)`。寫 `sim` 時，兩處都得到 `'sim' is not a method this server offers; it has tls, peap and ttls`（用戶端的前面同樣加上 `radius client '<name>': eap_methods: `）。

### 接受但沒有作用的鍵

這幾個鍵 `check` 接受、也不警告，但核心的程式不會用到它們。核心書不使用它們。

| 鍵 | 說明 |
| --- | --- |
| `radius.clients[].tls_name` | 企業版 RadSec 用來比對用戶端憑證的名稱。核心沒有 RadSec。和 `address` 一起寫時被接受但不使用；只寫 `tls_name`、不寫 `address` 時，`check` 失敗：`radius client '<name>' has no address, so it can only use RadSec, which needs rradius-enterprise`。 |
| `radius.clients[].coa_port` | 企業版送 CoA 與 Disconnect 給這個用戶端時用的埠（預設 3799）。核心不送 CoA。 |
| `radius.coa_timeout_ms` | 企業版每次 CoA 或 Disconnect 等待回應的時間（預設 1000 毫秒）。 |
| `radius.coa_attempts` | 企業版 CoA 或 Disconnect 的嘗試次數，含第一次（預設 3）。 |

### 核心拒絕的區段（不在核心 schema 裡）

下面這些區段與鍵不在 `rradiusd schema` 的輸出裡，是企業版的。核心讀到時 `check` 結束碼 2，訊息格式是 `<區段>: not supported by this program (<功能> needs rradius-enterprise)`：

| 區段或鍵 | 括號裡的說明 |
| --- | --- |
| `[proxy]` | `proxying needs rradius-enterprise` |
| `[mfa]` | `two-factor login needs rradius-enterprise` |
| `[diameter]` | `Diameter needs rradius-enterprise` |
| `[accounting_export]` | `accounting export needs rradius-enterprise` |
| `[cluster.backup]` | `backups need rradius-enterprise` |
| `[radius.tls]` | `RadSec needs rradius-enterprise` |
| `[radius.coa]` | `CoA needs rradius-enterprise` |
| `[radius.dynamic_clients]` | `dynamic clients need rradius-enterprise` |
| `[radius.dpsk]` | `DPSK needs rradius-enterprise` |
| `[radius.eap.aka]` | `EAP-AKA' needs rradius-enterprise` |
| `[accounts.aka]` | `EAP-AKA' needs rradius-enterprise` |
| `[accounts.dpsk]` | `DPSK needs rradius-enterprise` |
| `[accounts.clients]` | `dynamic clients need rradius-enterprise` |
| `[tacacs.tls]` | `TACACS+ over TLS needs rradius-enterprise` |
| `radius.eap.ocsp`、`radius.eap.ocsp_url` | `OCSP needs rradius-enterprise`（`methods` 要有 `tls`；沒有 `tls` 時先得到 `radius.eap.ocsp only applies to tls, which methods does not have`） |

實際的一行例如：

```console
$ rradiusd check --config rradius.toml
rradius.toml: proxy: not supported by this program (proxying needs rradius-enterprise)
$ echo $?
2
```

---

## 常見錯誤

**打錯鍵名。**

- 症狀：`check` 結束碼 2，例如（`[metrics]` 裡把 `port` 打成 `prot`）：

  ```console
  $ rradiusd check --config rradius.toml
  rradius.toml: line 15: `prot`: unknown field `prot`, expected `bind` or `port`
  ```

- 原因：每個表都拒絕未知的鍵。
- 解法：照訊息裡的行號與可用鍵名修正，對照本附錄的表。

**值不是可用的選項。**

- 症狀：例如 `auth.policy = "any"`：

  ```console
  $ rradiusd check --config rradius.toml
  rradius.toml: line 31: `policy`: unknown variant, expected `first_match` or `all_accept`
  ```

- 解法：改成訊息列出的其中一個值。

**被引入的檔案重複宣告主檔已有的表。**

- 症狀：`rradius.toml: line 413: duplicate key`。
- 原因：`include` 的檔案視同接在主檔後面，同一個表不能宣告兩次。
- 解法：把設定移到主檔原有的表，或從主檔刪掉那個表、只留在被引入的檔案裡。

**少了必要的區段或密碼。**

- `the [lua] section is required`、`the [cluster] section is required`：補上該區段，見 [lua](#lua)、[cluster](#cluster)。
- `at least one of [radius] or [tacacs] must be configured`：至少要有其中一個。
- `exactly one of cluster.secret_api or cluster.secret_api_file must be set`：兩個鍵剛好寫一個。
- `radius client '127.0.0.1': exactly one of secret or secret_file must be set`：用戶端的 `secret` 與 `secret_file` 剛好寫一個。
- `radius client '': address is required, or tls_name for a client without a fixed address`：補上 `address`。

**EAP 的條件沒滿足。**

- `radius.eap.cert_file is required`：`[radius.eap]` 一定要有 `cert_file` 與 `key_file`。
- `radius.eap.client_ca_file is required`：`methods` 有 `tls` 時要有 `client_ca_file`。
- `radius.eap.methods has a method whose password only a chain can check, and there is no [auth] section`：用 `peap` 或 `ttls` 時要有 `[auth]`。
- `radius client 'example': eap_methods is required when radius.eap.methods has more than one method; [] for a client without EAP`：每個用戶端寫上自己的 `eap_methods`。
- `radius client 'example': eap_methods needs [radius.eap]`：先設定 `[radius.eap]`。

**叢集埠監聽在所有介面上的警告。**

- 症狀：`check` 與 `serve` 印出 `WARN rradius_core::config: cluster.listen is not set, so the cluster's Raft and API ports are on all interfaces, with nothing but their shared secrets to protect them; …`。
- 原因：有 `[[cluster.nodes]]` 而沒寫 `cluster.listen` 時，預設監聽 `0.0.0.0`。
- 解法：把 `cluster.listen` 設成這個節點的私有位址；確定要所有介面時寫 `"0.0.0.0"`，警告就會消失。見 [16](16-cluster.md) 與 [20](20-security.md)。

**改了設定、reload 之後沒生效。**

- 症狀：日誌出現 `configuration reloaded from …; these need a restart to apply: [cluster], radius.auth_port` 這類訊息。
- 原因：列出的鍵在本附錄的 reload 欄是「重啟」。
- 解法：排定時間重新啟動。在那之前，這些鍵維持啟動時的值，之後每次 reload 都會再列一次。

**把企業版的設定放進核心。**

- 症狀：`… not supported by this program (… needs rradius-enterprise)`，結束碼 2。
- 解法：見[核心不支援或沒有作用的設定](#核心不支援或沒有作用的設定)。

---

## 相關章節

- [04 設定檔的基本功](04-configuration.md)：設定檔的結構、`include`、`check`、`config show`、`config diff`、reload
- [05 RADIUS 服務與 NAS 用戶端](05-radius-clients.md)：`[radius]` 與 `[[radius.clients]]`
- [06 字典與屬性](06-dictionary.md)：`dictionary`
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)：`[auth]`、`[accounts]`
- [08 接上 LDAP 與 Active Directory](08-ldap-and-ad.md)：ldap 與 netlogon 處理器
- [09 外部資料庫與具名查詢](09-external-databases.md)：`[databases]`、`[queries]`
- [10 用 Lua 寫政策](10-lua-policies.md)：`[lua]`、`[lua.http]`
- [11 802.1X 與 EAP](11-eap.md)：`[radius.eap]`
- [12 TACACS+ 設備管理](12-tacacs.md)：`[tacacs]`
- [13 計費與 session](13-accounting.md)：計費 spool、計費記錄檔、`[sessions]`
- [16 叢集與高可用](16-cluster.md)：`[cluster]`
- [17 監控、狀態與日誌](17-monitoring.md)：`[metrics]`、`[log]`
- [19 效能調校與壓測](19-performance.md)：`[runtime]`、`radius.sockets` 等
- [91 附錄 B：命令參考](91-command-reference.md)：`check`、`config`、`schema` 等指令
