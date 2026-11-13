# 8. 憑證的撤銷

**情境：** [第 3 章](03-enterprise-wifi.md)的企業 Wi-Fi 上線一陣子之後，常會遇到：

- **筆電遺失了。** 它的憑證還有兩年才到期。在資料表停用可以擋下登入，但資安政策要求憑證本身也要撤銷，而且同一張憑證可能還用在別的系統。

這一章在憑證登入（EAP-TLS）上加撤銷清單（CRL）。CA 只提供線上查詢（OCSP），或要讓漫遊的設備恢復上一次的 session（session resumption）、不必每次重做完整握手時，需要 rradius-enterprise，見企業版手冊第 8 章。

範例目錄：[`rradius-server/examples/scenarios/wifi-certs/`](../../rradius-server/examples/scenarios/wifi-certs/)

| 檔案 | 內容 |
| --- | --- |
| `rradius.toml` | `[radius.eap]` 的 `crl_file` |
| `accounting.lua` | 計費腳本，和[第 2 章](02-quick-start.md#accountinglua-做了什麼)說明的相同 |
| `setup.sh` | 產生示範用的 CA、伺服器憑證、兩張筆電憑證（一張已撤銷）和撤銷清單 |
| `migrations/` | `devices`（設備和 VLAN）、`accounting`，和兩台示範的筆電 |

## 一次憑證登入要過幾關

```
握手：憑證是 client_ca_file 簽的嗎？過期了嗎？在 crl_file 裡嗎？
  ▼
名稱：EAP Identity 符合憑證上的名稱嗎？
  ▼
認證串接：certificate_query 查得到這台設備嗎？
  ▼
Access-Accept
```

每一關都能擋下一台設備，快慢不同：

| 做法 | 多快生效 | 適合 |
| --- | --- | --- |
| 資料表停用（`enabled = 0`） | 下一次登入，整個叢集 | 日常的停用。只擋 rradius 這裡 |
| 撤銷清單（CRL） | 新的清單放上每個節點並送 SIGHUP 之後 | CA 會發 CRL |

## 步驟

### 1. 產生憑證和金鑰

```bash
cd rradius-server
examples/scenarios/wifi-certs/setup.sh
```

在情境目錄裡產生 `certs/`（CA、伺服器憑證、`laptop-001`、已撤銷的 `laptop-002`、`ca.crl`）。**只供測試。** 已經有的檔案不會覆蓋。

### 2. 啟動

本機測試時把 `wlc` 的 `address` 改成 `127.0.0.1`。

```bash
./target/release/rradius-server server --config examples/scenarios/wifi-certs/rradius.toml
```

### 3. 測試

用 `eapol_test`（[第 3 章](03-enterprise-wifi.md#3-用-eapol_test-測試)有安裝方式），設定檔照第 3 章的 `eapol/tls.conf`，把憑證換成這裡的 `certs/laptop-001.pem`、`certs/laptop-001.key`，`identity` 換成 `laptop-001.corp.example.com`。本章沒有另外附 `eapol_test` 的設定檔，也沒有用它跑過這個情境。

自動測試自己扮演筆電，走完整段 EAP-TLS，確認：

| 情況 | 結果 |
| --- | --- |
| `laptop-001` 第一次登入 | Access-Accept，VLAN 10，完整握手 |
| `laptop-001` 再登入一次 | Access-Accept，又是完整握手（rradius-server 不恢復 session） |
| `laptop-002`（在撤銷清單裡） | Access-Reject |
| `laptop-001` 的憑證，但自稱是 `laptop-002` | Access-Reject：名稱和憑證不符 |
| `user disable laptop-001.corp.example.com` 之後 | Access-Reject |
| `user enable ...` 之後 | Access-Accept |

## 撤銷清單（CRL）

```toml
[radius.eap]
client_ca_file = "certs/ca.pem"
crl_file = "certs/ca.crl"
```

- **列在清單裡的憑證握手就失敗。**
- **更新清單後要送 SIGHUP** 才會重讀；每個節點都要更新檔案並送信號。新的對話用新清單，進行中的對話用舊的做完（最多兩分鐘）。
- **清單要涵蓋憑證鏈上每一層 CA。** 有中繼 CA 時，把根 CA 和中繼 CA 的 CRL 串接放進同一個檔案（PEM）。缺一層時所有設備都會被拒絕，記錄寫 `UnknownRevocationStatus`。
- **清單過期時 EAP-TLS 照用，** 所以要自己確保定期更新；過期後才撤銷的憑證會漏掉。
- **排程更新：** 用 cron 或 systemd timer 定期從 CA 下載 CRL、轉成 PEM、放到 `crl_file` 的位置，再送 SIGHUP。下載失敗時保留舊檔，不要放一個空檔上去。

範例的 `setup.sh` 示範了怎麼用 `openssl ca` 撤銷一張憑證並產生 CRL，檔案開頭的說明有之後再撤銷一張的指令。

## 其他省 CPU 的做法

登入量大時，伺服器憑證改用 **ECDSA P-256**：在同樣的機器上，每次完整握手用的 CPU 大約是 RSA 2048 的三分之一。曲線用 P-256，不要用 P-384（有舊版 Android 不支援的回報）。伺服器一次只用一張憑證，換之前先用各種設備連一次確認。量測和相容性的說明見 [usage 2.12 節](../usage.md#212-eap-radiuseap)。

## 上線前

- **監控：** `rradius_eap_sessions_total{method="tls",result="reject"}` 突然增加，可能是 CRL 少了一層。
- **沒有測過的：**
  - **實體基地台和各作業系統。**
  - `setup.sh` 產生憑證和撤銷清單的部分，在 LibreSSL 3.3（macOS）和 OpenSSL 3.5（Linux 容器）上都跑過。

## 常見問題

| 現象 | 原因 |
| --- | --- |
| 設了 `crl_file` 之後所有設備都被拒絕，記錄有 `UnknownRevocationStatus` | 缺少某一層 CA 的 CRL，或放了別的 CA 的 CRL |
| 撤銷了還是能登入 | 新清單還沒放到那個節點，或沒送 SIGHUP |
