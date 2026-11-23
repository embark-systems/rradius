# 20 安全強化

> 本章讓你做到：
> - 用 `check` 一次找出設定裡所有和安全有關的警告，並逐一處理
> - 收緊祕密檔、共用密碼、Message-Authenticator 與 MS-CHAPv1
> - 設好雜湊成本上限與登入失敗鎖定
> - 限制腳本能做的事，加密到資料庫與 LDAP 的連線
> - 讓指標埠、叢集埠只開在該開的地方

前置條件：讀過[第 03 章](03-quick-start.md)，手上有一份能通過 `check` 的設定。本章的每一項都連到它的主章，這裡只講「怎麼收緊」與「沒收緊時會看到什麼」。

本章小節：

- [警告與錯誤](#警告與錯誤)
- [檢查清單](#檢查清單)
- [用 check 找出所有警告](#用-check-找出所有警告)
- [祕密檔的權限](#祕密檔的權限)
- [共用密碼的長度與範例值](#共用密碼的長度與範例值)
- [Message-Authenticator 與 limit_proxy_state](#message-authenticator-與-limit_proxy_state)
- [關掉 MS-CHAPv1](#關掉-ms-chapv1)
- [儲存的密碼：格式與雜湊成本上限](#儲存的密碼格式與雜湊成本上限)
- [登入失敗鎖定](#登入失敗鎖定)
- [同時處理的上限](#同時處理的上限)
- [腳本：沙箱、時限與 HTTP 白名單](#腳本沙箱時限與-http-白名單)
- [資料庫與 LDAP 的連線加密](#資料庫與-ldap-的連線加密)
- [指標埠只開在內網](#指標埠只開在內網)
- [叢集祕密與節點之間的網路](#叢集祕密與節點之間的網路)
- [trace 會寫出個人資料](#trace-會寫出個人資料)
- [systemd 與 Kubernetes 的限制](#systemd-與-kubernetes-的限制)
- [記憶體裡的金鑰與密碼](#記憶體裡的金鑰與密碼)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

本章的輸出都在一台 macOS 上用 `rradius-server` 0.1.0 實際跑過。日誌行開頭的時間戳記用 `…` 代替；路徑出現在訊息裡的地方，是設定檔裡寫的相對路徑。

## 警告與錯誤

rradius 對安全問題有兩種反應：

| 反應 | 什麼時候 | 結果 |
| --- | --- | --- |
| 警告（`WARN`） | 設定能用，但不安全，例如祕密檔別人讀得到 | `check` 仍印 `configuration ok`、結束碼 0；伺服器照常啟動 |
| 錯誤 | 設定本身不成立，例如叢集密碼太短 | `check` 印出原因、結束碼 2；伺服器不啟動；reload 時保留原本的設定 |

警告不會擋住你，所以很容易被忽略。大部分警告在三個時間點都會出現：`check`、伺服器啟動、每一次 reload。少數只有伺服器實際跑起來才會出現，下面會標出來。

> **注意：** 警告訊息只寫出設定名稱、檔名與權限，從不寫出密碼或金鑰的內容，也不寫出密碼的長度。可以放心把日誌交給別人看。

## 檢查清單

照表逐項確認。每一項的細節在下面的小節。

| 項目 | 怎麼確認做到了 | 小節 |
| --- | --- | --- |
| 祕密檔只有擁有者（和它的群組）讀得到 | `check` 沒有 `has mode` 的警告 | [祕密檔的權限](#祕密檔的權限) |
| `include` 的檔案與目錄別人寫不了 | `check` 沒有 `can be written by its group or by others` | [祕密檔的權限](#祕密檔的權限) |
| 共用密碼是隨機產生、至少 24 個位元組 | `check` 沒有 `shorter than 16 octets`、`public shared secret` | [共用密碼的長度與範例值](#共用密碼的長度與範例值) |
| Message-Authenticator 維持預設 `yes` | `config show` 裡 `require_message_authenticator = "yes"` | [Message-Authenticator 與 limit_proxy_state](#message-authenticator-與-limit_proxy_state) |
| MS-CHAPv1 維持預設關閉（有 `[auth]` 驗證鏈時） | `[auth]` 沒有 `allow_mschapv1 = true` | [關掉 MS-CHAPv1](#關掉-ms-chapv1) |
| 帳號表的密碼都有格式前綴 | 日誌沒有 `no scheme prefix` | [儲存的密碼：格式與雜湊成本上限](#儲存的密碼格式與雜湊成本上限) |
| 開啟登入失敗鎖定 | 設定有 `[auth.lockout]`，`/metrics` 有 `rradius_auth_lockouts_total` | [登入失敗鎖定](#登入失敗鎖定) |
| 腳本只能呼叫白名單上的主機 | `[lua.http]` 的 `allowed_hosts` 只列需要的主機 | [腳本：沙箱、時限與 HTTP 白名單](#腳本沙箱時限與-http-白名單) |
| 資料庫與 LDAP 的連線有加密 | 沒有 `tls = "off"`、`allow_plaintext = true`；啟動日誌沒有 `unencrypted` | [資料庫與 LDAP 的連線加密](#資料庫與-ldap-的連線加密) |
| 指標埠不對外 | `metrics.bind` 是 `127.0.0.1` 或內網位址 | [指標埠只開在內網](#指標埠只開在內網) |
| 叢集埠只綁在內網位址 | `check` 沒有 `cluster.listen is not set` | [叢集祕密與節點之間的網路](#叢集祕密與節點之間的網路) |
| trace 用完就關 | 設定裡沒有留著 `[log.trace]` | [trace 會寫出個人資料](#trace-會寫出個人資料) |

## 用 check 找出所有警告

目標：一次看到目前設定裡所有的安全警告。

`check` 讀設定、字典、腳本與祕密檔，不開任何埠，可以在正式機上隨時跑。拿沒有改過的 quick-start 情境試試：

```console
$ rradius-server check --config rradius.toml
… WARN rradius_core::common::seal: cluster.secret_raft_file: 'raft.secret' has mode 644, which others can reach or its group can write; make it 0600 or 0640
… WARN rradius_core::config: cluster.secret_raft is the public value shipped with the examples; replace it with a random one, for example from `openssl rand -hex 32`
… WARN rradius_core::common::seal: cluster.secret_api_file: 'api.secret' has mode 644, which others can reach or its group can write; make it 0600 or 0640
… WARN rradius_core::config: cluster.secret_api is the public value shipped with the examples; replace it with a random one, for example from `openssl rand -hex 32`
… WARN rradius_core::common::seal: secret_file: 'localhost.secret' has mode 644, which others can reach or its group can write; make it 0600 or 0640
… WARN rradius_core::reload: RADIUS client 'localhost' has the public shared secret shipped with the examples; replace it with a random one, for example from `openssl rand -hex 32`
configuration ok
$ echo $?
0
```

結束碼是 0，但有六個警告：三個檔案的權限太寬，三個密碼是範例附的公開值。處理方式在下面兩節。全部處理完之後，`check` 應該只剩下一行 `configuration ok`。

要在部署腳本裡擋住帶警告的設定，可以把標準錯誤裡的 `WARN` 當成失敗：

```console
$ rradius-server check --config rradius.toml 2>&1 | grep WARN && echo "有警告，先處理"
```

## 祕密檔的權限

目標：讓存放密碼與金鑰的檔案只有該讀的人讀得到。

rradius 讀下列檔案時，會檢查它的權限。只要「其他人」有任何權限，或「群組」可以寫入（權限位元 `0o027` 有任何一位），就記一行警告，但照樣讀取使用：

| 設定 | 主章 |
| --- | --- |
| `radius.clients[].secret_file`、`tacacs.clients[].secret_file` | [05](05-radius-clients.md)、[12](12-tacacs.md) |
| `cluster.secret_raft_file`、`cluster.secret_api_file` | [16](16-cluster.md) |
| `databases.<name>.password_file` | [09](09-external-databases.md) |
| ldap 處理器的 `bind_password_file` | [08](08-ldap-and-ad.md) |
| netlogon 處理器的 `machine_password_file` | [08](08-ldap-and-ad.md) |
| `radius.eap.key_file`（伺服器憑證的私鑰） | [11](11-eap.md) |

警告的格式是「設定名稱: '檔名' has mode 權限」：

```console
$ chmod 644 localhost.secret
$ rradius-server check --config rradius.toml
… WARN rradius_core::common::seal: secret_file: 'localhost.secret' has mode 644, which others can reach or its group can write; make it 0600 or 0640
configuration ok
```

實測各種權限的結果：

| 權限 | 警告 |
| --- | --- |
| `600`、`640`、`400` | 沒有 |
| `644`、`660`、`604` | 有 |

處置：

```console
$ chmod 600 *.secret
```

用套件安裝、以 `rradius` 帳號執行時，讓檔案屬於 root、群組是 `rradius`、權限 `640`，服務帳號就只能讀不能改：

```console
$ sudo chown root:rradius /etc/rradius/*.secret
$ sudo chmod 640 /etc/rradius/*.secret
```

其他幾個檔案的警告長這樣（都實測過）：

```text
… WARN rradius_core::common::seal: radius.eap.key_file: 'server.key' has mode 644, which others can reach or its group can write; make it 0600 or 0640
… WARN rradius_core::common::seal: bind_password_file: 'ldap.secret' has mode 644, which others can reach or its group can write; make it 0600 or 0640
… WARN rradius_core::common::seal: databases.legacy.password_file: 'db.secret' has mode 660, which others can reach or its group can write; make it 0600 or 0640
```

### include 的檔案與目錄

`include` 讀進來的檔案可以新增用戶端、改驗證鏈，等於決定誰能登入。所以檔案本身或它所在的目錄，只要群組或其他人可以寫入，就會有警告：

```console
$ chmod 666 conf.d/log.toml
$ rradius-server check --config rradius.toml
… WARN rradius_core::config: included configuration file conf.d/log.toml can be written by its group or by others, which lets them change who may log in
configuration ok
$ chmod 644 conf.d/log.toml; chmod 777 conf.d
$ rradius-server check --config rradius.toml
… WARN rradius_core::config: the directory of included configuration file conf.d/log.toml can be written by its group or by others, which lets them change who may log in
configuration ok
```

這裡的標準是「別人寫不了」，所以 `644` 的設定檔沒有警告；`664` 就有。

> **警告：** 主設定檔 `rradius.toml` 或它所在的目錄可以被群組或其他人寫入時，啟動、每次 reload 與 `check` 都會警告 `configuration file … can be written by its group or by others, which lets them change who may log in`（目錄則是 `the directory of configuration file …`），和被讀進來的檔案一樣；只可讀不警告。警告不會擋啟動。主設定檔與它所在的目錄仍要自己設成只有 root 能寫。

`include` 的用法見[第 04 章](04-configuration.md)。

## 共用密碼的長度與範例值

目標：每個 NAS 用一把夠長的隨機共用密碼，叢集密碼也一樣。

### 太短

RADIUS 或 TACACS+ 用戶端的共用密碼短於 16 個位元組，會有警告。警告不寫出密碼，也不寫出長度：

```console
$ printf 'short-secret\n' > localhost.secret
$ rradius-server check --config rradius.toml
… WARN rradius_core::reload: RADIUS client 'localhost' has a shared secret shorter than 16 octets; use at least 24 random octets
configuration ok
```

TACACS+ 用戶端也一樣（實測）：

```text
… WARN rradius_core::reload: TACACS+ client 'router1' has a shared secret shorter than 16 octets; use at least 24 random octets
```

TACACS+ 的封包內容只靠共用密碼保護，太短的密碼對它影響更大。

叢集的 `secret_raft`、`secret_api` 則是硬性規定，短於 16 個字元就不能啟動：

```console
$ printf 'abc\n' > raft.secret
$ rradius-server check --config rradius.toml
rradius.toml: cluster.secret_raft must be at least 16 characters
$ echo $?
2
```

### 範例附的公開值

範例情境附的密碼都含有 `change-me`。用戶端的共用密碼或叢集密碼含有這個字串（不分大小寫）時，會有警告：

```text
… WARN rradius_core::reload: RADIUS client 'localhost' has the public shared secret shipped with the examples; replace it with a random one, for example from `openssl rand -hex 32`
… WARN rradius_core::config: cluster.secret_raft is the public value shipped with the examples; replace it with a random one, for example from `openssl rand -hex 32`
```

### 處置

照警告說的產生隨機值。`openssl rand -hex 32` 產生 64 個十六進位字元，遠超過 24 個位元組：

```console
$ openssl rand -hex 32 > nas1.secret
$ chmod 600 nas1.secret
```

```toml
# rradius.toml
[[radius.clients]]
name = "nas1"
address = "192.0.2.10"
secret_file = "nas1.secret"
# …（其他設定略）
```

- 用 `secret_file`，不要把共用密碼直接寫在設定檔的 `secret`。兩者只能設一個，同時設或都不設會得到 `exactly one of secret or secret_file must be set`。
- 每台 NAS（或每個網段）用不同的密碼。一把密碼外洩時，只要換一個用戶端。
- 改共用密碼可以 reload，不必重啟（[第 05 章](05-radius-clients.md)）。

## Message-Authenticator 與 limit_proxy_state

目標：只接受帶有 Message-Authenticator 的 Access-Request。

Message-Authenticator 是用共用密碼對整個封包算出來的簽章，伺服器用它確認請求來自知道共用密碼的 NAS、內容沒有被改過。兩個相關的設定，可以寫在 `[radius]`，也可以寫在個別的 `[[radius.clients]]` 覆寫：

| 設定 | 預設 | reload | 意思 |
| --- | --- | --- | --- |
| `require_message_authenticator` | `"yes"` | 可以 | `yes`：沒有 Message-Authenticator 的 Access-Request 直接丟棄；`no`：接受；`auto`：看伺服器啟動後這個用戶端的第一個請求來決定 |
| `limit_proxy_state` | `"yes"` | 可以 | 只在不要求 Message-Authenticator 時有作用。`yes`：丟棄帶 Proxy-State 卻沒有 Message-Authenticator 的 Access-Request |

Status-Server 不看這兩個設定，一律要有正確的 Message-Authenticator。完整的鍵說明見[附錄 A](90-config-reference.md)，用戶端的寫法見[第 05 章](05-radius-clients.md)。

### 預設就是最安全的

什麼都不設，就是兩個 `yes`。用 `rradius-bench` 的 `--no-message-authenticator` 模擬一台不送 Message-Authenticator 的舊 NAS：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --no-message-authenticator \
    --load pap,concurrency=1 --requests 1 --timeout 2
…
  pap: 0/s, sent 1, accept 0, reject 0, challenge 0, accounting 0, timeouts 1, invalid 0, late 0, socket errors 0, retransmits 0
…
```

伺服器不回應，日誌有一行（同一個埠每秒最多記一次，`discarded` 後面是這段時間累計的數量）：

```text
… WARN rradius_core::radius: RADIUS: discarded 1 Access-Requests lacking a Message-Authenticator, the last from 127.0.0.1; see require_message_authenticator and limit_proxy_state
```

`status` 的 `Discarded: invalid` 會增加。

### 舊 NAS 不送 Message-Authenticator 時

只對那一台 NAS 設 `"no"`，不要改 `[radius]` 的全域值，`limit_proxy_state` 保持 `yes`：

```toml
# rradius.toml
[[radius.clients]]
name = "old-switch"
address = "192.0.2.20"
secret_file = "old-switch.secret"
require_message_authenticator = "no"
```

### 不要用 auto

`auto` 讓伺服器從啟動後這個用戶端的第一個 Access-Request 學：第一個有 Message-Authenticator，之後就一律要求；第一個沒有，之後就一律不要求，直到重啟。偽造的第一個封包就能把保護關掉，所以伺服器學到「不要求」時會記一行警告（每個來源位址一次）：

```text
… WARN rradius_core::lua_engine: RADIUS client 'localhost' (127.0.0.1): the first request had no Message-Authenticator, so require_message_authenticator (auto) became no and stays no until the server restarts; a forged first packet can switch this protection off; set it to "yes" or "no" in the client's settings
```

這是實測：設成 `auto` 之後，先送一個不帶 Message-Authenticator 的請求，之後不帶的請求也都被接受了。

處置：照訊息說的，明確寫 `"yes"` 或 `"no"`。

`auto` 也只能用在單一位址的用戶端。用在網段上，`check` 會拒絕：

```console
$ rradius-server check --config rradius.toml
rradius.toml: radius clients: client '127.0.0.0/24': require_message_authenticator = "auto" needs a single host, not a network
```

`limit_proxy_state = "auto"` 用在網段上的訊息相同，只是設定名稱不同。

## 關掉 MS-CHAPv1

目標：不接受 MS-CHAPv1 登入。

MS-CHAPv1 是很舊的方法。有 `[auth]` 驗證鏈時它**預設就是關的**，什麼都不用寫；沒有 `[auth]`、由 Lua 腳本自己處理 `MS-CHAP-Response` 的設定不在這個開關的範圍，腳本要自己決定。想在設定檔裡明確寫出來時：

```toml
# rradius.toml
[auth]
policy = "first_match"
allow_mschapv1 = false
# …（其他設定略）
```

- 預設 `false`；可以 reload。要讓非用它不可的 NAS 登入，才寫 `allow_mschapv1 = true`。
- 沒寫或 `false` 時，MS-CHAPv1 的請求在任何處理器之前就被拒絕，不查資料庫，也不計入登入失敗鎖定。
- PAP、CHAP、MS-CHAPv2 與 EAP 不受影響。TACACS+ 的 MS-CHAP 登入一樣被拒絕，回 FAIL。
- 值要寫布林值。寫成字串會得到 `` `allow_mschapv1`: invalid type: string, expected a boolean ``。

`rradius-bench` 沒有 MS-CHAPv1 的負載。這裡用一個自己寫的小程式送出一筆帶 `MS-CHAP-Challenge` 與 `MS-CHAP-Response` 的 Access-Request，伺服器回 Access-Reject，日誌是：

```text
… INFO rradius_core::auth_chain: MS-CHAPv1 refused: client localhost, protocol radius, user "dave"; [auth] allow_mschapv1 is not true
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="dave" account="" chain="default" handler="mschapv1_refused" decision="reject"
```

認證紀錄的 `handler="mschapv1_refused"` 讓你在日誌裡找出還有哪些 NAS 在用 MS-CHAPv1。

> **未實機驗證：** 沒有用真的 NAS 或 TACACS+ 設備送過 MS-CHAPv1，只用上面的測試封包跑過 RADIUS 的情況。

## 儲存的密碼：格式與雜湊成本上限

目標：帳號表裡的密碼都標明格式，而且沒有人能用一個超貴的雜湊拖垮伺服器。

### 每個密碼都要有格式前綴

sql 處理器讀到的密碼，以開頭的前綴判斷格式，例如 `{crypt}`、`{argon2}`、`{SHA256}`、`{nthash}`，`{clear}` 表示明文（格式清單見[第 07 章](07-users-and-chains.md)）。沒有前綴的值有兩種情況會記警告：

看起來像雜湊（例如 `$2y$…`）卻沒有前綴：拒絕登入。

```text
… WARN rradius_core::auth_chain: the stored password looks like a hash with no scheme prefix and is refused; add {crypt} or {argon2} before it, or {clear} if it is the password itself user="eve"
```

看起來像十六進位摘要卻沒有前綴：**當成明文比對**。

```text
… WARN rradius_core::auth_chain: the stored password looks like a digest with no scheme prefix and is compared as plaintext; add its prefix ({nthash}, {MD5}, {SHA}, {SHA256}, ...), or {clear} if it is the password itself user="frank"
```

> **警告：** 第二種情況實測過：`frank` 存的是 32 位十六進位的 MD5 摘要，沒有前綴。用原本的密碼登入被拒絕，**用摘要本身當密碼卻登入成功**。拿到帳號表的人可以直接用摘要登入。看到這行警告，一定要補上正確的前綴。

這兩種警告每個帳號在伺服器啟動後只記一次。

### 雜湊成本上限

`{PBKDF2…}`、`{crypt}` 的 `$5$`／`$6$`／bcrypt、`{argon2}` 這些格式，計算成本寫在儲存的值裡面。能寫帳號表的人，可以塞一個迭代次數極大的值，讓每次登入都把 CPU 或記憶體吃光。rradius 在計算之前先讀出成本，超過上限就不算，直接當作密碼錯誤：

| 設定（`[auth.password_limits]`） | 預設 | 限制的對象 |
| --- | --- | --- |
| `pbkdf2_iterations` | 5000000 | PBKDF2、X-PBKDF2 的迭代次數 |
| `crypt_rounds` | 1000000 | `$5$`、`$6$`、`$sha1$` 與延伸 DES（`_` 開頭）crypt 的 rounds |
| `bcrypt_cost` | 15 | bcrypt 的 cost，每加 1 工作量加倍 |
| `argon2_memory_kib` | 262144（256 MiB） | argon2 的記憶體 |
| `argon2_iterations` | 10 | argon2 的迭代次數 |
| `argon2_lanes` | 16 | argon2 的平行度 |

- 沒寫這一節，預設上限照樣生效。可以 reload。
- 每個值都要大於 0。寫 0 會得到 `auth.password_limits.bcrypt_cost must be greater than 0`（換成對應的鍵名），結束碼 2。
- `argon2_memory_kib` 的最壞情況是這個值乘以 CPU 數，因為同時進行的比對數等於 CPU 數。記憶體小的機器要調低。

實測：quick-start 的 `bob` 存的是 cost 10 的 bcrypt。把上限調到 9：

```toml
# rradius.toml
[auth.password_limits]
bcrypt_cost = 9
```

`bob` 用正確的密碼登入也被拒絕，日誌記一次（同一個帳號之後不再記）：

```text
… WARN rradius_core::auth_chain: the stored password asks for more than [auth.password_limits].bcrypt_cost allows and is refused; raise the limit if the hash is meant to be used user="bob" format="crypt" parameter="bcrypt_cost"
```

`rradius_password_checks_total{scheme="crypt",result="failed"}` 也跟著增加。如果你的帳號確實用了更高的成本，照訊息說的調高上限；不然就是有人改了帳號表，要查。

`user add` 存密碼用的 `[accounts] scheme` 見[第 07 章](07-users-and-chains.md)。

## 登入失敗鎖定

目標：同一個帳號從同一台裝置連續猜錯密碼，就暫時鎖住。

預設沒有鎖定，猜幾次都不限制。加上 `[auth.lockout]` 就開始計數：

```toml
# rradius.toml
[auth.lockout]
max_failures = 10
window_seconds = 300
lock_seconds = 300
```

| 設定 | 預設 | 意思 |
| --- | --- | --- |
| `max_failures` | 10 | 在 `window_seconds` 內失敗幾次就鎖住 |
| `window_seconds` | 300 | 從第一次失敗起算的計數時間 |
| `lock_seconds` | 300 | 鎖多久 |
| `max_entries` | 100000 | 每個節點最多記住幾組 |
| `account_max_failures` | 不設 | 同一個帳號不分來源的失敗總數上限，必須大於 `max_failures` |
| `max_in_flight` | `max_failures` 的 5 倍 | 同一組同時在等結果的登入數，多的直接拒絕 |

- 計數的單位是「帳號＋來源裝置」：RADIUS 的來源是 `Calling-Station-Id`，TACACS+ 是 `rem_addr`，沒有時是空字串。
- 鎖住期間直接拒絕，不驗密碼，密碼對也一樣。登入成功會清掉計數。
- 資料庫不能用而失敗的登入不計數；不存在的帳號照樣計數，所以從鎖定行為看不出帳號存不存在。
- 計數放在每個節點的記憶體：reload 保留，重啟歸零，叢集的節點之間不共用。
- 可以 reload。

實測：`max_failures = 3`、`lock_seconds = 60`，`alice` 打錯三次，第四次用正確的密碼：

```text
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="reject"
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="reject"
… INFO rradius_core::auth_chain: auth lockout: client localhost: account "alice" from "" is locked (this origin)
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="reject"
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="lockout" decision="reject"
```

- `from ""` 是空的來源：`rradius-bench` 不送 `Calling-Station-Id`。真的 NAS 會送裝置的 MAC 位址。
- 最後一行 `handler="lockout"`：密碼沒有被檢查。

指標：

```console
$ curl -s http://127.0.0.1:9812/metrics | grep '^rradius_auth_lockout'
rradius_auth_lockout_rejected_total 1
rradius_auth_lockouts_total 1
rradius_auth_lockout_untracked_total 0
rradius_auth_lockout_untracked_total 0
```

`rradius_auth_lockout_untracked_total` 增加，表示表格滿了而且全是鎖住的組，新的組沒被追蹤。實測 `max_entries = 1`、`max_failures = 1`，`alice` 打錯一次被鎖住，接著 `bob` 打錯：

```text
… WARN rradius_core::auth_lockout: auth lockout: the table of 1 keys is full of locks; failures of new keys are not counted
```

`rradius_auth_lockout_untracked_total` 變成 1。這行每分鐘最多記一次。正式環境出現它，通常代表正在被大量猜測，要從網路層擋。

> **警告：** `account_max_failures` 讓猜密碼的人換裝置也沒用，但也讓任何人都能故意打錯，把一個帳號對所有裝置鎖住。要不要設，看你比較怕哪一種。

設定錯誤的例子（結束碼都是 2）：

```text
rradius.toml: auth.lockout.max_failures must be greater than 0
rradius.toml: auth.lockout.account_max_failures must be greater than max_failures (10), not 5
```

驗證鏈的完整說明見[第 07 章](07-users-and-chains.md)。

## 同時處理的上限

核心沒有「每秒幾個請求」這種速率限制的設定。限制來自同時處理的數量：

| 設定 | 預設 | 超過時 |
| --- | --- | --- |
| `radius.max_auth_inflight`、`max_acct_inflight` | 4096 | 丟棄，`Discarded: overload` 增加，日誌 `RADIUS authentication overloaded: discarded … packets` |
| `tacacs.max_connections`、`max_connections_per_source` | 1024、64 | 超過的連線立刻關閉 |
| `tacacs.max_body_len` | 65536 | 封包內容超過就關閉連線 |
| `tacacs.idle_timeout_ms` | 300000 | 這段時間內沒收到完整封包就關閉連線 |
| `auth.lockout.max_in_flight` | 見上一節 | 直接拒絕 |

這些值怎麼依機器調整見[第 19 章](19-performance.md)。

重送偵測（同一個 NAS 重送同一個請求時，回之前的答案）是為了不重複處理，不是安全機制；說明見[第 05 章](05-radius-clients.md)。

## 腳本：沙箱、時限與 HTTP 白名單

目標：腳本出錯或被寫壞時，影響只限於那一個請求。

### 沙箱

Lua 腳本跑在受限的環境裡：沒有讀寫檔案的 `io`，`os.execute` 不存在，`require`、`loadstring`、`getfenv`、`setfenv` 都被拿掉，內建函式庫是唯讀的。實測腳本呼叫 `io.open` 或 `os.execute`，該請求被拒絕，日誌是：

```text
… ERROR rradius_core::lua_engine: Lua script error (radius/authentication): runtime error: api.lua:1: attempt to index nil with 'open'\nstack traceback:\n\tapi.lua:1: in function <api.lua:1>
… ERROR rradius_core::lua_engine: Lua script error (radius/authentication): runtime error: api.lua:1: attempt to call a nil value\nstack traceback:\n\tapi.lua:1: in function <api.lua:1>
```

### 時間與記憶體

| 設定 | 預設 | reload | 意思 |
| --- | --- | --- | --- |
| `lua.max_execution_ms` | 1000 | 可以 | 腳本每個請求自己執行的時間，等資料庫的時間不算 |
| `lua.max_memory_mb` | 256 | 要重啟 | 每個 Lua VM 的記憶體上限 |
| `lua.query_timeout_ms` | 5000 | 可以 | 每個請求等資料庫的總時間 |

實測 `max_execution_ms = 100`，腳本是 `while true do end`，請求被拒絕：

```text
… ERROR rradius_core::lua_engine: Lua script error (radius/authentication): runtime error: script exceeded the execution limit of 100 ms
```

依規格，腳本用 `pcall` 接住這個錯誤也沒用，請求一樣算失敗（這一點沒有另外實測）。

### HTTP 白名單

腳本的 `http_request` 只能呼叫 `[lua.http]` 的 `allowed_hosts` 列出的主機。沒有 `[lua.http]` 這一節，`http_request` 根本不存在：實測呼叫它得到 `attempt to call a nil value`，請求被拒絕。

```toml
# rradius.toml
[lua.http]
allowed_hosts = ["members.example.com", "http://legacy.internal:8080"]
timeout_ms = 1500
```

- 只寫主機名稱：只允許 HTTPS、443 埠。`host:port`：HTTPS、指定的埠。要用明文 HTTP，一定要寫成 `http://host:port`，明確表示你知道它不加密。
- 寫成 `https://…` 會被拒絕：`` lua.http.allowed_hosts: 'https://api.example.com' is not host, host:port or http://host:port ``。
- 空的清單會被拒絕：`lua.http.allowed_hosts must list at least one host`。
- HTTPS 預設信任系統的根憑證，內部 CA 用 `ca_file` 加上去。

實測腳本呼叫不在清單上的 `http://127.0.0.1:8080/login`：

```text
… WARN rradius_core::lua_engine: script http_request refused: http://127.0.0.1 is not in lua.http.allowed_hosts
```

`rradius_script_http_requests_total{result="denied"}` 增加。腳本從 `http_request` 拿到 `nil` 與錯誤訊息，怎麼處理由腳本決定。

組 URL 時，來自使用者的值一律先用 `url_encode` 編碼。腳本的寫法見[第 10 章](10-lua-policies.md)，函式的細節見[附錄 D](93-lua-reference.md)。

## 資料庫與 LDAP 的連線加密

目標：密碼與查詢結果不以明文走在網路上。

### 外部資料庫

`[databases.<name>]` 的 `tls`：

| 值 | 意思 |
| --- | --- |
| `"verify"`（預設） | 加密，並驗證憑證鏈與主機名稱 |
| `"off"` | 不加密 |

設成 `"off"` 而 `host` 不是本機（`localhost` 或 loopback 位址）時，伺服器啟動時會記警告：

```text
… WARN rradius_core::external_db: databases.remote: tls = "off" sends the password and the data to 192.0.2.10 unencrypted
```

> **注意：** 這個警告只在伺服器啟動時出現，`check` 不會印。實測同一份設定 `check` 只印 `configuration ok`。

資料庫用內部 CA 簽的憑證時，用 `ca_file` 指定 CA，不要改成 `"off"`。詳細見[第 09 章](09-external-databases.md)。

### LDAP

ldap 處理器的 `urls`：

- `ldaps://`：TLS 連線。
- `ldap://`：預設用 StartTLS 升級成加密連線。
- `ldap://` 加上 `allow_plaintext = true`：**不加密**，`bind` 檢查密碼時，使用者的密碼以明文送到目錄伺服器。

`allow_plaintext = true` 並使用 `ldap://` 時，啟動、每次 reload 與 `check` 都會警告 `ldap handler ldap://… is used without encryption because allow_plaintext is true`，但 `check` 照樣通過。除非目錄伺服器就在同一台機器，不要開它。內部 CA 用 `ca_file` 指定。詳細見[第 08 章](08-ldap-and-ad.md)。

> **未實機驗證：** 沒有接上真的 LDAP 或 PostgreSQL／MySQL 伺服器測試加密連線，只用 `check` 與一個連不到的位址確認了設定與警告。

## 指標埠只開在內網

`[metrics]` 開的 HTTP 埠有 `/metrics`、`/livez`、`/readyz`，沒有任何驗證，任何連得到的人都能讀。指標裡有用戶端名稱、資料庫名稱、各種計數。

- `metrics.bind` 預設 `127.0.0.1`。要讓 Prometheus 從別台抓，綁在內網或管理網段的位址，不要綁 `0.0.0.0` 到對外的介面。
- 改 `[metrics]` 要重啟。
- `status` 指令讀的就是這個埠，所以 `status` 要在能連到它的地方執行。

細節見[第 17 章](17-monitoring.md)。

## 叢集祕密與節點之間的網路

叢集的 Raft 埠與 API 埠，只靠 `secret_raft`、`secret_api` 保護，連線本身不加密。

- 兩個密碼至少 16 個字元，用 `secret_raft_file`、`secret_api_file` 存放，權限照[祕密檔的權限](#祕密檔的權限)。
- 單機（沒有 `cluster.nodes`）時，叢集埠預設只綁 `127.0.0.1`。
- 有 `cluster.nodes` 卻沒寫 `cluster.listen` 時，叢集埠綁在所有介面，`check` 會警告：

```text
… WARN rradius_core::config: cluster.listen is not set, so the cluster's Raft and API ports are on all interfaces, with nothing but their shared secrets to protect them; set listen to this node's private or WireGuard address, or to "0.0.0.0" to keep it and silence this warning
```

處置：把 `listen` 設成這個節點在內網或 WireGuard 這類加密通道上的位址。節點之間要跨過不可信的網路時，讓 `cluster.nodes` 的 `raft`、`api` 位址都走加密通道。叢集的部署與網路見[第 16 章](16-cluster.md)。

> **未實機驗證：** 多台機器之間的叢集網路與 WireGuard 沒有實機測試。上面的警告在單機上用 `check` 重現過。

## trace 會寫出個人資料

`[log.trace]` 會把指定使用者或用戶端的請求與回應整筆寫進日誌。開著時，伺服器啟動與每次 reload 都會提醒：

```text
… WARN rradius_core::radius: [log.trace] is on for 5 minutes: requests of these users and clients are written out in full, personal data included
```

實測的 trace 行，`User-Password` 與 `Message-Authenticator` 顯示成 `<hidden>`，其他屬性照實寫出：

```text
… INFO rradius_trace: request client="localhost" source=127.0.0.1 user="alice" code=1 attributes="Message-Authenticator = <hidden>, User-Name = alice, User-Password = <hidden>, NAS-IP-Address = 192.0.2.1, NAS-Identifier = rradius-bench"
```

- `minutes` 只能是 1 到 1440，超過範圍會得到 `log.trace.minutes: 2000 is not from 1 to 1440`。時間到了自動停止。
- 排錯完就把 `[log.trace]` 從設定拿掉，並處理好已經寫出的日誌。

trace 的用法見[第 17 章](17-monitoring.md)。

## systemd 與 Kubernetes 的限制

### systemd

套件附的 `packages/common/rradius.service` 已經做了這些限制：

| 設定 | 作用 |
| --- | --- |
| `User=rradius`、`Group=rradius` | 不用 root 執行 |
| `AmbientCapabilities=CAP_NET_BIND_SERVICE`、`CapabilityBoundingSet=CAP_NET_BIND_SERVICE` | 只給綁低埠（TACACS+ 的 49）的權限 |
| `NoNewPrivileges=yes` | 不能再取得更多權限 |
| `ProtectSystem=strict`、`ReadWritePaths=/var/lib/rradius /var/log/rradius` | 整個系統唯讀，只有資料目錄與日誌目錄可寫 |
| `ProtectHome=yes`、`PrivateTmp=yes` | 看不到家目錄，有自己的 `/tmp` |

所以祕密檔屬於 `root:rradius`、權限 `640` 是最合適的：服務讀得到，改不了。你在設定裡另外用到可寫的路徑（例如計費記錄檔放在別處）時，要加進 `ReadWritePaths`，或改放在 `/var/lib/rradius`、`/var/log/rradius` 底下。安裝與 unit 的其他設定見[第 02 章](02-install.md)。

### Kubernetes

`deploy/kubernetes/networkpolicy.yaml`（預設不在 `kustomization.yaml` 裡，要自己加）：

- RADIUS 的 1812、1813／UDP：從任何地方。
- 叢集的 8100、8200：只從其他 rradius pod。
- 指標的 9812：只從標籤 `kubernetes.io/metadata.name: monitoring` 的 namespace。標籤要改成你的 Prometheus 所在的 namespace。

`deploy/kubernetes/secret.example.yaml` 只示範 Secret 的形狀，裡面不是真的密碼。照檔案開頭的註解，用 `openssl rand -hex 32` 產生 `raft` 與 `api`，以 `kubectl create secret` 建立。叢集在 Kubernetes 上的部署見[第 16 章](16-cluster.md)。

> **未實機驗證：** systemd unit 與 NetworkPolicy 只對照了檔案內容，沒有在 Linux 主機或 Kubernetes 叢集上實測。NetworkPolicy 要網路外掛支援才有作用。

## 記憶體裡的金鑰與密碼

rradius 自己的程式碼在用完之後，會覆寫記憶體裡的這些資料：EAP 的 MSK 與推導它的金鑰材料、PAP 請求的密碼、NT 雜湊與從它推導的值、MPPE 金鑰（回覆屬性編碼完之後）。除錯用的輸出格式也不會顯示密碼。

第三方函式庫內部持有的資料、交給腳本的值，以及編譯器留下的複本，不在這個範圍內。

> **未實機驗證：** 這一節依據規格與程式碼，沒有用記憶體傾印驗證。

## 常見錯誤

**症狀：** `check` 印出 `configuration ok`，但前面有好幾行 `WARN`。
**原因：** 警告不影響結束碼。
**解法：** 照[用 check 找出所有警告](#用-check-找出所有警告)逐一處理。部署腳本可以用 `grep WARN` 擋下。

**症狀：** `chmod 600` 之後，用套件安裝的服務讀不到祕密檔，啟動失敗。
**原因：** 檔案屬於 root，服務帳號 `rradius` 讀不到。
**解法：** `chown root:rradius`、`chmod 640`。`640` 不會有警告。

**症狀：** `check` 失敗：`cluster.secret_raft must be at least 16 characters`，結束碼 2。
**原因：** 叢集密碼太短。
**解法：** `openssl rand -hex 32 > raft.secret`。每個節點用同一把。

**症狀：** `check` 失敗：`require_message_authenticator = "auto" needs a single host, not a network`。
**原因：** `auto` 用在網段的用戶端上，或 `[radius]` 設了 `auto`、網段的用戶端沒有覆寫。
**解法：** 改成 `"yes"` 或 `"no"`。

**症狀：** `check` 失敗：`` `require_message_authenticator`: unknown variant, expected one of `yes`, `no`, `auto` ``。
**原因：** 值寫錯，或寫成布林值。
**解法：** 寫成字串 `"yes"`、`"no"`。

**症狀：** 一台舊 NAS 的請求都沒有回應，日誌有 `discarded … Access-Requests lacking a Message-Authenticator`。
**原因：** 那台 NAS 不送 Message-Authenticator。
**解法：** 先看 NAS 能不能開啟。不能的話，只對它的用戶端設 `require_message_authenticator = "no"`。

**症狀：** 某個帳號密碼正確卻一直被拒絕，認證紀錄是 `handler="lockout"`。
**原因：** 這個帳號從這台裝置失敗太多次，被鎖住了。
**解法：** 等 `lock_seconds` 過去；急的話重啟伺服器會清掉所有計數。查一下是誰在猜。

**症狀：** 某個帳號密碼正確卻被拒絕，日誌有 `asks for more than [auth.password_limits]`。
**原因：** 儲存的雜湊成本超過上限。
**解法：** 確認這個雜湊是你們自己產生的，再調高對應的上限；不是的話，查帳號表被誰改過。

**症狀：** `check` 失敗：`` lua.http.allowed_hosts: '…' is not host, host:port or http://host:port ``。
**原因：** 寫了 `https://` 或路徑。
**解法：** HTTPS 只寫主機或 `host:port`；明文 HTTP 寫 `http://host:port`。

**症狀：** 腳本的 HTTP 呼叫失敗，日誌有 `script http_request refused: … is not in lua.http.allowed_hosts`。
**原因：** 主機不在白名單，或埠、協定和白名單上的寫法不同。
**解法：** 把它加進 `allowed_hosts`。改 `[lua]` 可以 reload。

## 相關章節

- [04 設定檔的基本功](04-configuration.md)
- [05 RADIUS 服務與 NAS 用戶端](05-radius-clients.md)
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)
- [08 接上 LDAP 與 Active Directory](08-ldap-and-ad.md)
- [09 外部資料庫與具名查詢](09-external-databases.md)
- [10 用 Lua 寫政策](10-lua-policies.md)
- [11 802.1X 與 EAP](11-eap.md)
- [12 TACACS+ 設備管理](12-tacacs.md)
- [16 叢集與高可用](16-cluster.md)
- [17 監控、狀態與日誌](17-monitoring.md)
- [19 效能調校與壓測](19-performance.md)
- [21 排錯](21-troubleshooting.md)
- [附錄 A：設定參考](90-config-reference.md)
