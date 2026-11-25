-- Lines, plans, IPv4 address pool, IPv6 prefix pool. A file that has been applied must not be edited; to change the structure, add a file with the next number.
-- The pool tables are taken from manual chapter 14, "IPv4 address pool" (ip_pool) and "IPv6 prefix pool" (prefix_pools, prefix_leases), using subscriber as the user name.

-- Each plan: the profile name returned to the BNG (rates and so on are decided by the profile of the same name on the BNG), the Interim interval, and
-- which IPv4 pool (pool, the pool name in ip_pool) and which prefix pool (pool6, the pool name in prefix_pools) a dynamic line takes addresses from.
CREATE TABLE plans (
  plan TEXT PRIMARY KEY,
  pool TEXT NOT NULL,
  pool6 TEXT NOT NULL,
  profile TEXT NOT NULL,
  interim INTEGER NOT NULL DEFAULT 300
);

-- One row per line. The primary key is (remote_id, circuit_id); both ids are stored in normalized form (see line.lua).
--   lock_mac     0: only record the last MAC seen; 1: the first login binds the MAC, and later a different MAC is rejected.
-- Fixed data (a NULL field means not fixed):
--   ipv4         fixed IPv4 address; if set, this line's IPv4 is not taken from the pool. When netmask is not set, 255.255.255.255 is returned.
--   routed_v4    subnet routed to the user, returned as Framed-Route; set it together with ipv4.
--   wan_v6       the prefix of the connection itself (Framed-IPv6-Prefix), for example 2001:db8:1:1::/64.
--   pd_v6        delegated prefix (Delegated-IPv6-Prefix), for example 2001:db8:f00::/56.
-- If either wan_v6 or pd_v6 is set, this line's IPv6 is not taken from the prefix pool; only the one that is set is returned.
-- IPv4 and IPv6 are decided separately: for a line with only a fixed IPv4, the IPv6 prefix is still taken from the pool.
-- Comparison is by text: always write IPv6 prefixes in lowercase, compressed form.
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
  ipv4 TEXT,
  netmask TEXT,
  routed_v4 TEXT,
  wan_v6 TEXT,
  pd_v6 TEXT,
  PRIMARY KEY (remote_id, circuit_id)
);
-- The pools and accounting use subscriber as the key, so a subscriber can have only one line.
CREATE UNIQUE INDEX lines_subscriber ON lines (subscriber);
CREATE UNIQUE INDEX lines_ipv4 ON lines (ipv4);
CREATE UNIQUE INDEX lines_wan_v6 ON lines (wan_v6);
CREATE UNIQUE INDEX lines_pd_v6 ON lines (pd_v6);

-- IPv4 address pool: one row per address. The primary key is (pool, ip).
--   free      nobody is using it
--   reserved  reserved at authentication, Accounting-Start not yet received; put back if not confirmed by expires
--   active    reported in use by the NAS; each Interim-Update extends expires, and it is put back if none arrives by then
-- last_user and released are for stickiness: when an address is put back, the previous user and the time are recorded.
CREATE TABLE ip_pool (
  pool TEXT NOT NULL,
  ip TEXT NOT NULL,
  n INTEGER NOT NULL,                     -- -- the address's sequence number in the pool, numbered consecutively from 1 within a pool
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
CREATE INDEX ip_pool_user ON ip_pool (username, pool, state);
CREATE INDEX ip_pool_state ON ip_pool (pool, state, n);
CREATE INDEX ip_pool_last_user ON ip_pool (last_user, pool, state, released);
CREATE INDEX ip_pool_expiry ON ip_pool (state, expires);

-- IPv6 prefix pool: each pool is determined by three constants only, and individual prefixes are not stored (see manual chapter 14, "IPv6 prefix pool").
--   base  the high 64 bits of the prefix, an integer; plen the pool's length; dlen the length delegated to each user, at most 64.
CREATE TABLE prefix_pools (
  pool TEXT PRIMARY KEY,
  base INTEGER NOT NULL,
  plen INTEGER NOT NULL CHECK (plen BETWEEN 0 AND 63),
  dlen INTEGER NOT NULL CHECK (dlen BETWEEN 1 AND 64),
  CHECK (dlen >= plen AND dlen - plen <= 30),
  CHECK ((base & ((1 << (64 - plen)) - 1)) = 0)
);

-- Only prefixes already handed out are stored: no row means free.
--   reserved  reserved at authentication, Accounting-Start not yet received; deleted if not confirmed by expires
--   active    reported in use by the NAS; each Interim-Update extends expires, and it becomes released if none arrives by then
--   released  the user went offline: the prefix is kept for them temporarily, and deleted only when expires (release time plus HOLD_SECONDS) is reached
-- prefix must be the same text the server produces when it decodes the Delegated-IPv6-Prefix reported by the NAS (compressed, lowercase).
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
CREATE UNIQUE INDEX prefix_leases_user ON prefix_leases (username, pool);
CREATE UNIQUE INDEX prefix_leases_prefix ON prefix_leases (pool, prefix);
CREATE INDEX prefix_leases_expiry ON prefix_leases (state, expires);
CREATE INDEX prefix_leases_nas ON prefix_leases (nas, state);
CREATE INDEX prefix_leases_released ON prefix_leases (pool, updated) WHERE state = 'released';
CREATE INDEX prefix_leases_pool_state ON prefix_leases (pool, state);

-- For periodic work triggered by requests: one row per job, recording when it last ran (see docs/manual/18-operations.md "Purging old data").
CREATE TABLE housekeeping (name TEXT PRIMARY KEY, last_run INTEGER NOT NULL);
INSERT INTO housekeeping VALUES ('reap-ip-pool', 0), ('reap-prefix-pool', 0);
