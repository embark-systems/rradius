# 情境實作：組織網路存取

> 本章讓你做到：
> - 讓印表機、IP 電話這類不會 802.1X 的設備用 MAC 位址登入，放進各自的 VLAN
> - 讓 VPN 閘道用公司目錄的帳號與群組決定誰能連、套用哪一組規則
> - 替跳板機加上「密碼之後再輸入 TOTP 驗證碼」的第二因素登入
> - 帳號放在別的系統時，每次登入用 HTTP API 去問它

前置條件：讀過[第 3 章](03-quick-start.md)與[第 14 章](14-isp-recipes.md)的「準備實驗目錄」；知道[第 7 章](07-users-and-chains.md)的驗證鏈。接 LDAP 與 Active Directory 的細節在[第 8 章](08-ldap-and-ad.md)，腳本的 HTTP 與 TOTP 函式在[第 10 章](10-lua-policies.md)。

本章小節：

- [準備實驗目錄](#準備實驗目錄)
- [有線網路的 MAC 認證：wired-mab](#有線網路的-mac-認證wired-mab)
- [遠端存取：remote-access](#遠端存取remote-access)
- [帳號在外部系統：external-api](#帳號在外部系統external-api)
- [單檔範例](#單檔範例)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

## 準備實驗目錄

本章的三個情境在原始碼的 `rradius-server/examples/scenarios/` 底下，複製、改字典路徑、換掉示範密碼的方式和[第 14 章](14-isp-recipes.md)相同：

```console
$ cp -R rradius-server/examples/scenarios/wired-mab ~/rradius-lab/
$ sed -i.bak "s#^dictionary = .*#dictionary = \"$PWD/share/dictionary\"#" ~/rradius-lab/wired-mab/rradius.toml
$ cd ~/rradius-lab/wired-mab
$ for f in *.secret; do openssl rand -hex 32 > $f; done; chmod 600 *.secret
$ rradiusd check --config rradius.toml
configuration ok
```

三個情境的用戶端位址都是真實網路的網段（交換器、VPN 閘道、熱點），`rradius-bench` 卻是從 `127.0.0.1` 送請求。實驗時只在實驗目錄裡把要測的那個用戶端的 `address` 改成 `"127.0.0.1"`，不然請求會被當成未知用戶端丟掉，`rradius-bench` 只看到 `timeouts 1`。要看回覆屬性，照第 14 章加上 `[log.trace]`，`clients` 寫這個情境的用戶端名稱。

## 有線網路的 MAC 認證：wired-mab

### 目標

印表機、IP 電話、監視器不會 802.1X。交換器改用 MAC 認證（MAB）：把設備的 MAC 位址當帳號送來問。認得的設備放進各自的 VLAN，不認得的拒絕。

### 資料表

```sql
-- migrations/1_schema.sql
CREATE TABLE mac_devices (
  mac TEXT PRIMARY KEY,
  vlan TEXT NOT NULL,
  note TEXT
);
```

`mac` 一律存 12 個小寫十六進位字元，不管交換器怎麼寫（`00-11-22-AA-BB-CC`、`00:11:22:aa:bb:cc`、`0011.22aa.bbcc` 都會被正規化成 `001122aabbcc`）。`note` 給人看，腳本不讀。示範資料有一台印表機（VLAN 30）與一支會議室電話（VLAN 40）。

### 設定重點

```toml
# rradius.toml（節錄，其他設定略）
[lua]
script = "mac-auth.lua"

[[radius.clients]]
name = "access-switches"
address = "10.40.0.0/24"
secret_file = "switches.secret"
```

- 沒有 `[auth]`，認證與計費都交給 `mac-auth.lua`，計費一律回覆。
- 整個管理網段的交換器共用一個用戶端（第 5 章）。

### 腳本重點

`mac-auth.lua`（也就是 `rradius-server/examples/mac-auth.lua`）：

1. 先擋掉非 RADIUS 的協定：它是最上層腳本，同一台伺服器若也開了 `[tacacs]`，一律接受會授權每個指令。
2. 把 `Calling-Station-Id` 與 `User-Name` 都正規化成 12 個十六進位字元，兩者必須相同。`User-Name` 可以是設備自己說的任何東西，`Calling-Station-Id` 才是交換器看到的那台設備。
3. 查 `mac_devices`，找到就回三個屬性，把設備放進 VLAN：

   ```lua
   set_attr('Tunnel-Type:1', 'VLAN')
   set_attr('Tunnel-Medium-Type:1', 'IEEE-802')
   set_attr('Tunnel-Private-Group-Id:1', rows[1].vlan)
   ```

   `:1` 是 tag（RFC 2868），三個屬性用同一個 tag，表示它們描述同一條通道。

腳本不檢查密碼：交換器送什麼密碼都一樣，只看位址。所以 PAP 與 CHAP 的 MAB 都能用。

> **警告：** MAC 位址不是祕密，任何能從設備標籤或線路上讀到它的人都能冒用。把這些設備放在只能到達它們需要的服務的 VLAN。

**不認得的設備怎麼辦。** 範例腳本對不認得的位址回 reject，之後由交換器決定那個 port 的去向（放進訪客 VLAN、維持隔離，或不通電）。這是交換器的設定，不在 rradius，各家的指令不同。

> **未實機驗證：** 交換器端「RADIUS 拒絕之後放進訪客或隔離 VLAN」的設定沒有在真的交換器上試過。

想讓 rradius 直接把不認得的設備放進訪客 VLAN，改寫腳本，查不到時不回 reject，改回 accept 並設訪客 VLAN：

```lua
-- mac-auth.lua（改寫的部分）
local GUEST_VLAN = '99'
local rows = sql_query('SELECT vlan FROM mac_devices WHERE mac = ?', {mac})
if #rows ~= 1 then
  set_attr('Tunnel-Type:1', 'VLAN')
  set_attr('Tunnel-Medium-Type:1', 'IEEE-802')
  set_attr('Tunnel-Private-Group-Id:1', GUEST_VLAN)
  return 'accept'
end
```

已驗證：把範例改成這樣，表裡沒有的 `00-ff-ff-ff-ff-ff`（`User-Name` 與 `Calling-Station-Id` 相同）得到 accept，追蹤行的 `Tunnel-Private-Group-Id = 0x013939`（tag 1 加文字 `99`）；認得的印表機照舊是 VLAN 30。`User-Name` 與 `Calling-Station-Id` 不同的請求仍然是 reject。

> **警告：** 這樣改之後，任何人把電腦插上網路線都能進訪客 VLAN。先確認訪客 VLAN 只到得了網際網路，到不了內部服務。

同一台交換器同時有「用帳號密碼登入的使用者」與 MAB 設備時，它們是同一個用戶端，不能靠用戶端分開。做法是把 `mac-auth.lua` 當驗證鏈裡的 lua 處理器，放在驗密碼的處理器之後，用 `first_match`：

```toml
# rradius.toml（節錄，其他設定略）
[auth]
policy = "first_match"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ?"

[[auth.handlers]]
type = "lua"
script = "mac-auth.lua"
```

sql 處理器認得的帳號由它決定，密碼錯就是 reject，不會再往下；它不認得的名稱才交給 `mac-auth.lua` 比對位址。EAP 的 PEAP 或 TTLS 也走這條鏈時，要知道 lua 處理器回 accept 不算證明了密碼，必須有 sql 或 ldap 處理器真的比對過，所以 MAB 的規則不會讓 PEAP 不用密碼就通過（見[第 11 章〈通道裡發生什麼〉](11-eap.md#通道裡發生什麼)）。實測（`users` 表有 `alice`，`mac_devices` 用 wired-mab 的示範資料）：`alice` 密碼對是 accept、密碼錯是 reject（帶了印表機的 `Calling-Station-Id` 也一樣），印表機的位址是 accept 並回 VLAN 30。

### 怎麼測與注意事項

已驗證。先把 `access-switches` 的 `address` 改成 `"127.0.0.1"`。帳號、密碼都寫 MAC 位址，再用 `--attr 31=text:…` 帶 `Calling-Station-Id`：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat switches.secret)" \
    --user 00-11-22-AA-BB-CC --password 00-11-22-AA-BB-CC \
    --attr 31=text:00-11-22-AA-BB-CC --load pap,concurrency=1 --requests 1
…
  pap: 1430/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

各種情況的結果：

| 請求 | 結果 |
| --- | --- |
| 印表機，`User-Name` 與 `Calling-Station-Id` 都是 `00-11-22-AA-BB-CC` | accept，VLAN 30 |
| 電話，`User-Name` 是 `00a0c9123456`、`Calling-Station-Id` 是 `00:a0:c9:12:34:56` | accept，VLAN 40（寫法不同，正規化後相同） |
| `User-Name` 是 `00a0.c912.3456`（點分寫法），`Calling-Station-Id` 是 `00:a0:c9:12:34:56` | accept，VLAN 40 |
| 用 `--load chap` 送印表機的位址 | accept |
| `User-Name` 是印表機，`Calling-Station-Id` 是別的位址 | reject |
| 沒有 `Calling-Station-Id` | reject |
| 表裡沒有的位址 | reject |

追蹤行裡，回覆的三個屬性顯示成這樣：

```text
… rradius_trace: response client="access-switches" source=127.0.0.1 user="00-11-22-AA-BB-CC" code=2 attributes="Message-Authenticator = <hidden>, Tunnel-Type = 16777229, Tunnel-Medium-Type = 16777222, Tunnel-Private-Group-Id = 0x013330"
```

追蹤顯示的是帶 tag 的原始值：`16777229` 是十六進位的 `0x0100000d`，最前面的 `01` 是 tag 1，`0d`（13）是 `VLAN`；`16777222` 是 tag 1 加 `IEEE-802`（6）；`0x013330` 是 tag 1 加文字 `30`。

注意事項：

- 腳本每次登入都查表，所以表裡多一列設備不用 reload。目前要用新的 migration 檔加資料，例如 `3_add_cameras.sql`：

  ```sql
  -- migrations/3_add_cameras.sql
  INSERT INTO mac_devices (mac, vlan, note) VALUES ('0011223344aa', '50', '大門監視器');
  ```

  放進 `migrations/` 後重新啟動伺服器，migration 才會套用（叢集要逐台重啟）。已驗證：加了這個檔、重啟之後，`User-Name` 與 `Calling-Station-Id` 是 `0011.2233.44aa`、`00:11:22:33:44:aa` 的請求得到 VLAN 50。設備多、變動頻繁時，一台台寫成 migration 很不方便，要另外找管理這張表的流程（例如外部系統匯入）。
- 認證紀錄的 `chain="script" handler="script"` 表示這個決定是最上層腳本做的。

## 遠端存取：remote-access

### 目標

兩種遠端登入放在同一台伺服器：

- VPN 閘道：員工用公司目錄（LDAP 或 Active Directory）的帳號登入，只有 `vpn-users` 群組的人能連，依群組回不同的存取規則。
- 跳板機：密碼之後回 Access-Challenge，要求輸入手機驗證器 App 上的 TOTP 驗證碼。

兩台設備是兩個用戶端，各自指定一條具名的驗證鏈（`auth`）。計費兩者共用 `accounting.lua`，寫法同 quick-start。

### 資料表

VPN 的帳號在目錄裡，這裡不存。跳板機用兩個表：

| 表 | 內容 |
| --- | --- |
| `mfa_users` | `password`（帶前綴的儲存密碼，例如 `{crypt}$6$…` 或 `{argon2}$…`）、`totp_secret`（Base32，和使用者手機上的驗證器相同）、`last_step`（上一次用過的時間步）、`failed_attempts`、`locked_until` |
| `mfa_pending` | 等待驗證碼的 challenge：`state`（隨機值）、`username`、`expires` |

另外還有計費用的 `accounting`。示範帳號 `ops1` 的密碼是 `ops1-password`（存的是 SHA-512 crypt 雜湊），TOTP 金鑰是常見的示範值 `JBSWY3DPEHPK3PXP`。

> **警告：** `JBSWY3DPEHPK3PXP` 是公開的示範金鑰，任何人都能算出它的驗證碼。正式環境每個使用者要有自己隨機產生的金鑰。

### 設定重點

```toml
# rradius.toml（節錄，其他設定略）
[lua]
script = "accounting.lua"

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

[auth.chains.mfa]
policy = "first_match"

[[auth.chains.mfa.handlers]]
type = "lua"
script = "two-step-totp.lua"

[[radius.clients]]
name = "vpn-gw"
address = "10.60.0.1"
secret_file = "vpn-gw.secret"
auth = "vpn"

[[radius.clients]]
name = "jump-host"
address = "10.60.0.20"
secret_file = "jump-host.secret"
auth = "mfa"
```

設定的組成：

| 設定 | 預設值 | reload | 這裡的用法 |
| --- | --- | --- | --- |
| `auth.chains.<name>` | 無 | 可以 | 兩條具名驗證鏈，欄位和 `[auth]` 相同 |
| `radius.clients[].auth` | 無 | 可以 | 用戶端用哪一條具名驗證鏈 |
| `auth.chains.<name>.username.strip_realm` | `none` | 可以 | `both`：使用者輸入 `CORP\alice` 或 `alice@corp.example.com` 都變成 `alice` |
| `auth.chains.<name>.username.lowercase` | `false` | 可以 | 轉小寫 |
| ldap 處理器的 `password_check` | `bind` | 可以 | 先用服務帳號找到使用者，再以使用者的身分 bind 驗證密碼 |
| ldap 處理器的 `required_groups` | 不要求群組 | 可以 | 屬於其中一個群組才接受 |
| ldap 處理器的 `group_reply` | 無 | 可以 | 屬於某群組時加上的回覆屬性 |

各鍵的完整說明見[附錄 A](90-config-reference.md)；ldap 處理器的其他鍵（`ca_file`、逾時、連線數等）見[第 8 章](08-ldap-and-ad.md)。

- `filter` 的後半段讓 AD 裡停用的帳號（`userAccountControl` 第 2 位元）找不到，等同不存在。OpenLDAP 的寫法（`pwdAccountLockedTime`）見[第 8 章](08-ldap-and-ad.md)〈用目錄帳號登入〉，要先在自己的目錄上確認。
- `required_groups` 預設看 `memberOf`，只算直接的成員。AD 要算巢狀群組，改用 `group_base_dn` 加 `group_filter = "(member:1.2.840.113556.1.4.1941:={dn})"`，見[第 8 章](08-ldap-and-ad.md)〈依群組決定能不能登入〉（這個寫法沒有在 AD 上測過）。
- `password_check = "bind"` 要拿到明文密碼，所以 VPN 閘道要用 PAP 送密碼。
- `group_reply` 的 `Filter-Id` 要對應 VPN 閘道上同名的規則（filter 或 group policy）。
- 目錄伺服器的憑證不是系統信任的 CA 簽的時候，加 `ca_file`。

兩個用戶端都指定了驗證鏈，所以認證不會來到 `[lua]` 的 `accounting.lua`，它只處理計費。

### 腳本重點

`two-step-totp.lua`（也就是 `rradius-server/examples/two-step-totp.lua`）分兩步：

1. 請求沒有 `State`：用 `password_verify` 比對 `mfa_users.password`，和驗證鏈比對儲存密碼的方式相同。對了就用 `random_token()` 產生一個隨機值，寫進 `mfa_pending`（60 秒有效），以 `State` 與 `Reply-Message = Enter your code` 回 `challenge`。
2. 請求帶著 `State`：用 `DELETE … WHERE state = ? AND username = ? AND expires >= ?` 認領這個 challenge，刪到 1 列才算數，所以一個 challenge 只能用一次。接著用 `totp_verify` 檢查驗證碼：6 位數、30 秒一步，前後各容許一步。最後只在這一步比 `last_step` 新時才更新，所以同一個驗證碼不能用兩次。

連續 5 次驗證碼錯誤，帳號鎖 5 分鐘；鎖定寫在叢集資料庫裡，每個節點都看得到。challenge 的回答可能送到另一個節點，那個節點的副本可能稍舊。

> **注意：** 腳本只限制猜驗證碼的次數，不限制猜密碼：如果猜密碼也會鎖帳號，任何知道帳號名稱的人都能故意把它鎖住。自己驗證密碼的腳本不受 `[auth.lockout]` 保護；要連密碼的猜測一起限制，改成在驗證鏈裡驗密碼（`[auth.lockout]` 會計算失敗次數，第 7 章），只讓腳本處理驗證碼。

> **企業版：** 企業版內建 TOTP／HOTP 第二因素，不必自己寫這個腳本。

換成自己的帳號時，驗證碼金鑰用 Base32、每人一把，產生之後以 QR code（`otpauth://totp/…`）給使用者掃描；示範金鑰換掉之後，QR code 與 `otpauth` 網址也要跟著重新產生。

### 怎麼測與注意事項

**跳板機：已驗證。** 把實驗目錄裡 `jump-host` 的 `address` 改成 `"127.0.0.1"`，送第一步：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat jump-host.secret)" \
    --user ops1 --password ops1-password --load pap,concurrency=1 --requests 1
…
  pap: 614/s, sent 1, accept 0, reject 0, challenge 1, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

`challenge 1` 就是腳本回的 Access-Challenge；密碼錯時是 `reject 1`。`rradius-bench` 不會回答 challenge，所以第二步要用會處理 Access-Challenge 的用戶端，例如 FreeRADIUS 的 `radclient`，或真的跳板機。用 `radclient` 時，驗證碼用 `oathtool` 算（和示範金鑰 `JBSWY3DPEHPK3PXP` 對應，SHA-1、6 位數、30 秒一步，與程式碼的 `totp_verify` 相同），連同第一步收到的 `State` 值送出：

```console
$ oathtool --totp -b JBSWY3DPEHPK3PXP
$ echo 'User-Name=ops1,User-Password=<驗證碼>,State=<State 的值>,Message-Authenticator=0x00' \
    | radclient 127.0.0.1:1812 auth "$(cat jump-host.secret)"
```

> **未實機驗證：** `oathtool` 與 `radclient` 的這兩行沒有在本版跑過；演算法的部分對照了 `rradius-core/src/totp.rs`。

真的跳板機：輸入密碼之後，它會顯示 `Enter your code`，把驗證碼當成下一次的密碼、連同 `State` 送回來。SSH 跳板機用 pam_radius 時，`/etc/pam_radius.conf` 寫一行 rradius 的位址、共用密碼（`jump-host.secret` 的內容）與逾時，逾時建議 5 秒以上；用推播時要更長，見〈單檔範例〉。

> **未實機驗證：** pam_radius 的設定沒有在真的跳板機上跑過。

本章用一個會處理 challenge 的測試用戶端驗證過第二步：正確的驗證碼得到 Access-Accept，同一個 `State` 再送一次得到 Access-Reject。伺服器的認證紀錄：

```text
… rradius_auth: authentication protocol="radius" client="jump-host" source=127.0.0.1 user="ops1" account="ops1" chain="mfa" handler="lua" decision="challenge"
… rradius_auth: authentication protocol="radius" client="jump-host" source=127.0.0.1 user="ops1" account="ops1" chain="mfa" handler="lua" decision="accept"
… rradius_auth: authentication protocol="radius" client="jump-host" source=127.0.0.1 user="ops1" account="ops1" chain="mfa" handler="lua" decision="reject"
```

`chain="mfa"` 表示這個請求走的是 `jump-host` 指定的驗證鏈。

**VPN 閘道：**

> **未實機驗證：** VPN 那條驗證鏈要有真的目錄伺服器，本章只用 `check` 確認設定有效，並確認連不到目錄時的行為（見下）。群組與回覆屬性的實際結果沒有在目錄上跑過。

把 `vpn-gw` 的 `address` 改成 `"127.0.0.1"`，同時把 `jump-host` 改回原本的位址：兩個用戶端都是 `127.0.0.1` 時，`check` 會得到 `radius clients: duplicate client network '127.0.0.1/32'`。在目錄不存在的情況下登入，`rradius-bench` 等到逾時，伺服器日誌寫：

```text
… WARN rradius_core::ldap_handler: auth ldap handler: ldaps://dc1.corp.example.com: I/O error: failed to lookup address information: …
… WARN rradius_core::ldap_handler: auth ldap handler: ldaps://dc2.corp.example.com: I/O error: failed to lookup address information: …
… ERROR rradius_core::ldap_handler: auth ldap handler: no server reachable; rejecting
… WARN rradius_core::lua_engine: RADIUS Access-Request from client vpn-gw not answered: the authentication backend could not be used
```

目錄連不上時伺服器不回應，而不是回 Access-Reject：NAS 會重送或改問下一台 RADIUS 伺服器，使用者不會因為目錄暫時故障而被當成密碼錯誤。DNS 查不到時，錯誤訊息的後半段依作業系統而不同。

注意事項：

- 這個情境沒有 `[sessions]`，`session list` 會得到 `sessions.list is not set in rradius.toml`。要用的話，照 quick-start 的寫法加上（第 13 章）。
- 使用者輸入驗證碼的時間在兩個請求之間，不佔 NAS 的 RADIUS 逾時；但 challenge 只有 60 秒有效。

## 帳號在外部系統：external-api

### 目標

會員系統或帳務系統已經有帳號和密碼，rradius 不存帳號，每次登入用 HTTPS 問它的 API。API 說可以，就回 API 給的方案名稱當 `Filter-Id`。計費照樣寫進本機的資料表。

### 資料表

只有計費用的 `accounting`，寫法同 quick-start。

### 設定重點

```toml
# rradius.toml（節錄，其他設定略）
[lua]
script = "accounting.lua"

[lua.http]
allowed_hosts = ["members.example.com"]
timeout_ms = 1500
max_concurrent = 64

[auth]
policy = "first_match"

[[auth.handlers]]
type = "lua"
script = "member-api.lua"
verifies_password = true
```

| 設定 | 預設值 | reload | 這裡的用法 |
| --- | --- | --- | --- |
| `lua.http.allowed_hosts` | 必填 | 可以 | 腳本只能呼叫這裡列出的主機。只寫主機名稱時，只允許 HTTPS 的 443 埠；`host:port` 允許那個埠；寫成 `http://host:port` 才允許明文 HTTP |
| `lua.http.timeout_ms` | `2000` | 可以 | 一次呼叫從連線到讀完回應的時限。要比 NAS 的重送時間短 |
| `lua.http.max_concurrent` | `64` | 可以 | 每台主機同時進行的呼叫數，超過的請求立刻失敗 |
| `auth.handlers[].verifies_password` | `false` | 可以 | 告訴驗證鏈這個腳本真的比對過密碼 |

- 沒有 `[lua.http]` 時，腳本裡沒有 `http_request`。
- `verifies_password = true` 讓驗證鏈可以只有這一個 lua 處理器，它的接受也算證明了密碼，EAP-TTLS 的內層 PAP 才會放行（第 11 章）。只在腳本真的驗證了密碼時才設。
- 沒有列在 `allowed_hosts` 的網址，呼叫會被拒絕，指標 `rradius_script_http_requests_total{result="denied"}` 加一。

### 腳本重點

`member-api.lua` 假設的 API（請換成你的系統實際提供的）：

- `POST https://members.example.com/v1/radius/login`，標頭 `Authorization: Bearer <API 金鑰>`，內容 `username=<帳號>&password=<密碼>`（`application/x-www-form-urlencoded`）。
- 200：帳號密碼正確，可以上網；回應標頭 `X-Plan` 是方案名稱（選填）。
- 401、403：帳號密碼不對，或不能上網。

腳本沒有 JSON 函式，所以請 API 用狀態碼和標頭回答。腳本的重點：

- 只有 PAP 帶得出明文密碼。CHAP、MS-CHAPv2 的請求沒有 `User-Password`，腳本直接拒絕，不問 API。
- 帳號與密碼來自使用者，放進內容之前一律用 `url_encode` 編碼。
- API 沒有回答（逾時、連不上、太忙）時拒絕，並寫一行日誌；不要把密碼寫進日誌。
- `X-Plan` 只收看起來像名稱的值（英數字、底線、連字號），才放進 `Filter-Id`；另外回 `Session-Timeout = 14400`。
- API 金鑰寫在腳本裡（`API_KEY`），這個檔案只讓執行 rradius 的帳號讀得到。

### 怎麼測與注意事項

已驗證，用本機的假 API。實驗時把實驗目錄裡的 API 換成本機的明文 HTTP：

```toml
# rradius.toml（實驗目錄裡改）
[lua.http]
allowed_hosts = ["http://127.0.0.1:8080"]
timeout_ms = 1500
max_concurrent = 64
```

`member-api.lua` 的 `API` 改成 `'http://127.0.0.1:8080/v1/radius/login'`，`hotspot` 的 `address` 改成 `"127.0.0.1"`。假 API 用 Python 寫，只認 `alice`／`alice-password`，回 `X-Plan: gold`。

> **注意：** `member_api.py` **不是 rradius 提供的工具**，僅供示範。它只用 Python 3 的標準函式庫，存成檔案就能執行；不要拿它當正式的服務。


```python
# member_api.py
import http.server, urllib.parse

class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get('Content-Length', 0))
        form = urllib.parse.parse_qs(self.rfile.read(length).decode())
        ok = (self.headers.get('Authorization') == 'Bearer member-api-key-change-me'
              and form.get('username') == ['alice'] and form.get('password') == ['alice-password'])
        self.send_response(200 if ok else 401)
        if ok:
            self.send_header('X-Plan', 'gold')
        self.send_header('Content-Length', '0')
        self.end_headers()

http.server.HTTPServer(('127.0.0.1', 8080), Handler).serve_forever()
```

用 `python3 member_api.py` 在另一個終端機啟動它，再啟動 rradius。結果：

| 請求 | 結果 |
| --- | --- |
| `alice`／`alice-password`，PAP | accept，`Filter-Id = gold, Session-Timeout = 14400` |
| 密碼錯 | reject（API 回 401，日誌不記） |
| `alice`，`--load chap` | reject，不問 API |
| `--load session` | 登入 accept，Start 與 Stop 都有 Accounting-Response |
| 停掉假 API 再登入 | reject，日誌寫 `member API gave no answer: the connection was refused` |

`/metrics` 的 `rradius_script_http_requests_total` 分成 `ok`、`timeout`、`error`、`denied`、`too_large`、`busy` 幾種結果，上面這一輪是 `ok` 3、`error` 1。API 回 401 也算 `ok`：那是 HTTP 呼叫成功，只是答案是「不行」。

注意事項：

- API 慢的時候，每個等待中的登入都佔著一個處理中的名額（`radius.max_auth_inflight`，預設 `4096`，第 19 章）。`timeout_ms` 要比 NAS 的重送時間短，NAS 才不會在腳本還在等時又送一次。
- API 故障時這個腳本拒絕所有人，使用者看到的是密碼錯誤。要監看 `rradius_script_http_requests_total` 的 `timeout` 與 `error`（第 17 章），才分得出是 API 的問題。

## 單檔範例

`rradius-server/examples/` 底下與本章有關的三個腳本：

| 檔案 | 說明 | 驗證 |
| --- | --- | --- |
| `mac-auth.lua` | 和 wired-mab 情境的腳本相同 | 已驗證（同 wired-mab） |
| `two-step-totp.lua` | 和 remote-access 情境的腳本相同 | 已驗證（同 remote-access） |
| `push-approval.lua` | 推播核准：密碼之後，使用者在手機上按「核准」 | 只用 `check` 驗證設定 |

`push-approval.lua` 放在 `all_accept` 驗證鏈的最後，前面的處理器驗過密碼之後才會執行：

```toml
# conf.d/push.toml
[auth]
policy = "all_accept"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ?"

[[auth.handlers]]
type = "lua"
script = "push-approval.lua"

[lua.http]
allowed_hosts = ["push.example.com"]
timeout_ms = 7000
```

這份片段放進 `config init` 的底稿（第 4 章）可以通過 `check`。腳本假設的推播服務：

- `POST https://push.example.com/v1/push`，內容 `username=<帳號>`，回 201 與標頭 `X-Push-Id`，表示推播已送到使用者的手機。
- `GET https://push.example.com/v1/push/<id>?wait=<秒數>`，等到使用者回答或時間到，回 200 與標頭 `X-Push-Result`：`approved`、`denied` 或 `pending`。

腳本最多等 `WAIT_SECONDS`（30 秒），每次請求最多讓服務等 `POLL_SECONDS`（5 秒），所以 `timeout_ms` 要比 5 秒長，片段裡寫 7000。

> **未實機驗證：** `push-approval.lua` 只用 `check` 確認設定，沒有接上推播服務跑過。

等待要花時間，要先想好三件事：

- NAS 的 RADIUS 逾時要比 `WAIT_SECONDS` 長，否則使用者還在看手機，NAS 就放棄或重送了。例如 `WAIT_SECONDS` 是 30 秒，pam_radius 伺服器那一行的逾時設 35 秒；VPN 閘道則是它的 RADIUS 伺服器逾時。
- 每個等待中的登入都佔著 `max_auth_inflight` 的一個名額，同時等待的人多時，要調高這個值。
- `lua.max_execution_ms`（預設 `1000`）只算腳本自己執行的時間，不算等待 HTTP 回應的時間。

## 常見錯誤

**症狀：** `rradius-bench` 的結果是 `timeouts 1`，伺服器日誌有 `RADIUS packet from unknown client 127.0.0.1 discarded`。
**原因：** 情境的用戶端位址是真實網段，`127.0.0.1` 不在裡面。`status` 的 `Discarded: unknown client` 會增加。
**解法：** 只在實驗目錄裡把要測的用戶端 `address` 改成 `"127.0.0.1"`。

**症狀：** MAB 一直被拒絕，日誌沒有錯誤。
**原因：** 交換器沒送 `Calling-Station-Id`，或它和 `User-Name` 不是同一個位址。
**解法：** 開 `[log.trace]` 看請求的屬性，對照兩個值。

**症狀：** VPN 登入沒有回應，日誌有 `no server reachable; rejecting` 與 `not answered: the authentication backend could not be used`。
**原因：** 連不到任何一台目錄伺服器：名稱查不到、埠不通，或憑證不被信任。
**解法：** 確認伺服器上能連到 `urls` 列的位址；目錄的憑證不是系統信任的 CA 簽的，加 `ca_file`（第 8 章）。

**症狀：** 跳板機輸入正確的驗證碼還是被拒絕。
**原因：** challenge 超過 60 秒才回答；同一個驗證碼已經用過；伺服器或手機的時間差超過一步（30 秒）；或連續錯了 5 次，帳號鎖了 5 分鐘。
**解法：** 確認兩邊的時鐘同步，等下一個驗證碼再試。

**症狀：** `external-api` 的登入都被拒絕，日誌有 `script http_request refused: https://… is not in lua.http.allowed_hosts`。
**原因：** 腳本呼叫的網址和 `allowed_hosts` 不相符：主機、埠或 http／https 有一個不一樣。
**解法：** `allowed_hosts` 照網址寫：HTTPS 的 443 埠只寫主機，其他埠寫 `host:port`，明文 HTTP 寫 `http://host:port`。

**症狀：** `external-api` 的 CHAP 或 MS-CHAPv2 登入都被拒絕。
**原因：** 這兩種方式不帶明文密碼，腳本沒有東西可以交給 API。
**解法：** NAS 改用 PAP；或改用 EAP-TTLS 內層 PAP（第 11 章），外層有 TLS 保護。

## 相關章節

- [05 RADIUS 服務與 NAS 用戶端](05-radius-clients.md)
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)
- [08 接上 LDAP 與 Active Directory](08-ldap-and-ad.md)
- [10 用 Lua 寫政策](10-lua-policies.md)
- [11 802.1X 與 EAP](11-eap.md)
- [13 計費與 session](13-accounting.md)
- [14 情境實作：寬頻與 ISP](14-isp-recipes.md)
- [附錄 A：設定參考](90-config-reference.md)
- [附錄 D：Lua API 參考](93-lua-reference.md)
