-- Demo data; delete this file in production and use your own data.
-- laptop-002 is still enabled in the table: what blocks it is the revocation list.
INSERT INTO devices (name, vlan, enabled) VALUES
  ('laptop-001.corp.example.com', '10', 1),
  ('laptop-002.corp.example.com', '10', 1);
