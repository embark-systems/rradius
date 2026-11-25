-- Accounts live in the member system; only accounting is here.
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
