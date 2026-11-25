-- Users (employees) and devices (company-issued computers) are in one table; grp decides the VLAN.
CREATE TABLE accounts (
  name TEXT PRIMARY KEY,                -- employee: lowercase user name; device: the name on the certificate (DNS SAN)
  kind TEXT NOT NULL,                   -- 'user' or 'device'
  password TEXT,                        -- for employees; PEAP needs plaintext or {nthash}; leave empty for devices
  grp TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1
);

-- Reply attributes of each group. All three VLAN attributes are needed, and the tag goes after the name.
CREATE TABLE group_reply (
  grp TEXT NOT NULL,
  attribute TEXT NOT NULL,
  value TEXT NOT NULL
);
CREATE INDEX group_reply_grp ON group_reply (grp);

-- accounting.lua (the quick-start script, shared) writes this table.
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
