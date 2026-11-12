# rradius 使用手冊

這份手冊依常見的部署情境，一步一步說明怎麼設定 rradius。每個情境都附一組可以直接啟動的設定檔、資料表和 Lua 腳本，放在 [`rradius-server/examples/scenarios/`](../../rradius-server/examples/scenarios/)。所有欄位的完整說明在[參考文件 `docs/usage.md`](../usage.md)，手冊只講情境需要的部分，並標出對應的章節。

範例都有自動測試（`rradius-server/tests/scenarios_test.rs`）：測試會照手冊的方式啟動每個情境，送出登入、計費和 TACACS+ 請求，確認結果和手冊寫的一樣。所以範例改壞了，測試會失敗。測試做不到的部分（實體設備、真的漫遊聯盟、外部的服務），各章的「上線前」會寫明沒有測過什麼。

## 先讀這兩章

| 章 | 內容 |
| --- | --- |
| [1. 基本概念](01-concepts.md) | rradius 怎麼處理一個請求、設定檔的結構、資料放在哪裡、怎麼測試 |
| [2. 快速開始](02-quick-start.md) | 單機、帳號存在資料表，十分鐘內收到第一個 Access-Accept |

## 我要做什麼，看哪一章

| 我要… | 章 |
| --- | --- |
| 讓員工用帳號密碼或憑證連公司 Wi-Fi | [3](03-enterprise-wifi.md)，撤銷憑證在 [8](08-wifi-certs.md) |
| 讓印表機、IP 電話這類設備上有線網路 | [4](04-wired-mab.md) |
| 讓 VPN 用公司的目錄登入，或加上驗證碼 | [5](05-remote-access.md) |
| 管理誰能登入路由器、能下哪些指令 | [6](06-device-admin.md) |
| 任何一台停機都不中斷服務 | [7](07-ha-cluster.md) |
| 帳號已經在別的系統裡，不想複製 | 目錄（LDAP／AD）看 [5](05-remote-access.md)，HTTP API 看 [10](10-external-api.md) |
| 從舊的認證系統搬過來，不想讓大家重設密碼 | [9](09-migrate-users.md) |
| 上線之後的日常工作 | [11](11-operations.md) |
| 出了問題，從現象找原因 | [12](12-troubleshooting.md) |

寬頻業者的方案和踢人、依 realm 轉送和漫遊聯盟、RadSec、每人一把 Wi-Fi 密碼、用 SIM 卡登入、從資料表管理幾百台 NAS、OCSP 和快速重新連線、TACACS+ over TLS 需要 rradius-enterprise，見企業版手冊。

## 依情境

| 章 | 情境 | 用到的功能 | 範例目錄 |
| --- | --- | --- | --- |
| [3. 企業 Wi-Fi](03-enterprise-wifi.md) | 802.1X：公司電腦用憑證，員工手機用帳號密碼，依群組分 VLAN | EAP-TLS、PEAP、`certificate_query`、動態 VLAN | `enterprise-wifi/` |
| [4. 有線網路 MAC 認證](04-wired-mab.md) | 印表機、IP 電話這類不會 802.1X 的設備 | Lua 腳本、網段 client | `wired-mab/` |
| [5. 遠端存取](05-remote-access.md) | VPN 用公司目錄（LDAP／AD）登入，跳板機加 TOTP 驗證碼 | LDAP、群組、具名串接、Access-Challenge | `remote-access/` |
| [6. 網路設備管理](06-device-admin.md) | 管理員登入路由器，依角色限制指令並記錄 | TACACS+ 認證、授權、計費、授權群組 | `device-admin/` |
| [7. 高可用叢集](07-ha-cluster.md) | 三個節點，任一台停掉都能繼續服務 | hiqlite 叢集、WireGuard、migration | `ha-cluster/` |
| [8. 憑證的撤銷](08-wifi-certs.md) | 遺失的筆電要撤銷 | CRL | `wifi-certs/` |
| [9. 從舊系統搬帳號](09-migrate-users.md) | 舊的密碼雜湊原樣搬過來，之後逐步換成新格式 | 各種密碼格式、名稱正規化、`[accounts]` | `migrate-users/` |
| [10. 帳號在別的系統](10-external-api.md) | 每次登入用 HTTPS 問會員系統的 API | `[lua.http]`、`http_request`、`verifies_password` | `external-api/` |

## 上線之後

| 章 | 內容 |
| --- | --- |
| [11. 日常維運](11-operations.md) | 帳號和設定的變更、重新載入、更換密碼和金鑰、監控指標、過載、計費暫存、升級、備份 |
| [12. 依症狀排查](12-troubleshooting.md) | 沒有回應、被拒絕、很慢、計費不見、啟動失敗，各從哪裡看起 |

## 範例的共同約定

- **設定檔裡的相對路徑以設定檔所在目錄為準，** 所以每個情境都能直接在 repo 裡啟動：

  ```bash
  cd rradius-server
  cargo build --release
  ./target/release/rradius-server --config examples/scenarios/quick-start/rradius.toml
  ```

- **用發行檔時，** 解開 `rradius-<版本>-<平台>.tar.gz`，裡面的目錄結構和 repo 相同。進到其中的 `rradius-server/` 目錄，把各章的 `./target/release/rradius-server` 換成 `../bin/rradius-server`，其他指令和路徑都一樣，也不用 `cargo build`：

  ```bash
  cd rradius-0.1.0-linux-x86_64/rradius-server
  ../bin/rradius-server --config examples/scenarios/quick-start/rradius.toml
  ```

- **有些情境要先執行一個腳本才能啟動，** 因為憑證和金鑰不該放在 repo 裡：

  | 腳本 | 情境 | 產生什麼 |
  | --- | --- | --- |
  | `make-certs.sh` | `enterprise-wifi/` | `certs/` 裡示範用的 CA 和憑證 |
  | `setup.sh` | `wifi-certs/` | 示範用的 CA、憑證和撤銷清單 |

  這些都只供測試。`setup.sh` 不會覆蓋已經有的檔案；要從頭再來，停止伺服器後刪掉它產生的檔案和 `data/`。

- **密碼檔（`*.secret`）都是示範值，** 內容寫明 `change-me`。正式環境一定要換掉，見[第 11 章](11-operations.md#更換密碼)。
- **示範資料放在 `migrations/2_demo_*.sql`**（搬帳號的情境是 `2_imported_users.sql`），結構放在 `1_schema.sql`。正式環境請刪掉示範資料的檔案，或在第一次啟動前換成自己的資料。檔案一旦套用就不能再改，見[第 1 章](01-concepts.md#資料表和-migration)。
- **埠號用標準值**（1812、1813、49）。在同一台機器上同時啟動兩個情境會衝突，一次啟動一個，或改埠。
- **啟動後產生的 `data/`、`spool/`、`certs/` 和金鑰檔不進 git。** 要從頭再來一次，停止伺服器後刪掉 `data/`。
- **NAS 的位址都是示範用的私有網段。** 本機測試時要把 client 的 `address` 改成 `127.0.0.1`（快速開始已經是），否則請求會被當成不認得的來源而沒有回應。
- **每個情境自成一套：** 設定、腳本和資料表都在情境自己的目錄裡（幾個情境的 `accounting.lua` 內容相同，各放一份），整個目錄複製到別處就能用。唯一指到目錄外的是字典（`dictionary = "../../../../share/dictionary"`），複製到別處時改成字典實際的位置。
