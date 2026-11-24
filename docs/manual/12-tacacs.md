# 12 TACACS+ 設備管理

> 本章讓你做到：
> - 讓路由器、交換器的管理員用 TACACS+ 登入
> - 依角色給權限等級，限制每個人能下的指令
> - 用授權群組（`authorize_groups`）不寫腳本就授權指令
> - 把每一次登入、登出與指令都記進資料表
> - 看懂 TACACS+ 的回覆狀態與日誌，排除常見的問題

前置條件：讀過[第 03 章](03-quick-start.md)，會啟動伺服器、用 `rradius-bench` 測試；知道驗證鏈是什麼（[第 07 章](07-users-and-chains.md)）。授權與計費要寫 Lua 腳本，語法見[第 10 章](10-lua-policies.md)。本章全部在一台機器上做，用 `rradius-bench` 當設備，不需要真的路由器。

本章小節：

- [TACACS+ 跟 RADIUS 差在哪裡](#tacacs-跟-radius-差在哪裡)
- [一個請求由誰決定](#一個請求由誰決定)
- [回覆狀態](#回覆狀態)
- [準備實驗環境](#準備實驗環境)
- [看懂 device-admin 的設定](#看懂-device-admin-的設定)
- [加入設備](#加入設備)
- [啟動並登入](#啟動並登入)
- [認證方式：ASCII、PAP、CHAP、MS-CHAP](#認證方式asciipapchapms-chap)
- [enable：進入更高的權限等級](#enable進入更高的權限等級)
- [不用驗證鏈：在腳本裡認證](#不用驗證鏈在腳本裡認證)
- [用腳本授權](#用腳本授權)
- [用授權群組授權](#用授權群組授權)
- [記下每一個指令](#記下每一個指令)
- [給使用者看的訊息：server_msg](#給使用者看的訊息server_msg)
- [連線的管理與限制](#連線的管理與限制)
- [資料庫無法使用時](#資料庫無法使用時)
- [設備那一端](#設備那一端)
- [監看 TACACS+](#監看-tacacs)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

## TACACS+ 跟 RADIUS 差在哪裡

RADIUS 多半用在「讓使用者上網」：Wi-Fi、VPN、寬頻。TACACS+（RFC 8907）用在「讓管理員管理設備」：登入交換器的 CLI、決定他能不能下 `configure`、把他下過的指令記下來。rradius 在同一個程式裡兩種都提供，`[radius]` 與 `[tacacs]` 至少要有一個，也可以兩個都開。

兩者在使用上的差別：

| | RADIUS | TACACS+ |
| --- | --- | --- |
| 傳輸 | UDP | TCP，一條連線可以跑好幾個 session |
| 加密 | 只有密碼欄位 | 整個封包內容用共用密碼加密（RFC 8907 的 MD5 pad） |
| AAA | 認證與授權在同一個回覆 | 認證、授權、計費是三種分開的請求 |
| 授權的單位 | 登入時一次給完回覆屬性 | 設備可以每下一個指令就問一次 |
| 本章的設定 | `[radius]`（第 05 章） | `[tacacs]` |

設備送出的授權與計費請求帶著一串 AV pair，例如 `service=shell`、`cmd=show`、`cmd-arg=version`。授權的回覆也是 AV pair，例如 `priv-lvl=15`。哪些 AV pair 有意義由設備決定；rradius 只負責收下、交給腳本或規則，再把結果送回去。

> **注意：** TACACS+ 的加密只靠共用密碼，強度有限。管理網路以外的路段，用 VPN 或其他方式保護。
>
> RFC 8907 的混淆只用 MD5 加共用密碼，不能取代真正的加密。設備與 rradius 之間放在管理網路，共用密碼要夠長（少於 16 個位元組啟動時會有警告，第 20 章）、每台設備不同（用多個 `[[tacacs.clients]]`，各自的 `secret_file`）。

> **企業版：** 企業版另外提供 TACACS+ over TLS（RFC 9887）。

## 一個請求由誰決定

設備連上 TACACS+ 埠時，rradius 先用來源 IP 比對 `[[tacacs.clients]]`，比對不到就立刻關掉連線，一個位元組都不讀。之後同一條連線上的**每個封包**都會再比對一次，所以 reload 改了用戶端清單，已經開著的連線也照新的清單走。

比對到用戶端之後，三種請求各自由不同的東西決定：

| 請求 | 有 `[auth]` 時 | 沒有 `[auth]` 時 |
| --- | --- | --- |
| 認證（authentication） | 這個用戶端的驗證鏈：它的 `auth` 指定的 `[auth.chains.<name>]`，沒指定就是預設鏈 | 最上層腳本，`phase` 是 `authentication` |
| 授權（authorization） | 用戶端有 `authorize_group`：那個授權群組的規則。沒有：最上層腳本，`phase` 是 `authorization` | 同左 |
| 計費（accounting） | 最上層腳本，`phase` 是 `accounting` | 同左 |

- 「最上層腳本」是 `[lua] script` 指定的那一個。腳本裡的全域變數 `protocol` 是 `"tacacs"`，`client` 是用戶端的名稱。
- 驗證鏈只管認證。授權與計費一定經過腳本，除非授權交給了授權群組。
- 同一個腳本也會收到 RADIUS 的請求（如果你開了 `[radius]`），所以腳本開頭要先看 `protocol`。

## 回覆狀態

TACACS+ 的回覆帶一個狀態。你在日誌、測試工具與設備上看到的就是這些：

| 請求 | 狀態 | rradius 什麼時候回 |
| --- | --- | --- |
| 認證 | `PASS` | 驗證鏈或腳本接受 |
| 認證 | `FAIL` | 拒絕：密碼錯、帳號不存在、enable 被拒、太長的密碼等 |
| 認證 | `GETUSER`、`GETPASS` | ASCII 登入時，問帳號、問密碼 |
| 認證 | `RESTART` | 這個認證方式在這裡無法檢查，請設備換一種再試 |
| 認證 | `ERROR` | 無法判斷：資料庫無法使用、封包內容不合規定 |
| 授權 | `PASS_ADD` | 允許，並附上回覆的 AV pair |
| 授權 | `FAIL` | 拒絕 |
| 授權 | `ERROR` | 無法判斷：資料庫無法使用、封包不合規定、回覆的 AV pair 超過 255 個 |
| 計費 | `SUCCESS` | 腳本回傳 `accept`，紀錄寫好了 |
| 計費 | `ERROR` | 腳本沒有接受 |

`FAIL` 與 `ERROR` 的差別很重要：`FAIL` 是「答案就是不行」，設備會告訴管理員登入失敗；`ERROR` 是「這台伺服器現在答不出來」，設備會改問設定裡的下一台 TACACS+ 伺服器。

## 準備實驗環境

本章用 device-admin 情境。它只開 TACACS+：管理員帳號放在叢集資料庫的 `admins` 表，認證交給 sql 處理器，授權與計費交給 `tacacs.lua`。它在原始碼的 `rradius-server/examples/scenarios/device-admin/`。

跟第 03 章一樣，先複製到工作目錄。以下的指令在原始碼 repo 的根目錄執行：

```console
$ export PATH="$PWD/target/release:$PATH"
$ mkdir -p ~/rradius-lab
$ cp -R rradius-server/examples/scenarios/device-admin ~/rradius-lab/
$ sed -i.bak "s#^dictionary = .*#dictionary = \"$PWD/share/dictionary\"#" ~/rradius-lab/device-admin/rradius.toml
$ cd ~/rradius-lab/device-admin
$ for f in raft api tacacs; do openssl rand -hex 32 > $f.secret; done
$ chmod 600 *.secret
```

- 第三個 `.secret` 是 TACACS+ 用戶端的共用密碼。情境附的值是公開的示範值，`check` 會警告 `TACACS+ client 'core-routers' has the public shared secret shipped with the examples`，所以一開始就換掉。
- 如果第 03 章的 quick-start 還在跑，先停掉它：兩個情境用的是同樣的叢集埠與指標埠。

接著改 `rradius.toml` 的三個地方，讓它在本機就能測：

```toml
# rradius.toml（只列出要改的地方）
[tacacs]
bind = "127.0.0.1"        # 原本 "0.0.0.0"
port = 4949               # 原本 49
idle_timeout_ms = 300000

[[tacacs.clients]]
name = "core-routers"
address = "127.0.0.1"     # 原本 "10.50.0.0/24"
secret_file = "tacacs.secret"
```

- 49 是 TACACS+ 的標準埠，小於 1024，一般帳號不能監聽，所以實驗時改成 4949。正式環境要用 49 時，給程式需要的權限，做法見[第 02 章](02-install.md)。
- `rradius-bench` 從 `127.0.0.1` 連過來，所以用戶端的位址改成它。

```console
$ rradius-server check --config rradius.toml
configuration ok
```

## 看懂 device-admin 的設定

拿掉註解之後，設定檔是這樣（第一行的字典路徑已經被 `sed` 換掉）：

```toml
# rradius.toml
dictionary = "…/share/dictionary"

[lua]
script = "tacacs.lua"

[cluster]
data_dir = "data"
secret_raft_file = "raft.secret"
secret_api_file = "api.secret"
migrations_dir = "migrations"

[auth]
policy = "first_match"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM admins WHERE username = ? AND enabled = 1"

[tacacs]
bind = "127.0.0.1"
port = 4949
idle_timeout_ms = 300000

[[tacacs.clients]]
name = "core-routers"
address = "127.0.0.1"
secret_file = "tacacs.secret"

[metrics]
bind = "127.0.0.1"
port = 9812
```

`[tacacs]` 的每一個鍵（完整的型別與說明見[附錄 A](90-config-reference.md)）：

| 設定 | 預設值 | reload | 意思 |
| --- | --- | --- | --- |
| `tacacs.bind` | `0.0.0.0` | 要重啟 | 監聽的位址 |
| `tacacs.port` | `49` | 要重啟 | 監聽的 TCP 埠 |
| `tacacs.max_connections` | `1024` | 要重啟 | 同時開著的連線上限，要大於 0 |
| `tacacs.max_connections_per_source` | `64` | 要重啟 | 同一個來源位址同時開著的連線上限 |
| `tacacs.idle_timeout_ms` | `300000` | 要重啟 | 這麼久沒收到完整的封包就關掉連線，毫秒，要大於 0 |
| `tacacs.max_body_len` | `65536` | 要重啟 | 封包內容的上限，位元組，超過就關掉連線，要大於 0（設 0 載入時就失敗：`tacacs.max_body_len must be greater than 0`） |
| `tacacs.enable` | `deny` | 可以 | enable 請求怎麼處理：`deny` 或 `login` |
| `tacacs.authorize_groups` | 空的 | 可以 | 授權群組：名稱對到一串規則 |
| `tacacs.clients` | 無，至少一個 | 可以 | 允許連線的設備 |

加入或移除整個 `[tacacs]` 要重啟。

示範帳號在 `migrations/2_demo_data.sql`。密碼都存成 bcrypt 雜湊：

| 帳號 | 密碼 | 角色 | 授權的結果（`tacacs.lua` 決定） |
| --- | --- | --- | --- |
| `netadmin1` | `netadmin1-password` | `netadmin` | 權限等級 15，所有指令 |
| `noc1` | `noc1-password` | `noc` | 權限等級 1，只能用列出的唯讀指令 |

`admins` 表另外有 `role` 與 `enabled` 兩欄。`user_query` 有 `AND enabled = 1`，所以停用的帳號等同不存在。

> **警告：** `2_demo_data.sql` 的帳號與密碼是公開的示範值。拿這個情境改成正式環境時，一定要刪掉這個檔案，換成你自己的資料。

## 加入設備

`[[tacacs.clients]]` 跟 `[[radius.clients]]` 用同一份欄位定義，但 TACACS+ 用戶端只用得到這幾個：

| 鍵 | 必填 | 意思 |
| --- | --- | --- |
| `address` | 是 | 設備的 IP 位址，或一整個 CIDR 網段 |
| `name` | | 日誌與指標用的名稱，預設是 `address` 的值 |
| `secret`、`secret_file` | 兩者剛好一個 | 共用密碼。範例一律用 `secret_file` |
| `auth` | | 這台設備的認證改用哪一條 `[auth.chains.<name>]`，預設是預設鏈 |
| `authorize_group` | | 用哪一個授權群組的規則授權（見[用授權群組授權](#用授權群組授權)） |

其他欄位寫在 TACACS+ 用戶端上，`check` 會失敗，例如 `tacacs client 'lab': eap_methods applies only to RADIUS clients`。位址與網段怎麼比對、共用密碼太短時的警告，跟 RADIUS 用戶端相同，見[第 05 章](05-radius-clients.md)。

一個網段裡的所有設備共用一個用戶端項目，也共用同一把共用密碼。不同用途的設備（例如核心路由器與機房的接取交換器）想要不同的密碼、不同的驗證鏈或不同的授權規則，就分成不同的項目：

```toml
# conf.d/tacacs-clients.toml
[[tacacs.clients]]
name = "core-routers"
address = "10.50.0.0/24"
secret_file = "core.secret"        # openssl rand -hex 32 > core.secret

[[tacacs.clients]]
name = "access-switches"
address = "10.60.0.0/16"
secret_file = "access.secret"      # openssl rand -hex 32 > access.secret
auth = "staff"                     # 要先有 [auth.chains.staff]
authorize_group = "noc"            # 要先有 tacacs.authorize_groups.noc
```

用戶端清單可以 reload：送 `SIGHUP` 之後，新的連線與已經開著的連線都照新清單比對。一個來源位址在新清單裡比對不到時，它的連線會在下一個封包到達時被關掉。

## 啟動並登入

```console
$ rradius-server serve --config rradius.toml
… INFO rradius_core::cli: cluster node 1 started
… INFO rradius_core::cli: cluster node 1 has caught up with the cluster
… INFO rradius_core::cli: Servers started. Press Ctrl+C to stop.
… INFO rradius_core::tacacs: TACACS+ Server listening on 127.0.0.1:4949
… INFO rradius_core::metrics: Metrics listening on 127.0.0.1:9812
```

第一次啟動時還有兩行 hiqlite 的 `No metadata exists inside the DB yet`，是正常的（第 03 章）。

另開一個終端機，用 `rradius-bench` 當設備，送一筆 PAP 登入：

```console
$ rradius-bench run --server 127.0.0.1 --tacacs-port 4949 --secret "$(cat tacacs.secret)" \
    --user netadmin1 --password netadmin1-password --load tacacs-pap,concurrency=1 --requests 1
measured 0.049 s after 0.000 s of warmup

tacacs-pap (fixed-concurrency): concurrency 1, 1 threads, 1 sockets over tcp, busiest worker 0%
  tacacs-pap: 20/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    latency us: p50 48922.6, p90 48922.6, p99 48922.6, p99.9 48922.6, max 48922.6
```

- `--tacacs-port` 是 TACACS+ 的埠，預設 49。
- `accept 1` 就是收到 `PASS`；`reject 1` 是 `FAIL`。結束碼跟第 03 章一樣：0 全部接受，1 有拒絕，2 有逾時或無效的回應，3 參數錯誤、什麼都沒送。
- 密碼存成 bcrypt，比對要花時間，所以速率只有每秒二十幾次，這是正常的。

`rradius-bench` 的 TACACS+ 負載有五種：

| `--load` | 送出的請求 |
| --- | --- |
| `tacacs-pap` | PAP 認證，帳號與密碼放在第一個封包 |
| `tacacs-chap` | CHAP 認證，第一個封包帶 PPP id、16 位元組的 challenge 與回應；伺服器的驗證鏈要能比對密碼，否則伺服器回 `RESTART`（見上面〈什麼時候回 `RESTART`〉），bench 算 `invalid` |
| `tacacs-ascii` | ASCII 認證：先等 `GETUSER`、再等 `GETPASS`，一共三個封包 |
| `tacacs-author` | 授權請求，AV pair 是 `service=shell`、`cmd=show`、`cmd-arg=version` |
| `tacacs-acct` | 計費 START，AV pair 是 `task_id`、`start_time`、`service=shell` |

再試幾種：

```console
$ rradius-bench run --server 127.0.0.1 --tacacs-port 4949 --secret "$(cat tacacs.secret)" \
    --user noc1 --password wrong --load tacacs-pap,concurrency=1 --requests 1
…
  tacacs-pap: 25/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --tacacs-port 4949 --secret "$(cat tacacs.secret)" \
    --user noc1 --password noc1-password --load tacacs-ascii,concurrency=1 --requests 1
…
  tacacs-ascii: 26/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    rounds per finished conversation: 3.0
…
```

伺服器的日誌，每次認證一行：

```text
… INFO rradius_auth: authentication protocol="tacacs" client="core-routers" source=127.0.0.1 user="netadmin1" account="netadmin1" chain="default" handler="sql" decision="accept"
… INFO rradius_auth: authentication protocol="tacacs" client="core-routers" source=127.0.0.1 user="noc1" account="noc1" chain="default" handler="sql" decision="reject"
```

格式跟 RADIUS 的認證紀錄相同，只是 `protocol="tacacs"`。授權與計費不寫這種紀錄。

`rradius-bench` 不印授權回覆的內容。要看回覆的 AV pair，或用 `rradius-bench` 沒有的送法，可以用 Python 套件 `tacacs_plus`（`pip install tacacs_plus`）附的 `tacacs_client`：

```console
$ K="$(cat tacacs.secret)"
$ tacacs_client -H 127.0.0.1 -p 4949 -k $K -u noc1 authenticate          # 輸入 noc1-password
$ tacacs_client -H 127.0.0.1 -p 4949 -k $K -u noc1 authorize -c service=shell cmd=
$ tacacs_client -H 127.0.0.1 -p 4949 -k $K -u noc1 authorize -c service=shell cmd=show cmd-arg=running-config
```

> **未實機驗證：** `tacacs_client` 的指令沿用舊版手冊，本版沒有重跑；它的參數依版本可能不同，以 `tacacs_client --help` 為準。本章表格裡的授權結果是用能解開回覆的測試用戶端實測的（見〈用腳本授權〉）。

## 認證方式：ASCII、PAP、CHAP、MS-CHAP

設備送出認證請求時會說明它用哪一種方式（`authen_type`）。rradius 對每一種的處理：

| 方式 | 處理 |
| --- | --- |
| ASCII | 互動式登入。沒帶帳號時回 `GETUSER`（提示文字 `Username: `），再回 `GETPASS`（提示文字 `Password: `，並要設備不要顯示輸入的字），兩者都拿到後才交給驗證鏈 |
| PAP | 帳號與密碼在第一個封包裡，直接交給驗證鏈 |
| CHAP | 驗證鏈裡要有會比對密碼的處理器，用它檢查 CHAP 的回應 |
| MS-CHAP（v1）、MS-CHAPv2 | 同 CHAP |
| ARAP | 一律回 `RESTART` |

**ASCII 登入的限制**（固定值，不是設定）：

- 帳號空白時會再問，三次都空白就回 `FAIL`。
- 一條連線上最多同時有 8 個還沒完成的 ASCII 登入，超過的回 `ERROR`。
- 一個 ASCII 登入要在 60 秒內完成，超過就作廢。
- 設備送 ABORT 時，登入結束，不回覆，連線保持開著。

實測的 ASCII 登入，三問三答：

```text
GETUSER  server_msg="Username: "
GETPASS  server_msg="Password: "（NOECHO）
PASS
```

**長度上限**（固定值）：密碼最長 128 個位元組，ASCII 登入時回答的帳號最長 255 個位元組。超過的直接回 `FAIL`，不進驗證鏈，也不算進 `[auth.lockout]` 的失敗次數。日誌只寫長度，不寫內容：

```text
… INFO rradius_core::tacacs: TACACS+ authentication refused: the password is longer than 128 octets client="core-routers" user="lab1" authen_type=2 length=129
… INFO rradius_core::tacacs: TACACS+ authentication refused: the username is too long client="core-routers" user="" authen_type=1 length=256
```

**CHAP 與 MS-CHAP 需要能比對的密碼。** CHAP 的回應是用明文密碼算出來的，伺服器手上必須有明文（或 `{clear}`）才能檢查；MS-CHAP 系列要明文或 NT 雜湊。存成 bcrypt 的帳號用 CHAP 登入一定失敗，日誌會寫 `handler="no_password_check"`：

```text
… INFO rradius_auth: authentication protocol="tacacs" client="core-routers" source=127.0.0.1 user="netadmin1" account="netadmin1" chain="default" handler="no_password_check" decision="reject"
```

哪些密碼格式能做 CHAP、MS-CHAP，`auth.allow_mschapv1` 怎麼打開 MS-CHAPv1（預設是關的），都在[第 07 章](07-users-and-chains.md)。PAP 與 ASCII 能用任何格式。

**什麼時候回 `RESTART`：** 下面幾種情況，rradius 回 `RESTART`，不跑驗證鏈，也不跑腳本，讓設備改用別的方式（通常是 PAP 或 ASCII）再試：

- 方式是 ARAP。
- 方式是 CHAP 或 MS-CHAP 系列，但這個用戶端的驗證鏈裡沒有會比對密碼的處理器（sql、ldap，或設了 `verifies_password` 的 lua 處理器）。沒有 `[auth]`、完全交給腳本時也是這樣：腳本沒有密碼可以比。
- 方式是 CHAP 或 MS-CHAP 系列，而且是 enable 請求。

CHAP 的 challenge 短於 8 個位元組、MS-CHAP 的 challenge 不是 8 個位元組、MS-CHAPv2 的 challenge 不是 16 個位元組時，回 `FAIL`，並寫一行日誌。

認證動作不是 LOGIN（改密碼 CHPASS、SENDPASS、SENDAUTH）時，一律回 `FAIL`，日誌是 `TACACS+ authentication refused: only LOGIN is supported`。

> **未實機驗證：** MS-CHAP 與 MS-CHAPv2 只對照了程式碼與測試，沒有用真的設備或用戶端登入過。PAP、CHAP 與 ASCII 已用 `rradius-bench` 在本機實測（CHAP 用 `tacacs-chap` 負載，對有驗證鏈的伺服器：密碼正確 `accept`、錯誤 `reject`），但都沒有用真的設備驗證過。

## enable：進入更高的權限等級

在設備上打 `enable`，設備會送一個認證請求，服務欄位是 ENABLE。`tacacs.enable` 決定怎麼回：

| 值 | 行為 |
| --- | --- |
| `deny`（預設） | 一律回 `FAIL`，不問密碼，不跑驗證鏈，也不跑腳本 |
| `login` | 當成一次普通的登入，由驗證鏈或腳本決定 |

預設的 `deny` 下，日誌是：

```text
… INFO rradius_core::tacacs: TACACS+ enable request refused: [tacacs].enable is "deny" client="core-routers" user="netadmin1" priv_lvl=15
```

改成 `login`：

```toml
# rradius.toml（[tacacs] 裡加一行）
[tacacs]
bind = "127.0.0.1"
port = 4949
idle_timeout_ms = 300000
enable = "login"
```

`enable` 可以 reload。送 `SIGHUP` 之後，`netadmin1` 用自己的密碼 enable 會得到 `PASS`，密碼錯則是 `FAIL`。其他值會讓 `check` 失敗：`tacacs.enable must be "deny" or "login", not "ask"`。

`login` 用的是登入密碼，不是另一組 enable 密碼。驗證鏈與腳本看得到兩個數字屬性：`authen_service`（enable 時是 2）與 `priv_lvl`（要進入的等級），想要分開處理，可以在 lua 處理器或腳本裡看這兩個值。下一節的例子就是這樣做的。

**設備端怎麼選：**

- 不要把 enable 交給 rradius，除非你設了 `enable = "login"`。預設的 `deny` 回 `FAIL`，而設備收到 `FAIL` 不會改用本機的 enable 密碼。
- 維持本機的 enable 密碼：設備上用 `aaa authentication enable default enable`。權限等級由登入時授權回覆的 `priv-lvl` 決定，管理員通常不需要 enable。
- 要用 `login`：設備用 `aaa authentication enable default group RRADIUS enable`（Cisco IOS 的寫法，未實機驗證）。這時管理員用自己的登入密碼 enable，所以要在腳本或 lua 處理器裡限制，例如 `authen_service == 2` 時只讓 `netadmin` 角色通過。

## 不用驗證鏈：在腳本裡認證

device-admin 用 sql 處理器認證，這是最簡單的做法。有時你想在認證時多看一些東西，例如「只有 netadmin 能 enable」，這時可以拿掉 `[auth]`，讓最上層腳本自己認證。

把 `rradius.toml` 的 `[auth]` 與 `[[auth.handlers]]` 兩段刪掉，`[tacacs]` 加上 `enable = "login"`，再在 `tacacs.lua` 的 `local user = get_attr('user')` 後面加上認證的部分：

```lua
-- tacacs.lua（加在 local user = get_attr('user') 後面）
if phase == 'authentication' then
  local rows = sql_query("SELECT password, role FROM admins WHERE username = ? AND enabled = 1", {user})
  local account = rows[1]
  if not account or not password_verify(account.password, get_attr('data')) then
    return 'reject'
  end
  -- enable（authen_service 2）只給 netadmin
  if get_attr('authen_service') == 2 and account.role ~= 'netadmin' then
    set_attr('server_msg', 'enable is for netadmin only')
    return 'reject'
  end
  return 'accept'
end
```

- 認證時，腳本看得到的 `user` 是帳號，`data` 是密碼（PAP 是第一個封包裡的密碼，ASCII 是回答 `GETPASS` 的那一個），`authen_type` 是認證方式的數字（ASCII 是 1，PAP 是 2）。
- `password_verify` 用跟驗證鏈相同的方式比對儲存的密碼，bcrypt 等各種格式都能用（第 10 章）。

送 `SIGHUP` 之後實測：

| 請求 | 回覆 |
| --- | --- |
| `noc1` 用 PAP 登入 | `PASS` |
| `noc1` 用錯的密碼 | `FAIL` |
| `netadmin1` 用 ASCII 登入 | `GETUSER`、`GETPASS`、`PASS` |
| `noc1` enable | `FAIL`，`server_msg` 是 `enable is for netadmin only` |
| `netadmin1` enable | `PASS` |
| 任何人用 CHAP | `RESTART`（腳本沒有密碼可以比對 CHAP） |

日誌的認證紀錄變成 `chain="script" handler="script"`：

```text
… INFO rradius_auth: authentication protocol="tacacs" client="core-routers" source=127.0.0.1 user="noc1" account="noc1" chain="script" handler="script" decision="accept"
```

> **注意：** 在腳本裡認證時，`[auth.lockout]` 的登入失敗鎖定不會作用：`password_verify` 不計算失敗次數。需要鎖定時，用驗證鏈（第 07 章）。

只有部分設備需要特別的認證規則時，不必整個拿掉 `[auth]`：在 `[auth.chains.<name>]` 定義一條含 lua 處理器的鏈，讓那些設備的用戶端用 `auth` 選它（第 07 章）。

## 用腳本授權

設備在管理員登入後要開 shell 時，會送一個授權請求；之後每下一個指令（如果設備設定成指令授權）再送一個。用戶端沒有 `authorize_group` 時，這些請求交給最上層腳本，`phase` 是 `authorization`。

腳本看得到的屬性：

| 屬性 | 內容 |
| --- | --- |
| `user`、`port`、`rem_addr` | 帳號、設備上的終端機名稱、管理員從哪個位址連進設備 |
| 每個 AV pair | 以名稱取值，例如 `get_attr('service')` 是 `shell`。`name=value` 與 `name*value` 都一樣取 |
| 重複的 AV pair | 用 `get_attrs`，例如 `get_attrs('cmd-arg')` 拿到所有參數 |
| `priv_lvl`、`authen_method`、`authen_type`、`authen_service` | 請求標頭裡的數字 |

腳本的回傳：

- `accept`：回 `PASS_ADD`。腳本用 `set_attr` 設的每一個屬性變成一個 `name=value` 的 AV pair 送回去。
- 其他：回 `FAIL`。

device-admin 的 `tacacs.lua` 是這樣決定的（節錄重點）：

```lua
-- tacacs.lua（節錄）
if protocol ~= 'tacacs' then
  return 'reject'
end
local user = get_attr('user')
-- …（計費的部分略）
local rows = sql_query("SELECT role FROM admins WHERE username = ? AND enabled = 1", {user})
local role = rows[1] and rows[1].role
if not role or get_attr('service') ~= 'shell' then
  return 'reject'
end
local cmd = get_attr('cmd')
if not cmd or cmd == '' then
  set_attr('priv-lvl', role == 'netadmin' and 15 or 1)   -- 開 shell：回權限等級
  return 'accept'
end
if role == 'netadmin' then
  return 'accept'
end
-- noc：只放行 READ_ONLY 列出的指令；show 只能接 SHOW 列出的項目
```

- 沒有 `cmd`（或 `cmd` 是空的）表示要開 shell，這時回權限等級。
- `noc` 用允許清單，不用禁止清單：看設定檔的指令寫法太多，列不完。

實測的結果（用一個能解開回覆 AV pair 的測試用戶端送出；`rradius-bench` 不印回覆內容）：

| 帳號 | 送出的 AV pair | 回覆 |
| --- | --- | --- |
| `netadmin1` | `service=shell cmd=` | `PASS_ADD`，`priv-lvl=15` |
| `noc1` | `service=shell cmd=` | `PASS_ADD`，`priv-lvl=1` |
| `noc1` | `service=shell cmd=show cmd-arg=version` | `PASS_ADD`，沒有 AV pair |
| `noc1` | `service=shell cmd=show cmd-arg=running-config` | `FAIL` |
| `noc1` | `service=shell cmd=configure` | `FAIL` |

用 `rradius-bench` 也能測，它送的是 `show version`：

```console
$ rradius-bench run --server 127.0.0.1 --tacacs-port 4949 --secret "$(cat tacacs.secret)" \
    --user noc1 --load tacacs-author,concurrency=1 --requests 1
…
  tacacs-author: 2006/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --tacacs-port 4949 --secret "$(cat tacacs.secret)" \
    --user nobody --load tacacs-author,concurrency=1 --requests 1
…
  tacacs-author: 5916/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

`tacacs-author` 不需要 `--password`：授權請求不帶密碼，設備在登入成功後才會送。

回覆的限制（固定值）：一個 AV pair 連名稱最長 255 個位元組，`set_attr` 設了太長的值，腳本會出錯；一個回覆最多 255 個 AV pair，超過時回 `ERROR` 並寫日誌 `has … AV pairs, more than the 255 a reply holds`。

## 用授權群組授權

很多設備只需要簡單的規則：「這一群交換器上，只能下這幾個指令」。這種情況可以不寫腳本，改用授權群組。

授權群組寫在 `[tacacs.authorize_groups]`：每個鍵是群組名稱，值是規則字串的陣列。用戶端用 `authorize_group` 選一個群組：

```toml
# rradius.toml（加在 [tacacs] 後面、[[tacacs.clients]] 前面）
[tacacs.authorize_groups]
noc = [
  "permit service=shell cmd= {priv-lvl=1 autocmd=\"show users\"}",
  "deny service=shell cmd=configure",
  "permit service=shell cmd=show cmd-arg=(version|interfaces|clock)",
  "permit service=shell cmd=(ping|traceroute|exit)",
]

[[tacacs.clients]]
name = "core-routers"
address = "127.0.0.1"
secret_file = "tacacs.secret"
authorize_group = "noc"
```

每一條規則的寫法：

```text
permit|deny  樣式 樣式 …  {回覆的 AV pair …}
```

- 第一個字是 `permit` 或 `deny`。
- 接著是零個或多個樣式，用空白分開。每個樣式是一個正規表示式，要**整個**符合請求裡的某一個 AV pair（`name=value` 或 `name*value`，照設備送來的樣子）。`cmd=` 只符合空的 `cmd=`，不會符合 `cmd=show`。
- 一條規則的每個樣式都各自符合某個 AV pair，這條規則才成立。
- 最後可以接一段 `{…}`，是 `permit` 時要送回去的 AV pair，用空白分開，照寫的順序送出。`deny` 不能有回覆。
- 回覆裡要放空白或 `}` 時用雙引號括起來，例如 `autocmd="show users"`。雙引號會原樣送給設備。
- 樣式不認得雙引號，也不比對 `user`、`port`、`rem_addr` 這三個欄位。

判斷的方式：照寫的順序找**第一條成立的規則**。是 `permit` 就回 `PASS_ADD` 加上它的回覆；是 `deny` 就回 `FAIL`；一條都不成立也是 `FAIL`。

上面的群組，實測結果：

| 送出的 AV pair | 第一條成立的規則 | 回覆 |
| --- | --- | --- |
| `service=shell cmd=` | 第 1 條 | `PASS_ADD`，`priv-lvl=1`、`autocmd="show users"` |
| `service=shell cmd=configure` | 第 2 條 | `FAIL` |
| `service=shell cmd=show cmd-arg=version` | 第 3 條 | `PASS_ADD`，沒有 AV pair |
| `service=shell cmd=show cmd-arg=running-config` | 沒有 | `FAIL` |
| `service=shell cmd=ping cmd-arg=192.0.2.1` | 第 4 條 | `PASS_ADD`，沒有 AV pair |
| `service=ppp protocol=ip` | 沒有 | `FAIL` |

要注意的地方：

- 有 `authorize_group` 的用戶端，授權**完全**由規則決定，最上層腳本不會跑，所以也不會有 `server_msg`。認證與計費照舊。
- 授權群組是「設備的群組」，不是 LDAP 或資料庫裡的使用者群組。規則看不到是誰在下指令：同一台設備上，`netadmin1` 與 `noc1` 得到一樣的答案。要依使用者的角色分開授權，用上一節的腳本。
- 群組可以 reload，跟用戶端清單一起換。實測改了規則、送 `SIGHUP` 之後，下一個授權請求就照新規則。
- 一個群組可以沒有規則，那就是全部拒絕。

**寫規則要小心的地方：**

- **用白名單，不要用黑名單。** 規則只比對設備送來的 AV pair。沒有列出的子命令（例如 `show tech-support`）與設備上的縮寫（例如打 `show run` 而不是 `show running-config`），都不會被 `deny cmd=show cmd-arg=running-config` 擋到，卻符合 `permit cmd=show`。上面的 `noc` 群組只放行列出的指令，其餘落到「沒有任何一條成立」而被拒絕，比較安全。縮寫在設備上實際送出什麼，依設備而定（未實機驗證）。
- **參數不是逐一比對。** 一條規則的每個樣式，只要符合請求裡**任何一個** AV pair 就成立，所以 `cmd-arg=interfaces` 會放行 `show interfaces` 之後再加別的參數。要設備真的唯讀，靠設備上那個權限等級本身能做什麼，授權規則只是第二道防線。
- **設備開 shell 時可能送 `cmd*`。** AV pair 的分隔符號是 `=`（必要）或 `*`（可有可無），照設備送來的樣子比對。設備送 `cmd*`（值是空的）時，樣式要寫成 `cmd\*`，寫 `cmd=` 不會成立：

  ```toml
  [tacacs.authorize_groups]
  core = [
    'permit service=shell cmd\* {priv-lvl=15}',
    'permit service=shell cmd=.*',
  ]
  ```

- **規則一定用單引號。** `\*` 在 TOML 的雙引號字串裡是不合法的跳脫，設定檔讀不進來；單引號的字串不處理跳脫，反斜線原樣交給樣式。把上面的規則寫成雙引號，`check` 會失敗，訊息是 `rradius.toml: line 39: missing escaped value, expected …`（後面列出合法的跳脫字元，行號是你的檔案裡的行）。本節前面的規則用雙引號，是因為裡面只有 `\"` 這種 TOML 本來就懂的跳脫；要讓樣式看到反斜線，就改單引號。

規則寫錯時，啟動、reload 與 `check` 都會失敗，並指出群組名稱與第幾條規則（從 1 開始數）。reload 失敗時，伺服器繼續用原來的設定：

| 寫法 | 錯誤訊息 |
| --- | --- |
| `"allow service=shell"` | `tacacs.authorize_groups.ops: rule 1: starts with 'allow'; it has to start with permit or deny` |
| `"permit service=shell cmd=(show"` | `tacacs.authorize_groups.ops: rule 1: pattern 'cmd=(show' is not a valid regular expression: …` |
| `"permit service=shell {priv-lvl=15"` | `tacacs.authorize_groups.ops: rule 1: the reply has no closing }` |
| `"permit service=shell {priv-lvl=15} x"` | `tacacs.authorize_groups.ops: rule 1: text after the reply's closing }: ' x'` |
| `"deny service=shell {priv-lvl=15}"` | `tacacs.authorize_groups.ops: rule 1: deny cannot have a reply: FAIL carries no AV pairs` |
| `"permit service=shell {priv-lvl}"` | `tacacs.authorize_groups.ops: rule 1: reply AV pair 'priv-lvl' has no = or *` |
| 回覆裡的雙引號不成對 | `tacacs.authorize_groups.ops: rule 1: the reply's double quotes are not paired` |
| `authorize_group = "nosuch"` | `tacacs client 'lab': unknown authorize_group 'nosuch' (no tacacs.authorize_groups.nosuch)` |
| `authorize_group` 寫在 `[[radius.clients]]` | `radius client '<name>': authorize_group applies only to TACACS+ clients` |

每個訊息前面還有設定檔的名稱，例如 `rradius.toml: `。

## 記下每一個指令

設備設定成指令計費時，每次登入、登出、每一個指令都會送一個計費請求。rradius 把它交給最上層腳本，`phase` 是 `accounting`。腳本回傳 `accept` 才回 `SUCCESS`；其他結果都回 `ERROR`，讓設備知道這筆沒有記下來。

計費請求的 AV pair 由設備決定，常見的有：

| AV pair | 意思 |
| --- | --- |
| `task_id` | 這件事的編號，開始與結束用同一個 |
| `start_time`、`stop_time` | 開始或結束的時間；都沒有的是中間的更新（watchdog） |
| `cmd`、`cmd-arg` | 指令與它的參數，參數可以有好幾個 |
| `service` | 例如 `shell` |
| `elapsed_time` 等 | 依設備而定 |

device-admin 把它們寫進 `tacacs_log` 表：

```lua
-- tacacs.lua（節錄）
if phase == 'accounting' then
  local event = (get_attr('stop_time') and 'stop') or (get_attr('start_time') and 'start') or 'update'
  local cmd = get_attr('cmd') or ''
  for _, arg in ipairs(get_attrs('cmd-arg')) do
    if arg ~= '<cr>' then
      cmd = cmd .. ' ' .. arg
    end
  end
  sql_execute([[INSERT OR IGNORE INTO tacacs_log (device, task_id, event, username, rem_addr, cmd, logged)
                VALUES (?, ?, ?, ?, ?, ?, ?)]],
    {client, get_attr('task_id') or '', event, user or '', get_attr('rem_addr') or '', cmd, math.floor(now_unix())})
  return 'accept'
end
```

```sql
-- migrations/1_schema.sql（節錄）
CREATE TABLE tacacs_log (
  device TEXT NOT NULL,          -- [[tacacs.clients]] 的 name
  task_id TEXT NOT NULL,
  event TEXT NOT NULL,           -- 'start'、'stop' 或 'update'
  username TEXT NOT NULL,
  rem_addr TEXT NOT NULL,        -- 管理員從哪裡連進設備
  cmd TEXT NOT NULL,             -- 指令全文；登入、登出是空字串
  logged INTEGER NOT NULL,
  PRIMARY KEY (device, task_id, event)
);
CREATE INDEX tacacs_log_user ON tacacs_log (username, logged);
```

- 指令與參數是分開送的，腳本把它們組回一行，並去掉設備附上的 `<cr>`。
- 主鍵是 `(device, task_id, event)`，加上 `INSERT OR IGNORE`：設備重送同一筆時，表裡只留一筆。計費腳本一定要能重複執行，理由見[第 13 章](13-accounting.md)。
- **`tacacs_log` 會一直變大，rradius 不會自動清除。** 每個指令都是一列。要定期刪除舊的列，做法與計費表相同，見[第 18 章](18-operations.md)；保存多久依公司的稽核要求決定，刪除前先確認。

實測送一筆 `show version` 的計費（AV pair：`task_id=42 start_time=1795000000 service=shell cmd=show cmd-arg=version cmd-arg=<cr>`），回覆是 `SUCCESS`，表裡多了這一列：

```text
device=core-routers
task_id=42
event=start
username=noc1
rem_addr=192.0.2.10
cmd=show version
logged=1795384714
```

用 `rradius-bench` 送：

```console
$ rradius-bench run --server 127.0.0.1 --tacacs-port 4949 --secret "$(cat tacacs.secret)" \
    --user noc1 --load tacacs-acct,concurrency=1 --requests 1
…
  tacacs-acct: 3185/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

`accounting 1` 表示收到 `SUCCESS`。

要把這些紀錄變成「每人每天下了幾個指令」這類指標，用具名查詢（[第 09 章](09-external-databases.md)）。

> **注意：** TACACS+ 的計費沒有計費 spool，也不會寫進 `[radius.accounting_log]`：那兩個功能只處理 RADIUS 的計費。資料庫寫不進去時，腳本出錯，設備收到 `ERROR`，要靠設備自己重送或改送下一台伺服器。

## 給使用者看的訊息：server_msg

腳本用 `set_attr('server_msg', '…')` 設的值，會放進回覆的 `server_msg` 欄位，大多數設備會把它顯示給管理員看。它適用於：

- 認證回覆的 `PASS` 與 `FAIL`。認證接受時，驗證鏈的處理器設的 `server_msg` 也算。
- 授權回覆的 `PASS_ADD` 與 `FAIL`。它不會變成 AV pair，也不算進 AV pair 的 255 位元組上限。
- 計費回覆的 `SUCCESS` 與 `ERROR`。

設了好幾次時用最後一次的值。超過 1024 個位元組的值不會送出，日誌寫一行 ERROR，但決定照舊。授權群組決定的授權沒有 `server_msg`。

例如在 `tacacs.lua` 拒絕 `noc` 的指令前加一行：

```lua
-- tacacs.lua（節錄）
if not READ_ONLY[cmd] then
  set_attr('server_msg', 'noc accounts may only look')
  return 'reject'
end
```

實測 `noc` 帳號的 `configure` 得到 `FAIL`，`server_msg` 是 `noc accounts may only look`。

## 連線的管理與限制

**單連線模式。** 設備在一條連線的第一個封包設了 single-connection 旗標，rradius 在第一個回覆也設這個旗標，表示同意在這條連線上跑多個 session。之後的回覆不再帶旗標。不管有沒有這個旗標，rradius 都在同一條連線上接受多個 session。實測：第一個回覆的旗標是 `0x4`，第二個是 `0x0`，兩次登入都是 `PASS`。

**連線上限。**

- `max_connections`：全部的連線數上限。到了上限，新連線立刻關掉。
- `max_connections_per_source`：同一個來源位址的上限。IPv4 位址與對應的 IPv4-mapped IPv6 位址算同一個來源。
- 不是設定裡的用戶端的連線，不算進這兩個上限。
- 被關掉的連線最多每秒寫一行日誌，並計入 `rradius_tacacs_connections_total{result="refused"}`。

用 `max_connections_per_source = 1` 與兩條同時的連線實測，第二條被關掉：

```text
… WARN rradius_core::tacacs: TACACS+ connection from 127.0.0.1 closed: it already holds max_connections_per_source = 1 connections
```

> **注意：** 很多台設備經過 NAT 從同一個位址連進來時，預設的 64 可能不夠，要調高 `max_connections_per_source`。

**閒置與寫入逾時。** 一條連線 `idle_timeout_ms`（預設 300000 毫秒，也就是 5 分鐘）沒收到完整的封包就關掉；腳本執行的時間不算閒置。回覆在 10 秒內寫不出去也關掉連線（固定值）。被關掉的連線會讓出它在 `max_connections` 裡的位置。

**封包的檢查。** 以下情況直接關掉連線，不回覆：

- 封包內容長度超過 `max_body_len`。
- 版本不是 TACACS+ 的主版本。
- 序號是偶數或 255（RFC 8907 規定設備只送奇數）。
- 設了「不加密」旗標：rradius 一定要有共用密碼。

封包解得開，但欄位不合規定時（權限等級大於 15、認證方式大於 6、服務大於 9、計費請求沒有 START／STOP／WATCHDOG 旗標等），回這種請求的 `ERROR`，不進驗證鏈或腳本，並計入 `rradius_tacacs_malformed_total`。內容完全解不開時（最常見的原因是共用密碼不一致），回 `ERROR` 之後關掉連線，見[常見錯誤](#常見錯誤)。

## 資料庫無法使用時

叢集資料庫無法使用（例如呼叫逾時，或腳本要寫入而叢集沒有 leader）而無法判斷時，認證與授權回 `ERROR`，不是 `FAIL`，讓設備改問下一台 TACACS+ 伺服器。資料本身說「不行」的（帳號不存在、密碼錯），照樣回 `FAIL`。日誌：

```text
TACACS+ authentication for client <名稱> answered ERROR: the authentication backend could not be used
TACACS+ authorization for client <名稱> answered ERROR: the cluster database could not be used
```

叢集設了 `[cluster] stop_without_leader_ms`、而且這個節點沒有 leader 超過這段時間時，認證與授權一律回 `ERROR`，連腳本都不跑。計費照常交給腳本；腳本寫不進資料庫時回 `ERROR`。這個設定與叢集的行為見[第 16 章](16-cluster.md)。

所以設備上至少要設兩台 TACACS+ 伺服器，最好分別指到叢集的不同節點。

> **未實機驗證：** 設了 `stop_without_leader_ms` 時認證回 `ERROR`，[第 16 章](16-cluster.md)在一台機器上模擬三個節點時實測過；授權、計費，以及資料庫呼叫逾時時的回覆，只對照了程式碼與規格，沒有在真的多機叢集上實測。

## 設備那一端

每家設備的指令不同，這裡只列出設備上一定要設定的東西，實際的指令看設備的文件：

- **伺服器位址、埠與 key。** 位址是 rradius 節點的位址，埠是 `tacacs.port`（正式環境通常是 49），key 是用戶端 `secret_file` 的內容。
- **至少兩台伺服器。** 叢集有多個節點時，分別指到不同的節點。rradius 回 `ERROR` 或沒有回應時，設備才會改問下一台。
- **認證（login）用 TACACS+，並保留本機帳號當最後的備援。** 所有 TACACS+ 伺服器都連不上時，管理員還能用設備上的本機帳號登入。
- **授權：** 開 shell（exec）時授權，才拿得到 `priv-lvl`；要逐一檢查指令，再開指令授權。
- **計費：** 開 exec 與指令的計費，才會有登入、登出與每一個指令的紀錄。
- **來源位址。** 設備送出 TACACS+ 用的來源介面，要跟 `[[tacacs.clients]]` 的 `address` 對得上。

以 Cisco IOS 為例，下面是把上述項目串起來的一組設定，位址與 key 換成你自己的：

```text
aaa new-model
tacacs server rradius-1
 address ipv4 10.50.0.100
 key <tacacs.secret 的內容>
aaa group server tacacs+ RRADIUS
 server name rradius-1
!
aaa authentication login default group RRADIUS local
aaa authorization exec default group RRADIUS local
aaa authorization commands 1 default group RRADIUS local
aaa authorization commands 15 default group RRADIUS local
aaa accounting exec default start-stop group RRADIUS
aaa accounting commands 1 default stop-only group RRADIUS
aaa accounting commands 15 default stop-only group RRADIUS
```

> **警告：** 一定要保留 `local` 當備援，並且在改設定之前先確認本機的緊急帳號能登入。rradius 停機或網路不通時，設備會改用本機帳號；沒有備援就會把自己鎖在設備外面。先只開認證（`aaa authentication login`），確認能登入，再開 exec 授權，最後才開指令授權（`aaa authorization commands`）：授權規則設錯時，連 `exit` 都可能被拒絕。多台 rradius 時，在 `aaa group server` 列多個 `server name`，設備會依序嘗試。

> **未實機驗證：** 本章沒有用真的路由器或交換器測試，以上是 TACACS+ 設備通常需要的設定項目；這組 IOS 指令沿用舊版手冊，不同版本的指令可能不同，上線前請在實驗設備上確認。

## 監看 TACACS+

`status` 有一段 TACACS+：

```console
$ rradius-server status --config rradius.toml
…
TACACS+
  Open connections  0
  Authentication    0.0/s (11)
  Authorization     0.0/s (19)
  Accounting        0.0/s (2)
  Malformed         0.0/s (0)
…
```

對應的指標是 `rradius_tacacs_connections_open`、`rradius_tacacs_connections_total{result}`、`rradius_tacacs_requests_total{type}`、`rradius_tacacs_malformed_total` 與 `rradius_tacacs_response_seconds`。一次 ASCII 登入不管來回幾個封包，在 `requests_total{type="authen"}` 只算一次。每個指標的意思見[第 17 章](17-monitoring.md)與[附錄 C](92-metrics-reference.md)。

`Malformed` 增加，最常見的原因是某台設備的共用密碼設錯了。

## 常見錯誤

**症狀：** `rradius-bench` 印出 `lengths: the fields of the reply do not add up to its length, so the shared secret may be wrong`，結果是 `invalid 1`，結束碼 2。伺服器日誌：`TACACS+ connection from 127.0.0.1 closed: a body did not decode, so the shared secret may be wrong`。
**原因：** 設備與用戶端的共用密碼不一致。`rradius_tacacs_malformed_total` 會增加。
**解法：** 確認設備上的 key 與 `secret_file` 的內容相同。檔案最後的換行不算在密碼裡。

**症狀：** 設備連不上，伺服器日誌：`TACACS+ connection from unknown client 127.0.0.1 closed`。
**原因：** 來源位址不在任何一個 `[[tacacs.clients]]` 的 `address` 裡。經過 NAT 時，看到的是 NAT 之後的位址。
**解法：** 把那個位址或網段加進用戶端，送 `SIGHUP`。

**症狀：** 有時連得上、有時被斷線，日誌：`TACACS+ connection from 127.0.0.1 closed: it already holds max_connections_per_source = 1 connections`（位址與數字是實測時的值，你看到的是那個來源與你的設定值）。
**原因：** 同一個來源開了太多連線，常見於 NAT 後面的多台設備，或沒有關閉連線的監控腳本。
**解法：** 調高 `max_connections_per_source`，要重啟才生效。

**症狀：** CHAP 登入失敗，日誌是 `handler="no_password_check" decision="reject"`，同一個帳號用 PAP 卻能登入。
**原因：** 帳號的密碼存成雜湊（例如 bcrypt），CHAP 無法檢查。
**解法：** 設備改用 PAP 或 ASCII；或者那些帳號改存能比對的格式（第 07 章，要考慮資料庫外流的風險）。

**症狀：** 設備用 CHAP 登入時，日誌：`TACACS+ authentication type not supported here: asking the client to restart with another`。
**原因：** 這個用戶端的驗證鏈沒有會比對密碼的處理器（例如沒有 `[auth]`，全部交給腳本），或這是 enable 請求。
**解法：** 加一個 sql 或 ldap 處理器，或讓設備改用 PAP、ASCII。

**症狀：** 打 `enable` 一定失敗，日誌：`TACACS+ enable request refused: [tacacs].enable is "deny"`。
**原因：** `tacacs.enable` 是預設的 `deny`。
**解法：** 設備上改用權限等級授權（登入時就給 `priv-lvl`），或把 `tacacs.enable` 設成 `login`。

**症狀：** 改了 `[tacacs]` 的埠、上限或 `idle_timeout_ms`，送 `SIGHUP` 之後沒有作用，日誌：`configuration reloaded from rradius.toml; these need a restart to apply: tacacs.max_connections_per_source, tacacs.idle_timeout_ms`。
**原因：** 這些鍵要重啟才生效。
**解法：** 排時間重啟伺服器。只有 `enable`、`authorize_groups` 與用戶端清單可以 reload。

**症狀：** `check` 印出 `[tacacs] is enabled but has no [[tacacs.clients]] entries`。
**原因：** 有 `[tacacs]` 卻沒有任何用戶端。
**解法：** 至少加一個 `[[tacacs.clients]]`。

**症狀：** `check` 印出 `tacacs client 'lab': unknown auth chain 'nosuch' (no [auth.chains.nosuch])`。
**原因：** 用戶端的 `auth` 指到不存在的驗證鏈。
**解法：** 先在 `[auth.chains.nosuch]` 定義那條鏈（第 07 章），或改成已有的名稱。

**症狀：** `check` 印出 `tacacs.idle_timeout_ms must be greater than 0` 或 `tacacs.max_connections must be greater than 0`。
**原因：** 這兩個值不能是 0。
**解法：** 改成正數，或刪掉這一行用預設值。

**症狀：** `check` 印出 `rradius.toml: line N: missing escaped value, expected …`，指到 `[tacacs.authorize_groups]` 的一條規則。
**原因：** 規則用雙引號寫，裡面有 `cmd\*` 這種反斜線，TOML 的雙引號字串不接受。
**解法：** 改用單引號（見〈用授權群組授權〉）。

**症狀：** 授權一律 `FAIL`，腳本裡的 `log` 都沒有印出來。
**原因：** 這個用戶端設了 `authorize_group`，授權交給規則，腳本不會跑；規則一條都不成立就是 `FAIL`。
**解法：** 檢查規則的樣式是否整個符合設備送來的 AV pair（例如 `cmd=show` 不會符合 `cmd=show interfaces`，參數在另一個 `cmd-arg` 裡），或拿掉 `authorize_group` 改用腳本。

**症狀：** 計費一直收到 `ERROR`，伺服器日誌有 `Lua script did not accept accounting` 或腳本錯誤。
**原因：** 腳本沒有回傳 `accept`，或執行時出錯。
**解法：** 照日誌修正腳本。腳本開頭用 `protocol` 與 `phase` 分流時，別讓 TACACS+ 的計費落到 `return 'reject'`。

## 相關章節

- [03 第一次啟動與登入](03-quick-start.md)
- [05 RADIUS 服務與 NAS 用戶端](05-radius-clients.md)：用戶端的比對方式與共用密碼
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)：驗證鏈、密碼格式、`auth.chains`
- [08 接上 LDAP 與 Active Directory](08-ldap-and-ad.md)：用目錄的帳號登入設備
- [09 外部資料庫與具名查詢](09-external-databases.md)：把計費紀錄變成指標
- [10 用 Lua 寫政策](10-lua-policies.md)：`get_attr`、`get_attrs`、`set_attr`、`sql_execute`
- [13 計費與 session](13-accounting.md)：計費腳本為什麼要能重複執行
- [16 叢集與高可用](16-cluster.md)：`stop_without_leader_ms`
- [17 監控、狀態與日誌](17-monitoring.md)
- [附錄 A：設定參考](90-config-reference.md)
- [附錄 C：指標參考](92-metrics-reference.md)
