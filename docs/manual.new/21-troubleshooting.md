# 21 排錯

> 本章讓你做到：
> - 照固定的順序找問題：`check` → 日誌 → trace → `status` → 指標 → `tuning`
> - 分辨「沒有回應」「拒絕」與 TACACS+ 的 ERROR，知道各自往哪裡查
> - 從一行錯誤訊息找到原因與處置
> - 處理用戶端比對失敗、共用密碼不一致、資料庫不能用、叢集沒有 leader、計費遺漏、過載、EAP 卡住這幾類常見問題

前置條件：讀過[第 03 章](03-quick-start.md)，會用 `rradius-bench` 送測試請求、用 `status` 看狀態。

本章小節：

- [排錯的順序](#排錯的順序)
- [沒有回應、拒絕、ERROR](#沒有回應拒絕error)
- [設定讀不進來](#設定讀不進來)
- [reload 失敗或沒有生效](#reload-失敗或沒有生效)
- [伺服器啟動失敗](#伺服器啟動失敗)
- [用戶端比對失敗](#用戶端比對失敗)
- [共用密碼不一致](#共用密碼不一致)
- [一直被拒絕](#一直被拒絕)
- [資料庫不能用](#資料庫不能用)
- [叢集沒有 leader](#叢集沒有-leader)
- [計費遺漏](#計費遺漏)
- [過載與 UDP 丟包](#過載與-udp-丟包)
- [EAP 卡住或失敗](#eap-卡住或失敗)
- [TACACS+ 連不上或回 ERROR](#tacacs-連不上或回-error)
- [腳本錯誤](#腳本錯誤)
- [needs rradius-enterprise](#needs-rradius-enterprise)
- [從頭走一次：新的 NAS 沒有回應](#從頭走一次新的-nas-沒有回應)
- [回報問題時要附上的資料](#回報問題時要附上的資料)
- [常見錯誤：訊息對照表](#常見錯誤訊息對照表)
- [相關章節](#相關章節)

本章的訊息都是在一台 macOS 上故意製造問題、實際跑出來的，`rradius-server` 0.1.0；少數只對照了程式碼的，會在旁邊註明。日誌行開頭的時間戳記用 `…` 代替。`os error` 後面的數字依作業系統而不同，本章的 48（位址已被使用）、61（連線被拒）是 macOS 的值，在你的系統上可能是別的數字。

## 排錯的順序

不管是什麼問題，照這個順序看，通常前三步就找到了：

| 步驟 | 指令或位置 | 看什麼 |
| --- | --- | --- |
| 1 | `rradius-server check --config <檔>` | 設定本身有沒有錯、有沒有警告 |
| 2 | 伺服器的日誌 | `WARN`、`ERROR`；每次認證一行 `rradius_auth` 認證紀錄 |
| 3 | `[log.trace]` | 指定的使用者或用戶端的完整請求與回應 |
| 4 | `rradius-server status --config <檔>` | 哪一類請求被丟棄、資料庫與叢集的狀態 |
| 5 | `curl -s http://127.0.0.1:9812/metrics` | `status` 沒有列出的細節，例如每個用戶端的計數 |
| 6 | `rradius-server tuning --config <檔>` | 作業系統有沒有丟掉 UDP 封包 |

### 認證紀錄

每次認證都有一行：

```text
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="reject"
```

- **有這一行：** 請求有到伺服器、比對到用戶端、通過了 Message-Authenticator 的檢查。問題通常在驗證鏈或帳號資料，看 `handler` 與 `decision`（例外見[共用密碼不一致](#共用密碼不一致)的第三種情況）。
- **沒有這一行：** 請求在更前面就被丟掉了。看 `status` 的 `Discarded` 開頭那幾行。

`handler` 常見的值：

| `handler` | 意思 |
| --- | --- |
| `sql`、`ldap`、`lua` 等 | 這個處理器做出了決定 |
| `none` | 沒有處理器給出答案，通常是帳號不存在或停用 |
| `lockout` | 被登入失敗鎖定擋下，沒有檢查密碼 |
| `mschapv1_refused` | `allow_mschapv1 = false`，拒絕 MS-CHAPv1 |
| `eap-peap`、`eap-tls` 等 | EAP 對話的結果 |

認證紀錄可以用 `[log] auth = false` 關掉；排錯時要開著。日誌等級用 `[log] level`（`error`、`warn`、`info`、`debug`），改了要重啟。細節見[第 17 章](17-monitoring.md)。

### trace

認證紀錄不寫屬性。要看 NAS 實際送了什麼、伺服器回了什麼，對那個使用者開 trace：

```toml
# rradius.toml
[log.trace]
users = ["alice"]
minutes = 30
```

reload 之後（`kill -HUP <pid>`），`alice` 的每個請求多兩行：

```text
… INFO rradius_trace: request client="localhost" source=127.0.0.1 user="alice" code=1 attributes="Message-Authenticator = <hidden>, User-Name = alice, User-Password = <hidden>, NAS-IP-Address = 192.0.2.1, NAS-Identifier = rradius-bench"
… INFO rradius_trace: response client="localhost" source=127.0.0.1 user="alice" code=2 attributes="Message-Authenticator = <hidden>, Session-Timeout = 3600, Filter-Id = staff"
```

`code=2` 是 Access-Accept，`code=3` 是 Access-Reject。trace 會寫出個人資料，用完就拿掉（[第 20 章](20-security.md)）。

> **注意：** trace 只記到通過共用密碼檢查的請求。被丟棄的請求不會出現在 trace 裡，要看 `status`。

## 沒有回應、拒絕、ERROR

NAS 看到的結果只有幾種，原因各不相同：

| NAS 看到 | 伺服器做了什麼 | 先看哪裡 |
| --- | --- | --- |
| Access-Reject | 驗證過，決定拒絕 | 認證紀錄的 `handler` |
| 沒有回應（逾時） | 丟掉了請求，或答不出來 | `status` 的 `Discarded` 幾行，以及日誌的 `not answered` |
| TACACS+ 的 ERROR | 答不出來 | 日誌的 `answered ERROR` |

「沒有回應」是刻意的：資料庫不能用、沒有 leader（設了 `stop_without_leader_ms`，或請求需要寫入時）、過載時，伺服器不回 Reject，讓 NAS 重送或改問別台伺服器，而不是把使用者踢掉。

`status` 的 RADIUS 區段：

```text
RADIUS
  …
  Discarded: overload        0.0/s (56)
  Discarded: unknown client  0.0/s (1)
  Discarded: invalid         0.0/s (3)
  Discarded: database        0.0/s (1)
  Discarded: no leader       0.0/s (1)
…
```

| 這一行增加 | 原因 | 小節 |
| --- | --- | --- |
| `Discarded: unknown client` | 來源位址不在用戶端清單 | [用戶端比對失敗](#用戶端比對失敗) |
| `Discarded: invalid` | 共用密碼不一致、缺 Message-Authenticator、封包格式錯 | [共用密碼不一致](#共用密碼不一致) |
| `Discarded: database` | 驗證或計費需要的資料庫不能用 | [資料庫不能用](#資料庫不能用) |
| `Discarded: no leader` | 設了 `stop_without_leader_ms`，叢集沒有 leader | [叢集沒有 leader](#叢集沒有-leader) |
| `Discarded: overload` | 同時處理的請求超過上限 | [過載與 UDP 丟包](#過載與-udp-丟包) |

同樣的計數在 `/metrics` 是 `rradius_radius_discarded_total{reason="…"}`，`reason` 是 `overload`、`unknown_client`、`invalid`、`database`、`no_leader`。

用 `rradius-bench` 測試時，看它的計數與結束碼：

| `rradius-bench` 的結果 | 結束碼 |
| --- | --- |
| `accept 1` | 0 |
| `reject 1` | 1 |
| `timeouts 1`（伺服器沒回應，或回應的簽章對不上） | 2 |
| `timeouts 1, … socket errors 1`（伺服器根本沒在跑） | 2 |

## 設定讀不進來

`check` 失敗時印一行原因，結束碼 2。訊息開頭是出錯的檔案，有行號時會寫出來。實測的訊息：

```console
$ rradius-server check --config nope.toml
nope.toml: cannot read configuration: No such file or directory (os error 2)
$ rradius-server check
/etc/rradius/rradius.toml: cannot read configuration: No such file or directory (os error 2)
```

沒有 `--config` 時讀 `/etc/rradius/rradius.toml`。

TOML 寫錯：

```text
rradius.toml: line 40: unclosed table, expected `]`
rradius.toml: line 28: `auth_prot`: unknown field `auth_prot`, expected one of `bind`, `auth_port`, `acct_port`, …
rradius.toml: line 28: `auth_port`: invalid type: string, expected u16
```

- 鍵名拼錯時，訊息會列出這個表接受的所有鍵。
- 數字加了引號就變成字串。

`include` 的檔案：

```text
conf.d/dup.toml: line 1: duplicate key
conf.d/bad.toml: line 1: an included file has to begin with a table (`[section]`), not a setting: a setting before its first table would belong to the file before it
```

合併後才檢查出的錯誤（例如 `radius clients: duplicate client name 'core'`）沒有行號，後面會附上 `(in conf.d/10-clients.toml line 3)` 之類的位置，用它找到出錯的那一筆。

`duplicate key` 最常見的原因，是 `conf.d/` 裡的檔案又宣告了主設定檔已經有的表，例如 `[radius]`。同一個表只能出現一次，要改就改在原來的地方。

字典與腳本：

```text
dict/missing: cannot read dictionary: No such file or directory (os error 2)
dictionary:1: ATTRIBUTE needs a name, number and type: ATTRIBUTE	Broken-Attr
cannot compile script 'accounting.lua': syntax error: 2: Expected identifier when parsing variable name, got <eof>
```

- 字典的錯誤寫出檔名與行號，以及那一行的內容。
- 自己的字典可以先用 `$INCLUDE` 帶入附的字典，再加自己的屬性（[第 06 章](06-dictionary.md)）。

缺了必要的部分：

```text
rradius.toml: the [cluster] section is required
```

`[lua]`、`[radius]`／`[tacacs]` 都沒有時的訊息見[第 03 章](03-quick-start.md)的常見錯誤。

用戶端與祕密檔：

```text
rradius.toml: radius client 'localhost': exactly one of secret or secret_file must be set
rradius.toml: radius client 'localhost': secret_file 'localhost.secret' is empty
rradius.toml: radius client 'localhost': cannot read secret_file 'localhost.secret': No such file or directory (os error 2)
rradius.toml: radius client 'localhost': unknown auth chain 'staff' (no [auth.chains.staff])
rradius.toml: radius clients: duplicate client network '127.0.0.1/32'
```

- 兩個用戶端的位址相同（單一位址會寫成 `/32`）不行；名稱相同則不會被擋，實測可以通過 `check`。名稱會出現在日誌與指標上，請自己保持唯一。

EAP 憑證：

```text
rradius.toml: radius.eap.key_file 'server.key' does not fit cert_file 'server.pem': keys may not be consistent: KeyMismatch
rradius.toml: radius.eap.key_file 'server.key': I/O error: No such file or directory (os error 2)
rradius.toml: radius.eap.client_ca_file is required
rradius.toml: radius.eap.fragment_size must be from 256 to 1400, not 2000
```

`client_ca_file` 是 `methods` 有 `tls` 時必填。

範圍與值的錯誤，訊息會直接寫出規則，例如：

```text
rradius.toml: auth.lockout.max_failures must be greater than 0
rradius.toml: log.trace.minutes: 0 is not from 1 to 1440
rradius.toml: lua.http.allowed_hosts must list at least one host
```

安全相關設定的錯誤訊息整理在[第 20 章](20-security.md)的常見錯誤。

> **注意：** `check` 的警告不影響結束碼。`configuration ok` 前面的 `WARN` 也要看，處理方式見[第 20 章](20-security.md)。

## reload 失敗或沒有生效

`SIGHUP` 讓伺服器重新讀設定。結果只看日誌：

| 日誌 | 意思 |
| --- | --- |
| `INFO rradius_core::reload: configuration reloaded from rradius.toml` | 全部套用 |
| `WARN rradius_core::reload: configuration reloaded from rradius.toml; these need a restart to apply: radius.auth_port` | 其他都套用了，列出的這些要重啟才生效 |
| `ERROR rradius_core::reload: reload failed, keeping the running configuration: rradius.toml: auth.lockout.max_failures must be greater than 0` | 新設定有錯，一項都沒套用，繼續用原本的設定 |

reload 失敗不會讓伺服器停下來，但下次重啟時會因為同一個錯誤起不來。所以先跑 `check` 再 reload，並用 `config diff` 確認改了什麼：

```console
$ rradius-server config diff rradius.toml.orig rradius.toml
radius.clients[localhost].address: "127.0.0.1" | "127.0.0.2"
$ echo $?
1
```

`config diff` 有差異時結束碼是 1。

指標也看得出來：`rradius_reload_total{result="error"}` 增加，`rradius_reload_last_success_timestamp_seconds` 不變。要重啟才生效的設定清單見[附錄 A](90-config-reference.md)，reload 的流程見[第 18 章](18-operations.md)。

## 伺服器啟動失敗

### 埠已經被佔用

RADIUS 或 TACACS+ 的埠被別的程式用了，伺服器結束，結束碼 1（RADIUS 印 `RADIUS server error: …`，TACACS+ 印 `TACACS+ server error: …`）：

```text
… INFO rradius_core::cli: cluster node 1 started
… INFO rradius_core::cli: cluster node 1 has caught up with the cluster
RADIUS server error: cannot listen on 127.0.0.1:1812: Address already in use (os error 48)
```

叢集的 Raft 埠被佔用，結束碼 2：

```text
cannot start cluster node 1: Config: Cannot bind Raft listen address '127.0.0.1:8100': Error("Address already in use (os error 48)")
```

處置：找出是誰佔了這個埠（Linux 用 `ss -lunp`、`ss -ltnp`，macOS 用 `lsof -i :<埠>`）。最常見的是舊的 rradius 還沒停完，或同一台機器上跑了兩份設定。

> **警告：** 指標埠被佔用時，伺服器**不會**結束。日誌只多一行 `ERROR … metrics: cannot listen on <位址>: Address already in use (os error 48); no metrics are served, and `status` cannot read them`，RADIUS 照常服務，但這個伺服器沒有開出 `/metrics`、`/livez`、`/readyz`。啟動後一定要確認日誌有 `Metrics listening on …`。

### 上一次還沒停完

叢集節點停止要一段時間。還沒停完就啟動新的，資料目錄被鎖住，新的那一份結束：

```text
… WARN hiqlite_wal::log_store: LockFile data/logs exists already - this is not a clean start!
thread 'tokio-rt-worker' (…) panicked at …/hiqlite-wal-0.16.0/src/log_store.rs:47:21:
LockFile data/logs is locked and in use by another process
… ERROR rradius_core::cli: cluster node: a panic in its database (…/hiqlite-wal-0.16.0/src/log_store.rs:47); exiting so that a restart recovers it
```

處置：等舊的行程結束（`pgrep -f rradius-server`），再啟動。套件附的 systemd unit 停止時最多等 `TimeoutStopSec=60` 秒。

### 沒有 leader 就不啟動

設了 `cluster.stop_without_leader_ms`，而啟動時找不到 leader（例如三個節點只有它一個在跑），等待一段時間後結束，結束碼 1：

```text
… INFO rradius_core::cli: cluster node 1 started
cluster was not ready: no usable leader within 53.980820917s, and stop_without_leader_ms is set
```

處置：先讓過半數的節點起來。沒設 `stop_without_leader_ms` 的節點會照常啟動，用自己的資料回答，見[叢集沒有 leader](#叢集沒有-leader)。

## 用戶端比對失敗

**現象：** NAS 收不到回應；伺服器日誌沒有認證紀錄，只有：

```text
… WARN rradius_core::radius: RADIUS packet from unknown client 127.0.0.1 discarded
```

`status` 的 `Discarded: unknown client` 增加。

**原因：** 封包的來源位址不在任何 `[[radius.clients]]` 的 `address` 裡。實測把用戶端改成 `127.0.0.2`、從 `127.0.0.1` 送，就是上面的結果。

**處置：**

- 用日誌裡的位址，不要用你以為的位址。NAS 有多張網卡、經過 NAT、或用了管理介面以外的位址送封包時，來源會和你想的不一樣。
- 一個網段的 NAS 可以用 CIDR 寫成一個用戶端，例如 `address = "10.0.0.0/24"`。
- 確認改了之後 reload 成功（看日誌的 `configuration reloaded`）。

這行警告同一段時間內只記一次，避免被大量的陌生封包灌爆日誌。用戶端的設定見[第 05 章](05-radius-clients.md)。

## 共用密碼不一致

NAS 與伺服器的共用密碼不同時，結果依請求有沒有 Message-Authenticator 而不同。三種情況都實測過：

| 請求 | 用戶端設定 | 伺服器 | 日誌 |
| --- | --- | --- | --- |
| 有 Message-Authenticator | 任何 | 簽章對不上，丟棄，`Discarded: invalid` 增加 | **沒有任何一行** |
| 沒有 Message-Authenticator | `require_message_authenticator = "yes"`（預設） | 丟棄，`Discarded: invalid` 增加 | `discarded … Access-Requests lacking a Message-Authenticator` |
| 沒有 Message-Authenticator | `"no"` | 解出來的密碼是亂碼，回 Access-Reject；NAS 用自己的密碼驗不過這個回應，當作沒收到 | 有認證紀錄，`decision="reject"` |

第一種最難發現，因為日誌完全安靜。看到 NAS 逾時、日誌沒有認證紀錄、也沒有 `unknown client`，就看 `Discarded: invalid` 有沒有在增加。

處置：

- 重新設定兩邊的共用密碼。從檔案複製，不要手打。伺服器讀 `secret_file` 時會去掉結尾的換行，所以複製到 NAS 時不要帶上換行。
- 用 `rradius-bench --secret "$(cat <檔>)"` 從伺服器本機測一次，確認伺服器這邊的密碼是對的，問題就縮小到 NAS 那一邊。

缺 Message-Authenticator 的處理見[第 20 章](20-security.md)。TACACS+ 的共用密碼錯誤見[TACACS+ 連不上或回 ERROR](#tacacs-連不上或回-error)。

## 一直被拒絕

有認證紀錄、`decision="reject"`。看 `handler`：

| 認證紀錄 | 原因 | 處置 |
| --- | --- | --- |
| `handler="sql"`（或 `ldap` 等） | 處理器查到帳號，但密碼不對 | 確認密碼；確認 NAS 用的方法（PAP、CHAP、MS-CHAPv2）與帳號存的格式相容（[第 07 章](07-users-and-chains.md)） |
| `handler="none"`，`account` 有值 | 沒有處理器認得這個帳號：不存在、停用，或查詢條件沒選到 | 用 `user show` 或直接查資料表 |
| `handler="lockout"` | 被登入失敗鎖定 | 見[第 20 章](20-security.md) |
| `handler="mschapv1_refused"` | MS-CHAPv1 被關掉 | 讓 NAS 改用其他方法，或打開 `allow_mschapv1` |

伴隨的警告，代表帳號資料本身有問題（都實測過）：

```text
… WARN rradius_core::auth_chain: the stored password asks for more than [auth.password_limits].bcrypt_cost allows and is refused; raise the limit if the hash is meant to be used user="bob" format="crypt" parameter="bcrypt_cost"
… WARN rradius_core::auth_chain: the stored password looks like a hash with no scheme prefix and is refused; add {crypt} or {argon2} before it, or {clear} if it is the password itself user="eve"
```

第一行：雜湊成本超過上限。第二行：雜湊少了格式前綴。處置見[第 20 章](20-security.md)。

### 密碼對，回覆屬性有問題

回覆屬性的名稱不在字典裡時，整個回應組不出來，伺服器改回 Access-Reject：

```text
… ERROR rradius_core::radius: RADIUS response attribute error: unknown attribute 'No-Such-Attr'
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="reject"
```

實測在 `user_reply` 表加一列 `No-Such-Attr`，`alice` 用正確的密碼就被拒絕了。處置：改正資料表裡的屬性名稱，或把屬性加進字典（[第 06 章](06-dictionary.md)）。

## 資料庫不能用

驗證或計費要用的資料庫連不上時，伺服器**不回應**，而不是拒絕。

### 外部資料庫

實測 sql 處理器指向一個沒有在聽的 PostgreSQL 位址：

```text
… ERROR rradius_core::auth_chain: auth sql handler error: database legacy: pool timed out while waiting for an open connection
… WARN rradius_core::lua_engine: RADIUS Access-Request from client localhost not answered: the authentication backend could not be used
```

`status` 的 `Discarded: database` 增加，NAS 逾時。

`required_for_ready = true` 的資料庫連不上時，`/readyz` 回 503，load balancer 可以把流量導走：

```text
… WARN rradius_core::external_db: databases.legacy: not reachable at startup: error communicating with database: Connection refused (os error 61); /readyz answers 503 until it is
```

```console
$ curl -s -w ' http=%{http_code}\n' http://127.0.0.1:9812/readyz
database legacy unreachable http=503
$ rradius-server status --config rradius.toml
…
Node
  Ready                     database legacy unreachable
…
External databases
  …
  legacy reachable               no
…
```

- 檢查每 5 秒一次，連續兩次失敗算連不上，一次成功就恢復。
- `required_for_ready` 預設 `false`：沒設的資料庫斷了，`/readyz` 照樣回 200，只有請求不回應。

處置：查資料庫本身、網路、帳號密碼。`connect_timeout_ms` 要比 NAS 的重送時間短，NAS 才有機會改問別台。詳細見[第 09 章](09-external-databases.md)。

> **未實機驗證：** 只測了「埠沒有在聽」這一種連不上。資料庫在跑但帳號密碼錯、TLS 憑證驗不過的訊息沒有實測。

### 叢集資料庫

叢集資料庫（帳號表、計費腳本寫入的表）的問題通常是沒有 leader，見下一節。

## 叢集沒有 leader

寫入要等 leader 確認，所以沒有 leader 時，寫不了資料庫。三個節點在同一台機器上模擬：三個都起來之後，停掉節點 2 與 3，只留節點 1。

### 沒設 stop_without_leader_ms（預設）

節點 1 繼續用自己的資料回答：

- 登入：只讀資料，`alice` 照樣 `accept`。
- 計費：腳本要寫入，失敗，不回應：

```text
… ERROR rradius_core::lua_engine: Lua script error (radius/accounting): runtime error: no leader for 5 s\nstack traceback:\n…\taccounting.lua:44: in function <accounting.lua:1>\n
```

`status` 的 Node 區段：

```text
Node
  Ready                     ok
  Leader                    1
  Has a leader              0
  Caught up                 1
  …
  Seconds without a leader  5
```

`Has a leader 0`、`Seconds without a leader` 在增加，就是沒有 leader。實測這時 `Leader` 那一行仍然是 1，判斷要看 `Has a leader`。

### 設了 stop_without_leader_ms

`[cluster]` 加 `stop_without_leader_ms = 10000`，沒有 leader 超過 10 秒後：

```console
$ curl -s -w ' http=%{http_code}\n' http://127.0.0.1:9812/readyz
no leader http=503
$ rradius-server status --config rradius.toml
…
Node
  Ready                     no leader
  …
  Has a leader              0
  …
  Seconds without a leader  16
…
  Discarded: no leader       0.0/s (1)
```

- RADIUS 請求一律不回應，連只讀的登入也一樣，NAS 可以改問其他節點。
- 節點 2、3 回來之後，`/readyz` 回到 `ok`，登入恢復（實測）。
- 這個設定也讓節點在啟動時找不到 leader 就結束，見[伺服器啟動失敗](#伺服器啟動失敗)。

處置：讓過半數的節點恢復。三個節點的叢集，至少要兩個在跑、而且彼此連得到。查節點之間的網路、`cluster.nodes` 的位址、兩個叢集密碼在每個節點是否相同。叢集的運作與成員管理見[第 16 章](16-cluster.md)。

> **未實機驗證：** 以上是三個節點在同一台機器上的模擬。跨機器的網路中斷、節點之間密碼不一致的情況沒有實測。

## 計費遺漏

NAS 送計費請求，要收到 Accounting-Response 才算送達；沒收到就重送，重送幾次之後放棄，那筆計費就沒了。所以「計費遺漏」幾乎都是「伺服器沒有回 Accounting-Response」。

伺服器只在計費腳本成功之後才回應。腳本失敗的兩種情況（都實測過）：

**資料庫暫時不能用**，例如沒有 leader：

```text
… ERROR rradius_core::lua_engine: Lua script error (radius/accounting): runtime error: no leader for 6 s\nstack traceback:\n…\taccounting.lua:44: in function <accounting.lua:1>\n
```

**腳本或 SQL 寫錯**，例如資料表名稱打錯：

```text
… ERROR rradius_core::lua_engine: Lua script error (radius/accounting): runtime error: PrepareStatement: no such table: accountingx\nstack traceback:\n…
```

兩種都不回應。`status` 的 `Acct requests` 增加、`Accounting-Response` 沒有跟著增加，就是這種情況。

### 用計費 spool 撐過資料庫中斷

設了 `[radius.accounting_spool]`，第一種情況（資料庫連不上或逾時）會先把請求寫進磁碟上的 spool，然後回應 NAS；資料庫恢復後自動重送：

```toml
# rradius.toml
[radius.accounting_spool]
dir = "spool"
```

實測：三個節點停掉兩個之後送一筆 Start，腳本照樣出錯，但 NAS 收到了回應（`rradius-bench` 是 `accounting 1`），指標：

```text
rradius_acct_spool_written_total 1
rradius_acct_spool_bytes 110
```

把節點 2、3 啟動回來之後：

```text
rradius_acct_spool_written_total 1
rradius_acct_spool_replayed_total 1
rradius_acct_spool_bytes 0
```

`status` 的 Accounting spool 區段有同樣的數字（`Bytes waiting`、`Replayed`、`Rejected`、`Not spooled`）。

- 第二種情況（SQL 寫錯）**不會**進 spool：重送也一樣會錯。實測 `rradius_acct_spool_written_total` 維持 0。看到這種錯誤要修腳本。
- `Bytes waiting` 一直不降，表示重送一直失敗，看日誌的錯誤。`Rejected` 增加，表示有記錄被判定無法處理，移到 spool 目錄裡的 `rejected` 檔案。
- spool 滿了（`radius.accounting_spool.max_bytes`）時不回應，`Not spooled` 增加；寫不進磁碟時也不回應。

spool 的設定、重送規則與腳本的寫法見[第 13 章](13-accounting.md)。

## 過載與 UDP 丟包

### 伺服器丟棄

同時處理的請求超過 `max_auth_inflight`（或 `max_acct_inflight`）時，多的直接丟掉。實測把 `max_auth_inflight` 設成 2，用 30 個並行送 bcrypt 帳號的登入：

```text
… WARN rradius_core::radius: RADIUS authentication overloaded: discarded 28 packets
```

`status` 的 `Discarded: overload` 增加，`Auth limit` 顯示目前的上限。

### 作業系統丟棄

封包在到達 rradius 之前，就可能因為 socket 的接收緩衝區滿了被作業系統丟掉。這種丟棄 rradius 看不到，要問作業系統：

```console
$ rradius-server tuning --config rradius.toml
rradius tuning advice for macOS

UDP packets dropped because a receive buffer was full, since the system started: 137296
  Raise the receive buffers below, then watch whether this count still grows.
…
```

這個數字是整台機器開機以來的總數。過一陣子再跑一次，看它有沒有增加。

`radius.recv_buffer_bytes` 要的緩衝區超過系統上限時，啟動時會警告：

```text
… WARN rradius_core::radius: RADIUS: asked for a receive buffer of 67108864 bytes and each socket got 8388608; raise kern.ipc.maxsockbuf to at least 67108864
```

訊息裡的系統設定名稱，在 Linux 是 `net.core.rmem_max`，在 macOS 與 FreeBSD 是 `kern.ipc.maxsockbuf`。`tuning` 會印出要改的值與指令。

處置與容量規劃見[第 19 章](19-performance.md)。

## EAP 卡住或失敗

EAP 是多回合的對話，NAS 與伺服器之間來回好幾次。認證紀錄的 `handler` 是 `eap-peap`、`eap-tls` 這類，失敗時多一行原因。實測（PEAP，`rradius-bench` 當用戶端）：

| 情況 | 伺服器日誌 | `rradius-bench` |
| --- | --- | --- |
| 內層密碼錯誤 | `EAP conversation with client localhost failed: the chain did not accept the inner name` | `reject 1` |
| 用戶端要的方法伺服器沒提供 | `EAP conversation with client localhost failed: the peer asked only for methods this server does not offer` | `reject 1` |
| 用戶端不信任伺服器憑證 | **沒有任何一行** | `TLS: invalid peer certificate: UnknownIssuer`，`invalid 1` |
| 伺服器憑證的名稱不對 | **沒有任何一行** | `certificate not valid for name "wrong.example.net"; …`，`invalid 1` |

後兩種是用戶端自己放棄了對話，伺服器只看到對話沒有下文。`status` 的 EAP 區段會看到 `Sessions under way` 留著沒結束的對話，它們在 `radius.eap.session_timeout_ms`（預設 30000 毫秒）後被清掉。所以：

> **注意：** 使用者說「連不上」、日誌卻沒有 EAP 的失敗紀錄，先懷疑用戶端裝置不信任伺服器憑證：裝置沒裝你的 CA、或設定的伺服器名稱和憑證不符。去看裝置那一端的訊息。

### 分段與 MTU

EAP 的 TLS 訊息比一個 RADIUS 封包大，要切成好幾段。`radius.eap.fragment_size` 是每段的上限，預設 1000，範圍 256 到 1400 位元組。NAS 與伺服器之間的路徑 MTU 比較小、或中間的設備會丟掉 IP 分段時，大的 RADIUS 封包會在路上消失，EAP 對話卡在憑證交換那一步。

處置：把 `fragment_size` 調小再試。

> **未實機驗證：** MTU 造成的封包遺失需要真的網路設備才能重現，沒有實測。上面的處置依據 `fragment_size` 的作用。

EAP 的設定與憑證見[第 11 章](11-eap.md)。

## TACACS+ 連不上或回 ERROR

共用密碼不一致時，伺服器解不開封包內容，關掉連線：

```text
… WARN rradius_core::tacacs: TACACS+ connection from 127.0.0.1 closed: a body did not decode, so the shared secret may be wrong
```

`status` 的 TACACS+ 區段 `Malformed` 增加。`rradius-bench` 那一端看到的是 `lengths: the fields of the reply do not add up to its length, so the shared secret may be wrong`。

驗證用的資料庫不能用時，TACACS+ 不是不回應，而是回 ERROR：

```text
… WARN rradius_core::lua_engine: TACACS+ authentication for client router1 answered ERROR: the authentication backend could not be used
```

設備收到 ERROR 之後怎麼做（改問下一台伺服器、改用本機帳號），依設備的設定而定。

連線超過 `tacacs.idle_timeout_ms`（預設 300000 毫秒）沒有送來完整的封包，會被關掉。實測設成 2000、開一條連線什麼都不送：

```text
… WARN rradius_core::tacacs: TACACS+ connection from 127.0.0.1 closed: idle for 2s
```

設備若要長時間保持一條連線，這個值要比設備送封包的間隔長。TACACS+ 的設定見[第 12 章](12-tacacs.md)。

> **未實機驗證：** 沒有用真的網路設備連過 TACACS+，只用 `rradius-bench` 的 `tacacs-pap`、`tacacs-chap` 負載。

## 腳本錯誤

腳本執行失敗時，請求當作拒絕（計費則是不回應），日誌是 `Lua script error (<協定>/<階段>)` 加上原因與行號：

```text
… ERROR rradius_core::lua_engine: Lua script error (radius/authentication): runtime error: script exceeded the execution limit of 100 ms
… ERROR rradius_core::lua_engine: Lua script error (radius/authentication): runtime error: api.lua:1: attempt to index nil with 'open'\nstack traceback:\n\tapi.lua:1: in function <api.lua:1>
… ERROR rradius_core::lua_engine: Lua script error (radius/authentication): runtime error: api.lua:1: attempt to call a nil value\nstack traceback:\n\tapi.lua:1: in function <api.lua:1>
```

- `exceeded the execution limit`：腳本跑太久，超過 `lua.max_execution_ms`。找出迴圈或慢的部分；等資料庫的時間不算在內。
- `attempt to index nil`、`attempt to call a nil value`：用了不存在的東西。沙箱拿掉了 `io`、`os.execute`、`require` 等；沒有 `[lua.http]` 時 `http_request` 也不存在。

腳本裡用 `log(...)` 寫的訊息，會帶著腳本名稱與用戶端出現在日誌：

```text
… INFO rradius_core::lua_engine: script api.lua (radius/authentication, client localhost): api gave no answer: http://127.0.0.1 is not in lua.http.allowed_hosts
```

腳本的寫法與除錯見[第 10 章](10-lua-policies.md)。

## needs rradius-enterprise

`rradius-server` 讀到只有企業版才有的設定時，明確拒絕，結束碼 2：

```text
rradius.toml: proxy: not supported by this program (proxying needs rradius-enterprise)
rradius.toml: radius.tls: not supported by this program (RadSec needs rradius-enterprise)
rradius.toml: radius.eap.resumption_ms: not supported by this program (session resumption needs rradius-enterprise)
rradius.toml: radius client 'localhost': mfa needs rradius-enterprise
rradius.toml: radius client 'localhost': eap_methods: 'sim' is not a method this server offers; it has tls, peap and ttls
```

這不是設定寫錯，而是這個程式沒有那個功能。

處置：把那一節或那個鍵拿掉。

> **企業版：** 上面這些功能在企業版可以使用。

## 從頭走一次：新的 NAS 沒有回應

把前面的方法串起來。情境：新裝了一台 NAS，使用者登入一直逾時。下面每一步的輸出都是實測。

**第一步：設定沒問題嗎？**

```console
$ rradius-server check --config rradius.toml
configuration ok
```

設定沒有錯，也沒有警告。

**第二步：請求有到伺服器嗎？** 看日誌有沒有這台 NAS 的認證紀錄。沒有，但有：

```text
… WARN rradius_core::radius: RADIUS packet from unknown client 127.0.0.1 discarded
```

NAS 用的來源位址不在用戶端清單。把它加進去（或改正 `address`），reload，確認日誌有 `configuration reloaded from rradius.toml`。

**第三步：還是逾時。** 日誌這次什麼都沒有：沒有認證紀錄，也沒有 `unknown client`。看 `status`：

```console
$ rradius-server status --config rradius.toml
…
  Discarded: unknown client  0.0/s (1)
  Discarded: invalid         0.0/s (3)
…
```

`Discarded: invalid` 在增加：請求帶了 Message-Authenticator，但簽章對不上，也就是共用密碼不一致。重設兩邊的共用密碼。

**第四步：現在有回應了，但是被拒絕。** 日誌有認證紀錄：

```text
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="reject"
```

`handler="sql"`：帳號查到了，密碼不對。請使用者確認密碼；還是不行，就對這個使用者開 trace，看 NAS 送來的是哪一種密碼屬性（`User-Password`、`CHAP-Password` 或 MS-CHAP 的屬性），確認帳號存的格式能驗證這種方法。

整個過程只用到 `check`、日誌與 `status`，順序就是[排錯的順序](#排錯的順序)那張表。

## 回報問題時要附上的資料

自己查不出來、要請別人幫忙時，附上這些，可以省掉很多來回：

- `rradius-server --version` 的輸出。
- `rradius-server check --config <檔>` 的完整輸出與結束碼。
- `rradius-server config show --config <檔>`：帶出預設值的完整設定，祕密顯示成 `<hidden>`，可以直接貼。不要貼原始的設定檔，裡面可能有寫明文的密碼。
- 出問題那段時間的日誌，包括前後的 `WARN`、`ERROR` 與認證紀錄。
- `rradius-server status --config <檔>` 的輸出；要給程式讀的話加 `--json`。
- UDP 丟包相關的問題，加上 `rradius-server tuning --config <檔>`。

trace 的輸出含有個人資料，給別人之前先把使用者名稱、位址這類資料遮掉。

## 常見錯誤：訊息對照表

看到這個訊息，原因與處置。全部是實測輸出；`…` 是會隨情況變化的部分。

### check 與啟動

| 訊息 | 原因 | 處置 |
| --- | --- | --- |
| `cannot read configuration: No such file or directory (os error 2)` | 設定檔路徑錯；沒有 `--config` 時讀 `/etc/rradius/rradius.toml` | 加 `--config` 或修正路徑 |
| `` line N: unclosed table, expected `]` `` | TOML 語法錯 | 看那一行 |
| `` `X`: unknown field `X`, expected one of … `` | 鍵名拼錯，或放錯表 | 對照訊息列出的鍵、[附錄 A](90-config-reference.md) |
| `` `X`: invalid type: string, expected u16 `` | 數字加了引號，或型別不對 | 改成正確的型別 |
| `line 1: duplicate key` | `conf.d/` 的檔案重複宣告了已有的表 | 合併到原來的表 |
| `an included file has to begin with a table` | `include` 的檔案第一行不是 `[表]` | 先寫表名 |
| `cannot read dictionary: …` | 字典路徑錯 | 改 `dictionary` |
| `dictionary:N: ATTRIBUTE needs a name, number and type: …` | 字典檔的格式錯 | 修正那一行 |
| `cannot compile script '…': syntax error: …` | Lua 語法錯 | 修正腳本 |
| `the [cluster] section is required` | 沒有 `[cluster]` | 加上（[第 16 章](16-cluster.md)） |
| `exactly one of secret or secret_file must be set` | 用戶端沒有共用密碼，或兩者都設了 | 只留 `secret_file` |
| `secret_file '…' is empty` | 祕密檔是空的 | 寫入密碼 |
| `cannot read secret_file '…': …` | 祕密檔不存在或讀不到 | 確認路徑與權限 |
| `unknown auth chain 'X' (no [auth.chains.X])` | 用戶端的 `auth` 指到不存在的鏈 | 新增 `[auth.chains.X]` 或改名 |
| `duplicate client network '…'` | 兩個用戶端位址相同 | 合併或刪掉一個 |
| `duplicate client name '…'` | 同一張清單裡兩個用戶端同名（沒寫名稱時以位址為名） | 改掉其中一個的 `name`（[第 05 章](05-radius-clients.md)） |
| `cluster.secret_raft must be at least 16 characters` | 叢集密碼太短 | `openssl rand -hex 32` |
| `key_file '…' does not fit cert_file '…'` | 私鑰與憑證不是一對 | 換成成對的檔案 |
| `radius.eap.client_ca_file is required` | 用 `tls` 卻沒有 CA | 加 `client_ca_file` |
| `fragment_size must be from 256 to 1400, not N` | 超出範圍 | 改在範圍內 |
| `… not supported by this program (… needs rradius-enterprise)` | 企業版才有的設定 | 拿掉，或換用企業版 |
| `RADIUS server error: cannot listen on …: Address already in use`（結束碼 1） | RADIUS 埠被佔用 | 停掉佔用的程式 |
| `Cannot bind Raft listen address '…'`（結束碼 2） | 叢集埠被佔用 | 停掉佔用的程式 |
| `metrics: cannot listen on …: Address already in use`（伺服器繼續跑） | 指標埠被佔用 | 停掉佔用的程式後重啟 rradius |
| `LockFile data/logs is locked and in use by another process` | 舊的行程還沒停完 | 等它結束再啟動 |
| `cluster was not ready: no usable leader within …, and stop_without_leader_ms is set`（結束碼 1） | 啟動時叢集沒有過半數 | 先讓其他節點起來 |

### 執行中

| 訊息 | 原因 | 處置 |
| --- | --- | --- |
| `RADIUS packet from unknown client X discarded` | 來源位址不在用戶端清單 | 用日誌裡的位址加用戶端 |
| `RADIUS: discarded N Access-Requests lacking a Message-Authenticator, …` | NAS 沒送 Message-Authenticator，或共用密碼錯而且沒送 | 開啟 NAS 的 Message-Authenticator；不行就只對它設 `"no"` |
| （沒有日誌，`Discarded: invalid` 增加） | 共用密碼不一致 | 重設兩邊的共用密碼 |
| `RADIUS Access-Request from client X not answered: the authentication backend could not be used` | 驗證用的資料庫或目錄不能用 | 查前一行的錯誤 |
| `auth sql handler error: database X: pool timed out while waiting for an open connection` | 外部資料庫連不上 | 查資料庫與網路 |
| `databases.X: not reachable at startup: …; /readyz answers 503 until it is` | `required_for_ready` 的資料庫連不上 | 同上 |
| `Lua script error (radius/accounting): runtime error: no leader for 5 s` | 叢集沒有 leader，寫不了 | 讓過半數的節點恢復 |
| `/readyz` 回 `no leader`（503） | 設了 `stop_without_leader_ms` 而沒有 leader | 同上 |
| `Lua script error (radius/accounting): runtime error: PrepareStatement: no such table: …` | 計費腳本的 SQL 寫錯，不回應也不進 spool | 修正腳本 |
| `RADIUS authentication overloaded: discarded N packets` | 超過 `max_auth_inflight` | 查是否異常流量；調整上限（[第 19 章](19-performance.md)） |
| `asked for a receive buffer of N bytes and each socket got M; raise … to at least N` | 系統的 socket 緩衝區上限太低 | 照 `tuning` 調整 |
| `RADIUS response attribute error: unknown attribute 'X'` | 回覆屬性不在字典 | 修正資料或字典 |
| `the stored password asks for more than [auth.password_limits].X allows and is refused` | 雜湊成本超過上限 | [第 20 章](20-security.md) |
| `the stored password looks like a hash with no scheme prefix and is refused` | 雜湊沒有格式前綴 | 補上前綴 |
| `auth lockout: client X: account "Y" from "Z" is locked (…)` | 失敗太多次 | 等 `lock_seconds`；查是誰在猜 |
| `EAP conversation with client X failed: the chain did not accept the inner name` | EAP 內層的密碼錯或帳號不存在 | 同一般的拒絕 |
| `EAP conversation with client X failed: the peer asked only for methods this server does not offer` | 裝置要的 EAP 方法沒有開 | 調整 `radius.eap.methods` 或裝置設定 |
| `TACACS+ connection from X closed: a body did not decode, so the shared secret may be wrong` | TACACS+ 共用密碼不一致 | 重設兩邊的共用密碼 |
| `TACACS+ authentication for client X answered ERROR: the authentication backend could not be used` | 驗證用的資料庫不能用 | 查資料庫 |
| `script exceeded the execution limit of N ms` | 腳本跑太久 | 修正腳本，或調整 `lua.max_execution_ms` |
| `script http_request refused: … is not in lua.http.allowed_hosts` | 腳本呼叫的主機不在白名單 | 加進 `allowed_hosts` |
| `reload failed, keeping the running configuration: …` | 新設定有錯 | 先 `check`，修正後再 reload |
| `configuration reloaded from …; these need a restart to apply: …` | 有些設定要重啟才生效 | 安排重啟 |

### 指令

| 訊息 | 原因 | 處置 |
| --- | --- | --- |
| `cannot reach the metrics port at 127.0.0.1:9812: Connection refused (os error 61)`（`status`，結束碼 3） | 伺服器沒在跑，或伺服器用的 `[metrics]` 位址與指令讀的設定不同 | 啟動伺服器，或用 `--config` 指向伺服器用的設定 |
| `the cluster did not answer within 30 s`（`user` 等指令，結束碼 3） | 伺服器沒在跑 | 啟動伺服器（[第 03 章](03-quick-start.md)） |
| `rradius-bench` 的 `timeouts 1, … socket errors 1`（結束碼 2） | 伺服器沒在跑或埠不對 | 確認位址與埠 |

`user` 那一列實測時，等待的 30 秒裡還有好幾行 `ERROR hiqlite::client::helpers: Error looking up DB metrics: …`。

## 相關章節

- [03 第一次啟動與登入](03-quick-start.md)
- [04 設定檔的基本功](04-configuration.md)
- [05 RADIUS 服務與 NAS 用戶端](05-radius-clients.md)
- [06 字典與屬性](06-dictionary.md)
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)
- [09 外部資料庫與具名查詢](09-external-databases.md)
- [10 用 Lua 寫政策](10-lua-policies.md)
- [11 802.1X 與 EAP](11-eap.md)
- [12 TACACS+ 設備管理](12-tacacs.md)
- [16 叢集與高可用](16-cluster.md)
- [17 監控、狀態與日誌](17-monitoring.md)
- [18 日常維運](18-operations.md)
- [19 效能調校與壓測](19-performance.md)
- [20 安全強化](20-security.md)
- [附錄 A：設定參考](90-config-reference.md)
