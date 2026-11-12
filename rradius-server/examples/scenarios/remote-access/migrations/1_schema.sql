-- 跳板機的兩步驟驗證（two-step-totp.lua 用的表）和計費。
CREATE TABLE mfa_users (
  username TEXT PRIMARY KEY,
  password TEXT,                          -- two-step-totp.lua 比對明文；要雜湊請自行改寫腳本
  totp_secret TEXT,                       -- Base32，和使用者手機上的驗證器 App 相同
  last_step INTEGER NOT NULL DEFAULT 0,
  failed_attempts INTEGER NOT NULL DEFAULT 0,
  locked_until INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE mfa_pending (state TEXT PRIMARY KEY, username TEXT, expires INTEGER);

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
