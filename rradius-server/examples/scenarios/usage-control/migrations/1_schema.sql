-- Plans, users, session slots and accounting. An applied file must not be edited; to change the structure, add a file with the next number.

-- Each plan: the maximum concurrent sessions, the traffic limit over a period, and what to do when it is exceeded.
--   quota_octets   traffic limit (bytes, in plus out); 0 means unlimited
--   period_days    metering period: the last N days (a rolling window, not a calendar month)
--   over_quota     when exceeded: 'throttle' (login succeeds but returns throttle_filter) or 'reject'
--   normal_filter  the Filter-Id returned to the NAS normally; throttle_filter is the one used when throttled
CREATE TABLE plans (
  plan TEXT PRIMARY KEY,
  max_sessions INTEGER NOT NULL CHECK (max_sessions >= 1),
  quota_octets INTEGER NOT NULL DEFAULT 0,
  period_days INTEGER NOT NULL DEFAULT 30,
  over_quota TEXT NOT NULL DEFAULT 'throttle' CHECK (over_quota IN ('throttle', 'reject')),
  normal_filter TEXT NOT NULL,
  throttle_filter TEXT
);

CREATE TABLE subscribers (
  username TEXT PRIMARY KEY,
  password TEXT,                          -- Format: see docs/usage.md section 5
  plan TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1
);

-- Each user has max_sessions slots, created at first login. The number of concurrent sessions is the number of occupied slots.
--   free      empty
--   reserved  held during authentication, no Accounting-Start yet; put back if not confirmed by expires
--   active    the NAS reports it online; each Interim-Update extends expires, and it is put back if none arrives by then
CREATE TABLE session_slots (
  username TEXT NOT NULL,
  slot INTEGER NOT NULL,
  state TEXT NOT NULL DEFAULT 'free' CHECK (state IN ('free', 'reserved', 'active')),
  session_id TEXT,
  nas TEXT,
  expires INTEGER NOT NULL DEFAULT 0,
  updated INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (username, slot)
);
CREATE INDEX session_slots_expiry ON session_slots (state, expires);

-- One row per session (written as in quick-start). A Session ID is unique only within one NAS, so the primary key includes the client and the NAS.
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
-- At authentication, sum a user's traffic over the period: this index lets it read only that user's rows.
CREATE INDEX accounting_user ON accounting (username, started);

-- For periodic work triggered by requests: one row per job, recording when it last ran (see `docs/manual/18-operations.md` "Purging old data").
CREATE TABLE housekeeping (name TEXT PRIMARY KEY, last_run INTEGER NOT NULL);
INSERT INTO housekeeping VALUES ('reap-session-slots', 0);
