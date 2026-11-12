# rradius-server 使用文件

rradius-server 是一個 AAA（認證、授權、計費）伺服器，支援 RADIUS 與 TACACS+。認證可以交給設定檔中的認證串接（SQL、LDAP、Lua），授權與計費由 Lua 腳本處理。資料存放在內嵌的 hiqlite（SQLite 加 Raft 複寫）叢集，可以單機執行，也可以組成多節點高可用叢集。

這份是參考文件，列出每個設定和行為。依部署情境的逐步說明和可以直接啟動的範例設定，見[使用手冊](manual/README.md)。

## 1. 建置與啟動

```bash
cd rradius-server
cargo build --release
./target/release/rradius-server --config /etc/rradius/rradius.toml
```

`rradius-server --version` 印出版本，例如 `rradius-server 0.1.0`。

**發行檔：** `rradius-<版本>-<平台>.tar.gz`，平台是 `macos-aarch64`、`linux-x86_64` 或 `linux-aarch64`，旁邊的 `.sha256` 可以用 `shasum -a 256 -c` 檢查。解開後的目錄結構和 repo 相同，所以範例設定和文件裡的相對路徑都照樣成立：

| 路徑 | 內容 |
| --- | --- |
| `bin/rradius-server` | 伺服器和管理指令 |
| `share/dictionary*` | RADIUS 字典 |
| `rradius-server/examples/` | 範例，含使用手冊的九個情境 |
| `docs/usage.md`、`docs/manual/` | 這份文件和使用手冊 |
| `README.md`、`VERSION` | 說明，以及版本和建置它的 commit；Linux 版另外寫出需要的 glibc 版本 |

- **Linux 版的 glibc：** 它在 Debian 12（bookworm）建置，需要的版本寫在壓縮檔的 `VERSION` 和 `README.md`。0.1.0 需要 glibc 2.34 以上，2026-11-08 在 Debian 12、Ubuntu 22.04（2.35）和 Rocky Linux 9（2.34）上確認過可以執行。更舊的發行版請用 Docker 映像（1.1 節）或自己建置。
- **正式部署時，** 把 `share/` 放到固定的位置（例如 `/usr/share/rradius`），設定檔的 `dictionary` 指向那裡；執行檔放到 `PATH` 裡。
- **自己產生發行檔：** `packaging/release.sh [--target <平台>]... [--out <目錄>] [--commit <rev>]`。沒有給 `--target` 時只建本機的平台；Linux 版用專案 `Dockerfile` 的建置階段在 Docker 裡建置，和 Docker 映像相同（arm64 一樣支援 64 KiB 分頁）。內容只取自指定的 commit，未提交的修改和被忽略的檔案不會打包。打包後會解開壓縮檔，用裡面的執行檔對各情境的設定執行 `--check`（有 `make-certs.sh` 或 `setup.sh` 的情境要先產生憑證或金鑰，不在檢查之內），失敗就不留下壓縮檔。

- 啟動只接受 `--config <path>` 一個參數；沒有給時印出 `usage: rradius-server --config <path>` 並以狀態碼 2 結束。
- `rradius-server --check --config <path>` 只檢查設定，見下方。`[radius.accounting_spool] dir` 也會檢查：要是目錄且寫得進去，還不存在時最近的上層目錄要寫得進去。只寫一個暫時檔再刪除，不會建立目錄或動到暫存內容。這是用執行 `--check` 的帳號判斷，和實際執行服務的帳號不同時結果可能不同。
- 設定檔錯誤（欄位拼錯、缺少必填項、腳本語法錯誤等）一律在啟動時失敗，狀態碼 2，錯誤訊息會指出設定檔路徑與出錯的欄位或檔案。
- 設定檔中的相對路徑都以設定檔所在目錄為準，與啟動時的工作目錄無關。
- 建置不需要另外安裝 Lua 或 LuaJIT：Luau 會隨程式一起編譯。
- **SQLite 以 `SQLITE_ENABLE_QPSG` 建置。** 資料庫用的 SQLite 隨程式一起編譯，repo 的 `.cargo/config.toml` 設了 `LIBSQLITE3_FLAGS="-DSQLITE_ENABLE_QPSG"`，在 repo 裡用 `cargo` 建置就會套用；`Dockerfile` 和 `packaging/release.sh` 也一樣。在 repo 外建置（例如只複製 `rradius-server/` 目錄）時要自己設這個環境變數；shell 裡已經有 `LIBSQLITE3_FLAGS` 時，`.cargo/config.toml` 不會蓋掉它，要自己把 `-DSQLITE_ENABLE_QPSG` 加進去。`packaging/release.sh` 會檢查發行檔裡的執行檔有沒有開，沒有就不打包。原因：SQLite 3.54 起，有 JOIN 又用參數篩選的查詢（例如寬頻情境的 `session-limit.lua`）、以及 `LIMIT ?`，每次綁定參數都會重新規劃，而每次讀取都會重新綁定。2026-11-08 在 Apple M4 Max 上量測寬頻情境的 PAP 登入（併發 64），沒有這個旗標時每秒約 17,000 次，有的話約 46,000–47,000 次，和 SQLite 3.53 相同。這個旗標讓查詢計畫不依參數值改變，只有在跑過 `ANALYZE`、有統計資料時才看得出差別，伺服器不會產生統計資料。`cargo test` 裡的 `test_sqlite_built_to_keep_query_plans` 會確認建出來的 SQLite 有開。

**只檢查設定：**

```bash
./target/release/rradius-server --check --config /etc/rradius/rradius.toml
```

做和啟動相同的檢查：設定檔、字典、migration 目錄、腳本語法、LDAP 的 `ca_file`。全部通過時 stdout 印 `configuration ok`，狀態碼 0；有問題時錯誤寫到 stderr，狀態碼 2。不開埠、不建立資料目錄，也不連線到 LDAP 或其他節點，所以 LDAP 帳號密碼錯誤、SQL 查詢寫錯這類問題查不出來。

範例設定在 `rradius-server/examples/rradius.toml`，可以直接啟動：

```bash
cd rradius-server
cargo run -- --config examples/rradius.toml
```

### 1.1 Docker

映像檔由專案根目錄的 `Dockerfile` 建置。範例設定在 `docker/config/`，使用 `sql` 處理器驗證，計費寫入 `accounting` 表。

單一節點：

```bash
docker/init-secrets.sh
docker compose up --build
```

三個節點，各自用不同的主機連接埠（`1812`/`1813`、`2812`/`2813`、`3812`/`3813`）：

```bash
docker/init-secrets.sh
docker compose -f compose.cluster.yaml up --build
```

確認服務是否回應（Status-Server、不存在的使用者會收到 Access-Reject，以及計費腳本能寫入；每次執行會在 `accounting` 資料表留下一筆 `o'brien` 的測試紀錄）：

```bash
docker/smoke-test.py --auth-port 1812 --acct-port 1813
```

- **密鑰：** `docker/init-secrets.sh` 產生 `docker/secrets/` 下的三個密鑰，這個目錄不進 git。NAS 要使用 `radius.secret` 的內容。
- **新增使用者：** 在 `docker/config/migrations/` 新增編號的 SQL 檔（已套用的檔案不能修改，見 2.5 節），再重新啟動。
- **client：** 範例允許三個私有網段，因為容器看到的來源位址由 Docker 決定。請改成你的設備位址。
- **TACACS+：** 範例沒有啟用。要用時在設定加上 `[tacacs]`，並在 Compose 發布 `49/tcp`。
- **統計：** 範例沒有啟用。要用時加上 `[metrics]`，並把 `bind` 改成容器可從外部連到的位址（見 2.8 節）。RadSec（`[radius.tls]`）需要 rradius-enterprise。
- **重新載入：** 修改 `docker/config/` 的腳本或 client 後，執行 `docker compose kill -s HUP rradius` 就會套用，不必重啟（見 2.9 節）。
- **資料：** 放在 Compose 的具名 volume。`docker compose down --volumes` 會刪除資料。
- **EAP：** 範例沒有啟用。要用時加上 `[radius.eap]`，憑證放在 `docker/config/`（見 2.12 節）。
- **計費暫存：** 範例沒有啟用。要用時，`[radius.accounting_spool].dir` 要指向 volume 裡的路徑（例如 `/var/lib/rradius/spool`；`/etc/rradius` 是唯讀的），否則換掉容器就遺失已確認的計費（見 2.11 節）。
- **停止：** `docker stop` 送的 SIGTERM 和 Ctrl+C 的 SIGINT 都會正常關閉資料庫，並等還在緩衝的記錄寫出，最多 5 秒。SIGKILL 不會，下次啟動時本機資料庫會重建。啟動途中也一樣能停止：等待叢集或套用 migration 時收到，會關閉節點後以狀態碼 0 結束，下次啟動接著做。正在跑超過 25 秒的 migration 檔時，等滿 25 秒就結束，這次不算乾淨關閉，下次啟動會重建本機資料庫；節點還沒起來時收到，會先等它起來（最多 25 秒）再正常關閉；25 秒內起不來（例如叢集沒有多數節點）就直接結束，這時無法乾淨關閉，資料目錄可能留下 `lock` 檔，下次啟動會重建本機資料庫。
- **多節點的關閉要約 10 秒：** hiqlite 關閉叢集成員前會先等 9.5 秒（讓負載平衡器有時間不再送請求過來；這段時間它還是 leader），之後其他節點才選出新 leader。所以 `docker stop` 要多給時間（`docker stop -t 30`，或 Compose 的 `stop_grace_period: 30s`，`compose.cluster.yaml` 已經設了），systemd 的預設 90 秒足夠。時間不夠被 SIGKILL 時，下次啟動會重建本機資料庫。
- **節點壞掉時自己結束：** 節點的 Raft 核心因錯誤停止時（例如下方 2.2 節的 follower 落後太多），或 hiqlite 內部 panic 時，程序記錄 `exiting so that a restart recovers it` 後以狀態碼 1 結束。所以一定要讓它自動重新啟動：systemd 設 `Restart=on-failure`，Docker 設 `restart: unless-stopped`（範例 Compose 都設了）。

## 2. 設定檔總覽

設定檔是 TOML 格式，由以下區段組成：

| 區段 | 必填 | 用途 |
| --- | --- | --- |
| `dictionary` | 是 | RADIUS 字典檔路徑（FreeRADIUS 格式，例如專案中的 `share/dictionary`） |
| `[lua]` | 是 | 最上層 Lua 腳本與 Lua 的時限 |
| `[cluster]` | 是 | 內嵌資料庫節點與叢集 |
| `[auth]` | 否 | 認證串接；沒有時由 `[lua].script` 認證 |
| `[radius]` | 否 | 啟用 RADIUS |
| `[radius.accounting_spool]` | 否 | 資料庫失敗時先把計費寫到本機（2.11 節） |
| `[radius.eap]` | 否 | EAP，給 802.1X 和 WPA-Enterprise 用（2.12 節） |
| `[tacacs]` | 否 | 啟用 TACACS+ |
| `[metrics]` | 否 | 啟用統計端點（2.8 節） |
| `[log]` | 否 | 記錄的等級、格式與認證紀錄（2.10 節） |
| `[accounts]` | 否 | `rradius-server user` 管理帳號時執行的 SQL（2.14 節） |
| `[runtime]` | 否 | `pin_threads`：把工作執行緒各綁在一顆 CPU 上，只在 Linux（2.4 節） |

`[radius]` 與 `[tacacs]` 至少要有一個。所有區段都拒絕不認得的欄位，拼錯的欄位會讓啟動失敗並指出欄位名稱。

**企業版功能：** 下列功能需要 rradius-enterprise，說明在 rradius-enterprise 使用文件。設定檔用到這些區段或欄位時，rradius-server 啟動、SIGHUP 和 `--check` 都會失敗，訊息寫明需要 rradius-enterprise。

| 功能 | 設定或指令 | 用途 |
| --- | --- | --- |
| RadSec | `[radius.tls]`；`[[radius.clients]]` 的 `tls_name`、`coa_sender`、`max_radsec_connections` | RADIUS over TLS（RFC 6614） |
| 代理 | `[proxy]` | 依 realm 把 RADIUS 請求轉給其他伺服器，包括 RadSec 上游、依 DNS 尋找上游和上游全掛時暫存計費 |
| 中斷連線與變更權限 | `send_disconnect`、`send_coa`、`disconnect` 和 `coa` 指令、`[radius.coa]` | 要求 NAS 中斷連線或變更權限（RFC 5176），以及接收其他系統的請求轉給 NAS |
| 動態 client | `[radius.dynamic_clients]`、`[accounts.clients]`、`client` 指令 | 從資料庫查不在設定檔裡的 UDP client |
| 從四次交握找出 WPA-PSK | `[radius.dpsk]`、`[accounts.dpsk]`、`dpsk` 和 `dpsk-seal` 指令 | 每人一把 WPA-PSK，設備不必先登記 |
| EAP-AKA' | `methods` 的 `aka-prime`、`[radius.eap.aka]`、`[accounts.aka]`、`aka` 和 `aka-seal` 指令 | 自己發的 USIM 卡 |
| EAP session resumption | `[radius.eap]` 的 `resumption_ms`、`shared_tickets`、`ticket_seal_file` | 用戶端重新連線時省掉完整握手 |
| OCSP | `[radius.eap]`、`[radius.tls]`、`[tacacs.tls]` 的 `ocsp`、`ocsp_url` | CA 只提供 OCSP、不發 CRL 時檢查撤銷 |
| TACACS+ over TLS | `[tacacs.tls]`；`[[tacacs.clients]]` 的 `tls_name` | RFC 9887 |

最小設定：

```toml
dictionary = "/usr/share/rradius/dictionary"

[lua]
script = "auth.lua"

[cluster]
data_dir = "data"
secret_raft_file = "raft.secret"
secret_api_file = "api.secret"

[radius]
[[radius.clients]]
address = "127.0.0.1"
secret_file = "local.secret"
```

### 2.1 `[lua]`

| 欄位 | 預設 | 說明 |
| --- | --- | --- |
| `script` | （必填） | 最上層腳本 |
| `query_timeout_ms` | 5000 | 一個請求的所有 `sql_query`／`sql_execute` 合計可以等資料庫多久，必須大於 0。每次呼叫拿剩下的時間，用完後的呼叫立刻以逾時失敗；算的是等資料庫的時間，等執行名額的時間不算 |
| `max_execution_ms` | 1000 | 腳本在一個請求中的執行時間上限（不含等待資料庫的時間），必須大於 0 |
| `max_memory_mb` | 256 | 每個 Lua VM 最多使用的記憶體（MiB），必須大於 0，改了要重啟。VM 數量等於 CPU 核心數，所以最壞情況是兩者相乘（4.1 節） |

兩個時限同時套用於最上層腳本與認證串接中的 `lua` 處理器。

最上層腳本負責：
- RADIUS 計費（Accounting-Request）
- TACACS+ 授權與計費
- 沒有被認證串接涵蓋的認證：設定中沒有 `[auth]`，或 `[auth]` 只有具名串接而用戶端沒有指定 `auth` 時

舊版設定中的最上層 `script = "…"` 與 `[cluster]` 的 `query_timeout_ms` 已不再接受；啟動時會提示新位置（`[lua].script`、`[lua].query_timeout_ms`）。

### 2.2 `[cluster]`

| 欄位 | 預設 | 說明 |
| --- | --- | --- |
| `data_dir` | （必填） | 資料目錄 |
| `secret_raft` / `secret_raft_file` | （二擇一） | Raft 通訊密碼，至少 16 個字元 |
| `secret_api` / `secret_api_file` | （二擇一） | API 通訊密碼，至少 16 個字元 |
| `node_id` | 1 | 本節點編號，必須 ≥ 1 並出現在 `nodes` 中 |
| `listen` | 沒有 `[[cluster.nodes]]` 時 `127.0.0.1`，有時 `0.0.0.0` | 監聽位址（IPv4 或 IPv6）。單節點沒有其他節點要連，資料庫 API 又只靠共用密碼保護，所以預設不對外 |
| `migrations_dir` | （無） | 啟動時套用的 SQL 檔目錄，見 2.5 節 |
| `migrations_wait_ms` | 60000 | 啟動時等待叢集就緒、本機副本追上的時限（每段各自計時），必須大於 0，見下方〈啟動時追上叢集〉 |
| `stop_without_leader_ms` | （不設） | 沒有 leader 超過這段時間時，認證和 Status-Server 都不回應，必須大於 0，改了要重啟，見下方〈資料庫不能用時〉 |
| `snapshot_every` | 100000 | 兩次快照之間的 Raft 紀錄數，必須大於 0，見下方 |
| `read_connections` | 工作執行緒數的兩倍，最多 32 | 讀本機資料庫的連線數，1 到 128，見下方 |
| `[[cluster.nodes]]` | 單節點：id 1、Raft `127.0.0.1:8100`、API `127.0.0.1:8200` | 叢集成員，每個有 `id`、`raft`、`api`（`host:port`） |

密碼檔讀取時會去掉結尾的換行。多節點叢集的每個節點都列出全部成員（包含自己），必須有編號 1 的節點，編號不可重複：

```toml
[cluster]
node_id = 2
data_dir = "data"
secret_raft_file = "raft.secret"
secret_api_file = "api.secret"

[[cluster.nodes]]
id = 1
raft = "10.0.0.1:8100"
api = "10.0.0.1:8200"
[[cluster.nodes]]
id = 2
raft = "10.0.0.2:8100"
api = "10.0.0.2:8200"
[[cluster.nodes]]
id = 3
raft = "10.0.0.3:8100"
api = "10.0.0.3:8200"
```

本節點監聽自己在 `nodes` 中 `raft` 與 `api` 的埠。寫入（`sql_execute`）經由 Raft 複寫到所有節點，需要叢集有多數節點可用；讀取（`sql_query`）讀本機的副本。

**節點不需要系統的 CA 憑證。** 節點之間的連線是明文 HTTP（見下方），用不到憑證；但建立這個連線用的 HTTP client 時，系統一張 CA 憑證都沒有就會失敗，所以 rradius 編入了一份內建的根憑證。沒裝 `ca-certificates` 的主機或極簡容器也能組叢集。LDAP over TLS（3.5 節）和腳本的 `http_request`（4.11 節）不用這份內建憑證，仍然讀系統的憑證或 `ca_file`。

**三節點叢集的吞吐。** 2026-11-08 在同一台 Apple M4 Max 上 Docker 的 Linux（arm64）量測：三個節點各限定 3 顆 CPU（0–2、3–5、6–8），以 127.0.0.11 到 .13 互連；`rradius-bench` 開三個程式各打一個節點，共用 CPU 9–13，各兩次。登入是寬頻情境的 PAP（`sql` 加 `lua` 處理器），計費是每筆 Accounting-Start 寫一次資料庫（`acct-sql.toml`）。單節點的兩欄是同樣的設定、同樣三個壓測程式都打同一個節點：

| 情況 | 單節點 3 顆 CPU（次/秒） | 單節點 9 顆 CPU（次/秒） | 三節點各 3 顆 CPU（次/秒） |
| --- | --- | --- | --- |
| 登入，併發 192 | 54,900–55,800，p99 5.4–5.7 ms | 73,000–73,200，p99 4.2–4.3 ms | 110,600–118,000，p99 3.2–3.8 ms |
| 登入，併發 768 | 51,600–54,500，p99 18–19 ms | 66,000–66,200，p99 14–15 ms | 105,000–113,600，p99 9–12 ms |
| 計費，併發 192 | 68,000–68,100，p99 6.6–6.8 ms | 116,400–116,500，p99 5.2–5.4 ms | 73,700–74,100，p99 8.5–9.6 ms |
| 計費，併發 768 | 66,000–66,700，p99 13–14 ms，每輪 480–630 個逾時 | 98,600–100,900，p99 13–14 ms | 102,400–107,200，p99 20 ms |

- **登入在每個節點各自處理，** 讀的是本機副本，所以三節點約是單節點 3 顆 CPU 的兩倍，也比單節點用同樣 9 顆 CPU 多五到七成。三個節點的 CPU 都用滿了，但沒有到三倍：同時忙碌的 CPU 越多，每次登入用的 CPU 時間越多（單節點 3 顆約 54 µs，三節點約 74 µs，單節點 9 顆約 97 µs），這台機器有效能核心和節能核心，虛擬機的 CPU 可能落在兩種核心上，沒有確認。
- **計費寫入都要經過 leader，** 並等多數節點寫進紀錄，所以多節點不會提高寫入上限：併發 192 時只比單節點 3 顆 CPU 多一點、p99 較高，併發 768 時和單節點 9 顆 CPU 差不多，也沒有單節點 3 顆 CPU 的逾時。
- **三個節點在同一台機器的 loopback 上，沒有網路延遲。** 分開的主機之間，每筆寫入都要多等節點之間的往返（跨機房更久），計費的數字會比這裡低；登入不受影響。

**讀取連線 `read_connections`：** `sql` 處理器查帳號、腳本的 `sql_query` 這些讀取，都要先拿到一條讀取連線。連線不夠時，請求要排隊等別人讀完。預設是工作執行緒數（程序可用的 CPU 數）的兩倍，最多 32 條：每條連線都有自己的頁面快取和檔案，CPU 很多的主機不該預設開上百條，量測也看不到 14 條以上的好處。可以設 1 到 128。寫入不受影響，仍然只有一條寫入連線。

- 2026-11-08 在 Docker 的 Linux（arm64，伺服器用 CPU 0–6、`rradius-bench` 用 7–13，各 2 次）量測寬頻情境的 PAP 登入（每次讀兩次資料庫）：4 條（以前的值）時併發 64 每秒 46,800–47,100 次、p50 1.32–1.33 ms，預設的 14 條時 74,500–75,300 次、p50 0.76–0.77 ms；併發 1024 時從 44,200–44,400 次升到 57,400–58,200 次。開了綁核（2.4 節）時，14 條在併發 64 可到每秒約 9 萬次。用實驗版本加到 28 條，和 14 條幾乎一樣。
- 代價是記憶體：每條連線有自己的頁面快取，同樣的負載後，14 條比 4 條多用約 27–47 MB（159–185 MB 比 112–158 MB）。記憶體很緊時可以調低，改了要重新啟動。

**快照間隔 `snapshot_every`：** 每累積這麼多筆 Raft 紀錄（一筆是一次寫入，或一批一起提交的寫入），節點就把整個資料庫複製一份當快照，之後就可以丟掉這之前的紀錄。複製時寫入都要等，資料庫越大越久，所以太頻繁會拖慢服務；但兩次快照之間的紀錄要保留，異常結束後重新啟動也要從上一次快照重播這些紀錄。

- 2026-11-08 在 Docker 的 Linux（arm64）量測，寬頻情境 PAP 併發 256 加每秒 2,000 筆計費，12 分鐘：設成 10,000 時，快照最後每 5–6 秒一次，每次複製 150–350 MB，有 4 個 10 秒區段的吞吐量掉到每秒 1.5–4.3 萬次、p99 42–54 ms；設成 100,000 時只做了 10 次快照，平均每秒 57,273 次（10,000 時 54,365 次），最低的區段 54,232 次，p99 沒有超過 15 ms。
- 代價（同樣的機器，寫入 90,000 筆後停止再啟動）：正常停止（SIGTERM）再啟動，兩種設定都在 0.1 秒內；**異常結束（SIGKILL、斷電）再啟動，10,000 時 0.95 秒，100,000 時 10.3 秒**，因為要重播上次快照以來的紀錄，大約每筆 0.1 ms。資料目錄也比較大（75 MB 比 26 MB）。
- 寫入不多、更在意異常結束後多快恢復時，調低到例如 10,000 或 20,000。改了要重新啟動。
- hiqlite 也用這個值決定落後多少筆的節點改收快照、而不是逐筆補上紀錄（這個值的兩倍，預設 200,000 筆，hiqlite 自己的預設是 20,000）。

**節點之間的連線沒有加密，也沒有 TLS 設定可以開。** 多節點叢集的 `raft` 和 `api` 埠都是明文：

- **能監聽這段網路的人，讀得到複寫中的資料：** 包含 `users` 資料表裡的密碼、計費紀錄。
- **`secret_api` 會以明文送出：** 節點查詢成員時放在 HTTP 標頭裡。側錄到它的人，連得到 `api` 埠就能在資料庫執行 SQL，例如新增或修改使用者。
- **`secret_raft` 不會直接送出，但連線內容不受保護：** 能攔截連線的人可以竄改握手之後的內容。

所以節點之間要走你信任的網路。跨機房或跨不信任的網段時，把這段連線放進 WireGuard 或 IPsec，並把 `raft` 和 `api` 的位址設成那個網路上的位址。不論哪一種，都用防火牆把這兩個埠限制成只有其他節點連得到，並用 `listen` 綁定到那個介面。預設的 `0.0.0.0` 會監聽所有介面。

**用 WireGuard 保護節點之間的連線。** 以下是三個節點的例子：實體位址 `192.0.2.1` 到 `192.0.2.3`，WireGuard 網路 `10.99.0.0/24`，介面 `wg0`，以 Linux 和 `wg-quick` 為例。NAS 照樣連各節點的實體位址，`[radius]` 和 `[tacacs]` 不用改。

1. 每個節點產生自己的金鑰，私鑰只留在本機：

   ```sh
   umask 077
   wg genkey | tee /etc/wireguard/private.key | wg pubkey > /etc/wireguard/public.key
   ```

2. 每個節點寫 `/etc/wireguard/wg0.conf`，把其他兩個節點列為 peer。節點 1 的寫法：

   ```ini
   [Interface]
   Address = 10.99.0.1/24
   ListenPort = 51820
   PrivateKey = <節點 1 private.key 的內容>

   [Peer]   # 節點 2
   PublicKey = <節點 2 public.key 的內容>
   Endpoint = 192.0.2.2:51820
   AllowedIPs = 10.99.0.2/32

   [Peer]   # 節點 3
   PublicKey = <節點 3 public.key 的內容>
   Endpoint = 192.0.2.3:51820
   AllowedIPs = 10.99.0.3/32
   ```

   `AllowedIPs` 只寫對方的一個位址，WireGuard 就只接受用那把金鑰加密、來源是那個位址的封包，其他節點冒用不了。啟用：`systemctl enable --now wg-quick@wg0`。

3. `[cluster]` 改用 WireGuard 位址，`listen` 寫明本節點的 WireGuard 位址，不要用預設的 `0.0.0.0`。節點 1 的寫法：

   ```toml
   [cluster]
   node_id = 1
   listen = "10.99.0.1"
   data_dir = "data"
   secret_raft_file = "raft.secret"
   secret_api_file = "api.secret"

   [[cluster.nodes]]
   id = 1
   raft = "10.99.0.1:8100"
   api = "10.99.0.1:8200"
   [[cluster.nodes]]
   id = 2
   raft = "10.99.0.2:8100"
   api = "10.99.0.2:8200"
   [[cluster.nodes]]
   id = 3
   raft = "10.99.0.3:8100"
   api = "10.99.0.3:8200"
   ```

4. 用防火牆讓 `raft` 和 `api` 的埠只收 `wg0` 進來的連線。只靠 `listen` 不夠：Linux 預設也會收下從實體網卡進來、目的地是 `10.99.0.1` 的封包。以 nftables 為例，節點 1：

   ```
   table inet rradius_cluster {
     chain input {
       type filter hook input priority 0; policy accept;
       udp dport 51820 ip saddr { 192.0.2.2, 192.0.2.3 } accept
       udp dport 51820 drop
       tcp dport { 8100, 8200 } iifname { "wg0", "lo" } accept
       tcp dport { 8100, 8200 } drop
     }
   }
   ```

   `lo` 是本機連自己的位址時走的介面。

5. rradius 要在 `wg0` 起來之後才啟動，否則綁不到 `listen` 的位址。用 systemd 時，在 rradius 的服務加上（`systemctl edit <服務名稱>`）：

   ```ini
   [Unit]
   Requires=wg-quick@wg0.service
   After=wg-quick@wg0.service
   ```

6. 確認：在實體網卡上抓不到 `8100` 和 `8200`，只看得到 WireGuard 的 UDP。`wg0` 上看得到明文是正常的，那是解密之後的內容。

   ```sh
   tcpdump -ni eth0 'tcp port 8100 or tcp port 8200'   # 應該沒有封包
   tcpdump -ni eth0 udp port 51820                     # 節點之間的流量
   wg show wg0                                          # 每個 peer 都有最近的 latest handshake
   ```

要注意的地方：

- **增減節點時，每台都要改。** 新節點要加進其他每台的 peer 和防火牆的來源位址，`[[cluster.nodes]]` 也一樣。改完設定檔可以用 `wg syncconf wg0 <(wg-quick strip wg0)` 套用，不必中斷現有連線。
- **金鑰沒有撤銷機制。** 換金鑰時，先在該節點產生新金鑰，再更新其他每台 peer 的 `PublicKey`。期間這個節點連不上其他節點，所以一次換一台，等它重新跟上再換下一台，叢集才能一直保有多數節點。
- **MTU 不必調整。** `wg-quick` 會自動把 `wg0` 設小一點，TCP 會自己配合。
- **已經有 IPsec 的環境也可以用，原則相同：** 節點位址放在受保護的網路上，`listen` 綁那個位址，防火牆只收受保護的流量。

#### 啟動時追上叢集

節點啟動後，不論有沒有 `migrations_dir`，都先等到有 leader，再經 leader 寫一個自己的標記（資料表 `rradius_nodes`），等本機副本讀到它才開始監聽。寫入依序套用，所以讀到標記就代表停機期間叢集寫入的帳號、NAS、金鑰都已經在本機，第一個回應就看得到。完成時記錄 `cluster node N has caught up with the cluster`。

- 兩段等待各自以 `migrations_wait_ms` 為上限。有 `migrations_dir` 時等不到就以狀態碼 1 結束，和 migration 失敗相同；沒有時記一行 WARN（`serving before catching up`）後照常服務，這時答案可能來自舊資料。
- 停機很久、要補的資料很多時，追上要比較久。

#### 資料庫不能用時

- **不回應，而不是拒絕。** 登入因為叢集資料庫逾時、找不到 leader，或等待寫入的數量滿了而無法決定時，RADIUS 不回覆，計入 `rradius_radius_discarded_total{reason="database"}`；TACACS+ 的認證和授權回 ERROR，設備會改問下一台伺服器（RFC 8907）。這適用於 `sql` 處理器、回 `reject` 或出錯的 `lua` 處理器（回 `ignore` 時照常交給下一個處理器）和沒有接受的最上層腳本，以及 EAP 對話。確定是帳號不存在、密碼錯，或腳本的資料庫呼叫都成功後才拒絕，照樣回 Access-Reject（FAIL）。
- **寫入有上限。** 等著交給叢集的寫入最多 16384 個，滿了直接失敗；交出去的一批最多等 30 秒，到時放開名額、回報失敗；沒有可用的 leader 超過 5 秒時，新的寫入立刻失敗，不排隊。所以被隔開的節點不會因為一直排隊而耗光記憶體。
- **交給叢集的寫入等到結果才回報。** 合併提交的 `sql_execute`（不回傳資料列的 INSERT、UPDATE、DELETE、REPLACE）還在排隊時，請求的資料庫時限一到就放棄，之後也不會交出去；一旦交給叢集（leader 在做快照、換 leader 時可能要等），就等叢集回覆結果，不再中途放棄，最多等 30 秒。所以「回報失敗、之後卻寫進去」只剩 30 秒內都沒有結果這種情況，這時呼叫回報結果不明（記錄寫 `may have been committed`），不算資料庫不能用：計費不進暫存也不回覆，由 NAS 重送，重送時可能重複一次，所以計費腳本仍要能重複執行。登入沒回應時，腳本已經做的寫入（配出的 IP、用掉的驗證碼）在這種情況下也可能留下來。代價是 leader 很慢時，一個請求最多多等 30 秒，占著它的名額；等待的時間照樣算進這個請求的資料庫時限，所以同一個請求之後的呼叫可能立刻逾時，計費可能因此進暫存、重放時再跑一次。帶 `RETURNING` 和其他不合併提交的語句照舊以時限為準，逾時後仍可能寫入。
- **被隔開的節點讀的是舊資料。** 讀取用本機副本，不需要多數節點，所以只讀的登入照常回應；但在另一側停用的帳號、刪掉的 NAS、撤掉的金鑰，在它這裡都還有效，SIGHUP 也沒用，直到網路恢復。
- **`stop_without_leader_ms`：** 設了之後，節點沒有可用的 leader 超過這段時間，就不回應任何 Access-Request 和送到認證埠的 Status-Server（`reason="no_leader"`），TACACS+ 的認證和授權回 ERROR，讓 NAS 和負載平衡器把它當成故障、改問其他節點。計費照常（有暫存時進暫存）。代價是叢集只剩這一台時，它也不再服務只讀的登入，所以預設不設。
- **沒有多數節點時不能重新啟動。** 三台剩一台時，正在跑的那台照常服務只讀的登入；但它一旦重新啟動，就要等到有 leader 才起得來：hiqlite 重新啟動叢集成員時，會等到知道 leader 是誰才把資料庫交給 rradius。有 `migrations_dir` 時等 `migrations_wait_ms` 後以狀態碼 1 結束，沒有時一直等。所以叢集失去多數時，不要重啟剩下的節點，先把其他節點救回來。
- **落後太多的 follower 會壞掉，要重新啟動。** follower 停住或斷線期間，叢集寫入超過 `snapshot_every`（預設 10 萬筆）、leader 做了快照並丟掉舊日誌時，follower 回來後 hiqlite 0.16 讀不到自己的日誌，Raft 核心停止（記錄 `quit RaftCore ... Integrity: Attempt to read non-existent data`）。2026-11-10 在三節點上用預設設定、暫停一台時寫入 13 萬筆重現；`snapshot_every = 50` 時每次都重現。這時節點會用不再跟隨叢集的副本回答，所以 rradius 偵測到後會乾淨關閉並以狀態碼 1 結束（見 1. 節〈停止〉），重新啟動後它會追上、恢復正常。沒有自動重啟時，要手動啟動它。
- **停掉 leader 時不會先交出 leader。** hiqlite 0.16 在關閉前先等 9.5 秒（仍是 leader），之後其他節點要 1.5 到 3 秒選出新的，這幾秒寫入逾時。所以升級時最後才停 leader（`rradius_cluster_is_leader` 為 1 的那台）。
- **錯誤怎麼分類。** 逾時、找不到 leader、連不到節點、內部通道關閉、快照失敗都算資料庫不能用；SQL 寫錯、違反限制（例如 UNIQUE）算語句的錯，照樣拒絕或回錯誤。管理指令 `user` 遇到前者以狀態碼 3 結束。
- **可用的 leader：** follower 知道 leader 是誰；leader 在 5 秒內得到多數節點確認（被隔開的 leader 仍自認是 leader，用這個分辨）。

#### 叢集狀態指標

節點每秒讀一次自己的 Raft 狀態，在 `/metrics` 提供（2.8 節）：

| 指標 | 意思 |
| --- | --- |
| `rradius_cluster_leader` | 有可用的 leader 時 1，否則 0 |
| `rradius_cluster_is_leader` | 這台是 leader 時 1 |
| `rradius_cluster_applied_index` | 本機副本最後套用的紀錄編號；和其他節點比，看出落後多少 |
| `rradius_cluster_seconds_without_leader` | 沒有可用 leader 的秒數，有時是 0 |

升級時最後才停 `rradius_cluster_is_leader` 為 1 的那台：停掉 leader 要重新選舉，那幾秒的寫入會逾時。

### 2.3 `[radius]` 與 `[tacacs]`

```toml
[radius]
bind = "0.0.0.0"     # 預設 0.0.0.0；也可以是列表，例如 ["192.0.2.1", "2001:db8::1"]
auth_port = 1812     # 預設 1812
acct_port = 1813     # 預設 1813
max_auth_inflight = 4096  # 預設 4096，同時處理的認證請求數
max_acct_inflight = 4096  # 預設 4096，同時處理的計費請求數
# sockets = 7       # 每個位址和 port 開幾個 socket；Linux 預設等於工作執行緒數，其他平台只能 1

[[radius.clients]]
name = "core-switches"          # 選填；沒有時以 address 當名稱
address = "192.168.2.0/24"      # IP 或 CIDR 網段
secret_file = "core.secret"     # 或 secret = "..."，二擇一
auth = "vpn"                    # 選填；指定具名認證串接

[tacacs]
bind = "0.0.0.0"     # 預設 0.0.0.0
port = 49            # 預設 49（需要權限）
max_body_len = 65536 # 預設 65536，超過的封包會中斷連線
max_connections = 1024 # 預設 1024，同時開啟的連線數
idle_timeout_ms = 300000 # 預設，這段時間沒有收到完整封包就關閉，必須大於 0
enable = "deny"      # 預設；enable 請求直接 FAIL。"login" 時和登入一樣驗證（4.6 節）
# TACACS+ over TLS（[tacacs.tls]）需要 rradius-enterprise

[[tacacs.clients]]
name = "router-1"
address = "10.0.0.5"
secret_file = "router-1.secret"
# authorize_group = "group15"   # 選填；授權改由規則決定，見下方〈TACACS+ 授權群組〉
```

**`bind` 用列表時：**

- 每個位址都服務認證和計費，回覆從收到請求的 socket 送出。列明確位址時，來源就是那個位址。萬用位址的來源由路由決定，主機有多個位址時 NAS 可能因來源不符而丟棄回應，這時要改列明確位址。
- 超過一個位址時，IPv6 位址只收 IPv6，所以 `["0.0.0.0", "::"]` 可以同時收兩種。**`["::", "10.0.0.1"]` 只有 `10.0.0.1` 收 IPv4。** 只有一個位址時，不論是不是列表都和以前相同：`"::"` 能不能收 IPv4 由系統決定。
- 萬用位址不能和同家族的其他位址並列，例如 `["0.0.0.0", "192.0.2.1"]` 啟動時就會失敗，因為它已經占用了該家族所有位址的埠。IPv4 對應位址（`::ffff:10.0.0.1`）在多個位址的列表裡也不行，請寫成 IPv4。
- **綁定失敗時程序會結束，** 狀態碼 1，訊息寫出位址和埠。`[tacacs]`、`[metrics]` 綁定失敗仍然只記一行錯誤，這些也只能有一個位址。

**`sockets`：每個 port 有幾個接收迴圈。**

- Linux 上每個位址的認證和計費 port 各開 `sockets` 個 SO_REUSEPORT socket，各有自己的接收迴圈。核心依來源位址和 port 把封包分給其中一個，同一個來源一直落在同一個，所以重送還是由同一個迴圈處理，去重和回覆都不受影響；回覆從收到請求的那個 socket 送出。預設等於工作執行緒數（程序可用的 CPU 數），啟動時記錄 `RADIUS: <N> sockets on each address and port`。
- **其他平台只能是 1。** macOS 等系統不會把封包分散到多個 socket，設大於 1 時啟動失敗。0 也會失敗。
- **同一個使用者的另一個程序也能綁同一個 port。** SO_REUSEPORT 讓有效 UID 相同、也開了這個選項的程序共用 port，封包會分給兩邊。用同一份設定再啟動一個 rradius 時，它會先在叢集的 port 失敗而結束，碰不到 RADIUS port；但叢集 port 不同、RADIUS port 相同的另一份設定就會分走流量。請用專用帳號執行，需要以前的行為時設 `sockets = 1`（這時 port 被占用就啟動失敗）。
- 改了要重新啟動，SIGHUP 時會記錄需要重啟的設定。
- 量測見 2.4 節。

- 用戶端以「最長前綴」比對來源位址，與列出順序無關；同一網段重複列出會啟動失敗。
- 不在任何用戶端網段內的來源會被忽略（RADIUS 不回應，TACACS+ 直接關閉連線）。
- 密碼檔去掉結尾換行後不可為空。
- 區段存在但沒有用戶端時啟動失敗。

#### TACACS+ 授權群組 `[tacacs.authorize_groups]`

不寫腳本，用一行一條的規則決定 TACACS+ 授權。規則依連線過來的設備套用：`[[tacacs.clients]]` 設了 `authorize_group` 的設備，授權只看該群組的規則。

```toml
[tacacs.authorize_groups]
group15 = [
  'permit service=shell cmd\* {priv-lvl=15}',
  'permit service=shell cmd=.*',
]
readonly = [
  'permit service=shell cmd\* {priv-lvl=1}',
  'deny cmd=show cmd-arg=running-config',
  'permit service=shell cmd=(show|ping|traceroute|exit)',
]

[[tacacs.clients]]
name = "core-routers"
address = "10.50.0.0/24"
secret_file = "tacacs.secret"
authorize_group = "group15"
```

- **格式：** `permit|deny <樣式> ... [{<AV> <AV> ...}]`，以空白分隔。`{}` 裡的 AV pair 寫成 `name=value`（或選填的 `name*value`），`permit` 時原樣回給設備；`deny` 不能帶 `{}`。
- **值含空白時用雙引號包住，** 例如 `{shell:roles="network-admin vdc-admin"}`。引號之間的空白和 `}` 都算值的一部分，引號本身也照樣送給設備：寫什麼就送什麼，上例送出的就是 `shell:roles="network-admin vdc-admin"` 一個 AV pair。設備不要引號時就不要寫。樣式部分不支援引號。
- **一定要用單引號。** TOML 的雙引號字串不接受 `\*` 這類寫法，會在讀設定時就失敗。
- **樣式是正規表示式，整串比對** 請求中的一個 AV pair，包括中間的 `=` 或 `*`。所以 `cmd=show` 不會符合 `cmd=showx`，`cmd\*` 符合設備開 shell（EXEC 授權）時送的 `cmd*`。比對以位元組進行，`.` 符合任何位元組。`user`、`port`、`rem_addr` 不參與比對。
- **一條規則的每個樣式都至少符合一個 AV pair 時成立。** 沒有樣式的規則什麼都符合。
- **第一條成立的規則決定：** `permit` 回 PASS_ADD，`deny` 回 FAIL。全部都不成立時回 FAIL，空的群組（`[]`）所有授權都 FAIL。要排除特定指令，把 `deny` 放在較寬的 `permit` 之前。
- **設了 `authorize_group` 的設備不執行腳本授權。** 認證和計費照舊。同一群組的設備，每個使用者得到的授權都一樣；要依使用者或角色決定時，那些設備不要設 `authorize_group`，改用腳本（4.6 節）。
- **限制：** 沒有「值含空白但不送引號」的寫法，也沒有跳脫字元；只回 PASS_ADD，沒有 PASS_REPL 和 `server_msg`。
- **檢查：** 啟動、SIGHUP 和 `--check` 時，規則開頭不是 `permit`/`deny`、正規表示式寫錯、`{}` 不成對或 `}` 後還有內容、`{}` 裡的雙引號不成對、AV pair 沒有 `=`/`*` 或名稱為空、超過 255 octets 或超過 255 個、群組名稱為空、`authorize_group` 指向不存在的群組、在 `[[radius.clients]]` 設 `authorize_group`，都會失敗，訊息指出 `tacacs.authorize_groups.<名稱>` 和第幾條規則。
- 規則隨 SIGHUP 套用，已開啟的連線下一個請求就用新規則（2.9 節）。
- **量測：規則比腳本快。** 2026-11-09 在 Apple M4 Max 上 OrbStack 的 Linux（arm64）量測，`main` `5ef8248`，伺服器限定 CPU 0–6、`rradius-bench` 7–13，`tacacs-author,concurrency=64,threads=7`（`service=shell`、`cmd=show`、`cmd-arg=version`），每列三次，都沒有逾時和 `invalid`：

  | 授權方式 | 次/秒 | p50 / p99 | 每個請求的伺服器 CPU |
  | --- | --- | --- | --- |
  | 腳本一律接受 | 364,700–372,900 | 0.16 / 0.44–0.47 ms | 約 14 µs |
  | 腳本依 `service`、`cmd` 判斷並設 `priv-lvl` | 311,800–317,900 | 0.19 / 0.50–0.51 ms | 約 17–18 µs |
  | 規則 1 條，`permit service=shell cmd=show {priv-lvl=15}` | 439,700–469,400 | 0.12–0.13 / 0.37–0.38 ms | 約 7 µs |
  | 規則 20 條，前 19 條不成立 | 407,600–429,600 | 0.13 / 0.40–0.45 ms | 約 8.4–9.0 µs |

  - 規則不必進 Lua，每個請求的 CPU 約是腳本的一半以下；多 19 條不成立的規則約多 1.5–2 µs。
  - 這時伺服器只用了約 3 顆 CPU，吞吐受 64 個連線、每條一次一個請求的限制（64 除以 p50），不是 CPU 上限。

TACACS+ over TLS（`[tacacs.tls]`）需要 rradius-enterprise。

動態 client（`[radius.dynamic_clients]`）需要 rradius-enterprise。

### 2.4 過載與重送

**RADIUS 重送偵測**（依 RFC 5080）：相同來源 IP、來源埠、Identifier 與 Request Authenticator 的封包視為重送。

- 原請求處理中：靜默丟棄。
- 原請求已回覆，且在 10 秒內：重送原本的回應，不執行腳本也不查 LDAP 或 SQL。回應是 Access-Reject 時也一樣。
- 未通過驗證的封包不影響判斷。

**計費腳本仍要自己防止重複寫入。** 設備更新 `Acct-Delay-Time` 再送時會換新的 Identifier，對伺服器來說是新封包，偵測不到。請以 `Acct-Session-Id` 加 `Acct-Status-Type` 當唯一鍵，例如建立 `UNIQUE` 索引並用 `INSERT OR IGNORE`。

**過載保護**：

- RADIUS 超過 `max_auth_inflight` 或 `max_acct_inflight` 時，封包會被靜默丟棄，不會回 Access-Reject，由設備重送。認證與計費的額度分開計算，所以計費不會佔掉認證的名額。
- 同時**執行**的計費腳本（RADIUS 與 TACACS+ 都算）不超過 CPU 核心數減 2，至少 1 個，其餘依序等待；留下的工作執行緒用來處理登入和其他請求。腳本等資料庫時會讓出名額。等待名額的時間不計入 `max_execution_ms` 與 `query_timeout_ms`。所以計費暴增時，變慢的主要是計費本身，登入不會被拖到數秒。
- 伺服器的每個記憶體配置至少佔一條快取線（macOS aarch64 是 128 位元組，其他平台是 64 位元組），避免不同 Lua VM 的資料共用一條快取線而互相拖慢：不對齊時，純運算的計費腳本在 macOS 上從每秒約 274 筆掉到 22 筆。代價是記憶體：小的配置也佔一整條快取線（以前用系統配置器時，在 macOS 量到比不對齊多最多約 3.8 倍）。記憶體向 jemalloc 要，不向系統配置器要。以前用系統配置器時，macOS 上的記憶體在穩定負載下一直增長、不會回收（每次登入都跑腳本的寬頻情境（rradius-enterprise 的範例）持續登入 40 秒長到 4.7 GB），Linux 上的封包路徑也比較慢。

  **自己編譯給 arm64 Linux 用時，** jemalloc 在建置時就固定支援的分頁大小，預設取建置機器的值。在 4 KiB 分頁的機器建置、拿到 16 或 64 KiB 分頁的核心（例如部分 ARM 伺服器和 Raspberry Pi 5 的核心）執行，伺服器會在啟動時失敗。建置時設 `JEMALLOC_SYS_WITH_LG_PAGE=16` 就能在 4、16、64 KiB 分頁上執行；專案的 Dockerfile 在 arm64 上已經這樣做。

  下表是 `rradius-bench` 的量測（見[效能量測](#9-效能量測)），兩種版本交錯各跑 3 次，伺服器和工具在同一台機器。macOS 是 14 核心的 Apple M4 Max；Linux 是同一台機器上 Docker 的 Linux（aarch64、14 個 CPU、Debian bookworm）。純運算是 `acct-cpu.toml` 併發 64，登入是 `accept.toml` 的 PAP 併發 64，記憶體是 `accept.toml` 持續登入 60 秒：

  | | macOS，系統配置器 | macOS，jemalloc | Linux，系統配置器 | Linux，jemalloc |
  | --- | --- | --- | --- | --- |
  | 純運算（筆/秒） | 232–287 | 236–276 | 227–268 | 240–260 |
  | 登入（次/秒） | 約 72,100 | 70,900–75,500 | 102,900–103,900 | 123,500–125,800 |
  | 60 秒後的記憶體（規格的「Steady load」） | 827 MB，還在增長 | 197 MB，15 秒後持平 | 172–268 MB | 271–490 MB，上下跳動 |

  Linux 上 jemalloc 會多留一些記憶體（量到 0.27–0.49 GB），閒置的記憶體依 jemalloc 預設的時間逐步還給系統，量測時停止負載 5 秒後還沒還完，但不會一直增長。表中 Linux 的 jemalloc 是以 4 KiB 分頁建置的；改用 Dockerfile 的 64 KiB 建置後，同樣的量測記憶體停在 286–291 MB，登入 126,500 次/秒，純運算 257–265 筆/秒（各 2 次）。

  `cargo test --release --test scenarios_test -- --ignored` 會跑「Steady load」：寬頻情境持續登入一分鐘，30 秒到 60 秒之間記憶體不能多出兩成加 16 MiB。它要一分鐘，也需要機器閒著，所以預設不跑。量測次數少，而且機器越跑越熱，同一列的數字會一輪比一輪低，所以只適合比較兩種版本，不能當成伺服器的能力。
- **多個接收 socket 和綁核。** Linux 上每個 port 預設有和工作執行緒一樣多的接收迴圈（`[radius] sockets`，2.3 節）。另外可以選擇把工作執行緒綁在 CPU 上：

  ```toml
  [runtime]
  pin_threads = true   # 預設 false，只在 Linux
  ```

  每個工作執行緒只在一顆 CPU 上跑，依程序可用的 CPU 輪流分配，啟動時記錄 `pinned <N> runtime worker threads to CPUs [...]`。之後才建立的執行緒（處理資料庫查詢、密碼雜湊的執行緒）恢復程序原本可用的全部 CPU，不會被綁在某個工作執行緒的 CPU 上。綁定失敗時記錄警告，照常不綁地執行。**預設關閉，** 因為和其他程序共用 CPU 時（共用主機、有 CPU 配額的容器）綁核可能更慢；只有這台機器專門跑 rradius、CPU 不受限時才建議開。其他平台設 `true` 會啟動失敗，改了要重新啟動。

  下表是 2026-11-08 在同一台 Apple M4 Max 上 Docker 的 Linux（arm64）量的，伺服器限定 CPU 0–6、`rradius-bench` 限定 7–13（`taskset`），各 2 次。這是虛擬機裡的數字，綁核特別容易受虛擬機排程影響，實體機要另外量：

  | 設定 | `accept.toml` PAP 併發 64 | 併發 1024 | 寬頻情境 PAP 併發 1024 |
  | --- | --- | --- | --- |
  | `sockets = 1`（以前） | 181,000–189,000 次/秒，p99 1.46–1.52 ms | 193,000–207,000 次/秒，每次約 1,744 個逾時 | 43,800–43,900 次/秒 |
  | 預設（7 個 socket） | 235,000–244,000 次/秒，p99 1.41–1.47 ms | 222,000–238,000 次/秒，0–9 個逾時 | 46,600 次/秒 |
  | 預設加 `pin_threads = true` | 314,000–315,000 次/秒，p99 0.61–0.62 ms | 298,000–299,000 次/秒，0–11 個逾時 | 59,500–60,700 次/秒 |

  只有一個 socket 時伺服器只用了 84 CPU 秒裡的 42–46 秒，限制在只有一個接收迴圈，不在 CPU。併發 1024 時一個 socket 每次約有 1,744 個請求沒有回應，推測是同時湧進的封包超過一個 socket 的接收緩衝區；分散到多個 socket 後幾乎沒有。macOS 不會把封包分給多個 socket（實測 1 個和 7 個都是每秒約 80,000 次），所以只能用 1。

- 腳本可用的函式（`get_attr`、`set_attr`、`sql_query`、`log` 等）每個 VM、每個腳本只建立一次。每個請求只建立自己的全域表，表裡的函式是幾個小的 Lua 函式，把這個請求的資料交給共用的函式。所以腳本出錯時的 stack traceback 會多一行 `rradius:` 開頭的位置，那是這些小函式，不是腳本的錯誤位置。以前每個請求都重建十幾個函式，在只回 `accept` 的腳本裡佔伺服器忙碌時間約 18%，現在約 3%。下表用 `rradius-bench` 和改動前交錯量測，macOS 14 核心的 Apple M4 Max，伺服器和工具在同一台：

  | 量測（各 4 次） | 改動前 | 改動後 |
  | --- | --- | --- |
  | `accept.toml` PAP 併發 64 | 66,300–68,400 次/秒，p99 1.50–2.00 ms | 67,500–80,400 次/秒，p99 1.06–1.66 ms |
  | `accept.toml` 每次登入用的伺服器 CPU | 118–124 µs | 79–96 µs |
  | 寬頻情境 PAP 每秒 30,000 次，12 秒的伺服器 CPU | 56.4–63.5 秒，p50 0.18–0.21 ms | 43.6–48.3 秒，p50 0.13–0.15 ms |
  | 寬頻情境 PAP 併發 64 | 41,900–47,300 次/秒 | 43,700–46,700 次/秒 |

  寬頻情境併發 64 時，吞吐在量測的起伏內沒有變：它每次登入都要查資料庫，量的是等待的時間，不是 CPU。只回 `accept` 的腳本，在 macOS 上剩下的時間大多花在送出回覆（同一個 UDP socket 的 `sendto` 約佔 60%）。機器越跑越熱，同一列的數字會一輪比一輪低，所以只適合比較兩種版本。

  **Linux 上送出回覆不是瓶頸。** 同一台機器上 Docker 的 Linux（aarch64、14 個 CPU、Debian bookworm）跑同樣的 `accept.toml` PAP：

  | 壓測 | 吞吐 | p50 | 伺服器每個工作執行緒的 CPU | 整台機器閒置 |
  | --- | --- | --- | --- | --- |
  | 併發 64 | 約 202,000 次/秒 | 0.25 ms | 約 25% | 70% |
  | 併發 256 | 約 227,000 次/秒 | 0.57 ms | 約 24% | 67% |
  | 併發 1024，工具 4 或 8 個執行緒 | 約 229,000–230,000 次/秒 | 0.56 ms | 約 30% | 85–87% |

  `perf` 取樣裡 `sendto` 只佔忙碌時間約 11%。macOS 的核心在同一個 socket 上一個一個送出，Linux 沒有這個情形，所以回覆仍從收到請求的那個 socket 送出。吞吐停在約 23 萬次時，沒有哪個執行緒用滿 CPU，各執行緒卻有 30–40% 的時間在等 CPU（`pidstat` 的 `%wait`），應該是 Docker Desktop 虛擬機的排程，所以這個環境量不出伺服器的上限，要在獨立的 Linux 機器上、壓測工具放另一台量。另外，認證紀錄由一個日誌執行緒寫出，每秒 23 萬次時它用了約 37% 的 CPU，負載再高約 2.5 倍就會成為限制；認證紀錄不受日誌等級影響，不需要時用 `[log]` 的 `auth = false` 關掉（見 [2.10 節](#210-記錄-log)）。
- 額度分開不代表完全隔離：認證與計費仍共用 CPU 與資料庫，計費腳本還是越輕量越好。認證與授權腳本沒有這個限制，吃 CPU 的認證腳本仍會互相影響。下表用 `rradius-bench` 量測（見[效能量測](#9-效能量測)）：本機 14 核心的 macOS、loopback，各 3 次，登入以每秒 100 次送入：

  | 計費負載 | 同時的請求 | 計費吞吐 | 登入 p50 | 登入最大值 |
  | --- | --- | --- | --- | --- |
  | 無 | 0 | 無 | 0.36–0.48 ms | 1.8–3.5 ms |
  | 每次寫一筆資料庫 | 256 | 40,900–41,500 次/秒 | 0.95–1.02 ms | 9.3–10.5 ms |
  | 每次寫一筆資料庫 | 4096 | 40,900–41,700 次/秒 | 1.13–1.24 ms | 22–43 ms |
  | 每次約 30 ms 的純運算 | 256 | 283–300 次/秒 | 0.19–0.28 ms | 486–667 ms |
  | 每次約 30 ms 的純運算 | 4096 | 280–289 次/秒 | 0.20–0.31 ms | 593–660 ms |

  寫資料庫這兩列是同時寫入合併提交（見下一項）之後重量的；以前每次寫入各自提交時，計費吞吐約 21,000 次/秒，登入 p50 0.41–0.76 ms、最大值 7.1–17 ms。現在計費多了一倍，伺服器也比較忙，所以登入稍慢。純運算這兩列在 6 次中有 5 次出現 `rate_not_met` 警告，所以登入的數字只能當參考。併發 4096 在 3 次中有 2 次丟掉 1 個請求（`server_discarded_overload`），那個請求逾時，所以這兩次的結束碼是 2。

- **同時的寫入會一起提交。** 各個請求同時等著的 `sql_execute`，只要是不回傳資料列的 INSERT、UPDATE、DELETE 或 REPLACE，就會合成一批，在一個交易、一筆 Raft 紀錄裡提交，每批最多 256 個，同時最多 4 批在路上。資料庫套用寫入只用一個執行緒，每次提交都有固定成本，所以各自提交時，計費寫入在上表的 macOS 機器上約每秒 21,000 次就到頂，合併後約 41,000 次（Linux 上高得多，見下一項），併發 256 時 p99 從 47–74 ms 降到 12–15 ms；每秒 1,000 次的輕負載下延遲和以前相同（p50 0.18 ms）。每個呼叫照樣拿到自己的受影響列數。同一批有一個陳述式失敗時，這批會對半拆開、各自再提交，直到出錯的那個單獨提交，所以只有它的呼叫失敗，次數記在 `rradius_db_split_batches_total`。這個數字一直增加，代表有陳述式常常失敗（例如重送的計費撞到 UNIQUE），可以改成 `INSERT OR IGNORE` 這類不會失敗的寫法。其他陳述式（帶 `RETURNING`、`SELECT`、`COMMIT`、`ROLLBACK` 等）照舊各自提交。叢集無法使用時，整批的呼叫都回同一個錯誤，不會再試。呼叫已經逾時的寫入，還沒交給叢集的就不寫入，因為呼叫端已經被告知失敗，計費請求也可能已經進了暫存；**已經交給叢集的，會等到叢集回覆結果才回報**（見 2.2 節〈資料庫不能用時〉）。代價是**同一批裡很慢的陳述式（例如大量 DELETE）會讓其他寫入一起等**，這種工作要分小批做（見[使用手冊第 11 章](manual/11-operations.md#清除舊資料)）。一個腳本的多個 `sql_execute` 不會在同一批，因為腳本會等每次寫入完成。
- **Linux 上的計費寫入上限。** 2026-11-08 在同一台 Apple M4 Max 上 Docker 的 Linux（arm64）量測，伺服器限定 CPU 0–6、`rradius-bench` 限定 7–13，`acct-sql.toml`（每筆 Accounting-Start 寫一次資料庫），各兩次：

  | 情況 | 併發 256（次/秒） | 併發 1024（次/秒） |
  | --- | --- | --- |
  | 資料目錄在 Docker Desktop 從 Mac 分享的資料夾 | 91,800–92,200，p99 13 ms | 88,600–89,700，p99 22 ms |
  | 資料目錄在容器自己的磁碟 | 95,700–108,200，p99 6–8 ms | 82,700–94,400，p99 15–19 ms |
  | 同上，計費腳本同時執行數從 5 改成 7（實驗版本） | 115,200 | 111,300–112,800 |

  - **每批上限和同時的批次不是限制：** 實驗版本把同時的批次從 4 改成 16、每批從 256 改成 1,024，四種組合都在每秒 74,000–88,000 次之間，沒有差別。
  - **資料放在分享資料夾時，** 唯一的 SQLite 寫入執行緒有一大段時間花在經過 FUSE 寫 WAL，所以比放在容器磁碟慢、p99 約加倍。正式部署的資料目錄應該放在本機磁碟。
  - **計費腳本的同時執行數（CPU 數減 2，見上面）在高併發時最多影響約兩成，** 改成和 VM 一樣多以後就沒有差別。這個限制是刻意留工作執行緒給登入的，所以沒有調整。
  - 到上限時 7 顆 CPU 只用了約 3 顆，也沒有哪個執行緒滿載，剩下的限制應該在叢集提交寫入的流程裡，還沒有確認。一般的負載遠低於這個上限：100 萬個上線中的 session 每 5 分鐘回報一次，大約是每秒 3,300 筆計費。
- TACACS+ 封包的序號是偶數或 255 時，連線會直接關閉且不回覆（RFC 8907 第 4.1 節）。
- TACACS+ 封包解密後格式不對（通常是共用密碼不同）時，回 ERROR 後關閉連線，記錄 `a body did not decode, so the shared secret may be wrong`，並計入 `rradius_tacacs_malformed_total`。依 RFC 8907 第 10.5.2 節，這條連線不再接受新的請求；設備會重新連線。序號不符這類和密碼無關的錯誤只回 ERROR，連線保持開啟。
- **single-connection：** 設備在連線的第一個封包設了 single-connection 旗標時，回覆也設這個旗標，表示接受在同一條連線上處理多個 session（RFC 8907 第 4.3 節），設備就不必每次重新連線。沒有設旗標的設備也可以沿用連線，rradius 不會在 session 結束時主動關閉。
- TACACS+ 超過 `max_connections` 時，新連線會被立刻關閉。設定內用戶端的閒置連線也佔額度，超過 `idle_timeout_ms` 沒有收到完整封包，或回應 10 秒寫不出去，就會關閉並釋放。腳本執行的時間不算閒置。長時間不送封包的設備要重新連線，通常不影響服務。
- 丟棄或關閉時，每個連接埠最多每秒記錄一行，例如 `RADIUS accounting overloaded: discarded 37 packets`。

這三個設定都必須大於 0。預設值是估計值。適合的值大約是「吞吐 × 設備的重送逾時」，再大也沒有幫助，因為設備已經放棄等待。

### 2.5 資料表 migration

rradius 本身沒有資料表。`sql` 處理器和腳本用到的資料表，可以寫成 SQL 檔放在 `[cluster].migrations_dir`，由伺服器在啟動時套用到整個叢集。

```toml
[cluster]
migrations_dir = "migrations"   # 以設定檔所在目錄解析
migrations_wait_ms = 60000      # 等待叢集就緒的時限，預設 60000，必須大於 0
```

```
migrations/
  1_users.sql
  2_reply.sql
```

**檔名規則：** `<編號>_<名稱>.sql`，編號從 1 開始、連續不重複。目錄裡不能有其他檔案，也不能有子目錄。

**套用時機：** 開始處理 RADIUS／TACACS+ 請求之前。流程是先檢查目錄和檔名（這時還沒有啟動節點，所以錯誤會立刻顯示），等待叢集就緒，套用尚未套用的檔案，再等本機副本追上。所以腳本和處理器可以假設資料表已存在。沒有設定 `migrations_dir` 時，不會執行這些步驟，也不會多等待；不過不論有沒有設定，節點第一次啟動時都要等到加入叢集才會開始服務。

**重啟：** 已套用的檔案會跳過，只執行新增的檔案。紀錄存放在 `_migrations` 表，每個檔案一筆，包含內容的 SHA-256。

**不能修改已套用的檔案：** 修改內容或改名都會讓啟動失敗。要變更結構時，請新增一個檔案。不支援回滾。

**每個檔案一個交易：** 檔案出錯時，那個檔案的變更全部復原，也不會留下紀錄；它之前的檔案維持已套用。修正該檔案後重啟即可。

**多節點：**

- 每個節點的目錄內容必須一致。新增檔案時，**先更新所有節點的檔案，再逐台重啟**。
- 節點的檔案比已套用的少時（還沒拿到新檔案），會記錄一行警告並照常啟動。
- 不要讓內容不同的目錄同時啟動。hiqlite 遇到不一致會在寫入執行緒中 panic，該節點的寫入會全部失敗。rradius 啟動時會先檢查，但同時啟動時無法完全排除。

**狀態碼：**

| 狀態碼 | 原因 |
| --- | --- |
| 2 | 目錄不存在、檔名或編號錯誤、已套用的檔案被修改或改名、檔案執行失敗；訊息會指出檔案，只有執行失敗後又讀不到 `_migrations` 時無法指出，這時訊息會說明並保留 SQL 錯誤 |
| 1 | 叢集在 `migrations_wait_ms` 內沒有就緒（例如多數節點尚未啟動）、套用時遇到叢集錯誤，或本機副本沒有追上 |

`migrations_wait_ms` 分別限制三段時間：連上叢集、套用檔案、本機副本追上，每段各自計時。**它必須大於最長的 SQL 檔執行時間**：不只是本節點，其他節點在跑慢的檔案時，本節點一樣要等；重新加入的節點也要花差不多的時間重播。逾時會以狀態碼 1 結束，已送出的檔案仍會在叢集中執行完，調高後重啟即可。大型資料變更前，請先調高這個值。

已經手動建立的資料表，可以放進 `1_*.sql`，用 `CREATE TABLE IF NOT EXISTS` 寫法納入管理。

### 2.6 RADIUS 安全設定

```toml
[radius]
require_message_authenticator = "yes"  # 預設 yes
limit_proxy_state = "yes"              # 預設 yes
status_server = true                   # 預設 true

[[radius.clients]]
address = "10.0.0.9"
secret_file = "old-switch.secret"
require_message_authenticator = "no"   # 覆寫 [radius] 的值
```

**短共用密碼：** RADIUS 或 TACACS+ client的密碼少於 16 octets 時，啟動、SIGHUP 和 `--check` 都會記錄警告（`has a shared secret shorter than 16 octets`），但不會拒絕啟動。16 是 RFC 8907 第 10.5.1 節和 RFC 2865 建議的下限；以前的門檻是 12 octets，所以 13–15 octets 的密碼升級後開始出現警告。建議至少 24 個隨機 octets。TACACS+ 的封包內容完全靠這個密碼保護。

**升級注意：預設會丟棄不帶 Message-Authenticator 的 Access-Request。** 不會送這個屬性的舊設備，升級後要設定 `no` 或 `auto`，否則認證會失敗，記錄中會出現 `lacking a Message-Authenticator`。

| 設定 | `yes` | `no` | `auto` |
| --- | --- | --- | --- |
| `require_message_authenticator` | 沒帶就丟棄 | 允許沒帶，再看 `limit_proxy_state` | 由第一個請求決定：有帶就變 `yes`，沒帶就變 `no` |
| `limit_proxy_state` | 帶 Proxy-State 但沒帶 Message-Authenticator 就丟棄 | 允許，**有 BlastRADIUS 風險** | 由第一個請求決定：帶 Proxy-State 但沒帶 Message-Authenticator 就變 `no`，否則變 `yes` |

- `limit_proxy_state` 只在不要求 Message-Authenticator 時才有作用。
- `auto` 只能用在單一主機的 client，用在網段時啟動失敗。學到的值在重啟後重新學習。
- `auto` 的第一個封包如果沒帶 Message-Authenticator，來源就無法驗證，攻擊者可以偽造這個封包把 client 降級。可以的話請明確設定 `yes`。
- 這些設定是緩解措施。設成 `no` 或 `auto` 的 client 仍有風險，根本解法是 RadSec（RADIUS over TLS，需要 rradius-enterprise）；用不了 RadSec 的設備跨不受信任的網路時，請用 VPN。

**回應：** Access-Accept 和 Access-Reject 一律帶 Message-Authenticator，放在第一個屬性。Access 和 Accounting 的回應會依序複製請求中的 Proxy-State。回應超過 4096 octets 時改回 Access-Reject 並記錄（EAP 的 Access-Challenge 也一樣，會結束這段對話），連 Access-Reject 都放不下時不回應。

**Status-Server**（RFC 5997）：供負載平衡器做健康檢查，不必建立測試帳號。不會執行腳本，也不經過重送偵測。認證埠回 Access-Accept，計費埠回 Accounting-Response。請求一定要帶有效的 Message-Authenticator，不受上面的設定影響。達到同時處理上限時不回應。

### 2.7 RadSec（RADIUS over TLS）

RadSec（`[radius.tls]`）需要 rradius-enterprise。

### 2.8 統計 `[metrics]`

```toml
[metrics]
bind = "127.0.0.1"  # 預設
port = 9812         # 預設
```

`GET /metrics` 回傳 Prometheus 文字格式。沒有這個區段時不開這個埠。**這個埠沒有認證也沒有 TLS**，請只綁定內部位址。

| 項目 | 說明 |
| --- | --- |
| `rradius_radius_packets_total{kind="auth\|acct"}` | 通過檢查與重送偵測、實際處理的請求 |
| `rradius_radius_responses_total{code="access_accept\|access_reject\|access_challenge\|accounting_response"}` | 送出的回應，不含重送快取與 Status-Server |
| `rradius_radius_client_responses_total{client="...",code="access_accept\|access_reject\|access_challenge\|accounting_response"}` | 送給每個 client 的回應，口徑和上一行相同。設定裡的每個 client 從 0 開始；重新載入時新增新的，移除不在設定裡的。一個 client 四個序列，client 很多時要留意 |
| `rradius_radius_status_server_total` | 回應的 Status-Server |
| `rradius_radius_duplicates_total` | 由重送快取回覆，或因為還在處理而丟棄的重送 |
| `rradius_radius_discarded_total{reason="overload\|unknown_client\|invalid\|database\|no_leader"}` | 沒有回應的封包；`invalid` 包含格式錯誤、驗證失敗和不符合 Message-Authenticator 政策；`database` 是叢集資料庫不能用而沒有決定的登入，`no_leader` 是 `stop_without_leader_ms` 讓它不回應的（2.2 節〈資料庫不能用時〉） |
| `rradius_cluster_leader`、`rradius_cluster_is_leader`、`rradius_cluster_applied_index`、`rradius_cluster_seconds_without_leader` | 這個節點看到的叢集狀態，見 2.2 節〈叢集狀態指標〉 |
| `rradius_tacacs_connections_total{result="accepted\|refused"}` | TACACS+ 連線 |
| `rradius_tacacs_connections_open` | 目前的 TACACS+ 連線數 |
| `rradius_tacacs_requests_total{type="authen\|author\|acct"}` | TACACS+ 請求 |
| `rradius_tacacs_malformed_total` | 解密後欄位不合 RFC 8907 而回 ERROR 的 START、授權和計費請求。持續增加通常是密碼設錯 |
| `rradius_log_dropped_lines_total` | 輸出跟不上或寫入失敗而遺失的記錄行數（2.10 節） |
| `rradius_db_batched_writes_total` | 和其他寫入在同一個交易裡提交的 `sql_execute`（2.4 節） |
| `rradius_db_split_batches_total` | 因為其中一個陳述式失敗而拆開的寫入交易（2.4 節） |
| `rradius_script_http_requests_total{result="ok\|timeout\|error\|denied\|too_large\|busy"}` | 腳本的 `http_request`（4.11 節） |
| `rradius_eap_sessions_open` | 進行中的 EAP 對話（2.12 節） |
| `rradius_eap_sessions_total{method="tls\|peap\|ttls",result="accept\|reject\|timeout\|overload\|replaced\|evicted"}` | 結束的 EAP 對話。已計入 `reject` 的對話之後逾時或被踢掉，不再計數；`overload` 是 `max_sessions_per_device` 滿了而沒有回應，`replaced` 是同一台設備重新開始而取代的舊對話，`evicted` 是表滿時踢掉的對話，先踢還停在 EAP Identity 的 |
| `rradius_acct_spool_written_total` | 寫入暫存的計費請求（2.11 節） |
| `rradius_acct_spool_replayed_total` | 重放時腳本接受的請求 |
| `rradius_acct_spool_rejected_total` | 重放時再跑一次結果也相同，移到 `rejected.log` 的請求 |
| `rradius_acct_spool_full_total` | 暫存已滿或寫入失敗，沒有回覆的請求 |
| `rradius_acct_spool_corrupt_total` | 損壞而改名保留的暫存檔 |
| `rradius_acct_spool_bytes` | 等待重放的位元組數；持續大於 0 代表資料庫的資料落後 |
| `rradius_reload_total{result="ok\|error"}` | 重新載入的結果 |
| `rradius_reload_last_success_timestamp_seconds` | 上次成功重新載入的時間，還沒有時是 0 |

數值是每個節點從啟動開始計算，重新載入不會歸零，叢集的總和由 Prometheus 彙總。例外是 `rradius_radius_client_responses_total`：重新載入移除的 client 序列會消失，之後再加回來時從 0 開始。

### 2.9 重新載入（SIGHUP）

```bash
kill -HUP <pid>
docker compose kill -s HUP rradius
```

收到 SIGHUP 時重讀設定檔（包含 `secret_file`）並套用，結果寫一行到記錄：`configuration reloaded from ...` 或 `reload failed, keeping the running configuration: ...`。

- **會套用：** 最上層腳本、認證串接與其腳本、`[lua]` 的時限、RADIUS 與 TACACS+ 的 client 名單與設定（包含 `[tacacs.authorize_groups]` 與 `[tacacs].enable`）、`[radius.eap]`（只影響新的對話）與 `[lua.http]`。
- **要重啟才會套用：** `dictionary`（字典檔的內容也不會重讀）、`[lua].max_memory_mb`、`[tacacs].idle_timeout_ms`、`[cluster]`（包含 migration）、`[log]`（`[log.trace]` 除外）、所有監聽位址與埠、`max_auth_inflight`、`max_acct_inflight`、`max_connections`、`max_body_len`、`status_server`、`idle_timeout_ms`、`[metrics]`、`[radius.accounting_spool]`，以及新增或移除 `[radius]`、`[tacacs]`。這些設定有變動時，記錄會列出名稱，其餘照常套用。
- **全部成功才會套用。** 檢查和啟動時相同，任何一項有誤就全部維持原狀，程序繼續服務；修正後再送一次 SIGHUP 即可。
- 進行中的請求用舊內容做完，不會中斷。已開啟的 TACACS+ 連線不會因此斷線，但下一個封包會依新名單處理：client 被移除時關閉連線，密碼更換後要用新密碼。
- `auto` 學到的值和重送快取會保留。每次重新載入都會重建 LDAP 連線池。
- 每個節點各自重新載入，要更新整個叢集時，請對每個節點更新檔案並送出信號。

### 2.10 記錄 `[log]`

```toml
[log]
level = "info"   # error、warn、info（預設）、debug
format = "text"  # text（預設）、json
auth = true      # 預設 true，寫認證紀錄
```

**升級注意：所有訊息都寫到 stderr，每行都有時間和等級。** 以前寫到 stdout 的啟動訊息也移到 stderr，讀取 stdout 或比對整行內容的外部腳本要跟著修改。

| 格式 | 範例 |
| --- | --- |
| `text` | `2026-11-05T01:02:03.000000Z  INFO rradius_server::radius: RADIUS Server listening on ...`，沒有色碼 |
| `json` | 每行一個物件，有 `timestamp`、`level`、`target`、`message`，其他欄位在同一層 |

- **`level`** 只影響 rradius 自己的訊息。其他函式庫固定只留 `warn` 以上，叢集正常啟動和關閉時也會有少量這類訊息，例如 `No metadata exists inside the DB yet`；節點加入前可能有 `Raft node has not been initialized`，加入後就會停止。三節點啟動 25 秒後關閉，實測共 27 行，其中 13 行是 `hiqlite` 的 `ERROR`。
- **設定載入前的錯誤和啟動失敗的致命錯誤** 直接寫到 stderr，沒有時間和等級前綴，方便在程序結束前一定寫出來。
- 外部來的值如果含有換行或控制字元，`text` 格式會跳脫，`json` 格式由 JSON 跳脫，所以一筆記錄一定只佔一行。腳本的 `log()` 也一樣。

**追蹤單一使用者或 client：** 不必開全域 `debug`，也不必重啟。

```toml
[log.trace]
users = ["alice"]    # 比對請求的 User-Name，完全相同才算
clients = ["nas1"]   # 比對 client 名稱，要是 [[radius.clients]] 裡有的名稱
minutes = 30         # 必填，1 到 1440
```

```text
2026-11-05T01:02:03.000000Z  INFO rradius_trace: request client="nas1" source=10.0.0.1 user="alice" code=1 attributes="User-Name = alice, User-Password = <hidden>"
2026-11-05T01:02:03.000000Z  INFO rradius_trace: response client="nas1" source=10.0.0.1 user="alice" code=2 attributes="Filter-Id = staff"
```

- **需要 `[radius]`。** `clients` 打錯名稱時啟動和重新載入都會失敗。
- **到期自動停止，** 從載入或重新載入設定開始算。留在設定檔裡的話，每次 SIGHUP 都會重新計時，用完請刪掉。每個節點各自計時，要查的請求可能在別的節點，請對有關的節點都設定。
- **SIGHUP 就會套用，** 不受 `[log]` 其他設定要重啟的限制，也不受 `level` 影響。
- **會寫進個人資料，** 保存多久、誰能讀取由部署方負責。字典標了 `encrypt` 或 `secret` 的屬性、名稱含 `pass`、`secret`、`key`、`token`、`chap`、`arap`、`-pw`、`-cpw`、`digest`、`signature`、`psk`、`pmk`、`credential`（不分大小寫）的屬性，以及標準屬性 2、3、24、60、69、70、79、80（`User-Password`、`CHAP-Password`、`State`、`CHAP-Challenge`、`Tunnel-Password`、`ARAP-Password`、`EAP-Message`、`Message-Authenticator`，不論字典怎麼命名）顯示 `<hidden>`。**只看屬性，不看值的內容：** 自訂屬性用了別的名稱放秘密，或把秘密放在值裡（例如 `Cisco-AVPair = "vpdn:l2tp-tunnel-password=..."`）都擋不到，腳本會送這種值時，不要對那個使用者開追蹤。使用者把密碼打進帳號欄位時，它會出現在 `user`。
- **範圍：** 只有 RADIUS，不含 TACACS+。重送快取回覆的封包、驗證失敗的封包和 Status-Server 不會寫。比對的是外層名稱，所以 PEAP、TTLS 的匿名名稱要改用 `clients`，或列出外層實際用的名稱。文字值最多顯示 128 個字元，其他值最多 64 octets 的十六進位。 字典沒有的屬性不會出現。兩行都在處理完才寫，所以時間相同，也不代表處理花了多久；腳本卡住時，連請求那行都要等它結束才會出現。
- **流量大時用 `clients` 要小心：** 每個請求兩行，輸出跟不上時會被丟棄，和其他記錄一樣。

**認證紀錄：** 每次認證決定寫一筆，不受 `level` 影響，`auth = false` 時不寫。

```text
2026-11-05T01:02:03.000000Z  INFO rradius_auth: authentication protocol="radius" client="nas1" source=10.0.0.1 user="Alice@Corp" account="alice" chain="vpn" handler="sql" decision="accept"
```

| 欄位 | 內容 |
| --- | --- |
| `protocol` | `radius` 或 `tacacs` |
| `client` | 比對到的 client 名稱 |
| `source` | 來源 IP 位址 |
| `user` | 收到的名稱，沒有時是空字串 |
| `account` | 查帳號用的名稱（3.6 節）。沒有名稱，或名稱被串接拒絕時是空字串 |
| `chain` | 具名串接的名稱、`default`，沒有串接時的 `script`，或 EAP 對話的 `eap` |
| `handler` | 做出決定的處理器：`sql`、`lua`、`ldap`、`script`，EAP 對話是 `eap-tls`、`eap-peap`、`eap-ttls`；全部略過時是 `none`；`all_accept` 沒有人比對過密碼而拒絕時是 `no_password_check`；名稱被拒絕時是 `normalization`。`all_accept` 接受時是最後一個接受的處理器 |
| `decision` | 設備實際收到的結果：`accept`、`reject`、`challenge`。回應送不出去而改成拒絕時，這裡寫 `reject` |

- **只有做出決定的請求才有紀錄。** 重送快取的回覆、過載丟棄、驗證失敗和未知 client 的封包不會有，請看 2.8 節的指標。計費和 TACACS+ 授權也不會有。
- **不會記錄密碼、CHAP 或 MS-CHAPv2 回應、TACACS+ 的 `data`。** 但使用者把密碼打進帳號欄位時，它會出現在 `user`，伺服器無法判斷。
- **紀錄含有帳號等個人資料。** 保存多久、誰能讀取由部署方負責；不需要時可以設 `auth = false`。

**不是可靠的稽核來源：** 記錄由背景執行緒寫出，最多緩衝 128000 行。輸出跟不上時會直接丟棄，寫入失敗（例如接收的程序結束了或磁碟滿了）時也會遺失，兩者都計入 `rradius_log_dropped_lines_total`，不會拖慢認證，所以要留意這個指標。SIGTERM 和 SIGINT 會在結束前等緩衝寫出，最多 5 秒，超過時剩下的會遺失，也不會計數；SIGKILL 或當機時，緩衝中的內容同樣會遺失。

**對吞吐的影響：** 在 Apple M4 Max（14 核心）的同一台機器上，用 release 建置和 `rradius-bench`（`accept.toml`、PAP、併發 64、10 秒），stderr 導到檔案，認證紀錄開關各跑兩次：

| `auth` | 第一次（次/秒） | 第二次（次/秒） |
| --- | --- | --- |
| `true` | 74732 | 74004 |
| `false` | 74023 | 74569 |

兩者的差異小於同設定兩次之間的差異，這次量不出影響。紀錄約每筆 190 octets，這個速率下每秒寫出約 14 MB。工具和伺服器在同一台機器，stderr 接到比本機檔案慢的地方時，可能會開始丟棄。

**更高的速率：** 2026-11-08 在同一台機器上 Docker 的 Linux（arm64）量測，伺服器限定 CPU 0–6 並開綁核（2.4 節）、`rradius-bench` 限定 7–13，`accept.toml` 的 PAP，各兩次：

| 認證紀錄 | stderr 接到 | 併發 64（次/秒） | 併發 1024（次/秒） | 寫出紀錄的執行緒用掉的 CPU | 丟棄的紀錄 |
| --- | --- | --- | --- | --- | --- |
| `auth = true` | 檔案 | 316,200–316,300 | 305,500–306,500 | 72% | 0 |
| `auth = true` | 管線（像交給 journald） | 285,400–316,900 | 289,500–303,300 | 86–87% | 0 |
| `auth = false` | 檔案 | 344,000–344,500 | 313,400–325,800 | 0 | — |

- **紀錄由一條執行緒寫出。** 這個速率下開紀錄大約慢 9%，約等於這條執行緒在這 7 顆 CPU 裡用掉的份量。
- **接到管線時它已經用到約 87% 的 CPU，** 這台機器上再快一些（大約每秒 34 萬次以上）就會滿載。滿載時新的紀錄直接丟掉，伺服器照常運作，丟掉多少只看得到 `rradius_log_dropped_lines_total`（2.8 節）。登入量接近這個範圍時請監看這個指標；不需要每筆紀錄時設 `auth = false`。
- 一般的部署很少有單台每秒數十萬次的登入。

### 2.11 計費暫存 `[radius.accounting_spool]`

叢集失去多數節點時，設備重送幾次就會放棄，那些計費就遺失了。設定這個區段後，資料庫失敗的計費請求會先寫到本機檔案、落盤後才回覆，之後由背景工作交回腳本重放。沒有這個區段時行為完全不變。

```toml
[radius.accounting_spool]
dir = "spool"              # 必填，相對於設定檔
max_bytes = 1073741824     # 預設 1 GiB
replay_interval_ms = 5000  # 預設，重放遇到資料庫失敗時，隔多久再試
replay_window = 1          # 預設，一次重放幾筆（1–1024）；見下方〈同時重放〉
```

- **只暫存資料庫造成的失敗。** 這個請求的 `sql_query` 或 `sql_execute` 因為逾時或叢集錯誤而失敗時才會暫存，不論腳本最後回傳什麼：腳本用 `pcall` 接住、之後回 `'accept'` 也暫存，因為那筆可能根本沒寫進去，重放時再判斷一次。腳本自己的 bug、SQL 語法錯誤、回傳 `'reject'` 都不暫存，和原本一樣不回覆，錯誤不會被藏起來。
- **至少一次。** 程序在腳本完成和更新進度之間中斷時，同一筆會再跑一次，所以腳本要能重複執行（見 4.4 節）。另一個來源是不合併提交的寫入（例如帶 `RETURNING`）：它在逾時之後才完成時，這筆已經進了暫存，重放時再跑一次；合併提交的寫入交出後會等到結果，它本身不會這樣；但它等得比時限久時，同一個請求之後的資料庫呼叫會立刻逾時，這筆計費仍可能因此進暫存、重放時整支腳本再跑一次（2.2 節〈資料庫不能用時〉）。所以有多個資料庫呼叫的計費腳本一樣要能重複執行。斷電時可能重跑更多：進度在累計 64 筆以上完成時和換 segment 時才落盤，所以最多再跑 64 筆；`replay_window` 大於 1 時，一批結算完才更新進度，最多是 63 筆加一批。`position` 檔案損壞（例如斷電後變成空檔）時，會記錄一行 ERROR，從最舊的 segment 開頭重放，最多 16 MiB。`rradius-server/examples/` 的計費腳本都能重複執行，有測試；`disconnect-disabled.lua` 遇到重放不會送中斷，因為那已是舊的回報。
- **重放。** 預設依寫入順序一次一筆（`replay_window` 大於 1 時見下一項），用當時的腳本和 client 設定。全域變數 `replayed` 是 `true`，`Acct-Delay-Time` 已加上暫存的時間。要計算時間的腳本應該用它，否則舊的回報看起來像剛發生。資料庫再次失敗時，隔 `replay_interval_ms` 重試同一筆，後面的請求繼續等。
- **同時重放（`replay_window`）。** 預設 1。設成更大的值時，一次讀出最多這麼多筆（不跨暫存檔）同時交給腳本，依寫入順序結算到第一筆要重試的為止，之後先單獨重試那一筆、有結果才恢復整批；排在它後面、腳本已經接受的會再跑一次（本來就是「至少一次」）。同時執行的腳本照常受計費腳本的名額限制，會和即時計費共用。
  - **預設不開的原因：** 同一批裡同一個 session 的 Start、Interim-Update、Stop 可能同時執行或順序顛倒。腳本只用 `INSERT OR IGNORE` 記下每筆、或依 `Acct-Status-Type` 分別處理而不依賴先後時，可以放心打開；腳本靠先後維護 session 狀態（例如 Stop 時更新 Start 新增的那一列）時，維持 1。
  - **建議從 8 開始。** 2026-11-09 在 OrbStack 的 Linux（arm64）量測：伺服器 7 顆 CPU，計費腳本以 `INSERT OR IGNORE` 寫一列，`query_timeout_ms = 1` 送 `acct-start` 5 秒累積約 4.3–4.6 萬筆，再恢復正常逾時量重放到 `rradius_acct_spool_bytes` 為 0 的時間，各兩次：

    | `replay_window` | 每秒重放筆數 |
    | --- | --- |
    | 1 | 4,600–4,700 |
    | 8 | 16,300–17,200 |
    | 32 | 14,900–25,700 |

    一次一筆時每筆約 0.2 ms，大多花在等資料庫寫入完成（叢集提交），不是暫存本身。視窗大時全部不到 3 秒就重放完，以 0.1 秒的間隔計時，32 的兩次差很多，只能看出比 8 快；真正累積更多筆時請在實際環境量。
- **什麼時候放棄一筆。** 只有再跑一次結果也相同時：client 已不存在、腳本正常結束但沒有回傳 `'accept'`、腳本因 SQL 語法、參數錯誤或違反約束而結束。這些會移到 `rejected.log` 並記錄。腳本接住 SQL 錯誤後又因為別的原因出錯，算其他失敗；把這個請求接住過的 SQL 錯誤再丟出去則算放棄。外鍵的父列之後才會到的情況也會放棄，所以腳本要自己處理順序，或用 `INSERT ... ON CONFLICT` 讓重複的請求不出錯。檔案不會自動清理，也不會重放，請自行查看。
- **其他失敗都會重試，佇列會卡住。** 資料庫不可用、Lua 錯誤、超過 `max_execution_ms`、腳本呼叫的 HTTP API 出錯而丟出錯誤，還有腳本接住資料庫不可用或原因不明的失敗卻回傳 `'accept'`，都會隔 `replay_interval_ms` 重試同一筆，因為已回覆的請求設備不會再送。腳本一直出錯時，`rradius_acct_spool_bytes` 會持續大於 0，修好腳本後自動恢復。腳本接住 SQL 語法這類錯誤後回傳 `'accept'` 算完成，因為再跑一次結果相同。
- **啟動時的檔案結尾。** 最後一筆不完整時會截掉，因為它沒有回覆過。長度欄損壞、但值還像一筆記錄時看起來一模一樣，而那可能是回覆過的請求，所以截掉的內容會先附加到暫存目錄的 `truncated.log`，並記錄一行 WARN。這個檔案不會自動清理，也不會重放，通常只有一筆寫到一半的內容，請自行查看。`truncated.log` 寫不進去時（例如磁碟滿了）不會截掉，啟動照常，新請求寫到新檔案，舊檔案之後整個改名為 `.corrupt`，並記錄一行 ERROR。結尾全是零（斷電常見）直接移除，不會留下；零的後面還有內容就算損壞，整個檔案改名為 `.corrupt`。
- **滿了或磁碟失敗。** 不回覆、記錄 `accounting spool ...`、計入 `rradius_acct_spool_full_total`，由設備重送，和沒有這個區段時相同。寫到一半失敗的內容會被截掉，不影響後面的請求。每筆大小見下方量測，可以換算撐多久。
- **查詢會延遲。** 回覆之後資料才進資料庫，依賴它的政策（配額、同時上線）在恢復前都看不到這些計費。**先後也可能顛倒：** 叢集恢復後，新的計費直接寫入，暫存裡較舊的還在等重放，所以同一個 session 的 Stop 可能比 Start 先進資料庫；腳本不要假設先後，例如用 `Acct-Status-Type` 和時間判斷，而不是以最後一筆為準。`rradius_acct_spool_bytes` 大於 0 就代表有落後，可以用它告警。
- **節點損毀會遺失。** 各節點只重放自己的檔案，沒有跨節點接手。磁碟永久損毀時，已確認但還沒重放的計費就遺失了，請放在持久儲存。多個節點同時恢復時可能一起寫入，由叢集處理。
- **個人資料。** 檔案是原始封包，含 `User-Name`、位址等個人資料，保存期間由資料庫中斷多久決定。目錄和檔案權限是 0700、0600，備份和磁碟丟棄時要一併考慮。
- **檔案。** `segment-<n>.log` 是資料，`position` 是重放進度，`.corrupt` 結尾是損壞而改名保留的檔案。尾端不完整的一筆是寫入中斷造成的，那筆還沒回覆，會被截斷並記錄。其他損壞會把檔案完整改名保留，之後的請求寫到新檔案。
- **回覆時間由 `query_timeout_ms` 決定。** 叢集剛失去多數節點時，寫入要等到逾時才算失敗（一個請求的所有呼叫合計最多 `query_timeout_ms`），這段時間設備收不到回覆；沒有 leader 超過 5 秒後寫入立刻失敗。預設的 5 秒比許多設備的重送間隔長，重送的封包會被視為處理中而丟棄，請考慮調低。同時處理的上限是 `max_acct_inflight`，所以吞吐上限約為「上限 ÷ 逾時」。
- **其他。** TACACS+ 不適用。SIGTERM 時停止重放，資料留在磁碟，下次啟動繼續。變更這個區段要重啟（2.9 節）。

**量測：** Apple M4 Max（14 核心），本機 SSD，release 建置與 `rradius-bench`，`acct-start`，10 秒。每筆約 106 octets（這個工具的封包，含每筆的表頭與 client 名稱；實際大小隨設備送的屬性增加），1 GiB 約 1000 萬筆。

| 情況 | 併發 | 吞吐（次/秒） | p50 |
| --- | --- | --- | --- |
| 沒有這個區段，第一、二次 | 64 | 74404、75578 | 0.8 ms |
| 有這個區段，資料庫正常，第一、二次 | 64 | 74593、74430 | 0.8 ms |
| 模擬中斷（每次查詢都以 1 ms 逾時），第一、二次 | 64 | 7979、7974 | 8 ms |
| 模擬中斷 | 256 | 16894 | 15 ms |
| 三節點失去兩個，`query_timeout_ms = 500` | 64 | 128 | 509 ms |
| 三節點失去兩個，`query_timeout_ms = 100` | 64 | 588 | 108 ms |
| 三節點失去兩個，`query_timeout_ms = 100` | 256 | 2260 | 113 ms |

資料庫正常時，有沒有這個區段的差異小於同設定兩次之間的差異，這次量不出影響。模擬中斷的數字是寫入暫存本身的上限，同時有多筆時併成一次 fsync，所以併發越高越快。實際中斷時限制來自逾時等待，吞吐約等於「併發 ÷ 逾時」，和磁碟無關。磁碟不同時 fsync 的數字也會不同，請在實際環境重量。

### 2.12 EAP `[radius.eap]`

WPA2／WPA3-Enterprise 和有線 802.1X 要用 EAP。支援 EAP-TLS（用戶端憑證）、PEAP-MSCHAPv2（帳號密碼）和 EAP-TTLS（帳號密碼，內層 PAP 或 MS-CHAPv2）。沒有這個區段時，`EAP-Message` 和其他屬性一樣交給腳本。**這個功能還沒有在實體無線基地台、交換器或 Windows、iOS、Android 上測過，** 只用 wpa_supplicant 的 `eapol_test` 驗證過（見第 7 節）。

```toml
[radius.eap]
cert_file = "eap-server.pem"   # 必填，用戶端看到的伺服器憑證
key_file = "eap-server.key"    # 必填
client_ca_file = "eap-ca.pem"  # methods 有 tls 時必填（沒有 tls 時設定會啟動失敗），簽發用戶端憑證的 CA
crl_file = "eap-ca.crl"        # 選填，只用於 tls（沒有 tls 時設定會啟動失敗），PEM，列出撤銷的憑證
methods = ["tls"]              # 預設；可以列 "tls"、"peap"、"ttls"
fragment_size = 1000           # 預設，256 到 1400
session_timeout_ms = 30000     # 預設
max_sessions = 4096            # 預設
max_sessions_per_client = 1024 # 預設
max_sessions_per_device = 4    # 預設
match_identity = true          # 預設
```

EAP-AKA'（`methods` 的 `aka-prime`）、session resumption（`resumption_ms`、`shared_tickets`）和 OCSP（`ocsp`）需要 rradius-enterprise。

- **憑證通過不等於可以登入。** 握手成功後，名稱會交給這個 client 的認證串接，沒有串接的 client 交給最上層腳本。拒絕就是 Access-Reject，所以停用帳號不必等 CRL，也能依使用者回覆 VLAN 等屬性。
- **串接怎麼決定。** 憑證通過後不比對密碼，所以處理器要另外判斷帳號能不能用。`all_accept` 裡有 `sql` 或 `ldap` 時，要它們其中一個接受才算通過（3.2 節），只有後面的腳本接受會被拒絕；`ldap` 沒設 `certificate_filter` 時不算接受。要只靠腳本判斷憑證，就不要把 `sql`、`ldap` 放進同一條串接，或改用 `first_match`：
  - **`sql`：** 預設用 `user_query`，第一欄是 NULL、空值或只有 `{clear}`、`{nthash}` 這類前綴就拒絕，和密碼登入一樣，所以清空密碼也能停用憑證登入。只用憑證、不存密碼時，設 `certificate_query`，例如 `SELECT 1 FROM users WHERE username = ? AND enabled = 1`，查得到就接受。`default_user` 不適用。
  - **`ldap`：** 只靠搜尋看不出帳號被鎖定，所以要設 `certificate_filter` 才接受憑證登入，並把停用條件寫進去，例如 OpenLDAP 的 `(&(uid={user})(!(pwdAccountLockedTime=*)))`、AD 的 `(&(sAMAccountName={user})(!(userAccountControl:1.2.840.113556.1.4.803:=2)))`。這兩個例子依目錄的設定而定，請先在自己的目錄上確認。查得到且符合 `required_groups` 就接受，不做 bind。沒有設定時略過，交給下一個處理器，也不會連線到目錄。
  - **`lua`：** 由腳本決定。腳本可以用全域變數 `eap_method`（`"tls"`）、`tls_subject`（憑證主體）和 `tls_subject_attrs`（依欄位分開的表）。**要判斷憑證的欄位時用 `tls_subject_attrs`，不要對 `tls_subject` 用 `find`：** CN 是 `x, OU=admins` 的憑證，文字是 `CN=x\, OU=admins`，仍然包含 `OU=admins`。
- **記憶體裡的金鑰。** MSK、NT hash 和它導出的金鑰、PAP 密碼在 rradius 自己的程式裡用完就覆寫，`Debug` 也不會印出密碼。這只是縮小範圍：TLS、雜湊和 LDAP 函式庫內部的複本、腳本拿到的 `User-Password`、從資料庫讀出的密碼都清不到。要防 core dump 洩漏，請在系統層關閉 core dump。
- **名稱怎麼來。** 憑證的名稱是它的 DNS SAN 和 email SAN；兩者都沒有時才用 CN。SAN 讀不出來，或裡面有不是文字的名稱時，這張憑證會被拒絕，不會改用 CN。當名稱的 CN 只接受 UTF8String、PrintableString、IA5String 和 NumericString，其他類型即使 `tls_subject` 讀得到，也不會拿來登入。EAP Identity 要符合其中一個，不分大小寫，不符合就拒絕。用戶端會把 Identity 填成匿名值時，設 `match_identity = false`，改用第一個名稱。名稱一律採憑證裡的拼法，腳本看到的 `User-Name` 就是它。腳本或串接沒有回 `User-Name` 時，Access-Accept 會帶上這個名稱，讓設備的記錄和計費用它，而不是用戶端自己填的值。
- **憑證登入不去掉 realm。** 串接設了 `strip_realm` 時，名稱被改變的不只是大小寫就拒絕，例如 `alice@partner.example` 不會以 `alice` 登入，因為憑證證明的是完整名稱。資料庫要存完整名稱，或讓這些 client 用沒有設 `strip_realm` 的串接。`lowercase` 只放行 ASCII 字母的大小寫：非 ASCII 字元被改變時（例如 `É` 或 Kelvin 符號）也會拒絕，因為 Kelvin 符號轉小寫就是另一個帳號的 `k`。
- **同一次認證要留在同一個節點。** 對話的 TLS 狀態只在收到它的節點，無法跨節點接手。一般的主備設定沒問題；切換節點時，用戶端重新認證即可。**放在負載平衡器後面時，要依來源固定節點，** 否則每輪落到不同節點就會失敗，因為 `State` 在別的節點找不到。
- **一律需要 Message-Authenticator。** 帶 `EAP-Message` 而沒有這個屬性的封包會被丟棄，不看 client 的 `require_message_authenticator`（RFC 3579）。
- **對話綁定設備。** 每輪要符合 `State`、client 和 `Calling-Station-Id`，別的設備不能接手。
- **對話有總上限。** 一段對話最多 256 個請求，存活時間最多是 `session_timeout_ms` 的四倍（預設 2 分鐘），再忙也一樣。所以 SIGHUP 更新 CRL 後，用舊設定進行中的對話最晚在這段時間內結束。沒有資料卻帶 More 旗標的分段會結束對話。
- **未認證的負擔有上限。** 握手前任何人都能開對話，所以 `max_sessions` 和 `max_sessions_per_client` 滿了，就踢掉該範圍內的一個對話，計入 `result="evicted"`：先從還停在 EAP Identity 的對話裡選，沒有才從全部裡選，都是選最久沒收到請求的。只送一個封包就能占位置的會先被踢，所以用隨機 MAC 洗滿時，已經回過一輪的使用者不受影響，等慢速 LDAP 的也一樣。這只能提高成本：攻擊者願意多回一輪，就和正常對話同一層。流量大到新使用者還沒回第一輪就被換掉時，要調高上限；`result="evicted"` 持續增加就是在提醒這件事。`max_sessions_per_device`（依 client 加 `Calling-Station-Id`）滿了則不回應，由設備重送，並計入 `result="overload"`。所有使用者經過同一個控制器或 RadSec 代理時，那就是一個 client，尖峰登入多的話要調高 `max_sessions_per_client`。
- **Identifier 不符。** Response 的 Identifier 和等待中的 Request 不同時靜默丟棄（RFC 3748 第 4.1 節），對話照常繼續，只算一輪；開 debug 等級才看得到。
- **設備重新開始。** 同一台設備開新對話時，它還停在 EAP Identity 的舊對話會被取代，計入 `result="replaced"`。已經開始握手的不受影響。沒有 `Calling-Station-Id` 的請求無法分辨設備，不會取代，也不套用 `max_sessions_per_device`。
- **其他負擔。** TLS 訊息超過 64 KiB 就結束對話。閒置超過 `session_timeout_ms` 的對話會被丟棄。
- **TLS。** EAP-TLS 支援 1.2 和 1.3，PEAP 和 TTLS 只用 1.2。不使用系統根憑證，撤銷檢查見下方 CRL。只支援 rustls 的安全加密套件，太舊的用戶端可能連不上。
- **PEAP、TTLS 為什麼不用 TLS 1.3（評估，尚未在實機驗證）：** RFC 9427 定義了 TTLS（和 TEAP、EAP-FAST）在 TLS 1.3 上的金鑰匯出方式；PEAP 不在它的範圍，各家自己實作。Windows 11 22H2 起 PEAP 和 TTLS 預設用 TLS 1.3，但伺服器只提供 1.2 時會用 1.2，所以不影響連線（Microsoft 文件也寫 NPS 不支援 TLS 1.3）。社群回報 Windows 11 的 TTLS 搭配 TLS 1.3 和 session 恢復時會出問題；wpa_supplicant 2.10 起支援。TLS 1.3 對 PEAP、TTLS 的好處主要是少一個來回，前向保密在 TLS 1.2 的 ECDHE 已經有了，而內層密碼本來就在加密通道裡。因此目前不做，等有用戶端只支援 TLS 1.3，或實機測試發現需要時再評估。
- **憑證。** 伺服器憑證的 EKU 建議包含 serverAuth，部分用戶端會檢查（依我的認知，尚未在實機驗證）。登入量大時用 ECDSA P-256 的憑證：每次登入都是完整的 TLS 握手，同樣的機器上 ECDSA 每次登入的 CPU 約是 RSA 2048 的三分之一，吞吐約 2 到 3 倍（第 9 節〈EAP 登入〉的量測）。ECDSA 的相容性（查資料整理，尚未在實機驗證）：
  - **曲線用 P-256，不要用 P-384。** 有報告指出 Android 7.0 的橢圓曲線只支援 P-256，P-384 的憑證鏈在那個版本會失敗（社群回報，不是官方文件）。CA 和中繼憑證也一樣。
  - **Windows 10 以後**預設啟用 `TLS_ECDHE_ECDSA_WITH_AES_128/256_GCM` 等套件，排在 RSA 套件之前（Microsoft 的 Schannel 文件）。
  - **能用 RSA 憑證連上的用戶端，大多也能用 ECDSA：** rradius 的 TLS 1.2 只提供 ECDHE 搭配 AES-GCM 或 ChaCha20 的套件，用戶端一定要支援 ECDHE；目前查到有支援這些套件的 TLS 實作，也都支援 ECDSA 簽章。macOS、iOS、Android 是依我的認知。
  - **不確定時先保留 RSA：** 伺服器一次只用一張憑證，換成 ECDSA 前，用各種設備連一次確認。
- **金鑰。** 會以 `MS-MPPE-Recv-Key`、`MS-MPPE-Send-Key` 回給設備，MSK 和 TLS 內容不會寫進記錄。
- **認證紀錄。** 每段以接受或拒絕結束的對話寫一筆，`chain` 是 `eap`，`handler` 依方法是 `eap-tls`、`eap-peap` 或 `eap-ttls`；中間每一輪不寫。不屬於任何對話的請求（例如未知的 `State`）一樣回 Access-Reject，但只寫 WARN 日誌。逾時的對話沒有紀錄，看 `result="timeout"`。PEAP 的密碼被拒時，當下就寫日誌和紀錄，不等用戶端走完，紀錄的 `account` 是正規化後的名稱。
- **重新載入。** SIGHUP 會重讀這個區段和憑證，新對話用新設定，進行中的用舊設定做完，包括 `match_identity` 和 `session_timeout_ms`；移除這個區段時也一樣，進行中的對話走完 EAP 才結束。CRL 更新後要送 SIGHUP。
- **CRL 要涵蓋每一層 CA。** 設了 `crl_file`，憑證鏈上每一個 CA 都要有對應的 CRL，查不到撤銷狀態就拒絕，日誌會寫 `UnknownRevocationStatus`。有中繼 CA 時，把根 CA 和中繼 CA 的 CRL 串接放進同一個檔案。

**每個 client 的方法**

```toml
[radius.eap]
methods = ["tls", "peap"]

[[radius.clients]]
name = "wifi-corp"
address = "10.0.0.1"
secret_file = "wifi-corp.secret"
eap_methods = ["tls"]          # 只用憑證，用戶端不能換成密碼

[[radius.clients]]
name = "wifi-guest"
address = "10.0.0.2"
secret_file = "wifi-guest.secret"
auth = "guest"
eap_methods = ["peap", "tls"]  # 先提供 peap，用戶端可以用 Nak 換 tls
```

- `methods` 有多個方法時，**每個 RADIUS client 都一定要設 `eap_methods`**，否則啟動失敗。用戶端可以用 Nak 換方法，所以加一個方法給某台 NAS，不能順便開放給所有 NAS，否則只用憑證的 NAS 也會接受密碼。
- 列表的順序就是提供的順序，只能列 `methods` 裡有的方法。不跑 EAP 的 client 設 `eap_methods = []`，它的 EAP 請求會得到 EAP-Failure。
- 只有一個方法時不用設。

**要用哪一個？** 每台裝置都發得了憑證就用 `tls`。要用帳號密碼時，看密碼怎麼存：明文或 `{nthash}` 用 `peap`，各作業系統內建；只存雜湊（例如 `{argon2}`）或要 `ldap` 的 `bind` 模式就用 `ttls` 的內層 PAP。

**PEAP-MSCHAPv2**

- **用戶端一定要驗證伺服器憑證。** MS-CHAPv2 本身很弱，保護全靠 TLS 通道。用戶端沒有設定驗證伺服器憑證時，假的基地台可以拿到密碼的雜湊，這是最常見的部署錯誤。
- **一定要有串接。** `methods` 有 `peap` 而沒有 `[auth]` 時啟動失敗；client 沒有串接時，對話以 EAP-Failure 結束，日誌寫明原因。
- **密碼要能讀到。** MS-CHAPv2 要比對明文或 NT 雜湊，所以 `sql` 處理器的密碼要是明文或 `{nthash}`；`ldap` 處理器要用 `password_check = "attribute"`，**`bind` 模式無法使用**。
- **一定要真的比對過密碼。** 只有處理器比對過密碼才算通過；`lua` 處理器回 `accept` 時視為拒絕，它自己設的 `MS-CHAP2-Success` 也會被丟掉，所以腳本寫錯不會變成不驗密碼。內層的金鑰不會送出，設備拿到的金鑰來自 TLS 通道。
- **名稱。** 串接和認證紀錄的 `account` 用內層名稱，認證紀錄的 `user` 是外層名稱，外層名稱可以是假的，不用來做任何決定。Access-Accept 帶串接正規化後的帳號名稱當 `User-Name`，除非串接自己回了一個。`default_user` 不適用。
- **內層名稱。** 超過 253 octets 就拒絕，不會執行串接，和 TTLS 相同。
- **範圍。** 只有 PEAPv0、內層 EAP-MSCHAPv2、TLS 1.2。沒有 PEAPv1、cryptobinding、內層 GTC、改密碼和用戶端憑證。TLS 1.3 不開放，因為各用戶端的支援情況沒有資料。內層最多 8 輪，正常是 4 輪。密碼錯誤不重試。
- **腳本。** 串接裡的 `lua` 處理器看到 `eap_method` 是 `"peap"`，`tls_subject` 是 `nil`。

**EAP-TTLS**

- **用戶端一定要驗證伺服器憑證，這裡比 PEAP 更重要。** 內層 PAP 把明文密碼送進 TLS 通道。用戶端沒有驗證伺服器憑證時，假的基地台直接拿到密碼，不只是雜湊。
- **內層 PAP：** `sql` 處理器可以比對雜湊，`ldap` 處理器可以用 `bind` 模式，這是 PEAP 做不到的。名稱超過 253 octets 或密碼超過 128 octets 會直接拒絕。
- **一定要真的比對過密碼。** 通過的處理器要是 `sql`、`ldap`，或設了 `verifies_password = true` 的 `lua`。沒有設定的 `lua` 處理器回 `accept` 視為拒絕，日誌寫 `without checking a password`。原因是給 EAP-TLS 用的腳本常常只檢查帳號是否啟用，把 `ttls` 加進 `eap_methods` 不該讓它變成任何密碼都通過，而且內層方法是用戶端自己選的。`all_accept` 串接裡，只要有一個比對過密碼的處理器接受就算數；`sql` 查不到名稱回「略過」，不算比對過。
- **內層 MS-CHAPv2：** 和 PEAP 相同，密碼要是明文或 `{nthash}`，`lua` 處理器不能通過。challenge 要是 TLS 通道匯出的值，用戶端自己選的會被拒絕。
- **腳本不要記錄密碼。** 內層 PAP 時，串接裡的 `lua` 處理器看得到 `User-Password`。腳本自己比對密碼時要設 `verifies_password = true`，也要自己負責不寫進日誌。伺服器本身不會記錄它。腳本看到 `eap_method` 是 `"ttls"`。
- **其他和 PEAP 相同：** 一定要有串接，`default_user` 不適用，`account` 和 Access-Accept 的 `User-Name` 用內層名稱正規化後的結果，外層名稱不做任何決定，拒絕時立刻記錄一筆。內層沒有名稱就拒絕。
- **範圍。** 只有 TTLSv0、TLS 1.2，內層只有 PAP 和 MS-CHAPv2。沒有內層 EAP、CHAP、MS-CHAPv1 和用戶端憑證。帶 M 旗標而不認得的 AVP 會結束對話。**沒有在實體設備上測試過**，各作業系統預設的內層方法不同，部署時要確認。

量測（Docker Desktop，Linux aarch64 容器，14 核心，Apple M4 Max，release 建置，`eapol_test` 和 rradius 在同一台機器；2048 位元 RSA 憑證，`fragment_size` 預設 1000，`match_identity` 開啟）：

| 項目 | TLS 1.2 | TLS 1.3 |
|------|---------|---------|
| 一次登入的 RADIUS 回合數 | 6 | 7 |
| 一次登入的時間（連續 50 次的平均，含啟動 `eapol_test`） | 5.7 ms | 6.1 ms |
| 伺服器 CPU（50 次共 4 個 10 ms 刻度，只是大略值） | 約 0.8 ms | 約 0.8 ms |

- **每個開啟中的對話占用的記憶體：** 剛回 EAP-TLS Start 時約 3.6 KB，收到 ClientHello、回出伺服器憑證後約 13.6 KB（各開 2000 個，看行程 RSS 的差）。所以預設的 `max_sessions` 全滿時約 56 MB。
- **最壞情況：** 對方把分段送到接近 64 KiB 上限後就停下，每個對話約 85 KB（開 1000 個，看行程 RSS 的差），未認證也做得到。所以 `max_sessions` 全滿時約 350 MB，單一 client 達到 `max_sessions_per_client` 時約 87 MB，要以這個數字估記憶體。閒置逾時後記憶體會回到原本的水準。
- **洗滿時的登入：** 用隨機 MAC 從同一個 client 開新對話（每秒 500、2000 和約 37000 個，後者是這個環境的極限），同時從那個 client 連續登入，三種流量各約 1500 次登入都成功。這個環境兩輪之間只隔幾毫秒。這些數字是改成先踢只送過 Identity 的對話之前量的。現在只有 Identity 到第一個回應之間要趕得上：間隔要短於 `max_sessions_per_client` 除以每秒新對話數，例如 1024 除以 2000 約 0.5 秒；這是計算值，無線網路的實際間隔沒有量過。
- **未認證的負擔：** 回應一個 ClientHello 約 0.46 ms CPU（TLS 1.3，連續 2000 個）。
- **憑證鏈長度：** 用戶端送 RSA-4096 的葉憑證（1442 位元組）和中繼 CA（1094 位元組），TLS 1.3 可以完成登入，遠低於 64 KiB 的上限。

這些數字只代表上述環境，不是效能保證，所以預設值維持不變。

### 2.13 代理 `[proxy]`

代理（`[proxy]`）需要 rradius-enterprise。

### 2.14 帳號管理 `[accounts]`

帳號存在叢集的資料表時，用 `rradius-server user` 新增、改密碼、啟用、停用、刪除和查看帳號。寫入經過叢集，所有節點立刻一致，下一次登入就看得到，不必寫 migration、重啟或 SIGHUP。

資料表的結構由你決定，所以每個動作執行哪一條 SQL 寫在 `[accounts]`：

```toml
[accounts]
scheme = "bcrypt"   # 預設；見下表
add = "INSERT INTO users (username, password) VALUES (?1, ?2)"
passwd = "UPDATE users SET password = ?2 WHERE username = ?1"
enable = "UPDATE users SET enabled = 1 WHERE username = ?1"
disable = "UPDATE users SET enabled = 0 WHERE username = ?1"
delete = "DELETE FROM users WHERE username = ?1"
show = "SELECT username, enabled FROM users WHERE username = ?1"
```

- **`?1` 是帳號名稱，`?2` 是存放用的密碼值。** 只能用這兩個有編號的參數，不能用沒有編號的 `?`，以免順序寫錯把密碼存進帳號欄位。每條都一定要用 `?1`：少了它，例如 `UPDATE users SET password = ?2` 漏了 `WHERE`，會改到所有帳號。`add` 和 `passwd` 一定要用 `?2`，其他動作不能用。字串和註解裡的 `?` 不算。違反時啟動、`--check` 和指令都會失敗並指出設定名稱。
- 每個動作都可以省略；省略的動作執行時以狀態 2 結束，說明缺哪個設定。
- `disable` 要搭配 `user_query` 一起設計：例如 `user_query` 只查 `enabled = 1` 的帳號，停用才會擋下登入。
- `show` 選哪些欄位由你決定，會原樣印出，所以不要選密碼欄。

**指令：**

```bash
rradius-server user add --config /etc/rradius/rradius.toml alice
rradius-server user passwd --config /etc/rradius/rradius.toml alice
rradius-server user disable --config /etc/rradius/rradius.toml alice
rradius-server user show --config /etc/rradius/rradius.toml alice
```

- **密碼只從標準輸入讀，** 不放在命令列（會留在 shell 歷史和程序列表）。在終端機上會問兩次、不顯示輸入；兩次不同或空的密碼都會拒絕，什麼都不寫。從管線讀時用第一行，例如批次匯入：`printf '%s\n' "$PW" | rradius-server user add --config c.toml alice`。
- **名稱和登入時一樣正規化：** 套用預設串接的 `[auth.username]`（去掉 realm、轉小寫、`allowed` 檢查），所以 `user add Alice@Corp` 在 `strip_realm = "suffix"`、`lowercase = true` 時存成 `alice`。具名串接自己的 `[auth.chains.<name>.username]` 不套用。
- **經過叢集的 api 埠：** 用 `[cluster]` 的 `nodes` 位址和 `secret_api` 連線，所以叢集要在執行、多數節點要在線上，執行指令的主機要連得到 api 埠。指令不會啟動節點或建立資料目錄。
- **`secret_api` 等於整個資料庫的鑰匙。** 能讀設定檔的人本來就能用它改任何資料表，請把設定檔和 `secret_api_file` 設成只有管理者能讀，並在節點上或經過 WireGuard（2.2 節）執行指令。

| `scheme` | 存成 | 適合 |
| --- | --- | --- |
| `bcrypt`（預設） | `{crypt}$2b$10$...` | 一般用途。驗證刻意很慢：2026-11-07 在 14 核心的 Apple M4 Max 上用 `rradius-bench`（PAP、併發 64）量到每秒約 281 次登入，登入量大時不夠 |
| `sha512-crypt` | `{crypt}$6$...` | 登入量大、只用 PAP。仍有鹽和多輪雜湊，驗證快很多 |
| `nthash` | `{nthash}...` | MS-CHAPv2、PEAP、EAP-TTLS 的 MS-CHAPv2（5.1 節）；PAP 也能用 |
| `clear` | `{clear}...` | CHAP（例如 PPPoE），它只能比對明文。資料庫外洩就等於密碼外洩 |

`scheme` 只影響之後寫入的密碼，已經存在的帳號保留原本的格式，伺服器照樣驗證（第 5 節）。

| 結果 | 輸出 | 狀態碼 |
| --- | --- | --- |
| 完成 | `added alice`、`password changed for alice`、`enabled alice`、`disabled alice`、`deleted alice`；`show` 印出每欄一行 `欄名=值`，多列之間空一行 | 0 |
| 沒有符合的帳號（`passwd`、`enable`、`disable`、`delete` 改到 0 列，`show` 查無資料） | `no account alice` | 1 |
| 資料庫拒絕，例如 `add` 已存在的帳號 | 錯誤訊息（stderr） | 1 |
| 用法、設定、名稱或密碼有誤，沒有送出任何東西 | 錯誤訊息（stderr） | 2 |
| 連不到叢集，或 10 秒內沒有回應 | 錯誤訊息（stderr） | 3 |

任何輸出和錯誤訊息都不包含密碼或存放值。

動態 client、DPSK 金鑰和 SIM 卡的管理指令（`client`、`dpsk`、`aka`，SQL 寫在 `[accounts.clients]`、`[accounts.dpsk]`、`[accounts.aka]`）需要 rradius-enterprise。

### 2.15 每人一把 WPA-PSK

同一個 WPA2-PSK 的 SSID 上，讓每個使用者用自己的密碼（各廠商叫 DPSK、iPSK、MPSK、PPSK），就能在不支援 802.1X 的設備上分開每個人、撤銷單一個人的密碼、依人給 VLAN。AP 有兩種做法：

| 做法 | AP 送什麼 | rradius 怎麼做 |
| --- | --- | --- |
| **MAC 認證，回覆這台的 PSK** | 以設備的 MAC 當 `User-Name` 的 Access-Request | 腳本或 `sql` 處理器依 MAC 查出密碼放進回覆，見下方〈依 MAC 回覆 PSK〉 |

另一種做法是從四次交握找出是哪一把（`[radius.dpsk]`），需要 rradius-enterprise。

#### 依 MAC 回覆 PSK

設備要先登記 MAC。AP 以 MAC 發 Access-Request，腳本依 MAC 查出密碼放進回覆。各廠商用的屬性：

| AP | 回覆屬性 | 寫法 |
| --- | --- | --- |
| Cisco iPSK | `Cisco-AVPair` | `set_attr('Cisco-AVPair', {'psk-mode=ascii', 'psk=' .. passphrase})` |
| Aruba MPSK | `Aruba-MPSK-Passphrase` | `set_attr('Aruba-MPSK-Passphrase', passphrase)`，依字典自動加密 |
| Meraki、Mist、hostapd（`wpa_psk_radius=1` 或 `2`） | `Tunnel-Password` | `set_attr('Tunnel-Password', passphrase)`，依 RFC 2868 自動加密 |
| Ruckus | `Ruckus-DPSK` | `set_attr('Ruckus-DPSK', '\0' .. hex_to_bytes(wpa_pmk(passphrase, ssid)))`，PMK 前面加一個 0x00；`hex_to_bytes` 要自己寫 |

- 屬性名稱和值的格式以各廠商文件為準，上表沒有用實體設備驗證過。
- `Cisco-AVPair` 的值會以明文送出，只在可信任的網路或 RadSec 上用。
- `wpa_pmk` 每次都跑 4096 輪 PBKDF2，約數毫秒；設備多時把算好的 PMK 存起來，不要每次登入都算。

## 3. 認證串接 `[auth]`

認證串接把多個處理器依設定順序執行，每個處理器回傳「接受」、「拒絕」、「略過」（交給下一個處理器）或「要求 challenge」（只有 `lua` 處理器，RADIUS 認證才有）。

### 3.1 結構

```toml
# 預設串接：沒有指定 auth 的用戶端使用
[auth]
policy = "first_match"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ?"

# 具名串接：用戶端以 auth = "vpn" 選擇
[auth.chains.vpn]
policy = "all_accept"

[[auth.chains.vpn.handlers]]
type = "ldap"
# ...

[[auth.chains.vpn.handlers]]
type = "lua"
script = "vpn_rules.lua"
```

- `[[auth.handlers]]` 是預設串接，可以省略；`[auth]` 底下的 `policy` 屬於預設串接，只在有 `[[auth.handlers]]` 時才能設定。
- `[auth]` 至少要有預設串接或一條具名串接；具名串接至少要有一個處理器。
- 用戶端的 `auth` 指向不存在的串接時啟動失敗，訊息含用戶端名稱與串接名稱。
- 每條串接在啟動時只建立一次，引用同一條串接的用戶端（包括 RADIUS 與 TACACS+）共用它，LDAP 連線池也共用。

選擇規則：用戶端有 `auth` → 該具名串接；沒有 `auth` → 預設串接；沒有預設串接 → `[lua].script`。

串接只用於 RADIUS Access-Request 與 TACACS+ 認證（PAP 在 START，ASCII 登入在最後一個 CONTINUE）；授權與計費仍由 `[lua].script` 處理。

### 3.2 合併規則 `policy`

| policy | 行為 |
| --- | --- |
| `first_match`（預設） | 依序執行，第一個不略過的處理器決定結果；接受時只帶回該處理器的回應屬性 |
| `all_accept` | 依序執行；任一拒絕或 challenge 就停止；至少一個接受才算通過；回應屬性依順序合併。串接裡有會比對密碼的處理器（`sql`、`ldap`、設了 `verifies_password` 的 `lua`）時，至少要有一個接受，否則拒絕；在它們接受之前，不會比對密碼的腳本也不能回 `challenge`。沒有人接受、後面也沒有這種處理器時，腳本不會執行，所以查不到的名稱不會留下腳本的寫入。憑證也適用：憑證證明的是名稱，不是帳號可以用 |

兩種模式下，全部略過都視為拒絕。

**`first_match` 不會替你檢查密碼：** `sql` 或 `ldap` 查不到名稱時回「略過」，交給下一個處理器。後面接只檢查政策的腳本時，不存在的使用者不用密碼就能通過，所以政策腳本要放進 `all_accept`，或設定 `verifies_password` 並自己比對。

### 3.3 `sql` 處理器

| 欄位 | 預設 | 說明 |
| --- | --- | --- |
| `user_query` | （必填） | 取得使用者密碼的查詢，剛好一個 `?`（使用者名稱），回傳第一欄為密碼 |
| `certificate_query` | 無 | EAP-TLS 憑證通過後改用這個查詢找使用者，剛好一個 `?`，查到就接受，不看第一欄。沒有設定時用 `user_query`，第一欄是 NULL、只有前綴或空值就拒絕（2.12 節） |
| `reply_query` | 無 | 取得回應屬性的查詢，剛好一個 `?`，回傳 `(attribute, value)` 兩欄 |
| `default_user` | 無 | 查無使用者時改用這個帳號的密碼驗證（知道該密碼的人都會通過，請謹慎使用）。通過時認證紀錄的 `account` 是這個帳號，`user` 是對方送來的名稱 |
| `timeout_ms` | 5000 | 每次查詢的時限，必須大於 0 |

- 查無使用者 → 略過；查到多筆 → 拒絕並記錄；密碼欄為 NULL、空字串或只有 `{clear}` 這類前綴 → 拒絕。
- `?` 的數量以字元計算，不要在查詢的字串常值中使用 `?`。
- 支援的密碼格式見第 5 節。

### 3.4 `lua` 處理器

```toml
[[auth.handlers]]
type = "lua"
script = "site_rules.lua"   # 以設定檔目錄解析
# verifies_password = true  # 選填，預設 false：這個腳本會比對 User-Password，見 2.12 節 EAP-TTLS
```

腳本回傳 `'accept'`、`'reject'`、`'ignore'`（略過）或 `'challenge'`（RADIUS 認證才有）。腳本 API 見第 4 節；`phase` 為 `"authentication"`。

### 3.5 `ldap` 處理器

| 欄位 | 預設 | 說明 |
| --- | --- | --- |
| `urls` | （必填） | 一或多個 `ldaps://` 或 `ldap://` URL，依序嘗試 |
| `bind_dn` | （必填） | 服務帳號 DN |
| `bind_password` / `bind_password_file` | （二擇一） | 服務帳號密碼，不可為空，否則載入時報錯：空密碼等於匿名綁定 |
| `base_dn` | （必填） | 搜尋起點 |
| `filter` | （必填） | 搜尋條件，剛好一個 `{user}`，例如 `(uid={user})` |
| `certificate_filter` | 無 | EAP-TLS 憑證通過後改用這個搜尋條件，剛好一個 `{user}`。沒有設定時，這個處理器略過憑證登入（2.12 節） |
| `password_check` | `"bind"` | `"bind"`：以找到的 DN 與使用者密碼登入；`"attribute"`：讀取密碼屬性比對 |
| `password_attribute` | 無 | `password_check = "attribute"` 時必填，例如 `userPassword` |
| `reply_attributes` | 無 | 回應屬性名稱對 LDAP 屬性名稱，例如 `{ "Filter-Id" = "radiusFilterId" }`，依寫入順序 |
| `timeout_ms` | 5000 | 每個 URL 從取得連線到最後一個操作的時限 |
| `ca_file` | 無 | 額外信任的 CA（PEM），以設定檔目錄解析 |
| `allow_plaintext` | `false` | 為 `false` 時 `ldap://` 一律以 StartTLS 升級 |
| `pool_size` | 4 | 每個 URL 保持的連線數上限 |
| `required_groups` | 無 | 必須屬於其中一個群組，否則拒絕 |
| `group_attribute` | `memberOf` | 從使用者項目的這個屬性讀群組 |
| `group_base_dn`、`group_filter` | 無 | 兩者一起設定時改用搜尋。`group_filter` 有一個 `{dn}`，例如 `(member={dn})`。不能和 `group_attribute` 同時設定 |
| `group_reply` | 無 | 屬於某群組時要加上的回應屬性，見下方 |

行為：
- `{user}` 會依 RFC 4515 跳脫，使用者名稱無法改變搜尋條件的結構。
- 查無 → 略過；多筆 → 拒絕並記錄。
- `bind` 模式只接受 PAP，空密碼一律拒絕（避免被當成匿名登入）；`invalidCredentials` 為一般拒絕，其他登入錯誤視為伺服器錯誤。
- `attribute` 模式支援 PAP，以及密碼以明文或 `{clear}` 儲存時的 CHAP，也支援 MS-CHAPv2（5.1 節）。
- 連線失敗、連線中斷或逾時才換下一個 URL，包含服務帳號登入、搜尋和使用者登入途中斷線；伺服器回應的錯誤，例如服務帳號密碼錯誤、搜尋失敗，一律拒絕並記錄，不會略過。
- 憑證一律驗證（系統信任的 CA 加上 `ca_file`）。
- 連線在認證之間重用：取用前確認連線仍開著，並在 `timeout_ms` 的一半內以服務帳號重新登入；出錯或逾時的連線會丟棄並改建新連線。

```toml
[[auth.handlers]]
type = "ldap"
urls = ["ldaps://dc1.example.com", "ldaps://dc2.example.com"]
bind_dn = "cn=radius,ou=services,dc=example,dc=com"
bind_password_file = "ldap.secret"
base_dn = "ou=people,dc=example,dc=com"
filter = "(uid={user})"
reply_attributes = { "Filter-Id" = "radiusFilterId" }
```

**群組：** 依群組限制登入，或給不同的屬性：

```toml
[[auth.handlers]]
type = "ldap"
# ...
required_groups = ["cn=vpn,ou=groups,dc=example,dc=com"]

[[auth.handlers.group_reply]]
group = "cn=staff,ou=groups,dc=example,dc=com"
attributes = { "Filter-Id" = "staff" }
```

- **不在群組和密碼錯誤的回應相同，** 外部無法分辨。判斷在密碼驗證通過之後。
- **`group_filter` 本身不是限制。** 沒有 `required_groups` 和 `group_reply` 時，設定 `group_attribute`、`group_base_dn` 或 `group_filter` 會啟動失敗，因為不會有任何查詢。
- 沒有 `memberOf` 的目錄用 `group_base_dn`、`group_filter`。搜尋以服務帳號在使用者登入之前執行，每次認證多一次搜尋。在本機的 OpenLDAP 上，debug 建置實測每次認證從約 603 微秒變成約 662 微秒。
- AD 要展開巢狀群組時，可以寫 `group_filter = "(member:1.2.840.113556.1.4.1941:={dn})"`。`group_attribute` 只看項目上的直接成員關係。這個寫法沒有在 AD 上測過。
- **DN 比對不分大小寫（只處理 ASCII），也忽略逗號旁的空白，** 除此之外要和目錄回傳的寫法相同。寫法不同時比對不到，結果是拒絕或沒有加上屬性，不會多給權限。
- 群組搜尋失敗視為伺服器錯誤，拒絕並記錄。
- `group_reply` 依設定順序加在 `reply_attributes` 之後，可以同時符合多個。屬性名稱或值不正確時，該次認證會被拒絕並記錄 `RADIUS response attribute error`；`--check` 會提早發現這類錯誤。

### 3.6 使用者名稱正規化 `username`

設備送來的名稱常帶網域或大小寫不一，例如 `alice@corp.example`、`CORP\Alice`。每條串接可以設定如何從收到的名稱算出查帳號用的名稱：

```toml
[auth.username]          # 預設串接；具名串接用 [auth.chains.<name>.username]
strip_realm = "suffix"   # 預設 none
lowercase = true         # 預設 false
allowed = "[a-z0-9._-]+" # 選填
```

| `strip_realm` | 去除的部分 | `CORP\Alice@Corp` 的結果 |
| --- | --- | --- |
| `none` | 不去除 | `CORP\Alice@Corp` |
| `suffix` | 最後一個 `@` 起的內容 | `CORP\Alice` |
| `prefix` | 最後一個反斜線以前的內容 | `Alice@Corp` |
| `both` | 先做 `prefix` 再做 `suffix` | `Alice` |

- **處理順序固定：** 去除 realm → 改小寫 → 檢查 `allowed`。所以 `allowed` 只要考慮處理後的名稱，不必列出 realm 或大寫。
- **`allowed` 必須符合整個名稱，** 不必自己加 `^`、`$`。`[a-z]+` 不會放行 `a'; --`。
- **結果為空或不符合 `allowed` 時，** 整條串接直接拒絕，不執行任何處理器，`first_match` 和 `all_accept` 都一樣。記錄只寫 client 和原因，不寫名稱，因為名稱可能含有控制字元。
- **沒有名稱的請求** 照常交給處理器，和沒有這個區段時相同。
- **不會繼承：** 預設串接的設定不會套用到具名串接。沒有這個區段的串接，直接用收到的名稱。
- `[auth.username]` 要有 `[[auth.handlers]]` 才能設定，和 `policy` 相同。設定有誤時啟動失敗。SIGHUP 時和串接一起生效，不必重啟。

各處理器如何使用：

| 處理器 | 用處理後的名稱 | 用收到的名稱 |
| --- | --- | --- |
| `sql` | `user_query`、`reply_query` | 驗證密碼 |
| `ldap` | `filter` 的搜尋 | `attribute` 模式驗證密碼 |
| `lua` | 全域變數 `auth_user` | `get_attr('User-Name')` |

- **MS-CHAPv2 一律用收到的名稱驗證，** 因為用戶端是用它計算回應的（見 5.1 節）。回應裡的屬性和 `get_attr` 也不會改變。
- **不經過這些設定：** 計費封包、TACACS+ 授權，以及沒有串接時的最上層腳本。它們要自己處理名稱，`auth_user` 在這些腳本中是 `nil`。
- **開啟 `lowercase` 之前，先把資料統一成小寫。** 否則資料表裡的 `Alice` 會查不到；原本只差大小寫的不同帳號，也會指向同一筆。
- **設定 `allowed` 之前，先在測試環境確認。** 樣式太嚴會擋掉合法使用者。
- 不做 Unicode 正規化（NFC 等），也不依 realm 選擇串接。

## 4. Lua 腳本

### 4.1 執行環境

- 腳本以 Luau 執行（以 Lua 5.1 為基礎，支援 `+=` 等語法，不支援 `goto`）。
- 所有腳本（最上層腳本與 `lua` 處理器）在啟動時編譯一次；語法錯誤會讓啟動失敗並指出檔案。
- 沙箱：內建函式庫唯讀；沒有 `io`、`os.execute`、`require`、`loadstring`、`getfenv`、`setfenv`，呼叫時會出錯，只在腳本執行到那一行時才會發現。可以使用 `string`、`table`、`math`、`os.time`、`os.clock` 等。
- 每個請求有自己的全域變數，請求之間不共享；需要跨請求保存的資料請寫入資料庫。
- **記憶體上限：** 每個 VM 最多用 `[lua].max_memory_mb`。超過時要配置的腳本會得到 `memory error: not enough memory`，處理方式和其他腳本錯誤相同，程序不會結束；當時在同一個 VM 上的其他腳本也可能遇到。腳本結束時 VM 用量超過上限一半，會做一次完整回收，這段時間會卡住一個工作執行緒，也不會有新請求分到這個 VM。之後用量比回收後多出上限的四分之一，或超過上限的八分之七，才會再做。**回收是逐步進行的：** 反覆產生很大的字串時，即使都沒有留下，也可能來不及回收而出錯。實測預設 256 MiB 時，反覆產生 8 MiB 的字串 400 次沒有問題，16 MiB 就會出錯。這個上限不涵蓋 Lua 以外的記憶體，例如等待中的資料庫查詢結果。
- 腳本在一個請求中的執行時間超過 `[lua].max_execution_ms` 時會被中止並視為拒絕；等待資料庫的時間不計入。時間每 64 次迴圈回跳或函式呼叫才檢查一次，超過後每次都檢查。`pcall` 接得住這個錯誤，但下一次迴圈或呼叫又會出錯，而且就算腳本接住後正常回傳 `accept`，也算失敗。所以超過後最多再跑 64 次才被中止，迴圈內容很慢時會晚一些。執行較久的腳本會定期讓出，不會阻擋其他請求。
- 腳本出錯（含逾時）時，該請求視為拒絕；但出錯或拒絕是因為叢集資料庫不能用（`sql_query`、`sql_execute` 逾時或找不到 leader，腳本用 `pcall` 接住也一樣）時，認證請求不回應、TACACS+ 回 ERROR（2.2 節〈資料庫不能用時〉）。錯誤會記錄為 `Lua script error (<protocol>/<phase>): <訊息>`，訊息中的位置是腳本路徑與行號，例如 `/etc/rradius/auth.lua:3: attempt to index nil with 'y'`。

### 4.2 全域變數

| 名稱 | 內容 |
| --- | --- |
| `protocol` | `"radius"` 或 `"tacacs"` |
| `phase` | `"authentication"`、`"authorization"` 或 `"accounting"` |
| `client` | 比對到的用戶端 `name`，沒有設定時為其 `address` 字串 |
| `auth_user` | 認證串接的 `lua` 處理器中，是查帳號用的名稱（見 3.6 節），沒有名稱時是 `nil`；其他腳本都是 `nil` |
| `replayed` | 這個計費請求是否來自暫存的重放（見 2.11 節），其他時候是 `false` |
| `eap_method` | EAP 授權請求是 `"tls"`、`"peap"` 或 `"ttls"`（見 2.12 節），其他時候是 `nil` |
| `tls_subject` | `eap_method` 是 `"tls"` 時，憑證的主體，例如 `CN=alice, O=Example`，值依 RFC 4514 跳脫，順序和憑證相同。各種字串類型都會解成文字；不是文字或解不開的值，寫成 `#` 加整個值的 DER 十六進位，例如 `O=#1c0400000061`。名稱表沒有的欄位用 OID，例如 `2.5.4.97=...`。其他時候是 `nil` |
| `tls_subject_attrs` | 同上時，依欄位分開的表，值不跳脫，例如 `tls_subject_attrs.OU[1]`；不是文字的值同樣是 `#` 開頭，和文字本身以 `#` 開頭的值分不出來，要比對就不要依賴這種值。其他時候是 `nil` |

### 4.3 函式

| 函式 | 說明 |
| --- | --- |
| `get_attr(name)` | 取得請求屬性；不存在時回傳 `nil`。同名屬性有多個時回傳第一個 |
| `get_attrs(name)` | 取得所有同名屬性的陣列，順序和封包相同；不存在時回傳空表 |
| `set_attr(name, value)` | 加入一個回應屬性（字串或數字），依呼叫順序保留。tagged 屬性可以在名稱後加 tag，例如 `Tunnel-Type:1` |
| `sql_query(sql [, params])` | 讀取本機資料庫副本，回傳列的陣列，每列是「欄名 → 值」的表；只能讀取 |
| `sql_execute(sql [, params])` | 透過叢集寫入並複寫，回傳受影響的列數 |
| `now_unix()` | 目前的 Unix 時間（秒） |
| `totp_verify(secret, code)` | 以 RFC 6238 驗證 TOTP 驗證碼（HMAC-SHA-1、6 位數、30 秒一步，前後各容許一步）。符合時回傳時間步，否則回傳 `nil`；`secret` 不是 Base32 時腳本出錯 |
| `random_token()` | 回傳 32 個十六進位字元的隨機值，可用來產生 `State` |
| `wpa_pmk(passphrase, ssid)` | 回傳 WPA2 的 PMK（PBKDF2-HMAC-SHA1、4096 輪、32 octets）的 64 個小寫十六進位字元。`passphrase` 要是 8–63 個可列印 ASCII 字元（或 64 個十六進位字元的 PSK，原樣回傳），`ssid` 要是 1–32 octets，否則腳本出錯，訊息不含密碼。每次呼叫都要跑 4096 輪，約數毫秒，見 2.15 節 |
| `send_disconnect`、`send_coa` | 要求 NAS 中斷連線或變更權限（RFC 5176），需要 rradius-enterprise；rradius-server 呼叫時腳本出錯 |
| `log(...)` | 寫一行記錄，見 4.9 節 |
| `http_request(options)` | 呼叫白名單上的 HTTP API，只有設定 `[lua.http]` 時才有，見 4.11 節 |
| `url_encode(text)` | 把一個值編碼成 URL 的一段，只有設定 `[lua.http]` 時才有，見 4.11 節 |

資料庫值轉換：INTEGER、REAL → 數字；TEXT、BLOB → 字串；NULL → `nil`。

`sql_execute` 不要用 `RETURNING`：陳述式會寫入，但呼叫會因為它回傳了資料列而出錯。需要寫入後的值時，寫入後再用 `sql_query` 讀。

`sql_execute` 會拒絕在各節點結果可能不同的寫法，例如 `datetime()`、`random()`、`CURRENT_TIMESTAMP`；名稱加了引號、中間夾註解也一樣。這些字當欄位或資料表名稱沒關係，但名稱後面接括號就算呼叫，只有 `INSERT INTO`、`CREATE TABLE`、`REFERENCES`、`WITH` 後面的資料表例外，所以 `CREATE INDEX i ON "date"(a)` 也會被拒絕，這種請放進 migration。需要時間時請用 `now_unix()` 取得後寫入：

```lua
sql_execute("INSERT INTO sessions VALUES (?, ?)", {id, math.floor(now_unix())})
```

**參數：** 來自請求的值一律用 `params` 傳入，不要接進 SQL 字串。這些值由設備或使用者控制，接進字串就是 SQL injection。

```lua
local rows = sql_query("SELECT plan FROM users WHERE username = ?", {get_attr('User-Name')})
```

- `params` 依序綁定 SQL 中的 `?`。數量不符時腳本會出錯，不會送出 SQL。字串、引號內的名稱和註解裡的 `?` 不算。
- 只支援 `?`，不支援 `?1`、`:name`、`@name`、`$name`。
- 字串綁定為 TEXT，不是有效 UTF-8 的字串綁定為 BLOB；有整數值的數字綁定為 INTEGER，其他數字綁定為 REAL；`true`、`false` 綁定為 1、0。
- 表、函式、NaN 和無限大都會讓腳本出錯。`nil` 在陣列中間會出錯，在結尾會被丟掉，只有 SQL 還需要那個值時才會因數量不符而出錯。**值可能不存在時要先檢查：** `{get_attr('User-Name')}` 在沒有這個屬性時是空陣列，會因數量不符而出錯。要比對或寫入 NULL，請直接寫在 SQL 裡（`IS NULL`、`NULL`）。
- 不傳 `params` 或傳 `nil` 時視為沒有值，所以 SQL 裡有 `?` 就會出錯。字串、引號內的名稱和註解裡的 `?` 不算。
- 上面對各節點結果不同的寫法的檢查只看 SQL 文字，不看參數值。

### 4.4 回傳值

| 回傳 | 結果 |
| --- | --- |
| `'accept'` | 接受 |
| `'ignore'` | 在認證串接的 `lua` 處理器中為略過；作為最上層腳本時視為拒絕 |
| `'challenge'` | RADIUS 認證時回 Access-Challenge，串接會在此結束；TACACS+ 與其他階段視為拒絕 |
| 其他值、沒有回傳、或發生錯誤 | 拒絕 |

**計費階段的回傳值有特別的意思**：`'accept'` 代表已記錄成功，設備收到確認後就不會再送。其他結果都代表沒有記錄，設備會重送，腳本也會再執行一次，每次都會記錄一行。因此：

- 刻意不想記錄某些封包時，請回傳 `'accept'`，不要回傳 `'reject'`，否則設備會一直重送到次數用完。
- 腳本要能重複執行：寫到一半失敗時，重送會再跑一次。請以 `UNIQUE` 索引加 `INSERT OR IGNORE` 之類的方式防止重複。
- 叢集失去多數節點時，寫入會逾時，計費請求都不會有回應，直到叢集恢復或設備重送次數用完為止。設定了計費暫存時，這些請求會先寫到本機再回覆（見 2.11 節）。

**Access-Challenge 能帶的屬性：** `Reply-Message`、最多一個 `State`、廠商屬性、`Idle-Timeout`、`Session-Timeout`（RFC 2865 第 4.4 節）。帶了其他屬性時會改回 Access-Reject 並記錄原因。設備會把 `State` 原樣帶回下一個請求，腳本用 `get_attr('State')` 讀取。

**兩步驟驗證範例：** 見 [`rradius-server/examples/two-step-totp.lua`](../rradius-server/examples/two-step-totp.lua)，檔案開頭有需要的資料表，測試也直接執行這份腳本。第一個請求檢查密碼，回傳 `'challenge'`；第二個請求帶回 `State`，`User-Password` 是驗證碼。範例處理了這些事，自己寫腳本時也要處理：

- **挑戰只能用一次，而且會過期：** `State` 存在資料庫，用 `DELETE` 回傳的列數確認是否由這個請求取得。
- **驗證碼只能用一次：** 把 `totp_verify` 回傳的時間步存起來，只接受更新的時間步。只回傳對錯的話做不到這點，RFC 6238 也要求防止重複使用。
- **限制猜測次數：** 6 位數的驗證碼可以被暴力猜中，範例在連續猜錯 5 次後鎖定帳號 5 分鐘。知道密碼的人才能觸發鎖定，密碼本身的猜測次數沒有限制，要另外處理。
- **沒有密碼的帳號：** 比對前先拒絕 `NULL` 或空字串，否則不帶密碼的請求會比對成功。
- **一律用參數：** `User-Name` 和 `State` 都由外部控制，要用 `params` 傳入。
- **多節點：** 第二個請求可能到另一個節點，那裡的資料庫副本可能略為落後，查不到剛寫入的資料，鎖定也可能晚一點生效；範例用的 `DELETE` 和 `UPDATE` 經過叢集，不受影響。

### 4.5 RADIUS 屬性

**動態 VLAN：** `Tunnel-Type`、`Tunnel-Medium-Type`、`Tunnel-Private-Group-Id` 是 tagged 屬性，`Tunnel-Password` 會自動依 RFC 2868 加密。`sql` 處理器的回覆欄名稱也可以用同樣的寫法。

```lua
set_attr('Tunnel-Type:1', 'VLAN')
set_attr('Tunnel-Medium-Type:1', 'IEEE-802')
set_attr('Tunnel-Private-Group-Id:1', '100')
```

**位址型別：** `set_attr`（以及 `sql` 處理器的 `reply_query`）和 `get_attr` 都用同樣的文字格式：

```lua
set_attr('Framed-IPv6-Prefix', '2001:db8::/64')
set_attr('Delegated-IPv6-Prefix', '2001:db8:1::/48')
set_attr('Framed-Interface-Id', '0011:2233:4455:6677')
```

| 型別 | 文字 | 注意 |
| --- | --- | --- |
| `ipaddr` | `192.0.2.1` | |
| `ipv6addr` | `2001:db8::1` | |
| `ipv6prefix` | `2001:db8::/64` | 前綴之後的位元必須是 0，`2001:db8::1/64` 會讓腳本出錯。一律送完整的 18 octets |
| `ipv4prefix` | `192.0.2.0/24` | 前綴之後的位元必須是 0 |
| `ifid` | `0011:2233:4455:6677` | 四組，每組固定 4 個十六進位字元 |
| `ether` | `00:11:22:aa:bb:cc` | 六組，每組固定 2 個十六進位字元，只接受 `:` 分隔 |
| `combo-ip` | IPv4 或 IPv6 位址 | |

輸入的十六進位字元不分大小寫，`get_attr` 一律回傳小寫。格式不符或給數字時，`set_attr` 讓腳本出錯，錯誤訊息會指出屬性名稱。

**`reply_query` 查到格式不符的值時，** 這時沒有腳本可以出錯，所以 Access-Accept 會改成 Access-Reject，就算密碼正確，並記錄 `RADIUS response attribute error`。上線前請確認資料表裡的值。

**升級注意：`get_attr` 讀 `ipv6addr` 屬性（例如 `NAS-IPv6-Address`）以前回傳 16 個原始位元組，現在回傳文字。** 把它當位元組處理的腳本要修改。

**延伸屬性和 TLV：** RFC 6929 的 `extended`、`long-extended`、`evs` 和 `tlv` 都用裡面屬性自己的名稱讀寫，字典的 `format=` 也會生效，所以 WiMAX 這類廠商屬性可以用。

```lua
local status = get_attr('Frag-Status')
set_attr('Response-Length', 4096)
set_attr('IPv6-6rd-IPv4MaskLen', 8)
set_attr('IPv6-6rd-BR-IPv4-Address', '192.0.2.1')
set_attr('WiMAX-Release', '2.0')
```

- **相鄰的才合併。** 連續設定同一個 `tlv` 裡的屬性，會放進同一個屬性，例如上面的 6rd 是一個屬性 173。中間夾了別的屬性就會分開。要送兩組獨立的 TLV，目前只能在中間放別的屬性。放不下時會在放不下的那一層開始新的 TLV，一路到新的屬性，不會把一個值切開，所以 `set_attr` 當下沒出錯的值，送出時也放得下。
- **長度。** `long-extended` 和帶 continuation 的廠商屬性（`format=1,1,c`）會自動分段，收到時也會自動合併。其他型別放不下時腳本出錯。TLV 自己的長度欄位只有一個 octet，所以會分段的格式裡，TLV 的值最多 253 octets；不分段的格式還要放進屬性本身，一般最多 251，預設格式的廠商屬性最多 245。每多一層巢狀少 2 octets。整個回應仍受 4096 octets 限制。
- **容器本身不能設定。** `set_attr('IPv6-6rd-Configuration', ...)`、`set_attr('Extended-Attribute-1', ...)` 會讓腳本出錯。
- **無效的屬性會被忽略。** 分段不完整或 TLV 長度不對時，那個屬性裡的內容腳本都看不到，請求的其他部分照常處理（RFC 6929 section 2.8）。字典沒有定義的 TLV 類型會被略過。
- **Access-Challenge：** `evs` 的屬性和其他延伸屬性一樣不能放，廠商屬性（Vendor-Specific）可以。
- **TLV 裡的加密屬性：** 字典裡只有 `WiMAX-hDHCP-DHCP-RK`、`WiMAX-vDHCP-DHCP-RK`。不能 `set_attr`，`get_attr` 讀到的是未解密的值。
- **升級注意：**
  - `format=` 以前沒有作用，`format=2,1`、`2,2`、`4,0` 的廠商屬性（例如 Lucent、Starent、USR）以前會讀錯，現在才讀得到正確的屬性。
  - `get_attr('WiMAX-Capability')`、`get_attr('Extended-Attribute-1')` 這類容器名稱以前回傳原始位元組，現在是 `nil`，要用裡面屬性的名稱。
  - 字典的 `VENDOR` 和 `BEGIN-VENDOR` 上不認得的選項或多餘的欄位以前被忽略，現在載入失敗。

字典的 `encrypt=` 只能是 1、2、3，其他寫法在載入時就報錯；選項可以用逗號或空白分隔。`set_attr` 不支援 `encrypt=1`、`encrypt=3` 的屬性，也不支援 `abinary`；延伸屬性和 TLV 裡有 `has_tag` 或 `encrypt` 的定義也不支援。這些都會讓腳本出錯。

- `get_attr` 使用字典中的屬性名稱，例如 `User-Name`、`NAS-IP-Address`；Vendor-Specific 屬性會拆成各廠商屬性。字典中沒有的屬性不會出現。
- 型別：`integer`、`date`、`byte`、`short`、`signed`、`integer64` → 數字；上表的位址型別 → 文字，值的長度不符時是原始位元組字串；其他 → 原始位元組字串。
- `User-Password` 會先解密成明文。
- `set_attr` 的名稱必須在字典中，值可以用字典定義的數值名稱，例如 `set_attr('Service-Type', 'Framed-User')`。名稱不存在時腳本出錯並拒絕。
- 計費腳本回傳 `'accept'` 才會回覆 Accounting-Response。腳本出錯（含查詢逾時、超過執行時限）或回傳其他值時不回應，由設備重送（RFC 2866 第 2 節：無法記錄時不可回應）。例外是設了計費暫存時，資料庫造成的失敗會先寫到本機再回覆（2.11 節）。

### 4.6 TACACS+ 屬性

- 共同欄位：`user`、`port`、`rem_addr`。
- 認證：另有 `data`（PAP 和 ASCII 是密碼；CHAP、MS-CHAPv2 是 PPP id、challenge 和回應，見下方）與 `authen_type`（數字，1 為 ASCII、2 為 PAP、3 為 CHAP、6 為 MS-CHAPv2）。
- **ASCII 互動式登入**（RFC 8907）：START 沒帶帳號時伺服器回 GETUSER，再回不顯示輸入的 GETPASS，帳號密碼都取得後才執行認證串接或腳本，和 PAP 一樣由 `user` 與 `data` 取得。空帳號最多問三次；每條連線最多 8 個未完成的登入，60 秒內要完成。
- **升級注意：** 以前 ASCII LOGIN 的 START 會直接交給腳本，現在會先詢問帳號密碼。腳本要改從 `data` 取得密碼。
- 認證的 START 另有 `priv_lvl`（0–15）與 `authen_service`（數字，1 為一般登入、2 為 enable）。
- **enable 請求**（設備的 `enable` 指令，`authen_service` 是 2）由 `[tacacs] enable` 決定：
  - `"deny"`（預設）：直接回 FAIL，不詢問密碼，也不執行串接或腳本，記錄 `TACACS+ enable request refused`。設備收到 FAIL 不會改用下一個方法（例如本機 enable 密碼），所以設備端不要把 rradius 放進 enable 的方法清單，或改用 `"login"`。
  - `"login"`：和登入一樣，以使用者自己的密碼經串接或腳本驗證。串接裡的 `lua` 處理器或最上層腳本可以用 `authen_service`、`priv_lvl` 限制誰能 enable，例如只讓特定角色通過；沒有這類判斷時，**每個能登入的人都能 enable**。
- **升級注意（enable）：** 以前 enable 請求被當成一般登入，能登入的人都能用自己的密碼 enable。現在預設拒絕；需要舊行為時設 `enable = "login"`，並建議在腳本裡限制。
- **只支援 LOGIN 動作。** 改密碼（CHPASS）、SENDPASS、SENDAUTH 一律回 FAIL，不詢問也不執行串接或腳本（RFC 8907：不支援的選項回 FAIL）。以前改密碼會被當成登入而回 PASS，設備會以為密碼已改。
- 授權與計費：每個 `key=value` 或 `key*value` 參數以 `key` 為名稱，例如 `get_attr('cmd')`。另有封包表頭的 `priv_lvl`（使用者目前的權限等級，0–15）、`authen_method`、`authen_type`、`authen_service`（都是數字）。名稱用底線，和設備送的 `priv-lvl` 這類參數區分；授權群組規則只比對參數，不看這些欄位。
- **CHAP 和 MS-CHAPv2 登入**（RFC 8907 第 5.4.2.3、5.4.2.5 節，例如用 PPP 撥接的設備）**要有認證串接**（第 3 節），由串接比對，一個 START 直接回 PASS 或 FAIL，和 RADIUS 的 CHAP、MS-CHAPv2 用同樣的處理器：
  - CHAP 只能比對明文或 `{clear}` 密碼，MS-CHAPv2 要明文或 `{nthash}`（5.1 節）；`ldap` 處理器要用 `attribute` 模式。
  - CHAP 的 challenge 短於 8 個位元組、MS-CHAPv2 的 challenge 不是 16 個位元組時直接回 FAIL 並記錄，不執行串接（RFC 8907 的建議與要求）。
  - MS-CHAPv2 只回 PASS，不帶 authenticator response（RFC 8907 沒有定義它放在哪裡），需要雙向驗證的 PPP 用戶端可能因此失敗，這種情況改用 RADIUS。
  - 沒有串接的 client、串接裡只有沒設 `verifies_password` 的 `lua` 處理器的 client，以及 enable 請求，CHAP 和 MS-CHAPv2 照舊回 RESTART，設備可以改用 PAP：腳本沒有可以比對的密碼。
- **不支援的驗證方式**（ARAP、MS-CHAP，以及上一項說的情況）的 START 回 RESTART，不執行串接或腳本，設備可以改用 PAP 或 ASCII 重試；不支援 RESTART 的設備會當成 FAIL（RFC 8907）。改密碼這類非 LOGIN 動作與被拒的 enable 仍回 FAIL。
- **設了 `authorize_group` 的 client，授權不執行腳本，** 由授權群組的規則決定（2.3 節），腳本只處理它的認證和計費。
- 授權接受時，`set_attr` 加入的屬性以 `name=value` AV pair 回傳（PASS_ADD），每個 AV pair 不可超過 255 個位元組，一個回覆最多 255 個 AV pair，超過時回 ERROR 並記錄。
- 計費腳本回傳 `'accept'` 時回 SUCCESS，出錯或回傳其他值時回 ERROR，連線維持開啟。
- **給使用者的訊息 `server_msg`：** 腳本 `set_attr('server_msg', '...')` 時，這段文字放進回覆的 `server_msg` 欄位，設備通常會顯示給使用者（RFC 8907）。認證的 PASS 和 FAIL、授權的 PASS_ADD 和 FAIL、計費的 SUCCESS 和 ERROR 都可以帶；授權時它不會變成 AV pair。
  - 認證走串接時，處理器（例如 `lua` 處理器）設的 `server_msg` 只有接受時會帶：串接拒絕時不保留任何屬性。
  - 超過 1024 octets 時不送並記錄錯誤，決定照舊。內容原樣送出，建議只用 ASCII。
  - 授權群組的規則不帶 `server_msg`。

### 4.7 範例

```lua
-- 計費：記錄工作階段，回覆不受影響
if phase == 'accounting' then
  local id = get_attr(protocol == 'radius' and 'Acct-Session-Id' or 'task_id')
  if id then
    sql_execute("INSERT INTO acct (id, ts) VALUES (?, ?)", {id, math.floor(now_unix())})
  end
  return 'accept'
end

-- TACACS+ 授權：只允許 show 指令
if phase == 'authorization' then
  return get_attr('cmd') == 'show' and 'accept' or 'reject'
end

-- 認證（沒有 [auth] 時）：查詢資料表
local user = protocol == 'radius' and get_attr('User-Name') or get_attr('user')
local rows = user and sql_query("SELECT 1 AS ok FROM allowed WHERE name = ?", {user}) or {}
if #rows == 1 then
  if protocol == 'radius' then
    set_attr('Filter-Id', 'std.ppp')
  end
  return 'accept'
end
return 'reject'
```

腳本由管理者撰寫並信任其中的 SQL。請求內容請用 `params` 傳入（見 4.3 節），不要組進 SQL 字串。

### 4.8 中斷連線與變更權限（CoA）

中斷連線與變更權限（`send_disconnect`、`send_coa`、`disconnect` 和 `coa` 指令、`[radius.coa]` 的轉送）需要 rradius-enterprise。

### 4.9 記錄 `log`

```lua
log('quota left', left, 'for', get_attr('User-Name'))
```

以 `info` 等級寫一行記錄：`script /etc/rradius/auth.lua (radius/accounting, client nas1): quota left 42 for bob`。`level` 設成 `warn` 或 `error` 時不會寫出（見 2.10 節）。

- 參數轉成字串，以空白相接；換行改成空白；超過 4096 octets 就截斷。
- 不會影響腳本的決定，也沒有頻率限制，寫太多會塞滿記錄。
- **不要記錄密碼。** `get_attr('User-Password')` 和 TACACS+ 的 `data` 都是明文。

### 4.10 範例腳本：配額、同時上線、IP 配發

這些在 `rradius-server/examples/` 下，開頭有需要的資料表，測試也直接執行這幾份腳本。

| 腳本 | 用途 | 要知道的限制 |
| --- | --- | --- |
| [`quota-and-hours.lua`](../rradius-server/examples/quota-and-hours.lua) | 累計時間配額與可登入時段（UTC），回覆 `Session-Timeout` | 不是硬限制：時間在 Stop 時才計入，`Session-Timeout` 是登入當下剩餘的時間，所以同時登入兩次可以用到兩倍；需要硬限制時，搭配上限設為 1 的 `simultaneous-use.lua`。`quota_sessions` 就是用量，刪除某個使用者的列就重新計算，可以用來每月重置；重置前，NAS 重開機後重複使用的 session ID 會被當成重送而不計入 |
| [`simultaneous-use.lua`](../rradius-server/examples/simultaneous-use.lua) | 限制同時上線數，整個叢集共用 | 不是精確值：從 Accounting-Start 才算一個連線，同時登入可能都通過。NAS 要定期送 Interim-Update，否則連線逾時後就不算了。舊的列隨時可以刪 |
| [`mac-auth.lua`](../rradius-server/examples/mac-auth.lua) | MAC 認證：`User-Name` 和 `Calling-Station-Id` 要是同一個位址，再查資料表，回覆 VLAN | MAC 位址不是秘密，任何人讀得到就能冒用，只適合沒有其他方式的設備，VLAN 要限制能連到的地方。只比對名單，不是認證 |
| [`ip-pool.lua`](../rradius-server/examples/ip-pool.lua) | 配發 `Framed-IP-Address`，不會同時發給兩個人 | NAS 要定期送 Interim-Update 延長租約，並在計費封包中帶回這個位址和 `User-Name`，只有持有者的封包才算數；沒有 `User-Name` 或內容為空的請求會被拒絕；同一個使用者又拿回同一個位址時，NAS 重送舊的 Stop 仍會釋放它；快用完時，副本落後可能讓少數請求被拒 |

`quota-and-hours.lua` 和 `simultaneous-use.lua` 的計費封包要帶 `User-Name` 和 `Acct-Session-Id`。缺少或內容為空時不會計入，也就不受限制，腳本會用 `log()` 記一行；請確認 NAS 有送這兩個屬性。兩份腳本的資料表都以 client 加 session ID 為鍵，所以不同 NAS 用同一個 ID 不會互相影響。

計費腳本都能重複執行，適合搭配計費暫存（見 2.11 節）。`simultaneous-use.lua` 和 `ip-pool.lua` 會扣掉 `Acct-Delay-Time`，舊的回報不會被當成最近的消息。

這些腳本只檢查政策，不驗證密碼。請同時設成 `[lua].script`（計費一定會走這裡），以及 `policy = "all_accept"` 串接中，`sql` 或 `ldap` 之後的 `lua` 處理器。**順序很重要：** 放在它們之前時，腳本在還沒驗證密碼時就會執行，`ip-pool.lua` 這類會寫入的腳本，可能為任意名稱佔走位址，即使請求最後被拒絕。

**寫入後立刻讀可能讀不到：** `sql_query` 讀本機副本，`sql_execute` 回傳時本機不一定已套用。三節點實測中，有些請求確實讀不到剛寫入的列。要判斷寫入是否成功，請用 `sql_execute` 回傳的受影響列數，不要再查一次。`ip-pool.lua` 就是這樣做的。

### 4.11 呼叫 HTTP API `http_request`

腳本可以呼叫外部的 HTTP API，但要先在設定列出允許的主機。沒有 `[lua.http]` 時，`http_request` 和 `url_encode` 都不存在。

```toml
[lua.http]
allowed_hosts = ["api.example.com", "billing.internal:8443", "http://legacy.internal:8080"]
timeout_ms = 2000            # 預設，從連線到讀完回應
max_response_bytes = 1048576 # 預設
max_concurrent = 64          # 預設，每台主機，計費和其他階段分開算
ca_file = "internal-ca.pem"  # 選填，系統根憑證之外信任的 CA
```

```lua
local response, problem = http_request{
  url = 'https://api.example.com/users/' .. url_encode(get_attr('User-Name')),
  method = 'GET',                             -- 預設；GET、POST、PUT、DELETE、PATCH
  headers = { Authorization = 'Bearer ...' }, -- 選填
  body = '...',                               -- 選填
}
if not response then
  log('billing API:', problem)
  return 'reject'
end
return response.status == 200 and 'accept' or 'reject'
```

- **回傳：** 有收到回應就是 `{status=, body=, headers=}`，4xx、5xx 也算，標頭名稱是小寫。被拒絕、逾時、連線失敗、回應太大、等待的請求太多時回傳 `nil` 和訊息，由腳本決定要拒絕還是放行。參數寫錯才是 Lua 錯誤。
- **白名單：** 完全比對主機和埠，不支援萬用字元。只寫主機時只允許預設埠。只允許 HTTPS，要用純文字的主機要寫成 `http://host:port`，只對那一項有效。
- **不跟隨轉址，也不走環境變數的代理。** 腳本會拿到 3xx，要跟隨就再呼叫一次，會再比對一次白名單。
- **一律用 `url_encode` 把外部的值接進 URL。** 它遇到空字串、`.`、`..` 會讓腳本出錯，帶 `.`、`..` 路徑、頭尾有空白或控制字元的 URL 也會被拒絕，否則登入名稱 `..` 可以走到同一台主機的其他路徑。**只靠 `status == 200` 判斷帳號存在時要特別小心。**
- **不能設定** `Host`、`Content-Length`、`Transfer-Encoding`、`Connection`、`Upgrade`、`TE`，否則會出錯。
- **逾時要小於設備的重送逾時，** 也就是腳本等待時，設備不會一直重送。`max_execution_ms` 不包含等待，所以連續呼叫多次時，總時間沒有上限，和多次 `sql_query` 相同。
- **DNS：** 白名單比對的是名稱，不檢查解析出來的位址，所以 DNS 被竄改時，請求可能送到內網。名稱查詢同時最多 16 個（登入 12 個、計費 4 個，估計值），超過時排隊，等待時間算在 `timeout_ms` 內。
- **記錄不會寫路徑、查詢字串、標頭或內容，** 只有主機和固定的錯誤種類。腳本自己用 `log()` 寫的內容，要自己注意不要放金鑰。
- 有 HTTPS 主機但這台機器沒有任何信任的根憑證時，啟動失敗，請設定 `ca_file`。設定改動隨 SIGHUP 生效。
- 指標：`rradius_script_http_requests_total{result="ok|timeout|error|denied|too_large|busy"}`。

## 5. 密碼格式

`sql` 處理器的密碼欄與 `ldap` 處理器的 `attribute` 模式支援：

| 格式 | 說明 |
| --- | --- |
| 明文 | 不以 `{` 開頭的值 |
| `{clear}` | 明文；明文本身以 `{` 開頭時必須加上這個前綴 |
| `{crypt}` | crypt(3) 雜湊 |
| `{SHA}` | Base64 的 SHA-1 |
| `{SSHA}` | Base64 的加鹽 SHA-1 |
| `{nthash}` | NT hash（十六進位） |
| `{argon2}` | PHC 字串 |
| `{MD5}`、`{SHA224}`、`{SHA256}`、`{SHA384}`、`{SHA512}` | Base64 的雜湊，**只用於遷移** |
| `{SMD5}`、`{SSHA224}`、`{SSHA256}`、`{SSHA384}`、`{SSHA512}` | Base64 的「雜湊 + 鹽」，**只用於遷移** |
| `{PBKDF2-SHA1}`、`{PBKDF2}`、`{PBKDF2-SHA256}`、`{PBKDF2-SHA512}` | `<次數>$<鹽>$<雜湊>`，鹽和雜湊是 adapted Base64（`.` 代替 `+`，沒有 padding），和 OpenLDAP 的 pw-pbkdf2 相同（以它實際產生的值測試過）。`{PBKDF2}` 和 `{PBKDF2-SHA1}` 一樣是 SHA-1，雜湊 20 octets。**只用於遷移** |
| `{X-PBKDF2}` | `<演算法>:<次數>:<鹽>:<雜湊>`，演算法是 `HMACSHA1` 或 `HMACSHA2+224`、`+256`、`+384`、`+512`，次數是 4 octets 大端數的 Base64，鹽和雜湊是 Base64（有沒有 padding 都可以），雜湊長度等於演算法的輸出長度。和 PBKDF2 一樣在處理請求的執行緒之外驗證，**只用於遷移** |
| `{NS-MTA-MD5}` | 舊郵件伺服器的格式：32 個十六進位字元的 MD5(鹽、0x59、密碼、0xF7、鹽)，後面接 32 個字元的鹽，**只用於遷移** |

這些遷移用的格式是為了從舊系統搬帳號時不必重設密碼，`{MD5}`、`{NS-MTA-MD5}`、沒有鹽的 SHA-2 和以 SHA-1 推導的 PBKDF2 特別弱，新帳號請用下面建議的格式。PBKDF2 的次數由雜湊本身決定，會在處理請求的執行緒之外驗證，太高時認證會變慢。格式錯誤的值一律不符。

CHAP 只能搭配明文或 `{clear}`；PAP 支援全部格式。比對使用固定時間比較。儲存值是空字串或 `{clear}` 後面沒有內容時，一律視為沒有密碼，任何密碼都不符。

`{crypt}` 依雜湊內容判斷演算法，包含 DES、`$1$`（MD5）、`$5$`（SHA-256）、`$6$`（SHA-512）與 bcrypt（`$2a$`、`$2b$`、`$2y$`）。新部署建議使用 `{argon2}` 或 `{crypt}` 的 bcrypt、`$6$`。帳號在叢集的資料表時，`rradius-server user add` 和 `user passwd` 會依 `[accounts]` 的 `scheme` 產生 bcrypt、`$6$`、`{nthash}` 或 `{clear}`（2.14 節）；其他情況請用現成工具，例如 `argon2` 指令或 `htpasswd -B`。

`{crypt}` 和 `{argon2}` 的驗證刻意很慢，所以會在處理請求的執行緒之外執行，同時執行數不超過 CPU 核心數，其餘排隊等待。`sql` 處理器的 `timeout_ms` 涵蓋排隊與驗證，逾時視為拒絕。`{argon2}` 的成本由雜湊本身的參數決定，設得太高時認證會變慢，也會用掉更多記憶體。

### 5.1 MS-CHAPv2

`sql` 處理器和 `ldap` 處理器的 `attribute` 模式支援 MS-CHAPv2（RFC 2759），成功時回覆 `MS-CHAP2-Success` 和 `MS-MPPE-Send-Key`、`MS-MPPE-Recv-Key`，失敗時回覆帶 `E=691 R=0` 的 `MS-CHAP-Error`。

- **只能搭配明文、`{clear}` 或 `{nthash}`。** 其他格式都會拒絕，因為這個協定需要 NT hash。
- **MS-CHAPv2 本身很弱：** 擷取到的交換可以離線破解，而且 NT hash 外洩就等於密碼外洩。請只用在加密通道內，能用其他方式就不要用它。
- **使用者名稱：** 驗證時只用最後一個反斜線之後的部分（`BIGCO\alice` 用 `alice`）。查帳號時預設用完整的 `User-Name`，要忽略網域請設定 `strip_realm`（見 3.6 節），或自行寫在查詢裡。正規化只影響查帳號，驗證仍用收到的名稱。
- **不支援：** `ldap` 處理器的 `bind` 模式、最上層腳本和 `lua` 處理器、MS-CHAPv1。EAP-TLS 不用密碼，PEAP、TTLS 內層的 MS-CHAPv2 見 2.12 節。

## 6. 範例：VPN 閘道走 LDAP、其他設備走 SQL

```toml
dictionary = "/usr/share/rradius/dictionary"

[lua]
script = "policy.lua"

[cluster]
data_dir = "/var/lib/rradius"
secret_raft_file = "raft.secret"
secret_api_file = "api.secret"

[auth]
[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ?"
reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"

[auth.chains.vpn]
policy = "all_accept"
[[auth.chains.vpn.handlers]]
type = "ldap"
urls = ["ldaps://dc1.example.com"]
bind_dn = "cn=radius,ou=services,dc=example,dc=com"
bind_password_file = "ldap.secret"
base_dn = "ou=people,dc=example,dc=com"
filter = "(uid={user})"
[[auth.chains.vpn.handlers]]
type = "lua"
script = "vpn_hours.lua"     # 例如只在上班時間回傳 'accept'

[radius]
[[radius.clients]]
name = "switches"
address = "10.0.0.0/16"
secret_file = "switches.secret"

[[radius.clients]]
name = "vpn-gw"
address = "10.1.0.1"
secret_file = "vpn-gw.secret"
auth = "vpn"
```

## 7. 互通性

下列組合用互通測試實際測過。這套測試和企業版的功能一起放在 rradius-enterprise 的 repo，核心的 CI 不跑。

| 項目 | 對端 |
| --- | --- |
| UDP 認證、計費、Status-Server | FreeRADIUS 3.2.10 的 `radclient` |
| 延伸屬性、`long-extended` 分段、EVS、TLV、WiMAX 廠商格式，收和送 | FreeRADIUS 3.2.10 的 `radclient`。它不會送 `IPv6-6rd-*`，收到時也只顯示編號，所以這組只驗證了回應的內容 |
| TACACS+ ASCII 與 PAP 登入 | Python `tacacs_plus` 2.6 |
| EAP-TLS（TLS 1.2、1.3），拒絕其他 CA、撤銷的憑證和不符的名稱 | wpa_supplicant 的 `eapol_test` |
| PEAP-MSCHAPv2：`sql` 的明文和 `{nthash}`、`ldap` 的 `attribute` 模式、錯誤密碼、不存在的名稱、匿名外層名稱、拒絕 TLS 1.3 | wpa_supplicant 的 `eapol_test` |
| EAP-TTLS：內層 PAP（`sql` 的明文和 `{argon2}`、`ldap` 的 `bind` 模式）和內層 MS-CHAPv2（明文和 `{nthash}`）、錯誤密碼、不存在的名稱、匿名外層名稱、拒絕 TLS 1.3 | wpa_supplicant 的 `eapol_test` |
| Linux 主機登入：對和錯的密碼、不存在的名稱、Access-Challenge 答對和答錯、錯的共用密碼 | FreeRADIUS 的 `pam_radius`（4.0.0 的 commit `36cf87d`）加 `pamtester`，開啟它的 `require_message_authenticator` |

**沒有測過：** 任何實體 NAS、路由器或交換器。各廠商的實作可能不同，上線前請實際測一次。`tacacs_plus` 的 ASCII 流程會在 START 帶帳號，所以 GETUSER 只有專案自己的測試涵蓋。

**主機登入：** 要讓 Linux 主機用 rradius 登入，用 FreeRADIUS 的 `pam_radius`，rradius 不提供自己的 PAM 或 NSS 模組。設定時注意：

- **開啟 `require_message_authenticator`，** 這是它的選項，預設關閉，不開就不會要求回應帶這個屬性。
- **只能走 UDP，** 密碼用共用密碼隱藏。請用夠長的隨機密碼，並只在信任的網路上使用。
- **它只負責認證，不提供帳號。** `pam_radius` 本身不查本機帳號，測試也沒有建帳號；但 `sshd` 這類程式要先查得到使用者，所以帳號要另外準備。RADIUS 沒有查詢使用者的方法，NSS 模組幫不上這個忙。
- **`/etc/services` 沒有 `radius` 時要寫明埠，** 例如 `172.30.0.10:1812 secret 3`，否則會回 `Authentication service cannot retrieve authentication info`。最小化的容器映像常常沒有這個檔案。

互通測試只用 `pamtester` 測到 PAM 這一層，沒有測過 `sshd`、`sudo` 或 `login`。

**模糊測試：** `rradius-server/fuzz/` 用 `cargo-fuzz` 測 RADIUS 封包、TACACS+ 封包、EAP 封包、字典和位址屬性文字。CI 的 `fuzz` 項目只回放 `seeds/` 的種子，`.github/workflows/fuzz.yml` 每週對每個目標各跑 300 秒。需要 nightly 工具鏈，要在本機跑時：

```bash
rustup toolchain install nightly-2026-11-05 --profile minimal
```

```bash
cargo install cargo-fuzz --version 0.13.2 --locked
```

```bash
cd rradius-server/fuzz && mkdir -p corpus/radius_packet && cargo +nightly-2026-11-05 fuzz run radius_packet corpus/radius_packet seeds/radius_packet -- -max_total_time=120
```

找到問題時，導致當機的輸入會存在 `fuzz/artifacts/<目標>/`。修正後把它加進 `fuzz/seeds/<目標>/`，CI 就會一直檢查。

## 8. 疑難排解

下表比對的是訊息本身。每行前面還有時間和等級，`json` 格式則在 `message` 欄位裡（見 2.10 節）。

| 現象 | 可能原因 |
| --- | --- |
| 啟動時狀態碼 2 | 設定錯誤；錯誤訊息會指出檔案與欄位 |
| ``moved to `[lua].script` `` | 舊設定檔，把最上層 `script` 移到 `[lua]` |
| `cannot compile script '<檔案>'` | 腳本語法錯誤 |
| `script exceeded the execution limit of N ms` | 腳本執行太久；檢查迴圈，或調高 `[lua].max_execution_ms` |
| `memory error: not enough memory` | VM 超過記憶體上限；檢查腳本有沒有留住大量資料，或調高 `[lua].max_memory_mb` |
| `sql_query timed out` / `sql_execute timed out` | 資料庫太慢或叢集沒有多數節點；調高 `query_timeout_ms` 或檢查叢集 |
| `sql_execute: non-deterministic ...` | 寫入中使用了時間或亂數函式；改用 `now_unix()` |
| `Lua script did not accept accounting ...` | 計費腳本沒有回傳 `'accept'`，設備會重送；確認記錄成功時有回傳 `'accept'` |
| `... failed: the chain did not accept the inner name` | PEAP 或 TTLS 密碼錯誤、查無使用者。PEAP 和 TTLS 的內層 MS-CHAPv2 也可能是 `ldap` 處理器用了 `bind` 模式；TTLS 的內層 PAP 可以用 `bind` |
| `... failed: the chain accepted the inner name without checking a password` | 串接接受了，但沒有處理器比對過密碼，例如只有 `lua` 處理器。TTLS 的內層 PAP 要有 `sql`、`ldap` 或設了 `verifies_password = true` 的 `lua` 處理器接受 |
| `... failed: this client has no chain to check a password with` | PEAP、TTLS 需要串接，這個 client 沒有 |
| `... failed: an MS-CHAP challenge other than the one the tunnel exports` | TTLS 的內層 MS-CHAPv2 用了不是通道匯出的 challenge，可能是用戶端不相容，也可能是轉送攻擊 |
| `... failed: an unrecognized mandatory inner AVP` | TTLS 用戶端的內層方法不是 PAP 或 MS-CHAPv2，例如內層 EAP 或 CHAP |
| `EAP conversation with client ... failed` | 看後面的原因：憑證、`State` 找不到（常見於切換節點）、TLS 版本或加密套件不符（2.12 節） |
| `EAP conversation with client ... refused` | 憑證通過，但名稱不符或串接、腳本沒有接受。使用 LDAP 時，確認設了 `certificate_filter`；使用 SQL 時，確認第一欄有值或設了 `certificate_query` |
| `accounting spool: request from client ... not written` | 暫存已滿或磁碟寫入失敗，這筆沒有回覆，由設備重送；檢查磁碟與 `max_bytes`（2.11 節） |
| `accounting spool: request from client ... moved to rejected.log` | 重放時放棄這筆，原因寫在後面；檔案不會自動清理（2.11 節） |
| `accounting spool: cannot read`、`cannot record progress` | 讀暫存檔或寫進度失敗，檢查磁碟和權限。之後會重試，可能重放同一筆 |
| `accounting spool: removed ... octets of an unfinished write` | 啟動時截掉寫到一半的內容，有內容時保留在 `truncated.log` |
| `accounting spool: could not remove an unfinished write` | 啟動時截不掉，那個檔案之後會整個改名為 `.corrupt` |
| `accounting spool: ... damaged files set aside` | 損壞的檔案改名為 `.corrupt`，裡面的請求不會重放 |
| `rradius_acct_spool_bytes` 一直不下降 | 資料庫還沒恢復，或腳本仍然失敗；看記錄中的 `Lua script` 開頭訊息 |
| 設備一直重送計費封包 | 計費腳本出錯或沒有回傳 `'accept'`；看記錄中的 `Lua script` 開頭訊息 |
| 腳本中 `io`、`os.execute`、`require`、`loadstring`、`getfenv`、`setfenv` 出錯 | 沙箱不提供這些函式 |
| `lacking a Message-Authenticator` | 設備沒送這個屬性；升級設備，或把該 client 的 `require_message_authenticator` 設成 `no` 或 `auto`（見 2.6 節）|
| RADIUS 沒有回應 | 來源位址不在 `[[radius.clients]]` 中、共用密碼不符、請求沒帶 Message-Authenticator，或已達同時處理上限 |
| `RADIUS ... overloaded: discarded N packets` | 已達 `max_auth_inflight` 或 `max_acct_inflight`；通常要先找出處理變慢的原因（LDAP、資料庫），再考慮調高上限 |
| `TACACS+ overloaded: closed N connections` | 已達 `max_connections` |
| `cluster was not ready to apply migrations` | 叢集沒有在 `migrations_wait_ms` 內就緒、SQL 檔執行太久，或本機副本沒有追上；啟動其他節點，或調高這個值。節點反覆以狀態碼 1 結束時，通常是這個值比最長的 SQL 檔執行時間小 |
| `migration file '...' ...` | 見 2.5 節：檔名、編號、修改過已套用的檔案，或 SQL 出錯 |
| `reload failed, keeping the running configuration: ...` | 重新載入失敗，服務維持原狀；修正訊息指出的問題後再送一次 SIGHUP |
| `these need a restart to apply: ...` | 列出的設定改了但沒有套用，見 2.9 節；重啟前每次重新載入都會列出 |
| 結尾是 `(N since the last line)` | 未知來源的 RADIUS 封包和 TACACS+ 連線，這些任何人都能觸發，所以每種每秒最多一行。這行只寫當下那一筆的來源，N 是上一行以來的總數，其他來源要看指標或抓封包 |
| `accepting a connection failed N times` | 通常是檔案描述元用完；程序會自動重試，不必重啟。請調高 `ulimit -n`，或調低 `max_connections`（預設 1024） |
| `auth chain: rejecting a request from client ...` | 名稱處理後是空的，或不符合 `allowed`，見 3.6 節 |
| `script ... : ...` | 腳本用 `log()` 寫的記錄，不是錯誤 |
| `RADIUS response attribute error: ...` | 回應屬性的值不符合字典型別，例如 `reply_query` 查到 `2001:db8::1/64` 當前綴。該次認證會被拒絕，即使密碼正確；修正資料表的值 |
| `rradius_auth: authentication ... decision="reject"` | 認證紀錄。`handler` 是做出決定的處理器；`none` 表示全部略過，`no_password_check` 表示 `all_accept` 沒有人比對過密碼，`normalization` 表示名稱被拒絕 |
| `auth chain: rejecting a request from client ...: all_accept without checking a password` | `all_accept` 裡的 `sql`、`ldap` 都沒有接受，只有腳本說可以，或腳本在它們接受之前就回 `challenge`。通常是名稱不在資料表裡。憑證也一樣，見 3.2 節 |
| `Raft node has not been initialized` | 節點還沒加入叢集，加入後就會停止；一直出現時，檢查其他節點是否啟動 |
| `rradius_log_dropped_lines_total` 持續增加 | 記錄輸出跟不上或寫入失敗，有記錄遺失；檢查 stderr 接到哪裡，或降低 `level`、關閉 `auth` |
| TACACS+ 設備登入一直回 ERROR | 同一個會談的封包順序不對、連線上未完成的登入超過 8 個，或登入超過 60 秒 |
| LDAP 認證全部被拒 | 檢查記錄中 `auth ldap handler:` 開頭的訊息：連線或憑證錯誤、服務帳號登入失敗、搜尋失敗 |

## 9. 效能量測

`rradius-bench/` 是獨立的 RADIUS 與 TACACS+ 壓力測試工具，用來量伺服器的吞吐和延遲。它不在 Docker 映像裡，要自己建置。

各功能已量過的數字寫在對應的設定說明裡：三節點叢集、讀取連線和快照在 2.2 節，多個接收 socket、綁核和計費寫入在 2.4 節，認證紀錄在 2.10 節，計費暫存在 2.11 節，TACACS+ 授權群組和腳本的比較在 2.3 節。

**只用測試密語。** `--secret` 會出現在程序列表，範例設定的密語和帳號也都寫在檔案裡，而且一律接受登入，不要拿去對正式環境或放上正式伺服器。

### 建置

```bash
cargo build --release --locked --manifest-path rradius-bench/Cargo.toml
```

```bash
cargo build --release --locked --manifest-path rradius-server/Cargo.toml
```

### 範例設定

`rradius-server/benchmarks/` 是量測用的伺服器設定，都只綁 `127.0.0.1`，密語是 `bench-secret`，指標埠是 9812。工具只負責送請求，伺服器要另外啟動。EAP-AKA' 和 RadSec 的設定（`aka.toml`、`radsec.toml`）在 rradius-enterprise 的 `benchmarks/`。

| 設定 | 計費腳本做的事 |
| --- | --- |
| `accept.toml` | 不寫任何東西，一律接受 |
| `acct-sql.toml` | 每次寫一筆資料庫 |
| `acct-cpu.toml` | 每次純運算；單獨跑時約 30 ms，換機器要調整 `acct-cpu.lua` 的迴圈次數 |
| `eap.toml` | 一律接受計費；EAP-TLS、PEAP 和 TTLS 給帳號 `bench`（密碼 `bench`），見下方〈EAP 登入〉 |
| `tacacs.toml` | 只開 TACACS+，埠 4949，認證、授權、計費一律接受，見下方〈TACACS+〉 |

在 `rradius-server/benchmarks/` 目錄啟動伺服器，資料會放在該目錄的 `data/`（已被 gitignore）：

```bash
cd rradius-server/benchmarks && ../../target/release/rradius-server --config accept.toml
```

換一組設定前請先停掉伺服器。下面的命令在專案根目錄的另一個終端機執行。

量最大吞吐，搭配 `accept.toml`。這個量測的延遲大多是排隊，不要拿來看延遲：

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --metrics-url http://127.0.0.1:9812/metrics --load pap,concurrency=64,window=1
```

[計費負載表](#24-過載與重送)的五列依序如下。無計費負載，搭配 `accept.toml`：

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --metrics-url http://127.0.0.1:9812/metrics --load pap,rate=100
```

每次寫一筆資料庫，併發 256 和 4096，搭配 `acct-sql.toml`：

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --metrics-url http://127.0.0.1:9812/metrics --load acct-start,concurrency=256 --load pap,rate=100
```

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --metrics-url http://127.0.0.1:9812/metrics --load acct-start,concurrency=4096 --load pap,rate=100
```

每次約 30 ms 的純運算，併發 256 和 4096，搭配 `acct-cpu.toml`。這個負載排隊久，所以預熱和逾時都要拉長：

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --metrics-url http://127.0.0.1:9812/metrics --load acct-start,concurrency=256 --load pap,rate=100 --warmup 15 --duration 20 --timeout 60
```

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --metrics-url http://127.0.0.1:9812/metrics --load acct-start,concurrency=4096 --load pap,rate=100 --warmup 15 --duration 20 --timeout 60
```

### EAP 登入

`eap-tls`、`peap`（內層 EAP-MSCHAPv2）和 `ttls`（內層 PAP）負載跑完整的 EAP 登入：一次登入算一個請求，從第一個 Access-Request 算到 Access-Accept 或 Access-Reject，報告的 `rounds` 是每次登入平均幾個 Access-Request。Access-Accept 帶的 MS-MPPE 金鑰要和 TLS 匯出的金鑰相同才算 `accept`，不同就算 `invalid`，所以算到的都是真的完成的登入。

- **選項。** `--cert-file`、`--key-file` 是 `eap-tls` 登入用的憑證；`--tls-version` 是 `1.2`（預設）或 `1.3`，`1.3` 只能用在 `eap-tls`；`--eap-fragment` 是工具每段送出的 TLS 資料上限（預設 1000 octets）。EAP 一律帶 Message-Authenticator，不能配 `--no-message-authenticator`。
- **伺服器憑證預設不檢查。** 壓測對象多半用自己做的憑證，所以預設接受任何伺服器憑證（仍檢查握手的簽章）；給 `--ca-file` 才檢查，名稱用 `--tls-name`（預設是 `--server`）。不要拿這個預設去連正式環境。
- **伺服器提供別的方法時，** 工具回一次 Nak 要自己的方法，這多一個來回。`eap.toml` 先提供 EAP-TLS，所以 PEAP 和 TTLS 的 `rounds` 各多 1。
- **工具也要做 TLS 握手，** 尤其 `eap-tls` 每次都要用私鑰簽一次。`tool_saturated` 警告出現時，把伺服器限定在較少的 CPU，或給工具更多執行緒；`concurrency` 小時記得同時調小 `window`，否則 socket 只有一個，工具也只用一個執行緒（例如 `concurrency=64,window=8,threads=7`）。

先在專案根目錄做 `eap.toml` 要的憑證（`eap-certs/` 已被 gitignore）：

```bash
mkdir -p rradius-server/benchmarks/eap-certs && cd rradius-server/benchmarks/eap-certs
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj /CN=bench-ca -keyout ca.key -out ca.pem
openssl req -newkey rsa:2048 -nodes -subj /CN=localhost -keyout server.key -out server.csr
printf 'subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n' > server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -extfile server.ext -out server.pem
openssl req -newkey rsa:2048 -nodes -subj /CN=bench -keyout client.key -out client.csr
printf 'subjectAltName=DNS:bench\nextendedKeyUsage=clientAuth\n' > client.ext
openssl x509 -req -in client.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -extfile client.ext -out client.pem
cd ../../..
```

用戶端憑證的名稱 `bench` 要和登入的名稱相同（`match_identity`）。然後在 `rradius-server/benchmarks/` 以 `eap.toml` 啟動伺服器，再從專案根目錄分別跑三種負載：

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --metrics-url http://127.0.0.1:9812/metrics --load eap-tls,concurrency=64,window=8 --cert-file rradius-server/benchmarks/eap-certs/client.pem --key-file rradius-server/benchmarks/eap-certs/client.key
```

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --metrics-url http://127.0.0.1:9812/metrics --load peap,concurrency=64,window=8
```

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --metrics-url http://127.0.0.1:9812/metrics --load ttls,concurrency=64,window=8
```

2026-11-08 在同一台 Apple M4 Max 上 Docker 的 Linux（arm64）量測，伺服器限定 CPU 0–6、工具限定 7–13（`threads=7`），`eap.toml`，TLS 1.2，各兩次。先是 RSA 2048 的憑證（上面的指令）：

| 方法 | 每次登入的來回 | 併發 64，`window=8`（次/秒） | 併發 512（次/秒） | 每次登入的伺服器 CPU |
| --- | --- | --- | --- | --- |
| EAP-TLS | 6 | 8,410–8,660，p99 13 ms | 9,100–9,380，p99 90–92 ms，每輪 21–41 個逾時 | 約 690–740 µs |
| PEAP | 10（含一次 Nak） | 8,770–8,900，p99 12.6–12.9 ms | 8,890–9,090，p99 81–84 ms，一輪 17 個逾時 | 約 730–780 µs |
| TTLS | 6（含一次 Nak） | 10,630–10,710，p99 12 ms | 10,880–10,930，p99 74–75 ms | 約 630–660 µs |

同樣的條件換成 ECDSA P-256 的憑證（伺服器和用戶端都換），併發 64、`window=8`，和 RSA 在同一個容器裡接連跑：

| 方法 | RSA 2048（次/秒） | ECDSA P-256（次/秒） | 每次登入的伺服器 CPU，RSA → ECDSA | 每次登入的來回，RSA → ECDSA |
| --- | --- | --- | --- | --- |
| EAP-TLS | 7,230–8,590，p99 13–22 ms | 24,080–24,730，p99 5.3–5.5 ms | 約 700–790 → 265–267 µs | 6 → 4 |
| PEAP | 7,850–8,620，p99 13–16 ms | 18,320–18,500，p99 6.0–6.1 ms | 約 776–777 → 350–352 µs | 10 → 9 |
| TTLS | 9,780–10,140，p99 13–14 ms | 28,940–29,340，p99 4.2–4.3 ms | 約 644–656 → 219–220 µs | 6 → 5 |

- **伺服器的 CPU 用滿了：** 併發 64 起，7 顆 CPU 幾乎全滿（每 10 秒用掉 56–69 CPU 秒），所以 RSA 2048 時每秒約 9 千到 1 萬 1 千次就是上限，提高併發只會增加延遲和逾時。
- **一次 EAP 登入的 CPU 遠多於 PAP：** RSA 2048 時一次登入約 0.6–0.8 ms，同樣環境下一律接受的 PAP 約 18 µs（rradius-enterprise 使用文件 2.13 節的量測），差在每次都是完整的 TLS 握手。
- **大部分是 RSA 簽章：** 換成 ECDSA P-256，每次登入的 CPU 降到約三分之一，吞吐約 2 到 3 倍；憑證變小，伺服器那一輪的分段也少，所以少一個來回。PEAP 仍比 TTLS 多約 130 µs，可能是內層 MS-CHAPv2 的雜湊和多出的來回，沒有確認。
- **ECDSA 憑證的指令：** 把上面每個 `-newkey rsa:2048` 換成 `-newkey ec -pkeyopt ec_paramgen_curve:prime256v1`。要用 OpenSSL 3：macOS 內建的 LibreSSL 產生的 EC 私鑰 rradius 讀不了（`failed to parse private key`）。
- **工具的記憶體：** 併發 512 時約 15 MB。
- 數字只適合比較，實際設備還有無線、重送和使用者數量的差別。

### UDP

UDP 的接收迴圈也用和 RadSec 相同的方式：收到一個封包後，先不等待地試收下一個；下一個已經在了，就把這個交給新 task，讓同一個 socket 的請求分到多個執行緒；沒有的話，先在接收迴圈處理一次，不必等待就直接送出回應，要等資料庫、LDAP、HTTP 或上游的才交給新 task。名額持有到回應送出為止，接收迴圈不等待送出（送不出去時交給新 task），過載丟棄和重送偵測不變。

2026-11-09 同樣環境（伺服器 CPU 0–6、工具 7–13，`radsec.toml`，計費用經資料庫寫入一列的腳本，PEAP 用 `eap.toml`），和改動前（`main` `5dc1c95`）在同一次量測裡對照，各兩輪，都沒有逾時：

| 負載 | 改動前 次/秒 | 改動後 次/秒 | 每個請求的伺服器 CPU |
| --- | --- | --- | --- |
| `pap,concurrency=64,window=64`（1 個來源 socket） | 188,200–196,500 | 230,900–238,700 | 約 21.5 → 18–18.5 µs |
| `pap,concurrency=64,window=8`（8 個） | 237,500–257,700 | 265,300–281,000 | 約 24 → 20–21 µs |
| `pap,concurrency=256,window=4`（64 個） | 245,600–271,100 | 281,000–282,200 | 約 23–24 → 20.3–20.4 µs |
| `acct-start,concurrency=64,window=8`，經資料庫 | 68,100–69,500 | 76,300–77,100 | — |
| `peap,concurrency=64,window=8` | 8,990–9,000 | 9,270–9,370 | 約 772 → 738–740 µs |

一律在接收迴圈處理不採用：同一個 NAS 的封包都落在同一個 socket（Linux 依來源位址和埠分配），會擠在一顆核心上。

- **很吃 CPU 又不必等待的腳本會佔住接收迴圈：** 這時同一個 socket 的新封包留在系統的接收緩衝區，等它做完才讀；緩衝區滿時系統直接丟掉，不會計入 `rradius_radius_discarded_total{reason="overload"}`。只在當時沒有其他封包在等時發生，RadSec 也一樣（見上方）。
- 接收迴圈裡的處理如果 panic，只會讓那個請求沒有回應（記一行錯誤），接收迴圈照常繼續。

### TACACS+

`tacacs-pap`、`tacacs-ascii`、`tacacs-author` 和 `tacacs-acct` 負載對 `--tacacs-port`（預設 49）送 TACACS+（RFC 8907），封包用 `--secret` 加密。

| 負載 | 送什麼 | 結果 |
| --- | --- | --- |
| `tacacs-pap` | 認證 START，PAP，帶 `--user`（或 `--users`）和 `--password` | PASS 算 `accept`，FAIL 算 `reject` |
| `tacacs-ascii` | 認證 START，ASCII，不帶帳號；GETUSER 後回帳號，GETPASS 後回密碼 | 同上，三個來回算一次登入，`rounds` 為 3 |
| `tacacs-author` | 授權 REQUEST：`service=shell`、`cmd=show`、`cmd-arg=version` | PASS_ADD 或 PASS_REPL 算 `accept`，FAIL 算 `reject` |
| `tacacs-acct` | 計費 START 紀錄 | SUCCESS 算 `accounting`，ERROR（腳本沒有接受）算 `reject` |

- **一條連線一次一個請求。** rradius 一條連線依序處理，在同一條連線多送只會排隊，所以 TACACS+ 負載的 `window` 固定是 1，給其他值是錯誤的參數；連線數等於 `concurrency`，例如 `concurrency=64` 像 64 台各開一條連線的設備。
- **`--tacacs-connect keep`（預設）：** 所有連線在送負載前就連好，之後一直用；量測前連不上時什麼都不送，exit 3。伺服器關閉連線時，在等回應的請求算 `socket_errors`，工具重新連線，次數記在 `reconnects`。
- **`--tacacs-connect per-request`：** 每個請求開一條新連線，收到最後的回覆後由工具關閉，延遲包含建立連線。連不上算 `socket_errors`。
- **不重送。** 超過 `--timeout` 沒有完成算 `timeouts`（`tacacs-ascii` 是整個登入的三個來回加起來），並關閉那條連線（`keep` 會重新連線），因為晚到的回覆會被當成下一個請求的。`--retries` 不套用到 TACACS+ 負載。
- **回覆要對得上才算。** 序號和 session id 要對上，解密後各欄位的長度要和封包長度相符，不然算 `invalid`。共用密碼設錯時解出來是亂碼，就是這樣被擋下，標準錯誤會印一次 `lengths: ...`。
- **`concurrency` 不能超過伺服器的 `max_connections`**（預設 1024）。伺服器先完成 TCP 連線再立刻關閉多的連線，所以工具在量測前看不出來，兩種模式都表現為 `socket_errors`；`keep` 還會一直重連（`reconnects` 增加，還沒收到任何回覆就被關閉的連線每 100 ms 才重試一次）。`tacacs.toml` 把它調到 8192。
- **`per-request` 會在工具這端留下 TIME_WAIT。** 由工具關閉的連線，在工具端停留一段時間（Linux 約 60 秒）才放掉連接埠。Linux 預設可用的連接埠約 28,000 個（`net.ipv4.ip_local_port_range` 是 32768–60999），所以持續每秒新開超過約 470 條連線，連接埠就會用完，出現連不上的 `socket_errors`。Linux 的 `net.ipv4.tcp_tw_reuse` 預設是 2，只在 loopback 重用 TIME_WAIT 的連接埠，所以工具和伺服器在同一台時不受這個限制，連到別台才會碰到。量更高的速率要放寬範圍（例如 `sysctl -w net.ipv4.ip_local_port_range="1024 65535"`）並把 `net.ipv4.tcp_tw_reuse` 設成 1，或用 `--bind` 分到多個來源位址；`ss -s` 的 `timewait` 可以看目前有多少。macOS 的範圍是 49152–65535（約 16,000 個），TIME_WAIT 約 30 秒，上限約每秒 540 條，超過時錯誤是 `Can't assign requested address`。

在 `rradius-server/benchmarks/` 以 `tacacs.toml` 啟動伺服器，再從專案根目錄分別跑四種負載：

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --tacacs-port 4949 --load tacacs-pap,concurrency=64
```

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --tacacs-port 4949 --load tacacs-ascii,concurrency=64
```

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --tacacs-port 4949 --load tacacs-author,concurrency=64
```

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --tacacs-port 4949 --load tacacs-acct,concurrency=64
```

每個請求開新連線。連到別台或在 macOS 上，用固定速率並低於上面 TIME_WAIT 的限制；固定併發會盡快開連線，幾秒內就把連接埠用完：

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --tacacs-port 4949 --tacacs-connect per-request --load tacacs-pap,rate=300
```

2026-11-08 在同一台 Apple M4 Max 上 OrbStack 的 Linux（arm64）量測，伺服器限定 CPU 0–6、工具限定 7–13（`threads=7`），`tacacs.toml`，每列三次，都沒有逾時、`invalid`、socket 錯誤和重連：

| 負載 | 連線 | 次/秒 | p50 / p99 | 每個請求的伺服器 CPU |
| --- | --- | --- | --- | --- |
| `tacacs-pap,concurrency=64` | 64，`keep` | 313,500–323,700 | 0.18–0.19 / 0.54–0.56 ms | 約 16 µs |
| `tacacs-ascii,concurrency=64` | 64，`keep` | 140,800–142,400 次登入 | 0.42 / 1.07–1.10 ms | 約 31 µs（一次登入） |
| `tacacs-author,concurrency=64` | 64，`keep` | 336,200–371,500 | 0.16–0.17 / 0.46–0.58 ms | 約 13–14 µs |
| `tacacs-acct,concurrency=64` | 64，`keep` | 372,500–385,600 | 0.15–0.16 / 0.46–0.48 ms | 約 14 µs |
| `tacacs-pap,concurrency=512` | 512，`keep` | 413,200–430,500 | 1.06–1.10 / 3.4–3.5 ms | 約 16 µs |
| `tacacs-pap,concurrency=64` | 每個請求一條，`per-request` | 16,600–21,300 | 0.97–1.09 / 13–36 ms | 約 38–42 µs |

固定每秒 400 次時，`keep` 的延遲是 p50 0.89–0.99 ms、p99 2.8–4.0 ms，`per-request` 是 p50 1.24–1.27 ms、p99 2.7–2.9 ms。

- **保持連線時比 RADIUS 還快：** 同樣環境下 UDP 的 PAP 是每秒 246,800–265,600 次、每個請求約 22–23 µs（和 RadSec 同一次量測，見 rradius-enterprise 使用文件第 9 章〈RadSec〉），TACACS+ 的 PAP 每個請求約 16 µs。授權和計費比 PAP 再少一點。這兩點的原因都沒有確認。
- **ASCII 登入是三個來回，** 一次登入的 CPU 約是 PAP 的兩倍，吞吐不到一半。
- **每個請求開新連線貴很多：** 吞吐約是 `keep` 的 6%，伺服器每個請求多約 22–26 µs，花在接受和關閉連線。這時工具的執行緒也忙到 81–93%（兩輪有 `tool_saturated` 警告），所以這個數字有一部分是工具的上限。量測中 TIME_WAIT 累積到整個連接埠範圍（28,232 個），因為是 loopback，靠 `tcp_tw_reuse` 預設的 2 重用，沒有出現錯誤。
- **在低速率下，`per-request` 的 p50 多約 0.3 ms，** 是建立連線的來回。實際設備多半是保持連線或一段時間開一條，要看設備的設定。

### 固定併發與固定速率

`--load` 的格式是 `KIND[,concurrency=N][,rate=N][,window=N][,threads=N][,alive=N]`，可以重複，每個負載各用自己的執行緒，所以背景壓力不會讓量測登入的執行緒排隊。`KIND` 可以是 `pap`、`chap`、`chap-authenticator`、`mschapv2`、`acct-start`、`acct-alive`、`acct-stop`、`acct-on`、`acct-off`、`status-auth`、`status-acct`、`session`，EAP 的 `eap-tls`、`peap`、`ttls`、`aka-prime`（見上方〈EAP 登入〉，以及 rradius-enterprise 使用文件第 9 章〈EAP-AKA'〉），以及 TACACS+ 的 `tacacs-pap`、`tacacs-ascii`、`tacacs-author`、`tacacs-acct`（見上方〈TACACS+〉）。

- **固定併發（`concurrency=N`）用來量吞吐。** 收到回應才送下一個，所以延遲大多是排隊，不適合拿來說明延遲。
- **固定速率（`rate=N`）用來量延遲。** 依排程送出，不等回應，延遲從預定時間起算，伺服器卡住時後面的請求也會算進去。

### 看報告

- 回應一律驗證 Authenticator，沒通過的算 `invalid`，不會算成成功。
- 預設不重送，遺失就算 `timeouts`。`--retries` 可以開啟重送。
- 結束碼：0 全部成功、1 有被拒絕或收到 Access-Challenge、2 有逾時、驗證失敗或 socket 錯誤、3 引數或設定有誤。
- 下面這些警告會讓 `trustworthy` 變成 false，這次的數字不能當成伺服器的能力：

| 警告 | 意思 |
| --- | --- |
| `tool_saturated` | 工具的 worker 忙碌超過 90%，量到的是工具的上限 |
| `rate_not_met` | 固定速率沒有照排程送出，通常是 CPU 被搶走 |
| `duplicate_path` | 伺服器走了重送快取，腳本沒有執行 |

- 下面這些警告不影響 `trustworthy`，要自己判斷。`trustworthy` 為 true 也不代表伺服器沒有丟封包：

| 警告 | 意思 |
| --- | --- |
| `server_discarded_<原因>` | 伺服器丟掉了封包，例如超過額度 |
| `metrics_unavailable` | 讀不到 `--metrics-url`，所以也不知道有沒有走重送快取或丟封包 |

`--metrics-url` 會比對伺服器在量測前後的計數。伺服器要在設定中開啟 `[metrics]`。

### 工具自身的上限

```bash
target/release/rradius-bench calibrate
```

`calibrate` 對程序內建的回應器跑四種組合，量出工具本身的上限和這台機器的延遲底線。伺服器的吞吐接近這個上限，或延遲接近這個底線時，要改到另一台機器量。

### 同機量測的限制

工具和伺服器在同一台機器時會搶 CPU，數字只能用來比較同一台機器上的前後差異，不能當成真實網路的能力。尤其是計費腳本把 CPU 用滿的情境，登入延遲會把工具和伺服器的排程都算進去。要知道真實網路的數字，請把工具放在另一台機器，並用 `--bind` 指定來源位址。

**不同時間量的數字會差 5–8%。** 2026-11-09 用 `main` `dd48f9c` 重量主要負載（EAP、TACACS+、EAP-AKA' 都和上面各節一致），UDP 和 RadSec 的 PAP 比之前各節的數字低 5–8%。同一個容器裡交錯跑 PR #86 合併時的版本和 `dd48f9c` 各三輪，兩者沒有差別（UDP 26.7–28.0 萬 對 27.4–27.9 萬、RadSec 7 條連線 27.9–28.2 萬 對 28.4–28.9 萬、64 條連線 27.4–27.5 萬 對 27.0–27.4 萬次/秒），所以是那次容器的狀況，不是程式變慢。要比較兩個版本，請在同一次執行裡交錯跑，不要拿不同時間的數字比。
