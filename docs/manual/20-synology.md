# 20. 在 Synology NAS 上用 Docker 執行

**情境：** 小型辦公室或分店沒有伺服器，只有一台 Synology NAS，想用它當 RADIUS 伺服器，讓 Wi-Fi 或有線網路的設備向它認證。rradius 沒有做成 Synology 的套件（SPK），這一章用 Container Manager（DSM 6 到 7.1 叫 Docker）跑官方 Dockerfile 做出的映像，一台、一個節點。

範例目錄：[`deploy/synology/`](../../deploy/synology/)

| 檔案 | 內容 |
| --- | --- |
| `compose.yaml` | Container Manager 的「專案」：映像、兩個 RADIUS 埠、設定與密鑰的資料夾、資料磁碟 |

設定檔用 repo 的 [`docker/config/`](../../docker/config/)（內建資料表、一個計費腳本、第 2 章的做法），密鑰用 `docker/init-secrets.sh` 產生。要換成別的情境，把那個資料夾換成該情境的設定檔即可。

**這一章沒有在 Synology 上測過：** 映像的建置與啟動、`compose.yaml` 的資料夾結構，是在一般的 Linux Docker 上驗證的（見下方「驗證過什麼」）；DSM 的畫面、防火牆和各機型的支援情況沒有實機驗證，寫的是 Synology 文件與 Docker 的一般行為。

## 先確認

| 項目 | 說明 |
| --- | --- |
| 機型 | 要能裝 Container Manager 或 Docker 套件。Synology 的「套件中心」搜尋得到才行；很多入門的 ARM 機型沒有。到 Synology 網站查你的型號 |
| CPU 架構 | 套件中心的 Container Manager 說明會寫。x86_64 機型用 `linux/amd64` 映像，ARM 機型用 `linux/arm64`；映像要和 NAS 的架構相同 |
| 記憶體 | 單節點的啟動記憶體用量不大，但沒有實測過 DSM 上的數字。同一台 NAS 上其他服務多時，先看可用記憶體 |
| 網路 | NAS 要有固定位址（DHCP 保留或手動設定），設備的 RADIUS 伺服器位址指向它 |
| 這台 NAS 的用途 | RADIUS 是網路的入口：NAS 重開機或更新 DSM 時，所有人都連不上。有條件就用[第 7 章](07-ha-cluster.md)的三個節點，不要只靠一台 |

## 步驟

### 1. 在別的電腦上建置映像

NAS 的 CPU 慢，Rust 編譯很久，所以在另一台有 Docker 的電腦上做出映像，再搬過去。`--platform` 要和 NAS 的架構一致：

```bash
# 在 repo 根目錄；x86_64 的 NAS：
docker buildx build --platform linux/amd64 -t rradius-server:latest --load .
# ARM 的 NAS 改成 linux/arm64
docker save rradius-server:latest | gzip > rradius-server.tar.gz
```

在 Apple Silicon 的電腦上建 `linux/amd64` 會用模擬，很慢，要有耐心。

### 2. 在 NAS 上建資料夾，放設定與密鑰

用 File Station 或 SSH 建 `docker/rradius/`（在你放 Docker 資料的共用資料夾下，下面用 `/volume1/docker/rradius/`），放進：

- `compose.yaml`：[`deploy/synology/compose.yaml`](../../deploy/synology/compose.yaml)
- `config/`：[`docker/config/`](../../docker/config/) 整個資料夾，再依下一節改 `rradius.toml`
- `secrets/`：先在電腦上跑 `docker/init-secrets.sh`，把產生的 `docker/secrets/` 裡三個 `*.secret` 檔複製過來

```
/volume1/docker/rradius/
├── compose.yaml
├── config/            rradius.toml、auth.lua、migrations/
└── secrets/           raft.secret、api.secret、radius.secret
```

密鑰檔要世界可讀（`644`）：容器裡的使用者（uid 10001）不是 NAS 上的任何帳號，`init-secrets.sh` 已經這樣設。`secrets/` 資料夾不要分享給別人。`radius.secret` 的內容是設備上要填的共用密碼。

### 3. 匯入映像

Container Manager →「映像」→「新增」→「從檔案新增」，選 `rradius-server.tar.gz`。DSM 6 的 Docker 套件的「映像」頁也有同樣的「從檔案新增」。匯入後映像名稱是 `rradius-server:latest`，和 `compose.yaml` 的 `image` 相同。

### 4. 改設定

編輯 `config/rradius.toml`：

- `[[radius.clients]]` 預設接受 `10.0.0.0/8`、`172.16.0.0/12`、`192.168.0.0/16`。把它縮成你的設備位址或網段；每一個用戶端有自己的名稱，記錄與計費才分得出是哪一台。
- 換掉 `migrations/` 裡示範用的帳號，見[第 2 章「換成自己的資料」](02-quick-start.md#換成自己的資料)。**第一次啟動前就決定好**，套用過的 migration 檔不能再改。
- `[metrics]` 只綁 `127.0.0.1`，不要改成對外位址（它沒有認證）。

### 5. 建立專案並啟動

Container Manager →「專案」→「建立」，專案名稱 `rradius`，路徑選 `/volume1/docker/rradius`，來源選「使用現有的 docker-compose.yml」，完成後啟動。

第一次啟動時伺服器會在資料磁碟建資料庫並套用 migration。「專案」的記錄頁看到 `RADIUS Server listening on` 就可以接受請求。

### 6. 開放防火牆，在設備上設定

- DSM 的「控制台」→「安全性」→「防火牆」若有啟用，允許來源設備的位址連到 UDP 1812 與 1813。
- 設備（AP、交換器、VPN 閘道）的 RADIUS 伺服器填 NAS 的位址，認證埠 1812、計費埠 1813，共用密碼是 `radius.secret` 的內容。

### 7. 試一次

從區網內的另一台電腦：

```bash
radtest alice alice-password <NAS 的位址> 0 <radius.secret 的內容>
```

（示範帳號在 `config/migrations/` 裡，換掉帳號之後以你的帳號為準。）會收到 Access-Accept 或 Access-Reject；沒有任何回應，看「疑難排解」。

## 日常

| 工作 | 做法 |
| --- | --- |
| 看記錄 | 專案頁的「記錄」，或 SSH 上去 `sudo docker compose -p rradius logs` |
| 改設定 | 改 `config/` 的檔案後，重新啟動專案。設定錯誤時容器會馬上結束，記錄寫明原因；先在電腦上用 `rradius-server check` 檢查更保險 |
| 更新版本 | 在電腦上建新映像、匯入、重新啟動專案。資料在 `data` 磁碟，不會因為更新而不見 |
| 備份 | 備份 `config/`、`secrets/`，資料磁碟是 Docker 管理的磁碟（DSM 上在 Docker 的 volumes 資料夾），用 Hyper Backup 備份那個資料夾，或用 `docker run --rm -v rradius_data:/d -v $PWD:/b alpine tar czf /b/data.tgz -C /d .`，備份時先停專案 |
| 停止 | 專案頁「停止」。`stop_grace_period` 設成 30 秒，給資料庫完成關閉的時間；不要用強制停止 |

## 疑難排解

| 現象 | 原因 |
| --- | --- |
| 設備沒有收到回應，記錄也沒有請求 | 防火牆擋住，或設備填錯 NAS 位址；在 NAS 上 `sudo tcpdump -ni any udp port 1812`（若有此工具）看封包有沒有到 |
| 記錄寫 `unknown client` | 來源位址不在 `[[radius.clients]]`。看記錄裡的來源位址：用橋接網路時，Docker 會對外部來的封包做位址轉換，多數情況下保留原來的來源位址，但這一點在 DSM 上沒有驗證；若記錄顯示的都是 Docker 的內部位址（例如 `172.17.0.1`），見下一列 |
| 所有設備來源位址都一樣 | 把 `compose.yaml` 的 `ports:` 拿掉，改成 `network_mode: host`，容器直接用 NAS 的網路。這時 NAS 自己的服務不能佔用 1812、1813 |
| 啟動就結束，記錄寫 `migration file ... changed after it was applied` | 改了已套用的 migration；改回原來的內容，另加新檔案 |
| 啟動就結束，權限錯誤 | `data` 若改成了 File Station 建立的資料夾，容器的使用者寫不進去；用範例的具名磁碟，或把那個資料夾的擁有者改成 uid 10001 |
| 記錄寫 `Read-only file system` | 範例 `compose.yaml` 把容器的根檔案系統設成唯讀（`read_only: true`），只有資料磁碟和 `/tmp` 可寫；設定裡有路徑指到別處（例如計費暫存、記錄檔）時，掛一個磁碟給它，或改成指向 `/var/lib/rradius` 底下 |
| 重開機後專案沒有起來 | `restart: unless-stopped` 在 Docker 啟動後會帶起容器；若 NAS 重開機後套件還沒啟動完成，等一下再看，仍然沒起來就手動啟動專案 |

## 為什麼是這個做法

- **不做成 SPK：** Synology 的套件格式要為每個架構與 DSM 版本各做一份，套件用的使用者權限（`package` 帳號）能不能綁 1812、1813 以外的特權埠（TACACS+ 的 49 埠）還沒驗證，也沒有簽章與發行管道。Docker 的映像是單一個、行為和其他平台相同，現階段先用它。原生套件列為研究項目，要先驗證 DSM 的 glibc 版本、非 root 綁埠與 ARM 的頁大小。
- **映像不在公開的 registry：** repo 沒有發行映像，所以用 `docker save` 搬過去。以後若有 registry，`compose.yaml` 的 `image` 改成它的位址，NAS 會自己拉。
- **密鑰用資料夾而不是 Compose 的 `secrets`：** 套件裡的 Docker Compose 版本不一定支援，直接掛成 `/run/secrets` 下的檔案，`rradius.toml` 寫的路徑完全相同。
- **只有一個節點：** 叢集要三個節點、分在不同的機器上才有意義；同一台 NAS 上跑三個容器不會比一個更可靠。

## 驗證過什麼

- **在一般 Linux 的 Docker 上：** 用官方 Dockerfile 建置映像，照 `compose.yaml` 的資料夾結構啟動，`docker/smoke-test.py` 通過（Status-Server、拒絕不存在的用戶、計費紀錄寫入）。倉庫根目錄的 `compose.yaml` 與 `compose.cluster.yaml` 帶著 `read_only`、`tmpfs`、`cap_drop` 與 `no-new-privileges`，在這些設定下單節點與三節點都通過；`deploy/synology/compose.yaml` 帶同樣四個設定，本身只驗證過 `docker compose config` 能解析，沒有實際啟動。
- **沒有驗證：** DSM 的 Container Manager 與舊版 Docker 套件的畫面與行為、各型號的支援、DSM 防火牆、位址轉換是否保留來源位址、Hyper Backup、NAS 重開機後的自動啟動。
