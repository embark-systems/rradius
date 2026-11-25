-- IPv6 prefix pool (DHCPv6-PD). A file that has been applied must not be changed; to change the structure, add a file with the next number.

CREATE TABLE subscribers (
  username TEXT PRIMARY KEY,
  password TEXT,                          -- format: see docs/usage.md section 5
  pool TEXT NOT NULL,                     -- which prefix pool this user gets prefixes from; also the Framed-IPv6-Pool name returned to the BNG
  enabled INTEGER NOT NULL DEFAULT 1
);

-- Each pool is defined by only three constants; individual prefixes are not stored:
--   base  the high 64 bits of the prefix, as an integer. Values whose top bit is 1 (for example the ULA fd00::/8) are negative in SQLite, which does not matter.
--   plen  the length of the pool, for example 48 for /48.
--   dlen  the length delegated to each user, for example 56 for /56, at most 64.
-- The high 64 bits of prefix n (n counts from 0) are base | (n << (64 - dlen)), so it is always aligned;
-- the pool has 2 to the power (dlen - plen) prefixes. base must be aligned to plen (the low 64 - plen bits are 0),
-- and one that is not aligned cannot be inserted (see CHECK). The capacity is at most 2 to the power 30.
CREATE TABLE prefix_pools (
  pool TEXT PRIMARY KEY,
  base INTEGER NOT NULL,
  plen INTEGER NOT NULL CHECK (plen BETWEEN 0 AND 63),
  dlen INTEGER NOT NULL CHECK (dlen BETWEEN 1 AND 64),
  CHECK (dlen >= plen AND dlen - plen <= 30),
  CHECK ((base & ((1 << (64 - plen)) - 1)) = 0)
);

-- Holds only prefixes that have been handed out: a prefix with no row is free.
--   reserved  reserved at authentication, no Accounting-Start received yet; deleted if expires passes without confirmation
--   active    reported in use by the NAS; each Interim-Update extends expires, and if none arrives by then it becomes released
--   released  the user went offline (Stop, NAS reboot, missed Interim): the prefix is kept for them for now, and it is deleted only when expires (release time plus
--             HOLD_SECONDS) passes; they get this row first when they return, and others take the oldest released one only when there is no free n
-- prefix is the text of the prefix, which must be the same as the text the server decodes from the Delegated-IPv6-Prefix the NAS reports
-- (compressed, lower case, for example 2001:db8:100::/56): accounting confirmation uses it to find the row. allocate.lua computes it from n.
CREATE TABLE prefix_leases (
  pool TEXT NOT NULL,
  n INTEGER NOT NULL,
  prefix TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('reserved', 'active', 'released')),
  username TEXT NOT NULL,
  session_id TEXT,
  nas TEXT,
  expires INTEGER NOT NULL,
  updated INTEGER NOT NULL,
  PRIMARY KEY (pool, n)
);
-- Within one pool a user has only one prefix, and a prefix belongs to only one user.
CREATE UNIQUE INDEX prefix_leases_user ON prefix_leases (username, pool);
CREATE UNIQUE INDEX prefix_leases_prefix ON prefix_leases (pool, prefix);
CREATE INDEX prefix_leases_expiry ON prefix_leases (state, expires);
-- Used to delete the active prefixes of a NAS on Accounting-On/Off.
CREATE INDEX prefix_leases_nas ON prefix_leases (nas, state);
-- Used to take the oldest released row and for the age of released rows.
CREATE INDEX prefix_leases_released ON prefix_leases (pool, updated) WHERE state = 'released';
-- Used by the monitoring GROUP BY pool, state (a covering index, no table lookup).
CREATE INDEX prefix_leases_pool_state ON prefix_leases (pool, state);

-- Used by request-triggered periodic jobs: one row per job, recording when it last ran (see docs/manual/18-operations.md "Purging old data").
CREATE TABLE housekeeping (name TEXT PRIMARY KEY, last_run INTEGER NOT NULL);
INSERT INTO housekeeping VALUES ('reap-prefix-pool', 0);
