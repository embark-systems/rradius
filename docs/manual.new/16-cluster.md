# 16 叢集與高可用

> 本章讓你做到：
> - 架起三個節點的叢集，確認寫進一個節點的資料，其他節點也讀得到
> - 看懂 leader、follower、複寫與 snapshot，知道失去 leader 時伺服器會怎麼做
> - 處理節點故障：重開節點、從其他節點重建資料
> - 讓新節點加入叢集，安全地移除節點
> - 用 Docker Compose 或 Kubernetes 部署三個節點的叢集

前置條件：做過[第 03 章](03-quick-start.md)，知道怎麼啟動伺服器、用 `rradius-bench` 測登入；讀過[第 04 章](04-configuration.md)，知道 `check` 與 reload。本章的練習在一台機器上起三到四個節點，需要原始碼建出來的 `rradius-server` 與 `rradius-bench`，以及原始碼裡的 `rradius-server/examples/scenarios/ha-cluster/`。

本章小節：

- [概念](#概念)
- [cluster 設定](#cluster-設定)
- [在一台機器上起三個節點](#在一台機器上起三個節點)
- [確認叢集成形](#確認叢集成形)
- [確認寫入會複寫](#確認寫入會複寫)
- [停掉 leader](#停掉-leader)
- [失去多數節點](#失去多數節點)
- [重開節點與重建資料](#重開節點與重建資料)
- [加入節點](#加入節點)
- [移除節點](#移除節點)
- [檢查各節點的設定](#檢查各節點的設定)
- [看整個叢集的狀態](#看整個叢集的狀態)
- [多台主機：ha-cluster 情境](#多台主機ha-cluster-情境)
- [用 Docker Compose 部署](#用-docker-compose-部署)
- [在 Kubernetes 上部署](#在-kubernetes-上部署)
- [叢集的指標](#叢集的指標)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

## 概念

### 單機也是一個叢集

rradius 的帳號、回覆屬性、計費這些資料，放在每個節點內嵌的 SQLite 資料庫裡（hiqlite）。節點之間用 Raft 協定複寫這份資料庫，每個節點都有一份完整的副本。

所以設定裡一定要有 `[cluster]`，單機也一樣。只跑一台時，它就是只有一個節點的叢集：

- 不寫 `[[cluster.nodes]]` 時，叢集只有節點 1，Raft 埠是 `127.0.0.1:8100`、API 埠是 `127.0.0.1:8200`，兩個埠預設只聽 `127.0.0.1`。
- 之後要擴成三個節點，資料表與腳本都不必改，只要改 `[cluster]`。

### leader、follower 與寫入

叢集裡隨時最多只有一個 leader，其他節點是 follower。

- **讀：** 腳本的 `sql_query`、`sql` 處理器查密碼，都讀本機的副本，不經過網路。
- **寫：** 腳本的 `sql_execute`、`user add` 這類指令，都要交給 leader。leader 等過半數的節點確認後才算寫入成功。follower 收到的寫入會轉給 leader。

因為寫入要過半數確認，叢集要有過半數的節點活著才能寫：

| 節點數 | 過半數 | 可以同時停掉幾個 |
| --- | --- | --- |
| 1 | 1 | 0 |
| 2 | 2 | 0 |
| 3 | 2 | 1 |
| 4 | 3 | 1 |
| 5 | 3 | 2 |

兩個節點的叢集比一個節點更脆弱：任何一個停掉，寫入就停了。所以 `cluster remove` 在只剩一或兩個 voter 時，要加 `--force` 才肯移除（見[移除節點](#移除節點)）。

leader 停掉時，剩下的節點會自己選出新的 leader，不需要人介入。

### 節點追上進度之後才開始服務

一個節點啟動時，不會馬上開始回應請求。它依序做這些事：

1. 啟動本機的叢集節點。第一次啟動的節點要先加入叢集。
2. 等到有可以寫入的 leader，最多等 `migrations_wait_ms`（預設 60000 毫秒）。
3. 有 `migrations_dir` 的話，套用還沒套用的 migration（[第 18 章](18-operations.md)）。
4. 透過 leader 寫一筆自己的記號，等本機副本讀回這筆記號，才算追上進度。日誌寫 `cluster node N has caught up with the cluster`。
5. 開 RADIUS、TACACS+ 與指標埠，日誌寫 `Servers started`。

第 4 步保證：一個停機兩個小時才回來的節點，回答的第一個請求就看得到這兩個小時裡新增的帳號。指標埠也是在這之後才開，所以啟動期間連 `/livez` 都連不上。

### snapshot

Raft 把每一筆寫入記在記錄（log）裡。記錄會一直變長，所以每套用 `snapshot_every` 筆（預設 100000），節點就把資料庫存一份 snapshot，之後可以丟掉更早的記錄。新加入、或落後太多的節點，從 leader 拿 snapshot 再接著追記錄。

snapshot 是複寫用的，不是備份。核心沒有備份功能，資料保護的做法見[第 18 章](18-operations.md)。

### 沒有 leader 時

一個節點跟其他節點斷了聯絡、湊不到過半數時，它就沒有可以寫入的 leader。這時的行為取決於 `stop_without_leader_ms`：

| 狀況 | 沒有設 `stop_without_leader_ms`（預設） | 設了，而且沒有 leader 超過這段時間 |
| --- | --- | --- |
| 只讀資料的登入（例如 `sql` 處理器查密碼） | 照常回答，用本機副本，資料可能是舊的 | 不回應，算進 `rradius_radius_discarded_total{reason="no_leader"}` |
| Status-Server | 照常回答 | 不回應 |
| 腳本要寫入的登入 | 沒有回應（寫入失敗） | 不回應 |
| 計費 | 前幾秒寫入在等 leader，沒有回應；沒有 leader 超過 5 秒後寫入立刻失敗，有計費 spool 就寫進 spool 並回應，沒有就不回應 | 同左 |
| TACACS+ 認證與授權 | 照常回答，用本機副本 | 回 ERROR |
| TACACS+ 計費 | 照常交給腳本 | 不受 `stop_without_leader_ms` 影響 |
| `/readyz` | 200 | 503 `no leader` |

另外兩個跟時間有關的規則：

- 沒有 leader 超過 5 秒後，新的寫入立刻失敗，不再等待。寫入失敗的計費，有計費 spool 時會寫進 spool（[第 13 章](13-accounting.md)）。
- leader 本身如果超過 5 秒沒有收到過半數節點的確認，也算沒有 leader。被隔開的舊 leader 不會以為自己還能寫。

什麼時候該設 `stop_without_leader_ms`？當 NAS 設了多台 RADIUS 伺服器時，設了它，被隔開的節點會停止回應，NAS 就會改問其他還在叢集裡的節點，`/readyz` 也會讓負載平衡器把它移出去。Kubernetes 的範例設定就設了 10000。只有一台伺服器可問的 NAS，不設比較好：至少只讀的登入還能通過。

### 叢集的埠與安全

每個節點開兩個叢集用的埠：

- `raft`：節點之間複寫用。
- `api`：節點之間轉送寫入、成員管理用；`user`、`session`、`cluster` 這些指令也是透過它跟叢集說話。

這兩個埠只用共用密碼保護（`secret_raft`、`secret_api`），流量本身**沒有加密**。所以：

- `listen` 設成節點的內部位址或 WireGuard 位址，不要讓外部網路連得到。
- 節點在不同機房或走公開網路時，節點之間的流量要走加密的通道，例如 WireGuard。ha-cluster 情境就是這樣設計的：節點的位址是 WireGuard 網段 `10.99.0.0/24`，`listen` 只聽 WireGuard 介面。WireGuard 本身怎麼設，不在本書範圍。

> **注意：** 有 `[[cluster.nodes]]` 而沒有寫 `listen` 時，兩個埠會聽 `0.0.0.0`，`check` 與啟動時都會警告 `cluster.listen is not set, so the cluster's Raft and API ports are on all interfaces, …`。真的要聽所有位址（例如在容器裡），寫明 `listen = "0.0.0.0"` 就不會再警告。

## cluster 設定

`[cluster]` 的每個鍵改了都要重新啟動才生效；reload 時日誌會在 `these need a restart to apply:` 後面列出 `[cluster]`。完整的型別與預設值見[附錄 A](90-config-reference.md)。

| 鍵 | 預設 | 意思 |
| --- | --- | --- |
| `node_id` | `1` | 這個節點的 id，至少 1，要出現在 `nodes` 裡 |
| `data_dir` | 必填 | 資料庫與 Raft 記錄的目錄，相對於設定檔 |
| `listen` | 沒有 `nodes` 時 `127.0.0.1`，有 `nodes` 時 `0.0.0.0` | `raft` 與 `api` 兩個埠監聽的位址；埠號取自 `nodes` 裡這個節點的那一項 |
| `nodes` | 只有節點 1，`127.0.0.1:8100`、`127.0.0.1:8200` | 叢集的節點清單，每項有 `id`、`raft`、`api`，都要寫成 `host:port` |
| `secret_raft`、`secret_raft_file` | 二選一，必填 | Raft 埠的共用密碼，至少 16 個字元，每個節點要相同 |
| `secret_api`、`secret_api_file` | 二選一，必填 | API 埠的共用密碼，至少 16 個字元，每個節點要相同 |
| `migrations_dir` | 無 | 啟動時套用的 SQL 檔目錄（[第 18 章](18-operations.md)） |
| `migrations_wait_ms` | `60000` | 啟動時等 leader、套用 migration、追上進度，三段各自的時限，毫秒 |
| `snapshot_every` | `100000` | 兩次 snapshot 之間的 Raft 記錄筆數 |
| `read_connections` | 工作執行緒數的兩倍，最多 32 | 讀本機資料庫的連線數，1 到 128（[第 19 章](19-performance.md)） |
| `stop_without_leader_ms` | 無（一直回應） | 沒有 leader 超過這段時間，就不回應登入與 Status-Server，毫秒 |

`nodes` 的規則（違反時 `check` 失敗）：

- 一定要有 id 為 1 的節點：`cluster.nodes: node 1 is required`。節點 1 是第一次啟動時建立叢集的那一個。
- id 不能重複，也不能是 0。
- `node_id` 要在清單裡：`cluster.node_id 4 is not in cluster.nodes`。
- 位址要是 `host:port`：`cluster.nodes: node 1 raft '10.0.0.1' must be host:port`。`host` 可以是名稱，例如 Kubernetes 的 `rradius-0.rradius-cluster`。

每個節點的設定裡，`nodes` 列出包含自己在內的所有節點。各節點的設定除了 `node_id`、`listen` 這類本機的值，其餘最好完全相同；用 `config diff` 檢查的方法見[檢查各節點的設定](#檢查各節點的設定)。

設了 `nodes` 而且超過一個節點時，`[shutdown]` 的 `ready_delay_ms` 預設變成 5000（單節點是 0），讓 NAS 與負載平衡器有時間改問別的節點。`[shutdown]` 的說明在[第 04 章](04-configuration.md)，關機的順序見[第 18 章](18-operations.md)。

## 在一台機器上起三個節點

目標：在一台機器上跑三個節點，接下來的各節都用它來練習。

> **注意：** 這是練習用的做法，跟多台主機的部署有幾個差別：
> - 三個節點都在 `127.0.0.1`，所以每個節點要用不同的埠與不同的 `data_dir`。正式環境每台主機用一樣的埠（例如 8100、8200、1812、1813、9812），只是位址不同。
> - 停掉一個節點等於停掉一個行程，不會遇到網路分割、封包遺失、時鐘不同步這些多台主機才有的狀況。本章量到的秒數只代表這台機器。
> - `status --cluster` 在這種做法下看不到其他節點，見[看整個叢集的狀態](#看整個叢集的狀態)。

### 準備目錄

以下指令在原始碼 repo 的根目錄執行：

```console
$ export PATH="$PWD/target/release:$PATH"
$ mkdir -p ~/rradius-lab/cluster
$ cp -R rradius-server/examples/scenarios/ha-cluster/accounting.lua \
        rradius-server/examples/scenarios/ha-cluster/migrations ~/rradius-lab/cluster/
$ cd ~/rradius-lab/cluster
$ for f in raft api nas; do openssl rand -hex 32 > $f.secret; done
$ chmod 600 *.secret
```

- `accounting.lua` 與 `migrations/` 跟 quick-start 情境相同：`users`、`user_reply`、`accounting` 三個表與示範帳號。
- `raft.secret`、`api.secret` 是叢集密碼，`nas.secret` 是 NAS 的共用密碼。三個節點共用這三個檔案；在多台主機上，就是把同樣的檔案複製到每一台。

### 節點 1 的設定

本章的練習用 27000 號段的埠，避免跟系統上其他程式用的預設埠衝突：

| 節點 | `node_id` | `data_dir` | `raft` | `api` | RADIUS 認證／計費 | TACACS+ | 指標 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 1 | `data1` | 27001 | 27011 | 27021／27031 | 27051 | 27041 |
| 2 | 2 | `data2` | 27002 | 27012 | 27022／27032 | 27052 | 27042 |
| 3 | 3 | `data3` | 27003 | 27013 | 27023／27033 | 27053 | 27043 |
| 4（[加入節點](#加入節點)用） | 4 | `data4` | 27004 | 27014 | 27024／27034 | 27054 | 27044 |

```toml
# node1.toml
dictionary = "/path/to/share/dictionary"   # 改成字典的絕對路徑

[lua]
script = "accounting.lua"
query_timeout_ms = 2000

[cluster]
node_id = 1
listen = "127.0.0.1"
data_dir = "data1"
secret_raft_file = "raft.secret"
secret_api_file = "api.secret"
migrations_dir = "migrations"
migrations_wait_ms = 120000

[[cluster.nodes]]
id = 1
raft = "127.0.0.1:27001"
api = "127.0.0.1:27011"

[[cluster.nodes]]
id = 2
raft = "127.0.0.1:27002"
api = "127.0.0.1:27012"

[[cluster.nodes]]
id = 3
raft = "127.0.0.1:27003"
api = "127.0.0.1:27013"

[auth]
policy = "first_match"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ? AND enabled = 1"
reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"

[accounts]
add = "INSERT INTO users (username, password) VALUES (?1, ?2)"
show = "SELECT username, enabled FROM users WHERE username = ?1"

[sessions]
list = "SELECT username, nas, session_id, started, updated FROM accounting WHERE stopped IS NULL ORDER BY started, session_id"

[radius]
bind = "127.0.0.1"
auth_port = 27021
acct_port = 27031
status_server = true

[[radius.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "nas.secret"

[radius.accounting_spool]
dir = "spool1"

[tacacs]
bind = "127.0.0.1"
port = 27051

[[tacacs.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "nas.secret"

[metrics]
bind = "127.0.0.1"
port = 27041
```

幾個跟叢集有關的選擇：

- `query_timeout_ms = 2000`：叢集失去多數節點時，腳本的寫入最多等這麼久就失敗，計費才會寫進 spool 並回應 NAS。它要比 NAS 的重送間隔短。ha-cluster 情境用的也是這個值。
- `migrations_wait_ms = 120000`：三個節點一起重開時，先起來的節點要等其他節點，給它長一點的時間。
- `[radius.accounting_spool]`：寫不進資料庫時，計費先寫在本機磁碟，之後重送（[第 13 章](13-accounting.md)）。每個節點要用自己的目錄。
- `[accounts]`、`[sessions]`：讓本章可以用 `user add` 與 `session list` 確認複寫（[第 07 章](07-users-and-chains.md)、[第 13 章](13-accounting.md)）。

把 `node1.toml` 複製成 `node2.toml`、`node3.toml`，照上表改 `node_id`、`data_dir`、RADIUS、TACACS+、指標的埠，以及 `[radius.accounting_spool]` 的 `dir`（`spool2`、`spool3`）。三份的 `[[cluster.nodes]]` 完全相同。

```console
$ for n in 1 2 3; do rradius-server check --config node$n.toml; done
configuration ok
configuration ok
configuration ok
```

### 一起啟動

三個節點要一起啟動：第一個起來的節點要等到湊滿過半數才會開始服務。每個節點開一個終端機，或像這樣放到背景，日誌寫進檔案：

```console
$ for n in 1 2 3; do rradius-server server --config node$n.toml > node$n.log 2>&1 & done
```

在這台機器上，從啟動到三個節點的 `/readyz` 都回 `ok`，大約 9 秒。節點 1 的日誌（拿掉時間）：

```text
WARN hiqlite::store::state_machine::sqlite::writer: No metadata exists inside the DB yet
WARN hiqlite::store::state_machine::sqlite::writer: No metadata exists inside the DB yet
ERROR hiqlite::init: Config: Raft node has not been initialized
…
INFO rradius_core::cli: cluster node 1 started
INFO rradius_core::cli: cluster node 1 has caught up with the cluster
INFO rradius_core::cli: Servers started. Press Ctrl+C to stop.
INFO rradius_core::radius: RADIUS Server listening on 127.0.0.1:27021 and 127.0.0.1:27031
INFO rradius_core::tacacs: TACACS+ Server listening on 127.0.0.1:27051
INFO rradius_core::metrics: Metrics listening on 127.0.0.1:27041
```

節點 2、3 的日誌在叢集成形之前會有好幾行 `ERROR hiqlite::init: … Raft is not initialized` 或 `Connection refused`：它們在等節點 1 建好叢集。只要接著出現 `has caught up with the cluster`，這些 ERROR 就不必理會。

## 確認叢集成形

### cluster members

`cluster members` 問設定裡的節點誰是 leader，再列出 leader 看到的成員：

```console
$ rradius-server cluster members --config node1.toml
node 1  voter    leader  raft 127.0.0.1:27001  api 127.0.0.1:27011
node 2  voter            raft 127.0.0.1:27002  api 127.0.0.1:27012
node 3  voter            raft 127.0.0.1:27003  api 127.0.0.1:27013
```

- `voter`：參與投票、算在過半數裡的節點。剛加入、還在追進度的節點會暫時是 `learner`。
- `leader`：目前的 leader。哪個節點當 leader 不固定。
- 用哪個節點的設定檔都可以，指令會自己找到 leader。
- 結束碼：0 成功；2 是參數或設定錯誤，或 API 密碼不對；3 是 30 秒內沒有任何節點回報 leader。

### status 的 Node 區段

`status` 讀一個節點的指標埠。`Node` 區段是這個節點眼中的叢集：

```console
$ rradius-server status --config node2.toml
rradius status of 127.0.0.1:27042 over 1.0 s

Node
  Ready                     ok
  Leader                    0
  Has a leader              1
  Caught up                 1
  Applied index             16
  Seconds without a leader  0
  Version                   0.1.0
  Seconds running           13
…
```

| 行 | 意思 |
| --- | --- |
| `Ready` | `/readyz` 的內容：`ok`、`no leader`、`stopping` |
| `Leader` | 這個節點是不是 leader（1 是） |
| `Has a leader` | 這個節點有沒有可以寫入的 leader |
| `Caught up` | 啟動後是否已經追上叢集的進度 |
| `Applied index` | 本機副本套用到第幾筆 Raft 記錄；各節點的數字應該很接近 |
| `Seconds without a leader` | 沒有 leader 多久了；有 leader 時是 0 |

`status` 其他區段的說明在[第 17 章](17-monitoring.md)。

## 確認寫入會複寫

目標：寫進一個節點，從另一個節點讀到。

透過節點 1 的設定新增帳號，再對三個節點各登入一次：

```console
$ printf 'erin-password\n' | rradius-server user add --config node1.toml erin
added erin
$ rradius-server user show --config node3.toml erin
username=erin
enabled=1
$ for n in 1 2 3; do rradius-bench run --server 127.0.0.1 --auth-port 2702$n --secret "$(cat nas.secret)" \
    --user erin --password erin-password --load pap,concurrency=1 --requests 1 | grep 'pap:'; done
  pap: 26/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
  pap: 26/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
  pap: 26/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
```

再把一筆計費送到節點 2，從節點 3 看：

```console
$ rradius-bench run --server 127.0.0.1 --acct-port 27032 --secret "$(cat nas.secret)" \
    --user erin --load acct-start,concurrency=1 --requests 1 | grep 'acct-start:'
  acct-start: 729/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
$ rradius-server session list --config node3.toml
username=erin
nas=192.0.2.1
session_id=4220000000000000
started=1795384786
updated=1795384786
```

節點 2 的腳本寫入，交給 leader；leader 等過半數確認後才回覆，節點 2 才回 Accounting-Response。等 `accounting 1` 出現時，資料已經在叢集裡了。

## 停掉 leader

目標：停掉 leader，看叢集自己選出新的 leader，寫入恢復。

先用 `cluster members` 找出 leader（上面是節點 1），對它送 SIGTERM。同時每 0.5 秒對節點 2 送一筆計費，看寫入有沒有中斷。這台機器上量到的過程：

| 從 SIGTERM 算起 | 舊 leader（節點 1） | 對節點 2 的計費 |
| --- | --- | --- |
| 0 秒 | `/readyz` 改成 503 `stopping`，但照常回答請求 | 照常 |
| 5 秒 | 停止接收新請求（`ready_delay_ms` 預設 5000），等處理中的請求 | 照常 |
| 約 14.5 秒 | 關閉資料庫，行程結束 | 開始變慢，有一筆逾時 |
| 約 19 秒 | — | 節點 3 被選為新 leader |
| 約 25 秒 | — | 恢復正常 |

- 舊 leader 在關機的 14.5 秒裡一直還是 leader，所以寫入照常。真正沒有 leader 的時間，是它關閉資料庫之後到新 leader 能接寫入之間，這台機器上大約 10 秒。
- 這段時間裡，只讀資料的登入不受影響：follower 用本機副本回答。
- 節點 2 的 `rradius_cluster_leader` 一直是 1：指標每秒更新一次，follower 在選舉期間仍認得某個 leader。所以不要只靠這個指標判斷寫入有沒有中斷，要一起看計費的回應與計費 spool。

新 leader 選出後：

```console
$ rradius-server cluster members --config node2.toml
node 1  voter            raft 127.0.0.1:27001  api 127.0.0.1:27011
node 2  voter            raft 127.0.0.1:27002  api 127.0.0.1:27012
node 3  voter    leader  raft 127.0.0.1:27003  api 127.0.0.1:27013
```

節點 1 已經停了，但它還是 voter：停機不等於離開叢集。三個 voter 停了一個，剩下兩個仍是過半數，所以還能寫。

再啟動節點 1，它以 follower 身分回來，追上進度後才開始服務：

```text
INFO rradius_core::cli: cluster node 1 started
INFO rradius_core::cli: cluster node 1 has caught up with the cluster
INFO rradius_core::cli: Servers started. Press Ctrl+C to stop.
```

> **注意：** 不要用 `kill -9` 停節點。強制結束的節點，下次啟動時要重建本機的資料庫（見[重開節點與重建資料](#重開節點與重建資料)）。多節點的叢集裡，這台機器上量到 SIGTERM 之後約 15 秒行程才結束，systemd、Docker、Kubernetes 的等待時間都要比這長，見[第 18 章](18-operations.md)。

## 失去多數節點

目標：看一個節點湊不到過半數時怎麼做，以及 `stop_without_leader_ms` 的作用。

在節點 1 的 `[cluster]` 加上：

```toml
# node1.toml（加在 [cluster]）
stop_without_leader_ms = 10000
```

重開節點 1 讓它生效，然後對節點 2、3 送 SIGTERM，等兩個行程都結束。之後每隔十幾秒，對節點 1 送一次 PAP 登入、Status-Server、計費與 TACACS+ 登入。實測結果：

| 兩個節點停掉之後 | `Seconds without a leader` | `/readyz` | PAP 登入 | Status-Server | 計費 | TACACS+ 登入 |
| --- | --- | --- | --- | --- | --- | --- |
| 0 秒 | 0 | 200 `ok` | accept | 回答 | 沒有回應 | accept |
| 8 秒 | 2 | 200 `ok` | accept | 回答 | 沒有回應 | accept |
| 16 秒 | 10 | 503 `no leader` | 沒有回應 | 沒有回應 | 回應（寫進 spool） | ERROR |
| 69 秒 | 63 | 503 `no leader` | 沒有回應 | 沒有回應 | 回應（寫進 spool） | ERROR |

- 節點 1 大約 6 秒後才發現沒有 leader。
- 前幾秒的計費沒有回應：腳本的寫入在等 leader，等到 `query_timeout_ms` 就失敗。NAS 會重送。
- 沒有 leader 超過 5 秒後，寫入立刻失敗，計費寫進 spool 並回應 NAS。
- 沒有 leader 超過 10 秒（`stop_without_leader_ms`）後，登入與 Status-Server 都不回應，算進 `rradius_radius_discarded_total{reason="no_leader"}`；TACACS+ 認證回 ERROR（`rradius-bench` 印出 `status: 0x07`）。`/livez` 一直是 200：行程還活著，只是不就緒。

`status` 的樣子：

```console
$ rradius-server status --config node1.toml
rradius status of 127.0.0.1:27041 over 1.0 s

Node
  Ready                     no leader
  Leader                    0
  Has a leader              0
  Caught up                 1
  Applied index             96
  Seconds without a leader  84
…
Accounting spool
  Bytes waiting  545
  Replayed       0.0/s (0)
…
```

日誌會一直出現 `Find DB leader error: Connect: Leader vote is in progress`，腳本的寫入失敗會寫成 `Lua script error (radius/accounting): runtime error: no leader for 83 s`。

再啟動節點 2、3。叢集恢復過半數後幾秒內，節點 1 的 `/readyz` 回到 `ok`，spool 裡的 5 筆計費自動重送進資料庫：

```text
rradius_cluster_leader 1
rradius_cluster_seconds_without_leader 0
rradius_acct_spool_written_total 5
rradius_acct_spool_replayed_total 5
rradius_acct_spool_bytes 0
```

## 重開節點與重建資料

### 一般的重開

停掉一個節點（SIGTERM）再啟動，它用自己 `data_dir` 裡的資料回來，從 leader 補上停機期間的寫入，追上之後才服務。這是升級、改 `[cluster]` 設定、換主機硬體時的標準做法。

> **注意：** 實測時，叢集剛恢復過半數、還在選舉的那幾秒裡啟動節點 2，它在 9 秒後結束，結束碼 1，最後一行是 `not ready to apply migrations: cannot read applied migrations: CheckIsLeaderError: has to forward request to: None, None (the limit is migrations_wait_ms; raise it if a migration file takes longer)`。再啟動一次就正常了。用 systemd 的 `Restart=on-failure` 或容器的重啟政策，這種情況會自己恢復。

### 強制結束之後

用 `kill -9` 強制結束節點 3 再啟動，日誌：

```text
WARN hiqlite_wal::log_store: LockFile data3/logs exists already - this is not a clean start!
WARN hiqlite::store::state_machine::sqlite::state_machine: Lock file already exists: data3/state_machine/lock\nNode did not shut down gracefully - auto-rebuilding State Machine
…
INFO rradius_core::cli: cluster node 3 started
INFO rradius_core::cli: cluster node 3 has caught up with the cluster
```

節點沒有正常關閉時，下次啟動會從 Raft 記錄重建本機的資料庫。資料量小時很快，資料量大時會花比較久。

伺服器自己也會在某些情況下主動結束，結束碼 1，讓行程管理程式把它重開：

- 節點的 Raft 核心因錯誤停止，例如 follower 落後到 leader 已經丟掉它需要的記錄。日誌寫 `cluster node N: its Raft core stopped (…); exiting so that a restart recovers it`。
- 叢集的程式庫裡發生 panic：`cluster node: a panic in its database (…); exiting so that a restart recovers it`。

### 從其他節點重建

一個節點的磁碟壞了、資料目錄不見了，或你懷疑它的資料有問題：停掉它，清空它的 `data_dir`，用同一份設定（同樣的 `node_id` 與位址）再啟動。它會從其他節點把資料抄回來。

```console
$ rm -rf data3
$ rradius-server server --config node3.toml > node3.log 2>&1 &
```

```text
WARN hiqlite::init: Found remote metrics and we (3) are a Raft member
…
INFO rradius_core::cli: cluster node 3 started
INFO rradius_core::cli: cluster node 3 has caught up with the cluster
```

這台機器上從啟動到就緒大約 2 秒，之後 `erin` 在節點 3 登入是 accept。這個做法要求其他節點仍有過半數在運作。

> **警告：** 不要同時清空多數節點的 `data_dir`。資料只存在叢集的節點上，過半數的副本都沒了，資料就沒了。

### 整個叢集一起重開

三個節點一起停、一起啟動，跟第一次啟動一樣：每個節點等到湊滿過半數、追上進度才服務，最多等 `migrations_wait_ms`。所以 `migrations_wait_ms` 要比你預期「最後一個節點起來」所需的時間長。

只重開一個節點、另外兩個還沒回來時（沒有設 `stop_without_leader_ms`、所有 migration 本機都已套用），程式的設計是：等不到 leader 就比對本機副本已套用的 migration，相符時印出警告，用本機副本服務。實測了三次，結果不一致：

- 一次照設計：大約 4 秒後印出 `cluster node 1: no usable leader within 3.981301833s; serving without a leader before catching up, so answers come from this node's replica, which may be behind`，然後用本機副本服務，`rradius_cluster_caught_up` 是 0。另外兩個節點回來後，日誌寫 `cluster node 1 has caught up with the cluster`，`rradius_cluster_caught_up` 變成 1。
- 兩次（`migrations_wait_ms = 10000`）沒有服務，結束碼 1：一次印出 `node 2 did not join within 10s`，另一次印出 `cannot read applied migrations: no answer within 8.990975917s`。

設了 `stop_without_leader_ms` 的節點，湊不到 leader 時一律結束，結束碼 1，不會用舊資料服務。

結論：失去多數節點時，不要指望重開某個節點就能讓它服務；先把其他節點救回來。

## 加入節點

目標：讓第 4 個節點加入運作中的叢集，其他節點都不必重開。

新節點會自己加入，沒有「加入」的指令。條件：

- 新節點的 `data_dir` 是空的（或不存在）。
- 它的設定的 `[[cluster.nodes]]` 列出它自己，以及目前的 leader。最簡單的做法是列出全部的節點。
- `node_id` 沒有被別的運作中節點用掉。

依照[節點 1 的設定](#節點-1-的設定)的表做 `node4.toml`，`[[cluster.nodes]]` 列出節點 1 到 4，然後啟動：

```console
$ rradius-server server --config node4.toml > node4.log 2>&1 &
```

這台機器上大約 4 秒後就緒，日誌依序是 `cluster node 4 started`、`has caught up with the cluster`、`Servers started`。加入前建立的帳號馬上能在節點 4 登入：

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 27024 --secret "$(cat nas.secret)" \
    --user erin --password erin-password --load pap,concurrency=1 --requests 1 | grep 'pap:'
  pap: 22/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
$ rradius-server cluster members --config node1.toml
node 1  voter            raft 127.0.0.1:27001  api 127.0.0.1:27011
node 2  voter            raft 127.0.0.1:27002  api 127.0.0.1:27012
node 3  voter    leader  raft 127.0.0.1:27003  api 127.0.0.1:27013
node 4  voter            raft 127.0.0.1:27004  api 127.0.0.1:27014  (not in this configuration)
```

`(not in this configuration)` 表示 `node1.toml` 的清單裡沒有節點 4。這不影響運作：節點 1 到 3 不必為了節點 4 重開，重開時也照常回來。用 `node4.toml` 看就沒有這個標記。之後找時間把節點 4 加進其他節點的設定，讓各節點的清單一致，`cluster members` 才不會一直提醒你。

> **注意：** 節點的總數通常保持奇數。從 3 個加到 4 個，過半數從 2 變 3，可以同時停掉的節點還是只有 1 個。

### id 被用掉時

如果新節點的 `node_id` 是某個還在運作的節點的 id，但位址不同，它會拒絕啟動，結束碼 2：

```text
cannot start cluster node 2: node id 2 is in use: the cluster has it at api 127.0.0.1:27012 (raft 127.0.0.1:27002), and that node answers, but this node's configuration gives it api 127.0.0.1:27015 (raft 127.0.0.1:27005); use an id no node has (see `rradius-server cluster members`), or stop the running node first
```

用同一個 id 加入會改寫叢集裡那個節點的位址，所以程式不允許。換掉一台主機時，要嘛用新的 id，要嘛先停掉舊的節點。

## 移除節點

目標：把節點 4 移出叢集。

`cluster remove` 會依序檢查，任何一項不通過就什麼都不送：

1. id 要是正整數：`the node id must be a positive integer, not 'abc'`，結束碼 2。
2. 這個 id 要是成員：`no node 9 in the cluster`，結束碼 1。
3. 這個節點要已經停了：還在運作的節點不能移除。
4. 移除後剩下的 voter 不能少於三個，除非加 `--force`；剩下零個一律拒絕。

先試著在節點 4 還在跑的時候移除：

```console
$ rradius-server cluster remove --config node1.toml 4
node 4 still answers at 127.0.0.1:27014: stop it first, then remove it
```

結束碼 2。還在運作的節點被移除後會繼續用它手上的舊資料回答，所以一定要先停。

停掉節點 4（SIGTERM，這台機器上 15 秒後結束），再移除：

```console
$ rradius-server cluster remove --config node1.toml 4
removed node 4
$ rradius-server cluster members --config node1.toml
node 1  voter            raft 127.0.0.1:27001  api 127.0.0.1:27011
node 2  voter            raft 127.0.0.1:27002  api 127.0.0.1:27012
node 3  voter    leader  raft 127.0.0.1:27003  api 127.0.0.1:27013
```

移除會讓叢集只剩兩個 voter 時：

```console
$ rradius-server cluster remove --config node1.toml 3
removing node 3 leaves two voters, and the cluster stops writing when either stops; give --force to remove it anyway
$ rradius-server cluster remove --force --config node1.toml 3
removed node 3
$ rradius-server cluster members --config node1.toml
node 1  voter    leader  raft 127.0.0.1:27001  api 127.0.0.1:27011
node 2  voter            raft 127.0.0.1:27002  api 127.0.0.1:27012
node 3  not in the cluster (listed in this configuration)
```

`not in the cluster (listed in this configuration)` 表示設定裡有、叢集裡沒有。

被移除的節點要再加回來，就照[加入節點](#加入節點)的條件：清空它的 `data_dir` 再啟動。上面的節點 3 清空 `data3` 後啟動，2 秒內就以 voter 身分回到叢集。

`cluster remove` 的結束碼：0 成功；1 是沒有這個成員，或叢集拒絕移除；2 是參數、設定錯誤，或檢查沒通過；3 是 30 秒內沒有回應。結束碼 3 時，移除可能已經做了，訊息會叫你用 `cluster members` 確認，不要直接重跑。

## 檢查各節點的設定

各節點的設定應該只差在本機的值。`config diff` 比較兩份設定（含預設值），列出不同之處；`cluster.node_id` 與 `cluster.listen` 這兩個本來就該不同的鍵，列在最後的 `node-specific:` 下面：

```console
$ rradius-server config diff node1.toml node2.toml
cluster.data_dir: "data1" | "data2"
cluster.stop_without_leader_ms: 10000 | (absent)
metrics.port: 27041 | 27042
radius.accounting_spool.dir: "spool1" | "spool2"
radius.acct_port: 27031 | 27032
radius.auth_port: 27021 | 27022
tacacs.port: 27051 | 27052
node-specific:
  cluster.node_id: 1 | 2
```

這是單機練習的結果，埠與目錄都不同。多台主機時差異會少很多，例如 ha-cluster 情境的兩個節點：

```console
$ rradius-server config diff node1.toml node2.toml
metrics.bind: "10.99.0.1" | "10.99.0.2"
node-specific:
  cluster.listen: "10.99.0.1" | "10.99.0.2"
  cluster.node_id: 1 | 2
```

有差異時結束碼是 1，完全相同（只差 `node-specific`）時是 0。改了設定之後、推到各節點之前，用它確認沒有只改到一台。祕密只顯示 `differs (hidden)`，不會印出內容。

## 看整個叢集的狀態

`status --cluster` 向叢集要成員清單，同時讀每個成員的指標埠，一行一個節點：

```console
$ rradius-server status --cluster --config node1.toml
rradius cluster status over 1.0 s, members from the cluster

node   role    ready  auth/s  acct/s  discarded/s  in progress  address
1      leader  ok     0.0     0.0     0.0          0            127.0.0.1:27041
2      leader  ok     0.0     0.0     0.0          0            127.0.0.1:27041
3      leader  ok     0.0     0.0     0.0          0            127.0.0.1:27041
total          3/3    0.0     0.0     0.0          0
```

這份輸出是錯的，而且是預期中的錯：三行都是節點 1。原因是 `status --cluster` 用每個成員 `api` 位址的主機名稱，加上**這個節點自己的指標埠**，去讀其他成員。三個節點都在 `127.0.0.1`、指標埠又各不相同，結果三行讀到的都是 `127.0.0.1:27041`。

正式部署時要讓它正確，每個節點要：

- 用相同的 `[metrics] port`；
- `[metrics] bind` 設成其他節點連得到的位址，而且就是 `api` 位址的那個主機。

讀不到的成員會顯示 `unreachable`，下面附上原因與提示 `every node's [metrics] bind has to be an address this host can reach, on the same port as here`。

> **未實機驗證：** 多台主機上 `status --cluster` 的正確輸出。本章只在單機跑過，結果如上。

`--cluster` 也能配 `--watch`、`--json`。`status` 的各欄說明在[第 17 章](17-monitoring.md)。

## 多台主機：ha-cluster 情境

`rradius-server/examples/scenarios/ha-cluster/` 是三台主機的範例：`node1.toml`、`node2.toml`、`node3.toml`，三份只差 `node_id`、`listen` 與 `[metrics] bind`。

| 設定 | 值 | 為什麼 |
| --- | --- | --- |
| `cluster.listen` | 各自的 WireGuard 位址 `10.99.0.N` | Raft 與 API 埠只聽 WireGuard 介面 |
| `cluster.nodes` | `10.99.0.1` 到 `10.99.0.3`，埠 8100、8200 | 每台主機用同樣的埠 |
| `cluster.migrations_wait_ms` | `120000` | 三台一起重開時，等其他節點的時間 |
| `lua.query_timeout_ms` | `2000` | 失去多數時寫入早點失敗，計費寫進 spool |
| `radius.bind` | `0.0.0.0` | NAS 從實體網路連進來 |
| `radius.clients` | `10.80.0.0/16`，共用密碼三台相同 | 每台 NAS 把三個節點都設成 RADIUS 伺服器 |
| `radius.status_server` | `true` | NAS 與負載平衡器用 Status-Server 判斷節點活著 |
| `[radius.accounting_spool]` | `spool`，最多 1 GiB | 失去多數時計費先寫在本機 |
| `metrics.bind` | 各自的 WireGuard 位址，埠 9812 | 監控從內部網路抓；三台同埠，`status --cluster` 才讀得到 |
| `log.format` | `json` | 方便集中收集 |

照[第 03 章](03-quick-start.md)的做法把情境複製到工作目錄、把 `dictionary` 改成字典的絕對路徑後，三份都能通過 `check`（會警告範例的公開密碼與檔案權限）：

```console
$ for n in 1 2 3; do rradius-server check --config node$n.toml; done 2>&1 | grep -v WARN
configuration ok
configuration ok
configuration ok
```

`check` 不會開任何埠，所以在沒有 `10.99.0.x` 位址的機器上也能檢查。要真的跑起來，要三台主機與 WireGuard。

> **未實機驗證：** ha-cluster 情境在三台主機、經 WireGuard 連線的實際運作。本章的行為都在單機上用改過埠的設定驗證。

部署時記得：

- 三台主機的 `raft.secret`、`api.secret`、`nas.secret` 要相同，換成自己用 `openssl rand -hex 32` 產生的值，權限 0600。
- `migrations/2_demo_users.sql` 是示範帳號，正式環境要刪掉。
- NAS 把三個節點都設成伺服器。設了 `stop_without_leader_ms` 的話，被隔開的節點會停止回應，NAS 就會換問別台。

## 用 Docker Compose 部署

原始碼根目錄的 `compose.cluster.yaml` 在一台 Docker 主機上起三個節點，設定檔在 `docker/config/cluster/node1.toml` 到 `node3.toml`：

| 項目 | 內容 |
| --- | --- |
| 服務 | `node1`、`node2`、`node3`，image `rradius-server`，從原始碼的 `Dockerfile` 建置 |
| RADIUS 埠 | 主機的 1812/1813、2812/2813、3812/3813 對應到各容器的 1812/1813（UDP） |
| 設定 | `./docker/config` 唯讀掛到 `/etc/rradius` |
| 資料 | 每個節點一個 volume：`data1`、`data2`、`data3`，掛到 `/var/lib/rradius` |
| 密碼 | Compose secrets：`docker/secrets/raft.secret`、`api.secret`、`radius.secret`，掛到 `/run/secrets/` |
| 安全 | 根檔案系統唯讀、`cap_drop: ALL`、`no-new-privileges` |
| 停止 | `stop_grace_period: 30s`，`restart: unless-stopped` |

節點設定的重點：

- 三份只差 `node_id`。
- `listen = "0.0.0.0"`：容器只有一個位址，其他容器要連得到。
- `nodes` 用服務名稱：`node1:8100`、`node1:8200`，以此類推。
- `migrations_dir = "../migrations"`，也就是 `docker/config/migrations/`。
- `[metrics]` 聽容器內的 `127.0.0.1:9812`，給容器內的 `status` 用。

啟動的步驟（`compose.cluster.yaml` 開頭的註解）：

```console
$ sh docker/init-secrets.sh
$ docker compose -f compose.cluster.yaml up --build
```

`init-secrets.sh` 產生三個隨機的密碼檔，已經存在的不覆蓋。在容器裡看狀態：

```console
$ docker compose -f compose.cluster.yaml exec node1 rradius-server status --config /etc/rradius/cluster/node1.toml
$ docker compose -f compose.cluster.yaml exec node1 rradius-server cluster members --config /etc/rradius/cluster/node1.toml
```

停掉 leader 的練習，用 `docker compose -f compose.cluster.yaml stop node1`（送 SIGTERM，最多等 30 秒）。

本書驗證過的部分：在工作目錄複製一份，`init-secrets.sh` 產生密碼後，`docker compose -f compose.cluster.yaml config` 通過；三份節點設定把字典與密碼路徑換成本機路徑後，`check` 都通過。

> **未實機驗證：** 用 `compose.cluster.yaml` 實際建置 image、起三個容器、停掉 leader 的過程。上面的 `exec` 指令沒有實際跑過。單機的行為與本章前面的練習相同，因為容器用的是同一個程式。

> **注意：** 三個容器在同一台 Docker 主機上，這台主機停了，整個叢集就停了。它適合測試與小型環境；要扛主機故障，節點要分散在不同主機上，像 ha-cluster 情境那樣。`[metrics]` 只聽容器內的位址，所以容器內的 `status --cluster` 讀不到其他節點。

## 在 Kubernetes 上部署

`deploy/kubernetes/` 用 StatefulSet 跑三個節點，`kubectl apply -k deploy/kubernetes` 套用。各檔案：

| 檔案 | 內容 |
| --- | --- |
| `kustomization.yaml` | 套用 `statefulset.yaml`、`services.yaml`、`pdb.yaml`；把 `config/` 產生成 ConfigMap `rradius-config` |
| `statefulset.yaml` | 3 個 pod `rradius-0` 到 `rradius-2`，`podManagementPolicy: Parallel`，每個 pod 一個 1Gi 的 PVC |
| `services.yaml` | headless Service `rradius-cluster`（節點互相找）；LoadBalancer Service `rradius`（給 NAS） |
| `pdb.yaml` | PodDisruptionBudget `maxUnavailable: 1` |
| `config/rradius-0.toml` 到 `rradius-2.toml` | 每個 pod 一份，只差 `node_id`：pod `rradius-N` 是節點 N+1 |
| `config/auth.lua`、`config/1_schema.sql` | 腳本與 migration |
| `secret.example.yaml` | Secret `rradius-secrets` 的樣子，不會被套用 |
| `networkpolicy.yaml` | 預設不套用：RADIUS 埠對外開放，叢集埠只給其他節點，指標埠只給 `monitoring` namespace |
| `hostnetwork-patch.yaml` | 預設不套用：沒有保留來源位址的負載平衡器時，讓 pod 用節點的網路 |

### 先建 Secret

```console
$ kubectl create secret generic rradius-secrets \
    --from-literal=raft="$(openssl rand -hex 32)" \
    --from-literal=api="$(openssl rand -hex 32)" \
    --from-literal=radius="<你的 NAS 共用密碼>"
```

Secret 掛在 `/run/secrets/rradius`，設定用 `secret_raft_file = "/run/secrets/rradius/raft"` 這類路徑讀它。三個 pod 用同一個 Secret，叢集密碼自然相同。

### 這份範例為什麼這樣設計

每一點都對應到本章前面的行為：

- **`podManagementPolicy: Parallel`：** 三個 pod 一起啟動。節點要湊滿過半數才服務，一個一個啟動的話，第一個會一直等。
- **`publishNotReadyAddresses: true`（headless Service）：** 還沒就緒的 pod 也保有 DNS 名稱。不然還在等 leader 的節點彼此找不到，就選不出 leader。
- **`nodes` 用 DNS 名稱：** `rradius-0.rradius-cluster:8100` 這種形式；`listen = "0.0.0.0"`，因為其他 pod 透過 pod 位址連進來。
- **探針：** 指標埠在節點追上進度後才開，所以先只跑 `startupProbe`（`/livez`，每 5 秒、最多 60 次）。之後 `livenessProbe` 看 `/livez`，`readinessProbe` 看 `/readyz`。
- **`stop_without_leader_ms = 10000`：** 被隔開的 pod 停止回應、`/readyz` 變 503，Service 就不把 NAS 的流量送給它。
- **`ready_delay_ms` 預設 5000：** 收到 SIGTERM 後 `/readyz` 先回 503，Service 先把流量移走，pod 才停止接收請求。`terminationGracePeriodSeconds: 30` 涵蓋整個關機。
- **`externalTrafficPolicy: Local`：** rradius 用來源位址分辨 NAS，來源位址要原封不動到 pod。
- **PodDisruptionBudget `maxUnavailable: 1`：** 排空 Kubernetes 節點這類自願中斷，一次只動一個 pod，三個裡留兩個，保住過半數。
- **`[metrics] bind = "0.0.0.0"`：** kubelet 從 pod 位址探測。三個 pod 同埠 9812，照程式的規則 `status --cluster` 應該讀得到其他成員；套用 `networkpolicy.yaml` 之後，9812 只開給 `monitoring` namespace，pod 之間就讀不到了。

### 改 NAS 的位址

`config/rradius-*.toml` 的 `[[radius.clients]]` 是文件用的網段 `192.0.2.0/24`，要改成你的 NAS 實際的位址。不要寫太大的網段：網段如果也涵蓋 Kubernetes 節點的位址，被負載平衡器改過來源位址的封包也會被接受，所有 NAS 都會被當成同一個用戶端。

### 在 pod 裡下指令

```console
$ kubectl exec -i rradius-0 -- rradius-server user add --config /etc/rradius/rradius-0.toml alice
$ kubectl exec rradius-0 -- rradius-server cluster members --config /etc/rradius/rradius-0.toml
$ kubectl exec rradius-0 -- rradius-server status --cluster --config /etc/rradius/rradius-0.toml
```

`user add` 從標準輸入讀密碼，所以要 `-i`。設定改了之後怎麼套用（ConfigMap 換名字、pod 逐一重啟），以及新增 migration 要改哪裡，見[第 18 章](18-operations.md)。

本書驗證過的部分：在工作目錄複製一份，`kubectl kustomize` 能產生 ConfigMap、2 個 Service、PodDisruptionBudget 與 StatefulSet；三份 `rradius-*.toml` 把字典與密碼路徑換成本機路徑後，`check` 都通過。

> **未實機驗證：** 實際套用到 Kubernetes 叢集、pod 啟動、刪掉 leader pod 的過程，以及 `hostnetwork-patch.yaml`、`networkpolicy.yaml` 的效果。上面的 `kubectl exec` 指令沒有實際跑過。

## 叢集的指標

每個節點的 `/metrics` 都有這些，數值是這個節點自己的看法：

| 指標 | 型別 | 意思 |
| --- | --- | --- |
| `rradius_cluster_leader` | gauge | 1 表示有可以寫入的 leader |
| `rradius_cluster_is_leader` | gauge | 1 表示這個節點就是 leader |
| `rradius_cluster_applied_index` | gauge | 本機副本套用到的 Raft 記錄編號 |
| `rradius_cluster_seconds_without_leader` | gauge | 沒有 leader 多少秒；有 leader 時是 0 |
| `rradius_cluster_caught_up` | gauge | 1 表示啟動後已追上叢集；0 表示正在用可能落後的副本服務 |
| `rradius_db_batched_writes_total` | counter | 跟其他寫入合在一個交易裡提交的腳本寫入 |
| `rradius_db_split_batches_total` | counter | 合併的交易失敗、拆開來找出失敗的那一句的次數 |
| `rradius_radius_discarded_total{reason="no_leader"}` | counter | 因 `stop_without_leader_ms` 沒有回應的請求 |
| `rradius_radius_discarded_total{reason="database"}` | counter | 因資料庫無法使用（例如寫入失敗）沒有回應的請求 |

建議的告警方向：

- 整個叢集加起來，`rradius_cluster_is_leader` 的總和應該是 1。是 0 表示沒有 leader。
- `rradius_cluster_seconds_without_leader` 持續大於 0。
- 某個節點的 `rradius_cluster_applied_index` 一直落後其他節點。
- `rradius_cluster_caught_up` 是 0 太久。
- 計費 spool 的位元組數（`rradius_acct_spool_bytes`）持續增加，表示寫不進資料庫。

原始碼的 `deploy/monitoring/` 有現成的規則，見[第 17 章](17-monitoring.md)。每個指標的完整說明在[附錄 C](92-metrics-reference.md)。

## 常見錯誤

**症狀：** `check` 印出 `cluster.nodes: node 1 is required`。
**原因：** `[[cluster.nodes]]` 裡沒有 id 為 1 的節點。
**解法：** 節點 id 從 1 開始編。

**症狀：** `check` 印出 `cluster.node_id 4 is not in cluster.nodes`。
**原因：** 這個節點的 `node_id` 沒有列在自己設定的清單裡。
**解法：** 在 `[[cluster.nodes]]` 加上自己這一項。

**症狀：** `check` 印出 `cluster.secret_raft must be at least 16 characters`，或 `exactly one of cluster.secret_api or cluster.secret_api_file must be set`。
**原因：** 叢集密碼太短，或兩種寫法都寫了、都沒寫。
**解法：** 用 `openssl rand -hex 32 > raft.secret` 產生，設定只寫 `secret_raft_file`。

**症狀：** `check` 或啟動時警告 `cluster.listen is not set, so the cluster's Raft and API ports are on all interfaces, …`。
**原因：** 有 `[[cluster.nodes]]` 但沒有 `listen`。
**解法：** 設成節點的內部或 WireGuard 位址；真的要聽所有位址就寫 `listen = "0.0.0.0"`。

**症狀：** 第二、三個節點的日誌一開始有很多 `ERROR hiqlite::init: … Raft is not initialized` 或 `Connection refused`。
**原因：** 節點 1 還沒建好叢集，或其他節點還沒啟動。
**解法：** 等。出現 `has caught up with the cluster` 就沒事。一直沒出現的話，確認三個節點都啟動了、`raft` 與 `api` 位址互相連得到。

**症狀：** 節點啟動後結束，印出 `not ready to apply migrations: node 2 did not join within 10s (the limit is migrations_wait_ms; raise it if a migration file takes longer)`，結束碼 1。
**原因：** 在 `migrations_wait_ms` 內湊不到過半數：其他節點沒在跑，或連不到。
**解法：** 先讓其他節點起來；一起重開時把 `migrations_wait_ms` 設長一點。

**症狀：** 啟動時印出 `cannot start cluster node 2: node id 2 is in use: …`，結束碼 2。
**原因：** 叢集裡另一個位址的節點已經用了這個 id，而且還在運作。
**解法：** 用沒人用的 id（先看 `cluster members`），或先停掉那個節點。

**症狀：** `cluster members` 或 `status --cluster` 印出 `127.0.0.1:27011 refused the api secret: [cluster] secret_api differs from the cluster's`，結束碼 2。
**原因：** 這份設定的 API 密碼跟叢集的不同。
**解法：** 用跟叢集相同的 `api.secret`。

**症狀：** `cluster members` 停了 30 秒，印出 `the cluster did not answer within 30 s`，結束碼 3。
**原因：** 設定裡的節點都沒有回應，或都沒有 leader。
**解法：** 確認節點在跑、API 埠連得到；失去多數節點時，先把節點救回來。

**症狀：** `cluster remove` 印出 `node 4 still answers at 127.0.0.1:27014: stop it first, then remove it`。
**原因：** 要移除的節點還在運作。
**解法：** 先停掉它，再移除。

**症狀：** `cluster remove` 印出 `removing node 3 leaves two voters, and the cluster stops writing when either stops; give --force to remove it anyway`。
**原因：** 移除後只剩兩個 voter。
**解法：** 先加入新節點再移除舊的；確定要縮成兩個節點才加 `--force`。

**症狀：** 節點的 `/readyz` 回 503 `no leader`，登入沒有回應，`Discarded: no leader` 增加。
**原因：** 設了 `stop_without_leader_ms`，而這個節點沒有 leader 超過這段時間。
**解法：** 讓它跟其他節點恢復連線，或把停掉的節點救回來。`/livez` 仍是 200，不要因此重開它。

**症狀：** `status --cluster` 每一行的位址都一樣，或其他節點顯示 `unreachable`。
**原因：** 各節點的指標埠不同，或 `[metrics] bind` 是其他主機連不到的位址。
**解法：** 每個節點用同一個指標埠，`bind` 設成 `api` 位址的那個主機。

**症狀：** 強制結束過的節點啟動時警告 `Node did not shut down gracefully - auto-rebuilding State Machine`。
**原因：** 上次沒有正常關閉。
**解法：** 不用處理，它會自己重建。之後用 SIGTERM 停，並給它足夠的時間。

## 相關章節

- [03 第一次啟動與登入](03-quick-start.md)
- [04 設定檔的基本功](04-configuration.md)
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)
- [13 計費與 session](13-accounting.md)
- [17 監控、狀態與日誌](17-monitoring.md)
- [18 日常維運](18-operations.md)
- [19 效能調校與壓測](19-performance.md)
- [20 安全強化](20-security.md)
- [附錄 A：設定參考](90-config-reference.md)
- [附錄 C：指標參考](92-metrics-reference.md)
