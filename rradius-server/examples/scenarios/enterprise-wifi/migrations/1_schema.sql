-- 帳號（員工）和裝置（公司配發的電腦）在同一張表，依 grp 決定 VLAN。
CREATE TABLE accounts (
  name TEXT PRIMARY KEY,                -- 員工：小寫帳號；裝置：憑證上的名稱（DNS SAN）
  kind TEXT NOT NULL,                   -- 'user' 或 'device'
  password TEXT,                        -- 員工用，PEAP 需要明文或 {nthash}；裝置留空
  grp TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1
);

-- 每個群組的回覆屬性。VLAN 的三個屬性都要有，tag 寫在名稱後面。
CREATE TABLE group_reply (
  grp TEXT NOT NULL,
  attribute TEXT NOT NULL,
  value TEXT NOT NULL
);
CREATE INDEX group_reply_grp ON group_reply (grp);

-- accounting.lua（共用快速開始的腳本）寫這張表。
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
