-- Users and fixed addresses. An applied file must not be edited; to change the structure, add a file with the next number.
CREATE TABLE subscribers (
  username TEXT PRIMARY KEY,
  password TEXT,                          -- Format: see docs/usage.md section 5
  pool TEXT NOT NULL,                     -- a user with no fixed data gets this pool name back, and the BNG's pool decides the address
  enabled INTEGER NOT NULL DEFAULT 1
);

-- Only a user with a fixed address or prefix has a row; every column may be NULL, and a NULL column does not return that attribute.
--   ipv4       Framed-IP-Address, a single address, for example 203.0.113.10
--   netmask    Framed-IP-Netmask, 255.255.255.255 is returned if not set
--   routed_v4  subnet routed to the user, returned as Framed-Route: <subnet> 0.0.0.0 1, for example 203.0.113.8/29
--   wan_v6     Framed-IPv6-Prefix, the prefix of the connection itself, usually a /64, for example 2001:db8:1:1::/64
--   pd_v6      Delegated-IPv6-Prefix, the prefix delegated to the user's inner network, for example 2001:db8:100::/56
-- ipv4, wan_v6 and pd_v6 each have a unique index: two users cannot register the same text (NULL does not count).
-- The comparison is on text: always write an IPv6 prefix in lowercase, compressed form (2001:db8:1:1::/64);
-- 2001:0db8:1:1::/64 and 2001:DB8:1:1::/64 are different text, and the index cannot tell them apart.
-- Overlapping subnets (one user's /29 containing another's /32) are not caught either; see the manual.
CREATE TABLE static_assignments (
  username TEXT PRIMARY KEY,
  ipv4 TEXT,
  netmask TEXT,
  routed_v4 TEXT,
  wan_v6 TEXT,
  pd_v6 TEXT
);
CREATE UNIQUE INDEX static_assignments_ipv4 ON static_assignments (ipv4);
CREATE UNIQUE INDEX static_assignments_wan_v6 ON static_assignments (wan_v6);
CREATE UNIQUE INDEX static_assignments_pd_v6 ON static_assignments (pd_v6);
