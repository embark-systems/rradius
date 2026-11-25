-- Company-issued devices. An applied file must not be changed; to change the structure, add a file with the next number.
CREATE TABLE devices (
  name TEXT PRIMARY KEY,             -- the name on the certificate, for example laptop-001.corp.example.com
  vlan TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1
);

-- Accounting, same as the quick start.
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
