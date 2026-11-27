# 使用者帳號與驗證鏈

> 本章讓你做到：
> - 用 `user` 指令新增、改密碼、停用、刪除本機帳號
> - 依 NAS 用的認證協定選擇密碼格式，把舊系統的雜湊原樣搬進來
> - 用驗證鏈組合多個處理器，讓不同的 NAS 走不同的鏈
> - 統一使用者名稱的大小寫與 realm
> - 限制猜密碼：登入失敗鎖定與雜湊成本上限

前置條件：做完[第 3 章](03-quick-start.md)，手上有一份能啟動的 quick-start 複本（`~/rradius-lab/quick-start`），而且照第 3 章加上了 `[accounts]`。LDAP 與 Active Directory 的處理器在[第 8 章](08-ldap-and-ad.md)。

本章小節：

- [驗證鏈怎麼做決定](#驗證鏈怎麼做決定)
- [用 user 指令管理帳號](#用-user-指令管理帳號)
- [選擇密碼格式](#選擇密碼格式)
- [搬入舊系統的密碼雜湊](#搬入舊系統的密碼雜湊)
- [限制雜湊的計算成本](#限制雜湊的計算成本)
- [認證協定與 MS-CHAPv1 開關](#認證協定與-ms-chapv1-開關)
- [sql 處理器](#sql-處理器)
- [lua 處理器](#lua-處理器)
- [all_accept：密碼之外再加一道政策](#all_accept密碼之外再加一道政策)
- [讓不同的 NAS 走不同的鏈](#讓不同的-nas-走不同的鏈)
- [統一使用者名稱與 realm](#統一使用者名稱與-realm)
- [登入失敗鎖定](#登入失敗鎖定)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

本章的指令都在 quick-start 複本的目錄裡執行，伺服器在另一個終端機跑著。登入測試照第 3 章用 `rradius-bench`，下文把共同的參數省略成 `…`：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
```

輸出只抄 `accept`、`reject` 那一段。

## 驗證鏈怎麼做決定

設定檔裡有 `[auth]` 時，每一筆 RADIUS Access-Request 和 TACACS+ 的登入都交給驗證鏈決定。沒有 `[auth]` 時，由 `[lua]` 的最上層腳本自己決定，那是[第 10 章](10-lua-policies.md)的寫法。

一條鏈是一串依序執行的處理器（handler）。核心有四種處理器：

| `type` | 做什麼 | 本書 |
| --- | --- | --- |
| `sql` | 在叢集資料庫或外部資料庫查帳號，比對密碼 | 本章、[第 9 章](09-external-databases.md) |
| `lua` | 跑一個 Lua 腳本，由腳本決定 | 本章、[第 10 章](10-lua-policies.md) |
| `ldap` | 在 LDAP 目錄找使用者，驗證密碼 | [第 8 章](08-ldap-and-ad.md) |
| `netlogon` | 把 MS-CHAPv2 交給 Active Directory 網域控制站驗證 | [第 8 章](08-ldap-and-ad.md) |

每個處理器對一筆請求給出四種答案之一：

| 答案 | 意思 | 例子 |
| --- | --- | --- |
| accept | 接受 | 密碼正確 |
| reject | 拒絕，鏈立刻停止 | 密碼錯誤、查到兩筆同名帳號 |
| ignore | 無法判斷，交給下一個處理器 | 查不到這個帳號；這種登入方式不歸它管 |
| challenge | 要 NAS 再問使用者一次（Access-Challenge），鏈立刻停止 | Lua 腳本要求輸入驗證碼 |

`[auth]` 的 `policy` 決定怎麼把答案合起來：

- `first_match`（預設）：第一個不是 ignore 的答案就是結果。
- `all_accept`：每個處理器都要跑；任何一個 reject 就拒絕；最後至少要有一個 accept。

每個處理器都 ignore 時，結果是拒絕。所以「查不到帳號」和「密碼錯誤」對 NAS 來說都是 Access-Reject，差別只在日誌裡：伺服器每做一次決定，就寫一行認證紀錄，`handler` 寫的是誰做了決定。

```text
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="reject"
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="nobody" account="nobody" chain="default" handler="none" decision="reject"
```

第一行是 sql 處理器拒絕了 `alice`（密碼錯）；第二行沒有處理器認得 `nobody`，所以是 `handler="none"`。`handler` 還可能是下面幾個值，本章後面都會遇到：

| `handler` | 意思 |
| --- | --- |
| `sql`、`lua`、`ldap`、`netlogon` | 這個處理器做了決定 |
| `none` | 每個處理器都 ignore |
| `normalization` | 使用者名稱不符合 `[auth.username]`，沒有執行任何處理器 |
| `no_password_check` | `all_accept` 的鏈裡，沒有一個比對密碼的處理器接受 |
| `lockout` | 這個帳號被[登入失敗鎖定](#登入失敗鎖定)擋下，沒有比對密碼 |
| `mschapv1_refused` | 預設（沒寫或 `allow_mschapv1 = false`），拒絕 MS-CHAPv1 |
| `length` | RADIUS 的 User-Password 長度不是 16 到 128 個位元組、也不是 16 的倍數，沒有執行任何處理器（見[密碼長度](#密碼長度)） |

### 拒絕與「後端不能用」

還有一種結果不是 accept 也不是一般的 reject：處理器因為後端不能用而失敗。例如叢集資料庫或外部資料庫查詢失敗、LDAP 伺服器全部連不上、網域控制站都沒有回應，或是慢雜湊排隊比對超過了處理器的 `timeout_ms`。

這時候：

- RADIUS **不回應**這筆請求，並在 `rradius_radius_discarded_total{reason="database"}` 加一（標籤固定叫 `database`，不論是哪一種後端）。NAS 收不到回應，會重送或改問另一台伺服器。
- TACACS+ 回 ERROR。
- 這筆不算一次登入失敗，[登入失敗鎖定](#登入失敗鎖定)不計數。

這樣設計的原因：後端故障時回 Access-Reject，NAS 會把它當成「密碼錯了」，使用者被擋在外面，也可能被 NAS 自己的失敗計數鎖住。不回應讓 NAS 有機會去問別台。`status` 的 `Discarded: database` 那一行就是這個計數（[第 17 章](17-monitoring.md)）。

### 收到的名稱與帳號名稱

認證紀錄裡有 `user` 與 `account` 兩欄：

- `user` 是請求裡的 `User-Name`，原樣記下。
- `account` 是處理器拿去查的帳號名稱。有 `[auth.username]` 時，這是去掉 realm、轉成小寫之後的名稱（見[統一使用者名稱與 realm](#統一使用者名稱與-realm)）；用了 `default_user` 時，這是代替的那個帳號。

## 用 user 指令管理帳號

`rradiusd user` 有六個子指令：

```text
rradiusd user add|passwd|enable|disable|delete|show [--config <path>] <name>
```

它們不直接碰資料表的結構，只執行你在 `[accounts]` 寫好的 SQL。所以帳號表長什麼樣子由你決定，`user` 指令照你的表去寫。

```toml
# rradius.toml（第 3 章加上的那一節）
[accounts]
scheme = "bcrypt"
add = "INSERT INTO users (username, password) VALUES (?1, ?2)"
passwd = "UPDATE users SET password = ?2 WHERE username = ?1"
enable = "UPDATE users SET enabled = 1 WHERE username = ?1"
disable = "UPDATE users SET enabled = 0 WHERE username = ?1"
delete = "DELETE FROM users WHERE username = ?1"
show = "SELECT username, enabled FROM users WHERE username = ?1"
```

| 鍵 | 預設 | 用在 | 說明 |
| --- | --- | --- | --- |
| `accounts.scheme` | `bcrypt` | `add`、`passwd` | 存密碼的格式，見下一節 |
| `accounts.add` | — | `user add` | `?1` 帳號名稱，`?2` 要存的密碼值 |
| `accounts.passwd` | — | `user passwd` | 同上 |
| `accounts.enable`、`disable`、`delete` | — | 同名子指令 | `?1` 帳號名稱 |
| `accounts.show` | — | `user show` | 查詢結果原樣印出，一欄一行 |

沒寫的那一句，對應的子指令就不能用。`[accounts]` 只有 `user` 指令讀，每次執行時讀一次，跟 reload 無關（附錄 A 標「指令」）。完整欄位見[附錄 A](90-config-reference.md)。

幾個要注意的地方：

- `disable` 只是改一個欄位。它要有效果，sql 處理器的 `user_query` 必須略過停用的帳號，例如 quick-start 的 `… WHERE username = ? AND enabled = 1`。
- `show` 不要選出密碼欄，它會原樣印在終端機上。
- 指令透過叢集的 API 埠寫入，所以**伺服器要在執行中**。寫入經過叢集，每個節點馬上都看得到，下一次登入就用新的資料，不必 reload。指令本身不啟動節點，也不建立 `data_dir`。
- 有 `[auth.username]` 時，指令會先把名稱正規化，跟登入時一樣（見[統一使用者名稱與 realm](#統一使用者名稱與-realm)）。

### 目標：新增帳號、改密碼、停用、刪除

```console
$ printf 'erin-password\n' | rradiusd user add --config rradius.toml erin
added erin
$ printf 'erin-new-password\n' | rradiusd user passwd --config rradius.toml erin
password changed for erin
$ rradiusd user disable --config rradius.toml erin
disabled erin
$ rradiusd user enable --config rradius.toml erin
enabled erin
$ rradiusd user delete --config rradius.toml erin
deleted erin
$ rradiusd user show --config rradius.toml erin
no account erin
```

密碼一律從標準輸入讀，不從命令列參數讀（參數會出現在行程清單裡）。在終端機上直接執行 `add` 或 `passwd` 時，指令關掉回顯，問兩次；兩次不同就什麼都不寫，結束碼 2。用管線餵進去時只讀取第一行，去掉行尾的換行。空密碼一律拒絕。

結束碼：

| 結束碼 | 意思 | 例子 |
| --- | --- | --- |
| 0 | 完成 | `added erin` |
| 1 | 沒有符合的帳號，或資料庫拒絕這句 SQL | `no account nobody`；帳號已經存在時 `add` 失敗 |
| 2 | 指令、設定或輸入有錯，什麼都沒送出 | `accounts.add is not set in rradius.toml`、`the password is empty; nothing was changed` |
| 3 | 連不到叢集 | `the cluster did not answer within 30 s` |

**已驗證**：上面每一行輸出，以及 `user add` 已存在的帳號時結束碼 1，訊息是資料庫的錯誤（`Connect: Error code 1555: constraint failed Some("UNIQUE constraint failed: users.username")`）。

## 選擇密碼格式

### user 指令存的四種格式

`accounts.scheme` 決定 `user add` 與 `user passwd` 存成什麼。它只影響之後寫入的密碼，已經存在的值不會改。

| `scheme` | 存成 | PAP | CHAP | MS-CHAPv1、MS-CHAPv2、PEAP |
| --- | --- | --- | --- | --- |
| `bcrypt`（預設） | `{crypt}$2b$…` | 可 | 不可 | 不可 |
| `sha512-crypt` | `{crypt}$6$…` | 可 | 不可 | 不可 |
| `nthash` | `{nthash}` 加 32 個十六進位數字 | 可 | 不可 | 可 |
| `clear` | `{clear}` 加明文 | 可 | 可 | 可 |

為什麼有這些限制：

- CHAP 的回應是用明文密碼算出來的，伺服器手上一定要有明文。
- MS-CHAPv1、MS-CHAPv2（PEAP 內層也是 MS-CHAPv2）用的是密碼的 NT hash，所以存 NT hash 或明文都可以。
- bcrypt 與 SHA-512 crypt 是單向雜湊，只能驗證 PAP 送來的明文密碼。

選法：

1. NAS 只用 PAP（多數 VPN 閘道、網頁登入、TACACS+ 的 PAP）：用預設的 `bcrypt`。
2. 有 Wi-Fi 802.1X 的 PEAP，或 NAS 用 MS-CHAPv2：用 `nthash`。
3. 一定要 CHAP：只能 `clear`。
4. `bcrypt` 每次比對都很花 CPU（cost 10）。登入量很大、只用 PAP 時，`sha512-crypt` 比對快得多。

**已驗證**：四種格式各新增一個帳號，存下的值開頭如上表；再分別用 PAP、CHAP、MS-CHAPv2 登入，結果如上表（MS-CHAPv1 `rradius-bench` 沒有這種負載，依程式碼與 spec）。

> **警告：** `nthash` 與 `clear` 拿到資料庫的人就能登入：NT hash 本身就能算出 MS-CHAPv2 的回應，不必知道原來的密碼。只在需要時用，並保護好叢集的資料目錄與備份。

### 伺服器看得懂的全部格式

`user` 指令只會寫上面四種，但 sql 處理器比對時看得懂更多格式，方便從其他系統搬帳號。存放的值寫成 `{格式}值`，前綴不分大小寫。

| 前綴 | 值的內容 | 能驗證的協定 |
| --- | --- | --- |
| （沒有前綴）、`{clear}` | 明文 | PAP、CHAP、MS-CHAPv1、MS-CHAPv2 |
| `{nthash}` | 32 個十六進位數字（MD4 of UTF-16LE） | PAP、MS-CHAPv1、MS-CHAPv2 |
| `{crypt}` | crypt(3) 的結果：傳統 DES、`$1$`（MD5-crypt）、`$5$`、`$6$`、`$2a$`／`$2b$`／`$2y$`（bcrypt） | PAP |
| `{argon2}` | PHC 字串，例如 `$argon2id$v=19$…` | PAP |
| `{SHA}`、`{MD5}`、`{SHA224}`、`{SHA256}`、`{SHA384}`、`{SHA512}` | 摘要的 Base64 | PAP |
| `{SSHA}`、`{SMD5}`、`{SSHA224}`、`{SSHA256}`、`{SSHA384}`、`{SSHA512}` | 摘要後面接著鹽，整串的 Base64；摘要算的是密碼接鹽，鹽至少 1 個位元組 | PAP |
| `{PBKDF2}`、`{PBKDF2-SHA1}`、`{PBKDF2-SHA256}`、`{PBKDF2-SHA512}` | `<迭代次數>$<鹽>$<摘要>`，鹽與摘要用改寫的 Base64（`.` 代替 `+`、不補 `=`）；`{PBKDF2}` 等於 `{PBKDF2-SHA1}` | PAP |
| `{X-PBKDF2}` | `<演算法>:<迭代次數>:<鹽>:<摘要>`，演算法是 `HMACSHA1` 或 `HMACSHA2+224`／`+256`／`+384`／`+512`，迭代次數是 4 個位元組的 Base64 | PAP |
| `{NS-MTA-MD5}` | 32 個十六進位數字加上 32 個字元的鹽（舊郵件伺服器的格式） | PAP |

規則：

- 不認得的前綴，或解析不了的值，永遠比對失敗，不會讓伺服器出錯。
- `{clear}` 後面什麼都沒有、或欄位是 NULL、空字串，都表示「沒有密碼」，任何登入都拒絕。空的 PAP 密碼也不會因此通過。
- `{crypt}`、`{argon2}` 與 PBKDF2 這幾種故意算得很慢的格式，在處理請求以外的執行緒上比對，同時進行的數量最多等於 CPU 數。排隊加上比對超過 sql 處理器的 `timeout_ms`（預設 5000 毫秒），這筆算「後端不能用」，不回應（見[拒絕與「後端不能用」](#拒絕與後端不能用)）。

### 沒有前綴的雜湊不是密碼

一個值沒有前綴，但開頭是 `$2a$`、`$2b$`、`$2y$`、`$1$`、`$5$`、`$6$` 或 `$argon2`，伺服器**不會**把它當明文比對，任何登入都拒絕。這種值通常是匯入時漏了 `{crypt}`。要是當明文比對，拿到這串雜湊的人直接把雜湊當密碼送來就能登入。

第一次遇到某個帳號時，日誌會寫一行警告（每個帳號只寫一次，不寫出存放的值）：

```text
… WARN rradius_core::auth_chain: the stored password looks like a hash with no scheme prefix and is refused; add {crypt} or {argon2} before it, or {clear} if it is the password itself user="gina"
```

解法：在值前面補上 `{crypt}` 或 `{argon2}`。如果這串字真的就是某人的明文密碼，寫成 `{clear}$2y$…`。

另一種情況：值沒有前綴，長得像摘要（32、40、56、64、96 或 128 個十六進位數字，或解得出 16、20、28、32、48、64 個位元組的 Base64）。這種值仍然**當明文比對**，但第一次會警告：

```text
… WARN rradius_core::auth_chain: the stored password looks like a digest with no scheme prefix and is compared as plaintext; add its prefix ({nthash}, {MD5}, {SHA}, {SHA256}, ...), or {clear} if it is the password itself user="frank"
```

`frank` 存的是十六進位的 MD5，使用者送來的是原本的密碼，當明文比對當然不符，所以被拒絕。補上正確的前綴（十六進位的 MD5 要先轉成 Base64，見下一節）就好。

這個規則對 PAP、CHAP、MS-CHAPv1 與 MS-CHAPv2 都一樣：CHAP 與 MS-CHAP 也用這串字當明文密碼去算比對的值。警告的名單每個行程最多記 1024 個用戶，超過之後不再寫警告，比對照舊。所以「日誌沒有這個警告」不代表資料庫裡沒有這種值；要找它們，看 `rradius_password_checks_total{scheme="none"}` 的 `ok` 是否多得不合理。

**已驗證**：兩種警告與拒絕，都在 quick-start 複本加了測試帳號後實測過。

### 看比對結果的指標

`rradius_password_checks_total` 依存放值的格式與結果計數每一次比對：

```console
$ curl -s http://127.0.0.1:9812/metrics | grep 'password_checks_total{scheme="crypt"'
rradius_password_checks_total{scheme="crypt",result="failed"} 6
rradius_password_checks_total{scheme="crypt",result="ok"} 4
```

`scheme` 是固定的一組值：`clear`、`none`（沒有前綴）、`crypt`、`md5`、`smd5`、`sha`、`ssha`、`sha224`、`ssha224`、`sha256`、`ssha256`、`sha384`、`ssha384`、`sha512`、`ssha512`、`pbkdf2-sha1`（`{PBKDF2}` 也算這裡）、`pbkdf2-sha256`、`pbkdf2-sha512`、`x-pbkdf2`、`ns-mta-md5`、`nthash`、`argon2`、`unknown`（不認得的前綴）。`result` 是 `ok` 或 `failed`。

搬移舊帳號時，用它看還有多少登入在用舊格式。指標的完整清單見[附錄 C](92-metrics-reference.md)。

## 搬入舊系統的密碼雜湊

目標：把舊目錄、舊 RADIUS 伺服器或 `/etc/shadow` 裡的密碼雜湊原樣放進資料表，使用者不必重設密碼；之後使用者改密碼時，換成新的格式。

範例情境 `migrate-users` 就是這樣做的。照第 3 章的方式複製一份到工作目錄（改 `dictionary` 的路徑、換掉 `.secret` 檔），它的重點在設定與 migration：

```toml
# rradius.toml（migrate-users，節錄）
[auth]
policy = "first_match"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ? AND enabled = 1"
reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"

[auth.username]
strip_realm = "both"
lowercase = true

[accounts]
scheme = "bcrypt"
add = "INSERT INTO users (username, password, origin) VALUES (?1, ?2, 'rradius')"
passwd = "UPDATE users SET password = ?2, origin = 'rradius' WHERE username = ?1"
enable = "UPDATE users SET enabled = 1 WHERE username = ?1"
disable = "UPDATE users SET enabled = 0 WHERE username = ?1"
delete = "DELETE FROM users WHERE username = ?1"
show = "SELECT username, enabled, origin FROM users WHERE username = ?1"
# …（其他設定略）
```

- 帳號表多了一欄 `origin`，記下這個帳號的密碼從哪裡來。`passwd` 在改密碼時順便把它改成 `rradius`，之後查 `origin` 不是 `rradius` 的列，就知道還有多少帳號留在舊格式。
- `[auth.username]` 統一大小寫並去掉網域，因為舊系統的名稱寫法常常不一致（見[統一使用者名稱與 realm](#統一使用者名稱與-realm)）。

`migrations/2_imported_users.sql` 放了每種來源的示範帳號，密碼都是「帳號名稱-password」：

| 帳號 | 來源 | 存放的值（開頭） |
| --- | --- | --- |
| `ldap1` | 舊 LDAP 目錄的 `userPassword` | `{SSHA}…` |
| `ldap2` | 同上 | `{SSHA512}…` |
| `ldap3` | 同上 | `{PBKDF2-SHA256}10000$…` |
| `ldap4` | 同上 | `{X-PBKDF2}HMACSHA2+256:…` |
| `shell1` | `/etc/shadow` 的第二欄，前面加 `{crypt}` | `{crypt}$1$…` |
| `mail1` | 舊郵件伺服器 | `{NS-MTA-MD5}…` |
| `old1` | 舊資料庫存的十六進位 MD5，轉成 Base64 | `{MD5}kfiBqWi4k+QJLsoSpSKT7w==` |
| `win1` | 舊系統存的 NT hash | `{nthash}…` |

### 驗證

這個情境只定義了用戶端 `nas`（`10.90.0.0/24`）。要從本機執行下面的登入，先在 `rradius.toml` 加一個 127.0.0.1 的用戶端：

```toml
[[radius.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "localhost.secret"
```

建立它的密鑰，再啟動伺服器：

```console
$ openssl rand -hex 32 > localhost.secret
$ chmod 600 localhost.secret
```

啟動伺服器後，逐一用 PAP 登入：

```console
$ rradius-bench run … --user ldap3 --password ldap3-password --load pap,concurrency=1 --requests 1
…
  pap: …, sent 1, accept 1, reject 0, …
```

**已驗證**：八個帳號用 PAP 全部 `accept 1`；`win1` 用 MS-CHAPv2 也 `accept 1`，`ldap1`（`{SSHA}`）用 MS-CHAPv2 是 `reject 1`；`CORP\Shell1` 與 `win1@corp` 經過名稱正規化後都能登入。

改一次密碼，看 `origin` 跟著變：

```console
$ rradiusd user show --config rradius.toml ldap1
username=ldap1
enabled=1
origin=ldap
$ printf 'new-pass\n' | rradiusd user passwd --config rradius.toml ldap1
password changed for ldap1
$ rradiusd user show --config rradius.toml ldap1
username=ldap1
enabled=1
origin=rradius
```

### 匯出舊資料時的轉換

大多數格式只要在原本的值前面加上前綴。需要轉換的：

- **十六進位的摘要**：`{MD5}`、`{SHA}` 這些前綴後面要的是 Base64，不是十六進位。用 `xxd` 與 `base64` 轉：

  ```console
  $ printf '%s' 91f881a968b893e4092eca12a52293ef | xxd -r -p | base64
  kfiBqWi4k+QJLsoSpSKT7w==
  ```

  存成 `{MD5}kfiBqWi4k+QJLsoSpSKT7w==`。這正是 `old1` 的值。
- **`/etc/shadow`**：第二欄（`$1$…`、`$5$…`、`$6$…`、`$2y$…`）前面加 `{crypt}`。少了前綴會被當成[沒有前綴的雜湊](#沒有前綴的雜湊不是密碼)而拒絕。
- **只存 NT hash 的系統**：前面加 `{nthash}`。這些帳號 PAP 和 MS-CHAPv2 都能用。

> **注意：** 舊格式只能驗證 PAP（`{nthash}` 與明文除外）。舊系統的 NAS 如果用 CHAP 或 MS-CHAPv2 登入這些帳號，搬過來之後會被拒絕，要讓使用者改一次密碼，並把 `scheme` 設成合適的格式。

### 能搬與不能搬的

**雜湊只能用在會把明文密碼送到伺服器的登入方式。** 搬之前先看你的設備用哪一種：

| 登入方式 | 能用雜湊嗎 |
| --- | --- |
| PAP（VPN、多數網頁登入、TACACS+ 的 PAP 與 ASCII） | 全部格式都可以 |
| EAP-TTLS 內層 PAP（Wi-Fi，見[第 11 章](11-eap.md)） | 全部格式都可以 |
| CHAP（PPPoE 常用） | 只能明文（`{clear}` 或沒有前綴） |
| MS-CHAPv2、PEAP（Wi-Fi 最常見的帳號密碼方式） | 只能明文或 `{nthash}` |
| MS-CHAPv1（PPTP 與舊撥接設備；預設拒絕，要 `[auth] allow_mschapv1 = true` 才開） | 只能明文或 `{nthash}` |

（這些對應來自上面「選擇密碼格式」與[認證協定與 MS-CHAPv1 開關](#認證協定與-ms-chapv1-開關)的規則；TACACS+ 見[第 12 章](12-tacacs.md#認證方式asciipapchapms-chap)。）所以：

- **舊系統有存 NT hash：** 搬過來的帳號可以直接用 PEAP（範例的 `win1`）。
- **舊系統只有其他雜湊，而 Wi-Fi 要用帳號密碼：** 兩條路。Wi-Fi 改用 EAP-TTLS 內層 PAP（各作業系統的支援程度不同，要先確認，本章未驗證）；或要求使用者改一次密碼，改的時候 `scheme` 設成 `nthash`。
- **PPPoE 的 CHAP：** 沒有明文就沒辦法，只能重設密碼。
- **加鹽的雜湊，鹽要在後面。** `{SSHA}` 這一類是「摘要接著鹽」一起做 Base64，摘要算的是密碼接鹽。舊系統是「鹽接密碼」，或用別的方式混合時，沒有對應的格式，這些帳號只能重設。
- **不確定對不對時，先拿一個知道密碼的測試帳號試。** 格式不對的值比對不過，不會放行任何人。

**已驗證**（`migrate-users` 的情境測試，見範例目錄）：八個示範帳號用 PAP 登入都接受，`{SSHA}` 的帳號用 CHAP 被拒絕。

### 匯入 SQL

把舊系統匯出的資料轉成一個新編號的 migration 檔（範例是 `migrations/2_imported_users.sql`）：

```sql
-- 2_imported_users.sql
INSERT INTO users (username, password, origin) VALUES
  ('ldap1', '{SSHA}ZvqZAfdyjd1PQiq8rurmdodN8ihkZW1vc2FsdA==', 'ldap'),
  ('shell1', '{crypt}$1$demosalt$EhA8qeu4qSun8gh9DajmS1', 'shadow'),
  ('o''brien', '{nthash}9d232eea31049319fc8ab84300855a34', 'nt');
```

- **第一次啟動前就把檔案準備好。** 套用過的 migration 檔不能再改（改了會 `migration file '…' changed after it was applied`，見[第 18 章](18-operations.md)）；之後要補帳號就新增下一個編號的檔案。
- **帳號名稱先轉成小寫、去掉網域，** 和[統一使用者名稱與 realm](#統一使用者名稱與-realm) 的設定一致。資料表裡留著大小寫不同的名稱，登入時查不到：`lowercase = true` 時，`Alice` 這一列永遠不會被比對到。
- **只差大小寫的兩個帳號，** 轉成小寫後是同一個名稱。範例的 `users.username` 是 `PRIMARY KEY`，轉成小寫後重複的那列，`INSERT` 會失敗（`UNIQUE constraint failed`），整個 migration 檔跟著失敗。匯入前先找出來，決定留哪一個。
- **SQL 字串裡的單引號要寫兩個**（上面 `o''brien`）。用程式產生檔案時，帳號和雜湊裡的引號都要處理。
- **`origin` 欄不是必要的。** 範例用它記下每個帳號的密碼來自哪裡，之後好統計還剩多少舊格式。
- **檔案很大時**（幾十萬列），匯入要一些時間：把 `[cluster] migrations_wait_ms`（預設 `60000`）調高到比匯入時間長。啟動時等不到會失敗，訊息是 `cluster was not ready to apply migrations: … (the limit is migrations_wait_ms; raise it if a migration file takes longer)`。注意叢集節點對一個 migration 檔的寫入只等 30 秒：超過時訊息會改成 `the cluster node gives one migration file 30 s to be answered, whatever migrations_wait_ms is`，檔案仍在叢集裡繼續跑，等它做完後用更長的 `migrations_wait_ms` 再啟動一次。匯入幾十萬列的實際耗時本章沒有量。
- **匯出的檔案是所有人的密碼雜湊。** 弱的雜湊可以被離線破解。傳輸和存放都要保護，匯入後刪掉暫存的副本；已經套用的 migration 檔也一樣，它留在 `migrations_dir` 裡。
- **`user add` 不能用來搬現成的雜湊。** 它只收明文密碼（從標準輸入），自己依 `scheme` 雜湊。

### 沒有前綴的明文：MySQL 與 `CONCAT`

舊系統把明文存在資料欄裡時，沒有前綴的值直接當明文用。例外是明文本身以 `{` 開頭（會被誤認成格式前綴），或以 `$2a$`、`$1$`、`$argon2` 這類開頭（會被當成漏了前綴的雜湊，見[沒有前綴的雜湊不是密碼](#沒有前綴的雜湊不是密碼)）。這時在 SQL 裡補上 `{clear}`：

```sql
-- SQLite、PostgreSQL：用 || 串接
SELECT '{clear}' || value FROM radcheck WHERE username = ?
-- MySQL：用 CONCAT
SELECT CONCAT('{clear}', value) FROM radcheck WHERE username = ?
```

（MySQL 預設把 `||` 當邏輯 OR，例如在 `mysql` 用戶端裡。rradius 開的連線會打開 `PIPES_AS_CONCAT`，所以經由 rradius 時 `||` 也是串接；`CONCAT` 兩邊都適用。）

### 上線前

- **先平行跑一陣子。** 挑幾台 NAS 指到 rradius，其他的留在舊系統，確認登入結果一致再全部切換。
- **舊系統給的回覆屬性**（VLAN、速率、權限）也要搬，放進 `user_reply`（範例的資料表，`reply_query` 讀它）。屬性名稱要是 rradius 字典裡的名稱（[第 6 章](06-dictionary.md)）；搬完用真的登入試幾個帳號，不要只靠 `check`。
- **舊系統的共用密碼** 可以沿用，NAS 那邊只要改伺服器位址；趁這次換成每台不同、夠長的更好。
- **慢雜湊會排隊。** `{crypt}`、`{argon2}` 與 PBKDF2 故意算得慢，同時登入很多時會排隊，尖峰時變慢。成本過高的值會被 `[auth.password_limits]` 擋下（見[限制雜湊的計算成本](#限制雜湊的計算成本)）；遷移來的雜湊成本本來就高時，先調高上限。
- **訂一個期限。** 期限到了還沒改密碼的帳號，用新的 migration 檔把密碼設成 `NULL`（例如 `UPDATE users SET password = NULL WHERE origin <> 'rradius'`）。密碼欄是 `NULL` 的帳號一律登不進來（日誌 `no stored password; rejecting`），請他們找管理者重設。這句 `UPDATE` 是依範例資料表寫的，未實際執行。
- **沒有測過的：** 從任何一套特定的舊系統實際匯出再匯入的流程。範例的雜湊是依各格式的定義算出來的，再由情境測試確認 rradius 驗證得過。

帳號想留在原本的 PostgreSQL 或 MySQL、由舊系統繼續維護時，sql 處理器可以用 `database` 直接查外部資料庫，情境裡的 `external-db.toml` 是這種寫法，見[第 9 章](09-external-databases.md)。

> **警告：** `2_imported_users.sql` 的帳號與密碼是公開的示範值。拿這個情境改成正式環境時，刪掉這個檔案，換成你自己匯出的資料。

## 限制雜湊的計算成本

慢雜湊的值本身就寫了計算成本，例如 PBKDF2 的迭代次數、bcrypt 的 cost、argon2 的記憶體用量。能寫入帳號表的人可以放一個成本極高的值，每次登入都佔住一個比對的位置很久，或吃掉大量記憶體。

`[auth.password_limits]` 設定上限。比對之前先讀出值裡的成本，超過上限就不計算，直接當作密碼錯誤：

| 鍵 | 預設 | 限制的格式 |
| --- | --- | --- |
| `auth.password_limits.pbkdf2_iterations` | `5000000` | `{PBKDF2*}`、`{X-PBKDF2}` 的迭代次數 |
| `auth.password_limits.crypt_rounds` | `1000000` | `{crypt}` 的 `$5$`、`$6$`、`$sha1$` 與延伸 DES 的 rounds |
| `auth.password_limits.bcrypt_cost` | `15` | `{crypt}` 的 bcrypt cost |
| `auth.password_limits.argon2_memory_kib` | `262144`（256 MiB） | `{argon2}` 的記憶體，KiB |
| `auth.password_limits.argon2_iterations` | `10` | `{argon2}` 的迭代次數 |
| `auth.password_limits.argon2_lanes` | `16` | `{argon2}` 的平行度 |

- 沒寫這一節時，上限照樣是這些預設值。每個值都要大於 0。
- 可以 reload；reload 失敗時維持原本的上限。
- 成本固定的格式（DES、`$1$`、各種 SHA 與 MD5 摘要、`{nthash}`）不受限制。
- 超過上限算一次失敗的登入：`rradius_password_checks_total` 記為 `failed`，[登入失敗鎖定](#登入失敗鎖定)也會計數。它不是「後端不能用」。

### 目標：把 PBKDF2 的上限降到十萬次

```toml
# rradius.toml（加在 [[auth.handlers]] 之後）
[auth.password_limits]
pbkdf2_iterations = 100000
```

存著 20 萬次迭代 `{PBKDF2-SHA256}` 的帳號 `erin`，在上限調低之前能登入；reload 之後，用正確的密碼也被拒絕，日誌寫一次警告（每個帳號只寫一次，不寫出存放的值）：

```text
… WARN rradius_core::auth_chain: the stored password asks for more than [auth.password_limits].pbkdf2_iterations allows and is refused; raise the limit if the hash is meant to be used user="erin" format="pbkdf2-sha256" parameter="pbkdf2_iterations"
```

**已驗證**。要讓這種帳號繼續能登入，調高上限，或讓使用者改密碼換成 `scheme` 的格式。

> **注意：** `argon2_memory_kib` 是一次比對的上限。同時進行的比對數等於 CPU 數，最壞情況的記憶體用量大約是這個值乘以 CPU 數。

## 認證協定與 MS-CHAPv1 開關

驗證鏈看請求帶了什麼來決定是哪一種登入：

| 協定 | 請求裡的屬性 | 能驗證的存放值 |
| --- | --- | --- |
| PAP | `User-Password` | 上一節的所有格式 |
| CHAP | `CHAP-Password`（`CHAP-Challenge` 或 Request Authenticator） | 明文、`{clear}` |
| MS-CHAPv2 | 微軟廠商屬性 `MS-CHAP-Challenge`（16 位元組）與 `MS-CHAP2-Response` | 明文、`{clear}`、`{nthash}` |
| MS-CHAPv1 | `MS-CHAP-Challenge`（8 位元組）與 `MS-CHAP-Response` | 明文、`{clear}`、`{nthash}` |

TACACS+ 的登入也走同一條鏈，`[tacacs]` 的設定見[第 12 章](12-tacacs.md)。

MS-CHAPv2 通過時，Access-Accept 帶 `MS-CHAP2-Success`、`MS-MPPE-Send-Key`、`MS-MPPE-Recv-Key`；失敗時 Access-Reject 帶 `MS-CHAP-Error`（錯誤 691）。使用者名稱寫成 `網域\帳號` 時，計算回應用的是反斜線之後的部分。MS-CHAPv1 只檢查 NT-Response，不接受只有 LM-Response 的回應。

MS-CHAPv1 是很舊的協定，有 `[auth]` 驗證鏈時**預設不接受**（沒有 `[auth]`、由 Lua 腳本自己處理的設定不在這個開關的範圍）。只有 NAS 非用它不可（例如較舊的 PPTP 閘道）時，才明確打開：

```toml
# rradius.toml（[auth] 裡）
[auth]
policy = "first_match"
allow_mschapv1 = true
```

`auth.allow_mschapv1` 預設 `false`，可以 reload。沒寫這個鍵、或寫 `false` 時，MS-CHAPv1 的登入在任何處理器之前就被拒絕，認證紀錄的 `handler` 是 `mschapv1_refused`；不查資料庫，也不算一次失敗的登入。PAP、CHAP、MS-CHAPv2、EAP 不受影響。

> **升級注意：** 這個預設以前是 `true`。只用 MS-CHAPv1 的舊設備升級後登入會全部失敗；認證紀錄的 `handler="mschapv1_refused"` 可以找出是哪些 NAS，確認非用不可再寫 `allow_mschapv1 = true`。

> **未實機驗證：** `rradius-bench` 沒有 MS-CHAPv1 的負載，這一段只對照了程式碼與測試；設定本身過了 `check`。

## sql 處理器

sql 處理器是最常用的處理器：用一句 SQL 查帳號的密碼，比對通過後再用另一句查回覆屬性。

```toml
# rradius.toml
[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ? AND enabled = 1"
reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"
```

| 鍵 | 預設 | 說明 |
| --- | --- | --- |
| `user_query` | —（必填） | 剛好一個 `?`，代入帳號名稱。第一欄是存放的密碼 |
| `reply_query` | — | 剛好一個 `?`。每一列是 `(屬性名稱, 值)`，依序加進 Access-Accept |
| `certificate_query` | — | EAP-TLS 憑證通過之後改用的查詢，查到就接受（[第 11 章](11-eap.md)） |
| `default_user` | — | 查不到帳號時，改用這個帳號的密碼驗證 |
| `database` | —（叢集資料庫） | 改查某個 `[databases.<name>]`（[第 9 章](09-external-databases.md)） |
| `timeout_ms` | `5000` | 一次查詢的時限，毫秒；也涵蓋慢雜湊排隊與比對的時間 |

整個 `[auth]` 都可以 reload（`database` 指向的連線本身例外：`[databases]` 改了要重啟）。

查詢的結果怎麼解讀：

| `user_query` 的結果 | 處理器的答案 |
| --- | --- |
| 沒有列 | ignore（有 `default_user` 時改查它） |
| 一列，密碼比對通過 | accept，接著跑 `reply_query` |
| 一列，密碼不符 | reject |
| 一列，密碼欄是 NULL 或空的 | reject（日誌：`no stored password; rejecting`） |
| 兩列以上 | reject（日誌：`user_query returned 2 rows; rejecting`） |
| 查詢失敗或逾時 | 後端不能用，不回應 |

帳號名稱一律以參數綁定，不會拼進 SQL 字串。

`reply_query` 的屬性名稱要在字典裡（[第 6 章](06-dictionary.md)）。quick-start 的 `alice` 有兩列：`Session-Timeout = 3600`、`Filter-Id = staff`，登入成功的 Access-Accept 就帶這兩個屬性，依查詢傳回的順序。

### default_user：查不到時的共用帳號

`default_user` 讓查不到的名稱改用一個共用帳號的密碼驗證，例如訪客網路的一組共用密碼：

```toml
# rradius.toml（節錄）
[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ? AND enabled = 1"
reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"
default_user = "DEFAULT"
```

資料表要有一個 `DEFAULT` 帳號。測試時存的是 `{clear}guest-password`，處理器放在下一節的 `lab` 鏈裡：

- 名稱 `visitor`、密碼 `guest-password`：`accept 1`。認證紀錄的 `user` 是 `visitor`、`account` 是 `DEFAULT`，`reply_query` 查的也是 `DEFAULT` 的回覆屬性。
- 名稱 `visitor`、密碼錯誤：`reject 1`。注意結果是 reject，不是 ignore：有了 `default_user`，任何查不到的名稱都變成比對 `DEFAULT` 的密碼，後面的處理器不會再有機會。

**已驗證**。

> **警告：** 知道 `DEFAULT` 密碼的人，用任何名稱都能登入。EAP 裡不套用 `default_user`（EAP 的名稱會被記錄並回給設備），EAP-TLS 的憑證登入也不套用。

## lua 處理器

lua 處理器跑一個獨立的 Lua 腳本，用來寫資料表與 SQL 做不到的規則。腳本的寫法與可用的函式見[第 10 章](10-lua-policies.md)與[附錄 D](93-lua-reference.md)。

```toml
# rradius.toml（節錄）
[[auth.handlers]]
type = "lua"
script = "policy.lua"
```

| 鍵 | 預設 | 說明 |
| --- | --- | --- |
| `script` | —（必填） | 腳本檔，相對於設定檔所在的目錄 |
| `verifies_password` | `false` | 腳本自己比對密碼時設成 `true` |

腳本的傳回值對應到處理器的答案：`"accept"`、`"ignore"`、`"challenge"`（只在 RADIUS），其他任何值或腳本出錯都是 reject。腳本裡的全域變數 `auth_user` 是正規化之後的帳號名稱，`get_attr('User-Name')` 仍然是收到的原樣。

`verifies_password` 很重要：

- 預設 `false`，表示這個腳本只檢查政策（例如時段、來源設備），它的 accept 不代表密碼正確。
- 設成 `true`，表示腳本自己驗證了密碼（例如用 `password_verify`）。在 `all_accept` 裡，它的 accept 才算「有人比對過密碼」；在 EAP 裡，它的 accept 也才算證明了密碼。

`check` 會讀取並編譯每個 lua 處理器的腳本，讀不到或編譯失敗都會報錯。

## all_accept：密碼之外再加一道政策

目標：密碼由 sql 處理器比對，再用一個腳本擋掉某台 NAS。兩個條件都通過才放行。

```lua
-- policy.lua
if get_attr('NAS-Identifier') == 'lab-blocked' then
  return 'reject'
end
set_attr('Reply-Message', 'policy ok')
return 'accept'
```

```toml
# rradius.toml（節錄）
[auth.chains.lab]
policy = "all_accept"

[[auth.chains.lab.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ? AND enabled = 1"
reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"

[[auth.chains.lab.handlers]]
type = "lua"
script = "policy.lua"
```

這是一條具名的鏈 `lab`，下一節說明怎麼讓用戶端用它。測試時把 `localhost` 用戶端設成 `auth = "lab"`：

```console
$ rradius-bench run … --user bob --password bob-password --load pap,concurrency=1 --requests 1
…  accept 1, reject 0 …
$ rradius-bench run … --user bob --password bob-password --nas-identifier lab-blocked --load pap,concurrency=1 --requests 1
…  accept 0, reject 1 …
```

伺服器的認證紀錄：

```text
… user="bob" account="bob" chain="lab" handler="lua" decision="accept"
… user="bob" account="bob" chain="lab" handler="lua" decision="reject"
```

兩個處理器都接受時，Access-Accept 帶兩者的回覆屬性，依執行的順序（先 `reply_query` 的，再 `set_attr` 的）。

**已驗證**。

### 沒有人比對密碼時

`all_accept` 有一條保護規則：鏈裡只要有比對密碼的處理器（sql、ldap、netlogon，或 `verifies_password = true` 的 lua），就一定要其中一個 accept，結果才會是 accept。不然像上面這條鏈，sql 查不到帳號（ignore），政策腳本又接受任何人，結果就變成「誰都能進來」。

所以同一條鏈，用一個不存在的名稱登入：

```text
… WARN rradius_core::lua_engine: auth chain: rejecting a request from client localhost: all_accept without checking a password
… user="visitor" account="visitor" chain="lab" handler="no_password_check" decision="reject"
```

結果是拒絕，而且政策腳本根本沒有跑：後面已經沒有會比對密碼的處理器，腳本寫的任何資料都會留給一筆注定被拒絕的請求。

其他細節：

- 比對密碼的處理器接受之前，政策腳本回 `challenge` 也會被拒絕，不送 Access-Challenge。
- 鏈裡只有不比對密碼的 lua 處理器時，照它們的答案決定。
- 這種拒絕不算一次失敗的登入。

**已驗證**（不存在的名稱被拒絕、`handler="no_password_check"`）。

## 讓不同的 NAS 走不同的鏈

`[[auth.handlers]]` 是預設的鏈，認證紀錄裡叫 `default`。另外可以定義任意條具名的鏈，用戶端用 `auth` 指定：

```toml
# rradius.toml（節錄）
[auth.chains.lab]
policy = "all_accept"
# …（處理器同上一節）

[[radius.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "localhost.secret"
auth = "lab"
```

規則：

- 用戶端沒有 `auth` 就用預設的鏈。沒有預設的鏈（`[auth]` 只有具名的鏈）時，這個用戶端由 `[lua]` 的最上層腳本決定。
- `auth` 指向不存在的鏈，`check` 失敗：`radius client 'localhost': unknown auth chain 'vpn' (no [auth.chains.vpn])`。
- TACACS+ 的用戶端也用 `auth` 選鏈（[第 12 章](12-tacacs.md)）。RADIUS 與 TACACS+ 的用戶端指向同一條鏈時，共用同一組處理器，包括 LDAP 的連線池。
- 每條鏈在啟動與 reload 時建好一次。
- 具名的鏈要寫自己的 `policy` 與 `username`，不會沿用 `[auth]` 的。在 `[auth]` 寫了 `policy` 或 `[auth.username]`、卻沒有 `[[auth.handlers]]`，`check` 會失敗。

範例情境 `remote-access` 就是這樣：VPN 閘道用 `vpn` 鏈（LDAP），跳板機用 `mfa` 鏈（Lua 第二因素驗證）。LDAP 的部分見[第 8 章](08-ldap-and-ad.md)，兩步驟驗證見[第 15 章](15-access-recipes.md)。

改鏈只要 reload。在跑伺服器的終端機之外執行：

```console
$ kill -HUP <伺服器的 PID>
```

伺服器日誌會寫 `configuration reloaded from rradius.toml`，之後的請求就走新的鏈。reload 的細節見[第 4 章](04-configuration.md)。

## 統一使用者名稱與 realm

使用者送來的名稱常常不一致：`alice`、`Alice`、`CORP\alice`、`alice@corp.example`。`[auth.username]` 在交給處理器之前，把收到的名稱整理成帳號名稱：

```toml
# rradius.toml（加在 [[auth.handlers]] 之後）
[auth.username]
strip_realm = "both"
lowercase = true
allowed = "[a-z0-9._-]+"
```

| 鍵 | 預設 | 說明 |
| --- | --- | --- |
| `strip_realm` | `none` | 要去掉的 realm：`none`、`suffix`、`prefix`、`both` |
| `lowercase` | `false` | 轉成小寫 |
| `allowed` | — | 整理後的名稱要**整個**符合的正規表示式 |

順序固定：先去掉 realm，再轉小寫，最後檢查 `allowed`。

| `strip_realm` | `lowercase` | 收到 | 帳號名稱 |
| --- | --- | --- | --- |
| `none` | `false` | `Alice@Corp` | `Alice@Corp` |
| `suffix` | `false` | `Alice@Corp` | `Alice`（從最後一個 `@` 起去掉） |
| `suffix` | `false` | `a@b@c` | `a@b` |
| `suffix` | `false` | `CORP\Alice` | `CORP\Alice` |
| `prefix` | `false` | `CORP\Alice` | `Alice`（到最後一個反斜線為止去掉） |
| `prefix` | `false` | `A\B\c` | `c` |
| `both` | `true` | `CORP\Alice@Corp` | `alice`（先 prefix 再 suffix） |

整理之後是空字串，或不符合 `allowed`，鏈直接拒絕，不執行任何處理器，認證紀錄的 `handler` 是 `normalization`。那一行 WARN 只寫原因，不寫收到的名稱，因為名稱來自外部，可能含有控制字元；接著的認證紀錄在 `user` 欄位記了它：

```text
… WARN rradius_core::lua_engine: auth chain: rejecting a request from client localhost: the user name does not match the allowed pattern
… user="alice smith" account="" chain="default" handler="normalization" decision="reject"
… WARN rradius_core::lua_engine: auth chain: rejecting a request from client localhost: the user name is empty after normalization
… user="@corp" account="" chain="default" handler="normalization" decision="reject"
```

用上面的設定實測：`CORP\Alice@corp.example` 與 `ALICE` 都以 `alice` 登入成功；`alice smith`（有空白）與 `@corp`（整理後是空的）被拒絕。**已驗證**。

### 對其他地方的影響

- **處理器查的是帳號名稱**：sql 的 `user_query`、`reply_query`，ldap 的搜尋條件都用整理後的名稱。資料表裡一律存小寫。
- **比對密碼用的是收到的名稱**：MS-CHAPv2 的回應是對使用者送出的名稱算的，所以驗證時用原樣。
- **`user` 指令也套用**：預設鏈有 `[auth.username]` 時，`user add 'CORP\Henry@Corp'` 存進去的是 `henry`（輸出 `added henry`）；不符合 `allowed` 的名稱，例如 `user add 'bad name'`，印出 `'bad name': the user name does not match the allowed pattern`，結束碼 2，什麼都不寫。具名的鏈的 `username` 不影響 `user` 指令。
- **lua 處理器**：`auth_user` 是帳號名稱，`get_attr('User-Name')` 是收到的名稱。
- **EAP-TLS 憑證**：憑證證明的是整個名稱。整理時除了 ASCII 大小寫之外還改了別的（例如去掉 realm），這筆登入就被拒絕（[第 11 章](11-eap.md)）。

> **注意：** `allowed` 會自動加上頭尾錨點，寫 `[a-z0-9._-]+` 就是「整個名稱只能有這些字元」，不必自己寫 `^…$`。

## 密碼長度

RADIUS 的 User-Password 依 RFC 2865 第 5.2 節固定是 16 到 128 個位元組，而且是 16 的倍數（NAS 把密碼補零到整塊）。收到長度不合的請求，rradius 在解密之前就回 Access-Reject，不執行處理器與腳本，也不算進[登入失敗鎖定](#登入失敗鎖定)，因為這不是猜密碼；認證紀錄的 `handler` 是 `length`。

每次拒絕記一行 WARN，只有用戶端名稱與長度，不含密碼，而且有限速，被灌請求時不會灌爆日誌：

```text
… WARN rradius_core::lua_engine: RADIUS client nas1: Access-Request refused, its User-Password is 240 octets, not 16 to 128 in whole blocks of 16
```

> **升級注意：** 以前超過 128 個位元組的 User-Password 會被解開、照常驗證；現在被拒絕。不補齊到 16 倍數的 NAS 也會被拒絕。升級前用認證紀錄確認沒有 `handler="length"` 的行；有的話是 NAS 的設定問題，不是密碼錯。

## 登入失敗鎖定

`[auth.lockout]` 限制猜密碼：同一個帳號從同一個來源，在一段時間內失敗太多次，就先鎖起來，鎖定期間直接拒絕，連密碼都不比對。

```toml
# rradius.toml（加在 [[auth.handlers]] 之後）
[auth.lockout]
max_failures = 3
lock_seconds = 60
```

| 鍵 | 預設 | 說明 |
| --- | --- | --- |
| `max_failures` | `10` | 視窗內失敗幾次就鎖定 |
| `window_seconds` | `300` | 從第一次失敗起算的視窗，秒 |
| `lock_seconds` | `300` | 鎖多久，秒 |
| `max_entries` | `100000` | 每個節點最多記住幾個鍵 |
| `account_max_failures` | —（不設帳號總上限） | 同一個帳號不分來源的失敗總數上限，要大於 `max_failures` |
| `max_in_flight` | `max_failures` 的 5 倍 | 同一個鍵同時最多幾個登入在等結果；已計的失敗也佔位置（見下文） |

每個值都要大於 0；可以 reload。沒有這一節就完全不計數。

### 計數的單位

鎖定的「鍵」是**帳號名稱加上來源**：

- 帳號名稱是鏈整理後要查的名稱，再轉成 ASCII 小寫。所以 `Alice` 與 `alice` 是同一個鍵，換個大小寫繞不過去。
- 來源是 RADIUS 請求的 `Calling-Station-Id`（通常是使用者設備的 MAC 位址），或 TACACS+ 的 `rem_addr`。沒有就是空的。

所以 `alice` 在自己的筆電被鎖住，用手機（另一個 `Calling-Station-Id`）登入不受影響。設了 `account_max_failures` 之後，同一個帳號從所有來源的失敗再另外合計，達到上限就對所有來源鎖住。

> **注意：** `account_max_failures` 讓攻擊者故意打錯密碼，就能把別人的帳號對所有來源鎖住。只在你確定需要時才設。

什麼算一次失敗：鏈拒絕的登入，而且不是因為後端不能用。下面這些**不算**：

- 後端不能用（資料庫、LDAP、網域控制站故障，或慢雜湊排隊逾時）
- 名稱不符合 `[auth.username]`（`normalization`）、`all_accept` 沒有人比對密碼（`no_password_check`）、`mschapv1_refused`
- Access-Challenge、EAP-TLS 的憑證登入

登入成功會清掉這個鍵的失敗次數。不存在的帳號也照樣計數與鎖定，所以從回應看不出某個帳號存不存在。

`max_in_flight` 處理另一種猜法：一次送出一大批，在任何一筆失敗之前全部進來。每一筆登入在處理器執行之前先佔一個位置，結果出來才歸還；同一個鍵佔滿 `max_in_flight` 個位置時，再來的直接拒絕。

窗口內已計的失敗也佔位置：等待中的登入加上已計的失敗，要少於 `max_in_flight`（`max_failures` 比它大時用 `max_failures`）。所以一個窗口內，不論錯誤密碼是一次湧入還是陸續到達，鎖定前最多檢查這麼多次（預設值是 50）。已有 `k` 次失敗紀錄的鍵，最多同時有 50 − `k` 個登入在等結果；登入成功會清掉計數。沒有鎖定、也沒有登入在等結果的鍵永遠有一個位置，所以 reload 把上限調到低於已計的失敗數時，下一次失敗就鎖住它。

### 目標：鎖住連續打錯的設備

用上面 `max_failures = 3` 的設定，`--attr 31=text:…` 讓 `rradius-bench` 帶上 `Calling-Station-Id`（屬性 31）：

```console
$ rradius-bench run … --user alice --password wrong --attr 31=text:aa-bb-cc-dd-ee-01 --load pap,concurrency=1 --requests 1
…  accept 0, reject 1 …
```

打錯三次就鎖定。之後同一台設備用**正確**的密碼也被拒絕；換一個 `Calling-Station-Id` 就能登入：

```console
$ rradius-bench run … --user alice --password alice-password --attr 31=text:aa-bb-cc-dd-ee-01 --load pap,concurrency=1 --requests 1
…  accept 0, reject 1 …
$ rradius-bench run … --user alice --password alice-password --attr 31=text:aa-bb-cc-dd-ee-02 --load pap,concurrency=1 --requests 1
…  accept 1, reject 0 …
```

伺服器日誌，第三次失敗時開始鎖定：

```text
… INFO rradius_core::auth_chain: auth lockout: client localhost: account "alice" from "aa-bb-cc-dd-ee-01" is locked (this origin)
… user="alice" account="alice" chain="default" handler="lockout" decision="reject"
```

指標：

```console
$ curl -s http://127.0.0.1:9812/metrics | grep '^rradius_auth_lockout'
rradius_auth_lockout_rejected_total 1
rradius_auth_lockouts_total 1
rradius_auth_lockout_untracked_total 0
```

| 指標 | 意思 |
| --- | --- |
| `rradius_auth_lockouts_total` | 開始了幾次鎖定 |
| `rradius_auth_lockout_rejected_total` | 因為鎖定或沒有空位置而拒絕的登入數 |
| `rradius_auth_lockout_untracked_total` | 表格塞滿了鎖定中的鍵，沒有計數的失敗數 |

這三個指標只在設定了 `[auth.lockout]` 時才出現。**已驗證**。

### 計數放在哪裡

- 計數放在**每個節點自己的記憶體**。叢集有三個節點、NAS 輪流問它們時，攻擊者實際能試的次數最多是三個節點各自的上限。
- reload 保留計數；重新啟動後歸零。
- 表格滿了（`max_entries`）先丟過期的，再丟最舊的、沒有鎖定的；鎖定中的鍵不丟。全部都是鎖定中時，新的鍵不追蹤，計入 `rradius_auth_lockout_untracked_total`，日誌最多每分鐘寫一次。

> **注意：** Lua 腳本自己比對密碼（例如 `two-step-totp.lua` 用 `password_verify`）時，`[auth.lockout]` 管不到腳本裡的那次比對。要限制猜密碼，讓密碼在驗證鏈裡比對。

## 常見錯誤

**症狀：** 使用者確定密碼正確，認證紀錄是 `handler="sql" decision="reject"`，日誌有 `looks like a hash with no scheme prefix and is refused`。
**原因：** 存放的雜湊沒有 `{crypt}` 或 `{argon2}` 前綴。
**解法：** 補上前綴；見[沒有前綴的雜湊不是密碼](#沒有前綴的雜湊不是密碼)。

**症狀：** PAP 可以登入，CHAP 或 MS-CHAPv2（含 PEAP）被拒絕，MS-CHAPv2 的 Access-Reject 帶 `MS-CHAP-Error`。
**原因：** 存的是單向雜湊（bcrypt、SHA-512 crypt、`{SSHA}` 等）。CHAP 只能比對明文，MS-CHAP 只能比對明文或 `{nthash}`。
**解法：** 把 `accounts.scheme` 改成 `nthash`（MS-CHAP）或 `clear`（CHAP），讓使用者用 `user passwd` 重設一次密碼。

**症狀：** 搬過來的帳號密碼正確卻被拒絕。
**原因：** 前綴不對（例如十六進位的 MD5 直接加了 `{MD5}`，沒有轉 Base64）；鹽的位置和格式定義不同；或匯入時雜湊被截斷、多了空白。
**解法：** 拿一個知道密碼的帳號，對照[伺服器看得懂的全部格式](#伺服器看得懂的全部格式)逐項檢查。

**症狀：** 只有名稱有大寫的帳號登不進來。
**原因：** 開了 `lowercase`，但資料表裡的名稱沒有轉小寫。
**解法：** 資料表統一成小寫（見[匯入 SQL](#匯入-sql)）。

**症狀：** PAP 可以，Wi-Fi 的 PEAP 或 PPPoE 的 CHAP 不行。
**原因：** PEAP 要明文或 `{nthash}`，CHAP 要明文，其他雜湊做不到。
**解法：** 見[能搬與不能搬的](#能搬與不能搬的)。

**症狀：** `user disable` 印出 `disabled erin`，`erin` 卻還能登入。
**原因：** `user_query` 沒有略過停用的帳號。
**解法：** 在 `user_query` 加上 `AND enabled = 1`（依你的資料表）。

**症狀：** `user add` 印出 `bad.toml: accounts.scheme: 'md5' is not one of bcrypt, sha512-crypt, nthash, clear`，結束碼 2。
**原因：** `scheme` 只能是這四個值。`{MD5}` 等格式伺服器看得懂，但 `user` 指令不會寫。
**解法：** 改成四個值之一。

**症狀：** `check` 印出 `radius client 'localhost': unknown auth chain 'vpn' (no [auth.chains.vpn])`。
**原因：** 用戶端的 `auth` 指向一條沒有定義的鏈。
**解法：** 新增 `[auth.chains.vpn]`，或改正 `auth` 的名稱。

**症狀：** `check` 印出 `auth.username is set but the default chain has no [[auth.handlers]] entries`，或 `auth.policy is set but …`。
**原因：** `[auth]` 只有具名的鏈，卻在 `[auth]` 寫了 `[auth.username]` 或 `policy`。
**解法：** 寫在具名的鏈裡：`[auth.chains.<name>.username]`、`[auth.chains.<name>] policy = …`。

**症狀：** `check` 印出 `[auth] needs [[auth.handlers]] entries or at least one [auth.chains.<name>] (auth.handlers is empty)`。
**原因：** 有 `[auth]`，但沒有任何處理器。
**解法：** 加處理器；不想用驗證鏈就整個拿掉 `[auth]`，改由 `[lua]` 的腳本決定。

**症狀：** `check` 印出 `auth.chains.lab.handlers[0].user_query must contain exactly one ? placeholder, found 2`。
**原因：** `user_query`、`reply_query`、`certificate_query` 都只能有一個 `?`，帳號名稱只代入一次。
**解法：** 改寫查詢，只留一個 `?`。

**症狀：** `check` 印出 `auth.lockout.account_max_failures must be greater than max_failures (3), not 2`，或 `auth.password_limits.pbkdf2_iterations must be greater than 0`。
**原因：** 數值超出範圍。
**解法：** `account_max_failures` 要大於 `max_failures`；每個上限都要大於 0。

**症狀：** `check` 印出 `auth.username.strip_realm: 'left' is not none, suffix, prefix or both`，或 `auth.username.allowed: regex parse error: …`。
**原因：** `strip_realm` 的值不對，或 `allowed` 不是正確的正規表示式。
**解法：** 照[統一使用者名稱與 realm](#統一使用者名稱與-realm)改正。

**症狀：** `check` 印出 `auth.chains.lab: cannot read auth handler script 'missing.lua': No such file or directory (os error 2)`。
**原因：** lua 處理器的 `script` 找不到。相對路徑以設定檔所在的目錄為準。
**解法：** 改正路徑。

**症狀：** 某些帳號偶爾沒有回應，`status` 的 `Discarded: database` 增加，那些帳號存的是 bcrypt 或 PBKDF2。
**原因：** 慢雜湊同時比對的數量等於 CPU 數，尖峰時排隊加比對超過了 `timeout_ms`，算成後端不能用。
**解法：** 檢查存放值的成本是否過高（`[auth.password_limits]`）；只用 PAP 的帳號改用比對較快的 `sha512-crypt`；或加 CPU、加節點。

**症狀：** 設備被鎖住，鎖定時間還沒到，想立刻解鎖。
**原因：** 計數只在節點的記憶體裡，沒有解鎖指令。
**解法：** 等 `lock_seconds` 過去；或把 `[auth.lockout]` 拿掉再 reload（會清掉所有計數），之後再加回來。

## 相關章節

- [03 第一次啟動與登入](03-quick-start.md)
- [04 設定檔的基本功](04-configuration.md)
- [05 RADIUS 服務與 NAS 用戶端](05-radius-clients.md)
- [06 字典與屬性](06-dictionary.md)
- [08 接上 LDAP 與 Active Directory](08-ldap-and-ad.md)
- [09 外部資料庫與具名查詢](09-external-databases.md)
- [10 用 Lua 寫政策](10-lua-policies.md)
- [11 802.1X 與 EAP](11-eap.md)
- [12 TACACS+ 設備管理](12-tacacs.md)
- [17 監控、狀態與日誌](17-monitoring.md)
- [附錄 A：設定參考](90-config-reference.md)
- [附錄 C：指標參考](92-metrics-reference.md)
