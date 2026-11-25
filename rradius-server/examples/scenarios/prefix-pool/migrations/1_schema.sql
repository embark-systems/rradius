-- IPv6 前綴池（DHCPv6-PD）。套用過的檔案不能再改，要改結構就新增下一個編號的檔案。

CREATE TABLE subscribers (
  username TEXT PRIMARY KEY,
  password TEXT,                          -- 格式見 docs/usage.md 第 5 節
  pool TEXT NOT NULL,                     -- 這個用戶從哪個前綴池拿前綴；也是回給 BNG 的 Framed-IPv6-Pool 名稱
  enabled INTEGER NOT NULL DEFAULT 1
);

-- 每個池只由三個常數決定，不存每個前綴：
--   base  前綴的高 64 位元，整數。高位元是 1 的（例如 fd00::/8 的 ULA）在 SQLite 是負數，沒有關係。
--   plen  池的長度，例如 /48 寫 48。
--   dlen  委派給每個用戶的長度，例如 /56 寫 56，不超過 64。
-- 第 n 個前綴（n 從 0 起）的高 64 位元是 base | (n << (64 - dlen))，所以一定對齊；
-- 池有 2 的 (dlen - plen) 次方個前綴。base 必須對齊 plen（低 64 - plen 位元是 0），
-- 沒對齊就寫不進來（見 CHECK）。容量最多 2 的 30 次方。
CREATE TABLE prefix_pools (
  pool TEXT PRIMARY KEY,
  base INTEGER NOT NULL,
  plen INTEGER NOT NULL CHECK (plen BETWEEN 0 AND 63),
  dlen INTEGER NOT NULL CHECK (dlen BETWEEN 1 AND 64),
  CHECK (dlen >= plen AND dlen - plen <= 30),
  CHECK ((base & ((1 << (64 - plen)) - 1)) = 0)
);

-- 只放已經分配出去的前綴：沒有那一列就是空閒。
--   reserved  認證時預留了，還沒收到 Accounting-Start；expires 到了還沒確認就刪掉
--   active    NAS 回報在用；每個 Interim-Update 延長 expires，到期沒收到就改成 released
--   released  用戶離線了（Stop、NAS 重開機、沒收到 Interim）：前綴暫時留給他，expires（釋放時間加
--             HOLD_SECONDS）到了才刪；他回來先拿這一列，別人只有在沒有空的 n 時才搶最舊的 released
-- prefix 是前綴的文字，必須和伺服器把 NAS 回報的 Delegated-IPv6-Prefix 解碼出來的文字一樣
-- （壓縮、小寫，例如 2001:db8:100::/56）：計費確認靠它找到那一列。allocate.lua 由 n 算出它。
CREATE TABLE prefix_leases (
  pool TEXT NOT NULL,
  n INTEGER NOT NULL,
  prefix TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('reserved', 'active', 'released')),
  username TEXT NOT NULL,
  session_id TEXT,
  nas TEXT,
  expires INTEGER NOT NULL,
  updated INTEGER NOT NULL,
  PRIMARY KEY (pool, n)
);
-- 同一個池裡一個用戶只有一個前綴，同一個前綴只有一個人。
CREATE UNIQUE INDEX prefix_leases_user ON prefix_leases (username, pool);
CREATE UNIQUE INDEX prefix_leases_prefix ON prefix_leases (pool, prefix);
CREATE INDEX prefix_leases_expiry ON prefix_leases (state, expires);
-- Accounting-On／Off 刪這台 NAS 的 active 前綴用。
CREATE INDEX prefix_leases_nas ON prefix_leases (nas, state);
-- 搶最舊的 released 列、和 released 的年齡用。
CREATE INDEX prefix_leases_released ON prefix_leases (pool, updated) WHERE state = 'released';
-- 監控的 GROUP BY pool, state 用（覆蓋索引，不回表）。
CREATE INDEX prefix_leases_pool_state ON prefix_leases (pool, state);

-- 請求觸發的定期工作用：每個工作一列，記著上次什麼時候跑（見 docs/manual.zh-TW/18-operations.md〈清除舊資料〉）。
CREATE TABLE housekeeping (name TEXT PRIMARY KEY, last_run INTEGER NOT NULL);
INSERT INTO housekeeping VALUES ('reap-prefix-pool', 0);
