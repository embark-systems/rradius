# 套件

主機安裝用的套件資料。每種都從已提交的原始碼建置，建置時會下載 crate，所以**不符合** Debian、Fedora、FreeBSD 官方倉庫「離線建置」的要求；這裡的目的是自己建、自己的倉庫發佈。發行用的壓縮檔另見 [`tarball/`](tarball/README.md)。

| 目錄 | 內容 | 產出 |
| --- | --- | --- |
| `common/` | 三種套件共用的檔案：systemd 服務、logrotate 設定、安裝後的說明 `README.packages` | |
| `deb/` | `debian/` 目錄和 `build.sh` | `rradius_<版本>_<架構>.deb`（amd64、arm64） |
| `rpm/` | `rradius.spec` 和 `build.sh` | `rradius-<版本>-1.<dist>.<架構>.rpm`（x86_64、aarch64） |
| `freebsd/` | `net/rradius` port、`generate-crates.py` | FreeBSD ports |

## 安裝後的樣子

| | Linux（deb、rpm） | FreeBSD |
| --- | --- | --- |
| 執行檔 | `/usr/bin/rradius-server` | `/usr/local/bin/rradius-server` |
| 字典 | `/usr/share/rradius/` | `/usr/local/share/rradius/` |
| 設定 | `/etc/rradius/`（預設 `rradius.toml`） | `/usr/local/etc/rradius/` |
| 資料 | `/var/lib/rradius` | `/var/db/rradius` |
| 記錄 | `/var/log/rradius` | `/var/log/rradius` |
| 文件、範例 | `/usr/share/doc/rradius/` | `/usr/local/share/doc/rradius/` |
| 服務 | systemd 的 `rradius.service` | `rc.d/rradius` |
| 用戶 | 系統用戶 `rradius` | 用戶 `rradius` |

預設設定檔路徑在建置時決定（`RRADIUS_PREFIX`，見[使用手冊參考](../docs/usage.md)第 1 節）：Linux 套件不設，是 `/etc/rradius/rradius.toml`；FreeBSD 設成 `/usr/local`。

套件**不建立設定檔，也不啟用服務**：deb 不啟用也不啟動；rpm 依發行版的預設（Rocky 等通常不啟用）；設定檔不存在時服務也不會啟動（`ConditionPathExists`）。裝好後照 [`common/README.packages`](common/README.packages)（裝在主機上的是 `/usr/share/doc/rradius/README.packages`）做：`rradius-server config init` 產生起始設定、改字典路徑、`check`、啟動。移除套件時，資料、記錄和用戶都保留。

## 建置

套件依賴的 glibc 版本跟著建置主機走。deb 用 `deb/docker-build.sh` 在 Debian 12 容器建（amd64 或 arm64，另一種架構用模擬執行，慢但結果相同），依賴 glibc 2.34 以上，Debian 12、13 和 Ubuntu 22.04 以後都能裝。rpm 也一樣：要裝到較舊的發行版，就在那個發行版上建。

所有套件都需要 Rust 1.99 以上（和 `Dockerfile` 一樣）、cmake、g++ 和 make。Rust 比各發行版附的新，所以套件沒有把 cargo 列為建置相依；用 rustup 或 `rust` 映像提供。建置用的環境變數和 `Dockerfile` 相同：`LIBSQLITE3_FLAGS=-DSQLITE_ENABLE_QPSG`，arm64 加 `JEMALLOC_SYS_WITH_LG_PAGE=16`，`debian/rules`、`rradius.spec` 和 port 都已設好。

```bash
# deb：另需 debhelper、dpkg-dev
packages/deb/build.sh --out dist

# deb，在 Debian 12 容器建（只需要 docker）
packages/deb/docker-build.sh --platform linux/amd64 --out dist

# rpm：另需 rpm-build、systemd-rpm-macros
packages/rpm/build.sh --out dist
```

兩個腳本都用 `git archive` 取已提交的內容（`--commit <rev>` 指定哪一個提交），所以沒提交的改動不會進套件。

FreeBSD：把 `freebsd/net/rradius` 放進 ports 樹的 `net/`，並在 ports 樹的 `UIDs` 和 `GIDs` 加上 `rradius`（port 用 `USERS`、`GROUPS` 建立用戶），`make makesum` 產生 `distinfo`，再 `make package`。`Makefile.crates` 由 `freebsd/generate-crates.py` 從 `Cargo.lock` 產生，`Cargo.lock` 一變就重跑（`--check` 只檢查是否過期）。port 抓的是 GitHub 的標籤 `v<版本>`，發佈前要先有這個標籤。

## 驗證了什麼

| | 狀態 |
| --- | --- |
| deb | 用 `docker-build.sh` 在 Debian 12 容器建出 amd64（模擬）和 arm64，依賴 glibc 2.34 以上；兩個都在 Debian 12 容器安裝：檢查內容、用戶與目錄、預設設定檔路徑 `/etc/rradius/rradius.toml`、`config init` 可執行、權限 0750。先前在 Ubuntu 24.04 arm64 容器也建過並驗證移除（用戶保留）、服務不自動啟用。**2026-11-18 在有 systemd 的 Debian 12 arm64 機器（OrbStack）上驗證：** 安裝後服務沒有啟用也沒有啟動、`conf.d/` 在、`config init` 後 `check` 通過、`systemctl enable --now` 啟動並服務、`status` 正常、`conf.d/` 的檔案被讀進 `config show`、`restart` 乾淨停止，升級（`--reinstall`）後服務仍在執行（見下一段的修正）、移除保留設定、資料與用戶，`purge` 只清掉空的目錄。amd64 的 deb 沒有在 systemd 主機上跑過 |
| deb 升級 | 2026-11-18 發現：`dh_installsystemd --no-start` 在升級時會停掉執行中的服務而不再啟動，`apt install --reinstall` 後服務是「啟用但停止」。改成只用 `--no-enable`（修正在 `debian/rules`）後，全新安裝仍然不啟用也不啟動，升級後服務重新啟動、持續服務 |
| rpm | 在 Rocky Linux 9（aarch64）容器建置；檢查內容、安裝（`--nodeps`，容器裡沒有 systemd）、移除（用戶保留）。**2026-11-18 在有 systemd 的 AlmaLinux 9 aarch64 機器（OrbStack）上建置並驗證：** `rpmbuild` 用 Rust 1.99 建出，`dnf install` 後服務沒有啟用也沒有啟動、`config init`、`check`、`systemctl enable --now` 啟動並服務、`status` 正常，`dnf reinstall` 時服務重新啟動（程序編號改變、仍在執行），`dnf remove` 停止服務並保留用戶、設定與資料，空的 `conf.d/` 隨套件移除。那台機器的 SELinux 是 Disabled，**沒有在 SELinux enforcing 下驗證**；沒有建 x86_64 |
| `conf.d/` | 三種套件的定義都建立空的 `/etc/rradius/conf.d`（FreeBSD 是 `etc/rradius/conf.d`）：`deb/debian/rules` 與 `rradius.postrm`、`rpm/rradius.spec` 的 `%dir`、`pkg-plist` 的 `@dir`，說明在 `common/README.packages` 與 FreeBSD 的 `README.packages`。**只檢查了定義檔，沒有重建套件，也沒有安裝驗證** |
| 服務檔 | 只有 `common/rradius.service` 一份；`deb/build.sh` 建置時把它複製進 `debian/`。 |
| 服務檔驗證 | `systemd-analyze verify` 沒有錯誤；沒有實際啟動 |
| FreeBSD port | **沒有在 FreeBSD 驗證。** 沒有 `distinfo`（要在 FreeBSD 用 `make makesum` 產生），沒有跑過 `make`、`portlint`、`poudriere`；rc.d 腳本只過了 `sh -n`；`Makefile.crates` 的筆數與 `Cargo.lock` 相同 |
| 版本和更新 | 版本來自 `rradius-server/Cargo.toml`；改版本時要同步 `deb/debian/changelog`、`rpm/rradius.spec`、`freebsd/net/rradius/Makefile`，沒有自動檢查 |
