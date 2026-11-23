# 06 字典與屬性

> 本章讓你做到：
> - 知道字典是什麼、rradius 附了哪些字典
> - 看懂字典檔的語法與屬性的型別
> - 加入自己的廠商屬性（VSA），在腳本裡讀寫它
> - 從資料庫或腳本設定回覆屬性，避開數值與封包大小的限制

前置條件：照[第 04 章](04-configuration.md)做出一份能啟動的設定。本章用 `[log.trace]` 看實際送出的屬性，它在[第 17 章](17-monitoring.md)有完整說明。

本章小節：

- [概念](#概念)
- [設定字典的位置](#設定字典的位置)
- [附帶的字典](#附帶的字典)
- [字典檔的語法](#字典檔的語法)
- [屬性的型別](#屬性的型別)
- [延伸屬性、TLV 與廠商的格式](#延伸屬性tlv-與廠商的格式)
- [加入自己的廠商屬性](#加入自己的廠商屬性)
- [設定回覆屬性](#設定回覆屬性)
- [數值與封包大小的限制](#數值與封包大小的限制)
- [改了字典要重新啟動](#改了字典要重新啟動)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

## 概念

RADIUS 封包裡的屬性只是「編號 + 長度 + 值」。**字典**把編號對到名稱與型別：編號 1 是 `User-Name`、型別 `string`；編號 27 是 `Session-Timeout`、型別 `integer`。有了字典，腳本、資料庫與日誌才能用 `Session-Timeout = 3600` 這種寫法，而不是一串位元組。

- **標準屬性**：RFC 定義的，編號 1 到 255，以及 RFC 6929 的延伸屬性。
- **廠商屬性（VSA）**：放在編號 26（`Vendor-Specific`）裡，前面加上廠商的 IANA 企業編號。各家 NAS 用它傳 VLAN、頻寬、權限等自家的設定。
- **VALUE**：給整數屬性的值取名，例如 `Service-Type` 的 2 叫 `Framed-User`。

伺服器只認得字典裡有的屬性。請求裡字典沒有的屬性，腳本看不到；回覆時用了字典沒有的名稱，是錯誤。

## 設定字典的位置

| 鍵 | 預設 | reload | 說明 |
| --- | --- | --- | --- |
| `dictionary` | 無，必填 | 重啟 | 字典主檔的路徑；相對路徑以主設定檔的目錄為準 |

```toml
# rradius.toml（節錄）
dictionary = "/usr/share/rradius/dictionary"
```

這是頂層的鍵，只能寫在主設定檔，而且要在第一個 `[表]` 之前。見[附錄 A](90-config-reference.md)。

## 附帶的字典

rradius 附了一整套字典，安裝後的位置：

| 安裝方式 | 位置 |
| --- | --- |
| deb、rpm 套件與 Docker 映像檔 | `/usr/share/rradius/` |
| FreeBSD port | `/usr/local/share/rradius/` |
| 原始碼 | repo 裡的 `share/` |

目錄裡有：

- `dictionary`：主檔。目錄裡有 246 個 `dictionary.*` 檔，主檔用 `$INCLUDE` 讀進其中 232 個；沒讀進來的，要用時自己 `$INCLUDE`。
- `dictionary.rfc2865`、`dictionary.rfc2866` 等：各 RFC 的標準屬性。
- `dictionary.<廠商>`：各廠商的 VSA，例如 `dictionary.cisco`、`dictionary.mikrotik`、`dictionary.microsoft`。
- `NOTICE`、`LICENSE`：來源與授權。

這些字典是 FreeRADIUS 專案 v3.2.x 分支 `share/` 目錄的原樣複本，沒有修改，授權是 CC-BY 4.0（不是 rradius 本身的授權），完整條文在同一目錄的 `LICENSE`，來源與著作權人在 `NOTICE`。散布時要連同這兩個檔案一起。

> **注意：** 不要直接改附帶的檔案：它們屬於套件，升級時會被換掉。要加屬性，照[加入自己的廠商屬性](#加入自己的廠商屬性)另寫一個檔案，用自己的主檔把兩者一起讀進來。

## 字典檔的語法

字典是純文字，一行一個指令，欄位以空白或 Tab 分開。`#` 之後到行尾是註解，可以出現在行的任何位置。

| 指令 | 寫法 | 說明 |
| --- | --- | --- |
| `ATTRIBUTE` | `ATTRIBUTE <名稱> <編號> <型別> [選項]` | 定義一個屬性 |
| `VALUE` | `VALUE <屬性> <值名稱> <數字>` | 給整數屬性的一個值取名 |
| `VENDOR` | `VENDOR <名稱> <企業編號> [format=t,l[,c]]` | 定義一個廠商 |
| `BEGIN-VENDOR`、`END-VENDOR` | `BEGIN-VENDOR <名稱> [format=<屬性>]` | 中間的 `ATTRIBUTE` 都屬於這個廠商 |
| `BEGIN-TLV`、`END-TLV` | `BEGIN-TLV <屬性>` | 中間的 `ATTRIBUTE` 是這個 `tlv` 屬性的子屬性 |
| `$INCLUDE` | `$INCLUDE <檔案>` | 讀進另一個字典檔 |

- 數字可以寫十進位，也可以寫 `0x` 開頭的十六進位；巢狀的屬性編號寫成以點分開的形式（例如 `241.1`）。
- `$INCLUDE` 的相對路徑以**寫這一行的那個檔案**所在的目錄為準（和設定檔的規則不同）。互相 `$INCLUDE` 形成迴圈時會被偵測出來。
- `VALUE` 指向一個還沒定義的屬性時，那一行被略過，不算錯誤。
- `ATTRIBUTE` 的選項用逗號分開：
  - `has_tag`：屬性帶 tag（RFC 2868 的通道屬性用）。
  - `encrypt=1`：用 User-Password 的方式加密。
  - `encrypt=2`：用 Tunnel-Password 的加鹽方式加密。
  - `encrypt=3`：讀得進來，但 `set_attr` 不能送它。
  - 其他值是錯誤。
- `VENDOR` 的 `format=t,l`：t 是廠商屬性編號占幾個位元組（1、2 或 4），l 是長度欄位占幾個位元組（0、1 或 2）。預設是 `1,1`。`c`（延續位元）只能和 `1,1` 一起用。
- 錯誤會指出檔案與行號；錯在被 `$INCLUDE` 的檔案時，會一層一層列出來，例如 `dictionary:3: dictionary.example:14: unsupported directive 'FLAGS'`。

## 屬性的型別

型別決定 `get_attr` 讀到什麼、`set_attr` 要給什麼：

| 型別 | 長度 | `get_attr` 讀到 | `set_attr` 接受 |
| --- | --- | --- | --- |
| `integer`、`date` | 4 | 數字 | 數字、數字的字串，或這個屬性的 `VALUE` 名稱 |
| `byte`、`short`、`integer64` | 1、2、8 | 數字 | 同上，超出範圍是錯誤 |
| `signed` | 4 | 數字（可以是負的） | 數字或數字的字串 |
| `ipaddr` | 4 | `"192.0.2.1"` | IPv4 位址的字串 |
| `ipv6addr` | 16 | `"2001:db8::1"` | IPv6 位址的字串 |
| `ipv6prefix`、`ipv4prefix` | 可變 | `"2001:db8:1::/64"` | 前綴的字串，主機部分要是 0 |
| `ifid` | 8 | `"0011:2233:4455:6677"` | 同樣格式的字串 |
| `ether` | 6 | `"00:11:22:33:44:55"` | MAC 位址的字串 |
| `combo-ip` | 4 或 16 | IPv4 或 IPv6 的字串 | IPv4 或 IPv6 的字串 |
| `string`、`text`、`octets` | 可變 | 原樣的位元組字串 | 字串，原樣送出；給數字時送出它的十進位文字 |
| `octets[N]` | N | 原樣的位元組字串 | 剛好 N 個位元組的字串 |

- 整數類型的屬性，`get_attr` 讀到的一定是**數字**，不是 `VALUE` 名稱：要判斷 `Service-Type` 時寫 `get_attr('Service-Type') == 2`。`set_attr` 則兩種都接受，`set_attr('Service-Type', 'Framed-User')` 和 `set_attr('Service-Type', 2)` 相同。
- 長度和型別對不上的值（例如 5 個位元組的 `integer`），`get_attr` 讀到原樣的位元組。
- `tlv`、`evs`、`vsa` 這類容器型別本身不能用 `set_attr` 設定，設定的是它們底下的屬性。
- 帶 tag 的屬性用 `名稱:tag` 指定 tag，tag 是 1 到 31，例如 `Tunnel-Type:1`。只有字典標了 `has_tag` 的屬性能這樣寫。

## 延伸屬性、TLV 與廠商的格式

標準屬性只有 255 個編號，廠商屬性的格式也各家不同。字典用下面幾種寫法描述它們，腳本不必知道封包裡怎麼排，一律用屬性的名稱讀寫。

**延伸屬性（RFC 6929）。** 編號 241 到 246 是容器，型別是 `extended`（241 到 244）或 `long-extended`（245、246）。真正的屬性寫成「容器編號.子編號」：

```text
# 附帶的 dictionary.rfc*（節錄）
ATTRIBUTE	Extended-Attribute-1			241	extended
ATTRIBUTE	Frag-Status				241.1	integer
ATTRIBUTE	Operator-NAS-Identifier			241.8	octets
```

- `long-extended` 的值超過一個屬性放得下的長度時，送出時切成每段 251 個位元組，收到時再接回來。
- 容器下的 `evs`（Extended-Vendor-Specific）放的是廠商屬性，用 `BEGIN-VENDOR <廠商> format=<evs 屬性>` 定義。

**TLV。** 型別 `tlv` 的屬性裡面還有子屬性，子屬性寫成「父編號.子編號」，或寫在 `BEGIN-TLV <父屬性>` 與 `END-TLV` 之間。例如附帶的 WiMAX 字典：

```text
# 附帶的 dictionary.wimax（節錄）
ATTRIBUTE	WiMAX-Capability			1	tlv
ATTRIBUTE	WiMAX-Release				1.1	string
ATTRIBUTE	WiMAX-Accounting-Capabilities		1.2	byte
```

- 腳本直接設定子屬性：`set_attr('WiMAX-Release', '2.1')`。
- 連續設定同一個 TLV 底下的子屬性時，它們被包進同一個 TLV；放不下時才另開一個。
- 子屬性的值還要放得進外面的 TLV 和屬性，放不下時 `set_attr` 失敗：`value of 'WiMAX-Release' is too long`。
- 收到的 TLV 格式錯誤時，整個屬性被略過；不認得的子屬性被跳過。

**廠商的格式。** 大多數廠商的屬性編號與長度各占 1 個位元組，也就是預設的 `format=1,1`。有些廠商不同，`VENDOR` 那一行會寫出來：

```text
# 附帶的字典（節錄）
VENDOR		Lucent				4846	format=2,1
VENDOR		WiMAX				24757	format=1,1,c
```

- `format=2,1`：屬性編號占 2 個位元組。
- `format=1,1,c`：有延續位元，超過一個 Vendor-Specific 放得下的值會分成好幾個送出，收到時接回來。

**驗證：** 讓腳本同時設定這幾種屬性，再用 trace 看回應：

```lua
-- auth.lua（節錄）
set_attr('WiMAX-Release', '2.1')
set_attr('WiMAX-Accounting-Capabilities', 1)
set_attr('Operator-NAS-Identifier', 'nas-01')
set_attr('Lucent-PPP-Circuit-Name', 'circuit-7')
set_attr('WiMAX-AAA-Session-Id', string.rep('s', 300))
return 'accept'
```

```text
… INFO rradius_trace: response client="localhost" … code=2 attributes="Message-Authenticator = <hidden>, WiMAX-Release = 2.1, WiMAX-Accounting-Capabilities = 1, Operator-NAS-Identifier = nas-01, Lucent-PPP-Circuit-Name = circuit-7, WiMAX-AAA-Session-Id = sss…"
```

**結果長什麼樣子：** 登入被接受。其中 `WiMAX-AAA-Session-Id` 有 300 個位元組，超過一般屬性的 253 個位元組，靠 WiMAX 的延續位元分段送出；換成沒有延續位元的廠商，同樣的值會得到 `value of '…' is too long`。trace 只顯示屬性與值，看不到封包裡怎麼分段。

> **未實機驗證：** 分段後的封包能不能被 NAS 正確接回，要看 NAS 的實作，本章沒有用真的 NAS 驗證。

## 加入自己的廠商屬性

**目標：** NAS 帶來一個自家的 VSA 說明使用者屬於哪個群組，伺服器依它決定權限等級並回給 NAS。附帶的字典沒有這家廠商。

**設定：** 自己寫一個字典主檔，讀進附帶的字典，再讀進自己的檔案。下面的廠商與編號 32473 是 IANA 保留給文件範例用的企業編號，換成你的 NAS 廠商的編號與屬性定義（通常在廠商的文件或它附的字典檔裡）。

```text
# dictionary：這個伺服器的字典主檔
$INCLUDE /usr/share/rradius/dictionary
$INCLUDE dictionary.example
```

```text
# dictionary.example：自己的廠商屬性
VENDOR		Example-Corp	32473

BEGIN-VENDOR	Example-Corp
ATTRIBUTE	Example-Group		1	string
ATTRIBUTE	Example-Level		2	integer
ATTRIBUTE	Example-Gateway		3	ipaddr

VALUE	Example-Level		Guest			1
VALUE	Example-Level		Staff			2
VALUE	Example-Level		Admin			3
END-VENDOR	Example-Corp
```

把兩個檔案放在主設定檔旁邊，`dictionary` 指向自己的主檔：

```toml
# rradius.toml（節錄）
dictionary = "dictionary"
```

腳本讀請求的 `Example-Group`，回覆 `Example-Level`、`Example-Gateway` 與標準的 `Session-Timeout`：

```lua
-- auth.lua
if protocol ~= 'radius' then
  return 'reject'
end
if phase ~= 'authentication' then
  return 'accept'
end
if get_attr('User-Name') ~= 'alice' or get_attr('User-Password') ~= 'alice-password' then
  return 'reject'
end
if get_attr('Example-Group') ~= 'staff' then
  return 'reject'
end
set_attr('Example-Level', 'Staff')
set_attr('Example-Gateway', '192.0.2.254')
set_attr('Session-Timeout', 3600)
return 'accept'
```

為了看到實際送出的屬性，暫時打開這個使用者的 trace：

```toml
# conf.d/90-trace.toml
[log.trace]
users = ["alice"]
minutes = 30
```

**驗證：** 先 `check`，再啟動伺服器。`rradius-bench` 不認得字典名稱，用 `--attr 26=hex:…` 直接帶一個 VSA：`00007ed9` 是企業編號 32473，`01` 是屬性編號，`07` 是長度，後面是 `staff` 的 ASCII。

```console
$ rradius-server check --config rradius.toml
configuration ok
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1 \
    --attr 26=hex:00007ed901077374616666
…
  pap: 1844/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
…
  pap: 4417/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

**結果長什麼樣子：** 帶了 `Example-Group = staff` 的請求被接受，沒帶的被拒絕。trace 的日誌：

```text
… INFO rradius_trace: request client="localhost" source=127.0.0.1 user="alice" code=1 attributes="Message-Authenticator = <hidden>, User-Name = alice, User-Password = <hidden>, NAS-IP-Address = 192.0.2.1, NAS-Identifier = rradius-bench, Example-Group = staff"
… INFO rradius_trace: response client="localhost" source=127.0.0.1 user="alice" code=2 attributes="Message-Authenticator = <hidden>, Example-Level = 2, Example-Gateway = 192.0.2.254, Session-Timeout = 3600"
```

- 請求裡的 VSA 以字典的名稱顯示。
- 回覆時寫的 `'Staff'` 依 `VALUE` 送成 2，trace 顯示的是數字。

用完記得刪掉 `90-trace.toml` 再 reload：trace 會把個人資料寫進日誌。

> **注意：** trace 以原始的數值顯示帶 tag 的屬性。例如 `Tunnel-Type:1` 設成 `VLAN`（13）時顯示 `Tunnel-Type = 16777229`，也就是 tag 1 放在最高位元組、值 13 在低三個位元組；帶 tag 的字串顯示成十六進位，例如 `Tunnel-Private-Group-Id = 0x01313030`（tag 1 加上 `100`）。加密的屬性一律顯示 `<hidden>`。

### 加密的屬性

有些 VSA 裝的是金鑰或密碼，字典會在型別後面標 `encrypt=1` 或 `encrypt=2`。`set_attr` 照常給明文，伺服器用這個用戶端的共用密碼加密後才送出：

```text
# dictionary.example（加在 END-VENDOR 之前）
ATTRIBUTE	Example-Key		5	string	encrypt=1
```

腳本寫 `set_attr('Example-Key', 'session-key-0123')`，登入被接受，trace 顯示 `Example-Key = <hidden>`，不會把明文寫進日誌。

## 設定回覆屬性

回覆屬性（Accept 時要送給 NAS 的屬性）有兩個來源，兩者可以並用：

1. **腳本**：`set_attr(名稱, 值)`，如上一節。腳本的寫法見[第 10 章](10-lua-policies.md)。
2. **驗證鏈的資料庫查詢**：`sql` 處理器的 `reply_query` 傳回「屬性名稱、值」兩欄的列，每一列變成一個回覆屬性（[第 07 章](07-users-and-chains.md)）。

`reply_query` 傳回的值一律是文字，依字典的型別轉換：整數屬性可以寫數字或 `VALUE` 名稱，帶 tag 的寫 `名稱:tag`。例如這樣的資料：

```sql
-- migrations/1_users.sql（節錄）
INSERT INTO user_reply VALUES
  ('alice', 'Session-Timeout', '3600'),
  ('alice', 'Example-Level', 'Staff'),
  ('alice', 'Tunnel-Type:1', 'VLAN'),
  ('alice', 'Tunnel-Medium-Type:1', 'IEEE-802'),
  ('alice', 'Tunnel-Private-Group-Id:1', '100'),
  ('alice', 'Framed-IP-Address', '192.0.2.10'),
  ('bad', 'Example-Level', 'Boss');
```

搭配 `reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"`，`alice` 登入時回覆這六個屬性（`Example-Level` 送成 2）。

**值不合字典時，整個登入變成拒絕。** `bad` 的 `Boss` 不是 `Example-Level` 的 `VALUE`，結果是 Access-Reject，並記一行錯誤：

```text
… ERROR rradius_core::radius: RADIUS response attribute error: 'Boss' is not a value of 'Example-Level'
```

伺服器寧可拒絕，也不送出一個少了屬性的 Accept：少了 VLAN 或頻寬限制的放行，可能讓使用者進到不該進的網路。

腳本裡的 `set_attr` 遇到錯誤時會丟出 Lua 錯誤。沒有用 `pcall` 接住時，腳本就此中止，登入被拒絕：

```text
… ERROR … Lua script error (radius/authentication): runtime error: unknown attribute 'Example-Nope'…
```

`set_attr` 常見的錯誤訊息：

| 寫法 | 訊息 |
| --- | --- |
| `set_attr('Example-Nope', 'x')` | `unknown attribute 'Example-Nope'` |
| `set_attr('Example-Level', 'Boss')` | `'Boss' is not a value of 'Example-Level'` |
| `set_attr('Example-Gateway', 'gateway')` | `'Example-Gateway' needs an IPv4 address` |
| `set_attr('Filter-Id:1', 'x')` | `attribute 'Filter-Id' takes no tag` |
| `set_attr('Framed-IPv6-Prefix', '2001:db8::1/64')` | `'2001:db8::1/64' is not a valid ipv6prefix for 'Framed-IPv6-Prefix'` |

## 數值與封包大小的限制

| 限制 | 超過時 |
| --- | --- |
| 一般屬性的值最多 253 個位元組 | `set_attr` 的錯誤 `value of 'Reply-Message' is too long` |
| `encrypt=1` 的值 1 到 128 個位元組，不能帶 tag | `set_attr` 的錯誤 |
| `encrypt=2` 的值最多 239 個位元組 | `set_attr` 的錯誤，例如 `'WiMAX-MSK': value is longer than 239 octets` |
| TLV 子屬性的值要放得進外面的 TLV 與屬性 | `set_attr` 的錯誤 `value of '…' is too long` |
| 整個回應封包最多 4096 個位元組 | 改送不帶屬性的 Access-Reject，並記一行錯誤 |

- `long-extended` 的屬性，以及字典標了延續位元（`format=1,1,c`）的廠商屬性，不受 253 個位元組的限制，會自動分段（見[延伸屬性、TLV 與廠商的格式](#延伸屬性tlv-與廠商的格式)）。
- 回應太大的錯誤，例如同一個 Accept 放了 20 個 250 位元組的 `Reply-Message`：

  ```text
  … ERROR rradius_core::radius: RADIUS response to localhost is longer than 4096 octets
  ```

  NAS 收到的是拒絕。把大量的屬性拆開，或改用較短的值。

## 改了字典要重新啟動

字典只在伺服器啟動時讀一次。

- 改了 `dictionary` 的**路徑**再 reload：reload 照樣成功，日誌的 `WARN` 列出 `these need a restart to apply: dictionary`。
- 只改了字典**檔案的內容**（例如在 `dictionary.example` 加一個 `Example-Room`）再 reload：日誌只寫 `configuration reloaded`，**沒有任何警告**，但伺服器仍用舊的字典，腳本用新屬性會得到 `unknown attribute 'Example-Room'`。重新啟動之後才生效。

> **警告：** 改了字典之後一定要重新啟動伺服器，不要只送 SIGHUP。先用 `check` 確認新的字典讀得進來：`check` 每次都從檔案重新讀字典。

## 常見錯誤

**症狀：** `check` 印出 `dictionary:3: dictionary.example:14: unsupported directive 'FLAGS'`。
**原因：** 字典用了 rradius 不支援的指令。訊息一層一層列出是從主檔第 3 行的 `$INCLUDE` 讀進來、`dictionary.example` 第 14 行。
**解法：** 刪掉或改寫那一行；支援的指令見[字典檔的語法](#字典檔的語法)。

**症狀：** `… unknown vendor 'Nobody'`。
**原因：** `BEGIN-VENDOR` 或 `ATTRIBUTE` 用了一個還沒有 `VENDOR` 定義的廠商。
**解法：** 在前面加上 `VENDOR` 那一行，或修正名稱。

**症狀：** `… cannot include 'dictionary.missing': No such file or directory (os error 2)`。
**原因：** `$INCLUDE` 的檔案不存在。相對路徑以寫 `$INCLUDE` 的那個字典檔的目錄為準。
**解法：** 修正路徑，或改用絕對路徑。

**症狀：** `… unknown encryption 'encrypt=9' in: ATTRIBUTE Example-Secret 9 string encrypt=9`。
**原因：** `encrypt=` 只能是 1、2、3。
**解法：** 照 NAS 廠商的定義改正。

**症狀：** `… invalid vendor format 'format=3,1'`。
**原因：** `format=` 的屬性編號長度只能是 1、2、4，長度欄位只能是 0、1、2。
**解法：** 照廠商的定義改正。

**症狀：** `… ATTRIBUTE needs a name, number and type: ATTRIBUTE Short 9`、`… END-TLV without matching BEGIN-TLV`、`… include cycle: 'dictionary' is already being loaded`。
**原因：** 少了欄位、`BEGIN-TLV`／`END-TLV` 沒有成對，或字典互相 `$INCLUDE`。
**解法：** 照訊息的檔案與行號修正。

**症狀：** 腳本 `get_attr('Service-Type') == 'Framed-User'` 永遠不成立。
**原因：** 整數屬性的 `get_attr` 傳回數字，不是 `VALUE` 名稱。
**解法：** 和數字比較：`get_attr('Service-Type') == 2`。

**症狀：** 使用者被拒絕，日誌有 `RADIUS response attribute error: …`。
**原因：** 資料庫的回覆屬性有不合字典的值或名稱。
**解法：** 照訊息修正那一列的資料；名稱要和字典一字不差（含大小寫）。

**症狀：** 加了新屬性、reload 也成功了，腳本卻得到 `unknown attribute`。
**原因：** 字典的內容只在啟動時讀。
**解法：** 重新啟動伺服器。

## 相關章節

- [04 設定檔的基本功](04-configuration.md)
- [05 RADIUS 服務與 NAS 用戶端](05-radius-clients.md)
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)
- [10 用 Lua 寫政策](10-lua-policies.md)
- [14 情境實作：寬頻與 ISP](14-isp-recipes.md)
- [15 情境實作：組織網路存取](15-access-recipes.md)
- [17 監控、狀態與日誌](17-monitoring.md)
- [附錄 A：設定參考](90-config-reference.md)
- [附錄 D：Lua API 參考](93-lua-reference.md)
