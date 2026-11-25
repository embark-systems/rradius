# RADIUS 服務與 NAS 用戶端

> 本章讓你做到：
> - 決定 RADIUS 監聽哪些位址與埠
> - 把 NAS 加成用戶端，給每台一個共用密碼
> - 看懂用戶端怎麼比對，以及不明來源的封包去了哪裡
> - 設定 Message-Authenticator 與 Proxy-State 的檢查
> - 理解 Status-Server、重送、同時處理上限，以及伺服器什麼時候「不回答」

前置條件：照[第 4 章](04-configuration.md)做出一份能啟動的設定。本章的驗證都用 `rradius-bench` 從本機送請求，不需要真的 NAS。

本章小節：

- [概念](#概念)
- [監聽位址與埠](#監聽位址與埠)
- [加入 NAS 用戶端](#加入-nas-用戶端)
- [用戶端怎麼比對](#用戶端怎麼比對)
- [更換共用密碼](#更換共用密碼)
- [Message-Authenticator 與 Proxy-State](#message-authenticator-與-proxy-state)
- [Status-Server](#status-server)
- [重送與重複的請求](#重送與重複的請求)
- [同時處理的上限](#同時處理的上限)
- [伺服器不回答的時候](#伺服器不回答的時候)
- [接上一台新的 NAS](#接上一台新的-nas)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

## 概念

NAS（Network Access Server）是使用者實際連上的設備：無線 AP、交換器、VPN 閘道、BRAS。使用者登入時，NAS 把帳號密碼包成 RADIUS 請求送給 rradius，依回答放行或拒絕。

- **用戶端（client）**：rradius 願意回答的 NAS。不在清單上的來源送來的封包一律丟掉，不回答。
- **共用密碼**：每個用戶端和 rradius 各存一份。它用來加密 User-Password、簽回應封包，也用來檢查 Message-Authenticator。兩邊不一致時，NAS 看到的不是拒絕，而是沒有回應或回應無效。
- **兩個埠**：認證埠收 Access-Request，計費埠收 Accounting-Request。兩個埠都收 Status-Server。

RADIUS 走 UDP，NAS 沒收到回答就會重送（retransmit），等一段時間還沒有就換下一台伺服器。所以 rradius 在無法正確回答時寧可不回答，讓 NAS 去問別台（見[伺服器不回答的時候](#伺服器不回答的時候)）。

## 監聽位址與埠

| 鍵 | 預設 | reload | 說明 |
| --- | --- | --- | --- |
| `radius.bind` | `0.0.0.0` | 重啟 | 一個位址，或位址的陣列；每個位址都開認證與計費兩個埠 |
| `radius.auth_port` | `1812` | 重啟 | 認證埠 |
| `radius.acct_port` | `1813` | 重啟 | 計費埠 |
| `radius.sockets` | Linux 上等於工作執行緒數，其他平台 `1` | 重啟 | 每個位址與埠開幾個接收 socket |
| `radius.recv_buffer_bytes` | 系統預設 | 重啟 | 每個 socket 的接收緩衝區，4096 到 1073741824 位元組 |

完整的型別與限制見[附錄 A](90-config-reference.md)。`sockets` 與 `recv_buffer_bytes` 影響的是高流量下的效能，調法在[第 19 章](19-performance.md)。

同時聽 IPv4 與 IPv6 的本機位址：

```toml
# rradius.toml（節錄）
[radius]
bind = ["127.0.0.1", "::1"]
auth_port = 1812
acct_port = 1813
```

啟動時每個位址各記一行：

```text
… INFO rradius_core::radius: RADIUS Server listening on 127.0.0.1:1812 and 127.0.0.1:1813
… INFO rradius_core::radius: RADIUS Server also listening on ::1
```

`bind` 的規則：

- 陣列至少一個位址，同一個位址不能出現兩次。
- `0.0.0.0` 表示所有 IPv4 位址，`::` 表示所有 IPv6 位址。寫了 `0.0.0.0` 就不能再寫別的 IPv4 位址（`::` 同理），但 `["0.0.0.0", "::"]` 可以。
- IPv4 位址不能寫成 IPv6 的樣子（`::ffff:10.0.0.1`），直接寫 `10.0.0.1`。

錯誤訊息見[常見錯誤](#常見錯誤)。

> **注意：** 預設的 `0.0.0.0` 會聽所有網路介面。NAS 只從管理網段來時，把 `bind` 設成那張介面的位址，再搭配防火牆。

## 加入 NAS 用戶端

**目標：** 讓一台固定位址的 AP 和一整個網段的交換器都能送請求，各用自己的共用密碼。

**設定：** 每個用戶端是一個 `[[radius.clients]]`。用戶端改了（含共用密碼）reload 就套用。

| 鍵 | 預設 | 說明 |
| --- | --- | --- |
| `address` | 必填 | IP 位址或 CIDR 網段，例如 `192.0.2.10`、`198.51.100.0/24`、`2001:db8::/32` |
| `name` | `address` 的值 | 日誌、指標與 `[log.trace]` 用的名稱；同一張清單（RADIUS 或 TACACS+）內不能重複，沒寫 `name` 時 `address` 也算，載入時重複會失敗（見〈常見錯誤〉） |
| `secret`、`secret_file` | 剛好設一個 | 共用密碼，或存放它的檔案（[第 4 章](04-configuration.md)） |
| `auth` | 預設的驗證鏈 | 這個用戶端的登入改用哪一條 `[auth.chains.<name>]`（[第 7 章](07-users-and-chains.md)） |
| `eap_methods` | `radius.eap.methods` | 這個用戶端可以用的 EAP 方法，需要 `[radius.eap]`（[第 11 章](11-eap.md)） |
| `require_message_authenticator` | `[radius]` 的值 | 覆寫 `[radius]` 的設定，見下一節 |
| `limit_proxy_state` | `[radius]` 的值 | 覆寫 `[radius]` 的設定，見下一節 |

每個鍵的細節見[附錄 A](90-config-reference.md)。

```toml
# conf.d/20-nas.toml
[[radius.clients]]
name = "branch-ap"
address = "192.0.2.10"
secret_file = "branch-ap.secret"   # openssl rand -hex 32 > branch-ap.secret

[[radius.clients]]
name = "campus"
address = "198.51.100.0/24"
secret_file = "campus.secret"      # openssl rand -hex 32 > campus.secret
```

**驗證：**

```console
$ for f in branch-ap campus; do openssl rand -hex 32 > $f.secret; done
$ chmod 600 *.secret
$ rradiusd check --config rradius.toml
configuration ok
$ kill -HUP <pid>
$ curl -s http://127.0.0.1:9812/metrics | grep 'client_responses_total{client="campus"'
rradius_radius_client_responses_total{client="campus",code="access_accept"} 0
rradius_radius_client_responses_total{client="campus",code="access_reject"} 0
rradius_radius_client_responses_total{client="campus",code="access_challenge"} 0
rradius_radius_client_responses_total{client="campus",code="accounting_response"} 0
```

**結果長什麼樣子：** 每個用戶端都有一組 `rradius_radius_client_responses_total`，以 `name` 為標籤。之後 NAS 送來的請求會在這裡依回應的種類計數，用它就能看出哪台 NAS 有流量、拒絕是不是突然變多。

把同一個共用密碼給 NAS 時，在 NAS 上設定 rradius 的位址、認證埠與計費埠，以及這個密碼。

- 用 `cat campus.secret` 取出密碼貼到 NAS 的設定畫面。檔案結尾的換行會被去掉，不必擔心。
- 少於 16 個位元組，或含有範例附的 `change-me` 的共用密碼會得到警告，見[第 4 章](04-configuration.md)。
- 有些舊設備限制共用密碼的長度或字元；`openssl rand -hex` 只產生 0–9、a–f，大多數設備都接受。

> **注意：** `authorize_group` 只用於 TACACS+ 用戶端，寫在 RADIUS 用戶端時 `check` 失敗。

> **企業版：** 用戶端的其他鍵（RadSec、CoA、第二因素相關）是企業版的功能，核心版寫了不是被忽略就是 `check` 失敗，見[附錄 A](90-config-reference.md)。

## 用戶端怎麼比對

伺服器用封包的**來源 IP** 找用戶端：

- **最長前綴優先。** 同一個來源同時符合好幾個項目時，用網段最小的那一個。所以可以先給整個網段一個用戶端，再為其中某一台另設一個。
- **網段會被正規化。** `10.0.0.5/24` 等於 `10.0.0.0/24`。兩個項目正規化後相同時，`check` 失敗。
- **IPv4-mapped 的 IPv6 來源當成 IPv4 比對。** 在雙協定的 socket 上，`::ffff:192.0.2.10` 送來的封包比對 `192.0.2.10`。
- **不看埠、不看 NAS-IP-Address、不看 NAS-Identifier。** 只看來源位址。NAS 在 NAT 後面時，要寫 NAT 之後的位址。

找不到用戶端的封包直接丟掉，計入 `rradius_radius_discarded_total{reason="unknown_client"}`，並記一行警告（每秒最多一行，避免被灌爆日誌）：

```text
… WARN rradius_core::radius: RADIUS packet from unknown client ::1 discarded
```

**動手做：** 確認「最長前綴優先」。在 `127.0.0.1` 已經是用戶端 `localhost` 的設定裡，再加一個涵蓋它的網段：

```toml
# conf.d/30-net.toml
[[radius.clients]]
name = "loopback-net"
address = "127.0.0.0/8"
secret_file = "net.secret"   # openssl rand -hex 32 > net.secret
```

reload 之後，用兩個密碼各送一次：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
…
  pap: 2109/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --secret "$(cat net.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1 --timeout 1 --retries 0
…
  pap: 0/s, sent 1, accept 0, reject 0, challenge 0, accounting 0, timeouts 1, invalid 0, late 0, socket errors 0, retransmits 0
…
```

**結果長什麼樣子：** 來自 `127.0.0.1` 的請求比對到 `/32` 的 `localhost`，只有它的密碼能用。用網段的密碼送，Message-Authenticator 對不上，請求被當成無效丟掉（`reason="invalid"` 加 1），bench 只看到逾時。

> **注意：** 共用密碼不對時，NAS 看到的通常是「沒有回應」而不是「拒絕」。看到某台 NAS 一直逾時，先看 `rradius_radius_discarded_total{reason="invalid"}` 有沒有跟著增加。

### IPv6 的 NAS

IPv6 的 NAS 和 IPv4 一樣寫成一個用戶端，`address` 寫 IPv6 位址或前綴，`radius.bind` 要包含一個 IPv6 位址（或 `::`）：

```toml
# conf.d/40-v6.toml
[[radius.clients]]
name = "localhost-v6"
address = "::1"
secret_file = "localhost-v6.secret"   # openssl rand -hex 32 > localhost-v6.secret
```

在 `bind = ["127.0.0.1", "::1"]` 的伺服器上，從 `127.0.0.1` 與 `::1` 送的請求各自比對到 `localhost` 與 `localhost-v6`，兩個都被接受。沒有 `localhost-v6` 時，`::1` 的請求被當成不明的來源丟掉，日誌就是上面那行 `unknown client ::1`。

> **注意：** 同一台 NAS 同時用 IPv4 與 IPv6 送請求時，兩個位址要各寫一個用戶端（可以用同一個 `secret_file`）。伺服器不會把它們當成同一台。

## 更換共用密碼

共用密碼外流、或是定期更換時，伺服器與 NAS 兩邊都要換。一個用戶端同時只有一個共用密碼，伺服器不會「新舊兩個都接受」。

**目標：** 換掉 `localhost-v6` 的共用密碼，並確認舊的不再能用。

**設定與驗證：** 產生新的密碼寫進同一個檔案。伺服器在 reload 之前仍用記憶體裡的舊密碼，所以寫入檔案不會馬上影響服務：

```console
$ cp localhost-v6.secret old.secret
$ openssl rand -hex 32 > localhost-v6.secret
$ rradius-bench run --server ::1 --secret "$(cat old.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1 --timeout 1 --retries 0
…
  pap: 1804/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ kill -HUP <pid>
$ rradius-bench run --server ::1 --secret "$(cat old.secret)" … --timeout 1 --retries 0
…
  pap: 0/s, sent 1, accept 0, reject 0, challenge 0, accounting 0, timeouts 1, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server ::1 --secret "$(cat localhost-v6.secret)" … --timeout 1 --retries 0
…
  pap: 2120/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

（第二、三個指令省略了和第一個相同的參數。）

**結果長什麼樣子：** reload 之前舊密碼還能用；reload 之後只有新密碼能用，舊的請求被當成無效丟掉。

實務上的順序：

1. 先寫好新的密碼檔，`check` 一次。
2. 在 NAS 上換成新密碼，緊接著送 SIGHUP。兩步之間 NAS 的請求會逾時，NAS 會重送，所以兩步靠得越近越好。
3. 看 `rradius_radius_discarded_total{reason="invalid"}` 不再增加，`rradius_radius_client_responses_total` 照常增加。
4. 刪掉舊密碼的副本。

一次要換很多台時，不要讓它們共用一個密碼檔：一台一個檔案，才能一台一台換，出事時也只影響一台。

## Message-Authenticator 與 Proxy-State

Message-Authenticator 是 Access-Request 裡的一個簽章屬性。沒有它時，站在 NAS 與伺服器之間的攻擊者有機會把拒絕改成接受（BlastRADIUS 攻擊，利用的正是 Proxy-State 加上沒有簽章的請求）。所以 rradius 預設要求每個 Access-Request 都帶它。

| 鍵 | 預設 | reload | 值 |
| --- | --- | --- | --- |
| `radius.require_message_authenticator` | `yes` | 可 | `yes`、`no`、`auto` |
| `radius.limit_proxy_state` | `yes` | 可 | `yes`、`no`、`auto` |

兩個鍵都可以在用戶端上個別覆寫，鍵名相同。見[附錄 A](90-config-reference.md)。

- `require_message_authenticator = "yes"`：沒有 Message-Authenticator 的 Access-Request 一律丟掉。
- `require_message_authenticator = "no"`：不要求。這時才看 `limit_proxy_state`。
- `limit_proxy_state = "yes"`：帶了 Proxy-State、卻沒有 Message-Authenticator 的 Access-Request 丟掉。沒有 Proxy-State 的照常處理。
- `limit_proxy_state = "no"`：不檢查。
- `auto`：由伺服器啟動後，這個用戶端的**第一個** Access-Request 決定。第一個有 Message-Authenticator，之後就當成 `yes`；沒有，就當成 `no`，並記一行警告。學到的結果在 reload 後仍保留，直到重新啟動。`auto` 只能用在單一位址的用戶端，網段不行。

丟掉的請求計入 `rradius_radius_discarded_total{reason="invalid"}`，並記一行合併過的警告：

```text
… WARN rradius_core::radius: RADIUS: discarded 1 Access-Requests lacking a Message-Authenticator, the last from 127.0.0.1; see require_message_authenticator and limit_proxy_state
```

**動手做：** 讓一台不送 Message-Authenticator 的舊設備可以用。先看預設的行為：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1 \
    --no-message-authenticator --timeout 1 --retries 0
…
  pap: 0/s, sent 1, accept 0, reject 0, challenge 0, accounting 0, timeouts 1, invalid 0, late 0, socket errors 0, retransmits 0
…
```

只對這個用戶端放寬：

```toml
# conf.d/10-clients.toml
[[radius.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "localhost.secret"
require_message_authenticator = "no"
```

reload 之後再送一次同樣的請求，結果是 `accept 1`。再加上 `--attr 33=hex:0102`（帶一個 Proxy-State）送，又變成 `timeouts 1`：`limit_proxy_state` 還是預設的 `yes`。

**結果長什麼樣子：**

| 用戶端設定 | 有 Message-Authenticator | 沒有，也沒有 Proxy-State | 沒有，但有 Proxy-State |
| --- | --- | --- | --- |
| 預設（`yes`） | 處理 | 丟掉 | 丟掉 |
| `require_message_authenticator = "no"` | 處理 | 處理 | 丟掉 |
| 再加 `limit_proxy_state = "no"` | 處理 | 處理 | 處理 |

最後一列只該留給確定沒有代理伺服器在中間、又真的無法更新的設備。

`auto` 從第一個請求學到 `no` 時的警告：

```text
… WARN rradius_core::radius: RADIUS client 'localhost-v6' (::1): the first request had no Message-Authenticator, so require_message_authenticator (auto) became no and stays no until the server restarts; …
```

> **警告：** `auto` 學到的是「第一個請求長什麼樣子」。若第一個請求是攻擊者偽造的，結果就是 `no`。能確定設備支援時，直接寫 `yes`。

## Status-Server

Status-Server（RFC 5997）是 NAS 或監控系統用來問「伺服器還活著嗎」的封包。

- 送到認證埠，回一個沒有屬性的 Access-Accept；送到計費埠，回 Accounting-Response。不會執行任何腳本或驗證鏈，也不寫認證紀錄。
- 一定要有正確的 Message-Authenticator，不論 `require_message_authenticator` 怎麼設。
- 來源要是已知的用戶端，共用密碼要對。
- 不進重複請求的快取，每一個都重新回答。
- 回答的次數計入 `rradius_radius_status_server_total`。
- `radius.status_server = false`（預設 `true`，要重啟）時，Status-Server 一律丟掉。見[附錄 A](90-config-reference.md)。

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --load status-auth,concurrency=1 --requests 1
…
  status-auth: 4144/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --load status-acct,concurrency=1 --requests 1
…
  status-acct: 5369/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

叢集裡失去多數節點而停止服務的節點，不回答認證埠的 Status-Server，讓 NAS 改問其他節點（[第 16 章](16-cluster.md)）。

> **未實機驗證：** 上面這一句與「沒有 Message-Authenticator 的 Status-Server 一律丟掉」依程式碼與規格整理；本章實測的只有帶 Message-Authenticator 的 Status-Server。

## 重送與重複的請求

NAS 沒在逾時內收到回答就重送同一個請求。rradius 用一個快取認出重送：

- 以「收到它的 socket、來源位址與埠、Identifier、Request Authenticator」當作一個請求。
- 原本的請求還在處理時收到重送：丟掉重送，等原本的做完再回答。
- 已經回答過：直接把同一個回應再送一次，不重新驗證、不重新計費。
- 回答後保留 10 秒，最多 500000 筆；快取滿了時，新的請求照常處理，只是不進快取。
- 同一個來源與 Identifier、但 Request Authenticator 不同的封包，是一個新的請求，會取代快取裡的舊項目。
- 重送計入 `rradius_radius_duplicates_total`。

所以一個慢的登入被 NAS 重送好幾次，也只會執行一次驗證、寫一筆認證紀錄。

**驗證：** 用一個要花幾百毫秒的腳本（例如對 `User-Name == 'slow'` 跑一段迴圈），讓 bench 在 0.1 秒就重送：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user slow --password x --load pap,concurrency=1 --requests 1 --timeout 0.1 --retries 3
…
  pap: 6/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 1
…
$ curl -s http://127.0.0.1:9812/metrics | grep '^rradius_radius_duplicates_total'
rradius_radius_duplicates_total 1
```

**結果長什麼樣子：** bench 重送了一次，伺服器認出它是重送，日誌裡這個使用者只有一筆認證紀錄。

## 同時處理的上限

| 鍵 | 預設 | reload |
| --- | --- | --- |
| `radius.max_auth_inflight` | `4096` | 重啟 |
| `radius.max_acct_inflight` | `4096` | 重啟 |

正在處理的認證（或計費）請求到達上限時，新的請求在建立任何工作之前就丟掉，不回答。這樣伺服器在流量暴增時不會把記憶體和 CPU 耗在注定逾時的請求上，NAS 會重送或改問別台。見[附錄 A](90-config-reference.md)。

- 丟掉的請求計入 `rradius_radius_discarded_total{reason="overload"}`。
- 日誌每個埠每秒最多記一行，寫出丟了幾個：`RADIUS authentication overloaded: discarded 1 packets`（計費是 `accounting`）。準確的數量看指標。
- `rradius_radius_requests_limit{kind="auth"}` 是設定的上限，`rradius_radius_requests_in_progress{kind="auth"}` 是現在正在處理的數量。

把上限設成 1、用慢的腳本同時送 8 個請求時，1 個被回答、7 個逾時，`reason="overload"` 是 7。值要設多少、怎麼量測，見[第 19 章](19-performance.md)。

## 伺服器不回答的時候

除了上面說的丟棄，下面這些情況伺服器也故意不回答，讓 NAS 重送或換一台伺服器，而不是給一個錯的拒絕：

| 情況 | 計數 |
| --- | --- |
| 來源不是已知的用戶端 | `reason="unknown_client"` |
| 封包格式錯誤、Message-Authenticator 不對或缺少 | `reason="invalid"` |
| 同時處理的請求到達上限 | `reason="overload"` |
| 決定 Access-Request 時資料庫暫時不能用（逾時、找不到 leader、寫入佇列滿了） | `reason="database"` |
| 叢集節點失去多數、已停止服務 | `reason="no_leader"` |

上表的計數都是 `rradius_radius_discarded_total` 的標籤。資料庫不能用時，日誌是 `RADIUS Access-Request from client <name> not answered: the authentication backend could not be used`。資料說「不」的情況（沒有這個帳號、密碼錯、資料庫都查得到而腳本拒絕）照樣回 Access-Reject；只有「無法判斷」才不回答。計費請求在資料庫不能用時的處理見[第 13 章](13-accounting.md)。

> **未實機驗證：** 資料庫不能用與 `no_leader` 這兩列依程式碼與規格整理，本章沒有實際製造資料庫故障來驗證。叢集的情況見[第 16 章](16-cluster.md)。

另外，回應封包超過 RADIUS 的上限 4096 位元組時，伺服器改送一個不帶屬性的 Access-Reject，並記一行錯誤，見[第 6 章](06-dictionary.md)。

## 接上一台新的 NAS

一台新的 NAS 第一次接上時，照這個順序檢查，大多數問題在前三步就能找到：

1. **伺服器有沒有收到？** 看 `rradius_radius_discarded_total`。`unknown_client` 增加，表示來源位址不在清單上；完全沒有變化，表示封包沒到伺服器（防火牆、NAS 設的位址或埠不對、`radius.bind` 沒聽那張介面）。
2. **共用密碼與 Message-Authenticator 對不對？** `invalid` 增加時，先看日誌有沒有 `lacking a Message-Authenticator`：有，是 NAS 沒送它；沒有，多半是共用密碼不一致。
3. **伺服器有沒有回答？** 看 `rradius_radius_client_responses_total{client="<name>"}`。`access_accept`、`access_reject` 有增加，表示伺服器有回答，問題在驗證本身或 NAS 收回應的那一端。
4. **為什麼被拒絕？** 每一次決定都有一行認證紀錄（`rradius_auth` 的 `decision=`），寫出用了哪條驗證鏈與哪個處理器。還不夠時，用 `[log.trace]` 暫時記下這台 NAS 的完整請求與回應（[第 17 章](17-monitoring.md)）。
5. **計費有沒有進來？** 計費埠的請求也照用戶端計數，`code="accounting_response"` 會增加。計費資料的處理見[第 13 章](13-accounting.md)。

NAS 端常見的設定項目，各家名稱不同，意思大致如下：

| NAS 上的設定 | 要填的值 |
| --- | --- |
| RADIUS 伺服器位址 | rradius 的位址（`radius.bind` 之一，或它前面負載平衡器的位址） |
| 認證埠、計費埠 | `radius.auth_port`、`radius.acct_port`，預設 1812、1813 |
| 共用密碼 | 這個用戶端 `secret_file` 的內容 |
| Message-Authenticator | 打開 |
| 逾時與重送 | 依 NAS 的預設；伺服器會認出重送，不會重複處理 |
| 伺服器狀態檢查 | 支援 Status-Server 時可以打開，NAS 用它判斷伺服器還能不能回答 |

## 常見錯誤

**症狀：** NAS 一直逾時，`rradius_radius_discarded_total{reason="unknown_client"}` 在增加，日誌有 `RADIUS packet from unknown client 203.0.113.7 discarded`。
**原因：** 封包的來源位址不在用戶端清單上，常見於 NAS 在 NAT 後面，或從另一張介面送出。
**解法：** 照日誌裡的位址加一個 `[[radius.clients]]`，或修正 NAS 的來源介面。

**症狀：** NAS 一直逾時，`reason="invalid"` 在增加，日誌有 `discarded … Access-Requests lacking a Message-Authenticator`。
**原因：** NAS 沒送 Message-Authenticator，而預設要求它。
**解法：** 先在 NAS 上打開 Message-Authenticator。設備不支援時，只對那個用戶端寫 `require_message_authenticator = "no"`。

**症狀：** NAS 一直逾時，`reason="invalid"` 在增加，但沒有上面那行警告。
**原因：** 共用密碼兩邊不一致。
**解法：** 從 `secret_file` 重新複製密碼到 NAS；確認沒有被另一個更精確的用戶端項目比對走（[用戶端怎麼比對](#用戶端怎麼比對)）。

**症狀：** `rradius.toml: radius client '127.0.0.1': authorize_group applies only to TACACS+ clients`。
**原因：** 在 RADIUS 用戶端寫了 TACACS+ 才有的 `authorize_group`。
**解法：** 刪掉它；依用戶端決定政策時，RADIUS 用 `auth` 指定驗證鏈，或在腳本裡讀 `client`。

**症狀：** `rradius.toml: radius client '127.0.0.1': unknown auth chain 'vpn' (no [auth.chains.vpn])`。
**原因：** `auth` 指向一條不存在的驗證鏈。
**解法：** 補上 `[auth.chains.vpn]`，或改成正確的名稱。

**症狀：** `rradius.toml: radius client '127.0.0.1': eap_methods needs [radius.eap]`。
**原因：** 沒有設定 EAP 就寫了 `eap_methods`。
**解法：** 先設定 `[radius.eap]`（[第 11 章](11-eap.md)），或刪掉 `eap_methods`。

**症狀：** `radius clients: duplicate client name 'core'`（TACACS+ 清單是 `tacacs clients: …`）。
**原因：** 同一張清單裡有兩個用戶端的名稱相同；沒寫 `name` 的用戶端，名稱就是它的 `address`，所以一個沒寫名稱的 `10.0.0.5` 與另一個 `name = "10.0.0.5"` 也算同名。名稱用來分指標、日誌，以及計費 spool 重放時找用戶端，所以不能重複。大小寫不同算不同名稱，RADIUS 與 TACACS+ 兩張清單之間可以同名。
**解法：** 改掉其中一個的 `name`。改名後兩個用戶端的指標計數器會分開。**升級前要檢查：** 舊版接受重名的設定，升級後啟動與 `check` 會以結束碼 2 失敗。

**症狀：** `radius clients: duplicate client network '10.0.0.0/24'`。
**原因：** 兩個項目正規化後是同一個網段，例如 `10.0.0.5/24` 與 `10.0.0.0/24`。
**解法：** 留一個。要為其中一台另設，寫單一位址（`/32`）。

**症狀：** `radius clients: client '127.0.0.0/8': require_message_authenticator = "auto" needs a single host, not a network`。
**原因：** `auto` 寫在網段用戶端上。
**解法：** 改成 `yes` 或 `no`，或把需要 `auto` 的那台拆成單一位址的用戶端。

**症狀：** `invalid client address 'nas1.example.com'`。
**原因：** `address` 只接受 IP 位址或 CIDR，不接受主機名稱。
**解法：** 寫 NAS 的 IP 位址。

**症狀：** `exactly one of secret or secret_file must be set`、`secret_file 'empty.secret' is empty`、`cannot read secret_file 'missing.secret': No such file or directory (os error 2)`。
**原因：** 共用密碼沒寫、兩個都寫，或檔案是空的、不存在。
**解法：** 只留 `secret_file`，確認檔案存在且有內容。

**症狀：** `rradius.toml: radius.bind lists 0.0.0.0, which takes all of its family, beside another of them`。
**原因：** `bind` 同時寫了 `0.0.0.0` 和另一個 IPv4 位址。
**解法：** 只留 `0.0.0.0`，或只列出要聽的位址。其他 `bind` 的錯誤：`radius.bind must list at least one address`（空陣列）、`'nowhere' is not an address`、`lists 127.0.0.1 twice`、`::ffff:10.0.0.1, an IPv4 address written as IPv6: list it as IPv4`。

**症狀：** `radius.sockets = 4: only Linux shares UDP packets out over several sockets on one port, so it must be 1 here`。
**原因：** 在 Linux 以外的平台把 `sockets` 設成大於 1。
**解法：** 刪掉這一行，用預設值。

> **注意：** 用戶端寫在被讀進來的檔案裡時，上面這些用戶端錯誤的開頭仍是主設定檔的名稱（例如 `rradius.toml:`），不是那個檔案。用 `name` 找出是哪一個用戶端。

## 相關章節

- [04 設定檔的基本功](04-configuration.md)
- [06 字典與屬性](06-dictionary.md)
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)
- [11 802.1X 與 EAP](11-eap.md)
- [13 計費與 session](13-accounting.md)
- [16 叢集與高可用](16-cluster.md)
- [17 監控、狀態與日誌](17-monitoring.md)
- [19 效能調校與壓測](19-performance.md)
- [20 安全強化](20-security.md)
- [附錄 A：設定參考](90-config-reference.md)
- [附錄 C：指標參考](92-metrics-reference.md)
