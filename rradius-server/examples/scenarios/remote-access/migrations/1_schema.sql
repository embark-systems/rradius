-- Two-step verification for the jump host (the table two-step-totp.lua uses) and accounting.
CREATE TABLE mfa_users (
  username TEXT PRIMARY KEY,
  password TEXT,                          -- prefixed stored password, for example {crypt}$6$... or {argon2}$...; two-step-totp.lua verifies it with password_verify
  totp_secret TEXT,                       -- Base32, the same as the authenticator app on the user's phone
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
