-- 方案、用戶、連線位和計費。套用過的檔案不能再改，要改結構就新增下一個編號的檔案。

-- 每個方案：最多同時幾個連線、一段時間的流量上限、超過之後怎麼辦。
--   quota_octets   流量上限（位元組，進加出）；0 表示不限
--   period_days    計量期間：最近幾天（滾動窗口，不是日曆月）
--   over_quota     超過之後：'throttle' 降速（登入成功，但回 throttle_filter）或 'reject' 拒絕
--   normal_filter  正常時回給 NAS 的 Filter-Id；throttle_filter 是降速時的
CREATE TABLE plans (
  plan TEXT PRIMARY KEY,
  max_sessions INTEGER NOT NULL CHECK (max_sessions >= 1),
  quota_octets INTEGER NOT NULL DEFAULT 0,
  period_days INTEGER NOT NULL DEFAULT 30,
  over_quota TEXT NOT NULL DEFAULT 'throttle' CHECK (over_quota IN ('throttle', 'reject')),
  normal_filter TEXT NOT NULL,
  throttle_filter TEXT
);

CREATE TABLE subscribers (
  username TEXT PRIMARY KEY,
  password TEXT,                          -- 格式見 docs/usage.md 第 5 節
  plan TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1
);

-- 每個用戶 max_sessions 個連線位，第一次登入時建立。同時連線數就是被佔住的連線位數。
--   free      空的
--   reserved  認證時預留了，還沒收到 Accounting-Start；expires 到了還沒確認就放回
--   active    NAS 回報在線；每個 Interim-Update 延長 expires，到期沒收到就放回
CREATE TABLE session_slots (
  username TEXT NOT NULL,
  slot INTEGER NOT NULL,
  state TEXT NOT NULL DEFAULT 'free' CHECK (state IN ('free', 'reserved', 'active')),
  session_id TEXT,
  nas TEXT,
  expires INTEGER NOT NULL DEFAULT 0,
  updated INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (username, slot)
);
CREATE INDEX session_slots_expiry ON session_slots (state, expires);

-- 每個 session 一列（寫法同 quick-start）。Session ID 只在同一台 NAS 內唯一，所以主鍵包含 client 和 NAS。
CREATE TABLE accounting (
  client TEXT NOT NULL,
  nas TEXT NOT NULL,
  session_id TEXT NOT NULL,
  username TEXT,
  started INTEGER NOT NULL,
  updated INTEGER NOT NULL,
  stopped INTEGER,
  session_time INTEGER NOT NULL DEFAULT 0,
  input_octets INTEGER NOT NULL DEFAULT 0,
  output_octets INTEGER NOT NULL DEFAULT 0,
  terminate_cause INTEGER,
  PRIMARY KEY (client, nas, session_id)
);
-- 認證時加總一個用戶期間內的流量：靠這個索引只看他自己的列。
CREATE INDEX accounting_user ON accounting (username, started);

-- 請求觸發的定期工作用：每個工作一列，記著上次什麼時候跑（見 docs/manual.zh-TW/18-operations.md〈清除舊資料〉）。
CREATE TABLE housekeeping (name TEXT PRIMARY KEY, last_run INTEGER NOT NULL);
INSERT INTO housekeeping VALUES ('reap-session-slots', 0);
