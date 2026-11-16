-- 方案、用戶和 BNG 的風格。套用過的檔案不能再改，要改結構就新增下一個編號的檔案。

-- 每個方案：下載（往用戶的方向）和上傳（用戶往網路的方向）的速率，單位 kbit/s；
-- profile 是 BNG 上同名設定檔的名稱，只收名稱的風格用它。
CREATE TABLE plans (
  plan TEXT PRIMARY KEY,
  down_kbps INTEGER NOT NULL CHECK (down_kbps > 0),
  up_kbps INTEGER NOT NULL CHECK (up_kbps > 0),
  profile TEXT NOT NULL
);

CREATE TABLE subscribers (
  username TEXT PRIMARY KEY,
  password TEXT,                          -- 格式見 docs/usage.md 第 5 節
  plan TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1
);

-- 每個 BNG（RADIUS client）用哪一種屬性。client 欄位是設定檔 [[radius.clients]] 的 name。
-- style：profile、wispr、mikrotik、alc_sla、erx（見 rate.lua）。沒有列的 client 用 profile。
CREATE TABLE nas_styles (
  client TEXT PRIMARY KEY,
  style TEXT NOT NULL
);
