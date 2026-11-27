# 安裝

> 本章讓你做到：
> - 依你的環境選一種安裝方式：原始碼、deb、rpm、FreeBSD port、發行壓縮檔、Docker、Docker Compose 或 Synology
> - 知道裝好之後程式、字典、設定、資料、日誌各在哪裡
> - 寫出第一份能通過 `check` 的設定檔，啟動服務
> - 用 `--version` 確認版本與預設設定檔路徑

前置條件：讀過[第 1 章](01-overview.md)，知道設定檔、資料目錄、字典的角色。

本章小節：

- [選擇安裝方式](#選擇安裝方式)
- [裝好之後檔案在哪裡](#裝好之後檔案在哪裡)
- [從原始碼建置](#從原始碼建置)
- [deb 套件（Debian、Ubuntu）](#deb-套件debianubuntu)
- [rpm 套件（RHEL 系列）](#rpm-套件rhel-系列)
- [FreeBSD port](#freebsd-port)
- [發行壓縮檔](#發行壓縮檔)
- [Docker image](#docker-image)
- [Docker Compose](#docker-compose)
- [Synology NAS](#synology-nas)
- [套件安裝後的第一次啟動](#套件安裝後的第一次啟動)
- [確認版本與預設設定檔路徑](#確認版本與預設設定檔路徑)
- [升級與移除](#升級與移除)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

## 選擇安裝方式

repo 提供的是各種安裝方式的建置腳本與定義檔，本書的每一種安裝方式都以**從原始碼自己建出來**為準，再裝到你的機器上：

| 方式 | 適合 | 建置腳本或定義檔 | 本書的驗證狀態 |
| --- | --- | --- | --- |
| 原始碼 | 開發、測試、跟著本書做練習 | `cargo` | 已驗證 |
| deb | Debian 12、13，Ubuntu 22.04 以後 | `packages/deb/` | 未實機驗證 |
| rpm | RHEL 系列（例如 Rocky、AlmaLinux） | `packages/rpm/` | 未實機驗證 |
| FreeBSD port | FreeBSD | `packages/freebsd/net/rradius/` | rc.d 腳本實機驗證，建置未驗證 |
| 發行壓縮檔 | 不想用套件管理、或在 macOS 上試用 | `packages/tarball/release.sh` | 未實機驗證 |
| Docker | 容器環境 | `Dockerfile` | 已驗證 |
| Docker Compose | 單機容器部署 | `compose.yaml`、`docker/` | 已驗證 |
| Synology | Synology NAS 的 Container Manager | `deploy/synology/compose.yaml` | 未實機驗證 |

叢集的 Compose 檔（`compose.cluster.yaml`）與 Kubernetes manifest（`deploy/kubernetes/`）在[第 16 章](16-cluster.md)說明。

本章提到的路徑都相對於 rradius 原始碼 repo 的根目錄。

> **注意：** 壓測程式 `rradius-bench` 不在任何套件、壓縮檔或 Docker image 裡。第 3 章用它來測試登入，第 19 章用它壓測。要用它，就照[從原始碼建置](#從原始碼建置)把它建出來，放在能連到伺服器的機器上。

## 裝好之後檔案在哪裡

不同安裝方式的位置整理如下。依據是 `packages/deb/debian/rules`、`packages/rpm/rradius.spec`、`packages/freebsd/net/rradius/` 與 `Dockerfile`：

| | deb、rpm | FreeBSD port | Docker image |
| --- | --- | --- | --- |
| 執行檔 | `/usr/bin/rradiusd` | `/usr/local/bin/rradiusd` | `/usr/local/bin/rradiusd` |
| 字典 | `/usr/share/rradius/` | `/usr/local/share/rradius/` | `/usr/share/rradius/` |
| 設定 | `/etc/rradius/`（`conf.d/` 是空的） | `/usr/local/etc/rradius/`（`conf.d/` 是空的） | `/etc/rradius/`，要自己掛進去 |
| 預設設定檔 | `/etc/rradius/rradius.toml` | `/usr/local/etc/rradius/rradius.toml` | `/etc/rradius/rradius.toml` |
| 資料 | `/var/lib/rradius`（0750） | `/var/db/rradius`（0750） | `/var/lib/rradius`（VOLUME） |
| 日誌目錄 | `/var/log/rradius`（0750） | `/var/log/rradius`（0750） | 無，日誌寫到標準錯誤 |
| 文件與範例 | `/usr/share/doc/rradius/` | `/usr/local/share/doc/rradius/`（DOCS 選項） | 無 |
| 服務 | systemd 的 `rradius.service` | `rc.d` 的 `rradius` | image 的 `CMD` |
| 執行身分 | 系統用戶 `rradius` | 用戶 `rradius` | 用戶 `rradius`（uid 10001） |

幾點說明：

- **預設設定檔路徑在建置時就決定了。** 建置時設了環境變數 `RRADIUS_PREFIX=<prefix>`，預設路徑就是 `<prefix>/etc/rradius/rradius.toml`；沒設、設成空字串或 `/` 時是 `/etc/rradius/rradius.toml`。執行時的環境變數改變不了它。設定檔沒寫 `dictionary` 時讀的字典也跟著前綴：`<prefix>/share/rradius/dictionary`，沒設時是 `/usr/share/rradius/dictionary`。FreeBSD port 建置時設了 `RRADIUS_PREFIX=/usr/local`，其他方式都沒設。用 `--version` 可以看到實際的值（見[確認版本](#確認版本與預設設定檔路徑)）。
- **套件不會建立設定檔，也不會啟用服務。** 裝好之後要自己產生設定檔、檢查、再啟用，步驟見[套件安裝後的第一次啟動](#套件安裝後的第一次啟動)。
- **日誌寫到標準錯誤。** systemd 下由 journal 收走。`/var/log/rradius` 是給計費記錄檔（`[radius.accounting_log]`，第 13 章）用的。

## 從原始碼建置

不管最後用哪種方式安裝，這一節都值得先做一次：本書的練習都用原始碼建出來的程式。

### 需要的工具

- Rust 工具鏈（`cargo`）。需要的版本比各 Linux 發行版附的新，用 rustup 安裝。確切的版本看 `Dockerfile` 第一行 `FROM rust:…` 用的那一版。
- cmake、C++ 編譯器（g++ 或 clang）、make。
- git。
- 建置時會從 crates.io 下載相依套件，所以要能連網。

### 建置

```console
$ git clone https://github.com/embark-systems/rradius.git
$ cd rradius
$ cargo build --locked --release -p rradius-server -p rradius-bench
…
    Finished `release` profile [optimized] target(s) in …
```

產出在：

- `target/release/rradiusd`：伺服器與管理指令
- `target/release/rradius-bench`：壓測與登入測試工具

說明：

- **一律加 `--locked`。** 它讓 cargo 照 repo 裡 `Cargo.lock` 記錄的版本建置，不自己升級相依套件。
- repo 裡的 `.cargo/config.toml` 會設好 `LIBSQLITE3_FLAGS=-DSQLITE_ENABLE_QPSG`，這是內建 SQLite 需要的編譯選項。在 repo 裡建置不必自己設。
- 在 arm64 的 Linux 上，`Dockerfile`、deb、rpm 與 FreeBSD port 都另外設了 `JEMALLOC_SYS_WITH_LG_PAGE=16`，讓程式在 4、16、64 KiB 分頁的核心上都能跑。你自己在 arm64 Linux 建置、又要把程式搬到別台機器時，也照樣設。
- 在 FreeBSD 上，jemalloc 要帶 `ac_cv_func__malloc_thread_cleanup=no` 和 `ac_cv_func__pthread_mutex_init_calloc_cb=no` 建置，否則它會蓋掉 C 函式庫的執行緒清理與 fork 掛勾，伺服器在執行緒結束後會崩潰。repo 的 `.cargo/config.toml` 與 FreeBSD port 都設好了；沒帶這兩個設定建出的 `rradiusd` 會拒絕啟動，並說明怎麼重建。之前建過的 `target` 目錄會沿用舊的 jemalloc：先執行 `cargo clean -p tikv-jemalloc-sys`（release 建置是 `cargo clean --release -p tikv-jemalloc-sys`）再建置。這件事在 FreeBSD 15.1 arm64 虛擬機上檢查過，amd64 與實體機器沒有。

建好之後先確認程式能跑：

```console
$ ./target/release/rradiusd --version
rradiusd 0.1.0 (community)
default config: /etc/rradius/rradius.toml
default dictionary: /usr/share/rradius/dictionary
```

`--version` 第一行括號裡寫的是版本類型：核心版是 `(community)`，企業版是 `(enterprise)`。兩個版本的執行檔都叫 `rradiusd`，共用同一份設定、資料目錄、服務與用戶，所以一台機器只裝其中一個；要並存做比對時，放在不同的目錄，用完整路徑執行。

想直接用範例情境試跑，接著讀[第 3 章](03-quick-start.md)。

要把原始碼建出來的程式當正式服務用，可以把 `target/release/rradiusd` 複製到 `/usr/local/bin`，字典目錄 `share/` 複製到固定的位置（例如 `/usr/share/rradius`），再自己寫服務檔。`packages/common/rradius.service` 是現成的 systemd unit，可以參考。不過比較省事的做法是建一個 deb 或 rpm。

## deb 套件（Debian、Ubuntu）

> **未實機驗證：** 本節對照了 `packages/deb/` 的建置腳本與 `debian/` 目錄內容，沒有在本書撰寫時重新建置或安裝。

### 建置

有兩種建法，都從**已提交**的原始碼建（用 `git archive`），沒提交的改動不會進套件。

在容器裡建，機器上只要有 docker：

```console
$ packages/deb/docker-build.sh --platform linux/amd64 --out dist
```

- `--platform` 可以是 `linux/amd64` 或 `linux/arm64`。不是本機架構時用模擬執行，比較慢，結果相同。
- 容器是 Debian 12。套件依賴建置環境的 glibc，在 Debian 12 建的套件也能裝在比它新的發行版上。
- `--commit <rev>` 指定要建哪一個提交，預設是 `HEAD`。
- 建置之前，腳本會檢查該提交的 Release workflow 已經通過（需要登入的 `gh`），以及 `Cargo.toml`、changelog、spec 和 FreeBSD Makefile 的版本一致。`--no-ci-check` 只略過 workflow 檢查，例如在沒有 `gh` 的容器裡跑 `build.sh`；版本檢查一定會跑。`packages/rpm/build.sh` 做同樣的檢查，也有同一個選項。

在本機直接建，需要 Rust 工具鏈、cmake、g++、make、debhelper、dpkg-dev：

```console
$ packages/deb/build.sh --out dist
```

產出是 `dist/rradius_<版本>_<架構>.deb`，架構是 amd64 或 arm64。

### 安裝

```console
$ sudo apt install ./dist/rradius_0.1.0_amd64.deb
```

套件做的事：

- 安裝 `/usr/bin/rradiusd`、字典 `/usr/share/rradius/`、文件與範例 `/usr/share/doc/rradius/`。
- 建立系統用戶與群組 `rradius`，建立 `/var/lib/rradius` 與 `/var/log/rradius`，擁有者是 `rradius`，權限 0750。
- 建立空的 `/etc/rradius/` 與 `/etc/rradius/conf.d/`。
- 安裝 systemd unit `rradius.service` 與 logrotate 設定 `/etc/logrotate.d/rradius`。
- **不啟用、也不啟動服務。**

接著做[套件安裝後的第一次啟動](#套件安裝後的第一次啟動)。

## rpm 套件（RHEL 系列）

> **未實機驗證：** 本節對照了 `packages/rpm/rradius.spec` 與 `packages/rpm/build.sh`，沒有在本書撰寫時重新建置或安裝。

### 建置

需要 Rust 工具鏈、cmake、gcc-c++、make、rpm-build、systemd-rpm-macros：

```console
$ packages/rpm/build.sh --out dist
```

產出是 `dist/rradius-<版本>-1.<dist>.<架構>.rpm`，架構是 x86_64 或 aarch64。套件依賴建置主機的 glibc，要裝到哪個發行版，就在那個發行版（或它的容器）裡建。

### 安裝

```console
$ sudo dnf install ./dist/rradius-0.1.0-1.el9.x86_64.rpm
```

安裝的檔案與 deb 相同：`/usr/bin/rradiusd`、`/usr/share/rradius/`、`/usr/share/doc/rradius/`、`/etc/rradius/`（含空的 `conf.d/`）、`/var/lib/rradius` 與 `/var/log/rradius`（0750，擁有者 `rradius`）、systemd unit 與 logrotate 設定。

服務要不要自動啟用，依發行版的 systemd preset 而定（spec 用的是 `%systemd_post`）。RHEL 系列通常不啟用。不管有沒有啟用，設定檔不存在時服務都不會啟動，原因見下一節。

接著做[套件安裝後的第一次啟動](#套件安裝後的第一次啟動)。

## FreeBSD port

> **部分實機驗證：** 本節對照了 `packages/freebsd/net/rradius/` 的 Makefile、`pkg-plist` 與 rc.d 腳本。rc.d 腳本在 FreeBSD 15.2-STABLE 上以 `rradius` 用戶啟動並穩定執行過；port 的建置與 `pkg` 安裝沒有實測過。

### 建置

1. 把 `packages/freebsd/net/rradius` 複製到 ports 樹的 `net/rradius`。
2. 在 ports 樹的 `UIDs` 與 `GIDs` 加上 `rradius`。port 用 `USERS`、`GROUPS` 建立這個用戶。
3. 在 port 目錄執行 `make makesum` 產生 `distinfo`，再 `make package`。

port 從 GitHub 抓 `v<版本>` 這個標籤的原始碼，所以那個版本要已經打好標籤。

### 安裝後

- 執行檔 `/usr/local/bin/rradiusd`，字典 `/usr/local/share/rradius/`。
- 資料目錄 `/var/db/rradius`、日誌目錄 `/var/log/rradius`，擁有者 `rradius`，0750。
- 空的 `/usr/local/etc/rradius/` 與 `conf.d/`。
- 預設設定檔是 `/usr/local/etc/rradius/rradius.toml`。
- 開了 DOCS 選項時，文件與範例在 `/usr/local/share/doc/rradius/`。

### rc.d 服務

rc.d 腳本 `rradius` 讀這些 `rc.conf` 變數：

| 變數 | 預設 | 意義 |
| --- | --- | --- |
| `rradius_enable` | `NO` | 設成 `YES` 才會啟動 |
| `rradius_config` | `/usr/local/etc/rradius/rradius.toml` | 設定檔 |
| `rradius_user` | `rradius` | 執行身分 |

服務透過 `daemon(8)` 執行，程式結束時隔 2 秒自動重新啟動，輸出交給 syslog（tag 是 `rradius`）。設了 `rradius_user` 時，rc.subr 已經用 `su -m` 以那個用戶執行整個指令，所以腳本不再給 `daemon` 加 `-u`（非 root 的 `daemon -u` 會在 initgroups 失敗）。PID 檔放在 `/var/run/rradius/`，啟動時由腳本建立，擁有者是 `rradius_user`。`service rradius reload` 會對程式送 SIGHUP，重新載入設定（第 4 章）。

```console
# sysrc rradius_enable=YES
# service rradius start
```

> **注意：** port 的程式以 `rradius` 用戶執行，不能綁 1024 以下的埠。要在 TACACS+ 的標準埠 49 提供服務，得用 `mac_portacl(4)`、把 `net.inet.ip.portrange.reservedhigh` 設成 0，或改用 1024 以上的埠。RADIUS 的 1812、1813 不受影響。

計費記錄檔可以交給 newsyslog 輪替，範本是 `/usr/local/etc/newsyslog.conf.d/rradius.conf.sample`，拿掉 `.sample` 就會生效。

## 發行壓縮檔

> **未實機驗證：** 本節對照了 `packages/tarball/release.sh`，沒有在本書撰寫時產生或解開壓縮檔。

壓縮檔用 `packages/tarball/release.sh` 產生：

```console
$ packages/tarball/release.sh --target linux-x86_64 --out dist
```

- `--target` 可以是 `macos-aarch64`、`linux-x86_64`、`linux-aarch64`，可以重複給多個。不給時建本機的平台。
- `macos-aarch64` 只能在 arm64 的 Mac 上建；Linux 版在 Docker 裡用 `Dockerfile` 的建置階段建。
- 腳本需要特定版本的 `cargo-about` 和 `cargo-deny`，用來檢查與列出相依套件的授權。版本不符時，腳本會印出要執行的安裝指令。
- 開始建置之前，腳本會用 `gh` 確認這個 commit 的 Release workflow（`release.yml`，對同一個 commit 把其他 workflow 全部跑一遍）已經通過；用 `gh workflow run release.yml --ref <ref>` 啟動它。不是正式 release 的建置（例如從 fork 建置）加 `--no-ci-check`。
- 產出是 `dist/rradius-<版本>-<平台>.tar.gz` 與對應的 `.sha256`。打包完成前，腳本會解開壓縮檔，用裡面的程式檢查範例情境，檢查失敗就不產出。

壓縮檔裡的目錄結構和原始碼 repo 一樣，所以範例設定裡的相對路徑照樣成立：

| 路徑 | 內容 |
| --- | --- |
| `bin/rradiusd` | 伺服器與管理指令 |
| `share/` | 字典，以及字典的 `LICENSE`、`NOTICE` |
| `rradius-server/examples/` | 範例設定、腳本與情境 |
| `docs/` | 文件 |
| `LICENSE`、`THIRD-PARTY-LICENSES.md` | rradius 與相依套件的授權 |
| `VERSION` | 版本與建置用的 commit；Linux 版另有一行 `glibc <版本>`，是它需要的最低 glibc |

壓縮檔沒有服務檔，也沒有建立用戶與目錄。拿它當正式服務時，這些都要自己準備。

## Docker image

repo 根目錄的 `Dockerfile` 分兩個階段：在 Rust image 裡用 `cargo build --release --locked -p rradius-server` 建置，再把程式與字典放進 Debian slim image。

```console
$ docker build -t rradius-server .
…
$ docker run --rm rradius-server --version
rradiusd 0.1.0 (community)
default config: /etc/rradius/rradius.toml
default dictionary: /usr/share/rradius/dictionary
```

image 的內容：

| 項目 | 值 |
| --- | --- |
| 執行檔 | `/usr/local/bin/rradiusd`，是 `ENTRYPOINT` |
| 預設指令 | `CMD ["serve", "--config", "/etc/rradius/rradius.toml"]` |
| 字典 | `/usr/share/rradius/` |
| 執行身分 | `rradius`（uid 10001），不是 root |
| 資料 | `VOLUME /var/lib/rradius`，擁有者是 `rradius` |
| EXPOSE | `1812/udp`、`1813/udp`、`49/tcp`、`8100/tcp`、`8200/tcp` |
| 其他 | 裝了 `ca-certificates`，LDAP over TLS 會用到系統的 CA 憑證 |

因為 `ENTRYPOINT` 是 `rradiusd`，`docker run` 後面接的就是子指令，例如 `docker run --rm rradius-server --version`。

image 裡沒有設定檔。`/etc/rradius/rradius.toml` 要從外面掛進去，最簡單的做法是用下一節的 Compose。

> **注意：** EXPOSE 列了叢集埠 8100、8200，但沒有 `[[cluster.nodes]]` 也沒有 `cluster.listen` 時，這兩個埠只綁 `127.0.0.1`，容器外面連不到。單機不需要對外開這兩個埠。

## Docker Compose

`compose.yaml` 跑一個節點，設定檔在 `docker/config/`，祕密檔由 `docker/init-secrets.sh` 產生。

### 步驟

```console
$ sh docker/init-secrets.sh
created docker/secrets/raft.secret
created docker/secrets/api.secret
created docker/secrets/radius.secret
$ docker compose up --build -d
…
 Container …-rradius-1 Started
```

`init-secrets.sh` 用 `openssl rand -hex 32` 產生三個檔案：叢集的 Raft 密碼、API 密碼，以及 RADIUS 用戶端的共用密碼。已經存在的檔案不會被覆蓋，所以重跑是安全的。

看日誌確認啟動成功：

```console
$ docker compose logs
…
rradius-1  | … INFO rradius_core::cli: cluster node 1 started
rradius-1  | … INFO rradius_core::cli: cluster node 1 has caught up with the cluster
rradius-1  | … INFO rradius_core::metrics: Metrics listening on 127.0.0.1:9812
rradius-1  | … INFO rradius_core::cli: Servers started. Press Ctrl+C to stop.
rradius-1  | … INFO rradius_core::radius: RADIUS Server listening on 0.0.0.0:1812 and 0.0.0.0:1813
…
```

在容器裡看狀態：

```console
$ docker compose exec rradius rradiusd status --config /etc/rradius/rradius.toml
… WARN rradius_core::common::seal: cluster.secret_raft_file: '/run/secrets/raft' has mode 644, which others can reach or its group can write; make it 0600 or 0640
…
rradius status of 127.0.0.1:9812 over 1.0 s

Node
  Ready                     ok
  Leader                    1
…
```

> **注意：** 這裡的 `has mode 644` 警告是預期的。`init-secrets.sh` 故意把祕密檔設成所有人可讀，因為容器裡的用戶不是主機上的用戶，Compose 掛進去時不改權限。保護祕密靠的是 `docker/secrets/` 目錄本身（權限 700）。

停止：

```console
$ docker compose down
```

加 `-v` 會連資料 volume 一起刪掉，帳號與計費資料也就沒了。

### 這份 Compose 做了什麼

`compose.yaml` 的設定：

- 只對外開 `1812/udp` 與 `1813/udp`。TACACS+ 和指標埠都沒有開到主機上。
- 設定目錄 `./docker/config` 以唯讀方式掛到 `/etc/rradius`；資料放在名為 `data` 的 volume，掛到 `/var/lib/rradius`。
- 三個祕密檔以 Compose secrets 掛到 `/run/secrets/raft`、`/run/secrets/api`、`/run/secrets/radius`。
- 根檔案系統唯讀（`read_only: true`），`/tmp` 用 tmpfs，拿掉所有 capability，加上 `no-new-privileges`。伺服器只寫資料 volume 與 `/tmp`，所以這樣就夠了。
- `stop_grace_period: 30s`：停止時給資料庫足夠的時間收尾。Docker 預設的 10 秒可能不夠。
- `restart: unless-stopped`。

`docker/config/` 裡有：

| 檔案 | 內容 |
| --- | --- |
| `rradius.toml` | 沒有 `dictionary` 行，所以用預設的 `/usr/share/rradius/dictionary`；資料目錄 `/var/lib/rradius`；sql 處理器查 `users` 表；三個私有網段（`10.0.0.0/8`、`172.16.0.0/12`、`192.168.0.0/16`）各一個用戶端；`[metrics]` 是空的，所以指標埠是預設的 `127.0.0.1:9812` |
| `auth.lua` | 只處理計費：把每筆計費寫進 `accounting` 表 |
| `migrations/1_schema.sql` | 建立 `users` 與 `accounting` 兩個表 |
| `cluster/` | 三節點叢集用的設定，見第 16 章 |

> **警告：** 預設的用戶端涵蓋三個完整的私有網段，因為 Docker 給容器的來源位址就在這些網段裡。正式使用前，把它們改成你的 NAS 實際所在的位址或網段（第 5 章）。

### 加入帳號

這份設定沒有 `[accounts]`，所以 `user add` 不能用。`1_schema.sql` 的註解建議的做法是新增一個編號更大的 migration 檔，例如 `docker/config/migrations/2_users.sql`，內容是 `INSERT INTO users …`。接著執行 `docker compose restart rradius`：migration 只在啟動時套用，reload 不會套用。密碼欄位的格式見[第 7 章](07-users-and-chains.md)。

也可以照第 7 章在 `rradius.toml` 加上 `[accounts]`，之後用 `docker compose exec rradius rradiusd user add …` 管理帳號。

這裡的 `users` 表沒有第 7 章 `enable`、`disable`、`show` 用到的 `enabled` 欄位。要用這三個指令，新增一個 migration：`ALTER TABLE users ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;`，`user_query` 加上 `AND enabled = 1`，然後重啟。

> **注意：** 已經套用過的 migration 檔不能再改。要改資料表結構或加資料，一律新增下一個編號的檔案。

## Synology NAS

> **未實機驗證：** 本節只對照了 `deploy/synology/compose.yaml`，沒有在 Synology NAS 上執行過。把 image 搬到 NAS 上的方法是本書的建議做法，repo 裡沒有現成的腳本。

`deploy/synology/compose.yaml` 讓 Synology 的 Container Manager 以「專案」的方式跑一個節點。它和上一節的 `compose.yaml` 幾乎一樣，差別有兩點：

- **image 不在 registry 上。** `image: rradius-server:latest` 要事先在別台機器用 `docker build -t rradius-server .` 建好，再匯入 NAS。例如在建置機器上執行 `docker save rradius-server:latest -o rradius-server.tar`，然後在 Container Manager 的映像檔頁面匯入這個檔案。NAS 是 x86_64 還是 arm64，image 就要用同一種架構建。
- **祕密檔直接掛成檔案。** 三個祕密檔以一般的唯讀 bind mount 掛到 `/run/secrets/`，不用 Compose secrets，所以不受 NAS 上 Compose 版本的影響。

NAS 上的目錄這樣放：

```text
/volume1/docker/rradius/compose.yaml   deploy/synology/compose.yaml
/volume1/docker/rradius/config/        docker/config/ 的複本（在這裡改 rradius.toml）
/volume1/docker/rradius/secrets/       raft.secret、api.secret、radius.secret
```

祕密檔可以在建置機器上用 `sh docker/init-secrets.sh` 產生，再把 `docker/secrets/` 裡的三個檔案複製過去。

資料用的是 named volume `data`，Docker 會讓容器的用戶（uid 10001）能寫入。如果改用在 File Station 建立的資料夾，容器的用戶通常沒有寫入權限。

### 事前檢查

| 項目 | 說明 |
| --- | --- |
| 機型 | 要能裝 Container Manager（DSM 6 到 7.1 叫 Docker）。Synology 的「套件中心」搜尋得到才行；有些入門的機型沒有。到 Synology 的網站查你的型號 |
| CPU 架構 | x86_64 機型建 `linux/amd64` 的 image，ARM 機型建 `linux/arm64`；image 要和 NAS 的架構相同 |
| 記憶體 | 單節點的記憶體用量沒有在 DSM 上量過。同一台 NAS 上其他服務多時，先看可用記憶體 |
| 網路 | NAS 要有固定位址（DHCP 保留或手動設定），設備的 RADIUS 伺服器位址指向它 |
| 單點故障 | RADIUS 是網路的入口：NAS 重開機或更新 DSM 時，所有人都連不上。有條件就用[第 16 章](16-cluster.md)分在不同機器上的三個節點，不要只靠一台。同一台 NAS 上跑三個容器不會比一個更可靠 |

### 在別的電腦上建置 image

NAS 的 CPU 慢，Rust 編譯很久，所以在另一台有 Docker 的電腦上建好再搬過去。`--platform` 要和 NAS 的架構一致：

```console
$ docker buildx build --platform linux/amd64 -t rradius-server:latest --load .
$ docker save rradius-server:latest | gzip > rradius-server.tar.gz
```

ARM 的 NAS 把 `linux/amd64` 換成 `linux/arm64`。`Dockerfile` 在 arm64 上建置時，把 jemalloc 的頁大小固定成 64 KiB，所以同一個 arm64 image 也能在 4 KiB、16 KiB 頁的核心上跑。在 Apple Silicon 的電腦上建 `linux/amd64` 會用模擬，很慢。

### 在 DSM 上操作

1. **放資料夾：** 用 File Station 或 SSH 建 `/volume1/docker/rradius/`（`docker` 共用資料夾的位置依你的設定），放進 `compose.yaml`、`config/`、`secrets/`。
2. **匯入 image：** Container Manager →「映像」→「新增」→「從檔案新增」，選 `rradius-server.tar.gz`（上一節用 `docker save` 不加 gzip 的 `.tar` 也可以）。匯入後名稱是 `rradius-server:latest`，和 `compose.yaml` 的 `image` 相同。
3. **改設定：** 編輯 `config/rradius.toml`。
   - `[[radius.clients]]` 預設接受 `10.0.0.0/8`、`172.16.0.0/12`、`192.168.0.0/16` 三個私有網段（見[Docker Compose](#docker-compose)的警告），把它縮成你的設備位址或網段；每個用戶端給一個 `name`，日誌與指標才分得出是哪一台。
   - 要有帳號的話，**第一次啟動前**在 `config/migrations/` 加 `2_users.sql`（見[加入帳號](#加入帳號)）；套用過的 migration 檔不能再改。
   - `[metrics]` 保持只綁 `127.0.0.1`，不要改成對外位址（它沒有認證）。
4. **建立專案並啟動：** Container Manager →「專案」→「建立」，專案名稱 `rradius`，路徑選 `/volume1/docker/rradius`，來源選「使用現有的 docker-compose.yml」，完成後啟動。第一次啟動時，伺服器在資料 volume 建資料庫並套用 migration；專案的「記錄」頁看到 `RADIUS Server listening on` 就可以接受請求。
5. **開放防火牆：** DSM 的「控制台」→「安全性」→「防火牆」若有啟用，允許設備的位址連到 UDP 1812 與 1813。
6. **在設備上設定：** RADIUS 伺服器填 NAS 的位址，認證埠 1812、計費埠 1813，共用密碼是 `secrets/radius.secret` 的內容。
7. **試一次：** 從區網內另一台裝了 `rradius-bench` 的電腦（[從原始碼建置](#從原始碼建置)）：

   ```console
   $ rradius-bench run --server <NAS 的位址> --auth-port 1812 --secret "$(cat radius.secret)" \
       --user <帳號> --password <密碼> --load pap,concurrency=1 --requests 1 | grep 'pap:'
   ```

   `accept 1` 表示通了；`reject 1` 表示伺服器有回應、只是帳號或密碼不對。沒有回應時看下面的排錯。

> **未實機驗證：** 上面 DSM 的畫面名稱與操作順序依 Synology 文件與 Docker 的一般行為寫成，沒有在實機上跑過。

### Synology 上的排錯

| 現象 | 原因與做法 |
| --- | --- |
| 設備沒有收到回應，日誌也沒有請求 | 防火牆擋住，或設備填錯 NAS 位址。在 NAS 上用 `sudo tcpdump -ni any udp port 1812`（若有這個工具）看封包有沒有到 |
| 日誌有 `RADIUS packet from unknown client <位址> discarded` | 來源位址不在 `[[radius.clients]]`。看日誌裡的位址：橋接網路下 Docker 對外部來的封包做位址轉換，是否保留原來的來源位址，在 DSM 上沒有驗證；若顯示的都是 Docker 的內部位址（例如 `172.17.0.1`），見下一列 |
| 所有設備的來源位址都一樣 | 把 `compose.yaml` 的 `ports:` 拿掉，改成 `network_mode: host`，容器直接用 NAS 的網路。這時 NAS 自己的服務不能佔用 1812、1813 |
| 啟動就結束，權限錯誤 | 資料目錄換成了 File Station 建立的資料夾，uid 10001 寫不進去；用範例的具名 volume，或把那個資料夾的擁有者改成 uid 10001 |
| 日誌寫 `Read-only file system` | 範例 `compose.yaml` 把根檔案系統設成唯讀（`read_only: true`），只有資料 volume 與 `/tmp` 可寫；設定裡有路徑指到別處（例如計費 spool、計費記錄檔）時，掛一個 volume 給它，或改成指向 `/var/lib/rradius` 底下 |
| NAS 重開機後專案沒有起來 | `restart: unless-stopped` 在 Docker 啟動後會帶起容器；若 NAS 重開機後套件還沒啟動完成，等一下再看，仍然沒起來就手動啟動專案 |

設定錯誤（例如 `migration file … changed after it was applied`）時容器會馬上結束，原因寫在專案的記錄裡，見[第 18 章](18-operations.md)。

### 為什麼不做成 Synology 套件

repo 裡只有 `deploy/synology/compose.yaml`，沒有 Synology 套件（SPK）的定義，也沒有發行 image 的 registry。理由：

- 套件格式要為每個架構與 DSM 版本各做一份，也沒有簽章與發行管道。
- 套件用的使用者權限能不能綁 TACACS+ 的 49 埠這種低埠，還沒驗證。
- Docker 的 image 是單一個，行為和其他平台相同。原生套件還要先驗證 DSM 的 glibc 版本、非 root 綁埠與 ARM 的頁大小。
- 沒有公開的 registry，所以用 `docker save` 搬過去。以後有 registry 時，把 `compose.yaml` 的 `image` 改成它的位址，NAS 就會自己拉。
- 祕密檔用 bind mount 而不是 Compose 的 `secrets`，因為 NAS 上的 Docker Compose 版本不一定支援。

### 驗證了什麼、沒驗證什麼

`deploy/synology/compose.yaml` 帶 `read_only`、`tmpfs`、`cap_drop`、`no-new-privileges`，同樣的設定在根目錄的 `compose.yaml` 也有；本書沒有用 `deploy/synology/compose.yaml` 實際啟動過。DSM 的 Container Manager 與舊版 Docker 套件的畫面與行為、各型號的支援、DSM 防火牆、位址轉換是否保留來源位址、Hyper Backup、NAS 重開機後的自動啟動，全都沒有驗證。

## 套件安裝後的第一次啟動

deb、rpm 與 FreeBSD port 裝好之後，都要自己建立設定檔。以下是 Linux 的步驟。FreeBSD 把 `/etc/rradius` 換成 `/usr/local/etc/rradius`，字典換成 `/usr/local/share/rradius/dictionary`，資料目錄換成 `/var/db/rradius`，啟動改用上面 rc.d 那一節的指令。

> **未實機驗證：** 下面的設定內容已在本機用 `check` 驗證過（路徑換成暫存目錄），但沒有在裝了套件的 Linux 上實際啟動服務。

### 1. 產生起始設定

```console
# rradiusd config init > /etc/rradius/rradius.toml
```

`config init` 印出一份可以直接用的底稿：叢集密碼和範例用戶端的共用密碼都是隨機產生的；`dictionary` 寫的是這個版本預設的字典位置（套件安裝是 `/usr/share/rradius/dictionary`）；沒有 `[[cluster.nodes]]`，所以是單節點；有 `include = ["conf.d/*.toml"]`，`conf.d/` 裡的 `.toml` 檔會一起讀進來；有一個 `[metrics]`。加上 `--lang zh-TW` 時，註解是中文。輸出大約 30 行；要看每個設定與預設值，用 `config init --full`。

### 2. 修改三個地方

服務只能寫 `/var/lib/rradius` 與 `/var/log/rradius`（unit 設了 `ProtectSystem=strict`），而設定檔裡的相對路徑以設定檔所在的目錄為基準。所以底稿的 `data_dir = "data"` 會變成 `/etc/rradius/data`，服務寫不進去。改成：

```toml
# /etc/rradius/rradius.toml
# …（其他設定略）
[cluster]
data_dir = "/var/lib/rradius"
# 單機：叢集埠只綁本機
listen = "127.0.0.1"
```

- 叢集：底稿沒有 `[[cluster.nodes]]`，是監聽 `127.0.0.1` 的單節點，不用設 `listen`。要組叢集時才加 `[[cluster.nodes]]` 並設 `listen`，做法見第 16 章。
- 底稿的 `[lua]` 指向 `auth.lua`，這個檔案要自己建，放在 `/etc/rradius/auth.lua`。腳本怎麼寫見第 10 章；只想先讓設定通過檢查，可以寫一行 `return "reject"`。
- 範例用戶端 `127.0.0.1` 要換成你的 NAS（第 5 章）。

之後如果用到計費 spool（`[radius.accounting_spool]` 的 `dir`），放在 `/var/lib/rradius` 底下；計費記錄檔（`[radius.accounting_log]` 的 `file`）放在 `/var/log/rradius` 底下。

### 3. 設定檔的權限

設定檔裡有密碼，只讓 `rradius` 群組讀：

```console
# chown root:rradius /etc/rradius/rradius.toml /etc/rradius/auth.lua
# chmod 640 /etc/rradius/rradius.toml /etc/rradius/auth.lua
```

`conf.d/` 裡的檔案也一樣：只讓 `rradius` 讀、只讓 root 寫。被 include 的檔案可以改變誰能登入，伺服器會警告群組或其他人可以寫入的檔案。

### 4. 檢查，再啟動

```console
# rradiusd check
configuration ok
# systemctl enable --now rradius
```

`check` 沒有 `--config` 時讀預設設定檔 `/etc/rradius/rradius.toml`，成功時印 `configuration ok`，結束碼 0；有錯時結束碼 2。

### systemd unit 的重點

`rradius.service`（來源是 `packages/common/rradius.service`）：

- `ExecStart=/usr/bin/rradiusd serve`：讀預設設定檔。
- `ConditionPathExists=/etc/rradius/rradius.toml`：設定檔不存在時，服務不會啟動。
- 以 `rradius` 用戶執行，`AmbientCapabilities=CAP_NET_BIND_SERVICE` 讓它能綁 TACACS+ 的埠 49。
- `Restart=on-failure`、`RestartSec=2`：程式異常結束時兩秒後重啟。
- `TimeoutStopSec=60`：給叢集節點足夠的時間停止。
- `ExecReload` 送 SIGHUP：`systemctl reload rradius` 會重新載入設定，並重新開啟計費記錄檔（第 4 章）。
- `LimitNOFILE=65536`、`ProtectSystem=strict`、`ProtectHome=yes`、`PrivateTmp=yes`、`NoNewPrivileges=yes`。

logrotate 設定 `/etc/logrotate.d/rradius` 每天輪替 `/var/log/rradius/*.jsonl`，保留 30 份，輪替後送 SIGHUP 讓程式開新檔。它不用 `copytruncate`，因為那樣會遺失資料。

## 確認版本與預設設定檔路徑

```console
$ rradiusd --version
rradiusd 0.1.0 (community)
default config: /etc/rradius/rradius.toml
default dictionary: /usr/share/rradius/dictionary
```

- 第一行是程式名稱與版本。
- 第二行是沒有 `--config` 時讀的設定檔。FreeBSD port 建出來的程式這裡是 `/usr/local/etc/rradius/rradius.toml`。
- 第三行是設定檔沒寫 `dictionary` 時讀的字典。FreeBSD port 建出來的程式這裡是 `/usr/local/share/rradius/dictionary`，套件把字典裝在這裡，所以不用寫。

`--version` 不讀設定檔、不開任何埠、不建立任何目錄，隨時都可以安全地執行。

## 升級與移除

> **未實機驗證：** 本節依據 deb 的 `rules`、`postrm` 與 rpm spec 的內容，沒有實際升級或移除過。

- **升級：** 套件不會覆蓋你改過的 `/etc/rradius/rradius.toml`，也不會動 `conf.d/`。服務正在執行時，deb 與 rpm 升級後都會重新啟動服務。
- **移除：** 資料（`/var/lib/rradius`）、日誌（`/var/log/rradius`）與 `rradius` 用戶都會保留，因為裡面有叢集資料庫與計費資料。
- **deb 的 purge：** 只刪除 `/etc/rradius/conf.d` 與 `/etc/rradius`，而且只在它們是空目錄時才刪。
- **Docker：** 換 image 後重新建立容器即可，資料在 volume 裡。`docker compose down -v` 會刪掉 volume。

## 常見錯誤

**症狀：** `rradiusd check` 印出：

```text
/etc/rradius/rradius.toml: cannot read configuration: No such file or directory (os error 2)
```

**原因：** 預設設定檔不存在。套件不會幫你建立它。
**解法：** 照[套件安裝後的第一次啟動](#套件安裝後的第一次啟動)用 `config init` 產生，或用 `--config` 指定別的檔案。用 `serve` 啟動時，錯誤訊息下面會多一行 `(the default configuration; give --config <path> for another)`。

**症狀：** `systemctl start rradius` 沒有任何錯誤，服務卻沒有在跑。
**原因：** `/etc/rradius/rradius.toml` 不存在，unit 的 `ConditionPathExists` 不成立，服務被略過。
**解法：** 建立設定檔，`check` 通過後再啟動。

**症狀：** `check` 印出 `/usr/share/rradius/dictionary: cannot read dictionary: No such file or directory (os error 2)`。
**原因：** 你用的不是套件或 Docker，字典不在 `config init` 預設的位置。
**解法：** 把 `dictionary` 改成字典實際的位置（原始碼與壓縮檔裡是 `share/dictionary`），或把字典放到預設字典路徑，再刪掉 `dictionary` 這一行。

**症狀：** `check` 印出 `cannot read script 'auth.lua': No such file or directory (os error 2)`。
**原因：** `config init` 的底稿指向 `auth.lua`，這個檔案要自己建立。
**解法：** 在設定檔旁邊建立 `auth.lua`（第 10 章）。

**症狀：** `check` 通過，`serve` 卻印出 `cannot start cluster node 1: …` 並結束，結束碼 2。
**原因：** 資料目錄不能寫入。最常見的是沿用了底稿的 `data_dir = "data"`，在套件安裝的環境裡它變成 `/etc/rradius/data`。`check` 不會試著寫資料目錄，所以檢查不出來。
**解法：** 把 `data_dir` 改成 `/var/lib/rradius`（FreeBSD 是 `/var/db/rradius`），確認擁有者是 `rradius`。

**症狀：** `check` 或其他指令出現 `cluster.listen is not set, so the cluster's Raft and API ports are on all interfaces …` 警告。
**原因：** 設定有 `[[cluster.nodes]]`，卻沒有設 `cluster.listen`（`config init` 的底稿沒有 `[[cluster.nodes]]`，不會出現）。
**解法：** 單機設 `listen = "127.0.0.1"`，或拿掉 `[[cluster.nodes]]`；叢集見第 16 章。

**症狀：** Compose 部署時，指令輸出一直出現 `has mode 644, which others can reach or its group can write`。
**原因：** `init-secrets.sh` 刻意把祕密檔設成 644，見[Docker Compose](#docker-compose)的說明。
**解法：** 在 Compose 部署裡可以不理會。不是用 Compose 時，把祕密檔改成 0600 或 0640。

**症狀：** FreeBSD 上 TACACS+ 沒有在埠 49 監聽。
**原因：** port 以一般用戶執行，不能綁 1024 以下的埠。
**解法：** 見[FreeBSD port](#freebsd-port)的注意事項。

## 相關章節

- [01 認識 rradius](01-overview.md)
- [03 第一次啟動與登入](03-quick-start.md)
- [04 設定檔的基本功](04-configuration.md)
- [05 RADIUS 服務與 NAS 用戶端](05-radius-clients.md)
- [10 用 Lua 寫政策](10-lua-policies.md)
- [16 叢集與高可用](16-cluster.md)
- [17 監控、狀態與日誌](17-monitoring.md)
- [20 安全強化](20-security.md)
