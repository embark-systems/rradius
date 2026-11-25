-- Plans and lines. An applied file must not be changed; to change the structure, add a file with the next number.

-- Each plan: the pool name returned to the BNG, the profile name (rates etc. are set by the profile of the same name on the BNG), and the Interim interval.
CREATE TABLE plans (
  plan TEXT PRIMARY KEY,
  pool TEXT NOT NULL,
  profile TEXT NOT NULL,
  interim INTEGER NOT NULL DEFAULT 300
);

-- One row per line. The primary key is (remote_id, circuit_id): a circuit-id is usually unique only within one access device,
-- and the remote-id is often that device's name; use an empty string when the BNG sends no remote-id.
-- Both ids are stored normalized (see auth.lua): lowercase, whitespace trimmed, and a value with non-printable characters written as 0x plus hex.
--   lock_mac   0: only record the last MAC and time seen; 1: the first login binds the MAC, and a different MAC afterwards is rejected.
--              When the device is replaced, clear bound_mac to NULL. bound_mac is 12 lowercase hex characters, with no colons or hyphens.
CREATE TABLE lines (
  remote_id TEXT NOT NULL DEFAULT '',
  circuit_id TEXT NOT NULL,
  subscriber TEXT NOT NULL,
  plan TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  lock_mac INTEGER NOT NULL DEFAULT 0,
  bound_mac TEXT,
  last_mac TEXT,
  last_seen INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (remote_id, circuit_id)
);
