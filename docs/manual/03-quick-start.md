# 03 第一次啟動與登入

> 本章讓你做到：
> - 用 quick-start 情境在十分鐘內啟動 rradius
> - 用 `rradius-bench` 送一筆登入，拿到第一個 Access-Accept
> - 用 `user add` 新增帳號，馬上登入
> - 送一筆計費，用 `session list` 看到它寫進資料表
> - 用 `status` 看伺服器狀態，再乾淨地停止它

前置條件：照[第 02 章](02-install.md)的「從原始碼建置」建好 `rradiusd` 與 `rradius-bench`。本章全部在一台機器上做，不需要真的 NAS。

本章小節：

- [準備工作目錄](#準備工作目錄)
- [看懂 quick-start 的檔案](#看懂-quick-start-的檔案)
- [換掉範例密碼](#換掉範例密碼)
- [讓 user 指令能用](#讓-user-指令能用)
- [檢查設定](#檢查設定)
- [啟動伺服器](#啟動伺服器)
- [第一次登入](#第一次登入)
- [新增帳號](#新增帳號)
- [送一筆計費](#送一筆計費)
- [看伺服器狀態](#看伺服器狀態)
- [停止伺服器](#停止伺服器)
- [另一個起點：config init](#另一個起點config-init)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

## 準備工作目錄

quick-start 是一個單機情境：帳號放在叢集資料庫的資料表裡，用 sql 處理器驗證密碼，計費由 Lua 腳本寫進資料表。它在原始碼的 `rradius-server/examples/scenarios/quick-start/`。

不要在原始碼目錄裡直接跑它：伺服器會在設定檔旁邊建立 `data/` 目錄，下面的步驟也會改動設定檔。先複製一份到工作目錄。以下的指令在原始碼 repo 的根目錄執行：

```console
$ export PATH="$PWD/target/release:$PATH"
$ mkdir -p ~/rradius-lab
$ cp -R rradius-server/examples/scenarios/quick-start ~/rradius-lab/
$ sed -i.bak "s#^dictionary = .*#dictionary = \"$PWD/share/dictionary\"#" ~/rradius-lab/quick-start/rradius.toml
$ cd ~/rradius-lab/quick-start
```

- 第一行讓本章的 `rradiusd`、`rradius-bench` 指令直接可用。
- 情境原本的 `dictionary = "../../../../share/dictionary"` 是相對於 repo 裡的位置。複製出來之後這個相對路徑不成立，所以 `sed` 把它改成字典的絕對路徑。
- 用 deb 或 rpm 安裝的話，範例在 `/usr/share/doc/rradius/examples/scenarios/quick-start/`，字典改成 `/usr/share/rradius/dictionary`。不過套件裡沒有 `rradius-bench`，登入測試還是要用原始碼建出來的那一個。

本章之後的指令都在 `~/rradius-lab/quick-start` 裡執行。

## 看懂 quick-start 的檔案

```console
$ ls -1
accounting.lua
api.secret
localhost.secret
migrations
raft.secret
rradius.toml
rradius.toml.bak
```

| 檔案 | 內容 |
| --- | --- |
| `rradius.toml` | 設定檔（`rradius.toml.bak` 是 `sed` 留下的原檔） |
| `accounting.lua` | 計費腳本：每個 session 一列，Start 新增，Interim-Update 與 Stop 更新 |
| `migrations/1_schema.sql` | 建立 `users`、`user_reply`、`accounting` 三個表 |
| `migrations/2_demo_users.sql` | 示範帳號與回覆屬性 |
| `raft.secret`、`api.secret` | 叢集的 Raft 密碼與 API 密碼 |
| `localhost.secret` | 用戶端 `localhost` 的共用密碼 |

設定檔的內容（拿掉註解）：

```toml
# rradius.toml
dictionary = "…/share/dictionary"

[lua]
script = "accounting.lua"

[cluster]
data_dir = "data"
secret_raft_file = "raft.secret"
secret_api_file = "api.secret"
migrations_dir = "migrations"

[auth]
policy = "first_match"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ? AND enabled = 1"
reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"

[radius]
bind = "127.0.0.1"
auth_port = 1812
acct_port = 1813
status_server = true

[[radius.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "localhost.secret"

[metrics]
bind = "127.0.0.1"
port = 9812

[sessions]
list = "SELECT username, nas, session_id, started, updated FROM accounting WHERE stopped IS NULL AND updated > strftime('%s', 'now') - 900 ORDER BY started, session_id"
show = "SELECT username, nas, session_id, started, updated FROM accounting WHERE username = ?1 AND stopped IS NULL AND updated > strftime('%s', 'now') - 900 ORDER BY started, session_id"
```

逐段說明（預設值與能否 reload 的完整列表見[附錄 A](90-config-reference.md)）：

| 設定 | 這裡的值 | 預設值 | reload | 意思 |
| --- | --- | --- | --- | --- |
| `dictionary` | 字典路徑 | 預設字典路徑 | 要重啟 | 屬性名稱的來源（第 06 章） |
| `lua.script` | `accounting.lua` | 必填 | 可以 | 最上層腳本。這裡有 `[auth]`，所以它只處理計費 |
| `cluster.data_dir` | `data` | 必填 | 要重啟 | 叢集資料庫的目錄，相對於設定檔 |
| `cluster.secret_raft_file`、`secret_api_file` | 檔名 | 無 | 要重啟 | 叢集密碼放在哪個檔案 |
| `cluster.migrations_dir` | `migrations` | 無 | 要重啟 | 啟動時依序套用的 SQL 檔 |
| `auth.policy` | `first_match` | `first_match` | 可以 | 驗證鏈怎麼決定（第 07 章） |
| `auth.handlers` | 一個 sql 處理器 | 無 | 可以 | `user_query` 查密碼，`reply_query` 查回覆屬性 |
| `radius.bind` | `127.0.0.1` | `0.0.0.0` | 要重啟 | RADIUS 監聽的位址 |
| `radius.auth_port`、`acct_port` | `1812`、`1813` | `1812`、`1813` | 要重啟 | 認證埠、計費埠 |
| `radius.status_server` | `true` | `true` | 要重啟 | 回應 Status-Server |
| `radius.clients` | `127.0.0.1` | 無 | 可以 | 允許送請求的用戶端（第 05 章） |
| `metrics.bind`、`port` | `127.0.0.1`、`9812` | `127.0.0.1`、`9812` | 要重啟 | 指標埠，`status` 讀它 |
| `sessions.list`、`show` | 兩段 SQL | 無 | 只有指令讀，不必 reload | `session list`、`session show` 執行的查詢（第 13 章） |

示範帳號在 `migrations/2_demo_users.sql`：

| 帳號 | 密碼 | 狀態 | 回覆屬性 |
| --- | --- | --- | --- |
| `alice` | `alice-password`（明文存放） | 啟用 | `Session-Timeout = 3600`、`Filter-Id = staff` |
| `bob` | `bob-password`（存的是 bcrypt 雜湊） | 啟用 | `Reply-Message = Welcome, bob` |
| `carol` | `carol-password` | 停用（`enabled = 0`） | 無 |

`carol` 停用了，`user_query` 有 `AND enabled = 1`，所以查不到她，等同帳號不存在。

> **警告：** `2_demo_users.sql` 的帳號與密碼是公開的示範值。拿這個情境改成正式環境時，一定要刪掉這個檔案，改用你自己的資料。

## 換掉範例密碼

情境附的三個 `.secret` 檔是公開的示範值，權限也太寬。先看看 `check` 怎麼說：

```console
$ rradiusd check --config rradius.toml
… WARN rradius_core::common::seal: cluster.secret_raft_file: 'raft.secret' has mode 644, which others can reach or its group can write; make it 0600 or 0640
… WARN rradius_core::config: cluster.secret_raft is the public value shipped with the examples; replace it with a random one, for example from `openssl rand -hex 32`
…
… WARN rradius_core::reload: RADIUS client 'localhost' has the public shared secret shipped with the examples; replace it with a random one, for example from `openssl rand -hex 32`
configuration ok
```

這些只是警告，設定本身是好的。照它說的換掉：

```console
$ for f in raft api localhost; do openssl rand -hex 32 > $f.secret; done
$ chmod 600 *.secret
```

## 讓 user 指令能用

`user add` 這類指令要知道對資料表執行哪一段 SQL，這些 SQL 寫在 `[accounts]`。quick-start 沒有這一節，所以現在執行 `user add` 會得到 `accounts.add is not set in rradius.toml`。

quick-start 的設定檔也沒有 `include`，所以不能放進 `conf.d/`，要直接加在 `rradius.toml` 的最後面：

```toml
# rradius.toml（加在檔案最後）
[accounts]
scheme = "bcrypt"
add = "INSERT INTO users (username, password) VALUES (?1, ?2)"
passwd = "UPDATE users SET password = ?2 WHERE username = ?1"
enable = "UPDATE users SET enabled = 1 WHERE username = ?1"
disable = "UPDATE users SET enabled = 0 WHERE username = ?1"
delete = "DELETE FROM users WHERE username = ?1"
show = "SELECT username, enabled FROM users WHERE username = ?1"
```

- `?1` 是帳號名稱，`?2` 是要存進資料表的密碼值。
- `scheme` 是 `user add` 與 `user passwd` 存密碼的方式，預設就是 `bcrypt`，這裡寫出來只是讓你看見。其他選擇與取捨見[第 07 章](07-users-and-chains.md)。
- `show` 不要選密碼欄。

`[accounts]` 只有 `user` 指令會讀，每次執行指令時讀一次，改了不必 reload 伺服器。

## 檢查設定

```console
$ rradiusd check --config rradius.toml
configuration ok
```

`check` 會讀設定檔、字典、腳本與祕密檔，檢查完就結束，不開任何埠，也不建立資料目錄。成功時結束碼 0；有錯時印出原因，結束碼 2。

## 啟動伺服器

開一個終端機，在前景啟動：

```console
$ rradiusd serve --config rradius.toml
… WARN hiqlite::store::state_machine::sqlite::writer: No metadata exists inside the DB yet
… WARN hiqlite::store::state_machine::sqlite::writer: No metadata exists inside the DB yet
… INFO rradius_core::cli: cluster node 1 started
… INFO rradius_core::cli: cluster node 1 has caught up with the cluster
… INFO rradius_core::cli: Servers started. Press Ctrl+C to stop.
… INFO rradius_core::radius: RADIUS Server listening on 127.0.0.1:1812 and 127.0.0.1:1813
… INFO rradius_core::metrics: Metrics listening on 127.0.0.1:9812
```

最後兩行的順序每次可能不同。這段過程中發生了：

1. 建立 `data/` 目錄，啟動叢集節點 1。第一次啟動時，資料庫是空的，所以 hiqlite 會印兩行 `No metadata exists inside the DB yet`，這是正常的。
2. 依檔名順序套用 `migrations/` 裡的 SQL 檔，建立資料表、寫入示範帳號。之後再啟動時，已經套用過的檔案不會再跑。
3. 開啟指標埠與 RADIUS 的兩個埠。看到 `Servers started` 就可以送請求了。

伺服器留在這個終端機。**再開一個終端機**，`cd ~/rradius-lab/quick-start`，做接下來的步驟。新的終端機要重新設定 `PATH`，這時你不在 repo 裡，所以寫 repo 的絕對路徑：`export PATH="<repo的路徑>/target/release:$PATH"`。

確認伺服器已經就緒：

```console
$ curl -s http://127.0.0.1:9812/readyz
ok
```

`/readyz` 回 `ok`（HTTP 200）表示可以處理請求。`/livez` 也一樣回 `ok`。兩者的差別見[第 17 章](17-monitoring.md)。

## 第一次登入

用 `rradius-bench` 當 NAS，以 PAP 送一筆 Access-Request：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
measured 0.001 s after 0.000 s of warmup

pap (fixed-concurrency): concurrency 1, 1 threads, 1 sockets, busiest worker 13%
  pap: 1092/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    latency us: p50 802.8, p90 802.8, p99 802.8, p99.9 802.8, max 802.8
```

看 `accept 1`：這就是你的第一個 Access-Accept。

指令的參數：

- `--server`、`--secret`：伺服器位址與共用密碼。共用密碼會出現在行程清單裡，只用在測試環境。
- `--user`、`--password`：登入的帳號與密碼。
- `--load pap,concurrency=1`：送 PAP 登入，同時只有一個請求在路上。`--load` 一定要帶 `concurrency=` 或 `rate=`。
- `--requests 1`：送一個就停。

`rradius-bench` 本來是壓測工具（第 19 章），所以會印出速率與延遲。當登入測試用時，只要看 `accept`、`reject`、`timeouts` 這幾個數字。它的結束碼也能直接拿來判斷：

| 結束碼 | 意思 |
| --- | --- |
| 0 | 全部都是 Accept（或計費都有回應） |
| 1 | 有 Reject 或 Challenge |
| 2 | 有逾時、無效的回應或 socket 錯誤 |
| 3 | 參數錯誤，沒有送出請求 |

再試幾種情況：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password wrong --load pap,concurrency=1 --requests 1
…
  pap: 3401/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user carol --password carol-password --load pap,concurrency=1 --requests 1
…
  pap: 4508/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user bob --password bob-password --load pap,concurrency=1 --requests 1
…
  pap: 20/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

- 密碼錯誤：`reject 1`。
- `carol` 停用中：`reject 1`。
- `bob` 存的是 bcrypt 雜湊，比對要花比較多時間，所以速率特別低，但結果是 `accept 1`。

回到跑伺服器的終端機，每次登入都有一行認證紀錄：

```text
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="accept"
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="reject"
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="carol" account="carol" chain="default" handler="none" decision="reject"
```

- `client` 是比對到的用戶端名稱，`chain="default"` 是 `[auth]` 那條驗證鏈。
- `handler="sql"` 表示 sql 處理器做出了決定；`carol` 查不到帳號，沒有處理器給出答案，所以是 `handler="none"`。

> **注意：** `rradius-bench` 不會印出回覆屬性。要確認 `alice` 拿到了 `Session-Timeout`、`Filter-Id`，可以用 `[log.trace]` 記下完整的請求與回應（第 17 章）。

## 新增帳號

`user add` 從標準輸入讀密碼。在終端機上直接執行時，它會關掉回顯、問兩次；用管線餵進去時只讀第一行：

```console
$ printf 'dave-password\n' | rradiusd user add --config rradius.toml dave
added dave
$ rradiusd user show --config rradius.toml dave
username=dave
enabled=1
```

馬上就能登入，不必重新載入伺服器：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user dave --password dave-password --load pap,concurrency=1 --requests 1
…
  pap: 25/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

停用再啟用：

```console
$ rradiusd user disable --config rradius.toml dave
disabled dave
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user dave --password dave-password --load pap,concurrency=1 --requests 1
…
  pap: 4187/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradiusd user enable --config rradius.toml dave
enabled dave
```

`user` 指令透過叢集的 API 埠寫入資料表，所以伺服器一定要在執行中。指令的結束碼：0 是成功；1 是沒有符合的帳號或 SQL 失敗；2 是參數或設定錯誤；3 是連不到叢集。`user` 指令的完整說明在[第 07 章](07-users-and-chains.md)。

## 送一筆計費

用 `acct-start` 送一筆 Accounting-Request（Start）：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --load acct-start,concurrency=1 --requests 1
measured 0.001 s after 0.000 s of warmup

acct-start (fixed-concurrency): concurrency 1, 1 threads, 1 sockets, busiest worker 3%
  acct-start: 1598/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    latency us: p50 610.8, p90 610.8, p99 610.8, p99.9 610.8, max 610.8
```

`accounting 1` 表示收到了 Accounting-Response：`accounting.lua` 把這筆寫進 `accounting` 表，回傳 `accept`，伺服器才回應。

用 `session list` 看在線的 session：

```console
$ rradiusd session list --config rradius.toml
username=alice
nas=192.0.2.1
session_id=2110000000000000
started=1795365534
updated=1795365534
```

- `nas` 是 NAS 在請求裡報的 `NAS-IP-Address`。`rradius-bench` 預設用 `192.0.2.1`。
- `session_id` 是 `rradius-bench` 產生的 Acct-Session-Id。每次執行 `rradius-bench run` 都會產生新的 session ID。
- `started`、`updated` 是 Unix 時間。
- 只看某個使用者：`session show --config rradius.toml alice`。要 JSON 格式就加 `--json`。

「在線」的條件由 `[sessions]` 的 SQL 決定：這個情境把「還沒有 Stop、而且 15 分鐘內有更新」算成在線。所以這筆 Start 在 15 分鐘後會自己從清單上消失，就算沒有收到 Stop 也一樣。

要看一個 session 完整的生命週期，用 `session` 這種負載。它依序送登入、Start、Stop，三個都用同一個 session ID：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user dave --password dave-password --load session,concurrency=1 --requests 1
…
  pap: 26/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
  acct-start: 26/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
  acct-stop: 26/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradiusd session show --config rradius.toml dave
no sessions for dave
```

Stop 之後這個 session 就不在清單上了，`session show` 的結束碼是 1。

> **注意：** 分開執行 `acct-start` 與 `acct-stop` 兩次，Stop 帶的是另一個 session ID，結束不了前一個 Start 建立的 session。要測試 Start 到 Stop，用 `session` 負載。

計費的資料表怎麼設計、腳本怎麼寫、資料庫故障時的計費 spool，都在[第 13 章](13-accounting.md)。

## 看伺服器狀態

`status` 讀指標埠，整理成一頁：

```console
$ rradiusd status --config rradius.toml
rradius status of 127.0.0.1:9812 over 1.0 s

Node
  Ready                     ok
  Leader                    1
  Has a leader              1
  Caught up                 1
  Applied index             11
  Seconds without a leader  0
  Version                   0.1.0
  Seconds running           12

…

RADIUS
  Auth requests              0.0/s (7)
  Acct requests              0.0/s (3)
  Access-Accept              0.0/s (4)
  Access-Reject              0.0/s (3)
  Access-Challenge           0.0/s (0)
  Accounting-Response        0.0/s (3)
  Duplicates                 0.0/s (0)
  Discarded: overload        0.0/s (0)
  Discarded: unknown client  0.0/s (0)
  Discarded: invalid         0.0/s (0)
  Discarded: database        0.0/s (0)
  Discarded: no leader       0.0/s (0)
…
```

- `Node` 這一段：`Ready ok` 表示就緒；單機時 `Leader` 就是自己（節點 1）。
- `RADIUS` 這一段：括號裡是啟動以來的總數，前面是這一秒的速率。
- `Discarded` 開頭的幾行是沒有回應的請求，排錯時最先看這裡。例如共用密碼不一致，`Discarded: invalid` 會增加。

`status` 還有 `--watch`（持續更新）與 `--json`，每一行的意思見[第 17 章](17-monitoring.md)。

## 停止伺服器

在跑伺服器的終端機按 Ctrl+C，或從另一個終端機送 SIGTERM。伺服器的日誌：

```text
… INFO rradius_core::cli: stopping: /readyz answers 503; requests are answered for 0 ms more, then those being handled are waited for, for at most 5000 ms
… INFO rradius_core::cli: stopping: no more requests taken
… INFO rradius_core::cli: stopping: every request taken was answered
… WARN hiqlite::store::state_machine::sqlite::writer: SQL writer is shutting down
```

停止的順序：`/readyz` 先改回 503；不再接新請求；等處理中的請求做完，最多等 5000 毫秒；關閉資料庫。等待的時間由 `[shutdown]` 決定，詳細見[第 04 章](04-configuration.md)。停止時再按一次 Ctrl+C，就不等處理中的請求。

> **注意：** 讓伺服器自己關閉資料庫再結束。用 `kill -9` 強制結束的節點，下次啟動時可能要重建本機的資料庫。

再啟動一次，資料都還在：`migrations/` 不會重複套用，`dave` 也還能登入。

想從頭再來，停止伺服器後刪掉 `data/`：

```console
$ rm -rf data
```

下次啟動時，資料庫重新建立，migrations 重新套用，帳號回到示範的三個。

## 另一個起點：config init

quick-start 是一份寫好的情境。如果你想從空白開始，用 `config init` 產生底稿：

```console
$ mkdir ~/rradius-lab/blank && cd ~/rradius-lab/blank
$ rradiusd config init --lang zh-TW > rradius.toml
```

這份底稿的內容：

- 叢集密碼與一個範例用戶端（`127.0.0.1`）的共用密碼，都是隨機產生的。
- `dictionary` 指向 `/usr/share/rradius/dictionary`，也就是套件安裝的位置。
- `include = ["conf.d/*.toml"]`：旁邊 `conf.d/` 的 `.toml` 檔會一起讀進來。
- 每一節都附上說明，被註解掉的設定列出預設值。

它沒有 migrations、沒有 `[auth]`、沒有 `[accounts]`，`[lua]` 指向一個還不存在的 `auth.lua`。所以直接 `check` 會失敗，要先：

1. 把 `dictionary` 改成字典實際的位置。
2. 建立 `auth.lua`。
3. 單機時在 `[cluster]` 加 `listen = "127.0.0.1"`，否則會看到 `cluster.listen is not set` 的警告。

從這份底稿長出完整設定的做法，見[第 04 章](04-configuration.md)與[第 07 章](07-users-and-chains.md)。

## 常見錯誤

**症狀：** `rradius-bench` 印出 `rradius-bench: --load pap: needs concurrency or rate`，結束碼 3。
**原因：** `--load` 只寫了種類。
**解法：** 寫成 `--load pap,concurrency=1`。

**症狀：** `rradius-bench` 的結果是 `timeouts 1`，伺服器日誌沒有認證紀錄。
**原因：** 伺服器丟掉了這個請求，最常見的是 `--secret` 和用戶端的共用密碼不一致；也可能是伺服器沒在跑，或位址、埠不對。
**解法：** 確認 `--secret "$(cat localhost.secret)"` 讀的是同一個檔案。共用密碼不一致時，`status` 的 `Discarded: invalid` 會增加。

**症狀：** `user add` 印出 `accounts.add is not set in rradius.toml`，結束碼 2。
**原因：** 設定檔沒有 `[accounts]`，或沒有 `add` 這一項。
**解法：** 照[讓 user 指令能用](#讓-user-指令能用)加上 `[accounts]`。

**症狀：** `user add` 印出 `Connect: Error code 1555: constraint failed Some("UNIQUE constraint failed: users.username")`，結束碼 1。
**原因：** 帳號已經存在。
**解法：** 要改密碼用 `user passwd`。

**症狀：** `user show` 印出 `no account nobody`，結束碼 1。
**原因：** 沒有這個帳號。
**解法：** 確認名稱；用 `session show` 時對應的訊息是 `no sessions for <名稱>`。

**症狀：** `user add` 停了 30 秒，最後印出 `the cluster did not answer within 30 s`，結束碼 3。等待時還有好幾行 `Error looking up DB metrics`。
**原因：** 伺服器沒在跑，指令連不到叢集的 API 埠。
**解法：** 先啟動伺服器。

**症狀：** `status` 印出 `cannot reach the metrics port at 127.0.0.1:9812: Connection refused (os error 61)`，結束碼 3。
**原因：** 伺服器沒在跑，或設定沒有 `[metrics]`。錯誤碼的數字依作業系統而不同。
**解法：** 先啟動伺服器，確認 `[metrics]` 存在。

**症狀：** `check` 印出 `cannot read dictionary: No such file or directory (os error 2)`。
**原因：** 複製情境之後沒有改 `dictionary`，相對路徑指到不存在的地方。
**解法：** 照[準備工作目錄](#準備工作目錄)改成字典的絕對路徑。

**症狀：** 用 `config init` 的底稿時，`check` 印出 `cannot read script 'auth.lua': No such file or directory (os error 2)`。
**原因：** 底稿的 `[lua]` 指向的腳本還沒建立。
**解法：** 建立 `auth.lua`。設定裡一定要有 `[lua]`，少了它會得到 `the [lua] section is required`。

**症狀：** `check` 印出 `at least one of [radius] or [tacacs] must be configured`。
**原因：** 設定檔裡 `[radius]` 與 `[tacacs]` 都沒有。
**解法：** 至少加一個（第 05 章、第 12 章）。

## 相關章節

- [02 安裝](02-install.md)
- [04 設定檔的基本功](04-configuration.md)
- [05 RADIUS 服務與 NAS 用戶端](05-radius-clients.md)
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)
- [10 用 Lua 寫政策](10-lua-policies.md)
- [13 計費與 session](13-accounting.md)
- [17 監控、狀態與日誌](17-monitoring.md)
- [19 效能調校與壓測](19-performance.md)
- [附錄 A：設定參考](90-config-reference.md)
