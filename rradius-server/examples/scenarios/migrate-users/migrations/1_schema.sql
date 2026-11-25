-- Users, reply attributes and accounting. A file that has been applied must not be changed; to change the structure, add a file with the next number.
CREATE TABLE users (
  username TEXT PRIMARY KEY,
  password TEXT,                          -- format: see docs/usage.md section 5
  enabled INTEGER NOT NULL DEFAULT 1,
  origin TEXT NOT NULL DEFAULT ''                 -- where this account's password came from, for statistics only
);

-- One reply attribute per row; the same attribute can have several rows.
CREATE TABLE user_reply (
  username TEXT NOT NULL,
  attribute TEXT NOT NULL,
  value TEXT NOT NULL
);
CREATE INDEX user_reply_user ON user_reply (username);

-- One row per session. The Session ID is unique only within one NAS, so the primary key includes the client and the NAS:
-- one client can be a whole subnet, and nas is the address or name the NAS reports (see accounting.lua).
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
