-- Address pool and users. A file that has been applied must not be changed; to change the structure, add a file with the next number.
CREATE TABLE subscribers (
  username TEXT PRIMARY KEY,
  password TEXT,                          -- format: see docs/usage.md section 5
  pool TEXT NOT NULL,                     -- which pool this user gets an address from
  enabled INTEGER NOT NULL DEFAULT 1
);

-- One row per address. The primary key is (pool, ip): different pools can have overlapping addresses (for example different VRFs).
--   free      nobody uses it
--   reserved  reserved at authentication, no Accounting-Start received yet; put back once expires passes without confirmation
--   active    reported in use by the NAS; each Interim-Update extends expires, and it is put back if none arrives by then
-- last_user and released are for stickiness: when an address is put back, the previous user and the time are recorded.
CREATE TABLE ip_pool (
  pool TEXT NOT NULL,
  ip TEXT NOT NULL,
  n INTEGER NOT NULL,                     -- the address's sequence number in the pool (numbered consecutively from 1 within a pool; gaps would make the random start point cluster); picking an address uses it to walk the index instead of scanning all free rows
  state TEXT NOT NULL DEFAULT 'free' CHECK (state IN ('free', 'reserved', 'active')),
  username TEXT,
  session_id TEXT,
  nas TEXT,
  expires INTEGER NOT NULL DEFAULT 0,
  updated INTEGER NOT NULL DEFAULT 0,
  last_user TEXT,
  released INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (pool, ip)
);
-- The index columns must cover the query conditions: when SQLite has no statistics and the condition uses only the leading columns of an index, it may scan the whole pool through the primary key instead.
CREATE INDEX ip_pool_user ON ip_pool (username, pool, state);
CREATE INDEX ip_pool_state ON ip_pool (pool, state, n);
CREATE INDEX ip_pool_last_user ON ip_pool (last_user, pool, state, released);
CREATE INDEX ip_pool_expiry ON ip_pool (state, expires);

-- Used by request-triggered periodic jobs: one row per job, recording when it last ran (see docs/manual/18-operations.md "Purging old data").
CREATE TABLE housekeeping (name TEXT PRIMARY KEY, last_run INTEGER NOT NULL);
INSERT INTO housekeeping VALUES ('reap-ip-pool', 0);
