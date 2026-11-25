# 附錄 E：術語與 RFC 對照

> 本章讓你做到：
> - 查到書中每個術語的寫法、意思，以及它對應的設定鍵或指令
> - 分清楚容易混淆的詞，例如用戶端與使用者、授權群組與 LDAP 群組
> - 查到核心實作了哪些 RFC 與標準，各用在哪個功能
> - 認出程式碼提到、但核心沒有實作的 RFC

前置條件：無。這份附錄可以單獨查閱。

本章小節：

- [怎麼讀這份對照](#怎麼讀這份對照)
- [基本詞](#基本詞)
- [驗證與帳號](#驗證與帳號)
- [計費與 session](#計費與-session)
- [叢集](#叢集)
- [運作與監控](#運作與監控)
- [憑證與安全](#憑證與安全)
- [核心不支援的功能用語](#核心不支援的功能用語)
- [核心實作的 RFC](#核心實作的-rfc)
- [其他標準](#其他標準)
- [程式碼提到、但核心沒有實作的 RFC](#程式碼提到但核心沒有實作的-rfc)
- [rradius-bench 用到的 RFC](#rradius-bench-用到的-rfc)
- [RFC 正式標題](#rfc-正式標題)
- [怎麼重新產生 RFC 清單](#怎麼重新產生-rfc-清單)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

---

## 怎麼讀這份對照

全書用同一套寫法。表格的「書中寫法」是正文用的詞；設定鍵、指令、屬性名稱、RFC 名稱一律保留原文。「不翻譯」的詞在書裡照英文寫，例如 realm、session、leader。

每個詞的「對應」一欄列出它在設定檔或指令裡的樣子，詳細用法見「主章」。

## 基本詞

| 原文 | 書中寫法 | 意思 | 對應 | 主章 |
| --- | --- | --- | --- | --- |
| NAS | NAS | 送 RADIUS 請求給伺服器的設備，例如 AP 控制器、交換器、BNG、VPN 閘道。第一次出現時寫成「NAS（網路存取設備，例如 AP 控制器、交換器、BNG、VPN 閘道）」 | — | [05](05-radius-clients.md) |
| client | 用戶端（client） | 設定裡的一個 `[[radius.clients]]` 或 `[[tacacs.clients]]` 項目，也就是允許送請求的 NAS。一個項目可以是一整個網段，所以一個用戶端不一定是一台設備 | `radius.clients`、`tacacs.clients` | [05](05-radius-clients.md)、[12](12-tacacs.md) |
| user | 使用者 | 登入的人或設備 | `User-Name` | [07](07-users-and-chains.md) |
| account | 帳號 | 資料表裡代表一個使用者的那一列 | `user` 指令、`[accounts]` | [07](07-users-and-chains.md) |
| shared secret | 共用密碼 | 用戶端與伺服器雙方設定的同一個密碼，用來簽章與隱藏密碼欄位 | `secret`、`secret_file` | [05](05-radius-clients.md) |
| authentication／authorization／accounting | 認證／授權／計費 | 確認身分／決定能做什麼／記錄用了多少。三者合稱 AAA | — | [01](01-overview.md) |
| realm | realm | 帳號名稱裡 `@` 後面的部分，例如 `@example.com` | `auth.username.strip_realm` | [07](07-users-and-chains.md) |
| dictionary | 字典 | 屬性名稱、編號與型別的對照檔 | `dictionary` | [06](06-dictionary.md) |
| vendor-specific attribute | 廠商屬性（VSA） | 由廠商自己定義編號的屬性，放在 `Vendor-Specific` 裡 | 字典的 `VENDOR` | [06](06-dictionary.md) |
| reply attribute | 回覆屬性 | Access-Accept 帶回給 NAS 的屬性，例如 `Session-Timeout`、`Filter-Id` | sql 處理器的 `reply_query`、Lua 的 `set_attr` | [07](07-users-and-chains.md)、[10](10-lua-policies.md) |

## 驗證與帳號

| 原文 | 書中寫法 | 意思 | 對應 | 主章 |
| --- | --- | --- | --- | --- |
| auth chain | 驗證鏈 | 依序詢問的一串處理器，決定接受或拒絕一次登入 | `[auth]`、`auth.chains` | [07](07-users-and-chains.md) |
| handler | 處理器 | 驗證鏈裡的一個環節，例如 sql 處理器、ldap 處理器 | `auth.handlers`、`type` | [07](07-users-and-chains.md) |
| policy | `first_match`、`all_accept` | 驗證鏈怎麼合併處理器的結果；原文照寫 | `auth.policy` | [07](07-users-and-chains.md) |
| lockout | 鎖定 | 密碼錯太多次之後，暫時拒絕這個帳號 | `auth.lockout` | [07](07-users-and-chains.md) |
| script | 腳本 | 用 Lua 寫的政策 | `lua.script` | [10](10-lua-policies.md) |
| phase | 階段（phase） | 腳本這一次被叫來處理什麼：`authentication`、`authorization`、`accounting` | Lua 的 `phase` | [10](10-lua-policies.md) |
| enable（TACACS+） | enable | TACACS+ 設備上提升權限的那一步，原文照寫 | `tacacs.enable` | [12](12-tacacs.md) |
| authorize group | 授權群組 | `authorize_groups` 裡的一組 TACACS+ 授權規則，按**設備**分組，不是 LDAP 群組 | `tacacs.authorize_groups` | [12](12-tacacs.md) |

## 計費與 session

| 原文 | 書中寫法 | 意思 | 對應 | 主章 |
| --- | --- | --- | --- | --- |
| session | session | 一個使用者從上線到下線的一段連線，由 Accounting Start 開始、Stop 結束 | `[sessions]`、`session` 指令 | [13](13-accounting.md) |
| accounting spool | 計費 spool | 資料庫暫時不能寫入時，計費請求先寫在磁碟上、等待重送的暫存。第一次出現時要這樣說明 | `radius.accounting_spool` | [13](13-accounting.md) |
| replay | 重送（replay；NAS 的 retransmit 也叫重送，兩者不同） | 把計費 spool 裡的請求再交給腳本處理一次 | `replay_interval_ms`、`replay_window` | [13](13-accounting.md) |

## 叢集

| 原文 | 書中寫法 | 意思 | 對應 | 主章 |
| --- | --- | --- | --- | --- |
| cluster | 叢集 | 共用同一份資料庫的一組 rradius 節點；單機也是一個節點的叢集 | `[cluster]` | [16](16-cluster.md) |
| node | 節點 | 叢集裡的一個 rradius 行程 | `cluster.node_id`、`cluster.nodes` | [16](16-cluster.md) |
| leader／follower | leader／follower | 負責寫入的那個節點，與跟著它複製資料的其他節點。不翻譯 | `status` 的 `Leader` | [16](16-cluster.md) |
| voter／learner | voter／learner | 參與投票選出 leader 的成員，與只複製資料、不投票的成員。不翻譯 | `cluster members` 的輸出 | [16](16-cluster.md) |
| snapshot | snapshot | 叢集把資料庫狀態整份存下來，之後的紀錄從這裡接續 | `cluster.snapshot_every` | [16](16-cluster.md) |
| migration | migration | `migrations_dir` 裡依編號套用的 SQL 檔，用來建表與改表 | `cluster.migrations_dir` | [16](16-cluster.md) |

## 運作與監控

| 原文 | 書中寫法 | 意思 | 對應 | 主章 |
| --- | --- | --- | --- | --- |
| reload | 重新載入（reload） | 不停止伺服器，重新讀設定；送 SIGHUP 觸發 | SIGHUP | [04](04-configuration.md) |
| drain | 排空（drain） | 停止時不再接新請求，等處理中的請求做完 | `shutdown.drain_ms` | [04](04-configuration.md) |
| metrics | 指標 | `/metrics` 提供的 Prometheus 格式數值 | `[metrics]` | [17](17-monitoring.md) |
| readiness／liveness | 就緒／存活 | 可以處理請求／行程還活著；分別對應 `/readyz`、`/livez` | `[metrics]` | [17](17-monitoring.md) |

## 憑證與安全

| 原文 | 書中寫法 | 意思 | 對應 | 主章 |
| --- | --- | --- | --- | --- |
| certificate | 憑證 | X.509 憑證 | `radius.eap.cert_file` | [11](11-eap.md) |
| CA | CA | 簽發憑證的單位，伺服器用它檢查用戶端憑證 | `radius.eap.client_ca_file` | [11](11-eap.md) |
| CRL | CRL（撤銷清單） | CA 公布的已撤銷憑證清單 | `radius.eap.crl_file` | [11](11-eap.md) |
| revocation | 撤銷 | 讓一張還沒過期的憑證失效 | `radius.eap.crl_file` | [11](11-eap.md) |

## 核心不支援的功能用語

下面這些詞會出現在核心的錯誤訊息或設定參考裡，功能本身只有企業版有。核心書只在提到「核心不支援」時使用：

| 原文 | 書中寫法 | 核心遇到時 |
| --- | --- | --- |
| RadSec | RadSec | `radius.tls: not supported by this program (RadSec needs rradius-enterprise)` |
| CoA、Disconnect-Request | CoA、Disconnect-Request | `radius.coa: not supported by this program (CoA needs rradius-enterprise)`；Lua 的 `send_disconnect`、`send_coa` 呼叫時拋出錯誤 |
| OCSP | OCSP | `radius.eap.ocsp: not supported by this program (OCSP needs rradius-enterprise)` |
| proxy | 轉送 | `proxy: not supported by this program (proxying needs rradius-enterprise)`；「代理伺服器」只在講上游時用 |
| upstream | 上游伺服器 | — |
| second factor／MFA | 第二因素 | `mfa: not supported by this program (two-factor login needs rradius-enterprise)`；設定鍵 `mfa` 保留原文 |
| sealed key／seal | 加密存放 | 用另一把主金鑰加密後才寫進資料庫，例如 `aka-seal` 是「產生加密存放的值」。核心沒有用到加密存放的設定或指令，`aka-seal` 得到 `not supported by this program` |

---

## 核心實作的 RFC

這份清單從 `rradius-core/src` 的程式碼註解取出 RFC 編號，再逐一打開註解所在的程式，只留下核心真的有實作的。「程式碼用它做什麼」照註解的內容寫，不是 RFC 的正式標題。

**RADIUS 本身**

| RFC | 程式碼用它做什麼 | 原始碼 | 主章 |
| --- | --- | --- | --- |
| RFC 2865 | RADIUS 認證：`User-Password` 的隱藏與解開；Access-Challenge 不帶前面處理器給的回覆屬性，因為大部分屬性不能放在 Access-Challenge 裡 | `radius.rs`、`auth_chain.rs` | [05](05-radius-clients.md) |
| RFC 2866 | RADIUS 計費：Accounting-Request 的 Request Authenticator | `radius.rs` | [13](13-accounting.md) |
| RFC 2868 | tunnel 屬性的 tag，以及 `Tunnel-Password` 的 salt 加密 | `radius.rs` | [06](06-dictionary.md) |
| RFC 3162 | IPv6 屬性：前綴後面的位元組可以省略 | `radius.rs` | [06](06-dictionary.md) |
| RFC 6929 | extended、long-extended 屬性與 Extended-Vendor-Specific；無效的屬性整個略過 | `radius.rs` | [06](06-dictionary.md) |
| RFC 5080 | 重送偵測：已回答的請求在重複快取裡留多久、怎麼認出重送 | `radius.rs` | [05](05-radius-clients.md) |
| RFC 5997 | Status-Server：不執行腳本、不進重複快取就回答 | `radius.rs` | [05](05-radius-clients.md) |
| RFC 3579 | RADIUS 承載 EAP，以及 Message-Authenticator 的檢查 | `radius.rs`、`eap.rs` | [05](05-radius-clients.md)、[11](11-eap.md) |

**EAP**

| RFC | 程式碼用它做什麼 | 原始碼 | 主章 |
| --- | --- | --- | --- |
| RFC 3748 | EAP 本身，例如過期的回應直接丟掉 | `eap.rs` | [11](11-eap.md) |
| RFC 5216 | EAP-TLS：TLS 1.2 的金鑰導出、長度欄位放在第一個分段 | `eap.rs` | [11](11-eap.md) |
| RFC 9190 | EAP-TLS 使用 TLS 1.3 時的金鑰導出與結束方式 | `eap.rs` | [11](11-eap.md) |
| RFC 5281 | EAP-TTLS：tunnel 裡的 AVP、金鑰導出 | `eap.rs` | [11](11-eap.md) |
| RFC 4514 | 把用戶端憑證的主旨（DN）寫成字串時的跳脫規則 | `eap.rs` | [11](11-eap.md) |

**密碼與挑戰式驗證**

| RFC | 程式碼用它做什麼 | 原始碼 | 主章 |
| --- | --- | --- | --- |
| RFC 2759 | MS-CHAPv2 的回應與 authenticator response，也用在 PEAP 內層 | `mschap.rs`、`eap.rs` | [07](07-users-and-chains.md) |
| RFC 2433 | MS-CHAPv1 的 NT-Response 檢查 | `mschap.rs`、`ldap_handler.rs` | [07](07-users-and-chains.md) |
| RFC 2548 | Microsoft 廠商屬性：MS-CHAP 的請求與 MPPE 金鑰的回覆屬性 | `mschap.rs`、`auth_chain.rs`、`reload.rs` | [07](07-users-and-chains.md) |
| RFC 3079 | MPPE 的 session 金鑰導出 | `mschap.rs` | [07](07-users-and-chains.md) |

**TACACS+、LDAP 與 Lua 函式**

| RFC | 程式碼用它做什麼 | 原始碼 | 主章 |
| --- | --- | --- | --- |
| RFC 8907 | TACACS+：封包格式、授權規則的參數、共用密碼太短時的警告 | `tacacs.rs`、`tacacs_rules.rs`、`reload.rs` | [12](12-tacacs.md) |
| RFC 4513 | LDAP bind：空密碼會變成一定成功的未驗證 bind，所以只拿非空的 PAP 密碼去 bind | `ldap_handler.rs` | [08](08-ldap-and-ad.md) |
| RFC 4515 | LDAP 篩選字串裡 `{user}` 的跳脫 | `ldap_handler.rs` | [08](08-ldap-and-ad.md) |
| RFC 6238 | TOTP，Lua 的 `totp_verify` | `totp.rs` | [10](10-lua-policies.md) |
| RFC 4226 | HOTP，TOTP 的計算基礎 | `totp.rs` | [10](10-lua-policies.md) |
| RFC 4648 | Base32，TOTP 祕密的編碼 | `totp.rs` | [10](10-lua-policies.md) |
| RFC 3986 | Lua 的 `url_encode` 保留哪些字元不編碼 | `script_http.rs` | [10](10-lua-policies.md) |
| RFC 3339 | 計費記錄檔的時間格式（UTC，到毫秒） | `accounting_log.rs` | [13](13-accounting.md) |

## 其他標準

| 標準 | 程式碼用它做什麼 | 原始碼 | 主章 |
| --- | --- | --- | --- |
| MS-NRPC | AD 的 Netlogon：建立安全通道、`NetrLogonSamLogonEx` 網路登入 | `netlogon/` | [08](08-ldap-and-ad.md) |
| PEAPv0 | PEAP，內層 EAP-MSCHAPv2。程式碼沒有引用標準編號 | `eap.rs` | [11](11-eap.md) |

核心的程式碼沒有引用任何 3GPP TS。

## 程式碼提到、但核心沒有實作的 RFC

這些編號出現在核心的原始碼或 `docs/spectra/specs/`，但功能只有企業版有，核心會拒絕設定或在呼叫時拋出錯誤：

| RFC | 主題 | 核心的情況 |
| --- | --- | --- |
| RFC 6614、RFC 6613 | RadSec（RADIUS over TLS／TCP） | `common/framing.rs`、`clients.rs` 有共用的程式，但 `radius.tls` 被拒 |
| RFC 5176 | CoA 與 Disconnect | `dynamic_authorization.rs` 是掛點；`radius.coa` 被拒，`send_disconnect`、`send_coa` 拋錯 |
| RFC 9887 | TACACS+ over TLS | `tacacs_transport.rs` 是掛點；`tacacs.tls` 被拒 |
| RFC 6960 | OCSP | 只在 spec；`radius.eap.ocsp` 被拒 |
| RFC 4186 | EAP-SIM | 只在 spec；`eap_methods = ["sim"]` 得到 `'sim' is not a method this server offers` |
| RFC 9048 | EAP-AKA' | 只在 spec 與壓測工具；`eap_methods = ["aka-prime"]` 得到 `aka-prime is not supported by this program (EAP-AKA' needs rradius-enterprise)` |
| RFC 5448 | EAP-AKA' 的第一版（RFC 9048 更新了它） | 只在 spec |
| RFC 6733、RFC 3539 | Diameter，以及 AAA 傳輸規範（watchdog 在這裡） | 只在 spec；`[diameter]` 被拒 |
| RFC 7585 | 用 DNS 找 realm 的 RadSec 伺服器 | 只在 spec；`[proxy]` 被拒 |

`docs/spectra/specs/` 裡的 3GPP TS（29.273、33.402、35.205、35.206、35.208）也都屬於企業版的 EAP-AKA' 與 Diameter。

另外，`passwords.rs` 引用 RFC 7914 只是因為它公布了一組 PBKDF2-HMAC-SHA-256 的測試向量，核心並沒有實作那份 RFC 的主題。

## rradius-bench 用到的 RFC

壓測工具是一個 RADIUS 與 TACACS+ 用戶端，它的程式碼引用了 RFC 2865、2866、2548、2759、3579、3748、5216、5281、9190、8907，與伺服器端相同。另外三個只跟對方伺服器有沒有這個功能有關：

| RFC | 工具裡的用途 | 對核心 |
| --- | --- | --- |
| RFC 6614 | `--transport tls` | 核心沒有 RadSec |
| RFC 4187、RFC 9048 | `aka-prime` 負載 | 核心回 Reject |

## RFC 正式標題

本附錄提到的每個 RFC 的正式標題，2026-11-23 對照 rfc-editor.org 的公開資料。上面各表的「主題」與「程式碼用它做什麼」是本書的說明，不是標題。

| RFC | 正式標題 |
| --- | --- |
| RFC 2433 | Microsoft PPP CHAP Extensions |
| RFC 2548 | Microsoft Vendor-specific RADIUS Attributes |
| RFC 2759 | Microsoft PPP CHAP Extensions, Version 2 |
| RFC 2865 | Remote Authentication Dial In User Service (RADIUS) |
| RFC 2866 | RADIUS Accounting |
| RFC 2868 | RADIUS Attributes for Tunnel Protocol Support |
| RFC 3079 | Deriving Keys for use with Microsoft Point-to-Point Encryption (MPPE) |
| RFC 3162 | RADIUS and IPv6 |
| RFC 3339 | Date and Time on the Internet: Timestamps |
| RFC 3539 | Authentication, Authorization and Accounting (AAA) Transport Profile |
| RFC 3579 | RADIUS (Remote Authentication Dial In User Service) Support For Extensible Authentication Protocol (EAP) |
| RFC 3748 | Extensible Authentication Protocol (EAP) |
| RFC 3986 | Uniform Resource Identifier (URI): Generic Syntax |
| RFC 4186 | Extensible Authentication Protocol Method for Global System for Mobile Communications (GSM) Subscriber Identity Modules (EAP-SIM) |
| RFC 4187 | Extensible Authentication Protocol Method for 3rd Generation Authentication and Key Agreement (EAP-AKA) |
| RFC 4226 | HOTP: An HMAC-Based One-Time Password Algorithm |
| RFC 4513 | Lightweight Directory Access Protocol (LDAP): Authentication Methods and Security Mechanisms |
| RFC 4514 | Lightweight Directory Access Protocol (LDAP): String Representation of Distinguished Names |
| RFC 4515 | Lightweight Directory Access Protocol (LDAP): String Representation of Search Filters |
| RFC 4648 | The Base16, Base32, and Base64 Data Encodings |
| RFC 5080 | Common Remote Authentication Dial In User Service (RADIUS) Implementation Issues and Suggested Fixes |
| RFC 5176 | Dynamic Authorization Extensions to Remote Authentication Dial In User Service (RADIUS) |
| RFC 5216 | The EAP-TLS Authentication Protocol |
| RFC 5281 | Extensible Authentication Protocol Tunneled Transport Layer Security Authenticated Protocol Version 0 (EAP-TTLSv0) |
| RFC 5448 | Improved Extensible Authentication Protocol Method for 3rd Generation Authentication and Key Agreement (EAP-AKA') |
| RFC 5997 | Use of Status-Server Packets in the Remote Authentication Dial In User Service (RADIUS) Protocol |
| RFC 6238 | TOTP: Time-Based One-Time Password Algorithm |
| RFC 6613 | RADIUS over TCP |
| RFC 6614 | Transport Layer Security (TLS) Encryption for RADIUS |
| RFC 6733 | Diameter Base Protocol |
| RFC 6929 | Remote Authentication Dial In User Service (RADIUS) Protocol Extensions |
| RFC 6960 | X.509 Internet Public Key Infrastructure Online Certificate Status Protocol - OCSP |
| RFC 7585 | Dynamic Peer Discovery for RADIUS/TLS and RADIUS/DTLS Based on the Network Access Identifier (NAI) |
| RFC 7914 | The scrypt Password-Based Key Derivation Function |
| RFC 8907 | The Terminal Access Controller Access-Control System Plus (TACACS+) Protocol |
| RFC 9048 | Improved Extensible Authentication Protocol Method for 3GPP Mobile Network Authentication and Key Agreement (EAP-AKA') |
| RFC 9190 | EAP-TLS 1.3: Using the Extensible Authentication Protocol with TLS 1.3 |
| RFC 9887 | Terminal Access Controller Access-Control System Plus (TACACS+) over TLS 1.3 |

## 怎麼重新產生 RFC 清單

```console
$ grep -rhoE 'RFC ?[0-9]{3,5}' rradius-core/src | sed 's/RFC */RFC /' | sort -u
$ grep -rhoE 'RFC ?[0-9]{3,5}' docs/spectra/specs | sed 's/RFC */RFC /' | sort -u
```

2026-11-23 的結果：第一個指令印出 31 行，其中 `RFC 822` 是憑證欄位名稱 `RFC822Name` 被順帶抓到，不是引用 RFC，所以是 30 個編號；第二個指令印出 29 個編號。兩邊的每一個編號都已經歸到本附錄的某一個表。每個新出現的編號都要打開註解所在的程式，確認核心真的實作了，才放進[核心實作的 RFC](#核心實作的-rfc)；只是掛點或被拒的，放進[程式碼提到、但核心沒有實作的 RFC](#程式碼提到但核心沒有實作的-rfc)。

## 常見錯誤

**把用戶端與使用者混為一談。** 用戶端是 NAS，寫在 `[[radius.clients]]`；使用者是登入的人，寫在帳號資料表。「用戶端被拒」看 `status` 的 `Discarded: unknown client`，「使用者被拒」看認證紀錄的 `decision="reject"`。

**把授權群組當成 LDAP 群組。** `tacacs.authorize_groups` 依 TACACS+ 設備分組。要依使用者的 LDAP 群組決定權限，是 ldap 處理器的事，見 [08](08-ldap-and-ad.md)。

**把計費 spool 與計費記錄檔混為一談。** 計費 spool（`radius.accounting_spool`）是資料庫寫不進去時的暫存，之後會重送；計費記錄檔（`radius.accounting_log`）是每個計費請求一行的紀錄檔，不會重送。兩者都在 [13](13-accounting.md)。

## 相關章節

- [01 認識 rradius](01-overview.md)
- [05 RADIUS 服務與 NAS 用戶端](05-radius-clients.md)
- [06 字典與屬性](06-dictionary.md)
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)
- [11 802.1X 與 EAP](11-eap.md)
- [12 TACACS+ 設備管理](12-tacacs.md)
- [13 計費與 session](13-accounting.md)
- [16 叢集與高可用](16-cluster.md)
- [附錄 A：設定參考](90-config-reference.md)
- [附錄 B：命令參考](91-command-reference.md)
