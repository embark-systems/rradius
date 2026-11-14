# 3. 企業 Wi-Fi（802.1X）

**情境：** 公司的 WPA2／WPA3-Enterprise 無線網路。

- 公司配發的筆電裝了公司 CA 簽的憑證，用 **EAP-TLS** 登入，放進設備 VLAN 10。
- 員工自己的手機沒有憑證，用帳號密碼走 **PEAP-MSCHAPv2**，放進員工 VLAN 20。
- 離職的人或遺失的筆電，在資料表停用就不能登入，不必等憑證撤銷。

範例目錄：[`rradius-server/examples/scenarios/enterprise-wifi/`](../../rradius-server/examples/scenarios/enterprise-wifi/)

| 檔案 | 內容 |
| --- | --- |
| `rradius.toml` | 設定檔 |
| `accounting.lua` | 計費腳本 |
| `make-certs.sh` | 產生示範用的 CA、伺服器憑證和一張筆電憑證 |
| `migrations/1_schema.sql` | `accounts`（帳號和設備）、`group_reply`（群組的 VLAN）、`accounting` |
| `migrations/2_demo_data.sql` | 員工 staff1（啟用）、staff2（停用）、筆電 laptop-001 |
| `eapol/peap.conf`、`eapol/tls.conf` | `eapol_test` 的測試設定 |

計費腳本 `accounting.lua` 和[第 2 章](02-quick-start.md#accountinglua-做了什麼)說明的相同。

## 步驟

### 1. 產生憑證

```bash
cd rradius-server/examples/scenarios/enterprise-wifi
./make-certs.sh
```

會在 `certs/` 產生 `ca.pem`、`server.pem`／`server.key`（名稱 `radius.corp.example.com`）和 `device.pem`／`device.key`（名稱 `laptop-001.corp.example.com`）。**只供測試。** 正式環境的伺服器憑證和用戶端憑證要由公司的 CA 簽發，伺服器私鑰不要離開伺服器。

### 2. 改 client 位址並啟動

本機測試時，把 `[[radius.clients]]` 的 `address = "10.30.0.10"` 改成 `127.0.0.1`。

```bash
cd ../../..   # 回到 rradius-server
./target/release/rradius-server server --config examples/scenarios/enterprise-wifi/rradius.toml
```

### 3. 用 eapol_test 測試

`eapol_test` 在 Debian／Ubuntu 的 `eapoltest` 套件裡。在情境目錄執行，設定檔裡的憑證路徑才對得上：

```bash
cd examples/scenarios/enterprise-wifi
eapol_test -c eapol/peap.conf -a 127.0.0.1 -s wlc-shared-secret-change-me
eapol_test -c eapol/tls.conf -a 127.0.0.1 -s wlc-shared-secret-change-me
```

兩個都要印出 `SUCCESS`，Access-Accept 裡有 `Tunnel-Private-Group-Id`（PEAP 是 20，TLS 是 10）。rradius 的認證紀錄：

```
authentication protocol="radius" client="wlc" source=127.0.0.1 user="anonymous@corp.example.com" account="staff1" chain="eap" handler="eap-peap" decision="accept"
authentication protocol="radius" client="wlc" source=127.0.0.1 user="laptop-001.corp.example.com" account="laptop-001.corp.example.com" chain="eap" handler="eap-tls" decision="accept"
```

PEAP 的 `user` 是外層的匿名名稱，`account` 是內層的真正帳號。

這兩份 `eapol_test` 設定是在 Docker 裡用 `eapoltest` 套件實際跑過的；這個情境的自動測試只用 PAP 確認串接和 VLAN，EAP 本身由專案的互通測試涵蓋。[第 8 章](08-wifi-certs.md)的自動測試會走完整段 EAP-TLS。

## 設定說明

```toml
[radius.eap]
cert_file = "certs/server.pem"
key_file = "certs/server.key"
client_ca_file = "certs/ca.pem"
methods = ["tls", "peap"]

[[radius.clients]]
name = "wlc"
address = "10.30.0.10"
secret_file = "wlc.secret"
eap_methods = ["tls", "peap"]
```

- **`client_ca_file` 是簽發用戶端憑證的 CA，** 只信任它，不信任系統的根憑證。
- **`methods` 有兩個以上時，每個 client 都要設 `eap_methods`，** 列表順序就是提供順序。用戶端可以拒絕第一個、改要第二個（EAP Nak），所以只想讓某台控制器收憑證的話，那台就只列 `["tls"]`。
- **憑證通過不等於可以登入。** 握手成功後，憑證上的名稱會交給認證串接決定，所以停用設備只要改資料表。

```toml
[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM accounts WHERE name = ? AND kind = 'user' AND enabled = 1"
certificate_query = "SELECT 1 FROM accounts WHERE name = ? AND kind = 'device' AND enabled = 1"
reply_query = "SELECT r.attribute, r.value FROM accounts a JOIN group_reply r ON r.grp = a.grp WHERE a.name = ?"
```

- **PEAP 用 `user_query`：** MS-CHAPv2 要比對明文或 NT hash，所以密碼存 `{nthash}`（`2_demo_data.sql` 有算法）。存 bcrypt、argon2 的帳號不能走 PEAP，要改用 EAP-TTLS 內層 PAP，見 [usage 2.12 節](../usage.md#212-eap-radiuseap)。
- **EAP-TLS 用 `certificate_query`：** 查得到就接受。設備沒有密碼，所以不能用 `user_query`（密碼欄是 NULL 會被拒絕）。
- **`reply_query` 兩種登入都用，** 依群組回 VLAN 的三個屬性。`Tunnel-Type:1` 的 `:1` 是 tag，三個屬性要用同一個 tag。

```toml
[auth.username]
strip_realm = "suffix"
lowercase = true
```

- 員工可能輸入 `Staff1@corp.example.com`，查帳號時變成 `staff1`。
- **憑證登入不去掉 realm：** 憑證證明的是完整名稱，名稱被改掉的不只大小寫時會拒絕。所以設備存憑證上的完整名稱，而且設備名稱不要有 `@`。

### 憑證上的名稱

- rradius 用憑證的 DNS SAN 和 email SAN 當名稱，兩者都沒有才用 CN。用戶端的 EAP Identity 要符合其中一個（`match_identity = true`，預設）。
- 用戶端把 Identity 填成匿名值時，設 `match_identity = false`，改用憑證的第一個名稱。
- 用腳本判斷憑證欄位（例如 OU）時用 `tls_subject_attrs`，不要對整個主體字串做比對，見 [usage 4.2 節](../usage.md#42-全域變數)。

## 其他登入方式

這一章用的是最常見的兩種。其他的選擇：

| 情況 | 用什麼 | 哪裡 |
| --- | --- | --- |
| 帳號的密碼只有雜湊（bcrypt、argon2、舊系統搬來的），或要讓 LDAP 用 bind 驗證 | EAP-TTLS 內層 PAP：`methods` 加 `"ttls"` | [usage 2.12 節](../usage.md#212-eap-radiuseap)、[第 9 章](09-migrate-users.md#能搬的和不能搬的) |
| 設備什麼都不會（印表機） | MAC 認證 | [第 4 章](04-wired-mab.md) |
| 員工帳號在 Active Directory，要用 PEAP-MSCHAPv2 | `netlogon` 處理器，由 DC 驗證 | 下一節 |

用自己發的 USIM 卡（EAP-AKA'），或讓不會 802.1X 的設備每人一把 Wi-Fi 密碼，需要 rradius-enterprise，見企業版手冊。

## 用 AD 帳號登入

AD 不會給出密碼的 NT hash，所以 PEAP-MSCHAPv2 沒辦法用 `ldap` 處理器對 AD 驗證。`netlogon` 處理器把 MS-CHAPv2 的回應交給網域控制站（DC）驗證，rradius 不必加入網域。細節見 [usage 3.7 節](../usage.md#37-netlogon-處理器active-directory-的-ms-chapv2)。

### 1. 為每台節點建立電腦帳號

每台 rradius 節點用一個自己的電腦帳號。兩台共用時，會互相弄斷對方和 DC 的 secure channel。名稱最多 15 個字元。

在 Windows（需要 RSAT 的 ActiveDirectory 模組）：

```powershell
$pw = Read-Host -AsSecureString "RRADIUS1 的密碼"
New-ADComputer -Name RRADIUS1 -SamAccountName 'RRADIUS1$' -AccountPassword $pw -Enabled $true
```

在 Samba AD DC：

```bash
samba-tool computer create RRADIUS1
samba-tool user setpassword 'RRADIUS1$'
```

Samba 預設不接受 MS-CHAPv2。在每台 DC 的 `/etc/samba/smb.conf` `[global]` 加上下面這行，再重新啟動 samba：

```ini
ntlm auth = mschapv2-and-ntlmv2-only
```

把同一個密碼存到節點上，只給 rradius 讀：

```bash
install -m 600 -o rradius /dev/null /etc/rradius/netlogon-rradius1.secret
read -rs pw && printf '%s' "$pw" > /etc/rradius/netlogon-rradius1.secret
```

### 2. 設定

把 `netlogon` 處理器放在原本的 `sql` 處理器前面。AD 沒有的名稱（例如筆電憑證的名稱）DC 回「沒有這個使用者」，交給 `sql` 處理器；憑證登入 `netlogon` 處理器不處理。

```toml
[[auth.handlers]]
type = "netlogon"
domain = "CORP"                       # NetBIOS 網域名稱
servers = ["dc1.corp.example", "dc2.corp.example"]
computer_name = "RRADIUS1"            # 每台節點不同
machine_password_file = "netlogon-rradius1.secret"
# 只讓 Wi-Fi 群組登入，並依群組給 VLAN：
required_groups = ["S-1-5-21-1004336348-1177238915-682003330-1110"]

[[auth.handlers.group_reply]]
group = "S-1-5-21-1004336348-1177238915-682003330-1110"
attributes = { "Tunnel-Type" = "VLAN", "Tunnel-Medium-Type" = "IEEE-802", "Tunnel-Private-Group-Id" = "20" }

[[auth.handlers]]
type = "sql"
# 原本的設定
```

- 群組用 SID，用 `Get-ADGroup wifi-staff | Select SID` 或 `samba-tool group show wifi-staff` 查。
- 員工輸入 `CORP\staff1` 或 `staff1@corp.example.com` 時，`[auth.username]` 的 `strip_realm = "both"` 讓查帳號用 `staff1`。
- `servers` 要寫 DC 的 DNS 或 NetBIOS 名稱，不能寫 IP 位址，節點要能解析這些名稱。
- 節點和 DC 之間要開 TCP 135（端點對應器）和 DC 的動態 RPC 埠（預設 49152 到 65535）。DC 把 Netlogon 固定在某個埠時，設 `port` 就不必開 135。

### 3. 測試

`rradius-server check` 只檢查設定，不連 DC。把 `eapol/peap.conf` 的 `identity`、`password` 改成 AD 帳號後跑 `eapol_test`，看到 `SUCCESS` 就對了。再試一次錯的密碼，應該被拒絕。`/metrics` 的 `rradius_netlogon_channel_setups_total{result="ok"}` 是 1 以上，`rradius_netlogon_logons_total` 跟著增加。

## 上線前

- **用戶端一定要驗證伺服器憑證。** PEAP 的保護全靠 TLS，用戶端不驗證時，假的基地台能拿到密碼的雜湊。用 MDM 或群組原則把 CA 和伺服器名稱（`radius.corp.example.com`）推到裝置上，這是最常見的部署錯誤。
- **伺服器憑證** 建議帶 `serverAuth` 的 EKU，`make-certs.sh` 有加。
- **撤銷：** 設 `crl_file` 時，憑證鏈上每一層 CA 都要有 CRL，更新後送 SIGHUP。不設的話，停用設備就靠資料表的 `enabled`。做法和取捨見[第 8 章](08-wifi-certs.md)。
- **設備漫遊時每次都重做完整握手。** CA 只提供 OCSP 時的撤銷檢查（`ocsp`），和讓重新連線的設備恢復上一次的 session（`resumption_ms`），需要 rradius-enterprise，見企業版手冊第 8 章。
- **同一次登入要在同一個節點完成。** 多節點時，無線控制器要固定送到同一台（主備），或在負載平衡器依來源固定節點，見[第 7 章](07-ha-cluster.md)。
- **這個功能還沒有在實體基地台和各作業系統上測過，** 只用 `eapol_test` 驗證過。上線前請用實際的控制器和幾種手機、筆電試一次。

## 常見問題

| 現象 | 原因 |
| --- | --- |
| 啟動失敗，提到 `eap_methods` | `methods` 有多個方法，某個 client 沒設 `eap_methods` |
| PEAP 失敗，紀錄 `decision="reject"` | 帳號不存在、停用、密碼不是明文或 `{nthash}` |
| EAP-TLS 握手成功但被拒絕 | `certificate_query` 查不到：名稱和資料表不同，或 `kind`、`enabled` 不對 |
| EAP-TLS 紀錄寫 Identity 不符 | 用戶端的 Identity 和憑證的 SAN／CN 都不同，改用戶端設定或設 `match_identity = false` |
| 有 `UnknownRevocationStatus` | 設了 `crl_file`，但缺少某一層 CA 的 CRL |
| 手機一直跳出「是否信任憑證」 | 裝置上沒有 CA，或伺服器名稱和設定的不同 |
| 記錄 `cannot set up the secure channel as RRADIUS1$`，提到 `STATUS_ACCESS_DENIED` | 密碼檔和 AD 上的電腦帳號密碼不同，或 `computer_name` 打錯。在 AD 重設密碼後更新密碼檔，送 SIGHUP |
| 同上，提到 `STATUS_NO_TRUST_SAM_ACCOUNT` | AD 沒有這個電腦帳號，或 `domain` 不是 NetBIOS 名稱 |
| 記錄 `no domain controller of CORP could answer`，RADIUS 沒有回應 | 連不到 DC：防火牆擋了 135 或動態 RPC 埠，或 DC 名稱解析不到 |
| 兩台節點輪流出現 `STATUS_ACCESS_DENIED` | 兩台用了同一個電腦帳號 |
| Samba DC，密碼正確也被拒，記錄 `STATUS_WRONG_PASSWORD` | `smb.conf` 沒有設 `ntlm auth = mschapv2-and-ntlmv2-only` |
| 啟動或 `check` 失敗，提到 `is an address` | `servers` 寫了 IP 位址，改成 DC 的名稱 |
| AD 帳號被拒，記錄 `not in any of required_groups` | 使用者不在列出的群組，或 SID 抄錯 |
