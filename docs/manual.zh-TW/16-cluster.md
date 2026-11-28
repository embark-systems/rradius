# 叢集與高可用

> 本章讓你做到：
> - 架起三個節點的叢集，確認寫進一個節點的資料，其他節點也讀得到
> - 看懂 leader、follower、複寫與 snapshot，知道失去 leader 時伺服器會怎麼做
> - 處理節點故障：重開節點、從其他節點重建資料
> - 讓新節點加入叢集，安全地移除節點
> - 用 Docker Compose 或 Kubernetes 部署三個節點的叢集

前置條件：做過[第 3 章](03-quick-start.md)，知道怎麼啟動伺服器、用 `rradius-bench` 測登入；讀過[第 4 章](04-configuration.md)，知道 `check` 與 reload。本章的練習在一台機器上起三到四個節點，需要原始碼建出來的 `rradiusd` 與 `rradius-bench`，以及原始碼裡的 `rradius-server/examples/scenarios/ha-cluster/`。

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
- [跨機房](#跨機房)
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

### 停一台與停兩台

三個節點的叢集停掉節點時，認證與計費的影響（沒有 leader 時各種請求的細節見下面的〈沒有 leader 時〉）：

| 狀況 | 認證 | 計費 |
| --- | --- | --- |
| 三台都在 | 正常 | 正常 |
| 停一台 | 正常，NAS 改問其他台。停的剛好是 leader 時，選舉那段時間要寫入的登入沒有回應（時間見[跨機房](#跨機房)） | 正常。停的是 leader 時，選舉那段時間寫入逾時，有計費 spool 就先寫進 spool，之後重放，可能重複一次 |
| 停兩台，剩一台 | 唯讀資料的登入（`sql`、`ldap`）讀本機副本，照常回應；要寫入的（驗證碼、IP 配發）不回應，讓 NAS 重送或改問別台。設了 `stop_without_leader_ms` 時，超過那段時間連唯讀的登入都不回應 | 寫不進資料庫；有計費 spool 時先寫在本機並回覆，叢集恢復後重放 |

因為計費可能被重放、也可能因為逾時重送而記兩次，計費腳本要能重複執行（例如以 `Acct-Session-Id` 為鍵、用 `INSERT OR IGNORE`），見[第 13 章](13-accounting.md)。

**資料庫不能用時不回應，而不是拒絕。** 登入因為叢集資料庫逾時或找不到 leader 而無法決定時，RADIUS 不回覆、TACACS+ 回 ERROR，NAS 與設備就會改問其他節點，使用者也不會看到「密碼錯」。這種請求算進 `rradius_radius_discarded_total{reason="database"}`，日誌在 RADIUS 寫 `not answered: the authentication backend could not be used`，TACACS+ 認證寫 `answered ERROR: the authentication backend could not be used`，TACACS+ 授權寫 `answered ERROR: the database could not be used`。只有確定帳號不存在或密碼錯、而且腳本的資料庫呼叫都成功時才回拒絕。

**被隔開的節點讀的是舊資料。** 它跟多數節點斷開時，在另一側停用的帳號、刪掉的 NAS、撤掉的金鑰，在它這裡都還有效，直到網路恢復。不能接受的話設 `stop_without_leader_ms`。

### 節點追上進度之後才開始服務

一個節點啟動時，不會馬上開始回應請求。它依序做這些事：

1. 啟動本機的叢集節點。第一次啟動的節點要先加入叢集；沒有叢集資料的節點 1 可能要先等其他節點（見[節點 1 在等待時](#節點-1-在等待時)）。接著向其他節點查目前的成員清單（這次查不出結果時，第 2 步之後再查一次）；已被移除的節點在這裡結束，不做之後的步驟（見[被移除的節點又啟動時](#被移除的節點又啟動時)）。
2. 等到有可以寫入、而且回應過一次讀取的 leader，最多等 `migrations_wait_ms`（預設 60000 毫秒）。節點停下前記得的狀態不算數。時間內沒有 leader 回應時，節點不做第 3、4 步，而是照[整個叢集一起重開](#整個叢集一起重開)的說明結束，或用本機副本服務。
3. 有 `migrations_dir` 的話，套用還沒套用的 migration（[第 18 章](18-operations.md)）。
4. 透過 leader 寫一筆自己的記號，等本機副本讀回這筆記號，才算追上進度。叢集當下不能接受這筆寫入時（例如正在選舉），會重試到 `migrations_wait_ms`。日誌寫 `cluster node N has caught up with the cluster`。
5. 開 RADIUS、TACACS+ 與指標埠，日誌寫 `Servers started`。

第 4 步保證：一個停機兩個小時才回來的節點，回答的第一個請求就看得到這兩個小時裡新增的帳號。指標埠也是在這之後才開，所以啟動期間連 `/livez` 都連不上。

### snapshot

Raft 把每一筆寫入記在記錄（log）裡。記錄會一直變長，所以每套用 `snapshot_every` 筆（預設 100000），節點就把資料庫存一份 snapshot，之後可以丟掉更早的記錄。新加入、或落後太多的節點，從 leader 拿 snapshot 再接著追記錄。

snapshot 是複寫用的，不是備份。核心沒有備份功能，資料保護的做法見[第 18 章](18-operations.md)。

### 沒有 leader 時

一個節點跟其他節點斷了聯絡、湊不到過半數時，它就沒有可以寫入的 leader。這時的行為取決於 `stop_without_leader_ms`：

| 狀況 | 沒有設 `stop_without_leader_ms`（預設） | 設了，而且沒有 leader 超過這段時間 |
| --- | --- | --- |
| 唯讀資料的登入（例如 `sql` 處理器查密碼） | 照常回答，用本機副本，資料可能是舊的 | 不回應，算進 `rradius_radius_discarded_total{reason="no_leader"}` |
| Status-Server | 照常回答 | 不回應 |
| 腳本要寫入的登入 | 沒有回應（寫入失敗） | 不回應 |
| 計費 | 前幾秒寫入在等 leader，沒有回應；沒有 leader 超過 5 秒後寫入立刻失敗，有計費 spool 就寫進 spool 並回應，沒有就不回應 | 同左 |
| TACACS+ 認證與授權 | 照常回答，用本機副本 | 回 ERROR |
| TACACS+ 計費 | 照常交給腳本 | 不受 `stop_without_leader_ms` 影響 |
| `/readyz` | 200 | 503 `no leader` |

另外兩個跟時間有關的規則：

- 沒有 leader 超過 5 秒後，新的寫入立刻失敗，不再等待。寫入失敗的計費，有計費 spool 時會寫進 spool（[第 13 章](13-accounting.md)）。
- 例外是失去多數後的第一次寫入：它在 5 秒之前就交給叢集，會等到叢集的 30 秒逾時才失敗，不受 `query_timeout_ms` 限制。失敗的訊息是 `the cluster took the write and said nothing of it within 30 s; it may have been committed`（這時寫入可能已經生效，不要當成一定沒寫進去）；叢集只剩 leader 自己時，是 `local client write timed out`，登入沒有回應。實測：三節點停掉兩個，剩下節點的第一次寫入 30 秒後失敗，第二次起 12 毫秒內失敗，讀取一直立即成功。這是以認證寫入量的；計費寫入與寫入重疊的情形沒有量。
- leader 本身如果超過 5 秒沒有收到過半數節點的確認，也算沒有 leader。被隔開的舊 leader 不會以為自己還能寫。

什麼時候該設 `stop_without_leader_ms`？當 NAS 設了多台 RADIUS 伺服器時，設了它，被隔開的節點會停止回應，NAS 就會改問其他還在叢集裡的節點，`/readyz` 也會讓負載平衡器把它移出去。Kubernetes 的範例設定就設了 10000。只有一台伺服器可問的 NAS，不設比較好：至少唯讀的登入還能通過。

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

- 一定要有 id 為 1 的節點：`cluster.nodes: node 1 is required`。節點 1 是第一次啟動時建立叢集的那一個，而且要清單裡的其他每個節點都在運作、回覆自己沒有叢集之後才建立（見[節點 1 在等待時](#節點-1-在等待時)）。
- id 不能重複，也不能是 0。
- `node_id` 要在清單裡：`cluster.node_id 4 is not in cluster.nodes`。
- 位址要是 `host:port`：`cluster.nodes: node 1 raft '10.0.0.1' must be host:port`。`host` 可以是名稱，例如 Kubernetes 的 `rradius-0.rradius-cluster`。

每個節點的設定裡，`nodes` 列出包含自己在內的所有節點。各節點的設定除了 `node_id`、`listen` 這類本機的值，其餘最好完全相同；用 `config diff` 檢查的方法見[檢查各節點的設定](#檢查各節點的設定)。

設了 `nodes` 而且超過一個節點時，`[shutdown]` 的 `ready_delay_ms` 預設變成 5000（單節點是 0），讓 NAS 與負載平衡器有時間改問別的節點。`[shutdown]` 的說明在[第 4 章](04-configuration.md)，關機的順序見[第 18 章](18-operations.md)。

### 哪些東西是每個節點自己的

資料表會自動複寫，但下面這些不會。規劃多個節點時逐項確認：

| 東西 | 每個節點 | 要注意 |
| --- | --- | --- |
| 設定檔、腳本、字典、`migrations_dir` | 各自一份 | 內容要相同。改了之後每台都要更新，並各自 reload 或重啟（用 `config diff` 檢查） |
| `secret_raft_file`、`secret_api_file`、NAS 的共用密碼檔 | 各自一份 | 內容要相同 |
| 憑證、私鑰、撤銷清單 | 各自一份 | 更新時每台都要換，並各自送 SIGHUP |
| 計費 spool | 只重放自己的檔案 | 放在持久的磁碟。節點壞掉時，它還沒重放的就遺失了。`rradius_acct_spool_bytes` 大於 0 代表有還沒寫進資料庫的計費，可以用來告警 |
| 進行中的 EAP 對話 | 只在收到它的節點 | 同一次登入要留在同一個節點（[第 11 章](11-eap.md)） |
| 叢集狀態 | 各自看自己的 | 見[叢集的指標](#叢集的指標) |
| 指標與 `[log.trace]` 的計時 | 各自的 | 叢集的總和由監控系統加總；要追蹤的使用者，每一台都要設 |

### 調整

- **`read_connections`：** 登入大多是讀取，連線不夠時請求要排隊；記憶體很緊時可以調低，每條連線有自己的快取。
- **`snapshot_every`：** 調低的話，異常結束（斷電、SIGKILL）後重新啟動比較快，但寫入多時 snapshot 太頻繁會拖慢服務。
- **多節點不會提高寫入上限。** 每筆寫入都要經過同一個 leader、等過半數的節點確認，分開的主機之間還要多等節點之間的往返（見[跨機房](#跨機房)）。登入（讀取）則是每個節點各自處理，節點越多，總量越高。

三項的預設與範圍見上表，改了都要重新啟動，量測的方法見[第 19 章](19-performance.md)。

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

- `query_timeout_ms = 2000`：一個請求等資料庫的總時間。叢集失去多數節點後，已交給叢集的第一次寫入仍然要等 30 秒才失敗（見上面的規則），這個值不縮短它；它限制的是腳本的讀取與還沒交出的寫入。ha-cluster 情境用的也是這個值。
- `migrations_wait_ms = 120000`：三個節點一起重開時，先起來的節點要等其他節點，給它長一點的時間。
- `[radius.accounting_spool]`：寫不進資料庫時，計費先寫在本機磁碟，之後重送（[第 13 章](13-accounting.md)）。每個節點要用自己的目錄。
- `[accounts]`、`[sessions]`：讓本章可以用 `user add` 與 `session list` 確認複寫（[第 7 章](07-users-and-chains.md)、[第 13 章](13-accounting.md)）。

把 `node1.toml` 複製成 `node2.toml`、`node3.toml`，照上表改 `node_id`、`data_dir`、RADIUS、TACACS+、指標的埠，以及 `[radius.accounting_spool]` 的 `dir`（`spool2`、`spool3`）。三份的 `[[cluster.nodes]]` 完全相同。

```console
$ for n in 1 2 3; do rradiusd check --config node$n.toml; done
configuration ok
configuration ok
configuration ok
```

### 一起啟動

三個節點要一起啟動。第一次啟動時叢集由節點 1 建立，而且要節點 2、3 都在運作、回覆自己沒有叢集之後才建立，節點 2、3 再加入。在那之前沒有任何節點服務，少一個節點，三個都服務不了（見[節點 1 在等待時](#節點-1-在等待時)）。每個節點開一個終端機，或像這樣放到背景，日誌寫進檔案：

```console
$ for n in 1 2 3; do rradiusd serve --config node$n.toml > node$n.log 2>&1 & done
```

在這台機器上，從啟動到三個節點的 `/readyz` 都回 `ok`，大約 9 秒。節點 1 的日誌（拿掉時間）：

```text
WARN hiqlite::store::state_machine::sqlite::writer: No metadata exists inside the DB yet
WARN hiqlite::store::state_machine::sqlite::writer: No metadata exists inside the DB yet
…
INFO rradius_core::cli: cluster node 1 started
INFO rradius_core::cli: cluster node 1 has caught up with the cluster
INFO rradius_core::cli: Servers started. Press Ctrl+C to stop.
INFO rradius_core::radius: RADIUS Server listening on 127.0.0.1:27021 and 127.0.0.1:27031
INFO rradius_core::tacacs: TACACS+ Server listening on 127.0.0.1:27051
INFO rradius_core::metrics: Metrics listening on 127.0.0.1:27041
```

節點 2、3 的日誌在叢集成形之前會有好幾行 `ERROR hiqlite::init: … Raft is not initialized` 或 `Connection refused`：它們在等節點 1 建好叢集。只要接著出現 `has caught up with the cluster`，這些 ERROR 就不必理會。節點 2、3 比節點 1 晚啟動時，節點 1 的日誌在它們回覆之前還會有 `Waiting for the other nodes before creating a new cluster` 警告。

### 節點 1 在等待時

節點 1 啟動時沒有叢集資料（第一次啟動，或 `data_dir` 是空的），會先經由 API 埠詢問 `[[cluster.nodes]]` 裡的其他節點，再做任何事：

- 任一節點回覆叢集的成員時，節點 1 加入那個叢集，不建立新的。
- 同一輪裡其他每個節點都回覆自己沒有叢集時，節點 1 建立叢集。
- 其他情形就 1 秒後再問。沒在運作或連不到、`secret_api` 不同、超過 3 秒才回覆、帶著叢集資料正在啟動或停止，或回覆任何其他錯誤的節點，都可能持有叢集，所以節點 1 等它。

等待期間節點 1 不開 RADIUS、TACACS+ 與指標埠，自己的 Raft 與 API 埠也還沒開，所以其他節點的日誌會出現連不到它的錯誤。每隔幾秒（下面的實測是 6 到 8 秒）記一行警告，列出它還在等的節點與原因：

```text
WARN hiqlite::init: Waiting for the other nodes before creating a new cluster: every one of them has to answer that it has none. Still waiting for node 2 (127.0.0.1:27212): Connection refused (os error 61); node 3 (127.0.0.1:27213): Connection refused (os error 61)
```

| 項目最後的原因 | 意思 | 怎麼做 |
| --- | --- | --- |
| `Connection refused (os error 61)`（數字依作業系統而異） | 那個節點的 API 位址沒有程式在聽 | 啟動該節點；檢查它的 `api` 位址與防火牆 |
| `no answer within 3 s` | 節點沒有及時回覆 | 檢查該節點與兩者之間的網路 |
| `Token: Invalid API Secret` | 兩個節點的 `secret_api` 不同 | 每個節點用同一個 API 密碼 |

會等多久：

- 有 `migrations_dir` 時，節點 1 在 `migrations_wait_ms` 之後以結束碼 1 結束，最後一行是 `cluster was not ready to apply migrations: node 1 did not join within …`（等著建立叢集的節點 1 訊息裡也寫 join）。等著加入的節點 2、3 也同樣結束。行程管理程式把它們重開後繼續等。
- 沒有 `migrations_dir` 時，節點 1 一直等。`stop_without_leader_ms` 不限制這段等待。
- 等待中的節點 1 收到 SIGTERM，大約 25 秒後以結束碼 0 結束，日誌寫 `stopped during startup`。沒有建立任何東西。

叢集建立之前就有節點永久壞掉時：修好它，或從**每一個**節點的 `[[cluster.nodes]]` 拿掉它再啟動；節點 1 就用剩下的節點建立叢集，第三個節點之後再加（[加入節點](#加入節點)）。清單只列節點 1 時沒有對象可問，節點 1 馬上建立叢集。

> **警告：** 只要還有節點可能持有叢集資料（例如節點 1 的磁碟壞掉之後），就不要為了跳過等待而縮短清單。沒有列出持有資料節點的節點 1，會建立第二個空的叢集。

2026-11-28 在一台機器上跑三個節點實測：節點 3 沒開時，節點 1、2 都沒有服務，`migrations_wait_ms = 20000` 時兩者在 20 秒後以結束碼 1 結束；沒有 `migrations_dir` 的節點 1 在 35 秒後仍在等，收到 SIGTERM 25.0 秒後結束；從節點 1、2 的清單拿掉節點 3 後，兩者組成兩個 voter 的叢集。表中三種原因分別由停掉的節點、接受連線但不回覆的埠、不同的 `secret_api` 產生。上面清單裡的其他情形（例如帶著叢集資料、正在啟動或停止的節點）沒有做出來，是照程式寫的。沒有在多台實體主機上測過。

## 確認叢集成形

### cluster members

`cluster members` 問設定裡的節點誰是 leader，再列出 leader 看到的成員：

```console
$ rradiusd cluster members --config node1.toml
node 1  voter    leader  raft 127.0.0.1:27001  api 127.0.0.1:27011
node 2  voter            raft 127.0.0.1:27002  api 127.0.0.1:27012
node 3  voter            raft 127.0.0.1:27003  api 127.0.0.1:27013
```

- `voter`：參與投票、算在過半數裡的節點。剛加入、還在追進度的節點會暫時是 `learner`。
- `leaving`：進行中的成員變更之前投票、之後不投票的節點。有進行中的變更時，最後一行是 `a change of the members is in progress`。正常的變更也可能短暫看到這兩樣；一直都在的話，見[移除節點](#移除節點)。
- `leader`：目前的 leader。哪個節點當 leader 不固定。
- 用哪個節點的設定檔都可以，指令會自己找到 leader。
- 結束碼：0 成功；2 是參數或設定錯誤，或 API 密碼不對；3 是 30 秒內沒有任何節點回報 leader。

### status 的 Node 區段

`status` 讀一個節點的指標埠。`Node` 區段是這個節點眼中的叢集：

```console
$ rradiusd status --config node2.toml
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
$ printf 'erin-password\n' | rradiusd user add --config node1.toml erin
added erin
$ rradiusd user show --config node3.toml erin
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
$ rradiusd session list --config node3.toml
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
- 這段時間裡，唯讀資料的登入不受影響：follower 用本機副本回答。
- 節點 2 的 `rradius_cluster_leader` 一直是 1：指標每秒更新一次，follower 在選舉期間仍認得某個 leader。所以不要只靠這個指標判斷寫入有沒有中斷，要一起看計費的回應與計費 spool。

新 leader 選出後：

```console
$ rradiusd cluster members --config node2.toml
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
- 前幾秒的計費沒有回應：腳本的寫入在等 leader。失去多數後的第一次寫入會等到叢集的 30 秒逾時（不是 `query_timeout_ms`），NAS 會重送。
- 沒有 leader 超過 5 秒後，寫入立刻失敗，計費寫進 spool 並回應 NAS。
- 沒有 leader 超過 10 秒（`stop_without_leader_ms`）後，登入與 Status-Server 都不回應，算進 `rradius_radius_discarded_total{reason="no_leader"}`；TACACS+ 認證回 ERROR（`rradius-bench` 印出 `status: 0x07`）。`/livez` 一直是 200：行程還活著，只是不就緒。

`status` 的樣子：

```console
$ rradiusd status --config node1.toml
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

> **注意：** 啟動中的節點先等 leader 回答它一次讀取，之後才讀「叢集已套用哪些 migration」。叢集剛恢復過半數、還在選舉的那幾秒裡啟動，它就停在這一步等；到 `migrations_wait_ms` 還沒有 leader 回應時，照[整個叢集一起重開](#整個叢集一起重開)的表處理。leader 回應之後讀清單時又遇到換 leader 的話，每 200 ms 重讀一次，直到從啟動起算的 `migrations_wait_ms` 用完；用完才以結束碼 1 結束，最後一行是 `cluster was not ready to apply migrations: cannot read applied migrations: …`，帶最後一次的錯誤。這段重試由單元測試涵蓋，選舉空窗本身沒有在多台實機上重現過。之後寫「追上進度」標記的那一步也會重試。

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
$ rradiusd serve --config node3.toml > node3.log 2>&1 &
```

```text
WARN hiqlite::init: Found remote metrics and we (3) are a Raft member
…
INFO rradius_core::cli: cluster node 3 started
INFO rradius_core::cli: cluster node 3 has caught up with the cluster
```

這台機器上從啟動到就緒大約 2 秒，之後 `erin` 在節點 3 登入是 accept。這個做法要求其他節點仍有過半數在運作。

節點 1 也用同樣方式重建。其他節點都沒有回覆叢集時，節點 1 不會把空的 `data_dir` 當成第一次啟動：它等它們（[節點 1 在等待時](#節點-1-在等待時)），它們回來後加入它們的叢集。2026-11-28 實測：三台全停、刪掉節點 1 的 `data_dir`，單獨啟動的節點 1 沒有回應任何請求；啟動節點 2、3 之後它加入它們，停機前寫入的計費紀錄在它上面查得到。

> **警告：** 不要同時清空多數節點的 `data_dir`。資料只存在叢集的節點上，過半數的副本都沒了，資料就沒了。

### 被節點 1 分裂的叢集

較早的版本沒有[節點 1 在等待時](#節點-1-在等待時)說的等待：`data_dir` 是空的節點 1 在連不到其他節點時，會自己建立一個叢集，並用它回應登入與計費。其他節點回來後，看過以下兩種情形：

- 三個節點都在運作、`cluster members` 看起來正常，但節點 1 單獨期間寫入的紀錄只在節點 1 上，重開也不會讓副本一致。
- 節點 1 之後每次啟動都 panic（`panicked at …/openraft-0.9.25/src/raft_state/log_state_reader.rs:23`），日誌卻寫 `exiting so that a restart recovers it`。節點 2、3 也可能以 `did not catch up with the cluster within …` 結束。

復原步驟：

1. 三個節點都裝目前的版本。裝新版不會修好已經發生的分裂。
2. 啟動節點 2、3，確認它們有 leader：`rradiusd cluster members --config node2.toml`。
3. 停掉節點 1。它單獨期間收下的紀錄其他節點沒有，NAS 已經收到那些紀錄的 `Accounting-Response`，不會重送。可能需要它們的話，下一步之前先把它的 `data_dir` 複製起來（資料庫是裡面的 SQLite 檔 `state_machine/db/hiqlite.db`）。
4. 清空節點 1 的 `data_dir` 再啟動。

2026-11-28 在一台機器上跑三個節點實測：用較早的版本做出這兩種狀態，再用目前的版本復原，之後三份副本一致、節點 1 恢復服務，節點 1 單獨期間收下的紀錄沒有了。從複製出來的目錄讀回那些紀錄沒有試過。

### 換掉壞掉的主機

主機壞了、資料救不回來時，用新主機頂替，做法跟上面一樣，只是設定要對得上：

1. 新主機用**同一個 `node_id`** 與同樣的位址（`[[cluster.nodes]]` 裡它的 `raft`、`api`）；設定檔、密碼檔與 `migrations_dir` 跟其他節點相同。增減過節點的話，`[[cluster.nodes]]` 要列出目前的所有成員（先用 `cluster members` 看）。
2. `data_dir` 是空的。舊主機的資料還在但不能信任時，整個刪掉再啟動。
3. 啟動。它從其他節點取得整份資料，日誌出現 `cluster node N has caught up with the cluster` 之後才開始回應。
4. 確認：`rradius_cluster_applied_index` 跟其他節點接近、`rradius_cluster_leader` 是 1；用一個它停機期間新增的帳號在它上面登入一次。

- **其他節點要有過半數在線上。** 新節點要從它們取得資料。
- **壞掉的是 leader 也一樣。** 剩下的節點自己選出新 leader，選舉那段時間要寫入的登入沒有回應，計費寫進 spool，之後補送（秒數見[跨機房](#跨機房)）。
- **leader 做過 snapshot、丟掉舊記錄也沒關係，** 新節點直接收到 snapshot。
- **管理指令會等久一點。** leader 剛停止的那段時間，`user` 這類指令會先連到舊 leader，等連線逾時才改連新的，可能要十幾秒才完成，上限是 30 秒。
- **同一個 id 不會被 `node id N is in use` 擋下：** 舊位址沒有回應時，檢查照常通過（見[id 被用掉時](#id-被用掉時)）。
- **測過的範圍：** 2026-11-13 用 `compose.cluster.yaml` 的三節點，在一台主機的 Docker 上，強制停止並刪掉容器與資料 volume 來模擬主機壞掉。換掉 follower 約 2 秒追上；設 `snapshot_every = 50`、寫入約 200 筆讓 leader 做過 snapshot 後換掉 leader，約 5 秒追上，三台的 applied index 相同，期間新增的帳號在每台都能登入。沒有在多台實體主機或經過 WireGuard 的環境測過。

### 整個叢集一起重開

三個節點一起停、一起啟動時，每個節點先等 leader 出現，追上進度才服務。等 leader 最多等 `migrations_wait_ms`，所以它要比你預期「最後一個節點起來」所需的時間長。到時間仍沒有 leader 的節點，照下表「單獨重開一個節點」的方式處理。

寫「追上進度」標記時遇到選舉，會重試到 `migrations_wait_ms`。2026-11-27 在一台機器上跑三個節點、`migrations_wait_ms = 15000` 實測：三個節點一起重開 10 次，每次重開前先讓一個舊 follower 在沒有過半數的情況下單獨跑 20 秒；沒有節點結束，三個節點在啟動後約 6.6 秒追上進度。這筆寫入還不會重試時，同樣的重開 10 次中有 4 次有節點結束（`cannot write the marker: … has to forward request to: None, None`）。

只重開一個節點、另外兩個還沒回來時：已經加入過叢集的節點不會無限期等 leader，結果與它停下時的角色無關。在 `migrations_wait_ms`（預設 60 秒）內沒有 leader 回應時：

| 設定與本機副本 | 節點的行為 |
| --- | --- |
| 沒設 `stop_without_leader_ms`，沒有 `migrations_dir` | 印出警告，用本機副本服務 |
| 沒設 `stop_without_leader_ms`，`migrations_dir` 的檔案本機都已套用 | 印出同樣的警告，用本機副本服務 |
| 沒設 `stop_without_leader_ms`，有檔案本機還沒套用 | 結束碼 1，`no leader to apply migration file '…', which this node has not applied` |
| 沒設 `stop_without_leader_ms`，有檔案以另一個名稱或另一份內容記為已套用 | 結束碼 2 |
| 設了 `stop_without_leader_ms` | 結束碼 1，`cluster was not ready: no usable leader within …, and stop_without_leader_ms is set`，不回應任何請求 |

警告是 `cluster node 1: no usable leader within …; serving without a leader before catching up, so answers come from this node's replica, which may be behind`，`rradius_cluster_caught_up` 是 0。其他節點回來後，日誌寫 `cluster node 1 has caught up with the cluster`，這個指標變成 1。

2026-11-27 在一台機器上跑三個節點、`migrations_wait_ms = 10000`，單獨重開一個 follower 實測，停下時 leader 還在跑、以及在 leader 之後停下各一種：有、沒有 `migrations_dir` 都用本機副本服務；有 `migrations_dir` 並設 `stop_without_leader_ms = 10000` 時以結束碼 1 結束。在其他節點還在跑時停下的原 leader（測試中是節點 1），單獨重開後，有、沒有 `migrations_dir` 同樣用本機副本服務。停掉一個沒有 leader 而在服務的節點（SIGTERM，`[shutdown]` 用預設時間）會以結束碼 0 結束：原 follower 約 20 秒，原 leader 約 15 秒。

已經被移除、而且問得到目前 leader 的節點不照這張表：它不開始服務，關閉本機的叢集節點後以結束碼 1 結束，見[被移除的節點又啟動時](#被移除的節點又啟動時)。

單獨重開的節點用可能是舊的副本回答只讀資料的登入，寫入要等過半數回來（見[沒有 leader 時](#沒有-leader-時)）。

## 加入節點

目標：讓第 4 個節點加入運作中的叢集，其他節點都不必重開。

新節點會自己加入，沒有「加入」的指令。條件：

- 新節點的 `data_dir` 是空的（或不存在）。
- 它的設定的 `[[cluster.nodes]]` 列出它自己，以及目前的 leader。最簡單的做法是列出全部的節點。
- `node_id` 沒有被別的運作中節點用掉。

依照[節點 1 的設定](#節點-1-的設定)的表做 `node4.toml`，`[[cluster.nodes]]` 列出節點 1 到 4，然後啟動：

```console
$ rradiusd serve --config node4.toml > node4.log 2>&1 &
```

這台機器上大約 4 秒後就緒，日誌依序是 `cluster node 4 started`、`has caught up with the cluster`、`Servers started`。加入前建立的帳號馬上能在節點 4 登入：

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 27024 --secret "$(cat nas.secret)" \
    --user erin --password erin-password --load pap,concurrency=1 --requests 1 | grep 'pap:'
  pap: 22/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
$ rradiusd cluster members --config node1.toml
node 1  voter            raft 127.0.0.1:27001  api 127.0.0.1:27011
node 2  voter            raft 127.0.0.1:27002  api 127.0.0.1:27012
node 3  voter    leader  raft 127.0.0.1:27003  api 127.0.0.1:27013
node 4  voter            raft 127.0.0.1:27004  api 127.0.0.1:27014  (not in this configuration)
```

`(not in this configuration)` 表示 `node1.toml` 的清單裡沒有節點 4。這不影響運作：節點 1 到 3 不必為了節點 4 重開，重開時也照常回來。用 `node4.toml` 看就沒有這個標記。之後找時間把節點 4 加進其他節點的設定，讓各節點的清單一致，`cluster members` 才不會一直提醒你。

> **注意：** 節點的總數通常保持奇數。從 3 個加到 4 個，過半數從 2 變 3，可以同時停掉的節點還是只有 1 個。

新節點加入時的幾個細節：

- **空資料加入的節點，要列出現在的 leader。** 它只向自己設定裡列的節點申請加入，其他節點只會叫它去找 leader；leader 不在它的清單裡就加不進去。設定有 `migrations_dir` 時，它等到 `migrations_wait_ms` 後以結束碼 1 結束，訊息是 `node N did not join within …`，被行程管理程式重啟後又重來。所以列出全部現有成員最保險。2026-11-13 實測：leader 換成節點 4 之後，設定只列 1 到 3 的空資料節點 1 加不進去；補上節點 4 後 8 秒就加入。
- **仍是成員、帶著資料重新啟動的節點不受影響。** 它從自己的資料知道成員，設定沒列 leader 也能回到叢集（同一次實測約 14 秒）。
- **密碼不同的節點加不進去，** 新節點的日誌寫出原因：`secret_raft` 不同是 `Invalid ChallengeResponse`，`secret_api` 不同是 `Invalid API Secret`；叢集的成員不變（2026-11-18 實測）。
- **新節點的 `migrations_dir` 比別台多一個檔案時，** 2026-11-18 只看了加入與其他節點重啟，兩者都照常，日誌沒有提到差異；各節點的 `migrations_dir` 要保持相同，這個組合沒有深入驗證。

### id 被用掉時

如果新節點的 `node_id` 是某個還在運作的節點的 id，但位址不同，它會拒絕啟動，結束碼 2：

```text
cannot start cluster node 2: node id 2 is in use: the cluster has it at api 127.0.0.1:27012 (raft 127.0.0.1:27002), and that node answers, but this node's configuration gives it api 127.0.0.1:27015 (raft 127.0.0.1:27005); use an id no node has (see `rradiusd cluster members`), or stop the running node first
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
$ rradiusd cluster remove --config node1.toml 4
node 4 still answers at 127.0.0.1:27014: stop it first, then remove it
```

結束碼 2。還在運作的節點被移除後，在它自己發現之前仍用手上的舊資料回答，所以一定要先停（見[為什麼一定要先停機](#為什麼一定要先停機)）。

停掉節點 4（SIGTERM，這台機器上 15 秒後結束），再移除：

```console
$ rradiusd cluster remove --config node1.toml 4
removed node 4
$ rradiusd cluster members --config node1.toml
node 1  voter            raft 127.0.0.1:27001  api 127.0.0.1:27011
node 2  voter            raft 127.0.0.1:27002  api 127.0.0.1:27012
node 3  voter    leader  raft 127.0.0.1:27003  api 127.0.0.1:27013
```

接著把節點 3 也移出去，同樣先停掉它。這次移除會讓叢集只剩兩個 voter，所以：

```console
$ rradiusd cluster remove --config node1.toml 3
removing node 3 leaves two voters, and the cluster stops writing when either stops; give --force to remove it anyway
$ rradiusd cluster remove --force --config node1.toml 3
removed node 3
$ rradiusd cluster members --config node1.toml
node 1  voter    leader  raft 127.0.0.1:27001  api 127.0.0.1:27011
node 2  voter            raft 127.0.0.1:27002  api 127.0.0.1:27012
node 3  not in the cluster (listed in this configuration)
```

`not in the cluster (listed in this configuration)` 表示設定裡有、叢集裡沒有。

被移除的節點要再加回來，就照[加入節點](#加入節點)的條件：清空它的 `data_dir` 再啟動。上面的節點 3 清空 `data3` 後啟動，2 秒內就以 voter 身分回到叢集。帶著原本的資料啟動它不會回到叢集，節點會自己結束（見[被移除的節點又啟動時](#被移除的節點又啟動時)）。

**移除後只剩一個 voter 時**（原本兩個 voter、停掉其中一個），`remove --force` 第一次不會完成，看來是因為這個變更需要目前 voter 的過半數同意，而被移除的節點正是其中之一。指令會說接下來怎麼做。2026-11-27 在一台機器上用兩個節點實跑，停掉的節點 2 是 follower：

```console
$ rradiusd cluster remove --force --config node1.toml 2
the removal of node 2 may have been made, but the cluster did not answer within 30 s; check with cluster members
the change needs node 2 to commit: start it again with its data, wait about 20 s, stop it, and run this command again
$ rradiusd cluster members --config node1.toml
node 1  voter    leader  raft 127.0.0.1:27001  api 127.0.0.1:27011
node 2  leaving          raft 127.0.0.1:27002  api 127.0.0.1:27012
a change of the members is in progress
$ rradiusd cluster remove --force --config node1.toml 2
127.0.0.1:27011: HTTP 500 Internal Server Error: {"ClientWriteError":{"APIError":{"ChangeMembershipError":{"InProgress":{"committed":{"leader_id":{"term":1,"node_id":1},"index":4},"membership_log_id":{"leader_id":{"term":1,"node_id":1},"index":9}}}}}}
the change needs node 2 to commit: start it again with its data, wait about 20 s, stop it, and run this command again
```

第一個指令 30 秒後以結束碼 3 結束，第二個馬上以結束碼 1 結束；兩者的第二行都寫著怎麼完成：

1. 帶著原本的資料把停掉的節點再啟動，等約 20 秒，停掉它。
2. 再執行一次 `cluster remove --force <id>`，印出 `removed node <id>`：

```console
$ rradiusd cluster remove --force --config node1.toml 2
removed node 2
$ rradiusd cluster members --config node1.toml
node 1  voter    leader  raft 127.0.0.1:27001  api 127.0.0.1:27011
node 2  not in the cluster (listed in this configuration)
```

步驟 1 啟動的節點這時還在成員清單裡（顯示 `leaving`），會照常啟動、開始服務，不會自己結束；整合測試裡它啟動後約 4 秒叢集恢復接受寫入（量一次）。移除完成後再啟動該節點，它以結束碼 1 結束（見[被移除的節點又啟動時](#被移除的節點又啟動時)）。

**停掉的是 leader 時**，剩下的節點沒辦法自己成為 leader，`cluster members` 和 `cluster remove` 只能等，30 秒後印出 `the cluster did not answer within 30 s`（結束碼 3）；移除請求沒有送出。把停掉的節點啟動回來，`cluster members` 顯示另一個節點是 `leader` 時才停掉它；它又當 leader 的話，停掉再啟動一次（2026-11-27 的一次實跑，第二次啟動後另一個節點當 leader）。然後照上面的方式移除。

改用空的 `data_dir` 啟動該節點，2026-11-27 的一次測試裡變更沒有完成（觀察約 20 秒）；所以資料沒了的節點，不要指望能移除。只有兩個 voter 的叢集要三思。

`cluster remove` 的結束碼：0 成功；1 是沒有這個成員，或叢集拒絕移除；2 是參數、設定錯誤，或檢查沒通過；3 是 30 秒內沒有回應。結束碼 3 時，移除可能已經做了，訊息會叫你用 `cluster members` 確認，不要直接重跑。

### 被移除的節點又啟動時

被移除的節點收不到之後的寫入，它手上的資料停在被移除的時候。節點啟動時會向其他節點查目前 leader 的成員清單，清單裡沒有自己就以結束碼 1 結束：不開始服務，也不寫入叢集。日誌：

```text
… ERROR rradius_core::cli: cluster node 3 has been removed from the cluster: node 1, its leader, does not list it (its members: node 1 at 127.0.0.1:27011, node 2 at 127.0.0.1:27012). To retire this host, stop and disable the service. To add it back, first confirm that this is the cluster you expect, then empty data_dir '…/data3' and start it again
```

處理方式有兩種：

- **主機要淘汰：** 把服務停掉並停用（例如 `systemctl disable --now rradius`），並從 NAS 的 RADIUS 伺服器清單拿掉。不停用的話，systemd 的 `Restart=on-failure` 或容器的重啟政策會一直重啟它，每次都結束。
- **要加回叢集：** 先用一份指向你預期的那個叢集的設定執行 `cluster members`，確認它就是日誌列出的成員，再清空這個節點的 `data_dir` 啟動。

判定的範圍：

- 只有目前的 leader（回報自己是 leader，而且 5 秒內得到過半數確認）的清單裡沒有這個節點，而且那份清單比本機資料記得的新（或本機的清單本身已經沒有自己），才算被移除。任何一個這樣的 leader 列出它，就不算。仍在清單裡的節點照常啟動，包括 `learner`，以及[兩個 voter 移除一個](#移除節點)做到一半時停掉的那個節點（`leaving`）。
- 查詢走各節點的 API 埠，對象是 `[[cluster.nodes]]` 與本機資料記得的其他節點。
- 執行中的節點，在超過 5 秒沒有 leader、或本機的成員清單沒有把自己列為 voter（沒有自己，或自己是 `learner`）時，每 5 秒查一次，查到被移除就停止回應並以結束碼 1 結束。

> **警告：** 連不到叢集的被移除節點沒有辦法知道自己被移除了。啟動時沒有任何目前的成員回應（網路隔離、其他節點全停），它照[整個叢集一起重開](#整個叢集一起重開)的表處理：沒設 `stop_without_leader_ms` 時用本機的舊資料回答登入，之後查得到 leader 才結束。要它在這種情況下不回應，設 `stop_without_leader_ms`。

2026-11-27 在一台機器上跑三個節點（debug build）實測：移除一個停掉的 follower，再帶原本的資料啟動它三次，有、沒有 `migrations_dir` 各做一輪，每次都以結束碼 1 結束（啟動後約 16 到 21 秒，大部分時間花在關閉節點），沒有開始監聽 RADIUS 埠。清空 `data_dir` 再啟動，約 2 秒以 voter 身分回到叢集。設 `stop_without_leader_ms = 10000` 的情況，以及「其他節點全停時用本機資料回答」，由整合測試涵蓋。沒有在多台實體主機上測過。

### 為什麼一定要先停機

- **被移除但還在執行的節點，會有一段時間用舊資料回答。** 它不再收到新的寫入，移除後新增的帳號在它上面登入被拒絕，移除後停用的帳號仍能登入（2026-11-13 在 Docker 上實測）。NAS 還把它列為 RADIUS 伺服器時，就會得到錯的答案。2026-11-27 繞過 `cluster remove` 的檢查、直接對 leader 的 API 移除一個執行中的 follower（單機三節點）：一次量到移除後約 12 秒內它照舊回答，14 秒起不再回應；三次都在移除後約 28 到 30 秒以結束碼 1 結束。連不到叢集時它不會結束。
- **清空資料、設定還列著自己時重新啟動，會自動加回叢集。** 所以淘汰的主機要把服務停用，不只是刪資料。
- **管理指令可能變慢。** 設定裡排第一的節點「已移除、正在結束」時，`user` 這類指令要等它：2026-11-27 一次在移除後 8 秒執行 `user add`，19 秒後成功；節點結束後恢復正常。先停機就不會發生。

### 其他注意事項

- **維持奇數個節點（3 或 5）。** 3 個可以壞 1 個，5 個可以壞 2 個；4 個跟 3 個一樣只能壞 1 個。
- **要移除的是 leader 也一樣，** 先停掉它：剩下的節點選出新 leader（選舉那段時間的寫入會逾時，見[跨機房](#跨機房)），再移除。
- **停機的主機排在設定第一位時，** `cluster members` 與 `cluster remove` 要先等它逾時，才換下一台。程式裡每台最多等 2 秒（`PING_WAIT`）；2026-11-18 的實測曾量到多等約 5 秒，是改這個值之前的版本。把停機的節點從設定刪掉，或移到後面，就不會。
- **失去多數時，`members` 看到的 leader 可能是過期的。** 三台只剩原來的 leader 時，它仍回報自己是 leader（結束碼 0），但它沒有多數，寫入不會成功；剩下的是 follower 時，`members` 約 30 秒後以結束碼 3 結束。要判斷能不能寫，看 `rradiusd status` 的 `Has a leader`，或直接試一次寫入（2026-11-18 實測）。
- **只停一台不必移除。** 升級、重開機、換硬體後用同一個 `node_id` 回來，見[重開節點與重建資料](#重開節點與重建資料)；移除是給不會再回來的節點。
- **測過的範圍：** 2026-11-13、2026-11-18，一台主機的 Docker，三到五個節點：移除 leader 約 5 秒完成，期間對剩下三台各每 0.05 秒登入一次，約 1,480 次都成功，最長空檔 0.1 秒（登入讀本機副本，不需要 leader）；加第 4 台 1 到 3 秒成為 voter、約 4 秒開始回應登入；寫入中加入與移除，394 次寫入沒有失敗。沒有在多台實體主機或經過 WireGuard 的環境測過，也沒有測真實 NAS 在選舉期間的行為。

## 檢查各節點的設定

各節點的設定應該只差在本機的值。`config diff` 比較兩份設定（含預設值），列出不同之處；`cluster.node_id` 與 `cluster.listen` 這兩個本來就該不同的鍵，列在最後的 `node-specific:` 下面：

```console
$ rradiusd config diff node1.toml node2.toml
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
$ rradiusd config diff node1.toml node2.toml
metrics.bind: "10.99.0.1" | "10.99.0.2"
node-specific:
  cluster.listen: "10.99.0.1" | "10.99.0.2"
  cluster.node_id: 1 | 2
```

有差異時結束碼是 1，完全相同（只差 `node-specific`）時是 0。改了設定之後、推到各節點之前，用它確認沒有只改到一台。祕密只顯示 `differs (hidden)`，不會印出內容。

## 看整個叢集的狀態

`status --cluster` 向叢集要成員清單，同時讀每個成員的指標埠，一行一個節點：

```console
$ rradiusd status --cluster --config node1.toml
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
| `lua.query_timeout_ms` | `2000` | 請求等資料庫的總時間；失去多數後的第一次寫入仍要等 30 秒，之後立即失敗並寫進 spool |
| `radius.bind` | `0.0.0.0` | NAS 從實體網路連進來 |
| `radius.clients` | `10.80.0.0/16`，共用密碼三台相同 | 每台 NAS 把三個節點都設成 RADIUS 伺服器 |
| `radius.status_server` | `true` | NAS 與負載平衡器用 Status-Server 判斷節點活著 |
| `[radius.accounting_spool]` | `spool`，最多 1 GiB | 失去多數時計費先寫在本機 |
| `metrics.bind` | 各自的 WireGuard 位址，埠 9812 | 監控從內部網路抓；三台同埠，`status --cluster` 才讀得到 |
| `log.format` | `json` | 方便集中收集 |

照[第 3 章](03-quick-start.md)的做法把情境複製到工作目錄、把 `dictionary` 改成字典的絕對路徑後，三份都能通過 `check`（會警告範例的公開密碼與檔案權限）：

```console
$ for n in 1 2 3; do rradiusd check --config node$n.toml; done 2>&1 | grep -v WARN
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
- RADIUS 走節點的實體位址，不是 WireGuard 位址；請求也要從 `radius.clients` 的網段送出，否則不會有回應。管理用的主機不在那個網段時，暫時加一個只有它位址的 `[[radius.clients]]`，reload 後測，測完再移除。
- 用 EAP 的話，一次登入要在同一個節點完成：NAS 設成主備，或讓負載平衡器依來源固定節點（[第 11 章](11-eap.md)）。
- 節點不是同時啟動時：第一次啟動，要三個節點都在運作才有節點服務，因為節點 1 要另外兩個都回覆之後才建立叢集（[節點 1 在等待時](#節點-1-在等待時)），節點 2、3 再加入；有 `migrations_dir`（如這個範例）時，`migrations_wait_ms` 內沒加入的節點以結束碼 1 結束（`did not join within …`），沒有時會一直等到加入為止；重開時，節點最多等 `migrations_wait_ms` 讓 leader 出現，之後照[整個叢集一起重開](#整個叢集一起重開)的說明處理。這段等待不是故障；日誌出現 `RADIUS Server listening` 才能服務。
- 每個節點都要設自動重啟（systemd `Restart=on-failure`、Docker `restart: unless-stopped`），因為伺服器在某些情況下會主動以結束碼 1 結束，讓行程管理程式重開它（見[強制結束之後](#強制結束之後)）。

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

啟動的步驟（`compose.cluster.yaml` 開頭的註解）。同一個目錄裡[第 2 章](02-install.md)的單機若還在跑，先 `docker compose down`：兩者的專案名稱與主機埠相同。`up` 在前景執行，後面的指令要另開終端機，或加上 `-d`。

```console
$ sh docker/init-secrets.sh
$ docker compose -f compose.cluster.yaml up --build
```

`init-secrets.sh` 產生三個隨機的密碼檔，已經存在的不覆蓋。在容器裡看狀態：

```console
$ docker compose -f compose.cluster.yaml exec node1 rradiusd status --config /etc/rradius/cluster/node1.toml
$ docker compose -f compose.cluster.yaml exec node1 rradiusd cluster members --config /etc/rradius/cluster/node1.toml
```

停 leader 的練習：先用 `cluster members` 找出 leader，再 `docker compose -f compose.cluster.yaml stop <該節點>`（送 SIGTERM，最多等 30 秒），之後換另一個節點的服務名稱與設定檔查看。

本書驗證過的部分，2026-11-27 在一台 Docker 主機上（OrbStack、arm64，埠改成只聽 `127.0.0.1`）：image 可以建置，三個容器啟動，兩個 `exec` 指令可用，停 leader 約 15 秒；另一個節點接手，送往節點 2 的計費約有 6 秒沒有回應。

> **未實機驗證：** 節點分散在多台主機上的叢集。這次沒有建置 x86-64 的 image。

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

### 建置並載入 image

`statefulset.yaml` 用 `rradius-server:latest`，`imagePullPolicy: IfNotPresent`。在原始碼根目錄建置：

```console
$ docker build -t rradius-server:latest .
```

- **Docker Desktop、OrbStack 內建的 Kubernetes：** 直接用本機的 image，不必再做什麼。
- **kind：** `kind load docker-image rradius-server:latest`，把 image 載入 kind 的節點。
- **正式環境：** 推送到自己的 registry，再打開 `kustomization.yaml` 裡註解掉的 `images`，改成那個名稱與版本：

  ```yaml
  images:
    - name: rradius-server
      newName: registry.example.com/rradius-server
      newTag: "1.0"
  ```

### 先建 Secret

```console
$ kubectl create secret generic rradius-secrets \
    --from-literal=raft="$(openssl rand -hex 32)" \
    --from-literal=api="$(openssl rand -hex 32)" \
    --from-literal=radius="<你的 NAS 共用密碼>"
```

Secret 掛在 `/run/secrets/rradius`，設定用 `secret_raft_file = "/run/secrets/rradius/raft"` 這類路徑讀它。三個 pod 用同一個 Secret，叢集密碼自然相同。`radius` 換成你的 NAS 實際設定的共用密碼。範例目錄不放真的密碼，`secret.example.yaml` 只是格式說明。

### 套用與等待

先把 `config/rradius-0.toml` 到 `rradius-2.toml` 的 `[[radius.clients]] address` 從文件用的 `192.0.2.0/24` 改成 NAS 實際的網段（三份要一樣；理由見[改 NAS 的位址](#改-nas-的位址)），然後：

```console
$ kubectl apply -k deploy/kubernetes
$ kubectl rollout status statefulset/rradius
```

三個 pod 同時啟動，選出 leader 後才開始服務；`rollout status` 等到三個都就緒才結束。接著用[在 pod 裡下指令](#在-pod-裡下指令)的做法加帳號。

### 這份範例為什麼這樣設計

每一點都對應到本章前面的行為：

- **`podManagementPolicy: Parallel`：** 三個 pod 一起啟動。設了 `stop_without_leader_ms` 時，重開的節點在 `migrations_wait_ms` 內找不到 leader 就以結束碼 1 結束、再被啟動；整組停掉的叢集如果一個一個啟動，第一個 pod 永遠不會 ready，另外兩個也就不會被啟動。第一次啟動也要它們一起啟動：節點 1 要另外兩個 pod 回覆之後才建立叢集。
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

### 驗證來源位址沒有被改寫

封包經過 Kubernetes 的 Service 時，來源位址常被改成節點或閘道的位址，結果有兩種：

- 改寫後的位址不在任何 client 的網段裡：rradius 不認得它，**不回應**，計入 `rradius_radius_discarded_total{reason="unknown_client"}`，日誌（有速率限制）寫 `RADIUS packet from unknown client <位址> discarded`。
- 改寫後的位址剛好落在某個 client 的網段裡，例如 client 寫了 `10.0.0.0/8` 這種寬網段：請求照樣被接受，**看起來一切正常**，但所有 NAS 都被當成同一個 client。這種情況計數不會增加，只能從認證紀錄看出來。

所以套用後一定要驗證：從一台 NAS 送一筆請求，看認證紀錄（`log.auth` 預設開）的 `source=` 是不是那台 NAS 的位址。請求可能送到任何一個 pod，三個都要看：

```console
$ kubectl logs rradius-0 | grep rradius_auth | tail -3
… INFO rradius_auth: authentication protocol="radius" client="192.0.2.0/24" source=192.0.2.10 user="alice" …
```

`source=` 是節點、閘道或 pod 網段的位址，就是被改寫了。沒有任何紀錄時，看 `unknown_client` 有沒有增加：

```console
$ kubectl port-forward pod/rradius-0 9812:9812 &
$ curl -s localhost:9812/metrics | grep 'reason="unknown_client"'
```

- 雲端的負載平衡器大多支援 `externalTrafficPolicy: Local`，但有些 UDP 負載平衡器本身會改寫來源位址，要看雲端的文件。
- OrbStack 內建的 Kubernetes 從 Mac 送到 LoadBalancer 的封包，`source=` 是 pod 網段的閘道，所以在 OrbStack 上驗證不了來源位址會不會保留。只是在本機試用時，可以暫時把 client 改成那個位址。

> **未實機驗證：** 上面兩種負載平衡器的行為依來源 `deploy/kubernetes/` 的註解與 Kubernetes 的一般行為寫成，本書沒有在實際的叢集上量過。

### 沒有合適的負載平衡器時：hostNetwork

`hostnetwork-patch.yaml` 讓 pod 直接用節點的網路，NAS 的封包到節點的位址，來源位址不會被改寫。在 `kustomization.yaml` 打開 `patches`：

```yaml
patches:
  - path: hostnetwork-patch.yaml
```

- patch 設了 `podAntiAffinity`，一個節點只能放一個 rradius pod，所以至少要三個 Kubernetes 節點。
- NAS 設定的是三個節點的位址，主備或輪流都可以。
- pod 搬到別的節點時位址會變，NAS 那邊要跟著改，所以適合固定的節點，例如用 `nodeSelector` 指定。
- **指標埠與叢集埠會開在節點的每個位址上，包括對外的位址。** 範例的 `[metrics] bind = "0.0.0.0"`，這時 9812 沒有認證也沒有 TLS 地對外開放；叢集的 8100、8200 也一樣。NetworkPolicy 管不到用 hostNetwork 的 pod，`networkpolicy.yaml` 擋不住。要用節點的防火牆只讓內部網路連 9812、8100、8200，或把 `[metrics] bind` 改成節點的內部位址（三個節點的位址不同，設定檔就不再只差 `node_id`）。

### TACACS+

設定加 `[tacacs]` 時，`statefulset.yaml` 加 TCP 的容器埠，再打開 `services.yaml` 裡註解掉的 `tacacs` 那段。TCP 經過 `externalTrafficPolicy: Local` 一樣保留來源位址，驗證方法同上。

> **注意：** 容器用 uid 10001 執行、拿掉所有 capability，能不能綁 49 這種低埠，取決於容器執行環境對低埠的設定，本書沒有驗證。穩當的做法是 `[tacacs]` 的 `port` 設成 1024 以上（例如 `49490`），Service 的 `port` 仍是 49，`targetPort` 指向那個埠。

### 探針與延遲

指標埠（`[metrics] bind = "0.0.0.0"`，因為 kubelet 從節點連 pod 的位址探測）有兩個路徑，細節見[第 17 章](17-monitoring.md)：

| 路徑 | 200 | 503 | 範例的用途 |
| --- | --- | --- | --- |
| `/livez` | 行程在回應 | 不會 | `startupProbe`、`livenessProbe` |
| `/readyz` | 會回應認證 | 正在停止（收到 SIGTERM）；設了 `stop_without_leader_ms` 且沒有 leader 超過這段時間；或某個設了 `required_for_ready` 的外部資料庫連不上（內容 `database <名稱> unreachable`，[第 9 章](09-external-databases.md)） | `readinessProbe` |

- **未就緒的 pod 會從 `rradius` Service 移出，** NAS 的請求改送其他 pod。範例設 `stop_without_leader_ms = 10000`：三台剩一台時，剩下那台約 10 秒後停止回應、變成未就緒；`/livez` 仍是 200，容器不會被重啟。
- **刪除或更新 pod 時，** kubelet 送 SIGTERM，`/readyz` 立刻變 503，pod 照常回應 5 秒（三節點的預設 `ready_delay_ms`），`readinessProbe` 每 5 秒一次、失敗 1 次就移出，所以 Service 先不再送請求過來，之後 pod 才停止接收、等處理中的請求做完再結束。整個過程約 15 秒，在 `terminationGracePeriodSeconds: 30` 之內。
- **沒設 `stop_without_leader_ms` 時，除了停止中以外 `/readyz` 一直是 200：** 節點沒有 leader 也照常用本機副本回應唯讀的登入，把它移出 Service 反而讓所有登入都失敗。
- **`startupProbe` 最多等 5 分鐘**（每 5 秒、最多 60 次）：指標埠在節點追上叢集後才開，這要先有 leader。沒有 `startupProbe` 的話，`livenessProbe` 會在選出 leader 前把容器重啟。

### 在 pod 裡下指令

```console
$ kubectl exec -i rradius-0 -- rradiusd user add --config /etc/rradius/rradius-0.toml alice
$ kubectl exec rradius-0 -- rradiusd cluster members --config /etc/rradius/rradius-0.toml
$ kubectl exec rradius-0 -- rradiusd status --cluster --config /etc/rradius/rradius-0.toml
```

`user add` 從標準輸入讀密碼，所以要 `-i`。設定改了之後怎麼套用（ConfigMap 換名字、pod 逐一重啟），以及新增 migration 要改哪裡，見[第 18 章](18-operations.md)。

本書驗證過的部分：在工作目錄複製一份，`kubectl kustomize` 能產生 ConfigMap、2 個 Service、PodDisruptionBudget 與 StatefulSet；三份 `rradius-*.toml` 把字典與密碼路徑換成本機路徑後，`check` 都通過。

> **未實機驗證：** 實際套用到 Kubernetes 叢集、pod 啟動、刪掉 leader pod 的過程，以及 `hostnetwork-patch.yaml`、`networkpolicy.yaml` 的效果。上面的 `kubectl exec` 指令沒有實際跑過。

## 跨機房

三個節點分在不同機房、節點之間隔著幾毫秒到幾十毫秒時，叢集會怎樣？本節的數字是在一台主機上模擬出來的，**用來判斷「值不值得往這個方向設計」，不是對真實廣域網路的承諾。** 沒有標日期的量測，沒有為本手冊重新量測；標了日期的是那天量的。原始數字與量測腳本不在這個 repo 裡。

### 量測條件

- **環境：** Apple M4 Max（14 核）的一台主機，OrbStack 上的 Docker，三個節點各一個容器；image 是核心的 `rradius-server` 加上 `iproute2`。節點設定是 `docker/config/auth.lua` 與 `sql` 處理器（登入讀本機副本，每個計費封包一次 `INSERT`）。
- **延遲怎麼加：** 在每個節點對另外兩個節點的 egress 用 `tc netem` 加單向延遲（來回時間 RTT 的一半），RADIUS 用戶端到節點的路徑不加。RTT 用節點之間的 `ping` 校正，下表一律用實測值。
- **模擬的限制：** 只有固定的延遲，沒有抖動、沒有封包遺失、兩個方向相同；節點在同一台主機上，CPU 與記憶體不是瓶頸。
- **負載：** `rradius-bench`，固定並行 64，暖機 2 秒、量測 10 秒，每個 RTT 點 3 次。讀取負載是 `pap` 登入（讀本機副本）；寫入負載是 `acct-start`（每個封包一次 `INSERT INTO accounting`，要經過 Raft）。用戶端打的是當時的 leader。

### 穩態：讀不受影響，寫入大約和 RTT 成反比

| 節點間 RTT（實測） | 登入（讀）吞吐 | 登入 p50／p99 | 計費 Start（寫）吞吐 | 寫入 p50／p99 |
| --- | --- | --- | --- | --- |
| 0.2 ms | 86,100／s | 0.7／1.4 ms | 42,700／s | 1.3／3.3 ms |
| 5.1 ms | 85,800／s | 0.7／1.5 ms | 3,940／s | 14.6／25.6 ms |
| 19.2 ms | 85,200／s | 0.7／1.5 ms | 826／s | 78／119 ms |
| 50.5 ms | 85,300／s | 0.7／1.6 ms | 318／s | 198／309 ms |

- **讀取不隨 RTT 改變：** 讀本機副本，不經過網路。
- **寫入吞吐在固定並行下大約和 RTT 成反比：** 吞吐乘以 RTT，在 5、19、50 ms 分別約 20,000、15,900、16,100；一次寫入的延遲約是 RTT 的 2.9、4.1、3.9 倍。也就是並行 64 時，寫入上限大約是 `64 ÷ (4 × RTT)`。要更高要加並行，沒有測。
- 這些寫入吞吐是對 leader 量的，對 follower 送寫入（要先轉給 leader）的吞吐沒有量。

### 殺掉 leader

用 `docker kill`（SIGKILL）殺掉 leader，用戶端每 50 ms 對一個存活的節點送一個會寫入的計費 Start。選舉時間是殺掉到存活節點之一的 `rradius_cluster_is_leader` 變成 1；寫入不可用時間是殺掉到第一個之後送出的寫入得到回應。

下表是**修補重連間隔之前**的量測，每點 5 次：

| 節點間 RTT（實測） | 選舉時間 | 寫入不可用時間 |
| --- | --- | --- |
| 0.15 ms | 5.7–6.9 秒 | 6.7–15.2 秒（5 次中 4 次 15 秒左右） |
| 5.2 ms | 5.0–6.4 秒 | 10.1–15.2 秒 |
| 20.0 ms | 6.0–6.9 秒 | 7.2–17.6 秒 |
| 50.1 ms | 6.4–6.8 秒 | 7.4–17.8 秒 |

- **選舉時間約 5 到 7 秒，量到的範圍內看不出和 RTT 有關。**
- **寫入恢復比選舉晚一點。** 原因：叢集資料庫的用戶端（hiqlite 0.16）把「現在的 leader 是誰」快取起來，只在重連的迴圈裡更新；原版的重連在前 5 次每 1 秒、之後每 5 秒，選舉在第 5 秒之後才完成時，要再多等一次 5 秒。現在 rradius 載入一份改過的 hiqlite（`vendor/hiqlite`），把 5 秒改成 1 秒（`RECONNECT_SLOW_INTERVAL`，改動記在該目錄的 `MODIFICATIONS`），所以每秒重連一次；這段時間節點每秒記一行 `Could not connect Client API WebSocket to node1`（舊 leader）。這個間隔是 hiqlite 內部的常數，沒有設定可以調。
- **2026-11-23 量（三節點，同一台主機的 Docker，RTT 0，停掉 leader）：** 選舉 6.3 到 6.7 秒，第一次寫入成功在 7.2 到 7.5 秒，即選舉後約 0.5 到 1 秒；同樣條件下改前的版本是 15.1 到 15.5 秒（3 次）。所以**單機實測的寫入中斷是約 7 到 8 秒**（選舉約 6 到 7 秒，加上用戶端重連）。每種只量 3 次，只在 RTT 約 0 的單機 Docker 上量過；廣域網路與選舉更久的情形沒有量。
- **規劃跨機房時，寫入中斷可以抓 20 秒左右當餘裕。** 這個數字來自上表改前的 15 到 18 秒，沒有在改後的版本與廣域網路上量過，是保守的估計，不是預期值。
- 登入（讀）不受影響，仍讀本機副本。

### 分區

用 `netem` 把節點之間的封包全丟（兩個方向），RADIUS 用戶端路徑不受影響。每種情形做 1 次，分區後約 5、12、22、32 秒取樣，每次對每個節點各送一個 `pap` 和一個計費 Start（逾時 2 秒）。RTT 0.2 與 20 ms 結果相同，除非另外註明。

| 情形 | 被隔開的節點 | 多數方 | 恢復（網路接回後） |
| --- | --- | --- | --- |
| 隔開一個 follower | 登入照常回應（讀舊副本）；計費不回應；約 10 秒內 `rradius_cluster_leader` 變 0，`seconds_without_leader` 一路增加；`applied_index` 停住 | 計費只有 leader 回應（0.2 ms 時兩台都回應；20 ms 時另一台 4 次取樣都沒有回應）；登入正常 | 追上 24.8 秒（0.2 ms）、29.3 秒（20 ms） |
| 隔開 leader | 登入照常回應；計費不回應；`rradius_cluster_is_leader` 仍是 1，但 `rradius_cluster_leader` 約 10 秒後變 0；`applied_index` 停住 | 選出新 leader（0.2 ms 時第 12 秒、20 ms 時第 22 秒的取樣已可見）；**32 秒取樣時計費仍然沒有回應**（包括新 leader）；登入正常 | 追上 1.3 秒（0.2 ms）、1.6 秒（20 ms）；舊 leader 退為 follower |

- 被隔開的節點副本停在分區當時，照常回應登入。不能接受的話設 `stop_without_leader_ms`（這次沒有設，也沒有測設了之後的行為）。
- 多數方的計費寫入在 leader 換人後要很久才恢復，這次觀察到分區後 32 秒仍沒有恢復。可能和上面殺掉 leader 是同一個原因，但分區時舊 leader 還活著，沒有驗證。

**隔開 follower 後的恢復時間（Linux，不用調 `tcp_retries2`）：** Raft 的節點之間連線（8100 埠）與 API 連線都設了 `TCP_USER_TIMEOUT` 10 秒：資料送出後 10 秒沒有收到對方的確認，核心就關掉這條連線，之後重連。沒有這個設定時，隔開期間沒有回應的連線每次重送間隔加倍，接回後要等下一次重送才送得出去，所以恢復時間隨隔開多久而變長。2026-11-25 用同樣的三節點 Docker 環境（RTT 0）量「接回後到這個節點知道 leader、追上資料、經它寫入有回應」，各 3 次的中位數：

| 隔開時間 | 沒有這個設定（2026-11-24，各 1 次） | 有這個設定（各 3 次） |
| --- | --- | --- |
| 12 秒 | 15.3 秒 | 6.4 秒（5.1 至 6.4） |
| 30 秒 | 29.4 秒 | 6.4 秒（5.1 至 8.2） |
| 60 秒 | 47.3 秒 | 4.0 秒（3.7 至 4.5） |

不管隔多久，都在 10 秒內恢復。要追的資料只有 3 到 12 筆，不是同步慢。這個設定只在 Linux 生效，其他系統維持原本的行為。

在節點之間 RTT 100 ms、每個封包 5% 掉包、不隔開的情形下連續寫入 5 分鐘（計費 Start，並行 16），有無這個設定的差別：吞吐 30.2/s 對 29.7/s，leader 沒有換人，也沒有 `Raft WebSocket stream ended` 日誌，所以這個值沒有造成誤判斷線。**更差的網路（持續掉包超過 10 秒、長時間完全中斷）沒有量**；那種情形連線被關閉是預期的，恢復後重連。

*歷史（2026-11-20，沒有 `TCP_USER_TIMEOUT` 的版本，各 1 次）：* 隔開 4、12、30、60 秒，恢復分別是 4.9、15.2、25.5、49.8 秒；把節點的 `net.ipv4.tcp_retries2` 從 15 降到 5 之後，隔開 12、30、60 秒的恢復變成 5.1、7.6、5.1 秒。現在的版本不需要調這個核心參數。

### 滾動升級

先量出該 RTT 的最高寫入吞吐，取一半，平均分給三個節點，每個節點各一支 `rradius-bench`（`acct-start`，固定速率，逾時 1 秒，不重送）；另外每個節點各有一個每 50 ms 一次的寫入探測。一次一台（先兩個 follower，最後 leader）`docker stop -t 30`、`docker start`，等日誌出現 `cluster node N has caught up with the cluster`。

| 節點間 RTT（實測） | 負載（共） | 每台停止到追上 | 停止本身 | 逾時的請求 | 升級時其他節點的最長空檔 |
| --- | --- | --- | --- | --- | --- |
| 0.2 ms | 21,600／s | 15.9、17.6、21.3 秒（leader 最後） | 14.6 秒 | 每個節點 5,500–6,500 個，約 0.2–0.3% | 0.1–1.4 秒 |
| 19.8 ms | 350／s | 17.1、17.1、21.5 秒（leader 最後） | 14.8 秒 | 每個節點 1,500–2,600 個，約 3.6–6.2% | 0.1–6.9 秒（停 leader 時 6.9） |

- **一台一台升級，另外兩台一直有回應。** 逾時的請求大部分是送給正在停機那一台的（用戶端沒有改問別台）。
- **核心版與企業版的執行檔同名（`rradiusd`），** 換版本時只換那個檔案，`rradiusd --version` 看得出每台是哪個版本別（核心版或企業版）和版本號。叢集裡同時有兩種版本別的節點，這份手冊沒有測過，不保證可以；要換的話先在測試叢集上試。核心版兩個建置混在同一叢集的情況，見[第 18 章〈升級程式〉](18-operations.md#升級程式)。
- **升級一台約 16 到 21 秒**（停止約 15 秒，加上啟動追上），leader 那台最久；停 leader 時其他節點的寫入空檔最長。
- 這次量測沒有測：新舊版本混在一起（同一個 image 重啟；兩個建置另外量過，見[第 18 章](18-operations.md#升級程式)）、資料量大時追上的時間。

### 三種拓撲

| | 跨三個機房的單一叢集 | 主叢集加冷備援 | 各機房獨立叢集 |
| --- | --- | --- | --- |
| 做法 | 三個節點分在三個機房，同一個叢集 | 一個機房跑叢集；另一個機房平時沒有節點，用企業版的 `[cluster.backup]`（[第 1 章](01-overview.md)）定期存副本，出事時把副本還原成新叢集 | 每個機房一個三節點叢集，互不相干 |
| 登入（讀） | 不受影響（量到 0.7 ms） | 同主叢集 | 同單機房 |
| 寫入（IP 配發、驗證碼、計費） | 每次約 3 到 4 個 RTT；並行 64 時 20 ms 約 830／s、50 ms 約 320／s | 同單機房 | 同單機房 |
| 一個機房整個失聯 | 剩兩個機房有多數；選舉約 5 到 7 秒，寫入中斷要抓餘裕（見上） | 主機房失聯就要靠還原，會丟掉上一份副本之後的寫入；切換要人操作 | 其他機房不受影響；失聯機房的用戶要靠別處 |
| 代價 | 寫入慢、升級期間的空檔變長；要自己確認真實網路比模擬的差多少 | 備援機房平時沒有熱資料；沒有量還原需要多久 | 帳號、設定、migration 要在每個叢集各做一次，一致性自己管 |

**熱備援目前做不到。** 備援機房常駐一個只複製、不投票的節點（learner）需要讓新節點一直當 learner；現在新節點先當 learner、複製完就自動升成 voter，沒有設定可以讓它停在 learner。

### 沒有測的部分

抖動與封包遺失、兩個方向延遲不同、實體主機與經過 WireGuard 的環境、真實 NAS 在選舉期間的重送行為、對 follower 送寫入的吞吐、更高的並行、五個節點、設了 `stop_without_leader_ms` 的分區、計費 spool 在高延遲下的行為、分區超過 30 秒、備份還原花的時間。

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
| `rradius_radius_discarded_total{reason="database"}` | counter | 因後端無法使用而沒有回應的請求：叢集資料庫（例如寫入失敗）、外部資料庫、LDAP、網域控制站，或逾時的密碼驗證 |

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
**原因：** 節點 1 還沒建好叢集（要其他每個節點都回覆之後才建），或其他節點還沒啟動。
**解法：** 等。出現 `has caught up with the cluster` 就沒事。一直沒出現的話，確認三個節點都啟動了、`raft` 與 `api` 位址互相連得到；節點 1 的日誌會寫出它在等哪些節點。

**症狀：** 節點 1 不服務，一直記 `Waiting for the other nodes before creating a new cluster: …`。
**原因：** 它沒有叢集資料，而清單裡至少有一個節點既沒有回覆叢集，也沒有回覆自己沒有叢集。
**解法：** 照[節點 1 在等待時](#節點-1-在等待時)處理警告列出的節點。

**症狀：** 第一次啟動的節點結束，印出 `not ready to apply migrations: node 2 did not join within 10s (the limit is migrations_wait_ms; raise it if a migration file takes longer)`，結束碼 1。
**原因：** 在 `migrations_wait_ms` 內加入不了叢集：其他節點沒在跑，或連不到。第一次啟動時，節點 1 要每個節點都回覆之後才建立叢集，所以少一個節點會讓其他節點（含節點 1）都這樣結束。
**解法：** 啟動每一個節點；節點 1 日誌裡的 `Waiting for the other nodes` 警告寫出缺哪一個。一起啟動時把 `migrations_wait_ms` 設長一點。

**症狀：** 啟動時印出 `cannot start cluster node 2: node id 2 is in use: …`，結束碼 2。
**原因：** 叢集裡另一個位址的節點已經用了這個 id，而且還在運作。
**解法：** 用沒人用的 id（先看 `cluster members`），或先停掉那個節點。

**症狀：** `cluster members` 或 `status --cluster` 印出 `127.0.0.1:27011 refused the api secret: [cluster] secret_api differs from the cluster's`，結束碼 2。
**原因：** 這份設定的 API 密碼跟叢集的不同。
**解法：** 用跟叢集相同的 `api.secret`。

**症狀：** `cluster members` 停了 30 秒，印出 `the cluster did not answer within 30 s`，結束碼 3。
**原因：** 設定裡的節點都沒有回應，或都沒有 leader。
**解法：** 確認節點在跑、API 埠連得到；失去多數節點時，先把節點救回來。兩個 voter、停掉的是 leader 時，見[移除節點](#移除節點)。

**症狀：** `cluster remove` 的最後一行是 `the change needs node 2 to commit: start it again with its data, wait about 20 s, stop it, and run this command again`，或只剩一個 `voter` 時，`cluster members` 一直顯示一個 `leaving` 節點與 `a change of the members is in progress`。
**原因：** 移除後只剩一個 voter 時，這個變更在等被移除節點自己的同意。
**解法：** 照[移除節點](#移除節點)的步驟做。

**症狀：** `cluster remove` 被拒絕（`InProgress`），最後一行是 `run this command again once cluster members shows no change in progress`。
**原因：** 先前的成員變更還沒提交。
**解法：** 等 `cluster members` 不再印 `a change of the members is in progress` 再執行一次。那一行一直都在的話，先完成那個變更。

**症狀：** `cluster remove` 印出 `node 4 still answers at 127.0.0.1:27014: stop it first, then remove it`。
**原因：** 要移除的節點還在運作。
**解法：** 先停掉它，再移除。

**症狀：** `cluster remove` 印出 `removing node 3 leaves two voters, and the cluster stops writing when either stops; give --force to remove it anyway`。
**原因：** 移除後只剩兩個 voter。
**解法：** 先加入新節點再移除舊的；確定要縮成兩個節點才加 `--force`。

**症狀：** 節點啟動後結束，或執行中突然結束，結束碼 1，日誌有 `cluster node 3 has been removed from the cluster: …`。
**原因：** 叢集目前的 leader 的成員清單裡沒有這個節點。
**解法：** 見[被移除的節點又啟動時](#被移除的節點又啟動時)：淘汰的主機停用服務；要加回就先確認叢集，再清空 `data_dir` 啟動。

**症狀：** 節點的 `/readyz` 回 503 `no leader`，登入沒有回應，`Discarded: no leader` 增加。
**原因：** 設了 `stop_without_leader_ms`，而這個節點沒有 leader 超過這段時間。
**解法：** 讓它跟其他節點恢復連線，或把停掉的節點救回來。`/livez` 仍是 200，不要因此重開它。

**症狀：** `status --cluster` 每一行的位址都一樣，或其他節點顯示 `unreachable`。
**原因：** 各節點的指標埠不同，或 `[metrics] bind` 是其他主機連不到的位址。
**解法：** 每個節點用同一個指標埠，`bind` 設成 `api` 位址的那個主機。

**症狀：** 強制結束過的節點啟動時警告 `Node did not shut down gracefully - auto-rebuilding State Machine`。
**原因：** 上次沒有正常關閉。
**解法：** 不用處理，它會自己重建。之後用 SIGTERM 停，並給它足夠的時間。

**症狀：** Kubernetes 上 pod 一直沒就緒，`startupProbe` 失敗。
**原因：** 選不出 leader：其他 pod 沒起來、`raft`／`api` 密碼不同，或 `rradius-cluster` 沒有 `publishNotReadyAddresses`。
**解法：** `kubectl logs` 看原因；密碼放在同一個 Secret。

**症狀：** Kubernetes 上從 NAS 送請求沒有回應，`unknown_client` 增加。
**原因：** 來源位址被負載平衡器改寫。
**解法：** 見[驗證來源位址沒有被改寫](#驗證來源位址沒有被改寫)。

**症狀：** 不同 NAS 的認證紀錄 `client=` 都相同，`source=` 是節點或閘道的位址。
**原因：** 來源位址被改寫，又剛好落在某個 client 的網段裡，所有 NAS 被當成同一個。
**解法：** 同上；把 `[[radius.clients]]` 縮成 NAS 實際的網段。

**症狀：** 三個 pod 都就緒，但停掉一個之後所有請求暫時沒有回應。
**原因：** 停的是 leader，剩下兩台還在選舉。
**解法：** 幾秒內會恢復；持續的話看 `rradius_cluster_leader`。

**症狀：** 新加的 migration 沒有套用（Kubernetes）。
**原因：** `statefulset.yaml` 的 `items` 漏了那一行，檔案沒有出現在 `migrations/` 裡。
**解法：** 見[第 18 章](18-operations.md)。

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
