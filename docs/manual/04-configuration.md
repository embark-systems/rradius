# 04 設定檔的基本功

> 本章讓你做到：
> - 看懂設定檔的結構，知道哪些區段一定要有
> - 用 `include` 把設定拆成好幾個檔案，把祕密放進權限正確的檔案
> - 用 `check`、`config show`、`config diff`、`schema` 檢查與查看設定
> - 用 SIGHUP 重新載入（reload）設定，分得出哪些變更要重新啟動
> - 用 `[shutdown]` 決定伺服器怎麼停止

前置條件：做過[第 03 章](03-quick-start.md)，手上有能啟動的 `rradiusd`。本章的指令都在一個工作目錄裡做，不需要真的 NAS。

本章小節：

- [設定檔的結構](#設定檔的結構)
- [一份從零開始的設定](#一份從零開始的設定)
- [相對路徑](#相對路徑)
- [把設定拆成多個檔案](#把設定拆成多個檔案)
- [把祕密放進檔案](#把祕密放進檔案)
- [檢查設定：check](#檢查設定check)
- [看實際生效的設定：config show](#看實際生效的設定config-show)
- [比較兩份設定：config diff](#比較兩份設定config-diff)
- [查設定鍵的說明：schema](#查設定鍵的說明schema)
- [不停機套用變更：reload](#不停機套用變更reload)
- [停止伺服器：shutdown](#停止伺服器shutdown)
- [runtime 與 log](#runtime-與-log)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

## 設定檔的結構

rradius 只有一份主設定檔，格式是 TOML。指令用 `--config <路徑>` 指定它；不寫 `--config` 時讀 `/etc/rradius/rradius.toml`（`rradiusd --version` 的第二行會印出這個預設路徑）。

TOML 用 `[表名]` 開一個表，用 `[[表名]]` 在表陣列裡加一項，表裡是 `鍵 = 值`。rradius 的設定分成下面這些頂層的鍵與表，清單和 `rradiusd schema` 輸出的頂層 `properties` 一致：

| 鍵或表 | 一定要有 | 用途 | 主章 |
| --- | --- | --- | --- |
| `dictionary` | 是 | 字典主檔的路徑 | [06](06-dictionary.md) |
| `include` | | 一起讀進來的其他設定檔 | 本章 |
| `[lua]` | 是 | 最上層的 Lua 腳本與它的資源上限 | [10](10-lua-policies.md) |
| `[cluster]` | 是 | 內建的叢集資料庫；單機也要有 | [16](16-cluster.md) |
| `[radius]` | 與 `[tacacs]` 至少一個 | RADIUS 的監聽與用戶端 | [05](05-radius-clients.md) |
| `[tacacs]` | 與 `[radius]` 至少一個 | TACACS+ 的監聽與用戶端 | [12](12-tacacs.md) |
| `[auth]` | | 驗證鏈 | [07](07-users-and-chains.md) |
| `[accounts]` | | `user` 指令執行的 SQL | [07](07-users-and-chains.md) |
| `[sessions]` | | `session` 指令執行的 SQL | [13](13-accounting.md) |
| `[databases.<name>]`、`[queries.<name>]` | | 外部資料庫、具名查詢 | [09](09-external-databases.md) |
| `[metrics]`、`[log]` | | 指標與健康檢查的 HTTP 埠、日誌 | [17](17-monitoring.md) |
| `[runtime]` | | 工作執行緒的設定 | [19](19-performance.md) |
| `[shutdown]` | | 停止時的等待時間 | 本章 |

幾個一定要知道的規則：

- **每個表都不接受未知的鍵。** 打錯字時 `check` 會指出檔名、行號，以及這個表可以用的鍵，見[常見錯誤](#常見錯誤)。
- **少了必要的部分就不能啟動。** 沒有 `[lua]` 得到 `the [lua] section is required`，沒有 `[cluster]` 得到 `the [cluster] section is required`，`[radius]` 與 `[tacacs]` 都沒有時得到 `at least one of [radius] or [tacacs] must be configured`。
- **有區段才有那個服務。** 沒寫 `[tacacs]` 就不開 TACACS+ 的埠；沒寫 `[metrics]` 就沒有 `/metrics`、`/readyz`，`status` 指令也讀不到東西。
- **錯誤訊息不會印出值。** 型別不對的值可能是忘了加引號的密碼，所以訊息只寫行號和鍵名。

每個鍵的型別、預設值、能不能 reload，完整列在[附錄 A](90-config-reference.md)。本章每個表都會寫出預設值與 reload 的行為。

## 一份從零開始的設定

**目標：** 做出一份單機、能啟動、祕密都放在檔案裡的設定，本章其他小節都拿它來示範。

`rradiusd config init --lang zh-TW` 會印出一份每一節都附中文說明的底稿（被註解掉的設定列出預設值），密碼是隨機產生的。它很適合拿來查有哪些鍵，但有兩點要自己改：底稿把叢集密碼和範例用戶端的共用密碼直接寫在檔案裡，本書的做法是放進 `*_file` 指向的檔案（見[把祕密放進檔案](#把祕密放進檔案)）；底稿還列了一個 `[[cluster.nodes]]`，卻沒寫 `cluster.listen`，所以會出現 `cluster.listen is not set` 的警告。

下面這份是整理過的版本。建一個工作目錄，在裡面放這幾個檔案：

```toml
# rradius.toml
dictionary = "/usr/share/rradius/dictionary"   # 字典實際的位置，見第 06 章
include = ["conf.d/*.toml"]

[lua]
script = "auth.lua"

[cluster]
data_dir = "data"
secret_raft_file = "raft.secret"   # openssl rand -hex 32 > raft.secret
secret_api_file = "api.secret"     # openssl rand -hex 32 > api.secret

[metrics]
bind = "127.0.0.1"
port = 9812

[radius]
bind = "127.0.0.1"
auth_port = 1812
acct_port = 1813
```

```toml
# conf.d/10-clients.toml
[[radius.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "localhost.secret"   # openssl rand -hex 32 > localhost.secret
```

`auth.lua` 是最上層腳本。這份設定沒有 `[auth]`，所以登入完全由它決定。先放一個只認得一組示範帳號的腳本，腳本的寫法在[第 10 章](10-lua-policies.md)：

```lua
-- auth.lua
if protocol ~= 'radius' then
  return 'reject'
end
if phase ~= 'authentication' then
  return 'accept'
end
if get_attr('User-Name') == 'alice' and get_attr('User-Password') == 'alice-password' then
  return 'accept'
end
return 'reject'
```

產生祕密檔、收緊權限，再檢查：

```console
$ for f in raft api localhost; do openssl rand -hex 32 > $f.secret; done
$ chmod 600 *.secret
$ rradiusd check --config rradius.toml
configuration ok
```

這份設定沒有 `[[cluster.nodes]]`，所以是只有一個節點的叢集，Raft 與 API 兩個叢集埠用預設的 `127.0.0.1:8100`、`127.0.0.1:8200`，只聽本機。叢集的設定在[第 16 章](16-cluster.md)。

## 相對路徑

設定裡所有的路徑，寫成相對路徑時都以**主設定檔所在的目錄**為準，不是以你執行指令時所在的目錄為準。上面那份設定放在 `/etc/rradius/rradius.toml` 時：

| 設定 | 寫的值 | 實際讀的檔案 |
| --- | --- | --- |
| `lua.script` | `auth.lua` | `/etc/rradius/auth.lua` |
| `cluster.data_dir` | `data` | `/etc/rradius/data` |
| `cluster.secret_raft_file` | `raft.secret` | `/etc/rradius/raft.secret` |
| `radius.clients[].secret_file` | `localhost.secret` | `/etc/rradius/localhost.secret` |

寫成絕對路徑（以 `/` 開頭）就照原樣使用。

> **注意：** 被 `include` 讀進來的檔案也一樣：裡面的相對路徑以**主設定檔**的目錄為準，不是以那個檔案自己的目錄為準。上面的 `conf.d/10-clients.toml` 寫 `secret_file = "localhost.secret"`，讀的是主檔旁邊的 `localhost.secret`，不是 `conf.d/localhost.secret`。

## 把設定拆成多個檔案

**目標：** 用戶端、驗證鏈這類常改的設定各放一個檔案，主檔不必每次都動。這也方便用設定管理工具一個檔案一個檔案地部署。

**設定：** 主檔的頂層寫 `include`，列出要一起讀的檔案。`config init` 產生的底稿已經有這一行：

```toml
# rradius.toml（節錄）
include = ["conf.d/*.toml"]
# …（其他設定略）
```

`include` 沒有預設值，不寫就只讀主檔；它跟著主檔一起被讀，所以 reload 就套用。然後在 `conf.d/` 放檔案，每個檔案都要以一個表開頭：

```toml
# conf.d/20-lab.toml
[[radius.clients]]
name = "lab-net"
address = "192.0.2.0/24"
secret_file = "lab.secret"   # openssl rand -hex 32 > lab.secret
```

**驗證：**

```console
$ openssl rand -hex 32 > lab.secret && chmod 600 lab.secret
$ rradiusd check --config rradius.toml
configuration ok
$ rradiusd config show --config rradius.toml | grep -A3 'radius.clients'
[[radius.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "localhost.secret"
--
[[radius.clients]]
name = "lab-net"
address = "192.0.2.0/24"
secret_file = "lab.secret"
```

**結果長什麼樣子：** `config show` 把所有檔案的內容合在一起印出。兩個用戶端都在，`10-` 的在前、`20-` 的在後。

讀檔的規則（以 `config.rs` 為準）：

- 效果就像把被讀進來的檔案依序接在主檔後面，再當成一整份 TOML 讀。
- 名稱可以是單一檔案（`"clients.toml"`），也可以有萬用字元 `*`、`?`。萬用字元只能出現在最後一段的檔名裡，`"conf.d/*.toml"` 可以，`"*.d/*.toml"` 不行。
- 一個名稱比對到的檔案依路徑排序。所以用 `10-`、`20-` 這種數字開頭的檔名來控制順序。
- 萬用字元不會比對到以 `.` 開頭的檔名，編輯器留下的暫存檔不會被誤讀。
- 有萬用字元的名稱比對不到任何檔案（例如 `conf.d/` 是空的或不存在）不算錯誤；沒有萬用字元的名稱一定要是讀得到的檔案，否則失敗。
- 同一個檔案被兩個名稱比對到，只讀一次；主檔自己會被略過。
- 最多 1000 個檔案，每個檔案最多 1048576 位元組（1 MiB）。
- 被讀進來的檔案不能再寫 `include`，而且第一個不是空白或註解的行一定要是 `[表]` 或 `[[表陣列]]`。
- 每個檔案自己就要是完整的 TOML：一個檔案裡沒關上的字串，不會吃掉下一個檔案。
- **同一個表不能在兩個檔案裡各寫一次。** 主檔已經有 `[radius]`，`conf.d/` 的檔案就不能再寫 `[radius]`，否則得到 `duplicate key`。表陣列不受這個限制：`[[radius.clients]]` 可以分散在好幾個檔案裡，依檔案順序加進去。
- TOML 的語法錯誤與型別錯誤，訊息指出出錯的是哪個檔案的第幾行，不是合併後的行號。合併之後才做的檢查（例如用戶端的檢查）訊息開頭是主設定檔的名稱，沒有行號；訊息裡有單引號括起的名稱（例如 `'core'`）時，後面會附上 `(in <檔案> line <行>)`，列出哪些行以引號字串寫著這個名稱（最多三處，其餘寫 `and N more`），主檔的行也會列出，所以名稱重複時兩處都看得到。只有找到的行至少有一行在被讀進來的檔案時才會附；名稱只在主檔或找不到時，訊息和沒有 `include` 時一樣。這是用名稱比對，名稱也出現在別的設定值時會多列一處。見[第 05 章](05-radius-clients.md)。

> **警告：** 被讀進來的檔案能決定誰可以登入。檔案本身或它所在的目錄可以被群組或其他人寫入時，`check`、啟動與每次 reload 都會印出警告，例如 `included configuration file conf.d/20-lab.toml can be written by its group or by others, which lets them change who may log in`。這只是警告，設定照樣讀；看到它就把權限改成只有擁有者能寫。

> **注意：** 把所有用戶端都搬進 `conf.d/` 之後，如果目錄是空的或檔名對不上，萬用字元不算錯誤，結果是 `[radius]` 沒有任何用戶端，`check` 失敗：`[radius] is enabled but has no [[radius.clients]] entries`。

## 把祕密放進檔案

**目標：** 設定檔可以給同事看、可以放進版本控制，祕密不跟著外流。

很多祕密都有兩種寫法：直接寫值，或用 `*_file` 指向一個檔案。兩種剛好要設一個，例如用戶端的 `secret` 與 `secret_file` 都寫或都不寫時，`check` 失敗：`exactly one of secret or secret_file must be set`。本書的範例一律用檔案。

核心版會讀的祕密檔：

| 設定 | 內容 | 主章 |
| --- | --- | --- |
| `radius.clients[].secret_file`、`tacacs.clients[].secret_file` | 用戶端的共用密碼 | [05](05-radius-clients.md)、[12](12-tacacs.md) |
| `cluster.secret_raft_file`、`cluster.secret_api_file` | 叢集兩個埠的密碼 | [16](16-cluster.md) |
| `databases.<name>.password_file` | 外部資料庫的密碼 | [09](09-external-databases.md) |
| `auth.handlers[ldap].bind_password_file` | LDAP 服務帳號的密碼 | [08](08-ldap-and-ad.md) |
| `auth.handlers[netlogon].machine_password_file` | 網域電腦帳號的密碼 | [08](08-ldap-and-ad.md) |
| `radius.eap.key_file` | EAP 伺服器憑證的私鑰 | [11](11-eap.md) |

產生一個隨機的共用密碼並收緊權限：

```console
$ openssl rand -hex 32 > nas1.secret
$ chmod 600 nas1.secret
```

規則：

- 用戶端的 `secret_file` 結尾的換行（`\n`、`\r`）會去掉，所以用 `echo` 或編輯器寫入的檔案都可以用。檔案是空的（或只有換行）時失敗：`secret_file 'nas1.secret' is empty`。讀不到時失敗：`cannot read secret_file 'nas1.secret': No such file or directory (os error 2)`。
- **權限警告：** 上表的檔案只要「其他人有任何權限」或「群組可以寫」（mode 有 `0o027` 中的任一位元），啟動、`check` 與每次 reload 都會各記一行警告，然後照樣使用。`0600`、`0640`、`0400` 不會警告；`0644`、`0660`、`0666` 會。警告只寫設定名稱、路徑和權限，不會寫出內容：

  ```text
  … WARN rradius_core::common::seal: secret_file: 'lab.secret' has mode 644, which others can reach or its group can write; make it 0600 or 0640
  ```

- **共用密碼太短：** 少於 16 個位元組時警告 `RADIUS client 'lab-net' has a shared secret shorter than 16 octets; use at least 24 random octets`。TACACS+ 用戶端也一樣，開頭寫 `TACACS+ client`。
- **用了範例的密碼：** 範例附的密碼都含有 `change-me`。用戶端的共用密碼或叢集密碼含有它（不分大小寫）時警告。用戶端是 ``RADIUS client 'lab-net' has the public shared secret shipped with the examples; replace it with a random one, for example from `openssl rand -hex 32` ``，叢集密碼是 ``cluster.secret_raft is the public value shipped with the examples; …``。

這三種都只是警告，不會擋下啟動。但它們出現在 `check` 的輸出裡，上線前看一眼就能抓到。

> **注意：** 用 systemd 跑的套件版本以 `rradius` 使用者執行。權限收到 `0600` 時，檔案的擁有者要是 `rradius`；或讓 root 擁有、群組是 `rradius`、權限 `0640`。安裝路徑與使用者見[第 02 章](02-install.md)。

## 檢查設定：check

```console
$ rradiusd check --config rradius.toml
configuration ok
$ echo $?
0
```

`check` 做的是啟動時讀設定的那一段：讀設定檔與所有被讀進來的檔案、讀祕密檔、讀字典、編譯最上層腳本、讀 migration 目錄、檢查驗證鏈與 `[lua.http]` 的設定、檢查計費 spool 的目錄，最後印出共用密碼的警告。它**不開任何埠、不建立資料目錄、不連線到任何主機**，所以可以在正式機上、伺服器跑著的時候放心執行。

- 全部通過：stdout 印 `configuration ok`，結束碼 0。警告（權限、密碼太短等）印在 stderr，不影響結束碼。
- 有錯：stderr 印出原因，結束碼 2。訊息的開頭是出錯的檔案，能定位時再加上行號。
- 參數錯誤（例如 `check --config a.toml extra`）：印 `usage: rradiusd check [--config <path>]`，結束碼 2。

`check` 不會發現的事：埠已經被別的程式占用、叢集的其他節點連不上、外部資料庫或 LDAP 伺服器的帳密錯誤。這些要啟動後看日誌。

改設定的建議流程：改檔 → `check` → `config diff` 看差異 → reload 或重新啟動 → 看日誌那一行結果。

## 看實際生效的設定：config show

`config show` 依伺服器讀設定的方式讀一次，把沒寫的鍵填上預設值後整份印出。下面是[一份從零開始的設定](#一份從零開始的設定)加上 `conf.d/20-lab.toml` 的結果（`…` 是省略的兩行在核心沒有作用的設定，見[附錄 A](90-config-reference.md)）：

```console
$ rradiusd config show --config rradius.toml
# computed on this machine: radius.sockets = 1, cluster.read_connections = 28
dictionary = "/usr/share/rradius/dictionary"
include = ["conf.d/*.toml"]

[lua]
script = "auth.lua"
max_execution_ms = 1000
max_memory_mb = 256
query_timeout_ms = 5000

[cluster]
data_dir = "data"
secret_raft_file = "raft.secret"
secret_api_file = "api.secret"
migrations_wait_ms = 60000
node_id = 1
read_connections = 28
snapshot_every = 100000

[metrics]
bind = "127.0.0.1"
port = 9812

[radius]
bind = "127.0.0.1"
auth_port = 1812
acct_port = 1813
…
limit_proxy_state = "yes"
max_acct_inflight = 4096
max_auth_inflight = 4096
require_message_authenticator = "yes"
status_server = true
sockets = 1

[[radius.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "localhost.secret"

[[radius.clients]]
name = "lab-net"
address = "192.0.2.0/24"
secret_file = "lab.secret"

[shutdown]
ready_delay_ms = 0
drain_ms = 5000
```

- 第一行的註解列出依這台機器算出來的預設值，例如 `radius.sockets` 與 `cluster.read_connections` 跟 CPU 數有關，換一台機器可能不同。
- 主檔與所有被讀進來的檔案合在一起印。
- 直接寫在設定裡的祕密印成 `<hidden>`；`*_file` 只印路徑，不印檔案內容。
- 有預設值的區段（例如這裡的 `[shutdown]`）即使沒寫也會印出；可有可無的區段（例如 `[log]`、`[auth]`）沒寫就不出現。
- 設定有錯時，`config show` 和 `check` 一樣印出錯誤，結束碼 2。

要知道某個鍵現在到底是多少，用它最準：它和伺服器用的是同一段讀設定的程式。

## 比較兩份設定：config diff

**目標：** reload 或部署之前，確認這次到底改了什麼；或比較叢集裡兩個節點的設定。

下面把整份設定（含 `conf.d/` 與祕密檔）複製到 `new/` 目錄，再改四個地方：`auth_port` 改成 1822、加上 `[shutdown]` 的 `drain_ms = 10000`、`[cluster]` 加上 `listen = "127.0.0.2"`，以及把 `localhost` 的 `secret_file` 換成直接寫的 `secret`。

```console
$ rradiusd config diff rradius.toml new/rradius.toml
radius.auth_port: 1812 | 1822
radius.clients[localhost].secret: differs (hidden)
radius.clients[localhost].secret_file: "localhost.secret" | (absent)
shutdown.drain_ms: 5000 | 10000
node-specific:
  cluster.listen: (absent) | "127.0.0.2"
$ echo $?
1
$ rradiusd config diff rradius.toml rradius.toml
no differences
$ echo $?
0
```

- 兩份都先填上預設值再比，所以「沒寫」和「寫了預設值」不算差異。
- 每行是 `鍵: 左邊的值 | 右邊的值`；一邊沒有這個鍵時寫 `(absent)`。
- 祕密只說 `differs (hidden)`，不印值。`*_file` 比的是路徑，不比檔案內容：換了共用密碼檔的內容，`config diff` 看不出來。
- `radius.clients`、`tacacs.clients` 以用戶端的 `name`（沒有 `name` 時用 `address`）配對，不看順序，所以把用戶端換個順序或搬到另一個檔案不會出現差異。
- `cluster.node_id` 與 `cluster.listen` 本來就該每個節點不同，所以列在最後的 `node-specific:` 底下，不影響結束碼。
- 結束碼：沒有差異是 0，有差異是 1，任一份設定有錯是 2。只有 `node-specific` 的差異時，先印 `no differences` 再列出它們，結束碼是 0。

## 查設定鍵的說明：schema

`schema` 印出設定檔的 JSON Schema：每個表與鍵的型別、預設值與說明。`--lang zh-TW` 印中文說明，不寫或 `--lang en` 印英文：

```console
$ rradiusd schema --lang zh-TW > schema.json
$ jq -r '.properties | keys | join(" ")' schema.json
accounts auth cluster databases dictionary include log lua metrics queries radius runtime sessions shutdown tacacs
$ jq '.properties.shutdown.anyOf[0].properties.drain_ms' schema.json
{
  "default": 5000,
  "description": "停止接收新請求後，最多等處理中的請求多久，毫秒。",
  "format": "uint64",
  "minimum": 0,
  "type": [
    "integer",
    "null"
  ]
}
```

- 這份 schema 就是從讀設定的程式產生的，所以程式更新後它也跟著更新。手冊和它不一致時，以你手上那一版的輸出為準。
- 支援 JSON Schema 的編輯器可以拿它來補全鍵名、檢查拼字。
- 依機器而定的預設值（`radius.sockets` 等）在 schema 裡是建置那台機器的值，實際值看 `config show`。
- 語言寫錯時印 `usage: rradiusd schema [--lang en|zh-TW]`，結束碼 2。

## 不停機套用變更：reload

**目標：** 改了用戶端、共用密碼、驗證鏈或腳本之後，不中斷服務就讓它生效。

**怎麼觸發：** 對伺服器的行程送 SIGHUP。

```console
$ kill -HUP <pid>
$ systemctl reload rradius      # systemd 的 unit 用 ExecReload 送 SIGHUP
$ service rradius reload        # FreeBSD 的 rc 腳本
```

**發生了什麼：** 伺服器用和啟動時相同的程式，把主檔、被讀進來的檔案、祕密檔與腳本全部重新讀一次。

- **全有或全無。** 先把新的設定全部建好、檢查過，才一次換上。任何一步失敗，就記一行錯誤，繼續用原來的設定服務，行程不會結束。
- **處理中的請求不受影響。** 已經在處理的請求用它開始時的腳本、驗證鏈與用戶端做完；之後的請求才用新的。
- **重送的時機：** reload 期間又收到 SIGHUP，做完這次後會再做一次。啟動過程中收到的 SIGHUP，在開始服務後生效。
- **結果只有一行日誌**，不會引用設定檔的內容（設定檔的某一行可能有密碼）。

**驗證：** 在[一份從零開始的設定](#一份從零開始的設定)的伺服器跑著時，加入 `conf.d/20-lab.toml`（見[把設定拆成多個檔案](#把設定拆成多個檔案)）再送 SIGHUP：

```console
$ kill -HUP $(pgrep -f 'rradiusd serve')
$ curl -s http://127.0.0.1:9812/metrics | grep '^rradius_reload'
rradius_reload_total{result="ok"} 1
rradius_reload_total{result="error"} 0
rradius_reload_last_success_timestamp_seconds 1795384713
```

伺服器的日誌：

```text
… INFO rradius_core::reload: configuration reloaded from rradius.toml
```

**結果長什麼樣子：** reload 的結果有三種，每種都看得到：

| 情況 | 日誌 | 指標 |
| --- | --- | --- |
| 成功，全部套用 | `INFO … configuration reloaded from <檔案>` | `rradius_reload_total{result="ok"}` 加 1，`rradius_reload_last_success_timestamp_seconds` 更新為這次的 Unix 時間 |
| 成功，但有些要重啟 | `WARN … configuration reloaded from <檔案>; these need a restart to apply: <清單>` | 同上 |
| 失敗，什麼都沒換 | `ERROR … reload failed, keeping the running configuration: <原因>` | `rradius_reload_total{result="error"}` 加 1 |

失敗的原因和 `check` 的訊息相同，例如被讀進來的檔案有語法錯誤，或腳本編譯不過：

```text
… ERROR rradius_core::reload: reload failed, keeping the running configuration: conf.d/20-lab.toml: line 6: `address`: invalid basic string, expected `"`
… ERROR rradius_core::reload: reload failed, keeping the running configuration: cannot compile script 'auth.lua': syntax error: 11: Expected <eof>, got 'retrun'
```

修好之後再送一次 SIGHUP，就會套用。為了避免這種情況，先跑 `check` 再 reload。

### reload 會套用的

- 最上層腳本（`lua.script` 和它的內容）、`[lua]` 的 `query_timeout_ms`、`max_execution_ms`、`[lua.http]`。
- 整個 `[auth]`：驗證鏈、處理器、腳本、`[auth.lockout]`、`[auth.password_limits]` 等。鎖定的失敗計數保留在行程裡，reload 不會清掉。
- RADIUS 與 TACACS+ 的用戶端清單，含每個用戶端的設定與共用密碼（祕密檔會重新讀）。移除的用戶端，之後的請求就當成不明的來源丟掉。
- `radius.require_message_authenticator`、`radius.limit_proxy_state`。用 `auto` 學到的結果在 reload 後仍保留（第 05 章）。
- `[radius.eap]`（新的 EAP 登入才用新設定）、`[tacacs]` 的 `enable` 與 `authorize_groups`、`[log.trace]`、`[shutdown]`。
- `[radius.accounting_log]` 會重新開檔，配合日誌輪替；也可以藉此改檔名（第 13 章）。
- `[accounts]`、`[sessions]` 根本不需要 reload：`user`、`session` 指令每次執行時才讀。

### 要重新啟動才生效的

下面這些在 reload 時不套用。和執行中的值不同時，reload 那一行 `WARN` 會列出它們的名稱，其他的變更照樣套用：

| 名稱 | 說明 |
| --- | --- |
| `dictionary` | 字典的路徑 |
| `lua.max_memory_mb` | Lua VM 的記憶體上限 |
| `[cluster]` | 整個叢集設定，含 migration |
| `[metrics]`、`[log]`、`[runtime]` | 整個表（`[log.trace]` 除外，它可以 reload） |
| `[databases]`、`[queries]` | 新增、移除或改任何一個外部資料庫與具名查詢 |
| `radius.bind`、`radius.auth_port`、`radius.acct_port` | 監聽的位址與埠 |
| `radius.max_auth_inflight`、`radius.max_acct_inflight`、`radius.status_server`、`radius.sockets`、`radius.recv_buffer_bytes` | |
| `[radius.accounting_spool]`、`[radius.accounting_log]` 的 `queue` | |
| `tacacs.bind`、`tacacs.port`、`tacacs.max_connections`、`tacacs.max_connections_per_source`、`tacacs.max_body_len`、`tacacs.idle_timeout_ms` | |
| `[radius]`、`[tacacs]` | 加入或移除整個區段 |

例如同時改了認證埠與腳本，送 SIGHUP 之後：

```text
… WARN rradius_core::reload: configuration reloaded from rradius.toml; these need a restart to apply: radius.auth_port
```

腳本已經換成新的，但伺服器還在舊的埠上。這行警告在重新啟動之前，每次 reload 都會再出現一次：伺服器拿來比較的是啟動時的設定。

> **注意：** 字典**檔案的內容**改了不算「設定不同」：reload 只比較 `dictionary` 這個路徑。但 reload 會重新讀字典檔，內容和啟動時不同就多寫一行 `WARN`：`dictionary files changed since the server started; restart to apply: <路徑>`（最多列五個檔，其餘寫 `and N more`），每次 reload 只要還不同就會再寫。伺服器仍用啟動時讀的字典，腳本用新屬性會得到 `unknown attribute`。改字典之後一定要重新啟動。詳見[第 06 章](06-dictionary.md)。

## 停止伺服器：shutdown

**目標：** 停止時不讓 NAS 看到逾時：先讓負載平衡器把流量移走，再等處理中的請求做完。

收到 SIGTERM（`systemctl stop`、`docker stop` 送的就是它）或 SIGINT（Ctrl+C）後，伺服器依序：

1. `/readyz` 改回 503（內容 `stopping`），`/livez` 仍回 200。請求照常回答，持續 `ready_delay_ms`。
2. 不再接新的請求。
3. 排空（drain）：等處理中的請求做完，最多 `drain_ms`。
4. 關閉叢集資料庫，結束行程。

這段時間內再收到一次 SIGTERM 或 SIGINT，就跳過剩下的等待，直接進入第 4 步。

| 鍵 | 預設 | reload | 說明 |
| --- | --- | --- | --- |
| `shutdown.ready_delay_ms` | 單節點 `0`；`cluster.nodes` 超過一個時 `5000` | 可 | 第 1 步的時間，毫秒，0 到 60000 |
| `shutdown.drain_ms` | `5000` | 可 | 第 3 步最多等多久，毫秒，0 到 60000 |

0 表示不等。超過 60000 時 `check` 失敗：`shutdown.drain_ms must be at most 60000`。停止開始那一刻生效中的值為準，所以 reload 就套用。見[附錄 A](90-config-reference.md)。

**設定：**

```toml
# conf.d/40-shutdown.toml
[shutdown]
ready_delay_ms = 3000
drain_ms = 2000
```

**驗證：** reload 之後送 SIGTERM，馬上看 `/readyz`，並在這 3 秒內送一筆登入：

```console
$ kill -HUP <pid>
$ kill -TERM <pid>
$ curl -s -w '%{http_code}\n' http://127.0.0.1:9812/readyz
stopping
503
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
…
  pap: 2626/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

**結果長什麼樣子：** 停止中的登入照樣得到 `accept 1`。日誌：

```text
… INFO rradius_core::cli: stopping: /readyz answers 503; requests are answered for 3000 ms more, then those being handled are waited for, for at most 2000 ms
… INFO rradius_auth: authentication protocol="radius" client="localhost" … decision="accept"
… INFO rradius_core::cli: stopping: no more requests taken
… INFO rradius_core::cli: stopping: every request taken was answered
… WARN hiqlite::store::state_machine::sqlite::writer: SQL writer is shutting down
```

在停止過程中再送一次訊號時，日誌會出現 `stopping: asked again; not waiting for the requests being handled`。`drain_ms` 到了還有請求沒做完時，記一行 `WARN`，寫出還有幾個 RADIUS 請求與 TACACS+ 連線被切斷。

- 前面放了負載平衡器、或是多節點叢集時，`ready_delay_ms` 要比負載平衡器檢查 `/readyz` 的間隔加上判定失敗所需的次數更長，流量才來得及移走。
- 單節點沒有別的伺服器可以接手，所以預設不延遲。

> **注意：** 讓伺服器自己關閉資料庫再結束。套件附的 systemd unit 設了 `TimeoutStopSec=60`；自己寫的啟動方式也要給足時間，不要在等待期間用 `kill -9`。

## runtime 與 log

- `[runtime]` 只有 `pin_threads`（預設 `false`，要重啟），把每個工作執行緒綁在一顆 CPU 上，只在 Linux 有效；在其他平台設成 `true` 時 `check` 失敗：`runtime.pin_threads is only supported on Linux`。何時該用見[第 19 章](19-performance.md)。
- `[log]` 決定日誌的等級（預設 `info`）、格式（預設 `text`）與是否寫認證紀錄（預設 `true`），整個表要重啟才生效；`[log.trace]` 可以 reload，用來暫時記下特定使用者或用戶端的完整請求。詳見[第 17 章](17-monitoring.md)。

## 常見錯誤

**症狀：** `check` 印出 ``rradius.toml: line 15: `prot`: unknown field `prot`, expected `bind` or `port` ``，結束碼 2。
**原因：** 鍵名打錯。每個表都拒絕未知的鍵。
**解法：** 照訊息的行號與它列出的可用鍵名改正。

**症狀：** ``conf.d/30-shutdown.toml: line 3: `drain_ms`: invalid type: string, expected u64``。
**原因：** 值的型別不對，例如數字寫成了字串 `"5s"`。訊息故意不印出值。
**解法：** 照 `expected` 後面的型別改；數字不加引號，時間一律寫成毫秒的整數。

**症狀：** `conf.d/30-bad.toml: line 1: duplicate key`。
**原因：** 被讀進來的檔案宣告了主檔（或另一個檔案）已經有的表，例如第二個 `[radius]`、`[lua]`。
**解法：** 把設定移回原本那個表所在的檔案；或從主檔刪掉那個表，整個表只放在一個檔案裡。表陣列 `[[radius.clients]]` 不受影響。

**症狀：** ``conf.d/30-bad.toml: line 2: an included file has to begin with a table (`[section]`), not a setting: a setting before its first table would belong to the file before it``。
**原因：** 被讀進來的檔案一開頭就是 `鍵 = 值`，例如 `dictionary = …`。頂層的鍵只能寫在主檔。
**解法：** 把頂層的鍵移到主檔；被讀進來的檔案以 `[表]` 開頭。

**症狀：** ``conf.d/30-bad.toml: line 1: an included file cannot have `include` ``。
**原因：** 被讀進來的檔案又寫了 `include`，不支援多層。
**解法：** 把所有名稱都列在主檔的 `include`。

**症狀：** `extra.toml: cannot read included file: No such file or directory (os error 2)`。
**原因：** `include` 裡沒有萬用字元的名稱，檔案不存在。
**解法：** 修正名稱，或改用萬用字元（比對不到時不算錯）。

**症狀：** `rradius.toml: include: '*.d/*.toml' has a wildcard before its last component; only the file name may have one`。
**原因：** 目錄名稱用了萬用字元。
**解法：** 只在最後一段的檔名用 `*`、`?`；有好幾個目錄就各寫一個名稱。

**症狀：** `rradius.toml: [radius] is enabled but has no [[radius.clients]] entries`，而用戶端明明寫在 `conf.d/` 裡。
**原因：** `include` 的萬用字元沒比對到那些檔案（目錄或副檔名不對），而比對不到不算錯誤。
**解法：** 用 `config show` 看讀進來了什麼；確認檔案的副檔名、所在目錄，以及相對路徑是以主檔的目錄為準。

**症狀：** `nonexist.toml: cannot read configuration: No such file or directory (os error 2)`；或沒寫 `--config` 時是 `/etc/rradius/rradius.toml: cannot read configuration: …`。
**原因：** 設定檔路徑錯誤，或忘了 `--config`。`serve` 指令在這種情況下還會加一行 `(the default configuration; give --config <path> for another)`。
**解法：** 寫上正確的 `--config`。

**症狀：** reload 之後日誌出現 `these need a restart to apply: …`，改的東西沒生效。
**原因：** 列出的設定要重新啟動才會套用，見[要重新啟動才生效的](#要重新啟動才生效的)。
**解法：** 排時間重新啟動。在那之前，每次 reload 都會再列一次。

**症狀：** 在字典加了屬性並 reload，日誌寫 `configuration reloaded`，但腳本得到 `unknown attribute '…'`。
**原因：** 字典只在啟動時讀；reload 只比較 `dictionary` 的路徑來決定要不要重啟，但字典檔內容和啟動時不同時日誌會有 `dictionary files changed` 的 `WARN`。
**解法：** 重新啟動伺服器。

**症狀：** `shutdown.drain_ms must be at most 60000`。
**原因：** `[shutdown]` 的兩個值上限都是 60000 毫秒。
**解法：** 改成 60000 以下。

## 相關章節

- [03 第一次啟動與登入](03-quick-start.md)
- [05 RADIUS 服務與 NAS 用戶端](05-radius-clients.md)
- [06 字典與屬性](06-dictionary.md)
- [10 用 Lua 寫政策](10-lua-policies.md)
- [16 叢集與高可用](16-cluster.md)
- [17 監控、狀態與日誌](17-monitoring.md)
- [18 日常維運](18-operations.md)
- [20 安全強化](20-security.md)
- [附錄 A：設定參考](90-config-reference.md)
- [附錄 B：命令參考](91-command-reference.md)
