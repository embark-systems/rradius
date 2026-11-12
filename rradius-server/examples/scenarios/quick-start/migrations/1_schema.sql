-- 使用者、回覆屬性和計費。套用過的檔案不能再改，要改結構就新增下一個編號的檔案。
CREATE TABLE users (
  username TEXT PRIMARY KEY,
  password TEXT,                          -- 格式見 docs/usage.md 第 5 節
  enabled INTEGER NOT NULL DEFAULT 1
);

-- 每列一個回覆屬性；同一個屬性可以有多列。
CREATE TABLE user_reply (
  username TEXT NOT NULL,
  attribute TEXT NOT NULL,
  value TEXT NOT NULL
);
CREATE INDEX user_reply_user ON user_reply (username);

-- 每個 session 一列。Session ID 只在同一台 NAS 內唯一，所以主鍵包含 client 和 NAS：
-- 一個 client 可以是整個網段，nas 是 NAS 回報的位址或名稱（見 accounting.lua）。
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
CREATE INDEX accounting_user ON accounting (username);
