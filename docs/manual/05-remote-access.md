# 5. 遠端存取：VPN 用公司目錄登入，跳板機加驗證碼

**情境：**

- **VPN 閘道** 用公司目錄（Active Directory 或 OpenLDAP）的帳號密碼登入。只有 `vpn-users` 群組的人能連；IT 人員和外包人員依群組套用不同的存取規則，外包人員最多連 8 小時。
- **跳板機**（SSH 用 pam_radius，或其他支援 RADIUS 的系統）要求密碼之後再輸入手機驗證器 App 的 6 位數驗證碼（TOTP）。

兩種設備各用一條**具名串接**：client 的 `auth = "名稱"` 決定走哪一條。

範例目錄：[`rradius-server/examples/scenarios/remote-access/`](../../rradius-server/examples/scenarios/remote-access/)

| 檔案 | 內容 |
| --- | --- |
| `rradius.toml` | `vpn` 和 `mfa` 兩條串接 |
| `two-step-totp.lua` | 跳板機的兩步驟驗證 |
| `accounting.lua` | 計費腳本 |
| `ldap.secret` | 目錄服務帳號的密碼（示範值） |
| `migrations/1_schema.sql` | 驗證碼用的 `mfa_users`、`mfa_pending`，和 `accounting` |
| `migrations/2_demo_data.sql` | 跳板機帳號 ops1，驗證碼金鑰 `JBSWY3DPEHPK3PXP` |

跳板機的腳本是目錄裡的 `two-step-totp.lua`，內容和 repo 的範例腳本 `examples/two-step-totp.lua` 相同。計費腳本 `accounting.lua` 和[第 2 章](02-quick-start.md#accountinglua-做了什麼)說明的相同。

## VPN：LDAP 串接

```toml
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
```

- **流程：** 用服務帳號 bind，以 `filter` 找使用者（`{user}` 會跳脫），再用使用者的 DN 和密碼 bind 一次驗證密碼。找不到就略過（交給下一個處理器），找到多筆就拒絕。
- **`urls` 依序嘗試，** 只有連不上或逾時才換下一台。一台回錯誤（例如服務帳號密碼錯）會直接拒絕，不換台，所以服務帳號的問題會立刻浮現。
- **`filter` 排除停用帳號：** 上面是 AD 的寫法。OpenLDAP 可以用 `(&(uid={user})(!(pwdAccountLockedTime=*)))`，依目錄的設定而定，請先在自己的目錄上確認。
- **`required_groups`：** 不在群組裡的人和密碼錯誤一樣被拒絕。預設看使用者的 `memberOf`；目錄沒有 `memberOf` 時改用 `group_base_dn` 和 `group_filter` 搜尋群組，見 [usage 3.5 節](../usage.md#35-ldap-處理器)。
- **`ldaps://` 或 StartTLS。** `ldap://` 會自動用 StartTLS 升級；目錄憑證不是系統信任的 CA 簽的，設 `ca_file`。明文 LDAP 要另外設 `allow_plaintext = true`，不建議。
- **`password_check = "bind"` 只支援 PAP。** VPN 閘道要用 PAP 送密碼（RADIUS 會加密 `User-Password`）。要支援 CHAP、MS-CHAPv2 或 PEAP，改用 `password_check = "attribute"` 讀密碼屬性，前提是目錄存的是明文或 NT hash。

```toml
[[auth.chains.vpn.handlers.group_reply]]
group = "CN=it-staff,OU=Groups,DC=corp,DC=example,DC=com"
attributes = { "Filter-Id" = "vpn-it" }

[[auth.chains.vpn.handlers.group_reply]]
group = "CN=contractors,OU=Groups,DC=corp,DC=example,DC=com"
attributes = { "Filter-Id" = "vpn-contractor", "Session-Timeout" = "28800" }
```

- 使用者屬於哪個群組，就帶回那組屬性。**VPN 閘道上要有同名的 filter 或 group policy，** 屬性名稱也依閘道而定（有些用 `Class`，有些用廠商屬性）。
- `reply_attributes` 可以把使用者自己的 LDAP 屬性變成 RADIUS 屬性，例如每個人固定的 IP。

```toml
[auth.chains.vpn.username]
strip_realm = "both"
lowercase = true
```

使用者可能輸入 `CORP\alice` 或 `alice@corp.example.com`，查目錄時都變成 `alice`。具名串接不會繼承 `[auth.username]`，要寫在自己的串接底下。

### 測試 VPN 串接

範例的 LDAP 位址是虛構的，要換成自己的目錄才能測：

```bash
radtest alice 'her-password' 127.0.0.1 0 vpn-gw-shared-secret-change-me
```

（本機測試時把 `vpn-gw` 的 `address` 改成 `127.0.0.1`。）認證紀錄的 `handler` 是 `ldap`。紀錄不分拒絕的原因（密碼錯、不在群組都一樣）。目錄出問題時會另外寫一行：連不上或逾時是 WARN 的 `auth ldap handler: <URL>: <錯誤>`（會改試下一台）；目錄回了錯誤，例如服務帳號密碼錯，是 ERROR 的 `auth ldap handler: <URL>: <錯誤>; rejecting`。沒有這類訊息又分不清楚時，先用 `ldapsearch` 以服務帳號查一次這個使用者和他的 `memberOf`。

## 跳板機：密碼加 TOTP

```toml
[auth.chains.mfa]
policy = "first_match"

[[auth.chains.mfa.handlers]]
type = "lua"
script = "two-step-totp.lua"
```

流程：

```
跳板機 ── Access-Request（帳號、密碼）──────────► rradius 比對密碼
       ◄─ Access-Challenge（State、"Enter your code"）
使用者輸入驗證碼
跳板機 ── Access-Request（帳號、驗證碼、State）──► rradius 驗證 TOTP
       ◄─ Access-Accept
```

- **`State` 只能用一次，60 秒內有效。** 存在 `mfa_pending`，第二步用 `DELETE` 取得，兩個請求不可能用同一個挑戰。
- **同一個驗證碼只能用一次：** 記下用過的時間步，只接受更新的。
- **猜錯 5 次鎖 5 分鐘。** 只有知道密碼的人能觸發鎖定；密碼本身的猜測次數沒有限制，要靠跳板機或防火牆限制。
- **兩步可以落在不同節點，** 範例的寫入都經過叢集，不受影響。

### 測試

本機測試時把 `jump-host` 的 `address` 改成 `127.0.0.1`。`radtest` 不會處理 Access-Challenge，所以分兩次送：

```bash
radtest ops1 ops1-password 127.0.0.1 0 jump-host-shared-secret-change-me
# 收到 Access-Challenge，記下 State 的值（0x 開頭）

oathtool --totp -b JBSWY3DPEHPK3PXP      # 產生現在的驗證碼
echo 'User-Name=ops1,User-Password=<驗證碼>,State=<State 的值>' \
  | radclient 127.0.0.1:1812 auth jump-host-shared-secret-change-me
```

第二次收到 Access-Accept。同一個 `State` 再送一次會被拒絕。

### 用在 SSH 跳板機

pam_radius 會把 Access-Challenge 的 `Reply-Message` 顯示給使用者，再把輸入送回來，所以 SSH 登入時會先問密碼、再問驗證碼。`/etc/pam_radius.conf` 寫 rradius 的位址和 `jump-host.secret` 的內容，逾時建議 5 秒以上。

### 用手機推播代替驗證碼

不想輸入驗證碼時，可以改用推播：密碼通過後，手機跳出通知，使用者按「核准」才登入。rradius 不內建推播（各家服務的介面和費用不同），repo 的 `examples/push-approval.lua` 示範怎麼用 `http_request` 呼叫推播服務：串接改成 `all_accept`，`two-step-totp.lua` 換成比對密碼的處理器，後面接這份腳本，再設定 `[lua.http]` 允許服務的主機（[核心文件 4.4 節〈推播核准範例〉](../usage.md#44-回傳值)）。等使用者按核准要時間，所以 pam_radius 的逾時要比腳本的 `WAIT_SECONDS` 長，例如 35 秒；同時等待的人也會佔用 `max_auth_inflight` 的名額。

### 換成自己的帳號

- `two-step-totp.lua` 的密碼存在 `mfa_users.password`，**以明文比對。** 要用雜湊或改用 LDAP 驗證密碼，就要改寫腳本：例如串接改成 `all_accept`，前面放 LDAP 處理器比對密碼，腳本只做驗證碼。這時第二步的「密碼」是驗證碼，LDAP 會拒絕，所以腳本要在有 `State` 時接手，寫法較複雜，建議先在測試環境確認。
- 驗證碼金鑰用 Base32，每人一把，產生後以 QR code（`otpauth://totp/...`）給使用者掃描。
- rradius-enterprise 有內建的雙因素（`[mfa]`）：client 加一行 `mfa = "challenge"`，密碼照樣由 LDAP 或資料表比對，驗證碼由程式處理，不必寫這份腳本，也支援 TACACS+ 和 HOTP 硬體 token。

## 上線前

- `ldap.secret` 是目錄的服務帳號密碼，權限設成只有執行 rradius 的帳號讀得到。服務帳號只要能搜尋使用者和群組，不需要寫入權限。
- `required_groups`、`group_reply` 的 DN 要和目錄裡的寫法一致，先用 `ldapsearch` 確認使用者的 `memberOf`。`memberOf` 只有直接的成員關係；AD 要算巢狀群組時，改用 `group_base_dn` 加 `group_filter = "(member:1.2.840.113556.1.4.1941:={dn})"`（這個寫法還沒在 AD 上測過）。
- LDAP 連線會保留重用，每個 URL 最多 `pool_size` 條（預設 4）。登入量大時調高。
- 外包人員的 `Session-Timeout` 要閘道支援才有效。
