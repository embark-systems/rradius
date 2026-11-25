# 11 802.1X 與 EAP

> 本章讓你做到：
> - 準備 EAP 用的伺服器憑證與 CA，設定 `[radius.eap]`
> - 用 EAP-TLS 讓公司配發的設備憑證登入，並用資料表或腳本決定誰能進來
> - 用 CRL 擋下遺失或撤銷的憑證
> - 用 PEAP 或 TTLS 讓員工以帳號密碼登入，並選對密碼的存放格式
> - 看懂 EAP 的指標與日誌，找出登入失敗的原因

前置條件：

- 讀過[第 03 章](03-quick-start.md)，會啟動伺服器、用 `rradius-bench` 測試登入。
- 知道驗證鏈與 sql 處理器是什麼（[第 07 章](07-users-and-chains.md)）。
- 機器上有 `openssl`，用來產生示範憑證。

本章小節：

- [概念](#概念)
- [核心提供的三種方法](#核心提供的三種方法)
- [一次 EAP 登入怎麼走](#一次-eap-登入怎麼走)
- [準備憑證](#準備憑證)
- [設定 radius.eap](#設定-radiuseap)
- [每個用戶端能用哪些方法](#每個用戶端能用哪些方法)
- [EAP-TLS：用憑證登入](#eap-tls用憑證登入)
- [憑證通過之後的授權](#憑證通過之後的授權)
- [用 CRL 擋下撤銷的憑證](#用-crl-擋下撤銷的憑證)
- [換發伺服器憑證](#換發伺服器憑證)
- [PEAP 與 TTLS：用帳號密碼登入](#peap-與-ttls用帳號密碼登入)
- [密碼要怎麼存](#密碼要怎麼存)
- [在 Lua 腳本裡看 EAP](#在-lua-腳本裡看-eap)
- [對話上限與逾時](#對話上限與逾時)
- [資料庫無法使用時](#資料庫無法使用時)
- [EAP 的指標與日誌](#eap-的指標與日誌)
- [接上真的 NAS 與用戶端](#接上真的-nas-與用戶端)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

## 概念

802.1X 是有線網路與 Wi-Fi（WPA2／WPA3-Enterprise）用的登入方式。一次登入有三方：

| 角色 | 是誰 | 做什麼 |
| --- | --- | --- |
| 用戶端（supplicant） | 筆電、手機上的 802.1X 程式 | 出示憑證或帳號密碼 |
| NAS（網路存取設備，例如 AP 控制器、交換器） | 在 rradius 設定裡是一個用戶端（client） | 把用戶端送來的 EAP 封包裝進 RADIUS，轉給 rradius |
| rradius | 認證伺服器 | 跟用戶端走完 EAP，決定接受或拒絕，把加密金鑰交給 NAS |

NAS 自己不懂 EAP 的內容，它只負責轉送。用戶端與 rradius 之間的對話放在 `EAP-Message` 屬性裡，一來一回要好幾輪：rradius 每一輪回 Access-Challenge，帶一個 `State`，NAS 下一輪把同一個 `State` 帶回來，rradius 就知道是哪一段對話。最後一輪回 Access-Accept 或 Access-Reject。

Access-Accept 裡除了你的回覆屬性（例如 VLAN），還有 `MS-MPPE-Recv-Key` 與 `MS-MPPE-Send-Key`。這是從 TLS 握手推導出來的金鑰，NAS 用它跟用戶端建立無線加密。這兩個屬性由 rradius 自動加上，你不用設定。

幾件和一般 PAP 登入不一樣的事：

- **整段對話由 rradius 自己處理。** 設定了 `[radius.eap]` 之後，帶 `EAP-Message` 的請求不會每一輪都去跑驗證鏈或腳本。驗證鏈只在最後被問一次：EAP-TLS 在握手完成之後，PEAP 與 TTLS 在拿到通道裡的密碼時。
- **一定要有 Message-Authenticator。** 帶 `EAP-Message` 的請求如果沒有 `Message-Authenticator`，rradius 不回應、直接丟掉，不管這個用戶端的 `require_message_authenticator` 怎麼設（RFC 3579）。NAS 送 EAP 時本來就會帶，一般不會遇到。
- **對話存在收到它的節點的記憶體裡。** 叢集裡有好幾個節點時，同一段對話的每一輪都要送到同一個節點；送到別的節點時，那裡沒有這個 `State`，回 EAP-Failure。所以 NAS 與 rradius 之間如果有負載平衡器，要讓同一個 NAS 的請求固定送到同一個節點。
- **沒有設定 `[radius.eap]` 時**，`EAP-Message` 只是一個普通屬性，請求照一般流程交給驗證鏈或腳本。

## 核心提供的三種方法

`rradiusd` 提供三種 EAP 方法。設定裡寫方法名稱：

| 設定裡的名稱 | 方法 | 用戶端出示 | TLS 版本 | 用戶端要不要憑證 | 驗證鏈拿到什麼 |
| --- | --- | --- | --- | --- | --- |
| `tls` | EAP-TLS（RFC 5216、RFC 9190） | 憑證 | TLS 1.2、1.3 | 要，且要由 `client_ca_file` 的 CA 簽發 | 憑證上的名稱，憑證本身就是證明 |
| `peap` | PEAP 版本 0，內層 EAP-MSCHAPv2 | 帳號密碼 | 只有 TLS 1.2 | 不要 | 內層的帳號與 MS-CHAPv2 回應 |
| `ttls` | EAP-TTLS 版本 0（RFC 5281），內層 PAP 或 MS-CHAPv2 | 帳號密碼 | 只有 TLS 1.2 | 不要 | 內層的帳號與密碼（PAP）或 MS-CHAPv2 回應 |

不提供的方法：

- EAP-MD5 不提供。用戶端只要求 EAP-MD5 時，對話以 EAP-Failure 結束。
- `methods` 寫其他名稱時，`check` 會拒絕，例如 `md5` 得到 `'md5' is not a method this server offers; it has tls, peap and ttls`。

TLS session 恢復也不提供：每一次登入都是完整的握手。

> **企業版：** EAP-SIM、EAP-AKA'、TLS session 恢復與 OCSP 憑證檢查都在企業版；核心的 `check` 遇到它們的設定會直接拒絕，見本章的[常見錯誤](#常見錯誤)。

怎麼選：

- **公司配發、能裝憑證的設備**（筆電、受管理的手機）用 EAP-TLS。沒有密碼可以猜，遺失時用 CRL 撤銷。
- **員工自己的設備**用 PEAP。幾乎所有作業系統都內建 PEAP-MSCHAPv2。
- **TTLS 內層 PAP** 適合密碼只能用明文比對的帳號來源，例如用 LDAP bind 檢查密碼的目錄（[第 08 章](08-ldap-and-ad.md)）。內層密碼在 TLS 通道裡，不會以明文經過空中。

## 一次 EAP 登入怎麼走

以 EAP-TLS 為例，`rradius-bench` 報的 `rounds per finished conversation: 6.0` 就是這六輪：

1. NAS 送出用戶端的 EAP-Response/Identity（例如 `laptop-001.corp.example.com`）。
2. rradius 挑出這個用戶端可以用的第一個方法，回 EAP-TLS Start。
3. 之後幾輪是 TLS 握手。一個 TLS 訊息太長時，rradius 依 `fragment_size` 切成好幾段，每段等用戶端確認再送下一段。
4. 握手成功之後，rradius 從憑證取出名稱，交給驗證鏈（或沒有驗證鏈時交給 Lua 腳本）決定。
5. 接受時回 Access-Accept，帶 EAP-Success、MPPE 金鑰與回覆屬性；拒絕時回 Access-Reject，帶 EAP-Failure。

用戶端不想用 rradius 先提議的方法時，會回一個 Legacy Nak，列出它要的方法。rradius 會在這個用戶端允許的方法裡挑一個重新開始；都不允許就結束對話。這只能在用戶端還沒送出任何 TLS 資料之前發生，握手開始之後就不能換方法。

整段對話結束時，伺服器日誌會有一行認證紀錄，`chain="eap"`，`handler` 是 `eap-tls`、`eap-peap` 或 `eap-ttls`。中間的 Access-Challenge 不寫紀錄。

## 準備憑證

EAP 至少需要伺服器憑證與它的私鑰。用 EAP-TLS 時，還要有簽發用戶端憑證的 CA。

| 檔案 | 設定 | 用途 |
| --- | --- | --- |
| 伺服器憑證鏈（PEM） | `cert_file` | 用戶端用它確認自己連到的是真的 RADIUS 伺服器 |
| 伺服器私鑰（PEM） | `key_file` | 只放在伺服器上，權限 0600 |
| 用戶端憑證的 CA（PEM） | `client_ca_file` | EAP-TLS 只接受這些 CA 簽發的用戶端憑證 |
| 撤銷清單（PEM） | `crl_file` | 可選，EAP-TLS 用來擋下撤銷的憑證 |

幾個要點：

- **用戶端憑證只信任 `client_ca_file`。** 系統的根憑證不會被信任，否則任何公開 CA 簽的憑證都能登入。
- **伺服器憑證要讓用戶端驗得過。** 用戶端要設定信任簽發伺服器憑證的 CA，並檢查伺服器名稱（例如 wpa_supplicant 設定裡的 `domain_match`）。範例的伺服器憑證是 `DNS:radius.corp.example.com`，用途 `serverAuth`。
- **私鑰權限。** `key_file` 如果其他人讀得到，每次載入設定都會警告：

  ```text
  … WARN rradius_core::common::seal: radius.eap.key_file: 'certs/server.key' has mode 644, which others can reach or its group can write; make it 0600 or 0640
  ```

  拿到這把私鑰的人可以冒充你的 RADIUS 伺服器，騙取 PEAP 的密碼雜湊。

兩個情境都附了產生示範憑證的腳本：

| 情境 | 腳本 | 產生的檔案（在 `certs/`） |
| --- | --- | --- |
| `enterprise-wifi` | `make-certs.sh` | `ca.pem`、`ca.key`、`server.pem`／`.key`、一張裝置憑證 `device.pem`／`.key`（`DNS:laptop-001.corp.example.com`） |
| `wifi-certs` | `setup.sh` | `ca.pem`、`ca.key`、`server.pem`／`.key`、`laptop-001` 與 `laptop-002` 兩張筆電憑證、`ca.crl`（裡面有 `laptop-002`），以及給 `openssl ca` 用的 `crl-db/` |

兩個腳本都用 `openssl req` 產生金鑰與 CSR，再用 `openssl x509` 簽發，OpenSSL 與 LibreSSL 都能用。`setup.sh` 遇到已經有 `certs/ca.pem` 時不會覆蓋。

> **警告：** 這些腳本產生的 CA 只能拿來測試。正式環境用公司的 CA 簽發伺服器憑證與裝置憑證，CA 的私鑰不要放在 RADIUS 伺服器上。

## 設定 radius.eap

`[radius.eap]` 掛在 `[radius]` 底下。最小的 EAP-TLS 設定只要四個檔案路徑：

```toml
# conf.d/eap.toml
[radius.eap]
methods = ["tls"]
cert_file = "certs/server.pem"
key_file = "certs/server.key"
client_ca_file = "certs/ca.pem"
crl_file = "certs/ca.crl"
```

這份片段放在 `config init` 底稿的 `conf.d/` 裡，已經用 `check` 驗證過（`configuration ok`）。路徑相對於主設定檔 `rradius.toml` 所在的目錄。

全部的鍵（預設值取自 `config show`，完整說明見[附錄 A](90-config-reference.md)）：

| 鍵 | 預設值 | 意思 |
| --- | --- | --- |
| `methods` | `["tls"]` | 提供的方法，依序；第一個是開頭提議的方法。至少一個，不能重複 |
| `cert_file` | 無 | 伺服器憑證鏈，PEM。有任何方法時都必填 |
| `key_file` | 無 | 伺服器私鑰，PEM。必填 |
| `client_ca_file` | 無 | 簽發用戶端憑證的 CA。`methods` 有 `tls` 時必填，沒有 `tls` 時不能寫 |
| `crl_file` | 無 | 用戶端憑證的撤銷清單。只用於 `tls`，沒有 `tls` 時不能寫 |
| `match_identity` | `true` | EAP-TLS：憑證上的名稱要對得上用戶端報的 EAP Identity |
| `fragment_size` | `1000` | 每段 TLS 資料的上限，256 到 1400 位元組 |
| `session_timeout_ms` | `30000` | 一段沒完成的對話多久沒有下一輪就丟掉（毫秒），要大於 0 |
| `max_sessions` | `4096` | 全部保留的未完成對話數 |
| `max_sessions_per_client` | `1024` | 每個用戶端（NAS）保留的未完成對話數 |
| `max_sessions_per_device` | `4` | 每台裝置（同一個用戶端、同一個 `Calling-Station-Id`）保留的未完成對話數 |

整個 `[radius.eap]` 都可以 reload：送 SIGHUP 之後，新的對話就用新的設定與重新讀進來的憑證、CRL；已經在進行的對話用它開始時的設定走完。新設定有錯時（例如憑證檔不見了），reload 失敗，伺服器繼續用原來的設定，登入不受影響。實測的日誌：

```text
… ERROR rradius_core::reload: reload failed, keeping the running configuration: rradius.toml: radius.eap.cert_file 'certs/gone.pem': I/O error: No such file or directory (os error 2)
```

這時 `rradius_reload_total{result="error"}` 加 1。換憑證時，先用 `check` 確認新的檔案，再送 SIGHUP。

`methods` 有 `peap` 或 `ttls` 時，設定裡一定要有 `[auth]`：通道裡的密碼只有驗證鏈能檢查。少了它，`check` 回 `radius.eap.methods has a method whose password only a chain can check, and there is no [auth] section`。

## 每個用戶端能用哪些方法

用戶端的 `eap_methods` 決定這台 NAS 上的設備能用哪些方法，依序提議：

```toml
# rradius.toml（節錄）
[[radius.clients]]
name = "wlc"
address = "10.30.0.10"
secret_file = "wlc.secret"
eap_methods = ["tls", "peap"]
# …（其他設定略）
```

規則：

- `radius.eap.methods` 只有一個方法時，用戶端可以不寫 `eap_methods`，就用那一個。
- `radius.eap.methods` 有兩個以上的方法時，**每個用戶端都要寫** `eap_methods`。不用 EAP 的用戶端寫 `eap_methods = []`。少寫時 `check` 回 `radius client 'example': eap_methods is required when radius.eap.methods has more than one method; [] for a client without EAP`。
- `eap_methods` 只能列 `radius.eap.methods` 裡有的方法，順序可以不同。例如 `methods = ["tls", "peap"]`，某台 NAS 寫 `eap_methods = ["peap"]`，那裡的設備一開始就被提議 PEAP。
- 寫了 `eap_methods = []` 的用戶端送來 EAP，rradius 直接回 EAP-Failure，不開對話。

為什麼要每個用戶端都寫？用戶端可以用 Legacy Nak 要求換方法。如果加了 PEAP 之後所有 NAS 都自動提供，原本只想讓設備用憑證登入的網路，也能改用密碼登入。逐台寫出來，加方法時就不會意外打開別的網路。

## EAP-TLS：用憑證登入

**目標：** 用 `wifi-certs` 情境讓 `laptop-001` 以憑證登入，並看到撤銷的 `laptop-002` 被擋下。

這個情境的設定重點：

```toml
# rradius.toml（節錄）
[auth]
policy = "first_match"

[[auth.handlers]]
type = "sql"
user_query = "SELECT NULL FROM devices WHERE name = ?"
certificate_query = "SELECT 1 FROM devices WHERE name = ? AND enabled = 1"
reply_query = """WITH d AS (SELECT vlan FROM devices WHERE name = ?)
  SELECT 'Tunnel-Type:1', 'VLAN' FROM d
  UNION ALL SELECT 'Tunnel-Medium-Type:1', 'IEEE-802' FROM d
  UNION ALL SELECT 'Tunnel-Private-Group-Id:1', vlan FROM d"""

[radius.eap]
cert_file = "certs/server.pem"
key_file = "certs/server.key"
client_ca_file = "certs/ca.pem"
methods = ["tls"]
crl_file = "certs/ca.crl"

[accounts]
enable = "UPDATE devices SET enabled = 1 WHERE name = ?1"
disable = "UPDATE devices SET enabled = 0 WHERE name = ?1"
show = "SELECT name, vlan, enabled FROM devices WHERE name = ?1"
# …（其他設定略）
```

- 設備列在 `devices` 表（`migrations/1_schema.sql`），名稱是憑證上的 DNS 名稱，`vlan` 決定放進哪個 VLAN。
- `certificate_query` 是憑證通過之後用的查詢：查到一列就接受。`user_query` 回 `NULL` 密碼，所以這張表不能拿來做密碼登入。
- `reply_query` 回三個 VLAN 屬性，`:1` 是 tag。
- `[accounts]` 只寫了 `enable`、`disable`、`show`，讓你用 `user` 指令停用或啟用設備。

**設定：** 照[第 03 章](03-quick-start.md)的方式準備工作目錄，然後產生憑證、換掉範例密碼。情境的用戶端位址是 `10.30.0.10`，在本機測試時改成 `127.0.0.1`：

```console
$ cp -R rradius-server/examples/scenarios/wifi-certs ~/rradius-lab/
$ sed -i.bak "s#^dictionary = .*#dictionary = \"$PWD/share/dictionary\"#" ~/rradius-lab/wifi-certs/rradius.toml
$ cd ~/rradius-lab/wifi-certs
$ sh setup.sh
wrote certs in …/rradius-lab/wifi-certs/certs: ca.pem, ca.crl, server, laptop-001, laptop-002 (revoked)
$ for f in raft api wlc; do openssl rand -hex 32 > $f.secret; done
$ chmod 600 *.secret
$ sed -i.bak2 's/^address = "10.30.0.10"/address = "127.0.0.1"/' rradius.toml
$ rradiusd check --config rradius.toml
configuration ok
```

沒有先跑 `setup.sh` 時，`check` 會失敗：`rradius.toml: radius.eap.cert_file 'certs/server.pem': I/O error: No such file or directory (os error 2)`。

啟動伺服器（`rradiusd serve --config rradius.toml`），再開一個終端機。

**驗證：** 用 `rradius-bench` 扮演用戶端與 NAS：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat wlc.secret)" \
    --user laptop-001.corp.example.com --load eap-tls,concurrency=1 --requests 1 \
    --ca-file certs/ca.pem --tls-name radius.corp.example.com \
    --cert-file certs/laptop-001.pem --key-file certs/laptop-001.key
measured 0.004 s after 0.000 s of warmup

eap-tls (fixed-concurrency): concurrency 1, 1 threads, 1 sockets, busiest worker 29%
  eap-tls: 224/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    rounds per finished conversation: 6.0
    latency us: p50 4415.5, p90 4415.5, p99 4415.5, p99.9 4415.5, max 4415.5
```

EAP 負載的參數：

- `--user`：EAP Identity。EAP-TLS 時要跟憑證上的名稱一樣（見下一節的 `match_identity`）。
- `--cert-file`、`--key-file`：用戶端憑證與私鑰。`eap-tls` 一定要有，少了會得到 `rradius-bench: eap-tls needs --cert-file`。
- `--ca-file`：用來檢查伺服器憑證的 CA。不給的話，任何伺服器憑證都接受；測試時也應該給，才能確認伺服器送的是對的憑證。
- `--tls-name`：伺服器憑證要對得上的名稱。預設是 `--server` 的值，用 `127.0.0.1` 連線時要明寫成憑證上的 `radius.corp.example.com`。
- `--tls-version 1.3`：改用 TLS 1.3（只有 `eap-tls`）。已實測，同樣 `accept 1`、6 輪。

伺服器日誌：

```text
… INFO rradius_auth: authentication protocol="radius" client="wlc" source=127.0.0.1 user="laptop-001.corp.example.com" account="laptop-001.corp.example.com" chain="eap" handler="eap-tls" decision="accept"
```

再試撤銷的 `laptop-002`：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat wlc.secret)" \
    --user laptop-002.corp.example.com --load eap-tls,concurrency=1 --requests 1 \
    --ca-file certs/ca.pem --tls-name radius.corp.example.com \
    --cert-file certs/laptop-002.pem --key-file certs/laptop-002.key
rradius-bench: eap-tls: TLS: received fatal alert: CertificateRevoked
…
  eap-tls: 0/s, sent 1, accept 0, reject 0, challenge 0, accounting 0, timeouts 0, invalid 1, late 0, socket errors 0, retransmits 0
…
```

`laptop-002` 在資料表裡還是啟用的，擋下它的是 CRL：握手時 rradius 送出 TLS 警示 `CertificateRevoked`，驗證鏈根本沒有被問到。

> **注意：** `rradius-bench` 收到 TLS 警示就停下來，算成 `invalid 1`，不會送最後那一輪確認。真的用戶端會確認警示，然後收到 Access-Reject 與 EAP-Failure。所以用 `rradius-bench` 測這種情況時，伺服器上的對話要等 `session_timeout_ms`（30 秒）之後才清掉，算成 `result="timeout"`，而不是 `reject`。

最後用 `user` 指令停用 `laptop-001`：

```console
$ rradiusd user disable --config rradius.toml laptop-001.corp.example.com
disabled laptop-001.corp.example.com
$ rradius-bench run --server 127.0.0.1 --secret "$(cat wlc.secret)" \
    --user laptop-001.corp.example.com --load eap-tls,concurrency=1 --requests 1 \
    --ca-file certs/ca.pem --tls-name radius.corp.example.com \
    --cert-file certs/laptop-001.pem --key-file certs/laptop-001.key
…
  eap-tls: 520/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradiusd user enable --config rradius.toml laptop-001.corp.example.com
enabled laptop-001.corp.example.com
```

這次握手成功，是 `certificate_query` 查不到啟用的設備，所以拒絕。日誌：

```text
… WARN rradius_core::lua_engine: EAP conversation with client wlc refused: not authorized
… INFO rradius_auth: authentication protocol="radius" client="wlc" source=127.0.0.1 user="laptop-001.corp.example.com" account="laptop-001.corp.example.com" chain="eap" handler="eap-tls" decision="reject"
```

**結果長什麼樣子：** 憑證要同時通過兩關才能登入：TLS 握手（CA、有效期、CRL），以及之後的授權（資料表、目錄或腳本）。前者失敗時用戶端收到 TLS 警示；後者失敗時日誌寫 `refused: not authorized`。

## 憑證通過之後的授權

### 憑證上的哪個名稱

握手成功之後，rradius 從用戶端憑證取出登入用的名稱：

1. 先看主體別名（subject alternative name）裡的 DNS 名稱與 email 名稱。
2. 兩者都沒有時，才用 common name（CN）。有 DNS 或 email 名稱時，CN 不算，因為只檢查別名的 CA 可能讓申請者自己填 CN。
3. 別名讀不出來（例如有一個不是文字的 DNS 名稱），整張憑證拒絕，不會退回用 CN。

`match_identity = true`（預設）時，用戶端報的 EAP Identity 必須是其中一個名稱，大小寫不計。對不上時拒絕，驗證鏈不會跑：

```text
… WARN rradius_core::lua_engine: EAP conversation with client wlc refused: the EAP Identity does not fit the certificate
… INFO rradius_auth: authentication protocol="radius" client="wlc" source=127.0.0.1 user="alice" account="" chain="eap" handler="eap-tls" decision="reject"
```

（這是用 `laptop-001` 的憑證、`--user alice` 實測的結果。紀錄裡的 `user` 是 NAS 送來的 RADIUS `User-Name`；拒絕的原因那一行不會寫出 EAP Identity，因為它來自外部，可能含有控制字元。）

有些用戶端的 EAP Identity 固定是 `anonymous` 之類的值。這時設 `match_identity = false`，名稱改用憑證上的第一個名稱。實測：同一張 `laptop-001` 憑證、`--user anonymous`，`match_identity = true` 時拒絕；改成 `false`、送 SIGHUP 之後接受，紀錄是 `user="anonymous" account="laptop-001.corp.example.com"`。

名稱照憑證上的寫法交給驗證鏈。驗證鏈與腳本看到的 `User-Name` 就是這個名稱，不是用戶端報的 Identity。

### 用 sql 處理器決定

sql 處理器遇到憑證登入時：

- 有 `certificate_query` 就用它查，查到一列就接受；沒有就用 `user_query`。
- 只用 `user_query` 時，查到的那一列要有密碼值才接受：沒有密碼常被用來表示帳號停用，憑證登入也一樣看待。不想替設備存密碼時，寫 `certificate_query`。
- `default_user` 不適用：憑證不能用一個不存在的名稱登入。
- `reply_query` 照常執行，結果放進 Access-Accept。

`enterprise-wifi` 情境把員工帳號與設備放在同一張 `accounts` 表，用 `kind` 分開：

```toml
# rradius.toml（節錄）
[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM accounts WHERE name = ? AND kind = 'user' AND enabled = 1"
certificate_query = "SELECT 1 FROM accounts WHERE name = ? AND kind = 'device' AND enabled = 1"
reply_query = "SELECT r.attribute, r.value FROM accounts a JOIN group_reply r ON r.grp = a.grp WHERE a.name = ?"
```

這樣設備的憑證不能冒用員工的帳號，員工的密碼也登入不了設備。

### 用 ldap 處理器決定

ldap 處理器要寫 `certificate_filter` 才會處理憑證登入，例如 `certificate_filter = "(uid={user})"`。找到項目、且符合 `required_groups` 時接受，不必 bind。沒有 `certificate_filter` 的 ldap 處理器遇到憑證登入會略過，並記一行 `ignoring a certificate login: this handler has no certificate_filter`。ldap 處理器的其他設定在[第 08 章](08-ldap-and-ad.md)。

### 沒有驗證鏈時用 Lua 腳本決定

設定裡沒有 `[auth]` 時，憑證通過之後由最上層的 Lua 腳本決定，腳本看得到 `eap_method` 與 `tls_subject`。例子見[在 Lua 腳本裡看 EAP](#在-lua-腳本裡看-eap)。

### 名稱正規化與憑證

`[auth.username]` 的 `strip_realm`、`lowercase` 也會套用在憑證的名稱上，但只允許改大小寫。正規化會去掉 realm 時，這個憑證登入會被拒絕：

```text
… WARN rradius_core::lua_engine: auth chain: rejecting a request from client wlc: normalization changes a certificate's name in more than case
```

這是在 `enterprise-wifi`（`strip_realm = "suffix"`）用一張 email 名稱是 `alice@corp.example.com` 的憑證實測的結果。憑證證明的是整個名稱，去掉 realm 就變成另一個帳號。所以資料表裡設備的名稱要存憑證上的完整名稱；DNS 名稱沒有 `@`，不受影響。

### Access-Accept 裡有什麼

接受時，Access-Accept 帶：

- EAP-Success 與 `MS-MPPE-Recv-Key`、`MS-MPPE-Send-Key`。
- 驗證鏈或腳本回的屬性，例如 VLAN。
- `User-Name`：驗證鏈或腳本沒有回 `User-Name` 時，rradius 放進憑證上的名稱，讓 NAS 在它的日誌和計費裡用同一個名稱。

回覆屬性只放在 Access-Accept，不會出現在中間的 Access-Challenge。

憑證登入不計入 `auth.lockout`：它沒有猜測的問題，被拒絕也不算一次失敗。

## 用 CRL 擋下撤銷的憑證

`crl_file` 是 PEM 格式的撤銷清單，可以放好幾份。設了它之後，握手時會檢查用戶端憑證鏈上的每一張憑證。

**一定要知道的三件事：**

1. **憑證鏈上每一層 CA 都要有 CRL。** 只放根 CA 的 CRL，而用戶端憑證是中繼 CA 簽的，握手會失敗。實測：用中繼 CA 簽一張憑證，`crl_file` 只有根 CA 的 CRL 時，用戶端收到 `UnknownCA` 警示；把根 CA 與中繼 CA 的 CRL 串成一個檔案之後，握手就通過了。
2. **更新 CRL 檔案之後要送 SIGHUP。** rradius 只在載入設定時讀 CRL。
3. **過期的 CRL 仍然照用。** CRL 過了 Next Update 也不會讓所有人都登入失敗，但新撤銷的憑證要等你換上新的清單才會被擋。要定期重產 CRL 並 reload。

撤銷一張憑證的流程，以 `wifi-certs` 的 `crl-db/` 為例（在 `certs/` 目錄執行）：

```console
$ openssl ca -config crl-db/ca.cnf -revoke laptop-001.pem
Using configuration from crl-db/ca.cnf
Adding Entry with serial number 72F38A3DEE757B6E6A37D73476289C204A1B85A5 to DB for /CN=laptop-001
Revoking Certificate 72F38A3DEE757B6E6A37D73476289C204A1B85A5.
Database updated
$ openssl ca -config crl-db/ca.cnf -gencrl -out ca.crl
Using configuration from crl-db/ca.cnf
$ kill -HUP <伺服器的 PID>
```

實測的順序與結果：

1. 產生新的 `ca.crl` 之後、送 SIGHUP 之前，`laptop-001` 還是 `accept 1`：新的清單還沒被讀進來。
2. 送 SIGHUP 之後，日誌出現 `configuration reloaded from rradius.toml`，`rradius_reload_total{result="ok"}` 加 1。
3. 再登入一次，`rradius-bench` 印出 `TLS: received fatal alert: CertificateRevoked`。

叢集裡每個節點都要放同一份新的 CRL，並各自送 SIGHUP。

> **注意：** 一段對話用它開始時的設定走完。一段對話最多只能拖到 `session_timeout_ms` 的四倍，所以舊的 CRL 不會因為用戶端一直講話而一直有效。

### 一次憑證登入要過幾關

```
握手：憑證是 client_ca_file 簽的嗎？過期了嗎？在 crl_file 裡嗎？
  ▼
名稱：EAP Identity 符合憑證上的名稱嗎？（match_identity）
  ▼
授權：certificate_query（或腳本）查得到這台設備嗎？
  ▼
Access-Accept
```

每一關都能擋下一台設備，生效速度不同：

| 做法 | 多快生效 | 適合 |
| --- | --- | --- |
| 資料表停用（例如 `enabled = 0`，由 `certificate_query` 判斷） | 下一次登入，整個叢集 | 日常的停用；只擋 rradius 這裡 |
| 撤銷清單（`crl_file`） | 新清單放上每個節點並送 SIGHUP 之後 | CA 會發 CRL，同一張憑證也用在別的系統時 |

（這張比較表來自舊版手冊，未重新量測。）CA 只提供線上查詢（OCSP）時需要企業版（見本章開頭的企業版說明）。

### 排程更新 CRL

CRL 有 Next Update，rradius 過期後仍照用（見上），所以要自己定期換新：

1. 用 cron 或 systemd timer 定期從 CA 下載 CRL，轉成 PEM（見[常見錯誤](#常見錯誤)的格式說明）。
2. 先下載到暫存檔，確認它是 `-----BEGIN X509 CRL-----` 開頭、檔案不是空的，再用 `mv` 換到 `crl_file` 的位置。下載失敗時保留舊檔，不要放一個空檔上去。空檔或沒有 CRL 的檔案，`check` 與 reload 都會拒絕（`crl_file '…' holds no certificate revocation list`），reload 失敗時伺服器繼續用舊的設定。
3. 送 SIGHUP，每個節點都要做。

這個排程本身（下載來源、CA 的 CRL 發行點）未實機驗證。

### 伺服器憑證用 ECDSA 還是 RSA

登入量大時，伺服器憑證改用 **ECDSA P-256**：每次完整握手的 CPU 大約是 RSA 2048 的三分之一（來源：舊版 `docs/usage.md` 的量測，本章未重新量測；基準列見[第 19 章](19-performance.md)）。

- **曲線用 P-256，不要用 P-384。** 有社群回報舊版 Android（7.0）只支援 P-256，P-384 的憑證鏈會失敗（非官方文件，未實機驗證）。CA 與中繼憑證也一樣。
- **伺服器一次只用一張憑證。** 換成 ECDSA 之前，先用各種設備各連一次。
- **用 OpenSSL 3 產生 EC 私鑰。** 舊版文件記載 macOS 內建的 LibreSSL 產生的 EC 私鑰 rradius 讀不了（`failed to parse private key`），本次未重新驗證。ECDSA 的產生指令是把 `-newkey rsa:2048` 換成 `-newkey ec -pkeyopt ec_paramgen_curve:prime256v1`。

### 監控

`rradius_eap_sessions_total{method="tls",result="reject"}` 突然增加，常是 CRL 少了一層 CA（見[常見錯誤](#常見錯誤)的 `UnknownRevocationStatus`），也可能是一批設備的憑證過期或被撤銷。

## 換發伺服器憑證

伺服器憑證過期之後，會檢查伺服器憑證的用戶端都會拒絕這個伺服器（用戶端的行為，本章未實機驗證），所以要在到期前換掉。`openssl x509 -in certs/server.pem -noout -enddate` 可以看到期日。

換發的步驟：

1. 用同一個 CA 簽一張新的伺服器憑證，名稱（例如 `DNS:radius.corp.example.com`）和用途（`serverAuth`）都跟舊的一樣。用戶端信任的是 CA 和名稱，不是那一張憑證，所以不必改用戶端的設定。
2. 把新的憑證與私鑰放到 `cert_file`、`key_file` 指的位置，私鑰權限設成 0600。
3. 執行 `rradiusd check --config rradius.toml`，確認是 `configuration ok`。私鑰跟憑證不是一對時，這裡就會看到 `does not fit cert_file`。
4. 送 SIGHUP。之後開始的對話用新的憑證；正在進行的對話用舊的走完。
5. 叢集裡每個節點都要換，並各自送 SIGHUP。

實測：伺服器執行中換上同一個 CA 簽的新憑證與私鑰，`check` 通過，送 SIGHUP 之後 EAP-TLS 登入照樣 `accept 1`。為了確認送出的真的是新憑證，又換上另一個 CA 簽的伺服器憑證再送 SIGHUP：之後 `rradius-bench --ca-file certs/ca.pem` 得到 `TLS: invalid peer certificate: BadSignature`，改用新 CA 的 `--ca-file` 才 `accept 1`。所以 SIGHUP 之後的新對話用的就是新憑證。

換 CA 就不一樣了：每個用戶端都要先信任新的 CA。過渡期間可以讓用戶端同時信任新舊兩個 CA，等全部換好再換伺服器憑證。

`client_ca_file` 可以放好幾張 CA 憑證。要讓兩個 CA 簽的裝置憑證都能登入（例如換 CA 的過渡期），把它們串在同一個檔案裡。用戶端憑證過期時，握手會失敗，跟撤銷一樣不會走到授權。

## PEAP 與 TTLS：用帳號密碼登入

**目標：** 用 `enterprise-wifi` 情境，讓員工以 PEAP 登入，設備以 EAP-TLS 登入，再加上 TTLS。

這個情境的 EAP 與用戶端設定：

```toml
# rradius.toml（節錄）
[auth.username]
strip_realm = "suffix"
lowercase = true

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

- 先提議 EAP-TLS；沒有憑證的設備用 Legacy Nak 改要 PEAP。
- 員工可能輸入 `Staff1@corp.example.com`，`[auth.username]` 把它變成資料表裡的 `staff1`。
- 示範帳號在 `migrations/2_demo_data.sql`：`staff1`（密碼 `staff1-password`，存成 `{nthash}`，啟用）、`staff2`（停用）、裝置 `laptop-001.corp.example.com`。

**設定：** 跟上一節一樣準備工作目錄，跑 `sh make-certs.sh`，換掉三個 `.secret`，把用戶端位址改成 `127.0.0.1`，`check` 之後啟動。

**驗證：**

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat wlc.secret)" \
    --user Staff1@corp.example.com --password staff1-password --load peap,concurrency=1 --requests 1 \
    --ca-file certs/ca.pem --tls-name radius.corp.example.com
…
  peap: 398/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    rounds per finished conversation: 10.0
…
```

PEAP 要 10 輪：rradius 先提議 EAP-TLS，用戶端回 Nak 改要 PEAP，再加上握手與通道裡的 MS-CHAPv2 與結果確認。

同樣的指令換幾種情況，實測結果：

| 測試 | 結果 | 日誌的原因 |
| --- | --- | --- |
| `staff1`，密碼錯 | `reject 1` | `failed: the chain did not accept the inner name` |
| `staff2`（停用），密碼對 | `reject 1` | 同上 |
| 裝置憑證 `--load eap-tls`，`--cert-file certs/device.pem` | `accept 1` | — |
| `staff1`，`--load ttls` | `reject 1`，只有 2 輪 | `failed: the peer asked only for methods this server does not offer` |

最後一列：這個用戶端沒有 `ttls`，所以用戶端的 Nak 要求被拒絕。

日誌裡的認證紀錄：

```text
… INFO rradius_auth: authentication protocol="radius" client="wlc" source=127.0.0.1 user="Staff1@corp.example.com" account="staff1" chain="eap" handler="eap-peap" decision="accept"
```

`user` 是 NAS 送來的外層名稱，`account` 是驗證鏈用的內層名稱經過正規化之後的結果。PEAP 與 TTLS 由內層名稱決定一切，外層名稱可以是 `anonymous@corp.example.com` 這種匿名值，不影響結果。

**加上 TTLS：** 改兩行，再 reload：

```console
$ rradiusd config diff rradius.toml.before rradius.toml
radius.clients[wlc].eap_methods: ["tls", "peap"] | ["tls", "peap", "ttls"]
radius.eap.methods: ["tls", "peap"] | ["tls", "peap", "ttls"]
$ kill -HUP <伺服器的 PID>
$ rradius-bench run --server 127.0.0.1 --secret "$(cat wlc.secret)" \
    --user staff1@corp.example.com --password staff1-password --load ttls,concurrency=1 --requests 1 \
    --ca-file certs/ca.pem --tls-name radius.corp.example.com
…
  ttls: 687/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    rounds per finished conversation: 6.0
…
```

（`rradius.toml.before` 是改之前另存的一份。）`rradius-bench` 的 `ttls` 負載用內層 PAP。

**結果長什麼樣子：** 同一台 NAS 上，設備用憑證、員工用密碼，都走同一條驗證鏈；`handler` 分別是 `eap-tls`、`eap-peap`、`eap-ttls`，`account` 是正規化之後的帳號。密碼錯或帳號停用時，日誌寫 `failed: the chain did not accept the inner name`。

### 通道裡發生什麼

**PEAP：**

- 只做 PEAP 版本 0，內層只有 EAP-MSCHAPv2，TLS 只用 1.2，不要求用戶端憑證。用戶端要求其他版本或只肯用 TLS 1.3 時，對話以 EAP-Failure 結束。
- 內層的 MS-CHAPv2 回應交給用戶端的驗證鏈。只有驗證鏈真的比對了密碼、回了 `MS-CHAP2-Success`，才算接受。腳本回 `accept` 不算，Lua 處理器也不能自己放 `MS-CHAP2-Success` 進去。
- 密碼錯時，用戶端在通道裡收到 MS-CHAPv2 Failure（`E=691`），再收到 EAP-Failure。

**TTLS：**

- 只做版本 0，TLS 只用 1.2，不要求用戶端憑證。
- 內層可以是 PAP（`User-Name`、`User-Password`）或 MS-CHAPv2（`MS-CHAP-Challenge`、`MS-CHAP2-Response`）。MS-CHAPv2 的 challenge 必須是從 TLS 通道匯出的值，用戶端不能自己挑。
- 內層 PAP 只有在接受它的處理器是「會比對密碼」的處理器時才算數：sql 處理器、ldap 處理器，或 `verifies_password = true` 的 lua 處理器。只檢查帳號是否啟用的腳本不算，日誌會寫 `the chain accepted the inner name without checking a password`。
- 內層名稱最長 253 位元組，PAP 密碼最長 128 位元組；超過時在驗證鏈之前就拒絕。內層出現不認得、又標了必要（M flag）的 AVP 時也拒絕。

**兩者共通：**

- 用戶端一定要有驗證鏈。沒有時拒絕，日誌寫 `this client has no chain to check a password with`。
- `default_user` 不適用於通道裡的密碼。
- 驗證鏈回的 MPPE 金鑰不會送出，送出的是從 TLS 通道推導的金鑰；其他回覆屬性照放進 Access-Accept。驗證鏈沒有回 `User-Name` 時，rradius 放進正規化之後的帳號名稱。
- 密碼錯誤在驗證鏈拒絕的當下就寫日誌與認證紀錄，不必等用戶端把對話走完；同一段對話不會記兩次。
- 通道裡的密碼登入照常計入 `auth.lockout`（[第 07 章](07-users-and-chains.md)）。
- 通道裡最多 8 輪。

> **未實機驗證：** TTLS 內層 MS-CHAPv2 只對照了程式碼與測試。`rradius-bench` 的 `ttls` 負載只送內層 PAP，所以本章沒有實際跑過內層 MS-CHAPv2。

## 密碼要怎麼存

PEAP 用的是 MS-CHAPv2，伺服器要能算出密碼的 NT hash 才能比對。所以 PEAP 的帳號，密碼要存成 `{nthash}` 或明文；bcrypt 這類單向雜湊無法用來比對 MS-CHAPv2。TTLS 的內層 PAP 收到的是明文密碼，任何格式都能比對。

實測：在 `enterprise-wifi` 加上 `[accounts]`，用兩種 `scheme` 各新增一個帳號：

```toml
# rradius.toml（加在檔案最後）
[accounts]
scheme = "nthash"
add = "INSERT INTO accounts (name, kind, password, grp) VALUES (?1, 'user', ?2, 'staff')"
passwd = "UPDATE accounts SET password = ?2 WHERE name = ?1 AND kind = 'user'"
show = "SELECT name, kind, grp, enabled FROM accounts WHERE name = ?1"
```

`staff3` 用 `scheme = "nthash"` 新增，`staff4` 把 `scheme` 改成 `"bcrypt"` 之後新增（`printf 'staff3-password\n' | rradiusd user add --config rradius.toml staff3`）。結果：

| 帳號 | 存放格式 | PEAP | TTLS（內層 PAP） |
| --- | --- | --- | --- |
| `staff3` | `nthash` | `accept 1` | `accept 1` |
| `staff4` | `bcrypt` | `reject 1` | `accept 1` |

選擇的原則：

- 要用 PEAP，就存 `nthash`。NT hash 沒有 salt，資料表外洩時跟明文差不多危險，要保護好叢集資料目錄。
- 只用 TTLS 內層 PAP 時，可以存 bcrypt 這類較安全的雜湊。
- 帳號在 LDAP 而且只能用 bind 檢查密碼時，bind 只認得明文密碼，所以只適用 TTLS 內層 PAP，不適用 PEAP。

`rradius_password_checks_total` 會依存放格式計數，例如上面的測試讓 `scheme="nthash"` 的 `ok` 增加。各種格式的說明在[第 07 章](07-users-and-chains.md)。

## 在 Lua 腳本裡看 EAP

EAP 登入走到驗證鏈或腳本時，腳本多了三個全域變數：

| 變數 | 何時有值 | 內容 |
| --- | --- | --- |
| `eap_method` | EAP 登入 | `"tls"`、`"peap"` 或 `"ttls"`；不是 EAP 時是 nil |
| `tls_subject` | `eap_method` 是 `"tls"` 時 | 用戶端憑證的主體，文字，照憑證上的順序，值依 RFC 4514 跳脫，例如 `CN=laptop-002` |
| `tls_subject_attrs` | 同上 | 一個表：屬性名稱對應到它的值的陣列（未跳脫），例如 `tls_subject_attrs.CN[1]` |

`get_attr('User-Name')` 拿到的是憑證上的名稱（EAP-TLS）或通道裡的名稱（PEAP、TTLS）。

下面的腳本在沒有 `[auth]` 的設定裡，讓 CN 以 `laptop-` 開頭的憑證登入，並放進一個 `Filter-Id`：

```lua
-- auth.lua：沒有 [auth] 時，EAP-TLS 憑證通過後由這個腳本決定
-- 這個示範不記錄計費：計費腳本回 'accept' 就代表已經記下來了，見第 13 章。
if phase ~= 'authentication' then
  return 'reject'
end
if eap_method == 'tls' then
  log('certificate', get_attr('User-Name'), tls_subject)
  local cn = tls_subject_attrs and tls_subject_attrs.CN and tls_subject_attrs.CN[1]
  if cn and cn:match('^laptop%-') then
    set_attr('Filter-Id', 'corp-devices')
    return 'accept'
  end
end
return 'reject'
```

實測（`config init` 底稿加上前面的 `conf.d/eap.toml`，沒有 `crl_file`）：用 `laptop-002` 的憑證登入得到 `accept 1`，日誌：

```text
… INFO rradius_core::lua_engine: script auth.lua (radius/authentication, client example): certificate laptop-002.corp.example.com CN=laptop-002
… INFO rradius_auth: authentication protocol="radius" client="example" source=127.0.0.1 user="laptop-002.corp.example.com" account="laptop-002.corp.example.com" chain="eap" handler="eap-tls" decision="accept"
```

注意這裡沒有設 `crl_file`，所以撤銷的 `laptop-002` 也進來了。腳本只能決定握手通過之後的事，撤銷要靠 CRL。

幾點限制：

- 這個腳本對 PEAP 與 TTLS 沒有用：它們的密碼要由驗證鏈檢查，所以設定裡一定要有 `[auth]`。
- 驗證鏈裡的 lua 處理器要替 TTLS 內層 PAP 比對密碼時，要設 `verifies_password = true`，並自己比對 `User-Password`。
- 腳本的其他寫法在[第 10 章](10-lua-policies.md)，完整 API 在[附錄 D](93-lua-reference.md)。

## 對話上限與逾時

每段還沒完成的對話都佔記憶體。任何人只要能透過 NAS 送一個 EAP Identity，就能開一段對話，所以 rradius 對它們設了上限：

| 情況 | rradius 怎麼做 | 計數（`rradius_eap_sessions_total` 的 `result`） |
| --- | --- | --- |
| `session_timeout_ms` 內沒有下一輪 | 丟掉這段對話，大約一秒內清掉 | `timeout` |
| 一直有下一輪，但已經超過 `session_timeout_ms` 的四倍 | 丟掉 | `timeout` |
| 一段對話收到第 257 個請求 | 回 EAP-Failure，丟掉 | `reject` |
| 全部的對話數到了 `max_sessions`，或這個用戶端的到了 `max_sessions_per_client` | 開新對話，丟掉一段舊的：先丟送完 Identity 之後就沒下文的，再丟最久沒動靜的 | `evicted` |
| 同一台裝置的未完成對話到了 `max_sessions_per_device` | 新的請求不回應，NAS 會重送 | `overload` |
| 同一台裝置送完 Identity 就沒下文，又重新開始 | 舊的那段換成新的 | `replaced` |

- 「同一台裝置」是同一個用戶端、同一個 `Calling-Station-Id`。請求沒有 `Calling-Station-Id` 時，分不出是哪台裝置，只有 `max_sessions` 與 `max_sessions_per_client` 有作用。
- 正在進行的對話每一輪都會被聽到，排在被丟掉的最後面，所以大量的假 Identity 擠不掉真的登入。
- 一段對話已經被記成拒絕之後，再逾時或被擠掉不會重複計數。

一般不必調這些值。要調時的參考：

- 一台控制器底下設備很多、同時登入的人多（例如上班時間），`max_sessions_per_client` 的 `1024` 不夠時調高。
- `session_timeout_ms` 太短，慢的用戶端或要等使用者按「信任憑證」的手機會在中途被丟掉。
- `fragment_size` 預設 `1000`。NAS 或中間的網路對大的 RADIUS 封包有問題時調小，最小 256。

`rradius_eap_sessions_open` 是目前未完成的對話數。

`rradius-bench` 本身也能用 EAP 負載量測吞吐量（例如 `--load eap-tls,concurrency=16`），那是[第 19 章](19-performance.md)的內容。量測時記得把 `max_sessions` 與 `max_sessions_per_client` 調得比同時進行的對話數大，原始碼的 `rradius-server/benchmarks/eap.toml` 就是這樣設的。

## 資料庫無法使用時

EAP 對話本身在記憶體裡，但最後那一步（驗證鏈查資料表）需要叢集資料庫。資料庫暫時不能用時（例如呼叫逾時；只讀的查詢在沒有 leader 時仍用本機副本回答，見第 16 章）：

- 這個請求不回應，`rradius_radius_discarded_total{reason="database"}` 加 1，日誌寫 `EAP request from client <名稱> not answered: …`。
- 對話留著。NAS 重送同一個 EAP-Response 時，rradius 重做那一步，不會把 TLS 資料再讀一次。
- 每次重送都算一輪，仍受 `session_timeout_ms` 與 256 輪的限制。

所以資料庫短暫中斷時，用戶端只是登入慢一點，不會被拒絕；中斷太久，對話逾時，用戶端要重新開始。

> **未實機驗證：** 這一段只對照了程式碼與 spec，沒有在實際的資料庫故障下跑過。叢集沒有 leader 時的整體行為見[第 16 章](16-cluster.md)。

## EAP 的指標與日誌

`/metrics` 的 EAP 指標（實測，名稱與標籤照抄）：

```text
rradius_eap_sessions_open 0
rradius_eap_sessions_total{method="tls",result="accept"} 1
rradius_eap_sessions_total{method="tls",result="reject"} 1
rradius_eap_sessions_total{method="peap",result="accept"} 2
rradius_eap_sessions_total{method="peap",result="reject"} 3
rradius_eap_sessions_total{method="ttls",result="accept"} 3
rradius_eap_sessions_total{method="ttls",result="reject"} 1
…
```

`method` 是 `tls`、`peap`、`ttls`，`result` 是 `accept`、`reject`、`timeout`、`overload`、`replaced`、`evicted` 六種（意思見上一節），每種組合都會出現，值為 0 的也一樣。一段對話只算一次。

排錯時怎麼看：

- `reject` 一直增加：多半是密碼錯、帳號停用或授權不過。看日誌的原因。
- `timeout` 一直增加：用戶端開了對話卻沒走完。常見原因是用戶端不信任伺服器憑證（它直接放棄），或 NAS 與 rradius 之間掉封包。
- `overload`、`evicted` 增加：對話數碰到上限，見上一節。

`status` 有一個 EAP 區段，顯示未完成的對話數：

```console
$ rradiusd status --config rradius.toml
…
EAP
  Sessions under way  1
…
```

（這是在 `wifi-certs` 用 `rradius-bench` 測過撤銷憑證之後看到的：那段對話在等 30 秒逾時。）

日誌裡跟 EAP 有關的幾種行，都是 WARN：

| 開頭 | 意思 |
| --- | --- |
| `EAP conversation with client <名稱> refused: <原因>` | 握手成功，但授權不過，例如 `not authorized`、`the EAP Identity does not fit the certificate` |
| `EAP conversation with client <名稱> failed: <原因>` | 對話中途失敗，例如密碼錯、方法不允許、TLS 握手失敗 |
| `EAP request from client <名稱> not answered: <原因>` | 資料庫無法使用，請求沒有回應，對話等 NAS 重送 |

每段結束的對話另有一行 `rradius_auth` 認證紀錄。日誌不會寫出 MSK 或 TLS 的內容，也不會寫出通道裡的密碼。指標與日誌的完整說明在[第 17 章](17-monitoring.md)與[附錄 C](92-metrics-reference.md)。

## 接上真的 NAS 與用戶端

本章的測試都用 `rradius-bench` 同時扮演 NAS 與用戶端。接上真的設備時：

1. **NAS：** 在 AP 控制器或交換器上把 RADIUS 伺服器指到 rradius 的認證埠（預設 1812），共用密碼與 `secret_file` 一樣；用戶端的 `address` 改回 NAS 真正的位址。計費照[第 13 章](13-accounting.md)設定。
2. **用戶端：** 設定信任簽發伺服器憑證的 CA，並指定伺服器名稱（範例是 `radius.corp.example.com`）。EAP-TLS 另外安裝裝置憑證與私鑰。設備多時，用 MDM 或群組原則（GPO）把 CA 憑證、伺服器名稱與 Wi-Fi 設定一起推到裝置上，不要靠使用者手動按「信任」；沒有預先設定時，使用者會一直被問是否信任這張憑證，或乾脆選擇不驗證。這一步是各作業系統與管理系統的操作，本書沒有測過。
3. **VLAN：** NAS 要支援依 `Tunnel-Private-Group-Id` 指派 VLAN，範例的回覆屬性才有作用。

`enterprise-wifi` 情境的 `eapol/` 目錄附了兩份 `eapol_test` 設定（`tls.conf`、`peap.conf`），可以在一台 Linux 上模擬用戶端。`eapol_test` 是 wpa_supplicant 附的測試工具，不在 repo 裡。

> **警告：** 用戶端一定要驗證伺服器憑證。不驗證的話，假的基地台可以冒充你的網路，拿到 PEAP 的 MS-CHAPv2 回應，再離線猜密碼。

> **未實機驗證：** 本章沒有用真的 AP、交換器、`eapol_test` 或各作業系統內建的 802.1X 用戶端測試過。上面的步驟與 `eapol/` 的設定只對照了情境檔案與程式碼。

## 常見錯誤

**症狀：** `check` 印出 `rradius.toml: radius.eap.cert_file 'certs/server.pem': I/O error: No such file or directory (os error 2)`。
**原因：** 憑證還沒產生，或路徑不對。路徑相對於主設定檔所在的目錄。
**解法：** 先跑情境的 `make-certs.sh` 或 `setup.sh`，或改成正確的路徑。

**症狀：** `check` 印出 `radius.eap.client_ca_file is required`。
**原因：** `methods` 有 `tls`（預設就是 `["tls"]`），卻沒有 `client_ca_file`。
**解法：** 加上簽發用戶端憑證的 CA；只用 PEAP 或 TTLS 時，把 `methods` 寫明，不要用預設值。

**症狀：** `check` 印出 `radius.eap.crl_file only applies to tls, which methods does not have`（`client_ca_file` 也有同樣的訊息）。
**原因：** 方法裡沒有 `tls`，這些設定不會有作用。
**解法：** 刪掉它們，或在 `methods` 加上 `tls`。

**症狀：** `check` 印出 `radius.eap.key_file 'certs/other.key' does not fit cert_file 'certs/server.pem': keys may not be consistent: KeyMismatch`。
**原因：** 私鑰跟憑證不是一對。
**解法：** 換成簽發這張憑證時用的私鑰。

**症狀：** `check` 印出 `radius.eap.client_ca_file '…' holds no certificate` 或 `radius.eap.crl_file '…' holds no certificate revocation list`。
**原因：** 檔案放錯了，或不是 PEM 格式。
**解法：** 確認檔案內容是 `-----BEGIN CERTIFICATE-----` 或 `-----BEGIN X509 CRL-----`。DER 格式要先用 `openssl x509` 或 `openssl crl` 轉成 PEM。

**症狀：** `check` 印出 `radius.eap.methods has a method whose password only a chain can check, and there is no [auth] section`。
**原因：** 用了 `peap` 或 `ttls`，卻沒有驗證鏈。
**解法：** 加上 `[auth]` 與會比對密碼的處理器（[第 07 章](07-users-and-chains.md)）。

**症狀：** `check` 印出 `radius client 'example': eap_methods is required when radius.eap.methods has more than one method; [] for a client without EAP`。
**原因：** 提供兩種以上的方法時，每個用戶端都要寫 `eap_methods`。
**解法：** 替每個用戶端寫上它的方法，不用 EAP 的寫 `[]`。

**症狀：** `check` 印出 `radius client 'example': eap_methods has a method that radius.eap.methods does not`，或 `eap_methods needs [radius.eap]`。
**原因：** 用戶端列了伺服器沒有提供的方法，或根本沒有 `[radius.eap]`。
**解法：** 讓 `eap_methods` 是 `radius.eap.methods` 的子集。

**症狀：** `check` 印出 `radius.eap.fragment_size must be from 256 to 1400, not 1500`、`radius.eap.session_timeout_ms must be greater than 0`，或 `radius.eap.max_sessions must be greater than 0`。
**原因：** 值超出範圍。
**解法：** 照訊息改。

**症狀：** `check` 印出 `radius.eap.<鍵>: not supported by this program (… needs rradius-enterprise)`，或 `radius.eap.methods: <方法> is not supported by this program (… needs rradius-enterprise)`。
**原因：** 設定裡有企業版才有的鍵或方法。哪些鍵會這樣被拒絕，列在[附錄 A](90-config-reference.md) 的「核心不支援或沒有作用的設定」。
**解法：** 在 `rradiusd` 的設定裡刪掉它們；方法只用 `tls`、`peap`、`ttls`。

**症狀：** `check` 印出 `radius.eap.methods: 'md5' is not a method this server offers; it has tls, peap and ttls`。
**原因：** 核心只有 `tls`、`peap`、`ttls` 三種方法。
**解法：** 改用這三種之一。

**症狀：** `rradius-bench` 印出 `TLS: received fatal alert: CertificateRevoked`。
**原因：** 用戶端憑證在 `crl_file` 裡。
**解法：** 這是預期的結果。憑證不該被撤銷的話，確認 CRL 是不是最新的那一份。

**症狀：** 用中繼 CA 簽的憑證一律握手失敗，用戶端收到 `UnknownCA`，但拿掉 `crl_file` 就能登入。
**原因：** `crl_file` 只有根 CA 的 CRL。憑證鏈上每一層 CA 都要有 CRL。
**解法：** 把每一層 CA 的 CRL 串進同一個 `crl_file`，再送 SIGHUP。

**症狀：** 設了 `crl_file` 之後所有設備都被拒絕，日誌的原因寫 `invalid peer certificate: UnknownRevocationStatus`。
**原因：** 憑證鏈上某一層 CA 沒有對應的 CRL（沒有放進 `crl_file`，或放了別的 CA 的）。程式沒有開啟「查不到撤銷狀態就放行」，所以查不到就拒絕（`rradius-core/src/common/tls.rs` 只呼叫 `with_crls`，沒有 `allow_unknown_revocation_status`）。用戶端收到的警示隨憑證鏈而不同，上一則實測的中繼 CA 例子是 `UnknownCA`；`UnknownRevocationStatus` 這句本身取自函式庫的錯誤型別，未另外實機重現。
**解法：** 把根 CA 與每一層中繼 CA 的 CRL 串進同一個 `crl_file`，`check` 之後送 SIGHUP。

**症狀：** 換了新的 CRL，撤銷的憑證還是能登入。
**原因：** 還沒 reload。
**解法：** 送 SIGHUP，確認日誌有 `configuration reloaded`。

**症狀：** 日誌寫 `refused: the EAP Identity does not fit the certificate`。
**原因：** 用戶端報的 EAP Identity 不是憑證上的 DNS 或 email 名稱（有這兩種名稱時，CN 不算）。
**解法：** 讓用戶端的 Identity 用憑證上的名稱；用戶端只會報匿名名稱時，設 `match_identity = false`。

**症狀：** 日誌寫 `refused: not authorized`。
**原因：** 握手成功，但驗證鏈或腳本沒有接受這個名稱。sql 處理器常見的原因是 `certificate_query` 查不到、設備停用，或沒有 `certificate_query` 而 `user_query` 查到的密碼是空的。
**解法：** 用日誌裡的 `account` 名稱去查資料表。

**症狀：** 日誌寫 `auth chain: rejecting a request from client wlc: normalization changes a certificate's name in more than case`。
**原因：** `[auth.username]` 的 `strip_realm` 會去掉憑證名稱的 realm。
**解法：** 資料表存憑證上的完整名稱，並讓憑證登入走另一條沒有 `strip_realm` 的驗證鏈，或改用 DNS 名稱的憑證。

**症狀：** PEAP 一律 `reject`，同一個帳號用 TTLS 內層 PAP 可以登入。
**原因：** 密碼存成 bcrypt 之類的單向雜湊，無法比對 MS-CHAPv2。
**解法：** PEAP 的帳號存 `{nthash}`（`accounts.scheme = "nthash"`），或改用 TTLS。

**症狀：** 日誌寫 `failed: the peer asked only for methods this server does not offer`。
**原因：** 用戶端要的方法不在這個用戶端的 `eap_methods` 裡（或用戶端只要 EAP-MD5）。
**解法：** 在 `radius.eap.methods` 與用戶端的 `eap_methods` 加上那個方法，或改用戶端的設定。

**症狀：** 日誌寫 `failed: the chain accepted the inner name without checking a password`。
**原因：** TTLS 內層 PAP 只被一個沒有比對密碼的 lua 處理器接受。
**解法：** 加上 sql 或 ldap 處理器，或讓 lua 處理器自己比對密碼並設 `verifies_password = true`。

**症狀：** `rradius-bench` 印出 `rradius-bench: --no-message-authenticator: EAP loads always carry Message-Authenticator (RFC 3579)`，結束碼 3。
**原因：** EAP 負載一定帶 `Message-Authenticator`；伺服器也會丟掉沒有它的 EAP 請求。
**解法：** EAP 負載不要加 `--no-message-authenticator`。

**症狀：** 用戶端一直重試，`rradius_eap_sessions_total{result="timeout"}` 一直增加，日誌沒有 `refused` 或 `failed`。
**原因：** 用戶端在 TLS 握手時放棄，最常見的是它不信任伺服器憑證、或伺服器名稱對不上。
**解法：** 檢查用戶端的 CA 設定與伺服器名稱；用 `rradius-bench --ca-file … --tls-name …` 確認伺服器送出的憑證驗得過。

## 相關章節

- [03 第一次啟動與登入](03-quick-start.md)
- [05 RADIUS 服務與 NAS 用戶端](05-radius-clients.md)
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)
- [08 接上 LDAP 與 Active Directory](08-ldap-and-ad.md)
- [10 用 Lua 寫政策](10-lua-policies.md)
- [13 計費與 session](13-accounting.md)
- [15 情境實作：組織網路存取](15-access-recipes.md)
- [16 叢集與高可用](16-cluster.md)
- [17 監控、狀態與日誌](17-monitoring.md)
- [19 效能調校與壓測](19-performance.md)
- [附錄 A：設定參考](90-config-reference.md)
- [附錄 C：指標參考](92-metrics-reference.md)
- [附錄 D：Lua API 參考](93-lua-reference.md)
