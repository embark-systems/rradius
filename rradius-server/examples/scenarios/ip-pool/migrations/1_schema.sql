-- 位址池和用戶。套用過的檔案不能再改，要改結構就新增下一個編號的檔案。
CREATE TABLE subscribers (
  username TEXT PRIMARY KEY,
  password TEXT,                          -- 格式見 docs/usage.md 第 5 節
  pool TEXT NOT NULL,                     -- 這個用戶從哪個池拿位址
  enabled INTEGER NOT NULL DEFAULT 1
);

-- 每個位址一列。主鍵是 (pool, ip)：不同的池可以有重疊的位址（例如不同的 VRF）。
--   free      沒人用
--   reserved  認證時預留了，還沒收到 Accounting-Start；expires 到了還沒確認就放回
--   active    NAS 回報在用；每個 Interim-Update 延長 expires，到期沒收到就放回
-- last_user 與 released 是黏著用的：位址放回時記下上一個用的人和時間。
CREATE TABLE ip_pool (
  pool TEXT NOT NULL,
  ip TEXT NOT NULL,
  n INTEGER NOT NULL,                     -- 位址在池裡的序號（同一個池內從 1 起連續編號；不連續會讓隨機起點集中）；挑位址靠它走索引，不用掃全部空閒列
  state TEXT NOT NULL DEFAULT 'free' CHECK (state IN ('free', 'reserved', 'active')),
  username TEXT,
  session_id TEXT,
  nas TEXT,
  expires INTEGER NOT NULL DEFAULT 0,
  updated INTEGER NOT NULL DEFAULT 0,
  last_user TEXT,
  released INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (pool, ip)
);
-- 索引的欄位要涵蓋查詢的條件：SQLite 沒有統計時，條件只用到索引前面幾欄的話，可能改走主鍵掃整個池。
CREATE INDEX ip_pool_user ON ip_pool (username, pool, state);
CREATE INDEX ip_pool_state ON ip_pool (pool, state, n);
CREATE INDEX ip_pool_last_user ON ip_pool (last_user, pool, state, released);
CREATE INDEX ip_pool_expiry ON ip_pool (state, expires);

-- 請求觸發的定期工作用：每個工作一列，記著上次什麼時候跑（見 docs/manual/11-operations.md）。
CREATE TABLE housekeeping (name TEXT PRIMARY KEY, last_run INTEGER NOT NULL);
INSERT INTO housekeeping VALUES ('reap-ip-pool', 0);
