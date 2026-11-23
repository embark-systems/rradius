# 08 接上 LDAP 與 Active Directory

> 本章讓你做到：
> - 讓使用者用公司目錄（LDAP 或 Active Directory）的帳號密碼登入
> - 依目錄裡的群組決定誰能登入、給哪些回覆屬性
> - 用 TLS 安全地連到目錄，目錄故障時知道伺服器會怎麼做
> - 在 Active Directory 環境用 Netlogon 驗證 MS-CHAPv2 與 PEAP

前置條件：讀過[第 07 章](07-users-and-chains.md)，知道驗證鏈、處理器與 `first_match`／`all_accept`。需要一台 LDAP 伺服器或 Active Directory 網域控制站，以及一個能搜尋使用者的服務帳號。

本章小節：

- [兩種處理器怎麼選](#兩種處理器怎麼選)
- [本章的測試目錄](#本章的測試目錄)
- [用目錄帳號登入](#用目錄帳號登入)
- [連線的加密：ldaps、StartTLS 與 CA](#連線的加密ldapsstarttls-與-ca)
- [依群組決定能不能登入](#依群組決定能不能登入)
- [依群組給回覆屬性](#依群組給回覆屬性)
- [比對目錄裡的密碼屬性](#比對目錄裡的密碼屬性)
- [憑證登入的搜尋條件](#憑證登入的搜尋條件)
- [目錄連不上的時候](#目錄連不上的時候)
- [remote-access 情境：VPN 用 AD 帳號](#remote-access-情境vpn-用-ad-帳號)
- [Active Directory 的 MS-CHAPv2：netlogon 處理器](#active-directory-的-ms-chapv2netlogon-處理器)
- [把 LDAP 與 netlogon 放在同一條鏈](#把-ldap-與-netlogon-放在同一條鏈)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

## 兩種處理器怎麼選

| | ldap 處理器 | netlogon 處理器 |
| --- | --- | --- |
| 連到 | 任何 LDAP 伺服器，含 AD 的 LDAP | AD 網域控制站的 Netlogon 服務 |
| 驗證方式 | 以使用者身分 bind（預設），或讀出密碼屬性比對 | 把 MS-CHAPv2 的回應交給網域控制站驗證 |
| 支援的登入 | bind：只有 PAP。讀屬性：PAP、CHAP、MS-CHAPv1、MS-CHAPv2，看存的格式（MS-CHAPv1 未實機驗證） | 只有 MS-CHAPv2（含 PEAP 內層） |
| 群組 | DN，例如 `CN=vpn,OU=Groups,DC=corp,…` | SID，例如 `S-1-5-21-…-1106` |
| 適合 | VPN、網頁登入、TACACS+ 這類送 PAP 的設備 | Wi-Fi 802.1X（PEAP-MSCHAPv2）用 AD 帳號 |

為什麼 AD 要另外一種處理器：Wi-Fi 的 PEAP 送來的是 MS-CHAPv2 回應，不是明文密碼，沒辦法拿去 bind；而 AD 也不會透過 LDAP 交出使用者的 NT hash，伺服器無從自己比對。netlogon 處理器把回應交給網域控制站，由它回答對不對。

兩種處理器可以放在同一條鏈，各自處理自己的那種登入，見[把 LDAP 與 netlogon 放在同一條鏈](#把-ldap-與-netlogon-放在同一條鏈)。

## 本章的測試目錄

本章的輸出來自一台在 Docker 裡跑的 OpenLDAP（alpine 映像加上 OpenLDAP 套件，啟用 `memberof` overlay，用自簽的 CA 開 ldaps 與 StartTLS），以及 rradius 原始碼裡 `docker/interop-netlogon/` 的 Samba AD 網域控制站映像。它們只是測試環境，你的目錄結構可能不同。

目錄裡的資料：

| DN | 密碼 | 所屬群組 | 其他 |
| --- | --- | --- | --- |
| `cn=radius,ou=services,dc=example,dc=com` | `svc-password` | — | 服務帳號，能讀 `userPassword` |
| `uid=alice,ou=people,dc=example,dc=com` | `alice-password`（明文） | `vpn-users`、`it-staff` | `description: std.ppp` |
| `uid=bob,ou=people,dc=example,dc=com` | `bob-password`（明文） | 無 | |
| `uid=dave,ou=people,dc=example,dc=com` | `dave-password`（存成 `{SSHA}`） | `vpn-users` | |

群組是 `ou=groups,dc=example,dc=com` 底下的 `groupOfNames`。

伺服器的其他設定照[第 03 章](03-quick-start.md)的 quick-start（單機叢集、一個 `127.0.0.1` 的用戶端）。登入測試用 `rradius-bench`，共同的參數省略成 `…`。

> **未實機驗證：** 沒有在真正的 Windows Server Active Directory 上測過。LDAP 的部分在 OpenLDAP 測過；netlogon 的部分在 Samba AD 網域控制站測過。

## 用目錄帳號登入

目標：使用者用目錄的帳號密碼，以 PAP 登入。

ldap 處理器的做法：

1. 用服務帳號（`bind_dn`）bind。
2. 在 `base_dn` 底下用 `filter` 搜尋使用者，`{user}` 換成帳號名稱。
3. 以找到的那筆 DN 和使用者送來的密碼再 bind 一次。成功就接受。

```toml
# rradius.toml（節錄）
[auth]
[[auth.handlers]]
type = "ldap"
urls = ["ldaps://ldap1.example.com", "ldaps://ldap2.example.com"]
bind_dn = "cn=radius,ou=services,dc=example,dc=com"
bind_password_file = "ldap.secret"
base_dn = "ou=people,dc=example,dc=com"
filter = "(uid={user})"
ca_file = "corp-ca.pem"
reply_attributes = { "Filter-Id" = "description" }

[auth.username]
strip_realm = "both"
lowercase = true
```

| 鍵 | 預設 | 說明 |
| --- | --- | --- |
| `urls` | —（必填） | `ldap://` 或 `ldaps://` 開頭，至少一個，依序嘗試 |
| `bind_dn` | —（必填） | 服務帳號的 DN |
| `bind_password_file` | — | 服務帳號密碼的檔案；行尾的換行會去掉 |
| `bind_password` | — | 直接寫密碼。和 `bind_password_file` 剛好設一個 |
| `base_dn` | —（必填） | 搜尋使用者的起點，往下整個子樹 |
| `filter` | —（必填） | 搜尋條件，剛好一個 `{user}` |
| `password_check` | `bind` | `bind` 或 `attribute`（見[比對目錄裡的密碼屬性](#比對目錄裡的密碼屬性)） |
| `reply_attributes` | — | 回覆屬性名稱對應到使用者的 LDAP 屬性 |
| `ca_file` | — | 信任的 CA 憑證（見下一節） |
| `allow_plaintext` | `false` | 允許不加密的 `ldap://` |
| `pool_size` | `4` | 每個 URL 保持開啟的連線數 |
| `timeout_ms` | `5000` | 對一個 URL 的連線加整個查詢的時限，毫秒 |

每個鍵都可以 reload。群組相關的鍵在後面兩節。完整欄位見[附錄 A](90-config-reference.md)。

建立密碼檔：

```console
$ printf '%s\n' '<服務帳號的密碼>' > ldap.secret
$ chmod 600 ldap.secret
```

- `{user}` 代入之前會依 RFC 4515 跳脫（`*`、`(`、`)`、`\` 與 NUL 換成 `\2a` 這類寫法），使用者名稱改變不了搜尋條件的結構。（依程式碼，本章沒有另外測。）
- 代入的是驗證鏈整理後的帳號名稱。上面的 `[auth.username]` 讓 `CORP\Alice`、`alice@example.com` 都用 `alice` 搜尋（[第 07 章](07-users-and-chains.md)）。
- `reply_attributes` 的值有好幾個時，每個值各加一個屬性，順序照目錄傳回的順序。
- 服務帳號的密碼不能是空的：空密碼的 bind 是匿名 bind，`check` 直接拒絕。
- **用 `filter` 排除停用的帳號。** 搜尋不到的使用者視同不存在，所以停用的帳號可以直接在條件裡排除。AD 的寫法是 `(&(sAMAccountName={user})(!(userAccountControl:1.2.840.113556.1.4.803:=2)))`（`userAccountControl` 的第 2 位元是停用）；OpenLDAP 若啟用了密碼政策（ppolicy），可以用 `(&(uid={user})(!(pwdAccountLockedTime=*)))` 排除被鎖定的帳號。`check` 只檢查 `{user}` 的個數，條件本身有沒有用要先在你的目錄上試過。

> **未實機驗證：** 兩種排除寫法的目錄端行為（`userAccountControl` 位元、ppolicy 的 `pwdAccountLockedTime`）沒有在真正的 AD 或啟用 ppolicy 的 OpenLDAP 上測過；程式碼只負責把條件原樣送出，`check` 確認過 OpenLDAP 的那一條能通過。

### 驗證

```console
$ rradius-bench run … --user alice --password alice-password --load pap,concurrency=1 --requests 1
…  accept 1, reject 0 …
$ rradius-bench run … --user alice --password wrong --load pap,concurrency=1 --requests 1
…  accept 0, reject 1 …
$ rradius-bench run … --user nobody --password x --load pap,concurrency=1 --requests 1
…  accept 0, reject 1 …
$ rradius-bench run … --user 'EXAMPLE\Alice' --password alice-password --load pap,concurrency=1 --requests 1
…  accept 1, reject 0 …
```

認證紀錄：

```text
… user="alice" account="alice" chain="default" handler="ldap" decision="accept"
… user="alice" account="alice" chain="default" handler="ldap" decision="reject"
… user="nobody" account="nobody" chain="default" handler="none" decision="reject"
… user="EXAMPLE\\Alice" account="alice" chain="default" handler="ldap" decision="accept"
```

- 密碼錯誤是 `handler="ldap"` 的 reject。
- 目錄裡找不到的名稱，ldap 處理器 ignore，交給下一個處理器；這裡沒有下一個，所以是 `handler="none"`。
- 搜尋找到兩筆以上，處理器拒絕，日誌寫 `filter matched 2 entries; rejecting`（依程式碼與它的測試；本章的測試目錄沒有重複的帳號）。

用 `[log.trace]` 看 `alice` 的 Access-Accept（[第 17 章](17-monitoring.md)），`description` 的值變成了 `Filter-Id`：

```text
… INFO rradius_trace: response client="localhost" source=127.0.0.1 user="alice" code=2 attributes="Message-Authenticator = <hidden>, Filter-Id = std.ppp, Class = it-staff, Session-Timeout = 28800"
```

（`Class` 與 `Session-Timeout` 來自[依群組給回覆屬性](#依群組給回覆屬性)。）

bind 模式不能驗證 CHAP 與 MS-CHAPv2：這兩種登入沒有明文密碼可以 bind，處理器直接拒絕（不是 ignore）。實測 `alice` 用 CHAP 與 MS-CHAPv2 都是 `reject 1`，MS-CHAPv2 的 Access-Reject 帶 `MS-CHAP-Error`。

**已驗證**：`alice`、錯誤密碼、`nobody`、`EXAMPLE\Alice` 四筆登入，trace 的回應，以及 CHAP 與 MS-CHAPv2 的拒絕。

另外依程式碼：空的 PAP 密碼直接拒絕，不送出 bind，因為空密碼的 bind 在 LDAP 裡是匿名登入，會成功。

### 連線怎麼重複使用

- 每個 URL 各有一個連線池，最多 `pool_size` 條。新的連線先完成 TLS 與服務帳號的 bind 才拿來用。
- 連線放回池子再拿出來時，先以服務帳號重新 bind（時限是 `timeout_ms` 的一半），所以上一個使用者的 bind 身分不會帶到下一筆請求。
- 搜尋失敗、bind 出現密碼錯誤以外的錯誤、或超過 `timeout_ms` 的連線，直接丟掉，不放回池子。密碼錯誤（invalidCredentials）的連線照常放回。
- RADIUS 與 TACACS+ 的用戶端指向同一條鏈時，共用這些連線池。

## 連線的加密：ldaps、StartTLS 與 CA

使用者的密碼會送到目錄去 bind，所以連線一定要加密。

| `urls` 寫法 | `allow_plaintext` | 結果 |
| --- | --- | --- |
| `ldaps://host[:port]` | 不論 | 一開始就用 TLS（預設埠 636） |
| `ldap://host[:port]` | `false`（預設） | 連上之後先做 StartTLS，才 bind |
| `ldap://host[:port]` | `true` | 不加密。服務帳號與使用者的密碼都以明文傳送 |

兩種 TLS 都會驗證伺服器的憑證與主機名稱。信任的 CA 是作業系統的根憑證，再加上 `ca_file` 裡的憑證（PEM，可以放好幾張）。公司目錄用自己的 CA 簽憑證時，把那張 CA 放進 `ca_file`。

**已驗證**：

- `ldaps://`、有 `ca_file`：登入成功。
- `ldap://` 加 StartTLS、有 `ca_file`：登入成功。
- 拿掉 `ca_file`：`ldaps://` 與 StartTLS 都連不上，日誌寫

  ```text
  … WARN rradius_core::ldap_handler: auth ldap handler: ldaps://localhost:636: I/O error: invalid peer certificate: UnknownIssuer
  … ERROR rradius_core::ldap_handler: auth ldap handler: no server reachable; rejecting
  ```

  這筆登入沒有回應（見[目錄連不上的時候](#目錄連不上的時候)）。
- `ldap://` 加 `allow_plaintext = true`：登入成功，但沒有加密。

> **警告：** `allow_plaintext = true` 讓密碼以明文走在網路上。只在測試環境，或連線本身已經在加密通道裡（例如 WireGuard）時使用。

`urls` 寫的主機名稱要和目錄憑證上的名稱相符。寫 IP 位址時，憑證要有那個 IP 的 SAN。

## 依群組決定能不能登入

目標：密碼正確，還要屬於某個群組才放行。

```toml
# rradius.toml（加在 ldap 處理器裡）
required_groups = ["cn=vpn-users,ou=groups,dc=example,dc=com"]
```

使用者要屬於 `required_groups` 裡的**其中一個**群組，才會接受。群組的 DN 比對時不分大小寫，也不管逗號前後的空白，所以 `CN=VPN-Users, OU=Groups, DC=example, DC=com` 和上面那行算同一個。

處理器從哪裡得知使用者的群組，有兩種方式：

| 方式 | 設定 | 適合 |
| --- | --- | --- |
| 讀使用者的屬性 | `group_attribute`，預設 `memberOf` | AD；有 `memberof` overlay 的 OpenLDAP |
| 搜尋群組 | `group_base_dn` 加 `group_filter`，例如 `(member={dn})` | 群組記在群組那一邊（`groupOfNames`），使用者身上沒有 `memberOf` |

```toml
# 第二種：搜尋群組（加在 ldap 處理器裡）
group_base_dn = "ou=groups,dc=example,dc=com"
group_filter = "(member={dn})"
required_groups = ["cn=vpn-users,ou=groups,dc=example,dc=com"]
```

- `group_filter` 剛好一個 `{dn}`，換成使用者那筆的 DN（跳脫過）。搜尋以服務帳號執行，結果的 DN 拿去比對。
- `group_attribute` 與 `group_filter` 只能選一個；`group_filter` 一定要配 `group_base_dn`。
- 這些鍵只在有 `required_groups` 或 `group_reply` 時才有意義。只寫了它們，`check` 會失敗，因為看起來像限制、其實什麼都沒限制。
- **巢狀群組：** `memberOf` 與 `(member={dn})` 都只算直接的成員關係，使用者透過子群組加入的不算。AD 要把巢狀群組算進去，改用搜尋群組的方式，`group_filter = "(member:1.2.840.113556.1.4.1941:={dn})"`（AD 的 `LDAP_MATCHING_RULE_IN_CHAIN`），並配上 `group_base_dn`。這是 AD 專有的比對規則，由網域控制站展開巢狀群組。

> **未實機驗證：** 這個 `group_filter` 的寫法 `check` 接受（剛好一個 `{dn}`），但沒有在 AD 上跑過。

### 驗證

`bob` 的密碼正確，但不在 `vpn-users`；`alice` 與 `dave` 在：

```console
$ rradius-bench run … --user bob --password bob-password --load pap,concurrency=1 --requests 1
…  accept 0, reject 1 …
$ rradius-bench run … --user dave --password dave-password --load pap,concurrency=1 --requests 1
…  accept 1, reject 0 …
```

**已驗證**：讀 `memberOf`（預設）與搜尋群組兩種方式，結果相同。

不在群組裡的拒絕，和密碼錯誤的拒絕，NAS 收到的 Access-Reject 一模一樣，外人看不出是哪一種。處理器在比對密碼之前就以服務帳號查好群組，等密碼結果出來才一起決定。

## 依群組給回覆屬性

目標：IT 人員連上 VPN 後套用不同的規則、可以待得比較久。

```toml
# rradius.toml（接在 ldap 處理器之後）
[[auth.handlers.group_reply]]
group = "cn=it-staff,ou=groups,dc=example,dc=com"
attributes = { "Class" = "it-staff", "Session-Timeout" = "28800" }
```

- 使用者屬於 `group` 時，加上 `attributes` 裡的回覆屬性。可以寫好幾個 `group_reply`，依設定的順序加，排在 `reply_attributes` 之後。
- 屬性名稱要在字典裡，值的格式要對：`check` 會把每個值編碼一次，寫錯直接報錯，不會等到使用者登入才失敗。
- 有 `group_reply` 但沒有 `required_groups` 時，任何人都能登入，只是群組決定多拿哪些屬性。

`alice` 在 `it-staff`，前面 `[log.trace]` 的那筆回應裡就有 `Class = it-staff, Session-Timeout = 28800`；`dave` 不在，他的 Access-Accept 沒有這兩個屬性。**已驗證**。

回覆屬性要在 NAS 那邊有對應的設定才有作用，例如 VPN 閘道上要有名稱相同的 filter 或 group policy。

## 比對目錄裡的密碼屬性

`password_check = "attribute"` 讓處理器不以使用者身分 bind，而是讀出使用者的密碼屬性，自己比對：

```toml
# rradius.toml（ldap 處理器裡）
password_check = "attribute"
password_attribute = "userPassword"
```

比對的規則和 sql 處理器一樣（[第 07 章](07-users-and-chains.md)的格式表）：`{SSHA}`、`{crypt}` 這些只能驗證 PAP；明文或 `{nthash}` 還能驗證 MS-CHAPv2，明文另外能驗證 CHAP。MS-CHAPv1 也照同樣的規則比對（`[auth]` 的 `allow_mschapv1 = false` 會在處理器之前就拒絕它，見[第 07 章](07-users-and-chains.md)），這一點**未實機驗證**。

用途：

- 目錄裡存的是明文或 NT hash，而 NAS 送的是 MS-CHAPv2 或 CHAP。
- 目錄不允許一般使用者 bind，但允許服務帳號讀密碼屬性。

**已驗證**：`alice`（目錄裡存明文）用 PAP、CHAP、MS-CHAPv2 都 `accept 1`；`dave`（`{SSHA}`）用 PAP `accept 1`、用 MS-CHAPv2 `reject 1`。

> **警告：** 這個模式要讓服務帳號讀得到每個人的密碼屬性。這個帳號外洩，等於所有密碼外洩（明文或 NT hash 可以直接拿來登入）。能用 bind 模式就用 bind 模式。

- 使用者那筆沒有 `password_attribute` 的值，處理器拒絕。
- 讀出的值拿回來之後才比對，比對期間不佔用連線；比對超過 `timeout_ms`，算「後端不能用」。

## 憑證登入的搜尋條件

EAP-TLS 用憑證登入時，沒有密碼可以 bind。ldap 處理器要有 `certificate_filter` 才會處理這種登入：

```toml
# rradius.toml（ldap 處理器裡）
certificate_filter = "(&(uid={user})(!(nsAccountLock=TRUE)))"
```

- 憑證通過之後，用 `certificate_filter` 搜尋憑證上的名稱，找到（而且符合 `required_groups`）就接受，不以使用者身分 bind。
- 沒有 `certificate_filter` 時，處理器 ignore 憑證登入，不去問目錄，並寫一行警告 `ignoring a certificate login: this handler has no certificate_filter`。原因：單純搜尋看不出帳號是否已經停用或鎖定，要由你在條件裡寫明。
- 上面的 `nsAccountLock` 只是例子；用你的目錄標示停用的那個屬性。AD 的寫法見下面的 remote-access 情境。

EAP-TLS 的設定見[第 11 章](11-eap.md)。

> **未實機驗證：** 憑證登入經過 ldap 處理器這一段，沒有在本章的測試目錄跑過，只對照了程式碼與測試。

## 目錄連不上的時候

ldap 處理器依序嘗試 `urls`：

| 情況 | 處理器怎麼做 |
| --- | --- |
| 連不上、TLS 失敗、連線中斷、逾時 | 換下一個 URL |
| 服務帳號 bind 被拒絕，或搜尋時伺服器回錯誤 | 不再試別的 URL，算「目錄不能用」 |
| 所有 URL 都試過了 | 算「目錄不能用」，日誌 `no server reachable; rejecting` |
| 使用者 bind 回 busy（51）、unavailable（52）、serverDown（81） | 算「目錄不能用」 |
| 使用者 bind 被拒絕（密碼錯誤，或帳號停用這類其他原因） | 拒絕這個使用者，不算目錄的問題 |
| 不在 `required_groups` | 拒絕，不算目錄的問題 |

「目錄不能用」時，RADIUS **不回應**這筆請求，計入 `rradius_radius_discarded_total{reason="database"}`；TACACS+ 回 ERROR；[登入失敗鎖定](07-users-and-chains.md)不計數。這樣 NAS 會重送或改問另一台 RADIUS 伺服器，而不是告訴使用者密碼錯了。

### 驗證

第一個 URL 故意指向沒有服務在聽的埠 1389，第二個是正常的目錄：

```toml
urls = ["ldap://localhost:1389", "ldap://localhost:389"]
```

登入成功，每一筆都先試第一個，日誌各寫一行：

```text
… WARN rradius_core::ldap_handler: auth ldap handler: ldap://localhost:1389: I/O error: Connection refused (os error 61)
```

把目錄的容器停掉，再登入一次：`rradius-bench` 的結果是 `timeouts 1`（伺服器沒有回應），日誌：

```text
… WARN rradius_core::ldap_handler: auth ldap handler: ldaps://localhost:636: timeout: deadline has elapsed
… ERROR rradius_core::ldap_handler: auth ldap handler: no server reachable; rejecting
```

`/metrics` 的 `rradius_radius_discarded_total{reason="database"}` 跟著增加。**已驗證**。

> **注意：** 請求依 `urls` 的順序試。某個 URL 連不上（連線被拒、TLS 失敗、中斷、逾時）之後的 30 秒內，請求先試其他 URL，最後才回頭試它；30 秒後再給它一次機會，恢復了就回到它。所以第一台長時間故障時，只有 30 秒一次的那一筆請求要等它失敗（連線被拒絕很快，沒有回應的主機要等到 `timeout_ms`），其餘直接由後面的 URL 回答。所有 URL 都在這 30 秒內失敗過時，照列出的順序試，多台都沒有回應時一筆請求要等「URL 數 × `timeout_ms`」。伺服器回了錯誤（例如服務帳號 bind 被拒）不算連不上，不會被排到後面。這份記憶只在處理器內，reload 之後重新開始。把最可靠的伺服器放第一個；有一台要長期停機時，仍建議先把它從 `urls` 拿掉再 reload。

## remote-access 情境：VPN 用 AD 帳號

範例情境 `remote-access` 的 `vpn` 鏈，是用 ldap 處理器接 AD 的寫法：

```toml
# rradius.toml（remote-access，節錄）
[auth.chains.vpn]
policy = "first_match"

[[auth.chains.vpn.handlers]]
type = "ldap"
urls = ["ldaps://dc1.corp.example.com", "ldaps://dc2.corp.example.com"]
bind_dn = "CN=svc-radius,OU=Service Accounts,DC=corp,DC=example,DC=com"
bind_password_file = "ldap.secret"
base_dn = "OU=People,DC=corp,DC=example,DC=com"
filter = "(&(sAMAccountName={user})(!(userAccountControl:1.2.840.113556.1.4.803:=2)))"
password_check = "bind"
required_groups = ["CN=vpn-users,OU=Groups,DC=corp,DC=example,DC=com"]

[[auth.chains.vpn.handlers.group_reply]]
group = "CN=it-staff,OU=Groups,DC=corp,DC=example,DC=com"
attributes = { "Filter-Id" = "vpn-it" }

[[auth.chains.vpn.handlers.group_reply]]
group = "CN=contractors,OU=Groups,DC=corp,DC=example,DC=com"
attributes = { "Filter-Id" = "vpn-contractor", "Session-Timeout" = "28800" }

[auth.chains.vpn.username]
strip_realm = "both"
lowercase = true

[[radius.clients]]
name = "vpn-gw"
address = "10.60.0.1"
secret_file = "vpn-gw.secret"
auth = "vpn"
```

重點：

- AD 的帳號名稱在 `sAMAccountName`。`filter` 的第二段排除了停用的帳號（`userAccountControl` 的第 2 位元），停用的帳號搜尋不到，等同不存在。
- 群組用預設的 `memberOf`，AD 會在使用者身上寫好。
- 使用者可能輸入 `CORP\alice` 或 `alice@corp.example.com`，`strip_realm = "both"` 都整理成 `alice`。
- 這是具名的鏈，只有 `auth = "vpn"` 的 VPN 閘道走這裡。同一個情境的跳板機走另一條 `mfa` 鏈（[第 15 章](15-access-recipes.md)）。
- 目錄伺服器的憑證不是系統信任的 CA 簽的時候，加 `ca_file`。

這份設定過得了 `check`（`check` 不會連到目錄）。**已驗證**：照第 03 章的方式複製 `rradius-server/examples/scenarios/remote-access/`，把 `dictionary` 改成字典的實際路徑、密碼檔改成 0600 之後，`check` 印 `configuration ok`（另外警告兩個用戶端還在用範例附的公開 secret）。

> **未實機驗證：** 這個情境沒有接上真正的 AD 跑過。過濾停用帳號的寫法是 AD 的語法，在本章的 OpenLDAP 測試目錄裡不適用。

## Active Directory 的 MS-CHAPv2：netlogon 處理器

目標：Wi-Fi 用 PEAP-MSCHAPv2，使用者輸入 AD 的帳號密碼就能連上，伺服器上不必裝 Samba。

netlogon 處理器以一個**電腦帳號**的身分，和網域控制站建立 Netlogon secure channel，把每一筆 MS-CHAPv2 回應交給網域控制站驗證。網域控制站回答接受時，也一併傳回算 MPPE 金鑰需要的資料，Access-Accept 裡的 `MS-CHAP2-Success`、`MS-MPPE-Send-Key`、`MS-MPPE-Recv-Key` 和本機驗證時算出來的完全一樣，所以 PEAP 的 EAP-Success 照常產生 MSK。

### 準備電腦帳號

處理器需要：

- 一個網域裡的電腦帳號，名稱最多 15 個字元，在設定裡**不含**結尾的 `$`（例如網域裡叫 `RRADIUS1$`，設定寫 `RRADIUS1`）。
- 這個電腦帳號的密碼，放在一個檔案裡（`machine_password_file`），行尾的換行會去掉。
- 建議每個節點用自己的電腦帳號。網域控制站對一個電腦帳號只記一組 secure channel 的狀態，幾台主機共用一個帳號時，後建立的會讓先建立的失效。這一點來自 Netlogon 協定本身的行為，程式碼與規格沒有寫，**未實機驗證**。

在網域裡建立電腦帳號並設定一個已知的密碼，要用你的網域管理工具。本章測試用的 Samba 網域控制站是這樣建的（只是測試環境的做法）：

```console
$ samba-tool computer create RRADIUS1
$ samba-tool user setpassword 'RRADIUS1$' --newpassword='…'
```

在 Windows 網域（需要 RSAT 的 ActiveDirectory 模組）可以用 PowerShell 建立。名稱最多 15 個字元，每台節點用自己的帳號：

```powershell
$pw = Read-Host -AsSecureString "RRADIUS1 的密碼"
New-ADComputer -Name RRADIUS1 -SamAccountName 'RRADIUS1$' -AccountPassword $pw -Enabled $true
```

> **未實機驗證：** 這段 `New-ADComputer` 沿用舊版手冊，沒有在 Windows Server 上跑過。

Samba AD 網域控制站預設不接受 MS-CHAPv2。在每台網域控制站的 `/etc/samba/smb.conf` 的 `[global]` 加上下面這行，再重新啟動 samba：

```ini
ntlm auth = mschapv2-and-ntlmv2-only
```

本章的測試映像（`docker/interop-netlogon/dc-start.sh`）建網域時就帶這個選項。沒有它時，Samba 對正確的密碼也可能回 `STATUS_WRONG_PASSWORD`（見[常見錯誤](#常見錯誤)）。

電腦帳號的密碼存到節點上，只給執行 rradius 的帳號讀：

```console
$ install -m 600 -o rradius /dev/null /etc/rradius/netlogon-rradius1.secret
$ read -rs pw && printf '%s' "$pw" > /etc/rradius/netlogon-rradius1.secret
```

> **警告：** 電腦帳號的密碼就是這台伺服器在網域裡的身分。檔案權限設成 0600；`check` 與 `server` 發現檔案別人讀得到時會警告。

### 設定

```toml
# rradius.toml（節錄）
[auth]
[[auth.handlers]]
type = "netlogon"
domain = "CORP"
servers = ["dc1.corp.example", "dc2.corp.example"]
computer_name = "RRADIUS1"
machine_password_file = "machine.secret"
required_groups = ["S-1-5-21-3266951882-2843208589-237589800-1106"]

[[auth.handlers.group_reply]]
group = "S-1-5-21-3266951882-2843208589-237589800-1106"
attributes = { "Filter-Id" = "staff" }
```

| 鍵 | 預設 | 說明 |
| --- | --- | --- |
| `domain` | —（必填） | 網域的 NetBIOS 名稱 |
| `servers` | —（必填） | 網域控制站的 DNS 或 NetBIOS 名稱，依序嘗試 |
| `port` | —（先問 135 埠的端點對應器） | 固定的 Netlogon 埠 |
| `computer_name` | —（必填） | 電腦帳號，不含 `$`，最多 15 個字元 |
| `machine_password_file` | —（必填） | 電腦帳號密碼的檔案 |
| `connections` | `4` | 每台網域控制站最多保持開啟的連線數 |
| `timeout_ms` | `2000` | 一次呼叫網域控制站的時限，毫秒 |
| `required_groups` | — | 群組的 SID，使用者要屬於其中一個 |
| `group_reply` | — | 屬於某個 SID 時加上的回覆屬性 |

每個鍵都可以 reload。

- `servers` **不能寫 IP 位址**：登入請求裡要帶網域控制站的名稱，Windows 網域控制站會拒絕以位址稱呼它的請求。`check` 遇到位址直接報錯。
- 沒有 `port` 時，先連網域控制站的 135 埠問端點對應器，Netlogon 在哪個埠；網域控制站換了埠，處理器會重新問。中間有防火牆時，要開 135 埠和它回答的那個埠，或在網域控制站固定 Netlogon 的埠，再寫在 `port`。Windows 網域控制站的動態 RPC 埠預設是 TCP 49152 到 65535，沒有固定埠時整段都要開（未實機驗證；Samba 測試映像固定在 49200）。
- 群組用 SID 寫，形如 `S-1-5-21-…`。寫群組名稱（例如 `Domain Users`），`check` 會失敗。群組的 SID 可以在網域管理工具查到：Windows 用 `Get-ADGroup wifi-staff | Select SID`（未實機驗證），測試時用的是 `samba-tool group show wifi-staff --attributes=objectSid`。
- `check` 只檢查設定、讀密碼檔，**不會**連到網域控制站。第一筆請求進來時才建立 secure channel。
- secure channel 一定用 AES。網域控制站不同意 AES，或回來的伺服器憑證驗不過，就當它不能用，不會退回 RC4 或 DES。

### 驗證

測試環境：Samba AD 網域控制站，網域 `CORP`；`alice` 在 `wifi-staff` 群組；`bob` 不在；`carol` 停用。Netlogon 埠固定，所以設定加了 `port`。`rradius-bench` 的 `mschapv2` 負載直接送 MS-CHAPv2 的 Access-Request：

```console
$ rradius-bench run … --user alice --password Alice-Pass-1 --load mschapv2,concurrency=1 --requests 1
…  accept 1, reject 0 …
$ rradius-bench run … --user 'CORP\alice' --password Alice-Pass-1 --load mschapv2,concurrency=1 --requests 1
…  accept 1, reject 0 …
$ rradius-bench run … --user alice --password Wrong-Pass-1 --load mschapv2,concurrency=1 --requests 1
…  accept 0, reject 1 …
$ rradius-bench run … --user carol --password Carol-Pass-1 --load mschapv2,concurrency=1 --requests 1
…  accept 0, reject 1 …
$ rradius-bench run … --user bob --password Bob-Pass-1 --load mschapv2,concurrency=1 --requests 1
…  accept 0, reject 1 …
$ rradius-bench run … --user nobody --password Any-Pass-1 --load mschapv2,concurrency=1 --requests 1
…  accept 0, reject 1 …
$ rradius-bench run … --user alice --password Alice-Pass-1 --load pap,concurrency=1 --requests 1
…  accept 0, reject 1 …
```

伺服器日誌（節錄）：

```text
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="netlogon" decision="accept"
… INFO rradius_trace: response client="localhost" source=127.0.0.1 user="alice" code=2 attributes="Message-Authenticator = <hidden>, MS-CHAP2-Success = <hidden>, MS-MPPE-Send-Key = <hidden>, MS-MPPE-Recv-Key = <hidden>, Filter-Id = staff"
… INFO rradius_core::netlogon::handler: auth netlogon handler: localhost: STATUS_WRONG_PASSWORD; rejecting account="alice"
… INFO rradius_core::netlogon::handler: auth netlogon handler: localhost: STATUS_ACCOUNT_DISABLED; rejecting account="carol"
… INFO rradius_core::netlogon::handler: auth netlogon handler: localhost: not in any of required_groups; rejecting account="bob"
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="nobody" account="nobody" chain="default" handler="none" decision="reject"
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="none" decision="reject"
```

- `alice` 通過，回應帶 MS-CHAPv2 的證明、MPPE 金鑰，以及 `group_reply` 的 `Filter-Id`。
- 名稱寫成 `CORP\alice` 也通過：處理器一律用設定的 `domain`，名稱裡的網域部分去掉。
- 網域控制站回答 `STATUS_NO_SUCH_USER` 時處理器 ignore（`nobody` 最後是 `handler="none"`），讓後面的處理器有機會。
- PAP 不歸 netlogon 處理器管，它 ignore，也不去問網域控制站。

**已驗證**（Samba AD 網域控制站）。PEAP-MSCHAPv2 經過這個處理器時，EAP 的部分見[第 11 章](11-eap.md)；本章沒有另外跑 PEAP。

### 網域控制站的回答

| 網域控制站回答 | 處理器的答案 | `rradius_netlogon_logons_total` 的 `result` |
| --- | --- | --- |
| 成功 | accept（不在 `required_groups` 時 reject） | `accept`（不在群組時 `reject`） |
| `STATUS_NO_SUCH_USER` | ignore | `no_such_user` |
| `STATUS_WRONG_PASSWORD`、`STATUS_ACCOUNT_DISABLED`、`STATUS_ACCOUNT_LOCKED_OUT`、`STATUS_PASSWORD_EXPIRED`、`STATUS_PASSWORD_MUST_CHANGE`、`STATUS_ACCOUNT_EXPIRED`、`STATUS_INVALID_LOGON_HOURS`、`STATUS_INVALID_WORKSTATION` | reject，日誌寫出狀態名稱 | `reject` |
| 其他失敗 | reject，日誌寫出十六進位的狀態碼 | `reject` |
| 以訪客帳號登入的成功 | reject | `reject` |
| 沒有一台網域控制站能回答 | 後端不能用：RADIUS 不回應 | `unavailable` |

- 使用者在 AD 被鎖定、密碼過期，在這裡一樣被拒絕，日誌寫出原因。
- 回答被標成「不具權威」時，換下一台網域控制站問；但上表的那些帳號狀態例外，直接拒絕，因為每一台網域控制站都會給一樣的答案。

### 連線、容錯與指標

- 每台網域控制站最多保持 `connections` 條連線，重複使用。secure channel 被網域控制站作廢或連線斷掉時，重新建立再試一次，還是不行就換下一台。
- 池子裡閒置的連線在 `timeout_ms` 內沒有回應（例如被防火牆悄悄丟掉），關掉那台的閒置連線，用新的連線再試一次。
- 電腦帳號的密碼、session key、NT-Response 與 MPPE 金鑰不會出現在任何層級的日誌。

```console
$ curl -s http://127.0.0.1:9812/metrics | grep '^rradius_netlogon'
rradius_netlogon_logons_total{result="accept"} 2
rradius_netlogon_logons_total{result="reject"} 3
rradius_netlogon_logons_total{result="no_such_user"} 1
rradius_netlogon_logons_total{result="unavailable"} 0
rradius_netlogon_channel_setups_total{result="ok"} 1
rradius_netlogon_channel_setups_total{result="failed"} 0
rradius_netlogon_open_connections 1
```

| 指標 | 意思 |
| --- | --- |
| `rradius_netlogon_logons_total{result}` | 問過網域控制站的 MS-CHAPv2 登入，依結果 |
| `rradius_netlogon_channel_setups_total{result}` | 建立 secure channel 的次數，`ok` 或 `failed` |
| `rradius_netlogon_open_connections` | 目前開著的 Netlogon 連線 |

上面七次登入只建了一次 secure channel，之後都重複使用。`status` 也有一段 `Netlogon`：

```console
$ rradius-server status --config rradius.toml
…
Netlogon
  Open connections         1
  Logons accepted          0.0/s (2)
  Logons rejected          0.0/s (3)
  Logons of unknown users  0.0/s (1)
  Logons without a DC      0.0/s (0)
…
```

**已驗證**。這幾個指標不論有沒有設定 netlogon 處理器都會出現，沒有設定時都是 0。

### 電腦帳號的密碼錯了

把 `machine_password_file` 換成錯的密碼再啟動，第一筆登入時：

```text
… ERROR rradius_core::lua_engine: auth netlogon handler: localhost: cannot set up the secure channel as RRADIUS1$: the domain controller refused the computer account's password (STATUS_ACCESS_DENIED)
… WARN rradius_core::netlogon::handler: auth netlogon handler: localhost: cannot set up the secure channel as RRADIUS1$: the domain controller refused the computer account's password (STATUS_ACCESS_DENIED)
… ERROR rradius_core::lua_engine: auth netlogon handler: no domain controller of CORP could answer; rejecting
```

這筆登入沒有回應（`rradius-bench` 是 `timeouts 1`），`rradius_netlogon_channel_setups_total{result="failed"}` 與 `rradius_netlogon_logons_total{result="unavailable"}` 各加一。**已驗證**。

> **注意：** AD 會定期要求電腦帳號換密碼。這個處理器只讀 `machine_password_file`，不會自己去換。電腦帳號的密碼在網域那邊被改了，要同時更新這個檔案再 reload。

## 把 LDAP 與 netlogon 放在同一條鏈

同一批 AD 使用者，VPN 送 PAP、Wi-Fi 送 PEAP-MSCHAPv2。兩種處理器放在同一條 `first_match` 鏈，**netlogon 放前面**：

```toml
# rradius.toml（節錄）
[auth]
policy = "first_match"

[[auth.handlers]]
type = "netlogon"
domain = "CORP"
servers = ["dc1.corp.example", "dc2.corp.example"]
computer_name = "RRADIUS1"
machine_password_file = "machine.secret"

[[auth.handlers]]
type = "ldap"
urls = ["ldaps://dc1.corp.example", "ldaps://dc2.corp.example"]
bind_dn = "CN=svc-radius,OU=Service Accounts,DC=corp,DC=example,DC=com"
bind_password_file = "ldap.secret"
base_dn = "OU=People,DC=corp,DC=example,DC=com"
filter = "(&(sAMAccountName={user})(!(userAccountControl:1.2.840.113556.1.4.803:=2)))"
```

順序的理由：

- PAP 的登入：netlogon 處理器 ignore，交給 ldap 處理器 bind。
- MS-CHAPv2 的登入：netlogon 處理器決定。反過來把 ldap 放前面的話，bind 模式的 ldap 處理器會**拒絕** MS-CHAPv2（它沒辦法驗證，但不是 ignore），netlogon 永遠輪不到。
- 網域控制站說查無此人（`STATUS_NO_SUCH_USER`）時 netlogon ignore，接著 ldap 處理器在目錄找不到這個人，也拒絕 MS-CHAPv2，結果是拒絕。

也可以分成兩條具名的鏈，VPN 閘道用一條、Wi-Fi 控制器用另一條（[第 07 章](07-users-and-chains.md)），各自只放一種處理器，比較好懂。

> **未實機驗證：** 這個組合只過了 `check`，沒有同時接上 LDAP 與網域控制站跑過；處理器的順序規則依程式碼。

## 常見錯誤

**症狀：** LDAP 使用者用 PAP 可以登入，Wi-Fi（PEAP）或 MS-CHAPv2 一律被拒絕，認證紀錄是 `handler="ldap" decision="reject"`。
**原因：** bind 模式只能驗證 PAP。
**解法：** AD 改用 netlogon 處理器；其他目錄若存的是明文或 NT hash，改用 `password_check = "attribute"`。

**症狀：** 登入沒有回應，日誌有 `invalid peer certificate: UnknownIssuer` 與 `no server reachable; rejecting`。
**原因：** 目錄的憑證不是信任的 CA 簽的。
**解法：** 把簽發的 CA 放進 `ca_file`。名稱不符時，`urls` 改寫成憑證上的主機名稱。

**症狀：** 登入沒有回應，日誌有 `service account bind failed: …`。
**原因：** 服務帳號的 DN 或密碼錯了，或帳號被停用。
**解法：** 確認 `bind_dn` 與 `ldap.secret` 的內容。這種情況不會再試下一個 URL。

**症狀：** 密碼正確，仍然被拒絕，日誌沒有錯誤。
**原因：** 使用者不在 `required_groups` 裡；或群組從 `memberOf` 讀，但目錄沒有在使用者身上維護 `memberOf`。
**解法：** 確認群組的 DN；目錄沒有 `memberOf` 時改用 `group_base_dn` 與 `group_filter`。

**症狀：** `check` 印出 `auth.handlers[0].filter must contain exactly one {user} placeholder, found 0`。
**原因：** `filter`（或 `certificate_filter`）沒有 `{user}`，或有兩個以上。
**解法：** 只寫一個 `{user}`。

**症狀：** `check` 印出 `auth.handlers[0].urls: 'localhost:636' must start with ldap:// or ldaps://`。
**原因：** URL 少了 scheme。
**解法：** 寫成 `ldaps://主機:埠`。

**症狀：** `check` 印出 `auth.handlers[0].bind_password must not be empty: an empty password makes an anonymous bind`，或 `bind_password: exactly one of bind_password or bind_password_file must be set`。
**原因：** 服務帳號的密碼是空的，或兩個鍵都寫了（都沒寫）。
**解法：** 只留 `bind_password_file`，檔案裡放非空的密碼。

**症狀：** `check` 印出 `auth.handlers[0].group_attribute has no effect without required_groups or group_reply`。
**原因：** 寫了群組的查法，卻沒有用到群組的設定。
**解法：** 加上 `required_groups` 或 `group_reply`，或拿掉這個鍵。`group_filter needs group_base_dn` 與 `required_groups must list at least one group; leave it out to require none` 也是同一類。

**症狀：** `check` 印出 `auth.handlers: group_reply: unknown attribute 'No-Such-Attr'`。
**原因：** `group_reply` 的屬性名稱不在字典裡。
**解法：** 改正名稱（[第 06 章](06-dictionary.md)）。

**症狀：** `check` 印出 `auth.handlers[0].password_attribute is required when password_check = "attribute"`。
**原因：** 用了 attribute 模式，沒說是哪個屬性。
**解法：** 加 `password_attribute = "userPassword"`（依你的目錄）。

**症狀：** `check` 印出 `auth.handlers[0].servers: '192.0.2.10' is an address or has a port; give the domain controller's DNS or NetBIOS name, which the logon names it by (a fixed port goes in port)`。
**原因：** netlogon 的 `servers` 寫了 IP 位址或 `主機:埠`。
**解法：** 寫網域控制站的名稱；固定的埠寫在 `port`。

**症狀：** `check` 印出 `auth.handlers[0].computer_name 'RRADIUS1$' must have 1 to 15 characters and no trailing $`。
**原因：** `computer_name` 帶了 `$`，或超過 15 個字元。
**解法：** 去掉 `$`；名稱太長就在網域裡建一個短一點的電腦帳號。

**症狀：** `check` 印出 `auth.handlers[0].required_groups: 'Domain Users' is not a SID of the form S-1-<authority>-<sub-authority>...`。
**原因：** netlogon 的群組要寫 SID，不能寫名稱。
**解法：** 查出群組的 SID 再寫。

**症狀：** MS-CHAPv2 登入沒有回應，日誌有 `cannot set up the secure channel as RRADIUS1$: the domain controller refused the computer account's password (STATUS_ACCESS_DENIED)`。
**原因：** `machine_password_file` 的密碼和網域裡電腦帳號的密碼不同。
**解法：** 在網域重設電腦帳號的密碼，更新檔案，reload。

**症狀：** 日誌有 `cannot set up the secure channel as RRADIUS1$: the domain controller has no such computer account (STATUS_NO_TRUST_SAM_ACCOUNT)`。
**原因：** 網域裡沒有這個電腦帳號，或 `computer_name` 打錯。
**解法：** 在網域建立電腦帳號（[準備電腦帳號](#準備電腦帳號)），或改正 `computer_name`。

**症狀：** 日誌有 `no Netlogon endpoint registered yet; the domain controller may still be starting`。
**原因：** 網域控制站的 135 埠已經回應，但 Netlogon 服務還沒註冊，常見於網域控制站剛開機。
**解法：** 等它起來，通常會自行恢復。一直出現就檢查網域控制站上的 Netlogon 服務。

**症狀：** 兩台節點輪流出現 `STATUS_ACCESS_DENIED`。
**原因：** 兩台節點共用了同一個電腦帳號。
**解法：** 每台節點用自己的電腦帳號。（沿用舊版手冊的說法，未實機驗證；原理見[準備電腦帳號](#準備電腦帳號)。）

**症狀：** Samba 網域控制站，密碼正確仍被拒絕，日誌有 `STATUS_WRONG_PASSWORD`。
**原因：** `smb.conf` 沒有 `ntlm auth = mschapv2-and-ntlmv2-only`，Samba 不接受 MS-CHAPv2。
**解法：** 加上這一行，重新啟動 samba。（未實機驗證：測試映像一開始就帶這個選項，沒有試過少了它的結果。）

**症狀：** 日誌有 `not in any of required_groups`，密碼明明正確。
**原因：** 使用者不在 `required_groups` 列出的群組，或 SID 抄錯。
**解法：** 對照使用者的群組 SID。

## 相關章節

- [07 使用者帳號與驗證鏈](07-users-and-chains.md)
- [06 字典與屬性](06-dictionary.md)
- [11 802.1X 與 EAP](11-eap.md)
- [12 TACACS+ 設備管理](12-tacacs.md)
- [15 情境實作：組織網路存取](15-access-recipes.md)
- [17 監控、狀態與日誌](17-monitoring.md)
- [20 安全強化](20-security.md)
- [附錄 A：設定參考](90-config-reference.md)
- [附錄 C：指標參考](92-metrics-reference.md)
