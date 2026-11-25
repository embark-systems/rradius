# 91 附錄 B：命令參考

> 本章讓你做到：
> - 查到 `rradiusd` 每個子命令的寫法、旗標、輸出與結束碼
> - 查到 `rradius-bench run`、`calibrate` 的每個旗標與預設值，以及 `--load` 的全部種類
> - 在腳本裡依結束碼判斷結果
> - 認出只有企業版才有的命令，知道核心遇到它們時的反應
> - 程式更新後，自己比對這份參考與 `help` 的輸出

前置條件：讀過 [03 第一次啟動與登入](03-quick-start.md)，會啟動 quick-start 情境。

本章小節：

- [怎麼讀這份參考](#怎麼讀這份參考)
- [驗證用的環境](#驗證用的環境)
- [命令總覽](#命令總覽)
- [結束碼總表](#結束碼總表)
- [serve](#serve)
- [check](#check)
- [help、--version](#help--version)
- [schema](#schema)
- [config](#config)
- [tuning](#tuning)
- [status](#status)
- [user](#user)
- [session](#session)
- [query](#query)
- [cluster](#cluster)
- [舊的旗標寫法](#舊的旗標寫法)
- [只有企業版才有的命令](#只有企業版才有的命令)
- [rradius-bench run](#rradius-bench-run)
- [--load 的寫法](#--load-的寫法)
- [run 的輸出與結束碼](#run-的輸出與結束碼)
- [rradius-bench calibrate](#rradius-bench-calibrate)
- [比對方法](#比對方法)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

---

## 怎麼讀這份參考

這份附錄的內容來自兩個執行檔的實測輸出，再對照命令列解析的程式碼：

- `rradiusd`：`rradius-core/src/cli.rs`，以及各子命令的 `status.rs`、`config_tools.rs`、`tuning.rs`、`accounts.rs`、`sessions.rs`、`query_command.rs`、`cluster_commands.rs`、`admin.rs`
- `rradius-bench`：`rradius-bench/src/cli.rs`、`main.rs`、`run.rs`、`report.rs`、`calibrate.rs`

本附錄的每個命令都實際執行過，範例裡的輸出照抄，只把機器相關的路徑縮成 `…`；少數結束碼的路徑只對照了程式碼，列在[結束碼總表](#結束碼總表)底下。速率、延遲這類數字每台機器不同，不要拿來當效能參考。

幾個全部命令共通的規則：

- **`--config <path>`：** 沒寫時讀 `/etc/rradius/rradius.toml`。實際的預設路徑是建置時決定的，`rradiusd --version` 的第二行會印出來。讀不到預設的設定檔時，訊息後面會多一行 `(the default configuration; give --config <path> for another)`。
- **旗標的位置：** `status`、`user`、`session`、`cluster` 的旗標可以放在子命令後面的任何位置，`user show --config a.toml dave` 與 `user show dave --config a.toml` 相同。其他命令要照用法的順序寫：例如 `config --config a.toml show`、`query --json list` 都是用法錯誤。同一個旗標寫兩次也是用法錯誤。
- **輸出：** 結果寫到標準輸出，錯誤與用法說明寫到標準錯誤。管理命令只把 `WARN` 以上的日誌寫到標準錯誤，所以 `check` 會印出祕密檔權限之類的警告。
- **子命令沒有自己的 `--help`。** 參數不對時，命令會印出自己的用法並以結束碼 2 結束；完整的用法清單用 `rradiusd help`。

## 驗證用的環境

範例都在 quick-start 情境的一份複本裡執行。為了讓一份設定就能測到 EAP、TACACS+ 與具名查詢，複本在 quick-start 之外做了這些改動：

```toml
# rradius.toml（只列出與 quick-start 不同的部分）
# …（其他設定略）

[cluster]
data_dir = "data"
secret_raft_file = "raft.secret"
secret_api_file = "api.secret"
migrations_dir = "migrations"
listen = "127.0.0.1"

[[cluster.nodes]]
id = 1
raft = "127.0.0.1:8100"
api = "127.0.0.1:8200"

[radius]
bind = "127.0.0.1"
auth_port = 1812
acct_port = 1813
status_server = true

[radius.eap]
cert_file = "certs/server.pem"
key_file = "certs/server.key"
client_ca_file = "certs/ca.pem"
methods = ["tls", "peap", "ttls"]

[[radius.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "localhost.secret"
eap_methods = ["tls", "peap", "ttls"]

[tacacs]
bind = "127.0.0.1"
port = 4949

[[tacacs.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "tacacs.secret"

[metrics]
bind = "127.0.0.1"
port = 9812

[queries.enabled_users]
help = "Accounts that can log in"
sql = "SELECT enabled, COUNT(*) AS value FROM users GROUP BY enabled"
labels = ["enabled"]
interval_ms = 60000
```

- 埠用預設值：RADIUS 1812、1813，指標 9812，叢集 8100、8200。TACACS+ 跟[第 12 章](12-tacacs.md)一樣用 4949，因為標準埠 49 小於 1024，一般帳號不能監聽。
- `[accounts]` 與 [03 第一次啟動與登入](03-quick-start.md) 加的相同。
- 憑證用 wifi-certs 情境的 `setup.sh` 產生到這個目錄：`sh <repo>/rradius-server/examples/scenarios/wifi-certs/setup.sh .`。伺服器憑證的名稱是 `radius.corp.example.com`。
- `tacacs.secret` 和其他 `.secret` 一樣用 `openssl rand -hex 32` 產生，權限 0600。

這份設定通過 `check`，各段的意思見[附錄 A](90-config-reference.md)。

## 命令總覽

`rradiusd` 的命令，依 `rradiusd help` 的順序：

| 命令 | 做什麼 | 要伺服器在執行中 | 主章 |
| --- | --- | --- | --- |
| `serve` | 啟動伺服器，直到收到停止訊號 | — | [03](03-quick-start.md) |
| `check` | 檢查設定，不開任何埠 | 否 | [04](04-configuration.md) |
| `status` | 讀指標埠，整理成一頁 | 是（讀 `[metrics]`） | [17](17-monitoring.md) |
| `--version` | 印出版本與預設設定檔路徑 | 否 | 本章 |
| `schema` | 印出設定檔的 JSON Schema | 否 | [04](04-configuration.md) |
| `config show` | 印出補上預設值的完整設定 | 否 | [04](04-configuration.md) |
| `config diff` | 比較兩份設定 | 否 | [04](04-configuration.md) |
| `config init` | 產生一份設定底稿 | 否 | [04](04-configuration.md) |
| `tuning` | 依這台機器與設定給系統調校建議 | 否 | [19](19-performance.md) |
| `user add`、`user passwd`、`user enable`、`user disable`、`user delete`、`user show` | 用 `[accounts]` 的 SQL 管理帳號 | 是（叢集 API 埠） | [07](07-users-and-chains.md) |
| `session list`、`session show` | 用 `[sessions]` 的 SQL 列出在線 session | 是（叢集 API 埠） | [13](13-accounting.md) |
| `query list`、`query <name>` | 列出或執行 `[queries.<name>]` | `list` 否；執行要 | [09](09-external-databases.md) |
| `cluster members`、`remove` | 列出叢集成員、移除節點 | 是（叢集 API 埠） | [16](16-cluster.md) |
| `help` | 印出全部命令的用法 | 否 | 本章 |

`rradius-bench` 的命令：

| 命令 | 做什麼 | 主章 |
| --- | --- | --- |
| `run` | 對伺服器送負載，報告速率與延遲；也能用 `--requests 1` 當登入測試 | [19](19-performance.md) |
| `calibrate` | 對自己行程裡的回應者送負載，量這個工具在這台機器上的上限 | [19](19-performance.md) |
| `help` | 印出命令清單，或某個命令的說明 | 本章 |

## 結束碼總表

`rradiusd` 的慣例是：0 成功，2 用法或設定錯誤。其他值依命令而不同：

| 命令 | 0 | 1 | 2 | 3 |
| --- | --- | --- | --- | --- |
| `serve` | 收到 SIGTERM 或 SIGINT 後正常停止，包括啟動途中被要求停止 | 啟動後無法服務：RADIUS 埠無法監聽、叢集錯誤中斷 migration、沒有 leader 而設了 `stop_without_leader_ms`、Raft 核心停止 | 設定、字典、腳本有錯，migration 檔不合規則或 SQL 失敗，叢集節點無法啟動（`cannot start cluster node …`），參數不對 | — |
| `check` | `configuration ok` | — | 設定有錯 | — |
| `help`、`--version`、不帶參數 | 印出結果 | — | 不認得的命令 | — |
| `schema` | 印出 schema | 寫出失敗，例如管線被關掉 | 參數不對 | — |
| `config show`、`init` | 印出結果 | — | 參數或設定有錯 | — |
| `config diff` | 沒有差異，或只有節點專屬的差異 | 有差異 | 參數或設定有錯 | — |
| `tuning` | 印出建議，包括讀不到預設設定檔、平台沒有建議 | — | 參數不對、`--config` 的檔案讀不到 | — |
| `status` | 印出結果；`--watch` 被 SIGINT 或 SIGTERM 結束 | — | 參數不對、沒有 `[metrics]`、設定讀不到、問不到叢集成員 | 讀不到指標埠；`--cluster` 是全部成員都讀不到 |
| `user` | 成功 | 沒有符合的帳號、SQL 失敗 | 參數或設定有錯、`[accounts]` 缺少這個動作、密碼空白或兩次不同 | 連不到叢集，或 30 秒內沒有回答 |
| `session` | 有資料 | 沒有 session | 參數或設定有錯、`[sessions]` 缺少這個動作、SQL 失敗 | 同 `user` |
| `query` | 成功，包括沒有任何查詢、查無資料列 | — | 參數或設定有錯、`queries.<name>` 沒設定、SQL 失敗 | 同 `user` |
| `cluster` | 成功 | `remove`：叢集裡沒有這個節點、leader 拒絕、leader 連續換了兩次 | 參數或設定有錯、API 密碼不對、要移除的節點還在回應、移除後只剩一或兩個 voter 而沒有 `--force`、最後一個 voter | 30 秒內沒有回答；`remove` 送出後沒有回應 |
| 企業版的命令 | — | — | 一律 2 | — |

> **未實機驗證：** 表中這幾格只對照了程式碼，沒有實際重現：`serve` 啟動途中被要求停止的 0；`serve` 因叢集錯誤中斷 migration、`stop_without_leader_ms`、Raft 核心停止的 1；`serve` 叢集節點無法啟動的 2；`tuning` 在沒有建議的平台上的 0；`status --cluster` 問不到成員的 2；`query` 的 SQL 失敗的 2、沒有任何查詢時 `query list` 的 0；`user` 兩次密碼不同的 2；`cluster` 需要多節點的各格（見 [cluster](#cluster)）。其他各格都實測過。

`rradius-bench` 另有一套，見 [run 的輸出與結束碼](#run-的輸出與結束碼)：0 全部 Accept，1 有 Reject 或 Challenge，2 有逾時、無效回應或 socket 錯誤，3 參數錯誤或沒能開始。

> **注意：** 同樣是「查不到東西」，`session` 的結束碼是 1，`query` 卻是 0（印出 `no rows`）。在腳本裡判斷 `query` 有沒有資料，要看輸出，不能看結束碼。

---

## serve

```text
rradiusd serve [--config <path>]
```

啟動伺服器：讀設定、啟動叢集節點、套用 migrations、開 RADIUS／TACACS+／指標埠，然後一直執行。啟動的過程與日誌見 [03 第一次啟動與登入](03-quick-start.md)。

執行中可以送這些訊號：

| 訊號 | 效果 |
| --- | --- |
| SIGHUP | 重新載入（reload）設定。日誌：`configuration reloaded from rradius.toml`；有要重啟才生效的鍵時會一併列出。啟動途中收到的 SIGHUP 等開始服務後才處理 |
| SIGTERM、SIGINT（Ctrl+C） | 停止：`/readyz` 先回 503，不再接新請求，等處理中的請求，最多等 `[shutdown]` 設定的時間，再關閉資料庫 |
| 停止途中再一次 SIGTERM 或 SIGINT | 不再等處理中的請求 |

```console
$ rradiusd serve --config rradius.toml
…
… INFO rradius_core::cli: Servers started. Press Ctrl+C to stop.
… INFO rradius_core::radius: RADIUS Server listening on 127.0.0.1:1812 and 127.0.0.1:1813
… INFO rradius_core::metrics: Metrics listening on 127.0.0.1:9812
… INFO rradius_core::tacacs: TACACS+ Server listening on 127.0.0.1:4949
```

結束碼的例子（都實測過）：

```console
$ rradiusd serve --config bindfail.toml      # RADIUS 埠被別的行程占用
…
RADIUS server error: cannot listen on 127.0.0.1:1812: Address already in use (os error 48)
$ echo $?
1
$ rradiusd serve --config badmig.toml        # migration 檔的 SQL 有錯
migration file '1_bad.sql' could not be applied: Sqlite: near "TABL": syntax error in CREATE TABL oops (x);
 at offset 7
$ echo $?
2
$ rradiusd serve --config rradius.toml extra
usage: rradiusd serve [--config <path>]
$ echo $?
2
```

- 埠被占用時，叢集節點已經啟動，伺服器會先把資料庫乾淨地關掉再結束，所以結束碼是 1，不是 2。錯誤碼的數字依作業系統而不同。
- 正常停止時結束碼是 0（實測：送 SIGTERM 後 `wait` 得到 0）。
- Raft 核心停止時，日誌會寫 `its Raft core stopped (…); exiting so that a restart recovers it`，結束碼 1，交給 systemd 或容器重新啟動。這個情況沒有實際重現過。

## check

```text
rradiusd check [--config <path>]
```

做啟動前會做的檢查，但不開埠、不建立資料目錄、不連任何東西：讀設定檔與 `include` 的檔案、字典、編譯 Lua 腳本、檢查 migrations 目錄的檔名、驗證鏈、`[lua.http]`、計費 spool 目錄，最後印出共用密碼太短之類的警告。

```console
$ rradiusd check --config rradius.toml
configuration ok
$ rradiusd check --config ent.toml           # 多了核心不支援的 [proxy]
ent.toml: proxy: not supported by this program (proxying needs rradius-enterprise)
$ echo $?
2
```

- 成功時印 `configuration ok`（標準輸出），結束碼 0；有錯時印出原因（標準錯誤），結束碼 2。
- `check` 只檢查 migration 檔的**檔名**，不執行 SQL。上一節 `1_bad.sql` 的語法錯誤，`check` 會通過，`serve` 才會發現。
- `check` 後面多一個參數，例如 `check --config a.toml extra`，印出 `usage: rradiusd check [--config <path>]`，結束碼 2。

## help、--version

```console
$ rradiusd --version
rradiusd 0.1.0 (community)
default config: /etc/rradius/rradius.toml
$ rradiusd help
usage: rradiusd serve [--config <path>]
usage: rradiusd check [--config <path>]
usage: rradiusd status [--config <path>] [--cluster] [--watch | --json | --tui]
usage: rradiusd --version
usage: rradiusd schema [--lang en|zh-TW]
usage: rradiusd config show [--config <path>]
usage: rradiusd config diff <a.toml> <b.toml>
usage: rradiusd config init [--full] [--lang en|zh-TW]
usage: rradiusd tuning [--config <path>]
usage: rradiusd user add|passwd|enable|disable|delete|show [--config <path>] <name>
  add and passwd read the password from standard input
usage: rradiusd session list [--config <path>] [--json]
usage: rradiusd session show [--config <path>] [--json] <name>
usage: rradiusd query list [--config <path>] [--json]
usage: rradiusd query <name> [--config <path>] [--json]
usage: rradiusd cluster members|remove [--config <path>] [--force] [<node_id>]
  remove takes a node id, and --force to leave one or two voters
without --config, the configuration is /etc/rradius/rradius.toml
```

- `help`、`--help`、`-h` 與不帶任何參數，印出的都是這份清單（標準輸出），結束碼 0。
- 不認得的命令，例如 `rradiusd bogus`，把同一份清單印到標準錯誤，結束碼 2。
- `--version` 只能單獨使用。

## schema

```text
rradiusd schema [--lang en|zh-TW]
```

把設定檔的 JSON Schema 印到標準輸出，說明文字用 `--lang` 指定的語言，預設 `en`。編輯器可以拿它做自動完成與檢查；附錄 A 也是從它整理的。

```console
$ rradiusd schema --lang zh-TW > schema.json
$ python3 -c 'import json; print(sorted(json.load(open("schema.json"))["properties"]))'
['accounts', 'auth', 'cluster', 'databases', 'dictionary', 'include', 'log', 'lua', 'metrics', 'queries', 'radius', 'runtime', 'sessions', 'shutdown', 'tacacs']
$ rradiusd schema --lang fr
usage: rradiusd schema [--lang en|zh-TW]
```

- 不認得的語言或多餘的參數：結束碼 2。
- 寫出失敗時印 `schema: …`，結束碼 1。例如 `rradiusd schema | head -1` 會在標準錯誤看到 `schema: Broken pipe (os error 32)`，這是 `head` 提早關掉管線造成的，不是 schema 有問題。

## config

```text
rradiusd config show [--config <path>]
rradiusd config diff <a.toml> <b.toml>
rradiusd config init [--full] [--lang en|zh-TW]
```

只有 `init` 接受 `--lang`，`show` 與 `diff` 給了 `--lang` 是用法錯誤。三個子命令都不連伺服器。用法上的細節見 [04 設定檔的基本功](04-configuration.md)。

### config show

印出補上全部預設值的設定（含 `include` 的檔案）。祕密顯示成 `<hidden>`；第一行的註解列出依這台機器算出來的值。

```console
$ rradiusd config show --config rradius.toml
# computed on this machine: radius.sockets = 1, cluster.read_connections = 28
dictionary = "…/share/dictionary"

[lua]
script = "accounting.lua"
max_execution_ms = 1000
max_memory_mb = 256
query_timeout_ms = 5000
…
```

設定有錯時，`config show` 與 `check` 印出相同的錯誤，結束碼 2。

### config diff

比較兩份設定補上預設值之後的結果，一個鍵一行，格式是 `<鍵>: <a 的值> | <b 的值>`：

```console
$ rradiusd config diff rradius.toml b.toml
auth.policy: "first_match" | "all_accept"
radius.auth_port: 1812 | 1822
$ echo $?
1
$ rradiusd config diff n2.toml n3.toml
cluster.secret_api: differs (hidden)
cluster.secret_api_file: "api.secret" | (absent)
node-specific:
  cluster.listen: "127.0.0.1" | "127.0.0.2"
$ rradiusd config diff rradius.toml rradius.toml
no differences
```

- 只有一邊有的鍵寫 `(absent)`；祕密只說 `differs (hidden)`，不印值。
- `cluster.node_id` 與 `cluster.listen` 本來就該每個節點不同，放在最後的 `node-specific:` 底下，這兩個鍵**不算**差異：只有這兩個鍵不同時，結束碼是 0。
- `radius.clients`、`tacacs.clients` 依名稱（沒有名稱時依位址）對應，順序不同不算差異。
- 結束碼：0 沒有差異，1 有差異，2 參數或任一份設定有錯。

### config init

把一份設定底稿印到標準輸出，叢集密碼與範例用戶端的共用密碼都是隨機產生的（32 個字元）。`--lang zh-TW` 讓註解變成中文。底稿的內容與接下來要改的地方見 [03 第一次啟動與登入](03-quick-start.md)。

```console
$ rradiusd config init --lang zh-TW > rradius.toml
$ head -2 rradius.toml
# 由 `config init` 產生的 rradius 設定檔。
# 叢集的每個節點都要用相同的 secret_raft 與 secret_api；範例用戶端的 secret 也要設在 NAS 上。
```

## tuning

```text
rradiusd tuning [--config <path>]
```

讀這台機器的系統設定與 UDP 掉包計數，再依設定檔估計需要的 socket 緩衝區與檔案數量，印出建議值、原因與指令。Linux、macOS、FreeBSD 有建議；其他平台印 `rradius tuning: no advice for this platform (Linux, macOS and FreeBSD have it)`，結束碼 0。每一項的意思見 [19 效能調校與壓測](19-performance.md)。

```console
$ rradiusd tuning --config rradius.toml
rradius tuning advice for macOS

UDP packets dropped because a receive buffer was full, since the system started: …
  Raise the receive buffers below, then watch whether this count still grows.

Largest socket buffer (kern.ipc.maxsockbuf)
  current:   8388608
  suggested: 33554432
  why:       the receive buffer counts its bookkeeping against this limit too, so twice the buffer; raise it before recvspace
  now:       sysctl -w kern.ipc.maxsockbuf=33554432
  persist:   /etc/sysctl.conf: kern.ipc.maxsockbuf = 33554432
…
These suggestions are unmeasured starting points: apply them, then confirm with the dropped packet counts above.
```

- `tuning` 只印建議，不改任何系統設定。
- 沒寫 `--config` 而預設的設定檔讀不到時，印出一行原因與 `the default settings are used`，用預設值給建議，結束碼仍是 0。寫了 `--config` 而讀不到，結束碼 2。

## status

```text
rradiusd status [--config <path>] [--cluster] [--watch | --json | --tui]
```

| 旗標 | 意思 |
| --- | --- |
| （無） | 隔一秒讀兩次這個節點的 `/metrics`，印出一頁後結束 |
| `--watch` | 之後每秒再讀一次、更新畫面，直到 Ctrl+C；在終端機上會清畫面重畫 |
| `--json` | 改印 JSON，一次一個物件 |
| `--cluster` | 向叢集問成員，讀每個成員的指標埠，一行一個節點 |
| `--tui` | 全螢幕互動畫面（項目表加最近 60 秒的折線圖；上下選項目、`[` `]` 選成員或全部、`p` 暫停）；不能和 `--json`、`--watch` 並用，標準輸出要是終端機，否則結束碼 2 |

`status` 讀的是設定檔 `[metrics]` 的位址；`bind` 是 `0.0.0.0` 或 `::` 時改連本機位址。每一段、每一行的意思見 [17 監控、狀態與日誌](17-monitoring.md)。

```console
$ rradiusd status --config rradius.toml
rradius status of 127.0.0.1:9812 over 1.0 s

Node
  Ready                     ok
  Leader                    1
…
$ rradiusd status --config rradius.toml --json
{"address":"127.0.0.1:9812","counter_reset":false,"interval_seconds":1.002444875,"ready":{"status":200,"text":"ok"},"sections":[…]}
$ rradiusd status --config rradius.toml --cluster
rradius cluster status over 1.0 s, members from the cluster

node   role    ready  auth/s  acct/s  discarded/s  in progress  address
1      leader  ok     0.0     0.0     0.0          0            127.0.0.1:9812
total          1/1    0.0     0.0     0.0          0
```

- `--watch --json` 每秒印一行 JSON，適合接到其他程式。
- `--cluster` 要用叢集的 API 密碼問成員，所以設定檔的 `[cluster]` 要跟叢集一致。每個成員的指標埠，取該成員 API 位址的主機加上這份設定 `[metrics]` 的埠。
- 錯誤：

  | 情況 | 輸出 | 結束碼 |
  | --- | --- | --- |
  | 沒有 `[metrics]` | `status needs [metrics]: the command reads this node's /metrics. Add to …`，後面附上一段 `[metrics]` 範例 | 2 |
  | 指標埠沒人回應 | `cannot reach the metrics port at 127.0.0.1:9812: Connection refused (os error 61)` | 3 |
  | `--cluster`，全部成員都讀不到 | 表格照印，讀不到的成員標 `unreachable`，最後列出原因 | 3 |
  | API 密碼不對 | `127.0.0.1:8200 refused the api secret: [cluster] secret_api differs from the cluster's` | 2 |
  | 同一個旗標寫兩次 | 用法 | 2 |

  `--watch` 時讀不到指標埠不會結束，每秒印一次錯誤並重試（這一點只對照了程式碼）。

## user

```text
rradiusd user add|passwd|enable|disable|delete|show [--config <path>] <name>
```

執行 `[accounts]` 裡同名的 SQL，透過叢集的 API 埠寫入或查詢資料表，所以伺服器一定要在執行中。`<name>` 會像登入時一樣正規化，例如預設驗證鏈的 `username` 設定會去掉 realm。帳號資料表與 `[accounts]` 的寫法見 [07 使用者帳號與驗證鏈](07-users-and-chains.md)。

| 動作 | 用到的鍵 | 成功時的輸出 |
| --- | --- | --- |
| `add` | `accounts.add` | `added <name>` |
| `passwd` | `accounts.passwd` | `password changed for <name>` |
| `enable` | `accounts.enable` | `enabled <name>` |
| `disable` | `accounts.disable` | `disabled <name>` |
| `delete` | `accounts.delete` | `deleted <name>` |
| `show` | `accounts.show` | 每欄一行 `欄位=值`；多列之間空一行 |

- `add` 與 `passwd` 從標準輸入讀密碼：在終端機上關掉回顯問兩次（`password: `、`again: `）；從管線讀時只讀第一行。存進資料表前依 `accounts.scheme` 處理。
- 寫入類的動作以「有沒有改到資料列」判斷：沒改到任何列時印 `no account <name>`，結束碼 1。

```console
$ printf 'dave-password\n' | rradiusd user add --config rradius.toml dave
added dave
$ printf 'new-password\n' | rradiusd user passwd --config rradius.toml dave
password changed for dave
$ rradiusd user show --config rradius.toml dave
username=dave
enabled=1
$ rradiusd user delete --config rradius.toml dave
deleted dave
$ rradiusd user delete --config rradius.toml dave
no account dave
$ echo $?
1
```

錯誤的例子：

| 情況 | 輸出 | 結束碼 |
| --- | --- | --- |
| 帳號已存在時 `add` | `Connect: Error code 1555: constraint failed Some("UNIQUE constraint failed: users.username")` | 1 |
| 密碼是空的 | `the password is empty; nothing was changed` | 2 |
| 終端機上兩次密碼不同 | `the two passwords differ; nothing was changed` | 2 |
| 少了名稱 | `a name is required`，後接用法 | 2 |
| 多一個參數 | `unexpected argument 'b'`，後接用法 | 2 |
| `--config` 後面沒有值 | `--config needs a value`，後接用法 | 2 |
| `[accounts]` 沒有這個動作 | `accounts.add is not set in rradius.toml` | 2 |
| 伺服器沒在跑 | 等 30 秒後 `the cluster did not answer within 30 s`；等待時標準錯誤會有 `Error looking up DB metrics` | 3 |
| API 密碼不對 | 立刻 `127.0.0.1:8200 refused the api secret: [cluster] secret_api differs from the cluster's` | 2 |

「兩次密碼不同」只在終端機上會發生，沒有實測，依程式碼寫出。

## session

```text
rradiusd session list [--config <path>] [--json]
rradiusd session show [--config <path>] [--json] <name>
```

執行 `[sessions]` 的 `list` 或 `show` 查詢（`show` 的 `?1` 是正規化後的名稱），把結果照 SELECT 的欄位順序印出。伺服器本身沒有 session 表，「在線」的定義全在你寫的 SQL。設定方式見 [13 計費與 session](13-accounting.md)。

```console
$ rradiusd session list --config rradius.toml
username=alice
nas=192.0.2.1
session_id=11b8000000000000
started=1795384834
updated=1795384834
$ rradiusd session list --config rradius.toml --json
[{"username":"alice","nas":"192.0.2.1","session_id":"11b8000000000000","started":1795384834,"updated":1795384834}]
$ rradiusd session show --config rradius.toml nobody
no sessions for nobody
$ echo $?
1
$ rradiusd session show --config rradius.toml --json nobody
[]
```

- 文字格式每欄一行 `欄位=值`，多個 session 之間空一行；NULL 印成空字串，BLOB 印成小寫十六進位。`--json` 是物件陣列，數字保持數字。
- 沒有資料時：文字格式印 `no sessions` 或 `no sessions for <name>`，`--json` 印 `[]`，結束碼都是 1。
- SQL 失敗時印 `the statement failed: …`，結束碼 **2**（`user` 的 SQL 失敗是 1）。實測：`the statement failed: Sqlite: no such column: nosuchcol in SELECT nosuchcol, …`。
- `[sessions]` 沒有 `list` 時印 `sessions.list is not set in …`，結束碼 2。連不到叢集時與 `user` 相同，結束碼 3。

## query

```text
rradiusd query list [--config <path>] [--json]
rradiusd query <name> [--config <path>] [--json]
```

- `query list` 只讀設定檔，列出每個 `[queries.<name>]` 的名稱、`help`、`interval_ms` 與 `sql`，不連叢集。沒有任何查詢時什麼都不印，結束碼 0。
- `query <name>` 在叢集上執行一次那段 SQL，以表格印出。名稱 `list` 保留給列出，不能當查詢名稱。
- 具名查詢的寫法、對應的指標見 [09 外部資料庫與具名查詢](09-external-databases.md)。

```console
$ rradiusd query list --config rradius.toml
name=enabled_users
help=Accounts that can log in
interval_ms=60000
sql=SELECT enabled, COUNT(*) AS value FROM users GROUP BY enabled
$ rradiusd query enabled_users --config rradius.toml
enabled  value
0        1
1        2
$ rradiusd query enabled_users --config rradius.toml --json
[{"enabled":0,"value":1},{"enabled":1,"value":2}]
$ rradiusd query nobody_here --config q2.toml
no rows
$ echo $?
0
$ rradiusd query nope --config rradius.toml
queries.nope is not set in rradius.toml
```

- 表格的 NULL 印成 `NULL`；`--json` 的 NULL 是 `null`。`query list --json` 是一行物件陣列。
- 多行的 SQL 在 `query list` 的文字格式裡，換行會寫成 `\n`，讓一個查詢保持一行。
- 錯誤：名稱沒設定 2；SQL 失敗 2（訊息同 `session`）；連不到叢集 3。

## cluster

```text
rradiusd cluster members [--config <path>]
rradiusd cluster remove [--config <path>] [--force] <node_id>
```

透過設定檔 `[[cluster.nodes]]` 的 API 位址找到 leader，再列出或修改成員。完整的成員管理流程見 [16 叢集與高可用](16-cluster.md)。

```console
$ rradiusd cluster members --config rradius.toml
node 1  voter    leader  raft 127.0.0.1:8100  api 127.0.0.1:8200
```

`members` 每個成員一行：節點編號、`voter` 或 `learner`、是不是 `leader`、Raft 與 API 位址。叢集裡有、設定檔沒列的成員，行尾加 `(not in this configuration)`；設定檔列了、叢集裡沒有的，印 `node <id>  not in the cluster (listed in this configuration)`。

`remove` 把一個**已經停止**的節點移出叢集：

| 情況 | 輸出 | 結束碼 |
| --- | --- | --- |
| 成功 | `removed node <id>` | 0 |
| 節點還在回應 | `node 1 still answers at 127.0.0.1:8200: stop it first, then remove it` | 2 |
| 叢集裡沒有這個節點 | `no node 2 in the cluster` | 1 |
| 編號不是正整數 | `the node id must be a positive integer, not '0'` | 2 |
| 沒給編號 | `a node id is required`，後接用法 | 2 |
| 移除後只剩一個 voter | `removing node <id> leaves one voter, with nothing to take over when it stops; give --force to remove it anyway` | 2 |
| 移除後只剩兩個 voter | `… leaves two voters, and the cluster stops writing when either stops; give --force to remove it anyway` | 2 |
| 最後一個 voter | `node <id> is the last voter; the cluster cannot remove it` | 2 |
| 送出後 30 秒內沒有回應 | `the removal of node <id> may have been made, …; check with cluster members` | 3 |

- `--force` 只用在 `remove`，讓上面「只剩一個或兩個 voter」的情況照樣移除。`members --force` 是用法錯誤。
- API 密碼不對：`… refused the api secret: [cluster] secret_api differs from the cluster's`，結束碼 2，立刻出現（`user`、`session`、`query`、`client`、`dpsk`、`aka` 與 `cluster`、`status --cluster` 都一樣）。叢集都沒有回應：`the cluster did not answer within 30 s`，結束碼 3。

> **未實機驗證：** 表中「成功」「只剩一個或兩個 voter」「最後一個 voter」「送出後沒有回應」這幾列需要多節點叢集，只對照了 `cluster_commands.rs` 與它的測試；其他各列在單節點上實測過。

## 舊的旗標寫法

啟動與檢查以前是旗標，現在是子命令。用舊的寫法時，`rradiusd` 不會啟動伺服器，而是印出完整的用法清單，最後多一行提示，結束碼 2：

| 舊的寫法 | 最後一行的提示 |
| --- | --- |
| `rradiusd --config <path>` | `to start the server: rradiusd serve --config <path>` |
| 任何位置有 `--check`，例如 `rradiusd --check --config <path>` | `to check a configuration: rradiusd check [--config <path>]` |

## 只有企業版才有的命令

核心的 `rradiusd` 認得企業版命令的名稱，但不執行：在讀設定檔之前就印出 `<命令>: not supported by this program (<功能> needs rradius-enterprise)`，結束碼 2。後面的參數不影響結果。

| 命令 | 核心印出的訊息 |
| --- | --- |
| `aka` | `aka: not supported by this program (EAP-AKA' needs rradius-enterprise)` |
| `aka-seal` | `aka-seal: not supported by this program (EAP-AKA' needs rradius-enterprise)` |
| `client` | `client: not supported by this program (dynamic clients need rradius-enterprise)` |
| `dpsk` | `dpsk: not supported by this program (DPSK needs rradius-enterprise)` |
| `dpsk-seal` | `dpsk-seal: not supported by this program (DPSK needs rradius-enterprise)` |
| `disconnect` | `disconnect: not supported by this program (CoA needs rradius-enterprise)` |
| `coa` | `coa: not supported by this program (CoA needs rradius-enterprise)` |
| `otp` | `otp: not supported by this program (two-factor login needs rradius-enterprise)` |

這份清單是 `rradius-core/src/extension.rs` 的 `COMMANDS_NEEDING_ENTERPRISE`，八個都實測過。這些命令不在 `rradiusd help` 的清單裡。

> **企業版：** 企業版的 `help` 會多列出這些命令與它們的用法。

---

## rradius-bench run

> **注意：** `rradius-bench` 不在任何套件、壓縮檔或 Docker image 裡，要從原始碼建置，做法見[第 02 章](02-install.md)。

```text
rradius-bench run [OPTIONS] --server <SERVER> --secret <SECRET> --load <LOADS>
```

對伺服器送負載，結束後印出報告。`--server`、`--secret` 與至少一個 `--load` 是必填。參數都在送出任何請求之前檢查，有錯就以結束碼 3 結束，什麼都不送。

旗標與預設值（以 `rradius-bench run --help` 為準）：

| 旗標 | 預設 | 意思 |
| --- | --- | --- |
| `--server <SERVER>` | 必填 | 伺服器的名稱或位址 |
| `--secret <SECRET>` | 必填 | 共用密碼；TACACS+ 負載也用它當金鑰。會出現在行程清單裡，只用測試用的密碼 |
| `--load <LOADS>` | 必填 | 負載，寫法見下一節；可以寫多次，同時跑 |
| `--auth-port` | `1812` | 認證埠 |
| `--acct-port` | `1813` | 計費埠 |
| `--bind <BIND>` | — | socket 的來源位址 |
| `--user` | `bench` | 帳號名稱 |
| `--password` | `bench` | 密碼；`aka-prime` 負載寫成 `KI:OPC`，各 32 個十六進位數字 |
| `--users <N>` | — | 輪流使用 `<user>0` 到 `<user>N-1` |
| `--nas-ip` | `192.0.2.1` | 每個請求的 `NAS-IP-Address` |
| `--nas-identifier` | `rradius-bench` | 每個請求的 `NAS-Identifier` |
| `--attr <TYPE=KIND:VALUE>` | — | 加在每個請求上的屬性；`KIND` 是 `int`、`text`、`hex`、`ip`；`TYPE` 1 到 255，不能是 80；可以寫多次 |
| `--no-message-authenticator` | 關 | Access-Request 不帶 Message-Authenticator |
| `--timeout` | `5` | 等回應的秒數，之後重送或放棄 |
| `--retries` | `0` | 重送次數 |
| `--warmup` | `2` | 開始量測前先送幾秒 |
| `--duration` | `10` | 量測幾秒 |
| `--requests <N>` | — | 每個負載量到 N 個請求就停（`session` 算流程數），沒有暖身；不能與 `--warmup`、`--duration` 同時用 |
| `--metrics-url` | — | 伺服器的 `http://host:port/metrics`，報告量測期間計數器的變化；只接受 `http://` |
| `--json` | 關 | 報告改印 JSON |
| `--tls-version` | `1.2` | EAP 負載的 TLS 版本，`1.2` 或 `1.3`；`1.3` 只能用在 `eap-tls` |
| `--ca-file` | — | 檢查伺服器 EAP 憑證用的 CA；沒給時任何憑證都接受 |
| `--tls-name` | `--server` 的值 | 配合 `--ca-file`，伺服器憑證必須符合的名稱 |
| `--cert-file`、`--key-file` | — | `eap-tls` 登入用的憑證與私鑰；`--transport tls` 也要 |
| `--eap-fragment` | `1000` | 一個 EAP-Response 最多帶幾個位元組的 TLS 資料 |
| `--transport` | `udp` | `udp`，或 `tls`（RadSec，RFC 6614，每個 socket 一條 TLS 連線） |
| `--tls-port` | `2083` | RadSec 埠，認證與計費共用 |
| `--tacacs-port` | `49` | `tacacs-*` 負載的 TACACS+ 埠 |
| `--tacacs-connect` | `keep` | `keep`：整個執行期間每個 socket 一條連線；`per-request`：每個請求一條新連線 |

這些組合會在送出前被拒絕（結束碼 3，訊息都實測過）：

| 組合 | 訊息 |
| --- | --- |
| `--transport dtls` | `--transport dtls: udp or tls` |
| `--transport tls` 沒有憑證 | `--transport tls needs --cert-file and --key-file: RadSec servers ask for a client certificate` |
| `--transport tls --retries 1` | `--retries: nothing is sent again over TLS (RFC 6614), so --transport tls takes 0` |
| TACACS+ 負載加 `--transport tls` | `--transport tls: TACACS+ loads go over TCP, not RadSec` |
| `--tacacs-connect sometimes` | `--tacacs-connect sometimes: keep or per-request` |
| EAP 負載加 `--no-message-authenticator` | `--no-message-authenticator: EAP loads always carry Message-Authenticator (RFC 3579)` |
| `peap` 加 `--tls-version 1.3` | `--tls-version 1.3: only for eap-tls, not peap` |
| `eap-tls` 沒有 `--cert-file` | `eap-tls needs --cert-file` |
| `aka-prime` 的 `--password` 格式不對 | `aka-prime needs --password KI:OPC, each 32 hex digits` |
| `--attr 80=hex:00` | `--attr 80=hex:00: type 80 is Message-Authenticator, which the tool handles` |
| `--metrics-url https://…` | `--metrics-url https://127.0.0.1/metrics: must start with http://` |
| `--requests 0` | `--requests must be above 0` |
| `--requests` 與 `--duration` 一起 | clap 的用法說明 |
| 名稱解析失敗 | `--server nonexistent.invalid: failed to lookup address information: …` |

> **未實機驗證：** `--transport tls` 需要 RadSec 伺服器，核心的 `rradiusd` 沒有 RadSec，所以只驗證了上表的參數檢查，沒有對伺服器實際跑過。

## --load 的寫法

```text
--load KIND[,concurrency=N][,rate=N][,window=N][,threads=N][,alive=N]
```

`concurrency` 與 `rate` 至少要寫一個，否則得到 `--load pap: needs concurrency or rate`。

| 鍵 | 預設 | 意思 |
| --- | --- | --- |
| `concurrency` | 有 `rate` 時 256 | 同時在路上的請求數。只寫它是固定並行（fixed-concurrency）；與 `rate` 一起時是上限。要大於 0 |
| `rate` | — | 每秒送幾個，固定速率（fixed-rate）。要大於 0 |
| `window` | RADIUS 64，TACACS+ 1 | 一個 socket 上最多同時幾個請求，1 到 256。TACACS+ 只能是 1 |
| `threads` | 固定並行 2，固定速率 1 | 這個負載用幾個執行緒；不會多於 socket 數 |
| `alive` | 0 | 只用於 `session`：Start 與 Stop 之間送幾個 Interim-Update |

socket 數是 `concurrency ÷ window` 無條件進位。例如 `pap,concurrency=4096` 用 64 個 socket，`tacacs-pap,concurrency=32` 用 32 個。

`KIND` 的全部種類。`--help` 不會列出它們，清單來自 `rradius-bench/src/packet.rs` 的 `Step::name` 加上 `session`；「對核心實測」一欄是對本章的驗證環境各送一次的結果：

| 種類 | 送什麼 | 對核心實測 |
| --- | --- | --- |
| `pap` | Access-Request，`User-Password` | accept |
| `chap` | `CHAP-Password` 與 `CHAP-Challenge` | accept |
| `chap-authenticator` | `CHAP-Password`，以 Request Authenticator 當 challenge，不帶 `CHAP-Challenge` | accept |
| `mschapv2` | `MS-CHAP-Challenge` 與 `MS-CHAP2-Response` | accept |
| `acct-start`、`acct-alive`、`acct-stop` | Accounting-Request，`Acct-Status-Type` 1、3、2 | accounting |
| `acct-on`、`acct-off` | `Acct-Status-Type` 7、8 | accounting |
| `status-auth`、`status-acct` | Status-Server，送到認證埠或計費埠 | accept、accounting |
| `session` | 同一個 session ID 依序送登入（`pap`）、Start、`alive` 個 Interim-Update、Stop，前一步有回應才送下一步；登入被拒就不送計費 | 各步驟分開報告 |
| `eap-tls` | EAP-TLS 對話，要 `--cert-file`、`--key-file` | reject（驗證環境沒有 `certificate_query`，日誌 `not authorized`） |
| `peap` | PEAP 對話，內層 MS-CHAPv2 | accept |
| `ttls` | EAP-TTLS 對話，內層 PAP | accept |
| `ttls-mschapv2` | EAP-TTLS 對話，內層 MS-CHAPv2 | accept |
| `aka-prime` | EAP-AKA' 對話，`--password KI:OPC` | reject（核心不提供這個方法，日誌 `the peer asked only for methods this server does not offer`） |
| `tacacs-pap` | TACACS+ PAP 認證 | accept |
| `tacacs-chap` | TACACS+ CHAP 認證（伺服器要有能比對密碼的驗證鏈，否則伺服器回 `RESTART`，算 `invalid`） | accept（有驗證鏈時） |
| `tacacs-ascii` | TACACS+ ASCII 登入，伺服器逐步要帳號與密碼（實測一次登入 `rounds` 3.0） | accept |
| `tacacs-author` | TACACS+ 授權 | reject（驗證環境沒有授權規則） |
| `tacacs-acct` | TACACS+ 計費 | accounting |

EAP 負載的 `concurrency`、`rate`、`--requests` 以「對話」計算；`tacacs-ascii` 以「登入」計算。

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load peap,concurrency=1 --requests 1 \
    --ca-file certs/ca.pem --tls-name radius.corp.example.com
measured 0.003 s after 0.000 s of warmup

peap (fixed-concurrency): concurrency 1, 1 threads, 1 sockets, busiest worker 15%
  peap: 382/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    rounds per finished conversation: 10.0
    latency us: p50 2578.4, p90 2578.4, p99 2578.4, p99.9 2578.4, max 2578.4
$ rradius-bench run --server 127.0.0.1 --tacacs-port 4949 --secret "$(cat tacacs.secret)" \
    --user alice --password alice-password --load tacacs-ascii,concurrency=1 --requests 1
measured 0.000 s after 0.000 s of warmup

tacacs-ascii (fixed-concurrency): concurrency 1, 1 threads, 1 sockets over tcp, busiest worker 12%
  tacacs-ascii: 2542/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    rounds per finished conversation: 3.0
    latency us: p50 391.2, p90 391.2, p99 391.2, p99.9 391.2, max 391.2
```

## run 的輸出與結束碼

文字報告的結構：

- 第一行 `measured <秒> s after <秒> s of warmup`。
- 每個負載一段：種類、模式（`fixed-concurrency` 或 `fixed-rate`）、`concurrency`、`rate`、執行緒與 socket 數；非 UDP 時加 `over tcp` 或 `over tls`；固定並行時有 `busiest worker`，固定速率時有 `send lag p99`。
- 每個步驟一行計數：`sent`、`accept`、`reject`、`challenge`、`accounting`、`timeouts`、`invalid`、`late`、`socket errors`、`retransmits`，再一行延遲（微秒）的 p50、p90、p99、p99.9、max。EAP 與 `tacacs-ascii` 多一行 `rounds per finished conversation`。
- 有 `--metrics-url` 時多一段 `server counters: …`，有警告時多一段 `warnings:`。

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,rate=50 --warmup 0.5 --duration 1 \
    --metrics-url http://127.0.0.1:9812/metrics
measured 1.000 s after 0.500 s of warmup

pap (fixed-rate): concurrency 256, rate 50/s, 1 threads, 4 sockets, send lag p99 89.0 us
  pap: 50/s, sent 50, accept 50, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    latency us: p50 383.7, p90 561.7, p99 604.2, p99.9 604.2, max 604.2

server counters: auth 49, acct 0, duplicates 0, discarded overload 0, unknown_client 0, invalid 0
```

`--json` 改印一份 JSON：頂層有 `schema`（目前是 1）、`warmup_s`、`duration_s`、`trustworthy`、`loads`、`server_metrics`、`warnings`；每個負載有 `kind`、`mode`、`transport`、`concurrency`、`rate`、`threads`、`sockets`、`reconnects`、`send_lag_us`、`worker_busy_max`、`steps`；每個步驟有 `step`、`sent`、`responses`（`accept`、`reject`、`challenge`、`accounting`）、`timeouts`、`invalid`、`late`、`socket_errors`、`retransmits`、`throughput_per_s`、`rounds`、`latency_us`（`p50`、`p90`、`p99`、`p999`、`max`）。

警告代碼：

| 代碼 | 什麼時候出現 |
| --- | --- |
| `tool_saturated` | 固定並行負載的工作執行緒，量測期間至少 90% 的時間在忙：量到的是工具的上限 |
| `rate_not_met` | 固定速率負載送出的不到排定的 99%，或 `send_lag_us` 的 p99 超過 1000 |
| `duplicate_path` | `--retries 0` 時伺服器的重複請求計數增加了 |
| `server_discarded_<原因>` | 伺服器丟棄封包的計數增加了，例如 `server_discarded_overload` |
| `metrics_unavailable` | `--metrics-url` 讀不到；量測照常完成 |

`tool_saturated`、`rate_not_met`、`duplicate_path` 這三種會讓 `trustworthy` 變成 false，文字報告最後印 `these numbers do not describe the server alone: see the warnings`。警告與 `late` 都不影響結束碼。

結束碼：

| 結束碼 | 意思 |
| --- | --- |
| 0 | 量到的請求全部是 Accept，或計費都有回應 |
| 1 | 有 Reject 或 Challenge，沒有逾時或錯誤 |
| 2 | 有逾時、無效的回應或 socket 錯誤 |
| 3 | 參數錯誤或沒能開始（例如名稱解析失敗），沒有結果；不認得的命令、少了必填旗標也是 3 |

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 1899 --secret x --load pap,concurrency=1 --requests 1 --timeout 0.5
measured 0.503 s after 0.000 s of warmup

pap (fixed-concurrency): concurrency 1, 1 threads, 1 sockets, busiest worker 0%
  pap: 0/s, sent 1, accept 0, reject 0, challenge 0, accounting 0, timeouts 1, invalid 0, late 0, socket errors 1, retransmits 0
    latency us: p50 0.0, p90 0.0, p99 0.0, p99.9 0.0, max 0.0
$ echo $?
2
```

這個例子故意用沒有人在聽的埠 1899。`rradius-bench --help`、`-h`、`--version`、`-V` 的結束碼是 0；不帶任何參數時印出命令清單，結束碼 3。

## rradius-bench calibrate

```text
rradius-bench calibrate [--duration <秒>] [--json]
```

| 旗標 | 預設 | 意思 |
| --- | --- | --- |
| `--duration` | `3` | 每種組合量幾秒，要大於 0 |
| `--json` | 關 | 改印 JSON：`schema` 1 與四筆 `results` |

對工具自己行程裡、會正確簽章回應的回應者送負載，用的是與 `run` 相同的程式碼。量四種組合：1 個執行緒、window 1、concurrency 1；以及 window 64 時 1、2、4 個執行緒。結果是這台機器上這個工具能送多快，`run` 量到接近這個數字時，瓶頸可能在工具。

```console
$ rradius-bench calibrate --duration 0.5
what this tool can do here, against responders that do no other work:
  1 threads, window 1, concurrency 1: …/s, p50 … us, p99 … us, busiest worker …%, timeouts 0
  1 threads, window 64, concurrency 64: …
  2 threads, window 64, concurrency 128: …
  4 threads, window 64, concurrency 256: …
```

數字依機器而不同，這裡省略。結束碼 0；`--duration 0` 印 `rradius-bench: --duration must be above 0`，結束碼 3。

---

## 比對方法

程式更新後，用下面的方法確認這份參考沒有漏掉或多出命令與旗標。2026-11-23 對本附錄執行的結果：每一項差異都是 0。

**1. `rradiusd` 的命令與旗標。** `help` 的輸出已經包含每個子命令的用法，從中取出「命令 子命令」與旗標，逐一檢查在本附錄出現：

```console
$ rradiusd help > help.txt
$ grep -oE -- '--[a-z]+(-[a-z]+)*' help.txt | sort -u > flags-help.txt
$ grep -oE -- '--[a-z]+(-[a-z]+)*' 91-command-reference.md | sort -u > flags-book.txt
$ comm -23 flags-help.txt flags-book.txt          # help 有、書沒有：要是空的
$ sed -n 's/^usage: rradiusd \([a-z-]*\) \([a-z|]*\).*/\1 \2/p' help.txt \
    | awk '{n=split($2,a,"|"); for(i=1;i<=n;i++) print $1" "a[i]}' \
    | grep -v ' $' \
    | while read c; do grep -qF "\`$c" 91-command-reference.md || echo "missing: $c"; done
```

第二段產生 `config show`、`user add`、`cluster remove` 這類組合，沒有輸出才算通過。

**2. 書裡多出來的旗標。** `comm -13 flags-help.txt flags-book.txt` 列出書裡有、`rradiusd help` 沒有的旗標。這些應該全部來自 `rradius-bench` 的 `--help`、舊的旗標 `--check`，以及 `--help`。把 `rradius-bench --help`、`rradius-bench run --help`、`rradius-bench calibrate --help` 的旗標也加進 `flags-help.txt` 再比，剩下的只能是 `--check`。

**3. `rradius-bench` 的旗標與預設值。** `rradius-bench run --help` 每個旗標都要在 [rradius-bench run](#rradius-bench-run) 的表裡，`[default: …]` 要與表的「預設」一致；`calibrate --help` 同理。

**4. `--load` 的種類。** `--help` 不列種類，要從原始碼取：

```console
$ sed -n '/pub fn name/,/^    }/p' rradius-bench/src/packet.rs | grep -oE '"[a-z0-9-]+"' | tr -d '"' > kinds.txt
$ echo session >> kinds.txt
$ while read k; do grep -qF "\`$k\`" 91-command-reference.md || echo "missing: $k"; done < kinds.txt
```

目前是 20 種。

**5. 企業版的命令。** 從 `extension.rs` 的 `COMMANDS_NEEDING_ENTERPRISE` 取出名稱，對核心逐一執行，每一個都要得到 `not supported by this program`、結束碼 2，而且出現在[只有企業版才有的命令](#只有企業版才有的命令)的表裡。

結束碼不在 `help` 裡，要對照各子命令的原始碼（`return`、`exit_code`、`STATUS_*` 常數）。

## 常見錯誤

**症狀：** 子命令後面加 `--help`，例如 `rradiusd user --help`，印出用法，結束碼 2。
**原因：** 子命令沒有 `--help`，`--help` 被當成不認得的參數。
**解法：** 用 `rradiusd help` 看全部用法；用法本身不是錯誤。

**症狀：** `rradiusd --config rradius.toml` 沒有啟動，印出整份用法與 `to start the server: …`。
**原因：** 舊的寫法，現在啟動是子命令。
**解法：** 寫成 `rradiusd serve --config rradius.toml`。

**症狀：** `rradiusd config show --config a.toml --lang zh-TW` 印出 `config` 的用法，結束碼 2。
**原因：** 只有 `config init` 接受 `--lang`。
**解法：** `config show` 不加 `--lang`。

**症狀：** 腳本用 `query <name>` 的結束碼判斷有沒有資料，永遠是 0。
**原因：** 查無資料列時 `query` 印 `no rows`，結束碼仍是 0。
**解法：** 判斷輸出，或改用 `--json` 檢查是不是 `[]`。

**症狀：** `user`、`session`、`cluster` 停 30 秒後印出 `the cluster did not answer within 30 s`，結束碼 3。
**原因：** 伺服器沒在跑、`[[cluster.nodes]]` 的 API 位址不對，或 API 密碼與叢集不一致。遇到密碼不一致時，命令會直接說 `refused the api secret`，結束碼 2，不會等到逾時；只有連不上或沒有回答才會等 30 秒。
**解法：** 確認伺服器在執行，命令用的設定檔與伺服器相同。

**症狀：** `rradius-bench` 的結果都是 `timeouts`，伺服器日誌有 `discarded 1 Access-Requests lacking a Message-Authenticator`。
**原因：** 用了 `--no-message-authenticator`，而伺服器預設要求 Access-Request 帶 Message-Authenticator。
**解法：** 拿掉這個旗標；要測不帶的 NAS，先看[附錄 A](90-config-reference.md) 的 `radius.clients[].require_message_authenticator`。

**症狀：** `rradius-bench run … --requests 1 --duration 3` 印出 clap 的用法，結束碼 3。
**原因：** `--requests` 不能與 `--warmup`、`--duration` 同時使用。
**解法：** 擇一。

**症狀：** `eap-tls` 負載得到 `reject 1`，伺服器日誌是 `the EAP Identity does not fit the certificate`。
**原因：** `--user` 送出的 EAP Identity 與用戶端憑證的名稱不符。
**解法：** `--user` 寫成憑證上的名稱，例如 wifi-certs 情境的 `laptop-001.corp.example.com`。

## 相關章節

- [03 第一次啟動與登入](03-quick-start.md)：`serve`、`user`、`session`、`status` 的第一次使用
- [04 設定檔的基本功](04-configuration.md)：`check`、`config`、`schema`、reload
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)：`user` 與 `[accounts]`
- [09 外部資料庫與具名查詢](09-external-databases.md)：`query` 與 `[queries]`
- [13 計費與 session](13-accounting.md)：`session` 與 `[sessions]`
- [16 叢集與高可用](16-cluster.md)：`cluster members`、`cluster remove`
- [17 監控、狀態與日誌](17-monitoring.md)：`status`
- [19 效能調校與壓測](19-performance.md)：`tuning`、`rradius-bench`
- [附錄 A：設定參考](90-config-reference.md)
- [附錄 E：術語與 RFC 對照](94-glossary.md)
